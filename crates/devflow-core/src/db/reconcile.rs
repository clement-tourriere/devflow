//! Crash recovery. Every operation records its row before its first external
//! effect; this module finishes or rolls back whatever a dead process left, by
//! inspecting `pg_database` and `pg_roles`. A failed operation runs the same
//! code on its own row.
use super::admin::{Session, bootstrap_leftovers, lock_lost};
use super::meta::{self, BranchRow};
use super::{ReconcileReport, RecordStatus, SnapshotInfo};
use anyhow::{Context, Result, bail};
use uuid::Uuid;

/// Recover everything pending for the project. Per-record failures go to
/// `problems`; only a lost lock or an unreadable metadata database is an error.
pub(super) fn run(s: &mut Session) -> Result<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let project = s.project_id();

    for row in meta::list_branches(&mut s.admin, project)? {
        let id = row.id;
        match recover_row(s, row) {
            Ok(Some(action)) => report.actions.push(action),
            Ok(None) => {}
            Err(e) => {
                let e = s.lock_failure(e);
                if lock_lost(&e) {
                    return Err(e);
                }
                let _ = meta::set_branch_error(&mut s.admin, id, Some(&format!("{e:#}")));
                report.problems.push(format!("{e:#}"));
            }
        }
    }

    for snapshot in meta::list_snapshots(&mut s.admin, project)? {
        match recover_snapshot_row(s, snapshot) {
            Ok(Some(action)) => report.actions.push(action),
            Ok(None) => {}
            Err(e) => {
                let e = s.lock_failure(e);
                if lock_lost(&e) {
                    return Err(e);
                }
                report.problems.push(format!("{e:#}"));
            }
        }
    }

    for role in meta::retired_roles(&mut s.admin, project)? {
        match retry_retired_role(s, &role) {
            Ok(Some(action)) => report.actions.push(action),
            Ok(None) => {}
            Err(e) => {
                let e = s.lock_failure(e);
                if lock_lost(&e) {
                    return Err(e);
                }
                report.problems.push(format!("retired role {role}: {e:#}"));
            }
        }
    }

    // Server-level: staging databases of an interrupted bootstrap. devflow never
    // adopts or drops them, so the operator is told.
    match bootstrap_leftovers(&mut s.admin) {
        Ok(found) => report.problems.extend(found),
        Err(e) => {
            let e = s.lock_failure(e.into());
            if lock_lost(&e) {
                return Err(e);
            }
            report.problems.push(format!("bootstrap leftovers: {e:#}"));
        }
    }
    Ok(report)
}

/// Recover one branch by id (after a failed operation). `None` if it is gone.
pub(super) fn recover_branch(s: &mut Session, id: Uuid) -> Result<Option<String>> {
    let project = s.project_id();
    match meta::get_branch(&mut s.admin, project, id)? {
        Some(row) => recover_row(s, row),
        None => Ok(None),
    }
}

pub(super) fn recover_snapshot(s: &mut Session, id: Uuid) -> Result<Option<String>> {
    let project = s.project_id();
    match meta::get_snapshot(&mut s.admin, project, id)? {
        Some(snapshot) => recover_snapshot_row(s, snapshot),
        None => Ok(None),
    }
}

fn recover_row(s: &mut Session, row: BranchRow) -> Result<Option<String>> {
    match (row.status, row.op.is_some()) {
        // The caller never received the password, so an unfinished creation
        // cannot be completed: remove what it made.
        (RecordStatus::Creating | RecordStatus::Failed, _) => {
            finish_drop(s, &row)?;
            Ok(Some(format!(
                "rolled back unfinished creation of branch '{}'",
                row.name
            )))
        }
        (RecordStatus::Deleting, _) => {
            finish_drop(s, &row)?;
            Ok(Some(format!("finished deleting branch '{}'", row.name)))
        }
        (RecordStatus::Ready, true) => recover_reset(s, &row),
        (RecordStatus::Ready, false) => Ok(None),
    }
}

/// Drop the database (`WITH (FORCE)`, which also ends a normalization
/// connection a dead operation left in it), then the login (retiring it if
/// other databases still reference it), then the row. Idempotent.
pub(super) fn finish_drop(s: &mut Session, row: &BranchRow) -> Result<()> {
    s.drop_database(&row.db_name)?;
    s.drop_login(&row.role_name, &format!("branch {} removed", row.id))?;
    s.step()?;
    meta::delete_branch(&mut s.admin, row.id)
}

pub(super) fn finish_drop_snapshot(s: &mut Session, snapshot: &SnapshotInfo) -> Result<()> {
    s.drop_database(&snapshot.db_name)?;
    s.step()?;
    meta::delete_snapshot(&mut s.admin, snapshot.id)
}

fn recover_snapshot_row(s: &mut Session, snapshot: SnapshotInfo) -> Result<Option<String>> {
    let label = format!("snapshot '{}@v{}'", snapshot.name, snapshot.version);
    match snapshot.status {
        RecordStatus::Creating | RecordStatus::Failed => {
            finish_drop_snapshot(s, &snapshot)?;
            Ok(Some(format!("rolled back unfinished creation of {label}")))
        }
        RecordStatus::Deleting => {
            finish_drop_snapshot(s, &snapshot)?;
            Ok(Some(format!("finished deleting {label}")))
        }
        RecordStatus::Ready => Ok(None),
    }
}

/// The rename transaction is atomic, so the only reachable states are:
/// `staging` (original untouched), `cutover` before commit (names unchanged),
/// and `cleanup` (renames committed). Anything else is left alone and reported.
fn recover_reset(s: &mut Session, row: &BranchRow) -> Result<Option<String>> {
    let phase = row.op_phase.as_deref().unwrap_or("");
    let (Some(replacement), Some(backup)) =
        (row.replacement_db.as_deref(), row.backup_db.as_deref())
    else {
        return inconsistent(row, "the operation row has no replacement/backup names");
    };
    let has_original = s.database_exists(&row.db_name)?;
    let has_replacement = s.database_exists(replacement)?;
    let has_backup = s.database_exists(backup)?;
    match phase {
        "staging" | "cutover" if has_original && !has_backup => {
            s.drop_database(replacement)?;
            // The original may have been closed for the takeover.
            s.set_connections(&row.db_name, true)?;
            s.step()?;
            meta::clear_op(&mut s.admin, row.id, row.generation, phase)?;
            Ok(Some(format!(
                "rolled back interrupted reset of branch '{}'",
                row.name
            )))
        }
        "cleanup" if has_original && !has_replacement => {
            finish_reset(
                s,
                row.id,
                row.generation,
                &row.db_name,
                &row.role_name,
                backup,
            )?;
            Ok(Some(format!("finished reset of branch '{}'", row.name)))
        }
        _ => inconsistent(
            row,
            &format!(
                "phase '{phase}' with original={has_original}, replacement={has_replacement}, backup={has_backup}"
            ),
        ),
    }
}

fn inconsistent(row: &BranchRow, detail: &str) -> Result<Option<String>> {
    bail!(
        "branch '{}' ({}) is in an inconsistent reset state and was not touched: {detail}",
        row.name,
        row.id
    )
}

/// After the renames: open the (new) original to its login, drop the old data,
/// clear the operation. Shared by the normal path and recovery. `generation` is
/// the branch's, after the renames bumped it.
pub(super) fn finish_reset(
    s: &mut Session,
    id: Uuid,
    generation: i32,
    db: &str,
    login: &str,
    backup: &str,
) -> Result<()> {
    s.set_connections(db, true)?;
    s.grant_connect(db, login)?;
    s.drop_database(backup)?;
    s.step()?;
    meta::clear_op(&mut s.admin, id, generation, "cleanup").context("cannot clear the reset record")
}

fn retry_retired_role(s: &mut Session, role: &str) -> Result<Option<String>> {
    s.step()?;
    if s.role_exists(role)? {
        match s
            .admin
            .batch_execute(&format!("DROP ROLE {}", super::admin::quote_ident(role)))
        {
            Ok(()) => {}
            // Still referenced from some database; try again later.
            Err(e)
                if e.code() == Some(&postgres::error::SqlState::DEPENDENT_OBJECTS_STILL_EXIST) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }
    }
    meta::forget_retired_role(&mut s.admin, role)?;
    Ok(Some(format!("dropped retired role {role}")))
}

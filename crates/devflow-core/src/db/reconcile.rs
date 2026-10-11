//! Crash recovery. Every operation records its row before its first external
//! effect; this module finishes or rolls back whatever a dead process left, by
//! inspecting `pg_database` and `pg_roles`. A failed operation runs the same
//! code on its own row.
use super::admin::{Session, bootstrap_leftovers, lock_lost, owner_name};
use super::meta::{self, BranchRow};
use super::scratch;
use super::{ReconcileReport, RecordStatus, SnapshotInfo};
use anyhow::{Context, Result, bail};
use uuid::Uuid;

/// Recover everything pending for the project. Per-record failures go to
/// `problems`; only a lost lock or an unreadable metadata database is an error.
pub(super) fn run(s: &mut Session) -> Result<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let project = s.project_id();

    // Before anything is built or opened: a login must not inherit the owner
    // by the time the owner holds `CONNECT` on a sibling's database. This is
    // a security invariant, so a failure stops the operation.
    let inheriting = stop_inheriting(s).context("cannot isolate the branch logins")?;
    if inheriting > 0 {
        report.actions.push(format!(
            "branch logins no longer inherit the project owner ({inheriting})"
        ));
    }
    match grant_owner_connect(s) {
        Ok(0) => {}
        Ok(granted) => report.actions.push(format!(
            "the project owner can connect to the {} of branches that lacked it",
            crate::text::counted(granted as u64, "database")
        )),
        Err(e) => {
            let e = s.lock_failure(e);
            if lock_lost(&e) {
                return Err(e);
            }
            report
                .problems
                .push(format!("owner access to branch databases: {e:#}"));
        }
    }

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

    // Scratch databases: finish what dead runs left, then say which ones the
    // configuration stopped listing. (Before the retired roles: a dropped
    // database may be what kept a role from being dropped.)
    scratch::recover(s, &mut report)?;
    match scratch::report_orphans(s, &mut report) {
        Ok(()) => {}
        Err(e) => {
            let e = s.lock_failure(e);
            if lock_lost(&e) {
                return Err(e);
            }
            report.problems.push(format!("scratch databases: {e:#}"));
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

/// Branch logins made by an earlier build inherited the project owner. The
/// owner now holds `CONNECT` on the branch databases (so that
/// `pg_database_size(current_database())` works for a session that acts as
/// the owner), which a login that still inherited it would use to reach its
/// siblings. Returns how many logins were changed.
fn stop_inheriting(s: &mut Session) -> Result<usize> {
    let project = s.project_id();
    let owner = owner_name(project);
    let logins: Vec<String> = meta::list_branches(&mut s.admin, project)?
        .into_iter()
        .map(|row| row.role_name)
        .collect();
    s.step()?;
    let inheriting: Vec<String> = s
        .admin
        .query(
            "SELECT r.rolname::text FROM pg_auth_members m \
             JOIN pg_roles r ON r.oid = m.member \
             WHERE m.roleid = to_regrole($1) AND m.inherit_option AND r.rolname::text = ANY($2)",
            &[&owner, &logins],
        )?
        .iter()
        .map(|row| row.get(0))
        .collect();
    for login in &inheriting {
        s.stop_inheriting(login, &owner)?;
    }
    Ok(inheriting.len())
}

/// Give the project owner `CONNECT` on the databases of the branches and
/// scratch databases that are ready and not being reset, where an earlier
/// build did not. Run after [`stop_inheriting`]. Returns how many databases
/// were changed; in-flight resets and builds get theirs when they finish.
fn grant_owner_connect(s: &mut Session) -> Result<usize> {
    let project = s.project_id();
    let owner = owner_name(project);
    let mut databases: Vec<String> = meta::list_branches(&mut s.admin, project)?
        .into_iter()
        .filter(|row| row.status == RecordStatus::Ready && row.op.is_none())
        .map(|row| row.db_name)
        .collect();
    databases.extend(
        meta::list_scratch(&mut s.admin, project)?
            .into_iter()
            .filter(|row| row.status == RecordStatus::Ready)
            .map(|row| row.db_name),
    );
    if databases.is_empty() {
        return Ok(0);
    }
    s.step()?;
    let lacking: Vec<String> = s
        .admin
        .query(
            "SELECT d.datname::text FROM pg_database d \
             WHERE d.datname::text = ANY($1) AND NOT has_database_privilege($2::name, d.oid, 'CONNECT')",
            &[&databases, &owner],
        )?
        .iter()
        .map(|row| row.get(0))
        .collect();
    for database in &lacking {
        s.grant_owner_connect(database, &owner)?;
    }
    Ok(lacking.len())
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

/// Drop the scratch databases, then the database (`WITH (FORCE)`, which also
/// ends a normalization connection a dead operation left in it), then the
/// login (retiring it if other databases still reference it), then the row.
/// The scratch databases go first: the login holds `CONNECT` on them, and a
/// row cannot outlive the branch row it points at. Idempotent.
pub(super) fn finish_drop(s: &mut Session, row: &BranchRow) -> Result<()> {
    scratch::drop_all(s, row.id)?;
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

/// After the renames: open the (new) original to its login, empty the scratch
/// databases again, drop the old data, clear the operation. Shared by the
/// normal path and recovery. `generation` is the branch's, after the renames
/// bumped it; the scratch databases record the generation they were emptied
/// for, so a repeat after a crash empties only the ones not done yet and
/// never what the user wrote after the reset.
pub(super) fn finish_reset(
    s: &mut Session,
    id: Uuid,
    generation: i32,
    db: &str,
    login: &str,
    backup: &str,
) -> Result<()> {
    s.set_connections(db, true)?;
    let owner = owner_name(s.project_id());
    s.grant_connect(db, login, &owner)?;
    scratch::reset(s, id, login, generation)?;
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

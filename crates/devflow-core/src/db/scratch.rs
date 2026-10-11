//! The scratch databases of a branch (`extra_databases`, plan decision D7).
//!
//! Branch logins keep `NOCREATEDB`, so tools that want a test or a shadow
//! database (Django's test runner, Prisma Migrate, Rails) cannot make their
//! own. Instead every branch has recorded scratch databases:
//!
//! - one row per database in `scratch_databases`, keyed by its own id and
//!   tied to the branch by id; the configured label is only a column, and
//!   the database name is generated (`df_<project8>_<branch12>_<n>`);
//! - created empty from `template0` with the closed-creation protocol of a
//!   branch's own database (closed, `PUBLIC` revoked, project owner granted
//!   `CREATE, TEMPORARY`, opened, schema ownership normalized, `CONNECT`
//!   granted to the branch's login last), owned by the admin;
//! - never cloned and never snapshotted: a clone of a branch gets new, empty
//!   ones, because they are separate databases;
//! - emptied (dropped and built again) when the branch is reset, in the
//!   cleanup phase of the reset, after the new data is in place;
//! - dropped with the branch, by recorded identity, before its login;
//! - kept as recorded orphans when the configuration stops listing their
//!   name, and dropped only on request.
//!
//! Every step records its intent on the row before the effect (`creating`,
//! `deleting`), so a run that dies is completed by the next mutation or by
//! `reconcile`: a `creating` row is built again from scratch, a `deleting`
//! row is dropped.
use super::admin::{Session, lock_lost, owner_name, scratch_db_name};
use super::meta::{self, NewScratch, ScratchRow};
use super::{ReconcileReport, RecordStatus};
use anyhow::{Context, Result};
use uuid::Uuid;

/// The names the configuration lists.
fn configured(s: &Session) -> Vec<String> {
    s.server.settings.extra_databases.clone()
}

/// Build `row`'s database empty and open it to `login`: drop what a dead
/// attempt (or the data being reset) left under its name, then create it
/// closed, restrict it, open it, make the project owner own the schema and
/// grant `CONNECT` last. The row must already be `creating` for the
/// generation it is built for. A failure is recorded on the row and the row
/// stays `creating`, to be built again.
fn build(s: &mut Session, row: &ScratchRow, login: &str) -> Result<()> {
    match build_steps(s, row, login) {
        Ok(()) => Ok(()),
        Err(error) => {
            let error = s.lock_failure(error);
            if !lock_lost(&error) {
                let _ = meta::set_scratch_error(&mut s.admin, row.id, &format!("{error:#}"));
            }
            Err(error.context(format!(
                "cannot build the '{}' scratch database {}",
                row.name, row.db_name
            )))
        }
    }
}

fn build_steps(s: &mut Session, row: &ScratchRow, login: &str) -> Result<()> {
    let owner = owner_name(s.project_id());
    s.drop_database(&row.db_name)?;
    s.create_empty_database(&row.db_name)?;
    s.restrict_database(&row.db_name, &owner)?;
    s.set_connections(&row.db_name, true)?;
    s.normalize(&row.db_name, &owner, None)?;
    s.grant_connect(&row.db_name, login, &owner)?;
    s.step()?;
    meta::finish_scratch_build(&mut s.admin, row.id)
}

/// Record the intent to create scratch database `name` of `branch`, before
/// anything exists.
fn insert(s: &mut Session, branch: Uuid, name: &str, generation: i32) -> Result<ScratchRow> {
    s.step()?;
    let project = s.project_id();
    let seq = meta::next_scratch_seq(&mut s.admin, branch)?;
    let id = Uuid::new_v4();
    let db_name = scratch_db_name(project, branch, seq);
    meta::insert_scratch(
        &mut s.admin,
        project,
        &NewScratch {
            id,
            branch_id: branch,
            name,
            seq,
            db_name: &db_name,
            generation,
        },
    )?;
    Ok(ScratchRow {
        id,
        branch_id: branch,
        name: name.to_string(),
        db_name,
        status: RecordStatus::Creating,
        generation,
        error: None,
    })
}

/// Give a `ready` branch (at `generation`, login `login`) every configured
/// scratch database it lacks, empty. Existing ones are left alone; a
/// half-made one is built again. Used for new branches and for branches that
/// exist before a name is added to the configuration.
pub(super) fn provision(s: &mut Session, branch: Uuid, login: &str, generation: i32) -> Result<()> {
    let project = s.project_id();
    for name in configured(s) {
        let existing = meta::scratch_of_branch(&mut s.admin, project, branch)?
            .into_iter()
            .find(|row| row.name == name);
        let row = match existing {
            Some(row) if row.status == RecordStatus::Ready => continue,
            // Being removed: finish that, then add the name afresh.
            Some(row) if row.status == RecordStatus::Deleting => {
                drop_row(s, &row)?;
                insert(s, branch, &name, generation)?
            }
            Some(row) => {
                s.step()?;
                meta::begin_scratch_build(&mut s.admin, row.id, generation)?;
                row
            }
            None => insert(s, branch, &name, generation)?,
        };
        build(s, &row, login)?;
    }
    Ok(())
}

/// The scratch databases a reset of `branch` empties: the ones the
/// configuration lists now, not being dropped. Orphans are not among them.
/// The reset records this set with its intent ([`meta::ResetIntent`]).
pub(super) fn selected_for_reset(s: &mut Session, branch: Uuid) -> Result<Vec<Uuid>> {
    let project = s.project_id();
    let names = configured(s);
    Ok(meta::scratch_of_branch(&mut s.admin, project, branch)?
        .into_iter()
        .filter(|row| names.contains(&row.name) && row.status != RecordStatus::Deleting)
        .map(|row| row.id)
        .collect())
}

/// After a branch's data was replaced (`generation` is its new one): drop and
/// build again, empty, each scratch database the reset recorded that is not
/// yet emptied for that generation, plus any that a dead run left half-built.
/// Idempotent, so the reset's recovery can run it again.
///
/// The set is the one journaled when the reset began, never recomputed from
/// the configuration: an interrupted reset finished after `extra_databases`
/// changed must still empty a database whose label was removed since (it
/// would keep what the reset was to clear), and must not touch one that was
/// an orphan then and has been listed again since (its contents were never
/// part of the reset). A reset journaled before the set was recorded falls
/// back to the configuration as it is now.
pub(super) fn reset(s: &mut Session, branch: Uuid, login: &str, generation: i32) -> Result<()> {
    let project = s.project_id();
    let recorded = match meta::reset_scratch_ids(&mut s.admin, branch)? {
        Some(ids) => ids,
        None => selected_for_reset(s, branch)?,
    };
    for row in meta::scratch_of_branch(&mut s.admin, project, branch)? {
        if !recorded.contains(&row.id) || row.status == RecordStatus::Deleting {
            continue;
        }
        if row.status == RecordStatus::Ready && row.generation >= generation {
            continue;
        }
        s.step()?;
        meta::begin_scratch_build(&mut s.admin, row.id, generation)?;
        build(s, &row, login)?;
    }
    Ok(())
}

/// Drop one scratch database and its row: record the intent, drop the
/// database (`WITH (FORCE)`), delete the row. Idempotent.
fn drop_row(s: &mut Session, row: &ScratchRow) -> Result<()> {
    s.step()?;
    meta::begin_scratch_delete(&mut s.admin, row.id)?;
    s.drop_database(&row.db_name)?;
    s.step()?;
    meta::delete_scratch(&mut s.admin, row.id)
}

/// Drop every scratch database of `branch` (orphans included), before its
/// own database and login. Idempotent.
pub(super) fn drop_all(s: &mut Session, branch: Uuid) -> Result<()> {
    let project = s.project_id();
    for row in meta::scratch_of_branch(&mut s.admin, project, branch)? {
        drop_row(s, &row)?;
    }
    Ok(())
}

/// Finish what dead runs left: `deleting` rows are dropped; `creating` rows of
/// a `ready` branch are built again when their name is still configured and
/// dropped when it is not (nothing of value was ever in them). Rows of a
/// branch that is itself being created, deleted or reset belong to that
/// operation's recovery. Per-row failures go to `problems`; only a lost lock
/// is an error.
pub(super) fn recover(s: &mut Session, report: &mut ReconcileReport) -> Result<()> {
    let project = s.project_id();
    let names = configured(s);
    for row in meta::list_scratch(&mut s.admin, project)? {
        let label = format!("scratch database '{}' ({})", row.name, row.db_name);
        let result = match row.status {
            RecordStatus::Ready => continue,
            RecordStatus::Deleting => {
                drop_row(s, &row).map(|()| Some(format!("finished dropping {label}")))
            }
            _ => recover_building(s, &row, &names, &label),
        };
        match result {
            Ok(Some(action)) => report.actions.push(action),
            Ok(None) => {}
            Err(e) => {
                let e = s.lock_failure(e);
                if lock_lost(&e) {
                    return Err(e);
                }
                report.problems.push(format!("{label}: {e:#}"));
            }
        }
    }
    Ok(())
}

fn recover_building(
    s: &mut Session,
    row: &ScratchRow,
    names: &[String],
    label: &str,
) -> Result<Option<String>> {
    let project = s.project_id();
    let Some(branch) = meta::get_branch(&mut s.admin, project, row.branch_id)? else {
        return Ok(None);
    };
    if branch.status != RecordStatus::Ready || branch.op.is_some() {
        return Ok(None);
    }
    if names.contains(&row.name) {
        s.step()?;
        meta::begin_scratch_build(&mut s.admin, row.id, branch.generation)?;
        build(s, row, &branch.role_name)?;
        Ok(Some(format!(
            "built {label} of branch '{}' again after an interrupted run",
            branch.name
        )))
    } else {
        drop_row(s, row)?;
        Ok(Some(format!(
            "dropped unfinished {label} of branch '{}': the configuration no longer lists it",
            branch.name
        )))
    }
}

/// The recorded scratch databases whose name the configuration no longer
/// lists: `(row, branch name)`.
fn orphans(s: &mut Session) -> Result<Vec<(ScratchRow, String)>> {
    let names = configured(s);
    orphans_unlisted_by(s, &names)
}

/// [`orphans`] against `names` instead of what the server was built with.
fn orphans_unlisted_by(s: &mut Session, names: &[String]) -> Result<Vec<(ScratchRow, String)>> {
    let project = s.project_id();
    let mut found = Vec::new();
    for row in meta::list_scratch(&mut s.admin, project)? {
        if names.contains(&row.name) || row.status == RecordStatus::Deleting {
            continue;
        }
        let branch = meta::get_branch(&mut s.admin, project, row.branch_id)?
            .map_or_else(|| row.branch_id.to_string(), |b| b.name);
        found.push((row, branch));
    }
    Ok(found)
}

/// Add a line to `report.orphans` for each orphan.
pub(super) fn report_orphans(s: &mut Session, report: &mut ReconcileReport) -> Result<()> {
    for (row, branch) in orphans(s)? {
        report.orphans.push(format!(
            "scratch database '{}' ({}) of branch '{branch}' is no longer in extra_databases; \
             it is kept until you drop it",
            row.name, row.db_name
        ));
    }
    Ok(())
}

/// Drop every scratch database `listed` (the configuration as it is on disk
/// now) does not list, by recorded identity; one line per database dropped.
pub(super) fn drop_orphans(s: &mut Session, listed: &[String]) -> Result<Vec<String>> {
    let mut dropped = Vec::new();
    for (row, branch) in orphans_unlisted_by(s, listed)? {
        drop_row(s, &row).with_context(|| {
            format!(
                "cannot drop the scratch database '{}' ({}) of branch '{branch}'",
                row.name, row.db_name
            )
        })?;
        dropped.push(format!(
            "dropped scratch database '{}' ({}) of branch '{branch}'",
            row.name, row.db_name
        ));
    }
    Ok(dropped)
}

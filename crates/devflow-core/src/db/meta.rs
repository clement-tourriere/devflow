//! The `devflow` metadata database: schema, migrations, row access. Row
//! identities here, never name prefixes, decide what devflow may touch.
use super::{BranchInfo, RecordStatus, SnapshotInfo};
use crate::config::CowCapability;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use postgres::{Client, Row};
use uuid::Uuid;

pub(super) const META_DB: &str = "devflow";
/// Comment devflow puts on the metadata database when it creates it. A
/// database named `devflow` without it is someone else's and is never touched.
pub(super) const META_MARKER: &str = "devflow metadata v1";
pub(super) const FOREIGN_META_DB: &str = "database 'devflow' exists and was not created by devflow; \
choose another server or drop/rename it";
/// Prefix of the staging database `devflow` is created under (see `admin`). A
/// database with this prefix is never adopted or dropped on the strength of its
/// name; only the mark decides, and leftovers are reported.
pub(super) const BOOTSTRAP_PREFIX: &str = "devflow_bootstrap_";
const SCHEMA_VERSION: i32 = 1;
/// Serializes schema migrations across processes ("devflow" + 1).
const MIGRATION_LOCK_KEY: i64 = 0x6465_7666_6c6f_7701;

const SCHEMA_V1: &str = "
CREATE TABLE projects (
    id          uuid PRIMARY KEY,
    name        text NOT NULL,
    owner_role  text NOT NULL UNIQUE,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE branches (
    id              uuid PRIMARY KEY,
    project_id      uuid NOT NULL REFERENCES projects(id),
    name            text NOT NULL,
    db_name         text NOT NULL UNIQUE,
    role_name       text NOT NULL UNIQUE,
    source_kind     text NOT NULL CHECK (source_kind IN ('empty','branch','snapshot')),
    source_id       uuid,
    instance_id     uuid,
    status          text NOT NULL CHECK (status IN ('creating','ready','deleting','failed')),
    cow             text NOT NULL DEFAULT 'unknown',
    error           text,
    generation      int NOT NULL DEFAULT 0,
    op              text CHECK (op IN ('reset')),
    op_phase        text CHECK (op_phase IN ('staging','cutover','cleanup')),
    replacement_db  text,
    backup_db       text,
    op_source_kind  text,
    op_source_id    uuid,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX branches_project ON branches(project_id);
CREATE INDEX branches_instance ON branches(instance_id);
CREATE TABLE snapshot_names (
    project_id    uuid NOT NULL REFERENCES projects(id),
    name          text NOT NULL,
    last_version  int NOT NULL DEFAULT 0,
    PRIMARY KEY (project_id, name)
);
CREATE TABLE snapshots (
    id                uuid PRIMARY KEY,
    project_id        uuid NOT NULL REFERENCES projects(id),
    name              text NOT NULL,
    version           int NOT NULL,
    db_name           text NOT NULL UNIQUE,
    source_branch_id  uuid,
    status            text NOT NULL CHECK (status IN ('creating','ready','deleting','failed')),
    created_at        timestamptz NOT NULL DEFAULT now(),
    UNIQUE (project_id, name, version)
);
CREATE TABLE retired_roles (
    role_name   text PRIMARY KEY,
    project_id  uuid NOT NULL REFERENCES projects(id),
    reason      text,
    created_at  timestamptz NOT NULL DEFAULT now()
);
";

/// Create or upgrade the schema under an advisory lock. Refuses to touch a
/// schema written by a newer devflow.
pub(super) fn migrate(client: &mut Client) -> Result<()> {
    let mut tx = client.transaction()?;
    tx.query("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK_KEY])?;
    tx.batch_execute(
        "CREATE TABLE IF NOT EXISTS meta (key text PRIMARY KEY, value text NOT NULL)",
    )?;
    let current: i32 = tx
        .query_opt("SELECT value FROM meta WHERE key = 'schema_version'", &[])?
        .map(|row| row.get::<_, String>(0).parse())
        .transpose()
        .context("corrupt schema_version in the devflow metadata database")?
        .unwrap_or(0);
    if current > SCHEMA_VERSION {
        bail!(
            "the devflow metadata schema is version {current}, newer than this devflow supports \
             ({SCHEMA_VERSION}); upgrade devflow"
        );
    }
    if current < 1 {
        tx.batch_execute(SCHEMA_V1)?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', $1) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            &[&SCHEMA_VERSION.to_string()],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Whether the database this client is connected to carries devflow's mark.
pub(super) fn is_marked(client: &mut Client) -> Result<bool> {
    Ok(client
        .query_one(
            "SELECT coalesce(shobj_description(oid, 'pg_database') = $1, false) \
             FROM pg_database WHERE datname = current_database()",
            &[&META_MARKER],
        )?
        .get(0))
}

/// Whether the schema exists (readers must not create it).
pub(super) fn schema_present(client: &mut Client) -> Result<bool> {
    Ok(client
        .query_one("SELECT to_regclass('public.branches') IS NOT NULL", &[])?
        .get(0))
}

// ---- status strings -------------------------------------------------------

pub(super) fn status_str(status: RecordStatus) -> &'static str {
    match status {
        RecordStatus::Creating => "creating",
        RecordStatus::Ready => "ready",
        RecordStatus::Deleting => "deleting",
        RecordStatus::Failed => "failed",
    }
}

fn parse_status(text: &str) -> Result<RecordStatus> {
    Ok(match text {
        "creating" => RecordStatus::Creating,
        "ready" => RecordStatus::Ready,
        "deleting" => RecordStatus::Deleting,
        "failed" => RecordStatus::Failed,
        other => bail!("unknown record status '{other}'"),
    })
}

pub(super) fn cow_str(cow: CowCapability) -> &'static str {
    match cow {
        CowCapability::Verified => "verified",
        CowCapability::Copy => "copy",
        CowCapability::Unknown => "unknown",
    }
}

fn parse_cow(text: &str) -> CowCapability {
    match text {
        "verified" => CowCapability::Verified,
        "copy" => CowCapability::Copy,
        _ => CowCapability::Unknown,
    }
}

// ---- branches -------------------------------------------------------------

const BRANCH_COLUMNS: &str = "id, name, db_name, role_name, source_kind, source_id, instance_id, \
     status, cow, error, generation, op, op_phase, replacement_db, backup_db, created_at";

pub(super) struct BranchRow {
    pub id: Uuid,
    pub name: String,
    pub db_name: String,
    pub role_name: String,
    pub source_kind: String,
    pub source_id: Option<Uuid>,
    pub instance_id: Option<Uuid>,
    pub status: RecordStatus,
    pub cow: CowCapability,
    pub error: Option<String>,
    pub generation: i32,
    /// `Some("reset")` while a reset is in flight.
    pub op: Option<String>,
    pub op_phase: Option<String>,
    pub replacement_db: Option<String>,
    pub backup_db: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl BranchRow {
    fn from_row(row: &Row) -> Result<BranchRow> {
        Ok(BranchRow {
            id: row.get("id"),
            name: row.get("name"),
            db_name: row.get("db_name"),
            role_name: row.get("role_name"),
            source_kind: row.get("source_kind"),
            source_id: row.get("source_id"),
            instance_id: row.get("instance_id"),
            status: parse_status(row.get("status"))?,
            cow: parse_cow(row.get("cow")),
            error: row.get("error"),
            generation: row.get("generation"),
            op: row.get("op"),
            op_phase: row.get("op_phase"),
            replacement_db: row.get("replacement_db"),
            backup_db: row.get("backup_db"),
            created_at: row.get("created_at"),
        })
    }

    pub(super) fn info(&self, size_bytes: Option<i64>) -> BranchInfo {
        BranchInfo {
            id: self.id,
            name: self.name.clone(),
            db_name: self.db_name.clone(),
            role_name: self.role_name.clone(),
            source_kind: self.source_kind.clone(),
            source_id: self.source_id,
            instance_id: self.instance_id,
            status: self.status,
            cow: self.cow,
            error: self.error.clone(),
            created_at: self.created_at,
            size_bytes,
            generation: self.generation,
            warnings: Vec::new(),
        }
    }
}

pub(super) struct NewBranch<'a> {
    pub id: Uuid,
    pub name: &'a str,
    pub db_name: &'a str,
    pub role_name: &'a str,
    pub source_kind: &'a str,
    pub source_id: Option<Uuid>,
    pub instance_id: Option<Uuid>,
    pub cow: CowCapability,
}

pub(super) fn insert_branch(client: &mut Client, project: Uuid, new: &NewBranch) -> Result<()> {
    client.execute(
        "INSERT INTO branches (id, project_id, name, db_name, role_name, source_kind, source_id, \
                               instance_id, status, cow) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'creating', $9)",
        &[
            &new.id,
            &project,
            &new.name,
            &new.db_name,
            &new.role_name,
            &new.source_kind,
            &new.source_id,
            &new.instance_id,
            &cow_str(new.cow),
        ],
    )?;
    Ok(())
}

pub(super) fn get_branch(
    client: &mut Client,
    project: Uuid,
    id: Uuid,
) -> Result<Option<BranchRow>> {
    client
        .query_opt(
            &format!("SELECT {BRANCH_COLUMNS} FROM branches WHERE id = $1 AND project_id = $2"),
            &[&id, &project],
        )?
        .map(|row| BranchRow::from_row(&row))
        .transpose()
}

pub(super) fn list_branches(client: &mut Client, project: Uuid) -> Result<Vec<BranchRow>> {
    client
        .query(
            &format!(
                "SELECT {BRANCH_COLUMNS} FROM branches WHERE project_id = $1 ORDER BY created_at, id"
            ),
            &[&project],
        )?
        .iter()
        .map(BranchRow::from_row)
        .collect()
}

pub(super) fn latest_for_instance(
    client: &mut Client,
    project: Uuid,
    instance: Uuid,
) -> Result<Option<BranchRow>> {
    client
        .query_opt(
            &format!(
                "SELECT {BRANCH_COLUMNS} FROM branches \
                 WHERE project_id = $1 AND instance_id = $2 AND status <> 'deleting' \
                 ORDER BY created_at DESC, id LIMIT 1"
            ),
            &[&project, &instance],
        )?
        .map(|row| BranchRow::from_row(&row))
        .transpose()
}

/// A journal write that matched no row means the record is no longer in the
/// state this operation read: it must stop without writing anything else.
pub(super) fn lost_race(what: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "lost a race on {what}: the record was changed by another operation; run `devflow db reconcile`"
    )
}

fn expect_one(updated: u64, what: &str) -> Result<()> {
    if updated == 1 {
        Ok(())
    } else {
        Err(lost_race(what))
    }
}

/// `from` → `to`, only while the row still has status `from`.
pub(super) fn set_branch_status(
    client: &mut Client,
    id: Uuid,
    from: RecordStatus,
    to: RecordStatus,
    error: Option<&str>,
) -> Result<()> {
    let updated = client.execute(
        "UPDATE branches SET status = $3, error = $4, updated_at = now() \
         WHERE id = $1 AND status = $2",
        &[&id, &status_str(from), &status_str(to), &error],
    )?;
    expect_one(updated, "the branch status")
}

/// Record the last failure of a branch whose status did not change.
pub(super) fn set_branch_error(client: &mut Client, id: Uuid, error: Option<&str>) -> Result<()> {
    client.execute(
        "UPDATE branches SET error = $2, updated_at = now() WHERE id = $1",
        &[&id, &error],
    )?;
    Ok(())
}

pub(super) fn delete_branch(client: &mut Client, id: Uuid) -> Result<()> {
    client.execute("DELETE FROM branches WHERE id = $1", &[&id])?;
    Ok(())
}

pub(super) struct ResetIntent<'a> {
    pub replacement_db: &'a str,
    pub backup_db: &'a str,
    pub source_kind: &'a str,
    pub source_id: Option<Uuid>,
}

/// Open the reset journal of a `ready` branch at `generation` with no operation.
pub(super) fn begin_reset(
    client: &mut Client,
    id: Uuid,
    generation: i32,
    intent: &ResetIntent,
) -> Result<()> {
    let updated = client.execute(
        "UPDATE branches SET op = 'reset', op_phase = 'staging', replacement_db = $3, \
                backup_db = $4, op_source_kind = $5, op_source_id = $6, error = NULL, \
                updated_at = now() \
         WHERE id = $1 AND generation = $2 AND status = 'ready' AND op IS NULL",
        &[
            &id,
            &generation,
            &intent.replacement_db,
            &intent.backup_db,
            &intent.source_kind,
            &intent.source_id,
        ],
    )?;
    expect_one(updated, "the reset record")
}

/// Advance the journal `from` → `to`, only if it is still where this reset left it.
pub(super) fn set_op_phase(
    client: &mut Client,
    id: Uuid,
    generation: i32,
    from: &str,
    to: &str,
) -> Result<()> {
    let updated = client.execute(
        "UPDATE branches SET op_phase = $4, updated_at = now() \
         WHERE id = $1 AND op = 'reset' AND op_phase = $3 AND generation = $2",
        &[&id, &generation, &from, &to],
    )?;
    expect_one(updated, "the reset record")
}

/// Close the journal that is in `phase` at `generation`; any other journal
/// (a successor's, for instance) is left alone.
pub(super) fn clear_op(client: &mut Client, id: Uuid, generation: i32, phase: &str) -> Result<()> {
    let updated = client.execute(
        "UPDATE branches SET op = NULL, op_phase = NULL, replacement_db = NULL, backup_db = NULL, \
                op_source_kind = NULL, op_source_id = NULL, updated_at = now() \
         WHERE id = $1 AND op = 'reset' AND op_phase = $3 AND generation = $2",
        &[&id, &generation, &phase],
    )?;
    expect_one(updated, "the reset record")
}

// ---- snapshots ------------------------------------------------------------

const SNAPSHOT_COLUMNS: &str = "id, name, version, db_name, source_branch_id, status, created_at";

fn snapshot_from_row(row: &Row) -> Result<SnapshotInfo> {
    Ok(SnapshotInfo {
        id: row.get("id"),
        name: row.get("name"),
        version: row.get("version"),
        db_name: row.get("db_name"),
        source_branch_id: row.get("source_branch_id"),
        status: parse_status(row.get("status"))?,
        created_at: row.get("created_at"),
        warnings: Vec::new(),
    })
}

/// Bump the monotonic version of `name` and insert a `creating` snapshot row in
/// one transaction. Versions are never reused, even after deletion.
pub(super) fn insert_snapshot(
    client: &mut Client,
    project: Uuid,
    id: Uuid,
    name: &str,
    db_name: &str,
    source_branch: Uuid,
) -> Result<i32> {
    let mut tx = client.transaction()?;
    let version: i32 = tx
        .query_one(
            "INSERT INTO snapshot_names (project_id, name, last_version) VALUES ($1, $2, 1) \
             ON CONFLICT (project_id, name) \
             DO UPDATE SET last_version = snapshot_names.last_version + 1 \
             RETURNING last_version",
            &[&project, &name],
        )?
        .get(0);
    tx.execute(
        "INSERT INTO snapshots (id, project_id, name, version, db_name, source_branch_id, status) \
         VALUES ($1, $2, $3, $4, $5, $6, 'creating')",
        &[&id, &project, &name, &version, &db_name, &source_branch],
    )?;
    tx.commit()?;
    Ok(version)
}

pub(super) fn get_snapshot(
    client: &mut Client,
    project: Uuid,
    id: Uuid,
) -> Result<Option<SnapshotInfo>> {
    client
        .query_opt(
            &format!("SELECT {SNAPSHOT_COLUMNS} FROM snapshots WHERE id = $1 AND project_id = $2"),
            &[&id, &project],
        )?
        .map(|row| snapshot_from_row(&row))
        .transpose()
}

pub(super) fn list_snapshots(client: &mut Client, project: Uuid) -> Result<Vec<SnapshotInfo>> {
    client
        .query(
            &format!(
                "SELECT {SNAPSHOT_COLUMNS} FROM snapshots WHERE project_id = $1 \
                 ORDER BY name, version"
            ),
            &[&project],
        )?
        .iter()
        .map(snapshot_from_row)
        .collect()
}

/// Latest `ready` version of `name`, or exactly `version`.
pub(super) fn find_ready_snapshot(
    client: &mut Client,
    project: Uuid,
    name: &str,
    version: Option<i32>,
) -> Result<Option<SnapshotInfo>> {
    client
        .query_opt(
            &format!(
                "SELECT {SNAPSHOT_COLUMNS} FROM snapshots \
                 WHERE project_id = $1 AND name = $2 AND status = 'ready' \
                   AND ($3::int IS NULL OR version = $3) \
                 ORDER BY version DESC LIMIT 1"
            ),
            &[&project, &name, &version],
        )?
        .map(|row| snapshot_from_row(&row))
        .transpose()
}

pub(super) fn get_snapshot_version(
    client: &mut Client,
    project: Uuid,
    name: &str,
    version: i32,
) -> Result<Option<SnapshotInfo>> {
    client
        .query_opt(
            &format!(
                "SELECT {SNAPSHOT_COLUMNS} FROM snapshots \
                 WHERE project_id = $1 AND name = $2 AND version = $3"
            ),
            &[&project, &name, &version],
        )?
        .map(|row| snapshot_from_row(&row))
        .transpose()
}

/// `from` → `to`, only while the row still has status `from`.
pub(super) fn set_snapshot_status(
    client: &mut Client,
    id: Uuid,
    from: RecordStatus,
    to: RecordStatus,
) -> Result<()> {
    let updated = client.execute(
        "UPDATE snapshots SET status = $3 WHERE id = $1 AND status = $2",
        &[&id, &status_str(from), &status_str(to)],
    )?;
    expect_one(updated, "the snapshot status")
}

pub(super) fn delete_snapshot(client: &mut Client, id: Uuid) -> Result<()> {
    client.execute("DELETE FROM snapshots WHERE id = $1", &[&id])?;
    Ok(())
}

// ---- retired roles --------------------------------------------------------

pub(super) fn retire_role(
    client: &mut Client,
    project: Uuid,
    role: &str,
    reason: &str,
) -> Result<()> {
    client.execute(
        "INSERT INTO retired_roles (role_name, project_id, reason) VALUES ($1, $2, $3) \
         ON CONFLICT (role_name) DO NOTHING",
        &[&role, &project, &reason],
    )?;
    Ok(())
}

pub(super) fn retired_roles(client: &mut Client, project: Uuid) -> Result<Vec<String>> {
    Ok(client
        .query(
            "SELECT role_name FROM retired_roles WHERE project_id = $1 ORDER BY created_at",
            &[&project],
        )?
        .iter()
        .map(|row| row.get(0))
        .collect())
}

pub(super) fn forget_retired_role(client: &mut Client, role: &str) -> Result<()> {
    client.execute("DELETE FROM retired_roles WHERE role_name = $1", &[&role])?;
    Ok(())
}

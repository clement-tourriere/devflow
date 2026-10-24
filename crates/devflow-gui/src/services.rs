//! Container services (Valkey, Redis, and whatever core adds) in the app: the
//! view models the Data tab and the overview draw, and the backend that does
//! the work.
//!
//! The models are plain data built from what core reports (`Service`,
//! `BranchView`, `SnapshotView`, the catalog's counts, the orphans), never from
//! names: a branch belongs to the workspace whose receipt carries the instance
//! id that owns it. Nothing is engine-specific here: an engine is a word
//! (`Engine::as_str`), so a new one in core renders without a change.
//!
//! A `sqlite` service is the exception that proves the rule: it is not a
//! container, so it has no branches, no Docker engine and no endpoint. Its
//! model ([`SqliteService`]) is a file per workspace and a list of snapshots,
//! read from local files alone on every refresh (no Docker, no server).
//!
//! The backend is a trait so the app can be tested without Docker:
//! [`CoreServices`] is the one that calls `devflow_core::services`; tests give
//! the state another. Every method is a small function over one core call.
use crate::{
    model::{ProjectRef, format_when},
    redaction::{self, RawName},
};
use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use devflow_core::{
    config::{CowCapability, Engine, SqliteSection},
    db::DataSource,
    project::Project,
    redact,
    services::{
        self, BranchView, Effects, LastClone, Operation, Service, ServiceStatus, SnapshotView,
        catalog::{self, CloneMethod, RecordState, SourceRef},
        probe, workspace,
    },
    setup::{self, BranchExpect, ResetSource, SqliteDatabase, WorkspaceStatus},
    sqlite::{self, How},
    worktree::Workspace,
};
use std::path::{Path, PathBuf};
use uuid::Uuid;

// ---- the models -------------------------------------------------------------------

/// What the app knows about a project's container services.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServicesModel {
    /// A count per configured service, from the catalog file alone (no
    /// Docker): the overview and the Data tab's cards.
    pub counts: Vec<ServiceCount>,
    /// Recorded data nothing uses any more.
    pub orphans: Orphans,
    /// Each configured service in detail, read on the Data tab (it asks
    /// Docker). `None` before the first such read.
    pub details: Option<Vec<ServiceDetail>>,
    /// The `sqlite` services: files, not containers, so they are read on
    /// every refresh with the counts (local files only, no Docker).
    pub sqlite: Vec<SqliteService>,
    /// Why the catalog could not be read.
    pub error: Option<String>,
}

impl ServicesModel {
    pub fn detail(&self, key: &str) -> Option<&ServiceDetail> {
        self.details.as_ref()?.iter().find(|d| d.key == key)
    }

    /// The `sqlite` service `key`, if the configuration has one.
    pub fn sqlite_service(&self, key: &str) -> Option<&SqliteService> {
        self.sqlite.iter().find(|s| s.key == key)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceCount {
    pub key: String,
    pub engine: String,
    /// False when the catalog could not be read: the numbers below are then
    /// placeholders, not zeros (`ServicesModel::error` says why).
    pub known: bool,
    /// Branches that are ready.
    pub branches: usize,
    /// Snapshots that are ready.
    pub snapshots: usize,
    /// Operations that did not end (a clone in progress or interrupted).
    pub unfinished: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Orphans {
    /// Services the catalog has and the configuration no longer does.
    pub services: Vec<String>,
    pub branches: Vec<OrphanBranch>,
    /// Workspaces whose receipt could not be read, one masked sentence each
    /// (`workspace: reason`). While there is any, who owns a branch is
    /// unknown: `branches` is empty then, and that is not "no orphans".
    pub incomplete: Vec<String>,
    /// Core's ownership scan ran and finished (`branches` is its answer, with
    /// `incomplete` saying what it could not read). False when it did not run
    /// or failed: no branch is then known to have an owner or not.
    pub scanned: bool,
}

/// Who owns a branch of a container service, as far as it is known. "No
/// workspace" is a statement only core's complete scan can make; a scan that
/// could not read a receipt, or did not run, leaves it unknown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ownership {
    /// A workspace of this project carries the instance that owns it.
    Workspace,
    /// The catalog records no owner for it (nothing to look up).
    Unowned,
    /// Confirmed: an owner is recorded, the scan read every receipt, and no
    /// workspace has that instance. Core lists it under orphans.
    Orphan,
    /// An owner is recorded but could not be matched to a workspace, and why.
    Unknown(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrphanBranch {
    pub service: String,
    pub id: Uuid,
    pub name: String,
    pub reason: String,
}

/// A workspace as a dialog showed it: its checkout, and the instance and
/// branch its receipt bound at that moment. The job hands the second to core,
/// which refuses to act if the workspace holds others by then (a
/// `setup --new-instance` in between), so a dialog never changes the data of a
/// replacement it did not show.
#[derive(Clone, Debug)]
pub struct BoundWorkspace {
    pub workspace: Workspace,
    pub expect: BranchExpect,
}

/// What an action on a container service was opened for: the project (by id
/// and root) and the service key, captured when the button or dialog was
/// drawn. A job runs against exactly these, or is refused if that project is no
/// longer the open one; it never reads the selection again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceTarget {
    pub project: ProjectRef,
    pub key: String,
}

/// The Docker engine the service lives on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DockerEngine {
    /// The first 12 characters of `docker info`'s ID.
    pub id: String,
    pub orbstack: bool,
}

/// The copy-on-write result for this engine: `verified`, `copy` or `unknown`,
/// and when and how it was found. Never assumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cow {
    pub capability: CowCapability,
    pub note: Option<String>,
    /// `None` when it was never measured on this engine.
    pub measured_at: Option<DateTime<Utc>>,
}

impl Cow {
    /// What the result means, for the pill's tooltip.
    pub fn help(&self) -> String {
        let when = self
            .measured_at
            .map(|at| format!(" Measured {}.", format_when(at)))
            .unwrap_or_default();
        match (self.capability, self.measured_at) {
            (CowCapability::Verified, _) => format!(
                "A 64 MiB test inside the Docker VM showed that a clone shares blocks with its \
                 source, so a branch takes almost no space until it diverges.{when}"
            ),
            (CowCapability::Copy, _) => format!(
                "The Docker VM's file system does not share blocks between copies: a branch is a \
                 full copy and takes as much space as its source.{when}{}",
                self.note
                    .as_deref()
                    .map(|n| format!(" {n}"))
                    .unwrap_or_default()
            ),
            (CowCapability::Unknown, None) => {
                "Not measured on this Docker engine yet. Measuring runs a short helper container \
                 (a 64 MiB test) in the Docker VM; devflow assumes nothing until then."
                    .to_string()
            }
            (CowCapability::Unknown, Some(_)) => format!(
                "The measurement was disturbed or could not be made, so devflow does not say \
                 either way.{when}{}",
                self.note
                    .as_deref()
                    .map(|n| format!(" {n}"))
                    .unwrap_or_default()
            ),
        }
    }
}

/// One container service with its branches and snapshots.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceDetail {
    pub key: String,
    pub engine: String,
    pub image: String,
    /// `None` when Docker could not be asked (`error` says why).
    pub docker: Option<DockerEngine>,
    pub cow: Cow,
    /// The configuration refuses a clone that is not copy-on-write.
    pub require_cow: bool,
    pub unfinished: usize,
    /// The last clone, snapshot or reset that finished, when core reports it.
    pub last_clone: Option<LastCloneRow>,
    pub branches: Vec<BranchRow>,
    pub snapshots: Vec<SnapshotRow>,
    pub error: Option<String>,
}

/// The last finished clone, snapshot or reset of a service, with how long its
/// source was unavailable where the engine has to stop it (MySQL).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastCloneRow {
    /// `clone`, `snapshot` or `reset`.
    pub operation: String,
    /// What it made, as drawn: the branch's name or `name@vN`, masked.
    pub target: String,
    /// The branch or snapshot it made, by identity.
    pub target_id: Uuid,
    pub finished: String,
    pub method: Option<String>,
    /// Milliseconds the source was stopped; `None` when no source was.
    pub source_downtime_ms: Option<u64>,
}

impl LastCloneRow {
    /// One sentence: what, when, how, and what it cost the source.
    pub fn text(&self) -> String {
        let mut text = format!(
            "Last {}: {} at {}",
            self.operation, self.target, self.finished
        );
        if let Some(method) = &self.method {
            text.push_str(&format!(", cloned by {method}"));
        }
        text.push_str(&match self.source_downtime_ms {
            Some(ms) => format!(
                "; its source was stopped for {}",
                crate::model::format_millis(ms)
            ),
            None => "; no source was stopped".to_string(),
        });
        text
    }
}

/// Where a branch stands: the record's state, and Docker's for the container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchState {
    Running,
    Stopped,
    /// Ready, with no container yet.
    NotStarted,
    /// The catalog has a container Docker no longer does.
    Missing,
    Creating,
    Deleting,
    Unknown,
}

impl BranchState {
    pub fn of(state: RecordState, container: &str) -> Self {
        match state {
            RecordState::Creating => Self::Creating,
            RecordState::Deleting => Self::Deleting,
            RecordState::Aborted => Self::Unknown,
            RecordState::Ready => match container {
                "running" => Self::Running,
                "exited" | "created" | "paused" | "dead" => Self::Stopped,
                "none" => Self::NotStarted,
                "missing" => Self::Missing,
                _ => Self::Unknown,
            },
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::NotStarted => "not started",
            Self::Missing => "container missing",
            Self::Creating => "creating",
            Self::Deleting => "deleting",
            Self::Unknown => "unknown",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            Self::Running => "The branch's container runs and answers on its port.",
            Self::Stopped => "The container is stopped; the data stays in its volume.",
            Self::NotStarted => "The branch exists but its container was never started.",
            Self::Missing => {
                "The catalog records a container that Docker does not have; Start creates it again from the volume."
            }
            Self::Creating => "An operation is creating this branch.",
            Self::Deleting => "An operation is removing this branch.",
            Self::Unknown => "Docker could not say what state the container is in.",
        }
    }

    pub fn running(self) -> bool {
        self == Self::Running
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchRow {
    pub id: Uuid,
    /// The name the branch was made under (usually its workspace's).
    pub name: String,
    /// The workspace whose receipt carries the instance id that owns it, as
    /// recorded: an identity for requests (`as_str`), drawn only through
    /// `shown` (a legal workspace name can look like a credential).
    pub workspace: Option<RawName>,
    /// The catalog records an owner instance for the branch (whether or not a
    /// workspace of the window carries it).
    pub owner_recorded: bool,
    pub state: BranchState,
    /// `127.0.0.1:port` on the host.
    pub endpoint: Option<String>,
    /// The OrbStack name other containers reach it by.
    pub container_endpoint: Option<String>,
    /// The workspace's URL for it with the password replaced by `***`.
    pub masked_url: Option<String>,
    pub source: String,
    pub captured: Option<String>,
    /// False when the source saved again while it was copied.
    pub capture_exact: bool,
    /// The capture is the source's last save, cloned without a new one.
    pub capture_last_save: bool,
    /// `reflink` or `copy`; `None` for an empty root.
    pub clone: Option<String>,
    pub allocated: Option<i64>,
    /// MySQL: the database the branch has (no credentials).
    pub database: Option<String>,
    /// Milliseconds the source was stopped while this branch was cloned from
    /// it (MySQL); `None` when it kept serving.
    pub source_downtime_ms: Option<u64>,
    pub generation: u32,
}

impl BranchRow {
    /// Who owns this branch, judged by core's scan `orphans`. A branch no
    /// workspace carries is an orphan only when core says so (its scan read
    /// every receipt); otherwise its owner is unknown, never "no workspace".
    pub fn ownership(&self, orphans: &Orphans) -> Ownership {
        if self.workspace.is_some() {
            Ownership::Workspace
        } else if !self.owner_recorded {
            Ownership::Unowned
        } else if orphans.branches.iter().any(|o| o.id == self.id) {
            Ownership::Orphan
        } else if !orphans.incomplete.is_empty() {
            Ownership::Unknown(format!(
                "{} workspace receipt(s) cannot be read ({})",
                orphans.incomplete.len(),
                orphans.incomplete.join("; ")
            ))
        } else if !orphans.scanned {
            Ownership::Unknown("core's scan for owners did not run or failed".to_string())
        } else {
            Ownership::Unknown(
                "the workspace that carries its instance is not among those listed".to_string(),
            )
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRow {
    pub id: Uuid,
    /// As recorded: the identity of requests, never drawn (see `label`).
    pub name: RawName,
    pub version: u32,
    pub source: String,
    pub captured: Option<String>,
    pub clone: Option<String>,
    pub allocated: Option<i64>,
    pub source_downtime_ms: Option<u64>,
    pub created: String,
}

impl SnapshotRow {
    /// `name@vN` as it is drawn, with credentials masked.
    pub fn label(&self) -> String {
        format!("{}@v{}", self.name.shown(), self.version)
    }
}

// ---- sqlite services: a file in each workspace ----------------------------------------

/// A `sqlite` service as the Data tab shows it: where its file is in a
/// workspace, the library that opens it, the snapshots, and the database of
/// every workspace. A part core could not read is an error, never an empty
/// list: "unknown" is not "none".
#[derive(Clone, Debug, PartialEq)]
pub struct SqliteService {
    /// The service key (the identity of every request).
    pub key: String,
    /// The configured path of the file, relative to a worktree.
    pub path: String,
    /// The version of the SQLite library devflow links (it is bundled).
    pub library: String,
    /// The configuration refuses a copy that does not share blocks.
    pub require_cow: bool,
    /// One per workspace whose checkout is present, or why they could not be
    /// listed.
    pub databases: Result<Vec<SqliteDb>, String>,
    /// The snapshots, or why the catalog could not be read.
    pub snapshots: Result<SqliteSnapshots, String>,
}

impl SqliteService {
    /// Databases that exist now; `None` when they could not be listed.
    pub fn database_count(&self) -> Option<usize> {
        self.databases
            .as_ref()
            .ok()
            .map(|all| all.iter().filter(|d| d.exists).count())
    }

    /// Snapshots that are ready; `None` when the catalog could not be read.
    pub fn snapshot_count(&self) -> Option<usize> {
        self.snapshots.as_ref().ok().map(|s| s.ready.len())
    }

    /// Snapshot operations that did not end (zero while unknown: the card
    /// says "unavailable" for the count itself).
    pub fn unfinished(&self) -> usize {
        self.snapshots.as_ref().map_or(0, |s| s.unfinished)
    }

    /// Ready snapshots whose file is gone or changed.
    pub fn damaged(&self) -> usize {
        self.snapshots.as_ref().map_or(0, |s| s.damaged)
    }

    /// The database of the workspace checked out at `checkout`.
    pub fn database_at(&self, checkout: &Path) -> Option<&SqliteDb> {
        self.databases
            .as_ref()
            .ok()?
            .iter()
            .find(|d| d.checkout == checkout)
    }
}

/// The ready snapshots of a `sqlite` service and what is wrong with the
/// catalog.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SqliteSnapshots {
    pub ready: Vec<SqliteSnapshotRow>,
    /// Snapshots being taken or removed, or whose making was abandoned.
    pub unfinished: usize,
    /// Ready snapshots whose file is gone or has another size than recorded.
    pub damaged: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SqliteSnapshotRow {
    pub id: Uuid,
    /// As recorded: the identity of requests, drawn only through `label`.
    pub name: RawName,
    pub version: u32,
    pub bytes: Option<u64>,
    /// `reflink` (the quiesced file was cloned) or `backup` (SQLite's backup
    /// API wrote a new file); `None` when core does not say.
    pub how: Option<String>,
    /// What it was taken from (`workspace:NAME`), masked.
    pub source: String,
    pub created: String,
    /// The file is gone or is not the one that was recorded.
    pub damaged: bool,
}

impl SqliteSnapshotRow {
    /// `name@vN` as it is drawn, with credentials masked.
    pub fn label(&self) -> String {
        format!("{}@v{}", self.name.shown(), self.version)
    }

    pub fn size_text(&self) -> String {
        self.bytes.map_or("size unknown".to_string(), sqlite_size)
    }

    /// "taken by reflink", "taken by backup".
    pub fn how_text(&self) -> String {
        match self.how.as_deref() {
            Some("reflink") => "taken by reflink (blocks shared)".to_string(),
            Some("backup") => "taken by backup (a new file)".to_string(),
            Some(other) => format!("taken by {other}"),
            None => "how it was taken is not recorded".to_string(),
        }
    }
}

/// One workspace's database file of a `sqlite` service.
#[derive(Clone, Debug, PartialEq)]
pub struct SqliteDb {
    /// The workspace's name as recorded; drawn through `RawName::shown`.
    pub workspace: RawName,
    /// The workspace's checkout: its identity (a name is only a label).
    pub checkout: PathBuf,
    /// The absolute path of the file in that checkout.
    pub file: PathBuf,
    pub is_default: bool,
    /// The identity the receipt gave the database.
    pub id: Option<Uuid>,
    pub exists: bool,
    pub bytes: Option<u64>,
    /// `wal` or `rollback`, from the file's header; `None` for an empty file,
    /// a file that is not a database, or no file.
    pub journal_mode: Option<String>,
    /// SQLite's files beside the database that exist now (`-wal`, `-shm`).
    pub sidecars: Vec<String>,
    /// What the receipt says it came from (`existing`, `empty`,
    /// `workspace:NAME`, `snapshot:NAME@vN`), masked; `None` when the
    /// workspace has no binding yet.
    pub source: Option<String>,
    /// devflow made this file for the workspace.
    pub owned: bool,
    /// `reflink` or `copy`; `None` when nothing was cloned.
    pub cloned: Option<String>,
}

/// Who a database file belongs to, in words, and what that allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileOwner {
    /// devflow created it for this workspace: it may reset or drop it.
    Workspace,
    /// The default workspace's file: the project's own, never replaced.
    Project,
    /// The workspace brought the file with it; devflow leaves it alone.
    AlreadyThere,
    /// The workspace has no binding for the service yet.
    NotSetUp,
}

impl FileOwner {
    pub fn of(is_default: bool, owned: bool, bound: bool) -> Self {
        if is_default {
            Self::Project
        } else if !bound {
            Self::NotSetUp
        } else if owned {
            Self::Workspace
        } else {
            Self::AlreadyThere
        }
    }

    /// The table's cell.
    pub fn label(self) -> &'static str {
        match self {
            Self::Workspace => "created by this workspace",
            Self::Project => "the project's own file",
            Self::AlreadyThere => "was already in the workspace",
            Self::NotSetUp => "not set up yet",
        }
    }

    /// Why core would refuse to reset or drop it, for a tooltip; `None` when
    /// it may. Core has the last word: this only explains the disabled button.
    pub fn refusal(self) -> Option<&'static str> {
        match self {
            Self::Workspace => None,
            Self::Project => Some(
                "This is the project's own file, in the default workspace. devflow never replaces or removes it.",
            ),
            Self::AlreadyThere => Some(
                "This file was already in the workspace, so devflow did not make it and leaves it alone.",
            ),
            Self::NotSetUp => {
                Some("This workspace has no database bound yet: set the workspace up first.")
            }
        }
    }
}

impl SqliteDb {
    pub fn owner(&self) -> FileOwner {
        FileOwner::of(self.is_default, self.owned, self.source.is_some())
    }

    pub fn size_text(&self) -> String {
        match (self.exists, self.bytes) {
            (false, _) => "not created yet".to_string(),
            (true, Some(bytes)) => sqlite_size(bytes),
            (true, None) => "size unknown".to_string(),
        }
    }

    /// The journal mode as the table words it.
    pub fn mode_text(&self) -> String {
        match (self.exists, self.journal_mode.as_deref()) {
            (false, _) => "—".to_string(),
            (true, Some("wal")) => "WAL".to_string(),
            (true, Some("rollback")) => "rollback journal".to_string(),
            (true, Some(other)) => other.to_string(),
            (true, None) => "not a database yet".to_string(),
        }
    }

    /// Where the data came from, in words.
    pub fn source_text(&self) -> String {
        self.source
            .as_deref()
            .map_or("not set up".to_string(), sqlite_source_text)
    }

    /// How the file got its data.
    pub fn clone_text(&self) -> String {
        if self.source.is_none() {
            return "—".to_string();
        }
        sqlite_clone_text(self.cloned.as_deref())
    }
}

fn sqlite_size(bytes: u64) -> String {
    crate::model::format_bytes(i64::try_from(bytes).unwrap_or(i64::MAX))
}

/// What a binding's `source` says, in words: `workspace:main` is "workspace
/// main", `snapshot:seed@v2` is "snapshot seed@v2".
pub fn sqlite_source_text(source: &str) -> String {
    match source {
        "existing" => "the file that was already there".to_string(),
        "empty" => "nothing (an empty start)".to_string(),
        other => other.split_once(':').map_or_else(
            || other.to_string(),
            |(kind, name)| format!("{kind} {name}"),
        ),
    }
}

/// How a database got its data: by which clone, or not cloned at all.
pub fn sqlite_clone_text(method: Option<&str>) -> String {
    match method {
        Some(method) => format!("cloned by {method}"),
        None => "nothing cloned".to_string(),
    }
}

/// A database as the receipt, the file and the workspace list say it is.
fn sqlite_db(database: &SqliteDatabase) -> SqliteDb {
    SqliteDb {
        workspace: RawName::from(database.workspace.clone()),
        checkout: database.workspace_path.clone(),
        file: database.path.clone(),
        is_default: database.is_default,
        id: database.id,
        exists: database.file.exists,
        bytes: database.file.bytes,
        journal_mode: database.file.journal_mode.clone(),
        sidecars: database.file.sidecars.clone(),
        source: database.source.clone(),
        owned: database.owned,
        cloned: database.cloned.map(|m| m.to_string()),
    }
}

fn how_word(how: How) -> &'static str {
    match how {
        How::Reflink => "reflink",
        How::Backup => "backup",
    }
}

/// The snapshots of one service, read from the catalog under the data
/// directory (a local file; nothing is created by reading).
fn sqlite_snapshots(project: &Project, key: &str) -> Result<SqliteSnapshots, String> {
    let read = || -> Result<SqliteSnapshots> {
        let store = sqlite::Store::open(project.id(), key)?;
        let ready = store.list()?;
        let unfinished = store.unfinished()?.len();
        let damaged = store.damaged()?;
        Ok(SqliteSnapshots {
            unfinished,
            damaged: damaged.len(),
            ready: ready
                .iter()
                .map(|s| SqliteSnapshotRow {
                    id: s.id,
                    name: RawName::from(s.name.clone()),
                    version: s.version,
                    bytes: s.bytes,
                    how: s.how.map(|h| how_word(h).to_string()),
                    source: s.source.clone(),
                    created: format_when(s.created_at),
                    damaged: damaged.contains(&s.label()),
                })
                .collect(),
        })
    };
    read().map_err(|error| format!("{error:#}"))
}

fn sqlite_service(project: &Project, key: &str, section: &SqliteSection) -> SqliteService {
    SqliteService {
        key: key.to_string(),
        path: section.path.clone(),
        library: sqlite::library_version().to_string(),
        require_cow: section.require_cow,
        databases: setup::sqlite_databases(project, key)
            .map(|all| all.iter().map(sqlite_db).collect())
            .map_err(|error| format!("{error:#}")),
        snapshots: sqlite_snapshots(project, key),
    }
}

/// The project's `sqlite` services, read from local files only.
pub fn sqlite_services(project: &Project) -> Vec<SqliteService> {
    project
        .config
        .sqlite_services()
        .map(|(key, section)| sqlite_service(project, key, section))
        .collect()
}

/// What settling a `sqlite` service's interrupted snapshots did, one sentence
/// each (the activity log's lines).
pub fn sqlite_reconciled_lines(report: &sqlite::Reconciled) -> Vec<String> {
    let mut lines = Vec::new();
    lines.extend(
        report
            .abandoned
            .iter()
            .map(|label| format!("removed the snapshot {label}, whose making was abandoned")),
    );
    lines.extend(
        report
            .removed
            .iter()
            .map(|label| format!("finished removing the snapshot {label}")),
    );
    lines.extend(report.damaged.iter().map(|label| {
        format!(
            "the file of snapshot {label} is gone or has changed; its data is lost, so the snapshot can only be deleted"
        )
    }));
    lines
}

/// A snapshot that was taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqliteSnapshotMade {
    /// `NAME@vN`.
    pub label: String,
    /// `reflink` or `backup`.
    pub how: Option<String>,
    pub bytes: Option<u64>,
}

/// A database that was replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqliteResetDone {
    /// What it was cloned from, as the receipt now records it.
    pub source: String,
    /// `reflink` or `copy`; `None` for an empty start.
    pub method: Option<String>,
    pub bytes: u64,
}

// ---- building them from core's reports ---------------------------------------------

fn clone_word(method: Option<CloneMethod>) -> Option<String> {
    method.map(|m| m.to_string())
}

fn short_id(id: &Uuid) -> String {
    id.simple().to_string()[..8].to_string()
}

fn describe_source(
    source: &SourceRef,
    branches: &[BranchView],
    snapshots: &[SnapshotView],
) -> String {
    match source {
        SourceRef::Empty => "empty".into(),
        SourceRef::Branch { id } => match branches.iter().find(|b| b.id == *id) {
            Some(branch) => format!("branch {}", branch.name),
            None => format!("branch {} (removed)", short_id(id)),
        },
        SourceRef::Snapshot { id } => match snapshots.iter().find(|s| s.id == *id) {
            Some(snapshot) => format!("snapshot {}@v{}", snapshot.name, snapshot.version),
            None => format!("snapshot {} (removed)", short_id(id)),
        },
    }
}

fn branch_row(
    key: &str,
    view: &BranchView,
    all: &[BranchView],
    snapshots: &[SnapshotView],
    workspaces: &[WorkspaceStatus],
) -> BranchRow {
    let owner = view.owner.and_then(|instance| {
        workspaces.iter().find(|w| {
            w.receipt
                .as_ref()
                .is_some_and(|r| r.instance_id == instance)
        })
    });
    let masked_url = owner
        .and_then(|w| w.receipt.as_ref()?.services.get(key))
        .filter(|binding| binding.branch_id == Some(view.id))
        .map(|binding| redact::text(&binding.url));
    BranchRow {
        id: view.id,
        name: view.name.clone(),
        workspace: owner.map(|w| RawName::from(w.workspace.name.clone())),
        owner_recorded: view.owner.is_some(),
        state: BranchState::of(view.state, &view.container),
        endpoint: view.endpoint.clone(),
        container_endpoint: view.container_endpoint.clone(),
        masked_url,
        source: describe_source(&view.source, all, snapshots),
        captured: view.captured_at.map(format_when),
        capture_exact: view.capture_exact,
        capture_last_save: view.capture_last_save,
        clone: clone_word(view.clone),
        allocated: view.allocated_bytes,
        database: view.database.clone(),
        source_downtime_ms: view.source_downtime_ms,
        generation: view.generation,
    }
}

fn last_clone_row(
    last: &LastClone,
    branches: &[BranchView],
    snapshots: &[SnapshotView],
) -> LastCloneRow {
    let target = branches
        .iter()
        .find(|b| b.id == last.target)
        .map(|b| b.name.clone())
        .or_else(|| {
            snapshots
                .iter()
                .find(|s| s.id == last.target)
                .map(|s| format!("{}@v{}", s.name, s.version))
        })
        .unwrap_or_else(|| format!("{} (removed)", short_id(&last.target)));
    LastCloneRow {
        operation: last.operation.clone(),
        target,
        target_id: last.target,
        finished: format_when(last.finished_at),
        method: clone_word(last.method),
        source_downtime_ms: last.source_downtime_ms,
    }
}

/// A service as the Data tab shows it, from core's status and snapshot list.
pub fn detail_of(
    status: &ServiceStatus,
    snapshots: &[SnapshotView],
    workspaces: &[WorkspaceStatus],
) -> ServiceDetail {
    ServiceDetail {
        key: status.key.clone(),
        engine: status.engine.as_str().to_string(),
        image: status.image.clone(),
        docker: Some(DockerEngine {
            id: status.docker.chars().take(12).collect(),
            orbstack: status.orbstack,
        }),
        cow: Cow {
            capability: status.cow,
            note: status.cow_note.clone(),
            measured_at: status.cow_measured_at,
        },
        require_cow: status.require_cow,
        unfinished: status.unfinished,
        last_clone: status
            .last_clone
            .as_ref()
            .map(|last| last_clone_row(last, &status.branches, snapshots)),
        branches: status
            .branches
            .iter()
            .map(|view| branch_row(&status.key, view, &status.branches, snapshots, workspaces))
            .collect(),
        snapshots: snapshots
            .iter()
            .filter(|s| s.state == RecordState::Ready)
            .map(|s| SnapshotRow {
                id: s.id,
                name: RawName::from(s.name.clone()),
                version: s.version,
                source: describe_source(&s.source, &status.branches, snapshots),
                captured: s.captured_at.map(format_when),
                clone: clone_word(s.clone),
                allocated: s.allocated_bytes,
                source_downtime_ms: s.source_downtime_ms,
                created: format_when(s.created_at),
            })
            .collect(),
        error: None,
    }
}

/// A service whose Docker side could not be read: what the configuration and
/// the catalog say, and why the rest is missing.
fn unavailable(
    key: &str,
    engine: Engine,
    image: String,
    require_cow: bool,
    why: String,
) -> ServiceDetail {
    ServiceDetail {
        key: key.to_string(),
        engine: engine.as_str().to_string(),
        image,
        docker: None,
        cow: Cow {
            capability: CowCapability::Unknown,
            note: None,
            measured_at: None,
        },
        require_cow,
        unfinished: 0,
        last_clone: None,
        branches: Vec::new(),
        snapshots: Vec::new(),
        error: Some(why),
    }
}

/// What the configuration names, read from the catalog file only.
fn counts_and_orphans(project: &Project, model: &mut ServicesModel) {
    let configured: Vec<(String, Engine)> = project
        .config
        .containers()
        .map(|(key, engine, _)| (key.to_string(), engine))
        .collect();
    let catalog = catalog::Store::path_for(project.id()).and_then(|path| catalog::read_file(&path));
    let mut catalog_read = true;
    let catalog = match catalog {
        Ok(found) => found,
        Err(error) => {
            model.error = Some(format!("{error:#}"));
            catalog_read = false;
            None
        }
    };
    for (key, engine) in &configured {
        let records = catalog.as_ref().and_then(|c| c.services.get(key));
        model.counts.push(ServiceCount {
            key: key.clone(),
            engine: engine.as_str().to_string(),
            known: catalog_read,
            branches: records.map_or(0, |r| {
                r.branches
                    .iter()
                    .filter(|b| b.state == RecordState::Ready)
                    .count()
            }),
            snapshots: records.map_or(0, |r| {
                r.snapshots
                    .iter()
                    .filter(|s| s.state == RecordState::Ready)
                    .count()
            }),
            unfinished: records.map_or(0, |r| r.unfinished().len()),
        });
    }
    if catalog.is_some() {
        match workspace::orphans(project) {
            Ok(found) => {
                model.orphans = Orphans {
                    services: found.services,
                    branches: found
                        .branches
                        .into_iter()
                        .map(|b| OrphanBranch {
                            service: b.service,
                            id: b.branch,
                            name: b.name,
                            reason: b.reason,
                        })
                        .collect(),
                    scanned: true,
                    incomplete: found
                        .incomplete
                        .into_iter()
                        .map(|p| format!("{}: {}", p.workspace, p.reason))
                        .collect(),
                }
            }
            Err(error) => {
                model.error.get_or_insert(format!("{error:#}"));
            }
        }
    }
}

// ---- the backend ---------------------------------------------------------------------

/// Everything the app asks of container services. [`CoreServices`] calls
/// core; a test substitutes its own.
pub trait ServiceBackend: Send + Sync + 'static {
    /// The services of `project`: counts and orphans from the catalog file
    /// and, with `full`, each service in detail (asking Docker).
    fn load(&self, project: &Project, workspaces: &[WorkspaceStatus], full: bool) -> ServicesModel;
    /// Measure copy-on-write on the service's Docker engine now.
    fn measure(&self, project: &Project, key: &str) -> Result<Cow>;
    /// Start a branch's container; the endpoint it answers on.
    fn up(&self, project: &Project, key: &str, branch: Uuid) -> Result<String>;
    /// Stop it, keeping its data; whether it was running.
    fn down(&self, project: &Project, key: &str, branch: Uuid) -> Result<bool>;
    /// Stop the containers of the workspace's own services, keeping their
    /// data and leaving the project's PostgreSQL server alone.
    fn stop_workspace(&self, project: &Project, workspace: &Workspace) -> workspace::Changes;
    /// Start them again.
    fn start_workspace(&self, project: &Project, workspace: &Workspace) -> workspace::Changes;
    /// The branch's connection URL with its password: for the clipboard only.
    fn url(&self, project: &Project, key: &str, branch: Uuid) -> Result<String>;
    /// Seal a new version of `name` from the workspace's branch; `name@vN`.
    /// Core refuses if the workspace no longer holds `expect`'s instance and
    /// branch.
    fn snapshot(
        &self,
        project: &Project,
        workspace: &Workspace,
        key: &str,
        name: &str,
        expect: BranchExpect,
    ) -> Result<String>;
    /// Remove a snapshot by its recorded id.
    fn delete_snapshot(&self, project: &Project, key: &str, snapshot: Uuid) -> Result<()>;
    /// Replace the workspace's data with a fresh clone of `to` (or of its
    /// recorded source); how it was cloned.
    /// Core refuses if the workspace no longer holds `expect`'s instance and
    /// branch.
    fn reset(
        &self,
        project: &Project,
        workspace: &Workspace,
        key: &str,
        to: Option<DataSource>,
        expect: BranchExpect,
    ) -> Result<String>;
    /// The identity a reset to `to` (or to the recorded source) resolves to
    /// now (core: `setup::reset_source`). The dialog pins it, and the reset is
    /// refused if the request resolves to another one by then.
    fn reset_source(
        &self,
        project: &Project,
        workspace: &Workspace,
        key: &str,
        to: Option<&DataSource>,
    ) -> Result<ResetSource>;
    /// What `op` would do to its source (core: `services::effects`).
    fn effects(&self, project: &Project, key: &str, op: Operation<'_>) -> Result<Effects> {
        let _ = (project, key, op);
        Ok(Effects::default())
    }
    /// Finish or roll back interrupted operations; what was done.
    fn reconcile(&self, project: &Project, key: &str) -> Result<Vec<String>>;
    /// Remove an orphan branch (container and volume) by recorded identity,
    /// if it is still an orphan.
    fn drop_branch(&self, project: &Project, key: &str, branch: Uuid) -> Result<()>;
    /// Remove a branch a workspace owns but whose volume is gone (its data is
    /// lost), if core finds the volume still missing.
    fn drop_lost_branch(
        &self,
        project: &Project,
        key: &str,
        branch: Uuid,
        volume: &str,
    ) -> Result<()>;
    /// Remove everything recorded for a service the configuration no longer has.
    fn forget(&self, project: &Project, key: &str) -> Result<Vec<String>>;
    /// Take a consistent copy of the `sqlite` database of `workspace` as the
    /// next version of snapshot `name`. With `expect`, core's check that the
    /// workspace still holds the database the dialog showed comes first.
    fn sqlite_snapshot(
        &self,
        project: &Project,
        workspace: &Workspace,
        key: &str,
        name: &str,
        expect: Option<BranchExpect>,
    ) -> Result<SqliteSnapshotMade>;
    /// Delete snapshot `name`@v`version` of a `sqlite` service.
    fn sqlite_delete_snapshot(
        &self,
        project: &Project,
        key: &str,
        name: &str,
        version: u32,
    ) -> Result<()>;
    /// Replace the `sqlite` database of `workspace` with a fresh copy of `to`,
    /// or of the source it was created from. Core refuses what it must (the
    /// default workspace's file, a file it did not make, a file another
    /// process has open).
    fn sqlite_reset(
        &self,
        project: &Project,
        workspace: &Workspace,
        key: &str,
        to: Option<DataSource>,
        expect: BranchExpect,
    ) -> Result<SqliteResetDone>;
    /// Remove the `sqlite` database of `workspace`; whether a file was there.
    fn sqlite_drop(
        &self,
        project: &Project,
        workspace: &Workspace,
        key: &str,
        expect: BranchExpect,
    ) -> Result<bool>;
}

/// The backend over `devflow_core::services`.
pub struct CoreServices;

impl ServiceBackend for CoreServices {
    fn load(&self, project: &Project, workspaces: &[WorkspaceStatus], full: bool) -> ServicesModel {
        let mut model = ServicesModel::default();
        counts_and_orphans(project, &mut model);
        // Files, not containers: always read, and never through Docker.
        model.sqlite = sqlite_services(project);
        if full {
            let details = project
                .config
                .containers()
                .map(|(key, engine, section)| {
                    let image = section.image(engine);
                    match Service::open(project, key).and_then(|svc| {
                        let status = svc.status_cached()?;
                        let snapshots = svc.snapshot_views()?;
                        Ok(detail_of(&status, &snapshots, workspaces))
                    }) {
                        Ok(detail) => detail,
                        Err(error) => unavailable(
                            key,
                            engine,
                            image,
                            section.require_cow,
                            format!("{error:#}"),
                        ),
                    }
                })
                .collect();
            model.details = Some(details);
        }
        redaction::services(&mut model);
        model
    }

    fn measure(&self, project: &Project, key: &str) -> Result<Cow> {
        let svc = Service::open(project, key)?;
        let found = probe::capability(svc.docker_id(), true);
        Ok(Cow {
            capability: found.capability,
            note: found.note,
            measured_at: Some(found.measured_at),
        })
    }

    fn up(&self, project: &Project, key: &str, branch: Uuid) -> Result<String> {
        let endpoint = Service::open(project, key)?.up(branch)?;
        Ok(format!("127.0.0.1:{}", endpoint.port))
    }

    fn down(&self, project: &Project, key: &str, branch: Uuid) -> Result<bool> {
        Service::open(project, key)?.down(branch)
    }

    fn stop_workspace(&self, project: &Project, workspace: &Workspace) -> workspace::Changes {
        workspace::stop_services(project, workspace)
    }

    fn start_workspace(&self, project: &Project, workspace: &Workspace) -> workspace::Changes {
        workspace::start_services(project, workspace)
    }

    fn url(&self, project: &Project, key: &str, branch: Uuid) -> Result<String> {
        let svc = Service::open(project, key)?;
        let record = svc
            .branch(branch)?
            .with_context(|| format!("service '{key}' has no branch {branch}"))?;
        Ok(svc.endpoint(&record)?.url)
    }

    fn snapshot(
        &self,
        project: &Project,
        workspace: &Workspace,
        key: &str,
        name: &str,
        expect: BranchExpect,
    ) -> Result<String> {
        let record = setup::service_snapshot(project, workspace, key, name, Some(expect))?;
        Ok(format!("{}@v{}", record.name, record.version))
    }

    fn delete_snapshot(&self, project: &Project, key: &str, snapshot: Uuid) -> Result<()> {
        Service::open(project, key)?.remove_snapshot_by_id(snapshot)?;
        Ok(())
    }

    fn reset(
        &self,
        project: &Project,
        workspace: &Workspace,
        key: &str,
        to: Option<DataSource>,
        expect: BranchExpect,
    ) -> Result<String> {
        let record = setup::service_reset(project, workspace, key, to, Some(expect))?;
        Ok(record
            .clone
            .map_or_else(|| "empty".to_string(), |m| m.to_string()))
    }

    fn reset_source(
        &self,
        project: &Project,
        workspace: &Workspace,
        key: &str,
        to: Option<&DataSource>,
    ) -> Result<ResetSource> {
        setup::reset_source(project, workspace, key, to)
    }

    fn effects(&self, project: &Project, key: &str, op: Operation<'_>) -> Result<Effects> {
        services::effects(project, key, op)
    }

    fn reconcile(&self, project: &Project, key: &str) -> Result<Vec<String>> {
        // A `sqlite` service has no containers: only its snapshots can be
        // left half-made.
        if project.config.sqlite_services().any(|(k, _)| k == key) {
            return Ok(sqlite_reconciled_lines(&setup::sqlite_reconcile(
                project, key,
            )?));
        }
        let report = Service::open(project, key)?.reconcile()?;
        let mut lines = report.actions;
        lines.extend(report.in_progress);
        lines.extend(report.orphans.into_iter().map(|o| format!("{o} (kept)")));
        lines.extend(
            report
                .problems
                .into_iter()
                .map(|p| format!("needs an operator: {p}")),
        );
        Ok(lines)
    }

    fn drop_branch(&self, project: &Project, key: &str, branch: Uuid) -> Result<()> {
        // Core reads the files and worktrees again: a branch a workspace holds
        // by now is not an orphan, and is kept.
        workspace::drop_orphan(project, key, branch)
    }

    fn drop_lost_branch(
        &self,
        project: &Project,
        key: &str,
        branch: Uuid,
        volume: &str,
    ) -> Result<()> {
        // Core reads the files again, takes the branch's lock, reads the record
        // under it and asks Docker whether the volume it names is still
        // missing; a branch whose data is there, or whose volume was replaced,
        // is kept.
        workspace::drop_lost_branch(project, key, branch, volume)
    }

    fn forget(&self, project: &Project, key: &str) -> Result<Vec<String>> {
        workspace::forget(project, key)
    }

    fn sqlite_snapshot(
        &self,
        project: &Project,
        workspace: &Workspace,
        key: &str,
        name: &str,
        expect: Option<BranchExpect>,
    ) -> Result<SqliteSnapshotMade> {
        let made = setup::sqlite_snapshot(project, workspace, key, name, expect)?;
        Ok(SqliteSnapshotMade {
            label: made.label(),
            how: made.how.map(|h| how_word(h).to_string()),
            bytes: made.bytes,
        })
    }

    fn sqlite_delete_snapshot(
        &self,
        project: &Project,
        key: &str,
        name: &str,
        version: u32,
    ) -> Result<()> {
        setup::sqlite_snapshot_delete(project, key, name, version)?;
        Ok(())
    }

    fn sqlite_reset(
        &self,
        project: &Project,
        workspace: &Workspace,
        key: &str,
        to: Option<DataSource>,
        expect: BranchExpect,
    ) -> Result<SqliteResetDone> {
        let done = setup::sqlite_reset(project, workspace, key, to, Some(expect))?;
        Ok(SqliteResetDone {
            source: done.source,
            method: done.method.map(|m| m.to_string()),
            bytes: done.bytes,
        })
    }

    fn sqlite_drop(
        &self,
        project: &Project,
        workspace: &Workspace,
        key: &str,
        expect: BranchExpect,
    ) -> Result<bool> {
        setup::sqlite_drop(project, workspace, key, Some(expect))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devflow_core::receipt::{CloneReport, Receipt, ServiceBinding};
    use devflow_core::worktree::Workspace;
    use std::path::PathBuf;
    // Plain tests, as in the views' test modules.
    use std::prelude::v1::test;

    // kingfisher:ignore
    const URL: &str = "redis://:review-password@127.0.0.1:6381/0";

    fn workspace(name: &str, receipt: Option<Receipt>) -> WorkspaceStatus {
        WorkspaceStatus {
            workspace: Workspace {
                name: name.into(),
                branch: Some(name.into()),
                path: PathBuf::from(format!("/tmp/shop/{name}")),
                git_dir: PathBuf::from(format!("/tmp/shop/{name}/.git")),
                is_default: false,
                locked: false,
                missing: false,
                head: None,
                fingerprint: None,
            },
            status: receipt.as_ref().map(Receipt::status),
            receipt,
            busy: false,
            drift: None,
            env_file: None,
            recipes: None,
        }
    }

    fn view(name: &str, owner: Option<Uuid>, container: &str, port: u16) -> BranchView {
        BranchView {
            id: Uuid::new_v4(),
            name: name.into(),
            owner,
            state: RecordState::Ready,
            container: container.into(),
            container_id: Some("abc".into()),
            container_name: format!("devflow-x-{name}"),
            volume: format!("vol-{name}"),
            image: "valkey/valkey:8".into(),
            pin: "valkey/valkey@sha256:1".into(),
            port: Some(port),
            endpoint: Some(format!("127.0.0.1:{port}")),
            container_endpoint: Some(format!("{name}.cache.shop-0b3c9d2e.devflow.orb.local:6379")),
            source: SourceRef::Empty,
            captured_at: None,
            capture_exact: true,
            capture_last_save: false,
            clone: None,
            allocated_bytes: None,
            database: None,
            source_downtime_ms: None,
            generation: 1,
            created_at: Utc::now(),
        }
    }

    fn status(branches: Vec<BranchView>) -> ServiceStatus {
        ServiceStatus {
            key: "cache".into(),
            engine: Engine::Valkey,
            image: "valkey/valkey:8".into(),
            docker: "3f2a9c1d7e04aabbccdd".into(),
            orbstack: true,
            cow: CowCapability::Verified,
            cow_note: None,
            cow_measured_at: Some(Utc::now()),
            require_cow: false,
            branches,
            snapshots: 0,
            unfinished: 0,
            last_clone: None,
        }
    }

    #[test]
    fn a_branch_belongs_to_the_workspace_whose_receipt_carries_its_owner_instance() {
        let mut receipt = Receipt::new(Uuid::new_v4(), "main", None);
        let main = view("main", Some(receipt.instance_id), "running", 6381);
        receipt.services.insert(
            "cache".into(),
            ServiceBinding::container(
                Engine::Valkey,
                main.id,
                URL.into(),
                None,
                "empty".into(),
                CloneReport {
                    method: None,
                    captured_at: None,
                    allocated_bytes: None,
                    source_downtime_ms: None,
                },
                None,
            ),
        );
        // A branch whose name matches a workspace but whose owner does not: not its.
        let stray = view("feature", Some(Uuid::new_v4()), "exited", 6382);
        let detail = detail_of(
            &status(vec![main, stray]),
            &[],
            &[workspace("main", Some(receipt)), workspace("feature", None)],
        );
        assert_eq!(
            detail.branches[0].workspace.as_ref().map(RawName::as_str),
            Some("main")
        );
        assert_eq!(detail.branches[1].workspace, None, "names prove nothing");
        // The row shows the workspace's URL with the password masked.
        let masked = detail.branches[0].masked_url.as_deref().unwrap();
        assert_eq!(masked, "redis://:***@127.0.0.1:6381/0");
        assert!(!format!("{detail:?}").contains("review-password"));
        assert_eq!(detail.branches[1].masked_url, None);
        assert_eq!(detail.docker.as_ref().unwrap().id, "3f2a9c1d7e04");
        assert!(detail.docker.as_ref().unwrap().orbstack);
        assert_eq!(detail.engine, "valkey");
    }

    #[test]
    fn no_workspace_is_said_only_when_core_confirmed_it() {
        let owned = view("main", Some(Uuid::new_v4()), "running", 6381);
        let ownerless = view("scratch", None, "exited", 6382);
        let stray = view("feature", Some(Uuid::new_v4()), "exited", 6383);
        let detail = detail_of(&status(vec![owned, ownerless, stray]), &[], &[]);
        let [_, unowned, stray] = &detail.branches[..] else {
            panic!("{:?}", detail.branches);
        };
        let scanned = |branches: Vec<OrphanBranch>, incomplete: Vec<String>| Orphans {
            services: Vec::new(),
            branches,
            incomplete,
            scanned: true,
        };
        let orphan = |row: &BranchRow| OrphanBranch {
            service: "cache".into(),
            id: row.id,
            name: row.name.clone(),
            reason: String::new(),
        };
        // Core listed it: confirmed.
        assert_eq!(
            stray.ownership(&scanned(vec![orphan(stray)], Vec::new())),
            Ownership::Orphan
        );
        // A receipt could not be read: the scan lists no orphan, and the
        // branch's owner is unknown (with the receipt problem), not absent.
        let unreadable = scanned(Vec::new(), vec!["feature: invalid receipt".into()]);
        let Ownership::Unknown(why) = stray.ownership(&unreadable) else {
            panic!("unknown expected");
        };
        assert!(why.contains("feature: invalid receipt"), "{why}");
        // The scan did not run or failed.
        assert!(matches!(
            stray.ownership(&Orphans::default()),
            Ownership::Unknown(_)
        ));
        // Read in full, yet listed nowhere and carried by no listed workspace.
        assert!(matches!(
            stray.ownership(&scanned(Vec::new(), Vec::new())),
            Ownership::Unknown(_)
        ));
        // No recorded owner is not an unknown one, whatever the scan says.
        assert_eq!(unowned.ownership(&unreadable), Ownership::Unowned);
    }

    #[test]
    fn a_state_is_the_records_and_then_dockers() {
        let word = BranchState::of;
        assert_eq!(word(RecordState::Ready, "running"), BranchState::Running);
        assert_eq!(word(RecordState::Ready, "exited"), BranchState::Stopped);
        assert_eq!(word(RecordState::Ready, "none"), BranchState::NotStarted);
        assert_eq!(word(RecordState::Ready, "missing"), BranchState::Missing);
        assert_eq!(word(RecordState::Ready, "weird"), BranchState::Unknown);
        assert_eq!(
            word(RecordState::Creating, "running"),
            BranchState::Creating
        );
        assert_eq!(
            word(RecordState::Deleting, "running"),
            BranchState::Deleting
        );
        for state in [
            BranchState::Running,
            BranchState::Stopped,
            BranchState::NotStarted,
            BranchState::Missing,
            BranchState::Creating,
            BranchState::Deleting,
            BranchState::Unknown,
        ] {
            assert!(!state.label().is_empty() && !state.help().is_empty());
        }
    }

    #[test]
    fn a_source_is_named_by_what_it_resolves_to() {
        let main = view("main", None, "running", 6381);
        let mut feature = view("feature", None, "running", 6382);
        feature.source = SourceRef::Branch { id: main.id };
        let mut orphan = view("orphan", None, "running", 6383);
        orphan.source = SourceRef::Branch { id: Uuid::new_v4() };
        let snapshot = SnapshotView {
            id: Uuid::new_v4(),
            name: "seed".into(),
            version: 2,
            state: RecordState::Ready,
            volume: "v".into(),
            source: SourceRef::Branch { id: main.id },
            image: "valkey/valkey:8".into(),
            pin: "p".into(),
            captured_at: None,
            capture_exact: true,
            capture_last_save: false,
            clone: Some(CloneMethod::Reflink),
            allocated_bytes: Some(12 * 1024 * 1024),
            source_downtime_ms: None,
            created_at: Utc::now(),
        };
        let mut from_snapshot = view("seeded", None, "running", 6384);
        from_snapshot.source = SourceRef::Snapshot { id: snapshot.id };
        let detail = detail_of(
            &status(vec![main, feature, orphan, from_snapshot]),
            std::slice::from_ref(&snapshot),
            &[],
        );
        let sources: Vec<_> = detail.branches.iter().map(|b| b.source.as_str()).collect();
        assert_eq!(sources[0], "empty");
        assert_eq!(sources[1], "branch main");
        assert!(sources[2].ends_with("(removed)"), "{sources:?}");
        assert_eq!(sources[3], "snapshot seed@v2");
        assert_eq!(detail.snapshots[0].label(), "seed@v2");
        assert_eq!(detail.snapshots[0].source, "branch main");
        assert_eq!(detail.snapshots[0].clone.as_deref(), Some("reflink"));
    }

    #[test]
    fn each_copy_on_write_result_says_what_it_means() {
        let cow = |capability, measured: bool| Cow {
            capability,
            note: Some("the guest says so".into()),
            measured_at: measured.then(Utc::now),
        };
        assert!(
            cow(CowCapability::Verified, true)
                .help()
                .contains("shares blocks")
        );
        let copy = cow(CowCapability::Copy, true).help();
        assert!(
            copy.contains("full copy") && copy.contains("the guest says so"),
            "{copy}"
        );
        let never = cow(CowCapability::Unknown, false).help();
        assert!(
            never.contains("Not measured") && never.contains("assumes nothing"),
            "{never}"
        );
        let disturbed = cow(CowCapability::Unknown, true).help();
        assert!(disturbed.contains("does not say either way"), "{disturbed}");
    }

    #[test]
    fn the_last_clone_says_what_it_made_and_what_it_cost_its_source() {
        let mut main = view("main", None, "running", 3307);
        main.database = Some("df_main".into());
        main.source_downtime_ms = Some(1800);
        let target = main.id;
        let mut status = status(vec![main]);
        status.engine = Engine::Mysql;
        status.last_clone = Some(LastClone {
            operation: "clone".into(),
            target,
            finished_at: Utc::now(),
            method: Some(CloneMethod::Reflink),
            source_downtime_ms: Some(1800),
        });
        let detail = detail_of(&status, &[], &[]);
        assert_eq!(detail.engine, "mysql");
        assert_eq!(detail.branches[0].database.as_deref(), Some("df_main"));
        assert_eq!(detail.branches[0].source_downtime_ms, Some(1800));
        let text = detail.last_clone.as_ref().unwrap().text();
        assert!(
            text.starts_with("Last clone: main at ")
                && text.contains("cloned by reflink")
                && text.ends_with("its source was stopped for 1.8 s"),
            "{text}"
        );
        // An engine whose source keeps serving reports no downtime.
        status.last_clone.as_mut().unwrap().source_downtime_ms = None;
        let text = detail_of(&status, &[], &[]).last_clone.unwrap().text();
        assert!(text.ends_with("no source was stopped"), "{text}");
        // A target that is gone is named by its id.
        status.last_clone.as_mut().unwrap().target = Uuid::new_v4();
        let text = detail_of(&status, &[], &[]).last_clone.unwrap().text();
        assert!(text.contains("(removed)"), "{text}");
    }

    /// Every text the Data tab draws for `detail`, from the same functions the
    /// view calls (the kit has no accessor for painted text).
    fn drawn(detail: &ServiceDetail) -> Vec<String> {
        let mut texts = vec![detail.image.clone(), format!("{:?}", detail.error)];
        texts.extend(detail.last_clone.iter().map(LastCloneRow::text));
        for branch in &detail.branches {
            texts.push(crate::views::branch_facts(branch));
            texts.extend([
                branch.name.clone(),
                branch.source.clone(),
                crate::views::branch_label(branch),
            ]);
            texts.extend(branch.endpoint.clone());
            texts.extend(branch.masked_url.clone());
        }
        for snapshot in &detail.snapshots {
            texts.extend([snapshot.label(), snapshot.source.clone()]);
        }
        texts
    }

    #[test]
    fn the_last_snapshot_and_every_other_name_is_masked_where_the_model_is_built() {
        // A legal branch and snapshot name that is also a credential.
        // kingfisher:ignore
        let name = "password=hunter2";
        let snapshot = SnapshotView {
            id: Uuid::new_v4(),
            name: name.into(),
            version: 3,
            state: RecordState::Ready,
            volume: "v".into(),
            source: SourceRef::Empty,
            image: "valkey/valkey:8".into(),
            pin: "p".into(),
            captured_at: None,
            capture_exact: true,
            capture_last_save: false,
            clone: Some(CloneMethod::Reflink),
            allocated_bytes: None,
            source_downtime_ms: None,
            created_at: Utc::now(),
        };
        // The workspace that owns it is named the same: a legal name too.
        let mut receipt = Receipt::new(Uuid::new_v4(), name, None);
        let mut odd = view(name, Some(receipt.instance_id), "running", 6381);
        odd.source = SourceRef::Snapshot { id: snapshot.id };
        receipt.services.insert(
            "cache".into(),
            ServiceBinding::container(
                Engine::Valkey,
                odd.id,
                URL.into(),
                None,
                format!("workspace:{name}"),
                CloneReport {
                    method: None,
                    captured_at: None,
                    allocated_bytes: None,
                    source_downtime_ms: None,
                },
                None,
            ),
        );
        let owner = workspace(name, Some(receipt));
        let (branch_id, snapshot_id) = (odd.id, snapshot.id);
        for (operation, target) in [("clone", branch_id), ("snapshot", snapshot_id)] {
            let mut status = status(vec![odd.clone()]);
            status.last_clone = Some(LastClone {
                operation: operation.into(),
                target,
                finished_at: Utc::now(),
                method: Some(CloneMethod::Reflink),
                source_downtime_ms: None,
            });
            let mut model = ServicesModel {
                details: Some(vec![detail_of(
                    &status,
                    std::slice::from_ref(&snapshot),
                    std::slice::from_ref(&owner),
                )]),
                ..ServicesModel::default()
            };
            // What the raw report would have drawn: the fixture leaks.
            let raw = drawn(&model.details.as_ref().unwrap()[0]).join("\n");
            assert!(raw.contains("hunter2"), "the fixture does not leak: {raw}");
            redaction::services(&mut model);
            let detail = &model.details.as_ref().unwrap()[0];
            let text = drawn(detail).join("\n");
            assert!(!text.contains("hunter2"), "{operation}: {text}");
            assert!(!format!("{model:?}").contains("hunter2"));
            let last = detail.last_clone.as_ref().unwrap();
            assert!(last.text().contains("password=***"), "{}", last.text());
            // The owning workspace is drawn masked too, and is still the
            // identity requests use.
            let row = &detail.branches[0];
            assert_eq!(
                crate::views::branch_label(row),
                "password=***",
                "the workspace is preferred over the branch name, masked"
            );
            assert_eq!(row.workspace.as_ref().map(RawName::as_str), Some(name));
            // Every dialog text built from the workspace's name.
            let bound = crate::services::BoundWorkspace {
                workspace: owner.workspace.clone(),
                expect: devflow_core::setup::BranchExpect {
                    instance: Uuid::new_v4(),
                    branch: row.id,
                    source: None,
                },
            };
            let mut dialogs = crate::views::service_dialogs::snapshot_choices(&[bound]);
            dialogs.extend(
                crate::views::service_dialogs::reset_options(
                    Some(detail),
                    std::slice::from_ref(&owner.workspace),
                    "other",
                )
                .into_iter()
                .map(|o| o.label),
            );
            dialogs.extend(crate::views::service_dialogs::reset_warning("cache", name));
            let dialogs = dialogs.join("\n");
            assert!(!dialogs.contains("hunter2"), "{dialogs}");
            assert!(dialogs.contains("password=***"), "{dialogs}");
            // The identities are untouched.
            assert_eq!(last.target_id, target);
            assert_eq!(detail.snapshots[0].name.as_str(), name);
            assert_eq!(detail.snapshots[0].id, snapshot_id);
        }
    }

    // ---- sqlite services -------------------------------------------------------------

    fn sqlite_database(
        workspace: &str,
        is_default: bool,
        file: sqlite::FileStatus,
        binding: Option<(&str, bool, Option<CloneMethod>)>,
    ) -> SqliteDatabase {
        SqliteDatabase {
            id: binding.map(|_| Uuid::new_v4()),
            workspace: workspace.into(),
            workspace_path: PathBuf::from(format!("/tmp/shop.worktrees/{workspace}")),
            path: PathBuf::from(format!("/tmp/shop.worktrees/{workspace}/db/app.sqlite3")),
            is_default,
            file,
            source: binding.map(|(source, _, _)| source.to_string()),
            owned: binding.is_some_and(|(_, owned, _)| owned),
            cloned: binding.and_then(|(_, _, cloned)| cloned),
        }
    }

    fn present(bytes: u64, mode: Option<&str>) -> sqlite::FileStatus {
        sqlite::FileStatus {
            exists: true,
            bytes: Some(bytes),
            journal_mode: mode.map(str::to_string),
            sidecars: Vec::new(),
        }
    }

    fn absent() -> sqlite::FileStatus {
        sqlite::FileStatus {
            exists: false,
            bytes: None,
            journal_mode: None,
            sidecars: Vec::new(),
        }
    }

    #[test]
    fn a_database_row_says_where_its_workspace_is_and_what_its_file_is_in_words() {
        let made = sqlite_db(&sqlite_database(
            "feature-x",
            false,
            present(2 * 1024 * 1024, Some("wal")),
            Some(("workspace:main", true, Some(CloneMethod::Reflink))),
        ));
        // The checkout is the identity of the workspace, whatever it is called.
        assert_eq!(
            made.checkout,
            PathBuf::from("/tmp/shop.worktrees/feature-x")
        );
        assert_eq!(made.size_text(), "2.0 MB");
        assert_eq!(made.mode_text(), "WAL");
        assert_eq!(made.source_text(), "workspace main");
        assert_eq!(made.clone_text(), "cloned by reflink");
        assert_eq!(made.owner(), FileOwner::Workspace);
        assert_eq!(made.owner().label(), "created by this workspace");
        assert!(made.owner().refusal().is_none());

        // The default workspace's own file: left alone, never cloned.
        let project = sqlite_db(&sqlite_database(
            "main",
            true,
            present(100, Some("rollback")),
            Some(("existing", false, None)),
        ));
        assert_eq!(project.mode_text(), "rollback journal");
        assert_eq!(project.source_text(), "the file that was already there");
        assert_eq!(project.clone_text(), "nothing cloned");
        assert_eq!(project.owner(), FileOwner::Project);
        assert!(project.owner().refusal().unwrap().contains("project's own"));

        // A file the workspace brought with it.
        let brought = sqlite_db(&sqlite_database(
            "legacy",
            false,
            present(100, None),
            Some(("existing", false, None)),
        ));
        assert_eq!(brought.mode_text(), "not a database yet");
        assert_eq!(brought.owner(), FileOwner::AlreadyThere);
        assert_eq!(brought.owner().label(), "was already in the workspace");
        assert!(brought.owner().refusal().unwrap().contains("did not make"));

        // A workspace nothing is bound for yet, and a file that is not there.
        let bare = sqlite_db(&sqlite_database("new", false, absent(), None));
        assert_eq!(bare.size_text(), "not created yet");
        assert_eq!(bare.mode_text(), "—");
        assert_eq!(bare.source_text(), "not set up");
        assert_eq!(bare.clone_text(), "—");
        assert_eq!(bare.owner(), FileOwner::NotSetUp);
        assert!(
            bare.owner()
                .refusal()
                .unwrap()
                .contains("set the workspace up")
        );

        // A snapshot or a workspace as a source reads as words.
        assert_eq!(sqlite_source_text("snapshot:seed@v2"), "snapshot seed@v2");
        assert_eq!(sqlite_source_text("empty"), "nothing (an empty start)");
        assert_eq!(sqlite_clone_text(Some("copy")), "cloned by copy");
    }

    fn sqlite_fixture() -> SqliteService {
        let rows = vec![
            sqlite_db(&sqlite_database(
                "main",
                true,
                present(4096, Some("wal")),
                Some(("existing", false, None)),
            )),
            sqlite_db(&sqlite_database("fresh", false, absent(), None)),
        ];
        SqliteService {
            key: "app".into(),
            path: "db/app.sqlite3".into(),
            library: "3.46.0".into(),
            require_cow: false,
            databases: Ok(rows),
            snapshots: Ok(SqliteSnapshots::default()),
        }
    }

    #[test]
    fn counts_know_what_they_could_not_read() {
        let mut service = sqlite_fixture();
        // Files that exist, not workspaces listed.
        assert_eq!(service.database_count(), Some(1));
        assert_eq!(service.snapshot_count(), Some(0));
        assert_eq!(
            service
                .database_at(Path::new("/tmp/shop.worktrees/main"))
                .map(|d| d.is_default),
            Some(true)
        );
        // A catalog that cannot be read is unknown, not none.
        service.snapshots = Err("catalog.json: invalid".into());
        assert_eq!(service.snapshot_count(), None);
        assert_eq!((service.unfinished(), service.damaged()), (0, 0));
        service.databases = Err("cannot list the worktrees".into());
        assert_eq!(service.database_count(), None);
        assert!(
            service
                .database_at(Path::new("/tmp/shop.worktrees/main"))
                .is_none()
        );
    }

    #[test]
    fn a_snapshot_is_labelled_masked_and_acted_on_by_its_recorded_name() {
        // kingfisher:ignore
        let name = "password=hunter2";
        let row = SqliteSnapshotRow {
            id: Uuid::new_v4(),
            name: name.into(),
            version: 3,
            bytes: Some(8192),
            how: Some("backup".into()),
            source: "workspace:main".into(),
            created: "2026-10-24 10:00".into(),
            damaged: false,
        };
        assert_eq!(row.label(), "password=***@v3");
        assert_eq!(row.name.as_str(), name, "the identity is untouched");
        assert!(!format!("{row:?}").contains("hunter2"));
        assert_eq!(row.how_text(), "taken by backup (a new file)");
        assert_eq!(row.size_text(), "8.0 kB");
        let reflinked = SqliteSnapshotRow {
            how: Some("reflink".into()),
            ..row.clone()
        };
        assert_eq!(reflinked.how_text(), "taken by reflink (blocks shared)");
    }

    #[test]
    fn interrupted_snapshot_operations_are_reported_one_sentence_each() {
        let lines = sqlite_reconciled_lines(&sqlite::Reconciled {
            abandoned: vec!["seed@v2".into()],
            removed: vec!["old@v1".into()],
            damaged: vec!["lost@v1".into()],
        });
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("seed@v2") && lines[0].contains("abandoned"));
        assert!(lines[1].contains("old@v1") && lines[1].contains("finished removing"));
        assert!(lines[2].contains("lost@v1") && lines[2].contains("data is lost"));
        assert!(sqlite_reconciled_lines(&sqlite::Reconciled::default()).is_empty());
    }

    #[test]
    fn what_a_sqlite_model_says_about_a_credential_like_name_is_masked_once() {
        // kingfisher:ignore
        let odd = "password=hunter2";
        let mut service = sqlite_fixture();
        if let Ok(rows) = &mut service.databases {
            rows[0].source = Some(format!("workspace:{odd}"));
        }
        service.snapshots = Ok(SqliteSnapshots {
            ready: vec![SqliteSnapshotRow {
                id: Uuid::new_v4(),
                name: odd.into(),
                version: 1,
                bytes: None,
                how: None,
                source: format!("workspace:{odd}"),
                created: String::new(),
                damaged: false,
            }],
            unfinished: 0,
            damaged: 0,
        });
        let mut other = sqlite_fixture();
        other.key = "other".into();
        other.databases = Err(format!("cannot read {odd}"));
        other.snapshots = Err(format!("cannot read {odd}"));
        let mut model = ServicesModel {
            sqlite: vec![service, other],
            ..ServicesModel::default()
        };
        redaction::services(&mut model);
        let text = format!("{model:?}");
        assert!(!text.contains("hunter2"), "{text}");
        let Ok(rows) = &model.sqlite[0].databases else {
            panic!("rows");
        };
        assert_eq!(rows[0].source.as_deref(), Some("workspace:password=***"));
        // The snapshot's identity stays what core recorded.
        let Ok(snapshots) = &model.sqlite[0].snapshots else {
            panic!("snapshots");
        };
        assert_eq!(snapshots.ready[0].name.as_str(), odd);
        assert_eq!(snapshots.ready[0].label(), "password=***@v1");
        // A failure is kept as one, masked.
        assert!(
            model.sqlite[1]
                .databases
                .as_ref()
                .unwrap_err()
                .contains("password=***")
        );
        assert!(model.sqlite[1].snapshots.is_err());
    }

    #[test]
    fn an_engine_core_adds_later_is_just_a_word() {
        // The detail carries `Engine::as_str`; nothing here matches engines.
        let mut status = status(Vec::new());
        status.engine = Engine::Redis;
        assert_eq!(detail_of(&status, &[], &[]).engine, "redis");
    }
}

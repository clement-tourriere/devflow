//! `devflow gc`: workspaces that Git forgot, and what they still own.
//!
//! [`inventory`] reads every record of the ledger ([`crate::ledger`]) and every
//! registered worktree, classifies each and shows what it owns and what is
//! measurable about it. [`remove`] removes the instances it is asked to, after
//! reading each again. Nothing here is automatic: the inventory removes
//! nothing, and a removal acts on the instances named, with the generation of
//! the record the person was shown.
//!
//! Classification fails closed. A workspace is
//!
//! - [`Class::Active`]: registered, its directory present, its receipt the one
//!   the record belongs to;
//! - [`Class::Moved`]: registered at another place than the record says, with
//!   the same Git administrative id and instance (nothing to clean up: the
//!   record is refreshed by the next write of the receipt);
//! - [`Class::Missing`]: registered, directory gone (what `devflow prune`
//!   sees);
//! - [`Class::RemovedElsewhere`]: not registered and its directory gone
//!   (`git worktree remove`, or an agent host that deleted it); the record
//!   still lists the resources;
//! - [`Class::Unknown`]: anything not positively identified: a record or
//!   receipt that cannot be read, a server or Docker daemon that cannot be
//!   reached for a branch the record names, a missing parent directory (a
//!   disconnected drive), a record written for another checkout, a directory
//!   Git does not know. Never removable.
//!
//! Only `Missing` and `RemovedElsewhere` are removable. Never removed:
//! attachments (`setup --database-url`: not owned, so never recorded), shared
//! servers, snapshots, branches another instance owns, anything unknown, an
//! active workspace. "Looks squash-merged" ([`crate::worktree::MergeEvidence`])
//! is a hint shown beside an entry and decides nothing.
use crate::config::Engine;
use crate::db::Server;
use crate::error::{PartialRemoval, Protected};
use crate::ledger::{self, Effect, Record};
use crate::pitchfork::Pitchfork;
use crate::processes::{self, MayStillRun};
use crate::project::Project;
use crate::receipt::Receipt;
use crate::remedy::{Remedy, serialize_remedies};
use crate::services::catalog::{RecordState, Store};
use crate::services::{Service, docker};
use crate::setup::{self, ExpectedWorkspace, Gate, NoProgress, RemoveOptions, removal};
use crate::worktree::{self, MergeEvidence, Workspace};
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

/// How long a single query of Docker for the inventory may take.
const DOCKER_QUERY: Duration = Duration::from_secs(5);

/// Where a workspace stands, as far as the files and Git say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    RemovedElsewhere,
    Missing,
    Unknown,
    Moved,
    Active,
}

impl Class {
    pub fn label(self) -> &'static str {
        match self {
            Class::Active => "active",
            Class::Moved => "moved",
            Class::Missing => "missing",
            Class::RemovedElsewhere => "removed elsewhere",
            Class::Unknown => "unknown",
        }
    }

    /// Whether a removal may act on it (when nothing else stands in the way).
    pub fn removable(self) -> bool {
        matches!(self, Class::Missing | Class::RemovedElsewhere)
    }
}

/// Why an entry is `unknown`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum Cause {
    /// The record's file cannot be read.
    RecordUnreadable { file: PathBuf },
    /// The worktree's receipt cannot be read.
    ReceiptUnreadable,
    /// The worktree is still registered but its receipt is gone.
    ReceiptMissing,
    /// The record was written for another repository that is still there (a
    /// copy of this one).
    OtherCheckout { common_dir: PathBuf },
    /// The record belongs to another project.
    OtherProject,
    /// The folder the worktree was in is missing or unreadable too: a drive
    /// that is not connected looks like a deleted folder.
    ParentMissing { parent: PathBuf },
    /// A directory is at the recorded place, and Git has no registration for it.
    Unregistered { path: PathBuf },
    /// The registration is locked: Git's mark for a worktree on a portable
    /// device that may be unplugged.
    Locked,
    /// The instance is live in another worktree (a copied or renamed one): the
    /// record is not about a workspace that is gone.
    AnotherWorktree { path: PathBuf },
    /// A database branch the record names cannot be asked about.
    ServerUnreachable,
    /// A container branch the record names cannot be asked about.
    DockerUnreachable,
}

/// Whether a resource the record names is there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Presence {
    /// There, and this instance's.
    Owned,
    /// There, and another instance's: left alone.
    Foreign,
    /// Not there: nothing to drop.
    Absent,
    /// The backend could not be asked.
    Unreachable { reason: String },
    /// Not asked (an active workspace's, or a local summary).
    NotChecked,
}

/// A data branch an entry owns, with what is measurable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Branch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<Engine>,
    pub id: Uuid,
    /// Recorded before its creation: it may never have been made.
    pub pending: bool,
    #[serde(flatten)]
    pub presence: Presence,
    /// A database's size as PostgreSQL reports it (shared blocks counted in
    /// full).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<i64>,
    /// A container's state as Docker reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// The backend has the branch ready (not being created, not failed): only
    /// a ready branch can be snapshotted. `None` when the backend was not
    /// asked or has no such branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready: Option<bool>,
    /// The backend's generation of the branch (a reset bumps it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,
    /// A container branch's volume.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
}

impl Branch {
    pub fn describe(&self) -> String {
        match &self.service {
            Some(key) => format!("branch {} of service '{key}'", crate::ids::short(self.id)),
            None => format!(
                "database branch {} from an earlier build",
                crate::ids::short(self.id)
            ),
        }
    }
}

/// One workspace of the inventory.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    /// The workspace instance (`--remove` takes its first eight characters or
    /// more); `None` for something whose record or receipt cannot be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<Uuid>,
    /// The workspace name at creation (the folder's name when unknown).
    pub workspace: String,
    pub class: Class,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<Cause>,
    /// Why it is `unknown` or otherwise worth a word, in plain text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Where the checkout is, or was.
    pub path: PathBuf,
    /// For a moved workspace, where the record has it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub has_record: bool,
    /// A removal began and did not finish.
    pub removing: bool,
    pub created_at: Option<DateTime<Utc>>,
    pub last_active_at: Option<DateTime<Utc>>,
    pub branches: Vec<Branch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// The daemon ids devflow started and has not seen stop.
    pub daemons: Vec<String>,
    /// The workspace's app port, given back to the machine's port ledger when
    /// it is removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_port: Option<u16>,
    /// A removal could not show that nothing runs under the namespace.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub daemons_unverified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge: Option<MergeEvidence>,
    /// What a removal is asked for: it is refused when the record changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    pub removable: bool,
    /// Why not, when it is not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_removable: Option<String>,
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "serialize_remedies"
    )]
    pub remedies: Vec<Remedy>,
}

impl Entry {
    /// The id listings show.
    pub fn short_id(&self) -> Option<String> {
        self.instance.map(crate::ids::short)
    }

    /// Whether the workspace owns anything a removal would have to account for.
    pub fn holds_resources(&self) -> bool {
        !self.branches.is_empty() || !self.daemons.is_empty() || self.daemons_unverified
    }

    /// The target a removal is asked for: the record's generation and what was
    /// seen of each branch in its backend, so that the removal drops the data
    /// that was shown and nothing a reset replaced it with since.
    pub fn target(&self) -> Option<Target> {
        Some(Target {
            instance: self.instance?,
            generation: self.generation.clone()?,
            backend: self
                .branches
                .iter()
                .filter(|b| b.presence == Presence::Owned)
                .filter(|b| b.generation.is_some() || b.volume.is_some())
                .map(|b| ledger::BranchExpect {
                    id: b.id,
                    generation: b.generation,
                    volume: b.volume.clone(),
                })
                .collect(),
        })
    }
}

/// The inventory of a project.
#[derive(Debug, Clone, Serialize)]
pub struct Inventory {
    pub project: String,
    /// The branch the merge evidence is judged against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    pub entries: Vec<Entry>,
    /// Things done or skipped on the way (records written for workspaces set
    /// up before the ledger existed).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl Inventory {
    pub fn entry(&self, instance: Uuid) -> Option<&Entry> {
        self.entries.iter().find(|e| e.instance == Some(instance))
    }

    /// The entries a removal may act on.
    pub fn removable(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.removable)
    }

    /// The instance a typed id (a UUID, or at least eight hex characters)
    /// names; ambiguous or unknown ids are errors that list the candidates.
    pub fn resolve(&self, id: &crate::ids::IdRef) -> Result<Uuid> {
        id.resolve("workspace", self.entries.iter().filter_map(|e| e.instance))
    }
}

/// What to look at beyond the files.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Ask the backends for sizes, container states and memory; without it
    /// only what is needed to tell whether a resource is there.
    pub measure: bool,
    /// Compute the merge evidence of each entry.
    pub evidence: bool,
    /// Write the records a workspace set up before the ledger lacks, and
    /// refresh the checkout of a record whose repository moved.
    pub backfill: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            measure: true,
            evidence: true,
            backfill: true,
        }
    }
}

// ---------------------------------------------------------------------------
// What the files say

/// A registered worktree, as the classification sees it.
struct Registered {
    admin_id: Option<String>,
    workspace: Workspace,
    receipt: ReceiptState,
}

enum ReceiptState {
    Instance(Uuid),
    /// No receipt file.
    Absent,
    Unreadable(String),
}

fn admin_id_of(workspace: &Workspace) -> Option<String> {
    if workspace.is_default {
        return None;
    }
    workspace
        .git_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

fn registered(project: &Project) -> Result<Vec<Registered>> {
    Ok(worktree::list(project)?
        .into_iter()
        .map(|workspace| {
            let receipt = if workspace.git_dir.is_dir() {
                match Receipt::load(&workspace.git_dir) {
                    Ok(Some(receipt)) if receipt.project_id == project.id() => {
                        ReceiptState::Instance(receipt.instance_id)
                    }
                    Ok(Some(_)) => ReceiptState::Unreadable("it belongs to another project".into()),
                    Ok(None) => ReceiptState::Absent,
                    Err(e) => ReceiptState::Unreadable(format!("{e:#}")),
                }
            } else {
                ReceiptState::Absent
            };
            Registered {
                admin_id: admin_id_of(&workspace),
                workspace,
                receipt,
            }
        })
        .collect())
}

/// The verdict on one record.
struct Verdict {
    class: Class,
    cause: Option<Cause>,
    reason: Option<String>,
    /// Where the registered worktree is now (moved, missing).
    now_at: Option<PathBuf>,
    /// The repository moved: the record's checkout identity is stale.
    relocated: bool,
}

impl Verdict {
    fn of(class: Class) -> Self {
        Self {
            class,
            cause: None,
            reason: None,
            now_at: None,
            relocated: false,
        }
    }

    fn because(class: Class, reason: impl Into<String>) -> Self {
        Self {
            reason: Some(reason.into()),
            ..Self::of(class)
        }
    }

    fn unknown(cause: Cause, reason: impl Into<String>) -> Self {
        Self {
            cause: Some(cause),
            ..Self::because(Class::Unknown, reason)
        }
    }
}

/// The folder a worktree was in must be there and readable; otherwise it is
/// likely a drive that is not connected.
fn parent_problem(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    std::fs::read_dir(parent)
        .err()
        .map(|_| parent.to_path_buf())
}

fn classify(project: &Project, record: &Record, registered: &[Registered]) -> Verdict {
    if record.project_id != project.id() {
        return Verdict::unknown(Cause::OtherProject, "the record belongs to another project");
    }
    let mut relocated = false;
    if record.checkout.common_dir != project.git_common_dir {
        // A record is acted on by the repository it was written for. Another
        // path is the same repository only on positive evidence: the directory
        // the record was written in is this very directory, moved. A copy of
        // the repository (another machine, a restored backup, a VM clone) has
        // other directories, and its records, which name branches on servers it
        // may share with the original, must not acquire the right to delete them
        // because the original's path happens to be missing or unreadable here.
        let moved = !record.checkout.common_dir.exists()
            && match (
                record.checkout.identity.as_deref(),
                ledger::repository_identity(&project.git_common_dir),
            ) {
                (Some(was), Some(now)) => was == now,
                _ => false,
            };
        if !moved {
            let where_ = record.checkout.common_dir.display();
            return Verdict::unknown(
                Cause::OtherCheckout {
                    common_dir: record.checkout.common_dir.clone(),
                },
                if record.checkout.common_dir.exists() {
                    format!(
                        "the record was written for the repository at {where_}, which is still \
                         there; this one is a copy"
                    )
                } else {
                    format!(
                        "the record was written for the repository at {where_}, and nothing \
                         shows that this is that repository moved rather than a copy of it; \
                         setting a workspace of this checkout up again refreshes its records"
                    )
                },
            );
        }
        relocated = true;
    }
    let mut verdict = classify_placed(record, registered);
    verdict.relocated = relocated;
    verdict
}

fn classify_placed(record: &Record, registered: &[Registered]) -> Verdict {
    // An instance whose receipt is in a registered worktree is not gone, however
    // the record places it: it lives in that worktree.
    if let Some(live) = registered.iter().find(|w| {
        w.admin_id != record.admin_id
            && matches!(&w.receipt, ReceiptState::Instance(id) if *id == record.instance_id)
    }) {
        return Verdict::unknown(
            Cause::AnotherWorktree {
                path: live.workspace.path.clone(),
            },
            format!(
                "this workspace instance is live in the worktree at {}",
                live.workspace.path.display()
            ),
        );
    }
    let Some(found) = registered.iter().find(|w| w.admin_id == record.admin_id) else {
        // Git has no registration for it.
        if let Some(parent) = parent_problem(&record.worktree) {
            return Verdict::unknown(
                Cause::ParentMissing {
                    parent: parent.clone(),
                },
                format!(
                    "{} is missing or unreadable too, so the drive it is on may just be \
                     disconnected; if the folder was deleted on purpose, create it again \
                     (empty) and look again",
                    parent.display()
                ),
            );
        }
        if record.worktree.symlink_metadata().is_ok() {
            if replaced(record) {
                return Verdict::because(
                    Class::RemovedElsewhere,
                    "another directory is at its place now",
                );
            }
            return Verdict::unknown(
                Cause::Unregistered {
                    path: record.worktree.clone(),
                },
                format!(
                    "a directory is still at {}, and Git has no registration for it",
                    record.worktree.display()
                ),
            );
        }
        return Verdict::of(Class::RemovedElsewhere);
    };
    let workspace = &found.workspace;
    match &found.receipt {
        ReceiptState::Unreadable(why) => Verdict::unknown(
            Cause::ReceiptUnreadable,
            format!("its receipt cannot be read ({why})"),
        ),
        ReceiptState::Instance(id) if *id == record.instance_id => {
            if workspace.locked {
                return Verdict::unknown(
                    Cause::Locked,
                    "its registration is locked, which is how Git marks a worktree on a \
                     portable device that may be unplugged",
                );
            }
            if workspace.missing {
                if let Some(parent) = parent_problem(&workspace.path) {
                    return Verdict::unknown(
                        Cause::ParentMissing {
                            parent: parent.clone(),
                        },
                        format!(
                            "{} is missing or unreadable too, so the drive it is on may just \
                             be disconnected; if the folder was deleted on purpose, create it \
                             again (empty) and look again",
                            parent.display()
                        ),
                    );
                }
                return Verdict {
                    now_at: Some(workspace.path.clone()),
                    ..Verdict::of(Class::Missing)
                };
            }
            if workspace.path != record.worktree {
                return Verdict {
                    now_at: Some(workspace.path.clone()),
                    ..Verdict::because(
                        Class::Moved,
                        format!("the checkout moved to {}", workspace.path.display()),
                    )
                };
            }
            Verdict::of(Class::Active)
        }
        ReceiptState::Instance(_) => Verdict::because(
            Class::RemovedElsewhere,
            "the worktree now belongs to another workspace instance",
        ),
        ReceiptState::Absent => {
            if !workspace.missing && workspace.path == record.worktree && replaced(record) {
                return Verdict::because(
                    Class::RemovedElsewhere,
                    "another directory took its place and has no receipt",
                );
            }
            Verdict::unknown(
                Cause::ReceiptMissing,
                "the worktree is still registered but its receipt is gone",
            )
        }
    }
}

/// Whether the directory at the recorded place is not the one the record saw.
fn replaced(record: &Record) -> bool {
    match (
        record.fingerprint.as_deref(),
        worktree::fingerprint(&record.worktree).as_deref(),
    ) {
        (Some(seen), Some(now)) => seen != now,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Summary

/// What `list` and `doctor` say, from the files alone (no server, no Docker).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// Workspaces whose worktree Git forgot and that still own resources.
    pub removed_elsewhere: Vec<String>,
    /// Registered workspaces whose directory is gone and that still own
    /// resources.
    pub missing: Vec<String>,
    /// Records that cannot be read.
    pub unreadable: usize,
}

impl Summary {
    pub fn is_empty(&self) -> bool {
        self.removed_elsewhere.is_empty() && self.missing.is_empty() && self.unreadable == 0
    }

    pub fn count(&self) -> usize {
        self.removed_elsewhere.len() + self.missing.len()
    }

    /// One line: `2 removed workspaces still hold data`.
    pub fn sentence(&self) -> Option<String> {
        let count = self.count();
        if count == 0 {
            return None;
        }
        let gone = match (self.removed_elsewhere.len(), self.missing.len()) {
            (n, 0) => format!("{n} workspace{} removed outside devflow", plural(n)),
            (0, n) => format!("{n} workspace{} with a missing directory", plural(n)),
            (a, b) => format!(
                "{a} workspace{} removed outside devflow and {b} with a missing directory",
                plural(a)
            ),
        };
        Some(format!("{gone} still own data or processes"))
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The workspaces gone from Git that still own resources, from local files
/// alone: never a server, Docker or pitchfork, and nothing is written.
pub fn summary(project: &Project) -> Result<Summary> {
    let listing = ledger::list(&project.git_common_dir)?;
    let registered = registered(project)?;
    let mut out = Summary {
        unreadable: listing.unreadable.len(),
        ..Summary::default()
    };
    for record in listing.records.iter().filter(|r| r.holds_resources()) {
        let verdict = classify(project, record, &registered);
        match verdict.class {
            Class::RemovedElsewhere => out.removed_elsewhere.push(record.workspace.clone()),
            Class::Missing => out.missing.push(record.workspace.clone()),
            _ => {}
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Backends

/// What is asked of the backends, once each.
struct Backends<'a> {
    project: &'a Project,
    measure: bool,
    pg: Option<std::result::Result<Option<Server>, String>>,
    docker: Option<std::result::Result<String, String>>,
    memory: Option<BTreeMap<String, u64>>,
}

impl<'a> Backends<'a> {
    fn new(project: &'a Project, measure: bool) -> Self {
        Self {
            project,
            measure,
            pg: None,
            docker: None,
            memory: None,
        }
    }

    fn pg(&mut self) -> &std::result::Result<Option<Server>, String> {
        let project = self.project;
        self.pg
            .get_or_insert_with(|| Server::for_project(project).map_err(|e| format!("{e:#}")))
    }

    fn docker(&mut self) -> &std::result::Result<String, String> {
        self.docker
            .get_or_insert_with(|| docker::daemon_id().map_err(|e| format!("{e:#}")))
    }

    fn memory_of(&mut self, container: &str) -> Option<u64> {
        if !self.measure {
            return None;
        }
        let memory = self
            .memory
            .get_or_insert_with(|| docker::memory_in_use().unwrap_or_default());
        let short: String = container.chars().take(12).collect();
        memory.get(&short).copied()
    }

    /// The project's PostgreSQL server, asked about branch `id`.
    fn in_postgres(&mut self, id: Uuid, instance: Uuid) -> Lookup {
        let measure = self.measure;
        match self.pg() {
            Ok(Some(server)) => match server.branch(id) {
                Ok(Some(info)) => Lookup::Found(Seen {
                    generation: u32::try_from(info.generation).ok(),
                    size_bytes: if measure { info.size_bytes } else { None },
                    ..Seen::new(
                        Engine::Postgres,
                        info.instance_id == Some(instance),
                        info.status == crate::db::RecordStatus::Ready,
                    )
                }),
                Ok(None) => Lookup::Absent,
                Err(e) => Lookup::Unknown(format!("{e:#}")),
            },
            // No server to ask is not "not there".
            Ok(None) => Lookup::Unknown(
                "the project has no postgres service any more, so no server can be asked"
                    .to_string(),
            ),
            Err(why) => Lookup::Unknown(why.clone()),
        }
    }

    /// The container services' catalog, asked about branch `id` of service
    /// `key`: of `engine`, or, when the engine was not recorded, of the one the
    /// catalog knows the key under.
    fn in_catalog(
        &mut self,
        key: &str,
        engine: Option<Engine>,
        id: Uuid,
        instance: Uuid,
    ) -> Lookup {
        let daemon = match self.docker() {
            Ok(daemon) => daemon.clone(),
            Err(reason) => return Lookup::Unknown(reason.clone()),
        };
        let engine = match engine {
            Some(engine) => engine,
            None => match Store::open(self.project.id(), &daemon).and_then(|s| s.load()) {
                Ok(catalog) => match catalog.services.get(key) {
                    Some(entry) => entry.engine,
                    None => return Lookup::Absent,
                },
                Err(e) => return Lookup::Unknown(format!("{e:#}")),
            },
        };
        let record =
            match Service::recorded(self.project, key, engine).and_then(|svc| svc.branch(id)) {
                Ok(Some(record)) => record,
                Ok(None) => return Lookup::Absent,
                Err(e) => return Lookup::Unknown(format!("{e:#}")),
            };
        let mut seen = Seen {
            generation: Some(record.generation),
            volume: Some(record.volume.clone()),
            ..Seen::new(
                engine,
                record.owner == Some(instance),
                record.state == RecordState::Ready,
            )
        };
        if self.measure {
            let reference = record
                .container_id
                .clone()
                .unwrap_or_else(|| record.container_name.clone());
            seen.container = match docker::inspect_container_within(&reference, DOCKER_QUERY) {
                Ok(Some(info)) => {
                    seen.memory_bytes = info.running.then(|| self.memory_of(&info.id)).flatten();
                    Some(info.status)
                }
                Ok(None) => Some("missing".to_string()),
                Err(_) => None,
            };
        }
        Lookup::Found(seen)
    }

    /// Ask for one branch of `instance`, where its record says it is and in the
    /// way removal looks for it: the recorded backend, or, when none was
    /// recorded, both (found in either decides; absent from both, with both
    /// asked, means never made; anything else is unknown). Today's
    /// configuration never says where a recorded branch is.
    fn probe(&mut self, branch: &ledger::BranchRef, instance: Uuid) -> Branch {
        let mut out = Branch {
            service: branch.service.clone(),
            engine: branch.lookup_engine(),
            id: branch.id,
            pending: branch.pending,
            presence: Presence::NotChecked,
            size_bytes: None,
            container: None,
            memory_bytes: None,
            ready: None,
            generation: None,
            volume: None,
        };
        let looked = match (branch.lookup_engine(), branch.service.as_deref()) {
            (Some(Engine::Postgres), _) | (_, None) => self.in_postgres(branch.id, instance),
            (Some(engine), Some(key)) => self.in_catalog(key, Some(engine), branch.id, instance),
            (None, Some(key)) => match self.in_postgres(branch.id, instance) {
                found @ Lookup::Found(_) => found,
                in_postgres => match (in_postgres, self.in_catalog(key, None, branch.id, instance))
                {
                    (_, found @ Lookup::Found(_)) => found,
                    (Lookup::Absent, Lookup::Absent) => Lookup::Absent,
                    (Lookup::Unknown(why), _) | (_, Lookup::Unknown(why)) => Lookup::Unknown(why),
                    (Lookup::Found(_), _) => unreachable!("found in PostgreSQL is returned above"),
                },
            },
        };
        match looked {
            Lookup::Found(seen) => {
                out.engine = Some(seen.engine);
                out.presence = if seen.owned {
                    Presence::Owned
                } else {
                    Presence::Foreign
                };
                out.ready = Some(seen.ready);
                out.generation = seen.generation;
                out.volume = seen.volume;
                out.size_bytes = seen.size_bytes;
                out.container = seen.container;
                out.memory_bytes = seen.memory_bytes;
            }
            Lookup::Absent => out.presence = Presence::Absent,
            Lookup::Unknown(reason) => out.presence = Presence::Unreachable { reason },
        }
        out
    }
}

impl Seen {
    fn new(engine: Engine, owned: bool, ready: bool) -> Seen {
        Seen {
            engine,
            owned,
            ready,
            generation: None,
            volume: None,
            size_bytes: None,
            container: None,
            memory_bytes: None,
        }
    }
}

/// What a backend says about a branch.
enum Lookup {
    Found(Seen),
    /// The backend could be asked, and does not have it.
    Absent,
    /// It could not be asked, or not answered.
    Unknown(String),
}

/// A branch a backend has.
struct Seen {
    engine: Engine,
    owned: bool,
    ready: bool,
    generation: Option<u32>,
    volume: Option<String>,
    size_bytes: Option<i64>,
    container: Option<String>,
    memory_bytes: Option<u64>,
}

// ---------------------------------------------------------------------------
// The inventory

/// Classify every record and every registered worktree of `project`.
///
/// With [`Options::backfill`] it also writes the records that workspaces set up
/// before the ledger lack (from their receipts, under the receipt lock, with no
/// provisioning) and refreshes the checkout of records that moved with their
/// repository; nothing else is written.
pub fn inventory(project: &Project, options: &Options) -> Result<Inventory> {
    let common = &project.git_common_dir;
    let mut notes = Vec::new();
    let mut registered = registered(project)?;
    let mut listing = ledger::list(common)?;
    if options.backfill && backfill(project, &registered, &listing, &mut notes) {
        listing = ledger::list(common)?;
        registered = self::registered(project)?;
    }

    let mut backends = Backends::new(project, options.measure);
    let mut entries: Vec<Entry> = Vec::new();
    let mut with_record: Vec<Uuid> = Vec::new();

    for record in &listing.records {
        with_record.push(record.instance_id);
        let verdict = classify(project, record, &registered);
        if options.backfill && verdict.relocated {
            let worktree = verdict.now_at.clone();
            let moved = ledger::Checkout {
                main: project.root.clone(),
                common_dir: common.clone(),
                identity: ledger::repository_identity(common),
            };
            if let Err(e) =
                ledger::Handle::new(common, record.instance_id).relocate(moved, worktree)
            {
                notes.push(format!(
                    "could not refresh the record of '{}': {e:#}",
                    record.workspace
                ));
            }
        }
        entries.push(entry_of(project, record, verdict, options, &mut backends));
    }
    for unreadable in &listing.unreadable {
        entries.push(Entry {
            instance: unreadable.instance,
            workspace: unreadable
                .instance
                .map(crate::ids::short)
                .unwrap_or_else(|| "(unnamed record)".to_string()),
            class: Class::Unknown,
            cause: Some(Cause::RecordUnreadable {
                file: unreadable.file.clone(),
            }),
            reason: Some(format!("its record cannot be read ({})", unreadable.reason)),
            path: unreadable.file.clone(),
            recorded_path: None,
            branch: None,
            has_record: true,
            removing: false,
            created_at: None,
            last_active_at: None,
            branches: Vec::new(),
            namespace: None,
            daemons: Vec::new(),
            app_port: None,
            daemons_unverified: false,
            merge: None,
            generation: None,
            removable: false,
            not_removable: Some("its record cannot be read, so what it owns is unknown".into()),
            remedies: vec![Remedy::Reveal {
                path: unreadable.file.clone(),
            }],
        });
    }
    // Registered worktrees the ledger has no record of.
    for w in &registered {
        let known = matches!(&w.receipt, ReceiptState::Instance(id) if with_record.contains(id));
        if known {
            continue;
        }
        let (class, cause, reason, instance) = match &w.receipt {
            ReceiptState::Instance(id) => (
                if w.workspace.missing {
                    Class::Missing
                } else {
                    Class::Active
                },
                None,
                Some(
                    "no record yet (it is written the next time the workspace is set up)"
                        .to_string(),
                ),
                Some(*id),
            ),
            ReceiptState::Absent => (
                Class::Active,
                None,
                Some("devflow has not set it up".to_string()),
                None,
            ),
            ReceiptState::Unreadable(why) => (
                Class::Unknown,
                Some(Cause::ReceiptUnreadable),
                Some(format!("its receipt cannot be read ({why})")),
                None,
            ),
        };
        // A registered worktree without a record owns nothing the ledger can
        // name: removing it is `remove`'s, not `gc`'s.
        let remedies = match w.receipt {
            ReceiptState::Unreadable(_) => vec![Remedy::Reveal {
                path: crate::receipt::Receipt::path(&w.workspace.git_dir),
            }],
            _ => Vec::new(),
        };
        entries.push(Entry {
            instance,
            workspace: w.workspace.name.clone(),
            class,
            cause,
            reason,
            path: w.workspace.path.clone(),
            recorded_path: None,
            branch: w.workspace.branch.clone(),
            has_record: false,
            removing: false,
            created_at: None,
            last_active_at: None,
            branches: Vec::new(),
            namespace: None,
            daemons: Vec::new(),
            app_port: None,
            daemons_unverified: false,
            merge: None,
            generation: None,
            removable: false,
            not_removable: Some(match class {
                Class::Unknown => "it cannot be identified".to_string(),
                _ => "it has no record to remove".to_string(),
            }),
            remedies,
        });
    }
    entries.sort_by(|a, b| {
        a.class
            .cmp(&b.class)
            .then_with(|| a.last_active_at.cmp(&b.last_active_at))
            .then_with(|| a.workspace.cmp(&b.workspace))
    });
    let base = entries
        .iter()
        .find_map(|e| e.merge.as_ref().and_then(|m| m.base.clone()));
    Ok(Inventory {
        project: project.name().to_string(),
        base,
        entries,
        notes,
    })
}

/// Write the records of registered workspaces that have none. Returns whether
/// anything was written.
fn backfill(
    project: &Project,
    registered: &[Registered],
    listing: &ledger::Listing,
    notes: &mut Vec<String>,
) -> bool {
    let mut wrote = false;
    for w in registered {
        let ReceiptState::Instance(id) = w.receipt else {
            continue;
        };
        if listing.records.iter().any(|r| r.instance_id == id)
            || listing.unreadable.iter().any(|u| u.instance == Some(id))
        {
            continue;
        }
        let git_dir = &w.workspace.git_dir;
        let written = (|| -> Result<bool> {
            // Under the receipt lock, like every writer of the pair.
            let Some(_lock) = Receipt::try_lock(git_dir)? else {
                return Ok(false);
            };
            let Some(receipt) = Receipt::load(git_dir)? else {
                return Ok(false);
            };
            ledger::sync(git_dir, &receipt)?;
            Ok(true)
        })();
        match written {
            Ok(true) => {
                wrote = true;
                notes.push(format!(
                    "recorded what workspace '{}' owns (it was set up before records existed)",
                    w.workspace.name
                ));
            }
            Ok(false) => notes.push(format!(
                "workspace '{}' is in use; its record is written the next time it is set up",
                w.workspace.name
            )),
            Err(e) => notes.push(format!(
                "could not write the record of workspace '{}': {e:#}",
                w.workspace.name
            )),
        }
    }
    let _ = project;
    wrote
}

fn entry_of(
    project: &Project,
    record: &Record,
    verdict: Verdict,
    options: &Options,
    backends: &mut Backends<'_>,
) -> Entry {
    let Verdict {
        mut class,
        mut cause,
        mut reason,
        now_at,
        ..
    } = verdict;
    // What the backends say. An entry that could be removed is only removable
    // when everything it owns can be asked about; the others are probed for
    // their numbers only when they are measured.
    let probe = class.removable() || options.measure;
    let branches: Vec<Branch> = record
        .resources
        .branches
        .iter()
        .map(|branch| {
            if probe {
                backends.probe(branch, record.instance_id)
            } else {
                Branch {
                    service: branch.service.clone(),
                    engine: branch.engine,
                    id: branch.id,
                    pending: branch.pending,
                    presence: Presence::NotChecked,
                    size_bytes: None,
                    container: None,
                    memory_bytes: None,
                    ready: None,
                    generation: None,
                    volume: None,
                }
            }
        })
        .collect();
    let unreachable = branches.iter().find_map(|b| match &b.presence {
        Presence::Unreachable { reason } => Some((b, reason.clone())),
        _ => None,
    });
    if class.removable()
        && let Some((branch, why)) = unreachable
    {
        class = Class::Unknown;
        cause = Some(if branch.engine == Some(Engine::Postgres) {
            Cause::ServerUnreachable
        } else {
            Cause::DockerUnreachable
        });
        reason = Some(format!(
            "{} cannot be looked at ({why}), so it cannot be told whether it is still there",
            branch.describe()
        ));
    }
    let merge = options.evidence.then(|| {
        worktree::merge_evidence(project, record.branch.as_deref(), record.head.as_deref())
    });
    let default_workspace = record.admin_id.is_none();
    let (removable, not_removable) = match class {
        _ if default_workspace => (false, Some("it is the default workspace".to_string())),
        Class::Missing | Class::RemovedElsewhere => (true, None),
        Class::Active => (
            false,
            Some("it is an active workspace, which a clean-up never removes (remove it the ordinary way)".into()),
        ),
        Class::Moved => (
            false,
            Some("its checkout is there, at another place".into()),
        ),
        Class::Unknown => (
            false,
            Some(
                reason
                    .clone()
                    .unwrap_or_else(|| "it cannot be identified".into()),
            ),
        ),
    };
    let remedies = remedies_of(project, record, class, cause.as_ref());
    Entry {
        instance: Some(record.instance_id),
        workspace: record.workspace.clone(),
        class,
        cause,
        reason,
        path: now_at.clone().unwrap_or_else(|| record.worktree.clone()),
        recorded_path: now_at.map(|_| record.worktree.clone()),
        branch: record.branch.clone(),
        has_record: true,
        removing: record.state == ledger::State::Removing,
        created_at: Some(record.created_at),
        last_active_at: record.last_active_at,
        branches,
        namespace: record.resources.namespace.clone(),
        daemons: record.resources.daemons.clone(),
        app_port: record.app_port,
        daemons_unverified: record.resources.namespace_unverified,
        merge,
        generation: Some(record.generation()),
        removable,
        not_removable,
        remedies,
    }
}

fn remedies_of(
    project: &Project,
    record: &Record,
    class: Class,
    cause: Option<&Cause>,
) -> Vec<Remedy> {
    let root = project.root.clone();
    match cause {
        Some(Cause::ServerUnreachable) => vec![Remedy::StartDatabase { root }, Remedy::Retry],
        Some(Cause::DockerUnreachable) => vec![Remedy::Retry],
        Some(Cause::ParentMissing { parent }) => vec![
            Remedy::Reveal {
                path: parent.clone(),
            },
            Remedy::Retry,
        ],
        Some(Cause::ReceiptMissing | Cause::ReceiptUnreadable) => vec![Remedy::Setup {
            workspace: record.worktree.clone(),
        }],
        Some(Cause::RecordUnreadable { file }) => vec![Remedy::Reveal { path: file.clone() }],
        _ if class == Class::Active && record.state == ledger::State::Removing => {
            vec![Remedy::RemoveWorkspace {
                root,
                name: record.workspace.clone(),
            }]
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Removal

/// One instance to remove, with the generation of the record the person was
/// shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Target {
    pub instance: Uuid,
    pub generation: String,
    /// What was seen of each branch in its backend ([`Entry::target`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backend: Vec<ledger::BranchExpect>,
}

#[derive(Debug, Clone, Default)]
pub struct RemoveRequest {
    pub targets: Vec<Target>,
    /// Snapshot each branch first (`gc-<workspace>-<date>`).
    pub snapshot: bool,
    /// Go on past what cannot be verified (daemons whose state cannot be read,
    /// a branch whose drop failed): it stays recorded. Nothing else is
    /// overridden: an unknown entry stays unknown.
    pub force: bool,
    /// The pitchfork to ask; the one on `PATH` when `None`.
    pub pitchfork: Option<Pitchfork>,
}

/// An instance that was removed.
#[derive(Debug, Clone, Serialize)]
pub struct Removed {
    pub instance: Uuid,
    pub workspace: String,
    /// What was dropped, in words.
    pub dropped: Vec<String>,
    /// The snapshots taken first, as `service/name` (or `database/name`).
    pub snapshots: Vec<String>,
    /// What the record still lists: it stays for a later clean-up.
    pub still_recorded: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub may_still_run: Option<MayStillRun>,
    pub warnings: Vec<String>,
}

/// An instance that was left as it was.
#[derive(Debug, Clone, Serialize)]
pub struct Skipped {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub reason: String,
    /// What was done before it stopped, when it did stop part-way.
    pub done: Vec<String>,
    /// `force` goes on past the reason.
    pub forceable: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Outcome {
    pub removed: Vec<Removed>,
    pub skipped: Vec<Skipped>,
}

/// Remove the instances `request` names. Each is read again first: it must
/// still be a removable entry whose record has the generation shown, and each
/// drop is decided under the backend's own lock by the branch's recorded owner.
/// `say` is told what is being done, for a progress line.
pub fn remove(
    project: &Project,
    request: &RemoveRequest,
    say: &mut dyn FnMut(&str),
) -> Result<Outcome> {
    let mut outcome = Outcome::default();
    for target in &request.targets {
        match remove_one(project, target, request, say) {
            Ok(removed) => outcome.removed.push(removed),
            Err(skipped) => outcome.skipped.push(skipped),
        }
    }
    Ok(outcome)
}

fn skipped(target: &Target, workspace: Option<&str>, reason: impl Into<String>) -> Skipped {
    Skipped {
        instance: Some(target.instance),
        workspace: workspace.map(str::to_string),
        reason: reason.into(),
        done: Vec::new(),
        forceable: false,
    }
}

fn skipped_by(target: &Target, workspace: &str, error: &anyhow::Error) -> Skipped {
    let mut out = skipped(target, Some(workspace), format!("{error:#}"));
    if let Some(protected) = error.downcast_ref::<Protected>() {
        out.reason = protected.reason.clone();
        out.forceable = protected.forceable;
    } else if let Some(partial) = error.downcast_ref::<PartialRemoval>() {
        out.reason = partial.to_string();
        out.done = partial.done.clone();
        out.forceable = partial.forceable;
    }
    out
}

/// The entry for `instance` as the files and the backends say now.
fn fresh(project: &Project, instance: Uuid) -> Result<Option<Entry>> {
    let listing = ledger::list(&project.git_common_dir)?;
    let registered = registered(project)?;
    let Some(record) = listing.records.iter().find(|r| r.instance_id == instance) else {
        return Ok(None);
    };
    let verdict = classify(project, record, &registered);
    let options = Options {
        measure: false,
        evidence: false,
        backfill: false,
    };
    let mut backends = Backends::new(project, false);
    Ok(Some(entry_of(
        project,
        record,
        verdict,
        &options,
        &mut backends,
    )))
}

fn remove_one(
    project: &Project,
    target: &Target,
    request: &RemoveRequest,
    say: &mut dyn FnMut(&str),
) -> std::result::Result<Removed, Skipped> {
    let common = &project.git_common_dir;
    let failed = |e: anyhow::Error| skipped(target, None, format!("{e:#}"));
    let _removal = ledger::try_removal_lock(common, target.instance)
        .map_err(failed)?
        .ok_or_else(|| {
            skipped(
                target,
                None,
                "another clean-up of this workspace is running",
            )
        })?;
    let entry = fresh(project, target.instance)
        .map_err(failed)?
        .ok_or_else(|| skipped(target, None, "it has no record any more"))?;
    let name = entry.workspace.clone();
    if entry.generation.as_deref() != Some(target.generation.as_str()) {
        return Err(skipped(
            target,
            Some(&name),
            "it changed since it was shown; look at the inventory again",
        ));
    }
    if !entry.removable {
        return Err(skipped(
            target,
            Some(&name),
            entry
                .not_removable
                .unwrap_or_else(|| format!("it is {}", entry.class.label())),
        ));
    }
    if let Some(why) = reset_since_shown(target, &entry) {
        return Err(skipped(target, Some(&name), why));
    }
    let handle = ledger::Handle::new(common, target.instance);
    // Reading the record again checks the generation shown. A removal that is
    // only refused changes nothing: the record says `removing` from the first
    // effect on (a snapshot, a drop), not before.
    let record = handle
        .read()
        .map_err(|e| skipped_by(target, &name, &e))?
        .filter(|record| record.generation() == target.generation)
        .ok_or_else(|| {
            skipped(
                target,
                Some(&name),
                "it changed since it was shown; look at the inventory again",
            )
        })?;

    let mut removed = Removed {
        instance: target.instance,
        workspace: name.clone(),
        dropped: Vec::new(),
        snapshots: Vec::new(),
        still_recorded: Vec::new(),
        may_still_run: None,
        warnings: Vec::new(),
    };
    if entry.class == Class::Missing {
        // The snapshot is taken by the removal itself, under the workspace's
        // process lock and receipt lock, with the same revalidation before it
        // and after it (see [`remove_missing`]).
        if request.snapshot {
            say(&format!("snapshotting the data of '{name}'"));
        }
        say(&format!("removing '{name}' (its directory is gone)"));
        return remove_missing(project, target, request, &record, removed);
    }
    if request.snapshot {
        say(&format!("snapshotting the data of '{name}'"));
        let taken = snapshot_all(project, &record, &entry, &handle, &mut removed.snapshots);
        after_snapshot();
        taken.map_err(|e| Skipped {
            done: snapshotted(&removed.snapshots),
            ..skipped(target, Some(&name), format!("{e:#}; nothing was dropped"))
        })?;
        // The snapshot took time: the workspace may have come back, or its
        // data been replaced, since it was looked at. (Journaling the snapshot
        // moved the record to `removing`: that is ours.)
        let ours = record.generation_in(ledger::State::Removing);
        revalidate(project, target, &entry.class, Some(&ours)).map_err(|e| Skipped {
            done: snapshotted(&removed.snapshots),
            ..skipped(target, Some(&name), format!("{e:#}; nothing was dropped"))
        })?;
    }
    say(&format!("removing '{name}' (removed outside devflow)"));
    remove_recorded(project, target, request, &record, &handle, removed)
}

/// What a clean-up shows of the snapshots it took.
fn snapshotted(labels: &[String]) -> Vec<String> {
    labels
        .iter()
        .map(|label| format!("snapshotted {label}"))
        .collect()
}

/// The entry for `target` as the files and the backends say now, and an error
/// unless it is still what the person was shown: a record of the same
/// generation, still of `class` and removable, whose branches are the data that
/// was shown.
///
/// `ours`: the generation the record has after what this clean-up itself did
/// (journaling a snapshot moves it to `removing`), also accepted.
fn revalidate(
    project: &Project,
    target: &Target,
    class: &Class,
    ours: Option<&str>,
) -> Result<Entry> {
    let entry = fresh(project, target.instance)?.context("it has no record any more")?;
    ensure!(
        entry.generation.as_deref() == Some(target.generation.as_str())
            || (ours.is_some() && entry.generation.as_deref() == ours),
        "it changed since it was shown; look at the inventory again"
    );
    ensure!(
        entry.class == *class && entry.removable,
        "it is {} now, not what was shown; look at the inventory again",
        entry.class.label()
    );
    if let Some(why) = reset_since_shown(target, &entry) {
        bail!(why);
    }
    Ok(entry)
}

// Run between the snapshot and the removal in a test, to change the world there.
#[cfg(test)]
thread_local! {
    static AFTER_SNAPSHOT: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

fn after_snapshot() {
    #[cfg(test)]
    AFTER_SNAPSHOT.with(|hook| {
        if let Some(hook) = &*hook.borrow() {
            hook();
        }
    });
}

/// Whether a branch the instance owns is not the data the person was shown: a
/// reset replaces a branch's data (a database's generation, a container's
/// volume) and keeps its id and owner. Every branch with such an identity must
/// be in the target with the same one. The drop checks it again under the
/// backend's own lock; this is the early, whole-entry refusal.
fn reset_since_shown(target: &Target, entry: &Entry) -> Option<String> {
    entry
        .branches
        .iter()
        .filter(|b| b.presence == Presence::Owned)
        .filter(|b| b.generation.is_some() || b.volume.is_some())
        .find(|b| {
            !target
                .backend
                .iter()
                .any(|e| e.id == b.id && e.generation == b.generation && e.volume == b.volume)
        })
        .map(|b| {
            format!(
                "{} is not the data that was shown (it was reset, re-created or is new); look at \
                 the inventory again",
                b.describe()
            )
        })
}

/// A registered worktree whose directory is gone: the ordinary removal, which
/// also forgets the registration. The removal is told the directory was shown as
/// gone, so it refuses a checkout that came back (whatever `force` says), and
/// it runs the snapshots and the revalidation of what was shown itself, under
/// the workspace's process lock and receipt lock, right before its first effect;
/// the workspace is compared again after them.
fn remove_missing(
    project: &Project,
    target: &Target,
    request: &RemoveRequest,
    record: &Record,
    mut removed: Removed,
) -> std::result::Result<Removed, Skipped> {
    let name = record.workspace.clone();
    let workspace = worktree::list(project)
        .map_err(|e| skipped(target, Some(&name), format!("{e:#}")))?
        .into_iter()
        .find(|w| admin_id_of(w) == record.admin_id)
        .ok_or_else(|| skipped(target, Some(&name), "its registration is gone"))?;
    let snapshots = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let gate = {
        let (project, target) = (project.clone(), target.clone());
        let (snapshot, snapshots) = (request.snapshot, snapshots.clone());
        Gate::new(move || {
            let entry = revalidate(&project, &target, &Class::Missing, None)?;
            if snapshot {
                let handle = ledger::Handle::new(&project.git_common_dir, target.instance);
                let record = handle
                    .read()?
                    .filter(|record| record.generation() == target.generation)
                    .context("it changed since it was shown; look at the inventory again")?;
                let ours = record.generation_in(ledger::State::Removing);
                let mut taken = snapshots.lock().unwrap_or_else(|e| e.into_inner());
                let result = snapshot_all(&project, &record, &entry, &handle, &mut taken);
                drop(taken);
                after_snapshot();
                result.map_err(|e| e.context("nothing was dropped"))?;
                revalidate(&project, &target, &Class::Missing, Some(&ours))?;
            }
            Ok(())
        })
    };
    let options = RemoveOptions {
        force: request.force,
        pitchfork: request.pitchfork.clone(),
        expect: Some(ExpectedWorkspace {
            path: workspace.path.clone(),
            instance_id: Some(target.instance),
            fingerprint: None,
            missing: true,
        }),
        expect_branches: target.backend.clone(),
        before_effects: Some(gate),
        ..RemoveOptions::default()
    };
    let result = setup::remove_workspace(project, workspace, options, &mut NoProgress);
    removed.snapshots = snapshots.lock().unwrap_or_else(|e| e.into_inner()).clone();
    match result {
        Ok(done) => {
            removed.dropped = done
                .dropped_branch
                .map(|id| vec![format!("dropped branch {id}")])
                .unwrap_or_default();
            removed.still_recorded = done.still_recorded;
            removed.may_still_run = done.may_still_run;
            removed.warnings.extend(done.warnings);
            Ok(removed)
        }
        Err(e) => {
            let mut out = skipped_by(target, &name, &e);
            let mut done = snapshotted(&removed.snapshots);
            done.append(&mut out.done);
            out.done = done;
            Err(out)
        }
    }
}

/// A workspace with no registration and no directory: its record is all there
/// is. Daemons first, then the branches, then the record.
fn remove_recorded(
    project: &Project,
    target: &Target,
    request: &RemoveRequest,
    record: &Record,
    handle: &ledger::Handle,
    mut removed: Removed,
) -> std::result::Result<Removed, Skipped> {
    let name = record.workspace.clone();
    let force = request.force;
    let partial = |removed: &Removed, reason: String, forceable: bool| Skipped {
        done: removed.dropped.clone(),
        forceable,
        ..skipped(target, Some(&name), reason)
    };

    // Daemons: asked about, never stopped (pitchfork would run their lifecycle
    // hooks from a configuration nobody approved). The record stops listing
    // them only when a listing that was read shows nothing of the namespace
    // running.
    if let Some(namespace) = record.resources.namespace.as_deref() {
        let ids = &record.resources.daemons;
        let pitchfork = request.pitchfork.clone().or_else(Pitchfork::locate);
        let unverified = record.resources.namespace_unverified;
        let ask = processes::uses_pitchfork(&project.root);
        // A namespace in doubt is asked about whatever the project uses: only
        // a listing that was read clears it.
        if !ids.is_empty() || ask || unverified {
            match processes::check_recorded(
                pitchfork.as_ref(),
                namespace,
                ids,
                force,
                ask,
                unverified,
            ) {
                Ok(checked) => {
                    removed.warnings.extend(checked.warnings);
                    removed.may_still_run = checked.may_still_run;
                    // Verified: the record stops naming the daemons. Otherwise
                    // the namespace stays named, with the ids it was recorded
                    // with and the ones found running, even with none.
                    let recorded = if checked.verified {
                        handle.release_daemons()
                    } else {
                        let observed = removed
                            .may_still_run
                            .as_ref()
                            .map(|may| may.ids.clone())
                            .unwrap_or_default();
                        handle.mark_unverified(&observed)
                    };
                    if let Err(e) = recorded {
                        removed
                            .warnings
                            .push(format!("could not update the workspace's record: {e:#}"));
                    }
                }
                Err(refusal) => {
                    return Err(partial(
                        &removed,
                        format!("cannot tell what pitchfork runs for '{name}': {refusal}"),
                        true,
                    ));
                }
            }
        }
    }

    // The branches.
    let mut warnings = Vec::new();
    let plan = removal::plan_recorded(project, record, &target.backend, force, &mut warnings);
    removed.warnings.append(&mut warnings);
    let plan = match plan {
        Ok(plan) => plan,
        Err(e) => return Err(skipped_by(target, &name, &e)),
    };
    match removal::execute(plan, force, &mut warnings, Some(handle)) {
        Ok(executed) => removed.dropped.extend(executed.done),
        Err(failed) => {
            removed.dropped.extend(failed.done);
            return Err(partial(
                &removed,
                format!("{:#}", failed.cause),
                failed.forceable,
            ));
        }
    }
    removed.warnings.extend(warnings);

    // The port it was given goes back, as a removal of a registered workspace
    // gives it back once its worktree is gone (best effort). The workspace was
    // classified as removed elsewhere (no registered checkout carries the
    // instance, its directory is gone), so the reservation is released by
    // project and instance and never by path.
    crate::ports::release_instance(record.project_id, record.instance_id);
    match handle.finish() {
        Ok(ledger::Finish::Kept) => {
            removed.still_recorded = handle
                .read()
                .ok()
                .flatten()
                .map(|record| setup::ledger_resources(&record))
                .unwrap_or_default();
        }
        Ok(_) => {}
        Err(e) => removed
            .warnings
            .push(format!("could not finish the workspace's record: {e:#}")),
    }
    Ok(removed)
}

/// The name of the snapshot a clean-up takes of `workspace`'s data.
fn snapshot_name(workspace: &str, instance: Uuid) -> String {
    let date = Utc::now().format("%Y%m%d");
    let name = format!("gc-{}-{date}", worktree::sanitize(workspace));
    if crate::db::validate_snapshot_name(&name).is_ok() {
        name
    } else {
        format!("gc-{}-{date}", crate::ids::short(instance))
    }
}

/// Snapshot every branch of the entry that is there and is the instance's,
/// each journaled before it is taken, and each only if it is still the data
/// that was shown (judged under the backend's lock). A branch recorded before
/// its creation is asked about like any other: when the backend has it ready
/// and the instance's, it is snapshotted. Whatever cannot be snapshotted (not
/// ready, not asked, not answered) stops the removal before anything is
/// dropped: a requested snapshot is never silently skipped.
fn snapshot_all(
    project: &Project,
    record: &Record,
    entry: &Entry,
    handle: &ledger::Handle,
    taken_labels: &mut Vec<String>,
) -> Result<()> {
    let name = snapshot_name(&record.workspace, record.instance_id);
    let instance = record.instance_id;
    for branch in &entry.branches {
        match &branch.presence {
            // Not there, or another instance's: nothing of this workspace's.
            Presence::Absent | Presence::Foreign => continue,
            Presence::Owned => {}
            Presence::Unreachable { reason } => {
                bail!(
                    "{} cannot be asked about ({reason}), so it cannot be snapshotted",
                    branch.describe()
                );
            }
            Presence::NotChecked => {
                bail!(
                    "{} was not looked at, so it cannot be snapshotted",
                    branch.describe()
                );
            }
        }
        if branch.ready != Some(true) {
            bail!(
                "{} is not ready (it is being made, or failed), so it cannot be snapshotted; \
                 remove without a snapshot to drop it as it is",
                branch.describe()
            );
        }
        let effect = Effect::Snapshot {
            service: branch.service.clone(),
            branch: branch.id,
            name: name.clone(),
        };
        handle.intend(&effect)?;
        let taken: Result<String> = match branch.engine {
            Some(Engine::Postgres) => Server::for_project(project)
                .and_then(|server| {
                    let server = server.context("the project has no postgres service any more")?;
                    match branch.generation {
                        Some(generation) => server.create_snapshot_owned_at(
                            &name,
                            branch.id,
                            instance,
                            generation as i32,
                        ),
                        None => server.create_snapshot(&name, branch.id),
                    }
                })
                .map(|snap| format!("database/{}@v{}", snap.name, snap.version)),
            Some(engine) => {
                let key = branch
                    .service
                    .as_deref()
                    .context("a container branch without a service")?;
                Service::recorded(project, key, engine)
                    .and_then(|svc| {
                        svc.snapshot_branch_if(branch.id, &name, |current| {
                            ensure!(
                                current.owner == Some(instance),
                                "branch {} was not created by this workspace's instance",
                                branch.id
                            );
                            ensure!(
                                branch.generation.is_none_or(|g| g == current.generation)
                                    && branch.volume.as_ref().is_none_or(|v| *v == current.volume),
                                "branch {} was reset since it was shown",
                                branch.id
                            );
                            Ok(())
                        })
                    })
                    .map(|snap| format!("{key}/{}@v{}", snap.name, snap.version))
            }
            None => Err(anyhow::anyhow!(
                "the engine of {} is not known",
                branch.describe()
            )),
        };
        match taken {
            Ok(label) => {
                handle.done(&effect, Some(label.clone()))?;
                taken_labels.push(label);
            }
            Err(e) => {
                handle.failed(&effect, &format!("{e:#}"));
                return Err(e.context(format!("cannot snapshot {}", branch.describe())));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

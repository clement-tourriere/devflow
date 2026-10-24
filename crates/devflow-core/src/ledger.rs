//! The workspace ledger: what a workspace owns, recorded where Git cannot take
//! it away.
//!
//! A workspace's receipt lives in its worktree's administrative directory
//! (`.git/worktrees/<id>/devflow/`), which `git worktree remove` deletes, as do
//! the agent hosts that remove worktrees themselves. The data branches,
//! containers and daemons the workspace owned then stay with nothing that names
//! them. The ledger keeps one record per workspace instance in the repository's
//! common Git directory, `devflow/workspaces/<instance id>.json`, which Git
//! never touches:
//!
//! - who: the instance id, the project, the checkout the record was written for
//!   (canonical main checkout and common directory), the workspace name at
//!   creation, the worktree and Git's administrative id for it;
//! - what it owns, by identity: the data branches (a pending one, recorded
//!   before its creation, included) per service, the pitchfork namespace and the
//!   daemon ids devflow started and has not seen stop;
//! - a removal journal: the effect a removal is about to have, recorded before
//!   it, and its outcome.
//!
//! The record never holds a credential. The receipt stays the phase record and
//! the holder of credentials; the record is authoritative for ownership once the
//! receipt is gone.
//!
//! **One writer.** [`Receipt::save`] writes the record first, then the receipt,
//! under the receipt lock its callers hold, so the two cannot disagree about a
//! resource the receipt names: the record is never behind the receipt. What a
//! record lists only grows while the receipt changes (a branch is listed once it
//! was ever recorded, as the receipt's `exported` names are), and shrinks only
//! when a removal has accounted for the resource (dropped, found to belong to
//! somebody else, or found not to exist). The daemons are the exception: they
//! mirror the receipt's `processes`, which is cleared by a stop whose result was
//! read back.
//!
//! **Locks.** Every record has its own lock (`<instance id>.lock`, held for one
//! read-modify-write and never across a call into a backend). The order is the
//! documented one with the record lock last: process lock, receipt lock, record
//! lock. A removal of a workspace that no longer has a receipt (nothing to lock
//! in the Git directory) holds a removal lock (`<instance id>.removal.lock`)
//! for the whole removal instead, which is not the record's lock, so its journal
//! writes do not wait for themselves.
use crate::config::Engine;
use crate::receipt::Receipt;
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// The version of the record this devflow writes. A newer one is refused, like
/// a newer receipt.
pub const VERSION: u32 = 1;

/// How long a write waits for the record's lock: a write holds it for one
/// read-modify-write of a small file.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// The most journal entries a record keeps (the oldest go first).
const JOURNAL_LIMIT: usize = 200;

/// Where the records of the repository whose common Git directory is
/// `common_dir` live.
pub fn dir(common_dir: &Path) -> PathBuf {
    common_dir.join("devflow").join("workspaces")
}

/// The record file of `instance`.
pub fn path(common_dir: &Path, instance: Uuid) -> PathBuf {
    dir(common_dir).join(format!("{instance}.json"))
}

fn lock_path(common_dir: &Path, instance: Uuid) -> PathBuf {
    dir(common_dir).join(format!("{instance}.lock"))
}

fn removal_lock_path(common_dir: &Path, instance: Uuid) -> PathBuf {
    dir(common_dir).join(format!("{instance}.removal.lock"))
}

/// Whether a workspace is being taken apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Active,
    /// A removal made its first effect and has not finished: `gc` or a later
    /// `remove` completes it.
    Removing,
}

/// The checkout a record was written for: a record copied along with a
/// repository to another place does not give the copy the right to drop what
/// the original owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkout {
    /// The canonical main checkout.
    pub main: PathBuf,
    /// The canonical common Git directory, where the record lives.
    pub common_dir: PathBuf,
    /// Which directory that is ([`repository_identity`]): what tells a
    /// repository that was moved (the same directory, at another path) from
    /// one that was copied here, with its records, from another place or
    /// machine. A record without it can never prove a move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

/// Which directory `common_dir` is, independently of where it is: its inode
/// and when it was created. A directory that was moved keeps both; a copy, a
/// restored backup or another machine's repository has others. `None` when the
/// file system does not say (the identity then proves nothing).
pub fn repository_identity(common_dir: &Path) -> Option<String> {
    let meta = std::fs::metadata(common_dir).ok()?;
    let born = meta
        .created()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!("{}:{born}", meta.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = born;
        None
    }
}

/// A data branch the workspace owns, by identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchRef {
    /// The service key; `None` for a database an earlier build bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Where the branch is looked for; `None` when only the configuration
    /// (which may have changed since) could say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<Engine>,
    pub id: Uuid,
    /// Recorded before its creation: it may never have been made.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
}

/// What a person saw of a branch in its backend, to be checked again under the
/// backend's own lock right before the branch is dropped: a reset replaces a
/// branch's data (a container's volume, a database's generation) while keeping
/// its id and owner, so ownership alone does not say it is the data that was
/// looked at, snapshotted or agreed to be dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchExpect {
    pub id: Uuid,
    /// The backend's generation of the branch (bumped by every reset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,
    /// The volume a container branch had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
}

impl BranchRef {
    /// Where the branch is looked for: the engine recorded with it, and for a
    /// database an earlier build bound (no service key) PostgreSQL. `None` is
    /// a branch whose backend was not recorded, which is looked for in every
    /// backend and never placed by today's configuration. Removal and the
    /// inventory both use this, so they cannot disagree about where a branch is.
    pub fn lookup_engine(&self) -> Option<Engine> {
        match self.service {
            Some(_) => self.engine,
            None => Some(Engine::Postgres),
        }
    }

    /// How to name it in a message.
    pub fn describe(&self) -> String {
        match &self.service {
            Some(key) => format!("branch {} of service '{key}'", self.id),
            None => format!("database branch {} from an earlier build", self.id),
        }
    }
}

/// What the workspace owns.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<BranchRef>,
    /// The pitchfork namespace devflow assigned (`df-<project8>-<instance8>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// The `namespace/name` ids devflow started and has not seen stop, and the
    /// ones a removal that could not stop them found running
    /// ([`Resources::namespace_unverified`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub daemons: Vec<String>,
    /// A removal went on without being able to show that nothing runs under
    /// the namespace (a listing that could not be read, daemons found running
    /// that devflow does not stop): the namespace stays named until a listing
    /// that was read shows none running, even with no id recorded.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub namespace_unverified: bool,
    /// Branches the person asked to keep when they removed the workspace
    /// (`--keep-data`). A receipt that outlives a removal that stopped
    /// half-way still names them as owned, so they are remembered here, apart
    /// from `branches`: a record never lists them again, and a removal that
    /// resumes without the flag does not drop them. Setting the workspace up
    /// again takes them back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disowned: Vec<Uuid>,
}

impl Resources {
    pub fn is_empty(&self) -> bool {
        self.branches.is_empty() && self.daemons.is_empty() && !self.namespace_unverified
    }
}

/// One effect of a removal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "snake_case")]
pub enum Effect {
    StopDaemons {
        ids: Vec<String>,
    },
    Teardown,
    Snapshot {
        service: Option<String>,
        branch: Uuid,
        name: String,
    },
    DropBranch {
        service: Option<String>,
        branch: Uuid,
    },
    /// The person asked to keep the data: the branches are released from the
    /// record, and no clean-up offers them later.
    KeepData {
        branches: Vec<Uuid>,
    },
    DeleteWorktree,
}

impl Effect {
    pub fn describe(&self) -> String {
        match self {
            Effect::StopDaemons { ids } => format!("stop the daemons {}", ids.join(", ")),
            Effect::Teardown => "run the teardown hooks".to_string(),
            Effect::Snapshot {
                service,
                branch,
                name,
            } => match service {
                Some(key) => format!("snapshot branch {branch} of service '{key}' as {name}"),
                None => format!("snapshot database branch {branch} as {name}"),
            },
            Effect::DropBranch { service, branch } => match service {
                Some(key) => format!("drop branch {branch} of service '{key}'"),
                None => format!("drop database branch {branch}"),
            },
            Effect::KeepData { branches } => {
                format!("keep the data of {} branch(es)", branches.len())
            }
            Effect::DeleteWorktree => "delete the worktree".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Recorded before the effect.
    Started,
    Done,
    Failed,
}

/// A line of the removal journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub at: DateTime<Utc>,
    #[serde(flatten)]
    pub effect: Effect,
    pub outcome: Outcome,
    /// Why it failed, or what was found (already gone, somebody else's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub version: u32,
    pub instance_id: Uuid,
    pub project_id: Uuid,
    pub checkout: Checkout,
    /// The workspace name at creation.
    pub workspace: String,
    /// Where the worktree was last seen.
    pub worktree: PathBuf,
    /// Git's name for the worktree (its administrative directory under
    /// `worktrees/`); `None` for the main checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin_id: Option<String>,
    /// Identity of the worktree directory ([`crate::worktree::fingerprint`]):
    /// tells it from another directory created later at the same place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// The branch the checkout held when the record was last written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The commit it was on (full id), for the merge evidence of a branch that
    /// no longer exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_at: Option<DateTime<Utc>>,
    /// The workspace's app port (`DEVFLOW_PORT`), reserved in the machine-wide
    /// port ledger under this instance: a removal gives it back as `remove`
    /// does. Kept once recorded, like the resources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_port: Option<u16>,
    pub updated_at: DateTime<Utc>,
    pub state: State,
    #[serde(default)]
    pub resources: Resources,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub journal: Vec<Entry>,
}

impl Record {
    /// Whether the record still names anything a removal has to account for.
    pub fn holds_resources(&self) -> bool {
        !self.resources.is_empty()
    }

    /// A digest of what a removal would act on: an inventory shows it, and a
    /// removal asked for it is refused when it is not the same any more.
    pub fn generation(&self) -> String {
        self.generation_in(self.state)
    }

    /// [`Record::generation`] as it would be in `state`: the first effect of a
    /// removal (a snapshot, a drop) moves an `active` record to `removing`, and
    /// the one who did that knows the generation it must find afterwards.
    pub fn generation_in(&self, state: State) -> String {
        let value = serde_json::json!({
            "instance": self.instance_id,
            "admin": self.admin_id,
            "state": state,
            "resources": self.resources,
        });
        crate::fsutil::sha256_hex(value.to_string().as_bytes())[..16].to_string()
    }

    fn same_content(&self, other: &Record) -> bool {
        let mut other = other.clone();
        other.updated_at = self.updated_at;
        *self == other
    }

    /// Fold what the receipt now says into the record.
    fn refresh(&mut self, cx: &Placement, receipt: &Receipt) {
        self.project_id = receipt.project_id;
        self.workspace = receipt.workspace.clone();
        self.checkout = cx.checkout.clone();
        self.worktree = cx.worktree.clone();
        self.admin_id = cx.admin_id.clone();
        self.fingerprint = cx.fingerprint.clone();
        if cx.branch.is_some() || cx.head.is_some() {
            self.branch = cx.branch.clone();
            self.head = cx.head.clone();
        }
        self.created_at = self.created_at.min(receipt.created_at);
        self.last_active_at = self.last_active_at.max(receipt.last_active_at);
        self.app_port = receipt.app_port.or(self.app_port);

        let disowned = self.resources.disowned.clone();
        for (key, binding) in &receipt.services {
            if let (true, Some(id)) = (binding.owned, binding.branch_id)
                && !disowned.contains(&id)
            {
                self.note_branch(Some(key), Some(binding.engine), id, false);
            }
        }
        if let Some(pending) = receipt
            .creating
            .as_ref()
            .filter(|pending| !disowned.contains(&pending.branch_id))
        {
            // The backend the creation was begun on is the one recorded with
            // it; a creation from before that was recorded has none, and is
            // looked for in every backend. The configuration (or another
            // branch of the same key) is never where to look.
            self.note_branch(
                Some(&pending.service),
                pending.engine,
                pending.branch_id,
                true,
            );
        }
        if let Some(data) = &receipt.unmapped
            && let (true, Some(id)) = (data.owned, data.branch_id)
            && !disowned.contains(&id)
        {
            self.note_branch(None, Some(Engine::Postgres), id, false);
        }
        self.resources.namespace = Some(receipt.slug());
        let started = receipt
            .processes
            .as_ref()
            .map(|record| record.ids.clone())
            .unwrap_or_default();
        if self.resources.namespace_unverified {
            // A removal went on without seeing that nothing runs: what it
            // found running stays listed, whatever the receipt says, until a
            // listing that was read shows it stopped ([`Handle::release_daemons`]).
            for id in started {
                if !self.resources.daemons.contains(&id) {
                    self.resources.daemons.push(id);
                }
            }
        } else {
            self.resources.daemons = started;
        }
    }

    /// List `id` once; a branch that is bound is no longer pending.
    fn note_branch(
        &mut self,
        service: Option<&str>,
        engine: Option<Engine>,
        id: Uuid,
        pending: bool,
    ) {
        match self.resources.branches.iter_mut().find(|b| b.id == id) {
            Some(known) => {
                known.pending &= pending;
                known.engine = engine.or(known.engine);
                if known.service.is_none() {
                    known.service = service.map(str::to_string);
                }
            }
            None => self.resources.branches.push(BranchRef {
                service: service.map(str::to_string),
                engine,
                id,
                pending,
            }),
        }
    }

    fn push_journal(&mut self, effect: Effect, outcome: Outcome, note: Option<String>) {
        self.journal.push(Entry {
            at: Utc::now(),
            effect,
            outcome,
            note,
        });
        if self.journal.len() > JOURNAL_LIMIT {
            let extra = self.journal.len() - JOURNAL_LIMIT;
            self.journal.drain(..extra);
        }
    }
}

/// What is known about a worktree's place in its repository from its
/// administrative directory alone.
struct Placement {
    checkout: Checkout,
    worktree: PathBuf,
    admin_id: Option<String>,
    fingerprint: Option<String>,
    branch: Option<String>,
    head: Option<String>,
}

/// Whether `dir` is `<common git dir>/worktrees/<name>` of a repository.
fn is_admin_position(dir: &Path) -> bool {
    let Some(worktrees) = dir
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n == "worktrees"))
    else {
        return false;
    };
    worktrees
        .parent()
        .is_some_and(|common| common.join("HEAD").is_file() && common.join("objects").is_dir())
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Where `git_dir` (a worktree's own Git directory: `.git` for the main
/// checkout, `.git/worktrees/<id>` for a linked one) is. `None` for a directory
/// that is not one (a receipt written to a scratch directory by a test): there
/// is no repository to record anything in.
fn context_of(git_dir: &Path) -> Result<Option<Placement>> {
    let linked = git_dir.join("commondir");
    let (common, admin_id, worktree) = if linked.is_file() {
        let relative = std::fs::read_to_string(&linked)
            .with_context(|| format!("cannot read {}", linked.display()))?;
        let common = canonical(&git_dir.join(relative.trim()));
        let marker = git_dir.join("gitdir");
        let target = std::fs::read_to_string(&marker)
            .with_context(|| format!("cannot read {}", marker.display()))?;
        let dot_git = PathBuf::from(target.trim());
        let dot_git = if dot_git.is_absolute() {
            dot_git
        } else {
            git_dir.join(dot_git)
        };
        let worktree = dot_git
            .parent()
            .map(canonical)
            .with_context(|| format!("{} names no worktree", marker.display()))?;
        let id = git_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .with_context(|| format!("{} has no name", git_dir.display()))?;
        (common, Some(id), Some(worktree))
    } else if git_dir.join("HEAD").is_file() && git_dir.join("objects").is_dir() {
        (canonical(git_dir), None, None)
    } else if is_admin_position(git_dir) {
        // Where a worktree's administrative directory is, without the files
        // that make it one: Git removed the worktree (a host deleting it while
        // it was being set up), and something recreated the directory. Not
        // "no repository to record in": the receipt must not be written
        // unrecorded.
        bail!(
            "{} is where Git keeps a worktree's administrative files, and they are gone: the \
             worktree was removed",
            git_dir.display()
        );
    } else {
        return Ok(None);
    };
    let main = git2::Repository::open(&common)
        .ok()
        .and_then(|repo| repo.workdir().map(canonical))
        .with_context(|| format!("{} has no main checkout", common.display()))?;
    let worktree = worktree.unwrap_or_else(|| main.clone());
    let (branch, head) = git2::Repository::open(git_dir)
        .ok()
        .map(|repo| {
            let head = repo.head().ok();
            let branch = head
                .as_ref()
                .filter(|head| head.is_branch())
                .and_then(|head| head.shorthand().ok())
                .map(str::to_string);
            let id = head
                .as_ref()
                .and_then(|head| head.target())
                .map(|id| id.to_string());
            (branch, id)
        })
        .unwrap_or((None, None));
    Ok(Some(Placement {
        fingerprint: crate::worktree::fingerprint(&worktree),
        checkout: Checkout {
            main,
            identity: repository_identity(&common),
            common_dir: common,
        },
        worktree,
        admin_id,
        branch,
        head,
    }))
}

// ---------------------------------------------------------------------------
// Files

/// The record of `instance`, as stored. `Ok(None)` when there is none; an error
/// when the file is there and cannot be read (a state devflow cannot read is
/// unknown, never "nothing").
pub fn read(common_dir: &Path, instance: Uuid) -> Result<Option<Record>> {
    let file = path(common_dir, instance);
    match std::fs::read_to_string(&file) {
        Ok(text) => parse(&text, &file, Some(instance)).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", file.display())),
    }
}

fn parse(text: &str, file: &Path, expected: Option<Uuid>) -> Result<Record> {
    let record: Record = crate::diag::json_from_str(
        text,
        &format!("invalid workspace record {}", file.display()),
    )?;
    ensure!(
        record.version <= VERSION,
        "{} was written by a newer devflow",
        file.display()
    );
    if let Some(expected) = expected {
        ensure!(
            record.instance_id == expected,
            "{} records another workspace instance than its name says",
            file.display()
        );
    }
    Ok(record)
}

fn write(common_dir: &Path, record: &Record) -> Result<()> {
    let text = serde_json::to_vec_pretty(record)?;
    crate::fsutil::write_atomic(&path(common_dir, record.instance_id), &text, true)
}

/// A record that is there and cannot be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unreadable {
    pub file: PathBuf,
    /// The instance the file name says, when it names one.
    pub instance: Option<Uuid>,
    pub reason: String,
}

/// Every record of the repository.
#[derive(Debug, Clone, Default)]
pub struct Listing {
    pub records: Vec<Record>,
    pub unreadable: Vec<Unreadable>,
}

/// Read every record. A file that cannot be read is listed, not skipped.
pub fn list(common_dir: &Path) -> Result<Listing> {
    let folder = dir(common_dir);
    let entries = match std::fs::read_dir(&folder) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Listing::default()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", folder.display())),
    };
    let mut listing = Listing::default();
    for entry in entries {
        let entry = entry.with_context(|| format!("cannot read {}", folder.display()))?;
        let file = entry.path();
        let Some(stem) = file
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".json"))
        else {
            continue;
        };
        // A temporary file of an atomic write is not a record.
        if stem.starts_with('.') {
            continue;
        }
        let named = Uuid::parse_str(stem).ok();
        let read = std::fs::read_to_string(&file)
            .map_err(anyhow::Error::from)
            .and_then(|text| parse(&text, &file, named));
        match (read, named) {
            (Ok(record), Some(_)) => listing.records.push(record),
            (Ok(_), None) => listing.unreadable.push(Unreadable {
                file,
                instance: None,
                reason: "its name is not a workspace instance id".to_string(),
            }),
            (Err(error), _) => listing.unreadable.push(Unreadable {
                file,
                instance: named,
                reason: format!("{error:#}"),
            }),
        }
    }
    listing
        .records
        .sort_by_key(|r| (r.created_at, r.instance_id));
    listing.unreadable.sort_by(|a, b| a.file.cmp(&b.file));
    Ok(listing)
}

/// Held while one record is read, changed and written.
pub struct RecordLock {
    _file: std::fs::File,
}

fn open_lock(file: &Path) -> Result<std::fs::File> {
    if let Some(folder) = file.parent() {
        std::fs::create_dir_all(folder)
            .with_context(|| format!("cannot create {}", folder.display()))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(file)
        .with_context(|| format!("cannot open {}", file.display()))
}

fn lock(common_dir: &Path, instance: Uuid) -> Result<RecordLock> {
    let file = lock_path(common_dir, instance);
    let handle = open_lock(&file)?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match handle.try_lock() {
            Ok(()) => return Ok(RecordLock { _file: handle }),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                bail!(
                    "another devflow operation is updating the record of workspace instance \
                     {instance}; try again in a moment"
                )
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("cannot lock {}", file.display()));
            }
        }
    }
}

/// Held for the whole removal of a workspace that has no receipt to lock.
pub struct RemovalLock {
    _file: std::fs::File,
}

/// The removal lock of `instance`, or `None` when another removal holds it.
/// Retried briefly, like [`Receipt::lock_retrying`]: a lock is released when
/// the last duplicate of its file descriptor closes, and a process another
/// thread is spawning can hold one for a few milliseconds after the removal
/// that held it is over.
pub fn try_removal_lock(common_dir: &Path, instance: Uuid) -> Result<Option<RemovalLock>> {
    let file = removal_lock_path(common_dir, instance);
    let handle = open_lock(&file)?;
    for attempt in 0..10 {
        match handle.try_lock() {
            Ok(()) => return Ok(Some(RemovalLock { _file: handle })),
            Err(std::fs::TryLockError::WouldBlock) => {
                if attempt < 9 {
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("cannot lock {}", file.display()));
            }
        }
    }
    Ok(None)
}

/// Read, change and write the record of `instance` under its lock. `Ok(None)`
/// when there is no record; the change is not made then.
fn update<T>(
    common_dir: &Path,
    instance: Uuid,
    change: impl FnOnce(&mut Record) -> Result<T>,
) -> Result<Option<T>> {
    let _held = lock(common_dir, instance)?;
    let Some(mut record) = read(common_dir, instance)? else {
        return Ok(None);
    };
    let before = record.clone();
    let value = change(&mut record)?;
    if !before.same_content(&record) {
        record.updated_at = Utc::now();
        write(common_dir, &record)?;
    }
    Ok(Some(value))
}

// ---------------------------------------------------------------------------
// The writer the receipt goes through

/// Write the record of the workspace whose receipt is `receipt`, in the
/// worktree whose Git directory is `git_dir`: created from the receipt when
/// there is none, otherwise brought up to date with it. The caller holds the
/// workspace's receipt lock and writes the receipt right after
/// ([`Receipt::save`] is the one caller): the record is written first.
///
/// A record that cannot be read is an error: writing the receipt without being
/// able to say what the record lists would let the two drift apart. A
/// `git_dir` that is not a Git directory has no repository to record in, and
/// nothing is written.
pub(crate) fn sync(git_dir: &Path, receipt: &Receipt) -> Result<()> {
    let Some(cx) = context_of(git_dir)? else {
        return Ok(());
    };
    let common = cx.checkout.common_dir.clone();
    let instance = receipt.instance_id;
    let _held = lock(&common, instance)?;
    let existing = read(&common, instance)?;
    let now = Utc::now();
    let mut record = existing.clone().unwrap_or_else(|| Record {
        version: VERSION,
        instance_id: instance,
        project_id: receipt.project_id,
        checkout: cx.checkout.clone(),
        workspace: receipt.workspace.clone(),
        worktree: cx.worktree.clone(),
        admin_id: cx.admin_id.clone(),
        fingerprint: None,
        branch: None,
        head: None,
        created_at: receipt.created_at,
        last_active_at: None,
        app_port: None,
        updated_at: now,
        state: State::Active,
        resources: Resources::default(),
        journal: Vec::new(),
    });
    ensure!(
        record.project_id == receipt.project_id,
        "the record of workspace instance {instance} belongs to another project"
    );
    record.refresh(&cx, receipt);
    if existing
        .as_ref()
        .is_some_and(|old| old.same_content(&record))
    {
        return Ok(());
    }
    record.updated_at = now;
    write(&common, &record)
}

/// A workspace being set up or entered again is no longer being removed.
pub(crate) fn mark_active(git_dir: &Path, instance: Uuid) -> Result<()> {
    let Some(cx) = context_of(git_dir)? else {
        return Ok(());
    };
    update(&cx.checkout.common_dir, instance, |record| {
        record.state = State::Active;
        // A workspace set up again is kept: what was disowned is its own again.
        record.resources.disowned.clear();
        Ok(())
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The removal's side

/// The journal of one record, for a removal. Every write is one locked
/// read-modify-write: nothing here holds the record's lock across an effect.
#[derive(Debug, Clone)]
pub struct Handle {
    common_dir: PathBuf,
    instance: Uuid,
}

/// How a removal's accounting ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    /// Everything was accounted for and the record is gone.
    Deleted,
    /// Something is still listed: the record stays, `removing`.
    Kept,
    /// There was no record.
    Absent,
}

impl Handle {
    pub fn new(common_dir: &Path, instance: Uuid) -> Handle {
        Handle {
            common_dir: common_dir.to_path_buf(),
            instance,
        }
    }

    pub fn instance(&self) -> Uuid {
        self.instance
    }

    pub fn read(&self) -> Result<Option<Record>> {
        read(&self.common_dir, self.instance)
    }

    /// The first effect of a removal is about to happen: the record says
    /// `removing` from now on. With `expected`, refused unless the record
    /// still has that generation.
    pub fn begin(&self, expected: Option<&str>) -> Result<Option<Record>> {
        update(&self.common_dir, self.instance, |record| {
            if let Some(expected) = expected {
                ensure!(
                    record.generation() == expected,
                    "the record of workspace '{}' changed since it was shown",
                    record.workspace
                );
            }
            record.state = State::Removing;
            Ok(record.clone())
        })
    }

    /// Record, before it happens, what a removal is going to do. An error means
    /// it must not happen: an effect nobody wrote down cannot be finished later.
    pub fn intend(&self, effect: &Effect) -> Result<()> {
        update(&self.common_dir, self.instance, |record| {
            record.state = State::Removing;
            record.push_journal(effect.clone(), Outcome::Started, None);
            Ok(())
        })
        .with_context(|| {
            format!(
                "cannot record that devflow is going to {}",
                effect.describe()
            )
        })
        .map(|_| ())
    }

    /// The effect happened (or was found unnecessary: `note` says which).
    pub fn done(&self, effect: &Effect, note: Option<String>) -> Result<()> {
        update(&self.common_dir, self.instance, |record| {
            record.push_journal(effect.clone(), Outcome::Done, note);
            Ok(())
        })
        .map(|_| ())
    }

    /// The effect failed; a record that cannot be written is not worse than the
    /// failure already being reported.
    pub fn failed(&self, effect: &Effect, why: &str) {
        let _ = update(&self.common_dir, self.instance, |record| {
            record.push_journal(effect.clone(), Outcome::Failed, Some(why.to_string()));
            Ok(())
        });
    }

    /// The branches `ids` are accounted for (dropped, somebody else's, or not
    /// there): the record stops listing them.
    pub fn release_branches(&self, ids: &[Uuid]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        update(&self.common_dir, self.instance, |record| {
            record.resources.branches.retain(|b| !ids.contains(&b.id));
            Ok(())
        })
        .map(|_| ())
    }

    /// The person keeps these branches (`--keep-data`): they are disowned, which
    /// is remembered, and the record stops listing them. One write, so a crash
    /// leaves them either owned or disowned and never neither.
    pub fn disown_branches(&self, ids: &[Uuid]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        update(&self.common_dir, self.instance, |record| {
            record.resources.branches.retain(|b| !ids.contains(&b.id));
            for id in ids {
                if !record.resources.disowned.contains(id) {
                    record.resources.disowned.push(*id);
                }
            }
            Ok(())
        })
        .map(|_| ())
    }

    /// A listing that was read shows nothing of the namespace running: the
    /// record stops naming the daemons, and the namespace is no longer in doubt.
    pub fn release_daemons(&self) -> Result<()> {
        update(&self.common_dir, self.instance, |record| {
            record.resources.daemons.clear();
            record.resources.namespace_unverified = false;
            Ok(())
        })
        .map(|_| ())
    }

    /// The removal goes on without having seen that nothing runs under the
    /// namespace: it stays named (see [`Resources::namespace_unverified`]),
    /// with the daemons it found running (`observed`) added to the ones already
    /// listed, so that none of them can be forgotten without a listing that
    /// shows it stopped.
    pub fn mark_unverified(&self, observed: &[String]) -> Result<()> {
        update(&self.common_dir, self.instance, |record| {
            record.resources.namespace_unverified = true;
            for id in observed {
                if !record.resources.daemons.contains(id) {
                    record.resources.daemons.push(id.clone());
                }
            }
            Ok(())
        })
        .map(|_| ())
    }

    /// Delete the record when nothing is left to account for; otherwise keep it
    /// `removing` for `gc` or a later `remove`.
    pub fn finish(&self) -> Result<Finish> {
        let _held = lock(&self.common_dir, self.instance)?;
        let Some(mut record) = read(&self.common_dir, self.instance)? else {
            return Ok(Finish::Absent);
        };
        if record.holds_resources() {
            record.state = State::Removing;
            record.updated_at = Utc::now();
            write(&self.common_dir, &record)?;
            return Ok(Finish::Kept);
        }
        std::fs::remove_file(path(&self.common_dir, self.instance)).with_context(|| {
            format!(
                "cannot delete the record of workspace instance {}",
                self.instance
            )
        })?;
        drop(_held);
        // The lock files are only ever useful beside a record.
        let _ = std::fs::remove_file(lock_path(&self.common_dir, self.instance));
        let _ = std::fs::remove_file(removal_lock_path(&self.common_dir, self.instance));
        Ok(Finish::Deleted)
    }

    /// Delete the record without accounting for anything: the workspace's
    /// identity was replaced on purpose (`setup --new-instance`, a forked VM),
    /// which disowns what the old identity owned.
    pub fn forget(&self) -> Result<()> {
        let _held = lock(&self.common_dir, self.instance)?;
        match std::fs::remove_file(path(&self.common_dir, self.instance)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| {
                format!(
                    "cannot delete the record of workspace instance {}",
                    self.instance
                )
            }),
        }
    }

    /// Refresh the worktree path and the checkout identity of a record that
    /// moved with its repository or its worktree.
    pub fn relocate(&self, checkout: Checkout, worktree: Option<PathBuf>) -> Result<()> {
        update(&self.common_dir, self.instance, |record| {
            record.checkout = checkout;
            if let Some(worktree) = worktree {
                record.worktree = worktree;
            }
            Ok(())
        })
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt::{PendingBranch, ProcessRecord};
    use crate::worktree::testutil::{TestRepo, repo};

    fn workspace(t: &TestRepo, name: &str) -> crate::worktree::Workspace {
        crate::worktree::create(&t.project, name, None).unwrap()
    }

    fn receipt_of(t: &TestRepo, name: &str) -> Receipt {
        Receipt::new(t.project.id(), name, None)
    }

    #[test]
    fn saving_the_receipt_writes_the_record_first_and_without_credentials() {
        let t = repo(None);
        let ws = workspace(&t, "feature/a");
        let mut receipt = receipt_of(&t, "feature/a");
        receipt.services.insert(
            "db".into(),
            crate::receipt::ServiceBinding::postgres(
                crate::receipt::DataBinding {
                    branch_id: Some(Uuid::from_u128(7)),
                    database: "df_x".into(),
                    role: "role".into(),
                    // kingfisher:ignore
                    url: "postgres://role:hunter2@127.0.0.1:5432/df_x".into(),
                    source: "empty".into(),
                    owned: true,
                },
                None,
            ),
        );
        receipt.save(&ws.git_dir).unwrap();

        let record = read(&t.project.git_common_dir, receipt.instance_id)
            .unwrap()
            .expect("the record is written with the receipt");
        assert_eq!(record.workspace, "feature/a");
        assert_eq!(record.project_id, t.project.id());
        assert_eq!(record.state, State::Active);
        assert_eq!(record.checkout.main, t.project.root);
        assert_eq!(record.checkout.common_dir, t.project.git_common_dir);
        assert_eq!(record.worktree, ws.path);
        assert_eq!(
            record.admin_id.as_deref(),
            ws.git_dir.file_name().and_then(|n| n.to_str())
        );
        assert_eq!(record.branch.as_deref(), Some("feature/a"));
        assert!(record.head.is_some() && record.fingerprint.is_some());
        assert_eq!(
            record.resources.branches,
            [BranchRef {
                service: Some("db".into()),
                engine: Some(Engine::Postgres),
                id: Uuid::from_u128(7),
                pending: false
            }]
        );
        assert_eq!(record.resources.namespace, Some(receipt.slug()));
        let text =
            std::fs::read_to_string(path(&t.project.git_common_dir, receipt.instance_id)).unwrap();
        assert!(
            !text.contains("hunter2") && !text.contains("postgres://"),
            "{text}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path(&t.project.git_common_dir, receipt.instance_id))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn a_record_outlives_the_worktree_and_its_receipt() {
        let t = repo(None);
        let ws = workspace(&t, "gone");
        let mut receipt = receipt_of(&t, "gone");
        receipt.creating = Some(PendingBranch {
            service: "cache".into(),
            branch_id: Uuid::from_u128(9),
            engine: None,
        });
        receipt.save(&ws.git_dir).unwrap();
        crate::worktree::force_remove(&t.project, &ws).unwrap();
        assert!(
            !ws.git_dir.exists(),
            "git took the receipt with the worktree"
        );

        let record = read(&t.project.git_common_dir, receipt.instance_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            record.resources.branches,
            [BranchRef {
                service: Some("cache".into()),
                engine: None,
                id: Uuid::from_u128(9),
                pending: true
            }]
        );
    }

    #[test]
    fn what_the_record_lists_grows_with_the_receipt_and_a_pending_branch_is_bound_once() {
        let t = repo(None);
        let ws = workspace(&t, "grow");
        let mut receipt = receipt_of(&t, "grow");
        let id = Uuid::from_u128(11);
        receipt.creating = Some(PendingBranch {
            service: "cache".into(),
            branch_id: id,
            engine: Some(Engine::Valkey),
        });
        receipt.save(&ws.git_dir).unwrap();
        let common = &t.project.git_common_dir;
        let first = read(common, receipt.instance_id).unwrap().unwrap();
        assert!(first.resources.branches[0].pending);

        receipt.creating = None;
        receipt.services.insert(
            "cache".into(),
            crate::receipt::ServiceBinding::container(
                Engine::Valkey,
                id,
                "redis://x".into(),
                None,
                "empty".into(),
                crate::receipt::CloneReport {
                    method: None,
                    captured_at: None,
                    allocated_bytes: None,
                    source_downtime_ms: None,
                },
                None,
            ),
        );
        receipt.save(&ws.git_dir).unwrap();
        let bound = read(common, receipt.instance_id).unwrap().unwrap();
        assert_eq!(bound.resources.branches.len(), 1, "{:?}", bound.resources);
        assert!(!bound.resources.branches[0].pending);
        assert_eq!(bound.resources.branches[0].engine, Some(Engine::Valkey));

        // The receipt forgets the binding; the record keeps what was owned.
        receipt.services.clear();
        receipt.save(&ws.git_dir).unwrap();
        let after = read(common, receipt.instance_id).unwrap().unwrap();
        assert_eq!(after.resources.branches, bound.resources.branches);
    }

    #[test]
    fn daemons_mirror_the_receipt_until_a_stop_clears_them() {
        let t = repo(None);
        let ws = workspace(&t, "ps");
        let mut receipt = receipt_of(&t, "ps");
        receipt.processes = Some(ProcessRecord {
            started_at: Utc::now(),
            ids: vec![format!("{}/web", receipt.slug())],
        });
        receipt.save(&ws.git_dir).unwrap();
        let common = &t.project.git_common_dir;
        let record = read(common, receipt.instance_id).unwrap().unwrap();
        assert_eq!(
            record.resources.daemons,
            [format!("{}/web", receipt.slug())]
        );
        receipt.processes = None;
        receipt.save(&ws.git_dir).unwrap();
        let record = read(common, receipt.instance_id).unwrap().unwrap();
        assert!(record.resources.daemons.is_empty());
    }

    #[test]
    fn an_unreadable_or_newer_record_stops_the_receipt_from_being_written() {
        let t = repo(None);
        let ws = workspace(&t, "bad");
        let receipt = receipt_of(&t, "bad");
        receipt.save(&ws.git_dir).unwrap();
        let file = path(&t.project.git_common_dir, receipt.instance_id);
        std::fs::write(&file, "{ not json").unwrap();
        let error = receipt.save(&ws.git_dir).unwrap_err();
        assert!(
            format!("{error:#}").contains("invalid workspace record"),
            "{error:#}"
        );

        let mut newer: serde_json::Value =
            serde_json::to_value(sample(&t, receipt.instance_id)).unwrap();
        newer["version"] = 99.into();
        std::fs::write(&file, newer.to_string()).unwrap();
        let error = receipt.save(&ws.git_dir).unwrap_err();
        assert!(format!("{error:#}").contains("newer devflow"), "{error:#}");

        let listing = list(&t.project.git_common_dir).unwrap();
        assert!(listing.records.is_empty());
        assert_eq!(listing.unreadable.len(), 1);
        assert_eq!(listing.unreadable[0].instance, Some(receipt.instance_id));
    }

    fn sample(t: &TestRepo, instance: Uuid) -> Record {
        Record {
            version: VERSION,
            instance_id: instance,
            project_id: t.project.id(),
            checkout: Checkout {
                main: t.project.root.clone(),
                common_dir: t.project.git_common_dir.clone(),
                identity: None,
            },
            workspace: "w".into(),
            worktree: t.project.root.join("w"),
            admin_id: Some("w".into()),
            fingerprint: None,
            branch: None,
            head: None,
            created_at: Utc::now(),
            last_active_at: None,
            app_port: None,
            updated_at: Utc::now(),
            state: State::Active,
            resources: Resources::default(),
            journal: Vec::new(),
        }
    }

    #[test]
    fn a_receipt_outside_a_repository_writes_no_record() {
        let dir = tempfile::tempdir().unwrap();
        let receipt = Receipt::new(Uuid::new_v4(), "x", None);
        receipt.save(dir.path()).unwrap();
        assert!(!dir.path().join("devflow").join("workspaces").exists());
    }

    #[test]
    fn the_journal_comes_before_the_effect_and_the_record_goes_when_everything_is_accounted_for() {
        let t = repo(None);
        let ws = workspace(&t, "rm");
        let mut receipt = receipt_of(&t, "rm");
        let id = Uuid::from_u128(21);
        receipt.creating = Some(PendingBranch {
            service: "cache".into(),
            branch_id: id,
            engine: Some(Engine::Valkey),
        });
        receipt.processes = Some(ProcessRecord {
            started_at: Utc::now(),
            ids: vec!["ns/web".into()],
        });
        receipt.save(&ws.git_dir).unwrap();
        let handle = Handle::new(&t.project.git_common_dir, receipt.instance_id);

        let drop_branch = Effect::DropBranch {
            service: Some("cache".into()),
            branch: id,
        };
        handle.intend(&drop_branch).unwrap();
        let record = handle.read().unwrap().unwrap();
        assert_eq!(record.state, State::Removing);
        assert_eq!(record.journal.len(), 1);
        assert_eq!(record.journal[0].outcome, Outcome::Started);
        assert!(record.holds_resources());

        handle.done(&drop_branch, None).unwrap();
        handle.release_branches(&[id]).unwrap();
        // The daemons are still listed: the record stays, removing.
        assert_eq!(handle.finish().unwrap(), Finish::Kept);
        let kept = handle.read().unwrap().unwrap();
        assert_eq!(kept.state, State::Removing);
        assert!(kept.resources.branches.is_empty());
        assert_eq!(kept.resources.daemons, ["ns/web"]);

        handle.release_daemons().unwrap();
        assert_eq!(handle.finish().unwrap(), Finish::Deleted);
        assert!(handle.read().unwrap().is_none());
        assert_eq!(handle.finish().unwrap(), Finish::Absent);
    }

    #[test]
    fn setting_a_workspace_up_again_ends_a_removal_that_was_left_part_way() {
        let t = repo(None);
        let ws = workspace(&t, "again");
        let receipt = receipt_of(&t, "again");
        receipt.save(&ws.git_dir).unwrap();
        let handle = Handle::new(&t.project.git_common_dir, receipt.instance_id);
        handle.begin(None).unwrap();
        // A write of the receipt alone does not change the state: a removal
        // that stops the daemons writes it.
        receipt.save(&ws.git_dir).unwrap();
        assert_eq!(handle.read().unwrap().unwrap().state, State::Removing);
        mark_active(&ws.git_dir, receipt.instance_id).unwrap();
        assert_eq!(handle.read().unwrap().unwrap().state, State::Active);
    }

    #[test]
    fn daemons_found_running_stay_listed_until_a_listing_shows_them_stopped() {
        let t = repo(None);
        let ws = workspace(&t, "doubt");
        let mut receipt = receipt_of(&t, "doubt");
        receipt.processes = Some(ProcessRecord {
            started_at: Utc::now(),
            ids: vec!["ns/web".into()],
        });
        receipt.save(&ws.git_dir).unwrap();
        let handle = Handle::new(&t.project.git_common_dir, receipt.instance_id);

        // A removal that could not stop them saw one more running that the
        // receipt never listed.
        handle
            .mark_unverified(&["ns/web".into(), "ns/by-hand".into()])
            .unwrap();
        let record = handle.read().unwrap().unwrap();
        assert!(record.resources.namespace_unverified);
        assert_eq!(record.resources.daemons, ["ns/web", "ns/by-hand"]);

        // The receipt stops listing its processes (a `down` that was not seen
        // through): the record does not forget what it was in doubt about.
        receipt.processes = None;
        receipt.save(&ws.git_dir).unwrap();
        let record = handle.read().unwrap().unwrap();
        assert!(record.resources.namespace_unverified);
        assert_eq!(record.resources.daemons, ["ns/web", "ns/by-hand"]);

        // Only a listing that was read lets go of them, and of the doubt.
        handle.release_daemons().unwrap();
        receipt.save(&ws.git_dir).unwrap();
        let record = handle.read().unwrap().unwrap();
        assert!(!record.resources.namespace_unverified);
        assert!(record.resources.daemons.is_empty());
    }

    #[test]
    fn disowned_data_stays_off_the_record_until_the_workspace_is_set_up_again() {
        let t = repo(None);
        let ws = workspace(&t, "keep");
        let mut receipt = receipt_of(&t, "keep");
        let id = Uuid::from_u128(31);
        receipt.services.insert(
            "cache".into(),
            crate::receipt::ServiceBinding::container(
                Engine::Valkey,
                id,
                "redis://x".into(),
                None,
                "empty".into(),
                crate::receipt::CloneReport {
                    method: None,
                    captured_at: None,
                    allocated_bytes: None,
                    source_downtime_ms: None,
                },
                None,
            ),
        );
        receipt.save(&ws.git_dir).unwrap();
        let handle = Handle::new(&t.project.git_common_dir, receipt.instance_id);
        assert_eq!(handle.read().unwrap().unwrap().resources.branches.len(), 1);

        handle.disown_branches(&[id]).unwrap();
        // The receipt still says the branch is owned; the record does not take
        // it back, however often the receipt is written.
        receipt.save(&ws.git_dir).unwrap();
        let record = handle.read().unwrap().unwrap();
        assert!(record.resources.branches.is_empty());
        assert_eq!(record.resources.disowned, [id]);
        assert!(
            record.resources.is_empty(),
            "kept data holds nothing to account for"
        );

        // Setting the workspace up again is changing one's mind.
        mark_active(&ws.git_dir, receipt.instance_id).unwrap();
        receipt.save(&ws.git_dir).unwrap();
        let record = handle.read().unwrap().unwrap();
        assert!(record.resources.disowned.is_empty());
        assert_eq!(record.resources.branches.len(), 1);
    }

    #[test]
    fn a_removal_asked_for_a_generation_that_changed_is_refused() {
        let t = repo(None);
        let ws = workspace(&t, "gen");
        let mut receipt = receipt_of(&t, "gen");
        receipt.save(&ws.git_dir).unwrap();
        let handle = Handle::new(&t.project.git_common_dir, receipt.instance_id);
        let shown = handle.read().unwrap().unwrap().generation();
        receipt.creating = Some(PendingBranch {
            service: "cache".into(),
            branch_id: Uuid::from_u128(5),
            engine: None,
        });
        receipt.save(&ws.git_dir).unwrap();
        let error = handle.begin(Some(&shown)).unwrap_err();
        assert!(
            format!("{error:#}").contains("changed since it was shown"),
            "{error:#}"
        );
        assert_eq!(handle.read().unwrap().unwrap().state, State::Active);
        let now = handle.read().unwrap().unwrap().generation();
        assert!(handle.begin(Some(&now)).is_ok());
    }

    #[test]
    fn two_removals_of_one_instance_cannot_hold_the_removal_lock_at_once() {
        let t = repo(None);
        let instance = Uuid::new_v4();
        let first = try_removal_lock(&t.project.git_common_dir, instance).unwrap();
        assert!(first.is_some());
        assert!(
            try_removal_lock(&t.project.git_common_dir, instance)
                .unwrap()
                .is_none()
        );
        drop(first);
        assert!(
            try_removal_lock(&t.project.git_common_dir, instance)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn the_journal_is_bounded() {
        let t = repo(None);
        let mut record = sample(&t, Uuid::new_v4());
        for n in 0..(JOURNAL_LIMIT + 20) {
            record.push_journal(
                Effect::Snapshot {
                    service: None,
                    branch: Uuid::from_u128(n as u128),
                    name: "x".into(),
                },
                Outcome::Done,
                None,
            );
        }
        assert_eq!(record.journal.len(), JOURNAL_LIMIT);
        assert!(
            matches!(&record.journal[0].effect, Effect::Snapshot { branch, .. } if *branch == Uuid::from_u128(20))
        );
    }
}

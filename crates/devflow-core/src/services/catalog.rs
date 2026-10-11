//! The catalog: the authority on which containers and volumes belong to a
//! project's container services.
//!
//! One JSON file per project in the data dir (`services/<project id>.json`,
//! written 0600 because it holds each branch's password), versioned, replaced
//! atomically and changed only under an exclusive lock, so two processes (a
//! terminal, the desktop app, a clone coordinator) never lose each other's
//! writes. It is bound to the Docker daemon whose resources it records:
//! another daemon's resources are not this project's, so a catalog opened on a
//! different daemon is refused instead of adopted.
//!
//! Ownership comes from here. A name is never a claim (`devflow-…` containers
//! may belong to anyone), and Docker labels only corroborate a record before
//! devflow touches the resource.
//!
//! The catalog also holds the operation journal. An operation is recorded
//! (ids, destination names, the port, the credentials, whether a full copy is
//! allowed) before its first effect, and each step is recorded before it is
//! taken; [`recovery`] says what an unfinished operation needs after a crash.
use crate::config::Engine;
use crate::fsutil;
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use uuid::Uuid;

/// Format version of the catalog file. A newer file is refused.
pub const VERSION: u32 = 1;

/// How many finished operations stay in the journal for diagnosis.
const KEEP_FINISHED: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub version: u32,
    pub project_id: Uuid,
    /// `docker info` ID of the daemon every resource in this file lives on.
    pub docker: String,
    #[serde(default)]
    pub services: BTreeMap<String, ServiceCatalog>,
}

/// One service key's records. The engine is fixed when the entry is created:
/// a key that changes engine is refused, because a Redis dump is not a Valkey
/// dump in general.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceCatalog {
    pub engine: Engine,
    #[serde(default)]
    pub branches: Vec<BranchRecord>,
    #[serde(default)]
    pub snapshots: Vec<SnapshotRecord>,
    /// The highest version ever given to each snapshot name. It outlives the
    /// snapshots: a deleted version is never given again to other data.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub snapshot_versions: BTreeMap<String, u32>,
    #[serde(default)]
    pub journal: Vec<Op>,
    /// Helper containers whose creation was asked for and not yet proven
    /// resolved. It outlives the operations: see [`HelperAttempt`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub helpers: Vec<HelperAttempt>,
    /// Set (with the time) by `forget` before it reads the records it removes:
    /// while it is there no planner records a new branch, snapshot or reset, so
    /// nothing can appear that `forget` has not seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forgetting: Option<DateTime<Utc>>,
}

/// A planner was refused because the service is being forgotten.
#[derive(Debug)]
pub struct Forgetting {
    pub key: String,
}

impl std::fmt::Display for Forgetting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "service '{key}' is being forgotten, so nothing new is recorded for it; wait for \
             `devflow service {key} forget` to finish, or run it again if it was interrupted",
            key = self.key
        )
    }
}

impl std::error::Error for Forgetting {}

/// `forget` found records or operations it had not handled when it came to
/// delete the service's entry: nothing was deleted.
#[derive(Debug)]
pub struct ForgetRefused {
    pub key: String,
    pub found: Vec<String>,
}

impl std::fmt::Display for ForgetRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "service '{key}' was not forgotten: it holds what `forget` did not handle ({}); \
             nothing was deleted. Settle it (`devflow service {key} reconcile`) and run `forget` \
             again",
            self.found.join(", "),
            key = self.key
        )
    }
}

impl std::error::Error for ForgetRefused {}

/// What a `forget` saw and dealt with: the ids of the records it read, once it
/// held the service.
#[derive(Debug, Clone, Default)]
pub struct Handled {
    pub branches: Vec<Uuid>,
    pub snapshots: Vec<Uuid>,
    /// Helper attempts whose tombstone `forget` removed (last of its effects):
    /// the only attempts that may go with the entry.
    pub fenced: Vec<FencedAttempt>,
}

/// An attempt and the tombstone `forget` removed for it. The attempt may go
/// with the entry only while the catalog still names that very container: a
/// tombstone made again under the same attempt name (a sweep that raced the
/// removal) is another container, which nothing has removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FencedAttempt {
    pub name: String,
    pub tombstone: String,
}

/// One attempt to create a helper container (the copy helper, the helper that
/// reads a volume), recorded before Docker is asked for it and kept until it
/// is resolved.
///
/// A `docker create` client that is ended (killed with its process, run past a
/// bound) says nothing about the request: the daemon keeps carrying it out and
/// the container can appear long after whoever asked has moved on, mounting a
/// volume and pinning it against removal. So the name of every attempt (unique
/// to it) is persisted here, apart from the operation, and every cleanup of
/// the service looks for it.
///
/// An attempt is resolved when the container it made is removed and confirmed
/// gone. One whose outcome is unknown (no id came back, the container is not
/// there) is never resolved by waiting: elapsed time proves nothing about a
/// request the daemon may still hold. It is **fenced** instead, by creating a
/// tombstone container under the attempt's exact name; a create of that name
/// that arrives later fails with a name conflict. The attempt stays listed,
/// with the tombstone's id, until the service is forgotten.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelperAttempt {
    /// The container's name, chosen for this attempt only.
    pub name: String,
    /// The operation whose helper label the container carries.
    pub op: Uuid,
    pub registered_at: DateTime<Utc>,
    /// The container's id, once `docker create` returned it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The id of the tombstone container that holds the attempt's name, once a
    /// cleanup fenced an attempt whose outcome was unknown. It is removed only
    /// by `forget`, after everything else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tombstone: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordState {
    /// Being created: not usable, and rolled back if the operation dies.
    Creating,
    Ready,
    /// Being removed: finished by whoever finds it.
    Deleting,
    /// A creation that did not complete. Its resources are removed; the
    /// record stays for diagnosis and is never promoted.
    Aborted,
}

/// How a clone was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloneMethod {
    /// `cp --reflink=always`: the blocks are shared with the source.
    Reflink,
    /// A full copy, allowed by the decision journaled before the clone.
    Copy,
}

impl std::fmt::Display for CloneMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CloneMethod::Reflink => "reflink",
            CloneMethod::Copy => "copy",
        })
    }
}

/// What a branch or snapshot was made from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceRef {
    Empty,
    Branch { id: Uuid },
    Snapshot { id: Uuid },
}

impl SourceRef {
    pub fn id(&self) -> Option<Uuid> {
        match self {
            SourceRef::Empty => None,
            SourceRef::Branch { id } | SourceRef::Snapshot { id } => Some(*id),
        }
    }
}

impl std::fmt::Display for SourceRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceRef::Empty => f.write_str("empty"),
            SourceRef::Branch { id } => write!(f, "branch:{id}"),
            SourceRef::Snapshot { id } => write!(f, "snapshot:{id}"),
        }
    }
}

/// The schema and account that live in a MySQL data directory. Clones
/// inherit them from their source, because they are part of the data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MysqlIdentity {
    pub database: String,
    pub user: String,
}

/// One branch: a container, its volume, its endpoint and its credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchRecord {
    pub id: Uuid,
    /// Display name (the workspace); never an identity.
    pub name: String,
    /// The workspace instance (receipt instance id) that may reset and drop
    /// it; `None` for a branch made by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<Uuid>,
    pub state: RecordState,
    /// Name the container has or will have, chosen before it exists.
    pub container_name: String,
    /// Full container id, recorded as soon as `docker create` returns it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
    pub volume: String,
    /// The image as configured when the branch was made (display).
    pub image: String,
    /// The digest-pinned reference containers are created from.
    pub pin: String,
    /// `requirepass` of this branch's server (Valkey, Redis), or the password
    /// of its application account (MySQL); no password is inherited.
    pub password: String,
    /// MySQL: the password of `root@localhost`, which only exists inside the
    /// container. Generated per branch, kept here (0600) and never printed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_password: Option<String>,
    /// MySQL: the database and application account the data has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mysql: Option<MysqlIdentity>,
    /// The host port on 127.0.0.1, allocated once and kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The candidate being tried; written before Docker is asked to publish it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_port: Option<u16>,
    /// OrbStack DNS name of the container, when the engine is OrbStack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    pub source: SourceRef,
    /// When the data was captured: the fork of the `BGSAVE` the clone copied
    /// (inherited from the snapshot when cloned from one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<DateTime<Utc>>,
    /// False when the server saved again while the file was copied, so the
    /// clone may be newer than `captured_at`.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub capture_exact: bool,
    /// The capture is the source's last save, cloned without asking for a new
    /// one because nothing had changed since (`captured_at` is then the
    /// server's `rdb_last_save_time`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub capture_last_save: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone: Option<CloneMethod>,
    /// Estimated blocks the clone took in the Docker VM (guest `df` delta).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocated_bytes: Option<i64>,
    /// Milliseconds the source was unavailable while this branch was cloned
    /// from it (MySQL: stopped cleanly, copied, started again).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downtime_ms: Option<u64>,
    /// An ordinary stop (`devflow service down`, no operation) that was asked
    /// for and not confirmed: journaled before it is issued, cleared when its
    /// client returns success. Left behind, the daemon may still be carrying
    /// it out, and the next start of the branch fences the container it names
    /// (see `stop.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopRequest>,
    /// Number of the volume and container pair, counted from 1; a reset moves
    /// the branch to the next.
    pub generation: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn yes() -> bool {
    true
}

/// How long a MySQL source may take to stop cleanly before it is killed:
/// generous, because a slow shutdown purges and merges everything.
pub const DEFAULT_STOP_TIMEOUT_SECS: u64 = 300;

fn default_stop_timeout() -> u64 {
    DEFAULT_STOP_TIMEOUT_SECS
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// A snapshot: an immutable volume holding one `dump.rdb`, no container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRecord {
    pub id: Uuid,
    pub name: String,
    pub version: u32,
    pub state: RecordState,
    pub volume: String,
    /// The branch it was taken from.
    pub source: SourceRef,
    pub image: String,
    pub pin: String,
    /// MySQL: the database and application account the data has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mysql: Option<MysqlIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<DateTime<Utc>>,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub capture_exact: bool,
    /// The capture is the source's last save, cloned without asking for a new
    /// one because nothing had changed since (`captured_at` is then the
    /// server's `rdb_last_save_time`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub capture_last_save: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone: Option<CloneMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocated_bytes: Option<i64>,
    /// Milliseconds the source was unavailable while it was snapshotted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downtime_ms: Option<u64>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    /// A branch with an empty dataset.
    CreateRoot,
    /// A branch cloned from a branch or snapshot.
    Clone,
    Snapshot,
    /// A branch's data replaced from its source.
    Reset,
    Drop,
    DeleteSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Planned,
    /// The source's save is being requested and awaited.
    Quiescing,
    Quiesced,
    /// The copy helper exists (recorded) and may be running.
    Copying,
    Copied,
    /// A source that was stopped for the copy (MySQL) is being started again
    /// and checked. Engines whose source keeps serving skip it.
    Restoring,
    /// The destination container is being created, started and checked.
    Starting,
    /// Reset: the replacement volume is started on its own and checked.
    Staging,
    /// Reset: the branch is moving from the old container to the new one.
    Cutover,
    /// Reset: the new pair is live; the old one is being removed.
    Cleanup,
    /// Drop: the resources are being removed.
    Removing,
    Done,
    /// The operation is being undone: recorded before anything is removed, so
    /// a rollback that is interrupted is finished by the next recovery and a
    /// forward completion is never attempted on what it half removed.
    RollingBack,
    Aborted,
}

impl Step {
    pub fn is_terminal(self) -> bool {
        matches!(self, Step::Done | Step::Aborted)
    }
}

impl OpKind {
    /// The steps of an operation in the order it takes them.
    pub fn sequence(self) -> &'static [Step] {
        use Step::*;
        match self {
            OpKind::CreateRoot => &[Planned, Starting, Done],
            OpKind::Clone => &[
                Planned, Quiescing, Quiesced, Copying, Copied, Restoring, Starting, Done,
            ],
            OpKind::Snapshot => &[
                Planned, Quiescing, Quiesced, Copying, Copied, Restoring, Done,
            ],
            OpKind::Reset => &[
                Planned, Quiescing, Quiesced, Copying, Copied, Restoring, Staging, Cutover,
                Cleanup, Done,
            ],
            OpKind::Drop | OpKind::DeleteSnapshot => &[Planned, Removing, Done],
        }
    }
}

/// What an operation found unfinished needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// Nothing: the operation ended.
    None,
    /// Undo it: kill the helper, remove what it made, never promote the
    /// destination.
    Abort,
    /// Finish it: what was done is enough evidence that it was meant to end.
    Forward,
}

/// The recovery an operation of `kind` stopped at `step` needs. Before the
/// copy is known complete (`Copied`) a clone is rolled back, because nothing
/// proves the destination holds the whole file; after it, the start is
/// repeatable and the clone is finished. A reset changes nothing the user
/// sees until its cutover is committed (the step moves to `Cleanup` in the
/// same write), so before that it is rolled back and after it finished. A
/// removal was asked for, so it is always finished.
pub fn recovery(kind: OpKind, step: Step) -> Recovery {
    use Step::*;
    if step.is_terminal() {
        return Recovery::None;
    }
    if step == RollingBack {
        return Recovery::Abort;
    }
    match (kind, step) {
        (OpKind::CreateRoot, _) => Recovery::Abort,
        (OpKind::Clone, Copied | Starting) => Recovery::Forward,
        (OpKind::Clone, _) => Recovery::Abort,
        (OpKind::Snapshot, Copied) => Recovery::Forward,
        (OpKind::Snapshot, _) => Recovery::Abort,
        (OpKind::Reset, Cleanup) => Recovery::Forward,
        (OpKind::Reset, _) => Recovery::Abort,
        (OpKind::Drop | OpKind::DeleteSnapshot, _) => Recovery::Forward,
    }
}

/// Whether an operation of `kind` at `step` can start rolling back. Every
/// creation can, from any step (a failure after the copy is rolled back, not
/// finished); a reset only until its cutover is committed; a removal never
/// (it was asked for).
pub fn can_roll_back(kind: OpKind, step: Step) -> bool {
    if step.is_terminal() || step == Step::RollingBack {
        return false;
    }
    match kind {
        OpKind::Drop | OpKind::DeleteSnapshot => false,
        OpKind::Reset => step != Step::Cleanup,
        OpKind::CreateRoot | OpKind::Clone | OpKind::Snapshot => true,
    }
}

/// Whether `from` → `to` is a legal move for an operation of `kind`: forward
/// along its sequence, to `RollingBack` from a step that can be undone, or to
/// `Aborted` from a step whose recovery is a rollback.
pub fn can_move(kind: OpKind, from: Step, to: Step) -> bool {
    if from.is_terminal() {
        return false;
    }
    if to == Step::RollingBack {
        return can_roll_back(kind, from);
    }
    if to == Step::Aborted {
        return recovery(kind, from) == Recovery::Abort;
    }
    let sequence = kind.sequence();
    match (
        sequence.iter().position(|s| *s == from),
        sequence.iter().position(|s| *s == to),
    ) {
        (Some(a), Some(b)) => b > a,
        _ => false,
    }
}

/// An operation in the journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Op {
    pub id: Uuid,
    pub kind: OpKind,
    pub step: Step,
    /// The branch or snapshot the operation creates, replaces or removes.
    pub target: Uuid,
    /// Where the data comes from (clone, snapshot, reset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceRef>,
    /// A full copy is allowed if the filesystem cannot share blocks. Decided
    /// and recorded before the source is quiesced.
    pub copy_allowed: bool,
    /// Seconds the copy may take before the helper is killed.
    pub copy_timeout_secs: u64,
    /// Whether the source container was running when the operation began.
    /// Recorded before the source is stopped (MySQL), so recovery knows
    /// whether a source it finds stopped is one the operation stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_running: Option<bool>,
    /// Seconds the source may take to stop before `docker stop` kills it,
    /// which aborts the operation (MySQL). Decided and recorded at planning.
    #[serde(default = "default_stop_timeout")]
    pub stop_timeout_secs: u64,
    /// Milliseconds the source was unavailable: from just before its stop
    /// to its first answer after the restart (MySQL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downtime_ms: Option<u64>,
    /// The throwaway bootstrap container of the destination (MySQL): no
    /// network, a name chosen before it exists, an id recorded before it
    /// starts, so recovery can remove it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    /// Name the copy helper will have; chosen before it exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper_name: Option<String>,
    /// The helper's container id, recorded before it is started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper_id: Option<String>,
    /// The short-lived helper a recovery uses to read a volume (`*-verify-*`):
    /// its name is recorded before Docker creates it and its id right after,
    /// so a recovery that is killed there leaves a helper that the next one
    /// finds and removes (it would pin the volume against removal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_id: Option<String>,
    /// The `docker stop` the operation asked for, recorded before it was
    /// issued: a client of the daemon that can outlive the coordinator, so
    /// whatever restarts the container first fences it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopRequest>,
    /// The process that runs the operation, recorded when it took its locks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<Coordinator>,
    /// Reset: the replacement and what it replaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset: Option<ResetPlan>,
    /// Size in bytes of the dump the copy produced, recorded when the copy
    /// completed: what a completion after a crash must find in the volume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copied_size: Option<u64>,
    /// The step the operation was at when it began rolling back; a rollback
    /// that resumes still knows what had been done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_from: Option<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A `docker stop` an operation asked for, journaled before it is issued.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopRequest {
    /// Full id of the container being stopped.
    pub container: String,
    /// The container's `StartedAt` (the daemon's clock) when the stop was
    /// asked for: a different one later means it was started again since.
    pub run_started_at: String,
    /// When (this machine's clock) it was asked for, and with what grace.
    pub requested_at: DateTime<Utc>,
    pub timeout_secs: u64,
    /// The process that asked.
    pub coordinator_pid: u32,
    /// `None` until the client returned (so also for a coordinator that died
    /// waiting for it); `Some(true)` when it succeeded, `Some(false)` when it
    /// failed or ran past its bound, which does not say the daemon is done.
    #[serde(default)]
    pub outcome: Option<bool>,
    /// Recovery's decision to stop trusting the container this request names:
    /// recorded before anything is done to it (see `stop.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fence: Option<Fence>,
}

/// The fence recovery puts around a stop it cannot prove finished. Docker
/// applies a stop only to the container id it was issued for, so a late stop
/// is harmless once that container is gone: recovery removes it by its
/// recorded id and starts a new one in its place. Each step is journaled
/// first, so an interrupted fence is finished by the next run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fence {
    /// When recovery decided to fence (before it stopped or removed anything).
    pub decided_at: DateTime<Utc>,
    /// The container that took the old one's place, once it runs and answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Coordinator {
    pub pid: u32,
    pub started_at: DateTime<Utc>,
}

/// A reset's names, chosen before anything exists, and the pair it retires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetPlan {
    /// The replacement volume (and the staging container's name base).
    pub volume: String,
    /// The final container's name on the replacement volume.
    pub container_name: String,
    /// The throwaway container that proves the replacement starts.
    pub staging_name: String,
    /// The image the replacement runs: its source's, so the data is read by
    /// the version that wrote it.
    pub image: String,
    pub pin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staging_id: Option<String>,
    /// The final container, once created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_id: Option<String>,
    /// The pair being replaced (recorded at the cutover).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_container_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_volume: Option<String>,
    /// Whether the old container was running before the cutover stopped it.
    #[serde(default)]
    pub old_was_running: bool,
    /// When the replacement's data was captured, and how it was cloned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<DateTime<Utc>>,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub capture_exact: bool,
    /// The capture is the source's last save, cloned without asking for a new
    /// one because nothing had changed since (`captured_at` is then the
    /// server's `rdb_last_save_time`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub capture_last_save: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone: Option<CloneMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocated_bytes: Option<i64>,
    /// Milliseconds the source was unavailable while the replacement was
    /// copied from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downtime_ms: Option<u64>,
}

impl Op {
    pub fn new(kind: OpKind, target: Uuid) -> Op {
        let now = Utc::now();
        Op {
            id: Uuid::new_v4(),
            kind,
            step: Step::Planned,
            target,
            source: None,
            copy_allowed: false,
            copy_timeout_secs: 600,
            source_running: None,
            stop_timeout_secs: default_stop_timeout(),
            downtime_ms: None,
            boot_name: None,
            boot_id: None,
            helper_name: None,
            helper_id: None,
            verify_name: None,
            verify_id: None,
            stop: None,
            coordinator: None,
            reset: None,
            copied_size: None,
            rollback_from: None,
            error: None,
            started_at: now,
            updated_at: now,
        }
    }

    /// Move to `to`, refusing an illegal move.
    pub fn advance(&mut self, to: Step) -> Result<()> {
        ensure!(
            can_move(self.kind, self.step, to),
            "illegal journal move {:?} -> {:?} for {:?}",
            self.step,
            to,
            self.kind
        );
        if to == Step::RollingBack {
            self.rollback_from = Some(self.step);
        }
        self.step = to;
        self.updated_at = Utc::now();
        Ok(())
    }

    /// The step the operation had reached before it began rolling back (its
    /// current step when it has not).
    pub fn reached(&self) -> Step {
        self.rollback_from.unwrap_or(self.step)
    }

    /// Every resource id the operation touches, for locking.
    pub fn resources(&self) -> Vec<Uuid> {
        let mut ids = vec![self.target];
        ids.extend(self.source.and_then(|s| s.id()));
        ids.sort();
        ids.dedup();
        ids
    }
}

impl ServiceCatalog {
    pub fn new(engine: Engine) -> Self {
        ServiceCatalog {
            engine,
            branches: Vec::new(),
            snapshots: Vec::new(),
            snapshot_versions: BTreeMap::new(),
            journal: Vec::new(),
            helpers: Vec::new(),
            forgetting: None,
        }
    }

    /// Everything this entry holds that `handled` does not account for: a
    /// record that is not in it or that was not removed (an aborted one is
    /// kept for diagnosis and goes with the entry), an operation that has not
    /// ended, a helper attempt that is not resolved.
    pub fn unhandled(&self, handled: &Handled) -> Vec<String> {
        let mut found = Vec::new();
        for branch in &self.branches {
            if !handled.branches.contains(&branch.id) {
                found.push(format!("branch {} recorded meanwhile", branch.id));
            } else if branch.state != RecordState::Aborted {
                found.push(format!("branch {} not removed", branch.id));
            }
        }
        for snapshot in &self.snapshots {
            if !handled.snapshots.contains(&snapshot.id) {
                found.push(format!("snapshot {} recorded meanwhile", snapshot.id));
            } else if snapshot.state != RecordState::Aborted {
                found.push(format!("snapshot {} not removed", snapshot.id));
            }
        }
        for op in self.unfinished() {
            found.push(format!("{:?} of {} not finished", op.kind, op.target));
        }
        for attempt in &self.helpers {
            let removed = handled.fenced.iter().any(|f| {
                f.name == attempt.name && attempt.tombstone.as_deref() == Some(f.tombstone.as_str())
            });
            if !removed {
                found.push(format!("helper {} not yet resolved", attempt.name));
            }
        }
        found
    }

    pub fn branch(&self, id: Uuid) -> Option<&BranchRecord> {
        self.branches.iter().find(|b| b.id == id)
    }

    pub fn branch_mut(&mut self, id: Uuid) -> Option<&mut BranchRecord> {
        self.branches.iter_mut().find(|b| b.id == id)
    }

    pub fn snapshot(&self, id: Uuid) -> Option<&SnapshotRecord> {
        self.snapshots.iter().find(|s| s.id == id)
    }

    pub fn snapshot_mut(&mut self, id: Uuid) -> Option<&mut SnapshotRecord> {
        self.snapshots.iter_mut().find(|s| s.id == id)
    }

    pub fn op(&self, id: Uuid) -> Option<&Op> {
        self.journal.iter().find(|o| o.id == id)
    }

    pub fn op_mut(&mut self, id: Uuid) -> Option<&mut Op> {
        self.journal.iter_mut().find(|o| o.id == id)
    }

    /// A ready snapshot by name; the latest version when `version` is `None`.
    pub fn find_snapshot(&self, name: &str, version: Option<u32>) -> Option<&SnapshotRecord> {
        self.snapshots
            .iter()
            .filter(|s| s.name == name && s.state == RecordState::Ready)
            .filter(|s| version.is_none_or(|v| s.version == v))
            .max_by_key(|s| s.version)
    }

    /// Give the next version number of snapshot `name`. Versions are never
    /// reused: the number comes from the counter kept per name, which deleting
    /// a snapshot does not lower (and from the surviving records, for a
    /// catalog written before the counter existed). Called inside the catalog
    /// update that records the snapshot, so two snapshots never share one.
    pub fn allocate_version(&mut self, name: &str) -> u32 {
        let surviving = self
            .snapshots
            .iter()
            .filter(|s| s.name == name)
            .map(|s| s.version)
            .max()
            .unwrap_or(0);
        let issued = self.snapshot_versions.get(name).copied().unwrap_or(0);
        let next = surviving.max(issued) + 1;
        self.snapshot_versions.insert(name.to_string(), next);
        next
    }

    /// Operations that have not ended, oldest first.
    pub fn unfinished(&self) -> Vec<&Op> {
        self.journal
            .iter()
            .filter(|op| !op.step.is_terminal())
            .collect()
    }

    /// Unfinished operations that touch any of `ids`.
    pub fn unfinished_touching(&self, ids: &[Uuid]) -> Vec<&Op> {
        self.unfinished()
            .into_iter()
            .filter(|op| op.resources().iter().any(|r| ids.contains(r)))
            .collect()
    }

    /// Drop the oldest finished operations and aborted records beyond the
    /// history kept.
    pub fn prune(&mut self) {
        let finished: Vec<Uuid> = self
            .journal
            .iter()
            .filter(|op| op.step.is_terminal())
            .map(|op| op.id)
            .collect();
        if finished.len() > KEEP_FINISHED {
            let drop: Vec<Uuid> = finished[..finished.len() - KEEP_FINISHED].to_vec();
            self.journal.retain(|op| !drop.contains(&op.id));
        }
        let live: Vec<Uuid> = self.journal.iter().map(|op| op.target).collect();
        self.branches
            .retain(|b| b.state != RecordState::Aborted || live.contains(&b.id));
        self.snapshots
            .retain(|s| s.state != RecordState::Aborted || live.contains(&s.id));
    }
}

impl Catalog {
    fn fresh(project_id: Uuid, docker: &str) -> Catalog {
        Catalog {
            version: VERSION,
            project_id,
            docker: docker.to_string(),
            services: BTreeMap::new(),
        }
    }

    /// The entry of service `key`, created for `engine` on first use. An
    /// existing entry of another engine is refused.
    pub fn service_mut(&mut self, key: &str, engine: Engine) -> Result<&mut ServiceCatalog> {
        let entry = self
            .services
            .entry(key.to_string())
            .or_insert_with(|| ServiceCatalog::new(engine));
        ensure!(
            entry.engine == engine,
            "service '{key}' was created as {} and is now configured as {engine}: their data is \
             not interchangeable. Use another key for the new engine (the old data stays \
             recorded; `devflow service {key} forget` removes it)",
            entry.engine
        );
        Ok(entry)
    }
}

/// The catalog file of a project and its lock.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
    lock: PathBuf,
    project: Uuid,
    docker: String,
}

impl Store {
    /// `<data dir>/services/<project id>.json`
    pub fn path_for(project: Uuid) -> Result<PathBuf> {
        Ok(crate::paths::data_dir()?
            .join("services")
            .join(format!("{project}.json")))
    }

    /// The store of `project` on the daemon `docker`.
    pub fn open(project: Uuid, docker: &str) -> Result<Store> {
        let path = Self::path_for(project)?;
        let lock = path.with_extension("lock");
        Ok(Store {
            path,
            lock,
            project,
            docker: docker.to_string(),
        })
    }

    /// A store whose files live in `dir`, for tests of what sits on top.
    #[cfg(test)]
    pub(crate) fn in_dir(dir: &std::path::Path, project: Uuid, docker: &str) -> Store {
        Store {
            path: dir.join(format!("{project}.json")),
            lock: dir.join(format!("{project}.lock")),
            project,
            docker: docker.to_string(),
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn docker(&self) -> &str {
        &self.docker
    }

    /// The directory the project's resource locks live in.
    pub fn lock_dir(&self) -> PathBuf {
        self.path.with_extension("locks")
    }

    /// The catalog as it is on disk, or an empty one bound to this daemon.
    /// Writers replace the file atomically, so no lock is needed to read.
    pub fn load(&self) -> Result<Catalog> {
        match read_file(&self.path)? {
            Some(catalog) => self.check(catalog),
            None => Ok(Catalog::fresh(self.project, &self.docker)),
        }
    }

    fn check(&self, catalog: Catalog) -> Result<Catalog> {
        ensure!(
            catalog.project_id == self.project,
            "{} belongs to project {}, not {}",
            self.path.display(),
            catalog.project_id,
            self.project
        );
        ensure!(
            catalog.docker == self.docker,
            "the container services of this project were created on Docker daemon {} and the \
             current one is {}: the recorded containers and volumes are not on this engine. \
             Switch back (`docker context use …`, DOCKER_CONTEXT or DOCKER_HOST), or remove {} \
             to start over with new containers",
            catalog.docker,
            self.docker,
            self.path.display()
        );
        Ok(catalog)
    }

    /// Mark service `key` as being forgotten (or clear the mark), in one locked
    /// write. A service with no entry has nothing to mark.
    pub fn mark_forgetting(&self, key: &str, on: bool) -> Result<()> {
        self.update(|catalog| {
            if let Some(entry) = catalog.services.get_mut(key) {
                entry.forgetting = on.then(Utc::now);
            }
            Ok(())
        })
    }

    /// Delete service `key`'s entry, but only if, read again now under the
    /// catalog's lock, it holds nothing `handled` does not account for. A
    /// planner that recorded something after `forget` read its records is
    /// found here: the error is a [`ForgetRefused`] and nothing is deleted.
    pub fn forget_service(&self, key: &str, handled: &Handled) -> Result<()> {
        self.update(|catalog| {
            let Some(entry) = catalog.services.get(key) else {
                return Ok(());
            };
            let found = entry.unhandled(handled);
            if !found.is_empty() {
                return Err(ForgetRefused {
                    key: key.to_string(),
                    found,
                }
                .into());
            }
            catalog.services.remove(key);
            Ok(())
        })
    }

    /// Run `check` while holding the catalog's lock, so no writer is half way
    /// through recording a change; nothing is written.
    pub fn with_lock<T>(&self, check: impl FnOnce() -> Result<T>) -> Result<T> {
        fsutil::with_lock(&self.lock, check)
    }

    /// Change the catalog under its lock. `change` sees the current content
    /// and its result is written atomically; an error writes nothing.
    pub fn update<T>(&self, change: impl FnOnce(&mut Catalog) -> Result<T>) -> Result<T> {
        fsutil::with_lock(&self.lock, || {
            let mut catalog = self.load()?;
            let out = change(&mut catalog)?;
            catalog.version = VERSION;
            for service in catalog.services.values_mut() {
                service.prune();
            }
            let bytes = serde_json::to_vec_pretty(&catalog)?;
            fsutil::write_atomic(&self.path, &bytes, true)?;
            Ok(out)
        })
    }
}

/// Read a catalog file without checking which daemon it belongs to (for
/// `forget`, `doctor` and listings that only describe it).
pub fn read_file(path: &std::path::Path) -> Result<Option<Catalog>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let catalog: Catalog = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is not a valid devflow service catalog", path.display()))?;
    if catalog.version > VERSION {
        bail!(
            "{} was written by a newer devflow (catalog version {}, this one reads {VERSION})",
            path.display(),
            catalog.version
        );
    }
    Ok(Some(catalog))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &std::path::Path, docker: &str) -> Store {
        let project = Uuid::from_u128(7);
        Store {
            path: dir.join(format!("{project}.json")),
            lock: dir.join(format!("{project}.lock")),
            project,
            docker: docker.to_string(),
        }
    }

    fn branch(id: u128) -> BranchRecord {
        let now = Utc::now();
        BranchRecord {
            id: Uuid::from_u128(id),
            name: "feature".into(),
            owner: None,
            state: RecordState::Ready,
            container_name: "c".into(),
            container_id: None,
            volume: "v".into(),
            image: "valkey/valkey:8-alpine".into(),
            pin: "valkey/valkey@sha256:abc".into(),
            password: "pw".into(),
            root_password: None,
            mysql: None,
            port: None,
            pending_port: None,
            domain: None,
            source: SourceRef::Empty,
            captured_at: None,
            capture_exact: true,
            capture_last_save: false,
            clone: None,
            allocated_bytes: None,
            downtime_ms: None,
            stop: None,
            generation: 1,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn a_missing_catalog_is_empty_and_bound_to_the_current_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), "daemon-a");
        let catalog = store.load().unwrap();
        assert_eq!(catalog.version, VERSION);
        assert_eq!(catalog.docker, "daemon-a");
        assert!(catalog.services.is_empty());
        assert!(!store.path().exists(), "reading writes nothing");
    }

    #[test]
    fn updates_are_atomic_private_and_all_or_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), "daemon-a");
        store
            .update(|c| {
                c.service_mut("cache", Engine::Valkey)?
                    .branches
                    .push(branch(1));
                Ok(())
            })
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the file holds passwords");
        }
        // A failing change writes nothing, even after it edited the catalog.
        let before = std::fs::read(store.path()).unwrap();
        let error = store
            .update(|c| {
                c.service_mut("cache", Engine::Valkey)?
                    .branches
                    .push(branch(2));
                bail!("crash")
            })
            .map(|_: ()| ())
            .unwrap_err();
        assert!(error.to_string().contains("crash"));
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        assert_eq!(
            store.load().unwrap().services["cache"].branches.len(),
            1,
            "the second branch never existed"
        );
        // No temporary file is left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn concurrent_updates_lose_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), "daemon-a");
        let handles: Vec<_> = (0..8u128)
            .map(|n| {
                let store = store.clone();
                std::thread::spawn(move || {
                    for k in 0..5u128 {
                        store
                            .update(|c| {
                                c.service_mut("cache", Engine::Valkey)?
                                    .branches
                                    .push(branch(n * 100 + k + 1));
                                Ok(())
                            })
                            .unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(store.load().unwrap().services["cache"].branches.len(), 40);
    }

    #[test]
    fn the_catalog_is_bound_to_its_daemon_project_and_engine() {
        let dir = tempfile::tempdir().unwrap();
        let a = store(dir.path(), "daemon-a");
        a.update(|c| c.service_mut("cache", Engine::Valkey).map(|_| ()))
            .unwrap();
        // Another daemon's resources are not this project's.
        let b = store(dir.path(), "daemon-b");
        let error = b.load().unwrap_err().to_string();
        assert!(
            error.contains("daemon-a") && error.contains("daemon-b"),
            "{error}"
        );
        assert!(b.update(|_| Ok(())).is_err());
        // The engine of a key does not change.
        let error = a
            .update(|c| c.service_mut("cache", Engine::Redis).map(|_| ()))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("valkey") && error.contains("redis"),
            "{error}"
        );
        // Another project's file is refused too.
        let other = Store {
            project: Uuid::from_u128(8),
            ..store(dir.path(), "daemon-a")
        };
        assert!(
            other
                .load()
                .unwrap_err()
                .to_string()
                .contains("belongs to project")
        );
    }

    #[test]
    fn newer_or_corrupt_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), "daemon-a");
        std::fs::write(store.path(), b"not json").unwrap();
        assert!(
            store
                .load()
                .unwrap_err()
                .to_string()
                .contains("not a valid")
        );
        let newer = serde_json::json!({
            "version": VERSION + 1,
            "project_id": Uuid::from_u128(7),
            "docker": "daemon-a",
        });
        std::fs::write(store.path(), newer.to_string()).unwrap();
        let error = store.load().unwrap_err().to_string();
        assert!(error.contains("newer devflow"), "{error}");
        assert!(store.update(|_| Ok(())).is_err(), "never rewritten");
    }

    #[test]
    fn the_journal_moves_forward_only() {
        for kind in [
            OpKind::CreateRoot,
            OpKind::Clone,
            OpKind::Snapshot,
            OpKind::Reset,
            OpKind::Drop,
            OpKind::DeleteSnapshot,
        ] {
            let sequence = kind.sequence();
            assert_eq!(sequence[0], Step::Planned);
            assert_eq!(*sequence.last().unwrap(), Step::Done);
            for (i, from) in sequence.iter().enumerate() {
                for (j, to) in sequence.iter().enumerate() {
                    assert_eq!(
                        can_move(kind, *from, *to),
                        j > i && !from.is_terminal(),
                        "{kind:?} {from:?} -> {to:?}"
                    );
                }
            }
        }
        // Nothing moves out of a terminal step; nothing moves to a step the
        // operation does not have.
        assert!(!can_move(OpKind::Clone, Step::Done, Step::Planned));
        assert!(!can_move(OpKind::Clone, Step::Aborted, Step::Done));
        assert!(!can_move(OpKind::Snapshot, Step::Planned, Step::Starting));
        assert!(!can_move(OpKind::Drop, Step::Planned, Step::Quiescing));
        let mut op = Op::new(OpKind::Clone, Uuid::from_u128(1));
        op.advance(Step::Quiescing).unwrap();
        assert!(op.advance(Step::Planned).is_err());
        assert!(op.advance(Step::Quiescing).is_err(), "no repeating a step");
    }

    #[test]
    fn recovery_rolls_back_before_the_copy_is_proven_and_forward_after() {
        use Recovery::*;
        use Step::*;
        for step in [Planned, Quiescing, Quiesced, Copying] {
            assert_eq!(recovery(OpKind::Clone, step), Abort, "{step:?}");
            assert_eq!(recovery(OpKind::Snapshot, step), Abort, "{step:?}");
        }
        for step in [Copied, Starting] {
            assert_eq!(recovery(OpKind::Clone, step), Forward, "{step:?}");
        }
        assert_eq!(recovery(OpKind::Snapshot, Copied), Forward);
        assert_eq!(recovery(OpKind::CreateRoot, Planned), Abort);
        assert_eq!(recovery(OpKind::CreateRoot, Starting), Abort);
        // A reset is rolled back until its cutover is committed.
        for step in [
            Planned, Quiescing, Quiesced, Copying, Copied, Staging, Cutover,
        ] {
            assert_eq!(recovery(OpKind::Reset, step), Abort, "{step:?}");
        }
        assert_eq!(recovery(OpKind::Reset, Cleanup), Forward);
        // Removals were asked for: always finished.
        for step in [Planned, Removing] {
            assert_eq!(recovery(OpKind::Drop, step), Forward);
            assert_eq!(recovery(OpKind::DeleteSnapshot, step), Forward);
        }
        // Ended operations need nothing, and cannot be aborted afterwards.
        for kind in [OpKind::Clone, OpKind::Reset, OpKind::Drop] {
            assert_eq!(recovery(kind, Done), None);
            assert_eq!(recovery(kind, Aborted), None);
        }
        assert!(can_move(OpKind::Clone, Planned, Aborted));
        assert!(can_move(OpKind::Reset, Cutover, Aborted));
        assert!(
            !can_move(OpKind::Reset, Cleanup, Aborted),
            "past the commit"
        );
        assert!(
            !can_move(OpKind::Clone, Copied, Aborted),
            "finished instead"
        );
        assert!(!can_move(OpKind::Drop, Removing, Aborted));
    }

    #[test]
    fn a_rollback_is_journaled_first_and_never_turns_into_a_completion() {
        use Step::*;
        // A creation can start rolling back from any step, also after its
        // copy was proven: that is how a failed start is undone.
        for kind in [OpKind::CreateRoot, OpKind::Clone, OpKind::Snapshot] {
            for step in kind.sequence() {
                assert_eq!(
                    can_move(kind, *step, RollingBack),
                    !step.is_terminal(),
                    "{kind:?} {step:?}"
                );
            }
        }
        // A reset until its cutover is committed; a removal never.
        assert!(can_move(OpKind::Reset, Cutover, RollingBack));
        assert!(!can_move(OpKind::Reset, Cleanup, RollingBack));
        assert!(!can_move(OpKind::Drop, Removing, RollingBack));
        assert!(!can_move(OpKind::DeleteSnapshot, Planned, RollingBack));
        // Once rolling back, recovery is a rollback whatever the kind and
        // however far the operation had got, and it ends only as aborted.
        for kind in [
            OpKind::CreateRoot,
            OpKind::Clone,
            OpKind::Snapshot,
            OpKind::Reset,
        ] {
            assert_eq!(recovery(kind, RollingBack), Recovery::Abort);
            assert!(can_move(kind, RollingBack, Aborted));
            assert!(!can_move(kind, RollingBack, Done));
            assert!(!can_move(kind, RollingBack, Starting));
            assert!(!can_move(kind, RollingBack, RollingBack));
        }
        // The step it had reached is kept, for the parts of a rollback that
        // depend on it, and the operation stays unfinished until it is aborted.
        let mut op = Op::new(OpKind::Clone, Uuid::from_u128(1));
        for step in [Quiescing, Quiesced, Copying, Copied, Starting] {
            op.advance(step).unwrap();
        }
        assert_eq!(op.reached(), Starting);
        op.advance(RollingBack).unwrap();
        assert_eq!((op.step, op.reached()), (RollingBack, Starting));
        let mut service = ServiceCatalog::new(Engine::Valkey);
        service.journal.push(op.clone());
        assert_eq!(service.unfinished().len(), 1);
        op.advance(Aborted).unwrap();
        assert!(op.step.is_terminal() && op.reached() == Starting);
    }

    #[test]
    fn snapshots_are_versioned_and_found_by_name() {
        let mut service = ServiceCatalog::new(Engine::Valkey);
        let snapshot = |version, state| SnapshotRecord {
            id: Uuid::new_v4(),
            name: "seed".into(),
            version,
            state,
            volume: format!("v{version}"),
            source: SourceRef::Empty,
            image: "i".into(),
            pin: "p".into(),
            mysql: None,
            captured_at: None,
            capture_exact: true,
            capture_last_save: false,
            clone: None,
            allocated_bytes: None,
            downtime_ms: None,
            created_at: Utc::now(),
        };
        // A catalog from before the counter existed: numbers continue after
        // the surviving records.
        service.snapshots.push(snapshot(1, RecordState::Ready));
        service.snapshots.push(snapshot(2, RecordState::Ready));
        service.snapshots.push(snapshot(3, RecordState::Creating));
        assert_eq!(service.allocate_version("seed"), 4, "after the records");
        assert_eq!(service.allocate_version("seed"), 5);
        assert_eq!(service.allocate_version("other"), 1, "per name");
        assert_eq!(service.find_snapshot("seed", None).unwrap().version, 2);
        assert_eq!(service.find_snapshot("seed", Some(1)).unwrap().version, 1);
        assert!(
            service.find_snapshot("seed", Some(3)).is_none(),
            "not ready"
        );
        assert!(service.find_snapshot("other", None).is_none());
    }

    #[test]
    fn a_deleted_version_is_never_given_again_and_concurrent_snapshots_get_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), "daemon-a");
        let snapshot = |name: &str, version| SnapshotRecord {
            id: Uuid::new_v4(),
            name: name.into(),
            version,
            state: RecordState::Ready,
            volume: format!("v-{name}-{version}"),
            source: SourceRef::Empty,
            image: "i".into(),
            pin: "p".into(),
            mysql: None,
            captured_at: None,
            capture_exact: true,
            capture_last_save: false,
            clone: None,
            allocated_bytes: None,
            downtime_ms: None,
            created_at: Utc::now(),
        };
        // Eight threads each record a snapshot of one name: the version is
        // taken in the same locked update that records it.
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                std::thread::spawn(move || {
                    store
                        .update(|c| {
                            let service = c.service_mut("cache", Engine::Valkey)?;
                            let version = service.allocate_version("seed");
                            service.snapshots.push(snapshot("seed", version));
                            Ok(version)
                        })
                        .unwrap()
                })
            })
            .collect();
        let mut versions: Vec<u32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        versions.sort_unstable();
        assert_eq!(versions, (1..=8).collect::<Vec<_>>());

        // Deleting the newest version, even the only one left, does not free
        // its number.
        let next = store
            .update(|c| {
                let service = c.service_mut("cache", Engine::Valkey)?;
                service.snapshots.clear();
                Ok(service.allocate_version("seed"))
            })
            .unwrap();
        assert_eq!(next, 9, "v1..v8 were given before and are not reused");
        // The counter is persisted: a fresh load sees it.
        let service = store.load().unwrap().services["cache"].clone();
        assert_eq!(service.snapshot_versions["seed"], 9);
    }

    #[test]
    fn finished_history_and_aborted_records_are_pruned() {
        let mut service = ServiceCatalog::new(Engine::Valkey);
        let mut aborted = branch(9);
        aborted.state = RecordState::Aborted;
        service.branches.push(aborted);
        service.branches.push(branch(10));
        for n in 0..(KEEP_FINISHED + 5) {
            let mut op = Op::new(OpKind::Drop, Uuid::from_u128(1000 + n as u128));
            op.step = Step::Done;
            service.journal.push(op);
        }
        let mut live = Op::new(OpKind::Clone, Uuid::from_u128(10));
        live.step = Step::Copying;
        service.journal.push(live);
        service.prune();
        assert_eq!(
            service
                .journal
                .iter()
                .filter(|o| o.step.is_terminal())
                .count(),
            KEEP_FINISHED
        );
        assert_eq!(
            service.unfinished().len(),
            1,
            "unfinished work is never pruned"
        );
        assert_eq!(service.branches.len(), 1, "the aborted record is dropped");
        assert_eq!(service.branches[0].id, Uuid::from_u128(10));
    }

    #[test]
    fn operations_name_every_resource_they_lock() {
        let mut op = Op::new(OpKind::Reset, Uuid::from_u128(5));
        op.source = Some(SourceRef::Snapshot {
            id: Uuid::from_u128(3),
        });
        assert_eq!(op.resources(), vec![Uuid::from_u128(3), Uuid::from_u128(5)]);
        op.source = Some(SourceRef::Empty);
        assert_eq!(op.resources(), vec![Uuid::from_u128(5)]);
        let mut service = ServiceCatalog::new(Engine::Valkey);
        service.journal.push(op);
        assert_eq!(service.unfinished_touching(&[Uuid::from_u128(5)]).len(), 1);
        assert!(
            service
                .unfinished_touching(&[Uuid::from_u128(6)])
                .is_empty()
        );
    }

    fn handled(branches: &[u128]) -> Handled {
        Handled {
            branches: branches.iter().map(|n| Uuid::from_u128(*n)).collect(),
            snapshots: Vec::new(),
            fenced: Vec::new(),
        }
    }

    #[test]
    fn forget_deletes_the_entry_only_when_it_holds_nothing_it_did_not_handle() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), "daemon-a");
        let mut aborted = branch(2);
        aborted.state = RecordState::Aborted;
        store
            .update(|c| {
                let service = c.service_mut("cache", Engine::Valkey)?;
                service.branches.push(aborted);
                Ok(())
            })
            .unwrap();
        // Nothing but what it saw (an aborted record goes with the entry).
        store.forget_service("cache", &handled(&[2])).unwrap();
        assert!(store.load().unwrap().services.is_empty());
        // A service with no entry has nothing to forget.
        store.forget_service("cache", &handled(&[])).unwrap();
    }

    #[test]
    fn a_branch_a_planner_journals_after_forget_read_its_records_is_not_erased() {
        let dir = tempfile::tempdir().unwrap();
        let other_dir = tempfile::tempdir().unwrap();
        let store2 = store(other_dir.path(), "daemon-a");
        let store = store(dir.path(), "daemon-a");
        store
            .update(|c| {
                c.service_mut("cache", Engine::Valkey)?
                    .branches
                    .push(branch(1));
                Ok(())
            })
            .unwrap();
        // `forget` read branch 1 and removed it; then a planner recorded a
        // new branch with an operation that has not ended.
        let seen = handled(&[1]);
        store
            .update(|c| {
                let service = c.service_mut("cache", Engine::Valkey)?;
                service.branches.clear();
                service.branches.push(branch(7));
                service
                    .journal
                    .push(Op::new(OpKind::CreateRoot, Uuid::from_u128(7)));
                Ok(())
            })
            .unwrap();
        let before = std::fs::read(store.path()).unwrap();
        let error = store.forget_service("cache", &seen).unwrap_err();
        let refused = error
            .downcast_ref::<ForgetRefused>()
            .expect("a typed error");
        assert!(
            refused
                .found
                .iter()
                .any(|f| f.contains("recorded meanwhile"))
                && refused.found.iter().any(|f| f.contains("not finished")),
            "{refused}"
        );
        assert_eq!(
            std::fs::read(store.path()).unwrap(),
            before,
            "nothing deleted"
        );
        assert_eq!(store.load().unwrap().services["cache"].branches.len(), 1);

        // Unfinished work of a branch it did see, and an unresolved helper,
        // are refused too.
        store2
            .update(|c| {
                let service = c.service_mut("cache", Engine::Valkey)?;
                service.helpers.push(HelperAttempt {
                    name: "h".into(),
                    op: Uuid::from_u128(1),
                    registered_at: Utc::now(),
                    id: None,
                    tombstone: None,
                });
                Ok(())
            })
            .unwrap();
        let error = store2.forget_service("cache", &handled(&[])).unwrap_err();
        assert!(error.to_string().contains("helper h not yet resolved"));
        // One whose tombstone forget removed goes with the entry, as long as
        // the catalog still names that container...
        let fence = |tombstone: &str| {
            let mut fenced = handled(&[]);
            fenced.fenced.push(FencedAttempt {
                name: "h".into(),
                tombstone: tombstone.into(),
            });
            fenced
        };
        store2
            .update(|c| {
                c.service_mut("cache", Engine::Valkey)?.helpers[0].tombstone = Some("t2".into());
                Ok(())
            })
            .unwrap();
        let accounted = |fenced: &Handled| {
            store2.load().unwrap().services["cache"]
                .unhandled(fenced)
                .iter()
                .all(|f| !f.contains("helper h"))
        };
        assert!(accounted(&fence("t2")));
        // ...and not when a tombstone made again under the same attempt name is
        // another container, which nothing removed.
        assert!(!accounted(&fence("t1")));
        let error = store2.forget_service("cache", &fence("t1")).unwrap_err();
        assert!(error.to_string().contains("helper h not yet resolved"));
        // An attempt with no tombstone was never handled.
        store2
            .update(|c| {
                c.service_mut("cache", Engine::Valkey)?.helpers[0].tombstone = None;
                Ok(())
            })
            .unwrap();
        assert!(!accounted(&fence("t1")));
    }

    #[test]
    fn a_service_marked_as_forgetting_refuses_planners_under_the_catalog_lock() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path(), "daemon-a");
        store
            .update(|c| c.service_mut("cache", Engine::Valkey).map(|_| ()))
            .unwrap();
        // Marking is persisted; the planner's write is a check-then-write under
        // the same lock, as `Service::update_planning` makes it.
        store.mark_forgetting("cache", true).unwrap();
        assert!(store.load().unwrap().services["cache"].forgetting.is_some());
        let plan = |store: &Store| {
            store.update(|c| {
                let service = c.service_mut("cache", Engine::Valkey)?;
                if service.forgetting.is_some() {
                    return Err(Forgetting {
                        key: "cache".into(),
                    }
                    .into());
                }
                service.branches.push(branch(3));
                Ok(())
            })
        };
        let before = std::fs::read(store.path()).unwrap();
        let error = plan(&store).unwrap_err();
        assert!(error.downcast_ref::<Forgetting>().is_some(), "{error}");
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        // Cleared (a forget that ended in an error), planning works again.
        store.mark_forgetting("cache", false).unwrap();
        plan(&store).unwrap();
        assert_eq!(store.load().unwrap().services["cache"].branches.len(), 1);
        // The marker survives the file like any other field.
        store.mark_forgetting("cache", true).unwrap();
        let again = store.load().unwrap();
        assert!(again.services["cache"].forgetting.is_some());
    }

    #[test]
    fn an_ordinary_stop_is_recorded_on_the_branch_and_a_record_without_one_still_reads() {
        let mut record = branch(1);
        let json = serde_json::to_value(&record).unwrap();
        assert!(json.get("stop").is_none(), "nothing is written when idle");
        assert!(
            serde_json::from_value::<BranchRecord>(json)
                .unwrap()
                .stop
                .is_none()
        );
        let requested_at = Utc::now();
        record.stop = Some(StopRequest {
            container: "abc".into(),
            run_started_at: "2026-10-11T10:00:00.1Z".into(),
            requested_at,
            timeout_secs: 30,
            coordinator_pid: 7,
            outcome: Some(false),
            fence: None,
        });
        let back: BranchRecord =
            serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
        let stop = back.stop.unwrap();
        assert_eq!(
            (stop.container.as_str(), stop.requested_at, stop.outcome),
            ("abc", requested_at, Some(false))
        );
    }

    #[test]
    fn a_stop_request_is_journaled_and_an_entry_without_one_still_reads() {
        let mut op = Op::new(OpKind::Clone, Uuid::from_u128(1));
        let json = serde_json::to_value(&op).unwrap();
        assert!(json.get("stop").is_none());
        assert!(serde_json::from_value::<Op>(json).unwrap().stop.is_none());
        op.stop = Some(StopRequest {
            container: "abc".into(),
            run_started_at: "2026-10-11T10:00:00.1Z".into(),
            requested_at: Utc::now(),
            timeout_secs: 300,
            coordinator_pid: 42,
            outcome: None,
            fence: None,
        });
        let back: Op = serde_json::from_str(&serde_json::to_string(&op).unwrap()).unwrap();
        let stop = back.stop.unwrap();
        assert_eq!((stop.coordinator_pid, stop.outcome), (42, None));
        // A fence is journaled with the request and survives the file; one
        // written before fences existed has none.
        assert!(stop.fence.is_none());
        let mut fenced = op.clone();
        fenced.stop.as_mut().unwrap().fence = Some(Fence {
            decided_at: Utc::now(),
            replacement: Some("def".into()),
        });
        let again: Op = serde_json::from_str(&serde_json::to_string(&fenced).unwrap()).unwrap();
        assert_eq!(
            again.stop.unwrap().fence.unwrap().replacement.as_deref(),
            Some("def")
        );
        // A request written before its outcome was noted has none.
        let mut value = serde_json::to_value(&op).unwrap();
        value["stop"].as_object_mut().unwrap().remove("outcome");
        assert_eq!(
            serde_json::from_value::<Op>(value)
                .unwrap()
                .stop
                .unwrap()
                .outcome,
            None
        );
    }

    #[test]
    fn a_journal_entry_without_the_verify_helper_fields_still_reads() {
        let mut op = Op::new(OpKind::Snapshot, Uuid::from_u128(1));
        let json = serde_json::to_value(&op).unwrap();
        assert!(json.get("verify_name").is_none() && json.get("verify_id").is_none());
        let back: Op = serde_json::from_value(json).unwrap();
        assert!(back.verify_name.is_none() && back.verify_id.is_none());
        // And what is journaled for the helper survives the file.
        op.verify_name = Some("devflow-x-cache-verify-1".into());
        op.verify_id = Some("abc".into());
        let back: Op = serde_json::from_str(&serde_json::to_string(&op).unwrap()).unwrap();
        assert_eq!(
            back.verify_name.as_deref(),
            Some("devflow-x-cache-verify-1")
        );
        assert_eq!(back.verify_id.as_deref(), Some("abc"));
    }
}

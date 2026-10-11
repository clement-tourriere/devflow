//! A workspace's processes: pitchfork daemons, approved and namespaced.
//!
//! devflow does not supervise anything; pitchfork does. This module decides
//! *whether* devflow may ask pitchfork to start or stop a workspace's daemons,
//! and reports what pitchfork says afterwards.
//!
//! - **Namespace.** Each worktree gets a machine-local `pitchfork.local.toml`
//!   holding only `namespace = "df-<project8>-<instance8>"` and
//!   `worktree_label`, so two checkouts (or two repositories) never share
//!   daemon ids. No secret is ever written to a pitchfork file; the
//!   environment reaches daemons through mise (`mise = true` and
//!   `[env] _.file = ".devflow/env"`), see [`env_wiring`].
//! - **Start** only for a `ready` workspace whose pitchfork configuration is
//!   the approved one (the per-workspace trust functions) and, when starting
//!   everything, only if no daemon starts on its own (`cron`, `watch`,
//!   `boot_start`, `auto`). A start has a deadline; afterwards the real state
//!   is read back from pitchfork instead of assumed.
//! - **Explicit ids.** devflow starts and stops by full `namespace/name` id,
//!   never with `--local`: pitchfork's `--local` selects the daemons of every
//!   configuration from the working directory up to the root. What a started
//!   daemon depends on must be a daemon of the workspace's own files too.
//! - **Stop** runs `pitchfork stop` only while the configuration still matches
//!   the approval: pitchfork runs lifecycle hooks from the current file.
//! - Process state is reported separately from workspace readiness: a failed
//!   start never makes a workspace not ready.
//! - **One operation at a time per workspace.** A start, stop, restart,
//!   removal and the replacement of the instance identity (`setup
//!   --new-instance`) each hold the workspace's *process lock* (a file lock in
//!   its Git directory, next to the receipt lock; see [`process_lock`]) from
//!   the record of intent through the effect to the verification, so a stop
//!   cannot read "nothing runs" between a start's record and its effect and
//!   erase the record. The order is process lock, then receipt lock. A second
//!   operation waits [`PROCESS_LOCK_WAIT`] and then fails with
//!   [`crate::error::Busy`].
use crate::error::{Busy, InstanceChanged};
use crate::pitchfork::{self, Daemon, DaemonStatus, Detection, Pitchfork, ProxyStatus};
use crate::pitchfork_scope::{Identity, Item, ProcessesScope};
use crate::project::Project;
use crate::receipt::{ProcessLock, ProcessRecord, Receipt, Status};
use crate::trust::{self, Approval, Scope, Trigger, TrustState};
use crate::worktree::Workspace;
use crate::{fsutil, setup};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long an operation on a workspace's daemons waits for another one to
/// finish before it gives up with [`Busy`]. Long enough to serialise an
/// operation that is just ending, short enough that a start waiting for
/// pitchfork (up to a minute) makes the next caller fail instead of hang.
pub const PROCESS_LOCK_WAIT: Duration = Duration::from_secs(2);

/// The process lock of `workspace`: the proof, for everything below that
/// records, starts, stops or replaces, that no other devflow operation is doing
/// any of that to the same workspace. Waits [`PROCESS_LOCK_WAIT`], then fails
/// with [`Busy`] (`downcast_ref::<Busy>()`).
pub fn process_lock(workspace: &Workspace) -> Result<ProcessLock> {
    anyhow::ensure!(
        workspace.git_dir.is_dir(),
        "the Git directory of workspace '{}' is missing: {}",
        workspace.name,
        workspace.git_dir.display()
    );
    Receipt::process_lock_waiting(&workspace.git_dir, PROCESS_LOCK_WAIT)?
        .ok_or_else(|| anyhow::Error::new(Busy::processes(&workspace.name)))
}

/// The machine-local file devflow writes into a worktree.
pub const LOCAL_FILE: &str = "pitchfork.local.toml";
/// How long `devflow up` waits for pitchfork by default.
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a stop may take.
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// Files whose daemons make a project "use pitchfork".
const COMMITTED: [&str; 2] = ["pitchfork.toml", ".config/pitchfork.toml"];
/// Files whose top-level `namespace` would disagree with ours (pitchfork
/// refuses siblings that declare different ones).
const NAMESPACE_SIBLINGS: [&str; 3] = [
    "pitchfork.toml",
    ".config/pitchfork.toml",
    ".config/pitchfork.local.toml",
];
const MAX_READ: u64 = 1 << 20;

const MARKER: &str = "# Written by devflow: this worktree's pitchfork namespace and label.\n\
# Machine-local (excluded from Git through the repository's info/exclude) and rewritten by\n\
# devflow when the workspace changes; put your own settings in another file.\n";

// ---------------------------------------------------------------------------
// Namespace

/// Where a worktree's pitchfork namespace comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum NamespaceSource {
    /// devflow's own `pitchfork.local.toml`.
    Devflow,
    /// A file someone wrote declares it.
    Declared { file: String },
    /// Nothing declares one: pitchfork uses the directory name.
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Namespace {
    pub value: String,
    #[serde(flatten)]
    pub source: NamespaceSource,
}

/// What [`ensure_namespace`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum NamespaceOutcome {
    /// The worktree has no `pitchfork.toml`: nothing to namespace.
    NotUsed,
    /// The file was created or updated.
    Written {
        namespace: String,
        label: String,
    },
    /// The file already says it.
    Current {
        namespace: String,
    },
    /// A committed pitchfork file declares its own `namespace`, and pitchfork
    /// errors on siblings that disagree: devflow wrote nothing.
    Conflict {
        file: String,
        declared: String,
    },
    /// `pitchfork.local.toml` exists and was not written by devflow: left alone.
    HandWritten {
        reason: String,
    },
    Failed {
        error: String,
    },
}

impl NamespaceOutcome {
    /// What the person should hear about, if anything.
    pub fn warning(&self) -> Option<String> {
        match self {
            NamespaceOutcome::Conflict { file, declared } => Some(format!(
                "{file} declares namespace \"{declared}\", so devflow did not write a \
                 per-worktree namespace: pitchfork refuses sibling files that disagree. Remove \
                 it from {file} (daemon ids of every worktree would otherwise share \
                 \"{declared}\")"
            )),
            NamespaceOutcome::HandWritten { reason } => Some(format!(
                "{LOCAL_FILE} {reason}; devflow left it alone, so this worktree keeps \
                 pitchfork's default namespace unless that file sets one"
            )),
            NamespaceOutcome::Failed { error } => {
                Some(format!("could not write {LOCAL_FILE}: {error}"))
            }
            _ => None,
        }
    }
}

/// Whether any daemon of the worktree at `dir` declares a `port` (so the proxy
/// can give it a URL).
pub fn has_ports(dir: &Path) -> bool {
    ProcessesScope::read(dir).layers.iter().any(|layer| {
        layer
            .daemons
            .iter()
            .any(|daemon| daemon.fields.contains_key("port"))
    })
}

/// Whether the worktree at `dir` has a committed pitchfork configuration.
pub fn uses_pitchfork(dir: &Path) -> bool {
    COMMITTED.iter().any(|file| dir.join(file).is_file())
}

fn slug_for(receipt: &Receipt) -> String {
    receipt.slug()
}

fn label_of(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string())
}

/// The exact text devflow writes.
fn render(namespace: &str, label: &str) -> String {
    format!(
        "{MARKER}namespace = {}\nworktree_label = {}\n",
        toml::Value::String(namespace.to_string()),
        toml::Value::String(label.to_string()),
    )
}

/// Whether `text` is something devflow wrote: the marker, and then exactly
/// the two keys it writes with a namespace of its shape. Anything else (an
/// added key, an edit) is somebody's file.
fn devflow_written(text: &str) -> Option<(String, String)> {
    let text = text.replace("\r\n", "\n");
    let rest = text.strip_prefix(MARKER)?;
    let table: toml::Table = rest.parse().ok()?;
    if table.len() != 2 {
        return None;
    }
    let namespace = table.get("namespace")?.as_str()?.to_string();
    let label = table.get("worktree_label")?.as_str()?.to_string();
    let shaped = namespace.strip_prefix("df-").is_some_and(|tail| {
        let mut parts = tail.split('-');
        let hex = |part: Option<&str>| {
            part.is_some_and(|p| p.len() == 8 && p.chars().all(|c| c.is_ascii_hexdigit()))
        };
        hex(parts.next()) && hex(parts.next()) && parts.next().is_none()
    });
    (shaped && render(&namespace, &label) == text).then_some((namespace, label))
}

/// Read a small regular file, `None` when it does not exist.
fn read_small(path: &Path) -> Result<Option<String>, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    if !meta.is_file() {
        return Err("is not a regular file (a link or a directory)".to_string());
    }
    if meta.len() > MAX_READ {
        return Err("is larger than 1 MiB".to_string());
    }
    std::fs::read_to_string(path)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// A top-level `namespace` declared by a committed (or `.config`) pitchfork
/// file, as `(file, namespace)`.
fn declared_namespace(dir: &Path) -> Option<(String, String)> {
    NAMESPACE_SIBLINGS.iter().find_map(|file| {
        let text = read_small(&dir.join(file)).ok()??;
        let table: toml::Table = text.parse().ok()?;
        let namespace = table.get("namespace")?.as_str()?.to_string();
        Some((file.to_string(), namespace))
    })
}

/// What is wrong with the namespace of the worktree at `dir`, without
/// changing anything: a committed `namespace` that disagrees with devflow's, or
/// a `pitchfork.local.toml` devflow did not write. `None` when it is fine or
/// the project does not use pitchfork.
pub fn namespace_problem(dir: &Path) -> Option<String> {
    if !uses_pitchfork(dir) {
        return None;
    }
    if let Some((file, declared)) = declared_namespace(dir) {
        return NamespaceOutcome::Conflict { file, declared }.warning();
    }
    match read_small(&dir.join(LOCAL_FILE)) {
        Ok(Some(text)) if devflow_written(&text).is_none() => NamespaceOutcome::HandWritten {
            reason: "was not written by devflow".to_string(),
        }
        .warning(),
        Err(reason) => NamespaceOutcome::HandWritten { reason }.warning(),
        _ => None,
    }
}

/// The namespace pitchfork gives the worktree at `dir`.
pub fn namespace_of(dir: &Path) -> Namespace {
    if let Ok(Some(text)) = read_small(&dir.join(LOCAL_FILE)) {
        if let Some((value, _)) = devflow_written(&text) {
            return Namespace {
                value,
                source: NamespaceSource::Devflow,
            };
        }
        if let Some(value) = text
            .parse::<toml::Table>()
            .ok()
            .and_then(|t| t.get("namespace")?.as_str().map(str::to_string))
        {
            return Namespace {
                value,
                source: NamespaceSource::Declared {
                    file: LOCAL_FILE.to_string(),
                },
            };
        }
    }
    if let Some((file, value)) = declared_namespace(dir) {
        return Namespace {
            value,
            source: NamespaceSource::Declared { file },
        };
    }
    Namespace {
        value: label_of(dir),
        source: NamespaceSource::Default,
    }
}

/// The namespace and label devflow generated for the workspace at `dir`: the
/// receipt's `df-<project8>-<instance8>` and the directory name. These are the
/// values it writes into `pitchfork.local.toml`, and the only ones the trust
/// scope leaves out. `None` when `dir` holds no readable receipt of this
/// project (nothing was generated, so nothing is exempt).
pub fn generated_identity(project: &Project, dir: &Path) -> Option<Identity> {
    let git_dir = crate::worktree::git_dir_of(dir)?;
    let receipt = Receipt::load(&git_dir).ok().flatten()?;
    (receipt.project_id == project.id()).then(|| Identity {
        namespace: receipt.slug(),
        label: label_of(&dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf())),
    })
}

/// Whether [`ensure_namespace`] would put the right namespace in place: the
/// file is absent or devflow's own (whatever it says), and no committed file
/// declares another. A start repairs these before anything else; a stop never
/// writes.
fn namespace_repairable(dir: &Path) -> bool {
    uses_pitchfork(dir)
        && declared_namespace(dir).is_none()
        && match read_small(&dir.join(LOCAL_FILE)) {
            Ok(None) => true,
            Ok(Some(text)) => devflow_written(&text).is_some(),
            Err(_) => false,
        }
}

/// The namespace devflow assigned to `workspace`, which is what its daemons'
/// ids carry and the only one a daemon is attributed through. Taken from the
/// receipt, never from a pitchfork file. A [`Refusal::NotReady`] when there is
/// no receipt (devflow never set the workspace up) or it cannot be read.
fn assigned_namespace(project: &Project, workspace: &Workspace) -> Result<String, Refusal> {
    match setup::receipt(project, workspace) {
        Ok(Some(receipt)) => Ok(receipt.slug()),
        Ok(None) => Err(Refusal::NotReady {
            status: "not_set_up".to_string(),
            detail: None,
        }),
        Err(e) => Err(Refusal::NotReady {
            status: "needs_attention".to_string(),
            detail: Some(format!("{e:#}")),
        }),
    }
}

/// The namespace assigned to `workspace`, when the namespace its pitchfork
/// files give it is that one; otherwise a [`Refusal::ForeignNamespace`]. A
/// file edited to name another workspace's namespace is the case this exists
/// for. With `repairable_ok`, a namespace that a start would write (the file
/// is missing, or is devflow's own and stale) is not held against the
/// workspace: used to show what could be done, never right before doing it.
fn own_namespace(
    project: &Project,
    workspace: &Workspace,
    repairable_ok: bool,
) -> Result<String, Refusal> {
    let expected = assigned_namespace(project, workspace)?;
    let found = namespace_of(&workspace.path);
    if found.value == expected || (repairable_ok && namespace_repairable(&workspace.path)) {
        return Ok(expected);
    }
    Err(Refusal::ForeignNamespace {
        expected,
        file: match found.source {
            NamespaceSource::Devflow => Some(LOCAL_FILE.to_string()),
            NamespaceSource::Declared { file } => Some(file),
            NamespaceSource::Default => None,
        },
        found: found.value,
    })
}

/// Add `pitchfork.local.toml` to the repository's `info/exclude`, once.
/// Returns whether the file changed. `info/exclude` is shared by every
/// worktree of the repository.
pub fn ensure_excluded(git_common_dir: &Path) -> Result<bool> {
    const PATTERN: &str = "/pitchfork.local.toml";
    let path = git_common_dir.join("info").join("exclude");
    let current = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let present = current
        .lines()
        .map(str::trim)
        .any(|line| line == PATTERN || line == LOCAL_FILE);
    if present {
        return Ok(false);
    }
    let mut next = current;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str("# devflow: machine-local pitchfork namespace of each worktree\n");
    next.push_str(PATTERN);
    next.push('\n');
    fsutil::write_atomic(&path, next.as_bytes(), false)?;
    Ok(true)
}

/// Write the worktree's namespace file when the project uses pitchfork.
///
/// Only the two identity keys, never a secret, never `[env]`. A committed
/// `namespace`, or a `pitchfork.local.toml` that is not devflow's (marker and
/// content must match), is reported and left untouched. Failures are reported
/// in the outcome, not raised: the workspace does not depend on this file.
pub fn ensure_namespace(
    project: &Project,
    workspace: &Workspace,
    receipt: &Receipt,
) -> NamespaceOutcome {
    let dir = &workspace.path;
    if !uses_pitchfork(dir) {
        return NamespaceOutcome::NotUsed;
    }
    if let Some((file, declared)) = declared_namespace(dir) {
        return NamespaceOutcome::Conflict { file, declared };
    }
    let namespace = slug_for(receipt);
    let label = label_of(dir);
    let wanted = render(&namespace, &label);
    let target = dir.join(LOCAL_FILE);
    let existing = match read_small(&target) {
        Ok(existing) => existing,
        Err(reason) => return NamespaceOutcome::HandWritten { reason },
    };
    if let Some(text) = &existing {
        if text.replace("\r\n", "\n") == wanted {
            // Written earlier: make sure the exclusion is there too.
            return match ensure_excluded(&project.git_common_dir) {
                Ok(_) => NamespaceOutcome::Current { namespace },
                Err(e) => NamespaceOutcome::Failed {
                    error: format!("{e:#}"),
                },
            };
        }
        if devflow_written(text).is_none() {
            return NamespaceOutcome::HandWritten {
                reason: "was not written by devflow".to_string(),
            };
        }
    }
    // Excluded first: a crash never leaves an unignored file behind.
    if let Err(e) = ensure_excluded(&project.git_common_dir) {
        return NamespaceOutcome::Failed {
            error: format!("{e:#}"),
        };
    }
    match fsutil::write_atomic(&target, wanted.as_bytes(), false) {
        Ok(()) => NamespaceOutcome::Written { namespace, label },
        Err(e) => NamespaceOutcome::Failed {
            error: format!("{e:#}"),
        },
    }
}

// ---------------------------------------------------------------------------
// Environment delivery

/// The daemons that do not get `.devflow/env`, and whether mise loads it.
/// devflow never puts environment or secrets into pitchfork's files (pitchfork
/// persists daemon environments in its state file); the one channel is mise.
/// Empty when everything is wired or there is nothing to deliver.
pub fn env_wiring(project: &Project, dir: &Path) -> Vec<String> {
    let processes = ProcessesScope::read(dir);
    let names: BTreeSet<&str> = processes
        .layers
        .iter()
        .flat_map(|layer| layer.daemons.iter().map(|daemon| daemon.id.as_str()))
        .collect();
    if names.is_empty() {
        return Vec::new();
    }
    // Only when there is something to deliver: a database, or `[env]` recipes.
    let delivers = project.config.postgres().is_some() || !project.config.env.is_empty();
    if !delivers {
        return Vec::new();
    }
    let mut problems = Vec::new();
    // `run = "devflow exec -- CMD"` delivers the file by itself; the others
    // depend on mise loading it.
    let through_mise: BTreeSet<&str> = names
        .iter()
        .copied()
        .filter(|name| !runs_devflow_exec(&processes, name))
        .collect();
    let without: BTreeSet<&str> = through_mise
        .iter()
        .copied()
        .filter(|name| !runs_under_mise(&processes, name))
        .collect();
    if !without.is_empty() {
        problems.push(format!(
            "daemon(s) {} neither run through `devflow exec` nor set `mise = true`, so \
             .devflow/env (the workspace's database URL and variables) never reaches them; \
             write their `run` as `devflow exec -- CMD`",
            without.iter().copied().collect::<Vec<_>>().join(", ")
        ));
    }
    if through_mise.len() > without.len() && !mise_loads_env(dir) {
        problems.push(
            "mise does not load .devflow/env here: add `[env]` with `_.file = \".devflow/env\"` \
             to mise.toml (setting the project up proposes it)"
                .to_string(),
        );
    }
    problems
}

/// Whether a mise configuration in `dir` loads this workspace's
/// `.devflow/env` (`[env] _.file`).
fn mise_loads_env(dir: &Path) -> bool {
    const FILES: [&str; 5] = [
        "mise.toml",
        ".mise.toml",
        ".config/mise.toml",
        "mise.local.toml",
        ".mise.local.toml",
    ];
    FILES.iter().any(|file| {
        let Ok(Some(text)) = read_small(&dir.join(file)) else {
            return false;
        };
        let Ok(table) = text.parse::<toml::Table>() else {
            return false;
        };
        let Some(files) = table
            .get("env")
            .and_then(|env| env.get("_"))
            .and_then(|under| under.get("file"))
        else {
            return false;
        };
        let mut paths = Vec::new();
        collect_paths(files, 0, &mut paths);
        paths
            .iter()
            .any(|path| crate::detect::is_devflow_env_path(path))
    })
}

/// File paths in a mise `_.file` value: a string, a table with `path`, or an
/// array of either.
fn collect_paths(value: &toml::Value, depth: usize, out: &mut Vec<String>) {
    if depth > 3 {
        return;
    }
    match value {
        toml::Value::String(s) => out.push(s.clone()),
        toml::Value::Array(items) => items.iter().for_each(|v| collect_paths(v, depth + 1, out)),
        toml::Value::Table(t) => {
            if let Some(path) = t.get("path") {
                collect_paths(path, depth + 1, out);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Gating

/// What is being asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Start,
    Stop,
}

/// Why devflow will not start or stop a workspace's daemons. A typed error:
/// frontends match on it (`error.downcast_ref::<Refusal>()`) to explain and to
/// grey out buttons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Refusal {
    NotInstalled {
        detail: String,
    },
    NothingConfigured,
    Unreadable {
        file: String,
        problem: String,
    },
    NotReady {
        /// `not_set_up`, `needs_attention` or `in_progress`.
        status: String,
        detail: Option<String>,
    },
    NotApproved {
        action: Action,
        /// An approval exists, but not for this content.
        changed: bool,
        /// That approval was made under an earlier scope version.
        format_changed: bool,
    },
    AutonomousTriggers {
        triggers: Vec<Trigger>,
    },
    UnknownDaemon {
        name: String,
    },
    /// pitchfork starts a daemon's dependencies by itself. `daemon` depends on
    /// `dependency`, which is not one of this workspace's own daemons (another
    /// namespace, or a name this workspace's files do not declare), so starting
    /// `daemon` would start something outside the workspace.
    ExternalDependency {
        daemon: String,
        dependency: String,
    },
    /// The namespace this workspace's pitchfork files give its daemons is not
    /// the one devflow assigned to it (`df-<project8>-<instance8>`, from the
    /// receipt). The namespace decides which daemons pitchfork attributes to the
    /// workspace and which a stop reaches, so a file that names another
    /// workspace's would let this one show and stop that one's daemons. devflow
    /// attributes and acts on nothing through a namespace it did not assign.
    ForeignNamespace {
        /// What devflow assigned.
        expected: String,
        /// What the files say, or pitchfork's default (the directory name).
        found: String,
        /// The file that declares `found`; `None` when none does.
        file: Option<String>,
    },
    /// What pitchfork runs for this workspace could not be read (the listing
    /// failed, or pitchfork lacks a feature devflow needs to ask), so devflow
    /// cannot say that nothing runs. Unlike the refusals above, `--force`
    /// answers it: the person accepts not knowing.
    StateUnknown {
        detail: String,
        /// What to do about it, when the generic advice would mislead
        /// (a supervisor that is down needs starting, not forcing first).
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },
    /// Daemons with `mise = true` get the workspace's variables only through
    /// mise, and mise does not hand them `.devflow/env` as devflow wrote it.
    MiseDelivery {
        daemons: Vec<String>,
        delivery: crate::env_file::MiseDelivery,
    },
    /// `setup --new-instance` would replace the identity the workspace's
    /// daemons run under while pitchfork lists them as active: they would keep
    /// running in a namespace nothing of the workspace names any more.
    DaemonsRunning {
        /// The `namespace/name` ids that are active.
        ids: Vec<String>,
    },
}

impl Refusal {
    /// A stable word for JSON and tests.
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::NotInstalled { .. } => "not_installed",
            Refusal::NothingConfigured => "nothing_configured",
            Refusal::Unreadable { .. } => "unreadable",
            Refusal::NotReady { .. } => "not_ready",
            Refusal::NotApproved { .. } => "not_approved",
            Refusal::AutonomousTriggers { .. } => "autonomous_triggers",
            Refusal::UnknownDaemon { .. } => "unknown_daemon",
            Refusal::ExternalDependency { .. } => "external_dependency",
            Refusal::ForeignNamespace { .. } => "foreign_namespace",
            Refusal::StateUnknown { .. } => "state_unknown",
            Refusal::MiseDelivery { .. } => "mise_delivery",
            Refusal::DaemonsRunning { .. } => "daemons_running",
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NotInstalled { detail } => write!(f, "pitchfork is not available: {detail}"),
            Refusal::NothingConfigured => f.write_str(
                "this workspace has no pitchfork configuration (no pitchfork.toml with daemons)",
            ),
            Refusal::Unreadable { file, problem } => write!(
                f,
                "{file} {problem}, so devflow cannot tell what it would run"
            ),
            Refusal::NotReady { status, detail } => {
                let what = match status.as_str() {
                    "not_set_up" => "has not been set up by devflow",
                    "in_progress" => "is still being set up",
                    _ => "needs attention",
                };
                write!(
                    f,
                    "the workspace {what}; set it up before starting processes{}",
                    detail
                        .as_deref()
                        .map(|d| format!(" ({d})"))
                        .unwrap_or_default()
                )
            }
            Refusal::NotApproved {
                action,
                changed,
                format_changed,
            } => {
                let why = if *format_changed {
                    "was approved under an older format that covered less"
                } else if *changed {
                    "changed since it was approved"
                } else {
                    "has not been approved"
                };
                match action {
                    Action::Start => write!(
                        f,
                        "what this workspace asks to run (hooks, [env], pitchfork processes) \
                         {why}; review and approve it first"
                    ),
                    Action::Stop => write!(
                        f,
                        "what this workspace asks to run (hooks, [env], pitchfork processes) {why}, \
                         and pitchfork runs a daemon's lifecycle hooks (on_stop, …) from the \
                         current file when it stops it; review it first, or stop the daemons \
                         yourself"
                    ),
                }
            }
            Refusal::AutonomousTriggers { triggers } => {
                let list: Vec<String> = triggers
                    .iter()
                    .map(|t| format!("{} ({}, in {})", t.daemon, t.trigger, t.file))
                    .collect();
                write!(
                    f,
                    "pitchfork starts {} on its own (cron, watch, boot_start or auto), so devflow \
                     does not start everything at once; start the plain daemons by name \
                     or run pitchfork yourself",
                    list.join(", ")
                )
            }
            Refusal::UnknownDaemon { name } => {
                write!(f, "this workspace's configuration has no daemon \"{name}\"")
            }
            Refusal::ExternalDependency { daemon, dependency } => write!(
                f,
                "daemon \"{daemon}\" depends on \"{dependency}\", which is not a daemon of this \
                 workspace's own pitchfork files; pitchfork would start it from outside the \
                 workspace, so devflow will not start \"{daemon}\", nor everything at once; the \
                 daemons that do not depend on it can be started by name"
            ),
            Refusal::ForeignNamespace {
                expected,
                found,
                file,
            } => write!(
                f,
                "the pitchfork namespace does not belong to this workspace: its files give \
                 \"{found}\"{}, devflow assigned \"{expected}\"; devflow does not attribute or \
                 act on daemons through a namespace it did not assign",
                match file {
                    Some(file) => format!(" (in {file})"),
                    None => " (pitchfork's default, no file declares one)".to_string(),
                }
            ),
            Refusal::StateUnknown { detail, .. } => write!(
                f,
                "devflow cannot tell what pitchfork runs for this workspace ({detail}), so it \
                 will not assume that nothing does"
            ),
            Refusal::MiseDelivery { daemons, delivery } => write!(
                f,
                "{} get the workspace's variables through mise, and {}",
                daemons.join(", "),
                delivery.describe()
            ),
            Refusal::DaemonsRunning { ids } => write!(
                f,
                "this workspace's pitchfork daemons ({}) are running under the identity a new                  instance would replace; they would keep running in a namespace devflow no longer                  owns and could not stop",
                ids.join(", ")
            ),
        }
    }
}

impl std::error::Error for Refusal {}

impl Refusal {
    /// What the person can do about it.
    pub fn hint(&self) -> Option<String> {
        Some(match self {
            Refusal::NotInstalled { .. } => "install pitchfork".to_string(),
            Refusal::NothingConfigured => return None,
            Refusal::Unreadable { file, .. } => format!("fix {file}, then try again"),
            Refusal::NotReady { .. } => "set the workspace up first".to_string(),
            Refusal::NotApproved {
                action: Action::Start,
                ..
            } => "review and approve what the workspace asks to run".to_string(),
            Refusal::NotApproved {
                action: Action::Stop,
                ..
            } => {
                "review and approve it, or stop the daemons anyway by forcing the stop".to_string()
            }
            Refusal::AutonomousTriggers { .. } => "start the plain daemons by name".to_string(),
            Refusal::UnknownDaemon { .. } => {
                "pick one of the daemons the workspace's pitchfork files declare".to_string()
            }
            Refusal::ExternalDependency { .. } => {
                "declare the dependency in this workspace's pitchfork.toml (a plain name, or \
                 this workspace's namespace), or remove it"
                    .to_string()
            }
            Refusal::ForeignNamespace { .. } => format!(
                "set the workspace up again to restore devflow's {LOCAL_FILE}, and remove any \
                 other `namespace` from the pitchfork files"
            ),
            Refusal::StateUnknown {
                hint: Some(hint), ..
            } => hint.clone(),
            Refusal::StateUnknown { hint: None, .. } => {
                "check pitchfork's state; forcing goes on without knowing and stops the daemons \
                 the workspace declares"
                    .to_string()
            }
            Refusal::MiseDelivery { .. } => {
                "check the doctor results for the workspace, fix the mise configuration, then \
                 start again"
                    .to_string()
            }
            Refusal::DaemonsRunning { .. } => {
                "stop the workspace's processes first, then set it up as a new instance; for a \
                 workspace copied from another machine, give the copy's identity with \
                 DEVFLOW_INSTANCE_ID instead"
                    .to_string()
            }
        })
    }
}

impl Refusal {
    /// What can be done about it, as actions, for the workspace at
    /// `workspace`: install pitchfork, set the workspace up, review and
    /// approve what it asks to run.
    pub fn remedies(&self, workspace: &Path) -> Vec<crate::remedy::Remedy> {
        use crate::remedy::Remedy;
        match self {
            Refusal::NotInstalled { .. } => vec![Remedy::InstallPitchfork],
            Refusal::NotReady { .. } | Refusal::ForeignNamespace { .. } => vec![Remedy::Setup {
                workspace: workspace.to_path_buf(),
            }],
            Refusal::NotApproved { .. } => vec![Remedy::Trust {
                workspace: workspace.to_path_buf(),
            }],
            _ => Vec::new(),
        }
    }
}

/// Whether an action is possible now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Gate {
    pub allowed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Refusal>,
}

impl Gate {
    fn of(result: Result<(), Refusal>) -> Gate {
        match result {
            Ok(()) => Gate {
                allowed: true,
                reason: None,
                refusal: None,
            },
            Err(refusal) => Gate {
                allowed: false,
                reason: Some(refusal.to_string()),
                refusal: Some(refusal),
            },
        }
    }
}

/// A daemon of the effective configuration.
///
/// pitchfork keeps a daemon defined in several files as the definition of the
/// highest layer, whole (verified in 2.29.0, `pitchfork_toml.rs` `merge`), so
/// what it `depends` on is what that definition says and nothing a replaced
/// one declared. The ways it starts without devflow (`triggers`) are still
/// collected from every layer: that only ever makes devflow more careful, and
/// every layer stays in the trust scope whatever pitchfork resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Configured {
    name: String,
    triggers: BTreeSet<String>,
    depends: BTreeSet<String>,
}

fn configured(scope: &Scope) -> Vec<Configured> {
    let mut by_name: BTreeMap<String, Configured> = BTreeMap::new();
    // Lowest layer first: a later definition replaces the dependencies of an
    // earlier one instead of adding to them.
    for layer in &scope.processes.layers {
        for daemon in &layer.daemons {
            let entry = by_name
                .entry(daemon.id.clone())
                .or_insert_with(|| Configured {
                    name: daemon.id.clone(),
                    triggers: BTreeSet::new(),
                    depends: BTreeSet::new(),
                });
            entry.triggers.extend(daemon.triggers.iter().cloned());
            // As written: `db`, or a qualified `namespace/db`. Which daemon
            // that is depends on the namespace, see [`dependency_name`].
            entry.depends = match daemon.fields.get("depends") {
                Some(Item::List(depends)) => depends
                    .iter()
                    .filter_map(|d| match d {
                        Item::Text(text) => Some(text.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => BTreeSet::new(),
            };
        }
    }
    by_name.into_values().collect()
}

fn unreadable(scope: &Scope) -> Option<Refusal> {
    scope.processes.layers.iter().find_map(|layer| {
        layer.problem.as_ref().map(|problem| Refusal::Unreadable {
            file: layer.file.clone(),
            problem: problem.clone(),
        })
    })
}

/// The approval check shared by start and stop: `DEVFLOW_TRUST_HOOKS`, the
/// approval on record for the workspace's own scope, or the caller's
/// [`Approval`] of exactly that scope.
fn approved(
    project: &Project,
    workspace: &Workspace,
    action: Action,
    approval: Option<&Approval>,
) -> Result<(), Refusal> {
    if trust::verdict_at(project, &workspace.path, approval).is_ok_and(trust::Verdict::approved) {
        return Ok(());
    }
    match trust::state_at(project, &workspace.path) {
        Ok(TrustState::Trusted) => Ok(()),
        Ok(TrustState::NothingToApprove) => Ok(()),
        Ok(TrustState::Untrusted {
            previously_trusted,
            format_changed,
            ..
        }) => Err(Refusal::NotApproved {
            action,
            changed: previously_trusted,
            format_changed,
        }),
        // An approval file that cannot be read approves nothing.
        Err(_) => Err(Refusal::NotApproved {
            action,
            changed: false,
            format_changed: false,
        }),
    }
}

/// The project as its `devflow.toml` says right now, for the last look before
/// an effect. The `Project` a caller holds is a snapshot from when it was
/// opened; readiness (the receipt against the configuration's generations),
/// the trust scope (`[hooks]`, `[env]`) and the approval are all derived from
/// its configuration, so judging them on it would approve what the file no
/// longer says. A configuration that cannot be read now is a refusal: nothing
/// can be judged on it.
fn fresh(project: &Project) -> Result<Project, Refusal> {
    project.reload().map_err(|error| Refusal::Unreadable {
        file: project
            .config_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "devflow.toml".to_string()),
        problem: format!("cannot be read now ({error:#})"),
    })
}

fn readiness(project: &Project, workspace: &Workspace) -> Result<(), Refusal> {
    let not_ready = |status: &str, detail: Option<String>| Refusal::NotReady {
        status: status.to_string(),
        detail,
    };
    match setup::receipt(project, workspace) {
        Ok(None) => Err(not_ready("not_set_up", None)),
        Ok(Some(receipt)) => {
            if Receipt::is_locked(&workspace.git_dir) {
                return Err(not_ready("in_progress", None));
            }
            match receipt.status() {
                Status::Ready => Ok(()),
                Status::NeedsAttention => Err(not_ready("needs_attention", None)),
                Status::InProgress => Err(not_ready("in_progress", None)),
            }
        }
        Err(e) => Err(not_ready("needs_attention", Some(format!("{e:#}")))),
    }
}

/// The workspace at `workspace.path` holds the instance the caller showed (or
/// the caller named none). The receipt is read now; one that is missing or
/// cannot be read is not that instance.
fn verify_instance(
    project: &Project,
    workspace: &Workspace,
    expect: Option<uuid::Uuid>,
) -> Result<(), InstanceChanged> {
    let Some(expected) = expect else {
        return Ok(());
    };
    let changed = |detail: String| InstanceChanged {
        workspace: workspace.name.clone(),
        detail,
    };
    match setup::receipt(project, workspace) {
        Ok(Some(receipt)) if receipt.instance_id == expected => Ok(()),
        Ok(Some(receipt)) => Err(changed(format!(
            "it is another instance now ({}, not {expected})",
            receipt.instance_id
        ))),
        Ok(None) => Err(changed(format!(
            "instance {expected} has no receipt there now"
        ))),
        Err(error) => Err(changed(format!(
            "its receipt cannot be read to confirm instance {expected} ({error:#})"
        ))),
    }
}

/// Everything that must hold for a start, in the order a person fixes it, and
/// the namespace the daemons will be started in (the one devflow assigned).
/// `only` names the daemons asked for (empty: all of them). With
/// `repairable_ok` a namespace file a start would write is not held against the
/// workspace (see [`own_namespace`]).
fn check_start(
    project: &Project,
    workspace: &Workspace,
    scope: &Scope,
    only: &[String],
    approval: Option<&Approval>,
    repairable_ok: bool,
) -> Result<String, Refusal> {
    if let Some(refusal) = unreadable(scope) {
        return Err(refusal);
    }
    let daemons = configured(scope);
    if daemons.is_empty() {
        return Err(Refusal::NothingConfigured);
    }
    readiness(project, workspace)?;
    let namespace = own_namespace(project, workspace, repairable_ok)?;
    approved(project, workspace, Action::Start, approval)?;
    let wanted = wanted_daemons(&daemons, &namespace, only)?;
    let triggers: Vec<Trigger> = scope
        .processes
        .autonomous
        .iter()
        .filter(|t| wanted.contains(&t.daemon))
        .cloned()
        .collect();
    if !triggers.is_empty() {
        return Err(Refusal::AutonomousTriggers { triggers });
    }
    Ok(namespace)
}

/// The daemon of the workspace that a `depends` entry names, as pitchfork
/// resolves it: a plain name is a daemon of the file's own namespace, a
/// qualified `namespace/name` is that namespace's daemon. `None` when the entry
/// names a daemon of another namespace (another project's, `global`, or the
/// parent directory's), which pitchfork would start from outside the workspace.
fn dependency_name<'a>(namespace: &str, written: &'a str) -> Option<&'a str> {
    match written.split_once('/') {
        None => Some(written),
        Some((own, name)) if own == namespace && !name.contains('/') => Some(name),
        Some(_) => None,
    }
}

/// Which daemons a start would run: all of them, or the named ones, and in
/// either case everything they depend on. Every dependency must be a daemon of
/// the workspace's own configuration: pitchfork starts dependencies by itself,
/// so one that points elsewhere (or at a name only another namespace declares)
/// would start something this workspace does not own.
fn wanted_daemons(
    daemons: &[Configured],
    namespace: &str,
    only: &[String],
) -> Result<BTreeSet<String>, Refusal> {
    // A daemon to visit, and the dependency entry that led to it.
    struct Visit {
        name: String,
        via: Option<(String, String)>,
    }
    let mut wanted: BTreeSet<String> = BTreeSet::new();
    let names: Vec<String> = if only.is_empty() {
        daemons.iter().map(|d| d.name.clone()).collect()
    } else {
        only.to_vec()
    };
    let mut pending: Vec<Visit> = names
        .into_iter()
        .map(|name| Visit { name, via: None })
        .collect();
    while let Some(Visit { name, via }) = pending.pop() {
        let Some(daemon) = daemons.iter().find(|d| d.name == name) else {
            return Err(match via {
                None => Refusal::UnknownDaemon { name },
                Some((daemon, dependency)) => Refusal::ExternalDependency { daemon, dependency },
            });
        };
        if wanted.insert(name) {
            for written in &daemon.depends {
                let Some(dependency) = dependency_name(namespace, written) else {
                    return Err(Refusal::ExternalDependency {
                        daemon: daemon.name.clone(),
                        dependency: written.clone(),
                    });
                };
                pending.push(Visit {
                    name: dependency.to_string(),
                    via: Some((daemon.name.clone(), written.clone())),
                });
            }
        }
    }
    Ok(wanted)
}

/// The daemons among `wanted` that run under mise: the ones that receive
/// `.devflow/env` (see [`runs_under_mise`]).
fn mise_daemons(scope: &Scope, wanted: &BTreeSet<String>) -> Vec<String> {
    let names: BTreeSet<&str> = scope
        .processes
        .layers
        .iter()
        .flat_map(|layer| layer.daemons.iter().map(|daemon| daemon.id.as_str()))
        .filter(|name| wanted.contains(*name))
        .collect();
    names
        .into_iter()
        .filter(|name| {
            runs_under_mise(&scope.processes, name) && !runs_devflow_exec(&scope.processes, name)
        })
        .map(str::to_string)
        .collect()
}

/// Whether daemon `name`'s `run`, as pitchfork resolves it (the definition of
/// the highest layer that has the daemon), is wholly `devflow exec -- CMD`,
/// which sets the workspace's `.devflow/env` itself, after whatever mise set.
///
/// Wholly: one simple command that starts no mise
/// ([`crate::env_file::command_is_wholly_devflow_exec`]). A `run` that merely
/// mentions `devflow exec` (`devflow exec -- true; npm run dev`) starts the app
/// outside the wrapper, and one that starts mise inside it
/// (`devflow exec -- mise exec -- npm run dev`) hands the app mise's values, so
/// neither is delivered by devflow and both are checked like any daemon that
/// relies on mise.
fn runs_devflow_exec(processes: &ProcessesScope, name: &str) -> bool {
    processes
        .layers
        .iter()
        .rev()
        .find_map(|layer| layer.daemons.iter().find(|daemon| daemon.id == name))
        .and_then(|daemon| daemon.run.as_deref())
        .is_some_and(crate::env_file::command_is_wholly_devflow_exec)
}

/// Whether pitchfork runs daemon `name` through mise, judged on every layer
/// the way pitchfork resolves it ([`runs_under_mise_with`]) with the
/// environment of this process, which a `pitchfork start` devflow runs inherits.
fn runs_under_mise(processes: &ProcessesScope, name: &str) -> bool {
    runs_under_mise_with(
        processes,
        name,
        std::env::var("PITCHFORK_MISE").ok().as_deref(),
    )
}

/// Whether pitchfork runs daemon `name` through mise.
///
/// What pitchfork does (verified in the source of 2.29.0, `ipc/batch.rs`
/// `build_run_options`, `pitchfork_toml.rs` `merge`, `settings.rs`
/// `Settings::file_layers_from`):
///
/// - A daemon defined in several files is *replaced* by the definition of the
///   highest layer that has it, not merged field by field. Its own `mise`,
///   when that definition sets one, decides.
/// - Otherwise the `general.mise` setting decides, and settings are merged over
///   *all* layers, not per file: the `PITCHFORK_MISE` environment variable of
///   the `pitchfork start` process first, then the layers highest first,
///   which is the reverse of [`crate::pitchfork_scope::LAYERS`] (a file in the
///   project directory outranks the same pair under `.config/`, and
///   `pitchfork.local.toml` outranks `pitchfork.toml`). The default is off.
///
/// Settings pitchfork reads from outside the project directory (parent
/// directories, the user's and the system's configuration, registered extra
/// files) are not visible to devflow, as for the rest of the scope. Anything
/// that is set but not a plain `false` counts as on: the check that follows
/// only ever refuses, so doubt errs on the side of checking.
fn runs_under_mise_with(processes: &ProcessesScope, name: &str, env: Option<&str>) -> bool {
    let own = processes
        .layers
        .iter()
        .rev()
        .find_map(|layer| layer.daemons.iter().find(|daemon| daemon.id == name))
        .and_then(|daemon| daemon.fields.get("mise"));
    if let Some(item) = own {
        return !matches!(item, Item::Bool(false));
    }
    if let Some(value) = env.map(str::trim).filter(|value| !value.is_empty()) {
        return !matches!(
            value.to_ascii_lowercase().as_str(),
            "false" | "0" | "no" | "off"
        );
    }
    processes
        .layers
        .iter()
        .rev()
        .find_map(|layer| match layer.shared.get("settings") {
            Some(Item::Table(settings)) => match settings.get("general") {
                Some(Item::Table(general)) => general.get("mise"),
                _ => None,
            },
            _ => None,
        })
        .is_some_and(|item| !matches!(item, Item::Bool(false)))
}

/// Checks that need a process of their own, so they run when a start is
/// actually asked for and not each time a gate is merely shown (the status of
/// a workspace is read on every refresh).
///
/// Daemons with `mise = true` get `.devflow/env` only through mise: before
/// starting any of them, mise must resolve every variable of the workspace's
/// `.devflow/env` as devflow wrote it (`env_file::check_mise_delivery`, the
/// check setup makes before its first hook). Anything else (overridden, not
/// loaded, untrusted, mise missing, the check failing) refuses, since those
/// daemons would otherwise talk to whatever database mise hands them.
fn pre_start_checks(
    workspace: &Workspace,
    scope: &Scope,
    namespace: &str,
    only: &[String],
    mise: &crate::env_file::MiseCommand,
) -> Result<(), Refusal> {
    let wanted = wanted_daemons(&configured(scope), namespace, only)?;
    let daemons = mise_daemons(scope, &wanted);
    if daemons.is_empty() {
        return Ok(());
    }
    let keys: Vec<String> = crate::env_file::read(&workspace.path)
        .map(|vars| vars.into_iter().map(|(key, _)| key).collect())
        .unwrap_or_default();
    let delivery = crate::env_file::check_mise_delivery_with(
        &workspace.path,
        &keys,
        crate::env_file::DEFAULT_MISE_TIMEOUT,
        mise,
    );
    if delivery.is_delivered() {
        Ok(())
    } else {
        Err(Refusal::MiseDelivery { daemons, delivery })
    }
}

// ---------------------------------------------------------------------------
// Status

/// One daemon of a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DaemonState {
    pub name: String,
    /// pitchfork's id (`namespace/name`) once it knows the daemon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// pitchfork's state (`running`, `stopped`, `errored`, …), or
    /// `not started` for a declared daemon pitchfork has not listed.
    pub state: String,
    pub pid: Option<u32>,
    pub ports: Vec<u16>,
    pub url: Option<String>,
    pub error: Option<String>,
    /// Ways pitchfork starts it without devflow.
    pub triggers: Vec<String>,
    /// Declared by the workspace's pitchfork files (not only left in pitchfork's state).
    pub configured: bool,
    /// devflow may start this daemon by name now: pitchfork runs, the
    /// workspace is ready, the configuration is approved, the daemon is not
    /// running and neither it nor what it depends on starts on its own.
    pub startable: bool,
}

/// The `state` of a declared daemon that pitchfork has not listed.
pub const NOT_STARTED: &str = "not started";

impl DaemonState {
    /// What the state says, through the one classifier everything shares
    /// ([`DaemonStatus`]): `running (ready)` is running, `failed: exit 3` is
    /// failed. [`DaemonState::is_not_started`] is the fallback it does not
    /// describe.
    pub fn status(&self) -> DaemonStatus {
        DaemonStatus::parse(&self.state)
    }

    /// A declared daemon pitchfork did not list: the fallback `not started`,
    /// which is an observation only while the listing could be relied on.
    pub fn is_not_started(&self) -> bool {
        self.id.is_none() && self.state == NOT_STARTED
    }

    pub fn is_active(&self) -> bool {
        self.status().is_active()
    }

    pub fn failed(&self) -> bool {
        self.status().is_failure()
    }
}

/// What pitchfork knows, read once.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub supervisor_up: Option<bool>,
    pub daemons: Vec<Daemon>,
    /// Listing entries that could not be read at all.
    pub skipped: usize,
    pub proxy: Option<ProxyStatus>,
    pub error: Option<String>,
}

impl Snapshot {
    /// Ask pitchfork (from `dir`). Never raises: problems are in `error`.
    pub fn read(pitchfork: &Pitchfork, dir: &Path) -> Snapshot {
        let mut snapshot = Snapshot::default();
        match pitchfork.supervisor_up(dir) {
            Ok(true) => snapshot.supervisor_up = Some(true),
            Ok(false) => {
                snapshot.supervisor_up = Some(false);
                return snapshot;
            }
            Err(e) => {
                snapshot.error = Some(format!("{e:#}"));
                return snapshot;
            }
        }
        match pitchfork.listing(dir) {
            Ok(listing) => {
                snapshot.daemons = listing.daemons;
                snapshot.skipped = listing.skipped;
            }
            Err(e) => snapshot.error = Some(format!("{e:#}")),
        }
        snapshot.proxy = pitchfork.proxy_status(dir).ok();
        snapshot
    }

    /// Why this listing cannot show that nothing of `namespace` runs, when it
    /// cannot: an entry that could not be read, a daemon whose id, status or
    /// pid is malformed, or one of the namespace in a state devflow does not
    /// recognise (`unknown`, or one a later pitchfork adds). Only a known
    /// stopped state counts as stopped, and for an operation that destroys
    /// something, doubt is not "inactive".
    pub fn unverifiable(&self, namespace: &str) -> Option<String> {
        let prefix = format!("{namespace}/");
        let mut reasons = Vec::new();
        if self.skipped > 0 {
            reasons.push(format!(
                "{} listing entr{} could not be read",
                self.skipped,
                if self.skipped == 1 { "y" } else { "ies" }
            ));
        }
        for daemon in &self.daemons {
            if let Some(why) = &daemon.malformed {
                reasons.push(format!("\"{}\" {why}", daemon.id));
            } else if (daemon.namespace == namespace || daemon.id.starts_with(&prefix))
                && !daemon.state_known()
            {
                reasons.push(format!(
                    "\"{}\" is in the state \"{}\", which devflow does not recognise",
                    daemon.id, daemon.status
                ));
            }
        }
        (!reasons.is_empty()).then(|| {
            format!(
                "pitchfork's listing cannot be relied on: {}",
                reasons.join("; ")
            )
        })
    }
}

/// How completely a [`ProcessStatus`] observed what runs. Its `daemons` are the
/// ones declared plus the ones pitchfork listed, with a fallback `not started`
/// for a declared daemon pitchfork did not list: that fallback is an
/// observation only while the listing could be relied on. A frontend must not
/// draw "0 running" or "not started" as known while this is incomplete, and
/// should keep Stop available: core answers a stop in that state with the
/// forceable [`Refusal::StateUnknown`].
///
/// Incomplete when pitchfork cannot be used or its listing failed; when the
/// files cannot be read or name another namespace (so the daemons would not
/// be attributed); when the listing has entries that could not be read or a
/// daemon in a state devflow does not know ([`Snapshot::unverifiable`], the
/// judgement a stop makes); when the supervisor is down while the receipt
/// records a start devflow has not seen stop (pitchfork's daemons survive a
/// supervisor that crashed); and when the daemons cannot be attributed to the
/// workspace at all, because its receipt cannot be read (no namespace is
/// assigned) or because devflow's namespace file is missing while the receipt
/// records a start or pitchfork lists daemons under the assigned namespace
/// (every one of them would be filtered out of `daemons`). A supervisor that is
/// down with no recorded start is complete: devflow started nothing here, and
/// so is a workspace devflow never set up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Observation {
    /// Every daemon state in the status was observed.
    pub complete: bool,
    /// Why not, one sentence each; empty when `complete`.
    pub reasons: Vec<String>,
}

impl Observation {
    /// Nothing is in doubt.
    pub fn observed() -> Self {
        Self {
            complete: true,
            reasons: Vec::new(),
        }
    }

    /// Complete exactly when there is no reason not to be.
    pub fn from_reasons(reasons: Vec<String>) -> Self {
        Self {
            complete: reasons.is_empty(),
            reasons,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// The reasons as one line; `None` when complete.
    pub fn summary(&self) -> Option<String> {
        (!self.complete).then(|| self.reasons.join("; "))
    }
}

/// A workspace's processes as `ps` shows them.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessStatus {
    pub workspace: String,
    pub path: PathBuf,
    /// The workspace instance (the receipt's `instance_id`) these gates were
    /// judged for; `None` without a readable receipt. A frontend hands it back
    /// as [`StartOptions::expect_instance`] / [`StopOptions::expect_instance`]
    /// so an action runs against the workspace it showed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<uuid::Uuid>,
    pub namespace: Namespace,
    /// pitchfork was found and runs.
    pub installed: bool,
    pub version: Option<String>,
    /// The workspace has pitchfork configuration with daemons.
    pub configured: bool,
    /// `None` when pitchfork could not be asked.
    pub supervisor_up: Option<bool>,
    pub daemons: Vec<DaemonState>,
    /// Whether `daemons` (and so "nothing runs") was observed, and why not.
    pub observation: Observation,
    /// What `pitchfork proxy status --json` says, when it could be asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyStatus>,
    /// Why pitchfork could not be read, or why a file could not be.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub can_start: Gate,
    pub can_stop: Gate,
    /// `[pitchfork] autostart` in devflow.toml.
    pub autostart: bool,
    /// Why autostart cannot work here (daemons that start on their own).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autostart_blocked: Option<String>,
}

/// Is there a pitchfork devflow can use? Its version if so, else why not. Usable
/// means it runs and has every feature devflow calls (see
/// [`pitchfork::Detection::usable`]), judged by the detection remembered for a
/// few seconds, so a refresh does not ask three subcommands for their help.
pub fn probe(pitchfork: Option<&Pitchfork>) -> Result<String, String> {
    let Some(pitchfork) = pitchfork else {
        return Err("pitchfork was not found on PATH".to_string());
    };
    let detection = pitchfork.detect_cached();
    if detection.usable() {
        Ok(detection.version.unwrap_or_default())
    } else {
        Err(detection.summary())
    }
}

/// The processes of `workspace`, read from pitchfork now (`pitchfork` is the
/// one on `PATH`).
pub fn status(project: &Project, workspace: &Workspace) -> ProcessStatus {
    status_with(Pitchfork::locate().as_ref(), project, workspace)
}

/// [`status`] with a given pitchfork (or none).
pub fn status_with(
    pitchfork: Option<&Pitchfork>,
    project: &Project,
    workspace: &Workspace,
) -> ProcessStatus {
    let probed = probe(pitchfork);
    let snapshot = match (&probed, pitchfork) {
        (Ok(_), Some(pitchfork)) if !workspace.missing => {
            Snapshot::read(pitchfork, &workspace.path)
        }
        _ => Snapshot::default(),
    };
    status_from(&probed, &snapshot, project, workspace)
}

/// The processes of every workspace of the project, asking pitchfork once.
pub fn status_all(
    pitchfork: Option<&Pitchfork>,
    project: &Project,
    workspaces: &[Workspace],
) -> Vec<ProcessStatus> {
    let probed = probe(pitchfork);
    let snapshot = match (&probed, pitchfork) {
        (Ok(_), Some(pitchfork)) => Snapshot::read(pitchfork, &project.root),
        _ => Snapshot::default(),
    };
    workspaces
        .iter()
        .map(|workspace| status_from(&probed, &snapshot, project, workspace))
        .collect()
}

/// Compose a workspace's status from a snapshot of pitchfork.
pub fn status_from(
    probed: &Result<String, String>,
    snapshot: &Snapshot,
    project: &Project,
    workspace: &Workspace,
) -> ProcessStatus {
    let scope = trust::scope_at(project, &workspace.path);
    let declared = configured(&scope);
    let namespace = namespace_of(&workspace.path);
    // A running daemon is this workspace's only through the namespace devflow
    // assigned (the receipt's), and only while the files say the same: a file
    // pointed at another workspace's namespace must not show that workspace's
    // daemons here.
    let attributed: Option<String> = assigned_namespace(project, workspace)
        .ok()
        .filter(|assigned| *assigned == namespace.value);
    // Why no daemon is shown, when the files name a namespace on purpose or by
    // accident (not when nothing declares one yet: a start writes it).
    let foreign = match own_namespace(project, workspace, false) {
        Err(refusal @ Refusal::ForeignNamespace { file: Some(_), .. }) => Some(refusal.to_string()),
        _ => None,
    };
    let mut daemons: Vec<DaemonState> = declared
        .iter()
        .map(|d| DaemonState {
            name: d.name.clone(),
            id: None,
            state: NOT_STARTED.to_string(),
            pid: None,
            ports: Vec::new(),
            url: None,
            error: None,
            triggers: d.triggers.iter().cloned().collect(),
            configured: true,
            startable: false,
        })
        .collect();
    for listed in snapshot
        .daemons
        .iter()
        .filter(|d| attributed.as_deref() == Some(d.namespace.as_str()))
    {
        let url = snapshot
            .proxy
            .as_ref()
            .and_then(|proxy| proxy.url_for(&workspace.path, &listed.name))
            .or_else(|| listed.url.clone());
        let state = DaemonState {
            name: listed.name.clone(),
            id: Some(listed.id.clone()),
            state: listed.status.clone(),
            pid: listed.pid,
            ports: listed.ports.clone(),
            url,
            error: listed.error.clone(),
            triggers: Vec::new(),
            configured: false,
            startable: false,
        };
        match daemons.iter_mut().find(|d| d.name == listed.name) {
            Some(existing) => {
                let triggers = std::mem::take(&mut existing.triggers);
                *existing = DaemonState {
                    triggers,
                    configured: true,
                    ..state
                };
            }
            None => daemons.push(state),
        }
    }
    daemons.sort_by(|a, b| a.name.cmp(&b.name));

    let (installed, version) = match probed {
        Ok(version) => (true, Some(version.clone()).filter(|v| !v.is_empty())),
        Err(_) => (false, None),
    };
    let observation = observe(
        probed,
        snapshot,
        project,
        workspace,
        &scope,
        &namespace,
        foreign.as_deref(),
    );
    let all_startable = if installed {
        check_start(project, workspace, &scope, &[], None, true).map(|_| ())
    } else {
        Err(Refusal::NotInstalled {
            detail: probed.clone().err().unwrap_or_default(),
        })
    };
    for daemon in &mut daemons {
        daemon.startable = daemon.configured
            && !daemon.is_active()
            && match &all_startable {
                Ok(()) => true,
                // Starting everything is refused because of a daemon's
                // triggers or dependencies, but each daemon is judged on its
                // own closure: one that depends on nothing outside the
                // workspace and starts on its own with nothing is fine by name.
                Err(Refusal::AutonomousTriggers { .. } | Refusal::ExternalDependency { .. }) => {
                    check_start(
                        project,
                        workspace,
                        &scope,
                        std::slice::from_ref(&daemon.name),
                        None,
                        true,
                    )
                    .is_ok()
                }
                Err(_) => false,
            };
    }
    let gate_start = if installed {
        Gate::of(all_startable)
    } else {
        Gate::of(Err(Refusal::NotInstalled {
            detail: probed.clone().err().unwrap_or_default(),
        }))
    };
    let gate_stop = if installed {
        Gate::of(check_stop(project, workspace, &scope, None, true))
    } else {
        Gate::of(Err(Refusal::NotInstalled {
            detail: probed.clone().err().unwrap_or_default(),
        }))
    };
    let autostart_blocked = scope.has_autonomous_triggers().then(|| {
        let list: Vec<String> = scope
            .processes
            .autonomous
            .iter()
            .map(|t| format!("{} ({})", t.daemon, t.trigger))
            .collect();
        format!(
            "autostart is off for this workspace: {} start on their own under pitchfork",
            list.join(", ")
        )
    });
    ProcessStatus {
        workspace: workspace.name.clone(),
        path: workspace.path.clone(),
        instance: setup::receipt(project, workspace)
            .ok()
            .flatten()
            .map(|receipt| receipt.instance_id),
        namespace,
        installed,
        version,
        configured: !declared.is_empty(),
        supervisor_up: snapshot.supervisor_up,
        daemons,
        observation,
        error: snapshot
            .error
            .clone()
            .or_else(|| unreadable(&scope).map(|r| r.to_string()))
            .or(foreign),
        proxy: snapshot.proxy.clone(),
        can_start: gate_start,
        can_stop: gate_stop,
        autostart: project.config.pitchfork.autostart,
        autostart_blocked,
    }
}

/// What a status could not establish about what runs, from the same facts a
/// stop is judged by ([`plan_stop`]): the listing, [`Snapshot::unverifiable`]
/// and, for a supervisor that is down, the receipt's record of a start.
fn observe(
    probed: &Result<String, String>,
    snapshot: &Snapshot,
    project: &Project,
    workspace: &Workspace,
    scope: &Scope,
    namespace: &Namespace,
    foreign: Option<&str>,
) -> Observation {
    let mut reasons = Vec::new();
    if let Err(why) = probed {
        reasons.push(format!("pitchfork cannot be used: {why}"));
    }
    reasons.extend(snapshot.error.clone());
    // Daemons are the workspace's only through its assigned namespace, and
    // only while the files name that one (see `status_from`).
    reasons.extend(unreadable(scope).map(|refusal| refusal.to_string()));
    reasons.extend(foreign.map(str::to_string));
    let assigned = match assigned_namespace(project, workspace) {
        Ok(assigned) => Some(assigned),
        // Never set up by devflow: it has started nothing here, and has no
        // namespace to attribute daemons through.
        Err(Refusal::NotReady { status, .. }) if status == "not_set_up" => None,
        // A receipt that is there and cannot be read: no namespace is assigned
        // as far as devflow can tell, so every listed daemon is filtered out
        // of the view, and what was started cannot be looked up.
        Err(refusal) => {
            let why = match &refusal {
                Refusal::NotReady {
                    detail: Some(detail),
                    ..
                } => detail.clone(),
                other => other.to_string(),
            };
            reasons.push(format!(
                "the workspace's receipt could not be read ({why}), so devflow cannot tell which \
                 of pitchfork's daemons are the workspace's"
            ));
            None
        }
    };
    // The files give no namespace of their own (devflow's file was deleted):
    // `status_from` attributes nothing through the namespace pitchfork falls
    // back to. That is only unremarkable while nothing is, or was, running
    // under the assigned one.
    if let Some(assigned) = &assigned
        && namespace.source == NamespaceSource::Default
        && namespace.value != *assigned
    {
        let listed: Vec<&str> = snapshot
            .daemons
            .iter()
            .filter(|d| d.namespace == *assigned)
            .map(|d| d.id.as_str())
            .collect();
        let recorded = recorded_start(project, workspace, assigned);
        if !listed.is_empty() {
            reasons.push(format!(
                "pitchfork lists daemons under the namespace devflow assigned to this workspace \
                 ({}), but the workspace's {LOCAL_FILE} is missing, so they are not attributed \
                 to it; a start writes it again",
                listed.join(", ")
            ));
        } else if let Some(record) = recorded {
            reasons.push(format!(
                "the receipt records a start of this workspace's daemons ({}), but the \
                 workspace's {LOCAL_FILE} is missing, so what pitchfork lists cannot be \
                 attributed to it; a start writes it again",
                record.ids.join(", ")
            ));
        }
    }
    match snapshot.supervisor_up {
        Some(false) => {
            if let Some(record) = assigned
                .as_deref()
                .and_then(|assigned| recorded_start(project, workspace, assigned))
            {
                reasons.push(supervisor_down_detail(&record));
            }
        }
        _ => reasons
            .extend(snapshot.unverifiable(assigned.as_deref().unwrap_or(namespace.value.as_str()))),
    }
    Observation::from_reasons(reasons)
}

// ---------------------------------------------------------------------------
// Start

#[derive(Debug, Clone)]
pub struct StartOptions {
    /// How long to wait for pitchfork.
    pub timeout: Duration,
    /// Daemons to start by name. Empty: every daemon of the worktree.
    pub only: Vec<String>,
    /// The person's approval of the workspace's reviewed scope for this start
    /// (see [`Approval`]); the approval on record counts without it.
    pub approval: Option<Approval>,
    /// How to start mise for the delivery check before daemons with
    /// `mise = true` start (the `mise` on `PATH` by default).
    pub mise: crate::env_file::MiseCommand,
    /// The workspace instance the caller showed ([`ProcessStatus::instance`]).
    /// With it, nothing starts unless the workspace's receipt holds that
    /// instance, checked before any effect and again right before pitchfork is
    /// called ([`InstanceChanged`] otherwise). The CLI passes `None`: it acts on
    /// whatever the checkout holds now.
    pub expect_instance: Option<uuid::Uuid>,
}

impl Default for StartOptions {
    fn default() -> Self {
        StartOptions {
            timeout: DEFAULT_START_TIMEOUT,
            only: Vec::new(),
            approval: None,
            mise: crate::env_file::MiseCommand::default(),
            expect_instance: None,
        }
    }
}

/// What a start or stop did, with the state pitchfork reports afterwards.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessReport {
    pub workspace: String,
    pub path: PathBuf,
    pub namespace: Namespace,
    pub action: Action,
    /// The daemons a start was asked for by name; empty means all of them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub requested: Vec<String>,
    /// pitchfork ran the command and it exited 0 before the deadline.
    pub ran: bool,
    pub timed_out: bool,
    /// pitchfork's complaint, when it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The state read back after the command, whatever it said.
    pub daemons: Vec<DaemonState>,
    /// Why the state could not be read, if it could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_error: Option<String>,
    pub warnings: Vec<String>,
}

impl ProcessReport {
    /// The command succeeded, the state could be read back, and it shows what
    /// was asked: after a start no requested daemon reads as failed (one that
    /// failed earlier and was not asked about does not count), after a stop
    /// nothing of the workspace still runs.
    pub fn ok(&self) -> bool {
        if !self.ran || self.state_error.is_some() {
            return false;
        }
        match self.action {
            Action::Start => !self
                .daemons
                .iter()
                .filter(|d| self.requested.is_empty() || self.requested.contains(&d.name))
                .any(DaemonState::failed),
            Action::Stop => !self.daemons.iter().any(DaemonState::is_active),
        }
    }
}

/// The pitchfork to act with, or why there is none. Not enough that it runs: a
/// build without a feature devflow calls (see [`pitchfork::Detection`]) is
/// refused by name, since the effect would otherwise be a command that means
/// something else on that build.
fn detected(pitchfork: Option<&Pitchfork>) -> Result<Pitchfork, Refusal> {
    let Some(pitchfork) = pitchfork else {
        return Err(Refusal::NotInstalled {
            detail: "pitchfork was not found on PATH".to_string(),
        });
    };
    let detection = pitchfork.detect_cached();
    if detection.usable() {
        Ok(pitchfork.clone())
    } else {
        Err(Refusal::NotInstalled {
            detail: detection.summary(),
        })
    }
}

/// Start the workspace's daemons: [`start_with`] with the pitchfork on `PATH`
/// and the default timeout.
pub fn start(project: &Project, workspace: &Workspace, timeout: Duration) -> Result<ProcessReport> {
    start_with(
        Pitchfork::locate().as_ref(),
        project,
        workspace,
        &StartOptions {
            timeout,
            ..StartOptions::default()
        },
    )
}

/// Start the daemons of `workspace` through `pitchfork` (`pitchfork start -q`
/// in its directory, with the full `namespace/name` id of each daemon its own
/// files declare, or of the named ones; never `--local`, which would also
/// start the daemons of parent directories).
///
/// Refuses, with a [`Refusal`], unless pitchfork runs, the workspace is
/// `ready`, its pitchfork configuration is the approved one and no daemon about
/// to start also starts on its own. Failures of pitchfork itself (including the
/// timeout) are not errors: the report says so and carries the state read back
/// afterwards. Writes the worktree's namespace file first when it is missing
/// (workspaces set up before it existed).
///
/// The slow parts (the mise check, asking pitchfork what runs) come first, and
/// everything that decides whether devflow may start is looked at again
/// immediately before `pitchfork start`: the files are read once more, devflow's
/// own `devflow.toml` included (the `project` in hand is a snapshot, and
/// readiness and the approval are judged on the configuration as it is then),
/// and a change in readiness, namespace, approval or content since the first
/// look refuses the start. What is left is the instant between that look and
/// pitchfork reading its own files, which devflow cannot close.
///
/// Holds the workspace's process lock from before the first look to the
/// report read back ([`process_lock`]); a second operation on the same
/// workspace's daemons is [`Busy`].
pub fn start_with(
    pitchfork: Option<&Pitchfork>,
    project: &Project,
    workspace: &Workspace,
    options: &StartOptions,
) -> Result<ProcessReport> {
    let pitchfork = detected(pitchfork)?;
    let lock = process_lock(workspace)?;
    start_locked(&pitchfork, project, workspace, options, &lock)
}

/// [`start_with`] by a caller that holds the process lock.
fn start_locked(
    pitchfork: &Pitchfork,
    project: &Project,
    workspace: &Workspace,
    options: &StartOptions,
    lock: &ProcessLock,
) -> Result<ProcessReport> {
    // Before anything is written or asked: is this the workspace that was shown?
    verify_instance(project, workspace, options.expect_instance)?;
    // devflow's own namespace file first: a workspace set up before it existed,
    // or one whose copy is stale, gets the namespace devflow assigned. (Only
    // a missing file or one devflow wrote is touched; see `ensure_namespace`.)
    let mut warnings = Vec::new();
    if let Ok(Some(receipt)) = setup::receipt(project, workspace) {
        warnings.extend(ensure_namespace(project, workspace, &receipt).warning());
    }
    // First look: may devflow start at all? Strict, the namespace in the files
    // must be the assigned one by now.
    let scope = trust::scope_at(project, &workspace.path);
    let namespace = check_start(
        project,
        workspace,
        &scope,
        &options.only,
        options.approval.as_ref(),
        false,
    )?;
    pre_start_checks(workspace, &scope, &namespace, &options.only, &options.mise)?;
    // Every query, before the last look. Daemons that already run are left
    // alone: nothing to ask pitchfork for.
    let before = Snapshot::read(pitchfork, &workspace.path);
    let wanted = start_names(&scope, &options.only);
    let running: BTreeSet<&str> = before
        .daemons
        .iter()
        .filter(|d| d.namespace == namespace && d.is_active())
        .map(|d| d.name.as_str())
        .collect();
    if !wanted.is_empty() && wanted.iter().all(|name| running.contains(name.as_str())) {
        warnings.push("the daemons are already running".to_string());
        let nothing = pitchfork::Ran {
            ok: true,
            timed_out: false,
            message: None,
        };
        return Ok(report_after(
            pitchfork,
            project,
            workspace,
            Action::Start,
            &options.only,
            nothing,
            warnings,
            Some(&namespace),
            Some(lock),
        ));
    }
    // Last look, right before the effect: read everything again, devflow's own
    // configuration included, and refuse on any change since the first look,
    // whatever the first look found.
    let current = fresh(project)?;
    verify_instance(&current, workspace, options.expect_instance)?;
    let last = trust::scope_at(&current, &workspace.path);
    let again = check_start(
        &current,
        workspace,
        &last,
        &options.only,
        options.approval.as_ref(),
        false,
    )?;
    if again != namespace || last.digest != scope.digest {
        // What was checked is not what would run: it has to be reviewed again.
        return Err(Refusal::NotApproved {
            action: Action::Start,
            changed: true,
            format_changed: false,
        }
        .into());
    }
    // The ids of exactly the daemons asked for, in the assigned namespace.
    let ids = qualified_ids(&namespace, start_names(&last, &options.only));
    // Intent first: if the supervisor dies after this, the receipt still says
    // that devflow started daemons here, which a removal or a stop must not
    // forget (see `supervisor_down`).
    record_start(project, workspace, &ids, lock)?;
    let ran = pitchfork.start(&workspace.path, &ids, options.timeout)?;
    Ok(report_after(
        pitchfork,
        project,
        workspace,
        Action::Start,
        &options.only,
        ran,
        warnings,
        Some(&namespace),
        Some(lock),
    ))
}

/// The daemons a start asks pitchfork for: the named ones, or every daemon
/// the workspace's own files declare. (What they depend on is started by
/// pitchfork; [`wanted_daemons`] has checked it is the workspace's too.)
fn start_names(scope: &Scope, only: &[String]) -> Vec<String> {
    if only.is_empty() {
        return configured(scope).into_iter().map(|d| d.name).collect();
    }
    let mut names: Vec<String> = Vec::new();
    for name in only {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    names
}

/// `namespace/name` for each of `names`.
fn qualified_ids(namespace: &str, names: Vec<String>) -> Vec<String> {
    names
        .into_iter()
        .map(|name| format!("{namespace}/{name}"))
        .collect()
}

/// Read the state back and build the report.
///
/// `captured` is the namespace devflow assigned to the workspace and acted in
/// (`None` when it has none). The result is verified against that namespace in
/// the raw `list --json` snapshot, not only against what the workspace's files
/// show: `status_from` attributes daemons through the namespace the files give
/// *now*, so a file pointed at another namespace while the operation ran would
/// make the daemons that still run under the captured one vanish from the
/// view, and a stop would read as successful. Every daemon of the captured
/// namespace is therefore in the report (so one that still runs makes a stop
/// not ok), and an attribution that changed between capture and verification
/// is itself a reason for the report not to be ok.
///
/// `lock` is the process lock of the operation. A stop that was seen to work
/// clears the receipt's record of started daemons through it; `None` leaves
/// the record alone (a removal holds the receipt lock itself, and the receipt
/// goes with the worktree).
#[allow(clippy::too_many_arguments)]
fn report_after(
    pitchfork: &Pitchfork,
    project: &Project,
    workspace: &Workspace,
    action: Action,
    requested: &[String],
    ran: pitchfork::Ran,
    mut warnings: Vec<String>,
    captured: Option<&str>,
    lock: Option<&ProcessLock>,
) -> ProcessReport {
    let snapshot = Snapshot::read(pitchfork, &workspace.path);
    let mut status = status_from(&Ok(String::new()), &snapshot, project, workspace);
    let mut state_error = snapshot.error.clone();
    if let Some(captured) = captured {
        let shown: BTreeSet<String> = status.daemons.iter().filter_map(|d| d.id.clone()).collect();
        for listed in snapshot
            .daemons
            .iter()
            .filter(|d| d.namespace == captured && !shown.contains(&d.id))
        {
            status.daemons.push(DaemonState {
                name: listed.name.clone(),
                id: Some(listed.id.clone()),
                state: listed.status.clone(),
                pid: listed.pid,
                ports: listed.ports.clone(),
                url: listed.url.clone(),
                error: listed.error.clone(),
                triggers: Vec::new(),
                configured: false,
                startable: false,
            });
        }
        status.daemons.sort_by(|a, b| a.name.cmp(&b.name));
        let files_now = namespace_of(&workspace.path).value;
        let assigned_now = assigned_namespace(project, workspace).ok();
        if state_error.is_none()
            && (files_now != captured || assigned_now.as_deref() != Some(captured))
        {
            state_error = Some(format!(
                "the pitchfork namespace of this workspace changed while the operation ran \
                 (devflow assigned \"{captured}\", its files now give \"{files_now}\"), so the \
                 result cannot be attributed to the workspace; check the workspace's \
                 processes and `pitchfork list`"
            ));
        }
    }
    // Nor over a listing that cannot show the daemons stopped: a state devflow
    // does not know is not "stopped".
    if action == Action::Stop
        && state_error.is_none()
        && let Some(captured) = captured
        && let Some(why) = snapshot.unverifiable(captured)
    {
        state_error = Some(why);
    }
    // A stop cannot be called successful over a supervisor that is down while
    // devflow knows it started daemons there: nothing was asked and nothing
    // was read back.
    if action == Action::Stop
        && state_error.is_none()
        && snapshot.supervisor_up == Some(false)
        && let Some(captured) = captured
        && let Some(record) = recorded_start(project, workspace, captured)
    {
        state_error = Some(supervisor_down_detail(&record));
    }
    let mut error = if ran.ok { None } else { ran.message.clone() };
    if ran.timed_out {
        error = Some(match error {
            Some(message) => format!("pitchfork timed out and was stopped: {message}"),
            None => "pitchfork timed out (it did not finish before the deadline) and was \
                     stopped; the state below is what it reports now"
                .to_string(),
        });
    }
    if snapshot.supervisor_up == Some(false) {
        warnings.push("the pitchfork supervisor is not running".to_string());
    }
    let mut report = ProcessReport {
        workspace: workspace.name.clone(),
        path: workspace.path.clone(),
        namespace: status.namespace,
        action,
        requested: requested.to_vec(),
        ran: ran.ok,
        timed_out: ran.timed_out,
        error,
        daemons: status.daemons,
        state_error,
        warnings,
    };
    // A stop that was seen to work (pitchfork answered, the supervisor is up,
    // nothing of the namespace is active) accounts for every daemon devflow
    // started there.
    if action == Action::Stop
        && captured.is_some()
        && snapshot.supervisor_up == Some(true)
        && report.ok()
        && let Some(lock) = lock
        && let Some(warning) = forget_start(project, workspace, lock)
    {
        report.warnings.push(warning);
    }
    report
}

// ---------------------------------------------------------------------------
// Stop

#[derive(Debug, Clone)]
pub struct StopOptions {
    pub timeout: Duration,
    /// Stop even though the configuration changed since it was approved. The
    /// report carries a warning.
    pub force: bool,
    /// The person's approval of the workspace's reviewed scope for this
    /// stop, like [`StartOptions::approval`].
    pub approval: Option<Approval>,
    /// As [`StartOptions::expect_instance`]: stop only the daemons of the
    /// workspace instance the caller showed.
    pub expect_instance: Option<uuid::Uuid>,
}

impl Default for StopOptions {
    fn default() -> Self {
        StopOptions {
            timeout: DEFAULT_STOP_TIMEOUT,
            force: false,
            approval: None,
            expect_instance: None,
        }
    }
}

/// Whether any pitchfork file devflow knows of is in `dir`, including the
/// namespace file devflow writes.
fn touches_pitchfork(dir: &Path) -> bool {
    crate::pitchfork_scope::LAYERS
        .iter()
        .any(|file| dir.join(file).symlink_metadata().is_ok())
}

/// Whether the workspace's daemons may be stopped: the namespace in its files
/// is the one devflow assigned (when there are files that give one), and the
/// configuration pitchfork would run lifecycle hooks from is approved.
fn check_stop(
    project: &Project,
    workspace: &Workspace,
    scope: &Scope,
    approval: Option<&Approval>,
    repairable_ok: bool,
) -> Result<(), Refusal> {
    if touches_pitchfork(&workspace.path) {
        own_namespace(project, workspace, repairable_ok)?;
    }
    // Without pitchfork files there is no lifecycle hook to run.
    if scope.processes.is_empty() {
        return Ok(());
    }
    if let Some(refusal) = unreadable(scope) {
        return Err(refusal);
    }
    approved(project, workspace, Action::Stop, approval)
}

/// Stop the workspace's daemons: [`stop_with`] with the pitchfork on `PATH`.
pub fn stop(project: &Project, workspace: &Workspace, timeout: Duration) -> Result<ProcessReport> {
    stop_with(
        Pitchfork::locate().as_ref(),
        project,
        workspace,
        &StopOptions {
            timeout,
            ..StopOptions::default()
        },
    )
}

/// Stop the daemons of `workspace`: `pitchfork stop` in its directory with the
/// `namespace/name` ids of its running daemons, while the configuration still
/// matches the approval; refused otherwise, unless `force`. Never `--local`,
/// which would also stop the daemons of parent directories.
///
/// What runs is asked first (a state that cannot be read is
/// [`Refusal::StateUnknown`], which `force` answers by stopping the daemons the
/// files declare); namespace and approval are judged again immediately before
/// `pitchfork stop`, by reading the files once more.
///
/// Holds the workspace's process lock from the first question to the report
/// read back ([`process_lock`]): a stop cannot slip between a start's record
/// and its effect and erase the record.
pub fn stop_with(
    pitchfork: Option<&Pitchfork>,
    project: &Project,
    workspace: &Workspace,
    options: &StopOptions,
) -> Result<ProcessReport> {
    let pitchfork = detected(pitchfork)?;
    let lock = process_lock(workspace)?;
    stop_locked(&pitchfork, project, workspace, options, &lock)
}

/// [`stop_with`] by a caller that holds the process lock.
fn stop_locked(
    pitchfork: &Pitchfork,
    project: &Project,
    workspace: &Workspace,
    options: &StopOptions,
    lock: &ProcessLock,
) -> Result<ProcessReport> {
    verify_instance(project, workspace, options.expect_instance)?;
    let plan = plan_stop(pitchfork, project, workspace, options.force)?;
    run_stop(pitchfork, project, workspace, &plan, options, Some(lock))
}

/// What a stop is going to do, decided before anything is stopped.
#[derive(Debug, Clone)]
pub struct StopPlan {
    /// The ids of the daemons of the workspace's namespace that are active.
    pub active: Vec<String>,
    /// The namespace devflow assigned to the workspace when the plan was
    /// drawn: what the result of the stop is verified against, whatever the
    /// files say by then. `None` when the workspace has none.
    pub namespace: Option<String>,
    /// What the plan itself has to say (a state that could not be read).
    pub warnings: Vec<String>,
}

/// Ask pitchfork what runs in the workspace's assigned namespace. Queries
/// only: nothing is judged here about whether those daemons may be stopped
/// (see [`authorize_stop`], which runs right before the effect).
///
/// pitchfork's state that cannot be read is not "nothing runs": it is a
/// [`Refusal::StateUnknown`], unless `force`, which then plans to stop the
/// daemons the workspace's files declare, by id (stopping one that does not
/// run is harmless).
fn plan_stop(
    pitchfork: &Pitchfork,
    project: &Project,
    workspace: &Workspace,
    force: bool,
) -> Result<StopPlan, Refusal> {
    let mut warnings = Vec::new();
    // The daemons of this workspace are the ones in the namespace devflow
    // assigned to it. A workspace devflow never set up has none.
    let assigned = match assigned_namespace(project, workspace) {
        Ok(namespace) => namespace,
        Err(Refusal::NotReady { status, .. }) if status == "not_set_up" => {
            return Ok(StopPlan {
                active: Vec::new(),
                namespace: None,
                warnings,
            });
        }
        Err(refusal) => return Err(refusal),
    };
    let snapshot = Snapshot::read(pitchfork, &workspace.path);
    // A listing that failed, or one that cannot show nothing of the namespace
    // runs (a state devflow does not know is not "stopped"), is unknown.
    let problem = snapshot.error.clone().or_else(|| {
        (snapshot.supervisor_up != Some(false))
            .then(|| snapshot.unverifiable(&assigned))
            .flatten()
    });
    if let Some(error) = &problem {
        if !force {
            return Err(Refusal::StateUnknown {
                detail: error.clone(),
                hint: None,
            });
        }
        let declared = configured(&trust::scope_at(project, &workspace.path));
        warnings.push(format!(
            "could not read what pitchfork runs ({error}); asked it to stop the daemons the \
             workspace declares without being able to tell which of them ran"
        ));
        return Ok(StopPlan {
            active: qualified_ids(&assigned, declared.into_iter().map(|d| d.name).collect()),
            namespace: Some(assigned),
            warnings,
        });
    }
    if snapshot.supervisor_up == Some(false) {
        return supervisor_down(project, workspace, assigned, force);
    }
    let active: Vec<String> = snapshot
        .daemons
        .iter()
        .filter(|d| d.namespace == assigned && d.is_active())
        .map(|d| d.id.clone())
        .collect();
    Ok(StopPlan {
        active,
        namespace: Some(assigned),
        warnings,
    })
}

/// What devflow recorded when it started daemons of `workspace` in
/// `namespace` and has not seen stop; see [`ProcessRecord`]. Ids of another
/// namespace (an instance the receipt no longer has) are not the workspace's.
fn recorded_start(
    project: &Project,
    workspace: &Workspace,
    namespace: &str,
) -> Option<ProcessRecord> {
    let record = setup::receipt(project, workspace).ok()??.processes?;
    let prefix = format!("{namespace}/");
    let ids: Vec<String> = record
        .ids
        .into_iter()
        .filter(|id| id.starts_with(&prefix))
        .collect();
    (!ids.is_empty()).then_some(ProcessRecord {
        started_at: record.started_at,
        ids,
    })
}

/// Record, in the receipt, that devflow is about to start `ids`. An error
/// (another run holds the receipt lock, the file cannot be written) means the
/// start must not happen: a daemon devflow cannot account for is worse than one
/// it did not start.
fn record_start(
    project: &Project,
    workspace: &Workspace,
    ids: &[String],
    lock: &ProcessLock,
) -> Result<()> {
    let now = chrono::Utc::now();
    Receipt::update_processes(
        &workspace.git_dir,
        &workspace.name,
        &project.config,
        lock,
        |record| {
            let mut all: BTreeSet<String> = record.take().into_iter().flat_map(|r| r.ids).collect();
            all.extend(ids.iter().cloned());
            *record = Some(ProcessRecord {
                started_at: now,
                ids: all.into_iter().collect(),
            });
        },
    )
    .context("cannot record in the receipt that devflow is starting the workspace's daemons")?;
    Ok(())
}

/// Forget the recorded start after a stop whose result was read back and
/// showed nothing of the workspace running. A receipt that cannot be written is
/// a warning, never a failure: the record then stays, which only ever makes the
/// next removal more careful.
fn forget_start(project: &Project, workspace: &Workspace, lock: &ProcessLock) -> Option<String> {
    let cleared = Receipt::update_processes(
        &workspace.git_dir,
        &workspace.name,
        &project.config,
        lock,
        |record| *record = None,
    );
    cleared.err().map(|e| {
        format!("could not clear the record of the daemons devflow started in the receipt: {e:#}")
    })
}

/// Why a supervisor that is down does not mean nothing runs: devflow started
/// the daemons in `record` and has not seen them stop, and a supervisor that
/// crashed leaves the daemons it started running.
fn supervisor_down_detail(record: &ProcessRecord) -> String {
    format!(
        "pitchfork's supervisor is down, and devflow started daemons for this workspace ({}, at \
         {}) that it has not seen stop; a supervisor that crashed leaves its daemons running",
        record.ids.join(", "),
        record.started_at.format("%Y-%m-%d %H:%M UTC")
    )
}

/// The supervisor is down, so there is no listing to read. Down is not
/// "verified nothing runs": pitchfork's daemons survive their supervisor
/// (reproduced against 2.29.0: SIGKILL of the supervisor leaves a daemon
/// running, and a restarted supervisor takes it back).
///
/// - devflow recorded a start it has not seen stop: refused as
///   [`Refusal::StateUnknown`] (with a hint to start the supervisor and stop
///   through devflow), or, with `force`, a plan that stops nothing and says the
///   daemons may still be running;
/// - devflow started nothing here: nothing to stop, and a warning that
///   processes started outside devflow cannot be checked.
fn supervisor_down(
    project: &Project,
    workspace: &Workspace,
    assigned: String,
    force: bool,
) -> Result<StopPlan, Refusal> {
    let Some(record) = recorded_start(project, workspace, &assigned) else {
        return Ok(StopPlan {
            active: Vec::new(),
            namespace: Some(assigned),
            warnings: vec![
                "pitchfork's supervisor is down; devflow started no daemons here, but processes \
                 started outside devflow can't be checked"
                    .to_string(),
            ],
        });
    };
    let detail = supervisor_down_detail(&record);
    if !force {
        return Err(Refusal::StateUnknown {
            detail,
            hint: Some(
                "start the supervisor with `pitchfork supervisor start` (it takes back the \
                 daemons that survived), then stop the workspace's processes; or go on \
                 without knowing by forcing it"
                    .to_string(),
            ),
        });
    }
    Ok(StopPlan {
        active: Vec::new(),
        namespace: Some(assigned),
        warnings: vec![format!(
            "daemons of this workspace may still be running and were not stopped: {detail}"
        )],
    })
}

/// The last look before `pitchfork stop`, with the files read again now,
/// devflow's own configuration included: the namespace they give the
/// workspace must still be the assigned one (nothing overrides that), and the
/// scope pitchfork will run lifecycle hooks under must still be approved, or
/// `force` accepts that, in which case the warning says so. Used by every
/// stop, so a change made since the plan was drawn is seen. A configuration
/// that cannot be read now cannot be judged: refused, or accepted by `force`.
fn authorize_stop(
    project: &Project,
    workspace: &Workspace,
    force: bool,
    approval: Option<&Approval>,
) -> Result<Vec<String>, Refusal> {
    let current = fresh(project);
    // The namespace comes from the receipt, whatever the configuration says.
    own_namespace(current.as_ref().unwrap_or(project), workspace, false)?;
    let judged = current.and_then(|current| {
        let scope = trust::scope_at(&current, &workspace.path);
        check_stop(&current, workspace, &scope, approval, false)
    });
    match judged {
        Ok(()) => Ok(Vec::new()),
        Err(refusal) if force => Ok(vec![format!(
            "stopped without an approval of the current configuration ({refusal}); pitchfork ran \
             lifecycle hooks from the file as it is now"
        )]),
        Err(refusal) => Err(refusal),
    }
}

fn run_stop(
    pitchfork: &Pitchfork,
    project: &Project,
    workspace: &Workspace,
    plan: &StopPlan,
    options: &StopOptions,
    lock: Option<&ProcessLock>,
) -> Result<ProcessReport> {
    let mut warnings = plan.warnings.clone();
    let ran = if plan.active.is_empty() {
        pitchfork::Ran {
            ok: true,
            timed_out: false,
            message: None,
        }
    } else {
        // The last look: still the workspace that was shown, files read again.
        if options.expect_instance.is_some() {
            verify_instance(&fresh(project)?, workspace, options.expect_instance)?;
        }
        warnings.extend(authorize_stop(
            project,
            workspace,
            options.force,
            options.approval.as_ref(),
        )?);
        // The ids of the running daemons of this namespace, never `--local`
        // (pitchfork would add the daemons of parent directories).
        pitchfork.stop(&workspace.path, &plan.active, options.timeout)?
    };
    Ok(report_after(
        pitchfork,
        project,
        workspace,
        Action::Stop,
        &[],
        ran,
        warnings,
        plan.namespace.as_deref(),
        lock,
    ))
}

/// Restart: stop (gated), then start (gated). A stop that did not leave the
/// workspace's daemons stopped (pitchfork failed or timed out, a daemon still
/// runs, or the state afterwards could not be read) is returned as it is, and
/// nothing is started on top of it.
///
/// The process lock is held across both halves, so nothing starts or stops the
/// workspace's daemons in between.
pub fn restart_with(
    pitchfork: Option<&Pitchfork>,
    project: &Project,
    workspace: &Workspace,
    start: &StartOptions,
    stop: &StopOptions,
) -> Result<ProcessReport> {
    let pitchfork = detected(pitchfork)?;
    let lock = process_lock(workspace)?;
    let stopped = stop_locked(&pitchfork, project, workspace, stop, &lock)?;
    if !stopped.ok() {
        return Ok(stopped);
    }
    let mut started = start_locked(&pitchfork, project, workspace, start, &lock)?;
    started.warnings.splice(0..0, stopped.warnings);
    Ok(started)
}

// ---------------------------------------------------------------------------
// Autostart

/// What `[pitchfork] autostart` did after a workspace became ready.
#[derive(Debug, Clone, Serialize)]
pub struct Autostart {
    /// The start that ran, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<ProcessReport>,
    /// Why nothing was started (a refusal or an error).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Refusal>,
}

/// Start the daemons of a `ready` workspace if the project asks for it
/// (`[pitchfork] autostart = true`). `None` when it does not, or when the
/// workspace has no pitchfork configuration. Never an error: whatever happens
/// is in the result, and the workspace stays as ready as it was.
pub fn autostart(
    pitchfork: Option<&Pitchfork>,
    project: &Project,
    workspace: &Workspace,
    timeout: Duration,
) -> Option<Autostart> {
    let options = StartOptions {
        timeout,
        ..StartOptions::default()
    };
    autostart_with(pitchfork, project, workspace, &options)
}

/// [`autostart`] with chosen start options.
pub fn autostart_with(
    pitchfork: Option<&Pitchfork>,
    project: &Project,
    workspace: &Workspace,
    options: &StartOptions,
) -> Option<Autostart> {
    if !project.config.pitchfork.autostart || !uses_pitchfork(&workspace.path) {
        return None;
    }
    Some(match start_with(pitchfork, project, workspace, options) {
        Ok(report) => Autostart {
            report: Some(report),
            skipped: None,
            refusal: None,
        },
        Err(error) => Autostart {
            report: None,
            skipped: Some(format!("{error:#}")),
            refusal: error.downcast_ref::<Refusal>().cloned(),
        },
    })
}

// ---------------------------------------------------------------------------
// Replacing the instance identity

/// Whether the workspace may take a new instance identity (`setup
/// --new-instance`, or `DEVFLOW_INSTANCE_ID` naming another instance), judged
/// on `old`, the receipt about to be replaced. The caller holds the workspace's
/// process lock ([`process_lock`]) and the receipt lock.
///
/// The identity is the pitchfork namespace (`df-<project8>-<instance8>`) the
/// workspace's daemons run under, and nothing but the receipt names it: once it
/// is replaced, `devflow down` and `remove` look in the new namespace, and
/// daemons left running in the old one are out of devflow's reach. So the
/// replacement is refused, before anything changes, when
///
/// - this machine's pitchfork lists a daemon of the old namespace as active
///   ([`Refusal::DaemonsRunning`]: `devflow down` first), or
/// - devflow cannot tell that none is: pitchfork cannot be asked, its listing
///   cannot be read or has a state devflow does not know, or its supervisor is
///   down while the receipt records a start devflow has not seen stop
///   ([`Refusal::StateUnknown`]).
///
/// **A copy is told from a second instance on one machine by how the new
/// identity is chosen.** A receipt copied from another machine (a VM fork)
/// describes that machine's daemons, not this one's. `copied` is true when an
/// orchestrator named the new identity through `DEVFLOW_INSTANCE_ID` (a
/// different instance than the receipt's): the person declares the receipt a
/// copy, so the record of started daemons and a pitchfork that cannot answer
/// are about another machine and do not block. What this machine's own
/// pitchfork shows as active in the old namespace still does: that is observed
/// here, not recorded. `--new-instance` alone declares nothing and gets no such
/// allowance; in a copy made with it, start the supervisor (`pitchfork
/// supervisor start`: an up supervisor whose listing shows nothing active
/// verifies that nothing runs) or use `DEVFLOW_INSTANCE_ID`.
///
/// A workspace with no pitchfork file and no recorded start has nothing to
/// check, and one that records no start on a machine without pitchfork
/// proceeds (the same exception as a removal's).
pub fn check_replacement(
    pitchfork: Option<&Pitchfork>,
    workspace: &Workspace,
    old: &Receipt,
    copied: bool,
) -> Result<(), Refusal> {
    let namespace = old.slug();
    let prefix = format!("{namespace}/");
    let record = old.processes.clone().and_then(|record| {
        let ids: Vec<String> = record
            .ids
            .into_iter()
            .filter(|id| id.starts_with(&prefix))
            .collect();
        (!ids.is_empty()).then_some(ProcessRecord {
            started_at: record.started_at,
            ids,
        })
    });
    if record.is_none() && !touches_pitchfork(&workspace.path) {
        return Ok(());
    }
    // What a person can do when this cannot be judged.
    let unknown = |detail: String| Refusal::StateUnknown {
        detail: format!(
            "{detail}; a new instance would leave any daemons of the old identity running in a \
             namespace devflow no longer owns"
        ),
        hint: Some(
            "start the supervisor with `pitchfork supervisor start`, stop the workspace's \
             processes, then set the workspace up as a new instance again; for a workspace \
             copied from another machine, give the copy's identity with DEVFLOW_INSTANCE_ID \
             instead"
                .to_string(),
        ),
    };
    // A copy's record, and a pitchfork that cannot answer about it, are about
    // another machine.
    let cannot_tell = |detail: String| {
        if copied { Ok(()) } else { Err(unknown(detail)) }
    };
    let Some(pitchfork) = pitchfork else {
        return match record {
            Some(record) => cannot_tell(format!(
                "pitchfork was not found on PATH, and devflow started daemons for this \
                 workspace ({}, at {}) that it has not seen stop",
                record.ids.join(", "),
                record.started_at.format("%Y-%m-%d %H:%M UTC")
            )),
            None => Ok(()),
        };
    };
    let detection = pitchfork.detect_cached();
    if !detection.usable() {
        return match record {
            Some(_) => cannot_tell(detection.summary()),
            None => Ok(()),
        };
    }
    let snapshot = Snapshot::read(pitchfork, &workspace.path);
    if let Some(error) = &snapshot.error {
        return cannot_tell(format!("pitchfork could not be asked: {error}"));
    }
    if snapshot.supervisor_up == Some(false) {
        return match record {
            Some(record) => cannot_tell(supervisor_down_detail(&record)),
            None => Ok(()),
        };
    }
    let active: Vec<String> = snapshot
        .daemons
        .iter()
        .filter(|d| d.namespace == namespace && d.is_active())
        .map(|d| d.id.clone())
        .collect();
    if !active.is_empty() {
        return Err(Refusal::DaemonsRunning { ids: active });
    }
    match snapshot.unverifiable(&namespace) {
        Some(why) => cannot_tell(why),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Removal

/// The daemons a removal has to stop, decided before it changes anything.
#[derive(Debug, Clone)]
pub struct RemovalStop {
    /// `None` when there is no pitchfork to ask: the plan then only carries a
    /// warning.
    pitchfork: Option<Pitchfork>,
    plan: StopPlan,
}

impl RemovalStop {
    pub fn daemons(&self) -> &[String] {
        &self.plan.active
    }
}

/// What a removal needs to know before any effect: which of the workspace's
/// daemons run and whether they may be stopped. `Ok(None)` when there is
/// nothing to stop: no pitchfork file in the workspace, a workspace devflow
/// never set up (it has no namespace), or nothing of the workspace running.
/// With no pitchfork on `PATH` the removal proceeds (a project that commits a
/// pitchfork.toml must not make everyone without pitchfork pass `--force`), but
/// a workspace that has devflow's namespace file or a recorded start carries a
/// warning that its daemons could not be checked.
///
/// Refuses, with a [`Refusal::StateUnknown`], when the state of the daemons
/// cannot be read, and that includes a pitchfork on `PATH` that cannot be
/// asked: one that does not run, or one without a feature devflow calls. Not
/// knowing is not "nothing runs". Also refuses when daemons run and the
/// configuration no longer matches the approval, or when the namespace in the
/// files is not the one assigned; `force` accepts the first two. A forced
/// removal never stops daemons through a namespace devflow did not assign, nor
/// with a pitchfork that cannot be asked: it leaves them running and says so.
///
/// A removal holds the process lock for its whole length and uses
/// [`plan_removal_locked`] and [`stop_for_removal_locked`]; this takes the
/// lock only while it plans, which is for a caller that plans and stops in
/// two separate steps of its own.
pub fn plan_removal(
    pitchfork: Option<&Pitchfork>,
    project: &Project,
    workspace: &Workspace,
    force: bool,
) -> Result<Option<RemovalStop>, Refusal> {
    if workspace.missing {
        return Ok(None);
    }
    let lock = lock_for_removal(workspace)?;
    plan_removal_locked(pitchfork, project, workspace, force, &lock)
}

/// The process lock for a removal's plan or stop, as a refusal: a workspace
/// that cannot be locked cannot be judged. Another operation holding it is
/// [`Busy`], which the caller recovers from the message.
fn lock_for_removal(workspace: &Workspace) -> Result<ProcessLock, Refusal> {
    process_lock(workspace).map_err(|error| Refusal::StateUnknown {
        detail: format!("{error:#}"),
        hint: None,
    })
}

/// [`plan_removal`] by a caller that holds the workspace's process lock.
pub fn plan_removal_locked(
    pitchfork: Option<&Pitchfork>,
    project: &Project,
    workspace: &Workspace,
    force: bool,
    _lock: &ProcessLock,
) -> Result<Option<RemovalStop>, Refusal> {
    if workspace.missing {
        return Ok(None);
    }
    let Some(located) = pitchfork else {
        return Ok(without_pitchfork(project, workspace));
    };
    // Without a pitchfork file there is nothing that gives the workspace a
    // namespace, so no daemon can be attributed to it.
    if !touches_pitchfork(&workspace.path) {
        return Ok(None);
    }
    // Nor is there one for a workspace devflow never set up: no receipt, no
    // assigned namespace.
    if matches!(
        assigned_namespace(project, workspace),
        Err(Refusal::NotReady { ref status, .. }) if status == "not_set_up"
    ) {
        return Ok(None);
    }
    let left_running = |why: &Refusal| StopPlan {
        active: Vec::new(),
        namespace: None,
        warnings: vec![format!(
            "daemons of this workspace may still be running and were not stopped: {why}"
        )],
    };
    // One that is on `PATH` but cannot be asked is another matter: the
    // workspace has pitchfork files and a namespace, daemons may run under it,
    // and a shim that does not run (`--version` exits 1) or a build without a
    // feature devflow needs to ask or stop says nothing about them. That is a
    // state devflow cannot read, not "nothing runs".
    let detection = located.detect_cached();
    if !detection.usable() {
        let unknown = Refusal::StateUnknown {
            detail: detection.summary(),
            hint: None,
        };
        if !force {
            return Err(unknown);
        }
        return Ok(Some(RemovalStop {
            pitchfork: Some(located.clone()),
            plan: left_running(&unknown),
        }));
    }
    let pitchfork = located.clone();
    let plan = match plan_stop(&pitchfork, project, workspace, force) {
        Ok(plan) => plan,
        // The receipt cannot be read: no namespace to look for.
        Err(refusal @ Refusal::NotReady { .. }) if force => left_running(&refusal),
        Err(refusal) => return Err(refusal),
    };
    if !plan.active.is_empty() {
        // The first look, so that a refusal comes before any effect. It is made
        // again right before the stop (`stop_for_removal`), which sees a change
        // made in between.
        match authorize_stop(project, workspace, force, None) {
            Ok(_) => {}
            Err(refusal @ Refusal::ForeignNamespace { .. }) if force => {
                return Ok(Some(RemovalStop {
                    pitchfork: Some(pitchfork),
                    plan: left_running(&refusal),
                }));
            }
            Err(refusal) => return Err(refusal),
        }
    }
    if plan.active.is_empty() && plan.warnings.is_empty() {
        return Ok(None);
    }
    Ok(Some(RemovalStop {
        pitchfork: Some(pitchfork),
        plan,
    }))
}

/// No pitchfork on `PATH`: devflow has nothing to ask, and a project that
/// commits a pitchfork.toml for those who use pitchfork must not make everyone
/// else pass `--force` (which also overrides the other protections) to remove a
/// workspace, so the removal proceeds. But a workspace that devflow gave a
/// namespace file, or whose daemons it recorded starting, may have daemons
/// somewhere that this machine's pitchfork cannot be asked about: that is said.
fn without_pitchfork(project: &Project, workspace: &Workspace) -> Option<RemovalStop> {
    let receipt = setup::receipt(project, workspace).ok()??;
    let namespace_file = namespace_of(&workspace.path).source == NamespaceSource::Devflow;
    if !namespace_file && receipt.processes.is_none() {
        return None;
    }
    Some(RemovalStop {
        pitchfork: None,
        plan: StopPlan {
            active: Vec::new(),
            namespace: None,
            warnings: vec![
                "pitchfork isn't on PATH, so devflow couldn't check this workspace's daemons; any \
                 that are running were not stopped"
                    .to_string(),
            ],
        },
    })
}

/// Stop what [`plan_removal`] planned, after looking at the files again: a
/// configuration changed since the plan (a new `on_stop`, another namespace) is
/// refused here as it would have been there. Returns the ids stopped and
/// warnings.
///
/// Removal needs the stop to be seen to work: pitchfork succeeded, the state
/// could be read back, and none of the workspace's daemons is active. Anything
/// else is an error, unless `force`, which goes on with a warning.
///
/// [`stop_for_removal_locked`] under a process lock taken here.
pub fn stop_for_removal(
    project: &Project,
    workspace: &Workspace,
    stop: &RemovalStop,
    force: bool,
) -> Result<(Vec<String>, Vec<String>)> {
    if stop.plan.active.is_empty() {
        return Ok((Vec::new(), stop.plan.warnings.clone()));
    }
    let lock = process_lock(workspace)?;
    stop_for_removal_locked(project, workspace, stop, force, &lock)
}

/// [`stop_for_removal`] by a caller that holds the workspace's process lock
/// (and, as a removal does, its receipt lock: the record of started daemons is
/// not cleared, the receipt goes with the worktree).
pub fn stop_for_removal_locked(
    project: &Project,
    workspace: &Workspace,
    stop: &RemovalStop,
    force: bool,
    _lock: &ProcessLock,
) -> Result<(Vec<String>, Vec<String>)> {
    if stop.plan.active.is_empty() {
        // Nothing was asked of pitchfork (only a note to pass on).
        return Ok((Vec::new(), stop.plan.warnings.clone()));
    }
    let Some(pitchfork) = &stop.pitchfork else {
        return Ok((Vec::new(), stop.plan.warnings.clone()));
    };
    let report = match run_stop(
        pitchfork,
        project,
        workspace,
        &stop.plan,
        &StopOptions {
            timeout: DEFAULT_STOP_TIMEOUT,
            force,
            approval: None,
            expect_instance: None,
        },
        None,
    ) {
        Ok(report) => report,
        Err(error) => {
            return match error.downcast_ref::<Refusal>() {
                Some(refusal @ Refusal::ForeignNamespace { .. }) if force => Ok((
                    Vec::new(),
                    vec![format!(
                        "daemons of this workspace may still be running and were not stopped: \
                         {refusal}"
                    )],
                )),
                _ => Err(error),
            };
        }
    };
    let mut warnings = report.warnings.clone();
    if !report.ok() {
        let still: Vec<&str> = report
            .daemons
            .iter()
            .filter(|d| d.is_active())
            .map(|d| d.name.as_str())
            .collect();
        let why = match (&report.error, &report.state_error) {
            (Some(error), _) => error.clone(),
            (None, Some(error)) => format!(
                "pitchfork's state could not be read or verified afterwards: {error}{}",
                if still.is_empty() {
                    String::new()
                } else {
                    format!(" ({} still running)", still.join(", "))
                }
            ),
            (None, None) => format!("{} still running", still.join(", ")),
        };
        anyhow::ensure!(
            force,
            "could not stop the workspace's pitchfork daemons: {why}"
        );
        warnings.push(format!(
            "could not stop every pitchfork daemon, or could not see that they stopped: {why}"
        ));
    }
    Ok((stop.plan.active.clone(), warnings))
}

/// The detection used when only the version matters.
pub fn detection_of(pitchfork: Option<&Pitchfork>) -> Detection {
    match pitchfork {
        Some(pitchfork) => pitchfork.detect(),
        None => pitchfork::detect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scope of a directory holding `files`.
    fn scope_of(files: &[(&str, &str)]) -> (tempfile::TempDir, ProcessesScope) {
        let dir = tempfile::tempdir().unwrap();
        for (file, text) in files {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let scope = ProcessesScope::read(dir.path());
        (dir, scope)
    }

    const DAEMON: &str = "[daemons.web]\nrun = \"a\"\n";
    const GENERAL_ON: &str = "[settings.general]\nmise = true\n";
    const GENERAL_OFF: &str = "[settings.general]\nmise = false\n";

    fn under_mise(files: &[(&str, &str)]) -> bool {
        let (_dir, scope) = scope_of(files);
        runs_under_mise_with(&scope, "web", None)
    }

    #[test]
    fn a_general_mise_setting_applies_to_the_daemons_of_every_layer() {
        // The setting and the daemon are in different files.
        assert!(under_mise(&[
            (".config/pitchfork.toml", GENERAL_ON),
            ("pitchfork.toml", DAEMON),
        ]));
        assert!(under_mise(&[
            ("pitchfork.toml", DAEMON),
            ("pitchfork.local.toml", GENERAL_ON),
        ]));
        assert!(!under_mise(&[("pitchfork.toml", DAEMON)]));
    }

    #[test]
    fn settings_follow_pitchforks_order_highest_layer_first() {
        // `.config/pitchfork.toml` < `.config/pitchfork.local.toml` <
        // `pitchfork.toml` < `pitchfork.local.toml`.
        assert!(!under_mise(&[
            (".config/pitchfork.toml", GENERAL_ON),
            (".config/pitchfork.local.toml", GENERAL_OFF),
            ("pitchfork.toml", DAEMON),
        ]));
        assert!(under_mise(&[
            (".config/pitchfork.toml", GENERAL_OFF),
            (".config/pitchfork.local.toml", GENERAL_ON),
            ("pitchfork.toml", DAEMON),
        ]));
        // The file in the project directory outranks the ones under `.config`.
        assert!(!under_mise(&[
            (".config/pitchfork.local.toml", GENERAL_ON),
            ("pitchfork.toml", &format!("{DAEMON}{GENERAL_OFF}")),
        ]));
        assert!(under_mise(&[
            (".config/pitchfork.toml", GENERAL_OFF),
            ("pitchfork.toml", &format!("{DAEMON}{GENERAL_ON}")),
        ]));
        // A layer that does not mention it does not hide a lower one.
        assert!(under_mise(&[
            (".config/pitchfork.toml", GENERAL_ON),
            (
                "pitchfork.toml",
                &format!("{DAEMON}[settings.general]\nshell = \"sh\"\n")
            ),
        ]));
    }

    #[test]
    fn a_daemons_own_setting_overrides_the_general_one() {
        let on = "[daemons.web]\nrun = \"a\"\nmise = true\n";
        let off = "[daemons.web]\nrun = \"a\"\nmise = false\n";
        assert!(under_mise(&[("pitchfork.toml", on)]));
        assert!(!under_mise(&[
            (".config/pitchfork.toml", GENERAL_ON),
            ("pitchfork.toml", off),
        ]));
        assert!(under_mise(&[
            (".config/pitchfork.toml", GENERAL_OFF),
            ("pitchfork.toml", on),
        ]));
    }

    #[test]
    fn a_higher_definition_replaces_the_daemon_instead_of_merging_with_it() {
        // pitchfork keeps the definition of the highest layer whole, so the
        // lower layer's `mise = true` is gone with the rest of it.
        let on = "[daemons.web]\nrun = \"a\"\nmise = true\n";
        assert!(!under_mise(&[
            (".config/pitchfork.toml", on),
            ("pitchfork.toml", DAEMON),
        ]));
        // ...and a lower `mise = false` does not survive a higher definition
        // that leaves it to the general setting.
        let off = "[daemons.web]\nrun = \"a\"\nmise = false\n";
        assert!(under_mise(&[
            (".config/pitchfork.toml", &format!("{off}{GENERAL_ON}")),
            ("pitchfork.toml", DAEMON),
        ]));
    }

    #[test]
    fn the_environment_of_the_pitchfork_process_outranks_every_file() {
        let (_dir, scope) = scope_of(&[("pitchfork.toml", &format!("{DAEMON}{GENERAL_OFF}"))]);
        assert!(runs_under_mise_with(&scope, "web", Some("true")));
        assert!(runs_under_mise_with(&scope, "web", Some("1")));
        assert!(!runs_under_mise_with(&scope, "web", None));
        assert!(!runs_under_mise_with(&scope, "web", Some("")));
        let (_dir, scope) = scope_of(&[("pitchfork.toml", &format!("{DAEMON}{GENERAL_ON}"))]);
        assert!(!runs_under_mise_with(&scope, "web", Some("false")));
        assert!(!runs_under_mise_with(&scope, "web", Some(" OFF ")));
        // A daemon's own value still wins over the environment.
        let (_dir, scope) = scope_of(&[(
            "pitchfork.toml",
            "[daemons.web]\nrun = \"a\"\nmise = false\n",
        )]);
        assert!(!runs_under_mise_with(&scope, "web", Some("true")));
    }

    #[test]
    fn a_setting_that_is_not_a_plain_false_counts_as_on() {
        assert!(under_mise(&[(
            "pitchfork.toml",
            "[daemons.web]\nrun = \"a\"\nmise = \"yes\"\n"
        )]));
        assert!(under_mise(&[(
            "pitchfork.toml",
            &format!("{DAEMON}[settings.general]\nmise = \"yes\"\n")
        )]));
    }
}

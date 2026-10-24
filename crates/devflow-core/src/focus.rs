//! Focus: one stable loopback address for whichever workspace you are using.
//!
//! Every workspace has a port of its own (`DEVFLOW_PORT`), which is what lets
//! several agents run side by side. Some things want one fixed address
//! anyway: an OAuth provider that only knows `http://localhost:3000/callback`,
//! cookies that are scoped to `localhost:3000`. Focus is a small devflow
//! process that listens on such a canonical port and forwards each connection
//! to the focused workspace's app port (or one of its daemons' ports).
//!
//! - **Opt-in and per user.** Nothing starts it but `devflow focus`, and
//!   `switch` never implies it: parallel agents keep their own ports.
//! - **One forwarder per project**, a plain `devflow` process (the hidden
//!   `__focus` command, [`serve`]) with std threads. It binds `127.0.0.1`
//!   only, and forwards to `127.0.0.1` only, to ports it was told about in its
//!   desired state and never to anything else.
//! - **State files**, in `<data dir>/focus/<project id>/`: `desired.json`
//!   (written by whoever asks, atomically, with a generation), `state.json`
//!   (written by the forwarder: its pid, what it applied, the last failure),
//!   `forwarder.lock` (held by the forwarder as long as it lives: that, not a
//!   pid, says whether it runs), `control.lock` (one `devflow focus` at a time)
//!   and `focus.log` (bounded: past 512 KiB it moves to `focus.log.1`).
//! - **Re-pointing.** The forwarder polls `desired.json`. A new generation is
//!   applied all or nothing: every new port is bound first, and if one cannot
//!   be, nothing changes and the failure is reported. A port that stays keeps
//!   its listening socket (a client's `connect` never fails because focus
//!   moved); only the connections to the previous target are closed, so a
//!   client reconnects to the new workspace.
//! - **TCP only.** A PostgreSQL database name travels inside the protocol, and
//!   a forward cannot change it; the use case is HTTP callbacks and cookies.
use crate::error::PortInUse;
use crate::project::Project;
use crate::remedy::{Problem, Remedy};
use crate::worktree::Workspace;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the forwarder looks for a new desired state.
const POLL: Duration = Duration::from_millis(100);
/// How long a request waits for the forwarder to apply it.
const APPLY_WAIT: Duration = if cfg!(test) {
    Duration::from_secs(3)
} else {
    Duration::from_secs(8)
};
/// How long a forward waits to reach its target.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// How many connections the forwarder carries at once.
const MAX_CONNECTIONS: usize = 512;
/// The log moves aside past this size.
const MAX_LOG: u64 = 512 * 1024;
/// The lowest port focus listens on: no privileges are ever needed.
pub const MIN_PORT: u16 = 1024;

// ---------------------------------------------------------------------------
// Requests and state

/// What a canonical port is forwarded to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    /// The workspace's own port (`DEVFLOW_PORT`).
    App,
    /// A daemon of the workspace, by name; its one port.
    Daemon { name: String },
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::App => f.write_str("app"),
            Target::Daemon { name } => write!(f, "daemon {name}"),
        }
    }
}

/// A canonical port and what it should reach: `3000` (the app port) or
/// `3000=web` (the daemon `web`; `3000=app` says the default out loud).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSpec {
    pub listen: u16,
    pub target: Target,
}

impl PortSpec {
    pub fn parse(text: &str) -> Result<Self> {
        let (port, target) = match text.split_once('=') {
            Some((port, target)) => (port, Some(target.trim())),
            None => (text, None),
        };
        let listen: u16 = port.trim().parse().map_err(|_| {
            anyhow::anyhow!("expected PORT or PORT=DAEMON (a port between {MIN_PORT} and 65535)")
        })?;
        anyhow::ensure!(
            listen >= MIN_PORT,
            "port {listen}: focus listens on ports from {MIN_PORT} up, so that no privileges are needed"
        );
        let target = match target {
            None | Some("app") | Some("") => Target::App,
            Some(name) => Target::Daemon {
                name: name.to_string(),
            },
        };
        Ok(Self { listen, target })
    }
}

/// The workspace focus points at.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Focused {
    pub name: String,
    pub path: PathBuf,
    /// The workspace's instance id (its receipt's).
    pub instance: uuid::Uuid,
    /// The project and where it, and the workspace's Git directory, live: what
    /// the forwarder looks at to know that the workspace is still the one the
    /// ports were taken from.
    #[serde(default)]
    pub project: uuid::Uuid,
    #[serde(default)]
    pub project_root: PathBuf,
    #[serde(default)]
    pub git_dir: PathBuf,
}

/// One forwarded port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mapping {
    /// The canonical port, on `127.0.0.1`.
    pub listen: u16,
    pub target: Target,
    /// The port connections are forwarded to, on `127.0.0.1`.
    pub to: u16,
    /// For a daemon's port: the process it belonged to when the forward was
    /// made. The port is only the daemon's while that process runs, so the
    /// forward ends when pitchfork says otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<Owner>,
}

/// The daemon process a forwarded port belonged to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    /// pitchfork's `namespace/name`.
    pub id: String,
    pub pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Desired {
    generation: u64,
    #[serde(default)]
    stop: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<Focused>,
    #[serde(default)]
    mappings: Vec<Mapping>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Failure {
    generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    /// The port was already in use.
    #[serde(default)]
    in_use: bool,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct State {
    pid: u32,
    started_at: DateTime<Utc>,
    /// The generation of the desired state in force.
    applied: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<Focused>,
    #[serde(default)]
    mappings: Vec<Mapping>,
    /// Why the last generation was not applied (what was in force before is
    /// still in force).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure: Option<Failure>,
}

/// Why a forwarder ended on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Ended {
    at: DateTime<Utc>,
    reason: String,
}

/// One port of the focus as `devflow focus` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Forward {
    pub listen: u16,
    pub target: Target,
    pub to: u16,
    /// Something accepts connections on the target port right now (it does
    /// not when the workspace's server is not running, or the workspace is
    /// suspended). `None` when it was not asked: that is a connection to the
    /// workspace's server, which a view that refreshes often should not make.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reachable: Option<bool>,
}

/// Where focus stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    /// The forwarder is running.
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
    /// The workspace in focus; the forwarder's own record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<Focused>,
    pub forwards: Vec<Forward>,
    /// Why the last request was not applied, if it was not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// Why the forwarder ended on its own (the workspace it forwarded to is
    /// no longer the one the ports were taken from); gone with the next focus.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
    pub log: PathBuf,
}

impl Status {
    /// Whether the forwarder points at the workspace at `path`.
    pub fn is_focused(&self, path: &Path) -> bool {
        self.running && self.workspace.as_ref().is_some_and(|w| w.path == path)
    }
}

// ---------------------------------------------------------------------------
// Files

struct Files {
    dir: PathBuf,
}

impl Files {
    fn of(project: &Project) -> Result<Self> {
        Self::of_id(project.id())
    }

    fn of_id(project: uuid::Uuid) -> Result<Self> {
        Ok(Self {
            dir: crate::paths::data_dir()?
                .join("focus")
                .join(project.simple().to_string()),
        })
    }

    fn at(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    fn create(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("cannot create {}", self.dir.display()))
    }

    fn desired(&self) -> PathBuf {
        self.dir.join("desired.json")
    }
    fn state(&self) -> PathBuf {
        self.dir.join("state.json")
    }
    fn forwarder_lock(&self) -> PathBuf {
        self.dir.join("forwarder.lock")
    }
    fn control_lock(&self) -> PathBuf {
        self.dir.join("control.lock")
    }
    fn log(&self) -> PathBuf {
        self.dir.join("focus.log")
    }
    fn ended(&self) -> PathBuf {
        self.dir.join("ended.json")
    }

    fn read<T: serde::de::DeserializeOwned>(&self, path: PathBuf) -> Option<T> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn write<T: Serialize>(&self, path: PathBuf, value: &T) -> Result<()> {
        crate::fsutil::write_atomic(&path, &serde_json::to_vec_pretty(value)?, true)
    }

    /// Whether a forwarder holds its lock.
    fn running(&self) -> bool {
        let Ok(file) = open_lock(&self.forwarder_lock()) else {
            return false;
        };
        matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
    }
}

fn open_lock(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
}

// ---------------------------------------------------------------------------
// Asking for a focus

/// Starts the forwarder process: `devflow __focus <dir>` detached, by default.
pub type Launcher<'a> = &'a dyn Fn(&Path) -> Result<()>;

/// Point focus at `workspace`: start the forwarder, or re-point the one that
/// runs. `ports` says what to forward; when empty, the ports it forwards now
/// (each to the same kind of target), else `[worktree] focus_ports`.
pub fn focus(project: &Project, workspace: &Workspace, ports: &[PortSpec]) -> Result<Status> {
    focus_with(project, workspace, ports, &spawn_forwarder)
}

/// [`focus`] with a given way to start the forwarder (tests run it in a
/// thread).
pub fn focus_with(
    project: &Project,
    workspace: &Workspace,
    ports: &[PortSpec],
    launch: Launcher<'_>,
) -> Result<Status> {
    let files = Files::of(project)?;
    files.create()?;
    // The workspace's process lock first, then the focus's control lock (the
    // lock order everywhere): a removal or a replacement of the workspace's
    // identity holds the same process lock while it ends the focus, so a focus
    // is either published before it (and ended by it) or after (and refused).
    // Identity and port are resolved below, inside both.
    let _process = crate::processes::process_lock(workspace)?;
    let _control = control(&files)?;
    let running = files.running();
    let state: Option<State> = running.then(|| files.read(files.state())).flatten();

    let specs = specs(project, ports, state.as_ref())?;
    let mappings = resolve(project, workspace, &specs)?;

    // A port the forwarder already holds is not in use by somebody else.
    let held: Vec<u16> = state
        .iter()
        .flat_map(|s| s.mappings.iter().map(|m| m.listen))
        .collect();
    for mapping in &mappings {
        if !held.contains(&mapping.listen) {
            check_free(mapping.listen)?;
        }
    }

    let receipt = crate::setup::receipt(project, workspace)?;
    let instance = receipt.map(|r| r.instance_id).unwrap_or_default();
    // A workspace being removed is not published: its removal ends the focus
    // under this lock, so a focus published after that would outlive it.
    match crate::ledger::Handle::new(&project.git_common_dir, instance).read() {
        Ok(Some(record)) if record.state == crate::ledger::State::Removing => {
            return Err(Problem::new(format!(
                "workspace '{}' is being removed, so the ports cannot be pointed at it",
                workspace.name
            ))
            .into_error());
        }
        Ok(_) => {}
        Err(error) => {
            return Err(Problem::new(format!(
                "the record of workspace '{}' cannot be read, so it is not known whether it is \
                 being removed: {error:#}",
                workspace.name
            ))
            .with(Remedy::Retry)
            .into_error());
        }
    }
    let generation = next_generation(&files, state.as_ref());
    let desired = Desired {
        generation,
        stop: false,
        workspace: Some(Focused {
            name: workspace.name.clone(),
            path: workspace.path.clone(),
            instance,
            project: project.id(),
            project_root: project.root.clone(),
            git_dir: workspace.git_dir.clone(),
        }),
        mappings,
    };
    let _ = std::fs::remove_file(files.ended());
    if !running {
        // What an earlier forwarder left says nothing about this one.
        let _ = std::fs::remove_file(files.state());
    }
    files.write(files.desired(), &desired)?;
    if !running {
        launch(&files.dir)?;
    }
    let failure = wait_applied(&files, generation)?;
    if let Some(failure) = failure {
        if !running {
            // A forwarder that never applied anything has nothing to keep.
            let _ = stop_locked(&files);
        }
        return Err(failure_error(&failure));
    }
    Ok(read_status(&files, true))
}

/// Where focus stands, without changing anything. Each target port is tried
/// with a connection, so [`Forward::reachable`] says whether it answers.
pub fn status(project: &Project) -> Result<Status> {
    Ok(read_status(&Files::of(project)?, true))
}

/// [`status`] without connecting to the target ports (`reachable` is `None`):
/// for a view that reads it on every refresh.
pub fn status_unprobed(project: &Project) -> Result<Status> {
    Ok(read_status(&Files::of(project)?, false))
}

/// End the forwarder. Connections in flight are closed; the workspaces are
/// untouched. Not running is not an error.
pub fn stop(project: &Project) -> Result<Status> {
    let files = Files::of(project)?;
    files.create()?;
    let _control = control(&files)?;
    stop_locked(&files)?;
    Ok(read_status(&files, false))
}

/// What a removal did to the focus: the forwards that targeted the removed
/// workspace are gone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Released {
    /// The workspace's name, as the focus knew it.
    pub workspace: String,
    /// The canonical loopback ports that were forwarded to it.
    pub ports: Vec<u16>,
    /// None of the project's forwards remain, so the forwarder was stopped.
    /// (A focus points at one workspace, so dropping its forwards is
    /// stopping it; this stays a separate fact for when that changes.)
    pub stopped: bool,
}

impl Released {
    /// In words, for a removal's output.
    pub fn describe(&self) -> String {
        let ports: Vec<String> = self
            .ports
            .iter()
            .map(|p| format!("127.0.0.1:{p}"))
            .collect();
        format!(
            "ended the focus on {}: {} no longer forward{} to it",
            self.workspace,
            ports.join(", "),
            if self.ports.len() == 1 { "s" } else { "" }
        )
    }
}

/// Drop every forward that targets the workspace instance `instance` of the
/// project `project_id` (it is being removed, and its port may be given to
/// another workspace, to which a forward left behind would silently lead), and
/// stop the forwarder when none remain. Matched by project and instance, never
/// by port or path. `None` when the focus is off or points elsewhere; an error
/// when the focus could not be read or ended, which the caller reports.
pub fn release_instance(project_id: uuid::Uuid, instance: uuid::Uuid) -> Result<Option<Released>> {
    release(project_id, instance, None)
}

/// [`release_instance`] for the checkout at `path` only: a checkout that is
/// about to get another identity (a fork) ends the focus on the one it had,
/// but a copy of it elsewhere keeps its own.
pub fn release_checkout(
    project_id: uuid::Uuid,
    instance: uuid::Uuid,
    path: &Path,
) -> Result<Option<Released>> {
    release(project_id, instance, Some(path))
}

fn release(
    project_id: uuid::Uuid,
    instance: uuid::Uuid,
    path: Option<&Path>,
) -> Result<Option<Released>> {
    let files = Files::of_id(project_id)?;
    if !files.dir.is_dir() {
        // A focus was never published for this project on this machine.
        return Ok(None);
    }
    // The control lock first, and everything is judged under it: a focus that
    // is being published holds it (and the workspace's process lock, which the
    // caller of a removal or a replacement holds too).
    let _control = control(&files)?;
    if !files.running() {
        return Ok(None);
    }
    let ours = |w: &Focused| w.instance == instance && path.is_none_or(|path| w.path == path);

    // What the forwarder applies now, and what it has been asked and may still
    // apply (a request that timed out leaves one behind). A running forwarder
    // whose applied state cannot be read is a target nobody can establish.
    let applied: Option<State> = match std::fs::read(files.state()) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).map_err(|_| {
            anyhow::anyhow!(
                "the focus forwarder is running and its state cannot be read, so it is not \
                 known where it forwards to; see {}",
                files.state().display()
            )
        })?),
        // A forwarder that runs has a state: without one, what it forwards to
        // is as unknown as with one that cannot be read. The release refuses
        // (the caller can try again) rather than conclude that nothing is
        // forwarded to the workspace.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow::anyhow!(
                "the focus forwarder is running and its state is missing, so it is not known \
                 where it forwards to; see {}",
                files.state().display()
            ));
        }
        Err(e) => {
            return Err(anyhow::Error::new(e).context("cannot read the focus forwarder's state"));
        }
    };
    let applied_generation = applied.as_ref().map_or(0, |s| s.applied);
    let outstanding: Option<Desired> = files
        .read::<Desired>(files.desired())
        .filter(|d| !d.stop && d.generation > applied_generation);

    let applied_here = applied
        .as_ref()
        .and_then(|s| s.workspace.as_ref())
        .is_some_and(ours);
    let outstanding_here = outstanding
        .as_ref()
        .and_then(|d| d.workspace.as_ref())
        .is_some_and(ours);

    if applied_here {
        let state = applied.expect("applied here");
        let workspace = state.workspace.expect("applied here");
        stop_locked(&files)?;
        return Ok(Some(Released {
            workspace: workspace.name,
            ports: state.mappings.iter().map(|m| m.listen).collect(),
            stopped: true,
        }));
    }
    if outstanding_here {
        let wanted = outstanding.expect("outstanding here");
        let workspace = wanted.workspace.expect("outstanding here");
        let ports = wanted.mappings.iter().map(|m| m.listen).collect();
        // Another workspace is in force: its focus stays, and the request for
        // this one is replaced by the state in force. Nothing is in force:
        // the forwarder has nothing to keep.
        match applied.filter(|s| s.workspace.is_some()) {
            Some(state) => {
                let generation = next_generation(&files, Some(&state));
                files.write(
                    files.desired(),
                    &Desired {
                        generation,
                        stop: false,
                        workspace: state.workspace.clone(),
                        mappings: state.mappings.clone(),
                    },
                )?;
                if let Some(failure) = wait_applied(&files, generation)? {
                    anyhow::bail!(
                        "the request for the removed workspace could not be replaced: {}",
                        failure.message
                    );
                }
                return Ok(Some(Released {
                    workspace: workspace.name,
                    ports,
                    stopped: false,
                }));
            }
            None => {
                stop_locked(&files)?;
                return Ok(Some(Released {
                    workspace: workspace.name,
                    ports,
                    stopped: true,
                }));
            }
        }
    }
    Ok(None)
}

fn stop_locked(files: &Files) -> Result<()> {
    if !files.running() {
        let _ = std::fs::remove_file(files.state());
        return Ok(());
    }
    let generation = next_generation(files, None);
    files.write(
        files.desired(),
        &Desired {
            generation,
            stop: true,
            workspace: None,
            mappings: Vec::new(),
        },
    )?;
    let deadline = Instant::now() + APPLY_WAIT;
    while files.running() {
        if Instant::now() >= deadline {
            // It does not answer, and nothing here can tell it apart from an
            // unrelated process that was given its pid: no signal is sent. The
            // caller is told it may still be forwarding.
            anyhow::bail!(
                "the focus forwarder did not stop and may still be forwarding; see {}",
                files.log().display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = std::fs::remove_file(files.state());
    Ok(())
}

fn control(files: &Files) -> Result<std::fs::File> {
    let file = open_lock(&files.control_lock())?;
    let deadline = Instant::now()
        + if cfg!(test) {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(5)
        };
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(Problem::new("another devflow focus is changing the focus")
                    .with(Remedy::Retry)
                    .into_error());
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

fn next_generation(files: &Files, state: Option<&State>) -> u64 {
    let desired = files
        .read::<Desired>(files.desired())
        .map_or(0, |d| d.generation);
    desired.max(state.map_or(0, |s| s.applied)) + 1
}

/// Which ports to forward: those asked for, else the ones in force, else the
/// configured ones.
fn specs(project: &Project, asked: &[PortSpec], state: Option<&State>) -> Result<Vec<PortSpec>> {
    let specs: Vec<PortSpec> = if !asked.is_empty() {
        asked.to_vec()
    } else if let Some(state) = state.filter(|s| !s.mappings.is_empty()) {
        state
            .mappings
            .iter()
            .map(|m| PortSpec {
                listen: m.listen,
                target: m.target.clone(),
            })
            .collect()
    } else {
        // A shared key: the main checkout's, whichever workspace this project
        // was resolved for.
        project
            .main_config()
            .worktree
            .focus_ports
            .iter()
            .map(|port| PortSpec {
                listen: *port,
                target: Target::App,
            })
            .collect()
    };
    if specs.is_empty() {
        return Err(Problem::new(
            "there is no port to forward: name one, or list the canonical ports in \
             [worktree] focus_ports",
        )
        .with(Remedy::OpenFile {
            path: project.root.join("devflow.toml"),
        })
        .into_error());
    }
    let mut seen = std::collections::BTreeSet::new();
    for spec in &specs {
        anyhow::ensure!(
            spec.listen >= MIN_PORT,
            "port {}: focus listens on ports from {MIN_PORT} up",
            spec.listen
        );
        anyhow::ensure!(
            seen.insert(spec.listen),
            "port {} is named twice",
            spec.listen
        );
    }
    Ok(specs)
}

/// The port each spec reaches in `workspace`.
fn resolve(project: &Project, workspace: &Workspace, specs: &[PortSpec]) -> Result<Vec<Mapping>> {
    let receipt = crate::setup::receipt(project, workspace)?.ok_or_else(|| {
        Problem::new(format!(
            "workspace '{}' is not set up, so it has no ports yet",
            workspace.name
        ))
        .with(Remedy::Setup {
            workspace: workspace.path.clone(),
        })
        .into_error()
    })?;
    let mut daemons: Option<Vec<crate::processes::DaemonState>> = None;
    let mut mappings = Vec::new();
    for spec in specs {
        let mut owner = None;
        let to = match &spec.target {
            Target::App => receipt.app_port.ok_or_else(|| {
                Problem::new(format!(
                    "workspace '{}' has no port of its own yet (DEVFLOW_PORT)",
                    workspace.name
                ))
                .with(Remedy::Setup {
                    workspace: workspace.path.clone(),
                })
                .into_error()
            })?,
            Target::Daemon { name } => {
                let all = daemons
                    .get_or_insert_with(|| crate::processes::status(project, workspace).daemons);
                let daemon = all.iter().find(|d| &d.name == name).ok_or_else(|| {
                    anyhow::anyhow!("workspace '{}' has no daemon '{name}'", workspace.name)
                })?;
                // The port is the daemon's only while its process runs: the
                // forward is bound to that process, so there must be one.
                match (&daemon.id, daemon.pid, daemon.is_active()) {
                    (Some(id), Some(pid), true) => {
                        owner = Some(Owner {
                            id: id.clone(),
                            pid,
                        });
                    }
                    _ => anyhow::bail!(
                        "daemon '{name}' of '{}' is not running, so there is no process for \
                         its port to belong to; start it first",
                        workspace.name
                    ),
                }
                match daemon.ports[..] {
                    [port] => port,
                    [] => anyhow::bail!(
                        "daemon '{name}' of '{}' reports no port to forward to",
                        workspace.name
                    ),
                    _ => anyhow::bail!(
                        "daemon '{name}' of '{}' listens on several ports ({}); \
                         it is ambiguous which one to forward to",
                        workspace.name,
                        daemon
                            .ports
                            .iter()
                            .map(u16::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                }
            }
        };
        mappings.push(Mapping {
            listen: spec.listen,
            target: spec.target.clone(),
            to,
            owner,
        });
    }
    if let Some(looped) = mappings
        .iter()
        .find(|m| mappings.iter().any(|other| other.listen == m.to))
    {
        anyhow::bail!(
            "port {} would be forwarded to {}, which focus itself listens on",
            looped.listen,
            looped.to
        );
    }
    Ok(mappings)
}

fn check_free(port: u16) -> Result<()> {
    match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => Err(PortInUse { port }.into()),
        Err(e) => Err(anyhow::Error::new(e).context(format!("cannot listen on 127.0.0.1:{port}"))),
    }
}

fn failure_error(failure: &Failure) -> anyhow::Error {
    match failure.port {
        Some(port) if failure.in_use => PortInUse { port }.into(),
        _ => anyhow::anyhow!(
            "the focus forwarder could not apply it: {}",
            failure.message
        ),
    }
}

/// Wait for the forwarder to apply `generation`; `Some` is why it did not.
fn wait_applied(files: &Files, generation: u64) -> Result<Option<Failure>> {
    let deadline = Instant::now() + APPLY_WAIT;
    loop {
        if let Some(state) = files.read::<State>(files.state()) {
            if state.applied >= generation {
                return Ok(None);
            }
            if let Some(failure) = state.failure.filter(|f| f.generation >= generation) {
                return Ok(Some(failure));
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "the focus forwarder did not answer in time; its log is {}",
                files.log().display()
            );
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

fn read_status(files: &Files, probe: bool) -> Status {
    let running = files.running();
    let state = running
        .then(|| files.read::<State>(files.state()))
        .flatten();
    let forwards = state
        .iter()
        .flat_map(|s| s.mappings.iter())
        .map(|m| Forward {
            listen: m.listen,
            target: m.target.clone(),
            to: m.to,
            reachable: probe.then(|| {
                TcpStream::connect_timeout(
                    &SocketAddr::from((Ipv4Addr::LOCALHOST, m.to)),
                    Duration::from_millis(250),
                )
                .is_ok()
            }),
        })
        .collect();
    Status {
        running,
        pid: state.as_ref().map(|s| s.pid),
        since: state.as_ref().map(|s| s.started_at),
        workspace: state.as_ref().and_then(|s| s.workspace.clone()),
        forwards,
        failure: state
            .as_ref()
            .and_then(|s| s.failure.as_ref())
            .map(|f| f.message.clone()),
        ended: (!running)
            .then(|| files.read::<Ended>(files.ended()))
            .flatten()
            .map(|e| e.reason),
        log: files.log(),
    }
}

/// Start `devflow __focus <dir>`: its own process group, no input, output to
/// the log.
#[cfg(unix)]
fn spawn_forwarder(dir: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;
    let program = crate::paths::devflow_program()?;
    let log = Files::at(dir).log();
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log)
        .with_context(|| format!("cannot open {}", log.display()))?;
    let mut command = std::process::Command::new(&program);
    command
        .arg("__focus")
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .process_group(0);
    let child = command
        .spawn()
        .with_context(|| format!("cannot start {}", program.display()))?;
    // Not waited for: it outlives this process.
    drop(child);
    Ok(())
}

#[cfg(not(unix))]
fn spawn_forwarder(_dir: &Path) -> Result<()> {
    anyhow::bail!("focus is not available on this system")
}

// ---------------------------------------------------------------------------
// The forwarder

struct Log {
    path: PathBuf,
    file: Mutex<(std::fs::File, u64)>,
}

impl Log {
    fn open(path: PathBuf) -> Result<Arc<Self>> {
        let file = open_log(&path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Arc::new(Self {
            path,
            file: Mutex::new((file, size)),
        }))
    }

    fn line(&self, text: &str) {
        let Ok(mut guard) = self.file.lock() else {
            return;
        };
        if guard.1 > MAX_LOG {
            let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
            if let Ok(file) = open_log(&self.path) {
                *guard = (file, 0);
            }
        }
        let line = format!("{} {text}\n", Utc::now().format("%Y-%m-%dT%H:%M:%SZ"));
        if guard.0.write_all(line.as_bytes()).is_ok() {
            guard.1 += line.len() as u64;
        }
    }
}

fn open_log(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))
}

/// One carried connection, kept so a re-point can close it.
struct Conn {
    id: u64,
    client: TcpStream,
    upstream: TcpStream,
}

/// Where a listener's connections go, and the ones that are open. One mutex
/// holds all of it, so that a connection that reads its target, connects, and
/// registers later can be refused if the route changed in between: a re-point
/// or a shutdown moves `generation` and drains `conns` under this lock, and a
/// connection registers only under the generation it started with.
struct Route {
    to: u16,
    generation: u64,
    /// Terminal: the listener was shut. Nothing is admitted or registered
    /// after it, whatever generation it started under.
    closed: bool,
    conns: Vec<Conn>,
}

type Shared = Arc<Mutex<Route>>;

struct Listener {
    addr: SocketAddr,
    route: Shared,
    mapping: Mapping,
}

/// What an accepted connection is admitted under: the target and generation
/// read together, or nothing when the listener is shut. One critical section,
/// so a shutdown or a re-point is either before it (seen here) or after it
/// (seen at registration).
fn admit(route: &Shared) -> Option<(u16, u64)> {
    let route = route.lock().unwrap_or_else(|e| e.into_inner());
    (!route.closed).then_some((route.to, route.generation))
}

impl Listener {
    /// Close every open connection and refuse the ones being made, now:
    /// optionally pointing the listener elsewhere in the same step.
    fn reroute(&self, to: Option<u16>) {
        let mut route = self.route.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(to) = to {
            route.to = to;
        }
        route.generation += 1;
        for conn in route.conns.drain(..) {
            let _ = conn.client.shutdown(Shutdown::Both);
            let _ = conn.upstream.shutdown(Shutdown::Both);
        }
    }

    /// Stop accepting and close what is open or being opened. The listener is
    /// closed for good in the same critical section that moves the generation
    /// and drains the connections; the accept thread is woken by a connection
    /// to the listener itself, and then drops the socket.
    fn shut(&self) {
        {
            let mut route = self.route.lock().unwrap_or_else(|e| e.into_inner());
            route.closed = true;
            route.generation += 1;
            for conn in route.conns.drain(..) {
                let _ = conn.client.shutdown(Shutdown::Both);
                let _ = conn.upstream.shutdown(Shutdown::Both);
            }
        }
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(500));
    }
}

struct Forwarder {
    log: Arc<Log>,
    live: Arc<AtomicUsize>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
    listeners: BTreeMap<u16, Listener>,
    probes: Arc<Probes>,
}

impl Forwarder {
    /// Apply `desired`, all or nothing: every new port is bound before
    /// anything changes.
    fn apply(&mut self, desired: &Desired) -> std::result::Result<(), Failure> {
        let fail = |port: Option<u16>, in_use: bool, message: String| Failure {
            generation: desired.generation,
            port,
            in_use,
            message,
        };
        // A request is looked at before it is applied: one that waited (a
        // timed-out request) may be about a workspace that is gone by now, and
        // its port may belong to another one.
        if let Some(workspace) = &desired.workspace
            && let Some(reason) = check(workspace, &desired.mappings, &self.probes, false).gone
        {
            return Err(fail(None, false, format!("not applied, {reason}")));
        }
        let mut seen = std::collections::BTreeSet::new();
        for m in &desired.mappings {
            if m.listen < MIN_PORT || m.to == 0 || !seen.insert(m.listen) {
                return Err(fail(
                    Some(m.listen),
                    false,
                    format!("invalid port {}", m.listen),
                ));
            }
        }
        // Never forward to oneself (or to a port that is about to be bound).
        if let Some(m) = desired
            .mappings
            .iter()
            .find(|m| desired.mappings.iter().any(|o| o.listen == m.to))
        {
            return Err(fail(
                Some(m.listen),
                false,
                format!(
                    "port {} would forward to {}, a port of the focus",
                    m.listen, m.to
                ),
            ));
        }
        let mut fresh: Vec<(&Mapping, TcpListener)> = Vec::new();
        for m in &desired.mappings {
            if self.listeners.contains_key(&m.listen) {
                continue;
            }
            match TcpListener::bind((Ipv4Addr::LOCALHOST, m.listen)) {
                Ok(listener) => fresh.push((m, listener)),
                Err(e) => {
                    return Err(fail(
                        Some(m.listen),
                        e.kind() == std::io::ErrorKind::AddrInUse,
                        format!("cannot listen on 127.0.0.1:{}: {e}", m.listen),
                    ));
                }
            }
        }
        // Past this point nothing fails.
        let wanted: Vec<u16> = desired.mappings.iter().map(|m| m.listen).collect();
        let gone: Vec<u16> = self
            .listeners
            .keys()
            .filter(|port| !wanted.contains(port))
            .copied()
            .collect();
        for port in gone {
            if let Some(listener) = self.listeners.remove(&port) {
                listener.shut();
                self.log.line(&format!("stopped forwarding {port}"));
            }
        }
        for m in &desired.mappings {
            if let Some(existing) = self.listeners.get_mut(&m.listen) {
                if existing.mapping.to != m.to {
                    // Clients of the previous target reconnect to the new one;
                    // the listening socket stays.
                    existing.reroute(Some(m.to));
                    self.log.line(&format!(
                        "re-pointed {} from {} to {}",
                        m.listen, existing.mapping.to, m.to
                    ));
                }
                existing.mapping = m.clone();
            }
        }
        for (m, listener) in fresh {
            let started = self.start_listener(m, listener);
            self.listeners.insert(m.listen, started);
            self.log.line(&format!(
                "forwarding 127.0.0.1:{} to 127.0.0.1:{}",
                m.listen, m.to
            ));
        }
        Ok(())
    }

    fn start_listener(&self, mapping: &Mapping, socket: TcpListener) -> Listener {
        let addr = socket
            .local_addr()
            .unwrap_or_else(|_| SocketAddr::from((Ipv4Addr::LOCALHOST, mapping.listen)));
        let route: Shared = Arc::new(Mutex::new(Route {
            to: mapping.to,
            generation: 0,
            closed: false,
            conns: Vec::new(),
        }));
        let (log, live, next_id) = (self.log.clone(), self.live.clone(), self.next_id.clone());
        let route_in = route.clone();
        std::thread::spawn(move || {
            loop {
                match socket.accept() {
                    Ok((client, _)) => {
                        // The target and the generation it belongs to, together;
                        // a closed listener admits nothing.
                        let Some((target, generation)) = admit(&route_in) else {
                            break;
                        };
                        if live.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                            live.fetch_sub(1, Ordering::SeqCst);
                            log.line("too many connections; one was refused");
                            continue;
                        }
                        let carry = Carry {
                            target,
                            generation,
                            route: route_in.clone(),
                            log: log.clone(),
                            live: live.clone(),
                            id: next_id.fetch_add(1, Ordering::SeqCst),
                        };
                        std::thread::spawn(move || carry.run(client));
                    }
                    Err(e) => {
                        if route_in.lock().unwrap_or_else(|e| e.into_inner()).closed {
                            break;
                        }
                        log.line(&format!("accept failed: {e}"));
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        });
        Listener {
            addr,
            route,
            mapping: mapping.clone(),
        }
    }

    /// Stop forwarding one port: its listener is shut and its connections end.
    fn remove(&mut self, listen: u16) {
        if let Some(listener) = self.listeners.remove(&listen) {
            listener.shut();
        }
    }

    fn shut_all(&mut self) {
        for listener in std::mem::take(&mut self.listeners).into_values() {
            listener.shut();
        }
    }
}

/// One connection: to the target, and bytes both ways until either ends.
struct Carry {
    target: u16,
    /// The route's generation when the connection was accepted.
    generation: u64,
    route: Shared,
    log: Arc<Log>,
    live: Arc<AtomicUsize>,
    id: u64,
}

impl Carry {
    fn run(self, client: TcpStream) {
        let outcome = self.carry(client);
        if let Err(message) = outcome {
            self.log.line(&message);
        }
        self.route
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .conns
            .retain(|c| c.id != self.id);
        self.live.fetch_sub(1, Ordering::SeqCst);
    }

    fn carry(&self, client: TcpStream) -> std::result::Result<(), String> {
        let target = SocketAddr::from((Ipv4Addr::LOCALHOST, self.target));
        let upstream = TcpStream::connect_timeout(&target, CONNECT_TIMEOUT)
            .map_err(|e| format!("cannot reach 127.0.0.1:{}: {e}", self.target))?;
        let _ = client.set_nodelay(true);
        let _ = upstream.set_nodelay(true);
        let (c2, u2) = (
            client.try_clone().map_err(|e| e.to_string())?,
            upstream.try_clone().map_err(|e| e.to_string())?,
        );
        {
            let mut route = self.route.lock().unwrap_or_else(|e| e.into_inner());
            if route.closed || route.generation != self.generation {
                // The route changed (a re-point, a shutdown) while this
                // connection was being made: it belongs to the previous
                // target, so it ends and the client reconnects.
                drop(route);
                let _ = client.shutdown(Shutdown::Both);
                let _ = upstream.shutdown(Shutdown::Both);
                return Ok(());
            }
            route.conns.push(Conn {
                id: self.id,
                client: c2,
                upstream: u2,
            });
        }
        let (mut from_client, mut to_upstream) = (
            client.try_clone().map_err(|e| e.to_string())?,
            upstream.try_clone().map_err(|e| e.to_string())?,
        );
        let back = std::thread::spawn({
            let (mut from_upstream, mut to_client) = (upstream, client);
            move || {
                let _ = std::io::copy(&mut from_upstream, &mut to_client);
                let _ = to_client.shutdown(Shutdown::Write);
            }
        });
        let _ = std::io::copy(&mut from_client, &mut to_upstream);
        let _ = to_upstream.shutdown(Shutdown::Write);
        let _ = back.join();
        Ok(())
    }
}

/// Run the forwarder for the state directory `dir` until it is told to stop.
/// This is what the hidden `__focus` command does; it returns when the desired
/// state says stop, when it is gone, or at once when a forwarder already
/// holds the lock.
pub fn serve(dir: &Path) -> Result<()> {
    serve_with(dir, Arc::new(Probes::real()))
}

/// How often the forwarder checks that the workspace it forwards to is still
/// the one its ports were taken from (files and one look at the port ledger).
const VALIDATE_EVERY: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(2)
};

/// How often it also asks pitchfork whether the daemons it forwards to are
/// still the processes their ports were taken from: a query that starts a
/// process, so a slower cadence.
const DAEMONS_EVERY: Duration = if cfg!(test) {
    Duration::from_millis(600)
} else {
    Duration::from_secs(15)
};

type HoldsFn = dyn Fn(uuid::Uuid, uuid::Uuid, &[u16]) -> Option<Vec<bool>> + Send + Sync;
type DaemonsFn = dyn Fn(&Focused) -> Option<crate::pitchfork::Listing> + Send + Sync;

/// What the validation asks of the machine, so that tests can answer.
struct Probes {
    /// Which of the ports the instance of the project holds in the port
    /// ledger, in order; `None` when it cannot be told.
    holds: Box<HoldsFn>,
    /// The daemons pitchfork lists for the workspace; `None` when it cannot be
    /// asked.
    daemons: Box<DaemonsFn>,
}

impl Probes {
    fn real() -> Self {
        Self {
            holds: Box::new(crate::ports::holds_all),
            daemons: Box::new(|workspace| {
                let pitchfork = crate::pitchfork::Pitchfork::locate()?;
                pitchfork.listing(&workspace.path).ok()
            }),
        }
    }
}

/// What a look at the workspace a focus points at found.
#[derive(Debug, Default, PartialEq, Eq)]
struct Check {
    /// The workspace is no longer the one the ports were taken from: every
    /// forward ends.
    gone: Option<String>,
    /// Daemon forwards whose process no longer owns the port: those end, the
    /// others go on.
    dropped: Vec<(u16, String)>,
    /// What could not be told (an unreadable receipt, ledger or listing): not a
    /// reason to stop forwarding, and said once in the log.
    unknown: Option<String>,
}

impl Check {
    fn gone(reason: impl Into<String>) -> Self {
        Self {
            gone: Some(reason.into()),
            ..Self::default()
        }
    }
    fn unknown(reason: impl Into<String>) -> Self {
        Self {
            unknown: Some(reason.into()),
            ..Self::default()
        }
    }
}

/// A path is there, is not, or cannot be told: absent is a fact, an error is
/// not.
enum Seen {
    Dir,
    Absent,
    Unknown(String),
}

fn seen_dir(path: &Path) -> Seen {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Seen::Dir,
        Ok(_) => Seen::Absent,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Seen::Absent,
        Err(e) => Seen::Unknown(format!("{}: {e}", path.display())),
    }
}

/// Whether the checkout at `path` is still the worktree whose Git directory is
/// `git_dir`: it exists, its `.git` is that directory (the main checkout) or a
/// file that points to it (a linked worktree).
fn checkout(path: &Path, git_dir: &Path) -> Option<Check> {
    match seen_dir(path) {
        Seen::Dir => {}
        Seen::Absent => return Some(Check::gone("the workspace's folder is gone")),
        Seen::Unknown(why) => return Some(Check::unknown(format!("cannot look at {why}"))),
    }
    let dot_git = path.join(".git");
    let meta = match std::fs::symlink_metadata(&dot_git) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Some(Check::gone("the folder is no longer a Git worktree"));
        }
        Err(e) => return Some(Check::unknown(format!("cannot look at .git: {e}"))),
    };
    // Where the checkout's `.git` points, resolved against the checkout (a
    // relative `gitdir:` is relative to it, whatever directory this process
    // runs in), and compared by identity. Not being able to examine either is
    // unknown; a target that is not there is a different one.
    let target = if meta.is_dir() {
        dot_git.clone()
    } else {
        let text = match std::fs::read_to_string(&dot_git) {
            Ok(text) => text,
            Err(e) => return Some(Check::unknown(format!("cannot read .git: {e}"))),
        };
        let Some(target) = text
            .lines()
            .find_map(|line| line.strip_prefix("gitdir:"))
            .map(|target| PathBuf::from(target.trim()))
        else {
            return Some(Check::unknown(".git names no Git directory".to_string()));
        };
        if target.is_relative() {
            path.join(target)
        } else {
            target
        }
    };
    let wanted = match git_dir.canonicalize() {
        Ok(wanted) => wanted,
        Err(e) => {
            return Some(Check::unknown(format!(
                "cannot look at the Git directory: {e}"
            )));
        }
    };
    match target.canonicalize() {
        Ok(found) if found == wanted => None,
        Ok(_) => Some(Check::gone(
            "the folder belongs to another Git directory now",
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Check::gone(
            "the folder belongs to another Git directory now",
        )),
        Err(e) => Some(Check::unknown(format!(
            "cannot look at where .git points: {e}"
        ))),
    }
}

/// Whether `workspace` is still the instance the forwards were taken from: the
/// project, the checkout and its Git association exist, the receipt still names
/// the same instance and app port, the machine's port ledger still gives that
/// port to it (one look, however many forwards) and, with `daemons`, each
/// daemon forward still belongs to the process it was made for. A port that
/// went back to the pool, or whose process is gone, can be given to another
/// service, and callbacks would then reach it.
fn check(workspace: &Focused, mappings: &[Mapping], probes: &Probes, daemons: bool) -> Check {
    if !workspace.project_root.as_os_str().is_empty() {
        match seen_dir(&workspace.project_root) {
            Seen::Dir => {}
            Seen::Absent => return Check::gone("the project's folder is gone"),
            Seen::Unknown(why) => return Check::unknown(format!("cannot look at {why}")),
        }
    }
    if workspace.git_dir.as_os_str().is_empty() {
        // A focus published before the forwarder looked: nothing to look at.
        return Check::default();
    }
    match seen_dir(&workspace.git_dir) {
        Seen::Dir => {}
        Seen::Absent => return Check::gone("the workspace's Git directory is gone"),
        Seen::Unknown(why) => return Check::unknown(format!("cannot look at {why}")),
    }
    if !workspace.path.as_os_str().is_empty()
        && let Some(found) = checkout(&workspace.path, &workspace.git_dir)
    {
        return found;
    }
    let receipt = match crate::receipt::Receipt::load(&workspace.git_dir) {
        Ok(Some(receipt)) => receipt,
        Ok(None) => return Check::gone("the workspace has no receipt any more"),
        Err(error) => return Check::unknown(format!("its receipt cannot be read: {error:#}")),
    };
    if receipt.instance_id != workspace.instance {
        return Check::gone("the workspace has another identity now");
    }
    if !workspace.project.is_nil() && receipt.project_id != workspace.project {
        return Check::gone("the workspace belongs to another project now");
    }
    let mut out = Check::default();
    // Each port once, whatever the number of forwards to it.
    let app: std::collections::BTreeSet<u16> = mappings
        .iter()
        .filter(|m| m.target == Target::App)
        .map(|m| m.to)
        .collect();
    if let Some(port) = app.iter().find(|port| receipt.app_port != Some(**port)) {
        return Check::gone(format!("port {port} is no longer its own port"));
    }
    if !app.is_empty() {
        let ports: Vec<u16> = app.iter().copied().collect();
        match (probes.holds)(workspace.project, workspace.instance, &ports) {
            Some(held) => {
                if let Some(at) = held.iter().position(|held| !held) {
                    return Check::gone(format!("port {} is no longer reserved for it", ports[at]));
                }
            }
            None => out.unknown = Some("the machine's port ledger cannot be read".to_string()),
        }
    }
    if daemons {
        let owned: Vec<&Mapping> = mappings
            .iter()
            .filter(|m| matches!(m.target, Target::Daemon { .. }) && m.owner.is_some())
            .collect();
        if !owned.is_empty() {
            match (probes.daemons)(workspace) {
                None => {
                    out.unknown = Some("pitchfork cannot be asked about the daemons".to_string());
                }
                Some(listed) => {
                    for mapping in owned {
                        let owner = mapping.owner.as_ref().expect("filtered on owner");
                        match daemon_changed(owner, mapping.to, &listed) {
                            Change::Same => {}
                            Change::Changed(reason) => out.dropped.push((mapping.listen, reason)),
                            Change::Unknown(why) => out.unknown = Some(why),
                        }
                    }
                }
            }
        }
    }
    out
}

/// What pitchfork's listing says about a daemon forward's process.
#[derive(Debug, PartialEq, Eq)]
enum Change {
    /// It still owns the port.
    Same,
    /// It positively does not: the process was replaced, stopped or is gone.
    Changed(String),
    /// The listing cannot tell (an entry it could not read, a state devflow
    /// does not know): nothing is concluded.
    Unknown(String),
}

/// Whether the daemon `owner` still owns `port`, judged from pitchfork's
/// listing. A positive change needs a state that is known: an unrecognized
/// status or a malformed entry is not "stopped", and a daemon missing from a
/// listing that skipped entries it could not read may be one of them.
fn daemon_changed(owner: &Owner, port: u16, listed: &crate::pitchfork::Listing) -> Change {
    let Some(daemon) = listed.daemons.iter().find(|d| d.id == owner.id) else {
        return if listed.skipped > 0 {
            Change::Unknown(format!(
                "daemon {} is not in a listing that had {} entr{} it could not read",
                owner.id,
                listed.skipped,
                if listed.skipped == 1 { "y" } else { "ies" }
            ))
        } else {
            Change::Changed(format!("daemon {} is gone", owner.id))
        };
    };
    if !daemon.state_known() {
        return Change::Unknown(format!("the state of daemon {} cannot be judged", owner.id));
    }
    if daemon.is_inactive() {
        return Change::Changed(format!("daemon {} is not running any more", owner.id));
    }
    match daemon.pid {
        Some(pid) if pid == owner.pid => {}
        Some(_) => return Change::Changed(format!("daemon {} was restarted", owner.id)),
        None => return Change::Unknown(format!("daemon {} runs without a known pid", owner.id)),
    }
    if daemon.ports.contains(&port) {
        Change::Same
    } else {
        Change::Changed(format!(
            "daemon {} does not listen on {port} any more",
            owner.id
        ))
    }
}

/// A validation running on its own thread, so that the forwarder keeps
/// answering to a stop or a new request while it waits for the ledger or for
/// pitchfork. Its result counts only for the state it was started for.
struct Job {
    handle: std::thread::JoinHandle<Check>,
    applied: u64,
}

/// [`serve`] with the machine's answers given (tests give their own).
fn serve_with(dir: &Path, probes: Arc<Probes>) -> Result<()> {
    let files = Files::at(dir);
    files.create()?;
    let lock = open_lock(&files.forwarder_lock())?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
    }
    let log = Log::open(files.log())?;
    log.line(&format!("started (pid {})", std::process::id()));
    let mut forwarder = Forwarder {
        log: log.clone(),
        live: Arc::default(),
        next_id: Arc::default(),
        listeners: BTreeMap::new(),
        probes: probes.clone(),
    };
    let mut state = State {
        pid: std::process::id(),
        started_at: Utc::now(),
        applied: 0,
        workspace: None,
        mappings: Vec::new(),
        failure: None,
    };
    files.write(files.state(), &state)?;
    let mut missing = 0u32;
    let (mut last_check, mut last_daemons) = (Instant::now(), Instant::now());
    let mut job: Option<Job> = None;
    let mut said_unknown = false;
    let end = |reason: String, name: &str| {
        log.line(&format!("ended: {reason}"));
        let _ = files.write(
            files.ended(),
            &Ended {
                at: Utc::now(),
                reason: format!("{name}: {reason}"),
            },
        );
    };
    loop {
        std::thread::sleep(POLL);
        // A finished validation is read here; a running one is left to run.
        if job.as_ref().is_some_and(|job| job.handle.is_finished()) {
            let finished = job.take().expect("a finished job");
            let judged = finished.handle.join().unwrap_or_default();
            if finished.applied == state.applied
                && let Some(workspace) = state.workspace.clone()
            {
                if let Some(reason) = judged.gone {
                    end(reason, &workspace.name);
                    break;
                }
                if let Some(why) = judged.unknown {
                    if !said_unknown {
                        log.line(&format!(
                            "cannot tell whether the focus is still valid: {why}"
                        ));
                    }
                    said_unknown = true;
                } else {
                    said_unknown = false;
                }
                if !judged.dropped.is_empty() {
                    for (listen, reason) in &judged.dropped {
                        log.line(&format!("stopped forwarding {listen}: {reason}"));
                        forwarder.remove(*listen);
                    }
                    state
                        .mappings
                        .retain(|m| !judged.dropped.iter().any(|(listen, _)| *listen == m.listen));
                    if state.mappings.is_empty() {
                        let reason = judged
                            .dropped
                            .iter()
                            .map(|(_, reason)| reason.as_str())
                            .collect::<Vec<_>>()
                            .join("; ");
                        end(reason, &workspace.name);
                        break;
                    }
                    files.write(files.state(), &state)?;
                }
            }
        }
        if job.is_none()
            && last_check.elapsed() >= VALIDATE_EVERY
            && let Some(workspace) = state.workspace.clone()
        {
            last_check = Instant::now();
            let with_daemons = last_daemons.elapsed() >= DAEMONS_EVERY;
            if with_daemons {
                last_daemons = Instant::now();
            }
            let (mappings, probes) = (state.mappings.clone(), probes.clone());
            job = Some(Job {
                applied: state.applied,
                handle: std::thread::spawn(move || {
                    check(&workspace, &mappings, &probes, with_daemons)
                }),
            });
        }
        let Some(desired) = files.read::<Desired>(files.desired()) else {
            // The file is replaced atomically, so a miss is not a torn read:
            // somebody removed the data. Do not outlive it for long.
            missing += 1;
            if missing > 50 {
                log.line("the desired state is gone; stopping");
                break;
            }
            continue;
        };
        missing = 0;
        let handled = desired.generation <= state.applied
            || state
                .failure
                .as_ref()
                .is_some_and(|f| f.generation >= desired.generation);
        if handled {
            continue;
        }
        if desired.stop {
            log.line("asked to stop");
            break;
        }
        match forwarder.apply(&desired) {
            Ok(()) => {
                state.applied = desired.generation;
                state.workspace = desired.workspace.clone();
                state.mappings = desired.mappings.clone();
                state.failure = None;
                said_unknown = false;
            }
            Err(failure) => {
                log.line(&format!("not applied: {}", failure.message));
                state.failure = Some(failure);
            }
        }
        files.write(files.state(), &state)?;
    }
    forwarder.shut_all();
    let _ = std::fs::remove_file(files.state());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::atomic::AtomicBool;

    /// A listener on an ephemeral loopback port that answers every connection
    /// with `name` and closes.
    fn server(name: &'static str) -> (u16, Arc<AtomicBool>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if flag.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(mut stream) = stream {
                    let _ = stream.write_all(name.as_bytes());
                }
            }
        });
        (port, stop)
    }

    fn free_port() -> u16 {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn ask(port: u16) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut text = String::new();
        let _ = stream.read_to_string(&mut text);
        text
    }

    fn desired(generation: u64, mappings: &[(u16, u16)]) -> Desired {
        Desired {
            generation,
            stop: false,
            workspace: None,
            mappings: mappings
                .iter()
                .map(|(listen, to)| Mapping {
                    listen: *listen,
                    target: Target::App,
                    to: *to,
                    owner: None,
                })
                .collect(),
        }
    }

    fn wait_for(files: &Files, generation: u64) -> State {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(state) = files.read::<State>(files.state())
                && (state.applied >= generation
                    || state
                        .failure
                        .as_ref()
                        .is_some_and(|f| f.generation >= generation))
            {
                return state;
            }
            assert!(Instant::now() < deadline, "the forwarder did not answer");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The forwarder released its lock. A lock goes when the last duplicate of
    /// its descriptor closes, and a process another test is spawning can hold
    /// one for a few milliseconds.
    fn stopped(files: &Files) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while files.running() {
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    fn start(dir: &Path) -> std::thread::JoinHandle<()> {
        let dir = dir.to_path_buf();
        std::thread::spawn(move || serve(&dir).unwrap())
    }

    #[test]
    fn ports_are_read_with_their_target() {
        assert_eq!(
            PortSpec::parse("3000").unwrap(),
            PortSpec {
                listen: 3000,
                target: Target::App
            }
        );
        assert_eq!(
            PortSpec::parse("8080=web").unwrap().target,
            Target::Daemon { name: "web".into() }
        );
        assert_eq!(PortSpec::parse("8080=app").unwrap().target, Target::App);
        assert!(PortSpec::parse("80").is_err());
        assert!(PortSpec::parse("web").is_err());
    }

    #[test]
    fn the_forwarder_carries_bytes_and_repoints_without_dropping_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let files = Files::at(dir.path());
        let (one, stop_one) = server("one");
        let (two, stop_two) = server("two");
        let listen = free_port();
        let handle = start(dir.path());
        // Wait until the forwarder holds its lock and wrote its state.
        wait_for(&files, 0);
        assert!(files.running());

        files
            .write(files.desired(), &desired(1, &[(listen, one)]))
            .unwrap();
        wait_for(&files, 1);
        assert_eq!(ask(listen), "one");

        // Re-point: the same listening socket now reaches the other server.
        files
            .write(files.desired(), &desired(2, &[(listen, two)]))
            .unwrap();
        wait_for(&files, 2);
        assert_eq!(ask(listen), "two");

        // Not listening on anything but loopback.
        let state: State = files.read(files.state()).unwrap();
        assert_eq!(state.mappings[0].to, two);

        // Stop.
        files
            .write(
                files.desired(),
                &Desired {
                    generation: 3,
                    stop: true,
                    workspace: None,
                    mappings: Vec::new(),
                },
            )
            .unwrap();
        handle.join().unwrap();
        assert!(stopped(&files));
        assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, listen)).is_err());
        stop_one.store(true, Ordering::SeqCst);
        stop_two.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, one));
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, two));
    }

    #[test]
    fn a_repoint_closes_the_connections_to_the_old_target() {
        let dir = tempfile::tempdir().unwrap();
        let files = Files::at(dir.path());
        // Two servers that hold a connection open until the peer goes.
        let quiet = |name: &'static str| {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    std::thread::spawn(move || {
                        let mut stream = stream;
                        let _ = stream.write_all(name.as_bytes());
                        let mut sink = [0u8; 16];
                        while stream.read(&mut sink).is_ok_and(|n| n > 0) {}
                    });
                }
            });
            port
        };
        let (old, new) = (quiet("old"), quiet("new"));
        let listen = free_port();
        let handle = start(dir.path());
        wait_for(&files, 0);
        files
            .write(files.desired(), &desired(1, &[(listen, old)]))
            .unwrap();
        wait_for(&files, 1);

        let mut held = TcpStream::connect((Ipv4Addr::LOCALHOST, listen)).unwrap();
        held.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut greeting = [0u8; 3];
        held.read_exact(&mut greeting).unwrap();
        assert_eq!(&greeting, b"old");

        files
            .write(files.desired(), &desired(2, &[(listen, new)]))
            .unwrap();
        wait_for(&files, 2);
        // The old connection ends (a reconnect is all it costs)...
        let mut rest = Vec::new();
        assert!(held.read_to_end(&mut rest).is_ok());
        assert!(rest.is_empty());
        // ... and a new one reaches the new target.
        let mut again = TcpStream::connect((Ipv4Addr::LOCALHOST, listen)).unwrap();
        again
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut greeting = [0u8; 3];
        again.read_exact(&mut greeting).unwrap();
        assert_eq!(&greeting, b"new");

        files
            .write(
                files.desired(),
                &Desired {
                    generation: 3,
                    stop: true,
                    workspace: None,
                    mappings: Vec::new(),
                },
            )
            .unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn a_port_in_use_is_reported_and_nothing_changes() {
        let dir = tempfile::tempdir().unwrap();
        let files = Files::at(dir.path());
        let (target, _stop) = server("x");
        let (listen, taken) = (free_port(), free_port());
        let _occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, taken)).unwrap();
        let handle = start(dir.path());
        wait_for(&files, 0);
        files
            .write(files.desired(), &desired(1, &[(listen, target)]))
            .unwrap();
        wait_for(&files, 1);

        files
            .write(
                files.desired(),
                &desired(2, &[(listen, target), (taken, target)]),
            )
            .unwrap();
        let state = wait_for(&files, 2);
        let failure = state.failure.unwrap();
        assert_eq!(failure.port, Some(taken));
        assert!(failure.in_use);
        // What was in force before still is.
        assert_eq!(state.applied, 1);
        assert_eq!(ask(listen), "x");
        assert!(matches!(
            failure_error(&failure).downcast_ref::<PortInUse>(),
            Some(PortInUse { port }) if *port == taken
        ));

        files
            .write(
                files.desired(),
                &Desired {
                    generation: 3,
                    stop: true,
                    workspace: None,
                    mappings: Vec::new(),
                },
            )
            .unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn it_never_forwards_to_a_port_it_was_not_told_about() {
        let dir = tempfile::tempdir().unwrap();
        let files = Files::at(dir.path());
        let (a, b) = (free_port(), free_port());
        let handle = start(dir.path());
        wait_for(&files, 0);
        // A forward to one of its own ports, to port zero, to a low listen
        // port, and the same listen port twice: all refused, none bound.
        for (generation, mappings) in [
            (1, vec![(a, b), (b, a)]),
            (2, vec![(a, 0)]),
            (3, vec![(80, 9000)]),
            (4, vec![(a, 9000), (a, 9001)]),
        ] {
            files
                .write(files.desired(), &desired(generation, &mappings))
                .unwrap();
            let state = wait_for(&files, generation);
            assert_eq!(state.applied, 0, "{mappings:?}");
            assert!(state.failure.is_some(), "{mappings:?}");
        }
        assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, a)).is_err());
        files
            .write(
                files.desired(),
                &Desired {
                    generation: 9,
                    stop: true,
                    workspace: None,
                    mappings: Vec::new(),
                },
            )
            .unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn a_removal_ends_the_forwards_to_its_instance_and_only_those() {
        let project = uuid::Uuid::new_v4();
        let (instance, other) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let files = Files::of_id(project).unwrap();
        files.create().unwrap();
        let (target, _stop) = server("x");
        let listen = free_port();
        let handle = start(&files.dir);
        wait_for(&files, 0);
        let mut want = desired(1, &[(listen, target)]);
        want.workspace = Some(Focused {
            name: "feature".into(),
            path: "/work/feature".into(),
            instance,
            ..Default::default()
        });
        files.write(files.desired(), &want).unwrap();
        wait_for(&files, 1);
        assert_eq!(ask(listen), "x");

        // Another instance of this project, and this instance in another
        // project: not this focus's.
        assert_eq!(release_instance(project, other).unwrap(), None);
        assert_eq!(
            release_instance(uuid::Uuid::new_v4(), instance).unwrap(),
            None
        );
        assert!(files.running());
        assert_eq!(ask(listen), "x");

        // The instance itself: its forwards go, and with none left the
        // forwarder stops.
        let released = release_instance(project, instance).unwrap().unwrap();
        assert_eq!(released.workspace, "feature");
        assert_eq!(released.ports, [listen]);
        assert!(released.stopped);
        handle.join().unwrap();
        assert!(stopped(&files));
        assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, listen)).is_err());
        // Nothing running: nothing to release.
        assert_eq!(release_instance(project, instance).unwrap(), None);
    }

    #[test]
    fn a_second_forwarder_does_not_start_beside_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let files = Files::at(dir.path());
        let handle = start(dir.path());
        wait_for(&files, 0);
        // Returns at once without touching the state of the one that runs.
        serve(dir.path()).unwrap();
        assert!(files.running());
        files
            .write(
                files.desired(),
                &Desired {
                    generation: 1,
                    stop: true,
                    workspace: None,
                    mappings: Vec::new(),
                },
            )
            .unwrap();
        handle.join().unwrap();
        assert!(stopped(&files));
    }

    // --- the forwarder looks again at what it forwards to ----------------------

    /// A workspace with a receipt on disk, a checkout, a server on its app port
    /// and a forwarder (running in a thread) pointed at it; optionally also a
    /// daemon forward, with a listing of pitchfork the test controls.
    struct Rig {
        _dir: tempfile::TempDir,
        git_dir: PathBuf,
        work: PathBuf,
        project_root: PathBuf,
        files: Files,
        listen: u16,
        daemon_listen: u16,
        port: u16,
        project: uuid::Uuid,
        /// What the port ledger answers.
        held: Arc<AtomicBool>,
        /// What pitchfork lists (`None`: it cannot be asked).
        listed: Arc<Mutex<Option<Vec<crate::pitchfork::Daemon>>>>,
        /// How many entries of that listing pitchfork could not read.
        skipped: Arc<AtomicUsize>,
        /// How many times, and for how many ports at most, the ledger was asked.
        asked: Arc<Mutex<Vec<usize>>>,
        /// How long the ledger takes to answer.
        slow: Arc<Mutex<Duration>>,
        handle: Option<std::thread::JoinHandle<()>>,
        _stop: Arc<AtomicBool>,
    }

    fn daemon(id: &str, status: &str, pid: u32, port: u16) -> crate::pitchfork::Daemon {
        crate::pitchfork::Daemon {
            id: id.to_string(),
            namespace: id.split('/').next().unwrap().to_string(),
            name: id.split('/').nth(1).unwrap().to_string(),
            status: status.to_string(),
            pid: Some(pid),
            ports: vec![port],
            url: None,
            error: None,
            malformed: None,
        }
    }

    fn rig(holds: bool) -> Rig {
        rig_with(holds, false)
    }

    fn rig_with(holds: bool, with_daemon: bool) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (git_dir, project_root, work) =
            (root.join("git"), root.join("project"), root.join("work"));
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        // A linked worktree: its `.git` is a file that names the Git directory.
        std::fs::write(
            work.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();
        let (port, stop) = server("x");
        let (daemon_port, _daemon_stop) = server("d");
        let project = uuid::Uuid::new_v4();
        let mut receipt = crate::receipt::Receipt::new(project, "feature", None);
        receipt.app_port = Some(port);
        receipt.save(&git_dir).unwrap();
        let files = Files::of_id(project).unwrap();
        files.create().unwrap();

        let held = Arc::new(AtomicBool::new(holds));
        let listed = Arc::new(Mutex::new(Some(vec![daemon(
            "ns/web",
            "running",
            100,
            daemon_port,
        )])));
        let skipped = Arc::new(AtomicUsize::new(0));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let slow = Arc::new(Mutex::new(Duration::ZERO));
        let probes = Arc::new(Probes {
            holds: {
                let (held, asked, slow) = (held.clone(), asked.clone(), slow.clone());
                Box::new(move |_, _, ports| {
                    asked.lock().unwrap().push(ports.len());
                    std::thread::sleep(*slow.lock().unwrap());
                    Some(vec![held.load(Ordering::SeqCst); ports.len()])
                })
            },
            daemons: {
                let (listed, skipped) = (listed.clone(), skipped.clone());
                Box::new(move |_| {
                    listed
                        .lock()
                        .unwrap()
                        .clone()
                        .map(|daemons| crate::pitchfork::Listing {
                            daemons,
                            skipped: skipped.load(Ordering::SeqCst),
                        })
                })
            },
        });
        let dir_for_thread = files.dir.clone();
        let handle = std::thread::spawn(move || serve_with(&dir_for_thread, probes).unwrap());
        wait_for(&files, 0);
        let (listen, daemon_listen) = (free_port(), free_port());
        let mut want = desired(1, &[(listen, port)]);
        if with_daemon {
            want.mappings.push(Mapping {
                listen: daemon_listen,
                target: Target::Daemon { name: "web".into() },
                to: daemon_port,
                owner: Some(Owner {
                    id: "ns/web".into(),
                    pid: 100,
                }),
            });
        }
        want.workspace = Some(Focused {
            name: "feature".into(),
            path: work.clone(),
            instance: receipt.instance_id,
            project,
            project_root: project_root.clone(),
            git_dir: git_dir.clone(),
        });
        files.write(files.desired(), &want).unwrap();
        wait_for(&files, 1);
        assert_eq!(ask(listen), "x");
        Rig {
            _dir: dir,
            git_dir,
            work,
            project_root,
            files,
            listen,
            daemon_listen,
            port,
            project,
            held,
            listed,
            skipped,
            asked,
            slow,
            handle: Some(handle),
            _stop: stop,
        }
    }

    impl Rig {
        /// The forwarder ended by itself, saying why.
        fn ended(&mut self) -> String {
            self.handle.take().unwrap().join().unwrap();
            assert!(stopped(&self.files));
            assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, self.listen)).is_err());
            self.files.read::<Ended>(self.files.ended()).unwrap().reason
        }

        fn still_forwarding(&self) {
            std::thread::sleep(VALIDATE_EVERY * 4);
            assert!(self.files.running());
            assert_eq!(ask(self.listen), "x");
        }

        fn end(mut self) {
            self.files
                .write(
                    self.files.desired(),
                    &Desired {
                        generation: 99,
                        stop: true,
                        workspace: None,
                        mappings: Vec::new(),
                    },
                )
                .unwrap();
            self.handle.take().unwrap().join().unwrap();
        }
    }

    #[test]
    fn a_new_identity_for_the_workspace_ends_the_forwarding() {
        let mut rig = rig(true);
        // `setup --new-instance` elsewhere: the receipt names another instance,
        // and the old port may now go to someone else.
        let mut receipt = crate::receipt::Receipt::new(rig.project, "feature", None);
        receipt.app_port = Some(rig.port);
        receipt.save(&rig.git_dir).unwrap();
        let reason = rig.ended();
        assert!(reason.contains("another identity"), "{reason}");
    }

    #[test]
    fn a_port_that_is_no_longer_the_workspaces_ends_the_forwarding() {
        // Its receipt moved to another port.
        let mut rig = rig(true);
        let mut receipt = crate::receipt::Receipt::load(&rig.git_dir)
            .unwrap()
            .unwrap();
        receipt.app_port = Some(rig.port.wrapping_add(1));
        receipt.save(&rig.git_dir).unwrap();
        assert!(rig.ended().contains("no longer its own port"));

        // The machine's ledger no longer gives the port to it.
        let mut rig = self::rig(true);
        rig.held.store(false, Ordering::SeqCst);
        assert!(rig.ended().contains("no longer reserved"));

        // A request about a port the ledger does not give to the workspace is
        // not even applied.
        let rig = self::rig(true);
        rig.held.store(false, Ordering::SeqCst);
        let mut again = desired(2, &[(free_port(), rig.port)]);
        again.workspace = rig
            .files
            .read::<State>(rig.files.state())
            .unwrap()
            .workspace;
        rig.files.write(rig.files.desired(), &again).unwrap();
        // (Whichever comes first, the periodic look or the request, the old
        // forwarding does not outlive the ledger's answer.)
        let deadline = Instant::now() + Duration::from_secs(5);
        while rig.files.running() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn a_receipt_that_is_gone_or_a_project_that_is_deleted_ends_the_forwarding() {
        let mut rig = rig(true);
        std::fs::remove_file(crate::receipt::Receipt::path(&rig.git_dir)).unwrap();
        assert!(rig.ended().contains("no receipt"));

        let mut rig = self::rig(true);
        std::fs::remove_dir_all(&rig.project_root).unwrap();
        assert!(rig.ended().contains("project's folder is gone"));

        let mut rig = self::rig(true);
        std::fs::remove_dir_all(&rig.git_dir).unwrap();
        assert!(rig.ended().contains("Git directory is gone"));
    }

    #[test]
    fn what_cannot_be_read_is_not_a_reason_to_stop_forwarding() {
        let rig = rig(true);
        // A receipt that cannot be read says nothing about who owns the port.
        std::fs::write(crate::receipt::Receipt::path(&rig.git_dir), "{ not json").unwrap();
        rig.still_forwarding();
        rig.end();
    }

    #[test]
    fn a_stop_that_is_not_answered_sends_no_signal_to_the_recorded_pid() {
        let files = Files::of_id(uuid::Uuid::new_v4()).unwrap();
        files.create().unwrap();
        // A forwarder that holds its lock and never answers, whose recorded
        // pid is this very process: a signal to it would end the test.
        let lock = open_lock(&files.forwarder_lock()).unwrap();
        lock.try_lock().unwrap();
        files
            .write(
                files.state(),
                &State {
                    pid: std::process::id(),
                    started_at: Utc::now(),
                    applied: 1,
                    workspace: None,
                    mappings: Vec::new(),
                    failure: None,
                },
            )
            .unwrap();
        let error = stop_locked(&files).unwrap_err();
        assert!(
            error.to_string().contains("may still be forwarding"),
            "{error:#}"
        );
        assert!(files.running(), "nothing ended it");
    }

    #[test]
    fn a_connection_made_while_the_route_changes_is_closed_not_kept() {
        // The seam: a connection accepted under generation 0 whose upstream is
        // connected after the route moved (a re-point, a shutdown) must not
        // register or carry bytes.
        let (target, _stop) = server("old");
        let route: Shared = Arc::new(Mutex::new(Route {
            to: target,
            generation: 0,
            closed: false,
            conns: Vec::new(),
        }));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client_side = TcpStream::connect(addr).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        let carry = Carry {
            target,
            generation: 0,
            route: route.clone(),
            log: Log::open(tempfile::tempdir().unwrap().keep().join("log")).unwrap(),
            live: Arc::new(AtomicUsize::new(1)),
            id: 1,
        };
        // The listener is re-pointed before the connection registers.
        let moved = Listener {
            addr,
            route: route.clone(),
            mapping: Mapping {
                listen: addr.port(),
                target: Target::App,
                to: target,
                owner: None,
            },
        };
        moved.reroute(Some(1));
        carry.run(accepted);
        assert!(route.lock().unwrap().conns.is_empty());
        client_side
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut text = String::new();
        let _ = client_side.read_to_string(&mut text);
        assert_eq!(text, "", "no byte of the old target reached the client");
    }

    // --- round 2 ---------------------------------------------------------------

    #[test]
    fn deleting_only_the_checkout_or_unlinking_it_ends_the_forwarding() {
        // The folder goes; the Git directory and the receipt stay (a host
        // deleted the worktree directory only).
        let mut rig = rig(true);
        std::fs::remove_dir_all(&rig.work).unwrap();
        assert!(rig.ended().contains("folder is gone"));

        // The folder stays but is no longer a worktree...
        let mut rig = self::rig(true);
        std::fs::remove_file(rig.work.join(".git")).unwrap();
        assert!(rig.ended().contains("no longer a Git worktree"));

        // ... or belongs to another Git directory.
        let mut rig = self::rig(true);
        let other = rig.work.parent().unwrap().join("elsewhere");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            rig.work.join(".git"),
            format!("gitdir: {}\n", other.display()),
        )
        .unwrap();
        assert!(rig.ended().contains("another Git directory"));
    }

    #[test]
    fn a_checkout_that_cannot_be_looked_at_is_unknown_not_gone() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join("git");
        std::fs::create_dir_all(&git_dir).unwrap();
        // A path under a regular file cannot be examined as a directory: not a
        // statement that the checkout is gone.
        let file = dir.path().join("file");
        std::fs::write(&file, "x").unwrap();
        let found = checkout(&file.join("below"), &git_dir).unwrap();
        assert!(found.gone.is_none(), "{found:?}");
        assert!(found.unknown.is_some(), "{found:?}");
    }

    #[test]
    fn a_daemon_forward_ends_when_the_daemon_is_not_the_process_it_was_made_for() {
        let daemon_listen =
            |rig: &Rig| TcpStream::connect((Ipv4Addr::LOCALHOST, rig.daemon_listen));
        let ports = |rig: &Rig| rig.files.read::<State>(rig.files.state()).unwrap().mappings;

        // Restarted: another pid. Only that forward ends; the app forward
        // goes on.
        let rig = rig_with(true, true);
        assert_eq!(ask(rig.daemon_listen), "d");
        let port = ports(&rig)[1].to;
        *rig.listed.lock().unwrap() = Some(vec![daemon("ns/web", "running", 101, port)]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while daemon_listen(&rig).is_ok() {
            assert!(Instant::now() < deadline, "the daemon forward should end");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(ask(rig.listen), "x", "the app forward is untouched");
        assert_eq!(ports(&rig).len(), 1);
        rig.end();

        // Not running any more, listening elsewhere, or gone from the listing:
        // the same.
        for listing in [
            vec![daemon("ns/web", "stopped", 100, 1)],
            vec![daemon("ns/web", "running", 100, 1)],
            vec![],
        ] {
            let rig = self::rig_with(true, true);
            *rig.listed.lock().unwrap() = Some(listing);
            let deadline = Instant::now() + Duration::from_secs(5);
            while daemon_listen(&rig).is_ok() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(50));
            }
            rig.end();
        }

        // When pitchfork cannot be asked, nothing is concluded.
        let rig = self::rig_with(true, true);
        *rig.listed.lock().unwrap() = None;
        std::thread::sleep(DAEMONS_EVERY * 3);
        assert_eq!(ask(rig.daemon_listen), "d");
        rig.end();

        // With every forward gone the forwarder ends by itself.
        let mut rig = self::rig_with(true, true);
        let mut only_daemon = desired(2, &[]);
        only_daemon.workspace = rig
            .files
            .read::<State>(rig.files.state())
            .unwrap()
            .workspace;
        only_daemon.mappings = ports(&rig)
            .into_iter()
            .filter(|m| m.target != Target::App)
            .collect();
        rig.files.write(rig.files.desired(), &only_daemon).unwrap();
        wait_for(&rig.files, 2);
        *rig.listed.lock().unwrap() = Some(vec![]);
        let reason = rig.ended();
        assert!(reason.contains("daemon ns/web is gone"), "{reason}");
    }

    #[test]
    fn a_daemon_that_does_not_run_is_not_forwarded_to() {
        // `resolve` binds a daemon forward to the process it found: without one
        // (not running, no pid) there is nothing for the port to belong to.
        let owner = Owner {
            id: "ns/web".into(),
            pid: 100,
        };
        let listing = |daemons: Vec<crate::pitchfork::Daemon>, skipped| crate::pitchfork::Listing {
            daemons,
            skipped,
        };
        let running = listing(vec![daemon("ns/web", "running", 100, 4000)], 0);
        assert_eq!(daemon_changed(&owner, 4000, &running), Change::Same);
        assert!(matches!(
            daemon_changed(&owner, 4001, &running),
            Change::Changed(why) if why.contains("does not listen")
        ));
        let restarted = listing(vec![daemon("ns/web", "running", 7, 4000)], 0);
        assert!(matches!(
            daemon_changed(&owner, 4000, &restarted),
            Change::Changed(why) if why.contains("restarted")
        ));
        let stopped = listing(vec![daemon("ns/web", "stopped", 100, 4000)], 0);
        assert!(matches!(
            daemon_changed(&owner, 4000, &stopped),
            Change::Changed(_)
        ));
        assert!(matches!(
            daemon_changed(&owner, 4000, &listing(vec![], 0)),
            Change::Changed(why) if why.contains("is gone")
        ));

        // Uncertain output concludes nothing: a status devflow does not know
        // is not "stopped", an entry that could not be read may have been this
        // daemon, a malformed entry cannot be judged.
        for status in ["paused", "unknown", "zombie", "runningish"] {
            let odd = listing(vec![daemon("ns/web", status, 100, 4000)], 0);
            assert!(
                matches!(daemon_changed(&owner, 4000, &odd), Change::Unknown(_)),
                "{status}"
            );
        }
        assert!(matches!(
            daemon_changed(&owner, 4000, &listing(vec![], 1)),
            Change::Unknown(_)
        ));
        let mut malformed = daemon("ns/web", "stopped", 100, 4000);
        malformed.malformed = Some("pid is not a number".into());
        assert!(matches!(
            daemon_changed(&owner, 4000, &listing(vec![malformed], 0)),
            Change::Unknown(_)
        ));
    }

    #[test]
    fn uncertain_pitchfork_output_leaves_a_healthy_forward_alone() {
        let rig = rig_with(true, true);
        let port = rig.files.read::<State>(rig.files.state()).unwrap().mappings[1].to;
        // An unrecognized status, then a listing with an unreadable entry in
        // which the daemon is not to be found.
        *rig.listed.lock().unwrap() = Some(vec![daemon("ns/web", "paused", 100, port)]);
        std::thread::sleep(DAEMONS_EVERY * 3);
        assert_eq!(ask(rig.daemon_listen), "d");
        *rig.listed.lock().unwrap() = Some(vec![]);
        rig.skipped.store(1, Ordering::SeqCst);
        std::thread::sleep(DAEMONS_EVERY * 3);
        assert_eq!(ask(rig.daemon_listen), "d");
        assert_eq!(ask(rig.listen), "x");
        // The same listing without the unreadable entry is a disappearance.
        rig.skipped.store(0, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(5);
        while TcpStream::connect((Ipv4Addr::LOCALHOST, rig.daemon_listen)).is_ok() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(50));
        }
        rig.end();
    }

    #[test]
    fn a_relative_gitdir_is_resolved_against_the_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (git_dir, work) = (root.join("git"), root.join("trees").join("work"));
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        // The process runs somewhere else entirely (the cargo test directory):
        // only the checkout can give `../../git` a meaning.
        std::fs::write(work.join(".git"), "gitdir: ../../git\n").unwrap();
        assert!(checkout(&work, &git_dir).is_none());

        // A relative path to somewhere else is another Git directory.
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::write(work.join(".git"), "gitdir: ../../other\n").unwrap();
        let found = checkout(&work, &git_dir).unwrap();
        assert!(found.gone.unwrap().contains("another Git directory"));

        // One that leads nowhere is not this one either; one that cannot be
        // examined is unknown.
        std::fs::write(work.join(".git"), "gitdir: ../../nowhere\n").unwrap();
        assert!(checkout(&work, &git_dir).unwrap().gone.is_some());
        std::fs::write(work.join(".git"), "nothing useful\n").unwrap();
        let found = checkout(&work, &git_dir).unwrap();
        assert!(found.gone.is_none() && found.unknown.is_some());
        // A Git directory that cannot be examined is not a gone checkout.
        let file = root.join("file");
        std::fs::write(&file, "x").unwrap();
        std::fs::write(work.join(".git"), "gitdir: ../../git\n").unwrap();
        let found = checkout(&work, &file.join("below")).unwrap();
        assert!(found.gone.is_none() && found.unknown.is_some(), "{found:?}");
    }

    #[test]
    fn a_running_forwarder_without_a_state_refuses_the_release() {
        let project = uuid::Uuid::new_v4();
        let files = Files::of_id(project).unwrap();
        files.create().unwrap();
        let lock = open_lock(&files.forwarder_lock()).unwrap();
        lock.try_lock().unwrap();
        // A request for B that was refused is still in desired.json, and the
        // state that said A is applied has disappeared.
        let b = uuid::Uuid::new_v4();
        let mut asked = desired(3, &[]);
        asked.workspace = Some(Focused {
            name: "b".into(),
            instance: b,
            ..Default::default()
        });
        files.write(files.desired(), &asked).unwrap();
        assert!(!files.state().exists());
        let error = release_instance(project, uuid::Uuid::new_v4()).unwrap_err();
        assert!(error.to_string().contains("state is missing"), "{error:#}");
        let error = release_instance(project, b).unwrap_err();
        assert!(error.to_string().contains("state is missing"), "{error:#}");
        // Not running: nothing to release.
        drop(lock);
        assert!(release_instance(project, b).unwrap().is_none());
    }

    #[test]
    fn however_many_forwards_the_ports_are_looked_up_once_per_cycle() {
        let mut rig = rig(true);
        // Three forwards to the same app port.
        let mut want = desired(
            2,
            &[
                (rig.listen, rig.port),
                (free_port(), rig.port),
                (free_port(), rig.port),
            ],
        );
        want.workspace = rig
            .files
            .read::<State>(rig.files.state())
            .unwrap()
            .workspace;
        rig.files.write(rig.files.desired(), &want).unwrap();
        wait_for(&rig.files, 2);
        rig.asked.lock().unwrap().clear();
        std::thread::sleep(VALIDATE_EVERY * 5);
        let asked = rig.asked.lock().unwrap().clone();
        assert!(!asked.is_empty());
        assert!(asked.iter().all(|ports| *ports == 1), "{asked:?}");
        let _ = rig.handle.take();
        rig.files
            .write(
                rig.files.desired(),
                &Desired {
                    generation: 9,
                    stop: true,
                    workspace: None,
                    mappings: Vec::new(),
                },
            )
            .unwrap();
        assert!(stopped(&rig.files));
    }

    #[test]
    fn a_stop_is_answered_while_a_validation_is_slow() {
        let rig = rig(true);
        // The ledger takes longer than the stop is willing to wait.
        *rig.slow.lock().unwrap() = Duration::from_secs(6);
        std::thread::sleep(VALIDATE_EVERY * 3);
        let started = Instant::now();
        stop_locked(&rig.files).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "stopping waited for the validation: {:?}",
            started.elapsed()
        );
        assert!(!rig.files.running());
    }

    #[test]
    fn a_request_for_a_workspace_that_is_gone_is_not_applied() {
        let rig = rig(true);
        let mut late = desired(2, &[(free_port(), rig.port)]);
        late.workspace = Some(Focused {
            name: "gone".into(),
            path: rig.work.clone(),
            instance: uuid::Uuid::new_v4(),
            project: rig.project,
            project_root: rig.project_root.clone(),
            git_dir: rig.git_dir.clone(),
        });
        rig.files.write(rig.files.desired(), &late).unwrap();
        let state = wait_for(&rig.files, 2);
        assert_eq!(state.applied, 1, "what was in force stays");
        assert!(state.failure.unwrap().message.contains("another identity"));
        assert_eq!(ask(rig.listen), "x");
        rig.end();
    }

    // --- release under the control lock, over both targets ----------------------

    fn focused_for(rig: &Rig, instance: uuid::Uuid) -> Focused {
        Focused {
            name: "other".into(),
            path: rig.work.clone(),
            instance,
            project: rig.project,
            // Not there: the forwarder will refuse to apply a request for it.
            project_root: rig.project_root.join("not-there"),
            git_dir: rig.git_dir.clone(),
        }
    }

    #[test]
    fn a_removal_cancels_an_outstanding_request_for_the_removed_workspace() {
        // The state points at A; a request for B timed out and is still in
        // desired.json. Removing B must not leave it behind.
        let rig = rig(true);
        let project = rig.project;
        let b = uuid::Uuid::new_v4();
        let mut asked = desired(5, &[(free_port(), rig.port)]);
        asked.workspace = Some(focused_for(&rig, b));
        rig.files.write(rig.files.desired(), &asked).unwrap();
        // Applied or refused, it is B's request that stands in desired.json.
        let released = release_instance(project, b).unwrap().unwrap();
        assert_eq!(released.workspace, "other");
        assert!(!released.stopped, "A's focus stays");
        let now = rig.files.read::<Desired>(rig.files.desired()).unwrap();
        assert_eq!(
            now.workspace.as_ref().map(|w| w.name.as_str()),
            Some("feature")
        );
        assert!(now.generation > 5);
        assert_eq!(ask(rig.listen), "x");
        // Another instance is not matched.
        assert!(
            release_instance(project, uuid::Uuid::new_v4())
                .unwrap()
                .is_none()
        );
        rig.end();
    }

    #[test]
    fn a_running_forwarder_whose_state_cannot_be_read_refuses_the_release() {
        let project = uuid::Uuid::new_v4();
        let files = Files::of_id(project).unwrap();
        files.create().unwrap();
        let lock = open_lock(&files.forwarder_lock()).unwrap();
        lock.try_lock().unwrap();
        std::fs::write(files.state(), "{ not json").unwrap();
        let error = release_instance(project, uuid::Uuid::new_v4()).unwrap_err();
        assert!(error.to_string().contains("cannot be read"), "{error:#}");
    }

    #[test]
    fn whether_it_runs_is_judged_under_the_control_lock() {
        let project = uuid::Uuid::new_v4();
        let files = Files::of_id(project).unwrap();
        files.create().unwrap();
        // A focus is being published: it holds the control lock.
        let _publishing = control(&files).unwrap();
        // The release waits for it (and gives up with the way on) instead of
        // concluding from a look made before the publication that nothing runs.
        let error = release_instance(project, uuid::Uuid::new_v4()).unwrap_err();
        assert!(
            error.to_string().contains("changing the focus"),
            "{error:#}"
        );
    }

    // --- the listener's terminal state -------------------------------------------

    #[test]
    fn nothing_is_admitted_or_registered_after_a_listener_is_shut() {
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = held.local_addr().unwrap();
        let route: Shared = Arc::new(Mutex::new(Route {
            to: 1,
            generation: 0,
            closed: false,
            conns: Vec::new(),
        }));
        let listener = Listener {
            addr,
            route: route.clone(),
            mapping: Mapping {
                listen: addr.port(),
                target: Target::App,
                to: 1,
                owner: None,
            },
        };
        // A connection admitted before the shutdown...
        let (target, generation) = admit(&route).unwrap();
        listener.shut();
        // ... cannot be admitted again, and cannot register under the new
        // generation either (it would be a connection of a closed listener).
        assert_eq!(admit(&route), None);
        let (server_side, _) = {
            let client = TcpStream::connect(addr).unwrap();
            let (accepted, _) = held.accept().unwrap();
            (accepted, client)
        };
        let carry = Carry {
            target,
            generation,
            route: route.clone(),
            log: Log::open(tempfile::tempdir().unwrap().keep().join("log")).unwrap(),
            live: Arc::new(AtomicUsize::new(1)),
            id: 7,
        };
        carry.run(server_side);
        assert!(route.lock().unwrap().conns.is_empty());
        // Even a connection that read the *new* generation after the shutdown
        // (what the old flag-then-generation order allowed) is refused.
        let late = Carry {
            target,
            generation: route.lock().unwrap().generation,
            route: route.clone(),
            log: Log::open(tempfile::tempdir().unwrap().keep().join("log")).unwrap(),
            live: Arc::new(AtomicUsize::new(1)),
            id: 8,
        };
        let (accepted, _client) = {
            let client = TcpStream::connect(addr).unwrap();
            let (accepted, _) = held.accept().unwrap();
            (accepted, client)
        };
        late.run(accepted);
        assert!(route.lock().unwrap().conns.is_empty());
    }
}

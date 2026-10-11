//! The `pitchfork` CLI (https://pitchfork.jdx.dev) as devflow uses it.
//!
//! devflow does not supervise processes. This module finds pitchfork, checks
//! that the installed build has the features devflow calls, and wraps the few
//! commands it runs: `list --json`, `proxy status --json`,
//! `supervisor status --json`, and `start` and `stop` with fully qualified
//! daemon ids (`namespace/name`). What may be started or stopped is decided one
//! level up, in [`crate::processes`].
//!
//! devflow never starts or stops with `--local`: pitchfork's `--local` selects
//! the non-global daemons of every configuration file from the working
//! directory up to the filesystem root, so a worktree inside another project
//! would start and stop that project's daemons too. Explicit qualified ids name
//! exactly the daemons of one workspace's own configuration.
//!
//! Every call goes through a [`Pitchfork`] handle, so tests point it at a fake
//! executable instead of changing `PATH`. Queries run with pitchfork's
//! supervisor auto-start switched off (`PITCHFORK_SUPERVISOR_AUTO_START=false`):
//! looking at processes never starts a supervisor. Only an explicit start does,
//! as pitchfork itself does for `pitchfork start`.
use crate::subprocess::{self, Limits, Output};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a query (`--version`, `list --json`, …) may take.
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
/// `--version` through a mise shim can be slow the first time.
const VERSION_TIMEOUT: Duration = Duration::from_secs(15);
/// `--help` of a subcommand.
const HELP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long [`Pitchfork::detect_cached`] trusts a detection.
pub const DETECTION_TTL: Duration = Duration::from_secs(10);
/// What [`Pitchfork::detect_cached`] remembers, per executable.
static DETECTIONS: Mutex<Vec<(PathBuf, Instant, Detection)>> = Mutex::new(Vec::new());

/// Forget every remembered detection: the next one asks pitchfork again. For
/// after pitchfork was installed or changed, and for tests that change a fake.
pub fn forget_detections() {
    if let Ok(mut cache) = DETECTIONS.lock() {
        cache.clear();
    }
}

/// What devflow calls, as `(name shown, arguments of its --help, flag the help
/// must mention)`. A missing feature is reported by name.
const FEATURES: [(&str, &[&str], &str); 2] = [
    ("list --json", &["list", "--help"], "--json"),
    (
        "proxy status --json",
        &["proxy", "status", "--help"],
        "--json",
    ),
];

/// The feature that lets `start` and `stop` take `namespace/name` ids. Its help
/// text does not say so, so it is judged on the version: daemon ids carry a
/// namespace since pitchfork 2.0.0 (an id with a `/` is parsed as a full id by
/// `PitchforkToml::resolve_ids_and_group`: read in the sources of 2.0.0, 2.14.0,
/// 2.29.0 and 2.30.1, and run against 2.25.0 and 2.29.0).
pub const QUALIFIED_IDS: &str = "start and stop by namespace/name (pitchfork 2.0 or later)";

/// The commands that turn the pitchfork proxy on, in order. devflow prints them
/// and never runs them: `proxy setup` changes system DNS and trust and uses sudo.
pub const PROXY_COMMANDS: [&str; 4] = [
    "pitchfork settings set proxy.enable true --global",
    "pitchfork supervisor stop && pitchfork supervisor start",
    "pitchfork proxy setup --dry-run   # preview; then run it without --dry-run (uses sudo)",
    "pitchfork proxy doctor",
];

/// How to get pitchfork on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallPlan {
    /// The command devflow itself runs, after asking, when mise is available:
    /// `mise use -g pitchfork@latest`.
    pub run: Option<Vec<String>>,
    /// Commands to print for a person: what to run when devflow cannot.
    pub commands: Vec<String>,
}

/// What `devflow pitchfork install` does: run mise when it is on `PATH`,
/// otherwise print instructions. Nothing here runs anything.
pub fn install_plan() -> InstallPlan {
    install_plan_with(executable_on_path("mise").is_some())
}

fn install_plan_with(mise: bool) -> InstallPlan {
    let mut commands = Vec::new();
    if mise {
        commands.push("mise use -g pitchfork@latest".to_string());
    } else {
        commands.push("# install mise first: https://mise.jdx.dev, then".to_string());
        commands.push("mise use -g pitchfork@latest".to_string());
    }
    commands.push("brew install pitchfork".to_string());
    commands.push("cargo install pitchfork-cli".to_string());
    InstallPlan {
        run: mise.then(|| {
            ["mise", "use", "-g", "pitchfork@latest"]
                .map(String::from)
                .to_vec()
        }),
        commands,
    }
}

/// Run an install plan's command (`mise use -g pitchfork@latest`), which
/// changes the user's global mise configuration: callers ask first.
pub fn run_install(plan: &InstallPlan, timeout: Duration) -> Result<Ran> {
    let argv = plan
        .run
        .as_ref()
        .context("mise is not on PATH, so devflow has nothing to run")?;
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1");
    // `mise use` starts nothing that outlives it, but nothing here ends
    // processes it did not start.
    let out = subprocess::run_bounded(command, None, Limits::new(timeout).leaving())
        .with_context(|| format!("cannot run {}", argv.join(" ")))?;
    // Whatever was remembered about pitchfork is about the build before this.
    forget_detections();
    Ok(Ran::of(out))
}

fn executable_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// A daemon as `pitchfork list --json` reports it. There is no directory in
/// the listing: daemons are told apart by their namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Daemon {
    /// pitchfork's identifier, `namespace/name`, used verbatim for start/stop.
    pub id: String,
    pub namespace: String,
    pub name: String,
    /// `running`, `waiting`, `stopped`, `errored`, `failed`, `available`, …
    /// (`unknown` when the listing gave none or not as text).
    pub status: String,
    pub pid: Option<u32>,
    pub ports: Vec<u16>,
    /// The proxy URL, when the proxy is on and the daemon has a port.
    pub url: Option<String>,
    /// Why it stopped or failed.
    pub error: Option<String>,
    /// What is wrong with the fields devflow relies on (the id, the status,
    /// the pid), when something is: such a daemon's state cannot be judged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub malformed: Option<String>,
}

/// What a daemon's status says about its process. The one place that reads
/// pitchfork's status strings: parsing, [`Daemon`], the snapshot's
/// verifiability, the reports (`processes::DaemonState`), the observation and
/// the desktop app's counts and tooltips all ask this, so they cannot disagree
/// about a state such as `running (ready)`.
///
/// pitchfork may decorate a status: `running (ready)`, `failed: exit 3`. Only
/// the leading word counts (ASCII letters, case-insensitive), so a decorated
/// state is the state it starts with. A word devflow does not know (`unknown`,
/// `paused`, a state a later pitchfork adds) is [`DaemonStatus::Unknown`]:
/// neither running nor not running, and an operation that destroys something
/// must not take it for the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonStatus {
    /// The process is alive.
    Running,
    /// Started; pitchfork is waiting for its readiness check.
    Waiting,
    /// On its way down, still alive.
    Stopping,
    /// Gone, stopped on purpose.
    Stopped,
    /// Gone, ended in an error.
    Errored,
    /// Gone, failed to start or run.
    Failed,
    /// Declared and never started.
    Available,
    /// Anything else.
    Unknown,
}

impl DaemonStatus {
    /// The status of a status string as pitchfork prints it.
    pub fn parse(status: &str) -> DaemonStatus {
        let word: String = status
            .trim_start()
            .chars()
            .take_while(char::is_ascii_alphabetic)
            .collect::<String>()
            .to_ascii_lowercase();
        match word.as_str() {
            "running" => DaemonStatus::Running,
            "waiting" => DaemonStatus::Waiting,
            "stopping" => DaemonStatus::Stopping,
            "stopped" => DaemonStatus::Stopped,
            "errored" => DaemonStatus::Errored,
            "failed" => DaemonStatus::Failed,
            "available" => DaemonStatus::Available,
            _ => DaemonStatus::Unknown,
        }
    }

    /// The process is alive or on its way to be (`waiting` is a daemon whose
    /// readiness check is still running, `stopping` one not yet gone).
    pub fn is_active(self) -> bool {
        matches!(
            self,
            DaemonStatus::Running | DaemonStatus::Waiting | DaemonStatus::Stopping
        )
    }

    /// The status says no process runs.
    pub fn is_inactive(self) -> bool {
        matches!(
            self,
            DaemonStatus::Stopped
                | DaemonStatus::Errored
                | DaemonStatus::Failed
                | DaemonStatus::Available
        )
    }

    /// It ended in an error or failed to start.
    pub fn is_failure(self) -> bool {
        matches!(self, DaemonStatus::Errored | DaemonStatus::Failed)
    }

    /// devflow knows what the status means.
    pub fn is_known(self) -> bool {
        self != DaemonStatus::Unknown
    }
}

impl Daemon {
    /// What the listing's status says ([`DaemonStatus`]).
    pub fn class(&self) -> DaemonStatus {
        DaemonStatus::parse(&self.status)
    }

    /// Whether the process is alive or on its way to be.
    pub fn is_active(&self) -> bool {
        self.class().is_active()
    }

    /// Whether the status is one that says no process runs.
    pub fn is_inactive(&self) -> bool {
        self.class().is_inactive()
    }

    /// Whether devflow knows what the status means, and the entry is well
    /// formed. Anything else (`unknown`, a state a later pitchfork adds) is
    /// neither "running" nor "not running": an operation that is destructive
    /// must not take it for the second.
    pub fn state_known(&self) -> bool {
        self.malformed.is_none() && self.class().is_known()
    }
}

/// What `pitchfork list --json` printed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub daemons: Vec<Daemon>,
    /// Entries that could not be read at all (not an object, or without a
    /// text `id`): they cannot be attributed to any workspace.
    pub skipped: usize,
}

/// What `pitchfork proxy status --json` says about the proxy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ProxyStatus {
    pub enabled: bool,
    pub scheme: Option<String>,
    pub tld: Option<String>,
    pub port: Option<u16>,
    /// Whether the proxy's certificate is in the system trust store.
    pub trusted: Option<bool>,
    /// The projects the proxy routes for, with the directory each came from.
    pub projects: Vec<ProxyProject>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProxyProject {
    /// pitchfork's project name: the namespace.
    pub project: String,
    pub dir: PathBuf,
    pub daemons: Vec<ProxyDaemon>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProxyDaemon {
    pub daemon: String,
    pub url: Option<String>,
    pub status: Option<String>,
    pub port: Option<u16>,
}

impl ProxyStatus {
    /// The proxy URL of daemon `name` of the worktree at `dir` (compared
    /// canonically), if the proxy routes it.
    pub fn url_for(&self, dir: &Path, name: &str) -> Option<String> {
        let want = canonical(dir);
        self.projects
            .iter()
            .filter(|project| canonical(&project.dir) == want)
            .flat_map(|project| &project.daemons)
            .find(|daemon| daemon.daemon == name)
            .and_then(|daemon| daemon.url.clone())
    }
}

/// The result of looking for pitchfork and its features.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Detection {
    /// The first `pitchfork` on `PATH`, whether or not it works.
    pub path: Option<PathBuf>,
    /// `pitchfork --version` exits 0. A mise shim with no version configured
    /// is on `PATH` and executable, but exits 1: not installed.
    pub installed: bool,
    pub version: Option<String>,
    /// Features devflow calls that this build lacks, by name.
    pub missing_features: Vec<String>,
    /// Why it is not installed, when something is on `PATH`.
    pub problem: Option<String>,
}

impl Detection {
    /// Installed and with every feature devflow needs.
    pub fn usable(&self) -> bool {
        self.installed && self.missing_features.is_empty()
    }

    /// One line for people.
    pub fn summary(&self) -> String {
        match (&self.path, self.installed) {
            (None, _) => "pitchfork was not found on PATH".to_string(),
            (Some(path), false) => format!(
                "{} is on PATH but does not run{}",
                path.display(),
                self.problem
                    .as_deref()
                    .map(|p| format!(": {p}"))
                    .unwrap_or_default()
            ),
            (Some(path), true) => {
                let version = self.version.as_deref().unwrap_or("unknown version");
                if self.missing_features.is_empty() {
                    format!("pitchfork {version} at {}", path.display())
                } else {
                    format!(
                        "pitchfork {version} at {} lacks {}",
                        path.display(),
                        self.missing_features.join(", ")
                    )
                }
            }
        }
    }
}

/// A `pitchfork` executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pitchfork {
    binary: PathBuf,
}

/// The `pitchfork` executable on `PATH` (a mise shim counts, working or not).
pub fn binary() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("pitchfork"))
        .find(|candidate| is_executable(candidate))
}

/// Look for pitchfork on `PATH` and check what it can do.
pub fn detect() -> Detection {
    match Pitchfork::locate() {
        Some(pitchfork) => pitchfork.detect(),
        None => Detection {
            path: None,
            installed: false,
            version: None,
            missing_features: Vec::new(),
            problem: None,
        },
    }
}

/// How a command that was run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    /// Exit status 0.
    pub ok: bool,
    /// devflow's timeout ended it (and killed it).
    pub timed_out: bool,
    /// What it printed on failure, without terminal noise, last lines only.
    pub message: Option<String>,
}

impl Pitchfork {
    /// The pitchfork on `PATH`.
    pub fn locate() -> Option<Pitchfork> {
        binary().map(|binary| Pitchfork { binary })
    }

    /// A specific executable (tests use a fake).
    pub fn at(binary: impl Into<PathBuf>) -> Pitchfork {
        Pitchfork {
            binary: binary.into(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.binary
    }

    /// `pitchfork --version` and the features devflow uses.
    pub fn detect(&self) -> Detection {
        let mut detection = self.probe();
        if !detection.installed {
            return detection;
        }
        for (name, args, flag) in FEATURES {
            let has = self
                .run(None, args, &[], Limits::new(HELP_TIMEOUT))
                .is_ok_and(|out| out.ok && format!("{}{}", out.stdout, out.stderr).contains(flag));
            if !has {
                detection.missing_features.push(name.to_string());
            }
        }
        if !detection
            .version
            .as_deref()
            .is_some_and(|version| version_at_least(version, 2, 0))
        {
            detection.missing_features.push(QUALIFIED_IDS.to_string());
        }
        detection
    }

    /// [`Pitchfork::detect`], remembered for [`DETECTION_TTL`] per executable:
    /// what an operation that acts on pitchfork (a start, a stop, a gate shown
    /// on every refresh) checks, without running `--help` of three subcommands
    /// each time. [`forget_detections`] drops what is remembered.
    pub fn detect_cached(&self) -> Detection {
        let remembered = |cache: &Vec<(PathBuf, Instant, Detection)>| {
            cache
                .iter()
                .find(|(path, at, _)| *path == self.binary && at.elapsed() < DETECTION_TTL)
                .map(|(_, _, detection)| detection.clone())
        };
        if let Some(found) = DETECTIONS.lock().ok().and_then(|cache| remembered(&cache)) {
            return found;
        }
        let detection = self.detect();
        if let Ok(mut cache) = DETECTIONS.lock() {
            cache.retain(|(path, at, _)| *path != self.binary && at.elapsed() < DETECTION_TTL);
            cache.push((self.binary.clone(), Instant::now(), detection.clone()));
        }
        detection
    }

    /// Only `pitchfork --version`: does it run, and which version. A mise shim
    /// with no version configured is executable but exits 1, which is "not
    /// installed".
    pub fn probe(&self) -> Detection {
        let mut detection = Detection {
            path: Some(self.binary.clone()),
            installed: false,
            version: None,
            missing_features: Vec::new(),
            problem: None,
        };
        match self.run(None, &["--version"], &[], Limits::new(VERSION_TIMEOUT)) {
            Ok(out) if out.ok => {
                detection.installed = true;
                detection.version = parse_version(&out.stdout);
            }
            Ok(out) => {
                detection.problem = Some(
                    first_line(&out.stderr)
                        .or_else(|| first_line(&out.stdout))
                        .unwrap_or_else(|| "`pitchfork --version` failed".to_string()),
                );
            }
            Err(error) => detection.problem = Some(format!("{error:#}")),
        }
        detection
    }

    /// Whether the supervisor is up (`supervisor status --json`). A query:
    /// it never starts one.
    pub fn supervisor_up(&self, dir: &Path) -> Result<bool> {
        let text = self.query(dir, &["supervisor", "status", "--json"])?;
        let value: serde_json::Value =
            serde_json::from_str(text.trim()).context("pitchfork printed invalid JSON")?;
        Ok(value.get("status").and_then(|s| s.as_str()) == Some("up"))
    }

    /// Every daemon the supervisor knows, run from `dir` so the ones that
    /// `dir`'s configuration declares show as `available`.
    pub fn list(&self, dir: &Path) -> Result<Vec<Daemon>> {
        self.listing(dir).map(|listing| listing.daemons)
    }

    /// [`Pitchfork::list`], with the count of entries that could not be read.
    pub fn listing(&self, dir: &Path) -> Result<Listing> {
        parse_listing(&self.query(dir, &["list", "--json"])?)
    }

    pub fn proxy_status(&self, dir: &Path) -> Result<ProxyStatus> {
        parse_proxy(&self.query(dir, &["proxy", "status", "--json"])?)
    }

    /// `pitchfork start -q ID…` in `dir` (whose configuration pitchfork reads
    /// for the daemons' definitions): exactly the daemons `ids`, each a full
    /// `namespace/name` id, and what they depend on. Blocks until pitchfork
    /// returns or `timeout` passes; the caller reads the real state afterwards.
    ///
    /// Never `--local`, and never an unqualified name or an empty list, which
    /// pitchfork would resolve against the configuration of `dir` and its
    /// parents: both are refused here.
    pub fn start(&self, dir: &Path, ids: &[String], timeout: Duration) -> Result<Ran> {
        ensure!(!ids.is_empty(), "no daemon to start was named");
        let mut args: Vec<&str> = vec!["start", "-q"];
        for id in ids {
            args.push(qualified(id)?);
        }
        // An effect: `pitchfork start` may launch the supervisor, which stays
        // in its process group, so nothing it leaves behind is ended.
        self.run(Some(dir), &args, &[], Limits::new(timeout).leaving())
            .map(Ran::of)
    }

    /// `pitchfork stop ID…` in `dir`: exactly the daemons `ids`, each a full
    /// `namespace/name` id. Never `--local`; see [`Pitchfork::start`].
    pub fn stop(&self, dir: &Path, ids: &[String], timeout: Duration) -> Result<Ran> {
        ensure!(!ids.is_empty(), "no daemon to stop was named");
        let mut args: Vec<&str> = vec!["stop"];
        for id in ids {
            args.push(qualified(id)?);
        }
        self.run(Some(dir), &args, &[], Limits::new(timeout).leaving())
            .map(Ran::of)
    }

    /// A read-only call: no supervisor is started on its behalf.
    fn query(&self, dir: &Path, args: &[&str]) -> Result<String> {
        let out = self.run(
            Some(dir),
            args,
            &[("PITCHFORK_SUPERVISOR_AUTO_START", "false")],
            Limits::new(QUERY_TIMEOUT),
        )?;
        if out.timed_out {
            bail!("`pitchfork {}` timed out", args.join(" "));
        }
        if !out.ok {
            bail!(
                "`pitchfork {}` failed: {}",
                args.join(" "),
                out.message.unwrap_or_default()
            );
        }
        // What is read here is JSON: a prefix of it is not an answer.
        ensure!(
            !out.truncated,
            "`pitchfork {}` printed more than {} MiB, which devflow does not read",
            args.join(" "),
            subprocess::DEFAULT_MAX_OUTPUT >> 20
        );
        Ok(out.stdout)
    }

    fn run(
        &self,
        dir: Option<&Path>,
        args: &[&str],
        env: &[(&str, &str)],
        limits: Limits,
    ) -> Result<Output> {
        let mut command = Command::new(&self.binary);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // No spinners or colours in what devflow reads back.
            .env("NO_COLOR", "1")
            .envs(env.iter().copied());
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        subprocess::run_bounded(command, None, limits)
            .with_context(|| format!("cannot run {} {}", self.binary.display(), args.join(" ")))
    }
}

impl Ran {
    fn of(out: Output) -> Ran {
        Ran {
            ok: out.ok,
            timed_out: out.timed_out,
            message: out.message,
        }
    }
}

/// A `namespace/name` id: one slash, two non-empty parts, nothing that could
/// be mistaken for an option.
fn qualified(id: &str) -> Result<&str> {
    let valid = id.split_once('/').is_some_and(|(namespace, name)| {
        let part = |part: &str| {
            !part.is_empty()
                && !part.starts_with('-')
                && !part.contains('/')
                && !part.chars().any(char::is_control)
        };
        part(namespace) && part(name)
    });
    ensure!(
        valid,
        "'{id}' is not a daemon id of the form namespace/name"
    );
    Ok(id)
}

/// Whether `version` (`2.29.0`, `v2.30.1-beta.1`) is at least `major.minor`.
/// A version that does not start with two numbers is not at least anything.
pub(crate) fn version_at_least(version: &str, major: u64, minor: u64) -> bool {
    let mut parts = version.trim_start_matches('v').split('.').map(|part| {
        part.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u64>()
            .ok()
    });
    match (parts.next().flatten(), parts.next().flatten()) {
        (Some(have_major), Some(have_minor)) => (have_major, have_minor) >= (major, minor),
        _ => false,
    }
}

fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// `pitchfork 2.29.0` or `2.29.0`.
fn parse_version(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|word| word.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(str::to_string)
}

/// Parse `pitchfork list --json`: an array of objects with at least `id`.
/// Unknown fields are ignored; entries without an id are skipped (see
/// [`parse_listing`] for the count).
pub fn parse_list(text: &str) -> Result<Vec<Daemon>> {
    parse_listing(text).map(|listing| listing.daemons)
}

/// [`parse_list`] that also says how many entries could not be read, and marks
/// a daemon whose id, status or pid is not what pitchfork prints
/// ([`Daemon::malformed`]).
pub fn parse_listing(text: &str) -> Result<Listing> {
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).context("pitchfork printed invalid JSON")?;
    let items = value
        .as_array()
        .context("pitchfork list --json did not print an array")?;
    let mut listing = Listing::default();
    for item in items {
        let Some(id) = item.get("id").and_then(|id| id.as_str()) else {
            listing.skipped += 1;
            continue;
        };
        let id = id.to_string();
        let text = |key: &str| {
            item.get(key)
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let (id_namespace, id_name) = id.split_once('/').unwrap_or(("", id.as_str()));
        let malformed = if id_namespace.is_empty() || id_name.is_empty() {
            Some("has an id that is not namespace/name".to_string())
        } else if text("namespace").is_some_and(|namespace| namespace != id_namespace) {
            Some("names one namespace in its id and another in its namespace field".to_string())
        } else if text("status").is_none() {
            Some("has no status as text".to_string())
        } else if item.get("pid").is_some_and(|pid| {
            !pid.is_null() && pid.as_u64().and_then(|p| u32::try_from(p).ok()).is_none()
        }) {
            Some("has a pid that is not a number".to_string())
        } else {
            None
        };
        listing.daemons.push(Daemon {
            namespace: text("namespace").unwrap_or_else(|| id_namespace.to_string()),
            name: text("name").unwrap_or_else(|| id_name.to_string()),
            status: text("status").unwrap_or_else(|| "unknown".to_string()),
            pid: item
                .get("pid")
                .and_then(|p| p.as_u64())
                .and_then(|p| u32::try_from(p).ok()),
            ports: item
                .get("port")
                .and_then(|p| p.as_array())
                .map(|ports| {
                    ports
                        .iter()
                        .filter_map(|p| p.as_u64().and_then(|p| u16::try_from(p).ok()))
                        .collect()
                })
                .unwrap_or_default(),
            url: text("url").or_else(|| text("proxy_url")),
            error: text("error"),
            malformed,
            id,
        });
    }
    Ok(listing)
}

/// Parse `pitchfork proxy status --json`. Fields devflow does not know are
/// ignored; a missing one is `None`.
pub fn parse_proxy(text: &str) -> Result<ProxyStatus> {
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).context("pitchfork printed invalid JSON")?;
    ensure!(
        value.is_object(),
        "pitchfork proxy status --json did not print an object"
    );
    let string = |value: &serde_json::Value, key: &str| {
        value.get(key).and_then(|v| v.as_str()).map(str::to_string)
    };
    let port = |value: &serde_json::Value| {
        value
            .get("port")
            .and_then(|p| p.as_u64())
            .and_then(|p| u16::try_from(p).ok())
    };
    let projects = value
        .get("projects")
        .and_then(|p| p.as_array())
        .map(|projects| {
            projects
                .iter()
                .filter_map(|project| {
                    Some(ProxyProject {
                        project: string(project, "project")?,
                        dir: PathBuf::from(string(project, "dir")?),
                        daemons: project
                            .get("daemons")
                            .and_then(|d| d.as_array())
                            .map(|daemons| {
                                daemons
                                    .iter()
                                    .filter_map(|daemon| {
                                        Some(ProxyDaemon {
                                            daemon: string(daemon, "daemon")?,
                                            url: string(daemon, "url"),
                                            status: string(daemon, "status"),
                                            port: port(daemon),
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(ProxyStatus {
        enabled: value
            .get("enabled")
            .and_then(|e| e.as_bool())
            .unwrap_or(false),
        scheme: string(&value, "scheme"),
        tld: string(&value, "tld"),
        port: port(&value),
        trusted: value.get("trusted").and_then(|t| t.as_bool()),
        projects,
    })
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real output of `pitchfork list --json` (2.29.0), a daemon with a port
    /// behind the proxy and one that is only declared.
    const LISTING: &str = r#"[
      {"id":"df-0b3c9d2e-a1b2c3d4/web","namespace":"df-0b3c9d2e-a1b2c3d4","name":"web","pid":78190,"status":"running",
       "oneshot":false,"disabled":false,"available":false,"proxy_url":"https://web.df-0b3c9d2e-a1b2c3d4.localhost",
       "url":"https://web.df-0b3c9d2e-a1b2c3d4.localhost","proxy_tls":"terminate","error":null,"port":[3999]},
      {"id":"proj/worker","namespace":"proj","name":"worker","pid":null,"status":"available",
       "oneshot":false,"disabled":false,"available":true,"proxy_url":null,"url":null,"error":null,"port":[]},
      {"id":"proj/bad","namespace":"proj","name":"bad","pid":null,"status":"errored",
       "oneshot":false,"disabled":false,"available":false,"proxy_url":null,"url":null,"error":"exit code 3","port":[]}
    ]"#;

    /// Real output of `pitchfork proxy status --json` (2.29.0), proxy on.
    const PROXY: &str = r#"{
      "enabled": true, "scheme": "https", "tld": "localhost", "port": 443,
      "tls_cert": "/tmp/state/proxy/ca.pem (auto-generated)", "trusted": false, "slugs": [],
      "projects": [
        {"project": "df-0b3c9d2e-a1b2c3d4", "dir": "/private/tmp/pf-iso/proj",
         "url": "https://df-0b3c9d2e-a1b2c3d4.localhost",
         "daemons": [{"daemon": "web", "host": "web.df-0b3c9d2e-a1b2c3d4",
                      "url": "https://web.df-0b3c9d2e-a1b2c3d4.localhost", "status": "running", "port": 3999}]}
      ]
    }"#;

    #[test]
    fn parses_pitchfork_listings() {
        let daemons = parse_list(LISTING).unwrap();
        assert_eq!(daemons.len(), 3);
        let web = &daemons[0];
        assert_eq!(web.id, "df-0b3c9d2e-a1b2c3d4/web");
        assert_eq!(web.namespace, "df-0b3c9d2e-a1b2c3d4");
        assert_eq!(web.name, "web");
        assert_eq!((web.status.as_str(), web.pid), ("running", Some(78190)));
        assert_eq!(web.ports, [3999]);
        assert_eq!(
            web.url.as_deref(),
            Some("https://web.df-0b3c9d2e-a1b2c3d4.localhost")
        );
        assert!(web.is_active());
        assert_eq!(daemons[1].status, "available");
        assert!(!daemons[1].is_active() && daemons[1].url.is_none());
        assert_eq!(daemons[2].error.as_deref(), Some("exit code 3"));
    }

    #[test]
    fn parsing_is_defensive() {
        assert!(parse_list("[]").unwrap().is_empty());
        assert!(parse_list("not json").is_err());
        assert!(parse_list("{}").is_err());
        let odd = parse_list(
            r#"[{"status":"running"},{"id":"a/b","pid":-1,"port":["x",70000,8080],"status":7},"junk",{"id":"solo"}]"#,
        )
        .unwrap();
        assert_eq!(odd.len(), 2);
        assert_eq!(odd[0].id, "a/b");
        assert_eq!(
            (odd[0].namespace.as_str(), odd[0].name.as_str()),
            ("a", "b")
        );
        assert_eq!((odd[0].pid, odd[0].status.as_str()), (None, "unknown"));
        assert!(odd[0].malformed.is_some() && odd[1].malformed.is_some());
        assert_eq!(
            parse_listing(r#"[{"status":"running"},"junk",{"id":"a/b"}]"#)
                .unwrap()
                .skipped,
            2
        );
        assert_eq!(odd[0].ports, [8080]);
        assert_eq!(
            (odd[1].namespace.as_str(), odd[1].name.as_str()),
            ("", "solo")
        );
    }

    #[test]
    fn only_known_statuses_say_whether_a_process_runs() {
        let status = |status: &str| {
            let text = format!(r#"[{{"id":"ns/web","status":"{status}","pid":7}}]"#);
            parse_list(&text).unwrap().remove(0)
        };
        for running in ["running", "Waiting", "stopping", "running (ready)"] {
            let d = status(running);
            assert!(
                d.is_active() && !d.is_inactive() && d.state_known(),
                "{running}"
            );
        }
        for stopped in [
            "stopped",
            "errored",
            "failed",
            "failed: exit 3",
            "available",
        ] {
            let d = status(stopped);
            assert!(
                !d.is_active() && d.is_inactive() && d.state_known(),
                "{stopped}"
            );
        }
        for unknown in [
            "unknown",
            "paused",
            "zombie",
            "disabled",
            "runningish",
            "not started",
            "(running)",
        ] {
            let d = status(unknown);
            assert!(
                !d.is_active() && !d.is_inactive() && !d.state_known(),
                "{unknown}"
            );
        }
        // Fields the listing got wrong make the state unjudgeable, whatever
        // the status says.
        for bad in [
            r#"{"id":"ns/web","status":"stopped","pid":"x"}"#,
            r#"{"id":"ns/web","status":"stopped","pid":-1}"#,
            r#"{"id":"ns/web","status":7}"#,
            r#"{"id":"ns/web"}"#,
            r#"{"id":"web","status":"stopped"}"#,
            r#"{"id":"ns/web","namespace":"other","status":"stopped"}"#,
        ] {
            let d = parse_list(&format!("[{bad}]")).unwrap().remove(0);
            assert!(d.malformed.is_some() && !d.state_known(), "{bad}");
        }
        let ok = parse_list(
            r#"[{"id":"ns/web","namespace":"ns","name":"web","status":"stopped","pid":null}]"#,
        )
        .unwrap()
        .remove(0);
        assert!(ok.malformed.is_none() && ok.state_known());
    }

    #[test]
    fn the_proxy_url_is_matched_by_worktree_directory_and_daemon() {
        let proxy = parse_proxy(PROXY).unwrap();
        assert!(proxy.enabled);
        assert_eq!(
            (proxy.scheme.as_deref(), proxy.tld.as_deref(), proxy.port),
            (Some("https"), Some("localhost"), Some(443))
        );
        assert_eq!(proxy.trusted, Some(false));
        assert_eq!(
            proxy.url_for(Path::new("/private/tmp/pf-iso/proj"), "web"),
            Some("https://web.df-0b3c9d2e-a1b2c3d4.localhost".to_string())
        );
        assert_eq!(
            proxy.url_for(Path::new("/private/tmp/pf-iso/proj"), "api"),
            None
        );
        assert_eq!(proxy.url_for(Path::new("/elsewhere"), "web"), None);
        // Disabled proxy: no more than the switch.
        let off = parse_proxy(r#"{"enabled": false, "slugs": []}"#).unwrap();
        assert_eq!(off, ProxyStatus::default());
        assert!(parse_proxy("[]").is_err());
    }

    #[test]
    fn versions_are_read_from_the_first_numeric_word() {
        assert_eq!(
            parse_version("pitchfork 2.29.0\n").as_deref(),
            Some("2.29.0")
        );
        assert_eq!(parse_version("2.30.1").as_deref(), Some("2.30.1"));
        assert_eq!(parse_version("pitchfork"), None);
    }

    #[test]
    fn only_namespace_slash_name_ids_are_accepted() {
        for bad in [
            "--all", "", "web", "/web", "ns/", "a/b/c", "-ns/web", "ns/-web", "ns/we\nb",
        ] {
            assert!(qualified(bad).is_err(), "{bad:?}");
        }
        assert_eq!(qualified("proj/web").unwrap(), "proj/web");
        assert_eq!(
            qualified("df-0b3c9d2e-a1b2c3d4/web-2").unwrap(),
            "df-0b3c9d2e-a1b2c3d4/web-2"
        );
        // Nothing is run for an empty list, an unqualified name or an option:
        // the executable does not even have to exist.
        let pitchfork = Pitchfork::at("/nonexistent/pitchfork");
        let dir = Path::new("/");
        let timeout = Duration::from_secs(1);
        for ids in [vec![], vec!["web".to_string()], vec!["--local".to_string()]] {
            let start = pitchfork.start(dir, &ids, timeout).unwrap_err();
            let stop = pitchfork.stop(dir, &ids, timeout).unwrap_err();
            for error in [start, stop] {
                let text = format!("{error:#}");
                assert!(
                    text.contains("no daemon") || text.contains("namespace/name"),
                    "refused before running anything: {text}"
                );
            }
        }
    }

    #[test]
    fn the_version_decides_whether_namespaced_ids_exist() {
        assert!(version_at_least("2.29.0", 2, 0));
        assert!(version_at_least("2.0.0", 2, 0));
        assert!(version_at_least("v2.30.1-beta.1", 2, 0));
        assert!(version_at_least("3.0.0", 2, 0));
        assert!(!version_at_least("1.6.0", 2, 0));
        assert!(!version_at_least("1.99.9", 2, 0));
        assert!(!version_at_least("2", 2, 0));
        assert!(!version_at_least("", 2, 0));
        assert!(!version_at_least("unknown", 2, 0));
    }
}

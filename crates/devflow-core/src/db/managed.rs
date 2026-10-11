//! Managed server: a `docker compose` project under the data dir, its
//! generated superuser credentials, and the copy-on-write probe.
use super::admin::{Target, quote_ident, random_password};
use crate::config::{CowCapability, PostgresSection};
use crate::project::Project;
use crate::remedy::{Problem, Remedy};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use postgres::Client;
use postgres::error::SqlState;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;
use uuid::Uuid;

const SERVICE: &str = "pg";
/// Host ports a managed server picks from when `port` is 0: [20000, 40000).
const PORT_MIN: u16 = 20_000;
const PORT_SPAN: u16 = 20_000;
/// How many consecutive candidates the first allocation tries before giving up.
const PORT_ATTEMPTS: u16 = 16;
/// Probe data size target (about 64 MB of heap).
const PROBE_ROWS: u32 = 600_000;
/// How long a read-only `docker compose` query may take when its caller has no
/// deadline of its own to give it.
pub(super) const QUERY_WITHIN: Duration = Duration::from_secs(30);

pub struct ManagedServer {
    /// `<data dir>/postgres/<project id>`.
    pub dir: PathBuf,
    /// Compose project name, `devflow-<sanitized name>-<id8>`.
    pub compose_project: String,
    pub image: String,
    /// The project's root, for the remedies of the errors it raises.
    root: PathBuf,
    project_id: Uuid,
    /// The key of the postgres service (`[services.<key>]`).
    service_key: String,
    /// OrbStack name of the server for other containers (no port).
    container_host: String,
    bind: String,
    /// The configured `port`: 0 means "allocate once and keep".
    port: u16,
    /// The Docker client (`docker`); tests point it at a stand-in.
    docker: std::ffi::OsString,
}

/// What `docker compose ps` says about the server container.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContainerState {
    pub exists: bool,
    pub running: bool,
}

/// `admin.json`: the superuser password (never regenerated once written) and
/// the cached CoW probe result.
#[derive(Debug, Serialize, Deserialize)]
struct AdminState {
    version: u32,
    password: String,
    /// The host port allocated when `port = 0`, kept for the life of the
    /// server: published fixed, so it survives restarts, and never changed
    /// silently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    /// The candidate a first allocation is trying, written before Docker is
    /// asked to publish it. After an interruption it says which port may
    /// already be bound and must not be skipped; it is replaced by `port` once
    /// the server runs on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cow: Option<CowRecord>,
    /// Probe databases being created; dropped by the next probe if a crash left them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    probe_databases: Vec<String>,
}

/// One probe result, valid for one postmaster start of one image. Applies to
/// the filesystem holding `$PGDATA` only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct CowRecord {
    pub postmaster_start: String,
    pub image: String,
    pub capability: CowCapability,
    pub probe_bytes: i64,
    pub growth_bytes: i64,
    /// Why the result is `unknown`.
    pub note: Option<String>,
    pub measured_at: DateTime<Utc>,
}

impl ManagedServer {
    /// The server of the postgres service `service_key`.
    pub fn new(project: &Project, service_key: &str, settings: &PostgresSection) -> Result<Self> {
        let id = project.id();
        anyhow::ensure!(
            settings.bind.parse::<IpAddr>().is_ok(),
            "services.{}.bind must be an IP address",
            crate::diag::key_label(service_key)
        );
        Ok(Self {
            dir: crate::paths::data_dir()?
                .join("postgres")
                .join(id.to_string()),
            compose_project: compose_project_name(project.name(), id),
            image: settings.image.clone(),
            root: project.root.clone(),
            project_id: id,
            service_key: service_key.to_string(),
            container_host: orbstack_host(project.name(), id, service_key),
            bind: settings.bind.clone(),
            port: settings.port,
            docker: "docker".into(),
        })
    }

    /// A server in `dir` whose Docker client is `docker`, for tests.
    #[cfg(test)]
    pub(super) fn for_test(dir: PathBuf, docker: PathBuf) -> Self {
        Self {
            dir,
            compose_project: "devflow-test".into(),
            image: "postgres:18".into(),
            root: PathBuf::new(),
            project_id: Uuid::nil(),
            service_key: "db".into(),
            container_host: "test".into(),
            bind: "127.0.0.1".into(),
            port: 0,
            docker: docker.into(),
        }
    }

    pub fn compose_file(&self) -> PathBuf {
        self.dir.join("compose.yaml")
    }

    fn admin_file(&self) -> PathBuf {
        self.dir.join("admin.json")
    }

    /// Write the compose file (and the credentials on first use), then
    /// `docker compose up -d --wait`.
    ///
    /// The host port is fixed. An explicit `port` is used as it is. With
    /// `port = 0` the first start allocates one from the project and service
    /// identity ([`candidate_ports`]), lets the bind decide whether it is free
    /// (trying the next candidates, a bounded number), and records it in
    /// `admin.json` once the server runs on it. The bind that decides is
    /// Docker's, preceded by a bind of our own on the host: OrbStack publishes
    /// a port even where a host process already listens on it, and a database
    /// client would then talk to that process. Later starts reuse that port;
    /// if something else holds it by then, the start fails naming the port and
    /// the cause: a recorded port is never changed behind the user's back.
    ///
    /// Starts of one server are serialized (a lock file beside `admin.json`):
    /// two processes must not allocate, or journal, at the same time.
    pub fn start(&self) -> Result<()> {
        crate::fsutil::with_lock(&self.dir.join("start.lock"), || self.start_locked())
    }

    fn start_locked(&self) -> Result<()> {
        let state = self.load_or_create_state()?;
        // The port a running container already holds is ours, not a conflict.
        let own = self.running_port();
        if self.port != 0 {
            return self
                .claim(&state.password, self.port, own)
                .map_err(|error| {
                    self.unavailable(
                        error,
                        self.port,
                        &format!("set as `port` in [services.{}]", self.service_key),
                    )
                });
        }
        if let Some(port) = state.port {
            return self.claim(&state.password, port, own).map_err(|error| {
                self.unavailable(
                    error,
                    port,
                    &format!(
                        "allocated for this server and recorded in {}",
                        self.admin_file().display()
                    ),
                )
            });
        }
        let candidates = candidate_ports(self.project_id, &self.service_key);
        let allocation = allocate_port(
            &candidates,
            state.pending_port,
            own,
            &mut |pending, port| {
                self.update_state(|state| {
                    state.pending_port = pending;
                    if port.is_some() {
                        state.port = port;
                    }
                })
            },
            &mut |port| self.claim(&state.password, port, own),
        );
        match allocation {
            Ok(_) => Ok(()),
            Err(AllocationError::Exhausted(refused)) => bail!(
                "no host port was available for the managed PostgreSQL server; tried {}. \
                 Free one of them or set `port` in [services.{}]",
                refused.join("; "),
                self.service_key
            ),
            Err(AllocationError::Other(error)) => Err(error),
        }
    }

    /// The host port of the running container, if there is one.
    fn running_port(&self) -> Option<u16> {
        self.container_state()
            .ok()
            .filter(|state| state.running)
            .and_then(|_| self.published_port().ok())
    }

    /// Bring the server up on `port`; `own` is the port a running container
    /// already publishes, which is not a conflict. A [`BindFailure`] means
    /// something else holds the port.
    fn claim(&self, password: &str, port: u16, own: Option<u16>) -> Result<()> {
        if own != Some(port)
            && let Some(conflict) = self.host_conflict(port)
        {
            return Err(BindFailure(conflict).into());
        }
        self.up(password, port)
    }

    /// Another process already listens on `port` of the bind address.
    fn host_conflict(&self, port: u16) -> Option<String> {
        let ip = self.bind.parse::<IpAddr>().ok()?;
        match std::net::TcpListener::bind((ip, port)) {
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => Some(format!(
                "another process already listens on {}:{port} ({error})",
                self.bind
            )),
            // Free, or not something a bind can tell us (permissions, an
            // address family this host lacks): Docker has the last word.
            _ => None,
        }
    }

    /// Render the compose file for `port` and bring the container up.
    fn up(&self, password: &str, port: u16) -> Result<()> {
        crate::fsutil::write_atomic(
            &self.compose_file(),
            self.render_compose(password, port).as_bytes(),
            true,
        )?;
        let Err(error) = self.compose(&["up", "-d", "--wait"]) else {
            return Ok(());
        };
        let text = format!("{error:#}");
        if is_bind_failure(&text) {
            return Err(BindFailure(summarize_bind_failure(&text)).into());
        }
        let logs = self
            .compose(&["logs", "--no-color", "--tail", "15", SERVICE])
            .unwrap_or_default();
        bail!(
            "the PostgreSQL container did not become healthy: {text}\n{}",
            logs.trim()
        )
    }

    /// Name the port and the conflict when `error` is a failed bind.
    fn unavailable(&self, error: anyhow::Error, port: u16, origin: &str) -> anyhow::Error {
        match error.downcast::<BindFailure>() {
            Ok(failure) => anyhow::anyhow!(
                "host port {port} ({origin}) is not available for the managed PostgreSQL \
                 server: {}. devflow keeps this port: stop what uses it, or set `port` in \
                 [services.{}] to another one",
                failure.0,
                self.service_key
            ),
            Err(error) => error,
        }
    }

    /// The name other containers on the same Docker networks reach the server
    /// at (`host:5432`), when Docker is OrbStack. `None` elsewhere: no other
    /// Docker host gives containers stable names.
    pub fn container_endpoint(&self) -> Option<String> {
        orbstack().then(|| format!("{}:5432", self.container_host))
    }

    /// The port allocated by [`ManagedServer::start`] when `port` is 0.
    pub fn recorded_port(&self) -> Result<Option<u16>> {
        Ok(self.load_state()?.and_then(|state| state.port))
    }

    /// `docker compose stop`: data stays in the volume. devflow never runs
    /// `down -v` or deletes volumes.
    pub fn stop(&self) -> Result<()> {
        if !self.compose_file().is_file() {
            return Ok(());
        }
        self.compose(&["stop"]).map(|_| ())
    }

    /// Host port Docker published for 5432.
    pub fn published_port(&self) -> Result<u16> {
        self.published_port_within(QUERY_WITHIN)
    }

    /// Host port Docker published for 5432, found within `budget`. The lookup
    /// is a subprocess, and a Docker that stalls must not outlast the deadline
    /// of the caller that is waiting for the server: past `budget` the command
    /// and what it started are killed and reaped.
    pub fn published_port_within(&self, budget: Duration) -> Result<u16> {
        if !self.compose_file().is_file() {
            return Err(self.not_started());
        }
        let out = self.compose_within(&["port", SERVICE, "5432"], budget)?;
        parse_published_port(&out).ok_or_else(|| {
            Problem::new("the managed PostgreSQL server is not running")
                .with(Remedy::StartDatabase {
                    root: self.root.clone(),
                })
                .into_error()
        })
    }

    /// The server was never started (or its files are gone): starting it
    /// creates them.
    fn not_started(&self) -> anyhow::Error {
        Problem::new("the managed PostgreSQL server has not been started")
            .with(Remedy::StartDatabase {
                root: self.root.clone(),
            })
            .into_error()
    }

    pub fn container_state(&self) -> Result<ContainerState> {
        if !self.compose_file().is_file() {
            return Ok(ContainerState::default());
        }
        let out = self.compose_within(&["ps", "--all", "--format", "json"], QUERY_WITHIN)?;
        Ok(parse_ps(&out))
    }

    /// The generated superuser password.
    pub fn admin_password(&self) -> Result<String> {
        let state = self.load_state()?.ok_or_else(|| self.not_started())?;
        Ok(state.password)
    }

    // ---- docker ----

    /// A read-only `docker compose` query (`port`, `ps`), over within
    /// `budget` whatever Docker does.
    fn compose_within(&self, args: &[&str], budget: Duration) -> Result<String> {
        compose_query(
            &self.docker,
            &self.compose_project,
            &self.compose_file(),
            self.dir.is_dir().then_some(self.dir.as_path()),
            args,
            budget,
        )
    }

    fn compose(&self, args: &[&str]) -> Result<String> {
        let mut command = Command::new(&self.docker);
        command
            .args(["compose", "-p", &self.compose_project, "-f"])
            .arg(self.compose_file())
            .args(args);
        if self.dir.is_dir() {
            command.current_dir(&self.dir);
        }
        let output = command
            .output()
            .context("cannot run `docker compose` (is Docker installed and running?)")?;
        if !output.status.success() {
            bail!(
                "`docker compose {}` failed: {}",
                args.first().copied().unwrap_or(""),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn render_compose(&self, password: &str, port: u16) -> String {
        let domain = orbstack().then_some(self.container_host.as_str());
        render_compose(
            &self.image,
            self.project_id,
            &self.bind,
            port,
            password,
            domain,
        )
    }

    // ---- admin.json ----

    fn load_state(&self) -> Result<Option<AdminState>> {
        match std::fs::read(self.admin_file()) {
            // `admin.json` holds the superuser password: a malformed file is
            // described by position and key path, never by what it says.
            Ok(bytes) => {
                let what = format!(
                    "{} is unreadable; refusing to regenerate the superuser password",
                    self.admin_file().display()
                );
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| anyhow::anyhow!("{what}: not UTF-8"))?;
                Ok(Some(crate::diag::json_from_str(text, &what)?))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => {
                Err(e).with_context(|| format!("cannot read {}", self.admin_file().display()))
            }
        }
    }

    /// Create `admin.json` only if absent: the file is linked into place so two
    /// racing processes cannot overwrite each other's password.
    fn load_or_create_state(&self) -> Result<AdminState> {
        if let Some(state) = self.load_state()? {
            return Ok(state);
        }
        let fresh = AdminState {
            version: 1,
            password: random_password(),
            port: None,
            pending_port: None,
            cow: None,
            probe_databases: Vec::new(),
        };
        let tmp = self.dir.join(format!(".admin.json.{}.new", Uuid::new_v4()));
        crate::fsutil::write_atomic(&tmp, &serde_json::to_vec_pretty(&fresh)?, true)?;
        let linked = std::fs::hard_link(&tmp, self.admin_file());
        let _ = std::fs::remove_file(&tmp);
        match linked {
            Ok(()) => Ok(fresh),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => self
                .load_state()?
                .context("admin.json disappeared while it was being created"),
            Err(e) => Err(e).context("cannot create admin.json"),
        }
    }

    fn update_state(&self, change: impl FnOnce(&mut AdminState)) -> Result<()> {
        let mut state = self
            .load_state()?
            .context("the managed server has no admin.json")?;
        change(&mut state);
        crate::fsutil::write_atomic(
            &self.admin_file(),
            &serde_json::to_vec_pretty(&state)?,
            true,
        )
    }

    // ---- CoW probe ----

    /// The cached probe result if it belongs to this postmaster start and image.
    pub(super) fn cached_cow(&self, postmaster_start: &str) -> Option<CowRecord> {
        self.load_state()
            .ok()
            .flatten()?
            .cow
            .filter(|c| c.postmaster_start == postmaster_start && c.image == self.image)
    }

    /// Measure and cache. Never fails: a probe error is recorded as `unknown`.
    pub(super) fn measure_cow(
        &self,
        target: &Target,
        admin: &mut Client,
        postmaster_start: &str,
    ) -> CowRecord {
        let mut record = CowRecord {
            postmaster_start: postmaster_start.to_string(),
            image: self.image.clone(),
            capability: CowCapability::Unknown,
            probe_bytes: 0,
            growth_bytes: 0,
            note: None,
            measured_at: Utc::now(),
        };
        match self.run_probe(target, admin) {
            Ok(measurement) => {
                record.probe_bytes = measurement.probe_bytes;
                record.growth_bytes = measurement.growth_bytes;
                record.capability = classify(&measurement);
                record.note = measurement.note;
            }
            Err(error) => record.note = Some(format!("{error:#}")),
        }
        let _ = self.update_state(|state| state.cow = Some(record.clone()));
        record
    }

    fn run_probe(&self, target: &Target, admin: &mut Client) -> Result<Measurement> {
        // Leftovers of a probe that crashed (recorded before they were created).
        let stale = self
            .load_state()?
            .map(|s| s.probe_databases)
            .unwrap_or_default();
        for name in &stale {
            drop_probe_database(admin, name)?;
        }
        let (a, b) = (probe_name(), probe_name());
        self.update_state(|state| state.probe_databases = vec![a.clone(), b.clone()])?;
        let result = self.probe_clone(target, admin, &a, &b);
        let dropped = [&a, &b]
            .iter()
            .all(|name| drop_probe_database(admin, name).is_ok());
        if dropped {
            let _ = self.update_state(|state| state.probe_databases.clear());
        }
        result
    }

    /// Fill a database, flush it, sample free space twice to detect
    /// interference, clone it with FILE_COPY and see how much space the clone
    /// took in `$PGDATA`'s filesystem.
    fn probe_clone(
        &self,
        target: &Target,
        admin: &mut Client,
        a: &str,
        b: &str,
    ) -> Result<Measurement> {
        admin.batch_execute(&format!(
            "CREATE DATABASE {} TEMPLATE template0",
            quote_ident(a)
        ))?;
        let mut loader = target.connect(a)?;
        loader.batch_execute(&format!(
            "CREATE TABLE probe AS SELECT g AS id, md5(g::text) AS a, md5((g * 7)::text) AS b \
             FROM generate_series(1, {PROBE_ROWS}) g"
        ))?;
        loader.close()?;
        admin.batch_execute("CHECKPOINT")?;
        self.compose(&["exec", "-T", SERVICE, "sync"])?;
        let probe_bytes: i64 = admin
            .query_one("SELECT pg_database_size($1::name)", &[&a])?
            .get(0);

        let first = self.free_bytes()?;
        std::thread::sleep(Duration::from_secs(1));
        let before = self.free_bytes()?;
        let drift = (first - before).abs();
        if drift * 20 > probe_bytes {
            return Ok(Measurement {
                probe_bytes,
                growth_bytes: 0,
                note: Some(format!(
                    "free space moved by {drift} bytes during the baseline (other activity on the filesystem)"
                )),
                interfered: true,
            });
        }

        // The loader's backend may take a moment to exit after the client closed.
        let mut attempt = 0;
        loop {
            match admin.batch_execute(&format!(
                "CREATE DATABASE {} TEMPLATE {} STRATEGY FILE_COPY",
                quote_ident(b),
                quote_ident(a)
            )) {
                Ok(()) => break,
                Err(e) if e.code() == Some(&SqlState::OBJECT_IN_USE) && attempt < 20 => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(e.into()),
            }
        }
        admin.batch_execute("CHECKPOINT")?;
        self.compose(&["exec", "-T", SERVICE, "sync"])?;
        let after = self.free_bytes()?;
        Ok(Measurement {
            probe_bytes,
            growth_bytes: (before - after).max(0),
            note: None,
            interfered: false,
        })
    }

    /// Available bytes on the filesystem holding `$PGDATA` (POSIX `df` so that
    /// Alpine/BusyBox images work too).
    fn free_bytes(&self) -> Result<i64> {
        let out = self.compose(&[
            "exec",
            "-T",
            SERVICE,
            "sh",
            "-c",
            r#"df -Pk "$PGDATA" | awk 'NR==2 {print $4}'"#,
        ])?;
        let kib: i64 = out
            .trim()
            .parse()
            .with_context(|| format!("unexpected df output '{}'", out.trim()))?;
        Ok(kib * 1024)
    }
}

struct Measurement {
    probe_bytes: i64,
    growth_bytes: i64,
    note: Option<String>,
    interfered: bool,
}

/// Less than 10% growth means the clone shared blocks.
fn classify(m: &Measurement) -> CowCapability {
    if m.interfered || m.probe_bytes <= 0 {
        CowCapability::Unknown
    } else if m.growth_bytes * 10 < m.probe_bytes {
        CowCapability::Verified
    } else {
        CowCapability::Copy
    }
}

fn probe_name() -> String {
    format!(
        "devflow_probe_{}",
        &Uuid::new_v4().simple().to_string()[..12]
    )
}

fn drop_probe_database(admin: &mut Client, name: &str) -> Result<()> {
    admin.batch_execute(&format!(
        "DROP DATABASE IF EXISTS {} WITH (FORCE)",
        quote_ident(name)
    ))?;
    Ok(())
}

// ---- pure helpers ---------------------------------------------------------

/// The project name reduced to lowercase letters, digits and single hyphens
/// (at most 30 characters, never empty): what compose project names and DNS
/// labels accept.
pub(crate) fn sanitize_name(name: &str) -> String {
    let mut clean = String::new();
    for c in name.to_lowercase().chars() {
        let c = if c.is_ascii_lowercase() || c.is_ascii_digit() {
            c
        } else {
            '-'
        };
        if c != '-' || !clean.ends_with('-') {
            clean.push(c);
        }
    }
    let clean: String = clean.trim_matches('-').chars().take(30).collect();
    let clean = clean.trim_end_matches('-');
    if clean.is_empty() {
        "project".to_string()
    } else {
        clean.to_string()
    }
}

/// `devflow-<name>-<id8>` with the name reduced to what compose accepts.
fn compose_project_name(name: &str, id: Uuid) -> String {
    format!(
        "devflow-{}-{}",
        sanitize_name(name),
        &id.simple().to_string()[..8]
    )
}

/// The OrbStack name of a service: `<service>.<project>-<id8>.devflow.orb.local`.
/// The project's identity is part of it, so two projects with the same name
/// (or service key) never share a name. Underscores of the service key become
/// hyphens, which is what a host name accepts.
fn orbstack_host(name: &str, id: Uuid, service_key: &str) -> String {
    let service = service_key.replace('_', "-");
    format!(
        "{}.{}-{}.devflow.orb.local",
        service.trim_end_matches('-'),
        sanitize_name(name),
        &id.simple().to_string()[..8]
    )
}

/// Whether `docker info`'s `OperatingSystem` names OrbStack.
fn is_orbstack(operating_system: &str) -> bool {
    operating_system.contains("OrbStack")
}

/// Whether the Docker host is OrbStack (`docker info`). A failed query is
/// "no" and is asked again next time; an answer is kept for the process.
pub(crate) fn orbstack() -> bool {
    static DETECTED: OnceLock<bool> = OnceLock::new();
    if let Some(known) = DETECTED.get() {
        return *known;
    }
    let output = Command::new("docker")
        .args(["info", "--format", "{{.OperatingSystem}}"])
        .stdin(std::process::Stdio::null())
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let detected = is_orbstack(&String::from_utf8_lossy(&out.stdout));
            *DETECTED.get_or_init(|| detected)
        }
        _ => false,
    }
}

/// The host ports a server may take when `port = 0`, in the order they are
/// tried: a starting point derived from the project and service identity in
/// [20000, 40000), then its successors, wrapping around the range. Bounded
/// by [`PORT_ATTEMPTS`].
pub fn candidate_ports(project: Uuid, service_key: &str) -> Vec<u16> {
    ports_from_seed(&format!("devflow.endpoint.v1\0{project}\0{service_key}"))
}

/// The candidates for an identity written as `seed`: a starting point derived
/// from its SHA-256 in [20000, 40000), then its successors. Container
/// services seed with the project, the service key and the branch.
pub(crate) fn ports_from_seed(seed: &str) -> Vec<u16> {
    let digest = Sha256::digest(seed);
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    // The remainder is below PORT_SPAN, so it fits.
    let start = (u64::from_be_bytes(head) % u64::from(PORT_SPAN)) as u16;
    (0..PORT_ATTEMPTS)
        .map(|i| PORT_MIN + (start + i) % PORT_SPAN)
        .collect()
}

/// Why a first allocation did not produce a port.
#[derive(Debug)]
pub(crate) enum AllocationError {
    /// Every candidate was refused (one reason each).
    Exhausted(Vec<String>),
    /// An effect or a journal write failed; the allocation is left resumable.
    Other(anyhow::Error),
}

/// Choose the host port of a server that has none recorded, resumable after
/// an interruption at any point.
///
/// - `pending` is the candidate a previous run journaled before asking Docker
///   to publish it; `running` is the port a running container of this server
///   already publishes. A running container's port is one Docker actually
///   bound: it is kept and recorded, whatever the candidates say now.
/// - Otherwise the candidates are tried in order starting at `pending` (every
///   earlier one was refused by a confirmed failed bind). Each is journaled
///   (`journal(Some(port), None)`) before `claim` is asked to bring the
///   server up on it, and the next candidate is only tried after `claim`
///   reported a [`BindFailure`]. Any other failure stops with the candidate
///   still journaled, so the next run resumes there.
/// - Success is recorded with `journal(None, Some(port))`.
pub(crate) fn allocate_port(
    candidates: &[u16],
    pending: Option<u16>,
    running: Option<u16>,
    journal: &mut dyn FnMut(Option<u16>, Option<u16>) -> Result<()>,
    claim: &mut dyn FnMut(u16) -> Result<()>,
) -> std::result::Result<u16, AllocationError> {
    if let Some(port) = running {
        claim(port).map_err(AllocationError::Other)?;
        journal(None, Some(port)).map_err(AllocationError::Other)?;
        return Ok(port);
    }
    let first = pending
        .and_then(|p| candidates.iter().position(|c| *c == p))
        .unwrap_or(0);
    let mut refused = Vec::new();
    for &port in &candidates[first..] {
        journal(Some(port), None).map_err(AllocationError::Other)?;
        match claim(port) {
            Ok(()) => {
                journal(None, Some(port)).map_err(AllocationError::Other)?;
                return Ok(port);
            }
            Err(error) => match error.downcast::<BindFailure>() {
                Ok(failure) => refused.push(format!("{port}: {}", failure.0)),
                Err(error) => return Err(AllocationError::Other(error)),
            },
        }
    }
    // Nothing is in flight any more: the next start begins again.
    journal(None, None).map_err(AllocationError::Other)?;
    Err(AllocationError::Exhausted(refused))
}

/// Docker could not publish the port: something else holds it.
#[derive(Debug)]
pub(crate) struct BindFailure(pub(crate) String);

impl std::fmt::Display for BindFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BindFailure {}

/// Whether `docker compose up` failed because the host port is taken. Docker
/// words it differently by version and platform.
pub(crate) fn is_bind_failure(stderr: &str) -> bool {
    let text = stderr.to_lowercase();
    text.contains("port is already allocated")
        || text.contains("address already in use")
        || text.contains("ports are not available")
        || (text.contains("bind for") && text.contains("failed"))
}

/// The line of Docker's message that names the conflict.
pub(crate) fn summarize_bind_failure(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|line| is_bind_failure(line))
        .unwrap_or_else(|| stderr.trim())
        .to_string()
}

/// The compose file. Strings are JSON-quoted (valid YAML) so no value can
/// break out of its scalar. `127.0.0.1` in the health check avoids the
/// entrypoint's temporary socket-only server reporting ready too early.
///
/// With a `domain` (OrbStack), the container is also reachable by that name
/// from other containers; the published loopback port is unaffected.
fn render_compose(
    image: &str,
    project: Uuid,
    bind: &str,
    port: u16,
    password: &str,
    domain: Option<&str>,
) -> String {
    let q = |s: &str| serde_json::to_string(s).expect("strings serialize");
    let host = if bind.contains(':') {
        format!("[{bind}]")
    } else {
        bind.to_string()
    };
    let port = if port == 0 {
        String::new()
    } else {
        port.to_string()
    };
    let domain = domain
        .map(|name| format!("\n      dev.orbstack.domains: {}", q(name)))
        .unwrap_or_default();
    format!(
        r#"services:
  {SERVICE}:
    image: {image}
    restart: unless-stopped
    command: ["postgres", "-c", "file_copy_method=clone"]
    environment:
      POSTGRES_PASSWORD: {password}
    ports:
      - {ports}
    volumes:
      - pgdata:/var/lib/postgresql
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -h 127.0.0.1 -U postgres"]
      interval: 2s
      timeout: 3s
      retries: 30
    labels:
      dev.devflow.project: {project}
      dev.devflow.version: "1"{domain}
volumes:
  pgdata: {{}}
"#,
        image = q(image),
        password = q(password),
        ports = q(&format!("{host}:{port}:5432")),
        project = q(&project.to_string()),
    )
}

/// `<program> compose -p PROJECT -f FILE ARGS…`, run in `dir` and over within
/// `budget`: on the deadline the command and what it started are killed and
/// reaped (see [`crate::subprocess`]) and the answer is an error saying so.
fn compose_query(
    program: &std::ffi::OsStr,
    project: &str,
    file: &std::path::Path,
    dir: Option<&std::path::Path>,
    args: &[&str],
    budget: Duration,
) -> Result<String> {
    let name = args.first().copied().unwrap_or("");
    anyhow::ensure!(
        !budget.is_zero(),
        "no time was left to run `docker compose {name}`"
    );
    let mut command = Command::new(program);
    command
        .args(["compose", "-p", project, "-f"])
        .arg(file)
        .args(args);
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let out = crate::subprocess::run(command, budget)
        .context("cannot run `docker compose` (is Docker installed and running?)")?;
    if out.timed_out {
        bail!(
            "`docker compose {name}` did not answer within {} and was stopped",
            humantime::format_duration(Duration::from_millis(
                budget.as_millis().try_into().unwrap_or(u64::MAX)
            ))
        );
    }
    if !out.ok {
        bail!(
            "`docker compose {name}` failed: {}",
            crate::redact::text(out.message.as_deref().unwrap_or("").trim())
        );
    }
    anyhow::ensure!(
        !out.truncated,
        "`docker compose {name}` printed more than {} MiB, which devflow does not read",
        crate::subprocess::DEFAULT_MAX_OUTPUT >> 20
    );
    Ok(out.stdout)
}

/// `127.0.0.1:49153`, `0.0.0.0:49153` or `[::]:49153` (first line).
fn parse_published_port(output: &str) -> Option<u16> {
    output
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())?
        .rsplit_once(':')?
        .1
        .parse()
        .ok()
}

/// `docker compose ps --format json`: one object per line, or one array.
fn parse_ps(output: &str) -> ContainerState {
    let text = output.trim();
    let items: Vec<serde_json::Value> = if text.starts_with('[') {
        serde_json::from_str(text).unwrap_or_default()
    } else {
        text.lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    };
    let mut state = ContainerState::default();
    for item in items {
        if item["Service"].as_str().is_some_and(|s| s != SERVICE) {
            continue;
        }
        state.exists = true;
        state.running |= item["State"].as_str() == Some("running");
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_that_was_never_started_offers_to_start_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = ManagedServer::for_test(dir.path().join("server"), "docker".into());
        server.root = PathBuf::from("/shop");
        for error in [
            server.published_port().unwrap_err(),
            server.admin_password().unwrap_err(),
        ] {
            assert!(!error.to_string().contains("`devflow"), "{error}");
            assert_eq!(
                crate::remedy::remedies_of(&error),
                [Remedy::StartDatabase {
                    root: PathBuf::from("/shop")
                }]
            );
        }
    }

    #[test]
    fn an_unreadable_admin_file_is_described_without_its_content() {
        let dir = tempfile::tempdir().unwrap();
        let server = ManagedServer {
            dir: dir.path().to_path_buf(),
            compose_project: "devflow-x".into(),
            image: "postgres:18".into(),
            root: PathBuf::new(),
            project_id: Uuid::nil(),
            service_key: "db".into(),
            container_host: "x".into(),
            bind: "127.0.0.1".into(),
            port: 0,
            docker: "docker".into(),
        };
        for (text, wanted) in [
            (
                r#"{"version": 1, "password": "REVIEWSECRET", "port": "REVIEWSECRET"}"#,
                "port: wrong type of value",
            ),
            (r#"{"version": 1, "password": 7}"#, "password: wrong type"),
            (
                r#"{"version": 1, "password": "REVIEWSECRET"#,
                "ends unexpectedly",
            ),
            ("password=REVIEWSECRET", "not valid JSON"),
        ] {
            std::fs::write(dir.path().join("admin.json"), text).unwrap();
            let error = server.load_state().unwrap_err();
            let shown = format!("{error} | {error:#} | {error:?}");
            assert!(shown.contains(wanted), "{text}\n{shown}");
            assert!(
                shown.contains("refusing to regenerate the superuser password"),
                "{shown}"
            );
            assert!(!shown.contains("REVIEWSECRET"), "{text}\n{shown}");
        }
    }

    #[test]
    fn compose_project_names_are_sanitized() {
        let id: Uuid = "0b3c9d2e-1111-4222-8333-444455556666".parse().unwrap();
        assert_eq!(
            compose_project_name("Shop App!", id),
            "devflow-shop-app-0b3c9d2e"
        );
        assert_eq!(compose_project_name("--", id), "devflow-project-0b3c9d2e");
        assert_eq!(compose_project_name("a__b", id), "devflow-a-b-0b3c9d2e");
        let long = compose_project_name(&"x".repeat(80), id);
        assert!(long.len() < 50, "{long}");
    }

    #[test]
    fn compose_file_matches_the_contract() {
        let id: Uuid = "0b3c9d2e-1111-4222-8333-444455556666".parse().unwrap();
        // The port is always fixed once allocated; 0 stays "docker picks".
        let text = render_compose("postgres:18", id, "127.0.0.1", 23456, "s3cret", None);
        assert!(!text.lines().any(|l| l.starts_with("version")));
        assert!(text.contains("image: \"postgres:18\""));
        assert!(text.contains("\"127.0.0.1:23456:5432\""));
        assert!(text.contains("file_copy_method=clone"));
        assert!(text.contains("pgdata:/var/lib/postgresql\n"));
        assert!(text.contains("dev.devflow.project: \"0b3c9d2e-1111-4222-8333-444455556666\""));
        assert!(text.contains("restart: unless-stopped"));
        assert!(text.contains("POSTGRES_PASSWORD: \"s3cret\""));
        assert!(!text.contains("orbstack"), "no OrbStack label elsewhere");
        let ephemeral = render_compose("postgres:18", id, "127.0.0.1", 0, "x", None);
        assert!(ephemeral.contains("\"127.0.0.1::5432\""));
        let v6 = render_compose("postgres:18", id, "::1", 5433, "x", None);
        assert!(v6.contains("\"[::1]:5433:5432\""));
        // A hostile image string stays inside its scalar.
        let hostile = render_compose("x\"\n  evil: 1", id, "127.0.0.1", 0, "x", None);
        assert!(!hostile.contains("\n  evil: 1"));
    }

    #[test]
    fn orbstack_gets_a_domain_label_and_the_loopback_port_is_unchanged() {
        let id: Uuid = "0b3c9d2e-1111-4222-8333-444455556666".parse().unwrap();
        let host = orbstack_host("Shop App!", id, "db");
        assert_eq!(host, "db.shop-app-0b3c9d2e.devflow.orb.local");
        let text = render_compose("postgres:18", id, "127.0.0.1", 23456, "x", Some(&host));
        assert!(
            text.contains(
                "      dev.orbstack.domains: \"db.shop-app-0b3c9d2e.devflow.orb.local\"\n"
            ),
            "{text}"
        );
        assert!(text.contains("\"127.0.0.1:23456:5432\""));
        // The label sits with the other labels, before the volumes.
        let labels = text.find("    labels:").unwrap();
        let label = text.find("dev.orbstack.domains").unwrap();
        assert!(labels < label && label < text.find("\nvolumes:").unwrap());
        // A host name accepts no underscore; the project's identity is in the name.
        assert_eq!(
            orbstack_host("shop", id, "main_db"),
            "main-db.shop-0b3c9d2e.devflow.orb.local"
        );
        assert_eq!(
            orbstack_host("shop", id, "db_"),
            "db.shop-0b3c9d2e.devflow.orb.local"
        );
        let other: Uuid = "ffffffff-1111-4222-8333-444455556666".parse().unwrap();
        assert_ne!(host, orbstack_host("Shop App!", other, "db"));
    }

    #[test]
    fn orbstack_is_recognised_from_docker_info() {
        assert!(is_orbstack("OrbStack\n"));
        assert!(!is_orbstack("Docker Desktop"));
        assert!(!is_orbstack("Ubuntu 24.04.1 LTS"));
        assert!(!is_orbstack(""));
    }

    #[test]
    fn candidate_ports_are_stable_distinct_and_bounded() {
        let id: Uuid = "0b3c9d2e-1111-4222-8333-444455556666".parse().unwrap();
        let ports = candidate_ports(id, "db");
        assert_eq!(
            ports,
            candidate_ports(id, "db"),
            "same identity, same ports"
        );
        assert_eq!(ports.len(), usize::from(PORT_ATTEMPTS));
        assert!(
            ports.iter().all(|p| (20_000..40_000).contains(p)),
            "{ports:?}"
        );
        let mut unique = ports.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ports.len());
        // The identity decides the starting point.
        let other: Uuid = "ffffffff-1111-4222-8333-444455556666".parse().unwrap();
        assert_ne!(ports[0], candidate_ports(other, "db")[0]);
        assert_ne!(ports[0], candidate_ports(id, "main")[0]);
        // Wrapping around the end of the range stays inside it.
        for n in 0..200u32 {
            let id = Uuid::from_u128(u128::from(n) * 0x9e37_79b9_7f4a_7c15);
            assert!(
                candidate_ports(id, "db")
                    .iter()
                    .all(|p| (PORT_MIN..PORT_MIN + PORT_SPAN).contains(p))
            );
        }
    }

    /// A fake host: the ports something else holds, the port a Docker
    /// container publishes, what `admin.json` says, and a crash switch.
    #[derive(Default)]
    struct Host {
        occupied: Vec<u16>,
        published: Option<u16>,
        pending: Option<u16>,
        port: Option<u16>,
        claims: Vec<u16>,
        /// Fail the journal write that records the final port (a crash
        /// after Docker bound it, before `admin.json` was updated).
        crash_before_recording: bool,
    }

    impl Host {
        fn allocate(&mut self, candidates: &[u16]) -> std::result::Result<u16, AllocationError> {
            let state = std::cell::RefCell::new(&mut *self);
            let (pending, running) = {
                let host = state.borrow();
                (host.pending, host.published)
            };
            allocate_port(
                candidates,
                pending,
                running,
                &mut |pending, port| {
                    let mut host = state.borrow_mut();
                    if port.is_some() && host.crash_before_recording {
                        anyhow::bail!("interrupted before admin.json was updated");
                    }
                    host.pending = pending;
                    if port.is_some() {
                        host.port = port;
                    }
                    Ok(())
                },
                &mut |port| {
                    let mut host = state.borrow_mut();
                    host.claims.push(port);
                    if host.occupied.contains(&port) {
                        return Err(BindFailure(format!("{port} is in use")).into());
                    }
                    host.published = Some(port);
                    Ok(())
                },
            )
        }
    }

    const CANDIDATES: [u16; 4] = [32220, 32221, 32222, 32223];

    #[test]
    fn a_busy_candidate_is_skipped_and_the_result_recorded() {
        let mut host = Host {
            occupied: vec![32220],
            ..Host::default()
        };
        assert_eq!(host.allocate(&CANDIDATES).unwrap(), 32221);
        assert_eq!((host.port, host.pending), (Some(32221), None));
        assert_eq!(host.claims, [32220, 32221]);
    }

    #[test]
    fn an_interrupted_allocation_keeps_the_port_docker_bound() {
        // 32220 is occupied, Docker publishes 32221, and the run is interrupted
        // before the port is recorded.
        let mut host = Host {
            occupied: vec![32220],
            crash_before_recording: true,
            ..Host::default()
        };
        assert!(matches!(
            host.allocate(&CANDIDATES),
            Err(AllocationError::Other(_))
        ));
        assert_eq!(host.published, Some(32221));
        assert_eq!(
            (host.port, host.pending),
            (None, Some(32221)),
            "the tentative allocation was journaled before Docker was asked"
        );
        // 32220 is released and the start is retried: 32221 stays.
        host.occupied.clear();
        host.crash_before_recording = false;
        host.claims.clear();
        assert_eq!(host.allocate(&CANDIDATES).unwrap(), 32221);
        assert_eq!((host.port, host.pending), (Some(32221), None));
        assert!(
            !host.claims.contains(&32220),
            "an earlier candidate is never revisited: {:?}",
            host.claims
        );
    }

    #[test]
    fn an_interruption_before_the_bind_resumes_at_the_journaled_candidate() {
        // Journaled 32221 (32220 was refused), then stopped before any bind.
        let mut host = Host {
            pending: Some(32221),
            ..Host::default()
        };
        assert_eq!(host.allocate(&CANDIDATES).unwrap(), 32221);
        assert_eq!(host.claims, [32221], "32220 is not tried again");
        // The journaled candidate was taken meanwhile: the failed bind is
        // confirmed, so the next one follows.
        let mut host = Host {
            pending: Some(32221),
            occupied: vec![32221],
            ..Host::default()
        };
        assert_eq!(host.allocate(&CANDIDATES).unwrap(), 32222);
        assert_eq!(host.claims, [32221, 32222]);
    }

    #[test]
    fn a_failure_that_is_not_a_refused_bind_leaves_the_candidate_journaled() {
        let mut calls = Vec::new();
        let mut pending = None;
        let result = allocate_port(
            &CANDIDATES,
            None,
            None,
            &mut |p, _| {
                pending = p;
                Ok(())
            },
            &mut |port| {
                calls.push(port);
                anyhow::bail!("the container did not become healthy")
            },
        );
        assert!(matches!(result, Err(AllocationError::Other(_))));
        assert_eq!(calls, [32220], "it does not go on to the next candidate");
        assert_eq!(pending, Some(32220), "the next start resumes here");
    }

    #[test]
    fn a_container_already_running_without_a_record_is_adopted() {
        // A start from before the journal existed was interrupted after Docker
        // published 32222: whatever the candidates are, that port is kept.
        let mut host = Host {
            published: Some(32222),
            ..Host::default()
        };
        assert_eq!(host.allocate(&CANDIDATES).unwrap(), 32222);
        assert_eq!(host.port, Some(32222));
        assert_eq!(host.claims, [32222]);
    }

    #[test]
    fn every_candidate_refused_reports_each_and_clears_the_journal() {
        let mut host = Host {
            occupied: CANDIDATES.to_vec(),
            ..Host::default()
        };
        let Err(AllocationError::Exhausted(refused)) = host.allocate(&CANDIDATES) else {
            panic!("all candidates are busy");
        };
        assert_eq!(refused.len(), CANDIDATES.len());
        assert!(refused[0].starts_with("32220: "), "{refused:?}");
        assert_eq!((host.port, host.pending), (None, None));
    }

    #[test]
    fn docker_bind_failures_are_recognised_in_their_usual_wordings() {
        for text in [
            "Error response from daemon: driver failed programming external connectivity on endpoint x: Bind for 127.0.0.1:20000 failed: port is already allocated",
            "Error starting userland proxy: listen tcp4 127.0.0.1:20000: bind: address already in use",
            "Ports are not available: exposing port TCP 127.0.0.1:20000 -> 0.0.0.0:0: listen tcp 127.0.0.1:20000: bind: Only one usage of each socket address",
        ] {
            assert!(is_bind_failure(text), "{text}");
        }
        for text in [
            "pull access denied for postgres",
            "container is unhealthy",
            "no such image",
        ] {
            assert!(!is_bind_failure(text), "{text}");
        }
        let summary = summarize_bind_failure(
            "Container x Starting\nError response from daemon: Bind for 127.0.0.1:20000 failed: port is already allocated\n",
        );
        assert!(summary.contains("port is already allocated"), "{summary}");
        assert!(!summary.contains("Starting"), "{summary}");
    }

    #[test]
    fn published_port_parsing() {
        assert_eq!(parse_published_port("127.0.0.1:49153\n"), Some(49153));
        assert_eq!(parse_published_port("0.0.0.0:5432"), Some(5432));
        assert_eq!(parse_published_port("[::]:32768\n"), Some(32768));
        assert_eq!(parse_published_port(""), None);
        assert_eq!(parse_published_port("garbage"), None);
    }

    #[test]
    fn ps_output_in_both_formats() {
        let line = r#"{"Service":"pg","State":"running","Health":"healthy"}"#;
        assert_eq!(
            parse_ps(line),
            ContainerState {
                exists: true,
                running: true
            }
        );
        let array = r#"[{"Service":"pg","State":"exited"}]"#;
        assert_eq!(
            parse_ps(array),
            ContainerState {
                exists: true,
                running: false
            }
        );
        assert_eq!(parse_ps(""), ContainerState::default());
        let other = r#"{"Service":"other","State":"running"}"#;
        assert_eq!(parse_ps(other), ContainerState::default());
    }

    #[test]
    fn probe_classification() {
        let m = |growth, interfered| Measurement {
            probe_bytes: 64_000_000,
            growth_bytes: growth,
            note: None,
            interfered,
        };
        assert_eq!(classify(&m(200_000, false)), CowCapability::Verified);
        assert_eq!(classify(&m(6_399_999, false)), CowCapability::Verified);
        assert_eq!(classify(&m(6_400_000, false)), CowCapability::Copy);
        assert_eq!(classify(&m(64_000_000, false)), CowCapability::Copy);
        assert_eq!(classify(&m(0, true)), CowCapability::Unknown);
    }
}

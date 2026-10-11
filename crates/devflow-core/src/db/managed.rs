//! Managed server: a `docker compose` project under the data dir, its
//! generated superuser credentials, and the copy-on-write probe.
use super::admin::{Target, quote_ident, random_password};
use crate::config::{CowCapability, PostgresSection};
use crate::project::Project;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use postgres::Client;
use postgres::error::SqlState;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use uuid::Uuid;

const SERVICE: &str = "pg";
/// Probe data size target (about 64 MB of heap).
const PROBE_ROWS: u32 = 600_000;

pub struct ManagedServer {
    /// `<data dir>/postgres/<project id>`.
    pub dir: PathBuf,
    /// Compose project name, `devflow-<sanitized name>-<id8>`.
    pub compose_project: String,
    pub image: String,
    project_id: Uuid,
    bind: String,
    port: u16,
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
    pub fn new(project: &Project, settings: &PostgresSection) -> Result<Self> {
        let id = project.id();
        anyhow::ensure!(
            settings.bind.parse::<IpAddr>().is_ok(),
            "postgres.bind must be an IP address, not '{}'",
            settings.bind
        );
        Ok(Self {
            dir: crate::paths::data_dir()?
                .join("postgres")
                .join(id.to_string()),
            compose_project: compose_project_name(project.name(), id),
            image: settings.image.clone(),
            project_id: id,
            bind: settings.bind.clone(),
            port: settings.port,
        })
    }

    pub fn compose_file(&self) -> PathBuf {
        self.dir.join("compose.yaml")
    }

    fn admin_file(&self) -> PathBuf {
        self.dir.join("admin.json")
    }

    /// Write the compose file (and the credentials on first use), then
    /// `docker compose up -d --wait`.
    pub fn start(&self) -> Result<()> {
        let state = self.load_or_create_state()?;
        crate::fsutil::write_atomic(
            &self.compose_file(),
            self.render_compose(&state.password).as_bytes(),
            true,
        )?;
        if let Err(error) = self.compose(&["up", "-d", "--wait"]) {
            let logs = self
                .compose(&["logs", "--no-color", "--tail", "15", SERVICE])
                .unwrap_or_default();
            bail!(
                "the PostgreSQL container did not become healthy: {error:#}\n{}",
                logs.trim()
            );
        }
        Ok(())
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
        anyhow::ensure!(
            self.compose_file().is_file(),
            "the managed PostgreSQL server has not been started; run `devflow db up`"
        );
        let out = self.compose(&["port", SERVICE, "5432"])?;
        parse_published_port(&out).with_context(|| {
            "the managed PostgreSQL server is not running; run `devflow db up`".to_string()
        })
    }

    pub fn container_state(&self) -> Result<ContainerState> {
        if !self.compose_file().is_file() {
            return Ok(ContainerState::default());
        }
        let out = self.compose(&["ps", "--all", "--format", "json"])?;
        Ok(parse_ps(&out))
    }

    /// The generated superuser password.
    pub fn admin_password(&self) -> Result<String> {
        let state = self
            .load_state()?
            .context("the managed PostgreSQL server has not been started; run `devflow db up`")?;
        Ok(state.password)
    }

    // ---- docker ----

    fn compose(&self, args: &[&str]) -> Result<String> {
        let mut command = Command::new("docker");
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

    fn render_compose(&self, password: &str) -> String {
        render_compose(
            &self.image,
            self.project_id,
            &self.bind,
            self.port,
            password,
        )
    }

    // ---- admin.json ----

    fn load_state(&self) -> Result<Option<AdminState>> {
        match std::fs::read(self.admin_file()) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "{} is unreadable; refusing to regenerate the superuser password",
                    self.admin_file().display()
                )
            })?)),
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

/// `devflow-<name>-<id8>` with the name reduced to what compose accepts.
fn compose_project_name(name: &str, id: Uuid) -> String {
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
    let clean = if clean.is_empty() { "project" } else { clean };
    format!("devflow-{clean}-{}", &id.simple().to_string()[..8])
}

/// The compose file. Strings are JSON-quoted (valid YAML) so no value can
/// break out of its scalar. `127.0.0.1` in the health check avoids the
/// entrypoint's temporary socket-only server reporting ready too early.
fn render_compose(image: &str, project: Uuid, bind: &str, port: u16, password: &str) -> String {
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
      dev.devflow.version: "1"
volumes:
  pgdata: {{}}
"#,
        image = q(image),
        password = q(password),
        ports = q(&format!("{host}:{port}:5432")),
        project = q(&project.to_string()),
    )
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
        let text = render_compose("postgres:18", id, "127.0.0.1", 0, "s3cret");
        assert!(!text.lines().any(|l| l.starts_with("version")));
        assert!(text.contains("image: \"postgres:18\""));
        assert!(text.contains("\"127.0.0.1::5432\""));
        assert!(text.contains("file_copy_method=clone"));
        assert!(text.contains("pgdata:/var/lib/postgresql\n"));
        assert!(text.contains("dev.devflow.project: \"0b3c9d2e-1111-4222-8333-444455556666\""));
        assert!(text.contains("restart: unless-stopped"));
        assert!(text.contains("POSTGRES_PASSWORD: \"s3cret\""));
        let fixed = render_compose("postgres:18", id, "::1", 5433, "x");
        assert!(fixed.contains("\"[::1]:5433:5432\""));
        // A hostile image string stays inside its scalar.
        let hostile = render_compose("x\"\n  evil: 1", id, "127.0.0.1", 0, "x");
        assert!(!hostile.contains("\n  evil: 1"));
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

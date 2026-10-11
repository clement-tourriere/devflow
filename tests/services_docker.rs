//! Docker tests of the container services (Valkey and Redis branches).
//!
//! Ignored by default: they need Docker and the images `valkey/valkey:8-alpine`
//! and `debian:stable-slim` (already present; nothing is pulled by a test run
//! that has them). Run them with
//!
//! ```sh
//! cargo test -p devflow --test services_docker -- --ignored --test-threads=1
//! ```
//!
//! Isolation: every test uses its own project id, data directory and
//! `DEVFLOW_TEST_LABEL`, which devflow puts on every container, volume and
//! helper it creates; a guard removes everything carrying the label when the
//! test ends, even by a panic. Nothing else on the machine is touched.
//!
//! They assert `reflink` only when `DEVFLOW_EXPECT_COW=verified` is set (the
//! measured capability of the Docker engine); otherwise `reflink` or `copy`
//! are both accepted and reported.
use devflow_core::Project;
use devflow_core::config::CowCapability;
use devflow_core::db::DataSource;
use devflow_core::receipt::{Phase, PhaseState};
use devflow_core::services::catalog::{
    CloneMethod, Coordinator, Op, OpKind, RecordState, ResetPlan, SourceRef, Step, Store,
};
use devflow_core::services::{NewBranch, Service, docker, probe};
use devflow_core::setup::{self, CreateRequest, NoProgress, SetupOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use uuid::Uuid;

// ---- a minimal RESP client, independent of devflow's own -------------------

#[derive(Debug)]
struct Redis {
    reader: BufReader<TcpStream>,
}

#[derive(Debug, Clone, PartialEq)]
enum Reply {
    Text(String),
    Int(i64),
    Nil,
    Array(Vec<Reply>),
}

impl Reply {
    fn text(&self) -> String {
        match self {
            Reply::Text(t) => t.clone(),
            Reply::Int(i) => i.to_string(),
            Reply::Nil => String::new(),
            Reply::Array(items) => items.iter().map(Reply::text).collect::<Vec<_>>().join(" "),
        }
    }
}

impl Redis {
    fn connect(port: u16) -> Redis {
        let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        Redis {
            reader: BufReader::new(stream),
        }
    }

    /// Connect and authenticate; `Err` is the server's refusal.
    fn auth(port: u16, password: &str) -> Result<Redis, String> {
        let mut conn = Redis::connect(port);
        conn.cmd(&["AUTH", password])?;
        Ok(conn)
    }

    fn send(&mut self, args: &[&str]) {
        let mut frame = format!("*{}\r\n", args.len()).into_bytes();
        for arg in args {
            frame.extend(format!("${}\r\n{arg}\r\n", arg.len()).into_bytes());
        }
        self.reader.get_mut().write_all(&frame).unwrap();
    }

    fn cmd(&mut self, args: &[&str]) -> Result<Reply, String> {
        self.send(args);
        self.read()
    }

    fn read(&mut self) -> Result<Reply, String> {
        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        let line = line.trim_end_matches("\r\n").to_string();
        let (kind, rest) = line.split_at(1);
        match kind {
            "+" => Ok(Reply::Text(rest.into())),
            "-" => Err(rest.into()),
            ":" => Ok(Reply::Int(rest.parse().unwrap())),
            "$" => {
                let len: i64 = rest.parse().unwrap();
                if len < 0 {
                    return Ok(Reply::Nil);
                }
                let mut bytes = vec![0u8; len as usize + 2];
                self.reader.read_exact(&mut bytes).unwrap();
                bytes.truncate(len as usize);
                Ok(Reply::Text(String::from_utf8_lossy(&bytes).into_owned()))
            }
            "*" => {
                let len: i64 = rest.parse().unwrap();
                let mut items = Vec::new();
                for _ in 0..len.max(0) {
                    items.push(self.read()?);
                }
                Ok(Reply::Array(items))
            }
            other => panic!("unexpected reply {other}{rest}"),
        }
    }

    fn get(&mut self, key: &str) -> Option<String> {
        match self.cmd(&["GET", key]).unwrap() {
            Reply::Nil => None,
            other => Some(other.text()),
        }
    }

    fn dbsize(&mut self) -> i64 {
        match self.cmd(&["DBSIZE"]).unwrap() {
            Reply::Int(n) => n,
            other => panic!("DBSIZE: {other:?}"),
        }
    }

    /// `SET key:i value` for `0..count`, pipelined in batches.
    fn fill(&mut self, prefix: &str, count: usize, value_len: usize) {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64 ^ count as u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for batch in (0..count).collect::<Vec<_>>().chunks(500) {
            for i in batch {
                // Hex of a pseudo-random stream: not compressible by LZF.
                let mut value = String::with_capacity(value_len);
                while value.len() < value_len {
                    value.push_str(&format!("{:016x}", next()));
                }
                value.truncate(value_len);
                self.send(&["SET", &format!("{prefix}:{i}"), &value]);
            }
            for _ in batch {
                self.read().unwrap();
            }
        }
    }
}

// ---- fixtures -----------------------------------------------------------------

/// Everything a test needs, and the cleanup of everything it creates.
struct Fixture {
    _dir: tempfile::TempDir,
    /// A private copy of the `devflow` binary: the coordinator and the CLI
    /// tests run it, and it must not vanish if the build directory is swept.
    bin: std::path::PathBuf,
    label: String,
    project: Project,
}

/// The `devflow` binary Cargo built, read once when the first test needs it:
/// a build directory that is swept or rebuilt while the suite runs must not
/// take the coordinator's executable away.
fn write_binary(to: &Path) {
    static BINARY: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    let bytes = BINARY.get_or_init(|| std::fs::read(env!("CARGO_BIN_EXE_devflow")).unwrap());
    std::fs::write(to, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn docker_ok(args: &[&str]) -> String {
    docker::run(args).unwrap_or_default()
}

impl Fixture {
    /// A Git repository with a `devflow.toml` holding `services` (TOML text).
    fn new(services: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let label = format!("df-svc-{}", &Uuid::new_v4().simple().to_string()[..12]);
        let bin = base.join("devflow");
        write_binary(&bin);
        // Process-wide, so the tests run on one thread (`--test-threads=1`).
        unsafe {
            std::env::set_var("DEVFLOW_DATA_DIR", base.join("data"));
            std::env::set_var("DEVFLOW_CONFIG_DIR", base.join("config"));
            std::env::set_var("DEVFLOW_BIN", &bin);
            std::env::set_var("DEVFLOW_TEST_LABEL", &label);
            for var in [
                "DEVFLOW_TEST_COPY_DELAY_SECS",
                "DEVFLOW_TEST_COPY_TIMEOUT_SECS",
                "DEVFLOW_TEST_NO_REFLINK",
                "DEVFLOW_TRUST_HOOKS",
                "DEVFLOW_INSTANCE_ID",
            ] {
                std::env::remove_var(var);
            }
        }
        let config = format!(
            "[project]\nname = \"svc-test\"\nid = \"{}\"\n\n[worktree]\ndir = \"../repo.worktrees\"\n\n{services}",
            Uuid::new_v4()
        );
        std::fs::write(root.join("devflow.toml"), config).unwrap();
        let git = git2::Repository::init(&root).unwrap();
        std::fs::write(root.join("README.md"), "hello\n").unwrap();
        let mut index = git.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        index.add_path(Path::new("devflow.toml")).unwrap();
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        git.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap();
        let project = Project::open(&root).unwrap();
        Fixture {
            _dir: dir,
            bin,
            label,
            project,
        }
    }

    fn valkey() -> Fixture {
        Fixture::new("[services.cache]\nengine = \"valkey\"\n")
    }

    fn service(&self) -> Service {
        Service::open(&self.project, "cache").unwrap()
    }

    /// A new empty root branch.
    fn root_branch(&self, svc: &Service) -> devflow_core::services::catalog::BranchRecord {
        svc.create_branch(NewBranch {
            id: None,
            name: "main".into(),
            owner: None,
            source: SourceRef::Empty,
        })
        .unwrap()
    }

    /// Containers carrying this run's label: `(id, name)`.
    fn containers(&self) -> Vec<(String, String)> {
        labelled_containers(&self.label)
    }

    fn volumes(&self) -> Vec<String> {
        labelled_volumes(&self.label)
    }
}

fn labelled_containers(label: &str) -> Vec<(String, String)> {
    let filter = format!("label={}={label}", docker::LABEL_TEST);
    docker_ok(&[
        "ps",
        "--all",
        "--no-trunc",
        "--filter",
        &filter,
        "--format",
        "{{.ID}} {{.Names}}",
    ])
    .lines()
    .filter_map(|l| l.split_once(' '))
    .map(|(id, name)| (id.to_string(), name.to_string()))
    .collect()
}

fn labelled_volumes(label: &str) -> Vec<String> {
    let filter = format!("label={}={label}", docker::LABEL_TEST);
    docker_ok(&["volume", "ls", "--quiet", "--filter", &filter])
        .lines()
        .map(str::to_string)
        .collect()
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for (id, _) in labelled_containers(&self.label) {
            let _ = docker::run(&["rm", "--force", &id]);
        }
        for volume in labelled_volumes(&self.label) {
            let _ = docker::run(&["volume", "rm", "--force", &volume]);
        }
        let left = labelled_containers(&self.label).len() + labelled_volumes(&self.label).len();
        if left > 0 && !std::thread::panicking() {
            panic!("{left} labelled Docker resources could not be removed");
        }
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn expect_cow() -> bool {
    std::env::var("DEVFLOW_EXPECT_COW").is_ok_and(|v| v == "verified")
}

fn connect(svc: &Service, id: Uuid) -> (Redis, String, u16) {
    let record = svc.branch(id).unwrap().unwrap();
    let port = record.port.expect("port");
    let conn = Redis::auth(port, &record.password).expect("the branch's own password works");
    (conn, record.password, port)
}

fn container_exists(reference: &str) -> bool {
    docker::inspect_container(reference).unwrap().is_some()
}

fn volume_exists(name: &str) -> bool {
    docker::inspect_volume(name).unwrap().is_some()
}

fn assert_method(method: Option<CloneMethod>) {
    let method = method.expect("a clone method is recorded");
    if expect_cow() {
        assert_eq!(method, CloneMethod::Reflink, "DEVFLOW_EXPECT_COW=verified");
    }
}

/// A volume that only looks like devflow's: it carries `labels` (and the
/// test label, so the run cleans it up) and holds a file nobody may lose.
fn namesake_volume(fx: &Fixture, name: &str, labels: &[(&str, &str)]) {
    let mut args: Vec<String> = vec!["volume".into(), "create".into()];
    for (key, value) in labels {
        args.extend(["--label".into(), format!("{key}={value}")]);
    }
    args.extend([
        "--label".into(),
        format!("{}={}", docker::LABEL_TEST, fx.label),
        name.into(),
    ]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    docker::run(&refs).unwrap();
    docker::run(&[
        "run",
        "--rm",
        "--network",
        "none",
        "--label",
        &format!("{}={}", docker::LABEL_TEST, fx.label),
        "-v",
        &format!("{name}:/v"),
        "debian:stable-slim",
        "sh",
        "-c",
        "echo precious > /v/sentinel",
    ])
    .unwrap();
}

/// What the namesake's file says, or `None` when the volume or the file is gone.
fn sentinel(fx: &Fixture, name: &str) -> Option<String> {
    if !volume_exists(name) {
        return None;
    }
    docker::run(&[
        "run",
        "--rm",
        "--network",
        "none",
        "--label",
        &format!("{}={}", docker::LABEL_TEST, fx.label),
        "-v",
        &format!("{name}:/v:ro"),
        "debian:stable-slim",
        "cat",
        "/v/sentinel",
    ])
    .ok()
}

// ---- tests -----------------------------------------------------------------------

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_root_branch_is_empty_authenticated_pinned_and_on_a_loopback_port() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    assert_eq!(root.state, RecordState::Ready);
    assert_eq!(root.source, SourceRef::Empty);

    // Reachable with its own password, and only with it.
    let (mut conn, password, port) = connect(&svc, root.id);
    assert!(
        (20_000..40_000).contains(&port),
        "allocated from the hash range: {port}"
    );
    assert!(svc.candidate_ports(root.id).contains(&port));
    assert_eq!(conn.dbsize(), 0, "an empty root");
    let mut anonymous = Redis::connect(port);
    assert!(
        anonymous.cmd(&["GET", "k"]).unwrap_err().contains("NOAUTH"),
        "no unauthenticated access"
    );
    assert!(Redis::auth(port, "not-the-password").is_err());

    // The profile: AOF off, RDB on, data in /data/dump.rdb.
    assert_eq!(
        conn.cmd(&["CONFIG", "GET", "appendonly"]).unwrap().text(),
        "appendonly no"
    );
    assert!(
        conn.cmd(&["CONFIG", "GET", "save"])
            .unwrap()
            .text()
            .contains("3600 1 300 100 60 10000")
    );
    assert_eq!(
        conn.cmd(&["CONFIG", "GET", "dir"]).unwrap().text(),
        "dir /data"
    );

    // The image is pinned by digest and recorded; ownership is the catalog's,
    // the labels only corroborate.
    assert!(
        root.pin.contains("@sha256:") || root.pin.starts_with("sha256:"),
        "{}",
        root.pin
    );
    let container = docker::inspect_container(root.container_id.as_deref().unwrap())
        .unwrap()
        .unwrap();
    assert!(container.running);
    assert_eq!(container.published, Some(port));
    assert_eq!(container.labels[docker::LABEL_BRANCH], root.id.to_string());
    assert_eq!(
        container.labels[docker::LABEL_PROJECT],
        fx.project.id().to_string()
    );
    assert_eq!(container.labels[docker::LABEL_TEST], fx.label);
    // Published on loopback only.
    let bindings = docker_ok(&["port", &container.id, "6379/tcp"]);
    assert!(bindings.starts_with("127.0.0.1:"), "{bindings}");

    // The catalog holds the password and is private.
    let catalog_path = Store::path_for(fx.project.id()).unwrap();
    let text = std::fs::read_to_string(&catalog_path).unwrap();
    assert!(text.contains(&password));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&catalog_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // The branch survives a stop and a start, with the same port and data.
    conn.cmd(&["SET", "k", "v"]).unwrap();
    assert!(svc.down(root.id).unwrap());
    assert!(!svc.down(root.id).unwrap(), "already stopped");
    let endpoint = svc.up(root.id).unwrap();
    assert_eq!(endpoint.port, port, "the port is kept");
    let (mut again, _, _) = connect(&svc, root.id);
    assert_eq!(
        again.get("k").as_deref(),
        Some("v"),
        "a graceful stop saves"
    );
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_clone_under_concurrent_writes_has_the_data_and_never_blocks_the_source() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let (mut conn, root_password, root_port) = connect(&svc, root.id);
    conn.fill("seed", 4000, 256);

    // A writer keeps writing to the source while it is cloned, and records the
    // slowest answer it got: a `docker pause` would show up as a stall.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let stop = stop.clone();
        let password = root_password.clone();
        std::thread::spawn(move || {
            let mut conn = Redis::auth(root_port, &password).unwrap();
            let (mut written, mut slowest) = (0u32, Duration::ZERO);
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let started = Instant::now();
                conn.cmd(&["SET", &format!("live:{written}"), "x"]).unwrap();
                slowest = slowest.max(started.elapsed());
                written += 1;
                std::thread::sleep(Duration::from_millis(2));
            }
            (written, slowest)
        })
    };
    let before = now_millis();
    let started = Instant::now();
    let clone = svc
        .create_branch(NewBranch {
            id: None,
            name: "feature".into(),
            owner: None,
            source: SourceRef::Branch { id: root.id },
        })
        .unwrap();
    let took = started.elapsed();
    let after = now_millis();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (written, slowest) = writer.join().unwrap();

    // The clone holds the data captured before the save; the writes after it
    // are the source's alone.
    let (mut copy, clone_password, clone_port) = connect(&svc, clone.id);
    assert_ne!(clone_port, root_port);
    assert_eq!(copy.get("seed:0").map(|v| v.len()), Some(256));
    assert_eq!(copy.get("seed:3999").map(|v| v.len()), Some(256));
    let in_clone = copy.dbsize() - 4000;
    assert!(
        (0..=i64::from(written)).contains(&in_clone),
        "{in_clone} live keys of {written}"
    );
    assert!(
        conn.dbsize() >= 4000 + in_clone,
        "the source kept everything"
    );
    assert!(
        slowest < Duration::from_secs(2),
        "the source answered within {slowest:?} during the clone (never paused)"
    );
    let source = docker::inspect_container(root.container_id.as_deref().unwrap())
        .unwrap()
        .unwrap();
    assert!(source.running && source.status != "paused");

    // Capture is the BGSAVE fork, and the method is reported, never assumed.
    let captured = clone.captured_at.expect("capture time").timestamp_millis();
    assert!(
        captured >= before && captured <= after,
        "{captured} in [{before}, {after}]"
    );
    assert!(clone.capture_exact);
    assert_method(clone.clone);
    assert_eq!(clone.source, SourceRef::Branch { id: root.id });
    assert_eq!(
        clone.pin, root.pin,
        "the clone boots the source's pinned image"
    );

    // Credentials: each branch has its own; neither opens the other.
    assert_ne!(clone_password, root_password);
    assert!(
        Redis::auth(clone_port, &root_password)
            .unwrap_err()
            .contains("WRONGPASS")
    );
    assert!(
        Redis::auth(root_port, &clone_password)
            .unwrap_err()
            .contains("WRONGPASS")
    );
    // The clone diverges: writing to it does not touch the source.
    copy.cmd(&["SET", "only-in-clone", "1"]).unwrap();
    assert_eq!(conn.get("only-in-clone"), None);
    eprintln!(
        "clone of {} keys took {took:?}, method {:?}, allocated {:?} bytes",
        4000 + written,
        clone.clone,
        clone.allocated_bytes
    );
}

/// One field of `INFO persistence`.
fn persistence_field(conn: &mut Redis, name: &str) -> String {
    let info = conn.cmd(&["INFO", "persistence"]).unwrap().text();
    info.lines()
        .find_map(|line| line.strip_prefix(&format!("{name}:")))
        .unwrap_or_else(|| panic!("INFO persistence has no {name}: {info}"))
        .trim()
        .to_string()
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_source_with_nothing_new_to_save_is_cloned_from_its_last_save_without_a_new_one() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let (mut conn, _, _) = connect(&svc, root.id);
    let new_branch = |name: &str| NewBranch {
        id: None,
        name: name.into(),
        owner: None,
        source: SourceRef::Branch { id: root.id },
    };

    // A server that has never saved has no dump.rdb to reuse, so it saves.
    let first = svc.create_branch(new_branch("first")).unwrap();
    assert!(!first.capture_last_save, "no dump existed: a save was made");
    let (mut copy, _, _) = connect(&svc, first.id);
    assert_eq!(copy.dbsize(), 0);

    // Seed, save, and let the server settle: nothing is left to save.
    conn.fill("seed", 2000, 128);
    conn.cmd(&["SAVE"]).unwrap();
    assert_eq!(
        persistence_field(&mut conn, "rdb_changes_since_last_save"),
        "0"
    );
    let saves = persistence_field(&mut conn, "rdb_saves");
    let saved_at: i64 = persistence_field(&mut conn, "rdb_last_save_time")
        .parse()
        .unwrap();

    let second = svc.create_branch(new_branch("second")).unwrap();
    let third = svc.create_branch(new_branch("third")).unwrap();
    assert_eq!(
        persistence_field(&mut conn, "rdb_saves"),
        saves,
        "two clones of an unchanged source asked for no save"
    );
    for clone in [&second, &third] {
        assert!(clone.capture_last_save, "recorded as the last save");
        assert_eq!(
            clone.captured_at.map(|t| t.timestamp()),
            Some(saved_at),
            "captured at the server's rdb_last_save_time"
        );
        assert!(clone.capture_exact);
        assert_method(clone.clone);
        let (mut copy, _, _) = connect(&svc, clone.id);
        assert_eq!(copy.dbsize(), 2000);
        assert_eq!(copy.get("seed:0").map(|v| v.len()), Some(128));
        assert_eq!(copy.get("seed:1999").map(|v| v.len()), Some(128));
    }
    let view = svc
        .branch_views()
        .unwrap()
        .into_iter()
        .find(|b| b.id == second.id)
        .unwrap();
    assert!(view.capture_last_save);
    assert!(
        serde_json::to_value(&view).unwrap()["capture_last_save"] == true,
        "the JSON output says so"
    );

    // A write since the last save puts the ordinary BGSAVE path back.
    conn.cmd(&["SET", "late", "1"]).unwrap();
    let fourth = svc.create_branch(new_branch("fourth")).unwrap();
    assert!(!fourth.capture_last_save);
    let (mut copy, _, _) = connect(&svc, fourth.id);
    assert_eq!(copy.get("late").as_deref(), Some("1"));
    assert_ne!(
        persistence_field(&mut conn, "rdb_saves"),
        saves,
        "a changed source is saved"
    );
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_snapshot_is_a_point_in_time_a_branch_can_start_from() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let (mut conn, _, _) = connect(&svc, root.id);
    conn.cmd(&["SET", "stage", "one"]).unwrap();

    let v1 = svc.snapshot_branch(root.id, "seed").unwrap();
    assert_eq!((v1.name.as_str(), v1.version), ("seed", 1));
    assert_eq!(v1.state, RecordState::Ready);
    assert_method(v1.clone);
    assert!(v1.captured_at.is_some());
    assert!(volume_exists(&v1.volume));
    // A snapshot has no container.
    assert!(
        !fx.containers()
            .iter()
            .any(|(_, name)| name.contains(&v1.id.simple().to_string()[..12])),
        "no container for a snapshot"
    );

    conn.cmd(&["SET", "stage", "two"]).unwrap();
    let v2 = svc.snapshot_branch(root.id, "seed").unwrap();
    assert_eq!(v2.version, 2);
    assert_eq!(svc.find_snapshot("seed", None).unwrap().id, v2.id);
    assert_eq!(svc.find_snapshot("seed", Some(1)).unwrap().id, v1.id);
    assert!(svc.find_snapshot("seed", Some(3)).is_err());

    // Branches from each version see that version's data, and the root has
    // moved on since.
    conn.cmd(&["SET", "stage", "three"]).unwrap();
    let from_v1 = svc
        .create_branch(NewBranch {
            id: None,
            name: "a".into(),
            owner: None,
            source: SourceRef::Snapshot { id: v1.id },
        })
        .unwrap();
    let from_v2 = svc
        .create_branch(NewBranch {
            id: None,
            name: "b".into(),
            owner: None,
            source: SourceRef::Snapshot { id: v2.id },
        })
        .unwrap();
    assert_eq!(
        connect(&svc, from_v1.id).0.get("stage").as_deref(),
        Some("one")
    );
    assert_eq!(
        connect(&svc, from_v2.id).0.get("stage").as_deref(),
        Some("two")
    );
    assert_eq!(conn.get("stage").as_deref(), Some("three"));
    // A branch from a snapshot inherits the snapshot's capture time.
    assert_eq!(from_v1.captured_at, v1.captured_at);

    // Deleting a version removes its volume by recorded identity.
    svc.remove_snapshot("seed", 1).unwrap();
    assert!(!volume_exists(&v1.volume));
    assert!(svc.find_snapshot("seed", Some(1)).is_err());
    assert!(volume_exists(&v2.volume), "other versions are untouched");
    assert_eq!(
        svc.snapshot_branch(root.id, "seed").unwrap().version,
        3,
        "versions are never reused"
    );
    // Not even the newest one's: deleting v3 does not make it the next again.
    svc.remove_snapshot("seed", 3).unwrap();
    assert_eq!(svc.snapshot_branch(root.id, "seed").unwrap().version, 4);
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn concurrent_snapshots_of_one_name_get_distinct_permanent_versions() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id).0.cmd(&["SET", "k", "v"]).unwrap();
    let workers: Vec<_> = (0..3)
        .map(|_| {
            let svc = svc.clone();
            let root = root.id;
            std::thread::spawn(move || svc.snapshot_branch(root, "seed"))
        })
        .collect();
    let mut versions: Vec<u32> = workers
        .into_iter()
        .map(|w| w.join().unwrap().unwrap().version)
        .collect();
    versions.sort_unstable();
    assert_eq!(
        versions,
        vec![1, 2, 3],
        "one number each, taken under the lock"
    );
    let ready: Vec<_> = svc
        .snapshot_views()
        .unwrap()
        .into_iter()
        .filter(|s| s.state == RecordState::Ready)
        .collect();
    assert_eq!(ready.len(), 3);

    // Delete the newest, then the others: the next number is still 4.
    for version in [3, 2, 1] {
        svc.remove_snapshot("seed", version).unwrap();
    }
    assert!(svc.snapshot_views().unwrap().is_empty());
    assert_eq!(svc.snapshot_branch(root.id, "seed").unwrap().version, 4);
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_staged_reset_replaces_the_data_keeps_the_endpoint_and_removes_the_original() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let (mut conn, _, _) = connect(&svc, root.id);
    conn.cmd(&["SET", "origin", "root"]).unwrap();
    let branch = svc
        .create_branch(NewBranch {
            id: None,
            name: "feature".into(),
            owner: None,
            source: SourceRef::Branch { id: root.id },
        })
        .unwrap();
    let (mut work, password, port) = connect(&svc, branch.id);
    work.cmd(&["SET", "scratch", "mine"]).unwrap();
    conn.cmd(&["SET", "newer", "root-only"]).unwrap();
    let (old_container, old_volume) = (branch.container_id.clone().unwrap(), branch.volume.clone());

    // Reset to the recorded source: the branch gets the root's current data.
    let reset = svc.reset_branch(branch.id, branch.source).unwrap();
    assert_eq!(reset.generation, 2);
    assert_eq!(reset.port, Some(port), "the port is kept");
    assert_eq!(reset.password, password, "the credentials are kept");
    assert_ne!(reset.volume, old_volume);
    let (mut after, _, _) = connect(&svc, branch.id);
    assert_eq!(
        after.get("scratch"),
        None,
        "the branch's own writes are gone"
    );
    assert_eq!(after.get("origin").as_deref(), Some("root"));
    assert_eq!(after.get("newer").as_deref(), Some("root-only"));
    assert_method(reset.clone);
    // The original pair is gone, by identity; the staging container too.
    assert!(!container_exists(&old_container));
    assert!(!volume_exists(&old_volume));
    assert!(
        !fx.containers()
            .iter()
            .any(|(_, name)| name.ends_with("-stage")),
        "{:?}",
        fx.containers()
    );
    // The journal records a finished reset.
    let records = svc.records().unwrap();
    let op = records
        .journal
        .iter()
        .find(|o| o.kind == OpKind::Reset)
        .unwrap();
    assert_eq!(op.step, Step::Done);

    // A reset to empty starts from nothing, on the same endpoint.
    let emptied = svc.reset_branch(branch.id, SourceRef::Empty).unwrap();
    assert_eq!(emptied.generation, 3);
    assert_eq!(emptied.port, Some(port));
    assert_eq!(connect(&svc, branch.id).0.dbsize(), 0);
    // A branch cannot be reset from itself.
    assert!(
        svc.reset_branch(branch.id, SourceRef::Branch { id: branch.id })
            .is_err()
    );
    // The source was never touched by any of this.
    assert_eq!(conn.get("origin").as_deref(), Some("root"));
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_reset_stranded_at_cleanup_still_owns_the_pair_it_replaced() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let project = fx.project.id().to_string();
    let branch = root.id.to_string();

    // The catalog moved to the new pair at the cutover; the old pair is only
    // in the reset's plan, which is waiting for its cleanup (planned a moment
    // ago, so recovery leaves the operation to its coordinator).
    let old_volume = format!("devflow-test-old-{}", &branch[..8]);
    namesake_volume(
        &fx,
        &old_volume,
        &[
            (docker::LABEL_PROJECT, &project),
            (docker::LABEL_SERVICE, "cache"),
            (docker::LABEL_BRANCH, &branch),
        ],
    );
    let old_container = docker::run(&[
        "create",
        "--network",
        "none",
        "--label",
        &format!("{}={project}", docker::LABEL_PROJECT),
        "--label",
        "dev.devflow.service=cache",
        "--label",
        &format!("{}={branch}", docker::LABEL_BRANCH),
        "--label",
        &format!("{}={}", docker::LABEL_TEST, fx.label),
        "--name",
        &format!("devflow-test-old-{}", &branch[..8]),
        "debian:stable-slim",
        "true",
    ])
    .unwrap();
    let mut op = Op::new(OpKind::Reset, root.id);
    op.source = Some(SourceRef::Empty);
    op.step = Step::Cleanup;
    op.reset = Some(ResetPlan {
        volume: root.volume.clone(),
        container_name: root.container_name.clone(),
        staging_name: format!("{}-stage", root.container_name),
        image: root.image.clone(),
        pin: root.pin.clone(),
        staging_id: None,
        container_id: root.container_id.clone(),
        old_container_id: Some(old_container.clone()),
        old_volume: Some(old_volume.clone()),
        old_was_running: false,
        captured_at: None,
        capture_exact: true,
        capture_last_save: false,
        clone: None,
        allocated_bytes: None,
        downtime_ms: None,
    });
    Store::open(fx.project.id(), svc.docker_id())
        .unwrap()
        .update(|catalog| {
            catalog
                .service_mut("cache", devflow_core::config::Engine::Valkey)?
                .journal
                .push(op);
            Ok(())
        })
        .unwrap();

    let report = svc.reconcile().unwrap();
    assert_eq!(report.in_progress.len(), 1, "{report:?}");
    assert!(
        report.orphans.is_empty(),
        "the old pair is recorded by the unfinished reset: {report:?}"
    );
    assert!(container_exists(&old_container) && volume_exists(&old_volume));
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_source_volume_that_is_missing_or_not_ours_never_becomes_an_empty_dataset() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let (mut conn, _, _) = connect(&svc, root.id);
    conn.cmd(&["SET", "precious", "1"]).unwrap();
    let snapshot = svc.snapshot_branch(root.id, "seed").unwrap();
    let branch = svc
        .create_branch(NewBranch {
            id: None,
            name: "feature".into(),
            owner: None,
            source: SourceRef::Snapshot { id: snapshot.id },
        })
        .unwrap();
    connect(&svc, branch.id)
        .0
        .cmd(&["SET", "mine", "1"])
        .unwrap();

    // Someone replaces the snapshot's volume with an empty namesake that only
    // carries the test label.
    docker::run(&["volume", "rm", &snapshot.volume]).unwrap();
    docker_ok(&[
        "volume",
        "create",
        "--label",
        &format!("{}={}", docker::LABEL_TEST, fx.label),
        &snapshot.volume,
    ]);
    let error = svc
        .reset_branch(branch.id, SourceRef::Snapshot { id: snapshot.id })
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("label does not match"),
        "{error:#}"
    );
    // The branch's own volume, container and data are exactly as they were.
    let after = svc.branch(branch.id).unwrap().unwrap();
    assert_eq!((after.generation, &after.volume), (1, &branch.volume));
    assert!(volume_exists(&branch.volume));
    assert_eq!(
        connect(&svc, branch.id).0.get("mine").as_deref(),
        Some("1"),
        "the branch kept its data"
    );
    // A clone from it is refused too, and leaves no resource behind.
    let volumes = fx.volumes().len();
    assert!(
        svc.create_branch(NewBranch {
            id: None,
            name: "other".into(),
            owner: None,
            source: SourceRef::Snapshot { id: snapshot.id },
        })
        .is_err()
    );
    assert_eq!(fx.volumes().len(), volumes, "{:?}", fx.volumes());

    // An absent source volume is not created by being mounted, and is not
    // empty data either.
    docker::run(&["volume", "rm", &snapshot.volume]).unwrap();
    let error = svc
        .reset_branch(branch.id, SourceRef::Snapshot { id: snapshot.id })
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("no longer exists"),
        "{error:#}"
    );
    assert!(
        !volume_exists(&snapshot.volume),
        "the missing volume was not recreated"
    );
    assert!(volume_exists(&branch.volume));

    // A branch volume with a dump that was removed is missing data as well:
    // the source is refused, not cloned as an empty dataset.
    docker::run(&["stop", root.container_id.as_deref().unwrap()]).unwrap();
    docker_ok(&[
        "run",
        "--rm",
        "--network",
        "none",
        "-v",
        &format!("{}:/v", root.volume),
        "--label",
        &format!("{}={}", docker::LABEL_TEST, fx.label),
        "debian:stable-slim",
        "rm",
        "-f",
        "/v/dump.rdb",
    ]);
    let error = svc
        .create_branch(NewBranch {
            id: None,
            name: "from-nothing".into(),
            owner: None,
            source: SourceRef::Branch { id: root.id },
        })
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("holds no dump.rdb"),
        "{error:#}"
    );
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn drop_removes_the_recorded_container_and_volume_and_never_a_namesake() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let branch = svc
        .create_branch(NewBranch {
            id: None,
            name: "feature".into(),
            owner: None,
            source: SourceRef::Branch { id: root.id },
        })
        .unwrap();
    let container = branch.container_id.clone().unwrap();
    assert!(container_exists(&container) && volume_exists(&branch.volume));
    svc.remove_branch(branch.id).unwrap();
    assert!(!container_exists(&container));
    assert!(!volume_exists(&branch.volume));
    assert!(svc.branch(branch.id).unwrap().is_none());
    // The root is untouched.
    assert!(container_exists(root.container_id.as_deref().unwrap()));
    assert!(
        svc.remove_branch(branch.id).is_err(),
        "unknown after the drop"
    );

    // A container that merely has the name devflow chose is not devflow's:
    // the catalog's id is gone, the labels do not agree, and nothing is
    // removed or adopted.
    let second = svc
        .create_branch(NewBranch {
            id: None,
            name: "second".into(),
            owner: None,
            source: SourceRef::Branch { id: root.id },
        })
        .unwrap();
    let namesake_name = second.container_name.clone();
    docker::run(&["rm", "--force", second.container_id.as_deref().unwrap()]).unwrap();
    let decoy = docker_ok(&[
        "create",
        "--name",
        &namesake_name,
        "--label",
        &format!("{}={}", docker::LABEL_TEST, fx.label),
        "debian:stable-slim",
        "true",
    ]);
    assert!(!decoy.is_empty());
    let error = svc.up(second.id).unwrap_err();
    assert!(
        format!("{error:#}").contains("labels say otherwise"),
        "{error:#}"
    );
    // Dropping the branch removes what the catalog recorded (the volume; the
    // recorded container is already gone) and leaves the namesake alone.
    svc.remove_branch(second.id).unwrap();
    assert!(container_exists(&decoy), "the namesake was not touched");
    assert!(!volume_exists(&second.volume));
    assert!(svc.branch(second.id).unwrap().is_none());
    docker::run(&["rm", "--force", &decoy]).unwrap();
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_branch_is_found_cloned_from_and_dropped_by_the_short_id_a_listing_shows() {
    use devflow_core::ids::{IdRef, short};
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id).0.cmd(&["SET", "k", "v"]).unwrap();
    let typed: IdRef = short(root.id).to_uppercase().parse().unwrap();
    assert_eq!(svc.resolve_branch(&typed).unwrap(), root.id);

    // `--data-from branch:SHORT` resolves against the catalog, and the label
    // pinned records the whole id.
    let source: DataSource = format!("branch:{}", short(root.id)).parse().unwrap();
    let (resolved, label) =
        devflow_core::services::workspace::resolve_source(&fx.project, &svc, &source).unwrap();
    assert_eq!(resolved, SourceRef::Branch { id: root.id });
    assert_eq!(label, format!("branch:{}", root.id));
    let clone = svc
        .create_branch(NewBranch {
            id: None,
            name: "from-short-id".into(),
            owner: None,
            source: resolved,
        })
        .unwrap();
    assert_eq!(connect(&svc, clone.id).0.get("k").as_deref(), Some("v"));

    // An id nothing starts with is an error, not another branch.
    let error = format!(
        "{:#}",
        svc.resolve_branch(&"deadbeef".parse().unwrap())
            .unwrap_err()
    );
    assert!(
        error.contains("no branch has an id starting with"),
        "{error}"
    );

    // The drop resolves the same way and removes what the catalog recorded.
    let id = svc
        .resolve_branch(&short(clone.id).parse().unwrap())
        .unwrap();
    svc.remove_branch(id).unwrap();
    assert!(svc.branch(clone.id).unwrap().is_none());
    assert!(!container_exists(clone.container_id.as_deref().unwrap()));
    assert!(container_exists(root.container_id.as_deref().unwrap()));
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_missing_volume_is_lost_data_never_an_empty_server_in_its_place() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id)
        .0
        .cmd(&["SET", "precious", "1"])
        .unwrap();
    // Someone removes the container and the volume by hand.
    docker::run(&["rm", "--force", root.container_id.as_deref().unwrap()]).unwrap();
    docker::run(&["volume", "rm", &root.volume]).unwrap();
    let error = format!("{:#}", svc.up(root.id).unwrap_err());
    assert!(
        error.contains("no longer exists") && error.contains("its data is gone"),
        "{error}"
    );
    assert!(fx.containers().is_empty(), "no empty server was started");
    // The drop still works, by recorded identity.
    svc.remove_branch(root.id).unwrap();
    assert!(svc.branch(root.id).unwrap().is_none());
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn killing_the_coordinator_mid_clone_is_recovered_without_touching_the_source() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let (mut conn, root_password, root_port) = connect(&svc, root.id);
    conn.fill("seed", 500, 64);
    let containers_before = fx.containers().len();

    // A copy that takes a while, so the helper outlives its coordinator.
    unsafe { std::env::set_var("DEVFLOW_TEST_COPY_DELAY_SECS", "60") };
    let id = Uuid::new_v4();
    let worker = {
        let svc = svc.clone();
        std::thread::spawn(move || {
            svc.create_branch(NewBranch {
                id: Some(id),
                name: "feature".into(),
                owner: None,
                source: SourceRef::Branch { id: root.id },
            })
        })
    };
    // Wait for the helper to be journaled, then kill the coordinator.
    let started = Instant::now();
    let (op_id, pid, helper) = loop {
        let records = svc.records().unwrap();
        if let Some(op) = records.journal.iter().find(|o| o.target == id)
            && op.step == Step::Copying
            && let (Some(helper), Some(coordinator)) = (&op.helper_id, &op.coordinator)
        {
            break (op.id, coordinator.pid, helper.clone());
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "no helper was journaled"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(container_exists(&helper), "the helper is running in the VM");
    let status = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let outcome = worker.join().unwrap();
    assert!(
        outcome.is_err(),
        "the caller learns the clone did not finish"
    );
    unsafe { std::env::remove_var("DEVFLOW_TEST_COPY_DELAY_SECS") };
    // The coordinator died; its helper did not.
    assert!(
        container_exists(&helper),
        "the helper outlives its coordinator"
    );
    let dest = svc.records().unwrap().branch(id).cloned().unwrap();
    assert_eq!(dest.state, RecordState::Creating, "never promoted");

    // The next operation (here an explicit reconcile) kills the helper,
    // confirms it is gone and aborts the destination.
    let report = svc.reconcile().unwrap();
    assert!(
        report.actions.iter().any(|a| a.contains("rolled back")),
        "{report:?}"
    );
    assert!(report.problems.is_empty(), "{report:?}");
    assert!(
        !container_exists(&helper),
        "the helper was killed and confirmed gone"
    );
    let records = svc.records().unwrap();
    assert_eq!(records.op(op_id).unwrap().step, Step::Aborted);
    assert!(
        records
            .branch(id)
            .is_none_or(|b| b.state == RecordState::Aborted)
    );
    assert!(
        !volume_exists(&dest.volume),
        "the half-copied volume is gone"
    );
    assert!(
        !fx.containers()
            .iter()
            .any(|(_, name)| name.contains(&id.simple().to_string()[..12])),
        "no container for the aborted destination"
    );

    // The source is exactly as it was: same container, same data, same login.
    assert_eq!(fx.containers().len(), containers_before);
    let (mut after, _, _) = connect(&svc, root.id);
    assert_eq!(after.dbsize(), 500);
    assert_eq!(
        svc.branch(root.id).unwrap().unwrap().password,
        root_password
    );
    assert_eq!(svc.branch(root.id).unwrap().unwrap().port, Some(root_port));

    // And a retry just works.
    let again = svc
        .create_branch(NewBranch {
            id: None,
            name: "feature".into(),
            owner: None,
            source: SourceRef::Branch { id: root.id },
        })
        .unwrap();
    assert_eq!(connect(&svc, again.id).0.dbsize(), 500);
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_clone_that_cannot_start_is_rolled_back_whole_and_never_returns_as_an_empty_branch() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let (mut conn, _, _) = connect(&svc, root.id);
    conn.fill("seed", 300, 64);

    // Every port candidate of the destination is taken: the copy succeeds,
    // the start fails.
    let id = Uuid::new_v4();
    let held: Vec<TcpListener> = svc
        .candidate_ports(id)
        .into_iter()
        .filter_map(|port| TcpListener::bind(("127.0.0.1", port)).ok())
        .collect();
    assert!(!held.is_empty());
    let error = svc
        .create_branch(NewBranch {
            id: Some(id),
            name: "blocked".into(),
            owner: None,
            source: SourceRef::Branch { id: root.id },
        })
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("no host port was available"),
        "{error:#}"
    );
    // The rollback was journaled, finished and recorded as aborted, instead
    // of failing on an illegal move and leaving the clone half done.
    let records = svc.records().unwrap();
    let op = records.journal.iter().find(|o| o.target == id).unwrap();
    assert_eq!(op.step, Step::Aborted, "{op:?}");
    assert_eq!(op.rollback_from, Some(Step::Starting));
    assert!(records.unfinished().is_empty());
    let dest = records.branch(id).unwrap();
    assert_eq!(dest.state, RecordState::Aborted);
    assert!(!volume_exists(&dest.volume), "the copied volume is gone");
    drop(held);

    // With the ports free again, reconcile has nothing to complete: no
    // branch appears, ready or otherwise, and no volume is made.
    let report = svc.reconcile().unwrap();
    assert!(report.actions.is_empty(), "{report:?}");
    assert!(
        svc.branch(id)
            .unwrap()
            .is_none_or(|b| b.state == RecordState::Aborted)
    );
    assert_eq!(fx.volumes(), vec![root.volume.clone()]);
    // A retry under a new identity works and has the data.
    let again = svc
        .create_branch(NewBranch {
            id: None,
            name: "retry".into(),
            owner: None,
            source: SourceRef::Branch { id: root.id },
        })
        .unwrap();
    assert_eq!(connect(&svc, again.id).0.dbsize(), 300);
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_copy_whose_volume_is_gone_is_rolled_back_by_recovery_never_completed_empty() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id).0.fill("seed", 100, 64);

    // The state a crash leaves after the copy was journaled, with the
    // destination's volume gone meanwhile (a coordinator that died during an
    // earlier, half-finished rollback).
    let id = Uuid::new_v4();
    let mut record = root.clone();
    record.id = id;
    record.name = "crashed".into();
    record.state = RecordState::Creating;
    record.container_id = None;
    record.container_name = format!("devflow-test-crashed-{}", id.simple());
    record.volume = svc.volume_name(id, 1);
    record.port = None;
    record.pending_port = None;
    record.source = SourceRef::Branch { id: root.id };
    let mut op = Op::new(OpKind::Clone, id);
    op.source = Some(SourceRef::Branch { id: root.id });
    op.step = Step::Copied;
    op.copied_size = Some(1234);
    op.coordinator = Some(Coordinator {
        pid: 999_999,
        started_at: root.created_at,
    });
    let op_id = op.id;
    let volume = record.volume.clone();
    Store::open(fx.project.id(), svc.docker_id())
        .unwrap()
        .update(|catalog| {
            let service = catalog.service_mut("cache", devflow_core::config::Engine::Valkey)?;
            service.branches.push(record);
            service.journal.push(op);
            Ok(())
        })
        .unwrap();

    let report = svc.reconcile().unwrap();
    assert!(
        report
            .actions
            .iter()
            .any(|a| a.contains("rolled back") && a.contains("cannot be completed")),
        "{report:?}"
    );
    assert!(report.problems.is_empty(), "{report:?}");
    let records = svc.records().unwrap();
    assert_eq!(records.op(op_id).unwrap().step, Step::Aborted);
    assert_eq!(records.branch(id).unwrap().state, RecordState::Aborted);
    assert!(!volume_exists(&volume), "no empty volume was made");
    assert_eq!(fx.containers().len(), 1, "no server was started");
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_rollback_keeps_a_volume_of_another_owner_and_reports_it_until_it_is_settled() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id).0.fill("seed", 100, 64);

    // A clone journaled at `copied` whose destination volume was replaced
    // meanwhile by a namesake: this project's label, another branch's
    // identity, and a file inside that nobody may lose.
    let id = Uuid::new_v4();
    let mut record = root.clone();
    record.id = id;
    record.name = "crashed".into();
    record.state = RecordState::Creating;
    record.container_id = None;
    record.container_name = format!("devflow-test-crashed-{}", id.simple());
    record.volume = svc.volume_name(id, 1);
    record.port = None;
    record.pending_port = None;
    record.source = SourceRef::Branch { id: root.id };
    let mut op = Op::new(OpKind::Clone, id);
    op.source = Some(SourceRef::Branch { id: root.id });
    op.step = Step::Copied;
    op.copied_size = Some(1234);
    op.coordinator = Some(Coordinator {
        pid: 999_999,
        started_at: root.created_at,
    });
    let op_id = op.id;
    let volume = record.volume.clone();
    Store::open(fx.project.id(), svc.docker_id())
        .unwrap()
        .update(|catalog| {
            let service = catalog.service_mut("cache", devflow_core::config::Engine::Valkey)?;
            service.branches.push(record);
            service.journal.push(op);
            Ok(())
        })
        .unwrap();
    let project = fx.project.id().to_string();
    let stranger = Uuid::new_v4().to_string();
    namesake_volume(
        &fx,
        &volume,
        &[
            (docker::LABEL_PROJECT, &project),
            (docker::LABEL_SERVICE, "cache"),
            (docker::LABEL_BRANCH, &stranger),
        ],
    );

    // The completion finds the mismatch and rolls back; the rollback does not
    // delete the namesake, says so, and is not counted as done.
    let report = svc.reconcile().unwrap();
    assert!(
        report.actions.is_empty(),
        "nothing was finished: {report:?}"
    );
    assert!(
        report
            .problems
            .iter()
            .any(|p| p.contains(&volume) && p.contains("label does not match")),
        "{report:?}"
    );
    assert_eq!(
        sentinel(&fx, &volume).as_deref(),
        Some("precious"),
        "the replacement and its file are untouched"
    );
    let records = svc.records().unwrap();
    assert_eq!(records.op(op_id).unwrap().step, Step::RollingBack);
    assert_eq!(records.branch(id).unwrap().state, RecordState::Creating);
    // It stays reported, run after run, and still nothing is deleted.
    let again = svc.reconcile().unwrap();
    assert!(
        again.problems.iter().any(|p| p.contains(&volume)),
        "{again:?}"
    );
    assert_eq!(sentinel(&fx, &volume).as_deref(), Some("precious"));

    // An operator who knows it is not devflow's removes it, and the rollback
    // runs to its end.
    docker::run(&["volume", "rm", &volume]).unwrap();
    let report = svc.reconcile().unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    assert!(
        report.actions.iter().any(|a| a.contains("rolled back")),
        "{report:?}"
    );
    let records = svc.records().unwrap();
    assert_eq!(records.op(op_id).unwrap().step, Step::Aborted);
    assert_eq!(records.branch(id).unwrap().state, RecordState::Aborted);
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_removal_never_deletes_a_volume_of_another_service_or_owner() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let snapshot = svc.snapshot_branch(root.id, "seed").unwrap();
    let branch = svc
        .create_branch(NewBranch {
            id: None,
            name: "feature".into(),
            owner: None,
            source: SourceRef::Snapshot { id: snapshot.id },
        })
        .unwrap();
    let project = fx.project.id().to_string();
    let stranger = Uuid::new_v4().to_string();

    // The snapshot's volume is replaced by a namesake of another service and
    // owner (this project's label is all it shares).
    docker::run(&["volume", "rm", &snapshot.volume]).unwrap();
    namesake_volume(
        &fx,
        &snapshot.volume,
        &[
            (docker::LABEL_PROJECT, &project),
            (docker::LABEL_SERVICE, "other"),
            (docker::LABEL_SNAPSHOT, &stranger),
        ],
    );
    let error = format!("{:#}", svc.remove_snapshot("seed", 1).unwrap_err());
    assert!(
        error.contains(&snapshot.volume) && error.contains("label does not match"),
        "{error}"
    );
    assert_eq!(sentinel(&fx, &snapshot.volume).as_deref(), Some("precious"));
    let records = svc.records().unwrap();
    let kept = records.snapshot(snapshot.id).unwrap();
    assert_eq!(
        kept.state,
        RecordState::Deleting,
        "the deletion is not done"
    );

    // A branch volume of the right service but another branch is kept too.
    docker::run(&["rm", "--force", branch.container_id.as_deref().unwrap()]).unwrap();
    docker::run(&["volume", "rm", &branch.volume]).unwrap();
    namesake_volume(
        &fx,
        &branch.volume,
        &[
            (docker::LABEL_PROJECT, &project),
            (docker::LABEL_SERVICE, "cache"),
            (docker::LABEL_BRANCH, &stranger),
        ],
    );
    let error = format!("{:#}", svc.remove_branch(branch.id).unwrap_err());
    assert!(
        error.contains(&branch.volume) && error.contains("label does not match"),
        "{error}"
    );
    assert_eq!(sentinel(&fx, &branch.volume).as_deref(), Some("precious"));
    assert_eq!(
        svc.records().unwrap().branch(branch.id).unwrap().state,
        RecordState::Deleting
    );

    // Both are reported, and neither is removed by looking again.
    let report = svc.reconcile().unwrap();
    assert!(report.actions.is_empty(), "{report:?}");
    for volume in [&snapshot.volume, &branch.volume] {
        assert!(
            report.problems.iter().any(|p| p.contains(volume.as_str())),
            "{report:?}"
        );
        assert_eq!(sentinel(&fx, volume).as_deref(), Some("precious"));
    }

    // Once the operator has removed the namesakes, both removals finish.
    docker::run(&["volume", "rm", &snapshot.volume]).unwrap();
    docker::run(&["volume", "rm", &branch.volume]).unwrap();
    let report = svc.reconcile().unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    let records = svc.records().unwrap();
    assert!(records.snapshot(snapshot.id).is_none());
    assert!(records.branch(branch.id).is_none());
    assert!(records.unfinished().is_empty());
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_branch_labelled_namesake_survives_a_snapshot_delete_despite_the_legacy_exception() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let snapshot = svc.snapshot_branch(root.id, "seed").unwrap();
    let project = fx.project.id().to_string();

    // The legacy exception (a snapshot volume without its snapshot label) is
    // for a volume that has no identity label at all. This one is a branch's:
    // the right project and service, no snapshot label, and a branch label.
    docker::run(&["volume", "rm", &snapshot.volume]).unwrap();
    namesake_volume(
        &fx,
        &snapshot.volume,
        &[
            (docker::LABEL_PROJECT, &project),
            (docker::LABEL_SERVICE, "cache"),
            (docker::LABEL_BRANCH, &root.id.to_string()),
        ],
    );
    let error = format!("{:#}", svc.remove_snapshot("seed", 1).unwrap_err());
    assert!(
        error.contains(&snapshot.volume) && error.contains(docker::LABEL_BRANCH),
        "{error}"
    );
    assert_eq!(sentinel(&fx, &snapshot.volume).as_deref(), Some("precious"));
    assert_eq!(
        svc.records().unwrap().snapshot(snapshot.id).unwrap().state,
        RecordState::Deleting,
        "the deletion is not done"
    );

    // The same with a helper's label.
    docker::run(&["volume", "rm", &snapshot.volume]).unwrap();
    namesake_volume(
        &fx,
        &snapshot.volume,
        &[
            (docker::LABEL_PROJECT, &project),
            (docker::LABEL_SERVICE, "cache"),
            (docker::LABEL_HELPER, &Uuid::new_v4().to_string()),
        ],
    );
    assert!(svc.remove_snapshot("seed", 1).is_err());
    assert_eq!(sentinel(&fx, &snapshot.volume).as_deref(), Some("precious"));
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_snapshot_of_a_pre_release_build_is_refused_in_words_and_can_still_be_deleted() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id).0.fill("seed", 50, 64);
    let snapshot = svc.snapshot_branch(root.id, "seed").unwrap();

    // Make it what an earlier build left behind: the same data under this
    // project's and this service's labels, and no snapshot label.
    let project = fx.project.id().to_string();
    let test = format!("{}={}", docker::LABEL_TEST, fx.label);
    let spare = format!("{}-spare", snapshot.volume);
    let copy = |from: &str, to: &str| {
        docker::run(&[
            "run",
            "--rm",
            "--network",
            "none",
            "--label",
            &test,
            "-v",
            &format!("{from}:/src:ro"),
            "-v",
            &format!("{to}:/dst"),
            "debian:stable-slim",
            "sh",
            "-c",
            "cp -a /src/. /dst/",
        ])
        .unwrap();
    };
    docker::run(&["volume", "create", "--label", &test, &spare]).unwrap();
    copy(&snapshot.volume, &spare);
    docker::run(&["volume", "rm", &snapshot.volume]).unwrap();
    docker::run(&[
        "volume",
        "create",
        "--label",
        &format!("{}={project}", docker::LABEL_PROJECT),
        "--label",
        &format!("{}=cache", docker::LABEL_SERVICE),
        "--label",
        &test,
        &snapshot.volume,
    ])
    .unwrap();
    copy(&spare, &snapshot.volume);
    docker::run(&["volume", "rm", &spare]).unwrap();

    // Nothing is migrated: a copy from it is refused, in words that say what
    // it is and what to do, and leaves nothing behind.
    let volumes = fx.volumes().len();
    let error = svc
        .create_branch(NewBranch {
            id: None,
            name: "feature".into(),
            owner: None,
            source: SourceRef::Snapshot { id: snapshot.id },
        })
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("pre-release build")
            && text.contains("dev.devflow.snapshot")
            && text.contains("devflow service cache snapshot delete seed@v1")
            && text.contains("take a new one"),
        "{text}"
    );
    assert_eq!(fx.volumes().len(), volumes, "{:?}", fx.volumes());

    // The command it names removes it, by the catalog's record and the
    // project and service labels, without a snapshot label.
    let out = devflow(&fx, &["service", "cache", "snapshot", "delete", "seed@v1"]);
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!volume_exists(&snapshot.volume));
    assert!(svc.snapshot(snapshot.id).unwrap().is_none());
    assert!(svc.records().unwrap().unfinished().is_empty());

    // A new snapshot is labelled and works; versions are not reused.
    let fresh = svc.snapshot_branch(root.id, "seed").unwrap();
    assert_eq!(fresh.version, 2);
    let branch = svc
        .create_branch(NewBranch {
            id: None,
            name: "feature".into(),
            owner: None,
            source: SourceRef::Snapshot { id: fresh.id },
        })
        .unwrap();
    assert_eq!(connect(&svc, branch.id).0.dbsize(), 50);
}

/// The real `docker` binary, found before any shim is put on the path.
fn real_docker() -> String {
    let out = Command::new("sh")
        .args(["-c", "command -v docker"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A directory with a `docker` that is the real one, except that it kills its
/// caller (SIGKILL, as a crash would) right after the real command it is told
/// to crash after: `create` of a `*-verify-*` helper, or any `start`.
fn crash_shim(fx: &Fixture) -> std::path::PathBuf {
    let dir = fx.bin.parent().unwrap().join("shim");
    std::fs::create_dir_all(&dir).unwrap();
    let script = r#"#!/bin/sh
case "$1:$DF_SHIM_CRASH_AFTER" in
  create:verify-create)
    case "$*" in
      *dev.devflow.tombstone*) ;;
      *-verify-*) "$DF_SHIM_REAL" "$@"; code=$?; kill -9 "$PPID"; exit $code ;;
    esac ;;
  start:start)
    "$DF_SHIM_REAL" "$@"; code=$?; kill -9 "$PPID"; exit $code ;;
esac
exec "$DF_SHIM_REAL" "$@"
"#;
    let path = dir.join("docker");
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// A snapshot of `root`, made for real and then put back in the state a crash
/// leaves it in after the copy was journaled: still being created, its
/// operation unfinished at `copied`, nobody holding its locks.
fn snapshot_left_at_copied(
    fx: &Fixture,
    svc: &Service,
    root: Uuid,
    name: &str,
) -> (devflow_core::services::catalog::SnapshotRecord, Uuid) {
    let snapshot = svc.snapshot_branch(root, name).unwrap();
    let size: u64 = docker::run(&[
        "run",
        "--rm",
        "--network",
        "none",
        "--label",
        &format!("{}={}", docker::LABEL_TEST, fx.label),
        "-v",
        &format!("{}:/v:ro", snapshot.volume),
        "debian:stable-slim",
        "stat",
        "-c",
        "%s",
        "/v/dump.rdb",
    ])
    .unwrap()
    .parse()
    .unwrap();
    let mut op = Op::new(OpKind::Snapshot, snapshot.id);
    op.source = Some(SourceRef::Branch { id: root });
    op.step = Step::Copied;
    op.copied_size = Some(size);
    op.coordinator = Some(Coordinator {
        pid: 999_999,
        started_at: snapshot.created_at,
    });
    let op_id = op.id;
    Store::open(fx.project.id(), svc.docker_id())
        .unwrap()
        .update(|catalog| {
            let service = catalog.service_mut("cache", devflow_core::config::Engine::Valkey)?;
            service.snapshot_mut(snapshot.id).unwrap().state = RecordState::Creating;
            service.journal.push(op);
            Ok(())
        })
        .unwrap();
    (snapshot, op_id)
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_verify_helper_left_by_a_killed_recovery_is_found_and_removed_by_the_next_one() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id).0.fill("seed", 50, 64);
    let shim = crash_shim(&fx);
    let path = format!(
        "{}:{}",
        shim.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    // Recovery is killed at two moments: right after Docker created the
    // helper (only its name is journaled), and right after it started (its id
    // is too). Either leaves a container that mounts the snapshot's volume.
    for (index, (mode, journaled)) in [("verify-create", "name"), ("start", "id")]
        .into_iter()
        .enumerate()
    {
        let (snapshot, op_id) =
            snapshot_left_at_copied(&fx, &svc, root.id, &format!("seed{index}"));
        let killed = Command::new(&fx.bin)
            .arg("-C")
            .arg(&fx.project.root)
            .args(["service", "cache", "reconcile"])
            .env("NO_COLOR", "1")
            .env("PATH", &path)
            .env("DF_SHIM_REAL", real_docker())
            .env("DF_SHIM_CRASH_AFTER", mode)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(
            !killed.status.success() && killed.status.code().is_none(),
            "the recovery was killed by a signal ({mode}): {:?}",
            killed.status
        );
        let entry = svc.records().unwrap().op(op_id).cloned().unwrap();
        assert!(!entry.step.is_terminal(), "{entry:?}");
        let name = entry
            .verify_name
            .clone()
            .expect("the name is journaled first");
        assert_eq!(entry.verify_id.is_some(), journaled == "id", "{entry:?}");
        assert!(
            fx.containers().iter().any(|(_, n)| *n == name),
            "{mode}: the helper outlived the recovery: {:?}",
            fx.containers()
        );
        // It mounts the snapshot's volume, which therefore cannot be removed.
        assert!(
            docker::run(&["volume", "rm", &snapshot.volume]).is_err(),
            "{mode}: the volume is in use"
        );

        // The next recovery removes it by what was journaled, then finishes
        // the snapshot, and the volume is free again.
        let report = svc.reconcile().unwrap();
        assert!(report.problems.is_empty(), "{mode}: {report:?}");
        assert!(
            report.actions.iter().any(|a| a.contains("completed")),
            "{mode}: {report:?}"
        );
        assert!(
            !fx.containers().iter().any(|(_, n)| n.contains("-verify-")),
            "{mode}: no verify helper is left: {:?}",
            fx.containers()
        );
        let records = svc.records().unwrap();
        let entry = records.op(op_id).unwrap();
        assert_eq!(entry.step, Step::Done);
        assert!(entry.verify_name.is_none() && entry.verify_id.is_none());
        assert_eq!(
            records.snapshot(snapshot.id).unwrap().state,
            RecordState::Ready
        );
        // Deleting it works: nothing pins its volume.
        svc.remove_snapshot(&format!("seed{index}"), 1).unwrap();
        assert!(!volume_exists(&snapshot.volume));
    }
}

/// A directory with a `docker` that is the real one, except that
///
/// - a `create` of a `*-verify-*` helper sleeps `DF_SHIM_CREATE_DELAY`
///   seconds first (a client that is slow to reach the daemon);
/// - `container inspect` never answers while `DF_SHIM_INSPECT_HANG` is set;
/// - a `create` of a `*-verify-*` helper is handed to a job in a process group
///   of its own that runs the real one `DF_SHIM_LATE_CREATE` seconds later,
///   while the client waits for it: ending the client (and its group) leaves
///   the request on its way, as a daemon that keeps creating does;
/// - `rm` never answers while `DF_SHIM_RM_HANG` is set;
/// - the first `container inspect` whose arguments contain `DF_SHIM_BLIND_ONCE`
///   answers "no such container" (the helper appears between a cleanup's look
///   and its tombstone);
/// - a `create` of a `*-verify-*` helper fails at once, creating nothing, while
///   `DF_SHIM_CREATE_FAIL` is set.
///
/// A tombstone, whose name is a helper's, is never delayed, handed over or
/// refused by the create faults.
fn fault_shim(fx: &Fixture) -> std::path::PathBuf {
    let dir = fx.bin.parent().unwrap().join("shim-faults");
    std::fs::create_dir_all(&dir).unwrap();
    let script = r#"#!/bin/sh
if [ "$1" = create ] && [ -n "$DF_SHIM_LATE_CREATE" ]; then
  case "$*" in
    *dev.devflow.tombstone*) ;;
    *-verify-*)
      f="$0.args.$$"
      for a; do printf '%s\0' "$a"; done > "$f"
      set -m
      DF_JOB_FILE="$f" /bin/sh "$0.late" &
      wait
      exit 1 ;;
  esac
fi
if [ "$1" = create ] && [ -n "$DF_SHIM_CREATE_FAIL" ]; then
  case "$*" in
    *dev.devflow.tombstone*) ;;
    *-verify-*) echo "Error response from daemon: shim: create refused" >&2; exit 1 ;;
  esac
fi
if [ "$1" = create ] && [ -n "$DF_SHIM_CREATE_DELAY" ]; then
  case "$*" in
    *dev.devflow.tombstone*) ;;
    *-verify-*) sleep "$DF_SHIM_CREATE_DELAY" ;;
  esac
fi
if [ "$1" = container ] && [ "$2" = inspect ] && [ -n "$DF_SHIM_BLIND_ONCE" ]; then
  case "$*" in
    *"$DF_SHIM_BLIND_ONCE"*)
      if [ ! -e "$0.blind" ]; then
        : > "$0.blind"
        echo "Error: No such container: blind" >&2
        exit 1
      fi ;;
  esac
fi
if [ "$1" = rm ] && [ -n "$DF_SHIM_RM_HANG" ]; then
  exec sleep 600
fi
if [ "$1" = container ] && [ "$2" = inspect ] && [ -n "$DF_SHIM_INSPECT_HANG" ]; then
  exec sleep 600
fi
exec "$DF_SHIM_REAL" "$@"
"#;
    let path = dir.join("docker");
    std::fs::write(&path, script).unwrap();
    // The job of a late create: its command line names neither `docker` nor
    // the container, so ending the clients of a create does not find it.
    std::fs::write(
        dir.join("docker.late"),
        "sleep \"$DF_SHIM_LATE_CREATE\"\nxargs -0 \"$DF_SHIM_REAL\" < \"$DF_JOB_FILE\" >\"$DF_JOB_FILE.out\" 2>&1\necho \"exit=$?\" >> \"$DF_JOB_FILE.out\"\nrm -f \"$DF_JOB_FILE\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_verify_helper_whose_create_is_delayed_past_its_recovery_is_settled_by_the_next_one() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id).0.fill("seed", 50, 64);
    let (snapshot, op_id) = snapshot_left_at_copied(&fx, &svc, root.id, "seed");
    let shim = fault_shim(&fx);
    let path = format!(
        "{}:{}",
        shim.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    // Recovery 1 is killed while its `docker create` of the helper is still
    // on the way: the name is journaled, Docker has not heard of it.
    let mut first = Command::new(&fx.bin)
        .arg("-C")
        .arg(&fx.project.root)
        .args(["service", "cache", "reconcile"])
        .env("NO_COLOR", "1")
        .env("PATH", &path)
        .env("DF_SHIM_REAL", real_docker())
        .env("DF_SHIM_CREATE_DELAY", "15")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let name = loop {
        let entry = svc.records().unwrap().op(op_id).cloned().unwrap();
        if let Some(name) = entry.verify_name
            && !docker::create_clients(&name).unwrap().is_empty()
        {
            break name;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the recovery never journaled a helper"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    first.kill().unwrap();
    first.wait().unwrap();
    assert!(
        !fx.containers().iter().any(|(_, n)| *n == name),
        "the delayed create has not reached Docker: {:?}",
        fx.containers()
    );
    assert!(!docker::create_clients(&name).unwrap().is_empty());

    // Recovery 2 finds the name, waits for (or ends) the client before it
    // replaces the name, and completes the snapshot.
    let report = svc.reconcile().unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    assert!(
        report.actions.iter().any(|a| a.contains("completed")),
        "{report:?}"
    );
    // Long past the delay: the old client has nothing left to create.
    let waited = Instant::now();
    while !docker::create_clients(&name).unwrap().is_empty() {
        assert!(waited.elapsed() < Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        !fx.containers().iter().any(|(_, n)| n.contains("-verify-")),
        "no verify helper appeared late: {:?}",
        fx.containers()
    );
    assert_eq!(svc.records().unwrap().op(op_id).unwrap().step, Step::Done);
    // Nothing pins the volume.
    svc.remove_snapshot("seed", 1).unwrap();
    assert!(!volume_exists(&snapshot.volume));
}

/// `devflow service cache reconcile` in a child process whose `PATH` starts
/// with `shim` and whose environment has `vars`.
fn reconcile_child(fx: &Fixture, shim: &Path, vars: &[(&str, &str)]) -> std::process::Child {
    service_child(fx, shim, "reconcile", vars)
}

/// `devflow service cache <subcommand>` in such a child process.
fn service_child(
    fx: &Fixture,
    shim: &Path,
    subcommand: &str,
    vars: &[(&str, &str)],
) -> std::process::Child {
    let path = format!(
        "{}:{}",
        shim.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new(&fx.bin);
    command
        .arg("-C")
        .arg(&fx.project.root)
        .args(["service", "cache", subcommand])
        .env("NO_COLOR", "1")
        .env("PATH", &path)
        .env("DF_SHIM_REAL", real_docker())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for (name, value) in vars {
        command.env(name, value);
    }
    command.spawn().unwrap()
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_helper_create_delivered_after_cleanup_is_refused_by_the_tombstone_holding_its_name() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id).0.fill("seed", 50, 64);
    let (snapshot, op_id) = snapshot_left_at_copied(&fx, &svc, root.id, "seed");
    let shim = fault_shim(&fx);

    // Recovery 1 is killed while its `docker create` of the verify helper is
    // on the way: the request will reach the daemon in 16 seconds, from a job
    // that no kill of the client reaches.
    let mut first = reconcile_child(&fx, &shim, &[("DF_SHIM_LATE_CREATE", "16")]);
    let started = Instant::now();
    let name = loop {
        let entry = svc.records().unwrap().op(op_id).cloned().unwrap();
        if let Some(name) = entry.verify_name
            && !docker::create_clients(&name).unwrap().is_empty()
        {
            break name;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the recovery never journaled a helper"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    first.kill().unwrap();
    first.wait().unwrap();
    // The attempt is recorded apart from the operation, with its own name.
    let attempts = svc.records().unwrap().helpers;
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    assert_eq!((attempts[0].name.as_str(), attempts[0].op), (&*name, op_id));
    assert!(attempts[0].id.is_none() && attempts[0].tombstone.is_none());

    // Recovery 2 gives the client 2 seconds, ends it (and its group), finds
    // nothing under the name, and cannot tell whether the daemon will still
    // create it. It does not wait to find out: it occupies the name with a
    // tombstone, and completes the snapshot.
    unsafe { std::env::set_var("DEVFLOW_TEST_CREATE_WAIT_SECS", "2") };
    let report = svc.reconcile().unwrap();
    unsafe { std::env::remove_var("DEVFLOW_TEST_CREATE_WAIT_SECS") };
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(svc.records().unwrap().op(op_id).unwrap().step, Step::Done);
    assert!(
        docker::create_clients(&name).unwrap().is_empty(),
        "the client is gone"
    );
    let attempts = svc.records().unwrap().helpers;
    assert_eq!(attempts.len(), 1, "kept after the operation completed");
    let tombstone = attempts[0].tombstone.clone().expect("the name is fenced");
    let held = docker::inspect_container(&name).unwrap().unwrap();
    assert_eq!(held.id, tombstone);
    assert!(!held.running && held.status == "created", "{held:?}");
    assert!(held.labels.contains_key(docker::LABEL_TOMBSTONE));
    assert!(
        !held.labels.contains_key(docker::LABEL_HELPER),
        "a tombstone is not a helper"
    );
    assert!(
        docker::containers_mounting(&snapshot.volume)
            .unwrap()
            .is_empty(),
        "no mounts"
    );

    // Cleanup runs before the late create is delivered: the snapshot and its
    // volume are removed. The daemon's create has not landed yet.
    svc.remove_snapshot("seed", 1).unwrap();
    assert!(!volume_exists(&snapshot.volume));
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the cleanup ran before the late create ({:?})",
        started.elapsed()
    );
    assert_eq!(svc.records().unwrap().helpers.len(), 1, "the fence is kept");

    // The late create is delivered now. It fails with a name conflict, and
    // with it no helper appears and the volume is not made again, empty.
    let log = loop {
        let found = std::fs::read_dir(&shim)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("docker.args.") && n.ends_with(".out"))
            })
            .and_then(|p| std::fs::read_to_string(p).ok())
            .filter(|text| text.contains("exit="));
        if let Some(text) = found {
            break text;
        }
        assert!(
            started.elapsed() < Duration::from_secs(90),
            "the late create was never delivered"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(
        log.contains("is already in use") && !log.contains("exit=0"),
        "the late create failed with a name conflict: {log}"
    );
    assert!(
        !volume_exists(&snapshot.volume),
        "the late create did not make the volume again"
    );
    assert_eq!(
        docker::inspect_container(&name).unwrap().unwrap().id,
        tombstone,
        "the name is still the tombstone's"
    );
    assert!(
        !fx.containers()
            .iter()
            .any(|(id, n)| n.contains("-verify-") && *id != tombstone),
        "{:?}",
        fx.containers()
    );

    // Nothing but forget removes the tombstone, and it removes it last.
    let report = svc.reconcile().unwrap();
    assert!(
        report.problems.is_empty() && report.actions.is_empty(),
        "{report:?}"
    );
    assert!(container_exists(&tombstone));
    svc.forget().unwrap();
    assert!(!container_exists(&tombstone));
    assert!(fx.containers().is_empty(), "{:?}", fx.containers());
    assert!(fx.volumes().is_empty(), "{:?}", fx.volumes());
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn an_attempt_of_unknown_outcome_is_fenced_never_dropped_by_time() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id).0.fill("seed", 50, 64);
    let (_snapshot, op_id) = snapshot_left_at_copied(&fx, &svc, root.id, "seed");
    let shim = fault_shim(&fx);

    // The daemon refuses the create: the outcome of the request is known to
    // nobody, so the recovery rolls the snapshot back and fences the name.
    let mut child = reconcile_child(&fx, &shim, &[("DF_SHIM_CREATE_FAIL", "1")]);
    child.wait().unwrap();
    let records = svc.records().unwrap();
    assert!(records.op(op_id).unwrap().step.is_terminal());
    assert_eq!(records.helpers.len(), 1, "{:?}", records.helpers);
    let name = records.helpers[0].name.clone();
    let tombstone = records.helpers[0].tombstone.clone().expect("fenced");
    assert!(
        fx.containers()
            .iter()
            .any(|(id, n)| *n == name && *id == tombstone)
    );

    // Cleanups, however late, neither drop the attempt nor the tombstone.
    std::thread::sleep(Duration::from_secs(12));
    let report = svc.reconcile().unwrap();
    assert!(
        report.problems.is_empty() && report.actions.is_empty(),
        "{report:?}"
    );
    let records = svc.records().unwrap();
    assert_eq!(records.helpers.len(), 1);
    assert_eq!(records.helpers[0].tombstone.as_deref(), Some(&*tombstone));
    assert!(container_exists(&tombstone));

    // A tombstone someone removed is made again by the next sweep.
    docker::run(&["rm", "--force", &tombstone]).unwrap();
    let report = svc.reconcile().unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    let again = svc.records().unwrap().helpers[0].tombstone.clone().unwrap();
    assert_ne!(again, tombstone);
    assert_eq!(docker::inspect_container(&name).unwrap().unwrap().id, again);

    // Forget removes it last, with everything else.
    svc.forget().unwrap();
    assert!(!container_exists(&name));
    assert!(fx.containers().is_empty(), "{:?}", fx.containers());
    assert!(fx.volumes().is_empty(), "{:?}", fx.volumes());
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_tombstone_that_meets_the_real_helper_removes_it_by_its_labels_and_takes_its_place() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id).0.fill("seed", 50, 64);
    let snapshot = svc.snapshot_branch(root.id, "seed").unwrap();
    let shim = fault_shim(&fx);

    // A helper Docker created after the cleanup looked (a recorded attempt
    // whose id never came back), mounting the snapshot's volume.
    let op = Uuid::new_v4();
    let name = format!("devflow-test-late-{}", &op.simple().to_string()[..8]);
    docker::run(&[
        "create",
        "--name",
        &name,
        "--network",
        "none",
        "--label",
        &format!("{}={}", docker::LABEL_PROJECT, fx.project.id()),
        "--label",
        &format!("{}=cache", docker::LABEL_SERVICE),
        "--label",
        &format!("{}={op}", docker::LABEL_HELPER),
        "--label",
        &format!("{}={}", docker::LABEL_TEST, fx.label),
        "-v",
        &format!("{}:/v:ro", snapshot.volume),
        "debian:stable-slim",
        "true",
    ])
    .unwrap();
    assert!(docker::run(&["volume", "rm", &snapshot.volume]).is_err());
    Store::open(fx.project.id(), svc.docker_id())
        .unwrap()
        .update(|catalog| {
            let service = catalog.service_mut("cache", devflow_core::config::Engine::Valkey)?;
            service
                .helpers
                .push(devflow_core::services::catalog::HelperAttempt {
                    name: name.clone(),
                    op,
                    registered_at: root.created_at,
                    id: None,
                    tombstone: None,
                });
            Ok(())
        })
        .unwrap();

    // The cleanup's first look at the name says "absent" (the helper appears
    // right after it). Its tombstone conflicts with the real helper, which is
    // removed by its labels, and the tombstone is created again.
    let mut child = reconcile_child(&fx, &shim, &[("DF_SHIM_BLIND_ONCE", &name)]);
    assert!(child.wait().unwrap().success());
    let attempts = svc.records().unwrap().helpers;
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    let tombstone = attempts[0].tombstone.clone().expect("fenced");
    let held = docker::inspect_container(&name).unwrap().unwrap();
    assert_eq!(held.id, tombstone);
    assert!(held.labels.contains_key(docker::LABEL_TOMBSTONE));
    assert!(
        docker::containers_mounting(&snapshot.volume)
            .unwrap()
            .is_empty(),
        "the real helper is gone"
    );
    svc.remove_snapshot("seed", 1).unwrap();
    assert!(!volume_exists(&snapshot.volume));
}

/// A recorded helper attempt whose id never came back, with the real helper
/// Docker created in the end: a container named `name`, never started,
/// mounting `volume`, with this operation's helper labels.
fn plant_late_helper(
    fx: &Fixture,
    svc: &Service,
    root: &devflow_core::services::catalog::BranchRecord,
    volume: &str,
) -> String {
    let op = Uuid::new_v4();
    let name = format!("devflow-test-race-{}", &op.simple().to_string()[..8]);
    docker::run(&[
        "create",
        "--name",
        &name,
        "--network",
        "none",
        "--label",
        &format!("{}={}", docker::LABEL_PROJECT, fx.project.id()),
        "--label",
        &format!("{}=cache", docker::LABEL_SERVICE),
        "--label",
        &format!("{}={op}", docker::LABEL_HELPER),
        "--label",
        &format!("{}={}", docker::LABEL_TEST, fx.label),
        "-v",
        &format!("{volume}:/v:ro"),
        "debian:stable-slim",
        "true",
    ])
    .unwrap();
    plant_attempt(fx, svc, op, &name, root);
    name
}

/// Record an attempt of unknown outcome (no id) named `name`.
fn plant_attempt(
    fx: &Fixture,
    svc: &Service,
    op: Uuid,
    name: &str,
    root: &devflow_core::services::catalog::BranchRecord,
) {
    Store::open(fx.project.id(), svc.docker_id())
        .unwrap()
        .update(|catalog| {
            let service = catalog.service_mut("cache", devflow_core::config::Engine::Valkey)?;
            service
                .helpers
                .push(devflow_core::services::catalog::HelperAttempt {
                    name: name.to_string(),
                    op,
                    registered_at: root.created_at,
                    id: None,
                    tombstone: None,
                });
            Ok(())
        })
        .unwrap();
}

/// A directory with a `docker` that is the real one, except that the first
/// `container inspect` whose arguments contain `DF_SHIM_DELAY_ON` creates the
/// file `DF_SHIM_MARK` and then sleeps `DF_SHIM_DELAY` seconds before it
/// reaches the daemon: a sweep that read its attempt and is slow to look at
/// the container. With `DF_SHIM_SKIP=n` the first `n` such inspects pass and the
/// next one is held.
fn inspect_delay_shim(fx: &Fixture) -> std::path::PathBuf {
    let dir = fx.bin.parent().unwrap().join("shim-inspect-delay");
    std::fs::create_dir_all(&dir).unwrap();
    let script = r#"#!/bin/sh
if [ "$1" = container ] && [ "$2" = inspect ] && [ -n "$DF_SHIM_DELAY_ON" ]; then
  case "$*" in
    *"$DF_SHIM_DELAY_ON"*)
      if [ ! -e "$DF_SHIM_MARK" ]; then
        seen=$(cat "$DF_SHIM_MARK.seen" 2>/dev/null || echo 0)
        if [ "$seen" -ge "${DF_SHIM_SKIP:-0}" ]; then
          : > "$DF_SHIM_MARK"
          sleep "$DF_SHIM_DELAY"
        else
          echo $((seen + 1)) > "$DF_SHIM_MARK.seen"
        fi
      fi ;;
  esac
fi
exec "$DF_SHIM_REAL" "$@"
"#;
    let path = dir.join("docker");
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

fn wait_for_file(path: &Path) {
    let until = Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(Instant::now() < until, "{} never appeared", path.display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn two_sweeps_of_one_attempt_leave_no_tombstone_nobody_tracks() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id).0.fill("seed", 50, 64);
    let snapshot = svc.snapshot_branch(root.id, "seed").unwrap();
    let name = plant_late_helper(&fx, &svc, &root, &snapshot.volume);
    let shim = inspect_delay_shim(&fx);
    let mark = shim.join("slow-has-read-the-attempt");
    let _ = std::fs::remove_file(&mark);

    // The first sweep has read the attempt and is slow to look at the
    // container; a second one runs from start to end meanwhile: it removes the
    // late helper and resolves the attempt. The first one then finds the name
    // free, and must not fence an attempt that no longer exists.
    let mut slow = reconcile_child(
        &fx,
        &shim,
        &[
            ("DF_SHIM_DELAY_ON", &name),
            ("DF_SHIM_DELAY", "8"),
            ("DF_SHIM_MARK", mark.to_str().unwrap()),
        ],
    );
    wait_for_file(&mark);
    let mut fast = reconcile_child(&fx, &shim, &[]);
    assert!(fast.wait().unwrap().success());
    assert!(slow.wait().unwrap().success());

    assert!(
        svc.records().unwrap().helpers.is_empty(),
        "{:?}",
        svc.records().unwrap().helpers
    );
    assert!(
        !fx.containers().iter().any(|(_, n)| *n == name),
        "an orphaned container holds the name: {:?}",
        fx.containers()
    );
    assert!(
        docker::containers_mounting(&snapshot.volume)
            .unwrap()
            .is_empty()
    );
    svc.remove_snapshot("seed", 1).unwrap();
    assert!(!volume_exists(&snapshot.volume));
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn simultaneous_reconciles_resolve_a_late_helper_once_and_fence_nothing() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id).0.fill("seed", 50, 64);
    let snapshot = svc.snapshot_branch(root.id, "seed").unwrap();
    // Several attempts, so that the threads meet on more than one.
    let names: Vec<String> = (0..3)
        .map(|_| plant_late_helper(&fx, &svc, &root, &snapshot.volume))
        .collect();
    let start = std::sync::Barrier::new(4);
    let reports = std::thread::scope(|scope| {
        let runs: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    let own = fx.service();
                    start.wait();
                    own.reconcile()
                })
            })
            .collect();
        runs.into_iter()
            .map(|run| run.join().unwrap())
            .collect::<Vec<_>>()
    });
    for report in reports {
        let report = report.unwrap();
        assert!(report.problems.is_empty(), "{report:?}");
    }
    assert!(
        svc.records().unwrap().helpers.is_empty(),
        "{:?}",
        svc.records().unwrap().helpers
    );
    let left = fx.containers();
    assert!(
        !left.iter().any(|(_, n)| names.contains(n)),
        "containers hold the names: {left:?}"
    );
    svc.remove_snapshot("seed", 1).unwrap();
    assert!(!volume_exists(&snapshot.volume));
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_sweep_that_read_a_fenced_attempt_does_not_fence_again_behind_forget() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    // An attempt of unknown outcome, fenced by a sweep: the catalog lists it
    // with its tombstone.
    let name = format!(
        "devflow-test-forget-{}",
        &Uuid::new_v4().simple().to_string()[..8]
    );
    plant_attempt(&fx, &svc, Uuid::new_v4(), &name, &root);
    let report = svc.reconcile().unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    let tombstone = svc.records().unwrap().helpers[0]
        .tombstone
        .clone()
        .expect("fenced");
    let shim = inspect_delay_shim(&fx);
    let mark = shim.join("sweep-has-read-the-tombstone");
    let _ = std::fs::remove_file(&mark);

    // Another sweep has read the attempt and is slow to look at its
    // tombstone. `forget` runs meanwhile and removes the tombstone and the
    // entry. The sweep then finds the tombstone gone: it must neither make a
    // new one nor write into a service that is no longer there.
    let mut sweep = reconcile_child(
        &fx,
        &shim,
        &[
            ("DF_SHIM_DELAY_ON", &tombstone),
            ("DF_SHIM_DELAY", "8"),
            ("DF_SHIM_MARK", mark.to_str().unwrap()),
        ],
    );
    wait_for_file(&mark);
    let forgotten = svc.forget().unwrap();
    assert!(!forgotten.is_empty());
    assert!(sweep.wait().unwrap().success());

    assert!(
        !fx.containers().iter().any(|(_, n)| *n == name),
        "a tombstone nobody tracks holds the name: {:?}",
        fx.containers()
    );
    assert!(fx.containers().is_empty(), "{:?}", fx.containers());
    assert!(fx.volumes().is_empty(), "{:?}", fx.volumes());
    assert!(svc.records().unwrap().helpers.is_empty());
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_tombstone_made_for_an_attempt_that_is_gone_is_removed_not_left_untracked() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let name = format!(
        "devflow-test-gone-{}",
        &Uuid::new_v4().simple().to_string()[..8]
    );
    plant_attempt(&fx, &svc, Uuid::new_v4(), &name, &root);
    let shim = inspect_delay_shim(&fx);
    let mark = shim.join("sweep-has-read-the-attempt");
    let _ = std::fs::remove_file(&mark);

    // A sweep read the attempt (nothing has its name: it will fence it) and
    // is slow to look. The attempt is resolved by a writer that does not take
    // the attempt's lock (a coordinator finishing its own helper) before the
    // sweep's tombstone is recorded: the catalog write cannot find the
    // attempt, and the tombstone is removed, not left behind.
    let mut sweep = reconcile_child(
        &fx,
        &shim,
        &[
            ("DF_SHIM_DELAY_ON", &name),
            ("DF_SHIM_DELAY", "6"),
            ("DF_SHIM_MARK", mark.to_str().unwrap()),
        ],
    );
    wait_for_file(&mark);
    Store::open(fx.project.id(), svc.docker_id())
        .unwrap()
        .update(|catalog| {
            let service = catalog.service_mut("cache", devflow_core::config::Engine::Valkey)?;
            service.helpers.retain(|h| h.name != name);
            Ok(())
        })
        .unwrap();
    assert!(sweep.wait().unwrap().success());
    assert!(svc.records().unwrap().helpers.is_empty());
    assert!(
        !fx.containers().iter().any(|(_, n)| *n == name),
        "a tombstone nobody tracks holds the name: {:?}",
        fx.containers()
    );
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn two_forgets_of_one_service_never_interleave_and_leave_no_tombstone_behind() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    // No branch or snapshot is left: the service holds two fenced helpers.
    svc.remove_branch(root.id).unwrap();
    let names: Vec<String> = (0..2)
        .map(|i| {
            format!(
                "devflow-test-forgets-{i}-{}",
                &Uuid::new_v4().simple().to_string()[..8]
            )
        })
        .collect();
    for name in &names {
        plant_attempt(&fx, &svc, Uuid::new_v4(), name, &root);
    }
    let report = svc.reconcile().unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    let helpers = svc.records().unwrap().helpers;
    assert_eq!(helpers.len(), 2, "{helpers:?}");
    let second = helpers
        .iter()
        .find(|h| h.name == names[1])
        .and_then(|h| h.tombstone.clone())
        .expect("fenced");
    assert_eq!(fx.containers().len(), 2, "{:?}", fx.containers());

    // The service is no longer configured, so it can be forgotten.
    let path = fx.project.root.join("devflow.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace("services.cache", "services.sessions")).unwrap();

    let shim = inspect_delay_shim(&fx);
    let mark = shim.join("forget-is-removing-the-second-tombstone");
    let _ = std::fs::remove_file(&mark);
    let _ = std::fs::remove_file(format!("{}.seen", mark.display()));

    // The first forget removes the first tombstone and releases its attempt
    // lock, then is held just before it inspects the second tombstone (the
    // first inspect of it, by the sweep, passes). A second forget runs
    // meanwhile. Unserialised, it would make the first tombstone again, and
    // the first forget would delete the entry over it.
    let mut first = service_child(
        &fx,
        &shim,
        "forget",
        &[
            ("DF_SHIM_DELAY_ON", &second),
            ("DF_SHIM_SKIP", "1"),
            ("DF_SHIM_DELAY", "6"),
            ("DF_SHIM_MARK", mark.to_str().unwrap()),
        ],
    );
    wait_for_file(&mark);
    let started = Instant::now();
    let out = devflow(&fx, &["--json", "service", "cache", "forget"]);
    let waited = started.elapsed();
    let doc = json_of(&out);
    assert!(
        waited >= Duration::from_secs(3),
        "the second forget did not wait for the first ({waited:?})"
    );
    assert!(
        doc["removed"].as_array().unwrap().is_empty(),
        "the first forget had removed everything: {doc}"
    );
    assert!(first.wait().unwrap().success());

    assert!(
        fx.containers().is_empty(),
        "a tombstone nobody tracks is left: {:?}",
        fx.containers()
    );
    assert!(fx.volumes().is_empty(), "{:?}", fx.volumes());
    let catalog = Store::open(fx.project.id(), svc.docker_id())
        .unwrap()
        .load()
        .unwrap();
    assert!(!catalog.services.contains_key("cache"));
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_docker_query_that_hangs_ends_at_its_bound_and_the_recovery_stays_pending() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id).0.fill("seed", 50, 64);
    let (snapshot, op_id) = snapshot_left_at_copied(&fx, &svc, root.id, "seed");
    let shim = fault_shim(&fx);
    let path = format!(
        "{}:{}",
        shim.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    // Every `docker container inspect` hangs; each is ended after 3 seconds.
    let started = Instant::now();
    let hung = Command::new(&fx.bin)
        .arg("-C")
        .arg(&fx.project.root)
        .args(["service", "cache", "reconcile"])
        .env("NO_COLOR", "1")
        .env("PATH", &path)
        .env("DF_SHIM_REAL", real_docker())
        .env("DF_SHIM_INSPECT_HANG", "1")
        .env("DEVFLOW_TEST_QUERY_TIMEOUT_SECS", "3")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&hung.stdout),
        String::from_utf8_lossy(&hung.stderr)
    );
    assert!(
        started.elapsed() < Duration::from_secs(90),
        "the recovery was bounded: {:?}",
        started.elapsed()
    );
    assert!(
        text.contains("did not answer within 3 seconds"),
        "the timeout is the reported reason: {text}"
    );
    // Nothing was declared done: the journal entry is unfinished, and what
    // the recovery created (the verify helper) is still named in it.
    let entry = svc.records().unwrap().op(op_id).cloned().unwrap();
    assert!(!entry.step.is_terminal(), "{entry:?}");
    assert!(entry.verify_name.is_some(), "{entry:?}");

    // The next run, with a daemon that answers, settles it and cleans up.
    let report = svc.reconcile().unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    let entry = svc.records().unwrap().op(op_id).cloned().unwrap();
    assert!(entry.step.is_terminal(), "{entry:?}");
    assert!(
        !fx.containers().iter().any(|(_, n)| n.contains("-verify-")),
        "{:?}",
        fx.containers()
    );
    if entry.step == Step::Aborted {
        assert!(!volume_exists(&snapshot.volume));
    } else {
        svc.remove_snapshot("seed", 1).unwrap();
    }
}

/// A `docker` on the path (process wide, put back when dropped) that is the
/// real one, except that the first `docker stop` sleeps `delay` seconds before
/// it reaches the daemon: a stop that is delivered late. A later stop, such as
/// a recovery's own, goes straight through.
struct LateStop {
    path: String,
}

impl LateStop {
    fn install(fx: &Fixture, delay: u64) -> LateStop {
        let dir = fx.bin.parent().unwrap().join("shim-late-stop");
        std::fs::create_dir_all(&dir).unwrap();
        let mark = dir.join("first-stop-seen");
        let _ = std::fs::remove_file(&mark);
        let script = r#"#!/bin/sh
if [ "$1" = stop ] && [ ! -e "$DF_SHIM_MARK" ]; then
  : > "$DF_SHIM_MARK"
  sleep "$DF_SHIM_STOP_DELAY_ONCE"
fi
exec "$DF_SHIM_REAL" "$@"
"#;
        let shim = dir.join("docker");
        std::fs::write(&shim, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::var("PATH").unwrap_or_default();
        let real = real_docker();
        unsafe {
            std::env::set_var("PATH", format!("{}:{path}", dir.display()));
            std::env::set_var("DF_SHIM_REAL", real);
            std::env::set_var("DF_SHIM_MARK", &mark);
            std::env::set_var("DF_SHIM_STOP_DELAY_ONCE", delay.to_string());
        }
        LateStop { path }
    }
}

impl Drop for LateStop {
    fn drop(&mut self) {
        unsafe {
            std::env::set_var("PATH", &self.path);
            for name in ["DF_SHIM_REAL", "DF_SHIM_MARK", "DF_SHIM_STOP_DELAY_ONCE"] {
                std::env::remove_var(name);
            }
        }
    }
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_stop_delivered_after_the_old_deadline_cannot_take_down_the_branch_a_reset_rollback_restored() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id).0.fill("seed", 50, 64);
    let before = svc.branch(root.id).unwrap().unwrap();
    let old_id = before.container_id.clone().expect("a container id");

    // The cutover's `docker stop` (30 seconds to finish) reaches the daemon 65
    // seconds late: past requested_at + timeout + 30 s, the deadline recovery
    // used to wait for. The coordinator is killed while it waits for it.
    let _shim = LateStop::install(&fx, 65);
    let worker = {
        let svc = svc.clone();
        let branch = root.id;
        std::thread::spawn(move || svc.reset_branch(branch, SourceRef::Empty))
    };
    let started = Instant::now();
    let (op_id, pid) = loop {
        let records = svc.records().unwrap();
        if let Some(op) = records
            .journal
            .iter()
            .find(|o| o.kind == OpKind::Reset && o.target == root.id)
            && op.stop.is_some()
            && let Some(coordinator) = &op.coordinator
            && !docker::stop_clients(&old_id).unwrap().is_empty()
        {
            break (op.id, coordinator.pid);
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "no stop was journaled and issued"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let status = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(worker.join().unwrap().is_err());
    let stop = svc
        .records()
        .unwrap()
        .op(op_id)
        .and_then(|op| op.stop.clone())
        .expect("the request was journaled");
    assert_eq!((stop.container.as_str(), stop.outcome), (&*old_id, None));
    assert!(docker::inspect_container(&old_id).unwrap().unwrap().running);

    // Recovery rolls the reset back. It cannot prove the stop over, so it
    // fences the old container: stops it itself (the server saves its
    // dataset), removes it by id, and starts a new one on the same volume,
    // name and port.
    svc.up(root.id).unwrap();
    assert!(
        !docker::stop_clients(&old_id).unwrap().is_empty(),
        "recovery did not wait for the late stop"
    );
    let after = svc.branch(root.id).unwrap().unwrap();
    let new_id = after.container_id.clone().expect("a new container id");
    assert_ne!(new_id, old_id);
    assert!(!container_exists(&old_id));
    assert_eq!(after.container_name, before.container_name);
    assert_eq!(after.volume, before.volume);
    assert_eq!(after.port, before.port);
    assert_eq!(connect(&svc, root.id).0.dbsize(), 50, "nothing was lost");
    let records = svc.records().unwrap();
    assert_eq!(records.op(op_id).unwrap().step, Step::Aborted);
    let fence = records
        .op(op_id)
        .and_then(|op| op.stop.as_ref())
        .and_then(|stop| stop.fence.clone())
        .expect("the fence was journaled");
    assert_eq!(fence.replacement.as_deref(), Some(new_id.as_str()));
    assert_eq!(fx.containers().len(), 1, "{:?}", fx.containers());
    assert_eq!(fx.volumes(), vec![before.volume.clone()]);

    // The late stop is delivered now. It names a container that is gone.
    let waited = Instant::now();
    while !docker::stop_clients(&old_id).unwrap().is_empty() {
        assert!(
            waited.elapsed() < Duration::from_secs(120),
            "the late stop never finished"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(
        now_millis() / 1000 > stop.requested_at.timestamp() + 60,
        "the stop was delivered after requested_at + timeout + margin"
    );
    std::thread::sleep(Duration::from_secs(3));
    assert!(docker::inspect_container(&new_id).unwrap().unwrap().running);
    assert_eq!(connect(&svc, root.id).0.dbsize(), 50);
    let report = svc.reconcile().unwrap();
    assert!(
        report.actions.is_empty() && report.problems.is_empty(),
        "{report:?}"
    );
}

/// A directory with a `docker` that is the real one, except that every `stop`
/// waits `DF_SHIM_STOP_DELAY` seconds before it reaches the daemon: a request
/// the daemon holds, and carries out later, after its client is gone.
fn stop_delay_shim(fx: &Fixture) -> std::path::PathBuf {
    let dir = fx.bin.parent().unwrap().join("shim-stop-delay");
    std::fs::create_dir_all(&dir).unwrap();
    let script = r#"#!/bin/sh
if [ "$1" = stop ] && [ -n "$DF_SHIM_STOP_DELAY" ]; then
  sleep "$DF_SHIM_STOP_DELAY"
fi
exec "$DF_SHIM_REAL" "$@"
"#;
    let path = dir.join("docker");
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// `devflow service cache down --all` in a child process whose stops reach the
/// daemon `delay` seconds late, started once its request is journaled in the
/// branch's record and its client is running; then the child is killed (the
/// caller of an ordinary stop dies while Docker still holds the request).
/// Returns the journaled request.
fn down_killed_while_stop_is_held(
    fx: &Fixture,
    svc: &Service,
    branch: Uuid,
    container: &str,
    delay: u64,
) -> devflow_core::services::catalog::StopRequest {
    let shim = stop_delay_shim(fx);
    let path = format!(
        "{}:{}",
        shim.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new(&fx.bin)
        .arg("-C")
        .arg(&fx.project.root)
        .args(["service", "cache", "down", "--all"])
        .env("NO_COLOR", "1")
        .env("PATH", &path)
        .env("DF_SHIM_REAL", real_docker())
        .env("DF_SHIM_STOP_DELAY", delay.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let stop = loop {
        let record = svc.branch(branch).unwrap().unwrap();
        if let Some(stop) = record.stop
            && !docker::stop_clients(container).unwrap().is_empty()
        {
            break stop;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "no stop was journaled and issued"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    child.kill().unwrap();
    child.wait().unwrap();
    // The request is on record, unconfirmed, and the container still runs: the
    // daemon has not heard of the stop yet.
    assert_eq!((stop.container.as_str(), stop.outcome), (container, None));
    assert!(
        docker::inspect_container(container)
            .unwrap()
            .unwrap()
            .running
    );
    stop
}

/// Wait until no `docker stop` client of `container` is left.
fn wait_for_late_stop(container: &str) {
    let waited = Instant::now();
    while !docker::stop_clients(container).unwrap().is_empty() {
        assert!(
            waited.elapsed() < Duration::from_secs(120),
            "the late stop never finished"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    // And the daemon has done what it was told.
    std::thread::sleep(Duration::from_secs(3));
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_down_whose_caller_died_is_fenced_by_the_next_up_and_the_late_stop_finds_nothing() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id).0.fill("seed", 50, 64);
    let before = svc.branch(root.id).unwrap().unwrap();
    let old_id = before.container_id.clone().expect("a container id");

    // `service down` journals its stop in the branch's record and dies while
    // the stop is held on its way to the daemon (20 seconds late).
    let stop = down_killed_while_stop_is_held(&fx, &svc, root.id, &old_id, 20);
    assert!(
        stop.requested_at.timestamp() <= now_millis() / 1000 && stop.timeout_secs == 30,
        "the request time and grace are on record: {stop:?}"
    );

    // The next `up` does not trust the container: it stops it itself (the
    // server saves its dataset), removes it by the recorded id, confirms, and
    // starts a new one on the same volume, name, port and labels.
    svc.up(root.id).unwrap();
    assert!(
        !docker::stop_clients(&old_id).unwrap().is_empty(),
        "the late stop is still on its way"
    );
    let after = svc.branch(root.id).unwrap().unwrap();
    let new_id = after.container_id.clone().expect("a new container id");
    assert_ne!(new_id, old_id);
    assert!(!container_exists(&old_id));
    assert!(after.stop.is_none(), "the settled stop is cleared");
    assert_eq!(after.container_name, before.container_name);
    assert_eq!(after.volume, before.volume);
    assert_eq!(after.port, before.port);
    assert_eq!(connect(&svc, root.id).0.dbsize(), 50, "nothing was lost");
    assert_eq!(fx.containers().len(), 1, "{:?}", fx.containers());
    assert_eq!(fx.volumes(), vec![before.volume.clone()]);

    // The late stop is delivered now. It names a container that is gone.
    wait_for_late_stop(&old_id);
    assert!(docker::inspect_container(&new_id).unwrap().unwrap().running);
    assert_eq!(connect(&svc, root.id).0.dbsize(), 50);
    let report = svc.reconcile().unwrap();
    assert!(
        report.actions.is_empty() && report.problems.is_empty(),
        "{report:?}"
    );
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_down_after_an_unconfirmed_down_removes_the_container_instead_of_stopping_it_twice() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id).0.fill("seed", 50, 64);
    let old_id = svc
        .branch(root.id)
        .unwrap()
        .unwrap()
        .container_id
        .expect("a container id");
    down_killed_while_stop_is_held(&fx, &svc, root.id, &old_id, 15);

    // Another `down` would only issue a second stop at the same container and
    // leave the first one outstanding: it fences instead.
    assert!(svc.down(root.id).unwrap(), "it was running");
    assert!(!container_exists(&old_id));
    assert!(svc.branch(root.id).unwrap().unwrap().stop.is_none());
    assert!(fx.containers().is_empty(), "{:?}", fx.containers());

    // `up` makes a new container on the same volume, with the data.
    svc.up(root.id).unwrap();
    let new_id = svc
        .branch(root.id)
        .unwrap()
        .unwrap()
        .container_id
        .expect("a new container id");
    assert_ne!(new_id, old_id);
    assert_eq!(connect(&svc, root.id).0.dbsize(), 50);
    wait_for_late_stop(&old_id);
    assert!(docker::inspect_container(&new_id).unwrap().unwrap().running);
    assert_eq!(connect(&svc, root.id).0.dbsize(), 50);
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_confirmed_down_clears_its_record_and_up_keeps_the_same_container() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id).0.fill("seed", 50, 64);
    let id = svc
        .branch(root.id)
        .unwrap()
        .unwrap()
        .container_id
        .expect("a container id");

    assert!(svc.down(root.id).unwrap());
    let stopped = svc.branch(root.id).unwrap().unwrap();
    assert!(
        stopped.stop.is_none(),
        "the client returned success, so nothing is left to fence"
    );
    assert!(!docker::inspect_container(&id).unwrap().unwrap().running);
    // A second `down` has nothing to stop and records nothing.
    assert!(!svc.down(root.id).unwrap());
    assert!(svc.branch(root.id).unwrap().unwrap().stop.is_none());

    svc.up(root.id).unwrap();
    let started = svc.branch(root.id).unwrap().unwrap();
    assert_eq!(started.container_id.as_deref(), Some(id.as_str()));
    assert_eq!(connect(&svc, root.id).0.dbsize(), 50);
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_container_removal_that_hangs_ends_at_its_bound_and_the_recovery_stays_pending() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    connect(&svc, root.id).0.fill("seed", 50, 64);
    let (snapshot, op_id) = snapshot_left_at_copied(&fx, &svc, root.id, "seed");
    let shim = fault_shim(&fx);

    // Every `docker rm` hangs; each is ended after 3 seconds. The recovery
    // reads the snapshot through a helper it cannot remove.
    let started = Instant::now();
    let path = format!(
        "{}:{}",
        shim.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let hung = Command::new(&fx.bin)
        .arg("-C")
        .arg(&fx.project.root)
        .args(["service", "cache", "reconcile"])
        .env("NO_COLOR", "1")
        .env("PATH", &path)
        .env("DF_SHIM_REAL", real_docker())
        .env("DF_SHIM_RM_HANG", "1")
        .env("DEVFLOW_TEST_QUERY_TIMEOUT_SECS", "3")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&hung.stdout),
        String::from_utf8_lossy(&hung.stderr)
    );
    assert!(
        started.elapsed() < Duration::from_secs(90),
        "the recovery was bounded: {:?}",
        started.elapsed()
    );
    assert!(
        text.contains("`docker rm` did not answer within 3 seconds"),
        "the timeout is the reported reason: {text}"
    );
    // Not rolled back on a guess, not declared done: the entry is unfinished
    // where it was, and still names the helper it could not remove.
    let entry = svc.records().unwrap().op(op_id).cloned().unwrap();
    assert_eq!(entry.step, Step::Copied, "{entry:?}");
    assert!(entry.verify_id.is_some(), "{entry:?}");
    assert!(volume_exists(&snapshot.volume));

    // The next run, with a daemon that answers, removes the helper and
    // completes the snapshot.
    let report = svc.reconcile().unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(svc.records().unwrap().op(op_id).unwrap().step, Step::Done);
    assert!(
        !fx.containers().iter().any(|(_, n)| n.contains("-verify-")),
        "{:?}",
        fx.containers()
    );
    assert!(svc.records().unwrap().helpers.is_empty());
    svc.remove_snapshot("seed", 1).unwrap();
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_copy_past_its_deadline_kills_the_helper_and_aborts_the_destination() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    unsafe {
        std::env::set_var("DEVFLOW_TEST_COPY_DELAY_SECS", "120");
        std::env::set_var("DEVFLOW_TEST_COPY_TIMEOUT_SECS", "3");
    }
    let started = Instant::now();
    let id = Uuid::new_v4();
    let error = svc
        .create_branch(NewBranch {
            id: Some(id),
            name: "slow".into(),
            owner: None,
            source: SourceRef::Branch { id: root.id },
        })
        .unwrap_err();
    unsafe {
        std::env::remove_var("DEVFLOW_TEST_COPY_DELAY_SECS");
        std::env::remove_var("DEVFLOW_TEST_COPY_TIMEOUT_SECS");
    }
    assert!(
        format!("{error:#}").contains("took longer than 3 seconds"),
        "{error:#}"
    );
    assert!(started.elapsed() < Duration::from_secs(60));
    let records = svc.records().unwrap();
    let op = records.journal.iter().find(|o| o.target == id).unwrap();
    assert_eq!(op.step, Step::Aborted);
    assert!(
        op.error
            .as_deref()
            .unwrap_or("")
            .contains("helper was killed")
    );
    assert!(
        records
            .branch(id)
            .is_none_or(|b| b.state == RecordState::Aborted)
    );
    // Only the root's container and volume remain.
    assert_eq!(fx.containers().len(), 1, "{:?}", fx.containers());
    assert_eq!(fx.volumes(), vec![root.volume.clone()]);
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn without_reflink_a_copy_is_reported_and_require_cow_refuses() {
    // The copy helper pretends the filesystem refuses reflinks (a test hook:
    // the engine's own disk may well support them).
    for require_cow in [false, true] {
        let fx = Fixture::new(&format!(
            "[services.cache]\nengine = \"valkey\"\nrequire_cow = {require_cow}\n"
        ));
        let svc = fx.service();
        let root = fx.root_branch(&svc);
        let (mut conn, _, _) = connect(&svc, root.id);
        conn.fill("seed", 100, 32);

        let id = Uuid::new_v4();
        let volume = svc.volume_name(id, 1);
        unsafe { std::env::set_var("DEVFLOW_TEST_NO_REFLINK", "1") };
        let result = svc.create_branch(NewBranch {
            id: Some(id),
            name: "nocow".into(),
            owner: None,
            source: SourceRef::Branch { id: root.id },
        });
        if require_cow {
            let error = format!("{:#}", result.unwrap_err());
            assert!(
                error.contains("cannot share blocks") && error.contains("require_cow"),
                "{error}"
            );
            assert!(!volume_exists(&volume), "the destination was removed");
            let records = svc.records().unwrap();
            assert!(
                records
                    .branch(id)
                    .is_none_or(|b| b.state == RecordState::Aborted)
            );
        } else {
            let branch = result.unwrap();
            assert_eq!(branch.clone, Some(CloneMethod::Copy), "reported as a copy");
            assert_eq!(connect(&svc, id).0.dbsize(), 100);
        }
    }
    unsafe { std::env::remove_var("DEVFLOW_TEST_NO_REFLINK") };
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn ports_are_allocated_once_kept_and_never_moved_silently() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let id = Uuid::new_v4();
    let candidates = svc.candidate_ports(id);
    // The first candidate is held by something else: the next one is taken,
    // because the bind decides.
    let blocker = TcpListener::bind(("127.0.0.1", candidates[0])).ok();
    let branch = svc
        .create_branch(NewBranch {
            id: Some(id),
            name: "ports".into(),
            owner: None,
            source: SourceRef::Empty,
        })
        .unwrap();
    let port = branch.port.unwrap();
    if blocker.is_some() {
        assert_eq!(port, candidates[1], "the taken candidate was skipped");
    } else {
        assert!(candidates.contains(&port));
    }
    drop(blocker);

    // Stop, let something take the retained port, and start again: the port
    // is reported with its cause, not replaced.
    svc.down(id).unwrap();
    let thief = TcpListener::bind(("127.0.0.1", port)).unwrap();
    let error = format!("{:#}", svc.up(id).unwrap_err());
    assert!(
        error.contains(&port.to_string()) && error.contains("keeps the port it allocated"),
        "{error}"
    );
    assert_eq!(
        svc.branch(id).unwrap().unwrap().port,
        Some(port),
        "recorded port unchanged"
    );
    drop(thief);
    let endpoint = svc.up(id).unwrap();
    assert_eq!(endpoint.port, port);
    let (mut conn, _, _) = connect(&svc, id);
    conn.cmd(&["PING"]).unwrap();
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn the_catalog_is_bound_to_the_docker_daemon_and_orbstack_gets_a_name() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let container = docker::inspect_container(root.container_id.as_deref().unwrap())
        .unwrap()
        .unwrap();
    let operating_system = docker_ok(&["info", "--format", "{{.OperatingSystem}}"]);
    let domain = container.labels.get(docker::LABEL_ORBSTACK_DOMAINS);
    if operating_system.contains("OrbStack") {
        let domain = domain.expect("an OrbStack name");
        assert!(
            domain.ends_with(&format!(
                ".cache.svc-test-{}.devflow.orb.local",
                &fx.project.id().simple().to_string()[..8]
            )),
            "{domain}"
        );
        assert!(domain.starts_with(&root.id.simple().to_string()[..8]));
        let view = svc.branch_views().unwrap().remove(0);
        assert_eq!(view.container_endpoint, Some(format!("{domain}:6379")));
    } else {
        assert!(domain.is_none());
    }

    // Another daemon's catalog is refused, not adopted.
    let path = Store::path_for(fx.project.id()).unwrap();
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    json["docker"] = "some-other-daemon".into();
    std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    let error = format!("{:#}", svc.records().unwrap_err());
    assert!(error.contains("some-other-daemon"), "{error}");
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn the_cow_probe_is_measured_once_per_daemon_and_cached() {
    let fx = Fixture::valkey();
    let daemon = docker::daemon_id().unwrap();
    let first = probe::capability(&daemon, true);
    assert!(
        matches!(
            first.capability,
            CowCapability::Verified | CowCapability::Copy | CowCapability::Unknown
        ),
        "{first:?}"
    );
    if expect_cow() {
        assert_eq!(first.capability, CowCapability::Verified, "{first:?}");
    }
    let second = probe::capability(&daemon, false);
    assert_eq!(
        second.measured_at, first.measured_at,
        "the cached result is reused"
    );
    eprintln!("cow probe: {:?} ({:?})", first.capability, first.note);
    // The probe cleaned up after itself.
    assert!(fx.containers().is_empty(), "{:?}", fx.containers());
    assert!(fx.volumes().is_empty(), "{:?}", fx.volumes());
}

// ---- the pipeline ---------------------------------------------------------------------

fn create(
    fx: &Fixture,
    name: &str,
    options: SetupOptions,
) -> anyhow::Result<devflow_core::setup::SetupOutcome> {
    setup::create(
        &fx.project,
        CreateRequest {
            name: name.into(),
            from: None,
            options,
        },
        &mut NoProgress,
    )
}

fn env_url(workspace: &Path, name: &str) -> String {
    devflow_core::env_file::read(workspace)
        .unwrap()
        .into_iter()
        .find(|(key, _)| key == name)
        .unwrap_or_else(|| panic!("{name} is not in .devflow/env"))
        .1
}

fn password_of(url: &str) -> String {
    // redis://:<password>@127.0.0.1:<port>/0
    url.strip_prefix("redis://:")
        .and_then(|rest| rest.split_once('@'))
        .map(|(password, _)| password.to_string())
        .unwrap()
}

fn port_of(url: &str) -> u16 {
    url.rsplit_once(':')
        .and_then(|(_, rest)| rest.split_once('/'))
        .and_then(|(port, _)| port.parse().ok())
        .unwrap()
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn workspaces_get_their_own_branch_exported_url_and_drop_it_on_removal() {
    let fx = Fixture::new("[services.cache]\nengine = \"valkey\"\nenv = \"CACHE_URL\"\n");
    // The first workspace clones the default workspace, whose empty root
    // branch is created for it.
    let first = create(&fx, "feature-a", SetupOptions::default()).unwrap();
    assert!(
        first.status == devflow_core::receipt::Status::Ready
            || first.receipt.phase(Phase::Services).state == PhaseState::Done
    );
    assert_eq!(first.receipt.phase(Phase::Services).state, PhaseState::Done);
    let url_a = env_url(&first.workspace.path, "CACHE_URL");
    assert!(
        url_a.starts_with("redis://:") && url_a.ends_with("/0"),
        "{url_a}"
    );
    assert!(
        devflow_core::env_file::read(&first.workspace.path)
            .unwrap()
            .iter()
            .all(|(key, _)| key != "REDIS_URL"),
        "only the configured variable is exported"
    );
    // The URL works with the workspace's password only.
    let mut conn = Redis::auth(port_of(&url_a), &password_of(&url_a)).unwrap();
    conn.cmd(&["SET", "mine", "a"]).unwrap();
    let default = fx_default_branch(&fx);
    assert_ne!(password_of(&url_a), default.password);
    assert!(Redis::auth(port_of(&url_a), &default.password).is_err());

    // The receipt records the binding, the owner and how the data was cloned.
    let binding = first.receipt.services.get("cache").unwrap();
    assert_eq!(binding.url, url_a);
    assert!(binding.owned);
    assert_eq!(
        binding.source,
        format!("workspace:{}", first.receipt.code_source.clone().unwrap())
    );
    assert_method(binding.cloned.as_ref().unwrap().method);

    // Data written to the default workspace's branch reaches a new workspace.
    let svc = fx.service();
    let (mut root, _, _) = connect(&svc, default.id);
    root.cmd(&["SET", "from-default", "yes"]).unwrap();
    let second = create(&fx, "feature-b", SetupOptions::default()).unwrap();
    let url_b = env_url(&second.workspace.path, "CACHE_URL");
    let mut b = Redis::auth(port_of(&url_b), &password_of(&url_b)).unwrap();
    assert_eq!(b.get("from-default").as_deref(), Some("yes"));
    assert_eq!(
        b.get("mine"),
        None,
        "feature-a's writes are not in feature-b"
    );
    assert_ne!(password_of(&url_a), password_of(&url_b));

    // A workspace created from another one clones that workspace's branch.
    let third = setup::create(
        &fx.project,
        CreateRequest {
            name: "feature-c".into(),
            from: Some("feature-a".into()),
            options: SetupOptions::default(),
        },
        &mut NoProgress,
    )
    .unwrap();
    let url_c = env_url(&third.workspace.path, "CACHE_URL");
    let mut c = Redis::auth(port_of(&url_c), &password_of(&url_c)).unwrap();
    assert_eq!(c.get("mine").as_deref(), Some("a"));

    // A snapshot of a workspace, and a workspace created from it (`--data-from`).
    let snapshot =
        setup::service_snapshot(&fx.project, &first.workspace, "cache", "seed", None).unwrap();
    assert_eq!((snapshot.name.as_str(), snapshot.version), ("seed", 1));
    conn.cmd(&["SET", "mine", "changed"]).unwrap();
    let from_snapshot = create(
        &fx,
        "feature-d",
        SetupOptions {
            data_from: Some("snapshot:seed".parse::<DataSource>().unwrap()),
            ..SetupOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        from_snapshot.receipt.service_sources["cache"],
        "snapshot:seed@v1"
    );
    let url_d = env_url(&from_snapshot.workspace.path, "CACHE_URL");
    assert_eq!(
        Redis::auth(port_of(&url_d), &password_of(&url_d))
            .unwrap()
            .get("mine")
            .as_deref(),
        Some("a")
    );

    // A staged reset keeps the URL and brings the data back to the source.
    let before = first.receipt.services["cache"].url.clone();
    setup::service_reset(&fx.project, &first.workspace, "cache", None, None).unwrap();
    let mut reset = Redis::auth(port_of(&url_a), &password_of(&url_a)).unwrap();
    assert_eq!(
        reset.get("mine"),
        None,
        "back to the default branch's data at reset time"
    );
    assert_eq!(reset.get("from-default").as_deref(), Some("yes"));
    assert_eq!(before, url_a);

    // Removing a workspace drops its branch by recorded identity; the others stay.
    let record = svc
        .branch(first.receipt.services["cache"].branch_id.unwrap())
        .unwrap()
        .unwrap();
    let (container, volume) = (record.container_id.clone().unwrap(), record.volume.clone());
    let removed = setup::remove(
        &fx.project,
        "feature-a",
        setup::RemoveOptions {
            force: true,
            ..setup::RemoveOptions::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(removed.dropped_branch, Some(record.id));
    assert!(!container_exists(&container) && !volume_exists(&volume));
    assert!(
        Redis::auth(port_of(&url_b), &password_of(&url_b)).is_ok(),
        "feature-b is untouched"
    );

    // --keep-data leaves the branch (and `service list --orphans` finds it).
    let kept = second.receipt.services["cache"].branch_id.unwrap();
    setup::remove(
        &fx.project,
        "feature-b",
        setup::RemoveOptions {
            force: true,
            keep_data: true,
            ..setup::RemoveOptions::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert!(svc.branch(kept).unwrap().is_some());
    let orphans = devflow_core::services::workspace::orphans(&fx.project).unwrap();
    assert!(
        orphans.branches.iter().any(|b| b.branch == kept),
        "{orphans:?}"
    );
    svc.reconcile().unwrap();
}

/// The default workspace's branch record.
fn fx_default_branch(fx: &Fixture) -> devflow_core::services::catalog::BranchRecord {
    let default = devflow_core::worktree::list(&fx.project).unwrap().remove(0);
    assert!(default.is_default);
    setup::service_branch(&fx.project, &default, "cache")
        .unwrap()
        .1
}

/// Run the real binary in `fx`'s repository (the process environment, with
/// the test's data directory and label, is inherited).
fn devflow(fx: &Fixture, args: &[&str]) -> std::process::Output {
    Command::new(&fx.bin)
        .arg("-C")
        .arg(&fx.project.root)
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn json_of(out: &std::process::Output) -> serde_json::Value {
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn the_service_command_drives_branches_snapshots_and_resets_end_to_end() {
    let fx = Fixture::valkey();
    let first = create(&fx, "feature-a", SetupOptions::default()).unwrap();
    let url = env_url(&first.workspace.path, "REDIS_URL");
    let password = password_of(&url);
    Redis::auth(port_of(&url), &password)
        .unwrap()
        .cmd(&["SET", "marker", "one"])
        .unwrap();

    // Status: the engine, the measured capability, the branches. No password.
    let doc = json_of(&devflow(&fx, &["--json", "service", "status"]));
    assert_eq!(doc["command"], "service status");
    let service = &doc["services"][0];
    assert_eq!(service["key"], "cache");
    assert_eq!(service["engine"], "valkey");
    assert!(["verified", "copy", "unknown"].contains(&service["cow"].as_str().unwrap()));
    assert_eq!(
        service["branches"].as_array().unwrap().len(),
        2,
        "default + feature-a"
    );
    assert!(
        !doc.to_string().contains(&password),
        "listings never carry the password"
    );
    let human = devflow(&fx, &["service", "cache", "status"]);
    let text = String::from_utf8_lossy(&human.stdout).into_owned();
    assert!(
        text.contains("cow ") && text.contains("BRANCH") && text.contains("feature-a"),
        "{text}"
    );

    // url: the one command that hands out the credential.
    let doc = json_of(&devflow(&fx, &["--json", "service", "url", "feature-a"]));
    assert_eq!(doc["url"], url);
    assert_eq!(doc["workspace"], "feature-a");
    let printed = devflow(&fx, &["service", "url", "feature-a"]);
    assert_eq!(String::from_utf8_lossy(&printed.stdout).trim(), url);

    // down and up keep the port and the data.
    let doc = json_of(&devflow(&fx, &["--json", "service", "down", "feature-a"]));
    assert_eq!(doc["stopped"][0]["was_running"], true);
    assert!(
        TcpStream::connect(("127.0.0.1", port_of(&url))).is_err(),
        "stopped"
    );
    let doc = json_of(&devflow(&fx, &["--json", "service", "up", "feature-a"]));
    assert_eq!(
        doc["started"][0]["endpoint"],
        format!("127.0.0.1:{}", port_of(&url))
    );
    assert_eq!(
        Redis::auth(port_of(&url), &password)
            .unwrap()
            .get("marker")
            .as_deref(),
        Some("one")
    );

    // Snapshots through the CLI, then a branch from one with --data-from.
    let doc = json_of(&devflow(
        &fx,
        &[
            "--json",
            "service",
            "snapshot",
            "create",
            "seed",
            "--from",
            "feature-a",
        ],
    ));
    assert_eq!(doc["snapshot"]["name"], "seed");
    assert_eq!(doc["snapshot"]["version"], 1);
    let doc = json_of(&devflow(&fx, &["--json", "service", "snapshot", "list"]));
    assert_eq!(doc["snapshots"].as_array().unwrap().len(), 1);
    Redis::auth(port_of(&url), &password)
        .unwrap()
        .cmd(&["SET", "marker", "two"])
        .unwrap();

    // reset to the recorded source: the URL does not change.
    let doc = json_of(&devflow(&fx, &["--json", "service", "reset", "feature-a"]));
    assert_eq!(doc["branch"]["generation"], 2);
    assert_eq!(
        Redis::auth(port_of(&url), &password).unwrap().get("marker"),
        None,
        "back to the default branch's (empty) data"
    );
    let reset_to_snapshot = json_of(&devflow(
        &fx,
        &[
            "--json",
            "service",
            "reset",
            "feature-a",
            "--to",
            "snapshot:seed@v1",
        ],
    ));
    assert_eq!(reset_to_snapshot["branch"]["generation"], 3);
    assert_eq!(
        Redis::auth(port_of(&url), &password)
            .unwrap()
            .get("marker")
            .as_deref(),
        Some("one"),
        "the snapshot holds the first value"
    );
    // A missing snapshot fails the command and changes nothing.
    let out = devflow(
        &fx,
        &["service", "reset", "feature-a", "--to", "snapshot:nope"],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no snapshot named 'nope'"));

    // list, orphans and reconcile.
    let doc = json_of(&devflow(&fx, &["--json", "service", "list"]));
    assert_eq!(doc["services"][0]["snapshots"].as_array().unwrap().len(), 1);
    let doc = json_of(&devflow(&fx, &["--json", "service", "list", "--orphans"]));
    assert!(doc["orphans"]["branches"].as_array().unwrap().is_empty());
    let doc = json_of(&devflow(&fx, &["--json", "service", "reconcile"]));
    assert!(
        doc["services"][0]["report"]["problems"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{doc}"
    );

    // doctor reports the engine and the capability.
    let doc = json_of(&devflow(&fx, &["--json", "doctor"]));
    let names: Vec<&str> = doc["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert!(
        names.contains(&"services-docker") && names.contains(&"services-cow"),
        "{names:?}"
    );

    // delete a snapshot, drop a branch by id.
    let doc = json_of(&devflow(
        &fx,
        &["--json", "service", "snapshot", "delete", "seed@v1"],
    ));
    assert_eq!(doc["deleted"]["version"], 1);
    let svc = fx.service();
    let branch = first.receipt.services["cache"].branch_id.unwrap();
    let doc = json_of(&devflow(
        &fx,
        &["--json", "service", "drop", &branch.to_string()],
    ));
    assert_eq!(doc["dropped"], branch.to_string());
    assert!(svc.branch(branch).unwrap().is_none());
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn switch_starts_a_stopped_container_and_a_taken_port_is_a_warning_not_a_failure() {
    let fx = Fixture::valkey();
    let first = create(&fx, "feature-a", SetupOptions::default()).unwrap();
    let url = env_url(&first.workspace.path, "REDIS_URL");
    let (port, password) = (port_of(&url), password_of(&url));
    Redis::auth(port, &password)
        .unwrap()
        .cmd(&["SET", "kept", "yes"])
        .unwrap();
    let id = first.receipt.services["cache"].branch_id.unwrap();
    let svc = fx.service();

    // The workspace is ready, so the services phase does not run again; the
    // switch still starts the container.
    svc.down(id).unwrap();
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    let switched = setup::switch(
        &fx.project,
        &first.workspace,
        SetupOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert!(switched.warnings.is_empty(), "{:?}", switched.warnings);
    assert_eq!(
        Redis::auth(port, &password).unwrap().get("kept").as_deref(),
        Some("yes"),
        "the same port, the same password, the data a graceful stop saved"
    );

    // Something else took the port meanwhile: reported, the switch happened.
    svc.down(id).unwrap();
    let thief = TcpListener::bind(("127.0.0.1", port)).unwrap();
    let switched = setup::switch(
        &fx.project,
        &first.workspace,
        SetupOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    let warning = switched.warnings.join("\n");
    assert!(
        warning.contains("could not start service 'cache'") && warning.contains(&port.to_string()),
        "{warning}"
    );
    assert_eq!(svc.branch(id).unwrap().unwrap().port, Some(port));
    drop(thief);
    let switched = setup::switch(
        &fx.project,
        &first.workspace,
        SetupOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert!(switched.warnings.is_empty(), "{:?}", switched.warnings);
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_service_removed_from_the_configuration_keeps_its_data_until_it_is_forgotten() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let snapshot = svc.snapshot_branch(root.id, "seed").unwrap();
    assert!(volume_exists(&root.volume) && volume_exists(&snapshot.volume));

    // Rename the key in the configuration: the old data is an orphan, not gone.
    let path = fx.project.root.join("devflow.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace("services.cache", "services.sessions")).unwrap();
    let doc = json_of(&devflow(&fx, &["--json", "service", "list", "--orphans"]));
    assert_eq!(doc["orphans"]["services"][0], "cache", "{doc}");
    assert!(
        volume_exists(&root.volume),
        "removing a service never deletes data"
    );
    let human = devflow(&fx, &["service", "reconcile"]);
    assert!(String::from_utf8_lossy(&human.stdout).contains("no longer configured"));

    // Forgetting is explicit, by recorded identity, and refused while configured.
    let out = devflow(&fx, &["service", "sessions", "forget"]);
    assert!(
        !out.status.success(),
        "'sessions' is configured and has no catalog entry"
    );
    let doc = json_of(&devflow(&fx, &["--json", "service", "cache", "forget"]));
    assert!(doc["removed"].as_array().unwrap().len() >= 2, "{doc}");
    assert!(!volume_exists(&root.volume) && !volume_exists(&snapshot.volume));
    assert!(!container_exists(root.container_id.as_deref().unwrap()));
    let doc = json_of(&devflow(&fx, &["--json", "service", "list", "--orphans"]));
    assert!(doc["orphans"]["services"].as_array().unwrap().is_empty());
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_missing_data_source_fails_that_service_and_never_becomes_an_empty_one() {
    let fx = Fixture::valkey();
    let error = create(
        &fx,
        "feature-x",
        SetupOptions {
            data_from: Some("snapshot:nope".parse::<DataSource>().unwrap()),
            ..SetupOptions::default()
        },
    )
    .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("service 'cache' has no snapshot named 'nope'"),
        "{text}"
    );
    // Nothing was created in its place.
    assert!(fx.containers().is_empty(), "{:?}", fx.containers());
    let workspace = devflow_core::worktree::find(&fx.project, "feature-x")
        .unwrap()
        .unwrap();
    let receipt = devflow_core::receipt::Receipt::load(&workspace.git_dir)
        .unwrap()
        .unwrap();
    assert!(!receipt.services.contains_key("cache"));
    assert_eq!(receipt.phase(Phase::Services).state, PhaseState::Failed);
    assert!(
        !receipt.service_sources.contains_key("cache"),
        "a failed choice is not pinned"
    );

    // Naming a workspace without a branch fails the same way.
    let error = create(
        &fx,
        "feature-y",
        SetupOptions {
            data_from: Some(DataSource::Workspace {
                name: "feature-x".into(),
            }),
            ..SetupOptions::default()
        },
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("has no branch of service 'cache'"),
        "{error:#}"
    );
}

/// Make the next copy outlive its deadline, so a clone fails after its
/// source was resolved.
fn slow_copies(on: bool) {
    unsafe {
        if on {
            std::env::set_var("DEVFLOW_TEST_COPY_DELAY_SECS", "120");
            std::env::set_var("DEVFLOW_TEST_COPY_TIMEOUT_SECS", "3");
        } else {
            std::env::remove_var("DEVFLOW_TEST_COPY_DELAY_SECS");
            std::env::remove_var("DEVFLOW_TEST_COPY_TIMEOUT_SECS");
        }
    }
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn a_retry_clones_the_source_that_was_pinned_never_a_workspace_of_the_same_name() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let source = create(&fx, "source", SetupOptions::default()).unwrap();
    let old = source.receipt.services["cache"].branch_id.unwrap();
    connect(&svc, old).0.cmd(&["SET", "marker", "old"]).unwrap();

    // Creating 'feature' from 'source' fails after the source was resolved.
    let from_source = || SetupOptions {
        data_from: Some(DataSource::Workspace {
            name: "source".into(),
        }),
        ..SetupOptions::default()
    };
    slow_copies(true);
    let error = create(&fx, "feature", from_source()).unwrap_err();
    slow_copies(false);
    assert!(
        format!("{error:#}").contains("took longer than 3 seconds"),
        "{error:#}"
    );
    let feature = devflow_core::worktree::find(&fx.project, "feature")
        .unwrap()
        .unwrap();
    let pinned = devflow_core::receipt::Receipt::load(&feature.git_dir)
        .unwrap()
        .unwrap();
    assert_eq!(pinned.service_sources["cache"], "workspace:source");
    assert_eq!(
        pinned.service_source_ids["cache"],
        format!("branch:{old}"),
        "the identity is recorded with the label"
    );

    // 'source' is recreated: same name, a new instance, other data.
    setup::remove(
        &fx.project,
        "source",
        setup::RemoveOptions {
            force: true,
            keep_data: true,
            ..setup::RemoveOptions::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    let replacement = create(&fx, "source", SetupOptions::default()).unwrap();
    let new = replacement.receipt.services["cache"].branch_id.unwrap();
    assert_ne!(new, old);
    connect(&svc, new).0.cmd(&["SET", "marker", "new"]).unwrap();

    // The retry clones the branch that was pinned, not the replacement.
    let outcome = setup::setup(
        &fx.project,
        &feature,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    let cloned = outcome.receipt.services["cache"].branch_id.unwrap();
    assert_eq!(
        connect(&svc, cloned).0.get("marker").as_deref(),
        Some("old"),
        "the pinned source, whatever 'source' means now"
    );
    assert_eq!(
        svc.branch(cloned).unwrap().unwrap().source,
        SourceRef::Branch { id: old }
    );

    // A pinned source that is gone fails the service; the name is not
    // looked up again.
    slow_copies(true);
    create(&fx, "feature2", from_source()).unwrap_err();
    slow_copies(false);
    let feature2 = devflow_core::worktree::find(&fx.project, "feature2")
        .unwrap()
        .unwrap();
    svc.remove_branch(new).unwrap();
    let branches = svc.records().unwrap().branches.len();
    let error = setup::setup(
        &fx.project,
        &feature2,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("does not look the name up again") && text.contains("workspace:source"),
        "{text}"
    );
    assert_eq!(svc.records().unwrap().branches.len(), branches);
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn two_container_services_live_side_by_side_with_their_own_variables() {
    let fx = Fixture::new(
        "[services.cache]\nengine = \"valkey\"\n\n[services.queue]\nengine = \"redis\"\nimage = \"redis:8-alpine\"\nenv = \"QUEUE_URL\"\n",
    );
    let outcome = create(&fx, "both", SetupOptions::default());
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            // redis:8-alpine may not be present on a machine that never pulled it.
            let text = format!("{error:#}");
            assert!(text.contains("redis:8-alpine"), "{text}");
            eprintln!("skipped the redis half: {text}");
            return;
        }
    };
    let cache = env_url(&outcome.workspace.path, "REDIS_URL");
    let queue = env_url(&outcome.workspace.path, "QUEUE_URL");
    assert_ne!(port_of(&cache), port_of(&queue));
    Redis::auth(port_of(&cache), &password_of(&cache)).unwrap();
    Redis::auth(port_of(&queue), &password_of(&queue)).unwrap();
    assert!(Redis::auth(port_of(&queue), &password_of(&cache)).is_err());
}

#[test]
#[ignore = "needs Docker, valkey/valkey:8-alpine and debian:stable-slim"]
fn measure_a_clone_of_a_large_dataset() {
    let fx = Fixture::valkey();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let (mut conn, _, _) = connect(&svc, root.id);
    // About 128 MiB of incompressible values.
    conn.fill("big", 32_000, 4096);
    let used = docker_ok(&["system", "df", "--format", "{{.Type}} {{.Size}}"]);
    let started = Instant::now();
    let clone = svc
        .create_branch(NewBranch {
            id: None,
            name: "big-clone".into(),
            owner: None,
            source: SourceRef::Branch { id: root.id },
        })
        .unwrap();
    let took = started.elapsed();
    let mut copy = connect(&svc, clone.id).0;
    assert_eq!(copy.dbsize(), 32_000);
    eprintln!(
        "MEASURE clone of 32000 x 4 KiB keys: {took:?} end to end, method {:?}, \
         allocated in the VM {:?} bytes, captured at {:?}\ndocker system df before: {used}",
        clone.clone, clone.allocated_bytes, clone.captured_at
    );
    assert_method(clone.clone);
    if expect_cow() {
        let allocated = clone.allocated_bytes.unwrap_or(i64::MAX);
        assert!(
            allocated < 32 * 1024 * 1024,
            "a reflink shares blocks: {allocated}"
        );
    }
}

//! Docker tests of the MySQL 8.4 container service: the release gate of the
//! engine (milestone 6 of the developer-experience round).
//!
//! Ignored by default: they need Docker, the image `mysql:8.4` and
//! `debian:stable-slim`. Run them with
//!
//! ```sh
//! cargo test -p devflow --test services_mysql_docker -- --ignored --test-threads=1
//! ```
//!
//! Isolation: every test uses its own project id, data directory and
//! `DEVFLOW_TEST_LABEL`, which devflow puts on every container, volume and
//! helper it creates; a guard removes everything carrying the label when the
//! test ends, even by a panic. Nothing else on the machine is touched.
//!
//! They assert `reflink` only when `DEVFLOW_EXPECT_COW=verified` is set;
//! otherwise `reflink` or `copy` are both accepted and reported. Timings
//! (`MEASURE …`) go to standard error; read them with `--nocapture`.
use devflow_core::Project;
use devflow_core::config::Engine;
use devflow_core::db::DataSource;
use devflow_core::services::catalog::{CloneMethod, OpKind, RecordState, SourceRef, Step, Store};
use devflow_core::services::{NewBranch, Service, docker};
use devflow_core::setup::{self, CreateRequest, NoProgress, SetupOptions};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use uuid::Uuid;

// ---- a MySQL client of the test's own: `docker exec mysql` ------------------

const AUTH: &str = "IFS= read -r PW || exit 70\nMYSQL_PWD=\"$PW\" exec mysql \"$@\"\n";

/// Run `sql` in `container` as `user`, over TCP on the container's loopback
/// (or `extra` arguments, e.g. `--protocol=socket`). `Err` is the client's
/// error text.
fn mysql_as(
    container: &str,
    user: &str,
    password: &str,
    database: &str,
    extra: &[&str],
    sql: &str,
) -> Result<String, String> {
    let mut child = Command::new("docker")
        .args(["exec", "-i", container, "bash", "-c", AUTH, "client"])
        .args(["-u", user, "--batch", "--skip-column-names"])
        .args(extra)
        .arg(database)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("docker exec");
    let mut stdin = child.stdin.take().unwrap();
    let input = format!("{password}\n{sql}\n");
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let out = child.wait_with_output().unwrap();
    let _ = writer.join();
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// A branch as the tests talk to it.
struct Db {
    container: String,
    port: u16,
    user: String,
    password: String,
    root_password: String,
    database: String,
}

impl Db {
    fn of(svc: &Service, id: Uuid) -> Db {
        let record = svc.branch(id).unwrap().unwrap();
        let identity = record.mysql.clone().expect("a MySQL identity");
        Db {
            container: record.container_id.clone().expect("a container"),
            port: record.port.expect("a port"),
            user: identity.user,
            password: record.password.clone(),
            root_password: record.root_password.clone().expect("a root password"),
            database: identity.database,
        }
    }

    /// As the application account, over the container's loopback.
    fn app(&self, sql: &str) -> Result<String, String> {
        mysql_as(
            &self.container,
            &self.user,
            &self.password,
            &self.database,
            &["--protocol=tcp", "-h", "127.0.0.1"],
            sql,
        )
    }

    /// As root, through the socket (root exists only inside the container).
    fn root(&self, sql: &str) -> Result<String, String> {
        mysql_as(
            &self.container,
            "root",
            &self.root_password,
            "mysql",
            &["--protocol=socket"],
            sql,
        )
    }

    fn scalar(&self, sql: &str) -> String {
        self.app(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    /// `COUNT(*)` of `table`.
    fn count(&self, table: &str) -> u64 {
        self.scalar(&format!("SELECT COUNT(*) FROM {table};"))
            .parse()
            .unwrap()
    }

    /// `CHECK TABLE` must say OK for every table named.
    fn check(&self, table: &str) {
        let out = self.scalar(&format!("CHECK TABLE {table};"));
        assert!(
            out.lines().last().unwrap_or("").ends_with("OK"),
            "CHECK TABLE {table}: {out}"
        );
    }

    /// Fill `table` (id, payload) with about `rows` rows of 1 KiB of
    /// incompressible text, in batches.
    fn fill(&self, table: &str, rows: u64) {
        self.scalar(&format!(
            "CREATE TABLE IF NOT EXISTS {table} (id BIGINT AUTO_INCREMENT PRIMARY KEY, \
             payload VARCHAR(1100) NOT NULL) ENGINE=InnoDB;\n\
             CREATE TABLE IF NOT EXISTS seq (n INT PRIMARY KEY);\n\
             INSERT IGNORE INTO seq WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL \
             SELECT n+1 FROM r WHERE n < 999) SELECT n FROM r;"
        ));
        let md5s = vec!["MD5(RAND())"; 31].join(", ");
        let mut left = rows;
        while left > 0 {
            let batch = left.min(40_000);
            self.scalar(&format!(
                "INSERT INTO {table}(payload) SELECT CONCAT({md5s}) FROM seq a, seq b LIMIT {batch};"
            ));
            left -= batch;
        }
    }
}

// ---- fixtures -----------------------------------------------------------------

/// Everything a test needs, and the cleanup of everything it creates.
struct Fixture {
    _dir: tempfile::TempDir,
    bin: std::path::PathBuf,
    label: String,
    project: Project,
}

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
        let label = format!("df-my-{}", &Uuid::new_v4().simple().to_string()[..12]);
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
                "DEVFLOW_TEST_STOP_TIMEOUT_SECS",
                "DEVFLOW_TRUST_HOOKS",
                "DEVFLOW_INSTANCE_ID",
            ] {
                std::env::remove_var(var);
            }
        }
        let config = format!(
            "[project]\nname = \"my-test\"\nid = \"{}\"\n\n[worktree]\ndir = \"../repo.worktrees\"\n\n{services}",
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

    fn mysql() -> Fixture {
        Fixture::new("[services.sql]\nengine = \"mysql\"\ndatabase = \"shop\"\n")
    }

    fn service(&self) -> Service {
        Service::open(&self.project, "sql").unwrap()
    }

    fn root_branch(&self, svc: &Service) -> devflow_core::services::catalog::BranchRecord {
        svc.create_branch(NewBranch {
            id: None,
            name: "main".into(),
            owner: None,
            source: SourceRef::Empty,
        })
        .unwrap()
    }

    fn clone_of(
        &self,
        svc: &Service,
        source: Uuid,
    ) -> devflow_core::services::catalog::BranchRecord {
        svc.create_branch(NewBranch {
            id: None,
            name: "feature".into(),
            owner: None,
            source: SourceRef::Branch { id: source },
        })
        .unwrap()
    }

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

fn expect_cow() -> bool {
    std::env::var("DEVFLOW_EXPECT_COW").is_ok_and(|v| v == "verified")
}

fn container_exists(reference: &str) -> bool {
    docker::inspect_container(reference).unwrap().is_some()
}

fn container_running(reference: &str) -> bool {
    docker::inspect_container(reference)
        .unwrap()
        .is_some_and(|c| c.running)
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

/// A `docker` on the path that is the real one with misbehaviours switched on
/// by variables, for the faults a test cannot cause otherwise:
///
/// - `DF_SHIM_STOP_DELAY=N`: a `docker stop` sleeps N seconds before it
///   reaches the daemon (a stop that lands late);
/// - `DF_SHIM_STOP_DELAY_ONCE=N`: the same, for the first `docker stop` only
///   (a later one, such as a recovery's own, goes straight through);
/// - `DF_SHIM_STOP_VANISH=1`: a `docker stop` is carried to the daemon, and two
///   seconds later its client is killed and so is the process that ran it (the
///   daemon keeps carrying out a stop whose client has gone);
/// - `DF_SHIM_FAST2_ROOT_PW=…`: right before a `docker stop`, a session asks
///   the container's server for `innodb_fast_shutdown=2`;
/// - `DF_SHIM_HIDE_FAST=1`: `docker logs` leaves out InnoDB's notice of a very
///   fast shutdown, so the stop proof cannot see it.
///
/// The process environment and path are put back when it is dropped. Process
/// wide, like the rest of these tests (`--test-threads=1`).
struct Shim {
    path: String,
    vars: Vec<&'static str>,
}

impl Shim {
    fn install(fx: &Fixture, vars: &[(&'static str, &str)]) -> Shim {
        let real = Command::new("sh")
            .args(["-c", "command -v docker"])
            .output()
            .unwrap();
        let real = String::from_utf8_lossy(&real.stdout).trim().to_string();
        let dir = fx.bin.parent().unwrap().join("shim");
        std::fs::create_dir_all(&dir).unwrap();
        let script = r#"#!/bin/sh
if [ "$1" = stop ]; then
  if [ -n "$DF_SHIM_STOP_DELAY" ]; then sleep "$DF_SHIM_STOP_DELAY"; fi
  if [ -n "$DF_SHIM_STOP_DELAY_ONCE" ] && [ ! -e "$DF_SHIM_MARK" ]; then
    : > "$DF_SHIM_MARK"
    sleep "$DF_SHIM_STOP_DELAY_ONCE"
  fi
  if [ -n "$DF_SHIM_STOP_VANISH" ]; then
    "$DF_SHIM_REAL" "$@" &
    client=$!
    sleep 2
    kill -9 "$client"
    kill -9 "$PPID"
    exit 0
  fi
  if [ -n "$DF_SHIM_FAST2_ROOT_PW" ]; then
    for last; do :; done
    printf 'SET GLOBAL innodb_fast_shutdown = 2;\n' | "$DF_SHIM_REAL" exec -i \
      -e MYSQL_PWD="$DF_SHIM_FAST2_ROOT_PW" "$last" mysql -uroot --protocol=socket
  fi
fi
if [ "$1" = logs ] && [ -n "$DF_SHIM_HIDE_FAST" ]; then
  "$DF_SHIM_REAL" "$@" 2>&1 | grep -v "very fast shutdown"
  exit 0
fi
exec "$DF_SHIM_REAL" "$@"
"#;
        let shim = dir.join("docker");
        let mark = dir.join("first-stop-seen");
        let _ = std::fs::remove_file(&mark);
        std::fs::write(&shim, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::var("PATH").unwrap_or_default();
        unsafe {
            std::env::set_var("PATH", format!("{}:{path}", dir.display()));
            std::env::set_var("DF_SHIM_REAL", real);
            std::env::set_var("DF_SHIM_MARK", &mark);
            for (name, value) in vars {
                std::env::set_var(name, value);
            }
        }
        Shim {
            path,
            vars: vars.iter().map(|(name, _)| *name).collect(),
        }
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        unsafe {
            std::env::set_var("PATH", &self.path);
            std::env::remove_var("DF_SHIM_REAL");
            std::env::remove_var("DF_SHIM_MARK");
            for name in &self.vars {
                std::env::remove_var(name);
            }
        }
    }
}

/// The GREETING a MySQL server sends to whoever connects to `port` on the
/// host: the proof that the published endpoint reaches a MySQL server.
fn greeting(port: u16) -> Vec<u8> {
    use std::io::Read;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).unwrap();
    buf[..n].to_vec()
}

// ---- tests --------------------------------------------------------------------------

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn a_root_branch_is_initialized_authenticated_pinned_and_on_a_loopback_port() {
    let fx = Fixture::mysql();
    let svc = fx.service();
    let started = Instant::now();
    let root = fx.root_branch(&svc);
    eprintln!("MEASURE root branch created in {:?}", started.elapsed());
    assert_eq!(root.state, RecordState::Ready);
    assert_eq!(root.source, SourceRef::Empty);
    let db = Db::of(&svc, root.id);

    // Reachable with its own password, and the endpoint greets like MySQL.
    assert!((20_000..40_000).contains(&db.port), "{}", db.port);
    assert!(svc.candidate_ports(root.id).contains(&db.port));
    let hello = greeting(db.port);
    assert_eq!(hello[4], 10, "the MySQL greeting, protocol 10");
    assert_eq!(db.scalar("SELECT 1;"), "1");
    assert_eq!(db.scalar("SELECT DATABASE();"), "shop");
    assert!(db.scalar("SELECT @@version;").starts_with("8.4."));

    // Passwords: ours, per branch, never empty, and the wrong one is refused.
    assert_eq!(root.password.len(), 32);
    assert_eq!(db.root_password.len(), 32);
    assert_ne!(root.password, db.root_password);
    let denied = mysql_as(
        &db.container,
        &db.user,
        "not-the-password",
        &db.database,
        &["--protocol=tcp", "-h", "127.0.0.1"],
        "SELECT 1;",
    )
    .unwrap_err();
    assert!(denied.contains("Access denied"), "{denied}");
    assert_eq!(db.root("SELECT 1;").unwrap(), "1");
    let empty_root = mysql_as(
        &db.container,
        "root",
        "",
        "mysql",
        &["--protocol=socket"],
        "SELECT 1;",
    )
    .unwrap_err();
    assert!(empty_root.contains("Access denied"), "{empty_root}");

    // The accounts are exactly MySQL's own, root@localhost and the app user.
    let accounts = db
        .root("SELECT CONCAT(user, '@', host) FROM mysql.user ORDER BY user, host;")
        .unwrap();
    assert_eq!(
        accounts.lines().collect::<Vec<_>>(),
        [
            "app@%",
            "mysql.infoschema@localhost",
            "mysql.session@localhost",
            "mysql.sys@localhost",
            "root@localhost"
        ]
    );

    // The profile: standalone InnoDB, small, slow shutdown.
    assert_eq!(db.root("SELECT @@log_bin;").unwrap(), "0");
    assert_eq!(db.root("SELECT @@performance_schema;").unwrap(), "0");
    assert_eq!(db.root("SELECT @@innodb_fast_shutdown;").unwrap(), "0");
    assert_eq!(
        db.root("SELECT @@innodb_buffer_pool_size DIV 1048576;")
            .unwrap(),
        "64"
    );
    assert_eq!(db.root("SELECT @@bind_address;").unwrap(), "0.0.0.0");

    // The image is pinned by digest and recorded; ownership is the
    // catalog's, the labels only corroborate.
    assert!(
        root.pin.contains("@sha256:") || root.pin.starts_with("sha256:"),
        "{}",
        root.pin
    );
    let container = docker::inspect_container(&db.container).unwrap().unwrap();
    assert!(container.running);
    assert_eq!(container.published, Some(db.port));
    assert_eq!(container.labels[docker::LABEL_BRANCH], root.id.to_string());
    assert_eq!(container.labels[docker::LABEL_TEST], fx.label);
    let bindings = docker_ok(&["port", &container.id, "3306/tcp"]);
    assert!(bindings.starts_with("127.0.0.1:"), "{bindings}");
    // The profile is mounted read-only, and carries no credential.
    let mounts = docker_ok(&[
        "inspect",
        "--format",
        "{{range .Mounts}}{{.Destination}}={{.RW}} {{end}}",
        &container.id,
    ]);
    assert!(
        mounts.contains("/etc/mysql/conf.d/devflow.cnf=false"),
        "{mounts}"
    );
    // No secret is in the container's command line or environment.
    let spec = docker_ok(&[
        "inspect",
        "--format",
        "{{.Config.Cmd}} {{.Config.Env}} {{.Path}} {{.Args}}",
        &container.id,
    ]);
    assert!(!spec.contains(&root.password), "{spec}");
    assert!(!spec.contains(&db.root_password), "{spec}");

    // The catalog holds the credentials and is private.
    let catalog_path = Store::path_for(fx.project.id()).unwrap();
    let text = std::fs::read_to_string(&catalog_path).unwrap();
    assert!(text.contains(&root.password) && text.contains(&db.root_password));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&catalog_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // Idle memory of the branch (a measurement, reported).
    let stats = docker_ok(&[
        "stats",
        "--no-stream",
        "--format",
        "{{.MemUsage}}",
        &container.id,
    ]);
    let rss = docker_ok(&["exec", &container.id, "grep", "VmRSS", "/proc/1/status"]);
    eprintln!(
        "MEASURE idle MySQL: docker stats {stats}; mysqld {}",
        rss.trim()
    );

    // A stop and a start keep the data, the port and the passwords.
    db.scalar("CREATE TABLE kept (k INT PRIMARY KEY); INSERT INTO kept VALUES (7);");
    assert!(svc.down(root.id).unwrap());
    assert!(!svc.down(root.id).unwrap(), "already stopped");
    let endpoint = svc.up(root.id).unwrap();
    assert_eq!(endpoint.port, db.port, "the port is kept");
    assert!(endpoint.url.starts_with("mysql://app:"), "{}", endpoint.url);
    assert!(
        endpoint
            .url
            .ends_with(&format!("@127.0.0.1:{}/shop", db.port))
    );
    assert_eq!(Db::of(&svc, root.id).scalar("SELECT k FROM kept;"), "7");
}

// ---- a client that reconnects ------------------------------------------------------

/// A writer that inserts rows 1, 2, 3, … into `w(id, v)` through a new
/// connection each time (a `docker exec` of the client), and keeps trying
/// the same row while the server is down: a client that reconnects.
struct Writer {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: std::thread::JoinHandle<WriterLog>,
}

struct WriterLog {
    /// Rows the server acknowledged, with the instant it did.
    acked: Vec<(u64, Instant)>,
    /// Attempts that failed (the server was stopped or starting).
    failures: u32,
}

fn spawn_writer(db: &Db) -> Writer {
    use std::sync::atomic::{AtomicBool, Ordering};
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let (container, user, password, database) = (
        db.container.clone(),
        db.user.clone(),
        db.password.clone(),
        db.database.clone(),
    );
    let handle = std::thread::spawn(move || {
        let mut log = WriterLog {
            acked: Vec::new(),
            failures: 0,
        };
        let mut id = 1u64;
        while !flag.load(Ordering::Relaxed) {
            let result = mysql_as(
                &container,
                &user,
                &password,
                &database,
                &["--protocol=tcp", "-h", "127.0.0.1"],
                &format!("INSERT INTO w(id, v) VALUES ({id}, REPEAT('x', 100));"),
            );
            match result {
                Ok(_) => {
                    log.acked.push((id, Instant::now()));
                    id += 1;
                }
                // Committed by the attempt a stop cut short: the same row.
                Err(e) if e.contains("Duplicate entry") => {
                    log.acked.push((id, Instant::now()));
                    id += 1;
                }
                Err(_) => {
                    log.failures += 1;
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        log
    });
    Writer { stop, handle }
}

impl Writer {
    fn finish(self) -> WriterLog {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.handle.join().unwrap()
    }
}

fn ids(db: &Db, table: &str) -> Vec<u64> {
    db.scalar(&format!("SELECT id FROM {table} ORDER BY id;"))
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.parse().unwrap())
        .collect()
}

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn a_clone_under_concurrent_writes_has_every_committed_row_and_the_source_serves_again() {
    let fx = Fixture::mysql();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let db = Db::of(&svc, root.id);
    db.scalar("CREATE TABLE w (id BIGINT PRIMARY KEY, v VARCHAR(200) NOT NULL);");
    let writer = spawn_writer(&db);
    // Let it commit a few rows before the clone starts.
    std::thread::sleep(Duration::from_secs(4));

    let called = Instant::now();
    let wall_before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let clone = fx.clone_of(&svc, root.id);
    let returned = Instant::now();
    let wall_after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    // The source accepts writes again: the writer keeps going after the
    // restart (a client that reconnects), for a few more seconds.
    std::thread::sleep(Duration::from_secs(4));
    let log = writer.finish();
    eprintln!(
        "MEASURE clone of a source under writes: {:?} end to end; source downtime {:?} ms; \
         writer failures during the stop and restart: {}; method {:?}",
        returned - called,
        clone.downtime_ms,
        log.failures,
        clone.clone
    );
    assert!(log.failures > 0, "the writer saw the source go down");
    let downtime = clone.downtime_ms.expect("the downtime is recorded");
    assert!(downtime > 0 && downtime < 120_000, "{downtime}");

    let before: Vec<u64> = log
        .acked
        .iter()
        .filter(|(_, at)| *at < called)
        .map(|(id, _)| *id)
        .collect();
    let after: Vec<u64> = log
        .acked
        .iter()
        .filter(|(_, at)| *at > returned)
        .map(|(id, _)| *id)
        .collect();
    assert!(before.len() >= 3, "{} rows before the clone", before.len());
    assert!(!after.is_empty(), "the writer committed after the restart");

    // Every row committed before the stop is in the clone; none committed
    // after the source came back is; what is in it is a prefix (no holes).
    let cdb = Db::of(&svc, clone.id);
    let in_clone = ids(&cdb, "w");
    for id in &before {
        assert!(
            in_clone.contains(id),
            "row {id} committed before the clone is missing"
        );
    }
    for id in &after {
        assert!(
            !in_clone.contains(id),
            "row {id} was committed after the clone"
        );
    }
    let expected_prefix: Vec<u64> = (1..=in_clone.len() as u64).collect();
    assert_eq!(
        in_clone, expected_prefix,
        "no gaps: the clone is a consistent point"
    );
    cdb.check("w");

    // The source kept everything it acknowledged, before and after, and
    // takes a write now.
    let in_source = ids(&db, "w");
    for (id, _) in &log.acked {
        assert!(in_source.contains(id), "the source lost row {id}");
    }
    db.check("w");
    db.scalar("INSERT INTO w(id, v) VALUES (99999999, 'after the clone');");
    assert!(container_running(&db.container));

    // Reported, not assumed.
    assert_method(clone.clone);
    assert!(clone.allocated_bytes.is_some());
    if expect_cow() {
        let allocated = clone.allocated_bytes.unwrap();
        assert!(
            allocated < 64 * 1024 * 1024,
            "a reflink shares blocks: {allocated}"
        );
    }
    assert_eq!(clone.source, SourceRef::Branch { id: root.id });
    assert_eq!(
        clone.pin, root.pin,
        "the clone boots the source's pinned image"
    );
    let captured = clone.captured_at.expect("capture time").timestamp_millis();
    assert!(
        captured >= wall_before && captured <= wall_after,
        "{captured} in [{wall_before}, {wall_after}]"
    );
    // The journal records the whole sequence and the downtime.
    let records = svc.records().unwrap();
    let op = records
        .journal
        .iter()
        .find(|o| o.target == clone.id)
        .unwrap();
    assert_eq!(op.step, Step::Done);
    assert_eq!(op.downtime_ms, Some(downtime));
    assert_eq!(op.source_running, Some(true));
    // No bootstrap container is left, and the clone is the only new pair.
    assert_eq!(fx.containers().len(), 2, "{:?}", fx.containers());
    assert_eq!(fx.volumes().len(), 2, "{:?}", fx.volumes());
}

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn a_clone_has_its_own_identity_and_none_of_its_sources_credentials() {
    let fx = Fixture::mysql();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let db = Db::of(&svc, root.id);
    db.scalar("CREATE TABLE t (k INT PRIMARY KEY); INSERT INTO t VALUES (1), (2);");
    let clone = fx.clone_of(&svc, root.id);
    let cdb = Db::of(&svc, clone.id);
    assert_eq!(cdb.count("t"), 2, "the data came along");
    assert_ne!(clone.port, root.port);
    assert!(cdb.port != db.port);

    // Passwords differ per branch, and the source's do not open the clone.
    assert_ne!(clone.password, root.password);
    assert_ne!(cdb.root_password, db.root_password);
    let denied = mysql_as(
        &cdb.container,
        "app",
        &db.password,
        &db.database,
        &["--protocol=tcp", "-h", "127.0.0.1"],
        "SELECT 1;",
    )
    .unwrap_err();
    assert!(denied.contains("Access denied"), "{denied}");
    let denied = mysql_as(
        &cdb.container,
        "root",
        &db.root_password,
        "mysql",
        &["--protocol=socket"],
        "SELECT 1;",
    )
    .unwrap_err();
    assert!(denied.contains("Access denied"), "{denied}");
    // And the clone's open it, and only it.
    assert_eq!(cdb.scalar("SELECT 1;"), "1");
    assert_eq!(cdb.root("SELECT 1;").unwrap(), "1");
    assert!(
        mysql_as(
            &db.container,
            "app",
            &cdb.password,
            &db.database,
            &["--protocol=tcp", "-h", "127.0.0.1"],
            "SELECT 1;"
        )
        .is_err()
    );

    // A server of its own: a new UUID, new key pairs, same accounts.
    assert_ne!(
        db.root("SELECT @@server_uuid;").unwrap(),
        cdb.root("SELECT @@server_uuid;").unwrap()
    );
    for file in ["ca.pem", "server-key.pem", "private_key.pem", "auto.cnf"] {
        let path = format!("/var/lib/mysql/{file}");
        let source = docker_ok(&["exec", &db.container, "md5sum", &path]);
        let copy = docker_ok(&["exec", &cdb.container, "md5sum", &path]);
        assert!(!source.is_empty() && !copy.is_empty(), "{file} exists");
        assert_ne!(
            source.split_whitespace().next(),
            copy.split_whitespace().next(),
            "{file} is not shared with the source"
        );
    }
    let accounts = cdb
        .root("SELECT CONCAT(user, '@', host) FROM mysql.user ORDER BY user, host;")
        .unwrap();
    assert_eq!(accounts.lines().count(), 5, "{accounts}");

    // The clone never listened before its identity was set: its bootstrap
    // had no network, and its container is a different one.
    assert!(!fx.containers().iter().any(|(_, n)| n.ends_with("-boot")));
    // The catalog holds each branch's own credentials and nothing else's.
    let catalog = std::fs::read_to_string(Store::path_for(fx.project.id()).unwrap()).unwrap();
    for secret in [
        &root.password,
        &clone.password,
        &db.root_password,
        &cdb.root_password,
    ] {
        assert_eq!(
            catalog.matches(secret.as_str()).count(),
            1,
            "one record holds each"
        );
    }
}

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn a_secondary_password_and_other_inherited_access_never_open_the_clone() {
    let fx = Fixture::mysql();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let db = Db::of(&svc, root.id);
    db.scalar("CREATE TABLE t (k INT PRIMARY KEY); INSERT INTO t VALUES (1), (2);");

    // A proxy privilege is access to another account that a clone would keep:
    // refused by what it is, and the source is started again.
    db.root(
        "INSERT INTO mysql.proxies_priv (Host, User, Proxied_host, Proxied_user, With_grant, Grantor) \
         VALUES ('%', 'app', '', 'admin', 0, ''); FLUSH PRIVILEGES;",
    )
    .unwrap();
    let id = Uuid::new_v4();
    let error = svc
        .create_branch(NewBranch {
            id: Some(id),
            name: "refused".into(),
            owner: None,
            source: SourceRef::Branch { id: root.id },
        })
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("proxy privilege (%,app,,admin,0)") && text.contains("REVOKE"),
        "{text}"
    );
    assert!(container_running(&db.container), "the source is back");
    assert_eq!(fx.containers().len(), 1, "{:?}", fx.containers());
    assert_eq!(fx.volumes(), vec![root.volume.clone()]);
    db.root("DELETE FROM mysql.proxies_priv WHERE User = 'app'; FLUSH PRIVILEGES;")
        .unwrap();

    // The source's accounts keep their old passwords as secondary passwords
    // (`RETAIN CURRENT PASSWORD`), which a plain `ALTER USER … IDENTIFIED BY`
    // does not remove. Both the old and the new passwords open the source.
    db.root(
        "ALTER USER 'app'@'%' IDENTIFIED BY 'second-app-pw' RETAIN CURRENT PASSWORD;\n\
         ALTER USER 'root'@'localhost' IDENTIFIED BY 'second-root-pw' RETAIN CURRENT PASSWORD;",
    ) // kingfisher:ignore
    .unwrap();
    assert_eq!(
        db.scalar("SELECT 1;"),
        "1",
        "the old app password is secondary"
    );
    let secondaries = "SELECT COUNT(*) FROM mysql.user \
                       WHERE JSON_EXTRACT(User_attributes, '$.additional_password') IS NOT NULL;";
    let source_has = mysql_as(
        &db.container,
        "root",
        "second-root-pw", // kingfisher:ignore
        "mysql",
        &["--protocol=socket"],
        secondaries,
    )
    .unwrap();
    assert_eq!(source_has, "2");

    let clone = fx.clone_of(&svc, root.id);
    let cdb = Db::of(&svc, clone.id);
    assert_eq!(cdb.count("t"), 2, "the data came along");
    // None of the source's four passwords opens the clone.
    let socket = ["--protocol=socket"];
    let tcp = ["--protocol=tcp", "-h", "127.0.0.1"];
    for (user, password, database, extra) in [
        ("app", db.password.as_str(), "shop", &tcp[..]),
        ("app", "second-app-pw", "shop", &tcp[..]), // kingfisher:ignore
        ("root", db.root_password.as_str(), "mysql", &socket[..]),
        ("root", "second-root-pw", "mysql", &socket[..]), // kingfisher:ignore
    ] {
        let denied =
            mysql_as(&cdb.container, user, password, database, extra, "SELECT 1;").unwrap_err();
        assert!(denied.contains("Access denied"), "{user}: {denied}");
    }
    assert_eq!(cdb.scalar("SELECT 1;"), "1");
    assert_eq!(cdb.root("SELECT 1;").unwrap(), "1");
    // No account of the clone has a secondary password, and its accounts are
    // normalized; the source's are untouched.
    assert_eq!(cdb.root(secondaries).unwrap(), "0");
    assert_eq!(
        cdb.root(
            "SELECT GROUP_CONCAT(CONCAT(user, ':', plugin, ':', account_locked, ':', \
             password_expired) ORDER BY user) FROM mysql.user WHERE user IN ('app', 'root');"
        )
        .unwrap(),
        "app:caching_sha2_password:N:N,root:caching_sha2_password:N:N"
    );
    assert_eq!(
        mysql_as(
            &db.container,
            "root",
            "second-root-pw", // kingfisher:ignore
            "mysql",
            &socket,
            secondaries
        )
        .unwrap(),
        "2"
    );
}

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn a_source_with_a_memory_table_is_refused_untouched_and_the_profile_forbids_new_ones() {
    let fx = Fixture::mysql();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let db = Db::of(&svc, root.id);

    // The profile: InnoDB is the default, and a table cannot be created with
    // (or altered to) an engine that loses rows when the server stops.
    for sql in [
        "CREATE TABLE m (i INT) ENGINE=MEMORY;",
        "CREATE TABLE y (i INT) ENGINE=MyISAM;",
        "CREATE TEMPORARY TABLE tm (i INT) ENGINE=MEMORY;",
    ] {
        let denied = db.app(sql).unwrap_err();
        assert!(denied.contains("is disabled"), "{sql}: {denied}");
    }
    db.scalar("CREATE TABLE plain (i INT); INSERT INTO plain VALUES (1);");
    assert_eq!(
        db.scalar(
            "SELECT engine FROM information_schema.tables \
             WHERE table_schema = 'shop' AND table_name = 'plain';"
        ),
        "InnoDB"
    );
    let denied = db.app("ALTER TABLE plain ENGINE=MEMORY;").unwrap_err();
    assert!(denied.contains("is disabled"), "{denied}");

    // A source that predates the restriction (here: the setting persisted
    // away, which a restart picks up) can hold a MEMORY table.
    db.root("SET PERSIST_ONLY disabled_storage_engines = '';")
        .unwrap();
    svc.down(root.id).unwrap();
    svc.up(root.id).unwrap();
    let db = Db::of(&svc, root.id);
    db.scalar(
        "CREATE TABLE cache (k INT PRIMARY KEY, v VARCHAR(10)) ENGINE=MEMORY; \
         INSERT INTO cache VALUES (1, 'a'), (2, 'b');",
    );
    let started = docker::inspect_container(&db.container)
        .unwrap()
        .unwrap()
        .started_at;

    // The clone is refused before anything is stopped: the source keeps
    // serving, with the rows a stop would have lost.
    let id = Uuid::new_v4();
    let error = svc
        .create_branch(NewBranch {
            id: Some(id),
            name: "refused".into(),
            owner: None,
            source: SourceRef::Branch { id: root.id },
        })
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("`shop`.`cache` (MEMORY)")
            && text.contains("ALTER TABLE")
            && text.contains("source was not touched"),
        "{text}"
    );
    assert!(
        !text.contains("`shop`.`plain`"),
        "InnoDB tables are not listed"
    );
    let after = docker::inspect_container(&db.container).unwrap().unwrap();
    assert!(
        after.running && after.started_at == started,
        "never stopped"
    );
    assert_eq!(db.count("cache"), 2, "the MEMORY table kept its rows");
    assert_eq!(db.count("plain"), 1);
    let records = svc.records().unwrap();
    let op = records.journal.iter().find(|o| o.target == id).unwrap();
    assert_eq!(op.step, Step::Aborted);
    assert!(op.downtime_ms.is_none());
    assert_eq!(fx.containers().len(), 1, "{:?}", fx.containers());
    assert_eq!(fx.volumes(), vec![root.volume.clone()]);

    // Converted, the table is carried over.
    db.scalar("ALTER TABLE cache ENGINE=InnoDB;");
    let clone = fx.clone_of(&svc, root.id);
    assert_eq!(Db::of(&svc, clone.id).count("cache"), 2);
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Whether the container's log since its last start says InnoDB did crash
/// recovery.
fn recovered_at_last_start(container: &str) -> bool {
    let info = docker::inspect_container(container).unwrap().unwrap();
    docker::logs_since(&info.id, &info.started_at)
        .unwrap()
        .contains("Starting crash recovery")
}

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn a_source_set_to_a_fast_shutdown_is_made_slow_before_the_stop_and_persisted_values_are_reset() {
    let fx = Fixture::mysql();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let db = Db::of(&svc, root.id);
    db.scalar("CREATE TABLE t (k INT PRIMARY KEY); INSERT INTO t VALUES (1), (2), (3);");

    // A session asked for a crash-style shutdown, and persisted it, which a
    // restart would apply over the profile.
    db.root("SET PERSIST innodb_fast_shutdown = 2;").unwrap();
    assert_eq!(
        db.root("SELECT @@GLOBAL.innodb_fast_shutdown;").unwrap(),
        "2"
    );

    let clone = fx.clone_of(&svc, root.id);
    let cdb = Db::of(&svc, clone.id);
    assert_eq!(cdb.count("t"), 3);

    // The source was made slow right before its stop: it stopped without
    // recovery to do, restarted without doing any, and the persisted entry is
    // gone (otherwise the restart would have applied it again).
    let db = Db::of(&svc, root.id);
    assert_eq!(
        db.root("SELECT @@GLOBAL.innodb_fast_shutdown;").unwrap(),
        "0"
    );
    let persisted = docker_ok(&[
        "exec",
        &db.container,
        "cat",
        "/var/lib/mysql/mysqld-auto.cnf",
    ]);
    assert!(!persisted.contains("innodb_fast_shutdown"), "{persisted}");
    assert!(
        !recovered_at_last_start(&db.container),
        "the source did no recovery"
    );
    assert!(
        !recovered_at_last_start(&cdb.container),
        "nor did the clone"
    );
    assert_eq!(
        cdb.root("SELECT @@GLOBAL.innodb_fast_shutdown;").unwrap(),
        "0"
    );
    assert_eq!(db.count("t"), 3);
}

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn a_source_stopped_in_crash_style_is_refused_and_one_that_hides_it_is_caught_by_the_clone() {
    let fx = Fixture::mysql();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let db = Db::of(&svc, root.id);
    db.scalar("CREATE TABLE t (k INT PRIMARY KEY); INSERT INTO t VALUES (1), (2);");

    // Someone sets a very fast shutdown and stops the container themselves:
    // exit code 0, both shutdown reports in the log, and a data directory
    // that needs crash recovery.
    db.root("SET GLOBAL innodb_fast_shutdown = 2;").unwrap();
    docker::run(&["stop", "-t", "120", &db.container]).unwrap();
    let source = docker::inspect_container(&db.container).unwrap().unwrap();
    assert!(!source.running && source.exit_code == 0);

    // The stop proof sees the notice and refuses; nothing is made.
    let id = Uuid::new_v4();
    let error = svc
        .create_branch(NewBranch {
            id: Some(id),
            name: "refused".into(),
            owner: None,
            source: SourceRef::Branch { id: root.id },
        })
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("innodb_fast_shutdown=2") && text.contains("crash recovery"),
        "{text}"
    );
    assert_eq!(fx.containers().len(), 1, "{:?}", fx.containers());
    assert_eq!(fx.volumes(), vec![root.volume.clone()]);
    assert!(
        !container_running(&db.container),
        "the source is as it was found"
    );

    // Even if the log could not tell (here: it is hidden), the clone's first
    // start does: InnoDB logs that it began crash recovery, and the clone is
    // abandoned, whole.
    let shim = Shim::install(&fx, &[("DF_SHIM_HIDE_FAST", "1")]);
    let id = Uuid::new_v4();
    let error = svc
        .create_branch(NewBranch {
            id: Some(id),
            name: "hidden".into(),
            owner: None,
            source: SourceRef::Branch { id: root.id },
        })
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("needed InnoDB crash recovery when the clone first started"),
        "{text}"
    );
    let records = svc.records().unwrap();
    assert_eq!(
        records
            .journal
            .iter()
            .find(|o| o.target == id)
            .unwrap()
            .step,
        Step::Aborted
    );
    assert_eq!(fx.containers().len(), 1, "{:?}", fx.containers());
    assert_eq!(fx.volumes(), vec![root.volume.clone()]);
    drop(shim);

    // Started again (it recovers by itself) and cloned the usual way, it is
    // fine: the preflight makes the next shutdown a slow one.
    svc.up(root.id).unwrap();
    let clone = fx.clone_of(&svc, root.id);
    assert_eq!(Db::of(&svc, clone.id).count("t"), 2);
}

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn a_very_fast_shutdown_asked_after_the_preflight_is_caught_when_the_source_starts_again() {
    let fx = Fixture::mysql();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let db = Db::of(&svc, root.id);
    db.scalar("CREATE TABLE t (k INT PRIMARY KEY); INSERT INTO t VALUES (1), (2), (3);");

    // A session asks for innodb_fast_shutdown=2 in the instant between the
    // preflight (which set 0) and the stop, and the log cannot show it.
    let _shim = Shim::install(
        &fx,
        &[
            ("DF_SHIM_FAST2_ROOT_PW", db.root_password.as_str()),
            ("DF_SHIM_HIDE_FAST", "1"),
        ],
    );
    let id = Uuid::new_v4();
    let error = svc
        .create_branch(NewBranch {
            id: Some(id),
            name: "raced".into(),
            owner: None,
            source: SourceRef::Branch { id: root.id },
        })
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("ran InnoDB crash recovery when it started again"),
        "{text}"
    );
    // The clone was rolled back whole; the source serves, with its data.
    let records = svc.records().unwrap();
    let op = records.journal.iter().find(|o| o.target == id).unwrap();
    assert_eq!(op.step, Step::Aborted);
    assert_eq!(op.rollback_from, Some(Step::Restoring));
    assert_eq!(fx.containers().len(), 1, "{:?}", fx.containers());
    assert_eq!(fx.volumes(), vec![root.volume.clone()]);
    assert!(container_running(&db.container));
    let after = Db::of(&svc, root.id);
    assert_eq!(after.count("t"), 3);
    after.check("t");
}

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn a_stop_delivered_after_the_old_deadline_cannot_take_down_the_replaced_source() {
    let fx = Fixture::mysql();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let db = Db::of(&svc, root.id);
    db.scalar("CREATE TABLE t (k INT PRIMARY KEY); INSERT INTO t VALUES (1), (2);");
    let old_id = svc
        .branch(root.id)
        .unwrap()
        .unwrap()
        .container_id
        .expect("the source's container id");

    // The first `docker stop` reaches the daemon 50 seconds late. The stop has
    // 10 seconds to finish, so the deadline recovery used to wait for
    // (requested_at + timeout + 30 s) is 40 seconds after the request: the
    // delivery comes after it. The coordinator is killed while it waits.
    unsafe { std::env::set_var("DEVFLOW_TEST_STOP_TIMEOUT_SECS", "10") };
    let _shim = Shim::install(&fx, &[("DF_SHIM_STOP_DELAY_ONCE", "50")]);
    let id = Uuid::new_v4();
    let worker = {
        let svc = svc.clone();
        let source = root.id;
        std::thread::spawn(move || {
            svc.create_branch(NewBranch {
                id: Some(id),
                name: "feature".into(),
                owner: None,
                source: SourceRef::Branch { id: source },
            })
        })
    };
    let started = Instant::now();
    let (op_id, pid) = loop {
        let records = svc.records().unwrap();
        if let Some(op) = records.journal.iter().find(|o| o.target == id)
            && op.stop.is_some()
            && let Some(coordinator) = &op.coordinator
            && !docker::stop_clients(&db.container).unwrap().is_empty()
        {
            break (op.id, coordinator.pid);
        }
        assert!(
            started.elapsed() < Duration::from_secs(90),
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
    // The request is journaled with what recovery needs, and nobody saw it
    // return; the stop has not landed, so the source still runs.
    let entry = svc.records().unwrap().op(op_id).cloned().unwrap();
    let stop = entry.stop.clone().unwrap();
    assert_eq!(stop.container, old_id);
    assert_eq!(stop.outcome, None);
    assert!(stop.fence.is_none());
    assert!(container_running(&db.container));
    assert!(!docker::stop_clients(&old_id).unwrap().is_empty());

    // Recovery cannot prove the stop over, and no clock could: it fences the
    // container instead of reusing it. It stops it itself, removes it by its
    // recorded id and starts a new one on the same volume, name and port.
    svc.up(root.id).unwrap();
    assert!(
        !docker::stop_clients(&old_id).unwrap().is_empty(),
        "recovery did not wait for the late stop: it is still on its way"
    );
    let after = svc.branch(root.id).unwrap().unwrap();
    let new_id = after.container_id.clone().expect("a new container id");
    assert_ne!(new_id, old_id, "the source runs in a new container");
    assert!(!container_exists(&old_id), "the old container is gone");
    assert_eq!(after.container_name, root.container_name);
    assert_eq!(after.volume, root.volume);
    assert_eq!(after.port, root.port);
    assert!(container_running(&new_id));
    let records = svc.records().unwrap();
    assert_eq!(records.op(op_id).unwrap().step, Step::Aborted);
    let fence = records
        .op(op_id)
        .unwrap()
        .stop
        .as_ref()
        .and_then(|stop| stop.fence.clone())
        .expect("the fence was journaled");
    assert_eq!(fence.replacement.as_deref(), Some(new_id.as_str()));
    assert!(
        records
            .branch(id)
            .is_none_or(|b| b.state == RecordState::Aborted)
    );
    assert_eq!(fx.containers().len(), 1, "{:?}", fx.containers());
    assert_eq!(fx.volumes(), vec![root.volume.clone()]);

    // Now the late stop is delivered, past the old deadline. It names a
    // container that no longer exists.
    let waited = Instant::now();
    while !docker::stop_clients(&old_id).unwrap().is_empty() {
        assert!(
            waited.elapsed() < Duration::from_secs(120),
            "the late stop never finished"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(
        unix_now() > stop.requested_at.timestamp() + 40,
        "the stop was delivered after requested_at + timeout + margin"
    );
    std::thread::sleep(Duration::from_secs(5));
    assert!(container_running(&new_id), "the source stays up");
    assert_eq!(
        svc.branch(root.id)
            .unwrap()
            .unwrap()
            .container_id
            .as_deref(),
        Some(new_id.as_str())
    );
    let after = Db::of(&svc, root.id);
    assert_eq!(after.count("t"), 2);
    after.check("t");
    assert!(
        !recovered_at_last_start(&new_id),
        "the source was stopped cleanly by the recovery's own stop"
    );
    let report = svc.reconcile().unwrap();
    assert!(
        report.actions.is_empty() && report.problems.is_empty(),
        "{report:?}"
    );
}

// ---- a clone during DDL, a stop that is killed, a coordinator that dies --------------

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn a_stop_whose_client_vanished_after_the_daemon_accepted_it_is_fenced_not_assumed_over() {
    let fx = Fixture::mysql();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let db = Db::of(&svc, root.id);
    db.scalar("CREATE TABLE t (k INT PRIMARY KEY); INSERT INTO t VALUES (1), (2), (3);");
    let old_id = svc
        .branch(root.id)
        .unwrap()
        .unwrap()
        .container_id
        .expect("the source's container id");

    // The daemon accepts the stop; two seconds later its client and the
    // coordinator are both gone. Nothing is left to see: no client, and a
    // server that may or may not have begun to shut down.
    unsafe { std::env::set_var("DEVFLOW_TEST_STOP_TIMEOUT_SECS", "5") };
    let _shim = Shim::install(&fx, &[("DF_SHIM_STOP_VANISH", "1")]);
    let id = Uuid::new_v4();
    let result = svc.create_branch(NewBranch {
        id: Some(id),
        name: "feature".into(),
        owner: None,
        source: SourceRef::Branch { id: root.id },
    });
    assert!(result.is_err(), "the coordinator was killed");
    let entry = svc
        .records()
        .unwrap()
        .journal
        .iter()
        .find(|o| o.target == id)
        .cloned()
        .unwrap();
    let stop = entry.stop.clone().expect("the request was journaled");
    assert_eq!(stop.outcome, None, "nobody saw it return");
    assert_eq!(stop.timeout_secs, 5);
    assert!(
        docker::stop_clients(&db.container).unwrap().is_empty(),
        "no client is visible"
    );
    assert!(!entry.step.is_terminal());

    // Recovery does not guess from the clock whether the daemon is done: it
    // stops the container itself (waiting for whatever the daemon is doing),
    // removes it by id and starts a new one.
    svc.up(root.id).unwrap();
    let records = svc.records().unwrap();
    assert_eq!(records.op(entry.id).unwrap().step, Step::Aborted);
    let after = svc.branch(root.id).unwrap().unwrap();
    let new_id = after.container_id.clone().expect("a new container id");
    assert_ne!(new_id, old_id);
    assert!(!container_exists(&old_id));
    assert!(container_running(&new_id), "the source serves again");
    std::thread::sleep(Duration::from_secs(5));
    assert!(container_running(&new_id), "and stays up");
    let after = Db::of(&svc, root.id);
    assert_eq!(after.count("t"), 3);
    after.check("t");
    assert!(
        !recovered_at_last_start(&new_id),
        "the stop that was carried out was a clean one"
    );
    assert_eq!(fx.containers().len(), 1, "{:?}", fx.containers());
    let report = svc.reconcile().unwrap();
    assert!(
        report.actions.is_empty() && report.problems.is_empty(),
        "{report:?}"
    );
}

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn a_queued_clone_recovers_the_operation_a_dead_coordinator_left_before_it_runs() {
    let fx = Fixture::mysql();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let db = Db::of(&svc, root.id);
    db.scalar("CREATE TABLE t (k INT PRIMARY KEY); INSERT INTO t VALUES (1), (2), (3);");

    // Operation A stops the source and copies slowly.
    unsafe {
        std::env::set_var("DEVFLOW_TEST_COPY_DELAY_SECS", "60");
        std::env::set_var("DEVFLOW_TEST_STOP_TIMEOUT_SECS", "20");
    }
    let a_branch = Uuid::new_v4();
    let a_worker = {
        let svc = svc.clone();
        let source = root.id;
        std::thread::spawn(move || {
            svc.create_branch(NewBranch {
                id: Some(a_branch),
                name: "first".into(),
                owner: None,
                source: SourceRef::Branch { id: source },
            })
        })
    };
    let started = Instant::now();
    let (a_op, a_pid) = loop {
        let records = svc.records().unwrap();
        if let Some(op) = records.journal.iter().find(|o| o.target == a_branch)
            && op.step == Step::Copying
            && op.helper_id.is_some()
            && let Some(coordinator) = &op.coordinator
        {
            break (op.id, coordinator.pid);
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "A never reached its copy"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(!container_running(&db.container), "A stopped the source");
    unsafe { std::env::remove_var("DEVFLOW_TEST_COPY_DELAY_SECS") };

    // Request B was planned before A died: it had already passed the caller's
    // check for unfinished operations. Its coordinator is started now and
    // queues for the source's lock.
    let b_branch = Uuid::new_v4();
    let suffix = &b_branch.simple().to_string()[..12];
    let mut record = root.clone();
    record.id = b_branch;
    record.name = "second".into();
    record.state = RecordState::Creating;
    record.container_name = format!("devflow-queued-{suffix}-g1");
    record.container_id = None;
    record.volume = format!("devflow-queued-{suffix}-g1-v");
    record.password = Uuid::new_v4().simple().to_string();
    record.root_password = Some(Uuid::new_v4().simple().to_string());
    record.port = None;
    record.pending_port = None;
    record.domain = None;
    record.source = SourceRef::Branch { id: root.id };
    record.generation = 1;
    let mut op = devflow_core::services::catalog::Op::new(OpKind::Clone, b_branch);
    op.source = Some(SourceRef::Branch { id: root.id });
    op.copy_allowed = true;
    op.copy_timeout_secs = 300;
    op.stop_timeout_secs = 20;
    op.helper_name = Some(format!("devflow-queued-{suffix}-copy"));
    op.boot_name = Some(format!("{}-boot", record.container_name));
    let b_op = op.id;
    Store::open(fx.project.id(), svc.docker_id())
        .unwrap()
        .update(|catalog| {
            let service = catalog.service_mut("sql", Engine::Mysql)?;
            service.branches.push(record);
            service.journal.push(op);
            Ok(())
        })
        .unwrap();
    let mut b = Command::new(&fx.bin)
        .arg("__clone")
        .arg(fx.project.id().to_string())
        .arg("sql")
        .arg(b_op.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (lines, rx) = std::sync::mpsc::channel::<String>();
    {
        use std::io::BufRead;
        let stdout = b.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
    }
    // B waits on A's lock: it has not said it holds it.
    assert!(
        rx.recv_timeout(Duration::from_secs(4)).is_err(),
        "B must be waiting for the lock A holds"
    );

    // A's coordinator dies mid-copy. B gets the locks, and looks again before
    // it runs: A is unfinished with nobody holding it.
    let status = Command::new("kill")
        .args(["-9", &a_pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(a_worker.join().unwrap().is_err());

    let started = Instant::now();
    let mut said = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(line) => {
                let end = line == "done" || line.starts_with("error");
                said.push(line);
                if end {
                    break;
                }
            }
            Err(_) => assert!(
                started.elapsed() < Duration::from_secs(420),
                "B never finished: {said:?}"
            ),
        }
    }
    let _ = b.wait();
    assert_eq!(said.last().map(String::as_str), Some("done"), "{said:?}");
    let records = svc.records().unwrap();
    assert_eq!(
        records.op(a_op).unwrap().step,
        Step::Aborted,
        "A was recovered by B before B ran"
    );
    assert_eq!(
        records.branch(a_branch).unwrap().state,
        RecordState::Aborted
    );
    assert_eq!(records.op(b_op).unwrap().step, Step::Done);
    assert_eq!(records.branch(b_branch).unwrap().state, RecordState::Ready);
    // The source serves again, with its data, and was not left stopped.
    assert!(container_running(&db.container), "the source stays up");
    let after = Db::of(&svc, root.id);
    assert_eq!(after.count("t"), 3);
    after.check("t");
    let clone = Db::of(&svc, b_branch);
    assert_eq!(clone.count("t"), 3);
    // A's helper and destination are gone.
    assert_eq!(fx.containers().len(), 2, "{:?}", fx.containers());
    assert!(
        !fx.volumes()
            .contains(&records.branch(a_branch).unwrap().volume),
        "{:?}",
        fx.volumes()
    );
    let report = svc.reconcile().unwrap();
    assert!(
        report.actions.is_empty() && report.problems.is_empty(),
        "{report:?}"
    );
}

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn a_clone_during_a_long_alter_table_waits_or_aborts_cleanly_and_both_servers_check_out() {
    let fx = Fixture::mysql();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let db = Db::of(&svc, root.id);
    db.fill("big", 120_000);
    let rows = db.count("big");
    assert!(rows >= 100_000, "{rows}");
    db.check("big");

    // A copying ALTER in its own session; wait until the server shows it.
    let alter = {
        let (container, user, password, database) = (
            db.container.clone(),
            db.user.clone(),
            db.password.clone(),
            db.database.clone(),
        );
        std::thread::spawn(move || {
            mysql_as(
                &container,
                &user,
                &password,
                &database,
                &["--protocol=tcp", "-h", "127.0.0.1"],
                "ALTER TABLE big ADD COLUMN extra VARCHAR(100) NULL, ALGORITHM=COPY;",
            )
        })
    };
    let waited = Instant::now();
    loop {
        let busy = db
            .root(
                "SELECT COUNT(*) FROM information_schema.processlist WHERE info LIKE 'ALTER TABLE big%';",
            )
            .unwrap_or_default();
        if busy == "1" {
            break;
        }
        assert!(
            waited.elapsed() < Duration::from_secs(60),
            "the ALTER never showed up"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let started = Instant::now();
    let outcome = svc.create_branch(NewBranch {
        id: None,
        name: "during-ddl".into(),
        owner: None,
        source: SourceRef::Branch { id: root.id },
    });
    let took = started.elapsed();
    let alter_result = alter.join().unwrap();
    eprintln!(
        "MEASURE clone during a long ALTER TABLE: {took:?}, {}; the ALTER session ended with {}",
        match &outcome {
            Ok(b) => format!("cloned (source down {:?} ms)", b.downtime_ms),
            Err(e) => format!("aborted: {}", format!("{e:#}").lines().next().unwrap_or("")),
        },
        match &alter_result {
            Ok(_) => "success".to_string(),
            Err(e) => e.lines().next().unwrap_or("").to_string(),
        }
    );

    // Whatever happened, the source is running, intact and consistent.
    assert!(container_running(&db.container));
    let db = Db::of(&svc, root.id);
    assert_eq!(db.count("big"), rows);
    db.check("big");
    let leftovers =
        db.root("SELECT COUNT(*) FROM information_schema.tables WHERE table_name LIKE '#sql%';");
    assert_eq!(leftovers.as_deref(), Ok("0"), "no half-made table");
    let sessions_ok = db.scalar("SELECT 1;");
    assert_eq!(sessions_ok, "1");

    match outcome {
        Ok(clone) => {
            // The stop waited for the DDL to end (it was cut off and rolled
            // back, or it finished first): the clone is whole either way.
            let cdb = Db::of(&svc, clone.id);
            assert_eq!(cdb.count("big"), rows);
            cdb.check("big");
            let leftovers = cdb.root(
                "SELECT COUNT(*) FROM information_schema.tables WHERE table_name LIKE '#sql%';",
            );
            assert_eq!(leftovers.as_deref(), Ok("0"));
            assert_eq!(svc.records().unwrap().unfinished().len(), 0);
        }
        Err(error) => {
            // The clone was abandoned: nothing of it is left, and the
            // journal says why.
            let text = format!("{error:#}");
            assert!(
                text.contains("did not shut down cleanly") || text.contains("took longer"),
                "{text}"
            );
            let records = svc.records().unwrap();
            assert!(records.unfinished().is_empty());
            assert_eq!(fx.containers().len(), 1, "{:?}", fx.containers());
            assert_eq!(fx.volumes().len(), 1, "{:?}", fx.volumes());
        }
    }
}

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn a_stop_that_ends_in_sigkill_aborts_the_clone_and_the_source_restarts() {
    let fx = Fixture::mysql();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let db = Db::of(&svc, root.id);
    db.scalar("CREATE TABLE t (k INT PRIMARY KEY); INSERT INTO t VALUES (1), (2), (3);");

    // A stop timeout of zero kills the server at once (exit 137).
    unsafe { std::env::set_var("DEVFLOW_TEST_STOP_TIMEOUT_SECS", "0") };
    let id = Uuid::new_v4();
    let error = svc
        .create_branch(NewBranch {
            id: Some(id),
            name: "killed".into(),
            owner: None,
            source: SourceRef::Branch { id: root.id },
        })
        .unwrap_err();
    unsafe { std::env::remove_var("DEVFLOW_TEST_STOP_TIMEOUT_SECS") };
    let text = format!("{error:#}");
    assert!(
        text.contains("did not shut down cleanly") && text.contains("137"),
        "{text}"
    );

    // The clone was rolled back whole; no unclean copy was ever made.
    let records = svc.records().unwrap();
    let op = records.journal.iter().find(|o| o.target == id).unwrap();
    assert_eq!(op.step, Step::Aborted);
    assert_eq!(op.rollback_from, Some(Step::Quiescing));
    assert_eq!(op.source_running, Some(true));
    assert!(op.downtime_ms.is_none());
    assert!(op.helper_id.is_none());
    assert!(
        records
            .branch(id)
            .is_none_or(|b| b.state == RecordState::Aborted)
    );
    assert_eq!(fx.containers().len(), 1, "{:?}", fx.containers());
    assert_eq!(fx.volumes(), vec![root.volume.clone()]);

    // The source was started again, ran its crash recovery, and has its data.
    assert!(container_running(&db.container), "the source restarts");
    let after = Db::of(&svc, root.id);
    assert_eq!(after.count("t"), 3);
    after.check("t");
    assert_eq!(after.port, db.port, "same endpoint");
    // And the next clone works.
    let clone = fx.clone_of(&svc, root.id);
    assert_eq!(Db::of(&svc, clone.id).count("t"), 3);
}

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn a_coordinator_killed_mid_copy_is_recovered_source_restarted_helper_killed_destination_aborted() {
    let fx = Fixture::mysql();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let db = Db::of(&svc, root.id);
    db.scalar("CREATE TABLE t (k INT PRIMARY KEY); INSERT INTO t VALUES (1), (2);");

    // A copy that takes a while, so the helper outlives its coordinator.
    unsafe { std::env::set_var("DEVFLOW_TEST_COPY_DELAY_SECS", "60") };
    let id = Uuid::new_v4();
    let worker = {
        let svc = svc.clone();
        let source = root.id;
        std::thread::spawn(move || {
            svc.create_branch(NewBranch {
                id: Some(id),
                name: "feature".into(),
                owner: None,
                source: SourceRef::Branch { id: source },
            })
        })
    };
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
            started.elapsed() < Duration::from_secs(90),
            "no helper was journaled"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    // The source is stopped, by the coordinator, and nobody has started it.
    assert!(container_exists(&helper), "the copier runs in the VM");
    assert!(!container_running(&db.container), "the source is down");
    let status = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(worker.join().unwrap().is_err());
    unsafe { std::env::remove_var("DEVFLOW_TEST_COPY_DELAY_SECS") };
    assert!(
        container_exists(&helper),
        "the helper outlives its coordinator"
    );
    assert!(
        !container_running(&db.container),
        "and so does the downtime"
    );
    let dest = svc.records().unwrap().branch(id).cloned().unwrap();
    assert_eq!(dest.state, RecordState::Creating, "never promoted");

    // The next operation on the source (here `up`, not `reconcile`) finds
    // the unfinished entry first: it kills the helper, confirms it gone,
    // aborts the destination, starts the source again.
    let endpoint = svc.up(root.id).unwrap();
    assert_eq!(endpoint.port, db.port);
    assert!(
        !container_exists(&helper),
        "the helper was killed and confirmed gone"
    );
    let records = svc.records().unwrap();
    assert_eq!(records.op(op_id).unwrap().step, Step::Aborted);
    assert_eq!(
        records.op(op_id).unwrap().rollback_from,
        Some(Step::Copying)
    );
    assert!(
        records
            .branch(id)
            .is_none_or(|b| b.state == RecordState::Aborted)
    );
    assert!(
        !volume_exists(&dest.volume),
        "the half-copied volume is gone"
    );
    assert!(container_running(&db.container));
    let after = Db::of(&svc, root.id);
    assert_eq!(after.count("t"), 2);
    after.check("t");
    assert_eq!(fx.containers().len(), 1, "{:?}", fx.containers());
    assert_eq!(fx.volumes(), vec![root.volume.clone()]);
    // `reconcile` has nothing left to do, and a retry just works.
    let report = svc.reconcile().unwrap();
    assert!(
        report.actions.is_empty() && report.problems.is_empty(),
        "{report:?}"
    );
    let again = fx.clone_of(&svc, root.id);
    assert_eq!(Db::of(&svc, again.id).count("t"), 2);
}

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn a_source_with_an_account_devflow_did_not_create_is_refused_by_name_and_restarts() {
    let fx = Fixture::mysql();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let db = Db::of(&svc, root.id);
    db.root("CREATE USER 'reporting'@'%' IDENTIFIED BY 'inherited-secret';")
        .unwrap(); // kingfisher:ignore
    db.scalar("CREATE TABLE t (k INT PRIMARY KEY); INSERT INTO t VALUES (1);");

    let id = Uuid::new_v4();
    let error = svc
        .create_branch(NewBranch {
            id: Some(id),
            name: "refused".into(),
            owner: None,
            source: SourceRef::Branch { id: root.id },
        })
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("'reporting'@'%'") && text.contains("DROP USER"),
        "{text}"
    );
    assert!(
        !text.contains("inherited-secret"),
        "a credential is never in a message"
    );
    // Nothing of the refused clone is left, and no server ever listened.
    let records = svc.records().unwrap();
    assert_eq!(
        records
            .op(records.journal.iter().find(|o| o.target == id).unwrap().id)
            .unwrap()
            .step,
        Step::Aborted
    );
    assert_eq!(fx.containers().len(), 1, "{:?}", fx.containers());
    assert_eq!(fx.volumes(), vec![root.volume.clone()]);
    // The source is back, with its data.
    assert!(container_running(&db.container));
    assert_eq!(Db::of(&svc, root.id).count("t"), 1);

    // Dropping the account makes the clone possible.
    db.root("DROP USER 'reporting'@'%';").unwrap();
    let clone = fx.clone_of(&svc, root.id);
    assert_eq!(Db::of(&svc, clone.id).count("t"), 1);
}

// ---- snapshots, staged resets, drops, ports ----------------------------------------------

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn snapshots_resets_drops_and_ports_follow_the_same_rules_as_clones() {
    let fx = Fixture::mysql();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let db = Db::of(&svc, root.id);
    db.scalar(
        "CREATE TABLE t (k INT PRIMARY KEY, stage VARCHAR(20)); INSERT INTO t VALUES (1, 'one');",
    );

    // A snapshot: a clean stop, a reflink into a volume of its own, no
    // container, and the source serving again.
    let started = Instant::now();
    let v1 = svc.snapshot_branch(root.id, "seed").unwrap();
    eprintln!(
        "MEASURE snapshot: {:?} end to end; source down {:?} ms; {:?}",
        started.elapsed(),
        v1.downtime_ms,
        v1.clone
    );
    assert_eq!((v1.name.as_str(), v1.version), ("seed", 1));
    assert_eq!(v1.state, RecordState::Ready);
    assert_method(v1.clone);
    assert!(v1.captured_at.is_some() && v1.downtime_ms.is_some());
    assert!(
        v1.mysql.is_some(),
        "the schema and account are recorded with the data"
    );
    assert!(volume_exists(&v1.volume));
    assert!(
        !fx.containers()
            .iter()
            .any(|(_, name)| name.contains(&v1.id.simple().to_string()[..12])),
        "no container for a snapshot"
    );
    assert!(container_running(&db.container), "the source serves again");
    db.scalar("UPDATE t SET stage = 'two' WHERE k = 1; INSERT INTO t VALUES (2, 'two');");
    let v2 = svc.snapshot_branch(root.id, "seed").unwrap();
    assert_eq!(v2.version, 2);

    // Branches from each version see that version's data; the snapshot was
    // not started, so it can be used again and again.
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
        from_v1.captured_at, v1.captured_at,
        "a snapshot's capture time is inherited"
    );
    assert_eq!(from_v1.downtime_ms, None, "no source was stopped for it");
    let (d1, d2) = (Db::of(&svc, from_v1.id), Db::of(&svc, from_v2.id));
    assert_eq!(d1.scalar("SELECT GROUP_CONCAT(stage) FROM t;"), "one");
    assert_eq!(
        d2.scalar("SELECT GROUP_CONCAT(stage ORDER BY k) FROM t;"),
        "two,two"
    );
    assert_ne!(d1.password, d2.password);
    assert_ne!(
        d1.root("SELECT @@server_uuid;").unwrap(),
        d2.root("SELECT @@server_uuid;").unwrap()
    );
    d1.check("t");

    // A staged reset keeps the endpoint and the credentials, replaces the
    // data from the recorded source, and removes the original pair.
    d1.scalar("INSERT INTO t VALUES (3, 'mine');");
    let before = svc.branch(from_v1.id).unwrap().unwrap();
    let (old_container, old_volume) = (before.container_id.clone().unwrap(), before.volume.clone());
    let reset = svc
        .reset_branch(from_v1.id, SourceRef::Snapshot { id: v2.id })
        .unwrap();
    assert_eq!(reset.generation, 2);
    assert_eq!(reset.port, Some(d1.port), "the port is kept");
    assert_eq!(reset.password, d1.password, "the credentials are kept");
    assert_eq!(reset.root_password, before.root_password);
    assert_ne!(reset.volume, old_volume);
    let after = Db::of(&svc, from_v1.id);
    assert_eq!(
        after.scalar("SELECT GROUP_CONCAT(stage ORDER BY k) FROM t;"),
        "two,two"
    );
    assert_eq!(
        after.scalar("SELECT COUNT(*) FROM t WHERE stage = 'mine';"),
        "0"
    );
    after.check("t");
    assert_method(reset.clone);
    assert!(
        !container_exists(&old_container),
        "the original container is gone"
    );
    assert!(!volume_exists(&old_volume), "and so is its volume");
    assert!(
        !fx.containers().iter().any(|(_, n)| n.ends_with("-stage")),
        "{:?}",
        fx.containers()
    );
    let op = svc
        .records()
        .unwrap()
        .journal
        .iter()
        .rev()
        .find(|o| o.kind == OpKind::Reset)
        .cloned()
        .unwrap();
    assert_eq!(op.step, Step::Done);

    // A reset to a branch resets from a running source (clean stop again);
    // to empty it starts from an initialized server, on the same endpoint.
    let reset = svc
        .reset_branch(from_v1.id, SourceRef::Branch { id: root.id })
        .unwrap();
    assert_eq!(reset.generation, 3);
    assert!(reset.downtime_ms.is_some(), "the source was stopped for it");
    assert_eq!(Db::of(&svc, from_v1.id).count("t"), 2);
    assert!(container_running(&db.container), "the source serves again");
    let emptied = svc.reset_branch(from_v1.id, SourceRef::Empty).unwrap();
    assert_eq!(emptied.generation, 4);
    assert_eq!(emptied.port, Some(d1.port));
    let cleared = Db::of(&svc, from_v1.id);
    assert_eq!(
        cleared
            .scalar("SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = 'shop';"),
        "0"
    );
    assert!(
        svc.reset_branch(from_v1.id, SourceRef::Branch { id: from_v1.id })
            .is_err(),
        "a branch cannot be reset from itself"
    );

    // Ports are allocated once and kept; a taken one is reported, never moved.
    svc.down(from_v1.id).unwrap();
    let up = svc.up(from_v1.id).unwrap();
    assert_eq!(up.port, d1.port);
    svc.down(from_v1.id).unwrap();
    let squatter = TcpListener::bind(("127.0.0.1", d1.port));
    if let Ok(_held) = squatter {
        let error = svc.up(from_v1.id).unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains(&d1.port.to_string()) && text.contains("not available"),
            "{text}"
        );
        assert_eq!(svc.branch(from_v1.id).unwrap().unwrap().port, Some(d1.port));
    }
    svc.up(from_v1.id).unwrap();

    // Drop removes the recorded container and volume, by identity, and a
    // snapshot's volume goes with its deletion; the others stay.
    let victim = svc.branch(from_v2.id).unwrap().unwrap();
    svc.remove_branch(from_v2.id).unwrap();
    assert!(!container_exists(victim.container_id.as_deref().unwrap()));
    assert!(!volume_exists(&victim.volume));
    svc.remove_snapshot("seed", 1).unwrap();
    assert!(!volume_exists(&v1.volume));
    assert!(volume_exists(&v2.volume), "other versions are untouched");
    assert!(container_running(&db.container));
    assert_eq!(
        svc.snapshot_branch(root.id, "seed").unwrap().version,
        3,
        "versions are never reused"
    );
}

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn a_reset_from_data_with_another_database_is_refused_and_a_foreign_volume_is_not_a_source() {
    let fx = Fixture::mysql();
    let svc = fx.service();
    let root = fx.root_branch(&svc);
    let branch = fx.clone_of(&svc, root.id);
    // The recorded database is part of the data a snapshot holds.
    let snapshot = svc.snapshot_branch(root.id, "seed").unwrap();
    // Replace the snapshot's volume with an empty namesake: not a source.
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
    // The branch kept its volume and its data.
    assert!(volume_exists(&branch.volume));
    assert!(container_running(branch.container_id.as_deref().unwrap()));
    assert_eq!(svc.branch(branch.id).unwrap().unwrap().generation, 1);

    // A reset keeps the URL, so data with another database is refused: a
    // second root made after `database` changed in the configuration.
    let path = fx.project.root.join("devflow.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replace("database = \"shop\"", "database = \"other\""),
    )
    .unwrap();
    let changed = Project::open(&fx.project.root).unwrap();
    let other = Service::open(&changed, "sql").unwrap();
    let other_root = other
        .create_branch(NewBranch {
            id: None,
            name: "other-root".into(),
            owner: None,
            source: SourceRef::Empty,
        })
        .unwrap();
    assert_eq!(other_root.mysql.as_ref().unwrap().database, "other");
    assert_eq!(
        branch.mysql.as_ref().unwrap().database,
        "shop",
        "an existing branch keeps the database its data has"
    );
    let error = svc
        .reset_branch(branch.id, SourceRef::Branch { id: other_root.id })
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("has database 'other'") && text.contains("names 'shop'"),
        "{text}"
    );
    assert_eq!(svc.branch(branch.id).unwrap().unwrap().generation, 1);
}

// ---- workspaces, variables, the command --------------------------------------------------

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

fn env_value(workspace: &Path, name: &str) -> String {
    devflow_core::env_file::read(workspace)
        .unwrap()
        .into_iter()
        .find(|(key, _)| key == name)
        .unwrap_or_else(|| panic!("{name} is not in .devflow/env"))
        .1
}

fn devflow(fx: &Fixture, args: &[&str]) -> std::process::Output {
    Command::new(&fx.bin)
        .arg("-C")
        .arg(&fx.project.root)
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
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

/// The `service-sql` line of `devflow doctor`: the engine, the branches and
/// the last clone. (An unrelated failing check makes `doctor` exit 1 with
/// its document all the same.)
fn doctor_line(fx: &Fixture) -> String {
    let doctor: serde_json::Value =
        serde_json::from_slice(&devflow(fx, &["--json", "doctor"]).stdout).unwrap();
    let checks = doctor["checks"].as_array().unwrap();
    assert!(checks.iter().any(|c| c["name"] == "services-cow"));
    checks
        .iter()
        .find(|c| c["name"] == "service-sql")
        .unwrap_or_else(|| panic!("no service-sql check in {doctor}"))["detail"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn workspaces_get_their_own_mysql_branch_and_variables_and_the_command_reports_them() {
    let fx = Fixture::mysql();
    let first = create(&fx, "feature-a", SetupOptions::default()).unwrap();
    assert_eq!(
        first.status,
        devflow_core::receipt::Status::Ready,
        "{:?}",
        first.receipt.phases
    );
    let binding = &first.receipt.services["sql"];
    assert_eq!(binding.engine, Engine::Mysql);
    assert!(
        binding.source.starts_with("workspace:"),
        "{}",
        binding.source
    );
    let cloned = binding.cloned.as_ref().expect("a clone report");
    assert!(cloned.method.is_some());
    assert!(cloned.source_downtime_ms.is_some(), "{cloned:?}");

    // `doctor` says the engine and what the last clone cost the source.
    let line = doctor_line(&fx);
    assert!(
        line.starts_with("mysql: ")
            && line.contains("last clone")
            && line.contains("source stopped"),
        "{line}"
    );

    // The URL and its parts, under the names a MySQL service exports.
    let path = &first.workspace.path;
    let url = env_value(path, "DATABASE_URL");
    assert!(
        url.starts_with("mysql://app:") && url.ends_with("/shop"),
        "{url}"
    );
    assert_eq!(env_value(path, "MYSQL_HOST"), "127.0.0.1");
    assert_eq!(env_value(path, "MYSQL_USER"), "app");
    assert_eq!(env_value(path, "MYSQL_DATABASE"), "shop");
    let port: u16 = env_value(path, "MYSQL_PORT").parse().unwrap();
    assert!(url.contains(&format!(":{port}/")), "{url}");
    assert_eq!(env_value(path, "MYSQL_PASSWORD").len(), 32);
    // `.devflow/env` is private.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path.join(".devflow/env"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // The branch is the workspace's own and reachable on the host port.
    let svc = fx.service();
    let (_, record) = setup::service_branch(&fx.project, &first.workspace, "sql").unwrap();
    assert_eq!(record.port, Some(port));
    assert_eq!(greeting(port)[4], 10);
    assert_eq!(Db::of(&svc, record.id).scalar("SELECT 1;"), "1");

    // `--data-from snapshot:…` per service, as for the other engines.
    let db = Db::of(&svc, record.id);
    db.scalar("CREATE TABLE t (k INT PRIMARY KEY); INSERT INTO t VALUES (42);");
    let snapshot = svc.snapshot_branch(record.id, "seed").unwrap();
    db.scalar("INSERT INTO t VALUES (43);");
    let second = create(
        &fx,
        "feature-b",
        SetupOptions {
            data_from: Some("snapshot:seed".parse::<DataSource>().unwrap()),
            ..SetupOptions::default()
        },
    )
    .unwrap();
    let (_, second_record) = setup::service_branch(&fx.project, &second.workspace, "sql").unwrap();
    assert_eq!(
        second_record.source,
        SourceRef::Snapshot { id: snapshot.id }
    );
    assert_eq!(Db::of(&svc, second_record.id).count("t"), 1);
    assert_ne!(
        env_value(&second.workspace.path, "MYSQL_PORT"),
        env_value(path, "MYSQL_PORT")
    );

    // The last clone was from a snapshot: no source was stopped for it.
    let line = doctor_line(&fx);
    assert!(line.contains("no source stopped"), "{line}");

    // The command: status, url, list, with the engine, the copy-on-write
    // result and the last clone's downtime; no password in the listings.
    let status = json_of(&devflow(&fx, &["--json", "service", "status"]));
    let service = &status["services"][0];
    assert_eq!(service["engine"], "mysql");
    assert!(["verified", "copy", "unknown"].contains(&service["cow"].as_str().unwrap()));
    assert_eq!(service["last_clone"]["operation"], "clone");
    assert!(
        service["last_clone"]["source_downtime_ms"].is_u64()
            || service["last_clone"]["source_downtime_ms"].is_null()
    );
    let text = status.to_string();
    for secret in [&record.password, &db.root_password, &second_record.password] {
        assert!(
            !text.contains(secret.as_str()),
            "a password is in the status"
        );
    }
    let listing = devflow(&fx, &["service", "status"]);
    let human = String::from_utf8_lossy(&listing.stdout).to_string();
    assert!(
        human.contains("mysql") && human.contains("last clone"),
        "{human}"
    );
    assert!(!human.contains(&record.password));
    let printed = devflow(&fx, &["service", "url", "feature-a"]);
    let printed = String::from_utf8_lossy(&printed.stdout).trim().to_string();
    assert_eq!(printed, url, "`service url` prints the password");

    // Removal drops each workspace's own branch, by recorded identity, and
    // leaves the default workspace's root.
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
    assert!(removed.warnings.is_empty(), "{:?}", removed.warnings);
    assert!(svc.branch(record.id).unwrap().is_none());
    assert!(!container_exists(record.container_id.as_deref().unwrap()));
    assert!(!volume_exists(&record.volume));
    assert!(svc.branch(second_record.id).unwrap().is_some());
}

// ---- numbers ------------------------------------------------------------------------------

/// Free kibibytes of the filesystem under the Docker volumes, as the VM
/// sees it (the same measure the clone itself reports).
fn guest_free_kib(label: &str) -> i64 {
    let volume = format!("df-measure-{}", &Uuid::new_v4().simple().to_string()[..8]);
    docker_ok(&[
        "volume",
        "create",
        "--label",
        &format!("{}={label}", docker::LABEL_TEST),
        &volume,
    ]);
    let out = docker_ok(&[
        "run",
        "--rm",
        "--network",
        "none",
        "--label",
        &format!("{}={label}", docker::LABEL_TEST),
        "-v",
        &format!("{volume}:/v"),
        "debian:stable-slim",
        "sh",
        "-c",
        "sync; df -Pk /v | awk 'NR==2 {print $4}'",
    ]);
    let _ = docker::run(&["volume", "rm", "--force", &volume]);
    out.trim().parse().expect("df output")
}

#[test]
#[ignore = "needs Docker, mysql:8.4 and debian:stable-slim"]
fn measure_a_clone_of_a_200_mb_data_directory() {
    let fx = Fixture::mysql();
    let svc = fx.service();
    let nothing = guest_free_kib(&fx.label);
    let created = Instant::now();
    let root = fx.root_branch(&svc);
    eprintln!("MEASURE root branch created in {:?}", created.elapsed());
    let db = Db::of(&svc, root.id);
    eprintln!(
        "MEASURE an empty, idle MySQL branch (initialized data directory, running server) holds \
         {} MiB of the guest's disk",
        (nothing - guest_free_kib(&fx.label)) / 1024
    );
    let filled = Instant::now();
    db.fill("big", 200_000);
    eprintln!(
        "MEASURE filled {} rows in {:?}",
        db.count("big"),
        filled.elapsed()
    );
    db.check("big");
    let table = docker_ok(&[
        "exec",
        &db.container,
        "du",
        "-sm",
        "/var/lib/mysql/shop/big.ibd",
    ]);
    let datadir = docker_ok(&["exec", &db.container, "du", "-sm", "/var/lib/mysql"]);
    let first = |text: &str| text.split_whitespace().next().unwrap_or("?").to_string();
    eprintln!(
        "MEASURE table file {} MiB; data directory {} MiB",
        first(&table),
        first(&datadir)
    );

    let free_before = guest_free_kib(&fx.label);
    let started = Instant::now();
    let clone = fx.clone_of(&svc, root.id);
    let took = started.elapsed();
    let free_after = guest_free_kib(&fx.label);
    eprintln!(
        "MEASURE clone of that directory: {took:?} end to end; source downtime {:?} ms; \
         method {:?}; the copy took {:?} bytes of the guest's disk; the whole clone (copy, \
         bootstrap, first start) took {} KiB of the guest's free space ({} MiB)",
        clone.downtime_ms,
        clone.clone,
        clone.allocated_bytes,
        free_before - free_after,
        (free_before - free_after) / 1024
    );
    let cdb = Db::of(&svc, clone.id);
    assert_eq!(cdb.count("big"), db.count("big"));
    cdb.check("big");
    assert_method(clone.clone);
    if expect_cow() {
        let allocated = clone.allocated_bytes.unwrap_or(i64::MAX);
        assert!(
            allocated < 64 * 1024 * 1024,
            "a reflink shares blocks: {allocated} bytes for a directory of {} MiB",
            first(&datadir)
        );
    }

    // A snapshot of the same data, for its timing.
    let started = Instant::now();
    let snapshot = svc.snapshot_branch(root.id, "big").unwrap();
    eprintln!(
        "MEASURE snapshot of that directory: {:?} end to end; source downtime {:?} ms; the copy \
         took {:?} bytes",
        started.elapsed(),
        snapshot.downtime_ms,
        snapshot.allocated_bytes
    );
}

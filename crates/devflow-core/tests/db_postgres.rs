//! Docker-backed tests for the Postgres branching engine.
//!
//! Run: `cargo test -p devflow-core --test db_postgres -- --ignored --test-threads=1`
//! (set `DEVFLOW_EXPECT_COW=verified` on a reflink filesystem such as OrbStack
//! to assert the CoW probe result; otherwise the measurement is only printed).
//!
//! Every test uses temporary data/config directories and a project named
//! `devflow-test-*`, and removes its own compose project and volume at the end.
//! Nothing else on the Docker engine is touched.
use devflow_core::Project;
use devflow_core::config::CowCapability;
use devflow_core::db::{BranchCredentials, CowSource, RecordStatus, ResolvedSource, Server};
use postgres::types::FromSql;
use postgres::{Client, NoTls};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use uuid::Uuid;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Removes the test's compose project and volume, whatever happens.
struct ComposeGuard {
    project: String,
    file: PathBuf,
}

impl Drop for ComposeGuard {
    fn drop(&mut self) {
        assert!(
            self.project.starts_with("devflow-devflow-test-"),
            "refusing to remove compose project {}",
            self.project
        );
        let _ = Command::new("docker")
            .args(["compose", "-p", &self.project, "-f"])
            .arg(&self.file)
            .args(["down", "-v", "--remove-orphans"])
            .output();
    }
}

struct Env {
    // Field order is drop order: remove the containers before the directories.
    _guard: ComposeGuard,
    data: tempfile::TempDir,
    _config: tempfile::TempDir,
    repo: tempfile::TempDir,
    name: String,
    id: Uuid,
    _lock: MutexGuard<'static, ()>,
}

fn write_config(repo: &std::path::Path, name: &str, id: Uuid, postgres: &str, local: Option<&str>) {
    let text = format!(
        "[project]\nname = \"{name}\"\nid = \"{id}\"\n\n[postgres]\nmode = \"managed\"\nimage = \"postgres:18\"\n{postgres}\n"
    );
    std::fs::write(repo.join("devflow.toml"), text).unwrap();
    let local_path = repo.join("devflow.local.toml");
    match local {
        Some(local) => std::fs::write(local_path, local).unwrap(),
        None => {
            let _ = std::fs::remove_file(local_path);
        }
    }
}

impl Env {
    /// `postgres` is extra lines for the `[postgres]` section of `devflow.toml`.
    fn new(postgres: &str) -> Env {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let data = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        // SAFETY: tests run one at a time under ENV_LOCK.
        unsafe {
            std::env::set_var("DEVFLOW_DATA_DIR", data.path());
            std::env::set_var("DEVFLOW_CONFIG_DIR", config.path());
            std::env::remove_var("DEVFLOW_PG_ADMIN_URL");
        }
        git2::Repository::init(repo.path()).unwrap();
        let name = format!("devflow-test-{}", &Uuid::new_v4().simple().to_string()[..6]);
        let id = Uuid::new_v4();
        write_config(repo.path(), &name, id, postgres, None);
        let project = Project::open(repo.path()).unwrap();
        let server = Server::for_project(&project)
            .unwrap()
            .expect("[postgres] section");
        let managed = server.managed().expect("managed mode");
        let guard = ComposeGuard {
            project: managed.compose_project.clone(),
            file: managed.compose_file(),
        };
        Env {
            _guard: guard,
            data,
            _config: config,
            repo,
            name,
            id,
            _lock: lock,
        }
    }

    fn write_config(&self, postgres: &str, local: Option<&str>) {
        write_config(self.repo.path(), &self.name, self.id, postgres, local);
    }

    fn project(&self) -> Project {
        Project::open(self.repo.path()).unwrap()
    }

    fn server(&self) -> Server {
        Server::for_project(&self.project())
            .unwrap()
            .expect("[postgres] section")
    }

    fn admin_json(&self) -> serde_json::Value {
        let path = self
            .data
            .path()
            .join("postgres")
            .join(self.id.to_string())
            .join("admin.json");
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }
}

// ---- helpers --------------------------------------------------------------

/// Superuser URL of the managed container (the harness's own access).
fn super_url(server: &Server, db: &str) -> String {
    let managed = server.managed().unwrap();
    format!(
        "postgres://postgres:{}@127.0.0.1:{}/{db}",
        managed.admin_password().unwrap(),
        managed.published_port().unwrap()
    )
}

fn connect(url: &str) -> Client {
    Client::connect(url, NoTls).unwrap_or_else(|e| panic!("cannot connect: {e}"))
}

fn superuser(server: &Server, db: &str) -> Client {
    connect(&super_url(server, db))
}

fn url_for(creds: &BranchCredentials, db: &str) -> String {
    format!(
        "postgres://{}:{}@{}:{}/{db}",
        creds.user, creds.password, creds.host, creds.port
    )
}

fn can_connect(url: &str) -> bool {
    Client::connect(url, NoTls).is_ok()
}

fn one<T: for<'a> FromSql<'a>>(client: &mut Client, sql: &str) -> T {
    client.query_one(sql, &[]).unwrap().get(0)
}

fn count(client: &mut Client, sql: &str) -> i64 {
    one(client, &format!("SELECT count(*) FROM ({sql}) q"))
}

/// Databases and login roles devflow created (test-side inspection only).
fn df_databases(admin: &mut Client) -> Vec<String> {
    admin
        .query(
            "SELECT datname::text FROM pg_database WHERE datname LIKE 'df\\_%' ORDER BY 1",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

fn login_roles(admin: &mut Client) -> Vec<String> {
    admin
        .query(
            "SELECT rolname::text FROM pg_roles WHERE rolname LIKE 'dfl\\_%' ORDER BY 1",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

/// A client that just disconnected may still occupy its backend for a few
/// milliseconds; statements that need an idle database retry briefly.
fn retry(mut attempt: impl FnMut() -> Result<(), postgres::Error>) {
    let mut last = None;
    for _ in 0..30 {
        match attempt() {
            Ok(()) => return,
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    panic!("statement kept failing: {}", last.unwrap());
}

/// A superuser connection to a managed server that was just started.
fn superuser_when_ready(server: &Server, db: &str) -> Client {
    for _ in 0..40 {
        if let Ok(client) = Client::connect(&super_url(server, db), NoTls) {
            return client;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    panic!("no superuser connection to the managed server");
}

/// Backend of a statement starting with `prefix` that waits on a lock, other
/// than the `exclude`d ones. Panics after a minute.
fn blocked_backend(probe: &mut Client, prefix: &str, exclude: &[i32]) -> i32 {
    for _ in 0..12_000 {
        let rows = probe
            .query(
                "SELECT pid FROM pg_stat_activity WHERE wait_event_type = 'Lock' \
                   AND query LIKE $1 AND pid <> pg_backend_pid()",
                &[&format!("{prefix}%")],
            )
            .unwrap();
        if let Some(row) = rows
            .iter()
            .find(|row| !exclude.contains(&row.get::<_, i32>(0)))
        {
            return row.get(0);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("no statement starting with {prefix:?} ever blocked");
}

/// Backend holding the session-level advisory lock `key` (of the current database's namespace).
fn advisory_holder(probe: &mut Client, key: i64) -> Option<i32> {
    probe
        .query_opt(
            "SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND granted \
               AND ((classid::bigint << 32) | objid::bigint) = $1",
            &[&key],
        )
        .unwrap()
        .map(|row| row.get(0))
}

/// `(name, marked)` of the `devflow_bootstrap_*` databases (test-side inspection only).
fn staging_databases(admin: &mut Client) -> Vec<(String, bool)> {
    admin
        .query(
            "SELECT datname::text, coalesce(shobj_description(oid, 'pg_database') = 'devflow metadata v1', false) \
             FROM pg_database WHERE datname LIKE 'devflow\\_bootstrap\\_%' ORDER BY 1",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect()
}

/// Switch the server's clone mode for new admin sessions. The managed server
/// sets `file_copy_method` on its command line, which `ALTER SYSTEM` cannot
/// override; a per-role setting does.
fn set_file_copy_method(admin: &mut Client, server: &Server, method: Option<&str>) {
    admin
        .batch_execute(&match method {
            Some(m) => format!("ALTER ROLE postgres SET file_copy_method = '{m}'"),
            None => "ALTER ROLE postgres RESET file_copy_method".to_string(),
        })
        .unwrap();
    let wanted = method.unwrap_or("clone");
    for _ in 0..100 {
        if server.status().file_copy_method.as_deref() == Some(wanted) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("file_copy_method never became {wanted}");
}

fn print_cow(env: &Env, status: &devflow_core::db::ServerStatus) {
    let cow = env.admin_json()["cow"].clone();
    println!(
        "CoW probe: capability={:?} source={:?} probe_bytes={} growth_bytes={} note={}",
        status.cow, status.cow_source, cow["probe_bytes"], cow["growth_bytes"], cow["note"]
    );
    if std::env::var("DEVFLOW_EXPECT_COW").as_deref() == Ok("verified") {
        assert_eq!(status.cow, CowCapability::Verified, "probe: {cow}");
        assert_eq!(status.cow_source, Some(CowSource::Measured));
    }
}

// ---- tests ----------------------------------------------------------------

/// Empty -> write as the login -> clone -> isolation -> snapshot -> clone from
/// snapshot -> reset keeps the URL and restores data -> drop.
#[test]
#[ignore = "needs Docker"]
fn lifecycle_isolation_snapshots_reset() {
    let env = Env::new("");
    let server = env.server();
    let status = server.up().unwrap();
    assert!(status.running && status.reachable, "{status:?}");
    assert!(status.server_version.as_deref().unwrap().starts_with("18"));
    assert_eq!(status.file_copy_method.as_deref(), Some("clone"));
    print_cow(&env, &status);
    let mut admin = superuser(&server, "postgres");
    let owner = format!("dfo_{}", &env.id.simple().to_string()[..8]);

    // Empty branch; write data as the login; objects belong to the project owner.
    let instance = Uuid::new_v4();
    let a = server
        .create_branch("a", ResolvedSource::Empty, Some(instance))
        .unwrap();
    assert_eq!(a.branch.status, RecordStatus::Ready);
    assert_eq!(a.branch.source_kind, "empty");
    assert!(a.url.starts_with("postgres://dfl_"), "{}", a.url);
    assert_eq!(a.user, a.branch.role_name);
    assert_eq!(a.database, a.branch.db_name);
    let mut ca = connect(&a.url);
    assert_eq!(one::<String>(&mut ca, "SELECT current_user::text"), owner);
    assert_eq!(one::<String>(&mut ca, "SELECT session_user::text"), a.user);
    ca.batch_execute(
        "CREATE TABLE items (id int PRIMARY KEY, name text);
         INSERT INTO items VALUES (1, 'one'), (2, 'two');
         CREATE SCHEMA app; CREATE TABLE app.t (i int);",
    )
    .unwrap();
    // RESET ROLE returns to the role from ALTER ROLE ... SET, so only
    // SET ROLE NONE produces login-owned stragglers.
    ca.batch_execute("RESET ROLE").unwrap();
    assert_eq!(one::<String>(&mut ca, "SELECT current_user::text"), owner);
    ca.batch_execute("SET ROLE NONE; CREATE TABLE straggler (i int); INSERT INTO straggler VALUES (7); RESET ROLE")
        .unwrap();
    assert_eq!(
        one::<String>(
            &mut ca,
            "SELECT tableowner::text FROM pg_tables WHERE tablename = 'straggler'"
        ),
        a.user
    );

    // The instance link and credential rotation (the crash-recovery path).
    assert_eq!(
        server.branch_for_instance(instance).unwrap().unwrap().id,
        a.branch.id
    );
    assert!(
        server
            .branch_for_instance(Uuid::new_v4())
            .unwrap()
            .is_none()
    );

    // Clone from branch A while A has an open session: the managed default
    // policy terminates it and says so.
    let b = server
        .create_branch("b", ResolvedSource::Branch(a.branch.id), None)
        .unwrap();
    assert!(
        b.warnings
            .iter()
            .any(|w| w.contains("terminated") && w.contains("aborted")),
        "{:?}",
        b.warnings
    );
    assert!(
        ca.simple_query("SELECT 1").is_err(),
        "A's session must be gone"
    );
    assert_eq!(b.branch.source_kind, "branch");
    assert_eq!(b.branch.source_id, Some(a.branch.id));
    let mut cb = connect(&b.url);
    assert_eq!(count(&mut cb, "SELECT * FROM items"), 2);
    assert_eq!(count(&mut cb, "SELECT * FROM straggler"), 1);
    // Stragglers were reassigned to the project owner in the clone only.
    assert_eq!(
        one::<String>(
            &mut cb,
            "SELECT tableowner::text FROM pg_tables WHERE tablename = 'straggler'"
        ),
        owner
    );
    cb.batch_execute("DELETE FROM items WHERE id = 2").unwrap();
    let mut ca = connect(&a.url);
    assert_eq!(
        count(&mut ca, "SELECT * FROM items"),
        2,
        "A is independent of B"
    );
    assert_eq!(
        one::<String>(
            &mut ca,
            "SELECT tableowner::text FROM pg_tables WHERE tablename = 'straggler'"
        ),
        a.user,
        "the source keeps its stragglers"
    );

    // Isolation: logins reach only their own database.
    assert!(!can_connect(&url_for(&a, &b.database)));
    assert!(!can_connect(&url_for(&b, &a.database)));
    for db in ["devflow", "postgres", "template1", "template0"] {
        assert!(!can_connect(&url_for(&a, db)), "login reached {db}");
    }
    // Role invariants: plain attributes, no admin option, owner owns no database.
    let plain = count(
        &mut admin,
        &format!(
            "SELECT 1 FROM pg_roles WHERE rolname IN ('{}', '{}', '{owner}') AND (rolsuper OR rolcreatedb \
             OR rolcreaterole OR rolreplication OR rolbypassrls)",
            a.user, b.user
        ),
    );
    assert_eq!(plain, 0);
    assert!(!one::<bool>(
        &mut admin,
        &format!("SELECT rolcanlogin FROM pg_roles WHERE rolname = '{owner}'")
    ));
    assert_eq!(
        count(
            &mut admin,
            &format!(
                "SELECT 1 FROM pg_auth_members m WHERE m.admin_option AND \
                 (m.member = to_regrole('{}') OR m.member = to_regrole('{}') OR m.member = to_regrole('{owner}'))",
                a.user, b.user
            )
        ),
        0
    );
    assert!(one::<bool>(
        &mut admin,
        &format!(
            "SELECT inherit_option AND set_option FROM pg_auth_members \
             WHERE roleid = to_regrole('{owner}') AND member = to_regrole('{}')",
            a.user
        )
    ));
    let acl: String = one(
        &mut admin,
        &format!(
            "SELECT datacl::text FROM pg_database WHERE datname = '{}'",
            a.database
        ),
    );
    assert!(
        acl.contains(&format!("{owner}=CT/")) && !acl.contains('*'),
        "owner gets CREATE, TEMPORARY and no grant option: {acl}"
    );
    assert!(
        acl.contains(&format!("{}=c/", a.user)) && acl.matches("=c/").count() == 1,
        "only the branch login has CONNECT: {acl}"
    );
    assert_eq!(
        count(
            &mut admin,
            &format!("SELECT 1 FROM pg_database WHERE datdba = to_regrole('{owner}')")
        ),
        0
    );
    assert_eq!(
        count(
            &mut admin,
            &format!("SELECT 1 FROM pg_tablespace WHERE spcowner = to_regrole('{owner}')")
        ),
        0
    );
    // A login acting as the owner (or as itself) cannot signal a sibling's session.
    let sibling_pid: i32 = one(&mut cb, "SELECT pg_backend_pid()");
    for prelude in ["", "SET ROLE NONE;"] {
        for function in ["pg_terminate_backend", "pg_cancel_backend"] {
            let attempt = ca.batch_execute(&format!(
                "{prelude} SELECT {function}({sibling_pid}); RESET ROLE"
            ));
            assert!(attempt.is_err(), "{prelude} {function} must be denied");
            ca.batch_execute("RESET ROLE").unwrap();
        }
    }
    assert!(
        cb.simple_query("SELECT 1").is_ok(),
        "the sibling session survived"
    );

    // Snapshots: monotonic versions, sealed, never templates.
    let s1 = server.create_snapshot("seed", a.branch.id).unwrap();
    assert_eq!((s1.version, s1.status), (1, RecordStatus::Ready));
    let mut ca = connect(&a.url);
    ca.batch_execute("INSERT INTO items VALUES (3, 'three')")
        .unwrap();
    let s2 = server.create_snapshot("seed", a.branch.id).unwrap();
    assert_eq!(s2.version, 2);
    assert_eq!(server.find_snapshot("seed", None).unwrap().version, 2);
    assert_eq!(server.find_snapshot("seed", Some(1)).unwrap().id, s1.id);
    server.delete_snapshot("seed", 2).unwrap();
    assert_eq!(server.find_snapshot("seed", None).unwrap().version, 1);
    let s3 = server.create_snapshot("seed", a.branch.id).unwrap();
    assert_eq!(s3.version, 3, "deleted versions are never reused");
    assert_eq!(server.list_snapshots().unwrap().len(), 2);
    assert_eq!(
        count(
            &mut admin,
            &format!(
                "SELECT 1 FROM pg_database WHERE datname = '{}' AND NOT datallowconn AND NOT datistemplate",
                s1.db_name
            )
        ),
        1
    );
    assert!(server.create_snapshot("bad@name", a.branch.id).is_err());

    // An unrelated CREATEDB role cannot clone a snapshot or a branch.
    admin
        .batch_execute("CREATE ROLE rogue LOGIN PASSWORD 'rogue' CREATEDB")
        .unwrap();
    admin
        .batch_execute("GRANT CONNECT ON DATABASE postgres TO rogue")
        .unwrap();
    let port = server.managed().unwrap().published_port().unwrap();
    let mut rogue = connect(&format!("postgres://rogue:rogue@127.0.0.1:{port}/postgres"));
    for template in [&s1.db_name, &a.database] {
        let attempt = rogue.batch_execute(&format!(
            "CREATE DATABASE stolen TEMPLATE \"{template}\" STRATEGY FILE_COPY"
        ));
        assert!(attempt.is_err(), "rogue cloned {template}");
    }
    assert!(!can_connect(&format!(
        "postgres://rogue:rogue@127.0.0.1:{port}/{}",
        s1.db_name
    )));

    // Clone from the first snapshot: the data as it was then.
    let c = server
        .create_branch("c", ResolvedSource::Snapshot(s1.id), None)
        .unwrap();
    assert_eq!(c.branch.source_kind, "snapshot");
    let mut cc = connect(&c.url);
    assert_eq!(count(&mut cc, "SELECT * FROM items"), 2);

    // Reset B to A's current data: same URL, restored data, new generation.
    cb = connect(&b.url);
    cb.batch_execute("DROP TABLE straggler; INSERT INTO items VALUES (99, 'scratch')")
        .unwrap();
    let before = server.branch(b.branch.id).unwrap().unwrap();
    assert_eq!(before.generation, 0);
    let reset = server
        .reset_branch(b.branch.id, ResolvedSource::Branch(a.branch.id))
        .unwrap();
    assert_eq!((reset.generation, reset.status), (1, RecordStatus::Ready));
    assert_eq!(reset.db_name, b.database);
    assert!(
        cb.simple_query("SELECT 1").is_err(),
        "reset drops old sessions"
    );
    let mut cb = connect(&b.url);
    assert_eq!(count(&mut cb, "SELECT * FROM items"), 3);
    assert_eq!(count(&mut cb, "SELECT * FROM straggler"), 1);
    assert_eq!(
        df_databases(&mut admin).len(),
        5,
        "A, B, C and two snapshots; no replacement or backup left"
    );
    // Reset to a snapshot restores that version; to empty clears everything.
    server
        .reset_branch(b.branch.id, ResolvedSource::Snapshot(s1.id))
        .unwrap();
    let mut cb = connect(&b.url);
    assert_eq!(count(&mut cb, "SELECT * FROM items"), 2);
    assert!(
        server
            .reset_branch(b.branch.id, ResolvedSource::Branch(b.branch.id))
            .is_err()
    );
    server
        .reset_branch(b.branch.id, ResolvedSource::Empty)
        .unwrap();
    let mut cb = connect(&b.url);
    assert_eq!(
        count(
            &mut cb,
            "SELECT tablename FROM pg_tables WHERE schemaname = 'public'"
        ),
        0
    );
    cb.batch_execute("CREATE TABLE fresh (i int)").unwrap();
    assert_eq!(
        one::<String>(
            &mut cb,
            "SELECT tableowner::text FROM pg_tables WHERE tablename = 'fresh'"
        ),
        owner
    );
    assert_eq!(server.branch(b.branch.id).unwrap().unwrap().generation, 3);
    // Provenance is immutable: resets (to a snapshot, to empty) never touch it.
    let recorded = server.branch(b.branch.id).unwrap().unwrap();
    assert_eq!(
        (recorded.source_kind.as_str(), recorded.source_id),
        ("branch", Some(a.branch.id))
    );

    // Credential rotation keeps the data and invalidates the old password.
    let rotated = server.rotate_credentials(a.branch.id).unwrap();
    assert_ne!(rotated.password, a.password);
    assert!(!can_connect(&a.url));
    assert_eq!(count(&mut connect(&rotated.url), "SELECT * FROM items"), 3);

    // list_branches reports sizes for existing databases.
    let listed = server.list_branches().unwrap();
    assert_eq!(listed.len(), 3);
    assert!(
        listed.iter().all(|b| b.size_bytes.unwrap_or(0) > 0),
        "{listed:?}"
    );

    // A reset re-clones the recorded source. Once that is gone it fails with a
    // pointer to `--to`, changes nothing, and an explicit source still works.
    let donor = server
        .create_branch("donor", ResolvedSource::Empty, None)
        .unwrap();
    let child = server
        .create_branch("child", ResolvedSource::Branch(donor.branch.id), None)
        .unwrap();
    connect(&child.url)
        .batch_execute("CREATE TABLE kept (i int)")
        .unwrap();
    server.drop_branch(donor.branch.id).unwrap();
    let recorded = server.branch(child.branch.id).unwrap().unwrap();
    assert_eq!(
        (recorded.source_kind.as_str(), recorded.source_id),
        ("branch", Some(donor.branch.id))
    );
    let error = server
        .reset_branch(child.branch.id, ResolvedSource::Branch(donor.branch.id))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("no longer exists") && error.contains("--to"),
        "{error}"
    );
    let gone = server.create_snapshot("gone", a.branch.id).unwrap();
    let from_gone = server
        .create_branch("from-gone", ResolvedSource::Snapshot(gone.id), None)
        .unwrap();
    server.delete_snapshot("gone", 1).unwrap();
    let error = server
        .reset_branch(from_gone.branch.id, ResolvedSource::Snapshot(gone.id))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("no longer exists") && error.contains("--to"),
        "{error}"
    );
    let unchanged = server.branch(child.branch.id).unwrap().unwrap();
    assert_eq!(unchanged.generation, 0);
    assert_eq!(
        count(
            &mut connect(&child.url),
            "SELECT * FROM pg_tables WHERE tablename = 'kept'"
        ),
        1
    );
    server
        .reset_branch(child.branch.id, ResolvedSource::Empty)
        .unwrap();
    let recorded = server.branch(child.branch.id).unwrap().unwrap();
    assert_eq!(
        (
            recorded.source_kind.as_str(),
            recorded.source_id,
            recorded.generation
        ),
        ("branch", Some(donor.branch.id), 1)
    );
    server.drop_branch(child.branch.id).unwrap();
    server.drop_branch(from_gone.branch.id).unwrap();

    // Drop everything.
    drop((ca, cb, cc));
    for id in [a.branch.id, b.branch.id, c.branch.id] {
        server.drop_branch(id).unwrap();
        assert!(server.branch(id).unwrap().is_none());
    }
    server.delete_snapshot("seed", 1).unwrap();
    server.delete_snapshot("seed", 3).unwrap();
    assert!(
        df_databases(&mut admin).is_empty(),
        "{:?}",
        df_databases(&mut admin)
    );
    assert!(login_roles(&mut admin).is_empty());
    assert!(server.list_branches().unwrap().is_empty());
    assert!(server.list_snapshots().unwrap().is_empty());
    server.drop_branch(a.branch.id).unwrap(); // idempotent
}

/// `fail` names the sessions and leaves them alone; `disconnect` terminates
/// them, retries, and warns.
#[test]
#[ignore = "needs Docker"]
fn busy_source_policies() {
    let env = Env::new("on_busy_source = \"fail\"");
    let server = env.server();
    server.up().unwrap();
    let mut admin = superuser(&server, "postgres");
    let a = server
        .create_branch("a", ResolvedSource::Empty, None)
        .unwrap();
    let mut held = connect(&a.url);
    held.batch_execute("CREATE TABLE t (i int)").unwrap();
    held.batch_execute("BEGIN; INSERT INTO t VALUES (1)")
        .unwrap();

    let error = server
        .create_branch("b", ResolvedSource::Branch(a.branch.id), None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("open session"), "{error}");
    let error = server
        .create_snapshot("s", a.branch.id)
        .unwrap_err()
        .to_string();
    assert!(error.contains("open session"), "{error}");
    assert!(
        held.simple_query("SELECT 1").is_ok(),
        "fail must not touch the session"
    );
    assert_eq!(
        df_databases(&mut admin).len(),
        1,
        "failed clones leave nothing behind"
    );
    assert_eq!(login_roles(&mut admin).len(), 1);
    assert_eq!(server.list_branches().unwrap().len(), 1);
    assert!(server.list_snapshots().unwrap().is_empty());

    env.write_config("on_busy_source = \"disconnect\"", None);
    let server = env.server();
    let b = server
        .create_branch("b", ResolvedSource::Branch(a.branch.id), None)
        .unwrap();
    assert!(
        b.warnings.iter().any(|w| w.contains("1 session")),
        "{:?}",
        b.warnings
    );
    assert!(held.simple_query("SELECT 1").is_err());
    let mut held = connect(&a.url);
    held.batch_execute("SELECT 1").unwrap();
    let snapshot = server.create_snapshot("s", a.branch.id).unwrap();
    assert!(
        snapshot.warnings.iter().any(|w| w.contains("terminated")),
        "{:?}",
        snapshot.warnings
    );
    assert!(held.simple_query("SELECT 1").is_err());
    // The uncommitted insert was aborted, not copied.
    assert_eq!(count(&mut connect(&b.url), "SELECT * FROM t"), 0);
}

/// `require_cow` follows the capability: measured (managed), declared
/// (external), and unknown (external, undeclared).
#[test]
#[ignore = "needs Docker"]
fn require_cow_follows_the_capability() {
    let env = Env::new("require_cow = true");
    let server = env.server();
    let status = server.up().unwrap();
    print_cow(&env, &status);
    assert_eq!(status.cow_source, Some(CowSource::Measured));
    match status.cow {
        CowCapability::Verified => {
            let a = server
                .create_branch("a", ResolvedSource::Empty, None)
                .unwrap();
            assert_eq!(a.branch.cow, CowCapability::Verified);
        }
        other => {
            let error = server
                .create_branch("a", ResolvedSource::Empty, None)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("require_cow")
                    && error.contains(&format!("{other:?}").to_lowercase()),
                "{error}"
            );
            assert!(server.list_branches().unwrap().is_empty());
        }
    }

    // External mode against the same server, as the superuser.
    let external = |cow: &str| {
        let local = format!(
            "[postgres]\nmode = \"external\"\nadmin_url = \"{}\"\nrequire_cow = true\n{cow}\n",
            super_url(&server, "postgres")
        );
        env.write_config("", Some(&local));
        env.server()
    };
    let undeclared = external("");
    let status = undeclared.status();
    assert_eq!(
        (status.cow, status.cow_source),
        (CowCapability::Unknown, None)
    );
    let error = undeclared
        .create_branch("x", ResolvedSource::Empty, None)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("require_cow") && error.contains("unknown"),
        "{error}"
    );
    let declared_copy = external("cow = \"copy\"");
    assert!(
        declared_copy
            .create_branch("x", ResolvedSource::Empty, None)
            .is_err()
    );
    let declared = external("cow = \"verified\"");
    let status = declared.status();
    assert_eq!(
        (status.cow, status.cow_source),
        (CowCapability::Verified, Some(CowSource::Declared))
    );
    let x = declared
        .create_branch("x", ResolvedSource::Empty, None)
        .unwrap();
    assert_eq!(x.branch.cow, CowCapability::Verified);
    // Declared verified still yields a source-specific downgrade only for tablespaces;
    // a plain clone keeps the declaration.
    let y = declared
        .create_branch("y", ResolvedSource::Branch(x.branch.id), None)
        .unwrap();
    assert_eq!(y.branch.cow, CowCapability::Verified);
}

/// A non-superuser `CREATEDB CREATEROLE` provisioner in external mode through
/// create / clone / snapshot / reset / drop, with post-`SET ROLE NONE`
/// stragglers, plus the clone failures for login-specific references.
#[test]
#[ignore = "needs Docker"]
fn non_superuser_provisioner_external_mode() {
    let env = Env::new("");
    let backend = env.server();
    let managed = backend.managed().unwrap();
    // Start only the container: the external server must bootstrap `devflow`
    // itself, as its own role.
    managed.start().unwrap();
    {
        let mut admin = None;
        for _ in 0..40 {
            if let Ok(c) = Client::connect(&super_url(&backend, "postgres"), NoTls) {
                admin = Some(c);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        admin
            .expect("superuser connection")
            .batch_execute("CREATE ROLE prov LOGIN PASSWORD 'provpw' CREATEDB CREATEROLE")
            .unwrap();
    }
    let port = managed.published_port().unwrap();
    env.write_config(
        "",
        Some(&format!(
            "[postgres]\nmode = \"external\"\nadmin_url = \"postgres://prov:provpw@127.0.0.1:{port}/postgres\"\non_busy_source = \"fail\"\n"
        )),
    );
    let server = env.server();
    let status = server.up().unwrap();
    assert!(status.reachable && status.problem.is_none(), "{status:?}");
    assert_eq!(
        (status.cow, status.cow_source),
        (CowCapability::Unknown, None)
    );
    let mut admin = superuser(&backend, "postgres");
    let owner = format!("dfo_{}", &env.id.simple().to_string()[..8]);
    assert_eq!(
        one::<String>(
            &mut admin,
            "SELECT pg_get_userbyid(datdba)::text FROM pg_database WHERE datname = 'devflow'"
        ),
        "prov"
    );

    // Create empty; build a payload with owner-based privileges, a policy, a
    // sequence, a trusted extension, and login-owned stragglers.
    let a = server
        .create_branch("a", ResolvedSource::Empty, None)
        .unwrap();
    assert_eq!(a.branch.cow, CowCapability::Unknown);
    let mut ca = connect(&a.url);
    ca.batch_execute(&format!(
        "CREATE EXTENSION citext;
         CREATE TABLE people (id serial PRIMARY KEY, email citext, secret text);
         INSERT INTO people (email, secret) VALUES ('a@x', 's1'), ('b@x', 's2');
         ALTER TABLE people ENABLE ROW LEVEL SECURITY;
         CREATE POLICY own ON people TO {owner} USING (true);
         ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO PUBLIC;
         CREATE SEQUENCE ticket START 100;
         SELECT nextval('ticket');
         SET ROLE NONE;
         CREATE TABLE straggler (i int);
         CREATE SEQUENCE straggler_seq;
         CREATE SCHEMA straggler_schema;
         CREATE TABLE straggler_schema.t (i int);
         RESET ROLE;"
    ))
    .unwrap();
    drop(ca); // the external default policy is `fail`

    let b = server
        .create_branch("b", ResolvedSource::Branch(a.branch.id), None)
        .unwrap();
    let mut cb = connect(&b.url);
    assert_eq!(count(&mut cb, "SELECT * FROM people"), 2);
    assert_eq!(one::<i64>(&mut cb, "SELECT nextval('ticket')"), 101);
    assert_eq!(
        count(
            &mut cb,
            "SELECT 1 FROM pg_extension WHERE extname = 'citext'"
        ),
        1
    );
    assert_eq!(
        count(&mut cb, "SELECT 1 FROM pg_policy WHERE polname = 'own'"),
        1
    );
    assert_eq!(
        count(
            &mut cb,
            &format!(
                "SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                      WHERE n.nspname IN ('public', 'straggler_schema') AND c.relkind IN ('r','S','v') \
                      AND pg_get_userbyid(c.relowner) <> '{owner}'"
            )
        ),
        0,
        "every object in the clone belongs to the project owner"
    );
    assert_eq!(
        one::<String>(
            &mut cb,
            "SELECT pg_get_userbyid(nspowner)::text FROM pg_namespace WHERE nspname = 'straggler_schema'"
        ),
        owner
    );
    cb.batch_execute("CREATE TABLE people2 (i int)").unwrap();
    drop(cb);

    // Snapshot, clone from it, reset, rotate.
    let s = server.create_snapshot("seed", a.branch.id).unwrap();
    let c = server
        .create_branch("c", ResolvedSource::Snapshot(s.id), None)
        .unwrap();
    assert_eq!(count(&mut connect(&c.url), "SELECT * FROM people"), 2);
    let mut cc = connect(&c.url);
    cc.batch_execute("DELETE FROM people").unwrap();
    drop(cc);
    server
        .reset_branch(c.branch.id, ResolvedSource::Snapshot(s.id))
        .unwrap();
    assert_eq!(count(&mut connect(&c.url), "SELECT * FROM people"), 2);
    server
        .reset_branch(c.branch.id, ResolvedSource::Branch(a.branch.id))
        .unwrap();
    assert_eq!(count(&mut connect(&c.url), "SELECT * FROM people"), 2);
    assert!(server.rotate_credentials(c.branch.id).is_ok());
    assert!(!can_connect(&c.url));

    // Isolation as seen from the non-superuser setup.
    assert!(!can_connect(&url_for(&a, &b.database)));
    assert!(!can_connect(&url_for(&a, &s.db_name)));
    assert!(!can_connect(&url_for(&a, "devflow")));
    assert!(
        can_connect(&url_for(&a, "postgres")),
        "an external server's other databases are never altered"
    );

    // Login-specific references cannot be translated: the clone fails cleanly.
    let cases = [
        (
            "SET ROLE NONE; ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO PUBLIC".to_string(),
            "SET ROLE NONE; ALTER DEFAULT PRIVILEGES IN SCHEMA public REVOKE SELECT ON TABLES FROM PUBLIC".to_string(),
        ),
        (
            format!("GRANT SELECT ON people TO {}", a.user),
            format!("REVOKE SELECT ON people FROM {}", a.user),
        ),
        (
            format!("CREATE POLICY mine ON people TO {} USING (true)", a.user),
            "DROP POLICY mine ON people".to_string(),
        ),
    ];
    for (setup, cleanup) in cases {
        connect(&a.url).batch_execute(&setup).unwrap();
        let databases = df_databases(&mut admin);
        let roles = login_roles(&mut admin);
        let error = server
            .create_branch("bad", ResolvedSource::Branch(a.branch.id), None)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("login-specific privileges/policies") && error.contains(&a.user),
            "{setup}: {error}"
        );
        assert_eq!(
            df_databases(&mut admin),
            databases,
            "the half-made database is gone"
        );
        assert_eq!(login_roles(&mut admin), roles);
        assert_eq!(
            server.list_branches().unwrap().len(),
            3,
            "the failed row is gone"
        );
        let error = server
            .create_snapshot("bad", a.branch.id)
            .unwrap_err()
            .to_string();
        assert!(error.contains("login-specific"), "{error}");
        assert_eq!(server.list_snapshots().unwrap().len(), 1);
        connect(&a.url).batch_execute(&cleanup).unwrap();
    }
    server
        .create_branch("fine", ResolvedSource::Branch(a.branch.id), None)
        .unwrap();

    // A session the provisioner is not allowed to terminate is reported.
    env.write_config(
        "",
        Some(&format!(
            "[postgres]\nmode = \"external\"\nadmin_url = \"postgres://prov:provpw@127.0.0.1:{port}/postgres\"\non_busy_source = \"disconnect\"\n"
        )),
    );
    let server = env.server();
    let mut su = superuser(&backend, &a.database);
    su.batch_execute("SELECT 1").unwrap();
    let error = server
        .create_branch("blocked", ResolvedSource::Branch(a.branch.id), None)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("could not terminate") && error.contains("postgres"),
        "{error}"
    );
    drop(su);
    assert!(
        server
            .create_branch("unblocked", ResolvedSource::Branch(a.branch.id), None)
            .is_ok()
    );

    // Drop everything; all databases were owned by the provisioner.
    assert_eq!(
        count(
            &mut admin,
            "SELECT 1 FROM pg_database WHERE datname LIKE 'df\\_%' AND pg_get_userbyid(datdba) <> 'prov'"
        ),
        0
    );
    for branch in server.list_branches().unwrap() {
        server.drop_branch(branch.id).unwrap();
    }
    server.delete_snapshot("seed", 1).unwrap();
    assert!(df_databases(&mut admin).is_empty());
    assert!(login_roles(&mut admin).is_empty());
}

/// Rows and databases a crash would leave behind, then `reconcile`.
#[test]
#[ignore = "needs Docker"]
fn reconcile_after_simulated_crashes() {
    let env = Env::new("");
    let server = env.server();
    server.up().unwrap();
    let mut admin = superuser(&server, "postgres");
    let mut meta = superuser(&server, "devflow");
    let project = env.id;
    let owner = format!("dfo_{}", &env.id.simple().to_string()[..8]);
    let hex = |id: Uuid, n: usize| id.simple().to_string()[..n].to_string();
    let db_of = |id: Uuid| format!("df_{}_{}", hex(project, 8), hex(id, 12));

    let a = server
        .create_branch("a", ResolvedSource::Empty, None)
        .unwrap();
    let mut ca = connect(&a.url);
    ca.batch_execute("CREATE TABLE t (i int); INSERT INTO t VALUES (1)")
        .unwrap();
    drop(ca);
    let baseline = df_databases(&mut admin);

    // 1. A creation that died after CREATE DATABASE and CREATE ROLE.
    let crashed = Uuid::new_v4();
    let crashed_db = db_of(crashed);
    let crashed_login = format!("dfl_{}", hex(crashed, 12));
    admin
        .batch_execute(&format!("CREATE ROLE {crashed_login} LOGIN"))
        .unwrap();
    admin
        .batch_execute(&format!("CREATE DATABASE {crashed_db}"))
        .unwrap();
    meta.execute(
        "INSERT INTO branches (id, project_id, name, db_name, role_name, source_kind, status) \
         VALUES ($1, $2, 'crashed', $3, $4, 'empty', 'creating')",
        &[&crashed, &project, &crashed_db, &crashed_login],
    )
    .unwrap();
    // A normalization connection the dead creation left inside the half-made
    // database: dropping the database must end it.
    let mut lingering = superuser(&server, &crashed_db);
    lingering.batch_execute("SELECT 1").unwrap();
    // 2. A deletion that died before DROP DATABASE.
    let dying = Uuid::new_v4();
    let dying_db = db_of(dying);
    let dying_login = format!("dfl_{}", hex(dying, 12));
    admin
        .batch_execute(&format!("CREATE ROLE {dying_login} LOGIN"))
        .unwrap();
    admin
        .batch_execute(&format!("CREATE DATABASE {dying_db}"))
        .unwrap();
    meta.execute(
        "INSERT INTO branches (id, project_id, name, db_name, role_name, source_kind, status) \
         VALUES ($1, $2, 'dying', $3, $4, 'empty', 'deleting')",
        &[&dying, &project, &dying_db, &dying_login],
    )
    .unwrap();
    // 3. A snapshot that died before sealing.
    let half = Uuid::new_v4();
    let half_db = db_of(half);
    admin
        .batch_execute(&format!("CREATE DATABASE {half_db}"))
        .unwrap();
    meta.execute(
        "INSERT INTO snapshot_names (project_id, name, last_version) VALUES ($1, 'half', 1)",
        &[&project],
    )
    .unwrap();
    meta.execute(
        "INSERT INTO snapshots (id, project_id, name, version, db_name, status) \
         VALUES ($1, $2, 'half', 1, $3, 'creating')",
        &[&half, &project, &half_db],
    )
    .unwrap();

    let report = server.reconcile().unwrap();
    assert_eq!(report.actions.len(), 3, "{report:?}");
    assert!(report.problems.is_empty(), "{report:?}");
    assert!(
        lingering.simple_query("SELECT 1").is_err(),
        "DROP DATABASE … FORCE ends sessions inside a rolled-back database"
    );
    assert_eq!(df_databases(&mut admin), baseline);
    assert_eq!(login_roles(&mut admin), vec![a.user.clone()]);
    assert_eq!(server.list_branches().unwrap().len(), 1);
    assert!(server.list_snapshots().unwrap().is_empty());
    // The burned version is never reused.
    let next = server.create_snapshot("half", a.branch.id).unwrap();
    assert_eq!(next.version, 2);
    server.delete_snapshot("half", 2).unwrap();
    assert!(server.reconcile().unwrap().actions.is_empty(), "converged");

    // 3b. A creation that died before the credentials were delivered: setup
    // finds it by instance; it is rolled back, never completed.
    let instance = Uuid::new_v4();
    let pending = Uuid::new_v4();
    meta.execute(
        "INSERT INTO branches (id, project_id, name, db_name, role_name, source_kind, instance_id, status) \
         VALUES ($1, $2, 'pending', $3, $4, 'empty', $5, 'creating')",
        &[&pending, &project, &db_of(pending), &format!("dfl_{}", hex(pending, 12)), &instance],
    )
    .unwrap();
    let found = server.branch_for_instance(instance).unwrap().unwrap();
    assert_eq!((found.id, found.status), (pending, RecordStatus::Creating));
    let error = server.rotate_credentials(pending).unwrap_err().to_string();
    assert!(error.contains("rolled back"), "{error}");
    assert!(server.branch_for_instance(instance).unwrap().is_none());

    // 4. A reset that died while staging.
    let replacement = db_of(Uuid::new_v4());
    let backup = db_of(Uuid::new_v4());
    retry(|| {
        admin.batch_execute(&format!(
            "CREATE DATABASE {replacement} TEMPLATE {}",
            a.database
        ))
    });
    meta.execute(
        "UPDATE branches SET op = 'reset', op_phase = 'staging', replacement_db = $2, backup_db = $3 WHERE id = $1",
        &[&a.branch.id, &replacement, &backup],
    )
    .unwrap();
    let report = server.reconcile().unwrap();
    assert_eq!(report.actions.len(), 1, "{report:?}");
    assert_eq!(
        count(
            &mut admin,
            &format!("SELECT 1 FROM pg_database WHERE datname = '{replacement}'")
        ),
        0
    );
    assert_eq!(count(&mut connect(&a.url), "SELECT * FROM t"), 1);

    // 5. A reset that died in cutover before the rename transaction committed.
    retry(|| {
        admin.batch_execute(&format!(
            "CREATE DATABASE {replacement} TEMPLATE {}",
            a.database
        ))
    });
    admin
        .batch_execute(&format!(
            "ALTER DATABASE {} ALLOW_CONNECTIONS false",
            a.database
        ))
        .unwrap();
    meta.execute(
        "UPDATE branches SET op = 'reset', op_phase = 'cutover', replacement_db = $2, backup_db = $3 WHERE id = $1",
        &[&a.branch.id, &replacement, &backup],
    )
    .unwrap();
    assert!(!can_connect(&a.url));
    let report = server.reconcile().unwrap();
    assert_eq!(report.actions.len(), 1, "{report:?}");
    assert_eq!(
        count(&mut connect(&a.url), "SELECT * FROM t"),
        1,
        "the original is open again"
    );
    assert_eq!(
        count(
            &mut admin,
            &format!("SELECT 1 FROM pg_database WHERE datname = '{replacement}'")
        ),
        0
    );

    // 6. A reset that died after the rename transaction committed.
    retry(|| {
        admin.batch_execute(&format!(
            "CREATE DATABASE {replacement} TEMPLATE {}",
            a.database
        ))
    });
    superuser(&server, &replacement)
        .batch_execute("CREATE TABLE marker (i int)")
        .unwrap();
    // As a staged replacement would be: no PUBLIC access, no CONNECT for the login.
    admin
        .batch_execute(&format!(
            "REVOKE CONNECT, TEMPORARY ON DATABASE {replacement} FROM PUBLIC; \
             GRANT CREATE, TEMPORARY ON DATABASE {replacement} TO {owner}"
        ))
        .unwrap();
    admin
        .batch_execute(&format!(
            "ALTER DATABASE {replacement} ALLOW_CONNECTIONS false"
        ))
        .unwrap();
    admin
        .batch_execute(&format!(
            "ALTER DATABASE {} ALLOW_CONNECTIONS false",
            a.database
        ))
        .unwrap();
    meta.execute(
        "UPDATE branches SET op = 'reset', op_phase = 'cutover', replacement_db = $2, backup_db = $3 WHERE id = $1",
        &[&a.branch.id, &replacement, &backup],
    )
    .unwrap();
    let rename = format!(
        "BEGIN; ALTER DATABASE {orig} RENAME TO {backup}; ALTER DATABASE {replacement} RENAME TO {orig}; \
         UPDATE branches SET op_phase = 'cleanup', generation = generation + 1 WHERE id = '{id}'; COMMIT",
        orig = a.database,
        id = a.branch.id
    );
    retry(|| {
        let result = meta.batch_execute(&rename);
        if result.is_err() {
            let _ = meta.batch_execute("ROLLBACK");
        }
        result
    });
    let report = server.reconcile().unwrap();
    assert_eq!(report.actions.len(), 1, "{report:?}");
    let mut ca = connect(&a.url); // CONNECT was re-granted on the new database
    assert_eq!(
        count(
            &mut ca,
            "SELECT 1 FROM pg_tables WHERE tablename = 'marker'"
        ),
        1
    );
    drop(ca);
    assert_eq!(
        count(
            &mut admin,
            &format!("SELECT 1 FROM pg_database WHERE datname = '{backup}'")
        ),
        0
    );
    let info = server.branch(a.branch.id).unwrap().unwrap();
    assert_eq!(info.generation, 1);

    // 7. A mixed state is left alone and reported (never guessed at).
    admin
        .batch_execute(&format!("ALTER DATABASE {} RENAME TO {backup}", a.database))
        .unwrap();
    meta.execute(
        "UPDATE branches SET op = 'reset', op_phase = 'cutover', replacement_db = $2, backup_db = $3 WHERE id = $1",
        &[&a.branch.id, &replacement, &backup],
    )
    .unwrap();
    let report = server.reconcile().unwrap();
    assert!(
        report.actions.is_empty() && report.problems.len() == 1,
        "{report:?}"
    );
    assert!(report.problems[0].contains("inconsistent"), "{report:?}");
    assert_eq!(
        count(
            &mut admin,
            &format!("SELECT 1 FROM pg_database WHERE datname = '{backup}'")
        ),
        1
    );
    let info = server.branch(a.branch.id).unwrap().unwrap();
    assert!(
        info.error.as_deref().unwrap_or("").contains("inconsistent"),
        "{info:?}"
    );
    assert!(
        server.drop_branch(a.branch.id).is_err(),
        "no drop while a reset is unresolved"
    );
    assert!(
        server
            .create_branch("x", ResolvedSource::Empty, None)
            .is_ok(),
        "other work continues"
    );
    // An operator repairs it.
    admin
        .batch_execute(&format!("ALTER DATABASE {backup} RENAME TO {}", a.database))
        .unwrap();
    meta.execute(
        "UPDATE branches SET op = NULL, op_phase = NULL, replacement_db = NULL, backup_db = NULL, error = NULL WHERE id = $1",
        &[&a.branch.id],
    )
    .unwrap();
    assert!(server.reconcile().unwrap().problems.is_empty());
}

/// A login referenced from a sibling's database cannot be dropped: it is
/// disabled and retried by reconcile.
#[test]
#[ignore = "needs Docker"]
fn retired_roles_are_retried() {
    let env = Env::new("");
    let server = env.server();
    server.up().unwrap();
    let mut admin = superuser(&server, "postgres");
    let mut meta = superuser(&server, "devflow");
    let x = server
        .create_branch("x", ResolvedSource::Empty, None)
        .unwrap();
    let y = server
        .create_branch("y", ResolvedSource::Empty, None)
        .unwrap();
    connect(&y.url)
        .batch_execute(&format!(
            "CREATE TABLE t (i int); GRANT SELECT ON t TO {}",
            x.user
        ))
        .unwrap();

    server.drop_branch(x.branch.id).unwrap();
    let mut both = vec![x.user.clone(), y.user.clone()];
    both.sort();
    assert_eq!(login_roles(&mut admin), both);
    let (can_login, no_password): (bool, bool) = {
        let row = admin
            .query_one(
                "SELECT rolcanlogin, rolpassword IS NULL FROM pg_authid WHERE rolname = $1",
                &[&x.user],
            )
            .unwrap();
        (row.get(0), row.get(1))
    };
    assert!(!can_login && no_password, "retired roles are disabled");
    assert_eq!(count(&mut meta, "SELECT 1 FROM retired_roles"), 1);
    assert!(!can_connect(&x.url));
    let report = server.reconcile().unwrap();
    assert!(report.actions.is_empty(), "still referenced: {report:?}");

    server.drop_branch(y.branch.id).unwrap();
    let report = server.reconcile().unwrap();
    assert!(
        report.actions.iter().any(|a| a.contains("retired role")),
        "{report:?}"
    );
    assert!(login_roles(&mut admin).is_empty());
    assert_eq!(count(&mut meta, "SELECT 1 FROM retired_roles"), 0);
}

/// Two clients on one project are serialized by the advisory lock, and a held
/// lock blocks mutations until it is released.
#[test]
#[ignore = "needs Docker"]
fn concurrent_clients_share_one_project_lock() {
    let env = Env::new("");
    let server = env.server();
    server.up().unwrap();
    let mut admin = superuser(&server, "postgres");
    let a = server
        .create_branch("a", ResolvedSource::Empty, None)
        .unwrap();
    connect(&a.url)
        .batch_execute("CREATE TABLE t AS SELECT generate_series(1, 1000) i")
        .unwrap();

    let second = env.server();
    let (first, second_ref) = (&server, &second);
    let (first_branches, second_branches) = std::thread::scope(|scope| {
        let from = a.branch.id;
        let one = scope.spawn(move || {
            (0..2)
                .map(|i| {
                    first
                        .create_branch(&format!("one-{i}"), ResolvedSource::Branch(from), None)
                        .unwrap()
                })
                .collect::<Vec<_>>()
        });
        let two = scope.spawn(move || {
            (0..2)
                .map(|i| {
                    second_ref
                        .create_branch(&format!("two-{i}"), ResolvedSource::Branch(from), None)
                        .unwrap()
                })
                .collect::<Vec<_>>()
        });
        (one.join().unwrap(), two.join().unwrap())
    });
    let all: Vec<_> = first_branches.iter().chain(&second_branches).collect();
    assert_eq!(all.len(), 4);
    for branch in &all {
        assert_eq!(count(&mut connect(&branch.url), "SELECT * FROM t"), 1000);
    }
    assert_eq!(df_databases(&mut admin).len(), 5);

    // Hold the project lock the way a peer process would.
    let key = i64::from_be_bytes(env.id.as_bytes()[..8].try_into().unwrap());
    let mut holder = superuser(&server, "devflow");
    holder
        .query("SELECT pg_advisory_lock($1)", &[&key])
        .unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            sender
                .send(
                    server
                        .create_branch("late", ResolvedSource::Empty, None)
                        .map(|c| c.branch.id),
                )
                .unwrap();
        });
        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(1500))
                .is_err(),
            "the mutation must wait for the lock"
        );
        holder
            .query("SELECT pg_advisory_unlock($1)", &[&key])
            .unwrap();
        let id = receiver
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("mutation resumes")
            .unwrap();
        assert!(server.branch(id).unwrap().is_some());
    });
}

/// `down` keeps the volume, `up` re-measures CoW for the new postmaster, the
/// password in admin.json never changes, and reads work without creating state.
#[test]
#[ignore = "needs Docker"]
fn managed_server_restart_keeps_data_and_remeasures_cow() {
    let env = Env::new("");
    let server = env.server();
    // Never started: status reports it, reads create nothing.
    let status = server.status();
    assert!(!status.running && status.problem.is_some(), "{status:?}");
    assert!(
        server.list_branches().is_err(),
        "nothing to read before the first start"
    );

    let status = server.up().unwrap();
    print_cow(&env, &status);
    let first = env.admin_json();
    let password = first["password"].as_str().unwrap().to_string();
    let a = server
        .create_branch("a", ResolvedSource::Empty, None)
        .unwrap();
    assert_eq!(a.branch.cow, status.cow);
    connect(&a.url)
        .batch_execute("CREATE TABLE t (i int); INSERT INTO t VALUES (42)")
        .unwrap();

    // A second `up` reuses the cached probe.
    server.up().unwrap();
    assert_eq!(
        env.admin_json()["cow"]["measured_at"],
        first["cow"]["measured_at"]
    );

    server.down().unwrap();
    let status = server.status();
    assert!(
        !status.running && !status.reachable && status.problem.is_some(),
        "{status:?}"
    );
    assert!(server.list_branches().is_err());
    assert!(
        server
            .create_branch("b", ResolvedSource::Empty, None)
            .is_err()
    );

    let status = server.up().unwrap();
    assert!(status.running && status.reachable);
    let second = env.admin_json();
    assert_eq!(
        second["password"], first["password"],
        "the password is never regenerated"
    );
    assert_eq!(second["password"].as_str().unwrap(), password);
    assert_ne!(
        second["cow"]["measured_at"], first["cow"]["measured_at"],
        "new postmaster, new probe"
    );
    assert_eq!(
        format!("{:?}", status.cow).to_lowercase(),
        second["cow"]["capability"].as_str().unwrap()
    );
    // The ephemeral port may have changed: URLs are rebuilt from the endpoint.
    let (host, port) = server.app_endpoint().unwrap();
    let url = format!(
        "postgres://{}:{}@{host}:{port}/{}",
        a.user, a.password, a.database
    );
    assert_eq!(one::<i32>(&mut connect(&url), "SELECT i FROM t"), 42);
    assert_eq!(server.list_branches().unwrap().len(), 1);
    // The compose project carries the contract labels and no obsolete version key.
    let compose = std::fs::read_to_string(server.managed().unwrap().compose_file()).unwrap();
    assert!(
        compose.contains("dev.devflow.project")
            && !compose.lines().any(|l| l.starts_with("version"))
    );
}

/// An older server image is refused with an actionable error.
#[test]
#[ignore = "needs Docker"]
fn older_postgres_is_refused() {
    let env = Env::new("");
    env.write_config("", Some("[postgres]\nimage = \"postgres:17\"\n"));
    let server = env.server();
    let error = format!("{:#}", server.up().unwrap_err());
    println!("postgres:17 -> {error}");
    assert!(
        error.contains("file_copy_method") || error.contains("PostgreSQL 18"),
        "{error}"
    );
}

/// The CoW verification covers `$PGDATA`'s filesystem only: a source with
/// objects in another tablespace yields an `unknown` branch.
#[test]
#[ignore = "needs Docker"]
fn sources_with_other_tablespaces_report_unknown_cow() {
    let env = Env::new("");
    let server = env.server();
    let status = server.up().unwrap();
    let managed = server.managed().unwrap();
    let exec = |script: &str| {
        let output = Command::new("docker")
            .args(["compose", "-p", &managed.compose_project, "-f"])
            .arg(managed.compose_file())
            .args(["exec", "-T", "pg", "sh", "-c", script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    exec("mkdir -p /tmp/ts && chown postgres /tmp/ts");
    let owner = format!("dfo_{}", &env.id.simple().to_string()[..8]);
    let mut admin = superuser(&server, "postgres");
    admin
        .batch_execute("CREATE TABLESPACE other LOCATION '/tmp/ts'")
        .unwrap();
    admin
        .batch_execute(&format!("GRANT CREATE ON TABLESPACE other TO {owner}"))
        .unwrap();

    let plain = server
        .create_branch("plain", ResolvedSource::Empty, None)
        .unwrap();
    assert_eq!(plain.branch.cow, status.cow);
    let a = server
        .create_branch("a", ResolvedSource::Empty, None)
        .unwrap();
    connect(&a.url)
        .batch_execute(
            "CREATE TABLE elsewhere (i int) TABLESPACE other; INSERT INTO elsewhere VALUES (1)",
        )
        .unwrap();
    let plain_clone = server
        .create_branch("pc", ResolvedSource::Branch(plain.branch.id), None)
        .unwrap();
    assert_eq!(plain_clone.branch.cow, status.cow);
    let b = server
        .create_branch("b", ResolvedSource::Branch(a.branch.id), None)
        .unwrap();
    let expected = if status.cow == CowCapability::Verified {
        CowCapability::Unknown
    } else {
        status.cow
    };
    assert_eq!(b.branch.cow, expected);
    assert_eq!(count(&mut connect(&b.url), "SELECT * FROM elsewhere"), 1);
    // A reset from the same source gets the same capability.
    let reset = server
        .reset_branch(plain_clone.branch.id, ResolvedSource::Branch(a.branch.id))
        .unwrap();
    assert_eq!(reset.cow, expected);
    assert_eq!(
        server.branch(plain_clone.branch.id).unwrap().unwrap().cow,
        expected
    );
    let reset = server
        .reset_branch(
            plain_clone.branch.id,
            ResolvedSource::Branch(plain.branch.id),
        )
        .unwrap();
    assert_eq!(reset.cow, status.cow);
    if status.cow == CowCapability::Verified {
        env.write_config("require_cow = true", None);
        let strict = env.server();
        let error = strict
            .reset_branch(plain_clone.branch.id, ResolvedSource::Branch(a.branch.id))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("require_cow") && error.contains("unknown"),
            "{error}"
        );
        assert_eq!(
            strict
                .branch(plain_clone.branch.id)
                .unwrap()
                .unwrap()
                .generation,
            2,
            "the refused reset changed nothing"
        );
        let error = strict
            .create_branch("c", ResolvedSource::Branch(a.branch.id), None)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("require_cow") && error.contains("unknown"),
            "{error}"
        );
        assert!(
            strict
                .create_branch("c", ResolvedSource::Branch(plain.branch.id), None)
                .is_ok()
        );
    }
}

/// Killing the lock connection mid-operation stops the operation at its next
/// step, whether it was running DDL or a metadata write; `reconcile` then
/// converges without touching anything else.
#[test]
#[ignore = "needs Docker"]
fn losing_the_project_lock_stops_the_operation_and_reconcile_converges() {
    let env = Env::new("");
    let server = env.server();
    server.up().unwrap();
    let mut admin = superuser(&server, "postgres");
    let a = server
        .create_branch("a", ResolvedSource::Empty, None)
        .unwrap();
    connect(&a.url)
        .batch_execute("CREATE TABLE t AS SELECT generate_series(1, 300000) i")
        .unwrap();
    let keep = server
        .create_branch("keep", ResolvedSource::Empty, None)
        .unwrap();
    let key = i64::from_be_bytes(env.id.as_bytes()[..8].try_into().unwrap());
    let mut killed = 0;
    for delay_ms in [0u64, 15, 60, 150, 400] {
        let mut killer = superuser(&server, "devflow");
        let outcome = std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                server.create_branch("victim", ResolvedSource::Branch(a.branch.id), None)
            });
            // Wait for the operation to take the lock, then cut its connection.
            for _ in 0..4000 {
                let row = killer
                    .query_opt(
                        "SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND granted \
                           AND ((classid::bigint << 32) | objid::bigint) = $1",
                        &[&key],
                    )
                    .unwrap();
                if let Some(row) = row {
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    let pid: i32 = row.get(0);
                    killer
                        .query("SELECT pg_terminate_backend($1)", &[&pid])
                        .unwrap();
                    killed += 1;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            worker.join().unwrap()
        });
        println!(
            "lock killed after {delay_ms} ms -> {}",
            match &outcome {
                Ok(_) => "operation had already finished".to_string(),
                Err(error) => format!("{error:#}"),
            }
        );
        if let Err(error) = &outcome {
            assert!(
                error
                    .to_string()
                    .contains("project lock connection was lost"),
                "{error:#}"
            );
        }
        let report = server.reconcile().unwrap();
        assert!(report.problems.is_empty(), "{report:?}");
        assert!(server.reconcile().unwrap().actions.is_empty(), "converged");
        let branches = server.list_branches().unwrap();
        let victims = branches.iter().filter(|b| b.name == "victim").count();
        assert_eq!(victims, usize::from(outcome.is_ok()), "{branches:?}");
        assert_eq!(df_databases(&mut admin).len(), branches.len());
        assert_eq!(login_roles(&mut admin).len(), branches.len());
        for branch in branches.into_iter().filter(|b| b.name == "victim") {
            server.drop_branch(branch.id).unwrap();
        }
    }
    assert!(killed > 0);

    // A statement stuck in the catalog when the lock is lost must die with the
    // lock session instead of finishing behind the back of the reconcile that
    // takes over (which would leave a database no row knows about).
    let mut blocker = superuser(&server, "postgres");
    blocker
        .batch_execute("BEGIN; LOCK TABLE pg_catalog.pg_database IN SHARE ROW EXCLUSIVE MODE")
        .unwrap();
    let mut killer = superuser(&server, "devflow");
    let (outcome, reconciled, early, pids) = std::thread::scope(|scope| {
        let worker = scope.spawn(|| server.create_branch("blocked", ResolvedSource::Empty, None));
        // Wait for CREATE DATABASE to block on the catalog lock; remember which
        // session runs it and which one holds the project lock.
        let mut pids = None;
        for _ in 0..4000 {
            let statement = killer
                .query_opt(
                    "SELECT pid FROM pg_stat_activity WHERE wait_event_type = 'Lock' \
                       AND query LIKE 'CREATE DATABASE%' AND pid <> pg_backend_pid()",
                    &[],
                )
                .unwrap();
            if let Some(statement) = statement {
                let holder = killer
                    .query_opt(
                        "SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND granted \
                           AND ((classid::bigint << 32) | objid::bigint) = $1",
                        &[&key],
                    )
                    .unwrap();
                pids = Some((
                    statement.get::<_, i32>(0),
                    holder.map(|r| r.get::<_, i32>(0)),
                ));
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Reconcile starts waiting for the project lock; then the lock session
        // is cut and reconcile runs while the original CREATE is still blocked.
        let reconciler = scope.spawn(|| server.reconcile());
        std::thread::sleep(std::time::Duration::from_millis(400));
        if let Some((_, Some(holder))) = pids {
            killer
                .query("SELECT pg_terminate_backend($1)", &[&holder])
                .unwrap();
        }
        let mut early = false;
        for _ in 0..100 {
            if reconciler.is_finished() {
                early = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        blocker.batch_execute("ROLLBACK").unwrap();
        (
            worker.join().unwrap(),
            reconciler.join().unwrap(),
            early,
            pids,
        )
    });
    let (statement, holder) = pids.expect("CREATE DATABASE never blocked");
    assert_eq!(
        Some(statement),
        holder,
        "cluster DDL must run on the session that holds the project lock"
    );
    println!("blocked create: {outcome:?}; reconcile finished before the release: {early}");
    let error = outcome.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("project lock connection was lost"),
        "{error:#}"
    );
    let report = reconciled.unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(report.actions.len(), 1, "{report:?}");
    let report = server.reconcile().unwrap();
    assert!(
        report.actions.is_empty() && report.problems.is_empty(),
        "{report:?}"
    );
    let branches = server.list_branches().unwrap();
    assert!(branches.iter().all(|b| b.name != "blocked"), "{branches:?}");
    let mut expected: Vec<String> = branches.iter().map(|b| b.db_name.clone()).collect();
    expected.sort();
    let mut databases = df_databases(&mut admin);
    databases.sort();
    assert_eq!(
        databases, expected,
        "no database without a row, no row without a database"
    );
    assert_eq!(login_roles(&mut admin).len(), branches.len());
    assert!(
        server.branch(keep.branch.id).unwrap().is_some(),
        "unrelated branches are untouched"
    );

    // Metadata writes follow the same rule. A reset loses its lock while its
    // final journal write is stalled in the metadata database; a successor then
    // opens its own journal. The stalled write must have died with the lock
    // session, instead of landing on the successor's journal when released.
    let seed = server
        .create_branch("seed", ResolvedSource::Empty, None)
        .unwrap();
    let r = server
        .create_branch("r", ResolvedSource::Branch(seed.branch.id), None)
        .unwrap();
    connect(&r.url)
        .batch_execute("CREATE TABLE marker (i int)")
        .unwrap();
    let source = ResolvedSource::Branch(seed.branch.id);
    let mut meta = superuser(&server, "devflow");
    // A statement-level trigger stalls the first write that finds a journal in
    // the named phase, on an advisory lock the test holds, before the write has
    // locked any row (a sequence survives the aborted transaction, so only that
    // first write stalls). `stall_cutover` is armed later: creating a trigger
    // would wait for a stalled write.
    meta.batch_execute(
        "CREATE SEQUENCE stall_final; CREATE SEQUENCE stall_cutover MINVALUE 0 START 5; \
         CREATE FUNCTION df_test_stall() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN \
           IF EXISTS (SELECT 1 FROM branches WHERE op = 'reset' AND op_phase = TG_ARGV[2]) THEN \
             IF nextval(TG_ARGV[0]::regclass) = 1 THEN \
               PERFORM pg_advisory_xact_lock(TG_ARGV[1]::bigint); \
             END IF; \
           END IF; \
           RETURN NULL; \
         END $$; \
         CREATE TRIGGER stall_final BEFORE UPDATE ON branches FOR EACH STATEMENT \
           EXECUTE FUNCTION df_test_stall('stall_final', '4201', 'cleanup'); \
         CREATE TRIGGER stall_cutover BEFORE UPDATE ON branches FOR EACH STATEMENT \
           EXECUTE FUNCTION df_test_stall('stall_cutover', '4202', 'staging')",
    )
    .unwrap();
    let journal = |meta: &mut Client| -> (Option<String>, Option<String>, Option<String>) {
        let row = meta
            .query_one(
                "SELECT op, op_phase, replacement_db FROM branches WHERE id = $1",
                &[&r.branch.id],
            )
            .unwrap();
        (row.get(0), row.get(1), row.get(2))
    };
    let mut probe = superuser(&server, "devflow");
    let observed = std::thread::scope(|scope| {
        // Declared in here so that a failing assertion releases the stalls.
        let mut gate = superuser(&server, "devflow");
        gate.query("SELECT pg_advisory_lock(4201), pg_advisory_lock(4202)", &[])
            .unwrap();
        // Reset A stalls on the write that closes its journal; its lock session
        // is cut and a reconcile finishes the reset.
        let reset_a = scope.spawn(|| server.reset_branch(r.branch.id, source));
        let stalled_a = blocked_backend(&mut probe, "UPDATE branches", &[]);
        let holder_a = advisory_holder(&mut probe, key);
        if let Some(holder) = holder_a {
            probe
                .query("SELECT pg_terminate_backend($1)", &[&holder])
                .unwrap();
        }
        let reconciled_a = server.reconcile();
        // Reset B opens its journal and stalls before the cutover.
        meta.batch_execute("SELECT setval('stall_cutover', 0)")
            .unwrap();
        let reset_b = scope.spawn(|| server.reset_branch(r.branch.id, source));
        let stalled_b = blocked_backend(&mut probe, "UPDATE branches", &[stalled_a]);
        let holder_b = advisory_holder(&mut probe, key);
        // A's stalled write is released only now, with B's journal open.
        gate.query("SELECT pg_advisory_unlock(4201)", &[]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let journal_b = journal(&mut meta);
        let replacement_b = journal_b.2.as_ref().map(|name| {
            count(
                &mut probe,
                &format!("SELECT 1 FROM pg_database WHERE datname = '{name}'"),
            )
        });
        // B is interrupted in turn.
        if let Some(holder) = holder_b {
            probe
                .query("SELECT pg_terminate_backend($1)", &[&holder])
                .unwrap();
        }
        gate.query("SELECT pg_advisory_unlock(4202)", &[]).unwrap();
        (
            (stalled_a, holder_a, reset_a.join().unwrap(), reconciled_a),
            (stalled_b, holder_b, reset_b.join().unwrap()),
            (journal_b, replacement_b),
        )
    });
    let (
        (stalled_a, holder_a, outcome_a, reconciled_a),
        (stalled_b, holder_b, outcome_b),
        (journal_b, replacement_b),
    ) = observed;
    assert_eq!(
        Some(stalled_a),
        holder_a,
        "metadata writes must run on the session that holds the project lock"
    );
    assert_eq!(Some(stalled_b), holder_b);
    let error = outcome_a.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("project lock connection was lost"),
        "reset A must not report success: {error:#}"
    );
    let report = reconciled_a.unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(report.actions.len(), 1, "{report:?}");
    assert!(report.actions[0].contains("finished reset"), "{report:?}");
    assert_eq!(
        (journal_b.0.as_deref(), journal_b.1.as_deref()),
        (Some("reset"), Some("staging")),
        "reset B's journal survived the release of A's write"
    );
    assert_eq!(replacement_b, Some(1), "B's replacement database exists");
    let error = outcome_b.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("project lock connection was lost"),
        "{error:#}"
    );
    // Reconcile sees B's journal and removes B's replacement.
    let report = server.reconcile().unwrap();
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(report.actions.len(), 1, "{report:?}");
    assert!(
        report.actions[0].contains("rolled back interrupted reset"),
        "{report:?}"
    );
    let report = server.reconcile().unwrap();
    assert!(
        report.actions.is_empty() && report.problems.is_empty(),
        "converged: {report:?}"
    );
    assert_eq!(journal(&mut meta), (None, None, None));
    let info = server.branch(r.branch.id).unwrap().unwrap();
    assert_eq!((info.status, info.generation), (RecordStatus::Ready, 1));
    assert_eq!(
        count(
            &mut connect(&r.url),
            "SELECT 1 FROM pg_tables WHERE tablename = 'marker'"
        ),
        0,
        "A's reset took effect"
    );
    // A journal write that finds its record changed fails as a lost race
    // instead of overwriting it.
    meta.batch_execute("SELECT setval('stall_cutover', 0)")
        .unwrap();
    let outcome_c = std::thread::scope(|scope| {
        let mut gate = superuser(&server, "devflow");
        gate.query("SELECT pg_advisory_lock(4202)", &[]).unwrap();
        let reset_c = scope.spawn(|| server.reset_branch(r.branch.id, source));
        blocked_backend(&mut probe, "UPDATE branches", &[]);
        meta.execute(
            "UPDATE branches SET generation = generation + 1 WHERE id = $1",
            &[&r.branch.id],
        )
        .unwrap();
        gate.query("SELECT pg_advisory_unlock(4202)", &[]).unwrap();
        reset_c.join().unwrap()
    });
    let error = format!("{:#}", outcome_c.unwrap_err());
    assert!(error.contains("lost a race"), "{error}");
    assert_eq!(
        journal(&mut meta),
        (None, None, None),
        "recovery cleaned up"
    );
    let info = server.branch(r.branch.id).unwrap().unwrap();
    assert_eq!(
        (info.status, info.generation),
        (RecordStatus::Ready, 2),
        "the other writer's change stands"
    );
    let report = server.reconcile().unwrap();
    assert!(
        report.actions.is_empty() && report.problems.is_empty(),
        "converged: {report:?}"
    );
    let branches = server.list_branches().unwrap();
    let mut expected: Vec<String> = branches.iter().map(|b| b.db_name.clone()).collect();
    expected.sort();
    let mut databases = df_databases(&mut admin);
    databases.sort();
    assert_eq!(
        databases, expected,
        "no database without a row, no row without a database"
    );
}

/// A reset follows the same capability rules as a creation: `require_cow` is
/// enforced before anything is staged, and the branch records the capability of
/// its new data instead of keeping a stale one.
#[test]
#[ignore = "needs Docker"]
fn reset_enforces_require_cow_and_records_the_new_capability() {
    let env = Env::new("");
    let server = env.server();
    let status = server.up().unwrap();
    print_cow(&env, &status);
    let mut admin = superuser(&server, "postgres");
    let a = server
        .create_branch("a", ResolvedSource::Empty, None)
        .unwrap();
    connect(&a.url)
        .batch_execute("CREATE TABLE t (i int); INSERT INTO t VALUES (1)")
        .unwrap();
    let b = server
        .create_branch("b", ResolvedSource::Branch(a.branch.id), None)
        .unwrap();
    assert_eq!(b.branch.cow, status.cow);
    connect(&b.url)
        .batch_execute("INSERT INTO t VALUES (2)")
        .unwrap();

    // The server stops cloning: new data would be a full copy.
    set_file_copy_method(&mut admin, &server, Some("copy"));
    env.write_config("require_cow = true", None);
    let strict = env.server();
    let before = df_databases(&mut admin);
    let error = strict
        .reset_branch(b.branch.id, ResolvedSource::Branch(a.branch.id))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("require_cow") && error.contains("copy"),
        "{error}"
    );
    assert_eq!(df_databases(&mut admin), before, "nothing was staged");
    let info = strict.branch(b.branch.id).unwrap().unwrap();
    assert_eq!(
        (info.status, info.generation, info.cow),
        (RecordStatus::Ready, 0, status.cow)
    );
    assert_eq!(count(&mut connect(&b.url), "SELECT * FROM t"), 2);
    assert!(strict.reconcile().unwrap().actions.is_empty());

    // Without `require_cow` the reset goes ahead and the stale value is replaced.
    let reset = server
        .reset_branch(b.branch.id, ResolvedSource::Branch(a.branch.id))
        .unwrap();
    assert_eq!((reset.generation, reset.cow), (1, CowCapability::Copy));
    assert_eq!(
        server.branch(b.branch.id).unwrap().unwrap().cow,
        CowCapability::Copy
    );
    assert_eq!(count(&mut connect(&b.url), "SELECT * FROM t"), 1);

    // Cloning again: the capability comes back with the next reset.
    set_file_copy_method(&mut admin, &server, None);
    let reset = server
        .reset_branch(b.branch.id, ResolvedSource::Branch(a.branch.id))
        .unwrap();
    assert_eq!((reset.generation, reset.cow), (2, status.cow));
    if status.cow == CowCapability::Verified {
        let reset = strict
            .reset_branch(b.branch.id, ResolvedSource::Branch(a.branch.id))
            .unwrap();
        assert_eq!((reset.generation, reset.cow), (3, CowCapability::Verified));
    }
    assert_eq!(
        df_databases(&mut admin).len(),
        2,
        "no replacement or backup left"
    );
}

/// A database that merely shares the name `devflow` is never adopted: bootstrap
/// refuses it without touching its privileges, and without staging anything. A
/// marked `devflow` (an interrupted bootstrap past the rename) resumes.
#[test]
#[ignore = "needs Docker"]
fn bootstrap_never_adopts_a_foreign_devflow_database() {
    const REFUSAL: &str = "was not created by devflow";
    let env = Env::new("");
    let backend = env.server();
    let managed = backend.managed().unwrap();
    managed.start().unwrap();
    let mut admin = superuser_when_ready(&backend, "postgres");

    // An application database called `devflow`, with its own grants.
    admin
        .batch_execute(
            "CREATE ROLE app LOGIN PASSWORD 'apppw'; CREATE ROLE reader LOGIN PASSWORD 'readerpw'",
        )
        .unwrap();
    admin.batch_execute("CREATE DATABASE devflow").unwrap();
    admin
        .batch_execute("GRANT CONNECT ON DATABASE devflow TO reader")
        .unwrap();
    superuser(&backend, "devflow")
        .batch_execute("CREATE TABLE precious (i int); INSERT INTO precious VALUES (1)")
        .unwrap();
    let port = managed.published_port().unwrap();
    let as_role = |role: &str, pw: &str| format!("postgres://{role}:{pw}@127.0.0.1:{port}/devflow");
    let snapshot = |admin: &mut Client| -> String {
        admin
            .query_one(
                "SELECT coalesce(datacl::text, '-') || ' ' || datallowconn::text || ' ' \
                        || pg_get_userbyid(datdba)::text || ' ' \
                        || coalesce(shobj_description(oid, 'pg_database'), '-') \
                 FROM pg_database WHERE datname = 'devflow'",
                &[],
            )
            .unwrap()
            .get(0)
    };
    let postgres_acl = |admin: &mut Client| -> String {
        one(
            admin,
            "SELECT coalesce(datacl::text, '-') FROM pg_database WHERE datname = 'postgres'",
        )
    };
    let foreign = snapshot(&mut admin);
    let postgres_before = postgres_acl(&mut admin);
    assert!(can_connect(&as_role("app", "apppw")) && can_connect(&as_role("reader", "readerpw")));

    // External mode as the superuser (the worst case), then the managed server.
    env.write_config(
        "",
        Some(&format!(
            "[postgres]\nmode = \"external\"\nadmin_url = \"{}\"\n",
            super_url(&backend, "postgres")
        )),
    );
    let external = env.server();
    for error in [
        format!("{:#}", external.up().unwrap_err()),
        format!(
            "{:#}",
            external
                .create_branch("x", ResolvedSource::Empty, None)
                .unwrap_err()
        ),
        format!("{:#}", external.list_branches().unwrap_err()),
        format!("{:#}", backend.up().unwrap_err()),
    ] {
        assert!(
            error.contains(REFUSAL) && error.contains("choose another server"),
            "{error}"
        );
    }
    assert_eq!(
        snapshot(&mut admin),
        foreign,
        "grants and flags are untouched"
    );
    assert_eq!(postgres_acl(&mut admin), postgres_before);
    assert!(can_connect(&as_role("app", "apppw")) && can_connect(&as_role("reader", "readerpw")));
    assert_eq!(
        count(
            &mut superuser(&backend, "devflow"),
            "SELECT 1 FROM pg_tables WHERE schemaname = 'public'"
        ),
        1,
        "no devflow tables were installed"
    );
    assert!(
        staging_databases(&mut admin).is_empty(),
        "refusing stages nothing"
    );

    // Moved out of the way, the name is free and bootstrap marks its own database.
    retry(|| admin.batch_execute("ALTER DATABASE devflow RENAME TO app_devflow"));
    external.up().unwrap();
    let ours = snapshot(&mut admin);
    assert!(ours.ends_with("devflow metadata v1"), "{ours}");
    assert!(!can_connect(&as_role("app", "apppw")));
    assert!(external.list_branches().unwrap().is_empty());
    assert!(
        staging_databases(&mut admin).is_empty(),
        "the staging database became `devflow`"
    );

    // A marked `devflow` without a schema (closed): resumes.
    let owner = format!("dfo_{}", &env.id.simple().to_string()[..8]);
    let reset_server = |admin: &mut Client| {
        // A project row that vanished with its database must not adopt the old owner role.
        admin
            .batch_execute("DROP DATABASE devflow WITH (FORCE)")
            .unwrap();
        admin.batch_execute(&format!("DROP ROLE {owner}")).unwrap();
    };
    reset_server(&mut admin);
    admin
        .batch_execute("CREATE DATABASE devflow TEMPLATE template0 ALLOW_CONNECTIONS false")
        .unwrap();
    admin
        .batch_execute("COMMENT ON DATABASE devflow IS 'devflow metadata v1'")
        .unwrap();
    external.up().unwrap();
    assert!(external.list_branches().unwrap().is_empty());
    assert!(!can_connect(&as_role("app", "apppw")));
    let branch = external
        .create_branch("after-resume", ResolvedSource::Empty, None)
        .unwrap();
    assert_eq!(branch.branch.status, RecordStatus::Ready);

    // A closed `devflow` without the mark is not ours either: bootstrap never
    // creates `devflow` unmarked, so there is nothing to wait for.
    external.drop_branch(branch.branch.id).unwrap();
    reset_server(&mut admin);
    admin
        .batch_execute("CREATE DATABASE devflow TEMPLATE template0 ALLOW_CONNECTIONS false")
        .unwrap();
    let unmarked = snapshot(&mut admin);
    let started = std::time::Instant::now();
    let error = format!("{:#}", external.up().unwrap_err());
    assert!(error.contains(REFUSAL), "{error}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "refused at once"
    );
    assert_eq!(snapshot(&mut admin), unmarked, "untouched");
    assert!(staging_databases(&mut admin).is_empty());
}

/// `devflow` is only ever created by renaming a marked staging database into
/// place. A bootstrap killed after `CREATE DATABASE` but before the mark leaves
/// an unmarked `devflow_bootstrap_*` that devflow reports but never adopts or
/// drops (the retry stages a new one); killed after the mark but before the
/// rename it leaves a marked one that the retry renames into place. Bootstraps
/// take turns on an advisory lock.
#[test]
#[ignore = "needs Docker"]
fn bootstrap_recovers_from_a_crash_before_the_mark_or_the_rename() {
    // Mirrors `db::admin::BOOTSTRAP_LOCK_KEY` ("devflow" + 2).
    const BOOTSTRAP_LOCK_KEY: i64 = 0x6465_7666_6c6f_7702;
    let env = Env::new("");
    let server = env.server();
    server.managed().unwrap().start().unwrap();
    let mut admin = superuser_when_ready(&server, "postgres");
    let mut probe = superuser(&server, "postgres");
    let owner = format!("dfo_{}", &env.id.simple().to_string()[..8]);
    let devflow_exists = |admin: &mut Client| -> i64 {
        count(admin, "SELECT 1 FROM pg_database WHERE datname = 'devflow'")
    };
    let oid_of = |admin: &mut Client, name: &str| -> u32 {
        one(
            admin,
            &format!("SELECT oid FROM pg_database WHERE datname = '{name}'"),
        )
    };

    // 1. Killed after CREATE DATABASE, while the mark waits for pg_shdescription.
    let outcome = std::thread::scope(|scope| {
        // Declared in here so that a failing assertion releases it.
        let mut blocker = superuser(&server, "postgres");
        blocker
            .batch_execute(
                "BEGIN; LOCK TABLE pg_catalog.pg_shdescription IN SHARE ROW EXCLUSIVE MODE",
            )
            .unwrap();
        let bootstrap = scope.spawn(|| server.up());
        let pid = blocked_backend(&mut probe, "COMMENT ON DATABASE", &[]);
        probe
            .query("SELECT pg_terminate_backend($1)", &[&pid])
            .unwrap();
        let outcome = bootstrap.join().unwrap();
        blocker.batch_execute("ROLLBACK").unwrap();
        outcome
    });
    assert!(outcome.is_err(), "the killed bootstrap fails");
    assert_eq!(devflow_exists(&mut admin), 0);
    let left = staging_databases(&mut admin);
    assert_eq!(left.len(), 1, "{left:?}");
    assert!(!left[0].1, "killed before the mark: unmarked");
    let unmarked = left[0].0.clone();
    let unmarked_oid = oid_of(&mut admin, &unmarked);

    // The retry stages a new database; the unmarked one is neither adopted nor dropped.
    let status = server.up().unwrap();
    assert_eq!(
        count(
            &mut admin,
            "SELECT 1 FROM pg_database WHERE datname = 'devflow' \
             AND shobj_description(oid, 'pg_database') = 'devflow metadata v1'"
        ),
        1
    );
    assert_eq!(
        staging_databases(&mut admin),
        vec![(unmarked.clone(), false)]
    );
    assert_eq!(oid_of(&mut admin, &unmarked), unmarked_oid);
    assert_ne!(oid_of(&mut admin, "devflow"), unmarked_oid);
    // ... and the operator hears about it, from every angle.
    let problem = status.problem.expect("status reports the leftover");
    assert!(problem.contains(&unmarked), "{problem}");
    let report = server.reconcile().unwrap();
    assert!(report.actions.is_empty(), "{report:?}");
    assert_eq!(report.problems.len(), 1, "{report:?}");
    assert!(report.problems[0].contains(&unmarked), "{report:?}");
    let branch = server
        .create_branch("with-leftover", ResolvedSource::Empty, None)
        .unwrap();
    assert!(
        branch.warnings.iter().any(|w| w.contains(&unmarked)),
        "{:?}",
        branch.warnings
    );
    server.drop_branch(branch.branch.id).unwrap();
    // The operator drops it.
    admin
        .batch_execute(&format!("DROP DATABASE {unmarked}"))
        .unwrap();
    assert!(server.reconcile().unwrap().problems.is_empty());
    assert!(server.status().problem.is_none());

    // 2. Killed after the mark, while the rename waits for pg_database.
    admin
        .batch_execute("DROP DATABASE devflow WITH (FORCE)")
        .unwrap();
    admin.batch_execute(&format!("DROP ROLE {owner}")).unwrap();
    let outcome = std::thread::scope(|scope| {
        let mut mark_blocker = superuser(&server, "postgres");
        mark_blocker
            .batch_execute(
                "BEGIN; LOCK TABLE pg_catalog.pg_shdescription IN SHARE ROW EXCLUSIVE MODE",
            )
            .unwrap();
        let bootstrap = scope.spawn(|| server.up());
        blocked_backend(&mut probe, "COMMENT ON DATABASE", &[]);
        let mut rename_blocker = superuser(&server, "postgres");
        rename_blocker
            .batch_execute(
                "SET lock_timeout = '10s'; \
                 BEGIN; LOCK TABLE pg_catalog.pg_database IN SHARE ROW EXCLUSIVE MODE",
            )
            .unwrap();
        // The mark lands; the rename waits.
        mark_blocker.batch_execute("ROLLBACK").unwrap();
        let pid = blocked_backend(&mut probe, "ALTER DATABASE", &[]);
        probe
            .query("SELECT pg_terminate_backend($1)", &[&pid])
            .unwrap();
        let outcome = bootstrap.join().unwrap();
        rename_blocker.batch_execute("ROLLBACK").unwrap();
        outcome
    });
    assert!(outcome.is_err(), "the killed bootstrap fails");
    assert_eq!(devflow_exists(&mut admin), 0);
    let left = staging_databases(&mut admin);
    assert_eq!(left.len(), 1, "{left:?}");
    assert!(left[0].1, "killed after the mark: marked");
    let staged_oid = oid_of(&mut admin, &left[0].0);

    // The retry renames the marked one into place instead of making another.
    let status = server.up().unwrap();
    assert!(staging_databases(&mut admin).is_empty());
    assert_eq!(oid_of(&mut admin, "devflow"), staged_oid);
    assert!(status.problem.is_none(), "{status:?}");
    let report = server.reconcile().unwrap();
    assert!(
        report.actions.is_empty() && report.problems.is_empty(),
        "{report:?}"
    );
    let branch = server
        .create_branch("after-rename", ResolvedSource::Empty, None)
        .unwrap();
    assert_eq!(branch.branch.status, RecordStatus::Ready);
    assert!(
        !can_connect(&url_for(&branch, "devflow")),
        "closed to logins"
    );

    // 3. Bootstraps take turns: while a peer holds the bootstrap lock, `up` waits.
    let mut holder = superuser(&server, "postgres");
    holder
        .query("SELECT pg_advisory_lock($1)", &[&BOOTSTRAP_LOCK_KEY])
        .unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| sender.send(server.up().map(|_| ())).unwrap());
        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(1500))
                .is_err(),
            "bootstrap must wait for its lock"
        );
        holder
            .query("SELECT pg_advisory_unlock($1)", &[&BOOTSTRAP_LOCK_KEY])
            .unwrap();
        receiver
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("bootstrap resumes")
            .unwrap();
    });
    assert!(staging_databases(&mut admin).is_empty());
}

// ---- no Docker needed -----------------------------------------------------

/// `status()` never fails: an unreachable external server is a problem string,
/// and `DEVFLOW_PG_ADMIN_URL` takes precedence over `admin_url`.
#[test]
fn external_status_reports_unreachable_servers_and_honours_the_env_override() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = tempfile::tempdir().unwrap();
    git2::Repository::init(repo.path()).unwrap();
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    std::fs::write(
        repo.path().join("devflow.toml"),
        format!(
            "[project]\nname = \"devflow-test-ext\"\nid = \"{}\"\n\n[postgres]\nmode = \"external\"\n\
             admin_url = \"postgres://prov:pw@127.0.0.1:{closed}/postgres\"\napp_host = \"db.internal\"\napp_port = 6543\n",
            Uuid::new_v4()
        ),
    )
    .unwrap();
    // SAFETY: serialized by ENV_LOCK.
    unsafe { std::env::remove_var("DEVFLOW_PG_ADMIN_URL") };
    let project = Project::open(repo.path()).unwrap();
    let server = Server::for_project(&project).unwrap().unwrap();
    assert!(server.managed().is_none());
    let status = server.status();
    assert!(status.running && !status.reachable, "{status:?}");
    assert_eq!(status.endpoint, Some(format!("127.0.0.1:{closed}")));
    assert!(status.problem.is_some());
    assert_eq!(
        server.app_endpoint().unwrap(),
        ("db.internal".to_string(), 6543)
    );
    assert!(
        server
            .create_branch("x", ResolvedSource::Empty, None)
            .is_err()
    );
    assert!(server.list_branches().is_err());

    unsafe {
        std::env::set_var(
            "DEVFLOW_PG_ADMIN_URL",
            format!(
                "postgres://other:pw@127.0.0.1:{}/postgres",
                closed.wrapping_add(1)
            ),
        )
    };
    let status = server.status();
    assert_eq!(
        status.endpoint,
        Some(format!("127.0.0.1:{}", closed.wrapping_add(1)))
    );
    unsafe { std::env::remove_var("DEVFLOW_PG_ADMIN_URL") };
}

//! The `sqlite` engine through the whole pipeline, against real Git
//! repositories and real SQLite files in temporary directories. Nothing here
//! needs Docker.
use devflow_core::config::Engine;
use devflow_core::db::DataSource;
use devflow_core::receipt::{Receipt, Status};
use devflow_core::remedy::{Remedy, remedies_of};
use devflow_core::setup::{
    self, CreateRequest, NoProgress, RemoveOptions, SetupOptions, SetupOutcome,
};
use devflow_core::sqlite::{self, Store, ops::staged_name};
use devflow_core::worktree::{self, Workspace};
use devflow_core::{Project, env_file};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::{Once, OnceLock};
use std::time::Duration;

/// Process-wide state directories (every test has a project id of its own).
fn init() -> &'static Path {
    static ONCE: Once = Once::new();
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    ONCE.call_once(|| {
        let dir = DIR.get_or_init(|| tempfile::tempdir().unwrap());
        // SAFETY: runs once, before any test body touches the environment.
        unsafe {
            std::env::set_var("DEVFLOW_CONFIG_DIR", dir.path().join("config"));
            std::env::set_var("DEVFLOW_DATA_DIR", dir.path().join("data"));
            std::env::remove_var("DEVFLOW_TRUST_HOOKS");
            std::env::remove_var("DEVFLOW_INSTANCE_ID");
        }
    });
    DIR.get().unwrap().path()
}

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    project: Project,
}

fn commit_all(repo: &git2::Repository, message: &str) {
    let mut index = repo.index().unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<_> = parent.iter().collect();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
        .unwrap();
}

/// A repository `shop` whose configuration has the sqlite service `app` at
/// `db/app.sqlite3`, plus `extra`.
fn repo(extra: &str) -> Repo {
    init();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("shop");
    std::fs::create_dir_all(&root).unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.initial_head("main");
    let git = git2::Repository::init_opts(&root, &options).unwrap();
    std::fs::write(
        root.join("devflow.toml"),
        format!(
            "[project]\nname = \"shop\"\nid = \"{}\"\n\n\
             [services.app]\nengine = \"sqlite\"\npath = \"db/app.sqlite3\"\nbusy_wait = \"400ms\"\n\n\
             {extra}",
            uuid::Uuid::new_v4()
        ),
    )
    .unwrap();
    std::fs::write(
        root.join(".gitignore"),
        ".env\ndb/\ndevflow.local.toml\nnode_modules/\n",
    )
    .unwrap();
    std::fs::write(root.join("README.md"), "shop\n").unwrap();
    commit_all(&git, "initial");
    let project = Project::open(&root).unwrap();
    Repo {
        _dir: dir,
        root,
        project,
    }
}

/// The database of the main checkout, with `rows` rows in each of two tables
/// that are always written together.
fn main_database(t: &Repo, rows: i64, mode: &str) -> PathBuf {
    let path = t.root.join("db/app.sqlite3");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "journal_mode", mode).unwrap();
    conn.execute_batch(
        "CREATE TABLE a (n INTEGER PRIMARY KEY, v TEXT); CREATE TABLE b (n INTEGER PRIMARY KEY, v TEXT);",
    )
    .unwrap();
    for n in 1..=rows {
        conn.execute("INSERT INTO a VALUES (?1, 'x')", [n]).unwrap();
        conn.execute("INSERT INTO b VALUES (?1, 'x')", [n]).unwrap();
    }
    path
}

/// Rows of both tables of a database, which must agree.
fn rows(path: &Path) -> i64 {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let a: i64 = conn
        .query_row("SELECT count(*) FROM a", [], |r| r.get(0))
        .unwrap();
    let b: i64 = conn
        .query_row("SELECT count(*) FROM b", [], |r| r.get(0))
        .unwrap();
    assert_eq!(a, b, "{} is torn", path.display());
    a
}

fn create(project: &Project, name: &str, options: SetupOptions) -> anyhow::Result<SetupOutcome> {
    setup::create(
        project,
        CreateRequest {
            name: name.into(),
            from: None,
            base: None,
            options,
        },
        &mut NoProgress,
    )
}

fn from(source: DataSource) -> SetupOptions {
    SetupOptions {
        data_from: Some(source),
        ..Default::default()
    }
}

fn default_workspace(project: &Project) -> Workspace {
    worktree::list(project).unwrap().remove(0)
}

fn setup_default(project: &Project) -> SetupOutcome {
    setup::setup(
        project,
        &default_workspace(project),
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap()
}

fn env_map(ws: &Workspace) -> std::collections::HashMap<String, String> {
    env_file::read(&ws.path).unwrap().into_iter().collect()
}

fn db_in(ws: &Workspace) -> PathBuf {
    ws.path.join("db/app.sqlite3")
}

fn names(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    found
}

#[test]
fn the_default_workspace_uses_the_file_in_the_main_checkout_and_owns_nothing() {
    let t = repo("");
    let file = main_database(&t, 5, "delete");
    let before = std::fs::read(&file).unwrap();
    let out = setup_default(&t.project);
    assert_eq!(out.status, Status::Ready, "{:?}", out.warnings);
    let binding = &out.receipt.services["app"];
    assert_eq!(binding.engine, Engine::Sqlite);
    assert!(
        !binding.owned,
        "the project's own database is not devflow's"
    );
    assert_eq!(binding.source, "existing");
    assert_eq!(binding.database, "db/app.sqlite3");
    assert!(binding.cloned.as_ref().unwrap().method.is_none());
    // The workspace's variables name that file, as a path and as a URL.
    let env = env_map(&out.workspace);
    assert_eq!(env["SQLITE_PATH"], file.display().to_string());
    assert_eq!(env["DATABASE_URL"], format!("sqlite://{}", file.display()));
    // Setup changed nothing.
    assert_eq!(std::fs::read(&file).unwrap(), before);
    assert_eq!(names(file.parent().unwrap()), ["app.sqlite3"]);
    // A reset or a drop of it is refused.
    let error = setup::sqlite_reset(&t.project, &out.workspace, "app", None, None).unwrap_err();
    assert!(
        format!("{error:#}").contains("default workspace"),
        "{error:#}"
    );
    let error = setup::sqlite_drop(&t.project, &out.workspace, "app", None).unwrap_err();
    assert!(format!("{error:#}").contains("not removed"), "{error:#}");
    assert_eq!(std::fs::read(&file).unwrap(), before);
}

#[test]
fn a_new_workspace_gets_a_consistent_copy_and_variables_of_its_own() {
    let t = repo("");
    let main = main_database(&t, 40, "wal");
    // An application with the database open, frames still in the log.
    let app = Connection::open(&main).unwrap();
    app.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    for n in 41..=60 {
        app.execute("INSERT INTO a VALUES (?1, 'x')", [n]).unwrap();
        app.execute("INSERT INTO b VALUES (?1, 'x')", [n]).unwrap();
    }
    let out = create(&t.project, "feature/a", SetupOptions::default()).unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.warnings);
    let file = db_in(&out.workspace);
    assert_eq!(rows(&file), 60, "the committed rows, the log's included");
    let binding = &out.receipt.services["app"];
    assert!(binding.owned);
    assert_eq!(binding.source, "workspace:main");
    let method = binding.cloned.as_ref().unwrap().method;
    assert!(method.is_some(), "the clone says how it was made");
    assert!(
        out.receipt
            .phase(devflow_core::receipt::Phase::Services)
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("from workspace:main"),
        "{:?}",
        out.receipt.phase(devflow_core::receipt::Phase::Services)
    );
    // Its own variables: this worktree's file, not the main checkout's.
    let env = env_map(&out.workspace);
    assert_eq!(env["SQLITE_PATH"], file.display().to_string());
    assert_eq!(env["DATABASE_URL"], format!("sqlite://{}", file.display()));
    assert_ne!(env["SQLITE_PATH"], main.display().to_string());
    // Independent: writing in the clone leaves the source alone, and back.
    let mine = Connection::open(&file).unwrap();
    mine.execute("INSERT INTO a VALUES (1000, 'mine')", [])
        .unwrap();
    mine.execute("INSERT INTO b VALUES (1000, 'mine')", [])
        .unwrap();
    drop(mine);
    assert_eq!(rows(&file), 61);
    assert_eq!(rows(&main), 60);
    // Nothing is left beside the file but SQLite's own.
    assert!(
        names(file.parent().unwrap())
            .iter()
            .all(|n| n.starts_with("app.sqlite3")),
        "{:?}",
        names(file.parent().unwrap())
    );
    // Entering the workspace again keeps the same file.
    let again = setup::activate(
        &t.project,
        &out.workspace,
        SetupOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(again.status, Status::Ready);
    assert_eq!(rows(&file), 61);
}

#[test]
fn writers_in_the_source_never_tear_the_workspaces_made_beside_them() {
    let t = repo("");
    let main = main_database(&t, 10, "wal");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writers: Vec<_> = (0..2)
        .map(|w| {
            let (path, stop) = (main.clone(), stop.clone());
            std::thread::spawn(move || {
                let conn = Connection::open(&path).unwrap();
                conn.busy_timeout(Duration::from_secs(30)).unwrap();
                let mut n = 10_000 * (w + 1);
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(3));
                    n += 1;
                    if conn.execute_batch("BEGIN IMMEDIATE").is_err() {
                        continue;
                    }
                    let ok = conn.execute("INSERT INTO a VALUES (?1, 'x')", [n]).is_ok()
                        && conn.execute("INSERT INTO b VALUES (?1, 'x')", [n]).is_ok();
                    let _ = conn.execute_batch(if ok { "COMMIT" } else { "ROLLBACK" });
                }
            })
        })
        .collect();
    let mut made = Vec::new();
    for n in 0..4 {
        let out = create(&t.project, &format!("beside-{n}"), SetupOptions::default()).unwrap();
        made.push(db_in(&out.workspace));
        std::thread::sleep(Duration::from_millis(20));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for writer in writers {
        writer.join().unwrap();
    }
    for file in made {
        assert!(rows(&file) >= 10, "{}", file.display());
    }
}

#[test]
fn data_comes_from_a_workspace_a_snapshot_or_nothing() {
    let t = repo("");
    let main = main_database(&t, 7, "delete");
    let main_ws = default_workspace(&t.project);
    let snapshot = setup::sqlite_snapshot(&t.project, &main_ws, "app", "seed", None).unwrap();
    assert_eq!((snapshot.name.as_str(), snapshot.version), ("seed", 1));
    assert_eq!(snapshot.source, "workspace:main");
    // The main database moves on; the snapshot does not.
    let conn = Connection::open(&main).unwrap();
    conn.execute("INSERT INTO a VALUES (100, 'x')", []).unwrap();
    conn.execute("INSERT INTO b VALUES (100, 'x')", []).unwrap();
    drop(conn);
    let second = setup::sqlite_snapshot(&t.project, &main_ws, "app", "seed", None).unwrap();
    assert_eq!(second.version, 2);

    let seeded = create(
        &t.project,
        "seeded",
        from(DataSource::Snapshot {
            name: "seed".into(),
            version: Some(1),
        }),
    )
    .unwrap();
    assert_eq!(rows(&db_in(&seeded.workspace)), 7);
    assert_eq!(seeded.receipt.services["app"].source, "snapshot:seed@v1");
    // Latest by default.
    let latest = create(
        &t.project,
        "latest",
        from(DataSource::Snapshot {
            name: "seed".into(),
            version: None,
        }),
    )
    .unwrap();
    assert_eq!(rows(&db_in(&latest.workspace)), 8);
    assert_eq!(latest.receipt.services["app"].source, "snapshot:seed@v2");
    // The clone of a snapshot is writable and leaves the snapshot alone.
    let mine = Connection::open(db_in(&latest.workspace)).unwrap();
    mine.execute("INSERT INTO a VALUES (500, 'x')", []).unwrap();
    mine.execute("INSERT INTO b VALUES (500, 'x')", []).unwrap();
    drop(mine);
    let store = Store::open(t.project.id(), "app").unwrap();
    let file = store
        .verified_file(&store.find("seed", Some(2)).unwrap().unwrap())
        .unwrap();
    assert_eq!(rows(&file), 8);

    // Another workspace's database.
    let copy = create(
        &t.project,
        "copy",
        from(DataSource::Workspace {
            name: "seeded".into(),
        }),
    )
    .unwrap();
    assert_eq!(rows(&db_in(&copy.workspace)), 7);
    assert_eq!(copy.receipt.services["app"].source, "workspace:seeded");

    // Nothing: no file, the application creates one; the directory is there.
    let empty = create(&t.project, "blank", from(DataSource::Empty)).unwrap();
    assert!(!db_in(&empty.workspace).exists());
    assert!(db_in(&empty.workspace).parent().unwrap().is_dir());
    assert_eq!(empty.receipt.services["app"].source, "empty");
    assert!(empty.receipt.services["app"].owned);
    assert_eq!(
        env_map(&empty.workspace)["SQLITE_PATH"],
        db_in(&empty.workspace).display().to_string(),
        "the variable names the file the application will create"
    );

    // What cannot be: a branch (there are none), a snapshot nobody took.
    let error = create(
        &t.project,
        "no-branches",
        from(DataSource::Branch {
            id: uuid::Uuid::new_v4(),
        }),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("no branches"), "{error:#}");
    let error = create(
        &t.project,
        "no-snapshot",
        from(DataSource::Snapshot {
            name: "ghost".into(),
            version: None,
        }),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("no snapshot named 'ghost'"),
        "{error:#}"
    );
}

#[test]
fn a_workspace_whose_source_has_no_file_yet_starts_without_one_and_says_so() {
    let t = repo("");
    let out = create(&t.project, "early", SetupOptions::default()).unwrap();
    assert_eq!(out.status, Status::Ready);
    assert!(!db_in(&out.workspace).exists());
    assert!(
        out.warnings.iter().any(|w| w.contains("no database file")),
        "{:?}",
        out.warnings
    );
    assert!(
        out.receipt.services["app"]
            .cloned
            .as_ref()
            .unwrap()
            .method
            .is_none()
    );
}

#[test]
fn seed_snapshots_serve_runs_that_cannot_prompt() {
    let t = repo("[data]\nnoninteractive_from = \"snapshot:seed\"\n");
    main_database(&t, 3, "delete");
    let main_ws = default_workspace(&t.project);
    let quiet = SetupOptions {
        non_interactive: true,
        ..Default::default()
    };
    // No seed: the run fails, with the snapshot to take as a way on, and does
    // not clone the live main database in its place.
    let error = create(&t.project, "unseeded", quiet.clone()).unwrap_err();
    let remedies = remedies_of(&error);
    assert!(
        remedies.iter().any(|r| matches!(
            r,
            Remedy::CreateSeedSnapshot { key, snapshot, engine, .. }
                if key == "app" && snapshot == "seed" && *engine == Engine::Sqlite
        )),
        "{remedies:?} / {error:#}"
    );
    setup::sqlite_snapshot(&t.project, &main_ws, "app", "seed", None).unwrap();
    // Main moves on: the seed is what the run takes.
    let conn = Connection::open(t.root.join("db/app.sqlite3")).unwrap();
    conn.execute("INSERT INTO a VALUES (9, 'x')", []).unwrap();
    conn.execute("INSERT INTO b VALUES (9, 'x')", []).unwrap();
    drop(conn);
    let seeded = create(&t.project, "seeded", quiet.clone()).unwrap();
    assert_eq!(rows(&db_in(&seeded.workspace)), 3);
    assert_eq!(seeded.receipt.services["app"].source, "snapshot:seed@v1");
    // An interactive run is not seeded: its default is the code source.
    let live = create(&t.project, "live", SetupOptions::default()).unwrap();
    assert_eq!(rows(&db_in(&live.workspace)), 4);
}

#[test]
fn a_file_that_was_already_there_is_left_alone_and_never_owned() {
    let t = repo("[worktree]\ncopy = [\"db/app.sqlite3\"]\n");
    let t = Repo {
        project: {
            // The payload clones the file before the service looks at it.
            main_database(&t, 6, "delete");
            Project::open(&t.root).unwrap()
        },
        ..t
    };
    let out = create(&t.project, "brought-along", SetupOptions::default()).unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.warnings);
    assert_eq!(rows(&db_in(&out.workspace)), 6);
    let binding = &out.receipt.services["app"];
    assert!(!binding.owned);
    assert_eq!(binding.source, "existing");
    assert!(
        out.warnings.iter().any(|w| w.contains("left it alone")),
        "{:?}",
        out.warnings
    );
    // A reset, a drop or a removal never touches it.
    let error = setup::sqlite_reset(&t.project, &out.workspace, "app", None, None).unwrap_err();
    assert!(
        format!("{error:#}").contains("not made by devflow"),
        "{error:#}"
    );
    let error = setup::sqlite_drop(&t.project, &out.workspace, "app", None).unwrap_err();
    assert!(format!("{error:#}").contains("not removed"), "{error:#}");
    assert_eq!(rows(&db_in(&out.workspace)), 6);
}

/// A worktree and a receipt as a run that died after recording its intent.
fn interrupted(t: &Repo, name: &str, id: uuid::Uuid) -> Workspace {
    use devflow_core::receipt::{PendingBranch, Phase, PhaseState};
    let ws = worktree::create(&t.project, name, None).unwrap();
    let mut receipt = Receipt::new(t.project.id(), name, Some("main".into()));
    receipt.set_phase(Phase::Materialize, PhaseState::Done, None);
    receipt.set_phase(Phase::Payload, PhaseState::Done, None);
    receipt.set_phase(Phase::Services, PhaseState::Started, None);
    receipt.creating = Some(PendingBranch {
        service: "app".into(),
        branch_id: id,
        engine: Some(Engine::Sqlite),
        file: None,
    });
    receipt
        .service_sources
        .insert("app".into(), "workspace:main".into());
    receipt
        .service_source_ids
        .insert("app".into(), format!("checkout:{}", t.root.display()));
    receipt.save(&ws.git_dir).unwrap();
    ws
}

#[test]
fn a_run_that_died_while_copying_is_finished_by_its_identity() {
    let t = repo("");
    main_database(&t, 12, "delete");
    let id = uuid::Uuid::new_v4();
    let ws = interrupted(&t, "died-copying", id);
    let dest = db_in(&ws);
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    // What it had staged, under the identity it recorded.
    std::fs::write(staged_name(&dest, id), b"half a database").unwrap();
    let out = setup::setup(
        &t.project,
        &ws,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.warnings);
    assert_eq!(rows(&dest), 12);
    assert_eq!(
        out.receipt.services["app"].branch_id,
        Some(id),
        "same identity"
    );
    assert!(out.receipt.services["app"].owned);
    assert!(out.receipt.creating.is_none());
    assert_eq!(names(dest.parent().unwrap()), ["app.sqlite3"]);
}

/// The identity of the file at `path`, as a run records it before installing.
fn identity_of(path: &Path) -> devflow_core::sqlite::Identity {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).unwrap();
    devflow_core::sqlite::Identity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        len: metadata.len(),
    }
}

/// Record `identity` as the file the interrupted run staged.
fn staged_identity(ws: &Workspace, identity: Option<devflow_core::sqlite::Identity>) {
    let mut receipt = Receipt::load(&ws.git_dir).unwrap().unwrap();
    receipt.creating.as_mut().unwrap().file = identity;
    receipt.save(&ws.git_dir).unwrap();
}

fn setup_again(t: &Repo, ws: &Workspace) -> anyhow::Result<SetupOutcome> {
    setup::setup(
        &t.project,
        ws,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .map(|out| out.unwrap())
}

#[cfg(unix)]
#[test]
fn a_run_that_died_after_installing_the_file_adopts_exactly_that_file() {
    let t = repo("");
    let main = main_database(&t, 4, "delete");
    let id = uuid::Uuid::new_v4();
    let ws = interrupted(&t, "died-installed", id);
    let dest = db_in(&ws);
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    // What the run staged, installed by the rename it then died after.
    let staged = devflow_core::sqlite::ops::staged_name(&dest, id);
    std::fs::copy(&main, &staged).unwrap();
    staged_identity(&ws, Some(identity_of(&staged)));
    std::fs::rename(&staged, &dest).unwrap();
    let out = setup_again(&t, &ws).unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.warnings);
    let binding = &out.receipt.services["app"];
    assert!(binding.owned, "it is the file the intent recorded");
    assert_eq!(binding.branch_id, Some(id));
    assert!(out.receipt.creating.is_none());
    assert_eq!(rows(&dest), 4);
}

#[cfg(unix)]
#[test]
fn a_file_somebody_else_put_there_is_never_adopted_whatever_its_header_says() {
    let t = repo("");
    let main = main_database(&t, 4, "delete");

    // The run staged a file and recorded it; an application created the
    // destination (a valid database) before the run could install.
    let id = uuid::Uuid::new_v4();
    let ws = interrupted(&t, "taken-by-an-app", id);
    let dest = db_in(&ws);
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    let staged = devflow_core::sqlite::ops::staged_name(&dest, id);
    std::fs::copy(&main, &staged).unwrap();
    staged_identity(&ws, Some(identity_of(&staged)));
    std::fs::copy(&main, &dest).unwrap();
    let out = setup_again(&t, &ws).unwrap();
    let binding = &out.receipt.services["app"];
    assert!(!binding.owned, "a different inode is not the staged file");
    assert_eq!(binding.source, "existing");
    assert!(out.receipt.creating.is_none(), "the intent is settled");
    assert!(
        out.warnings.iter().any(|w| w.contains("left it alone")),
        "{:?}",
        out.warnings
    );
    assert!(!staged.exists(), "what the run staged goes");
    assert_eq!(rows(&dest), 4);
    // Left alone for good: a reset refuses it.
    let error = setup::sqlite_reset(&t.project, &ws, "app", None, None).unwrap_err();
    assert!(
        format!("{error:#}").contains("not made by devflow"),
        "{error:#}"
    );

    // The intent never got as far as recording a staged file: whatever is
    // at the destination cannot be ours.
    let ws = interrupted(&t, "taken-before-staging", uuid::Uuid::new_v4());
    let dest = db_in(&ws);
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::write(&dest, "this is a text file, not a database at all").unwrap();
    let out = setup_again(&t, &ws).unwrap();
    assert!(!out.receipt.services["app"].owned);
    assert!(out.receipt.creating.is_none());
    assert_eq!(
        std::fs::read_to_string(&dest).unwrap(),
        "this is a text file, not a database at all",
        "left alone"
    );
}

#[cfg(unix)]
#[test]
fn a_destination_that_appears_while_installing_is_a_typed_problem_and_never_adopted() {
    let t = repo("");
    let main = main_database(&t, 3, "delete");
    // The workspace is created for real; the application creates its
    // database in the moment between staging and installing.
    let ws = worktree::create(&t.project, "racing", None).unwrap();
    let dest = db_in(&ws);
    let mine = dest.clone();
    devflow_core::sqlite::ops::seams::set_before_install(dest.clone(), move || {
        std::fs::write(&mine, "an application got there first").unwrap();
    });
    let error = setup::setup(
        &t.project,
        &ws,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("appeared while it was being cloned"),
        "{error:#}"
    );
    assert!(
        remedies_of(&error)
            .iter()
            .any(|r| matches!(r, Remedy::Setup { .. }))
    );
    assert_eq!(
        std::fs::read_to_string(&dest).unwrap(),
        "an application got there first"
    );
    assert_eq!(
        names(dest.parent().unwrap()),
        ["app.sqlite3"],
        "the staged file is gone"
    );
    let receipt = Receipt::load(&ws.git_dir).unwrap().unwrap();
    assert!(receipt.creating.is_none(), "the intent is cleared");
    assert!(!receipt.services.contains_key("app"));
    // The retry finds the file there, as anyone's: left alone, not owned.
    let out = setup_again(&t, &ws).unwrap();
    let binding = &out.receipt.services["app"];
    assert!(!binding.owned);
    assert_eq!(
        std::fs::read_to_string(&dest).unwrap(),
        "an application got there first"
    );
    let _ = main;
}

#[test]
fn a_source_a_writer_holds_is_refused_with_a_way_on_and_resumes_when_it_lets_go() {
    let t = repo("");
    let main = main_database(&t, 5, "delete");
    let holder = Connection::open(&main).unwrap();
    holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
    holder
        .execute("INSERT INTO a VALUES (50, 'x')", [])
        .unwrap();
    let error = create(&t.project, "blocked", SetupOptions::default()).unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("locked for writing"), "{text}");
    let remedies = remedies_of(&error);
    assert!(remedies.contains(&Remedy::Retry), "{remedies:?}");
    assert!(
        remedies.iter().any(|r| matches!(r, Remedy::Setup { .. })),
        "{remedies:?}"
    );
    // The workspace is kept, with nothing torn in it.
    let ws = worktree::find(&t.project, "blocked").unwrap().unwrap();
    assert!(!db_in(&ws).exists());
    assert!(
        !names(db_in(&ws).parent().unwrap_or(&ws.path))
            .iter()
            .any(|n| n.contains("partial"))
    );
    // The writer commits both halves; setup resumes and copies that state.
    holder
        .execute("INSERT INTO b VALUES (50, 'x')", [])
        .unwrap();
    holder.execute_batch("COMMIT").unwrap();
    let out = setup::setup(
        &t.project,
        &ws,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.warnings);
    assert_eq!(rows(&db_in(&ws)), 6);
}

#[test]
fn a_reset_goes_back_to_the_recorded_source_or_to_another() {
    let t = repo("");
    let main = main_database(&t, 5, "delete");
    let main_ws = default_workspace(&t.project);
    let out = create(&t.project, "resettable", SetupOptions::default()).unwrap();
    let file = db_in(&out.workspace);
    let mine = Connection::open(&file).unwrap();
    mine.execute("DELETE FROM a", []).unwrap();
    mine.execute("DELETE FROM b", []).unwrap();
    drop(mine);
    assert_eq!(rows(&file), 0);

    // Open elsewhere: refused, and nothing changed.
    let open = Connection::open(&file).unwrap();
    open.execute_batch("BEGIN").unwrap();
    open.query_row("SELECT count(*) FROM a", [], |r| r.get::<_, i64>(0))
        .unwrap();
    let error = setup::sqlite_reset(&t.project, &out.workspace, "app", None, None).unwrap_err();
    assert!(format!("{error:#}").contains("open"), "{error:#}");
    assert!(remedies_of(&error).contains(&Remedy::Retry));
    open.execute_batch("ROLLBACK").unwrap();
    drop(open);
    assert_eq!(rows(&file), 0);

    // Back to where it came from: the main checkout's file as it is now.
    let reset = setup::sqlite_reset(&t.project, &out.workspace, "app", None, None).unwrap();
    assert_eq!(reset.source, "workspace:main");
    assert!(reset.method.is_some());
    assert_eq!(rows(&file), 5);
    assert_eq!(names(file.parent().unwrap()), ["app.sqlite3"]);
    // To a snapshot: recorded as the new source, and what a plain reset
    // returns to afterwards.
    let conn = Connection::open(&main).unwrap();
    conn.execute("INSERT INTO a VALUES (8, 'x')", []).unwrap();
    conn.execute("INSERT INTO b VALUES (8, 'x')", []).unwrap();
    drop(conn);
    setup::sqlite_snapshot(&t.project, &main_ws, "app", "later", None).unwrap();
    let to = DataSource::Snapshot {
        name: "later".into(),
        version: None,
    };
    let reset = setup::sqlite_reset(&t.project, &out.workspace, "app", Some(to), None).unwrap();
    assert_eq!(reset.source, "snapshot:later@v1");
    assert_eq!(rows(&file), 6);
    let receipt = setup::receipt(&t.project, &out.workspace).unwrap().unwrap();
    assert_eq!(receipt.services["app"].source, "snapshot:later@v1");
    let conn = Connection::open(&file).unwrap();
    conn.execute("DELETE FROM a", []).unwrap();
    conn.execute("DELETE FROM b", []).unwrap();
    drop(conn);
    setup::sqlite_reset(&t.project, &out.workspace, "app", None, None).unwrap();
    assert_eq!(rows(&file), 6);
    // Not from itself, and not from a branch.
    let error = setup::sqlite_reset(
        &t.project,
        &out.workspace,
        "app",
        Some(DataSource::Workspace {
            name: "resettable".into(),
        }),
        None,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("from itself"), "{error:#}");
}

#[test]
fn dropping_removes_the_file_and_the_next_setup_clones_it_again() {
    let t = repo("");
    main_database(&t, 9, "wal");
    let out = create(&t.project, "droppable", SetupOptions::default()).unwrap();
    let file = db_in(&out.workspace);
    // SQLite's files beside it go with it.
    std::fs::write(sqlite::capture::sidecar(&file, "-wal"), b"stale").unwrap();
    assert!(setup::sqlite_drop(&t.project, &out.workspace, "app", None).unwrap());
    assert_eq!(names(file.parent().unwrap()), Vec::<String>::new());
    let receipt = setup::receipt(&t.project, &out.workspace).unwrap().unwrap();
    assert!(!receipt.services.contains_key("app"));
    // Dropping what is not there is not an error.
    assert!(setup::sqlite_drop(&t.project, &out.workspace, "app", None).is_err());
    let again = setup::setup(
        &t.project,
        &out.workspace,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(again.status, Status::Ready);
    assert_eq!(rows(&file), 9);
}

#[test]
fn removing_a_workspace_takes_its_database_with_it_and_keeps_the_snapshots() {
    let t = repo("");
    main_database(&t, 3, "delete");
    let main_ws = default_workspace(&t.project);
    setup::sqlite_snapshot(&t.project, &main_ws, "app", "keep", None).unwrap();
    let out = create(&t.project, "short-lived", SetupOptions::default()).unwrap();
    let file = db_in(&out.workspace);
    assert!(file.exists());
    setup::remove(
        &t.project,
        "short-lived",
        RemoveOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert!(!file.exists());
    assert!(!out.workspace.path.exists());
    assert_eq!(setup::sqlite_snapshots(&t.project, "app").unwrap().len(), 1);
    // The main checkout's database is untouched.
    assert_eq!(rows(&t.root.join("db/app.sqlite3")), 3);
}

#[test]
fn a_workspace_set_up_without_databases_exports_nothing_of_it() {
    let t = repo("");
    main_database(&t, 2, "delete");
    let out = create(
        &t.project,
        "no-db",
        SetupOptions {
            no_db: true,
            ..Default::default()
        },
    )
    .unwrap();
    let env = env_map(&out.workspace);
    assert!(!env.contains_key("SQLITE_PATH") && !env.contains_key("DATABASE_URL"));
    assert!(!db_in(&out.workspace).exists());
    // But it still owns the names, so a caller's values for them (another
    // workspace's database) are cleared in what it starts.
    let contents = env_file::read_contents(&out.workspace.path).unwrap();
    let owned = contents.declared.unwrap();
    assert!(
        owned.contains("SQLITE_PATH") && owned.contains("DATABASE_URL"),
        "{owned:?}"
    );
}

#[test]
fn listings_show_where_each_database_is_and_what_it_came_from() {
    let t = repo("");
    main_database(&t, 2, "wal");
    create(&t.project, "listed", SetupOptions::default()).unwrap();
    let all = setup::sqlite_databases(&t.project, "app").unwrap();
    assert_eq!(all.len(), 2);
    let main = all.iter().find(|d| d.is_default).unwrap();
    assert!(main.file.exists);
    assert_eq!(main.file.journal_mode.as_deref(), Some("wal"));
    assert!(!main.owned);
    let listed = all.iter().find(|d| d.workspace == "listed").unwrap();
    assert!(listed.owned);
    assert_eq!(listed.source.as_deref(), Some("workspace:main"));
    assert!(listed.cloned.is_some());
    assert!(setup::sqlite_databases(&t.project, "nope").is_err());
    // Looking at them opened nothing: no log, no index beside any file.
    for database in &all {
        for suffix in ["-wal", "-shm"] {
            assert!(!sqlite::capture::sidecar(&database.path, suffix).exists());
        }
    }
}

#[test]
fn a_service_that_moves_its_file_is_provisioned_again_and_says_so() {
    let t = repo("");
    main_database(&t, 4, "delete");
    let out = create(&t.project, "mover", SetupOptions::default()).unwrap();
    // The configuration now puts the file elsewhere.
    let text = std::fs::read_to_string(t.root.join("devflow.toml")).unwrap();
    std::fs::write(
        t.root.join("devflow.toml"),
        text.replace("db/app.sqlite3", "db/other.sqlite3"),
    )
    .unwrap();
    let project = Project::open(&t.root).unwrap();
    let again = setup::setup(
        &project,
        &out.workspace,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(again.status, Status::Ready, "{:?}", again.warnings);
    assert!(
        again
            .warnings
            .iter()
            .any(|w| w.contains("now uses db/other.sqlite3")),
        "{:?}",
        again.warnings
    );
    assert_eq!(again.receipt.services["app"].database, "db/other.sqlite3");
    // The old file stays where it was; the new one is the data's source again
    // (main has no file at the new path yet, so there is nothing to clone).
    assert!(db_in(&out.workspace).exists());
}

#[test]
fn a_reset_or_a_drop_after_the_configured_path_changed_acts_on_nothing() {
    let t = repo("");
    main_database(&t, 4, "delete");
    let out = create(&t.project, "bound", SetupOptions::default()).unwrap();
    let bound = db_in(&out.workspace);
    // Somebody's own file at the path the configuration will name.
    let other = out.workspace.path.join("db/other.sqlite3");
    std::fs::write(&other, "somebody's own data").unwrap();
    let text = std::fs::read_to_string(t.root.join("devflow.toml")).unwrap();
    std::fs::write(
        t.root.join("devflow.toml"),
        text.replace("db/app.sqlite3", "db/other.sqlite3"),
    )
    .unwrap();
    let project = Project::open(&t.root).unwrap();

    for what in ["reset", "drop"] {
        let error = match what {
            "reset" => setup::sqlite_reset(&project, &out.workspace, "app", None, None)
                .map(|_| ())
                .unwrap_err(),
            _ => setup::sqlite_drop(&project, &out.workspace, "app", None)
                .map(|_| ())
                .unwrap_err(),
        };
        let text = format!("{error:#}");
        assert!(text.contains("db/other.sqlite3"), "{what}: {text}");
        assert!(text.contains("db/app.sqlite3"), "{what}: {text}");
        assert!(
            remedies_of(&error)
                .iter()
                .any(|r| matches!(r, Remedy::Setup { .. })),
            "{what}: the way on is to bind the new path"
        );
    }
    // Neither file was touched, and the binding still names the file it made.
    assert_eq!(rows(&bound), 4);
    assert_eq!(std::fs::read(&other).unwrap(), b"somebody's own data");
    let receipt = setup::receipt(&project, &out.workspace).unwrap().unwrap();
    let binding = &receipt.services["app"];
    assert_eq!(binding.database, "db/app.sqlite3");
    assert!(binding.owned);
    assert!(receipt.creating.is_none());
}

#[cfg(unix)]
#[test]
fn a_database_reached_through_a_link_is_never_dropped_reset_or_snapshotted() {
    let t = repo("");
    let main = main_database(&t, 6, "delete");
    let out = create(&t.project, "linked", SetupOptions::default()).unwrap();
    // The branch ships `db -> <the main checkout's db>`: what the workspace
    // calls its database is the project's own.
    let dir = out.workspace.path.join("db");
    std::fs::remove_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(t.root.join("db"), &dir).unwrap();
    assert_eq!(rows(&main), 6);

    let before = names(&t.root.join("db"));
    let expect_refused = |error: anyhow::Error, what: &str| {
        let text = format!("{error:#}");
        assert!(text.contains("symbolic link"), "{what}: {text}");
    };
    expect_refused(
        setup::sqlite_drop(&t.project, &out.workspace, "app", None).unwrap_err(),
        "drop",
    );
    expect_refused(
        setup::sqlite_reset(&t.project, &out.workspace, "app", None, None).unwrap_err(),
        "reset",
    );
    expect_refused(
        setup::sqlite_snapshot(&t.project, &out.workspace, "app", "stolen", None).unwrap_err(),
        "snapshot",
    );
    // The main checkout's database is as it was, and nothing was recorded.
    assert_eq!(names(&t.root.join("db")), before);
    assert_eq!(rows(&main), 6);
    assert!(
        setup::sqlite_snapshots(&t.project, "app")
            .unwrap()
            .is_empty()
    );
    let receipt = setup::receipt(&t.project, &out.workspace).unwrap().unwrap();
    assert!(receipt.services["app"].owned, "the binding is as it was");

    // Removing the workspace does not follow the link either.
    let _ = setup::remove(
        &t.project,
        "linked",
        RemoveOptions {
            force: true,
            ..Default::default()
        },
        &mut NoProgress,
    );
    assert_eq!(rows(&main), 6, "the project's own database survives");
}

#[cfg(unix)]
#[test]
fn a_branch_that_ships_a_link_cannot_make_setup_write_through_it() {
    let t = repo("");
    let main = main_database(&t, 6, "delete");
    let outside = tempfile::tempdir().unwrap();
    let ws = worktree::create(&t.project, "ships-a-link", None).unwrap();
    // What the checkout of that branch contains: `db` points out of it.
    let dir = ws.path.join("db");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::os::unix::fs::symlink(outside.path(), &dir).unwrap();

    let outcome = setup::setup(
        &t.project,
        &ws,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    );
    match outcome {
        Err(error) => assert!(format!("{error:#}").contains("symbolic link"), "{error:#}"),
        Ok(out) => {
            let out = out.unwrap();
            assert_ne!(out.status, Status::Ready, "{:?}", out.warnings);
        }
    }
    assert!(
        std::fs::read_dir(outside.path()).unwrap().next().is_none(),
        "nothing was written through the link"
    );
    assert_eq!(rows(&main), 6);
}

#[test]
fn the_snapshots_of_a_removed_service_are_orphans_until_forgotten() {
    let t = repo("");
    main_database(&t, 2, "delete");
    let main_ws = default_workspace(&t.project);
    setup::sqlite_snapshot(&t.project, &main_ws, "app", "seed", None).unwrap();
    setup::sqlite_snapshot(&t.project, &main_ws, "app", "seed", None).unwrap();
    let store = Store::open(t.project.id(), "app").unwrap();
    let files = store.files_dir();
    assert_eq!(names(&files).len(), 2);
    // Configured: not an orphan, and forgetting it is refused.
    assert!(
        devflow_core::services::workspace::orphans(&t.project)
            .unwrap()
            .services
            .is_empty()
    );
    let error = devflow_core::services::workspace::forget(&t.project, "app").unwrap_err();
    assert!(
        format!("{error:#}").contains("still configured"),
        "{error:#}"
    );
    // The configuration drops the service: its snapshots are kept, reported
    // and forgotten only on request.
    let text = std::fs::read_to_string(t.root.join("devflow.toml")).unwrap();
    let without = text
        .lines()
        .take_while(|l| !l.starts_with("[services.app]"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(t.root.join("devflow.toml"), format!("{without}\n")).unwrap();
    let project = Project::open(&t.root).unwrap();
    let orphans = devflow_core::services::workspace::orphans(&project).unwrap();
    assert_eq!(orphans.services, ["app"]);
    assert_eq!(names(&files).len(), 2, "nothing was removed");
    let removed = devflow_core::services::workspace::forget(&project, "app").unwrap();
    assert_eq!(removed, ["2 snapshots of service 'app'"]);
    assert!(!files.exists());
    assert!(
        devflow_core::services::workspace::orphans(&project)
            .unwrap()
            .services
            .is_empty()
    );
    let error = devflow_core::services::workspace::forget(&project, "app").unwrap_err();
    assert!(
        format!("{error:#}").contains("records no service"),
        "{error:#}"
    );
}

#[test]
fn forgetting_refuses_a_path_or_a_service_nothing_records_before_any_effect() {
    let t = repo("");
    main_database(&t, 2, "delete");
    let main_ws = default_workspace(&t.project);
    setup::sqlite_snapshot(&t.project, &main_ws, "app", "seed", None).unwrap();
    let kept = Store::open(t.project.id(), "app").unwrap().files_dir();

    // Somebody's directory, with a file in it.
    let victim = tempfile::tempdir().unwrap();
    std::fs::write(victim.path().join("keep.txt"), "mine").unwrap();
    let absolute = victim.path().to_str().unwrap().to_string();
    let with_slash = format!("{absolute}/");
    let data_root =
        devflow_core::sqlite::store::root_of(&Store::open(t.project.id(), "app").unwrap())
            .to_path_buf();
    let parent = data_root.parent().unwrap().to_string_lossy().into_owned();
    for key in [
        absolute.as_str(),
        with_slash.as_str(),
        parent.as_str(),
        "..",
        "../sqlite",
        ".",
        "a/b",
        "",
        "App",
        "/",
    ] {
        let by_workspace = devflow_core::services::workspace::forget(&t.project, key);
        let by_setup = setup::sqlite_forget(&t.project, key);
        let by_store = Store::open(t.project.id(), key).map(|_| ());
        for error in [
            by_workspace.unwrap_err(),
            by_setup.unwrap_err(),
            by_store.unwrap_err(),
        ] {
            assert!(
                format!("{error:#}").contains("is not a service key"),
                "{key:?}: {error:#}"
            );
        }
    }
    assert_eq!(
        std::fs::read_to_string(victim.path().join("keep.txt")).unwrap(),
        "mine"
    );
    assert!(data_root.exists(), "the data directory is untouched");
    assert_eq!(names(&kept).len(), 1, "so are the snapshots");

    // A valid key nothing records: refused before any effect, and a directory
    // of that name is not touched.
    let ghost = Store::open(t.project.id(), "ghost").unwrap();
    std::fs::create_dir_all(ghost.files_dir()).unwrap();
    std::fs::write(ghost.files_dir().join("stranger.sqlite"), "not ours").unwrap();
    let error = devflow_core::services::workspace::forget(&t.project, "ghost").unwrap_err();
    assert!(
        format!("{error:#}").contains("records no service named 'ghost'"),
        "{error:#}"
    );
    let error = setup::sqlite_forget(&t.project, "ghost").unwrap_err();
    assert!(
        format!("{error:#}").contains("records no snapshot of service 'ghost'"),
        "{error:#}"
    );
    assert_eq!(
        std::fs::read_to_string(ghost.files_dir().join("stranger.sqlite")).unwrap(),
        "not ours"
    );
}

#[test]
fn recipes_build_a_frameworks_own_form_from_the_path() {
    let t = repo(
        "[env]\nSQLALCHEMY_DATABASE_URI = \"sqlite:///${SQLITE_PATH}\"\nPRISMA_URL = \"file:${SQLITE_PATH}\"\n",
    );
    main_database(&t, 2, "delete");
    let approval = devflow_core::trust::Approval::of(&devflow_core::trust::scope(&t.project));
    let out = create(
        &t.project,
        "recipes",
        SetupOptions {
            approval: Some(approval),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.warnings);
    let env = env_map(&out.workspace);
    let path = db_in(&out.workspace).display().to_string();
    assert_eq!(env["SQLALCHEMY_DATABASE_URI"], format!("sqlite:///{path}"));
    assert!(
        env["SQLALCHEMY_DATABASE_URI"].starts_with("sqlite:////"),
        "four slashes"
    );
    assert_eq!(env["PRISMA_URL"], format!("file:{path}"));
    // Not secrets: a path is not a credential.
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
}

#[test]
fn the_intent_a_reset_left_behind_is_settled_by_the_next_setup() {
    use devflow_core::receipt::PendingBranch;
    let t = repo("");
    main_database(&t, 3, "delete");
    let out = create(&t.project, "died-resetting", SetupOptions::default()).unwrap();
    let dest = db_in(&out.workspace);
    // A reset that died after journaling its intent and staging a file.
    let id = uuid::Uuid::new_v4();
    let mut receipt = setup::receipt(&t.project, &out.workspace).unwrap().unwrap();
    receipt.creating = Some(PendingBranch {
        service: "app".into(),
        branch_id: id,
        engine: Some(Engine::Sqlite),
        file: None,
    });
    receipt.save(&out.workspace.git_dir).unwrap();
    std::fs::write(staged_name(&dest, id), b"half a database").unwrap();
    // Setup again (the services phase reopened, as a configuration change
    // would): the file in place is kept, the leftover goes by its identity.
    let mut receipt = setup::receipt(&t.project, &out.workspace).unwrap().unwrap();
    receipt.set_phase(
        devflow_core::receipt::Phase::Services,
        devflow_core::receipt::PhaseState::Pending,
        None,
    );
    receipt.save(&out.workspace.git_dir).unwrap();
    let again = setup::setup(
        &t.project,
        &out.workspace,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(again.status, Status::Ready);
    assert!(again.receipt.creating.is_none());
    assert_eq!(names(dest.parent().unwrap()), ["app.sqlite3"]);
    assert_eq!(rows(&dest), 3);
}

#[test]
fn a_database_that_changed_hands_since_it_was_shown_is_not_touched() {
    use devflow_core::setup::BranchExpect;
    let t = repo("");
    main_database(&t, 3, "delete");
    let out = create(&t.project, "shown", SetupOptions::default()).unwrap();
    let binding = out.receipt.services["app"].clone();
    let file = db_in(&out.workspace);
    let shown = BranchExpect {
        instance: out.receipt.instance_id,
        branch: binding.branch_id.unwrap(),
        source: None,
    };
    // Another instance (a `setup --new-instance`) or another database: refused
    // before any effect, by the receipt as it is under the lock.
    for wrong in [
        BranchExpect {
            instance: uuid::Uuid::new_v4(),
            ..shown
        },
        BranchExpect {
            branch: uuid::Uuid::new_v4(),
            ..shown
        },
    ] {
        let error =
            setup::sqlite_reset(&t.project, &out.workspace, "app", None, Some(wrong)).unwrap_err();
        assert!(
            error
                .downcast_ref::<devflow_core::error::WorkspaceChanged>()
                .is_some(),
            "{error:#}"
        );
        let error = setup::sqlite_drop(&t.project, &out.workspace, "app", Some(wrong)).unwrap_err();
        assert!(
            error
                .downcast_ref::<devflow_core::error::WorkspaceChanged>()
                .is_some(),
            "{error:#}"
        );
        let error = setup::sqlite_snapshot(&t.project, &out.workspace, "app", "x", Some(wrong))
            .unwrap_err();
        assert!(
            error
                .downcast_ref::<devflow_core::error::WorkspaceChanged>()
                .is_some(),
            "{error:#}"
        );
    }
    assert_eq!(rows(&file), 3);
    assert!(
        setup::sqlite_snapshots(&t.project, "app")
            .unwrap()
            .is_empty()
    );
    // What was shown goes through.
    setup::sqlite_snapshot(&t.project, &out.workspace, "app", "x", Some(shown)).unwrap();
    setup::sqlite_reset(&t.project, &out.workspace, "app", None, Some(shown)).unwrap();
    assert!(setup::sqlite_drop(&t.project, &out.workspace, "app", Some(shown)).unwrap());
}

#[test]
fn a_reset_does_not_erase_the_unfinished_creation_of_another_service() {
    use devflow_core::receipt::PendingBranch;
    let t = repo("");
    main_database(&t, 3, "delete");
    let out = create(&t.project, "busy-slot", SetupOptions::default()).unwrap();
    let file = db_in(&out.workspace);
    let pending = PendingBranch {
        service: "cache".into(),
        branch_id: uuid::Uuid::new_v4(),
        engine: Some(Engine::Valkey),
        file: None,
    };
    let mut receipt = setup::receipt(&t.project, &out.workspace).unwrap().unwrap();
    receipt.creating = Some(pending.clone());
    receipt.save(&out.workspace.git_dir).unwrap();
    let error = setup::sqlite_reset(&t.project, &out.workspace, "app", None, None).unwrap_err();
    assert!(format!("{error:#}").contains("did not finish"), "{error:#}");
    assert!(
        remedies_of(&error)
            .iter()
            .any(|r| matches!(r, Remedy::Setup { .. }))
    );
    let after = setup::receipt(&t.project, &out.workspace).unwrap().unwrap();
    assert_eq!(
        after.creating,
        Some(pending),
        "the other intent is untouched"
    );
    assert_eq!(rows(&file), 3);
}

#[test]
fn a_branch_cannot_move_a_sqlite_file_or_rename_its_variables() {
    let t = repo("");
    main_database(&t, 4, "delete");
    let out = create(&t.project, "branchy", SetupOptions::default()).unwrap();
    let file = db_in(&out.workspace);
    assert_eq!(rows(&file), 4);
    // The branch's own devflow.toml names another file and other variables for
    // the service; a sqlite service is a shared `[services.*]` section, so
    // the main checkout's applies and the difference is said, never applied.
    let text = std::fs::read_to_string(t.root.join("devflow.toml")).unwrap();
    std::fs::write(
        out.workspace.path.join("devflow.toml"),
        text.replace("db/app.sqlite3", "elsewhere/other.sqlite3")
            .replace(
                "engine = \"sqlite\"\n",
                "engine = \"sqlite\"\nenv = \"BRANCH_URL\"\n",
            ),
    )
    .unwrap();
    let at = t.project.at(&out.workspace.path);
    let (key, section) = at.config.sqlite_services().next().unwrap();
    assert_eq!((key, section.path.as_str()), ("app", "db/app.sqlite3"));
    assert_eq!(at.config.url_variable("app"), Some("DATABASE_URL"));
    assert!(
        at.effective()
            .differing
            .iter()
            .any(|d| d.section == "[services.app]"),
        "{:?}",
        at.effective().differing
    );
    let again = setup::activate(
        &t.project,
        &out.workspace,
        SetupOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(again.status, Status::Ready, "{:?}", again.warnings);
    assert_eq!(again.receipt.services["app"].database, "db/app.sqlite3");
    assert!(!out.workspace.path.join("elsewhere").exists());
    let env = env_map(&out.workspace);
    assert_eq!(env["SQLITE_PATH"], file.display().to_string());
    assert!(!env.contains_key("BRANCH_URL"));
    assert_eq!(rows(&file), 4);
}

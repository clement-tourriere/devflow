//! Projects that cannot be opened as they are: migrating an earlier build's
//! configuration without losing the server, and mending the project list.
//! Everything runs against scratch directories (`DEVFLOW_CONFIG_DIR`,
//! `DEVFLOW_DATA_DIR`); no Docker is needed.
use devflow_core::Project;
use devflow_core::config::Config;
use devflow_core::config_edit::migrate;
use devflow_core::db::Server;
use devflow_core::health::{self, Health, ProblemKind};
use devflow_core::receipt::{Phase, Receipt};
use devflow_core::registry::{self, Entry};
use devflow_core::remedy::{Problem, Remedy, remedies_of};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use uuid::Uuid;

/// One scratch config and data directory for the process; tests take the
/// lock because the registry is shared.
fn scratch() -> (MutexGuard<'static, ()>, &'static Path) {
    static LOCK: Mutex<()> = Mutex::new(());
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = DIR.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: set once, before any test body reads the environment; the
        // lock above serializes the tests that use them.
        unsafe {
            std::env::set_var("DEVFLOW_CONFIG_DIR", dir.path().join("config"));
            std::env::set_var("DEVFLOW_DATA_DIR", dir.path().join("data"));
        }
        dir
    });
    (guard, dir.path())
}

fn repo(parent: &Path, name: &str, config: &str) -> PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(&root).unwrap();
    git2::Repository::init(&root).unwrap();
    std::fs::write(root.join("devflow.toml"), config).unwrap();
    root.canonicalize().unwrap()
}

fn pre_release(id: Uuid, name: &str) -> String {
    format!(
        "[project]\nname = \"{name}\"\nid = \"{id}\"\n\n[postgres]\nmode = \"managed\"\nimage = \"postgres:18\"\n"
    )
}

#[test]
fn migrating_keeps_the_server_its_identity_and_the_workspaces_receipts() {
    let (_lock, base) = scratch();
    let id = Uuid::new_v4();
    let root = repo(base, "tt2", &pre_release(id, "tt2"));

    // What the earlier build identified the server by: the project id and
    // name, never a service key.
    let id8 = &id.simple().to_string()[..8];
    let wanted_dir = base.join("data").join("postgres").join(id.to_string());
    let wanted_compose = format!("devflow-tt2-{id8}");

    // The earlier configuration is refused, and the problem says it can be mended.
    let refused = Project::open(&root).unwrap_err();
    assert!(
        remedies_of(&refused)
            .iter()
            .any(|r| matches!(r, Remedy::MigrateConfig { .. })),
        "{refused:#}"
    );

    let migration = migrate::migrate(&root).unwrap();
    assert_eq!(migration.service_key, "db");

    let project = Project::open(&root).unwrap();
    assert_eq!(project.id(), id);
    let server = Server::for_project(&project).unwrap().unwrap();
    let managed = server.managed().unwrap();
    assert_eq!(managed.dir, wanted_dir);
    assert_eq!(managed.compose_project, wanted_compose);

    // Any other key would have named the same server: the key is only a label.
    let mut renamed = project.clone();
    let (_, section) = project.config.postgres().unwrap();
    let section = section.clone();
    renamed.config.services.clear();
    renamed.config.set_postgres("main", section);
    let other = Server::for_project(&renamed).unwrap().unwrap();
    assert_eq!(other.managed().unwrap().dir, wanted_dir);
    assert_eq!(other.managed().unwrap().compose_project, wanted_compose);

    // A version 1 receipt written by that build binds to the migrated service.
    let git_dir = project.git_common_dir.clone();
    let mut receipt = Receipt::new(id, "feature", Some("main".into()));
    receipt.set_phase(
        Phase::Services,
        devflow_core::receipt::PhaseState::Done,
        None,
    );
    let mut json = serde_json::to_value(&receipt).unwrap();
    let object = json.as_object_mut().unwrap();
    object.insert("version".into(), 1.into());
    let phases = object["phases"].as_object_mut().unwrap();
    let record = phases.remove("services").unwrap();
    phases.insert("database".into(), record);
    object.insert(
        "data".into(),
        serde_json::json!({
            "branch_id": Uuid::new_v4(),
            "database": "df_tt2_feature",
            "role": "dfl_feature",
            "url": "postgres://dfl_feature:pw@127.0.0.1:20001/df_tt2_feature", // kingfisher:ignore
            "source": "main",
            "owned": true,
        }),
    );
    std::fs::create_dir_all(Receipt::path(&git_dir).parent().unwrap()).unwrap();
    std::fs::write(Receipt::path(&git_dir), serde_json::to_vec(&json).unwrap()).unwrap();
    let mut loaded = Receipt::load(&git_dir).unwrap().unwrap();
    assert_eq!(loaded.version, 1);
    loaded.migrate(&project.config);
    assert!(loaded.unmapped.is_none(), "the binding is not orphaned");
    let binding = loaded.services.get("db").expect("bound to services.db");
    assert_eq!(binding.database, "df_tt2_feature");
    assert_eq!(binding.role, "dfl_feature");
}

#[test]
fn the_project_list_can_be_mended_without_touching_the_projects() {
    let (_lock, base) = scratch();
    let id = Uuid::new_v4();
    let config = format!("[project]\nname = \"blog\"\nid = \"{id}\"\n");
    let old = repo(base, "old-place", &config);
    let entry = Entry {
        id,
        name: "blog".into(),
        root: old.clone(),
    };
    // Register it the way the app does (the project is healthy there).
    registry::add(&Project::open(&old).unwrap()).unwrap();
    assert_eq!(health::check(&entry), Health::Ok);

    // The folder moves.
    let new = base.join("new-place");
    std::fs::rename(&old, &new).unwrap();
    let new = new.canonicalize().unwrap();
    let problem = health::check(&entry).problem().unwrap();
    assert_eq!(problem.kind, ProblemKind::FolderMissing);
    assert!(
        problem
            .remedies
            .iter()
            .any(|r| matches!(r, Remedy::LocateProject { .. }))
    );

    // Another project's folder is not this project.
    let other = repo(
        base,
        "someone-else",
        &format!("[project]\nname = \"x\"\nid = \"{}\"\n", Uuid::new_v4()),
    );
    let refused = registry::relocate(id, &other).unwrap_err();
    assert!(refused.downcast_ref::<Problem>().is_some(), "{refused:#}");
    assert!(!refused.to_string().contains('`'), "{refused:#}");
    // A folder that is not a repository is refused too.
    assert!(registry::relocate(id, &base.join("nothing")).is_err());

    // Locating accepts any folder inside the repository and finds its main checkout.
    std::fs::create_dir_all(new.join("src")).unwrap();
    let located = registry::relocate(id, &new.join("src")).unwrap();
    assert_eq!(located.root, new);
    assert_eq!(located.id, id);
    let listed: Vec<Entry> = registry::list().unwrap();
    assert_eq!(listed.iter().filter(|e| e.id == id).count(), 1);
    assert_eq!(registry::find(&id.to_string()).unwrap().root, new);
    assert_eq!(registry::find(new.to_str().unwrap()).unwrap().id, id);
    assert!(registry::find("/no/such/project").is_err());

    // Missing folders go in one step; a folder that exists stays.
    let gone_id = Uuid::new_v4();
    let gone = repo(
        base,
        "gone",
        &format!("[project]\nname = \"gone\"\nid = \"{gone_id}\"\n"),
    );
    registry::add(&Project::open(&gone).unwrap()).unwrap();
    std::fs::remove_dir_all(&gone).unwrap();
    let pruned = registry::prune_missing().unwrap();
    assert_eq!(pruned.iter().map(|e| e.id).collect::<Vec<_>>(), [gone_id]);
    assert!(registry::list().unwrap().iter().any(|e| e.id == id));

    // Forgetting removes the entry and nothing else.
    assert!(registry::remove(&new).unwrap());
    assert!(!registry::remove(&new).unwrap());
    assert!(new.join("devflow.toml").is_file());
    assert!(Config::find_file(&new).is_some());
}

#[test]
fn locating_finds_a_project_whose_configuration_needs_migrating() {
    let (_lock, base) = scratch();
    let id = Uuid::new_v4();
    let healthy = format!("[project]\nname = \"needs\"\nid = \"{id}\"\n");
    let root = repo(base, "needs-migration", &healthy);
    registry::add(&Project::open(&root).unwrap()).unwrap();
    std::fs::write(root.join("devflow.toml"), pre_release(id, "needs")).unwrap();
    assert!(Project::open(&root).is_err());
    // The configuration is refused, yet the identity is readable, so the
    // folder is recognised as the project.
    let located = registry::relocate(id, &root).unwrap();
    assert_eq!(located.root, root);
    assert_eq!(located.name, "needs");
    registry::remove(&root).unwrap();
}

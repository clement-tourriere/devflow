//! The workspace ledger through the public lifecycle: what a record keeps when
//! Git or an agent host removes a worktree, and what a removal does to it.
//!
//! No test here needs Docker: the services are an external server at a closed
//! port (nothing can be asked of it, which is the point) and a fake pitchfork.
use devflow_core::config::Engine;
use devflow_core::error::{PartialRemoval, Protected};
use devflow_core::gc::{self, Class};
use devflow_core::ledger::{self, State};
use devflow_core::receipt::{DataBinding, PendingBranch, Receipt, ServiceBinding};
use devflow_core::setup::{
    self, CreateRequest, NoProgress, RemoveOptions, SetupOptions, SetupOutcome,
};
use devflow_core::worktree::{self, Workspace};
use devflow_core::{Project, trust};
use std::path::PathBuf;
use std::sync::{Once, OnceLock};
use uuid::Uuid;

fn init() {
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
}

fn quiet_pitchfork() -> devflow_core::pitchfork::Pitchfork {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = DIR.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("pitchfork");
        std::fs::write(
            &script,
            include_str!("../../../tests/support/fake-pitchfork.sh"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir
    });
    devflow_core::pitchfork::Pitchfork::at(dir.path().join("pitchfork"))
}

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    project: Project,
}

const UNREACHABLE: &str = "\n[services.db]\nengine = \"postgres\"\nmode = \"external\"\n\
                           admin_url = \"postgres://prov:pw@127.0.0.1:1/postgres\"\n";

fn repo(teardown: &[&str], extra: &str) -> Repo {
    init();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("shop");
    std::fs::create_dir_all(&root).unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.initial_head("main");
    let git = git2::Repository::init_opts(&root, &options).unwrap();
    let teardown: Vec<String> = teardown.iter().map(|c| format!("'{c}'")).collect();
    std::fs::write(
        root.join("devflow.toml"),
        format!(
            "[project]\nname = \"shop\"\nid = \"{}\"\n\n[hooks]\nteardown = [{}]\n{extra}",
            Uuid::new_v4(),
            teardown.join(", ")
        ),
    )
    .unwrap();
    std::fs::write(root.join(".gitignore"), ".env\ndevflow.local.toml\n").unwrap();
    std::fs::write(root.join("README.md"), "shop\n").unwrap();
    let mut index = git.index().unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    git.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
        .unwrap();
    let project = Project::open(&root).unwrap();
    Repo {
        _dir: dir,
        root,
        project,
    }
}

fn create(t: &Repo, name: &str) -> SetupOutcome {
    setup::create(
        &t.project,
        CreateRequest {
            name: name.into(),
            from: None,
            base: None,
            options: SetupOptions {
                approval: Some(trust::Approval::of(&trust::scope(&t.project))),
                ..SetupOptions::default()
            },
        },
        &mut NoProgress,
    )
    .unwrap()
}

fn record_of(t: &Repo, ws: &Workspace) -> Option<ledger::Record> {
    let receipt = Receipt::load(&ws.git_dir).ok().flatten()?;
    ledger::read(&t.project.git_common_dir, receipt.instance_id).unwrap()
}

fn files(t: &Repo) -> Vec<String> {
    let dir = ledger::dir(&t.project.git_common_dir);
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn remove(t: &Repo, name: &str, options: RemoveOptions) -> anyhow::Result<setup::RemoveOutcome> {
    setup::remove(
        &t.project,
        name,
        RemoveOptions {
            pitchfork: Some(quiet_pitchfork()),
            ..options
        },
        &mut NoProgress,
    )
}

/// A workspace whose receipt owns a database branch on a server that cannot be
/// reached, as a setup would have left it. The server is configured after the
/// workspace exists (creating it would have needed the server).
fn with_unreachable_branch(t: &mut Repo, name: &str, pending: bool) -> (Workspace, Uuid) {
    let ws = create(t, name).workspace;
    let config = t.root.join("devflow.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}{UNREACHABLE}")).unwrap();
    t.project = Project::open(&t.root).unwrap();
    let mut receipt = Receipt::load(&ws.git_dir).unwrap().unwrap();
    let branch = Uuid::new_v4();
    if pending {
        receipt.creating = Some(PendingBranch {
            service: "db".into(),
            branch_id: branch,
            engine: Some(Engine::Postgres),
            file: None,
        });
    } else {
        receipt.services.insert(
            "db".into(),
            ServiceBinding::postgres(
                DataBinding {
                    branch_id: Some(branch),
                    database: "df_x".into(),
                    role: "role".into(),
                    url: "postgres://role@127.0.0.1:1/df_x".into(),
                    source: "empty".into(),
                    owned: true,
                },
                None,
            ),
        );
    }
    receipt.save(&ws.git_dir).unwrap();
    (ws, branch)
}

#[test]
fn setting_a_workspace_up_writes_its_record_before_anything_else_is_provisioned() {
    let t = repo(&[], "");
    let ws = create(&t, "feature/a").workspace;
    let receipt = Receipt::load(&ws.git_dir).unwrap().unwrap();
    let record = record_of(&t, &ws).expect("a record");
    assert_eq!(record.instance_id, receipt.instance_id);
    assert_eq!(record.workspace, "feature/a");
    assert_eq!(record.state, State::Active);
    assert_eq!(record.worktree, ws.path);
    assert!(record.last_active_at.is_none(), "created, not entered");

    // Entering it (activation) sets when it was last active.
    let entered = setup::activate(
        &t.project,
        &ws,
        SetupOptions {
            pitchfork: Some(quiet_pitchfork()),
            ..SetupOptions::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert!(entered.receipt.last_active_at.is_some());
    let record = record_of(&t, &ws).unwrap();
    assert_eq!(record.last_active_at, entered.receipt.last_active_at);
}

#[test]
fn a_removal_that_accounts_for_everything_deletes_the_record_and_its_lock_files() {
    let t = repo(&[], "");
    let ws = create(&t, "done").workspace;
    assert_eq!(
        files(&t).len(),
        3,
        "the record, its lock and the instance lock: {:?}",
        files(&t)
    );
    let outcome = remove(&t, "done", RemoveOptions::default()).unwrap();
    assert!(outcome.still_recorded.is_empty());
    assert!(!ws.path.exists());
    assert!(files(&t).is_empty(), "{:?}", files(&t));
}

#[test]
fn data_the_person_keeps_is_not_offered_for_clean_up_later() {
    let mut t = repo(&[], "");
    let (ws, branch) = with_unreachable_branch(&mut t, "kept", false);
    assert!(
        record_of(&t, &ws)
            .unwrap()
            .resources
            .branches
            .iter()
            .any(|b| b.id == branch)
    );
    let outcome = remove(
        &t,
        "kept",
        RemoveOptions {
            keep_data: true,
            ..RemoveOptions::default()
        },
    )
    .unwrap();
    assert!(outcome.still_recorded.is_empty(), "{outcome:?}");
    assert!(
        files(&t).is_empty(),
        "the data is the person's: {:?}",
        files(&t)
    );
}

#[test]
fn a_forced_removal_that_cannot_ask_the_server_leaves_the_branch_named_for_gc() {
    let mut t = repo(&[], "");
    let (ws, branch) = with_unreachable_branch(&mut t, "stuck", false);

    // Without force the removal stops before any effect, and says force leaves it.
    let error = remove(&t, "stuck", RemoveOptions::default()).unwrap_err();
    let refusal = error.downcast_ref::<Protected>().unwrap();
    assert!(refusal.forceable, "{refusal}");
    assert!(ws.path.exists());

    let outcome = remove(
        &t,
        "stuck",
        RemoveOptions {
            force: true,
            ..RemoveOptions::default()
        },
    )
    .unwrap();
    assert!(
        !ws.path.exists() && !ws.git_dir.exists(),
        "the worktree and receipt are gone"
    );
    assert_eq!(outcome.still_recorded.len(), 1, "{outcome:?}");
    assert!(outcome.still_recorded[0].contains(&branch.to_string()));
    assert!(
        outcome
            .warnings
            .iter()
            .any(|w| w.contains("stays recorded")),
        "{:?}",
        outcome.warnings
    );

    // The record survived the receipt: it names the branch, removing.
    let records = ledger::list(&t.project.git_common_dir).unwrap().records;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, State::Removing);
    assert_eq!(records[0].resources.branches[0].id, branch);
    // And gc sees it (the server still cannot be asked, so it is unknown and
    // not removable until it can).
    let inventory = gc::inventory(
        &t.project,
        &gc::Options {
            measure: false,
            evidence: false,
            backfill: false,
        },
    )
    .unwrap();
    let entry = inventory.entry(records[0].instance_id).unwrap();
    assert_eq!(entry.class, Class::Unknown);
    assert!(!entry.removable);
    assert!(
        gc::summary(&t.project)
            .unwrap()
            .removed_elsewhere
            .contains(&"stuck".to_string())
    );
}

#[test]
fn a_branch_recorded_before_its_creation_that_cannot_be_looked_for_is_forceable_now() {
    let mut t = repo(&[], "");
    let (ws, branch) = with_unreachable_branch(&mut t, "pending", true);
    assert!(
        record_of(&t, &ws).unwrap().resources.branches[0].pending,
        "recorded before the creation"
    );
    let error = remove(&t, "pending", RemoveOptions::default()).unwrap_err();
    let refusal = error.downcast_ref::<Protected>().unwrap();
    assert!(
        refusal.forceable,
        "the record survives the receipt: {refusal}"
    );
    assert!(refusal.reason.contains(&branch.to_string()));

    let outcome = remove(
        &t,
        "pending",
        RemoveOptions {
            force: true,
            ..RemoveOptions::default()
        },
    )
    .unwrap();
    assert!(!ws.path.exists());
    assert_eq!(outcome.still_recorded.len(), 1, "{outcome:?}");
    let records = ledger::list(&t.project.git_common_dir).unwrap().records;
    assert!(records[0].resources.branches[0].pending);
    assert_eq!(records[0].resources.branches[0].id, branch);
}

#[test]
fn a_removal_that_stops_part_way_leaves_the_record_removing_and_setup_ends_it() {
    let t = repo(&["exit 1"], "");
    let ws = create(&t, "halfway").workspace;
    trust::approve(&t.project).unwrap();
    let error = remove(&t, "halfway", RemoveOptions::default()).unwrap_err();
    assert!(
        error.downcast_ref::<PartialRemoval>().is_some(),
        "{error:#}"
    );
    let record = record_of(&t, &ws).unwrap();
    assert_eq!(record.state, State::Removing);
    assert!(
        record
            .journal
            .iter()
            .any(|entry| entry.effect == ledger::Effect::Teardown),
        "{:?}",
        record.journal
    );

    // gc does not touch it: its worktree is still there.
    let inventory = gc::inventory(
        &t.project,
        &gc::Options {
            measure: false,
            evidence: false,
            backfill: false,
        },
    )
    .unwrap();
    let entry = inventory.entry(record.instance_id).unwrap();
    assert_eq!(
        (entry.class, entry.removing, entry.removable),
        (Class::Active, true, false)
    );

    // Setting it up again is no removal.
    setup::setup(
        &t.project,
        &ws,
        SetupOptions {
            approval: Some(trust::Approval::of(&trust::scope(&t.project))),
            pitchfork: Some(quiet_pitchfork()),
            ..SetupOptions::default()
        },
        false,
        false,
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(record_of(&t, &ws).unwrap().state, State::Active);
}

#[test]
fn a_new_instance_forgets_what_the_old_identity_owned() {
    let t = repo(&[], "");
    let ws = create(&t, "forked").workspace;
    let old = Receipt::load(&ws.git_dir).unwrap().unwrap().instance_id;
    setup::setup(
        &t.project,
        &ws,
        SetupOptions {
            pitchfork: Some(quiet_pitchfork()),
            ..SetupOptions::default()
        },
        false,
        true,
        &mut NoProgress,
    )
    .unwrap();
    let new = Receipt::load(&ws.git_dir).unwrap().unwrap().instance_id;
    assert_ne!(old, new);
    assert!(
        ledger::read(&t.project.git_common_dir, old)
            .unwrap()
            .is_none(),
        "a copied record would let a clean-up drop what the original owns"
    );
    assert!(
        ledger::read(&t.project.git_common_dir, new)
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_worktree_git_removed_is_cleaned_up_by_gc_and_then_the_record_is_gone() {
    let t = repo(&[], "");
    let ws = create(&t, "agent-made").workspace;
    let instance = Receipt::load(&ws.git_dir).unwrap().unwrap().instance_id;
    // What an agent host does: git worktree remove (and prune), nothing else.
    worktree::force_remove(&t.project, &ws).unwrap();
    assert!(!ws.git_dir.exists());
    assert_eq!(
        files(&t).len(),
        3,
        "the record (with its two locks) outlived the receipt: {:?}",
        files(&t)
    );

    let quick = gc::Options {
        measure: false,
        evidence: false,
        backfill: false,
    };
    let inventory = gc::inventory(&t.project, &quick).unwrap();
    let entry = inventory.entry(instance).unwrap();
    assert_eq!(entry.class, Class::RemovedElsewhere);
    assert!(entry.removable);
    // Nothing owned: it is not counted as something still holding resources.
    assert!(gc::summary(&t.project).unwrap().is_empty());

    let outcome = gc::remove(
        &t.project,
        &gc::RemoveRequest {
            targets: vec![entry.target().unwrap()],
            pitchfork: Some(quiet_pitchfork()),
            ..gc::RemoveRequest::default()
        },
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(outcome.removed.len(), 1, "{:?}", outcome.skipped);
    assert!(files(&t).is_empty(), "{:?}", files(&t));
    assert!(
        gc::inventory(&t.project, &quick)
            .unwrap()
            .entry(instance)
            .is_none()
    );
}

#[test]
fn a_clean_up_gives_the_app_port_back_as_a_removal_does() {
    let t = repo(&[], "");
    let quick = gc::Options {
        measure: false,
        evidence: false,
        backfill: false,
    };
    let reserved = |instance: Uuid| {
        devflow_core::ports::reserved()
            .iter()
            .any(|(_, _, held)| *held == instance)
    };

    // An ordinary removal gives the port back.
    let kept = create(&t, "removed-here");
    let kept_instance = Receipt::load(&kept.workspace.git_dir)
        .unwrap()
        .unwrap()
        .instance_id;
    assert!(reserved(kept_instance), "the workspace holds a port");
    remove(&t, "removed-here", RemoveOptions::default()).unwrap();
    assert!(!reserved(kept_instance));

    // A workspace Git forgot: the record carries the port, and the clean-up
    // gives it back.
    let ws = create(&t, "agent-made").workspace;
    let receipt = Receipt::load(&ws.git_dir).unwrap().unwrap();
    let port = receipt.app_port.expect("a port was chosen");
    assert!(reserved(receipt.instance_id));
    worktree::force_remove(&t.project, &ws).unwrap();
    // (The machine's port ledger already ignores an entry whose directory is
    // gone; giving the port back is still the clean-up's part, as it is a
    // removal's.)
    assert_eq!(
        record_by(&t, receipt.instance_id).app_port,
        Some(port),
        "the record carries it"
    );
    let inventory = gc::inventory(&t.project, &quick).unwrap();
    let entry = inventory.entry(receipt.instance_id).unwrap();
    assert_eq!(entry.app_port, Some(port));

    let outcome = gc::remove(
        &t.project,
        &gc::RemoveRequest {
            targets: vec![entry.target().unwrap()],
            pitchfork: Some(quiet_pitchfork()),
            ..gc::RemoveRequest::default()
        },
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(outcome.removed.len(), 1, "{:?}", outcome.skipped);
    assert!(!reserved(receipt.instance_id), "the port went back");
}

fn record_by(t: &Repo, instance: Uuid) -> ledger::Record {
    ledger::read(&t.project.git_common_dir, instance)
        .unwrap()
        .expect("a record")
}

#[test]
fn releasing_an_instance_frees_its_ports_by_project_and_instance_and_no_other() {
    let t = repo(&[], "");
    let one = create(&t, "one").workspace;
    let two = create(&t, "two").workspace;
    let instance = |w: &Workspace| Receipt::load(&w.git_dir).unwrap().unwrap().instance_id;
    let (one_id, two_id) = (instance(&one), instance(&two));
    let held = |id: Uuid| {
        devflow_core::ports::reserved()
            .iter()
            .any(|(_, _, held)| *held == id)
    };
    assert!(held(one_id) && held(two_id));
    // Another project's release with the same instance id frees nothing.
    devflow_core::ports::release_instance(Uuid::new_v4(), one_id);
    assert!(held(one_id));
    // The directory is still there, so the ledger would keep the entry: only
    // the release frees it, by project and instance.
    devflow_core::ports::release_instance(t.project.id(), one_id);
    assert!(!held(one_id) && held(two_id));
}

/// Read-only directories do not stop the superuser: a test about a ledger that
/// cannot be written has nothing to say then.
#[cfg(unix)]
fn unwritable(dir: &std::path::Path) -> Option<impl Drop> {
    use std::os::unix::fs::PermissionsExt;
    struct Restore(std::path::PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let guard = Restore(dir.to_path_buf());
    std::fs::File::create(dir.join(".probe"))
        .is_err()
        .then_some(guard)
}

#[test]
#[cfg(unix)]
fn a_ledger_that_cannot_be_written_refuses_even_a_forced_removal_before_the_receipt_goes() {
    let mut t = repo(&[], "");
    let (ws, branch) = with_unreachable_branch(&mut t, "older", false);
    // An older workspace: no record yet, and the ledger cannot be written.
    let records = ledger::dir(&t.project.git_common_dir);
    std::fs::remove_dir_all(&records).unwrap();
    std::fs::create_dir_all(&records).unwrap();
    let Some(_guard) = unwritable(&records) else {
        return;
    };
    for force in [false, true] {
        let error = remove(
            &t,
            "older",
            RemoveOptions {
                force,
                ..RemoveOptions::default()
            },
        )
        .unwrap_err();
        let protected = error
            .downcast_ref::<Protected>()
            .unwrap_or_else(|| panic!("{error:#}"));
        assert!(!protected.forceable, "force={force}: {}", protected.reason);
        assert!(protected.reason.contains("record"), "{}", protected.reason);
        // Nothing was lost: the worktree and the only receipt that names the
        // database branch are still there.
        assert!(ws.path.is_dir() && Receipt::path(&ws.git_dir).is_file());
        let receipt = Receipt::load(&ws.git_dir).unwrap().unwrap();
        assert_eq!(receipt.services["db"].branch_id, Some(branch));
    }
}

#[test]
fn nothing_is_written_into_a_git_directory_git_removed_so_provisioning_cannot_go_on_unrecorded() {
    let t = repo(&[], "");
    let ws = create(&t, "taken").workspace;
    let receipt = Receipt::load(&ws.git_dir).unwrap().unwrap();
    // The host removes the worktree while it is being set up.
    worktree::force_remove(&t.project, &ws).unwrap();
    assert!(!ws.git_dir.exists());

    let error = receipt.save(&ws.git_dir).unwrap_err();
    assert!(format!("{error:#}").contains("is gone"), "{error:#}");
    assert!(
        Receipt::try_lock(&ws.git_dir).is_err() && Receipt::try_process_lock(&ws.git_dir).is_err()
    );
    assert!(!ws.git_dir.exists(), "no stray directory was brought back");

    // Something recreates the directory without Git's files in it: it is where
    // a worktree's administrative files belong, so the receipt is not written
    // unrecorded either.
    std::fs::create_dir_all(ws.git_dir.join("devflow")).unwrap();
    let error = receipt.save(&ws.git_dir).unwrap_err();
    assert!(
        format!("{error:#}").contains("administrative files"),
        "{error:#}"
    );
    assert!(!Receipt::path(&ws.git_dir).exists());
}

#[test]
fn if_new_gives_a_ready_workspace_without_a_record_one_and_provisions_nothing() {
    let t = repo(&[], "");
    let ws = create(&t, "upgraded").workspace;
    let receipt = Receipt::load(&ws.git_dir).unwrap().unwrap();
    let file = ledger::path(&t.project.git_common_dir, receipt.instance_id);
    // A workspace set up before records existed.
    std::fs::remove_file(&file).unwrap();
    let before = std::fs::read(Receipt::path(&ws.git_dir)).unwrap();
    let options = || SetupOptions {
        pitchfork: Some(quiet_pitchfork()),
        ..SetupOptions::default()
    };
    let if_new = |options: SetupOptions| {
        setup::setup(&t.project, &ws, options, true, false, &mut NoProgress).unwrap()
    };

    // Another run holds the lock: the setup is skipped as before, nothing waits.
    {
        let _held = Receipt::try_lock(&ws.git_dir).unwrap().unwrap();
        assert!(if_new(options()).is_none());
        assert!(
            !file.exists(),
            "that run writes the receipt, and the record"
        );
    }

    // Skipped, and the record is written first.
    assert!(if_new(options()).is_none(), "ready: nothing to set up");
    let record = ledger::read(&t.project.git_common_dir, receipt.instance_id)
        .unwrap()
        .expect("the record was written");
    assert_eq!(record.workspace, "upgraded");
    assert_eq!(record.worktree, ws.path);
    assert_eq!(
        std::fs::read(Receipt::path(&ws.git_dir)).unwrap(),
        before,
        "the receipt is not touched"
    );

    // A worktree host then deletes the worktree: the record is what remains.
    worktree::force_remove(&t.project, &ws).unwrap();
    let kept = ledger::read(&t.project.git_common_dir, receipt.instance_id)
        .unwrap()
        .unwrap();
    assert_eq!(kept.instance_id, receipt.instance_id);
}

#[test]
#[cfg(unix)]
fn data_the_person_kept_is_still_kept_when_the_removal_stopped_halfway_and_is_resumed() {
    use devflow_core::error::PartialRemoval;
    let mut t = repo(&[], "");
    let (ws, branch) = with_unreachable_branch(&mut t, "kept", false);
    let instance = Receipt::load(&ws.git_dir).unwrap().unwrap().instance_id;

    // `remove --keep-data` stops after the journal and before the worktree is
    // gone (its folder cannot be changed).
    let parent = ws.path.parent().unwrap().to_path_buf();
    let Some(guard) = unwritable(&parent) else {
        return;
    };
    let error = remove(
        &t,
        "kept",
        RemoveOptions {
            keep_data: true,
            ..RemoveOptions::default()
        },
    )
    .unwrap_err();
    assert!(
        error.downcast_ref::<PartialRemoval>().is_some(),
        "{error:#}"
    );
    drop(guard);
    let record = ledger::read(&t.project.git_common_dir, instance)
        .unwrap()
        .unwrap();
    assert!(record.resources.branches.is_empty(), "not listed as owned");
    assert_eq!(
        record.resources.disowned,
        [branch],
        "and remembered as kept"
    );
    // The receipt that survives still names the branch as owned.
    assert_eq!(
        Receipt::load(&ws.git_dir).unwrap().unwrap().services["db"].branch_id,
        Some(branch)
    );

    // The directory is gone (the crash came after that, before the registration
    // was pruned); gc sees nothing of the data...
    std::fs::remove_dir_all(&ws.path).unwrap();
    let inventory = gc::inventory(
        &t.project,
        &gc::Options {
            measure: false,
            evidence: false,
            backfill: false,
        },
    )
    .unwrap();
    let entry = inventory.entry(instance).unwrap();
    assert_eq!(entry.class, Class::Missing);
    assert!(entry.branches.is_empty());

    // ...and the ordinary removal that finishes it, without the flag, syncs the
    // receipt back and plans the branch no more: it would have refused (the
    // server cannot be asked) or dropped it.
    let outcome = remove(&t, "kept", RemoveOptions::default()).unwrap();
    assert!(outcome.still_recorded.is_empty(), "{outcome:?}");
    assert!(
        outcome
            .warnings
            .iter()
            .any(|w| w.contains("kept at your request")),
        "{:?}",
        outcome.warnings
    );
    assert!(files(&t).is_empty(), "{:?}", files(&t));
}

/// A `remove --keep-data` that failed part-way leaves the workspace in use: its
/// receipt is still ready and still names the data as owned. The host's later
/// `setup --if-new` stopped at "ready" and never reached the place that gives
/// the data back, so the workspace went on using branches that a later removal
/// and a clean-up excluded for good.
#[test]
fn if_new_gives_data_a_failed_keep_data_removal_disowned_back_to_a_workspace_in_use() {
    let t = repo(&[], "");
    let ws = create(&t, "reused").workspace;
    let mut receipt = Receipt::load(&ws.git_dir).unwrap().unwrap();
    let branch = Uuid::new_v4();
    // The receipt names a branch as owned (a database an earlier build bound).
    receipt.unmapped = Some(DataBinding {
        branch_id: Some(branch),
        database: "df_x".into(),
        role: "role".into(),
        url: "postgres://role@127.0.0.1:1/df_x".into(),
        source: "empty".into(),
        owned: true,
    });
    receipt.save(&ws.git_dir).unwrap();
    let instance = receipt.instance_id;
    let handle = ledger::Handle::new(&t.project.git_common_dir, instance);
    assert_eq!(handle.read().unwrap().unwrap().resources.branches.len(), 1);

    // The removal got as far as the record: kept data, `removing`...
    handle.begin(None).unwrap();
    handle.disown_branches(&[branch]).unwrap();
    let stuck = handle.read().unwrap().unwrap();
    assert_eq!(stuck.state, State::Removing);
    assert!(stuck.resources.branches.is_empty() && stuck.resources.disowned == [branch]);
    // ...and stopped, with the receipt (and the workspace) still there.
    assert_eq!(
        Receipt::load(&ws.git_dir).unwrap().unwrap().status(),
        devflow_core::receipt::Status::Ready
    );

    let options = || SetupOptions {
        pitchfork: Some(quiet_pitchfork()),
        ..SetupOptions::default()
    };
    let if_new = || setup::setup(&t.project, &ws, options(), true, false, &mut NoProgress);

    // A run that holds the receipt lock is a run that is writing: not touched.
    {
        let _held = Receipt::try_lock(&ws.git_dir).unwrap().unwrap();
        assert!(if_new().unwrap().is_none());
        assert_eq!(handle.read().unwrap().unwrap().state, State::Removing);
    }

    assert!(if_new().unwrap().is_none(), "ready: nothing to set up");
    let record = handle.read().unwrap().unwrap();
    assert_eq!(record.state, State::Active, "the removal is over");
    assert!(record.resources.disowned.is_empty());
    assert_eq!(
        record
            .resources
            .branches
            .iter()
            .map(|b| b.id)
            .collect::<Vec<_>>(),
        [branch],
        "owned again, so a removal and a clean-up account for it"
    );
    // The receipt is not touched, and nothing was provisioned.
    let inventory = gc::inventory(
        &t.project,
        &gc::Options {
            measure: false,
            evidence: false,
            backfill: false,
        },
    )
    .unwrap();
    assert_eq!(inventory.entry(instance).unwrap().class, Class::Active);
}

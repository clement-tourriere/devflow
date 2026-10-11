//! Headless tests of the developer-experience screens: processes in the
//! workspace detail, the Data tab's services, the pitchfork offers, the
//! overview and the palette entries. Pitchfork is a fake executable
//! (`tests/support/fake-pitchfork.sh`) and the container services a fake
//! backend; nothing here needs Docker.
use super::{
    data::{Running, counts_text},
    data_cards,
    processes::actions,
    setup_tests::{click, click_scrolled, exists, visible, wait, wizard},
    ui_tests::{
        Harness, Scratch, allowed, another_scratch_project, frame, install_loaded, open,
        open_scratch, press_key, refused, scratch_project, settle, update,
    },
};
use crate::{
    jobs::{DoctorSource, Done, Event, Failure, FailureKind, PitchforkSource, ProcessNote},
    model::{PitchforkModel, Tab},
    ops::{FailureHint, JobMeta},
    redaction::RefusalView,
    services::{
        BranchRow, BranchState, Cow, DockerEngine, LastCloneRow, OrphanBranch, Orphans,
        ServiceBackend, ServiceCount, ServiceDetail, ServicesModel, SnapshotRow,
    },
    state::PitchforkInstall,
};
use devflow_core::{
    config::{CowCapability, Engine},
    db::DataSource,
    doctor::{Check, CheckStatus},
    env_file::MiseDelivery,
    pitchfork::{InstallPlan, Pitchfork},
    processes::{Action, Observation, Refusal},
    project::Project,
    receipt::{CloneReport, Receipt, ServiceBinding, Status},
    services::{Effects, Operation, StoppedSource},
    setup::{BranchExpect, WorkspaceStatus},
    worktree::Workspace,
};
use gpui_kit::{
    AppContext as _, TestAppContext, component::WindowExt as _, test::TestWindowExt as _,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use uuid::Uuid;

const FAKE: &str = include_str!("../../../../tests/support/fake-pitchfork.sh");

/// A directory with the fake `pitchfork` and the files it prints.
pub(super) struct Fake {
    dir: tempfile::TempDir,
}

impl Fake {
    pub(super) fn new() -> Fake {
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

    pub(super) fn pitchfork(&self) -> Pitchfork {
        Pitchfork::at(self.dir.path().join("pitchfork"))
    }

    pub(super) fn source(&self) -> PitchforkSource {
        PitchforkSource::fixed(Some(self.pitchfork()))
    }

    pub(super) fn set(&self, name: &str, content: &str) {
        std::fs::write(self.dir.path().join(name), content).unwrap();
    }

    pub(super) fn unset(&self, name: &str) {
        let _ = std::fs::remove_file(self.dir.path().join(name));
    }

    /// The arguments of every call so far, except the feature checks
    /// (`--help`).
    pub(super) fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(|line| line.split('|').nth(1).unwrap_or("").to_string())
            .filter(|args| !args.contains("--help"))
            .collect()
    }
}

/// A committed `pitchfork.toml` with one daemon, in the main checkout.
pub(super) fn write_daemon_file(root: &Path) {
    let file = root.join("pitchfork.toml");
    std::fs::write(&file, "[daemons.web]\nrun = \"bun run dev\"\nport = 3000\n").unwrap();
}

// kingfisher:ignore
const URL: &str = "postgres://u:review-password@localhost/db";
const SECRET: &str = "review-password";

#[gpui_kit::test]
fn a_credential_in_a_successful_pitchfork_listing_never_reaches_a_view_model(
    cx: &mut TestAppContext,
) {
    // A set-up workspace: core only acts through the namespace it assigned.
    let rig = start_rig(cx);
    let StartRig { h, fake, .. } = &rig;
    let namespace = devflow_core::processes::namespace_of(&main_path(cx, h)).value;
    fake.set(
        "list.json",
        &serde_json::json!([{
            "id": format!("{namespace}/web"),
            "status": "errored",
            "error": format!("cannot connect to {URL}"),
        }])
        .to_string(),
    );
    update(cx, h, |s, cx| s.set_tab(Tab::Daemons, cx));
    wait(cx, "the daemons are read", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.selection.current().is_some_and(|m| {
                m.pitchfork
                    .status
                    .as_ref()
                    .is_some_and(|status| status.daemons.iter().any(|d| d.error.is_some()))
            })
        })
    });
    // The daemon's error is what pitchfork said, with the password masked.
    let shown = cx.read_entity(&h.state, |s, _| {
        let model = s.selection.current().unwrap();
        let status = model.pitchfork.status.as_ref().unwrap();
        assert_eq!(status.daemons[0].state, "errored");
        format!("{:?}", model.pitchfork)
    });
    assert!(shown.contains("cannot connect to"), "{shown}");
    assert!(!shown.contains(SECRET), "{shown}");
    // And the tab draws it.
    frame(cx, h, |window| {
        assert!(window.find("daemons-start").visible());
    });
    assert!(exists(cx, h, "daemons-start"));
}

// ---- processes in the workspace detail ----------------------------------------

/// Show `model` as the selected workspace's processes, in the Workspaces tab.
fn show_processes(cx: &mut TestAppContext, h: &Harness, model: PitchforkModel) {
    update(cx, h, |s, cx| {
        s.tab = Tab::Workspaces;
        s.selection.current_mut().unwrap().pitchfork = model;
        cx.notify();
    });
    frame(cx, h, |_| {});
}

fn mise_refusal() -> Refusal {
    Refusal::MiseDelivery {
        daemons: vec!["web".into()],
        delivery: MiseDelivery::Overridden {
            keys: vec!["DATABASE_URL".into()],
        },
    }
}

fn current_pitchfork(cx: &mut TestAppContext, h: &Harness) -> PitchforkModel {
    cx.read_entity(&h.state, |s, _| {
        s.selection.current().unwrap().pitchfork.clone()
    })
}

#[gpui_kit::test]
fn the_detail_lists_the_daemons_with_state_port_url_and_the_gates_of_the_daemons_tab(
    cx: &mut TestAppContext,
) {
    let h = open(cx, install_loaded);
    let model = current_pitchfork(cx, &h);
    show_processes(cx, &h, model.clone());
    for target in [
        "detail-processes",
        "detail-daemons-start",
        "detail-daemons-stop",
        "detail-daemons-restart",
        "detail-autostart",
    ] {
        assert!(exists(cx, &h, target), "{target}");
    }
    click_scrolled(cx, &h, "workspace-detail", ("detail-daemon-url", 0usize));
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://web.main.localhost"),
        "the proxy URL opens in the browser"
    );
    assert!(visible(cx, &h, ("detail-daemon-start", 1usize)));

    // The gates are core's, and the same as the Daemons tab's.
    let status = model.status.clone().unwrap();
    let approved = actions(&status, false);
    assert!(approved.start && !approved.review && approved.stop && approved.restart);

    // Not approved: starting is a review, stopping is refused, and the
    // note says why.
    let mut unapproved = model.clone();
    {
        let status = unapproved.status.as_mut().unwrap();
        status.can_start = refused(Refusal::NotApproved {
            action: Action::Start,
            changed: false,
            format_changed: false,
        });
        status.can_stop = refused(Refusal::NotApproved {
            action: Action::Stop,
            changed: false,
            format_changed: false,
        });
    }
    let gates = actions(unapproved.status.as_ref().unwrap(), false);
    assert!(gates.start && gates.review && !gates.stop && !gates.restart);
    show_processes(cx, &h, unapproved);
    assert!(exists(cx, &h, "detail-start-note"));
    assert!(exists(cx, &h, "detail-stop-note"));

    // Mise cannot hand the daemons their variables: no start.
    let mut mise = model.clone();
    mise.status.as_mut().unwrap().can_start = refused(mise_refusal());
    let gates = actions(mise.status.as_ref().unwrap(), false);
    assert!(!gates.start && !gates.review);
    show_processes(cx, &h, mise);
    assert!(exists(cx, &h, "detail-start-note"));

    // A daemon that starts on its own: autostart says why it is off.
    let mut blocked = model.clone();
    {
        let status = blocked.status.as_mut().unwrap();
        status.autostart = true;
        status.autostart_blocked =
            Some("autostart is off for this workspace: web (cron) start on their own".into());
    }
    show_processes(cx, &h, blocked);
    assert!(exists(cx, &h, "detail-autostart-note"));
    assert!(exists(cx, &h, "detail-autostart"));

    // A running job disables every control.
    let busy = actions(&status, true);
    assert!(!busy.start && !busy.stop && !busy.restart);
    let _ = allowed;
}

#[gpui_kit::test]
fn a_listing_pitchfork_could_not_give_is_not_drawn_as_daemons_that_are_not_started(
    cx: &mut TestAppContext,
) {
    use super::processes::{state_view, unread_reason};
    use super::workspace_detail::process_hint;
    let h = open(cx, install_loaded);
    let mut model = current_pitchfork(cx, &h);
    // What core keeps when `pitchfork list` fails: the declared daemons with
    // the fallback state, and the failure.
    {
        let status = model.status.as_mut().unwrap();
        status.error = Some("pitchfork list failed: exit 1".into());
        status.observation =
            Observation::from_reasons(vec!["pitchfork list failed: exit 1".into()]);
        for daemon in &mut status.daemons {
            daemon.state = "not started".into();
            daemon.id = None;
        }
    }
    let status = model.status.clone().unwrap();
    let why = unread_reason(&status).expect("a failed listing is unread");
    assert!(why.contains("pitchfork list failed"), "{why}");
    // The pill says unknown, with the reason; it never says "not started".
    let (word, _, help) = state_view(&status.daemons[0], Some(&why));
    assert_eq!(word, "unknown");
    assert!(help.contains("pitchfork list failed"), "{help}");
    let (word, _, _) = state_view(&status.daemons[0], None);
    assert_eq!(word, "not started", "a read state is shown as it is");
    // A daemon pitchfork did list keeps the state it gave, even when another
    // entry of the listing could not be read.
    let mut listed = status.daemons[0].clone();
    listed.id = Some("df-ns/web".into());
    listed.state = "running".into();
    assert_eq!(state_view(&listed, Some(&why)).0, "running");
    // The hint is not "0 of N running".
    let hint = process_hint(0, status.daemons.len(), "df-ns", Some(&why));
    assert!(hint.contains("running state unknown"), "{hint}");
    assert!(!hint.contains("0 of"), "{hint}");
    assert_eq!(process_hint(1, 2, "df-ns", None), "1 of 2 running · df-ns");
    // A pitchfork that could not be asked at all is unread too.
    let mut missing = status.clone();
    missing.error = None;
    missing.installed = false;
    missing.observation = Observation::from_reasons(vec!["pitchfork cannot be used".into()]);
    assert!(unread_reason(&missing).is_some());

    show_processes(cx, &h, model);
    assert!(
        exists(cx, &h, "detail-processes-unread"),
        "the failure is shown"
    );
    // The Daemons tab shows the same failure and the same pills.
    update(cx, &h, |s, cx| s.set_tab(Tab::Daemons, cx));
    frame(cx, &h, |_| {});
    assert!(exists(cx, &h, "daemons-start"));
}

#[test]
fn a_decorated_daemon_state_is_drawn_and_counted_as_the_state_it_starts_with() {
    use super::processes::{state_help, state_tone};
    use super::ui::Tone;
    use devflow_core::processes::DaemonState;
    let daemon = |state: &str| DaemonState {
        name: "web".into(),
        id: Some("df-ns/web".into()),
        state: state.into(),
        pid: Some(1),
        ports: Vec::new(),
        url: None,
        error: None,
        triggers: Vec::new(),
        configured: true,
        startable: false,
    };
    // Counted as running, drawn green, explained as running.
    for running in [
        "running",
        "running (ready)",
        "Waiting: ready check",
        "stopping (SIGTERM)",
    ] {
        let d = daemon(running);
        assert!(d.is_active() && !d.failed(), "{running}");
        assert!(matches!(state_tone(&d), Tone::Success), "{running}");
    }
    assert_eq!(
        state_help(&daemon("running (ready)")),
        "pitchfork reports this daemon running."
    );
    assert_eq!(
        state_help(&daemon("Waiting: ready check")),
        "pitchfork started it and is waiting for its readiness check."
    );
    // Failed, drawn red, whatever follows the word.
    for failed in ["errored", "failed: exit 3", "errored (exit 3)"] {
        let d = daemon(failed);
        assert!(!d.is_active() && d.failed(), "{failed}");
        assert!(matches!(state_tone(&d), Tone::Danger), "{failed}");
        assert!(state_help(&d).contains("failed"), "{failed}");
    }
    // Stopped, or a word devflow does not know: neither.
    for other in ["stopped (SIGTERM)", "available", "paused"] {
        let d = daemon(other);
        assert!(!d.is_active() && !d.failed(), "{other}");
        assert!(matches!(state_tone(&d), Tone::Neutral), "{other}");
    }
    assert_eq!(
        state_help(&daemon("stopped (SIGTERM)")),
        "pitchfork stopped it."
    );
    assert_eq!(state_help(&daemon("Paused")), "pitchfork reports: paused.");
    // The fallback for a daemon pitchfork did not list is not a state.
    let mut unlisted = daemon("not started");
    unlisted.id = None;
    assert!(unlisted.is_not_started() && !unlisted.is_active());
    assert!(state_help(&unlisted).contains("has not started it"));
}

#[test]
fn the_service_page_counts_running_branches_only_as_far_as_docker_said() {
    use super::service_view::branches_hint;
    let (model, _) = services_fixture();
    let mut cache = model.details.unwrap().remove(0);
    // Every state read: exact.
    cache.branches[2].state = BranchState::Stopped;
    assert_eq!(branches_hint(&cache), "3 · 1 running");
    // One container Docker could not describe: a lower bound that says so.
    cache.branches[2].state = BranchState::Unknown;
    let hint = branches_hint(&cache);
    assert!(hint.starts_with("3 · at least 1 running"), "{hint}");
    assert!(hint.contains("1 of 3 branches could not be read"), "{hint}");
    // Docker could not be asked: no count at all.
    cache.docker = None;
    cache.error = Some("Cannot connect to the Docker daemon".into());
    assert!(branches_hint(&cache).starts_with("unavailable"));
    // The card and the page word the same reading the same way.
    use super::data::{Running, running_word};
    for (running, expect) in [
        (Running::Known(2), "2 running"),
        (Running::AtLeast(2), "at least 2 running"),
        (Running::Unavailable("x".into()), "unavailable"),
    ] {
        let (n, reading) = running.counted().unwrap();
        assert_eq!(running_word(n, &reading), expect);
    }
}

#[gpui_kit::test]
fn the_processes_section_says_what_is_missing_instead_of_staying_blank(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    // Not read yet.
    show_processes(cx, &h, PitchforkModel::default());
    assert!(exists(cx, &h, "detail-processes"));
    assert!(!exists(cx, &h, ("detail-daemon-row", 0usize)));
    // Read, nothing declared: one way forward.
    show_processes(
        cx,
        &h,
        PitchforkModel {
            loaded: true,
            ..Default::default()
        },
    );
    assert!(exists(cx, &h, "detail-detect-processes"));
    // pitchfork missing: the Daemons tab has the way to get it.
    show_processes(
        cx,
        &h,
        PitchforkModel {
            loaded: true,
            detection: Some(devflow_core::pitchfork::Detection {
                path: None,
                installed: false,
                version: None,
                missing_features: Vec::new(),
                problem: None,
            }),
            ..Default::default()
        },
    );
    assert!(exists(cx, &h, "detail-open-daemons"));
}

/// Finish a job as `start_job` would, with a result the test chooses.
fn finish_with(
    cx: &mut TestAppContext,
    h: &Harness,
    hint: FailureHint,
    result: Result<Done, Failure>,
) {
    let (tx, rx) = std::sync::mpsc::sync_channel(crate::jobs::EVENT_QUEUE);
    let key = cx.read_entity(&h.state, |s, _| s.selection.current().unwrap().key);
    cx.update_window(h.window.into(), |_, window, cx| {
        h.state.update(cx, |s, cx| {
            s.mutating.insert(key.project, "Start".into());
            s.inflight += 1;
            s.watch_job(
                key,
                JobMeta::new("Start the daemons", hint),
                rx,
                None,
                window,
                cx,
            );
        });
    })
    .unwrap();
    tx.send(Event::Finished(result)).unwrap();
    settle(cx, h, |s| s.busy().is_none());
}

fn main_path(cx: &mut TestAppContext, h: &Harness) -> PathBuf {
    cx.read_entity(&h.state, |s, _| {
        s.selection
            .current()
            .unwrap()
            .workspace("main")
            .unwrap()
            .workspace
            .path
            .clone()
    })
}

fn kept_refusals(cx: &mut TestAppContext, h: &Harness) -> usize {
    cx.read_entity(&h.state, |s, _| {
        s.selection.current().unwrap().refusals.len()
    })
}

#[gpui_kit::test]
fn a_start_core_refused_is_kept_with_its_explanation_and_the_doctor_is_one_click_away(
    cx: &mut TestAppContext,
) {
    let h = open(cx, install_loaded);
    update(cx, &h, |s, _| {
        s.doctor = DoctorSource::Fixed(vec![
            Check {
                name: "mise-env".into(),
                status: CheckStatus::Fail,
                detail: "mise resolves DATABASE_URL elsewhere".into(),
                hint: Some("check later _.file entries".into()),
                remedies: Vec::new(),
            },
            Check {
                name: "git".into(),
                status: CheckStatus::Ok,
                detail: "git 2.50".into(),
                hint: None,
                remedies: Vec::new(),
            },
        ]);
    });
    let path = main_path(cx, &h);
    let refusal = mise_refusal();
    let failure = Failure {
        message: refusal.to_string(),
        kind: FailureKind::Other,
        refusal: Some(Box::new(RefusalView::of(&refusal))),
        remedies: Box::default(),
    };
    finish_with(
        cx,
        &h,
        FailureHint::Processes { path: path.clone() },
        Err(failure),
    );
    let kept = cx.read_entity(&h.state, |s, _| {
        s.selection.current().unwrap().refusals.get(&path).cloned()
    });
    let kept = kept.expect("the refusal is kept for the workspace");
    assert_eq!(kept.code, "mise_delivery");
    assert!(kept.text.contains("DATABASE_URL"), "{}", kept.text);
    assert!(kept.hint.is_some(), "core's hint travels with it");

    let model = current_pitchfork(cx, &h);
    show_processes(cx, &h, model);
    assert!(exists(cx, &h, "detail-start-refusal"));
    click_scrolled(cx, &h, "workspace-detail", "detail-start-refusal-doctor");
    wait(cx, "the doctor's checks", |cx| {
        exists(cx, &h, "doctor-checks")
    });
    // What is wrong is listed first.
    assert!(exists(cx, &h, "doctor-check-0"));
    click(cx, &h, "doctor-close");

    // Dismissing forgets it.
    click_scrolled(cx, &h, "workspace-detail", "detail-start-refusal-dismiss");
    assert_eq!(kept_refusals(cx, &h), 0);

    // Any typed refusal is kept, whatever its kind; a start that works clears it.
    let other = Refusal::NothingConfigured;
    finish_with(
        cx,
        &h,
        FailureHint::Processes { path: path.clone() },
        Err(Failure {
            message: other.to_string(),
            kind: FailureKind::Other,
            refusal: Some(Box::new(RefusalView::of(&other))),
            remedies: Box::default(),
        }),
    );
    assert_eq!(kept_refusals(cx, &h), 1);
    finish_with(
        cx,
        &h,
        FailureHint::Processes { path: path.clone() },
        Ok(Done {
            process: Some(ProcessNote {
                path: path.clone(),
                refusal: None,
            }),
            ..Default::default()
        }),
    );
    assert_eq!(kept_refusals(cx, &h), 0);
}

/// A project whose main checkout declares `web`, set up, with the fake
/// pitchfork printing it running at a proxy URL once it was started.
struct StartRig {
    _scratch: Scratch,
    h: Harness,
    fake: Fake,
}

fn start_rig(cx: &mut TestAppContext) -> StartRig {
    start_rig_with(cx, None)
}

/// [`start_rig`] with the project's `pitchfork.toml` given (the one `web`
/// daemon when `None`).
fn start_rig_with(cx: &mut TestAppContext, daemons: Option<&str>) -> StartRig {
    let scratch = scratch_project();
    let root = scratch.entry.root.canonicalize().unwrap();
    match daemons {
        Some(text) => std::fs::write(root.join("pitchfork.toml"), text).unwrap(),
        None => write_daemon_file(&root),
    }
    let fake = Fake::new();
    let h = open_scratch(cx, &scratch);
    update(cx, &h, |s, _| s.pitchfork = fake.source());
    // The workspace has to be set up before core lets anything start.
    click(cx, &h, ("setup", 0usize));
    wait(cx, "main is set up", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.busy().is_none()
                && s.selection.current().is_some_and(|m| {
                    m.workspace("main")
                        .is_some_and(|w| w.status == Some(Status::Ready))
                })
        })
    });
    // The setup's output is not what these tests look at: it would take the
    // room the detail pane needs.
    click(cx, &h, "dismiss-output");
    let namespace = devflow_core::processes::namespace_of(&root).value;
    fake.set(
        "proxy.json",
        &serde_json::json!({
            "enabled": true, "scheme": "https", "tld": "localhost", "port": 443, "trusted": true,
            "projects": [{
                "project": namespace, "dir": root,
                "daemons": [{"daemon": "web", "url": "https://web.main.localhost",
                             "status": "running", "port": 3000}],
            }],
        })
        .to_string(),
    );
    fake.set(
        "after_start.json",
        &serde_json::json!([{
            "id": format!("{namespace}/web"), "status": "running", "port": [3000],
        }])
        .to_string(),
    );
    StartRig {
        _scratch: scratch,
        h,
        fake,
    }
}

#[gpui_kit::test]
fn starting_from_the_detail_is_reviewed_first_and_then_runs_through_pitchfork(
    cx: &mut TestAppContext,
) {
    let rig = start_rig(cx);
    let StartRig { h, fake, .. } = &rig;
    update(cx, h, |s, cx| {
        s.set_tab(Tab::Workspaces, cx);
        s.refresh(cx);
    });
    wait(cx, "core's gates for main", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.selection
                .current()
                .and_then(|m| m.pitchfork.status.as_ref())
                .is_some_and(|status| status.configured)
        })
    });
    let status = current_pitchfork(cx, h).status.unwrap();
    assert!(
        actions(&status, false).review,
        "the pitchfork file is not approved: the button reviews first ({:?})",
        status.can_start.reason
    );
    assert!(!fake.calls().iter().any(|c| c.starts_with("start")));

    // Start asks for the review of this workspace's own files ...
    click_scrolled(cx, h, "workspace-detail", "detail-daemons-start");
    wait(cx, "the review", |cx| exists(cx, h, "trust"));
    assert!(
        exists(cx, h, "trust-processes-1"),
        "it lists the pitchfork file"
    );
    assert!(!fake.calls().iter().any(|c| c.starts_with("start")));

    // ... and only the answer starts it.
    click(cx, h, "trust");
    wait(cx, "pitchfork was asked to start", |_| {
        fake.calls().iter().any(|c| c.starts_with("start"))
    });
    wait(cx, "the daemon reads as running", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.busy().is_none()
                && s.selection
                    .current()
                    .and_then(|m| m.pitchfork.status.as_ref())
                    .is_some_and(|status| status.daemons.iter().any(|d| d.is_active()))
        })
    });
    let url = current_pitchfork(cx, h).status.unwrap().daemons[0]
        .url
        .clone();
    assert_eq!(url.as_deref(), Some("https://web.main.localhost"));
    assert!(exists(cx, h, ("detail-daemon-url", 0usize)));
}

/// Open the review of main's pitchfork files from the detail's button and
/// leave the dialog open. Returns main's namespace.
fn open_start_review(cx: &mut TestAppContext, rig: &StartRig) -> String {
    let StartRig { h, .. } = rig;
    update(cx, h, |s, cx| {
        s.set_tab(Tab::Workspaces, cx);
        s.refresh(cx);
    });
    wait(cx, "core's gates for main", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.selection
                .current()
                .and_then(|m| m.pitchfork.status.as_ref())
                .is_some_and(|status| status.configured)
        })
    });
    click_scrolled(cx, h, "workspace-detail", "detail-daemons-start");
    wait(cx, "the review", |cx| exists(cx, h, "trust"));
    // Dialogs slide in on the wall clock.
    std::thread::sleep(Duration::from_millis(350));
    let root = main_path(cx, h);
    devflow_core::processes::namespace_of(&root).value
}

#[gpui_kit::test]
fn a_start_confirmed_after_the_selection_moved_starts_the_workspace_it_reviewed(
    cx: &mut TestAppContext,
) {
    let rig = start_rig(cx);
    let StartRig { h, fake, .. } = &rig;
    let namespace = open_start_review(cx, &rig);
    assert!(!fake.calls().iter().any(|c| c.starts_with("start")));

    // While the dialog is open, another workspace of the same project is
    // selected (same project key, same digest of the project's hooks).
    update(cx, h, |s, cx| {
        let model = s.selection.current_mut().unwrap();
        let mut other = model.workspaces[0].clone();
        other.workspace.name = "feature-b".into();
        other.workspace.is_default = false;
        other.workspace.path = other.workspace.path.join("feature-b");
        other.receipt = None;
        model.workspaces.push(other);
        model.selected_workspace = Some("feature-b".into());
        cx.notify();
    });
    click(cx, h, "trust");
    wait(cx, "pitchfork was asked to start", |_| {
        fake.calls().iter().any(|c| c.starts_with("start"))
    });
    wait(cx, "the job finished", |cx| {
        cx.read_entity(&h.state, |s, _| s.busy().is_none())
    });
    // It started main's daemons, by main's ids, and nothing else.
    assert_eq!(
        fake.calls()
            .into_iter()
            .filter(|c| c.starts_with("start"))
            .collect::<Vec<_>>(),
        [format!("start -q {namespace}/web")]
    );
}

#[gpui_kit::test]
fn a_palette_process_action_acts_on_the_workspace_it_was_listed_for(cx: &mut TestAppContext) {
    use super::palette::{PaletteAction, process_actions};
    let rig = start_rig(cx);
    let StartRig { h, fake, .. } = &rig;
    update(cx, h, |s, cx| {
        s.set_tab(Tab::Workspaces, cx);
        s.refresh(cx);
    });
    wait(cx, "core's gates for main", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.selection
                .current()
                .and_then(|m| m.pitchfork.status.as_ref())
                .is_some_and(|status| status.configured)
        })
    });
    let root = main_path(cx, h);
    let namespace = devflow_core::processes::namespace_of(&root).value;
    let listed = cx.read_entity(&h.state, |s, _| process_actions(s));
    let [
        PaletteAction::StartProcesses { target, review },
        PaletteAction::StopProcesses(stop),
        PaletteAction::RestartProcesses(restart),
    ] = listed.as_slice()
    else {
        panic!("{listed:?}");
    };
    assert!(*review, "main's files are not approved yet");
    assert_eq!(&target.path, &root);
    assert!(target.instance.is_some(), "{target:?}");
    assert_eq!((stop, restart), (target, target));
    let listed_start = listed[0].clone();

    // The palette was open while another workspace got selected.
    update(cx, h, |s, cx| {
        let model = s.selection.current_mut().unwrap();
        let mut other = model.workspaces[0].clone();
        other.workspace.name = "feature-b".into();
        other.workspace.is_default = false;
        other.workspace.path = other.workspace.path.join("feature-b");
        other.receipt = None;
        model.workspaces.push(other);
        model.selected_workspace = Some("feature-b".into());
        cx.notify();
    });
    cx.update_window(h.window.into(), |_, window, cx| {
        listed_start.run(&h.state, window, cx)
    })
    .unwrap();
    wait(cx, "the review of main's files", |cx| {
        exists(cx, h, "trust")
    });
    // Dialogs slide in on the wall clock.
    std::thread::sleep(Duration::from_millis(350));
    click(cx, h, "trust");
    wait(cx, "pitchfork was asked to start", |_| {
        fake.calls().iter().any(|c| c.starts_with("start"))
    });
    assert_eq!(
        fake.calls()
            .into_iter()
            .filter(|c| c.starts_with("start"))
            .collect::<Vec<_>>(),
        [format!("start -q {namespace}/web")]
    );
}

#[gpui_kit::test]
fn a_stop_after_a_new_instance_took_the_workspace_changes_nothing(cx: &mut TestAppContext) {
    let rig = start_rig(cx);
    let StartRig { h, fake, .. } = &rig;
    open_start_review(cx, &rig);
    click(cx, h, "trust");
    wait(cx, "the daemon reads as running", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.busy().is_none()
                && s.selection
                    .current()
                    .and_then(|m| m.pitchfork.status.as_ref())
                    .is_some_and(|status| status.daemons.iter().any(|d| d.is_active()))
        })
    });
    // `devflow setup --new-instance` runs in a terminal: the window still
    // shows the old receipt and its Stop button, the disk has another instance.
    let git_dir = cx.read_entity(&h.state, |s, _| {
        s.selection
            .current()
            .unwrap()
            .workspace("main")
            .unwrap()
            .workspace
            .git_dir
            .clone()
    });
    let mut receipt = Receipt::load(&git_dir).unwrap().unwrap();
    receipt.instance_id = Uuid::new_v4();
    receipt.save(&git_dir).unwrap();
    cx.update_window(h.window.into(), |_, window, cx| {
        window.clear_notifications(cx)
    })
    .unwrap();
    click_scrolled(cx, h, "workspace-detail", "detail-daemons-stop");
    // The job ran, core refused it (its error is the toast) and stopped nothing.
    wait(cx, "core's refusal as a toast", |cx| {
        let toasts = cx
            .update_window(h.window.into(), |_, window, cx| {
                window.notifications(cx).len()
            })
            .unwrap();
        toasts > 0 && cx.read_entity(&h.state, |s, _| s.busy().is_none())
    });
    assert!(
        !fake.calls().iter().any(|c| c.starts_with("stop")),
        "{:?}",
        fake.calls()
    );
}

#[gpui_kit::test]
fn every_page_draws_workspace_branch_daemon_and_snapshot_names_masked(cx: &mut TestAppContext) {
    use super::{daemons, palette, sidebar, workspace_detail::Detail};
    // Legal names that look like credentials, one per kind.
    // kingfisher:ignore
    let (workspace, branch, daemon, snapshot) = (
        "password=hunter2",
        "token=abc12345678",
        "secret=daemon9",
        "password=snap-1",
    );
    let h = open(cx, install_loaded);
    update(cx, &h, |s, cx| {
        let model = s.selection.current_mut().unwrap();
        let ws = &mut model.workspaces[1];
        ws.workspace.name = workspace.into();
        ws.workspace.branch = Some(branch.into());
        model.selected_workspace = Some(workspace.into());
        let status = model.pitchfork.status.as_mut().unwrap();
        status.daemons[0].name = daemon.into();
        model.database.snapshots[0].name = snapshot.into();
        cx.notify();
    });
    let (drawn, identities) = cx.read_entity(&h.state, |s, _| {
        let model = s.selection.current().unwrap();
        let status = model.pitchfork.status.as_ref().unwrap();
        let mut drawn: Vec<String> = vec![
            // The sidebar and the Workspaces tab.
            sidebar::workspace_label(&model.workspaces[1].workspace.name),
            // The Daemons tab's header and rows, and the detail's.
            daemons::title(model.selected_workspace.as_deref()),
            super::processes::daemon_label(&status.daemons[0]),
        ];
        for row in crate::model::workspace_rows(&model.workspaces) {
            drawn.extend([row.label(), row.branch.clone()]);
        }
        let detail = Detail::of(model, &model.workspaces[1]);
        drawn.extend([detail.title(), detail.branch.clone()]);
        // The palette's entries (the processes' detail names the workspace).
        drawn.extend(palette::drawn_texts(s));
        // The Data tab's PostgreSQL branches and snapshots, and the choices.
        for row in crate::model::branch_rows(&model.database, &model.workspaces) {
            drawn.extend([row.name.clone(), row.source.clone()]);
        }
        for row in crate::model::snapshot_rows(&model.database) {
            drawn.extend([row.label(), row.source.clone()]);
        }
        drawn.extend(
            crate::model::data_options("same", &model.workspaces, &model.database.snapshots)
                .into_iter()
                .map(|o| o.label),
        );
        drawn.extend(super::overview::numbers(model, &[], None).last_activity);
        let identities = (
            model.workspaces[1].workspace.name.clone(),
            model.workspaces[1].workspace.branch.clone(),
            status.daemons[0].name.clone(),
            crate::model::snapshot_rows(&model.database)[0]
                .name
                .as_str()
                .to_string(),
        );
        (drawn, identities)
    });
    let text = drawn.join("\n");
    for secret in ["hunter2", "abc12345678", "daemon9", "snap-1"] {
        assert!(!text.contains(secret), "{secret} is drawn:\n{text}");
    }
    for masked in [
        "password=***",
        "token=***",
        "secret=***",
        "Daemons in password=***",
    ] {
        assert!(text.contains(masked), "{masked} missing:\n{text}");
    }
    // Ordinary names are drawn as they are.
    assert_eq!(sidebar::workspace_label("feature-x"), "feature-x");
    // The identities are what core recorded.
    assert_eq!(
        identities,
        (
            workspace.to_string(),
            Some(branch.to_string()),
            daemon.to_string(),
            snapshot.to_string()
        )
    );
}

#[gpui_kit::test]
fn a_state_nobody_observed_is_unknown_and_stop_stays_available_through_core(
    cx: &mut TestAppContext,
) {
    use super::processes::{state_view, unread_reason};
    let rig = start_rig(cx);
    let StartRig { h, fake, .. } = &rig;
    let root = main_path(cx, h);
    let namespace = devflow_core::processes::namespace_of(&root).value;
    // The files were reviewed, and the daemon was started through devflow
    // (the receipt records it).
    let project = Project::open(&root).unwrap();
    let reviewed = devflow_core::trust::scope_at(&project, &root);
    devflow_core::trust::approve_scope_at(&project, &root, &reviewed).unwrap();
    update(cx, h, |s, cx| {
        s.set_tab(Tab::Workspaces, cx);
        s.refresh(cx);
    });
    wait(cx, "core's gates for main", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.selection
                .current()
                .and_then(|m| m.pitchfork.status.as_ref())
                .is_some_and(|status| status.configured && status.can_start.allowed)
        })
    });
    click_scrolled(cx, h, "workspace-detail", "detail-daemons-start");
    wait(cx, "the daemon reads as running", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.busy().is_none()
                && s.selection
                    .current()
                    .and_then(|m| m.pitchfork.status.as_ref())
                    .is_some_and(|status| status.daemons.iter().any(|d| d.is_active()))
        })
    });
    let observed = current_pitchfork(cx, h).status.unwrap();
    assert!(observed.observation.is_complete());
    assert!(actions(&observed, false).stop);

    // The supervisor crashes: the daemon lives on, pitchfork has no listing.
    fake.set("supervisor_down", "1");
    update(cx, h, |s, cx| s.refresh(cx));
    wait(cx, "the status is not an observation", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.busy().is_none()
                && s.selection
                    .current()
                    .and_then(|m| m.pitchfork.status.as_ref())
                    .is_some_and(|status| !status.observation.is_complete())
        })
    });
    let status = current_pitchfork(cx, h).status.unwrap();
    let why = unread_reason(&status).expect("core says it was not observed");
    assert!(why.contains("supervisor is down"), "{why}");
    // No daemon reads "not started" and Stop is not disabled.
    assert!(
        state_view(&status.daemons[0], Some(&why)).0 == "unknown",
        "{:?}",
        status.daemons
    );
    assert!(actions(&status, false).stop, "Stop stays available");
    let (value, _) = cx.read_entity(&h.state, |s, _| {
        let numbers = super::overview::numbers(s.selection.current().unwrap(), &[], None);
        super::overview::processes_tile(&numbers)
    });
    assert_eq!(value, "unknown", "not 0 running");
    assert!(exists(cx, h, "detail-processes-unread"));

    // Stop asks core, which refuses it as `state_unknown` before any effect;
    // the banner offers to go on, and only the confirmation forces it. The
    // supervisor answers again, but the listing is broken: the daemons the
    // workspace declares are stopped by id.
    fake.unset("supervisor_down");
    fake.set("list_fail", "1");
    update(cx, h, |s, cx| s.refresh(cx));
    wait(cx, "the listing failure", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.busy().is_none()
                && s.selection
                    .current()
                    .and_then(|m| m.pitchfork.status.as_ref())
                    .is_some_and(|status| status.error.is_some())
        })
    });
    click_scrolled(cx, h, "workspace-detail", "detail-daemons-stop");
    wait(cx, "core's refusal is kept", |cx| {
        kept_refusals(cx, h) == 1 && cx.read_entity(&h.state, |s, _| s.busy().is_none())
    });
    assert!(
        !fake.calls().iter().any(|c| c.starts_with("stop")),
        "refused before any effect: {:?}",
        fake.calls()
    );
    let kept = cx.read_entity(&h.state, |s, _| {
        s.selection
            .current()
            .unwrap()
            .refusals
            .values()
            .next()
            .cloned()
    });
    assert!(kept.unwrap().is_state_unknown());
    click_scrolled(
        cx,
        h,
        "workspace-detail",
        "detail-start-refusal-stop-anyway",
    );
    wait(cx, "the confirmation", |cx| exists(cx, h, "confirm"));
    // Dialogs slide in on the wall clock.
    std::thread::sleep(Duration::from_millis(350));
    assert!(
        !fake.calls().iter().any(|c| c.starts_with("stop")),
        "nothing happens before the answer"
    );
    click(cx, h, "confirm");
    wait(cx, "pitchfork was asked to stop", |_| {
        fake.calls().iter().any(|c| c.starts_with("stop"))
    });
    wait(cx, "the job finished", |cx| {
        cx.read_entity(&h.state, |s, _| s.busy().is_none())
    });
    assert_eq!(
        fake.calls()
            .into_iter()
            .filter(|c| c.starts_with("stop"))
            .collect::<Vec<_>>(),
        [format!("stop {namespace}/web")]
    );
}

#[gpui_kit::test]
fn a_daemon_named_like_a_credential_is_drawn_masked_and_started_by_its_own_name(
    cx: &mut TestAppContext,
) {
    // Two legal daemon names: the second is what masking the first would say.
    let rig = start_rig_with(
        cx,
        Some(
            "[daemons.\"password=hunter2\"]\nrun = \"a\"\nport = 3000\n\n[daemons.\"password=***\"]\nrun = \"b\"\nport = 3001\n",
        ),
    );
    let StartRig { h, fake, .. } = &rig;
    let root = main_path(cx, h);
    let namespace = devflow_core::processes::namespace_of(&root).value;
    // The files were reviewed: starting a daemon by name is allowed.
    let project = Project::open(&root).unwrap();
    let reviewed = devflow_core::trust::scope_at(&project, &root);
    devflow_core::trust::approve_scope_at(&project, &root, &reviewed).unwrap();
    update(cx, h, |s, cx| {
        s.set_tab(Tab::Workspaces, cx);
        s.refresh(cx);
    });
    wait(cx, "both daemons are startable", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.selection
                .current()
                .and_then(|m| m.pitchfork.status.as_ref())
                .is_some_and(|status| {
                    status.daemons.len() == 2 && status.daemons.iter().all(|d| d.startable)
                })
        })
    });
    // The model keeps the names as pitchfork has them ...
    let names: Vec<String> = current_pitchfork(cx, h)
        .status
        .unwrap()
        .daemons
        .iter()
        .map(|d| d.name.clone())
        .collect();
    assert_eq!(names, ["password=***", "password=hunter2"]);
    // ... the row of the first is the second in the list, and its Start asks
    // for exactly its name.
    click_scrolled(cx, h, "workspace-detail", ("detail-daemon-start", 1usize));
    wait(cx, "pitchfork was asked to start", |_| {
        fake.calls().iter().any(|c| c.starts_with("start"))
    });
    wait(cx, "the job finished", |cx| {
        cx.read_entity(&h.state, |s, _| s.busy().is_none())
    });
    assert_eq!(
        fake.calls()
            .into_iter()
            .filter(|c| c.starts_with("start"))
            .collect::<Vec<_>>(),
        [format!("start -q {namespace}/password=hunter2")]
    );
}

// ---- the Data tab: container services without Docker -------------------------------

/// A backend that records what it is asked and answers from a fixture, so the
/// Data tab and its confirmations run without Docker.
struct FakeServices {
    model: ServicesModel,
    calls: std::sync::Mutex<Vec<String>>,
    url: String,
    /// The receipts "on disk", by workspace name: what core checks an
    /// expectation against.
    receipts: std::sync::Mutex<std::collections::HashMap<String, Receipt>>,
    /// What every operation does to its source, as core would report it.
    effects: std::sync::Mutex<Effects>,
    /// Core's answer to a drop: the workspaces' receipts cannot all be read.
    ownership_unknown: std::sync::atomic::AtomicBool,
}

impl FakeServices {
    fn new(model: ServicesModel, url: &str) -> Arc<FakeServices> {
        Arc::new(FakeServices {
            model,
            calls: Default::default(),
            url: url.to_string(),
            receipts: Default::default(),
            effects: Default::default(),
            ownership_unknown: Default::default(),
        })
    }

    /// The receipt the workspace has on disk now.
    fn bind(&self, workspace: &str, receipt: Receipt) {
        self.receipts
            .lock()
            .unwrap()
            .insert(workspace.to_string(), receipt);
    }

    /// Core's own check of what the dialog showed against the disk.
    fn verify(&self, workspace: &Workspace, key: &str, expect: BranchExpect) -> anyhow::Result<()> {
        let receipts = self.receipts.lock().unwrap();
        let receipt = receipts
            .get(&workspace.name)
            .ok_or_else(|| anyhow::anyhow!("{} has no receipt", workspace.name))?;
        expect.verify(workspace, key, receipt)
    }

    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl ServiceBackend for FakeServices {
    fn load(&self, _: &Project, _: &[WorkspaceStatus], full: bool) -> ServicesModel {
        let mut model = self.model.clone();
        if !full {
            model.details = None;
        }
        model
    }

    fn measure(&self, _: &Project, key: &str) -> anyhow::Result<Cow> {
        self.record(format!("measure {key}"));
        Ok(Cow {
            capability: CowCapability::Verified,
            note: None,
            measured_at: Some(chrono::Utc::now()),
        })
    }

    fn up(&self, _: &Project, key: &str, branch: Uuid) -> anyhow::Result<String> {
        self.record(format!("up {key} {branch}"));
        Ok("127.0.0.1:6382".into())
    }

    fn down(&self, _: &Project, key: &str, branch: Uuid) -> anyhow::Result<bool> {
        self.record(format!("down {key} {branch}"));
        Ok(true)
    }

    fn url(&self, _: &Project, key: &str, branch: Uuid) -> anyhow::Result<String> {
        self.record(format!("url {key} {branch}"));
        Ok(self.url.clone())
    }

    fn snapshot(
        &self,
        _: &Project,
        workspace: &Workspace,
        key: &str,
        name: &str,
        expect: BranchExpect,
    ) -> anyhow::Result<String> {
        self.verify(workspace, key, expect)?;
        self.record(format!("snapshot {key} {} {name}", workspace.name));
        Ok(format!("{name}@v1"))
    }

    fn delete_snapshot(&self, _: &Project, key: &str, snapshot: Uuid) -> anyhow::Result<()> {
        self.record(format!("delete-snapshot {key} {snapshot}"));
        Ok(())
    }

    fn reset(
        &self,
        project: &Project,
        workspace: &Workspace,
        key: &str,
        to: Option<DataSource>,
        expect: BranchExpect,
    ) -> anyhow::Result<String> {
        self.verify(workspace, key, expect)?;
        // Core resolves the request and holds it to what was shown.
        let now = self.reset_source(project, workspace, key, to.as_ref())?;
        expect.verify_source(workspace, key, now.source)?;
        self.record(format!("reset {key} {} {to:?}", workspace.name));
        Ok("reflink".into())
    }

    fn reset_source(
        &self,
        _: &Project,
        _: &Workspace,
        key: &str,
        to: Option<&DataSource>,
    ) -> anyhow::Result<devflow_core::setup::ResetSource> {
        use devflow_core::services::catalog::SourceRef;
        let source = match to {
            None | Some(DataSource::Empty) => SourceRef::Empty,
            Some(DataSource::Workspace { name }) => {
                let receipts = self.receipts.lock().unwrap();
                let id = receipts
                    .get(name)
                    .and_then(|r| r.services.get(key))
                    .and_then(|b| b.branch_id)
                    .ok_or_else(|| anyhow::anyhow!("{name} has no branch of {key}"))?;
                SourceRef::Branch { id }
            }
            Some(DataSource::Branch { id }) => SourceRef::Branch { id: *id },
            Some(DataSource::BranchPrefix { prefix }) => {
                anyhow::bail!("this fake does not resolve the prefix {prefix}")
            }
            Some(DataSource::Snapshot { name, version }) => {
                let model = self.model.details.as_ref().and_then(|d| {
                    d.iter()
                        .find(|d| d.key == key)?
                        .snapshots
                        .iter()
                        .find(|s| s.name.as_str() == name && Some(s.version as i32) == *version)
                });
                SourceRef::Snapshot {
                    id: model
                        .ok_or_else(|| anyhow::anyhow!("no snapshot {name}"))?
                        .id,
                }
            }
        };
        Ok(devflow_core::setup::ResetSource {
            source,
            label: String::new(),
        })
    }

    fn effects(&self, _: &Project, key: &str, _: Operation<'_>) -> anyhow::Result<Effects> {
        self.record(format!("effects {key}"));
        Ok(self.effects.lock().unwrap().clone())
    }

    fn reconcile(&self, _: &Project, key: &str) -> anyhow::Result<Vec<String>> {
        self.record(format!("reconcile {key}"));
        Ok(vec!["rolled back a clone of feature-x".into()])
    }

    fn drop_branch(&self, _: &Project, key: &str, branch: Uuid) -> anyhow::Result<()> {
        if self
            .ownership_unknown
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(devflow_core::error::OwnershipUnknown {
                problems: vec![devflow_core::error::UnreadReceipt {
                    workspace: "feature-x".into(),
                    reason: "invalid receipt: expected value at line 1 column 1".into(),
                }],
            }
            .into());
        }
        self.record(format!("drop {key} {branch}"));
        Ok(())
    }

    fn drop_lost_branch(&self, _: &Project, key: &str, branch: Uuid) -> anyhow::Result<()> {
        self.record(format!("drop-lost {key} {branch}"));
        Ok(())
    }

    fn forget(&self, _: &Project, key: &str) -> anyhow::Result<Vec<String>> {
        self.record(format!("forget {key}"));
        Ok(vec!["branch x".into()])
    }
}

// A legal snapshot name that the masking would change.
// kingfisher:ignore
const ODD_SNAPSHOT: &str = "password=hunter2";

// kingfisher:ignore
const REDIS_URL: &str = "redis://:cache-password@127.0.0.1:6381/0";

struct Ids {
    /// The `seed@v2` snapshot.
    seed: Uuid,
    /// A snapshot whose name looks like a credential.
    odd: Uuid,
    main: Uuid,
    feature: Uuid,
    orphan: Uuid,
}

fn branch(
    id: Uuid,
    name: &str,
    workspace: Option<&str>,
    state: BranchState,
    port: u16,
) -> BranchRow {
    BranchRow {
        id,
        name: name.into(),
        workspace: workspace.map(crate::redaction::RawName::from),
        owner_recorded: true,
        state,
        endpoint: Some(format!("127.0.0.1:{port}")),
        container_endpoint: Some(format!("{name}.cache.shop-0b3c9d2e.devflow.orb.local:6379")),
        masked_url: workspace.map(|_| format!("redis://:***@127.0.0.1:{port}/0")),
        source: "snapshot seed@v2".into(),
        captured: Some("2026-10-11 09:30".into()),
        capture_exact: true,
        capture_last_save: false,
        clone: Some("reflink".into()),
        allocated: Some(12 * 1024 * 1024),
        database: None,
        source_downtime_ms: None,
        generation: 1,
    }
}

/// A project with a Valkey service (two workspaces' branches, one snapshot,
/// one orphan branch), a MySQL one core did not have when this was written,
/// and a service the configuration dropped.
fn services_fixture() -> (ServicesModel, Ids) {
    let ids = Ids {
        seed: Uuid::new_v4(),
        odd: Uuid::new_v4(),
        main: Uuid::new_v4(),
        feature: Uuid::new_v4(),
        orphan: Uuid::new_v4(),
    };
    let docker = Some(DockerEngine {
        id: "3f2a9c1d7e04".into(),
        orbstack: true,
    });
    let cache = ServiceDetail {
        key: "cache".into(),
        engine: "valkey".into(),
        image: "valkey/valkey:8".into(),
        docker: docker.clone(),
        cow: Cow {
            capability: CowCapability::Verified,
            note: None,
            measured_at: Some(chrono::Utc::now()),
        },
        require_cow: true,
        unfinished: 1,
        branches: vec![
            branch(ids.main, "main", Some("main"), BranchState::Running, 6381),
            branch(
                ids.feature,
                "feature-x",
                Some("feature-x"),
                BranchState::Stopped,
                6382,
            ),
            branch(ids.orphan, "old-feature", None, BranchState::Stopped, 6383),
        ],
        snapshots: vec![
            SnapshotRow {
                id: ids.seed,
                name: "seed".into(),
                version: 2,
                source: "branch main".into(),
                captured: Some("2026-10-10 18:00".into()),
                clone: Some("reflink".into()),
                allocated: Some(4 * 1024 * 1024),
                source_downtime_ms: None,
                created: "2026-10-10 18:01".into(),
            },
            SnapshotRow {
                id: ids.odd,
                name: ODD_SNAPSHOT.into(),
                version: 1,
                source: "branch main".into(),
                captured: None,
                clone: Some("reflink".into()),
                allocated: None,
                source_downtime_ms: None,
                created: "2026-10-10 19:00".into(),
            },
        ],
        last_clone: Some(LastCloneRow {
            operation: "clone".into(),
            target: "feature-x".into(),
            target_id: ids.feature,
            finished: "2026-10-11 09:31".into(),
            method: Some("reflink".into()),
            source_downtime_ms: None,
        }),
        error: None,
    };
    let shop = ServiceDetail {
        key: "shop-data".into(),
        engine: "mysql".into(),
        image: "mysql:8.4".into(),
        docker,
        cow: Cow {
            capability: CowCapability::Unknown,
            note: None,
            measured_at: None,
        },
        require_cow: false,
        unfinished: 0,
        // A MySQL branch: a database of its own, and a source that was
        // stopped for the copy.
        last_clone: Some(LastCloneRow {
            operation: "clone".into(),
            target: "main".into(),
            target_id: Uuid::new_v4(),
            finished: "2026-10-11 09:40".into(),
            method: Some("reflink".into()),
            source_downtime_ms: Some(1800),
        }),
        branches: vec![BranchRow {
            database: Some("df_main".into()),
            source_downtime_ms: Some(1800),
            container_endpoint: Some("main.shop-data.shop-0b3c9d2e.devflow.orb.local:3306".into()),
            masked_url: Some("mysql://df_main:***@127.0.0.1:3307/df_main".into()),
            ..branch(
                Uuid::new_v4(),
                "main",
                Some("main"),
                BranchState::Running,
                3307,
            )
        }],
        snapshots: Vec::new(),
        error: None,
    };
    let model = ServicesModel {
        counts: vec![
            ServiceCount {
                key: "cache".into(),
                engine: "valkey".into(),
                known: true,
                branches: 3,
                snapshots: 2,
                unfinished: 1,
            },
            ServiceCount {
                key: "shop-data".into(),
                engine: "mysql".into(),
                known: true,
                branches: 1,
                snapshots: 0,
                unfinished: 0,
            },
        ],
        orphans: Orphans {
            services: vec!["queue".into()],
            branches: vec![OrphanBranch {
                service: "cache".into(),
                id: ids.orphan,
                name: "old-feature".into(),
                reason: "no workspace of this project has the instance that owns it".into(),
            }],
            incomplete: Vec::new(),
            scanned: true,
        },
        details: Some(vec![cache, shop]),
        error: None,
    };
    (model, ids)
}

/// A scratch project on the Data tab, served by the fake backend; `main` and
/// `feature-x` carry a binding to the `cache` service in their receipts.
struct DataRig {
    _scratch: Scratch,
    h: Harness,
    fake: Arc<FakeServices>,
    ids: Ids,
}

fn data_rig(cx: &mut TestAppContext) -> DataRig {
    let scratch = scratch_project();
    let h = open_scratch(cx, &scratch);
    let (model, ids) = services_fixture();
    let fake = FakeServices::new(model, REDIS_URL);
    let for_state = fake.clone();
    update(cx, &h, |s, cx| {
        s.services = for_state;
        s.set_tab(Tab::Data, cx);
    });
    wait(cx, "the services are read", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.selection
                .current()
                .is_some_and(|m| m.services.details.is_some())
        })
    });
    install_workspaces(cx, &h, &fake, &ids);
    DataRig {
        _scratch: scratch,
        h,
        fake,
        ids,
    }
}

/// The workspaces the fixture's branches belong to: a receipt that binds the
/// service is what lets a dialog offer them. A refresh reads the real list
/// again and drops them, so a test that refreshes calls this once more.
/// Returns the receipts, which are also what the fake's "disk" holds.
fn install_workspaces(
    cx: &mut TestAppContext,
    h: &Harness,
    fake: &Arc<FakeServices>,
    ids: &Ids,
) -> Vec<Receipt> {
    let (main, feature) = (ids.main, ids.feature);
    let for_fake = fake.clone();
    let made = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let for_made = made.clone();
    update(cx, h, |s, cx| {
        let model = s.selection.current_mut().unwrap();
        if model.workspaces.len() < 2 {
            let mut feature_ws = model.workspaces[0].clone();
            feature_ws.workspace.name = "feature-x".into();
            feature_ws.workspace.is_default = false;
            feature_ws.workspace.path = feature_ws.workspace.path.join("feature-x");
            model.workspaces.push(feature_ws);
        }
        for (ix, branch) in [main, feature].into_iter().enumerate() {
            let mut receipt = Receipt::new(Uuid::new_v4(), "w", None);
            receipt.services.insert(
                "cache".into(),
                ServiceBinding::container(
                    Engine::Valkey,
                    branch,
                    REDIS_URL.into(),
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
            for_fake.bind(&model.workspaces[ix].workspace.name, receipt.clone());
            for_made.lock().unwrap().push(receipt.clone());
            model.workspaces[ix].receipt = Some(receipt);
        }
        cx.notify();
    });
    frame(cx, h, |_| {});
    made.lock().unwrap().clone()
}

/// Click `opener` (scrolled into view in the service page) and wait until
/// the dialog it opens shows `inside`; dialogs slide in on the wall clock.
fn open_dialog(
    cx: &mut TestAppContext,
    h: &Harness,
    opener: impl Into<gpui_kit::ElementId> + Clone,
    inside: &'static str,
) {
    click_scrolled(cx, h, "service-view", opener);
    wait(cx, "the dialog opened", |cx| exists(cx, h, inside));
    std::thread::sleep(Duration::from_millis(350));
}

/// Cancel the open dialog and wait until it is gone.
fn cancel_dialog(cx: &mut TestAppContext, h: &Harness) {
    click(cx, h, "cancel");
    wait(cx, "the dialog closed", |cx| !exists(cx, h, "cancel"));
}

#[gpui_kit::test]
fn the_data_tab_lists_every_service_with_branches_endpoints_snapshots_and_orphans(
    cx: &mut TestAppContext,
) {
    let DataRig { h, ids, .. } = &data_rig(cx);
    for target in [
        "data-tab",
        "service-card-cache",
        "service-card-shop-data",
        "service-header",
        "service-cow",
        "service-engine",
        "service-require-cow",
        "service-unfinished",
        "service-snapshots",
        "service-orphans",
        "service-orphan-service",
    ] {
        let found = if target == "service-orphan-service" {
            exists(cx, h, ("service-orphan-service", 0usize))
        } else {
            exists(cx, h, target)
        };
        assert!(found, "{target}");
    }
    for ix in 0..3usize {
        for target in [
            "service-branch",
            "service-endpoint",
            "service-container-endpoint",
        ] {
            assert!(exists(cx, h, (target, ix)), "{target} {ix}");
        }
    }
    // The URL is shown masked, for the branches a workspace owns.
    assert!(exists(cx, h, ("service-masked-url", 0usize)));
    assert!(!exists(cx, h, ("service-masked-url", 2usize)));
    assert!(exists(cx, h, ("service-snapshot-row", 0usize)));
    assert!(exists(cx, h, ("service-orphan", 0usize)));
    // The view model never holds a password.
    let model = cx.read_entity(&h.state, |s, _| {
        format!("{:?}", s.selection.current().unwrap().services)
    });
    assert!(!model.contains("cache-password"), "{model}");

    // The cards: PostgreSQL would come first; engines are just words.
    let cards = cx.read_entity(&h.state, |s, _| {
        super::data_cards(s.selection.current().unwrap())
    });
    let words: Vec<_> = cards
        .iter()
        .map(|c| (c.key.as_str(), c.engine.as_str(), c.branches))
        .collect();
    assert_eq!(
        words,
        [
            ("cache", "valkey", Some(3)),
            ("shop-data", "mysql", Some(1))
        ]
    );
    assert_eq!(cards[0].running, Running::Known(1));
    assert_eq!((cards[0].unfinished, cards[0].orphans), (1, 1));
    assert_eq!(cards[0].cow.as_deref(), Some("verified"));
    assert_eq!(cards[1].cow.as_deref(), Some("not measured"));

    // Choosing another card shows that service.
    click(cx, h, "service-card-shop-data");
    assert_eq!(
        cx.read_entity(&h.state, |s, _| s.selected_data_service()),
        Some("shop-data".to_string())
    );
    assert!(exists(cx, h, "service-header"));
    // MySQL renders through the same rows; its source downtime is shown.
    assert!(exists(cx, h, ("service-branch", 0usize)));
    assert!(exists(cx, h, "service-last-clone"));
    assert!(exists(cx, h, ("service-branch-facts", 0usize)));
    let facts = cx.read_entity(&h.state, |s, _| {
        let detail = s
            .selection
            .current()
            .and_then(|m| m.services.detail("shop-data").cloned())
            .unwrap();
        (
            detail.last_clone.unwrap().text(),
            crate::views::branch_facts(&detail.branches[0]),
        )
    });
    assert!(
        facts.0.contains("its source was stopped for 1.8 s"),
        "{}",
        facts.0
    );
    assert!(facts.1.contains("database df_main"), "{}", facts.1);
    assert!(facts.1.contains("source stopped for 1.8 s"), "{}", facts.1);
    let _ = ids;
}

#[gpui_kit::test]
fn copying_a_url_asks_core_when_clicked_and_only_the_clipboard_gets_the_password(
    cx: &mut TestAppContext,
) {
    let DataRig { h, fake, ids, .. } = &data_rig(cx);
    click_scrolled(cx, h, "service-view", ("service-copy-url", 0usize));
    wait(cx, "the URL was asked for", |_| {
        fake.calls().contains(&format!("url cache {}", ids.main))
    });
    wait(cx, "the clipboard has it", |cx| {
        cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()))
            .is_some()
    });
    let copied = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(copied.as_deref(), Some(REDIS_URL), "copied in full");
    let everything = cx.read_entity(&h.state, |s, _| {
        let model = s.selection.current().unwrap();
        format!(
            "{:?} {:?}",
            model.services,
            s.activity.as_ref().map(|a| a.log.text())
        )
    });
    assert!(!everything.contains("cache-password"), "{everything}");
}

#[gpui_kit::test]
fn start_and_stop_are_jobs_with_their_progress_in_the_activity_log(cx: &mut TestAppContext) {
    let DataRig { h, fake, ids, .. } = &data_rig(cx);
    click_scrolled(cx, h, "service-view", ("service-start", 1usize));
    wait(cx, "feature-x was started", |cx| {
        fake.calls().contains(&format!("up cache {}", ids.feature))
            && cx.read_entity(&h.state, |s, _| s.busy().is_none())
    });
    cx.read_entity(&h.state, |s, _| {
        let activity = s.activity.as_ref().expect("the job left its output");
        assert_eq!(activity.title, "Start cache for feature-x");
        assert!(activity.log.text().contains("It answers on 127.0.0.1:6382"));
        assert!(matches!(activity.outcome, Some(Ok(_))));
    });
    assert!(exists(cx, h, "activity-panel"));

    // The running branch offers Stop.
    update(cx, h, |s, cx| s.set_tab(Tab::Data, cx));
    wait(cx, "the tab is read again", |cx| {
        cx.read_entity(&h.state, |s, _| s.busy().is_none())
    });
    click_scrolled(cx, h, "service-view", ("service-stop", 0usize));
    wait(cx, "main was stopped", |_| {
        fake.calls().contains(&format!("down cache {}", ids.main))
    });
}

#[gpui_kit::test]
fn a_reset_asks_first_and_says_what_is_lost(cx: &mut TestAppContext) {
    let DataRig { h, fake, .. } = &data_rig(cx);
    open_dialog(cx, h, ("service-reset", 1usize), "service-reset-confirm");
    assert!(exists(cx, h, "service-reset-dialog"));
    // Cancel: nothing happens.
    cancel_dialog(cx, h);
    assert!(fake.calls().iter().all(|c| !c.starts_with("reset")));

    // Confirm: the fresh clone of the recorded source.
    open_dialog(cx, h, ("service-reset", 1usize), "service-reset-confirm");
    click(cx, h, "service-reset-confirm");
    wait(cx, "the reset ran", |_| {
        fake.calls()
            .contains(&"reset cache feature-x None".to_string())
    });
    wait(cx, "the job finished", |cx| {
        cx.read_entity(&h.state, |s, _| s.busy().is_none())
    });
    cx.read_entity(&h.state, |s, _| {
        let activity = s.activity.as_ref().unwrap();
        assert_eq!(activity.title, "Reset cache of feature-x");
        assert!(activity.log.text().contains("fresh clone"));
    });
}

/// What `devflow setup --new-instance` leaves in a workspace: another
/// instance, bound to a branch of its own.
fn replaced(old: &Receipt) -> Receipt {
    let mut next = old.clone();
    next.instance_id = Uuid::new_v4();
    if let Some(binding) = next.services.get_mut("cache") {
        binding.branch_id = Some(Uuid::new_v4());
    }
    next
}

/// The failure message of the job that ran last, when its title starts with
/// `title`.
fn failure_of(cx: &mut TestAppContext, h: &Harness, title: &str) -> Option<String> {
    cx.read_entity(&h.state, |s, _| {
        s.activity
            .as_ref()
            .filter(|a| a.title.starts_with(title))
            .and_then(|a| a.outcome.as_ref())
            .and_then(|outcome| outcome.as_ref().err().cloned())
    })
}

#[gpui_kit::test]
fn a_snapshot_opened_from_a_row_needs_the_row_to_match_its_workspace_binding(
    cx: &mut TestAppContext,
) {
    use super::service_dialogs::{bound_workspaces, snapshot_row_workspace};
    let DataRig { h, ids, .. } = &data_rig(cx);
    let (workspaces, rows) = cx.read_entity(&h.state, |s, _| {
        (
            bound_workspaces(s, "cache"),
            s.selection
                .current()
                .unwrap()
                .services
                .detail("cache")
                .unwrap()
                .branches
                .clone(),
        )
    });
    let names: Vec<_> = workspaces
        .iter()
        .map(|w| w.workspace.name.as_str())
        .collect();
    assert_eq!(names, ["main", "feature-x"]);
    // The rows of the two workspaces preselect their own workspace.
    assert_eq!(snapshot_row_workspace(&workspaces, &rows[0]), Ok(0));
    assert_eq!(snapshot_row_workspace(&workspaces, &rows[1]), Ok(1));
    // A row whose branch the workspace no longer binds is stale, whatever its
    // name says: it must not fall back to the first workspace.
    let rebound = BranchRow {
        id: Uuid::new_v4(),
        ..rows[1].clone()
    };
    let error = snapshot_row_workspace(&workspaces, &rebound).unwrap_err();
    assert!(error.contains("no longer has the branch"), "{error}");
    // Another workspace's branch under this name is stale too.
    let crossed = BranchRow {
        id: ids.main,
        ..rows[1].clone()
    };
    assert!(snapshot_row_workspace(&workspaces, &crossed).is_err());
    // An orphan has no workspace to snapshot.
    let error = snapshot_row_workspace(&workspaces, &rows[2]).unwrap_err();
    assert!(error.contains("No workspace owns"), "{error}");
}

#[gpui_kit::test]
fn an_operation_on_a_workspace_named_like_a_credential_is_titled_masked_and_still_runs(
    cx: &mut TestAppContext,
) {
    use super::service_dialogs::bound_workspaces;
    let DataRig { h, fake, .. } = &data_rig(cx);
    // The workspace's name is a legal one that looks like a credential.
    // kingfisher:ignore
    let odd = "password=hunter2";
    let (target, bound) = cx.read_entity(&h.state, |s, _| {
        (
            crate::services::ServiceTarget {
                project: s.selected_project_ref().unwrap(),
                key: "cache".into(),
            },
            bound_workspaces(s, "cache")
                .into_iter()
                .find(|w| w.workspace.name == "feature-x")
                .unwrap(),
        )
    });
    let mut renamed = bound.clone();
    renamed.workspace.name = odd.into();
    let receipt = fake.receipts.lock().unwrap()["feature-x"].clone();
    fake.bind(odd, receipt);
    cx.update_window(h.window.into(), |_, window, cx| {
        h.state.update(cx, |s, cx| {
            s.service_reset(
                target,
                renamed,
                None,
                "its recorded source".into(),
                window,
                cx,
            )
        })
    })
    .unwrap();
    wait(cx, "the reset ran against the recorded name", |_| {
        fake.calls()
            .iter()
            .any(|c| c == &format!("reset cache {odd} None"))
    });
    wait(cx, "idle", |cx| {
        cx.read_entity(&h.state, |s, _| s.busy().is_none())
    });
    cx.read_entity(&h.state, |s, _| {
        let activity = s.activity.as_ref().unwrap();
        assert_eq!(activity.title, "Reset cache of password=***");
        assert!(
            !activity.log.text().contains("hunter2"),
            "{}",
            activity.log.text()
        );
    });
}

#[gpui_kit::test]
fn a_reset_from_another_workspace_is_held_to_the_branch_that_was_shown(cx: &mut TestAppContext) {
    use super::service_dialogs::{bound_workspaces, pin_options, reset_options};
    use devflow_core::services::catalog::SourceRef;
    let DataRig { h, fake, ids, .. } = &data_rig(cx);
    let (target, bound, options, pins, main_receipt) = cx.read_entity(&h.state, |s, _| {
        let target = crate::services::ServiceTarget {
            project: s.selected_project_ref().unwrap(),
            key: "cache".into(),
        };
        let bound = bound_workspaces(s, "cache")
            .into_iter()
            .find(|w| w.workspace.name == "feature-x")
            .unwrap();
        let others: Vec<Workspace> = bound_workspaces(s, "cache")
            .into_iter()
            .map(|w| w.workspace)
            .collect();
        let detail = s.selection.current().unwrap().services.detail("cache");
        let options = reset_options(detail, &others, "feature-x");
        let pins = pin_options(s, &target, &bound.workspace, &options);
        let main_receipt = s.selection.current().unwrap().workspaces[0]
            .receipt
            .clone()
            .unwrap();
        (target, bound, options, pins, main_receipt)
    });
    // "Workspace main" resolved to main's branch when the dialog was drawn.
    let ix = options
        .iter()
        .position(|o| {
            o.source
                == Some(DataSource::Workspace {
                    name: "main".into(),
                })
        })
        .unwrap();
    let shown = pins[ix].clone().unwrap();
    assert_eq!(shown.source, SourceRef::Branch { id: ids.main });
    // The recorded source and a snapshot are identities as well.
    assert!(pins[0].is_ok(), "{:?}", pins[0]);
    assert!(
        matches!(
            pins.last().unwrap().as_ref().unwrap().source,
            SourceRef::Snapshot { .. }
        ),
        "{:?}",
        pins.last()
    );

    // `setup --new-instance` runs in main before the confirmation.
    fake.bind("main", replaced(&main_receipt));
    let mut held = bound.clone();
    held.expect.source = Some(shown.source);
    let reset = |cx: &mut TestAppContext, bound: crate::services::BoundWorkspace, refused: bool| {
        let (state, target) = (h.state.clone(), target.clone());
        cx.update_window(h.window.into(), |_, window, cx| {
            state.update(cx, |s, cx| {
                s.service_reset(
                    target,
                    bound,
                    Some(DataSource::Workspace {
                        name: "main".into(),
                    }),
                    "workspace main".into(),
                    window,
                    cx,
                )
            })
        })
        .unwrap();
        wait(cx, "the job finished", |cx| {
            if refused {
                failure_of(cx, h, "Reset cache").is_some()
            } else {
                fake.calls().iter().any(|c| c.starts_with("reset"))
            }
        });
        wait(cx, "idle", |cx| {
            cx.read_entity(&h.state, |s, _| s.busy().is_none())
        });
    };
    reset(cx, held, true);
    let message = failure_of(cx, h, "Reset cache").expect("refused");
    assert!(
        message.contains("changed since you reviewed it")
            && message.contains(&ids.main.to_string()),
        "{message}"
    );
    assert!(
        fake.calls().iter().all(|c| !c.starts_with("reset")),
        "feature-x was not reset from the replacement's data: {:?}",
        fake.calls()
    );

    // Control: with no pin, whatever main holds now is taken (the CLI's way).
    let again = install_workspaces(cx, h, fake, ids);
    fake.bind("main", replaced(&again[0]));
    let mut bound = bound;
    bound.expect.instance = again[1].instance_id;
    reset(cx, bound, false);
    assert!(fake.calls().iter().any(|c| c.starts_with("reset cache")));
}

#[gpui_kit::test]
fn a_reset_or_snapshot_confirmed_after_a_new_instance_took_the_workspace_changes_nothing(
    cx: &mut TestAppContext,
) {
    let DataRig { h, fake, ids, .. } = &data_rig(cx);
    // The dialogs show feature-x as it is now.
    open_dialog(cx, h, ("service-reset", 1usize), "service-reset-confirm");
    // `setup --new-instance` runs in a terminal while the dialog is open: the
    // window still shows the old receipt, the disk has another instance.
    let old = cx.read_entity(&h.state, |s, _| {
        s.selection.current().unwrap().workspaces[1]
            .receipt
            .clone()
            .unwrap()
    });
    fake.bind("feature-x", replaced(&old));
    click(cx, h, "service-reset-confirm");
    wait(cx, "the refusal", |cx| {
        failure_of(cx, h, "Reset cache").is_some()
            && cx.read_entity(&h.state, |s, _| s.busy().is_none())
    });
    let message = failure_of(cx, h, "Reset cache").unwrap();
    assert!(
        message.contains("changed since you reviewed it"),
        "{message}"
    );
    assert!(
        fake.calls().iter().all(|c| !c.starts_with("reset")),
        "the replacement's data was not touched: {:?}",
        fake.calls()
    );

    // The snapshot dialog shows the same workspace: same refusal. (The job's
    // refresh read the real list again, so the receipts are put back.)
    let shown = install_workspaces(cx, h, fake, ids);
    open_dialog(cx, h, "service-snapshot-create", "service-snapshot-save");
    fake.bind("feature-x", replaced(&shown[1]));
    fake.bind("main", replaced(&shown[0]));
    click(cx, h, "service-snapshot-name");
    cx.update_window(h.window.into(), |_, window, cx| {
        window.input("baseline", cx)
    })
    .unwrap();
    click(cx, h, "service-snapshot-save");
    wait(cx, "the snapshot was refused", |cx| {
        failure_of(cx, h, "Snapshot baseline").is_some()
            && cx.read_entity(&h.state, |s, _| s.busy().is_none())
    });
    let message = failure_of(cx, h, "Snapshot baseline").unwrap();
    assert!(
        message.contains("changed since you reviewed it"),
        "{message}"
    );
    assert!(fake.calls().iter().all(|c| !c.starts_with("snapshot")));
}

/// Make `id` the selected project and wait until it is loaded.
fn switch_to(cx: &mut TestAppContext, h: &Harness, id: Uuid) {
    update(cx, h, |s, cx| s.select_project(id, cx));
    wait(cx, "the project is loaded", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.selection.current().is_some_and(|m| {
                m.key.project == id && m.project.is_some() && !m.workspaces.is_empty()
            })
        })
    });
}

#[gpui_kit::test]
fn a_confirmation_opened_for_one_project_never_acts_on_another(cx: &mut TestAppContext) {
    let rig = data_rig(cx);
    let DataRig { h, fake, .. } = &rig;
    let a = cx.read_entity(&h.state, |s, _| s.selection.current().unwrap().entry.id);
    let other = another_scratch_project(&rig._scratch, "blog");
    let b = other.id;
    update(cx, h, |s, _| s.entries.push(other));

    // Delete snapshot seed@v2 of project A's cache: confirm it from B.
    open_dialog(cx, h, ("service-snapshot-delete", 0usize), "confirm");
    switch_to(cx, h, b);
    click(cx, h, "confirm");
    cx.run_until_parked();
    assert!(
        fake.calls()
            .iter()
            .all(|c| !c.starts_with("delete-snapshot")),
        "B's project must not receive A's delete: {:?}",
        fake.calls()
    );
    assert!(cx.read_entity(&h.state, |s, _| s.busy().is_none()));
    assert_eq!(
        cx.read_entity(&h.state, |s, _| s.selection.current().map(|m| m.entry.id)),
        Some(b)
    );

    // Forget "queue" of A: the same refusal.
    switch_to(cx, h, a);
    install_workspaces(cx, h, fake, &rig.ids);
    click_scrolled(cx, h, "data-tab", ("service-forget", 0usize));
    wait(cx, "the confirmation", |cx| exists(cx, h, "confirm"));
    std::thread::sleep(Duration::from_millis(350));
    switch_to(cx, h, b);
    click(cx, h, "confirm");
    cx.run_until_parked();
    assert!(
        fake.calls().iter().all(|c| !c.starts_with("forget")),
        "{:?}",
        fake.calls()
    );

    // Back in A, the same confirmation does run.
    switch_to(cx, h, a);
    install_workspaces(cx, h, fake, &rig.ids);
    click_scrolled(cx, h, "data-tab", ("service-forget", 0usize));
    wait(cx, "the confirmation", |cx| exists(cx, h, "confirm"));
    std::thread::sleep(Duration::from_millis(350));
    click(cx, h, "confirm");
    wait(cx, "A's service was forgotten", |_| {
        fake.calls().contains(&"forget queue".to_string())
    });
}

#[test]
fn the_confirmation_says_whose_server_a_mysql_copy_stops_and_for_how_long() {
    use super::service_dialogs::interruption;
    let stops = Effects {
        stops_source: Some(StoppedSource {
            branch: Uuid::new_v4(),
            name: "main".into(),
            workspace: Some("main".into()),
        }),
        last_downtime_ms: Some(1800),
    };
    let running = interruption("mysql", &stops, Some(BranchState::Running)).unwrap();
    assert_eq!(
        running,
        "mysql stops the source workspace main's server for a few seconds; its connections are interrupted. The last stop took 1.8 s."
    );
    let unsure = interruption("mysql", &stops, None).unwrap();
    assert!(
        unsure.contains("for a few seconds if it is running"),
        "{unsure}"
    );
    let stopped = interruption("mysql", &stops, Some(BranchState::Stopped)).unwrap();
    assert!(
        stopped.contains("not running now, so nothing is interrupted"),
        "{stopped}"
    );
    // No measurement yet: no figure invented. An engine that keeps serving: no line.
    let first = Effects {
        last_downtime_ms: None,
        ..stops
    };
    assert!(
        !interruption("mysql", &first, None)
            .unwrap()
            .contains("last stop")
    );
    assert_eq!(interruption("valkey", &Effects::default(), None), None);
}

#[gpui_kit::test]
fn a_snapshot_or_reset_dialog_states_the_source_it_will_stop(cx: &mut TestAppContext) {
    let DataRig { h, fake, ids, .. } = &data_rig(cx);
    // A source that keeps serving: no extra line.
    open_dialog(cx, h, "service-snapshot-create", "service-snapshot-save");
    assert!(!exists(cx, h, "service-source-stopped"));
    cancel_dialog(cx, h);

    *fake.effects.lock().unwrap() = Effects {
        stops_source: Some(StoppedSource {
            branch: ids.main,
            name: "main".into(),
            workspace: Some("main".into()),
        }),
        last_downtime_ms: Some(1800),
    };
    open_dialog(cx, h, "service-snapshot-create", "service-snapshot-save");
    assert!(exists(cx, h, "service-source-stopped"));
    cancel_dialog(cx, h);
    open_dialog(cx, h, ("service-reset", 1usize), "service-reset-confirm");
    assert!(exists(cx, h, "service-source-stopped"));
    assert!(fake.calls().iter().any(|c| c == "effects cache"));
}

#[gpui_kit::test]
fn dropping_an_orphan_and_forgetting_a_service_ask_first(cx: &mut TestAppContext) {
    let DataRig { h, fake, ids, .. } = &data_rig(cx);
    open_dialog(cx, h, ("service-orphan-drop", 0usize), "confirm");
    cancel_dialog(cx, h);
    assert!(fake.calls().iter().all(|c| !c.starts_with("drop")));
    open_dialog(cx, h, ("service-orphan-drop", 0usize), "confirm");
    click(cx, h, "confirm");
    wait(cx, "the orphan was dropped", |_| {
        fake.calls().contains(&format!("drop cache {}", ids.orphan))
    });
    wait(cx, "the job finished", |cx| {
        cx.read_entity(&h.state, |s, _| s.busy().is_none())
    });

    // A service the configuration dropped: Forget… asks too.
    click_scrolled(cx, h, "data-tab", ("service-forget", 0usize));
    wait(cx, "the confirmation", |cx| exists(cx, h, "confirm"));
    std::thread::sleep(Duration::from_millis(350));
    cancel_dialog(cx, h);
    assert!(fake.calls().iter().all(|c| !c.starts_with("forget")));
    click(cx, h, ("service-forget", 0usize));
    wait(cx, "the confirmation", |cx| exists(cx, h, "confirm"));
    std::thread::sleep(Duration::from_millis(350));
    click(cx, h, "confirm");
    wait(cx, "the service was forgotten", |_| {
        fake.calls().contains(&"forget queue".to_string())
    });
}

#[gpui_kit::test]
fn a_branch_whose_volume_is_gone_is_dropped_by_its_own_confirmed_operation(
    cx: &mut TestAppContext,
) {
    use devflow_core::remedy::Remedy;
    let DataRig { h, fake, ids, .. } = &data_rig(cx);
    let root = cx.read_entity(&h.state, |s, _| {
        s.selection.current().unwrap().entry.root.clone()
    });
    // `feature-x` owns this branch: it is no orphan, so the orphan operation
    // (which keeps what a workspace holds) is not the one that runs.
    let lost = Remedy::DropLostBranch {
        root: root.clone(),
        key: "cache".into(),
        branch: ids.feature,
    };
    assert!(lost.destructive());
    let run = |cx: &mut TestAppContext| {
        cx.update_window(h.window.into(), |_, window, cx| {
            super::remedies::run_scoped(&h.state, &lost, window, cx)
        })
        .unwrap();
        wait(cx, "the confirmation", |cx| exists(cx, h, "confirm"));
        std::thread::sleep(Duration::from_millis(350));
    };
    // Nothing runs before the person confirms.
    run(cx);
    cancel_dialog(cx, h);
    assert!(fake.calls().iter().all(|c| !c.contains("drop")));
    run(cx);
    click(cx, h, "confirm");
    wait(cx, "the branch was dropped", |_| {
        fake.calls()
            .contains(&format!("drop-lost cache {}", ids.feature))
    });
    wait(cx, "the job finished", |cx| {
        cx.read_entity(&h.state, |s, _| s.busy().is_none())
    });
    assert!(
        fake.calls().iter().all(|c| !c.starts_with("drop cache")),
        "{:?}",
        fake.calls()
    );
    // The words do not claim that nobody owns it.
    let text = super::service_dialogs::drop_lost_lines("cache", ids.feature).join(" ");
    assert!(!text.contains("No workspace"), "{text}");
    assert!(text.contains("no longer exists"), "{text}");
    assert!(!text.contains("devflow "), "{text}");
}

#[gpui_kit::test]
fn an_unreadable_receipt_shows_why_and_offers_no_orphan_to_drop(cx: &mut TestAppContext) {
    let DataRig { h, fake, ids, .. } = &data_rig(cx);
    assert!(exists(cx, h, ("service-orphan-drop", 0usize)));
    // The scan read every receipt: the branch no workspace carries is an
    // orphan, and the row says "no workspace".
    assert!(exists(cx, h, ("service-branch-orphan", 2usize)));
    assert!(!exists(cx, h, ("service-branch-owner-unknown", 2usize)));

    // Core's scan could not read feature-x's receipt: it lists no orphan and
    // says why. The page must not read that as "no orphans", and the branch
    // is not "no workspace" either: its owner is unknown.
    update(cx, h, |s, cx| {
        let orphans = &mut s.selection.current_mut().unwrap().services.orphans;
        orphans.branches.clear();
        orphans.incomplete = vec!["feature-x: invalid receipt: expected value".into()];
        cx.notify();
    });
    frame(cx, h, |_| {});
    assert!(exists(cx, h, ("service-branch-owner-unknown", 2usize)));
    assert!(!exists(cx, h, ("service-branch-orphan", 2usize)));
    assert!(exists(cx, h, "service-ownership-unknown"));
    assert!(!exists(cx, h, ("service-orphan-drop", 0usize)));
    assert!(!exists(cx, h, "service-orphans"));
    let cards = cx.read_entity(&h.state, |s, _| data_cards(s.selection.current().unwrap()));
    assert!(cards[0].owners_unknown, "{:?}", cards[0]);
    assert_eq!(cards[0].orphans, 0);
    assert!(
        super::service_view::ownership_unknown_text(&["feature-x: invalid receipt".into()])
            .contains("feature-x: invalid receipt")
    );

    // A confirmation drawn before the receipt broke: core refuses, and the
    // window shows its error instead of dropping anything.
    update(cx, h, |s, cx| {
        let model = s.selection.current_mut().unwrap();
        model.services.orphans.incomplete.clear();
        model.services.orphans.branches.push(OrphanBranch {
            service: "cache".into(),
            id: ids.orphan,
            name: "old-feature".into(),
            reason: String::new(),
        });
        cx.notify();
    });
    frame(cx, h, |_| {});
    fake.ownership_unknown
        .store(true, std::sync::atomic::Ordering::SeqCst);
    open_dialog(cx, h, ("service-orphan-drop", 0usize), "confirm");
    click(cx, h, "confirm");
    wait(cx, "the refusal", |cx| {
        failure_of(cx, h, "Drop old-feature").is_some()
            && cx.read_entity(&h.state, |s, _| s.busy().is_none())
    });
    let message = failure_of(cx, h, "Drop old-feature").unwrap();
    assert!(
        message.contains("cannot tell whether a workspace owns"),
        "{message}"
    );
    assert!(fake.calls().iter().all(|c| !c.starts_with("drop")));
}

#[gpui_kit::test]
fn snapshots_are_created_from_a_dialog_and_deleted_after_a_confirmation(cx: &mut TestAppContext) {
    let DataRig { h, fake, ids, .. } = &data_rig(cx);
    open_dialog(cx, h, "service-snapshot-create", "service-snapshot-save");
    click(cx, h, "service-snapshot-name");
    cx.update_window(h.window.into(), |_, window, cx| {
        window.input("baseline", cx)
    })
    .unwrap();
    click(cx, h, "service-snapshot-save");
    wait(cx, "the snapshot was made", |_| {
        fake.calls()
            .iter()
            .any(|c| c.starts_with("snapshot cache ") && c.ends_with(" baseline"))
    });
    wait(cx, "the job finished", |cx| {
        cx.read_entity(&h.state, |s, _| s.busy().is_none())
    });

    open_dialog(cx, h, ("service-snapshot-delete", 0usize), "confirm");
    click(cx, h, "confirm");
    wait(cx, "the snapshot was deleted", |_| {
        fake.calls()
            .contains(&format!("delete-snapshot cache {}", ids.seed))
    });
}

#[gpui_kit::test]
fn a_snapshot_is_drawn_masked_and_still_acted_on_by_what_it_is(cx: &mut TestAppContext) {
    let DataRig { h, fake, ids, .. } = &data_rig(cx);
    let (label, source) = cx.read_entity(&h.state, |s, _| {
        let detail = s
            .selection
            .current()
            .and_then(|m| m.services.detail("cache").cloned())
            .unwrap();
        let odd = detail.snapshots[1].clone();
        let options = super::service_dialogs::reset_options(Some(&detail), &[], "x");
        (odd.label(), options.last().cloned().unwrap())
    });
    // What is drawn.
    assert_eq!(label, "password=***@v1");
    assert_eq!(source.label, "Snapshot password=***@v1");
    // What is asked for: the recorded name, not the drawn one.
    assert_eq!(
        source.source,
        Some(DataSource::Snapshot {
            name: ODD_SNAPSHOT.into(),
            version: Some(1)
        })
    );
    let model = cx.read_entity(&h.state, |s, _| {
        format!("{:?}", s.selection.current().unwrap().services)
    });
    assert!(!model.contains("hunter2"), "{model}");

    // Delete submits the snapshot's id, and the job is titled with the label.
    open_dialog(cx, h, ("service-snapshot-delete", 1usize), "confirm");
    click(cx, h, "confirm");
    wait(cx, "the snapshot was deleted", |_| {
        fake.calls()
            .contains(&format!("delete-snapshot cache {}", ids.odd))
    });
    wait(cx, "the job finished", |cx| {
        cx.read_entity(&h.state, |s, _| s.busy().is_none())
    });
    cx.read_entity(&h.state, |s, _| {
        let title = &s.activity.as_ref().unwrap().title;
        assert!(
            title.contains("password=***@v1") && !title.contains("hunter2"),
            "{title}"
        );
    });
}

#[gpui_kit::test]
fn reconcile_and_measure_run_as_jobs_and_report_in_the_log(cx: &mut TestAppContext) {
    let DataRig { h, fake, .. } = &data_rig(cx);
    click(cx, h, "service-reconcile");
    wait(cx, "reconcile ran", |cx| {
        fake.calls().contains(&"reconcile cache".to_string())
            && cx.read_entity(&h.state, |s, _| s.busy().is_none())
    });
    cx.read_entity(&h.state, |s, _| {
        let log = s.activity.as_ref().unwrap().log.text();
        assert!(log.contains("rolled back a clone of feature-x"), "{log}");
    });
    click(cx, h, "service-measure");
    wait(cx, "the measurement ran", |cx| {
        fake.calls().contains(&"measure cache".to_string())
            && cx.read_entity(&h.state, |s, _| s.busy().is_none())
    });
}

// ---- connections ----------------------------------------------------------------

#[gpui_kit::test]
fn the_detail_lists_a_container_binding_beside_postgres_with_both_endpoints(
    cx: &mut TestAppContext,
) {
    let h = open(cx, install_loaded);
    update(cx, &h, |s, cx| {
        let model = s.selection.current_mut().unwrap();
        let receipt = model.workspaces[0].receipt.as_mut().unwrap();
        receipt.services.insert(
            "cache".into(),
            ServiceBinding::container(
                Engine::Valkey,
                Uuid::new_v4(),
                REDIS_URL.into(),
                Some("main.cache.shop-0b3c9d2e.devflow.orb.local:6379".into()),
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
        cx.notify();
    });
    frame(cx, &h, |_| {});
    for target in [
        "connection-copy-db",
        "connection-copy-cache",
        "connection-copy-container-cache",
    ] {
        assert!(exists(cx, &h, target), "{target}");
    }
    // The copy button of the cache puts the whole URL on the clipboard.
    click_scrolled(cx, &h, "workspace-detail", "connection-copy-cache");
    let copied = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(copied.as_deref(), Some(REDIS_URL));
}

// ---- pitchfork: install on a click, proxy commands only copied ------------------

/// A directory where `pitchfork` does not exist yet and `installer.sh`
/// creates it (a copy of the fake), leaving `ran` behind. The plan names the
/// installer as the command devflow runs.
struct InstallRig {
    dir: tempfile::TempDir,
    plan: InstallPlan,
}

impl InstallRig {
    fn new() -> InstallRig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        std::fs::write(path.join("pitchfork.src"), FAKE).unwrap();
        // The proxy is off, so the panel has commands to show.
        std::fs::write(
            path.join("proxy.json"),
            r#"{"enabled": false, "projects": []}"#,
        )
        .unwrap();
        let installer = path.join("installer.sh");
        std::fs::write(
            &installer,
            "#!/bin/sh\nd=$(dirname \"$0\")\ncp \"$d/pitchfork.src\" \"$d/pitchfork\"\nchmod +x \"$d/pitchfork\"\necho ran > \"$d/ran\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&installer, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let plan = InstallPlan {
            run: Some(vec![installer.display().to_string()]),
            commands: vec![
                installer.display().to_string(),
                "brew install pitchfork".into(),
                "cargo install pitchfork-cli".into(),
            ],
        };
        InstallRig { dir, plan }
    }

    fn source(&self) -> PitchforkSource {
        PitchforkSource::Fixed {
            pitchfork: Some(Pitchfork::at(self.dir.path().join("pitchfork"))),
            install: Some(self.plan.clone()),
        }
    }

    fn ran(&self) -> bool {
        self.dir.path().join("ran").exists()
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(|line| line.split('|').nth(1).unwrap_or("").to_string())
            .collect()
    }
}

fn pitchfork_usable(cx: &mut TestAppContext, h: &Harness) -> bool {
    cx.read_entity(&h.state, |s, _| {
        s.selection
            .current()
            .is_some_and(|m| m.pitchfork.detection.as_ref().is_some_and(|d| d.usable()))
    })
}

#[gpui_kit::test]
fn pitchfork_is_installed_by_a_click_after_the_command_was_shown_and_its_proxy_commands_are_never_run(
    cx: &mut TestAppContext,
) {
    let rig = InstallRig::new();
    let scratch = scratch_project();
    let h = open_scratch(cx, &scratch);
    update(cx, &h, |s, _| s.pitchfork = rig.source());
    update(cx, &h, |s, cx| s.set_tab(Tab::Daemons, cx));
    wait(cx, "pitchfork is looked for", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.selection
                .current()
                .is_some_and(|m| m.pitchfork.detection.is_some())
        })
    });

    // The card shows the command and the way to run it; nothing ran yet.
    assert!(exists(cx, &h, "pitchfork-install"));
    assert!(exists(cx, &h, "pitchfork-install-run"));
    let offer = cx.read_entity(&h.state, |s, _| {
        s.selection.current().unwrap().pitchfork.offer()
    });
    assert_eq!(
        super::pitchfork_panel::install_command(&offer).as_deref(),
        rig.plan.run.as_ref().map(|argv| argv.join(" ")).as_deref(),
        "the command shown is the one that runs"
    );
    assert!(!rig.ran(), "showing the offer runs nothing");
    // The other ways are for the person to copy.
    click(cx, &h, ("copy-install", 1usize));
    let copied = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(copied.as_deref(), Some("brew install pitchfork"));
    assert!(!rig.ran());

    // The click installs it.
    click(cx, &h, "pitchfork-install-run");
    wait(cx, "the installer ran and pitchfork is usable", |cx| {
        rig.ran() && pitchfork_usable(cx, &h)
    });
    cx.read_entity(&h.state, |s, _| {
        assert!(
            matches!(&s.pitchfork_install, PitchforkInstall::Done(m) if m.contains("is installed")),
            "{:?}",
            s.pitchfork_install
        );
    });
    assert!(
        !exists(cx, &h, "pitchfork-install"),
        "the card goes away once pitchfork works"
    );

    // The proxy is off: its commands are shown to copy and nothing runs them.
    wait(cx, "the proxy card", |cx| exists(cx, &h, "pitchfork-proxy"));
    click(cx, &h, ("copy-proxy", 2usize));
    let copied = cx
        .update(|cx| cx.read_from_clipboard().and_then(|item| item.text()))
        .unwrap();
    assert!(copied.contains("pitchfork proxy setup"), "{copied}");
    let calls = rig.calls();
    assert!(
        !calls.iter().any(|c| c.starts_with("settings")
            || c.contains("proxy setup")
            || c.starts_with("supervisor stop")),
        "devflow never runs the proxy commands: {calls:?}"
    );
}

#[gpui_kit::test]
fn without_mise_there_is_no_install_button_only_commands_to_copy(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    update(cx, &h, |s, cx| {
        s.tab = Tab::Daemons;
        s.selection.current_mut().unwrap().pitchfork = PitchforkModel {
            loaded: true,
            detection: Some(devflow_core::pitchfork::Detection {
                path: None,
                installed: false,
                version: None,
                missing_features: Vec::new(),
                problem: None,
            }),
            install: Some(InstallPlan {
                run: None,
                commands: vec![
                    "# install mise first: https://mise.jdx.dev, then".into(),
                    "mise use -g pitchfork@latest".into(),
                    "brew install pitchfork".into(),
                ],
            }),
            ..Default::default()
        };
        cx.notify();
    });
    frame(cx, &h, |_| {});
    assert!(exists(cx, &h, "pitchfork-install"));
    assert!(!exists(cx, &h, "pitchfork-install-run"));
    assert!(exists(cx, &h, ("copy-install", 0usize)));
    assert!(exists(cx, &h, ("copy-install", 2usize)));
}

#[gpui_kit::test]
fn the_wizard_offers_pitchfork_when_the_plan_involves_processes(cx: &mut TestAppContext) {
    use super::onboarding::Step;
    let rig = InstallRig::new();
    let w = wizard(cx);
    update(cx, &w.h, |s, cx| {
        s.pitchfork = rig.source();
        s.pitchfork_epoch += 1;
        cx.notify();
    });
    press_key(cx, &w.h, "enter");
    assert_eq!(cx.read_entity(&w.wizard, |v, _| v.step()), Step::Proposal);
    assert!(
        cx.read_entity(&w.wizard, |v, _| v.proposes_processes()),
        "the fixture project has a dev process"
    );
    wait(cx, "pitchfork is offered", |cx| {
        exists(cx, &w.h, "wizard-pitchfork-install")
    });
    assert!(exists(cx, &w.h, "wizard-pitchfork-install-run"));
    assert!(!rig.ran(), "the wizard runs nothing by itself");

    click_scrolled(cx, &w.h, "wizard-body", "wizard-pitchfork-install-run");
    wait(cx, "installed", |cx| {
        rig.ran() && !exists(cx, &w.h, "wizard-pitchfork-install")
    });
    // Installed: its state is a line, and the proxy's commands are offered.
    wait(cx, "the proxy card", |cx| {
        exists(cx, &w.h, "wizard-pitchfork-proxy")
    });
}

// ---- the overview ---------------------------------------------------------------

fn overview_model() -> crate::model::ProjectModel {
    let dir = tempfile::tempdir().unwrap();
    super::ui_tests::loaded_model(dir.path())
}

#[test]
fn the_overview_counts_workspaces_services_processes_and_what_copy_on_write_saved() {
    use super::{
        overview::{ProcessReading, Saved, numbers},
        workspace_detail::{MeasureKey, PathSize},
    };
    use crate::activity::Activity;
    use devflow_core::{cow::Measure, receipt::PayloadOutcome};
    let mut model = overview_model();

    // Workspaces by state, data services with their branches, nothing running
    // and nothing cloned: said as it is.
    let first = numbers(&model, &[], None);
    assert_eq!(first.workspaces, 3);
    let states: Vec<_> = first
        .by_state
        .iter()
        .map(|(state, n)| (state.label(), *n))
        .collect();
    assert_eq!(
        states,
        [("ready", 1), ("needs attention", 1), ("not set up", 1)]
    );
    assert_eq!(
        first
            .services
            .iter()
            .map(|s| (s.key.as_str(), s.engine.as_str(), s.branches))
            .collect::<Vec<_>>(),
        [("db", "postgres", Some(3))]
    );
    assert_eq!(first.processes, ProcessReading::NotAsked);
    assert_eq!(first.saved, Saved::Nothing);
    assert!(
        first
            .last_activity
            .as_deref()
            .is_some_and(|text| text.contains("was set up")),
        "{:?}",
        first.last_activity
    );

    // Container services count beside PostgreSQL, whatever their engine.
    let (fixture, _) = services_fixture();
    model.services = fixture;
    let with_services = numbers(&model, &[], None);
    let lines: Vec<_> = with_services
        .services
        .iter()
        .map(|s| (s.key.as_str(), s.engine.as_str(), s.branches))
        .collect();
    assert_eq!(
        lines,
        [
            ("db", "postgres", Some(3)),
            ("cache", "valkey", Some(3)),
            ("shop-data", "mysql", Some(1))
        ]
    );

    // Processes: what pitchfork reports over every workspace.
    use super::ui_tests::{daemon, process_status};
    model.pitchfork.loaded = true;
    model.pitchfork.all = vec![
        process_status(
            "main",
            "/tmp/shop/main",
            vec![
                daemon("web", "running", &[3000], None, false),
                daemon("api", "not started", &[], None, true),
            ],
        ),
        process_status(
            "feature-x",
            "/tmp/shop/feature-x",
            vec![daemon("web", "running", &[3001], None, false)],
        ),
        process_status("adopted", "/tmp/shop/adopted", Vec::new()),
    ];
    let running = numbers(&model, &[], None);
    assert_eq!(running.processes, ProcessReading::Known);
    assert_eq!(
        (running.daemons_running, running.workspaces_running),
        (2, 2)
    );

    // A database cloned from another branch by copy-on-write counts, by its
    // logical size; one that started empty does not.
    model.database.branches[1].source_kind = "branch".into();
    assert_eq!(
        numbers(&model, &[], None).saved,
        Saved::UpTo(392 * 1024 * 1024)
    );

    // A cloned payload counts by its allocated blocks once measured, and is
    // measuring until then.
    {
        let status = &mut model.workspaces[1];
        let receipt = status.receipt.as_mut().unwrap();
        receipt.payload = vec![devflow_core::receipt::PayloadRecord {
            path: "node_modules".into(),
            outcome: PayloadOutcome::Cloned,
        }];
    }
    assert_eq!(numbers(&model, &[], None).saved, Saved::Measuring);
    let key = MeasureKey::of(&model.workspaces[1]).unwrap();
    let sizes = |partial: bool| {
        vec![(
            key.clone(),
            vec![PathSize {
                path: "node_modules".into(),
                outcome: PayloadOutcome::Cloned,
                measure: Ok(Measure {
                    apparent_bytes: 100 * 1024 * 1024,
                    allocated_bytes: Some(110 * 1024 * 1024),
                    entries: 9000,
                    partial,
                }),
            }],
        )]
    };
    assert_eq!(
        numbers(&model, &sizes(false), None).saved,
        Saved::UpTo((110 + 392) * 1024 * 1024),
        "allocated blocks, not the sum of the file lengths"
    );
    // Files that could not be read make it a count, not a bound.
    let incomplete = numbers(&model, &sizes(true), None).saved;
    assert!(matches!(incomplete, Saved::AtLeast(_)), "{incomplete:?}");
    assert!(
        !incomplete.text().contains("up to"),
        "{}",
        incomplete.text()
    );
    for saved in [Saved::UpTo(1), Saved::Nothing, Saved::Measuring] {
        assert!(!saved.text().to_lowercase().contains("zero disk"));
    }

    // The last activity is the app's own operation, with how it ended.
    let mut activity = Activity::new(model.key, "Create workspace feature-y");
    assert_eq!(
        numbers(&model, &[], Some(&activity))
            .last_activity
            .as_deref(),
        Some("Create workspace feature-y: running")
    );
    activity.outcome = Some(Err("boom".into()));
    assert_eq!(
        numbers(&model, &[], Some(&activity))
            .last_activity
            .as_deref(),
        Some("Create workspace feature-y: failed")
    );
}

#[gpui_kit::test]
fn the_workspaces_tab_opens_on_the_overview_with_one_primary_action(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    update(cx, &h, |s, cx| {
        s.selection.current_mut().unwrap().services = services_fixture().0;
        cx.notify();
    });
    frame(cx, &h, |_| {});
    for target in [
        "overview",
        "overview-workspaces",
        "overview-services",
        "overview-processes",
        "overview-saved",
        "overview-activity",
    ] {
        assert!(exists(cx, &h, target), "{target}");
    }
    assert!(visible(cx, &h, "new-workspace"), "the one primary action");
    click(cx, &h, "new-workspace");
    wait(cx, "the New workspace dialog", |cx| {
        exists(cx, &h, "create")
    });
}

// ---- polish: the progress indicator, the pills, the empty states -------------

#[gpui_kit::test]
fn the_titlebar_shows_what_runs_for_as_long_as_it_runs(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    frame(cx, &h, |_| {});
    assert!(!exists(cx, &h, "titlebar-progress"), "idle");

    // A project's operation (a setup, a clone, a reset…).
    update(cx, &h, |s, cx| {
        let project = s.selection.current().unwrap().key.project;
        s.mutating.insert(project, "Reset cache".into());
        cx.notify();
    });
    assert!(exists(cx, &h, "titlebar-progress"));
    update(cx, &h, |s, cx| {
        s.mutating.clear();
        cx.notify();
    });
    assert!(!exists(cx, &h, "titlebar-progress"), "it went away");

    // The install of pitchfork has no project operation behind it.
    update(cx, &h, |s, cx| {
        s.pitchfork_install = PitchforkInstall::Running("mise use -g pitchfork".into());
        cx.notify();
    });
    assert!(exists(cx, &h, "titlebar-progress"));
    update(cx, &h, |s, cx| {
        s.pitchfork_install = PitchforkInstall::Done("installed".into());
        cx.notify();
    });
    assert!(!exists(cx, &h, "titlebar-progress"));
}

#[test]
fn every_pill_says_what_its_state_means_and_no_two_states_say_the_same() {
    use crate::model::{ServerBadge, WorkspaceState};
    fn distinct(what: &str, helps: Vec<&str>) {
        for help in &helps {
            assert!(help.len() > 20, "{what}: {help:?} says too little");
        }
        let mut sorted = helps.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), helps.len(), "{what}: two states, one text");
    }
    distinct(
        "workspace",
        [
            WorkspaceState::Ready,
            WorkspaceState::NeedsAttention,
            WorkspaceState::InProgress,
            WorkspaceState::NotSetUp,
            WorkspaceState::Busy,
            WorkspaceState::Missing,
        ]
        .map(WorkspaceState::help)
        .to_vec(),
    );
    distinct(
        "server",
        [
            ServerBadge::Running,
            ServerBadge::Stopped,
            ServerBadge::Unreachable,
            ServerBadge::NotConfigured,
        ]
        .map(ServerBadge::help)
        .to_vec(),
    );
    distinct(
        "branch",
        [
            BranchState::Running,
            BranchState::Stopped,
            BranchState::NotStarted,
            BranchState::Missing,
            BranchState::Creating,
            BranchState::Deleting,
            BranchState::Unknown,
        ]
        .map(BranchState::help)
        .to_vec(),
    );
    let cows: Vec<String> = [
        CowCapability::Verified,
        CowCapability::Copy,
        CowCapability::Unknown,
    ]
    .map(|capability| {
        Cow {
            capability,
            note: None,
            measured_at: None,
        }
        .help()
    })
    .to_vec();
    distinct("cow", cows.iter().map(String::as_str).collect());
}

#[gpui_kit::test]
fn the_pills_of_a_workspace_and_a_service_are_all_there_to_hover(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    // The workspace list and the selected workspace's detail carry their
    // pills (the list has one per row, hence the scope).
    let mut found = false;
    frame(cx, &h, |window| {
        found = window
            .within("workspace-detail")
            .try_find("workspace-state")
            .is_some();
    });
    assert!(found, "the detail's state pill");
    // ... and a container service's page its own, one per fact.
    let DataRig { h, .. } = &data_rig(cx);
    for target in ["service-cow", "service-engine"] {
        assert!(exists(cx, h, target), "{target}");
    }
}

#[gpui_kit::test]
fn an_empty_logs_tab_has_one_primary_action_that_leads_to_the_workspaces(cx: &mut TestAppContext) {
    let scratch = scratch_project();
    let h = open_scratch(cx, &scratch);
    update(cx, &h, |s, cx| s.set_tab(Tab::Logs, cx));
    assert!(visible(cx, &h, "logs-open-workspaces"));
    click(cx, &h, "logs-open-workspaces");
    assert_eq!(
        cx.read_entity(&h.state, |s, _| s.tab),
        Tab::Workspaces,
        "the action leads where logs come from"
    );
}

#[gpui_kit::test]
fn a_workspace_without_daemons_offers_to_detect_them(cx: &mut TestAppContext) {
    let fake = Fake::new();
    let scratch = scratch_project();
    let h = open_scratch(cx, &scratch);
    update(cx, &h, |s, cx| {
        s.pitchfork = fake.source();
        s.set_tab(Tab::Daemons, cx);
    });
    wait(cx, "the daemons are read", |cx| {
        cx.read_entity(&h.state, |s, _| {
            s.selection
                .current()
                .is_some_and(|m| m.pitchfork.status.is_some())
        })
    });
    assert!(visible(cx, &h, "daemons-detect"));
    // The wizard opens, on the scan of the repository.
    click(cx, &h, "daemons-detect");
    wait(cx, "the wizard", |cx| exists(cx, &h, "wizard-detected"));
}

#[gpui_kit::test]
fn a_service_without_branches_offers_a_new_workspace(cx: &mut TestAppContext) {
    let DataRig { h, .. } = &data_rig(cx);
    update(cx, h, |s, cx| {
        let model = s.selection.current_mut().unwrap();
        for detail in model.services.details.as_mut().unwrap() {
            detail.branches.clear();
        }
        cx.notify();
    });
    frame(cx, h, |_| {});
    click_scrolled(cx, h, "service-view", "service-new-workspace");
    wait(cx, "the New workspace dialog", |cx| exists(cx, h, "create"));
}

#[gpui_kit::test]
fn postgres_without_branches_or_snapshots_offers_the_two_ways_to_get_some(cx: &mut TestAppContext) {
    let h = open(cx, install_loaded);
    update(cx, &h, |s, cx| {
        let database = &mut s.selection.current_mut().unwrap().database;
        database.branches.clear();
        database.snapshots.clear();
        s.tab = Tab::Data;
        cx.notify();
    });
    frame(cx, &h, |_| {});
    assert!(exists(cx, &h, "branches-new-workspace"));
    assert!(exists(cx, &h, "snapshots-create"));
    click(cx, &h, "branches-new-workspace");
    wait(cx, "the New workspace dialog", |cx| {
        exists(cx, &h, "create")
    });
}

#[gpui_kit::test]
fn a_state_that_could_not_be_read_is_unknown_never_zero(cx: &mut TestAppContext) {
    use super::overview::{ProcessReading, numbers, processes_tile};
    use super::ui_tests::{daemon, process_status};
    let dir = tempfile::tempdir().unwrap();
    let mut model = super::ui_tests::loaded_model(dir.path());
    model.pitchfork.loaded = true;
    let failed = |name: &str| {
        let mut status = process_status(name, &format!("/tmp/shop/{name}"), Vec::new());
        // What core keeps when the listing fails.
        status.error = Some("pitchfork list failed: exit 1".into());
        status.observation =
            Observation::from_reasons(vec!["pitchfork list failed: exit 1".into()]);
        status
    };

    // A supervisor that crashed after devflow started daemons: no listing,
    // no error, and the observation says why nothing is known.
    let mut crashed = process_status("main", "/tmp/shop/main", Vec::new());
    crashed.supervisor_up = Some(false);
    crashed.observation = Observation::from_reasons(vec![
        "pitchfork's supervisor is down, and devflow started daemons for this workspace".into(),
    ]);
    model.pitchfork.all = vec![crashed.clone()];
    let (value, sub) = processes_tile(&numbers(&model, &[], None));
    assert_eq!(value, "unknown", "not 0 running");
    assert!(sub.contains("supervisor is down"), "{sub}");

    // Every listing failed: unknown, with the reason.
    model.pitchfork.all = vec![failed("main"), failed("feature-x")];
    let all_failed = numbers(&model, &[], None);
    assert!(matches!(
        all_failed.processes,
        ProcessReading::Unknown { .. }
    ));
    let (value, sub) = processes_tile(&all_failed);
    assert_eq!(value, "unknown");
    assert!(sub.contains("pitchfork list failed"), "{sub}");

    // One failed beside one that runs a daemon: a lower bound, not a count.
    model.pitchfork.all = vec![
        process_status(
            "main",
            "/tmp/shop/main",
            vec![daemon("web", "running", &[3000], None, false)],
        ),
        failed("feature-x"),
    ];
    let partial = numbers(&model, &[], None);
    let (value, sub) = processes_tile(&partial);
    assert_eq!(value, "at least 1 running");
    assert!(
        sub.starts_with("1 of 2 workspaces could not be read"),
        "{sub}"
    );

    // The whole model failed (the call itself): unknown as well.
    model.pitchfork.all = Vec::new();
    model.pitchfork.error = Some("the call panicked".into());
    let (value, _) = processes_tile(&numbers(&model, &[], None));
    assert_eq!(value, "unknown");

    // A server that is not reachable lists no branches: that is not "0".
    let mut db = model.database.clone();
    db.status.as_mut().unwrap().reachable = false;
    model.database = db;
    let line = &numbers(&model, &[], None).services[0];
    assert_eq!(line.branches, None);
    let card = &data_cards(&model)[0];
    assert_eq!((card.branches, card.snapshots), (None, None));
    assert_eq!(counts_text(card), "branches unavailable");

    // A container service whose Docker could not be asked: unavailable with
    // the reason, and a catalog that could not be read has no counts.
    let (mut fixture, _) = services_fixture();
    {
        let detail = &mut fixture.details.as_mut().unwrap()[0];
        detail.docker = None;
        detail.branches.clear();
        detail.error = Some("Cannot connect to the Docker daemon".into());
    }
    fixture.counts[1].known = false;
    model.services = fixture;
    let cards = data_cards(&model);
    let cache = cards.iter().find(|c| c.key == "cache").unwrap();
    assert_eq!(
        cache.running,
        Running::Unavailable("Cannot connect to the Docker daemon".into())
    );
    let shop = cards.iter().find(|c| c.key == "shop-data").unwrap();
    assert_eq!(shop.branches, None);
    assert_eq!(counts_text(shop), "branches unavailable");
    let lines = numbers(&model, &[], None).services;
    assert_eq!(lines.last().unwrap().branches, None);
    let _ = cx;
}

#[gpui_kit::test]
fn the_data_tab_says_when_docker_could_not_be_asked_instead_of_listing_nothing(
    cx: &mut TestAppContext,
) {
    let DataRig { h, .. } = &data_rig(cx);
    update(cx, h, |s, cx| {
        let detail = &mut s
            .selection
            .current_mut()
            .unwrap()
            .services
            .details
            .as_mut()
            .unwrap()[0];
        detail.docker = None;
        detail.branches.clear();
        detail.error = Some("Cannot connect to the Docker daemon".into());
        cx.notify();
    });
    frame(cx, h, |_| {});
    assert!(exists(cx, h, "service-error"));
    assert!(
        !exists(cx, h, "service-new-workspace"),
        "no empty-state invitation"
    );
}

// ---- remedies as buttons -------------------------------------------------------------

#[gpui_kit::test]
fn a_check_offers_its_fixes_as_buttons_that_call_the_app(cx: &mut TestAppContext) {
    use devflow_core::remedy::Remedy;
    let h = open(cx, install_loaded);
    let path = main_path(cx, &h);
    let fixes = vec![
        Remedy::Trust {
            workspace: path.clone(),
        },
        Remedy::InstallGitHook { root: path.clone() },
    ];
    update(cx, &h, |s, _| {
        s.doctor = DoctorSource::Fixed(vec![
            Check {
                name: "hook-trust".into(),
                status: CheckStatus::Warn,
                detail: "the hooks have not been approved".into(),
                hint: Some("review and approve them in that workspace".into()),
                remedies: fixes.clone(),
            },
            Check {
                name: "git".into(),
                status: CheckStatus::Ok,
                detail: "libgit2".into(),
                hint: None,
                remedies: fixes.clone(),
            },
        ]);
    });
    cx.update_window(h.window.into(), |_, window, cx| {
        super::doctor::open(&h.state, path.clone(), window, cx)
    })
    .unwrap();
    wait(cx, "the doctor's checks", |cx| {
        exists(cx, &h, "doctor-checks")
    });
    // One button per remedy of the check that is wrong, none for the one that
    // is fine; each is labelled in plain words, with no command line.
    for (index, fix) in fixes.iter().enumerate() {
        let id = super::setup_tests::id(format!("doctor-fix-0-{index}-{}", fix.kind()));
        assert!(exists(cx, &h, id), "{}", fix.kind());
        assert!(!fix.label().contains("devflow "), "{}", fix.label());
    }
    assert!(!exists(
        cx,
        &h,
        super::setup_tests::id("doctor-fix-1-0-trust".into())
    ));
    // The button runs the app's own operation: approving is the Setup tab's.
    click(
        cx,
        &h,
        super::setup_tests::id("doctor-fix-0-0-trust".into()),
    );
    assert_eq!(cx.read_entity(&h.state, |s, _| s.tab), Tab::Setup);
    assert!(
        !exists(cx, &h, "doctor-checks"),
        "the dialog closes when a fix starts"
    );
}

#[gpui_kit::test]
fn a_failed_operation_shows_the_fix_core_names_as_a_button(cx: &mut TestAppContext) {
    use devflow_core::remedy::Remedy;
    let h = open(cx, install_loaded);
    let path = main_path(cx, &h);
    update(cx, &h, |s, _| {
        let key = s.selection.current().unwrap().key;
        s.activity = Some(crate::activity::Activity::new(key, "Set up main"));
    });
    finish_with(
        cx,
        &h,
        FailureHint::None,
        Err(Failure {
            message: "hooks failed; 'main' is kept".into(),
            kind: FailureKind::Other,
            refusal: None,
            remedies: vec![Remedy::Trust {
                workspace: path.clone(),
            }]
            .into(),
        }),
    );
    frame(cx, &h, |_| {});
    assert!(exists(
        cx,
        &h,
        super::setup_tests::id("activity-fix-0-trust".into())
    ));
    click(
        cx,
        &h,
        super::setup_tests::id("activity-fix-0-trust".into()),
    );
    assert_eq!(cx.read_entity(&h.state, |s, _| s.tab), Tab::Setup);

    // A fix for a project that is not the open one changes nothing.
    update(cx, &h, |s, cx| s.go_to_tab(Tab::Workspaces, cx));
    cx.update_window(h.window.into(), |_, window, cx| {
        super::remedies::run_scoped(
            &h.state,
            &Remedy::InstallGitHook {
                root: PathBuf::from("/somewhere/else"),
            },
            window,
            cx,
        )
    })
    .unwrap();
    assert!(cx.read_entity(&h.state, |s, _| s.busy().is_none()));
}

#[gpui_kit::test]
fn a_hook_somebody_else_wrote_is_refused_in_plain_words_and_added_to_after_confirming(
    cx: &mut TestAppContext,
) {
    use devflow_core::remedy::Remedy;
    let scratch = scratch_project();
    let h = open_scratch(cx, &scratch);
    let root = scratch.entry.root.clone();
    let hook = root.join(".git/hooks/post-checkout");
    let original = "#!/bin/sh\necho mine\n";
    std::fs::write(&hook, original).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let run = |cx: &mut TestAppContext, remedy: &Remedy| {
        cx.update_window(h.window.into(), |_, window, cx| {
            super::remedies::run_scoped(&h.state, remedy, window, cx)
        })
        .unwrap();
    };

    // Installing is refused. The message is plain words (no shell line in a
    // window); the way on is a button.
    run(cx, &Remedy::InstallGitHook { root: root.clone() });
    settle(cx, &h, |s| {
        s.busy().is_none()
            && s.activity
                .as_ref()
                .is_some_and(|a| a.title.starts_with("Install the Git hook") && a.outcome.is_some())
    });
    let message = failure_of(cx, &h, "Install the Git hook").unwrap();
    assert!(message.contains("was not written by devflow"), "{message}");
    assert!(
        !message.contains("--from-git-hook") && !message.contains("devflow setup"),
        "{message}"
    );
    let offered = cx.read_entity(&h.state, |s, _| {
        s.activity.as_ref().unwrap().remedies.clone()
    });
    assert_eq!(
        offered,
        [
            Remedy::ChainGitHook { root: root.clone() },
            Remedy::OpenFile { path: hook.clone() }
        ]
    );
    assert!(offered[0].destructive(), "it edits a file somebody wrote");
    assert_eq!(std::fs::read_to_string(&hook).unwrap(), original);

    // Adding the line is confirmed first, and says which file it edits.
    let text = super::remedies::chain_lines(Some(&hook)).join(" ");
    assert!(text.contains("post-checkout"), "{text}");
    assert!(
        !text.contains("--from-git-hook") && !text.contains("devflow setup"),
        "{text}"
    );
    run(cx, &offered[0]);
    wait(cx, "the confirmation", |cx| exists(cx, &h, "confirm"));
    std::thread::sleep(Duration::from_millis(350));
    cancel_dialog(cx, &h);
    assert_eq!(std::fs::read_to_string(&hook).unwrap(), original);
    run(cx, &offered[0]);
    wait(cx, "the confirmation", |cx| exists(cx, &h, "confirm"));
    std::thread::sleep(Duration::from_millis(350));
    click(cx, &h, "confirm");
    settle(cx, &h, |s| {
        s.busy().is_none()
            && s.activity
                .as_ref()
                .is_some_and(|a| a.title.starts_with("Add worktree setup") && a.outcome.is_some())
    });
    let chained = std::fs::read_to_string(&hook).unwrap();
    assert!(chained.starts_with(original), "{chained}");

    // A project that is not the open one changes nothing.
    run(
        cx,
        &Remedy::ChainGitHook {
            root: PathBuf::from("/somewhere/else"),
        },
    );
    assert_eq!(std::fs::read_to_string(&hook).unwrap(), chained);
}

#[gpui_kit::test]
fn a_workspace_that_waits_for_approval_leads_to_where_it_is_given(cx: &mut TestAppContext) {
    // The reason is in words, and the matching button is next to it.
    let h = open(cx, install_loaded);
    update(cx, &h, |s, cx| {
        let model = s.selection.current_mut().unwrap();
        let receipt = model
            .workspaces
            .iter_mut()
            .find(|w| w.workspace.name == "feature-x")
            .and_then(|w| w.receipt.as_mut())
            .unwrap();
        receipt.skip_required(
            devflow_core::receipt::Phase::Hooks,
            "setup hooks are not trusted; review and approve them, then run setup again".into(),
        );
        model.selected_workspace = Some("feature-x".into());
        cx.notify();
    });
    frame(cx, &h, |window| {
        assert!(window.find("problem-hooks").visible());
        // Either the review (when core finds the scope unapproved) or setup.
        assert!(
            window.try_find("problem-review-hooks").is_some()
                || window.try_find("problem-fix-hooks").is_some()
        );
    });
}

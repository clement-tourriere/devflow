//! What the Daemons tab and the workspace detail share about processes: the
//! gates (what core allows), the state pills, the start/stop/restart
//! buttons with their approval flow, the notes under them and the banner of
//! a refusal.
//!
//! What is shown and what the buttons may do both come from core
//! (`devflow_core::processes`): its state of the workspace's namespace, and
//! its gates for starting and stopping (the workspace is ready, its own
//! pitchfork files are the approved ones, no daemon starts on its own). The
//! views never decide; a disabled button carries core's reason.
use super::{dialogs, doctor, overview::ProcessReading, remedies, ui::Tone, widgets::banner};
use crate::{
    model::ProcessTarget,
    ops::TrustChoice,
    redaction::{RefusalView, shown},
    state::{AppState, TrustPurpose},
};
use devflow_core::{
    pitchfork::DaemonStatus,
    processes::{DaemonState, ProcessStatus, Refusal},
};
use gpui_kit::{
    assets::IconName as Lucide,
    component::{
        Disableable as _, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
    },
    prelude::*,
    *,
};

/// A daemon's name as a view draws it (masked); `DaemonState::name` is the
/// identity a start asks core for.
pub(crate) fn daemon_label(daemon: &DaemonState) -> String {
    shown(&daemon.name)
}

/// The tone of a daemon's state pill: running green, failed red, the rest gray.
pub(super) fn state_tone(daemon: &DaemonState) -> Tone {
    if daemon.is_active() {
        Tone::Success
    } else if daemon.failed() {
        Tone::Danger
    } else {
        Tone::Neutral
    }
}

/// What a daemon's state means, for the pill's tooltip. The state is read by
/// core's one classifier ([`DaemonState::status`]), so a decorated state such
/// as `running (ready)` or `failed: exit 3` means what it starts with, exactly
/// as it does for the counts and the tones.
pub(super) fn state_help(daemon: &DaemonState) -> String {
    if daemon.is_not_started() {
        return "Declared in pitchfork.toml; pitchfork has not started it for this workspace."
            .to_string();
    }
    match daemon.status() {
        DaemonStatus::Running => "pitchfork reports this daemon running.".to_string(),
        DaemonStatus::Waiting => {
            "pitchfork started it and is waiting for its readiness check.".to_string()
        }
        DaemonStatus::Stopping => "pitchfork is stopping it.".to_string(),
        DaemonStatus::Stopped => "pitchfork stopped it.".to_string(),
        DaemonStatus::Errored | DaemonStatus::Failed => match &daemon.error {
            Some(error) => format!("pitchfork reports it failed: {error}"),
            None => "pitchfork reports it failed.".to_string(),
        },
        DaemonStatus::Available | DaemonStatus::Unknown => {
            format!("pitchfork reports: {}.", daemon.state.to_ascii_lowercase())
        }
    }
}

/// A daemon's state as it is drawn, with its tone and tooltip. When core says
/// the workspace's state was not observed (`unread` is the reason), a daemon
/// pitchfork did not list carries a fallback "not started" that nobody
/// observed: it is drawn as `unknown`, with the reason. A daemon pitchfork did
/// list (it has an id) keeps the state pitchfork gave it.
pub(crate) fn state_view(daemon: &DaemonState, unread: Option<&str>) -> (String, Tone, String) {
    match unread.filter(|_| daemon.id.is_none()) {
        Some(reason) => (
            "unknown".to_string(),
            Tone::Warning,
            format!(
                "pitchfork's state of this daemon could not be read, so none is shown: {reason}"
            ),
        ),
        None => (daemon.state.clone(), state_tone(daemon), state_help(daemon)),
    }
}

/// Why the workspace's daemon states are not observations, if they are not:
/// core's `ProcessStatus::observation`, through the shared [`ProcessReading`].
pub(crate) fn unread_reason(status: &ProcessStatus) -> Option<String> {
    match ProcessReading::of_status(status) {
        ProcessReading::Unknown { reason } => Some(reason),
        _ => None,
    }
}

/// What the toolbar may do now. Every flag comes from core's gates
/// ([`ProcessStatus::can_start`], [`ProcessStatus::can_stop`]) and whether a job
/// is running; the view decides nothing itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Actions {
    /// Start is enabled (directly, or through a review when `review`).
    pub start: bool,
    /// The configuration is not approved: Start opens the review of the
    /// workspace's files first.
    pub review: bool,
    pub stop: bool,
    pub restart: bool,
}

/// Is what runs in the workspace unknown (core's observation is incomplete)?
/// Then "nothing runs" is not an answer, so Stop stays available: core
/// refuses it as the forceable `state_unknown` and the banner offers to go on.
pub(crate) fn state_unknown(status: &ProcessStatus) -> bool {
    !status.observation.is_complete()
}

pub(crate) fn actions(status: &ProcessStatus, busy: bool) -> Actions {
    let active = status.daemons.iter().any(DaemonState::is_active);
    let review = matches!(status.can_start.refusal, Some(Refusal::NotApproved { .. }));
    Actions {
        start: !busy && (status.can_start.allowed || review),
        review,
        stop: !busy && status.can_stop.allowed && (active || state_unknown(status)),
        restart: !busy && active && status.can_stop.allowed && status.can_start.allowed,
    }
}

/// A row's Start is enabled when core says that daemon can be started by name.
pub(crate) fn row_can_start(daemon: &DaemonState, busy: bool) -> bool {
    !busy && daemon.startable
}

/// Start every daemon of the workspace `target` names: directly, or after the
/// person reviewed what its pitchfork files run when they are not approved.
/// The answer is for exactly what is shown, and core checks it again before
/// it starts anything. The review and the start both act on `target`, captured
/// when the button was drawn; the selection is not read again, so choosing
/// another workspace while the dialog is open changes nothing about what the
/// confirmation starts.
pub(crate) fn start(
    state: &Entity<AppState>,
    target: ProcessTarget,
    review: bool,
    window: &mut Window,
    cx: &mut App,
) {
    if review {
        let run_state = state.clone();
        let path = target.path.clone();
        dialogs::with_trust(
            state,
            &path,
            TrustPurpose::Processes,
            window,
            cx,
            move |choice, window, cx| {
                let target = target.clone();
                run_state.update(cx, |s, cx| {
                    s.start_processes(target, Vec::new(), choice, window, cx)
                })
            },
        );
    } else {
        state.update(cx, |s, cx| {
            s.start_processes(target, Vec::new(), TrustChoice::NotNeeded, window, cx)
        });
    }
}

/// Start by name: one daemon of the workspace (and what it depends on).
pub(crate) fn start_one(
    state: &Entity<AppState>,
    target: ProcessTarget,
    name: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let only = vec![name.to_string()];
    state.update(cx, |s, cx| {
        s.start_processes(target, only, TrustChoice::NotNeeded, window, cx)
    });
}

pub(crate) fn stop(
    state: &Entity<AppState>,
    target: ProcessTarget,
    window: &mut Window,
    cx: &mut App,
) {
    state.update(cx, |s, cx| s.stop_processes(target, false, window, cx));
}

/// Go on with a stop core refused because what runs is unknown
/// (`state_unknown`, the one refusal a forced stop answers). It says what core
/// said and what going on means, and acts on `target` only after the answer.
pub(crate) fn stop_anyway(
    state: &Entity<AppState>,
    target: ProcessTarget,
    refusal: &RefusalView,
    window: &mut Window,
    cx: &mut App,
) {
    let mut lines = vec![refusal.text.clone()];
    lines.extend(refusal.hint.clone());
    lines.push(
        "Stop anyway asks pitchfork to stop every daemon this workspace declares, whether or not it runs. If pitchfork cannot be asked, nothing is stopped and the result says the daemons may still be running."
            .to_string(),
    );
    let state = state.clone();
    dialogs::open_confirm(
        window,
        cx,
        "Stop without knowing what runs",
        lines,
        "Stop anyway",
        move |window, cx| {
            let target = target.clone();
            state.update(cx, |s, cx| s.stop_processes(target, true, window, cx));
        },
    );
}

pub(crate) fn restart(
    state: &Entity<AppState>,
    target: ProcessTarget,
    window: &mut Window,
    cx: &mut App,
) {
    state.update(cx, |s, cx| {
        s.restart_processes(target, TrustChoice::NotNeeded, window, cx)
    });
}

/// The Start, Stop and Restart buttons of a workspace's processes. `prefix`
/// keeps their element ids apart where the tab and the detail both draw them
/// (`daemons-start`, `detail-daemons-start`). Each acts on `target`, the
/// workspace `status` describes.
pub(crate) fn buttons(
    prefix: &'static str,
    state: &Entity<AppState>,
    status: &ProcessStatus,
    target: &ProcessTarget,
    busy: bool,
) -> [Button; 3] {
    let Actions {
        start: can_start,
        review,
        stop: can_stop,
        restart: can_restart,
    } = actions(status, busy);
    let id = |name: &str| SharedString::from(format!("{prefix}{name}"));
    let (start_state, stop_state, restart_state) = (state.clone(), state.clone(), state.clone());
    let (start_target, stop_target, restart_target) =
        (target.clone(), target.clone(), target.clone());
    [
        Button::new(id("start"))
            .icon(IconName::Play)
            .label(if review {
                "Review and start…"
            } else {
                "Start"
            })
            .disabled(!can_start)
            .on_click(move |_, window, cx| {
                start(&start_state, start_target.clone(), review, window, cx)
            }),
        Button::new(id("stop"))
            .icon(IconName::Square)
            .label("Stop")
            .disabled(!can_stop)
            .on_click(move |_, window, cx| stop(&stop_state, stop_target.clone(), window, cx)),
        Button::new(id("restart"))
            .icon(IconName::Redo)
            .label("Restart")
            .disabled(!can_restart)
            .on_click(move |_, window, cx| {
                restart(&restart_state, restart_target.clone(), window, cx)
            }),
    ]
}

/// The lines under the toolbar: why a button is off, and what autostart does.
/// Each has a stable id.
pub(crate) fn notes(status: &ProcessStatus) -> Vec<(&'static str, String)> {
    let mut notes = Vec::new();
    if status.autostart
        && let Some(blocked) = &status.autostart_blocked
    {
        notes.push(("autostart-note", blocked.clone()));
    }
    if status.configured
        && let Some(reason) = &status.can_start.reason
    {
        notes.push(("start-note", format!("Cannot start: {reason}")));
    }
    if (status.daemons.iter().any(DaemonState::is_active) || state_unknown(status))
        && let Some(reason) = &status.can_stop.reason
    {
        notes.push(("stop-note", format!("Cannot stop: {reason}")));
    }
    // The reason is drawn once: as the error banner when it is also the status
    // error, otherwise here.
    if let Some(why) = status.observation.summary()
        && status.error.as_deref() != Some(why.as_str())
    {
        notes.push((
            "observation-note",
            format!(
                "What runs could not be established, so no daemon is shown as stopped: {why}. Stop is still offered; devflow asks before it goes on without knowing."
            ),
        ));
    }
    if status.supervisor_up == Some(false) {
        notes.push((
            "supervisor-note",
            "The pitchfork supervisor is not running; it starts with the first daemon.".to_string(),
        ));
    }
    notes
}

/// A start that core refused after the gates allowed it (the mise delivery
/// check runs when a start is asked for), kept on the workspace until the
/// next start works or the person dismisses it. It shows core's sentence and
/// hint whatever the kind; the one kind with an action of its own is the
/// mise delivery, whose explanation is `devflow doctor`.
pub(crate) fn refusal_banner(
    id: &'static str,
    refusal: &RefusalView,
    target: &ProcessTarget,
    state: &Entity<AppState>,
    cx: &App,
) -> AnyElement {
    let mut text = refusal.text.clone();
    if let Some(hint) = &refusal.hint {
        text.push_str(&format!(" To fix it: {hint}."));
    }
    let doctor_state = state.clone();
    let dismiss_state = state.clone();
    let path = target.path.clone();
    let dismissed = target.path.clone();
    let action = if refusal.is_mise_delivery() {
        Some(
            Button::new(SharedString::from(format!("{id}-doctor")))
                .icon(Lucide::ListChecks)
                .label("Run doctor")
                .tooltip("Check what mise hands this workspace")
                .on_click(move |_, window, cx| {
                    doctor::open(&doctor_state, path.clone(), window, cx)
                }),
        )
    } else if refusal.is_state_unknown() {
        let (stop_state, stop_target, shown) = (state.clone(), target.clone(), refusal.clone());
        Some(
            Button::new(SharedString::from(format!("{id}-stop-anyway")))
                .icon(IconName::Square)
                .label("Stop anyway…")
                .tooltip("Core could not tell what runs; go on after confirming")
                .on_click(move |_, window, cx| {
                    stop_anyway(&stop_state, stop_target.clone(), &shown, window, cx)
                }),
        )
    } else {
        // Install pitchfork, set the workspace up, approve what it runs: the
        // button core's refusal implies, if it implies one.
        refusal.remedies(&target.path).first().map(|remedy| {
            remedies::button_with_id(state, format!("{id}-fix").into(), remedy, false)
        })
    };
    let dismiss = Button::new(SharedString::from(format!("{id}-dismiss")))
        .ghost()
        .small()
        .icon(IconName::Close)
        .tooltip("Dismiss")
        .on_click(move |_, _, cx| {
            let path = dismissed.clone();
            dismiss_state.update(cx, |s, cx| s.dismiss_refusal(&path, cx));
        });
    banner(Tone::Warning, Lucide::TriangleAlert, text, action, cx)
        .child(dismiss)
        .id(id)
        .test_support()
        .into_any_element()
}

//! The output of the running (or last) operation of the selected project:
//! its phases, a result tag, the error if it failed and the log. The
//! Workspaces tab and the Data tab both show it, so a snapshot or a reset
//! has its progress and its errors where it was started.
use super::ui;
use crate::{
    activity::{Activity, state_label},
    jobs::Done,
    model::Section,
    state::AppState,
};
use devflow_core::receipt::{Phase, PhaseState};
use gpui_kit::{
    component::{
        ActiveTheme as _, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Editor, EditorState},
        spinner::Spinner,
        v_flex,
    },
    prelude::*,
    *,
};

/// Owned copy of the activity for rendering (no borrow of the state held).
struct ActivityView {
    title: String,
    phases: Vec<(Phase, PhaseState, Option<String>)>,
    outcome: Option<Section<Done>>,
}

impl ActivityView {
    fn of(activity: &Activity) -> Self {
        Self {
            title: activity.title.clone(),
            phases: activity.phases.clone(),
            outcome: activity.outcome.clone(),
        }
    }

    fn running(&self) -> bool {
        self.outcome.is_none()
    }
}

pub struct ActivityPanel {
    state: Entity<AppState>,
    output: Entity<EditorState>,
    shown_revision: Option<u64>,
    _subscription: Subscription,
}

impl ActivityPanel {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let output = cx.new(|cx| {
            EditorState::new(window, cx)
                .line_number(false)
                .soft_wrap(true)
        });
        output.update(cx, |editor, cx| editor.set_readonly(true, cx));
        let subscription = cx.observe_in(&state, window, |this, _, window, cx| {
            this.sync(window, cx);
            cx.notify();
        });
        let mut this = Self {
            state,
            output,
            shown_revision: None,
            _subscription: subscription,
        };
        this.sync(window, cx);
        this
    }

    /// Push the log into the output editor when it changed.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (revision, text) = {
            let state = self.state.read(cx);
            let Some(model) = state.selection.current() else {
                return;
            };
            let activity = state
                .activity
                .as_ref()
                .filter(|a| a.key.project == model.key.project);
            let revision = activity.map(|a| a.log.revision());
            // The output can be megabytes: build it only when it changed.
            (
                revision,
                activity
                    .filter(|_| revision != self.shown_revision)
                    .map(|a| a.log.text()),
            )
        };
        if revision != self.shown_revision {
            self.shown_revision = revision;
            let text = text.unwrap_or_default();
            self.output.update(cx, |editor, cx| {
                editor.set_value(text.clone(), window, cx);
                // Follow the output: the cursor at the end scrolls it into view.
                editor.set_selected_range(text.len()..text.len(), cx);
            });
        }
    }
}

impl Render for ActivityPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let activity = {
            let state = self.state.read(cx);
            let project = state.selection.current().map(|m| m.key.project);
            state
                .activity
                .as_ref()
                .filter(|a| Some(a.key.project) == project)
                .map(ActivityView::of)
        };
        let Some(activity) = activity else {
            return div().into_any_element();
        };
        let running = activity.running();
        let phases = activity
            .phases
            .iter()
            .filter(|(_, state, _)| *state != PhaseState::Pending);
        let outcome = match &activity.outcome {
            Some(Ok(done)) => Some(ui::pill(
                "activity-outcome",
                if done.needs_attention {
                    "needs attention"
                } else {
                    "finished"
                },
                if done.needs_attention {
                    ui::Tone::Warning
                } else {
                    ui::Tone::Success
                },
                if done.needs_attention {
                    "It ran, but something needs a look: see the output."
                } else {
                    "It ran to the end."
                },
            )),
            Some(Err(_)) => Some(ui::pill(
                "activity-outcome",
                "failed",
                ui::Tone::Danger,
                "It stopped with an error: see the message below.",
            )),
            None => None,
        };
        v_flex()
            .id("activity-panel")
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(ui::heading(activity.title.clone()))
                    .when(running, |row| row.child(Spinner::new().small()))
                    .children(phases.map(|(phase, state, detail)| {
                        let tone = match state {
                            PhaseState::Done => ui::Tone::Success,
                            PhaseState::Failed => ui::Tone::Danger,
                            PhaseState::Skipped => ui::Tone::Warning,
                            _ => ui::Tone::Info,
                        };
                        let text = match detail {
                            Some(d) if *state == PhaseState::Failed => {
                                format!("{} {}: {d}", phase.as_str(), state_label(*state))
                            }
                            _ => format!("{} {}", phase.as_str(), state_label(*state)),
                        };
                        ui::pill(
                            SharedString::from(format!("activity-phase-{}", phase.as_str())),
                            text,
                            tone,
                            "The state of this setup phase.",
                        )
                    }))
                    .children(outcome)
                    .child(div().flex_1())
                    .when(!running, |row| {
                        row.child(
                            Button::new("dismiss-output")
                                .icon(IconName::Close)
                                .ghost()
                                .xsmall()
                                .tooltip("Dismiss")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.state.update(cx, |s, cx| {
                                        s.activity = None;
                                        cx.notify();
                                    })
                                })),
                        )
                    }),
            )
            .when_some(
                activity.outcome.as_ref().and_then(|o| o.as_ref().err()),
                |col, error| col.child(ui::error_banner(error.clone(), cx)),
            )
            .child(
                div()
                    .h(px(120.))
                    .child(Editor::new(&self.output).bordered(true).h_full()),
            )
            .test_support()
            .into_any_element()
    }
}

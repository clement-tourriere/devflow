//! The project overview at the top of the Workspaces tab: how many
//! workspaces and in what state, the data services and their branches, the
//! processes running, what copy-on-write saved (an upper bound, per D8: never
//! "zero disk" and never a claim the file system did not make) and the last
//! thing that happened. One primary action: New workspace.
use super::{
    reading::Reading,
    ui::{self, Tone},
    widgets::{card, chip},
    workspace_detail::{MeasureKey, PathSize, measure_paths},
};
use crate::{
    actions::NewWorkspace,
    activity::Activity,
    model::{ProjectModel, WorkspaceState, format_bytes, format_when},
    redaction::shown,
    state::AppState,
};
use devflow_core::{db::RecordStatus, processes::ProcessStatus, receipt::PayloadOutcome};
use gpui_kit::{
    component::{
        Disableable as _, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        spinner::Spinner,
        v_flex,
    },
    prelude::*,
    *,
};

/// What copy-on-write saved, as far as it can be said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Saved {
    /// Cloned paths are being measured.
    Measuring,
    /// Nothing was cloned (or nothing that can be counted).
    Nothing,
    /// At most this many bytes were not copied: what plain copies of the
    /// cloned payload paths and of the databases cloned from another branch
    /// would have taken. Whatever a workspace changes is written anew.
    UpTo(i64),
    /// Some files could not be read: this much was counted, and more may be.
    AtLeast(i64),
}

impl Saved {
    pub fn text(&self) -> String {
        match self {
            Saved::Measuring => "measuring…".to_string(),
            Saved::Nothing => "nothing cloned yet".to_string(),
            Saved::UpTo(bytes) => format!("up to {}", format_bytes(*bytes)),
            Saved::AtLeast(bytes) => format!("{} counted", format_bytes(*bytes)),
        }
    }

    pub fn help(&self) -> &'static str {
        match self {
            Saved::AtLeast(_) => {
                "Some files could not be read, so this is what was counted, not a bound."
            }
            _ => {
                "What plain copies would have taken: the payload paths cloned by copy-on-write (allocated blocks, shared blocks counted in full) and PostgreSQL branches cloned from another branch (logical size). Whatever a workspace changes is written anew, so the real saving is less. Container services are not counted."
            }
        }
    }
}

/// One data service and its branches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceLine {
    pub key: String,
    pub engine: String,
    /// `None` when they could not be counted: never zero.
    pub branches: Option<usize>,
}

/// What is known of the project's processes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessReading {
    /// Pitchfork was not asked, is not installed, or nothing uses it.
    NotAsked,
    /// Every workspace's listing was read.
    Known,
    /// Some workspaces' listings could not be read: the counts are what the
    /// others show, so at least that many run.
    Partial {
        unread: usize,
        total: usize,
        reason: String,
    },
    /// No listing could be read.
    Unknown { reason: String },
}

impl ProcessReading {
    /// How completely one workspace's daemons were read, as core judged it
    /// (`ProcessStatus::observation`): a listing that failed, entries that
    /// could not be read, or a supervisor that is down after devflow started
    /// daemons leave the declared daemons with a fallback "not started" that
    /// nobody observed.
    pub fn of_status(status: &ProcessStatus) -> Self {
        match status.observation.summary() {
            Some(reason) => Self::Unknown { reason },
            None => Self::Known,
        }
    }

    /// The reading the shared wording works from; `None` when nothing was
    /// asked.
    pub fn reading(&self) -> Option<Reading> {
        match self {
            Self::NotAsked => None,
            Self::Known => Some(Reading::Known),
            Self::Partial {
                unread,
                total,
                reason,
            } => Some(Reading::Partial {
                unread: *unread,
                total: *total,
                reason: reason.clone(),
            }),
            Self::Unknown { reason } => Some(Reading::Unknown {
                reason: reason.clone(),
            }),
        }
    }
}

/// The numbers the overview shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Numbers {
    pub workspaces: usize,
    /// Each state that has workspaces, in a fixed order.
    pub by_state: Vec<(WorkspaceState, usize)>,
    pub services: Vec<ServiceLine>,
    /// Daemons pitchfork reports running, over the workspaces it could be
    /// read for.
    pub daemons_running: usize,
    /// Workspaces with at least one running.
    pub workspaces_running: usize,
    /// Whether those counts are complete.
    pub processes: ProcessReading,
    pub saved: Saved,
    pub last_activity: Option<String>,
}

const STATE_ORDER: [WorkspaceState; 6] = [
    WorkspaceState::Ready,
    WorkspaceState::NeedsAttention,
    WorkspaceState::InProgress,
    WorkspaceState::Busy,
    WorkspaceState::NotSetUp,
    WorkspaceState::Missing,
];

/// The last thing that happened: the app's own operation if there was one
/// (with its outcome), else the newest setup the receipts remember.
fn last_activity(model: &ProjectModel, activity: Option<&Activity>) -> Option<String> {
    if let Some(activity) = activity.filter(|a| a.key.project == model.key.project) {
        let outcome = match &activity.outcome {
            None => "running",
            Some(Ok(done)) if done.needs_attention => "needs attention",
            Some(Ok(_)) => "finished",
            Some(Err(_)) => "failed",
        };
        let when = activity
            .finished_at
            .map(|at| format!(" at {}", at.format("%H:%M")))
            .unwrap_or_default();
        return Some(format!("{}: {outcome}{when}", activity.title));
    }
    model
        .workspaces
        .iter()
        .filter_map(|w| Some((w.receipt.as_ref()?.created_at, &w.workspace.name)))
        .max_by_key(|(at, _)| *at)
        .map(|(at, name)| format!("{} was set up {}", shown(name), format_when(at)))
}

/// How much copy-on-write kept from being copied. `measured` holds the
/// figures of the workspaces whose payload was measured.
fn saved(model: &ProjectModel, measured: &[(MeasureKey, Vec<PathSize>)]) -> Saved {
    let mut total: i64 = 0;
    let mut counted = false;
    let mut incomplete = false;
    let mut pending = false;
    for status in &model.workspaces {
        let Some(key) = MeasureKey::of(status) else {
            continue;
        };
        let cloned = key
            .payload
            .iter()
            .any(|(_, o)| matches!(o, PayloadOutcome::Cloned | PayloadOutcome::Mixed));
        if !cloned {
            continue;
        }
        let Some((_, sizes)) = measured.iter().find(|(k, _)| *k == key) else {
            pending = true;
            continue;
        };
        for size in sizes
            .iter()
            .filter(|s| matches!(s.outcome, PayloadOutcome::Cloned | PayloadOutcome::Mixed))
        {
            match &size.measure {
                Ok(m) => {
                    let bytes = m.allocated_bytes.unwrap_or(m.apparent_bytes);
                    total = total.saturating_add(bytes.min(i64::MAX as u64) as i64);
                    counted = true;
                    incomplete |= m.partial;
                }
                // Not readable at all: it is not counted, and that is said.
                Err(_) => incomplete = true,
            }
        }
    }
    // Databases cloned from another branch by copy-on-write.
    for branch in &model.database.branches {
        if branch.status == RecordStatus::Ready
            && branch.source_kind != "empty"
            && branch.cow == devflow_core::config::CowCapability::Verified
            && let Some(size) = branch.size_bytes
        {
            total = total.saturating_add(size.max(0));
            counted = true;
        }
    }
    if pending {
        Saved::Measuring
    } else if !counted && !incomplete {
        Saved::Nothing
    } else if incomplete {
        Saved::AtLeast(total)
    } else {
        Saved::UpTo(total)
    }
}

/// What pitchfork's answers amount to, and the statuses that can be counted.
/// A listing that failed shows no daemons, which is not "none running": it
/// stays unknown instead of counting as zero.
fn process_reading(model: &ProjectModel) -> (ProcessReading, Vec<&ProcessStatus>) {
    let pitchfork = &model.pitchfork;
    if !pitchfork.loaded {
        return (ProcessReading::NotAsked, Vec::new());
    }
    if let Some(reason) = &pitchfork.error {
        return (
            ProcessReading::Unknown {
                reason: reason.clone(),
            },
            Vec::new(),
        );
    }
    if pitchfork.all.is_empty() {
        return (ProcessReading::NotAsked, Vec::new());
    }
    let (unread, readable): (Vec<_>, Vec<_>) = pitchfork
        .all
        .iter()
        .partition(|s| !s.observation.is_complete());
    let reason = |s: &ProcessStatus| s.observation.summary().unwrap_or_default();
    let reading = match (unread.first(), readable.is_empty()) {
        (None, _) => ProcessReading::Known,
        (Some(first), true) => ProcessReading::Unknown {
            reason: reason(first),
        },
        (Some(first), false) => ProcessReading::Partial {
            unread: unread.len(),
            total: pitchfork.all.len(),
            reason: reason(first),
        },
    };
    (reading, readable)
}

/// The Processes tile: its value and the line under it.
pub fn processes_tile(numbers: &Numbers) -> (String, String) {
    let running = numbers.daemons_running;
    let Some(reading) = numbers.processes.reading() else {
        return (
            "—".to_string(),
            "none configured, or pitchfork is not installed".to_string(),
        );
    };
    let value = reading.count_text(running, "running");
    let sub = match reading.note("pitchfork", "workspaces") {
        Some(note) => note,
        None if running == 0 => "no daemon is running".to_string(),
        None => format!(
            "in {} workspace{}",
            numbers.workspaces_running,
            if numbers.workspaces_running == 1 {
                ""
            } else {
                "s"
            }
        ),
    };
    (value, sub)
}

/// The overview of the selected project. Pure: what the tiles say for a
/// model, the measurements made so far and the last operation.
pub(super) fn numbers(
    model: &ProjectModel,
    measured: &[(MeasureKey, Vec<PathSize>)],
    activity: Option<&Activity>,
) -> Numbers {
    let by_state = STATE_ORDER
        .iter()
        .filter_map(|&state| {
            let n = model
                .workspaces
                .iter()
                .filter(|w| WorkspaceState::of(w) == state)
                .count();
            (n > 0).then_some((state, n))
        })
        .collect();
    let mut services = Vec::new();
    if model.database.configured {
        services.push(ServiceLine {
            key: model
                .project
                .as_ref()
                .and_then(|p| p.config.postgres().map(|(k, _)| k.to_string()))
                .unwrap_or_else(|| "db".to_string()),
            engine: "postgres".to_string(),
            // An unreachable server lists nothing, which is not "no branches".
            branches: (model.database.error.is_none()
                && model
                    .database
                    .status
                    .as_ref()
                    .is_some_and(|status| status.reachable))
            .then_some(model.database.branches.len()),
        });
    }
    services.extend(model.services.counts.iter().map(|c| ServiceLine {
        key: c.key.clone(),
        engine: c.engine.clone(),
        branches: c.known.then_some(c.branches),
    }));
    let (processes, readable) = process_reading(model);
    let daemons_running = readable
        .iter()
        .map(|s| s.daemons.iter().filter(|d| d.is_active()).count())
        .sum();
    let workspaces_running = readable
        .iter()
        .filter(|s| s.daemons.iter().any(|d| d.is_active()))
        .count();
    Numbers {
        workspaces: model.workspaces.len(),
        by_state,
        services,
        daemons_running,
        workspaces_running,
        processes,
        saved: saved(model, measured),
        last_activity: last_activity(model, activity),
    }
}

pub struct OverviewView {
    state: Entity<AppState>,
    /// The payload measurements made so far; each is dropped when the
    /// workspace it was made for changed.
    measured: Vec<(MeasureKey, Vec<PathSize>)>,
    measuring: Option<MeasureKey>,
    _subscription: Subscription,
}

impl OverviewView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe_in(&state, window, |this, _, window, cx| {
            this.sync(window, cx);
            cx.notify();
        });
        let mut this = Self {
            state,
            measured: Vec::new(),
            measuring: None,
            _subscription: subscription,
        };
        this.sync(window, cx);
        this
    }

    /// Measure the next cloned payload that has not been measured, one at a
    /// time, off the UI thread; forget the measurements of what is gone.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wanted: Vec<MeasureKey> = {
            let state = self.state.read(cx);
            let Some(model) = state.selection.current() else {
                self.measured.clear();
                return;
            };
            model
                .workspaces
                .iter()
                .filter_map(MeasureKey::of)
                .filter(|key| {
                    key.payload
                        .iter()
                        .any(|(_, o)| matches!(o, PayloadOutcome::Cloned | PayloadOutcome::Mixed))
                })
                .collect()
        };
        self.measured.retain(|(key, _)| wanted.contains(key));
        if self.measuring.is_some() {
            return;
        }
        let Some(next) = wanted
            .into_iter()
            .find(|key| !self.measured.iter().any(|(k, _)| k == key))
        else {
            return;
        };
        self.measuring = Some(next.clone());
        let work = next.clone();
        cx.spawn_in(window, async move |this, cx| {
            let sizes = cx
                .background_spawn(async move { measure_paths(&work) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.measuring = None;
                this.measured.push((next, sizes));
                // Another one may be waiting.
                this.sync(window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn tile(
        &self,
        id: &'static str,
        title: &'static str,
        value: impl IntoElement,
        sub: impl IntoElement,
        cx: &App,
    ) -> Stateful<Div> {
        v_flex()
            .id(id)
            .flex_1()
            .min_w(px(150.))
            .gap_0p5()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui::muted(cx))
                    .child(title),
            )
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(value),
            )
            .child(sub)
    }
}

impl Render for OverviewView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (numbers, busy) = {
            let state = self.state.read(cx);
            let Some(model) = state.selection.current() else {
                return div().into_any_element();
            };
            (
                numbers(model, &self.measured, state.activity.as_ref()),
                state.busy().is_some(),
            )
        };
        let muted = ui::muted(cx);
        let states = h_flex()
            .gap_1()
            .flex_wrap()
            .children(numbers.by_state.iter().map(|(state, n)| {
                ui::pill(
                    SharedString::from(format!("overview-state-{}", state.label())),
                    format!("{n} {}", state.label()),
                    ui::workspace_tone(*state),
                    "Workspaces in this state.",
                )
            }));
        let services = v_flex()
            .gap_0p5()
            .children(numbers.services.iter().take(4).map(|line| {
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_xs()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child(line.key.clone()),
                    )
                    .child(chip(line.engine.clone(), Tone::Info, cx))
                    .child(div().text_color(muted).child(match line.branches {
                        Some(1) => "1 branch".to_string(),
                        Some(n) => format!("{n} branches"),
                        None => "branches unavailable".to_string(),
                    }))
            }))
            .when(numbers.services.is_empty(), |col| {
                col.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("No data service configured"),
                )
            });
        let (processes_value, processes_sub) = processes_tile(&numbers);
        let saved_value = numbers.saved.text();
        let saved_help = numbers.saved.help();
        let measuring = numbers.saved == Saved::Measuring;
        card(cx)
            .id("overview")
            .child(
                h_flex()
                    .px_4()
                    .py_3()
                    .gap_4()
                    .flex_wrap()
                    .items_start()
                    .child(
                        self.tile(
                            "overview-workspaces",
                            "Workspaces",
                            numbers.workspaces.to_string(),
                            states,
                            cx,
                        )
                        .test_support(),
                    )
                    .child(
                        self.tile("overview-services", "Data", "", services, cx)
                            .test_support(),
                    )
                    .child(
                        self.tile(
                            "overview-processes",
                            "Processes",
                            processes_value,
                            div().text_xs().text_color(muted).child(processes_sub),
                            cx,
                        )
                        .test_support(),
                    )
                    .child(
                        self.tile(
                            "overview-saved",
                            "Saved by copy-on-write",
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(saved_value)
                                .when(measuring, |row| row.child(Spinner::new().small())),
                            div()
                                .id("overview-saved-help")
                                .text_xs()
                                .text_color(muted)
                                .child("payload and databases")
                                .tooltip(move |window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new(saved_help)
                                        .build(window, cx)
                                }),
                            cx,
                        )
                        .test_support(),
                    )
                    .child(
                        self.tile(
                            "overview-activity",
                            "Last activity",
                            "",
                            div().text_xs().text_color(muted).child(
                                numbers
                                    .last_activity
                                    .clone()
                                    .unwrap_or_else(|| "nothing yet".to_string()),
                            ),
                            cx,
                        )
                        .flex_1()
                        .test_support(),
                    )
                    .child(
                        v_flex().gap_1().items_end().child(
                            Button::new("new-workspace")
                                .primary()
                                .icon(IconName::Plus)
                                .label("New workspace")
                                .tooltip_with_action("New workspace", &NewWorkspace, None)
                                .small()
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    super::dialogs::open_new_workspace(&this.state, window, cx)
                                })),
                        ),
                    ),
            )
            .test_support()
            .into_any_element()
    }
}

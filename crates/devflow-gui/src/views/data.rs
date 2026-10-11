//! The Data tab: every data service the project configures. A card per
//! service on top (PostgreSQL first, then the container engines: Valkey, Redis
//! and whatever core adds), and below the one that is selected: PostgreSQL's
//! server, branches and snapshots as before, or a container service's
//! branches with their endpoints, snapshots and orphans.
use super::{
    activity_panel::ActivityPanel,
    database::DatabaseView,
    reading::Reading,
    service_dialogs,
    service_view::ServiceView,
    ui::{self, Tone},
    widgets::{card, chip},
};
use crate::{
    model::{ProjectModel, ServerBadge, Tab},
    redaction::shown,
    services::{BranchState, ServiceDetail, ServiceTarget},
    state::AppState,
};
use gpui_kit::{
    assets::IconName as Lucide,
    component::{
        ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex, v_flex,
    },
    prelude::*,
    *,
};

/// What one card of the strip says. Plain data, so what the tab shows for a
/// fixture project can be checked without drawing it.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceCard {
    pub key: String,
    pub engine: String,
    pub postgres: bool,
    /// `None` when they could not be counted (the catalog or the server could
    /// not be read): never zero.
    pub branches: Option<usize>,
    /// Branches whose container runs.
    pub running: Running,
    pub snapshots: Option<usize>,
    pub unfinished: usize,
    /// Branches no workspace owns.
    pub orphans: usize,
    /// A workspace's receipt could not be read, so `orphans` is not known.
    pub owners_unknown: bool,
    /// PostgreSQL: the server's state.
    pub server: Option<ServerBadge>,
    /// The copy-on-write result, as its pill words it, when there is one.
    pub cow: Option<String>,
}

/// The card's pill and the service page's header word a running count the
/// same way: exact only when every container was read.
pub fn running_word(running: usize, reading: &Reading) -> String {
    match reading {
        // Unavailable has always said so in these words.
        Reading::Unknown { .. } => "unavailable".to_string(),
        _ => reading.count_text(running, "running"),
    }
}

/// What the count rests on, for a tooltip.
pub fn running_help(reading: &Reading) -> String {
    match reading {
        Reading::Known => "Branches whose container runs now.".to_string(),
        Reading::Partial { reason, .. } => {
            format!("{reason}, so more may run.")
        }
        Reading::Unknown { reason } => reason.clone(),
    }
}

/// How many branches run, said as far as it is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Running {
    /// The service was not read yet (or has no containers to count).
    NotRead,
    Known(usize),
    /// Docker could not say what some containers are doing: at least this many.
    AtLeast(usize),
    /// Docker or the catalog could not be read: why.
    Unavailable(String),
}

impl Running {
    /// The count and how completely it was read; `None` before the service
    /// was read.
    pub fn counted(&self) -> Option<(usize, Reading)> {
        match self {
            Self::NotRead => None,
            Self::Known(n) => Some((*n, Reading::Known)),
            Self::AtLeast(n) => Some((
                *n,
                Reading::Partial {
                    unread: 0,
                    total: 0,
                    reason: "Docker could not say what some containers are doing".to_string(),
                },
            )),
            Self::Unavailable(why) => Some((
                0,
                Reading::Unknown {
                    reason: why.clone(),
                },
            )),
        }
    }

    pub(super) fn of(detail: Option<&ServiceDetail>) -> Self {
        let Some(detail) = detail else {
            return Self::NotRead;
        };
        if detail.docker.is_none() || detail.error.is_some() {
            return Self::Unavailable(
                detail
                    .error
                    .clone()
                    .unwrap_or_else(|| "Docker could not be asked.".to_string()),
            );
        }
        let running = detail.branches.iter().filter(|b| b.state.running()).count();
        if detail
            .branches
            .iter()
            .any(|b| matches!(b.state, BranchState::Unknown))
        {
            Self::AtLeast(running)
        } else {
            Self::Known(running)
        }
    }
}

/// The cards of a project: PostgreSQL if configured, then the container
/// services in the configuration's order.
pub fn cards(model: &ProjectModel) -> Vec<ServiceCard> {
    let mut cards = Vec::new();
    let postgres_read = model.database.error.is_none()
        && model
            .database
            .status
            .as_ref()
            .is_some_and(|status| status.reachable);
    if model.database.configured {
        let key = model
            .project
            .as_ref()
            .and_then(|p| p.config.postgres().map(|(k, _)| k.to_string()))
            .unwrap_or_else(|| "db".to_string());
        cards.push(ServiceCard {
            key,
            engine: "postgres".to_string(),
            postgres: true,
            // An unreachable server lists nothing: that is not "none".
            branches: postgres_read.then_some(model.database.branches.len()),
            running: Running::NotRead,
            snapshots: postgres_read.then_some(model.database.snapshots.len()),
            unfinished: 0,
            orphans: 0,
            owners_unknown: false,
            server: Some(model.server_badge()),
            cow: model
                .database
                .status
                .as_ref()
                .map(|s| crate::model::cow_label(s.cow).to_string()),
        });
    }
    for count in &model.services.counts {
        let detail = model.services.detail(&count.key);
        cards.push(ServiceCard {
            key: count.key.clone(),
            engine: count.engine.clone(),
            postgres: false,
            branches: count.known.then_some(count.branches),
            running: Running::of(detail),
            snapshots: count.known.then_some(count.snapshots),
            unfinished: count.unfinished,
            orphans: model
                .services
                .orphans
                .branches
                .iter()
                .filter(|o| o.service == count.key)
                .count(),
            owners_unknown: !model.services.orphans.incomplete.is_empty(),
            server: None,
            cow: detail.map(|d| super::service_view::cow_text(&d.cow)),
        });
    }
    cards
}

/// "3 branches · 1 snapshot", or that they could not be counted.
pub fn counts_text(card: &ServiceCard) -> String {
    let branches = match card.branches {
        Some(1) => "1 branch".to_string(),
        Some(n) => format!("{n} branches"),
        None => "branches unavailable".to_string(),
    };
    let snapshots = match card.snapshots {
        Some(1) => " · 1 snapshot".to_string(),
        Some(n) => format!(" · {n} snapshots"),
        None if card.branches.is_none() => String::new(),
        None => " · snapshots unavailable".to_string(),
    };
    format!("{branches}{snapshots}")
}

pub struct DataView {
    state: Entity<AppState>,
    database: Entity<DatabaseView>,
    service: Entity<ServiceView>,
    activity: Entity<ActivityPanel>,
    _subscription: Subscription,
}

impl DataView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            database: cx.new(|cx| DatabaseView::new(state.clone(), window, cx)),
            service: cx.new(|cx| ServiceView::new(state.clone(), cx)),
            activity: cx.new(|cx| ActivityPanel::new(state.clone(), window, cx)),
            state,
            _subscription: subscription,
        }
    }

    fn card(
        &self,
        card: &ServiceCard,
        selected: bool,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = card.key.clone();
        let state = self.state.clone();
        let status = match (&card.server, card.running.counted()) {
            (Some(badge), _) => ui::server_tag(*badge).into_any_element(),
            (None, Some((running, reading))) => ui::pill(
                "service-card-running",
                running_word(running, &reading),
                reading.tone(running),
                running_help(&reading),
            )
            .into_any_element(),
            (None, None) => div().into_any_element(),
        };
        let _ = busy;
        let id = SharedString::from(format!("service-card-{}", card.key));
        let border = if selected {
            cx.theme().primary
        } else {
            cx.theme().border
        };
        v_flex()
            .id(id)
            .cursor_pointer()
            .min_w(px(220.))
            .p_3()
            .gap_1p5()
            .rounded_lg()
            .border_1()
            .border_color(border)
            .bg(if selected {
                cx.theme().primary.opacity(0.06)
            } else {
                cx.theme().background
            })
            .hover(|style| style.bg(cx.theme().muted.opacity(0.5)))
            .on_click(move |_, _, cx| {
                let key = key.clone();
                state.update(cx, |s, cx| s.select_data_service(key, cx))
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Icon::new(if card.postgres {
                            Lucide::Database
                        } else {
                            Lucide::Layers
                        })
                        .size_4()
                        .text_color(ui::muted(cx)),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(card.key.clone()),
                    )
                    .child(chip(card.engine.clone(), Tone::Info, cx)),
            )
            .child(
                h_flex().gap_2().items_center().child(status).child(
                    div()
                        .text_xs()
                        .text_color(ui::muted(cx))
                        .child(counts_text(card)),
                ),
            )
            .when(
                card.unfinished > 0 || card.orphans > 0 || card.owners_unknown,
                |col| {
                    col.child(
                        h_flex()
                            .gap_1()
                            .children((card.unfinished > 0).then(|| {
                                chip(format!("{} unfinished", card.unfinished), Tone::Warning, cx)
                            }))
                            .children((card.orphans > 0).then(|| {
                                chip(format!("{} orphan", card.orphans), Tone::Warning, cx)
                            }))
                            .children(
                                card.owners_unknown
                                    .then(|| chip("owners unknown", Tone::Warning, cx)),
                            ),
                    )
                },
            )
            .test_support()
            .into_any_element()
    }
}

impl Render for DataView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (cards, selected, orphan_services, busy, project) = {
            let state = self.state.read(cx);
            let Some(model) = state.selection.current() else {
                return div().into_any_element();
            };
            (
                cards(model),
                state.selected_data_service(),
                model.services.orphans.services.clone(),
                state.busy().is_some(),
                model.project_ref(),
            )
        };
        if cards.is_empty() && orphan_services.is_empty() {
            let state = self.state.clone();
            return ui::empty_state(
                Lucide::Database,
                "No data service is configured",
                "Add a postgres, valkey or redis service ([services.NAME], engine = \"valkey\") to devflow.toml to give every workspace its own copy-on-write data.",
                Some(
                    Button::new("open-config")
                        .primary()
                        .label("Open the config")
                        .on_click(move |_, _, cx| {
                            state.update(cx, |s, cx| s.go_to_tab(Tab::Config, cx))
                        }),
                ),
            )
            .into_any_element();
        }
        let postgres_selected = cards
            .iter()
            .find(|c| Some(&c.key) == selected.as_ref())
            .is_some_and(|c| c.postgres);
        let strip = h_flex()
            .id("service-cards")
            .px_4()
            .pt_4()
            .gap_3()
            .flex_wrap()
            .children(cards.iter().map(|c| {
                let is_selected = Some(&c.key) == selected.as_ref();
                self.card(c, is_selected, busy, cx)
            }));
        v_flex()
            .id("data-tab")
            .size_full()
            .child(strip)
            .children(orphan_services.into_iter().enumerate().map(|(ix, key)| {
                let state = self.state.clone();
                let forget_target = ServiceTarget {
                    project: project.clone(),
                    key: key.clone(),
                };
                div().px_4().pt_3().child(
                    card(cx)
                        .id(("service-orphan-service", ix))
                        .child(
                            h_flex()
                                .px_3()
                                .py_2()
                                .gap_3()
                                .items_center()
                                .child(
                                    Icon::new(IconName::TriangleAlert)
                                        .size_4()
                                        .text_color(cx.theme().warning),
                                )
                                .child(div().flex_1().text_sm().child(format!(
                                    "The configuration no longer has the service '{}', and devflow keeps its data until you say so.",
                                    shown(&key)
                                )))
                                .child(
                                    Button::new(("service-forget", ix))
                                        .small()
                                        .icon(IconName::Delete)
                                        .label("Forget…")
                                        .disabled(busy)
                                        .on_click(move |_, window, cx| {
                                            service_dialogs::open_forget(
                                                &state,
                                                forget_target.clone(),
                                                window,
                                                cx,
                                            )
                                        }),
                                ),
                        )
                        .test_support(),
                )
            }))
            .child(div().flex_1().min_h_0().child(if postgres_selected {
                self.database.clone().into_any_element()
            } else {
                self.service.clone().into_any_element()
            }))
            .child(self.activity.clone())
            .test_support()
            .into_any_element()
    }
}

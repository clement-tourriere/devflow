//! The selected project: header (name, path, badges, refresh), tabs, and the
//! tab content. Also the not-initialized / failed / loading states.
use super::{
    config::ConfigView, daemons::DaemonsView, database::DatabaseView, dialogs, logs::LogsView, ui,
    workspaces::WorkspacesView,
};
use crate::{
    model::{LoadPhase, ProjectModel, ServerBadge, Tab, shorten_home},
    state::AppState,
};
use devflow_core::{config::CowCapability, db::CowSource, trust::TrustState};
use gpui_kit::{
    component::{
        ActiveTheme as _, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        select::{Select, SelectEvent, SelectState},
        spinner::Spinner,
        tab::{Tab as TabItem, TabBar},
        v_flex,
    },
    prelude::*,
    *,
};
use std::path::PathBuf;

/// Owned copy of what the header shows, so no borrow of the state outlives it.
struct HeaderInfo {
    name: String,
    root: PathBuf,
    badge: ServerBadge,
    cow: Option<(CowCapability, Option<CowSource>)>,
    hooks_untrusted: bool,
    refreshing: bool,
}

impl HeaderInfo {
    fn of(model: &ProjectModel) -> Self {
        Self {
            name: model.entry.name.clone(),
            root: model.entry.root.clone(),
            badge: model.server_badge(),
            cow: model
                .database
                .status
                .as_ref()
                .map(|s| (s.cow, s.cow_source)),
            hooks_untrusted: matches!(model.trust, Some(TrustState::Untrusted { .. })),
            refreshing: model.refreshing,
        }
    }
}

pub struct ProjectView {
    state: Entity<AppState>,
    workspaces: Entity<WorkspacesView>,
    database: Entity<DatabaseView>,
    daemons: Entity<DaemonsView>,
    pub(super) config: Entity<ConfigView>,
    pub(super) logs: Entity<LogsView>,
    picker: Entity<SelectState<Vec<SharedString>>>,
    picker_items: Vec<String>,
    _subscriptions: Vec<Subscription>,
}

impl ProjectView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let picker = cx.new(|cx| SelectState::new(Vec::<SharedString>::new(), None, window, cx));
        let subscriptions = vec![
            cx.observe_in(&state, window, |this, _, window, cx| {
                this.sync_picker(window, cx);
                cx.notify();
            }),
            cx.subscribe(
                &picker,
                |this, _, event: &SelectEvent<Vec<SharedString>>, cx| {
                    let SelectEvent::Confirm(Some(name)) = event else {
                        return;
                    };
                    let name = name.to_string();
                    this.state.update(cx, |s, cx| s.select_workspace(name, cx));
                },
            ),
        ];
        Self {
            workspaces: cx.new(|cx| WorkspacesView::new(state.clone(), window, cx)),
            database: cx.new(|cx| DatabaseView::new(state.clone(), window, cx)),
            daemons: cx.new(|cx| DaemonsView::new(state.clone(), window, cx)),
            config: cx.new(|cx| ConfigView::new(state.clone(), window, cx)),
            logs: cx.new(|cx| LogsView::new(state.clone(), window, cx)),
            state,
            picker,
            picker_items: Vec::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Keep the workspace picker in step with the model without resetting an
    /// open dropdown on every notification.
    fn sync_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (names, selected) = match self.state.read(cx).selection.current() {
            Some(model) => (
                model
                    .workspaces
                    .iter()
                    .map(|w| w.workspace.name.clone())
                    .collect(),
                model.selected_workspace.clone(),
            ),
            None => (Vec::new(), None),
        };
        let items_changed = names != self.picker_items;
        let shown = self.picker.read(cx).selected_value().map(|v| v.to_string());
        if !items_changed && shown == selected {
            return;
        }
        if items_changed {
            let items: Vec<SharedString> = names.iter().cloned().map(Into::into).collect();
            self.picker
                .update(cx, |p, cx| p.set_items(items, window, cx));
            self.picker_items = names;
        }
        if let Some(name) = selected {
            self.picker.update(cx, |p, cx| {
                p.set_selected_value(&SharedString::from(name), window, cx)
            });
        }
    }

    fn header(
        &self,
        info: HeaderInfo,
        busy: Option<&str>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let refreshing = info.refreshing || busy.is_some();
        let mut badges = h_flex().gap_2().items_center();
        badges = badges.child(ui::server_tag(info.badge));
        if let Some((cow, source)) = info.cow {
            badges = badges.child(ui::cow_tag(cow, source));
        }
        if info.hooks_untrusted {
            badges = badges.child(ui::tag("hooks not trusted", ui::Tone::Warning));
        }
        h_flex()
            .px_4()
            .pt_4()
            .pb_2()
            .gap_3()
            .items_center()
            .justify_between()
            .child(
                v_flex()
                    .gap_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(info.name),
                            )
                            .child(badges),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(ui::muted(cx))
                            .truncate()
                            .child(shorten_home(&info.root)),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .when_some(busy.map(str::to_string), |row, title| {
                        row.child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .text_sm()
                                .text_color(ui::muted(cx))
                                .child(Spinner::new().small())
                                .child(title),
                        )
                    })
                    .child(
                        Button::new("refresh")
                            .icon(IconName::RefreshCw)
                            .label("Refresh")
                            .small()
                            .loading(refreshing && busy.is_none())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.update(cx, |s, cx| s.refresh(cx))
                            })),
                    ),
            )
    }

    fn tabs(&self, active: Tab, cx: &mut Context<Self>) -> impl IntoElement {
        let picker = active.needs_workspace().then(|| {
            h_flex()
                .gap_2()
                .items_center()
                .child(div().text_sm().text_color(ui::muted(cx)).child("Workspace"))
                .child(Select::new(&self.picker).small().w(px(220.)))
        });
        h_flex()
            .px_4()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                TabBar::new("project-tabs")
                    .underline()
                    .selected_index(active.index())
                    .on_click(cx.listener(|this, ix: &usize, _, cx| {
                        let tab = Tab::ALL[*ix];
                        this.state.update(cx, |s, cx| s.set_tab(tab, cx));
                    }))
                    .children(Tab::ALL.iter().map(|t| TabItem::new().label(t.title()))),
            )
            .children(picker)
    }
}

impl Render for ProjectView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (info, phase, tab, busy, entries_error) = {
            let state = self.state.read(cx);
            let info = state.selection.current().map(HeaderInfo::of);
            let phase = state.selection.current().map(|m| m.phase.clone());
            (
                info,
                phase,
                state.tab,
                state.busy().map(str::to_string),
                state.entries_error.clone(),
            )
        };
        let (Some(info), Some(phase)) = (info, phase) else {
            let message = entries_error.unwrap_or_else(|| {
                "Add a Git repository with \"Add project…\" in the sidebar.".into()
            });
            return ui::note("No project selected", message, cx).into_any_element();
        };
        let root = info.root.clone();
        match phase {
            LoadPhase::Loading => ui::loading("Loading project…", cx).into_any_element(),
            LoadPhase::NotInitialized => {
                let app = self.state.clone();
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(ui::heading("Not initialized"))
                    .child(
                        div()
                            .max_w(px(520.))
                            .text_sm()
                            .text_center()
                            .text_color(ui::muted(cx))
                            .child(format!(
                                "{} has no devflow.toml. Initialize it to create worktrees with copy-on-write data and setup hooks.",
                                shorten_home(&root)
                            )),
                    )
                    .child(Button::new("initialize").primary().label("Initialize…").on_click(
                        move |_, window, cx| dialogs::open_initialize(&app, root.clone(), window, cx),
                    ))
                    .into_any_element()
            }
            LoadPhase::Failed(message) => v_flex()
                .size_full()
                .p_8()
                .gap_3()
                .child(ui::heading("Cannot load this project"))
                .child(ui::error_banner(message, cx))
                .child(Button::new("retry").label("Retry").on_click(
                    cx.listener(|this, _, _, cx| this.state.update(cx, |s, cx| s.refresh(cx))),
                ))
                .into_any_element(),
            LoadPhase::Ready => {
                let header = self.header(info, busy.as_deref(), cx);
                let content = match tab {
                    Tab::Workspaces => self.workspaces.clone().into_any_element(),
                    Tab::Database => self.database.clone().into_any_element(),
                    Tab::Daemons => self.daemons.clone().into_any_element(),
                    Tab::Config => self.config.clone().into_any_element(),
                    Tab::Logs => self.logs.clone().into_any_element(),
                };
                v_flex()
                    .size_full()
                    .child(header)
                    .child(self.tabs(tab, cx))
                    .child(div().flex_1().min_h_0().child(content))
                    .into_any_element()
            }
        }
    }
}

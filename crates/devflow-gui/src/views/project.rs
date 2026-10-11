//! The selected project: header (name, path, badges, refresh), tabs, and the
//! tab content. Also the not-initialized / failed / loading states.
use super::{
    config::ConfigView, daemons::DaemonsView, data::DataView, dialogs,
    hooks_editor::HooksEditorView, logs::LogsView, onboarding, palette::tab_icon, ui,
    workspaces::WorkspacesView,
};
use crate::{
    model::{LoadPhase, ProjectModel, ServerBadge, Tab, shorten_home},
    redaction::shown,
    state::AppState,
};
use devflow_core::{config::CowCapability, db::CowSource, trust::TrustState};
use gpui_kit::{
    assets::IconName as Lucide,
    component::{
        ActiveTheme as _, Icon, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        select::{Select, SelectEvent, SelectItem, SelectState},
        spinner::Spinner,
        tab::{Tab as TabItem, TabBar},
        v_flex,
    },
    prelude::*,
    *,
};
use std::path::PathBuf;

/// A workspace in the picker: drawn masked (a legal name can look like a
/// credential), chosen by its recorded name.
#[derive(Clone)]
struct WorkspaceChoice {
    name: String,
    label: SharedString,
}

impl WorkspaceChoice {
    fn of(name: &str) -> Self {
        Self {
            name: name.to_string(),
            label: shown(name).into(),
        }
    }
}

impl SelectItem for WorkspaceChoice {
    type Value = String;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &String {
        &self.name
    }
}

/// Owned copy of what the header shows, so no borrow of the state outlives it.
struct HeaderInfo {
    name: String,
    root: PathBuf,
    badge: ServerBadge,
    cow: Option<(CowCapability, Option<CowSource>)>,
    /// The selected workspace's own scope is not approved (judged per
    /// workspace: a linked worktree has its own pitchfork files).
    untrusted: bool,
    /// The workspace that was judged, when one is selected.
    judged: Option<String>,
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
            untrusted: matches!(model.trust, Some(TrustState::Untrusted { .. })),
            judged: model.selected_workspace.clone(),
        }
    }
}

pub struct ProjectView {
    state: Entity<AppState>,
    pub(super) workspaces: Entity<WorkspacesView>,
    data: Entity<DataView>,
    daemons: Entity<DaemonsView>,
    pub(super) setup: Entity<HooksEditorView>,
    pub(super) config: Entity<ConfigView>,
    pub(super) logs: Entity<LogsView>,
    picker: Entity<SelectState<Vec<WorkspaceChoice>>>,
    picker_items: Vec<String>,
    _subscriptions: Vec<Subscription>,
}

impl ProjectView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let picker = cx.new(|cx| SelectState::new(Vec::<WorkspaceChoice>::new(), None, window, cx));
        let subscriptions = vec![
            cx.observe_in(&state, window, |this, _, window, cx| {
                this.sync_picker(window, cx);
                cx.notify();
            }),
            cx.subscribe(
                &picker,
                |this, _, event: &SelectEvent<Vec<WorkspaceChoice>>, cx| {
                    let SelectEvent::Confirm(Some(name)) = event else {
                        return;
                    };
                    let name = name.clone();
                    this.state.update(cx, |s, cx| s.select_workspace(name, cx));
                },
            ),
        ];
        Self {
            workspaces: cx.new(|cx| WorkspacesView::new(state.clone(), window, cx)),
            data: cx.new(|cx| DataView::new(state.clone(), window, cx)),
            daemons: cx.new(|cx| DaemonsView::new(state.clone(), window, cx)),
            setup: cx.new(|cx| HooksEditorView::new(state.clone(), window, cx)),
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
        let shown = self.picker.read(cx).selected_value().cloned();
        if !items_changed && shown == selected {
            return;
        }
        if items_changed {
            let items: Vec<WorkspaceChoice> =
                names.iter().map(|n| WorkspaceChoice::of(n)).collect();
            self.picker
                .update(cx, |p, cx| p.set_items(items, window, cx));
            self.picker_items = names;
        }
        if let Some(name) = selected {
            self.picker
                .update(cx, |p, cx| p.set_selected_value(&name, window, cx));
        }
    }

    fn header(
        &self,
        info: HeaderInfo,
        busy: Option<&str>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut badges = h_flex().gap_2().items_center();
        badges = badges.child(ui::server_tag(info.badge));
        if let Some((cow, source)) = info.cow {
            badges = badges.child(ui::cow_tag(cow, source));
        }
        if info.untrusted {
            let label = match &info.judged {
                Some(workspace) => format!("not trusted in {}", shown(workspace)),
                None => "not trusted".to_string(),
            };
            badges = badges.child(ui::pill(
                "trust-badge",
                label,
                ui::Tone::Warning,
                "What this workspace asks to run (hooks, [env], pitchfork processes) is not approved. Review it in the Setup tab.",
            ));
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
            .child(h_flex().gap_2().items_center().when_some(
                busy.map(str::to_string),
                |row, title| {
                    row.child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .text_sm()
                            .text_color(ui::muted(cx))
                            .child(Spinner::new().small())
                            .child(title),
                    )
                },
            ))
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
                    // A tab with `.icon()` renders the icon alone (the label
                    // becomes its accessible name), so the icon goes in the
                    // prefix and the label stays visible.
                    .children(Tab::ALL.iter().map(|&t| {
                        TabItem::new()
                            .prefix(div().pl_2().child(Icon::new(tab_icon(t)).size_3p5()))
                            .label(t.title())
                    })),
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
            let state = self.state.clone();
            return match entries_error {
                Some(message) => ui::empty_state(
                    Lucide::TriangleAlert,
                    "Cannot read the project list",
                    message,
                    Some(
                        Button::new("retry-projects")
                            .primary()
                            .label("Retry")
                            .on_click(move |_, _, cx| {
                                state.update(cx, |s, cx| s.reload_entries(None, None, cx))
                            }),
                    ),
                ),
                None => ui::empty_state(
                    Lucide::Rocket,
                    "Add your first project",
                    "Give every task its own worktree, copy-on-write data and setup hooks. Start by adding a Git repository.",
                    Some(
                        Button::new("add-project")
                            .primary()
                            .icon(IconName::Plus)
                            .label("Add project")
                            .on_click(move |_, window, cx| {
                                state.update(cx, |s, cx| s.add_project(window, cx))
                            }),
                    ),
                ),
            }
            .into_any_element();
        };
        let root = info.root.clone();
        match phase {
            LoadPhase::Loading => ui::loading("Loading project…", cx).into_any_element(),
            LoadPhase::NotInitialized => {
                let app = self.state.clone();
                let text = format!(
                    "{} has no devflow.toml. devflow will look at the repository, propose a setup (worktree files, a database, hooks, environment) and show you every line before it writes anything.",
                    shorten_home(&root)
                );
                let wizard_root = root.clone();
                let wizard_state = app.clone();
                v_flex()
                    .size_full()
                    .child(ui::empty_state(
                        Lucide::WandSparkles,
                        "Set up devflow for this project",
                        text,
                        Some(
                            Button::new("setup-wizard")
                                .primary()
                                .icon(Lucide::WandSparkles)
                                .label("Set up devflow…")
                                .on_click(move |_, window, cx| {
                                    onboarding::open(&wizard_state, wizard_root.clone(), window, cx)
                                }),
                        ),
                    ))
                    .child(
                        h_flex().pb_6().justify_center().child(
                            Button::new("initialize")
                                .ghost()
                                .small()
                                .label("Or start from a minimal devflow.toml")
                                .on_click(move |_, window, cx| {
                                    dialogs::open_initialize(&app, root.clone(), window, cx)
                                }),
                        ),
                    )
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
                    Tab::Data => self.data.clone().into_any_element(),
                    Tab::Daemons => self.daemons.clone().into_any_element(),
                    Tab::Setup => self.setup.clone().into_any_element(),
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

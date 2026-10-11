//! Daemons tab: the selected workspace's pitchfork daemons.
//!
//! What is shown and what the buttons may do both come from core
//! (`devflow_core::processes`): its state of the workspace's namespace, and its
//! gates for starting and stopping (the workspace is ready, its own pitchfork
//! files are the approved ones, no daemon starts on its own). The view never
//! decides; a disabled button carries core's reason.
use super::{
    pitchfork_panel,
    processes::{self, row_can_start, state_view, unread_reason},
    ui,
};
use crate::{model::ProcessTarget, redaction::shown, state::AppState};
use devflow_core::processes::{DaemonState, ProcessStatus};
use gpui_kit::{
    assets::IconName as Lucide,
    component::{
        ActiveTheme as _, Disableable as _, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        table::{Column, DataTable, TableDelegate, TableState},
        v_flex,
    },
    prelude::*,
    *,
};

struct DaemonTable {
    state: Entity<AppState>,
    rows: Vec<DaemonState>,
    /// The workspace these rows are the status of: what a row's Start acts on.
    target: Option<ProcessTarget>,
    /// Why the rows' states were not read, when they were not.
    unread: Option<String>,
    columns: Vec<Column>,
    busy: bool,
}

impl TableDelegate for DaemonTable {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let daemon = &self.rows[row_ix];
        match self.columns[col_ix].key.as_ref() {
            "name" => h_flex()
                .gap_2()
                .items_center()
                .child(div().truncate().child(processes::daemon_label(daemon)))
                .when(!daemon.triggers.is_empty(), |row| {
                    row.child(ui::pill(
                        "daemon-triggers",
                        format!("starts on its own: {}", daemon.triggers.join(", ")),
                        ui::Tone::Warning,
                        "pitchfork starts this daemon without devflow (cron, watch, boot_start or auto), so devflow does not start everything at once.",
                    ))
                })
                .into_any_element(),
            "state" => h_flex()
                .gap_2()
                .items_center()
                .child({
                    let (word, tone, help) = state_view(daemon, self.unread.as_deref());
                    ui::pill("daemon-state", word, tone, help)
                })
                .when_some(daemon.error.clone(), |row, error| {
                    row.child(
                        div()
                            .truncate()
                            .text_xs()
                            .text_color(cx.theme().danger)
                            .child(error),
                    )
                })
                .into_any_element(),
            "port" => div()
                .child(if daemon.ports.is_empty() {
                    "—".to_string()
                } else {
                    daemon
                        .ports
                        .iter()
                        .map(u16::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .into_any_element(),
            "url" => match daemon.url.clone() {
                Some(url) => {
                    let target = url.clone();
                    Button::new(("daemon-url", row_ix))
                        .ghost()
                        .xsmall()
                        .label(url)
                        .tooltip("Open in the browser")
                        .on_click(move |_, _, cx| cx.open_url(&target))
                        .into_any_element()
                }
                None => div()
                    .text_color(ui::muted(cx))
                    .child("—")
                    .into_any_element(),
            },
            _ => {
                let (state, name) = (self.state.clone(), daemon.name.clone());
                let target = self.target.clone();
                Button::new(("daemon-start", row_ix))
                    .icon(IconName::Play)
                    .ghost()
                    .xsmall()
                    .tooltip(if daemon.startable {
                        "Start this daemon"
                    } else {
                        "Cannot be started now (see the notes above)"
                    })
                    .disabled(!row_can_start(daemon, self.busy) || target.is_none())
                    .on_click(move |_, window, cx| {
                        if let Some(target) = target.clone() {
                            processes::start_one(&state, target, &name, window, cx)
                        }
                    })
                    .into_any_element()
            }
        }
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let state = self.state.clone();
        ui::empty_state(
            Lucide::Server,
            "No daemons in this workspace",
            "A pitchfork.toml with daemons runs this project's servers per workspace, each with its own URL. devflow can propose one from what it finds in the repository.",
            Some(
                Button::new("daemons-detect")
                    .primary()
                    .label("Detect processes…")
                    .on_click(move |_, _, cx| state.update(cx, |s, cx| s.request_setup_wizard(cx))),
            ),
        )
    }
}

/// The line above the daemons: whose they are, the name masked.
pub(crate) fn title(workspace: Option<&str>) -> String {
    format!(
        "Daemons in {}",
        workspace.map_or_else(|| "no workspace".into(), shown)
    )
}

pub struct DaemonsView {
    state: Entity<AppState>,
    table: Entity<TableState<DaemonTable>>,
    _subscription: Subscription,
}

impl DaemonsView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let table = cx.new(|cx| {
            let delegate = DaemonTable {
                state: state.clone(),
                rows: Vec::new(),
                target: None,
                unread: None,
                columns: vec![
                    Column::new("name", "Daemon").width(300.),
                    Column::new("state", "State").width(200.),
                    Column::new("port", "Port").width(90.),
                    Column::new("url", "URL").width(300.),
                    Column::new("actions", "").width(60.),
                ],
                busy: false,
            };
            TableState::new(delegate, window, cx)
                .sortable(false)
                .col_movable(false)
        });
        let subscription = cx.observe(&state, |this, _, cx| {
            this.sync(cx);
            cx.notify();
        });
        let mut this = Self {
            state,
            table,
            _subscription: subscription,
        };
        this.sync(cx);
        this
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let (rows, target, unread, busy) = {
            let state = self.state.read(cx);
            let Some(model) = state.selection.current() else {
                return;
            };
            (
                model.pitchfork.daemons().to_vec(),
                model
                    .pitchfork
                    .status
                    .as_ref()
                    .map(|status| ProcessTarget::of(model, status)),
                model.pitchfork.status.as_ref().and_then(unread_reason),
                state.busy().is_some(),
            )
        };
        self.table.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            delegate.rows = rows;
            delegate.target = target;
            delegate.unread = unread;
            delegate.busy = busy;
            table.refresh(cx);
        });
    }
}

impl Render for DaemonsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (pitchfork, target, workspace, busy, refusals, install) = {
            let state = self.state.read(cx);
            let Some(model) = state.selection.current() else {
                return div().into_any_element();
            };
            (
                model.pitchfork.clone(),
                model
                    .pitchfork
                    .status
                    .as_ref()
                    .map(|status| ProcessTarget::of(model, status)),
                model.selected_workspace.clone(),
                state.busy().is_some(),
                model.refusals.clone(),
                state.pitchfork_install.clone(),
            )
        };
        if !pitchfork.loaded {
            return ui::loading("Looking for pitchfork…", cx).into_any_element();
        }
        let offer = pitchfork.offer();
        let mut page = v_flex().size_full().p_4().gap_3();
        if let Some(card) =
            pitchfork_panel::install_card(pitchfork_panel::TAB, &offer, &install, &self.state, cx)
        {
            page = page.child(card);
        }
        let Some(status) = pitchfork.status.clone() else {
            // Installed, but nothing to show: no workspace, or core could not be read.
            return page
                .when(!pitchfork.installed(), |page| {
                    page.child(ui::empty_state(
                        Lucide::Server,
                        "No daemons to show",
                        "Daemons appear here per workspace once pitchfork is installed.",
                        None,
                    ))
                })
                .children(pitchfork.error.clone().map(|e| ui::error_banner(e, cx)))
                .into_any_element();
        };
        if let Some(card) = pitchfork_panel::proxy_card(pitchfork_panel::TAB, &offer, cx) {
            page = page.child(card);
        }
        if let Some(target) = &target {
            page = page.child(self.header(&status, target, workspace, busy, cx));
        }
        for (id, text) in processes::notes(&status) {
            page = page.child(
                div()
                    .id(id)
                    .test_support()
                    .text_sm()
                    .text_color(ui::muted(cx))
                    .child(text),
            );
        }
        if let (Some(refusal), Some(target)) = (refusals.get(&status.path), &target) {
            page = page.child(processes::refusal_banner(
                "start-refusal",
                refusal,
                target,
                &self.state,
                cx,
            ));
        }
        page.children(pitchfork.error.clone().map(|e| ui::error_banner(e, cx)))
            .children(status.error.clone().map(|e| ui::error_banner(e, cx)))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(DataTable::new(&self.table).stripe(true)),
            )
            .into_any_element()
    }
}

impl DaemonsView {
    fn header(
        &self,
        status: &ProcessStatus,
        target: &ProcessTarget,
        workspace: Option<String>,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .gap_3()
            .items_center()
            .justify_between()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .text_color(ui::muted(cx))
                            .child(title(workspace.as_deref())),
                    )
                    .child(
                        div()
                            .id("daemons-namespace")
                            .text_xs()
                            .font_family("Menlo")
                            .text_color(ui::muted(cx))
                            .child(status.namespace.value.clone()),
                    ),
            )
            .child(h_flex().gap_2().children(processes::buttons(
                "daemons-",
                &self.state,
                status,
                target,
                busy,
            )))
    }
}

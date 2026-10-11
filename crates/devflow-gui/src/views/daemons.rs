//! Daemons tab: the selected workspace's pitchfork daemons.
use super::ui;
use crate::{model::shorten_home, state::AppState};
use devflow_core::pitchfork::Daemon;
use gpui_kit::{
    component::{
        Disableable as _, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        table::{Column, DataTable, TableDelegate, TableState},
        v_flex,
    },
    prelude::*,
    *,
};

/// Pitchfork keeps "waiting" daemons alive while their ready check runs.
fn is_running(status: &str) -> bool {
    matches!(status.to_ascii_lowercase().as_str(), "running" | "waiting")
}

struct DaemonTable {
    state: Entity<AppState>,
    rows: Vec<Daemon>,
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
            "id" => div().truncate().child(daemon.id.clone()).into_any_element(),
            "status" => {
                let tone = if is_running(&daemon.status) {
                    ui::Tone::Success
                } else {
                    ui::Tone::Neutral
                };
                ui::tag(daemon.status.clone(), tone).into_any_element()
            }
            "pid" => div()
                .child(daemon.pid.map_or("—".to_string(), |p| p.to_string()))
                .into_any_element(),
            "dir" => div()
                .truncate()
                .text_color(ui::muted(cx))
                .child(
                    daemon
                        .dir
                        .as_ref()
                        .map_or(String::new(), |d| shorten_home(d)),
                )
                .into_any_element(),
            _ => {
                let running = is_running(&daemon.status);
                let button = |id: &'static str,
                              icon: IconName,
                              tip: &'static str,
                              start: bool,
                              disabled: bool| {
                    let (state, daemon_id) = (self.state.clone(), daemon.id.clone());
                    Button::new((id, row_ix))
                        .icon(icon)
                        .ghost()
                        .xsmall()
                        .tooltip(tip)
                        .disabled(disabled)
                        .on_click(move |_, window, cx| {
                            let id = daemon_id.clone();
                            state.update(cx, |s, cx| s.daemon_action(id, start, window, cx));
                        })
                };
                h_flex()
                    .gap_1()
                    .child(button(
                        "start",
                        IconName::Play,
                        "Start",
                        true,
                        self.busy || running,
                    ))
                    .child(button(
                        "stop",
                        IconName::Square,
                        "Stop",
                        false,
                        self.busy || !running,
                    ))
                    .into_any_element()
            }
        }
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(ui::muted(cx))
            .child("No daemons in this workspace")
    }
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
                columns: vec![
                    Column::new("id", "Daemon").width(240.),
                    Column::new("status", "Status").width(110.),
                    Column::new("pid", "PID").width(90.),
                    Column::new("dir", "Directory").width(330.),
                    Column::new("actions", "").width(90.),
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
        let (rows, busy) = {
            let state = self.state.read(cx);
            let Some(model) = state.selection.current() else {
                return;
            };
            (model.pitchfork.daemons.clone(), state.busy().is_some())
        };
        self.table.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            delegate.rows = rows;
            delegate.busy = busy;
            table.refresh(cx);
        });
    }
}

impl Render for DaemonsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (available, error, workspace, loaded) = {
            let state = self.state.read(cx);
            let Some(model) = state.selection.current() else {
                return div().into_any_element();
            };
            (
                model.pitchfork.available,
                model.pitchfork.error.clone(),
                model.selected_workspace.clone(),
                model.pitchfork.loaded,
            )
        };
        if !loaded {
            return ui::loading("Looking for pitchfork…", cx).into_any_element();
        }
        if !available {
            return ui::note(
                "pitchfork is not installed",
                "devflow does not supervise processes. Install pitchfork (https://pitchfork.jdx.dev) and declare daemons in pitchfork.toml; they will be listed here per workspace.",
                cx,
            )
            .into_any_element();
        }
        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .child(div().text_sm().text_color(ui::muted(cx)).child(format!(
                "Daemons running in {}",
                workspace.unwrap_or_else(|| "no workspace".into())
            )))
            .children(error.map(|e| ui::error_banner(e, cx)))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(DataTable::new(&self.table).stripe(true)),
            )
            .into_any_element()
    }
}

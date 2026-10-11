//! Database tab: server status and control, branches, snapshots.
use super::{db_dialogs, dialogs, ui};
use crate::{
    model::{BranchRow, DatabaseModel, ServerBadge, SnapshotRow, Tab, branch_rows, snapshot_rows},
    state::AppState,
};
use devflow_core::config::PostgresMode;
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

struct BranchTable {
    state: Entity<AppState>,
    rows: Vec<BranchRow>,
    columns: Vec<Column>,
}

impl TableDelegate for BranchTable {
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
        let row = &self.rows[row_ix];
        match self.columns[col_ix].key.as_ref() {
            "name" => h_flex()
                .gap_2()
                .child(div().truncate().child(row.name.clone()))
                .when(row.unattached, |r| {
                    r.child(ui::pill(
                        "unattached-branch",
                        "unattached locally",
                        ui::Tone::Warning,
                        "No workspace of this project on this machine is bound to this branch.",
                    ))
                })
                .into_any_element(),
            "database" => div()
                .truncate()
                .text_color(ui::muted(cx))
                .child(row.database.clone())
                .into_any_element(),
            "source" => div()
                .truncate()
                .child(row.source.clone())
                .into_any_element(),
            "status" => div().child(row.status.clone()).into_any_element(),
            "size" => div().child(row.size.clone()).into_any_element(),
            _ => div().child(row.cow).into_any_element(),
        }
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let state = self.state.clone();
        ui::empty_inline(
            "No branches yet: every workspace gets one of its own.",
            Some(
                Button::new("branches-new-workspace")
                    .primary()
                    .small()
                    .label("New workspace")
                    .on_click(move |_, window, cx| dialogs::open_new_workspace(&state, window, cx)),
            ),
            cx,
        )
    }
}

struct SnapshotTable {
    state: Entity<AppState>,
    rows: Vec<SnapshotRow>,
    columns: Vec<Column>,
    busy: bool,
}

impl TableDelegate for SnapshotTable {
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
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let row = &self.rows[row_ix];
        match self.columns[col_ix].key.as_ref() {
            "snapshot" => div().truncate().child(row.label()).into_any_element(),
            "source" => div()
                .truncate()
                .child(row.source.clone())
                .into_any_element(),
            "created" => div().child(row.created.clone()).into_any_element(),
            _ => {
                let (state, name, version) = (self.state.clone(), row.name.clone(), row.version);
                let label = row.label();
                Button::new(("delete-snapshot", row_ix))
                    .icon(IconName::Delete)
                    .ghost()
                    .xsmall()
                    .tooltip("Delete snapshot")
                    .disabled(self.busy)
                    .on_click(move |_, window, cx| {
                        let (state, name) = (state.clone(), name.clone());
                        dialogs::open_confirm(
                            window,
                            cx,
                            "Delete snapshot",
                            vec![
                                format!("Delete {label}? Workspaces already created from it keep their data."),
                                "Version numbers are never reused.".into(),
                            ],
                            "Delete",
                            move |window, cx| {
                                let name = name.clone();
                                state.update(cx, |s, cx| s.delete_snapshot(name.as_str().to_string(), version, window, cx));
                            },
                        );
                    })
                    .into_any_element()
            }
        }
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let state = self.state.clone();
        ui::empty_inline(
            "No snapshots yet: a snapshot is a sealed copy new workspaces can start from.",
            Some(
                Button::new("snapshots-create")
                    .primary()
                    .small()
                    .label("Create snapshot…")
                    .on_click(move |_, window, cx| db_dialogs::open_snapshot(&state, window, cx)),
            ),
            cx,
        )
    }
}

pub struct DatabaseView {
    state: Entity<AppState>,
    branches: Entity<TableState<BranchTable>>,
    snapshots: Entity<TableState<SnapshotTable>>,
    _subscription: Subscription,
}

impl DatabaseView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let branches = cx.new(|cx| {
            let delegate = BranchTable {
                state: state.clone(),
                rows: Vec::new(),
                columns: vec![
                    Column::new("name", "Name").width(260.),
                    Column::new("database", "Database").width(170.),
                    Column::new("source", "Source").width(200.),
                    Column::new("status", "Status").width(90.),
                    Column::new("size", "Size").width(90.),
                    Column::new("cow", "CoW").width(90.),
                ],
            };
            TableState::new(delegate, window, cx)
                .sortable(false)
                .col_movable(false)
        });
        let snapshots = cx.new(|cx| {
            let delegate = SnapshotTable {
                state: state.clone(),
                rows: Vec::new(),
                columns: vec![
                    Column::new("snapshot", "Snapshot").width(240.),
                    Column::new("source", "Source").width(240.),
                    Column::new("created", "Created").width(170.),
                    Column::new("actions", "").width(70.),
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
            branches,
            snapshots,
            _subscription: subscription,
        };
        this.sync(cx);
        this
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let (branches, snapshots, busy) = {
            let state = self.state.read(cx);
            let Some(model) = state.selection.current() else {
                return;
            };
            (
                branch_rows(&model.database, &model.workspaces),
                snapshot_rows(&model.database),
                state.busy().is_some(),
            )
        };
        self.branches.update(cx, |table, cx| {
            table.delegate_mut().rows = branches;
            table.refresh(cx);
        });
        self.snapshots.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            delegate.rows = snapshots;
            delegate.busy = busy;
            table.refresh(cx);
        });
    }

    fn server_card(
        &self,
        db: &DatabaseModel,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let badge = crate::model::server_badge(db);
        let status = db.status.as_ref();
        let managed = status.is_some_and(|s| s.mode == PostgresMode::Managed);
        let kv = |label: &'static str, value: String| {
            v_flex()
                .gap_0p5()
                .child(div().text_xs().text_color(ui::muted(cx)).child(label))
                .child(div().text_sm().child(value))
        };
        let dash = || "—".to_string();
        h_flex()
            .p_3()
            .gap_6()
            .items_center()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .child(ui::server_tag(badge))
            .child(kv(
                "Endpoint",
                status.and_then(|s| s.endpoint.clone()).unwrap_or_else(dash),
            ))
            .child(kv(
                "Version",
                status
                    .and_then(|s| s.server_version.clone())
                    .unwrap_or_else(dash),
            ))
            .child(kv(
                "file_copy_method",
                status
                    .and_then(|s| s.file_copy_method.clone())
                    .unwrap_or_else(dash),
            ))
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .text_xs()
                            .text_color(ui::muted(cx))
                            .child("Copy-on-write"),
                    )
                    .child(match status {
                        Some(s) => ui::cow_tag(s.cow, s.cow_source).into_any_element(),
                        None => div().text_sm().child("—").into_any_element(),
                    }),
            )
            .child(div().flex_1())
            .child(
                Button::new("server-up")
                    // The suggested action only while the server is not running.
                    .when(badge != ServerBadge::Running, |button| button.primary())
                    .small()
                    .icon(IconName::Play)
                    .label(if managed || status.is_none() {
                        "Start"
                    } else {
                        "Check"
                    })
                    .disabled(busy || badge == ServerBadge::Running && !managed)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.state.update(cx, |s, cx| s.server_up(window, cx))
                    })),
            )
            .when(managed, |row| {
                row.child(
                    Button::new("server-down")
                        .small()
                        .icon(IconName::Square)
                        .label("Stop")
                        .disabled(busy || badge != ServerBadge::Running)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.state.update(cx, |s, cx| s.server_down(window, cx))
                        })),
                )
            })
    }
}

impl Render for DatabaseView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (db, busy) = {
            let state = self.state.read(cx);
            let Some(model) = state.selection.current() else {
                return div().into_any_element();
            };
            (model.database.clone(), state.busy().is_some())
        };
        if !db.configured {
            let state = self.state.clone();
            return ui::empty_state(
                Lucide::Database,
                "PostgreSQL branching is not configured",
                "Add a postgres service ([services.db], engine = \"postgres\") to devflow.toml to give every workspace its own copy-on-write database.",
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
        let problem = db
            .status
            .as_ref()
            .and_then(|s| s.problem.clone())
            .or_else(|| db.error.clone());
        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .child(self.server_card(&db, busy, cx))
            .children(problem.map(|p| ui::error_banner(p, cx)))
            .child(ui::heading("Branches"))
            .child(
                div()
                    .flex_1()
                    .min_h(px(120.))
                    .child(DataTable::new(&self.branches).stripe(true)),
            )
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(ui::heading("Snapshots"))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("create-snapshot")
                                    .small()
                                    .icon(IconName::Plus)
                                    .label("Create snapshot…")
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        db_dialogs::open_snapshot(&this.state, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("reset-data")
                                    .small()
                                    .icon(IconName::RotateCw)
                                    .label("Reset workspace data…")
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        db_dialogs::open_reset(&this.state, window, cx)
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(120.))
                    .child(DataTable::new(&self.snapshots).stripe(true)),
            )
            .into_any_element()
    }
}

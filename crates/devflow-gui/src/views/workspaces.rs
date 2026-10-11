//! Workspaces tab: the table with row actions, "New workspace", and the
//! output of the running (or last) operation.
use super::{
    activity_panel::ActivityPanel, dialogs, overview::OverviewView, ui,
    workspace_detail::WorkspaceDetailView,
};
use crate::{
    model::{WorkspaceRow, workspace_rows},
    state::{AppState, TrustPurpose},
};
use devflow_core::worktree::Workspace;
use gpui_kit::{
    assets::IconName as Lucide,
    component::{
        Disableable as _, IconName, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        menu::{PopupMenu, PopupMenuItem},
        notification::Notification,
        table::{Column, DataTable, TableDelegate, TableEvent, TableState},
        v_flex,
    },
    prelude::*,
    *,
};

#[derive(Clone, Copy)]
enum RowAction {
    Editor,
    Terminal,
    CopyPath,
    Setup,
    Remove,
}

fn perform(
    action: RowAction,
    state: &Entity<AppState>,
    ws: &Workspace,
    window: &mut Window,
    cx: &mut App,
) {
    match action {
        RowAction::Editor => state.update(cx, |s, cx| s.open_in_editor(&ws.path, window, cx)),
        RowAction::Terminal => state.update(cx, |s, cx| s.open_in_terminal(&ws.path, window, cx)),
        RowAction::CopyPath => {
            cx.write_to_clipboard(ClipboardItem::new_string(ws.path.display().to_string()));
            window.push_notification(Notification::success("Path copied."), cx);
        }
        RowAction::Setup => {
            let (app, ws) = (state.clone(), ws.clone());
            let path = ws.path.clone();
            dialogs::with_trust(
                state,
                &path,
                TrustPurpose::Setup,
                window,
                cx,
                move |trust, window, cx| {
                    let ws = ws.clone();
                    app.update(cx, |s, cx| s.rerun_setup(ws, trust, window, cx));
                },
            );
        }
        RowAction::Remove => dialogs::open_remove(state, ws, window, cx),
    }
}

pub(super) struct WorkspaceTable {
    state: Entity<AppState>,
    rows: Vec<WorkspaceRow>,
    columns: Vec<Column>,
    busy: bool,
}

impl WorkspaceTable {
    fn new(state: Entity<AppState>) -> Self {
        Self {
            state,
            rows: Vec::new(),
            columns: vec![
                // Sums to 876 px: the row actions must stay visible in the
                // default window (968 px of content) without horizontal scrolling.
                Column::new("name", "Name").width(150.),
                Column::new("branch", "Branch").width(130.),
                Column::new("status", "Status").width(125.),
                Column::new("database", "Database").width(200.),
                Column::new("path", "Path").width(200.),
                Column::new("actions", "").width(150.),
            ],
            busy: false,
        }
    }

    fn actions(&self, row_ix: usize) -> AnyElement {
        let row = &self.rows[row_ix];
        let button = |id: &'static str,
                      icon: IconName,
                      tip: &'static str,
                      action: RowAction,
                      disabled: bool| {
            let (state, ws) = (self.state.clone(), row.workspace.clone());
            Button::new((id, row_ix))
                .icon(icon)
                .ghost()
                .xsmall()
                .tooltip(tip)
                .disabled(disabled)
                .on_click(move |_, window, cx| perform(action, &state, &ws, window, cx))
        };
        let protected = row.workspace.is_default;
        h_flex()
            .gap_1()
            .child(button(
                "editor",
                IconName::FolderOpen,
                "Open in editor",
                RowAction::Editor,
                false,
            ))
            .child(button(
                "terminal",
                IconName::SquareTerminal,
                "Open in terminal",
                RowAction::Terminal,
                false,
            ))
            .child(button(
                "copy",
                IconName::Copy,
                "Copy path",
                RowAction::CopyPath,
                false,
            ))
            .child(button(
                "setup",
                IconName::RotateCw,
                "Rerun setup",
                RowAction::Setup,
                self.busy,
            ))
            .child(button(
                "remove",
                IconName::Delete,
                "Remove",
                RowAction::Remove,
                self.busy || protected,
            ))
            .into_any_element()
    }
}

impl TableDelegate for WorkspaceTable {
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
                .child(div().truncate().child(row.label()))
                .when(row.workspace.is_default, |r| {
                    r.child(ui::pill(
                        "default-workspace",
                        "default",
                        ui::Tone::Neutral,
                        "The main checkout. It cannot be removed.",
                    ))
                })
                .into_any_element(),
            "branch" => div()
                .truncate()
                .child(row.branch.clone())
                .into_any_element(),
            "status" => ui::workspace_tag(row.state).into_any_element(),
            "database" => div()
                .truncate()
                .text_color(ui::muted(cx))
                .child(row.database.clone())
                .into_any_element(),
            "path" => div()
                .truncate()
                .text_color(ui::muted(cx))
                .child(row.path.clone())
                .into_any_element(),
            _ => self.actions(row_ix),
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let row = &self.rows[row_ix];
        let item = |label: &'static str, icon: IconName, action: RowAction, disabled: bool| {
            let (state, ws) = (self.state.clone(), row.workspace.clone());
            PopupMenuItem::new(label)
                .icon(icon)
                .disabled(disabled)
                .on_click(move |_, window, cx| perform(action, &state, &ws, window, cx))
        };
        menu.item(item(
            "Open in editor",
            IconName::FolderOpen,
            RowAction::Editor,
            false,
        ))
        .item(item(
            "Open in terminal",
            IconName::SquareTerminal,
            RowAction::Terminal,
            false,
        ))
        .item(item(
            "Copy path",
            IconName::Copy,
            RowAction::CopyPath,
            false,
        ))
        .item(PopupMenuItem::separator())
        .item(item(
            "Rerun setup",
            IconName::RotateCw,
            RowAction::Setup,
            self.busy,
        ))
        .item(item(
            "Remove…",
            IconName::Delete,
            RowAction::Remove,
            self.busy || row.workspace.is_default,
        ))
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let state = self.state.clone();
        ui::empty_state(
            Lucide::GitBranch,
            "No workspaces",
            "A workspace is a worktree with its own branch and data. Create one to work on a task in isolation.",
            Some(
                Button::new("empty-new-workspace")
                    .primary()
                    .icon(IconName::Plus)
                    .label("New workspace")
                    .disabled(self.busy)
                    .on_click(move |_, window, cx| dialogs::open_new_workspace(&state, window, cx)),
            ),
        )
    }
}

pub struct WorkspacesView {
    state: Entity<AppState>,
    pub(super) table: Entity<TableState<WorkspaceTable>>,
    /// The project at a glance, with the one primary action.
    overview: Entity<OverviewView>,
    /// The output of the running (or last) operation.
    activity: Entity<ActivityPanel>,
    /// The selected workspace in detail, below the table.
    detail: Entity<WorkspaceDetailView>,
    _subscriptions: Vec<Subscription>,
}

impl WorkspacesView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let table = cx.new(|cx| {
            TableState::new(WorkspaceTable::new(state.clone()), window, cx)
                .sortable(false)
                .col_movable(false)
        });
        let subscriptions = vec![
            cx.observe_in(&state, window, |this, _, window, cx| {
                this.sync(window, cx);
                cx.notify();
            }),
            // The clicked row is the workspace the Daemons and Logs tabs show.
            cx.subscribe(&table, |this, table, event: &TableEvent, cx| {
                if let TableEvent::SelectRow(ix) = event {
                    let name = table
                        .read(cx)
                        .delegate()
                        .rows
                        .get(*ix)
                        .map(|r| r.workspace.name.clone());
                    if let Some(name) = name {
                        this.state.update(cx, |s, cx| s.select_workspace(name, cx));
                    }
                }
            }),
        ];
        let detail = cx.new(|cx| WorkspaceDetailView::new(state.clone(), window, cx));
        let activity = cx.new(|cx| ActivityPanel::new(state.clone(), window, cx));
        let overview = cx.new(|cx| OverviewView::new(state.clone(), window, cx));
        let mut this = Self {
            state,
            table,
            overview,
            activity,
            detail,
            _subscriptions: subscriptions,
        };
        this.sync(window, cx);
        this
    }

    /// Push model changes into the table.
    fn sync(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let (rows, selected, busy) = {
            let state = self.state.read(cx);
            let Some(model) = state.selection.current() else {
                return;
            };
            let rows = workspace_rows(&model.workspaces);
            // The row of the workspace the other tabs show (also when it was
            // chosen elsewhere, such as in the sidebar).
            let selected = model
                .selected_workspace
                .as_ref()
                .and_then(|name| rows.iter().position(|r| &r.workspace.name == name));
            (rows, selected, state.busy().is_some())
        };
        self.table.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            delegate.rows = rows;
            delegate.busy = busy;
            table.refresh(cx);
            if let Some(ix) = selected
                && table.selected_row() != Some(ix)
            {
                table.set_selected_row(ix, cx);
            }
        });
    }
}

impl Render for WorkspacesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (error, has_detail) = {
            let state = self.state.read(cx);
            let model = state.selection.current();
            (
                model.and_then(|m| m.workspaces_error.clone()),
                model.is_some_and(|m| {
                    m.selected_workspace
                        .as_ref()
                        .is_some_and(|name| m.workspace(name).is_some())
                }),
            )
        };
        v_flex()
            .size_full()
            .child(div().px_4().pt_3().pb_2().child(self.overview.clone()))
            .children(error.map(|e| div().px_4().pb_2().child(ui::error_banner(e, cx))))
            .child(
                div()
                    .when(has_detail, |table| table.flex_none().h(px(228.)))
                    .when(!has_detail, |table| table.flex_1().min_h_0())
                    .px_4()
                    .pb_2()
                    .child(DataTable::new(&self.table).stripe(true)),
            )
            .child(self.activity.clone())
            .when(has_detail, |page| {
                page.child(div().flex_1().min_h_0().child(self.detail.clone()))
            })
    }
}

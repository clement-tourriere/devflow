//! Workspaces tab: the table with row actions, "New workspace", and the
//! output of the running (or last) operation.
use super::{dialogs, ui};
use crate::{
    activity::{Activity, state_label},
    jobs::Done,
    model::{Section, WorkspaceRow, workspace_rows},
    state::AppState,
};
use devflow_core::{
    receipt::{Phase, PhaseState},
    worktree::Workspace,
};
use gpui_kit::{
    component::{
        ActiveTheme as _, Disableable as _, IconName, Sizable as _, WindowExt as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Editor, EditorState},
        menu::{PopupMenu, PopupMenuItem},
        notification::Notification,
        spinner::Spinner,
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
            dialogs::with_trust(state, false, window, cx, move |trust, window, cx| {
                let ws = ws.clone();
                app.update(cx, |s, cx| s.rerun_setup(ws, trust, window, cx));
            });
        }
        RowAction::Remove => dialogs::open_remove(state, ws, window, cx),
    }
}

struct WorkspaceTable {
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
                .child(div().truncate().child(row.workspace.name.clone()))
                .when(row.workspace.is_default, |r| {
                    r.child(ui::tag("default", ui::Tone::Neutral))
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
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(ui::muted(cx))
            .child("No workspaces")
    }
}

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

pub struct WorkspacesView {
    state: Entity<AppState>,
    table: Entity<TableState<WorkspaceTable>>,
    output: Entity<EditorState>,
    shown_revision: Option<u64>,
    _subscriptions: Vec<Subscription>,
}

impl WorkspacesView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let table = cx.new(|cx| {
            TableState::new(WorkspaceTable::new(state.clone()), window, cx)
                .sortable(false)
                .col_movable(false)
        });
        let output = cx.new(|cx| {
            EditorState::new(window, cx)
                .line_number(false)
                .soft_wrap(true)
        });
        output.update(cx, |editor, cx| editor.set_readonly(true, cx));
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
        let mut this = Self {
            state,
            table,
            output,
            shown_revision: None,
            _subscriptions: subscriptions,
        };
        this.sync(window, cx);
        this
    }

    /// Push model changes into the table and the output editor.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (rows, busy, revision, text) = {
            let state = self.state.read(cx);
            let Some(model) = state.selection.current() else {
                return;
            };
            let activity = state
                .activity
                .as_ref()
                .filter(|a| a.key.project == model.key.project);
            let revision = activity.map(|a| a.log.revision());
            (
                workspace_rows(&model.workspaces),
                state.busy().is_some(),
                revision,
                // The output can be megabytes: build it only when it changed.
                activity
                    .filter(|_| revision != self.shown_revision)
                    .map(|a| a.log.text()),
            )
        };
        self.table.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            delegate.rows = rows;
            delegate.busy = busy;
            table.refresh(cx);
        });
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

    fn activity_panel(&self, activity: ActivityView, cx: &mut Context<Self>) -> impl IntoElement {
        let running = activity.running();
        let phases = activity
            .phases
            .iter()
            .filter(|(_, state, _)| *state != PhaseState::Pending);
        let outcome = match &activity.outcome {
            Some(Ok(done)) => Some(ui::tag(
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
            )),
            Some(Err(_)) => Some(ui::tag("failed", ui::Tone::Danger)),
            None => None,
        };
        v_flex()
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
                        ui::tag(text, tone)
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
                    .h(px(180.))
                    .child(Editor::new(&self.output).bordered(true).h_full()),
            )
    }
}

impl Render for WorkspacesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (busy, error, activity) = {
            let state = self.state.read(cx);
            let model = state.selection.current();
            let project = model.map(|m| m.key.project);
            (
                state.busy().is_some(),
                model.and_then(|m| m.workspaces_error.clone()),
                state
                    .activity
                    .as_ref()
                    .filter(|a| Some(a.key.project) == project)
                    .map(ActivityView::of),
            )
        };
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_4()
                    .py_2()
                    .gap_2()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .text_color(ui::muted(cx))
                            .child("Right-click a row for more actions."),
                    )
                    .child(
                        Button::new("new-workspace")
                            .primary()
                            .icon(IconName::Plus)
                            .label("New workspace")
                            .small()
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                dialogs::open_new_workspace(&this.state, window, cx)
                            })),
                    ),
            )
            .children(error.map(|e| div().px_4().pb_2().child(ui::error_banner(e, cx))))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px_4()
                    .pb_2()
                    .child(DataTable::new(&self.table).stripe(true)),
            )
            .children(activity.map(|a| self.activity_panel(a, cx)))
    }
}

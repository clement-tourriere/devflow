//! Database dialogs: create a snapshot, reset a workspace's data.
use super::dialogs::{cancel_button, footer};
use crate::{
    model::{DataOption, data_options, data_workspaces},
    state::AppState,
};
use devflow_core::worktree::Workspace;
use gpui_kit::{
    component::{
        ActiveTheme as _, IndexPath, WindowExt as _,
        button::{Button, ButtonVariants as _},
        input::{Input, InputEvent, InputState},
        select::{Select, SelectState},
        v_flex,
    },
    prelude::*,
    *,
};

fn workspace_select<T: 'static>(
    workspaces: &[Workspace],
    window: &mut Window,
    cx: &mut Context<T>,
) -> Entity<SelectState<Vec<SharedString>>> {
    // Drawn text only: a choice is the position in the list.
    let items: Vec<SharedString> = workspaces
        .iter()
        .map(|w| devflow_core::redact::text(&w.name).into())
        .collect();
    cx.new(|cx| SelectState::new(items, Some(IndexPath::new(0)), window, cx))
}

fn selected(select: &Entity<SelectState<Vec<SharedString>>>, cx: &App) -> Option<usize> {
    select.read(cx).selected_index(cx).map(|ix| ix.row)
}

fn label(text: &'static str) -> Div {
    div().text_sm().font_weight(FontWeight::MEDIUM).child(text)
}

// ---- snapshot ------------------------------------------------------------------

pub fn open_snapshot(state: &Entity<AppState>, window: &mut Window, cx: &mut App) {
    let workspaces = match state.read(cx).selection.current() {
        Some(model) => data_workspaces(&model.workspaces),
        None => return,
    };
    if workspaces.is_empty() {
        window.push_notification(
            gpui_kit::component::notification::Notification::info(
                "No workspace has a database yet; set one up first.",
            ),
            cx,
        );
        return;
    }
    let state = state.clone();
    let view = cx.new(|cx| {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("baseline"));
        let subscription = cx.subscribe_in(
            &name,
            window,
            |this: &mut SnapshotDialog, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.submit(window, cx);
                }
            },
        );
        SnapshotDialog {
            state,
            name,
            select: workspace_select(&workspaces, window, cx),
            workspaces,
            error: None,
            _subscription: subscription,
        }
    });
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("Create snapshot")
            .w(px(480.))
            .child(view.clone())
    });
}

struct SnapshotDialog {
    state: Entity<AppState>,
    name: Entity<InputState>,
    select: Entity<SelectState<Vec<SharedString>>>,
    workspaces: Vec<Workspace>,
    error: Option<SharedString>,
    _subscription: Subscription,
}

impl SnapshotDialog {
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).value().trim().to_string();
        let Some(workspace) = selected(&self.select, cx)
            .and_then(|ix| self.workspaces.get(ix))
            .cloned()
        else {
            return;
        };
        if name.is_empty() {
            self.error = Some("Enter a snapshot name.".into());
            cx.notify();
            return;
        }
        window.close_dialog(cx);
        self.state
            .update(cx, |s, cx| s.create_snapshot(workspace, name, window, cx));
    }
}

impl Render for SnapshotDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_3()
            .child(v_flex().gap_1().child(label("Snapshot name")).child(Input::new(&self.name)))
            .child(
                v_flex()
                    .gap_1()
                    .child(label("From the database of workspace"))
                    .child(Select::new(&self.select).w_full()),
            )
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(
                "A snapshot is sealed as the next version of this name (NAME@vN) and can seed new workspaces.",
            ))
            .children(self.error.clone().map(|e| div().text_sm().text_color(cx.theme().danger).child(e)))
            .child(footer([
                cancel_button(),
                Button::new("snapshot")
                    .primary()
                    .label("Create snapshot")
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            ]))
    }
}

// ---- reset ---------------------------------------------------------------------

pub fn open_reset(state: &Entity<AppState>, window: &mut Window, cx: &mut App) {
    let (workspaces, options) = match state.read(cx).selection.current() {
        Some(model) => (
            data_workspaces(&model.workspaces),
            data_options(
                "Recorded source",
                &model.workspaces,
                &model.database.snapshots,
            ),
        ),
        None => return,
    };
    if workspaces.is_empty() {
        window.push_notification(
            gpui_kit::component::notification::Notification::info(
                "No workspace has a database to reset.",
            ),
            cx,
        );
        return;
    }
    let state = state.clone();
    let view = cx.new(|cx| {
        let items: Vec<SharedString> = options.iter().map(|o| o.label.clone().into()).collect();
        ResetDialog {
            state,
            select: workspace_select(&workspaces, window, cx),
            source: cx.new(|cx| SelectState::new(items, Some(IndexPath::new(0)), window, cx)),
            workspaces,
            options,
        }
    });
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("Reset workspace data")
            .w(px(520.))
            .child(view.clone())
    });
}

struct ResetDialog {
    state: Entity<AppState>,
    select: Entity<SelectState<Vec<SharedString>>>,
    source: Entity<SelectState<Vec<SharedString>>>,
    workspaces: Vec<Workspace>,
    options: Vec<DataOption>,
}

impl Render for ResetDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_3()
            .child(v_flex().gap_1().child(label("Workspace")).child(Select::new(&self.select).w_full()))
            .child(v_flex().gap_1().child(label("Reset to")).child(Select::new(&self.source).w_full()))
            .child(div().text_sm().text_color(cx.theme().warning).child(
                "The workspace's database is replaced by a fresh clone. Changes in it are lost; its connection URL stays the same.",
            ))
            .child(footer([
                cancel_button(),
                Button::new("reset").danger().label("Reset data").on_click(cx.listener(
                    |this, _, window, cx| {
                        let workspace = selected(&this.select, cx).and_then(|ix| this.workspaces.get(ix)).cloned();
                        let to = selected(&this.source, cx)
                            .and_then(|ix| this.options.get(ix))
                            .and_then(|o| o.source.clone());
                        let Some(workspace) = workspace else {
                            return;
                        };
                        window.close_dialog(cx);
                        this.state
                            .update(cx, |s, cx| s.reset_data(workspace, to, window, cx));
                    },
                )),
            ]))
    }
}

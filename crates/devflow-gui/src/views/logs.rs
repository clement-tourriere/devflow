//! Logs tab: the selected workspace's setup logs, newest first, read-only.
use super::ui;
use crate::{
    model::{Tab, fingerprint},
    state::AppState,
};
use gpui_kit::{
    assets::IconName as Lucide,
    component::{
        ActiveTheme as _, Selectable as _, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Editor, EditorState},
        v_flex,
    },
    prelude::*,
    *,
};
use std::path::PathBuf;

struct LogEntry {
    name: String,
    path: PathBuf,
}

pub struct LogsView {
    state: Entity<AppState>,
    content: Entity<EditorState>,
    /// Path and fingerprint of the text currently shown, to avoid resetting the
    /// editor needlessly. The length alone would stall once a log outgrows the
    /// tail window: the window keeps its size while its content moves.
    shown: Option<(PathBuf, (usize, u64))>,
    _subscription: Subscription,
}

impl LogsView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let content = cx.new(|cx| {
            EditorState::new(window, cx)
                .line_number(false)
                .soft_wrap(true)
        });
        content.update(cx, |editor, cx| editor.set_readonly(true, cx));
        let subscription = cx.observe_in(&state, window, |this, _, window, cx| {
            this.sync(window, cx);
            cx.notify();
        });
        let mut this = Self {
            state,
            content,
            shown: None,
            _subscription: subscription,
        };
        this.sync(window, cx);
        this
    }

    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (key, text) = {
            let state = self.state.read(cx);
            let Some(model) = state.selection.current() else {
                return;
            };
            // The key is taken on the borrowed text; it is copied only when it changed.
            let unreadable;
            let (path, text): (Option<&PathBuf>, &str) =
                match (&model.logs.selected, &model.logs.content) {
                    (Some(path), Some(Ok(text))) => (Some(path), text.as_str()),
                    (Some(path), Some(Err(error))) => {
                        unreadable = format!("Cannot read this log: {error}");
                        (Some(path), unreadable.as_str())
                    }
                    _ => (None, ""),
                };
            let key = path.map(|path| (path.clone(), fingerprint(text)));
            if key == self.shown {
                return;
            }
            (key, text.to_string())
        };
        self.shown = key;
        self.content.update(cx, |editor, cx| {
            editor.set_value(text, window, cx);
        });
    }
}

#[cfg(test)]
impl LogsView {
    /// The text in the viewer, for tests.
    pub(super) fn text(&self, cx: &App) -> String {
        self.content.read(cx).value().to_string()
    }
}

impl Render for LogsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (files, selected, error, has_workspace) = {
            let state = self.state.read(cx);
            let Some(model) = state.selection.current() else {
                return div().into_any_element();
            };
            let files: Vec<LogEntry> = model
                .logs
                .files
                .iter()
                .map(|f| LogEntry {
                    name: f.name.clone(),
                    path: f.path.clone(),
                })
                .collect();
            (
                files,
                model.logs.selected.clone(),
                model.logs.error.clone(),
                model.selected_workspace.is_some(),
            )
        };
        if let Some(error) = error {
            return v_flex()
                .p_4()
                .child(ui::error_banner(error, cx))
                .into_any_element();
        }
        if !has_workspace {
            return ui::loading("Loading workspaces…", cx).into_any_element();
        }
        if files.is_empty() {
            let state = self.state.clone();
            return ui::empty_state(
                Lucide::Terminal,
                "No setup logs yet",
                "Logs appear here after a workspace's setup runs hooks. Set a workspace up from the Workspaces tab.",
                Some(
                    Button::new("logs-open-workspaces")
                        .primary()
                        .label("Open Workspaces")
                        .on_click(move |_, _, cx| {
                            state.update(cx, |s, cx| s.go_to_tab(Tab::Workspaces, cx))
                        }),
                ),
            )
            .into_any_element();
        }
        let list = v_flex()
            .w(px(260.))
            .flex_shrink_0()
            .h_full()
            .p_2()
            .gap_1()
            .id("log-list")
            .overflow_y_scroll()
            .border_r_1()
            .border_color(cx.theme().border)
            .children(files.into_iter().enumerate().map(|(ix, file)| {
                let active = selected.as_ref() == Some(&file.path);
                let path = file.path.clone();
                Button::new(("log", ix))
                    .ghost()
                    .small()
                    .w_full()
                    .selected(active)
                    .label(file.name)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let path = path.clone();
                        this.state.update(cx, |s, cx| s.select_log(path, cx));
                    }))
            }));
        h_flex()
            .size_full()
            .child(list)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .p_3()
                    .child(Editor::new(&self.content).bordered(true).h_full()),
            )
            .into_any_element()
    }
}

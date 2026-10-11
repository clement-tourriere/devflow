//! Config tab: `devflow.toml` in a TOML editor, validated and saved through
//! `ConfigDocument`, which refuses to overwrite concurrent edits.
use super::ui;
use crate::{model::ConfigIdentity, state::AppState};
use gpui_kit::{
    component::{
        ActiveTheme as _, Disableable as _, IconName, Sizable as _,
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Editor, EditorState, InputEvent},
        v_flex,
    },
    prelude::*,
    *,
};

pub struct ConfigView {
    state: Entity<AppState>,
    editor: Entity<EditorState>,
    /// The document the editor's text was loaded from. Save is bound to it.
    loaded: Option<ConfigIdentity>,
    _subscriptions: Vec<Subscription>,
}

impl ConfigView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("toml")
                .line_number(true)
                .soft_wrap(false)
        });
        let subscriptions = vec![
            cx.observe_in(&state, window, |this, _, window, cx| {
                this.sync(window, cx);
                cx.notify();
            }),
            // Re-render the dirty marker and the Save button as the text changes.
            cx.subscribe(&editor, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        let mut this = Self {
            state,
            editor,
            loaded: None,
            _subscriptions: subscriptions,
        };
        this.sync(window, cx);
        this
    }

    /// Load the document into the editor when a different one is current: a
    /// reload, a save, or another project. Polling never overwrites what the
    /// user is typing, because the same document keeps the same identity.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = {
            let state = self.state.read(cx);
            state
                .selection
                .current()
                .and_then(|m| m.config.as_ref())
                .map(|c| (c.identity.clone(), c.doc.content.clone()))
        };
        let Some((identity, content)) = current else {
            return;
        };
        if self.loaded.as_ref() == Some(&identity) {
            return;
        }
        self.loaded = Some(identity);
        if self.editor.read(cx).value().as_ref() != content.as_str() {
            self.editor
                .update(cx, |editor, cx| editor.set_value(content, window, cx));
        }
    }

    /// The text in the editor, for tests.
    #[cfg(test)]
    pub(super) fn text(&self, cx: &App) -> String {
        self.editor.read(cx).value().to_string()
    }

    /// Type `text` over the editor's content, for tests.
    #[cfg(test)]
    pub(super) fn set_text(&self, text: String, window: &mut Window, cx: &mut App) {
        self.editor
            .update(cx, |editor, cx| editor.set_value(text, window, cx));
    }
}

impl Render for ConfigView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (path, saved, identity, error, busy) = {
            let state = self.state.read(cx);
            let Some(model) = state.selection.current() else {
                return div().into_any_element();
            };
            (
                model.config.as_ref().map(|c| c.doc.path.clone()),
                model.config.as_ref().map(|c| c.doc.content.clone()),
                model.config.as_ref().map(|c| c.identity.clone()),
                model.config_error.clone(),
                state.busy().is_some(),
            )
        };
        let (Some(path), Some(saved), Some(identity)) = (path, saved, identity) else {
            return match error {
                Some(e) => v_flex()
                    .p_4()
                    .child(ui::error_banner(e, cx))
                    .into_any_element(),
                None => ui::loading("Reading devflow.toml…", cx).into_any_element(),
            };
        };
        // The editor still holds another document until the next sync; never
        // show it (or let it be saved) as this one.
        if self.loaded.as_ref() != Some(&identity) {
            return ui::loading("Reading devflow.toml…", cx).into_any_element();
        }
        let text = self.editor.read(cx).value();
        let dirty = text.as_ref() != saved.as_str();
        let file = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(ui::heading(file))
                            .when(dirty, |row| {
                                row.child(ui::pill(
                                    "config-unsaved",
                                    "unsaved changes",
                                    ui::Tone::Warning,
                                    "The editor holds changes that are not saved to the file yet.",
                                ))
                            })
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(ui::muted(cx))
                                    .child(super::super::model::shorten_home(&path)),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("config-reload")
                                    .small()
                                    .icon(IconName::RefreshCw)
                                    .label("Reload")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.state.update(cx, |s, cx| s.reload_config(window, cx))
                                    })),
                            )
                            .child(
                                Button::new("config-validate")
                                    .small()
                                    .icon(IconName::Check)
                                    .label("Validate")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let text = this.editor.read(cx).value();
                                        this.state.update(cx, |s, cx| s.validate_config(&text, window, cx));
                                    })),
                            )
                            .child(
                                Button::new("config-save")
                                    .small()
                                    .primary()
                                    .label("Save")
                                    .disabled(!dirty || busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let text = this.editor.read(cx).value().to_string();
                                        let Some(loaded) = this.loaded.clone() else {
                                            return;
                                        };
                                        this.state
                                            .update(cx, |s, cx| s.save_config(text, loaded, window, cx));
                                    })),
                            ),
                    ),
            )
            .child(div().flex_1().min_h_0().child(Editor::new(&self.editor).bordered(true).h_full()))
            .child(div().text_xs().text_color(cx.theme().muted_foreground).child(
                "Machine-local overrides belong in devflow.local.toml. Saving is refused if the file changed on disk.",
            ))
            .into_any_element()
    }
}

//! Settings page: editor and terminal commands, theme. Saved to gui.toml.
use super::ui;
use crate::{
    settings::{self, Settings, ThemeChoice},
    state::AppState,
};
use gpui_kit::{
    component::{
        IndexPath, WindowExt as _,
        button::{Button, ButtonVariants as _},
        form::{Field, Form},
        input::{Input, InputState},
        notification::Notification,
        select::{Select, SelectEvent, SelectState},
        v_flex,
    },
    prelude::*,
    *,
};

pub struct SettingsView {
    state: Entity<AppState>,
    editor: Entity<InputState>,
    terminal: Entity<InputState>,
    theme: Entity<SelectState<Vec<SharedString>>>,
    _subscription: Subscription,
}

impl SettingsView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let current = state.read(cx).settings.clone();
        let editor = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("code")
                .default_value(current.editor_command.clone())
        });
        let terminal = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("open -a Terminal")
                .default_value(current.terminal_command.clone())
        });
        let themes: Vec<SharedString> = ThemeChoice::ALL.iter().map(|t| t.label().into()).collect();
        let selected = ThemeChoice::ALL
            .iter()
            .position(|t| *t == current.theme)
            .unwrap_or(0);
        let theme =
            cx.new(|cx| SelectState::new(themes, Some(IndexPath::new(selected)), window, cx));
        // Preview the theme as soon as it is chosen; Save persists it.
        let subscription = cx.subscribe_in(
            &theme,
            window,
            |_, _, event: &SelectEvent<Vec<SharedString>>, window, cx| {
                if let SelectEvent::Confirm(Some(label)) = event
                    && let Some(choice) = ThemeChoice::ALL
                        .iter()
                        .find(|t| t.label() == label.as_ref())
                {
                    choice.apply(Some(window), cx);
                }
            },
        );
        Self {
            state,
            editor,
            terminal,
            theme,
            _subscription: subscription,
        }
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let theme = self
            .theme
            .read(cx)
            .selected_index(cx)
            .and_then(|ix| ThemeChoice::ALL.get(ix.row).copied())
            .unwrap_or_default();
        let settings = Settings {
            editor_command: self.editor.read(cx).value().trim().to_string(),
            terminal_command: self.terminal.read(cx).value().trim().to_string(),
            theme,
        };
        match settings::save(&settings) {
            Ok(()) => {
                self.state.update(cx, |s, cx| {
                    s.settings = settings;
                    cx.notify();
                });
                window.push_notification(Notification::success("Settings saved."), cx);
            }
            Err(e) => window.push_notification(
                Notification::error(format!("{e:#}"))
                    .title("Cannot save settings")
                    .autohide(false),
                cx,
            ),
        }
    }
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let path = settings::path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        v_flex()
            .size_full()
            .p_6()
            .gap_4()
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child("Settings"))
            .child(
                div()
                    .max_w(px(560.))
                    .child(
                        Form::new()
                            .child(
                                Field::new()
                                    .label("Editor command")
                                    .description("Run with the workspace path appended, or substituted for {path}.")
                                    .child(Input::new(&self.editor)),
                            )
                            .child(
                                Field::new()
                                    .label("Terminal command")
                                    .description("Started in the workspace directory. Apps launched from the Dock have a short PATH; use a full path if a command is not found.")
                                    .child(Input::new(&self.terminal)),
                            )
                            .child(Field::new().label("Theme").child(Select::new(&self.theme).w_full()))
                            .footer(
                                Button::new("save-settings")
                                    .primary()
                                    .label("Save")
                                    .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                            ),
                    ),
            )
            .child(div().text_xs().text_color(ui::muted(cx)).child(format!("Stored in {path}")))
    }
}

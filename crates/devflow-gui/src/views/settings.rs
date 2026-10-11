//! Settings page: editor and terminal commands, theme. Saved to gui.toml. The
//! theme applies and is saved as soon as it is chosen, as with the title bar's
//! switch; the page follows when the switch changes it.
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
        switch::Switch,
        v_flex,
    },
    prelude::*,
    *,
};

pub struct SettingsView {
    state: Entity<AppState>,
    editor: Entity<InputState>,
    terminal: Entity<InputState>,
    pub(super) theme: Entity<SelectState<Vec<SharedString>>>,
    _subscriptions: Vec<Subscription>,
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
        let subscriptions = vec![
            // Applied and saved as soon as it is chosen.
            cx.subscribe_in(
                &theme,
                window,
                |this, _, event: &SelectEvent<Vec<SharedString>>, window, cx| {
                    if let SelectEvent::Confirm(Some(label)) = event
                        && let Some(&choice) = ThemeChoice::ALL
                            .iter()
                            .find(|t| t.label() == label.as_ref())
                    {
                        this.state
                            .update(cx, |s, cx| s.set_theme(choice, window, cx));
                    }
                },
            ),
            // The title bar's switch (or the palette) changed it.
            cx.observe_in(&state, window, |this, _, window, cx| {
                this.show_stored_theme(window, cx)
            }),
        ];
        Self {
            state,
            editor,
            terminal,
            theme,
            _subscriptions: subscriptions,
        }
    }

    fn show_stored_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let stored = self.state.read(cx).settings.theme;
        let shown = self
            .theme
            .read(cx)
            .selected_index(cx)
            .and_then(|ix| ThemeChoice::ALL.get(ix.row).copied());
        if shown != Some(stored)
            && let Some(row) = ThemeChoice::ALL.iter().position(|t| *t == stored)
        {
            self.theme.update(cx, |t, cx| {
                t.set_selected_index(Some(IndexPath::new(row)), window, cx)
            });
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
            // Applied and saved as soon as it is switched; not part of the form.
            ..self.state.read(cx).settings.clone()
        };
        let stored = self.state.update(cx, |s, cx| {
            let stored = s.store_settings(settings);
            cx.notify();
            stored
        });
        match stored {
            Ok(()) => {
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
        let keep_in_menu_bar = self.state.read(cx).settings.keep_in_menu_bar;
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
                            .child(
                                Field::new()
                                    .label("Theme")
                                    .description("Applied and saved when chosen. The title bar switches between light and dark; System follows the OS.")
                                    .child(Select::new(&self.theme).w_full()),
                            )
                            .children(cfg!(target_os = "macos").then(|| {
                                Field::new()
                                    .label("Menu bar")
                                    .description("Closing the window (or ⌘W) hides devflow to the menu bar instead of quitting, and the Dock icon goes with it. Quit from the menu bar item or with ⌘Q.")
                                    .child(
                                        Switch::new("keep-in-menu-bar")
                                            .label("Keep devflow running in the menu bar when the window is closed")
                                            .checked(keep_in_menu_bar)
                                            .on_click(cx.listener(|this, value: &bool, window, cx| {
                                                this.state.update(cx, |s, cx| {
                                                    s.set_keep_in_menu_bar(*value, window, cx)
                                                });
                                            })),
                                    )
                            }))
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

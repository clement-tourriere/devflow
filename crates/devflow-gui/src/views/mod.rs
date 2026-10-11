//! The window: title bar, sidebar, and either a project or the settings page.
mod activity_panel;
mod config;
mod daemons;
mod data;
mod database;
mod db_dialogs;
mod dialogs;
mod doctor;
#[cfg(test)]
mod dx_tests;
mod hooks_editor;
mod logs;
mod onboarding;
mod overview;
mod palette;
mod pitchfork_panel;
mod problems;
#[cfg(test)]
mod problems_tests;
mod processes;
mod project;
mod reading;
pub(crate) mod remedies;
pub(crate) mod service_dialogs;
mod service_view;
mod settings;
#[cfg(test)]
mod setup_tests;
mod sidebar;
mod titlebar;
#[cfg(test)]
mod tray_tests;
mod ui;
#[cfg(test)]
mod ui_tests;
mod widgets;
mod workspace_detail;
mod workspaces;

use crate::{
    actions::ShellHandle,
    model::{Page, Tab},
    state::{AppEvent, AppState},
};
use gpui_kit::{
    component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex},
    prelude::*,
    *,
};

/// The cards of the Data tab for a project.
pub use data::cards as data_cards;
#[cfg(test)]
pub use service_view::{branch_facts, branch_label};

/// Register the setup wizard's key bindings.
pub use onboarding::init as init_wizard;

pub struct Shell {
    state: Entity<AppState>,
    project: Entity<project::ProjectView>,
    settings: Entity<settings::SettingsView>,
    _subscriptions: Vec<Subscription>,
}

impl Shell {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = cx.new(|cx| project::ProjectView::new(state.clone(), window, cx));
        let settings = cx.new(|cx| settings::SettingsView::new(state.clone(), window, cx));
        // The shortcuts reach this window through its handle.
        cx.set_global(ShellHandle::new(window, cx.weak_entity()));
        let subscriptions = vec![
            cx.observe(&state, |_, _, cx| cx.notify()),
            // With the setting on System, the theme follows the OS live.
            window.observe_window_appearance({
                let state = state.clone();
                move |window, cx| {
                    state.update(cx, |s, cx| s.follow_system_appearance(window, cx));
                }
            }),
            cx.subscribe_in(
                &state,
                window,
                |_, state, event: &AppEvent, window, cx| match event {
                    AppEvent::OfferInitialize(root) => {
                        onboarding::open(state, root.clone(), window, cx)
                    }
                    AppEvent::OpenNewWorkspace => dialogs::open_new_workspace(state, window, cx),
                    AppEvent::OfferForceRemove {
                        target,
                        keep_data,
                        delete_branch,
                    } => dialogs::open_remove_forced(
                        state,
                        target.clone(),
                        *keep_data,
                        *delete_branch,
                        window,
                        cx,
                    ),
                },
            ),
        ];
        Self {
            state,
            project,
            settings,
            _subscriptions: subscriptions,
        }
    }
}

/// What the shortcuts do. A dialog is modal: while one is open they leave the
/// page behind it alone (switching the theme is harmless and still works).
impl Shell {
    fn page_is_covered(window: &mut Window, cx: &mut App) -> bool {
        window.has_active_dialog(cx)
    }

    pub fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !Self::page_is_covered(window, cx) {
            palette::open(&self.state, window, cx);
        }
    }

    pub fn new_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !Self::page_is_covered(window, cx) {
            self.state
                .update(cx, |s, cx| s.request_new_workspace(None, cx));
        }
    }

    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !Self::page_is_covered(window, cx) {
            self.state.update(cx, |s, cx| s.refresh_all(cx));
        }
    }

    pub fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !Self::page_is_covered(window, cx) {
            self.state
                .update(cx, |s, cx| s.set_page(Page::Settings, cx));
        }
    }

    /// ⌘W: hide the window to the menu bar, or close it (and quit).
    pub fn close_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        crate::tray::close_window(window, cx);
    }

    pub fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |s, cx| s.toggle_theme(window, cx));
    }

    pub fn go_to_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        if !Self::page_is_covered(window, cx) {
            self.state.update(cx, |s, cx| s.go_to_tab(tab, cx));
        }
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = self.state.read(cx).page;
        let content = match page {
            Page::Project => self.project.clone().into_any_element(),
            Page::Settings => self.settings.clone().into_any_element(),
        };
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(titlebar::render(&self.state, window, cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(sidebar::render(&self.state, cx))
                    .child(div().flex_1().min_w_0().h_full().child(content)),
            )
    }
}

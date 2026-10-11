//! The window: sidebar plus either a project or the settings page.
mod config;
mod daemons;
mod database;
mod db_dialogs;
mod dialogs;
mod logs;
mod project;
mod settings;
mod sidebar;
mod ui;
#[cfg(test)]
mod ui_tests;
mod workspaces;

use crate::{
    model::Page,
    state::{AppEvent, AppState},
};
use gpui_kit::{
    component::{ActiveTheme as _, h_flex},
    prelude::*,
    *,
};

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
        let subscriptions = vec![
            cx.observe(&state, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                &state,
                window,
                |_, state, event: &AppEvent, window, cx| match event {
                    AppEvent::OfferInitialize(root) => {
                        dialogs::open_initialize(state, root.clone(), window, cx)
                    }
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

impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = self.state.read(cx).page;
        let content = match page {
            Page::Project => self.project.clone().into_any_element(),
            Page::Settings => self.settings.clone().into_any_element(),
        };
        h_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(sidebar::render(&self.state, cx))
            .child(div().flex_1().min_w_0().h_full().child(content))
    }
}

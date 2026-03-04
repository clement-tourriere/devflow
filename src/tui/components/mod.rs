pub mod capabilities;
pub mod config_view;
pub mod doctor;
pub mod help;
pub mod hooks;
pub mod logs;
pub mod proxy_tab;
pub mod services_tab;
pub mod system;
pub mod workspaces;

use super::action::Action;
use crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::Frame;

/// Trait for TUI components. Each tab/panel implements this.
pub trait Component {
    /// Handle a key event. Return an action if the event was consumed.
    fn handle_key_event(&mut self, key: KeyEvent) -> Action {
        let _ = key;
        Action::None
    }

    /// Process an action dispatched by the App.
    fn update(&mut self, action: &Action) {
        let _ = action;
    }

    /// Render the component into the given area.
    /// `spinner` is the current spinner animation frame for loading indicators.
    fn render(&self, frame: &mut Frame, area: Rect, spinner: &str);

    /// Called when this component gains focus.
    fn on_focus(&mut self) {}

    /// Called when this component loses focus.
    fn on_blur(&mut self) {}
}

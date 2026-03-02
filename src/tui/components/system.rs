use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Tabs},
    Frame,
};

use super::capabilities::CapabilitiesComponent;
use super::config_view::ConfigViewComponent;
use super::doctor::DoctorComponent;
use super::hooks::HooksComponent;
use super::Component;
use crate::tui::action::*;
use crate::tui::theme;

/// Sub-sections within the System tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubSection {
    Config = 0,
    Hooks = 1,
    Doctor = 2,
    Capabilities = 3,
}

impl SubSection {
    fn from_index(i: usize) -> Self {
        match i {
            0 => SubSection::Config,
            1 => SubSection::Hooks,
            2 => SubSection::Doctor,
            3 => SubSection::Capabilities,
            _ => SubSection::Config,
        }
    }

    fn title(&self) -> &'static str {
        match self {
            SubSection::Config => "Config",
            SubSection::Hooks => "Hooks",
            SubSection::Doctor => "Doctor",
            SubSection::Capabilities => "Capabilities",
        }
    }
}

const SUB_SECTIONS: [SubSection; 4] = [
    SubSection::Config,
    SubSection::Hooks,
    SubSection::Doctor,
    SubSection::Capabilities,
];

/// The System tab consolidates Config, Hooks, Doctor, and Capabilities views.
/// Users switch sub-sections with 1/2/3/4 keys (or left/right arrows).
pub struct SystemComponent {
    active_section: SubSection,
    config_view: ConfigViewComponent,
    hooks_view: HooksComponent,
    doctor: DoctorComponent,
    capabilities: CapabilitiesComponent,
}

impl SystemComponent {
    pub fn new() -> Self {
        Self {
            active_section: SubSection::Config,
            config_view: ConfigViewComponent::new(),
            hooks_view: HooksComponent::new(),
            doctor: DoctorComponent::new(),
            capabilities: CapabilitiesComponent::new(),
        }
    }

    fn switch_section(&mut self, section: SubSection) {
        if section == self.active_section {
            return;
        }
        // Blur old
        match self.active_section {
            SubSection::Config => self.config_view.on_blur(),
            SubSection::Hooks => self.hooks_view.on_blur(),
            SubSection::Doctor => self.doctor.on_blur(),
            SubSection::Capabilities => self.capabilities.on_blur(),
        }
        self.active_section = section;
        // Focus new
        match self.active_section {
            SubSection::Config => self.config_view.on_focus(),
            SubSection::Hooks => self.hooks_view.on_focus(),
            SubSection::Doctor => self.doctor.on_focus(),
            SubSection::Capabilities => self.capabilities.on_focus(),
        }
    }

    fn render_section_tabs(&self, frame: &mut Frame, area: Rect) {
        let titles: Vec<Line> = SUB_SECTIONS
            .iter()
            .enumerate()
            .map(|(i, section)| {
                let is_active = *section == self.active_section;
                let style = if is_active {
                    Style::default().fg(theme::SUBSECTION_ACTIVE).bold()
                } else {
                    Style::default().fg(theme::SUBSECTION_INACTIVE)
                };
                Line::styled(format!(" {} {} ", i + 1, section.title()), style)
            })
            .collect();

        let tabs = Tabs::new(titles)
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(theme::BORDER_INACTIVE)),
            )
            .select(self.active_section as usize)
            .highlight_style(Style::default().fg(theme::SUBSECTION_ACTIVE).bold())
            .divider(Span::styled(
                " | ",
                Style::default().fg(theme::SUBSECTION_INACTIVE),
            ));

        frame.render_widget(tabs, area);
    }
}

impl Component for SystemComponent {
    fn title(&self) -> &str {
        "System"
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Action {
        // Section switching keys
        match key.code {
            KeyCode::Char('1') => {
                self.switch_section(SubSection::Config);
                return Action::None;
            }
            KeyCode::Char('2') => {
                self.switch_section(SubSection::Hooks);
                return Action::None;
            }
            KeyCode::Char('3') => {
                self.switch_section(SubSection::Doctor);
                return Action::None;
            }
            KeyCode::Char('4') => {
                self.switch_section(SubSection::Capabilities);
                return Action::None;
            }
            KeyCode::Left | KeyCode::Char('h') => {
                let idx = self.active_section as usize;
                if idx > 0 {
                    self.switch_section(SubSection::from_index(idx - 1));
                }
                return Action::None;
            }
            KeyCode::Right | KeyCode::Char('l') => {
                let idx = self.active_section as usize;
                if idx < SUB_SECTIONS.len() - 1 {
                    self.switch_section(SubSection::from_index(idx + 1));
                }
                return Action::None;
            }
            _ => {}
        }

        // Delegate to active sub-component
        match self.active_section {
            SubSection::Config => self.config_view.handle_key_event(key),
            SubSection::Hooks => self.hooks_view.handle_key_event(key),
            SubSection::Doctor => self.doctor.handle_key_event(key),
            SubSection::Capabilities => self.capabilities.handle_key_event(key),
        }
    }

    fn update(&mut self, action: &Action) {
        // Forward to all sub-components so they stay in sync
        self.config_view.update(action);
        self.hooks_view.update(action);
        self.doctor.update(action);
        self.capabilities.update(action);
    }

    fn render(&self, frame: &mut Frame, area: Rect, spinner: &str) {
        // Layout: section tabs (2 lines) + content
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(3)])
            .split(area);

        self.render_section_tabs(frame, chunks[0]);

        // Render active sub-component
        match self.active_section {
            SubSection::Config => self.config_view.render(frame, chunks[1], spinner),
            SubSection::Hooks => self.hooks_view.render(frame, chunks[1], spinner),
            SubSection::Doctor => self.doctor.render(frame, chunks[1], spinner),
            SubSection::Capabilities => self.capabilities.render(frame, chunks[1], spinner),
        }
    }

    fn on_focus(&mut self) {
        match self.active_section {
            SubSection::Config => self.config_view.on_focus(),
            SubSection::Hooks => self.hooks_view.on_focus(),
            SubSection::Doctor => self.doctor.on_focus(),
            SubSection::Capabilities => self.capabilities.on_focus(),
        }
    }

    fn on_blur(&mut self) {
        match self.active_section {
            SubSection::Config => self.config_view.on_blur(),
            SubSection::Hooks => self.hooks_view.on_blur(),
            SubSection::Doctor => self.doctor.on_blur(),
            SubSection::Capabilities => self.capabilities.on_blur(),
        }
    }
}

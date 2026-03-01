use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
    Frame,
};

use super::Component;
use crate::tui::action::*;
use crate::tui::theme;

pub struct ConfigViewComponent {
    yaml_content: Option<String>,
    scroll_offset: u16,
    content_height: u16,
    loading: bool,
}

impl ConfigViewComponent {
    pub fn new() -> Self {
        Self {
            yaml_content: None,
            scroll_offset: 0,
            content_height: 0,
            loading: true,
        }
    }

    pub fn set_data(&mut self, yaml: String) {
        self.content_height = yaml.lines().count() as u16;
        self.yaml_content = Some(yaml);
        self.loading = false;
    }

    fn render_yaml(&self, frame: &mut Frame, area: Rect) {
        let content = self.yaml_content.as_deref().unwrap_or("");

        let lines: Vec<Line> = content
            .lines()
            .map(|line| {
                // Simple YAML syntax highlighting
                if line.trim_start().starts_with('#') {
                    Line::styled(line, Style::default().fg(theme::YAML_COMMENT))
                } else if line.contains(':') {
                    let parts: Vec<&str> = line.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        Line::from(vec![
                            Span::styled(parts[0], Style::default().fg(theme::YAML_KEY)),
                            Span::styled(":", Style::default().fg(theme::TEXT_PRIMARY)),
                            Span::styled(parts[1], Style::default().fg(theme::YAML_VALUE)),
                        ])
                    } else {
                        Line::styled(line, Style::default().fg(theme::TEXT_PRIMARY))
                    }
                } else if line.trim_start().starts_with('-') {
                    Line::styled(line, Style::default().fg(theme::YAML_LIST))
                } else {
                    Line::styled(line, Style::default().fg(theme::TEXT_PRIMARY))
                }
            })
            .collect();

        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::BORDER_ACTIVE))
                    .title(" Effective Configuration (read-only) "),
            )
            .scroll((self.scroll_offset, 0))
            .wrap(Wrap { trim: false });

        frame.render_widget(paragraph, area);

        // Scrollbar
        let visible_height = area.height.saturating_sub(2); // borders
        if self.content_height > visible_height {
            let mut scrollbar_state = ScrollbarState::new(self.content_height as usize)
                .position(self.scroll_offset as usize)
                .viewport_content_length(visible_height as usize);

            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("^"))
                    .end_symbol(Some("v")),
                area,
                &mut scrollbar_state,
            );
        }
    }
}

impl Component for ConfigViewComponent {
    fn title(&self) -> &str {
        "Config"
    }

    fn handle_key_event(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.scroll_offset < self.content_height.saturating_sub(1) {
                    self.scroll_offset += 1;
                }
                Action::None
            }
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_sub(20);
                Action::None
            }
            KeyCode::PageDown => {
                self.scroll_offset =
                    (self.scroll_offset + 20).min(self.content_height.saturating_sub(1));
                Action::None
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.scroll_offset = 0;
                Action::None
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.scroll_offset = self.content_height.saturating_sub(1);
                Action::None
            }
            KeyCode::Char('r') => Action::Refresh,
            _ => Action::None,
        }
    }

    fn update(&mut self, action: &Action) {
        if let Action::DataLoaded(DataPayload::ConfigYaml(yaml)) = action {
            self.set_data(yaml.clone());
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, spinner: &str) {
        if self.loading {
            let loading = Paragraph::new(format!(" {} Loading configuration...", spinner))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(theme::BORDER_ACTIVE))
                        .title(" Config "),
                )
                .style(Style::default().fg(theme::TEXT_MUTED));
            frame.render_widget(loading, area);
            return;
        }

        self.render_yaml(frame, area);
    }
}

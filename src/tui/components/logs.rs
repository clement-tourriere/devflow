use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};

use super::Component;
use crate::tui::action::*;
use crate::tui::theme;

/// Focus area within the logs tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogsFocus {
    /// Left sidebar: service/workspace picker
    Picker,
    /// Right panel: log content
    Content,
}

/// An entry in the service/workspace picker sidebar.
#[derive(Debug, Clone)]
struct PickerEntry {
    service_name: String,
    workspace_name: String,
    state: Option<String>,
}

pub struct LogsComponent {
    // Currently viewed log
    service_name: String,
    workspace_name: String,
    content: String,
    scroll_offset: u16,
    content_height: u16,
    loading: bool,
    // Service/workspace picker
    all_picker_entries: Vec<PickerEntry>,
    picker_entries: Vec<PickerEntry>,
    picker_state: ListState,
    picker_selected: usize,
    picker_filter: String,
    focus: LogsFocus,
}

impl LogsComponent {
    pub fn new() -> Self {
        let mut picker_state = ListState::default();
        picker_state.select(Some(0));
        Self {
            service_name: String::new(),
            workspace_name: String::new(),
            content: String::new(),
            scroll_offset: 0,
            content_height: 0,
            loading: false,
            all_picker_entries: Vec::new(),
            picker_entries: Vec::new(),
            picker_state,
            picker_selected: 0,
            picker_filter: String::new(),
            focus: LogsFocus::Picker,
        }
    }

    pub fn set_data(&mut self, service: String, content: String) {
        self.service_name = service;
        self.content_height = content.lines().count() as u16;
        // Auto-scroll to bottom
        self.scroll_offset = self.content_height.saturating_sub(20);
        self.content = content;
        self.loading = false;
        // Switch focus to content after loading
        self.focus = LogsFocus::Content;
    }

    pub fn set_loading(&mut self, service: &str, workspace: &str) {
        self.service_name = service.to_string();
        self.workspace_name = workspace.to_string();
        self.loading = true;
    }

    /// Build picker entries from services data (all service workspaces across all services).
    fn update_picker(&mut self, services: &ServicesData) {
        self.all_picker_entries.clear();
        for svc in &services.services {
            for workspace in &svc.workspaces {
                self.all_picker_entries.push(PickerEntry {
                    service_name: svc.name.clone(),
                    workspace_name: workspace.name.clone(),
                    state: workspace.state.clone(),
                });
            }
        }
        self.apply_picker_filter();
    }

    fn apply_picker_filter(&mut self) {
        if self.picker_filter.is_empty() {
            self.picker_entries = self.all_picker_entries.clone();
        } else {
            let needle = self.picker_filter.to_lowercase();
            self.picker_entries = self
                .all_picker_entries
                .iter()
                .filter(|entry| {
                    entry.service_name.to_lowercase().contains(&needle)
                        || entry.workspace_name.to_lowercase().contains(&needle)
                })
                .cloned()
                .collect();
        }

        // Clamp selection
        if self.picker_selected >= self.picker_entries.len() {
            self.picker_selected = self.picker_entries.len().saturating_sub(1);
        }
        self.picker_state.select(Some(self.picker_selected));
    }

    fn move_picker(&mut self, delta: i32) {
        if self.picker_entries.is_empty() {
            return;
        }
        let len = self.picker_entries.len() as i32;
        self.picker_selected = ((self.picker_selected as i32 + delta).rem_euclid(len)) as usize;
        self.picker_state.select(Some(self.picker_selected));
    }

    fn render_picker(&self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .picker_entries
            .iter()
            .map(|entry| {
                let state_str = entry.state.as_deref().unwrap_or("?");
                let color = theme::state_color(state_str);
                ListItem::new(Line::from(vec![
                    Span::styled(
                        entry.service_name.to_string(),
                        Style::default().fg(theme::SERVICE_TYPE),
                    ),
                    Span::styled(" / ", Style::default().fg(theme::TEXT_MUTED)),
                    Span::styled(
                        &entry.workspace_name,
                        Style::default().fg(theme::TEXT_PRIMARY),
                    ),
                    Span::styled(format!(" [{}]", state_str), Style::default().fg(color)),
                ]))
            })
            .collect();

        let border_color = if self.focus == LogsFocus::Picker {
            theme::BORDER_ACTIVE
        } else {
            theme::BORDER_INACTIVE
        };

        let title = if self.picker_filter.is_empty() {
            format!(" Services ({}) ", self.picker_entries.len())
        } else {
            format!(
                " Services ({}) [filter: {}] ",
                self.picker_entries.len(),
                self.picker_filter
            )
        };

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color))
                    .title(title),
            )
            .highlight_style(theme::highlight_style())
            .highlight_symbol(">> ");

        frame.render_stateful_widget(list, area, &mut self.picker_state.clone());

        // Scrollbar
        let visible_height = area.height.saturating_sub(2) as usize;
        if self.picker_entries.len() > visible_height {
            let mut scrollbar_state = ScrollbarState::new(self.picker_entries.len())
                .position(self.picker_selected)
                .viewport_content_length(visible_height);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("^"))
                    .end_symbol(Some("v")),
                area,
                &mut scrollbar_state,
            );
        }
    }

    fn render_log_content(&self, frame: &mut Frame, area: Rect, spinner: &str) {
        let border_color = if self.focus == LogsFocus::Content {
            theme::BORDER_ACTIVE
        } else {
            theme::BORDER_INACTIVE
        };

        if self.loading {
            let loading = Paragraph::new(format!(
                " {} Loading logs for {} on {}...",
                spinner, self.service_name, self.workspace_name
            ))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color))
                    .title(" Logs "),
            )
            .style(Style::default().fg(theme::TEXT_MUTED));
            frame.render_widget(loading, area);
            return;
        }

        if self.content.is_empty() {
            let empty = Paragraph::new(
                " Select a service/workspace and press Enter to view logs.\n Press f to switch between picker and log content.",
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color))
                    .title(" Logs "),
            )
            .style(Style::default().fg(theme::TEXT_MUTED));
            frame.render_widget(empty, area);
            return;
        }

        let lines: Vec<Line> = self
            .content
            .lines()
            .map(|line| {
                if line.contains("ERROR") || line.contains("FATAL") {
                    Line::styled(line, Style::default().fg(theme::LOG_ERROR))
                } else if line.contains("WARN") {
                    Line::styled(line, Style::default().fg(theme::LOG_WARN))
                } else if line.contains("INFO") {
                    Line::styled(line, Style::default().fg(theme::LOG_INFO))
                } else if line.contains("DEBUG") {
                    Line::styled(line, Style::default().fg(theme::LOG_DEBUG))
                } else {
                    Line::styled(line, Style::default().fg(theme::TEXT_PRIMARY))
                }
            })
            .collect();

        let title = format!(
            " Logs: {} / {} ({} lines) ",
            self.service_name, self.workspace_name, self.content_height
        );

        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color))
                    .title(title),
            )
            .scroll((self.scroll_offset, 0));

        frame.render_widget(paragraph, area);

        // Scrollbar
        let visible_height = area.height.saturating_sub(2);
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

impl Component for LogsComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Action {
        match key.code {
            // Focus toggle between picker and content
            KeyCode::Char('f') | KeyCode::Char('F') => {
                self.focus = match self.focus {
                    LogsFocus::Picker => LogsFocus::Content,
                    LogsFocus::Content => LogsFocus::Picker,
                };
                return Action::None;
            }
            _ => {}
        }

        match self.focus {
            LogsFocus::Picker => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.move_picker(-1);
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.move_picker(1);
                    Action::None
                }
                KeyCode::Char('/') => Action::ShowInput {
                    title: "Filter logs picker".to_string(),
                    on_submit: InputTarget::FilterLogsPicker,
                },
                KeyCode::Esc => {
                    if !self.picker_filter.is_empty() {
                        self.picker_filter.clear();
                        self.apply_picker_filter();
                    }
                    Action::None
                }
                KeyCode::Enter => {
                    if let Some(entry) = self.picker_entries.get(self.picker_selected) {
                        Action::ViewLogs {
                            service: entry.service_name.clone(),
                            workspace: entry.workspace_name.clone(),
                        }
                    } else {
                        Action::None
                    }
                }
                KeyCode::Char('r') => Action::Refresh,
                _ => Action::None,
            },
            LogsFocus::Content => match key.code {
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
                KeyCode::Char('r') => {
                    if !self.service_name.is_empty() && !self.workspace_name.is_empty() {
                        Action::ViewLogs {
                            service: self.service_name.clone(),
                            workspace: self.workspace_name.clone(),
                        }
                    } else {
                        Action::Refresh
                    }
                }
                _ => Action::None,
            },
        }
    }

    fn update(&mut self, action: &Action) {
        match action {
            Action::DataLoaded(DataPayload::Logs { service, content }) => {
                self.set_data(service.clone(), content.clone());
            }
            Action::DataLoaded(DataPayload::Services(data)) => {
                self.update_picker(data);
            }
            _ => {}
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, spinner: &str) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(area);

        self.render_picker(frame, chunks[0]);
        self.render_log_content(frame, chunks[1], spinner);
    }
}

impl LogsComponent {
    pub fn set_filter(&mut self, filter: String) {
        self.picker_filter = filter;
        self.apply_picker_filter();
    }
}

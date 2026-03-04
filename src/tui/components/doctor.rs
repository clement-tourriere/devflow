use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
    Frame,
};

use super::Component;
use crate::tui::action::*;
use crate::tui::theme;

pub struct DoctorComponent {
    data: Option<Vec<DoctorEntry>>,
    list_state: ListState,
    selected_service: usize,
    loading: bool,
}

impl DoctorComponent {
    pub fn new() -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            data: None,
            list_state,
            selected_service: 0,
            loading: true,
        }
    }

    pub fn set_data(&mut self, data: Vec<DoctorEntry>) {
        self.data = Some(data);
        self.loading = false;
    }

    fn current_entry(&self) -> Option<&DoctorEntry> {
        self.data
            .as_ref()
            .and_then(|d| d.get(self.selected_service))
    }

    fn move_selection(&mut self, delta: i32) {
        if let Some(ref data) = self.data {
            if data.is_empty() {
                return;
            }
            let len = data.len() as i32;
            self.selected_service =
                ((self.selected_service as i32 + delta).rem_euclid(len)) as usize;
            self.list_state.select(Some(self.selected_service));
        }
    }
}

impl Component for DoctorComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                Action::None
            }
            KeyCode::Char('r') | KeyCode::Char('D') => Action::RunDoctor,
            _ => Action::None,
        }
    }

    fn update(&mut self, action: &Action) {
        if let Action::DataLoaded(DataPayload::DoctorResults(data)) = action {
            self.set_data(data.clone());
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, spinner: &str) {
        if self.loading {
            let loading = Paragraph::new(format!(
                " {} Press 'D' or 'r' to run doctor checks...",
                spinner
            ))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::BORDER_ACTIVE))
                    .title(" Doctor "),
            )
            .style(Style::default().fg(theme::TEXT_MUTED));
            frame.render_widget(loading, area);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(area);

        // Service list
        let entries = self.data.as_deref().unwrap_or(&[]);
        let items: Vec<ListItem> = entries
            .iter()
            .map(|entry| {
                let all_ok = entry.checks.iter().all(|c| c.available);
                let icon = if all_ok { "+" } else { "!" };
                let color = if all_ok {
                    theme::CHECK_PASS
                } else {
                    theme::CHECK_FAIL
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("[{}] ", icon), Style::default().fg(color)),
                    Span::styled(
                        &entry.service_name,
                        Style::default().fg(theme::TEXT_PRIMARY),
                    ),
                ]))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::BORDER_ACTIVE))
                    .title(" Services "),
            )
            .highlight_style(theme::highlight_style())
            .highlight_symbol(">> ");

        frame.render_stateful_widget(list, chunks[0], &mut self.list_state.clone());

        // Scrollbar for service list
        if entries.len() as u16 > chunks[0].height.saturating_sub(2) {
            let mut scrollbar_state = ScrollbarState::new(entries.len())
                .position(self.selected_service)
                .viewport_content_length(chunks[0].height.saturating_sub(2) as usize);

            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("^"))
                    .end_symbol(Some("v")),
                chunks[0],
                &mut scrollbar_state,
            );
        }

        // Detail panel
        let mut lines = Vec::new();

        if let Some(entry) = self.current_entry() {
            lines.push(Line::from(vec![
                Span::styled("Service: ", Style::default().fg(theme::TEXT_SECONDARY)),
                Span::styled(
                    &entry.service_name,
                    Style::default().fg(theme::TEXT_PRIMARY).bold(),
                ),
            ]));
            lines.push(Line::raw(""));

            for check in &entry.checks {
                let (icon, color) = if check.available {
                    ("+", theme::CHECK_PASS)
                } else {
                    ("x", theme::CHECK_FAIL)
                };

                lines.push(Line::from(vec![
                    Span::styled(format!("  [{}] ", icon), Style::default().fg(color)),
                    Span::styled(&check.name, Style::default().fg(theme::TEXT_PRIMARY)),
                ]));
                lines.push(Line::from(vec![
                    Span::raw("      "),
                    Span::styled(&check.detail, Style::default().fg(theme::TEXT_SECONDARY)),
                ]));
            }

            lines.push(Line::raw(""));
            let total = entry.checks.len();
            let passed = entry.checks.iter().filter(|c| c.available).count();
            let summary_color = if passed == total {
                theme::CHECK_PASS
            } else {
                theme::LOG_WARN
            };
            lines.push(Line::styled(
                format!("  {}/{} checks passed", passed, total),
                Style::default().fg(summary_color),
            ));
        } else {
            lines.push(Line::styled(
                "No doctor results available",
                Style::default().fg(theme::TEXT_MUTED),
            ));
        }

        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("  D/r", Style::default().fg(theme::KEY_HINT)),
            Span::raw("  Re-run doctor checks"),
        ]));

        let detail = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::BORDER_ACTIVE))
                    .title(" Check Results "),
            )
            .wrap(Wrap { trim: false });

        frame.render_widget(detail, chunks[1]);
    }
}

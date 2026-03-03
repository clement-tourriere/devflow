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

pub struct CapabilitiesComponent {
    data: Option<CapabilitiesData>,
    list_state: ListState,
    selected_service: usize,
    loading: bool,
}

impl CapabilitiesComponent {
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

    pub fn set_data(&mut self, data: CapabilitiesData) {
        self.data = Some(data);
        self.loading = false;

        let len = self
            .data
            .as_ref()
            .map(|d| d.services.len())
            .unwrap_or_default();
        if len == 0 {
            self.selected_service = 0;
            self.list_state.select(None);
        } else {
            if self.selected_service >= len {
                self.selected_service = len - 1;
            }
            self.list_state.select(Some(self.selected_service));
        }
    }

    fn current_entry(&self) -> Option<&ServiceCapabilityEntry> {
        self.data
            .as_ref()
            .and_then(|d| d.services.get(self.selected_service))
    }

    fn move_selection(&mut self, delta: i32) {
        let Some(data) = self.data.as_ref() else {
            return;
        };

        if data.services.is_empty() {
            return;
        }

        let len = data.services.len() as i32;
        self.selected_service = ((self.selected_service as i32 + delta).rem_euclid(len)) as usize;
        self.list_state.select(Some(self.selected_service));
    }
}

impl Component for CapabilitiesComponent {
    fn title(&self) -> &str {
        "Capabilities"
    }

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
            KeyCode::Char('r') => Action::Refresh,
            _ => Action::None,
        }
    }

    fn update(&mut self, action: &Action) {
        if let Action::DataLoaded(DataPayload::Capabilities(data)) = action {
            self.set_data(data.clone());
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, spinner: &str) {
        if self.loading {
            let loading = Paragraph::new(format!(" {} Loading capability matrix...", spinner))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(theme::BORDER_ACTIVE))
                        .title(" Capabilities "),
                )
                .style(Style::default().fg(theme::TEXT_MUTED));
            frame.render_widget(loading, area);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(area);

        let services = self
            .data
            .as_ref()
            .map(|d| d.services.as_slice())
            .unwrap_or(&[]);

        // Left: service list
        let items: Vec<ListItem> = services
            .iter()
            .map(|entry| {
                ListItem::new(Line::from(vec![
                    Span::styled(
                        &entry.service_name,
                        Style::default().fg(theme::TEXT_PRIMARY),
                    ),
                    Span::styled(" (", Style::default().fg(theme::TEXT_MUTED)),
                    Span::styled(
                        &entry.provider_name,
                        Style::default().fg(theme::SERVICE_TYPE),
                    ),
                    Span::styled(")", Style::default().fg(theme::TEXT_MUTED)),
                ]))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::BORDER_ACTIVE))
                    .title(format!(" Services ({}) ", services.len())),
            )
            .highlight_style(theme::highlight_style())
            .highlight_symbol(">> ");

        frame.render_stateful_widget(list, chunks[0], &mut self.list_state.clone());

        if services.len() as u16 > chunks[0].height.saturating_sub(2) {
            let mut scrollbar_state = ScrollbarState::new(services.len())
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

        // Right: environment + selected service details
        let mut lines = Vec::new();

        if let Some(data) = &self.data {
            lines.push(Line::styled(
                "Environment",
                Style::default().fg(theme::KEY_HINT).bold(),
            ));
            lines.push(Line::from(vec![
                Span::styled(
                    "  VCS provider: ",
                    Style::default().fg(theme::TEXT_SECONDARY),
                ),
                Span::styled(
                    data.vcs_provider.as_deref().unwrap_or("none"),
                    Style::default().fg(theme::TEXT_PRIMARY),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled(
                    "  Worktree CoW: ",
                    Style::default().fg(theme::TEXT_SECONDARY),
                ),
                Span::styled(&data.worktree_cow, Style::default().fg(theme::VALUE_PATH)),
            ]));
            lines.push(Line::raw(""));
        }

        if let Some(entry) = self.current_entry() {
            let caps = &entry.capabilities;
            lines.push(Line::styled(
                format!("Service: {} ({})", entry.service_name, entry.provider_name),
                Style::default().fg(theme::TEXT_PRIMARY).bold(),
            ));
            lines.push(Line::raw(""));

            let rows = vec![
                ("lifecycle", caps.lifecycle),
                ("logs", caps.logs),
                ("seed_from_source", caps.seed_from_source),
                ("destroy_project", caps.destroy_project),
                ("cleanup", caps.cleanup),
                ("template_from_time", caps.template_from_time),
            ];

            for (name, enabled) in rows {
                let (symbol, color) = if enabled {
                    ("yes", theme::CHECK_PASS)
                } else {
                    ("no", theme::TEXT_MUTED)
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {:20}", name),
                        Style::default().fg(theme::TEXT_SECONDARY),
                    ),
                    Span::styled(symbol, Style::default().fg(color)),
                ]));
            }

            lines.push(Line::from(vec![
                Span::styled(
                    "  max_workspace_name_length",
                    Style::default().fg(theme::TEXT_SECONDARY),
                ),
                Span::styled(
                    format!(" {}", caps.max_workspace_name_length),
                    Style::default().fg(theme::TEXT_PRIMARY),
                ),
            ]));
        } else {
            lines.push(Line::styled(
                "No services configured. Add one with `devflow service add`.",
                Style::default().fg(theme::TEXT_MUTED),
            ));
        }

        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("  r", Style::default().fg(theme::KEY_HINT)),
            Span::raw("  Refresh capability probe"),
        ]));

        let detail = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::BORDER_ACTIVE))
                    .title(" Details "),
            )
            .wrap(Wrap { trim: false });

        frame.render_widget(detail, chunks[1]);
    }
}

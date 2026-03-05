use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use super::Component;
use crate::tui::action::*;
use crate::tui::theme;

/// Services tab — detailed per-service management.
///
/// Left panel:  list of configured services with type/provider badges.
/// Right panel: per-service workspace list, connection info, seed controls.
pub struct ServicesTabComponent {
    data: Option<ServicesData>,
    list_state: ListState,
    selected_service: usize,
    /// Which workspace is focused inside the selected service
    selected_workspace: usize,
    loading: bool,
}

impl ServicesTabComponent {
    pub fn new() -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            data: None,
            list_state,
            selected_service: 0,
            selected_workspace: 0,
            loading: false,
        }
    }

    fn service_count(&self) -> usize {
        self.data.as_ref().map(|d| d.services.len()).unwrap_or(0)
    }

    fn selected_entry(&self) -> Option<&ServiceEntry> {
        self.data
            .as_ref()
            .and_then(|d| d.services.get(self.selected_service))
    }
}

impl Component for ServicesTabComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Action {
        let count = self.service_count();

        // 'a' to add a service works even when no services exist
        if key.code == KeyCode::Char('a') {
            return Action::ShowSelect {
                title: "Add Service — select type".to_string(),
                options: vec![
                    "postgres    — PostgreSQL database".to_string(),
                    "clickhouse  — ClickHouse analytics database".to_string(),
                    "mysql       — MySQL database".to_string(),
                    "generic     — Generic Docker container".to_string(),
                ],
                on_select: SelectTarget::AddServiceType,
            };
        }

        if count == 0 {
            return match key.code {
                KeyCode::Char('r') => Action::Refresh,
                _ => Action::None,
            };
        }

        match key.code {
            // Navigate services
            KeyCode::Char('j') | KeyCode::Down => {
                self.selected_service = (self.selected_service + 1) % count;
                self.list_state.select(Some(self.selected_service));
                self.selected_workspace = 0;
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected_service = (self.selected_service + count - 1) % count;
                self.list_state.select(Some(self.selected_service));
                self.selected_workspace = 0;
                Action::None
            }
            // Navigate workspaces within service
            KeyCode::Char('n') | KeyCode::Right => {
                if let Some(svc) = self.selected_entry() {
                    let ws_count = svc.workspaces.len();
                    if ws_count > 0 {
                        self.selected_workspace = (self.selected_workspace + 1) % ws_count;
                    }
                }
                Action::None
            }
            KeyCode::Char('p') | KeyCode::Left => {
                if let Some(svc) = self.selected_entry() {
                    let ws_count = svc.workspaces.len();
                    if ws_count > 0 {
                        self.selected_workspace =
                            (self.selected_workspace + ws_count - 1) % ws_count;
                    }
                }
                Action::None
            }
            // Start focused workspace's service
            KeyCode::Char('S') => {
                if let Some(svc) = self.selected_entry() {
                    if let Some(ws) = svc.workspaces.get(self.selected_workspace) {
                        return Action::StartService {
                            service: svc.name.clone(),
                            workspace: ws.name.clone(),
                        };
                    }
                }
                Action::None
            }
            // Stop focused workspace's service
            KeyCode::Char('x') => {
                if let Some(svc) = self.selected_entry() {
                    if let Some(ws) = svc.workspaces.get(self.selected_workspace) {
                        return Action::StopService {
                            service: svc.name.clone(),
                            workspace: ws.name.clone(),
                        };
                    }
                }
                Action::None
            }
            // View logs
            KeyCode::Char('l') => {
                if let Some(svc) = self.selected_entry() {
                    if let Some(ws) = svc.workspaces.get(self.selected_workspace) {
                        return Action::ViewLogs {
                            service: svc.name.clone(),
                            workspace: ws.name.clone(),
                        };
                    }
                }
                Action::None
            }
            // Remove service config
            KeyCode::Char('D') => {
                if let Some(svc) = self.selected_entry() {
                    let name = svc.name.clone();
                    return Action::ShowConfirm {
                        title: "Remove Service".to_string(),
                        message: format!(
                            "Remove service '{}' configuration? This does not destroy data.",
                            name
                        ),
                        on_confirm: Box::new(Action::RemoveServiceConfig(name)),
                    };
                }
                Action::None
            }
            KeyCode::Char('r') => Action::Refresh,
            _ => Action::None,
        }
    }

    fn update(&mut self, action: &Action) {
        match action {
            Action::DataLoaded(DataPayload::Services(data)) => {
                self.data = Some(data.clone());
                self.loading = false;
                // Clamp selection
                let count = self.service_count();
                if self.selected_service >= count && count > 0 {
                    self.selected_service = count - 1;
                    self.list_state.select(Some(self.selected_service));
                }
            }
            Action::Refresh => {
                self.loading = true;
            }
            _ => {}
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, spinner: &str) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(area);

        self.render_service_list(frame, chunks[0], spinner);
        self.render_service_detail(frame, chunks[1]);
    }
}

impl ServicesTabComponent {
    fn render_service_list(&self, frame: &mut Frame, area: Rect, spinner: &str) {
        let title = if self.loading {
            format!(" Services {} ", spinner)
        } else {
            format!(" Services ({}) ", self.service_count())
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::BORDER_ACTIVE))
            .title(Span::styled(
                title,
                Style::default().fg(theme::TAB_TITLE).bold(),
            ));

        let items: Vec<ListItem> = match &self.data {
            Some(data) => data
                .services
                .iter()
                .enumerate()
                .map(|(i, svc)| {
                    let badge = format!("[{}]", svc.provider_type);
                    let ws_count = svc.workspaces.len();
                    let line = Line::from(vec![
                        Span::styled(&svc.name, Style::default().fg(theme::TEXT_PRIMARY).bold()),
                        Span::raw(" "),
                        Span::styled(badge, Style::default().fg(theme::SERVICE_TYPE)),
                        Span::raw(" "),
                        Span::styled(
                            format!("({} ws)", ws_count),
                            Style::default().fg(theme::TEXT_MUTED),
                        ),
                    ]);
                    if i == self.selected_service {
                        ListItem::new(line).style(theme::highlight_style())
                    } else {
                        ListItem::new(line)
                    }
                })
                .collect(),
            None => vec![ListItem::new(Line::styled(
                if self.loading {
                    "Loading..."
                } else {
                    "No services configured"
                },
                Style::default().fg(theme::TEXT_MUTED),
            ))],
        };

        let list = List::new(items).block(block);
        frame.render_widget(list, area);
    }

    fn render_service_detail(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::BORDER_INACTIVE))
            .title(Span::styled(
                " Detail ",
                Style::default().fg(theme::TAB_TITLE).bold(),
            ));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let svc = match self.selected_entry() {
            Some(s) => s,
            None => {
                let hint = Paragraph::new("Select a service")
                    .style(Style::default().fg(theme::TEXT_MUTED));
                frame.render_widget(hint, inner);
                return;
            }
        };

        let mut lines: Vec<Line> = Vec::new();

        // Service header
        lines.push(Line::from(vec![
            Span::styled("Service: ", Style::default().fg(theme::TEXT_SECONDARY)),
            Span::styled(&svc.name, Style::default().fg(theme::TEXT_PRIMARY).bold()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Type:    ", Style::default().fg(theme::TEXT_SECONDARY)),
            Span::styled(&svc.service_type, Style::default().fg(theme::SERVICE_TYPE)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Provider:", Style::default().fg(theme::TEXT_SECONDARY)),
            Span::styled(
                format!(" {}", svc.provider_type),
                Style::default().fg(theme::TEXT_PRIMARY),
            ),
        ]));

        if let Some(ref info) = svc.project_info {
            if let Some(ref image) = info.image {
                lines.push(Line::from(vec![
                    Span::styled("Image:   ", Style::default().fg(theme::TEXT_SECONDARY)),
                    Span::styled(image, Style::default().fg(theme::VALUE_PATH)),
                ]));
            }
            if let Some(ref driver) = info.storage_driver {
                lines.push(Line::from(vec![
                    Span::styled("Storage: ", Style::default().fg(theme::TEXT_SECONDARY)),
                    Span::styled(driver, Style::default().fg(theme::TEXT_PRIMARY)),
                ]));
            }
        }

        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Workspaces:",
            Style::default().fg(theme::TEXT_SECONDARY).bold(),
        ));

        if svc.workspaces.is_empty() {
            lines.push(Line::styled(
                "  (no workspaces)",
                Style::default().fg(theme::TEXT_MUTED),
            ));
        } else {
            for (i, ws) in svc.workspaces.iter().enumerate() {
                let marker = if i == self.selected_workspace {
                    ">"
                } else {
                    " "
                };
                let state_str = ws.state.as_deref().unwrap_or("unknown");
                let state_color = theme::state_color(state_str);
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {} ", marker),
                        Style::default().fg(theme::TEXT_PRIMARY),
                    ),
                    Span::styled(&ws.name, Style::default().fg(theme::TEXT_PRIMARY).bold()),
                    Span::raw("  "),
                    Span::styled(state_str, Style::default().fg(state_color)),
                ]));
                if !ws.database_name.is_empty() {
                    lines.push(Line::from(vec![
                        Span::raw("      db: "),
                        Span::styled(
                            &ws.database_name,
                            Style::default().fg(theme::VALUE_DATABASE),
                        ),
                    ]));
                }
                if let Some(ref parent) = ws.parent_workspace {
                    lines.push(Line::from(vec![
                        Span::raw("      parent: "),
                        Span::styled(parent, Style::default().fg(theme::VALUE_PARENT)),
                    ]));
                }
            }
        }

        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "a:Add  D:Remove  n/p:Workspace  S:Start  x:Stop  l:Logs  r:Refresh",
            Style::default().fg(theme::KEY_HINT),
        ));

        let content = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(content, inner);
    }
}

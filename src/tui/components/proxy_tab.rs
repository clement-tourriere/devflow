use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

use super::Component;
use crate::tui::action::*;
use crate::tui::theme;

/// Proxy tab — reverse proxy management.
///
/// Top:    proxy status (running/stopped, ports, CA status).
/// Bottom: container routing table (domain → container → upstream).
pub struct ProxyTabComponent {
    status: Option<ProxyStatusData>,
    targets: Vec<ProxyTargetEntry>,
    list_state: ListState,
    selected_target: usize,
    loading: bool,
}

/// Proxy status information.
#[derive(Debug, Clone)]
pub struct ProxyStatusData {
    pub running: bool,
    pub https_port: u16,
    pub http_port: u16,
    pub api_port: u16,
    pub ca_installed: bool,
}

/// A single proxy routing target.
#[derive(Debug, Clone)]
pub struct ProxyTargetEntry {
    pub domain: String,
    pub container_name: String,
    pub container_ip: String,
    pub port: u16,
}

impl ProxyTabComponent {
    pub fn new() -> Self {
        Self {
            status: None,
            targets: Vec::new(),
            list_state: ListState::default(),
            selected_target: 0,
            loading: false,
        }
    }

    pub fn set_status(&mut self, status: ProxyStatusData) {
        self.status = Some(status);
        self.loading = false;
    }

    pub fn set_targets(&mut self, targets: Vec<ProxyTargetEntry>) {
        self.targets = targets;
        if self.selected_target >= self.targets.len() && !self.targets.is_empty() {
            self.selected_target = self.targets.len() - 1;
        }
        self.list_state.select(if self.targets.is_empty() {
            None
        } else {
            Some(self.selected_target)
        });
    }
}

impl Component for ProxyTabComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.targets.is_empty() {
                    self.selected_target = (self.selected_target + 1) % self.targets.len();
                    self.list_state.select(Some(self.selected_target));
                }
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if !self.targets.is_empty() {
                    self.selected_target =
                        (self.selected_target + self.targets.len() - 1) % self.targets.len();
                    self.list_state.select(Some(self.selected_target));
                }
                Action::None
            }
            KeyCode::Char('s') => Action::StartProxy,
            KeyCode::Char('x') => Action::StopProxy,
            KeyCode::Char('r') => Action::Refresh,
            _ => Action::None,
        }
    }

    fn update(&mut self, action: &Action) {
        match action {
            Action::DataLoaded(DataPayload::ProxyStatus(data)) => {
                self.set_status(data.clone());
            }
            Action::DataLoaded(DataPayload::ProxyTargets(targets)) => {
                self.set_targets(targets.clone());
            }
            Action::Refresh => {
                self.loading = true;
            }
            _ => {}
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, spinner: &str) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(8), Constraint::Min(5)])
            .split(area);

        self.render_status_panel(frame, chunks[0], spinner);
        self.render_targets_panel(frame, chunks[1]);
    }
}

impl ProxyTabComponent {
    fn render_status_panel(&self, frame: &mut Frame, area: Rect, spinner: &str) {
        let title = if self.loading {
            format!(" Proxy Status {} ", spinner)
        } else {
            " Proxy Status ".to_string()
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::BORDER_ACTIVE))
            .title(Span::styled(
                title,
                Style::default().fg(theme::TAB_TITLE).bold(),
            ));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();

        match &self.status {
            Some(status) => {
                let (state_text, state_color) = if status.running {
                    ("running", theme::CHECK_PASS)
                } else {
                    ("stopped", theme::CHECK_FAIL)
                };
                lines.push(Line::from(vec![
                    Span::styled("Status:  ", Style::default().fg(theme::TEXT_SECONDARY)),
                    Span::styled(state_text, Style::default().fg(state_color).bold()),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("HTTPS:   ", Style::default().fg(theme::TEXT_SECONDARY)),
                    Span::styled(
                        format!("https://localhost:{}", status.https_port),
                        Style::default().fg(theme::VALUE_PATH),
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("HTTP:    ", Style::default().fg(theme::TEXT_SECONDARY)),
                    Span::styled(
                        format!("http://localhost:{}", status.http_port),
                        Style::default().fg(theme::VALUE_PATH),
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("API:     ", Style::default().fg(theme::TEXT_SECONDARY)),
                    Span::styled(
                        format!("http://localhost:{}", status.api_port),
                        Style::default().fg(theme::TEXT_PRIMARY),
                    ),
                ]));
                let ca_text = if status.ca_installed {
                    "installed"
                } else {
                    "not installed"
                };
                let ca_color = if status.ca_installed {
                    theme::CHECK_PASS
                } else {
                    theme::CHECK_FAIL
                };
                lines.push(Line::from(vec![
                    Span::styled("CA cert: ", Style::default().fg(theme::TEXT_SECONDARY)),
                    Span::styled(ca_text, Style::default().fg(ca_color)),
                ]));
            }
            None => {
                lines.push(Line::styled(
                    if self.loading {
                        "Checking proxy status..."
                    } else {
                        "Proxy status unknown — press r to refresh"
                    },
                    Style::default().fg(theme::TEXT_MUTED),
                ));
            }
        }

        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "s:Start  x:Stop  r:Refresh",
            Style::default().fg(theme::KEY_HINT),
        ));

        let content = Paragraph::new(lines);
        frame.render_widget(content, inner);
    }

    fn render_targets_panel(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::BORDER_INACTIVE))
            .title(Span::styled(
                format!(" Routing Table ({}) ", self.targets.len()),
                Style::default().fg(theme::TAB_TITLE).bold(),
            ));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if self.targets.is_empty() {
            let hint = Paragraph::new(if self.status.as_ref().is_some_and(|s| s.running) {
                "No proxied containers"
            } else {
                "Start the proxy to see routing targets"
            })
            .style(Style::default().fg(theme::TEXT_MUTED));
            frame.render_widget(hint, inner);
            return;
        }

        // Header
        let header = Line::from(vec![
            Span::styled(
                format!("{:<40}", "DOMAIN"),
                Style::default().fg(theme::TEXT_SECONDARY).bold(),
            ),
            Span::styled(
                format!("{:<20}", "CONTAINER"),
                Style::default().fg(theme::TEXT_SECONDARY).bold(),
            ),
            Span::styled(
                "UPSTREAM",
                Style::default().fg(theme::TEXT_SECONDARY).bold(),
            ),
        ]);

        let items: Vec<ListItem> = std::iter::once(ListItem::new(header))
            .chain(self.targets.iter().enumerate().map(|(i, t)| {
                let line = Line::from(vec![
                    Span::styled(
                        format!("{:<40}", format!("https://{}", t.domain)),
                        Style::default().fg(theme::VALUE_PATH),
                    ),
                    Span::styled(
                        format!("{:<20}", t.container_name),
                        Style::default().fg(theme::TEXT_PRIMARY),
                    ),
                    Span::styled(
                        format!("{}:{}", t.container_ip, t.port),
                        Style::default().fg(theme::TEXT_MUTED),
                    ),
                ]);
                if i == self.selected_target {
                    ListItem::new(line).style(theme::highlight_style())
                } else {
                    ListItem::new(line)
                }
            }))
            .collect();

        let list = List::new(items);
        frame.render_widget(list, inner);
    }
}

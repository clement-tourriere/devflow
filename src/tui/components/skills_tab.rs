use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
    Frame,
};

use super::Component;
use crate::tui::action::{Action, DataPayload, InputTarget, SkillEntry, SkillSearchEntry};
use crate::tui::theme;

#[derive(Debug, Clone, Copy, PartialEq)]
enum SkillsMode {
    Installed,
    Search,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SkillScope {
    Project,
    User,
}

pub struct SkillsTabComponent {
    installed: Vec<SkillEntry>,
    search_results: Vec<SkillSearchEntry>,
    updates_available: Vec<String>,
    selected_index: usize,
    list_state: ListState,
    mode: SkillsMode,
    #[allow(dead_code)]
    search_query: String,
    detail_scroll: u16,
    loading: bool,
    scope: SkillScope,
}

impl SkillsTabComponent {
    pub fn new() -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            installed: Vec::new(),
            search_results: Vec::new(),
            updates_available: Vec::new(),
            selected_index: 0,
            list_state,
            mode: SkillsMode::Installed,
            search_query: String::new(),
            detail_scroll: 0,
            loading: false,
            scope: SkillScope::Project,
        }
    }

    /// Return the current scope so the app can decide which fetcher to call.
    pub fn scope(&self) -> SkillScope {
        self.scope
    }

    fn current_list_len(&self) -> usize {
        match self.mode {
            SkillsMode::Installed => self.installed.len(),
            SkillsMode::Search => self.search_results.len(),
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.current_list_len();
        if len == 0 {
            return;
        }
        let new = (self.selected_index as isize + delta).clamp(0, len as isize - 1) as usize;
        self.selected_index = new;
        self.list_state.select(Some(new));
        self.detail_scroll = 0;
    }

    fn render_list(&self, frame: &mut Frame, area: Rect, spinner: &str) {
        let scope_label = match self.scope {
            SkillScope::Project => "Project",
            SkillScope::User => "User",
        };
        let title = match self.mode {
            SkillsMode::Installed => {
                if self.loading {
                    format!(" Skills [{}] {} ", scope_label, spinner)
                } else {
                    format!(" Skills [{}] ({}) ", scope_label, self.installed.len())
                }
            }
            SkillsMode::Search => {
                if self.loading {
                    format!(" Search {} ", spinner)
                } else {
                    format!(" Search ({}) ", self.search_results.len())
                }
            }
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::BORDER_ACTIVE))
            .title(Span::styled(
                title,
                Style::default()
                    .fg(theme::TAB_TITLE)
                    .add_modifier(Modifier::BOLD),
            ));

        let items: Vec<ListItem> = match self.mode {
            SkillsMode::Installed => {
                if self.installed.is_empty() {
                    vec![ListItem::new(Line::styled(
                        if self.loading {
                            "Loading..."
                        } else {
                            "No skills installed. Press / to search."
                        },
                        Style::default().fg(theme::TEXT_MUTED),
                    ))]
                } else {
                    self.installed
                        .iter()
                        .enumerate()
                        .map(|(i, skill)| {
                            let has_update =
                                skill.managed && self.updates_available.contains(&skill.name);
                            let mut spans = vec![Span::styled(
                                &skill.name,
                                Style::default()
                                    .fg(theme::TEXT_PRIMARY)
                                    .add_modifier(Modifier::BOLD),
                            )];
                            if !skill.managed {
                                spans.push(Span::raw(" "));
                                spans.push(Span::styled(
                                    "[ext]",
                                    Style::default().fg(theme::TEXT_MUTED),
                                ));
                            }
                            if has_update {
                                spans.push(Span::raw(" "));
                                spans.push(Span::styled(
                                    "↑",
                                    Style::default().fg(theme::CHECK_FAIL),
                                ));
                            }
                            let line = Line::from(spans);
                            if i == self.selected_index {
                                ListItem::new(line).style(theme::highlight_style())
                            } else {
                                ListItem::new(line)
                            }
                        })
                        .collect()
                }
            }
            SkillsMode::Search => {
                if self.search_results.is_empty() {
                    vec![ListItem::new(Line::styled(
                        if self.loading {
                            "Searching..."
                        } else {
                            "No results"
                        },
                        Style::default().fg(theme::TEXT_MUTED),
                    ))]
                } else {
                    self.search_results
                        .iter()
                        .enumerate()
                        .map(|(i, result)| {
                            let installs_label = if result.installs >= 1000 {
                                format!("{:.1}K", result.installs as f64 / 1000.0)
                            } else {
                                result.installs.to_string()
                            };
                            let line = Line::from(vec![
                                Span::styled(
                                    &result.name,
                                    Style::default()
                                        .fg(theme::TEXT_PRIMARY)
                                        .add_modifier(Modifier::BOLD),
                                ),
                                Span::raw(" "),
                                Span::styled(
                                    installs_label,
                                    Style::default().fg(theme::TEXT_MUTED),
                                ),
                            ]);
                            if i == self.selected_index {
                                ListItem::new(line).style(theme::highlight_style())
                            } else {
                                ListItem::new(line)
                            }
                        })
                        .collect()
                }
            }
        };

        let list = List::new(items).block(block).highlight_symbol(">> ");
        frame.render_stateful_widget(list, area, &mut self.list_state.clone());

        // Scrollbar
        let len = self.current_list_len();
        let visible = area.height.saturating_sub(2) as usize;
        if len > visible {
            let mut scrollbar_state = ScrollbarState::new(len)
                .position(self.selected_index)
                .viewport_content_length(visible);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                area,
                &mut scrollbar_state,
            );
        }
    }

    fn render_detail(&self, frame: &mut Frame, area: Rect) {
        match self.mode {
            SkillsMode::Installed => {
                if let Some(skill) = self.installed.get(self.selected_index) {
                    let source_line = format!("Source: {}", skill.source_label);

                    let mut lines: Vec<Line> = vec![Line::styled(
                        source_line,
                        Style::default().fg(theme::TEXT_SECONDARY),
                    )];

                    if skill.managed {
                        let hash_line = format!(
                            "Hash:   {}",
                            &skill.content_hash[..12.min(skill.content_hash.len())]
                        );
                        let date_line = format!("Installed: {}", skill.installed_at);
                        lines.push(Line::styled(
                            hash_line,
                            Style::default().fg(theme::TEXT_SECONDARY),
                        ));
                        lines.push(Line::styled(
                            date_line,
                            Style::default().fg(theme::TEXT_SECONDARY),
                        ));
                    } else {
                        lines.push(Line::styled(
                            "Status: read-only (not managed by devflow)".to_string(),
                            Style::default().fg(theme::TEXT_MUTED),
                        ));
                    }
                    lines.push(Line::raw(""));

                    if let Some(ref content) = skill.content {
                        for raw_line in content.lines() {
                            let style = if raw_line.starts_with('#') {
                                Style::default()
                                    .fg(theme::TAB_TITLE)
                                    .add_modifier(Modifier::BOLD)
                            } else if raw_line.starts_with("```") {
                                Style::default().fg(theme::TEXT_MUTED)
                            } else {
                                Style::default().fg(theme::TEXT_PRIMARY)
                            };
                            lines.push(Line::styled(raw_line.to_string(), style));
                        }
                    }

                    let block = Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(theme::BORDER_ACTIVE))
                        .title(Span::styled(
                            format!(" {} ", skill.name),
                            Style::default()
                                .fg(theme::TAB_TITLE)
                                .add_modifier(Modifier::BOLD),
                        ));

                    let paragraph = Paragraph::new(lines)
                        .block(block)
                        .wrap(Wrap { trim: false })
                        .scroll((self.detail_scroll, 0));
                    frame.render_widget(paragraph, area);
                } else {
                    let block = Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(theme::BORDER_INACTIVE))
                        .title(Span::styled(
                            " Detail ",
                            Style::default()
                                .fg(theme::TAB_TITLE)
                                .add_modifier(Modifier::BOLD),
                        ));
                    let msg = Paragraph::new(Line::styled(
                        "Select a skill to view details",
                        Style::default().fg(theme::TEXT_MUTED),
                    ))
                    .block(block);
                    frame.render_widget(msg, area);
                }
            }
            SkillsMode::Search => {
                if let Some(result) = self.search_results.get(self.selected_index) {
                    let installs_label = if result.installs >= 1000 {
                        format!("{:.1}K", result.installs as f64 / 1000.0)
                    } else {
                        result.installs.to_string()
                    };
                    let lines = vec![
                        Line::styled(
                            format!("Source:   {}", result.source),
                            Style::default().fg(theme::TEXT_SECONDARY),
                        ),
                        Line::styled(
                            format!("Installs: {}", installs_label),
                            Style::default().fg(theme::TEXT_SECONDARY),
                        ),
                        Line::raw(""),
                        Line::styled(
                            "Press Enter to install",
                            Style::default().fg(theme::KEY_HINT),
                        ),
                    ];
                    let block = Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(theme::BORDER_ACTIVE))
                        .title(Span::styled(
                            format!(" {} ", result.name),
                            Style::default()
                                .fg(theme::TAB_TITLE)
                                .add_modifier(Modifier::BOLD),
                        ));
                    let paragraph = Paragraph::new(lines).block(block);
                    frame.render_widget(paragraph, area);
                } else {
                    let block = Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(theme::BORDER_INACTIVE))
                        .title(Span::styled(
                            " Detail ",
                            Style::default()
                                .fg(theme::TAB_TITLE)
                                .add_modifier(Modifier::BOLD),
                        ));
                    let msg = Paragraph::new(Line::styled(
                        "Select a search result",
                        Style::default().fg(theme::TEXT_MUTED),
                    ))
                    .block(block);
                    frame.render_widget(msg, area);
                }
            }
        }
    }
}

impl Component for SkillsTabComponent {
    fn handle_key_event(&mut self, key: KeyEvent) -> Action {
        match key.code {
            // Navigation
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_selection(1);
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_selection(-1);
                Action::None
            }
            // Scroll detail
            KeyCode::Char('J') => {
                self.detail_scroll = self.detail_scroll.saturating_add(1);
                Action::None
            }
            KeyCode::Char('K') => {
                self.detail_scroll = self.detail_scroll.saturating_sub(1);
                Action::None
            }
            // Toggle scope (Tab)
            KeyCode::Tab if self.mode == SkillsMode::Installed => {
                self.scope = match self.scope {
                    SkillScope::Project => SkillScope::User,
                    SkillScope::User => SkillScope::Project,
                };
                self.selected_index = 0;
                self.list_state.select(Some(0));
                self.detail_scroll = 0;
                self.installed.clear();
                self.updates_available.clear();
                self.loading = true;
                Action::SkillToggleScope
            }
            // Search
            KeyCode::Char('/') if self.mode == SkillsMode::Installed => Action::ShowInput {
                title: "Search skills.sh".to_string(),
                on_submit: InputTarget::SkillSearch,
            },
            // Back to installed
            KeyCode::Esc if self.mode == SkillsMode::Search => {
                self.mode = SkillsMode::Installed;
                self.selected_index = 0;
                self.list_state.select(Some(0));
                self.search_results.clear();
                self.search_query.clear();
                self.detail_scroll = 0;
                Action::None
            }
            // Install from search (uses current scope)
            KeyCode::Enter if self.mode == SkillsMode::Search => {
                if let Some(result) = self.search_results.get(self.selected_index) {
                    let identifier = format!("{}/{}", result.source, result.name);
                    match self.scope {
                        SkillScope::Project => Action::SkillInstall(identifier),
                        SkillScope::User => Action::UserSkillInstall(identifier),
                    }
                } else {
                    Action::None
                }
            }
            // Remove (only managed skills)
            KeyCode::Char('d') if self.mode == SkillsMode::Installed => {
                if let Some(skill) = self.installed.get(self.selected_index) {
                    if !skill.managed {
                        return Action::None; // Cannot remove external skills
                    }
                    let action = match self.scope {
                        SkillScope::Project => Action::SkillRemove(skill.name.clone()),
                        SkillScope::User => Action::UserSkillRemove(skill.name.clone()),
                    };
                    Action::ShowConfirm {
                        title: "Remove skill".to_string(),
                        message: format!("Remove '{}'?", skill.name),
                        on_confirm: Box::new(action),
                    }
                } else {
                    Action::None
                }
            }
            // Update selected (only managed skills)
            KeyCode::Char('u') if self.mode == SkillsMode::Installed => {
                if let Some(skill) = self.installed.get(self.selected_index) {
                    if !skill.managed {
                        return Action::None; // Cannot update external skills
                    }
                    match self.scope {
                        SkillScope::Project => Action::SkillUpdate(Some(skill.name.clone())),
                        SkillScope::User => Action::UserSkillUpdate(Some(skill.name.clone())),
                    }
                } else {
                    Action::None
                }
            }
            // Update all
            KeyCode::Char('U') if self.mode == SkillsMode::Installed => match self.scope {
                SkillScope::Project => Action::SkillUpdate(None),
                SkillScope::User => Action::UserSkillUpdate(None),
            },
            // Refresh
            KeyCode::Char('r') => Action::Refresh,
            _ => Action::None,
        }
    }

    fn update(&mut self, action: &Action) {
        match action {
            Action::DataLoaded(DataPayload::Skills(data)) if self.scope == SkillScope::Project => {
                self.installed = data.installed.clone();
                self.updates_available = data.updates_available.clone();
                self.loading = false;
                if self.selected_index >= self.installed.len() && !self.installed.is_empty() {
                    self.selected_index = 0;
                    self.list_state.select(Some(0));
                }
            }
            Action::DataLoaded(DataPayload::UserSkills(data)) if self.scope == SkillScope::User => {
                self.installed = data.installed.clone();
                self.updates_available = data.updates_available.clone();
                self.loading = false;
                if self.selected_index >= self.installed.len() && !self.installed.is_empty() {
                    self.selected_index = 0;
                    self.list_state.select(Some(0));
                }
            }
            Action::SkillSearchResults(results) => {
                self.search_results = results.clone();
                self.mode = SkillsMode::Search;
                self.selected_index = 0;
                self.list_state.select(Some(0));
                self.detail_scroll = 0;
                self.loading = false;
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
        self.render_list(frame, chunks[0], spinner);
        self.render_detail(frame, chunks[1]);
    }
}

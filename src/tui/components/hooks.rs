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

const TEMPLATE_VARIABLES: &[(&str, &str)] = &[
    ("{{ branch }}", "Current branch name"),
    ("{{ repo }}", "Repository directory name"),
    (
        "{{ worktree_path }}",
        "Current worktree path (if available)",
    ),
    ("{{ default_branch }}", "Default branch (main/master)"),
    ("{{ commit }}", "Current commit SHA when available"),
    ("{{ target }}", "Merge target branch for merge hooks"),
    ("{{ base }}", "Parent/base branch for create hooks"),
    ("{{ service['db'].url }}", "Service connection URL"),
    ("{{ service['db'].host }}", "Service host"),
    ("{{ service['db'].port }}", "Service port"),
    ("{{ service['db'].database }}", "Service database name"),
    ("{{ service['db'].user }}", "Service username"),
    ("{{ service['db'].password }}", "Service password (if set)"),
];

const TEMPLATE_FILTERS: &[(&str, &str)] = &[
    ("sanitize", "Make path/shell-safe branch slugs"),
    ("sanitize_db", "Make DB-safe identifiers"),
    ("hash_port", "Stable pseudo-random port (10000-19999)"),
    ("lower", "Lowercase text"),
    ("upper", "Uppercase text"),
    ("replace", "Substring replacement"),
    ("truncate", "Trim to max length"),
];

const HOOK_SCAFFOLDS: &[(&str, &str)] = &[
    (
        "post-switch env file",
        r#"hooks:
  post-switch:
    env:
      command: "echo DATABASE_URL={{ service['app-db'].url }} > .env.local""#,
    ),
    (
        "post-create migrate",
        r#"hooks:
  post-create:
    migrate:
      command: "cargo sqlx migrate run"
      continue_on_error: false"#,
    ),
    (
        "service-aware restart",
        r#"hooks:
  post-switch:
    restart-app:
      command: "docker compose up -d app"
      condition: "file_exists:docker-compose.yml""#,
    ),
];

pub struct HooksComponent {
    data: Option<HooksData>,
    list_state: ListState,
    selected_phase: usize,
    loading: bool,
    show_reference: bool,
    scaffold_index: usize,
}

impl HooksComponent {
    pub fn new() -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            data: None,
            list_state,
            selected_phase: 0,
            loading: true,
            show_reference: false,
            scaffold_index: 0,
        }
    }

    pub fn set_data(&mut self, data: HooksData) {
        self.data = Some(data);
        self.loading = false;
    }

    fn current_phase(&self) -> Option<&HookPhaseEntry> {
        self.data
            .as_ref()
            .and_then(|d| d.phases.get(self.selected_phase))
    }

    fn move_selection(&mut self, delta: i32) {
        if let Some(ref data) = self.data {
            if data.phases.is_empty() {
                return;
            }
            let len = data.phases.len() as i32;
            self.selected_phase = ((self.selected_phase as i32 + delta).rem_euclid(len)) as usize;
            self.list_state.select(Some(self.selected_phase));
        }
    }

    fn next_scaffold(&mut self) {
        if HOOK_SCAFFOLDS.is_empty() {
            self.scaffold_index = 0;
            return;
        }
        self.scaffold_index = (self.scaffold_index + 1) % HOOK_SCAFFOLDS.len();
    }

    fn render_phase_list(&self, frame: &mut Frame, area: Rect) {
        let phases = self.data.as_ref().map(|d| &d.phases[..]).unwrap_or(&[]);

        let items: Vec<ListItem> = phases
            .iter()
            .map(|phase| {
                let count = phase.hooks.len();
                ListItem::new(Line::from(vec![
                    Span::styled(&phase.phase, Style::default().fg(theme::HOOK_PHASE)),
                    Span::styled(
                        format!(" ({} hooks)", count),
                        Style::default().fg(theme::TEXT_SECONDARY),
                    ),
                ]))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::BORDER_ACTIVE))
                    .title(format!(" Hook Phases ({}) ", phases.len())),
            )
            .highlight_style(theme::highlight_style())
            .highlight_symbol(">> ");

        frame.render_stateful_widget(list, area, &mut self.list_state.clone());

        // Scrollbar
        let visible_height = area.height.saturating_sub(2) as usize;
        if phases.len() > visible_height {
            let mut scrollbar_state = ScrollbarState::new(phases.len())
                .position(self.selected_phase)
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

    fn render_hook_detail(&self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::new();

        if let Some(phase) = self.current_phase() {
            lines.push(Line::from(vec![
                Span::styled("Phase: ", Style::default().fg(theme::TEXT_SECONDARY)),
                Span::styled(&phase.phase, Style::default().fg(theme::HOOK_PHASE).bold()),
            ]));
            lines.push(Line::raw(""));

            if phase.hooks.is_empty() {
                lines.push(Line::styled(
                    "  No hooks configured for this phase",
                    Style::default().fg(theme::TEXT_MUTED),
                ));
            } else {
                for hook in &phase.hooks {
                    // Hook name
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {}", hook.name),
                            Style::default().fg(theme::HOOK_NAME).bold(),
                        ),
                        if hook.is_extended {
                            Span::styled(" (extended)", Style::default().fg(theme::TEXT_SECONDARY))
                        } else {
                            Span::raw("")
                        },
                        if hook.background {
                            Span::styled(" [bg]", Style::default().fg(theme::HOOK_BACKGROUND))
                        } else {
                            Span::raw("")
                        },
                    ]));

                    // Command
                    lines.push(Line::from(vec![
                        Span::styled("    cmd: ", Style::default().fg(theme::TEXT_SECONDARY)),
                        Span::styled(&hook.command, Style::default().fg(theme::HOOK_COMMAND)),
                    ]));

                    // Condition
                    if let Some(ref cond) = hook.condition {
                        lines.push(Line::from(vec![
                            Span::styled("    if:  ", Style::default().fg(theme::TEXT_SECONDARY)),
                            Span::styled(cond, Style::default().fg(theme::HOOK_CONDITION)),
                        ]));
                    }

                    lines.push(Line::raw(""));
                }
            }
        } else {
            lines.push(Line::styled(
                "No hooks configured",
                Style::default().fg(theme::TEXT_MUTED),
            ));
        }

        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("  v", Style::default().fg(theme::KEY_HINT)),
            Span::raw("  Toggle template reference"),
        ]));

        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::BORDER_ACTIVE))
                    .title(" Hook Details "),
            )
            .wrap(Wrap { trim: false });

        frame.render_widget(paragraph, area);
    }

    fn render_template_reference(&self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::new();

        lines.push(Line::styled(
            "Template Variables",
            Style::default().fg(theme::TEXT_PRIMARY).bold(),
        ));
        for (name, desc) in TEMPLATE_VARIABLES {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:30}", name),
                    Style::default().fg(theme::HOOK_COMMAND),
                ),
                Span::styled(*desc, Style::default().fg(theme::TEXT_SECONDARY)),
            ]));
        }

        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Filters",
            Style::default().fg(theme::TEXT_PRIMARY).bold(),
        ));
        for (name, desc) in TEMPLATE_FILTERS {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:20}", format!("| {}", name)),
                    Style::default().fg(theme::HOOK_CONDITION),
                ),
                Span::styled(*desc, Style::default().fg(theme::TEXT_SECONDARY)),
            ]));
        }

        lines.push(Line::raw(""));
        let (scaffold_name, scaffold_text) = HOOK_SCAFFOLDS
            .get(self.scaffold_index)
            .copied()
            .unwrap_or(("example", "hooks:\n  post-switch: {}"));
        lines.push(Line::styled(
            format!(
                "Scaffold {} / {}: {}",
                self.scaffold_index.saturating_add(1),
                HOOK_SCAFFOLDS.len().max(1),
                scaffold_name
            ),
            Style::default().fg(theme::TEXT_PRIMARY).bold(),
        ));
        for line in scaffold_text.lines() {
            lines.push(Line::styled(
                format!("  {}", line),
                Style::default().fg(theme::YAML_VALUE),
            ));
        }

        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("  v", Style::default().fg(theme::KEY_HINT)),
            Span::raw("  Back to hook details"),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  s", Style::default().fg(theme::KEY_HINT)),
            Span::raw("  Next scaffold example"),
        ]));

        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::BORDER_ACTIVE))
                    .title(" Hook Templates "),
            )
            .wrap(Wrap { trim: false });

        frame.render_widget(paragraph, area);
    }
}

impl Component for HooksComponent {
    fn title(&self) -> &str {
        "Hooks"
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
            KeyCode::Char('v') => {
                self.show_reference = !self.show_reference;
                Action::None
            }
            KeyCode::Char('s') => {
                if self.show_reference {
                    self.next_scaffold();
                }
                Action::None
            }
            KeyCode::Char('r') => Action::Refresh,
            _ => Action::None,
        }
    }

    fn update(&mut self, action: &Action) {
        if let Action::DataLoaded(DataPayload::HooksData(data)) = action {
            self.set_data(data.clone());
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, spinner: &str) {
        if self.loading {
            let loading = Paragraph::new(format!(" {} Loading hooks...", spinner))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(theme::BORDER_ACTIVE))
                        .title(" Hooks "),
                )
                .style(Style::default().fg(theme::TEXT_MUTED));
            frame.render_widget(loading, area);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(area);

        self.render_phase_list(frame, chunks[0]);
        if self.show_reference {
            self.render_template_reference(frame, chunks[1]);
        } else {
            self.render_hook_detail(frame, chunks[1]);
        }
    }
}

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
    Frame,
};
use std::collections::{HashMap, HashSet};

use super::Component;
use crate::tui::action::*;
use crate::tui::theme;

// ── Tree data structures ────────────────────────────────────────────

/// A flattened tree row ready for rendering.
#[derive(Debug, Clone)]
struct TreeRow {
    workspace: EnrichedBranch,
    depth: usize,
    /// Whether this node is the last child at its level.
    is_last_sibling: bool,
    /// For each ancestor level, whether that ancestor has more siblings below.
    /// Used to draw the vertical continuation lines (│).
    ancestor_has_next: Vec<bool>,
    collapsed: bool,
    has_children: bool,
}

pub struct WorkspacesComponent {
    data: Option<BranchesData>,
    tree_rows: Vec<TreeRow>,
    list_state: ListState,
    selected_index: usize,
    filter: String,
    loading: bool,
    collapsed: HashSet<String>,
    service_focus: HashMap<String, usize>,
}

impl WorkspacesComponent {
    pub fn new() -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            data: None,
            tree_rows: Vec::new(),
            list_state,
            selected_index: 0,
            filter: String::new(),
            loading: true,
            collapsed: HashSet::new(),
            service_focus: HashMap::new(),
        }
    }

    pub fn set_data(&mut self, data: BranchesData) {
        for warning in &data.warnings {
            log::warn!("Workspace inventory: {warning}");
        }
        self.data = Some(data);
        self.loading = false;
        self.rebuild_tree();
        self.normalize_service_focus();
        // Try to select current workspace
        if let Some(idx) = self.tree_rows.iter().position(|r| r.workspace.is_current) {
            self.selected_index = idx;
            self.list_state.select(Some(idx));
        }
    }

    /// Build the flattened tree from the canonical inventory graph.
    fn rebuild_tree(&mut self) {
        self.tree_rows.clear();

        let data = match &self.data {
            Some(d) => d,
            None => return,
        };

        // Build a name->workspace lookup (clone data to avoid borrow conflict)
        let branches_owned: Vec<EnrichedBranch> = data.workspaces.clone();
        let branch_map: HashMap<&str, &EnrichedBranch> = branches_owned
            .iter()
            .map(|b| (b.name.as_str(), b))
            .collect();

        // Walk the canonical flat order from devflow-core (shared with the
        // CLI and GUI); collapse and filter are display concerns applied here.
        let filter_lower = self.filter.to_lowercase();
        let mut tree_rows = Vec::new();
        let mut collapse_below: Option<usize> = None;

        for row in &data.flat_order {
            if let Some(depth) = collapse_below {
                if row.depth > depth {
                    continue;
                }
                collapse_below = None;
            }
            let Some(workspace) = branch_map.get(row.name.as_str()) else {
                continue;
            };

            let is_collapsed = self.collapsed.contains(&row.name);
            let matches_filter =
                filter_lower.is_empty() || row.name.to_lowercase().contains(&filter_lower);

            if matches_filter || row.has_children {
                tree_rows.push(TreeRow {
                    workspace: (*workspace).clone(),
                    depth: row.depth,
                    is_last_sibling: row.is_last_sibling,
                    ancestor_has_next: row.ancestor_has_next.clone(),
                    collapsed: is_collapsed,
                    has_children: row.has_children,
                });
            }

            if is_collapsed && row.has_children {
                collapse_below = Some(row.depth);
            }
        }

        self.tree_rows = tree_rows;
        self.normalize_service_focus();
    }

    fn visible_rows(&self) -> &[TreeRow] {
        &self.tree_rows
    }

    fn selected_row(&self) -> Option<&TreeRow> {
        self.tree_rows.get(self.selected_index)
    }

    fn normalize_service_focus(&mut self) {
        // Drop stale entries for workspaces no longer present.
        let valid_branches: HashSet<&str> = self
            .tree_rows
            .iter()
            .map(|row| row.workspace.name.as_str())
            .collect();
        self.service_focus
            .retain(|workspace, _| valid_branches.contains(workspace.as_str()));

        // Clamp focused service index per workspace.
        for row in &self.tree_rows {
            let service_len = row.workspace.services.len();
            if service_len == 0 {
                self.service_focus.remove(&row.workspace.name);
                continue;
            }
            let idx = self
                .service_focus
                .entry(row.workspace.name.clone())
                .or_insert(0);
            if *idx >= service_len {
                *idx = service_len - 1;
            }
        }
    }

    fn selected_service_for_row<'a>(&'a self, row: &'a TreeRow) -> Option<&'a BranchServiceState> {
        if row.workspace.services.is_empty() {
            return None;
        }

        let idx = self
            .service_focus
            .get(&row.workspace.name)
            .copied()
            .unwrap_or(0);
        row.workspace
            .services
            .get(idx)
            .or_else(|| row.workspace.services.first())
    }

    fn cycle_service_focus(&mut self, delta: i32) {
        let Some(row) = self.selected_row() else {
            return;
        };

        let workspace_name = row.workspace.name.clone();
        let len = row.workspace.services.len();
        if len <= 1 {
            return;
        }

        let current = self
            .service_focus
            .get(&workspace_name)
            .copied()
            .unwrap_or(0) as i32;
        let next = (current + delta).rem_euclid(len as i32) as usize;
        self.service_focus.insert(workspace_name, next);
    }

    fn move_selection(&mut self, delta: i32) {
        let rows = self.tree_rows.len();
        if rows == 0 {
            return;
        }
        let new_idx = ((self.selected_index as i32 + delta).rem_euclid(rows as i32)) as usize;
        self.selected_index = new_idx;
        self.list_state.select(Some(new_idx));
    }

    fn toggle_collapse(&mut self) {
        if let Some(row) = self.tree_rows.get(self.selected_index) {
            if row.has_children {
                let name = row.workspace.name.clone();
                if self.collapsed.contains(&name) {
                    self.collapsed.remove(&name);
                } else {
                    self.collapsed.insert(name);
                }
                self.rebuild_tree();
                // Clamp selection
                if self.selected_index >= self.tree_rows.len() {
                    self.selected_index = self.tree_rows.len().saturating_sub(1);
                }
                self.list_state.select(Some(self.selected_index));
            }
        }
    }

    fn render_tree(&self, frame: &mut Frame, area: Rect) {
        let rows = self.visible_rows();

        let items: Vec<ListItem> = rows
            .iter()
            .map(|row| {
                let mut spans = Vec::new();

                // Draw tree lines
                if row.depth > 0 {
                    // Ancestor continuation lines
                    for &has_next in &row.ancestor_has_next {
                        if has_next {
                            spans.push(Span::styled("│  ", Style::default().fg(theme::TREE_LINE)));
                        } else {
                            spans.push(Span::raw("   "));
                        }
                    }
                    // This node's connector
                    if row.is_last_sibling {
                        spans.push(Span::styled("└──", Style::default().fg(theme::TREE_LINE)));
                    } else {
                        spans.push(Span::styled("├──", Style::default().fg(theme::TREE_LINE)));
                    }
                }

                // Collapse/expand indicator
                if row.has_children {
                    if row.collapsed {
                        spans.push(Span::styled(
                            "[+] ",
                            Style::default().fg(theme::TREE_COLLAPSED),
                        ));
                    } else {
                        spans.push(Span::styled("[-] ", Style::default().fg(theme::TREE_LINE)));
                    }
                } else if row.depth > 0 {
                    spans.push(Span::raw(" "));
                }

                // Current workspace indicator
                if row.workspace.is_current {
                    spans.push(Span::styled(
                        "* ",
                        Style::default().fg(theme::BRANCH_CURRENT).bold(),
                    ));
                }

                // Workspace name
                let name_style = if row.workspace.is_current {
                    Style::default().fg(theme::BRANCH_CURRENT).bold()
                } else if row.workspace.is_default {
                    Style::default().fg(theme::BRANCH_DEFAULT)
                } else {
                    Style::default().fg(theme::TEXT_PRIMARY)
                };
                spans.push(Span::styled(&row.workspace.name, name_style));

                // Service status badges
                if !row.workspace.services.is_empty() {
                    spans.push(Span::raw("  "));
                    for svc in &row.workspace.services {
                        let state_str = if svc.provisioned {
                            svc.state.as_deref().unwrap_or("unknown")
                        } else {
                            "not-provisioned"
                        };
                        let color = if svc.provisioned {
                            theme::state_color(state_str)
                        } else {
                            theme::TEXT_MUTED
                        };
                        spans.push(Span::styled(
                            format!("[{}:{}]", svc.service_name, state_str),
                            Style::default().fg(color),
                        ));
                    }
                }

                // Worktree path
                if let Some(ref wt) = row.workspace.worktree_path {
                    spans.push(Span::styled(
                        format!(" {}", wt),
                        Style::default().fg(theme::BRANCH_WORKTREE),
                    ));
                }

                ListItem::new(Line::from(spans))
            })
            .collect();

        let title = if self.filter.is_empty() {
            format!(" Environments ({}) ", rows.len())
        } else {
            format!(" Environments ({}) [filter: {}] ", rows.len(), self.filter)
        };

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::BORDER_ACTIVE))
                    .title(title),
            )
            .highlight_style(theme::highlight_style())
            .highlight_symbol(">> ");

        frame.render_stateful_widget(list, area, &mut self.list_state.clone());

        // Scrollbar
        let visible_height = area.height.saturating_sub(2) as usize;
        if rows.len() > visible_height {
            let mut scrollbar_state = ScrollbarState::new(rows.len())
                .position(self.selected_index)
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

    fn render_detail_panel(&self, frame: &mut Frame, area: Rect) {
        let row = self.selected_row();

        let content = match row {
            None => {
                vec![Line::styled(
                    "No environment selected",
                    Style::default().fg(theme::TEXT_MUTED),
                )]
            }
            Some(row) => {
                let workspace = &row.workspace;
                let mut lines = Vec::new();

                // Header
                lines.push(Line::from(vec![
                    Span::styled("Workspace: ", Style::default().fg(theme::TEXT_SECONDARY)),
                    Span::styled(
                        &workspace.name,
                        Style::default().fg(theme::TEXT_PRIMARY).bold(),
                    ),
                ]));

                // Current/default indicators
                if workspace.is_current {
                    lines.push(Line::styled(
                        "  (current workspace)",
                        Style::default().fg(theme::BRANCH_CURRENT),
                    ));
                }
                if workspace.is_default {
                    lines.push(Line::styled(
                        "  (default workspace)",
                        Style::default().fg(theme::BRANCH_DEFAULT),
                    ));
                }
                lines.push(Line::from(vec![
                    Span::styled("Health: ", Style::default().fg(theme::TEXT_SECONDARY)),
                    Span::styled(
                        &workspace.health,
                        if workspace.health == "ready" {
                            Style::default().fg(theme::BRANCH_CURRENT)
                        } else {
                            Style::default().fg(theme::TREE_COLLAPSED)
                        },
                    ),
                ]));
                // Parent
                if let Some(ref parent) = workspace.parent {
                    lines.push(Line::from(vec![
                        Span::styled("Parent: ", Style::default().fg(theme::TEXT_SECONDARY)),
                        Span::styled(parent, Style::default().fg(theme::VALUE_PARENT)),
                        Span::styled(
                            if workspace.parent_state.as_deref() == Some("missing") {
                                " (missing)"
                            } else {
                                ""
                            },
                            Style::default().fg(theme::TREE_COLLAPSED),
                        ),
                    ]));
                }

                lines.push(Line::raw(""));

                // Worktree
                lines.push(Line::from(vec![
                    Span::styled("Worktree: ", Style::default().fg(theme::TEXT_SECONDARY)),
                    Span::styled(
                        workspace.worktree_path.as_deref().unwrap_or("(none)"),
                        Style::default().fg(theme::VALUE_PATH),
                    ),
                ]));

                lines.push(Line::raw(""));

                // Services section
                if workspace.services.is_empty() {
                    lines.push(Line::styled(
                        "Services: (no service workspaces)",
                        Style::default().fg(theme::TEXT_MUTED),
                    ));
                } else {
                    lines.push(Line::styled(
                        "Services:",
                        Style::default().fg(theme::TEXT_SECONDARY),
                    ));
                    for svc in &workspace.services {
                        let state = if svc.provisioned {
                            svc.state.as_deref().unwrap_or("unknown")
                        } else {
                            "not provisioned"
                        };
                        let color = if svc.provisioned {
                            theme::state_color(state)
                        } else {
                            theme::TEXT_MUTED
                        };

                        lines.push(Line::from(vec![
                            Span::raw("  "),
                            Span::styled(
                                &svc.service_name,
                                Style::default().fg(theme::TEXT_PRIMARY),
                            ),
                            Span::raw(": "),
                            Span::styled(state, Style::default().fg(color)),
                        ]));

                        if let Some(ref db) = svc.database_name {
                            lines.push(Line::from(vec![
                                Span::raw("    db: "),
                                Span::styled(db, Style::default().fg(theme::VALUE_DATABASE)),
                            ]));
                        }
                    }

                    if let Some(selected_service) = self.selected_service_for_row(row) {
                        let focused_idx = self
                            .service_focus
                            .get(&workspace.name)
                            .copied()
                            .unwrap_or(0)
                            .saturating_add(1);
                        lines.push(Line::raw(""));
                        lines.push(Line::from(vec![
                            Span::styled(
                                "Focused service: ",
                                Style::default().fg(theme::TEXT_SECONDARY),
                            ),
                            Span::styled(
                                format!(
                                    "{} ({}/{})",
                                    selected_service.service_name,
                                    focused_idx,
                                    workspace.services.len()
                                ),
                                Style::default().fg(theme::SERVICE_TYPE).bold(),
                            ),
                        ]));
                        if workspace.services.len() > 1 {
                            lines.push(Line::styled(
                                "  n/p: cycle focused service",
                                Style::default().fg(theme::KEY_HINT),
                            ));
                        }
                    }
                }

                lines.push(Line::raw(""));

                // Processes section
                if workspace.processes.is_empty() {
                    lines.push(Line::styled(
                        "Processes: (none)",
                        Style::default().fg(theme::TEXT_MUTED),
                    ));
                } else {
                    lines.push(Line::styled(
                        "Processes:",
                        Style::default().fg(theme::TEXT_SECONDARY),
                    ));
                    for process in &workspace.processes {
                        let status_color = theme::state_color(&process.status);
                        let pid = process
                            .pid
                            .map(|pid| format!(" pid={pid}"))
                            .unwrap_or_default();
                        lines.push(Line::from(vec![
                            Span::raw("  "),
                            Span::styled(
                                &process.process,
                                Style::default().fg(theme::TEXT_PRIMARY),
                            ),
                            Span::raw(": "),
                            Span::styled(&process.status, Style::default().fg(status_color)),
                            Span::styled(pid, Style::default().fg(theme::TEXT_MUTED)),
                        ]));
                    }
                }

                lines.push(Line::raw(""));

                // Actions hint
                lines.push(Line::styled(
                    "Actions:",
                    Style::default().fg(theme::TEXT_SECONDARY),
                ));
                let has_any_service = self
                    .data
                    .as_ref()
                    .map(|d| d.workspaces.iter().any(|b| !b.services.is_empty()))
                    .unwrap_or(false);
                let has_lifecycle = self
                    .selected_service_for_row(row)
                    .map(|svc| svc.provisioned && svc.supports_lifecycle)
                    .unwrap_or(false);
                let has_any_lifecycle = workspace
                    .services
                    .iter()
                    .any(|svc| svc.provisioned && svc.supports_lifecycle);
                let enter_action = if workspace.is_current {
                    "Already on this workspace"
                } else if has_any_service {
                    "Align services to this workspace"
                } else {
                    "Align services (no services configured)"
                };
                let mut hint_lines = vec![
                    ("Enter", enter_action),
                    ("o", "Open workspace/worktree (exit TUI)"),
                ];
                if has_lifecycle {
                    hint_lines.extend([
                        ("S", "Start focused service"),
                        ("x", "Stop focused service"),
                        ("R", "Reset focused service"),
                        ("l", "Logs for focused service"),
                    ]);
                }
                if has_any_lifecycle {
                    hint_lines.extend([
                        ("A", "Start all provisioned services"),
                        ("X", "Stop all provisioned services"),
                    ]);
                }
                hint_lines.extend([("c", "Create child workspace"), ("d", "Delete workspace")]);
                if row.has_children {
                    hint_lines.push(("Space", "Collapse/expand"));
                }
                for (key, desc) in hint_lines {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {:8}", key), Style::default().fg(theme::KEY_HINT)),
                        Span::styled(desc, Style::default().fg(theme::TEXT_PRIMARY)),
                    ]));
                }

                lines
            }
        };

        let detail = Paragraph::new(content)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::BORDER_ACTIVE))
                    .title(" Details "),
            )
            .wrap(Wrap { trim: false });

        frame.render_widget(detail, area);
    }
}

impl Component for WorkspacesComponent {
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
            KeyCode::Home | KeyCode::Char('g') => {
                self.selected_index = 0;
                self.list_state.select(Some(0));
                Action::None
            }
            KeyCode::End | KeyCode::Char('G') => {
                if !self.tree_rows.is_empty() {
                    self.selected_index = self.tree_rows.len() - 1;
                    self.list_state.select(Some(self.selected_index));
                }
                Action::None
            }
            KeyCode::Char(' ') => {
                self.toggle_collapse();
                Action::None
            }
            KeyCode::Char('n') => {
                self.cycle_service_focus(1);
                Action::None
            }
            KeyCode::Char('p') => {
                self.cycle_service_focus(-1);
                Action::None
            }
            KeyCode::Enter => {
                if let Some(row) = self.selected_row() {
                    if !row.workspace.is_current {
                        Action::SwitchServices(row.workspace.name.clone())
                    } else {
                        Action::None
                    }
                } else {
                    Action::None
                }
            }
            KeyCode::Char('o') => {
                if let Some(row) = self.selected_row() {
                    Action::OpenBranchAndExit(row.workspace.name.clone())
                } else {
                    Action::None
                }
            }
            KeyCode::Char('c') => Action::ShowInput {
                title: self
                    .selected_row()
                    .map(|row| format!("Create new workspace (from: {})", row.workspace.name))
                    .unwrap_or_else(|| "Create new workspace".to_string()),
                on_submit: InputTarget::CreateBranch {
                    from: self.selected_row().map(|row| row.workspace.name.clone()),
                },
            },
            KeyCode::Char('d') => {
                if let Some(row) = self.selected_row() {
                    if !row.workspace.is_current && !row.workspace.is_default {
                        Action::ShowConfirm {
                            title: "Delete Workspace".to_string(),
                            message: format!(
                                "Delete workspace '{}' and all its service workspaces?",
                                row.workspace.name
                            ),
                            on_confirm: Box::new(Action::DeleteBranch {
                                name: row.workspace.name.clone(),
                                force: false,
                            }),
                        }
                    } else {
                        Action::None
                    }
                } else {
                    Action::None
                }
            }
            KeyCode::Char('S') => {
                if let Some(row) = self.selected_row() {
                    if let Some(svc) = self.selected_service_for_row(row) {
                        if svc.provisioned && svc.supports_lifecycle {
                            Action::StartService {
                                service: svc.service_name.clone(),
                                workspace: row.workspace.name.clone(),
                            }
                        } else if !svc.provisioned {
                            Action::Error(format!(
                                "Service '{}' is not provisioned for workspace '{}'",
                                svc.service_name, row.workspace.name
                            ))
                        } else {
                            Action::Error(format!(
                                "Service '{}' does not support lifecycle operations",
                                svc.service_name
                            ))
                        }
                    } else {
                        Action::Error(format!(
                            "No services attached to workspace '{}'",
                            row.workspace.name
                        ))
                    }
                } else {
                    Action::None
                }
            }
            KeyCode::Char('x') => {
                if let Some(row) = self.selected_row() {
                    if let Some(svc) = self.selected_service_for_row(row) {
                        if svc.provisioned && svc.supports_lifecycle {
                            Action::StopService {
                                service: svc.service_name.clone(),
                                workspace: row.workspace.name.clone(),
                            }
                        } else if !svc.provisioned {
                            Action::Error(format!(
                                "Service '{}' is not provisioned for workspace '{}'",
                                svc.service_name, row.workspace.name
                            ))
                        } else {
                            Action::Error(format!(
                                "Service '{}' does not support lifecycle operations",
                                svc.service_name
                            ))
                        }
                    } else {
                        Action::Error(format!(
                            "No services attached to workspace '{}'",
                            row.workspace.name
                        ))
                    }
                } else {
                    Action::None
                }
            }
            KeyCode::Char('A') => {
                if let Some(row) = self.selected_row() {
                    if self.services_for_branch(&row.workspace.name).is_empty() {
                        Action::Error(format!(
                            "No provisioned lifecycle services on workspace '{}'",
                            row.workspace.name
                        ))
                    } else {
                        Action::StartAllServices(row.workspace.name.clone())
                    }
                } else {
                    Action::None
                }
            }
            KeyCode::Char('X') => {
                if let Some(row) = self.selected_row() {
                    if self.services_for_branch(&row.workspace.name).is_empty() {
                        Action::Error(format!(
                            "No provisioned lifecycle services on workspace '{}'",
                            row.workspace.name
                        ))
                    } else {
                        Action::StopAllServices(row.workspace.name.clone())
                    }
                } else {
                    Action::None
                }
            }
            KeyCode::Char('R') => {
                if let Some(row) = self.selected_row() {
                    if let Some(svc) = self.selected_service_for_row(row) {
                        if svc.provisioned && svc.supports_lifecycle {
                            Action::ShowConfirm {
                                title: "Reset Service".to_string(),
                                message: format!(
                                    "Reset '{}' on {}? This will restore it to its parent state.",
                                    row.workspace.name, svc.service_name
                                ),
                                on_confirm: Box::new(Action::ResetService {
                                    service: svc.service_name.clone(),
                                    workspace: row.workspace.name.clone(),
                                }),
                            }
                        } else if !svc.provisioned {
                            Action::Error(format!(
                                "Service '{}' is not provisioned for workspace '{}'",
                                svc.service_name, row.workspace.name
                            ))
                        } else {
                            Action::Error(format!(
                                "Service '{}' does not support lifecycle operations",
                                svc.service_name
                            ))
                        }
                    } else {
                        Action::Error(format!(
                            "No services attached to workspace '{}'",
                            row.workspace.name
                        ))
                    }
                } else {
                    Action::None
                }
            }
            KeyCode::Char('l') => {
                if let Some(row) = self.selected_row() {
                    if let Some(svc) = self.selected_service_for_row(row) {
                        if svc.provisioned && svc.supports_lifecycle {
                            Action::ViewLogs {
                                service: svc.service_name.clone(),
                                workspace: row.workspace.name.clone(),
                            }
                        } else if !svc.provisioned {
                            Action::Error(format!(
                                "Service '{}' is not provisioned for workspace '{}'",
                                svc.service_name, row.workspace.name
                            ))
                        } else {
                            Action::Error(format!(
                                "Service '{}' does not support logs",
                                svc.service_name
                            ))
                        }
                    } else {
                        Action::Error(format!(
                            "No services attached to workspace '{}'",
                            row.workspace.name
                        ))
                    }
                } else {
                    Action::None
                }
            }
            KeyCode::Char('/') => Action::ShowInput {
                title: "Filter environments".to_string(),
                on_submit: InputTarget::FilterBranches,
            },
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.rebuild_tree();
                    self.selected_index = 0;
                    self.list_state.select(Some(0));
                }
                Action::None
            }
            KeyCode::Char('r') => Action::Refresh,
            _ => Action::None,
        }
    }

    fn update(&mut self, action: &Action) {
        if let Action::DataLoaded(DataPayload::Branches(data)) = action {
            self.set_data(data.clone());
        }
    }

    fn render(&self, frame: &mut Frame, area: Rect, spinner: &str) {
        if self.loading {
            let loading = Paragraph::new(format!(" {} Loading environments...", spinner))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(theme::BORDER_ACTIVE))
                        .title(" Environments "),
                )
                .style(Style::default().fg(theme::TEXT_MUTED));
            frame.render_widget(loading, area);
            return;
        }

        // No-config detection: if no workspaces loaded, show helpful message
        if self.data.is_some() && self.tree_rows.is_empty() && self.filter.is_empty() {
            let msg = Paragraph::new(vec![
                Line::raw(""),
                Line::styled(
                    " No devflow project found.",
                    Style::default().fg(theme::TEXT_PRIMARY).bold(),
                ),
                Line::raw(""),
                Line::styled(
                    " Run 'devflow init' to get started.",
                    Style::default().fg(theme::TEXT_SECONDARY),
                ),
                Line::raw(""),
                Line::styled(
                    " Press 'c' to create a workspace, or 'q' to quit.",
                    Style::default().fg(theme::TEXT_MUTED),
                ),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::BORDER_ACTIVE))
                    .title(" Environments "),
            );
            frame.render_widget(msg, area);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(area);

        self.render_tree(frame, chunks[0]);
        self.render_detail_panel(frame, chunks[1]);
    }

    fn on_focus(&mut self) {}

    fn on_blur(&mut self) {}
}

impl WorkspacesComponent {
    pub fn services_for_branch(&self, workspace_name: &str) -> Vec<String> {
        let mut names = Vec::new();

        let workspaces = match &self.data {
            Some(data) => &data.workspaces,
            None => return names,
        };

        if let Some(workspace) = workspaces.iter().find(|b| b.name == workspace_name) {
            for svc in &workspace.services {
                if svc.provisioned
                    && svc.supports_lifecycle
                    && !names.iter().any(|n| n == &svc.service_name)
                {
                    names.push(svc.service_name.clone());
                }
            }
        }

        names
    }

    pub fn set_filter(&mut self, filter: String) {
        self.filter = filter;
        self.rebuild_tree();
        self.selected_index = 0;
        self.list_state.select(Some(0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(
        name: &str,
        children: &[&str],
        services: Vec<BranchServiceState>,
    ) -> EnrichedBranch {
        EnrichedBranch {
            name: name.to_string(),
            is_current: name == "root",
            is_default: name == "root",
            worktree_path: Some(format!("/tmp/{name}")),
            health: "ready".to_string(),
            services,
            processes: Vec::new(),
            parent: (name != "root").then(|| "root".to_string()),
            parent_state: (name != "root").then(|| "present".to_string()),
            children: children.iter().map(|child| (*child).to_string()).collect(),
        }
    }

    #[test]
    fn tree_uses_inventory_roots_and_children() {
        let mut component = WorkspacesComponent::new();
        component.set_data(BranchesData {
            roots: vec!["root".to_string()],
            workspaces: vec![
                workspace("child", &[], Vec::new()),
                workspace("root", &["child"], Vec::new()),
            ],
            warnings: Vec::new(),
        });

        let rows = component.visible_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].workspace.name, "root");
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[1].workspace.name, "child");
        assert_eq!(rows[1].depth, 1);
    }

    #[test]
    fn bulk_lifecycle_excludes_unprovisioned_templates() {
        let provisioned = BranchServiceState {
            service_name: "database".to_string(),
            state: Some("running".to_string()),
            database_name: Some("db".to_string()),
            provisioned: true,
            supports_lifecycle: true,
        };
        let template = BranchServiceState {
            service_name: "cache".to_string(),
            state: None,
            database_name: None,
            provisioned: false,
            supports_lifecycle: true,
        };
        let mut component = WorkspacesComponent::new();
        component.set_data(BranchesData {
            roots: vec!["root".to_string()],
            workspaces: vec![workspace("root", &[], vec![template, provisioned])],
            warnings: Vec::new(),
        });

        assert_eq!(
            component.services_for_branch("root"),
            vec!["database".to_string()]
        );
    }
}

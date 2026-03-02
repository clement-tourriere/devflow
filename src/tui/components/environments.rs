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
use std::collections::{HashMap, HashSet};

use super::Component;
use crate::tui::action::*;
use crate::tui::theme;

// ── Tree data structures ────────────────────────────────────────────

/// A flattened tree row ready for rendering.
#[derive(Debug, Clone)]
struct TreeRow {
    branch: EnrichedBranch,
    depth: usize,
    /// Whether this node is the last child at its level.
    is_last_sibling: bool,
    /// For each ancestor level, whether that ancestor has more siblings below.
    /// Used to draw the vertical continuation lines (│).
    ancestor_has_next: Vec<bool>,
    collapsed: bool,
    has_children: bool,
}

pub struct EnvironmentsComponent {
    data: Option<BranchesData>,
    tree_rows: Vec<TreeRow>,
    list_state: ListState,
    selected_index: usize,
    filter: String,
    loading: bool,
    collapsed: HashSet<String>,
    service_focus: HashMap<String, usize>,
}

impl EnvironmentsComponent {
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
        self.data = Some(data);
        self.loading = false;
        self.rebuild_tree();
        self.normalize_service_focus();
        // Try to select current branch
        if let Some(idx) = self.tree_rows.iter().position(|r| r.branch.is_current) {
            self.selected_index = idx;
            self.list_state.select(Some(idx));
        }
    }

    /// Build the flattened tree from branch data.
    /// Uses parent info from EnrichedBranch.services (parent_branch field)
    /// and from the branch registry.
    fn rebuild_tree(&mut self) {
        self.tree_rows.clear();

        let data = match &self.data {
            Some(d) => d,
            None => return,
        };

        // Build parent map: branch_name -> parent_name
        let mut parent_map: HashMap<String, String> = HashMap::new();

        for branch in &data.branches {
            // Check service-level parent info
            for svc in &branch.services {
                if let Some(ref parent) = svc.parent_branch {
                    parent_map
                        .entry(branch.name.clone())
                        .or_insert_with(|| parent.clone());
                }
            }
            // Registry parent takes precedence
            if let Some(ref parent) = branch.parent {
                parent_map.insert(branch.name.clone(), parent.clone());
            }
        }

        // Build children map
        let mut children_map: HashMap<String, Vec<String>> = HashMap::new();
        let all_names: HashSet<String> = data.branches.iter().map(|b| b.name.clone()).collect();

        for branch in &data.branches {
            if let Some(parent) = parent_map.get(&branch.name) {
                if all_names.contains(parent) {
                    children_map
                        .entry(parent.clone())
                        .or_default()
                        .push(branch.name.clone());
                }
            }
        }

        // Find root nodes (no parent, or parent not in our branch list)
        let mut roots: Vec<EnrichedBranch> = data
            .branches
            .iter()
            .filter(|b| match parent_map.get(&b.name) {
                None => true,
                Some(parent) => !all_names.contains(parent),
            })
            .cloned()
            .collect();

        // Sort: default branch first, then current, then alphabetical
        roots.sort_by(|a, b| {
            if a.is_default != b.is_default {
                return b.is_default.cmp(&a.is_default);
            }
            if a.is_current != b.is_current {
                return b.is_current.cmp(&a.is_current);
            }
            a.name.cmp(&b.name)
        });

        // Build a name->branch lookup (clone data to avoid borrow conflict)
        let branches_owned: Vec<EnrichedBranch> = data.branches.clone();
        let branch_map: HashMap<&str, &EnrichedBranch> = branches_owned
            .iter()
            .map(|b| (b.name.as_str(), b))
            .collect();

        // Flatten the tree via DFS
        let collapsed = &self.collapsed;
        let filter = &self.filter;
        let mut tree_rows = Vec::new();

        for (i, root) in roots.iter().enumerate() {
            let is_last = i == roots.len() - 1;
            Self::flatten_node_static(
                root,
                0,
                is_last,
                &[],
                &children_map,
                &branch_map,
                collapsed,
                filter,
                &mut tree_rows,
            );
        }

        self.tree_rows = tree_rows;
        self.normalize_service_focus();
    }

    fn flatten_node_static(
        branch: &EnrichedBranch,
        depth: usize,
        is_last_sibling: bool,
        ancestor_has_next: &[bool],
        children_map: &HashMap<String, Vec<String>>,
        branch_map: &HashMap<&str, &EnrichedBranch>,
        collapsed: &HashSet<String>,
        filter: &str,
        tree_rows: &mut Vec<TreeRow>,
    ) {
        let children = children_map.get(&branch.name);
        let has_children = children.map_or(false, |c| !c.is_empty());
        let is_collapsed = collapsed.contains(&branch.name);

        // Apply filter
        let matches_filter =
            filter.is_empty() || branch.name.to_lowercase().contains(&filter.to_lowercase());

        if matches_filter || has_children {
            tree_rows.push(TreeRow {
                branch: branch.clone(),
                depth,
                is_last_sibling,
                ancestor_has_next: ancestor_has_next.to_vec(),
                collapsed: is_collapsed,
                has_children,
            });
        }

        // Recurse into children if not collapsed
        if has_children && !is_collapsed {
            let child_names = children.unwrap();
            for (i, child_name) in child_names.iter().enumerate() {
                if let Some(child_branch) = branch_map.get(child_name.as_str()) {
                    let child_is_last = i == child_names.len() - 1;
                    let mut child_ancestors = ancestor_has_next.to_vec();
                    child_ancestors.push(!is_last_sibling);
                    Self::flatten_node_static(
                        child_branch,
                        depth + 1,
                        child_is_last,
                        &child_ancestors,
                        children_map,
                        branch_map,
                        collapsed,
                        filter,
                        tree_rows,
                    );
                }
            }
        }
    }

    fn visible_rows(&self) -> &[TreeRow] {
        &self.tree_rows
    }

    fn selected_row(&self) -> Option<&TreeRow> {
        self.tree_rows.get(self.selected_index)
    }

    fn normalize_service_focus(&mut self) {
        // Drop stale entries for branches no longer present.
        let valid_branches: HashSet<&str> = self
            .tree_rows
            .iter()
            .map(|row| row.branch.name.as_str())
            .collect();
        self.service_focus
            .retain(|branch, _| valid_branches.contains(branch.as_str()));

        // Clamp focused service index per branch.
        for row in &self.tree_rows {
            let service_len = row.branch.services.len();
            if service_len == 0 {
                self.service_focus.remove(&row.branch.name);
                continue;
            }
            let idx = self
                .service_focus
                .entry(row.branch.name.clone())
                .or_insert(0);
            if *idx >= service_len {
                *idx = service_len - 1;
            }
        }
    }

    fn selected_service_for_row<'a>(&'a self, row: &'a TreeRow) -> Option<&'a BranchServiceState> {
        if row.branch.services.is_empty() {
            return None;
        }

        let idx = self
            .service_focus
            .get(&row.branch.name)
            .copied()
            .unwrap_or(0);
        row.branch
            .services
            .get(idx)
            .or_else(|| row.branch.services.first())
    }

    fn cycle_service_focus(&mut self, delta: i32) {
        let Some(row) = self.selected_row() else {
            return;
        };

        let branch_name = row.branch.name.clone();
        let len = row.branch.services.len();
        if len <= 1 {
            return;
        }

        let current = self.service_focus.get(&branch_name).copied().unwrap_or(0) as i32;
        let next = (current + delta).rem_euclid(len as i32) as usize;
        self.service_focus.insert(branch_name, next);
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
                let name = row.branch.name.clone();
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

                // Current branch indicator
                if row.branch.is_current {
                    spans.push(Span::styled(
                        "* ",
                        Style::default().fg(theme::BRANCH_CURRENT).bold(),
                    ));
                }

                // Branch name
                let name_style = if row.branch.is_current {
                    Style::default().fg(theme::BRANCH_CURRENT).bold()
                } else if row.branch.is_default {
                    Style::default().fg(theme::BRANCH_DEFAULT)
                } else {
                    Style::default().fg(theme::TEXT_PRIMARY)
                };
                spans.push(Span::styled(&row.branch.name, name_style));

                // Service status badges
                if !row.branch.services.is_empty() {
                    spans.push(Span::raw("  "));
                    for svc in &row.branch.services {
                        let state_str = svc.state.as_deref().unwrap_or("?");
                        let color = theme::state_color(state_str);
                        spans.push(Span::styled(
                            format!("[{}:{}]", svc.service_name, state_str),
                            Style::default().fg(color),
                        ));
                    }
                }

                // Worktree path
                if let Some(ref wt) = row.branch.worktree_path {
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
                let branch = &row.branch;
                let mut lines = Vec::new();

                // Header
                lines.push(Line::from(vec![
                    Span::styled("Branch: ", Style::default().fg(theme::TEXT_SECONDARY)),
                    Span::styled(
                        &branch.name,
                        Style::default().fg(theme::TEXT_PRIMARY).bold(),
                    ),
                ]));

                // Current/default indicators
                if branch.is_current {
                    lines.push(Line::styled(
                        "  (current branch)",
                        Style::default().fg(theme::BRANCH_CURRENT),
                    ));
                }
                if branch.is_default {
                    lines.push(Line::styled(
                        "  (default/main branch)",
                        Style::default().fg(theme::BRANCH_DEFAULT),
                    ));
                }

                // Parent
                if let Some(ref parent) = branch.parent {
                    lines.push(Line::from(vec![
                        Span::styled("Parent: ", Style::default().fg(theme::TEXT_SECONDARY)),
                        Span::styled(parent, Style::default().fg(theme::VALUE_PARENT)),
                    ]));
                }

                lines.push(Line::raw(""));

                // Worktree
                lines.push(Line::from(vec![
                    Span::styled("Worktree: ", Style::default().fg(theme::TEXT_SECONDARY)),
                    Span::styled(
                        branch.worktree_path.as_deref().unwrap_or("(none)"),
                        Style::default().fg(theme::VALUE_PATH),
                    ),
                ]));

                lines.push(Line::raw(""));

                // Services section
                if branch.services.is_empty() {
                    lines.push(Line::styled(
                        "Services: (no service branches)",
                        Style::default().fg(theme::TEXT_MUTED),
                    ));
                } else {
                    lines.push(Line::styled(
                        "Services:",
                        Style::default().fg(theme::TEXT_SECONDARY),
                    ));
                    for svc in &branch.services {
                        let state = svc.state.as_deref().unwrap_or("unknown");
                        let color = theme::state_color(state);

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
                        if let Some(ref parent) = svc.parent_branch {
                            lines.push(Line::from(vec![
                                Span::raw("    parent: "),
                                Span::styled(parent, Style::default().fg(theme::VALUE_PARENT)),
                            ]));
                        }
                    }

                    if let Some(selected_service) = self.selected_service_for_row(row) {
                        let focused_idx = self
                            .service_focus
                            .get(&branch.name)
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
                                    branch.services.len()
                                ),
                                Style::default().fg(theme::SERVICE_TYPE).bold(),
                            ),
                        ]));
                        if branch.services.len() > 1 {
                            lines.push(Line::styled(
                                "  n/p: cycle focused service",
                                Style::default().fg(theme::KEY_HINT),
                            ));
                        }
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
                    .map(|d| d.branches.iter().any(|b| !b.services.is_empty()))
                    .unwrap_or(false);
                let enter_action = if branch.is_current {
                    "Already on this branch"
                } else if has_any_service {
                    "Align services to this branch"
                } else {
                    "Align services (no services configured)"
                };
                let mut hint_lines = vec![
                    ("Enter", enter_action),
                    ("o", "Open branch/worktree (exit TUI)"),
                    ("S", "Start focused service"),
                    ("x", "Stop focused service"),
                    ("R", "Reset focused service"),
                    ("A", "Start all services"),
                    ("X", "Stop all services"),
                    ("l", "Logs for focused service"),
                    ("c", "Create child branch"),
                    ("d", "Delete branch"),
                ];
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

impl Component for EnvironmentsComponent {
    fn title(&self) -> &str {
        "Environments"
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
                    if !row.branch.is_current {
                        Action::SwitchServices(row.branch.name.clone())
                    } else {
                        Action::None
                    }
                } else {
                    Action::None
                }
            }
            KeyCode::Char('o') => {
                if let Some(row) = self.selected_row() {
                    Action::OpenBranchAndExit(row.branch.name.clone())
                } else {
                    Action::None
                }
            }
            KeyCode::Char('c') => Action::ShowInput {
                title: self
                    .selected_row()
                    .map(|row| format!("Create new branch (from: {})", row.branch.name))
                    .unwrap_or_else(|| "Create new branch".to_string()),
                on_submit: InputTarget::CreateBranch {
                    from: self.selected_row().map(|row| row.branch.name.clone()),
                },
            },
            KeyCode::Char('d') => {
                if let Some(row) = self.selected_row() {
                    if !row.branch.is_current && !row.branch.is_default {
                        Action::ShowConfirm {
                            title: "Delete Branch".to_string(),
                            message: format!(
                                "Delete branch '{}' and all its service branches?",
                                row.branch.name
                            ),
                            on_confirm: Box::new(Action::DeleteBranch(row.branch.name.clone())),
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
                        Action::StartService {
                            service: svc.service_name.clone(),
                            branch: row.branch.name.clone(),
                        }
                    } else {
                        Action::Error(format!(
                            "No services attached to branch '{}'",
                            row.branch.name
                        ))
                    }
                } else {
                    Action::None
                }
            }
            KeyCode::Char('x') => {
                if let Some(row) = self.selected_row() {
                    if let Some(svc) = self.selected_service_for_row(row) {
                        Action::StopService {
                            service: svc.service_name.clone(),
                            branch: row.branch.name.clone(),
                        }
                    } else {
                        Action::Error(format!(
                            "No services attached to branch '{}'",
                            row.branch.name
                        ))
                    }
                } else {
                    Action::None
                }
            }
            KeyCode::Char('A') => {
                if let Some(row) = self.selected_row() {
                    Action::StartAllServices(row.branch.name.clone())
                } else {
                    Action::None
                }
            }
            KeyCode::Char('X') => {
                if let Some(row) = self.selected_row() {
                    Action::StopAllServices(row.branch.name.clone())
                } else {
                    Action::None
                }
            }
            KeyCode::Char('R') => {
                if let Some(row) = self.selected_row() {
                    if let Some(svc) = self.selected_service_for_row(row) {
                        Action::ShowConfirm {
                            title: "Reset Service".to_string(),
                            message: format!(
                                "Reset '{}' on {}? This will restore it to its parent state.",
                                row.branch.name, svc.service_name
                            ),
                            on_confirm: Box::new(Action::ResetService {
                                service: svc.service_name.clone(),
                                branch: row.branch.name.clone(),
                            }),
                        }
                    } else {
                        Action::Error(format!(
                            "No services attached to branch '{}'",
                            row.branch.name
                        ))
                    }
                } else {
                    Action::None
                }
            }
            KeyCode::Char('l') => {
                if let Some(row) = self.selected_row() {
                    if let Some(svc) = self.selected_service_for_row(row) {
                        Action::ViewLogs {
                            service: svc.service_name.clone(),
                            branch: row.branch.name.clone(),
                        }
                    } else {
                        Action::Error(format!(
                            "No services attached to branch '{}'",
                            row.branch.name
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

impl EnvironmentsComponent {
    pub fn services_for_branch(&self, branch_name: &str) -> Vec<String> {
        let mut names = Vec::new();

        let branches = match &self.data {
            Some(data) => &data.branches,
            None => return names,
        };

        if let Some(branch) = branches.iter().find(|b| b.name == branch_name) {
            for svc in &branch.services {
                if !names.iter().any(|n| n == &svc.service_name) {
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

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

    /// Build the flattened tree from workspace data.
    /// Uses parent info from EnrichedBranch.services (parent_workspace field)
    /// and from the workspace registry.
    fn rebuild_tree(&mut self) {
        self.tree_rows.clear();

        let data = match &self.data {
            Some(d) => d,
            None => return,
        };

        // Build parent map: workspace_name -> parent_name
        let mut parent_map: HashMap<String, String> = HashMap::new();

        for workspace in &data.workspaces {
            // Check service-level parent info
            for svc in &workspace.services {
                if let Some(ref parent) = svc.parent_workspace {
                    parent_map
                        .entry(workspace.name.clone())
                        .or_insert_with(|| parent.clone());
                }
            }
            // Registry parent takes precedence
            if let Some(ref parent) = workspace.parent {
                parent_map.insert(workspace.name.clone(), parent.clone());
            }
        }

        // Build children map
        let mut children_map: HashMap<String, Vec<String>> = HashMap::new();
        let all_names: HashSet<String> = data.workspaces.iter().map(|b| b.name.clone()).collect();

        for workspace in &data.workspaces {
            if let Some(parent) = parent_map.get(&workspace.name) {
                if all_names.contains(parent) {
                    children_map
                        .entry(parent.clone())
                        .or_default()
                        .push(workspace.name.clone());
                }
            }
        }

        // Find root nodes (no parent, or parent not in our workspace list)
        let mut roots: Vec<EnrichedBranch> = data
            .workspaces
            .iter()
            .filter(|b| match parent_map.get(&b.name) {
                None => true,
                Some(parent) => !all_names.contains(parent),
            })
            .cloned()
            .collect();

        // Sort: default workspace first, then current, then alphabetical
        roots.sort_by(|a, b| {
            if a.is_default != b.is_default {
                return b.is_default.cmp(&a.is_default);
            }
            if a.is_current != b.is_current {
                return b.is_current.cmp(&a.is_current);
            }
            a.name.cmp(&b.name)
        });

        // Build a name->workspace lookup (clone data to avoid borrow conflict)
        let branches_owned: Vec<EnrichedBranch> = data.workspaces.clone();
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
        workspace: &EnrichedBranch,
        depth: usize,
        is_last_sibling: bool,
        ancestor_has_next: &[bool],
        children_map: &HashMap<String, Vec<String>>,
        branch_map: &HashMap<&str, &EnrichedBranch>,
        collapsed: &HashSet<String>,
        filter: &str,
        tree_rows: &mut Vec<TreeRow>,
    ) {
        let children = children_map.get(&workspace.name);
        let has_children = children.is_some_and(|c| !c.is_empty());
        let is_collapsed = collapsed.contains(&workspace.name);

        // Apply filter
        let matches_filter = filter.is_empty()
            || workspace
                .name
                .to_lowercase()
                .contains(&filter.to_lowercase());

        if matches_filter || has_children {
            tree_rows.push(TreeRow {
                workspace: workspace.clone(),
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
                        let state_str = svc.state.as_deref().unwrap_or("?");
                        let color = theme::state_color(state_str);
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
                        "  (default/main workspace)",
                        Style::default().fg(theme::BRANCH_DEFAULT),
                    ));
                }

                // Parent
                if let Some(ref parent) = workspace.parent {
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
                        if let Some(ref parent) = svc.parent_workspace {
                            lines.push(Line::from(vec![
                                Span::raw("    parent: "),
                                Span::styled(parent, Style::default().fg(theme::VALUE_PARENT)),
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
                    .map(|svc| svc.supports_lifecycle)
                    .unwrap_or(false);
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
                        ("A", "Start all services"),
                        ("X", "Stop all services"),
                        ("l", "Logs for focused service"),
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
                            on_confirm: Box::new(Action::DeleteBranch(row.workspace.name.clone())),
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
                        if svc.supports_lifecycle {
                            Action::StartService {
                                service: svc.service_name.clone(),
                                workspace: row.workspace.name.clone(),
                            }
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
                        if svc.supports_lifecycle {
                            Action::StopService {
                                service: svc.service_name.clone(),
                                workspace: row.workspace.name.clone(),
                            }
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
                    Action::StartAllServices(row.workspace.name.clone())
                } else {
                    Action::None
                }
            }
            KeyCode::Char('X') => {
                if let Some(row) = self.selected_row() {
                    Action::StopAllServices(row.workspace.name.clone())
                } else {
                    Action::None
                }
            }
            KeyCode::Char('R') => {
                if let Some(row) = self.selected_row() {
                    if let Some(svc) = self.selected_service_for_row(row) {
                        if svc.supports_lifecycle {
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
                        if svc.supports_lifecycle {
                            Action::ViewLogs {
                                service: svc.service_name.clone(),
                                workspace: row.workspace.name.clone(),
                            }
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
            KeyCode::Char('m') => {
                if let Some(row) = self.selected_row() {
                    if !row.workspace.is_default {
                        let default = self
                            .tree_rows
                            .iter()
                            .find(|r| r.workspace.is_default)
                            .map(|r| r.workspace.name.clone())
                            .unwrap_or_else(|| "main".to_string());
                        Action::ShowConfirm {
                            title: "Merge Workspace".to_string(),
                            message: format!("Merge '{}' into '{}'?", row.workspace.name, default),
                            on_confirm: Box::new(Action::MergeWorkspace {
                                source: row.workspace.name.clone(),
                                target: default,
                            }),
                        }
                    } else {
                        Action::Error("Cannot merge the default workspace".to_string())
                    }
                } else {
                    Action::None
                }
            }
            KeyCode::Char('b') => {
                if let Some(row) = self.selected_row() {
                    if !row.workspace.is_default {
                        let default = self
                            .tree_rows
                            .iter()
                            .find(|r| r.workspace.is_default)
                            .map(|r| r.workspace.name.clone())
                            .unwrap_or_else(|| "main".to_string());
                        Action::ShowConfirm {
                            title: "Rebase Workspace".to_string(),
                            message: format!("Rebase '{}' onto '{}'?", row.workspace.name, default),
                            on_confirm: Box::new(Action::RebaseWorkspace {
                                source: row.workspace.name.clone(),
                                target: default,
                            }),
                        }
                    } else {
                        Action::Error("Cannot rebase the default workspace".to_string())
                    }
                } else {
                    Action::None
                }
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

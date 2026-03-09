use std::path::PathBuf;

use anyhow::{Context, Result};
use devflow_core::config::Config;
use devflow_core::hooks::HookPhase;
use devflow_core::services::{self};
use devflow_core::state::{DevflowWorkspace, LocalStateManager};
use devflow_core::vcs;

// ── Branch context ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BranchContextSource {
    EnvOverride,
    Cwd,
    None,
}

#[derive(Debug, Clone)]
pub(crate) struct BranchContext {
    /// Raw workspace used as context (env override or cwd workspace).
    pub(crate) context_branch_raw: Option<String>,
    /// Normalized devflow context workspace name.
    pub(crate) context_branch: Option<String>,
    /// Raw VCS workspace currently checked out in this directory.
    pub(crate) cwd_branch: Option<String>,
    pub(crate) source: BranchContextSource,
}

pub(crate) fn resolve_branch_context(config: &Config) -> BranchContext {
    let cwd_branch = vcs::detect_vcs_provider(".")
        .ok()
        .and_then(|repo| repo.current_workspace().ok().flatten());

    if let Ok(env_branch) = std::env::var("DEVFLOW_CONTEXT_BRANCH") {
        let trimmed = env_branch.trim();
        if !trimmed.is_empty() {
            return BranchContext {
                context_branch_raw: Some(trimmed.to_string()),
                context_branch: Some(config.get_normalized_workspace_name(trimmed)),
                cwd_branch,
                source: BranchContextSource::EnvOverride,
            };
        }
    }

    if let Some(cwd) = cwd_branch.as_deref() {
        return BranchContext {
            context_branch_raw: Some(cwd.to_string()),
            context_branch: Some(config.get_normalized_workspace_name(cwd)),
            cwd_branch,
            source: BranchContextSource::Cwd,
        };
    }

    BranchContext {
        context_branch_raw: None,
        context_branch: None,
        cwd_branch: None,
        source: BranchContextSource::None,
    }
}

pub(crate) fn context_matches_branch(
    config: &Config,
    context_branch: Option<&str>,
    workspace_name: &str,
) -> bool {
    let Some(context) = context_branch else {
        return false;
    };
    context == workspace_name || context == config.get_normalized_workspace_name(workspace_name)
}

pub(super) fn linked_workspace_exists(
    config: &Config,
    config_path: &Option<PathBuf>,
    workspace_name: &str,
) -> bool {
    let Some(path) = config_path.as_ref() else {
        return false;
    };

    let normalized = config.get_normalized_workspace_name(workspace_name);
    LocalStateManager::new()
        .ok()
        .and_then(|state| state.get_workspace(path, &normalized))
        .is_some()
}

pub(super) fn register_workspace_in_state(
    config: &Config,
    config_path: &Option<PathBuf>,
    workspace_name: &str,
    parent_workspace: Option<&str>,
    worktree_path: Option<String>,
) -> Result<()> {
    let Some(path) = config_path.as_ref() else {
        return Ok(());
    };

    let mut state = LocalStateManager::new()?;
    let normalized_branch = config.get_normalized_workspace_name(workspace_name);
    let normalized_parent = parent_workspace.map(|p| config.get_normalized_workspace_name(p));

    let existing = state.get_workspace(path, &normalized_branch);
    let created_at = existing
        .as_ref()
        .map(|b| b.created_at)
        .unwrap_or_else(chrono::Utc::now);

    let final_parent =
        normalized_parent.or_else(|| existing.as_ref().and_then(|b| b.parent.clone()));
    let final_worktree = worktree_path.or_else(|| {
        existing
            .as_ref()
            .and_then(|b| b.worktree_path.as_ref().cloned())
    });

    state.register_workspace(
        path,
        DevflowWorkspace {
            name: normalized_branch,
            parent: final_parent,
            worktree_path: final_worktree,
            created_at,
            executed_command: None,
            execution_status: None,
            executed_at: None,
            sandboxed: existing.as_ref().map(|b| b.sandboxed).unwrap_or(false),
        },
    )?;

    Ok(())
}

pub(crate) fn ensure_default_workspace_registered(
    config: &Config,
    config_path: &Option<PathBuf>,
) -> Result<()> {
    let main = config.git.main_workspace.clone();
    if !linked_workspace_exists(config, config_path, &main) {
        register_workspace_in_state(config, config_path, &main, None, None)?;
    }
    Ok(())
}

pub(crate) fn load_registry_branches_for_list(
    config: &Config,
    config_path: &Option<PathBuf>,
) -> Vec<DevflowWorkspace> {
    let Some(config_file) = config_path.as_ref() else {
        return Vec::new();
    };
    let Some(project_dir) = config_file.parent() else {
        return Vec::new();
    };

    let Ok(mut state) = LocalStateManager::new() else {
        return Vec::new();
    };

    state
        .get_or_init_workspaces_by_dir(project_dir, &config.git.main_workspace)
        .unwrap_or_else(|_| state.get_workspaces(config_file))
}

pub(crate) fn collect_list_workspace_names(
    registry_branches: &[DevflowWorkspace],
    git_branches: &[devflow_core::vcs::WorkspaceInfo],
    service_branches: &[services::WorkspaceInfo],
) -> Vec<String> {
    if !registry_branches.is_empty() {
        return registry_branches.iter().map(|b| b.name.clone()).collect();
    }

    let mut all_names: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for gb in git_branches {
        if seen.insert(gb.name.clone()) {
            all_names.push(gb.name.clone());
        }
    }
    for sb in service_branches {
        if seen.insert(sb.name.clone()) {
            all_names.push(sb.name.clone());
        }
    }

    all_names
}

/// Print an enriched workspace list as a tree, showing git workspaces, worktree paths, and service status.
///
/// Unifies information from the VCS provider, the service provider, and the
/// workspace registry (for parent-child relationships) into a single tree view.
fn print_enriched_branch_list(
    service_branches: &[services::WorkspaceInfo],
    config: &Config,
    config_path: &Option<PathBuf>,
) {
    use std::collections::{HashMap, HashSet};

    // Gather VCS + worktree info
    let vcs_provider = vcs::detect_vcs_provider(".").ok();
    let git_branches: Vec<devflow_core::vcs::WorkspaceInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_workspaces().ok())
        .unwrap_or_default();
    let worktrees: Vec<devflow_core::vcs::WorktreeInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_worktrees().ok())
        .unwrap_or_default();
    let current_git = vcs_provider
        .as_ref()
        .and_then(|r| r.current_workspace().ok().flatten());
    let current_normalized = current_git
        .as_deref()
        .map(|b| config.get_normalized_workspace_name(b));

    // Build a set of service workspace names for quick lookup
    let mut service_names: HashSet<String> = HashSet::new();
    for b in service_branches {
        service_names.insert(b.name.clone());
        service_names.insert(config.get_normalized_workspace_name(&b.name));
    }

    // Build a worktree lookup: workspace name -> path
    let mut wt_lookup: HashMap<String, PathBuf> = HashMap::new();
    for wt in &worktrees {
        if let Some(workspace) = wt.workspace.as_ref() {
            wt_lookup.insert(workspace.clone(), wt.path.clone());
            wt_lookup
                .entry(config.get_normalized_workspace_name(workspace))
                .or_insert_with(|| wt.path.clone());
        }
    }

    // Load workspace registry from local state
    let registry_branches = load_registry_branches_for_list(config, config_path);
    let registry: HashMap<String, Option<String>> = registry_branches
        .iter()
        .map(|b| (b.name.clone(), b.parent.clone()))
        .collect();
    let sandbox_lookup: HashSet<String> = registry_branches
        .iter()
        .filter(|b| b.sandboxed)
        .map(|b| b.name.clone())
        .collect();

    let context = resolve_branch_context(config);

    // Registry-first scope: align CLI with GUI/TUI workspace model.
    let all_names =
        collect_list_workspace_names(&registry_branches, &git_branches, service_branches);
    let seen: HashSet<&str> = all_names.iter().map(|s| s.as_str()).collect();

    if all_names.is_empty() {
        println!("  (none)");
        return;
    }

    // Build parent map: child_name -> parent_name
    // Sources: 1) service-level parent, 2) registry parent (takes precedence)
    let mut parent_map: HashMap<&str, &str> = HashMap::new();

    for sb in service_branches {
        if !seen.contains(sb.name.as_str()) {
            continue;
        }
        if let Some(ref parent) = sb.parent_workspace {
            if seen.contains(parent.as_str()) {
                parent_map.insert(sb.name.as_str(), parent.as_str());
            }
        }
    }
    for name in &all_names {
        if let Some(Some(ref parent)) = registry.get(name.as_str()) {
            if seen.contains(parent.as_str()) {
                parent_map.insert(name.as_str(), parent.as_str());
            }
        }
    }

    // Build children map
    let mut children_map: HashMap<&str, Vec<&str>> = HashMap::new();
    for (child, parent) in &parent_map {
        children_map.entry(parent).or_default().push(child);
    }
    // Sort children alphabetically for deterministic output
    for kids in children_map.values_mut() {
        kids.sort();
    }

    // Find root nodes (no parent, or parent not in the known set)
    let mut roots: Vec<&str> = all_names
        .iter()
        .filter(|name| !parent_map.contains_key(name.as_str()))
        .map(|s| s.as_str())
        .collect();

    // Sort roots: default workspace first, then context workspace, then cwd, then alphabetical
    let default_workspace = config.get_normalized_workspace_name(&config.git.main_workspace);
    roots.sort_by(|a, b| {
        let a_default = *a == default_workspace
            || git_branches.iter().any(|gb| {
                gb.is_default
                    && (gb.name == *a || config.get_normalized_workspace_name(&gb.name) == *a)
            });
        let b_default = *b == default_workspace
            || git_branches.iter().any(|gb| {
                gb.is_default
                    && (gb.name == *b || config.get_normalized_workspace_name(&gb.name) == *b)
            });
        if a_default != b_default {
            return b_default.cmp(&a_default);
        }
        let a_context = context_matches_branch(config, context.context_branch.as_deref(), a);
        let b_context = context_matches_branch(config, context.context_branch.as_deref(), b);
        if a_context != b_context {
            return b_context.cmp(&a_context);
        }
        let a_current =
            current_git.as_deref() == Some(*a) || current_normalized.as_deref() == Some(*a);
        let b_current =
            current_git.as_deref() == Some(*b) || current_normalized.as_deref() == Some(*b);
        if a_current != b_current {
            return b_current.cmp(&a_current);
        }
        a.cmp(b)
    });

    if context.source == BranchContextSource::EnvOverride {
        if let Some(context_branch) = context.context_branch.as_deref() {
            let cwd = context.cwd_branch.as_deref().unwrap_or("unknown");
            println!(
                "Context override: '{}' (from DEVFLOW_CONTEXT_BRANCH), cwd workspace='{}'",
                context_branch, cwd
            );
        }
    }

    // Recursive tree printer
    #[allow(clippy::too_many_arguments)]
    fn print_node(
        name: &str,
        prefix: &str,
        connector: &str,
        children_map: &HashMap<&str, Vec<&str>>,
        current_git: &Option<String>,
        current_normalized: &Option<String>,
        context_branch: Option<&str>,
        service_branches: &[services::WorkspaceInfo],
        service_names: &HashSet<String>,
        wt_lookup: &HashMap<String, PathBuf>,
        sandbox_lookup: &HashSet<String>,
        config: &Config,
        #[allow(unused_variables)] _git_branches: &[devflow_core::vcs::WorkspaceInfo],
    ) {
        let is_current =
            current_git.as_deref() == Some(name) || current_normalized.as_deref() == Some(name);
        let marker = if is_current { "* " } else { "  " };
        let is_context = context_matches_branch(config, context_branch, name);

        let normalized = config.get_normalized_workspace_name(name);
        let has_service = service_names.contains(&normalized) || service_names.contains(name);

        let service_state = service_branches
            .iter()
            .find(|b| b.name == normalized || b.name == name)
            .and_then(|b| b.state.as_deref());

        let wt_path = wt_lookup.get(name);
        let is_sandboxed = sandbox_lookup.contains(name) || sandbox_lookup.contains(&normalized);

        let mut parts = Vec::new();
        if let Some(state) = service_state {
            parts.push(format!("service: {}", state));
        } else if has_service {
            parts.push("service: ok".to_string());
        }
        if let Some(path) = wt_path {
            parts.push(format!("worktree: {}", path.display()));
        }
        if is_context {
            parts.push("context".to_string());
        }
        if is_sandboxed {
            parts.push("sandboxed".to_string());
        }

        let suffix = if parts.is_empty() {
            String::new()
        } else {
            format!("  [{}]", parts.join(", "))
        };

        if connector.is_empty() {
            println!("{}{}{}", marker, name, suffix);
        } else {
            println!("{}{}{}{}", marker, connector, name, suffix);
        }

        if let Some(kids) = children_map.get(name) {
            let count = kids.len();
            for (i, child) in kids.iter().enumerate() {
                let is_last = i == count - 1;
                let child_connector = if is_last {
                    format!("{}└─ ", prefix)
                } else {
                    format!("{}├─ ", prefix)
                };
                let child_prefix = if is_last {
                    format!("{}   ", prefix)
                } else {
                    format!("{}│  ", prefix)
                };
                print_node(
                    child,
                    &child_prefix,
                    &child_connector,
                    children_map,
                    current_git,
                    current_normalized,
                    context_branch,
                    service_branches,
                    service_names,
                    wt_lookup,
                    sandbox_lookup,
                    config,
                    _git_branches,
                );
            }
        }
    }

    for root in &roots {
        print_node(
            root,
            "  ",
            "",
            &children_map,
            &current_git,
            &current_normalized,
            context.context_branch.as_deref(),
            service_branches,
            &service_names,
            &wt_lookup,
            &sandbox_lookup,
            config,
            &git_branches,
        );
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct EnvGraphServiceEntry {
    service_name: String,
    provider_name: String,
    state: Option<String>,
    database_name: String,
    parent_workspace: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct EnvGraphNode {
    name: String,
    parent: Option<String>,
    is_default: bool,
    is_cwd: bool,
    is_context: bool,
    worktree_path: Option<String>,
    services: Vec<EnvGraphServiceEntry>,
}

async fn handle_environment_graph(
    config: &Config,
    config_path: &Option<PathBuf>,
    json_output: bool,
) -> Result<()> {
    use std::collections::{HashMap, HashSet};

    // VCS view
    let vcs_provider = vcs::detect_vcs_provider(".").ok();
    let vcs_provider_name = vcs_provider
        .as_ref()
        .map(|p| p.provider_name().to_string())
        .unwrap_or_else(|| "none".to_string());
    let git_branches: Vec<devflow_core::vcs::WorkspaceInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_workspaces().ok())
        .unwrap_or_default();
    let worktrees: Vec<devflow_core::vcs::WorktreeInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_worktrees().ok())
        .unwrap_or_default();
    let cwd_branch = vcs_provider
        .as_ref()
        .and_then(|r| r.current_workspace().ok().flatten());

    // Local state view (workspace registry only)
    let registry_branches = load_registry_branches_for_list(config, config_path);
    let registry: HashMap<String, Option<String>> = registry_branches
        .into_iter()
        .map(|b| (b.name, b.parent))
        .collect();

    let context = resolve_branch_context(config);

    // Service view
    let mut service_entries_by_branch: HashMap<String, Vec<EnvGraphServiceEntry>> = HashMap::new();
    let mut service_probe_warnings: Vec<String> = Vec::new();
    match services::factory::create_all_providers(config).await {
        Ok(all_providers) => {
            for named in &all_providers {
                let provider_name = named.provider.provider_name().to_string();
                match named.provider.list_workspaces().await {
                    Ok(workspaces) => {
                        for b in workspaces {
                            service_entries_by_branch
                                .entry(b.name.clone())
                                .or_default()
                                .push(EnvGraphServiceEntry {
                                    service_name: named.name.clone(),
                                    provider_name: provider_name.clone(),
                                    state: b.state.clone(),
                                    database_name: b.database_name.clone(),
                                    parent_workspace: b.parent_workspace.clone(),
                                });
                        }
                    }
                    Err(e) => {
                        service_probe_warnings
                            .push(format!("{} ({}): {}", named.name, provider_name, e));
                    }
                }
            }
        }
        Err(e) => {
            service_probe_warnings.push(format!("provider initialization failed: {}", e));
        }
    }

    let wt_lookup: HashMap<String, PathBuf> = worktrees
        .iter()
        .filter_map(|wt| wt.workspace.as_ref().map(|b| (b.clone(), wt.path.clone())))
        .collect();

    // Union of all known workspace names
    let mut all_names: Vec<String> = Vec::new();
    let mut seen = HashSet::new();

    for gb in &git_branches {
        if seen.insert(gb.name.clone()) {
            all_names.push(gb.name.clone());
        }
    }
    for name in registry.keys() {
        if seen.insert(name.clone()) {
            all_names.push(name.clone());
        }
    }
    for name in service_entries_by_branch.keys() {
        if seen.insert(name.clone()) {
            all_names.push(name.clone());
        }
    }

    if all_names.is_empty() {
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "vcs_provider": vcs_provider_name,
                    "nodes": [],
                    "roots": [],
                    "cwd_branch": cwd_branch,
                    "context_branch": context.context_branch.clone(),
                    "context_source": match context.source {
                        BranchContextSource::EnvOverride => "env",
                        BranchContextSource::Cwd => "cwd",
                        BranchContextSource::None => "none",
                    },
                    "warnings": service_probe_warnings,
                }))?
            );
        } else {
            println!("Environment graph: (empty)");
        }
        return Ok(());
    }

    // Parent map with precedence: registry > service workspace parent
    let mut parent_map: HashMap<String, String> = HashMap::new();

    for (name, entries) in &service_entries_by_branch {
        if let Some(parent) = entries.iter().find_map(|e| e.parent_workspace.clone()) {
            if seen.contains(parent.as_str()) {
                parent_map.insert(name.clone(), parent);
            }
        }
    }

    for (name, parent) in &registry {
        if let Some(parent_name) = parent {
            if seen.contains(parent_name.as_str()) {
                parent_map.insert(name.clone(), parent_name.clone());
            }
        }
    }

    let mut children_map: HashMap<String, Vec<String>> = HashMap::new();
    for (child, parent) in &parent_map {
        children_map
            .entry(parent.clone())
            .or_default()
            .push(child.clone());
    }
    for kids in children_map.values_mut() {
        kids.sort();
    }

    // Roots
    let mut roots: Vec<String> = all_names
        .iter()
        .filter(|name| !parent_map.contains_key(name.as_str()))
        .cloned()
        .collect();

    let cwd_normalized = cwd_branch
        .as_deref()
        .map(|b| config.get_normalized_workspace_name(b));

    roots.sort_by(|a, b| {
        let a_default = git_branches.iter().any(|gb| gb.name == *a && gb.is_default);
        let b_default = git_branches.iter().any(|gb| gb.name == *b && gb.is_default);
        if a_default != b_default {
            return b_default.cmp(&a_default);
        }

        let a_context = context_matches_branch(config, context.context_branch.as_deref(), a);
        let b_context = context_matches_branch(config, context.context_branch.as_deref(), b);
        if a_context != b_context {
            return b_context.cmp(&a_context);
        }

        let a_cwd =
            cwd_branch.as_deref() == Some(a.as_str()) || cwd_normalized.as_deref() == Some(a);
        let b_cwd =
            cwd_branch.as_deref() == Some(b.as_str()) || cwd_normalized.as_deref() == Some(b);
        if a_cwd != b_cwd {
            return b_cwd.cmp(&a_cwd);
        }

        a.cmp(b)
    });

    // Build node map for JSON and human rendering
    let mut node_map: HashMap<String, EnvGraphNode> = HashMap::new();
    for name in &all_names {
        let normalized = config.get_normalized_workspace_name(name);

        let mut services = Vec::new();
        if let Some(entries) = service_entries_by_branch.get(name) {
            services.extend(entries.iter().cloned());
        }
        if normalized != *name {
            if let Some(entries) = service_entries_by_branch.get(&normalized) {
                for entry in entries {
                    if !services
                        .iter()
                        .any(|e| e.service_name == entry.service_name)
                    {
                        services.push(entry.clone());
                    }
                }
            }
        }
        services.sort_by(|a, b| a.service_name.cmp(&b.service_name));

        let is_cwd =
            cwd_branch.as_deref() == Some(name.as_str()) || cwd_normalized.as_deref() == Some(name);
        let is_context = context_matches_branch(config, context.context_branch.as_deref(), name);
        let is_default = git_branches
            .iter()
            .any(|gb| gb.name == *name && gb.is_default);

        node_map.insert(
            name.clone(),
            EnvGraphNode {
                name: name.clone(),
                parent: parent_map.get(name).cloned(),
                is_default,
                is_cwd,
                is_context,
                worktree_path: wt_lookup
                    .get(name)
                    .map(|p| p.display().to_string())
                    .or_else(|| {
                        wt_lookup
                            .iter()
                            .find(|(workspace, _)| {
                                config.get_normalized_workspace_name(workspace) == *name
                            })
                            .map(|(_, p)| p.display().to_string())
                    }),
                services,
            },
        );
    }

    if json_output {
        let mut nodes: Vec<EnvGraphNode> = node_map.values().cloned().collect();
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "vcs_provider": vcs_provider_name,
                "cwd_branch": cwd_branch,
                "context_branch": context.context_branch.clone(),
                "context_source": match context.source {
                    BranchContextSource::EnvOverride => "env",
                    BranchContextSource::Cwd => "cwd",
                    BranchContextSource::None => "none",
                },
                "roots": roots,
                "nodes": nodes,
                "warnings": service_probe_warnings,
            }))?
        );
        return Ok(());
    }

    println!("Environment graph ({})", vcs_provider_name);
    if let Some(context_branch) = context.context_branch.as_deref() {
        println!("Context workspace: {}", context_branch);
    }
    if let Some(cwd) = cwd_branch.as_deref() {
        println!("CWD workspace: {}", cwd);
    }
    if !service_probe_warnings.is_empty() {
        println!("Warnings:");
        for warning in &service_probe_warnings {
            println!("  - {}", warning);
        }
    }

    fn print_node(
        name: &str,
        prefix: &str,
        connector: &str,
        children_map: &std::collections::HashMap<String, Vec<String>>,
        node_map: &std::collections::HashMap<String, EnvGraphNode>,
    ) {
        let Some(node) = node_map.get(name) else {
            return;
        };

        let marker = if node.is_cwd { "* " } else { "  " };
        let mut tags = Vec::new();
        if node.is_default {
            tags.push("default".to_string());
        }
        if node.is_context {
            tags.push("context".to_string());
        }
        if let Some(path) = &node.worktree_path {
            tags.push(format!("worktree: {}", path));
        }

        if tags.is_empty() {
            println!("{}{}{}", marker, connector, node.name);
        } else {
            println!(
                "{}{}{}  [{}]",
                marker,
                connector,
                node.name,
                tags.join(", ")
            );
        }

        for svc in &node.services {
            let state = svc.state.as_deref().unwrap_or("unknown");
            let mut parts = vec![format!("{}:{}", svc.service_name, state)];
            parts.push(format!("provider: {}", svc.provider_name));
            parts.push(format!("db: {}", svc.database_name));
            if let Some(parent) = &svc.parent_workspace {
                parts.push(format!("parent: {}", parent));
            }
            println!("{}   • {}", prefix, parts.join(", "));
        }

        if let Some(kids) = children_map.get(name) {
            let count = kids.len();
            for (i, child) in kids.iter().enumerate() {
                let is_last = i == count - 1;
                let child_connector = if is_last {
                    format!("{}└─ ", prefix)
                } else {
                    format!("{}├─ ", prefix)
                };
                let child_prefix = if is_last {
                    format!("{}   ", prefix)
                } else {
                    format!("{}│  ", prefix)
                };
                print_node(
                    child,
                    &child_prefix,
                    &child_connector,
                    children_map,
                    node_map,
                );
            }
        }
    }

    for root in &roots {
        print_node(root, "", "", &children_map, &node_map);
    }

    Ok(())
}

/// Build enriched JSON for the list command, merging git + worktree + service info.
fn enrich_branch_list_json(
    service_branches: &[services::WorkspaceInfo],
    config: &Config,
    config_path: &Option<PathBuf>,
) -> serde_json::Value {
    let vcs_provider = vcs::detect_vcs_provider(".").ok();
    let git_branches: Vec<devflow_core::vcs::WorkspaceInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_workspaces().ok())
        .unwrap_or_default();
    let worktrees: Vec<devflow_core::vcs::WorktreeInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_worktrees().ok())
        .unwrap_or_default();
    let current_git = vcs_provider
        .as_ref()
        .and_then(|r| r.current_workspace().ok().flatten());
    let current_normalized = current_git
        .as_deref()
        .map(|b| config.get_normalized_workspace_name(b));

    let mut wt_lookup: std::collections::HashMap<String, PathBuf> =
        std::collections::HashMap::new();
    for wt in &worktrees {
        if let Some(workspace) = wt.workspace.as_ref() {
            wt_lookup.insert(workspace.clone(), wt.path.clone());
            wt_lookup
                .entry(config.get_normalized_workspace_name(workspace))
                .or_insert_with(|| wt.path.clone());
        }
    }

    let mut service_map: std::collections::HashMap<String, &services::WorkspaceInfo> =
        std::collections::HashMap::new();
    for b in service_branches {
        service_map.entry(b.name.clone()).or_insert(b);
        service_map
            .entry(config.get_normalized_workspace_name(&b.name))
            .or_insert(b);
    }

    let registry_branches = load_registry_branches_for_list(config, config_path);
    let registry: std::collections::HashMap<String, Option<String>> = registry_branches
        .iter()
        .map(|b| (b.name.clone(), b.parent.clone()))
        .collect();

    let context = resolve_branch_context(config);

    let mut entries = Vec::new();

    let all_names =
        collect_list_workspace_names(&registry_branches, &git_branches, service_branches);
    let default_workspace = config.get_normalized_workspace_name(&config.git.main_workspace);

    for name in &all_names {
        let normalized = config.get_normalized_workspace_name(name);
        let sb = service_map
            .get(name)
            .or_else(|| service_map.get(&normalized))
            .copied();
        let wt = wt_lookup.get(name).or_else(|| wt_lookup.get(&normalized));
        let is_context = context_matches_branch(config, context.context_branch.as_deref(), name);
        let is_current = current_git.as_deref() == Some(name.as_str())
            || current_normalized.as_deref() == Some(name.as_str());
        let is_default = *name == default_workspace
            || git_branches.iter().any(|gb| {
                gb.is_default
                    && (gb.name == *name || config.get_normalized_workspace_name(&gb.name) == *name)
            });

        let mut entry = serde_json::json!({
            "name": name,
            "is_current": is_current,
            "is_default": is_default,
            "is_context": is_context,
        });

        if let Some(svc) = sb {
            entry["service"] = serde_json::json!({
                "database": svc.database_name,
                "state": svc.state,
                "parent": svc.parent_workspace,
            });
        }

        if let Some(path) = wt {
            entry["worktree_path"] = serde_json::Value::String(path.display().to_string());
        }

        // Parent from registry (preferred) or service
        let parent = registry
            .get(name)
            .and_then(|p| p.clone())
            .or_else(|| registry.get(&normalized).and_then(|p| p.clone()))
            .or_else(|| sb.and_then(|s| s.parent_workspace.clone()));
        if let Some(parent_name) = parent {
            entry["parent"] = serde_json::Value::String(parent_name);
        }

        entries.push(entry);
    }

    serde_json::Value::Array(entries)
}

// ── Main dispatcher ────────────────────────────────────────────────────────────

pub(super) async fn handle_branch_command(
    cmd: super::Commands,
    config: &mut Config,
    json_output: bool,
    non_interactive: bool,
    database_name: Option<&str>,
    config_path: &Option<std::path::PathBuf>,
) -> Result<()> {
    match cmd {
        super::Commands::List => {
            // List: show combined VCS + service workspace info
            let has_multiple_services = config.resolve_services().len() > 1;
            if database_name.is_none() && has_multiple_services {
                return super::service::handle_multi_service_aggregation(
                    super::service::ServiceAggregation::List,
                    config,
                    json_output,
                    config_path,
                )
                .await;
            }

            // Try to resolve a service provider; if none is available we
            // still show VCS workspaces with an empty service workspace list.
            let (provider_name, workspaces) =
                match services::factory::resolve_provider(config, database_name).await {
                    Ok(named) => {
                        let workspaces = named.provider.list_workspaces().await?;
                        (named.provider.provider_name().to_string(), workspaces)
                    }
                    Err(_) => {
                        // No service provider available — still show VCS workspaces.
                        ("none".to_string(), Vec::new())
                    }
                };

            if json_output {
                let enriched = enrich_branch_list_json(&workspaces, config, config_path);
                println!("{}", serde_json::to_string_pretty(&enriched)?);
            } else {
                if provider_name == "none" {
                    println!("Branches (no service configured):");
                } else {
                    println!("Branches ({}):", provider_name);
                }
                print_enriched_branch_list(&workspaces, config, config_path);
            }
        }
        super::Commands::Graph => {
            handle_environment_graph(config, config_path, json_output).await?;
        }
        super::Commands::Link {
            workspace_name,
            from,
        } => {
            handle_link_command(
                config,
                config_path,
                &workspace_name,
                from.as_deref(),
                json_output,
                non_interactive,
            )
            .await?;
        }
        super::Commands::Switch {
            workspace_name,
            create,
            from,
            execute,
            detach,
            open,
            execute_args,
            no_services,
            no_verify,
            template,
            dry_run,
            no_respect_gitignore,
            sandboxed,
            no_sandbox,
        } => {
            let sandbox_resolved = if sandboxed || no_sandbox {
                Some(devflow_core::sandbox::resolve_sandbox_enabled(
                    sandboxed,
                    no_sandbox,
                    false,
                    config.sandbox.as_ref(),
                ))
            } else {
                let is_sandboxed = devflow_core::sandbox::resolve_sandbox_enabled(
                    false,
                    false,
                    false,
                    config.sandbox.as_ref(),
                );
                if is_sandboxed {
                    Some(true)
                } else {
                    None
                }
            };

            if dry_run {
                if let Some(ref workspace) = workspace_name {
                    let normalized_branch = config.get_normalized_workspace_name(workspace);
                    let worktree_enabled = config.worktree.as_ref().is_some_and(|wt| wt.enabled);
                    let context = resolve_branch_context(config);
                    let default_parent = if create {
                        from.clone().or_else(|| context.context_branch_raw.clone())
                    } else {
                        None
                    };
                    let workspace_exists = vcs::detect_vcs_provider(".")
                        .ok()
                        .and_then(|repo| repo.workspace_exists(workspace).ok());

                    let project_dir = config_path
                        .as_ref()
                        .and_then(|p| p.parent())
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| PathBuf::from("."));

                    if json_output {
                        let mut wt_path_value = serde_json::Value::Null;
                        if worktree_enabled {
                            let wt_path = super::config::resolve_cd_target(
                                &devflow_core::workspace::worktree::resolve_worktree_path(
                                    config,
                                    &project_dir,
                                    &normalized_branch,
                                ),
                            )?;
                            wt_path_value =
                                serde_json::Value::String(wt_path.display().to_string());
                        }
                        let auto_providers: Vec<serde_json::Value> = if !no_services {
                            config
                                .resolve_services()
                                .into_iter()
                                .filter(|b| b.auto_workspace)
                                .map(|b| {
                                    serde_json::json!({
                                        "name": b.name,
                                        "service_type": b.service_type,
                                    })
                                })
                                .collect()
                        } else {
                            vec![]
                        };
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "dry_run": true,
                                "workspace": normalized_branch,
                                "worktree_enabled": worktree_enabled,
                                "worktree_path": wt_path_value,
                                "parent": default_parent,
                                "workspace_exists": workspace_exists,
                                "services_skipped": no_services,
                                "auto_branch_services": auto_providers,
                                "hooks_skipped": no_verify,
                                "execute": execute,
                                "would_fail_without_create": workspace_exists == Some(false) && !create,
                            }))?
                        );
                    } else {
                        println!("Dry run: would switch to workspace: {}", normalized_branch);
                        if let Some(ref parent) = default_parent {
                            println!("  Parent workspace: {}", parent);
                        }
                        if workspace_exists == Some(false) && !create {
                            println!(
                                "  Note: workspace does not exist; this would fail (use -c to create it)"
                            );
                        }
                        if worktree_enabled {
                            println!("  Worktree mode: enabled");
                            let wt_path = super::config::resolve_cd_target(
                                &devflow_core::workspace::worktree::resolve_worktree_path(
                                    config,
                                    &project_dir,
                                    &normalized_branch,
                                ),
                            )?;
                            println!("  Worktree path: {}", wt_path.display());
                        }
                        if !no_services {
                            let auto_providers = config
                                .resolve_services()
                                .into_iter()
                                .filter(|b| b.auto_workspace)
                                .collect::<Vec<_>>();
                            if auto_providers.is_empty() {
                                println!(
                                    "  Would not switch any service workspaces (none configured)"
                                );
                            } else {
                                println!(
                                    "  Would create/switch service workspaces on {} service(s):",
                                    auto_providers.len()
                                );
                                for b in &auto_providers {
                                    println!("    - {} ({})", b.name, b.service_type);
                                }
                            }
                        }
                        if !no_verify && config.hooks.is_some() {
                            println!("  Would run post-switch hooks");
                        }
                        if let Some(ref cmd) = execute {
                            println!("  Would execute after switch: {}", cmd);
                        }
                    }
                } else {
                    anyhow::bail!("Dry run requires a workspace name");
                }
            } else if template {
                handle_switch_to_main(
                    config,
                    config_path,
                    json_output,
                    no_services,
                    no_verify,
                    non_interactive,
                    None,
                    None,
                )
                .await?;
            } else if let Some(ref workspace) = workspace_name {
                if workspace == &config.git.main_workspace {
                    handle_switch_to_main(
                        config,
                        config_path,
                        json_output,
                        no_services,
                        no_verify,
                        non_interactive,
                        None,
                        None,
                    )
                    .await?;
                } else {
                    handle_switch_command(
                        config,
                        workspace,
                        config_path,
                        create,
                        from.as_deref(),
                        no_services,
                        no_verify,
                        json_output,
                        non_interactive,
                        None,
                        None,
                        if no_respect_gitignore {
                            Some(true)
                        } else {
                            None
                        },
                        sandbox_resolved,
                    )
                    .await?;
                }
            } else if non_interactive {
                anyhow::bail!(
                    "No workspace specified. Use 'devflow switch <workspace>' in non-interactive mode."
                );
            } else {
                handle_interactive_switch(config, config_path).await?;
            }

            // Execute command or open interactive session in workspace
            if open || execute.is_some() {
                let workspace = workspace_name
                    .as_deref()
                    .unwrap_or(&config.git.main_workspace);
                let cmd = execute.as_deref().unwrap_or("");
                execute_in_workspace(
                    config,
                    config_path,
                    workspace,
                    cmd,
                    &execute_args,
                    detach || open,
                    sandbox_resolved,
                    json_output,
                )
                .await?;
            }
        }
        super::Commands::Remove {
            workspace_name,
            force,
            keep_services,
        } => {
            handle_remove_command(
                config,
                &workspace_name,
                force,
                keep_services,
                config_path,
                json_output,
                non_interactive,
            )
            .await?;
        }
        super::Commands::Merge {
            target,
            cleanup,
            dry_run,
            force,
            check_only,
            cascade_rebase,
        } => {
            handle_merge_command(
                config,
                target.as_deref(),
                cleanup,
                dry_run,
                json_output,
                force,
                check_only,
                cascade_rebase,
            )
            .await?;
        }
        super::Commands::Rebase { target, dry_run } => {
            handle_rebase_command(config, target.as_deref(), dry_run, json_output).await?;
        }
        super::Commands::Train { action } => {
            super::train::handle_train_command(config, action, json_output).await?;
        }
        super::Commands::Cleanup { max_count } => {
            // Top-level alias for `devflow service cleanup`
            return super::service::handle_service_provider_command(
                super::ServiceCommands::Cleanup { max_count },
                config,
                json_output,
                non_interactive,
                database_name,
                config_path,
            )
            .await;
        }
        super::Commands::Doctor => {
            // Run pre-checks (VCS, config, hooks) unconditionally — they never fail
            if !json_output {
                super::config::run_doctor_pre_checks(config, config_path);
            }
            let has_multiple_services = config.resolve_services().len() > 1;
            if database_name.is_none() && has_multiple_services {
                return super::service::handle_multi_service_aggregation(
                    super::service::ServiceAggregation::Doctor,
                    config,
                    json_output,
                    config_path,
                )
                .await;
            }
            // Service-specific doctor report is optional
            match services::factory::resolve_provider(config, database_name).await {
                Ok(named) => {
                    let report = named.provider.doctor().await?;
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "general": {
                                    "config_path": config_path.as_ref().map(|p| p.display().to_string()),
                                },
                                "service": report,
                            }))?
                        );
                    } else {
                        println!("Service ({}):", named.provider.provider_name());
                        for check in &report.checks {
                            let icon = if check.available { "OK" } else { "FAIL" };
                            println!("  [{}] {}: {}", icon, check.name, check.detail);
                        }
                    }
                }
                Err(_) => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "general": {
                                    "config_path": config_path.as_ref().map(|p| p.display().to_string()),
                                },
                                "services": null,
                            }))?
                        );
                    } else {
                        println!("Services:");
                        println!("  [WARN] No service provider available (run 'devflow service add' to configure one)");
                    }
                }
            }
        }
        super::Commands::GitHook {
            worktree,
            main_worktree_dir,
        } => {
            super::git_hook::handle_git_hook(config, config_path, worktree, main_worktree_dir)
                .await?;
        }
        super::Commands::WorktreeSetup => {
            super::git_hook::handle_worktree_setup(config, config_path).await?;
        }
        _ => unreachable!(),
    }

    Ok(())
}

// ── Interactive switch ─────────────────────────────────────────────────────────

async fn handle_interactive_switch(
    config: &Config,
    config_path: &Option<std::path::PathBuf>,
) -> Result<()> {
    let mut workspace_names = std::collections::BTreeSet::new();
    let mut vcs_workspace_names = std::collections::HashSet::new();

    // 1) VCS workspaces (authoritative source)
    if let Ok(vcs_repo) = vcs::detect_vcs_provider(".") {
        if let Ok(vcs_branches) = vcs_repo.list_workspaces() {
            for workspace in vcs_branches {
                vcs_workspace_names.insert(workspace.name.clone());
                workspace_names.insert(workspace.name);
            }
        }
    }

    // 2) Devflow workspace registry
    if let Some(path) = config_path.as_ref() {
        if let Ok(state) = LocalStateManager::new() {
            for workspace in state.get_workspaces(path) {
                if vcs_workspace_names.is_empty() || vcs_workspace_names.contains(&workspace.name) {
                    workspace_names.insert(workspace.name);
                }
            }
        }
    }

    // 3) Service workspaces (best effort)
    if !config.resolve_services().is_empty() {
        if let Ok(providers) = services::factory::create_all_providers(config).await {
            for named in providers {
                if let Ok(service_branches) = named.provider.list_workspaces().await {
                    for workspace in service_branches {
                        if vcs_workspace_names.is_empty()
                            || vcs_workspace_names.contains(&workspace.name)
                        {
                            workspace_names.insert(workspace.name);
                        }
                    }
                }
            }
        }
    }

    // Include configured main workspace when visible in VCS (or if VCS probing failed).
    if vcs_workspace_names.is_empty() || vcs_workspace_names.contains(&config.git.main_workspace) {
        workspace_names.insert(config.git.main_workspace.clone());
    }

    let context = resolve_branch_context(config);
    let current_git = context.cwd_branch.clone();

    // Create workspace items with display info
    let mut branch_items: Vec<BranchItem> = workspace_names
        .iter()
        .map(|workspace| {
            let is_cwd = current_git.as_deref() == Some(workspace.as_str());
            let is_context =
                context_matches_branch(config, context.context_branch.as_deref(), workspace);

            BranchItem {
                name: workspace.clone(),
                display_name: workspace.clone(),
                is_cwd,
                is_context,
            }
        })
        .collect();

    // Add a "Create new workspace" option at the end
    branch_items.push(BranchItem {
        name: "__create_new__".to_string(),
        display_name: "+ Create new workspace".to_string(),
        is_cwd: false,
        is_context: false,
    });

    // Run interactive selector
    match run_interactive_selector(branch_items) {
        Ok(selected_branch) => {
            if selected_branch == "__create_new__" {
                // Prompt for a new workspace name
                let new_name = inquire::Text::new("New workspace name:")
                    .with_help_message("Enter the name for the new workspace")
                    .prompt()
                    .context("Failed to read workspace name")?;
                let new_name = new_name.trim().to_string();
                if new_name.is_empty() {
                    anyhow::bail!("Workspace name cannot be empty");
                }
                handle_switch_command(
                    config,
                    &new_name,
                    config_path,
                    true,  // create
                    None,  // from
                    false, // no_services
                    false, // no_verify
                    false, // json_output
                    false, // non_interactive
                    None,
                    None,
                    None, // copy_ignored — use config default
                    None, // sandboxed — use config default
                )
                .await?;
            } else if selected_branch == config.git.main_workspace {
                handle_switch_to_main(config, config_path, false, false, false, false, None, None)
                    .await?;
            } else {
                handle_switch_command(
                    config,
                    &selected_branch,
                    config_path,
                    false, // create
                    None,  // from
                    false, // no_services
                    false, // no_verify
                    false, // json_output — interactive mode
                    false, // non_interactive
                    None,
                    None,
                    None, // copy_ignored — use config default
                    None, // sandboxed — use existing state
                )
                .await?;
            }
        }
        Err(e) => match e {
            inquire::InquireError::OperationCanceled => {
                println!("Cancelled.");
            }
            inquire::InquireError::OperationInterrupted => {
                println!("Interrupted.");
            }
            _ => {
                println!("Interactive mode failed: {}", e);
                println!("Try using: devflow switch <workspace-name> or devflow switch --template");
            }
        },
    }

    Ok(())
}

#[derive(Clone)]
struct BranchItem {
    name: String,
    display_name: String,
    is_cwd: bool,
    is_context: bool,
}

fn run_interactive_selector(items: Vec<BranchItem>) -> Result<String, inquire::InquireError> {
    use inquire::Select;

    if items.is_empty() {
        return Err(inquire::InquireError::InvalidConfiguration(
            "No workspaces available".to_string(),
        ));
    }

    // Create display options with context/cwd markers.
    let options: Vec<String> = items
        .iter()
        .map(|item| {
            if item.is_context && item.is_cwd {
                format!("{} *", item.display_name)
            } else if item.is_context {
                format!("{} (context)", item.display_name)
            } else if item.is_cwd {
                format!("{} (cwd)", item.display_name)
            } else {
                item.display_name.clone()
            }
        })
        .collect();

    // Prefer context workspace as default; fall back to cwd workspace.
    let default = items
        .iter()
        .position(|item| item.is_context)
        .or_else(|| items.iter().position(|item| item.is_cwd));

    let mut select = Select::new("Select a workspace to switch to:", options.clone())
        .with_help_message(
        "Use arrow keys to navigate, type to filter, Enter to select, Esc to cancel (*=context+cwd)",
    );

    if let Some(default_index) = default {
        select = select.with_starting_cursor(default_index);
    }

    // Run the selector
    let selected_display = select.prompt()?;

    // Find the corresponding workspace name
    let selected_index = options
        .iter()
        .position(|opt| opt == &selected_display)
        .ok_or_else(|| {
            inquire::InquireError::InvalidConfiguration("Selected option not found".to_string())
        })?;

    Ok(items[selected_index].name.clone())
}

// ── Link ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct LinkServiceResult {
    service_name: String,
    success: bool,
    message: String,
}

#[derive(Debug, Clone)]
struct LinkBranchResult {
    workspace: String,
    parent: Option<String>,
    worktree_path: Option<String>,
    service_results: Vec<LinkServiceResult>,
    services_failed: usize,
}

async fn link_branch_internal(
    config: &Config,
    config_path: &Option<PathBuf>,
    workspace_name: &str,
    from: Option<&str>,
    non_interactive: bool,
) -> Result<LinkBranchResult> {
    let project_dir = config_path
        .as_ref()
        .and_then(|p| p.parent())
        .map(|d| d.to_path_buf())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    let normalized_branch = config.get_normalized_workspace_name(workspace_name);
    let normalized_main = config.get_normalized_workspace_name(&config.git.main_workspace);

    // Ensure main workspace is registered
    if let Ok(mut state_mgr) = LocalStateManager::new() {
        let _ = state_mgr.ensure_default_workspace(&project_dir, &config.git.main_workspace);
    }

    let vcs_repo = vcs::detect_vcs_provider(".").context("Failed to open VCS repository")?;
    if !vcs_repo.workspace_exists(workspace_name)? {
        anyhow::bail!(
            "Workspace '{}' does not exist in {}. Create/switch it first, then run `devflow link {}`.",
            workspace_name,
            vcs_repo.provider_name(),
            workspace_name
        );
    }

    let existing_parent = LocalStateManager::new()
        .ok()
        .and_then(|state| state.get_workspace_by_dir(&project_dir, &normalized_branch))
        .and_then(|b| b.parent);

    let mut parent = from
        .map(|p| config.get_normalized_workspace_name(p))
        .or(existing_parent);

    if parent.is_none() && normalized_branch != normalized_main {
        parent = Some(normalized_main.clone());
    }

    if let Some(ref parent_workspace) = parent {
        if parent_workspace != &normalized_main
            && !linked_workspace_exists(config, config_path, parent_workspace)
        {
            anyhow::bail!(
                "Parent '{}' is not linked in devflow. Run `devflow link {}` first.",
                parent_workspace,
                parent_workspace
            );
        }
        if parent_workspace == &normalized_main {
            if let Ok(mut state_mgr) = LocalStateManager::new() {
                let _ =
                    state_mgr.ensure_default_workspace(&project_dir, &config.git.main_workspace);
            }
        }
    }

    let worktree_path = vcs_repo
        .worktree_path(workspace_name)?
        .map(|p| p.display().to_string())
        .or_else(|| {
            if normalized_branch == normalized_main {
                vcs_repo
                    .main_worktree_dir()
                    .map(|p| p.display().to_string())
            } else {
                None
            }
        });

    // Register workspace in state using project-dir-based API
    if let Ok(mut state_mgr) = LocalStateManager::new() {
        let existing = state_mgr.get_workspace_by_dir(&project_dir, &normalized_branch);
        let workspace = DevflowWorkspace {
            name: normalized_branch.clone(),
            parent: parent
                .clone()
                .or_else(|| existing.as_ref().and_then(|b| b.parent.clone())),
            worktree_path: worktree_path
                .clone()
                .or_else(|| existing.as_ref().and_then(|b| b.worktree_path.clone())),
            created_at: existing
                .as_ref()
                .map(|b| b.created_at)
                .unwrap_or_else(chrono::Utc::now),
            executed_command: existing.as_ref().and_then(|b| b.executed_command.clone()),
            execution_status: existing.as_ref().and_then(|b| b.execution_status.clone()),
            executed_at: existing.as_ref().and_then(|b| b.executed_at),
            sandboxed: existing.as_ref().map(|b| b.sandboxed).unwrap_or(false),
        };
        if let Err(e) = state_mgr.register_workspace_by_dir(&project_dir, workspace) {
            log::warn!("Failed to register workspace in devflow state: {}", e);
        }
    }

    let hook_opts = devflow_core::workspace::LifecycleOptions {
        hook_approval: if non_interactive {
            devflow_core::workspace::hooks::HookApprovalMode::NonInteractive
        } else {
            devflow_core::workspace::hooks::HookApprovalMode::Interactive
        },
        verbose_hooks: true,
        ..Default::default()
    };

    // Fire pre-service-switch hooks before service orchestration
    devflow_core::workspace::hooks::run_lifecycle_hooks_best_effort(
        config,
        &project_dir,
        workspace_name,
        HookPhase::PreSwitch,
        &hook_opts,
    )
    .await;

    let mut service_results = Vec::new();
    let mut services_failed = 0usize;

    if !config.resolve_services().is_empty() {
        let orchestration =
            services::factory::orchestrate_switch(config, &normalized_branch, parent.as_deref())
                .await?;
        for result in orchestration {
            if !result.success {
                services_failed += 1;
            }
            service_results.push(LinkServiceResult {
                service_name: result.service_name,
                success: result.success,
                message: result.message,
            });
        }
    }

    // Fire post-switch hooks
    devflow_core::workspace::hooks::run_lifecycle_hooks_best_effort(
        config,
        &project_dir,
        workspace_name,
        HookPhase::PostSwitch,
        &hook_opts,
    )
    .await;

    Ok(LinkBranchResult {
        workspace: normalized_branch,
        parent,
        worktree_path,
        service_results,
        services_failed,
    })
}

async fn handle_link_command(
    config: &Config,
    config_path: &Option<PathBuf>,
    workspace_name: &str,
    from: Option<&str>,
    json_output: bool,
    non_interactive: bool,
) -> Result<()> {
    let linked =
        link_branch_internal(config, config_path, workspace_name, from, non_interactive).await?;

    if json_output {
        let service_results: Vec<serde_json::Value> = linked
            .service_results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "service": r.service_name,
                    "success": r.success,
                    "message": r.message,
                })
            })
            .collect();

        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": if linked.services_failed == 0 { "ok" } else { "error" },
                "workspace": linked.workspace,
                "parent": linked.parent,
                "worktree_path": linked.worktree_path,
                "services_failed": linked.services_failed,
                "service_results": service_results,
            }))?
        );
    } else {
        println!("Linked devflow workspace: {}", linked.workspace);
        if let Some(parent) = linked.parent.as_deref() {
            println!("  Parent: {}", parent);
        }
        if let Some(path) = linked.worktree_path.as_deref() {
            println!("  Worktree: {}", path);
        }

        if linked.service_results.is_empty() {
            println!("  Services: none configured");
        } else {
            for r in &linked.service_results {
                if r.success {
                    println!("  [{}] {}", r.service_name, r.message);
                } else {
                    println!("  [{}] Warning: {}", r.service_name, r.message);
                }
            }
        }
    }

    if linked.services_failed > 0 {
        anyhow::bail!(
            "Linked workspace '{}' but failed on {}/{} service(s)",
            linked.workspace,
            linked.services_failed,
            linked.service_results.len()
        );
    }

    Ok(())
}

async fn resolve_parent_for_branch_creation(
    config: &Config,
    config_path: &Option<PathBuf>,
    target_workspace: &str,
    requested_parent: Option<&str>,
    context: &BranchContext,
    json_output: bool,
    non_interactive: bool,
) -> Result<Option<String>> {
    let mut parent = requested_parent
        .map(|p| p.to_string())
        .or_else(|| context.context_branch_raw.clone());

    let Some(parent_name) = parent.as_deref() else {
        return Ok(None);
    };

    let target_normalized = config.get_normalized_workspace_name(target_workspace);
    let parent_normalized = config.get_normalized_workspace_name(parent_name);
    if parent_normalized == target_normalized {
        anyhow::bail!(
            "Parent workspace '{}' resolves to the target workspace '{}'. Choose a different --from value.",
            parent_name,
            target_workspace
        );
    }

    // If we have no project config path, we cannot enforce workspace-link checks.
    if config_path.is_none() {
        return Ok(parent);
    }

    if linked_workspace_exists(config, config_path, parent_name) {
        return Ok(parent);
    }

    if json_output || non_interactive {
        anyhow::bail!(
            "Parent workspace '{}' is not linked in devflow. Run `devflow link {}` first.",
            parent_name,
            parent_name
        );
    }

    let default_workspace = config.git.main_workspace.clone();
    let options = vec![
        format!("Link '{}' now (recommended)", parent_name),
        format!("Use default workspace '{}' as parent", default_workspace),
        "Cancel".to_string(),
    ];

    let choice = inquire::Select::new(
        "Parent workspace is not linked in devflow. Choose how to proceed:",
        options,
    )
    .with_starting_cursor(0)
    .prompt()?;

    if choice.starts_with("Link '") {
        let linked = link_branch_internal(config, config_path, parent_name, None, false).await?;
        if linked.services_failed > 0 {
            anyhow::bail!(
                "Linked parent '{}' but failed on {}/{} service(s)",
                parent_name,
                linked.services_failed,
                linked.service_results.len()
            );
        }
        return Ok(parent);
    }

    if choice.starts_with("Use default workspace") {
        if !linked_workspace_exists(config, config_path, &default_workspace) {
            match link_branch_internal(config, config_path, &default_workspace, None, false).await {
                Ok(linked) if linked.services_failed == 0 => {}
                Ok(linked) => {
                    anyhow::bail!(
                        "Linked default workspace '{}' but failed on {}/{} service(s)",
                        default_workspace,
                        linked.services_failed,
                        linked.service_results.len()
                    );
                }
                Err(_) => {
                    // Fallback for repos where the default workspace is not materialized yet.
                    ensure_default_workspace_registered(config, config_path)?;
                }
            }
        }
        parent = Some(default_workspace);
        return Ok(parent);
    }

    anyhow::bail!("Cancelled")
}

// ── Switch ─────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_switch_command(
    config: &Config,
    workspace_name: &str,
    config_path: &Option<std::path::PathBuf>,
    create: bool,
    from: Option<&str>,
    no_services: bool,
    no_verify: bool,
    json_output: bool,
    non_interactive: bool,
    trigger_source: Option<&str>,
    vcs_event: Option<&str>,
    copy_ignored_override: Option<bool>,
    sandboxed: Option<bool>,
) -> Result<()> {
    // Resolve parent via CLI-specific interactive prompt (if needed)
    let from_workspace = if create {
        let context = resolve_branch_context(config);
        resolve_parent_for_branch_creation(
            config,
            config_path,
            workspace_name,
            from,
            &context,
            json_output,
            non_interactive,
        )
        .await?
    } else {
        from.map(|s| s.to_string())
    };

    let approval_mode = if non_interactive || json_output {
        devflow_core::workspace::hooks::HookApprovalMode::NonInteractive
    } else {
        devflow_core::workspace::hooks::HookApprovalMode::Interactive
    };

    let project_dir = config_path
        .as_ref()
        .and_then(|p| p.parent())
        .map(|d| d.to_path_buf())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    let options = devflow_core::workspace::switch::SwitchOptions {
        lifecycle: devflow_core::workspace::LifecycleOptions {
            skip_hooks: no_verify,
            skip_services: no_services,
            hook_approval: approval_mode,
            verbose_hooks: !json_output,
            trigger_source: trigger_source.map(String::from),
            vcs_event: vcs_event.map(String::from),
        },
        create_if_missing: create,
        from_workspace,
        copy_files: None,
        copy_ignored: copy_ignored_override,
        sandboxed,
    };

    let result = devflow_core::workspace::switch::switch_workspace(
        config,
        &project_dir,
        workspace_name,
        &options,
    )
    .await?;

    // ── CLI-specific output ──────────────────────────────────────────
    let worktree_enabled = config.worktree.as_ref().is_some_and(|wt| wt.enabled);
    let shell_integration = super::config::shell_integration_enabled();

    // Worktree DEVFLOW_CD output
    if let Some(ref wt) = result.worktree {
        if !json_output {
            if wt.created {
                println!(
                    "Created worktree for '{}' at {}",
                    workspace_name,
                    wt.path.display(),
                );
            } else {
                println!("Switching to existing worktree: {}", wt.path.display());
            }
            println!("DEVFLOW_CD={}", wt.path.display());
            if !shell_integration {
                super::config::print_manual_cd_hint(&wt.path);
            }
        }
    } else if !json_output {
        if result.branch_created {
            println!(
                "Creating workspace '{}' (parent: {})",
                workspace_name,
                result.parent.as_deref().unwrap_or("HEAD")
            );
        }
        println!("Switched git workspace: {}", result.workspace);
    }

    // Service results output
    let success_count = result.services.iter().filter(|r| r.success).count();
    let fail_count = result.services.iter().filter(|r| !r.success).count();

    if json_output {
        let service_results: Vec<serde_json::Value> = result
            .services
            .iter()
            .map(|r| {
                serde_json::json!({
                    "service": r.service_name,
                    "success": r.success,
                    "message": r.message,
                })
            })
            .collect();
        let summary = serde_json::json!({
            "workspace": result.workspace,
            "parent": result.parent,
            "worktree_path": result.worktree.as_ref().map(|w| w.path.display().to_string()),
            "worktree_created": result.worktree.as_ref().map(|w| w.created).unwrap_or(false),
            "services_switched": success_count,
            "services_failed": fail_count,
            "services_skipped": no_services,
            "service_results": service_results,
        });
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else if !no_services && !result.services.is_empty() {
        for r in &result.services {
            if r.success {
                log::info!("[{}] {}", r.service_name, r.message);
            } else {
                println!("Warning: {}", r.message);
            }
        }

        if success_count > 0 && fail_count == 0 {
            println!(
                "Switched to service workspace: {} ({} service(s))",
                result.workspace, success_count
            );
        } else if success_count > 0 {
            println!(
                "Switched to service workspace: {} ({}/{} service(s), {} failed)",
                result.workspace,
                success_count,
                result.services.len(),
                fail_count
            );
        } else {
            println!(
                "Warning: Failed to switch service workspaces on all {} service(s)",
                result.services.len()
            );
        }

        if fail_count > 0 {
            anyhow::bail!(
                "Failed to switch service workspaces on {}/{} service(s)",
                fail_count,
                result.services.len()
            );
        }
    } else if !no_services && !json_output {
        if worktree_enabled {
            println!("Selected workspace/worktree: {}", result.workspace);
        }
        println!("  (no services configured — use 'devflow service add' to add one)");
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_switch_to_main(
    config: &Config,
    config_path: &Option<std::path::PathBuf>,
    json_output: bool,
    no_services: bool,
    no_verify: bool,
    non_interactive: bool,
    trigger_source: Option<&str>,
    vcs_event: Option<&str>,
) -> Result<()> {
    let main_workspace = config.git.main_workspace.clone();

    if !json_output {
        println!("Switching to main workspace: {}", main_workspace);
    }

    // Delegate to the shared switch command — main is just a special case
    handle_switch_command(
        config,
        &main_workspace,
        config_path,
        false,
        None,
        no_services,
        no_verify,
        json_output,
        non_interactive,
        trigger_source,
        vcs_event,
        None, // copy_ignored — use config default
        None, // sandboxed — main workspace is never sandboxed
    )
    .await
}

// ── Execute in workspace ────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn execute_in_workspace(
    config: &Config,
    config_path: &Option<PathBuf>,
    workspace_name: &str,
    cmd: &str,
    execute_args: &[String],
    detach: bool,
    sandbox_resolved: Option<bool>,
    json_output: bool,
) -> Result<()> {
    // Build full command from -x value + trailing args
    let full_cmd = if execute_args.is_empty() {
        cmd.to_string()
    } else {
        format!("{} {}", cmd, execute_args.join(" "))
    };

    // Resolve worktree path
    let work_dir = vcs::detect_vcs_provider(".")
        .ok()
        .and_then(|repo| repo.worktree_path(workspace_name).ok().flatten())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    // Build sandbox policy if workspace is sandboxed
    let is_sandboxed = sandbox_resolved.unwrap_or(false);
    let sandbox_policy = if is_sandboxed {
        let sandbox_config = config.sandbox.clone().unwrap_or_default();
        Some(devflow_core::sandbox::SandboxPolicy::from_config(
            &sandbox_config,
            &work_dir,
        ))
    } else {
        None
    };

    // Validate command against sandbox policy
    if let Some(ref policy) = sandbox_policy {
        policy.validate_command(&full_cmd)?;
    }

    if !json_output && is_sandboxed {
        println!(
            "Sandbox: enabled (platform: {})",
            sandbox_policy
                .as_ref()
                .map(|p| p.platform.to_string())
                .unwrap_or_else(|| "none".to_string())
        );
    }

    // Record execution state
    let normalized = config.get_normalized_workspace_name(workspace_name);
    if let Some(ref path) = config_path {
        if let Ok(mut state) = LocalStateManager::new() {
            if let Some(mut ws) = state.get_workspace(path, &normalized) {
                ws.executed_command = Some(full_cmd.clone());
                ws.execution_status = Some(if detach { "detached" } else { "running" }.to_string());
                ws.executed_at = Some(chrono::Utc::now());
                if let Err(e) = state.register_workspace(path, ws) {
                    log::warn!("Failed to record execution state: {}", e);
                }
            }
        }
    }

    if detach {
        // Detached/interactive execution via configured multiplexer
        let is_interactive = full_cmd.is_empty();

        let template = config
            .execute
            .as_ref()
            .and_then(|e| e.detach_command.clone())
            .or_else(|| {
                // Respect configured multiplexer preference, then auto-detect
                let preferred = config
                    .execute
                    .as_ref()
                    .and_then(|e| e.multiplexer.as_deref());

                match preferred {
                    Some("zellij") if which::which("zellij").is_ok() => {
                        Some("zellij --session {session} --cwd {dir} {cmd}".to_string())
                    }
                    Some("tmux") if which::which("tmux").is_ok() => {
                        Some("tmux new-session -d -s {session} -c {dir} {cmd}".to_string())
                    }
                    Some(name) => {
                        log::warn!(
                            "Configured multiplexer '{}' not found, falling back to auto-detection",
                            name
                        );
                        None
                    }
                    None => None,
                }
                .or_else(|| {
                    if which::which("tmux").is_ok() {
                        Some("tmux new-session -d -s {session} -c {dir} {cmd}".to_string())
                    } else if which::which("zellij").is_ok() {
                        Some("zellij --session {session} --cwd {dir} {cmd}".to_string())
                    } else {
                        None
                    }
                })
            });

        let Some(template) = template else {
            anyhow::bail!(
                "No multiplexer available for --detach/--open. Install tmux or zellij, or configure execute.detach_command in .devflow.yml"
            );
        };

        let session = normalized.replace('/', "-");

        // Build the {cmd} replacement
        let cmd_replacement = if is_interactive {
            String::new()
        } else if template.contains("sh -c") {
            // Custom template already includes sh -c — pass raw command
            let escaped = full_cmd.replace('\'', "'\\''");
            format!("'{}'", escaped)
        } else {
            let escaped = full_cmd.replace('\'', "'\\''");
            format!("sh -c '{}'", escaped)
        };

        let expanded = template
            .replace("{session}", &session)
            .replace("{dir}", &work_dir.display().to_string())
            .replace("{cmd}", &cmd_replacement);
        // Trim trailing whitespace from empty {cmd} expansion
        let expanded = expanded.trim_end().to_string();

        if !json_output {
            if is_interactive {
                println!("Opening session: {}", expanded);
            } else {
                println!("Detaching: {}", expanded);
            }
        }

        let status = tokio::process::Command::new("sh")
            .args(["-c", &expanded])
            .status()
            .await
            .context("Failed to launch multiplexer session")?;

        if !status.success() {
            anyhow::bail!(
                "Multiplexer command failed with exit code: {}",
                status.code().unwrap_or(-1)
            );
        }

        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "workspace": normalized,
                    "command": full_cmd,
                    "session": session,
                    "worktree": work_dir.display().to_string(),
                    "sandboxed": is_sandboxed,
                    "detached": true,
                }))?
            );
        }
    } else {
        // Foreground execution
        if json_output {
            eprintln!("Running: {}", full_cmd);
        } else {
            println!("Running: {}", full_cmd);
        }

        let status = if let Some(ref policy) = sandbox_policy {
            let (prog, args) = policy.wrap_command_string(&full_cmd);
            let mut cmd = tokio::process::Command::new(&prog);
            cmd.args(&args).current_dir(&work_dir);
            cmd.status()
                .await
                .context("Failed to execute sandboxed command")?
        } else {
            tokio::process::Command::new("sh")
                .args(["-c", &full_cmd])
                .current_dir(&work_dir)
                .status()
                .await
                .context("Failed to execute command")?
        };

        // Update state on completion
        let execution_status = if status.success() { "done" } else { "failed" };
        if let Some(ref path) = config_path {
            if let Ok(mut state_mgr) = LocalStateManager::new() {
                if let Some(mut ws) = state_mgr.get_workspace(path, &normalized) {
                    ws.execution_status = Some(execution_status.to_string());
                    if let Err(e) = state_mgr.register_workspace(path, ws) {
                        log::warn!("Failed to update execution state: {}", e);
                    }
                }
            }
        }

        if !status.success() {
            anyhow::bail!(
                "Command failed with exit code: {}",
                status.code().unwrap_or(-1)
            );
        }

        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "workspace": normalized,
                    "command": full_cmd,
                    "exit_code": status.code(),
                    "worktree": work_dir.display().to_string(),
                    "sandboxed": is_sandboxed,
                    "detached": false,
                }))?
            );
        }
    }

    Ok(())
}

// ── Remove ─────────────────────────────────────────────────────────────────────

async fn handle_remove_command(
    config: &Config,
    workspace_name: &str,
    force: bool,
    keep_services: bool,
    config_path: &Option<std::path::PathBuf>,
    json_output: bool,
    non_interactive: bool,
) -> Result<()> {
    // ── CLI-specific safety checks ──────────────────────────────────
    let vcs_repo = vcs::detect_vcs_provider(".").ok();

    // Safety check: don't remove main workspace
    if workspace_name == config.git.main_workspace {
        anyhow::bail!("Cannot remove the main workspace '{}'", workspace_name);
    }

    // Safety check: don't remove the currently checked-out workspace
    if let Some(ref repo) = vcs_repo {
        if let Ok(Some(current)) = repo.current_workspace() {
            if current == workspace_name {
                anyhow::bail!(
                    "Cannot remove workspace '{}' because it is currently checked out. Switch to another workspace first.",
                    workspace_name
                );
            }
        }
    }

    // Confirm unless --force (skip prompt in JSON/non-interactive mode — require --force)
    if !force {
        if json_output || non_interactive {
            anyhow::bail!("Use --force to confirm removal in non-interactive or JSON output mode");
        }
        println!("This will remove:");
        if vcs_repo.is_some() {
            println!("  - VCS workspace: {}", workspace_name);
        }
        if let Some(ref repo) = vcs_repo {
            if repo.worktree_path(workspace_name)?.is_some() {
                println!("  - Worktree directory");
            }
        }
        if !keep_services {
            println!("  - Associated service workspaces");
        }
        print!("Continue? [y/N] ");
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    // ── Delegate to core lifecycle ──────────────────────────────────
    let approval_mode = if non_interactive || json_output {
        devflow_core::workspace::hooks::HookApprovalMode::NonInteractive
    } else {
        devflow_core::workspace::hooks::HookApprovalMode::Interactive
    };

    let project_dir = config_path
        .as_ref()
        .and_then(|p| p.parent())
        .map(|d| d.to_path_buf())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    let options = devflow_core::workspace::delete::DeleteOptions {
        lifecycle: devflow_core::workspace::LifecycleOptions {
            skip_hooks: false,
            skip_services: false,
            hook_approval: approval_mode,
            verbose_hooks: !json_output,
            ..Default::default()
        },
        keep_services,
    };

    let result = devflow_core::workspace::delete::delete_workspace(
        config,
        &project_dir,
        workspace_name,
        &options,
    )
    .await?;

    // ── CLI-specific output ──────────────────────────────────────────
    let service_failures = result.services.iter().filter(|r| !r.success).count();

    if json_output {
        let service_json: Vec<serde_json::Value> = result
            .services
            .iter()
            .map(|r| {
                serde_json::json!({
                    "service": r.service_name,
                    "success": r.success,
                    "message": r.message,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": if service_failures == 0 && result.branch_deleted { "ok" } else { "error" },
                "workspace": workspace_name,
                "branch_deleted": result.branch_deleted,
                "worktree_removed": result.worktree_removed,
                "worktree_path": result.worktree_path,
                "services_skipped": keep_services,
                "service_failures": service_failures,
                "service_results": service_json,
            }))?
        );
    } else {
        if result.worktree_removed {
            if let Some(ref wt) = result.worktree_path {
                println!("Removed worktree: {}", wt);
            }
        }
        for r in &result.services {
            if r.success {
                println!("  [{}] {}", r.service_name, r.message);
            } else {
                println!("  [{}] Warning: {}", r.service_name, r.message);
            }
        }
        if result.branch_deleted {
            println!("Workspace deleted: {}", workspace_name);
        }
        if service_failures == 0 && result.branch_deleted {
            println!("Workspace '{}' removed successfully.", workspace_name);
        } else {
            println!(
                "Workspace '{}' removal completed with errors.",
                workspace_name
            );
        }
    }

    if service_failures > 0 {
        anyhow::bail!(
            "Failed to remove service workspaces on {}/{} service(s)",
            service_failures,
            result.services.len()
        );
    }

    if !result.branch_deleted {
        anyhow::bail!("Failed to delete VCS workspace '{}'", workspace_name);
    }

    Ok(())
}

// ── Merge ──────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn handle_merge_command(
    config: &Config,
    target: Option<&str>,
    cleanup_flag: bool,
    dry_run: bool,
    json_output: bool,
    force: bool,
    check_only: bool,
    cascade_rebase_flag: bool,
) -> Result<()> {
    // Resolve effective values from config + CLI flags
    let merge_defaults = config.merge.clone().unwrap_or_default();
    let cleanup = merge_defaults.effective_cleanup(cleanup_flag);
    let cascade_rebase = merge_defaults.effective_cascade_rebase(cascade_rebase_flag);
    let strategy = merge_defaults.effective_strategy();
    let vcs_repo = vcs::detect_vcs_provider(".").context("Failed to open VCS repository")?;

    if vcs_repo.provider_name() != "git" {
        anyhow::bail!(
            "Merge is currently supported for git repositories only (detected: {}).",
            vcs_repo.provider_name()
        );
    }

    let initial_dir = std::env::current_dir().context("Failed to get current directory")?;

    // Determine source workspace (current workspace)
    let source = vcs_repo
        .current_workspace()?
        .ok_or_else(|| anyhow::anyhow!("Could not determine current workspace (detached HEAD?)"))?;

    // Determine target workspace
    let target_workspace = target.unwrap_or(&config.git.main_workspace);

    if !vcs_repo.workspace_exists(target_workspace)? {
        anyhow::bail!(
            "Target workspace '{}' does not exist. Run 'devflow list' to see available workspaces.",
            target_workspace
        );
    }

    if source == target_workspace {
        anyhow::bail!("Source and target workspace are the same: '{}'", source);
    }

    // If a dedicated worktree already exists for the target workspace, perform the
    // merge there to avoid checking out a workspace that may be locked elsewhere.
    let merge_dir = vcs_repo
        .worktree_path(target_workspace)?
        .unwrap_or_else(|| initial_dir.clone());

    // ── Merge readiness checks (gated by smart_merge feature flag) ──
    let smart_merge_enabled = devflow_core::config::GlobalConfig::load()
        .ok()
        .flatten()
        .map(|g| g.smart_merge_enabled())
        .unwrap_or(false);

    if !force && smart_merge_enabled {
        if let Some(ref merge_config) = config.merge {
            let checks = devflow_core::merge::build_checks_from_config(merge_config);
            if !checks.is_empty() {
                if !json_output {
                    println!("Running merge readiness checks...");
                }
                let report = devflow_core::merge::run_checks(
                    &checks,
                    vcs_repo.as_ref(),
                    &source,
                    target_workspace,
                );

                if json_output {
                    if check_only {
                        println!("{}", serde_json::to_string_pretty(&report)?);
                        return Ok(());
                    }
                } else {
                    for check in &report.checks {
                        let icon = if check.passed {
                            "✓"
                        } else if check.severity == devflow_core::merge::CheckSeverity::Error {
                            "✗"
                        } else {
                            "⚠"
                        };
                        println!("  {} {} — {}", icon, check.check_name, check.message);
                        if let Some(ref suggestion) = check.suggestion {
                            println!("    Suggestion: {}", suggestion);
                        }
                    }
                }

                if check_only {
                    if report.ready {
                        println!("\nMerge readiness: READY");
                    } else {
                        println!("\nMerge readiness: NOT READY");
                    }
                    return Ok(());
                }

                if !report.ready {
                    anyhow::bail!("Merge readiness checks failed. Use --force to skip checks.");
                }
            }
        } else if check_only {
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "source": source,
                        "target": target_workspace,
                        "ready": true,
                        "checks": [],
                    }))?
                );
            } else {
                println!("No merge checks configured. Merge is ready.");
            }
            return Ok(());
        }
    }

    if dry_run {
        if json_output {
            let normalized = config.get_normalized_workspace_name(&source);
            let has_worktree = vcs_repo.worktree_path(&source)?.is_some();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "dry_run": true,
                    "source": source,
                    "target": target_workspace,
                    "merge_directory": merge_dir,
                    "cleanup": cleanup,
                    "has_worktree": has_worktree,
                    "normalized_service_branch": normalized,
                }))?
            );
        } else {
            println!("Merge plan:");
            println!("  Source: {}", source);
            println!("  Target: {}", target_workspace);
            if cleanup {
                println!(
                    "  Cleanup: will delete source workspace, worktree, and service workspaces after merge"
                );
            }
            println!("\n[dry-run] No changes made.");
        }
        return Ok(());
    }

    if !json_output {
        println!("Merge plan:");
        println!("  Source: {}", source);
        println!("  Target: {}", target_workspace);
        if cleanup {
            println!(
                "  Cleanup: will delete source workspace, worktree, and service workspaces after merge"
            );
        }
    }

    // Fire pre-merge hooks
    {
        let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let hook_opts = devflow_core::workspace::LifecycleOptions {
            hook_approval: devflow_core::workspace::hooks::HookApprovalMode::Interactive,
            verbose_hooks: !json_output,
            ..Default::default()
        };
        devflow_core::workspace::hooks::run_lifecycle_hooks(
            config,
            &project_dir,
            &source,
            HookPhase::PreMerge,
            &hook_opts,
        )
        .await?;
    }

    // If rebase strategy, rebase source onto target first (before switching to target)
    if strategy == devflow_core::config::MergeStrategy::Rebase {
        if !json_output {
            println!("\nRebasing '{}' onto '{}'...", source, target_workspace);
        }
        // Ensure we're on the source workspace for rebase
        if merge_dir == initial_dir {
            vcs_repo
                .checkout_workspace(&source)
                .context("Failed to checkout source for rebase")?;
        }
        let rebase_result = vcs_repo
            .rebase(target_workspace)
            .context("Rebase failed. Resolve conflicts and try again.")?;
        if !rebase_result.success {
            anyhow::bail!(
                "Rebase had conflicts in: {}",
                rebase_result.conflict_files.join(", ")
            );
        }
        if !json_output {
            println!("Rebased {} commits.", rebase_result.commits_replayed);
        }
    }

    // Perform the merge
    if merge_dir == initial_dir {
        // Merge in the current worktree, so we must first move to target workspace.
        vcs_repo
            .checkout_workspace(target_workspace)
            .with_context(|| {
                format!(
                    "Failed to switch to target workspace '{}' before merge",
                    target_workspace
                )
            })?;
    }

    if !json_output {
        println!("\nMerging '{}' into '{}'...", source, target_workspace);
        if merge_dir != initial_dir {
            println!("Using target worktree: {}", merge_dir.display());
        }
    }
    // Use the VcsProvider merge rather than spawning git CLI
    let merge_vcs =
        vcs::detect_vcs_provider(&merge_dir).context("Failed to open VCS repository for merge")?;
    merge_vcs
        .merge_branch(&source)
        .context("Merge failed. Resolve conflicts and try again.")?;

    if !json_output {
        println!("Merge successful.");
    }

    // Fire post-merge hooks
    {
        let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let hook_opts = devflow_core::workspace::LifecycleOptions {
            hook_approval: devflow_core::workspace::hooks::HookApprovalMode::Interactive,
            verbose_hooks: !json_output,
            ..Default::default()
        };
        devflow_core::workspace::hooks::run_lifecycle_hooks(
            config,
            &project_dir,
            target_workspace,
            HookPhase::PostMerge,
            &hook_opts,
        )
        .await?;
    }

    // ── Cascade report (gated by smart_merge feature flag) ──────────
    let mut cascade_json = serde_json::json!(null);
    if smart_merge_enabled {
        let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        match devflow_core::merge::build_cascade_report(
            merge_vcs.as_ref(),
            &project_dir,
            &source,
            target_workspace,
            config.merge.as_ref(),
        ) {
            Ok(cascade) => {
                if !cascade.affected_children.is_empty() {
                    if !json_output {
                        println!("\nCascade report:");
                        println!(
                            "  Affected child workspaces: {}",
                            cascade.affected_children.join(", ")
                        );
                        for nr in &cascade.needs_rebase {
                            println!("  ⚠ {} needs rebase: {}", nr.workspace, nr.reason);
                        }
                    }

                    // Auto-rebase if requested
                    if cascade_rebase {
                        for nr in &cascade.needs_rebase {
                            if !json_output {
                                println!("  Rebasing '{}'...", nr.workspace);
                            }
                            // Checkout child, rebase onto target
                            if let Ok(child_vcs) = vcs::detect_vcs_provider(&project_dir) {
                                if child_vcs.checkout_workspace(&nr.workspace).is_ok() {
                                    match child_vcs.rebase(target_workspace) {
                                        Ok(result) if result.success => {
                                            if !json_output {
                                                println!(
                                                    "    ✓ Rebased {} commits",
                                                    result.commits_replayed
                                                );
                                            }
                                        }
                                        Ok(result) => {
                                            if !json_output {
                                                println!(
                                                    "    ✗ Rebase conflicts in: {}",
                                                    result.conflict_files.join(", ")
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            if !json_output {
                                                println!("    ✗ Rebase failed: {}", e);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        // Return to target workspace
                        let _ = vcs::detect_vcs_provider(&project_dir)
                            .ok()
                            .and_then(|v| v.checkout_workspace(target_workspace).ok());
                    }

                    cascade_json = serde_json::to_value(&cascade).unwrap_or_default();
                }
            }
            Err(e) => {
                log::warn!("Failed to build cascade report: {}", e);
            }
        }
    }

    let mut cleanup_result = serde_json::json!(null);

    // Cleanup if requested — delegate to the core workspace delete lifecycle
    if cleanup {
        if !json_output {
            println!("\nCleaning up source workspace '{}'...", source);
        }

        // Safety: if we are still on the source workspace, detach HEAD first
        // so the branch becomes deletable.
        if let Ok(Some(current)) = vcs_repo.current_workspace() {
            if current == source {
                if let Err(e) = vcs_repo.detach_head() {
                    log::warn!(
                        "Failed to detach HEAD before deleting workspace '{}': {}",
                        source,
                        e
                    );
                }
            }
        }

        let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        let delete_options = devflow_core::workspace::delete::DeleteOptions {
            lifecycle: devflow_core::workspace::LifecycleOptions {
                skip_hooks: false,
                skip_services: false,
                hook_approval: devflow_core::workspace::hooks::HookApprovalMode::Interactive,
                verbose_hooks: !json_output,
                ..Default::default()
            },
            keep_services: false,
        };

        let delete_result = devflow_core::workspace::delete::delete_workspace(
            config,
            &project_dir,
            &source,
            &delete_options,
        )
        .await;

        match delete_result {
            Ok(result) => {
                if !json_output {
                    if result.worktree_removed {
                        if let Some(ref wt) = result.worktree_path {
                            println!("Removed worktree: {}", wt);
                        }
                    }
                    for r in &result.services {
                        if r.success {
                            println!("{}", r.message);
                        } else {
                            println!("Warning: {}", r.message);
                        }
                    }
                    if result.branch_deleted {
                        println!("Deleted workspace: {}", source);
                    }
                    println!("Cleanup complete.");
                }

                let service_json: Vec<serde_json::Value> = result
                    .services
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "service": r.service_name,
                            "success": r.success,
                            "message": r.message,
                        })
                    })
                    .collect();

                cleanup_result = serde_json::json!({
                    "worktree_removed": result.worktree_removed,
                    "branch_deleted": result.branch_deleted,
                    "service_results": service_json,
                });
            }
            Err(e) => {
                log::warn!("Failed to clean up source workspace '{}': {}", source, e);
                if !json_output {
                    println!("Warning: Failed to clean up source workspace: {}", e);
                }
                cleanup_result = serde_json::json!({
                    "error": e.to_string(),
                });
            }
        }
    }

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "ok",
                "source": source,
                "target": target_workspace,
                "cleanup": cleanup_result,
                "cascade": cascade_json,
            }))?
        );
    }

    Ok(())
}

// ── Rebase ─────────────────────────────────────────────────────────────────────

async fn handle_rebase_command(
    config: &Config,
    target: Option<&str>,
    dry_run: bool,
    json_output: bool,
) -> Result<()> {
    let smart_merge_enabled = devflow_core::config::GlobalConfig::load()
        .ok()
        .flatten()
        .map(|g| g.smart_merge_enabled())
        .unwrap_or(false);
    if !smart_merge_enabled {
        anyhow::bail!(
            "Rebase requires the smart_merge feature flag. Enable it with:\n  \
             devflow config set smart_merge true\n  \
             or set smart_merge: true in ~/.config/devflow/config.yml"
        );
    }

    let vcs_repo = vcs::detect_vcs_provider(".").context("Failed to open VCS repository")?;

    if vcs_repo.provider_name() != "git" {
        anyhow::bail!(
            "Rebase is currently supported for git repositories only (detected: {}).",
            vcs_repo.provider_name()
        );
    }

    let source = vcs_repo
        .current_workspace()?
        .ok_or_else(|| anyhow::anyhow!("Could not determine current workspace (detached HEAD?)"))?;

    let target_workspace = target.unwrap_or(&config.git.main_workspace);

    if source == target_workspace {
        anyhow::bail!("Cannot rebase '{}' onto itself", source);
    }

    if dry_run {
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "dry_run": true,
                    "source": source,
                    "target": target_workspace,
                }))?
            );
        } else {
            println!("Rebase plan:");
            println!("  Source: {} (current)", source);
            println!("  Onto: {}", target_workspace);
            println!("\n[dry-run] No changes made.");
        }
        return Ok(());
    }

    // Fire pre-rebase hooks
    {
        let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let hook_opts = devflow_core::workspace::LifecycleOptions {
            hook_approval: devflow_core::workspace::hooks::HookApprovalMode::Interactive,
            verbose_hooks: !json_output,
            ..Default::default()
        };
        devflow_core::workspace::hooks::run_lifecycle_hooks(
            config,
            &project_dir,
            &source,
            HookPhase::PreRebase,
            &hook_opts,
        )
        .await?;
    }

    if !json_output {
        println!("Rebasing '{}' onto '{}'...", source, target_workspace);
    }

    let result = vcs_repo.rebase(target_workspace).context("Rebase failed")?;

    if result.success {
        if !json_output {
            println!(
                "Rebase successful: {} commit(s) replayed.",
                result.commits_replayed
            );
        }

        // Fire post-rebase hooks
        {
            let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let hook_opts = devflow_core::workspace::LifecycleOptions {
                hook_approval: devflow_core::workspace::hooks::HookApprovalMode::Interactive,
                verbose_hooks: !json_output,
                ..Default::default()
            };
            devflow_core::workspace::hooks::run_lifecycle_hooks(
                config,
                &project_dir,
                &source,
                HookPhase::PostRebase,
                &hook_opts,
            )
            .await?;
        }
    } else if !json_output {
        println!("Rebase encountered conflicts:");
        for f in &result.conflict_files {
            println!("  - {}", f);
        }
        println!("Rebase aborted. Resolve conflicts manually.");
    }

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }

    Ok(())
}

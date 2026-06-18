use std::path::PathBuf;

use anyhow::Result;
use devflow_core::config::Config;
use devflow_core::services;
use devflow_core::vcs;

use super::context::{
    collect_list_workspace_names, context_matches_branch, load_registry_branches_for_list,
    resolve_branch_context, BranchContextSource,
};

pub(super) fn print_enriched_branch_list(
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

pub(super) async fn handle_environment_graph(
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
pub(super) fn enrich_branch_list_json(
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

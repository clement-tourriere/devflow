//! Canonical workspace inventory shared by every frontend.
//!
//! A devflow workspace is a materialized VCS worktree/workspace. The registry
//! contributes durable lineage and execution metadata; VCS contributes live
//! paths; services and processes contribute runtime health.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::processes::{self, ProcessStatus};
use crate::services::factory;
use crate::state::{DevflowWorkspace, LocalStateManager};
use crate::vcs::{self, VcsProvider};

pub const INVENTORY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryProject {
    pub name: String,
    pub root: String,
    pub vcs_provider: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceService {
    pub name: String,
    pub provider: String,
    pub state: Option<String>,
    pub database_name: Option<String>,
    pub parent_workspace: Option<String>,
    pub provisioned: bool,
    pub supports_lifecycle: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceNode {
    /// Exact branch/bookmark name used for every VCS operation.
    pub name: String,
    /// Collision-resistant key used by service backends and filesystem paths.
    pub service_key: String,
    /// Key newly-derived releases would assign to this raw name. This can
    /// differ from `service_key` when an old namespace was safely adopted.
    pub canonical_service_key: String,
    /// `canonical`, `legacy_adopted`, or `legacy_unresolved`.
    pub identity_status: String,
    /// Immutable raw-name creation source.
    pub parent: Option<String>,
    /// `present` or `missing` when `parent` is set.
    pub parent_state: Option<String>,
    pub children: Vec<String>,
    pub is_default: bool,
    pub is_context: bool,
    pub worktree_path: Option<String>,
    /// `ready`, `degraded`, or `missing`.
    pub health: String,
    pub created_at: String,
    pub executed_command: Option<String>,
    pub execution_status: Option<String>,
    pub services: Vec<WorkspaceService>,
    pub processes: Vec<ProcessStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInventory {
    pub schema_version: u32,
    pub project: InventoryProject,
    pub context_workspace: Option<String>,
    pub default_workspace: String,
    pub roots: Vec<String>,
    pub workspaces: Vec<WorkspaceNode>,
    /// Canonical depth-first display order (see [`flatten_tree`]).
    #[serde(default)]
    pub flat_order: Vec<FlatWorkspaceRow>,
    pub warnings: Vec<String>,
}

/// Build the authoritative workspace graph for a project.
///
/// Live worktrees are reconciled into local state so manually-created
/// worktrees become visible everywhere. Registry-only entries are retained as
/// `missing` nodes, making stale paths and deleted parents explicit.
pub async fn build_workspace_inventory(
    config: &Config,
    project_dir: &Path,
) -> Result<WorkspaceInventory> {
    let mut warnings = Vec::new();

    // A broken VCS (moved repo, deleted .git, missing jj binary) must not
    // make the project uninspectable: fall back to a registry-only view and
    // surface the failure as a warning instead of erroring the whole
    // inventory out of the CLI/TUI/GUI. Provider method failures likewise
    // degrade with an explicit warning — silently empty worktree data would
    // report every workspace as `missing` with no explanation.
    let provider = match vcs::detect_vcs_provider(project_dir) {
        Ok(provider) => Some(provider),
        Err(error) => {
            warnings.push(format!(
                "VCS unavailable for '{}': {error:#}. Showing registry-only inventory; live worktree state is unknown",
                project_dir.display()
            ));
            None
        }
    };
    let provider_name = provider
        .as_deref()
        .map(|provider| provider.provider_name().to_string())
        .unwrap_or_else(|| "unavailable".to_string());
    let project_root = vcs::resolve_project_root(project_dir);
    let context_workspace = match provider.as_deref().map(VcsProvider::current_workspace) {
        Some(Ok(context)) => context,
        Some(Err(error)) => {
            warnings.push(format!(
                "failed to resolve the current workspace: {error:#}"
            ));
            None
        }
        None => None,
    };
    // DEVFLOW_CONTEXT_BRANCH pins the context for CI/agents; it must override
    // the cwd-derived context in the shared inventory so all frontends agree.
    let context_workspace = std::env::var("DEVFLOW_CONTEXT_BRANCH")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or(context_workspace);
    let default_workspace = config.git.main_workspace.clone();
    let live_worktrees = match provider.as_deref().map(VcsProvider::list_worktrees) {
        Some(Ok(worktrees)) => worktrees,
        Some(Err(error)) => {
            warnings.push(format!(
                "failed to enumerate live worktrees: {error:#}. Workspaces may be reported as missing"
            ));
            Vec::new()
        }
        None => Vec::new(),
    };
    if let Some(provider) = provider.as_deref() {
        if let Some(mismatch) =
            super::invariant::git_primary_workspace_mismatch_from(config, provider, &live_worktrees)
        {
            warnings.push(mismatch.diagnostic());
        }
    }

    let mut state = LocalStateManager::new()?;
    let mut registry = state.get_or_init_workspaces_by_dir(project_dir, &default_workspace)?;
    let registered_names: HashSet<String> = registry.iter().map(|w| w.name.clone()).collect();

    // Adopt manually-created worktrees without inventing lineage.
    for live in &live_worktrees {
        let Some(name) = live.workspace.as_ref() else {
            continue;
        };
        if registered_names.contains(name) {
            continue;
        }
        let mut workspace = DevflowWorkspace {
            name: name.clone(),
            service_key: config.get_service_workspace_key(name),
            raw_identity_verified: true,
            parent: None,
            worktree_path: Some(live.path.display().to_string()),
            created_at: chrono::Utc::now(),
            executed_command: None,
            execution_status: None,
            executed_at: None,
        };
        // Never persist a name every lifecycle entry point (switch, delete,
        // link, hooks) will refuse to operate on: the registry entry would
        // be visible but impossible to manage or remove without hand-editing
        // the state file.
        if let Err(reason) = super::validate_workspace_name(name) {
            warnings.push(format!(
                "workspace '{}' was not adopted into local state: {reason}. Rename the branch to manage it with devflow",
                name
            ));
            workspace.raw_identity_verified = false;
            registry.push(workspace);
            continue;
        }
        if let Err(error) = state.register_workspace_by_dir(project_dir, workspace.clone()) {
            // Keep the live raw identity visible in this inventory snapshot,
            // but do not persist a key assignment while legacy ownership is
            // unresolved or another raw workspace owns that key.
            warnings.push(format!(
                "workspace '{}' was not adopted into local state: {error:#}",
                name
            ));
            workspace.raw_identity_verified = false;
        }
        registry.push(workspace);
    }

    let mut live_by_legacy_key: HashMap<String, Vec<String>> = HashMap::new();
    for raw_name in live_worktrees
        .iter()
        .filter_map(|worktree| worktree.workspace.as_ref())
    {
        live_by_legacy_key
            .entry(crate::config::legacy_normalize_workspace_name(raw_name))
            .or_default()
            .push(raw_name.clone());
    }
    for workspace in &registry {
        let canonical_key = crate::config::workspace_service_key(&workspace.name);
        if !workspace.raw_identity_verified {
            let legacy_key = crate::config::legacy_normalize_workspace_name(&workspace.name);
            let mut candidates = live_by_legacy_key
                .get(&legacy_key)
                .cloned()
                .unwrap_or_default();
            candidates.sort();
            candidates.dedup();
            let candidates = if candidates.is_empty() {
                "none currently materialized".to_string()
            } else {
                candidates.join(", ")
            };
            warnings.push(format!(
                "legacy workspace key '{}' has unresolved raw ownership (candidates: {}). Service and process operations are blocked to prevent data hiding or namespace duplication; rename/remove the collision or edit the workspace registry (~/.config/devflow/local_state.yml) after identifying the existing resource owner",
                legacy_key,
                candidates
            ));
        } else if workspace.service_key != canonical_key {
            warnings.push(format!(
                "workspace '{}' retains legacy service key '{}' (new canonical key '{}') to keep existing services and process state visible",
                workspace.name, workspace.service_key, canonical_key
            ));
        }
    }
    let mut owners_by_service_key: HashMap<&str, Vec<&str>> = HashMap::new();
    for workspace in &registry {
        owners_by_service_key
            .entry(&workspace.service_key)
            .or_default()
            .push(&workspace.name);
    }
    // Deterministic warning order: HashMap iteration would shuffle the
    // collision warnings between runs, breaking JSON diffing/snapshots.
    let mut collision_groups: Vec<(&str, Vec<&str>)> = owners_by_service_key
        .into_iter()
        .filter(|(_, owners)| owners.len() > 1)
        .collect();
    collision_groups.sort_by_key(|(service_key, _)| *service_key);
    for (service_key, mut owners) in collision_groups {
        owners.sort();
        warnings.push(format!(
            "service key '{}' is assigned to multiple raw workspaces ({}); service/process operations are blocked until the registry collision is repaired",
            service_key,
            owners.join(", ")
        ));
    }

    let known_names: HashSet<String> = registry.iter().map(|w| w.name.clone()).collect();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    for workspace in &registry {
        if let Some(parent) = workspace.parent.as_ref() {
            children
                .entry(parent.clone())
                .or_default()
                .push(workspace.name.clone());
        }
    }
    for names in children.values_mut() {
        names.sort();
    }

    let configured_services = config.resolve_services();
    let expected_service_names: HashSet<String> = configured_services
        .iter()
        .filter(|service| service.auto_workspace)
        .map(|service| service.name.clone())
        .collect();
    let mut raw_by_service_key: HashMap<String, Option<String>> = HashMap::new();
    for workspace in &registry {
        raw_by_service_key
            .entry(workspace.service_key.clone())
            .and_modify(|raw| *raw = None)
            .or_insert_with(|| Some(workspace.name.clone()));
    }
    let mut service_by_key: HashMap<String, Vec<WorkspaceService>> = HashMap::new();
    let mut service_templates = Vec::new();
    for service_config in &configured_services {
        match factory::create_provider_from_named_config(config, service_config).await {
            Ok(service_provider) => {
                let template = WorkspaceService {
                    name: service_config.name.clone(),
                    provider: service_provider.provider_name().to_string(),
                    state: None,
                    database_name: None,
                    parent_workspace: None,
                    provisioned: false,
                    supports_lifecycle: service_provider.supports_lifecycle(),
                };
                match service_provider.list_workspaces().await {
                    Ok(service_workspaces) => {
                        for service_workspace in service_workspaces {
                            if !registry
                                .iter()
                                .any(|w| w.service_key == service_workspace.name)
                            {
                                warnings.push(format!(
                                    "service '{}' has orphan workspace '{}'",
                                    service_config.name, service_workspace.name
                                ));
                            }
                            let parent_workspace = service_workspace
                                .parent_workspace
                                .as_ref()
                                .and_then(|parent| raw_by_service_key.get(parent))
                                .and_then(Option::as_ref)
                                .cloned()
                                .or(service_workspace.parent_workspace);
                            service_by_key
                                .entry(service_workspace.name.clone())
                                .or_default()
                                .push(WorkspaceService {
                                    name: service_config.name.clone(),
                                    provider: service_provider.provider_name().to_string(),
                                    state: service_workspace.state,
                                    database_name: Some(service_workspace.database_name),
                                    parent_workspace,
                                    provisioned: true,
                                    supports_lifecycle: service_provider.supports_lifecycle(),
                                });
                        }
                    }
                    Err(error) => warnings.push(format!(
                        "failed to inspect service '{}': {error:#}",
                        service_config.name
                    )),
                }
                service_templates.push(template);
            }
            Err(error) => {
                warnings.push(format!(
                    "failed to initialize service '{}': {error:#}",
                    service_config.name
                ));
                service_templates.push(WorkspaceService {
                    name: service_config.name.clone(),
                    provider: service_config.provider_type.clone(),
                    state: None,
                    database_name: None,
                    parent_workspace: None,
                    provisioned: false,
                    supports_lifecycle: false,
                });
            }
        }
    }

    let all_processes = match processes::list_workspace_processes(config, project_dir, None) {
        Ok(statuses) => statuses,
        Err(error) => {
            warnings.push(format!("failed to inspect workspace processes: {error:#}"));
            Vec::new()
        }
    };

    let live_by_name: HashMap<&str, &crate::vcs::WorktreeInfo> = live_worktrees
        .iter()
        .filter_map(|w| w.workspace.as_deref().map(|name| (name, w)))
        .collect();

    let mut nodes = Vec::with_capacity(registry.len());
    for workspace in registry {
        let canonical_service_key = crate::config::workspace_service_key(&workspace.name);
        let identity_status = if !workspace.raw_identity_verified {
            "legacy_unresolved"
        } else if workspace.service_key != canonical_service_key {
            "legacy_adopted"
        } else {
            "canonical"
        };
        let live = live_by_name.get(workspace.name.as_str()).copied();
        let path = live
            .map(|w| w.path.display().to_string())
            .or(workspace.worktree_path.clone());
        let path_exists = path.as_deref().is_some_and(|p| Path::new(p).is_dir());

        let mut services = service_by_key
            .remove(&workspace.service_key)
            .unwrap_or_default();
        for template in &service_templates {
            if !services.iter().any(|s| s.name == template.name) {
                services.push(template.clone());
            }
        }
        services.sort_by(|a, b| a.name.cmp(&b.name));

        let processes = all_processes
            .iter()
            .filter(|process| {
                process.workspace == workspace.name || process.workspace == workspace.service_key
            })
            .cloned()
            .collect::<Vec<_>>();

        let runtime_degraded = !workspace.raw_identity_verified
            || services.iter().any(|service| {
                (!service.provisioned && expected_service_names.contains(&service.name))
                    || (service.provisioned
                        && service
                            .state
                            .as_deref()
                            .is_some_and(|state| matches!(state, "error" | "failed" | "stopped")))
            })
            || processes.iter().any(|process| {
                process.required && !matches!(process.status.as_str(), "running" | "ready")
            });

        let health = if live.is_none() || !path_exists {
            "missing"
        } else if runtime_degraded {
            "degraded"
        } else {
            "ready"
        };

        let parent_state = workspace.parent.as_ref().map(|parent| {
            if known_names.contains(parent) {
                "present".to_string()
            } else {
                "missing".to_string()
            }
        });

        nodes.push(WorkspaceNode {
            children: children.remove(&workspace.name).unwrap_or_default(),
            is_default: workspace.name == default_workspace,
            is_context: context_workspace.as_deref() == Some(workspace.name.as_str()),
            worktree_path: path,
            health: health.to_string(),
            created_at: workspace.created_at.to_rfc3339(),
            executed_command: workspace.executed_command,
            execution_status: workspace.execution_status,
            services,
            processes,
            name: workspace.name,
            service_key: workspace.service_key,
            canonical_service_key,
            identity_status: identity_status.to_string(),
            parent: workspace.parent,
            parent_state,
        });
    }

    nodes.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.name.cmp(&b.name))
    });
    let node_names: HashSet<&str> = nodes.iter().map(|node| node.name.as_str()).collect();
    let mut roots = nodes
        .iter()
        .filter(|node| {
            node.parent
                .as_deref()
                .is_none_or(|parent| !node_names.contains(parent))
        })
        .map(|node| node.name.clone())
        .collect::<Vec<_>>();
    roots.sort_by(|a, b| {
        let a_default = a == &default_workspace;
        let b_default = b == &default_workspace;
        b_default.cmp(&a_default).then_with(|| a.cmp(b))
    });

    let flat_order = flatten_tree(&roots, &nodes);

    Ok(WorkspaceInventory {
        schema_version: INVENTORY_SCHEMA_VERSION,
        project: InventoryProject {
            name: config.project_name(),
            root: project_root.display().to_string(),
            vcs_provider: provider_name,
        },
        context_workspace,
        default_workspace,
        roots,
        workspaces: nodes,
        flat_order,
        warnings,
    })
}

/// One row of the canonical depth-first display order for the workspace
/// tree. Computed once here so the CLI, TUI, and GUI all render the same
/// order and the same connector glyphs instead of re-deriving the tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlatWorkspaceRow {
    pub name: String,
    pub depth: usize,
    /// Whether this node is the last of its siblings (└─ vs ├─).
    pub is_last_sibling: bool,
    /// Per ancestor level: whether that ancestor has further siblings
    /// (drives │ continuation columns).
    pub ancestor_has_next: Vec<bool>,
    pub has_children: bool,
}

/// Flatten the workspace graph depth-first: roots in inventory order, then a
/// defensive pass so corrupt/cyclic lineage stays visible instead of hidden.
pub fn flatten_tree(roots: &[String], workspaces: &[WorkspaceNode]) -> Vec<FlatWorkspaceRow> {
    let map: HashMap<&str, &WorkspaceNode> = workspaces
        .iter()
        .map(|node| (node.name.as_str(), node))
        .collect();
    let mut rows = Vec::new();
    let mut visited: HashSet<&str> = HashSet::new();

    for (i, root) in roots.iter().enumerate() {
        flatten_node(
            root,
            0,
            i + 1 == roots.len(),
            &[],
            &map,
            &mut visited,
            &mut rows,
        );
    }
    for node in workspaces {
        if !visited.contains(node.name.as_str()) {
            flatten_node(&node.name, 0, true, &[], &map, &mut visited, &mut rows);
        }
    }
    rows
}

fn flatten_node<'a>(
    name: &'a str,
    depth: usize,
    is_last_sibling: bool,
    ancestor_has_next: &[bool],
    map: &HashMap<&'a str, &'a WorkspaceNode>,
    visited: &mut HashSet<&'a str>,
    rows: &mut Vec<FlatWorkspaceRow>,
) {
    let Some(node) = map.get(name).copied() else {
        return;
    };
    if !visited.insert(&node.name) {
        return;
    }

    let children: Vec<&String> = node
        .children
        .iter()
        .filter(|child| map.contains_key(child.as_str()))
        .collect();

    rows.push(FlatWorkspaceRow {
        name: node.name.clone(),
        depth,
        is_last_sibling,
        ancestor_has_next: ancestor_has_next.to_vec(),
        has_children: !children.is_empty(),
    });

    for (i, child) in children.iter().enumerate() {
        let mut child_ancestors = ancestor_has_next.to_vec();
        child_ancestors.push(!is_last_sibling);
        flatten_node(
            child,
            depth + 1,
            i + 1 == children.len(),
            &child_ancestors,
            map,
            visited,
            rows,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{GitRepository, VcsProvider};

    #[tokio::test]
    async fn mismatch_is_warned_without_hiding_either_workspace() {
        let project = tempfile::tempdir().unwrap();
        let repo = GitRepository::init(project.path()).unwrap();
        repo.create_workspace("feature/primary", Some("main"))
            .unwrap();
        let raw = git2::Repository::open(project.path()).unwrap();
        raw.set_head("refs/heads/feature/primary").unwrap();

        let inventory = build_workspace_inventory(&Config::default(), project.path())
            .await
            .unwrap();
        assert_eq!(inventory.default_workspace, "main");
        assert!(inventory
            .workspaces
            .iter()
            .any(|workspace| workspace.name == "main"));
        assert!(inventory
            .workspaces
            .iter()
            .any(|workspace| workspace.name == "feature/primary"));
        assert!(inventory.warnings.iter().any(|warning| {
            warning.contains("Git primary workspace mismatch")
                && warning.contains("update git.main_workspace")
        }));
    }
}

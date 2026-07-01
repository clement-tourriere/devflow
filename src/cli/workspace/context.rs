use std::path::PathBuf;

use anyhow::Result;
use devflow_core::config::Config;
use devflow_core::services;
use devflow_core::state::{DevflowWorkspace, LocalStateManager};
use devflow_core::vcs;

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

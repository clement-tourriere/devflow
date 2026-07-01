use anyhow::{Context, Result};
use std::path::Path;

use crate::config::Config;
use crate::hooks::HookPhase;
use crate::services;
use crate::state::{DevflowWorkspace, LocalStateManager};
use crate::vcs;

use super::hooks::run_lifecycle_hooks_best_effort;
use super::{LifecycleOptions, ServiceResult};

/// Options for linking an existing VCS workspace into devflow state.
#[derive(Debug, Clone, Default)]
pub struct LinkOptions {
    /// Shared lifecycle options.
    pub lifecycle: LifecycleOptions,
    /// Parent workspace to record for service/materialization lineage.
    pub from_workspace: Option<String>,
}

/// Result of `link_workspace()`.
#[derive(Debug, Clone)]
pub struct LinkWorkspaceResult {
    /// Normalized workspace name.
    pub workspace: String,
    /// Recorded parent workspace.
    pub parent: Option<String>,
    /// Existing worktree path, when known.
    pub worktree_path: Option<String>,
    /// Per-service results from orchestration.
    pub services: Vec<ServiceResult>,
}

/// Link an existing VCS workspace into the devflow registry and materialize
/// matching service workspaces.
///
/// This is intentionally separate from `switch_workspace`: it records/imports a
/// workspace that was created outside devflow without changing the user's VCS
/// checkout or creating a worktree.  Lifecycle hook execution and service
/// orchestration still live here in core so CLI/TUI/GUI callers do not duplicate
/// the behavior.
pub async fn link_workspace(
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
    options: &LinkOptions,
) -> Result<LinkWorkspaceResult> {
    let normalized = config.get_normalized_workspace_name(workspace_name);
    let normalized_main = config.get_normalized_workspace_name(&config.git.main_workspace);

    if let Ok(mut state_mgr) = LocalStateManager::new() {
        let _ = state_mgr.ensure_default_workspace(project_dir, &config.git.main_workspace);
    }

    let vcs_repo =
        vcs::detect_vcs_provider(project_dir).context("Failed to open VCS repository")?;
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
        .and_then(|state| state.get_workspace_by_dir(project_dir, &normalized))
        .and_then(|b| b.parent);

    let mut parent = options
        .from_workspace
        .as_deref()
        .map(|p| config.get_normalized_workspace_name(p))
        .or(existing_parent);

    if parent.is_none() && normalized != normalized_main {
        parent = Some(normalized_main.clone());
    }

    if let Some(ref parent_workspace) = parent {
        if parent_workspace != &normalized_main
            && !workspace_is_linked(config, project_dir, parent_workspace)
        {
            anyhow::bail!(
                "Parent '{}' is not linked in devflow. Run `devflow link {}` first.",
                parent_workspace,
                parent_workspace
            );
        }
        if parent_workspace == &normalized_main {
            if let Ok(mut state_mgr) = LocalStateManager::new() {
                let _ = state_mgr.ensure_default_workspace(project_dir, &config.git.main_workspace);
            }
        }
    }

    let worktree_path = vcs_repo
        .worktree_path(workspace_name)?
        .map(|p| p.display().to_string())
        .or_else(|| {
            if normalized == normalized_main {
                vcs_repo
                    .main_worktree_dir()
                    .map(|p| p.display().to_string())
            } else {
                None
            }
        });

    register_linked_workspace(
        project_dir,
        &normalized,
        parent.clone(),
        worktree_path.clone(),
    );

    let opts = &options.lifecycle;
    if !opts.skip_hooks {
        let _ = run_lifecycle_hooks_best_effort(
            config,
            project_dir,
            workspace_name,
            HookPhase::PreSwitch,
            opts,
        )
        .await;
    }

    let services: Vec<ServiceResult> = if !opts.skip_services
        && !config.resolve_services().is_empty()
    {
        let results =
            services::factory::orchestrate_switch(config, &normalized, parent.as_deref()).await?;
        results.into_iter().map(ServiceResult::from).collect()
    } else {
        Vec::new()
    };

    let any_service_success = services.iter().any(|r| r.success);
    if any_service_success && !opts.skip_hooks {
        let _ = run_lifecycle_hooks_best_effort(
            config,
            project_dir,
            workspace_name,
            HookPhase::PostServiceSwitch,
            opts,
        )
        .await;
    }

    if !opts.skip_hooks {
        let _ = run_lifecycle_hooks_best_effort(
            config,
            project_dir,
            workspace_name,
            HookPhase::PostSwitch,
            opts,
        )
        .await;
    }

    Ok(LinkWorkspaceResult {
        workspace: normalized,
        parent,
        worktree_path,
        services,
    })
}

fn workspace_is_linked(config: &Config, project_dir: &Path, workspace_name: &str) -> bool {
    let normalized = config.get_normalized_workspace_name(workspace_name);
    LocalStateManager::new()
        .ok()
        .and_then(|state| state.get_workspace_by_dir(project_dir, &normalized))
        .is_some()
}

fn register_linked_workspace(
    project_dir: &Path,
    normalized: &str,
    parent: Option<String>,
    worktree_path: Option<String>,
) {
    let Ok(mut state_mgr) = LocalStateManager::new() else {
        return;
    };

    let existing = state_mgr.get_workspace_by_dir(project_dir, normalized);
    let workspace = DevflowWorkspace {
        name: normalized.to_string(),
        parent: parent.or_else(|| existing.as_ref().and_then(|b| b.parent.clone())),
        worktree_path: worktree_path
            .or_else(|| existing.as_ref().and_then(|b| b.worktree_path.clone())),
        created_at: existing
            .as_ref()
            .map(|b| b.created_at)
            .unwrap_or_else(chrono::Utc::now),
        executed_command: existing.as_ref().and_then(|b| b.executed_command.clone()),
        execution_status: existing.as_ref().and_then(|b| b.execution_status.clone()),
        executed_at: existing.as_ref().and_then(|b| b.executed_at),
    };

    if let Err(e) = state_mgr.register_workspace_by_dir(project_dir, workspace) {
        log::warn!("Failed to register workspace in devflow state: {}", e);
    }
}

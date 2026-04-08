use anyhow::{Context, Result};
use std::path::Path;

use crate::config::Config;
use crate::hooks::HookPhase;
use crate::services;
use crate::state::LocalStateManager;
use crate::vcs;

use super::hooks::{run_lifecycle_hooks, run_lifecycle_hooks_best_effort};
use super::{DeleteWorkspaceResult, LifecycleOptions, ServiceResult};

/// Options specific to workspace deletion.
#[derive(Debug, Clone, Default)]
pub struct DeleteOptions {
    /// Shared lifecycle options.
    pub lifecycle: LifecycleOptions,
    /// Whether to keep service workspaces (don't delete databases, etc.).
    pub keep_services: bool,
}

/// Delete a workspace with the full lifecycle: pre-remove hooks,
/// worktree removal, service deletion, VCS branch deletion, state
/// cleanup, and post-remove hooks.
///
/// **Safety checks are NOT included** — callers must verify:
/// - The workspace is not the main workspace
/// - The workspace is not currently checked out
/// - The user has confirmed the operation (if interactive)
///
/// Hook phase ordering:
///   PreRemove → worktree remove → PreServiceDelete → services →
///   PostServiceDelete → VCS delete → state cleanup → PostRemove
pub async fn delete_workspace(
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
    options: &DeleteOptions,
) -> Result<DeleteWorkspaceResult> {
    let opts = &options.lifecycle;
    // VCS is optional — `remove` must work even without a git/jj repo
    let vcs_provider = vcs::detect_vcs_provider(project_dir).ok();
    let normalized = config.get_normalized_workspace_name(workspace_name);
    let mut hook_results = Vec::new();

    // 1. Pre-remove hooks (blocking)
    if !opts.skip_hooks {
        run_lifecycle_hooks(
            config,
            project_dir,
            workspace_name,
            HookPhase::PreRemove,
            opts,
        )
        .await?;
    }

    // 2. Remove worktree (if VCS is available and worktree exists)
    let mut worktree_removed = false;
    let mut worktree_path_str: Option<String> = None;

    if let Some(ref repo) = vcs_provider {
        if let Ok(Some(wt_path)) = repo.worktree_path(workspace_name) {
            worktree_path_str = Some(wt_path.display().to_string());
            if let Err(e) = repo.remove_worktree(&wt_path) {
                log::warn!(
                    "Failed to remove worktree via VCS, falling back to fs removal: {}",
                    e
                );
                if wt_path.exists() {
                    std::fs::remove_dir_all(&wt_path)
                        .context("Failed to remove worktree directory")?;
                }
            }
            worktree_removed = true;
        }
    }

    // 3. Service deletion (unless keep_services)
    let service_results: Vec<ServiceResult> = if !options.keep_services && !opts.skip_services {
        // Pre-service-delete hooks
        if !opts.skip_hooks {
            if let Some(summary) = run_lifecycle_hooks_best_effort(
                config,
                project_dir,
                workspace_name,
                HookPhase::PreServiceDelete,
                opts,
            )
            .await
            {
                hook_results.push(summary);
            }
        }

        let results = services::factory::orchestrate_delete(config, &normalized).await?;
        let service_results: Vec<ServiceResult> =
            results.into_iter().map(ServiceResult::from).collect();

        // Post-service-delete hooks (best-effort)
        if !opts.skip_hooks {
            if let Some(summary) = run_lifecycle_hooks_best_effort(
                config,
                project_dir,
                workspace_name,
                HookPhase::PostServiceDelete,
                opts,
            )
            .await
            {
                hook_results.push(summary);
            }
        }

        service_results
    } else {
        vec![]
    };

    // 4. Delete VCS workspace
    let mut branch_deleted = false;
    if let Some(ref repo) = vcs_provider {
        match repo.delete_workspace(workspace_name) {
            Ok(_) => {
                branch_deleted = true;
            }
            Err(e) => {
                log::warn!("Failed to delete workspace '{}': {}", workspace_name, e);
            }
        }
    }

    // 5. Unregister from devflow state
    if let Ok(mut state_mgr) = LocalStateManager::new() {
        if let Err(e) = state_mgr.unregister_workspace_by_dir(project_dir, &normalized) {
            log::warn!("Failed to unregister workspace from devflow state: {}", e);
        }
    }

    // 6. Post-remove hooks (best-effort)
    if !opts.skip_hooks {
        if let Some(summary) = run_lifecycle_hooks_best_effort(
            config,
            project_dir,
            workspace_name,
            HookPhase::PostRemove,
            opts,
        )
        .await
        {
            hook_results.push(summary);
        }
    }

    Ok(DeleteWorkspaceResult {
        workspace: normalized,
        worktree_removed,
        worktree_path: worktree_path_str,
        branch_deleted,
        services: service_results,
        hooks: hook_results,
    })
}

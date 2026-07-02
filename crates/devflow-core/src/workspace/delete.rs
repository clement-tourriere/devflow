use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;

use crate::config::Config;
use crate::hooks::HookPhase;
use crate::processes;
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
    /// Remove the worktree even if it has uncommitted changes (and fall
    /// back to plain directory removal when VCS removal fails).
    pub force: bool,
}

/// Delete a workspace with the full lifecycle: pre-remove hooks,
/// worktree removal, service deletion, VCS branch deletion, state
/// cleanup, and post-remove hooks.
///
/// Refuses to delete the main workspace or the currently checked-out
/// workspace, for every frontend. Interactive confirmation (if any) remains
/// the caller's responsibility.
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

    // Safety checks — shared by CLI, TUI, and GUI so no frontend can drift.
    if normalized == config.get_normalized_workspace_name(&config.git.main_workspace) {
        anyhow::bail!("Cannot remove the main workspace '{}'", workspace_name);
    }
    if let Some(ref repo) = vcs_provider {
        if let Ok(Some(current)) = repo.current_workspace() {
            if current == workspace_name
                || config.get_normalized_workspace_name(&current) == normalized
            {
                anyhow::bail!(
                    "Cannot remove workspace '{}' because it is currently checked out. \
                     Switch to another workspace first.",
                    workspace_name
                );
            }
        }
    }
    let registered_workspace = LocalStateManager::new()
        .ok()
        .and_then(|state| state.get_workspace_by_dir(project_dir, &normalized));
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

    // 2. Stop workspace processes before deleting files/services.
    let process_results = if !opts.skip_processes {
        let results = match tokio::time::timeout(
            Duration::from_secs(20),
            processes::auto_stop_workspace_processes(config, project_dir, workspace_name),
        )
        .await
        {
            Ok(results) => results,
            Err(_) => vec![processes::ProcessResult {
                process: "(process-runtime)".to_string(),
                success: false,
                message: "timed out stopping workspace processes; continuing cleanup".to_string(),
                required: false,
                pid: None,
                ports: Vec::new(),
            }],
        };
        if results.iter().all(|r| r.success) {
            if let Err(e) =
                processes::cleanup_workspace_process_state(config, project_dir, workspace_name)
            {
                log::warn!(
                    "Failed to clean process state for '{}': {}",
                    workspace_name,
                    e
                );
            }
        }
        results
    } else {
        Vec::new()
    };

    // 3. Remove worktree (if VCS is available and worktree exists)
    let mut worktree_removed = false;
    let mut worktree_path_str: Option<String> = None;

    if let Some(ref repo) = vcs_provider {
        // First prune stale metadata. This is safe for valid worktrees and is
        // critical when the user manually deleted the worktree directory — git
        // otherwise keeps a phantom checked-out branch that blocks deletion.
        if let Err(e) = repo.prune_worktrees() {
            log::debug!("Failed to prune stale worktrees before delete: {:#}", e);
        }

        let registered_path = registered_workspace
            .as_ref()
            .and_then(|w| w.worktree_path.as_ref())
            .map(std::path::PathBuf::from);
        let wt_path = repo
            .worktree_path(workspace_name)
            .ok()
            .flatten()
            .or(registered_path);

        if let Some(wt_path) = wt_path {
            worktree_path_str = Some(wt_path.display().to_string());
            if !wt_path.exists() {
                // Already removed outside devflow. Treat this as success and
                // continue cleaning services, branch, state, and git metadata.
                worktree_removed = true;
            } else {
                match repo.remove_worktree(&wt_path, options.force) {
                    Ok(()) => worktree_removed = true,
                    Err(e) if options.force => {
                        // Forced: VCS removal failed (e.g. stale metadata) — fall
                        // back to plain directory removal.
                        log::warn!(
                            "Failed to remove worktree via VCS, falling back to fs removal: {}",
                            e
                        );
                        if wt_path.exists() {
                            std::fs::remove_dir_all(&wt_path)
                                .context("Failed to remove worktree directory")?;
                        }
                        worktree_removed = true;
                    }
                    Err(e) => {
                        // Abort before deleting services/branch — nothing has been
                        // destroyed yet and the user can retry with force.
                        return Err(e.context(format!(
                            "Refusing to delete workspace '{}'",
                            workspace_name
                        )));
                    }
                }
            }
        }
    }

    // 4. Service deletion (unless keep_services)
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

        let service_results: Vec<ServiceResult> = match tokio::time::timeout(
            Duration::from_secs(30),
            services::factory::orchestrate_delete(config, &normalized),
        )
        .await
        {
            Ok(Ok(results)) => results.into_iter().map(ServiceResult::from).collect(),
            Ok(Err(e)) => return Err(e),
            Err(_) => vec![ServiceResult {
                service_name: "(orchestration)".to_string(),
                success: false,
                message: "timed out deleting service workspaces; continuing cleanup".to_string(),
            }],
        };

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

    // 5. Delete VCS workspace
    let mut branch_deleted = false;
    if let Some(ref repo) = vcs_provider {
        match repo.delete_workspace(workspace_name) {
            Ok(_) => {
                branch_deleted = true;
            }
            Err(e) => {
                if repo.workspace_exists(workspace_name).unwrap_or(true) {
                    log::warn!("Failed to delete workspace '{}': {}", workspace_name, e);
                } else {
                    // Already deleted outside devflow.
                    branch_deleted = true;
                }
            }
        }
        if let Err(e) = repo.prune_worktrees() {
            log::debug!("Failed to prune stale worktrees after delete: {:#}", e);
        }
    } else {
        // Plain-directory / VCS-missing cleanup still removed devflow state and
        // services; there is no VCS branch left for devflow to delete.
        branch_deleted = true;
    }

    // 6. Unregister from devflow state
    if let Ok(mut state_mgr) = LocalStateManager::new() {
        if let Err(e) = state_mgr.unregister_workspace_by_dir(project_dir, &normalized) {
            log::warn!("Failed to unregister workspace from devflow state: {}", e);
        }
    }

    // 7. Post-remove hooks (best-effort)
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
        processes: process_results,
        hooks: hook_results,
    })
}

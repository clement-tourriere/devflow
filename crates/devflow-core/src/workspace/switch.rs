use anyhow::{Context, Result};
use std::path::Path;

use crate::config::Config;
use crate::hooks::HookPhase;
use crate::services;
use crate::state::{DevflowWorkspace, LocalStateManager};
use crate::vcs;

use super::hooks::{run_lifecycle_hooks, run_lifecycle_hooks_best_effort};
use super::worktree::create_worktree_with_files;
use super::{LifecycleOptions, ServiceResult, SwitchWorkspaceResult, WorktreeSetupResult};

/// Options specific to workspace switching.
#[derive(Debug, Clone)]
pub struct SwitchOptions {
    /// Shared lifecycle options.
    pub lifecycle: LifecycleOptions,
    /// Allow creating the workspace if it doesn't exist.
    pub create_if_missing: bool,
    /// Parent workspace to branch from when creating.
    pub from_workspace: Option<String>,
    /// Override the config `worktree.copy_files` for worktree creation.
    pub copy_files: Option<Vec<String>>,
    /// Override the config `worktree.copy_ignored` for worktree creation.
    pub copy_ignored: Option<bool>,
    /// Whether the workspace should be sandboxed.
    pub sandboxed: Option<bool>,
}

impl Default for SwitchOptions {
    fn default() -> Self {
        Self {
            lifecycle: LifecycleOptions::default(),
            create_if_missing: false,
            from_workspace: None,
            copy_files: None,
            copy_ignored: None,
            sandboxed: None,
        }
    }
}

/// Switch to a workspace with the full lifecycle: pre-switch hooks,
/// VCS checkout (with optional creation), worktree setup, service
/// orchestration, and post-switch hooks.
///
/// Hook phase ordering:
///   PreSwitch → VCS checkout → services → PostServiceSwitch →
///   PostCreate (if new) → PostSwitch
pub async fn switch_workspace(
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
    options: &SwitchOptions,
) -> Result<SwitchWorkspaceResult> {
    let opts = &options.lifecycle;
    let vcs_provider =
        vcs::detect_vcs_provider(project_dir).context("Failed to open VCS repository")?;

    let normalized_name = config.get_normalized_workspace_name(workspace_name);
    let worktree_enabled = config.worktree.as_ref().is_some_and(|wt| wt.enabled);

    // Ensure main workspace is registered in state
    ensure_default_workspace_registered(config, project_dir);

    // 1. Pre-switch hooks
    if !opts.skip_hooks {
        run_lifecycle_hooks(
            config,
            project_dir,
            &normalized_name,
            HookPhase::PreSwitch,
            opts,
        )
        .await?;
    }

    let mut branch_created = false;
    let mut parent_for_new: Option<String> = None;
    let mut worktree_result: Option<WorktreeSetupResult> = None;

    // 2. VCS workspace creation / checkout
    if worktree_enabled {
        // Worktree mode: check for existing worktree, create if needed
        let existing_path = vcs_provider.worktree_path(workspace_name)?;

        if let Some(wt_path) = existing_path {
            // Existing worktree — just use it
            let resolved = std::fs::canonicalize(&wt_path).unwrap_or(wt_path);
            worktree_result = Some(WorktreeSetupResult {
                path: resolved,
                created: false,
            });
        } else {
            // Need to create workspace + worktree
            let workspace_exists = vcs_provider.workspace_exists(workspace_name)?;
            if !workspace_exists {
                if !options.create_if_missing {
                    anyhow::bail!(
                        "Workspace '{}' does not exist. Use the create flag to create it.",
                        workspace_name
                    );
                }

                vcs_provider.create_workspace(workspace_name, options.from_workspace.as_deref())?;
                branch_created = true;
                parent_for_new = options.from_workspace.clone();
            }

            // Create worktree with file copying
            let wt = create_worktree_with_files(
                vcs_provider.as_ref(),
                config,
                project_dir,
                workspace_name,
                options.copy_files.as_deref(),
                options.copy_ignored,
            )?;
            worktree_result = Some(wt);
        }
    } else {
        // Classic mode (no worktrees)
        let workspace_exists = vcs_provider.workspace_exists(workspace_name)?;
        if !workspace_exists {
            if !options.create_if_missing {
                anyhow::bail!(
                    "Workspace '{}' does not exist. Use the create flag to create it.",
                    workspace_name
                );
            }
            vcs_provider.create_workspace(workspace_name, options.from_workspace.as_deref())?;
            branch_created = true;
            parent_for_new = options.from_workspace.clone();
        }
        vcs_provider.checkout_workspace(workspace_name)?;
    }

    // 3. Register workspace in state (before services, independent of their success)
    let normalized_parent = if branch_created {
        parent_for_new
            .as_deref()
            .map(|p| config.get_normalized_workspace_name(p))
    } else {
        None
    };

    register_workspace_state(
        config,
        project_dir,
        &normalized_name,
        normalized_parent.as_deref(),
        worktree_result.as_ref(),
        options.sandboxed,
    );

    // 4. Service orchestration
    let service_results: Vec<ServiceResult> =
        if !opts.skip_services && !config.resolve_services().is_empty() {
            // Determine parent for service creation
            let service_parent = if branch_created {
                normalized_parent.clone()
            } else {
                // Look up stored parent from registry
                LocalStateManager::new()
                    .ok()
                    .and_then(|state| state.get_workspace_by_dir(project_dir, &normalized_name))
                    .and_then(|b| b.parent)
            };

            let results = services::factory::orchestrate_switch(
                config,
                &normalized_name,
                service_parent.as_deref(),
            )
            .await?;

            let service_results: Vec<ServiceResult> =
                results.into_iter().map(ServiceResult::from).collect();

            // Post-service-switch hooks (only if any service succeeded)
            let any_success = service_results.iter().any(|r| r.success);
            if any_success && !opts.skip_hooks {
                run_lifecycle_hooks_best_effort(
                    config,
                    project_dir,
                    &normalized_name,
                    HookPhase::PostServiceSwitch,
                    opts,
                )
                .await;
            }

            service_results
        } else {
            vec![]
        };

    // 5. Post-create hooks (only if branch was freshly created)
    if branch_created && !opts.skip_hooks {
        run_lifecycle_hooks_best_effort(
            config,
            project_dir,
            &normalized_name,
            HookPhase::PostCreate,
            opts,
        )
        .await;
    }

    // 6. Post-switch hooks (always)
    if !opts.skip_hooks {
        run_lifecycle_hooks_best_effort(
            config,
            project_dir,
            &normalized_name,
            HookPhase::PostSwitch,
            opts,
        )
        .await;
    }

    Ok(SwitchWorkspaceResult {
        workspace: normalized_name,
        parent: normalized_parent,
        worktree: worktree_result,
        branch_created,
        services: service_results,
    })
}

fn ensure_default_workspace_registered(config: &Config, project_dir: &Path) {
    let main = &config.git.main_workspace;
    if let Ok(mut state_mgr) = LocalStateManager::new() {
        let _ = state_mgr.ensure_default_workspace(project_dir, main);
    }
}

fn register_workspace_state(
    _config: &Config,
    project_dir: &Path,
    normalized_name: &str,
    normalized_parent: Option<&str>,
    worktree: Option<&WorktreeSetupResult>,
    sandboxed: Option<bool>,
) {
    let Ok(mut state_mgr) = LocalStateManager::new() else {
        return;
    };

    // Preserve existing metadata on upsert
    let existing = state_mgr.get_workspace_by_dir(project_dir, normalized_name);

    let workspace = DevflowWorkspace {
        name: normalized_name.to_string(),
        parent: normalized_parent
            .map(String::from)
            .or_else(|| existing.as_ref().and_then(|b| b.parent.clone())),
        worktree_path: worktree
            .map(|w| w.path.display().to_string())
            .or_else(|| existing.as_ref().and_then(|b| b.worktree_path.clone())),
        created_at: existing
            .as_ref()
            .map(|b| b.created_at)
            .unwrap_or_else(chrono::Utc::now),
        executed_command: existing.as_ref().and_then(|b| b.executed_command.clone()),
        execution_status: existing.as_ref().and_then(|b| b.execution_status.clone()),
        executed_at: existing.as_ref().and_then(|b| b.executed_at),
        sandboxed: sandboxed
            .unwrap_or_else(|| existing.as_ref().map(|b| b.sandboxed).unwrap_or(false)),
    };

    if let Err(e) = state_mgr.register_workspace_by_dir(project_dir, workspace) {
        log::warn!("Failed to register workspace in devflow state: {}", e);
    }
}

use anyhow::{Context, Result};
use std::path::Path;

use crate::config::Config;
use crate::hooks::HookPhase;
use crate::services;
use crate::state::{DevflowWorkspace, LocalStateManager};
use crate::vcs;

use super::hooks::{run_lifecycle_hooks_best_effort, run_lifecycle_hooks_with_result};
use super::worktree::create_worktree_with_files;
use super::{
    CreateWorkspaceResult, LifecycleHookResult, LifecycleOptions, ServiceResult,
    WorkspaceCreationMode, WorktreeSetupResult,
};

/// Options specific to workspace creation.
#[derive(Debug, Clone)]
pub struct CreateOptions {
    /// Shared lifecycle options.
    pub lifecycle: LifecycleOptions,
    /// How to create the workspace (worktree, branch, or default from config).
    pub creation_mode: WorkspaceCreationMode,
    /// Parent workspace to branch from (like `--from`).
    pub from_workspace: Option<String>,
    /// Override the config `worktree.copy_files` for this creation.
    pub copy_files: Option<Vec<String>>,
    /// Override the config `worktree.copy_ignored` for this creation.
    pub copy_ignored: Option<bool>,
    /// Whether the workspace should be sandboxed.
    pub sandboxed: Option<bool>,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            lifecycle: LifecycleOptions::default(),
            creation_mode: WorkspaceCreationMode::Default,
            from_workspace: None,
            copy_files: None,
            copy_ignored: None,
            sandboxed: None,
        }
    }
}

/// Create a new workspace with the full lifecycle: VCS branch creation,
/// optional worktree setup, hook execution, service orchestration, and
/// state registration.
///
/// Hook phase ordering:
///   PreServiceCreate → services → PostServiceCreate → PostCreate → PostSwitch
pub async fn create_workspace(
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
    options: &CreateOptions,
) -> Result<CreateWorkspaceResult> {
    let opts = &options.lifecycle;
    let vcs_provider =
        vcs::detect_vcs_provider(project_dir).context("Failed to open VCS repository")?;

    let normalized_name = config.get_normalized_workspace_name(workspace_name);
    let normalized_parent = options
        .from_workspace
        .as_deref()
        .map(|fb| config.get_normalized_workspace_name(fb));
    let mut hook_results = Vec::new();

    // Decide whether to create a worktree
    let config_prefers_worktree = config.worktree.as_ref().is_some_and(|wt| wt.enabled);

    let create_as_worktree = match options.creation_mode {
        WorkspaceCreationMode::Default => config_prefers_worktree,
        WorkspaceCreationMode::Worktree => true,
        WorkspaceCreationMode::Branch => false,
    };

    if create_as_worktree && !vcs_provider.supports_worktrees() {
        anyhow::bail!(
            "VCS provider '{}' does not support worktrees",
            vcs_provider.provider_name()
        );
    }

    // 1. Create VCS branch
    vcs_provider.create_workspace(workspace_name, options.from_workspace.as_deref())?;

    // 2. Create worktree if enabled
    let worktree_result = if create_as_worktree {
        Some(create_worktree_with_files(
            vcs_provider.as_ref(),
            config,
            project_dir,
            workspace_name,
            options.copy_files.as_deref(),
            options.copy_ignored,
        )?)
    } else {
        None
    };

    // 3. Pre-service-create hooks
    if !opts.skip_hooks {
        if let Some(summary) = run_lifecycle_hooks_best_effort(
            config,
            project_dir,
            workspace_name,
            HookPhase::PreServiceCreate,
            opts,
        )
        .await
        {
            hook_results.push(summary);
        }
    }

    // 4. Service orchestration
    let service_results: Vec<ServiceResult> = if !opts.skip_services {
        let results = services::factory::orchestrate_create(
            config,
            workspace_name,
            options.from_workspace.as_deref(),
        )
        .await?;
        results.into_iter().map(ServiceResult::from).collect()
    } else {
        vec![]
    };

    // 5. Post-service-create hooks
    if !opts.skip_hooks {
        if let Some(summary) = run_lifecycle_hooks_best_effort(
            config,
            project_dir,
            workspace_name,
            HookPhase::PostServiceCreate,
            opts,
        )
        .await
        {
            hook_results.push(summary);
        }
    }

    // 6. Register in devflow state
    register_workspace_state(
        config,
        project_dir,
        &normalized_name,
        normalized_parent.as_deref(),
        worktree_result.as_ref(),
        options.sandboxed,
    );

    // 7. Post-create + post-switch hooks
    if !opts.skip_hooks {
        let post_create = run_lifecycle_hooks_with_result(
            config,
            project_dir,
            workspace_name,
            HookPhase::PostCreate,
            opts,
        )
        .await?;
        hook_results.push(LifecycleHookResult::from_run_result(
            &HookPhase::PostCreate,
            post_create,
        ));

        if let Some(summary) = run_lifecycle_hooks_best_effort(
            config,
            project_dir,
            workspace_name,
            HookPhase::PostSwitch,
            opts,
        )
        .await
        {
            hook_results.push(summary);
        }
    }

    Ok(CreateWorkspaceResult {
        workspace: normalized_name,
        parent: normalized_parent,
        worktree: worktree_result,
        branch_created: true,
        services: service_results,
        hooks: hook_results,
    })
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

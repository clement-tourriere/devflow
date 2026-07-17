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
    /// Exact VCS branch/bookmark name.
    pub workspace: String,
    /// Collision-resistant service/filesystem identity.
    pub service_key: String,
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
/// materialized workspace that was created outside devflow without changing
/// the user's VCS context. Lifecycle hook execution and service
/// orchestration still live here in core so CLI/TUI/GUI callers do not duplicate
/// the behavior.
pub async fn link_workspace(
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
    options: &LinkOptions,
) -> Result<LinkWorkspaceResult> {
    super::validate_workspace_name(workspace_name).map_err(anyhow::Error::msg)?;
    let main_workspace = &config.git.main_workspace;

    let vcs_repo =
        vcs::detect_vcs_provider(project_dir).context("Failed to open VCS repository")?;
    super::invariant::ensure_git_primary_workspace_matches_config(config, vcs_repo.as_ref())?;

    LocalStateManager::new()?.ensure_default_workspace(project_dir, &config.git.main_workspace)?;
    let service_key = LocalStateManager::new()?
        .resolve_workspace_service_key_by_dir(project_dir, workspace_name)?;

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
        .and_then(|state| state.get_workspace_by_dir(project_dir, workspace_name))
        .and_then(|workspace| workspace.parent);
    let parent = resolve_link_parent(
        main_workspace,
        workspace_name,
        existing_parent,
        options.from_workspace.clone(),
    )?;
    let parent_service_key = parent
        .as_deref()
        .map(|parent| {
            LocalStateManager::new()?.resolve_workspace_service_key_by_dir(project_dir, parent)
        })
        .transpose()?;

    if let Some(ref parent_workspace) = parent {
        if parent_workspace != main_workspace && !workspace_is_linked(project_dir, parent_workspace)
        {
            anyhow::bail!(
                "Parent '{}' is not linked in devflow. Run `devflow link {}` first.",
                parent_workspace,
                parent_workspace
            );
        }
    }

    let worktree_path = vcs_repo
        .worktree_path(workspace_name)?
        .map(|p| p.display().to_string())
        .or_else(|| {
            if workspace_name == main_workspace {
                vcs_repo
                    .main_worktree_dir()
                    .map(|p| p.display().to_string())
            } else {
                None
            }
        });

    if worktree_path.is_none() {
        anyhow::bail!(
            "Workspace '{}' has no materialized worktree. Run `devflow switch {}` to create it before linking.",
            workspace_name,
            workspace_name
        );
    }

    register_linked_workspace(
        config,
        project_dir,
        workspace_name,
        &service_key,
        parent.clone(),
        worktree_path.clone(),
    )?;

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

    let services: Vec<ServiceResult> =
        if !opts.skip_services && !config.resolve_services().is_empty() {
            let results = services::factory::orchestrate_switch(
                config,
                &service_key,
                parent_service_key.as_deref(),
            )
            .await?;
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
        workspace: workspace_name.to_string(),
        service_key,
        parent,
        worktree_path,
        services,
    })
}

fn workspace_is_linked(project_dir: &Path, workspace_name: &str) -> bool {
    LocalStateManager::new()
        .ok()
        .and_then(|state| state.get_workspace_by_dir(project_dir, workspace_name))
        .is_some()
}

fn resolve_link_parent(
    main_workspace: &str,
    workspace_name: &str,
    existing_parent: Option<String>,
    requested_parent: Option<String>,
) -> Result<Option<String>> {
    if let (Some(existing), Some(requested)) =
        (existing_parent.as_deref(), requested_parent.as_deref())
    {
        if existing != requested {
            anyhow::bail!(
                "Workspace '{}' was created from '{}'; parent provenance is immutable and cannot be changed to '{}'",
                workspace_name,
                existing,
                requested
            );
        }
    }

    let parent = existing_parent.or(requested_parent);
    if workspace_name == main_workspace && parent.is_some() {
        anyhow::bail!(
            "The default workspace '{}' is the project root and cannot have a parent",
            workspace_name
        );
    }
    if parent.as_deref() == Some(workspace_name) {
        anyhow::bail!("Workspace '{}' cannot be its own parent", workspace_name);
    }
    // No --from and no recorded provenance (manual worktrees are adopted
    // with `parent: None`): default to the main workspace. Leaving the
    // parent unset would hand service provisioning a `from_workspace=None`,
    // and e.g. the postgres-local provider then clones the new database
    // from an ARBITRARY existing workspace (most recently created) instead
    // of main.
    if parent.is_none() && workspace_name != main_workspace {
        return Ok(Some(main_workspace.to_string()));
    }
    Ok(parent)
}

fn register_linked_workspace(
    _config: &Config,
    project_dir: &Path,
    workspace_name: &str,
    service_key: &str,
    parent: Option<String>,
    worktree_path: Option<String>,
) -> Result<()> {
    let mut state_mgr = LocalStateManager::new()?;

    let existing = state_mgr.get_workspace_by_dir(project_dir, workspace_name);
    let workspace = DevflowWorkspace {
        name: workspace_name.to_string(),
        service_key: service_key.to_string(),
        raw_identity_verified: true,
        parent: existing
            .as_ref()
            .and_then(|workspace| workspace.parent.clone())
            .or(parent),
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

    state_mgr.register_workspace_by_dir(project_dir, workspace)
}

#[cfg(test)]
mod tests {
    use super::resolve_link_parent;

    #[test]
    fn default_and_self_parent_are_rejected() {
        assert!(resolve_link_parent("main", "main", None, Some("main".into())).is_err());
        assert!(
            resolve_link_parent("main", "feature/auth", None, Some("feature/auth".into())).is_err()
        );
    }

    #[test]
    fn known_parent_is_immutable_but_unknown_parent_can_be_repaired() {
        let error = resolve_link_parent(
            "main",
            "feature/auth",
            Some("main".into()),
            Some("release".into()),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("immutable"));

        assert_eq!(
            resolve_link_parent("main", "feature/auth", None, Some("main".into())).unwrap(),
            Some("main".into())
        );
    }
}

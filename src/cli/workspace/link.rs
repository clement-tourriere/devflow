use super::context::BranchContext;
use anyhow::{Context, Result};
use devflow_core::config::Config;
use devflow_core::hooks::HookPhase;
use devflow_core::services;
use devflow_core::state::{DevflowWorkspace, LocalStateManager};
use devflow_core::vcs;
use std::path::PathBuf;

use super::context::{ensure_default_workspace_registered, linked_workspace_exists};

#[derive(Debug, Clone)]
struct LinkServiceResult {
    service_name: String,
    success: bool,
    message: String,
}

#[derive(Debug, Clone)]
pub(super) struct LinkBranchResult {
    workspace: String,
    parent: Option<String>,
    worktree_path: Option<String>,
    service_results: Vec<LinkServiceResult>,
    services_failed: usize,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn link_branch_internal(
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

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_link_command(
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

#[allow(clippy::too_many_arguments)]
pub(super) async fn resolve_parent_for_branch_creation(
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

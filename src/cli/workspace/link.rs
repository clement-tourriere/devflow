use super::context::BranchContext;
use anyhow::Result;
use devflow_core::config::Config;
use devflow_core::workspace::link::{LinkOptions, LinkWorkspaceResult};
use std::path::PathBuf;

use super::context::{ensure_default_workspace_registered, linked_workspace_exists};

pub(super) type LinkBranchResult = LinkWorkspaceResult;

fn services_failed(linked: &LinkWorkspaceResult) -> usize {
    linked.services.iter().filter(|r| !r.success).count()
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn link_branch_internal(
    config: &Config,
    config_path: &Option<PathBuf>,
    workspace_name: &str,
    from: Option<&str>,
    non_interactive: bool,
) -> Result<LinkBranchResult> {
    let project_dir = super::super::operation_project_dir(config_path);

    let options = LinkOptions {
        lifecycle: devflow_core::workspace::LifecycleOptions {
            hook_approval: if non_interactive {
                devflow_core::workspace::hooks::HookApprovalMode::NonInteractive
            } else {
                devflow_core::workspace::hooks::HookApprovalMode::Interactive
            },
            verbose_hooks: true,
            ..Default::default()
        },
        from_workspace: from.map(ToString::to_string),
    };

    devflow_core::workspace::link::link_workspace(config, &project_dir, workspace_name, &options)
        .await
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

    let failed = services_failed(&linked);

    if json_output {
        let service_results: Vec<serde_json::Value> = linked
            .services
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
                "status": if failed == 0 { "ok" } else { "error" },
                "workspace": linked.workspace,
                "service_key": linked.service_key,
                "parent": linked.parent,
                "worktree_path": linked.worktree_path,
                "services_failed": failed,
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

        if linked.services.is_empty() {
            println!("  Services: none configured");
        } else {
            for r in &linked.services {
                if r.success {
                    println!("  [{}] {}", r.service_name, r.message);
                } else {
                    println!("  [{}] Warning: {}", r.service_name, r.message);
                }
            }
        }
    }

    if failed > 0 {
        anyhow::bail!(
            "Linked workspace '{}' but failed on {}/{} service(s)",
            linked.workspace,
            failed,
            linked.services.len()
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
        .or_else(|| context.context_branch.clone());

    let Some(parent_name) = parent.as_deref() else {
        return Ok(None);
    };

    if parent_name == target_workspace {
        anyhow::bail!(
            "Parent workspace '{}' is the target workspace '{}'. Choose a different --from value.",
            parent_name,
            target_workspace
        );
    }

    // If we have no project config path, we cannot enforce workspace-link checks.
    if config_path.is_none() {
        return Ok(parent);
    }

    if linked_workspace_exists(config_path, parent_name) {
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
        let failed = services_failed(&linked);
        if failed > 0 {
            anyhow::bail!(
                "Linked parent '{}' but failed on {}/{} service(s)",
                parent_name,
                failed,
                linked.services.len()
            );
        }
        return Ok(parent);
    }

    if choice.starts_with("Use default workspace") {
        if !linked_workspace_exists(config_path, &default_workspace) {
            match link_branch_internal(config, config_path, &default_workspace, None, false).await {
                Ok(linked) if services_failed(&linked) == 0 => {}
                Ok(linked) => {
                    anyhow::bail!(
                        "Linked default workspace '{}' but failed on {}/{} service(s)",
                        default_workspace,
                        services_failed(&linked),
                        linked.services.len()
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

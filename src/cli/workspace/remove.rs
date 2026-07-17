use anyhow::Result;
use devflow_core::config::Config;
use devflow_core::vcs;

pub(super) async fn handle_remove_command(
    config: &Config,
    workspace_name: &str,
    force: bool,
    keep_services: bool,
    config_path: &Option<std::path::PathBuf>,
    json_output: bool,
    non_interactive: bool,
) -> Result<()> {
    // Safety checks (main workspace / currently checked out) live in the
    // shared core `delete_workspace`.
    let vcs_repo = vcs::detect_vcs_provider(".").ok();

    // Confirm unless --force (skip prompt in JSON/non-interactive mode — require --force)
    if !force {
        if json_output || non_interactive {
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "error",
                        "workspace": workspace_name,
                        "error": "Use --force to confirm removal in non-interactive or JSON output mode",
                    }))?
                );
            }
            anyhow::bail!("Use --force to confirm removal in non-interactive or JSON output mode");
        }
        println!("This will remove:");
        if vcs_repo.is_some() {
            println!("  - VCS workspace: {}", workspace_name);
        }
        if let Some(ref repo) = vcs_repo {
            if repo.worktree_path(workspace_name)?.is_some() {
                println!("  - Worktree directory");
            }
        }
        if !keep_services {
            println!("  - Associated service workspaces");
        }
        print!("Continue? [y/N] ");
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    // ── Delegate to core lifecycle ──────────────────────────────────
    let approval_mode = if non_interactive || json_output {
        devflow_core::workspace::hooks::HookApprovalMode::NonInteractive
    } else {
        devflow_core::workspace::hooks::HookApprovalMode::Interactive
    };

    let project_dir = super::super::operation_project_dir(config_path);

    let options = devflow_core::workspace::delete::DeleteOptions {
        lifecycle: devflow_core::workspace::LifecycleOptions {
            skip_hooks: false,
            skip_services: false,
            hook_approval: approval_mode,
            verbose_hooks: !json_output,
            ..Default::default()
        },
        keep_services,
        force,
    };

    let result = match devflow_core::workspace::delete::delete_workspace(
        config,
        &project_dir,
        workspace_name,
        &options,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "error",
                        "workspace": workspace_name,
                        "error": error.to_string(),
                    }))?
                );
            }
            return Err(error);
        }
    };

    // ── CLI-specific output ──────────────────────────────────────────
    let service_failures = result.services.iter().filter(|r| !r.success).count();
    let process_failures = result
        .processes
        .iter()
        .filter(|r| !r.success && r.required)
        .count();

    if json_output {
        let service_json: Vec<serde_json::Value> = result
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
        let process_json: Vec<serde_json::Value> = result
            .processes
            .iter()
            .map(|r| {
                serde_json::json!({
                    "process": r.process,
                    "success": r.success,
                    "message": r.message,
                    "required": r.required,
                    "pid": r.pid,
                    "ports": r.ports,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": if service_failures == 0 && process_failures == 0 && result.vcs_ref_deleted { "ok" } else { "error" },
                "workspace": workspace_name,
                "service_key": result.service_key,
                "vcs_ref_deleted": result.vcs_ref_deleted,
                "worktree_removed": result.worktree_removed,
                "worktree_path": result.worktree_path,
                "services_skipped": keep_services,
                "service_failures": service_failures,
                "service_results": service_json,
                "process_failures": process_failures,
                "process_results": process_json,
            }))?
        );
    } else {
        if result.worktree_removed {
            if let Some(ref wt) = result.worktree_path {
                println!("Removed worktree: {}", wt);
            }
        }
        for r in &result.processes {
            if r.success {
                let required = if r.required { "" } else { " (optional)" };
                println!("  [process:{}{}] {}", r.process, required, r.message);
            } else {
                let required = if r.required { "" } else { " (optional)" };
                println!(
                    "  [process:{}{}] Warning: {}",
                    r.process, required, r.message
                );
            }
        }
        for r in &result.services {
            if r.success {
                println!("  [{}] {}", r.service_name, r.message);
            } else {
                println!("  [{}] Warning: {}", r.service_name, r.message);
            }
        }
        if result.vcs_ref_deleted {
            println!("Workspace deleted: {}", workspace_name);
        }
        if service_failures == 0 && result.vcs_ref_deleted {
            println!("Workspace '{}' removed successfully.", workspace_name);
        } else {
            println!(
                "Workspace '{}' removal completed with errors.",
                workspace_name
            );
        }
    }

    if process_failures > 0 {
        anyhow::bail!(
            "Failed to stop {}/{} process(es)",
            process_failures,
            result.processes.len()
        );
    }

    if service_failures > 0 {
        anyhow::bail!(
            "Failed to remove service workspaces on {}/{} service(s)",
            service_failures,
            result.services.len()
        );
    }

    if !result.vcs_ref_deleted {
        anyhow::bail!("Failed to delete VCS workspace '{}'", workspace_name);
    }

    Ok(())
}

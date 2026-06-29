use std::path::PathBuf;

use anyhow::Result;
use devflow_core::config::Config;
use devflow_core::services::{self};
use devflow_core::vcs;

mod context;
mod exec;
mod interactive;
mod link;
mod list;
mod merge;
mod remove;

pub(crate) use context::{
    collect_list_workspace_names, context_matches_branch, ensure_default_workspace_registered,
    load_registry_branches_for_list, resolve_branch_context, BranchContextSource,
};
use interactive::handle_interactive_switch;
use link::{handle_link_command, resolve_parent_for_branch_creation};
use list::{enrich_branch_list_json, handle_environment_graph, print_enriched_branch_list};
use merge::{handle_merge_command, handle_rebase_command};
use remove::handle_remove_command;

pub(super) async fn handle_branch_command(
    cmd: super::Commands,
    config: &mut Config,
    json_output: bool,
    non_interactive: bool,
    database_name: Option<&str>,
    config_path: &Option<std::path::PathBuf>,
) -> Result<()> {
    match cmd {
        super::Commands::List => {
            // List: show combined VCS + service workspace info
            let has_multiple_services = config.resolve_services().len() > 1;
            if database_name.is_none() && has_multiple_services {
                return super::service::handle_multi_service_aggregation(
                    super::service::ServiceAggregation::List,
                    config,
                    json_output,
                    config_path,
                )
                .await;
            }

            // Try to resolve a service provider; if none is available we
            // still show VCS workspaces with an empty service workspace list.
            let (provider_name, workspaces) =
                match services::factory::resolve_provider(config, database_name).await {
                    Ok(named) => {
                        let workspaces = named.provider.list_workspaces().await?;
                        (named.provider.provider_name().to_string(), workspaces)
                    }
                    Err(_) => {
                        // No service provider available — still show VCS workspaces.
                        ("none".to_string(), Vec::new())
                    }
                };

            if json_output {
                let enriched = enrich_branch_list_json(&workspaces, config, config_path);
                println!("{}", serde_json::to_string_pretty(&enriched)?);
            } else {
                if provider_name == "none" {
                    println!("Branches (no service configured):");
                } else {
                    println!("Branches ({}):", provider_name);
                }
                print_enriched_branch_list(&workspaces, config, config_path);
            }
        }
        super::Commands::Graph => {
            handle_environment_graph(config, config_path, json_output).await?;
        }
        super::Commands::Link {
            workspace_name,
            from,
        } => {
            handle_link_command(
                config,
                config_path,
                &workspace_name,
                from.as_deref(),
                json_output,
                non_interactive,
            )
            .await?;
        }
        super::Commands::Switch {
            workspace_name,
            create,
            from,
            execute,
            detach,
            open,
            execute_args,
            no_services,
            no_processes,
            no_verify,
            template,
            dry_run,
            no_respect_gitignore,
            sandboxed,
            no_sandbox,
        } => {
            let sandbox_resolved = if sandboxed || no_sandbox {
                Some(devflow_core::sandbox::resolve_sandbox_enabled(
                    sandboxed,
                    no_sandbox,
                    false,
                    config.sandbox.as_ref(),
                ))
            } else {
                let is_sandboxed = devflow_core::sandbox::resolve_sandbox_enabled(
                    false,
                    false,
                    false,
                    config.sandbox.as_ref(),
                );
                if is_sandboxed {
                    Some(true)
                } else {
                    None
                }
            };

            if dry_run {
                if let Some(ref workspace) = workspace_name {
                    let normalized_branch = config.get_normalized_workspace_name(workspace);
                    let worktree_enabled = config.worktree.as_ref().is_some_and(|wt| wt.enabled);
                    let context = resolve_branch_context(config);
                    let default_parent = if create {
                        from.clone().or_else(|| context.context_branch_raw.clone())
                    } else {
                        None
                    };
                    let workspace_exists = vcs::detect_vcs_provider(".")
                        .ok()
                        .and_then(|repo| repo.workspace_exists(workspace).ok());

                    let project_dir = config_path
                        .as_ref()
                        .and_then(|p| p.parent())
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| PathBuf::from("."));

                    if json_output {
                        let mut wt_path_value = serde_json::Value::Null;
                        if worktree_enabled {
                            let wt_path = super::config::resolve_cd_target(
                                &devflow_core::workspace::worktree::resolve_worktree_path(
                                    config,
                                    &project_dir,
                                    &normalized_branch,
                                ),
                            )?;
                            wt_path_value =
                                serde_json::Value::String(wt_path.display().to_string());
                        }
                        let auto_providers: Vec<serde_json::Value> = if !no_services {
                            config
                                .resolve_services()
                                .into_iter()
                                .filter(|b| b.auto_workspace)
                                .map(|b| {
                                    serde_json::json!({
                                        "name": b.name,
                                        "service_type": b.service_type,
                                    })
                                })
                                .collect()
                        } else {
                            vec![]
                        };
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "dry_run": true,
                                "workspace": normalized_branch,
                                "worktree_enabled": worktree_enabled,
                                "worktree_path": wt_path_value,
                                "parent": default_parent,
                                "workspace_exists": workspace_exists,
                                "services_skipped": no_services,
                                "auto_branch_services": auto_providers,
                                "processes_skipped": no_processes,
                                "auto_start_processes": if !no_processes { config.processes.as_ref().map(|p| p.daemons.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() } else { Vec::<String>::new() },
                                "hooks_skipped": no_verify,
                                "execute": execute,
                                "would_fail_without_create": workspace_exists == Some(false) && !create,
                            }))?
                        );
                    } else {
                        println!("Dry run: would switch to workspace: {}", normalized_branch);
                        if let Some(ref parent) = default_parent {
                            println!("  Parent workspace: {}", parent);
                        }
                        if workspace_exists == Some(false) && !create {
                            println!(
                                "  Note: workspace does not exist; this would fail (use -c to create it)"
                            );
                        }
                        if worktree_enabled {
                            println!("  Worktree mode: enabled");
                            let wt_path = super::config::resolve_cd_target(
                                &devflow_core::workspace::worktree::resolve_worktree_path(
                                    config,
                                    &project_dir,
                                    &normalized_branch,
                                ),
                            )?;
                            println!("  Worktree path: {}", wt_path.display());
                        }
                        if !no_services {
                            let auto_providers = config
                                .resolve_services()
                                .into_iter()
                                .filter(|b| b.auto_workspace)
                                .collect::<Vec<_>>();
                            if auto_providers.is_empty() {
                                println!(
                                    "  Would not switch any service workspaces (none configured)"
                                );
                            } else {
                                println!(
                                    "  Would create/switch service workspaces on {} service(s):",
                                    auto_providers.len()
                                );
                                for b in &auto_providers {
                                    println!("    - {} ({})", b.name, b.service_type);
                                }
                            }
                        }
                        if !no_processes {
                            if let Some(processes) = config.processes.as_ref() {
                                if !processes.daemons.is_empty() {
                                    println!(
                                        "  Would auto-start {} process(es):",
                                        processes.daemons.len()
                                    );
                                    for name in processes.daemons.keys() {
                                        println!("    - {}", name);
                                    }
                                }
                            }
                        }
                        if !no_verify && config.hooks.is_some() {
                            println!("  Would run post-switch hooks");
                        }
                        if let Some(ref cmd) = execute {
                            println!("  Would execute after switch: {}", cmd);
                        }
                    }
                } else {
                    anyhow::bail!("Dry run requires a workspace name");
                }
            } else if template {
                handle_switch_to_main(
                    config,
                    config_path,
                    json_output,
                    no_services,
                    no_processes,
                    no_verify,
                    non_interactive,
                    None,
                    None,
                )
                .await?;
            } else if let Some(ref workspace) = workspace_name {
                if workspace == &config.git.main_workspace {
                    handle_switch_to_main(
                        config,
                        config_path,
                        json_output,
                        no_services,
                        no_processes,
                        no_verify,
                        non_interactive,
                        None,
                        None,
                    )
                    .await?;
                } else {
                    handle_switch_command(
                        config,
                        workspace,
                        config_path,
                        create,
                        from.as_deref(),
                        no_services,
                        no_processes,
                        no_verify,
                        json_output,
                        non_interactive,
                        None,
                        None,
                        if no_respect_gitignore {
                            Some(true)
                        } else {
                            None
                        },
                        sandbox_resolved,
                    )
                    .await?;
                }
            } else if non_interactive {
                anyhow::bail!(
                    "No workspace specified. Use 'devflow switch <workspace>' in non-interactive mode."
                );
            } else {
                handle_interactive_switch(config, config_path).await?;
            }

            // Execute command or open interactive session in workspace
            if open || execute.is_some() {
                let workspace = workspace_name
                    .as_deref()
                    .unwrap_or(&config.git.main_workspace);
                let cmd = execute.as_deref().unwrap_or("");
                exec::execute_in_workspace(
                    config,
                    config_path,
                    workspace,
                    cmd,
                    &execute_args,
                    detach || open,
                    sandbox_resolved,
                    json_output,
                )
                .await?;
            }
        }
        super::Commands::Remove {
            workspace_name,
            force,
            keep_services,
        } => {
            handle_remove_command(
                config,
                &workspace_name,
                force,
                keep_services,
                config_path,
                json_output,
                non_interactive,
            )
            .await?;
        }
        super::Commands::Merge {
            target,
            cleanup,
            dry_run,
            force,
            check_only,
            cascade_rebase,
        } => {
            handle_merge_command(
                config,
                target.as_deref(),
                cleanup,
                dry_run,
                json_output,
                force,
                check_only,
                cascade_rebase,
            )
            .await?;
        }
        super::Commands::Rebase { target, dry_run } => {
            handle_rebase_command(config, target.as_deref(), dry_run, json_output).await?;
        }
        super::Commands::Train { action } => {
            super::train::handle_train_command(config, action, json_output).await?;
        }
        super::Commands::Cleanup { max_count } => {
            // Top-level alias for `devflow service cleanup`
            return super::service::handle_service_provider_command(
                super::ServiceCommands::Cleanup { max_count },
                config,
                json_output,
                non_interactive,
                database_name,
                config_path,
            )
            .await;
        }
        super::Commands::Doctor => {
            // Run pre-checks (VCS, config, hooks). Track failures so `doctor`
            // can exit non-zero — usable as a CI/script health gate.
            let mut healthy = true;
            if !json_output {
                healthy = super::config::run_doctor_pre_checks(config, config_path);
            }
            let has_multiple_services = config.resolve_services().len() > 1;
            if database_name.is_none() && has_multiple_services {
                return super::service::handle_multi_service_aggregation(
                    super::service::ServiceAggregation::Doctor,
                    config,
                    json_output,
                    config_path,
                )
                .await;
            }
            // Service-specific doctor report is optional
            match services::factory::resolve_provider(config, database_name).await {
                Ok(named) => {
                    let report = named.provider.doctor().await?;
                    let service_healthy = report.checks.iter().all(|c| c.available);
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "general": {
                                    "config_path": config_path.as_ref().map(|p| p.display().to_string()),
                                },
                                "service": report,
                            }))?
                        );
                    } else {
                        println!("Service ({}):", named.provider.provider_name());
                        for check in &report.checks {
                            let icon = if check.available { "OK" } else { "FAIL" };
                            println!("  [{}] {}: {}", icon, check.name, check.detail);
                        }
                    }
                    healthy = healthy && service_healthy;
                }
                Err(_) => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "general": {
                                    "config_path": config_path.as_ref().map(|p| p.display().to_string()),
                                },
                                "services": null,
                            }))?
                        );
                    } else {
                        println!("Services:");
                        println!("  [WARN] No service provider available (run 'devflow service add' to configure one)");
                    }
                }
            }
            if !healthy {
                anyhow::bail!("devflow doctor reported one or more failing checks");
            }
        }
        super::Commands::GitHook {
            worktree,
            main_worktree_dir,
        } => {
            super::git_hook::handle_git_hook(config, config_path, worktree, main_worktree_dir)
                .await?;
        }
        super::Commands::WorktreeSetup => {
            super::git_hook::handle_worktree_setup(config, config_path).await?;
        }
        _ => anyhow::bail!("command is not handled by the workspace dispatch path"),
    }

    Ok(())
}

// ── Interactive switch ─────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_switch_command(
    config: &Config,
    workspace_name: &str,
    config_path: &Option<std::path::PathBuf>,
    create: bool,
    from: Option<&str>,
    no_services: bool,
    no_processes: bool,
    no_verify: bool,
    json_output: bool,
    non_interactive: bool,
    trigger_source: Option<&str>,
    vcs_event: Option<&str>,
    copy_ignored_override: Option<bool>,
    sandboxed: Option<bool>,
) -> Result<()> {
    // Resolve parent via CLI-specific interactive prompt (if needed)
    let from_workspace = if create {
        let context = resolve_branch_context(config);
        resolve_parent_for_branch_creation(
            config,
            config_path,
            workspace_name,
            from,
            &context,
            json_output,
            non_interactive,
        )
        .await?
    } else {
        from.map(|s| s.to_string())
    };

    let approval_mode = if non_interactive || json_output {
        devflow_core::workspace::hooks::HookApprovalMode::NonInteractive
    } else {
        devflow_core::workspace::hooks::HookApprovalMode::Interactive
    };

    let project_dir = config_path
        .as_ref()
        .and_then(|p| p.parent())
        .map(|d| d.to_path_buf())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    let options = devflow_core::workspace::switch::SwitchOptions {
        lifecycle: devflow_core::workspace::LifecycleOptions {
            skip_hooks: no_verify,
            skip_services: no_services,
            skip_processes: no_processes,
            hook_approval: approval_mode,
            verbose_hooks: !json_output,
            trigger_source: trigger_source.map(String::from),
            vcs_event: vcs_event.map(String::from),
        },
        create_if_missing: create,
        from_workspace,
        copy_files: None,
        copy_ignored: copy_ignored_override,
        sandboxed,
    };

    let result = devflow_core::workspace::switch::switch_workspace(
        config,
        &project_dir,
        workspace_name,
        &options,
    )
    .await?;

    // ── CLI-specific output ──────────────────────────────────────────
    let worktree_enabled = config.worktree.as_ref().is_some_and(|wt| wt.enabled);
    let shell_integration = super::config::shell_integration_enabled();

    // Worktree DEVFLOW_CD output
    if let Some(ref wt) = result.worktree {
        if !json_output {
            if wt.created {
                println!(
                    "Created worktree for '{}' at {}",
                    workspace_name,
                    wt.path.display(),
                );
            } else {
                println!("Switching to existing worktree: {}", wt.path.display());
            }
            println!("DEVFLOW_CD={}", wt.path.display());
            if !shell_integration {
                super::config::print_manual_cd_hint(&wt.path);
            }
        }
    } else if !json_output {
        if result.branch_created {
            println!(
                "Creating workspace '{}' (parent: {})",
                workspace_name,
                result.parent.as_deref().unwrap_or("HEAD")
            );
        }
        println!("Switched git workspace: {}", result.workspace);
    }

    // Service/process results output
    let success_count = result.services.iter().filter(|r| r.success).count();
    let fail_count = result.services.iter().filter(|r| !r.success).count();
    let process_success_count = result.processes.iter().filter(|r| r.success).count();
    let process_fail_count = result
        .processes
        .iter()
        .filter(|r| !r.success && r.required)
        .count();

    if json_output {
        let service_results: Vec<serde_json::Value> = result
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
        let process_results: Vec<serde_json::Value> = result
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
        let hook_results: Vec<serde_json::Value> = result
            .hooks
            .iter()
            .map(|r| {
                serde_json::json!({
                    "phase": r.phase,
                    "succeeded": r.succeeded,
                    "failed": r.failed,
                    "skipped": r.skipped,
                    "background": r.background,
                    "errors": r.errors,
                })
            })
            .collect();
        let summary = serde_json::json!({
            "workspace": result.workspace,
            "parent": result.parent,
            "worktree_path": result.worktree.as_ref().map(|w| w.path.display().to_string()),
            "worktree_created": result.worktree.as_ref().map(|w| w.created).unwrap_or(false),
            "services_switched": success_count,
            "services_failed": fail_count,
            "services_skipped": no_services,
            "service_results": service_results,
            "processes_started": process_success_count,
            "processes_failed": process_fail_count,
            "processes_skipped": no_processes,
            "process_results": process_results,
            "hook_results": hook_results,
        });
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else if !no_services && !result.services.is_empty() {
        for r in &result.services {
            if r.success {
                log::info!("[{}] {}", r.service_name, r.message);
            } else {
                println!("Warning: {}", r.message);
            }
        }

        if success_count > 0 && fail_count == 0 {
            println!(
                "Switched to service workspace: {} ({} service(s))",
                result.workspace, success_count
            );
        } else if success_count > 0 {
            println!(
                "Switched to service workspace: {} ({}/{} service(s), {} failed)",
                result.workspace,
                success_count,
                result.services.len(),
                fail_count
            );
        } else {
            println!(
                "Warning: Failed to switch service workspaces on all {} service(s)",
                result.services.len()
            );
        }

        if fail_count > 0 {
            anyhow::bail!(
                "Failed to switch service workspaces on {}/{} service(s)",
                fail_count,
                result.services.len()
            );
        }
    } else if !no_services && !json_output {
        if worktree_enabled {
            println!("Selected workspace/worktree: {}", result.workspace);
        }
        println!("  (no services configured — use 'devflow service add' to add one)");
    }

    if !json_output && !result.processes.is_empty() {
        for r in &result.processes {
            if r.success {
                let ports = if r.ports.is_empty() {
                    String::new()
                } else {
                    format!(" ports={:?}", r.ports)
                };
                let required = if r.required { "" } else { " (optional)" };
                println!(
                    "  [process:{}{}] {}{}",
                    r.process, required, r.message, ports
                );
            } else {
                let required = if r.required { "" } else { " (optional)" };
                println!(
                    "  [process:{}{}] Warning: {}",
                    r.process, required, r.message
                );
            }
        }
    }

    if process_fail_count > 0 {
        anyhow::bail!(
            "Failed to start {}/{} process(es)",
            process_fail_count,
            result.processes.len()
        );
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_switch_to_main(
    config: &Config,
    config_path: &Option<std::path::PathBuf>,
    json_output: bool,
    no_services: bool,
    no_processes: bool,
    no_verify: bool,
    non_interactive: bool,
    trigger_source: Option<&str>,
    vcs_event: Option<&str>,
) -> Result<()> {
    let main_workspace = config.git.main_workspace.clone();

    if !json_output {
        println!("Switching to main workspace: {}", main_workspace);
    }

    // Delegate to the shared switch command — main is just a special case
    handle_switch_command(
        config,
        &main_workspace,
        config_path,
        false,
        None,
        no_services,
        no_processes,
        no_verify,
        json_output,
        non_interactive,
        trigger_source,
        vcs_event,
        None, // copy_ignored — use config default
        None, // sandboxed — main workspace is never sandboxed
    )
    .await
}

// ── Remove ─────────────────────────────────────────────────────────────────────

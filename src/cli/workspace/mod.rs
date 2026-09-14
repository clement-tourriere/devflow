use anyhow::Result;
use devflow_core::config::Config;
use devflow_core::services;
use devflow_core::vcs;

mod context;
mod exec;
mod interactive;
mod link;
pub(crate) mod list;
mod remove;

pub(crate) use context::{
    ensure_default_workspace_registered, resolve_workspace_context, WorkspaceContextSource,
};
use interactive::handle_interactive_switch;
use link::{handle_link_command, resolve_parent_for_workspace_creation};
use list::handle_workspace_list;
use remove::handle_remove_command;

pub(super) async fn handle_workspace_command(
    cmd: super::Commands,
    config: &mut Config,
    json_output: bool,
    non_interactive: bool,
    database_name: Option<&str>,
    config_path: &Option<std::path::PathBuf>,
) -> Result<()> {
    match cmd {
        super::Commands::List => {
            let _ = database_name;
            handle_workspace_list(config, config_path, json_output).await?;
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
            data_from,
            execute,
            detach,
            open,
            execute_args,
            no_services,
            no_processes,
            no_verify,
            template,
            dry_run,
            copy_ignored,
        } => {
            let mut machine_output = None;
            let mut switch_outcome = None;
            // The workspace actually switched to, for -x/--open targeting.
            let mut switched_workspace: Option<String> = None;
            if dry_run {
                if let Some(ref workspace) = workspace_name {
                    let context = resolve_workspace_context();
                    let default_parent = if create {
                        from.clone().or_else(|| context.context_workspace.clone())
                    } else {
                        None
                    };
                    let workspace_exists = vcs::detect_vcs_provider(".")
                        .ok()
                        .and_then(|repo| repo.workspace_exists(workspace).ok());

                    let project_dir = super::operation_project_dir(config_path);
                    let service_key = devflow_core::state::LocalStateManager::new()?
                        .resolve_workspace_service_key_by_dir(&project_dir, workspace)?;

                    if json_output {
                        let wt_path = super::config::resolve_cd_target(
                            &devflow_core::workspace::worktree::resolve_existing_or_planned_worktree_path(
                                config,
                                &project_dir,
                                workspace,
                            ),
                        )?;
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
                        machine_output = Some(serde_json::json!({
                            "status": "ok",
                            "dry_run": true,
                            "workspace": workspace,
                            "service_key": service_key,
                            "worktree_path": wt_path.display().to_string(),
                            "parent": default_parent,
                            "workspace_exists": workspace_exists,
                            "services_skipped": no_services,
                            "auto_branch_services": auto_providers,
                            "processes_skipped": no_processes,
                            "auto_start_processes": if !no_processes { config.processes.as_ref().map(|p| p.daemons.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() } else { Vec::<String>::new() },
                            "hooks_skipped": no_verify,
                            "execute": execute,
                            "would_fail_without_create": workspace_exists == Some(false) && !create,
                        }));
                    } else {
                        println!("Dry run: would select workspace: {}", workspace);
                        if let Some(ref parent) = default_parent {
                            println!("  Parent workspace: {}", parent);
                        }
                        if workspace_exists == Some(false) && !create {
                            println!(
                                "  Note: workspace does not exist; this would fail (use -c to create it)"
                            );
                        }
                        println!("  Service key: {}", service_key);
                        let wt_path = super::config::resolve_cd_target(
                            &devflow_core::workspace::worktree::resolve_existing_or_planned_worktree_path(
                                config,
                                &project_dir,
                                workspace,
                            ),
                        )?;
                        println!("  Worktree path: {}", wt_path.display());
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
            } else if template
                || workspace_name.as_deref() == Some(config.git.main_workspace.as_str())
            {
                let main_workspace = config.git.main_workspace.clone();
                if !json_output {
                    println!("Switching to main workspace: {}", main_workspace);
                }
                switched_workspace = Some(main_workspace.clone());
                let presentation = handle_switch_command(
                    config,
                    &main_workspace,
                    config_path,
                    false, // create — the default workspace always exists
                    None,  // from
                    None,  // data_from
                    no_services,
                    no_processes,
                    no_verify,
                    json_output,
                    non_interactive,
                    None,
                    None,
                    None, // copy_ignored — use config default
                )
                .await?;
                machine_output = presentation.output;
                switch_outcome = Some(presentation.result);
            } else if let Some(ref workspace) = workspace_name {
                switched_workspace = Some(workspace.clone());
                let presentation = handle_switch_command(
                    config,
                    workspace,
                    config_path,
                    create,
                    from.as_deref(),
                    data_from.as_deref(),
                    no_services,
                    no_processes,
                    no_verify,
                    json_output,
                    non_interactive,
                    None,
                    None,
                    if copy_ignored { Some(true) } else { None },
                )
                .await?;
                machine_output = presentation.output;
                switch_outcome = Some(presentation.result);
            } else if non_interactive || json_output {
                anyhow::bail!(
                    "No workspace specified. Use 'devflow switch <workspace>' in non-interactive or JSON mode."
                );
            } else {
                // The picked (or newly created) workspace is the target for
                // -x/--open below; a cancelled picker must not fall back to
                // executing in the default workspace.
                if let Some(presentation) = handle_interactive_switch(config, config_path).await? {
                    switched_workspace = Some(presentation.result.workspace.clone());
                    machine_output = presentation.output;
                    switch_outcome = Some(presentation.result);
                }
            }

            // Execute command or open interactive session in workspace
            let switch_failed = switch_outcome
                .as_ref()
                .is_some_and(|result| result.failed());
            let setup_incomplete = switch_outcome
                .as_ref()
                .is_some_and(|result| !result.ready());
            if setup_incomplete && !json_output && (open || execute.is_some()) {
                eprintln!("Command skipped: workspace setup needs attention; retry after resolving the reported prerequisites.");
            }
            let execution_output = if !dry_run
                && !setup_incomplete
                && !switch_failed
                && (open || execute.is_some())
                && switched_workspace.is_some()
            {
                let workspace = switched_workspace
                    .as_deref()
                    .unwrap_or(&config.git.main_workspace);
                let cmd = execute.as_deref().unwrap_or("");
                Some(
                    exec::execute_in_workspace(
                        config,
                        config_path,
                        workspace,
                        cmd,
                        &execute_args,
                        detach || open,
                        json_output,
                    )
                    .await?,
                )
            } else {
                None
            };

            if json_output {
                let output = machine_output.ok_or_else(|| {
                    anyhow::anyhow!("switch did not produce a machine-readable result")
                })?;
                let execution_failed = execution_output
                    .as_ref()
                    .is_some_and(|execution| execution.exit_code.is_some_and(|code| code != 0));
                let mut output = compose_switch_output(output, execution_output)?;
                if setup_incomplete && (open || execute.is_some()) {
                    output["execution"] = serde_json::json!({"status":"skipped", "reason":"workspace setup needs attention"});
                }
                println!("{}", serde_json::to_string_pretty(&output)?);
                if switch_failed {
                    anyhow::bail!("Workspace switch completed with orchestration failures");
                }
                if execution_failed {
                    anyhow::bail!("Workspace command completed with a non-zero exit code");
                }
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
        super::Commands::Doctor { strict } => {
            let general_checks = super::config::collect_doctor_pre_checks(config, config_path);
            let general_summary = devflow_core::doctor::DoctorSummary::from_checks(&general_checks);
            let mut all_checks = general_checks.clone();
            let mut failed_checks = general_summary.failed;
            let general_json = serde_json::json!({
                "checks": &general_checks,
                "summary": general_summary,
                "next_steps": devflow_core::doctor::doctor_next_steps(&general_checks),
            });

            if !json_output {
                super::config::run_doctor_pre_checks(config, config_path);
            }

            let has_multiple_services = config.resolve_services().len() > 1;
            if database_name.is_none() && has_multiple_services {
                let services_json = match services::factory::create_all_providers(config).await {
                    Ok(providers) => {
                        let mut map = serde_json::Map::new();
                        for named in providers {
                            let value = match named.provider.doctor().await {
                                Ok(report) => {
                                    failed_checks += super::service::failed_doctor_checks(&report);
                                    all_checks.extend(report.checks.clone());
                                    if !json_output {
                                        super::service::print_doctor_report(
                                            &named.name,
                                            named.provider.provider_name(),
                                            &report,
                                        );
                                    }
                                    super::service::doctor_report_json(&report)
                                }
                                Err(err) => {
                                    failed_checks += 1;
                                    let check = super::service::service_doctor_failure_check(
                                        err.to_string(),
                                    );
                                    all_checks.push(check.clone());
                                    if !json_output {
                                        println!("[{}] Doctor report:", named.name);
                                        println!("  [FAIL] Service doctor: {}", err);
                                        println!();
                                    }
                                    super::service::doctor_checks_json(vec![check])
                                }
                            };
                            map.insert(named.name, value);
                        }
                        serde_json::Value::Object(map)
                    }
                    Err(err) => {
                        failed_checks += 1;
                        all_checks.push(super::service::service_doctor_failure_check(
                            err.to_string(),
                        ));
                        if !json_output {
                            println!("Services:");
                            println!("  [FAIL] Could not initialize providers: {}", err);
                        }
                        serde_json::json!({
                            "error": format!("Failed to create service providers: {}", err),
                        })
                    }
                };

                if json_output {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "healthy": failed_checks == 0,
                            "failed_checks": failed_checks,
                            "strict": strict,
                            "summary": devflow_core::doctor::DoctorSummary::from_checks(&all_checks),
                            "next_steps": devflow_core::doctor::doctor_next_steps(&all_checks),
                            "general": general_json,
                            "services": services_json,
                        }))?
                    );
                } else {
                    let next_steps = devflow_core::doctor::doctor_next_steps(&all_checks);
                    super::service::print_doctor_next_steps(&next_steps);
                }
                if strict && failed_checks > 0 {
                    anyhow::bail!("doctor found {} failing check(s)", failed_checks);
                }
                return Ok(());
            }
            // Service-specific doctor report is optional
            match services::factory::resolve_provider(config, database_name).await {
                Ok(named) => match named.provider.doctor().await {
                    Ok(report) => {
                        failed_checks += super::service::failed_doctor_checks(&report);
                        all_checks.extend(report.checks.clone());
                        if json_output {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&serde_json::json!({
                                    "healthy": failed_checks == 0,
                                    "failed_checks": failed_checks,
                                    "strict": strict,
                                    "summary": devflow_core::doctor::DoctorSummary::from_checks(&all_checks),
                                    "next_steps": devflow_core::doctor::doctor_next_steps(&all_checks),
                                    "general": general_json,
                                    "service": super::service::doctor_report_json(&report),
                                }))?
                            );
                        } else {
                            super::service::print_doctor_report(
                                &named.name,
                                named.provider.provider_name(),
                                &report,
                            );
                        }
                    }
                    Err(err) => {
                        failed_checks += 1;
                        let check = super::service::service_doctor_failure_check(err.to_string());
                        all_checks.push(check.clone());
                        let service_json = super::service::doctor_checks_json(vec![check]);
                        if json_output {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&serde_json::json!({
                                    "healthy": false,
                                    "failed_checks": failed_checks,
                                    "strict": strict,
                                    "summary": devflow_core::doctor::DoctorSummary::from_checks(&all_checks),
                                    "next_steps": devflow_core::doctor::doctor_next_steps(&all_checks),
                                    "general": general_json,
                                    "service": service_json,
                                }))?
                            );
                        } else {
                            println!("Service ({}):", named.provider.provider_name());
                            println!("  [FAIL] Service doctor: {}", err);
                        }
                    }
                },
                Err(_) => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "healthy": failed_checks == 0,
                                "failed_checks": failed_checks,
                                "strict": strict,
                                "summary": devflow_core::doctor::DoctorSummary::from_checks(&all_checks),
                                "next_steps": devflow_core::doctor::doctor_next_steps(&all_checks),
                                "general": general_json,
                                "services": null,
                            }))?
                        );
                    } else {
                        println!("Services:");
                        println!("  [WARN] No service provider available (run 'devflow service add' to configure one)");
                    }
                }
            }
            if !json_output {
                let next_steps = devflow_core::doctor::doctor_next_steps(&all_checks);
                super::service::print_doctor_next_steps(&next_steps);
            }
            if strict && failed_checks > 0 {
                anyhow::bail!("doctor found {} failing check(s)", failed_checks);
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

fn compose_switch_output(
    mut switch_output: serde_json::Value,
    execution: Option<exec::ExecutionOutput>,
) -> Result<serde_json::Value> {
    if let Some(execution) = execution {
        let execution_failed = execution.exit_code.is_some_and(|code| code != 0);
        let object = switch_output.as_object_mut().ok_or_else(|| {
            anyhow::anyhow!("machine-readable switch result must be a JSON object")
        })?;
        object.insert("execution".to_string(), serde_json::to_value(execution)?);
        if execution_failed {
            object.insert("status".to_string(), serde_json::json!("error"));
        }
    }
    Ok(switch_output)
}

#[cfg(test)]
fn machine_switch_failed(output: &serde_json::Value) -> bool {
    output.get("status").and_then(serde_json::Value::as_str) == Some("error")
}

pub(super) struct SwitchPresentation {
    pub result: devflow_core::workspace::SwitchWorkspaceResult,
    pub output: Option<serde_json::Value>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_switch_command(
    config: &Config,
    workspace_name: &str,
    config_path: &Option<std::path::PathBuf>,
    create: bool,
    from: Option<&str>,
    data_from: Option<&str>,
    no_services: bool,
    no_processes: bool,
    no_verify: bool,
    json_output: bool,
    non_interactive: bool,
    trigger_source: Option<&str>,
    vcs_event: Option<&str>,
    copy_ignored_override: Option<bool>,
) -> Result<SwitchPresentation> {
    ensure_default_workspace_registered(config, config_path)?;
    // Resolve parent via CLI-specific interactive prompt (if needed)
    let from_workspace = if create {
        let context = resolve_workspace_context();
        resolve_parent_for_workspace_creation(
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

    let project_dir = super::operation_project_dir(config_path);

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
        data_from_workspace: data_from.map(String::from),
        copy_files: None,
        copy_ignored: copy_ignored_override,
    };

    let result =
        devflow_core::application::switch_workspace(config, &project_dir, workspace_name, &options)
            .await?;

    // ── CLI-specific output ──────────────────────────────────────────
    let shell_integration = super::config::shell_integration_enabled();
    let mut machine_output = None;

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
        println!("Selected workspace: {}", result.workspace);
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
    // Post-create/post-switch hooks run best-effort, so their failures reach
    // callers only through `result.hooks`. The machine-readable status must
    // reflect them: the TUI and GUI already treat hook failures as blocking,
    // and CI/agents drive this JSON — reporting "ok" would let them proceed
    // on a broken workspace.
    let hook_fail_count: usize = result.hooks.iter().map(|r| r.failed).sum();

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
                    "skipped_reasons": r.skipped_reasons,
                    "background": r.background,
                        "background_records": r.background_records,
                        "required_failed": r.required_failed,
                        "required_skipped": r.required_skipped,
                    "errors": r.errors,
                })
            })
            .collect();
        let summary = serde_json::json!({
            "operation": result.operation,
            "status": if fail_count == 0 && process_fail_count == 0 && hook_fail_count == 0 { "ok" } else { "error" },
            "workspace": result.workspace,
            "service_key": result.service_key,
            "parent": result.parent,
            "worktree_path": result.worktree.as_ref().map(|w| w.path.display().to_string()),
            "agent_workdir": result.worktree.as_ref().map(|w| w.path.display().to_string()).unwrap_or_else(|| project_dir.display().to_string()),
            "cd_hint": result.worktree.as_ref().map(|w| format!("cd {}", w.path.display())).unwrap_or_else(|| format!("cd {}", project_dir.display())),
            "worktree_created": result.worktree.as_ref().map(|w| w.created).unwrap_or(false),
            "services_switched": success_count,
            "services_failed": fail_count,
            "services_skipped": no_services,
            "service_results": service_results,
            "processes_started": process_success_count,
            "processes_failed": process_fail_count,
            "processes_skipped": no_processes || result.operation.as_ref().is_some_and(|o| o.status != "ready") && result.processes.is_empty(),
            "process_results": process_results,
            "hooks_failed": hook_fail_count,
            "hook_results": hook_results,
        });
        machine_output = Some(summary);
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
        println!("Selected workspace/worktree: {}", result.workspace);
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

    if process_fail_count > 0 && !json_output {
        anyhow::bail!(
            "Failed to start {}/{} process(es)",
            process_fail_count,
            result.processes.len()
        );
    }

    Ok(SwitchPresentation {
        result,
        output: machine_output,
    })
}

#[cfg(test)]
mod tests {
    use super::{compose_switch_output, exec::ExecutionOutput, machine_switch_failed};

    #[test]
    fn switch_and_execution_are_composed_into_one_document() {
        let switch = serde_json::json!({
            "workspace": "feature/auth",
            "worktree_path": "/tmp/project.feature_auth",
            "services_switched": 1,
        });
        let execution = ExecutionOutput {
            execution_id: "fixture".into(),
            workspace: "feature/auth".into(),
            service_key: "feature_auth-abc123".into(),
            command: "cargo test".into(),
            session: None,
            worktree: "/tmp/project.feature_auth".into(),
            detached: false,
            exit_code: Some(0),
            stdout: Some("tests passed\n".into()),
            stderr: None,
        };

        let output = compose_switch_output(switch, Some(execution)).unwrap();
        assert_eq!(output["workspace"], "feature/auth");
        assert_eq!(output["services_switched"], 1);
        assert_eq!(output["execution"]["workspace"], "feature/auth");
        assert_eq!(output["execution"]["exit_code"], 0);
        assert_eq!(output["execution"]["stdout"], "tests passed\n");

        let rendered = serde_json::to_string_pretty(&output).unwrap();
        let documents = serde_json::Deserializer::from_str(&rendered)
            .into_iter::<serde_json::Value>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(documents.len(), 1);
    }

    #[test]
    fn switch_without_execution_keeps_the_existing_shape() {
        let switch = serde_json::json!({"workspace": "main", "worktree_created": false});
        assert_eq!(compose_switch_output(switch.clone(), None).unwrap(), switch);
    }

    #[test]
    fn failed_execution_keeps_one_document_and_sets_error_status() {
        let switch = serde_json::json!({
            "status": "ok",
            "workspace": "feature/auth",
        });
        let execution = ExecutionOutput {
            execution_id: "fixture".into(),
            workspace: "feature/auth".into(),
            service_key: "feature_auth-abc123".into(),
            command: "exit 7".into(),
            session: None,
            worktree: "/tmp/project.feature_auth".into(),
            detached: false,
            exit_code: Some(7),
            stdout: None,
            stderr: Some("failed\n".into()),
        };

        let output = compose_switch_output(switch, Some(execution)).unwrap();
        assert!(machine_switch_failed(&output));
        assert_eq!(output["execution"]["exit_code"], 7);
        assert_eq!(output["execution"]["stderr"], "failed\n");
    }
}

// ── Remove ─────────────────────────────────────────────────────────────────────

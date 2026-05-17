use std::path::PathBuf;

use anyhow::Result;
use devflow_core::agent::{self, AgentGuideTarget};
use devflow_core::config::Config;
use devflow_core::state::LocalStateManager;
use devflow_core::vcs;

pub(super) async fn handle_agent_command(
    action: super::AgentCommands,
    config: &Config,
    json_output: bool,
    _non_interactive: bool,
    config_path: &Option<PathBuf>,
) -> Result<()> {
    match action {
        super::AgentCommands::Status => {
            let state_manager = LocalStateManager::new()?;
            if config_path.is_some() {
                let project_dir = crate::cli::operation_project_dir(config_path);
                let workspaces = state_manager.get_workspaces_by_dir(&project_dir);

                let project_dir_string = project_dir.display().to_string();
                let executed_count = workspaces
                    .iter()
                    .filter(|b| b.executed_command.is_some())
                    .count();
                let agent_workspace_count = workspaces
                    .iter()
                    .filter(|b| b.name.starts_with("agent/"))
                    .count();

                if json_output {
                    let items: Vec<serde_json::Value> = workspaces
                        .iter()
                        .map(|b| {
                            let agent_workdir = b
                                .worktree_path
                                .clone()
                                .unwrap_or_else(|| project_dir_string.clone());
                            serde_json::json!({
                                "workspace": b.name,
                                "is_agent_workspace": b.name.starts_with("agent/"),
                                "created_at": b.created_at.to_rfc3339(),
                                "worktree_path": b.worktree_path,
                                "agent_workdir": agent_workdir,
                                "executed_command": b.executed_command,
                                "execution_status": b.execution_status,
                                "executed_at": b.executed_at.map(|t| t.to_rfc3339()),
                            })
                        })
                        .collect();
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "project_dir": project_dir_string,
                            "total_workspaces": workspaces.len(),
                            "agent_workspaces": agent_workspace_count,
                            "executed_workspaces": executed_count,
                            "workspaces": items,
                        }))?
                    );
                } else {
                    println!(
                        "Registered workspaces: {} ({} agent, {} with executed commands)",
                        workspaces.len(),
                        agent_workspace_count,
                        executed_count
                    );
                    let executed: Vec<_> = workspaces
                        .iter()
                        .filter(|b| b.executed_command.is_some())
                        .collect();
                    if executed.is_empty() {
                        println!("No workspaces with executed commands.");
                    } else {
                        println!("Workspaces with commands:");
                        for b in executed {
                            let cmd = b.executed_command.as_deref().unwrap_or("unknown");
                            let status = b.execution_status.as_deref().unwrap_or("unknown");
                            println!(
                                "  {} ({}, {}) — created {}",
                                b.name,
                                cmd,
                                status,
                                b.created_at.format("%Y-%m-%d %H:%M")
                            );
                        }
                    }
                }
            } else if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "project_dir": null,
                        "total_workspaces": 0,
                        "agent_workspaces": 0,
                        "executed_workspaces": 0,
                        "workspaces": [],
                        "warning": "No project configuration found",
                    }))?
                );
            } else {
                println!("No project configuration found.");
            }
            Ok(())
        }

        super::AgentCommands::Context { format, workspace } => {
            let workspace_name = if let Some(b) = workspace {
                b
            } else {
                match vcs::detect_vcs_provider(".") {
                    Ok(vcs_repo) => vcs_repo
                        .current_workspace()
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| "unknown".to_string()),
                    Err(_) => "unknown".to_string(),
                }
            };

            let fmt = if json_output { "json" } else { format.as_str() };
            let project_dir = super::operation_project_dir(config_path);
            let output = devflow_core::agent::generate_agent_context(
                config,
                &project_dir,
                &workspace_name,
                fmt,
            )
            .await?;
            println!("{}", output);
            Ok(())
        }

        super::AgentCommands::Skill { target } => {
            let project_dir = super::operation_project_dir(config_path);
            let targets = parse_targets(Some(target.as_str()))?;
            let written = agent::install_agent_guides(config, &project_dir, &targets)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "written": written,
                        "targets": targets.iter().map(|t| t.label()).collect::<Vec<_>>(),
                    }))?
                );
            } else {
                for path in &written {
                    println!("Wrote: {}", path);
                }
                println!("Generated guide files only; devflow no longer installs bundled skills.");
            }
            Ok(())
        }

        super::AgentCommands::Docs { force } => {
            let project_dir = super::operation_project_dir(config_path);
            let path = project_dir.join("AGENTS.md");
            if path.exists() && !force {
                anyhow::bail!(
                    "AGENTS.md already exists. Re-run with `devflow agent docs --force` to replace it."
                );
            }
            let written = agent::install_agents_md(config, &project_dir)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "written": written.display().to_string(),
                    }))?
                );
            } else {
                println!("Wrote: {}", written.display());
            }
            Ok(())
        }
    }
}

fn parse_targets(target: Option<&str>) -> Result<Vec<AgentGuideTarget>> {
    match target {
        None => Ok(agent::default_agent_guide_targets()),
        Some(value) if value.eq_ignore_ascii_case("all") => {
            Ok(agent::default_agent_guide_targets())
        }
        Some(value) => Ok(vec![AgentGuideTarget::parse(value)?]),
    }
}

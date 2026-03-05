use std::path::PathBuf;

use anyhow::Result;
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
            if let Some(ref path) = config_path {
                let workspaces = state_manager.get_workspaces(path);

                let executed: Vec<_> = workspaces
                    .iter()
                    .filter(|b| b.executed_command.is_some())
                    .collect();

                if json_output {
                    let items: Vec<serde_json::Value> = executed
                        .iter()
                        .map(|b| {
                            serde_json::json!({
                                "workspace": b.name,
                                "created_at": b.created_at.to_rfc3339(),
                                "worktree_path": b.worktree_path,
                                "executed_command": b.executed_command,
                                "execution_status": b.execution_status,
                                "sandboxed": b.sandboxed,
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&items)?);
                } else if executed.is_empty() {
                    println!("No workspaces with executed commands.");
                } else {
                    println!("Workspaces with commands:");
                    for b in executed {
                        let cmd = b.executed_command.as_deref().unwrap_or("unknown");
                        let status = b.execution_status.as_deref().unwrap_or("unknown");
                        let sandbox_label = if b.sandboxed { " [sandboxed]" } else { "" };
                        println!(
                            "  {} ({}, {}{}) — created {}",
                            b.name,
                            cmd,
                            status,
                            sandbox_label,
                            b.created_at.format("%Y-%m-%d %H:%M")
                        );
                    }
                }
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
            let project_dir = config_path
                .as_ref()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("."));
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

        super::AgentCommands::Skill => {
            let project_dir = std::env::current_dir()?;
            let written = devflow_core::agent::install_agent_skills(config, &project_dir)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({"paths": written}))?
                );
            } else {
                for path in &written {
                    println!("Installed: {}", path);
                }
            }
            Ok(())
        }
    }
}

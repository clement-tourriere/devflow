use std::path::PathBuf;

use anyhow::{Context, Result};
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
        super::AgentCommands::Start {
            workspace,
            command,
            prompt,
            dry_run,
        } => {
            let agent_config = config.agent.as_ref();
            let prefix = agent_config
                .map(|a| a.workspace_prefix.as_str())
                .unwrap_or("agent/");

            let workspace_name = if workspace.starts_with(prefix) {
                workspace.clone()
            } else {
                format!("{}{}", prefix, workspace)
            };

            let agent_cmd = command
                .or_else(|| agent_config.and_then(|a| a.command.clone()))
                .or_else(|| std::env::var("DEVFLOW_AGENT_COMMAND").ok())
                .unwrap_or_else(|| "claude".to_string());

            let prompt_str = prompt.join(" ");

            if dry_run {
                if json_output {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "workspace": workspace_name,
                            "agent_command": agent_cmd,
                            "prompt": prompt_str,
                        }))?
                    );
                } else {
                    println!("Would create workspace: {}", workspace_name);
                    println!("Would launch agent:  {}", agent_cmd);
                    if !prompt_str.is_empty() {
                        println!("With prompt:         {}", prompt_str);
                    }
                }
                return Ok(());
            }

            // 1. Create the isolated workspace + worktree via the switch handler
            if !json_output {
                println!("Creating isolated workspace: {}", workspace_name);
            }
            super::workspace::handle_switch_command(
                config,
                &workspace_name,
                config_path,
                true,  // create
                None,  // from (defaults to current)
                false, // no_services
                false, // no_verify (let hooks run; --non-interactive handles approval)
                json_output,
                true, // non_interactive
                None,
                None,
                None, // copy_ignored — use config default
            )
            .await?;

            // 2. Record agent metadata in state
            if let Some(ref path) = config_path {
                if let Ok(mut state) = LocalStateManager::new() {
                    let normalized = config.get_normalized_workspace_name(&workspace_name);
                    if let Some(mut branch_state) = state.get_workspace(path, &normalized) {
                        branch_state.agent_tool = Some(agent_cmd.clone());
                        branch_state.agent_status = Some("running".to_string());
                        branch_state.agent_started_at = Some(chrono::Utc::now());
                        if let Err(e) = state.register_workspace(path, branch_state) {
                            log::warn!("Failed to record agent state: {}", e);
                        }
                    }
                }
            }

            // 3. Resolve the worktree path for the agent to work in
            let work_dir = vcs::detect_vcs_provider(".")
                .ok()
                .and_then(|repo| repo.worktree_path(&workspace_name).ok().flatten())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

            // 4. Build the launch command with proper shell escaping
            let escaped_prompt = prompt_str.replace('\'', "'\\''");
            let launch_cmd = if prompt_str.is_empty() {
                agent_cmd.clone()
            } else {
                match agent_cmd.as_str() {
                    "claude" => {
                        format!("claude --dangerously-skip-permissions '{}'", escaped_prompt)
                    }
                    "codex" => format!("codex '{}'", escaped_prompt),
                    _ => format!("{} '{}'", agent_cmd, escaped_prompt),
                }
            };

            // 5. Launch in tmux if available, otherwise direct
            let has_tmux = which::which("tmux").is_ok();

            if has_tmux {
                let session_name = workspace_name.replace('/', "-");
                if !json_output {
                    println!("Launching agent in tmux session: {}", session_name);
                }
                let tmux_status = std::process::Command::new("tmux")
                    .args([
                        "new-session",
                        "-d",
                        "-s",
                        &session_name,
                        "-c",
                        &work_dir.display().to_string(),
                        "sh",
                        "-c",
                        &launch_cmd,
                    ])
                    .status()
                    .context("Failed to launch tmux session")?;
                if !tmux_status.success() {
                    anyhow::bail!(
                        "tmux exited with code {}. Is session '{}' already running? Check: tmux ls",
                        tmux_status.code().unwrap_or(-1),
                        session_name
                    );
                }
                if json_output {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "workspace": workspace_name,
                            "agent_command": agent_cmd,
                            "tmux_session": session_name,
                            "worktree": work_dir.display().to_string(),
                        }))?
                    );
                } else {
                    println!(
                        "Agent running in tmux session '{}'. Attach with: tmux attach -t {}",
                        session_name, session_name
                    );
                }
            } else {
                if !json_output {
                    println!("Launching agent in: {}", work_dir.display());
                }
                let agent_status = std::process::Command::new("sh")
                    .args(["-c", &launch_cmd])
                    .current_dir(&work_dir)
                    .status()
                    .context("Failed to launch agent")?;
                if json_output {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "workspace": workspace_name,
                            "agent_command": agent_cmd,
                            "exit_code": agent_status.code(),
                            "worktree": work_dir.display().to_string(),
                        }))?
                    );
                }
            }

            Ok(())
        }

        super::AgentCommands::Status => {
            let state_manager = LocalStateManager::new()?;
            if let Some(ref path) = config_path {
                let workspaces = state_manager.get_workspaces(path);
                let agent_prefix = config
                    .agent
                    .as_ref()
                    .map(|a| a.workspace_prefix.as_str())
                    .unwrap_or("agent/");

                let agent_branches: Vec<_> = workspaces
                    .iter()
                    .filter(|b| b.name.starts_with(agent_prefix))
                    .collect();

                if json_output {
                    let items: Vec<serde_json::Value> = agent_branches
                        .iter()
                        .map(|b| {
                            serde_json::json!({
                                "workspace": b.name,
                                "created_at": b.created_at.to_rfc3339(),
                                "worktree_path": b.worktree_path,
                                "agent_tool": b.agent_tool,
                                "agent_status": b.agent_status,
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&items)?);
                } else if agent_branches.is_empty() {
                    println!("No active agent workspaces.");
                } else {
                    println!("Agent Branches:");
                    for b in agent_branches {
                        let tool = b.agent_tool.as_deref().unwrap_or("unknown");
                        let status = b.agent_status.as_deref().unwrap_or("unknown");
                        println!(
                            "  {} ({}, {}) — created {}",
                            b.name,
                            tool,
                            status,
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

use std::path::PathBuf;

use anyhow::{Context, Result};
use devflow_core::config::Config;
use devflow_core::state::LocalStateManager;
use devflow_core::vcs;

/// Execute a command inside a workspace's worktree, optionally detached via a
/// terminal multiplexer (tmux/zellij), and record execution state in the local store.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_in_workspace(
    config: &Config,
    config_path: &Option<PathBuf>,
    workspace_name: &str,
    cmd: &str,
    execute_args: &[String],
    detach: bool,
    json_output: bool,
) -> Result<()> {
    // Build full command from -x value + trailing args
    let full_cmd = if execute_args.is_empty() {
        cmd.to_string()
    } else {
        format!("{} {}", cmd, execute_args.join(" "))
    };

    // Resolve worktree path
    let work_dir = vcs::detect_vcs_provider(".")
        .ok()
        .and_then(|repo| repo.worktree_path(workspace_name).ok().flatten())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    // Record execution state
    let normalized = config.get_normalized_workspace_name(workspace_name);
    if let Some(ref path) = config_path {
        if let Ok(mut state) = LocalStateManager::new() {
            if let Some(mut ws) = state.get_workspace(path, &normalized) {
                ws.executed_command = Some(full_cmd.clone());
                ws.execution_status = Some(if detach { "detached" } else { "running" }.to_string());
                ws.executed_at = Some(chrono::Utc::now());
                if let Err(e) = state.register_workspace(path, ws) {
                    log::warn!("Failed to record execution state: {}", e);
                }
            }
        }
    }

    if detach {
        // Detached/interactive execution via configured multiplexer
        let is_interactive = full_cmd.is_empty();

        let template = config
            .execute
            .as_ref()
            .and_then(|e| e.detach_command.clone())
            .or_else(|| {
                // Respect configured multiplexer preference, then auto-detect
                let preferred = config
                    .execute
                    .as_ref()
                    .and_then(|e| e.multiplexer.as_deref());

                match preferred {
                    Some("zellij") if which::which("zellij").is_ok() => {
                        Some("zellij --session {session} --cwd {dir} {cmd}".to_string())
                    }
                    Some("tmux") if which::which("tmux").is_ok() => {
                        Some("tmux new-session -d -s {session} -c {dir} {cmd}".to_string())
                    }
                    Some(name) => {
                        log::warn!(
                            "Configured multiplexer '{}' not found, falling back to auto-detection",
                            name
                        );
                        None
                    }
                    None => None,
                }
                .or_else(|| {
                    if which::which("tmux").is_ok() {
                        Some("tmux new-session -d -s {session} -c {dir} {cmd}".to_string())
                    } else if which::which("zellij").is_ok() {
                        Some("zellij --session {session} --cwd {dir} {cmd}".to_string())
                    } else {
                        None
                    }
                })
            });

        let Some(template) = template else {
            anyhow::bail!(
                "No multiplexer available for --detach/--open. Install tmux or zellij, or configure execute.detach_command in .devflow.yml"
            );
        };

        let session = normalized.replace('/', "-");

        // Build the {cmd} replacement
        let cmd_replacement = if is_interactive {
            String::new()
        } else if template.contains("sh -c") {
            // Custom template already includes sh -c — pass raw command
            let escaped = full_cmd.replace('\'', "'\\''");
            format!("'{}'", escaped)
        } else {
            let escaped = full_cmd.replace('\'', "'\\''");
            format!("sh -c '{}'", escaped)
        };

        let expanded = template
            .replace("{session}", &session)
            .replace("{dir}", &work_dir.display().to_string())
            .replace("{cmd}", &cmd_replacement);
        // Trim trailing whitespace from empty {cmd} expansion
        let expanded = expanded.trim_end().to_string();

        if !json_output {
            if is_interactive {
                println!("Opening session: {}", expanded);
            } else {
                println!("Detaching: {}", expanded);
            }
        }

        let status = tokio::process::Command::new("sh")
            .args(["-c", &expanded])
            .status()
            .await
            .context("Failed to launch multiplexer session")?;

        if !status.success() {
            anyhow::bail!(
                "Multiplexer command failed with exit code: {}",
                status.code().unwrap_or(-1)
            );
        }

        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "workspace": normalized,
                    "command": full_cmd,
                    "session": session,
                    "worktree": work_dir.display().to_string(),
                    "detached": true,
                }))?
            );
        }
    } else {
        // Foreground execution
        if json_output {
            eprintln!("Running: {}", full_cmd);
        } else {
            println!("Running: {}", full_cmd);
        }

        let status = tokio::process::Command::new("sh")
            .args(["-c", &full_cmd])
            .current_dir(&work_dir)
            .status()
            .await
            .context("Failed to execute command")?;

        // Update state on completion
        let execution_status = if status.success() { "done" } else { "failed" };
        if let Some(ref path) = config_path {
            if let Ok(mut state_mgr) = LocalStateManager::new() {
                if let Some(mut ws) = state_mgr.get_workspace(path, &normalized) {
                    ws.execution_status = Some(execution_status.to_string());
                    if let Err(e) = state_mgr.register_workspace(path, ws) {
                        log::warn!("Failed to update execution state: {}", e);
                    }
                }
            }
        }

        if !status.success() {
            anyhow::bail!(
                "Command failed with exit code: {}",
                status.code().unwrap_or(-1)
            );
        }

        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "workspace": normalized,
                    "command": full_cmd,
                    "exit_code": status.code(),
                    "worktree": work_dir.display().to_string(),
                    "detached": false,
                }))?
            );
        }
    }

    Ok(())
}

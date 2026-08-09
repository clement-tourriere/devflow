use std::path::PathBuf;

use anyhow::{Context, Result};
use devflow_core::config::Config;
use devflow_core::state::LocalStateManager;
use devflow_core::vcs;
use serde::Serialize;

/// Machine-readable result of running a command or opening a detached session.
///
/// This is returned to the switch dispatcher instead of being printed here so
/// `devflow --json switch ... -x/--open` can emit one composed JSON document.
#[derive(Debug, Clone, Serialize)]
pub(super) struct ExecutionOutput {
    pub workspace: String,
    pub service_key: String,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub worktree: String,
    pub detached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
}

fn captured_text(bytes: &[u8]) -> Option<String> {
    (!bytes.is_empty()).then(|| String::from_utf8_lossy(bytes).into_owned())
}

/// Execute a command inside a workspace's worktree, optionally detached via a
/// terminal multiplexer (tmux/zellij), and record execution state in the local store.
pub(super) async fn execute_in_workspace(
    config: &Config,
    config_path: &Option<PathBuf>,
    workspace_name: &str,
    cmd: &str,
    execute_args: &[String],
    detach: bool,
    json_output: bool,
) -> Result<ExecutionOutput> {
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

    // Record execution state. Derive the project dir the same way as every
    // sibling command so the service key resolves against the same project
    // identity (cwd first, then the config file's directory).
    let state_project_dir = crate::cli::operation_project_dir(config_path);
    let service_key = LocalStateManager::new()?
        .resolve_workspace_service_key_by_dir(&state_project_dir, workspace_name)?;
    if config_path.is_some() {
        if let Ok(mut state) = LocalStateManager::new() {
            if let Some(mut ws) = state.get_workspace_by_dir(&state_project_dir, workspace_name) {
                ws.executed_command = Some(full_cmd.clone());
                ws.execution_status = Some(if detach { "detached" } else { "running" }.to_string());
                ws.executed_at = Some(chrono::Utc::now());
                if let Err(e) = state.register_workspace_by_dir(&state_project_dir, ws) {
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

        let session = service_key.replace('/', "-");

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

        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", &expanded])
            .env("DEVFLOW_WORKSPACE", workspace_name)
            .env("DEVFLOW_WORKSPACE_KEY", &service_key)
            .env("DEVFLOW_BRANCH", workspace_name);
        let (status, stdout, stderr) = if json_output {
            let output = command
                .output()
                .await
                .context("Failed to launch multiplexer session")?;
            let stderr = captured_text(&output.stderr);
            if let Some(ref diagnostics) = stderr {
                eprint!("{diagnostics}");
            }
            (output.status, captured_text(&output.stdout), stderr)
        } else {
            (
                command
                    .status()
                    .await
                    .context("Failed to launch multiplexer session")?,
                None,
                None,
            )
        };

        if !status.success() && !json_output {
            anyhow::bail!(
                "Multiplexer command failed with exit code: {}",
                status.code().unwrap_or(-1)
            );
        }

        Ok(ExecutionOutput {
            workspace: workspace_name.to_string(),
            service_key,
            command: full_cmd,
            session: Some(session),
            worktree: work_dir.display().to_string(),
            detached: true,
            // In --json mode a failed multiplexer launch is reported through
            // the composed document (mirroring the foreground path) instead
            // of bailing before any JSON reaches stdout — attach-style
            // multiplexers (zellij) legitimately fail without a tty there.
            exit_code: if status.success() {
                None
            } else {
                Some(status.code().unwrap_or(-1))
            },
            stdout,
            stderr,
        })
    } else {
        // Foreground execution
        if json_output {
            eprintln!("Running: {}", full_cmd);
        } else {
            println!("Running: {}", full_cmd);
        }

        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", &full_cmd])
            .current_dir(&work_dir)
            .env("DEVFLOW_WORKSPACE", workspace_name)
            .env("DEVFLOW_WORKSPACE_KEY", &service_key)
            .env("DEVFLOW_BRANCH", workspace_name);
        let (status, stdout, stderr) = if json_output {
            let output = command
                .output()
                .await
                .context("Failed to execute command")?;
            let stderr = captured_text(&output.stderr);
            if let Some(ref diagnostics) = stderr {
                eprint!("{diagnostics}");
            }
            (output.status, captured_text(&output.stdout), stderr)
        } else {
            (
                command
                    .status()
                    .await
                    .context("Failed to execute command")?,
                None,
                None,
            )
        };

        // Update state on completion
        let execution_status = if status.success() { "done" } else { "failed" };
        if config_path.is_some() {
            let project_dir = crate::cli::operation_project_dir(config_path);
            if let Ok(mut state_mgr) = LocalStateManager::new() {
                if let Some(mut ws) = state_mgr.get_workspace_by_dir(&project_dir, workspace_name) {
                    ws.execution_status = Some(execution_status.to_string());
                    if let Err(e) = state_mgr.register_workspace_by_dir(&project_dir, ws) {
                        log::warn!("Failed to update execution state: {}", e);
                    }
                }
            }
        }

        if !status.success() && !json_output {
            anyhow::bail!(
                "Command failed with exit code: {}",
                status.code().unwrap_or(-1)
            );
        }

        // Foreground execution always has a terminal outcome. Unix signals do
        // not expose a numeric exit code, so use -1 rather than `None` (which
        // is reserved for detached sessions that are still running).
        let exit_code = Some(
            status
                .code()
                .unwrap_or(if status.success() { 0 } else { -1 }),
        );

        Ok(ExecutionOutput {
            workspace: workspace_name.to_string(),
            service_key,
            command: full_cmd,
            session: None,
            worktree: work_dir.display().to_string(),
            detached: false,
            exit_code,
            stdout,
            stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::ExecutionOutput;

    #[test]
    fn execution_output_preserves_the_previous_conditional_fields() {
        let foreground = serde_json::to_value(ExecutionOutput {
            workspace: "feature/auth".into(),
            service_key: "feature_auth-abc123".into(),
            command: "cargo test".into(),
            session: None,
            worktree: "/tmp/project.feature_auth-abc123".into(),
            detached: false,
            exit_code: Some(0),
            stdout: Some("ok\n".into()),
            stderr: None,
        })
        .unwrap();
        assert_eq!(foreground["workspace"], "feature/auth");
        assert_eq!(foreground["exit_code"], 0);
        assert_eq!(foreground["stdout"], "ok\n");
        assert!(foreground.get("session").is_none());
        assert!(foreground.get("stderr").is_none());

        let detached = serde_json::to_value(ExecutionOutput {
            workspace: "feature/auth".into(),
            service_key: "feature_auth-abc123".into(),
            command: "codex".into(),
            session: Some("feature_auth-abc123".into()),
            worktree: "/tmp/project.feature_auth-abc123".into(),
            detached: true,
            exit_code: None,
            stdout: None,
            stderr: None,
        })
        .unwrap();
        assert_eq!(detached["session"], "feature_auth-abc123");
        assert!(detached.get("exit_code").is_none());
    }
}

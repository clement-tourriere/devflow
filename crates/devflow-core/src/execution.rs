//! Typed command execution shared by CLI and desktop callers.
use serde::Deserialize;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandSpec {
    Program { program: String, args: Vec<String> },
    Shell { script: String, args: Vec<String> },
}

impl CommandSpec {
    /// Old -x command strings remain supported; trailing arguments are always
    /// passed as data. New callers should construct Program or Shell explicitly.
    pub fn legacy(command: &str, args: &[String]) -> Self {
        if args.is_empty() && command.chars().any(char::is_whitespace) {
            Self::Shell {
                script: command.into(),
                args: vec![],
            }
        } else {
            Self::Program {
                program: command.into(),
                args: args.to_vec(),
            }
        }
    }

    pub fn command(&self) -> tokio::process::Command {
        match self {
            Self::Program { program, args } => {
                let mut command = tokio::process::Command::new(program);
                command.args(args);
                command
            }
            Self::Shell { script, args } => {
                let mut command = tokio::process::Command::new("sh");
                command.args(["-c", script, "devflow-shell"]).args(args);
                command
            }
        }
    }

    pub fn shell_command(&self) -> String {
        match self {
            Self::Program { program, args } if program.is_empty() && args.is_empty() => {
                String::new()
            }
            Self::Program { program, args } => std::iter::once(program)
                .chain(args)
                .map(|s| shell_quote(s))
                .collect::<Vec<_>>()
                .join(" "),
            Self::Shell { script, args } => std::iter::once("sh".to_string())
                .chain(["-c".into(), script.clone(), "devflow-shell".into()])
                .chain(args.iter().cloned())
                .map(|s| shell_quote(&s))
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

use crate::config::Config;
use crate::state::LocalStateManager;
use crate::vcs;
use anyhow::{Context, Result};
use serde::Serialize;

/// Machine-readable result of running a command or opening a detached session.
///
/// This is returned to the switch dispatcher instead of being printed here so
/// `devflow --json switch ... -x/--open` can emit one composed JSON document.
#[derive(Debug, Clone, Serialize)]
pub struct ExecutionOutput {
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
pub async fn execute_in_workspace(
    config: &Config,
    project_dir: &std::path::Path,
    workspace_name: &str,
    spec: &CommandSpec,
    detach: bool,
    json_output: bool,
) -> Result<ExecutionOutput> {
    let full_cmd = spec.shell_command();
    let repo = vcs::detect_vcs_provider(project_dir)?;
    let work_dir = repo
        .worktree_path(workspace_name)?
        .or_else(|| {
            (workspace_name == config.git.main_workspace)
                .then(|| vcs::resolve_project_root(project_dir))
        })
        .context("workspace has no materialized worktree")?;
    anyhow::ensure!(
        work_dir.is_dir(),
        "workspace directory is missing: {}",
        work_dir.display()
    );
    let pending = crate::workspace::operation::OperationJournal::pending(project_dir)?;
    anyhow::ensure!(
        !pending.iter().any(|op| op.workspace == workspace_name),
        "workspace setup needs attention; resume its pending operation before executing a command"
    );

    // Record execution state. Derive the project dir the same way as every
    // sibling command so the service key resolves against the same project
    // identity (cwd first, then the config file's directory).
    let state_project_dir = project_dir.to_path_buf();
    let service_key = LocalStateManager::new()?
        .resolve_workspace_service_key_by_dir(&state_project_dir, workspace_name)?;
    {
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

        let project_key =
            crate::services::ownership::identity_digest(&[&config.resource_project_identity()]);
        let session = format!("devflow-{}-{}", &project_key[..12], service_key);

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
            .replace("{dir}", &shell_quote(&work_dir.display().to_string()))
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

        let mut command = spec.command();
        command
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
        {
            let project_dir = project_dir.to_path_buf();
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

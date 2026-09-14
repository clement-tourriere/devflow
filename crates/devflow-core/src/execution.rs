//! Typed command execution shared by CLI and desktop callers.
pub mod records;
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
    pub execution_id: String,
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

    let service_key = LocalStateManager::new()?
        .resolve_workspace_service_key_by_dir(project_dir, workspace_name)?;
    let mut execution = records::Guard::new(project_dir, workspace_name, &work_dir, spec)?;

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
        let session = format!(
            "devflow-{}-{}-{}",
            &project_key[..12],
            service_key,
            &execution.record.id[..8]
        );
        execution.record.session = Some(session.clone());
        if is_interactive {
            execution.record.command = CommandSpec::Program {
                program: std::env::var("SHELL").unwrap_or_else(|_| "sh".into()),
                args: vec![],
            };
        }
        execution.save()?;
        anyhow::ensure!(template.contains("{cmd}"), "execute.detach_command must include {{cmd}} so the session can report its execution lifecycle");
        let runner = CommandSpec::Program {
            program: std::env::current_exe()?.display().to_string(),
            args: vec!["execution-run".into(), execution.path.display().to_string()],
        }
        .shell_command();

        let cmd_replacement = if template.contains("sh -c") {
            shell_quote(&runner)
        } else {
            format!("sh -c {}", shell_quote(&runner))
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

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let handoff = loop {
            let current = execution.read()?;
            if !matches!(current.state, records::ExecutionState::Starting) {
                execution.transfer();
                break Some(current);
            }
            if !status.success() || tokio::time::Instant::now() >= deadline {
                break execution.cancel_start(status.code().filter(|c| *c != 0).unwrap_or(-1))?;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        };
        let detached = handoff.is_some();
        let exit_code = handoff
            .as_ref()
            .and_then(|record| record.exit_code)
            .or_else(|| (!detached).then_some(status.code().filter(|c| *c != 0).unwrap_or(-1)));
        let stderr = if status.success() && !detached {
            Some("Detached launcher exited without starting the execution runner; check execute.detach_command".into())
        } else {
            stderr
        };
        if !detached && !json_output {
            anyhow::bail!(
                "{}",
                stderr
                    .as_deref()
                    .unwrap_or("Failed to launch detached execution")
            );
        }

        Ok(ExecutionOutput {
            workspace: workspace_name.to_string(),
            execution_id: execution.record.id.clone(),
            service_key,
            command: full_cmd,
            session: Some(session),
            worktree: work_dir.display().to_string(),
            detached,
            exit_code,
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
        let (status, stdout, stderr) =
            run_recorded(&mut command, &mut execution, json_output).await?;

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
            execution_id: execution.record.id.clone(),
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

async fn run_recorded(
    command: &mut tokio::process::Command,
    execution: &mut records::Guard,
    capture: bool,
) -> Result<(std::process::ExitStatus, Option<String>, Option<String>)> {
    execution.claim()?;
    command.kill_on_drop(true);
    if capture {
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            execution.finish(-1)?;
            return Err(error).context("Failed to execute command");
        }
    };
    execution.record.child = child
        .id()
        .and_then(crate::process_identity::ProcessIdentity::read);
    execution.save()?;
    let result = if capture {
        let output = child.wait_with_output().await?;
        let stderr = captured_text(&output.stderr);
        if let Some(text) = &stderr {
            eprint!("{text}");
        }
        (output.status, captured_text(&output.stdout), stderr)
    } else {
        (child.wait().await?, None, None)
    };
    execution.finish(result.0.code().unwrap_or(-1))?;
    Ok(result)
}

/// Entry point used inside a detached terminal session. The record owns its
/// exact argument vector and workdir; there is no internal command wrapper.
pub async fn run_saved(path: &std::path::Path) -> Result<i32> {
    let mut execution = records::Guard::open(path)?;
    anyhow::ensure!(
        matches!(execution.record.state, records::ExecutionState::Starting),
        "Execution has already been claimed or completed"
    );
    let mut command = execution.record.command.command();
    command
        .current_dir(&execution.record.workdir)
        .env("DEVFLOW_WORKSPACE", &execution.record.workspace);
    let key = LocalStateManager::new()?.resolve_workspace_service_key_by_dir(
        &execution.record.project,
        &execution.record.workspace,
    )?;
    command
        .env("DEVFLOW_WORKSPACE_KEY", key)
        .env("DEVFLOW_BRANCH", &execution.record.workspace);
    let (status, _, _) = run_recorded(&mut command, &mut execution, false).await?;
    Ok(status.code().unwrap_or(-1))
}

#[cfg(test)]
mod tests {
    use super::ExecutionOutput;

    #[test]
    fn execution_output_preserves_the_previous_conditional_fields() {
        let foreground = serde_json::to_value(ExecutionOutput {
            workspace: "feature/auth".into(),
            execution_id: "fixture".into(),
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
            execution_id: "fixture".into(),
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

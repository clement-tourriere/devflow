use anyhow::{Context, Result};
use std::path::Path;

use super::ActionResult;
use crate::hooks::template::TemplateEngine;
use crate::hooks::HookContext;

/// Execute a command inside a Docker container.
///
/// Uses the Docker Engine API (bollard) when the `service-local` feature is
/// available; otherwise falls back to the `docker` CLI.
pub async fn execute(
    container_template: &str,
    command_template: &str,
    user: Option<&str>,
    context: &HookContext,
    template_engine: &TemplateEngine,
    working_dir: &Path,
    print_output: bool,
) -> Result<ActionResult> {
    let container = template_engine.render(container_template, context)?;
    let command = template_engine.render(command_template, context)?;
    let rendered_user = user
        .map(|tpl| template_engine.render(tpl, context))
        .transpose()?;

    let (exit_code, stdout, stderr) =
        run_in_container(&container, &command, rendered_user.as_deref(), working_dir).await?;

    if exit_code != 0 {
        anyhow::bail!(
            "docker exec failed (exit {}): {} in {}\nstdout: {}\nstderr: {}",
            exit_code,
            command,
            container,
            stdout.trim(),
            stderr.trim()
        );
    }

    if print_output && !stdout.trim().is_empty() {
        println!("{}", stdout.trim());
    }

    Ok(ActionResult {
        summary: format!("docker-exec: {} in {}", command, container),
    })
}

#[cfg(feature = "service-local")]
async fn run_in_container(
    container: &str,
    command: &str,
    user: Option<&str>,
    _working_dir: &Path,
) -> Result<(i64, String, String)> {
    use futures_util::TryStreamExt;

    let docker = bollard::Docker::connect_with_local_defaults()
        .context("Failed to connect to Docker daemon. Is Docker installed and running?")?;

    let config = bollard::models::ExecConfig {
        cmd: Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            command.to_string(),
        ]),
        user: user.map(str::to_string),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        ..Default::default()
    };

    let exec = docker
        .create_exec(container, config)
        .await
        .with_context(|| format!("Failed to docker exec in container: {}", container))?;

    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    match docker
        .start_exec(&exec.id, None::<bollard::exec::StartExecOptions>)
        .await?
    {
        bollard::exec::StartExecResults::Attached { mut output, .. } => {
            while let Some(msg) = output.try_next().await? {
                match msg {
                    bollard::container::LogOutput::StdOut { message } => {
                        stdout_buf.extend_from_slice(&message)
                    }
                    bollard::container::LogOutput::StdErr { message } => {
                        stderr_buf.extend_from_slice(&message)
                    }
                    _ => {}
                }
            }
        }
        bollard::exec::StartExecResults::Detached => {}
    }

    let inspect = docker.inspect_exec(&exec.id).await?;
    Ok((
        inspect.exit_code.unwrap_or(-1),
        String::from_utf8_lossy(&stdout_buf).to_string(),
        String::from_utf8_lossy(&stderr_buf).to_string(),
    ))
}

#[cfg(not(feature = "service-local"))]
async fn run_in_container(
    container: &str,
    command: &str,
    user: Option<&str>,
    working_dir: &Path,
) -> Result<(i64, String, String)> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.arg("exec");
    if let Some(user) = user {
        cmd.args(["-u", user]);
    }
    cmd.arg(container);
    cmd.args(["sh", "-c", command]);
    cmd.current_dir(working_dir);

    let output = cmd
        .output()
        .await
        .with_context(|| format!("Failed to docker exec in container: {}", container))?;

    Ok((
        output.status.code().unwrap_or(-1) as i64,
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    ))
}

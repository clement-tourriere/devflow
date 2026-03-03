use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

use super::ActionResult;
use crate::hooks::template::TemplateEngine;
use crate::hooks::HookContext;

/// Execute a command inside a Docker container.
pub fn execute(
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

    let mut cmd = Command::new("docker");
    cmd.arg("exec");

    if let Some(user_template) = user {
        let rendered_user = template_engine.render(user_template, context)?;
        cmd.args(["-u", &rendered_user]);
    }

    cmd.arg(&container);
    cmd.args(["sh", "-c", &command]);
    cmd.current_dir(working_dir);

    let output = cmd
        .output()
        .with_context(|| format!("Failed to docker exec in container: {}", container))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "docker exec failed (exit {}): {} in {}\nstdout: {}\nstderr: {}",
            output.status.code().unwrap_or(-1),
            command,
            container,
            stdout.trim(),
            stderr.trim()
        );
    }

    if print_output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.trim().is_empty() {
            println!("{}", stdout.trim());
        }
    }

    Ok(ActionResult {
        summary: format!("docker-exec: {} in {}", command, container),
    })
}

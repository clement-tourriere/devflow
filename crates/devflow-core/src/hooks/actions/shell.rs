use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use super::ActionResult;
use crate::hooks::template::TemplateEngine;
use crate::hooks::HookContext;

/// Execute a shell command (extracted from the old executor for reuse).
pub fn execute(
    command_template: &str,
    context: &HookContext,
    template_engine: &TemplateEngine,
    working_dir: &Path,
    print_output: bool,
) -> Result<ActionResult> {
    let rendered = template_engine.render(command_template, context)?;
    run_shell_command(
        &rendered,
        working_dir,
        None,
        context,
        template_engine,
        print_output,
    )?;
    Ok(ActionResult {
        summary: format!("shell: {}", rendered),
    })
}

/// Low-level shell command runner shared across the hooks system.
pub fn run_shell_command(
    command: &str,
    working_dir: &Path,
    environment: Option<&HashMap<String, String>>,
    context: &HookContext,
    template_engine: &TemplateEngine,
    print_stdout: bool,
) -> Result<()> {
    let mut cmd = if cfg!(target_os = "windows") {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", command]);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", command]);
        cmd
    };

    cmd.current_dir(working_dir);

    if let Some(env_vars) = environment {
        for (key, value_template) in env_vars {
            let rendered_value = template_engine
                .render(value_template, context)
                .unwrap_or_else(|_| value_template.clone());
            cmd.env(key, rendered_value);
        }
    }

    let output = cmd
        .output()
        .with_context(|| format!("Failed to execute hook command: {}", command))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "Hook command failed (exit {}): {}\nstdout: {}\nstderr: {}",
            output.status.code().unwrap_or(-1),
            command,
            stdout.trim(),
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if print_stdout && !stdout.trim().is_empty() {
        println!("{}", stdout.trim());
    }

    Ok(())
}

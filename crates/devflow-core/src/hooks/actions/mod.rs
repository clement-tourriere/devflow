pub mod copy;
pub mod docker_exec;
pub mod http;
pub mod notify;
pub mod replace;
pub mod shell;
pub mod write_env;
pub mod write_file;

use anyhow::Result;
use std::path::Path;

use super::template::TemplateEngine;
use super::HookContext;

/// Result of executing a single action.
#[derive(Debug)]
pub struct ActionResult {
    /// Human-readable summary of what was done
    pub summary: String,
}

/// Execute a `HookAction`, rendering all template fields first.
pub async fn execute_action(
    action: &super::HookAction,
    context: &HookContext,
    template_engine: &TemplateEngine,
    working_dir: &Path,
    print_output: bool,
) -> Result<ActionResult> {
    match action {
        super::HookAction::Shell { command } => {
            shell::execute(command, context, template_engine, working_dir, print_output)
        }
        super::HookAction::Replace {
            file,
            pattern,
            replacement,
            regex,
            create_if_missing,
        } => replace::execute(
            file,
            pattern,
            replacement,
            *regex,
            *create_if_missing,
            context,
            template_engine,
            working_dir,
        ),
        super::HookAction::WriteFile {
            path,
            content,
            mode,
        } => write_file::execute(path, content, mode, context, template_engine, working_dir),
        super::HookAction::WriteEnv { path, vars, mode } => {
            write_env::execute(path, vars, mode, context, template_engine, working_dir)
        }
        super::HookAction::Copy {
            from,
            to,
            overwrite,
        } => copy::execute(from, to, *overwrite, context, template_engine, working_dir),
        super::HookAction::DockerExec {
            container,
            command,
            user,
        } => docker_exec::execute(
            container,
            command,
            user.as_deref(),
            context,
            template_engine,
            working_dir,
            print_output,
        ),
        super::HookAction::Http {
            url,
            method,
            body,
            headers,
        } => {
            http::execute(
                url,
                method,
                body.as_deref(),
                headers.as_ref(),
                context,
                template_engine,
            )
            .await
        }
        super::HookAction::Notify {
            title,
            message,
            level,
        } => notify::execute(title, message, level, context, template_engine),
    }
}

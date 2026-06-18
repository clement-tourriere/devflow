pub mod copy;
pub mod docker_exec;
pub mod http;
pub mod notify;
pub mod replace;
pub mod shell;
pub mod write_env;
pub mod write_file;

use anyhow::Result;
use std::path::{Component, Path, PathBuf};

use super::template::TemplateEngine;
use super::HookContext;

/// Result of executing a single action.
#[derive(Debug)]
pub struct ActionResult {
    /// Human-readable summary of what was done
    pub summary: String,
}

/// Resolve a template-rendered path relative to `working_dir` and verify it
/// stays confined to the workspace. Rejects absolute paths and relative paths
/// that escape via `..` components.
///
/// This is defense-in-depth against a malicious or typo'd config writing to
/// locations outside the project (e.g. `~/.ssh/authorized_keys`, `/etc/...`).
/// The working directory itself is the workspace root, so the common case of
/// writing `.env.local` is unaffected.
pub(crate) fn confine_path(working_dir: &Path, rendered: &str) -> Result<PathBuf> {
    let rendered_path = Path::new(rendered);
    if rendered_path.is_absolute() {
        anyhow::bail!(
            "action path must be relative to the workspace (got absolute path: {})",
            rendered
        );
    }

    let joined = working_dir.join(rendered_path);

    // Lexically normalize and confirm the result still starts with working_dir.
    // We can't canonicalize `joined` because the target may not exist yet
    // (write-file/create), so component analysis is the safe option.
    let mut normalized = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::ParentDir => {
                if !normalized.pop() {
                    anyhow::bail!("action path escapes the workspace: {}", rendered);
                }
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }

    if !normalized.starts_with(working_dir) {
        anyhow::bail!("action path escapes the workspace: {}", rendered);
    }

    Ok(normalized)
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

#[cfg(test)]
mod tests {
    use super::confine_path;
    use std::path::Path;

    #[test]
    fn test_confine_path_accepts_relative() {
        let wd = Path::new("/tmp/proj");
        assert_eq!(
            confine_path(wd, ".env.local").unwrap(),
            wd.join(".env.local")
        );
        assert_eq!(
            confine_path(wd, "config/db.yml").unwrap(),
            wd.join("config/db.yml")
        );
    }

    #[test]
    fn test_confine_path_rejects_absolute() {
        let wd = Path::new("/tmp/proj");
        let err = confine_path(wd, "/etc/passwd").unwrap_err();
        assert!(format!("{err}").contains("absolute"));
        assert!(confine_path(wd, "/Users/x/.ssh/authorized_keys").is_err());
    }

    #[test]
    fn test_confine_path_rejects_traversal_escape() {
        let wd = Path::new("/tmp/proj");
        assert!(confine_path(wd, "../../etc/passwd").is_err());
        assert!(confine_path(wd, "../sibling/.env").is_err());
    }

    #[test]
    fn test_confine_path_allows_traversal_that_stays_inside() {
        // `../proj/sub` from `/tmp/proj` resolves back into the workspace.
        let wd = Path::new("/tmp/proj");
        assert!(confine_path(wd, "../proj/sub").is_ok());
    }
}

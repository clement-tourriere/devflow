use anyhow::Result;
use std::path::Path;

use super::ActionResult;
use crate::files::{WorkspaceFiles, WritePolicy};
use crate::hooks::template::TemplateEngine;
use crate::hooks::{HookContext, WriteMode};

/// Write content to a file with configurable write mode.
pub fn execute(
    path_template: &str,
    content_template: &str,
    mode: &WriteMode,
    context: &HookContext,
    template_engine: &TemplateEngine,
    working_dir: &Path,
) -> Result<ActionResult> {
    let file_path = template_engine.render(path_template, context)?;
    let content = template_engine.render(content_template, context)?;

    let policy = match mode {
        WriteMode::Overwrite => WritePolicy::Replace,
        WriteMode::Append => WritePolicy::Append,
        WriteMode::CreateOnly => WritePolicy::CreateOnly,
    };
    let changed =
        WorkspaceFiles::open(working_dir)?.write(&file_path, content.as_bytes(), policy)?;
    if !changed {
        return Ok(ActionResult {
            summary: format!("write-file: {} (skipped, already exists)", file_path),
        });
    }

    Ok(ActionResult {
        summary: format!("write-file: {}", file_path),
    })
}

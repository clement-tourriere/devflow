use anyhow::Result;
use std::path::Path;

use super::ActionResult;
use crate::files::WorkspaceFiles;
use crate::hooks::template::TemplateEngine;
use crate::hooks::HookContext;

/// Copy a file from one location to another.
pub fn execute(
    from_template: &str,
    to_template: &str,
    overwrite: bool,
    context: &HookContext,
    template_engine: &TemplateEngine,
    working_dir: &Path,
) -> Result<ActionResult> {
    let from = template_engine.render(from_template, context)?;
    let to = template_engine.render(to_template, context)?;

    let changed = WorkspaceFiles::open(working_dir)?.copy(&from, &to, overwrite)?;
    if !changed {
        return Ok(ActionResult {
            summary: format!("copy: {} -> {} (skipped, destination exists)", from, to),
        });
    }

    Ok(ActionResult {
        summary: format!("copy: {} -> {}", from, to),
    })
}

use anyhow::{Context, Result};
use std::path::Path;

use super::ActionResult;
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

    let src = working_dir.join(&from);
    let dst = working_dir.join(&to);

    if !src.exists() {
        anyhow::bail!("Source file not found: {}", src.display());
    }

    if dst.exists() && !overwrite {
        return Ok(ActionResult {
            summary: format!("copy: {} -> {} (skipped, destination exists)", from, to),
        });
    }

    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }

    std::fs::copy(&src, &dst)
        .with_context(|| format!("Failed to copy {} -> {}", src.display(), dst.display()))?;

    Ok(ActionResult {
        summary: format!("copy: {} -> {}", from, to),
    })
}

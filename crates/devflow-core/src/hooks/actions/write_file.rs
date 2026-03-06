use anyhow::{Context, Result};
use std::path::Path;

use super::ActionResult;
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

    let path = working_dir.join(&file_path);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }

    match mode {
        WriteMode::Overwrite => {
            std::fs::write(&path, &content)
                .with_context(|| format!("Failed to write file: {}", path.display()))?;
        }
        WriteMode::Append => {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("Failed to open file for append: {}", path.display()))?;
            file.write_all(content.as_bytes())
                .with_context(|| format!("Failed to append to file: {}", path.display()))?;
        }
        WriteMode::CreateOnly => {
            if path.exists() {
                return Ok(ActionResult {
                    summary: format!("write-file: {} (skipped, already exists)", file_path),
                });
            }
            std::fs::write(&path, &content)
                .with_context(|| format!("Failed to write file: {}", path.display()))?;
        }
    }

    Ok(ActionResult {
        summary: format!("write-file: {}", file_path),
    })
}

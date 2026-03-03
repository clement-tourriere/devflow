use anyhow::{Context, Result};
use std::path::Path;

use super::ActionResult;
use crate::hooks::template::TemplateEngine;
use crate::hooks::HookContext;

/// Find-and-replace in a file. Supports plain string or regex patterns.
#[allow(clippy::too_many_arguments)]
pub fn execute(
    file_template: &str,
    pattern_template: &str,
    replacement_template: &str,
    use_regex: bool,
    create_if_missing: bool,
    context: &HookContext,
    template_engine: &TemplateEngine,
    working_dir: &Path,
) -> Result<ActionResult> {
    let file = template_engine.render(file_template, context)?;
    let pattern = template_engine.render(pattern_template, context)?;
    let replacement = template_engine.render(replacement_template, context)?;

    let path = working_dir.join(&file);

    let content = if path.exists() {
        std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read file: {}", path.display()))?
    } else if create_if_missing {
        String::new()
    } else {
        anyhow::bail!("File not found: {}", path.display());
    };

    let new_content = if use_regex {
        let re = regex::Regex::new(&pattern)
            .with_context(|| format!("Invalid regex pattern: {}", pattern))?;
        re.replace_all(&content, replacement.as_str()).to_string()
    } else {
        content.replace(&pattern, &replacement)
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }

    std::fs::write(&path, &new_content)
        .with_context(|| format!("Failed to write file: {}", path.display()))?;

    Ok(ActionResult {
        summary: format!("replace in {}", file),
    })
}

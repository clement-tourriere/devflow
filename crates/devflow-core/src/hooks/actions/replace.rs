use anyhow::{Context, Result};
use std::path::Path;

use super::ActionResult;
use crate::files::{WorkspaceFiles, WritePolicy};
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

    let files = WorkspaceFiles::open(working_dir)?;
    let content = match files.read(&file)? {
        Some(content) => content,
        None if create_if_missing => String::new(),
        None => anyhow::bail!("File not found: {}", file),
    };

    let new_content = if use_regex {
        let re = regex::Regex::new(&pattern)
            .with_context(|| format!("Invalid regex pattern: {}", pattern))?;
        re.replace_all(&content, replacement.as_str()).to_string()
    } else {
        content.replace(&pattern, &replacement)
    };

    files.write(&file, new_content.as_bytes(), WritePolicy::Replace)?;

    Ok(ActionResult {
        summary: format!("replace in {}", file),
    })
}

use anyhow::{Context, Result};
use indexmap::IndexMap;
use std::path::Path;

use super::ActionResult;
use crate::hooks::template::TemplateEngine;
use crate::hooks::{EnvWriteMode, HookContext};

/// Write environment variables to a dotenv-style file.
pub fn execute(
    path_template: &str,
    vars: &IndexMap<String, String>,
    mode: &EnvWriteMode,
    context: &HookContext,
    template_engine: &TemplateEngine,
    working_dir: &Path,
) -> Result<ActionResult> {
    let file_path = template_engine.render(path_template, context)?;
    let path = working_dir.join(&file_path);

    // Render all variable values through the template engine
    let mut rendered_vars = IndexMap::new();
    for (key, value_template) in vars {
        let rendered_key = template_engine.render(key, context)?;
        let rendered_value = template_engine.render(value_template, context)?;
        rendered_vars.insert(rendered_key, rendered_value);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }

    match mode {
        EnvWriteMode::Overwrite => {
            let content = format_env_file(&rendered_vars);
            std::fs::write(&path, content)
                .with_context(|| format!("Failed to write env file: {}", path.display()))?;
        }
        EnvWriteMode::Merge => {
            let mut existing = if path.exists() {
                parse_env_file(
                    &std::fs::read_to_string(&path)
                        .with_context(|| format!("Failed to read env file: {}", path.display()))?,
                )
            } else {
                IndexMap::new()
            };

            // Merge: new vars overwrite existing ones, keep others
            for (key, value) in &rendered_vars {
                existing.insert(key.clone(), value.clone());
            }

            let content = format_env_file(&existing);
            std::fs::write(&path, content)
                .with_context(|| format!("Failed to write env file: {}", path.display()))?;
        }
    }

    Ok(ActionResult {
        summary: format!(
            "write-env: {} ({} vars)",
            file_path,
            rendered_vars.len()
        ),
    })
}

/// Format variables as a dotenv file. Values containing spaces/special chars are quoted.
fn format_env_file(vars: &IndexMap<String, String>) -> String {
    let mut lines = Vec::new();
    for (key, value) in vars {
        if value.contains(' ')
            || value.contains('"')
            || value.contains('\'')
            || value.contains('#')
            || value.contains('\n')
        {
            // Quote the value, escaping inner double quotes
            let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
            lines.push(format!("{}=\"{}\"", key, escaped));
        } else {
            lines.push(format!("{}={}", key, value));
        }
    }
    let mut content = lines.join("\n");
    if !content.is_empty() {
        content.push('\n');
    }
    content
}

/// Parse a dotenv file into key-value pairs, preserving order.
fn parse_env_file(content: &str) -> IndexMap<String, String> {
    let mut vars = IndexMap::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            let key = key.trim().to_string();
            let value = value.trim();
            // Strip surrounding quotes
            let value = if (value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\''))
            {
                value[1..value.len() - 1].to_string()
            } else {
                value.to_string()
            };
            vars.insert(key, value);
        }
    }
    vars
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_env_simple() {
        let mut vars = IndexMap::new();
        vars.insert("FOO".to_string(), "bar".to_string());
        vars.insert("BAZ".to_string(), "qux".to_string());
        assert_eq!(format_env_file(&vars), "FOO=bar\nBAZ=qux\n");
    }

    #[test]
    fn test_format_env_quoted() {
        let mut vars = IndexMap::new();
        vars.insert("URL".to_string(), "hello world".to_string());
        assert_eq!(format_env_file(&vars), "URL=\"hello world\"\n");
    }

    #[test]
    fn test_parse_env_file() {
        let content = "FOO=bar\nBAZ=\"hello world\"\n# comment\nEMPTY=\n";
        let vars = parse_env_file(content);
        assert_eq!(vars.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(vars.get("BAZ"), Some(&"hello world".to_string()));
        assert_eq!(vars.get("EMPTY"), Some(&String::new()));
        assert_eq!(vars.len(), 3);
    }

    #[test]
    fn test_parse_env_preserves_order() {
        let content = "C=3\nA=1\nB=2\n";
        let vars = parse_env_file(content);
        let keys: Vec<_> = vars.keys().collect();
        assert_eq!(keys, vec!["C", "A", "B"]);
    }
}

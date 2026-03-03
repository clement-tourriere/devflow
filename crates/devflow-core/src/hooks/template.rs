use anyhow::{Context, Result};
use minijinja::{Environment, Value};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::HookContext;

/// MiniJinja-based template engine for hook commands and config values.
pub struct TemplateEngine {
    env: Environment<'static>,
}

impl TemplateEngine {
    /// Create a new template engine with devflow custom filters registered.
    pub fn new() -> Self {
        let mut env = Environment::new();

        // Register custom filters
        env.add_filter("sanitize", filter_sanitize);
        env.add_filter("sanitize_db", filter_sanitize_db);
        env.add_filter("hash_port", filter_hash_port);
        env.add_filter("lower", filter_lower);
        env.add_filter("upper", filter_upper);
        env.add_filter("replace", filter_replace);
        env.add_filter("truncate", filter_truncate);

        Self { env }
    }

    /// Render a template string with the given hook context.
    pub fn render(&self, template_str: &str, context: &HookContext) -> Result<String> {
        let ctx_value = minijinja::context! { ..Value::from_serialize(context) };

        self.env
            .render_str(template_str, ctx_value)
            .with_context(|| format!("Failed to render template: {}", template_str))
    }

    /// Render a template string with raw minijinja Value context.
    /// Useful when building context manually.
    #[allow(dead_code)] // Public API for advanced template usage
    pub fn render_with_value(&self, template_str: &str, ctx: Value) -> Result<String> {
        self.env
            .render_str(template_str, ctx)
            .with_context(|| format!("Failed to render template: {}", template_str))
    }
}

impl Default for TemplateEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ── Custom Filters ──────────────────────────────────────────────────────────

/// Replace `/`, `\`, and whitespace with `-`, then collapse consecutive dashes.
fn filter_sanitize(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut prev_dash = false;

    for ch in value.chars() {
        match ch {
            '/' | '\\' | ' ' | '\t' | '\n' => {
                if !prev_dash {
                    result.push('-');
                    prev_dash = true;
                }
            }
            c if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' => {
                result.push(c);
                prev_dash = false;
            }
            _ => {
                if !prev_dash {
                    result.push('-');
                    prev_dash = true;
                }
            }
        }
    }

    result.trim_matches('-').to_string()
}

/// Database-safe identifier: lowercase alphanumeric + underscore, max 63 chars
/// with a 4-hex-char hash suffix if truncated.
fn filter_sanitize_db(value: &str) -> String {
    let mut sanitized = String::new();

    for ch in value.to_lowercase().chars() {
        match ch {
            'a'..='z' | '0'..='9' | '_' => sanitized.push(ch),
            _ => sanitized.push('_'),
        }
    }

    // Ensure starts with letter or underscore
    if sanitized.starts_with(|c: char| c.is_ascii_digit()) {
        sanitized = format!("_{}", sanitized);
    }

    // Collapse consecutive underscores
    while sanitized.contains("__") {
        sanitized = sanitized.replace("__", "_");
    }
    sanitized = sanitized.trim_end_matches('_').to_string();
    // Trim leading underscores but keep one if needed to avoid starting with a digit
    while sanitized.starts_with('_') && sanitized.len() > 1 {
        let without = &sanitized[1..];
        if without.starts_with(|c: char| c.is_ascii_digit()) {
            break; // Keep the underscore prefix
        }
        sanitized = without.to_string();
    }
    sanitized = sanitized.trim_end_matches('_').to_string();
    // Trim leading underscores only if they weren't deliberately added for digit-start
    if !sanitized.starts_with(|c: char| {
        c == '_'
            && sanitized.len() > 1
            && sanitized
                .chars()
                .nth(1)
                .is_some_and(|c2| c2.is_ascii_digit())
    }) {
        sanitized = sanitized.trim_start_matches('_').to_string();
    }

    if sanitized.is_empty() {
        sanitized = "workspace".to_string();
    }

    // Truncate with hash if too long
    const MAX_LEN: usize = 63;
    if sanitized.len() > MAX_LEN {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        let hash = (hasher.finish() as u32) & 0xFFFF;
        let suffix = format!("_{:04x}", hash);
        let prefix_len = MAX_LEN - suffix.len();
        sanitized = format!("{}{}", &sanitized[..prefix_len], suffix);
    }

    sanitized
}

/// Hash a string to a port number in range 10000-19999.
fn filter_hash_port(value: &str) -> u16 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    let hash = hasher.finish();
    10000 + (hash % 10000) as u16
}

/// Lowercase filter (in case users expect it beyond MiniJinja builtins).
fn filter_lower(value: &str) -> String {
    value.to_lowercase()
}

/// Uppercase filter.
fn filter_upper(value: &str) -> String {
    value.to_uppercase()
}

/// Replace occurrences of `from` with `to` in value.
fn filter_replace(value: &str, from: &str, to: &str) -> String {
    value.replace(from, to)
}

/// Truncate to max length.
fn filter_truncate(value: &str, length: usize) -> String {
    if value.len() <= length {
        value.to_string()
    } else {
        value[..length].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{HookContext, ServiceContext};
    use std::collections::HashMap;

    fn test_context() -> HookContext {
        let mut services = HashMap::new();
        services.insert(
            "app-db".to_string(),
            ServiceContext {
                host: "localhost".to_string(),
                port: 55432,
                database: "myapp_feature_x".to_string(),
                user: "dev".to_string(),
                password: Some("dev".to_string()),
                url: "postgresql://dev:dev@localhost:55432/myapp_feature_x".to_string(),
            },
        );

        HookContext {
            workspace: "feature/my-cool-feature".to_string(),
            repo: "myapp".to_string(),
            default_workspace: "main".to_string(),
            service: services,
            ..Default::default()
        }
    }

    #[test]
    fn test_simple_template() {
        let engine = TemplateEngine::new();
        let ctx = test_context();
        let result = engine.render("echo {{ workspace }}", &ctx).unwrap();
        assert_eq!(result, "echo feature/my-cool-feature");
    }

    #[test]
    fn test_service_variable() {
        let engine = TemplateEngine::new();
        let ctx = test_context();
        let result = engine
            .render("DATABASE_URL={{ service['app-db'].url }}", &ctx)
            .unwrap();
        assert_eq!(
            result,
            "DATABASE_URL=postgresql://dev:dev@localhost:55432/myapp_feature_x"
        );
    }

    #[test]
    fn test_sanitize_filter() {
        assert_eq!(
            filter_sanitize("feature/my-workspace"),
            "feature-my-workspace"
        );
        assert_eq!(filter_sanitize("fix\\back\\slash"), "fix-back-slash");
        assert_eq!(filter_sanitize("  spaces  "), "spaces");
    }

    #[test]
    fn test_sanitize_db_filter() {
        assert_eq!(
            filter_sanitize_db("feature/my-workspace"),
            "feature_my_workspace"
        );
        assert_eq!(filter_sanitize_db("123start"), "_123start");
        assert_eq!(filter_sanitize_db(""), "workspace");
    }

    #[test]
    fn test_hash_port_filter() {
        let port = filter_hash_port("feature/my-workspace");
        assert!((10000..20000).contains(&port));
        // Same input always produces same port
        assert_eq!(port, filter_hash_port("feature/my-workspace"));
        // Different input produces different port (probabilistically)
        assert_ne!(filter_hash_port("feature/a"), filter_hash_port("feature/b"));
    }

    #[test]
    fn test_sanitize_filter_in_template() {
        let engine = TemplateEngine::new();
        let ctx = test_context();
        let result = engine.render("{{ workspace | sanitize }}", &ctx).unwrap();
        assert_eq!(result, "feature-my-cool-feature");
    }

    #[test]
    fn test_hash_port_filter_in_template() {
        let engine = TemplateEngine::new();
        let ctx = test_context();
        let result = engine.render("{{ workspace | hash_port }}", &ctx).unwrap();
        let port: u16 = result.parse().unwrap();
        assert!((10000..20000).contains(&port));
    }

    #[test]
    fn test_sanitize_db_filter_in_template() {
        let engine = TemplateEngine::new();
        let ctx = test_context();
        let result = engine
            .render("{{ workspace | sanitize_db }}", &ctx)
            .unwrap();
        assert_eq!(result, "feature_my_cool_feature");
    }

    #[test]
    fn test_complex_template() {
        let engine = TemplateEngine::new();
        let ctx = test_context();
        let template = r#"docker stop {{ repo }}-{{ workspace | sanitize }}-* 2>/dev/null || true"#;
        let result = engine.render(template, &ctx).unwrap();
        assert_eq!(
            result,
            "docker stop myapp-feature-my-cool-feature-* 2>/dev/null || true"
        );
    }
}

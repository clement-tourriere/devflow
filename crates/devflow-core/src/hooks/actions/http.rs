use anyhow::{Context, Result};
use std::collections::HashMap;

use super::ActionResult;
use crate::hooks::template::TemplateEngine;
use crate::hooks::HookContext;

/// Make an HTTP request. Uses a simple blocking approach via std::process
/// calling curl, since reqwest is an optional dependency.
pub async fn execute(
    url_template: &str,
    method_template: &str,
    body_template: Option<&str>,
    headers: Option<&HashMap<String, String>>,
    context: &HookContext,
    template_engine: &TemplateEngine,
) -> Result<ActionResult> {
    let url = template_engine.render(url_template, context)?;
    let method = template_engine.render(method_template, context)?;

    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-sS", "-X", &method.to_uppercase()]);

    // Add headers
    if let Some(hdrs) = headers {
        for (key, value_template) in hdrs {
            let rendered_key = template_engine.render(key, context)?;
            let rendered_value = template_engine.render(value_template, context)?;
            cmd.args(["-H", &format!("{}: {}", rendered_key, rendered_value)]);
        }
    }

    // Add body
    if let Some(body_tmpl) = body_template {
        let rendered_body = template_engine.render(body_tmpl, context)?;
        cmd.args(["-d", &rendered_body]);
    }

    cmd.arg(&url);

    let output = cmd
        .output()
        .with_context(|| format!("Failed to execute HTTP request to: {}", url))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "HTTP request failed (exit {}): {} {}\nstderr: {}",
            output.status.code().unwrap_or(-1),
            method.to_uppercase(),
            url,
            stderr.trim()
        );
    }

    Ok(ActionResult {
        summary: format!("http: {} {}", method.to_uppercase(), url),
    })
}

use anyhow::{Context, Result};
use std::collections::HashMap;

use super::ActionResult;
use crate::hooks::template::TemplateEngine;
use crate::hooks::HookContext;

/// Make an HTTP request (in-process via reqwest).
pub async fn execute(
    url_template: &str,
    method_template: &str,
    body_template: Option<&str>,
    headers: Option<&HashMap<String, String>>,
    context: &HookContext,
    template_engine: &TemplateEngine,
) -> Result<ActionResult> {
    let url = template_engine.render(url_template, context)?;
    let method = template_engine
        .render(method_template, context)?
        .to_uppercase();

    let http_method: reqwest::Method = method
        .parse()
        .with_context(|| format!("Invalid HTTP method: {}", method))?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("Failed to build HTTP client")?;

    let mut request = client.request(http_method, &url);

    if let Some(hdrs) = headers {
        for (key, value_template) in hdrs {
            let rendered_key = template_engine.render(key, context)?;
            let rendered_value = template_engine.render(value_template, context)?;
            request = request.header(rendered_key, rendered_value);
        }
    }

    if let Some(body_tmpl) = body_template {
        let rendered_body = template_engine.render(body_tmpl, context)?;
        request = request.body(rendered_body);
    }

    let response = request
        .send()
        .await
        .with_context(|| format!("Failed to execute HTTP request to: {}", url))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "HTTP request failed ({}): {} {}\nbody: {}",
            status,
            method,
            url,
            body.chars().take(500).collect::<String>()
        );
    }

    Ok(ActionResult {
        summary: format!("http: {} {}", method, url),
    })
}

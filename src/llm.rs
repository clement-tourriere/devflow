//! LLM integration for AI-powered commit message generation.
//!
//! Supports any OpenAI-compatible API (OpenAI, Anthropic via proxy, Ollama, etc.)
//! Configuration via environment variables:
//! - `DEVFLOW_LLM_API_KEY` — API key (optional for local models)
//! - `DEVFLOW_LLM_API_URL` — Base URL (default: `https://api.openai.com/v1`)
//! - `DEVFLOW_LLM_MODEL` — Model name (default: `gpt-4o-mini`)

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Maximum diff size (in bytes) to send to the LLM. Larger diffs get truncated.
const MAX_DIFF_SIZE: usize = 32_000;

/// Environment variable names.
const ENV_API_KEY: &str = "DEVFLOW_LLM_API_KEY";
const ENV_API_URL: &str = "DEVFLOW_LLM_API_URL";
const ENV_MODEL: &str = "DEVFLOW_LLM_MODEL";

/// Default values.
const DEFAULT_API_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "gpt-4o-mini";

/// LLM configuration resolved from environment variables.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub api_key: Option<String>,
    pub api_url: String,
    pub model: String,
}

impl LlmConfig {
    /// Load configuration from environment variables.
    pub fn from_env() -> Self {
        Self {
            api_key: std::env::var(ENV_API_KEY).ok().filter(|s| !s.is_empty()),
            api_url: std::env::var(ENV_API_URL)
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_API_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            model: std::env::var(ENV_MODEL)
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        }
    }

    /// Check if an API key is configured (required for remote APIs, optional for local).
    pub fn has_api_key(&self) -> bool {
        self.api_key.is_some()
    }

    /// Returns true if the URL looks like a local/Ollama endpoint.
    pub fn is_local(&self) -> bool {
        self.api_url.contains("localhost") || self.api_url.contains("127.0.0.1")
    }

    /// Check if LLM is configured (has API key or is local).
    pub fn is_configured(&self) -> bool {
        self.has_api_key() || self.is_local()
    }
}

// --- OpenAI-compatible API types ---

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
    max_tokens: u32,
}

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    content: String,
}

#[derive(Deserialize)]
struct ApiError {
    error: ApiErrorDetail,
}

#[derive(Deserialize)]
struct ApiErrorDetail {
    message: String,
}

/// Generate a commit message from a diff using an LLM.
///
/// Returns the generated message, or an error if the API call fails.
#[cfg(feature = "llm")]
pub async fn generate_commit_message(diff: &str, summary: &str) -> Result<String> {
    let config = LlmConfig::from_env();

    if !config.is_configured() {
        anyhow::bail!(
            "LLM not configured. Set {} for remote APIs, or point {} to a local endpoint (e.g. Ollama).",
            ENV_API_KEY,
            ENV_API_URL,
        );
    }

    let truncated_diff = truncate_diff(diff);

    let system_prompt = build_system_prompt();
    let user_prompt = build_user_prompt(&truncated_diff, summary);

    call_chat_api(&config, &system_prompt, &user_prompt).await
}

/// Call the OpenAI-compatible chat completions API.
#[cfg(feature = "llm")]
async fn call_chat_api(
    config: &LlmConfig,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<String> {
    let url = format!("{}/chat/completions", config.api_url);

    let request_body = ChatRequest {
        model: config.model.clone(),
        messages: vec![
            ChatMessage {
                role: "system".to_string(),
                content: system_prompt.to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: user_prompt.to_string(),
            },
        ],
        temperature: 0.3,
        max_tokens: 256,
    };

    let client = reqwest::Client::new();
    let mut req = client.post(&url).json(&request_body);

    if let Some(ref api_key) = config.api_key {
        req = req.header("Authorization", format!("Bearer {}", api_key));
    }

    let response = req.send().await.context("Failed to connect to LLM API")?;

    let status = response.status();
    let body = response
        .text()
        .await
        .context("Failed to read LLM API response")?;

    if !status.is_success() {
        // Try to parse structured error
        if let Ok(api_err) = serde_json::from_str::<ApiError>(&body) {
            anyhow::bail!("LLM API error ({}): {}", status, api_err.error.message);
        }
        anyhow::bail!("LLM API error ({}): {}", status, body);
    }

    let chat_response: ChatResponse =
        serde_json::from_str(&body).context("Failed to parse LLM API response")?;

    let message = chat_response
        .choices
        .first()
        .map(|c| c.message.content.trim().to_string())
        .unwrap_or_default();

    if message.is_empty() {
        anyhow::bail!("LLM returned an empty commit message");
    }

    Ok(message)
}

/// Truncate a diff to fit within the token budget.
fn truncate_diff(diff: &str) -> String {
    if diff.len() <= MAX_DIFF_SIZE {
        return diff.to_string();
    }
    let mut truncated = diff[..MAX_DIFF_SIZE].to_string();
    truncated.push_str("\n\n... [diff truncated — too large for LLM context]");
    truncated
}

fn build_system_prompt() -> String {
    r#"You are a commit message generator. Given a git diff and summary, write a concise conventional commit message.

Rules:
- Use the Conventional Commits format: type(scope): description
- Types: feat, fix, refactor, docs, style, test, chore, perf, ci, build
- The scope is optional — include it when the change is clearly scoped to a module/file
- The description should be lowercase, imperative mood, no period at the end
- Keep the first line under 72 characters
- If the change is substantial, add a blank line then a short body (1-3 bullet points)
- Do NOT wrap the message in quotes or backticks
- Output ONLY the commit message, nothing else"#.to_string()
}

fn build_user_prompt(diff: &str, summary: &str) -> String {
    format!(
        "Here is the diff summary:\n\n```\n{}\n```\n\nHere is the full diff:\n\n```\n{}\n```\n\nWrite a commit message for these changes.",
        summary, diff
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_llm_config_from_env() {
        // Test all config scenarios sequentially to avoid env var races.
        // Each scenario sets ALL relevant vars explicitly.

        // 1. No API key, default URL and model
        std::env::remove_var(ENV_API_KEY);
        std::env::remove_var(ENV_API_URL);
        std::env::remove_var(ENV_MODEL);
        let config = LlmConfig::from_env();
        assert!(!config.has_api_key());
        assert_eq!(config.api_url, DEFAULT_API_URL);
        assert_eq!(config.model, DEFAULT_MODEL);
        assert!(!config.is_configured());

        // 2. With API key
        std::env::set_var(ENV_API_KEY, "test-key-123");
        let config = LlmConfig::from_env();
        assert!(config.has_api_key());
        assert!(config.is_configured());
        std::env::remove_var(ENV_API_KEY);

        // 3. Local endpoint (no API key needed)
        std::env::set_var(ENV_API_URL, "http://localhost:11434/v1");
        let config = LlmConfig::from_env();
        assert!(!config.has_api_key());
        assert!(config.is_local());
        assert!(config.is_configured());
        std::env::remove_var(ENV_API_URL);

        // 4. Trailing slash stripped
        std::env::set_var(ENV_API_URL, "https://api.example.com/v1/");
        let config = LlmConfig::from_env();
        assert_eq!(config.api_url, "https://api.example.com/v1");
        std::env::remove_var(ENV_API_URL);
    }

    #[test]
    fn test_truncate_diff_small() {
        let diff = "small diff";
        assert_eq!(truncate_diff(diff), "small diff");
    }

    #[test]
    fn test_truncate_diff_large() {
        let diff = "x".repeat(MAX_DIFF_SIZE + 1000);
        let result = truncate_diff(&diff);
        assert!(result.len() < diff.len());
        assert!(result.contains("[diff truncated"));
    }

    #[test]
    fn test_build_system_prompt() {
        let prompt = build_system_prompt();
        assert!(prompt.contains("Conventional Commits"));
        assert!(prompt.contains("feat"));
    }

    #[test]
    fn test_build_user_prompt() {
        let prompt = build_user_prompt("some diff", "3 files changed");
        assert!(prompt.contains("some diff"));
        assert!(prompt.contains("3 files changed"));
    }
}

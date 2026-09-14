//! External commit-message generation. No API clients or credentials.
use anyhow::{Context, Result};
const MAX_DIFF_SIZE: usize = 32_000;

pub async fn generate_commit_message_via_cli(
    command: &str,
    diff: &str,
    summary: &str,
) -> Result<String> {
    use tokio::io::AsyncWriteExt;

    let truncated_diff = truncate_diff(diff);
    let prompt = build_user_prompt(&truncated_diff, summary);
    let system = build_system_prompt();
    let full_prompt = format!("{}\n\n{}", system, prompt);

    let mut child = tokio::process::Command::new("sh")
        .args(["-c", command])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to spawn LLM CLI command")?;

    // Write prompt to stdin
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(full_prompt.as_bytes()).await?;
        stdin.shutdown().await?;
    }

    let output = child
        .wait_with_output()
        .await
        .context("Failed to read LLM CLI output")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "LLM CLI command failed (exit {}): {}",
            output.status,
            stderr.trim()
        );
    }

    let message = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if message.is_empty() {
        anyhow::bail!("LLM CLI returned an empty commit message");
    }

    Ok(message)
}

/// Truncate a diff to fit within the token budget.
fn truncate_diff(diff: &str) -> String {
    if diff.len() <= MAX_DIFF_SIZE {
        return diff.to_string();
    }
    let mut end = MAX_DIFF_SIZE;
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = diff[..end].to_string();
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

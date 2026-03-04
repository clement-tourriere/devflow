use anyhow::{Context, Result};
use devflow_core::config::Config;
use devflow_core::vcs;

pub(super) async fn handle_commit_command(
    message: Option<String>,
    ai: bool,
    edit: bool,
    dry_run: bool,
    json_output: bool,
    config: &Config,
) -> Result<()> {
    let vcs = vcs::detect_vcs_provider(".")?;

    // Check for staged changes
    if !vcs.has_staged_changes()? {
        if json_output {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({"error": "no staged changes"}))?
            );
        } else {
            println!("No staged changes to commit.");
            println!("Stage changes first, e.g.: git add <files>");
        }
        return Ok(());
    }

    // Determine the commit message
    let final_message = if let Some(msg) = message {
        // Explicit -m message — use as-is
        msg
    } else if ai {
        // AI-generated message
        generate_ai_commit_message(vcs.as_ref(), config, json_output).await?
    } else {
        // No --ai, no --message: open editor for manual message
        let initial = String::new();
        open_editor_for_message(&initial)?
    };

    // --edit: let user review/edit (even with -m or --ai)
    let final_message = if edit {
        open_editor_for_message(&final_message)?
    } else {
        final_message
    };

    if final_message.trim().is_empty() {
        anyhow::bail!("Aborting commit: empty commit message");
    }

    if dry_run {
        if json_output {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({"message": final_message}))?
            );
        } else {
            println!("Generated commit message:\n");
            println!("{}", final_message);
        }
        return Ok(());
    }

    // Perform the commit
    vcs.commit(&final_message)?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "committed": true,
                "message": final_message
            }))?
        );
    } else {
        println!("Committed: {}", first_line(&final_message));
    }

    Ok(())
}

/// Generate a commit message using the configured LLM.
///
/// Prefers external CLI command (e.g., `claude -p`, `llm`, `aichat`) if configured,
/// falling back to the OpenAI-compatible API.
#[cfg(feature = "llm")]
async fn generate_ai_commit_message(
    vcs: &dyn vcs::VcsProvider,
    config: &Config,
    _json_output: bool,
) -> Result<String> {
    use devflow_core::llm;

    let commit_gen_config = config.commit.as_ref().and_then(|c| c.generation.as_ref());
    let llm_config = llm::LlmConfig::from_config_and_env(commit_gen_config);

    // Prefer external CLI command
    if let Some(ref cmd) = llm_config.cli_command {
        let diff = vcs.staged_diff()?;
        let summary = vcs.staged_summary()?;
        eprintln!("Generating commit message via: {}...", cmd);
        return llm::generate_commit_message_via_cli(cmd, &diff, &summary).await;
    }

    // Fallback to API
    if !llm_config.is_configured() {
        anyhow::bail!(
            "LLM not configured. Either:\n\
             1. Set 'commit.generation.command' in .devflow.yml (e.g., \"claude -p --model=haiku\")\n\
             2. Set DEVFLOW_COMMIT_COMMAND env var\n\
             3. Set DEVFLOW_LLM_API_KEY for OpenAI-compatible API"
        );
    }

    let diff = vcs.staged_diff()?;
    let summary = vcs.staged_summary()?;
    eprintln!(
        "Generating commit message with {} ({})...",
        llm_config.model, llm_config.api_url
    );
    llm::generate_commit_message(&diff, &summary).await
}

#[cfg(not(feature = "llm"))]
async fn generate_ai_commit_message(
    _vcs: &dyn vcs::VcsProvider,
    _config: &Config,
    _json_output: bool,
) -> Result<String> {
    anyhow::bail!("LLM support not compiled in. Rebuild with the `llm` feature enabled.");
}

/// Open the user's editor to compose or edit a commit message.
fn open_editor_for_message(initial_content: &str) -> Result<String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());

    // Write initial content to a temp file
    let dir = std::env::temp_dir();
    let file_path = dir.join("devflow_commit_msg.txt");
    let content_with_help = if initial_content.is_empty() {
        "\n# Write your commit message above.\n# Lines starting with '#' will be ignored.\n# Empty message aborts the commit.\n".to_string()
    } else {
        format!(
            "{}\n\n# Edit the commit message above.\n# Lines starting with '#' will be ignored.\n# Empty message aborts the commit.\n",
            initial_content
        )
    };
    std::fs::write(&file_path, &content_with_help)?;

    // Open editor
    let status = std::process::Command::new(&editor)
        .arg(&file_path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .with_context(|| format!("Failed to open editor: {}", editor))?;

    if !status.success() {
        anyhow::bail!("Editor exited with non-zero status");
    }

    // Read back and strip comment lines
    let raw = std::fs::read_to_string(&file_path)?;
    let _ = std::fs::remove_file(&file_path);

    let message: String = raw
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    Ok(message)
}

/// Return the first line of a message (for display).
pub(super) fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

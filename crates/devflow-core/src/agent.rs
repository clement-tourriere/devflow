//! AI agent integration for devflow.
//!
//! Provides commands for launching, tracking, and managing AI coding agents
//! that work in isolated branch environments.

use anyhow::Result;
use std::path::Path;

use crate::config::Config;

/// Generate a Claude Code skill file for this project.
pub fn generate_claude_skill(config: &Config, _project_dir: &Path) -> Result<String> {
    let project_name = config.project_name();
    let services = config.resolve_services();

    let mut skill = String::new();
    skill.push_str(&format!("# devflow - {}\n\n", project_name));
    skill.push_str(
        "This project uses **devflow** for branch-isolated development environments.\n\n",
    );

    // Commands reference
    skill.push_str("## Quick Reference\n\n");
    skill.push_str("```bash\n");
    skill.push_str("# Switch to a branch (creates isolated services)\n");
    skill.push_str("devflow switch -c <branch-name>\n\n");
    skill.push_str("# Get connection info\n");
    skill.push_str("devflow connection <branch-name>\n");
    skill.push_str("devflow --json connection <branch-name>\n\n");
    skill.push_str("# Show current status\n");
    skill.push_str("devflow status\n\n");
    skill.push_str("# AI-powered commit\n");
    skill.push_str("devflow commit --ai\n\n");
    skill.push_str("# Run hooks manually\n");
    skill.push_str("devflow hook run post-switch\n\n");
    skill.push_str("# Show template variables\n");
    skill.push_str("devflow hook vars\n\n");
    skill.push_str("# Get full context for current branch\n");
    skill.push_str("devflow agent context\n");
    skill.push_str("```\n\n");

    // Services
    if !services.is_empty() {
        skill.push_str("## Configured Services\n\n");
        for svc in &services {
            skill.push_str(&format!(
                "- **{}**: {} ({}) {}\n",
                svc.name,
                svc.service_type,
                svc.provider_type,
                if svc.auto_branch {
                    "[auto-branch]"
                } else {
                    "[shared]"
                }
            ));
        }
        skill.push('\n');
        skill.push_str(
            "Connection strings are available via `devflow --json connection <branch>`.\n",
        );
        skill.push_str("In hooks, use `{{ service['<name>'].url }}` template syntax.\n\n");
    }

    // Hooks
    if let Some(ref hooks) = config.hooks {
        skill.push_str("## Configured Hooks\n\n");
        for (phase, named_hooks) in hooks {
            skill.push_str(&format!("### {}\n", phase));
            for (name, _entry) in named_hooks {
                skill.push_str(&format!("- {}\n", name));
            }
            skill.push('\n');
        }
    }

    // Agent workflow
    skill.push_str("## Agent Workflow\n\n");
    skill.push_str("When working on a task:\n\n");
    skill.push_str("1. Create an isolated branch: `devflow switch -c agent/<task-id>`\n");
    skill.push_str("2. Get connection info: `devflow --json connection agent/<task-id>`\n");
    skill.push_str("3. Do your work in the isolated environment\n");
    skill.push_str("4. Commit with AI message: `devflow commit --ai`\n");
    skill.push_str("5. Clean up when done: `devflow remove agent/<task-id>`\n\n");

    // Flags for automation
    skill.push_str("## Automation Flags\n\n");
    skill.push_str("- `--json`: Structured JSON output on stdout\n");
    skill.push_str("- `--non-interactive`: Skip prompts, use defaults\n");
    skill.push_str("- `--no-verify` on `switch`: Skip hook approval prompts\n");

    Ok(skill)
}

/// Generate an OpenCode / AGENTS.md configuration for this project.
pub fn generate_opencode_config(config: &Config, _project_dir: &Path) -> Result<String> {
    let project_name = config.project_name();
    let services = config.resolve_services();

    let mut content = String::new();
    content.push_str(&format!("# devflow for {}\n\n", project_name));
    content.push_str("This guide is for autonomous coding agents and CI runners.\n\n");

    content.push_str("## Goal\n\n");
    content.push_str("Use devflow to create an isolated development branch environment per task, with machine-readable output and deterministic behavior.\n\n");

    content.push_str("## Recommended Flags\n\n");
    content.push_str("- `--json`: structured output on stdout\n");
    content.push_str("- `--non-interactive`: disable prompts in automation\n");
    content.push_str(
        "- `--no-verify` on `switch`: skip lifecycle hooks when approval prompts are possible\n\n",
    );

    content.push_str("## Start Work on a New Task\n\n");
    content.push_str("```bash\n");
    content.push_str("TASK_ID=\"issue-123\"\n");
    content.push_str(
        "devflow --json --non-interactive switch -c \"agent/$TASK_ID\" --no-verify\n",
    );
    content.push_str("CONN=$(devflow --json connection \"agent/$TASK_ID\")\n");
    content.push_str("```\n\n");

    if !services.is_empty() {
        content.push_str("## Available Services\n\n");
        for svc in &services {
            content.push_str(&format!(
                "- **{}** ({}): `devflow --json connection <branch> -s {}`\n",
                svc.name, svc.service_type, svc.name
            ));
        }
        content.push('\n');
    }

    content.push_str("## Cleanup\n\n");
    content.push_str("```bash\n");
    content.push_str("devflow --json --non-interactive remove \"agent/$TASK_ID\" --force\n");
    content.push_str("```\n\n");

    content.push_str("## Automation Contract\n\n");
    content.push_str(
        "- `service create`, `service delete`, and `switch` return non-zero exit code on failure\n",
    );
    content.push_str("- `destroy` and `remove` require `--force` in `--non-interactive` mode\n");
    content
        .push_str("- Use `devflow --json capabilities` for a machine-readable summary\n");

    Ok(content)
}

/// Generate Cursor rules for this project.
pub fn generate_cursor_rules(config: &Config, _project_dir: &Path) -> Result<String> {
    let project_name = config.project_name();
    let services = config.resolve_services();

    let mut content = String::new();
    content.push_str(&format!("# devflow rules for {}\n\n", project_name));
    content.push_str(
        "This project uses devflow for branch-isolated development environments.\n\n",
    );

    content.push_str("## Key Commands\n\n");
    content.push_str("- Create isolated branch: `devflow switch -c <branch>`\n");
    content.push_str("- Get connection info: `devflow --json connection <branch>`\n");
    content.push_str("- Show status: `devflow status`\n");
    content.push_str("- AI commit: `devflow commit --ai`\n");
    content.push_str("- Show template vars: `devflow hook vars`\n\n");

    if !services.is_empty() {
        content.push_str("## Services\n\n");
        for svc in &services {
            content.push_str(&format!(
                "- {}: {} ({})\n",
                svc.name, svc.service_type, svc.provider_type
            ));
        }
        content.push('\n');
    }

    content.push_str("## Rules\n\n");
    content.push_str("- Always use `devflow --json` for machine-readable output\n");
    content.push_str(
        "- Use `devflow connection <branch>` to get database URLs, never hardcode them\n",
    );
    content.push_str("- Use `devflow switch -c` to create new branches with isolated services\n");
    content.push_str("- Use `devflow commit --ai` for consistent commit messages\n");

    Ok(content)
}

/// Generate project context for agents (JSON or markdown).
pub async fn generate_agent_context(
    config: &Config,
    branch_name: &str,
    format: &str,
) -> Result<String> {
    let context = crate::hooks::build_hook_context(config, branch_name).await;

    match format {
        "json" => Ok(serde_json::to_string_pretty(&context)?),
        _ => {
            let mut md = String::new();
            md.push_str(&format!("# Agent Context: {}\n\n", branch_name));
            md.push_str(&format!("**Branch**: {}\n", context.branch));
            md.push_str(&format!("**Repo**: {}\n", context.repo));
            md.push_str(&format!(
                "**Default Branch**: {}\n",
                context.default_branch
            ));
            if let Some(ref wt) = context.worktree_path {
                md.push_str(&format!("**Worktree**: {}\n", wt));
            }
            md.push_str("\n## Services\n\n");
            for (name, svc) in &context.service {
                md.push_str(&format!("### {}\n", name));
                md.push_str(&format!("- URL: `{}`\n", svc.url));
                md.push_str(&format!("- Host: {}\n", svc.host));
                md.push_str(&format!("- Port: {}\n", svc.port));
                md.push_str(&format!("- Database: {}\n", svc.database));
                md.push_str(&format!("- User: {}\n\n", svc.user));
            }
            Ok(md)
        }
    }
}

//! AI agent integration for devflow.
//!
//! This module deliberately generates small, project-specific guide files at
//! install time instead of embedding a bundle of fixed skills in the binary.
//! The generated files are plain Markdown so different agents/harnesses can
//! consume the same workflow guidance in their preferred location.

use anyhow::{bail, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::config::Config;

/// A supported destination for generated agent guidance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentGuideTarget {
    /// Harness-agnostic Markdown guide under `.agents/`.
    Generic,
    /// Claude Code skill file.
    Claude,
    /// Cursor rule file.
    Cursor,
    /// OpenCode-local guide file.
    Opencode,
}

impl AgentGuideTarget {
    pub fn parse(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "generic" | "agents" | "agent" => Ok(Self::Generic),
            "claude" | "claude-code" => Ok(Self::Claude),
            "cursor" => Ok(Self::Cursor),
            "opencode" | "open-code" => Ok(Self::Opencode),
            other => bail!(
                "Unknown agent guide target '{}'. Use one of: generic, claude, cursor, opencode, all.",
                other
            ),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Claude => "claude",
            Self::Cursor => "cursor",
            Self::Opencode => "opencode",
        }
    }

    pub fn default_path(self) -> &'static str {
        match self {
            Self::Generic => ".agents/devflow.md",
            Self::Claude => ".claude/skills/devflow/SKILL.md",
            Self::Cursor => ".cursor/rules/devflow.md",
            Self::Opencode => ".opencode/AGENTS.md",
        }
    }
}

/// All generated guide targets installed by the compatibility `agent skill` path.
pub fn default_agent_guide_targets() -> Vec<AgentGuideTarget> {
    vec![
        AgentGuideTarget::Generic,
        AgentGuideTarget::Claude,
        AgentGuideTarget::Cursor,
        AgentGuideTarget::Opencode,
    ]
}

/// Status of generated agent guide installation for a project.
#[derive(Debug, Clone, Serialize)]
pub struct SkillInstallStatus {
    pub installed: bool,
    pub installed_skills: Vec<String>,
    pub missing_skills: Vec<String>,
    /// Whether any installed guide has outdated content or a default target is missing.
    pub update_available: bool,
    /// Installed guide targets whose content differs from current generated content.
    pub stale_skills: Vec<String>,
}

/// Generate a project-specific devflow guide for one target.
pub fn generate_agent_guide(
    config: &Config,
    project_dir: &Path,
    target: AgentGuideTarget,
) -> String {
    let project_name = config.project_name();
    let repo = project_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| project_name.clone());
    let services = config.resolve_services();

    let mut out = String::new();
    if target == AgentGuideTarget::Claude {
        out.push_str("---\n");
        out.push_str("name: devflow\n");
        out.push_str(&format!(
            "description: Workspace, hook, and service workflow for the {} devflow project.\n",
            project_name
        ));
        out.push_str("---\n\n");
    }

    out.push_str(&format!("# devflow workflow for {}\n\n", project_name));
    out.push_str(&format!("Repository: `{}`\n\n", repo));
    out.push_str("devflow creates/switches per-workspace development environments: VCS worktrees, service instances, and lifecycle hooks. This guide is generated locally by `devflow agent skill`; it is not a bundled skill catalog.\n\n");

    out.push_str("## Agent-safe workspace flow\n\n");
    out.push_str("```bash\n");
    out.push_str("WORKSPACE=agent/<task-id>\n");
    out.push_str("OUTPUT=$(devflow --json --non-interactive switch -c \"$WORKSPACE\")\n");
    out.push_str(
        "AGENT_WORKDIR=$(echo \"$OUTPUT\" | jq -r '.agent_workdir // .worktree_path // empty')\n",
    );
    out.push_str("# Pass AGENT_WORKDIR as the working directory/workdir to later tool calls.\n");
    if !services.is_empty() {
        out.push_str("devflow --json service connection \"$WORKSPACE\"\n");
    }
    out.push_str("```\n\n");
    out.push_str("Important: a child process cannot `cd` the already-running agent or parent shell. Treat `agent_workdir` from JSON output as the authoritative directory and set the harness/tool workdir explicitly.\n\n");

    out.push_str("## Hooks\n\n");
    out.push_str("- Hooks run by default during `devflow switch`; do **not** pass `--no-verify` unless the user explicitly wants to skip setup/migration hooks.\n");
    out.push_str("- In `--non-interactive` mode, shell hooks must be pre-approved with `devflow hook approvals add \"<command>\"`.\n");
    out.push_str("- Inspect hooks without running them: `devflow hook show`, `devflow hook explain post-create`, `devflow --json hook vars`.\n\n");

    if !services.is_empty() {
        out.push_str("## Services\n\n");
        for svc in &services {
            out.push_str(&format!(
                "- `{}`: {} via {}{}\n",
                svc.name,
                svc.service_type,
                svc.provider_type,
                if svc.auto_workspace {
                    " (per-workspace)"
                } else {
                    " (shared)"
                }
            ));
        }
        out.push_str("\nUse `devflow --json service connection <workspace>` for machine-readable connection info.\n\n");
    }

    if let Some(ref hooks) = config.hooks {
        if !hooks.is_empty() {
            out.push_str("## Configured hook phases\n\n");
            for (phase, named_hooks) in hooks {
                let names = named_hooks.keys().cloned().collect::<Vec<_>>().join(", ");
                out.push_str(&format!("- `{}`: {}\n", phase, names));
            }
            out.push('\n');
        }
    }

    out.push_str("## Sandbox note\n\n");
    out.push_str("devflow no longer tries to be a general-purpose sandbox. If a task needs strong isolation, use the agent harness, containers, VMs, or another dedicated sandbox and let devflow manage workspaces/services inside it.\n\n");

    out.push_str("## Useful commands\n\n");
    out.push_str("```bash\n");
    out.push_str("devflow --json list\n");
    out.push_str("devflow --json status\n");
    out.push_str("devflow --json capabilities\n");
    out.push_str("devflow hook approvals list\n");
    out.push_str("devflow service status\n");
    out.push_str("devflow remove <workspace> --force\n");
    out.push_str("```\n");

    out
}

/// Generate a root `AGENTS.md` style guide.
pub fn generate_agents_md(config: &Config, project_dir: &Path) -> String {
    let mut out = generate_agent_guide(config, project_dir, AgentGuideTarget::Generic);
    out.push_str("\n## Recommended default for agents\n\n");
    out.push_str("Prefer `devflow --json --non-interactive switch -c <workspace>` and then use the returned `agent_workdir` as the workdir.\n");
    out
}

/// Install generated devflow guide files for the requested targets.
pub fn install_agent_guides(
    config: &Config,
    project_dir: &Path,
    targets: &[AgentGuideTarget],
) -> Result<Vec<String>> {
    let mut written = Vec::new();
    for target in targets {
        let rel = target.default_path();
        let full_path = project_dir.join(rel);
        write_text_file(
            &full_path,
            &generate_agent_guide(config, project_dir, *target),
        )?;
        written.push(full_path.display().to_string());
    }
    Ok(written)
}

/// Backward-compatible API name used by GUI/TUI surfaces.
pub fn install_agent_skills(config: &Config, project_dir: &Path) -> Result<Vec<String>> {
    install_agent_guides(config, project_dir, &default_agent_guide_targets())
}

/// Write or replace the root AGENTS.md guide.
pub fn install_agents_md(config: &Config, project_dir: &Path) -> Result<PathBuf> {
    let path = project_dir.join("AGENTS.md");
    write_text_file(&path, &generate_agents_md(config, project_dir))?;
    Ok(path)
}

/// Remove generated devflow guide files and legacy bundled-skill directories.
pub fn uninstall_agent_skills(project_dir: &Path) -> Result<()> {
    for target in default_agent_guide_targets() {
        remove_file_or_dir(&project_dir.join(target.default_path()))?;
    }

    // Legacy cleanup from the old embedded-skill implementation.
    for legacy in [
        ".claude/skills/devflow-workspace-list",
        ".claude/skills/devflow-workspace-switch",
        ".claude/skills/devflow-workspace-create",
        ".claude/skills/devflow-brainstorming",
    ] {
        remove_file_or_dir(&project_dir.join(legacy))?;
    }

    Ok(())
}

/// Check whether generated agent guides are installed and current.
pub fn check_agent_skills_installed(project_dir: &Path) -> SkillInstallStatus {
    let config = Config::from_file(&project_dir.join(".devflow.yml")).unwrap_or_default();
    let mut installed_skills = Vec::new();
    let mut missing_skills = Vec::new();
    let mut stale_skills = Vec::new();

    for target in default_agent_guide_targets() {
        let label = target.label().to_string();
        let path = project_dir.join(target.default_path());
        if path.exists() {
            installed_skills.push(label.clone());
            let expected = generate_agent_guide(&config, project_dir, target);
            if std::fs::read_to_string(&path)
                .map(|actual| actual.trim() != expected.trim())
                .unwrap_or(true)
            {
                stale_skills.push(label);
            }
        } else {
            missing_skills.push(label);
        }
    }

    SkillInstallStatus {
        installed: missing_skills.is_empty() && stale_skills.is_empty(),
        installed_skills,
        update_available: !missing_skills.is_empty() || !stale_skills.is_empty(),
        missing_skills,
        stale_skills,
    }
}

/// Generate project context for agents (JSON or markdown).
pub async fn generate_agent_context(
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
    format: &str,
) -> Result<String> {
    let context = crate::hooks::build_hook_context(config, project_dir, workspace_name).await;
    let project_workdir = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf())
        .to_string_lossy()
        .to_string();
    let agent_workdir = context
        .worktree_path
        .clone()
        .unwrap_or_else(|| project_workdir.clone());
    let cd_hint = format!("cd {}", agent_workdir);

    match format {
        "json" => {
            let mut value = serde_json::to_value(&context)?;
            if let serde_json::Value::Object(ref mut map) = value {
                map.insert(
                    "agent_workdir".to_string(),
                    serde_json::Value::String(agent_workdir),
                );
                map.insert("cd_hint".to_string(), serde_json::Value::String(cd_hint));
            }
            Ok(serde_json::to_string_pretty(&value)?)
        }
        _ => {
            let mut md = String::new();
            md.push_str(&format!("# Agent Context: {}\n\n", workspace_name));
            md.push_str(&format!("**Project**: {}\n", context.name));
            md.push_str(&format!("**Workspace**: {}\n", context.workspace));
            md.push_str(&format!("**Repo**: {}\n", context.repo));
            md.push_str(&format!(
                "**Default Workspace**: {}\n",
                context.default_workspace
            ));
            if let Some(ref wt) = context.worktree_path {
                md.push_str(&format!("**Worktree**: {}\n", wt));
            }
            md.push_str(&format!("**Agent workdir**: {}\n", agent_workdir));
            md.push_str(&format!("**cd hint**: `{}`\n", cd_hint));
            md.push_str("\n## Services\n\n");
            if context.service.is_empty() {
                md.push_str("No services configured.\n");
            } else {
                for (name, svc) in &context.service {
                    md.push_str(&format!("### {}\n", name));
                    md.push_str(&format!("- URL: `{}`\n", svc.url));
                    md.push_str(&format!("- Host: {}\n", svc.host));
                    md.push_str(&format!("- Port: {}\n", svc.port));
                    md.push_str(&format!("- Database: {}\n", svc.database));
                    md.push_str(&format!("- User: {}\n\n", svc.user));
                }
            }
            Ok(md)
        }
    }
}

fn write_text_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}

fn remove_file_or_dir(path: &Path) -> Result<()> {
    if path.is_symlink() || path.is_file() {
        std::fs::remove_file(path)?;
    } else if path.exists() {
        std::fs::remove_dir_all(path)?;
    }
    Ok(())
}

//! AI agent integration for devflow.
//!
//! Provides commands for launching, tracking, and managing AI coding agents
//! that work in isolated workspace environments.

use anyhow::Result;
use std::path::Path;

use crate::config::Config;

/// The standard skills directory (Agent Skills open standard, supported by Claude Code, Cursor, OpenCode).
const SKILLS_DIR: &str = ".agents/skills";

/// A generated skill file with its relative path and content.
#[derive(Debug, Clone)]
pub struct SkillFile {
    /// Relative path under `.agents/skills/devflow/`, e.g. `SKILL.md` or `workspace-list/SKILL.md`
    pub relative_path: String,
    pub content: String,
}

/// Status of agent skill installation for a project.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillInstallStatus {
    pub installed: bool,
    pub installed_skills: Vec<String>,
    pub missing_skills: Vec<String>,
    /// Whether any installed skills have outdated content or new skills are available.
    pub update_available: bool,
    /// Skills whose installed content differs from the current generated content.
    pub stale_skills: Vec<String>,
}

/// Generate a Claude Code skill file for this project.
pub fn generate_claude_skill(config: &Config, _project_dir: &Path) -> Result<String> {
    let project_name = config.project_name();
    let services = config.resolve_services();

    let mut skill = String::new();
    skill.push_str("---\n");
    skill.push_str("name: devflow\n");
    skill.push_str(&format!(
        "description: devflow workspace management overview for {}\n",
        project_name
    ));
    skill.push_str("---\n\n");
    skill.push_str(&format!("# devflow - {}\n\n", project_name));
    skill.push_str(
        "This project uses **devflow** for workspace-isolated development environments.\n\n",
    );

    // Commands reference
    skill.push_str("## Quick Reference\n\n");
    skill.push_str("```bash\n");
    skill.push_str("# Switch to a workspace (creates isolated services)\n");
    skill.push_str("devflow switch -c <workspace-name>\n\n");
    if !services.is_empty() {
        skill.push_str("# Get connection info\n");
        skill.push_str("devflow connection <workspace-name>\n");
        skill.push_str("devflow --json connection <workspace-name>\n\n");
    }
    skill.push_str("# Show current status\n");
    skill.push_str("devflow status\n\n");
    skill.push_str("# AI-powered commit\n");
    skill.push_str("devflow commit --ai\n\n");
    skill.push_str("# Run hooks manually\n");
    skill.push_str("devflow hook run post-switch\n\n");
    skill.push_str("# Show template variables\n");
    skill.push_str("devflow hook vars\n\n");
    skill.push_str("# Get full context for current workspace\n");
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
                if svc.auto_workspace {
                    "[auto-workspace]"
                } else {
                    "[shared]"
                }
            ));
        }
        skill.push('\n');
        skill.push_str(
            "Connection strings are available via `devflow --json connection <workspace>`.\n",
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
    skill.push_str("1. Create an isolated workspace:\n");
    skill.push_str("   ```bash\n");
    skill.push_str("   OUTPUT=$(devflow --json --non-interactive switch -c agent/<task-id>)\n");
    skill.push_str("   ```\n");
    skill.push_str("2. If worktrees are enabled, switch to the worktree directory:\n");
    skill.push_str("   ```bash\n");
    skill.push_str(
        "   WORKTREE=$(echo \"$OUTPUT\" | jq -r '.worktree_path // empty')\n",
    );
    skill.push_str("   [ -n \"$WORKTREE\" ] && cd \"$WORKTREE\"\n");
    skill.push_str("   ```\n");
    if !services.is_empty() {
        skill.push_str(
            "3. Get connection info: `devflow --json connection agent/<task-id>`\n",
        );
        skill.push_str("4. Do your work in the isolated environment\n");
        skill.push_str("5. Commit with AI message: `devflow commit --ai`\n");
        skill.push_str("6. Clean up when done: `devflow remove agent/<task-id>`\n\n");
    } else {
        skill.push_str("3. Do your work in the isolated environment\n");
        skill.push_str("4. Commit with AI message: `devflow commit --ai`\n");
        skill.push_str("5. Clean up when done: `devflow remove agent/<task-id>`\n\n");
    }

    // Flags for automation
    skill.push_str("## Automation Flags\n\n");
    skill.push_str("- `--json`: Structured JSON output on stdout\n");
    skill.push_str("- `--non-interactive`: Skip prompts, use defaults (hooks still run but require pre-approval)\n");
    skill.push_str("- `--no-verify` on `switch`: Skip **all** hooks entirely (not recommended — use `--non-interactive` instead)\n\n");
    skill.push_str("### Hook Pre-Approval\n\n");
    skill.push_str("In `--non-interactive` mode, hooks with shell commands require pre-approval:\n");
    skill.push_str("```bash\n");
    skill.push_str("devflow hook approvals add \"<command>\"  # Approve a specific hook command\n");
    skill.push_str("devflow hook approvals list              # List approved hooks\n");
    skill.push_str("```\n");

    Ok(skill)
}

/// Generate individual workspace management skills (Agent Skills open standard).
///
/// Each skill is a separate top-level directory under `.agents/skills/`.
pub fn generate_workspace_skills() -> Vec<SkillFile> {
    vec![
        SkillFile {
            relative_path: "devflow-workspace-list/SKILL.md".to_string(),
            content: r#"---
name: devflow-workspace-list
description: List all devflow workspaces with their status, services, and worktree paths.
---

## When to use

- You need to see which workspaces exist in the project
- You want to check service statuses across workspaces
- You need to find a workspace's worktree path before navigating to it
- You want to verify workspace state after creating or switching

## Instructions

1. Run `devflow --json list` to get structured workspace data
2. Parse the JSON array — each object contains:
   - `name` — workspace identifier
   - `is_current` — boolean, whether this is the active workspace
   - `is_default` — boolean, whether this is the default (main) workspace
   - `worktree_path` — filesystem path to the worktree directory (if any)
   - `parent` — parent workspace name
   - `services` — array of service objects with `name`, `status`, `service_type`
3. Present the results clearly, highlighting the current workspace

Use `devflow list` (without `--json`) for human-readable output when not parsing programmatically.

## Examples

List all workspaces as JSON:

```bash
devflow --json list
```

List workspaces in human-readable format:

```bash
devflow list
```

Check which workspace is currently active:

```bash
devflow --json list | jq '.[] | select(.is_current) | .name'
```
"#
            .to_string(),
        },
        SkillFile {
            relative_path: "devflow-workspace-switch/SKILL.md".to_string(),
            content: r#"---
name: devflow-workspace-switch
description: Switch to an existing devflow workspace and its isolated services.
---

## When to use

- You need to change the active workspace to work on a different task
- You want to switch services (databases, caches) to match a specific workspace
- After listing workspaces, you want to activate one of them

## Instructions

1. The workspace name is provided in `$ARGUMENTS`
2. Run `devflow --json --non-interactive switch $ARGUMENTS` to switch
3. Parse the JSON output and check for `worktree_path` — if present, change your working directory to it
4. Verify the switch succeeded with `devflow status`
5. If the workspace has services, retrieve connection info with `devflow --json connection $ARGUMENTS`
   - If this returns `"services": "none_configured"`, the project uses workspaces without database services — skip this step
6. Report the new workspace state and any connection strings to the user

Always use `--json --non-interactive` when running as an agent. Do NOT use `--no-verify` — it skips all lifecycle hooks (e.g. migrations, env setup) which are usually needed.

## Examples

Switch to an existing workspace:

```bash
OUTPUT=$(devflow --json --non-interactive switch my-feature)
WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
[ -n "$WORKTREE" ] && cd "$WORKTREE"
```

Verify the switch and get connection info:

```bash
devflow status
devflow --json connection my-feature
```
"#
            .to_string(),
        },
        SkillFile {
            relative_path: "devflow-workspace-create/SKILL.md".to_string(),
            content: r#"---
name: devflow-workspace-create
description: Create a new devflow workspace with isolated services for a task or feature.
---

## When to use

- You are starting work on a new task or feature that needs isolated services
- You need a fresh database or cache instance that won't affect other workspaces
- You want to set up a parallel development environment with its own worktree

## Instructions

1. The workspace name is provided in `$ARGUMENTS`
2. Run `devflow --json --non-interactive switch -c $ARGUMENTS` to create and switch
   - The `-c` flag creates the workspace if it doesn't exist
   - This provisions isolated service instances (databases, caches) automatically
   - If worktrees are enabled, a new Git worktree directory is created
   - Lifecycle hooks (e.g. `post-create`, `post-switch`) run automatically
3. **Parse the JSON output** to check for `worktree_path`:
   - If `worktree_path` is present, **change your working directory** to it — this is where you should do all subsequent work
   - If `worktree_created` is `true`, a new worktree was just created for this workspace
4. If the project has database services, retrieve connection info with `devflow --json connection $ARGUMENTS`
   - If this returns `"services": "none_configured"`, the project uses workspaces without database services — skip this step
5. Report the new workspace details including service connection strings to the user

Use a descriptive name like `feature/auth-refactor` or `agent/task-123` for the workspace.

**Important**: Do NOT use `--no-verify` — it skips all lifecycle hooks (migrations, env setup, etc.) which are usually needed for a working environment.

## Examples

Create a new workspace for a feature:

```bash
OUTPUT=$(devflow --json --non-interactive switch -c feature/my-task)

# If worktrees are enabled, switch to the worktree directory
WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
[ -n "$WORKTREE" ] && cd "$WORKTREE"
```

Get connection strings for the new workspace:

```bash
devflow --json connection feature/my-task
```

Create a workspace and immediately get full context:

```bash
OUTPUT=$(devflow --json --non-interactive switch -c agent/task-42)
WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
[ -n "$WORKTREE" ] && cd "$WORKTREE"
devflow --json connection agent/task-42
devflow agent context
```
"#
            .to_string(),
        },
    ]
}

/// All top-level skill directory names managed by devflow.
const MANAGED_SKILL_DIRS: &[&str] = &[
    "devflow-workspace-list",
    "devflow-workspace-switch",
    "devflow-workspace-create",
];

/// Install all agent skills into `.agents/skills/` under the project directory.
///
/// Uses the Agent Skills open standard (agentskills.io), compatible with
/// Cursor, OpenCode, and other tools that support the standard.
///
/// Also creates per-skill symlinks in `.claude/skills/` pointing back to
/// `.agents/skills/` so Claude Code picks them up too.
///
/// Returns the list of written file paths.
pub fn install_agent_skills(_config: &Config, project_dir: &Path) -> Result<Vec<String>> {
    let skills_dir = project_dir.join(SKILLS_DIR);

    let mut written = Vec::new();

    // Write individual workspace skills
    for skill_file in generate_workspace_skills() {
        let full_path = skills_dir.join(&skill_file.relative_path);
        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full_path, &skill_file.content)?;
        written.push(full_path.display().to_string());
    }

    // Symlink each skill dir in .claude/skills/ → ../../.agents/skills/<name>
    ensure_claude_skill_symlinks(project_dir)?;

    Ok(written)
}

/// Create a symlink in `.claude/skills/<name>` → `../../.agents/skills/<name>`
/// for each devflow-managed skill directory.
fn ensure_claude_skill_symlinks(project_dir: &Path) -> Result<()> {
    let claude_skills_dir = project_dir.join(".claude").join("skills");
    std::fs::create_dir_all(&claude_skills_dir)?;

    for dir_name in MANAGED_SKILL_DIRS {
        let claude_link = claude_skills_dir.join(dir_name);
        let relative_target = Path::new("..").join("..").join(SKILLS_DIR).join(dir_name);

        if claude_link.is_symlink() {
            if let Ok(target) = std::fs::read_link(&claude_link) {
                if target == relative_target {
                    continue;
                }
            }
            std::fs::remove_file(&claude_link)?;
        } else if claude_link.exists() {
            std::fs::remove_dir_all(&claude_link)?;
        }

        #[cfg(unix)]
        std::os::unix::fs::symlink(&relative_target, &claude_link)?;

        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&relative_target, &claude_link)?;
    }

    Ok(())
}

/// Remove all devflow-managed skills from `.agents/skills/` and their Claude symlinks.
pub fn uninstall_agent_skills(project_dir: &Path) -> Result<()> {
    let skills_dir = project_dir.join(SKILLS_DIR);
    let claude_skills_dir = project_dir.join(".claude").join("skills");

    for dir_name in MANAGED_SKILL_DIRS {
        // Remove canonical directory
        let dir = skills_dir.join(dir_name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }

        // Remove Claude symlink or copy
        let claude_link = claude_skills_dir.join(dir_name);
        if claude_link.is_symlink() {
            std::fs::remove_file(&claude_link)?;
        } else if claude_link.exists() {
            std::fs::remove_dir_all(&claude_link)?;
        }
    }

    Ok(())
}

/// Check whether agent skills are installed for a project, and whether they need updating.
///
/// Compares installed skill file content against the current generated content
/// to detect outdated skills. Also detects new skills that aren't installed yet.
pub fn check_agent_skills_installed(project_dir: &Path) -> SkillInstallStatus {
    let skills_dir = project_dir.join(SKILLS_DIR);

    let mut installed_skills = Vec::new();
    let mut missing_skills = Vec::new();
    let mut stale_skills = Vec::new();

    // Build a map of expected skill content for comparison
    let generated_skills = generate_workspace_skills();
    let expected_content: std::collections::HashMap<&str, &str> = generated_skills
        .iter()
        .map(|s| {
            // Extract the top-level dir name from the relative path (e.g. "devflow-workspace-list/SKILL.md" -> "devflow-workspace-list")
            let dir_name = s
                .relative_path
                .split('/')
                .next()
                .unwrap_or(&s.relative_path);
            (dir_name, s.content.as_str())
        })
        .collect();

    for dir_name in MANAGED_SKILL_DIRS {
        let skill_file = skills_dir.join(dir_name).join("SKILL.md");
        if skill_file.exists() {
            installed_skills.push(dir_name.to_string());

            // Check if content matches
            if let Some(expected) = expected_content.get(dir_name) {
                if let Ok(actual) = std::fs::read_to_string(&skill_file) {
                    if actual.trim() != expected.trim() {
                        stale_skills.push(dir_name.to_string());
                    }
                }
            }
        } else {
            missing_skills.push(dir_name.to_string());
        }
    }

    let update_available = !stale_skills.is_empty() || !missing_skills.is_empty();

    SkillInstallStatus {
        installed: missing_skills.is_empty() && stale_skills.is_empty(),
        installed_skills,
        missing_skills,
        update_available,
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

    match format {
        "json" => Ok(serde_json::to_string_pretty(&context)?),
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

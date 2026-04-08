//! AI agent integration for devflow.
//!
//! Provides commands for launching, tracking, and managing AI coding agents
//! that work in isolated workspace environments.

use anyhow::Result;
use std::path::Path;

use crate::config::Config;

/// The standard skills directory (Agent Skills open standard, supported by Claude Code, Cursor, OpenCode).
const SKILLS_DIR: &str = ".claude/skills";

/// A generated skill file with its relative path and content.
#[derive(Debug, Clone)]
pub struct SkillFile {
    /// Relative path under `.claude/skills/`, e.g. `devflow-workspace-list/SKILL.md`
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
    skill.push_str("# Run a command in a workspace\n");
    skill.push_str("devflow switch -c <workspace-name> -x 'command' -- 'args'\n\n");
    skill.push_str("# Run in background (detached)\n");
    skill.push_str("devflow switch -c <workspace-name> -x 'command' --detach -- 'args'\n\n");
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
    skill.push_str("1. Create an isolated workspace and run a command:\n");
    skill.push_str("   ```bash\n");
    skill.push_str("   # Sandboxed AI agent in a new workspace\n");
    skill.push_str(
        "   devflow switch -c --sandboxed agent/<task-id> -x claude -- 'Fix the bug'\n\n",
    );
    skill.push_str("   # Or detached in background\n");
    skill.push_str(
        "   devflow switch -c --sandboxed agent/<task-id> -x claude --detach -- 'Fix the bug'\n",
    );
    skill.push_str("   ```\n");
    skill.push_str("2. Or create workspace first, then work interactively:\n");
    skill.push_str("   ```bash\n");
    skill.push_str("   OUTPUT=$(devflow --json --non-interactive switch -c agent/<task-id>)\n");
    skill.push_str("   WORKTREE=$(echo \"$OUTPUT\" | jq -r '.worktree_path // empty')\n");
    skill.push_str("   # For agents, use WORKTREE as the workdir for later tool calls\n");
    skill.push_str("   ```\n");
    if !services.is_empty() {
        skill.push_str("3. Get connection info: `devflow --json connection agent/<task-id>`\n");
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
    skill
        .push_str("In `--non-interactive` mode, hooks with shell commands require pre-approval:\n");
    skill.push_str("```bash\n");
    skill.push_str("devflow hook approvals add \"<command>\"  # Approve a specific hook command\n");
    skill.push_str("devflow hook approvals list              # List approved hooks\n");
    skill.push_str("```\n");

    Ok(skill)
}

/// Generate individual workspace management skills (Agent Skills open standard).
///
/// Each skill is a separate top-level directory under `.claude/skills/`.
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
3. Parse the JSON output and check for `worktree_path`
   - In agent tools, do not rely on `cd` inside a shell command to retarget the session
   - Instead, use `worktree_path` as the working directory/workdir for subsequent tool calls
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
# For agents, use WORKTREE as the workdir for later tool calls
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
2. Run `devflow --json --non-interactive switch -c --sandboxed $ARGUMENTS` to create and switch
   - The `-c` flag creates the workspace if it doesn't exist
   - The `--sandboxed` flag enables sandbox mode — restricting filesystem access and blocking dangerous commands (git push, npm publish, sudo, ssh, etc.)
   - This provisions isolated service instances (databases, caches) automatically
   - If worktrees are enabled, a new Git worktree directory is created
   - Lifecycle hooks (e.g. `post-create`, `post-switch`) run automatically
3. **Parse the JSON output** to check for `worktree_path`:
   - If `worktree_path` is present, use it as the working directory/workdir for subsequent tool calls
   - Do not rely on shell `cd` to retarget an already running agent session
   - If `worktree_created` is `true`, a new worktree was just created for this workspace
4. If the project has database services, retrieve connection info with `devflow --json connection $ARGUMENTS`
   - If this returns `"services": "none_configured"`, the project uses workspaces without database services — skip this step
5. Report the new workspace details including service connection strings to the user

Use a descriptive name like `feature/auth-refactor` or `agent/task-123` for the workspace.

**Important**: Do NOT use `--no-verify` — it skips all lifecycle hooks (migrations, env setup, etc.) which are usually needed for a working environment.

### Sandbox mode

By default, always use `--sandboxed` when creating workspaces. This provides OS-level filesystem isolation and blocks dangerous commands.

If the user explicitly asks you to create a workspace **without** sandbox restrictions (e.g. they need `git push` access or SSH), use `--no-sandbox` instead:

```bash
devflow --json --non-interactive switch -c --no-sandbox $ARGUMENTS
```

## Examples

Create a new sandboxed workspace for a feature:

```bash
OUTPUT=$(devflow --json --non-interactive switch -c --sandboxed feature/my-task)

# For agents, use WORKTREE as the workdir for later tool calls
WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
```

Get connection strings for the new workspace:

```bash
devflow --json connection feature/my-task
```

Create a workspace and run a command in it:

```bash
devflow switch -c --sandboxed agent/task-42 -x claude -- 'Implement the auth feature'
```

Create a workspace and immediately get full context:

```bash
OUTPUT=$(devflow --json --non-interactive switch -c --sandboxed agent/task-42)
WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
# For agents, use WORKTREE as the workdir for later tool calls
devflow --json connection agent/task-42
devflow agent context
```
"#
            .to_string(),
        },
        SkillFile {
            relative_path: "devflow-brainstorming/SKILL.md".to_string(),
            content: r#"---
name: devflow-brainstorming
description: "Use before any creative work — creating features, building components, adding functionality, or modifying behavior. Explores user intent, requirements and design through collaborative dialogue, then transitions to implementation in an isolated devflow workspace."
---

# Brainstorming Ideas Into Designs

## Overview

Help turn ideas into fully formed designs and specs through natural collaborative dialogue.

Start by understanding the current project context, then ask questions one at a time to refine the idea. Once you understand what you're building, present the design, get user approval, then create an isolated devflow workspace for implementation.

**HARD GATE**: Do NOT write any code, scaffold any project, or take any implementation action until you have presented a design and the user has approved it. This applies to EVERY task regardless of perceived simplicity.

## Anti-Pattern: "This Is Too Simple To Need A Design"

Every project goes through this process. A todo list, a single-function utility, a config change — all of them. "Simple" projects are where unexamined assumptions cause the most wasted work. The design can be short (a few sentences for truly simple tasks), but you MUST present it and get approval.

## Checklist

Complete these steps in order:

1. **Explore project context** — check files, docs, recent commits, `devflow status`
2. **Ask clarifying questions** — one at a time, understand purpose/constraints/success criteria
3. **Propose 2-3 approaches** — with trade-offs and your recommendation
4. **Present design** — in sections scaled to complexity, get user approval after each section
5. **Write design doc** — save to `docs/plans/YYYY-MM-DD-<topic>-design.md`
6. **Optionally create devflow workspace** — only if the user requests isolation or the task is complex (multi-file changes, risky refactors, long-running work)
7. **Write implementation plan** — break the approved design into concrete tasks

## The Process

### Understanding the idea

- Check out the current project state first (files, docs, recent commits)
- Run `devflow status` and `devflow list` to understand the workspace context
- Ask questions one at a time to refine the idea
- Prefer multiple choice questions when possible, but open-ended is fine too
- Only one question per message — if a topic needs more exploration, break it into multiple questions
- Focus on understanding: purpose, constraints, success criteria

### Exploring approaches

- Propose 2-3 different approaches with trade-offs
- Present options conversationally with your recommendation and reasoning
- Lead with your recommended option and explain why

### Presenting the design

- Once you believe you understand what you're building, present the design
- Scale each section to its complexity: a few sentences if straightforward, up to 200-300 words if nuanced
- Ask after each section whether it looks right so far
- Cover: architecture, components, data flow, error handling, testing
- Be ready to go back and clarify if something doesn't make sense

### Transition to implementation

After the user approves the design:

1. Save the design to `docs/plans/YYYY-MM-DD-<topic>-design.md`
2. **Evaluate whether an isolated workspace is needed.** Create one if:
   - The user explicitly asks for isolation
   - The task involves significant multi-file changes, risky refactors, or long-running work
   - There's risk of interfering with other ongoing work

   For simple, contained changes, skip workspace creation and work directly in the current workspace.

   To create an isolated workspace:

```bash
OUTPUT=$(devflow --json --non-interactive switch -c --sandboxed feature/<topic>)
WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
# For agents, use WORKTREE as the workdir for later tool calls
```

3. Write the implementation plan as `docs/plans/YYYY-MM-DD-<topic>-plan.md`
4. Begin implementation — if in an isolated workspace, changes are contained and won't affect other work

## Key Principles

- **One question at a time** — Don't overwhelm with multiple questions
- **Multiple choice preferred** — Easier to answer than open-ended when possible
- **YAGNI ruthlessly** — Remove unnecessary features from all designs
- **Explore alternatives** — Always propose 2-3 approaches before settling
- **Incremental validation** — Present design, get approval before moving on
- **Isolate work** — Use devflow workspaces so implementation doesn't interfere with ongoing work
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
    "devflow-brainstorming",
];

/// Install all agent skills into `.claude/skills/` under the project directory.
///
/// Skills are written directly to `.claude/skills/<name>/SKILL.md`, which is
/// natively discovered by Claude Code, OpenCode, Cursor, and other tools.
///
/// When the `skills` feature is enabled, also updates `.devflow/skills.lock`
/// so the skills management system and GUI/TUI can see installed skills.
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

    // Update skills.lock so the skills management system stays in sync
    #[cfg(feature = "skills")]
    sync_skills_lock(project_dir)?;

    Ok(written)
}

/// Sync the skills.lock file with the bundled skills that were just written to disk.
///
/// This ensures that the skills management system (GUI, TUI, CLI `skill list`)
/// can see skills installed by `install_agent_skills()`.
#[cfg(feature = "skills")]
fn sync_skills_lock(project_dir: &Path) -> Result<()> {
    use crate::skills::{bundled::bundled_skills, manifest, types::InstalledSkill};
    use chrono::Utc;

    let mut lock = manifest::load_lock(project_dir)?;
    let now = Utc::now();

    for skill in bundled_skills() {
        // Only add/update if not already in lock or if content changed
        let needs_update = match lock.skills.get(&skill.name) {
            Some(existing) => existing.content_hash != skill.content_hash,
            None => true,
        };
        if needs_update {
            lock.skills.insert(
                skill.name.clone(),
                InstalledSkill {
                    source: skill.source,
                    content_hash: skill.content_hash,
                    installed_at: now,
                },
            );
        }
    }

    manifest::save_lock(project_dir, &lock)?;
    Ok(())
}

/// Remove all devflow-managed skills from `.claude/skills/`.
pub fn uninstall_agent_skills(project_dir: &Path) -> Result<()> {
    let skills_dir = project_dir.join(SKILLS_DIR);

    for dir_name in MANAGED_SKILL_DIRS {
        let dir = skills_dir.join(dir_name);
        if dir.is_symlink() {
            std::fs::remove_file(&dir)?;
        } else if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
    }

    // Remove uninstalled skills from skills.lock
    #[cfg(feature = "skills")]
    {
        use crate::skills::manifest;
        if let Ok(mut lock) = manifest::load_lock(project_dir) {
            for dir_name in MANAGED_SKILL_DIRS {
                lock.skills.remove(*dir_name);
            }
            let _ = manifest::save_lock(project_dir, &lock);
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

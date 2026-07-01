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
2. Run `devflow --json --non-interactive switch -c $ARGUMENTS` to create and switch
   - The `-c` flag creates the workspace if it doesn't exist
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

## Examples

Create a new workspace for a feature:

```bash
OUTPUT=$(devflow --json --non-interactive switch -c feature/my-task)

# For agents, use WORKTREE as the workdir for later tool calls
WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
```

Get connection strings for the new workspace:

```bash
devflow --json connection feature/my-task
```

Create a workspace and run a command in it:

```bash
devflow switch -c agent/task-42 -x claude -- 'Implement the auth feature'
```

Create a workspace and immediately get full context:

```bash
OUTPUT=$(devflow --json --non-interactive switch -c agent/task-42)
WORKTREE=$(echo "$OUTPUT" | jq -r '.worktree_path // empty')
# For agents, use WORKTREE as the workdir for later tool calls
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

/// Install all agent skills into `.claude/skills/` under the project directory.
///
/// Skills are written directly to `.claude/skills/<name>/SKILL.md`, which is
/// natively discovered by Claude Code, OpenCode, Cursor, and other tools.
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

    Ok(written)
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

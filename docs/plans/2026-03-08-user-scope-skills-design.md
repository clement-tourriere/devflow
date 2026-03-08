# User-Scope Skills Management Design

**Date:** 2026-03-08
**Status:** Approved

## Overview

Add user-scope (global) skill management alongside the existing project-scope system. Users can install skills once at the user level and have them automatically available across all projects, while also symlinking into AI agents that support user-scope skill directories (OpenCode, Codex CLI).

## Agent User-Scope Skill Support

| Agent | User-scope path | Supports skills dir | Symlink target |
|-------|----------------|--------------------|-|
| OpenCode | `~/.config/opencode/skills/<name>/SKILL.md` | Yes | Yes |
| Codex CLI | `~/.codex/skills/<name>/SKILL.md` | Yes | Yes |
| Claude Code | No user-scope skills | No | N/A |
| Cursor | No user-scope skills | No | N/A |
| Windsurf | `~/.codeium/windsurf/memories/global_rules.md` | Single file only | No |
| GitHub Copilot | VS Code settings.json | No | No |

## Architecture

```
User Scope (global)                          Project Scope (existing, unchanged)
─────────────────                            ──────────────────────────────────
Canonical:                                   Canonical:
  ~/.local/share/devflow/user-skills/          .agents/skills/<name>/SKILL.md
    <name>/SKILL.md
    skills.lock                              Lock file:
                                               .devflow/skills.lock
Agent symlinks:
  ~/.config/opencode/skills/<name> ->        Project symlinks:
    ~/.local/share/devflow/user-skills/<name>  .claude/skills/<name> ->
  ~/.codex/skills/<name> ->                      ../../.agents/skills/<name>
    ~/.local/share/devflow/user-skills/<name>

Inheritance (on workspace create):
  User-scope skills symlinked into new
  projects' .agents/skills/ + .claude/skills/
  automatically. Controlled by
  inherit_user_skills in .devflow.yml
  (default: true).
```

## Storage Layout

### User-scope directory

```
~/.local/share/devflow/user-skills/
  brainstorming/
    SKILL.md
  test-driven-development/
    SKILL.md
  skills.lock          # Same SkillLock format (version: 1)
```

Skills are stored as actual files (not symlinks to the cache). Unlike project-scope where many projects share cached content, user-scope has exactly one copy, so the cache indirection adds no value and would make agent symlinks point at hash-prefixed paths that change on update.

### Lock file format

Same `SkillLock` struct as project-scope:

```json
{
  "version": 1,
  "skills": {
    "brainstorming": {
      "source": { "type": "github", "owner": "obra", "repo": "agent-skills", "path": "skills/brainstorming" },
      "content_hash": "a1b2c3...",
      "installed_at": "2026-03-08T12:00:00Z"
    }
  }
}
```

## Rust Core Module

### New: `crates/devflow-core/src/skills/user_installer.rs`

Functions:

- `user_skills_dir() -> Result<PathBuf>` — `~/.local/share/devflow/user-skills/`
- `user_lock_path() -> Result<PathBuf>` — `user_skills_dir()/skills.lock`
- `install_user_skill(skill: &Skill, cache: &SkillCache) -> Result<()>`
  1. Write `SKILL.md` to `user_skills_dir/<name>/SKILL.md`
  2. Create agent symlinks (OpenCode, Codex) for dirs that exist
  3. Update user-scope `skills.lock`
- `remove_user_skill(name: &str) -> Result<()>`
  1. Remove `user_skills_dir/<name>/`
  2. Remove agent symlinks
  3. Update user-scope `skills.lock`
- `list_user_skills() -> Result<SkillLock>` — reads user-scope lock
- `show_user_skill(name: &str) -> Result<(InstalledSkill, String)>` — lock entry + content
- `check_user_updates(available: &[Skill]) -> Result<Vec<(String, String, String)>>`
- `inherit_into_project(project_dir: &Path) -> Result<Vec<String>>`
  - For each user-scope skill, symlink into `.agents/skills/<name>` and `.claude/skills/<name>`
  - Skips skills already present in project (project-scope takes precedence)
- `agent_symlink_targets() -> Vec<(String, PathBuf)>`
  - Returns `[("opencode", ~/.config/opencode/skills), ("codex", ~/.codex/skills)]`
  - Only includes dirs whose **parent** exists (agent is installed)

### Agent symlink logic

When installing a user-scope skill, create symlinks in agent dirs only if the agent's config directory already exists on the system:

- OpenCode: if `~/.config/opencode/` exists, create `~/.config/opencode/skills/<name>` -> `~/.local/share/devflow/user-skills/<name>`
- Codex CLI: if `~/.codex/` exists, create `~/.codex/skills/<name>` -> `~/.local/share/devflow/user-skills/<name>`

### Inheritance into projects

Called during `devflow switch -c` (workspace creation). For each user-scope skill:

1. Check if `.agents/skills/<name>` already exists in project (project-scope takes precedence)
2. If not, create symlink: `.agents/skills/<name>` -> `~/.local/share/devflow/user-skills/<name>`
3. Create Claude symlink: `.claude/skills/<name>` -> `../../.agents/skills/<name>`
4. Do NOT add to project lock file — these are user-scope, not project-scope

Controlled by `inherit_user_skills: bool` in `.devflow.yml` (default: true).

## Tauri Backend

New commands (no `project_path` parameter):

```rust
user_skill_list()              -> Vec<InstalledSkillInfo>
user_skill_install(identifier) -> Vec<String>
user_skill_remove(name)        -> ()
user_skill_update(name?)       -> Vec<String>
user_skill_show(name)          -> SkillDetail
user_skill_check_updates()     -> Vec<String>
```

## GUI

### New: Global Skills page (`/skills`)

- Re-add "Skills" sidebar link under Tools section
- New `SkillsPage.tsx` — same two-panel layout as `ProjectSkillsTab`
- Calls `user_skill_*` commands instead of project-scoped ones
- Shows agent symlink status badges (OpenCode, Codex)
- Reuses existing components: `ConfirmDialog`, `<Markdown>`, CSS classes

### Existing: ProjectSkillsTab

- Show "user" badge on skills inherited from user-scope (detect via symlink target pointing at user-skills dir)

## CLI

Add `--user` flag to existing skill subcommands:

```
devflow skill list [--user]
devflow skill install [--user] <identifier>
devflow skill remove [--user] <name>
devflow skill update [--user] [name]
devflow skill show [--user] <name>
```

`--user` switches to user-scope operations. No project directory needed.

## TUI

Add a scope toggle (User / Project) to the Skills tab header. When in User scope, calls user-scope functions.

## Non-goals

- No migration from existing user-scope agent skills (e.g., manually installed OpenCode skills)
- No Windsurf/Copilot agent symlinks (format incompatible)
- No per-project override of individual inherited skills (project-scope install simply takes precedence)

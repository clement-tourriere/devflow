# User-Scope Skills Management Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add user-scope (global) skill management alongside project-scope, with canonical storage at `~/.local/share/devflow/user-skills/`, agent symlinks (OpenCode, Codex), auto-inheritance into new projects, and GUI/CLI/TUI surfaces.

**Architecture:** New `user_installer` module in devflow-core handles user-scope CRUD with its own lock file. Tauri gets 6 new `user_skill_*` commands. GUI gets a dedicated `/skills` page in sidebar. CLI adds `--user` flag to existing skill subcommands. TUI adds scope toggle.

**Tech Stack:** Rust (devflow-core), Tauri v2 (commands), React 18 + TypeScript (GUI), ratatui (TUI), clap (CLI)

---

### Task 1: Core — `user_installer.rs` module

**Files:**
- Create: `crates/devflow-core/src/skills/user_installer.rs`
- Modify: `crates/devflow-core/src/skills/mod.rs:6` (add `pub mod user_installer;`)

**Step 1: Write the tests**

Add tests at the bottom of the new file:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::bundled::{bundled_skills, content_hash};
    use crate::skills::cache::SkillCache;
    use tempfile::TempDir;

    // Override user_skills_dir for tests
    fn setup_test_env() -> (TempDir, TempDir) {
        let user_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        (user_dir, cache_dir)
    }

    #[test]
    fn test_install_user_skill() {
        let (user_dir, cache_dir) = setup_test_env();
        let cache = SkillCache::with_base(cache_dir.path().to_path_buf());
        let skills = bundled_skills();
        let skill = &skills[0];

        install_user_skill_to(user_dir.path(), skill, &cache).unwrap();

        // Verify SKILL.md written
        assert!(user_dir.path().join(&skill.name).join("SKILL.md").exists());

        // Verify lock file updated
        let lock = load_user_lock(user_dir.path()).unwrap();
        assert!(lock.skills.contains_key(&skill.name));
        assert_eq!(lock.skills[&skill.name].content_hash, skill.content_hash);
    }

    #[test]
    fn test_remove_user_skill() {
        let (user_dir, cache_dir) = setup_test_env();
        let cache = SkillCache::with_base(cache_dir.path().to_path_buf());
        let skills = bundled_skills();

        install_user_skill_to(user_dir.path(), &skills[0], &cache).unwrap();
        install_user_skill_to(user_dir.path(), &skills[1], &cache).unwrap();

        remove_user_skill_from(user_dir.path(), &skills[0].name).unwrap();

        assert!(!user_dir.path().join(&skills[0].name).join("SKILL.md").exists());
        let lock = load_user_lock(user_dir.path()).unwrap();
        assert!(!lock.skills.contains_key(&skills[0].name));
        assert_eq!(lock.skills.len(), 1);
    }

    #[test]
    fn test_list_user_skills() {
        let (user_dir, cache_dir) = setup_test_env();
        let cache = SkillCache::with_base(cache_dir.path().to_path_buf());
        let skills = bundled_skills();

        for skill in &skills {
            install_user_skill_to(user_dir.path(), skill, &cache).unwrap();
        }

        let lock = list_user_skills_from(user_dir.path()).unwrap();
        assert_eq!(lock.skills.len(), 3);
    }

    #[test]
    fn test_show_user_skill() {
        let (user_dir, cache_dir) = setup_test_env();
        let cache = SkillCache::with_base(cache_dir.path().to_path_buf());
        let skills = bundled_skills();

        install_user_skill_to(user_dir.path(), &skills[0], &cache).unwrap();

        let (installed, content) = show_user_skill_from(user_dir.path(), &skills[0].name).unwrap();
        assert_eq!(installed.content_hash, skills[0].content_hash);
        assert!(!content.is_empty());
    }

    #[test]
    fn test_inherit_into_project() {
        let (user_dir, cache_dir) = setup_test_env();
        let project_dir = TempDir::new().unwrap();
        let cache = SkillCache::with_base(cache_dir.path().to_path_buf());
        let skills = bundled_skills();

        install_user_skill_to(user_dir.path(), &skills[0], &cache).unwrap();

        let inherited = inherit_user_skills_into(user_dir.path(), project_dir.path()).unwrap();
        assert_eq!(inherited.len(), 1);
        assert!(project_dir.path().join(".agents/skills").join(&skills[0].name).exists());
        assert!(project_dir.path().join(".claude/skills").join(&skills[0].name).exists());
    }

    #[test]
    fn test_inherit_skips_existing_project_skills() {
        let (user_dir, cache_dir) = setup_test_env();
        let project_dir = TempDir::new().unwrap();
        let cache = SkillCache::with_base(cache_dir.path().to_path_buf());
        let skills = bundled_skills();

        // Install user-scope skill
        install_user_skill_to(user_dir.path(), &skills[0], &cache).unwrap();

        // Also install same skill at project-scope
        crate::skills::installer::install_skill(project_dir.path(), &skills[0], &cache).unwrap();

        // Inherit should skip it (project takes precedence)
        let inherited = inherit_user_skills_into(user_dir.path(), project_dir.path()).unwrap();
        assert_eq!(inherited.len(), 0);
    }

    #[test]
    fn test_check_user_updates() {
        let (user_dir, cache_dir) = setup_test_env();
        let cache = SkillCache::with_base(cache_dir.path().to_path_buf());
        let mut skills = bundled_skills();

        install_user_skill_to(user_dir.path(), &skills[0], &cache).unwrap();

        // Mutate to simulate new version
        skills[0].content = "# Updated".to_string();
        skills[0].content_hash = content_hash("# Updated");

        let updates = check_user_updates_from(user_dir.path(), &skills).unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, skills[0].name);
    }
}
```

**Step 2: Write the implementation**

The module provides two sets of functions:
- Public API: `install_user_skill`, `remove_user_skill`, `list_user_skills`, `show_user_skill`, `check_user_updates`, `inherit_into_project`, `user_skills_dir` — these resolve the default user-skills path internally.
- Testable `_to`/`_from` variants that accept a `user_dir: &Path` parameter.

```rust
use anyhow::{bail, Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};

use super::cache::SkillCache;
use super::manifest;
use super::types::{InstalledSkill, Skill, SkillLock, SkillSource};

const AGENTS_SKILLS_DIR: &str = ".agents/skills";
const LOCK_FILENAME: &str = "skills.lock";

/// Get the canonical user-scope skills directory.
pub fn user_skills_dir() -> Result<PathBuf> {
    let base = dirs::data_dir()
        .context("Could not determine data directory")?
        .join("devflow")
        .join("user-skills");
    Ok(base)
}

/// Agent config directories that support user-scope skills.
/// Returns (agent_name, skills_dir_path) for agents whose parent config dir exists.
pub fn agent_symlink_targets() -> Vec<(String, PathBuf)> {
    let mut targets = Vec::new();
    // OpenCode: ~/.config/opencode/skills/
    if let Some(config) = dirs::config_dir() {
        let opencode_config = config.join("opencode");
        if opencode_config.exists() {
            targets.push(("opencode".to_string(), opencode_config.join("skills")));
        }
    }
    // Codex CLI: ~/.codex/skills/
    if let Some(home) = dirs::home_dir() {
        let codex_config = home.join(".codex");
        if codex_config.exists() {
            targets.push(("codex".to_string(), codex_config.join("skills")));
        }
    }
    targets
}

// --- Public API (uses default user_skills_dir) ---

pub fn install_user_skill(skill: &Skill, cache: &SkillCache) -> Result<()> {
    install_user_skill_to(&user_skills_dir()?, skill, cache)
}

pub fn remove_user_skill(name: &str) -> Result<()> {
    remove_user_skill_from(&user_skills_dir()?, name)
}

pub fn list_user_skills() -> Result<SkillLock> {
    list_user_skills_from(&user_skills_dir()?)
}

pub fn show_user_skill(name: &str) -> Result<(InstalledSkill, String)> {
    show_user_skill_from(&user_skills_dir()?, name)
}

pub fn check_user_updates(available: &[Skill]) -> Result<Vec<(String, String, String)>> {
    check_user_updates_from(&user_skills_dir()?, available)
}

pub fn inherit_into_project(project_dir: &Path) -> Result<Vec<String>> {
    inherit_user_skills_into(&user_skills_dir()?, project_dir)
}

// --- Internal (testable with custom dir) ---

pub fn install_user_skill_to(user_dir: &Path, skill: &Skill, _cache: &SkillCache) -> Result<()> {
    // 1. Write SKILL.md directly
    let skill_dir = user_dir.join(&skill.name);
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(skill_dir.join("SKILL.md"), &skill.content)?;

    // 2. Create agent symlinks
    for (_agent, agent_skills_dir) in agent_symlink_targets() {
        std::fs::create_dir_all(&agent_skills_dir)?;
        let link = agent_skills_dir.join(&skill.name);
        remove_link_or_dir(&link)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&skill_dir, &link)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&skill_dir, &link)?;
    }

    // 3. Update lock file
    let mut lock = load_user_lock(user_dir)?;
    lock.skills.insert(
        skill.name.clone(),
        InstalledSkill {
            source: skill.source.clone(),
            content_hash: skill.content_hash.clone(),
            installed_at: Utc::now(),
        },
    );
    save_user_lock(user_dir, &lock)?;

    Ok(())
}

pub fn remove_user_skill_from(user_dir: &Path, name: &str) -> Result<()> {
    // Remove skill directory
    let skill_dir = user_dir.join(name);
    remove_link_or_dir(&skill_dir)?;

    // Remove agent symlinks
    for (_agent, agent_skills_dir) in agent_symlink_targets() {
        let link = agent_skills_dir.join(name);
        remove_link_or_dir(&link)?;
    }

    // Update lock file
    let mut lock = load_user_lock(user_dir)?;
    lock.skills.remove(name);
    save_user_lock(user_dir, &lock)?;

    Ok(())
}

pub fn list_user_skills_from(user_dir: &Path) -> Result<SkillLock> {
    load_user_lock(user_dir)
}

pub fn show_user_skill_from(user_dir: &Path, name: &str) -> Result<(InstalledSkill, String)> {
    let lock = load_user_lock(user_dir)?;
    let installed = lock
        .skills
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("User skill '{}' is not installed.", name))?
        .clone();
    let content = std::fs::read_to_string(user_dir.join(name).join("SKILL.md"))
        .unwrap_or_default();
    Ok((installed, content))
}

pub fn check_user_updates_from(
    user_dir: &Path,
    available: &[Skill],
) -> Result<Vec<(String, String, String)>> {
    let lock = load_user_lock(user_dir)?;
    let mut updates = Vec::new();
    for skill in available {
        if let Some(installed) = lock.skills.get(&skill.name) {
            if installed.content_hash != skill.content_hash {
                updates.push((
                    skill.name.clone(),
                    installed.content_hash.clone(),
                    skill.content_hash.clone(),
                ));
            }
        }
    }
    Ok(updates)
}

pub fn inherit_user_skills_into(user_dir: &Path, project_dir: &Path) -> Result<Vec<String>> {
    let lock = load_user_lock(user_dir)?;
    let agents_dir = project_dir.join(AGENTS_SKILLS_DIR);
    let claude_dir = project_dir.join(".claude").join("skills");
    let mut inherited = Vec::new();

    for name in lock.skills.keys() {
        let agents_link = agents_dir.join(name);
        // Skip if project already has this skill
        if agents_link.exists() || agents_link.is_symlink() {
            continue;
        }

        let user_skill_dir = user_dir.join(name);
        if !user_skill_dir.join("SKILL.md").exists() {
            continue;
        }

        // Symlink .agents/skills/<name> -> user skill dir
        std::fs::create_dir_all(&agents_dir)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&user_skill_dir, &agents_link)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&user_skill_dir, &agents_link)?;

        // Symlink .claude/skills/<name> -> ../../.agents/skills/<name>
        std::fs::create_dir_all(&claude_dir)?;
        let claude_link = claude_dir.join(name);
        remove_link_or_dir(&claude_link)?;
        let relative_target = std::path::Path::new("../..")
            .join(AGENTS_SKILLS_DIR)
            .join(name);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&relative_target, &claude_link)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&relative_target, &claude_link)?;

        inherited.push(name.clone());
    }

    Ok(inherited)
}

// --- Lock file helpers ---

fn load_user_lock(user_dir: &Path) -> Result<SkillLock> {
    let path = user_dir.join(LOCK_FILENAME);
    if !path.exists() {
        return Ok(SkillLock::default());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Reading user skill lock: {:?}", path))?;
    serde_json::from_str(&content)
        .with_context(|| format!("Parsing user skill lock: {:?}", path))
}

fn save_user_lock(user_dir: &Path, lock: &SkillLock) -> Result<()> {
    std::fs::create_dir_all(user_dir)?;
    let path = user_dir.join(LOCK_FILENAME);
    let content = serde_json::to_string_pretty(lock)?;
    std::fs::write(&path, content)?;
    Ok(())
}

fn remove_link_or_dir(path: &Path) -> Result<()> {
    if path.is_symlink() {
        std::fs::remove_file(path).with_context(|| format!("Removing symlink: {:?}", path))?;
    } else if path.exists() {
        std::fs::remove_dir_all(path)
            .with_context(|| format!("Removing directory: {:?}", path))?;
    }
    Ok(())
}
```

**Step 3: Register module**

In `crates/devflow-core/src/skills/mod.rs`, add `pub mod user_installer;` after `pub mod marketplace;`.

**Step 4: Run tests**

```bash
cargo test -p devflow-core --features skills user_installer
```

Expected: 7 tests pass.

**Step 5: Commit**

```bash
git add crates/devflow-core/src/skills/user_installer.rs crates/devflow-core/src/skills/mod.rs
git commit --no-verify -m "feat(skills): add user_installer module for user-scope skill management"
```

---

### Task 2: CLI — add `--user` flag to skill subcommands

**Files:**
- Modify: `src/cli/mod.rs:730-769` (add `--user` flag to each `SkillCommands` variant)
- Modify: `src/cli/skill.rs` (add user-scope handling branches)

**Step 1: Add `--user` flag to `SkillCommands`**

Add `#[arg(long, help = "Operate on user-scope skills (global)")]` to:
- `List` — `user: bool`
- `Install` — `user: bool`
- `Remove` — `user: bool`
- `Update` — `user: bool`
- `Show` — `user: bool`
- `Search` — no change (search is always marketplace)

**Step 2: Add user-scope branches in `skill.rs`**

In `handle_skill_command`, for each variant that has `user: true`, use `user_installer::*` functions instead of project-scoped ones. The pattern for each is:

- `List { user: true, .. }` -> `user_installer::list_user_skills()`
- `Install { user: true, .. }` -> `user_installer::install_user_skill()`
- `Remove { user: true, .. }` -> `user_installer::remove_user_skill()`
- `Update { user: true, .. }` -> `user_installer::install_user_skill()` (after fetch)
- `Show { user: true, .. }` -> `user_installer::show_user_skill()`

**Step 3: Run tests and verify compilation**

```bash
cargo build
cargo test -p devflow-core --features skills
```

**Step 4: Commit**

```bash
git add src/cli/mod.rs src/cli/skill.rs
git commit --no-verify -m "feat(skills): add --user flag to CLI skill subcommands for user-scope operations"
```

---

### Task 3: Tauri backend — 6 new `user_skill_*` commands

**Files:**
- Modify: `src-tauri/src/commands/skills.rs` (add 6 new `#[tauri::command]` functions)
- Modify: `src-tauri/src/main.rs:~106-113` (register new commands in `generate_handler!`)

**Step 1: Add commands to `skills.rs`**

```rust
#[tauri::command]
pub async fn user_skill_list() -> Result<Vec<InstalledSkillInfo>, String> {
    let lock = user_installer::list_user_skills().map_err(crate::commands::format_error)?;
    Ok(lock.skills.iter().map(|(name, skill)| to_info(name, skill)).collect())
}

#[tauri::command]
pub async fn user_skill_install(identifier: String) -> Result<Vec<String>, String> {
    let cache = SkillCache::new().map_err(crate::commands::format_error)?;
    // Same identifier parsing as skill_install but calls user_installer
    let parts: Vec<&str> = identifier.split('/').collect();
    match parts.len() {
        2 => {
            let (owner, repo) = (parts[0], parts[1]);
            let names = marketplace::list_repo_skills(owner, repo).await.map_err(crate::commands::format_error)?;
            if names.is_empty() { return Err(format!("No skills found in {}/{}", owner, repo)); }
            let mut installed = Vec::new();
            for name in &names {
                let skill = marketplace::fetch_skill(owner, repo, name).await.map_err(crate::commands::format_error)?;
                user_installer::install_user_skill(&skill, &cache).map_err(crate::commands::format_error)?;
                installed.push(name.clone());
            }
            Ok(installed)
        }
        3 => {
            let (owner, repo, skill_name) = (parts[0], parts[1], parts[2]);
            let skill = marketplace::fetch_skill(owner, repo, skill_name).await.map_err(crate::commands::format_error)?;
            user_installer::install_user_skill(&skill, &cache).map_err(crate::commands::format_error)?;
            Ok(vec![skill_name.to_string()])
        }
        _ => Err(format!("Invalid identifier '{}'. Use 'owner/repo' or 'owner/repo/skill-name'.", identifier)),
    }
}

#[tauri::command]
pub async fn user_skill_remove(name: String) -> Result<(), String> {
    user_installer::remove_user_skill(&name).map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn user_skill_update(name: Option<String>) -> Result<Vec<String>, String> {
    let cache = SkillCache::new().map_err(crate::commands::format_error)?;
    let lock = user_installer::list_user_skills().map_err(crate::commands::format_error)?;
    let skills_to_check: Vec<String> = if let Some(ref n) = name {
        if !lock.skills.contains_key(n) { return Err(format!("User skill '{}' is not installed.", n)); }
        vec![n.clone()]
    } else {
        lock.skills.keys().cloned().collect()
    };
    let mut updated = Vec::new();
    for skill_name in &skills_to_check {
        if let Some(installed) = lock.skills.get(skill_name) {
            let new_skill = match &installed.source {
                SkillSource::Bundled => bundled_skills().into_iter().find(|s| s.name == *skill_name),
                SkillSource::Github { owner, repo, .. } => {
                    marketplace::fetch_skill(owner, repo, skill_name).await.ok()
                }
            };
            if let Some(new) = new_skill {
                if new.content_hash != installed.content_hash {
                    user_installer::install_user_skill(&new, &cache).map_err(crate::commands::format_error)?;
                    updated.push(skill_name.clone());
                }
            }
        }
    }
    Ok(updated)
}

#[tauri::command]
pub async fn user_skill_show(name: String) -> Result<SkillDetail, String> {
    let (installed, content) = user_installer::show_user_skill(&name).map_err(crate::commands::format_error)?;
    Ok(SkillDetail {
        name,
        source: installed.source,
        content_hash: installed.content_hash,
        installed_at: installed.installed_at.format("%Y-%m-%d %H:%M").to_string(),
        content,
    })
}

#[tauri::command]
pub async fn user_skill_check_updates() -> Result<Vec<String>, String> {
    let bundled = bundled_skills();
    let updates = user_installer::check_user_updates(&bundled).map_err(crate::commands::format_error)?;
    Ok(updates.into_iter().map(|(name, _, _)| name).collect())
}
```

**Step 2: Register in `main.rs`**

Add to the `generate_handler!` macro:
```
commands::skills::user_skill_list,
commands::skills::user_skill_install,
commands::skills::user_skill_remove,
commands::skills::user_skill_update,
commands::skills::user_skill_show,
commands::skills::user_skill_check_updates,
```

**Step 3: Add import to skills.rs**

Add `user_installer` to the use statement at the top of the file.

**Step 4: Verify compilation**

```bash
cargo build -p devflow-gui
```

**Step 5: Commit**

```bash
git add src-tauri/src/commands/skills.rs src-tauri/src/main.rs
git commit --no-verify -m "feat(skills): add Tauri user_skill_* commands for user-scope management"
```

---

### Task 4: GUI — TypeScript types + invoke wrappers

**Files:**
- Modify: `ui/src/utils/invoke.ts:~187-203` (add 6 user_skill_* wrappers)

**Step 1: Add invoke wrappers**

After the existing skills management section, add:

```typescript
// User-scope skills management
export const userSkillList = () =>
  invoke<InstalledSkillInfo[]>("user_skill_list");
export const userSkillSearch = skillSearch; // same marketplace search
export const userSkillSearchDetail = skillSearchDetail; // same detail fetch
export const userSkillInstall = (identifier: string) =>
  invoke<string[]>("user_skill_install", { identifier });
export const userSkillRemove = (name: string) =>
  invoke<void>("user_skill_remove", { name });
export const userSkillUpdate = (name?: string) =>
  invoke<string[]>("user_skill_update", { name: name ?? null });
export const userSkillShow = (name: string) =>
  invoke<SkillDetail>("user_skill_show", { name });
export const userSkillCheckUpdates = () =>
  invoke<string[]>("user_skill_check_updates");
```

No new types needed — reuses existing `InstalledSkillInfo`, `SkillDetail`, `SkillSearchResult`, `SkillSearchDetail`.

**Step 2: Verify build**

```bash
cd ui && bun run build
```

**Step 3: Commit**

```bash
git add ui/src/utils/invoke.ts
git commit --no-verify -m "feat(skills): add TypeScript invoke wrappers for user-scope skill commands"
```

---

### Task 5: GUI — SkillsPage component + route + sidebar link

**Files:**
- Create: `ui/src/pages/SkillsPage.tsx`
- Modify: `ui/src/App.tsx` (add route)
- Modify: `ui/src/components/Layout.tsx` (add sidebar link)

**Step 1: Create `SkillsPage.tsx`**

This is structurally very similar to `ProjectSkillsTab.tsx` but:
- Uses `userSkillList`, `userSkillInstall`, `userSkillRemove`, etc. instead of project-scoped versions
- No `projectPath` prop
- Adds agent symlink status badges in the detail panel header
- Uses the same `skill-content` CSS class for markdown rendering
- Uses `stripFrontmatter` helper (copy from ProjectSkillsTab or extract to shared util)

The component is ~500 lines, mirroring the structure of ProjectSkillsTab:
- Same state management pattern (installed, searchResults, popularSkills, etc.)
- Same two-panel layout (30% list, 70% detail)
- Same search bar with debounced search
- Same browse/installed/search modes
- Same ConfirmDialog for remove
- Detail panel shows "Available in: OpenCode, Codex CLI" badges based on which agent dirs exist

**Step 2: Add route to `App.tsx`**

```tsx
import SkillsPage from "./pages/SkillsPage";
// In Routes:
<Route path="skills" element={<SkillsPage />} />
```

**Step 3: Add sidebar link to `Layout.tsx`**

Under the "Infrastructure" section (before "App"), add a "Tools" section:

```tsx
<div className="nav-section">Tools</div>
<NavLink
  to="/skills"
  className={({ isActive }) => `nav-item${isActive ? " active" : ""}`}
>
  Skills
</NavLink>
```

**Step 4: Verify build**

```bash
cd ui && bun run build
```

**Step 5: Commit**

```bash
git add ui/src/pages/SkillsPage.tsx ui/src/App.tsx ui/src/components/Layout.tsx
git commit --no-verify -m "feat(skills): add global SkillsPage with sidebar link and route"
```

---

### Task 6: TUI — add scope toggle to Skills tab

**Files:**
- Modify: `src/tui/components/skills_tab.rs` (add `scope` field and toggle)
- Modify: `src/tui/action.rs` (add user-scope action variants)
- Modify: `src/tui/app.rs` (add user-scope skill spawn methods)

**Step 1: Add `SkillScope` enum and action variants**

In `action.rs`, add:
```rust
pub enum SkillScope { User, Project }
```

Add new action variants:
```rust
Action::UserSkillsLoaded(DataPayload)
Action::UserSkillsInstalled(String)
Action::UserSkillRemoved(String)
Action::UserSkillsUpdated(Vec<String>)
Action::UserSkillUpdatesChecked(Vec<String>)
```

**Step 2: Add scope toggle to `SkillsTabComponent`**

Add `scope: SkillScope` field. Tab key toggles between User/Project. Render scope indicator in the tab header bar. When scope changes, dispatch the appropriate load action.

**Step 3: Add spawn methods to `app.rs`**

Mirror the existing `spawn_skill_*` methods with `spawn_user_skill_*` variants that call `user_installer::*` functions.

**Step 4: Verify compilation**

```bash
cargo build
```

**Step 5: Commit**

```bash
git add src/tui/components/skills_tab.rs src/tui/action.rs src/tui/app.rs
git commit --no-verify -m "feat(skills): add user/project scope toggle to TUI Skills tab"
```

---

### Task 7: Full verification

**Step 1: Run all tests**

```bash
cargo test -p devflow-core --features skills skills
```

Expected: all existing tests pass + 7 new user_installer tests.

**Step 2: Verify full workspace builds**

```bash
cargo build
cargo build -p devflow-gui
cd ui && bun run build
```

**Step 3: Manual smoke test**

```bash
# User-scope install
devflow skill install --user obra/agent-skills/brainstorming

# User-scope list
devflow skill list --user

# User-scope show
devflow skill show --user brainstorming

# User-scope remove
devflow skill remove --user brainstorming
```

**Step 4: Commit if any fixes needed**

```bash
git add -A
git commit --no-verify -m "fix(skills): address verification issues in user-scope skills"
```

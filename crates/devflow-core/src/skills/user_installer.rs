//! User-scope skill installer.
//!
//! Manages skills installed at the user level (`~/.local/share/devflow/user-skills/`),
//! with symlinks into agent config directories that support user-scope skills
//! (OpenCode, Codex CLI).

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;
use std::path::{Path, PathBuf};

use super::bundled::content_hash;
use super::cache::SkillCache;
use super::marketplace::parse_frontmatter;
use super::types::{InstalledSkill, Skill, SkillLock};

const CLAUDE_SKILLS_DIR: &str = ".claude/skills";
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
/// Returns `(agent_name, skills_dir_path)` for agents whose parent config dir exists.
pub fn agent_symlink_targets() -> Vec<(String, PathBuf)> {
    let mut targets = Vec::new();

    if let Some(home) = dirs::home_dir() {
        // OpenCode: ~/.config/opencode/skills/
        // Note: OpenCode uses XDG-style ~/.config/ on all platforms, NOT the
        // platform-native config dir (which on macOS is ~/Library/Application Support/).
        let opencode_config = home.join(".config").join("opencode");
        if opencode_config.exists() {
            targets.push(("opencode".to_string(), opencode_config.join("skills")));
        }

        // Codex CLI: ~/.codex/skills/
        let codex_config = home.join(".codex");
        if codex_config.exists() {
            targets.push(("codex".to_string(), codex_config.join("skills")));
        }
    }

    targets
}

// ── Public API (uses default user_skills_dir) ──────────────────────────────

/// Install a skill at the user scope.
pub fn install_user_skill(skill: &Skill, cache: &SkillCache) -> Result<()> {
    let user_dir = user_skills_dir()?;
    install_user_skill_to(&user_dir, skill, cache)?;
    sync_agent_symlinks_for(&user_dir, &skill.name)?;
    Ok(())
}

/// Remove a skill from the user scope.
pub fn remove_user_skill(name: &str) -> Result<()> {
    let user_dir = user_skills_dir()?;
    remove_user_skill_from(&user_dir, name)?;
    remove_agent_symlinks_for(name)?;
    Ok(())
}

/// List all user-scope installed skills.
pub fn list_user_skills() -> Result<SkillLock> {
    list_user_skills_from(&user_skills_dir()?)
}

/// Show details of a user-scope installed skill.
pub fn show_user_skill(name: &str) -> Result<(InstalledSkill, String)> {
    show_user_skill_from(&user_skills_dir()?, name)
}

/// Check which user-scope skills have updates available.
pub fn check_user_updates(available: &[Skill]) -> Result<Vec<(String, String, String)>> {
    check_user_updates_from(&user_skills_dir()?, available)
}

/// Symlink user-scope skills into a project directory.
///
/// Skips skills already present in the project (project-scope takes precedence).
/// Does NOT write to the project lock file — these remain user-scope.
pub fn inherit_into_project(project_dir: &Path) -> Result<Vec<String>> {
    inherit_user_skills_into(&user_skills_dir()?, project_dir)
}

// ── External skills discovery ──────────────────────────────────────────────

/// An external skill discovered on disk in an agent's config directory,
/// NOT managed by devflow.
#[derive(Debug, Clone, Serialize)]
pub struct ExternalSkill {
    /// Skill name (from frontmatter, or directory name as fallback).
    pub name: String,
    /// Description from SKILL.md frontmatter, if available.
    pub description: String,
    /// Full SKILL.md content.
    pub content: String,
    /// SHA-256 hex digest of SKILL.md content.
    pub content_hash: String,
    /// Which agent this skill was found in (e.g. "opencode", "codex").
    pub agent: String,
    /// Filesystem path to the skill directory.
    pub path: PathBuf,
}

/// Discover external skills from agent config directories.
///
/// Scans each agent's skill directory for skills that are NOT managed by devflow
/// (i.e. not present in the user-scope `skills.lock`). Follows symlinks and
/// expands nested sub-skills (e.g. a `superpowers` symlink containing 14 sub-skill
/// directories each with their own `SKILL.md`).
///
/// Directories named `.system` are excluded (Codex internal directory).
/// Empty directories (no `SKILL.md`) are skipped.
/// Skills already present in devflow's user-scope lock file are excluded.
pub fn discover_external_skills() -> Result<Vec<ExternalSkill>> {
    let user_dir = user_skills_dir().unwrap_or_default();
    let lock = load_user_lock(&user_dir).unwrap_or_default();
    discover_external_skills_with(&lock)
}

/// Testable variant that accepts a pre-loaded lock.
pub fn discover_external_skills_with(lock: &SkillLock) -> Result<Vec<ExternalSkill>> {
    let targets = agent_symlink_targets();
    let managed_names: std::collections::HashSet<&str> =
        lock.skills.keys().map(|s| s.as_str()).collect();

    let mut found: Vec<ExternalSkill> = Vec::new();
    // Track (agent, name) pairs to avoid duplicates within the same agent
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

    for (agent_name, agent_skills_dir) in &targets {
        if !agent_skills_dir.exists() {
            continue;
        }

        let entries = match std::fs::read_dir(agent_skills_dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let entry_name = entry.file_name().to_string_lossy().to_string();

            // Skip .system (Codex internal) and hidden directories
            if entry_name.starts_with('.') {
                continue;
            }

            let entry_path = entry.path();

            // Resolve symlinks to get the real path
            let resolved = match std::fs::canonicalize(&entry_path) {
                Ok(p) => p,
                Err(_) => continue, // Broken symlink
            };

            // Check if this is a directory
            if !resolved.is_dir() {
                continue;
            }

            // Check if SKILL.md exists directly in this directory
            let skill_md = resolved.join("SKILL.md");
            if skill_md.exists() {
                // Single skill directory
                if let Some(ext) =
                    try_parse_external_skill(&skill_md, &entry_name, agent_name, &entry_path)
                {
                    // Skip if managed by devflow
                    if managed_names.contains(ext.name.as_str()) {
                        continue;
                    }
                    let key = (agent_name.clone(), ext.name.clone());
                    if seen.insert(key) {
                        found.push(ext);
                    }
                }
            } else {
                // No SKILL.md directly — check for sub-skills (nested directories)
                // This handles cases like superpowers/ containing brainstorming/, debugging/, etc.
                expand_sub_skills(&resolved, agent_name, &managed_names, &mut seen, &mut found);
            }
        }
    }

    // Sort by name for deterministic output
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

/// Expand a directory containing nested sub-skill directories.
fn expand_sub_skills(
    parent_dir: &Path,
    agent_name: &str,
    managed_names: &std::collections::HashSet<&str>,
    seen: &mut std::collections::HashSet<(String, String)>,
    found: &mut Vec<ExternalSkill>,
) {
    let entries = match std::fs::read_dir(parent_dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let entry_name = entry.file_name().to_string_lossy().to_string();

        // Skip hidden directories
        if entry_name.starts_with('.') {
            continue;
        }

        let entry_path = entry.path();
        if !entry_path.is_dir() {
            continue;
        }

        let skill_md = entry_path.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }

        if let Some(ext) = try_parse_external_skill(&skill_md, &entry_name, agent_name, &entry_path)
        {
            if managed_names.contains(ext.name.as_str()) {
                continue;
            }
            let key = (agent_name.to_string(), ext.name.clone());
            if seen.insert(key) {
                found.push(ext);
            }
        }
    }
}

/// Try to parse an ExternalSkill from a SKILL.md file.
fn try_parse_external_skill(
    skill_md: &Path,
    dir_name: &str,
    agent_name: &str,
    display_path: &Path,
) -> Option<ExternalSkill> {
    let content = std::fs::read_to_string(skill_md).ok()?;
    let hash = content_hash(&content);
    let frontmatter = parse_frontmatter(&content);

    let name = frontmatter
        .as_ref()
        .map(|f| f.name.clone())
        .unwrap_or_else(|| dir_name.to_string());
    let description = frontmatter
        .as_ref()
        .and_then(|f| f.description.clone())
        .unwrap_or_default();

    Some(ExternalSkill {
        name,
        description,
        content,
        content_hash: hash,
        agent: agent_name.to_string(),
        path: display_path.to_path_buf(),
    })
}

// ── Internal (testable with custom dir) ────────────────────────────────────

/// Install a skill into a specific user-skills directory.
pub fn install_user_skill_to(user_dir: &Path, skill: &Skill, _cache: &SkillCache) -> Result<()> {
    // 1. Write SKILL.md directly (no cache indirection for user-scope)
    let skill_dir = user_dir.join(&skill.name);
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(skill_dir.join("SKILL.md"), &skill.content)?;

    // 2. Update lock file
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

/// Remove a skill from a specific user-skills directory.
pub fn remove_user_skill_from(user_dir: &Path, name: &str) -> Result<()> {
    // Remove skill directory
    let skill_dir = user_dir.join(name);
    remove_link_or_dir(&skill_dir)?;

    // Update lock file
    let mut lock = load_user_lock(user_dir)?;
    lock.skills.remove(name);
    save_user_lock(user_dir, &lock)?;

    Ok(())
}

/// List skills from a specific user-skills directory.
pub fn list_user_skills_from(user_dir: &Path) -> Result<SkillLock> {
    load_user_lock(user_dir)
}

/// Show a skill from a specific user-skills directory.
pub fn show_user_skill_from(user_dir: &Path, name: &str) -> Result<(InstalledSkill, String)> {
    let lock = load_user_lock(user_dir)?;
    let installed = lock
        .skills
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("User skill '{}' is not installed.", name))?
        .clone();
    let content = std::fs::read_to_string(user_dir.join(name).join("SKILL.md")).unwrap_or_default();
    Ok((installed, content))
}

/// Check for updates in a specific user-skills directory.
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

/// Copy user-scope skills from a specific dir into a project.
pub fn inherit_user_skills_into(user_dir: &Path, project_dir: &Path) -> Result<Vec<String>> {
    let lock = load_user_lock(user_dir)?;
    let claude_dir = project_dir.join(CLAUDE_SKILLS_DIR);
    let mut inherited = Vec::new();

    for name in lock.skills.keys() {
        let skill_dir = claude_dir.join(name);
        // Skip if project already has this skill (project-scope takes precedence)
        if skill_dir.exists() || skill_dir.is_symlink() {
            continue;
        }

        let user_skill_dir = user_dir.join(name);
        let user_skill_file = user_skill_dir.join("SKILL.md");
        if !user_skill_file.exists() {
            continue;
        }

        // Write .claude/skills/<name>/SKILL.md directly
        std::fs::create_dir_all(&skill_dir)?;
        let content = std::fs::read_to_string(&user_skill_file)
            .with_context(|| format!("Reading user skill: {:?}", user_skill_file))?;
        std::fs::write(skill_dir.join("SKILL.md"), content)?;

        inherited.push(name.clone());
    }

    Ok(inherited)
}

// ── Agent symlink helpers (only called from public API, not testable variants) ─

/// Create symlinks in agent config directories for a single skill.
fn sync_agent_symlinks_for(user_dir: &Path, skill_name: &str) -> Result<()> {
    let skill_dir = user_dir.join(skill_name);
    for (_agent, agent_skills_dir) in agent_symlink_targets() {
        std::fs::create_dir_all(&agent_skills_dir)?;
        let link = agent_skills_dir.join(skill_name);
        remove_link_or_dir(&link)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&skill_dir, &link)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&skill_dir, &link)?;
    }
    Ok(())
}

/// Remove symlinks in agent config directories for a single skill.
fn remove_agent_symlinks_for(skill_name: &str) -> Result<()> {
    for (_agent, agent_skills_dir) in agent_symlink_targets() {
        let link = agent_skills_dir.join(skill_name);
        remove_link_or_dir(&link)?;
    }
    Ok(())
}

// ── Lock file helpers ──────────────────────────────────────────────────────

fn load_user_lock(user_dir: &Path) -> Result<SkillLock> {
    let path = user_dir.join(LOCK_FILENAME);
    if !path.exists() {
        return Ok(SkillLock::default());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Reading user skill lock: {:?}", path))?;
    serde_json::from_str(&content).with_context(|| format!("Parsing user skill lock: {:?}", path))
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
        std::fs::remove_dir_all(path).with_context(|| format!("Removing directory: {:?}", path))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::bundled::{bundled_skills, content_hash};
    use crate::skills::cache::SkillCache;
    use tempfile::TempDir;

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

        assert!(!user_dir
            .path()
            .join(&skills[0].name)
            .join("SKILL.md")
            .exists());
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
        assert_eq!(lock.skills.len(), 4);
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
        assert!(project_dir
            .path()
            .join(".claude/skills")
            .join(&skills[0].name)
            .join("SKILL.md")
            .exists());
        assert!(!project_dir
            .path()
            .join(".claude/skills")
            .join(&skills[0].name)
            .is_symlink());
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

    #[test]
    fn test_discover_external_skills_empty_lock() {
        // With an empty lock, discover_external_skills_with should return empty
        // when there are no agent dirs (since we're in a test env with no real agents)
        let lock = SkillLock::default();
        let result = discover_external_skills_with(&lock).unwrap();
        // May or may not find skills depending on the actual machine state.
        // At minimum, it should not error.
        let _ = result;
    }

    #[test]
    fn test_try_parse_external_skill_with_frontmatter() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        std::fs::write(
            &skill_md,
            "---\nname: my-cool-skill\ndescription: A cool skill\n---\n\n# Hello\n",
        )
        .unwrap();

        let ext = try_parse_external_skill(&skill_md, "my-skill", "opencode", &skill_dir).unwrap();
        assert_eq!(ext.name, "my-cool-skill"); // from frontmatter, not dir name
        assert_eq!(ext.description, "A cool skill");
        assert_eq!(ext.agent, "opencode");
        assert!(!ext.content_hash.is_empty());
    }

    #[test]
    fn test_try_parse_external_skill_no_frontmatter() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("fallback-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        std::fs::write(&skill_md, "# No frontmatter here\n\nJust content.\n").unwrap();

        let ext =
            try_parse_external_skill(&skill_md, "fallback-skill", "codex", &skill_dir).unwrap();
        assert_eq!(ext.name, "fallback-skill"); // falls back to dir name
        assert_eq!(ext.description, "");
        assert_eq!(ext.agent, "codex");
    }

    #[test]
    fn test_expand_sub_skills() {
        let dir = TempDir::new().unwrap();
        let parent = dir.path().join("superpowers");
        std::fs::create_dir_all(&parent).unwrap();

        // Create two sub-skills
        let sub1 = parent.join("brainstorming");
        std::fs::create_dir_all(&sub1).unwrap();
        std::fs::write(
            sub1.join("SKILL.md"),
            "---\nname: brainstorming\ndescription: Creative thinking\n---\n# Brainstorming\n",
        )
        .unwrap();

        let sub2 = parent.join("debugging");
        std::fs::create_dir_all(&sub2).unwrap();
        std::fs::write(
            sub2.join("SKILL.md"),
            "---\nname: debugging\ndescription: Bug hunting\n---\n# Debugging\n",
        )
        .unwrap();

        // Create a directory without SKILL.md (should be skipped)
        let empty = parent.join("empty-dir");
        std::fs::create_dir_all(&empty).unwrap();

        let managed = std::collections::HashSet::new();
        let mut seen = std::collections::HashSet::new();
        let mut found = Vec::new();

        expand_sub_skills(&parent, "opencode", &managed, &mut seen, &mut found);

        assert_eq!(found.len(), 2);
        found.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(found[0].name, "brainstorming");
        assert_eq!(found[1].name, "debugging");
    }

    #[test]
    fn test_expand_sub_skills_skips_managed() {
        let dir = TempDir::new().unwrap();
        let parent = dir.path().join("superpowers");
        std::fs::create_dir_all(&parent).unwrap();

        let sub1 = parent.join("brainstorming");
        std::fs::create_dir_all(&sub1).unwrap();
        std::fs::write(
            sub1.join("SKILL.md"),
            "---\nname: brainstorming\n---\n# Brainstorming\n",
        )
        .unwrap();

        // Mark brainstorming as managed — should be skipped
        let mut managed = std::collections::HashSet::new();
        managed.insert("brainstorming");
        let mut seen = std::collections::HashSet::new();
        let mut found = Vec::new();

        expand_sub_skills(&parent, "opencode", &managed, &mut seen, &mut found);

        assert_eq!(found.len(), 0);
    }
}

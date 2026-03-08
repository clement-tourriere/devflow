use anyhow::{Context, Result};
use std::path::Path;

use super::cache::SkillCache;
use super::manifest;
use super::types::{InstalledSkill, Skill, SkillSource};
use chrono::Utc;

/// The standard skills directory (Agent Skills open standard).
const AGENTS_SKILLS_DIR: &str = ".agents/skills";

/// Install a skill into a project.
///
/// 1. Write content to global cache
/// 2. Create symlink in `.agents/skills/{name}` -> cache dir
/// 3. Create symlink in `.claude/skills/{name}` -> `../../.agents/skills/{name}`
/// 4. Update `.devflow/skills.lock`
pub fn install_skill(project_dir: &Path, skill: &Skill, cache: &SkillCache) -> Result<()> {
    // 1. Cache the content
    let cache_dir = match &skill.source {
        SkillSource::Bundled => {
            cache.store_bundled(&skill.name, &skill.content_hash, &skill.content)?
        }
        SkillSource::Github { owner, repo, .. } => cache.store(
            owner,
            repo,
            &skill.name,
            &skill.content_hash,
            &skill.content,
        )?,
    };

    // 2. Symlink .agents/skills/{name} -> cache dir
    let agents_dir = project_dir.join(AGENTS_SKILLS_DIR);
    std::fs::create_dir_all(&agents_dir)?;
    let agents_link = agents_dir.join(&skill.name);
    remove_link_or_dir(&agents_link)?;

    #[cfg(unix)]
    std::os::unix::fs::symlink(&cache_dir, &agents_link)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&cache_dir, &agents_link)?;

    // 3. Symlink .claude/skills/{name} -> ../../.agents/skills/{name}
    let claude_dir = project_dir.join(".claude").join("skills");
    std::fs::create_dir_all(&claude_dir)?;
    let claude_link = claude_dir.join(&skill.name);
    let relative_target = std::path::Path::new("../..")
        .join(AGENTS_SKILLS_DIR)
        .join(&skill.name);
    remove_link_or_dir(&claude_link)?;

    #[cfg(unix)]
    std::os::unix::fs::symlink(&relative_target, &claude_link)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&relative_target, &claude_link)?;

    // 4. Update lock file
    let mut lock = manifest::load_lock(project_dir)?;
    lock.skills.insert(
        skill.name.clone(),
        InstalledSkill {
            source: skill.source.clone(),
            content_hash: skill.content_hash.clone(),
            installed_at: Utc::now(),
        },
    );
    manifest::save_lock(project_dir, &lock)?;

    Ok(())
}

/// Remove a skill from a project.
pub fn remove_skill(project_dir: &Path, skill_name: &str) -> Result<()> {
    // Remove .agents/skills/{name}
    let agents_link = project_dir.join(AGENTS_SKILLS_DIR).join(skill_name);
    remove_link_or_dir(&agents_link)?;

    // Remove .claude/skills/{name}
    let claude_link = project_dir.join(".claude").join("skills").join(skill_name);
    remove_link_or_dir(&claude_link)?;

    // Update lock file
    let mut lock = manifest::load_lock(project_dir)?;
    lock.skills.remove(skill_name);
    manifest::save_lock(project_dir, &lock)?;

    Ok(())
}

/// Install all bundled skills into a project.
pub fn install_bundled_skills(project_dir: &Path, cache: &SkillCache) -> Result<Vec<String>> {
    let skills = super::bundled::bundled_skills();
    let mut installed = Vec::new();
    for skill in &skills {
        install_skill(project_dir, skill, cache)?;
        installed.push(skill.name.clone());
    }
    Ok(installed)
}

/// Check which installed skills have different content from what's available.
pub fn check_updates(
    project_dir: &Path,
    available: &[Skill],
) -> Result<Vec<(String, String, String)>> {
    let lock = manifest::load_lock(project_dir)?;
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

/// Remove a symlink or directory if it exists.
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
    use super::super::bundled::{bundled_skills, content_hash};
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_install_bundled_skills() {
        let project = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let cache = SkillCache::with_base(cache_dir.path().to_path_buf());

        let installed = install_bundled_skills(project.path(), &cache).unwrap();

        assert_eq!(installed.len(), 4);
        assert!(project
            .path()
            .join(".agents/skills/devflow-workspace-list")
            .exists());
        assert!(project
            .path()
            .join(".agents/skills/devflow-workspace-switch")
            .exists());
        assert!(project
            .path()
            .join(".agents/skills/devflow-workspace-create")
            .exists());
        assert!(project
            .path()
            .join(".agents/skills/devflow-brainstorming")
            .exists());
        assert!(project
            .path()
            .join(".claude/skills/devflow-workspace-list")
            .exists());
        assert!(project.path().join(".devflow/skills.lock").exists());
    }

    #[test]
    fn test_remove_skill() {
        let project = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let cache = SkillCache::with_base(cache_dir.path().to_path_buf());

        install_bundled_skills(project.path(), &cache).unwrap();
        remove_skill(project.path(), "devflow-workspace-list").unwrap();

        assert!(!project
            .path()
            .join(".agents/skills/devflow-workspace-list")
            .exists());
        assert!(!project
            .path()
            .join(".claude/skills/devflow-workspace-list")
            .exists());

        let lock = manifest::load_lock(project.path()).unwrap();
        assert!(!lock.skills.contains_key("devflow-workspace-list"));
        assert_eq!(lock.skills.len(), 3);
    }

    #[test]
    fn test_check_updates_detects_changes() {
        let project = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let cache = SkillCache::with_base(cache_dir.path().to_path_buf());

        install_bundled_skills(project.path(), &cache).unwrap();

        // Create a "new version" with different content
        let mut new_skills = bundled_skills();
        new_skills[0].content = "# Updated content".to_string();
        new_skills[0].content_hash = content_hash("# Updated content");

        let updates = check_updates(project.path(), &new_skills).unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, new_skills[0].name);
    }
}

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use super::types::SkillLock;

const LOCK_FILE: &str = ".devflow/skills.lock";

/// Load the skill lock file from a project directory.
pub fn load_lock(project_dir: &Path) -> Result<SkillLock> {
    let path = project_dir.join(LOCK_FILE);
    if !path.exists() {
        return Ok(SkillLock::default());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Reading skill lock file: {:?}", path))?;
    serde_json::from_str(&content).with_context(|| format!("Parsing skill lock file: {:?}", path))
}

/// Save the skill lock file to a project directory.
pub fn save_lock(project_dir: &Path, lock: &SkillLock) -> Result<()> {
    let path = project_dir.join(LOCK_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(lock)?;
    std::fs::write(&path, content)?;
    Ok(())
}

/// Get the lock file path for a project directory.
pub fn lock_path(project_dir: &Path) -> PathBuf {
    project_dir.join(LOCK_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::types::{InstalledSkill, SkillSource};
    use chrono::Utc;
    use tempfile::TempDir;

    #[test]
    fn test_load_missing_returns_default() {
        let tmp = TempDir::new().unwrap();
        let lock = load_lock(tmp.path()).unwrap();
        assert_eq!(lock.version, 1);
        assert!(lock.skills.is_empty());
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut lock = SkillLock::default();
        lock.skills.insert(
            "test-skill".to_string(),
            InstalledSkill {
                source: SkillSource::Bundled,
                content_hash: "abc123".to_string(),
                installed_at: Utc::now(),
            },
        );

        save_lock(tmp.path(), &lock).unwrap();
        let loaded = load_lock(tmp.path()).unwrap();

        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.skills.len(), 1);
        assert!(loaded.skills.contains_key("test-skill"));
        assert_eq!(loaded.skills["test-skill"].content_hash, "abc123");
    }

    #[test]
    fn test_creates_devflow_dir() {
        let tmp = TempDir::new().unwrap();
        assert!(!tmp.path().join(".devflow").exists());

        save_lock(tmp.path(), &SkillLock::default()).unwrap();
        assert!(tmp.path().join(".devflow").exists());
        assert!(tmp.path().join(".devflow/skills.lock").exists());
    }
}

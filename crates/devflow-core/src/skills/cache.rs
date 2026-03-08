use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Global skill cache directory.
///
/// Structure: `~/.local/share/devflow/skills/{owner}/{repo}/{skill-name}/{hash-prefix}/SKILL.md`
pub struct SkillCache {
    base_dir: PathBuf,
}

impl SkillCache {
    /// Create a cache using the default XDG data directory.
    pub fn new() -> Result<Self> {
        let base = dirs::data_dir()
            .context("Could not determine data directory")?
            .join("devflow")
            .join("skills");
        Ok(Self { base_dir: base })
    }

    /// Create a cache rooted at a custom directory (for testing).
    pub fn with_base(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Get the cache path for a skill identified by owner/repo/name and content hash.
    pub fn skill_path(
        &self,
        owner: &str,
        repo: &str,
        skill_name: &str,
        content_hash: &str,
    ) -> PathBuf {
        let hash_prefix = &content_hash[..12.min(content_hash.len())];
        self.base_dir
            .join(owner)
            .join(repo)
            .join(skill_name)
            .join(hash_prefix)
    }

    /// Get the cache path for a bundled skill.
    pub fn bundled_skill_path(&self, skill_name: &str, content_hash: &str) -> PathBuf {
        let hash_prefix = &content_hash[..12.min(content_hash.len())];
        self.base_dir
            .join("_bundled")
            .join(skill_name)
            .join(hash_prefix)
    }

    /// Check if a skill is cached.
    pub fn is_cached(&self, owner: &str, repo: &str, skill_name: &str, content_hash: &str) -> bool {
        self.skill_path(owner, repo, skill_name, content_hash)
            .join("SKILL.md")
            .exists()
    }

    /// Store skill content in cache. Returns the directory path.
    pub fn store(
        &self,
        owner: &str,
        repo: &str,
        skill_name: &str,
        content_hash: &str,
        content: &str,
    ) -> Result<PathBuf> {
        let dir = self.skill_path(owner, repo, skill_name, content_hash);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("SKILL.md"), content)?;
        Ok(dir)
    }

    /// Store a bundled skill in cache. Returns the directory path.
    pub fn store_bundled(
        &self,
        skill_name: &str,
        content_hash: &str,
        content: &str,
    ) -> Result<PathBuf> {
        let dir = self.bundled_skill_path(skill_name, content_hash);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("SKILL.md"), content)?;
        Ok(dir)
    }

    /// Read cached skill content.
    pub fn read(&self, cache_dir: &Path) -> Result<String> {
        let path = cache_dir.join("SKILL.md");
        std::fs::read_to_string(&path).with_context(|| format!("Reading cached skill: {:?}", path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_store_and_read() {
        let tmp = TempDir::new().unwrap();
        let cache = SkillCache::with_base(tmp.path().to_path_buf());

        let dir = cache
            .store(
                "obra",
                "superpowers",
                "brainstorming",
                "abcdef123456789",
                "# Skill content",
            )
            .unwrap();

        assert!(dir.join("SKILL.md").exists());
        assert_eq!(cache.read(&dir).unwrap(), "# Skill content");
    }

    #[test]
    fn test_is_cached() {
        let tmp = TempDir::new().unwrap();
        let cache = SkillCache::with_base(tmp.path().to_path_buf());

        assert!(!cache.is_cached("obra", "superpowers", "brainstorming", "abcdef123456789"));
        cache
            .store(
                "obra",
                "superpowers",
                "brainstorming",
                "abcdef123456789",
                "content",
            )
            .unwrap();
        assert!(cache.is_cached("obra", "superpowers", "brainstorming", "abcdef123456789"));
    }

    #[test]
    fn test_store_bundled() {
        let tmp = TempDir::new().unwrap();
        let cache = SkillCache::with_base(tmp.path().to_path_buf());

        let dir = cache
            .store_bundled("devflow-workspace-list", "hash123456789abc", "content")
            .unwrap();

        assert!(dir.join("SKILL.md").exists());
        assert!(dir.to_string_lossy().contains("_bundled"));
    }

    #[test]
    fn test_hash_prefix_in_path() {
        let tmp = TempDir::new().unwrap();
        let cache = SkillCache::with_base(tmp.path().to_path_buf());

        let path = cache.skill_path("owner", "repo", "name", "abcdef123456extra");
        assert!(path.to_string_lossy().contains("abcdef123456"));
        assert!(!path.to_string_lossy().contains("extra"));
    }
}

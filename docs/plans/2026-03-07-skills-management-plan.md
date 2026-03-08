# Skills Management System Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a full skills management system to devflow that can search skills.sh, install/remove/update skills from GitHub, and manage them per-project with content-hash versioning.

**Architecture:** New `skills/` module in devflow-core with types, cache, installer, marketplace client, and manifest management. New `SkillCommands` enum in CLI dispatching to a `skill.rs` handler. Existing `agent.rs` bundled skills preserved as offline fallback. Published skills in `skills/` at repo root.

**Tech Stack:** Rust, reqwest (HTTP), sha2 (hashing), serde_json (manifest), existing devflow-core patterns.

---

### Task 1: Add `sha2` dependency and `skills` feature to devflow-core

**Files:**
- Modify: `crates/devflow-core/Cargo.toml`

**Step 1: Add sha2 dependency and skills feature**

In `crates/devflow-core/Cargo.toml`, add to `[features]`:
```toml
skills = ["dep:reqwest", "dep:sha2"]
```

Add to `[dependencies]`:
```toml
# Content hashing for skill versioning
sha2 = { version = "0.10", optional = true }
```

**Step 2: Enable skills feature in root Cargo.toml**

In `Cargo.toml` (root), add to the `[features]` section:
```toml
skills = ["devflow-core/skills"]
```

And add `"skills"` to the `default` feature list.

**Step 3: Verify it compiles**

Run: `cargo check -p devflow-core 2>&1 | tail -5`
Expected: compiles without errors

**Step 4: Commit**

```
feat(core): add sha2 dependency and skills feature flag
```

---

### Task 2: Create skills module with core types

**Files:**
- Create: `crates/devflow-core/src/skills/mod.rs`
- Create: `crates/devflow-core/src/skills/types.rs`
- Modify: `crates/devflow-core/src/lib.rs`

**Step 1: Create the module structure**

Create `crates/devflow-core/src/skills/mod.rs`:
```rust
//! Skills management for devflow.
//!
//! Provides skill discovery, installation, removal, and update management
//! using the Agent Skills open standard (agentskills.io).

pub mod types;

pub use types::*;
```

**Step 2: Create core types**

Create `crates/devflow-core/src/skills/types.rs`:
```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Where a skill comes from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SkillSource {
    /// Embedded in the devflow binary (offline fallback).
    Bundled,
    /// Fetched from a GitHub repository.
    Github {
        owner: String,
        repo: String,
        /// Path within the repo, e.g. "skills/brainstorming".
        path: String,
    },
}

/// A skill with its metadata and content.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub source: SkillSource,
    pub content: String,
    /// SHA-256 hex digest of the SKILL.md content.
    pub content_hash: String,
}

/// A skill installed in a project, as recorded in the lock file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledSkill {
    pub source: SkillSource,
    pub content_hash: String,
    pub installed_at: DateTime<Utc>,
}

/// Project skill lock file (`.devflow/skills.lock`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillLock {
    pub version: u32,
    pub skills: HashMap<String, InstalledSkill>,
}

impl Default for SkillLock {
    fn default() -> Self {
        Self {
            version: 1,
            skills: HashMap::new(),
        }
    }
}

/// Search result from the skills.sh marketplace.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillSearchResult {
    pub id: String,
    #[serde(rename = "skillId")]
    pub skill_id: String,
    pub name: String,
    pub installs: u64,
    pub source: String,
}

/// Response from the skills.sh search API.
#[derive(Debug, Clone, Deserialize)]
pub struct SearchResponse {
    pub query: String,
    pub skills: Vec<SkillSearchResult>,
    pub count: usize,
}

/// Parsed SKILL.md frontmatter.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillFrontmatter {
    pub name: String,
    pub description: Option<String>,
}
```

**Step 3: Register the module**

In `crates/devflow-core/src/lib.rs`, add:
```rust
#[cfg(feature = "skills")]
pub mod skills;
```

**Step 4: Verify it compiles**

Run: `cargo check -p devflow-core 2>&1 | tail -5`
Expected: compiles without errors

**Step 5: Commit**

```
feat(core): add skills module with core types
```

---

### Task 3: Implement bundled skills provider

**Files:**
- Create: `crates/devflow-core/src/skills/bundled.rs`
- Modify: `crates/devflow-core/src/skills/mod.rs`

**Step 1: Create bundled.rs**

Create `crates/devflow-core/src/skills/bundled.rs` that provides the 3 workspace skills from the existing `agent.rs` string literals, but returns them as `Skill` structs with content hashes:

```rust
use sha2::{Digest, Sha256};
use super::types::{Skill, SkillSource};
use crate::agent::generate_workspace_skills;

/// Compute SHA-256 hex hash of content.
pub(crate) fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Return all bundled skills (embedded in the binary).
pub fn bundled_skills() -> Vec<Skill> {
    generate_workspace_skills()
        .into_iter()
        .map(|sf| {
            // Extract the skill name from the relative path
            // e.g. "devflow-workspace-list/SKILL.md" -> "devflow-workspace-list"
            let name = sf.relative_path
                .split('/')
                .next()
                .unwrap_or(&sf.relative_path)
                .to_string();

            // Parse frontmatter for description
            let description = parse_description(&sf.content)
                .unwrap_or_else(|| format!("Bundled devflow skill: {}", name));

            let hash = content_hash(&sf.content);

            Skill {
                name,
                description,
                source: SkillSource::Bundled,
                content: sf.content,
                content_hash: hash,
            }
        })
        .collect()
}

/// Extract description from YAML frontmatter.
fn parse_description(content: &str) -> Option<String> {
    let content = content.trim();
    if !content.starts_with("---") {
        return None;
    }
    let rest = &content[3..];
    let end = rest.find("---")?;
    let frontmatter = &rest[..end];
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(desc) = line.strip_prefix("description:") {
            return Some(desc.trim().trim_matches('"').to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bundled_skills_returns_three() {
        let skills = bundled_skills();
        assert_eq!(skills.len(), 3);
    }

    #[test]
    fn test_bundled_skill_names() {
        let skills = bundled_skills();
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"devflow-workspace-list"));
        assert!(names.contains(&"devflow-workspace-switch"));
        assert!(names.contains(&"devflow-workspace-create"));
    }

    #[test]
    fn test_bundled_skills_have_hashes() {
        let skills = bundled_skills();
        for skill in &skills {
            assert!(!skill.content_hash.is_empty());
            assert_eq!(skill.content_hash.len(), 64); // SHA-256 hex
        }
    }

    #[test]
    fn test_content_hash_deterministic() {
        assert_eq!(content_hash("hello"), content_hash("hello"));
        assert_ne!(content_hash("hello"), content_hash("world"));
    }

    #[test]
    fn test_bundled_skills_have_descriptions() {
        let skills = bundled_skills();
        for skill in &skills {
            assert!(!skill.description.is_empty());
        }
    }
}
```

**Step 2: Register in mod.rs**

Add to `crates/devflow-core/src/skills/mod.rs`:
```rust
pub mod bundled;
```

**Step 3: Run tests**

Run: `cargo test -p devflow-core bundled 2>&1 | tail -20`
Expected: 5 tests pass

**Step 4: Commit**

```
feat(core): implement bundled skills provider with content hashing
```

---

### Task 4: Implement skill cache manager

**Files:**
- Create: `crates/devflow-core/src/skills/cache.rs`
- Modify: `crates/devflow-core/src/skills/mod.rs`

**Step 1: Create cache.rs**

```rust
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
    pub fn is_cached(
        &self,
        owner: &str,
        repo: &str,
        skill_name: &str,
        content_hash: &str,
    ) -> bool {
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
            .store("obra", "superpowers", "brainstorming", "abcdef123456789", "# Skill content")
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
            .store("obra", "superpowers", "brainstorming", "abcdef123456789", "content")
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
```

**Step 2: Register in mod.rs**

Add `pub mod cache;` to `crates/devflow-core/src/skills/mod.rs`.

**Step 3: Run tests**

Run: `cargo test -p devflow-core cache 2>&1 | tail -20`
Expected: 4 tests pass

**Step 4: Commit**

```
feat(core): implement global skill cache with content-hash paths
```

---

### Task 5: Implement skill lock file (manifest) management

**Files:**
- Create: `crates/devflow-core/src/skills/manifest.rs`
- Modify: `crates/devflow-core/src/skills/mod.rs`

**Step 1: Create manifest.rs**

```rust
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
    serde_json::from_str(&content)
        .with_context(|| format!("Parsing skill lock file: {:?}", path))
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
```

**Step 2: Register in mod.rs**

Add `pub mod manifest;` to `crates/devflow-core/src/skills/mod.rs`.

**Step 3: Run tests**

Run: `cargo test -p devflow-core manifest 2>&1 | tail -20`
Expected: 3 tests pass

**Step 4: Commit**

```
feat(core): implement skill lock file management
```

---

### Task 6: Implement skill installer (local operations)

**Files:**
- Create: `crates/devflow-core/src/skills/installer.rs`
- Modify: `crates/devflow-core/src/skills/mod.rs`

**Step 1: Create installer.rs**

This handles the local filesystem operations: symlinking skills into `.agents/skills/` and `.claude/skills/`, and updating the lock file.

```rust
use anyhow::{Context, Result};
use std::path::Path;

use super::bundled::content_hash;
use super::cache::SkillCache;
use super::manifest;
use super::types::{InstalledSkill, Skill, SkillLock, SkillSource};
use chrono::Utc;

/// The standard skills directory (Agent Skills open standard).
const AGENTS_SKILLS_DIR: &str = ".agents/skills";

/// Install a skill into a project.
///
/// 1. Write content to global cache
/// 2. Create symlink in `.agents/skills/{name}` -> cache dir
/// 3. Create symlink in `.claude/skills/{name}` -> `../../.agents/skills/{name}`
/// 4. Update `.devflow/skills.lock`
pub fn install_skill(
    project_dir: &Path,
    skill: &Skill,
    cache: &SkillCache,
) -> Result<()> {
    // 1. Cache the content
    let cache_dir = match &skill.source {
        SkillSource::Bundled => {
            cache.store_bundled(&skill.name, &skill.content_hash, &skill.content)?
        }
        SkillSource::Github { owner, repo, .. } => {
            cache.store(owner, repo, &skill.name, &skill.content_hash, &skill.content)?
        }
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
    let relative_target = std::path::Path::new("../..").join(AGENTS_SKILLS_DIR).join(&skill.name);
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
        std::fs::remove_file(path)
            .with_context(|| format!("Removing symlink: {:?}", path))?;
    } else if path.exists() {
        std::fs::remove_dir_all(path)
            .with_context(|| format!("Removing directory: {:?}", path))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::bundled::bundled_skills;
    use tempfile::TempDir;

    #[test]
    fn test_install_bundled_skills() {
        let project = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let cache = SkillCache::with_base(cache_dir.path().to_path_buf());

        let installed = install_bundled_skills(project.path(), &cache).unwrap();

        assert_eq!(installed.len(), 3);
        assert!(project.path().join(".agents/skills/devflow-workspace-list").exists());
        assert!(project.path().join(".agents/skills/devflow-workspace-switch").exists());
        assert!(project.path().join(".agents/skills/devflow-workspace-create").exists());
        assert!(project.path().join(".claude/skills/devflow-workspace-list").exists());
        assert!(project.path().join(".devflow/skills.lock").exists());
    }

    #[test]
    fn test_remove_skill() {
        let project = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let cache = SkillCache::with_base(cache_dir.path().to_path_buf());

        install_bundled_skills(project.path(), &cache).unwrap();
        remove_skill(project.path(), "devflow-workspace-list").unwrap();

        assert!(!project.path().join(".agents/skills/devflow-workspace-list").exists());
        assert!(!project.path().join(".claude/skills/devflow-workspace-list").exists());

        let lock = manifest::load_lock(project.path()).unwrap();
        assert!(!lock.skills.contains_key("devflow-workspace-list"));
        assert_eq!(lock.skills.len(), 2);
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
```

**Step 2: Register in mod.rs**

Add `pub mod installer;` to `crates/devflow-core/src/skills/mod.rs`.

**Step 3: Run tests**

Run: `cargo test -p devflow-core installer 2>&1 | tail -20`
Expected: 3 tests pass

**Step 4: Commit**

```
feat(core): implement skill installer with symlinks and lock file
```

---

### Task 7: Implement marketplace client (skills.sh + GitHub)

**Files:**
- Create: `crates/devflow-core/src/skills/marketplace.rs`
- Modify: `crates/devflow-core/src/skills/mod.rs`

**Step 1: Create marketplace.rs**

```rust
use anyhow::{bail, Context, Result};

use super::bundled::content_hash;
use super::types::{SearchResponse, Skill, SkillFrontmatter, SkillSource, SkillSearchResult};

const SKILLS_SH_API: &str = "https://skills.sh/api/search";

/// Search skills.sh marketplace.
pub async fn search(query: &str, limit: usize) -> Result<Vec<SkillSearchResult>> {
    let client = reqwest::Client::new();
    let resp = client
        .get(SKILLS_SH_API)
        .query(&[("q", query), ("limit", &limit.to_string())])
        .send()
        .await
        .context("Failed to reach skills.sh")?;

    if !resp.status().is_success() {
        bail!("skills.sh returned status {}", resp.status());
    }

    let search_resp: SearchResponse = resp.json().await.context("Parsing skills.sh response")?;
    Ok(search_resp.skills)
}

/// Fetch a skill from GitHub.
///
/// `identifier` can be:
/// - `owner/repo` — fetches all skills from the repo's `skills/` directory (returns first found)
/// - `owner/repo/skill-name` — fetches a specific skill
pub async fn fetch_skill(owner: &str, repo: &str, skill_name: &str) -> Result<Skill> {
    let client = reqwest::Client::builder()
        .user_agent("devflow")
        .build()?;

    // Try common paths where skills are stored
    let paths_to_try = vec![
        format!("skills/{}/SKILL.md", skill_name),
        format!("{}/SKILL.md", skill_name),
        format!(".agents/skills/{}/SKILL.md", skill_name),
    ];

    let mut last_error = None;
    for path in &paths_to_try {
        let url = format!(
            "https://raw.githubusercontent.com/{}/{}/main/{}",
            owner, repo, path
        );

        let resp = client.get(&url).send().await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let content = r.text().await.context("Reading skill content")?;
                let hash = content_hash(&content);
                let frontmatter = parse_frontmatter(&content);

                return Ok(Skill {
                    name: frontmatter
                        .as_ref()
                        .map(|f| f.name.clone())
                        .unwrap_or_else(|| skill_name.to_string()),
                    description: frontmatter
                        .as_ref()
                        .and_then(|f| f.description.clone())
                        .unwrap_or_default(),
                    source: SkillSource::Github {
                        owner: owner.to_string(),
                        repo: repo.to_string(),
                        path: path.clone(),
                    },
                    content,
                    content_hash: hash,
                });
            }
            Ok(r) => {
                last_error = Some(format!("{} returned {}", url, r.status()));
            }
            Err(e) => {
                last_error = Some(format!("{}: {}", url, e));
            }
        }
    }

    bail!(
        "Could not find skill '{}' in {}/{}. Last error: {}",
        skill_name,
        owner,
        repo,
        last_error.unwrap_or_else(|| "unknown".to_string())
    )
}

/// List all skills in a GitHub repository.
pub async fn list_repo_skills(owner: &str, repo: &str) -> Result<Vec<String>> {
    // Use the GitHub API to list the skills/ directory
    let client = reqwest::Client::builder()
        .user_agent("devflow")
        .build()?;

    let url = format!(
        "https://api.github.com/repos/{}/{}/contents/skills",
        owner, repo
    );

    let mut request = client.get(&url);

    // Use GITHUB_TOKEN if available for higher rate limits
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        request = request.header("Authorization", format!("Bearer {}", token));
    }

    let resp = request.send().await.context("Listing repo skills")?;

    if !resp.status().is_success() {
        bail!(
            "Could not list skills in {}/{}: {} (try setting GITHUB_TOKEN for higher rate limits)",
            owner,
            repo,
            resp.status()
        );
    }

    #[derive(serde::Deserialize)]
    struct GithubEntry {
        name: String,
        #[serde(rename = "type")]
        entry_type: String,
    }

    let entries: Vec<GithubEntry> = resp.json().await?;
    Ok(entries
        .into_iter()
        .filter(|e| e.entry_type == "dir")
        .map(|e| e.name)
        .collect())
}

/// Parse YAML frontmatter from SKILL.md content.
fn parse_frontmatter(content: &str) -> Option<SkillFrontmatter> {
    let content = content.trim();
    if !content.starts_with("---") {
        return None;
    }
    let rest = &content[3..];
    let end = rest.find("---")?;
    let yaml = &rest[..end];
    serde_yaml_ng::from_str(yaml).ok()
}
```

**Step 2: Register in mod.rs**

Add `pub mod marketplace;` to `crates/devflow-core/src/skills/mod.rs`.

**Step 3: Verify it compiles**

Run: `cargo check -p devflow-core 2>&1 | tail -5`
Expected: compiles (network-dependent tests can be added later)

**Step 4: Commit**

```
feat(core): implement marketplace client for skills.sh and GitHub
```

---

### Task 8: Create the public API surface in skills/mod.rs

**Files:**
- Modify: `crates/devflow-core/src/skills/mod.rs`

**Step 1: Update mod.rs with full public API**

```rust
//! Skills management for devflow.
//!
//! Provides skill discovery, installation, removal, and update management
//! using the Agent Skills open standard (agentskills.io).

pub mod bundled;
pub mod cache;
pub mod installer;
pub mod manifest;
pub mod marketplace;
pub mod types;

pub use types::*;
```

**Step 2: Run all skills tests**

Run: `cargo test -p devflow-core skills 2>&1 | tail -30`
Expected: All tests pass

**Step 3: Commit**

```
feat(core): finalize skills module public API
```

---

### Task 9: Add CLI SkillCommands and skill.rs handler

**Files:**
- Create: `src/cli/skill.rs`
- Modify: `src/cli/mod.rs`

**Step 1: Add SkillCommands enum to mod.rs**

In `src/cli/mod.rs`, add after the `AgentCommands` enum:

```rust
/// Subcommands for `devflow skill`.
#[derive(Subcommand)]
pub enum SkillCommands {
    #[command(about = "List installed skills")]
    List {
        #[arg(long, help = "Include available skills from marketplace")]
        available: bool,
        #[arg(long, help = "Show only skills with updates available")]
        updates: bool,
    },
    #[command(about = "Search skills.sh marketplace")]
    Search {
        /// Search query
        query: String,
        #[arg(long, default_value = "20", help = "Maximum results")]
        limit: usize,
    },
    #[command(about = "Install a skill")]
    Install {
        /// Skill identifier: owner/repo, owner/repo/skill-name, or skill name from search
        identifier: String,
        #[arg(long, help = "Install specific skill(s) from a repo")]
        skill: Option<Vec<String>>,
    },
    #[command(about = "Remove a skill from this project")]
    Remove {
        /// Skill name to remove
        name: String,
    },
    #[command(about = "Update skills to latest versions")]
    Update {
        /// Specific skill to update (updates all if omitted)
        name: Option<String>,
        #[arg(long, help = "Check for updates without applying")]
        check: bool,
    },
    #[command(about = "Show details of an installed skill")]
    Show {
        /// Skill name
        name: String,
    },
}
```

Also add a new top-level `Skill` variant to the `Commands` enum:

```rust
    /// Manage agent skills
    #[command(subcommand)]
    Skill(SkillCommands),
```

And add the dispatch in `handle_command`:

```rust
    Commands::Skill(action) => {
        skill::handle_skill_command(action, json_output, &config_path).await?;
    }
```

**Step 2: Create src/cli/skill.rs**

```rust
use anyhow::{bail, Result};
use std::path::PathBuf;

use devflow_core::skills::{
    bundled::bundled_skills,
    cache::SkillCache,
    installer,
    manifest,
    marketplace,
    SkillSource,
};

pub(super) async fn handle_skill_command(
    action: super::SkillCommands,
    json_output: bool,
    config_path: &Option<PathBuf>,
) -> Result<()> {
    let project_dir = config_path
        .as_ref()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    match action {
        super::SkillCommands::List { available, updates } => {
            let lock = manifest::load_lock(&project_dir)?;

            if json_output {
                if available {
                    // Show installed + bundled not yet installed
                    let bundled = bundled_skills();
                    let mut all: Vec<serde_json::Value> = lock
                        .skills
                        .iter()
                        .map(|(name, s)| {
                            serde_json::json!({
                                "name": name,
                                "source": s.source,
                                "content_hash": &s.content_hash[..12],
                                "installed": true,
                            })
                        })
                        .collect();

                    for skill in &bundled {
                        if !lock.skills.contains_key(&skill.name) {
                            all.push(serde_json::json!({
                                "name": skill.name,
                                "source": skill.source,
                                "installed": false,
                            }));
                        }
                    }
                    println!("{}", serde_json::to_string_pretty(&all)?);
                } else {
                    println!("{}", serde_json::to_string_pretty(&lock)?);
                }
            } else if lock.skills.is_empty() {
                println!("No skills installed.");
                println!("Run `devflow skill install <name>` to install skills.");
            } else {
                println!("Installed skills:\n");
                println!(
                    "  {:<30} {:<15} {}",
                    "NAME", "HASH", "SOURCE"
                );
                for (name, skill) in &lock.skills {
                    let source_label = match &skill.source {
                        SkillSource::Bundled => "bundled".to_string(),
                        SkillSource::Github { owner, repo, .. } => {
                            format!("{}/{}", owner, repo)
                        }
                    };
                    println!(
                        "  {:<30} {:<15} {}",
                        name,
                        &skill.content_hash[..12.min(skill.content_hash.len())],
                        source_label,
                    );
                }

                if updates {
                    let bundled = bundled_skills();
                    let update_list = installer::check_updates(&project_dir, &bundled)?;
                    if !update_list.is_empty() {
                        println!("\nUpdates available:");
                        for (name, _, _) in &update_list {
                            println!("  {}", name);
                        }
                    }
                }
            }
            Ok(())
        }

        super::SkillCommands::Search { query, limit } => {
            let results = marketplace::search(&query, limit).await?;

            if json_output {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else if results.is_empty() {
                println!("No skills found for \"{}\".", query);
            } else {
                println!("Search results for \"{}\":\n", query);
                println!(
                    "  {:<30} {:<25} {}",
                    "NAME", "SOURCE", "INSTALLS"
                );
                for result in &results {
                    let installs = if result.installs >= 1000 {
                        format!("{:.1}K", result.installs as f64 / 1000.0)
                    } else {
                        result.installs.to_string()
                    };
                    println!(
                        "  {:<30} {:<25} {}",
                        result.name, result.source, installs,
                    );
                }
                println!("\nRun `devflow skill install <source>/<name>` to install.");
            }
            Ok(())
        }

        super::SkillCommands::Install { identifier, skill: skill_names } => {
            let cache = SkillCache::new()?;

            // Parse identifier: "owner/repo" or "owner/repo/skill"
            let parts: Vec<&str> = identifier.split('/').collect();
            match parts.len() {
                2 => {
                    // owner/repo — install specific skills or list available
                    let owner = parts[0];
                    let repo = parts[1];

                    let names = if let Some(names) = skill_names {
                        names
                    } else {
                        // List and let user choose, or install all
                        let available = marketplace::list_repo_skills(owner, repo).await?;
                        if available.is_empty() {
                            bail!("No skills found in {}/{}", owner, repo);
                        }
                        if !json_output {
                            println!("Found {} skills in {}/{}:", available.len(), owner, repo);
                            for name in &available {
                                println!("  {}", name);
                            }
                            println!("\nInstalling all...");
                        }
                        available
                    };

                    for name in &names {
                        let skill = marketplace::fetch_skill(owner, repo, name).await?;
                        installer::install_skill(&project_dir, &skill, &cache)?;
                        if !json_output {
                            println!("Installed: {}", name);
                        }
                    }

                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string(&serde_json::json!({"installed": names}))?
                        );
                    }
                }
                3 => {
                    // owner/repo/skill-name
                    let owner = parts[0];
                    let repo = parts[1];
                    let skill_name = parts[2];

                    let skill = marketplace::fetch_skill(owner, repo, skill_name).await?;
                    installer::install_skill(&project_dir, &skill, &cache)?;

                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string(&serde_json::json!({
                                "installed": [skill_name],
                                "content_hash": &skill.content_hash[..12],
                            }))?
                        );
                    } else {
                        println!("Installed: {} ({})", skill_name, &skill.content_hash[..12]);
                    }
                }
                _ => {
                    bail!(
                        "Invalid identifier '{}'. Use 'owner/repo' or 'owner/repo/skill-name'.",
                        identifier
                    );
                }
            }
            Ok(())
        }

        super::SkillCommands::Remove { name } => {
            installer::remove_skill(&project_dir, &name)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({"removed": name}))?
                );
            } else {
                println!("Removed: {}", name);
            }
            Ok(())
        }

        super::SkillCommands::Update { name, check } => {
            let cache = SkillCache::new()?;
            let lock = manifest::load_lock(&project_dir)?;

            let skills_to_check: Vec<String> = if let Some(ref n) = name {
                if !lock.skills.contains_key(n) {
                    bail!("Skill '{}' is not installed.", n);
                }
                vec![n.clone()]
            } else {
                lock.skills.keys().cloned().collect()
            };

            let mut updated = Vec::new();
            for skill_name in &skills_to_check {
                if let Some(installed) = lock.skills.get(skill_name) {
                    let new_skill = match &installed.source {
                        SkillSource::Bundled => {
                            // Check against current bundled version
                            bundled_skills()
                                .into_iter()
                                .find(|s| s.name == *skill_name)
                        }
                        SkillSource::Github { owner, repo, .. } => {
                            marketplace::fetch_skill(owner, repo, skill_name)
                                .await
                                .ok()
                        }
                    };

                    if let Some(new) = new_skill {
                        if new.content_hash != installed.content_hash {
                            if check {
                                if !json_output {
                                    println!(
                                        "  {} — update available ({} -> {})",
                                        skill_name,
                                        &installed.content_hash[..12],
                                        &new.content_hash[..12],
                                    );
                                }
                                updated.push(skill_name.clone());
                            } else {
                                installer::install_skill(&project_dir, &new, &cache)?;
                                if !json_output {
                                    println!("Updated: {}", skill_name);
                                }
                                updated.push(skill_name.clone());
                            }
                        }
                    }
                }
            }

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "checked": skills_to_check.len(),
                        "updates": updated,
                    }))?
                );
            } else if updated.is_empty() {
                println!("All skills are up to date.");
            } else if check {
                println!("\n{} update(s) available. Run `devflow skill update` to apply.", updated.len());
            }
            Ok(())
        }

        super::SkillCommands::Show { name } => {
            let lock = manifest::load_lock(&project_dir)?;
            if let Some(installed) = lock.skills.get(&name) {
                if json_output {
                    println!("{}", serde_json::to_string_pretty(&installed)?);
                } else {
                    println!("Skill: {}", name);
                    println!(
                        "Source: {}",
                        match &installed.source {
                            SkillSource::Bundled => "bundled".to_string(),
                            SkillSource::Github { owner, repo, .. } =>
                                format!("{}/{}", owner, repo),
                        }
                    );
                    println!("Hash: {}", &installed.content_hash[..12]);
                    println!("Installed: {}", installed.installed_at.format("%Y-%m-%d %H:%M"));

                    // Try to read content
                    let skill_path = project_dir
                        .join(".agents/skills")
                        .join(&name)
                        .join("SKILL.md");
                    if skill_path.exists() {
                        if let Ok(content) = std::fs::read_to_string(&skill_path) {
                            println!("\n---\n{}", content);
                        }
                    }
                }
            } else {
                bail!("Skill '{}' is not installed.", name);
            }
            Ok(())
        }
    }
}
```

**Step 3: Register skill module in cli/mod.rs**

Add `mod skill;` to the module declarations at the top.

**Step 4: Verify it compiles**

Run: `cargo check 2>&1 | tail -10`
Expected: compiles

**Step 5: Commit**

```
feat(cli): add devflow skill commands (list, search, install, remove, update, show)
```

---

### Task 10: Publish skills at repo root

**Files:**
- Create: `skills/devflow-workspace-list/SKILL.md`
- Create: `skills/devflow-workspace-switch/SKILL.md`
- Create: `skills/devflow-workspace-create/SKILL.md`

**Step 1: Create skill files**

Copy the content from the existing `.agents/skills/` directories (which are generated from `agent.rs`) into `skills/` at the repo root. These become the published, canonical versions for skills.sh.

**Step 2: Commit**

```
feat: publish devflow skills for skills.sh discovery
```

---

### Task 11: Update the existing agent skill command to use the new system

**Files:**
- Modify: `src/cli/agent.rs` (lines 96-110)
- Modify: `src/cli/mod.rs` (AgentCommands::Skill help text)

**Step 1: Update AgentCommands::Skill to delegate to new system**

In `src/cli/agent.rs`, change the `Skill` handler to use the new skills installer:

```rust
super::AgentCommands::Skill => {
    let project_dir = std::env::current_dir()?;
    let cache = devflow_core::skills::cache::SkillCache::new()?;
    let installed = devflow_core::skills::installer::install_bundled_skills(&project_dir, &cache)?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({"installed": installed}))?
        );
    } else {
        for name in &installed {
            println!("Installed: {}", name);
        }
    }
    Ok(())
}
```

**Step 2: Verify it compiles and works**

Run: `cargo check 2>&1 | tail -5`
Expected: compiles

**Step 3: Commit**

```
refactor(cli): agent skill command now uses new skills system
```

---

### Task 12: Full integration test

**Step 1: Run all tests**

Run: `cargo test -p devflow-core 2>&1 | tail -30`
Expected: All tests pass

**Step 2: Run cargo check on entire workspace**

Run: `cargo check --workspace 2>&1 | tail -10`
Expected: compiles

**Step 3: Manual smoke test**

```bash
cargo run -- skill list
cargo run -- skill search "brainstorming" --limit 5
cargo run -- agent skill
cargo run -- skill list
```

**Step 4: Commit if any fixes were needed**

```
fix: integration fixes for skills management system
```

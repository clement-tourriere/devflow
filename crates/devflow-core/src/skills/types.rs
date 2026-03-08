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

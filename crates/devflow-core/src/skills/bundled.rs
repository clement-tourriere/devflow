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
            let name = sf
                .relative_path
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
    fn test_bundled_skills_returns_four() {
        let skills = bundled_skills();
        assert_eq!(skills.len(), 4);
    }

    #[test]
    fn test_bundled_skill_names() {
        let skills = bundled_skills();
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"devflow-workspace-list"));
        assert!(names.contains(&"devflow-workspace-switch"));
        assert!(names.contains(&"devflow-workspace-create"));
        assert!(names.contains(&"devflow-brainstorming"));
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

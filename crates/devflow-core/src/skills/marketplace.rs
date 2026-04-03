use anyhow::{bail, Context, Result};

use super::bundled::content_hash;
use super::types::{SearchResponse, Skill, SkillFrontmatter, SkillSearchResult, SkillSource};

const SKILLS_SH_API: &str = "https://skills.sh/api/search";

/// Search skills.sh marketplace.
pub async fn search(query: &str, limit: usize) -> Result<Vec<SkillSearchResult>> {
    let client = reqwest::Client::new();
    let url = format!("{}?q={}&limit={}", SKILLS_SH_API, urlencoded(query), limit);
    let resp = client
        .get(&url)
        .send()
        .await
        .context("Failed to reach skills.sh")?;

    if !resp.status().is_success() {
        bail!("skills.sh returned status {}", resp.status());
    }

    let search_resp: SearchResponse = resp
        .json::<SearchResponse>()
        .await
        .context("Parsing skills.sh response")?;
    Ok(search_resp.skills)
}

/// Simple URL encoding for query parameters.
fn urlencoded(s: &str) -> String {
    s.replace(' ', "+")
        .replace('&', "%26")
        .replace('=', "%3D")
        .replace('#', "%23")
}

/// Build a reqwest client with user-agent and optional GitHub token.
fn github_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().user_agent("devflow").build()?)
}

/// Build a GET request, adding GITHUB_TOKEN auth if available.
fn github_get(client: &reqwest::Client, url: &str) -> reqwest::RequestBuilder {
    let mut request = client.get(url);
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        request = request.header("Authorization", format!("Bearer {}", token));
    }
    request
}

/// Try to fetch a SKILL.md at a specific path in a GitHub repo.
/// Returns `Ok(Some(Skill))` if found, `Ok(None)` if 404, `Err` on network errors.
async fn try_fetch_skill_at_path(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
    path: &str,
    skill_name: &str,
) -> Result<Option<Skill>> {
    let url = format!(
        "https://raw.githubusercontent.com/{}/{}/main/{}",
        owner, repo, path
    );

    let resp = github_get(client, &url).send().await;
    match resp {
        Ok(r) if r.status().is_success() => {
            let content = r.text().await.context("Reading skill content")?;
            let hash = content_hash(&content);
            let frontmatter = parse_frontmatter(&content);

            Ok(Some(Skill {
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
                    path: path.to_string(),
                },
                content,
                content_hash: hash,
            }))
        }
        Ok(_) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Fetch a skill from GitHub.
///
/// `skill_name` is the skill name as known to skills.sh (from frontmatter `name` field).
/// This may differ from the directory name on disk (e.g. skill name `vercel-react-best-practices`
/// lives in directory `skills/react-best-practices/`).
///
/// Strategy:
/// 1. Fast path: try common directory patterns using `skill_name` as the directory name.
/// 2. Fallback: list directories via GitHub Contents API, then check each SKILL.md's
///    frontmatter until we find a matching `name`.
pub async fn fetch_skill(owner: &str, repo: &str, skill_name: &str) -> Result<Skill> {
    let client = github_client()?;

    // Fast path: try using skill_name directly as the directory name
    let paths_to_try = vec![
        format!("skills/{}/SKILL.md", skill_name),
        format!("{}/SKILL.md", skill_name),
        format!(".claude/skills/{}/SKILL.md", skill_name),
    ];

    for path in &paths_to_try {
        match try_fetch_skill_at_path(&client, owner, repo, path, skill_name).await {
            Ok(Some(skill)) => return Ok(skill),
            Ok(None) => continue,
            Err(_) => continue,
        }
    }

    // Fallback: directory name doesn't match skill name.
    // List directories in the repo and try each one.
    if let Ok(dirs) = list_repo_dirs(&client, owner, repo, "skills").await {
        for dir_name in &dirs {
            // Skip if we already tried this name
            if dir_name == skill_name {
                continue;
            }
            let path = format!("skills/{}/SKILL.md", dir_name);
            match try_fetch_skill_at_path(&client, owner, repo, &path, skill_name).await {
                Ok(Some(skill)) if skill.name == skill_name => return Ok(skill),
                Ok(Some(_)) => continue, // Found a SKILL.md but name doesn't match
                Ok(None) => continue,
                Err(_) => continue,
            }
        }
    }

    // Also try root-level directories
    if let Ok(dirs) = list_repo_dirs(&client, owner, repo, "").await {
        for dir_name in &dirs {
            if dir_name == skill_name {
                continue; // Already tried in fast path
            }
            let path = format!("{}/SKILL.md", dir_name);
            match try_fetch_skill_at_path(&client, owner, repo, &path, skill_name).await {
                Ok(Some(skill)) if skill.name == skill_name => return Ok(skill),
                Ok(Some(_)) => continue,
                Ok(None) => continue,
                Err(_) => continue,
            }
        }
    }

    bail!(
        "Could not find skill '{}' in {}/{}. The skill name may not match any directory in the repository.",
        skill_name,
        owner,
        repo
    )
}

/// List directory names within a path in a GitHub repo.
async fn list_repo_dirs(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
    path: &str,
) -> Result<Vec<String>> {
    let url = if path.is_empty() {
        format!("https://api.github.com/repos/{}/{}/contents", owner, repo)
    } else {
        format!(
            "https://api.github.com/repos/{}/{}/contents/{}",
            owner, repo, path
        )
    };

    let resp = github_get(client, &url)
        .send()
        .await
        .context("Listing repo contents")?;

    if !resp.status().is_success() {
        bail!("GitHub API returned {}", resp.status());
    }

    #[derive(serde::Deserialize)]
    struct GithubEntry {
        name: String,
        #[serde(rename = "type")]
        entry_type: String,
    }

    let entries = resp.json::<Vec<GithubEntry>>().await?;
    Ok(entries
        .into_iter()
        .filter(|e| e.entry_type == "dir")
        .map(|e| e.name)
        .collect())
}

/// List all skills in a GitHub repository.
pub async fn list_repo_skills(owner: &str, repo: &str) -> Result<Vec<String>> {
    let client = github_client()?;
    list_repo_dirs(&client, owner, repo, "skills").await
}

/// Parse YAML frontmatter from SKILL.md content.
pub fn parse_frontmatter(content: &str) -> Option<SkillFrontmatter> {
    let content = content.trim();
    if !content.starts_with("---") {
        return None;
    }
    let rest = &content[3..];
    let end = rest.find("---")?;
    let yaml = &rest[..end];
    serde_yaml_ng::from_str(yaml).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter_valid() {
        let content = r#"---
name: test-skill
description: A test skill
---

# Test Skill
"#;
        let fm = parse_frontmatter(content).unwrap();
        assert_eq!(fm.name, "test-skill");
        assert_eq!(fm.description.unwrap(), "A test skill");
    }

    #[test]
    fn test_parse_frontmatter_no_description() {
        let content = r#"---
name: test-skill
---

# Test Skill
"#;
        let fm = parse_frontmatter(content).unwrap();
        assert_eq!(fm.name, "test-skill");
        assert!(fm.description.is_none());
    }

    #[test]
    fn test_parse_frontmatter_missing() {
        let content = "# Just a heading\n\nNo frontmatter here.";
        assert!(parse_frontmatter(content).is_none());
    }
}

use serde::Serialize;

use devflow_core::skills::{
    bundled::bundled_skills, cache::SkillCache, installer, manifest, marketplace,
    types::SkillLock, user_installer, InstalledSkill, SkillSource,
};

#[derive(Serialize)]
pub struct InstalledSkillInfo {
    pub name: String,
    pub source: SkillSource,
    pub content_hash: String,
    pub installed_at: String,
}

#[derive(Serialize)]
pub struct SkillDetail {
    pub name: String,
    pub source: SkillSource,
    pub content_hash: String,
    pub installed_at: String,
    pub content: String,
}

#[derive(Serialize)]
pub struct SkillSearchResult {
    pub id: String,
    pub name: String,
    pub source: String,
    pub installs: u64,
}

#[derive(Serialize)]
pub struct SkillSearchDetail {
    pub name: String,
    pub source: String,
    pub description: String,
    pub content: String,
}

fn to_info(name: &str, skill: &InstalledSkill) -> InstalledSkillInfo {
    InstalledSkillInfo {
        name: name.to_string(),
        source: skill.source.clone(),
        content_hash: skill.content_hash.clone(),
        installed_at: skill.installed_at.format("%Y-%m-%d %H:%M").to_string(),
    }
}

#[tauri::command]
pub async fn skill_list(project_path: String) -> Result<Vec<InstalledSkillInfo>, String> {
    let project_dir = std::path::Path::new(&project_path);
    let lock = reconcile_lock(project_dir).map_err(crate::commands::format_error)?;
    Ok(lock
        .skills
        .iter()
        .map(|(name, skill)| to_info(name, skill))
        .collect())
}

/// Load the skills.lock and reconcile it with what's actually on disk.
///
/// Skills installed by the older `agent::install_agent_skills()` path wrote files
/// to `.agents/skills/` without updating `skills.lock`. This function detects
/// on-disk bundled skills not tracked in the lock and auto-registers them.
fn reconcile_lock(project_dir: &std::path::Path) -> anyhow::Result<SkillLock> {
    let mut lock = manifest::load_lock(project_dir)?;
    let bundled = bundled_skills();
    let agents_dir = project_dir.join(".agents/skills");

    let mut changed = false;
    for skill in &bundled {
        if lock.skills.contains_key(&skill.name) {
            continue;
        }
        // Check if the skill exists on disk but isn't tracked
        let skill_md = agents_dir.join(&skill.name).join("SKILL.md");
        if skill_md.exists() {
            lock.skills.insert(
                skill.name.clone(),
                InstalledSkill {
                    source: skill.source.clone(),
                    content_hash: skill.content_hash.clone(),
                    installed_at: chrono::Utc::now(),
                },
            );
            changed = true;
        }
    }

    if changed {
        manifest::save_lock(project_dir, &lock)?;
    }

    Ok(lock)
}

#[tauri::command]
pub async fn skill_search(query: String, limit: usize) -> Result<Vec<SkillSearchResult>, String> {
    let results = marketplace::search(&query, limit)
        .await
        .map_err(crate::commands::format_error)?;
    Ok(results
        .into_iter()
        .map(|r| SkillSearchResult {
            id: r.id,
            name: r.name,
            source: r.source,
            installs: r.installs,
        })
        .collect())
}

#[tauri::command]
pub async fn skill_install(
    project_path: String,
    identifier: String,
) -> Result<Vec<String>, String> {
    let project_dir = std::path::Path::new(&project_path);
    let cache = SkillCache::new().map_err(crate::commands::format_error)?;

    let parts: Vec<&str> = identifier.split('/').collect();
    match parts.len() {
        2 => {
            let (owner, repo) = (parts[0], parts[1]);
            let names = marketplace::list_repo_skills(owner, repo)
                .await
                .map_err(crate::commands::format_error)?;
            if names.is_empty() {
                return Err(format!("No skills found in {}/{}", owner, repo));
            }
            let mut installed = Vec::new();
            for name in &names {
                let skill = marketplace::fetch_skill(owner, repo, name)
                    .await
                    .map_err(crate::commands::format_error)?;
                installer::install_skill(project_dir, &skill, &cache)
                    .map_err(crate::commands::format_error)?;
                installed.push(name.clone());
            }
            Ok(installed)
        }
        3 => {
            let (owner, repo, skill_name) = (parts[0], parts[1], parts[2]);
            let skill = marketplace::fetch_skill(owner, repo, skill_name)
                .await
                .map_err(crate::commands::format_error)?;
            installer::install_skill(project_dir, &skill, &cache)
                .map_err(crate::commands::format_error)?;
            Ok(vec![skill_name.to_string()])
        }
        _ => Err(format!(
            "Invalid identifier '{}'. Use 'owner/repo' or 'owner/repo/skill-name'.",
            identifier
        )),
    }
}

#[tauri::command]
pub async fn skill_remove(project_path: String, name: String) -> Result<(), String> {
    let project_dir = std::path::Path::new(&project_path);
    installer::remove_skill(project_dir, &name).map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn skill_update(
    project_path: String,
    name: Option<String>,
) -> Result<Vec<String>, String> {
    let project_dir = std::path::Path::new(&project_path);
    let cache = SkillCache::new().map_err(crate::commands::format_error)?;
    let lock = manifest::load_lock(project_dir).map_err(crate::commands::format_error)?;

    let skills_to_check: Vec<String> = if let Some(ref n) = name {
        if !lock.skills.contains_key(n) {
            return Err(format!("Skill '{}' is not installed.", n));
        }
        vec![n.clone()]
    } else {
        lock.skills.keys().cloned().collect()
    };

    let mut updated = Vec::new();
    for skill_name in &skills_to_check {
        if let Some(installed) = lock.skills.get(skill_name) {
            let new_skill = match &installed.source {
                SkillSource::Bundled => bundled_skills()
                    .into_iter()
                    .find(|s| s.name == *skill_name),
                SkillSource::Github { owner, repo, .. } => {
                    marketplace::fetch_skill(owner, repo, skill_name)
                        .await
                        .ok()
                }
            };

            if let Some(new) = new_skill {
                if new.content_hash != installed.content_hash {
                    installer::install_skill(project_dir, &new, &cache)
                        .map_err(crate::commands::format_error)?;
                    updated.push(skill_name.clone());
                }
            }
        }
    }
    Ok(updated)
}

#[tauri::command]
pub async fn skill_show(project_path: String, name: String) -> Result<SkillDetail, String> {
    let project_dir = std::path::Path::new(&project_path);
    let lock = manifest::load_lock(project_dir).map_err(crate::commands::format_error)?;
    let installed = lock
        .skills
        .get(&name)
        .ok_or_else(|| format!("Skill '{}' is not installed.", name))?;

    let skill_path = project_dir
        .join(".agents/skills")
        .join(&name)
        .join("SKILL.md");
    let content = if skill_path.exists() {
        std::fs::read_to_string(&skill_path).unwrap_or_default()
    } else {
        String::new()
    };

    Ok(SkillDetail {
        name,
        source: installed.source.clone(),
        content_hash: installed.content_hash.clone(),
        installed_at: installed.installed_at.format("%Y-%m-%d %H:%M").to_string(),
        content,
    })
}

#[tauri::command]
pub async fn skill_search_detail(
    source: String,
    name: String,
) -> Result<SkillSearchDetail, String> {
    // source is "owner/repo" format
    let parts: Vec<&str> = source.split('/').collect();
    if parts.len() != 2 {
        return Err(format!(
            "Invalid source '{}'. Expected 'owner/repo'.",
            source
        ));
    }
    let (owner, repo) = (parts[0], parts[1]);
    let skill = marketplace::fetch_skill(owner, repo, &name)
        .await
        .map_err(crate::commands::format_error)?;

    Ok(SkillSearchDetail {
        name: skill.name,
        source,
        description: skill.description,
        content: skill.content,
    })
}

#[tauri::command]
pub async fn skill_check_updates(project_path: String) -> Result<Vec<String>, String> {
    let project_dir = std::path::Path::new(&project_path);
    let bundled = bundled_skills();
    let updates =
        installer::check_updates(project_dir, &bundled).map_err(crate::commands::format_error)?;
    Ok(updates.into_iter().map(|(name, _, _)| name).collect())
}

// ── User-scope skill commands ──────────────────────────────────────────────

#[derive(Serialize)]
pub struct UserSkillInfo {
    pub name: String,
    pub source: SkillSource,
    pub content_hash: String,
    pub installed_at: String,
    pub agents: Vec<String>,
    /// Whether this skill is managed by devflow (true) or discovered externally (false).
    pub managed: bool,
    /// For external skills: which agent this was discovered in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_agent: Option<String>,
    /// For external skills: short description from frontmatter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[tauri::command]
pub async fn user_skill_list() -> Result<Vec<UserSkillInfo>, String> {
    let lock = user_installer::list_user_skills().map_err(crate::commands::format_error)?;
    let agents: Vec<String> = user_installer::agent_symlink_targets()
        .into_iter()
        .map(|(name, _)| name)
        .collect();

    // Collect managed skills
    let mut results: Vec<UserSkillInfo> = lock
        .skills
        .iter()
        .map(|(name, skill)| UserSkillInfo {
            name: name.to_string(),
            source: skill.source.clone(),
            content_hash: skill.content_hash.clone(),
            installed_at: skill.installed_at.format("%Y-%m-%d %H:%M").to_string(),
            agents: agents.clone(),
            managed: true,
            external_agent: None,
            description: None,
        })
        .collect();

    // Discover and merge external skills (read-only, not managed by devflow)
    if let Ok(external) = user_installer::discover_external_skills() {
        let managed_names: std::collections::HashSet<String> =
            results.iter().map(|s| s.name.clone()).collect();
        for ext in external {
            // Skip if already covered by a managed skill (shouldn't happen since
            // discover_external_skills already deduplicates, but be safe)
            if managed_names.contains(&ext.name) {
                continue;
            }
            results.push(UserSkillInfo {
                name: ext.name,
                source: SkillSource::Bundled, // placeholder — external skills don't have a devflow source
                content_hash: ext.content_hash,
                installed_at: String::new(),
                agents: vec![ext.agent.clone()],
                managed: false,
                external_agent: Some(ext.agent),
                description: if ext.description.is_empty() {
                    None
                } else {
                    Some(ext.description)
                },
            });
        }
    }

    Ok(results)
}

#[tauri::command]
pub async fn user_skill_install(identifier: String) -> Result<Vec<String>, String> {
    let cache = SkillCache::new().map_err(crate::commands::format_error)?;

    let parts: Vec<&str> = identifier.split('/').collect();
    match parts.len() {
        2 => {
            let (owner, repo) = (parts[0], parts[1]);
            let names = marketplace::list_repo_skills(owner, repo)
                .await
                .map_err(crate::commands::format_error)?;
            if names.is_empty() {
                return Err(format!("No skills found in {}/{}", owner, repo));
            }
            let mut installed = Vec::new();
            for name in &names {
                let skill = marketplace::fetch_skill(owner, repo, name)
                    .await
                    .map_err(crate::commands::format_error)?;
                user_installer::install_user_skill(&skill, &cache)
                    .map_err(crate::commands::format_error)?;
                installed.push(name.clone());
            }
            Ok(installed)
        }
        3 => {
            let (owner, repo, skill_name) = (parts[0], parts[1], parts[2]);
            let skill = marketplace::fetch_skill(owner, repo, skill_name)
                .await
                .map_err(crate::commands::format_error)?;
            user_installer::install_user_skill(&skill, &cache)
                .map_err(crate::commands::format_error)?;
            Ok(vec![skill_name.to_string()])
        }
        _ => Err(format!(
            "Invalid identifier '{}'. Use 'owner/repo' or 'owner/repo/skill-name'.",
            identifier
        )),
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
        if !lock.skills.contains_key(n) {
            return Err(format!("User skill '{}' is not installed.", n));
        }
        vec![n.clone()]
    } else {
        lock.skills.keys().cloned().collect()
    };

    let mut updated = Vec::new();
    for skill_name in &skills_to_check {
        if let Some(installed) = lock.skills.get(skill_name) {
            let new_skill = match &installed.source {
                SkillSource::Bundled => bundled_skills()
                    .into_iter()
                    .find(|s| s.name == *skill_name),
                SkillSource::Github { owner, repo, .. } => {
                    marketplace::fetch_skill(owner, repo, skill_name)
                        .await
                        .ok()
                }
            };

            if let Some(new) = new_skill {
                if new.content_hash != installed.content_hash {
                    user_installer::install_user_skill(&new, &cache)
                        .map_err(crate::commands::format_error)?;
                    updated.push(skill_name.clone());
                }
            }
        }
    }
    Ok(updated)
}

#[tauri::command]
pub async fn user_skill_show(name: String) -> Result<SkillDetail, String> {
    // First try devflow-managed user skills
    match user_installer::show_user_skill(&name) {
        Ok((installed, content)) => {
            return Ok(SkillDetail {
                name,
                source: installed.source,
                content_hash: installed.content_hash,
                installed_at: installed.installed_at.format("%Y-%m-%d %H:%M").to_string(),
                content,
            });
        }
        Err(_) => {
            // Not a managed skill — try external skills
            if let Ok(external) = user_installer::discover_external_skills() {
                if let Some(ext) = external.into_iter().find(|e| e.name == name) {
                    return Ok(SkillDetail {
                        name: ext.name,
                        source: SkillSource::Bundled, // placeholder for external
                        content_hash: ext.content_hash,
                        installed_at: String::new(),
                        content: ext.content,
                    });
                }
            }
            Err(format!("Skill '{}' not found.", name))
        }
    }
}

#[tauri::command]
pub async fn user_skill_check_updates() -> Result<Vec<String>, String> {
    let bundled = bundled_skills();
    let updates =
        user_installer::check_user_updates(&bundled).map_err(crate::commands::format_error)?;
    Ok(updates.into_iter().map(|(name, _, _)| name).collect())
}

use anyhow::{bail, Result};
use std::path::PathBuf;

use devflow_core::skills::{
    bundled::bundled_skills, cache::SkillCache, installer, manifest, marketplace, user_installer,
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
        super::SkillCommands::List {
            available,
            updates,
            user,
        } => {
            if user {
                return handle_user_list(json_output).await;
            }

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
                println!("  {:<30} {:<15} SOURCE", "NAME", "HASH");
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
                println!("  {:<30} {:<25} INSTALLS", "NAME", "SOURCE");
                for result in &results {
                    let installs = if result.installs >= 1000 {
                        format!("{:.1}K", result.installs as f64 / 1000.0)
                    } else {
                        result.installs.to_string()
                    };
                    println!("  {:<30} {:<25} {}", result.name, result.source, installs,);
                }
                println!("\nRun `devflow skill install <source>/<name>` to install.");
            }
            Ok(())
        }

        super::SkillCommands::Install {
            identifier,
            skill: skill_names,
            user,
        } => {
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
                        if user {
                            user_installer::install_user_skill(&skill, &cache)?;
                        } else {
                            installer::install_skill(&project_dir, &skill, &cache)?;
                        }
                        if !json_output {
                            let scope = if user { " (user)" } else { "" };
                            println!("Installed{}: {}", scope, name);
                        }
                    }

                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string(&serde_json::json!({
                                "installed": names,
                                "scope": if user { "user" } else { "project" },
                            }))?
                        );
                    }
                }
                3 => {
                    // owner/repo/skill-name
                    let owner = parts[0];
                    let repo = parts[1];
                    let skill_name = parts[2];

                    let skill = marketplace::fetch_skill(owner, repo, skill_name).await?;
                    if user {
                        user_installer::install_user_skill(&skill, &cache)?;
                    } else {
                        installer::install_skill(&project_dir, &skill, &cache)?;
                    }

                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string(&serde_json::json!({
                                "installed": [skill_name],
                                "content_hash": &skill.content_hash[..12],
                                "scope": if user { "user" } else { "project" },
                            }))?
                        );
                    } else {
                        let scope = if user { " (user)" } else { "" };
                        println!(
                            "Installed{}: {} ({})",
                            scope,
                            skill_name,
                            &skill.content_hash[..12]
                        );
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

        super::SkillCommands::Remove { name, user } => {
            if user {
                user_installer::remove_user_skill(&name)?;
            } else {
                installer::remove_skill(&project_dir, &name)?;
            }
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "removed": name,
                        "scope": if user { "user" } else { "project" },
                    }))?
                );
            } else {
                let scope = if user { " (user)" } else { "" };
                println!("Removed{}: {}", scope, name);
            }
            Ok(())
        }

        super::SkillCommands::Update { name, check, user } => {
            let cache = SkillCache::new()?;
            let lock = if user {
                user_installer::list_user_skills()?
            } else {
                manifest::load_lock(&project_dir)?
            };

            let skills_to_check: Vec<String> = if let Some(ref n) = name {
                if !lock.skills.contains_key(n) {
                    let scope = if user { "user" } else { "project" };
                    bail!("Skill '{}' is not installed ({} scope).", n, scope);
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
                            bundled_skills().into_iter().find(|s| s.name == *skill_name)
                        }
                        SkillSource::Github { owner, repo, .. } => {
                            marketplace::fetch_skill(owner, repo, skill_name).await.ok()
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
                                if user {
                                    user_installer::install_user_skill(&new, &cache)?;
                                } else {
                                    installer::install_skill(&project_dir, &new, &cache)?;
                                }
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
                        "scope": if user { "user" } else { "project" },
                    }))?
                );
            } else if updated.is_empty() {
                println!("All skills are up to date.");
            } else if check {
                println!(
                    "\n{} update(s) available. Run `devflow skill update{}` to apply.",
                    updated.len(),
                    if user { " --user" } else { "" },
                );
            }
            Ok(())
        }

        super::SkillCommands::Show { name, user } => {
            if user {
                return handle_user_show(&name, json_output);
            }

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
                    println!(
                        "Installed: {}",
                        installed.installed_at.format("%Y-%m-%d %H:%M")
                    );

                    // Try to read content
                    let skill_path = project_dir
                        .join(".claude/skills")
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

// ── User-scope helpers ─────────────────────────────────────────────────────

async fn handle_user_list(json_output: bool) -> Result<()> {
    let lock = user_installer::list_user_skills()?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&lock)?);
    } else if lock.skills.is_empty() {
        println!("No user-scope skills installed.");
        println!("Run `devflow skill install --user <identifier>` to install globally.");
    } else {
        println!("User-scope skills:\n");
        println!("  {:<30} {:<15} SOURCE", "NAME", "HASH");
        for (name, skill) in &lock.skills {
            let source_label = match &skill.source {
                SkillSource::Bundled => "bundled".to_string(),
                SkillSource::Github { owner, repo, .. } => format!("{}/{}", owner, repo),
            };
            println!(
                "  {:<30} {:<15} {}",
                name,
                &skill.content_hash[..12.min(skill.content_hash.len())],
                source_label,
            );
        }

        // Show which agents have symlinks
        let targets = user_installer::agent_symlink_targets();
        if !targets.is_empty() {
            let agents: Vec<&str> = targets.iter().map(|(name, _)| name.as_str()).collect();
            println!("\nSymlinked into: {}", agents.join(", "));
        }
    }
    Ok(())
}

fn handle_user_show(name: &str, json_output: bool) -> Result<()> {
    let (installed, content) = user_installer::show_user_skill(name)?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "name": name,
                "source": installed.source,
                "content_hash": installed.content_hash,
                "installed_at": installed.installed_at.format("%Y-%m-%d %H:%M").to_string(),
                "scope": "user",
            }))?
        );
    } else {
        println!("Skill: {} (user scope)", name);
        println!(
            "Source: {}",
            match &installed.source {
                SkillSource::Bundled => "bundled".to_string(),
                SkillSource::Github { owner, repo, .. } => format!("{}/{}", owner, repo),
            }
        );
        println!("Hash: {}", &installed.content_hash[..12]);
        println!(
            "Installed: {}",
            installed.installed_at.format("%Y-%m-%d %H:%M")
        );

        if !content.is_empty() {
            println!("\n---\n{}", content);
        }
    }
    Ok(())
}

use devflow_core::{config, services, vcs};
use serde::Serialize;

#[derive(Serialize)]
pub struct BranchEntry {
    pub name: String,
    pub is_current: bool,
    pub is_default: bool,
    pub worktree_path: Option<String>,
}

#[derive(Serialize)]
pub struct BranchesResponse {
    pub branches: Vec<BranchEntry>,
    pub current: Option<String>,
}

#[derive(Serialize)]
pub struct OrchestrationResultDto {
    pub service_name: String,
    pub success: bool,
    pub message: String,
}

#[tauri::command]
pub async fn list_branches(project_path: String) -> Result<BranchesResponse, String> {
    let vcs_provider =
        vcs::detect_vcs_provider(&project_path).map_err(|e| e.to_string())?;

    let branches = vcs_provider
        .list_branches()
        .map_err(|e| e.to_string())?;

    let worktrees = vcs_provider
        .list_worktrees()
        .unwrap_or_default();

    let current = vcs_provider
        .current_branch()
        .ok()
        .flatten();

    let entries: Vec<BranchEntry> = branches
        .into_iter()
        .map(|b| {
            let worktree_path = worktrees
                .iter()
                .find(|w| w.branch.as_deref() == Some(&b.name))
                .map(|w| w.path.display().to_string());

            BranchEntry {
                name: b.name,
                is_current: b.is_current,
                is_default: b.is_default,
                worktree_path,
            }
        })
        .collect();

    Ok(BranchesResponse {
        branches: entries,
        current,
    })
}

#[tauri::command]
pub async fn get_connection_info(
    project_path: String,
    branch_name: String,
    service_name: Option<String>,
) -> Result<serde_json::Value, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(|e| e.to_string())?;

    let named_services = config.resolve_services();
    let service_name = service_name.unwrap_or_else(|| "default".to_string());

    if let Some(svc) = named_services
        .iter()
        .find(|s| s.name == service_name)
        .or(named_services.first())
    {
        let provider =
            services::factory::create_provider_from_named_config(&config, svc)
                .await
                .map_err(|e| e.to_string())?;

        let info = provider
            .get_connection_info(&branch_name)
            .await
            .map_err(|e| e.to_string())?;

        serde_json::to_value(&info).map_err(|e| e.to_string())
    } else {
        Err("No services configured".to_string())
    }
}

#[derive(Serialize)]
pub struct CreateBranchResult {
    pub services: Vec<OrchestrationResultDto>,
    pub worktree_path: Option<String>,
}

#[tauri::command]
pub async fn create_branch(
    project_path: String,
    branch_name: String,
    from_branch: Option<String>,
) -> Result<CreateBranchResult, String> {
    let project_dir = std::path::Path::new(&project_path);
    let vcs_provider =
        vcs::detect_vcs_provider(&project_path).map_err(|e| e.to_string())?;

    // Load config if it exists
    let config_path = project_dir.join(".devflow.yml");
    let cfg = if config_path.exists() {
        Some(config::Config::from_file(&config_path).map_err(|e| e.to_string())?)
    } else {
        None
    };

    let worktree_enabled = cfg
        .as_ref()
        .and_then(|c| c.worktree.as_ref())
        .is_some_and(|wt| wt.enabled);

    // Create VCS branch
    vcs_provider
        .create_branch(&branch_name, from_branch.as_deref())
        .map_err(|e| e.to_string())?;

    // Create worktree if enabled
    let mut worktree_path: Option<String> = None;
    if worktree_enabled && vcs_provider.supports_worktrees() {
        // Check if a worktree already exists for this branch
        let existing = vcs_provider
            .worktree_path(&branch_name)
            .unwrap_or(None);

        if let Some(existing_path) = existing {
            worktree_path = Some(existing_path.display().to_string());
        } else {
            // Resolve worktree path from template
            let repo_name = project_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "repo".to_string());
            let wt_config = cfg.as_ref().and_then(|c| c.worktree.as_ref());
            let path_template = wt_config
                .map(|wt| wt.path_template.as_str())
                .unwrap_or("../{repo}.{branch}");
            let wt_path_str = path_template
                .replace("{repo}", &repo_name)
                .replace("{branch}", &branch_name);
            let wt_path = project_dir.join(&wt_path_str);

            // Resolve to absolute path
            let wt_path = if wt_path.is_relative() {
                project_dir.join(&wt_path)
            } else {
                wt_path
            };

            vcs_provider
                .create_worktree(&branch_name, &wt_path)
                .map_err(|e| format!("Failed to create worktree: {}", e))?;

            // Copy configured files from main worktree
            if let Some(ref wt_cfg) = wt_config {
                let main_dir = vcs_provider
                    .main_worktree_dir()
                    .unwrap_or_else(|| project_dir.to_path_buf());

                // Copy explicitly listed files
                for file in &wt_cfg.copy_files {
                    let src = main_dir.join(file);
                    let dst = wt_path.join(file);
                    if src.exists() {
                        if let Some(parent) = dst.parent() {
                            std::fs::create_dir_all(parent).ok();
                        }
                        if let Err(e) = std::fs::copy(&src, &dst) {
                            log::warn!("Failed to copy '{}' to worktree: {}", file, e);
                        }
                    }
                }

                // Copy gitignored files when copy_ignored is enabled
                if wt_cfg.copy_ignored {
                    if let Ok(ignored_files) = vcs_provider.list_ignored_files() {
                        for rel_path in &ignored_files {
                            let src = main_dir.join(rel_path);
                            let dst = wt_path.join(rel_path);
                            if src.exists() && !dst.exists() {
                                if let Some(parent) = dst.parent() {
                                    std::fs::create_dir_all(parent).ok();
                                }
                                if let Err(e) = std::fs::copy(&src, &dst) {
                                    log::warn!(
                                        "Failed to copy ignored file '{}': {}",
                                        rel_path.display(),
                                        e
                                    );
                                }
                            }
                        }
                    }
                }
            }

            let resolved = std::fs::canonicalize(&wt_path)
                .unwrap_or(wt_path);
            worktree_path = Some(resolved.display().to_string());
        }
    }

    // Orchestrate service branches if config exists
    let service_results = if let Some(ref cfg) = cfg {
        let results = services::factory::orchestrate_create(cfg, &branch_name, from_branch.as_deref())
            .await
            .map_err(|e| e.to_string())?;
        results
            .into_iter()
            .map(|r| OrchestrationResultDto {
                service_name: r.service_name,
                success: r.success,
                message: r.message,
            })
            .collect()
    } else {
        vec![]
    };

    Ok(CreateBranchResult {
        services: service_results,
        worktree_path,
    })
}

#[tauri::command]
pub async fn switch_branch(
    project_path: String,
    branch_name: String,
) -> Result<Vec<OrchestrationResultDto>, String> {
    let vcs_provider =
        vcs::detect_vcs_provider(&project_path).map_err(|e| e.to_string())?;

    // Checkout VCS branch
    vcs_provider
        .checkout_branch(&branch_name)
        .map_err(|e| e.to_string())?;

    // Orchestrate service switch if config exists
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    if config_path.exists() {
        let cfg = config::Config::from_file(&config_path).map_err(|e| e.to_string())?;
        let results = services::factory::orchestrate_switch(&cfg, &branch_name, None)
            .await
            .map_err(|e| e.to_string())?;
        Ok(results
            .into_iter()
            .map(|r| OrchestrationResultDto {
                service_name: r.service_name,
                success: r.success,
                message: r.message,
            })
            .collect())
    } else {
        Ok(vec![])
    }
}

#[tauri::command]
pub async fn delete_branch(
    project_path: String,
    branch_name: String,
) -> Result<Vec<OrchestrationResultDto>, String> {
    let vcs_provider =
        vcs::detect_vcs_provider(&project_path).map_err(|e| e.to_string())?;

    // 1. Remove worktree if one exists for this branch
    if let Ok(Some(wt_path)) = vcs_provider.worktree_path(&branch_name) {
        if let Err(e) = vcs_provider.remove_worktree(&wt_path) {
            log::warn!(
                "Failed to remove worktree via VCS, falling back to fs removal: {}",
                e
            );
            if wt_path.exists() {
                std::fs::remove_dir_all(&wt_path)
                    .map_err(|e| format!("Failed to remove worktree directory: {}", e))?;
            }
        }
    }

    // 2. Orchestrate service branch deletion if config exists
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let mut results = vec![];
    if config_path.exists() {
        let cfg = config::Config::from_file(&config_path).map_err(|e| e.to_string())?;
        results = services::factory::orchestrate_delete(&cfg, &branch_name)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|r| OrchestrationResultDto {
                service_name: r.service_name,
                success: r.success,
                message: r.message,
            })
            .collect();
    }

    // 3. Delete VCS branch
    vcs_provider
        .delete_branch(&branch_name)
        .map_err(|e| e.to_string())?;

    Ok(results)
}

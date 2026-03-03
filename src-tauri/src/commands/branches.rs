use devflow_core::state::{DevflowBranch, LocalStateManager};
use devflow_core::{config, services, vcs};
use serde::Serialize;

#[derive(Serialize)]
pub struct BranchEntry {
    pub name: String,
    pub is_current: bool,
    pub is_default: bool,
    pub worktree_path: Option<String>,
    pub cow_used: bool,
    pub parent: Option<String>,
    pub created_at: Option<String>,
    pub agent_tool: Option<String>,
    pub agent_status: Option<String>,
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
    let project_dir = std::path::Path::new(&project_path);
    let config_path = project_dir.join(".devflow.yml");

    // Load config for main_branch / normalization
    let cfg = if config_path.exists() {
        config::Config::from_file(&config_path).ok()
    } else {
        None
    };

    let main_branch = cfg
        .as_ref()
        .map(|c| c.git.main_branch.clone())
        .unwrap_or_else(|| "main".to_string());

    // Read devflow branch registry
    let mut state_mgr = LocalStateManager::new().map_err(|e| e.to_string())?;

    let devflow_branches = state_mgr
        .get_or_init_branches_by_dir(project_dir, &main_branch)
        .map_err(|e| e.to_string())?;

    // Determine current git branch to find active devflow branch
    let vcs_provider = vcs::detect_vcs_provider(&project_path).ok();
    let current_vcs_branch = vcs_provider
        .as_ref()
        .and_then(|v| v.current_branch().ok().flatten());

    // Normalize the VCS branch name for matching
    let normalized_current = current_vcs_branch.as_deref().map(|b| {
        cfg.as_ref()
            .map(|c| c.get_normalized_branch_name(b))
            .unwrap_or_else(|| b.to_string())
    });

    // Get worktrees for enrichment
    let worktrees = vcs_provider
        .as_ref()
        .and_then(|v| v.list_worktrees().ok())
        .unwrap_or_default();

    let entries: Vec<BranchEntry> = devflow_branches
        .into_iter()
        .map(|b| {
            // Check if this is the current branch
            let is_current = normalized_current
                .as_deref()
                .map(|cur| cur == b.name)
                .unwrap_or(false);

            let is_default = b.name == main_branch;

            // Prefer worktree_path from VCS if available, fall back to registry
            let worktree_path = worktrees
                .iter()
                .find(|w| w.branch.as_deref() == Some(&b.name))
                .map(|w| w.path.display().to_string())
                .or(b.worktree_path);

            BranchEntry {
                name: b.name,
                is_current,
                is_default,
                worktree_path,
                cow_used: b.cow_used,
                parent: b.parent,
                created_at: Some(b.created_at.format("%Y-%m-%d %H:%M").to_string()),
                agent_tool: b.agent_tool,
                agent_status: b.agent_status,
            }
        })
        .collect();

    let current = entries
        .iter()
        .find(|e| e.is_current)
        .map(|e| e.name.clone());

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
    let config =
        devflow_core::config::Config::from_file(&config_path).map_err(|e| e.to_string())?;

    let named_services = config.resolve_services();
    let service_name = service_name.unwrap_or_else(|| "default".to_string());

    if let Some(svc) = named_services
        .iter()
        .find(|s| s.name == service_name)
        .or(named_services.first())
    {
        let provider = services::factory::create_provider_from_named_config(&config, svc)
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
    pub cow_used: bool,
}

#[tauri::command]
pub async fn create_branch(
    project_path: String,
    branch_name: String,
    from_branch: Option<String>,
) -> Result<CreateBranchResult, String> {
    let project_dir = std::path::Path::new(&project_path);
    let vcs_provider = vcs::detect_vcs_provider(&project_path).map_err(|e| e.to_string())?;

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

    // Normalize the branch name
    let normalized_name = cfg
        .as_ref()
        .map(|c| c.get_normalized_branch_name(&branch_name))
        .unwrap_or_else(|| branch_name.clone());

    let normalized_parent = from_branch.as_deref().map(|fb| {
        cfg.as_ref()
            .map(|c| c.get_normalized_branch_name(fb))
            .unwrap_or_else(|| fb.to_string())
    });

    // Create VCS branch
    vcs_provider
        .create_branch(&branch_name, from_branch.as_deref())
        .map_err(|e| e.to_string())?;

    // Create worktree if enabled
    let mut worktree_path: Option<String> = None;
    let mut cow_used = false;
    if worktree_enabled && vcs_provider.supports_worktrees() {
        // Check if a worktree already exists for this branch
        let existing = vcs_provider.worktree_path(&branch_name).unwrap_or(None);

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

            let wt_result = vcs_provider
                .create_worktree(&branch_name, &wt_path)
                .map_err(|e| format!("Failed to create worktree: {}", e))?;
            cow_used = wt_result.cow_used;

            // Copy configured files from main worktree
            if let Some(wt_cfg) = wt_config {
                let main_dir = vcs_provider
                    .main_worktree_dir()
                    .unwrap_or_else(|| project_dir.to_path_buf());

                // Copy explicitly listed files.
                // When CoW was used, these already exist as clones — overwrite with
                // independent copies so they can diverge between branches.
                for file in &wt_cfg.copy_files {
                    let src = main_dir.join(file);
                    let dst = wt_path.join(file);
                    if src.exists() {
                        if let Some(parent) = dst.parent() {
                            std::fs::create_dir_all(parent).ok();
                        }
                        // Remove existing clone first so the new copy is independent
                        if cow_used && dst.exists() {
                            let _ = std::fs::remove_file(&dst);
                        }
                        if let Err(e) = std::fs::copy(&src, &dst) {
                            log::warn!("Failed to copy '{}' to worktree: {}", file, e);
                        }
                    }
                }

                // Copy gitignored files — skip when CoW already cloned everything
                if !cow_used && wt_cfg.copy_ignored {
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

            let resolved = std::fs::canonicalize(&wt_path).unwrap_or(wt_path);
            worktree_path = Some(resolved.display().to_string());
        }
    }

    // Orchestrate service branches if config exists
    let service_results = if let Some(ref cfg) = cfg {
        let results =
            services::factory::orchestrate_create(cfg, &branch_name, from_branch.as_deref())
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

    // Register the new branch in devflow state
    if let Ok(mut state_mgr) = LocalStateManager::new() {
        let final_cow_used = if cow_used {
            true
        } else {
            state_mgr
                .get_branch_by_dir(project_dir, &normalized_name)
                .map(|existing| existing.cow_used)
                .unwrap_or(false)
        };

        let branch = DevflowBranch {
            name: normalized_name,
            parent: normalized_parent,
            worktree_path: worktree_path.clone(),
            created_at: chrono::Utc::now(),
            cow_used: final_cow_used,
            agent_tool: None,
            agent_status: None,
            agent_started_at: None,
        };
        if let Err(e) = state_mgr.register_branch_by_dir(project_dir, branch) {
            log::warn!("Failed to register branch in devflow state: {}", e);
        }
    }

    Ok(CreateBranchResult {
        services: service_results,
        worktree_path,
        cow_used,
    })
}

#[tauri::command]
pub async fn switch_branch(
    project_path: String,
    branch_name: String,
) -> Result<Vec<OrchestrationResultDto>, String> {
    let vcs_provider = vcs::detect_vcs_provider(&project_path).map_err(|e| e.to_string())?;

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
    let project_dir = std::path::Path::new(&project_path);
    let vcs_provider = vcs::detect_vcs_provider(&project_path).map_err(|e| e.to_string())?;

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
    let config_path = project_dir.join(".devflow.yml");
    let mut results = vec![];
    if config_path.exists() {
        let cfg = config::Config::from_file(&config_path).map_err(|e| e.to_string())?;

        // Normalize the branch name for unregistration
        let normalized = cfg.get_normalized_branch_name(&branch_name);

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

        // 3. Unregister from devflow state
        if let Ok(mut state_mgr) = LocalStateManager::new() {
            if let Err(e) = state_mgr.unregister_branch_by_dir(project_dir, &normalized) {
                log::warn!("Failed to unregister branch from devflow state: {}", e);
            }
        }
    } else {
        // No config — still unregister from state using the raw name
        if let Ok(mut state_mgr) = LocalStateManager::new() {
            if let Err(e) = state_mgr.unregister_branch_by_dir(project_dir, &branch_name) {
                log::warn!("Failed to unregister branch from devflow state: {}", e);
            }
        }
    }

    // 4. Delete VCS branch
    vcs_provider
        .delete_branch(&branch_name)
        .map_err(|e| e.to_string())?;

    Ok(results)
}

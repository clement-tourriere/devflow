use crate::state::{AppState, ProjectEntry};
use devflow_core::services;
use serde::Serialize;
use tauri::State;

#[derive(Serialize)]
pub struct ProjectDetail {
    pub name: String,
    pub path: String,
    pub has_config: bool,
    pub current_branch: Option<String>,
    pub service_count: usize,
    pub branch_count: usize,
    pub worktree_enabled: bool,
}

#[tauri::command]
pub async fn list_projects(state: State<'_, AppState>) -> Result<Vec<ProjectEntry>, String> {
    let settings = state.settings.read().await;
    Ok(settings.projects.clone())
}

#[tauri::command]
pub async fn add_project(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    name: Option<String>,
) -> Result<ProjectEntry, String> {
    let abs_path = std::path::Path::new(&path)
        .canonicalize()
        .map_err(|e| format!("Invalid path: {}", e))?;

    let name = name.unwrap_or_else(|| {
        abs_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    });

    let entry = ProjectEntry {
        path: abs_path.display().to_string(),
        name,
    };

    let mut settings = state.settings.write().await;
    if !settings.projects.iter().any(|p| p.path == entry.path) {
        settings.projects.push(entry.clone());
        settings.save().map_err(|e| e.to_string())?;
    }

    // Update tray menu to include new project
    crate::update_tray_menu(&app);

    Ok(entry)
}

#[tauri::command]
pub async fn remove_project(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    let mut settings = state.settings.write().await;
    settings.projects.retain(|p| p.path != path);
    settings.save().map_err(|e| e.to_string())?;

    // Update tray menu to remove project
    crate::update_tray_menu(&app);

    Ok(())
}

#[tauri::command]
pub async fn init_project(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    name: Option<String>,
) -> Result<ProjectEntry, String> {
    let dir = std::path::Path::new(&path);
    if !dir.is_dir() {
        return Err(format!("Not a directory: {}", path));
    }

    let abs_path = dir
        .canonicalize()
        .map_err(|e| format!("Invalid path: {}", e))?;

    // Initialize VCS if not already a repo
    if devflow_core::vcs::detect_vcs_kind(&abs_path).is_none() {
        devflow_core::vcs::init_vcs_repository(&abs_path, None, false)
            .map_err(|e| format!("Failed to init VCS: {}", e))?;
    } else {
        // VCS already exists — ensure it has at least one commit so the
        // default branch is materialised and `list_branches` returns it.
        if let Ok(vcs) = devflow_core::vcs::detect_vcs_provider(&abs_path) {
            let _ = vcs.ensure_initial_commit();
        }
    }

    // Derive project name first so we can embed it in config
    let project_name = name.unwrap_or_else(|| {
        abs_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    });

    // Create default .devflow.yml if it doesn't exist
    let config_path = abs_path.join(".devflow.yml");
    if !config_path.exists() {
        let mut config = devflow_core::config::Config::default();
        config.name = Some(project_name.clone());
        config
            .save_to_file(&config_path)
            .map_err(|e| format!("Failed to create config: {}", e))?;
    }

    let entry = ProjectEntry {
        path: abs_path.display().to_string(),
        name: project_name,
    };

    let mut settings = state.settings.write().await;
    if !settings.projects.iter().any(|p| p.path == entry.path) {
        settings.projects.push(entry.clone());
        settings.save().map_err(|e| e.to_string())?;
    }

    crate::update_tray_menu(&app);

    Ok(entry)
}

#[tauri::command]
pub async fn get_project_detail(project_path: String) -> Result<ProjectDetail, String> {
    let path = std::path::Path::new(&project_path);
    let config_path = path.join(".devflow.yml");
    let has_config = config_path.exists();

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let vcs = devflow_core::vcs::detect_vcs_provider(&project_path).ok();

    let current_branch = vcs
        .as_ref()
        .and_then(|v| v.current_branch().ok().flatten());

    let branch_count = vcs
        .as_ref()
        .and_then(|v| v.list_branches().ok())
        .map(|b| b.len())
        .unwrap_or(0);

    let config = if has_config {
        devflow_core::config::Config::from_file(&config_path).ok()
    } else {
        None
    };

    let service_count = config
        .as_ref()
        .and_then(|c| c.services.as_ref().map(|s| s.len()))
        .unwrap_or(0);

    let worktree_enabled = config
        .as_ref()
        .and_then(|c| c.worktree.as_ref())
        .map(|w| w.enabled)
        .unwrap_or(false);

    Ok(ProjectDetail {
        name,
        path: project_path,
        has_config,
        current_branch,
        service_count,
        branch_count,
        worktree_enabled,
    })
}

#[derive(Serialize)]
pub struct ServiceDestroyResult {
    pub name: String,
    pub success: bool,
    pub branches_destroyed: Vec<String>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct DestroyResult {
    pub services_destroyed: Vec<ServiceDestroyResult>,
    pub worktrees_removed: usize,
    pub hooks_uninstalled: bool,
    pub config_deleted: bool,
}

#[tauri::command]
pub async fn destroy_project(project_path: String) -> Result<DestroyResult, String> {
    let path = std::path::Path::new(&project_path);
    let config_path = path.join(".devflow.yml");
    let local_config_path = path.join(".devflow.local.yml");

    let mut config = if config_path.exists() {
        devflow_core::config::Config::from_file(&config_path)
            .map_err(|e| e.to_string())?
    } else {
        devflow_core::config::Config::default()
    };

    // Inject services from local state
    if let Ok(state_mgr) = devflow_core::state::LocalStateManager::new() {
        if let Some(state_services) = state_mgr.get_services(&config_path) {
            config.services = Some(state_services);
        }
    }

    let service_configs = config.resolve_services();
    let vcs_repo = devflow_core::vcs::detect_vcs_provider(&project_path).ok();

    let mut services_destroyed = Vec::new();
    let mut worktrees_removed = 0usize;
    let mut hooks_uninstalled = false;
    let mut config_deleted = false;

    // 1. Destroy all service data
    for svc_config in &service_configs {
        match services::factory::create_provider_from_named_config(&config, svc_config).await {
            Ok(provider) => {
                if provider.supports_destroy() {
                    match provider.destroy_project().await {
                        Ok(branches) => {
                            services_destroyed.push(ServiceDestroyResult {
                                name: svc_config.name.clone(),
                                success: true,
                                branches_destroyed: branches,
                                error: None,
                            });
                        }
                        Err(e) => {
                            services_destroyed.push(ServiceDestroyResult {
                                name: svc_config.name.clone(),
                                success: false,
                                branches_destroyed: Vec::new(),
                                error: Some(e.to_string()),
                            });
                        }
                    }
                } else {
                    // Fallback: delete all branches individually
                    match provider.list_branches().await {
                        Ok(branches) => {
                            let mut deleted = Vec::new();
                            for branch in &branches {
                                if provider.delete_branch(&branch.name).await.is_ok() {
                                    deleted.push(branch.name.clone());
                                }
                            }
                            services_destroyed.push(ServiceDestroyResult {
                                name: svc_config.name.clone(),
                                success: true,
                                branches_destroyed: deleted,
                                error: None,
                            });
                        }
                        Err(e) => {
                            services_destroyed.push(ServiceDestroyResult {
                                name: svc_config.name.clone(),
                                success: false,
                                branches_destroyed: Vec::new(),
                                error: Some(e.to_string()),
                            });
                        }
                    }
                }
            }
            Err(e) => {
                services_destroyed.push(ServiceDestroyResult {
                    name: svc_config.name.clone(),
                    success: false,
                    branches_destroyed: Vec::new(),
                    error: Some(e.to_string()),
                });
            }
        }
    }

    // 2. Remove worktrees
    if let Some(ref repo) = vcs_repo {
        if let Ok(worktrees) = repo.list_worktrees() {
            for wt in worktrees.iter().filter(|wt| !wt.is_main) {
                if repo.remove_worktree(&wt.path).is_ok() {
                    worktrees_removed += 1;
                } else if wt.path.exists() {
                    if std::fs::remove_dir_all(&wt.path).is_ok() {
                        worktrees_removed += 1;
                    }
                }
            }
        }
    }

    // 3. Uninstall VCS hooks
    if let Some(ref repo) = vcs_repo {
        if repo.uninstall_hooks().is_ok() {
            hooks_uninstalled = true;
        }
    }

    // 4. Clear local state
    if let Ok(mut state_mgr) = devflow_core::state::LocalStateManager::new() {
        let _ = state_mgr.remove_project(&config_path);
    }

    // 5. Clear hook approvals
    if let Ok(state_mgr) = devflow_core::state::LocalStateManager::new() {
        if let Some(project_key) = state_mgr.get_project_key_for(&config_path) {
            if let Ok(mut store) = devflow_core::hooks::approval::ApprovalStore::load() {
                let _ = store.clear_project(&project_key);
            }
        }
    }

    // 6. Delete config files
    if config_path.exists() {
        if std::fs::remove_file(&config_path).is_ok() {
            config_deleted = true;
        }
    }
    if local_config_path.exists() {
        let _ = std::fs::remove_file(&local_config_path);
    }

    Ok(DestroyResult {
        services_destroyed,
        worktrees_removed,
        hooks_uninstalled,
        config_deleted,
    })
}

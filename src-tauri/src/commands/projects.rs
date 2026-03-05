use crate::state::{AppState, ProjectEntry};
use devflow_core::services;
use devflow_core::services::orphan::{self, OrphanSource};
use serde::Serialize;
use tauri::State;

#[derive(Serialize)]
pub struct ProjectDetail {
    pub name: String,
    pub path: String,
    pub has_config: bool,
    pub current_workspace: Option<String>,
    pub service_count: usize,
    pub workspace_count: usize,
    pub hook_count: usize,
    pub worktree_enabled: bool,
    pub worktree_copy_files: Vec<String>,
    pub worktree_copy_ignored: bool,
    pub vcs_type: Option<String>,
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

    let explicit_name = name.filter(|n| !n.trim().is_empty());

    let config_name = {
        let config_path = abs_path.join(".devflow.yml");
        if config_path.exists() {
            devflow_core::config::Config::from_file(&config_path)
                .ok()
                .and_then(|c| c.name)
                .filter(|n| !n.trim().is_empty())
        } else {
            None
        }
    };

    let fallback_name = abs_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let name = explicit_name
        .clone()
        .or(config_name)
        .unwrap_or(fallback_name);

    // If user explicitly provided a project name, persist it into config when available.
    if let Some(explicit) = explicit_name {
        let config_path = abs_path.join(".devflow.yml");
        if config_path.exists() {
            if let Ok(mut config) = devflow_core::config::Config::from_file(&config_path) {
                if config.name.as_ref() != Some(&explicit) {
                    config.name = Some(explicit);
                    let _ = config.save_to_file(&config_path);
                }
            }
        }
    }

    let entry = ProjectEntry {
        path: abs_path.display().to_string(),
        name,
    };

    let mut settings = state.settings.write().await;
    if !settings.projects.iter().any(|p| p.path == entry.path) {
        settings.projects.push(entry.clone());
        settings.save().map_err(crate::commands::format_error)?;
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
    settings.save().map_err(crate::commands::format_error)?;

    // Update tray menu to remove project
    crate::update_tray_menu(&app);

    Ok(())
}

/// Information about VCS availability for a directory.
#[derive(Serialize)]
pub struct VcsInfo {
    /// VCS already present in the directory (e.g. "git", "jj"), or null.
    pub existing_vcs: Option<String>,
    /// VCS tools available on the system.
    pub available_tools: Vec<String>,
}

#[tauri::command]
pub async fn detect_vcs_info(path: String) -> Result<VcsInfo, String> {
    let dir = std::path::Path::new(&path);
    let existing_vcs = devflow_core::vcs::detect_vcs_kind(dir).map(|k| k.to_string());
    let available_tools = devflow_core::vcs::available_vcs_tools()
        .into_iter()
        .map(|k| k.to_string())
        .collect();
    Ok(VcsInfo {
        existing_vcs,
        available_tools,
    })
}

#[tauri::command]
pub async fn init_project(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    name: Option<String>,
    vcs_preference: Option<String>,
    worktree_enabled: Option<bool>,
) -> Result<ProjectEntry, String> {
    let dir = std::path::Path::new(&path);
    if !dir.is_dir() {
        return Err(format!("Not a directory: {}", path));
    }

    let abs_path = dir
        .canonicalize()
        .map_err(|e| format!("Invalid path: {}", e))?;

    let explicit_name = name.filter(|n| !n.trim().is_empty());

    // Initialize VCS if not already a repo
    if devflow_core::vcs::detect_vcs_kind(&abs_path).is_none() {
        let preference = vcs_preference.as_deref().and_then(|s| match s {
            "git" => Some(devflow_core::vcs::VcsKind::Git),
            "jj" => Some(devflow_core::vcs::VcsKind::Jj),
            _ => None,
        });
        devflow_core::vcs::init_vcs_repository(&abs_path, preference, false)
            .map_err(|e| format!("Failed to init VCS: {}", e))?;
    } else {
        // VCS already exists — ensure it has at least one commit so the
        // default workspace is materialised and `list_workspaces` returns it.
        if let Ok(vcs) = devflow_core::vcs::detect_vcs_provider(&abs_path) {
            let _ = vcs.ensure_initial_commit();
        }
    }

    // Derive project name: explicit arg -> existing config name -> directory name
    let existing_config_name = {
        let config_path = abs_path.join(".devflow.yml");
        if config_path.exists() {
            devflow_core::config::Config::from_file(&config_path)
                .ok()
                .and_then(|c| c.name)
                .filter(|n| !n.trim().is_empty())
        } else {
            None
        }
    };

    let fallback_name = abs_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let project_name = explicit_name
        .clone()
        .or(existing_config_name)
        .unwrap_or(fallback_name);

    // Create default .devflow.yml if it doesn't exist
    let config_path = abs_path.join(".devflow.yml");
    if !config_path.exists() {
        let mut config = devflow_core::config::Config::default();
        config.name = Some(project_name.clone());
        // Enable worktrees based on user selection (defaults to enabled)
        if worktree_enabled.unwrap_or(true) {
            config.worktree = Some(devflow_core::config::WorktreeConfig::recommended_default());
        }
        config
            .save_to_file(&config_path)
            .map_err(|e| format!("Failed to create config: {}", e))?;
    } else if let Ok(mut config) = devflow_core::config::Config::from_file(&config_path) {
        let should_set_name =
            explicit_name.is_some() || config.name.as_ref().is_none_or(|n| n.trim().is_empty());
        if should_set_name {
            config.name = Some(project_name.clone());
            config
                .save_to_file(&config_path)
                .map_err(|e| format!("Failed to update config: {}", e))?;
        }
    }

    let entry = ProjectEntry {
        path: abs_path.display().to_string(),
        name: project_name,
    };

    let mut settings = state.settings.write().await;
    if !settings.projects.iter().any(|p| p.path == entry.path) {
        settings.projects.push(entry.clone());
        settings.save().map_err(crate::commands::format_error)?;
    }

    crate::update_tray_menu(&app);

    Ok(entry)
}

/// Unified project add/init command.
///
/// Works for both new and existing projects:
/// 1. Initializes VCS if missing
/// 2. Creates `.devflow.yml` if missing (with optional worktree config)
/// 3. Adds worktree config to existing `.devflow.yml` if requested and missing
/// 4. Registers default devflow workspace
/// 5. Registers project in GUI settings
#[tauri::command]
pub async fn add_or_init_project(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    path: String,
    name: Option<String>,
    vcs_preference: Option<String>,
    worktree_enabled: Option<bool>,
) -> Result<ProjectEntry, String> {
    let dir = std::path::Path::new(&path);
    if !dir.is_dir() {
        return Err(format!("Not a directory: {}", path));
    }

    let abs_path = dir
        .canonicalize()
        .map_err(|e| format!("Invalid path: {}", e))?;

    let explicit_name = name.filter(|n| !n.trim().is_empty());

    // Initialize VCS if not already a repo
    if devflow_core::vcs::detect_vcs_kind(&abs_path).is_none() {
        let preference = vcs_preference.as_deref().and_then(|s| match s {
            "git" => Some(devflow_core::vcs::VcsKind::Git),
            "jj" => Some(devflow_core::vcs::VcsKind::Jj),
            _ => None,
        });
        devflow_core::vcs::init_vcs_repository(&abs_path, preference, false)
            .map_err(|e| format!("Failed to init VCS: {}", e))?;
    } else {
        // VCS already exists — ensure it has at least one commit
        if let Ok(vcs) = devflow_core::vcs::detect_vcs_provider(&abs_path) {
            let _ = vcs.ensure_initial_commit();
        }
    }

    // Derive project name: explicit arg -> existing config name -> directory name
    let existing_config_name = {
        let config_path = abs_path.join(".devflow.yml");
        if config_path.exists() {
            devflow_core::config::Config::from_file(&config_path)
                .ok()
                .and_then(|c| c.name)
                .filter(|n| !n.trim().is_empty())
        } else {
            None
        }
    };

    let fallback_name = abs_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let project_name = explicit_name
        .clone()
        .or(existing_config_name)
        .unwrap_or(fallback_name);

    let config_path = abs_path.join(".devflow.yml");
    let wt_enabled = worktree_enabled.unwrap_or(true);

    if !config_path.exists() {
        // Create a new config
        let mut config = devflow_core::config::Config::default();
        config.name = Some(project_name.clone());
        if wt_enabled {
            config.worktree = Some(devflow_core::config::WorktreeConfig::recommended_default());
        }
        config
            .save_to_file(&config_path)
            .map_err(|e| format!("Failed to create config: {}", e))?;
    } else if let Ok(mut config) = devflow_core::config::Config::from_file(&config_path) {
        // Config exists — keep name canonical and optionally add worktree settings.
        let mut changed = false;
        if explicit_name.is_some() || config.name.as_ref().is_none_or(|n| n.trim().is_empty()) {
            config.name = Some(project_name.clone());
            changed = true;
        }
        if wt_enabled && config.worktree.is_none() {
            config.worktree = Some(devflow_core::config::WorktreeConfig::recommended_default());
            changed = true;
        }
        if changed {
            config
                .save_to_file(&config_path)
                .map_err(|e| format!("Failed to update config: {}", e))?;
        }
    }

    // Register default devflow workspace
    let main_workspace = if config_path.exists() {
        devflow_core::config::Config::from_file(&config_path)
            .ok()
            .map(|c| c.git.main_workspace.clone())
            .unwrap_or_else(|| "main".to_string())
    } else {
        "main".to_string()
    };

    if let Ok(mut state_mgr) = devflow_core::state::LocalStateManager::new() {
        if let Err(e) = state_mgr.ensure_default_workspace(&abs_path, &main_workspace) {
            log::warn!("Failed to register default workspace: {}", e);
        }
    }

    let entry = ProjectEntry {
        path: abs_path.display().to_string(),
        name: project_name,
    };

    let mut settings = state.settings.write().await;
    if !settings.projects.iter().any(|p| p.path == entry.path) {
        settings.projects.push(entry.clone());
        settings.save().map_err(crate::commands::format_error)?;
    }

    crate::update_tray_menu(&app);

    Ok(entry)
}

#[tauri::command]
pub async fn get_project_detail(
    state: State<'_, AppState>,
    project_path: String,
) -> Result<ProjectDetail, String> {
    let path = std::path::Path::new(&project_path);
    let config_path = path.join(".devflow.yml");
    let has_config = config_path.exists();

    let vcs = devflow_core::vcs::detect_vcs_provider(&project_path).ok();
    let vcs_type = vcs.as_ref().map(|v| v.provider_name().to_string());

    let config = if has_config {
        devflow_core::config::Config::from_file(&config_path).ok()
    } else {
        None
    };

    // Derive current devflow workspace: VCS workspace → normalize → look up in registry
    let vcs_branch = vcs
        .as_ref()
        .and_then(|v| v.current_workspace().ok().flatten());

    let normalized_branch = vcs_branch.as_deref().map(|b| {
        config
            .as_ref()
            .map(|c| c.get_normalized_workspace_name(b))
            .unwrap_or_else(|| b.to_string())
    });

    // Use devflow registry for workspace count
    let workspace_count = if let Ok(state_mgr) = devflow_core::state::LocalStateManager::new() {
        let workspaces = state_mgr.get_workspaces_by_dir(path);
        workspaces.len()
    } else {
        0
    };

    let current_workspace = normalized_branch;

    let service_count = config
        .as_ref()
        .and_then(|c| c.services.as_ref().map(|s| s.len()))
        .unwrap_or(0);

    let hook_count = config
        .as_ref()
        .and_then(|c| c.hooks.as_ref())
        .map(|h| h.values().map(|phase| phase.len()).sum())
        .unwrap_or(0);

    let worktree_enabled = config
        .as_ref()
        .and_then(|c| c.worktree.as_ref())
        .map(|w| w.enabled)
        .unwrap_or(false);

    let worktree_copy_files: Vec<String> = config
        .as_ref()
        .and_then(|c| c.worktree.as_ref())
        .map(|w| w.copy_files.clone())
        .unwrap_or_default();

    let worktree_copy_ignored: bool = config
        .as_ref()
        .and_then(|c| c.worktree.as_ref())
        .map(|w| w.copy_ignored)
        .unwrap_or(false);

    let config_name = config
        .as_ref()
        .and_then(|c| c.name.clone())
        .filter(|n| !n.trim().is_empty());

    let settings_name = {
        let canonical_input = path.canonicalize().ok();
        let settings = state.settings.read().await;
        settings
            .projects
            .iter()
            .find(|p| {
                if p.path == project_path {
                    return true;
                }
                if let Some(ref wanted) = canonical_input {
                    return std::path::Path::new(&p.path).canonicalize().ok().as_ref()
                        == Some(wanted);
                }
                false
            })
            .map(|p| p.name.clone())
            .filter(|n| !n.trim().is_empty())
    };

    let fallback_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let name = config_name.or(settings_name).unwrap_or(fallback_name);

    Ok(ProjectDetail {
        name,
        path: project_path,
        has_config,
        current_workspace,
        service_count,
        workspace_count,
        hook_count,
        worktree_enabled,
        worktree_copy_files,
        worktree_copy_ignored,
        vcs_type,
    })
}

#[derive(Serialize)]
pub struct ServiceDestroyResult {
    pub name: String,
    pub success: bool,
    pub workspaces_destroyed: Vec<String>,
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
            .map_err(crate::commands::format_error)?
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
                        Ok(workspaces) => {
                            services_destroyed.push(ServiceDestroyResult {
                                name: svc_config.name.clone(),
                                success: true,
                                workspaces_destroyed: workspaces,
                                error: None,
                            });
                        }
                        Err(e) => {
                            services_destroyed.push(ServiceDestroyResult {
                                name: svc_config.name.clone(),
                                success: false,
                                workspaces_destroyed: Vec::new(),
                                error: Some(e.to_string()),
                            });
                        }
                    }
                } else {
                    // Fallback: delete all workspaces individually
                    match provider.list_workspaces().await {
                        Ok(workspaces) => {
                            let mut deleted = Vec::new();
                            for workspace in &workspaces {
                                if provider.delete_workspace(&workspace.name).await.is_ok() {
                                    deleted.push(workspace.name.clone());
                                }
                            }
                            services_destroyed.push(ServiceDestroyResult {
                                name: svc_config.name.clone(),
                                success: true,
                                workspaces_destroyed: deleted,
                                error: None,
                            });
                        }
                        Err(e) => {
                            services_destroyed.push(ServiceDestroyResult {
                                name: svc_config.name.clone(),
                                success: false,
                                workspaces_destroyed: Vec::new(),
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
                    workspaces_destroyed: Vec::new(),
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

// ── Orphan detection and cleanup ────────────────────────────────────

#[derive(Serialize)]
pub struct OrphanProjectEntry {
    pub project_name: String,
    pub project_path: Option<String>,
    pub sources: Vec<String>,
    pub sqlite_project_id: Option<String>,
    pub sqlite_workspace_count: usize,
    pub container_names: Vec<String>,
    pub local_state_service_count: usize,
    pub local_state_workspace_count: usize,
}

#[derive(Serialize)]
pub struct OrphanCleanupResult {
    pub project_name: String,
    pub containers_removed: usize,
    pub sqlite_rows_deleted: bool,
    pub local_state_cleared: bool,
    pub data_dirs_removed: usize,
    pub errors: Vec<String>,
}

#[tauri::command]
pub async fn detect_orphan_projects() -> Result<Vec<OrphanProjectEntry>, String> {
    let orphans = orphan::detect_orphans()
        .await
        .map_err(|e| format!("Failed to detect orphans: {}", e))?;

    Ok(orphans
        .into_iter()
        .map(|o| OrphanProjectEntry {
            project_name: o.project_name,
            project_path: o.project_path,
            sources: o
                .sources
                .iter()
                .map(|s| match s {
                    OrphanSource::Sqlite => "sqlite".to_string(),
                    OrphanSource::LocalState => "local_state".to_string(),
                    OrphanSource::Docker => "docker".to_string(),
                })
                .collect(),
            sqlite_project_id: o.sqlite_project_id,
            sqlite_workspace_count: o.sqlite_workspace_count,
            container_names: o.container_names,
            local_state_service_count: o.local_state_service_count,
            local_state_workspace_count: o.local_state_workspace_count,
        })
        .collect())
}

#[tauri::command]
pub async fn cleanup_orphan_project(project_name: String) -> Result<OrphanCleanupResult, String> {
    let orphans = orphan::detect_orphans()
        .await
        .map_err(|e| format!("Failed to detect orphans: {}", e))?;

    let orphan = orphans
        .iter()
        .find(|o| o.project_name == project_name)
        .ok_or_else(|| format!("Orphan project '{}' not found", project_name))?;

    let result = orphan::cleanup_orphan(orphan).await;

    Ok(OrphanCleanupResult {
        project_name: result.project_name,
        containers_removed: result.containers_removed,
        sqlite_rows_deleted: result.sqlite_rows_deleted,
        local_state_cleared: result.local_state_cleared,
        data_dirs_removed: result.data_dirs_removed,
        errors: result.errors,
    })
}

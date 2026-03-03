use devflow_core::state::{DevflowWorkspace, LocalStateManager};
use devflow_core::{config, services, vcs};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceCreationMode {
    Default,
    Worktree,
    Branch,
}

impl WorkspaceCreationMode {
    fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw
            .unwrap_or("default")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "default" => Ok(Self::Default),
            "worktree" => Ok(Self::Worktree),
            "branch" => Ok(Self::Branch),
            other => Err(format!(
                "Invalid workspace creation mode '{}'. Use: default, worktree, branch",
                other
            )),
        }
    }
}

fn apply_worktree_path_template(
    path_template: &str,
    repo_name: &str,
    workspace_name: &str,
) -> String {
    path_template
        .replace("{repo}", repo_name)
        .replace("{workspace}", workspace_name)
        // Backward compatibility with legacy templates.
        .replace("{branch}", workspace_name)
}

#[derive(Serialize)]
pub struct WorkspaceEntry {
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
pub struct WorkspacesResponse {
    pub workspaces: Vec<WorkspaceEntry>,
    pub current: Option<String>,
}

#[derive(Serialize)]
pub struct OrchestrationResultDto {
    pub service_name: String,
    pub success: bool,
    pub message: String,
}

#[tauri::command]
pub async fn list_workspaces(project_path: String) -> Result<WorkspacesResponse, String> {
    let project_dir = std::path::Path::new(&project_path);
    let config_path = project_dir.join(".devflow.yml");

    // Load config for main_workspace / normalization
    let cfg = if config_path.exists() {
        config::Config::from_file(&config_path).ok()
    } else {
        None
    };

    let main_workspace = cfg
        .as_ref()
        .map(|c| c.git.main_workspace.clone())
        .unwrap_or_else(|| "main".to_string());

    // Read devflow workspace registry
    let mut state_mgr = LocalStateManager::new().map_err(|e| e.to_string())?;

    let devflow_branches = state_mgr
        .get_or_init_workspaces_by_dir(project_dir, &main_workspace)
        .map_err(|e| e.to_string())?;

    // Determine current git workspace to find active devflow workspace
    let vcs_provider = vcs::detect_vcs_provider(&project_path).ok();
    let current_vcs_branch = vcs_provider
        .as_ref()
        .and_then(|v| v.current_workspace().ok().flatten());

    // Normalize the VCS workspace name for matching
    let normalized_current = current_vcs_branch.as_deref().map(|b| {
        cfg.as_ref()
            .map(|c| c.get_normalized_workspace_name(b))
            .unwrap_or_else(|| b.to_string())
    });

    // Get worktrees for enrichment
    let worktrees = vcs_provider
        .as_ref()
        .and_then(|v| v.list_worktrees().ok())
        .unwrap_or_default();

    let entries: Vec<WorkspaceEntry> = devflow_branches
        .into_iter()
        .map(|b| {
            // Check if this is the current workspace
            let is_current = normalized_current
                .as_deref()
                .map(|cur| cur == b.name)
                .unwrap_or(false);

            let is_default = b.name == main_workspace;

            // Prefer worktree_path from VCS if available, fall back to registry
            let worktree_path = worktrees
                .iter()
                .find(|w| w.workspace.as_deref() == Some(&b.name))
                .map(|w| w.path.display().to_string())
                .or(b.worktree_path);

            WorkspaceEntry {
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

    Ok(WorkspacesResponse {
        workspaces: entries,
        current,
    })
}

#[tauri::command]
pub async fn get_connection_info(
    project_path: String,
    workspace_name: String,
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
            .get_connection_info(&workspace_name)
            .await
            .map_err(|e| e.to_string())?;

        serde_json::to_value(&info).map_err(|e| e.to_string())
    } else {
        Err("No services configured".to_string())
    }
}

#[derive(Serialize)]
pub struct CreateWorkspaceResult {
    pub services: Vec<OrchestrationResultDto>,
    pub worktree_path: Option<String>,
    pub cow_used: bool,
}

#[tauri::command]
pub async fn create_workspace(
    app: tauri::AppHandle,
    project_path: String,
    workspace_name: String,
    from_workspace: Option<String>,
    creation_mode: Option<String>,
) -> Result<CreateWorkspaceResult, String> {
    let project_dir = std::path::Path::new(&project_path);
    let vcs_provider = vcs::detect_vcs_provider(&project_path).map_err(|e| e.to_string())?;
    let creation_mode = WorkspaceCreationMode::parse(creation_mode.as_deref())?;

    // Load config if it exists
    let config_path = project_dir.join(".devflow.yml");
    let cfg = if config_path.exists() {
        Some(config::Config::from_file(&config_path).map_err(|e| e.to_string())?)
    } else {
        None
    };

    let config_prefers_worktree = cfg
        .as_ref()
        .and_then(|c| c.worktree.as_ref())
        .is_some_and(|wt| wt.enabled);

    let create_as_worktree = match creation_mode {
        WorkspaceCreationMode::Default => config_prefers_worktree,
        WorkspaceCreationMode::Worktree => true,
        WorkspaceCreationMode::Branch => false,
    };

    if create_as_worktree && !vcs_provider.supports_worktrees() {
        return Err(format!(
            "VCS provider '{}' does not support worktrees",
            vcs_provider.provider_name()
        ));
    }

    // Normalize the workspace name
    let normalized_name = cfg
        .as_ref()
        .map(|c| c.get_normalized_workspace_name(&workspace_name))
        .unwrap_or_else(|| workspace_name.clone());

    let normalized_parent = from_workspace.as_deref().map(|fb| {
        cfg.as_ref()
            .map(|c| c.get_normalized_workspace_name(fb))
            .unwrap_or_else(|| fb.to_string())
    });

    // Create VCS workspace
    vcs_provider
        .create_workspace(&workspace_name, from_workspace.as_deref())
        .map_err(|e| e.to_string())?;

    // Create worktree if enabled
    let mut worktree_path: Option<String> = None;
    let mut cow_used = false;
    if create_as_worktree {
        // Check if a worktree already exists for this workspace
        let existing = vcs_provider.worktree_path(&workspace_name).unwrap_or(None);

        if let Some(existing_path) = existing {
            worktree_path = Some(existing_path.display().to_string());
        } else {
            // Resolve worktree path from template
            let repo_name = cfg
                .as_ref()
                .and_then(|c| c.name.as_ref())
                .filter(|n| !n.trim().is_empty())
                .map(|n| n.to_string())
                .unwrap_or_else(|| {
                    project_dir
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "repo".to_string())
                });
            let wt_config = cfg.as_ref().and_then(|c| c.worktree.as_ref());
            let path_template = wt_config
                .map(|wt| wt.path_template.as_str())
                .unwrap_or("../{repo}.{workspace}");
            let wt_path_str =
                apply_worktree_path_template(path_template, &repo_name, &normalized_name);
            let wt_path = project_dir.join(&wt_path_str);

            // Resolve to absolute path
            let wt_path = if wt_path.is_relative() {
                project_dir.join(&wt_path)
            } else {
                wt_path
            };

            let wt_result = vcs_provider
                .create_worktree(&workspace_name, &wt_path)
                .map_err(|e| format!("Failed to create worktree: {}", e))?;
            cow_used = wt_result.cow_used;

            // Copy configured files from main worktree
            if let Some(wt_cfg) = wt_config {
                let main_dir = vcs_provider
                    .main_worktree_dir()
                    .unwrap_or_else(|| project_dir.to_path_buf());

                // Copy explicitly listed files.
                // When CoW was used, these already exist as clones — overwrite with
                // independent copies so they can diverge between workspaces.
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

    // Orchestrate service workspaces if config exists
    let service_results = if let Some(ref cfg) = cfg {
        let results =
            services::factory::orchestrate_create(cfg, &workspace_name, from_workspace.as_deref())
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

    // Register the new workspace in devflow state
    if let Ok(mut state_mgr) = LocalStateManager::new() {
        let final_cow_used = if cow_used {
            true
        } else {
            state_mgr
                .get_workspace_by_dir(project_dir, &normalized_name)
                .map(|existing| existing.cow_used)
                .unwrap_or(false)
        };

        let workspace = DevflowWorkspace {
            name: normalized_name,
            parent: normalized_parent,
            worktree_path: worktree_path.clone(),
            created_at: chrono::Utc::now(),
            cow_used: final_cow_used,
            agent_tool: None,
            agent_status: None,
            agent_started_at: None,
        };
        if let Err(e) = state_mgr.register_workspace_by_dir(project_dir, workspace) {
            log::warn!("Failed to register workspace in devflow state: {}", e);
        }
    }

    let response = CreateWorkspaceResult {
        services: service_results,
        worktree_path,
        cow_used,
    };

    crate::update_tray_menu(&app);

    Ok(response)
}

#[tauri::command]
pub async fn switch_workspace(
    app: tauri::AppHandle,
    project_path: String,
    workspace_name: String,
) -> Result<Vec<OrchestrationResultDto>, String> {
    let vcs_provider = vcs::detect_vcs_provider(&project_path).map_err(|e| e.to_string())?;

    // Checkout VCS workspace
    vcs_provider
        .checkout_workspace(&workspace_name)
        .map_err(|e| e.to_string())?;

    // Orchestrate service switch if config exists
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    if config_path.exists() {
        let cfg = config::Config::from_file(&config_path).map_err(|e| e.to_string())?;
        let results = services::factory::orchestrate_switch(&cfg, &workspace_name, None)
            .await
            .map_err(|e| e.to_string())?;
        let response: Vec<OrchestrationResultDto> = results
            .into_iter()
            .map(|r| OrchestrationResultDto {
                service_name: r.service_name,
                success: r.success,
                message: r.message,
            })
            .collect();
        crate::update_tray_menu(&app);
        Ok(response)
    } else {
        crate::update_tray_menu(&app);
        Ok(vec![])
    }
}

#[tauri::command]
pub async fn delete_workspace(
    app: tauri::AppHandle,
    project_path: String,
    workspace_name: String,
) -> Result<Vec<OrchestrationResultDto>, String> {
    let project_dir = std::path::Path::new(&project_path);
    let vcs_provider = vcs::detect_vcs_provider(&project_path).map_err(|e| e.to_string())?;

    // 1. Remove worktree if one exists for this workspace
    if let Ok(Some(wt_path)) = vcs_provider.worktree_path(&workspace_name) {
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

    // 2. Orchestrate service workspace deletion if config exists
    let config_path = project_dir.join(".devflow.yml");
    let mut results = vec![];
    if config_path.exists() {
        let cfg = config::Config::from_file(&config_path).map_err(|e| e.to_string())?;

        // Normalize the workspace name for unregistration
        let normalized = cfg.get_normalized_workspace_name(&workspace_name);

        results = services::factory::orchestrate_delete(&cfg, &workspace_name)
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
            if let Err(e) = state_mgr.unregister_workspace_by_dir(project_dir, &normalized) {
                log::warn!("Failed to unregister workspace from devflow state: {}", e);
            }
        }
    } else {
        // No config — still unregister from state using the raw name
        if let Ok(mut state_mgr) = LocalStateManager::new() {
            if let Err(e) = state_mgr.unregister_workspace_by_dir(project_dir, &workspace_name) {
                log::warn!("Failed to unregister workspace from devflow state: {}", e);
            }
        }
    }

    // 4. Delete VCS workspace
    vcs_provider
        .delete_workspace(&workspace_name)
        .map_err(|e| e.to_string())?;

    crate::update_tray_menu(&app);
    Ok(results)
}

#[derive(Serialize)]
pub struct PruneResult {
    pub pruned: usize,
    pub details: Vec<String>,
}

#[tauri::command]
pub async fn prune_worktrees(project_path: String) -> Result<PruneResult, String> {
    let vcs_provider = vcs::detect_vcs_provider(&project_path).map_err(|e| e.to_string())?;

    if !vcs_provider.supports_worktrees() {
        return Err("VCS provider does not support worktrees".to_string());
    }

    // Identify stale worktrees (path no longer exists on disk)
    let worktrees = vcs_provider.list_worktrees().map_err(|e| e.to_string())?;
    let stale: Vec<_> = worktrees
        .iter()
        .filter(|wt| !wt.is_main && !wt.path.exists())
        .collect();

    if stale.is_empty() {
        return Ok(PruneResult {
            pruned: 0,
            details: vec![],
        });
    }

    // Use `git worktree prune` to clean up all stale entries at once
    let output = std::process::Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(&project_path)
        .output()
        .map_err(|e| format!("Failed to run git worktree prune: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree prune failed: {}", stderr));
    }

    let details: Vec<String> = stale
        .iter()
        .map(|wt| wt.path.display().to_string())
        .collect();
    let pruned = details.len();

    Ok(PruneResult { pruned, details })
}

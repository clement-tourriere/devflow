use devflow_core::state::LocalStateManager;
use devflow_core::workspace::hooks::HookApprovalMode;
use devflow_core::workspace::{self, LifecycleOptions, WorkspaceCreationMode};
use devflow_core::{config, vcs};
use serde::Serialize;
use tauri::Emitter;

#[derive(Serialize)]
pub struct WorkspaceEntry {
    pub name: String,
    pub is_current: bool,
    pub is_default: bool,
    pub worktree_path: Option<String>,
    pub parent: Option<String>,
    pub created_at: Option<String>,
    pub executed_command: Option<String>,
    pub execution_status: Option<String>,
    pub sandboxed: bool,
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

#[derive(Serialize, Clone)]
pub struct WorkspaceSwitchedEvent {
    pub project_path: String,
    pub workspace_name: String,
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
    let mut state_mgr = LocalStateManager::new().map_err(crate::commands::format_error)?;

    let devflow_branches = state_mgr
        .get_or_init_workspaces_by_dir(project_dir, &main_workspace)
        .map_err(crate::commands::format_error)?;

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
                parent: b.parent,
                created_at: Some(b.created_at.format("%Y-%m-%d %H:%M").to_string()),
                executed_command: b.executed_command,
                execution_status: b.execution_status,
                sandboxed: b.sandboxed,
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
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

    let named_services = config.resolve_services();
    let service_name = service_name.unwrap_or_else(|| "default".to_string());

    if let Some(svc) = named_services
        .iter()
        .find(|s| s.name == service_name)
        .or(named_services.first())
    {
        let provider =
            devflow_core::services::factory::create_provider_from_named_config(&config, svc)
                .await
                .map_err(crate::commands::format_error)?;

        let info = provider
            .get_connection_info(&workspace_name)
            .await
            .map_err(crate::commands::format_error)?;

        serde_json::to_value(&info).map_err(|e| e.to_string())
    } else {
        Ok(serde_json::json!({
            "status": "ok",
            "services": "none_configured",
            "message": "No services configured for this project"
        }))
    }
}

#[derive(Serialize)]
pub struct CreateWorkspaceResult {
    pub services: Vec<OrchestrationResultDto>,
    pub worktree_path: Option<String>,
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn create_workspace(
    app: tauri::AppHandle,
    project_path: String,
    workspace_name: String,
    from_workspace: Option<String>,
    creation_mode: Option<String>,
    copy_files: Option<Vec<String>>,
    copy_ignored: Option<bool>,
    sandboxed: Option<bool>,
) -> Result<CreateWorkspaceResult, String> {
    let project_dir = std::path::Path::new(&project_path);
    let config_path = project_dir.join(".devflow.yml");
    let cfg = if config_path.exists() {
        config::Config::from_file(&config_path).map_err(crate::commands::format_error)?
    } else {
        config::Config::default()
    };

    let creation_mode =
        WorkspaceCreationMode::parse(creation_mode.as_deref()).map_err(|e| e.to_string())?;

    let options = workspace::create::CreateOptions {
        lifecycle: gui_lifecycle_options(),
        creation_mode,
        from_workspace,
        copy_files,
        copy_ignored,
        sandboxed,
    };

    let result = workspace::create::create_workspace(&cfg, project_dir, &workspace_name, &options)
        .await
        .map_err(crate::commands::format_error)?;

    let response = CreateWorkspaceResult {
        services: result
            .services
            .into_iter()
            .map(|r| OrchestrationResultDto {
                service_name: r.service_name,
                success: r.success,
                message: r.message,
            })
            .collect(),
        worktree_path: result
            .worktree
            .as_ref()
            .map(|w| w.path.display().to_string()),
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
    let project_dir = std::path::Path::new(&project_path);
    let config_path = project_dir.join(".devflow.yml");
    let cfg = if config_path.exists() {
        config::Config::from_file(&config_path).map_err(crate::commands::format_error)?
    } else {
        config::Config::default()
    };

    let options = workspace::switch::SwitchOptions {
        lifecycle: gui_lifecycle_options(),
        create_if_missing: false,
        from_workspace: None,
        copy_files: None,
        copy_ignored: None,
        sandboxed: None,
    };

    let result = workspace::switch::switch_workspace(&cfg, project_dir, &workspace_name, &options)
        .await
        .map_err(crate::commands::format_error)?;

    let response = result
        .services
        .into_iter()
        .map(|r| OrchestrationResultDto {
            service_name: r.service_name,
            success: r.success,
            message: r.message,
        })
        .collect();

    let _ = app.emit(
        "workspace-switched",
        WorkspaceSwitchedEvent {
            project_path: project_path.clone(),
            workspace_name: workspace_name.clone(),
        },
    );

    crate::update_tray_menu(&app);
    Ok(response)
}

#[tauri::command]
pub async fn delete_workspace(
    app: tauri::AppHandle,
    project_path: String,
    workspace_name: String,
) -> Result<Vec<OrchestrationResultDto>, String> {
    let project_dir = std::path::Path::new(&project_path);
    let config_path = project_dir.join(".devflow.yml");
    let cfg = if config_path.exists() {
        config::Config::from_file(&config_path).map_err(crate::commands::format_error)?
    } else {
        config::Config::default()
    };

    let options = workspace::delete::DeleteOptions {
        lifecycle: gui_lifecycle_options(),
        keep_services: false,
    };

    let result = workspace::delete::delete_workspace(&cfg, project_dir, &workspace_name, &options)
        .await
        .map_err(crate::commands::format_error)?;

    let response = result
        .services
        .into_iter()
        .map(|r| OrchestrationResultDto {
            service_name: r.service_name,
            success: r.success,
            message: r.message,
        })
        .collect();

    crate::update_tray_menu(&app);
    Ok(response)
}

#[derive(Serialize)]
pub struct PruneResult {
    pub pruned: usize,
    pub details: Vec<String>,
}

#[tauri::command]
pub async fn prune_worktrees(project_path: String) -> Result<PruneResult, String> {
    let vcs_provider =
        vcs::detect_vcs_provider(&project_path).map_err(crate::commands::format_error)?;

    if !vcs_provider.supports_worktrees() {
        return Err("VCS provider does not support worktrees".to_string());
    }

    // Identify stale worktrees (path no longer exists on disk)
    let worktrees = vcs_provider
        .list_worktrees()
        .map_err(crate::commands::format_error)?;
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

/// Shared lifecycle options for GUI commands: no approval, quiet hooks.
fn gui_lifecycle_options() -> LifecycleOptions {
    LifecycleOptions {
        skip_hooks: false,
        skip_services: false,
        hook_approval: HookApprovalMode::NoApproval,
        verbose_hooks: false,
        trigger_source: Some("gui".to_string()),
        vcs_event: None,
    }
}

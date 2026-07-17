use devflow_core::processes::ProcessResult;
use devflow_core::state::LocalStateManager;
use devflow_core::vcs;
use devflow_core::workspace::hooks::HookApprovalMode;
use devflow_core::workspace::{self, LifecycleOptions};
use serde::Serialize;
use tauri::Emitter;

pub use devflow_core::workspace::inventory::WorkspaceNode as WorkspaceEntry;
pub type WorkspacesResponse = devflow_core::workspace::inventory::WorkspaceInventory;

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
    let cfg = crate::commands::project_config::load_project_config_with_local_state(project_dir)?;
    workspace::inventory::build_workspace_inventory(&cfg, project_dir)
        .await
        .map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn get_connection_info(
    project_path: String,
    workspace_name: String,
    service_name: Option<String>,
) -> Result<serde_json::Value, String> {
    let project_dir = std::path::Path::new(&project_path);
    let config =
        crate::commands::project_config::load_project_config_with_local_state(project_dir)?;

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

        // Accepts raw names AND provider-side keys — the UI's service rows
        // echo keys from list_service_workspaces back into this command.
        let service_key = LocalStateManager::new()
            .map_err(crate::commands::format_error)?
            .resolve_workspace_or_key_by_dir(project_dir, &workspace_name)
            .map_err(crate::commands::format_error)?;
        let info = provider
            .get_connection_info(&service_key)
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
    pub workspace: String,
    pub service_key: String,
    pub parent: Option<String>,
    pub vcs_ref_created: bool,
    pub services: Vec<OrchestrationResultDto>,
    pub processes: Vec<ProcessResult>,
    pub worktree_path: Option<String>,
    pub hooks: Vec<HookRunResultDto>,
}

#[derive(Serialize)]
pub struct HookRunResultDto {
    pub phase: String,
    pub succeeded: usize,
    pub failed: usize,
    pub skipped: usize,
    pub background: usize,
    pub errors: Vec<String>,
}

#[derive(Serialize)]
pub struct SwitchWorkspaceResult {
    pub workspace: String,
    pub service_key: String,
    pub parent: Option<String>,
    pub worktree_path: Option<String>,
    pub vcs_ref_created: bool,
    pub services: Vec<OrchestrationResultDto>,
    pub processes: Vec<ProcessResult>,
    pub hooks: Vec<HookRunResultDto>,
}

#[derive(Serialize)]
pub struct DeleteWorkspaceResult {
    pub workspace: String,
    pub service_key: String,
    pub worktree_removed: bool,
    pub worktree_path: Option<String>,
    pub vcs_ref_deleted: bool,
    pub services: Vec<OrchestrationResultDto>,
    pub processes: Vec<ProcessResult>,
    pub hooks: Vec<HookRunResultDto>,
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn create_workspace(
    app: tauri::AppHandle,
    project_path: String,
    workspace_name: String,
    from_workspace: Option<String>,
    copy_files: Option<Vec<String>>,
    copy_ignored: Option<bool>,
) -> Result<CreateWorkspaceResult, String> {
    let project_dir = std::path::Path::new(&project_path);
    let cfg = crate::commands::project_config::load_project_config_with_local_state(project_dir)?;

    let options = workspace::create::CreateOptions {
        lifecycle: gui_lifecycle_options(),
        from_workspace,
        copy_files,
        copy_ignored,
    };

    // Hard upper bound so a wedged service/process backend surfaces as an
    // error instead of leaving the GUI on "Creating..." forever. Creation can
    // legitimately take minutes (CoW seed clone, image pull, per-process
    // readiness), hence the generous limit.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        workspace::create::create_workspace(&cfg, project_dir, &workspace_name, &options),
    )
    .await
    .map_err(|_| {
        "Timed out creating workspace after 5 minutes; parts of it may exist. Refresh and retry."
            .to_string()
    })?
    .map_err(crate::commands::format_error)?;

    let response = CreateWorkspaceResult {
        workspace: result.workspace.clone(),
        service_key: result.service_key.clone(),
        parent: result.parent.clone(),
        vcs_ref_created: result.vcs_ref_created,
        services: result
            .services
            .into_iter()
            .map(|r| OrchestrationResultDto {
                service_name: r.service_name,
                success: r.success,
                message: r.message,
            })
            .collect(),
        processes: result.processes,
        worktree_path: result
            .worktree
            .as_ref()
            .map(|w| w.path.display().to_string()),
        hooks: result
            .hooks
            .into_iter()
            .map(|r| HookRunResultDto {
                phase: r.phase,
                succeeded: r.succeeded,
                failed: r.failed,
                skipped: r.skipped,
                background: r.background,
                errors: r.errors,
            })
            .collect(),
    };

    crate::update_tray_menu(&app);
    Ok(response)
}

#[tauri::command]
pub async fn switch_workspace(
    app: tauri::AppHandle,
    project_path: String,
    workspace_name: String,
) -> Result<SwitchWorkspaceResult, String> {
    let project_dir = std::path::Path::new(&project_path);
    let cfg = crate::commands::project_config::load_project_config_with_local_state(project_dir)?;

    let options = workspace::switch::SwitchOptions {
        lifecycle: gui_lifecycle_options(),
        create_if_missing: false,
        from_workspace: None,
        copy_files: None,
        copy_ignored: None,
    };

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        workspace::switch::switch_workspace(&cfg, project_dir, &workspace_name, &options),
    )
    .await
    .map_err(|_| "Timed out switching workspace after 5 minutes. Refresh and retry.".to_string())?
    .map_err(crate::commands::format_error)?;

    let response = SwitchWorkspaceResult {
        workspace: result.workspace.clone(),
        service_key: result.service_key.clone(),
        parent: result.parent.clone(),
        worktree_path: result
            .worktree
            .as_ref()
            .map(|worktree| worktree.path.display().to_string()),
        vcs_ref_created: result.vcs_ref_created,
        services: result
            .services
            .into_iter()
            .map(|r| OrchestrationResultDto {
                service_name: r.service_name,
                success: r.success,
                message: r.message,
            })
            .collect(),
        processes: result.processes,
        hooks: result
            .hooks
            .into_iter()
            .map(|r| HookRunResultDto {
                phase: r.phase,
                succeeded: r.succeeded,
                failed: r.failed,
                skipped: r.skipped,
                background: r.background,
                errors: r.errors,
            })
            .collect(),
    };

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
pub async fn preflight_delete_workspace(
    project_path: String,
    workspace_name: String,
) -> Result<workspace::delete::DeleteWorkspacePreflight, String> {
    let project_dir = std::path::Path::new(&project_path);
    let cfg = crate::commands::project_config::load_project_config_with_local_state(project_dir)?;
    workspace::delete::preflight_delete_workspace(&cfg, project_dir, &workspace_name)
        .map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn delete_workspace(
    app: tauri::AppHandle,
    project_path: String,
    workspace_name: String,
    force: Option<bool>,
) -> Result<DeleteWorkspaceResult, String> {
    let project_dir = std::path::Path::new(&project_path);
    let cfg = crate::commands::project_config::load_project_config_with_local_state(project_dir)?;

    let options = workspace::delete::DeleteOptions {
        lifecycle: gui_lifecycle_options(),
        keep_services: false,
        // A normal GUI confirmation still uses the safe path. Force is only
        // sent after a failed safe deletion and a second explicit warning.
        force: force.unwrap_or(false),
    };

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        workspace::delete::delete_workspace(&cfg, project_dir, &workspace_name, &options),
    )
    .await
    .map_err(|_| {
        "Timed out deleting workspace; cleanup may still be partially complete. Refresh and retry."
            .to_string()
    })?
    .map_err(crate::commands::format_error)?;

    let response = DeleteWorkspaceResult {
        workspace: result.workspace.clone(),
        service_key: result.service_key.clone(),
        worktree_removed: result.worktree_removed,
        worktree_path: result.worktree_path.clone(),
        vcs_ref_deleted: result.vcs_ref_deleted,
        services: result
            .services
            .into_iter()
            .map(|r| OrchestrationResultDto {
                service_name: r.service_name,
                success: r.success,
                message: r.message,
            })
            .collect(),
        processes: result.processes,
        hooks: result
            .hooks
            .into_iter()
            .map(|r| HookRunResultDto {
                phase: r.phase,
                succeeded: r.succeeded,
                failed: r.failed,
                skipped: r.skipped,
                background: r.background,
                errors: r.errors,
            })
            .collect(),
    };

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

    // Clean up all stale entries via the shared VCS abstraction.
    vcs_provider
        .prune_worktrees()
        .map_err(crate::commands::format_error)?;

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
        skip_processes: false,
        hook_approval: HookApprovalMode::NoApproval,
        verbose_hooks: false,
        trigger_source: Some("gui".to_string()),
        vcs_event: None,
    }
}

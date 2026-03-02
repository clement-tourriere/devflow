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

#[tauri::command]
pub async fn create_branch(
    project_path: String,
    branch_name: String,
    from_branch: Option<String>,
) -> Result<Vec<OrchestrationResultDto>, String> {
    let vcs_provider =
        vcs::detect_vcs_provider(&project_path).map_err(|e| e.to_string())?;

    // Create VCS branch
    vcs_provider
        .create_branch(&branch_name, from_branch.as_deref())
        .map_err(|e| e.to_string())?;

    // Orchestrate service branches if config exists
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    if config_path.exists() {
        let cfg = config::Config::from_file(&config_path).map_err(|e| e.to_string())?;
        let results = services::factory::orchestrate_create(&cfg, &branch_name, from_branch.as_deref())
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
    // Orchestrate service branch deletion if config exists
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

    // Delete VCS branch
    let vcs_provider =
        vcs::detect_vcs_provider(&project_path).map_err(|e| e.to_string())?;
    vcs_provider
        .delete_branch(&branch_name)
        .map_err(|e| e.to_string())?;

    Ok(results)
}

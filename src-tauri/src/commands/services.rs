use devflow_core::config::{
    ClickHouseConfig, GenericDockerConfig, LocalServiceConfig, MySQLConfig, NamedServiceConfig,
};
use devflow_core::docker::discovery;
use devflow_core::services;
use devflow_core::state::LocalStateManager;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize)]
pub struct ServiceEntry {
    pub name: String,
    pub service_type: String,
    pub provider_type: String,
    pub auto_workspace: bool,
}

#[derive(Serialize)]
pub struct ServiceWorkspaceStatus {
    pub service_name: String,
    pub workspace_name: String,
    pub state: Option<String>,
}

#[derive(Deserialize)]
pub struct AddServiceRequest {
    pub name: String,
    pub service_type: String,
    pub provider_type: String,
    pub auto_workspace: Option<bool>,
    pub image: Option<String>,
    pub seed_from: Option<String>,
}

#[tauri::command]
pub async fn add_service(
    project_path: String,
    request: AddServiceRequest,
) -> Result<ServiceEntry, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let mut config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

    // Ensure config.name is set so container names derive from the project, not cwd
    if config.name.is_none() {
        config.name = std::path::Path::new(&project_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string());
    }

    let named = build_named_config(&request)?;

    config
        .add_service(named.clone(), false)
        .map_err(crate::commands::format_error)?;
    config
        .save_to_file(&config_path)
        .map_err(crate::commands::format_error)?;

    // For local providers, initialize the service
    if request.provider_type == "local" || request.provider_type.is_empty() {
        if let Ok(provider) =
            services::factory::create_provider_from_named_config(&config, &named).await
        {
            let _ = provider.create_workspace("main", None).await;

            if let Some(ref seed) = request.seed_from {
                if !seed.is_empty() {
                    let _ = provider.seed_from_source("main", seed).await;
                }
            }
        }
    }

    Ok(ServiceEntry {
        name: named.name,
        service_type: named.service_type,
        provider_type: named.provider_type,
        auto_workspace: named.auto_workspace,
    })
}

fn build_named_config(request: &AddServiceRequest) -> Result<NamedServiceConfig, String> {
    let provider_type = if request.provider_type.is_empty() {
        "local".to_string()
    } else {
        request.provider_type.clone()
    };

    let auto_workspace = request.auto_workspace.unwrap_or(true);

    let mut named = NamedServiceConfig {
        name: request.name.clone(),
        provider_type: provider_type.clone(),
        service_type: request.service_type.clone(),
        auto_workspace,
        default: false,
        local: None,
        neon: None,
        dblab: None,
        xata: None,
        clickhouse: None,
        mysql: None,
        generic: None,
        plugin: None,
    };

    match request.service_type.as_str() {
        "postgres" => {
            if provider_type == "local" {
                named.local = Some(LocalServiceConfig {
                    image: Some(
                        request
                            .image
                            .clone()
                            .unwrap_or_else(|| "postgres:17".to_string()),
                    ),
                    data_root: None,
                    storage: None,
                    port_range_start: None,
                    postgres_user: None,
                    postgres_password: None,
                    postgres_db: None,
                });
            }
            // For neon, dblab, xata: cloud providers
            // require API keys configured separately, so we just set the type
        }
        "clickhouse" => {
            named.clickhouse = Some(ClickHouseConfig {
                image: request
                    .image
                    .clone()
                    .unwrap_or_else(|| "clickhouse/clickhouse-server:latest".to_string()),
                port_range_start: None,
                data_root: None,
                user: "default".to_string(),
                password: None,
            });
        }
        "mysql" => {
            named.mysql = Some(MySQLConfig {
                image: request
                    .image
                    .clone()
                    .unwrap_or_else(|| "mysql:8".to_string()),
                port_range_start: None,
                data_root: None,
                root_password: "dev".to_string(),
                database: None,
                user: None,
                password: None,
            });
        }
        "generic" => {
            let image = request
                .image
                .clone()
                .filter(|s| !s.is_empty())
                .ok_or("Docker image is required for generic services")?;
            named.generic = Some(GenericDockerConfig {
                image,
                port_mapping: None,
                port_range_start: None,
                environment: HashMap::new(),
                volumes: Vec::new(),
                command: None,
                healthcheck: None,
            });
        }
        other => {
            return Err(format!("Unsupported service type: {}", other));
        }
    }

    Ok(named)
}

#[tauri::command]
pub async fn list_services(project_path: String) -> Result<Vec<ServiceEntry>, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

    let named_services = config.resolve_services();
    Ok(named_services
        .iter()
        .map(|s| ServiceEntry {
            name: s.name.clone(),
            service_type: s.service_type.clone(),
            provider_type: s.provider_type.clone(),
            auto_workspace: s.auto_workspace,
        })
        .collect())
}

#[tauri::command]
pub async fn start_service(
    project_path: String,
    service_name: String,
    workspace_name: String,
) -> Result<(), String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

    let named_services = config.resolve_services();
    let svc = named_services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or("Service not found")?;

    let provider = services::factory::create_provider_from_named_config(&config, svc)
        .await
        .map_err(crate::commands::format_error)?;

    provider
        .start_workspace(&workspace_name)
        .await
        .map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn stop_service(
    project_path: String,
    service_name: String,
    workspace_name: String,
) -> Result<(), String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

    let named_services = config.resolve_services();
    let svc = named_services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or("Service not found")?;

    let provider = services::factory::create_provider_from_named_config(&config, svc)
        .await
        .map_err(crate::commands::format_error)?;

    provider
        .stop_workspace(&workspace_name)
        .await
        .map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn run_doctor(project_path: String) -> Result<serde_json::Value, String> {
    fn check(name: &str, available: bool, detail: impl Into<String>) -> services::DoctorCheck {
        services::DoctorCheck {
            name: name.to_string(),
            available,
            detail: detail.into(),
        }
    }

    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let project_dir = std::path::Path::new(&project_path);

    let mut general_checks: Vec<services::DoctorCheck> = Vec::new();

    // Config file + parsing
    if config_path.exists() {
        general_checks.push(check(
            "Config file",
            true,
            format!("Found {}", config_path.display()),
        ));
    } else {
        general_checks.push(check(
            "Config file",
            false,
            "Missing .devflow.yml (run devflow init)",
        ));
    }

    let config = match devflow_core::config::Config::from_file(&config_path) {
        Ok(cfg) => {
            general_checks.push(check("Config syntax", true, "Configuration is valid"));
            Some(cfg)
        }
        Err(e) => {
            general_checks.push(check("Config syntax", false, format!("{}", e)));
            None
        }
    };

    // VCS repository detection
    let vcs_repo = match devflow_core::vcs::detect_vcs_provider(&project_path) {
        Ok(vcs) => {
            general_checks.push(check(
                "VCS repository",
                true,
                format!("Detected {} repository", vcs.provider_name()),
            ));
            Some(vcs)
        }
        Err(e) => {
            general_checks.push(check("VCS repository", false, format!("{}", e)));
            None
        }
    };

    // Hooks installation (best effort)
    let hooks_dir = project_dir.join(".git").join("hooks");
    let has_hooks = if hooks_dir.exists() {
        let post_checkout = hooks_dir.join("post-checkout");
        let post_merge = hooks_dir.join("post-merge");
        if let Some(ref vcs) = vcs_repo {
            (post_checkout.exists() && vcs.is_devflow_hook(&post_checkout).unwrap_or(false))
                || (post_merge.exists() && vcs.is_devflow_hook(&post_merge).unwrap_or(false))
        } else {
            post_checkout.exists() || post_merge.exists()
        }
    } else {
        false
    };

    general_checks.push(check(
        "VCS hooks",
        has_hooks,
        if has_hooks {
            "devflow hooks installed".to_string()
        } else {
            "Hooks not installed (use Install hooks below or run devflow install-hooks)".to_string()
        },
    ));

    // Worktree metadata health
    if let Some(ref vcs) = vcs_repo {
        if vcs.supports_worktrees() {
            match vcs.list_worktrees() {
                Ok(worktrees) => {
                    let stale: Vec<_> = worktrees
                        .iter()
                        .filter(|wt| !wt.is_main && !wt.path.exists())
                        .collect();

                    if stale.is_empty() {
                        general_checks.push(check("Worktree metadata", true, "No stale entries"));
                    } else {
                        let examples = stale
                            .iter()
                            .take(3)
                            .map(|wt| wt.path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        general_checks.push(check(
                            "Worktree metadata",
                            false,
                            format!(
                                "{} stale entr{} (run `git worktree prune`): {}",
                                stale.len(),
                                if stale.len() == 1 { "y" } else { "ies" },
                                examples
                            ),
                        ));
                    }
                }
                Err(e) => {
                    general_checks.push(check(
                        "Worktree metadata",
                        false,
                        format!("Inspection failed: {}", e),
                    ));
                }
            }
        }
    }

    // Workspace registry stale paths
    match LocalStateManager::new() {
        Ok(state) => {
            let missing: Vec<_> = state
                .get_workspaces_by_dir(project_dir)
                .into_iter()
                .filter_map(|b| b.worktree_path.map(|p| (b.name, p)))
                .filter(|(_, p)| !std::path::Path::new(p).exists())
                .collect();

            if missing.is_empty() {
                general_checks.push(check("Workspace registry paths", true, "No stale entries"));
            } else {
                let examples = missing
                    .iter()
                    .take(3)
                    .map(|(workspace, p)| format!("{} -> {}", workspace, p))
                    .collect::<Vec<_>>()
                    .join(", ");
                general_checks.push(check(
                    "Workspace registry paths",
                    false,
                    format!(
                        "{} stale entr{}: {}",
                        missing.len(),
                        if missing.len() == 1 { "y" } else { "ies" },
                        examples
                    ),
                ));
            }
        }
        Err(e) => {
            general_checks.push(check(
                "Workspace registry paths",
                false,
                format!("Inspection failed: {}", e),
            ));
        }
    }

    // Agent skills check
    let skill_status = devflow_core::agent::check_agent_skills_installed(project_dir);
    general_checks.push(check(
        "Agent skills",
        skill_status.installed,
        if skill_status.installed {
            format!("{} skills installed", skill_status.installed_skills.len())
        } else {
            "Not installed".to_string()
        },
    ));

    let named_services = config
        .as_ref()
        .map(|c| c.resolve_services())
        .unwrap_or_default();
    let mut reports = Vec::new();

    for svc in &named_services {
        if let Some(ref cfg) = config {
            if let Ok(provider) =
                services::factory::create_provider_from_named_config(cfg, svc).await
            {
                if let Ok(report) = provider.doctor().await {
                    reports.push(serde_json::json!({
                        "service": svc.name,
                        "checks": report.checks,
                    }));
                }
            }
        }
    }

    Ok(serde_json::json!({
        "general": general_checks,
        "services": reports,
    }))
}

#[tauri::command]
pub async fn get_service_logs(
    project_path: String,
    service_name: String,
    workspace_name: String,
) -> Result<String, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

    let named_services = config.resolve_services();
    let svc = named_services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or("Service not found")?;

    let provider = services::factory::create_provider_from_named_config(&config, svc)
        .await
        .map_err(crate::commands::format_error)?;

    provider
        .logs(&workspace_name, Some(200))
        .await
        .map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn reset_service(
    project_path: String,
    service_name: String,
    workspace_name: String,
) -> Result<(), String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

    let named_services = config.resolve_services();
    let svc = named_services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or("Service not found")?;

    let provider = services::factory::create_provider_from_named_config(&config, svc)
        .await
        .map_err(crate::commands::format_error)?;

    provider
        .reset_workspace(&workspace_name)
        .await
        .map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn delete_service_workspace(
    project_path: String,
    service_name: String,
    workspace_name: String,
) -> Result<(), String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

    let named_services = config.resolve_services();
    let svc = named_services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or("Service not found")?;

    let provider = services::factory::create_provider_from_named_config(&config, svc)
        .await
        .map_err(crate::commands::format_error)?;

    provider
        .delete_workspace(&workspace_name)
        .await
        .map_err(crate::commands::format_error)
}

#[derive(Serialize)]
pub struct DestroyServiceResult {
    pub service_name: String,
    pub destroyed_workspaces: Vec<String>,
}

#[tauri::command]
pub async fn destroy_service(
    project_path: String,
    service_name: String,
) -> Result<DestroyServiceResult, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let mut config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

    let named_services = config.resolve_services();
    let svc = named_services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or("Service not found")?
        .clone();

    let provider = services::factory::create_provider_from_named_config(&config, &svc)
        .await
        .map_err(crate::commands::format_error)?;

    if !provider.supports_destroy() {
        return Err(format!(
            "Service '{}' (provider: {}) does not support destruction",
            service_name, svc.provider_type
        ));
    }

    let destroyed_workspaces = provider
        .destroy_project()
        .await
        .map_err(crate::commands::format_error)?;

    // Remove from local state
    let path = std::path::Path::new(&project_path);
    if let Ok(mut state_mgr) = LocalStateManager::new() {
        let _ = state_mgr.remove_service(path, &service_name);
    }

    // Remove from config and save
    config.remove_service(&service_name);
    config
        .save_to_file(&config_path)
        .map_err(crate::commands::format_error)?;

    Ok(DestroyServiceResult {
        service_name,
        destroyed_workspaces,
    })
}

#[derive(Serialize)]
pub struct ServiceWorkspaceInfo {
    pub name: String,
    pub created_at: Option<String>,
    pub parent_workspace: Option<String>,
    pub database_name: String,
    pub state: Option<String>,
}

#[tauri::command]
pub async fn list_service_workspaces(
    project_path: String,
    service_name: String,
) -> Result<Vec<ServiceWorkspaceInfo>, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

    let named_services = config.resolve_services();
    let svc = named_services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or("Service not found")?;

    let provider = services::factory::create_provider_from_named_config(&config, svc)
        .await
        .map_err(crate::commands::format_error)?;

    let workspaces = provider
        .list_workspaces()
        .await
        .map_err(crate::commands::format_error)?;

    Ok(workspaces
        .into_iter()
        .map(|b| ServiceWorkspaceInfo {
            name: b.name,
            created_at: b.created_at.map(|dt| dt.to_rfc3339()),
            parent_workspace: b.parent_workspace,
            database_name: b.database_name,
            state: b.state,
        })
        .collect())
}

#[tauri::command]
pub async fn get_service_status(
    project_path: String,
    service_name: String,
    workspace_name: String,
) -> Result<ServiceWorkspaceStatus, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

    let named_services = config.resolve_services();
    let svc = named_services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or("Service not found")?;

    let provider = services::factory::create_provider_from_named_config(&config, svc)
        .await
        .map_err(crate::commands::format_error)?;

    let workspaces = provider
        .list_workspaces()
        .await
        .map_err(crate::commands::format_error)?;

    let state = workspaces
        .iter()
        .find(|b| b.name == workspace_name)
        .and_then(|b| b.state.clone());

    Ok(ServiceWorkspaceStatus {
        service_name,
        workspace_name,
        state,
    })
}

#[derive(Serialize)]
pub struct DiscoveredContainerEntry {
    pub container_id: String,
    pub container_name: String,
    pub image: String,
    pub service_type: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
    pub connection_url: String,
    pub is_compose: bool,
    pub compose_project: Option<String>,
    pub compose_service: Option<String>,
}

#[tauri::command]
pub async fn discover_docker_containers(
    service_type: Option<String>,
) -> Result<Vec<DiscoveredContainerEntry>, String> {
    let containers = discovery::discover_containers(service_type.as_deref())
        .await
        .map_err(crate::commands::format_error)?;

    Ok(containers
        .into_iter()
        .map(|c| DiscoveredContainerEntry {
            container_id: c.container_id,
            container_name: c.container_name,
            image: c.image,
            service_type: format!("{:?}", c.service_type).to_lowercase(),
            host: c.host,
            port: c.port,
            username: c.username,
            password: c.password,
            database: c.database,
            connection_url: c.connection_url,
            is_compose: c.is_compose,
            compose_project: c.compose_project,
            compose_service: c.compose_service,
        })
        .collect())
}

#[tauri::command]
pub async fn install_agent_skills(project_path: String) -> Result<Vec<String>, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let project_dir = std::path::Path::new(&project_path);
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

    devflow_core::agent::install_agent_skills(&config, project_dir)
        .map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn uninstall_agent_skills(project_path: String) -> Result<(), String> {
    let project_dir = std::path::Path::new(&project_path);
    devflow_core::agent::uninstall_agent_skills(project_dir).map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn check_agent_skills(
    project_path: String,
) -> Result<devflow_core::agent::SkillInstallStatus, String> {
    let project_dir = std::path::Path::new(&project_path);
    Ok(devflow_core::agent::check_agent_skills_installed(
        project_dir,
    ))
}

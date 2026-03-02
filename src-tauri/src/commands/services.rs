use devflow_core::config::{
    ClickHouseConfig, GenericDockerConfig, LocalServiceConfig, MySQLConfig, NamedServiceConfig,
};
use devflow_core::services;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize)]
pub struct ServiceEntry {
    pub name: String,
    pub service_type: String,
    pub provider_type: String,
    pub auto_branch: bool,
}

#[derive(Serialize)]
pub struct ServiceBranchStatus {
    pub service_name: String,
    pub branch_name: String,
    pub state: Option<String>,
}

#[derive(Deserialize)]
pub struct AddServiceRequest {
    pub name: String,
    pub service_type: String,
    pub provider_type: String,
    pub auto_branch: Option<bool>,
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
        .map_err(|e| e.to_string())?;

    // Ensure config.name is set so container names derive from the project, not cwd
    if config.name.is_none() {
        config.name = std::path::Path::new(&project_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string());
    }

    let named = build_named_config(&request)?;

    config
        .add_service(named.clone(), false)
        .map_err(|e| e.to_string())?;
    config
        .save_to_file(&config_path)
        .map_err(|e| e.to_string())?;

    // For local providers, initialize the service
    if request.provider_type == "local" || request.provider_type.is_empty() {
        if let Ok(provider) =
            services::factory::create_provider_from_named_config(&config, &named).await
        {
            let _ = provider.create_branch("main", None).await;

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
        auto_branch: named.auto_branch,
    })
}

fn build_named_config(request: &AddServiceRequest) -> Result<NamedServiceConfig, String> {
    let provider_type = if request.provider_type.is_empty() {
        "local".to_string()
    } else {
        request.provider_type.clone()
    };

    let auto_branch = request.auto_branch.unwrap_or(true);

    let mut named = NamedServiceConfig {
        name: request.name.clone(),
        provider_type: provider_type.clone(),
        service_type: request.service_type.clone(),
        auto_branch,
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
            // For neon, dblab, xata, postgres_template: cloud providers
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
        .map_err(|e| e.to_string())?;

    let named_services = config.resolve_services();
    Ok(named_services
        .iter()
        .map(|s| ServiceEntry {
            name: s.name.clone(),
            service_type: s.service_type.clone(),
            provider_type: s.provider_type.clone(),
            auto_branch: s.auto_branch,
        })
        .collect())
}

#[tauri::command]
pub async fn start_service(
    project_path: String,
    service_name: String,
    branch_name: String,
) -> Result<(), String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(|e| e.to_string())?;

    let named_services = config.resolve_services();
    let svc = named_services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or("Service not found")?;

    let provider = services::factory::create_provider_from_named_config(&config, svc)
        .await
        .map_err(|e| e.to_string())?;

    provider
        .start_branch(&branch_name)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn stop_service(
    project_path: String,
    service_name: String,
    branch_name: String,
) -> Result<(), String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(|e| e.to_string())?;

    let named_services = config.resolve_services();
    let svc = named_services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or("Service not found")?;

    let provider = services::factory::create_provider_from_named_config(&config, svc)
        .await
        .map_err(|e| e.to_string())?;

    provider
        .stop_branch(&branch_name)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn run_doctor(project_path: String) -> Result<serde_json::Value, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(|e| e.to_string())?;

    let named_services = config.resolve_services();
    let mut reports = Vec::new();

    for svc in &named_services {
        if let Ok(provider) =
            services::factory::create_provider_from_named_config(&config, svc).await
        {
            if let Ok(report) = provider.doctor().await {
                reports.push(serde_json::json!({
                    "service": svc.name,
                    "checks": report.checks,
                }));
            }
        }
    }

    Ok(serde_json::Value::Array(reports))
}

#[tauri::command]
pub async fn get_service_logs(
    project_path: String,
    service_name: String,
    branch_name: String,
) -> Result<String, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(|e| e.to_string())?;

    let named_services = config.resolve_services();
    let svc = named_services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or("Service not found")?;

    let provider = services::factory::create_provider_from_named_config(&config, svc)
        .await
        .map_err(|e| e.to_string())?;

    provider
        .logs(&branch_name, Some(200))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn reset_service(
    project_path: String,
    service_name: String,
    branch_name: String,
) -> Result<(), String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(|e| e.to_string())?;

    let named_services = config.resolve_services();
    let svc = named_services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or("Service not found")?;

    let provider = services::factory::create_provider_from_named_config(&config, svc)
        .await
        .map_err(|e| e.to_string())?;

    provider
        .reset_branch(&branch_name)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_service_status(
    project_path: String,
    service_name: String,
    branch_name: String,
) -> Result<ServiceBranchStatus, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(|e| e.to_string())?;

    let named_services = config.resolve_services();
    let svc = named_services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or("Service not found")?;

    let provider = services::factory::create_provider_from_named_config(&config, svc)
        .await
        .map_err(|e| e.to_string())?;

    let branches = provider
        .list_branches()
        .await
        .map_err(|e| e.to_string())?;

    let state = branches
        .iter()
        .find(|b| b.name == branch_name)
        .and_then(|b| b.state.clone());

    Ok(ServiceBranchStatus {
        service_name,
        branch_name,
        state,
    })
}

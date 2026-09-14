use devflow_core::config::{
    ClickHouseConfig, DockerCustomSettings, GenericDockerConfig, LocalServiceConfig, MySQLConfig,
    NamedServiceConfig, SharedServiceConfig,
};
use devflow_core::docker::discovery;
use devflow_core::services;
use devflow_core::state::LocalStateManager;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize)]
pub struct ServiceEntry {
    pub operations: services::OperationCapabilities,
    pub name: String,
    pub service_type: String,
    pub provider_type: String,
    pub auto_workspace: bool,
    /// Where this service definition came from: `config` or `local_state`.
    pub source: String,
}

#[derive(Deserialize)]
pub struct AddServiceRequest {
    pub name: String,
    pub service_type: String,
    pub provider_type: String,
    pub auto_workspace: Option<bool>,
    pub image: Option<String>,
    pub seed_from: Option<String>,
    pub storage: Option<String>,
    pub docker_command: Option<Vec<String>>,
    pub docker_environment: Option<HashMap<String, String>>,
    pub docker_restart_policy: Option<String>,
}

fn service_entry(named: &NamedServiceConfig, source: &str) -> ServiceEntry {
    ServiceEntry {
        operations: services::descriptor::providers()
            .into_iter()
            .find(|d| {
                d.provider == named.provider_type
                    && d.service_types.contains(&named.service_type.as_str())
            })
            .map(|d| d.operations)
            .unwrap_or_default(),
        name: named.name.clone(),
        service_type: named.service_type.clone(),
        provider_type: named.provider_type.clone(),
        auto_workspace: named.auto_workspace,
        source: source.to_string(),
    }
}

fn resolve_effective_service_key(
    project_dir: &std::path::Path,
    raw_workspace: &str,
) -> Result<String, String> {
    // The UI's service-workspace rows carry provider-side service keys (from
    // list_service_workspaces), while workspace rows carry raw names — accept
    // both: a key belonging to exactly one verified workspace passes through
    // instead of tripping the raw-name owner guard.
    LocalStateManager::new()
        .map_err(crate::commands::format_error)?
        .resolve_workspace_or_key_by_dir(project_dir, raw_workspace)
        .map_err(crate::commands::format_error)
}

/// Like [`resolve_effective_service_key`], but for operations that target an
/// EXISTING provider-side workspace: an unregistered input that exactly names
/// one (an orphan row echoed back from `list_service_workspaces`) is targeted
/// verbatim so it stays reachable for cleanup instead of being re-derived
/// into a namespace that never existed.
async fn resolve_operation_service_key(
    project_dir: &std::path::Path,
    raw_workspace: &str,
    provider: &dyn services::ServiceProvider,
) -> Result<String, String> {
    services::factory::resolve_service_operation_key(project_dir, raw_workspace, provider)
        .await
        .map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn add_service(
    project_path: String,
    request: AddServiceRequest,
) -> Result<ServiceEntry, String> {
    let project_dir = std::path::Path::new(&project_path);
    let _policy_lease =
        devflow_core::workspace::operation::project_policy_lease(project_dir, false)
            .await
            .map_err(crate::commands::format_error)?;
    let config_path = crate::commands::project_config::config_path(project_dir);
    let named = build_named_config(&request)?;

    // Match CLI behavior: service definitions added from the GUI are stored in
    // local state, not committed into `.devflow.yml`, unless the user edits the
    // config file explicitly. This avoids leaking local images/credentials and
    // keeps CLI and GUI service lists consistent.
    let mut state = LocalStateManager::new().map_err(crate::commands::format_error)?;
    state
        .add_service(&config_path, named.clone(), false)
        .map_err(crate::commands::format_error)?;

    let config =
        crate::commands::project_config::load_project_config_with_local_state(project_dir)?;

    if services::factory::ProviderType::is_local(&named.provider_type) {
        let provider = services::factory::create_provider_from_named_config(&config, &named)
            .await
            .map_err(crate::commands::format_error)?;
        let key = resolve_effective_service_key(project_dir, &config.git.main_workspace)?;
        if !provider
            .workspace_exists(&key)
            .await
            .map_err(crate::commands::format_error)?
        {
            provider
                .create_workspace(&key, None)
                .await
                .map_err(crate::commands::format_error)?;
        }
        if let Some(seed) = request.seed_from.as_deref().filter(|s| !s.is_empty()) {
            provider
                .seed_from_source(&key, seed)
                .await
                .map_err(crate::commands::format_error)?;
        }
    }

    Ok(service_entry(&named, "local_state"))
}

fn normalize_storage_preference(
    storage: Option<&str>,
    provider_type: &str,
    service_type: &str,
) -> Result<Option<String>, String> {
    let Some(storage) = storage.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };

    if provider_type != "local" || service_type != "postgres" {
        return Err(
            "storage preference is only supported for local PostgreSQL services".to_string(),
        );
    }

    let normalized = storage.to_ascii_lowercase().replace('-', "_");
    let normalized = match normalized.as_str() {
        "" | "auto" => "auto".to_string(),
        "apfs" | "apfs_clone" => "apfs_clone".to_string(),
        "zfs" | "reflink" | "copy" => normalized,
        other => {
            return Err(format!(
                "invalid storage preference '{}'; expected auto, zfs, apfs_clone, reflink, or copy",
                other
            ));
        }
    };

    Ok(Some(normalized))
}

#[cfg(test)]
mod tests {
    use super::{normalize_storage_preference, run_doctor};

    #[test]
    fn storage_preference_normalizes_gui_values() {
        assert_eq!(
            normalize_storage_preference(Some("APFS"), "local", "postgres").unwrap(),
            Some("apfs_clone".to_string())
        );
        assert_eq!(
            normalize_storage_preference(Some("copy"), "local", "postgres").unwrap(),
            Some("copy".to_string())
        );
    }

    #[test]
    fn storage_preference_rejects_wrong_service_type() {
        let err = normalize_storage_preference(Some("copy"), "local", "mysql").unwrap_err();
        assert!(err.contains("only supported for local PostgreSQL"));
    }

    #[tokio::test]
    async fn run_doctor_reports_summary_and_next_steps() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tmp = std::env::temp_dir().join(format!("devflow-tauri-doctor-test-{unique}"));
        std::fs::create_dir_all(&tmp).unwrap();

        let report = run_doctor(tmp.to_string_lossy().to_string()).await.unwrap();
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(report["healthy"].as_bool(), Some(false));
        assert!(report["summary"]["failed"].as_u64().unwrap_or(0) > 0);
        let next_steps = report["next_steps"].as_array().expect("next_steps array");
        assert!(next_steps
            .iter()
            .any(|step| step["command"] == "devflow init"));
    }
}

fn build_named_config(request: &AddServiceRequest) -> Result<NamedServiceConfig, String> {
    let provider_type = if request.provider_type.is_empty() {
        if request.service_type == "redis" {
            "shared".to_string()
        } else {
            "local".to_string()
        }
    } else {
        request.provider_type.clone()
    };

    let descriptor = services::descriptor::validate_choice(&provider_type, &request.service_type)
        .map_err(crate::commands::format_error)?;
    if !descriptor.required_configuration.is_empty() {
        return Err(format!(
            "Configure {} in the raw project configuration before adding this provider",
            descriptor.required_configuration.join(", ")
        ));
    }
    let auto_workspace = request.auto_workspace.unwrap_or(true);
    let storage = normalize_storage_preference(
        request.storage.as_deref(),
        &provider_type,
        &request.service_type,
    )?;

    let mut named = NamedServiceConfig {
        name: request.name.clone(),
        provider_type: provider_type.clone(),
        service_type: request.service_type.clone(),
        auto_workspace,
        default: false,
        local: None,
        shared: if request.service_type == "redis" {
            Some(SharedServiceConfig {
                image: Some(
                    request
                        .image
                        .clone()
                        .unwrap_or_else(|| "redis:7".to_string()),
                ),
                ..Default::default()
            })
        } else {
            None
        },
        neon: None,
        dblab: None,
        xata: None,
        clickhouse: None,
        mysql: None,
        generic: None,
        plugin: None,
        docker: {
            let settings = DockerCustomSettings {
                command: request.docker_command.clone().unwrap_or_default(),
                environment: request.docker_environment.clone().unwrap_or_default(),
                restart_policy: request.docker_restart_policy.clone(),
            };
            if settings.is_empty() {
                None
            } else {
                Some(settings)
            }
        },
    };

    match request.service_type.as_str() {
        "postgres" => {
            if provider_type == "local" {
                named.local = Some(LocalServiceConfig {
                    legacy_project_id: None,
                    image: Some(
                        request
                            .image
                            .clone()
                            .unwrap_or_else(|| "postgres:17".to_string()),
                    ),
                    data_root: None,
                    storage,
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
        "redis" => {
            if provider_type == "local" {
                named.shared = None;
                named.generic = Some(GenericDockerConfig {
                    image: request.image.clone().unwrap_or_else(|| "redis:7".into()),
                    port_mapping: Some("6379:6379".into()),
                    port_range_start: Some(56379),
                    environment: Default::default(),
                    volumes: Vec::new(),
                    command: None,
                    healthcheck: Some("redis-cli ping".into()),
                });
            }
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
    let project_dir = std::path::Path::new(&project_path);
    Ok(
        crate::commands::project_config::list_services_with_sources(project_dir)?
            .into_iter()
            .map(|entry| service_entry(&entry.service, entry.source.as_str()))
            .collect(),
    )
}

#[tauri::command]
pub async fn start_service(
    project_path: String,
    service_name: String,
    workspace_name: String,
) -> Result<(), String> {
    let project_dir = std::path::Path::new(&project_path);
    let config =
        crate::commands::project_config::load_project_config_with_local_state(project_dir)?;

    let named_services = config.resolve_services();
    let svc = named_services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or("Service not found")?;

    let provider = services::factory::create_provider_from_named_config(&config, svc)
        .await
        .map_err(crate::commands::format_error)?;

    devflow_core::application::service_operation(
        &config,
        project_dir,
        &service_name,
        &workspace_name,
        provider.as_ref(),
        services::ServiceOperation::Start,
    )
    .await
    .map(|_| ())
    .map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn stop_service(
    project_path: String,
    service_name: String,
    workspace_name: String,
) -> Result<(), String> {
    let project_dir = std::path::Path::new(&project_path);
    let config =
        crate::commands::project_config::load_project_config_with_local_state(project_dir)?;

    let named_services = config.resolve_services();
    let svc = named_services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or("Service not found")?;

    let provider = services::factory::create_provider_from_named_config(&config, svc)
        .await
        .map_err(crate::commands::format_error)?;

    devflow_core::application::service_operation(
        &config,
        project_dir,
        &service_name,
        &workspace_name,
        provider.as_ref(),
        services::ServiceOperation::Stop,
    )
    .await
    .map(|_| ())
    .map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn run_doctor(project_path: String) -> Result<serde_json::Value, String> {
    let project_dir = std::path::Path::new(&project_path);
    let config_path = devflow_core::config::Config::find_config_file_in(project_dir)
        .unwrap_or_else(|| project_dir.join(".devflow.yml"));

    let config = devflow_core::config::Config::from_file(&config_path).ok();
    let service_config =
        crate::commands::project_config::load_project_config_with_local_state(project_dir).ok();
    let config_for_checks = service_config.clone().unwrap_or_default();
    let mut general_checks = devflow_core::doctor::project_doctor_checks(
        &config_for_checks,
        config_path.exists().then_some(config_path.as_path()),
        project_dir,
    );
    let syntax_insert_at = usize::from(!general_checks.is_empty());
    general_checks.insert(
        syntax_insert_at,
        services::DoctorCheck {
            name: "Config syntax".to_string(),
            available: config.is_some(),
            detail: if config.is_some() {
                "Configuration is valid".to_string()
            } else if config_path.exists() {
                devflow_core::config::Config::from_file(&config_path)
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "Configuration could not be parsed".to_string())
            } else {
                "Skipped because .devflow.yml is missing".to_string()
            },
        },
    );

    general_checks.push(
        match crate::commands::project_config::service_source_counts(project_dir) {
            Ok((committed, local, names)) => services::DoctorCheck {
                name: "Service config sources".to_string(),
                available: true,
                detail: format!(
                    "{} service(s) from config, {} from local state ({})",
                    committed,
                    local,
                    names.join(", ")
                ),
            },
            Err(error) => services::DoctorCheck {
                name: "Service config sources".to_string(),
                available: false,
                detail: error,
            },
        },
    );
    let named_services = service_config
        .as_ref()
        .map(|c| c.resolve_services())
        .unwrap_or_default();
    let mut reports = Vec::new();
    let mut all_checks = general_checks.clone();

    for svc in &named_services {
        if let Some(ref cfg) = service_config {
            match services::factory::create_provider_from_named_config(cfg, svc).await {
                Ok(provider) => match provider.doctor().await {
                    Ok(report) => {
                        let summary =
                            devflow_core::doctor::DoctorSummary::from_checks(&report.checks);
                        let next_steps = devflow_core::doctor::doctor_next_steps(&report.checks);
                        all_checks.extend(report.checks.clone());
                        reports.push(serde_json::json!({
                            "service": svc.name,
                            "checks": report.checks,
                            "summary": summary,
                            "next_steps": next_steps,
                        }));
                    }
                    Err(err) => {
                        let checks = vec![services::DoctorCheck {
                            name: "Service doctor".to_string(),
                            available: false,
                            detail: crate::commands::format_error(err),
                        }];
                        let summary = devflow_core::doctor::DoctorSummary::from_checks(&checks);
                        let next_steps = devflow_core::doctor::doctor_next_steps(&checks);
                        all_checks.extend(checks.clone());
                        reports.push(serde_json::json!({
                            "service": svc.name,
                            "checks": checks,
                            "summary": summary,
                            "next_steps": next_steps,
                        }));
                    }
                },
                Err(err) => {
                    let checks = vec![services::DoctorCheck {
                        name: "Service doctor".to_string(),
                        available: false,
                        detail: crate::commands::format_error(err),
                    }];
                    let summary = devflow_core::doctor::DoctorSummary::from_checks(&checks);
                    let next_steps = devflow_core::doctor::doctor_next_steps(&checks);
                    all_checks.extend(checks.clone());
                    reports.push(serde_json::json!({
                        "service": svc.name,
                        "checks": checks,
                        "summary": summary,
                        "next_steps": next_steps,
                    }));
                }
            }
        }
    }

    let summary = devflow_core::doctor::DoctorSummary::from_checks(&all_checks);
    Ok(serde_json::json!({
        "healthy": summary.failed == 0,
        "failed_checks": summary.failed,
        "summary": summary,
        "next_steps": devflow_core::doctor::doctor_next_steps(&all_checks),
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
    let project_dir = std::path::Path::new(&project_path);
    let config =
        crate::commands::project_config::load_project_config_with_local_state(project_dir)?;

    let named_services = config.resolve_services();
    let svc = named_services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or("Service not found")?;

    let provider = services::factory::create_provider_from_named_config(&config, svc)
        .await
        .map_err(crate::commands::format_error)?;

    let service_key =
        resolve_operation_service_key(project_dir, &workspace_name, provider.as_ref()).await?;
    provider
        .logs(&service_key, Some(200))
        .await
        .map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn reset_service(
    project_path: String,
    service_name: String,
    workspace_name: String,
) -> Result<(), String> {
    let project_dir = std::path::Path::new(&project_path);
    let config =
        crate::commands::project_config::load_project_config_with_local_state(project_dir)?;

    let named_services = config.resolve_services();
    let svc = named_services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or("Service not found")?;

    let provider = services::factory::create_provider_from_named_config(&config, svc)
        .await
        .map_err(crate::commands::format_error)?;

    devflow_core::application::service_operation(
        &config,
        project_dir,
        &service_name,
        &workspace_name,
        provider.as_ref(),
        services::ServiceOperation::Reset,
    )
    .await
    .map(|_| ())
    .map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn delete_service_workspace(
    project_path: String,
    service_name: String,
    workspace_name: String,
) -> Result<(), String> {
    let project_dir = std::path::Path::new(&project_path);
    let _policy_lease = devflow_core::workspace::operation::project_policy_lease(project_dir, true)
        .await
        .map_err(crate::commands::format_error)?;
    let config =
        crate::commands::project_config::load_project_config_with_local_state(project_dir)?;

    let named_services = config.resolve_services();
    let svc = named_services
        .iter()
        .find(|s| s.name == service_name)
        .ok_or("Service not found")?;

    let provider = services::factory::create_provider_from_named_config(&config, svc)
        .await
        .map_err(crate::commands::format_error)?;

    let service_key =
        resolve_operation_service_key(project_dir, &workspace_name, provider.as_ref()).await?;
    provider
        .delete_workspace(&service_key)
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
    let project_dir = std::path::Path::new(&project_path);
    let _policy_lease = devflow_core::workspace::operation::project_policy_lease(project_dir, true)
        .await
        .map_err(crate::commands::format_error)?;
    let config_path = crate::commands::project_config::config_path(project_dir);
    let config =
        crate::commands::project_config::load_project_config_with_local_state(project_dir)?;

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

    // Remove from local state (CLI/GUI-managed services) and committed config
    // (if this service came from `.devflow.yml`).
    if let Ok(mut state_mgr) = LocalStateManager::new() {
        let _ = state_mgr.remove_service(&config_path, &service_name);
    }

    if config_path.exists() {
        if let Ok(mut disk_config) = devflow_core::config::Config::from_file(&config_path) {
            disk_config.remove_service(&service_name);
            disk_config
                .save_to_file(&config_path)
                .map_err(crate::commands::format_error)?;
        }
    }

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
    let project_dir = std::path::Path::new(&project_path);
    let config =
        crate::commands::project_config::load_project_config_with_local_state(project_dir)?;

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
    pub command: Vec<String>,
    pub extra_env: std::collections::HashMap<String, String>,
    pub restart_policy: Option<String>,
}

#[tauri::command]
pub async fn discover_docker_containers(
    service_type: Option<String>,
    project_path: Option<String>,
    global: Option<bool>,
) -> Result<Vec<DiscoveredContainerEntry>, String> {
    let scoped_project_root = if global.unwrap_or(false) {
        None
    } else if let Some(path) = project_path.as_deref() {
        Some(std::path::PathBuf::from(path))
    } else {
        std::env::current_dir().ok()
    };

    let containers =
        discovery::discover_containers(service_type.as_deref(), scoped_project_root.as_deref())
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
            command: c.command,
            extra_env: HashMap::new(),
            restart_policy: c.restart_policy,
        })
        .collect())
}

#[tauri::command]
pub async fn generate_agent_guides(project_path: String) -> Result<Vec<String>, String> {
    let project_dir = std::path::Path::new(&project_path);
    let config =
        crate::commands::project_config::load_project_config_with_local_state(project_dir)?;
    devflow_core::agent::install_agent_guides(
        &config,
        project_dir,
        &devflow_core::agent::default_agent_guide_targets(),
    )
    .map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn uninstall_agent_skills(project_path: String) -> Result<(), String> {
    let project_dir = std::path::Path::new(&project_path);
    devflow_core::agent::uninstall_agent_skills(project_dir).map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn check_agent_guides(
    project_path: String,
) -> Result<devflow_core::agent::SkillInstallStatus, String> {
    let project_dir = std::path::Path::new(&project_path);
    Ok(devflow_core::agent::check_agent_guides(project_dir))
}

#[tauri::command]
pub fn provider_descriptors() -> Vec<services::descriptor::ProviderDescriptor> {
    services::descriptor::providers()
}

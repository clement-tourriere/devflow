use std::path::{Path, PathBuf};

use anyhow::Result;
use devflow_core::config::{Config, EffectiveConfig};
use devflow_core::docker;
use devflow_core::hooks::HookPhase;
use devflow_core::services::{self};
use devflow_core::state::LocalStateManager;
use devflow_core::vcs;
use serde::Serialize;

/// Public CLI representation of a provider workspace.
///
/// Providers deliberately operate on backend-safe keys. The CLI contract uses
/// raw VCS identities, while retaining all previous WorkspaceInfo fields and
/// exposing the provider identity explicitly as `service_key`.
#[derive(Debug, Clone, Serialize)]
struct PublicWorkspaceInfo {
    name: String,
    workspace: String,
    service_key: String,
    created_at: Option<chrono::DateTime<chrono::Utc>>,
    parent_workspace: Option<String>,
    parent_service_key: Option<String>,
    database_name: String,
    state: Option<String>,
}

/// Stable machine-readable summary of one configured service definition.
#[derive(Debug, Clone, Serialize)]
struct ConfiguredServiceInfo {
    name: String,
    service_type: String,
    provider_type: String,
    auto_workspace: bool,
    default: bool,
    source: String,
}

fn configured_service_infos(
    config: &Config,
    config_path: &Option<PathBuf>,
) -> Vec<ConfiguredServiceInfo> {
    let services = if let Some(path) = config_path {
        config.services_with_sources(path)
    } else {
        config
            .resolve_services()
            .into_iter()
            .map(|service| devflow_core::config::ServiceWithSource {
                service,
                source: devflow_core::config::ServiceSource::Config,
            })
            .collect()
    };

    services
        .into_iter()
        .map(|entry| ConfiguredServiceInfo {
            name: entry.service.name,
            service_type: entry.service.service_type,
            provider_type: entry.service.provider_type,
            auto_workspace: entry.service.auto_workspace,
            default: entry.service.default,
            source: entry.source.as_str().to_string(),
        })
        .collect()
}

fn print_configured_services(
    config: &Config,
    config_path: &Option<PathBuf>,
    json_output: bool,
) -> Result<()> {
    let services = configured_service_infos(config, config_path);
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": 1,
                "services": services,
            }))?
        );
    } else if services.is_empty() {
        println!("No services configured.");
    } else {
        println!("Configured services:");
        for service in services {
            let mut tags = Vec::new();
            if service.default {
                tags.push("default");
            }
            if service.auto_workspace {
                tags.push("auto-workspace");
            }
            tags.push(service.source.as_str());
            println!(
                "  {} ({} via {}) [{}]",
                service.name,
                service.service_type,
                service.provider_type,
                tags.join(", ")
            );
        }
    }
    Ok(())
}

fn provider_uses_local_runtime(provider_type: &str) -> bool {
    matches!(
        provider_type.to_ascii_lowercase().as_str(),
        "local" | "docker" | "shared" | "global"
    )
}

fn validate_scaffoldable_service(service_type: &str, provider_type: &str) -> Result<()> {
    let service_type = service_type.to_ascii_lowercase();
    let provider_type = provider_type.to_ascii_lowercase();
    let supported = match provider_type.as_str() {
        "local" | "docker" => matches!(
            service_type.as_str(),
            "postgres" | "clickhouse" | "mysql" | "mariadb"
        ),
        "shared" | "global" => matches!(
            service_type.as_str(),
            "postgres" | "clickhouse" | "redis" | "rustfs" | "s3" | "objectstorage"
        ),
        "neon" | "dblab" | "database_lab" | "xata" | "xata_lite" => {
            anyhow::bail!(
                "Provider '{}' requires credentials and provider-specific fields. Define it explicitly under `services:` in .devflow.yml instead of using `service add`.",
                provider_type
            );
        }
        _ => false,
    };

    if !supported {
        anyhow::bail!(
            "`service add` cannot fully configure service type '{}' with provider '{}'. Supported scaffolds: postgres (local/shared), clickhouse (local/shared), mysql (local), redis (shared), and rustfs (shared). Configure custom/plugin services explicitly in .devflow.yml.",
            service_type,
            provider_type
        );
    }
    Ok(())
}

fn resolve_effective_service_key(
    config_path: &Option<PathBuf>,
    raw_workspace: &str,
) -> Result<String> {
    LocalStateManager::new()?.resolve_workspace_service_key_by_dir(
        &super::operation_project_dir(config_path),
        raw_workspace,
    )
}

/// Resolution for operations that target an EXISTING service workspace
/// (delete/start/stop/reset/logs/connection/seed): registered names resolve
/// through local state, and an unregistered input that exactly names a
/// provider-side workspace (an orphan surfaced by inventory warnings) is
/// targeted verbatim so it stays reachable for cleanup. Creation keeps
/// [`resolve_effective_service_key`] so new workspaces never adopt orphans.
async fn resolve_operation_service_key(
    config_path: &Option<PathBuf>,
    raw_workspace: &str,
    provider: &dyn devflow_core::services::ServiceProvider,
) -> Result<String> {
    services::factory::resolve_service_operation_key(
        &super::operation_project_dir(config_path),
        raw_workspace,
        provider,
    )
    .await
}

fn raw_workspace_for_service_key(
    config: &Config,
    config_path: &Option<PathBuf>,
    service_key: &str,
) -> Option<String> {
    if config_path.is_some() {
        let project_dir = super::operation_project_dir(config_path);
        if let Ok(state) = LocalStateManager::new() {
            let mut owners = state
                .get_workspaces_by_dir(&project_dir)
                .into_iter()
                .filter(|workspace| workspace.service_key == service_key);
            if let Some(workspace) = owners.next() {
                if owners.next().is_none() {
                    return Some(workspace.name);
                }
                return None;
            }
        }
    }

    let default_workspace = &config.git.main_workspace;
    let default_key = if config_path.is_some() {
        resolve_effective_service_key(config_path, default_workspace).ok()?
    } else {
        config.get_service_workspace_key(default_workspace)
    };
    (default_key == service_key).then(|| default_workspace.clone())
}

fn public_workspace_info(
    config: &Config,
    config_path: &Option<PathBuf>,
    raw_workspace: &str,
    raw_parent: Option<&str>,
    info: &services::WorkspaceInfo,
) -> PublicWorkspaceInfo {
    let parent_service_key = info.parent_workspace.clone();
    let parent_workspace = parent_service_key.as_deref().map(|key| {
        raw_parent
            .filter(|parent| {
                let parent_key = if config_path.is_some() {
                    resolve_effective_service_key(config_path, parent).ok()
                } else {
                    Some(config.get_service_workspace_key(parent))
                };
                parent_key.as_deref() == Some(key)
            })
            .map(str::to_owned)
            .or_else(|| raw_workspace_for_service_key(config, config_path, key))
            // Unknown provider-owned parents retain their previous value for
            // compatibility; `parent_service_key` makes its identity explicit.
            .unwrap_or_else(|| key.to_string())
    });

    PublicWorkspaceInfo {
        name: raw_workspace.to_string(),
        workspace: raw_workspace.to_string(),
        service_key: info.name.clone(),
        created_at: info.created_at,
        parent_workspace,
        parent_service_key,
        database_name: info.database_name.clone(),
        state: info.state.clone(),
    }
}

/// Internal enum for multi-service aggregation dispatch.
pub(super) enum ServiceAggregation {
    Status,
    Doctor,
    Capabilities,
}

/// Reusable interactive service-add wizard.
///
/// Walks the user through service type, provider, Docker discovery, and name selection.
/// Returns the created `NamedServiceConfig` on success, or `None` if cancelled.
/// In non-interactive/JSON mode, requires explicit parameters.
pub(crate) async fn run_add_service_wizard(
    config: &mut Config,
    config_path: &Path,
    non_interactive: bool,
    json_output: bool,
    from: Option<&str>,
) -> Result<Option<devflow_core::config::NamedServiceConfig>> {
    // 1. Service type selection
    let service_type = if non_interactive || json_output {
        devflow_core::config::default_service_type()
    } else {
        use inquire::Select;
        let service_types = vec![
            "postgres    — PostgreSQL database",
            "clickhouse  — ClickHouse analytics database",
            "mysql       — MySQL database",
            "redis       — Redis cache (shared DB per workspace)",
            "rustfs      — S3-compatible object storage (shared buckets)",
        ];
        let selection = Select::new("What type of service?", service_types)
            .with_help_message("Use arrow keys to navigate, Enter to select")
            .prompt();
        match selection {
            Ok(s) => s
                .split_whitespace()
                .next()
                .unwrap_or("postgres")
                .to_string(),
            Err(
                inquire::InquireError::OperationCanceled
                | inquire::InquireError::OperationInterrupted,
            ) => {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }
    };

    // 2. Provider selection
    let provider_type = if non_interactive || json_output {
        "local".to_string()
    } else {
        use inquire::Select;
        let provider_options: Vec<&str> = match service_type.as_str() {
            "postgres" => vec![
                "local               — Docker container per workspace (CoW)",
                "shared              — One global container, a database per workspace",
            ],
            "clickhouse" => vec!["local               — Docker container on this machine"],
            "mysql" => vec!["local               — Docker container on this machine"],
            "redis" => vec!["shared              — One global container, DB index per workspace"],
            "rustfs" => vec!["shared              — One global container, bucket per workspace"],
            _ => vec!["local               — Docker container on this machine"],
        };

        if provider_options.len() == 1 {
            let only = provider_options[0]
                .split_whitespace()
                .next()
                .unwrap_or("local")
                .to_string();
            println!("Provider: {}", only);
            only
        } else {
            let selection = Select::new("Which provider?", provider_options)
                .with_help_message("Use arrow keys to navigate, Enter to select")
                .prompt();
            match selection {
                Ok(s) => s.split_whitespace().next().unwrap_or("local").to_string(),
                Err(
                    inquire::InquireError::OperationCanceled
                    | inquire::InquireError::OperationInterrupted,
                ) => {
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
    };
    validate_scaffoldable_service(&service_type, &provider_type)?;

    // 2.5. Docker container discovery
    let discovered = offer_discovered_containers(
        &service_type,
        config_path.parent(),
        non_interactive,
        json_output,
    )
    .await;

    // 3. Service name
    let name = if non_interactive || json_output {
        match service_type.as_str() {
            "clickhouse" => "analytics".to_string(),
            "mysql" => "mysql".to_string(),
            "redis" => "redis".to_string(),
            "rustfs" => "storage".to_string(),
            _ => "db".to_string(),
        }
    } else {
        use inquire::Text;
        let default_name = if let Some(ref disc) = discovered {
            disc.name.as_str()
        } else {
            match service_type.as_str() {
                "clickhouse" => "analytics",
                "mysql" => "mysql",
                "redis" => "redis",
                "rustfs" => "storage",
                _ => "db",
            }
        };
        let input = Text::new("Service name:")
            .with_default(default_name)
            .with_help_message("A short identifier for this service (e.g. db, analytics)")
            .prompt();
        match input {
            Ok(n) if n.trim().is_empty() => default_name.to_string(),
            Ok(n) => n.trim().to_string(),
            Err(
                inquire::InquireError::OperationCanceled
                | inquire::InquireError::OperationInterrupted,
            ) => {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }
    };

    let is_local = services::factory::ProviderType::is_local(&provider_type);

    let discovered_image = discovered.as_ref().map(|d| d.image.clone());
    let discovered_seed = discovered.as_ref().map(|d| d.seed_url.clone());

    // Build named service config
    let mut named_cfg = devflow_core::config::NamedServiceConfig {
        name: name.clone(),
        provider_type: provider_type.clone(),
        service_type: service_type.clone(),
        auto_workspace: devflow_core::config::default_auto_branch(),
        default: false,
        local: if is_local {
            Some(devflow_core::config::LocalServiceConfig {
                image: discovered_image.clone(),
                data_root: None,
                storage: None,
                port_range_start: None,
                postgres_user: None,
                postgres_password: None,
                postgres_db: None,
            })
        } else {
            None
        },
        shared: if matches!(provider_type.as_str(), "shared" | "global")
            || matches!(
                service_type.as_str(),
                "redis" | "rustfs" | "s3" | "objectstorage"
            ) {
            Some(devflow_core::config::SharedServiceConfig {
                image: if service_type == "redis" {
                    discovered_image
                        .clone()
                        .or_else(|| Some("redis:7".to_string()))
                } else {
                    discovered_image.clone()
                },
                ..Default::default()
            })
        } else {
            None
        },
        neon: None,
        dblab: None,
        xata: None,
        clickhouse: if service_type == "clickhouse" {
            Some(devflow_core::config::ClickHouseConfig {
                image: discovered_image
                    .clone()
                    .unwrap_or_else(|| "clickhouse/clickhouse-server:latest".to_string()),
                port_range_start: None,
                data_root: None,
                user: "default".to_string(),
                password: None,
            })
        } else {
            None
        },
        mysql: if service_type == "mysql" {
            Some(devflow_core::config::MySQLConfig {
                image: discovered_image.unwrap_or_else(|| "mysql:8".to_string()),
                port_range_start: None,
                data_root: None,
                root_password: "dev".to_string(),
                database: None,
                user: None,
                password: None,
            })
        } else {
            None
        },
        generic: None,
        plugin: None,
        docker: discovered.as_ref().and_then(|d| d.docker_settings.clone()),
    };

    // Store service in local state. The wizard is intentionally idempotent:
    // removing `.devflow.yml` should not strand users with an opaque duplicate
    // error when local devflow state still remembers a service.
    let mut state = LocalStateManager::new()?;
    loop {
        let existing = state
            .get_services(config_path)
            .unwrap_or_default()
            .into_iter()
            .find(|service| service.name == named_cfg.name)
            .or_else(|| {
                config
                    .resolve_services()
                    .into_iter()
                    .find(|service| service.name == named_cfg.name)
            });

        let Some(existing) = existing else {
            state.add_service(config_path, named_cfg.clone(), false)?;
            if !json_output {
                println!("Added service '{}' ({})", named_cfg.name, service_type);
            }
            break;
        };

        if non_interactive || json_output {
            if !json_output {
                println!(
                    "Service '{}' already exists; reusing existing configuration.",
                    existing.name
                );
            }
            named_cfg = existing;
            break;
        }

        let choices = vec![
            "Keep existing service",
            "Replace existing service",
            "Use a different service name",
            "Cancel",
        ];
        let selection = inquire::Select::new(
            &format!("Service '{}' already exists. What should devflow do?", named_cfg.name),
            choices,
        )
        .with_help_message(
            "Services are stored in local devflow state; deleting .devflow.yml does not delete them.",
        )
        .prompt();

        match selection {
            Ok("Keep existing service") => {
                println!("Keeping existing service '{}'.", existing.name);
                named_cfg = existing;
                break;
            }
            Ok("Replace existing service") => {
                named_cfg.default = existing.default;
                state.add_service(config_path, named_cfg.clone(), true)?;
                println!("Replaced service '{}' ({}).", named_cfg.name, service_type);
                break;
            }
            Ok("Use a different service name") => {
                named_cfg.default = false;
                let default_name = format!("{}-2", named_cfg.name);
                let input = inquire::Text::new("New service name:")
                    .with_default(&default_name)
                    .prompt();
                match input {
                    Ok(n) if !n.trim().is_empty() => named_cfg.name = n.trim().to_string(),
                    Ok(_) => named_cfg.name = default_name,
                    Err(
                        inquire::InquireError::OperationCanceled
                        | inquire::InquireError::OperationInterrupted,
                    ) => return Ok(None),
                    Err(e) => return Err(e.into()),
                }
            }
            Ok("Cancel")
            | Err(
                inquire::InquireError::OperationCanceled
                | inquire::InquireError::OperationInterrupted,
            ) => return Ok(None),
            Err(e) => return Err(e.into()),
            _ => return Ok(None),
        }
    }

    // Use explicit seed source or discovered container's connection URL
    let effective_seed = from.map(|s| s.to_string()).or(discovered_seed);

    // Create the configured default workspace for local providers.
    let is_effective_local = services::factory::ProviderType::is_local(&named_cfg.provider_type);
    if provider_uses_local_runtime(&named_cfg.provider_type) {
        let mut config_with_service = config.clone();
        if let Some(state_services) = state.get_services(config_path) {
            config_with_service.services = Some(state_services);
        }

        #[cfg(feature = "service-local")]
        if cfg!(target_os = "linux") && is_effective_local {
            if let Some(data_root) =
                super::init::attempt_zfs_auto_setup(non_interactive, json_output).await
            {
                let mut updated_cfg = named_cfg.clone();
                if let Some(ref mut local) = updated_cfg.local {
                    local.data_root = Some(data_root);
                }
                if let Err(e) = state.add_service(config_path, updated_cfg.clone(), true) {
                    log::warn!(
                        "Failed to persist updated service config in local state: {}",
                        e
                    );
                }
                if let Some(state_services) = state.get_services(config_path) {
                    config_with_service.services = Some(state_services);
                }
                super::init::init_local_service_main(
                    &config_with_service,
                    &updated_cfg,
                    effective_seed.as_deref(),
                    json_output,
                )
                .await;
            } else {
                super::init::init_local_service_main(
                    &config_with_service,
                    &named_cfg,
                    effective_seed.as_deref(),
                    json_output,
                )
                .await;
            }
        } else {
            super::init::init_local_service_main(
                &config_with_service,
                &named_cfg,
                effective_seed.as_deref(),
                json_output,
            )
            .await;
        }
        #[cfg(not(feature = "service-local"))]
        {
            super::init::init_local_service_main(
                &config_with_service,
                &named_cfg,
                effective_seed.as_deref(),
                json_output,
            )
            .await;
        }
    }

    Ok(Some(named_cfg))
}

pub(super) async fn handle_service_dispatch(
    action: super::ServiceCommands,
    config: &mut Config,
    _effective_config: &EffectiveConfig,
    json_output: bool,
    non_interactive: bool,
    database_name: Option<&str>,
    config_path: &Option<std::path::PathBuf>,
) -> Result<()> {
    match action {
        super::ServiceCommands::Add {
            name,
            provider,
            service_type,
            force,
            from,
        } => {
            let config_path_buf = config_path
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap().join(".devflow.yml"));

            // When explicit flags are provided, use them directly; otherwise delegate to wizard
            if name.is_some() || provider.is_some() || service_type.is_some() {
                // Direct mode with explicit flags — keep existing behavior for CLI power users
                let service_type = service_type
                    .unwrap_or_else(devflow_core::config::default_service_type)
                    .to_ascii_lowercase();
                let provider_type = provider
                    .unwrap_or_else(|| {
                        if matches!(
                            service_type.as_str(),
                            "redis" | "rustfs" | "s3" | "objectstorage"
                        ) {
                            "shared".to_string()
                        } else {
                            "local".to_string()
                        }
                    })
                    .to_ascii_lowercase();
                validate_scaffoldable_service(&service_type, &provider_type)?;
                let name = if let Some(n) = name {
                    n
                } else if non_interactive || json_output {
                    anyhow::bail!("Service name is required in non-interactive mode. Usage: devflow service add <name>");
                } else {
                    use inquire::Text;
                    let default_name = match service_type.as_str() {
                        "clickhouse" => "analytics",
                        "mysql" => "mysql",
                        "redis" => "redis",
                        "generic" => "app",
                        "plugin" => "plugin",
                        _ => "db",
                    };
                    Text::new("Service name:")
                        .with_default(default_name)
                        .prompt()
                        .unwrap_or_else(|_| default_name.to_string())
                };

                let is_local = services::factory::ProviderType::is_local(&provider_type);
                let named_cfg = devflow_core::config::NamedServiceConfig {
                    name: name.clone(),
                    provider_type: provider_type.clone(),
                    service_type: service_type.clone(),
                    auto_workspace: devflow_core::config::default_auto_branch(),
                    default: false,
                    local: if is_local {
                        Some(devflow_core::config::LocalServiceConfig {
                            image: None,
                            data_root: None,
                            storage: None,
                            port_range_start: None,
                            postgres_user: None,
                            postgres_password: None,
                            postgres_db: None,
                        })
                    } else {
                        None
                    },
                    shared: if matches!(provider_type.as_str(), "shared" | "global")
                        || matches!(
                            service_type.as_str(),
                            "redis" | "rustfs" | "s3" | "objectstorage"
                        ) {
                        Some(devflow_core::config::SharedServiceConfig {
                            image: (service_type == "redis").then(|| "redis:7".to_string()),
                            ..Default::default()
                        })
                    } else {
                        None
                    },
                    neon: None,
                    dblab: None,
                    xata: None,
                    clickhouse: if service_type == "clickhouse" {
                        Some(devflow_core::config::ClickHouseConfig {
                            image: "clickhouse/clickhouse-server:latest".to_string(),
                            port_range_start: None,
                            data_root: None,
                            user: "default".to_string(),
                            password: None,
                        })
                    } else {
                        None
                    },
                    mysql: if service_type == "mysql" {
                        Some(devflow_core::config::MySQLConfig {
                            image: "mysql:8".to_string(),
                            port_range_start: None,
                            data_root: None,
                            root_password: "dev".to_string(),
                            database: None,
                            user: None,
                            password: None,
                        })
                    } else {
                        None
                    },
                    generic: None,
                    plugin: None,
                    docker: None,
                };

                let mut state = LocalStateManager::new()?;
                state.add_service(&config_path_buf, named_cfg.clone(), force)?;
                if !json_output {
                    println!("Added service '{}' to local state", name);
                }

                if provider_uses_local_runtime(&provider_type) {
                    let mut config_with_service = config.clone();
                    if let Some(state_services) = state.get_services(&config_path_buf) {
                        config_with_service.services = Some(state_services);
                    }
                    super::init::init_local_service_main(
                        &config_with_service,
                        &named_cfg,
                        from.as_deref(),
                        json_output,
                    )
                    .await;
                }

                if json_output {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "status": "ok",
                            "action": "add_service",
                            "name": name,
                            "provider_type": provider_type,
                        }))?
                    );
                }
            } else {
                // Interactive wizard mode
                let result = run_add_service_wizard(
                    config,
                    &config_path_buf,
                    non_interactive,
                    json_output,
                    from.as_deref(),
                )
                .await?;

                if let Some(ref cfg) = result {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "status": "ok",
                                "action": "add_service",
                                "name": cfg.name,
                                "provider_type": cfg.provider_type,
                            }))?
                        );
                    }
                } else {
                    println!("Cancelled.");
                }
            }
        }
        super::ServiceCommands::Remove { name } => {
            let config_path_buf = config_path
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap().join(".devflow.yml"));

            let mut state = LocalStateManager::new()?;
            state.remove_service(&config_path_buf, &name)?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "action": "remove_service",
                        "name": name,
                    }))?
                );
            } else {
                println!("Removed service '{}' from local state", name);
            }
        }
        super::ServiceCommands::List => {
            let _ = database_name;
            return print_configured_services(config, config_path, json_output);
        }
        super::ServiceCommands::Up => {
            let statuses = services::factory::reconcile_shared_engines(config).await?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "action": "service_up",
                        "engines": statuses,
                    }))?
                );
            } else if statuses.is_empty() {
                println!("No shared global engines configured (type: shared, or service_type rustfs/redis).");
            } else {
                println!("Shared global engines:");
                for s in &statuses {
                    let mark = if s.running { "✓" } else { "✗" };
                    println!(
                        "  {} {} ({}) — {}",
                        mark, s.service_name, s.provider, s.detail
                    );
                }
                let failed = statuses.iter().filter(|s| !s.running).count();
                if failed > 0 {
                    anyhow::bail!("{} engine(s) failed to start", failed);
                }
            }
            return Ok(());
        }
        super::ServiceCommands::Status => {
            let has_multiple_services = config.resolve_services().len() > 1;
            if database_name.is_none() && has_multiple_services {
                return handle_multi_service_aggregation(
                    ServiceAggregation::Status,
                    config,
                    json_output,
                    config_path,
                )
                .await;
            }
            let named = services::factory::resolve_provider(config, database_name).await?;
            let provider = named.provider;
            let workspaces = provider.list_workspaces().await.unwrap_or_default();
            let running = workspaces
                .iter()
                .filter(|b| b.state.as_deref() == Some("running"))
                .count();
            let stopped = workspaces
                .iter()
                .filter(|b| b.state.as_deref() == Some("stopped"))
                .count();
            let project_info = provider.project_info();

            if json_output {
                let mut status = serde_json::json!({
                    "provider": provider.provider_name(),
                    "total_branches": workspaces.len(),
                    "running": running,
                    "stopped": stopped,
                    "supports_lifecycle": provider.supports_lifecycle(),
                });
                if let Some(ref info) = project_info {
                    status["project"] = serde_json::Value::String(info.name.clone());
                    if let Some(ref storage) = info.storage_driver {
                        status["storage"] = serde_json::Value::String(storage.clone());
                    }
                    if let Some(ref image) = info.image {
                        status["image"] = serde_json::Value::String(image.clone());
                    }
                }
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("Provider: {}", provider.provider_name());
                if let Some(ref info) = project_info {
                    println!("Project: {}", info.name);
                    if let Some(ref storage) = info.storage_driver {
                        println!("Storage: {}", storage);
                    }
                    if let Some(ref image) = info.image {
                        println!("Image: {}", image);
                    }
                }
                println!(
                    "Branches: {} total ({} running, {} stopped)",
                    workspaces.len(),
                    running,
                    stopped
                );
                if provider.supports_lifecycle() {
                    println!("Lifecycle: supported (start/stop/reset)");
                }
            }
        }
        super::ServiceCommands::Capabilities => {
            let has_multiple_services = config.resolve_services().len() > 1;
            if database_name.is_none() && has_multiple_services {
                return handle_multi_service_aggregation(
                    ServiceAggregation::Capabilities,
                    config,
                    json_output,
                    config_path,
                )
                .await;
            }

            match services::factory::resolve_provider(config, database_name).await {
                Ok(named) => {
                    let caps = named.provider.capabilities();

                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "service": named.name,
                                "provider": named.provider.provider_name(),
                                "capabilities": caps,
                            }))?
                        );
                    } else {
                        println!(
                            "Service: {} ({})",
                            named.name,
                            named.provider.provider_name()
                        );
                        println!("  lifecycle: {}", if caps.lifecycle { "yes" } else { "no" });
                        println!("  logs: {}", if caps.logs { "yes" } else { "no" });
                        println!(
                            "  seed_from_source: {}",
                            if caps.seed_from_source { "yes" } else { "no" }
                        );
                        println!(
                            "  destroy_project: {}",
                            if caps.destroy_project { "yes" } else { "no" }
                        );
                        println!("  cleanup: {}", if caps.cleanup { "yes" } else { "no" });
                        println!(
                            "  template_from_time: {}",
                            if caps.template_from_time { "yes" } else { "no" }
                        );
                        println!(
                            "  max_workspace_name_length: {}",
                            caps.max_workspace_name_length
                        );
                    }
                }
                Err(e) => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "error": e.to_string(),
                                "services": null,
                            }))?
                        );
                    } else {
                        println!("No service provider available: {}", e);
                    }
                }
            }
        }
        super::ServiceCommands::Discover {
            service_type,
            global,
        } => {
            handle_discover(
                service_type.as_deref(),
                global,
                config_path.as_ref().and_then(|p| p.parent()),
                json_output,
            )
            .await?;
        }
        // Provider operations: delegate to handle_service_provider_command
        other => {
            return handle_service_provider_command(
                other,
                config,
                json_output,
                non_interactive,
                database_name,
                config_path,
            )
            .await;
        }
    }

    Ok(())
}

/// Handle service provider operations (create, delete, start, stop, reset, destroy, connection, logs, seed).
pub(super) async fn handle_service_provider_command(
    cmd: super::ServiceCommands,
    config: &mut Config,
    json_output: bool,
    non_interactive: bool,
    database_name: Option<&str>,
    config_path: &Option<std::path::PathBuf>,
) -> Result<()> {
    if matches!(
        &cmd,
        super::ServiceCommands::Cleanup { .. } | super::ServiceCommands::Connection { .. }
    ) && config.resolve_services().is_empty()
    {
        if json_output {
            let mut obj = serde_json::json!({
                "status": "ok",
                "services": "none_configured",
            });
            if matches!(&cmd, super::ServiceCommands::Cleanup { .. }) {
                obj["deleted"] = serde_json::json!([]);
            }
            if matches!(&cmd, super::ServiceCommands::Connection { .. }) {
                obj["message"] = serde_json::json!("No services configured for this project");
            }
            println!("{}", serde_json::to_string_pretty(&obj)?);
        } else if matches!(&cmd, super::ServiceCommands::Cleanup { .. }) {
            println!("No services configured. Nothing to clean up.");
        } else {
            println!(
                "No services configured. This project uses workspaces without database services."
            );
        }
        return Ok(());
    }

    // Orchestratable mutation commands: Create and Delete operate on all auto_workspace services
    let is_orchestratable_mutation = matches!(
        &cmd,
        super::ServiceCommands::Create { .. } | super::ServiceCommands::Delete { .. }
    );
    let has_multiple_services = config.resolve_services().len() > 1;

    // For Create/Delete: if there are multiple services and no --service flag,
    // use orchestration to operate on all auto_workspace services atomically.
    if is_orchestratable_mutation && database_name.is_none() && has_multiple_services {
        return handle_orchestrated_mutation(
            cmd,
            config,
            json_output,
            non_interactive,
            config_path,
        )
        .await;
    }

    let named = services::factory::resolve_provider(config, database_name).await?;
    let provider = named.provider;
    let resolved_name = named.name;

    // For non-orchestratable mutation commands with multiple services and no --service, print a note
    if !is_orchestratable_mutation && database_name.is_none() && has_multiple_services {
        eprintln!(
            "note: using default service '{}'. Use --service to target a specific one.",
            resolved_name
        );
    }

    match cmd {
        super::ServiceCommands::Create {
            workspace_name,
            from,
        } => {
            let service_key = resolve_effective_service_key(config_path, &workspace_name)?;
            let parent_service_key = from
                .as_deref()
                .map(|parent| resolve_effective_service_key(config_path, parent))
                .transpose()?;
            let project_dir = super::operation_project_dir(config_path);
            let hook_opts = devflow_core::workspace::LifecycleOptions {
                hook_approval: if non_interactive || json_output {
                    devflow_core::workspace::hooks::HookApprovalMode::NonInteractive
                } else {
                    devflow_core::workspace::hooks::HookApprovalMode::Interactive
                },
                verbose_hooks: !json_output,
                ..Default::default()
            };

            // Fire pre-service-create hooks
            devflow_core::workspace::hooks::run_lifecycle_hooks(
                config,
                &project_dir,
                &workspace_name,
                HookPhase::PreServiceCreate,
                &hook_opts,
            )
            .await?;

            // Single-service path (explicit --service or single service)
            let info = provider
                .create_workspace(&service_key, parent_service_key.as_deref())
                .await?;

            // Fire post-service-create hooks
            devflow_core::workspace::hooks::run_lifecycle_hooks(
                config,
                &project_dir,
                &workspace_name,
                HookPhase::PostServiceCreate,
                &hook_opts,
            )
            .await?;

            if json_output {
                let public_info = public_workspace_info(
                    config,
                    config_path,
                    &workspace_name,
                    from.as_deref(),
                    &info,
                );
                println!("{}", serde_json::to_string_pretty(&public_info)?);
            } else {
                println!("Created service workspace: {}", workspace_name);
                if let Some(state) = &info.state {
                    println!("  State: {}", state);
                }
                let public_info = public_workspace_info(
                    config,
                    config_path,
                    &workspace_name,
                    from.as_deref(),
                    &info,
                );
                if let Some(parent) = &public_info.parent_workspace {
                    println!("  Parent: {}", parent);
                }
                // Show connection info
                if let Ok(conn) = provider.get_connection_info(&service_key).await {
                    if let Some(ref uri) = conn.connection_string {
                        println!("  Connection: {}", uri);
                    }
                }
            }
        }
        super::ServiceCommands::Delete { workspace_name } => {
            let service_key =
                resolve_operation_service_key(config_path, &workspace_name, provider.as_ref())
                    .await?;
            let project_dir = super::operation_project_dir(config_path);
            let hook_opts = devflow_core::workspace::LifecycleOptions {
                hook_approval: if non_interactive || json_output {
                    devflow_core::workspace::hooks::HookApprovalMode::NonInteractive
                } else {
                    devflow_core::workspace::hooks::HookApprovalMode::Interactive
                },
                verbose_hooks: !json_output,
                ..Default::default()
            };

            // Fire pre-service-delete hooks
            devflow_core::workspace::hooks::run_lifecycle_hooks(
                config,
                &project_dir,
                &workspace_name,
                HookPhase::PreServiceDelete,
                &hook_opts,
            )
            .await?;

            // Single-service path (explicit --service or single service)
            provider.delete_workspace(&service_key).await?;

            // Fire post-service-delete hooks
            devflow_core::workspace::hooks::run_lifecycle_hooks(
                config,
                &project_dir,
                &workspace_name,
                HookPhase::PostServiceDelete,
                &hook_opts,
            )
            .await?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "status": "ok",
                        "deleted": workspace_name
                    }))?
                );
            } else {
                println!("Deleted service workspace: {}", workspace_name);
            }
        }
        super::ServiceCommands::Cleanup { max_count } => {
            if !provider.supports_cleanup() {
                anyhow::bail!(
                    "Service '{}' does not support cleanup",
                    provider.provider_name()
                );
            }

            let max = max_count.unwrap_or(config.behavior.max_workspaces.unwrap_or(10));
            let deleted = provider.cleanup_old_workspaces(max).await?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "service": resolved_name,
                        "max_count": max,
                        "deleted": deleted,
                    }))?
                );
            } else if deleted.is_empty() {
                println!("No workspaces to clean up on service '{}'", resolved_name);
            } else {
                println!(
                    "Cleaned up {} workspaces on '{}': {}",
                    deleted.len(),
                    resolved_name,
                    deleted.join(", ")
                );
            }
        }
        super::ServiceCommands::Start { workspace_name } => {
            let service_key =
                resolve_operation_service_key(config_path, &workspace_name, provider.as_ref())
                    .await?;
            if !provider.supports_lifecycle() {
                anyhow::bail!(
                    "Service '{}' does not support start/stop lifecycle",
                    provider.provider_name()
                );
            }
            provider.start_workspace(&service_key).await?;

            // Fire post-start hooks
            {
                let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let hook_opts = devflow_core::workspace::LifecycleOptions {
                    hook_approval: if non_interactive || json_output {
                        devflow_core::workspace::hooks::HookApprovalMode::NonInteractive
                    } else {
                        devflow_core::workspace::hooks::HookApprovalMode::Interactive
                    },
                    verbose_hooks: !json_output,
                    ..Default::default()
                };
                devflow_core::workspace::hooks::run_lifecycle_hooks(
                    config,
                    &project_dir,
                    &workspace_name,
                    HookPhase::PostStart,
                    &hook_opts,
                )
                .await?;
            }

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "status": "ok",
                        "started": workspace_name
                    }))?
                );
            } else {
                println!("Started workspace: {}", workspace_name);
            }
        }
        super::ServiceCommands::Stop { workspace_name } => {
            let service_key =
                resolve_operation_service_key(config_path, &workspace_name, provider.as_ref())
                    .await?;
            if !provider.supports_lifecycle() {
                anyhow::bail!(
                    "Service '{}' does not support start/stop lifecycle",
                    provider.provider_name()
                );
            }
            provider.stop_workspace(&service_key).await?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "status": "ok",
                        "stopped": workspace_name
                    }))?
                );
            } else {
                println!("Stopped workspace: {}", workspace_name);
            }
        }
        super::ServiceCommands::Reset { workspace_name } => {
            let service_key =
                resolve_operation_service_key(config_path, &workspace_name, provider.as_ref())
                    .await?;
            if !provider.supports_lifecycle() {
                anyhow::bail!(
                    "Service '{}' does not support reset",
                    provider.provider_name()
                );
            }
            provider.reset_workspace(&service_key).await?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "status": "ok",
                        "reset": workspace_name
                    }))?
                );
            } else {
                println!("Reset workspace: {}", workspace_name);
            }
        }
        super::ServiceCommands::Connection {
            workspace_name,
            format,
        } => {
            let service_key =
                resolve_operation_service_key(config_path, &workspace_name, provider.as_ref())
                    .await?;
            let conn = provider.get_connection_info(&service_key).await?;
            // Global --json flag overrides --format
            let fmt = if json_output {
                "json"
            } else {
                format.as_deref().unwrap_or("uri")
            };
            match fmt {
                "uri" => {
                    if let Some(ref uri) = conn.connection_string {
                        println!("{}", uri);
                    } else {
                        println!(
                            "postgresql://{}@{}:{}/{}",
                            conn.user, conn.host, conn.port, conn.database
                        );
                    }
                }
                "env" => {
                    println!("DATABASE_HOST={}", conn.host);
                    println!("DATABASE_PORT={}", conn.port);
                    println!("DATABASE_NAME={}", conn.database);
                    println!("DATABASE_USER={}", conn.user);
                    if let Some(ref password) = conn.password {
                        println!("DATABASE_PASSWORD={}", password);
                    }
                    if let Some(ref uri) = conn.connection_string {
                        println!("DATABASE_URL={}", uri);
                    }
                }
                _ => {
                    println!("{}", serde_json::to_string_pretty(&conn)?);
                }
            }
        }
        super::ServiceCommands::Destroy { force } => {
            if !provider.supports_destroy() {
                anyhow::bail!(
                    "Service '{}' does not support destroy. This command is only available for the local (Docker + CoW) provider.",
                    provider.provider_name()
                );
            }

            let preview = provider.destroy_preview().await?;
            let (project_name, workspace_names) = match preview {
                Some(p) => p,
                None => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string(&serde_json::json!({
                                "status": "ok",
                                "message": "no project found"
                            }))?
                        );
                    } else {
                        println!(
                            "No project found for service '{}'. Nothing to destroy.",
                            resolved_name
                        );
                    }
                    return Ok(());
                }
            };

            if !force {
                if json_output || non_interactive {
                    anyhow::bail!(
                        "Use --force to confirm destroy in non-interactive or JSON output mode"
                    );
                }

                println!("This will permanently destroy the following:");
                println!("  Project: {}", project_name);
                if workspace_names.is_empty() {
                    println!("  Branches: (none)");
                } else {
                    println!("  Branches ({}):", workspace_names.len());
                    for name in &workspace_names {
                        println!("    - {}", name);
                    }
                }
                println!();
                println!("All containers, storage data, and state will be removed.");

                let confirm =
                    inquire::Confirm::new("Are you sure you want to destroy this project?")
                        .with_default(false)
                        .prompt()?;

                if !confirm {
                    println!("Aborted.");
                    return Ok(());
                }
            }

            let destroyed = provider.destroy_project().await?;

            // Remove the service entry from local state
            if let Some(ref path) = config_path {
                if let Ok(mut state) = LocalStateManager::new() {
                    if let Err(e) = state.remove_service(path, &resolved_name) {
                        log::warn!(
                            "Failed to remove service '{}' from local state: {}",
                            resolved_name,
                            e
                        );
                    }
                }
            }

            // Also remove from committed config for backward compat (legacy configs)
            config.remove_service(&resolved_name);
            if let Some(path) = config_path {
                config.save_to_file(path)?;
            }

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "project": project_name,
                        "destroyed_branches": destroyed,
                    }))?
                );
            } else {
                println!(
                    "Destroyed project '{}' and {} workspace(es)",
                    project_name,
                    destroyed.len()
                );
                for name in &destroyed {
                    println!("  - {}", name);
                }
            }
        }
        super::ServiceCommands::Logs {
            workspace_name,
            tail,
        } => {
            let service_key =
                resolve_operation_service_key(config_path, &workspace_name, provider.as_ref())
                    .await?;
            let output = provider.logs(&service_key, tail).await?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "workspace": workspace_name,
                        "logs": output,
                    }))?
                );
            } else {
                print!("{output}");
            }
        }
        super::ServiceCommands::Seed {
            workspace_name,
            from,
        } => {
            let service_key =
                resolve_operation_service_key(config_path, &workspace_name, provider.as_ref())
                    .await?;
            if !json_output {
                println!("Seeding workspace '{}' from '{}'...", workspace_name, from);
            }
            provider.seed_from_source(&service_key, &from).await?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "seeded": workspace_name,
                        "from": from,
                    }))?
                );
            } else {
                println!("Seed complete.");
            }
        }
        // Add, Remove, List, Status are handled by handle_service_dispatch
        _ => anyhow::bail!("service subcommand is not handled by this dispatch path"),
    }

    Ok(())
}

/// Show top-level status: VCS info + service status.
pub(super) async fn handle_top_level_status(
    config: &mut Config,
    json_output: bool,
    _non_interactive: bool,
    database_name: Option<&str>,
    _config_path: &Option<std::path::PathBuf>,
) -> Result<()> {
    // Show VCS info
    let vcs_info = vcs::detect_vcs_provider(".").ok().and_then(|vcs| {
        let workspace = vcs.current_workspace().ok()?;
        Some(serde_json::json!({
            "provider": vcs.provider_name(),
            "workspace": workspace,
        }))
    });

    let context = super::resolve_branch_context();
    let context_differs_from_cwd = |cwd: &str| {
        let Some(context_branch) = context.context_branch.as_deref() else {
            return false;
        };
        context.source == super::BranchContextSource::EnvOverride && context_branch != cwd
    };

    // Show service info — services are optional; show VCS/project info even without them
    let has_multiple_services = config.resolve_services().len() > 1;
    if database_name.is_none() && has_multiple_services {
        let all_providers = services::factory::create_all_providers(config).await?;
        if json_output {
            let mut services_map = serde_json::Map::new();
            for named in &all_providers {
                let workspaces = named.provider.list_workspaces().await.unwrap_or_default();
                let running = workspaces
                    .iter()
                    .filter(|b| b.state.as_deref() == Some("running"))
                    .count();
                let stopped = workspaces
                    .iter()
                    .filter(|b| b.state.as_deref() == Some("stopped"))
                    .count();
                let project_info = named.provider.project_info();
                let mut status = serde_json::json!({
                    "provider": named.provider.provider_name(),
                    "total_branches": workspaces.len(),
                    "running": running,
                    "stopped": stopped,
                    "supports_lifecycle": named.provider.supports_lifecycle(),
                });
                if let Some(ref info) = project_info {
                    status["project"] = serde_json::Value::String(info.name.clone());
                    if let Some(ref storage) = info.storage_driver {
                        status["storage"] = serde_json::Value::String(storage.clone());
                    }
                    if let Some(ref image) = info.image {
                        status["image"] = serde_json::Value::String(image.clone());
                    }
                }
                services_map.insert(named.name.clone(), status);
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "vcs": vcs_info,
                    "devflow_context_branch": context.context_branch.clone(),
                    "context_source": match context.source {
                        super::BranchContextSource::EnvOverride => "env",
                        super::BranchContextSource::Cwd => "cwd",
                        super::BranchContextSource::None => "none",
                    },
                    "services": services_map,
                }))?
            );
        } else {
            if let Some(ref info) = vcs_info {
                println!(
                    "VCS: {} (workspace: {})",
                    info["provider"].as_str().unwrap_or("unknown"),
                    info["workspace"].as_str().unwrap_or("unknown")
                );
                if let Some(context_branch) = context.context_branch.as_deref() {
                    let cwd = info["workspace"].as_str().unwrap_or("unknown");
                    if context_differs_from_cwd(cwd) {
                        println!("Devflow context workspace: {}", context_branch);
                    }
                }
                println!();
            }
            for named in &all_providers {
                let workspaces = named.provider.list_workspaces().await.unwrap_or_default();
                let running = workspaces
                    .iter()
                    .filter(|b| b.state.as_deref() == Some("running"))
                    .count();
                let stopped = workspaces
                    .iter()
                    .filter(|b| b.state.as_deref() == Some("stopped"))
                    .count();
                let project_info = named.provider.project_info();
                println!("[{}] ({}):", named.name, named.provider.provider_name());
                if let Some(ref info) = project_info {
                    println!("  Project: {}", info.name);
                    if let Some(ref storage) = info.storage_driver {
                        println!("  Storage: {}", storage);
                    }
                    if let Some(ref image) = info.image {
                        println!("  Image: {}", image);
                    }
                }
                println!(
                    "  Branches: {} total ({} running, {} stopped)",
                    workspaces.len(),
                    running,
                    stopped
                );
            }
        }
    } else {
        // Single service or no services — try to resolve, fall back gracefully
        match services::factory::resolve_provider(config, database_name).await {
            Ok(named) => {
                let workspaces = named.provider.list_workspaces().await.unwrap_or_default();
                let running = workspaces
                    .iter()
                    .filter(|b| b.state.as_deref() == Some("running"))
                    .count();
                let stopped = workspaces
                    .iter()
                    .filter(|b| b.state.as_deref() == Some("stopped"))
                    .count();

                let project_info = named.provider.project_info();

                if json_output {
                    let mut service_status = serde_json::json!({
                        "name": named.name,
                        "provider": named.provider.provider_name(),
                        "total_branches": workspaces.len(),
                        "running": running,
                        "stopped": stopped,
                        "supports_lifecycle": named.provider.supports_lifecycle(),
                    });
                    if let Some(ref info) = project_info {
                        service_status["project"] = serde_json::Value::String(info.name.clone());
                        if let Some(ref storage) = info.storage_driver {
                            service_status["storage"] = serde_json::Value::String(storage.clone());
                        }
                        if let Some(ref image) = info.image {
                            service_status["image"] = serde_json::Value::String(image.clone());
                        }
                    }

                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "vcs": vcs_info,
                            "devflow_context_branch": context.context_branch.clone(),
                            "context_source": match context.source {
                                super::BranchContextSource::EnvOverride => "env",
                                super::BranchContextSource::Cwd => "cwd",
                                super::BranchContextSource::None => "none",
                            },
                            "service": service_status,
                        }))?
                    );
                } else {
                    if let Some(ref info) = vcs_info {
                        println!(
                            "VCS: {} (workspace: {})",
                            info["provider"].as_str().unwrap_or("unknown"),
                            info["workspace"].as_str().unwrap_or("unknown")
                        );
                        if let Some(context_branch) = context.context_branch.as_deref() {
                            let cwd = info["workspace"].as_str().unwrap_or("unknown");
                            if context_differs_from_cwd(cwd) {
                                println!("Devflow context workspace: {}", context_branch);
                            }
                        }
                        println!();
                    } else if let Some(context_branch) = context.context_branch.as_deref() {
                        if context.source == super::BranchContextSource::EnvOverride {
                            println!("Devflow context workspace: {}", context_branch);
                            println!();
                        }
                    }
                    println!(
                        "Service: {} ({})",
                        named.name,
                        named.provider.provider_name()
                    );
                    if let Some(ref info) = project_info {
                        println!("  Project: {}", info.name);
                        if let Some(ref storage) = info.storage_driver {
                            println!("  Storage: {}", storage);
                        }
                        if let Some(ref image) = info.image {
                            println!("  Image: {}", image);
                        }
                    }
                    println!(
                        "  Branches: {} total ({} running, {} stopped)",
                        workspaces.len(),
                        running,
                        stopped
                    );
                }
            }
            Err(_) => {
                // No service provider available — show VCS info only
                if json_output {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "vcs": vcs_info,
                            "devflow_context_branch": context.context_branch.clone(),
                            "context_source": match context.source {
                                super::BranchContextSource::EnvOverride => "env",
                                super::BranchContextSource::Cwd => "cwd",
                                super::BranchContextSource::None => "none",
                            },
                            "services": null,
                        }))?
                    );
                } else {
                    if let Some(ref info) = vcs_info {
                        println!(
                            "VCS: {} (workspace: {})",
                            info["provider"].as_str().unwrap_or("unknown"),
                            info["workspace"].as_str().unwrap_or("unknown")
                        );
                        if let Some(context_branch) = context.context_branch.as_deref() {
                            let cwd = info["workspace"].as_str().unwrap_or("unknown");
                            if context_differs_from_cwd(cwd) {
                                println!("Devflow context workspace: {}", context_branch);
                            }
                        }
                        println!();
                    } else if let Some(context_branch) = context.context_branch.as_deref() {
                        if context.source == super::BranchContextSource::EnvOverride {
                            println!("Devflow context workspace: {}", context_branch);
                            println!();
                        }
                    }
                    println!(
                        "Services: none configured (run 'devflow service add' to configure one)"
                    );
                }
            }
        }
    }

    Ok(())
}

/// Handle aggregation commands (List, Status, Doctor) across all services.
pub(super) async fn handle_multi_service_aggregation(
    aggregation: ServiceAggregation,
    config: &Config,
    json_output: bool,
    _config_path: &Option<PathBuf>,
) -> Result<()> {
    let all_providers = match services::factory::create_all_providers(config).await {
        Ok(providers) => providers,
        Err(e) => {
            // Service providers unavailable — degrade gracefully
            log::warn!("Failed to create service providers: {}", e);
            match aggregation {
                ServiceAggregation::Status => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "error": format!("Failed to create service providers: {}", e),
                                "services": null,
                            }))?
                        );
                    } else {
                        println!("Services: failed to initialize providers ({})", e);
                    }
                }
                ServiceAggregation::Doctor => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "error": format!("Failed to create service providers: {}", e),
                                "services": null,
                            }))?
                        );
                    } else {
                        println!("Services:");
                        println!("  [FAIL] Could not initialize providers: {}", e);
                    }
                }
                ServiceAggregation::Capabilities => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "error": format!("Failed to create service providers: {}", e),
                                "services": null,
                            }))?
                        );
                    } else {
                        println!("Services: failed to initialize providers ({})", e);
                    }
                }
            }
            return Ok(());
        }
    };

    match aggregation {
        ServiceAggregation::Status => {
            if json_output {
                let mut map = serde_json::Map::new();
                for named in &all_providers {
                    let workspaces = named.provider.list_workspaces().await.unwrap_or_default();
                    let running = workspaces
                        .iter()
                        .filter(|b| b.state.as_deref() == Some("running"))
                        .count();
                    let stopped = workspaces
                        .iter()
                        .filter(|b| b.state.as_deref() == Some("stopped"))
                        .count();
                    let project_info = named.provider.project_info();

                    let mut status = serde_json::json!({
                        "provider": named.provider.provider_name(),
                        "total_branches": workspaces.len(),
                        "running": running,
                        "stopped": stopped,
                        "supports_lifecycle": named.provider.supports_lifecycle(),
                    });
                    if let Some(ref info) = project_info {
                        status["project"] = serde_json::Value::String(info.name.clone());
                        if let Some(ref storage) = info.storage_driver {
                            status["storage"] = serde_json::Value::String(storage.clone());
                        }
                        if let Some(ref image) = info.image {
                            status["image"] = serde_json::Value::String(image.clone());
                        }
                    }
                    map.insert(named.name.clone(), status);
                }
                println!("{}", serde_json::to_string_pretty(&map)?);
            } else {
                for named in &all_providers {
                    let workspaces = named.provider.list_workspaces().await.unwrap_or_default();
                    let running = workspaces
                        .iter()
                        .filter(|b| b.state.as_deref() == Some("running"))
                        .count();
                    let stopped = workspaces
                        .iter()
                        .filter(|b| b.state.as_deref() == Some("stopped"))
                        .count();
                    let project_info = named.provider.project_info();

                    println!("[{}] ({}):", named.name, named.provider.provider_name());
                    if let Some(ref info) = project_info {
                        println!("  Project: {}", info.name);
                        if let Some(ref storage) = info.storage_driver {
                            println!("  Storage: {}", storage);
                        }
                        if let Some(ref image) = info.image {
                            println!("  Image: {}", image);
                        }
                    }
                    println!(
                        "  Branches: {} total ({} running, {} stopped)",
                        workspaces.len(),
                        running,
                        stopped
                    );
                    if named.provider.supports_lifecycle() {
                        println!("  Lifecycle: supported (start/stop/reset)");
                    }
                    println!();
                }
            }
        }
        ServiceAggregation::Doctor => {
            if json_output {
                let mut map = serde_json::Map::new();
                for named in &all_providers {
                    let report = named.provider.doctor().await?;
                    map.insert(named.name.clone(), serde_json::to_value(&report)?);
                }
                println!("{}", serde_json::to_string_pretty(&map)?);
            } else {
                for named in &all_providers {
                    let report = named.provider.doctor().await?;
                    println!(
                        "[{}] Doctor report ({}):",
                        named.name,
                        named.provider.provider_name()
                    );
                    for check in &report.checks {
                        let icon = if check.available { "OK" } else { "FAIL" };
                        println!("  [{}] {}: {}", icon, check.name, check.detail);
                    }
                    println!();
                }
            }
        }
        ServiceAggregation::Capabilities => {
            if json_output {
                let mut map = serde_json::Map::new();
                for named in &all_providers {
                    map.insert(
                        named.name.clone(),
                        serde_json::json!({
                            "provider": named.provider.provider_name(),
                            "capabilities": named.provider.capabilities(),
                        }),
                    );
                }
                println!("{}", serde_json::to_string_pretty(&map)?);
            } else {
                for named in &all_providers {
                    let caps = named.provider.capabilities();
                    println!("[{}] ({})", named.name, named.provider.provider_name());
                    println!(
                        "  lifecycle={} logs={} seed={} destroy={} cleanup={} template_from_time={} max_workspace_name_length={}",
                        if caps.lifecycle { "yes" } else { "no" },
                        if caps.logs { "yes" } else { "no" },
                        if caps.seed_from_source { "yes" } else { "no" },
                        if caps.destroy_project { "yes" } else { "no" },
                        if caps.cleanup { "yes" } else { "no" },
                        if caps.template_from_time { "yes" } else { "no" },
                        caps.max_workspace_name_length,
                    );
                    println!();
                }
            }
        }
    }

    Ok(())
}

/// Handle Create/Delete across all auto-workspace services when no specific --service is given.
async fn handle_orchestrated_mutation(
    cmd: super::ServiceCommands,
    config: &Config,
    json_output: bool,
    non_interactive: bool,
    config_path: &Option<PathBuf>,
) -> Result<()> {
    let project_dir = super::operation_project_dir(config_path);
    let hook_opts = devflow_core::workspace::LifecycleOptions {
        hook_approval: if non_interactive || json_output {
            devflow_core::workspace::hooks::HookApprovalMode::NonInteractive
        } else {
            devflow_core::workspace::hooks::HookApprovalMode::Interactive
        },
        verbose_hooks: !json_output,
        ..Default::default()
    };

    match cmd {
        super::ServiceCommands::Create {
            workspace_name,
            from,
        } => {
            let service_key = resolve_effective_service_key(config_path, &workspace_name)?;
            let parent_service_key = from
                .as_deref()
                .map(|parent| resolve_effective_service_key(config_path, parent))
                .transpose()?;
            // Fire pre-service-create hooks before orchestrated creation
            devflow_core::workspace::hooks::run_lifecycle_hooks(
                config,
                &project_dir,
                &workspace_name,
                HookPhase::PreServiceCreate,
                &hook_opts,
            )
            .await?;

            let results = services::factory::orchestrate_create(
                config,
                &service_key,
                parent_service_key.as_deref(),
            )
            .await?;
            let success_count = results.iter().filter(|r| r.success).count();
            let fail_count = results.iter().filter(|r| !r.success).count();
            let mut json_payload: Option<serde_json::Value> = None;

            if json_output {
                let json_results: Vec<_> = results
                    .iter()
                    .map(|r| {
                        let branch_info = r.branch_info.as_ref().map(|info| {
                            public_workspace_info(
                                config,
                                config_path,
                                &workspace_name,
                                from.as_deref(),
                                info,
                            )
                        });
                        let message = if r.success {
                            format!(
                                "Created workspace '{}' on {}",
                                workspace_name, r.service_name
                            )
                        } else {
                            r.message.clone()
                        };
                        serde_json::json!({
                            "service": r.service_name,
                            "success": r.success,
                            "message": message,
                            "branch_info": branch_info,
                        })
                    })
                    .collect();
                json_payload = Some(serde_json::json!({
                    "operation": "create",
                    "workspace": workspace_name,
                    "service_key": service_key,
                    "parent": from,
                    "parent_service_key": parent_service_key,
                    "ok": fail_count == 0,
                    "succeeded": success_count,
                    "failed": fail_count,
                    "results": json_results,
                }));
            } else {
                for r in &results {
                    if r.success {
                        println!(
                            "[{}] Created workspace '{}'",
                            r.service_name, workspace_name
                        );
                        if let Some(ref info) = r.branch_info {
                            if let Some(ref state) = info.state {
                                println!("  State: {}", state);
                            }
                        }
                    } else {
                        eprintln!("[{}] {}", r.service_name, r.message);
                    }
                }

                if fail_count > 0 {
                    eprintln!(
                        "\nCreated workspace on {}/{} services ({} failed)",
                        success_count,
                        results.len(),
                        fail_count
                    );
                }
            }

            if fail_count > 0 {
                if let Some(payload) = json_payload.take() {
                    println!("{}", serde_json::to_string_pretty(&payload)?);
                }
                anyhow::bail!(
                    "Failed to create workspace '{}' on {}/{} service(s)",
                    workspace_name,
                    fail_count,
                    results.len()
                );
            }

            // Run hooks after all services are created
            devflow_core::workspace::hooks::run_lifecycle_hooks(
                config,
                &project_dir,
                &workspace_name,
                HookPhase::PostServiceCreate,
                &hook_opts,
            )
            .await?;

            if let Some(payload) = json_payload {
                println!("{}", serde_json::to_string_pretty(&payload)?);
            }
        }
        super::ServiceCommands::Delete { workspace_name } => {
            let service_key = resolve_effective_service_key(config_path, &workspace_name)?;
            // Fire pre-service-delete hooks before orchestrated deletion
            devflow_core::workspace::hooks::run_lifecycle_hooks(
                config,
                &project_dir,
                &workspace_name,
                HookPhase::PreServiceDelete,
                &hook_opts,
            )
            .await?;

            let results = services::factory::orchestrate_delete(config, &service_key).await?;
            let success_count = results.iter().filter(|r| r.success).count();
            let fail_count = results.iter().filter(|r| !r.success).count();

            if json_output {
                let json_results: Vec<_> = results
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "service": r.service_name,
                            "success": r.success,
                            "message": r.message,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "operation": "delete",
                        "workspace": workspace_name,
                        "service_key": service_key,
                        "ok": fail_count == 0,
                        "succeeded": success_count,
                        "failed": fail_count,
                        "results": json_results,
                    }))?
                );
            } else {
                for r in &results {
                    if r.success {
                        println!("[{}] {}", r.service_name, r.message);
                    } else {
                        eprintln!("[{}] {}", r.service_name, r.message);
                    }
                }

                if fail_count > 0 {
                    eprintln!(
                        "\nDeleted workspace on {}/{} services ({} failed)",
                        success_count,
                        results.len(),
                        fail_count
                    );
                }
            }

            // Fire post-service-delete hooks after orchestrated deletion
            devflow_core::workspace::hooks::run_lifecycle_hooks(
                config,
                &project_dir,
                &workspace_name,
                HookPhase::PostServiceDelete,
                &hook_opts,
            )
            .await?;

            if fail_count > 0 {
                anyhow::bail!(
                    "Failed to delete workspace '{}' on {}/{} service(s)",
                    workspace_name,
                    fail_count,
                    results.len()
                );
            }
        }
        _ => anyhow::bail!("service provider subcommand is not handled by this dispatch path"),
    }

    Ok(())
}

/// Handle `devflow service discover` subcommand.
async fn handle_discover(
    service_type: Option<&str>,
    global: bool,
    project_root: Option<&Path>,
    json_output: bool,
) -> Result<()> {
    let scoped_project_root = if global {
        None
    } else if let Some(root) = project_root {
        Some(root.to_path_buf())
    } else {
        Some(std::env::current_dir()?)
    };

    let containers =
        docker::discovery::discover_containers(service_type, scoped_project_root.as_deref())
            .await?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&containers)?);
        return Ok(());
    }

    if containers.is_empty() {
        println!("No matching Docker containers found.");
        return Ok(());
    }

    println!("{:<25} {:<40} {:<25} TYPE", "NAME", "IMAGE", "HOST:PORT");
    println!("{}", "-".repeat(100));
    for c in &containers {
        let compose_label = if c.is_compose {
            format!(" ({})", c.compose_project.as_deref().unwrap_or("compose"))
        } else {
            String::new()
        };
        println!(
            "{:<25} {:<40} {:<25} {}{}",
            c.container_name,
            c.image,
            format!("{}:{}", c.host, c.port),
            format!("{:?}", c.service_type).to_lowercase(),
            compose_label,
        );
    }

    Ok(())
}

/// Info extracted from a discovered Docker container during `service add`.
pub(super) struct DiscoveredServiceInfo {
    pub image: String,
    pub seed_url: String,
    pub name: String,
    pub docker_settings: Option<devflow_core::config::DockerCustomSettings>,
}

/// Offer discovered Docker containers to the user during `service add` interactive wizard.
/// Returns `DiscoveredServiceInfo` if user picks a container, or `None` to skip.
pub(super) async fn offer_discovered_containers(
    service_type: &str,
    project_root: Option<&Path>,
    non_interactive: bool,
    json_output: bool,
) -> Option<DiscoveredServiceInfo> {
    if non_interactive || json_output {
        return None;
    }

    let containers =
        match docker::discovery::discover_containers(Some(service_type), project_root).await {
            Ok(c) if !c.is_empty() => c,
            _ => return None,
        };

    let options: Vec<String> = containers
        .iter()
        .map(|c| {
            let compose_tag = if c.is_compose {
                format!(" [{}]", c.compose_project.as_deref().unwrap_or("compose"))
            } else {
                String::new()
            };
            format!(
                "{} — {} ({}:{}){}",
                c.container_name, c.image, c.host, c.port, compose_tag
            )
        })
        .collect();

    let mut all_options = vec!["Skip — configure manually".to_string()];
    all_options.extend(options);

    let selection = inquire::Select::new(
        "Detected running Docker containers. Import settings?",
        all_options,
    )
    .with_help_message("Select a container to pre-fill image, seed URL, and name")
    .prompt();

    match selection {
        Ok(s) if s.starts_with("Skip") => None,
        Ok(s) => {
            // Find which container was selected
            let idx = containers
                .iter()
                .position(|c| s.starts_with(&c.container_name))
                .unwrap_or(0);
            let c = &containers[idx];
            let name = c.compose_service.clone().unwrap_or_else(|| {
                c.container_name
                    .replace(|ch: char| !ch.is_alphanumeric() && ch != '-', "-")
            });
            let docker_settings = {
                let settings = devflow_core::config::DockerCustomSettings {
                    command: c.command.clone(),
                    environment: c.extra_env.clone(),
                    restart_policy: c.restart_policy.clone(),
                };
                if settings.is_empty() {
                    None
                } else {
                    Some(settings)
                }
            };
            Some(DiscoveredServiceInfo {
                image: c.image.clone(),
                seed_url: c.connection_url.clone(),
                name,
                docker_settings,
            })
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{public_workspace_info, validate_scaffoldable_service};
    use devflow_core::{config::Config, services::WorkspaceInfo};

    #[test]
    fn public_workspace_info_separates_raw_and_backend_identities() {
        let config = Config::default();
        let raw_workspace = "feature/Auth.flow";
        let raw_parent = "release/2026.07";
        let service_key = config.get_service_workspace_key(raw_workspace);
        let parent_service_key = config.get_service_workspace_key(raw_parent);
        let backend = WorkspaceInfo {
            name: service_key.clone(),
            created_at: None,
            parent_workspace: Some(parent_service_key.clone()),
            database_name: format!("app_{service_key}"),
            state: Some("running".into()),
        };

        let public =
            public_workspace_info(&config, &None, raw_workspace, Some(raw_parent), &backend);
        let value = serde_json::to_value(public).unwrap();

        assert_eq!(value["name"], raw_workspace);
        assert_eq!(value["workspace"], raw_workspace);
        assert_eq!(value["service_key"], service_key);
        assert_eq!(value["parent_workspace"], raw_parent);
        assert_eq!(value["parent_service_key"], parent_service_key);
        assert_eq!(value["state"], "running");
        assert!(value.get("database_name").is_some());
        assert!(value.get("created_at").is_some());
    }

    #[test]
    fn public_workspace_info_maps_the_configured_default_parent() {
        let config = Config::default();
        let default_workspace = config.git.main_workspace.clone();
        let default_key = config.get_service_workspace_key(&default_workspace);
        let backend = WorkspaceInfo {
            name: "feature_auth-abc123".into(),
            created_at: None,
            parent_workspace: Some(default_key.clone()),
            database_name: "app_feature_auth".into(),
            state: None,
        };

        let public = public_workspace_info(&config, &None, "feature/auth", None, &backend);
        assert_eq!(
            public.parent_workspace.as_deref(),
            Some(default_workspace.as_str())
        );
        assert_eq!(
            public.parent_service_key.as_deref(),
            Some(default_key.as_str())
        );
    }

    #[test]
    fn service_add_only_accepts_complete_scaffolds() {
        for (service_type, provider) in [
            ("postgres", "local"),
            ("postgres", "shared"),
            ("clickhouse", "local"),
            ("mysql", "local"),
            ("redis", "shared"),
            ("rustfs", "shared"),
        ] {
            validate_scaffoldable_service(service_type, provider).unwrap();
        }

        for (service_type, provider) in [
            ("postgres", "neon"),
            ("generic", "local"),
            ("plugin", "local"),
            ("mysql", "shared"),
        ] {
            assert!(validate_scaffoldable_service(service_type, provider).is_err());
        }
    }
}

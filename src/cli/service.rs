use std::path::{Path, PathBuf};

use anyhow::Result;
use devflow_core::config::{Config, EffectiveConfig};
use devflow_core::docker;
use devflow_core::hooks::HookPhase;
use devflow_core::services::{self};
use devflow_core::state::LocalStateManager;
use devflow_core::vcs;

/// Internal enum for multi-service aggregation dispatch.
pub(super) enum ServiceAggregation {
    List,
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
            "generic     — Generic Docker container",
            "plugin      — External plugin",
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
                "local               — Docker container on this machine",
                "neon                 — Neon serverless Postgres (cloud)",
                "dblab               — Database Lab Engine (clone-based branching)",
                "xata                — Xata serverless database (cloud)",
            ],
            "clickhouse" => vec!["local               — Docker container on this machine"],
            "mysql" => vec!["local               — Docker container on this machine"],
            "generic" => vec!["local               — Docker container on this machine"],
            "plugin" => vec!["local               — Managed by plugin"],
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
            "generic" => "app".to_string(),
            "plugin" => "plugin".to_string(),
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
                "generic" => "app",
                "plugin" => "plugin",
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
    let named_cfg = devflow_core::config::NamedServiceConfig {
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

    // Store service in local state
    let mut state = LocalStateManager::new()?;
    state.add_service(config_path, named_cfg.clone(), false)?;
    if !json_output {
        println!("Added service '{}' ({})", name, service_type);
    }

    // Use explicit seed source or discovered container's connection URL
    let effective_seed = from.map(|s| s.to_string()).or(discovered_seed);

    // Create main workspace for local providers
    if is_local {
        let mut config_with_service = config.clone();
        if let Some(state_services) = state.get_services(config_path) {
            config_with_service.services = Some(state_services);
        }

        #[cfg(feature = "service-local")]
        if cfg!(target_os = "linux") {
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
                let service_type =
                    service_type.unwrap_or_else(devflow_core::config::default_service_type);
                let provider_type = provider.unwrap_or_else(|| "local".to_string());
                let name = if let Some(n) = name {
                    n
                } else if non_interactive || json_output {
                    anyhow::bail!("Service name is required in non-interactive mode. Usage: devflow service add <name>");
                } else {
                    use inquire::Text;
                    let default_name = match service_type.as_str() {
                        "clickhouse" => "analytics",
                        "mysql" => "mysql",
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

                if is_local {
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
            // List configured services (not workspaces)
            let has_multiple_services = config.resolve_services().len() > 1;
            if database_name.is_none() && has_multiple_services {
                return handle_multi_service_aggregation(
                    ServiceAggregation::List,
                    config,
                    json_output,
                    config_path,
                )
                .await;
            }
            let named = services::factory::resolve_provider(config, database_name).await?;
            let workspaces = named.provider.list_workspaces().await?;
            if json_output {
                let enriched = enrich_branch_list_json(&workspaces, config, config_path);
                println!("{}", serde_json::to_string_pretty(&enriched)?);
            } else {
                println!("Branches ({}):", named.provider.provider_name());
                print_enriched_branch_list(&workspaces, config, config_path);
            }
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
        return handle_orchestrated_mutation(cmd, config, json_output, non_interactive).await;
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
                .create_workspace(&workspace_name, from.as_deref())
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
                println!("{}", serde_json::to_string_pretty(&info)?);
            } else {
                println!("Created service workspace: {}", info.name);
                if let Some(state) = &info.state {
                    println!("  State: {}", state);
                }
                if let Some(parent) = &info.parent_workspace {
                    println!("  Parent: {}", parent);
                }
                // Show connection info
                if let Ok(conn) = provider.get_connection_info(&workspace_name).await {
                    if let Some(ref uri) = conn.connection_string {
                        println!("  Connection: {}", uri);
                    }
                }
            }
        }
        super::ServiceCommands::Delete { workspace_name } => {
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
            provider.delete_workspace(&workspace_name).await?;

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
            if !provider.supports_lifecycle() {
                anyhow::bail!(
                    "Service '{}' does not support start/stop lifecycle",
                    provider.provider_name()
                );
            }
            provider.start_workspace(&workspace_name).await?;

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
            if !provider.supports_lifecycle() {
                anyhow::bail!(
                    "Service '{}' does not support start/stop lifecycle",
                    provider.provider_name()
                );
            }
            provider.stop_workspace(&workspace_name).await?;
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
            if !provider.supports_lifecycle() {
                anyhow::bail!(
                    "Service '{}' does not support reset",
                    provider.provider_name()
                );
            }
            provider.reset_workspace(&workspace_name).await?;
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
            let conn = provider.get_connection_info(&workspace_name).await?;
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
            let output = provider.logs(&workspace_name, tail).await?;
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
            if !json_output {
                println!("Seeding workspace '{}' from '{}'...", workspace_name, from);
            }
            provider.seed_from_source(&workspace_name, &from).await?;
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
        _ => unreachable!(),
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

    let context = super::resolve_branch_context(config);
    let context_differs_from_cwd = |cwd: &str| {
        let Some(context_branch) = context.context_branch.as_deref() else {
            return false;
        };
        let normalized_cwd = config.get_normalized_workspace_name(cwd);
        context.source == super::BranchContextSource::EnvOverride
            && context_branch != cwd
            && context_branch != normalized_cwd
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
                services_map.insert(
                    named.name.clone(),
                    serde_json::json!({
                        "provider": named.provider.provider_name(),
                        "total_branches": workspaces.len(),
                        "running": running,
                        "stopped": stopped,
                    }),
                );
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
                println!("[{}] ({}):", named.name, named.provider.provider_name());
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
                            "service": {
                                "name": named.name,
                                "provider": named.provider.provider_name(),
                                "total_branches": workspaces.len(),
                                "running": running,
                                "stopped": stopped,
                            },
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
    config_path: &Option<PathBuf>,
) -> Result<()> {
    let all_providers = match services::factory::create_all_providers(config).await {
        Ok(providers) => providers,
        Err(e) => {
            // Service providers unavailable — degrade gracefully
            log::warn!("Failed to create service providers: {}", e);
            match aggregation {
                ServiceAggregation::List => {
                    // Show workspace registry info without service data
                    if json_output {
                        let enriched = enrich_branch_list_json(&[], config, config_path);
                        println!("{}", serde_json::to_string_pretty(&enriched)?);
                    } else {
                        println!("Branches (no service providers available):");
                        print_enriched_branch_list(&[], config, config_path);
                    }
                }
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
        ServiceAggregation::List => {
            // Gather all service workspaces from all services
            let mut all_service_branches: Vec<services::WorkspaceInfo> = Vec::new();
            if json_output {
                let mut map = serde_json::Map::new();
                for named in &all_providers {
                    let workspaces = named.provider.list_workspaces().await.unwrap_or_default();
                    map.insert(
                        named.name.clone(),
                        enrich_branch_list_json(&workspaces, config, config_path),
                    );
                }
                println!("{}", serde_json::to_string_pretty(&map)?);
            } else {
                for named in &all_providers {
                    let workspaces = named.provider.list_workspaces().await.unwrap_or_default();
                    all_service_branches.extend(workspaces);
                    println!("[{}] ({}):", named.name, named.provider.provider_name());
                }
                print_enriched_branch_list(&all_service_branches, config, config_path);
                println!();
            }
        }
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
) -> Result<()> {
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

    match cmd {
        super::ServiceCommands::Create {
            workspace_name,
            from,
        } => {
            // Fire pre-service-create hooks before orchestrated creation
            devflow_core::workspace::hooks::run_lifecycle_hooks(
                config,
                &project_dir,
                &workspace_name,
                HookPhase::PreServiceCreate,
                &hook_opts,
            )
            .await?;

            let results =
                services::factory::orchestrate_create(config, &workspace_name, from.as_deref())
                    .await?;
            let success_count = results.iter().filter(|r| r.success).count();
            let fail_count = results.iter().filter(|r| !r.success).count();
            let mut json_payload: Option<serde_json::Value> = None;

            if json_output {
                let json_results: Vec<_> = results
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "service": r.service_name,
                            "success": r.success,
                            "message": r.message,
                            "branch_info": r.branch_info,
                        })
                    })
                    .collect();
                json_payload = Some(serde_json::json!({
                    "operation": "create",
                    "workspace": workspace_name,
                    "ok": fail_count == 0,
                    "succeeded": success_count,
                    "failed": fail_count,
                    "results": json_results,
                }));
            } else {
                for r in &results {
                    if r.success {
                        println!("[{}] {}", r.service_name, r.message);
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
            // Fire pre-service-delete hooks before orchestrated deletion
            devflow_core::workspace::hooks::run_lifecycle_hooks(
                config,
                &project_dir,
                &workspace_name,
                HookPhase::PreServiceDelete,
                &hook_opts,
            )
            .await?;

            let results = services::factory::orchestrate_delete(config, &workspace_name).await?;
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
        _ => unreachable!(),
    }

    Ok(())
}

/// Unifies information from the VCS provider, the service provider, and the
/// workspace registry (for parent-child relationships) into a single tree view.
pub(super) fn print_enriched_branch_list(
    service_branches: &[services::WorkspaceInfo],
    config: &Config,
    config_path: &Option<PathBuf>,
) {
    use std::collections::{HashMap, HashSet};

    // Gather VCS + worktree info
    let vcs_provider = vcs::detect_vcs_provider(".").ok();
    let git_branches: Vec<devflow_core::vcs::WorkspaceInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_workspaces().ok())
        .unwrap_or_default();
    let worktrees: Vec<devflow_core::vcs::WorktreeInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_worktrees().ok())
        .unwrap_or_default();
    let current_git = vcs_provider
        .as_ref()
        .and_then(|r| r.current_workspace().ok().flatten());
    let current_normalized = current_git
        .as_deref()
        .map(|b| config.get_normalized_workspace_name(b));

    // Build a set of service workspace names for quick lookup
    let mut service_names: HashSet<String> = HashSet::new();
    for b in service_branches {
        service_names.insert(b.name.clone());
        service_names.insert(config.get_normalized_workspace_name(&b.name));
    }

    // Build a worktree lookup: workspace name -> path
    let mut wt_lookup: HashMap<String, PathBuf> = HashMap::new();
    for wt in &worktrees {
        if let Some(workspace) = wt.workspace.as_ref() {
            wt_lookup.insert(workspace.clone(), wt.path.clone());
            wt_lookup
                .entry(config.get_normalized_workspace_name(workspace))
                .or_insert_with(|| wt.path.clone());
        }
    }

    // Load workspace registry from local state
    let registry_branches = super::load_registry_branches_for_list(config, config_path);
    let registry: HashMap<String, Option<String>> = registry_branches
        .iter()
        .map(|b| (b.name.clone(), b.parent.clone()))
        .collect();

    let context = super::resolve_branch_context(config);

    // Registry-first scope: align CLI with GUI/TUI workspace model.
    let all_names =
        super::collect_list_workspace_names(&registry_branches, &git_branches, service_branches);
    let seen: HashSet<&str> = all_names.iter().map(|s| s.as_str()).collect();

    if all_names.is_empty() {
        println!("  (none)");
        return;
    }

    // Build parent map: child_name -> parent_name
    // Sources: 1) service-level parent, 2) registry parent (takes precedence)
    let mut parent_map: HashMap<&str, &str> = HashMap::new();

    for sb in service_branches {
        if !seen.contains(sb.name.as_str()) {
            continue;
        }
        if let Some(ref parent) = sb.parent_workspace {
            if seen.contains(parent.as_str()) {
                parent_map.insert(sb.name.as_str(), parent.as_str());
            }
        }
    }
    for name in &all_names {
        if let Some(Some(ref parent)) = registry.get(name.as_str()) {
            if seen.contains(parent.as_str()) {
                parent_map.insert(name.as_str(), parent.as_str());
            }
        }
    }

    // Build children map
    let mut children_map: HashMap<&str, Vec<&str>> = HashMap::new();
    for (child, parent) in &parent_map {
        children_map.entry(parent).or_default().push(child);
    }
    // Sort children alphabetically for deterministic output
    for kids in children_map.values_mut() {
        kids.sort();
    }

    // Find root nodes (no parent, or parent not in the known set)
    let mut roots: Vec<&str> = all_names
        .iter()
        .filter(|name| !parent_map.contains_key(name.as_str()))
        .map(|s| s.as_str())
        .collect();

    // Sort roots: default workspace first, then context workspace, then cwd, then alphabetical
    let default_workspace = config.get_normalized_workspace_name(&config.git.main_workspace);
    roots.sort_by(|a, b| {
        let a_default = *a == default_workspace
            || git_branches.iter().any(|gb| {
                gb.is_default
                    && (gb.name == *a || config.get_normalized_workspace_name(&gb.name) == *a)
            });
        let b_default = *b == default_workspace
            || git_branches.iter().any(|gb| {
                gb.is_default
                    && (gb.name == *b || config.get_normalized_workspace_name(&gb.name) == *b)
            });
        if a_default != b_default {
            return b_default.cmp(&a_default);
        }
        let a_context = super::context_matches_branch(config, context.context_branch.as_deref(), a);
        let b_context = super::context_matches_branch(config, context.context_branch.as_deref(), b);
        if a_context != b_context {
            return b_context.cmp(&a_context);
        }
        let a_current =
            current_git.as_deref() == Some(*a) || current_normalized.as_deref() == Some(*a);
        let b_current =
            current_git.as_deref() == Some(*b) || current_normalized.as_deref() == Some(*b);
        if a_current != b_current {
            return b_current.cmp(&a_current);
        }
        a.cmp(b)
    });

    if context.source == super::BranchContextSource::EnvOverride {
        if let Some(context_branch) = context.context_branch.as_deref() {
            let cwd = context.cwd_branch.as_deref().unwrap_or("unknown");
            println!(
                "Context override: '{}' (from DEVFLOW_CONTEXT_BRANCH), cwd workspace='{}'",
                context_branch, cwd
            );
        }
    }

    // Recursive tree printer
    #[allow(clippy::too_many_arguments)]
    fn print_node(
        name: &str,
        prefix: &str,
        connector: &str,
        children_map: &HashMap<&str, Vec<&str>>,
        current_git: &Option<String>,
        current_normalized: &Option<String>,
        context_branch: Option<&str>,
        service_branches: &[services::WorkspaceInfo],
        service_names: &HashSet<String>,
        wt_lookup: &HashMap<String, PathBuf>,
        config: &Config,
        _git_branches: &[devflow_core::vcs::WorkspaceInfo],
    ) {
        let is_current =
            current_git.as_deref() == Some(name) || current_normalized.as_deref() == Some(name);
        let marker = if is_current { "* " } else { "  " };
        let is_context = super::context_matches_branch(config, context_branch, name);

        let normalized = config.get_normalized_workspace_name(name);
        let has_service = service_names.contains(&normalized) || service_names.contains(name);

        let service_state = service_branches
            .iter()
            .find(|b| b.name == normalized || b.name == name)
            .and_then(|b| b.state.as_deref());

        let wt_path = wt_lookup.get(name);

        let mut parts = Vec::new();
        if let Some(state) = service_state {
            parts.push(format!("service: {}", state));
        } else if has_service {
            parts.push("service: ok".to_string());
        }
        if let Some(path) = wt_path {
            parts.push(format!("worktree: {}", path.display()));
        }
        if is_context {
            parts.push("context".to_string());
        }

        let suffix = if parts.is_empty() {
            String::new()
        } else {
            format!("  [{}]", parts.join(", "))
        };

        if connector.is_empty() {
            println!("{}{}{}", marker, name, suffix);
        } else {
            println!("{}{}{}{}", marker, connector, name, suffix);
        }

        if let Some(kids) = children_map.get(name) {
            let count = kids.len();
            for (i, child) in kids.iter().enumerate() {
                let is_last = i == count - 1;
                let child_connector = if is_last {
                    format!("{}└─ ", prefix)
                } else {
                    format!("{}├─ ", prefix)
                };
                let child_prefix = if is_last {
                    format!("{}   ", prefix)
                } else {
                    format!("{}│  ", prefix)
                };
                print_node(
                    child,
                    &child_prefix,
                    &child_connector,
                    children_map,
                    current_git,
                    current_normalized,
                    context_branch,
                    service_branches,
                    service_names,
                    wt_lookup,
                    config,
                    _git_branches,
                );
            }
        }
    }

    for root in &roots {
        print_node(
            root,
            "  ",
            "",
            &children_map,
            &current_git,
            &current_normalized,
            context.context_branch.as_deref(),
            service_branches,
            &service_names,
            &wt_lookup,
            config,
            &git_branches,
        );
    }
}

/// Build enriched JSON for the list command, merging git + worktree + service info.
pub(super) fn enrich_branch_list_json(
    service_branches: &[services::WorkspaceInfo],
    config: &Config,
    config_path: &Option<PathBuf>,
) -> serde_json::Value {
    let vcs_provider = vcs::detect_vcs_provider(".").ok();
    let git_branches: Vec<devflow_core::vcs::WorkspaceInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_workspaces().ok())
        .unwrap_or_default();
    let worktrees: Vec<devflow_core::vcs::WorktreeInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_worktrees().ok())
        .unwrap_or_default();
    let current_git = vcs_provider
        .as_ref()
        .and_then(|r| r.current_workspace().ok().flatten());
    let current_normalized = current_git
        .as_deref()
        .map(|b| config.get_normalized_workspace_name(b));

    let mut wt_lookup: std::collections::HashMap<String, PathBuf> =
        std::collections::HashMap::new();
    for wt in &worktrees {
        if let Some(workspace) = wt.workspace.as_ref() {
            wt_lookup.insert(workspace.clone(), wt.path.clone());
            wt_lookup
                .entry(config.get_normalized_workspace_name(workspace))
                .or_insert_with(|| wt.path.clone());
        }
    }

    let mut service_map: std::collections::HashMap<String, &services::WorkspaceInfo> =
        std::collections::HashMap::new();
    for b in service_branches {
        service_map.entry(b.name.clone()).or_insert(b);
        service_map
            .entry(config.get_normalized_workspace_name(&b.name))
            .or_insert(b);
    }

    let registry_branches = super::load_registry_branches_for_list(config, config_path);
    let registry: std::collections::HashMap<String, Option<String>> = registry_branches
        .iter()
        .map(|b| (b.name.clone(), b.parent.clone()))
        .collect();

    let context = super::resolve_branch_context(config);

    let mut entries = Vec::new();

    let all_names =
        super::collect_list_workspace_names(&registry_branches, &git_branches, service_branches);
    let default_workspace = config.get_normalized_workspace_name(&config.git.main_workspace);

    for name in &all_names {
        let normalized = config.get_normalized_workspace_name(name);
        let sb = service_map
            .get(name)
            .or_else(|| service_map.get(&normalized))
            .copied();
        let wt = wt_lookup.get(name).or_else(|| wt_lookup.get(&normalized));
        let is_context =
            super::context_matches_branch(config, context.context_branch.as_deref(), name);
        let is_current = current_git.as_deref() == Some(name.as_str())
            || current_normalized.as_deref() == Some(name.as_str());
        let is_default = *name == default_workspace
            || git_branches.iter().any(|gb| {
                gb.is_default
                    && (gb.name == *name || config.get_normalized_workspace_name(&gb.name) == *name)
            });

        let mut entry = serde_json::json!({
            "name": name,
            "is_current": is_current,
            "is_default": is_default,
            "is_context": is_context,
        });

        if let Some(svc) = sb {
            entry["service"] = serde_json::json!({
                "database": svc.database_name,
                "state": svc.state,
                "parent": svc.parent_workspace,
            });
        }

        if let Some(path) = wt {
            entry["worktree_path"] = serde_json::Value::String(path.display().to_string());
        }

        // Parent from registry (preferred) or service
        let parent = registry
            .get(name)
            .and_then(|p| p.clone())
            .or_else(|| registry.get(&normalized).and_then(|p| p.clone()))
            .or_else(|| sb.and_then(|s| s.parent_workspace.clone()));
        if let Some(parent_name) = parent {
            entry["parent"] = serde_json::Value::String(parent_name);
        }

        entries.push(entry);
    }

    serde_json::Value::Array(entries)
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

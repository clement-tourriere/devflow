#[cfg(feature = "service-dblab")]
use super::postgres::dblab::DBLabProvider;
#[cfg(feature = "service-local")]
use super::postgres::local::LocalProvider;
#[cfg(feature = "service-neon")]
use super::postgres::neon::NeonProvider;
#[cfg(feature = "service-postgres-template")]
use super::postgres::template::PostgresTemplateProvider;
#[cfg(feature = "service-xata")]
use super::postgres::xata::XataProvider;

#[cfg(feature = "service-local")]
use super::clickhouse::local::ClickHouseLocalProvider;
#[cfg(feature = "service-local")]
use super::generic::GenericDockerProvider;
#[cfg(feature = "service-local")]
use super::mysql::local::MySQLLocalProvider;

use super::plugin::PluginProvider;
use super::ServiceProvider;
use crate::config::{Config, NamedServiceConfig};
use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProviderType {
    #[cfg(feature = "service-local")]
    Local,
    #[cfg(feature = "service-postgres-template")]
    PostgresTemplate,
    #[cfg(feature = "service-neon")]
    Neon,
    #[cfg(feature = "service-dblab")]
    DBLab,
    #[cfg(feature = "service-xata")]
    Xata,
}

impl ProviderType {
    pub fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            #[cfg(feature = "service-local")]
            "local" | "docker" => Ok(ProviderType::Local),
            #[cfg(not(feature = "service-local"))]
            "local" | "docker" => anyhow::bail!("Local provider not compiled. Rebuild with --features service-local"),

            #[cfg(feature = "service-postgres-template")]
            "postgres_template" | "postgres" | "postgresql" => Ok(ProviderType::PostgresTemplate),
            #[cfg(not(feature = "service-postgres-template"))]
            "postgres_template" | "postgres" | "postgresql" => anyhow::bail!("PostgreSQL template provider not compiled. Rebuild with --features service-postgres-template"),

            #[cfg(feature = "service-neon")]
            "neon" => Ok(ProviderType::Neon),
            #[cfg(not(feature = "service-neon"))]
            "neon" => anyhow::bail!("Neon provider not compiled. Rebuild with --features service-neon"),

            #[cfg(feature = "service-dblab")]
            "dblab" | "database_lab" => Ok(ProviderType::DBLab),
            #[cfg(not(feature = "service-dblab"))]
            "dblab" | "database_lab" => anyhow::bail!("DBLab provider not compiled. Rebuild with --features service-dblab"),

            #[cfg(feature = "service-xata")]
            "xata" | "xata_lite" => Ok(ProviderType::Xata),
            #[cfg(not(feature = "service-xata"))]
            "xata" | "xata_lite" => anyhow::bail!("Xata provider not compiled. Rebuild with --features service-xata"),

            _ => anyhow::bail!("Unknown provider type: {}. Valid types: local, postgres_template, neon, dblab, xata", s),
        }
    }

    pub fn is_local(s: &str) -> bool {
        matches!(s.to_lowercase().as_str(), "local" | "docker")
    }
}

pub struct NamedService {
    pub name: String,
    pub provider: Box<dyn ServiceProvider>,
}

/// Create a provider from a NamedServiceConfig.
///
/// Dispatches based on `service_type` first (postgres, clickhouse, mysql, generic),
/// then on `provider_type` (local, neon, dblab, etc.) for postgres services.
pub async fn create_provider_from_named_config(
    config: &Config,
    named: &NamedServiceConfig,
) -> Result<Box<dyn ServiceProvider>> {
    let project_name = config.project_name();

    match named.service_type.as_str() {
        "postgres" | "" => {
            // Dispatch on provider_type for postgres services
            create_postgres_provider(config, named).await
        }

        #[cfg(feature = "service-local")]
        "clickhouse" => {
            let ch_config = named.clickhouse.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Service '{}' has type 'clickhouse' but no clickhouse config section",
                    named.name
                )
            })?;
            let provider = ClickHouseLocalProvider::new(&project_name, &named.name, ch_config)
                .context("Failed to create ClickHouse provider")?;
            Ok(Box::new(provider))
        }
        #[cfg(not(feature = "service-local"))]
        "clickhouse" => {
            anyhow::bail!("ClickHouse provider requires the 'service-local' feature (Docker support). Rebuild with --features service-local")
        }

        #[cfg(feature = "service-local")]
        "mysql" | "mariadb" => {
            let mysql_config = named.mysql.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Service '{}' has type '{}' but no mysql config section",
                    named.name,
                    named.service_type
                )
            })?;
            let provider = MySQLLocalProvider::new(&project_name, &named.name, mysql_config)
                .context("Failed to create MySQL provider")?;
            Ok(Box::new(provider))
        }
        #[cfg(not(feature = "service-local"))]
        "mysql" | "mariadb" => {
            anyhow::bail!("MySQL provider requires the 'service-local' feature (Docker support). Rebuild with --features service-local")
        }

        #[cfg(feature = "service-local")]
        "generic" => {
            let generic_config = named.generic.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Service '{}' has type 'generic' but no generic config section",
                    named.name
                )
            })?;
            let provider = GenericDockerProvider::new(&project_name, &named.name, generic_config)
                .context("Failed to create generic Docker provider")?;
            Ok(Box::new(provider))
        }
        #[cfg(not(feature = "service-local"))]
        "generic" => {
            anyhow::bail!("Generic Docker provider requires the 'service-local' feature. Rebuild with --features service-local")
        }

        "plugin" => {
            let plugin_config = named.plugin.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Service '{}' has type 'plugin' but no plugin config section",
                    named.name
                )
            })?;
            let provider = PluginProvider::new(&named.name, plugin_config)
                .context("Failed to create plugin provider")?;
            Ok(Box::new(provider))
        }

        other => {
            anyhow::bail!(
                "Unknown service type '{}' for service '{}'. Valid types: postgres, clickhouse, mysql, mariadb, generic, plugin",
                other,
                named.name
            )
        }
    }
}

/// Create a postgres-specific provider, dispatching on `provider_type`.
async fn create_postgres_provider(
    config: &Config,
    named: &NamedServiceConfig,
) -> Result<Box<dyn ServiceProvider>> {
    let provider_type = ProviderType::from_str(&named.provider_type)?;

    match provider_type {
        #[cfg(feature = "service-local")]
        ProviderType::Local => {
            let local_config = named.local.as_ref();
            let provider = LocalProvider::new(&named.name, config, local_config)
                .await
                .context("Failed to create local provider")?;
            Ok(Box::new(provider))
        }
        #[cfg(feature = "service-postgres-template")]
        ProviderType::PostgresTemplate => {
            let provider = PostgresTemplateProvider::new(config)
                .await
                .context("Failed to create PostgreSQL template provider")?;
            Ok(Box::new(provider))
        }
        #[cfg(feature = "service-neon")]
        ProviderType::Neon => {
            if let Some(ref neon_config) = named.neon {
                let provider = NeonProvider::new(
                    resolve_env_var(&neon_config.api_key)?,
                    resolve_env_var(&neon_config.project_id)?,
                    Some(neon_config.base_url.clone()),
                )?;
                Ok(Box::new(provider))
            } else {
                anyhow::bail!("Neon provider selected but no neon configuration provided");
            }
        }
        #[cfg(feature = "service-dblab")]
        ProviderType::DBLab => {
            if let Some(ref dblab_config) = named.dblab {
                let provider = DBLabProvider::new(
                    resolve_env_var(&dblab_config.api_url)?,
                    resolve_env_var(&dblab_config.auth_token)?,
                )?;
                Ok(Box::new(provider))
            } else {
                anyhow::bail!("DBLab provider selected but no dblab configuration provided");
            }
        }
        #[cfg(feature = "service-xata")]
        ProviderType::Xata => {
            if let Some(ref xata_config) = named.xata {
                let provider = XataProvider::new(
                    resolve_env_var(&xata_config.api_key)?,
                    resolve_env_var(&xata_config.organization_id)?,
                    resolve_env_var(&xata_config.project_id)?,
                    Some(xata_config.base_url.clone()),
                )?;
                Ok(Box::new(provider))
            } else {
                anyhow::bail!("Xata provider selected but no xata configuration provided");
            }
        }
    }
}

/// Resolve a single service provider by name (or the default).
pub async fn resolve_provider(config: &Config, service_name: Option<&str>) -> Result<NamedService> {
    config.validate_services()?;

    let services = config.resolve_services();

    if services.is_empty() {
        if service_name.is_some() {
            anyhow::bail!(
                "No services configured. Run 'devflow service add' before using --service."
            );
        }
        anyhow::bail!("No services configured. Run 'devflow service add' to configure one.");
    }

    let named = if let Some(name) = service_name {
        services
            .iter()
            .find(|b| b.name == name)
            .ok_or_else(|| anyhow::anyhow!("Service '{}' not found in configuration", name))?
    } else {
        services
            .iter()
            .find(|b| b.default)
            .or(services.first())
            .ok_or_else(|| anyhow::anyhow!("No services configured"))?
    };

    let provider = create_provider_from_named_config(config, named).await?;
    Ok(NamedService {
        name: named.name.clone(),
        provider,
    })
}

/// Instantiate all configured service providers.
pub async fn create_all_providers(config: &Config) -> Result<Vec<NamedService>> {
    config.validate_services()?;

    let named_configs = config.resolve_services();

    if named_configs.is_empty() {
        return Ok(Vec::new());
    }

    let mut result = Vec::with_capacity(named_configs.len());
    for named in &named_configs {
        let provider = create_provider_from_named_config(config, named).await?;
        result.push(NamedService {
            name: named.name.clone(),
            provider,
        });
    }

    Ok(result)
}

/// Auto-detect provider when no config section is present.
#[allow(dead_code)]
async fn create_provider_default(config: &Config) -> Result<Box<dyn ServiceProvider>> {
    // If database config differs from defaults,
    // use postgres_template provider
    #[cfg(feature = "service-postgres-template")]
    if config.database.host != "localhost"
        || config.database.port != 5432
        || config.database.template_database != "template0"
    {
        let provider = PostgresTemplateProvider::new(config)
            .await
            .context("Failed to create PostgreSQL template provider")?;
        return Ok(Box::new(provider));
    }

    #[cfg(not(feature = "service-postgres-template"))]
    if config.database.host != "localhost"
        || config.database.port != 5432
        || config.database.template_database != "template0"
    {
        anyhow::bail!("PostgreSQL template provider not compiled. Rebuild with --features service-postgres-template");
    }

    // Default to local provider — derive name from cwd
    #[cfg(feature = "service-local")]
    {
        let default_name = std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "default".to_string());
        let provider = LocalProvider::new(&default_name, config, None)
            .await
            .context("Failed to create local provider")?;
        Ok(Box::new(provider))
    }

    #[cfg(not(feature = "service-local"))]
    {
        anyhow::bail!("Local provider not compiled. Rebuild with --features service-local");
    }
}

/// Instantiate only services with `auto_workspace: true`.
///
/// These are the services that should be automatically branched when a git
/// workspace is created/switched/deleted.
/// If no services are configured, returns an empty list.
pub async fn create_auto_branch_providers(config: &Config) -> Result<Vec<NamedService>> {
    config.validate_services()?;

    let named_configs = config.resolve_services();

    if named_configs.is_empty() {
        return Ok(Vec::new());
    }

    let auto_configs: Vec<_> = named_configs.iter().filter(|c| c.auto_workspace).collect();

    if auto_configs.is_empty() {
        return Ok(vec![]);
    }

    let mut result = Vec::with_capacity(auto_configs.len());
    for named in auto_configs {
        let provider = create_provider_from_named_config(config, named).await?;
        result.push(NamedService {
            name: named.name.clone(),
            provider,
        });
    }

    Ok(result)
}

/// Result of an orchestrated operation on a single service.
#[derive(Debug)]
pub struct OrchestrationResult {
    pub service_name: String,
    pub success: bool,
    pub message: String,
    pub branch_info: Option<super::WorkspaceInfo>,
}

/// Create a workspace across all auto-workspace services.
///
/// Iterates over all services with `auto_workspace: true` and calls
/// `create_workspace()` on each. Collects results with partial failure
/// tolerance — one service failing doesn't prevent others from succeeding.
pub async fn orchestrate_create(
    config: &Config,
    workspace_name: &str,
    from_workspace: Option<&str>,
) -> Result<Vec<OrchestrationResult>> {
    let providers = create_auto_branch_providers(config).await?;
    let mut results = Vec::with_capacity(providers.len());

    for named in &providers {
        let result = match named
            .provider
            .create_workspace(workspace_name, from_workspace)
            .await
        {
            Ok(info) => OrchestrationResult {
                service_name: named.name.clone(),
                success: true,
                message: format!("Created workspace '{}' on {}", workspace_name, named.name),
                branch_info: Some(info),
            },
            Err(e) => OrchestrationResult {
                service_name: named.name.clone(),
                success: false,
                message: format!("Failed to create workspace on {}: {}", named.name, e),
                branch_info: None,
            },
        };
        results.push(result);
    }

    Ok(results)
}

/// Delete a workspace across all auto-workspace services.
///
/// Iterates over all services with `auto_workspace: true` and calls
/// `delete_workspace()` on each. Partial failures are tolerated.
pub async fn orchestrate_delete(
    config: &Config,
    workspace_name: &str,
) -> Result<Vec<OrchestrationResult>> {
    let providers = create_auto_branch_providers(config).await?;
    let mut results = Vec::with_capacity(providers.len());

    for named in &providers {
        // Skip services that don't have this workspace
        let has_branch = match named.provider.workspace_exists(workspace_name).await {
            Ok(v) => v,
            Err(e) => {
                results.push(OrchestrationResult {
                    service_name: named.name.clone(),
                    success: false,
                    message: format!(
                        "Failed to check workspace existence on {}: {}",
                        named.name, e
                    ),
                    branch_info: None,
                });
                continue;
            }
        };

        if !has_branch {
            results.push(OrchestrationResult {
                service_name: named.name.clone(),
                success: true,
                message: format!(
                    "Workspace '{}' not found on {} (skipped)",
                    workspace_name, named.name
                ),
                branch_info: None,
            });
            continue;
        }

        let result = match named.provider.delete_workspace(workspace_name).await {
            Ok(_) => OrchestrationResult {
                service_name: named.name.clone(),
                success: true,
                message: format!("Deleted workspace '{}' on {}", workspace_name, named.name),
                branch_info: None,
            },
            Err(e) => OrchestrationResult {
                service_name: named.name.clone(),
                success: false,
                message: format!("Failed to delete workspace on {}: {}", named.name, e),
                branch_info: None,
            },
        };
        results.push(result);
    }

    Ok(results)
}

/// Switch to a workspace across all auto-workspace services.
///
/// For each service with `auto_workspace: true`:
/// 1. If the workspace doesn't exist, create it
/// 2. Switch to the workspace
///
/// Partial failures are tolerated.
pub async fn orchestrate_switch(
    config: &Config,
    workspace_name: &str,
    from_workspace: Option<&str>,
) -> Result<Vec<OrchestrationResult>> {
    let providers = create_auto_branch_providers(config).await?;
    let mut results = Vec::with_capacity(providers.len());

    for named in &providers {
        // Check if workspace already exists
        let exists = match named.provider.workspace_exists(workspace_name).await {
            Ok(v) => v,
            Err(e) => {
                results.push(OrchestrationResult {
                    service_name: named.name.clone(),
                    success: false,
                    message: format!(
                        "Failed to check workspace existence on {}: {}",
                        named.name, e
                    ),
                    branch_info: None,
                });
                continue;
            }
        };

        let result = if !exists {
            // Create the workspace first
            match named
                .provider
                .create_workspace(workspace_name, from_workspace)
                .await
            {
                Ok(info) => OrchestrationResult {
                    service_name: named.name.clone(),
                    success: true,
                    message: format!(
                        "Created and switched to workspace '{}' on {}",
                        workspace_name, named.name
                    ),
                    branch_info: Some(info),
                },
                Err(e) => OrchestrationResult {
                    service_name: named.name.clone(),
                    success: false,
                    message: format!("Failed to create workspace on {}: {}", named.name, e),
                    branch_info: None,
                },
            }
        } else {
            // Workspace exists, just switch
            match named.provider.switch_to_branch(workspace_name).await {
                Ok(info) => OrchestrationResult {
                    service_name: named.name.clone(),
                    success: true,
                    message: format!(
                        "Switched to workspace '{}' on {}",
                        workspace_name, named.name
                    ),
                    branch_info: Some(info),
                },
                Err(e) => OrchestrationResult {
                    service_name: named.name.clone(),
                    success: false,
                    message: format!("Failed to switch workspace on {}: {}", named.name, e),
                    branch_info: None,
                },
            }
        };
        results.push(result);
    }

    Ok(results)
}

/// Get connection info from all services for a given workspace.
///
/// Returns a map of service_name -> ConnectionInfo. Used by the hook context
/// builder to populate per-service template variables.
///
/// Queries ALL configured services (not just auto-workspace ones) so hooks can
/// reference any service.  Services that fail to return connection info for
/// the given workspace are silently skipped.
pub async fn get_all_connection_info(
    config: &Config,
    workspace_name: &str,
) -> Result<Vec<(String, super::ConnectionInfo)>> {
    let providers = create_all_providers(config).await?;
    let mut results = Vec::with_capacity(providers.len());

    for named in &providers {
        match named.provider.get_connection_info(workspace_name).await {
            Ok(info) => results.push((named.name.clone(), info)),
            Err(e) => {
                log::debug!(
                    "Could not get connection info for {} on workspace '{}': {}",
                    named.name,
                    workspace_name,
                    e
                );
            }
        }
    }

    Ok(results)
}

fn resolve_env_var(value: &str) -> Result<String> {
    if value.starts_with("${") && value.ends_with('}') {
        let env_var = &value[2..value.len() - 1];
        std::env::var(env_var)
            .with_context(|| format!("Environment variable {} not found", env_var))
    } else {
        Ok(value.to_string())
    }
}

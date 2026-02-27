#[cfg(feature = "backend-dblab")]
use super::postgres::dblab::DBLabBackend;
#[cfg(feature = "backend-local")]
use super::postgres::local::LocalBackend;
#[cfg(feature = "backend-neon")]
use super::postgres::neon::NeonBackend;
#[cfg(feature = "backend-postgres-template")]
use super::postgres::template::PostgresTemplateBackend;
#[cfg(feature = "backend-xata")]
use super::postgres::xata::XataBackend;

#[cfg(feature = "backend-local")]
use super::clickhouse::local::ClickHouseLocalBackend;
#[cfg(feature = "backend-local")]
use super::generic::GenericDockerBackend;
#[cfg(feature = "backend-local")]
use super::mysql::local::MySQLLocalBackend;

use super::plugin::PluginBackend;
use super::ServiceBackend;
use crate::config::{Config, NamedBackendConfig};
use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BackendType {
    #[cfg(feature = "backend-local")]
    Local,
    #[cfg(feature = "backend-postgres-template")]
    PostgresTemplate,
    #[cfg(feature = "backend-neon")]
    Neon,
    #[cfg(feature = "backend-dblab")]
    DBLab,
    #[cfg(feature = "backend-xata")]
    Xata,
}

impl BackendType {
    pub fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            #[cfg(feature = "backend-local")]
            "local" | "docker" => Ok(BackendType::Local),
            #[cfg(not(feature = "backend-local"))]
            "local" | "docker" => anyhow::bail!("Local backend not compiled. Rebuild with --features backend-local"),

            #[cfg(feature = "backend-postgres-template")]
            "postgres_template" | "postgres" | "postgresql" => Ok(BackendType::PostgresTemplate),
            #[cfg(not(feature = "backend-postgres-template"))]
            "postgres_template" | "postgres" | "postgresql" => anyhow::bail!("PostgreSQL template backend not compiled. Rebuild with --features backend-postgres-template"),

            #[cfg(feature = "backend-neon")]
            "neon" => Ok(BackendType::Neon),
            #[cfg(not(feature = "backend-neon"))]
            "neon" => anyhow::bail!("Neon backend not compiled. Rebuild with --features backend-neon"),

            #[cfg(feature = "backend-dblab")]
            "dblab" | "database_lab" => Ok(BackendType::DBLab),
            #[cfg(not(feature = "backend-dblab"))]
            "dblab" | "database_lab" => anyhow::bail!("DBLab backend not compiled. Rebuild with --features backend-dblab"),

            #[cfg(feature = "backend-xata")]
            "xata" | "xata_lite" => Ok(BackendType::Xata),
            #[cfg(not(feature = "backend-xata"))]
            "xata" | "xata_lite" => anyhow::bail!("Xata backend not compiled. Rebuild with --features backend-xata"),

            _ => anyhow::bail!("Unknown backend type: {}. Valid types: local, postgres_template, neon, dblab, xata", s),
        }
    }

    pub fn is_local(s: &str) -> bool {
        matches!(s.to_lowercase().as_str(), "local" | "docker")
    }
}

pub struct NamedBackend {
    pub name: String,
    pub backend: Box<dyn ServiceBackend>,
}

/// Create a backend from a NamedBackendConfig.
///
/// Dispatches based on `service_type` first (postgres, clickhouse, mysql, generic),
/// then on `backend_type` (local, neon, dblab, etc.) for postgres services.
pub async fn create_backend_from_named_config(
    config: &Config,
    named: &NamedBackendConfig,
) -> Result<Box<dyn ServiceBackend>> {
    match named.service_type.as_str() {
        "postgres" | "" => {
            // Dispatch on backend_type for postgres services
            create_postgres_backend(config, named).await
        }

        #[cfg(feature = "backend-local")]
        "clickhouse" => {
            let ch_config = named.clickhouse.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Service '{}' has type 'clickhouse' but no clickhouse config section",
                    named.name
                )
            })?;
            let backend = ClickHouseLocalBackend::new(&named.name, ch_config)
                .context("Failed to create ClickHouse backend")?;
            Ok(Box::new(backend))
        }
        #[cfg(not(feature = "backend-local"))]
        "clickhouse" => {
            anyhow::bail!("ClickHouse backend requires the 'backend-local' feature (Docker support). Rebuild with --features backend-local")
        }

        #[cfg(feature = "backend-local")]
        "mysql" | "mariadb" => {
            let mysql_config = named.mysql.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Service '{}' has type '{}' but no mysql config section",
                    named.name,
                    named.service_type
                )
            })?;
            let backend = MySQLLocalBackend::new(&named.name, mysql_config)
                .context("Failed to create MySQL backend")?;
            Ok(Box::new(backend))
        }
        #[cfg(not(feature = "backend-local"))]
        "mysql" | "mariadb" => {
            anyhow::bail!("MySQL backend requires the 'backend-local' feature (Docker support). Rebuild with --features backend-local")
        }

        #[cfg(feature = "backend-local")]
        "generic" => {
            let generic_config = named.generic.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Service '{}' has type 'generic' but no generic config section",
                    named.name
                )
            })?;
            let backend = GenericDockerBackend::new(&named.name, generic_config)
                .context("Failed to create generic Docker backend")?;
            Ok(Box::new(backend))
        }
        #[cfg(not(feature = "backend-local"))]
        "generic" => {
            anyhow::bail!("Generic Docker backend requires the 'backend-local' feature. Rebuild with --features backend-local")
        }

        "plugin" => {
            let plugin_config = named.plugin.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Service '{}' has type 'plugin' but no plugin config section",
                    named.name
                )
            })?;
            let backend = PluginBackend::new(&named.name, plugin_config)
                .context("Failed to create plugin backend")?;
            Ok(Box::new(backend))
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

/// Create a postgres-specific backend, dispatching on `backend_type`.
async fn create_postgres_backend(
    config: &Config,
    named: &NamedBackendConfig,
) -> Result<Box<dyn ServiceBackend>> {
    let backend_type = BackendType::from_str(&named.backend_type)?;

    match backend_type {
        #[cfg(feature = "backend-local")]
        BackendType::Local => {
            let local_config = named.local.as_ref();
            let backend = LocalBackend::new(&named.name, config, local_config)
                .await
                .context("Failed to create local backend")?;
            Ok(Box::new(backend))
        }
        #[cfg(feature = "backend-postgres-template")]
        BackendType::PostgresTemplate => {
            let backend = PostgresTemplateBackend::new(config)
                .await
                .context("Failed to create PostgreSQL template backend")?;
            Ok(Box::new(backend))
        }
        #[cfg(feature = "backend-neon")]
        BackendType::Neon => {
            if let Some(ref neon_config) = named.neon {
                let backend = NeonBackend::new(
                    resolve_env_var(&neon_config.api_key)?,
                    resolve_env_var(&neon_config.project_id)?,
                    Some(neon_config.base_url.clone()),
                )?;
                Ok(Box::new(backend))
            } else {
                anyhow::bail!("Neon backend selected but no neon configuration provided");
            }
        }
        #[cfg(feature = "backend-dblab")]
        BackendType::DBLab => {
            if let Some(ref dblab_config) = named.dblab {
                let backend = DBLabBackend::new(
                    resolve_env_var(&dblab_config.api_url)?,
                    resolve_env_var(&dblab_config.auth_token)?,
                )?;
                Ok(Box::new(backend))
            } else {
                anyhow::bail!("DBLab backend selected but no dblab configuration provided");
            }
        }
        #[cfg(feature = "backend-xata")]
        BackendType::Xata => {
            if let Some(ref xata_config) = named.xata {
                let backend = XataBackend::new(
                    resolve_env_var(&xata_config.api_key)?,
                    resolve_env_var(&xata_config.organization_id)?,
                    resolve_env_var(&xata_config.project_id)?,
                    Some(xata_config.base_url.clone()),
                )?;
                Ok(Box::new(backend))
            } else {
                anyhow::bail!("Xata backend selected but no xata configuration provided");
            }
        }
    }
}

/// Resolve a single backend by name (or the default).
pub async fn resolve_backend(config: &Config, backend_name: Option<&str>) -> Result<NamedBackend> {
    config.validate_backends()?;

    let backends = config.resolve_backends();

    // If backends list is populated, use it
    if !backends.is_empty() {
        let named = if let Some(name) = backend_name {
            backends
                .iter()
                .find(|b| b.name == name)
                .ok_or_else(|| anyhow::anyhow!("Backend '{}' not found in configuration", name))?
        } else {
            backends
                .iter()
                .find(|b| b.default)
                .or(backends.first())
                .ok_or_else(|| anyhow::anyhow!("No backends configured"))?
        };

        let backend = create_backend_from_named_config(config, named).await?;
        return Ok(NamedBackend {
            name: named.name.clone(),
            backend,
        });
    }

    // No backends or backend config — fall back to auto-detection
    if backend_name.is_some() {
        anyhow::bail!("--database specified but no backends configured");
    }

    let backend = create_backend_default(config).await?;
    Ok(NamedBackend {
        name: "default".to_string(),
        backend,
    })
}

/// Instantiate all configured backends.
pub async fn create_all_backends(config: &Config) -> Result<Vec<NamedBackend>> {
    config.validate_backends()?;

    let named_configs = config.resolve_backends();

    if named_configs.is_empty() {
        // Fall back to default auto-detection
        let backend = create_backend_default(config).await?;
        return Ok(vec![NamedBackend {
            name: "default".to_string(),
            backend,
        }]);
    }

    let mut result = Vec::with_capacity(named_configs.len());
    for named in &named_configs {
        let backend = create_backend_from_named_config(config, named).await?;
        result.push(NamedBackend {
            name: named.name.clone(),
            backend,
        });
    }

    Ok(result)
}

/// Auto-detect backend when no config section is present.
async fn create_backend_default(config: &Config) -> Result<Box<dyn ServiceBackend>> {
    // If database config differs from defaults,
    // use postgres_template backend
    #[cfg(feature = "backend-postgres-template")]
    if config.database.host != "localhost"
        || config.database.port != 5432
        || config.database.template_database != "template0"
    {
        let backend = PostgresTemplateBackend::new(config)
            .await
            .context("Failed to create PostgreSQL template backend")?;
        return Ok(Box::new(backend));
    }

    #[cfg(not(feature = "backend-postgres-template"))]
    if config.database.host != "localhost"
        || config.database.port != 5432
        || config.database.template_database != "template0"
    {
        anyhow::bail!("PostgreSQL template backend not compiled. Rebuild with --features backend-postgres-template");
    }

    // Default to local backend — derive name from cwd
    #[cfg(feature = "backend-local")]
    {
        let default_name = std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "default".to_string());
        let backend = LocalBackend::new(&default_name, config, None)
            .await
            .context("Failed to create local backend")?;
        Ok(Box::new(backend))
    }

    #[cfg(not(feature = "backend-local"))]
    {
        anyhow::bail!("Local backend not compiled. Rebuild with --features backend-local");
    }
}

/// Instantiate only backends with `auto_branch: true`.
///
/// These are the backends that should be automatically branched when a git
/// branch is created/switched/deleted. Falls back to default auto-detection
/// if no backends are configured.
pub async fn create_auto_branch_backends(config: &Config) -> Result<Vec<NamedBackend>> {
    config.validate_backends()?;

    let named_configs = config.resolve_backends();

    if named_configs.is_empty() {
        // Fall back to default auto-detection (single backend, implicitly auto_branch)
        let backend = create_backend_default(config).await?;
        return Ok(vec![NamedBackend {
            name: "default".to_string(),
            backend,
        }]);
    }

    let auto_configs: Vec<_> = named_configs.iter().filter(|c| c.auto_branch).collect();

    if auto_configs.is_empty() {
        return Ok(vec![]);
    }

    let mut result = Vec::with_capacity(auto_configs.len());
    for named in auto_configs {
        let backend = create_backend_from_named_config(config, named).await?;
        result.push(NamedBackend {
            name: named.name.clone(),
            backend,
        });
    }

    Ok(result)
}

/// Result of an orchestrated operation on a single backend.
#[derive(Debug)]
pub struct OrchestrationResult {
    pub service_name: String,
    pub success: bool,
    pub message: String,
    pub branch_info: Option<super::BranchInfo>,
}

/// Create a branch across all auto-branch backends.
///
/// Iterates over all backends with `auto_branch: true` and calls
/// `create_branch()` on each. Collects results with partial failure
/// tolerance — one backend failing doesn't prevent others from succeeding.
pub async fn orchestrate_create(
    config: &Config,
    branch_name: &str,
    from_branch: Option<&str>,
) -> Result<Vec<OrchestrationResult>> {
    let backends = create_auto_branch_backends(config).await?;
    let mut results = Vec::with_capacity(backends.len());

    for named in &backends {
        let result = match named.backend.create_branch(branch_name, from_branch).await {
            Ok(info) => OrchestrationResult {
                service_name: named.name.clone(),
                success: true,
                message: format!("Created branch '{}' on {}", branch_name, named.name),
                branch_info: Some(info),
            },
            Err(e) => OrchestrationResult {
                service_name: named.name.clone(),
                success: false,
                message: format!("Failed to create branch on {}: {}", named.name, e),
                branch_info: None,
            },
        };
        results.push(result);
    }

    Ok(results)
}

/// Delete a branch across all auto-branch backends.
///
/// Iterates over all backends with `auto_branch: true` and calls
/// `delete_branch()` on each. Partial failures are tolerated.
pub async fn orchestrate_delete(
    config: &Config,
    branch_name: &str,
) -> Result<Vec<OrchestrationResult>> {
    let backends = create_auto_branch_backends(config).await?;
    let mut results = Vec::with_capacity(backends.len());

    for named in &backends {
        // Skip backends that don't have this branch
        let has_branch = match named.backend.branch_exists(branch_name).await {
            Ok(v) => v,
            Err(e) => {
                results.push(OrchestrationResult {
                    service_name: named.name.clone(),
                    success: false,
                    message: format!("Failed to check branch existence on {}: {}", named.name, e),
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
                    "Branch '{}' not found on {} (skipped)",
                    branch_name, named.name
                ),
                branch_info: None,
            });
            continue;
        }

        let result = match named.backend.delete_branch(branch_name).await {
            Ok(_) => OrchestrationResult {
                service_name: named.name.clone(),
                success: true,
                message: format!("Deleted branch '{}' on {}", branch_name, named.name),
                branch_info: None,
            },
            Err(e) => OrchestrationResult {
                service_name: named.name.clone(),
                success: false,
                message: format!("Failed to delete branch on {}: {}", named.name, e),
                branch_info: None,
            },
        };
        results.push(result);
    }

    Ok(results)
}

/// Switch to a branch across all auto-branch backends.
///
/// For each backend with `auto_branch: true`:
/// 1. If the branch doesn't exist, create it
/// 2. Switch to the branch
///
/// Partial failures are tolerated.
pub async fn orchestrate_switch(
    config: &Config,
    branch_name: &str,
    from_branch: Option<&str>,
) -> Result<Vec<OrchestrationResult>> {
    let backends = create_auto_branch_backends(config).await?;
    let mut results = Vec::with_capacity(backends.len());

    for named in &backends {
        // Check if branch already exists
        let exists = match named.backend.branch_exists(branch_name).await {
            Ok(v) => v,
            Err(e) => {
                results.push(OrchestrationResult {
                    service_name: named.name.clone(),
                    success: false,
                    message: format!("Failed to check branch existence on {}: {}", named.name, e),
                    branch_info: None,
                });
                continue;
            }
        };

        let result = if !exists {
            // Create the branch first
            match named.backend.create_branch(branch_name, from_branch).await {
                Ok(info) => OrchestrationResult {
                    service_name: named.name.clone(),
                    success: true,
                    message: format!(
                        "Created and switched to branch '{}' on {}",
                        branch_name, named.name
                    ),
                    branch_info: Some(info),
                },
                Err(e) => OrchestrationResult {
                    service_name: named.name.clone(),
                    success: false,
                    message: format!("Failed to create branch on {}: {}", named.name, e),
                    branch_info: None,
                },
            }
        } else {
            // Branch exists, just switch
            match named.backend.switch_to_branch(branch_name).await {
                Ok(info) => OrchestrationResult {
                    service_name: named.name.clone(),
                    success: true,
                    message: format!("Switched to branch '{}' on {}", branch_name, named.name),
                    branch_info: Some(info),
                },
                Err(e) => OrchestrationResult {
                    service_name: named.name.clone(),
                    success: false,
                    message: format!("Failed to switch branch on {}: {}", named.name, e),
                    branch_info: None,
                },
            }
        };
        results.push(result);
    }

    Ok(results)
}

/// Get connection info from all auto-branch backends for a given branch.
///
/// Returns a map of service_name -> ConnectionInfo. Used by the hook context
/// builder to populate per-service template variables.
///
/// Queries ALL configured backends (not just auto-branch ones) so hooks can
/// reference any service.  Backends that fail to return connection info for
/// the given branch are silently skipped.
pub async fn get_all_connection_info(
    config: &Config,
    branch_name: &str,
) -> Result<Vec<(String, super::ConnectionInfo)>> {
    let backends = create_all_backends(config).await?;
    let mut results = Vec::with_capacity(backends.len());

    for named in &backends {
        match named.backend.get_connection_info(branch_name).await {
            Ok(info) => results.push((named.name.clone(), info)),
            Err(e) => {
                log::debug!(
                    "Could not get connection info for {} on branch '{}': {}",
                    named.name,
                    branch_name,
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

//! Shared (global-container, logical-isolation) service providers.
//!
//! Instead of one Docker container per workspace with Copy-on-Write storage,
//! these keep a single global container per engine and carve out a logical
//! boundary per workspace inside it — a `CREATE DATABASE` for postgres. This
//! is the "global container + on-the-fly provisioning" model.

pub mod clickhouse;
pub mod container;
pub mod naming;
pub mod rustfs;

pub use clickhouse::SharedClickHouseProvider;
pub use rustfs::RustFsProvider;

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bollard::Docker;

use super::{ConnectionInfo, DoctorCheck, DoctorReport, ServiceProvider, WorkspaceInfo};
use crate::config::SharedServiceConfig;

const DEFAULT_IMAGE: &str = "postgres:17";
const DEFAULT_PORT: u16 = 5432;
const DEFAULT_USER: &str = "postgres";
const DEFAULT_PASSWORD: &str = "postgres";
const DEFAULT_CONTAINER: &str = "devflow-shared-postgres";
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// A postgres provider that keeps one global container and gives each
/// workspace its own logical database inside it.
pub struct SharedPostgresProvider {
    project_name: String,
    /// Retained for future per-service disambiguation; the global container
    /// is currently shared across all `shared` postgres services.
    #[allow(dead_code)]
    service_name: String,
    image: String,
    container_name: String,
    host_port: u16,
    user: String,
    password: String,
    template_branching: bool,
}

impl SharedPostgresProvider {
    pub fn new(
        project_name: &str,
        service_name: &str,
        config: Option<&SharedServiceConfig>,
    ) -> Result<Self> {
        let c = config.cloned().unwrap_or_default();
        Ok(Self {
            project_name: project_name.to_string(),
            service_name: service_name.to_string(),
            image: c.image.unwrap_or_else(|| DEFAULT_IMAGE.to_string()),
            container_name: c
                .container_name
                .unwrap_or_else(|| DEFAULT_CONTAINER.to_string()),
            host_port: c.port.unwrap_or(DEFAULT_PORT),
            user: c.user.unwrap_or_else(|| DEFAULT_USER.to_string()),
            password: c.password.unwrap_or_else(|| DEFAULT_PASSWORD.to_string()),
            template_branching: c.template_branching.unwrap_or(true),
        })
    }

    fn db_name(&self, workspace: &str) -> String {
        naming::logical_db_name(&self.project_name, workspace)
    }

    fn container_spec(&self) -> container::GlobalContainerSpec {
        container::GlobalContainerSpec {
            container_name: self.container_name.clone(),
            image: self.image.clone(),
            host_port: self.host_port,
            container_port: DEFAULT_PORT,
            extra_port: None,
            cmd: vec![],
            env: vec![
                format!("POSTGRES_USER={}", self.user),
                format!("POSTGRES_PASSWORD={}", self.password),
                // Maintenance DB; per-workspace DBs are created separately.
                "POSTGRES_DB=postgres".to_string(),
                "PGDATA=/var/lib/postgresql/data".to_string(),
            ],
            binds: vec![format!(
                "{}-data:/var/lib/postgresql/data",
                self.container_name
            )],
            labels: vec![
                ("devflow.service-type".to_string(), "postgres".to_string()),
                ("devflow.shared".to_string(), "true".to_string()),
            ],
        }
    }

    /// Ensure the global container is running and accepting connections.
    async fn ensure_ready(&self, docker: &Docker) -> Result<()> {
        container::ensure_running_container(docker, &self.container_spec()).await?;
        container::wait_ready_pg(docker, &self.container_name, &self.user, READY_TIMEOUT).await
    }

    /// Run a SQL statement against the maintenance database and return stdout
    /// (tuples-only, unaligned). Errors include the postgres stderr.
    async fn psql(&self, docker: &Docker, sql: &str) -> Result<String> {
        let out = container::exec_capture(
            docker,
            &self.container_name,
            &["psql", "-U", &self.user, "-d", "postgres", "-tAqc", sql],
            Some(vec![format!("PGPASSWORD={}", self.password)]),
        )
        .await?;
        if !out.ok() {
            anyhow::bail!(
                "psql failed (exit {}): {}",
                out.exit_code,
                out.stderr.trim()
            );
        }
        Ok(out.stdout)
    }

    async fn database_exists(&self, docker: &Docker, db: &str) -> Result<bool> {
        let out = self.psql(docker, &naming::database_exists_sql(db)).await?;
        Ok(out.trim() == "1")
    }

    fn connection_info_for(&self, db: &str) -> ConnectionInfo {
        let url = format!(
            "postgres://{}:{}@127.0.0.1:{}/{}",
            self.user, self.password, self.host_port, db
        );
        ConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: self.host_port,
            database: db.to_string(),
            user: self.user.clone(),
            password: Some(self.password.clone()),
            connection_string: Some(url),
        }
    }
}

#[async_trait]
impl ServiceProvider for SharedPostgresProvider {
    async fn create_workspace(
        &self,
        workspace_name: &str,
        from_workspace: Option<&str>,
    ) -> Result<WorkspaceInfo> {
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;

        let db = self.db_name(workspace_name);

        // Idempotent: a workspace that already maps to an existing database
        // is a no-op (it's already provisioned).
        if self.database_exists(&docker, &db).await? {
            return Ok(WorkspaceInfo {
                name: workspace_name.to_string(),
                created_at: None,
                parent_workspace: from_workspace.map(String::from),
                database_name: db,
                state: Some("running".to_string()),
            });
        }

        // Branch-from-parent via TEMPLATE when enabled and the parent exists.
        let template = if self.template_branching {
            match from_workspace {
                Some(parent) => {
                    let parent_db = self.db_name(parent);
                    if self.database_exists(&docker, &parent_db).await? {
                        // TEMPLATE requires no active sessions on the source.
                        let _ = self
                            .psql(&docker, &naming::terminate_connections_sql(&parent_db))
                            .await;
                        Some(parent_db)
                    } else {
                        None
                    }
                }
                None => None,
            }
        } else {
            None
        };

        self.psql(
            &docker,
            &naming::create_database_sql(&db, template.as_deref()),
        )
        .await
        .with_context(|| format!("failed to create logical database '{db}'"))?;

        Ok(WorkspaceInfo {
            name: workspace_name.to_string(),
            created_at: None,
            parent_workspace: from_workspace.map(String::from),
            database_name: db,
            state: Some("running".to_string()),
        })
    }

    async fn delete_workspace(&self, workspace_name: &str) -> Result<()> {
        let docker = container::connect()?;
        // Only touch the database if the container is up; if the engine isn't
        // running there is nothing to drop.
        if container::ensure_running_container(&docker, &self.container_spec())
            .await
            .is_err()
        {
            return Ok(());
        }
        let db = self.db_name(workspace_name);
        // Terminate sessions so DROP doesn't fail on "database is being accessed".
        let _ = self
            .psql(&docker, &naming::terminate_connections_sql(&db))
            .await;
        self.psql(
            &docker,
            &format!("DROP DATABASE IF EXISTS {}", naming::quote_ident(&db)),
        )
        .await
        .with_context(|| format!("failed to drop logical database '{db}'"))?;
        Ok(())
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;

        let prefix = naming::project_db_prefix(&self.project_name);
        let out = self
            .psql(&docker, &naming::list_databases_sql(&prefix))
            .await?;

        let workspaces = out
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|db| {
                let ws = db.strip_prefix(&prefix).unwrap_or(db).to_string();
                WorkspaceInfo {
                    name: ws,
                    created_at: None,
                    parent_workspace: None,
                    database_name: db.to_string(),
                    state: Some("running".to_string()),
                }
            })
            .collect();
        Ok(workspaces)
    }

    async fn workspace_exists(&self, workspace_name: &str) -> Result<bool> {
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;
        self.database_exists(&docker, &self.db_name(workspace_name))
            .await
    }

    async fn switch_to_branch(&self, workspace_name: &str) -> Result<WorkspaceInfo> {
        // There is no per-workspace container to start — ensure the database
        // exists and return its connection info.
        self.create_workspace(workspace_name, None).await
    }

    async fn get_connection_info(&self, workspace_name: &str) -> Result<ConnectionInfo> {
        Ok(self.connection_info_for(&self.db_name(workspace_name)))
    }

    fn supports_destroy(&self) -> bool {
        true
    }

    async fn destroy_preview(&self) -> Result<Option<(String, Vec<String>)>> {
        let workspaces = self.list_workspaces().await.unwrap_or_default();
        let dbs = workspaces.into_iter().map(|w| w.database_name).collect();
        Ok(Some((self.container_name.clone(), dbs)))
    }

    async fn destroy_project(&self) -> Result<Vec<String>> {
        // Drop only this project's databases — never remove the global
        // container, which serves other projects.
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;

        let workspaces = self.list_workspaces().await?;
        let mut dropped = Vec::new();
        for w in workspaces {
            let _ = self
                .psql(
                    &docker,
                    &naming::terminate_connections_sql(&w.database_name),
                )
                .await;
            match self
                .psql(
                    &docker,
                    &format!(
                        "DROP DATABASE IF EXISTS {}",
                        naming::quote_ident(&w.database_name)
                    ),
                )
                .await
            {
                Ok(_) => dropped.push(w.database_name),
                Err(e) => log::warn!("Failed to drop '{}': {}", w.database_name, e),
            }
        }
        Ok(dropped)
    }

    async fn doctor(&self) -> Result<DoctorReport> {
        let mut checks = Vec::new();

        match container::connect() {
            Ok(docker) => {
                checks.push(DoctorCheck {
                    name: "docker".to_string(),
                    available: true,
                    detail: "Connected to Docker daemon".to_string(),
                });
                let running = matches!(
                    crate::services::local_docker::inspect_container_status(
                        &docker,
                        &self.container_name
                    )
                    .await,
                    Ok(crate::services::local_docker::ContainerStatus::Running)
                );
                checks.push(DoctorCheck {
                    name: "shared-container".to_string(),
                    available: running,
                    detail: if running {
                        format!(
                            "'{}' is running on port {}",
                            self.container_name, self.host_port
                        )
                    } else {
                        format!(
                            "'{}' is not running (it will be started on first use)",
                            self.container_name
                        )
                    },
                });
            }
            Err(e) => checks.push(DoctorCheck {
                name: "docker".to_string(),
                available: false,
                detail: format!("Docker unavailable: {e}"),
            }),
        }

        Ok(DoctorReport { checks })
    }

    async fn test_connection(&self) -> Result<()> {
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;
        Ok(())
    }

    fn provider_name(&self) -> &'static str {
        "shared-postgres"
    }

    fn project_info(&self) -> Option<super::ProjectInfo> {
        Some(super::ProjectInfo {
            name: self.project_name.clone(),
            storage_driver: Some("shared".to_string()),
            image: Some(self.image.clone()),
        })
    }
}

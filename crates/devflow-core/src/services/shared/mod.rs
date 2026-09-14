//! Shared (global-container, logical-isolation) service providers.
//!
//! Instead of one Docker container per workspace with Copy-on-Write storage,
//! these keep a single global container per engine and carve out a logical
//! boundary per workspace inside it — a `CREATE DATABASE` for postgres. This
//! is the "global container + on-the-fly provisioning" model.

pub mod clickhouse;
pub mod container;
pub mod naming;
pub mod redis;
pub mod rustfs;

pub use clickhouse::SharedClickHouseProvider;
pub use redis::SharedRedisProvider;
pub use rustfs::RustFsProvider;

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bollard::Docker;

use super::{ConnectionInfo, DoctorReport, ServiceProvider, WorkspaceInfo};
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
    resources: crate::services::ownership::ResourceRegistry,
    image: String,
    container_name: String,
    host_port: u16,
    user: String,
    password: String,
    template_branching: bool,
}

impl SharedPostgresProvider {
    pub fn new(project_name: &str, config: Option<&SharedServiceConfig>) -> Result<Self> {
        Self::new_scoped(project_name, project_name, "db", config)
    }

    pub fn new_scoped(
        project_name: &str,
        project_identity: &str,
        service_name: &str,
        config: Option<&SharedServiceConfig>,
    ) -> Result<Self> {
        let c = config.cloned().unwrap_or_default();
        Ok(Self {
            project_name: project_name.to_string(),
            resources: crate::services::ownership::ResourceRegistry::new(
                project_identity,
                service_name,
                c.container_name.as_deref().unwrap_or(DEFAULT_CONTAINER),
                c.legacy_resources.clone(),
            )?,
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
        self.resources.resource_name(workspace)
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
        let _lease = self.resources.lease(workspace_name).await?;
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;
        let db = self.db_name(workspace_name);
        let exists = self.database_exists(&docker, &db).await?;
        self.resources.check_existing(workspace_name, exists)?;
        let legacy = naming::logical_db_name(&self.project_name, workspace_name);
        self.resources.check_legacy(
            workspace_name,
            &legacy,
            self.database_exists(&docker, &legacy).await?,
        )?;
        self.resources.claim(workspace_name, from_workspace)?;
        if !exists {
            let _parent_lease = match from_workspace.filter(|p| *p != workspace_name) {
                Some(parent) if self.template_branching => {
                    Some(self.resources.lease(parent).await?)
                }
                _ => None,
            };
            let template = if self.template_branching {
                match from_workspace.filter(|p| *p != workspace_name) {
                    Some(parent) => {
                        let record = self.resources.record(parent)?.context("parent database is not owned/provisioned; create or explicitly adopt it first")?;
                        anyhow::ensure!(
                            record.ready && self.database_exists(&docker, &record.resource).await?,
                            "parent database is not ready"
                        );
                        self.psql(
                            &docker,
                            &naming::terminate_connections_sql(&record.resource),
                        )
                        .await?;
                        Some(record.resource)
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
        }
        self.resources.complete(workspace_name)?;
        Ok(self
            .resources
            .record(workspace_name)?
            .context("missing resource ownership")?
            .into_workspace_info("running"))
    }

    async fn delete_workspace(&self, workspace_name: &str) -> Result<()> {
        let _lease = self.resources.lease(workspace_name).await?;
        if self.resources.was_deleted(workspace_name)? {
            return Ok(());
        }
        let Some(record) = self.resources.record(workspace_name)? else {
            anyhow::bail!("workspace '{workspace_name}' has no ownership record; refusing deletion (adopt legacy resources explicitly)");
        };
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;
        self.psql(
            &docker,
            &naming::terminate_connections_sql(&record.resource),
        )
        .await?;
        self.psql(
            &docker,
            &format!(
                "DROP DATABASE IF EXISTS {}",
                naming::quote_ident(&record.resource)
            ),
        )
        .await?;
        self.resources.release(workspace_name)
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let docker = container::connect()?;
        let status = container::observe_container(&docker, &self.container_spec()).await?;
        let mut result = Vec::new();
        for record in self.resources.list()? {
            let state = match status {
                crate::services::local_docker::ContainerStatus::NotFound => "missing",
                crate::services::local_docker::ContainerStatus::Running => {
                    if self.database_exists(&docker, &record.resource).await? {
                        "running"
                    } else {
                        "missing"
                    }
                }
                _ => "stopped",
            };
            result.push(record.into_workspace_info(state));
        }
        Ok(result)
    }

    async fn workspace_exists(&self, workspace_name: &str) -> Result<bool> {
        let docker = container::connect()?;
        let status = container::observe_container(&docker, &self.container_spec()).await?;
        let Some(record) = self.resources.record(workspace_name)? else {
            return Ok(false);
        };
        match status {
            crate::services::local_docker::ContainerStatus::NotFound => Ok(false),
            crate::services::local_docker::ContainerStatus::Running => {
                self.database_exists(&docker, &record.resource).await
            }
            _ => Ok(record.ready),
        }
    }

    async fn switch_to_branch(&self, workspace_name: &str) -> Result<WorkspaceInfo> {
        // There is no per-workspace container to start — ensure the database
        // exists and return its connection info.
        self.create_workspace(workspace_name, None).await
    }

    async fn get_connection_info(&self, workspace_name: &str) -> Result<ConnectionInfo> {
        let docker = container::connect()?;
        container::observe_container(&docker, &self.container_spec()).await?;
        Ok(self.connection_info_for(&self.db_name(workspace_name)))
    }

    fn supports_destroy(&self) -> bool {
        true
    }

    async fn destroy_preview(&self) -> Result<Option<(String, Vec<String>)>> {
        let workspaces = self.list_workspaces().await?;
        let dbs = workspaces.into_iter().map(|w| w.database_name).collect();
        Ok(Some((self.container_name.clone(), dbs)))
    }

    async fn destroy_project(&self) -> Result<Vec<String>> {
        let mut dropped = Vec::new();
        for record in self.resources.list()? {
            self.delete_workspace(&record.workspace).await?;
            dropped.push(record.resource);
        }
        Ok(dropped)
    }

    async fn doctor(&self) -> Result<DoctorReport> {
        Ok(container::doctor(&self.container_spec()).await)
    }

    async fn test_connection(&self) -> Result<()> {
        let docker = container::connect()?;
        let status = container::observe_container(&docker, &self.container_spec()).await?;
        anyhow::ensure!(
            matches!(
                status,
                crate::services::local_docker::ContainerStatus::Running
            ),
            "shared engine is stopped or missing; run devflow service up"
        );
        Ok(())
    }
    async fn ensure_engine_ready(&self) -> Result<()> {
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;
        Ok(())
    }

    fn data_strategy(&self) -> &'static str {
        if self.template_branching {
            "template_copy"
        } else {
            "empty"
        }
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

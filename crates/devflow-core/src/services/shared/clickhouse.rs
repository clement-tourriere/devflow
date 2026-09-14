//! Shared ClickHouse provider — one global container, a database per workspace
//! created via `CREATE DATABASE`. This logical-isolation path sidesteps the
//! per-workspace CoW ClickHouse backend entirely.

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bollard::Docker;

use super::container::{self, GlobalContainerSpec};
use super::naming;
use crate::config::SharedServiceConfig;
use crate::services::{ConnectionInfo, DoctorReport, ProjectInfo, ServiceProvider, WorkspaceInfo};

const DEFAULT_IMAGE: &str = "clickhouse/clickhouse-server:latest";
const DEFAULT_HTTP_PORT: u16 = 8123;
const DEFAULT_USER: &str = "default";
const DEFAULT_CONTAINER: &str = "devflow-shared-clickhouse";
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// ClickHouse provider that keeps one global container and gives each workspace
/// its own database inside it.
pub struct SharedClickHouseProvider {
    project_name: String,
    resources: crate::services::ownership::ResourceRegistry,
    image: String,
    container_name: String,
    http_port: u16,
    user: String,
    password: String,
}

impl SharedClickHouseProvider {
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
            http_port: c.port.unwrap_or(DEFAULT_HTTP_PORT),
            user: c.user.unwrap_or_else(|| DEFAULT_USER.to_string()),
            password: c.password.unwrap_or_default(),
        })
    }

    fn db_name(&self, workspace: &str) -> String {
        self.resources.resource_name(workspace)
    }

    fn container_spec(&self) -> GlobalContainerSpec {
        let mut env = vec![
            format!("CLICKHOUSE_USER={}", self.user),
            "CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1".to_string(),
        ];
        if !self.password.is_empty() {
            env.push(format!("CLICKHOUSE_PASSWORD={}", self.password));
        }
        GlobalContainerSpec {
            container_name: self.container_name.clone(),
            image: self.image.clone(),
            // Expose only the HTTP interface (8123); the native port (9000)
            // would collide with a shared RustFS S3 endpoint. clickhouse-client
            // still uses the native protocol locally inside the container.
            host_port: self.http_port,
            container_port: DEFAULT_HTTP_PORT,
            extra_port: None,
            cmd: vec![],
            env,
            binds: vec![format!("{}-data:/var/lib/clickhouse", self.container_name)],
            labels: vec![
                ("devflow.service-type".to_string(), "clickhouse".to_string()),
                ("devflow.shared".to_string(), "true".to_string()),
            ],
        }
    }

    /// Run a query via `clickhouse-client` and return stdout. Errors include
    /// the client's stderr.
    async fn query(&self, docker: &Docker, sql: &str) -> Result<String> {
        let mut cmd: Vec<String> = vec![
            "clickhouse-client".to_string(),
            "--user".to_string(),
            self.user.clone(),
        ];
        if !self.password.is_empty() {
            cmd.push("--password".to_string());
            cmd.push(self.password.clone());
        }
        cmd.push("--query".to_string());
        cmd.push(sql.to_string());

        let refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
        let out = container::exec_capture(docker, &self.container_name, &refs, None).await?;
        if !out.ok() {
            anyhow::bail!(
                "clickhouse-client failed (exit {}): {}",
                out.exit_code,
                out.stderr.trim()
            );
        }
        Ok(out.stdout)
    }

    async fn ensure_ready(&self, docker: &Docker) -> Result<()> {
        container::ensure_running_container(docker, &self.container_spec()).await?;
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            if self.query(docker, "SELECT 1").await.is_ok() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for ClickHouse readiness");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn database_exists(&self, docker: &Docker, name: &str) -> Result<bool> {
        let sql = format!(
            "SELECT count() FROM system.databases WHERE name = {}",
            naming::quote_literal(name)
        );
        Ok(self.query(docker, &sql).await?.trim() == "1")
    }

    /// Backtick-quote a ClickHouse identifier (names are already sanitized).
    fn quote(name: &str) -> String {
        format!("`{}`", name.replace('`', "\\`"))
    }
}

#[async_trait]
impl ServiceProvider for SharedClickHouseProvider {
    async fn create_workspace(
        &self,
        workspace_name: &str,
        _from_workspace: Option<&str>,
    ) -> Result<WorkspaceInfo> {
        let _lease = self.resources.lease(workspace_name).await?;
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;
        let db = self.db_name(workspace_name);
        self.resources
            .check_existing(workspace_name, self.database_exists(&docker, &db).await?)?;
        let legacy = naming::logical_db_name(&self.project_name, workspace_name);
        self.resources.check_legacy(
            workspace_name,
            &legacy,
            self.database_exists(&docker, &legacy).await?,
        )?;
        self.resources.claim(workspace_name, None)?;
        self.query(
            &docker,
            &format!("CREATE DATABASE IF NOT EXISTS {}", Self::quote(&db)),
        )
        .await?;
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
        let record = self
            .resources
            .record(workspace_name)?
            .context("no ownership record; adopt legacy resources explicitly before deletion")?;
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;
        self.query(
            &docker,
            &format!("DROP DATABASE IF EXISTS {}", Self::quote(&record.resource)),
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
        self.create_workspace(workspace_name, None).await
    }

    async fn get_connection_info(&self, workspace_name: &str) -> Result<ConnectionInfo> {
        let docker = container::connect()?;
        container::observe_container(&docker, &self.container_spec()).await?;
        let db = self.db_name(workspace_name);
        let pw = if self.password.is_empty() {
            String::new()
        } else {
            format!(":{}", self.password)
        };
        Ok(ConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: self.http_port,
            database: db.clone(),
            user: self.user.clone(),
            password: if self.password.is_empty() {
                None
            } else {
                Some(self.password.clone())
            },
            connection_string: Some(format!(
                "http://{}{}@127.0.0.1:{}/?database={}",
                self.user, pw, self.http_port, db
            )),
        })
    }

    fn supports_destroy(&self) -> bool {
        true
    }

    async fn destroy_preview(&self) -> Result<Option<(String, Vec<String>)>> {
        let workspaces = self.list_workspaces().await?;
        Ok(Some((
            self.container_name.clone(),
            workspaces.into_iter().map(|w| w.database_name).collect(),
        )))
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
        self.ensure_ready(&docker).await
    }

    fn provider_name(&self) -> &'static str {
        "shared-clickhouse"
    }

    fn project_info(&self) -> Option<ProjectInfo> {
        Some(ProjectInfo {
            name: self.project_name.clone(),
            storage_driver: Some("shared".to_string()),
            image: Some(self.image.clone()),
        })
    }
}

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
use crate::services::{
    ConnectionInfo, DoctorCheck, DoctorReport, ProjectInfo, ServiceProvider, WorkspaceInfo,
};

const DEFAULT_IMAGE: &str = "clickhouse/clickhouse-server:latest";
const DEFAULT_HTTP_PORT: u16 = 8123;
const DEFAULT_USER: &str = "default";
const DEFAULT_CONTAINER: &str = "devflow-shared-clickhouse";
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// ClickHouse provider that keeps one global container and gives each workspace
/// its own database inside it.
pub struct SharedClickHouseProvider {
    project_name: String,
    image: String,
    container_name: String,
    http_port: u16,
    user: String,
    password: String,
}

impl SharedClickHouseProvider {
    pub fn new(project_name: &str, config: Option<&SharedServiceConfig>) -> Result<Self> {
        let c = config.cloned().unwrap_or_default();
        Ok(Self {
            project_name: project_name.to_string(),
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
        naming::logical_db_name(&self.project_name, workspace)
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
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;
        let db = self.db_name(workspace_name);
        // ClickHouse has no CREATE DATABASE ... TEMPLATE, so branch-from-parent
        // is not supported here; the database is simply created if missing.
        self.query(
            &docker,
            &format!("CREATE DATABASE IF NOT EXISTS {}", Self::quote(&db)),
        )
        .await
        .with_context(|| format!("failed to create ClickHouse database '{db}'"))?;

        Ok(WorkspaceInfo {
            name: workspace_name.to_string(),
            created_at: None,
            parent_workspace: None,
            database_name: db,
            state: Some("running".to_string()),
        })
    }

    async fn delete_workspace(&self, workspace_name: &str) -> Result<()> {
        let docker = container::connect()?;
        if container::ensure_running_container(&docker, &self.container_spec())
            .await
            .is_err()
        {
            return Ok(());
        }
        let db = self.db_name(workspace_name);
        self.query(
            &docker,
            &format!("DROP DATABASE IF EXISTS {}", Self::quote(&db)),
        )
        .await
        .with_context(|| format!("failed to drop ClickHouse database '{db}'"))?;
        Ok(())
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;
        let prefix = naming::project_db_prefix(&self.project_name);
        let escaped = prefix.replace('_', "\\_");
        let sql = format!(
            "SELECT name FROM system.databases WHERE name LIKE '{}%' ORDER BY name",
            escaped.replace('\'', "''")
        );
        let out = self.query(&docker, &sql).await?;
        Ok(out
            .lines()
            .map(str::trim)
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
            .collect())
    }

    async fn workspace_exists(&self, workspace_name: &str) -> Result<bool> {
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;
        let db = self.db_name(workspace_name);
        let out = self
            .query(
                &docker,
                &format!(
                    "SELECT count() FROM system.databases WHERE name = '{}'",
                    db.replace('\'', "''")
                ),
            )
            .await?;
        Ok(out.trim() != "0" && !out.trim().is_empty())
    }

    async fn switch_to_branch(&self, workspace_name: &str) -> Result<WorkspaceInfo> {
        self.create_workspace(workspace_name, None).await
    }

    async fn get_connection_info(&self, workspace_name: &str) -> Result<ConnectionInfo> {
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
        let workspaces = self.list_workspaces().await.unwrap_or_default();
        Ok(Some((
            self.container_name.clone(),
            workspaces.into_iter().map(|w| w.database_name).collect(),
        )))
    }

    async fn destroy_project(&self) -> Result<Vec<String>> {
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;
        let mut dropped = Vec::new();
        for w in self.list_workspaces().await? {
            match self
                .query(
                    &docker,
                    &format!("DROP DATABASE IF EXISTS {}", Self::quote(&w.database_name)),
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
                    name: "clickhouse-container".to_string(),
                    available: running,
                    detail: if running {
                        format!(
                            "'{}' running (HTTP {})",
                            self.container_name, self.http_port
                        )
                    } else {
                        format!(
                            "'{}' not running (started on first use)",
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

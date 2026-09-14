//! Shared object-storage provider backed by RustFS (Rust-native, S3-compatible,
//! Apache-2.0). One global `rustfs/rustfs` container is kept running and each
//! workspace gets its own bucket, provisioned on the fly.

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use s3::creds::Credentials;
use s3::{Bucket, BucketConfiguration, Region};

use super::container::{self, GlobalContainerSpec};
use super::naming;
use crate::config::SharedServiceConfig;
use crate::services::{ConnectionInfo, DoctorReport, ProjectInfo, ServiceProvider, WorkspaceInfo};

const DEFAULT_IMAGE: &str = "rustfs/rustfs:latest";
const DEFAULT_S3_PORT: u16 = 9000;
const DEFAULT_CONSOLE_PORT: u16 = 9001;
const DEFAULT_ACCESS_KEY: &str = "rustfsadmin";
const DEFAULT_SECRET_KEY: &str = "rustfsadmin";
const DEFAULT_CONTAINER: &str = "devflow-shared-rustfs";
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// A RustFS-backed object-storage provider (one global container, a bucket per
/// workspace).
pub struct RustFsProvider {
    project_name: String,
    resources: crate::services::ownership::ResourceRegistry,
    image: String,
    container_name: String,
    s3_port: u16,
    console_port: u16,
    access_key: String,
    secret_key: String,
}

impl RustFsProvider {
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
            s3_port: c.port.unwrap_or(DEFAULT_S3_PORT),
            console_port: DEFAULT_CONSOLE_PORT,
            access_key: c.user.unwrap_or_else(|| DEFAULT_ACCESS_KEY.to_string()),
            secret_key: c.password.unwrap_or_else(|| DEFAULT_SECRET_KEY.to_string()),
        })
    }

    fn bucket_name(&self, workspace: &str) -> String {
        self.resources.resource_name(workspace)
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.s3_port)
    }

    fn region(&self) -> Region {
        Region::Custom {
            region: "us-east-1".to_string(),
            endpoint: self.endpoint(),
        }
    }

    fn credentials(&self) -> Result<Credentials> {
        Credentials::new(
            Some(&self.access_key),
            Some(&self.secret_key),
            None,
            None,
            None,
        )
        .context("invalid RustFS credentials")
    }

    /// A path-style bucket handle for object operations.
    fn bucket_handle(&self, name: &str) -> Result<Box<Bucket>> {
        Ok(Bucket::new(name, self.region(), self.credentials()?)?.with_path_style())
    }

    fn container_spec(&self) -> GlobalContainerSpec {
        GlobalContainerSpec {
            container_name: self.container_name.clone(),
            image: self.image.clone(),
            host_port: self.s3_port,
            container_port: DEFAULT_S3_PORT,
            extra_port: Some((self.console_port, DEFAULT_CONSOLE_PORT)),
            // RustFS takes the data path as its command argument.
            cmd: vec!["/data".to_string()],
            env: vec![
                format!("RUSTFS_ACCESS_KEY={}", self.access_key),
                format!("RUSTFS_SECRET_KEY={}", self.secret_key),
                "RUSTFS_CONSOLE_ENABLE=true".to_string(),
                format!("RUSTFS_ADDRESS=:{}", DEFAULT_S3_PORT),
            ],
            binds: vec![format!("{}-data:/data", self.container_name)],
            labels: vec![
                ("devflow.service-type".to_string(), "rustfs".to_string()),
                ("devflow.shared".to_string(), "true".to_string()),
            ],
        }
    }

    /// Ensure the global container is running and the S3 API answers.
    async fn ensure_ready(&self) -> Result<()> {
        let docker = container::connect()?;
        container::ensure_running_container(&docker, &self.container_spec()).await?;

        // Poll ListBuckets until the S3 API is up (no `pg_isready` equivalent).
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            if Bucket::list_buckets(self.region(), self.credentials()?)
                .await
                .is_ok()
            {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for RustFS S3 API on {}", self.endpoint());
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }

    /// Empty a bucket (delete all objects) so it can be removed.
    async fn empty_bucket(&self, name: &str) -> Result<()> {
        let bucket = self.bucket_handle(name)?;
        let pages = bucket.list(String::new(), None).await?;
        for page in pages {
            for obj in page.contents {
                let response = bucket.delete_object(&obj.key).await?;
                anyhow::ensure!(
                    response.status_code() < 300,
                    "failed to delete object from owned bucket"
                );
            }
        }
        Ok(())
    }
}

#[async_trait]
impl ServiceProvider for RustFsProvider {
    async fn create_workspace(
        &self,
        workspace_name: &str,
        _from_workspace: Option<&str>,
    ) -> Result<WorkspaceInfo> {
        let _lease = self.resources.lease(workspace_name).await?;
        self.ensure_ready().await?;
        let name = self.bucket_name(workspace_name);
        let exists = self.bucket_handle(&name)?.exists().await?;
        self.resources.check_existing(workspace_name, exists)?;
        let legacy = naming::logical_bucket_name(&self.project_name, workspace_name);
        self.resources.check_legacy(
            workspace_name,
            &legacy,
            self.bucket_handle(&legacy)?.exists().await?,
        )?;
        self.resources.claim(workspace_name, None)?;
        if !exists {
            std::env::set_var("RUST_S3_SKIP_LOCATION_CONSTRAINT", "true");
            Bucket::create_with_path_style(
                &name,
                self.region(),
                self.credentials()?,
                BucketConfiguration::default(),
            )
            .await?;
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
        let record = self
            .resources
            .record(workspace_name)?
            .context("no ownership record; adopt legacy resources explicitly before deletion")?;
        self.ensure_ready().await?;
        let handle = self.bucket_handle(&record.resource)?;
        if handle.exists().await? {
            self.empty_bucket(&record.resource).await?;
            handle.delete().await?;
        }
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
                    if self.bucket_handle(&record.resource)?.exists().await? {
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
                Ok(self.bucket_handle(&record.resource)?.exists().await?)
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
        let bucket = self.bucket_name(workspace_name);
        Ok(ConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: self.s3_port,
            database: bucket.clone(),
            user: self.access_key.clone(),
            password: Some(self.secret_key.clone()),
            connection_string: Some(format!("{}/{}", self.endpoint(), bucket)),
        })
    }

    fn supports_destroy(&self) -> bool {
        true
    }

    async fn destroy_preview(&self) -> Result<Option<(String, Vec<String>)>> {
        Ok(Some((
            self.container_name.clone(),
            self.resources
                .list()?
                .into_iter()
                .map(|r| r.resource)
                .collect(),
        )))
    }

    async fn destroy_project(&self) -> Result<Vec<String>> {
        let mut removed = Vec::new();
        for record in self.resources.list()? {
            self.delete_workspace(&record.workspace).await?;
            removed.push(record.resource);
        }
        Ok(removed)
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
        self.ensure_ready().await
    }

    fn provider_name(&self) -> &'static str {
        "shared-rustfs"
    }

    fn project_info(&self) -> Option<ProjectInfo> {
        Some(ProjectInfo {
            name: self.project_name.clone(),
            storage_driver: Some("rustfs".to_string()),
            image: Some(self.image.clone()),
        })
    }
}

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
use crate::services::{
    ConnectionInfo, DoctorCheck, DoctorReport, ProjectInfo, ServiceProvider, WorkspaceInfo,
};

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
    #[allow(dead_code)]
    service_name: String,
    image: String,
    container_name: String,
    s3_port: u16,
    console_port: u16,
    access_key: String,
    secret_key: String,
}

impl RustFsProvider {
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
            s3_port: c.port.unwrap_or(DEFAULT_S3_PORT),
            console_port: DEFAULT_CONSOLE_PORT,
            access_key: c.user.unwrap_or_else(|| DEFAULT_ACCESS_KEY.to_string()),
            secret_key: c.password.unwrap_or_else(|| DEFAULT_SECRET_KEY.to_string()),
        })
    }

    fn bucket_name(&self, workspace: &str) -> String {
        naming::logical_bucket_name(&self.project_name, workspace)
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

    async fn list_project_buckets(&self) -> Result<Vec<String>> {
        let prefix = naming::project_bucket_prefix(&self.project_name);
        let resp = Bucket::list_buckets(self.region(), self.credentials()?)
            .await
            .context("failed to list RustFS buckets")?;
        Ok(resp
            .bucket_names()
            .filter(|n| n.starts_with(&prefix))
            .collect())
    }

    /// Empty a bucket (delete all objects) so it can be removed.
    async fn empty_bucket(&self, name: &str) -> Result<()> {
        let bucket = self.bucket_handle(name)?;
        let pages = bucket.list(String::new(), None).await.unwrap_or_default();
        for page in pages {
            for obj in page.contents {
                let _ = bucket.delete_object(&obj.key).await;
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
        self.ensure_ready().await?;
        let name = self.bucket_name(workspace_name);

        // Tell rust-s3 to skip the LocationConstraint body, which RustFS/MinIO
        // don't require (and reject for non-AWS regions).
        std::env::set_var("RUST_S3_SKIP_LOCATION_CONSTRAINT", "true");

        let handle = self.bucket_handle(&name)?;
        let already = handle.exists().await.unwrap_or(false);
        if !already {
            Bucket::create_with_path_style(
                &name,
                self.region(),
                self.credentials()?,
                BucketConfiguration::default(),
            )
            .await
            .with_context(|| format!("failed to create bucket '{name}'"))?;
        }

        Ok(WorkspaceInfo {
            name: workspace_name.to_string(),
            created_at: None,
            parent_workspace: None,
            database_name: name,
            state: Some("running".to_string()),
        })
    }

    async fn delete_workspace(&self, workspace_name: &str) -> Result<()> {
        // If the engine isn't reachable there's nothing to remove.
        if self.ensure_ready().await.is_err() {
            return Ok(());
        }
        let name = self.bucket_name(workspace_name);
        let handle = self.bucket_handle(&name)?;
        if !handle.exists().await.unwrap_or(false) {
            return Ok(());
        }
        // A bucket must be empty before it can be deleted.
        self.empty_bucket(&name).await?;
        handle
            .delete()
            .await
            .with_context(|| format!("failed to delete bucket '{name}'"))?;
        Ok(())
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        self.ensure_ready().await?;
        let prefix = naming::project_bucket_prefix(&self.project_name);
        let buckets = self.list_project_buckets().await?;
        Ok(buckets
            .into_iter()
            .map(|bucket| {
                let ws = bucket.strip_prefix(&prefix).unwrap_or(&bucket).to_string();
                WorkspaceInfo {
                    name: ws,
                    created_at: None,
                    parent_workspace: None,
                    database_name: bucket,
                    state: Some("running".to_string()),
                }
            })
            .collect())
    }

    async fn workspace_exists(&self, workspace_name: &str) -> Result<bool> {
        self.ensure_ready().await?;
        let handle = self.bucket_handle(&self.bucket_name(workspace_name))?;
        Ok(handle.exists().await.unwrap_or(false))
    }

    async fn switch_to_branch(&self, workspace_name: &str) -> Result<WorkspaceInfo> {
        self.create_workspace(workspace_name, None).await
    }

    async fn get_connection_info(&self, workspace_name: &str) -> Result<ConnectionInfo> {
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
        let buckets = self.list_project_buckets().await.unwrap_or_default();
        Ok(Some((self.container_name.clone(), buckets)))
    }

    async fn destroy_project(&self) -> Result<Vec<String>> {
        self.ensure_ready().await?;
        let mut removed = Vec::new();
        for bucket in self.list_project_buckets().await? {
            let _ = self.empty_bucket(&bucket).await;
            match self.bucket_handle(&bucket) {
                Ok(h) => match h.delete().await {
                    Ok(_) => removed.push(bucket),
                    Err(e) => log::warn!("Failed to delete bucket '{}': {}", bucket, e),
                },
                Err(e) => log::warn!("Failed to handle bucket '{}': {}", bucket, e),
            }
        }
        Ok(removed)
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
                    name: "rustfs-container".to_string(),
                    available: running,
                    detail: if running {
                        format!(
                            "'{}' running (S3 {}, console {})",
                            self.container_name, self.s3_port, self.console_port
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

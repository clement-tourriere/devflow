//! ClickHouse local provider — manages branch-isolated ClickHouse Docker containers.
//!
//! Each branch gets its own ClickHouse container. Data is stored in bind-mounted
//! directories under `data_root/clickhouse/{service_name}/{branch_name}/`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use bollard::models::{ContainerCreateBody, HostConfig, PortBinding, PortMap};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, RemoveContainerOptions, StopContainerOptions,
};
use bollard::Docker;
use chrono::Utc;
use futures_util::TryStreamExt;
use tokio::time::{sleep, Instant};

use crate::config::ClickHouseConfig;
use crate::services::{
    local_docker::{
        collect_container_logs, inspect_container_status, list_managed_service_containers,
        pick_available_port_pair, sanitize_name_component, service_branch_prefix, ContainerStatus,
    },
    BranchInfo, ConnectionInfo, DoctorCheck, DoctorReport, ProjectInfo, ServiceCapabilities,
    ServiceProvider,
};

/// Default ClickHouse ports: HTTP 8123, native TCP 9000.
const CLICKHOUSE_HTTP_PORT: u16 = 8123;
const CLICKHOUSE_NATIVE_PORT: u16 = 9000;

pub struct ClickHouseLocalProvider {
    project_name: String,
    service_name: String,
    image: String,
    port_range_start: u16,
    data_root: PathBuf,
    user: String,
    password: Option<String>,
    client: Docker,
}

impl ClickHouseLocalProvider {
    pub fn new(
        project_name: &str,
        service_name: &str,
        config: &ClickHouseConfig,
    ) -> anyhow::Result<Self> {
        let client =
            Docker::connect_with_local_defaults().context("Failed to connect to Docker daemon. Is Docker installed and running? Check with: docker info")?;

        let data_root = if let Some(ref root) = config.data_root {
            let expanded = shellexpand(root);
            PathBuf::from(expanded)
        } else {
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("devflow")
                .join("clickhouse")
        };

        Ok(Self {
            project_name: project_name.to_string(),
            service_name: service_name.to_string(),
            image: config.image.clone(),
            port_range_start: config.port_range_start.unwrap_or(59000),
            data_root,
            user: config.user.clone(),
            password: config.password.clone(),
            client,
        })
    }

    fn container_name(&self, branch_name: &str) -> String {
        let raw = format!(
            "devflow-{}-{}-{}",
            sanitize_name_component(&self.project_name),
            sanitize_name_component(&self.service_name),
            sanitize_name_component(branch_name)
        );
        if raw.len() > 128 {
            raw[..128].trim_end_matches('-').to_string()
        } else {
            raw
        }
    }

    fn branch_data_dir(&self, branch_name: &str) -> PathBuf {
        self.data_root
            .join(&self.service_name)
            .join(sanitize_name_component(branch_name))
    }

    async fn container_status(&self, container_name: &str) -> anyhow::Result<ContainerStatus> {
        inspect_container_status(&self.client, container_name).await
    }

    async fn ensure_image(&self) -> anyhow::Result<()> {
        if self.client.inspect_image(&self.image).await.is_ok() {
            return Ok(());
        }

        let (from_image, tag) = if let Some((name, tag)) = self.image.rsplit_once(':') {
            (name.to_string(), Some(tag.to_string()))
        } else {
            (self.image.clone(), None)
        };

        let options = CreateImageOptions {
            from_image: Some(from_image),
            tag,
            ..Default::default()
        };

        self.client
            .create_image(Some(options), None, None)
            .try_collect::<Vec<_>>()
            .await
            .with_context(|| format!("failed to pull docker image '{}'", self.image))?;

        Ok(())
    }

    async fn pick_port(&self) -> anyhow::Result<u16> {
        pick_available_port_pair(&self.client, self.port_range_start).await
    }

    async fn get_container_port(
        &self,
        container_name: &str,
        container_port: &str,
    ) -> anyhow::Result<Option<u16>> {
        let info = self
            .client
            .inspect_container(
                container_name,
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
            .await?;

        if let Some(network) = info.network_settings {
            if let Some(ports) = network.ports {
                if let Some(Some(bindings)) = ports.get(container_port) {
                    for binding in bindings {
                        if let Some(ref host_port) = binding.host_port {
                            if let Ok(port) = host_port.parse::<u16>() {
                                return Ok(Some(port));
                            }
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    async fn create_and_start(
        &self,
        container_name: &str,
        branch_name: &str,
        http_port: u16,
    ) -> anyhow::Result<()> {
        self.ensure_image().await?;

        let data_dir = self.branch_data_dir(branch_name);
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("failed to create data dir: {}", data_dir.display()))?;

        let native_port = http_port + 1; // Use next port for native protocol

        let mut port_bindings: PortMap = HashMap::new();
        port_bindings.insert(
            format!("{CLICKHOUSE_HTTP_PORT}/tcp"),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(http_port.to_string()),
            }]),
        );
        port_bindings.insert(
            format!("{CLICKHOUSE_NATIVE_PORT}/tcp"),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some(native_port.to_string()),
            }]),
        );

        let mount = format!("{}:/var/lib/clickhouse", data_dir.display());

        let mut env = vec![format!("CLICKHOUSE_USER={}", self.user)];
        if let Some(ref password) = self.password {
            env.push(format!("CLICKHOUSE_PASSWORD={password}"));
        }

        let mut labels = HashMap::new();
        labels.insert("devflow.managed".to_string(), "true".to_string());
        labels.insert("devflow.project".to_string(), self.project_name.clone());
        labels.insert("devflow.service".to_string(), self.service_name.clone());
        labels.insert("devflow.service-type".to_string(), "clickhouse".to_string());

        let config = ContainerCreateBody {
            image: Some(self.image.clone()),
            env: Some(env),
            labels: Some(labels),
            host_config: Some(HostConfig {
                binds: Some(vec![mount]),
                port_bindings: Some(port_bindings),
                ..Default::default()
            }),
            ..Default::default()
        };

        let options = CreateContainerOptions {
            name: Some(container_name.to_string()),
            ..Default::default()
        };

        self.client
            .create_container(Some(options), config)
            .await
            .with_context(|| format!("failed to create container '{container_name}'"))?;

        self.client
            .start_container(
                container_name,
                None::<bollard::query_parameters::StartContainerOptions>,
            )
            .await
            .with_context(|| format!("failed to start container '{container_name}'"))?;

        Ok(())
    }

    async fn wait_ready(&self, container_name: &str, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;

        loop {
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "timed out waiting for ClickHouse readiness in '{container_name}'"
                ));
            }

            match self.container_status(container_name).await? {
                ContainerStatus::NotFound => {
                    return Err(anyhow!("container '{container_name}' does not exist"));
                }
                ContainerStatus::Running => {
                    // clickhouse-client --query "SELECT 1"
                    if self
                        .exec_check(
                            container_name,
                            &[
                                "clickhouse-client",
                                "--user",
                                &self.user,
                                "--query",
                                "SELECT 1",
                            ],
                        )
                        .await
                    {
                        return Ok(());
                    }
                }
                _ => {}
            }

            sleep(Duration::from_millis(500)).await;
        }
    }

    async fn exec_check(&self, container_name: &str, cmd: &[&str]) -> bool {
        let config = bollard::models::ExecConfig {
            cmd: Some(cmd.iter().map(|s| s.to_string()).collect()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        };

        let exec = match self.client.create_exec(container_name, config).await {
            Ok(e) => e,
            Err(_) => return false,
        };

        let start_opts = Some(bollard::exec::StartExecOptions {
            detach: false,
            ..Default::default()
        });

        match self.client.start_exec(&exec.id, start_opts).await {
            Ok(bollard::exec::StartExecResults::Attached { mut output, .. }) => {
                while output.try_next().await.ok().flatten().is_some() {}
            }
            Ok(bollard::exec::StartExecResults::Detached) => {}
            Err(_) => return false,
        }

        match self.client.inspect_exec(&exec.id).await {
            Ok(info) => info.exit_code == Some(0),
            Err(_) => false,
        }
    }

    async fn list_managed_containers(&self) -> anyhow::Result<Vec<(String, String, bool)>> {
        let prefix = service_branch_prefix(&self.project_name, &self.service_name);
        list_managed_service_containers(&self.client, &self.service_name, &prefix).await
    }
}

#[async_trait]
impl ServiceProvider for ClickHouseLocalProvider {
    async fn create_branch(
        &self,
        branch_name: &str,
        from_branch: Option<&str>,
    ) -> anyhow::Result<BranchInfo> {
        let container_name = self.container_name(branch_name);

        match self.container_status(&container_name).await? {
            ContainerStatus::Running => {
                return Ok(BranchInfo {
                    name: branch_name.to_string(),
                    created_at: Some(Utc::now()),
                    parent_branch: from_branch.map(|s| s.to_string()),
                    database_name: container_name,
                    state: Some("running".to_string()),
                });
            }
            ContainerStatus::Exited | ContainerStatus::Paused => {
                self.client
                    .start_container(
                        &container_name,
                        None::<bollard::query_parameters::StartContainerOptions>,
                    )
                    .await
                    .with_context(|| format!("failed to start container '{container_name}'"))?;

                self.wait_ready(&container_name, Duration::from_secs(60))
                    .await?;

                return Ok(BranchInfo {
                    name: branch_name.to_string(),
                    created_at: Some(Utc::now()),
                    parent_branch: from_branch.map(|s| s.to_string()),
                    database_name: container_name,
                    state: Some("running".to_string()),
                });
            }
            ContainerStatus::NotFound | ContainerStatus::Other(_) => {}
        }

        // Clone data from parent branch if specified
        if let Some(parent_name) = from_branch {
            let parent_container = self.container_name(parent_name);
            let parent_data_dir = self.branch_data_dir(parent_name);
            let new_data_dir = self.branch_data_dir(branch_name);

            if parent_data_dir.exists() {
                // Stop parent container to ensure data consistency
                let parent_running = matches!(
                    self.container_status(&parent_container).await?,
                    ContainerStatus::Running
                );
                if parent_running {
                    self.client
                        .stop_container(
                            &parent_container,
                            Some(StopContainerOptions {
                                t: Some(10),
                                ..Default::default()
                            }),
                        )
                        .await
                        .with_context(|| {
                            format!("failed to stop parent container '{parent_container}'")
                        })?;
                }

                crate::services::clone_data_dir(&parent_data_dir, &new_data_dir).await?;

                // Restart parent if it was running
                if parent_running {
                    self.client
                        .start_container(
                            &parent_container,
                            None::<bollard::query_parameters::StartContainerOptions>,
                        )
                        .await
                        .with_context(|| {
                            format!("failed to restart parent container '{parent_container}'")
                        })?;
                }
            }
        }

        // Allocate two consecutive ports (HTTP + native)
        let http_port = self.pick_port().await?;
        self.create_and_start(&container_name, branch_name, http_port)
            .await?;
        self.wait_ready(&container_name, Duration::from_secs(120))
            .await?;

        Ok(BranchInfo {
            name: branch_name.to_string(),
            created_at: Some(Utc::now()),
            parent_branch: from_branch.map(|s| s.to_string()),
            database_name: container_name,
            state: Some("running".to_string()),
        })
    }

    async fn delete_branch(&self, branch_name: &str) -> anyhow::Result<()> {
        let container_name = self.container_name(branch_name);

        if !matches!(
            self.container_status(&container_name).await?,
            ContainerStatus::NotFound
        ) {
            let options = RemoveContainerOptions {
                force: true,
                ..Default::default()
            };
            self.client
                .remove_container(&container_name, Some(options))
                .await
                .with_context(|| format!("failed to remove container '{container_name}'"))?;
        }

        // Clean up data directory
        let data_dir = self.branch_data_dir(branch_name);
        if data_dir.exists() {
            std::fs::remove_dir_all(&data_dir)
                .with_context(|| format!("failed to remove data dir: {}", data_dir.display()))?;
        }

        Ok(())
    }

    async fn list_branches(&self) -> anyhow::Result<Vec<BranchInfo>> {
        let containers = self.list_managed_containers().await?;
        Ok(containers
            .into_iter()
            .map(|(branch, container_name, is_running)| BranchInfo {
                name: branch,
                created_at: None,
                parent_branch: None,
                database_name: container_name,
                state: Some(if is_running { "running" } else { "stopped" }.to_string()),
            })
            .collect())
    }

    async fn branch_exists(&self, branch_name: &str) -> anyhow::Result<bool> {
        let container_name = self.container_name(branch_name);
        Ok(!matches!(
            self.container_status(&container_name).await?,
            ContainerStatus::NotFound
        ))
    }

    async fn switch_to_branch(&self, branch_name: &str) -> anyhow::Result<BranchInfo> {
        let container_name = self.container_name(branch_name);

        match self.container_status(&container_name).await? {
            ContainerStatus::Running => {}
            ContainerStatus::Exited | ContainerStatus::Paused | ContainerStatus::Other(_) => {
                self.client
                    .start_container(
                        &container_name,
                        None::<bollard::query_parameters::StartContainerOptions>,
                    )
                    .await
                    .with_context(|| format!("failed to start container '{container_name}'"))?;
                self.wait_ready(&container_name, Duration::from_secs(60))
                    .await?;
            }
            ContainerStatus::NotFound => {
                return Err(anyhow!(
                    "no ClickHouse container for branch '{branch_name}' on service '{}'",
                    self.service_name
                ));
            }
        }

        Ok(BranchInfo {
            name: branch_name.to_string(),
            created_at: None,
            parent_branch: None,
            database_name: container_name,
            state: Some("running".to_string()),
        })
    }

    async fn get_connection_info(&self, branch_name: &str) -> anyhow::Result<ConnectionInfo> {
        let container_name = self.container_name(branch_name);
        let http_port = self
            .get_container_port(&container_name, &format!("{CLICKHOUSE_HTTP_PORT}/tcp"))
            .await?
            .unwrap_or(self.port_range_start);

        Ok(ConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: http_port,
            database: "default".to_string(),
            user: self.user.clone(),
            password: self.password.clone(),
            connection_string: Some(format!(
                "http://{}:{}@127.0.0.1:{http_port}",
                self.user,
                self.password.as_deref().unwrap_or("")
            )),
        })
    }

    fn supports_lifecycle(&self) -> bool {
        true
    }

    async fn start_branch(&self, branch_name: &str) -> anyhow::Result<()> {
        let container_name = self.container_name(branch_name);
        match self.container_status(&container_name).await? {
            ContainerStatus::Running => Ok(()),
            ContainerStatus::NotFound => Err(anyhow!(
                "no ClickHouse container for branch '{branch_name}'"
            )),
            _ => {
                self.client
                    .start_container(
                        &container_name,
                        None::<bollard::query_parameters::StartContainerOptions>,
                    )
                    .await
                    .with_context(|| format!("failed to start container '{container_name}'"))?;
                self.wait_ready(&container_name, Duration::from_secs(60))
                    .await
            }
        }
    }

    async fn stop_branch(&self, branch_name: &str) -> anyhow::Result<()> {
        let container_name = self.container_name(branch_name);
        match self.container_status(&container_name).await? {
            ContainerStatus::NotFound | ContainerStatus::Exited => return Ok(()),
            ContainerStatus::Paused => {
                self.client.unpause_container(&container_name).await.ok();
            }
            _ => {}
        }

        let options = StopContainerOptions {
            t: Some(20),
            ..Default::default()
        };
        self.client
            .stop_container(&container_name, Some(options))
            .await
            .with_context(|| format!("failed to stop container '{container_name}'"))?;
        Ok(())
    }

    fn supports_destroy(&self) -> bool {
        true
    }

    async fn destroy_preview(&self) -> anyhow::Result<Option<(String, Vec<String>)>> {
        let containers = self.list_managed_containers().await?;
        if containers.is_empty() {
            return Ok(None);
        }
        let names: Vec<String> = containers.into_iter().map(|(b, _, _)| b).collect();
        Ok(Some((self.service_name.clone(), names)))
    }

    async fn destroy_project(&self) -> anyhow::Result<Vec<String>> {
        let containers = self.list_managed_containers().await?;
        let mut deleted = Vec::new();

        for (branch_name, container_name, _) in &containers {
            let options = RemoveContainerOptions {
                force: true,
                ..Default::default()
            };
            match self
                .client
                .remove_container(container_name, Some(options))
                .await
            {
                Ok(()) => deleted.push(branch_name.clone()),
                Err(e) => log::warn!("failed to remove container '{}': {}", container_name, e),
            }
        }

        // Clean up all data
        let service_dir = self.data_root.join(&self.service_name);
        if service_dir.exists() {
            std::fs::remove_dir_all(&service_dir).ok();
        }

        Ok(deleted)
    }

    async fn doctor(&self) -> anyhow::Result<DoctorReport> {
        let mut checks = Vec::new();

        match self.client.version().await {
            Ok(info) => {
                checks.push(DoctorCheck {
                    name: "Docker".to_string(),
                    available: true,
                    detail: format!("Docker {} reachable", info.version.unwrap_or_default()),
                });
            }
            Err(err) => {
                checks.push(DoctorCheck {
                    name: "Docker".to_string(),
                    available: false,
                    detail: format!(
                        "Docker unreachable: {err}. Is Docker running? Try: docker info"
                    ),
                });
            }
        }

        let image_available = self.client.inspect_image(&self.image).await.is_ok();
        checks.push(DoctorCheck {
            name: format!("Image: {}", self.image),
            available: image_available,
            detail: if image_available {
                "available locally".to_string()
            } else {
                "not pulled yet".to_string()
            },
        });

        checks.push(DoctorCheck {
            name: "Data root".to_string(),
            available: true,
            detail: self.data_root.display().to_string(),
        });

        Ok(DoctorReport { checks })
    }

    async fn test_connection(&self) -> anyhow::Result<()> {
        self.client
            .version()
            .await
            .context("Docker is not available")?;
        Ok(())
    }

    fn project_info(&self) -> Option<ProjectInfo> {
        Some(ProjectInfo {
            name: self.service_name.clone(),
            storage_driver: Some("docker-bind".to_string()),
            image: Some(self.image.clone()),
        })
    }

    async fn logs(&self, branch_name: &str, tail: Option<usize>) -> anyhow::Result<String> {
        let container = self.container_name(branch_name);
        collect_container_logs(&self.client, &container, tail).await
    }

    fn provider_name(&self) -> &'static str {
        "ClickHouse (Docker)"
    }

    fn capabilities(&self) -> ServiceCapabilities {
        ServiceCapabilities {
            lifecycle: true,
            logs: true,
            destroy_project: true,
            cleanup: true,
            seed_from_source: false,
            template_from_time: false,
            max_branch_name_length: 255,
        }
    }

    fn max_branch_name_length(&self) -> usize {
        255
    }
}

fn shellexpand(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).display().to_string();
        }
    }
    path.to_string()
}

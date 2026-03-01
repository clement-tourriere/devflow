//! Generic Docker service provider.
//!
//! Runs arbitrary Docker images as branch-isolated containers.
//! Each branch gets its own container instance. Data persistence is optional
//! (via Docker volumes or bind mounts configured in the service config).

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use bollard::models::{
    ContainerCreateBody, ContainerStateStatusEnum, HostConfig, PortBinding, PortMap,
};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, ListContainersOptions, LogsOptionsBuilder,
    RemoveContainerOptions, StopContainerOptions,
};
use bollard::Docker;
use chrono::Utc;
use futures_util::TryStreamExt;
use tokio::time::{sleep, Instant};

use crate::config::GenericDockerConfig;
use crate::services::{
    BranchInfo, ConnectionInfo, DoctorCheck, DoctorReport, ProjectInfo, ServiceProvider,
};

/// A generic Docker service provider that manages branch-isolated containers.
pub struct GenericDockerProvider {
    /// Project name from config (e.g. "myapp").
    project_name: String,
    /// Logical service name from config (e.g. "cache", "search").
    service_name: String,
    /// Docker image to use.
    image: String,
    /// Static port mapping (e.g. "6379:6379"). Used when auto_branch is false.
    port_mapping: Option<String>,
    /// Start of port range for branch-specific instances.
    port_range_start: Option<u16>,
    /// Environment variables for the container.
    environment: HashMap<String, String>,
    /// Docker volumes to mount.
    volumes: Vec<String>,
    /// Custom command override.
    command: Option<String>,
    /// Health check command.
    healthcheck: Option<String>,
    /// Docker client.
    client: Docker,
}

impl GenericDockerProvider {
    pub fn new(
        project_name: &str,
        service_name: &str,
        config: &GenericDockerConfig,
    ) -> anyhow::Result<Self> {
        let client =
            Docker::connect_with_local_defaults().context("Failed to connect to Docker daemon. Is Docker installed and running? Check with: docker info")?;

        Ok(Self {
            project_name: project_name.to_string(),
            service_name: service_name.to_string(),
            image: config.image.clone(),
            port_mapping: config.port_mapping.clone(),
            port_range_start: config.port_range_start,
            environment: config.environment.clone(),
            volumes: config.volumes.clone(),
            command: config.command.clone(),
            healthcheck: config.healthcheck.clone(),
            client,
        })
    }

    fn container_name(&self, branch_name: &str) -> String {
        let raw = format!(
            "devflow-{}-{}-{}",
            sanitize(&self.project_name),
            sanitize(&self.service_name),
            sanitize(branch_name)
        );
        if raw.len() > 128 {
            raw[..128].trim_end_matches('-').to_string()
        } else {
            raw
        }
    }

    async fn container_status(&self, container_name: &str) -> anyhow::Result<ContainerStatus> {
        match self
            .client
            .inspect_container(
                container_name,
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
            .await
        {
            Ok(info) => {
                let status = info.state.and_then(|s| s.status);
                match status {
                    Some(ContainerStateStatusEnum::RUNNING) => Ok(ContainerStatus::Running),
                    Some(ContainerStateStatusEnum::PAUSED) => Ok(ContainerStatus::Paused),
                    Some(ContainerStateStatusEnum::EXITED)
                    | Some(ContainerStateStatusEnum::CREATED) => Ok(ContainerStatus::Exited),
                    Some(other) => Ok(ContainerStatus::Other(other.to_string())),
                    None => Ok(ContainerStatus::Other("unknown".to_string())),
                }
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(ContainerStatus::NotFound),
            Err(err) => Err(anyhow!(
                "failed to inspect container '{container_name}': {err}"
            )),
        }
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

    /// Pick a port for a branch. Uses port_range_start + offset based on existing containers.
    async fn pick_port_for_branch(&self) -> anyhow::Result<u16> {
        let start = self.port_range_start.unwrap_or(56000);
        pick_available_port(&self.client, start).await
    }

    /// Get the host port for an existing container by inspecting it.
    async fn get_container_port(&self, container_name: &str) -> anyhow::Result<Option<u16>> {
        let info = self
            .client
            .inspect_container(
                container_name,
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
            .await?;

        // Look at the network settings for any published port
        if let Some(network) = info.network_settings {
            if let Some(ports) = network.ports {
                for (_container_port, bindings) in ports {
                    if let Some(bindings) = bindings {
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
        }

        Ok(None)
    }

    /// Build environment variables as Docker env format.
    fn build_env(&self) -> Vec<String> {
        self.environment
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect()
    }

    /// Determine the container port from the image's port_mapping or a known default.
    fn container_port(&self) -> Option<String> {
        // If port_mapping is specified like "6379:6379", extract the container port (right side)
        if let Some(ref mapping) = self.port_mapping {
            if let Some((_host, container)) = mapping.rsplit_once(':') {
                return Some(format!("{container}/tcp"));
            }
        }
        None
    }

    async fn create_and_start_container(
        &self,
        container_name: &str,
        port: u16,
    ) -> anyhow::Result<()> {
        self.ensure_image().await?;

        let mut port_bindings: PortMap = HashMap::new();

        // Determine what container port to map
        if let Some(container_port_key) = self.container_port() {
            port_bindings.insert(
                container_port_key,
                Some(vec![PortBinding {
                    host_ip: Some("0.0.0.0".to_string()),
                    host_port: Some(port.to_string()),
                }]),
            );
        }

        let mut labels = HashMap::new();
        labels.insert("devflow.managed".to_string(), "true".to_string());
        labels.insert("devflow.service".to_string(), self.service_name.clone());

        let binds: Option<Vec<String>> = if self.volumes.is_empty() {
            None
        } else {
            Some(self.volumes.clone())
        };

        let cmd = self
            .command
            .as_ref()
            .map(|c| vec!["/bin/sh".to_string(), "-c".to_string(), c.clone()]);

        let config = ContainerCreateBody {
            image: Some(self.image.clone()),
            env: Some(self.build_env()),
            labels: Some(labels),
            cmd,
            host_config: Some(HostConfig {
                binds,
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

    /// Wait for a generic container to be healthy using the configured healthcheck.
    async fn wait_healthy(&self, container_name: &str, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;

        // If no healthcheck configured, just wait for the container to be running
        if self.healthcheck.is_none() {
            // Give it a moment to start
            sleep(Duration::from_secs(1)).await;
            return Ok(());
        }

        let hc_cmd = self.healthcheck.as_ref().unwrap();

        loop {
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "timed out waiting for container '{container_name}' to become healthy"
                ));
            }

            match self.container_status(container_name).await? {
                ContainerStatus::NotFound => {
                    return Err(anyhow!("container '{container_name}' does not exist"));
                }
                ContainerStatus::Running => {
                    if self.exec_check(container_name, hc_cmd).await {
                        return Ok(());
                    }
                }
                _ => {}
            }

            sleep(Duration::from_millis(500)).await;
        }
    }

    /// Run a shell command inside a container and return true if it exits successfully.
    async fn exec_check(&self, container_name: &str, cmd: &str) -> bool {
        let config = bollard::models::ExecConfig {
            cmd: Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                cmd.to_string(),
            ]),
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

    /// List all devflow-managed containers for this service.
    async fn list_managed_containers(&self) -> anyhow::Result<Vec<(String, String, bool)>> {
        let prefix = format!(
            "devflow-{}-{}-",
            sanitize(&self.project_name),
            sanitize(&self.service_name)
        );

        let options = ListContainersOptions {
            all: true,
            ..Default::default()
        };

        let containers = self
            .client
            .list_containers(Some(options))
            .await
            .context("failed to list Docker containers")?;

        let mut result = Vec::new();
        for container in containers {
            // Check if this container belongs to our service
            let is_managed = container
                .labels
                .as_ref()
                .and_then(|l| l.get("devflow.managed"))
                .map(|v| v == "true")
                .unwrap_or(false);

            let is_our_service = container
                .labels
                .as_ref()
                .and_then(|l| l.get("devflow.service"))
                .map(|v| v == &self.service_name)
                .unwrap_or(false);

            if !is_managed || !is_our_service {
                continue;
            }

            // Extract branch name from container name
            if let Some(names) = &container.names {
                for name in names {
                    let clean_name = name.trim_start_matches('/');
                    if let Some(branch_part) = clean_name.strip_prefix(&prefix) {
                        let is_running = container
                            .state
                            .as_ref()
                            .map(|s| {
                                matches!(s, bollard::models::ContainerSummaryStateEnum::RUNNING)
                            })
                            .unwrap_or(false);
                        result.push((branch_part.to_string(), clean_name.to_string(), is_running));
                    }
                }
            }
        }

        Ok(result)
    }
}

#[async_trait]
impl ServiceProvider for GenericDockerProvider {
    async fn create_branch(
        &self,
        branch_name: &str,
        from_branch: Option<&str>,
    ) -> anyhow::Result<BranchInfo> {
        let container_name = self.container_name(branch_name);

        if from_branch.is_some() {
            eprintln!(
                "note: generic Docker provider does not support data cloning from parent branches. \
                 Creating a fresh container instead."
            );
        }

        // Check if already exists
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
                // Restart it
                self.client
                    .start_container(
                        &container_name,
                        None::<bollard::query_parameters::StartContainerOptions>,
                    )
                    .await
                    .with_context(|| format!("failed to start container '{container_name}'"))?;

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

        let port = self.pick_port_for_branch().await?;
        self.create_and_start_container(&container_name, port)
            .await?;
        self.wait_healthy(&container_name, Duration::from_secs(60))
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

        if matches!(
            self.container_status(&container_name).await?,
            ContainerStatus::NotFound
        ) {
            return Ok(());
        }

        let options = RemoveContainerOptions {
            force: true,
            ..Default::default()
        };

        self.client
            .remove_container(&container_name, Some(options))
            .await
            .with_context(|| format!("failed to remove container '{container_name}'"))?;

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

                self.wait_healthy(&container_name, Duration::from_secs(60))
                    .await?;
            }
            ContainerStatus::NotFound => {
                return Err(anyhow!(
                    "no container found for branch '{}' on service '{}'",
                    branch_name,
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
        let port = self.get_container_port(&container_name).await?.unwrap_or(0);

        Ok(ConnectionInfo {
            host: "127.0.0.1".to_string(),
            port,
            database: self.service_name.clone(),
            user: String::new(),
            password: None,
            connection_string: None,
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
                "no container for branch '{}' on service '{}'",
                branch_name,
                self.service_name
            )),
            _ => {
                self.client
                    .start_container(
                        &container_name,
                        None::<bollard::query_parameters::StartContainerOptions>,
                    )
                    .await
                    .with_context(|| format!("failed to start container '{container_name}'"))?;
                self.wait_healthy(&container_name, Duration::from_secs(60))
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

        Ok(deleted)
    }

    async fn doctor(&self) -> anyhow::Result<DoctorReport> {
        let mut checks = Vec::new();

        // Check Docker connectivity
        match self.client.version().await {
            Ok(info) => {
                let version = info.version.unwrap_or_default();
                checks.push(DoctorCheck {
                    name: "Docker".to_string(),
                    available: true,
                    detail: format!("Docker {} reachable", version),
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

        // Check if image is available locally
        let image_available = self.client.inspect_image(&self.image).await.is_ok();
        checks.push(DoctorCheck {
            name: format!("Image: {}", self.image),
            available: image_available,
            detail: if image_available {
                "available locally".to_string()
            } else {
                "not pulled yet (will be pulled on first use)".to_string()
            },
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
            storage_driver: None,
            image: Some(self.image.clone()),
        })
    }

    async fn logs(&self, branch_name: &str, tail: Option<usize>) -> anyhow::Result<String> {
        let container = self.container_name(branch_name);
        let options = LogsOptionsBuilder::default()
            .stdout(true)
            .stderr(true)
            .tail(&tail.map_or_else(|| "100".to_string(), |n| n.to_string()))
            .build();

        let stream = self.client.logs(&container, Some(options));
        let chunks: Vec<_> = stream
            .try_collect()
            .await
            .with_context(|| format!("failed to fetch logs for container '{container}'"))?;

        Ok(chunks
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(""))
    }

    fn provider_name(&self) -> &'static str {
        "Generic Docker"
    }

    fn max_branch_name_length(&self) -> usize {
        255
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ContainerStatus {
    NotFound,
    Running,
    Paused,
    Exited,
    Other(String),
}

fn sanitize(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            output.push(ch.to_ascii_lowercase());
        } else {
            output.push('-');
        }
    }
    while output.contains("--") {
        output = output.replace("--", "-");
    }
    let trimmed = output.trim_matches('-').to_string();
    if trimmed.is_empty() {
        return "service".to_string();
    }
    trimmed
}

async fn pick_available_port(client: &Docker, start_port: u16) -> anyhow::Result<u16> {
    let options = ListContainersOptions {
        all: false,
        ..Default::default()
    };

    let mut docker_ports = std::collections::HashSet::new();
    if let Ok(containers) = client.list_containers(Some(options)).await {
        for container in containers {
            if let Some(port_list) = container.ports {
                for port in port_list {
                    if let Some(public_port) = port.public_port {
                        docker_ports.insert(public_port);
                    }
                }
            }
        }
    }

    let mut port = start_port;
    for _ in 0..1000 {
        if !docker_ports.contains(&port) {
            if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                drop(listener);
                return Ok(port);
            }
        }
        port = port.saturating_add(1);
        if port == u16::MAX {
            break;
        }
    }

    Err(anyhow!(
        "failed to find available port starting from {start_port}"
    ))
}

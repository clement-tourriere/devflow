//! Shared per-workspace Docker engine backend.
//!
//! [`LocalEngineBackend`] implements the container lifecycle shared by the
//! per-workspace ClickHouse and MySQL providers: one bind-mounted Docker
//! container per workspace, named `devflow-{project}-{service}-{workspace}`,
//! with data stored under `data_root/{service_name}/{workspace_name}/` and
//! cloned from the parent workspace via Copy-on-Write when branching.
//!
//! Everything engine-specific — image, env vars, port layout, readiness
//! probe, connection info — is supplied by a [`LocalEngineSpec`]
//! implementation. The concrete providers are thin type aliases:
//! `LocalEngineBackend<ClickHouseEngine>` / `LocalEngineBackend<MySQLEngine>`.
//!
//! NOTE: container names, labels, env vars, mounts and connection strings
//! produced here must stay byte-identical to the pre-refactor providers —
//! existing user containers and data depend on them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use bollard::models::{ContainerCreateBody, HostConfig, PortBinding, PortMap};
use bollard::query_parameters::{
    CreateContainerOptions, RemoveContainerOptions, StopContainerOptions,
};
use bollard::Docker;
use chrono::Utc;
use tokio::time::{sleep, Instant};

use crate::config::DockerCustomSettings;
use crate::services::{
    local_docker::{
        bounded_container_name, collect_container_logs, expand_home, inspect_container_status,
        list_managed_service_containers, pick_available_port, pick_available_port_pair,
        sanitize_name_component, service_workspace_prefix, ContainerStatus,
    },
    shared::container::{ensure_image, exec_check},
    ConnectionInfo, DoctorCheck, DoctorReport, ProjectInfo, ServiceCapabilities, ServiceProvider,
    WorkspaceInfo,
};

/// How an engine's container ports are published on the host.
#[derive(Debug, Clone, Copy)]
pub enum PortLayout {
    /// One container port published on one picked host port.
    Single { container_port: u16 },
    /// Two container ports published on two consecutive host ports: the
    /// primary gets a picked host port `p` (allocated so that `p + 1` is
    /// free as well), the secondary gets `p + 1`.
    ConsecutivePair {
        primary_container_port: u16,
        secondary_container_port: u16,
    },
}

impl PortLayout {
    /// The container port whose host binding is reported in connection info.
    fn primary_container_port(&self) -> u16 {
        match *self {
            PortLayout::Single { container_port } => container_port,
            PortLayout::ConsecutivePair {
                primary_container_port,
                ..
            } => primary_container_port,
        }
    }
}

/// Engine-specific behavior for [`LocalEngineBackend`].
///
/// Implementations must keep their output byte-compatible with the
/// pre-refactor providers (labels, env vars, URLs, error wording), because
/// existing user containers and data depend on it.
pub trait LocalEngineSpec: Send + Sync + 'static {
    /// Engine kind: the `devflow.service-type` label value and the default
    /// `data_root` directory name (e.g. `"clickhouse"`, `"mysql"`).
    fn kind(&self) -> &'static str;
    /// Human-facing engine name used in error messages (e.g. `"ClickHouse"`).
    fn display_name(&self) -> &'static str;
    /// Value reported by [`ServiceProvider::provider_name`].
    fn provider_name(&self) -> &'static str;
    /// Docker image to run.
    fn image(&self) -> &str;
    /// Start of the host port range to allocate from; also the fallback port
    /// reported when a container has no inspectable binding.
    fn port_range_start(&self) -> u16;
    /// Container-internal directory the engine stores its data in
    /// (bind-mount target, e.g. `/var/lib/mysql`).
    fn data_mount_path(&self) -> &'static str;
    /// How container ports map onto host ports.
    fn port_layout(&self) -> PortLayout;
    /// Environment variables for the container, as `KEY=value` pairs.
    fn env(&self) -> Vec<String>;
    /// Command exec'd inside the container to probe readiness.
    fn readiness_command(&self) -> Vec<String>;
    /// Readiness timeout used by `create_workspace` when it restarts an
    /// already-existing stopped container. Preserved per engine: the
    /// original providers diverged here (ClickHouse 60s, MySQL 120s).
    fn restart_ready_timeout(&self) -> Duration;
    /// Connection info for a workspace whose primary container port is
    /// published on `host_port`.
    fn connection_info(&self, host_port: u16) -> ConnectionInfo;
}

/// A `127.0.0.1` host binding for one published port.
fn host_binding(host_port: u16) -> Option<Vec<PortBinding>> {
    Some(vec![PortBinding {
        host_ip: Some("127.0.0.1".to_string()),
        host_port: Some(host_port.to_string()),
    }])
}

/// Shared per-workspace Docker backend: manages one engine container per
/// workspace, parameterized by a [`LocalEngineSpec`].
pub struct LocalEngineBackend<E: LocalEngineSpec> {
    project_name: String,
    service_name: String,
    data_root: PathBuf,
    client: Docker,
    docker_settings: DockerCustomSettings,
    engine: E,
}

impl<E: LocalEngineSpec> LocalEngineBackend<E> {
    /// Connect to Docker and assemble a backend around `engine`.
    ///
    /// `data_root` overrides the storage root (with `~` expansion); it
    /// defaults to `dirs::data_local_dir()/devflow/{kind}`.
    pub fn with_engine(
        project_name: &str,
        service_name: &str,
        data_root: Option<&str>,
        docker_settings: Option<&DockerCustomSettings>,
        engine: E,
    ) -> anyhow::Result<Self> {
        let client =
            Docker::connect_with_local_defaults().context("Failed to connect to Docker daemon. Is Docker installed and running? Check with: docker info")?;

        let data_root = if let Some(root) = data_root {
            PathBuf::from(expand_home(root))
        } else {
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("devflow")
                .join(engine.kind())
        };

        Ok(Self {
            project_name: project_name.to_string(),
            service_name: service_name.to_string(),
            data_root,
            client,
            docker_settings: docker_settings.cloned().unwrap_or_default(),
            engine,
        })
    }

    fn container_name(&self, workspace_name: &str) -> String {
        let raw = format!(
            "devflow-{}-{}-{}",
            sanitize_name_component(&self.project_name),
            sanitize_name_component(&self.service_name),
            sanitize_name_component(workspace_name)
        );
        bounded_container_name(&raw)
    }

    fn workspace_data_dir(&self, workspace_name: &str) -> PathBuf {
        self.data_root
            .join(&self.service_name)
            .join(sanitize_name_component(workspace_name))
    }

    async fn container_status(&self, container_name: &str) -> anyhow::Result<ContainerStatus> {
        inspect_container_status(&self.client, container_name).await
    }

    async fn pick_port(&self) -> anyhow::Result<u16> {
        match self.engine.port_layout() {
            PortLayout::Single { .. } => {
                pick_available_port(&self.client, self.engine.port_range_start()).await
            }
            PortLayout::ConsecutivePair { .. } => {
                pick_available_port_pair(&self.client, self.engine.port_range_start()).await
            }
        }
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

        // A stopped container has empty live network settings, but its
        // configured bindings persist in HostConfig — report the port it
        // WILL bind instead of falling back to a made-up default.
        if let Some(bindings) = info.host_config.and_then(|hc| hc.port_bindings) {
            if let Some(Some(bindings)) = bindings.get(container_port) {
                for binding in bindings {
                    if let Some(ref host_port) = binding.host_port {
                        if let Ok(port) = host_port.parse::<u16>() {
                            return Ok(Some(port));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    /// Bring an existing container to Running and wait for engine readiness.
    /// Docker rejects `start` on a paused container, so paused resumes via
    /// `unpause` instead.
    async fn resume_container(
        &self,
        container_name: &str,
        status: &ContainerStatus,
    ) -> anyhow::Result<()> {
        if matches!(status, ContainerStatus::Paused) {
            self.client
                .unpause_container(container_name)
                .await
                .with_context(|| format!("failed to unpause container '{container_name}'"))?;
        } else {
            self.client
                .start_container(
                    container_name,
                    None::<bollard::query_parameters::StartContainerOptions>,
                )
                .await
                .with_context(|| format!("failed to start container '{container_name}'"))?;
        }
        self.wait_ready(container_name, self.engine.restart_ready_timeout())
            .await
    }

    async fn create_and_start(
        &self,
        container_name: &str,
        workspace_name: &str,
        host_port: u16,
    ) -> anyhow::Result<()> {
        ensure_image(&self.client, self.engine.image()).await?;

        let data_dir = self.workspace_data_dir(workspace_name);
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("failed to create data dir: {}", data_dir.display()))?;

        let mut port_bindings: PortMap = HashMap::new();
        match self.engine.port_layout() {
            PortLayout::Single { container_port } => {
                port_bindings.insert(format!("{container_port}/tcp"), host_binding(host_port));
            }
            PortLayout::ConsecutivePair {
                primary_container_port,
                secondary_container_port,
            } => {
                port_bindings.insert(
                    format!("{primary_container_port}/tcp"),
                    host_binding(host_port),
                );
                // The secondary protocol uses the next host port.
                port_bindings.insert(
                    format!("{secondary_container_port}/tcp"),
                    host_binding(host_port + 1),
                );
            }
        }

        let mount = format!("{}:{}", data_dir.display(), self.engine.data_mount_path());

        let env = self.engine.env();

        let mut labels = HashMap::new();
        labels.insert("devflow.managed".to_string(), "true".to_string());
        labels.insert("devflow.project".to_string(), self.project_name.clone());
        labels.insert("devflow.service".to_string(), self.service_name.clone());
        labels.insert(
            "devflow.service-type".to_string(),
            self.engine.kind().to_string(),
        );
        labels.insert("devflow.workspace".to_string(), workspace_name.to_string());

        let mut host_config = HostConfig {
            binds: Some(vec![mount]),
            port_bindings: Some(port_bindings),
            ..Default::default()
        };

        let mut config = ContainerCreateBody {
            image: Some(self.engine.image().to_string()),
            env: Some(env),
            labels: Some(labels),
            ..Default::default()
        };

        if !self.docker_settings.is_empty() {
            crate::docker::settings::apply_custom_settings(
                &mut config,
                &mut host_config,
                &self.docker_settings,
            );
        }

        config.host_config = Some(host_config);

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
        let probe = self.engine.readiness_command();
        let probe_args: Vec<&str> = probe.iter().map(String::as_str).collect();

        loop {
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "timed out waiting for {} readiness in '{container_name}'",
                    self.engine.display_name()
                ));
            }

            match self.container_status(container_name).await? {
                ContainerStatus::NotFound => {
                    return Err(anyhow!("container '{container_name}' does not exist"));
                }
                ContainerStatus::Running
                    if exec_check(&self.client, container_name, &probe_args).await =>
                {
                    return Ok(());
                }
                _ => {}
            }

            sleep(Duration::from_millis(500)).await;
        }
    }

    async fn list_managed_containers(&self) -> anyhow::Result<Vec<(String, String, bool)>> {
        let prefix = service_workspace_prefix(&self.project_name, &self.service_name);
        list_managed_service_containers(
            &self.client,
            &self.project_name,
            &self.service_name,
            &prefix,
        )
        .await
    }
}

#[async_trait]
impl<E: LocalEngineSpec> ServiceProvider for LocalEngineBackend<E> {
    async fn create_workspace(
        &self,
        workspace_name: &str,
        from_workspace: Option<&str>,
    ) -> anyhow::Result<WorkspaceInfo> {
        let container_name = self.container_name(workspace_name);

        match self.container_status(&container_name).await? {
            ContainerStatus::Running => {
                return Ok(WorkspaceInfo {
                    name: workspace_name.to_string(),
                    created_at: Some(Utc::now()),
                    parent_workspace: from_workspace.map(|s| s.to_string()),
                    database_name: container_name,
                    state: Some("running".to_string()),
                });
            }
            status @ (ContainerStatus::Exited | ContainerStatus::Paused) => {
                self.resume_container(&container_name, &status).await?;

                return Ok(WorkspaceInfo {
                    name: workspace_name.to_string(),
                    created_at: Some(Utc::now()),
                    parent_workspace: from_workspace.map(|s| s.to_string()),
                    database_name: container_name,
                    state: Some("running".to_string()),
                });
            }
            ContainerStatus::NotFound | ContainerStatus::Other(_) => {}
        }

        // Clone data from parent workspace if specified
        if let Some(parent_name) = from_workspace {
            let parent_container = self.container_name(parent_name);
            let parent_data_dir = self.workspace_data_dir(parent_name);
            let new_data_dir = self.workspace_data_dir(workspace_name);

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

        let host_port = self.pick_port().await?;
        self.create_and_start(&container_name, workspace_name, host_port)
            .await?;
        self.wait_ready(&container_name, Duration::from_secs(120))
            .await?;

        Ok(WorkspaceInfo {
            name: workspace_name.to_string(),
            created_at: Some(Utc::now()),
            parent_workspace: from_workspace.map(|s| s.to_string()),
            database_name: container_name,
            state: Some("running".to_string()),
        })
    }

    async fn delete_workspace(&self, workspace_name: &str) -> anyhow::Result<()> {
        let container_name = self.container_name(workspace_name);

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
        let data_dir = self.workspace_data_dir(workspace_name);
        if data_dir.exists() {
            std::fs::remove_dir_all(&data_dir)
                .with_context(|| format!("failed to remove data dir: {}", data_dir.display()))?;
        }

        Ok(())
    }

    async fn list_workspaces(&self) -> anyhow::Result<Vec<WorkspaceInfo>> {
        let containers = self.list_managed_containers().await?;
        Ok(containers
            .into_iter()
            .map(|(workspace, container_name, is_running)| WorkspaceInfo {
                name: workspace,
                created_at: None,
                parent_workspace: None,
                database_name: container_name,
                state: Some(if is_running { "running" } else { "stopped" }.to_string()),
            })
            .collect())
    }

    async fn workspace_exists(&self, workspace_name: &str) -> anyhow::Result<bool> {
        let container_name = self.container_name(workspace_name);
        Ok(!matches!(
            self.container_status(&container_name).await?,
            ContainerStatus::NotFound
        ))
    }

    async fn switch_to_branch(&self, workspace_name: &str) -> anyhow::Result<WorkspaceInfo> {
        let container_name = self.container_name(workspace_name);

        match self.container_status(&container_name).await? {
            ContainerStatus::Running => {}
            status @ (ContainerStatus::Exited
            | ContainerStatus::Paused
            | ContainerStatus::Other(_)) => {
                self.resume_container(&container_name, &status).await?;
            }
            ContainerStatus::NotFound => {
                return Err(anyhow!(
                    "no {} container for workspace '{workspace_name}' on service '{}'",
                    self.engine.display_name(),
                    self.service_name
                ));
            }
        }

        Ok(WorkspaceInfo {
            name: workspace_name.to_string(),
            created_at: None,
            parent_workspace: None,
            database_name: container_name,
            state: Some("running".to_string()),
        })
    }

    async fn get_connection_info(&self, workspace_name: &str) -> anyhow::Result<ConnectionInfo> {
        let container_name = self.container_name(workspace_name);
        let primary_port = self.engine.port_layout().primary_container_port();
        let host_port = self
            .get_container_port(&container_name, &format!("{primary_port}/tcp"))
            .await?
            .unwrap_or(self.engine.port_range_start());

        Ok(self.engine.connection_info(host_port))
    }

    fn supports_lifecycle(&self) -> bool {
        true
    }

    async fn reset_workspace(&self, workspace_name: &str) -> anyhow::Result<()> {
        // Honest error instead of the trait default's silent no-op: these
        // backends have no parent snapshot to reset from.
        anyhow::bail!(
            "reset is not implemented for {} local containers; delete and re-create \
             the workspace to reset '{workspace_name}'",
            self.engine.display_name()
        )
    }

    async fn start_workspace(&self, workspace_name: &str) -> anyhow::Result<()> {
        let container_name = self.container_name(workspace_name);
        match self.container_status(&container_name).await? {
            ContainerStatus::Running => Ok(()),
            ContainerStatus::NotFound => Err(anyhow!(
                "no {} container for workspace '{workspace_name}'",
                self.engine.display_name()
            )),
            status => self.resume_container(&container_name, &status).await,
        }
    }

    async fn stop_workspace(&self, workspace_name: &str) -> anyhow::Result<()> {
        let container_name = self.container_name(workspace_name);
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
        let names: Vec<String> = containers
            .into_iter()
            .map(|(workspace, _, _)| workspace)
            .collect();
        Ok(Some((self.service_name.clone(), names)))
    }

    async fn destroy_project(&self) -> anyhow::Result<Vec<String>> {
        let containers = self.list_managed_containers().await?;
        let mut deleted = Vec::new();

        for (workspace_name, container_name, _) in &containers {
            let options = RemoveContainerOptions {
                force: true,
                ..Default::default()
            };
            match self
                .client
                .remove_container(container_name, Some(options))
                .await
            {
                Ok(()) => deleted.push(workspace_name.clone()),
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

        let image_available = self.client.inspect_image(self.engine.image()).await.is_ok();
        checks.push(DoctorCheck {
            name: format!("Image: {}", self.engine.image()),
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
            image: Some(self.engine.image().to_string()),
        })
    }

    async fn logs(&self, workspace_name: &str, tail: Option<usize>) -> anyhow::Result<String> {
        let container = self.container_name(workspace_name);
        collect_container_logs(&self.client, &container, tail).await
    }

    fn provider_name(&self) -> &'static str {
        self.engine.provider_name()
    }

    fn capabilities(&self) -> ServiceCapabilities {
        ServiceCapabilities {
            lifecycle: true,
            logs: true,
            destroy_project: true,
            cleanup: true,
            seed_from_source: false,
            template_from_time: false,
            max_workspace_name_length: 255,
        }
    }

    fn max_workspace_name_length(&self) -> usize {
        255
    }
}

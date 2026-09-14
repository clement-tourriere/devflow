use std::collections::HashMap;
use std::{path::PathBuf, time::Duration};

use crate::config::DockerCustomSettings;

use anyhow::{anyhow, Context};
use bollard::models::{
    ContainerCreateBody, ContainerStateStatusEnum, HostConfig, PortBinding, PortMap,
};
use bollard::query_parameters::{
    CreateContainerOptions, LogsOptionsBuilder, RemoveContainerOptions, StopContainerOptions,
};
use bollard::Docker;
use futures_util::TryStreamExt;
use tokio::time::{sleep, Instant};

const PGDATA_CONTAINER_PATH: &str = "/var/lib/postgresql/data";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerStatus {
    NotFound,
    Running,
    Paused,
    Exited,
    Other(String),
}

#[derive(Debug, Clone)]
pub struct ReserveWorkspaceSpec {
    pub project_name: String,
    pub service_name: String,
    pub workspace_name: String,
}

#[derive(Debug, Clone)]
pub struct ReservedWorkspaceRuntime {
    pub container_name: String,
}

#[derive(Debug, Clone)]
pub struct StartWorkspaceSpec {
    pub image: String,
    pub container_name: String,
    pub data_dir: PathBuf,
    pub port: u16,
    pub pg_user: String,
    pub pg_password: String,
    pub pg_db: String,
    pub project_name: String,
    pub service_name: String,
    pub workspace_name: String,
    pub docker_settings: DockerCustomSettings,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DockerDoctorResult {
    pub available: bool,
    pub detail: String,
    pub version: Option<String>,
}

fn workspace_config(spec: &StartWorkspaceSpec) -> ContainerCreateBody {
    // Create and start a new container
    let mount = format!("{}:{PGDATA_CONTAINER_PATH}", spec.data_dir.display());

    let mut port_bindings: PortMap = HashMap::new();
    port_bindings.insert(
        "5432/tcp".to_string(),
        Some(vec![PortBinding {
            host_ip: Some("127.0.0.1".to_string()),
            host_port: Some(spec.port.to_string()),
        }]),
    );

    let mut labels = HashMap::new();
    labels.insert("devflow.managed".to_string(), "true".to_string());
    labels.insert("devflow.project".to_string(), spec.project_name.clone());
    labels.insert("devflow.service".to_string(), spec.service_name.clone());
    labels.insert("devflow.service-type".to_string(), "postgres".to_string());
    labels.insert("devflow.workspace".to_string(), spec.workspace_name.clone());

    let mut host_config = HostConfig {
        binds: Some(vec![mount]),
        port_bindings: Some(port_bindings),
        ..Default::default()
    };

    let mut config = ContainerCreateBody {
        image: Some(spec.image.clone()),
        user: get_host_uid_gid(),
        env: Some(vec![
            format!("POSTGRES_USER={}", spec.pg_user),
            format!("POSTGRES_PASSWORD={}", spec.pg_password),
            format!("POSTGRES_DB={}", spec.pg_db),
            // postgres:18+ moved the default PGDATA to a version-scoped
            // directory; pin it explicitly so the server always uses the
            // bind-mounted path regardless of image version.
            format!("PGDATA={PGDATA_CONTAINER_PATH}"),
        ]),
        labels: Some(labels),
        ..Default::default()
    };

    if !spec.docker_settings.is_empty() {
        crate::docker::settings::apply_custom_settings(
            &mut config,
            &mut host_config,
            &spec.docker_settings,
        );
    }

    config.host_config = Some(host_config);

    config
}

pub struct DockerRuntime {
    client: Docker,
}

impl DockerRuntime {
    pub fn new() -> anyhow::Result<Self> {
        let client =
            Docker::connect_with_local_defaults().context("Failed to connect to Docker daemon. Is Docker installed and running? Check with: docker info")?;
        Ok(Self { client })
    }

    pub fn client(&self) -> &Docker {
        &self.client
    }

    pub async fn doctor(&self) -> DockerDoctorResult {
        match self.client.version().await {
            Ok(info) => {
                let version = info.version.unwrap_or_default();
                DockerDoctorResult {
                    available: true,
                    detail: "Docker engine reachable".to_string(),
                    version: Some(version),
                }
            }
            Err(err) => DockerDoctorResult {
                available: false,
                detail: format!(
                    "Docker engine unreachable: {err}. Is Docker running? Try: docker info"
                ),
                version: None,
            },
        }
    }

    pub async fn reserve_workspace(
        &self,
        spec: &ReserveWorkspaceSpec,
    ) -> anyhow::Result<ReservedWorkspaceRuntime> {
        let container_name = format!(
            "devflow-pg-{}",
            &crate::services::ownership::identity_digest(&[
                &spec.project_name,
                &spec.service_name,
                &spec.workspace_name,
            ])[..48]
        );

        Ok(ReservedWorkspaceRuntime { container_name })
    }

    pub async fn ensure_image(&self, image: &str) -> anyhow::Result<()> {
        crate::services::shared::container::ensure_image(&self.client, image).await
    }

    pub async fn container_status(&self, container_name: &str) -> anyhow::Result<ContainerStatus> {
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

    /// Inspect the exact engine, ownership and runtime settings without changes.
    pub async fn verify_workspace(&self, spec: &StartWorkspaceSpec) -> anyhow::Result<()> {
        let config = workspace_config(spec);
        let actual = self
            .client
            .inspect_container(&spec.container_name, None)
            .await?;
        let image = self
            .client
            .inspect_image(&spec.image)
            .await?
            .id
            .context("Docker image identity missing")?;
        anyhow::ensure!(actual.image.as_ref() == Some(&image), "Container '{}' uses a different engine image; stop and remove that container before retrying (keep its data directory)", spec.container_name);
        let existing = actual.config.context("Container configuration missing")?;
        let actual_env = existing.env.unwrap_or_default();
        for expected in config.env.as_ref().into_iter().flatten() {
            anyhow::ensure!(actual_env.contains(expected), "Container '{}' has different database environment; recreate its container while retaining data", spec.container_name);
        }
        for (key, value) in config.labels.as_ref().into_iter().flatten() {
            anyhow::ensure!(
                existing.labels.as_ref().and_then(|labels| labels.get(key)) == Some(value),
                "Container '{}' ownership label '{}' differs",
                spec.container_name,
                key
            );
        }
        if let Some(user) = &config.user {
            anyhow::ensure!(
                existing.user.as_ref() == Some(user),
                "Container user differs from workspace specification"
            );
        }
        let expected = serde_json::to_value(&config.host_config)?;
        let actual = serde_json::to_value(actual.host_config)?;
        for (key, value) in expected.as_object().context("Host configuration missing")? {
            if !value.is_null() {
                anyhow::ensure!(actual.get(key) == Some(value), "Container '{}' has a different {} setting; recreate its container while retaining data", spec.container_name, key);
            }
        }
        Ok(())
    }

    pub async fn start_workspace(&self, spec: &StartWorkspaceSpec) -> anyhow::Result<()> {
        self.ensure_image(&spec.image).await?;

        let config = workspace_config(spec);

        let status = self.container_status(&spec.container_name).await?;
        if status != ContainerStatus::NotFound {
            self.verify_workspace(spec).await?;
        }
        match status {
            ContainerStatus::Running => return Ok(()),
            ContainerStatus::Paused => {
                self.unpause_workspace(&spec.container_name).await?;
                return Ok(());
            }
            ContainerStatus::Exited | ContainerStatus::Other(_) => {
                self.client
                    .start_container(
                        &spec.container_name,
                        None::<bollard::query_parameters::StartContainerOptions>,
                    )
                    .await
                    .with_context(|| {
                        format!("failed to start container '{}'", spec.container_name)
                    })?;
                return Ok(());
            }
            ContainerStatus::NotFound => {}
        }

        let options = CreateContainerOptions {
            name: Some(spec.container_name.clone()),
            ..Default::default()
        };

        self.client
            .create_container(Some(options), config)
            .await
            .with_context(|| format!("failed to create container '{}'", spec.container_name))?;

        self.client
            .start_container(
                &spec.container_name,
                None::<bollard::query_parameters::StartContainerOptions>,
            )
            .await
            .with_context(|| format!("failed to start container '{}'", spec.container_name))?;

        Ok(())
    }

    pub async fn stop_workspace(&self, container_name: &str) -> anyhow::Result<()> {
        match self.container_status(container_name).await? {
            ContainerStatus::NotFound | ContainerStatus::Exited | ContainerStatus::Other(_) => {
                return Ok(())
            }
            ContainerStatus::Paused => {
                self.unpause_workspace(container_name).await?;
            }
            ContainerStatus::Running => {}
        }

        let options = StopContainerOptions {
            t: Some(20),
            ..Default::default()
        };

        self.client
            .stop_container(container_name, Some(options))
            .await
            .with_context(|| format!("failed to stop container '{container_name}'"))?;

        Ok(())
    }

    /// Gracefully stop a container before cloning its data dir.
    ///
    /// Uses a generous timeout so postgres can complete its shutdown
    /// checkpoint — a SIGKILL here would make the clone a crash-state copy,
    /// which is exactly what stopping (instead of pausing) is meant to avoid.
    /// Start an existing (stopped or paused) container without re-creating it.
    pub async fn unpause_workspace(&self, container_name: &str) -> anyhow::Result<()> {
        match self.container_status(container_name).await? {
            ContainerStatus::Paused => {}
            _ => return Ok(()),
        }

        self.client
            .unpause_container(container_name)
            .await
            .with_context(|| format!("failed to unpause container '{container_name}'"))?;

        Ok(())
    }

    pub async fn remove_branch(&self, container_name: &str) -> anyhow::Result<()> {
        if matches!(
            self.container_status(container_name).await?,
            ContainerStatus::NotFound
        ) {
            return Ok(());
        }

        let options = RemoveContainerOptions {
            force: true,
            ..Default::default()
        };

        self.client
            .remove_container(container_name, Some(options))
            .await
            .with_context(|| format!("failed to remove container '{container_name}'"))?;

        Ok(())
    }

    pub async fn container_logs(
        &self,
        container_name: &str,
        tail: Option<usize>,
    ) -> anyhow::Result<String> {
        let options = LogsOptionsBuilder::default()
            .stdout(true)
            .stderr(true)
            .tail(&tail.map_or_else(|| "100".to_string(), |n| n.to_string()))
            .build();

        let stream = self.client.logs(container_name, Some(options));
        let chunks: Vec<_> = stream
            .try_collect()
            .await
            .with_context(|| format!("failed to fetch logs for container '{container_name}'"))?;

        let output = chunks
            .iter()
            .map(|chunk| chunk.to_string())
            .collect::<Vec<_>>()
            .join("");

        Ok(output)
    }

    pub async fn wait_ready(
        &self,
        container_name: &str,
        pg_user: &str,
        pg_db: &str,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;

        loop {
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "timed out waiting for postgres readiness in '{container_name}'"
                ));
            }

            match self.container_status(container_name).await? {
                ContainerStatus::NotFound => {
                    return Err(anyhow!("container '{container_name}' does not exist"));
                }
                ContainerStatus::Running
                    // Probe over TCP: during initdb the image's entrypoint runs
                    // a temporary server that only listens on the unix socket,
                    // so a socket-based pg_isready false-positives before the
                    // real server is up.
                    if self
                        .exec_check(
                            container_name,
                            &[
                                "pg_isready",
                                "-h",
                                "127.0.0.1",
                                "-p",
                                "5432",
                                "-U",
                                pg_user,
                                "-d",
                                pg_db,
                            ],
                        )
                        .await
                    => {
                        return Ok(());
                    }
                _ => {}
            }

            sleep(Duration::from_millis(500)).await;
        }
    }

    /// Run a command inside a container and return true if it exits successfully.
    async fn exec_check(&self, container_name: &str, cmd: &[&str]) -> bool {
        crate::services::shared::container::exec_check(&self.client, container_name, cmd).await
    }
}

/// Returns the current host user's UID:GID as a string (e.g. "1000:1000").
/// This ensures files created inside the container are owned by the host user,
/// allowing `cp` operations on the bind-mounted pgdata directory to succeed.
#[cfg(unix)]
fn get_host_uid_gid() -> Option<String> {
    Some(format!(
        "{}:{}",
        nix::unistd::getuid().as_raw(),
        nix::unistd::getgid().as_raw()
    ))
}

#[cfg(not(unix))]
fn get_host_uid_gid() -> Option<String> {
    None
}

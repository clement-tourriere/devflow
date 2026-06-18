//! Global-container lifecycle + exec helpers shared by the `shared` providers.
//!
//! Unlike the CoW backends (one container per workspace), these keep a single
//! long-lived container per engine and run logical-provisioning commands
//! inside it via `docker exec`.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bollard::exec::{StartExecOptions, StartExecResults};
use bollard::models::{ContainerCreateBody, ExecConfig, HostConfig, PortBinding, PortMap};
use bollard::query_parameters::{CreateContainerOptions, CreateImageOptions};
use bollard::Docker;
use futures_util::TryStreamExt;
use tokio::time::Instant;

use crate::services::local_docker::{inspect_container_status, ContainerStatus};

/// Specification for the single global container of an engine.
pub struct GlobalContainerSpec {
    pub container_name: String,
    pub image: String,
    /// Host port to publish.
    pub host_port: u16,
    /// Container port to publish from.
    pub container_port: u16,
    /// Optional extra port to publish (e.g. a web console). `(host, container)`.
    pub extra_port: Option<(u16, u16)>,
    /// Override the image's default command (e.g. RustFS needs the data path).
    pub cmd: Vec<String>,
    /// Environment variables (`KEY=value`).
    pub env: Vec<String>,
    /// Docker bind/volume mounts (`source:/container/path`). A named volume
    /// keeps the global engine's data across container recreation.
    pub binds: Vec<String>,
    /// Extra labels beyond the devflow-managed markers.
    pub labels: Vec<(String, String)>,
}

/// Captured result of a `docker exec`.
pub struct ExecOutput {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
}

impl ExecOutput {
    pub fn ok(&self) -> bool {
        self.exit_code == 0
    }
}

/// Connect to the Docker daemon using the environment's defaults.
pub fn connect() -> Result<Docker> {
    Docker::connect_with_defaults().context("Failed to connect to Docker")
}

/// Pull `image` if it isn't present locally.
pub async fn ensure_image(docker: &Docker, image: &str) -> Result<()> {
    if docker.inspect_image(image).await.is_ok() {
        return Ok(());
    }
    let (from_image, tag) = match image.rsplit_once(':') {
        Some((name, tag)) => (name.to_string(), Some(tag.to_string())),
        None => (image.to_string(), None),
    };
    let options = CreateImageOptions {
        from_image: Some(from_image),
        tag,
        ..Default::default()
    };
    docker
        .create_image(Some(options), None, None)
        .try_collect::<Vec<_>>()
        .await
        .with_context(|| format!("failed to pull docker image '{image}'"))?;
    Ok(())
}

/// Ensure the global container exists and is running. Idempotent and tolerant
/// of races between concurrent devflow processes (Docker 409 = already exists).
pub async fn ensure_running_container(docker: &Docker, spec: &GlobalContainerSpec) -> Result<()> {
    match inspect_container_status(docker, &spec.container_name).await? {
        ContainerStatus::Running => return Ok(()),
        ContainerStatus::Paused => {
            docker
                .unpause_container(&spec.container_name)
                .await
                .with_context(|| format!("failed to unpause '{}'", spec.container_name))?;
            return Ok(());
        }
        ContainerStatus::Exited | ContainerStatus::Other(_) => {
            return start_existing(docker, &spec.container_name).await;
        }
        ContainerStatus::NotFound => {}
    }

    ensure_image(docker, &spec.image).await?;

    let mut port_bindings: PortMap = HashMap::new();
    // Bind to loopback only — dev databases should not be reachable from the
    // LAN. Users who genuinely need external access can publish ports manually
    // via `docker` or a custom `type: generic` service config.
    port_bindings.insert(
        format!("{}/tcp", spec.container_port),
        Some(vec![PortBinding {
            host_ip: Some("127.0.0.1".to_string()),
            host_port: Some(spec.host_port.to_string()),
        }]),
    );
    if let Some((host, container)) = spec.extra_port {
        port_bindings.insert(
            format!("{container}/tcp"),
            Some(vec![PortBinding {
                host_ip: Some("127.0.0.1".to_string()),
                host_port: Some(host.to_string()),
            }]),
        );
    }

    let mut labels = HashMap::new();
    labels.insert("devflow.managed".to_string(), "true".to_string());
    labels.insert("devflow.scope".to_string(), "global".to_string());
    for (k, v) in &spec.labels {
        labels.insert(k.clone(), v.clone());
    }

    let host_config = HostConfig {
        port_bindings: Some(port_bindings),
        binds: if spec.binds.is_empty() {
            None
        } else {
            Some(spec.binds.clone())
        },
        restart_policy: Some(bollard::models::RestartPolicy {
            name: Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED),
            ..Default::default()
        }),
        ..Default::default()
    };

    let config = ContainerCreateBody {
        image: Some(spec.image.clone()),
        cmd: if spec.cmd.is_empty() {
            None
        } else {
            Some(spec.cmd.clone())
        },
        env: Some(spec.env.clone()),
        labels: Some(labels),
        host_config: Some(host_config),
        ..Default::default()
    };

    let options = CreateContainerOptions {
        name: Some(spec.container_name.clone()),
        ..Default::default()
    };

    match docker.create_container(Some(options), config).await {
        Ok(_) => {}
        // 409: another process created it first — fine, fall through to start.
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 409, ..
        }) => {
            log::debug!(
                "Global container '{}' already exists (concurrent create)",
                spec.container_name
            );
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "failed to create global container '{}'",
                    spec.container_name
                )
            });
        }
    }

    start_existing(docker, &spec.container_name).await
}

async fn start_existing(docker: &Docker, container_name: &str) -> Result<()> {
    match docker
        .start_container(
            container_name,
            None::<bollard::query_parameters::StartContainerOptions>,
        )
        .await
    {
        Ok(_) => Ok(()),
        // 304: already running.
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 304, ..
        }) => Ok(()),
        Err(e) => Err(e).with_context(|| format!("failed to start container '{container_name}'")),
    }
}

/// Run a command inside the container and capture stdout/stderr/exit code.
pub async fn exec_capture(
    docker: &Docker,
    container_name: &str,
    cmd: &[&str],
    env: Option<Vec<String>>,
) -> Result<ExecOutput> {
    let config = ExecConfig {
        cmd: Some(cmd.iter().map(|s| s.to_string()).collect()),
        env,
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        ..Default::default()
    };

    let exec = docker
        .create_exec(container_name, config)
        .await
        .context("failed to create exec instance")?;

    let start_opts = Some(StartExecOptions {
        detach: false,
        ..Default::default()
    });

    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    match docker.start_exec(&exec.id, start_opts).await? {
        StartExecResults::Attached { mut output, .. } => {
            while let Some(msg) = output.try_next().await? {
                match msg {
                    bollard::container::LogOutput::StdOut { message } => {
                        stdout_buf.extend_from_slice(&message)
                    }
                    bollard::container::LogOutput::StdErr { message } => {
                        stderr_buf.extend_from_slice(&message)
                    }
                    _ => {}
                }
            }
        }
        StartExecResults::Detached => {}
    }

    let inspect = docker.inspect_exec(&exec.id).await?;
    Ok(ExecOutput {
        exit_code: inspect.exit_code.unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout_buf).to_string(),
        stderr: String::from_utf8_lossy(&stderr_buf).to_string(),
    })
}

/// Wait until `pg_isready` succeeds inside the container (TCP loopback, so the
/// initdb temp-socket server can't false-positive).
pub async fn wait_ready_pg(
    docker: &Docker,
    container_name: &str,
    user: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "timed out waiting for postgres readiness in '{container_name}'"
            ));
        }
        if let Ok(out) = exec_capture(
            docker,
            container_name,
            &["pg_isready", "-h", "127.0.0.1", "-U", user],
            None,
        )
        .await
        {
            if out.ok() {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

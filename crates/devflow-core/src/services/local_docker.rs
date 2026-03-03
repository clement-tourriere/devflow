use anyhow::{anyhow, Context, Result};
use bollard::models::ContainerStateStatusEnum;
use bollard::query_parameters::{ListContainersOptions, LogsOptionsBuilder};
use bollard::Docker;
use futures_util::TryStreamExt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerStatus {
    NotFound,
    Running,
    Paused,
    Exited,
    Other(String),
}

pub fn sanitize_name_component(input: &str) -> String {
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

pub fn service_workspace_prefix(project_name: &str, service_name: &str) -> String {
    format!(
        "devflow-{}-{}-",
        sanitize_name_component(project_name),
        sanitize_name_component(service_name)
    )
}

pub async fn inspect_container_status(
    client: &Docker,
    container_name: &str,
) -> Result<ContainerStatus> {
    match client
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

pub async fn list_managed_service_containers(
    client: &Docker,
    service_name: &str,
    prefix: &str,
) -> Result<Vec<(String, String, bool)>> {
    let options = ListContainersOptions {
        all: true,
        ..Default::default()
    };

    let containers = client
        .list_containers(Some(options))
        .await
        .context("failed to list Docker containers")?;

    let mut result = Vec::new();
    for container in containers {
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
            .map(|v| v == service_name)
            .unwrap_or(false);

        if !is_managed || !is_our_service {
            continue;
        }

        if let Some(names) = &container.names {
            for name in names {
                let clean_name = name.trim_start_matches('/');
                if let Some(branch_part) = clean_name.strip_prefix(prefix) {
                    let is_running = container
                        .state
                        .as_ref()
                        .map(|s| matches!(s, bollard::models::ContainerSummaryStateEnum::RUNNING))
                        .unwrap_or(false);
                    result.push((branch_part.to_string(), clean_name.to_string(), is_running));
                }
            }
        }
    }

    Ok(result)
}

pub async fn collect_container_logs(
    client: &Docker,
    container_name: &str,
    tail: Option<usize>,
) -> Result<String> {
    let options = LogsOptionsBuilder::default()
        .stdout(true)
        .stderr(true)
        .tail(&tail.map_or_else(|| "100".to_string(), |n| n.to_string()))
        .build();

    let stream = client.logs(container_name, Some(options));
    let chunks: Vec<_> = stream
        .try_collect()
        .await
        .with_context(|| format!("failed to fetch logs for container '{container_name}'"))?;

    Ok(chunks
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(""))
}

pub async fn pick_available_port(client: &Docker, start_port: u16) -> Result<u16> {
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

pub async fn pick_available_port_pair(client: &Docker, start_port: u16) -> Result<u16> {
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
        if !docker_ports.contains(&port) && !docker_ports.contains(&(port + 1)) {
            if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                drop(listener);
                if let Ok(listener2) = tokio::net::TcpListener::bind(("127.0.0.1", port + 1)).await
                {
                    drop(listener2);
                    return Ok(port);
                }
            }
        }
        port = port.saturating_add(2);
        if port >= u16::MAX - 1 {
            break;
        }
    }

    Err(anyhow!(
        "failed to find two available consecutive ports starting from {start_port}"
    ))
}

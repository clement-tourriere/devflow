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

/// Expand a leading `~/` to the user's home directory.
pub fn expand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).display().to_string();
        }
    }
    path.to_string()
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

/// Enforce devflow's Docker-name bound without discarding the distinguishing
/// workspace suffix. Hashing the full name prevents long project/service
/// prefixes from collapsing otherwise distinct workspace containers.
/// Delegates to the one shared truncation scheme so container, database, and
/// bucket naming can never drift apart.
pub fn bounded_container_name(raw: &str) -> String {
    const MAX_LEN: usize = 128;
    crate::services::shared::naming::stable_truncate(raw, MAX_LEN, '-')
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
    project_name: &str,
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

        let Some((workspace, container_name)) = managed_container_identity(
            container.labels.as_ref(),
            container.names.as_deref().unwrap_or_default(),
            project_name,
            prefix,
        ) else {
            continue;
        };

        let is_running = container
            .state
            .as_ref()
            .map(|s| matches!(s, bollard::models::ContainerSummaryStateEnum::RUNNING))
            .unwrap_or(false);
        result.push((workspace, container_name, is_running));
    }

    Ok(result)
}

/// Resolve the exact workspace identity and usable Docker name for a managed
/// container. Current containers carry authoritative project/workspace labels;
/// name parsing remains only for containers created by older devflow releases.
fn managed_container_identity(
    labels: Option<&std::collections::HashMap<String, String>>,
    names: &[String],
    project_name: &str,
    legacy_prefix: &str,
) -> Option<(String, String)> {
    let labels = labels?;
    let labeled_project = labels
        .get("devflow.project")
        .filter(|value| !value.is_empty());

    if labeled_project.is_some_and(|project| project != project_name) {
        return None;
    }

    let labeled_workspace = labels
        .get("devflow.workspace")
        .filter(|workspace| !workspace.is_empty());

    if labeled_project.is_some() {
        let container_name = names.first()?.trim_start_matches('/').to_string();
        let workspace = labeled_workspace.cloned().or_else(|| {
            container_name
                .strip_prefix(legacy_prefix)
                .map(ToOwned::to_owned)
        })?;
        return Some((workspace, container_name));
    }

    // Without a project label, the old unbounded name prefix is the only safe
    // way to distinguish same-named services belonging to different projects.
    names.iter().find_map(|name| {
        let container_name = name.trim_start_matches('/');
        let legacy_workspace = container_name.strip_prefix(legacy_prefix)?;
        Some((
            labeled_workspace
                .cloned()
                .unwrap_or_else(|| legacy_workspace.to_string()),
            container_name.to_string(),
        ))
    })
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

/// Ports handed out by the `pick_available_port*` helpers in this process
/// whose containers may not be listening yet. Service providers run
/// concurrently during orchestration; between picking a port and starting the
/// container (data cloning happens in between) the port looks free to every
/// other picker, so a purely probe-based check would let two services choose
/// the same port.
fn claimed_ports() -> &'static std::sync::Mutex<std::collections::HashSet<u16>> {
    static CLAIMED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<u16>>> =
        std::sync::OnceLock::new();
    CLAIMED.get_or_init(Default::default)
}

/// Atomically claim all given ports. Returns false (claiming none) if any of
/// them was already claimed by another picker in this process.
pub(crate) fn try_claim_ports(ports: &[u16]) -> bool {
    let mut claimed = claimed_ports()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if ports.iter().any(|p| claimed.contains(p)) {
        return false;
    }
    claimed.extend(ports.iter().copied());
    true
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
                if try_claim_ports(&[port]) {
                    return Ok(port);
                }
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
                    if try_claim_ports(&[port, port + 1]) {
                        return Ok(port);
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_claim_ports_rejects_already_claimed() {
        // Use a high, unlikely-to-collide range: the registry is process-wide
        // and shared with other tests.
        assert!(try_claim_ports(&[64901, 64902]));
        assert!(!try_claim_ports(&[64902]));
        assert!(!try_claim_ports(&[64900, 64901]));
        assert!(try_claim_ports(&[64903]));
    }

    #[test]
    fn try_claim_ports_is_all_or_nothing() {
        assert!(try_claim_ports(&[64910]));
        // 64911 must remain claimable: the failed batch below must not
        // have claimed it on the way to rejecting 64910.
        assert!(!try_claim_ports(&[64911, 64910]));
        assert!(try_claim_ports(&[64911]));
    }

    #[test]
    fn bounded_container_names_retain_long_identity() {
        let prefix = format!("devflow-{}-service-", "project".repeat(20));
        let first = bounded_container_name(&format!("{prefix}workspace-a"));
        let second = bounded_container_name(&format!("{prefix}workspace-b"));

        assert_eq!(first.len(), 128);
        assert_eq!(second.len(), 128);
        assert_ne!(first, second);
        assert_eq!(first.rsplit('-').next().unwrap().len(), 12);
    }

    #[test]
    fn managed_container_uses_labels_for_bounded_names() {
        let project = "project".repeat(20);
        let prefix = service_workspace_prefix(&project, "database");
        let raw_name = format!("{prefix}feature-auth");
        let bounded_name = bounded_container_name(&raw_name);
        assert!(!bounded_name.starts_with(&prefix));

        let labels = std::collections::HashMap::from([
            ("devflow.project".to_string(), project.clone()),
            ("devflow.workspace".to_string(), "feature/auth".to_string()),
        ]);
        let names = vec![format!("/{bounded_name}")];

        assert_eq!(
            managed_container_identity(Some(&labels), &names, &project, &prefix),
            Some(("feature/auth".to_string(), bounded_name))
        );
    }

    #[test]
    fn managed_container_falls_back_to_legacy_name_parsing() {
        let prefix = service_workspace_prefix("legacy-project", "database");
        let labels = std::collections::HashMap::new();
        let names = vec![format!("/{prefix}feature-auth")];

        assert_eq!(
            managed_container_identity(Some(&labels), &names, "legacy-project", &prefix),
            Some(("feature-auth".to_string(), format!("{prefix}feature-auth")))
        );
    }

    #[test]
    fn managed_container_rejects_another_project_label() {
        let prefix = service_workspace_prefix("project-a", "database");
        let labels = std::collections::HashMap::from([
            ("devflow.project".to_string(), "project-b".to_string()),
            ("devflow.workspace".to_string(), "main".to_string()),
        ]);
        let names = vec![format!("/{prefix}main")];

        assert_eq!(
            managed_container_identity(Some(&labels), &names, "project-a", &prefix),
            None
        );
    }
}

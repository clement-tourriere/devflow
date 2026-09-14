use anyhow::{anyhow, Context, Result};
use bollard::models::ContainerStateStatusEnum;
use bollard::query_parameters::{ListContainersOptions, LogsOptionsBuilder};
use bollard::Docker;
use futures_util::TryStreamExt;

/// Keep the parent quiesced until copying and recovery finish, even if the
/// caller times out. A journal survives process termination for the next retry.
pub async fn quiesced_operation<T, F>(
    docker: Docker,
    parent: String,
    lease_path: std::path::PathBuf,
    readiness_command: Vec<String>,
    operation: F,
) -> Result<T>
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T>> + Send + 'static,
{
    tokio::spawn(async move {
        use crate::services::ownership::{atomic_write, OperationLease};
        use futures_util::FutureExt;
        let _lease = OperationLease::acquire(&lease_path).await?;
        let journal = lease_path.with_extension("recovery.json");
        if journal.exists() {
            let recorded: String = serde_json::from_slice(&std::fs::read(&journal)?)?;
            anyhow::ensure!(
                recorded == parent,
                "clone recovery owner does not match parent"
            );
            if inspect_container_status(&docker, &parent).await? != ContainerStatus::Running {
                docker
                    .start_container(
                        &parent,
                        None::<bollard::query_parameters::StartContainerOptions>,
                    )
                    .await
                    .context("failed to restore parent from interrupted clone")?;
            }
            wait_parent_ready(&docker, &parent, &readiness_command).await?;
            std::fs::remove_file(&journal)?;
        }
        let running = inspect_container_status(&docker, &parent).await? == ContainerStatus::Running;
        if running {
            // Docker running does not mean the entrypoint has exec'd the database.
            // Sending its stop signal during startup can otherwise be ignored.
            wait_parent_ready(&docker, &parent, &readiness_command).await?;
            atomic_write(&journal, &serde_json::to_vec(&parent)?)?;
        }
        let result = std::panic::AssertUnwindSafe(async {
            if running {
                docker
                    .stop_container(
                        &parent,
                        Some(bollard::query_parameters::StopContainerOptions {
                            t: Some(-1),
                            ..Default::default()
                        }),
                    )
                    .await
                    .context("failed to quiesce parent")?;
            }
            operation.await
        })
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Err(anyhow!("clone operation panicked")));
        if running {
            if inspect_container_status(&docker, &parent).await? != ContainerStatus::Running {
                docker
                    .start_container(
                        &parent,
                        None::<bollard::query_parameters::StartContainerOptions>,
                    )
                    .await
                    .context("failed to restart clone parent; recovery journal retained")?;
            }
            // Keep the lease and recovery intent until the service itself is ready,
            // including when the initiating caller has been cancelled.
            wait_parent_ready(&docker, &parent, &readiness_command).await?;
            std::fs::remove_file(&journal)?;
        }
        result
    })
    .await
    .context("clone/recovery task failed")?
}

async fn wait_parent_ready(docker: &Docker, parent: &str, command: &[String]) -> Result<()> {
    let args = command.iter().map(String::as_str).collect::<Vec<_>>();
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if crate::services::shared::container::exec_check(docker, parent, &args).await {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .with_context(|| format!("parent '{parent}' did not become ready; recovery intent retained"))?;
    Ok(())
}

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

/// Cross-process reservations held until Docker has published the ports.
/// Released automatically on failure, cancellation or process exit.
pub struct PortReservation {
    pub port: u16,
    _leases: Vec<super::ownership::OperationLease>,
}
impl PortReservation {
    fn try_acquire(root: &std::path::Path, ports: &[u16]) -> Result<Option<Self>> {
        let mut leases = Vec::new();
        for port in ports {
            let Some(lease) =
                super::ownership::OperationLease::try_acquire(&root.join(format!("{port}.lock")))?
            else {
                return Ok(None);
            };
            leases.push(lease);
        }
        Ok(Some(Self {
            port: ports[0],
            _leases: leases,
        }))
    }
}

pub async fn pick_available_port(client: &Docker, start_port: u16) -> Result<PortReservation> {
    reserve_ports(client, start_port, 1).await
}
pub async fn pick_available_port_pair(client: &Docker, start_port: u16) -> Result<PortReservation> {
    reserve_ports(client, start_port, 2).await
}
async fn reserve_ports(client: &Docker, start_port: u16, count: u16) -> Result<PortReservation> {
    anyhow::ensure!(start_port > 0, "port range must start above zero");
    let root = crate::paths::devflow_config_dir()?.join("port-reservations");
    let mut port = start_port;
    for _ in 0..1000 {
        let Some(last) = port.checked_add(count - 1) else {
            break;
        };
        let ports = (port..=last).collect::<Vec<_>>();
        if let Some(reservation) = PortReservation::try_acquire(&root, &ports)? {
            // Refresh after acquiring the lease: another process may have just
            // published a port and released its reservation.
            let containers = client
                .list_containers(Some(ListContainersOptions {
                    all: true,
                    ..Default::default()
                }))
                .await?;
            let published = containers
                .iter()
                .flat_map(|c| c.ports.iter().flatten())
                .filter_map(|p| p.public_port)
                .collect::<std::collections::HashSet<_>>();
            let mut listeners = Vec::new();
            if ports.iter().all(|p| !published.contains(p)) {
                for candidate in &ports {
                    match tokio::net::TcpListener::bind(("127.0.0.1", *candidate)).await {
                        Ok(listener) => listeners.push(listener),
                        Err(_) => break,
                    }
                }
                if listeners.len() == ports.len() {
                    return Ok(reservation);
                }
            }
        }
        let Some(next) = port.checked_add(count) else {
            break;
        };
        port = next;
    }
    Err(anyhow!(
        "failed to reserve {count} available port(s) starting at {start_port}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_are_atomic_and_released_after_failure() {
        let temp = tempfile::tempdir().unwrap();
        let occupied = PortReservation::try_acquire(temp.path(), &[64002])
            .unwrap()
            .unwrap();
        assert!(PortReservation::try_acquire(temp.path(), &[64001, 64002])
            .unwrap()
            .is_none());
        let first = PortReservation::try_acquire(temp.path(), &[64001])
            .unwrap()
            .unwrap();
        drop(first);
        drop(occupied);
        assert!(PortReservation::try_acquire(temp.path(), &[64001, 64002])
            .unwrap()
            .is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_different_process_cannot_take_a_reserved_port() {
        let temp = tempfile::tempdir().unwrap();
        let lease = PortReservation::try_acquire(temp.path(), &[64003])
            .unwrap()
            .unwrap();
        let probe = || {
            std::process::Command::new("/usr/bin/python3").args(["-c", "import fcntl,sys; f=open(sys.argv[1],'r+'); fcntl.flock(f,fcntl.LOCK_EX|fcntl.LOCK_NB)"]).arg(temp.path().join("64003.lock")).output().unwrap().status.success()
        };
        assert!(!probe());
        drop(lease);
        assert!(probe());
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
}

use anyhow::{Context, Result};
use bollard::models::ContainerInspectResponse;
use bollard::query_parameters::{EventsOptions, ListContainersOptions};
use bollard::Docker;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// A Docker container event (start/stop/die).
#[derive(Debug, Clone)]
pub struct ContainerEvent {
    pub action: String,
    pub container: ContainerInspectResponse,
}

/// Monitors Docker for container lifecycle events.
pub struct DockerMonitor {
    docker: Arc<Docker>,
}

impl DockerMonitor {
    /// Create a new Docker monitor.
    pub fn new() -> Result<Self> {
        let docker = Docker::connect_with_defaults().context("Failed to connect to Docker")?;
        Ok(Self {
            docker: Arc::new(docker),
        })
    }

    /// Start monitoring Docker events. Sends ContainerEvents to the channel.
    pub async fn start(
        &self,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
        events_tx: mpsc::Sender<ContainerEvent>,
    ) -> Result<()> {
        let mut filters = HashMap::new();
        filters.insert("type".to_string(), vec!["container".to_string()]);
        filters.insert(
            "event".to_string(),
            vec!["start".to_string(), "stop".to_string(), "die".to_string()],
        );

        let options = EventsOptions {
            filters: Some(filters),
            ..Default::default()
        };

        let mut events = self.docker.events(Some(options));
        let docker = self.docker.clone();

        loop {
            tokio::select! {
                event = events.next() => {
                    match event {
                        Some(Ok(event)) => {
                            let action = event.action.unwrap_or_default().to_string();
                            let container_id = event
                                .actor
                                .and_then(|a| a.id)
                                .unwrap_or_default();

                            if container_id.is_empty() {
                                continue;
                            }

                            match docker.inspect_container(&container_id, None).await {
                                Ok(info) => {
                                    log::info!(
                                        "Container event: {} {}",
                                        action,
                                        info.name.as_deref().unwrap_or(&container_id[..12])
                                    );
                                    let _ = events_tx.send(ContainerEvent {
                                        action,
                                        container: info,
                                    }).await;
                                }
                                Err(e) => {
                                    log::debug!("Failed to inspect container {}: {}", &container_id[..12.min(container_id.len())], e);
                                }
                            }
                        }
                        Some(Err(e)) => {
                            log::error!("Docker events error: {}", e);
                            break;
                        }
                        None => break,
                    }
                }
                _ = shutdown.changed() => {
                    log::info!("Docker monitor shutting down");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Get all currently running containers.
    pub async fn get_running_containers(&self) -> Result<Vec<ContainerInspectResponse>> {
        let options = ListContainersOptions {
            all: false,
            ..Default::default()
        };

        let containers = self
            .docker
            .list_containers(Some(options))
            .await
            .context("Failed to list containers")?;

        let mut results = Vec::new();
        for container in containers {
            if let Some(id) = container.id {
                match self.docker.inspect_container(&id, None).await {
                    Ok(info) => results.push(info),
                    Err(e) => log::debug!("Failed to inspect container: {}", e),
                }
            }
        }

        Ok(results)
    }

    /// Inspect a single container by ID.
    pub async fn inspect_container(&self, container_id: &str) -> Result<ContainerInspectResponse> {
        self.docker
            .inspect_container(container_id, None)
            .await
            .context("Failed to inspect container")
    }
}

pub mod docker;
pub mod model;
pub mod reconcile;
pub mod seed;
pub mod state;
pub mod storage;

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use uuid::Uuid;

use super::super::{
    ConnectionInfo, DoctorCheck, DoctorReport, ProjectInfo, ServiceCapabilities, ServiceProvider,
    WorkspaceInfo,
};
use crate::config::{Config, DockerCustomSettings, LocalServiceConfig};
use docker::{DockerRuntime, ReserveWorkspaceSpec, StartWorkspaceSpec};
use model::WorkspaceState;
use state::{NewProject, NewWorkspace, Store};
use storage::StorageCoordinator;

const DEFAULT_IMAGE: &str = "postgres:17";
const DEFAULT_PORT_RANGE_START: u16 = 55432;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);

pub struct LocalProvider {
    project_name: String,
    service_name: String,
    image: String,
    port_range_start: u16,
    pg_user: String,
    pg_password: String,
    pg_db: String,
    store: Mutex<Store>,
    runtime: DockerRuntime,
    storage: StorageCoordinator,
    data_root: PathBuf,
    /// Canonical filesystem path of the project directory (for orphan detection).
    project_path: Option<String>,
    docker_settings: DockerCustomSettings,
}

impl LocalProvider {
    pub async fn new(
        service_name: &str,
        config: &Config,
        local_config: Option<&LocalServiceConfig>,
        docker_settings: Option<&DockerCustomSettings>,
    ) -> Result<Self> {
        let image = local_config
            .and_then(|c| c.image.as_deref())
            .unwrap_or(DEFAULT_IMAGE)
            .to_string();

        let port_range_start = local_config
            .and_then(|c| c.port_range_start)
            .unwrap_or(DEFAULT_PORT_RANGE_START);

        let pg_user = local_config
            .and_then(|c| c.postgres_user.as_deref())
            .unwrap_or("postgres")
            .to_string();

        let pg_password = local_config
            .and_then(|c| c.postgres_password.as_deref())
            .unwrap_or("postgres")
            .to_string();

        let pg_db = local_config
            .and_then(|c| c.postgres_db.as_deref())
            .unwrap_or("postgres")
            .to_string();

        let data_root = if let Some(root) = local_config.and_then(|c| c.data_root.as_deref()) {
            let expanded = crate::services::local_docker::expand_home(root);
            PathBuf::from(expanded)
        } else {
            dirs::data_local_dir()
                .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
                .join("devflow")
        };

        // Ensure directories exist
        let projects_root = data_root.join("projects");
        tokio::fs::create_dir_all(&projects_root)
            .await
            .with_context(|| {
                format!(
                    "failed to create projects root: {}",
                    projects_root.display()
                )
            })?;

        let db_path = data_root.join("state.db");
        let store = Store::open(&db_path)
            .with_context(|| format!("failed to open state database: {}", db_path.display()))?;

        let runtime = DockerRuntime::new().context("failed to initialize Docker runtime")?;
        let storage = StorageCoordinator::new(projects_root.clone());

        let project_name = config.project_name();
        let service_name = service_name.to_string();

        // Capture the canonical MAIN-repo root for orphan detection, so a
        // worktree and its main repo resolve to the same project_path (and
        // don't ping-pong the stored value or trigger false orphan GC).
        // The config's load directory takes precedence: GUI/daemon processes
        // run with an unrelated cwd (Finder launches apps with cwd "/"), and
        // syncing that into the store would corrupt orphan detection.
        let project_path = config
            .project_root
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .map(|p| {
                crate::vcs::resolve_project_root(&p)
                    .to_string_lossy()
                    .to_string()
            });

        Ok(Self {
            project_name,
            service_name,
            image,
            port_range_start,
            pg_user,
            pg_password,
            pg_db,
            store: Mutex::new(store),
            runtime,
            storage,
            data_root,
            project_path,
            docker_settings: docker_settings.cloned().unwrap_or_default(),
        })
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        // Recover from a poisoned mutex instead of panicking. With
        // `panic = "abort"` a panic terminates the process, but a poisoned
        // lock can also occur across `.await` boundaries in threaded runtimes;
        // recovering the inner store keeps the provider usable after a
        // transient failure rather than cascading into every subsequent call.
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    async fn ensure_project(&self) -> Result<model::Project> {
        // Bind the lookup to a statement so the store() MutexGuard drops
        // immediately. Keeping it in an `if let` scrutinee extends the guard
        // over the whole block, and the second store() call below would then
        // self-deadlock (std::sync::Mutex is not reentrant). That exact bug
        // froze every provider operation in the GUI, whose cwd never matches
        // the stored project_path.
        let existing = self.store().get_project_by_name(&self.project_name)?;
        if let Some(mut project) = existing {
            // Keep project_path in sync with where the project actually lives;
            // a stale path (after `mv`/rename) would make orphan detection
            // declare a live project orphaned and GC its data.
            if let Some(ref path) = self.project_path {
                if project.project_path.as_deref() != Some(path.as_str()) {
                    self.store().update_project_path(&project.id, path)?;
                    log::debug!(
                        "Updated project_path for project '{}': {}",
                        self.project_name,
                        path
                    );
                    project.project_path = Some(path.clone());
                }
            }
            return Ok(project);
        }

        // Auto-create project
        let selection = self.storage.select_for_new_project().await;

        let project = self.store().create_project(NewProject {
            name: self.project_name.clone(),
            image: self.image.clone(),
            storage_driver: selection.driver,
            storage_config: selection.config,
            project_path: self.project_path.clone(),
        })?;

        log::info!(
            "Auto-created project '{}' with {} storage",
            self.project_name,
            project.storage_driver.as_str()
        );
        Ok(project)
    }

    async fn reconcile_project(&self, project: &model::Project) -> Result<()> {
        // Read workspaces from store (sync, releases lock before await)
        let workspaces = self.store().list_workspaces(&project.id)?;

        // Compute state changes (async, no store reference held)
        let changes = reconcile::compute_state_changes(&self.runtime, workspaces).await;

        // Apply changes (sync)
        if !changes.is_empty() {
            let store = self.store();
            for (workspace_id, new_state) in changes {
                store.update_workspace_state(&workspace_id, new_state)?;
            }
        }

        Ok(())
    }

    fn connection_uri(&self, port: u16) -> String {
        format!(
            "postgresql://{}:{}@127.0.0.1:{}/{}",
            self.pg_user, self.pg_password, port, self.pg_db
        )
    }
}

#[async_trait]
impl ServiceProvider for LocalProvider {
    async fn create_workspace(
        &self,
        workspace_name: &str,
        from_workspace: Option<&str>,
    ) -> Result<WorkspaceInfo> {
        let project = self.ensure_project().await?;
        self.reconcile_project(&project).await?;

        // Check if workspace already exists
        if let Some(existing) = self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?
        {
            if existing.state == WorkspaceState::Running {
                return Ok(WorkspaceInfo {
                    name: existing.name,
                    created_at: None,
                    parent_workspace: None,
                    database_name: self.pg_db.clone(),
                    state: Some(existing.state.as_str().to_string()),
                });
            }
        }

        let workspace_id = Uuid::new_v4().to_string();
        let data_dir = self
            .data_root
            .join("projects")
            .join(&project.id)
            .join("workspaces")
            .join(&workspace_id)
            .join("pgdata");

        // Reserve container name and find port
        let reserved = self
            .runtime
            .reserve_workspace(&ReserveWorkspaceSpec {
                project_name: self.project_name.clone(),
                service_name: self.service_name.clone(),
                workspace_name: workspace_name.to_string(),
            })
            .await?;

        let start_port = self.store().next_port()?.max(self.port_range_start);
        let port = docker::pick_available_port(self.runtime.client(), start_port).await?;

        // Clone or create empty
        let parent = if let Some(from_name) = from_workspace {
            self.store().get_workspace_by_name(&project.id, from_name)?
        } else {
            // No explicit parent: clone from an existing workspace, preferring
            // the most recent *stopped* one — its PGDATA is already quiesced,
            // so we skip the stop/restart cycle a running parent requires
            // (postgres shutdown checkpoint + restart, easily several seconds
            // of both latency and parent downtime).
            let workspaces = self.store().list_workspaces(&project.id)?;
            workspaces
                .iter()
                .find(|b| b.state == WorkspaceState::Stopped)
                .or_else(|| {
                    workspaces
                        .iter()
                        .find(|b| b.state == WorkspaceState::Running)
                })
                .cloned()
        };

        let storage_metadata = if let Some(ref parent_workspace) = parent {
            // Quiesce the source with a graceful STOP (shutdown checkpoint),
            // not a pause: a paused postgres still has dirty buffers and
            // unflushed WAL, and the per-file copy is not atomic — only a
            // cleanly stopped server guarantees a consistent PGDATA.
            let parent_running = self
                .runtime
                .container_status(&parent_workspace.container_name)
                .await?
                == docker::ContainerStatus::Running;

            if parent_running {
                let stop_started = std::time::Instant::now();
                self.runtime
                    .stop_workspace_for_clone(&parent_workspace.container_name)
                    .await?;
                log::debug!(
                    "[{}] stopped parent '{}' for clone in {:.2?}",
                    self.service_name,
                    parent_workspace.name,
                    stop_started.elapsed()
                );
            }

            let clone_started = std::time::Instant::now();
            let result = self
                .storage
                .clone_branch_from_parent(&project, parent_workspace, &workspace_id, &data_dir)
                .await;
            log::debug!(
                "[{}] cloned data dir from parent '{}' ({} storage) in {:.2?}",
                self.service_name,
                parent_workspace.name,
                project.storage_driver.as_str(),
                clone_started.elapsed()
            );

            if parent_running {
                // Restart even when the clone failed — never leave the
                // parent down as a side effect.
                if let Err(e) = self
                    .runtime
                    .start_existing(&parent_workspace.container_name)
                    .await
                {
                    log::warn!(
                        "Failed to restart parent container '{}' after clone: {}",
                        parent_workspace.container_name,
                        e
                    );
                }
            }

            result?
        } else {
            self.storage
                .create_empty_branch(&project, &workspace_id, &data_dir)
                .await?
        };

        // Persist to state
        let workspace = self.store().create_workspace(NewWorkspace {
            id: workspace_id,
            project_id: project.id.clone(),
            name: workspace_name.to_string(),
            parent_workspace_id: parent.as_ref().map(|p| p.id.clone()),
            state: WorkspaceState::Provisioning,
            data_dir: data_dir.to_string_lossy().to_string(),
            container_name: reserved.container_name.clone(),
            port,
            storage_metadata,
        })?;

        // Start container
        let start_started = std::time::Instant::now();
        self.runtime
            .start_workspace(&StartWorkspaceSpec {
                image: project.image.clone(),
                container_name: reserved.container_name.clone(),
                data_dir,
                port,
                pg_user: self.pg_user.clone(),
                pg_password: self.pg_password.clone(),
                pg_db: self.pg_db.clone(),
                project_name: self.project_name.clone(),
                service_name: self.service_name.clone(),
                workspace_name: workspace_name.to_string(),
                docker_settings: self.docker_settings.clone(),
            })
            .await?;

        // Wait for readiness
        self.runtime
            .wait_ready(
                &reserved.container_name,
                &self.pg_user,
                &self.pg_db,
                STARTUP_TIMEOUT,
            )
            .await?;
        log::debug!(
            "[{}] container '{}' started and ready in {:.2?}",
            self.service_name,
            reserved.container_name,
            start_started.elapsed()
        );

        // Update state
        self.store()
            .update_workspace_state(&workspace.id, WorkspaceState::Running)?;

        Ok(WorkspaceInfo {
            name: workspace_name.to_string(),
            created_at: Some(Utc::now()),
            parent_workspace: parent.as_ref().map(|p| p.name.clone()),
            database_name: self.pg_db.clone(),
            state: Some("running".to_string()),
        })
    }

    async fn delete_workspace(&self, workspace_name: &str) -> Result<()> {
        let project = self.ensure_project().await?;

        let workspace = self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' not found", workspace_name))?;

        // Remove container
        self.runtime
            .remove_branch(&workspace.container_name)
            .await?;

        // Delete storage data
        self.storage
            .delete_workspace_data(&project, &workspace)
            .await?;

        // Delete from state
        self.store().delete_workspace(&workspace.id)?;

        Ok(())
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let project = self.ensure_project().await?;
        self.reconcile_project(&project).await?;

        let workspaces = self.store().list_workspaces(&project.id)?;

        // Build id→name map so we can resolve parent_workspace_id to a name
        let id_to_name: std::collections::HashMap<&str, &str> = workspaces
            .iter()
            .map(|b| (b.id.as_str(), b.name.as_str()))
            .collect();

        Ok(workspaces
            .iter()
            .map(|b| WorkspaceInfo {
                name: b.name.clone(),
                created_at: None,
                parent_workspace: b
                    .parent_workspace_id
                    .as_deref()
                    .and_then(|pid| id_to_name.get(pid))
                    .map(|name| name.to_string()),
                database_name: self.pg_db.clone(),
                state: Some(b.state.as_str().to_string()),
            })
            .collect())
    }

    async fn workspace_exists(&self, workspace_name: &str) -> Result<bool> {
        let project = self.ensure_project().await?;
        Ok(self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?
            .is_some())
    }

    async fn switch_to_branch(&self, workspace_name: &str) -> Result<WorkspaceInfo> {
        let project = self.ensure_project().await?;
        self.reconcile_project(&project).await?;

        let workspace = self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' not found", workspace_name))?;

        // Start if stopped
        if workspace.state == WorkspaceState::Stopped {
            self.runtime
                .start_workspace(&StartWorkspaceSpec {
                    image: project.image.clone(),
                    container_name: workspace.container_name.clone(),
                    data_dir: PathBuf::from(&workspace.data_dir),
                    port: workspace.port,
                    pg_user: self.pg_user.clone(),
                    pg_password: self.pg_password.clone(),
                    pg_db: self.pg_db.clone(),
                    project_name: self.project_name.clone(),
                    service_name: self.service_name.clone(),
                    workspace_name: workspace_name.to_string(),
                    docker_settings: self.docker_settings.clone(),
                })
                .await?;

            self.runtime
                .wait_ready(
                    &workspace.container_name,
                    &self.pg_user,
                    &self.pg_db,
                    STARTUP_TIMEOUT,
                )
                .await?;
            self.store()
                .update_workspace_state(&workspace.id, WorkspaceState::Running)?;
        }

        Ok(WorkspaceInfo {
            name: workspace.name,
            created_at: None,
            parent_workspace: None,
            database_name: self.pg_db.clone(),
            state: Some("running".to_string()),
        })
    }

    async fn get_connection_info(&self, workspace_name: &str) -> Result<ConnectionInfo> {
        let project = self.ensure_project().await?;

        let workspace = self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' not found", workspace_name))?;

        Ok(ConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: workspace.port,
            database: self.pg_db.clone(),
            user: self.pg_user.clone(),
            password: Some(self.pg_password.clone()),
            connection_string: Some(self.connection_uri(workspace.port)),
        })
    }

    async fn start_workspace(&self, workspace_name: &str) -> Result<()> {
        let project = self.ensure_project().await?;

        let workspace = self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' not found", workspace_name))?;

        self.runtime
            .start_workspace(&StartWorkspaceSpec {
                image: project.image.clone(),
                container_name: workspace.container_name.clone(),
                data_dir: PathBuf::from(&workspace.data_dir),
                port: workspace.port,
                pg_user: self.pg_user.clone(),
                pg_password: self.pg_password.clone(),
                pg_db: self.pg_db.clone(),
                project_name: self.project_name.clone(),
                service_name: self.service_name.clone(),
                workspace_name: workspace_name.to_string(),
                docker_settings: self.docker_settings.clone(),
            })
            .await?;

        self.runtime
            .wait_ready(
                &workspace.container_name,
                &self.pg_user,
                &self.pg_db,
                STARTUP_TIMEOUT,
            )
            .await?;
        self.store()
            .update_workspace_state(&workspace.id, WorkspaceState::Running)?;

        Ok(())
    }

    async fn stop_workspace(&self, workspace_name: &str) -> Result<()> {
        let project = self.ensure_project().await?;

        let workspace = self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' not found", workspace_name))?;

        self.runtime
            .stop_workspace(&workspace.container_name)
            .await?;
        self.store()
            .update_workspace_state(&workspace.id, WorkspaceState::Stopped)?;

        Ok(())
    }

    async fn reset_workspace(&self, workspace_name: &str) -> Result<()> {
        let project = self.ensure_project().await?;

        let workspace = self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' not found", workspace_name))?;

        let was_running = workspace.state == WorkspaceState::Running;

        // Stop container
        self.runtime
            .stop_workspace(&workspace.container_name)
            .await?;

        // Re-clone from parent if available
        if let Some(parent_id) = &workspace.parent_workspace_id {
            let parent = self
                .store()
                .list_workspaces(&project.id)?
                .into_iter()
                .find(|b| &b.id == parent_id);

            if let Some(parent_workspace) = parent {
                let parent_running = self
                    .runtime
                    .container_status(&parent_workspace.container_name)
                    .await?
                    == docker::ContainerStatus::Running;

                if parent_running {
                    self.runtime
                        .stop_workspace_for_clone(&parent_workspace.container_name)
                        .await?;
                }

                let data_dir = PathBuf::from(&workspace.data_dir);
                let clone_result = self
                    .storage
                    .clone_branch_from_parent(&project, &parent_workspace, &workspace.id, &data_dir)
                    .await;

                if parent_running {
                    if let Err(e) = self
                        .runtime
                        .start_existing(&parent_workspace.container_name)
                        .await
                    {
                        log::warn!(
                            "Failed to restart parent container '{}' after reset clone: {}",
                            parent_workspace.container_name,
                            e
                        );
                    }
                }

                let new_metadata = clone_result?;

                if let Some(metadata) = &new_metadata {
                    self.store()
                        .update_workspace_storage_metadata(&workspace.id, Some(metadata))?;
                }
            }
        }

        // Restart if it was running
        if was_running {
            self.runtime
                .start_workspace(&StartWorkspaceSpec {
                    image: project.image.clone(),
                    container_name: workspace.container_name.clone(),
                    data_dir: PathBuf::from(&workspace.data_dir),
                    port: workspace.port,
                    pg_user: self.pg_user.clone(),
                    pg_password: self.pg_password.clone(),
                    pg_db: self.pg_db.clone(),
                    project_name: self.project_name.clone(),
                    service_name: self.service_name.clone(),
                    workspace_name: workspace_name.to_string(),
                    docker_settings: self.docker_settings.clone(),
                })
                .await?;

            self.runtime
                .wait_ready(
                    &workspace.container_name,
                    &self.pg_user,
                    &self.pg_db,
                    STARTUP_TIMEOUT,
                )
                .await?;
            self.store()
                .update_workspace_state(&workspace.id, WorkspaceState::Running)?;
        } else {
            self.store()
                .update_workspace_state(&workspace.id, WorkspaceState::Stopped)?;
        }

        Ok(())
    }

    fn supports_lifecycle(&self) -> bool {
        true
    }

    async fn test_connection(&self) -> Result<()> {
        let doctor = self.runtime.doctor().await;
        if !doctor.available {
            anyhow::bail!("Docker is not available: {}. Make sure Docker is installed and the daemon is running (try: docker info).", doctor.detail);
        }
        Ok(())
    }

    async fn doctor(&self) -> Result<DoctorReport> {
        let mut checks = vec![];

        // Docker check
        let docker_result = self.runtime.doctor().await;
        checks.push(DoctorCheck {
            name: "Docker".to_string(),
            available: docker_result.available,
            detail: if let Some(version) = docker_result.version {
                format!("Docker {} available", version)
            } else {
                docker_result.detail
            },
        });

        // Storage check
        let storage_report = self.storage.doctor().await;
        for entry in &storage_report.entries {
            if entry.available || entry.selected {
                checks.push(DoctorCheck {
                    name: format!("Storage: {}", entry.kind),
                    available: entry.available,
                    detail: entry.detail.clone(),
                });
            }
        }

        checks.push(DoctorCheck {
            name: "Default storage".to_string(),
            available: true,
            detail: format!(
                "Using {} for new projects",
                storage_report.default_driver.as_str()
            ),
        });

        // State database
        checks.push(DoctorCheck {
            name: "State database".to_string(),
            available: true,
            detail: format!("{}/state.db", self.data_root.display()),
        });

        Ok(DoctorReport { checks })
    }

    async fn seed_from_source(&self, workspace_name: &str, source: &str) -> Result<()> {
        let project = self.ensure_project().await?;
        let workspace = self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' not found", workspace_name))?;
        let parsed = seed::parse_source(source)?;
        seed::seed_branch(
            self.runtime.client(),
            &parsed,
            &workspace.container_name,
            &self.pg_user,
            &self.pg_db,
            &self.image,
        )
        .await
    }

    fn project_info(&self) -> Option<ProjectInfo> {
        let project = self
            .store()
            .get_project_by_name(&self.project_name)
            .ok()??;
        Some(ProjectInfo {
            name: project.name,
            storage_driver: Some(project.storage_driver.as_str().to_string()),
            image: Some(project.image),
        })
    }

    fn provider_name(&self) -> &'static str {
        "Local (Docker + CoW)"
    }

    fn capabilities(&self) -> ServiceCapabilities {
        ServiceCapabilities {
            lifecycle: true,
            logs: true,
            destroy_project: true,
            cleanup: true,
            seed_from_source: true,
            template_from_time: false,
            max_workspace_name_length: 255,
        }
    }

    fn supports_cleanup(&self) -> bool {
        true
    }

    fn max_workspace_name_length(&self) -> usize {
        255
    }

    fn supports_destroy(&self) -> bool {
        true
    }

    async fn logs(&self, workspace_name: &str, tail: Option<usize>) -> Result<String> {
        let project = self.ensure_project().await?;
        let workspace = self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' not found", workspace_name))?;

        self.runtime
            .container_logs(&workspace.container_name, tail)
            .await
    }

    async fn destroy_preview(&self) -> Result<Option<(String, Vec<String>)>> {
        let project = match self.store().get_project_by_name(&self.project_name)? {
            Some(p) => p,
            None => return Ok(None),
        };

        let workspaces = self.store().list_workspaces(&project.id)?;
        let workspace_names: Vec<String> = workspaces.iter().map(|b| b.name.clone()).collect();

        Ok(Some((project.name.clone(), workspace_names)))
    }

    async fn destroy_project(&self) -> Result<Vec<String>> {
        let project = self
            .store()
            .get_project_by_name(&self.project_name)?
            .ok_or_else(|| anyhow::anyhow!("Project '{}' not found", self.project_name))?;

        let workspaces = self.store().list_workspaces(&project.id)?;
        let workspace_names: Vec<String> = workspaces.iter().map(|b| b.name.clone()).collect();

        // 1. Remove all Docker containers (best-effort)
        for workspace in &workspaces {
            if let Err(e) = self.runtime.remove_branch(&workspace.container_name).await {
                log::warn!(
                    "Failed to remove container '{}': {}",
                    workspace.container_name,
                    e
                );
            }
        }

        // 2. Delete project-level storage data
        self.storage.delete_project_data(&project).await?;

        // 3. Delete project from SQLite (cascades to workspaces)
        self.store().delete_project(&project.id)?;

        Ok(workspace_names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn provider_for(
        data_root: &std::path::Path,
        project_root: &std::path::Path,
    ) -> LocalProvider {
        let config = Config {
            name: Some("deadlock-regression".to_string()),
            project_root: Some(project_root.to_path_buf()),
            ..Default::default()
        };
        let local = LocalServiceConfig {
            data_root: Some(data_root.to_string_lossy().to_string()),
            ..Default::default()
        };
        LocalProvider::new("db", &config, Some(&local), None)
            .await
            .expect("provider construction must not require a running Docker daemon")
    }

    /// Regression test: `ensure_project` used to hold the store MutexGuard in
    /// its `if let` scrutinee and lock the store again in the
    /// project_path-mismatch branch — a guaranteed self-deadlock that froze
    /// every workspace/service operation in the GUI (whose cwd never matches
    /// the stored project path).
    #[tokio::test]
    async fn ensure_project_path_resync_does_not_deadlock() {
        let data_dir = tempfile::tempdir().unwrap();
        let root_a = tempfile::tempdir().unwrap();
        let root_b = tempfile::tempdir().unwrap();

        // First provider auto-creates the project with root A as its path.
        let provider_a = provider_for(data_dir.path(), root_a.path()).await;
        let project = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            provider_a.ensure_project(),
        )
        .await
        .expect("ensure_project must not hang")
        .unwrap();
        assert_eq!(project.name, "deadlock-regression");

        // Second provider sees a different project root → hits the
        // project_path-mismatch branch that used to self-deadlock.
        let provider_b = provider_for(data_dir.path(), root_b.path()).await;
        let project = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            provider_b.ensure_project(),
        )
        .await
        .expect("ensure_project must not deadlock on project_path resync")
        .unwrap();

        let expected = root_b
            .path()
            .canonicalize()
            .unwrap_or_else(|_| root_b.path().to_path_buf());
        assert_eq!(
            project.project_path.as_deref(),
            Some(expected.to_string_lossy().as_ref()),
            "project_path must be resynced to the new root"
        );
    }
}

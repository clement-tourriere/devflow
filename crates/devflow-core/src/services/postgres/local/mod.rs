pub mod docker;
pub mod model;
pub mod reconcile;
pub mod seed;
pub mod state;
pub mod storage;

use std::path::{Path, PathBuf};
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
    owner_key: String,
    legacy_project_id: Option<String>,
    service_name: String,
    image: String,
    port_range_start: u16,
    pg_user: String,
    pg_password: String,
    pg_db: String,
    store: Mutex<Store>,
    runtime: DockerRuntime,
    storage: StorageCoordinator,
    storage_preference: Option<String>,
    data_root: PathBuf,
    /// Canonical filesystem path of the project directory (for orphan detection).
    project_path: Option<String>,
    docker_settings: DockerCustomSettings,
}

impl LocalProvider {
    fn image_path(&self, workspace_id: &str) -> PathBuf {
        self.data_root
            .join("images")
            .join(format!("{workspace_id}.image"))
    }
    fn seal_path(&self, workspace_id: &str) -> PathBuf {
        self.data_root
            .join("sealed")
            .join(format!("{workspace_id}.image"))
    }
    fn sealed_image(&self, name: &str) -> Result<Option<String>> {
        let Some(project) = self.store().project_for_owner(&self.owner_key)? else {
            return Ok(None);
        };
        let Some(workspace) = self.store().get_workspace_by_name(&project.id, name)? else {
            return Ok(None);
        };
        let path = self.seal_path(&workspace.id);
        if path.exists() {
            Ok(Some(std::fs::read_to_string(path)?))
        } else {
            Ok(None)
        }
    }
    fn workspace_image(
        &self,
        project: &model::Project,
        workspace: &model::Workspace,
    ) -> Result<String> {
        let workspaces = self.store().list_workspaces(&project.id)?;
        let mut current = workspace;
        for _ in 0..=workspaces.len() {
            let pinned = self.image_path(&current.id);
            if pinned.try_exists()? {
                return Ok(std::fs::read_to_string(pinned)?);
            }
            let seal = self.seal_path(&current.id);
            if seal.exists() {
                return Ok(std::fs::read_to_string(seal)?);
            }
            let Some(parent) = &current.parent_workspace_id else {
                return Ok(project.image.clone());
            };
            current = workspaces
                .iter()
                .find(|w| &w.id == parent)
                .context("workspace data parent is missing")?;
        }
        anyhow::bail!("cyclic workspace data ancestry")
    }

    fn ensure_unsealed(&self, name: &str) -> Result<()> {
        anyhow::ensure!(
            self.sealed_image(name)?.is_none(),
            "'{name}' is a sealed baseline; create a child workspace to modify its data"
        );
        Ok(())
    }
    async fn delete_workspace_inner(&self, workspace_name: &str, allow_sealed: bool) -> Result<()> {
        let project = self.ensure_project().await?;
        let _lease = std::sync::Arc::new(
            crate::services::ownership::OperationLease::acquire(
                &self.operation_path(&project.id, workspace_name),
            )
            .await?,
        );

        let Some(workspace) = self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?
        else {
            // A retry after partial multi-provider cleanup must accept the
            // resource already being removed from this exact owned project.
            return Ok(());
        };

        if !allow_sealed {
            self.ensure_unsealed(workspace_name)?;
        }
        anyhow::ensure!(
            !self
                .store()
                .list_workspaces(&project.id)?
                .iter()
                .any(|w| w.parent_workspace_id.as_deref() == Some(&workspace.id)),
            "workspace has dependent data generations"
        );
        // Remove container
        self.runtime
            .remove_branch(&workspace.container_name)
            .await?;

        // Delete storage data
        self.storage
            .delete_workspace_data(&project, &workspace)
            .await?;

        let seal = self.seal_path(&workspace.id);
        if seal.exists() {
            std::fs::remove_file(seal)?;
        }
        let image = self.image_path(&workspace.id);
        if image.try_exists()? {
            std::fs::remove_file(image)?;
        }
        // Delete from state
        self.store().delete_workspace(&workspace.id)?;

        Ok(())
    }

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
        let storage_preference = local_config.and_then(|c| c.storage.clone());

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
            owner_key: crate::services::ownership::identity_digest(&[
                &config.resource_project_identity(),
                &service_name,
            ]),
            legacy_project_id: local_config.and_then(|c| c.legacy_project_id.clone()),
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
            storage_preference,
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

    fn operation_path(&self, project_id: &str, workspace: &str) -> PathBuf {
        self.data_root.join("operations").join(format!(
            "{}.lock",
            crate::services::ownership::identity_digest(&[project_id, workspace])
        ))
    }

    async fn ensure_project(&self) -> Result<model::Project> {
        let _lease = crate::services::ownership::OperationLease::acquire(
            &self
                .data_root
                .join("operations")
                .join(format!("project-{}.lock", self.owner_key)),
        )
        .await?;
        let existing = self.store().project_for_owner(&self.owner_key)?;
        if let Some(mut project) = existing {
            if let Some(path) = &self.project_path {
                if project.project_path.as_ref() != Some(path) {
                    self.store().update_project_path(&project.id, path)?;
                    project.project_path = Some(path.clone());
                }
            }
            return Ok(project);
        }
        if let Some(id) = &self.legacy_project_id {
            let project = self
                .store()
                .list_projects()?
                .into_iter()
                .find(|p| &p.id == id)
                .context("configured legacy_project_id does not exist")?;
            self.store().claim_project(&project.id, &self.owner_key)?;
            return Ok(project);
        }
        if let Some(legacy) = self.store().get_project_by_name(&self.project_name)? {
            anyhow::bail!("legacy local project '{}' (id {}) has no service ownership. Verify its data and configure local.legacy_project_id to adopt it; existing data was left untouched", self.project_name, legacy.id);
        }
        let storage_name = format!("{} [{}]", self.project_name, self.owner_key);
        let existing = self.store().get_project_by_name(&storage_name)?;
        let project = match existing {
            Some(project) => project,
            None => {
                create_project_record(
                    &self.store,
                    &self.storage,
                    self.storage_preference.as_deref(),
                    &storage_name,
                    &self.image,
                    self.project_path.clone(),
                )
                .await?
            }
        };
        self.store().claim_project(&project.id, &self.owner_key)?;
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

async fn create_project_record(
    store: &Mutex<Store>,
    storage: &StorageCoordinator,
    storage_preference: Option<&str>,
    project_name: &str,
    image: &str,
    project_path: Option<String>,
) -> Result<model::Project> {
    // local.storage may force a specific CoW driver; otherwise devflow probes the
    // best available option and falls back to copy.
    let selection = storage
        .select_for_new_project_with(storage_preference)
        .await?;

    let project = store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .create_project(NewProject {
            name: project_name.to_string(),
            image: image.to_string(),
            storage_driver: selection.driver,
            storage_config: selection.config,
            project_path,
        })?;

    Ok(project)
}

#[async_trait]
impl ServiceProvider for LocalProvider {
    async fn create_workspace(
        &self,
        workspace_name: &str,
        from_workspace: Option<&str>,
    ) -> Result<WorkspaceInfo> {
        let project = self.ensure_project().await?;
        let _lease = std::sync::Arc::new(
            crate::services::ownership::OperationLease::acquire(
                &self.operation_path(&project.id, workspace_name),
            )
            .await?,
        );
        self.ensure_unsealed(workspace_name)?;
        self.reconcile_project(&project).await?;

        let existing = self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?;
        if let Some(existing) = existing {
            if matches!(
                existing.state,
                WorkspaceState::Running | WorkspaceState::Stopped
            ) {
                drop(_lease);
                return self.switch_to_branch(workspace_name).await;
            }
            anyhow::ensure!(existing.state != WorkspaceState::Failed,
                "Workspace '{}' has a failed seed/setup; retry the seed or explicitly remove and recreate it", workspace_name);
            // Provisioning intent is persisted before a clone. Its owned partial
            // resources can be cleaned and retried after the clone lease ends.
            self.runtime.remove_branch(&existing.container_name).await?;
            self.storage
                .delete_workspace_data(&project, &existing)
                .await?;
            self.store().delete_workspace(&existing.id)?;
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
                project_name: project.id.clone(),
                service_name: self.service_name.clone(),
                workspace_name: workspace_name.to_string(),
            })
            .await?;

        let start_port = self.store().next_port()?.max(self.port_range_start);
        let _port_reservation =
            crate::services::local_docker::pick_available_port(self.runtime.client(), start_port)
                .await?;
        let port = _port_reservation.port;

        // No implicit "most recent" baseline: data origin must be deterministic.
        let parent = from_workspace
            .map(|name| {
                self.store()
                    .get_workspace_by_name(&project.id, name)
                    .and_then(|parent| {
                        parent.ok_or_else(|| anyhow::anyhow!("Data parent '{name}' does not exist"))
                    })
            })
            .transpose()?;
        if let Some(parent) = &parent {
            anyhow::ensure!(
                matches!(
                    parent.state,
                    WorkspaceState::Running | WorkspaceState::Stopped
                ),
                "Data parent is not ready"
            );
            anyhow::ensure!(
                Path::new(&parent.data_dir).is_dir(),
                "Data parent directory is missing"
            );
        }
        let workspace = self.store().create_workspace(NewWorkspace {
            id: workspace_id.clone(),
            project_id: project.id.clone(),
            name: workspace_name.into(),
            parent_workspace_id: parent.as_ref().map(|p| p.id.clone()),
            state: WorkspaceState::Provisioning,
            data_dir: data_dir.to_string_lossy().into(),
            container_name: reserved.container_name.clone(),
            port,
            storage_metadata: None,
        })?;
        let image = if let Some(parent) = &parent {
            self.workspace_image(&project, parent)?
        } else {
            project.image.clone()
        };
        self.runtime.ensure_image(&image).await?;
        let image = self
            .runtime
            .client()
            .inspect_image(&image)
            .await?
            .id
            .context("Docker image lacks immutable identity")?;
        crate::services::ownership::atomic_write(
            &self.image_path(&workspace.id),
            image.as_bytes(),
        )?;
        if let Some(parent) = &parent {
            anyhow::ensure!(
                parent.name != workspace_name,
                "cannot clone a workspace onto itself"
            );
        }
        let storage_metadata = if let Some(ref parent_workspace) = parent {
            let storage = self.storage.clone();
            let clone_project = project.clone();
            let clone_parent = parent_workspace.clone();
            let clone_id = workspace_id.clone();
            let clone_dir = data_dir.clone();
            let target_lease = _lease.clone();
            crate::services::local_docker::quiesced_operation(
                self.runtime.client().clone(),
                parent_workspace.container_name.clone(),
                self.operation_path(&project.id, &parent_workspace.name),
                vec![
                    "pg_isready".into(),
                    "-h".into(),
                    "127.0.0.1".into(),
                    "-U".into(),
                    self.pg_user.clone(),
                    "-d".into(),
                    self.pg_db.clone(),
                ],
                async move {
                    let _target_lease = target_lease;
                    storage
                        .clone_branch_from_parent(
                            &clone_project,
                            &clone_parent,
                            &clone_id,
                            &clone_dir,
                        )
                        .await
                },
            )
            .await?
        } else {
            self.storage
                .create_empty_branch(&project, &workspace_id, &data_dir)
                .await?
        };

        self.store()
            .update_workspace_storage_metadata(&workspace.id, storage_metadata.as_deref())?;

        // Start container
        let start_started = std::time::Instant::now();
        self.runtime
            .start_workspace(&StartWorkspaceSpec {
                image,
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
        self.delete_workspace_inner(workspace_name, false).await
    }
    async fn delete_baseline(&self, workspace_name: &str) -> Result<()> {
        self.delete_workspace_inner(workspace_name, true).await
    }
    async fn baseline_fingerprint(&self, workspace_name: &str) -> Result<String> {
        self.sealed_image(workspace_name)?
            .context("baseline is not sealed")
    }
    async fn seal_baseline(&self, workspace_name: &str) -> Result<String> {
        let project = self.ensure_project().await?;
        let _lease = crate::services::ownership::OperationLease::acquire(
            &self.operation_path(&project.id, workspace_name),
        )
        .await?;
        if let Some(image) = self.sealed_image(workspace_name)? {
            return Ok(image);
        }
        let workspace = self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?
            .context("baseline workspace missing")?;
        let (code, error) = seed::docker_exec(
            self.runtime.client(),
            &workspace.container_name,
            &[
                "psql",
                "-U",
                &self.pg_user,
                "-d",
                &self.pg_db,
                "-v",
                "ON_ERROR_STOP=1",
                "-c",
                "SELECT 1",
            ],
        )
        .await?;
        anyhow::ensure!(code == 0, "baseline validation failed: {error}");
        let inspect = self
            .runtime
            .client()
            .inspect_container(&workspace.container_name, None)
            .await?;
        let image = inspect
            .image
            .context("baseline container lacks immutable image identity")?;
        self.runtime
            .stop_workspace(&workspace.container_name)
            .await?;
        self.store()
            .update_workspace_state(&workspace.id, WorkspaceState::Stopped)?;
        crate::services::ownership::atomic_write(&self.seal_path(&workspace.id), image.as_bytes())?;
        Ok(image)
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let project = self.store().project_for_owner(&self.owner_key)?;
        let Some(project) = project else {
            return Ok(Vec::new());
        };
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
        let project = self.store().project_for_owner(&self.owner_key)?;
        let Some(project) = project else {
            return Ok(false);
        };
        Ok(self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?
            .is_some())
    }

    async fn switch_to_branch(&self, workspace_name: &str) -> Result<WorkspaceInfo> {
        let project = self.ensure_project().await?;
        let _lease = std::sync::Arc::new(
            crate::services::ownership::OperationLease::acquire(
                &self.operation_path(&project.id, workspace_name),
            )
            .await?,
        );
        self.ensure_unsealed(workspace_name)?;
        self.reconcile_project(&project).await?;

        let workspace = self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' not found", workspace_name))?;

        anyhow::ensure!(
            matches!(
                workspace.state,
                WorkspaceState::Running | WorkspaceState::Stopped
            ),
            "workspace setup is incomplete or failed; retry setup before switching"
        );
        // Start if stopped
        if workspace.state == WorkspaceState::Stopped {
            self.runtime
                .start_workspace(&StartWorkspaceSpec {
                    image: self.workspace_image(&project, &workspace)?,
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
        let project = self.store().project_for_owner(&self.owner_key)?;
        let Some(project) = project else {
            anyhow::bail!("service project has not been initialized");
        };

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
        let _lease = std::sync::Arc::new(
            crate::services::ownership::OperationLease::acquire(
                &self.operation_path(&project.id, workspace_name),
            )
            .await?,
        );
        self.ensure_unsealed(workspace_name)?;

        let workspace = self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' not found", workspace_name))?;

        anyhow::ensure!(
            matches!(
                workspace.state,
                WorkspaceState::Running | WorkspaceState::Stopped
            ),
            "workspace setup is incomplete or failed; retry the failed operation first"
        );

        self.runtime
            .start_workspace(&StartWorkspaceSpec {
                image: self.workspace_image(&project, &workspace)?,
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
        let _lease = std::sync::Arc::new(
            crate::services::ownership::OperationLease::acquire(
                &self.operation_path(&project.id, workspace_name),
            )
            .await?,
        );

        let workspace = self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' not found", workspace_name))?;

        self.runtime
            .stop_workspace(&workspace.container_name)
            .await?;
        if !matches!(
            workspace.state,
            WorkspaceState::Provisioning | WorkspaceState::Failed
        ) {
            self.store()
                .update_workspace_state(&workspace.id, WorkspaceState::Stopped)?;
        }

        Ok(())
    }

    async fn reset_workspace(&self, workspace_name: &str) -> Result<()> {
        let project = self.ensure_project().await?;
        let _lease = std::sync::Arc::new(
            crate::services::ownership::OperationLease::acquire(
                &self.operation_path(&project.id, workspace_name),
            )
            .await?,
        );
        self.ensure_unsealed(workspace_name)?;

        let workspace = self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' not found", workspace_name))?;

        let parent_id = workspace
            .parent_workspace_id
            .as_ref()
            .context("workspace has no data parent to reset from")?;
        anyhow::ensure!(
            self.store()
                .list_workspaces(&project.id)?
                .iter()
                .any(|p| &p.id == parent_id
                    && matches!(p.state, WorkspaceState::Running | WorkspaceState::Stopped)
                    && Path::new(&p.data_dir).is_dir()),
            "data parent is missing or not ready; reset was not started"
        );
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
                let storage = self.storage.clone();
                let clone_project = project.clone();
                let clone_parent = parent_workspace.clone();
                let clone_id = workspace.id.clone();
                let clone_dir = PathBuf::from(&workspace.data_dir);
                let target_lease = _lease.clone();
                let new_metadata = crate::services::local_docker::quiesced_operation(
                    self.runtime.client().clone(),
                    parent_workspace.container_name.clone(),
                    self.operation_path(&project.id, &parent_workspace.name),
                    vec![
                        "pg_isready".into(),
                        "-h".into(),
                        "127.0.0.1".into(),
                        "-U".into(),
                        self.pg_user.clone(),
                        "-d".into(),
                        self.pg_db.clone(),
                    ],
                    async move {
                        let _target_lease = target_lease;
                        storage
                            .clone_branch_from_parent(
                                &clone_project,
                                &clone_parent,
                                &clone_id,
                                &clone_dir,
                            )
                            .await
                    },
                )
                .await?;

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
                    image: self.workspace_image(&project, &workspace)?,
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

    fn operations(&self) -> crate::services::OperationCapabilities {
        crate::services::OperationCapabilities {
            start: true,
            stop: true,
            logs: true,
            reset: true,
            seed: true,
            snapshot: true,
        }
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

        match storage::replacement_diagnostics(&self.data_root) {
            Ok(recovery) => checks.extend(recovery),
            Err(error) => checks.push(DoctorCheck {
                name: "ZFS replacement recovery".into(),
                available: false,
                detail: format!("Cannot inspect replacement records: {error:#}"),
            }),
        }
        checks.push(data_root_doctor_check(&self.data_root));
        checks.push(port_range_doctor_check(self.port_range_start));

        if self.storage_preference.is_some() {
            let preference = self
                .storage
                .check_preference(self.storage_preference.as_deref())
                .await;
            checks.push(DoctorCheck {
                name: "Configured storage preference".to_string(),
                available: preference.available,
                detail: preference.detail,
            });
        }

        // State database
        checks.push(DoctorCheck {
            name: "State database".to_string(),
            available: true,
            detail: format!("{}/state.db", self.data_root.display()),
        });

        if docker_result.available {
            let project = self.store().project_for_owner(&self.owner_key)?;
            if let Some(project) = project {
                let workspaces = self.store().list_workspaces(&project.id)?;
                for workspace in workspaces {
                    let result: Result<()> = async {
                        let image = self.workspace_image(&project, &workspace)?;
                        self.runtime
                            .verify_workspace(&StartWorkspaceSpec {
                                image,
                                container_name: workspace.container_name.clone(),
                                data_dir: PathBuf::from(&workspace.data_dir),
                                port: workspace.port,
                                pg_user: self.pg_user.clone(),
                                pg_password: self.pg_password.clone(),
                                pg_db: self.pg_db.clone(),
                                project_name: self.project_name.clone(),
                                service_name: self.service_name.clone(),
                                workspace_name: workspace.name.clone(),
                                docker_settings: self.docker_settings.clone(),
                            })
                            .await
                    }
                    .await;
                    checks.push(DoctorCheck {
                        name: format!("Workspace engine: {}", workspace.name),
                        available: result.is_ok(),
                        detail: result.err().map(|e| format!("{e:#}")).unwrap_or_else(|| {
                            "Immutable image and container settings match".into()
                        }),
                    });
                }
            }
        }

        Ok(DoctorReport { checks })
    }

    async fn seed_from_source(&self, workspace_name: &str, source: &str) -> Result<()> {
        let project = self.ensure_project().await?;
        let _lease = std::sync::Arc::new(
            crate::services::ownership::OperationLease::acquire(
                &self.operation_path(&project.id, workspace_name),
            )
            .await?,
        );
        self.ensure_unsealed(workspace_name)?;
        let workspace = self
            .store()
            .get_workspace_by_name(&project.id, workspace_name)?
            .ok_or_else(|| anyhow::anyhow!("Workspace '{}' not found", workspace_name))?;
        let parsed = seed::parse_source(source)?;
        anyhow::ensure!(
            workspace.state != WorkspaceState::Provisioning,
            "workspace creation is incomplete; retry creation before seeding"
        );
        self.store()
            .update_workspace_state(&workspace.id, WorkspaceState::Failed)?;
        let result = async {
            // Explicit seeding is also the repair path for a failed import.
            // Keep the workspace failed until both startup and restore finish.
            self.runtime
                .start_workspace(&StartWorkspaceSpec {
                    image: self.workspace_image(&project, &workspace)?,
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
            seed::seed_branch(
                self.runtime.client(),
                &parsed,
                &workspace.container_name,
                &self.pg_user,
                &self.pg_db,
                &self.workspace_image(&project, &workspace)?,
            )
            .await
        }
        .await;
        self.store().update_workspace_state(
            &workspace.id,
            if result.is_ok() {
                WorkspaceState::Running
            } else {
                WorkspaceState::Failed
            },
        )?;
        result
    }

    fn project_info(&self) -> Option<ProjectInfo> {
        let project = self.store().project_for_owner(&self.owner_key).ok()??;
        Some(ProjectInfo {
            name: self.project_name.clone(),
            storage_driver: Some(project.storage_driver.as_str().to_string()),
            image: Some(project.image),
        })
    }

    fn provider_name(&self) -> &'static str {
        "Local (Docker + CoW)"
    }

    fn data_strategy(&self) -> &'static str {
        "physical_copy"
    }

    fn capabilities(&self) -> ServiceCapabilities {
        ServiceCapabilities {
            data_strategy: self.data_strategy().into(),
            lifecycle: true,
            logs: true,
            destroy_project: true,
            cleanup: true,
            seed_from_source: true,
            operations: self.operations(),
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
        let project = match self.store().project_for_owner(&self.owner_key)? {
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
            .project_for_owner(&self.owner_key)?
            .ok_or_else(|| anyhow::anyhow!("Project '{}' not found", self.project_name))?;

        let workspaces = self.store().list_workspaces(&project.id)?;
        let workspace_names: Vec<String> = workspaces.iter().map(|b| b.name.clone()).collect();

        // Delete exact workspace resources and retain the small project ownership
        // record. Never recursively remove a root that a concurrent create may use.
        let mut remaining = workspaces;
        while !remaining.is_empty() {
            let parents = remaining
                .iter()
                .filter_map(|w| w.parent_workspace_id.as_deref())
                .collect::<std::collections::HashSet<_>>();
            let leaves = remaining
                .iter()
                .filter(|w| !parents.contains(w.id.as_str()))
                .map(|w| w.name.clone())
                .collect::<Vec<_>>();
            anyhow::ensure!(
                !leaves.is_empty(),
                "cyclic data dependencies prevent project destruction"
            );
            for name in &leaves {
                self.delete_workspace_inner(name, true).await?;
            }
            remaining = self.store().list_workspaces(&project.id)?;
        }

        Ok(workspace_names)
    }
}

fn data_root_doctor_check(data_root: &std::path::Path) -> DoctorCheck {
    match std::fs::metadata(data_root) {
        Ok(metadata) if metadata.is_dir() => DoctorCheck {
            name: "Data root".to_string(),
            available: true,
            detail: format!("Using {}", data_root.display()),
        },
        Ok(_) => DoctorCheck {
            name: "Data root".to_string(),
            available: false,
            detail: format!("{} exists but is not a directory", data_root.display()),
        },
        Err(err) => DoctorCheck {
            name: "Data root".to_string(),
            available: false,
            detail: format!("{} is not accessible: {}", data_root.display(), err),
        },
    }
}

fn port_range_doctor_check(port_range_start: u16) -> DoctorCheck {
    let available = port_range_start >= 1024;
    DoctorCheck {
        name: "Port range".to_string(),
        available,
        detail: if available {
            format!("Workspace ports start at {}", port_range_start)
        } else {
            format!(
                "Workspace ports start at {}; choose an unprivileged port >= 1024",
                port_range_start
            )
        },
    }
}

#[cfg(test)]
mod regression_tests {
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
        assert!(project.name.starts_with("deadlock-regression ["));
        let first_id = project.id.clone();

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

        assert_ne!(
            project.id, first_id,
            "repositories sharing a display name must own separate data"
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use model::StorageDriver;

    #[test]
    fn doctor_reports_data_root_and_privileged_port_range() {
        let tmp = tempfile::tempdir().unwrap();
        let root_check = data_root_doctor_check(tmp.path());
        assert_eq!(root_check.name, "Data root");
        assert!(root_check.available);

        let port_check = port_range_doctor_check(80);
        assert_eq!(port_check.name, "Port range");
        assert!(!port_check.available);
        assert!(port_check.detail.contains(">= 1024"));
    }

    #[tokio::test]
    async fn create_project_record_honors_copy_storage_preference() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Mutex::new(Store::open(&tmp.path().join("state.db")).unwrap());
        let storage = StorageCoordinator::new(tmp.path().join("projects"));

        let project = create_project_record(
            &store,
            &storage,
            Some("copy"),
            "app-db",
            DEFAULT_IMAGE,
            Some("/tmp/devflow-test-project".to_string()),
        )
        .await
        .expect("copy storage preference should create a local PostgreSQL project record");

        assert_eq!(project.storage_driver, StorageDriver::Copy);
        assert_eq!(project.storage_config, None);

        let persisted = store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_project_by_name("app-db")
            .unwrap()
            .expect("project should be persisted");
        assert_eq!(persisted.storage_driver, StorageDriver::Copy);
        assert_eq!(persisted.image, DEFAULT_IMAGE);
    }
}

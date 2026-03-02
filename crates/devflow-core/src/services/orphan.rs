//! Orphan project detection and cleanup.
//!
//! Detects projects whose directories no longer exist on disk but still have
//! leftover state in one or more stores (SQLite, local state YAML, Docker
//! containers).  Provides cleanup routines that remove all associated resources
//! without requiring the original `.devflow.yml` config file.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::state::LocalStateManager;

// ── Public types ────────────────────────────────────────────────────

/// Where orphaned state was found.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrphanSource {
    /// Entry in the SQLite `projects` table (local Docker provider).
    Sqlite,
    /// Entry in `~/.config/devflow/local_state.yml`.
    LocalState,
    /// Docker containers still running/stopped for this project.
    Docker,
}

/// Describes a single orphaned project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrphanInfo {
    /// Human-readable project name (from SQLite or local-state).
    pub project_name: String,
    /// Original filesystem path (the local-state key), if known.
    pub project_path: Option<String>,
    /// Which state stores reference this project.
    pub sources: Vec<OrphanSource>,

    // ── SQLite details ──
    /// Project row id in the SQLite `projects` table, if present.
    pub sqlite_project_id: Option<String>,
    /// Number of branches tracked in SQLite.
    pub sqlite_branch_count: usize,

    // ── Docker details ──
    /// Docker container names belonging to this project.
    pub container_names: Vec<String>,

    // ── Local state details ──
    /// Number of services registered in local state YAML.
    pub local_state_service_count: usize,
    /// Number of branches registered in local state YAML.
    pub local_state_branch_count: usize,
}

/// Result of cleaning up a single orphaned project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupResult {
    pub project_name: String,
    pub containers_removed: usize,
    pub sqlite_rows_deleted: bool,
    pub local_state_cleared: bool,
    pub data_dirs_removed: usize,
    pub errors: Vec<String>,
}

// ── Detection ───────────────────────────────────────────────────────

/// Scan all state stores and return orphaned projects whose directories no
/// longer exist on disk.
///
/// This function is intentionally synchronous for the local-state / SQLite
/// parts and uses `tokio::runtime::Handle` for the Docker query so it can be
/// called from both sync and async contexts.
pub async fn detect_orphans() -> Result<Vec<OrphanInfo>> {
    let mut orphans: HashMap<String, OrphanInfo> = HashMap::new();

    // 1. Scan local state YAML ────────────────────────────────────────
    if let Ok(state_mgr) = LocalStateManager::new() {
        let projects = state_mgr.list_all_projects();
        for (project_key, project_state) in &projects {
            let path = Path::new(project_key);
            if path.exists() {
                continue; // directory still exists — not an orphan
            }

            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| project_key.clone());

            let service_count = project_state
                .services
                .as_ref()
                .map(|s| s.len())
                .unwrap_or(0);
            let branch_count = project_state
                .branches
                .as_ref()
                .map(|b| b.len())
                .unwrap_or(0);

            let entry = orphans.entry(name.clone()).or_insert_with(|| OrphanInfo {
                project_name: name,
                project_path: Some(project_key.clone()),
                sources: Vec::new(),
                sqlite_project_id: None,
                sqlite_branch_count: 0,
                container_names: Vec::new(),
                local_state_service_count: 0,
                local_state_branch_count: 0,
            });
            if !entry.sources.contains(&OrphanSource::LocalState) {
                entry.sources.push(OrphanSource::LocalState);
            }
            entry.local_state_service_count = service_count;
            entry.local_state_branch_count = branch_count;
        }
    }

    // 2. Scan SQLite store ────────────────────────────────────────────
    #[cfg(feature = "service-local")]
    {
        if let Ok(store) = open_sqlite_store() {
            if let Ok(sqlite_projects) = store.list_projects() {
                // Build a set of known-live project names from local state
                let live_project_names: HashSet<String> = if let Ok(mgr) = LocalStateManager::new()
                {
                    mgr.list_all_projects()
                        .iter()
                        .filter(|(key, _)| Path::new(key.as_str()).exists())
                        .filter_map(|(key, _)| {
                            Path::new(key.as_str())
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                        })
                        .collect()
                } else {
                    HashSet::new()
                };

                for proj in sqlite_projects {
                    // If this project name matches a live project, skip
                    if live_project_names.contains(&proj.name) {
                        continue;
                    }

                    // Also check if any local state path exists for this name
                    let has_live_path = if let Ok(mgr) = LocalStateManager::new() {
                        mgr.list_all_projects().iter().any(|(key, _)| {
                            let p = Path::new(key.as_str());
                            p.exists()
                                && p.file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .as_deref()
                                    == Some(&proj.name)
                        })
                    } else {
                        false
                    };

                    if has_live_path {
                        continue;
                    }

                    let branch_count =
                        store.list_branches(&proj.id).map(|b| b.len()).unwrap_or(0);

                    let entry =
                        orphans
                            .entry(proj.name.clone())
                            .or_insert_with(|| OrphanInfo {
                                project_name: proj.name.clone(),
                                project_path: None,
                                sources: Vec::new(),
                                sqlite_project_id: None,
                                sqlite_branch_count: 0,
                                container_names: Vec::new(),
                                local_state_service_count: 0,
                                local_state_branch_count: 0,
                            });
                    if !entry.sources.contains(&OrphanSource::Sqlite) {
                        entry.sources.push(OrphanSource::Sqlite);
                    }
                    entry.sqlite_project_id = Some(proj.id.clone());
                    entry.sqlite_branch_count = branch_count;
                }
            }
        }
    }

    // 3. Scan Docker containers ───────────────────────────────────────
    #[cfg(feature = "service-local")]
    {
        if let Ok(containers) = list_devflow_containers().await {
            // Build a set of known-live project names
            let live_names: HashSet<String> = if let Ok(mgr) = LocalStateManager::new() {
                mgr.list_all_projects()
                    .iter()
                    .filter(|(key, _)| Path::new(key.as_str()).exists())
                    .filter_map(|(key, _)| {
                        Path::new(key.as_str())
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                    })
                    .collect()
            } else {
                HashSet::new()
            };

            for (project_name, container_name) in &containers {
                if live_names.contains(project_name) {
                    continue;
                }

                let entry =
                    orphans
                        .entry(project_name.clone())
                        .or_insert_with(|| OrphanInfo {
                            project_name: project_name.clone(),
                            project_path: None,
                            sources: Vec::new(),
                            sqlite_project_id: None,
                            sqlite_branch_count: 0,
                            container_names: Vec::new(),
                            local_state_service_count: 0,
                            local_state_branch_count: 0,
                        });
                if !entry.sources.contains(&OrphanSource::Docker) {
                    entry.sources.push(OrphanSource::Docker);
                }
                if !entry.container_names.contains(container_name) {
                    entry.container_names.push(container_name.clone());
                }
            }
        }
    }

    let mut result: Vec<OrphanInfo> = orphans.into_values().collect();
    result.sort_by(|a, b| a.project_name.cmp(&b.project_name));
    Ok(result)
}

// ── Cleanup ─────────────────────────────────────────────────────────

/// Clean up all resources for a single orphaned project.
pub async fn cleanup_orphan(orphan: &OrphanInfo) -> CleanupResult {
    let mut result = CleanupResult {
        project_name: orphan.project_name.clone(),
        containers_removed: 0,
        sqlite_rows_deleted: false,
        local_state_cleared: false,
        data_dirs_removed: 0,
        errors: Vec::new(),
    };

    // 1. Remove Docker containers ─────────────────────────────────────
    #[cfg(feature = "service-local")]
    {
        for container_name in &orphan.container_names {
            match remove_docker_container(container_name).await {
                Ok(_) => result.containers_removed += 1,
                Err(e) => result
                    .errors
                    .push(format!("Failed to remove container '{}': {}", container_name, e)),
            }
        }

        // Also discover and remove containers by project name pattern
        if let Ok(containers) = list_devflow_containers().await {
            for (proj_name, container_name) in &containers {
                if proj_name == &orphan.project_name
                    && !orphan.container_names.contains(container_name)
                {
                    match remove_docker_container(container_name).await {
                        Ok(_) => result.containers_removed += 1,
                        Err(e) => result.errors.push(format!(
                            "Failed to remove container '{}': {}",
                            container_name, e
                        )),
                    }
                }
            }
        }
    }

    // 2. Delete SQLite project + branches + data dirs ─────────────────
    #[cfg(feature = "service-local")]
    {
        if let Some(ref project_id) = orphan.sqlite_project_id {
            if let Ok(store) = open_sqlite_store() {
                // First, clean up data directories for each branch
                if let Ok(branches) = store.list_branches(project_id) {
                    for branch in &branches {
                        let data_path = Path::new(&branch.data_dir);
                        if data_path.exists() {
                            match std::fs::remove_dir_all(data_path) {
                                Ok(_) => result.data_dirs_removed += 1,
                                Err(e) => result.errors.push(format!(
                                    "Failed to remove data dir '{}': {}",
                                    branch.data_dir, e
                                )),
                            }
                        }
                    }
                }

                // Also try to remove the project-level data directory
                if let Some(project_data_dir) = find_project_data_dir(project_id) {
                    if project_data_dir.exists() {
                        match std::fs::remove_dir_all(&project_data_dir) {
                            Ok(_) => result.data_dirs_removed += 1,
                            Err(e) => result.errors.push(format!(
                                "Failed to remove project data dir '{}': {}",
                                project_data_dir.display(),
                                e
                            )),
                        }
                    }
                }

                // Delete SQLite rows (CASCADE handles branches)
                match store.delete_project(project_id) {
                    Ok(_) => result.sqlite_rows_deleted = true,
                    Err(e) => result
                        .errors
                        .push(format!("Failed to delete SQLite project: {}", e)),
                }
            }
        }
    }

    // 3. Clear local state YAML entry ─────────────────────────────────
    if let Some(ref project_path) = orphan.project_path {
        if let Ok(mut state_mgr) = LocalStateManager::new() {
            match state_mgr.remove_project_by_key(project_path) {
                Ok(_) => result.local_state_cleared = true,
                Err(e) => result
                    .errors
                    .push(format!("Failed to clear local state: {}", e)),
            }
        }
    }

    // 4. Clear hook approvals ─────────────────────────────────────────
    if let Some(ref project_path) = orphan.project_path {
        if let Ok(mut approval_store) = crate::hooks::approval::ApprovalStore::load() {
            let _ = approval_store.clear_project(project_path);
        }
    }

    result
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Open the shared SQLite store used by the local Docker provider.
#[cfg(feature = "service-local")]
fn open_sqlite_store() -> Result<super::postgres::local::state::Store> {
    let data_root = dirs::data_local_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
        .join("devflow");
    let db_path = data_root.join("state.db");
    if !db_path.exists() {
        anyhow::bail!("SQLite state database does not exist at {}", db_path.display());
    }
    super::postgres::local::state::Store::open(&db_path)
}

/// Find the project-level data directory in the default data root.
#[cfg(feature = "service-local")]
fn find_project_data_dir(project_id: &str) -> Option<PathBuf> {
    let data_root = dirs::data_local_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
        .join("devflow")
        .join("projects")
        .join(project_id);
    Some(data_root)
}

/// List all Docker containers managed by devflow, returning (project_name, container_name) pairs.
#[cfg(feature = "service-local")]
async fn list_devflow_containers() -> Result<Vec<(String, String)>> {
    use bollard::query_parameters::ListContainersOptions;
    use bollard::Docker;

    let docker = Docker::connect_with_local_defaults()
        .context("failed to connect to Docker")?;

    let options = ListContainersOptions {
        all: true,
        ..Default::default()
    };

    let containers = docker
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

        if !is_managed {
            continue;
        }

        if let Some(names) = &container.names {
            for name in names {
                let clean_name = name.trim_start_matches('/');
                // Container naming convention: devflow-{project}-{service}-{branch}
                if let Some(rest) = clean_name.strip_prefix("devflow-") {
                    // Extract the project name (first segment before the next dash that
                    // starts the service name). We can't perfectly parse this since names
                    // can contain dashes, but we'll use the devflow.project label if available.
                    let project_name = container
                        .labels
                        .as_ref()
                        .and_then(|l| l.get("devflow.project"))
                        .cloned()
                        .unwrap_or_else(|| {
                            // Fallback: take the first segment
                            rest.split('-').next().unwrap_or("unknown").to_string()
                        });

                    result.push((project_name, clean_name.to_string()));
                }
            }
        }
    }

    Ok(result)
}

/// Force-remove a Docker container by name.
#[cfg(feature = "service-local")]
async fn remove_docker_container(container_name: &str) -> Result<()> {
    use bollard::query_parameters::RemoveContainerOptions;
    use bollard::Docker;

    let docker = Docker::connect_with_local_defaults()
        .context("failed to connect to Docker")?;

    // Stop first (ignore errors — may already be stopped)
    let _ = docker
        .stop_container(container_name, None::<bollard::query_parameters::StopContainerOptions>)
        .await;

    docker
        .remove_container(
            container_name,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await
        .with_context(|| format!("failed to remove container '{}'", container_name))?;

    Ok(())
}

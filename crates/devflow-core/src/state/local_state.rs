use crate::config::NamedServiceConfig;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalState {
    pub projects: HashMap<String, ProjectState>,
}

/// A devflow workspace — an abstraction above git workspaces.
/// Tracks parent-child relationships, worktree paths, and creation time
/// independently of the VCS provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevflowWorkspace {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// AI agent tool used for this workspace (e.g., "claude", "codex").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_tool: Option<String>,
    /// Agent status marker (e.g., "running", "idle", "done").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_status: Option<String>,
    /// When the agent was started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_started_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectState {
    pub last_updated: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<NamedServiceConfig>>,
    /// Registry of devflow workspaces tracked for this project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<Vec<DevflowWorkspace>>,
}

pub struct LocalStateManager {
    state_file_path: PathBuf,
    state: LocalState,
}

struct FileLockGuard {
    path: PathBuf,
}

impl Drop for FileLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_file_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", path.display()))
}

fn acquire_file_lock(lock_path: &Path) -> Result<FileLockGuard> {
    const MAX_ATTEMPTS: usize = 200;
    const SLEEP_MS: u64 = 25;
    const STALE_LOCK_SECS: u64 = 30;

    for _ in 0..MAX_ATTEMPTS {
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(lock_path)
        {
            Ok(_) => {
                return Ok(FileLockGuard {
                    path: lock_path.to_path_buf(),
                });
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                if let Ok(metadata) = fs::metadata(lock_path) {
                    if let Ok(modified) = metadata.modified() {
                        if modified.elapsed().unwrap_or_default().as_secs() > STALE_LOCK_SECS {
                            let _ = fs::remove_file(lock_path);
                            continue;
                        }
                    }
                }
                thread::sleep(Duration::from_millis(SLEEP_MS));
            }
            Err(e) => {
                let msg = format!("Failed to acquire lock '{}': {}", lock_path.display(), e);
                return Err(e).context(msg);
            }
        }
    }

    anyhow::bail!("Timed out waiting for lock file '{}'", lock_path.display())
}

impl LocalStateManager {
    pub fn new() -> Result<Self> {
        let state_file_path = Self::get_state_file_path()?;
        let state = Self::load_state(&state_file_path)?;

        Ok(Self {
            state_file_path,
            state,
        })
    }

    fn refresh_state(&mut self) -> Result<()> {
        self.state = Self::load_state(&self.state_file_path)?;
        Ok(())
    }

    pub fn get_services(&self, project_path: &Path) -> Option<Vec<NamedServiceConfig>> {
        let project_key = self.get_project_key(project_path)?;
        self.state
            .projects
            .get(&project_key)
            .and_then(|project| project.services.clone())
    }

    #[allow(dead_code)]
    pub fn set_services(
        &mut self,
        project_path: &Path,
        services: Vec<NamedServiceConfig>,
    ) -> Result<()> {
        self.refresh_state()?;

        let project_key = self.get_project_key(project_path).ok_or_else(|| {
            anyhow::anyhow!(
                "Failed to get project key for path: {}",
                project_path.display()
            )
        })?;

        let existing = self.state.projects.get(&project_key);
        let existing_branches = existing.and_then(|p| p.workspaces.clone());

        let project_state = ProjectState {
            last_updated: chrono::Utc::now(),
            services: Some(services),
            workspaces: existing_branches,
        };

        self.state.projects.insert(project_key, project_state);
        self.save_state()?;
        Ok(())
    }

    pub fn add_service(
        &mut self,
        project_path: &Path,
        service: NamedServiceConfig,
        force: bool,
    ) -> Result<()> {
        self.refresh_state()?;

        let project_key = self.get_project_key(project_path).ok_or_else(|| {
            anyhow::anyhow!(
                "Failed to get project key for path: {}",
                project_path.display()
            )
        })?;

        let existing = self.state.projects.get(&project_key);
        let existing_branches = existing.and_then(|p| p.workspaces.clone());
        let mut services = existing
            .and_then(|p| p.services.clone())
            .unwrap_or_default();

        if let Some(pos) = services.iter().position(|b| b.name == service.name) {
            if force {
                services[pos] = service;
            } else {
                anyhow::bail!(
                    "Service '{}' already exists. Use --force to overwrite.",
                    services[pos].name
                );
            }
        } else {
            let mut service = service;
            if services.is_empty() {
                service.default = true;
            }
            services.push(service);
        }

        let project_state = ProjectState {
            last_updated: chrono::Utc::now(),
            services: Some(services),
            workspaces: existing_branches,
        };

        self.state.projects.insert(project_key, project_state);
        self.save_state()?;
        Ok(())
    }

    pub fn remove_service(&mut self, project_path: &Path, name: &str) -> Result<()> {
        self.refresh_state()?;

        let project_key = self.get_project_key(project_path).ok_or_else(|| {
            anyhow::anyhow!(
                "Failed to get project key for path: {}",
                project_path.display()
            )
        })?;

        if let Some(project) = self.state.projects.get_mut(&project_key) {
            if let Some(ref mut services) = project.services {
                services.retain(|b| b.name != name);
            }
            project.last_updated = chrono::Utc::now();
            self.save_state()?;
        }

        Ok(())
    }

    /// Remove an entire project from the local state (workspace registry + services).
    pub fn remove_project(&mut self, project_path: &Path) -> Result<()> {
        self.refresh_state()?;

        let project_key = self.get_project_key(project_path).ok_or_else(|| {
            anyhow::anyhow!(
                "Failed to get project key for path: {}",
                project_path.display()
            )
        })?;

        self.state.projects.remove(&project_key);
        self.save_state()?;
        Ok(())
    }

    /// Remove a project by its raw key (canonical path string).
    ///
    /// Unlike [`remove_project`] this does **not** call `canonicalize()`, so it
    /// works even when the directory no longer exists on disk — which is exactly
    /// the situation during orphan cleanup.
    pub fn remove_project_by_key(&mut self, project_key: &str) -> Result<()> {
        self.refresh_state()?;
        self.state.projects.remove(project_key);
        self.save_state()?;
        Ok(())
    }

    /// Return a snapshot of **all** projects in the local state, keyed by their
    /// canonical path.  Used by orphan detection to iterate over every known
    /// project.
    pub fn list_all_projects(&self) -> HashMap<String, ProjectState> {
        self.state.projects.clone()
    }

    // ── Workspace registry CRUD ────────────────────────────────────────

    /// Get all registered devflow workspaces for a project.
    pub fn get_workspaces(&self, project_path: &Path) -> Vec<DevflowWorkspace> {
        self.get_project_key(project_path)
            .and_then(|key| self.state.projects.get(&key))
            .and_then(|p| p.workspaces.clone())
            .unwrap_or_default()
    }

    /// Look up a single registered workspace by name.
    pub fn get_workspace(&self, project_path: &Path, name: &str) -> Option<DevflowWorkspace> {
        self.get_workspaces(project_path)
            .into_iter()
            .find(|b| b.name == name)
    }

    /// Register (upsert) a devflow workspace in the registry.
    /// If a workspace with the same name exists, it is updated.
    pub fn register_workspace(
        &mut self,
        project_path: &Path,
        workspace: DevflowWorkspace,
    ) -> Result<()> {
        self.refresh_state()?;

        let project_key = self.get_project_key(project_path).ok_or_else(|| {
            anyhow::anyhow!(
                "Failed to get project key for path: {}",
                project_path.display()
            )
        })?;

        let project = self
            .state
            .projects
            .entry(project_key)
            .or_insert_with(|| ProjectState {
                last_updated: chrono::Utc::now(),
                services: None,
                workspaces: None,
            });

        let workspaces = project.workspaces.get_or_insert_with(Vec::new);

        if let Some(pos) = workspaces.iter().position(|b| b.name == workspace.name) {
            workspaces[pos] = workspace;
        } else {
            workspaces.push(workspace);
        }

        project.last_updated = chrono::Utc::now();
        self.save_state()?;
        Ok(())
    }

    // ── Convenience methods that accept a project directory ────────
    //
    // The standard CRUD methods above expect the `.devflow.yml` config file
    // path (because `get_project_key` strips the last component).  These
    // `_by_dir` variants accept the **project directory** and append
    // `.devflow.yml` internally, eliminating a common source of bugs.

    /// Get all registered devflow workspaces for a project directory.
    pub fn get_workspaces_by_dir(&self, project_dir: &Path) -> Vec<DevflowWorkspace> {
        self.get_workspaces(&project_dir.join(".devflow.yml"))
    }

    /// Look up a single registered workspace by name (project directory variant).
    pub fn get_workspace_by_dir(&self, project_dir: &Path, name: &str) -> Option<DevflowWorkspace> {
        self.get_workspace(&project_dir.join(".devflow.yml"), name)
    }

    /// Register (upsert) a devflow workspace (project directory variant).
    pub fn register_workspace_by_dir(
        &mut self,
        project_dir: &Path,
        workspace: DevflowWorkspace,
    ) -> Result<()> {
        self.register_workspace(&project_dir.join(".devflow.yml"), workspace)
    }

    /// Remove a workspace from the registry by name (project directory variant).
    pub fn unregister_workspace_by_dir(&mut self, project_dir: &Path, name: &str) -> Result<()> {
        self.unregister_workspace(&project_dir.join(".devflow.yml"), name)
    }

    /// Get project workspaces, initializing the default workspace when empty.
    ///
    /// This is the common workspace-loading path used by CLI/TUI/GUI so all
    /// surfaces share the same bootstrap behavior.
    pub fn get_or_init_workspaces_by_dir(
        &mut self,
        project_dir: &Path,
        main_workspace: &str,
    ) -> Result<Vec<DevflowWorkspace>> {
        let workspaces = self.get_workspaces_by_dir(project_dir);
        if !workspaces.is_empty() {
            return Ok(workspaces);
        }

        self.ensure_default_workspace(project_dir, main_workspace)?;
        Ok(self.get_workspaces_by_dir(project_dir))
    }

    /// Ensure a default devflow workspace exists for this project.
    ///
    /// If the workspace registry is empty, registers `main_workspace` as the
    /// default workspace with `created_at = now` and no parent.  This is the
    /// auto-migration path for projects created before the registry existed.
    pub fn ensure_default_workspace(
        &mut self,
        project_dir: &Path,
        main_workspace: &str,
    ) -> Result<()> {
        let config_path = project_dir.join(".devflow.yml");
        let existing = self.get_workspaces(&config_path);
        if !existing.is_empty() {
            return Ok(());
        }

        let workspace = DevflowWorkspace {
            name: main_workspace.to_string(),
            parent: None,
            worktree_path: None,
            created_at: chrono::Utc::now(),
            agent_tool: None,
            agent_status: None,
            agent_started_at: None,
        };

        self.register_workspace(&config_path, workspace)
    }

    /// Remove a workspace from the registry by name.
    pub fn unregister_workspace(&mut self, project_path: &Path, name: &str) -> Result<()> {
        self.refresh_state()?;

        let project_key = self.get_project_key(project_path).ok_or_else(|| {
            anyhow::anyhow!(
                "Failed to get project key for path: {}",
                project_path.display()
            )
        })?;

        if let Some(project) = self.state.projects.get_mut(&project_key) {
            if let Some(ref mut workspaces) = project.workspaces {
                workspaces.retain(|b| b.name != name);
            }
            project.last_updated = chrono::Utc::now();
            self.save_state()?;
        }

        Ok(())
    }

    #[allow(dead_code)]
    pub fn cleanup_old_projects(&mut self, max_age_days: u32) -> Result<()> {
        self.refresh_state()?;

        let cutoff = chrono::Utc::now() - chrono::Duration::days(max_age_days as i64);

        let old_projects: Vec<String> = self
            .state
            .projects
            .iter()
            .filter(|(_, project)| project.last_updated < cutoff)
            .map(|(key, _)| key.clone())
            .collect();

        let mut projects_removed = false;
        for project_key in &old_projects {
            // Check if project still exists before removing
            if let Ok(path) = PathBuf::from(&project_key).canonicalize() {
                if !path.exists() {
                    log::debug!("Removing state for non-existent project: {}", project_key);
                    self.state.projects.remove(project_key);
                    projects_removed = true;
                }
            } else {
                log::debug!("Removing state for inaccessible project: {}", project_key);
                self.state.projects.remove(project_key);
                projects_removed = true;
            }
        }

        if projects_removed {
            self.save_state()?;
        }

        Ok(())
    }

    fn get_project_key(&self, project_path: &Path) -> Option<String> {
        // Use the canonical path of the directory containing .devflow.yml as the project key
        project_path
            .parent()
            .and_then(|dir| dir.canonicalize().ok())
            .map(|canonical_path| canonical_path.to_string_lossy().to_string())
    }

    /// Public accessor for the project key, used by `devflow destroy` to clear hook approvals.
    pub fn get_project_key_for(&self, project_path: &Path) -> Option<String> {
        self.get_project_key(project_path)
    }

    fn get_state_file_path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .context("Failed to get user config directory")?
            .join("devflow");

        // Ensure the config directory exists
        fs::create_dir_all(&config_dir).with_context(|| {
            format!(
                "Failed to create config directory: {}",
                config_dir.display()
            )
        })?;

        Ok(config_dir.join("local_state.yml"))
    }

    fn load_state(state_file_path: &Path) -> Result<LocalState> {
        if !state_file_path.exists() {
            log::debug!("Local state file does not exist, creating new state");
            return Ok(LocalState::default());
        }

        let content = fs::read_to_string(state_file_path).with_context(|| {
            format!(
                "Failed to read local state file: {}",
                state_file_path.display()
            )
        })?;

        let state: LocalState = serde_yaml_ng::from_str(&content).with_context(|| {
            format!(
                "Failed to parse local state file: {}",
                state_file_path.display()
            )
        })?;

        log::debug!("Loaded local state with {} projects", state.projects.len());
        Ok(state)
    }

    fn save_state(&self) -> Result<()> {
        let lock_path = lock_file_path(&self.state_file_path);
        let _lock = acquire_file_lock(&lock_path)?;

        let content = serde_yaml_ng::to_string(&self.state)
            .context("Failed to serialize local state to YAML")?;

        let tmp_path = PathBuf::from(format!(
            "{}.tmp.{}",
            self.state_file_path.display(),
            std::process::id()
        ));

        fs::write(&tmp_path, content).with_context(|| {
            format!(
                "Failed to write temporary local state file: {}",
                tmp_path.display()
            )
        })?;

        fs::rename(&tmp_path, &self.state_file_path).with_context(|| {
            format!(
                "Failed to write local state file: {}",
                self.state_file_path.display()
            )
        })?;

        log::debug!("Saved local state to: {}", self.state_file_path.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_project_key_generation() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join(".devflow.yml");

        let manager = LocalStateManager::new().unwrap();
        let project_key = manager.get_project_key(&config_path);

        assert!(project_key.is_some());
        assert!(project_key
            .unwrap()
            .contains(temp_dir.path().to_str().unwrap()));
    }
}

//! Project-level lifecycle operations shared by every frontend.
//!
//! `devflow destroy` (CLI) and the GUI's Danger Zone previously each carried
//! their own teardown sequence and drifted apart (the GUI forgot process
//! state, the two disagreed on config loading). This module is the single
//! implementation; frontends only render the [`DestroyPlan`] for confirmation
//! and the [`DestroyOutcome`] afterwards.

use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::config::{Config, NamedServiceConfig};
use crate::hooks::approval::ApprovalStore;
use crate::processes::{self, ProcessResult};
use crate::services;
use crate::state::LocalStateManager;
use crate::vcs;

/// Options for [`destroy`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DestroyOptions {
    /// Remove worktrees even when the VCS refuses (e.g. uncommitted changes),
    /// falling back to plain filesystem deletion.
    pub force_worktrees: bool,
}

/// Progress callback receiving one human-readable line per teardown step.
/// Pass `None` when no live feedback is wanted (GUI, tests).
pub type DestroyProgress<'a> = Option<&'a (dyn Fn(&str) + Send + Sync)>;

fn emit(progress: DestroyProgress<'_>, message: String) {
    if let Some(callback) = progress {
        callback(&message);
    }
}

/// What a destroy would touch. Frontends use this for confirmation prompts so
/// the preview cannot drift from what [`destroy`] actually does.
#[derive(Debug, Clone, Serialize)]
pub struct DestroyPlan {
    pub project_name: String,
    pub services: Vec<String>,
    pub worktrees: Vec<PathBuf>,
    pub has_vcs: bool,
    pub config_path: Option<PathBuf>,
    pub local_config_path: Option<PathBuf>,
}

/// Per-service destroy outcome.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceDestroyOutcome {
    pub name: String,
    pub success: bool,
    pub workspaces_destroyed: Vec<String>,
    pub error: Option<String>,
}

/// Result of a project destroy.
#[derive(Debug, Clone, Serialize)]
pub struct DestroyOutcome {
    pub project_name: String,
    pub processes_stopped: usize,
    pub process_results: Vec<ProcessResult>,
    pub services_destroyed: Vec<ServiceDestroyOutcome>,
    pub worktrees_removed: usize,
    pub hooks_uninstalled: bool,
    pub state_cleared: bool,
    pub config_deleted: bool,
    pub local_config_deleted: bool,
}

/// Describe what [`destroy`] would tear down for the project at `project_dir`.
pub fn destroy_plan(project_dir: &Path) -> Result<DestroyPlan> {
    let config = Config::load_effective_for_dir(project_dir)?;
    let vcs_repo = vcs::detect_vcs_provider(project_dir).ok();
    let worktrees = vcs_repo
        .as_ref()
        .and_then(|repo| repo.list_worktrees().ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|wt| !wt.is_main)
        .map(|wt| wt.path)
        .collect();
    let local_config_path = project_dir.join(".devflow.local.yml");
    Ok(DestroyPlan {
        project_name: config.project_name(),
        services: config
            .resolve_services()
            .iter()
            .map(|svc| svc.name.clone())
            .collect(),
        worktrees,
        has_vcs: vcs_repo.is_some(),
        config_path: Config::find_config_file_in(project_dir),
        local_config_path: local_config_path.exists().then_some(local_config_path),
    })
}

/// Destroy a devflow project and all associated resources.
///
/// This is the inverse of `devflow init`. It removes:
///   1. Workspace processes and their persisted runtime state
///   2. All service data (containers, databases, workspaces)
///   3. Worktrees created by devflow
///   4. VCS hooks installed by devflow
///   5. Hook approvals for this project
///   6. Workspace registry and local state for this project
///   7. Configuration files (committed config + `.devflow.local.yml`)
///
/// Individual step failures are logged and reflected in the outcome instead
/// of aborting: a half-destroyed project should end up as destroyed as
/// possible rather than wedged.
pub async fn destroy(
    project_dir: &Path,
    options: DestroyOptions,
    progress: DestroyProgress<'_>,
) -> Result<DestroyOutcome> {
    let config = Config::load_effective_for_dir(project_dir)?;
    let project_name = config.project_name();
    let vcs_repo = vcs::detect_vcs_provider(project_dir).ok();
    // Local state and hook approvals are keyed by the committed config path.
    let state_key = Config::find_config_file_in(project_dir)
        .unwrap_or_else(|| project_dir.join(".devflow.yml"));

    // 1. Stop workspace processes and purge their persisted runtime state.
    // Deliberately independent of the current `processes` config section:
    // runtime-only records from an earlier config must not survive a destroy
    // and resurface as ghost processes after a re-init.
    let process_results = processes::destroy_project_process_state(&config, project_dir).await;
    let processes_stopped = process_results.iter().filter(|r| r.success).count();
    if processes_stopped > 0 {
        emit(
            progress,
            format!("Stopped {} workspace process(es)", processes_stopped),
        );
    }
    for result in process_results.iter().filter(|r| !r.success) {
        log::warn!(
            "Process cleanup during destroy: {}: {}",
            result.process,
            result.message
        );
    }

    // 2. Destroy all service data.
    let mut services_destroyed = Vec::new();
    for svc_config in &config.resolve_services() {
        emit(
            progress,
            format!("Destroying service '{}'...", svc_config.name),
        );
        let outcome = destroy_one_service(&config, svc_config).await;
        match &outcome.error {
            Some(error) => {
                log::warn!("Failed to destroy service '{}': {}", outcome.name, error);
                emit(
                    progress,
                    format!("  Warning: could not destroy '{}': {}", outcome.name, error),
                );
            }
            None => emit(
                progress,
                format!(
                    "  Destroyed '{}': {} workspace(es) removed",
                    outcome.name,
                    outcome.workspaces_destroyed.len()
                ),
            ),
        }
        services_destroyed.push(outcome);
    }

    // 3. Remove worktrees — never silently rm-rf one the VCS refuses to drop
    // (uncommitted work) unless the caller forced it.
    let mut worktrees_removed = 0usize;
    if let Some(ref repo) = vcs_repo {
        if let Ok(worktrees) = repo.list_worktrees() {
            for wt in worktrees.iter().filter(|wt| !wt.is_main) {
                emit(
                    progress,
                    format!("Removing worktree: {}", wt.path.display()),
                );
                match repo.remove_worktree(&wt.path, options.force_worktrees) {
                    Ok(()) => worktrees_removed += 1,
                    Err(e) if !options.force_worktrees => {
                        log::warn!(
                            "Skipping worktree '{}' during destroy: {}",
                            wt.path.display(),
                            e
                        );
                        emit(progress, format!("  Skipping {}: {}", wt.path.display(), e));
                    }
                    Err(e) => {
                        log::warn!("Failed to remove worktree via VCS: {}", e);
                        if wt.path.exists() {
                            if let Err(e2) = std::fs::remove_dir_all(&wt.path) {
                                log::warn!("Failed to remove worktree directory: {}", e2);
                                emit(
                                    progress,
                                    format!(
                                        "  Warning: could not remove {}: {}",
                                        wt.path.display(),
                                        e2
                                    ),
                                );
                                continue;
                            }
                        }
                        worktrees_removed += 1;
                    }
                }
            }
        }
    }

    // 4. Uninstall VCS hooks.
    let mut hooks_uninstalled = false;
    if let Some(ref repo) = vcs_repo {
        match repo.uninstall_hooks() {
            Ok(_) => {
                hooks_uninstalled = true;
                emit(progress, "Uninstalled VCS hooks".to_string());
            }
            Err(e) => log::warn!("Failed to uninstall hooks: {}", e),
        }
    }

    // 5. Clear hook approvals (before the registry entry disappears).
    if let Ok(state_mgr) = LocalStateManager::new() {
        if let Some(project_key) = state_mgr.get_project_key_for(&state_key) {
            if let Ok(mut store) = ApprovalStore::load() {
                if let Err(e) = store.clear_project(&project_key) {
                    log::warn!("Failed to clear hook approvals: {}", e);
                }
            }
        }
    }

    // 6. Clear local state (workspace registry, services, current workspace).
    let mut state_cleared = false;
    match LocalStateManager::new() {
        Ok(mut state_mgr) => match state_mgr.remove_project(&state_key) {
            Ok(()) => {
                state_cleared = true;
                emit(
                    progress,
                    "Cleared project state and workspace registry".to_string(),
                );
            }
            Err(e) => log::warn!("Failed to clear project state: {}", e),
        },
        Err(e) => log::warn!("Failed to open local state: {}", e),
    }

    // 7. Delete config files.
    let mut config_deleted = false;
    if let Some(config_path) = Config::find_config_file_in(project_dir) {
        match std::fs::remove_file(&config_path) {
            Ok(()) => {
                config_deleted = true;
                emit(progress, format!("Deleted {}", config_path.display()));
            }
            Err(e) => log::warn!("Failed to delete config file: {}", e),
        }
    }
    let mut local_config_deleted = false;
    let local_config_path = project_dir.join(".devflow.local.yml");
    if local_config_path.exists() {
        match std::fs::remove_file(&local_config_path) {
            Ok(()) => {
                local_config_deleted = true;
                emit(progress, format!("Deleted {}", local_config_path.display()));
            }
            Err(e) => log::warn!("Failed to delete local config file: {}", e),
        }
    }

    Ok(DestroyOutcome {
        project_name,
        processes_stopped,
        process_results,
        services_destroyed,
        worktrees_removed,
        hooks_uninstalled,
        state_cleared,
        config_deleted,
        local_config_deleted,
    })
}

async fn destroy_one_service(
    config: &Config,
    svc_config: &NamedServiceConfig,
) -> ServiceDestroyOutcome {
    let provider =
        match services::factory::create_provider_from_named_config(config, svc_config).await {
            Ok(provider) => provider,
            Err(e) => {
                return ServiceDestroyOutcome {
                    name: svc_config.name.clone(),
                    success: false,
                    workspaces_destroyed: Vec::new(),
                    error: Some(e.to_string()),
                }
            }
        };

    if provider.supports_destroy() {
        match provider.destroy_project().await {
            Ok(workspaces) => ServiceDestroyOutcome {
                name: svc_config.name.clone(),
                success: true,
                workspaces_destroyed: workspaces,
                error: None,
            },
            Err(e) => ServiceDestroyOutcome {
                name: svc_config.name.clone(),
                success: false,
                workspaces_destroyed: Vec::new(),
                error: Some(e.to_string()),
            },
        }
    } else {
        // Provider has no project-level destroy — delete workspaces one by one.
        match provider.list_workspaces().await {
            Ok(workspaces) => {
                let mut deleted = Vec::new();
                for workspace in &workspaces {
                    match provider.delete_workspace(&workspace.name).await {
                        Ok(_) => deleted.push(workspace.name.clone()),
                        Err(e) => log::warn!(
                            "Failed to delete workspace '{}' on '{}': {}",
                            workspace.name,
                            svc_config.name,
                            e
                        ),
                    }
                }
                ServiceDestroyOutcome {
                    name: svc_config.name.clone(),
                    success: true,
                    workspaces_destroyed: deleted,
                    error: None,
                }
            }
            Err(e) => ServiceDestroyOutcome {
                name: svc_config.name.clone(),
                success: false,
                workspaces_destroyed: Vec::new(),
                error: Some(e.to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processes::PROCESS_TEST_ENV_LOCK;
    use tempfile::TempDir;

    #[tokio::test]
    #[cfg(unix)]
    #[allow(clippy::await_holding_lock)]
    async fn destroy_stops_processes_and_removes_all_project_artifacts() {
        let _guard = PROCESS_TEST_ENV_LOCK.lock().unwrap();
        let project = TempDir::new().unwrap();
        let process_state = TempDir::new().unwrap();
        let config_dir = TempDir::new().unwrap();
        std::env::set_var("DEVFLOW_PROCESS_STATE_DIR", process_state.path());
        std::env::set_var("DEVFLOW_CONFIG_DIR", config_dir.path());

        std::fs::write(
            project.path().join(".devflow.yml"),
            "name: doomed-app\nprocesses:\n  daemons:\n    napper:\n      run: \"sleep 30\"\n",
        )
        .unwrap();
        std::fs::write(project.path().join(".devflow.local.yml"), "{}\n").unwrap();

        let plan = destroy_plan(project.path()).unwrap();
        assert_eq!(plan.project_name, "doomed-app");
        assert!(plan.services.is_empty());
        assert!(plan.worktrees.is_empty());
        assert!(plan.config_path.is_some());
        assert!(plan.local_config_path.is_some());

        // Start a real process through the public runtime API.
        let config = Config::load_effective_for_dir(project.path()).unwrap();
        let results =
            processes::start_workspace_processes(&config, project.path(), "main", &[], false)
                .await
                .unwrap();
        assert!(results.iter().all(|r| r.success), "{:?}", results);
        let pid = results[0].pid.expect("started process has a pid");

        let outcome = destroy(project.path(), DestroyOptions::default(), None)
            .await
            .unwrap();

        assert_eq!(outcome.project_name, "doomed-app");
        assert_eq!(
            outcome.processes_stopped, 1,
            "{:?}",
            outcome.process_results
        );
        assert!(outcome.services_destroyed.is_empty());
        assert_eq!(outcome.worktrees_removed, 0);
        assert!(outcome.state_cleared);
        assert!(outcome.config_deleted);
        assert!(outcome.local_config_deleted);

        assert!(!project.path().join(".devflow.yml").exists());
        assert!(!project.path().join(".devflow.local.yml").exists());
        // The process was killed and the whole per-project state dir is gone.
        assert!(!crate::processes::process_alive(pid));
        assert_eq!(
            std::fs::read_dir(process_state.path()).unwrap().count(),
            0,
            "process state dir should be empty after destroy"
        );

        std::env::remove_var("DEVFLOW_PROCESS_STATE_DIR");
        std::env::remove_var("DEVFLOW_CONFIG_DIR");
    }
}

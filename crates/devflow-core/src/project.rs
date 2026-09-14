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
    /// Explicitly accept dirty-worktree deletion; ownership and locks still apply.
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
    pub resources: Vec<ResourceDestroyOutcome>,
    pub processes_stopped: usize,
    pub process_results: Vec<ProcessResult>,
    pub services_destroyed: Vec<ServiceDestroyOutcome>,
    pub worktrees_removed: usize,
    pub hooks_uninstalled: bool,
    pub state_cleared: bool,
    pub config_deleted: bool,
    pub local_config_deleted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceDestroyOutcome {
    pub kind: String,
    pub resource: String,
    pub success: bool,
    pub error: Option<String>,
}
fn resource(kind: &str, name: impl Into<String>, result: Result<()>) -> ResourceDestroyOutcome {
    ResourceDestroyOutcome {
        kind: kind.into(),
        resource: name.into(),
        success: result.is_ok(),
        error: result.err().map(|e| format!("{e:#}")),
    }
}
fn optional_vcs(project: &Path) -> Result<Option<Box<dyn vcs::VcsProvider>>> {
    vcs::detect_vcs_kind(project)
        .map(|_| vcs::detect_vcs_provider(project))
        .transpose()
}

impl DestroyOutcome {
    pub fn succeeded(&self) -> bool {
        self.state_cleared
            && self.resources.iter().all(|resource| resource.success)
            && self
                .services_destroyed
                .iter()
                .all(|service| service.success)
            && self
                .process_results
                .iter()
                .all(|process| process.success || !process.required)
    }
}

/// Describe what [`destroy`] would tear down for the project at `project_dir`.
pub fn destroy_plan(project_dir: &Path) -> Result<DestroyPlan> {
    let config = Config::load_effective_for_dir(project_dir)?;
    let vcs_repo = optional_vcs(project_dir)?;
    let worktrees = vcs_repo
        .as_ref()
        .map(|repo| repo.list_worktrees())
        .transpose()?
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
    let _policy_lease =
        crate::workspace::operation::project_policy_lease(project_dir, true).await?;
    let config = Config::load_effective_for_dir(project_dir)?;
    let project_name = config.project_name();
    let vcs_repo = optional_vcs(project_dir)?;
    let worktrees = vcs_repo
        .as_ref()
        .map(|repo| repo.list_worktrees())
        .transpose()?
        .unwrap_or_default();
    let mut resources = Vec::new();
    // Validate every checkout before stopping a process or deleting data.
    if let Some(repo) = &vcs_repo {
        anyhow::ensure!(
            !repo.is_worktree(),
            "Destroy must run from the primary checkout"
        );
        for wt in worktrees.iter().filter(|wt| !wt.is_main) {
            anyhow::ensure!(
                !repo.worktree_is_locked(&wt.path)?,
                "Worktree {} is locked",
                wt.path.display()
            );
            if !options.force_worktrees {
                anyhow::ensure!(!repo.worktree_is_dirty(&wt.path)?, "Worktree {} has uncommitted changes; preserve them or explicitly force deletion", wt.path.display());
            }
        }
    }
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

    if services_destroyed.iter().any(|service| !service.success)
        || process_results
            .iter()
            .any(|process| !process.success && process.required)
    {
        // Keep configuration and ownership references so failed deletions can be retried.
        return Ok(DestroyOutcome {
            project_name,
            resources,
            processes_stopped,
            process_results,
            services_destroyed,
            worktrees_removed: 0,
            hooks_uninstalled: false,
            state_cleared: false,
            config_deleted: false,
            local_config_deleted: false,
        });
    }

    // Remove only paths still owned by the VCS. Keep state/config on any failure.
    let mut worktrees_removed = 0;
    if let Some(repo) = &vcs_repo {
        for wt in worktrees.iter().filter(|wt| !wt.is_main) {
            let result = resource(
                "worktree",
                wt.path.display().to_string(),
                repo.remove_worktree(&wt.path, options.force_worktrees),
            );
            if result.success {
                worktrees_removed += 1;
            }
            resources.push(result);
        }
    }
    let mut hooks_uninstalled = false;
    if resources.iter().all(|r| r.success) {
        if let Some(repo) = &vcs_repo {
            let result = resource("hooks", "managed Git hooks", repo.uninstall_hooks());
            hooks_uninstalled = result.success;
            resources.push(result);
        }
    }
    let mut config_deleted = false;
    let mut local_config_deleted = false;
    let mut state_cleared = false;
    if resources.iter().all(|r| r.success) {
        // Read all fallible state before removing the configuration needed for retry.
        let mut state = LocalStateManager::new()?;
        if let Some(key) = state.get_project_key_for(&state_key) {
            resources.push(resource(
                "approvals",
                &key,
                ApprovalStore::load().and_then(|mut store| store.clear_project(&key).map(|_| ())),
            ));
        }
        if resources.iter().all(|r| r.success) {
            // Save recoverable contents before staging; only discard ownership
            // after both configuration deletions succeed. A failure restores
            // the configuration so the remaining cleanup can be retried.
            let paths = [
                Config::find_config_file_in(project_dir),
                project_dir
                    .join(".devflow.local.yml")
                    .try_exists()?
                    .then(|| project_dir.join(".devflow.local.yml")),
            ];
            let backups = paths
                .iter()
                .enumerate()
                .filter_map(|(index, path)| path.as_ref().map(|path| (index, path)))
                .map(|(index, path)| -> Result<_> {
                    Ok((
                        index,
                        path.clone(),
                        std::fs::read(path)?,
                        std::fs::symlink_metadata(path)?.permissions(),
                        std::fs::read_link(path).ok(),
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            let mut staged = Vec::new();
            for (index, path, _, _, _) in &backups {
                let stage =
                    path.with_file_name(format!(".devflow-destroy-{}", uuid::Uuid::new_v4()));
                match std::fs::rename(path, &stage) {
                    Ok(()) => staged.push((*index, path.clone(), stage)),
                    Err(error) => {
                        resources.push(resource(
                            "config",
                            path.display().to_string(),
                            Err(error.into()),
                        ));
                        break;
                    }
                }
            }
            if resources.iter().all(|r| r.success) {
                for (index, original, stage) in &staged {
                    let result = resource(
                        "config",
                        original.display().to_string(),
                        std::fs::remove_file(stage).map_err(Into::into),
                    );
                    if *index == 0 {
                        config_deleted = result.success;
                    } else {
                        local_config_deleted = result.success;
                    }
                    resources.push(result);
                }
            }
            if resources.iter().all(|r| r.success) {
                let result = resource(
                    "state",
                    "project registry",
                    state.remove_project(&state_key),
                );
                state_cleared = result.success;
                resources.push(result);
            }
            if !state_cleared {
                for (index, original, stage) in &staged {
                    let restore = (|| -> Result<()> {
                        anyhow::ensure!(
                            std::fs::symlink_metadata(original)
                                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
                            "Configuration path was replaced during cleanup: {}",
                            original.display()
                        );
                        if std::fs::symlink_metadata(stage).is_ok() {
                            return Ok(std::fs::rename(stage, original)?);
                        }
                        let (_, _, bytes, permissions, link) =
                            backups.iter().find(|b| b.0 == *index).unwrap();
                        if let Some(target) = link {
                            #[cfg(unix)]
                            {
                                std::os::unix::fs::symlink(target, original)?;
                            }
                            #[cfg(windows)]
                            {
                                std::os::windows::fs::symlink_file(target, original)?;
                            }
                        } else {
                            use std::io::Write;
                            let mut file = std::fs::OpenOptions::new()
                                .write(true)
                                .create_new(true)
                                .open(original)?;
                            file.write_all(bytes)?;
                            file.set_permissions(permissions.clone())?;
                            file.sync_all()?;
                        }
                        Ok(())
                    })();
                    if restore.is_ok() {
                        if *index == 0 {
                            config_deleted = false;
                        } else {
                            local_config_deleted = false;
                        }
                    }
                    resources.push(resource(
                        "config_restore",
                        original.display().to_string(),
                        restore,
                    ));
                }
            }
        }
    }

    Ok(DestroyOutcome {
        project_name,
        resources,
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
                let mut failures = Vec::new();
                let mut pending = workspaces;
                while !pending.is_empty() {
                    let Some(index) = pending.iter().position(|w| {
                        !pending
                            .iter()
                            .any(|child| child.parent_workspace.as_deref() == Some(w.name.as_str()))
                    }) else {
                        failures.push(
                            "cyclic provider data dependencies prevent ordered cleanup".into(),
                        );
                        break;
                    };
                    let workspace = pending.remove(index);
                    match provider.delete_workspace(&workspace.name).await {
                        Ok(_) => deleted.push(workspace.name.clone()),
                        Err(error) => failures.push(format!("{}: {error:#}", workspace.name)),
                    }
                }
                ServiceDestroyOutcome {
                    name: svc_config.name.clone(),
                    success: failures.is_empty(),
                    workspaces_destroyed: deleted,
                    error: (!failures.is_empty()).then(|| failures.join("; ")),
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

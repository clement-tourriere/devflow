use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Config;
use crate::hooks::HookPhase;
use crate::processes;
use crate::services;
use crate::state::LocalStateManager;
use crate::vcs;

use super::hooks::{run_lifecycle_hooks, run_lifecycle_hooks_best_effort};
use super::{DeleteWorkspaceResult, LifecycleOptions, ServiceResult};

/// Options specific to workspace deletion.
#[derive(Debug, Clone, Default)]
pub struct DeleteOptions {
    /// Shared lifecycle options.
    pub lifecycle: LifecycleOptions,
    /// Whether to keep service workspaces (don't delete databases, etc.).
    pub keep_services: bool,
    /// Discard a dirty worktree and continue through partial cleanup failures.
    pub force: bool,
}

/// A safety issue discovered before workspace deletion begins.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeletePreflightIssue {
    /// Stable machine-readable issue identifier.
    pub code: String,
    pub message: String,
    /// Whether explicit force is allowed to override this issue.
    pub force_overridable: bool,
}

/// Non-mutating deletion safety report shared by every frontend.
#[derive(Debug, Clone, Serialize)]
pub struct DeleteWorkspacePreflight {
    pub workspace: String,
    pub service_key: String,
    pub worktree_path: Option<String>,
    pub vcs_ref_exists: bool,
    pub issues: Vec<DeletePreflightIssue>,
}

impl DeleteWorkspacePreflight {
    pub fn can_delete(&self, force: bool) -> bool {
        self.issues
            .iter()
            .all(|issue| force && issue.force_overridable)
    }
}

/// Inspect all destructive workspace conditions without changing VCS, state,
/// services, processes, or files.
pub fn preflight_delete_workspace(
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
) -> Result<DeleteWorkspacePreflight> {
    let registered = LocalStateManager::new()
        .ok()
        .and_then(|state| state.get_workspace_by_dir(project_dir, workspace_name));
    let registered_path = registered
        .as_ref()
        .and_then(|workspace| workspace.worktree_path.as_ref())
        .map(PathBuf::from);

    preflight_delete_workspace_with_registered_path(
        config,
        project_dir,
        workspace_name,
        registered_path,
    )
}

fn preflight_delete_workspace_with_registered_path(
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
    registered_path: Option<PathBuf>,
) -> Result<DeleteWorkspacePreflight> {
    super::validate_workspace_name(workspace_name).map_err(anyhow::Error::msg)?;
    // Resolve before any destructive action. Legacy keys are retained only
    // when their raw owner is unambiguous; unresolved ownership cannot be
    // overridden with `--force`.
    let service_key = LocalStateManager::new()?
        .resolve_workspace_service_key_by_dir(project_dir, workspace_name)?;
    let vcs_provider = vcs::detect_vcs_provider(project_dir).ok();
    let live_path = vcs_provider
        .as_ref()
        .and_then(|repo| repo.worktree_path(workspace_name).ok().flatten());
    let path_owned_by_vcs = live_path.is_some();
    let worktree_path = live_path.or(registered_path);

    let mut issues = Vec::new();
    let physical_git_primary = vcs_provider
        .as_ref()
        .map(|repo| super::invariant::inspect_git_primary_workspace(repo.as_ref()))
        .transpose()?
        .flatten()
        .and_then(|primary| primary.workspace);
    if workspace_name == config.git.main_workspace {
        issues.push(DeletePreflightIssue {
            code: "default_workspace".to_string(),
            message: format!("Cannot remove the default workspace '{}'", workspace_name),
            force_overridable: false,
        });
    } else if physical_git_primary.as_deref() == Some(workspace_name) {
        issues.push(DeletePreflightIssue {
            code: "primary_workspace".to_string(),
            message: format!(
                "Cannot remove workspace '{}' because it occupies Git's physical primary checkout, even though .devflow.yml configures '{}' as the default. Either check out '{}' in the primary checkout or update git.main_workspace in .devflow.yml to '{}'.",
                workspace_name,
                config.git.main_workspace,
                config.git.main_workspace,
                workspace_name,
            ),
            force_overridable: false,
        });
    }

    let current = vcs_provider
        .as_ref()
        .and_then(|repo| repo.current_workspace().ok().flatten());
    if current.as_deref() == Some(workspace_name) {
        issues.push(DeletePreflightIssue {
            code: "current_context".to_string(),
            message: format!(
                "Cannot remove workspace '{}' from its own worktree context. Run the command from another workspace.",
                workspace_name
            ),
            force_overridable: false,
        });
    }

    if let Some(path) = worktree_path.as_ref().filter(|path| path.exists()) {
        if !path_owned_by_vcs {
            issues.push(DeletePreflightIssue {
                code: "unverified_worktree_path".to_string(),
                message: format!(
                    "Refusing to remove '{}' because it is only present in devflow's registry and is not owned by this project's live VCS worktree metadata. Remove or relink the stale registry entry without deleting that directory.",
                    path.display()
                ),
                force_overridable: false,
            });
        } else if let Some(repo) = vcs_provider.as_ref() {
            if repo.worktree_is_dirty(path)? {
                issues.push(DeletePreflightIssue {
                    code: "dirty_worktree".to_string(),
                    message: format!(
                        "Worktree at '{}' has uncommitted changes. Commit or stash them, or explicitly force deletion.",
                        path.display()
                    ),
                    force_overridable: true,
                });
            }
        }
    }

    let vcs_ref_exists = vcs_provider
        .as_ref()
        .and_then(|repo| repo.workspace_exists(workspace_name).ok())
        .unwrap_or(false);

    Ok(DeleteWorkspacePreflight {
        workspace: workspace_name.to_string(),
        service_key,
        worktree_path: worktree_path.map(|path| path.display().to_string()),
        vcs_ref_exists,
        issues,
    })
}

fn same_path(left: &Path, right: &Path) -> bool {
    crate::vcs::paths_equal(left, right)
}

fn vcs_owns_worktree_path(
    repo: &dyn vcs::VcsProvider,
    workspace_name: &str,
    path: &Path,
) -> Result<bool> {
    Ok(repo
        .worktree_path(workspace_name)?
        .as_ref()
        .is_some_and(|live_path| same_path(live_path, path)))
}

/// Remove a materialized worktree only when the current VCS metadata proves
/// that this exact path belongs to the requested workspace. Registry state is
/// historical metadata, not filesystem ownership evidence.
fn remove_materialized_worktree(
    repo: Option<&dyn vcs::VcsProvider>,
    workspace_name: &str,
    path: &Path,
    force: bool,
) -> Result<bool> {
    if !path.exists() {
        return Ok(true);
    }

    let repo = repo.with_context(|| {
        format!(
            "Refusing to remove '{}' because live VCS worktree metadata is unavailable",
            path.display()
        )
    })?;
    if !vcs_owns_worktree_path(repo, workspace_name, path)? {
        anyhow::bail!(
            "Refusing to remove '{}' because this project's live VCS metadata does not identify it as workspace '{}'",
            path.display(),
            workspace_name
        );
    }

    match repo.remove_worktree(path, force) {
        Ok(()) => Ok(true),
        Err(error) if force => {
            log::warn!(
                "Failed to remove VCS-owned worktree via VCS, falling back to filesystem removal: {}",
                error
            );
            std::fs::remove_dir_all(path).context("Failed to remove worktree directory")?;
            Ok(true)
        }
        Err(error) => {
            Err(error.context(format!("Refusing to delete workspace '{}'", workspace_name)))
        }
    }
}

/// Delete a workspace after a shared safety preflight.
///
/// Hook/resource ordering deliberately keeps the worktree present until all
/// hooks, processes, and services have completed:
///   preflight → PreRemove → PreServiceDelete → processes → services →
///   PostServiceDelete → PostRemove → worktree → VCS ref → state.
pub async fn delete_workspace(
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
    options: &DeleteOptions,
) -> Result<DeleteWorkspaceResult> {
    let opts = &options.lifecycle;
    let preflight = preflight_delete_workspace(config, project_dir, workspace_name)?;
    if !preflight.can_delete(options.force) {
        let messages = preflight
            .issues
            .iter()
            .filter(|issue| !options.force || !issue.force_overridable)
            .map(|issue| issue.message.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!(
            "Refusing to delete workspace '{}':\n{}",
            workspace_name,
            messages
        );
    }

    let vcs_provider = vcs::detect_vcs_provider(project_dir).ok();
    let mut hook_results = Vec::new();

    // All lifecycle hooks execute while their worktree is still available.
    if !opts.skip_hooks {
        run_lifecycle_hooks(
            config,
            project_dir,
            workspace_name,
            HookPhase::PreRemove,
            opts,
        )
        .await?;
    }

    let deleting_services = !options.keep_services && !opts.skip_services;
    if deleting_services && !opts.skip_hooks {
        if let Some(summary) = run_lifecycle_hooks_best_effort(
            config,
            project_dir,
            workspace_name,
            HookPhase::PreServiceDelete,
            opts,
        )
        .await
        {
            hook_results.push(summary);
        }
    }

    let process_results = if !opts.skip_processes {
        let results = match tokio::time::timeout(
            Duration::from_secs(20),
            processes::auto_stop_workspace_processes(config, project_dir, workspace_name),
        )
        .await
        {
            Ok(results) => results,
            Err(_) => vec![processes::ProcessResult {
                process: "(process-runtime)".to_string(),
                success: false,
                message: "timed out stopping workspace processes".to_string(),
                required: true,
                pid: None,
                ports: Vec::new(),
            }],
        };
        if results.iter().all(|result| result.success) {
            if let Err(error) =
                processes::cleanup_workspace_process_state(config, project_dir, workspace_name)
            {
                log::warn!(
                    "Failed to clean process state for '{}': {}",
                    workspace_name,
                    error
                );
            }
        }
        results
    } else {
        Vec::new()
    };

    ensure_required_processes_stopped(&process_results, workspace_name, options.force)?;

    // Preflight's dirty check ran BEFORE the PreRemove/PreServiceDelete hooks
    // and the process stop — any of which can legitimately write into the
    // worktree (hooks execute with it as their working directory, e.g. a
    // `pg_dump > backup.sql` safety net). Services are destroyed next, and
    // the worktree removal at the end re-checks cleanliness anyway; re-verify
    // NOW, while nothing has been destroyed and a non-forced retry is safe.
    if !options.force {
        if let (Some(path), Some(repo)) = (
            preflight.worktree_path.as_ref().map(PathBuf::from),
            vcs_provider.as_ref(),
        ) {
            if path.exists()
                && vcs_owns_worktree_path(repo.as_ref(), workspace_name, &path)?
                && repo.worktree_is_dirty(&path)?
            {
                anyhow::bail!(
                    "Worktree at '{}' gained uncommitted changes after preflight (a pre-remove hook or a stopping process likely wrote files). Nothing has been deleted yet — commit or clean them, or use --force.",
                    path.display()
                );
            }
        }
    }

    let service_results: Vec<ServiceResult> = if deleting_services {
        match tokio::time::timeout(
            Duration::from_secs(30),
            services::factory::orchestrate_delete(config, &preflight.service_key),
        )
        .await
        {
            Ok(Ok(results)) => results.into_iter().map(ServiceResult::from).collect(),
            Ok(Err(error)) => vec![ServiceResult {
                service_name: "(orchestration)".to_string(),
                success: false,
                message: format!("{error:#}"),
            }],
            Err(_) => vec![ServiceResult {
                service_name: "(orchestration)".to_string(),
                success: false,
                message: "timed out deleting service workspaces".to_string(),
            }],
        }
    } else {
        Vec::new()
    };

    if deleting_services && !opts.skip_hooks {
        if let Some(summary) = run_lifecycle_hooks_best_effort(
            config,
            project_dir,
            workspace_name,
            HookPhase::PostServiceDelete,
            opts,
        )
        .await
        {
            hook_results.push(summary);
        }
    }

    let service_failures = service_results
        .iter()
        .filter(|result| !result.success)
        .count();
    if service_failures > 0 && !options.force {
        anyhow::bail!(
            "Failed to delete {} service workspace(s) for '{}'; the worktree and VCS ref were preserved. Retry after fixing the service failure, or use --force to continue cleanup.",
            service_failures,
            workspace_name
        );
    }

    if !opts.skip_hooks {
        if let Some(summary) = run_lifecycle_hooks_best_effort(
            config,
            project_dir,
            workspace_name,
            HookPhase::PostRemove,
            opts,
        )
        .await
        {
            hook_results.push(summary);
        }
    }

    let worktree_path = preflight.worktree_path.as_ref().map(PathBuf::from);
    let mut worktree_removed = false;
    if let Some(path) = worktree_path.as_ref() {
        worktree_removed = remove_materialized_worktree(
            vcs_provider.as_deref(),
            workspace_name,
            path,
            options.force,
        )
        .map_err(|error| {
            // Be honest about partial state: services were already deleted
            // above, so "refusing" here must not read as a clean abort.
            if deleting_services {
                error.context(format!(
                    "service workspaces for '{workspace_name}' were already deleted; the worktree and VCS ref remain"
                ))
            } else {
                error
            }
        })?;
    }

    if worktree_removed {
        if let Some(repo) = vcs_provider.as_ref() {
            if let Err(error) = repo.prune_worktrees() {
                log::debug!("Failed to prune removed worktree metadata: {error:#}");
            }
        }
    }

    let mut vcs_ref_deleted = !preflight.vcs_ref_exists;
    if preflight.vcs_ref_exists {
        if let Some(repo) = vcs_provider.as_ref() {
            match repo.delete_workspace(workspace_name) {
                Ok(()) => vcs_ref_deleted = true,
                Err(error) if options.force => {
                    log::warn!("Failed to delete VCS ref '{}': {}", workspace_name, error);
                }
                Err(error) => {
                    return Err(error.context(format!(
                        "Worktree removed, but failed to delete VCS ref '{}'; registry state was retained for retry",
                        workspace_name
                    )));
                }
            }
            if let Err(error) = repo.prune_worktrees() {
                log::debug!("Failed to prune stale worktrees after delete: {error:#}");
            }
        }
    }

    if vcs_ref_deleted || options.force {
        if let Ok(mut state_mgr) = LocalStateManager::new() {
            if let Err(error) = state_mgr.unregister_workspace_by_dir(project_dir, workspace_name) {
                log::warn!("Failed to unregister workspace from devflow state: {error}");
            }
        }
    }

    Ok(DeleteWorkspaceResult {
        workspace: workspace_name.to_string(),
        service_key: preflight.service_key,
        worktree_removed,
        worktree_path: preflight.worktree_path,
        vcs_ref_deleted,
        services: service_results,
        processes: process_results,
        hooks: hook_results,
    })
}

fn ensure_required_processes_stopped(
    process_results: &[processes::ProcessResult],
    workspace_name: &str,
    force: bool,
) -> Result<()> {
    let failures = process_results
        .iter()
        .filter(|result| result.required && !result.success)
        .count();
    if failures > 0 && !force {
        anyhow::bail!(
            "Failed to stop {} required workspace process(es) for '{}'; services, worktree, and VCS ref were preserved. Retry after fixing the process failure, or use --force to continue cleanup.",
            failures,
            workspace_name
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vcs::{GitRepository, VcsProvider};

    fn materialized_repo() -> (tempfile::TempDir, Config, PathBuf) {
        let project = tempfile::tempdir().unwrap();
        GitRepository::init(project.path()).unwrap();
        let repo = GitRepository::new(project.path()).unwrap();
        repo.create_workspace("feature/delete", Some("main"))
            .unwrap();
        let worktree = project.path().join("feature-delete");
        repo.create_worktree("feature/delete", &worktree).unwrap();
        (project, Config::default(), worktree)
    }

    #[test]
    fn preflight_reports_dirty_worktree_as_force_overridable() {
        let (project, config, worktree) = materialized_repo();
        std::fs::write(worktree.join("untracked.txt"), "keep me").unwrap();

        let report = preflight_delete_workspace(&config, project.path(), "feature/delete").unwrap();

        assert!(!report.can_delete(false));
        assert!(report.can_delete(true));
        assert!(report
            .issues
            .iter()
            .any(|issue| { issue.code == "dirty_worktree" && issue.force_overridable }));
    }

    #[test]
    fn preflight_never_allows_default_workspace_deletion() {
        let project = tempfile::tempdir().unwrap();
        GitRepository::init(project.path()).unwrap();
        let report =
            preflight_delete_workspace(&Config::default(), project.path(), "main").unwrap();

        assert!(!report.can_delete(false));
        assert!(!report.can_delete(true));
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.code == "default_workspace"));
    }

    #[test]
    fn preflight_protects_physical_primary_when_configured_default_differs() {
        let project = tempfile::tempdir().unwrap();
        let repo = GitRepository::init(project.path()).unwrap();
        repo.create_workspace("feature/primary", Some("main"))
            .unwrap();
        let raw = git2::Repository::open(project.path()).unwrap();
        raw.set_head("refs/heads/feature/primary").unwrap();

        let report =
            preflight_delete_workspace(&Config::default(), project.path(), "feature/primary")
                .unwrap();
        assert!(!report.can_delete(false));
        assert!(!report.can_delete(true));
        assert!(report.issues.iter().any(|issue| {
            issue.code == "primary_workspace"
                && !issue.force_overridable
                && issue.message.contains("physical primary checkout")
        }));
    }

    #[test]
    fn force_never_removes_a_registry_only_path_reused_by_another_repo() {
        let project = tempfile::tempdir().unwrap();
        GitRepository::init(project.path()).unwrap();
        let repo = GitRepository::new(project.path()).unwrap();
        repo.create_workspace("feature/stale", Some("main"))
            .unwrap();

        let unrelated = tempfile::tempdir().unwrap();
        GitRepository::init(unrelated.path()).unwrap();
        std::fs::write(unrelated.path().join("keep.txt"), "must survive").unwrap();

        let report = preflight_delete_workspace_with_registered_path(
            &Config::default(),
            project.path(),
            "feature/stale",
            Some(unrelated.path().to_path_buf()),
        )
        .unwrap();
        assert!(!report.can_delete(true));
        assert!(report
            .issues
            .iter()
            .any(|issue| { issue.code == "unverified_worktree_path" && !issue.force_overridable }));

        let error =
            remove_materialized_worktree(Some(&repo), "feature/stale", unrelated.path(), true)
                .unwrap_err();
        assert!(error.to_string().contains("live VCS metadata"));
        assert_eq!(
            std::fs::read_to_string(unrelated.path().join("keep.txt")).unwrap(),
            "must survive"
        );
        assert!(unrelated.path().join(".git").exists());
    }

    #[test]
    fn missing_registry_only_path_allows_partial_cleanup() {
        let project = tempfile::tempdir().unwrap();
        GitRepository::init(project.path()).unwrap();
        let repo = GitRepository::new(project.path()).unwrap();
        repo.create_workspace("feature/stale", Some("main"))
            .unwrap();
        let missing = project.path().join("already-removed-worktree");

        let report = preflight_delete_workspace_with_registered_path(
            &Config::default(),
            project.path(),
            "feature/stale",
            Some(missing.clone()),
        )
        .unwrap();
        assert!(report.can_delete(false));
        assert!(
            remove_materialized_worktree(Some(&repo), "feature/stale", &missing, true).unwrap()
        );
    }

    #[test]
    fn required_process_failures_need_force_before_destructive_cleanup() {
        let results = vec![processes::ProcessResult {
            process: "api".to_string(),
            success: false,
            message: "still running".to_string(),
            required: true,
            pid: Some(42),
            ports: vec![3000],
        }];

        let error = ensure_required_processes_stopped(&results, "feature/api", false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("services, worktree, and VCS ref were preserved"));
        assert!(error.contains("--force"));
        ensure_required_processes_stopped(&results, "feature/api", true).unwrap();
    }

    #[tokio::test]
    async fn clean_delete_removes_worktree_before_vcs_ref() {
        let (project, config, worktree) = materialized_repo();
        let result = delete_workspace(
            &config,
            project.path(),
            "feature/delete",
            &DeleteOptions {
                lifecycle: LifecycleOptions {
                    skip_hooks: true,
                    skip_services: true,
                    skip_processes: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(result.worktree_removed);
        assert!(result.vcs_ref_deleted);
        assert!(!worktree.exists());
        let repo = GitRepository::new(project.path()).unwrap();
        assert!(!repo.workspace_exists("feature/delete").unwrap());
    }
}

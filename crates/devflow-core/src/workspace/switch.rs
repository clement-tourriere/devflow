use anyhow::{Context, Result};
use std::path::Path;

use crate::config::Config;
use crate::hooks::HookPhase;
use crate::processes;
use crate::services;
use crate::state::{DevflowWorkspace, LocalStateManager};
use crate::vcs;

use super::hooks::{run_lifecycle_hooks, run_lifecycle_hooks_best_effort};
use super::worktree::create_worktree_with_files;
use super::{LifecycleOptions, ServiceResult, SwitchWorkspaceResult, WorktreeSetupResult};

/// Options specific to workspace switching.
#[derive(Debug, Clone, Default)]
pub struct SwitchOptions {
    /// Shared lifecycle options.
    pub lifecycle: LifecycleOptions,
    /// Allow creating the workspace if it doesn't exist.
    pub create_if_missing: bool,
    /// Parent workspace to branch from when creating.
    pub from_workspace: Option<String>,
    /// Override the config `worktree.copy_files` for worktree creation.
    pub copy_files: Option<Vec<String>>,
    /// Override the config `worktree.copy_ignored` for worktree creation.
    pub copy_ignored: Option<bool>,
}

/// Switch to a workspace with the full lifecycle: pre-switch hooks,
/// VCS reference creation (when requested), worktree setup, service
/// orchestration, and post-switch hooks.
///
/// Hook phase ordering:
///   PreSwitch → VCS materialization → services → PostServiceSwitch →
///   PostCreate (if new) → PostSwitch
pub async fn switch_workspace(
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
    options: &SwitchOptions,
) -> Result<SwitchWorkspaceResult> {
    super::validate_workspace_name(workspace_name).map_err(anyhow::Error::msg)?;
    let opts = &options.lifecycle;
    let vcs_provider =
        vcs::detect_vcs_provider(project_dir).context("Failed to open VCS repository")?;

    if !vcs_provider.supports_worktrees() {
        anyhow::bail!(
            "{} does not support materialized workspaces",
            vcs_provider.provider_name()
        );
    }
    super::invariant::ensure_git_primary_workspace_matches_config(config, vcs_provider.as_ref())?;

    let mut hook_results = Vec::new();

    // Ensure main workspace is registered in state
    ensure_default_workspace_registered(config, project_dir)?;
    // Resolve before hooks or VCS mutation. This preserves an unambiguously
    // adopted legacy namespace and fails closed when old lossy state could
    // refer to more than one raw workspace.
    let service_key = LocalStateManager::new()?
        .resolve_workspace_service_key_by_dir(project_dir, workspace_name)?;

    // 1. Pre-switch hooks
    if !opts.skip_hooks {
        let phase_started = std::time::Instant::now();
        run_lifecycle_hooks(
            config,
            project_dir,
            workspace_name,
            HookPhase::PreSwitch,
            opts,
        )
        .await?;
        log::debug!(
            "Phase pre-switch hooks took {:.2?}",
            phase_started.elapsed()
        );
    }

    let mut vcs_ref_created = false;
    let mut parent_for_new: Option<String> = None;
    let worktree_result: Option<WorktreeSetupResult>;

    // 2. VCS reference creation / worktree materialization
    let vcs_phase_started = std::time::Instant::now();
    let existing_path = vcs_provider.worktree_path(workspace_name)?;
    if let Some(wt_path) = existing_path {
        let resolved = std::fs::canonicalize(&wt_path).unwrap_or(wt_path);
        worktree_result = Some(WorktreeSetupResult {
            path: resolved,
            created: false,
        });
    } else {
        let workspace_exists = vcs_provider.workspace_exists(workspace_name)?;
        if !workspace_exists {
            if !options.create_if_missing {
                anyhow::bail!(
                    "Workspace '{}' does not exist. Use the create flag to create it.",
                    workspace_name
                );
            }
            parent_for_new = options
                .from_workspace
                .clone()
                .or(vcs_provider.current_workspace()?);
            vcs_provider.create_workspace(workspace_name, parent_for_new.as_deref())?;
            vcs_ref_created = true;
        }

        worktree_result = Some(create_worktree_with_files(
            vcs_provider.as_ref(),
            config,
            project_dir,
            workspace_name,
            options.copy_files.as_deref(),
            options.copy_ignored,
        )?);
    }
    log::debug!(
        "Phase VCS ref/worktree setup took {:.2?}",
        vcs_phase_started.elapsed()
    );

    // 3. Register workspace in state (before services, independent of their success)
    register_workspace_state(
        config,
        project_dir,
        workspace_name,
        &service_key,
        parent_for_new.as_deref(),
        worktree_result.as_ref(),
    )?;

    let worktree_created = worktree_result.as_ref().is_some_and(|wt| wt.created);
    let workspace_created = vcs_ref_created || worktree_created;
    let workspace_parent = if vcs_ref_created {
        parent_for_new.clone()
    } else {
        // Look up stored parent from registry. This covers newly-created
        // worktrees for existing branches and existing workspaces selected from
        // the GUI.
        LocalStateManager::new()
            .ok()
            .and_then(|state| state.get_workspace_by_dir(project_dir, workspace_name))
            .and_then(|b| b.parent)
    };
    let parent_service_key = workspace_parent
        .as_deref()
        .map(|parent| {
            LocalStateManager::new()?.resolve_workspace_service_key_by_dir(project_dir, parent)
        })
        .transpose()?;

    // 4. Service orchestration
    let services_skipped = opts.skip_services || config.resolve_services().is_empty();
    let service_results: Vec<ServiceResult> = if !services_skipped {
        let services_phase_started = std::time::Instant::now();
        let service_results: Vec<ServiceResult> = match services::factory::orchestrate_switch(
            config,
            &service_key,
            parent_service_key.as_deref(),
        )
        .await
        {
            Ok(results) => results.into_iter().map(ServiceResult::from).collect(),
            Err(e) => {
                // Branch/worktree already exist — record the failure and
                // finish the switch instead of aborting half-way.
                log::warn!("Service orchestration failed: {:#}", e);
                vec![ServiceResult {
                    service_name: "(orchestration)".to_string(),
                    success: false,
                    message: format!("{:#}", e),
                }]
            }
        };
        log::debug!(
            "Phase service orchestration took {:.2?}",
            services_phase_started.elapsed()
        );

        // Post-service-switch hooks (only if any service succeeded)
        let any_success = service_results.iter().any(|r| r.success);
        if any_success && !opts.skip_hooks {
            if let Some(summary) = run_lifecycle_hooks_best_effort(
                config,
                project_dir,
                workspace_name,
                HookPhase::PostServiceSwitch,
                opts,
            )
            .await
            {
                hook_results.push(summary);
            }
        }

        service_results
    } else {
        vec![]
    };

    if !services_skipped && service_results.iter().any(|r| r.success) {
        if let Err(e) = write_workspace_env_overrides(
            config,
            project_dir,
            workspace_name,
            &service_key,
            worktree_result.as_ref(),
        )
        .await
        {
            log::warn!(
                "Failed to write workspace environment overrides for '{}': {:#}",
                workspace_name,
                e
            );
        }
    }

    // 5. Post-create hooks (branch or worktree newly created)
    if (vcs_ref_created || worktree_created) && !opts.skip_hooks {
        let phase_started = std::time::Instant::now();
        if let Some(summary) = run_lifecycle_hooks_best_effort(
            config,
            project_dir,
            workspace_name,
            HookPhase::PostCreate,
            opts,
        )
        .await
        {
            hook_results.push(summary);
        }
        log::debug!(
            "Phase post-create hooks took {:.2?}",
            phase_started.elapsed()
        );
    }

    // 6. Post-switch hooks (always)
    if !opts.skip_hooks {
        let phase_started = std::time::Instant::now();
        if let Some(summary) = run_lifecycle_hooks_best_effort(
            config,
            project_dir,
            workspace_name,
            HookPhase::PostSwitch,
            opts,
        )
        .await
        {
            hook_results.push(summary);
        }
        log::debug!(
            "Phase post-switch hooks took {:.2?}",
            phase_started.elapsed()
        );
    }

    // 7. Process orchestration (after hooks so generated .env files exist).
    let process_phase_started = std::time::Instant::now();
    let process_results = if !opts.skip_processes {
        let clone_parent_processes = workspace_created
            || workspace_parent.as_deref().is_some_and(|_| {
                workspace_has_no_runtime_processes(config, project_dir, workspace_name)
            });

        if clone_parent_processes {
            if let Some(parent) = workspace_parent.as_deref() {
                processes::auto_start_workspace_processes_like_parent(
                    config,
                    project_dir,
                    workspace_name,
                    parent,
                    process_approval_mode(opts.hook_approval),
                )
                .await
            } else {
                Vec::new()
            }
        } else {
            processes::auto_start_workspace_processes(
                config,
                project_dir,
                workspace_name,
                process_approval_mode(opts.hook_approval),
            )
            .await
        }
    } else {
        Vec::new()
    };
    if !opts.skip_processes {
        log::debug!(
            "Phase process orchestration took {:.2?}",
            process_phase_started.elapsed()
        );
    }

    Ok(SwitchWorkspaceResult {
        workspace: workspace_name.to_string(),
        service_key,
        parent: workspace_parent,
        worktree: worktree_result,
        vcs_ref_created,
        services: service_results,
        processes: process_results,
        hooks: hook_results,
    })
}

async fn write_workspace_env_overrides(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    service_workspace: &str,
    worktree: Option<&WorktreeSetupResult>,
) -> Result<()> {
    let target_dir = worktree
        .map(|wt| wt.path.clone())
        .unwrap_or_else(|| project_dir.to_path_buf());
    let env_path = target_dir.join(".env.local");

    let mut updates = std::collections::BTreeMap::new();
    updates.insert("DEVFLOW_WORKSPACE".to_string(), workspace.to_string());

    let services = config.resolve_services();
    for service in services.iter().filter(|service| service.auto_workspace) {
        let Ok(provider) =
            services::factory::create_provider_from_named_config(config, service).await
        else {
            continue;
        };
        let Ok(info) = provider.get_connection_info(service_workspace).await else {
            continue;
        };
        let Some(url) = info.connection_string else {
            continue;
        };

        let service_key = service
            .name
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>();
        updates.insert(format!("DEVFLOW_{}_URL", service_key), url.clone());

        match service.service_type.as_str() {
            "postgres" => {
                if service.default || service.name == "db" || !updates.contains_key("DATABASE_URL")
                {
                    updates.insert("DATABASE_URL".to_string(), url);
                }
            }
            "redis" if !updates.contains_key("REDIS_URL") => {
                updates.insert("REDIS_URL".to_string(), url);
            }
            _ => {}
        }
    }

    if updates.len() <= 1 {
        return Ok(());
    }

    upsert_env_file(&env_path, &updates)
}

fn upsert_env_file(
    path: &Path,
    updates: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let existing = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };

    let mut seen = std::collections::HashSet::new();
    let mut lines = Vec::new();
    for line in existing.lines() {
        let key = line
            .split_once('=')
            .map(|(key, _)| key.trim())
            .filter(|key| !key.is_empty() && !key.starts_with('#'));
        if let Some(key) = key {
            if let Some(value) = updates.get(key) {
                lines.push(format!("{}={}", key, value));
                seen.insert(key.to_string());
                continue;
            }
        }
        lines.push(line.to_string());
    }

    if !updates.keys().all(|key| seen.contains(key)) && !lines.is_empty() {
        lines.push(String::new());
    }
    for (key, value) in updates {
        if !seen.contains(key) {
            lines.push(format!("{}={}", key, value));
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, lines.join("\n") + "\n")?;
    Ok(())
}

fn workspace_has_no_runtime_processes(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
) -> bool {
    processes::list_workspace_processes(config, project_dir, Some(workspace))
        .map(|statuses| statuses.iter().all(|status| status.source == "config"))
        .unwrap_or(false)
}

fn process_approval_mode(mode: super::hooks::HookApprovalMode) -> processes::ProcessApprovalMode {
    match mode {
        super::hooks::HookApprovalMode::Interactive => processes::ProcessApprovalMode::Interactive,
        super::hooks::HookApprovalMode::NonInteractive => {
            processes::ProcessApprovalMode::NonInteractive
        }
        super::hooks::HookApprovalMode::NoApproval => processes::ProcessApprovalMode::NoApproval,
    }
}

fn ensure_default_workspace_registered(config: &Config, project_dir: &Path) -> Result<()> {
    let main = &config.git.main_workspace;
    LocalStateManager::new()?.ensure_default_workspace(project_dir, main)
}

fn register_workspace_state(
    _config: &Config,
    project_dir: &Path,
    workspace_name: &str,
    service_key: &str,
    parent: Option<&str>,
    worktree: Option<&WorktreeSetupResult>,
) -> Result<()> {
    let mut state_mgr = LocalStateManager::new()?;

    // Preserve existing metadata on upsert
    let existing = state_mgr.get_workspace_by_dir(project_dir, workspace_name);

    let workspace = DevflowWorkspace {
        name: workspace_name.to_string(),
        service_key: service_key.to_string(),
        raw_identity_verified: true,
        parent: existing
            .as_ref()
            .and_then(|workspace| workspace.parent.clone())
            .or_else(|| parent.map(String::from)),
        worktree_path: worktree
            .map(|w| w.path.display().to_string())
            .or_else(|| existing.as_ref().and_then(|b| b.worktree_path.clone())),
        created_at: existing
            .as_ref()
            .map(|b| b.created_at)
            .unwrap_or_else(chrono::Utc::now),
        executed_command: existing.as_ref().and_then(|b| b.executed_command.clone()),
        execution_status: existing.as_ref().and_then(|b| b.execution_status.clone()),
        executed_at: existing.as_ref().and_then(|b| b.executed_at),
    };

    state_mgr.register_workspace_by_dir(project_dir, workspace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, WorktreeConfig};
    use crate::hooks::{HookEntry, HookPhase, HooksConfig, IndexMap};
    use crate::vcs::git::GitRepository;
    use crate::vcs::VcsProvider;
    use tempfile::TempDir;

    struct TestEnv {
        _config_home: TempDir,
        _project_home: TempDir,
        _old_xdg_config_home: Option<String>,
        _old_home: Option<String>,
    }

    impl TestEnv {
        fn new() -> Self {
            let config_home = tempfile::tempdir().unwrap();
            let project_home = tempfile::tempdir().unwrap();
            let old_xdg_config_home = std::env::var("XDG_CONFIG_HOME").ok();
            let old_home = std::env::var("HOME").ok();
            std::env::set_var("XDG_CONFIG_HOME", config_home.path());
            std::env::set_var("HOME", project_home.path());
            Self {
                _config_home: config_home,
                _project_home: project_home,
                _old_xdg_config_home: old_xdg_config_home,
                _old_home: old_home,
            }
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            if let Some(value) = self._old_xdg_config_home.as_ref() {
                std::env::set_var("XDG_CONFIG_HOME", value);
            } else {
                std::env::remove_var("XDG_CONFIG_HOME");
            }

            if let Some(value) = self._old_home.as_ref() {
                std::env::set_var("HOME", value);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }

    fn setup_repo() -> (TempDir, Config) {
        let temp = tempfile::tempdir().unwrap();
        GitRepository::init(temp.path()).unwrap();
        let config = Config {
            worktree: WorktreeConfig {
                path_template: "../{repo}.{workspace}".to_string(),
                copy_files: Vec::new(),
                copy_ai_configs: false,
                ..Default::default()
            },
            ..Default::default()
        };
        (temp, config)
    }

    #[tokio::test]
    async fn switch_workspace_skips_unapproved_hooks_non_interactive() {
        let _env = TestEnv::new();
        let (project, mut config) = setup_repo();

        let mut hooks: HooksConfig = IndexMap::new();
        let mut post_create = IndexMap::new();
        post_create.insert(
            "needs-approval".to_string(),
            HookEntry::Simple("printf blocked > post-create-marker.txt".to_string()),
        );
        hooks.insert(HookPhase::PostCreate, post_create);
        config.hooks = Some(hooks);

        let result = switch_workspace(
            &config,
            project.path(),
            "feature/approval",
            &SwitchOptions {
                lifecycle: LifecycleOptions {
                    hook_approval: crate::workspace::hooks::HookApprovalMode::NonInteractive,
                    ..Default::default()
                },
                create_if_missing: true,
                ..Default::default()
            },
        )
        .await
        .expect("switch must succeed; the unapproved hook is skipped, not fatal");

        // The worktree was created and reported (agents need worktree_path)
        let wt = result.worktree.as_ref().expect("worktree result present");
        assert!(wt.created);

        // The unapproved hook was skipped — visibly counted, never executed
        let post_create_summary = result
            .hooks
            .iter()
            .find(|h| h.phase == "post-create")
            .expect("post-create summary present");
        assert_eq!(post_create_summary.skipped, 1);
        assert_eq!(post_create_summary.succeeded, 0);
        assert!(!wt.path.join("post-create-marker.txt").exists());
    }

    #[tokio::test]
    async fn switch_workspace_runs_post_create_for_new_worktree_on_existing_branch() {
        let _env = TestEnv::new();
        let (project, mut config) = setup_repo();
        let repo = GitRepository::new(project.path()).unwrap();
        repo.create_workspace("feature/existing", Some("main"))
            .unwrap();

        let mut hooks: HooksConfig = IndexMap::new();
        let mut post_create = IndexMap::new();
        post_create.insert(
            "write-marker".to_string(),
            HookEntry::Simple("printf created > post-create-marker.txt".to_string()),
        );
        hooks.insert(HookPhase::PostCreate, post_create);
        config.hooks = Some(hooks);

        let result = switch_workspace(
            &config,
            project.path(),
            "feature/existing",
            &SwitchOptions {
                lifecycle: LifecycleOptions {
                    hook_approval: crate::workspace::hooks::HookApprovalMode::NoApproval,
                    ..Default::default()
                },
                create_if_missing: true,
                ..Default::default()
            },
        )
        .await
        .expect("switch should succeed");

        assert!(!result.vcs_ref_created);
        assert!(result.worktree.as_ref().is_some_and(|wt| wt.created));
        assert!(result.hooks.iter().any(|h| h.phase == "post-create"));

        let marker = result
            .worktree
            .as_ref()
            .unwrap()
            .path
            .join("post-create-marker.txt");
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "created");
    }

    #[tokio::test]
    async fn primary_mismatch_blocks_a_second_default_worktree() {
        let (project, mut config) = setup_repo();
        // Enforcement requires a literally-configured default: the serde
        // default must never hard-fail repos whose primary branch isn't
        // "main" (see invariant::ensure_git_primary_workspace_matches_config).
        std::fs::write(
            project.path().join(".devflow.yml"),
            "git:\n  main_workspace: main\n",
        )
        .unwrap();
        config.project_root = Some(project.path().to_path_buf());
        let repo = GitRepository::new(project.path()).unwrap();
        repo.create_workspace("feature/primary", Some("main"))
            .unwrap();
        let raw = git2::Repository::open(project.path()).unwrap();
        raw.set_head("refs/heads/feature/primary").unwrap();

        let error = switch_workspace(
            &config,
            project.path(),
            "main",
            &SwitchOptions {
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
        .unwrap_err();

        assert!(error.to_string().contains("Git primary workspace mismatch"));
        assert!(repo.worktree_path("main").unwrap().is_none());
        let worktrees = repo.list_worktrees().unwrap();
        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].workspace.as_deref(), Some("feature/primary"));
    }
}

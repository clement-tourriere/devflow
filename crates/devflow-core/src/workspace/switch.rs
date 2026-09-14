use anyhow::{Context, Result};
use std::path::Path;

use crate::config::Config;
use crate::hooks::HookPhase;
use crate::processes;
use crate::services;
use crate::state::{DevflowWorkspace, LocalStateManager};
use crate::vcs;

use super::hooks::{run_lifecycle_hooks_best_effort, run_lifecycle_hooks_with_result};
use super::worktree::create_worktree_with_files;
use super::{LifecycleOptions, ServiceResult, SwitchWorkspaceResult, WorktreeSetupResult};

/// Options specific to workspace switching.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SwitchOptions {
    /// Shared lifecycle options.
    pub lifecycle: LifecycleOptions,
    /// Allow creating the workspace if it doesn't exist.
    pub create_if_missing: bool,
    /// Parent workspace to branch from when creating.
    pub from_workspace: Option<String>,
    /// Independent data baseline; defaults to the creation parent.
    pub data_from_workspace: Option<String>,
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
    let repo = vcs::detect_vcs_provider(project_dir)?;
    let existing = LocalStateManager::new()?.get_workspace_by_dir(project_dir, workspace_name);
    let creating = repo.worktree_path(workspace_name)?.is_none()
        && workspace_name != config.git.main_workspace;
    let code_parent = existing
        .as_ref()
        .and_then(|w| w.parent.clone())
        .or_else(|| options.from_workspace.clone())
        .or(if creating {
            repo.current_workspace()?
        } else {
            None
        });
    let data_parent = options
        .data_from_workspace
        .clone()
        .or_else(|| {
            existing
                .as_ref()
                .and_then(|w| w.data_parent.clone().or(w.parent.clone()))
        })
        .or_else(|| code_parent.clone());
    let mut journal = super::operation::OperationJournal::begin(
        project_dir,
        workspace_name,
        "switch",
        code_parent,
        data_parent,
        creating,
    )
    .await?;
    journal.set_request(
        serde_json::json!({"workspace":workspace_name,"options":options}),
        config,
    )?;
    if let Some(requested) = &options.data_from_workspace {
        if journal.record.data_parent.as_ref() != Some(requested)
            || existing
                .as_ref()
                .and_then(|w| w.data_parent.as_ref().or(w.parent.as_ref()))
                .is_some_and(|parent| parent != requested)
        {
            return Err(journal.fail(anyhow::anyhow!(
                "data origin is immutable; retry with the original --data-from workspace"
            )));
        }
    }
    match switch_workspace_inner(config, project_dir, workspace_name, options, &mut journal).await {
        Ok(mut result) => {
            let ready = result.services.iter().all(|r| r.success)
                && result.processes.iter().all(|r| r.success || !r.required)
                && result
                    .hooks
                    .iter()
                    .all(|r| r.required_failed == 0 && r.required_skipped == 0);
            journal.finish(ready)?;
            result.operation = Some(journal.record.clone());
            Ok(result)
        }
        Err(error) => Err(journal.fail(error)),
    }
}

async fn switch_workspace_inner(
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
    options: &SwitchOptions,
    journal: &mut super::operation::OperationJournal,
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

    journal.phase("pre-switch")?;
    // 1. Pre-switch hooks
    if !opts.skip_hooks {
        let phase_started = std::time::Instant::now();
        let summary = run_lifecycle_hooks_with_result(
            config,
            project_dir,
            workspace_name,
            HookPhase::PreSwitch,
            opts,
        )
        .await?;
        hook_results.push(super::LifecycleHookResult::from_run_result(
            &HookPhase::PreSwitch,
            summary,
        ));
        log::debug!(
            "Phase pre-switch hooks took {:.2?}",
            phase_started.elapsed()
        );
    }

    let mut vcs_ref_created = false;
    let mut parent_for_new = journal.record.code_parent.clone();
    journal.record.worktree_path = Some(
        super::worktree::resolve_existing_or_planned_worktree_path(
            config,
            project_dir,
            workspace_name,
        )
        .display()
        .to_string(),
    );
    journal.phase("materialize")?;
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
            parent_for_new = journal.record.code_parent.clone();
            register_workspace_state(
                config,
                project_dir,
                workspace_name,
                &service_key,
                parent_for_new.as_deref(),
                journal.record.data_parent.as_deref(),
                None,
            )?;
            vcs_provider.create_workspace(workspace_name, parent_for_new.as_deref())?;
            vcs_ref_created = true;
        }

        worktree_result = Some(
            match create_worktree_with_files(
                vcs_provider.as_ref(),
                config,
                project_dir,
                workspace_name,
                options.copy_files.as_deref(),
                options.copy_ignored,
            ) {
                Ok(worktree) => worktree,
                Err(error) => {
                    if vcs_ref_created && vcs_provider.worktree_path(workspace_name)?.is_none() {
                        vcs_provider.delete_workspace(workspace_name)?;
                    }
                    return Err(error);
                }
            },
        );
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
        journal.record.data_parent.as_deref(),
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
    journal.record.worktree_path = worktree_result
        .as_ref()
        .map(|w| w.path.display().to_string());
    journal.completed("materialize")?;
    let origins = crate::baseline::bind_origins(
        config,
        project_dir,
        workspace_name,
        journal.record.data_parent.as_deref(),
    )?;
    journal.phase("services")?;
    // 4. Service orchestration
    let services_skipped = opts.skip_services || config.resolve_services().is_empty();
    let mut service_results: Vec<ServiceResult> = if !services_skipped {
        let services_phase_started = std::time::Instant::now();
        let service_results: Vec<ServiceResult> =
            match services::factory::orchestrate_switch_with_origins(
                config,
                &service_key,
                None,
                &origins,
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

        service_results
    } else {
        vec![]
    };

    for result in &service_results {
        journal.checkpoint(
            &format!("service:{}", result.service_name),
            super::operation::StepKind::Service,
            result.success,
            Some(result.message.clone()),
        )?;
    }
    if service_results.iter().all(|r| r.success) {
        journal.completed("services")?;
    }
    journal.phase("environment")?;
    if !services_skipped && service_results.iter().all(|r| r.success) {
        if let Err(e) = write_workspace_env_overrides(
            config,
            project_dir,
            workspace_name,
            &service_key,
            worktree_result.as_ref(),
        )
        .await
        {
            service_results.push(ServiceResult {
                service_name: "(environment)".into(),
                success: false,
                message: format!("{e:#}"),
            });
        }
    }

    let mut setup_ready = service_results.iter().all(|r| r.success)
        && hook_results
            .iter()
            .all(|r| r.required_failed == 0 && r.required_skipped == 0);
    for (phase, needed) in [
        (HookPhase::PostServiceSwitch, !services_skipped),
        (HookPhase::PostCreate, journal.record.post_create_pending),
        (HookPhase::PostSwitch, true),
    ] {
        if opts.skip_hooks || !needed {
            continue;
        }
        journal.phase(&phase.to_string())?;
        if !setup_ready {
            hook_results.push(super::LifecycleHookResult {
                required_failed: 0,
                required_skipped: 1,
                background_records: Vec::new(),
                phase: phase.to_string(),
                succeeded: 0,
                failed: 0,
                skipped: 1,
                background: 0,
                errors: vec![],
                skipped_reasons: vec![
                    "required services, generated environment, or an earlier hook failed".into(),
                ],
            });
            continue;
        }
        if let Some(summary) = run_lifecycle_hooks_best_effort(
            config,
            project_dir,
            workspace_name,
            phase.clone(),
            opts,
        )
        .await
        {
            setup_ready &= summary.required_failed == 0 && summary.required_skipped == 0;
            if summary.required_failed == 0
                && summary.required_skipped == 0
                && summary.background == 0
            {
                journal.completed(&phase.to_string())?;
            }
            journal.checkpoint(
                &format!("hooks:{phase}"),
                super::operation::StepKind::Hook,
                summary.ready(),
                Some(serde_json::to_string(&summary)?),
            )?;
            hook_results.push(summary);
        }
    }
    setup_ready &= hook_results
        .iter()
        .all(|r| r.required_failed == 0 && r.required_skipped == 0);
    journal.phase("processes")?;

    // 7. Process orchestration (after hooks so generated .env files exist).
    let process_phase_started = std::time::Instant::now();
    let process_results = if !opts.skip_processes && setup_ready {
        let clone_parent_processes = config
            .processes
            .as_ref()
            .is_some_and(|p| p.start_policy == processes::ProcessStartPolicy::InheritParent)
            && (workspace_created
                || workspace_parent.as_deref().is_some_and(|_| {
                    workspace_has_no_runtime_processes(config, project_dir, workspace_name)
                }));

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
        operation: None,
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
        let provider =
            services::factory::create_provider_from_named_config(config, service).await?;
        let info = provider
            .get_connection_info(service_workspace)
            .await
            .with_context(|| format!("service {} has no usable connection", service.name))?;
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
    let files =
        crate::files::WorkspaceFiles::open(path.parent().context("env file has no directory")?)?;
    let filename = path.file_name().context("env file has no name")?;
    let existing = files.read(filename)?.unwrap_or_default();

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

    files.write(
        filename,
        (lines.join("\n") + "\n").as_bytes(),
        crate::files::WritePolicy::Replace,
    )?;
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
    data_parent: Option<&str>,
    worktree: Option<&WorktreeSetupResult>,
) -> Result<()> {
    let mut state_mgr = LocalStateManager::new()?;

    // Preserve existing metadata on upsert
    let existing = state_mgr.get_workspace_by_dir(project_dir, workspace_name);

    let workspace = DevflowWorkspace {
        data_parent: existing
            .as_ref()
            .and_then(|w| w.data_parent.clone().or(w.parent.clone()))
            .or_else(|| data_parent.map(String::from)),
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
        // Env vars are process-global while tests run in parallel threads:
        // without this crate-wide guard, one test's TempDir is dropped (and
        // its HOME restored) while another is still resolving state paths
        // under it, so local_state.yml's lock file lands in a deleted dir.
        _env_guard: std::sync::MutexGuard<'static, ()>,
        _config_home: TempDir,
        _project_home: TempDir,
        _old_xdg_config_home: Option<String>,
        _old_home: Option<String>,
    }

    impl TestEnv {
        fn new() -> Self {
            let env_guard = crate::processes::PROCESS_TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let config_home = tempfile::tempdir().unwrap();
            let project_home = tempfile::tempdir().unwrap();
            let old_xdg_config_home = std::env::var("XDG_CONFIG_HOME").ok();
            let old_home = std::env::var("HOME").ok();
            std::env::set_var("XDG_CONFIG_HOME", config_home.path());
            std::env::set_var("HOME", project_home.path());
            Self {
                _env_guard: env_guard,
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

use std::path::PathBuf;

use anyhow::Result;
use devflow_core::config::Config;
use devflow_core::state::LocalStateManager;
use devflow_core::vcs;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WorkspaceContextSource {
    EnvOverride,
    Cwd,
    None,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceContext {
    /// Exact raw VCS workspace used as devflow context (env override or cwd).
    pub(crate) context_workspace: Option<String>,
    /// Raw VCS workspace currently checked out in this directory.
    pub(crate) cwd_workspace: Option<String>,
    pub(crate) source: WorkspaceContextSource,
}

pub(crate) fn resolve_workspace_context() -> WorkspaceContext {
    let cwd_workspace = vcs::detect_vcs_provider(".")
        .ok()
        .and_then(|repo| repo.current_workspace().ok().flatten());

    if let Ok(env_value) = std::env::var("DEVFLOW_CONTEXT_BRANCH") {
        let trimmed = env_value.trim();
        if !trimmed.is_empty() {
            return WorkspaceContext {
                context_workspace: Some(trimmed.to_string()),
                cwd_workspace,
                source: WorkspaceContextSource::EnvOverride,
            };
        }
    }

    if let Some(cwd) = cwd_workspace.as_deref() {
        return WorkspaceContext {
            context_workspace: Some(cwd.to_string()),
            cwd_workspace,
            source: WorkspaceContextSource::Cwd,
        };
    }

    WorkspaceContext {
        context_workspace: None,
        cwd_workspace: None,
        source: WorkspaceContextSource::None,
    }
}

pub(super) fn linked_workspace_exists(config_path: &Option<PathBuf>, workspace_name: &str) -> bool {
    if config_path.is_none() {
        return false;
    }
    let project_dir = crate::cli::operation_project_dir(config_path);

    LocalStateManager::new()
        .ok()
        .and_then(|state| state.get_workspace_by_dir(&project_dir, workspace_name))
        .is_some()
}

pub(crate) fn ensure_default_workspace_registered(
    config: &Config,
    config_path: &Option<PathBuf>,
) -> Result<()> {
    let Some(path) = config_path.as_ref() else {
        return Ok(());
    };
    let project_dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    LocalStateManager::new()?.ensure_default_workspace(project_dir, &config.git.main_workspace)?;
    Ok(())
}

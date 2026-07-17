use std::path::PathBuf;

use anyhow::Result;
use devflow_core::config::Config;
use devflow_core::state::LocalStateManager;
use devflow_core::vcs;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BranchContextSource {
    EnvOverride,
    Cwd,
    None,
}

#[derive(Debug, Clone)]
pub(crate) struct BranchContext {
    /// Exact raw VCS workspace used as devflow context (env override or cwd).
    pub(crate) context_branch: Option<String>,
    /// Raw VCS workspace currently checked out in this directory.
    pub(crate) cwd_branch: Option<String>,
    pub(crate) source: BranchContextSource,
}

pub(crate) fn resolve_branch_context() -> BranchContext {
    let cwd_branch = vcs::detect_vcs_provider(".")
        .ok()
        .and_then(|repo| repo.current_workspace().ok().flatten());

    if let Ok(env_branch) = std::env::var("DEVFLOW_CONTEXT_BRANCH") {
        let trimmed = env_branch.trim();
        if !trimmed.is_empty() {
            return BranchContext {
                context_branch: Some(trimmed.to_string()),
                cwd_branch,
                source: BranchContextSource::EnvOverride,
            };
        }
    }

    if let Some(cwd) = cwd_branch.as_deref() {
        return BranchContext {
            context_branch: Some(cwd.to_string()),
            cwd_branch,
            source: BranchContextSource::Cwd,
        };
    }

    BranchContext {
        context_branch: None,
        cwd_branch: None,
        source: BranchContextSource::None,
    }
}

pub(crate) fn context_matches_branch(context_branch: Option<&str>, workspace_name: &str) -> bool {
    context_branch == Some(workspace_name)
}

pub(super) fn linked_workspace_exists(config_path: &Option<PathBuf>, workspace_name: &str) -> bool {
    let Some(path) = config_path.as_ref() else {
        return false;
    };

    LocalStateManager::new()
        .ok()
        .and_then(|state| state.get_workspace(path, workspace_name))
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

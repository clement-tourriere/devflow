use anyhow::Result;
use std::path::Path;

use super::{CreateWorkspaceResult, LifecycleOptions};
use crate::config::Config;

/// Options specific to workspace creation.
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    /// Shared lifecycle options.
    pub lifecycle: LifecycleOptions,
    /// Parent workspace to branch from (like `--from`).
    pub from_workspace: Option<String>,
    /// Override the config `worktree.copy_files` for this creation.
    pub copy_files: Option<Vec<String>>,
    /// Override the config `worktree.copy_ignored` for this creation.
    pub copy_ignored: Option<bool>,
}

/// Create a new workspace using the same core lifecycle as switching with
/// `create_if_missing`.
///
/// This intentionally delegates to `switch::switch_workspace` so CLI, TUI, and
/// GUI paths cannot drift into separate branch/worktree/service/process
/// semantics.
pub async fn create_workspace(
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
    options: &CreateOptions,
) -> Result<CreateWorkspaceResult> {
    let result = super::switch::switch_workspace(
        config,
        project_dir,
        workspace_name,
        &super::switch::SwitchOptions {
            lifecycle: options.lifecycle.clone(),
            create_if_missing: true,
            from_workspace: options.from_workspace.clone(),
            copy_files: options.copy_files.clone(),
            copy_ignored: options.copy_ignored,
        },
    )
    .await?;

    Ok(CreateWorkspaceResult {
        workspace: result.workspace,
        service_key: result.service_key,
        parent: result.parent,
        worktree: result.worktree,
        vcs_ref_created: result.vcs_ref_created,
        services: result.services,
        processes: result.processes,
        hooks: result.hooks,
    })
}

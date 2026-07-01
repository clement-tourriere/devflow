use anyhow::Result;
use std::path::Path;

use super::{CreateWorkspaceResult, LifecycleOptions, WorkspaceCreationMode};
use crate::config::Config;

/// Options specific to workspace creation.
#[derive(Debug, Clone)]
pub struct CreateOptions {
    /// Shared lifecycle options.
    pub lifecycle: LifecycleOptions,
    /// How to create the workspace (worktree, branch, or default from config).
    pub creation_mode: WorkspaceCreationMode,
    /// Parent workspace to branch from (like `--from`).
    pub from_workspace: Option<String>,
    /// Override the config `worktree.copy_files` for this creation.
    pub copy_files: Option<Vec<String>>,
    /// Override the config `worktree.copy_ignored` for this creation.
    pub copy_ignored: Option<bool>,
    /// Whether the workspace should be sandboxed.
    pub sandboxed: Option<bool>,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            lifecycle: LifecycleOptions::default(),
            creation_mode: WorkspaceCreationMode::Default,
            from_workspace: None,
            copy_files: None,
            copy_ignored: None,
            sandboxed: None,
        }
    }
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
            creation_mode: options.creation_mode,
            from_workspace: options.from_workspace.clone(),
            copy_files: options.copy_files.clone(),
            copy_ignored: options.copy_ignored,
            sandboxed: options.sandboxed,
        },
    )
    .await?;

    Ok(CreateWorkspaceResult {
        workspace: result.workspace,
        parent: result.parent,
        worktree: result.worktree,
        branch_created: result.branch_created,
        services: result.services,
        processes: result.processes,
        hooks: result.hooks,
    })
}

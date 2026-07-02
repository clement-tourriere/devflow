//! Shared workspace-list enrichment.
//!
//! One definition of "which workspaces exist, which is current, which is the
//! default, and where their worktrees live" — used by the frontends so they
//! cannot drift into different answers.

use anyhow::Result;
use std::path::Path;

use crate::config::Config;
use crate::state::LocalStateManager;
use crate::vcs;

/// A registry workspace enriched with VCS state.
#[derive(Debug, Clone)]
pub struct EnrichedWorkspace {
    pub name: String,
    pub is_current: bool,
    pub is_default: bool,
    pub worktree_path: Option<String>,
    pub parent: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub executed_command: Option<String>,
    pub execution_status: Option<String>,
}

/// List registered workspaces for `project_dir`, enriched with the current
/// VCS workspace and worktree paths. Registers the main workspace on first
/// use.
pub fn enriched_workspaces(config: &Config, project_dir: &Path) -> Result<Vec<EnrichedWorkspace>> {
    let main_workspace = config.git.main_workspace.clone();

    let mut state_mgr = LocalStateManager::new()?;
    let registry = state_mgr.get_or_init_workspaces_by_dir(project_dir, &main_workspace)?;

    let vcs_provider = vcs::detect_vcs_provider(project_dir).ok();
    let current_vcs_workspace = vcs_provider
        .as_ref()
        .and_then(|v| v.current_workspace().ok().flatten());
    let normalized_current = current_vcs_workspace
        .as_deref()
        .map(|w| config.get_normalized_workspace_name(w));

    let worktrees = vcs_provider
        .as_ref()
        .and_then(|v| v.list_worktrees().ok())
        .unwrap_or_default();

    Ok(registry
        .into_iter()
        .map(|entry| {
            let is_current = normalized_current
                .as_deref()
                .map(|cur| cur == entry.name)
                .unwrap_or(false);
            let is_default = entry.name == main_workspace;

            // Prefer the live worktree path from the VCS, falling back to the
            // registry (covers worktrees listed while the VCS is unavailable).
            let worktree_path = worktrees
                .iter()
                .find(|wt| wt.workspace.as_deref() == Some(&entry.name))
                .map(|wt| wt.path.display().to_string())
                .or(entry.worktree_path);

            EnrichedWorkspace {
                name: entry.name,
                is_current,
                is_default,
                worktree_path,
                parent: entry.parent,
                created_at: entry.created_at,
                executed_command: entry.executed_command,
                execution_status: entry.execution_status,
            }
        })
        .collect())
}

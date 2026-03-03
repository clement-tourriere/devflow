pub mod create;
pub mod delete;
pub mod hooks;
pub mod switch;
pub mod worktree;

use crate::services::factory::OrchestrationResult;
use serde::Serialize;
use std::path::PathBuf;

/// How a workspace was created (worktree vs. classic branch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceCreationMode {
    /// Use the project's `.devflow.yml` worktree config (worktree if enabled, branch otherwise).
    Default,
    /// Force worktree creation regardless of config.
    Worktree,
    /// Force classic branch-only mode (no worktree).
    Branch,
}

impl WorkspaceCreationMode {
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        match raw
            .unwrap_or("default")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "default" => Ok(Self::Default),
            "worktree" => Ok(Self::Worktree),
            "branch" => Ok(Self::Branch),
            other => Err(format!(
                "Invalid workspace creation mode '{}'. Use: default, worktree, branch",
                other
            )),
        }
    }
}

/// Result of a worktree setup operation.
#[derive(Debug, Clone)]
pub struct WorktreeSetupResult {
    /// Resolved absolute path to the worktree.
    pub path: PathBuf,
    /// Whether CoW (APFS clone / reflink) was used.
    pub cow_used: bool,
    /// Whether the worktree was freshly created (vs. already existing).
    pub created: bool,
}

/// Per-service result in a lifecycle operation.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceResult {
    pub service_name: String,
    pub success: bool,
    pub message: String,
}

impl From<OrchestrationResult> for ServiceResult {
    fn from(r: OrchestrationResult) -> Self {
        Self {
            service_name: r.service_name,
            success: r.success,
            message: r.message,
        }
    }
}

/// Result of `create_workspace()`.
#[derive(Debug, Clone)]
pub struct CreateWorkspaceResult {
    /// Normalized workspace name.
    pub workspace: String,
    /// Parent workspace (if this was a newly created branch).
    pub parent: Option<String>,
    /// Worktree details (if worktree mode was used).
    pub worktree: Option<WorktreeSetupResult>,
    /// Whether the VCS branch was freshly created.
    pub branch_created: bool,
    /// Per-service results from orchestration.
    pub services: Vec<ServiceResult>,
}

/// Result of `switch_workspace()`.
#[derive(Debug, Clone)]
pub struct SwitchWorkspaceResult {
    /// Normalized workspace name.
    pub workspace: String,
    /// Parent workspace (if the branch was freshly created).
    pub parent: Option<String>,
    /// Worktree details (if worktree mode was used).
    pub worktree: Option<WorktreeSetupResult>,
    /// Whether the VCS branch was freshly created during the switch.
    pub branch_created: bool,
    /// Per-service results from orchestration.
    pub services: Vec<ServiceResult>,
}

/// Result of `delete_workspace()`.
#[derive(Debug, Clone)]
pub struct DeleteWorkspaceResult {
    /// Workspace that was deleted.
    pub workspace: String,
    /// Whether a worktree was removed.
    pub worktree_removed: bool,
    /// Filesystem path of the removed worktree (if any).
    pub worktree_path: Option<String>,
    /// Whether the VCS branch was deleted.
    pub branch_deleted: bool,
    /// Per-service results from orchestration.
    pub services: Vec<ServiceResult>,
}

/// Options shared across lifecycle operations.
#[derive(Debug, Clone)]
pub struct LifecycleOptions {
    /// Skip hook execution entirely.
    pub skip_hooks: bool,
    /// Skip service orchestration.
    pub skip_services: bool,
    /// Hook approval mode.
    pub hook_approval: hooks::HookApprovalMode,
    /// Whether hook output should be verbose (headers/footers).
    pub verbose_hooks: bool,
    /// Override `trigger_source` in the hook context (e.g. `"vcs"`, `"cli"`).
    /// When `None`, the default `"cli"` is used.
    pub trigger_source: Option<String>,
    /// Override `vcs_event` in the hook context (e.g. `"post-checkout"`).
    pub vcs_event: Option<String>,
}

impl Default for LifecycleOptions {
    fn default() -> Self {
        Self {
            skip_hooks: false,
            skip_services: false,
            hook_approval: hooks::HookApprovalMode::NoApproval,
            verbose_hooks: false,
            trigger_source: None,
            vcs_event: None,
        }
    }
}

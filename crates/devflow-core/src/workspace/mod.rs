pub mod create;
pub mod delete;
pub mod hooks;
pub mod switch;
pub mod worktree;

use crate::hooks::HookPhase;
use crate::processes::ProcessResult;
use crate::services::factory::OrchestrationResult;
use serde::Serialize;
use std::path::PathBuf;

/// Validate a workspace name before it is used anywhere — as a VCS branch
/// name, a shell-hook template value, a file path component, a database name,
/// or a container name.
///
/// Workspace names flow into `sh -c` hook commands via `{{ workspace }}`, so
/// they must not carry shell metacharacters. Git branch names permit `;`,
/// `$`, backticks, `|`, `()`, `<>`, `&`, `!` and whitespace — all of which
/// would let a malicious workspace name break out of an approved shell hook
/// template (approval is keyed on the template, so one approval covers every
/// workspace). Rejecting these at creation time treats the root cause.
///
/// Allowed: alphanumerics, `/` (e.g. `feature/auth`), `-`, `_`, `.`.
pub fn validate_workspace_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("workspace name must not be empty".to_string());
    }
    if name.starts_with('-') {
        return Err(format!(
            "workspace name must not start with '-' (would be read as a flag): '{}'",
            name
        ));
    }
    for (idx, ch) in name.chars().enumerate() {
        let safe = ch.is_ascii_alphanumeric() || matches!(ch, '/' | '-' | '_' | '.');
        if !safe {
            return Err(format!(
                "workspace name '{}' contains an unsupported character {:?} at position {}.\n\
                 Allowed characters: letters, digits, '/', '-', '_', '.'.\n\
                 This is enforced because workspace names are interpolated into \
                 shell hooks, file paths, database names, and container names.",
                name, ch, idx
            ));
        }
    }
    Ok(())
}

/// How a workspace was created (worktree vs. classic branch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkspaceCreationMode {
    /// Use the project's `.devflow.yml` worktree config (worktree if enabled, branch otherwise).
    #[default]
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

/// Summary of a lifecycle hook phase execution.
#[derive(Debug, Clone, Serialize)]
pub struct LifecycleHookResult {
    pub phase: String,
    pub succeeded: usize,
    pub failed: usize,
    pub skipped: usize,
    pub background: usize,
    pub errors: Vec<String>,
}

impl LifecycleHookResult {
    pub fn from_run_result(
        phase: &HookPhase,
        result: crate::hooks::executor::HookRunResult,
    ) -> Self {
        Self {
            phase: phase.to_string(),
            succeeded: result.succeeded,
            failed: result.failed,
            skipped: result.skipped,
            background: result.background,
            errors: result.errors,
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
    /// Per-process results from runtime orchestration.
    pub processes: Vec<ProcessResult>,
    /// Lifecycle hook summaries that ran during this operation.
    pub hooks: Vec<LifecycleHookResult>,
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
    /// Per-process results from runtime orchestration.
    pub processes: Vec<ProcessResult>,
    /// Lifecycle hook summaries that ran during this operation.
    pub hooks: Vec<LifecycleHookResult>,
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
    /// Per-process results from runtime orchestration.
    pub processes: Vec<ProcessResult>,
    /// Lifecycle hook summaries that ran during this operation.
    pub hooks: Vec<LifecycleHookResult>,
}

/// Options shared across lifecycle operations.
#[derive(Debug, Clone)]
pub struct LifecycleOptions {
    /// Skip hook execution entirely.
    pub skip_hooks: bool,
    /// Skip service orchestration.
    pub skip_services: bool,
    /// Skip process orchestration.
    pub skip_processes: bool,
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
            skip_processes: false,
            hook_approval: hooks::HookApprovalMode::NoApproval,
            verbose_hooks: false,
            trigger_source: None,
            vcs_event: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::validate_workspace_name;

    #[test]
    fn test_validate_workspace_name_accepts_common_names() {
        assert!(validate_workspace_name("main").is_ok());
        assert!(validate_workspace_name("feature/auth").is_ok());
        assert!(validate_workspace_name("fix-123").is_ok());
        assert!(validate_workspace_name("release_2.0").is_ok());
        assert!(validate_workspace_name("agent/task-42").is_ok());
    }

    #[test]
    fn test_validate_workspace_name_rejects_shell_metacharacters() {
        // The primary threat: a branch name that breaks out of an approved
        // `sh -c` hook template.
        assert!(validate_workspace_name("foo;evil").is_err());
        assert!(validate_workspace_name("foo$HOME").is_err());
        assert!(validate_workspace_name("foo`whoami`").is_err());
        assert!(validate_workspace_name("foo|curl evil").is_err());
        assert!(validate_workspace_name("foo&(echo x)").is_err());
        assert!(validate_workspace_name("foo>bar").is_err());
        assert!(validate_workspace_name("foo!bar").is_err());
        assert!(validate_workspace_name("foo bar").is_err()); // whitespace
    }

    #[test]
    fn test_validate_workspace_name_rejects_empty_and_flag_prefix() {
        assert!(validate_workspace_name("").is_err());
        assert!(validate_workspace_name("-x").is_err());
    }
}

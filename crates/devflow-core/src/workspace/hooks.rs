use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::hooks::{self, HookEngine, HookPhase};

use super::LifecycleOptions;

/// How hook approval is handled for a lifecycle operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookApprovalMode {
    /// CLI default: interactive prompts for approval.
    Interactive,
    /// CLI --json / --non-interactive: require pre-approval, no prompts.
    NonInteractive,
    /// GUI: skip approval entirely (user-initiated actions are implicitly approved).
    NoApproval,
}

/// Run hooks for a lifecycle phase.
///
/// Handles context building, worktree-aware working directory, and engine
/// creation per `approval_mode`. If no hooks are configured for the phase
/// this is a no-op.
pub async fn run_lifecycle_hooks(
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
    phase: HookPhase,
    opts: &LifecycleOptions,
) -> Result<()> {
    let Some(ref hooks_config) = config.hooks else {
        return Ok(());
    };
    if hooks_config.is_empty() {
        return Ok(());
    }

    let mut context = hooks::build_hook_context(config, project_dir, workspace_name).await;

    // Apply trigger overrides from lifecycle options
    if let Some(ref source) = opts.trigger_source {
        context.trigger_source = source.clone();
    }
    if let Some(ref event) = opts.vcs_event {
        context.vcs_event = Some(event.clone());
    }

    // Use worktree path as working directory when available, so that hooks
    // like `mise trust` run in the correct directory.
    let working_dir = context
        .worktree_path
        .as_ref()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| project_dir.to_path_buf());

    let project_key = project_dir
        .canonicalize()
        .ok()
        .map(|p| p.to_string_lossy().to_string());

    let engine = match opts.hook_approval {
        HookApprovalMode::Interactive => {
            HookEngine::new(hooks_config.clone(), working_dir, project_key)
        }
        HookApprovalMode::NonInteractive => {
            HookEngine::new_non_interactive(hooks_config.clone(), working_dir, project_key)
        }
        HookApprovalMode::NoApproval => {
            HookEngine::new_no_approval(hooks_config.clone(), working_dir)
        }
    };

    let engine = engine.with_quiet_output(!opts.verbose_hooks);

    if opts.verbose_hooks {
        engine.run_phase_verbose(&phase, &context).await?;
    } else {
        engine.run_phase(&phase, &context).await?;
    }

    Ok(())
}

/// Best-effort hook execution — logs warnings but never fails the caller.
pub async fn run_lifecycle_hooks_best_effort(
    config: &Config,
    project_dir: &Path,
    workspace_name: &str,
    phase: HookPhase,
    opts: &LifecycleOptions,
) {
    if let Err(e) = run_lifecycle_hooks(config, project_dir, workspace_name, phase.clone(), opts)
        .await
    {
        log::warn!("Hook phase {:?} failed: {}", phase, e);
    }
}

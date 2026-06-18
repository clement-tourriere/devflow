use anyhow::{Context, Result};
use devflow_core::config::Config;
use devflow_core::hooks::HookPhase;
use devflow_core::vcs;
use std::path::PathBuf;

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_merge_command(
    config: &Config,
    target: Option<&str>,
    cleanup_flag: bool,
    dry_run: bool,
    json_output: bool,
    force: bool,
    check_only: bool,
    cascade_rebase_flag: bool,
) -> Result<()> {
    // Resolve effective values from config + CLI flags
    let merge_defaults = config.merge.clone().unwrap_or_default();
    let cleanup = merge_defaults.effective_cleanup(cleanup_flag);
    let cascade_rebase = merge_defaults.effective_cascade_rebase(cascade_rebase_flag);
    let strategy = merge_defaults.effective_strategy();
    let vcs_repo = vcs::detect_vcs_provider(".").context("Failed to open VCS repository")?;

    if vcs_repo.provider_name() != "git" {
        anyhow::bail!(
            "Merge is currently supported for git repositories only (detected: {}).",
            vcs_repo.provider_name()
        );
    }

    let initial_dir = std::env::current_dir().context("Failed to get current directory")?;

    // Determine source workspace (current workspace)
    let source = vcs_repo
        .current_workspace()?
        .ok_or_else(|| anyhow::anyhow!("Could not determine current workspace (detached HEAD?)"))?;

    // Determine target workspace
    let target_workspace = target.unwrap_or(&config.git.main_workspace);

    if !vcs_repo.workspace_exists(target_workspace)? {
        anyhow::bail!(
            "Target workspace '{}' does not exist. Run 'devflow list' to see available workspaces.",
            target_workspace
        );
    }

    if source == target_workspace {
        anyhow::bail!("Source and target workspace are the same: '{}'", source);
    }

    // If a dedicated worktree already exists for the target workspace, perform the
    // merge there to avoid checking out a workspace that may be locked elsewhere.
    let merge_dir = vcs_repo
        .worktree_path(target_workspace)?
        .unwrap_or_else(|| initial_dir.clone());

    // ── Merge readiness checks (gated by smart_merge feature flag) ──
    let smart_merge_enabled = devflow_core::config::GlobalConfig::load()
        .ok()
        .flatten()
        .map(|g| g.smart_merge_enabled())
        .unwrap_or(false);

    if !force && smart_merge_enabled {
        if let Some(ref merge_config) = config.merge {
            let checks = devflow_core::merge::build_checks_from_config(merge_config);
            if !checks.is_empty() {
                if !json_output {
                    println!("Running merge readiness checks...");
                }
                let report = devflow_core::merge::run_checks(
                    &checks,
                    vcs_repo.as_ref(),
                    &source,
                    target_workspace,
                );

                if json_output {
                    if check_only {
                        println!("{}", serde_json::to_string_pretty(&report)?);
                        return Ok(());
                    }
                } else {
                    for check in &report.checks {
                        let icon = if check.passed {
                            "✓"
                        } else if check.severity == devflow_core::merge::CheckSeverity::Error {
                            "✗"
                        } else {
                            "⚠"
                        };
                        println!("  {} {} — {}", icon, check.check_name, check.message);
                        if let Some(ref suggestion) = check.suggestion {
                            println!("    Suggestion: {}", suggestion);
                        }
                    }
                }

                if check_only {
                    if report.ready {
                        println!("\nMerge readiness: READY");
                    } else {
                        println!("\nMerge readiness: NOT READY");
                    }
                    return Ok(());
                }

                if !report.ready {
                    anyhow::bail!("Merge readiness checks failed. Use --force to skip checks.");
                }
            }
        } else if check_only {
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "source": source,
                        "target": target_workspace,
                        "ready": true,
                        "checks": [],
                    }))?
                );
            } else {
                println!("No merge checks configured. Merge is ready.");
            }
            return Ok(());
        }
    }

    if dry_run {
        if json_output {
            let normalized = config.get_normalized_workspace_name(&source);
            let has_worktree = vcs_repo.worktree_path(&source)?.is_some();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "dry_run": true,
                    "source": source,
                    "target": target_workspace,
                    "merge_directory": merge_dir,
                    "cleanup": cleanup,
                    "has_worktree": has_worktree,
                    "normalized_service_branch": normalized,
                }))?
            );
        } else {
            println!("Merge plan:");
            println!("  Source: {}", source);
            println!("  Target: {}", target_workspace);
            if cleanup {
                println!(
                    "  Cleanup: will delete source workspace, worktree, and service workspaces after merge"
                );
            }
            println!("\n[dry-run] No changes made.");
        }
        return Ok(());
    }

    if !json_output {
        println!("Merge plan:");
        println!("  Source: {}", source);
        println!("  Target: {}", target_workspace);
        if cleanup {
            println!(
                "  Cleanup: will delete source workspace, worktree, and service workspaces after merge"
            );
        }
    }

    // Fire pre-merge hooks
    {
        let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let hook_opts = devflow_core::workspace::LifecycleOptions {
            hook_approval: devflow_core::workspace::hooks::HookApprovalMode::Interactive,
            verbose_hooks: !json_output,
            ..Default::default()
        };
        devflow_core::workspace::hooks::run_lifecycle_hooks(
            config,
            &project_dir,
            &source,
            HookPhase::PreMerge,
            &hook_opts,
        )
        .await?;
    }

    // If rebase strategy, rebase source onto target first (before switching to target)
    if strategy == devflow_core::config::MergeStrategy::Rebase {
        if !json_output {
            println!("\nRebasing '{}' onto '{}'...", source, target_workspace);
        }
        // Ensure we're on the source workspace for rebase
        if merge_dir == initial_dir {
            vcs_repo
                .checkout_workspace(&source)
                .context("Failed to checkout source for rebase")?;
        }
        let rebase_result = vcs_repo
            .rebase(target_workspace)
            .context("Rebase failed. Resolve conflicts and try again.")?;
        if !rebase_result.success {
            anyhow::bail!(
                "Rebase had conflicts in: {}",
                rebase_result.conflict_files.join(", ")
            );
        }
        if !json_output {
            println!("Rebased {} commits.", rebase_result.commits_replayed);
        }
    }

    // Perform the merge
    if merge_dir == initial_dir {
        // Merge in the current worktree, so we must first move to target workspace.
        vcs_repo
            .checkout_workspace(target_workspace)
            .with_context(|| {
                format!(
                    "Failed to switch to target workspace '{}' before merge",
                    target_workspace
                )
            })?;
    }

    if !json_output {
        println!("\nMerging '{}' into '{}'...", source, target_workspace);
        if merge_dir != initial_dir {
            println!("Using target worktree: {}", merge_dir.display());
        }
    }
    // Use the VcsProvider merge rather than spawning git CLI
    let merge_vcs =
        vcs::detect_vcs_provider(&merge_dir).context("Failed to open VCS repository for merge")?;
    merge_vcs
        .merge_branch(&source)
        .context("Merge failed. Resolve conflicts and try again.")?;

    if !json_output {
        println!("Merge successful.");
    }

    // Fire post-merge hooks
    {
        let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let hook_opts = devflow_core::workspace::LifecycleOptions {
            hook_approval: devflow_core::workspace::hooks::HookApprovalMode::Interactive,
            verbose_hooks: !json_output,
            ..Default::default()
        };
        devflow_core::workspace::hooks::run_lifecycle_hooks(
            config,
            &project_dir,
            target_workspace,
            HookPhase::PostMerge,
            &hook_opts,
        )
        .await?;
    }

    // ── Cascade report (gated by smart_merge feature flag) ──────────
    let mut cascade_json = serde_json::json!(null);
    if smart_merge_enabled {
        let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        match devflow_core::merge::build_cascade_report(
            merge_vcs.as_ref(),
            &project_dir,
            &source,
            target_workspace,
            config.merge.as_ref(),
        ) {
            Ok(cascade) => {
                if !cascade.affected_children.is_empty() {
                    if !json_output {
                        println!("\nCascade report:");
                        println!(
                            "  Affected child workspaces: {}",
                            cascade.affected_children.join(", ")
                        );
                        for nr in &cascade.needs_rebase {
                            println!("  ⚠ {} needs rebase: {}", nr.workspace, nr.reason);
                        }
                    }

                    // Auto-rebase if requested
                    if cascade_rebase {
                        for nr in &cascade.needs_rebase {
                            if !json_output {
                                println!("  Rebasing '{}'...", nr.workspace);
                            }
                            // Checkout child, rebase onto target
                            if let Ok(child_vcs) = vcs::detect_vcs_provider(&project_dir) {
                                if child_vcs.checkout_workspace(&nr.workspace).is_ok() {
                                    match child_vcs.rebase(target_workspace) {
                                        Ok(result) if result.success => {
                                            if !json_output {
                                                println!(
                                                    "    ✓ Rebased {} commits",
                                                    result.commits_replayed
                                                );
                                            }
                                        }
                                        Ok(result) => {
                                            if !json_output {
                                                println!(
                                                    "    ✗ Rebase conflicts in: {}",
                                                    result.conflict_files.join(", ")
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            if !json_output {
                                                println!("    ✗ Rebase failed: {}", e);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        // Return to target workspace
                        let _ = vcs::detect_vcs_provider(&project_dir)
                            .ok()
                            .and_then(|v| v.checkout_workspace(target_workspace).ok());
                    }

                    cascade_json = serde_json::to_value(&cascade).unwrap_or_default();
                }
            }
            Err(e) => {
                log::warn!("Failed to build cascade report: {}", e);
            }
        }
    }

    let mut cleanup_result = serde_json::json!(null);

    // Cleanup if requested — delegate to the core workspace delete lifecycle
    if cleanup {
        if !json_output {
            println!("\nCleaning up source workspace '{}'...", source);
        }

        // Safety: if we are still on the source workspace, detach HEAD first
        // so the branch becomes deletable.
        if let Ok(Some(current)) = vcs_repo.current_workspace() {
            if current == source {
                if let Err(e) = vcs_repo.detach_head() {
                    log::warn!(
                        "Failed to detach HEAD before deleting workspace '{}': {}",
                        source,
                        e
                    );
                }
            }
        }

        let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        let delete_options = devflow_core::workspace::delete::DeleteOptions {
            lifecycle: devflow_core::workspace::LifecycleOptions {
                skip_hooks: false,
                skip_services: false,
                hook_approval: devflow_core::workspace::hooks::HookApprovalMode::Interactive,
                verbose_hooks: !json_output,
                ..Default::default()
            },
            keep_services: false,
            // Never discard uncommitted work as a side effect of merge;
            // a dirty source worktree must be removed explicitly.
            force: false,
        };

        let delete_result = devflow_core::workspace::delete::delete_workspace(
            config,
            &project_dir,
            &source,
            &delete_options,
        )
        .await;

        match delete_result {
            Ok(result) => {
                if !json_output {
                    if result.worktree_removed {
                        if let Some(ref wt) = result.worktree_path {
                            println!("Removed worktree: {}", wt);
                        }
                    }
                    for r in &result.services {
                        if r.success {
                            println!("{}", r.message);
                        } else {
                            println!("Warning: {}", r.message);
                        }
                    }
                    if result.branch_deleted {
                        println!("Deleted workspace: {}", source);
                    }
                    println!("Cleanup complete.");
                }

                let service_json: Vec<serde_json::Value> = result
                    .services
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "service": r.service_name,
                            "success": r.success,
                            "message": r.message,
                        })
                    })
                    .collect();

                cleanup_result = serde_json::json!({
                    "worktree_removed": result.worktree_removed,
                    "branch_deleted": result.branch_deleted,
                    "service_results": service_json,
                });
            }
            Err(e) => {
                log::warn!("Failed to clean up source workspace '{}': {}", source, e);
                if !json_output {
                    println!("Warning: Failed to clean up source workspace: {}", e);
                }
                cleanup_result = serde_json::json!({
                    "error": e.to_string(),
                });
            }
        }
    }

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "ok",
                "source": source,
                "target": target_workspace,
                "cleanup": cleanup_result,
                "cascade": cascade_json,
            }))?
        );
    }

    Ok(())
}

// ── Rebase ─────────────────────────────────────────────────────────────────────

pub(super) async fn handle_rebase_command(
    config: &Config,
    target: Option<&str>,
    dry_run: bool,
    json_output: bool,
) -> Result<()> {
    let smart_merge_enabled = devflow_core::config::GlobalConfig::load()
        .ok()
        .flatten()
        .map(|g| g.smart_merge_enabled())
        .unwrap_or(false);
    if !smart_merge_enabled {
        anyhow::bail!(
            "Rebase requires the smart_merge feature flag. Enable it with:\n  \
             devflow config set smart_merge true\n  \
             or set smart_merge: true in ~/.config/devflow/config.yml"
        );
    }

    let vcs_repo = vcs::detect_vcs_provider(".").context("Failed to open VCS repository")?;

    if vcs_repo.provider_name() != "git" {
        anyhow::bail!(
            "Rebase is currently supported for git repositories only (detected: {}).",
            vcs_repo.provider_name()
        );
    }

    let source = vcs_repo
        .current_workspace()?
        .ok_or_else(|| anyhow::anyhow!("Could not determine current workspace (detached HEAD?)"))?;

    let target_workspace = target.unwrap_or(&config.git.main_workspace);

    if source == target_workspace {
        anyhow::bail!("Cannot rebase '{}' onto itself", source);
    }

    if dry_run {
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "dry_run": true,
                    "source": source,
                    "target": target_workspace,
                }))?
            );
        } else {
            println!("Rebase plan:");
            println!("  Source: {} (current)", source);
            println!("  Onto: {}", target_workspace);
            println!("\n[dry-run] No changes made.");
        }
        return Ok(());
    }

    // Fire pre-rebase hooks
    {
        let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let hook_opts = devflow_core::workspace::LifecycleOptions {
            hook_approval: devflow_core::workspace::hooks::HookApprovalMode::Interactive,
            verbose_hooks: !json_output,
            ..Default::default()
        };
        devflow_core::workspace::hooks::run_lifecycle_hooks(
            config,
            &project_dir,
            &source,
            HookPhase::PreRebase,
            &hook_opts,
        )
        .await?;
    }

    if !json_output {
        println!("Rebasing '{}' onto '{}'...", source, target_workspace);
    }

    let result = vcs_repo.rebase(target_workspace).context("Rebase failed")?;

    if result.success {
        if !json_output {
            println!(
                "Rebase successful: {} commit(s) replayed.",
                result.commits_replayed
            );
        }

        // Fire post-rebase hooks
        {
            let project_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let hook_opts = devflow_core::workspace::LifecycleOptions {
                hook_approval: devflow_core::workspace::hooks::HookApprovalMode::Interactive,
                verbose_hooks: !json_output,
                ..Default::default()
            };
            devflow_core::workspace::hooks::run_lifecycle_hooks(
                config,
                &project_dir,
                &source,
                HookPhase::PostRebase,
                &hook_opts,
            )
            .await?;
        }
    } else if !json_output {
        println!("Rebase encountered conflicts:");
        for f in &result.conflict_files {
            println!("  - {}", f);
        }
        println!("Rebase aborted. Resolve conflicts manually.");
    }

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }

    Ok(())
}

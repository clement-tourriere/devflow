use anyhow::Result;
use devflow_core::config::Config;

/// Handle `devflow sync-ai-configs` — merge AI tool configs from current worktree
/// back to the main worktree. The actual merge lives in `devflow_core::ai_configs`.
pub(super) fn handle_sync_ai_configs(json_output: bool) -> Result<()> {
    // Same effective-config resolution as the sync-ai-configs hook action,
    // so both paths see the same worktree.extra_ai_dirs.
    let current_dir = std::env::current_dir()?;
    let config = Config::load_effective_for_dir(&current_dir).unwrap_or_default();
    let outcome = devflow_core::ai_configs::sync_ai_configs(&config, &current_dir)?;

    for warning in &outcome.warnings {
        log::warn!("{warning}");
        if !json_output {
            eprintln!("Warning: {warning}");
        }
    }

    if json_output {
        let payload = if outcome.skipped_in_main {
            serde_json::json!({
                "status": "skipped",
                "reason": "already in main worktree",
            })
        } else {
            serde_json::json!({
                "status": "ok",
                "synced_dirs": outcome.synced_dirs,
                "synced_files": outcome.synced_files,
            })
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if outcome.skipped_in_main {
        println!("Already in the main worktree, nothing to sync.");
    } else if outcome.is_noop() {
        println!("No AI configs to sync.");
    } else {
        for f in &outcome.synced_files {
            println!("Merged: {}", f);
        }
        for d in &outcome.synced_dirs {
            println!("Synced: {}/", d);
        }
    }

    Ok(())
}

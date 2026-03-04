use anyhow::Result;
use devflow_core::services::orphan::{cleanup_orphan, detect_orphans, OrphanInfo, OrphanSource};

pub(super) async fn handle_gc_command(
    list: bool,
    all: bool,
    force: bool,
    json_output: bool,
    non_interactive: bool,
) -> Result<()> {
    let orphans = detect_orphans().await?;

    if orphans.is_empty() {
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": "ok",
                    "orphans": [],
                    "message": "No orphaned projects found"
                }))?
            );
        } else {
            println!("No orphaned projects found.");
        }
        return Ok(());
    }

    // ── List mode (or JSON output always includes the list) ──────────
    if json_output {
        let orphan_json: Vec<serde_json::Value> = orphans
            .iter()
            .map(|o| {
                serde_json::json!({
                    "project_name": o.project_name,
                    "project_path": o.project_path,
                    "sources": o.sources,
                    "sqlite_project_id": o.sqlite_project_id,
                    "sqlite_workspace_count": o.sqlite_workspace_count,
                    "container_names": o.container_names,
                    "local_state_service_count": o.local_state_service_count,
                    "local_state_workspace_count": o.local_state_workspace_count,
                })
            })
            .collect();

        if list || (!all && non_interactive) {
            // List-only mode in JSON
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": "ok",
                    "orphans": orphan_json,
                    "count": orphans.len(),
                }))?
            );
            return Ok(());
        }

        // Clean all in JSON mode
        if all {
            let mut results = Vec::new();
            for orphan in &orphans {
                let result = cleanup_orphan(orphan).await;
                results.push(serde_json::json!({
                    "project_name": result.project_name,
                    "containers_removed": result.containers_removed,
                    "sqlite_rows_deleted": result.sqlite_rows_deleted,
                    "local_state_cleared": result.local_state_cleared,
                    "data_dirs_removed": result.data_dirs_removed,
                    "errors": result.errors,
                }));
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": "ok",
                    "orphans": orphan_json,
                    "cleanup_results": results,
                }))?
            );
            return Ok(());
        }

        // Non-interactive without --all: just list
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "ok",
                "orphans": orphan_json,
                "count": orphans.len(),
                "hint": "Use --all to clean up all orphans"
            }))?
        );
        return Ok(());
    }

    // ── Human-readable mode ─────────────────────────────────────────
    fn print_orphan_table(orphans: &[OrphanInfo]) {
        println!(
            "Found {} orphaned project{}:",
            orphans.len(),
            if orphans.len() == 1 { "" } else { "s" }
        );
        println!();

        for (i, orphan) in orphans.iter().enumerate() {
            let sources: Vec<&str> = orphan
                .sources
                .iter()
                .map(|s| match s {
                    OrphanSource::Sqlite => "sqlite",
                    OrphanSource::LocalState => "local-state",
                    OrphanSource::Docker => "docker",
                })
                .collect();

            println!(
                "  {}. {} (sources: {})",
                i + 1,
                orphan.project_name,
                sources.join(", ")
            );

            if let Some(ref path) = orphan.project_path {
                println!("     Path: {} (missing)", path);
            }
            if orphan.sqlite_workspace_count > 0 {
                println!(
                    "     SQLite: {} workspace{}",
                    orphan.sqlite_workspace_count,
                    if orphan.sqlite_workspace_count == 1 {
                        ""
                    } else {
                        "es"
                    }
                );
            }
            if !orphan.container_names.is_empty() {
                println!(
                    "     Docker: {} container{}",
                    orphan.container_names.len(),
                    if orphan.container_names.len() == 1 {
                        ""
                    } else {
                        "s"
                    }
                );
            }
            if orphan.local_state_service_count > 0 || orphan.local_state_workspace_count > 0 {
                println!(
                    "     Local state: {} service{}, {} workspace{}",
                    orphan.local_state_service_count,
                    if orphan.local_state_service_count == 1 {
                        ""
                    } else {
                        "s"
                    },
                    orphan.local_state_workspace_count,
                    if orphan.local_state_workspace_count == 1 {
                        ""
                    } else {
                        "es"
                    }
                );
            }
        }
        println!();
    }

    print_orphan_table(&orphans);

    if list {
        return Ok(());
    }

    // ── Clean all mode ──────────────────────────────────────────────
    if all {
        if !force {
            if non_interactive {
                anyhow::bail!("Use --force to confirm cleanup in non-interactive mode");
            }

            let confirm =
                inquire::Confirm::new("Clean up all orphaned projects? This is irreversible.")
                    .with_default(false)
                    .prompt()?;

            if !confirm {
                println!("Aborted.");
                return Ok(());
            }
        }

        for orphan in &orphans {
            print!("Cleaning up '{}'... ", orphan.project_name);
            let result = cleanup_orphan(orphan).await;

            let mut parts = Vec::new();
            if result.containers_removed > 0 {
                parts.push(format!(
                    "{} container{} removed",
                    result.containers_removed,
                    if result.containers_removed == 1 {
                        ""
                    } else {
                        "s"
                    }
                ));
            }
            if result.sqlite_rows_deleted {
                parts.push("sqlite cleared".to_string());
            }
            if result.local_state_cleared {
                parts.push("local state cleared".to_string());
            }
            if result.data_dirs_removed > 0 {
                parts.push(format!(
                    "{} data dir{} removed",
                    result.data_dirs_removed,
                    if result.data_dirs_removed == 1 {
                        ""
                    } else {
                        "s"
                    }
                ));
            }

            if parts.is_empty() {
                println!("done (nothing to remove)");
            } else {
                println!("done ({})", parts.join(", "));
            }

            for err in &result.errors {
                eprintln!("  Warning: {}", err);
            }
        }

        println!();
        println!("Cleanup complete.");
        return Ok(());
    }

    // ── Interactive selection mode ───────────────────────────────────
    if non_interactive {
        println!("Use --all to clean up all orphans, or --list to just list them.");
        return Ok(());
    }

    let options: Vec<String> = orphans
        .iter()
        .map(|o| {
            let mut details = Vec::new();
            if o.sqlite_workspace_count > 0 {
                details.push(format!("{} sqlite workspaces", o.sqlite_workspace_count));
            }
            if !o.container_names.is_empty() {
                details.push(format!("{} containers", o.container_names.len()));
            }
            if o.local_state_service_count > 0 {
                details.push(format!("{} state entries", o.local_state_service_count));
            }
            if details.is_empty() {
                o.project_name.clone()
            } else {
                format!("{} ({})", o.project_name, details.join(", "))
            }
        })
        .collect();

    let selected = inquire::MultiSelect::new("Select orphans to clean up:", options)
        .with_help_message("Space to select, Enter to confirm, Esc to cancel")
        .prompt();

    let selected = match selected {
        Ok(s) if s.is_empty() => {
            println!("No orphans selected. Nothing to do.");
            return Ok(());
        }
        Ok(s) => s,
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => {
            println!("Cancelled.");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    // Map selected labels back to orphan indices
    let selected_orphans: Vec<&OrphanInfo> = selected
        .iter()
        .filter_map(|label| {
            let name = label.split(" (").next().unwrap_or(label);
            orphans.iter().find(|o| o.project_name == name)
        })
        .collect();

    for orphan in &selected_orphans {
        print!("Cleaning up '{}'... ", orphan.project_name);
        let result = cleanup_orphan(orphan).await;

        let mut parts = Vec::new();
        if result.containers_removed > 0 {
            parts.push(format!("{} containers removed", result.containers_removed));
        }
        if result.sqlite_rows_deleted {
            parts.push("sqlite cleared".to_string());
        }
        if result.local_state_cleared {
            parts.push("local state cleared".to_string());
        }
        if result.data_dirs_removed > 0 {
            parts.push(format!("{} data dirs removed", result.data_dirs_removed));
        }

        if parts.is_empty() {
            println!("done (nothing to remove)");
        } else {
            println!("done ({})", parts.join(", "));
        }

        for err in &result.errors {
            eprintln!("  Warning: {}", err);
        }
    }

    println!();
    println!(
        "Cleaned up {} orphaned project{}.",
        selected_orphans.len(),
        if selected_orphans.len() == 1 { "" } else { "s" }
    );

    Ok(())
}

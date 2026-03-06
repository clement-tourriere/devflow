use anyhow::{Context, Result};
use devflow_core::config::Config;
use devflow_core::merge::train::MergeTrainEngine;
use devflow_core::vcs;

use super::TrainAction;

pub async fn handle_train_command(
    config: &Config,
    action: TrainAction,
    json_output: bool,
) -> Result<()> {
    let smart_merge_enabled = devflow_core::config::GlobalConfig::load()
        .ok()
        .flatten()
        .map(|g| g.smart_merge_enabled())
        .unwrap_or(false);
    if !smart_merge_enabled {
        anyhow::bail!(
            "Merge train requires the smart_merge feature flag. Enable it with:\n  \
             devflow config set smart_merge true\n  \
             or set smart_merge: true in ~/.config/devflow/config.yml"
        );
    }

    let project_dir = std::env::current_dir().context("Failed to get current directory")?;

    match action {
        TrainAction::Add { target, workspace } => {
            let target = target
                .as_deref()
                .unwrap_or(&config.git.main_workspace);

            let workspace = match workspace {
                Some(w) => w,
                None => {
                    let vcs_repo = vcs::detect_vcs_provider(".")
                        .context("Failed to open VCS repository")?;
                    vcs_repo
                        .current_workspace()?
                        .ok_or_else(|| anyhow::anyhow!("Could not determine current workspace"))?
                }
            };

            let engine = MergeTrainEngine::new(&project_dir, config);
            engine.enqueue(&workspace, target)?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "action": "add",
                        "workspace": workspace,
                        "target": target,
                    }))?
                );
            } else {
                println!("Added '{}' to merge train for '{}'", workspace, target);
            }
        }
        TrainAction::Remove { workspace } => {
            let engine = MergeTrainEngine::new(&project_dir, config);
            engine.dequeue(&workspace)?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "action": "remove",
                        "workspace": workspace,
                    }))?
                );
            } else {
                println!("Removed '{}' from merge train", workspace);
            }
        }
        TrainAction::Status { target } => {
            let target = target
                .as_deref()
                .unwrap_or(&config.git.main_workspace);

            let engine = MergeTrainEngine::new(&project_dir, config);
            let train = engine.status(target)?;

            if json_output {
                println!("{}", serde_json::to_string_pretty(&train)?);
            } else if let Some(train) = train {
                println!("Merge train for '{}' ({:?}):", train.target, train.status);
                if train.entries.is_empty() {
                    println!("  (empty)");
                } else {
                    for entry in &train.entries {
                        let status_icon = match entry.status {
                            devflow_core::merge::train::MergeTrainEntryStatus::Queued => "○",
                            devflow_core::merge::train::MergeTrainEntryStatus::Checking => "◉",
                            devflow_core::merge::train::MergeTrainEntryStatus::Merging => "◉",
                            devflow_core::merge::train::MergeTrainEntryStatus::Succeeded => "✓",
                            devflow_core::merge::train::MergeTrainEntryStatus::Failed => "✗",
                            devflow_core::merge::train::MergeTrainEntryStatus::NeedsRebase => "⚠",
                            devflow_core::merge::train::MergeTrainEntryStatus::Cancelled => "⊘",
                        };
                        println!(
                            "  {} {} [{:?}]",
                            status_icon, entry.workspace, entry.status
                        );
                        if let Some(ref err) = entry.error {
                            println!("    Error: {}", err);
                        }
                    }
                }
            } else {
                println!("No merge train found for '{}'", target);
            }
        }
        TrainAction::Run {
            target,
            stop_on_failure,
            cleanup,
        } => {
            let target = target
                .as_deref()
                .unwrap_or(&config.git.main_workspace);

            if !json_output {
                println!("Running merge train for '{}'...", target);
            }

            let engine = MergeTrainEngine::new(&project_dir, config);
            let results = engine.run(target, stop_on_failure, cleanup)?;

            if json_output {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                let succeeded = results
                    .iter()
                    .filter(|r| {
                        r.status == devflow_core::merge::train::MergeTrainEntryStatus::Succeeded
                    })
                    .count();
                let failed = results.len() - succeeded;
                println!(
                    "\nMerge train complete: {} succeeded, {} failed",
                    succeeded, failed
                );
                for entry in &results {
                    let icon = if entry.status
                        == devflow_core::merge::train::MergeTrainEntryStatus::Succeeded
                    {
                        "✓"
                    } else {
                        "✗"
                    };
                    println!("  {} {} [{:?}]", icon, entry.workspace, entry.status);
                    if let Some(ref err) = entry.error {
                        println!("    {}", err);
                    }
                }
            }
        }
        TrainAction::Pause { target } => {
            let target = target
                .as_deref()
                .unwrap_or(&config.git.main_workspace);

            let engine = MergeTrainEngine::new(&project_dir, config);
            engine.pause(target)?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "action": "pause",
                        "target": target,
                    }))?
                );
            } else {
                println!("Paused merge train for '{}'", target);
            }
        }
        TrainAction::Resume { target } => {
            let target = target
                .as_deref()
                .unwrap_or(&config.git.main_workspace);

            let engine = MergeTrainEngine::new(&project_dir, config);
            engine.resume(target)?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "action": "resume",
                        "target": target,
                    }))?
                );
            } else {
                println!("Resumed merge train for '{}'", target);
            }
        }
    }

    Ok(())
}

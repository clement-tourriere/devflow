use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use devflow_core::config::Config;
use devflow_core::vcs;

pub(super) fn copy_worktree_files(config: &Config, main_worktree_dir: &str) -> Result<()> {
    let wt_config = match config.worktree {
        Some(ref wt) => wt,
        None => return Ok(()),
    };

    let main_dir = std::path::Path::new(main_worktree_dir);
    let current_dir = std::env::current_dir()?;

    // 1. Copy explicitly listed files
    for file in &wt_config.copy_files {
        let source = main_dir.join(file);
        let target = current_dir.join(file);

        if source.exists() && !target.exists() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            reflink_copy::reflink_or_copy(&source, &target)?;
            println!("Copied {} from main worktree", file);
        }
    }

    // 2. Copy gitignored entries when copy_ignored is enabled.
    // Uses list_ignored_entries() for collapsed directory-level entries
    // (e.g. "node_modules" as one entry) instead of enumerating every file.
    // Copies run in parallel via rayon for maximum throughput.
    if wt_config.copy_ignored {
        if let Ok(vcs_repo) = vcs::detect_vcs_provider(main_worktree_dir) {
            match vcs_repo.list_ignored_entries() {
                Ok(ignored_entries) => {
                    let count = AtomicUsize::new(0);

                    rayon::scope(|s| {
                        for rel_path in &ignored_entries {
                            let source = main_dir.join(rel_path);
                            let target = current_dir.join(rel_path);
                            let count = &count;

                            if !source.exists() || target.exists() {
                                continue;
                            }

                            s.spawn(move |_| {
                                if source.is_dir() {
                                    devflow_core::workspace::worktree::reflink_copy_dir(
                                        &source, &target,
                                    );
                                    count.fetch_add(1, Ordering::Relaxed);
                                } else if source.is_file() {
                                    if let Some(parent) = target.parent() {
                                        std::fs::create_dir_all(parent).ok();
                                    }
                                    if let Err(e) =
                                        reflink_copy::reflink_or_copy(&source, &target)
                                    {
                                        log::warn!(
                                            "Failed to copy ignored entry '{}': {}",
                                            rel_path.display(),
                                            e
                                        );
                                    } else {
                                        count.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            });
                        }
                    });

                    let copied = count.load(Ordering::Relaxed);
                    if copied > 0 {
                        println!(
                            "Copied {} ignored entr{} from main worktree",
                            copied,
                            if copied == 1 { "y" } else { "ies" }
                        );
                    }
                }
                Err(e) => {
                    log::warn!("Failed to enumerate ignored entries: {}", e);
                }
            }
        }
    }

    Ok(())
}

pub(super) async fn handle_worktree_setup(
    config: &Config,
    config_path: &Option<PathBuf>,
) -> Result<()> {
    let vcs_repo = vcs::detect_vcs_provider(".")?;

    if !vcs_repo.is_worktree() {
        anyhow::bail!(
            "Not inside a VCS worktree. Use this command from within a worktree directory."
        );
    }

    let main_dir = vcs_repo
        .main_worktree_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine main worktree directory"))?;

    // Copy files from main worktree
    copy_worktree_files(config, main_dir.to_str().unwrap_or(""))?;

    // Run normal git-hook logic to create/switch service workspaces
    handle_git_hook(config, config_path, false, None).await?;

    Ok(())
}

pub(super) async fn handle_git_hook(
    config: &Config,
    config_path: &Option<PathBuf>,
    worktree: bool,
    main_worktree_dir: Option<String>,
) -> Result<()> {
    // If called from a worktree, copy files first
    if worktree {
        if let Some(ref main_dir) = main_worktree_dir {
            copy_worktree_files(config, main_dir)?;
        }
    }

    let vcs_repo = vcs::detect_vcs_provider(".")?;

    if let Some(current_git_branch) = vcs_repo.current_workspace()? {
        log::info!("Git hook triggered for workspace: {}", current_git_branch);

        // Check if this workspace should trigger a switch
        if config.should_switch_on_workspace(&current_git_branch) {
            // If switching to main git workspace, use main database
            if current_git_branch == config.git.main_workspace {
                super::workspace::handle_switch_to_main(config, config_path, false, false, false, true, Some("vcs"), Some("post-checkout")).await?;
            } else {
                // For other workspaces, check if we should create them and switch
                if config.should_create_workspace(&current_git_branch) {
                    super::workspace::handle_switch_command(
                        config,
                        &current_git_branch,
                        config_path,
                        false, // create — workspace already exists from git
                        None,  // from
                        false, // no_services
                        false, // no_verify
                        false, // json_output — git hooks are non-interactive
                        true,  // non_interactive
                        Some("vcs"),
                        Some("post-checkout"),
                        None, // copy_ignored — use config default
                    )
                    .await?;
                } else {
                    log::info!(
                        "Git workspace {} configured not to create service workspaces",
                        current_git_branch
                    );
                }
            }
        } else {
            log::info!(
                "Git workspace {} filtered out by auto_switch configuration",
                current_git_branch
            );
        }
    }

    Ok(())
}

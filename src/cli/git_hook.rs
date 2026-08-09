use std::path::PathBuf;

use anyhow::Result;
use devflow_core::config::Config;
use devflow_core::vcs;

pub(super) fn copy_worktree_files(config: &Config, main_worktree_dir: &str) -> Result<()> {
    let main_dir = std::path::Path::new(main_worktree_dir);
    let current_dir = std::env::current_dir()?;

    // Same payload as worktree creation (copy_files, AI config dirs,
    // gitignored entries) — adopting a manually created worktree must not
    // behave differently from `devflow switch -c`.
    let vcs_repo = vcs::detect_vcs_provider(main_worktree_dir)?;
    let copied = devflow_core::workspace::worktree::copy_worktree_payload(
        vcs_repo.as_ref(),
        config,
        main_dir,
        &current_dir,
        None,
        None,
    );

    if copied > 0 {
        println!(
            "Copied {} entr{} from main worktree",
            copied,
            if copied == 1 { "y" } else { "ies" }
        );
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

    // Same composition as the generated hook's
    // `devflow git-hook --worktree --main-worktree-dir <dir>` invocation.
    handle_git_hook(
        config,
        config_path,
        true,
        Some(main_dir.to_string_lossy().into_owned()),
    )
    .await
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

    // Hook scripts installed by pre-worktree-only releases invoke plain
    // `devflow git-hook` on every in-place checkout in the PRIMARY checkout
    // (the new script exits early there instead). Routing those into the
    // switch flow would hard-fail the primary/default invariant on every
    // non-main checkout after a binary upgrade — mirror the new script's
    // primary-checkout guard here so old installed hooks stay silent no-ops.
    if !worktree && !vcs_repo.is_worktree() {
        log::info!(
            "Primary checkout post-checkout hook: no lifecycle action (re-run `devflow install-hooks` to refresh the installed hook script)"
        );
        return Ok(());
    }

    if let Some(current_git_workspace) = vcs_repo.current_workspace()? {
        log::info!(
            "Git hook triggered for workspace: {}",
            current_git_workspace
        );

        // Worktree-only model: the hook's sole job is adopting a linked
        // worktree by provisioning services for its workspace. In-place
        // checkout switching is gone; the default workspace needs no
        // provisioning here.
        if config.should_create_workspace(&current_git_workspace) {
            super::workspace::handle_switch_command(
                config,
                &current_git_workspace,
                config_path,
                false, // create — workspace already exists from git
                None,  // from
                false, // no_services
                false, // no_processes
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
                current_git_workspace
            );
        }
    }

    Ok(())
}

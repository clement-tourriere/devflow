use anyhow::Result;
use devflow_core::config::Config;
use devflow_core::hooks::approval::ApprovalStore;
use devflow_core::services::{self};
use devflow_core::state::LocalStateManager;
use devflow_core::vcs;

/// Check if ZFS auto-setup should be offered during init (Linux only).
/// Returns `Some(data_root)` if a pool was created or already exists,
/// so the caller can set it on the `LocalServiceConfig`.
#[cfg(feature = "service-local")]
pub(super) async fn attempt_zfs_auto_setup(
    non_interactive: bool,
    quiet_output: bool,
) -> Option<String> {
    use devflow_core::services::postgres::local::storage::zfs_setup::*;

    // Use a placeholder path — the actual projects_root hasn't been established yet
    let placeholder = std::path::PathBuf::from("/var/lib/devflow/data/projects");
    let status = check_zfs_setup_status(&placeholder).await;

    match status {
        ZfsSetupStatus::NotSupported => None,
        ZfsSetupStatus::ToolsNotInstalled => {
            if !quiet_output {
                println!();
                println!("Tip: Install ZFS for near-instant Copy-on-Write service branching:");
                println!("  sudo apt install zfsutils-linux");
            }
            None
        }
        ZfsSetupStatus::AlreadyAvailable { root_dataset } => {
            if !quiet_output {
                println!();
                println!(
                    "ZFS dataset '{}' detected - will use ZFS for Copy-on-Write storage.",
                    root_dataset
                );
            }
            None
        }
        ZfsSetupStatus::DevflowPoolExists { mountpoint } => {
            if !quiet_output {
                println!();
                println!(
                    "ZFS pool 'devflow' already exists (mountpoint: {}).",
                    mountpoint
                );
            }
            Some(mountpoint)
        }
        ZfsSetupStatus::ToolsAvailableNoPool => {
            if non_interactive {
                if !quiet_output {
                    println!();
                    println!(
                        "ZFS tools detected but no pool found. Run 'devflow setup-zfs' to create one."
                    );
                }
                return None;
            }

            if quiet_output {
                return None;
            }

            println!();
            println!("ZFS tools detected but no ZFS pool found.");
            println!("devflow can create a file-backed ZFS pool for near-instant Copy-on-Write branching.");
            println!();
            println!("This will:");
            println!("  1. Create a 10G sparse image at /var/lib/devflow/pgdata.img");
            println!("  2. Create ZFS pool 'devflow' with compression=lz4, recordsize=8k");
            println!("  3. Mount at /var/lib/devflow/data");
            println!();
            println!("Note: This requires sudo. The 10G image is sparse (starts at ~0 disk usage, grows as needed).");
            println!();

            let confirm = inquire::Confirm::new("Create a file-backed ZFS pool?")
                .with_default(true)
                .prompt();

            match confirm {
                Ok(true) => {
                    let config = ZfsPoolSetupConfig::default();
                    match create_file_backed_pool(&config).await {
                        Ok(data_root) => {
                            println!("ZFS pool 'devflow' created successfully");
                            println!();
                            Some(data_root)
                        }
                        Err(e) => {
                            eprintln!("Warning: ZFS pool creation failed: {}", e);
                            eprintln!("Continuing without ZFS (will use copy/reflink fallback).");
                            None
                        }
                    }
                }
                Ok(false) => {
                    println!("Skipping ZFS setup. You can run 'devflow setup-zfs' later.");
                    None
                }
                Err(_) => {
                    println!("Skipping ZFS setup.");
                    None
                }
            }
        }
    }
}

pub(super) async fn init_local_service_main(
    config: &Config,
    named_cfg: &devflow_core::config::NamedServiceConfig,
    from: Option<&str>,
    quiet_output: bool,
) {
    match services::factory::create_provider_from_named_config(config, named_cfg).await {
        Ok(be) => {
            match be.create_workspace("main", None).await {
                Ok(info) => {
                    if !quiet_output {
                        println!("Created main workspace");
                    }
                    if let Ok(conn) = be.get_connection_info("main").await {
                        if let Some(ref uri) = conn.connection_string {
                            if !quiet_output {
                                println!("  Connection: {}", uri);
                            }
                        }
                    }
                    if let Some(state) = &info.state {
                        if !quiet_output {
                            println!("  State: {}", state);
                        }
                    }

                    // Seed if --from specified
                    if let Some(source) = from {
                        if !quiet_output {
                            println!("Seeding main workspace from: {}", source);
                        }
                        match be.seed_from_source("main", source).await {
                            Ok(_) => {
                                if !quiet_output {
                                    println!("Seeding completed successfully");
                                }
                            }
                            Err(e) => eprintln!("Warning: seeding failed: {}", e),
                        }
                    }
                }
                Err(e) => {
                    eprintln!(
                        "Warning: could not create main workspace for '{}': {}",
                        named_cfg.name, e
                    );
                    eprintln!("  You can create it later with: devflow service create main");
                }
            }
        }
        Err(e) => {
            eprintln!(
                "Warning: could not initialize service '{}': {}",
                named_cfg.name, e
            );
            eprintln!(
                "  You can create the main workspace later with: devflow service create main"
            );
        }
    }
}

/// Destroy a devflow project and all associated resources.
///
/// This is the inverse of `devflow init`. It removes:
///   1. All service data (containers, databases, workspaces) via destroy_project()
///   2. Git worktrees created by devflow
///   3. VCS hooks installed by devflow
///   4. Workspace registry and local state for this project
///   5. Hook approvals for this project
///   6. Configuration files (.devflow.yml, .devflow.local.yml)
pub(super) async fn handle_destroy_project(
    config: &mut Config,
    config_path: &Option<std::path::PathBuf>,
    force: bool,
    json_output: bool,
    non_interactive: bool,
) -> Result<()> {
    let project_dir = std::env::current_dir()?;
    let project_name = config.name.clone().unwrap_or_else(|| {
        project_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    });

    // Gather preview info
    let vcs_repo = vcs::detect_vcs_provider(".").ok();

    // Inject services from local state so we can destroy them
    if let Some(ref path) = config_path {
        if let Ok(state_mgr) = LocalStateManager::new() {
            if let Some(state_services) = state_mgr.get_services(path) {
                config.services = Some(state_services);
            }
        }
    }

    let service_configs = config.resolve_services();
    let config_file_path = project_dir.join(".devflow.yml");
    let local_config_path = project_dir.join(".devflow.local.yml");

    // Count worktrees
    let worktrees: Vec<vcs::WorktreeInfo> = vcs_repo
        .as_ref()
        .and_then(|repo| repo.list_worktrees().ok())
        .unwrap_or_default();
    // Filter to non-main worktrees (those that devflow would have created)
    let removable_worktrees: Vec<&vcs::WorktreeInfo> =
        worktrees.iter().filter(|wt| !wt.is_main).collect();

    // Confirm unless --force
    if !force {
        if json_output || non_interactive {
            anyhow::bail!(
                "Use --force to confirm project destruction in non-interactive or JSON output mode"
            );
        }

        println!(
            "This will permanently destroy the devflow project '{}':",
            project_name
        );
        println!();

        if !service_configs.is_empty() {
            println!("  Services ({}):", service_configs.len());
            for svc in &service_configs {
                println!("    - {} (all workspaces and data)", svc.name);
            }
        } else {
            println!("  Services: none configured");
        }

        if !removable_worktrees.is_empty() {
            println!("  Worktrees ({}):", removable_worktrees.len());
            for wt in &removable_worktrees {
                println!("    - {}", wt.path.display());
            }
        }

        if vcs_repo.is_some() {
            println!("  VCS hooks: will be uninstalled");
        }

        println!("  Workspace registry: will be cleared");

        if config_file_path.exists() {
            println!("  Config: {} (will be deleted)", config_file_path.display());
        }
        if local_config_path.exists() {
            println!(
                "  Local config: {} (will be deleted)",
                local_config_path.display()
            );
        }

        println!();
        println!("This is irreversible.");

        let confirm = inquire::Confirm::new("Are you sure you want to destroy this project?")
            .with_default(false)
            .prompt()?;

        if !confirm {
            println!("Aborted.");
            return Ok(());
        }
    }

    let mut destroyed_services: Vec<serde_json::Value> = Vec::new();
    let mut worktrees_removed = 0usize;
    let mut hooks_uninstalled = false;
    let mut state_cleared = false;
    let mut config_deleted = false;
    let mut local_config_deleted = false;

    // 1. Destroy all service data
    for svc_config in &service_configs {
        if !json_output {
            println!("Destroying service '{}'...", svc_config.name);
        }
        match services::factory::create_provider_from_named_config(config, svc_config).await {
            Ok(provider) => {
                if provider.supports_destroy() {
                    match provider.destroy_project().await {
                        Ok(workspaces) => {
                            if !json_output {
                                println!(
                                    "  Destroyed '{}': {} workspace(es) removed",
                                    svc_config.name,
                                    workspaces.len()
                                );
                            }
                            destroyed_services.push(serde_json::json!({
                                "service": svc_config.name,
                                "success": true,
                                "workspaces_destroyed": workspaces,
                            }));
                        }
                        Err(e) => {
                            log::warn!("Failed to destroy service '{}': {}", svc_config.name, e);
                            if !json_output {
                                println!(
                                    "  Warning: Failed to destroy '{}': {}",
                                    svc_config.name, e
                                );
                            }
                            destroyed_services.push(serde_json::json!({
                                "service": svc_config.name,
                                "success": false,
                                "error": e.to_string(),
                            }));
                        }
                    }
                } else {
                    // Provider doesn't support destroy — try deleting all workspaces individually
                    match provider.list_workspaces().await {
                        Ok(workspaces) => {
                            let mut deleted = 0;
                            for workspace in &workspaces {
                                if let Err(e) = provider.delete_workspace(&workspace.name).await {
                                    log::warn!(
                                        "Failed to delete workspace '{}' on '{}': {}",
                                        workspace.name,
                                        svc_config.name,
                                        e
                                    );
                                } else {
                                    deleted += 1;
                                }
                            }
                            if !json_output {
                                println!(
                                    "  Deleted {}/{} workspace(es) from '{}'",
                                    deleted,
                                    workspaces.len(),
                                    svc_config.name
                                );
                            }
                            destroyed_services.push(serde_json::json!({
                                "service": svc_config.name,
                                "success": true,
                                "branches_deleted": deleted,
                            }));
                        }
                        Err(e) => {
                            log::warn!(
                                "Failed to list workspaces for service '{}': {}",
                                svc_config.name,
                                e
                            );
                            if !json_output {
                                println!(
                                    "  Warning: Could not clean up '{}': {}",
                                    svc_config.name, e
                                );
                            }
                            destroyed_services.push(serde_json::json!({
                                "service": svc_config.name,
                                "success": false,
                                "error": e.to_string(),
                            }));
                        }
                    }
                }
            }
            Err(e) => {
                log::warn!(
                    "Failed to create provider for service '{}': {}",
                    svc_config.name,
                    e
                );
                if !json_output {
                    println!(
                        "  Warning: Could not initialize '{}': {}",
                        svc_config.name, e
                    );
                }
                destroyed_services.push(serde_json::json!({
                    "service": svc_config.name,
                    "success": false,
                    "error": e.to_string(),
                }));
            }
        }
    }

    // 2. Remove worktrees (if VCS available)
    if let Some(ref repo) = vcs_repo {
        for wt in &removable_worktrees {
            if !json_output {
                println!("Removing worktree: {}", wt.path.display());
            }
            if let Err(e) = repo.remove_worktree(&wt.path) {
                log::warn!("Failed to remove worktree via VCS: {}", e);
                // Fallback to filesystem removal
                if wt.path.exists() {
                    if let Err(e2) = std::fs::remove_dir_all(&wt.path) {
                        log::warn!("Failed to remove worktree directory: {}", e2);
                        if !json_output {
                            println!("  Warning: Could not remove {}: {}", wt.path.display(), e2);
                        }
                        continue;
                    }
                }
            }
            worktrees_removed += 1;
        }
    }

    // 3. Uninstall VCS hooks
    if let Some(ref repo) = vcs_repo {
        match repo.uninstall_hooks() {
            Ok(_) => {
                hooks_uninstalled = true;
                if !json_output {
                    println!("Uninstalled VCS hooks.");
                }
            }
            Err(e) => {
                log::warn!("Failed to uninstall hooks: {}", e);
                if !json_output {
                    println!("Warning: Could not uninstall hooks: {}", e);
                }
            }
        }
    }

    // 4. Clear local state (workspace registry, services, current workspace)
    if let Some(ref path) = config_path {
        if let Ok(mut state_mgr) = LocalStateManager::new() {
            if let Err(e) = state_mgr.remove_project(path) {
                log::warn!("Failed to clear project state: {}", e);
                if !json_output {
                    println!("Warning: Could not clear project state: {}", e);
                }
            } else {
                state_cleared = true;
                if !json_output {
                    println!("Cleared project state and workspace registry.");
                }
            }
        }
    }

    // 5. Clear hook approvals
    if let Some(ref path) = config_path {
        if let Ok(state_mgr) = LocalStateManager::new() {
            if let Some(project_key) = state_mgr.get_project_key_for(path) {
                if let Ok(mut store) = ApprovalStore::load() {
                    if let Err(e) = store.clear_project(&project_key) {
                        log::warn!("Failed to clear hook approvals: {}", e);
                    }
                }
            }
        }
    }

    // 6. Delete config files
    if config_file_path.exists() {
        if let Err(e) = std::fs::remove_file(&config_file_path) {
            log::warn!("Failed to delete config file: {}", e);
            if !json_output {
                println!(
                    "Warning: Could not delete {}: {}",
                    config_file_path.display(),
                    e
                );
            }
        } else {
            config_deleted = true;
            if !json_output {
                println!("Deleted {}", config_file_path.display());
            }
        }
    }
    if local_config_path.exists() {
        if let Err(e) = std::fs::remove_file(&local_config_path) {
            log::warn!("Failed to delete local config file: {}", e);
            if !json_output {
                println!(
                    "Warning: Could not delete {}: {}",
                    local_config_path.display(),
                    e
                );
            }
        } else {
            local_config_deleted = true;
            if !json_output {
                println!("Deleted {}", local_config_path.display());
            }
        }
    }

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "ok",
                "project": project_name,
                "services": destroyed_services,
                "worktrees_removed": worktrees_removed,
                "hooks_uninstalled": hooks_uninstalled,
                "state_cleared": state_cleared,
                "config_deleted": config_deleted,
                "local_config_deleted": local_config_deleted,
            }))?
        );
    } else {
        println!();
        println!("Project '{}' destroyed.", project_name);
    }

    Ok(())
}

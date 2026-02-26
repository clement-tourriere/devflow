use std::path::PathBuf;

use crate::config::{Config, EffectiveConfig};
use crate::services::{self, ServiceBackend};

use crate::docker;
use crate::hooks::{
    approval::ApprovalStore, HookContext, HookEngine, HookEntry, HookPhase, IndexMap,
    ServiceContext,
};
use crate::state::LocalStateManager;
use crate::vcs;
use anyhow::{Context, Result};
use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    #[command(
        about = "Create a new service branch",
        long_about = "Create a new service branch.\n\nCreates Docker containers and/or cloud branches for the specified branch name.\nIf worktrees are enabled, also creates a Git worktree directory.\n\nExamples:\n  devflow create feature-auth\n  devflow create feature-auth --from develop"
    )]
    Create {
        #[arg(help = "Name of the branch to create")]
        branch_name: String,
        #[arg(long, help = "Parent branch to clone from")]
        from: Option<String>,
    },
    #[command(
        about = "Delete a service branch (keeps Git branch and worktree)",
        long_about = "Delete a service branch (keeps Git branch and worktree).\n\nRemoves service branches (containers, cloud branches) but preserves the Git branch\nand any worktree directory. Use 'devflow remove' to delete everything including\nthe Git branch and worktree.\n\nExamples:\n  devflow delete feature-auth"
    )]
    Delete {
        #[arg(help = "Name of the branch to delete")]
        branch_name: String,
    },
    #[command(about = "List all branches (with service + worktree status)")]
    List,
    #[command(
        about = "Initialize devflow configuration",
        long_about = "Initialize devflow configuration.\n\nCreates a .devflow.yml config file and sets up the first service backend.\nOn subsequent runs, adds additional backends to the project.\n\nExamples:\n  devflow init                              # Auto-detect name from directory\n  devflow init myapp                        # Explicit name\n  devflow init myapp --backend neon         # Use Neon cloud backend\n  devflow init analytics --service-type clickhouse  # Add ClickHouse service\n  devflow init myapp --from dump.sql        # Seed from a local dump file\n  devflow init myapp --from postgresql://user:pass@host/db  # Seed from URL"
    )]
    Init {
        #[arg(help = "Database/backend name (defaults to project directory name)")]
        name: Option<String>,
        #[arg(long, help = "Force overwrite existing configuration")]
        force: bool,
        #[arg(
            long,
            help = "Backend type to use (local, postgres_template, neon, dblab, xata)"
        )]
        backend: Option<String>,
        #[arg(
            long,
            help = "Seed main branch from source (PostgreSQL URL, file path, or s3:// URL)"
        )]
        from: Option<String>,
    },
    #[command(about = "Clean up old service branches")]
    Cleanup {
        #[arg(long, help = "Maximum number of branches to keep")]
        max_count: Option<usize>,
    },
    #[command(about = "Show current configuration")]
    Config {
        #[arg(
            short,
            long,
            help = "Show effective configuration with precedence details"
        )]
        verbose: bool,
    },
    #[command(about = "Install Git hooks")]
    InstallHooks,
    #[command(about = "Uninstall Git hooks")]
    UninstallHooks,
    #[command(about = "Handle Git hook execution", hide = true)]
    GitHook {
        #[arg(long, hide = true)]
        worktree: bool,
        #[arg(long, hide = true)]
        main_worktree_dir: Option<String>,
    },
    #[command(
        about = "Switch to a branch (creates worktree/service branches if needed)",
        long_about = "Switch to a branch (creates worktree/service branches if needed).\n\nWith no arguments, shows an interactive branch picker with fuzzy search.\nWith a branch name, switches to that branch, creating service branches and\nworktrees if they don't exist.\n\nExamples:\n  devflow switch                     # Interactive picker\n  devflow switch feature-auth        # Switch to existing branch\n  devflow switch -c feature-new      # Create new Git branch and switch\n  devflow switch --template           # Switch to main/template\n  devflow switch feature-auth -x 'npm run migrate'  # Run command after switch"
    )]
    Switch {
        #[arg(
            help = "Branch name to switch to (optional - if omitted, shows interactive selection)"
        )]
        branch_name: Option<String>,
        #[arg(short = 'c', long, help = "Create a new branch before switching")]
        create: bool,
        #[arg(short, long, help = "Base branch for new branch creation")]
        base: Option<String>,
        #[arg(short = 'x', long, help = "Run a command after switching")]
        execute: Option<String>,
        #[arg(long, help = "Skip service branching (only VCS switch)")]
        no_services: bool,
        #[arg(long, help = "Skip hook execution")]
        no_verify: bool,
        #[arg(long, help = "Switch to main database (template/development database)")]
        template: bool,
        #[arg(long, help = "Simulate switching without actual operations")]
        dry_run: bool,
    },
    #[command(about = "Start a stopped branch container (local backend)")]
    Start {
        #[arg(help = "Name of the branch to start")]
        branch_name: String,
    },
    #[command(about = "Stop a running branch container (local backend)")]
    Stop {
        #[arg(help = "Name of the branch to stop")]
        branch_name: String,
    },
    #[command(about = "Reset a branch to its parent state (local backend)")]
    Reset {
        #[arg(help = "Name of the branch to reset")]
        branch_name: String,
    },
    #[command(about = "Run diagnostics and check system health")]
    Doctor,
    #[command(
        about = "Show connection info for a service branch",
        long_about = "Show connection info for a service branch.\n\nOutputs connection details in various formats for use in scripts and configuration.\n\nExamples:\n  devflow connection feature-auth              # Connection URI\n  devflow connection feature-auth --format env  # Environment variables\n  devflow connection feature-auth --format json # JSON object"
    )]
    Connection {
        #[arg(help = "Name of the branch")]
        branch_name: String,
        #[arg(long, help = "Output format: uri, env, or json")]
        format: Option<String>,
    },
    #[command(about = "Show current project and backend status")]
    Status,
    #[command(about = "Destroy all branches and data for a service (local backend)")]
    Destroy {
        #[arg(long, help = "Skip confirmation prompt")]
        force: bool,
    },
    #[command(
        about = "Remove a branch, its worktree, and associated service branches",
        long_about = "Remove a branch, its worktree, and associated service branches.\n\nThis is a comprehensive cleanup command that removes:\n  - The Git branch\n  - The worktree directory (if any)\n  - All associated service branches (containers, cloud branches)\n\nUnlike 'devflow delete' which only removes service branches, 'remove' cleans\nup everything related to the branch.\n\nExamples:\n  devflow remove feature-auth\n  devflow remove feature-auth --force\n  devflow remove feature-auth --keep-services  # Only remove worktree + git branch"
    )]
    Remove {
        #[arg(help = "Branch name to remove")]
        branch_name: String,
        #[arg(long, help = "Skip confirmation prompt")]
        force: bool,
        #[arg(long, help = "Keep service branches (only remove worktree)")]
        keep_services: bool,
    },
    #[command(about = "Merge current branch into target (with optional cleanup)")]
    Merge {
        #[arg(help = "Target branch to merge into (default: main branch)")]
        target: Option<String>,
        #[arg(long, help = "Delete the source branch and worktree after merge")]
        cleanup: bool,
        #[arg(long, help = "Simulate merge without actual operations")]
        dry_run: bool,
    },
    #[command(
        name = "worktree-setup",
        about = "Set up devflow in a Git worktree (copy files, create DB branch)"
    )]
    WorktreeSetup,
    #[command(
        name = "setup-zfs",
        about = "Set up a file-backed ZFS pool for Copy-on-Write storage (Linux)"
    )]
    SetupZfs {
        #[arg(long, default_value = "devflow", help = "ZFS pool name")]
        pool_name: Option<String>,
        #[arg(long, default_value = "10G", help = "Pool image size (sparse file)")]
        size: Option<String>,
    },
    #[command(
        about = "Manage lifecycle hooks",
        long_about = "Manage lifecycle hooks.\n\nHooks are MiniJinja-templated commands that run at specific lifecycle phases\n(post-create, post-switch, pre-merge, etc.). Configure them in .devflow.yml\nunder the 'hooks' section.\n\nExamples:\n  devflow hook show                  # List all configured hooks\n  devflow hook show post-create      # Show hooks for a specific phase\n  devflow hook run post-create       # Run hooks for a phase manually\n  devflow hook approvals             # List approved hooks\n  devflow hook approvals --clear     # Clear all approvals"
    )]
    Hook {
        #[command(subcommand)]
        action: HookCommands,
    },
    #[command(
        name = "shell-init",
        about = "Print shell integration script (eval \"$(devflow shell-init)\")",
        long_about = "Print shell integration script.\n\nThe shell wrapper enables automatic 'cd' into worktree directories after\n'devflow switch'. Without it, switch works but you must cd manually.\n\nAdd to your shell profile:\n  eval \"$(devflow shell-init)\"        # auto-detects shell\n  eval \"$(devflow shell-init bash)\"   # ~/.bashrc\n  eval \"$(devflow shell-init zsh)\"    # ~/.zshrc\n  devflow shell-init fish | source    # ~/.config/fish/config.fish\n\nThis creates a 'devflow' shell wrapper function."
    )]
    ShellInit {
        #[arg(help = "Shell type: bash, zsh, or fish (auto-detected from $SHELL if omitted)")]
        shell: Option<String>,
    },
    #[command(about = "Commit staged changes with optional AI-generated message")]
    Commit {
        #[arg(short, long, help = "Commit message (skips AI generation)")]
        message: Option<String>,
        #[arg(long, help = "Generate commit message using LLM")]
        ai: bool,
        #[arg(
            long,
            help = "Open editor to review/edit AI-generated message before committing"
        )]
        edit: bool,
        #[arg(long, help = "Show generated message without committing")]
        dry_run: bool,
    },
    #[command(
        about = "Manage plugin backends",
        long_about = "Manage plugin backends.\n\nPlugins extend devflow with custom service backends via JSON-over-stdio protocol.\nAny executable that speaks the protocol can be a backend.\n\nExamples:\n  devflow plugin list                # List configured plugin backends\n  devflow plugin check my-plugin     # Verify a plugin works\n  devflow plugin init ./my-plugin.sh # Generate a plugin scaffold"
    )]
    Plugin {
        #[command(subcommand)]
        action: PluginCommands,
    },
    #[command(
        about = "Show container logs for a branch",
        long_about = "Show container logs for a branch.\n\nDisplays stdout/stderr from the Docker container backing a service branch.\nUseful for debugging startup failures, query errors, or crash loops.\n\nExamples:\n  devflow logs main                    # Last 100 lines from main\n  devflow logs feature/auth --tail 50  # Last 50 lines\n  devflow logs main -d analytics       # Logs from a specific backend"
    )]
    Logs {
        branch_name: String,
        #[arg(long, help = "Number of lines to show (default: 100)")]
        tail: Option<usize>,
    },
    #[command(
        about = "Seed a branch from an external source",
        long_about = "Seed a branch database from an external source.\n\nLoads data into an existing branch from a PostgreSQL URL, local dump file,\nor S3 URL. The branch must already exist.\n\nExamples:\n  devflow seed main --from dump.sql                    # Seed from local file\n  devflow seed feature/auth --from postgresql://...     # Seed from live database\n  devflow seed main --from s3://bucket/path/dump.sql   # Seed from S3"
    )]
    Seed {
        branch_name: String,
        #[arg(
            long,
            help = "Source to seed from (PostgreSQL URL, file path, or s3:// URL)"
        )]
        from: String,
    },
}

#[derive(Subcommand)]
pub enum HookCommands {
    #[command(about = "Show configured hooks")]
    Show {
        #[arg(help = "Only show hooks for this phase (e.g. post-switch)")]
        phase: Option<String>,
    },
    #[command(about = "Run hooks for a phase manually")]
    Run {
        #[arg(help = "Hook phase to run (e.g. post-switch, post-service-create)")]
        phase: String,
        #[arg(help = "Run only a specific named hook within the phase")]
        name: Option<String>,
        #[arg(long, help = "Branch name context (defaults to current Git branch)")]
        branch: Option<String>,
    },
    #[command(about = "Manage hook approvals")]
    Approvals {
        #[command(subcommand)]
        action: ApprovalCommands,
    },
}

#[derive(Subcommand)]
pub enum ApprovalCommands {
    #[command(about = "List approved hooks for this project")]
    List,
    #[command(about = "Approve a specific hook command")]
    Add {
        #[arg(help = "The hook command to approve")]
        command: String,
    },
    #[command(about = "Clear all approvals for this project")]
    Clear,
}

#[derive(Subcommand)]
pub enum PluginCommands {
    #[command(about = "List registered plugins and their status")]
    List,
    #[command(about = "Check if a plugin is reachable and responds correctly")]
    Check {
        #[arg(help = "Plugin service name (as defined in backends config)")]
        name: String,
    },
    #[command(about = "Print a skeleton plugin script")]
    Init {
        #[arg(help = "Name for the new plugin")]
        name: String,
        #[arg(
            long,
            default_value = "bash",
            help = "Language for the skeleton: bash or python"
        )]
        lang: String,
    },
}

pub async fn handle_command(
    cmd: Commands,
    json_output: bool,
    _non_interactive: bool,
    database_name: Option<&str>,
) -> Result<()> {
    // Commands that use the new backend system
    let uses_backend = matches!(
        cmd,
        Commands::Create { .. }
            | Commands::Delete { .. }
            | Commands::List
            | Commands::Start { .. }
            | Commands::Stop { .. }
            | Commands::Reset { .. }
            | Commands::Doctor
            | Commands::Connection { .. }
            | Commands::Status
            | Commands::Cleanup { .. }
            | Commands::Destroy { .. }
            | Commands::Switch { .. }
            | Commands::GitHook { .. }
            | Commands::WorktreeSetup
            | Commands::Remove { .. }
            | Commands::Merge { .. }
            | Commands::Logs { .. }
            | Commands::Seed { .. }
    );

    // Check if command requires configuration file
    let requires_config = uses_backend;

    // Load effective configuration (includes local config and environment overrides)
    let (effective_config, config_path) = Config::load_effective_config_with_path_info()?;

    // Early exit if devflow is disabled
    if effective_config.should_exit_early()? {
        if effective_config.is_disabled() {
            log::debug!("devflow is globally disabled via configuration");
        } else {
            log::debug!("devflow is disabled for current branch");
        }
        return Ok(());
    }

    // Check for required config file after checking if disabled
    if requires_config && config_path.is_none() {
        // Backend commands allow no config (will use local backend defaults)
        // This is fine — create_backend_default() handles auto-detection
    }

    // Get the merged configuration for normal operations
    let mut config = effective_config.get_merged_config();

    // Inject backends from state (state backends take precedence over committed)
    let local_state_for_backends = if uses_backend {
        LocalStateManager::new().ok()
    } else {
        None
    };
    if let Some(ref state_manager) = local_state_for_backends {
        if let Some(ref path) = config_path {
            if let Some(state_backends) = state_manager.get_backends(path) {
                config.backends = Some(state_backends);
            }
        }
    }

    // Handle backend-based commands
    if uses_backend {
        // For GitHook, check if hooks are disabled early
        if matches!(cmd, Commands::GitHook { .. }) && effective_config.should_skip_hooks() {
            log::debug!("Git hooks are disabled via configuration");
            return Ok(());
        }
        // For doctor, run config/git pre-checks before backend-specific checks
        if matches!(cmd, Commands::Doctor) && !json_output {
            run_doctor_pre_checks(&config, &config_path);
        }
        return handle_backend_command(
            cmd,
            &mut config,
            json_output,
            _non_interactive,
            database_name,
            &config_path,
        )
        .await;
    }

    match cmd {
        Commands::Init {
            name,
            force,
            backend,
            from,
        } => {
            let config_path = std::env::current_dir()?.join(".devflow.yml");

            // Resolve the name: if None, derive from current directory
            let resolved_name = match name {
                Some(n) => n,
                None => std::env::current_dir()?
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "default".to_string()),
            };

            let backend_type = backend.as_deref().unwrap_or("local").to_string();
            let is_local = services::factory::BackendType::is_local(&backend_type);
            let is_postgres_template = matches!(
                backend_type.as_str(),
                "postgres_template" | "postgres" | "postgresql"
            );

            if config_path.exists() {
                // --- Subsequent init: add a new backend to state (don't modify .devflow.yml) ---
                let config = Config::from_file(&config_path)?;

                // Build new named backend config
                let named_cfg = crate::config::NamedBackendConfig {
                    name: resolved_name.clone(),
                    backend_type: backend_type.clone(),
                    service_type: crate::config::default_service_type(),
                    auto_branch: crate::config::default_auto_branch(),
                    default: false,
                    local: if is_local {
                        Some(crate::config::LocalBackendConfig {
                            image: None,
                            data_root: None,
                            storage: None,
                            port_range_start: None,
                            postgres_user: None,
                            postgres_password: None,
                            postgres_db: None,
                        })
                    } else {
                        None
                    },
                    neon: None,
                    dblab: None,
                    xata: None,
                    clickhouse: None,
                    mysql: None,
                    generic: None,
                    plugin: None,
                };

                // Store backend in local state instead of committed config
                let mut state = LocalStateManager::new()?;
                state.add_backend(&config_path, named_cfg.clone(), force)?;
                if !json_output {
                    println!("Added backend '{}' to local state", resolved_name);
                }

                // Create main branch for local backends
                if is_local {
                    // Build a config with the backend injected so the factory can find it
                    let mut config_with_backend = config;
                    if let Some(state_backends) = state.get_backends(&config_path) {
                        config_with_backend.backends = Some(state_backends);
                    }

                    // On Linux, offer ZFS auto-setup before creating the main branch
                    #[cfg(feature = "backend-local")]
                    if cfg!(target_os = "linux") {
                        if let Some(data_root) = attempt_zfs_auto_setup(_non_interactive).await {
                            let mut updated_cfg = named_cfg.clone();
                            if let Some(ref mut local) = updated_cfg.local {
                                local.data_root = Some(data_root);
                            }
                            let _ = state.add_backend(&config_path, updated_cfg.clone(), true);
                            if let Some(state_backends) = state.get_backends(&config_path) {
                                config_with_backend.backends = Some(state_backends);
                            }
                            init_local_backend_main(
                                &config_with_backend,
                                &updated_cfg,
                                from.as_deref(),
                            )
                            .await;
                        } else {
                            init_local_backend_main(
                                &config_with_backend,
                                &named_cfg,
                                from.as_deref(),
                            )
                            .await;
                        }
                    } else {
                        init_local_backend_main(&config_with_backend, &named_cfg, from.as_deref())
                            .await;
                    }
                    #[cfg(not(feature = "backend-local"))]
                    {
                        init_local_backend_main(&config_with_backend, &named_cfg, from.as_deref())
                            .await;
                    }
                }

                if json_output {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "status": "ok",
                            "action": "add_backend",
                            "name": resolved_name,
                            "backend_type": backend_type,
                            "config_path": config_path.display().to_string(),
                        }))?
                    );
                }
            } else {
                // --- First-time init: create .devflow.yml ---
                let mut config = Config::default();

                // Auto-detect main branch from VCS
                if let Ok(vcs) = vcs::detect_vcs_provider(".") {
                    if let Ok(Some(detected_main)) = vcs.default_branch() {
                        config.git.main_branch = detected_main.clone();
                        println!(
                            "Auto-detected main branch: {} ({})",
                            detected_main,
                            vcs.provider_name()
                        );
                    } else {
                        println!("Could not auto-detect main branch, using default: main");
                    }
                }

                // For postgres_template backend, look for Docker Compose files
                if is_postgres_template {
                    let compose_files = docker::find_docker_compose_files();
                    if !compose_files.is_empty() {
                        println!("Found Docker Compose files: {}", compose_files.join(", "));

                        if let Some(postgres_config) =
                            docker::parse_postgres_config_from_files(&compose_files)?
                        {
                            if docker::prompt_user_for_config_usage(&postgres_config)? {
                                if let Some(host) = postgres_config.host {
                                    config.database.host = host;
                                }
                                if let Some(port) = postgres_config.port {
                                    config.database.port = port;
                                }
                                if let Some(user) = postgres_config.user {
                                    config.database.user = user;
                                }
                                if let Some(password) = postgres_config.password {
                                    config.database.password = Some(password);
                                }
                                if let Some(database) = postgres_config.database {
                                    config.database.template_database = database;
                                }

                                println!("Using PostgreSQL configuration from Docker Compose");
                            }
                        }
                    }
                }

                // Build named backend config
                let named_cfg = crate::config::NamedBackendConfig {
                    name: resolved_name.clone(),
                    backend_type: backend_type.clone(),
                    service_type: crate::config::default_service_type(),
                    auto_branch: crate::config::default_auto_branch(),
                    default: true,
                    local: if is_local {
                        Some(crate::config::LocalBackendConfig {
                            image: None,
                            data_root: None,
                            storage: None,
                            port_range_start: None,
                            postgres_user: None,
                            postgres_password: None,
                            postgres_db: None,
                        })
                    } else {
                        None
                    },
                    neon: None,
                    dblab: None,
                    xata: None,
                    clickhouse: None,
                    mysql: None,
                    generic: None,
                    plugin: None,
                };

                // Don't write backends to committed config — store in state
                config.backends = None;
                config.save_to_file(&config_path)?;
                if !json_output {
                    println!(
                        "Initialized devflow configuration at: {}",
                        config_path.display()
                    );
                }

                // Store backend in local state
                let mut state = LocalStateManager::new()?;
                state.set_backends(&config_path, vec![named_cfg.clone()])?;
                if !json_output {
                    println!("Stored backend '{}' in local state", resolved_name);
                }

                // Inject backends into config so init_local_backend_main can use them
                config.backends = Some(vec![named_cfg.clone()]);

                // Create main branch for local backends
                if is_local {
                    // On Linux, offer ZFS auto-setup before creating the main branch
                    #[cfg(feature = "backend-local")]
                    if cfg!(target_os = "linux") {
                        if let Some(data_root) = attempt_zfs_auto_setup(_non_interactive).await {
                            // Update the named backend config with the ZFS data_root
                            let mut updated_cfg = named_cfg.clone();
                            if let Some(ref mut local) = updated_cfg.local {
                                local.data_root = Some(data_root);
                            }
                            // Update in state and injected config
                            if let Ok(mut state) = LocalStateManager::new() {
                                let _ = state.set_backends(&config_path, vec![updated_cfg.clone()]);
                            }
                            config.backends = Some(vec![updated_cfg.clone()]);
                            init_local_backend_main(&config, &updated_cfg, from.as_deref()).await;
                        } else {
                            init_local_backend_main(&config, &named_cfg, from.as_deref()).await;
                        }
                    } else {
                        init_local_backend_main(&config, &named_cfg, from.as_deref()).await;
                    }
                    #[cfg(not(feature = "backend-local"))]
                    {
                        init_local_backend_main(&config, &named_cfg, from.as_deref()).await;
                    }
                }

                // Suggest adding local config to gitignore
                if json_output {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "status": "ok",
                            "action": "init",
                            "name": resolved_name,
                            "backend_type": backend_type,
                            "config_path": config_path.display().to_string(),
                        }))?
                    );
                } else {
                    let gitignore_path = std::env::current_dir()?.join(".gitignore");
                    if gitignore_path.exists() {
                        let gitignore_content =
                            std::fs::read_to_string(&gitignore_path).unwrap_or_default();
                        if !gitignore_content.contains(".devflow.local.yml") {
                            println!(
                                "\nSuggestion: Add '.devflow.local.yml' to your .gitignore file:"
                            );
                            println!("   echo '.devflow.local.yml' >> .gitignore");
                        }
                    }

                    println!("\nNext steps:");
                    println!(
                        "  devflow install-hooks     Install Git hooks for automatic branching"
                    );
                    println!("  devflow create <branch>   Create a service branch manually");
                    println!("  devflow doctor            Check system health and configuration");
                }
            }
        }
        Commands::SetupZfs { pool_name, size } => {
            if !cfg!(target_os = "linux") {
                anyhow::bail!("setup-zfs is only supported on Linux");
            }

            #[cfg(not(feature = "backend-local"))]
            {
                let _ = (pool_name, size);
                anyhow::bail!("Local backend not compiled. Rebuild with --features backend-local");
            }

            #[cfg(feature = "backend-local")]
            {
                use crate::services::postgres::local::storage::zfs_setup::*;

                let pool = pool_name.unwrap_or_else(|| "devflow".to_string());
                let img_size = size.unwrap_or_else(|| "10G".to_string());

                let config = ZfsPoolSetupConfig {
                    pool_name: pool.clone(),
                    image_path: PathBuf::from(format!("/var/lib/devflow/{}.img", pool)),
                    image_size: img_size.clone(),
                    mountpoint: PathBuf::from("/var/lib/devflow/data"),
                };

                println!("Creating file-backed ZFS pool:");
                println!("  Pool name:  {}", config.pool_name);
                println!(
                    "  Image:      {} (sparse, {})",
                    config.image_path.display(),
                    img_size
                );
                println!("  Mountpoint: {}", config.mountpoint.display());
                println!();

                let data_root = create_file_backed_pool(&config).await?;
                println!();
                println!("ZFS pool '{}' created successfully", pool);
                println!("Data root: {}", data_root);
                println!();
                println!("Run 'devflow init' to set up a project using this pool.");
            }
        }
        Commands::Hook { action } => {
            handle_hook_command(action, &config, json_output).await?;
        }
        Commands::Plugin { action } => {
            handle_plugin_command(action, &config, json_output).await?;
        }
        Commands::Config { verbose } => {
            if json_output {
                if verbose {
                    let merged_config = effective_config.get_merged_config();
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "disabled": effective_config.is_disabled(),
                            "skip_hooks": effective_config.should_skip_hooks(),
                            "current_branch_disabled": effective_config.is_current_branch_disabled(),
                            "has_local_config": effective_config.local_config.is_some(),
                            "config": serde_json::to_value(&merged_config)?,
                        }))?
                    );
                } else {
                    println!("{}", serde_json::to_string_pretty(&config)?);
                }
            } else if verbose {
                show_effective_config(&effective_config)?;
            } else {
                println!("Current configuration:");
                println!("{}", serde_yaml_ng::to_string(&config)?);
            }
        }
        Commands::ShellInit { shell } => {
            let detected_shell = match shell {
                Some(s) => s,
                None => detect_shell_from_env()?,
            };
            print_shell_init(&detected_shell)?;
        }
        Commands::InstallHooks => {
            let vcs = vcs::detect_vcs_provider(".")?;
            vcs.install_hooks()?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "action": "install_hooks",
                        "vcs_provider": vcs.provider_name(),
                    }))?
                );
            } else {
                println!("Installed {} hooks", vcs.provider_name());
            }
        }
        Commands::UninstallHooks => {
            let vcs = vcs::detect_vcs_provider(".")?;
            vcs.uninstall_hooks()?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "action": "uninstall_hooks",
                        "vcs_provider": vcs.provider_name(),
                    }))?
                );
            } else {
                println!("Uninstalled {} hooks", vcs.provider_name());
            }
        }
        Commands::Commit {
            message,
            ai,
            edit,
            dry_run,
        } => {
            handle_commit_command(message, ai, edit, dry_run, json_output).await?;
        }
        _ => unreachable!(),
    }

    Ok(())
}

/// Check if ZFS auto-setup should be offered during init (Linux only).
/// Returns `Some(data_root)` if a pool was created or already exists,
/// so the caller can set it on the `LocalBackendConfig`.
#[cfg(feature = "backend-local")]
async fn attempt_zfs_auto_setup(non_interactive: bool) -> Option<String> {
    use crate::services::postgres::local::storage::zfs_setup::*;

    // Use a placeholder path — the actual projects_root hasn't been established yet
    let placeholder = std::path::PathBuf::from("/var/lib/devflow/data/projects");
    let status = check_zfs_setup_status(&placeholder).await;

    match status {
        ZfsSetupStatus::NotSupported => None,
        ZfsSetupStatus::ToolsNotInstalled => {
            println!();
            println!("Tip: Install ZFS for near-instant Copy-on-Write database branching:");
            println!("  sudo apt install zfsutils-linux");
            None
        }
        ZfsSetupStatus::AlreadyAvailable { root_dataset } => {
            println!();
            println!(
                "ZFS dataset '{}' detected - will use ZFS for Copy-on-Write storage.",
                root_dataset
            );
            None
        }
        ZfsSetupStatus::DevflowPoolExists { mountpoint } => {
            println!();
            println!(
                "ZFS pool 'devflow' already exists (mountpoint: {}).",
                mountpoint
            );
            Some(mountpoint)
        }
        ZfsSetupStatus::ToolsAvailableNoPool => {
            if non_interactive {
                println!();
                println!(
                    "ZFS tools detected but no pool found. Run 'devflow setup-zfs' to create one."
                );
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

async fn init_local_backend_main(
    config: &Config,
    named_cfg: &crate::config::NamedBackendConfig,
    from: Option<&str>,
) {
    match services::factory::create_backend_from_named_config(config, named_cfg).await {
        Ok(be) => {
            match be.create_branch("main", None).await {
                Ok(info) => {
                    println!("Created main branch");
                    if let Ok(conn) = be.get_connection_info("main").await {
                        if let Some(ref uri) = conn.connection_string {
                            println!("  Connection: {}", uri);
                        }
                    }
                    if let Some(state) = &info.state {
                        println!("  State: {}", state);
                    }

                    // Seed if --from specified
                    if let Some(source) = from {
                        println!("Seeding main branch from: {}", source);
                        match be.seed_from_source("main", source).await {
                            Ok(_) => println!("Seeding completed successfully"),
                            Err(e) => eprintln!("Warning: seeding failed: {}", e),
                        }
                    }
                }
                Err(e) => {
                    eprintln!(
                        "Warning: could not create main branch for '{}': {}",
                        named_cfg.name, e
                    );
                    eprintln!("  You can create it later with: devflow create main");
                }
            }
        }
        Err(e) => {
            eprintln!(
                "Warning: could not initialize backend '{}': {}",
                named_cfg.name, e
            );
            eprintln!("  You can create the main branch later with: devflow create main");
        }
    }
}

#[allow(dead_code)] // Kept as fallback for tree-only service display
fn print_branch_tree(branches: &[services::BranchInfo], indent: &str) {
    use std::collections::HashMap;

    if branches.is_empty() {
        println!("{}(none)", indent);
        return;
    }

    // Collect the set of known branch names for parent lookups
    let known: std::collections::HashSet<&str> = branches.iter().map(|b| b.name.as_str()).collect();

    // Group children by parent name
    let mut children: HashMap<&str, Vec<&services::BranchInfo>> = HashMap::new();
    let mut roots: Vec<&services::BranchInfo> = Vec::new();

    for b in branches {
        match b.parent_branch.as_deref() {
            Some(parent) if known.contains(parent) => {
                children.entry(parent).or_default().push(b);
            }
            _ => roots.push(b),
        }
    }

    fn print_node(
        branch: &services::BranchInfo,
        prefix: &str,
        connector: &str,
        children: &std::collections::HashMap<&str, Vec<&services::BranchInfo>>,
    ) {
        let state_str = branch.state.as_deref().unwrap_or("unknown");
        println!("{}{} [{}]", connector, branch.name, state_str);

        if let Some(kids) = children.get(branch.name.as_str()) {
            let count = kids.len();
            for (i, child) in kids.iter().enumerate() {
                let is_last = i == count - 1;
                let child_connector = if is_last {
                    format!("{}└─ ", prefix)
                } else {
                    format!("{}├─ ", prefix)
                };
                let child_prefix = if is_last {
                    format!("{}   ", prefix)
                } else {
                    format!("{}│  ", prefix)
                };
                print_node(child, &child_prefix, &child_connector, children);
            }
        }
    }

    for root in &roots {
        print_node(root, indent, indent, &children);
    }
}

/// Print an enriched branch list showing git branches, worktree paths, and service status.
///
/// Unifies information from the VCS provider and the service backend into a single view.
fn print_enriched_branch_list(service_branches: &[services::BranchInfo], config: &Config) {
    // Gather VCS + worktree info
    let vcs_provider = vcs::detect_vcs_provider(".").ok();
    let git_branches: Vec<crate::vcs::BranchInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_branches().ok())
        .unwrap_or_default();
    let worktrees: Vec<crate::vcs::WorktreeInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_worktrees().ok())
        .unwrap_or_default();
    let current_git = vcs_provider
        .as_ref()
        .and_then(|r| r.current_branch().ok().flatten());

    // Build a set of service branch names for quick lookup
    let service_names: std::collections::HashSet<&str> =
        service_branches.iter().map(|b| b.name.as_str()).collect();

    // Build a worktree lookup: branch name -> path
    let wt_lookup: std::collections::HashMap<String, PathBuf> = worktrees
        .iter()
        .filter_map(|wt| wt.branch.as_ref().map(|b| (b.clone(), wt.path.clone())))
        .collect();

    // Collect all branch names (union of git branches + service branches)
    let mut all_names: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Add git branches first (they're the primary)
    for gb in &git_branches {
        if seen.insert(gb.name.clone()) {
            all_names.push(gb.name.clone());
        }
    }
    // Add any service branches that don't correspond to a git branch
    for sb in service_branches {
        let normalized = &sb.name;
        if seen.insert(normalized.clone()) {
            all_names.push(normalized.clone());
        }
    }

    if all_names.is_empty() {
        println!("  (none)");
        return;
    }

    for name in &all_names {
        let is_current = current_git.as_deref() == Some(name.as_str());
        let marker = if is_current { "* " } else { "  " };

        // Git status
        let normalized = config.get_normalized_branch_name(name);
        let has_service =
            service_names.contains(normalized.as_str()) || service_names.contains(name.as_str());

        // Service state
        let service_state = service_branches
            .iter()
            .find(|b| b.name == normalized || b.name == *name)
            .and_then(|b| b.state.as_deref());

        // Worktree path
        let wt_path = wt_lookup.get(name);

        // Format the line
        let mut parts = Vec::new();

        if let Some(state) = service_state {
            parts.push(format!("service: {}", state));
        } else if has_service {
            parts.push("service: ok".to_string());
        }

        if let Some(path) = wt_path {
            parts.push(format!("worktree: {}", path.display()));
        }

        if parts.is_empty() {
            println!("{}{}", marker, name);
        } else {
            println!("{}{}  [{}]", marker, name, parts.join(", "));
        }
    }
}

/// Build enriched JSON for the list command, merging git + worktree + service info.
fn enrich_branch_list_json(
    service_branches: &[services::BranchInfo],
    config: &Config,
) -> serde_json::Value {
    let vcs_provider = vcs::detect_vcs_provider(".").ok();
    let git_branches: Vec<crate::vcs::BranchInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_branches().ok())
        .unwrap_or_default();
    let worktrees: Vec<crate::vcs::WorktreeInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_worktrees().ok())
        .unwrap_or_default();

    let wt_lookup: std::collections::HashMap<String, PathBuf> = worktrees
        .iter()
        .filter_map(|wt| wt.branch.as_ref().map(|b| (b.clone(), wt.path.clone())))
        .collect();

    let service_map: std::collections::HashMap<&str, &services::BranchInfo> = service_branches
        .iter()
        .map(|b| (b.name.as_str(), b))
        .collect();

    let mut entries = Vec::new();

    // Collect all branch names
    let mut seen = std::collections::HashSet::new();
    let mut all_names: Vec<String> = Vec::new();
    for gb in &git_branches {
        if seen.insert(gb.name.clone()) {
            all_names.push(gb.name.clone());
        }
    }
    for sb in service_branches {
        if seen.insert(sb.name.clone()) {
            all_names.push(sb.name.clone());
        }
    }

    for name in &all_names {
        let gb = git_branches.iter().find(|b| b.name == *name);
        let normalized = config.get_normalized_branch_name(name);
        let sb = service_map
            .get(normalized.as_str())
            .or_else(|| service_map.get(name.as_str()));
        let wt = wt_lookup.get(name);

        let mut entry = serde_json::json!({
            "name": name,
            "is_current": gb.map(|b| b.is_current).unwrap_or(false),
            "is_default": gb.map(|b| b.is_default).unwrap_or(false),
        });

        if let Some(svc) = sb {
            entry["service"] = serde_json::json!({
                "database": svc.database_name,
                "state": svc.state,
                "parent": svc.parent_branch,
            });
        }

        if let Some(path) = wt {
            entry["worktree_path"] = serde_json::Value::String(path.display().to_string());
        }

        entries.push(entry);
    }

    serde_json::Value::Array(entries)
}

/// Detect the current shell from the `$SHELL` environment variable.
fn detect_shell_from_env() -> Result<String> {
    let shell_path = std::env::var("SHELL")
        .context("Cannot auto-detect shell: $SHELL is not set. Please specify a shell: devflow shell-init <bash|zsh|fish>")?;
    let shell_name = std::path::Path::new(&shell_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or(shell_path.clone());
    match shell_name.as_str() {
        "bash" | "zsh" | "fish" => Ok(shell_name),
        other => anyhow::bail!(
            "Unsupported shell '{}' (from $SHELL={}). Supported shells: bash, zsh, fish",
            other,
            shell_path
        ),
    }
}

/// Print shell integration script for the given shell type.
///
/// Users should add `eval "$(devflow shell-init bash)"` (or zsh/fish) to their
/// shell profile. This defines a `devflow` wrapper function that:
/// 1. Runs `devflow` normally, preserving stderr
/// 2. Parses `DEVFLOW_CD=<path>` output from `switch` commands
/// 3. Automatically `cd`s into the target worktree directory
fn print_shell_init(shell: &str) -> Result<()> {
    let script = match shell {
        "bash" => {
            r#"
# devflow shell integration (bash)
# Wrapper function that adds auto-cd into worktree directories after switch
devflow() {
    local output
    output="$(command devflow "$@")"
    local exit_code=$?

    # Print all output lines, skipping DEVFLOW_CD directives
    while IFS= read -r line; do
        case "$line" in
            DEVFLOW_CD=*)
                local target="${line#DEVFLOW_CD=}"
                if [ -d "$target" ]; then
                    cd "$target" || return 1
                    echo "Changed directory to: $target"
                fi
                ;;
            *)
                echo "$line"
                ;;
        esac
    done <<< "$output"

    return $exit_code
}
"#
        }
        "zsh" => {
            r#"
# devflow shell integration (zsh)
# Wrapper function that adds auto-cd into worktree directories after switch
devflow() {
    local output
    output="$(command devflow "$@")"
    local exit_code=$?

    # Print all output lines, skipping DEVFLOW_CD directives
    while IFS= read -r line; do
        case "$line" in
            DEVFLOW_CD=*)
                local target="${line#DEVFLOW_CD=}"
                if [ -d "$target" ]; then
                    cd "$target" || return 1
                    echo "Changed directory to: $target"
                fi
                ;;
            *)
                echo "$line"
                ;;
        esac
    done <<< "$output"

    return $exit_code
}
"#
        }
        "fish" => {
            r#"
# devflow shell integration (fish)
# Wrapper function that adds auto-cd into worktree directories after switch
function devflow --wraps devflow --description "devflow with auto-cd"
    set -l output (command devflow $argv)
    set -l exit_code $status

    for line in $output
        if string match -q 'DEVFLOW_CD=*' -- $line
            set -l target (string replace 'DEVFLOW_CD=' '' -- $line)
            if test -d "$target"
                cd "$target"
                echo "Changed directory to: $target"
            end
        else
            echo $line
        end
    end

    return $exit_code
end
"#
        }
        _ => {
            anyhow::bail!(
                "Unsupported shell '{}'. Supported shells: bash, zsh, fish",
                shell
            );
        }
    };

    print!("{}", script.trim_start());
    Ok(())
}

/// Build a `HookContext` from the current config and branch name.
///
/// Populates legacy template variables (branch_name, db_name, etc.) so that
/// both new MiniJinja templates and old `{branch_name}` style work.
async fn build_hook_context(config: &Config, branch_name: &str) -> HookContext {
    let db_name = config.get_database_name(branch_name);
    let repo = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_default();

    // Detect worktree path if we're inside a VCS worktree
    let worktree_path = vcs::detect_vcs_provider(".").ok().and_then(|vcs_repo| {
        if vcs_repo.is_worktree() {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().to_string())
        } else {
            // Not in a worktree — check if there's a worktree for this branch elsewhere
            vcs_repo
                .worktree_path(branch_name)
                .ok()
                .flatten()
                .map(|p| p.to_string_lossy().to_string())
        }
    });

    // Build service map from all configured backends
    let mut service = std::collections::HashMap::new();

    if let Ok(conn_infos) = services::factory::get_all_connection_info(config, branch_name).await {
        for (name, info) in conn_infos {
            let url = info.connection_string.clone().unwrap_or_else(|| {
                // No connection string provided by the backend — build a generic
                // host:port reference so templates have *something* useful.
                format!("{}:{}", info.host, info.port)
            });
            service.insert(
                name,
                ServiceContext {
                    host: info.host,
                    port: info.port,
                    database: info.database,
                    user: info.user,
                    password: info.password,
                    url,
                },
            );
        }
    }

    // Fallback: if no backends populated the service map, use legacy config.database
    // as a service named "db" for backward compatibility
    if service.is_empty() {
        let url = format!(
            "postgresql://{}{}@{}:{}/{}",
            config.database.user,
            config
                .database
                .password
                .as_ref()
                .map(|p| format!(":{}", p))
                .unwrap_or_default(),
            config.database.host,
            config.database.port,
            db_name,
        );
        service.insert(
            "db".to_string(),
            ServiceContext {
                host: config.database.host.clone(),
                port: config.database.port,
                database: db_name.clone(),
                user: config.database.user.clone(),
                password: config.database.password.clone(),
                url,
            },
        );
    }

    HookContext {
        branch: branch_name.to_string(),
        repo,
        worktree_path,
        default_branch: config.git.main_branch.clone(),
        commit: None,
        target: None,
        base: None,
        service,
    }
}

/// Run hooks for the given phase.
async fn run_hooks(
    config: &Config,
    branch_name: &str,
    phase: HookPhase,
) -> Result<()> {
    if let Some(ref hooks_config) = config.hooks {
        let working_dir =
            std::env::current_dir().map_err(|e| anyhow::anyhow!("Failed to get cwd: {}", e))?;
        let project_key = working_dir
            .canonicalize()
            .ok()
            .map(|p| p.to_string_lossy().to_string());
        let engine = HookEngine::new(hooks_config.clone(), working_dir, project_key);
        engine
            .run_phase_verbose(&phase, &build_hook_context(config, branch_name).await)
            .await?;
    }
    Ok(())
}

/// Handle `devflow hook` subcommands.
async fn handle_hook_command(
    action: HookCommands,
    config: &Config,
    json_output: bool,
) -> Result<()> {
    match action {
        HookCommands::Show { phase } => {
            handle_hook_show(config, phase.as_deref(), json_output)?;
        }
        HookCommands::Run {
            phase,
            name,
            branch,
        } => {
            handle_hook_run(
                config,
                &phase,
                name.as_deref(),
                branch.as_deref(),
                json_output,
            )
            .await?;
        }
        HookCommands::Approvals { action } => {
            handle_hook_approvals(action, json_output)?;
        }
    }
    Ok(())
}

/// `devflow hook show [phase]` — display configured hooks.
fn handle_hook_show(config: &Config, phase_filter: Option<&str>, json_output: bool) -> Result<()> {
    let hooks = match &config.hooks {
        Some(h) if !h.is_empty() => h,
        _ => {
            println!("No hooks configured.");
            println!("  Add a 'hooks' section to .devflow.yml to configure lifecycle hooks.");
            return Ok(());
        }
    };

    // Optionally filter to a single phase
    let phase_filter_parsed: Option<HookPhase> = match phase_filter {
        Some(s) => {
            let parsed: HookPhase = s.parse().unwrap();
            if let HookPhase::Custom(ref name) = parsed {
                eprintln!(
                    "Warning: '{}' is not a built-in phase. Built-in phases: pre-switch, post-create, \
                     post-start, post-switch, pre-remove, post-remove, pre-commit, pre-merge, \
                     post-merge, pre-service-create, post-service-create, pre-service-delete, \
                     post-service-delete, post-service-switch",
                    name
                );
            }
            Some(parsed)
        }
        None => None,
    };

    if json_output {
        let filtered: std::collections::HashMap<_, _> = hooks
            .iter()
            .filter(|(phase, _)| phase_filter_parsed.as_ref().is_none_or(|pf| *phase == pf))
            .collect();
        println!("{}", serde_json::to_string_pretty(&filtered)?);
        return Ok(());
    }

    let mut shown = false;
    for (phase, named_hooks) in hooks {
        if let Some(ref pf) = phase_filter_parsed {
            if phase != pf {
                continue;
            }
        }

        shown = true;
        println!(
            "{} ({}):",
            phase,
            if phase.is_blocking() {
                "blocking"
            } else {
                "background"
            }
        );

        for (name, entry) in named_hooks {
            match entry {
                HookEntry::Simple(cmd) => {
                    println!("  {}: {}", name, cmd);
                }
                HookEntry::Extended(ext) => {
                    println!("  {}:", name);
                    println!("    command: {}", ext.command);
                    if let Some(ref wd) = ext.working_dir {
                        println!("    working_dir: {}", wd);
                    }
                    if let Some(ref cond) = ext.condition {
                        println!("    condition: {}", cond);
                    }
                    if let Some(coe) = ext.continue_on_error {
                        println!("    continue_on_error: {}", coe);
                    }
                    if ext.background {
                        println!("    background: true");
                    }
                    if let Some(ref env) = ext.environment {
                        println!("    environment:");
                        for (k, v) in env {
                            println!("      {}: {}", k, v);
                        }
                    }
                }
            }
        }
    }

    if !shown {
        if let Some(pf) = phase_filter {
            println!("No hooks configured for phase '{}'.", pf);
        }
    }

    Ok(())
}

/// `devflow hook run <phase> [name]` — manually execute hooks.
async fn handle_hook_run(
    config: &Config,
    phase_str: &str,
    name_filter: Option<&str>,
    branch_override: Option<&str>,
    json_output: bool,
) -> Result<()> {
    let hooks_config = match &config.hooks {
        Some(h) if !h.is_empty() => h.clone(),
        _ => {
            anyhow::bail!("No hooks configured. Add a 'hooks' section to .devflow.yml first.");
        }
    };

    let phase: HookPhase = phase_str.parse().unwrap();

    if let HookPhase::Custom(ref name) = phase {
        eprintln!(
            "Warning: '{}' is not a built-in phase. Built-in phases: pre-switch, post-create, \
             post-start, post-switch, pre-remove, post-remove, pre-commit, pre-merge, \
             post-merge, pre-service-create, post-service-create, pre-service-delete, \
             post-service-delete, post-service-switch",
            name
        );
    }

    // Determine branch name: use override, or try current git branch, or fallback
    let branch_name = if let Some(b) = branch_override {
        b.to_string()
    } else {
        match vcs::detect_vcs_provider(".") {
            Ok(vcs_repo) => vcs_repo
                .current_branch()
                .ok()
                .flatten()
                .unwrap_or_else(|| "unknown".to_string()),
            Err(_) => "unknown".to_string(),
        }
    };

    let context = build_hook_context(config, &branch_name).await;

    // If a specific hook name is given, build a filtered config
    let effective_config = if let Some(name) = name_filter {
        let phase_hooks = hooks_config
            .get(&phase)
            .ok_or_else(|| anyhow::anyhow!("No hooks configured for phase '{}'", phase))?;

        let entry = phase_hooks.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "Hook '{}' not found in phase '{}'. Available: {}",
                name,
                phase,
                phase_hooks
                    .keys()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

        let mut filtered = IndexMap::new();
        let mut phase_map = IndexMap::new();
        phase_map.insert(name.to_string(), entry.clone());
        filtered.insert(phase.clone(), phase_map);
        filtered
    } else {
        hooks_config
    };

    let working_dir =
        std::env::current_dir().map_err(|e| anyhow::anyhow!("Failed to get cwd: {}", e))?;

    // Manual runs don't require approval
    let engine = HookEngine::new_no_approval(effective_config, working_dir);
    let result = engine.run_phase_verbose(&phase, &context).await?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "phase": phase.to_string(),
                "succeeded": result.succeeded,
                "failed": result.failed,
                "skipped": result.skipped,
                "background": result.background,
            }))?
        );
    } else if result.succeeded == 0 && result.background == 0 && result.skipped == 0 {
        println!("No hooks ran for phase '{}'.", phase);
    }

    Ok(())
}

/// `devflow hook approvals` — manage hook approval store.
fn handle_hook_approvals(action: ApprovalCommands, json_output: bool) -> Result<()> {
    let project_key = std::env::current_dir()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    match action {
        ApprovalCommands::List => {
            let store = ApprovalStore::load().unwrap_or_default();
            let approved = store.list_approved(&project_key);

            if json_output {
                let items: Vec<serde_json::Value> = approved
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "command": r.command,
                            "approved_at": r.approved_at.to_rfc3339(),
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "project": project_key,
                        "approvals": items,
                    }))?
                );
            } else if approved.is_empty() {
                println!("No approved hooks for this project.");
            } else {
                println!("Approved hooks ({}):", approved.len());
                for record in approved {
                    println!(
                        "  - {} (approved {})",
                        record.command,
                        record.approved_at.format("%Y-%m-%d %H:%M")
                    );
                }
            }
        }
        ApprovalCommands::Add { command } => {
            let mut store = ApprovalStore::load().unwrap_or_default();
            store.approve(&project_key, &command)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "action": "approve",
                        "command": command,
                    }))?
                );
            } else {
                println!("Approved hook command: {}", command);
            }
        }
        ApprovalCommands::Clear => {
            let mut store = ApprovalStore::load().unwrap_or_default();
            store.clear_project(&project_key)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "action": "clear_approvals",
                        "project": project_key,
                    }))?
                );
            } else {
                println!("Cleared all hook approvals for this project.");
            }
        }
    }

    Ok(())
}

/// Handle `devflow plugin` subcommands.
async fn handle_plugin_command(
    action: PluginCommands,
    config: &Config,
    json_output: bool,
) -> Result<()> {
    match action {
        PluginCommands::List => {
            let backends = config.resolve_backends();
            let plugins: Vec<_> = backends
                .iter()
                .filter(|b| b.service_type == "plugin")
                .collect();

            if plugins.is_empty() {
                if json_output {
                    println!("[]");
                } else {
                    println!("No plugin backends configured.");
                    println!(
                        "Add a backend with service_type: plugin in your .devflow.yml to get started."
                    );
                }
                return Ok(());
            }

            if json_output {
                let items: Vec<serde_json::Value> = plugins
                    .iter()
                    .map(|p| {
                        let plugin_cfg = p.plugin.as_ref();
                        let executable = plugin_cfg
                            .and_then(|c| {
                                c.path.clone().or_else(|| {
                                    c.name.as_ref().map(|n| format!("devflow-plugin-{}", n))
                                })
                            })
                            .unwrap_or_else(|| "(not configured)".to_string());
                        serde_json::json!({
                            "name": p.name,
                            "executable": executable,
                            "auto_branch": p.auto_branch,
                            "timeout": plugin_cfg.map(|c| c.timeout).unwrap_or(30),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else {
                println!("Plugin backends ({}):", plugins.len());
                for p in &plugins {
                    let plugin_cfg = p.plugin.as_ref();
                    let executable = plugin_cfg
                        .and_then(|c| {
                            c.path.clone().or_else(|| {
                                c.name.as_ref().map(|n| format!("devflow-plugin-{}", n))
                            })
                        })
                        .unwrap_or_else(|| "(not configured)".to_string());
                    println!("  {} -> {}", p.name, executable);
                    if let Some(cfg) = plugin_cfg {
                        println!("    timeout: {}s", cfg.timeout);
                    }
                    println!("    auto_branch: {}", p.auto_branch);
                }
            }
        }
        PluginCommands::Check { name } => {
            let backends = config.resolve_backends();
            let named = backends.iter().find(|b| b.name == name).ok_or_else(|| {
                anyhow::anyhow!(
                    "Backend '{}' not found in configuration. Available backends: {}",
                    name,
                    backends
                        .iter()
                        .map(|b| b.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;

            if named.service_type != "plugin" {
                anyhow::bail!(
                    "Backend '{}' is not a plugin (service_type: '{}')",
                    name,
                    named.service_type
                );
            }

            let plugin_cfg = named.plugin.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Backend '{}' has type 'plugin' but no plugin config section",
                    name
                )
            })?;

            // Try to create the backend and invoke backend_name
            match crate::services::plugin::PluginBackend::new(&name, plugin_cfg) {
                Ok(backend) => {
                    // Try test_connection as a health check
                    match backend.test_connection().await {
                        Ok(()) => {
                            if json_output {
                                println!(
                                    "{}",
                                    serde_json::to_string_pretty(&serde_json::json!({
                                        "status": "ok",
                                        "name": name,
                                        "reachable": true,
                                    }))?
                                );
                            } else {
                                println!("Plugin '{}': OK (reachable and responding)", name);
                            }
                        }
                        Err(e) => {
                            if json_output {
                                println!(
                                    "{}",
                                    serde_json::to_string_pretty(&serde_json::json!({
                                        "status": "error",
                                        "name": name,
                                        "reachable": false,
                                        "error": e.to_string(),
                                    }))?
                                );
                            } else {
                                println!("Plugin '{}': FAIL ({})", name, e);
                            }
                        }
                    }
                }
                Err(e) => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "status": "error",
                                "name": name,
                                "reachable": false,
                                "error": e.to_string(),
                            }))?
                        );
                    } else {
                        println!("Plugin '{}': FAIL (could not initialize: {})", name, e);
                    }
                }
            }
        }
        PluginCommands::Init { name, lang } => {
            let script = match lang.as_str() {
                "bash" | "sh" => generate_plugin_skeleton_bash(&name),
                "python" | "py" => generate_plugin_skeleton_python(&name),
                other => {
                    anyhow::bail!(
                        "Unsupported plugin language '{}'. Supported: bash, python",
                        other
                    );
                }
            };

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "name": name,
                        "lang": lang,
                        "script": script,
                    }))?
                );
            } else {
                println!("{}", script);
            }
        }
    }

    Ok(())
}

/// Generate a skeleton bash plugin script.
fn generate_plugin_skeleton_bash(name: &str) -> String {
    format!(
        r#"#!/usr/bin/env bash
# devflow plugin: {name}
#
# This plugin is invoked by devflow with a JSON request on stdin.
# It should write a JSON response to stdout.
#
# Install: chmod +x this file, then reference in .devflow.yml:
#   backends:
#     - name: {name}
#       service_type: plugin
#       plugin:
#         path: ./plugins/devflow-plugin-{name}
#         config:
#           key: value
#
set -euo pipefail

# Read the full JSON request from stdin
REQUEST=$(cat)

METHOD=$(echo "$REQUEST" | jq -r '.method')
PARAMS=$(echo "$REQUEST" | jq -c '.params // {{}}'  )
CONFIG=$(echo "$REQUEST" | jq -c '.config // {{}}'  )
SERVICE=$(echo "$REQUEST" | jq -r '.service_name')

ok()    {{ echo "{{\\"ok\\": true,  \\"result\\": $1}}"; }}
error() {{ echo "{{\\"ok\\": false, \\"error\\": \\"$1\\"}}"; }}

case "$METHOD" in
  backend_name)
    ok "\"{name}\""
    ;;
  test_connection)
    ok "null"
    ;;
  create_branch)
    BRANCH=$(echo "$PARAMS" | jq -r '.branch_name')
    # TODO: implement branch creation for {name}
    ok "{{\\"name\\": \\"$BRANCH\\", \\"created_at\\": null, \\"parent_branch\\": null, \\"database_name\\": \\"$BRANCH\\"}}"
    ;;
  delete_branch)
    BRANCH=$(echo "$PARAMS" | jq -r '.branch_name')
    # TODO: implement branch deletion for {name}
    ok "null"
    ;;
  list_branches)
    # TODO: implement branch listing for {name}
    ok "[]"
    ;;
  branch_exists)
    BRANCH=$(echo "$PARAMS" | jq -r '.branch_name')
    # TODO: implement branch existence check
    ok "false"
    ;;
  switch_to_branch)
    BRANCH=$(echo "$PARAMS" | jq -r '.branch_name')
    ok "{{\\"name\\": \\"$BRANCH\\", \\"created_at\\": null, \\"parent_branch\\": null, \\"database_name\\": \\"$BRANCH\\"}}"
    ;;
  get_connection_info)
    BRANCH=$(echo "$PARAMS" | jq -r '.branch_name')
    ok "{{\\"host\\": \\"localhost\\", \\"port\\": 6379, \\"database\\": \\"$BRANCH\\", \\"user\\": \\"default\\", \\"password\\": null, \\"connection_string\\": null}}"
    ;;
  doctor)
    ok "{{\\"checks\\": [{{  \\"name\\": \\"{name}\\", \\"available\\": true, \\"detail\\": \\"Plugin is running\\"}}]}}"
    ;;
  *)
    error "Unsupported method: $METHOD"
    ;;
esac
"#
    )
}

/// Generate a skeleton Python plugin script.
fn generate_plugin_skeleton_python(name: &str) -> String {
    format!(
        r#"#!/usr/bin/env python3
"""devflow plugin: {name}

This plugin is invoked by devflow with a JSON request on stdin.
It should write a JSON response to stdout.

Install: chmod +x this file, then reference in .devflow.yml:
  backends:
    - name: {name}
      service_type: plugin
      plugin:
        path: ./plugins/devflow-plugin-{name}
        config:
          key: value
"""
import json
import sys
from datetime import datetime, timezone


def ok(result=None):
    print(json.dumps({{"ok": True, "result": result}}))

def error(msg: str):
    print(json.dumps({{"ok": False, "error": msg}}))

def main():
    request = json.loads(sys.stdin.read())
    method = request.get("method", "")
    params = request.get("params", {{}})
    config = request.get("config", {{}})
    service_name = request.get("service_name", "")

    if method == "backend_name":
        ok("{name}")
    elif method == "test_connection":
        ok(None)
    elif method == "create_branch":
        branch = params["branch_name"]
        # TODO: implement branch creation for {name}
        ok({{"name": branch, "created_at": None, "parent_branch": None, "database_name": branch}})
    elif method == "delete_branch":
        branch = params["branch_name"]
        # TODO: implement branch deletion for {name}
        ok(None)
    elif method == "list_branches":
        # TODO: implement branch listing for {name}
        ok([])
    elif method == "branch_exists":
        branch = params["branch_name"]
        # TODO: implement branch existence check
        ok(False)
    elif method == "switch_to_branch":
        branch = params["branch_name"]
        ok({{"name": branch, "created_at": None, "parent_branch": None, "database_name": branch}})
    elif method == "get_connection_info":
        branch = params["branch_name"]
        ok({{
            "host": "localhost",
            "port": 6379,
            "database": branch,
            "user": "default",
            "password": None,
            "connection_string": None,
        }})
    elif method == "doctor":
        ok({{"checks": [{{"name": "{name}", "available": True, "detail": "Plugin is running"}}]}})
    elif method == "cleanup_old_branches":
        ok([])
    elif method == "destroy_project":
        ok([])
    else:
        error(f"Unsupported method: {{method}}")

if __name__ == "__main__":
    main()
"#
    )
}

async fn handle_backend_command(
    cmd: Commands,
    config: &mut Config,
    json_output: bool,
    non_interactive: bool,
    database_name: Option<&str>,
    config_path: &Option<std::path::PathBuf>,
) -> Result<()> {
    // Aggregation commands (List, Status, Doctor) show all backends when no --database given
    let is_aggregation = matches!(cmd, Commands::List | Commands::Status | Commands::Doctor);
    // Orchestratable mutation commands: Create and Delete operate on all auto_branch backends
    let is_orchestratable_mutation =
        matches!(cmd, Commands::Create { .. } | Commands::Delete { .. });
    let has_multiple_backends = config.resolve_backends().len() > 1;

    if is_aggregation && database_name.is_none() && has_multiple_backends {
        return handle_multi_backend_command(cmd, config, json_output).await;
    }

    // For Create/Delete: if there are multiple backends and no --database flag,
    // use orchestration to operate on all auto_branch backends atomically.
    if is_orchestratable_mutation && database_name.is_none() && has_multiple_backends {
        return handle_orchestrated_mutation(cmd, config, json_output).await;
    }

    let named = services::factory::resolve_backend(config, database_name).await?;
    let backend = named.backend;
    let resolved_name = named.name;

    // For non-orchestratable mutation commands with multiple backends and no --database, print a note
    if !is_aggregation
        && !is_orchestratable_mutation
        && database_name.is_none()
        && has_multiple_backends
    {
        eprintln!(
            "note: using default database '{}'. Use --database to target a specific one.",
            resolved_name
        );
    }

    match cmd {
        Commands::Create { branch_name, from } => {
            // Single-backend path (explicit --database or single backend)
            let info = backend.create_branch(&branch_name, from.as_deref()).await?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&info)?);
            } else {
                println!("Created service branch: {}", info.name);
                if let Some(state) = &info.state {
                    println!("  State: {}", state);
                }
                if let Some(parent) = &info.parent_branch {
                    println!("  Parent: {}", parent);
                }
                // Show connection info
                if let Ok(conn) = backend.get_connection_info(&branch_name).await {
                    if let Some(ref uri) = conn.connection_string {
                        println!("  Connection: {}", uri);
                    }
                }
            }

            // Execute hooks
            run_hooks(config, &branch_name, HookPhase::PostServiceCreate).await?;
        }
        Commands::Delete { branch_name } => {
            // Single-backend path (explicit --database or single backend)
            backend.delete_branch(&branch_name).await?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "status": "ok",
                        "deleted": branch_name
                    }))?
                );
            } else {
                println!("Deleted service branch: {}", branch_name);
            }
        }
        Commands::List => {
            let branches = backend.list_branches().await?;
            if json_output {
                let enriched = enrich_branch_list_json(&branches, config);
                println!("{}", serde_json::to_string_pretty(&enriched)?);
            } else {
                println!("Branches ({}):", backend.backend_name());
                print_enriched_branch_list(&branches, config);
            }
        }
        Commands::Start { branch_name } => {
            if !backend.supports_lifecycle() {
                anyhow::bail!(
                    "Backend '{}' does not support start/stop lifecycle",
                    backend.backend_name()
                );
            }
            backend.start_branch(&branch_name).await?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "status": "ok",
                        "started": branch_name
                    }))?
                );
            } else {
                println!("Started branch: {}", branch_name);
            }
        }
        Commands::Stop { branch_name } => {
            if !backend.supports_lifecycle() {
                anyhow::bail!(
                    "Backend '{}' does not support start/stop lifecycle",
                    backend.backend_name()
                );
            }
            backend.stop_branch(&branch_name).await?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "status": "ok",
                        "stopped": branch_name
                    }))?
                );
            } else {
                println!("Stopped branch: {}", branch_name);
            }
        }
        Commands::Reset { branch_name } => {
            if !backend.supports_lifecycle() {
                anyhow::bail!(
                    "Backend '{}' does not support reset",
                    backend.backend_name()
                );
            }
            backend.reset_branch(&branch_name).await?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "status": "ok",
                        "reset": branch_name
                    }))?
                );
            } else {
                println!("Reset branch: {}", branch_name);
            }
        }
        Commands::Doctor => {
            let report = backend.doctor().await?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("Doctor report ({}):", backend.backend_name());
                for check in &report.checks {
                    let icon = if check.available { "OK" } else { "FAIL" };
                    println!("  [{}] {}: {}", icon, check.name, check.detail);
                }
            }
        }
        Commands::Connection {
            branch_name,
            format,
        } => {
            let conn = backend.get_connection_info(&branch_name).await?;
            // Global --json flag overrides --format
            let fmt = if json_output {
                "json"
            } else {
                format.as_deref().unwrap_or("uri")
            };
            match fmt {
                "uri" => {
                    if let Some(ref uri) = conn.connection_string {
                        println!("{}", uri);
                    } else {
                        println!(
                            "postgresql://{}@{}:{}/{}",
                            conn.user, conn.host, conn.port, conn.database
                        );
                    }
                }
                "env" => {
                    println!("DATABASE_HOST={}", conn.host);
                    println!("DATABASE_PORT={}", conn.port);
                    println!("DATABASE_NAME={}", conn.database);
                    println!("DATABASE_USER={}", conn.user);
                    if let Some(ref password) = conn.password {
                        println!("DATABASE_PASSWORD={}", password);
                    }
                    if let Some(ref uri) = conn.connection_string {
                        println!("DATABASE_URL={}", uri);
                    }
                }
                _ => {
                    println!("{}", serde_json::to_string_pretty(&conn)?);
                }
            }
        }
        Commands::Status => {
            let branches = backend.list_branches().await.unwrap_or_default();
            let running = branches
                .iter()
                .filter(|b| b.state.as_deref() == Some("running"))
                .count();
            let stopped = branches
                .iter()
                .filter(|b| b.state.as_deref() == Some("stopped"))
                .count();
            let project_info = backend.project_info();

            if json_output {
                let mut status = serde_json::json!({
                    "backend": backend.backend_name(),
                    "total_branches": branches.len(),
                    "running": running,
                    "stopped": stopped,
                    "supports_lifecycle": backend.supports_lifecycle(),
                });
                if let Some(ref info) = project_info {
                    status["project"] = serde_json::Value::String(info.name.clone());
                    if let Some(ref storage) = info.storage_backend {
                        status["storage"] = serde_json::Value::String(storage.clone());
                    }
                    if let Some(ref image) = info.image {
                        status["image"] = serde_json::Value::String(image.clone());
                    }
                }
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("Backend: {}", backend.backend_name());
                if let Some(ref info) = project_info {
                    println!("Project: {}", info.name);
                    if let Some(ref storage) = info.storage_backend {
                        println!("Storage: {}", storage);
                    }
                    if let Some(ref image) = info.image {
                        println!("Image: {}", image);
                    }
                }
                println!(
                    "Branches: {} total ({} running, {} stopped)",
                    branches.len(),
                    running,
                    stopped
                );
                if backend.supports_lifecycle() {
                    println!("Lifecycle: supported (start/stop/reset)");
                }
            }
        }
        Commands::Cleanup { max_count } => {
            let max = max_count.unwrap_or(config.behavior.max_branches.unwrap_or(10));
            let deleted = backend.cleanup_old_branches(max).await?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&deleted)?);
            } else if deleted.is_empty() {
                println!("No branches to clean up");
            } else {
                println!(
                    "Cleaned up {} branches: {}",
                    deleted.len(),
                    deleted.join(", ")
                );
            }
        }
        Commands::Destroy { force } => {
            if !backend.supports_destroy() {
                anyhow::bail!(
                    "Backend '{}' does not support destroy. This command is only available for the local (Docker + CoW) backend.",
                    backend.backend_name()
                );
            }

            let preview = backend.destroy_preview().await?;
            let (project_name, branch_names) = match preview {
                Some(p) => p,
                None => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string(&serde_json::json!({
                                "status": "ok",
                                "message": "no project found"
                            }))?
                        );
                    } else {
                        println!(
                            "No project found for database '{}'. Nothing to destroy.",
                            resolved_name
                        );
                    }
                    return Ok(());
                }
            };

            if !force && !non_interactive {
                println!("This will permanently destroy the following:");
                println!("  Project: {}", project_name);
                if branch_names.is_empty() {
                    println!("  Branches: (none)");
                } else {
                    println!("  Branches ({}):", branch_names.len());
                    for name in &branch_names {
                        println!("    - {}", name);
                    }
                }
                println!();
                println!("All containers, storage data, and state will be removed.");

                let confirm =
                    inquire::Confirm::new("Are you sure you want to destroy this project?")
                        .with_default(false)
                        .prompt()?;

                if !confirm {
                    println!("Aborted.");
                    return Ok(());
                }
            }

            let destroyed = backend.destroy_project().await?;

            // Remove the backend entry from local state
            if let Some(ref path) = config_path {
                if let Ok(mut state) = LocalStateManager::new() {
                    let _ = state.remove_backend(path, &resolved_name);
                }
            }

            // Also remove from committed config for backward compat (legacy configs)
            config.remove_backend(&resolved_name);
            if let Some(path) = config_path {
                config.save_to_file(path)?;
            }

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "project": project_name,
                        "destroyed_branches": destroyed,
                    }))?
                );
            } else {
                println!(
                    "Destroyed project '{}' and {} branch(es)",
                    project_name,
                    destroyed.len()
                );
                for name in &destroyed {
                    println!("  - {}", name);
                }
            }
        }
        Commands::Switch {
            branch_name,
            create,
            base,
            execute,
            no_services,
            no_verify,
            template,
            dry_run,
        } => {
            if dry_run {
                if let Some(branch) = branch_name {
                    let normalized_branch = config.get_normalized_branch_name(&branch);
                    let worktree_enabled = config.worktree.as_ref().is_some_and(|wt| wt.enabled);

                    if json_output {
                        let mut wt_path_value = serde_json::Value::Null;
                        if worktree_enabled {
                            let repo_name = std::env::current_dir()
                                .ok()
                                .and_then(|p| {
                                    p.file_name().map(|n| n.to_string_lossy().to_string())
                                })
                                .unwrap_or_else(|| "repo".to_string());
                            let path_template = config
                                .worktree
                                .as_ref()
                                .map(|wt| wt.path_template.as_str())
                                .unwrap_or("../{repo}.{branch}");
                            let wt_path = path_template
                                .replace("{repo}", &repo_name)
                                .replace("{branch}", &branch);
                            wt_path_value = serde_json::Value::String(wt_path);
                        }
                        let auto_backends: Vec<serde_json::Value> = if !no_services {
                            config
                                .resolve_backends()
                                .into_iter()
                                .filter(|b| b.auto_branch)
                                .map(|b| {
                                    serde_json::json!({
                                        "name": b.name,
                                        "service_type": b.service_type,
                                    })
                                })
                                .collect()
                        } else {
                            vec![]
                        };
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "dry_run": true,
                                "branch": normalized_branch,
                                "worktree_enabled": worktree_enabled,
                                "worktree_path": wt_path_value,
                                "services_skipped": no_services,
                                "auto_branch_services": auto_backends,
                                "hooks_skipped": no_verify,
                                "execute": execute,
                            }))?
                        );
                    } else {
                        println!("Dry run: would switch to branch: {}", normalized_branch);
                        if worktree_enabled {
                            println!("  Worktree mode: enabled");
                            let repo_name = std::env::current_dir()
                                .ok()
                                .and_then(|p| {
                                    p.file_name().map(|n| n.to_string_lossy().to_string())
                                })
                                .unwrap_or_else(|| "repo".to_string());
                            let path_template = config
                                .worktree
                                .as_ref()
                                .map(|wt| wt.path_template.as_str())
                                .unwrap_or("../{repo}.{branch}");
                            let wt_path = path_template
                                .replace("{repo}", &repo_name)
                                .replace("{branch}", &branch);
                            println!("  Worktree path: {}", wt_path);
                        }
                        if !no_services {
                            let auto_backends = config
                                .resolve_backends()
                                .into_iter()
                                .filter(|b| b.auto_branch)
                                .collect::<Vec<_>>();
                            if auto_backends.is_empty() {
                                println!(
                                    "  Would create/switch service branches (default backend)"
                                );
                            } else {
                                println!(
                                    "  Would create/switch service branches on {} service(s):",
                                    auto_backends.len()
                                );
                                for b in &auto_backends {
                                    println!("    - {} ({})", b.name, b.service_type);
                                }
                            }
                        }
                        if !no_verify {
                            if config.hooks.is_some() {
                                println!("  Would run post-switch hooks");
                            }
                        }
                        if let Some(ref cmd) = execute {
                            println!("  Would execute after switch: {}", cmd);
                        }
                    }
                } else if json_output {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "error": "dry_run_requires_branch_name",
                            "message": "Dry run requires a branch name",
                        }))?
                    );
                } else {
                    println!("Dry run requires a branch name");
                }
            } else if template {
                handle_switch_to_main(config, config_path).await?;
            } else if let Some(branch) = branch_name {
                handle_switch_command(
                    config,
                    &branch,
                    config_path,
                    create,
                    base.as_deref(),
                    no_services,
                    no_verify,
                    json_output,
                )
                .await?;
            } else if non_interactive {
                anyhow::bail!(
                    "No branch specified. Use 'devflow switch <branch>' in non-interactive mode."
                );
            } else {
                handle_interactive_switch(config, config_path).await?;
            }

            // Execute post-switch command if requested
            if let Some(ref cmd) = execute {
                println!("Running post-switch command: {}", cmd);
                let status = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(cmd)
                    .status()
                    .await
                    .context("Failed to execute post-switch command")?;
                if !status.success() {
                    anyhow::bail!(
                        "Post-switch command failed with exit code: {}",
                        status.code().unwrap_or(-1)
                    );
                }
            }
        }
        Commands::GitHook {
            worktree,
            main_worktree_dir,
        } => {
            handle_git_hook(config, config_path, worktree, main_worktree_dir).await?;
        }
        Commands::WorktreeSetup => {
            handle_worktree_setup(config, config_path).await?;
        }
        Commands::Remove {
            branch_name,
            force,
            keep_services,
        } => {
            handle_remove_command(
                config,
                &branch_name,
                force,
                keep_services,
                config_path,
                json_output,
                non_interactive,
            )
            .await?;
        }
        Commands::Merge {
            target,
            cleanup,
            dry_run,
        } => {
            handle_merge_command(config, target.as_deref(), cleanup, dry_run, json_output).await?;
        }
        Commands::Logs { branch_name, tail } => {
            let output = backend.logs(&branch_name, tail).await?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "branch": branch_name,
                        "logs": output,
                    }))?
                );
            } else {
                print!("{output}");
            }
        }
        Commands::Seed { branch_name, from } => {
            println!("Seeding branch '{}' from '{}'...", branch_name, from);
            backend.seed_from_source(&branch_name, &from).await?;
            println!("Seed complete.");
        }
        _ => unreachable!(),
    }

    Ok(())
}

/// Handle aggregation commands (List, Status, Doctor) across all backends.
async fn handle_multi_backend_command(
    cmd: Commands,
    config: &Config,
    json_output: bool,
) -> Result<()> {
    let all_backends = services::factory::create_all_backends(config).await?;

    match cmd {
        Commands::List => {
            // Gather all service branches from all backends
            let mut all_service_branches: Vec<services::BranchInfo> = Vec::new();
            if json_output {
                let mut map = serde_json::Map::new();
                for named in &all_backends {
                    let branches = named.backend.list_branches().await.unwrap_or_default();
                    map.insert(
                        named.name.clone(),
                        enrich_branch_list_json(&branches, config),
                    );
                }
                println!("{}", serde_json::to_string_pretty(&map)?);
            } else {
                for named in &all_backends {
                    let branches = named.backend.list_branches().await.unwrap_or_default();
                    all_service_branches.extend(branches);
                    println!("[{}] ({}):", named.name, named.backend.backend_name());
                }
                print_enriched_branch_list(&all_service_branches, config);
                println!();
            }
        }
        Commands::Status => {
            if json_output {
                let mut map = serde_json::Map::new();
                for named in &all_backends {
                    let branches = named.backend.list_branches().await.unwrap_or_default();
                    let running = branches
                        .iter()
                        .filter(|b| b.state.as_deref() == Some("running"))
                        .count();
                    let stopped = branches
                        .iter()
                        .filter(|b| b.state.as_deref() == Some("stopped"))
                        .count();
                    let project_info = named.backend.project_info();

                    let mut status = serde_json::json!({
                        "backend": named.backend.backend_name(),
                        "total_branches": branches.len(),
                        "running": running,
                        "stopped": stopped,
                        "supports_lifecycle": named.backend.supports_lifecycle(),
                    });
                    if let Some(ref info) = project_info {
                        status["project"] = serde_json::Value::String(info.name.clone());
                        if let Some(ref storage) = info.storage_backend {
                            status["storage"] = serde_json::Value::String(storage.clone());
                        }
                        if let Some(ref image) = info.image {
                            status["image"] = serde_json::Value::String(image.clone());
                        }
                    }
                    map.insert(named.name.clone(), status);
                }
                println!("{}", serde_json::to_string_pretty(&map)?);
            } else {
                for named in &all_backends {
                    let branches = named.backend.list_branches().await.unwrap_or_default();
                    let running = branches
                        .iter()
                        .filter(|b| b.state.as_deref() == Some("running"))
                        .count();
                    let stopped = branches
                        .iter()
                        .filter(|b| b.state.as_deref() == Some("stopped"))
                        .count();
                    let project_info = named.backend.project_info();

                    println!("[{}] ({}):", named.name, named.backend.backend_name());
                    if let Some(ref info) = project_info {
                        println!("  Project: {}", info.name);
                        if let Some(ref storage) = info.storage_backend {
                            println!("  Storage: {}", storage);
                        }
                        if let Some(ref image) = info.image {
                            println!("  Image: {}", image);
                        }
                    }
                    println!(
                        "  Branches: {} total ({} running, {} stopped)",
                        branches.len(),
                        running,
                        stopped
                    );
                    if named.backend.supports_lifecycle() {
                        println!("  Lifecycle: supported (start/stop/reset)");
                    }
                    println!();
                }
            }
        }
        Commands::Doctor => {
            if json_output {
                let mut map = serde_json::Map::new();
                for named in &all_backends {
                    let report = named.backend.doctor().await?;
                    map.insert(named.name.clone(), serde_json::to_value(&report)?);
                }
                println!("{}", serde_json::to_string_pretty(&map)?);
            } else {
                for named in &all_backends {
                    let report = named.backend.doctor().await?;
                    println!(
                        "[{}] Doctor report ({}):",
                        named.name,
                        named.backend.backend_name()
                    );
                    for check in &report.checks {
                        let icon = if check.available { "OK" } else { "FAIL" };
                        println!("  [{}] {}: {}", icon, check.name, check.detail);
                    }
                    println!();
                }
            }
        }
        _ => unreachable!(),
    }

    Ok(())
}

/// Handle Create/Delete across all auto-branch backends when no specific --database is given.
async fn handle_orchestrated_mutation(
    cmd: Commands,
    config: &Config,
    json_output: bool,
) -> Result<()> {
    match cmd {
        Commands::Create { branch_name, from } => {
            let results =
                services::factory::orchestrate_create(config, &branch_name, from.as_deref())
                    .await?;

            if json_output {
                let json: Vec<_> = results
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "service": r.service_name,
                            "success": r.success,
                            "message": r.message,
                            "branch_info": r.branch_info,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json)?);
            } else {
                let success_count = results.iter().filter(|r| r.success).count();
                let fail_count = results.iter().filter(|r| !r.success).count();

                for r in &results {
                    if r.success {
                        println!("[{}] {}", r.service_name, r.message);
                        if let Some(ref info) = r.branch_info {
                            if let Some(ref state) = info.state {
                                println!("  State: {}", state);
                            }
                        }
                    } else {
                        eprintln!("[{}] {}", r.service_name, r.message);
                    }
                }

                if fail_count > 0 {
                    eprintln!(
                        "\nCreated branch on {}/{} services ({} failed)",
                        success_count,
                        results.len(),
                        fail_count
                    );
                }
            }

            // Run hooks after all services are created
            run_hooks(config, &branch_name, HookPhase::PostServiceCreate).await?;
        }
        Commands::Delete { branch_name } => {
            let results = services::factory::orchestrate_delete(config, &branch_name).await?;

            if json_output {
                let json: Vec<_> = results
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "service": r.service_name,
                            "success": r.success,
                            "message": r.message,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json)?);
            } else {
                let success_count = results.iter().filter(|r| r.success).count();
                let fail_count = results.iter().filter(|r| !r.success).count();

                for r in &results {
                    if r.success {
                        println!("[{}] {}", r.service_name, r.message);
                    } else {
                        eprintln!("[{}] {}", r.service_name, r.message);
                    }
                }

                if fail_count > 0 {
                    eprintln!(
                        "\nDeleted branch on {}/{} services ({} failed)",
                        success_count,
                        results.len(),
                        fail_count
                    );
                }
            }
        }
        _ => unreachable!(),
    }

    Ok(())
}

/// Run configuration and environment checks as part of `doctor`.
fn run_doctor_pre_checks(config: &Config, config_path: &Option<std::path::PathBuf>) {
    println!("General:");

    // Config file
    match config_path {
        Some(path) => println!("  [OK] Config file: {}", path.display()),
        None => {
            println!("  [WARN] Config file: not found (run 'devflow init' to create .devflow.yml)")
        }
    }

    // VCS repository
    let vcs_repo = vcs::detect_vcs_provider(".");
    match &vcs_repo {
        Ok(vcs) => println!("  [OK] {} repository: detected", vcs.provider_name()),
        Err(_) => println!("  [FAIL] VCS repository: not found"),
    }

    // VCS hooks
    let hooks_dir = std::path::Path::new(".git/hooks");
    let has_hooks = if hooks_dir.exists() {
        let post_checkout = hooks_dir.join("post-checkout");
        let post_merge = hooks_dir.join("post-merge");
        if let Ok(ref vcs) = vcs_repo {
            (post_checkout.exists() && vcs.is_devflow_hook(&post_checkout).unwrap_or(false))
                || (post_merge.exists() && vcs.is_devflow_hook(&post_merge).unwrap_or(false))
        } else {
            post_checkout.exists() || post_merge.exists()
        }
    } else {
        false
    };
    if has_hooks {
        println!("  [OK] VCS hooks: installed");
    } else {
        println!("  [WARN] VCS hooks: not installed (run 'devflow install-hooks')");
    }

    // Branch filter regex
    if let Some(ref regex_pattern) = config.git.branch_filter_regex {
        match regex::Regex::new(regex_pattern) {
            Ok(_) => println!("  [OK] Branch filter regex: valid"),
            Err(e) => println!("  [FAIL] Branch filter regex: {}", e),
        }
    }

    println!();
}

fn copy_worktree_files(config: &Config, main_worktree_dir: &str) -> Result<()> {
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
            std::fs::copy(&source, &target)?;
            println!("Copied {} from main worktree", file);
        }
    }

    // 2. Copy gitignored files when copy_ignored is enabled
    if wt_config.copy_ignored {
        if let Ok(vcs_repo) = vcs::detect_vcs_provider(main_worktree_dir) {
            match vcs_repo.list_ignored_files() {
                Ok(ignored_files) => {
                    let mut count = 0;
                    for rel_path in &ignored_files {
                        let source = main_dir.join(rel_path);
                        let target = current_dir.join(rel_path);

                        if source.exists() && !target.exists() {
                            if let Some(parent) = target.parent() {
                                std::fs::create_dir_all(parent).ok();
                            }
                            if let Err(e) = std::fs::copy(&source, &target) {
                                log::warn!(
                                    "Failed to copy ignored file '{}': {}",
                                    rel_path.display(),
                                    e
                                );
                            } else {
                                count += 1;
                                log::debug!("Copied ignored file: {}", rel_path.display());
                            }
                        }
                    }
                    if count > 0 {
                        println!("Copied {} ignored file(s) from main worktree", count);
                    }
                }
                Err(e) => {
                    log::warn!("Failed to enumerate ignored files: {}", e);
                }
            }
        }
    }

    Ok(())
}

async fn handle_worktree_setup(
    config: &Config,
    config_path: &Option<std::path::PathBuf>,
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

    // Run normal git-hook logic to create/switch service branches
    handle_git_hook(config, config_path, false, None).await?;

    Ok(())
}

async fn handle_git_hook(
    config: &Config,
    config_path: &Option<std::path::PathBuf>,
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

    if let Some(current_git_branch) = vcs_repo.current_branch()? {
        log::info!("Git hook triggered for branch: {}", current_git_branch);

        // Check if this branch should trigger a switch
        if config.should_switch_on_branch(&current_git_branch) {
            // If switching to main git branch, use main database
            if current_git_branch == config.git.main_branch {
                handle_switch_to_main(config, config_path).await?;
            } else {
                // For other branches, check if we should create them and switch
                if config.should_create_branch(&current_git_branch) {
                    handle_switch_command(
                        config,
                        &current_git_branch,
                        config_path,
                        false, // create — branch already exists from git
                        None,  // base
                        false, // no_services
                        false, // no_verify
                        false, // json_output — git hooks are non-interactive
                    )
                    .await?;
                } else {
                    log::info!(
                        "Git branch {} configured not to create service branches",
                        current_git_branch
                    );
                }
            }
        } else {
            log::info!(
                "Git branch {} filtered out by auto_switch configuration",
                current_git_branch
            );
        }
    }

    Ok(())
}

async fn handle_interactive_switch(
    config: &Config,
    config_path: &Option<std::path::PathBuf>,
) -> Result<()> {
    // Get available branches from the default backend
    let mut branches = match services::factory::resolve_backend(config, None).await {
        Ok(named) => match named.backend.list_branches().await {
            Ok(branch_infos) => branch_infos.into_iter().map(|b| b.name).collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        },
        Err(_) => Vec::new(),
    };

    // Always add main at the beginning if not already present
    if !branches.iter().any(|b| b == "main") {
        branches.insert(0, "main".to_string());
    }

    // Detect current branch
    let current_git = vcs::detect_vcs_provider(".")
        .ok()
        .and_then(|r| r.current_branch().ok().flatten());

    // Create branch items with display info
    let mut branch_items: Vec<BranchItem> = branches
        .iter()
        .map(|branch| {
            let is_current = current_git.as_deref() == Some(branch.as_str())
                || (branch == "main" && current_git.as_deref() == Some(&config.git.main_branch));

            BranchItem {
                name: branch.clone(),
                display_name: branch.clone(),
                is_current,
            }
        })
        .collect();

    // Add a "Create new branch" option at the end
    branch_items.push(BranchItem {
        name: "__create_new__".to_string(),
        display_name: "+ Create new branch".to_string(),
        is_current: false,
    });

    // Run interactive selector
    match run_interactive_selector(branch_items) {
        Ok(selected_branch) => {
            if selected_branch == "__create_new__" {
                // Prompt for a new branch name
                let new_name = inquire::Text::new("New branch name:")
                    .with_help_message("Enter the name for the new branch")
                    .prompt()
                    .context("Failed to read branch name")?;
                let new_name = new_name.trim().to_string();
                if new_name.is_empty() {
                    anyhow::bail!("Branch name cannot be empty");
                }
                handle_switch_command(
                    config,
                    &new_name,
                    config_path,
                    true,  // create
                    None,  // base
                    false, // no_services
                    false, // no_verify
                    false, // json_output
                )
                .await?;
            } else if selected_branch == "main" {
                handle_switch_to_main(config, config_path).await?;
            } else {
                handle_switch_command(
                    config,
                    &selected_branch,
                    config_path,
                    false, // create
                    None,  // base
                    false, // no_services
                    false, // no_verify
                    false, // json_output — interactive mode
                )
                .await?;
            }
        }
        Err(e) => match e {
            inquire::InquireError::OperationCanceled => {
                println!("Cancelled.");
            }
            inquire::InquireError::OperationInterrupted => {
                println!("Interrupted.");
            }
            _ => {
                println!("Interactive mode failed: {}", e);
                println!("Try using: devflow switch <branch-name> or devflow switch --template");
            }
        },
    }

    Ok(())
}

#[derive(Clone)]
struct BranchItem {
    name: String,
    display_name: String,
    is_current: bool,
}

fn run_interactive_selector(items: Vec<BranchItem>) -> Result<String, inquire::InquireError> {
    use inquire::Select;

    if items.is_empty() {
        return Err(inquire::InquireError::InvalidConfiguration(
            "No branches available".to_string(),
        ));
    }

    // Create display options with current branch marker
    let options: Vec<String> = items
        .iter()
        .map(|item| {
            if item.is_current {
                format!("{} *", item.display_name)
            } else {
                item.display_name.clone()
            }
        })
        .collect();

    // Find the default selection (current branch if available)
    let default = items.iter().position(|item| item.is_current);

    let mut select = Select::new("Select a branch to switch to:", options.clone())
        .with_help_message(
            "Use arrow keys to navigate, type to filter, Enter to select, Esc to cancel",
        );

    if let Some(default_index) = default {
        select = select.with_starting_cursor(default_index);
    }

    // Run the selector
    let selected_display = select.prompt()?;

    // Find the corresponding branch name
    let selected_index = options
        .iter()
        .position(|opt| opt == &selected_display)
        .ok_or_else(|| {
            inquire::InquireError::InvalidConfiguration("Selected option not found".to_string())
        })?;

    Ok(items[selected_index].name.clone())
}

#[allow(clippy::too_many_arguments)]
async fn handle_switch_command(
    config: &Config,
    branch_name: &str,
    config_path: &Option<std::path::PathBuf>,
    create: bool,
    base: Option<&str>,
    no_services: bool,
    no_verify: bool,
    json_output: bool,
) -> Result<()> {
    let worktree_enabled = config.worktree.as_ref().is_some_and(|wt| wt.enabled);
    let mut worktree_path: Option<String> = None;
    let mut worktree_created = false;

    // ── Worktree mode ──────────────────────────────────────────────────
    if worktree_enabled {
        let vcs_repo = vcs::detect_vcs_provider(".").context("Failed to open VCS repository")?;

        // Check if a worktree already exists for this branch
        let existing_path = vcs_repo.worktree_path(branch_name)?;

        if let Some(wt_path) = existing_path {
            if !json_output {
                println!("Switching to existing worktree: {}", wt_path.display());
            }
            worktree_path = Some(wt_path.display().to_string());
            // Print the path so shell integration can cd to it
            println!("DEVFLOW_CD={}", wt_path.display());
        } else {
            // Resolve worktree path from template
            let repo_name = std::env::current_dir()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_else(|| "repo".to_string());
            let path_template = config
                .worktree
                .as_ref()
                .map(|wt| wt.path_template.as_str())
                .unwrap_or("../{repo}.{branch}");
            let wt_path_str = path_template
                .replace("{repo}", &repo_name)
                .replace("{branch}", branch_name);
            let wt_path = PathBuf::from(&wt_path_str);

            // Create branch if --create or branch doesn't exist
            if create || !vcs_repo.branch_exists(branch_name)? {
                if !json_output {
                    println!(
                        "Creating branch '{}' (base: {})",
                        branch_name,
                        base.unwrap_or("HEAD")
                    );
                }
                let _ = vcs_repo.create_branch(branch_name, base);
            }

            if !json_output {
                println!("Creating worktree at: {}", wt_path.display());
            }
            vcs_repo
                .create_worktree(branch_name, &wt_path)
                .with_context(|| {
                    format!("Failed to create worktree for branch '{}'", branch_name)
                })?;

            // Copy files if configured
            if let Some(ref wt_config) = config.worktree {
                let main_dir = std::env::current_dir()?;

                // Copy explicitly listed files
                for file in &wt_config.copy_files {
                    let src = main_dir.join(file);
                    let dst = wt_path.join(file);
                    if src.exists() {
                        if let Some(parent) = dst.parent() {
                            std::fs::create_dir_all(parent).ok();
                        }
                        if let Err(e) = std::fs::copy(&src, &dst) {
                            log::warn!("Failed to copy '{}' to worktree: {}", file, e);
                        } else {
                            log::debug!("Copied '{}' to worktree", file);
                        }
                    }
                }

                // Copy gitignored files when copy_ignored is enabled
                if wt_config.copy_ignored {
                    match vcs_repo.list_ignored_files() {
                        Ok(ignored_files) => {
                            let mut count = 0;
                            for rel_path in &ignored_files {
                                let src = main_dir.join(rel_path);
                                let dst = wt_path.join(rel_path);
                                if src.exists() && !dst.exists() {
                                    if let Some(parent) = dst.parent() {
                                        std::fs::create_dir_all(parent).ok();
                                    }
                                    if let Err(e) = std::fs::copy(&src, &dst) {
                                        log::warn!(
                                            "Failed to copy ignored file '{}': {}",
                                            rel_path.display(),
                                            e
                                        );
                                    } else {
                                        count += 1;
                                        log::debug!("Copied ignored file: {}", rel_path.display());
                                    }
                                }
                            }
                            if count > 0 {
                                println!("Copied {} ignored file(s) to worktree", count);
                            }
                        }
                        Err(e) => {
                            log::warn!("Failed to enumerate ignored files: {}", e);
                        }
                    }
                }
            }

            if !json_output {
                println!(
                    "Created worktree for '{}' at {}",
                    branch_name,
                    wt_path.display()
                );
            }
            worktree_path = Some(wt_path.display().to_string());
            worktree_created = true;
            println!("DEVFLOW_CD={}", wt_path.display());
        }
    } else {
        // ── Classic mode (no worktrees) ────────────────────────────────
        let vcs_repo = vcs::detect_vcs_provider(".").context("Failed to open VCS repository")?;
        if create || !vcs_repo.branch_exists(branch_name)? {
            if !json_output {
                println!(
                    "Creating branch '{}' (base: {})",
                    branch_name,
                    base.unwrap_or("HEAD")
                );
            }
            vcs_repo.create_branch(branch_name, base)?;
        }
        // Switch the working directory to the target branch
        if !json_output {
            println!("Checking out branch: {}", branch_name);
        }
        vcs_repo.checkout_branch(branch_name)?;
    }

    // ── Service branching (orchestrated across all auto_branch backends) ──
    let normalized_branch = config.get_normalized_branch_name(branch_name);

    if !no_services {
        if !json_output {
            println!("Switching service branches: {}", normalized_branch);
        }

        // Update current branch in local state
        if let Some(ref path) = config_path {
            if let Ok(mut state) = LocalStateManager::new() {
                let _ = state.set_current_branch(path, Some(normalized_branch.clone()));
            }
        }

        // Orchestrate switch across all auto-branch backends
        let results =
            services::factory::orchestrate_switch(config, &normalized_branch, None).await?;

        let success_count = results.iter().filter(|r| r.success).count();
        let fail_count = results.iter().filter(|r| !r.success).count();

        if json_output {
            let service_results: Vec<serde_json::Value> = results
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "service": r.service_name,
                        "success": r.success,
                        "message": r.message,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "branch": normalized_branch,
                    "worktree_path": worktree_path,
                    "worktree_created": worktree_created,
                    "services_switched": success_count,
                    "services_failed": fail_count,
                    "service_results": service_results,
                }))?
            );
        } else {
            for r in &results {
                if r.success {
                    log::info!("[{}] {}", r.service_name, r.message);
                } else {
                    println!("Warning: {}", r.message);
                }
            }

            if success_count > 0 && fail_count == 0 {
                println!(
                    "Switched to service branch: {} ({} service(s))",
                    normalized_branch, success_count
                );
            } else if success_count > 0 {
                println!(
                    "Switched to service branch: {} ({}/{} service(s), {} failed)",
                    normalized_branch,
                    success_count,
                    results.len(),
                    fail_count
                );
            } else if !results.is_empty() {
                println!(
                    "Warning: Failed to switch service branches on all {} service(s)",
                    results.len()
                );
            }
        }
    } else {
        // Still update state even if skipping services
        if let Some(ref path) = config_path {
            if let Ok(mut state) = LocalStateManager::new() {
                let _ = state.set_current_branch(path, Some(normalized_branch.clone()));
            }
        }
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "branch": normalized_branch,
                    "worktree_path": worktree_path,
                    "worktree_created": worktree_created,
                    "services_skipped": true,
                }))?
            );
        } else {
            println!("Switched branch (services skipped): {}", normalized_branch);
        }
    }

    // ── Hooks ──────────────────────────────────────────────────────────
    if !no_verify {
        run_hooks(config, &normalized_branch, HookPhase::PostSwitch).await?;
    }

    Ok(())
}

async fn handle_switch_to_main(
    config: &Config,
    config_path: &Option<std::path::PathBuf>,
) -> Result<()> {
    let main_name = "_main";

    println!("Switching to main database");

    // Update current branch in local state to a special main marker
    if let Some(ref path) = config_path {
        if let Ok(mut state) = LocalStateManager::new() {
            let _ = state.set_current_branch(path, Some(main_name.to_string()));
        }
    }

    // Switch to main on all auto-branch backends
    let results = services::factory::orchestrate_switch(config, "main", None).await;
    if let Ok(results) = results {
        for r in &results {
            if !r.success {
                log::warn!("{}", r.message);
            }
        }
    }

    println!(
        "Switched to main database: {}",
        config.database.template_database
    );

    // Execute hooks
    run_hooks(config, main_name, HookPhase::PostSwitch).await?;

    Ok(())
}

async fn handle_remove_command(
    config: &Config,
    branch_name: &str,
    force: bool,
    keep_services: bool,
    config_path: &Option<std::path::PathBuf>,
    json_output: bool,
    non_interactive: bool,
) -> Result<()> {
    let vcs_repo = vcs::detect_vcs_provider(".").context("Failed to open VCS repository")?;

    // Safety check: don't remove main branch
    if branch_name == config.git.main_branch {
        anyhow::bail!("Cannot remove the main branch '{}'", branch_name);
    }

    // Safety check: don't remove the currently checked-out branch
    if let Ok(Some(current)) = vcs_repo.current_branch() {
        if current == branch_name {
            anyhow::bail!(
                "Cannot remove branch '{}' because it is currently checked out. Switch to another branch first.",
                branch_name
            );
        }
    }

    // Confirm unless --force (skip prompt in JSON/non-interactive mode — require --force)
    if !force {
        if json_output || non_interactive {
            anyhow::bail!("Use --force to confirm removal in non-interactive or JSON output mode");
        }
        println!("This will remove:");
        println!("  - VCS branch: {}", branch_name);
        if vcs_repo.worktree_path(branch_name)?.is_some() {
            println!("  - Worktree directory");
        }
        if !keep_services {
            println!("  - Associated service branches");
        }
        print!("Continue? [y/N] ");
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    let mut worktree_removed = false;
    let mut worktree_path_str: Option<String> = None;
    let mut service_results: Vec<serde_json::Value> = Vec::new();
    let mut branch_deleted = false;

    // 1. Remove worktree (if it exists)
    if let Some(wt_path) = vcs_repo.worktree_path(branch_name)? {
        worktree_path_str = Some(wt_path.display().to_string());
        if !json_output {
            println!("Removing worktree at: {}", wt_path.display());
        }
        if let Err(e) = vcs_repo.remove_worktree(&wt_path) {
            log::warn!(
                "Failed to remove worktree, falling back to fs removal: {}",
                e
            );
            if wt_path.exists() {
                std::fs::remove_dir_all(&wt_path).context("Failed to remove worktree directory")?;
            }
        }
        worktree_removed = true;
        if !json_output {
            println!("Worktree removed.");
        }
    }

    // 2. Delete service branches (unless --keep-services)
    if !keep_services {
        let normalized = config.get_normalized_branch_name(branch_name);
        if !json_output {
            println!("Deleting service branches for: {}", normalized);
        }

        let results = services::factory::orchestrate_delete(config, &normalized).await?;

        for r in &results {
            if json_output {
                service_results.push(serde_json::json!({
                    "service": r.service_name,
                    "success": r.success,
                    "message": r.message,
                }));
            } else if r.success {
                println!("  [{}] {}", r.service_name, r.message);
            } else {
                println!("  [{}] Warning: {}", r.service_name, r.message);
            }
        }
    }

    // 3. Delete the VCS branch
    if !json_output {
        println!("Deleting branch: {}", branch_name);
    }
    if let Err(e) = vcs_repo.delete_branch(branch_name) {
        log::warn!("Failed to delete branch '{}': {}", branch_name, e);
        if !json_output {
            println!("Warning: Failed to delete branch: {}", e);
        }
    } else {
        branch_deleted = true;
        if !json_output {
            println!("Branch deleted: {}", branch_name);
        }
    }

    // 4. Clear local state if this was the current branch
    if let Some(ref path) = config_path {
        if let Ok(state) = LocalStateManager::new() {
            if let Some(current) = state.get_current_branch(path) {
                let normalized = config.get_normalized_branch_name(branch_name);
                if current == normalized {
                    if let Ok(mut state) = LocalStateManager::new() {
                        let _ = state.set_current_branch(path, None);
                    }
                }
            }
        }
    }

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "ok",
                "branch": branch_name,
                "branch_deleted": branch_deleted,
                "worktree_removed": worktree_removed,
                "worktree_path": worktree_path_str,
                "services_skipped": keep_services,
                "service_results": service_results,
            }))?
        );
    } else {
        println!("Branch '{}' removed successfully.", branch_name);
    }
    Ok(())
}

async fn handle_merge_command(
    config: &Config,
    target: Option<&str>,
    cleanup: bool,
    dry_run: bool,
    json_output: bool,
) -> Result<()> {
    let vcs_repo = vcs::detect_vcs_provider(".").context("Failed to open VCS repository")?;

    // Determine source branch (current branch)
    let source = vcs_repo
        .current_branch()?
        .ok_or_else(|| anyhow::anyhow!("Could not determine current branch (detached HEAD?)"))?;

    // Determine target branch
    let target_branch = target.unwrap_or(&config.git.main_branch);

    if source == target_branch {
        anyhow::bail!("Source and target branch are the same: '{}'", source);
    }

    if dry_run {
        if json_output {
            let normalized = config.get_normalized_branch_name(&source);
            let has_worktree = vcs_repo.worktree_path(&source)?.is_some();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "dry_run": true,
                    "source": source,
                    "target": target_branch,
                    "cleanup": cleanup,
                    "has_worktree": has_worktree,
                    "normalized_service_branch": normalized,
                }))?
            );
        } else {
            println!("Merge plan:");
            println!("  Source: {}", source);
            println!("  Target: {}", target_branch);
            if cleanup {
                println!(
                    "  Cleanup: will delete source branch, worktree, and service branches after merge"
                );
            }
            println!("\n[dry-run] No changes made.");
        }
        return Ok(());
    }

    if !json_output {
        println!("Merge plan:");
        println!("  Source: {}", source);
        println!("  Target: {}", target_branch);
        if cleanup {
            println!(
                "  Cleanup: will delete source branch, worktree, and service branches after merge"
            );
        }
    }

    // Perform the merge using git CLI (git2 merge is complex; shelling out is more reliable)
    if !json_output {
        println!("\nMerging '{}' into '{}'...", source, target_branch);
    }
    let status = tokio::process::Command::new("git")
        .args(["merge", &source, "--no-edit"])
        .status()
        .await
        .context("Failed to execute git merge")?;

    if !status.success() {
        anyhow::bail!(
            "git merge failed (exit code {}). Resolve conflicts and try again.",
            status.code().unwrap_or(-1)
        );
    }

    if !json_output {
        println!("Merge successful.");
    }

    let mut cleanup_result = serde_json::json!(null);

    // Cleanup if requested
    if cleanup {
        if !json_output {
            println!("\nCleaning up source branch '{}'...", source);
        }

        let mut worktree_removed = false;
        let mut branch_deleted = false;
        let mut service_results: Vec<serde_json::Value> = Vec::new();

        // Remove worktree if exists
        if let Ok(Some(wt_path)) = vcs_repo.worktree_path(&source) {
            if !json_output {
                println!("Removing worktree at: {}", wt_path.display());
            }
            if let Err(e) = vcs_repo.remove_worktree(&wt_path) {
                log::warn!("Failed to remove worktree: {}", e);
                if wt_path.exists() {
                    std::fs::remove_dir_all(&wt_path).ok();
                }
            }
            worktree_removed = true;
        }

        // Delete VCS branch
        if let Err(e) = vcs_repo.delete_branch(&source) {
            log::warn!("Failed to delete branch '{}': {}", source, e);
            if !json_output {
                println!("Warning: Failed to delete branch: {}", e);
            }
        } else {
            branch_deleted = true;
            if !json_output {
                println!("Deleted branch: {}", source);
            }
        }

        // Delete service branches across all auto-branch backends
        let normalized = config.get_normalized_branch_name(&source);
        let results = services::factory::orchestrate_delete(config, &normalized).await;
        match results {
            Ok(results) => {
                for r in &results {
                    if json_output {
                        service_results.push(serde_json::json!({
                            "service": r.service_name,
                            "success": r.success,
                            "message": r.message,
                        }));
                    } else if r.success {
                        println!("{}", r.message);
                    } else {
                        log::warn!("{}", r.message);
                        println!("Warning: {}", r.message);
                    }
                }
            }
            Err(e) => {
                log::warn!("Failed to delete service branches: {}", e);
                if !json_output {
                    println!("Warning: Failed to delete service branches: {}", e);
                }
            }
        }

        if !json_output {
            println!("Cleanup complete.");
        }

        cleanup_result = serde_json::json!({
            "worktree_removed": worktree_removed,
            "branch_deleted": branch_deleted,
            "service_results": service_results,
        });
    }

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "ok",
                "source": source,
                "target": target_branch,
                "cleanup": cleanup_result,
            }))?
        );
    }

    Ok(())
}

fn show_effective_config(effective_config: &EffectiveConfig) -> Result<()> {
    println!("🔧 Effective Configuration");
    println!("==========================\n");

    // Show configuration status
    println!("📊 Status:");
    if effective_config.is_disabled() {
        println!("  ❌ devflow is DISABLED globally");
    } else {
        println!("  ✅ devflow is enabled");
    }

    if effective_config.should_skip_hooks() {
        println!("  ❌ Git hooks are DISABLED");
    } else {
        println!("  ✅ Git hooks are enabled");
    }

    if effective_config.is_current_branch_disabled() {
        println!("  ❌ Current branch operations are DISABLED");
    } else {
        println!("  ✅ Current branch operations are enabled");
    }

    // Check if current git branch is disabled
    match effective_config.check_current_git_branch_disabled() {
        Ok(true) => println!("  ❌ Current Git branch is DISABLED"),
        Ok(false) => {
            if let Ok(vcs_repo) = vcs::detect_vcs_provider(".") {
                if let Ok(Some(branch)) = vcs_repo.current_branch() {
                    println!(
                        "  ✅ Current {} branch '{}' is enabled",
                        vcs_repo.provider_name(),
                        branch
                    );
                } else {
                    println!("  ⚠️  Could not determine current branch");
                }
            } else {
                println!("  ⚠️  Not in a VCS repository");
            }
        }
        Err(e) => println!("  ⚠️  Error checking current branch: {}", e),
    }

    println!();

    // Show environment variable overrides
    println!("🌍 Environment Variable Overrides:");
    let has_env_overrides = effective_config.env_config.disabled.is_some()
        || effective_config.env_config.skip_hooks.is_some()
        || effective_config.env_config.auto_create.is_some()
        || effective_config.env_config.auto_switch.is_some()
        || effective_config.env_config.branch_filter_regex.is_some()
        || effective_config.env_config.disabled_branches.is_some()
        || effective_config
            .env_config
            .current_branch_disabled
            .is_some()
        || effective_config.env_config.database_host.is_some()
        || effective_config.env_config.database_port.is_some()
        || effective_config.env_config.database_user.is_some()
        || effective_config.env_config.database_password.is_some()
        || effective_config.env_config.database_prefix.is_some();

    if !has_env_overrides {
        println!("  (none)");
    } else {
        if let Some(disabled) = effective_config.env_config.disabled {
            println!("  DEVFLOW_DISABLED: {}", disabled);
        }
        if let Some(skip_hooks) = effective_config.env_config.skip_hooks {
            println!("  DEVFLOW_SKIP_HOOKS: {}", skip_hooks);
        }
        if let Some(auto_create) = effective_config.env_config.auto_create {
            println!("  DEVFLOW_AUTO_CREATE: {}", auto_create);
        }
        if let Some(auto_switch) = effective_config.env_config.auto_switch {
            println!("  DEVFLOW_AUTO_SWITCH: {}", auto_switch);
        }
        if let Some(ref regex) = effective_config.env_config.branch_filter_regex {
            println!("  DEVFLOW_BRANCH_FILTER_REGEX: {}", regex);
        }
        if let Some(ref branches) = effective_config.env_config.disabled_branches {
            println!("  DEVFLOW_DISABLED_BRANCHES: {}", branches.join(","));
        }
        if let Some(current_disabled) = effective_config.env_config.current_branch_disabled {
            println!("  DEVFLOW_CURRENT_BRANCH_DISABLED: {}", current_disabled);
        }
        if let Some(ref host) = effective_config.env_config.database_host {
            println!("  DEVFLOW_DATABASE_HOST: {}", host);
        }
        if let Some(port) = effective_config.env_config.database_port {
            println!("  DEVFLOW_DATABASE_PORT: {}", port);
        }
        if let Some(ref user) = effective_config.env_config.database_user {
            println!("  DEVFLOW_DATABASE_USER: {}", user);
        }
        if effective_config.env_config.database_password.is_some() {
            println!("  DEVFLOW_DATABASE_PASSWORD: [hidden]");
        }
        if let Some(ref prefix) = effective_config.env_config.database_prefix {
            println!("  DEVFLOW_DATABASE_PREFIX: {}", prefix);
        }
    }

    println!();

    // Show local config overrides
    println!("📁 Local Config File Overrides:");
    if let Some(ref local_config) = effective_config.local_config {
        println!("  ✅ Local config file found (.devflow.local.yml)");
        if local_config.disabled.is_some()
            || local_config.disabled_branches.is_some()
            || local_config.database.is_some()
            || local_config.git.is_some()
            || local_config.behavior.is_some()
        {
            println!("  Local overrides present (see merged config below)");
        } else {
            println!("  No overrides in local config");
        }
    } else {
        println!("  (no local config file found)");
    }

    println!();

    // Show backend source
    println!("Backends:");
    if let Ok(state) = LocalStateManager::new() {
        // Try to find config path to look up state backends
        let config_path = Config::find_config_file().ok().flatten();
        let state_backends = config_path.as_ref().and_then(|p| state.get_backends(p));

        if let Some(ref backends) = state_backends {
            println!("  Source: local state (~/.config/devflow/local_state.yml)");
            for b in backends {
                let default_marker = if b.default { " (default)" } else { "" };
                println!("  - {} [{}]{}", b.name, b.backend_type, default_marker);
            }
        } else {
            let committed_backends = effective_config.config.resolve_backends();
            if committed_backends.is_empty() {
                println!("  (none configured)");
            } else {
                println!("  Source: committed config (.devflow.yml)");
                for b in &committed_backends {
                    let default_marker = if b.default { " (default)" } else { "" };
                    println!("  - {} [{}]{}", b.name, b.backend_type, default_marker);
                }
            }
        }
    }

    println!();

    // Show final merged configuration
    println!("Final Merged Configuration:");
    let merged_config = effective_config.get_merged_config();
    println!("{}", serde_yaml_ng::to_string(&merged_config)?);

    Ok(())
}

/// Handle the `devflow commit` command.
async fn handle_commit_command(
    message: Option<String>,
    ai: bool,
    edit: bool,
    dry_run: bool,
    json_output: bool,
) -> Result<()> {
    let vcs = vcs::detect_vcs_provider(".")?;

    // Check for staged changes
    if !vcs.has_staged_changes()? {
        if json_output {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({"error": "no staged changes"}))?
            );
        } else {
            println!("No staged changes to commit.");
            println!("Stage changes first, e.g.: git add <files>");
        }
        return Ok(());
    }

    // Determine the commit message
    let final_message = if let Some(msg) = message {
        // Explicit -m message — use as-is
        msg
    } else if ai {
        // AI-generated message
        generate_ai_commit_message(vcs.as_ref(), json_output).await?
    } else {
        // No --ai, no --message: open editor for manual message
        let initial = String::new();
        open_editor_for_message(&initial)?
    };

    // --edit: let user review/edit (even with -m or --ai)
    let final_message = if edit {
        open_editor_for_message(&final_message)?
    } else {
        final_message
    };

    if final_message.trim().is_empty() {
        anyhow::bail!("Aborting commit: empty commit message");
    }

    if dry_run {
        if json_output {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({"message": final_message}))?
            );
        } else {
            println!("Generated commit message:\n");
            println!("{}", final_message);
        }
        return Ok(());
    }

    // Perform the commit
    vcs.commit(&final_message)?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "committed": true,
                "message": final_message
            }))?
        );
    } else {
        println!("Committed: {}", first_line(&final_message));
    }

    Ok(())
}

/// Generate a commit message using the configured LLM.
#[cfg(feature = "llm")]
async fn generate_ai_commit_message(
    vcs: &dyn vcs::VcsProvider,
    _json_output: bool,
) -> Result<String> {
    use crate::llm;

    let config = llm::LlmConfig::from_env();
    if !config.is_configured() {
        anyhow::bail!(
            "LLM not configured. Set DEVFLOW_LLM_API_KEY or point DEVFLOW_LLM_API_URL to a local endpoint.\n\
             Example: export DEVFLOW_LLM_API_KEY=sk-...\n\
             For Ollama: export DEVFLOW_LLM_API_URL=http://localhost:11434/v1"
        );
    }

    let diff = vcs.staged_diff()?;
    let summary = vcs.staged_summary()?;

    eprintln!(
        "Generating commit message with {} ({})...",
        config.model, config.api_url
    );
    let message = llm::generate_commit_message(&diff, &summary).await?;

    Ok(message)
}

#[cfg(not(feature = "llm"))]
async fn generate_ai_commit_message(
    _vcs: &dyn vcs::VcsProvider,
    _json_output: bool,
) -> Result<String> {
    anyhow::bail!("LLM support not compiled in. Rebuild with the `llm` feature enabled.");
}

/// Open the user's editor to compose or edit a commit message.
fn open_editor_for_message(initial_content: &str) -> Result<String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());

    // Write initial content to a temp file
    let dir = std::env::temp_dir();
    let file_path = dir.join("devflow_commit_msg.txt");
    let content_with_help = if initial_content.is_empty() {
        "\n# Write your commit message above.\n# Lines starting with '#' will be ignored.\n# Empty message aborts the commit.\n".to_string()
    } else {
        format!(
            "{}\n\n# Edit the commit message above.\n# Lines starting with '#' will be ignored.\n# Empty message aborts the commit.\n",
            initial_content
        )
    };
    std::fs::write(&file_path, &content_with_help)?;

    // Open editor
    let status = std::process::Command::new(&editor)
        .arg(&file_path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .with_context(|| format!("Failed to open editor: {}", editor))?;

    if !status.success() {
        anyhow::bail!("Editor exited with non-zero status");
    }

    // Read back and strip comment lines
    let raw = std::fs::read_to_string(&file_path)?;
    let _ = std::fs::remove_file(&file_path);

    let message: String = raw
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    Ok(message)
}

/// Return the first line of a message (for display).
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

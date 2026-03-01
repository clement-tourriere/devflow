use std::path::PathBuf;

use crate::config::{Config, EffectiveConfig, GlobalConfig, WorktreeConfig};
use crate::services::{self, ServiceProvider};

use crate::docker;
use crate::hooks::{
    approval::ApprovalStore, HookContext, HookEngine, HookEntry, HookPhase, IndexMap,
    ServiceContext,
};
use crate::state::{DevflowBranch, LocalStateManager};
use crate::vcs;
use anyhow::{Context, Result};
use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    // ── Branch Management ──
    #[command(about = "List all branches (with service + worktree status)")]
    List,
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
    #[command(
        about = "Remove a branch, its worktree, and associated service branches",
        long_about = "Remove a branch, its worktree, and associated service branches.\n\nThis is a comprehensive cleanup command that removes:\n  - The Git branch\n  - The worktree directory (if any)\n  - All associated service branches (containers, cloud branches)\n\nUnlike 'devflow service delete' which only removes service branches, 'remove'\ncleans up everything related to the branch.\n\nExamples:\n  devflow remove feature-auth\n  devflow remove feature-auth --force\n  devflow remove feature-auth --keep-services  # Only remove worktree + git branch"
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
    #[command(about = "Clean up old service branches")]
    Cleanup {
        #[arg(long, help = "Maximum number of branches to keep")]
        max_count: Option<usize>,
    },

    // ── Services ──
    #[command(
        about = "Manage services (create, delete, start, stop, reset, ...)",
        long_about = "Manage service providers and their branches.\n\nService commands operate on the configured service providers (local Docker,\nNeon, DBLab, etc.) to create, delete, and manage branch-isolated environments.\n\nExamples:\n  devflow service add                       # Interactive wizard\n  devflow service add mydb --provider local # Add with explicit options\n  devflow service create feature-auth       # Create service branch\n  devflow service delete feature-auth       # Delete service branch\n  devflow service start feature-auth        # Start a stopped container\n  devflow service stop feature-auth         # Stop a running container\n  devflow service reset feature-auth        # Reset to parent state\n  devflow service connection feature-auth   # Show connection info\n  devflow service status                    # Show service status\n  devflow service list                      # List configured services\n  devflow service remove mydb               # Remove a service config\n  devflow service logs feature-auth         # Show container logs\n  devflow service seed main --from dump.sql # Seed from external source\n  devflow service destroy                   # Destroy all data"
    )]
    Service {
        #[command(subcommand)]
        action: ServiceCommands,
    },

    // ── Top-level aliases ──
    #[command(
        about = "Show connection info for a service branch (alias for 'service connection')",
        long_about = "Show connection info for a service branch.\n\nOutputs connection details in various formats for use in scripts and configuration.\nThis is an alias for 'devflow service connection'.\n\nExamples:\n  devflow connection feature-auth              # Connection URI\n  devflow connection feature-auth --format env  # Environment variables\n  devflow connection feature-auth --format json # JSON object"
    )]
    Connection {
        #[arg(help = "Name of the branch")]
        branch_name: String,
        #[arg(long, help = "Output format: uri, env, or json")]
        format: Option<String>,
    },
    #[command(about = "Show current project and service status")]
    Status,

    // ── VCS ──
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

    // ── Setup & Config ──
    #[command(
        about = "Initialize devflow configuration",
        long_about = "Initialize devflow configuration.\n\nWith no arguments, initializes the current directory.\nWith a path argument, creates the directory and initializes it.\n\nExamples:\n  devflow init                              # Initialize current directory\n  devflow init myapp                        # Create ./myapp/ and initialize it\n  devflow init --name myapp                 # Initialize current dir with explicit name\n  devflow init myapp --force                # Overwrite existing config in ./myapp/"
    )]
    Init {
        #[arg(help = "Directory to create and initialize (omit to use current directory)")]
        path: Option<String>,
        #[arg(long, help = "Project name (defaults to directory name)")]
        name: Option<String>,
        #[arg(long, help = "Force overwrite existing configuration")]
        force: bool,
    },
    #[command(
        about = "Tear down the entire devflow project",
        long_about = "Tear down the entire devflow project.\n\nThis is the inverse of 'devflow init'. It permanently destroys:\n  - All service data (containers, databases, branches)\n  - Git worktrees created by devflow\n  - VCS hooks installed by devflow\n  - Branch registry and local state\n  - Hook approvals\n  - Configuration files (.devflow.yml, .devflow.local.yml)\n\nThis is irreversible. Use --force to skip the confirmation prompt.\n\nExamples:\n  devflow destroy              # Interactive confirmation\n  devflow destroy --force      # Skip confirmation"
    )]
    Destroy {
        #[arg(long, help = "Skip confirmation prompt")]
        force: bool,
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
    #[command(about = "Run diagnostics and check system health")]
    Doctor,
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
        name = "shell-init",
        about = "Print shell integration script (eval \"$(devflow shell-init)\")",
        long_about = "Print shell integration script.\n\nThe shell wrapper enables automatic 'cd' into worktree directories after\n'devflow switch'. Without it, switch works but you must cd manually.\n\nAdd to your shell profile:\n  eval \"$(devflow shell-init)\"        # auto-detects shell\n  eval \"$(devflow shell-init bash)\"   # ~/.bashrc\n  eval \"$(devflow shell-init zsh)\"    # ~/.zshrc\n  devflow shell-init fish | source    # ~/.config/fish/config.fish\n\nThis creates a 'devflow' shell wrapper function."
    )]
    ShellInit {
        #[arg(help = "Shell type: bash, zsh, or fish (auto-detected from $SHELL if omitted)")]
        shell: Option<String>,
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
        about = "Show automation capabilities",
        long_about = "Show automation capabilities.

Returns a machine-readable summary of devflow's automation contract, including
JSON output behavior, non-interactive guarantees, and recommended command usage
for AI agents and CI pipelines.

Examples:
  devflow capabilities
  devflow --json capabilities"
    )]
    Capabilities,

    // ── Extensibility ──
    #[command(
        about = "Manage lifecycle hooks",
        long_about = "Manage lifecycle hooks.\n\nHooks are MiniJinja-templated commands that run at specific lifecycle phases\n(post-create, post-switch, pre-merge, etc.). Configure them in .devflow.yml\nunder the 'hooks' section.\n\nExamples:\n  devflow hook show                  # List all configured hooks\n  devflow hook show post-create      # Show hooks for a specific phase\n  devflow hook run post-create       # Run hooks for a phase manually\n  devflow hook approvals             # List approved hooks\n  devflow hook approvals clear       # Clear all approvals"
    )]
    Hook {
        #[command(subcommand)]
        action: HookCommands,
    },
    #[command(
        about = "Manage plugin services",
        long_about = "Manage plugin services.\n\nPlugins extend devflow with custom service providers via JSON-over-stdio protocol.\nAny executable that speaks the protocol can be a provider.\n\nExamples:\n  devflow plugin list                    # List configured plugin services\n  devflow plugin check my-plugin         # Verify a plugin works\n  devflow plugin init my-plugin --lang bash  # Print a plugin scaffold script"
    )]
    Plugin {
        #[command(subcommand)]
        action: PluginCommands,
    },

    // ── Hidden ──
    #[command(about = "Generate shell completions", hide = true)]
    Completions {
        #[arg(help = "Shell to generate completions for: bash, zsh, fish, elvish, powershell")]
        shell: clap_complete::Shell,
    },

    // ── TUI ──
    #[cfg(feature = "tui")]
    #[command(about = "Launch the interactive terminal UI dashboard")]
    Tui,
}

/// Subcommands for `devflow service`.
#[derive(Subcommand)]
pub enum ServiceCommands {
    #[command(
        about = "Create a new service branch",
        long_about = "Create a new service branch.\n\nCreates Docker containers and/or cloud branches for the specified branch name.\n\nExamples:\n  devflow service create feature-auth\n  devflow service create feature-auth --from develop"
    )]
    Create {
        #[arg(help = "Name of the branch to create")]
        branch_name: String,
        #[arg(long, help = "Parent branch to clone from")]
        from: Option<String>,
    },
    #[command(
        about = "Delete a service branch (keeps Git branch and worktree)",
        long_about = "Delete a service branch (keeps Git branch and worktree).\n\nRemoves service branches (containers, cloud branches) but preserves the Git branch\nand any worktree directory. Use 'devflow remove' to delete everything.\n\nExamples:\n  devflow service delete feature-auth"
    )]
    Delete {
        #[arg(help = "Name of the branch to delete")]
        branch_name: String,
    },
    #[command(about = "List configured services")]
    List,
    #[command(about = "Show service status")]
    Status,
    #[command(
        about = "Add a new service provider",
        long_about = "Add a new service provider to the project.\n\nConfigures a new service provider (local Docker, Neon, ClickHouse, etc.) and\nstores it in local state. When run without flags, an interactive wizard guides\nyou through service type, provider, and name selection.\n\nExamples:\n  devflow service add                              # Interactive wizard\n  devflow service add mydb                         # Interactive (name pre-filled)\n  devflow service add mydb --provider neon          # Add Neon cloud provider\n  devflow service add analytics --provider local --service-type clickhouse"
    )]
    Add {
        #[arg(help = "Service name (prompted if omitted)")]
        name: Option<String>,
        #[arg(
            long,
            help = "Provider type (local, postgres_template, neon, dblab, xata)"
        )]
        provider: Option<String>,
        #[arg(
            long,
            help = "Service type (postgres, clickhouse, mysql, generic, plugin)"
        )]
        service_type: Option<String>,
        #[arg(long, help = "Force overwrite existing service with same name")]
        force: bool,
        #[arg(
            long,
            help = "Seed main branch from source (PostgreSQL URL, file path, or s3:// URL)"
        )]
        from: Option<String>,
    },
    #[command(
        about = "Remove a service provider configuration",
        long_about = "Remove a service provider from local state.\n\nThis removes the service configuration but does not destroy any data.\nUse 'devflow service destroy' to remove all data first.\n\nExamples:\n  devflow service remove mydb"
    )]
    Remove {
        #[arg(help = "Service name to remove")]
        name: String,
    },
    #[command(about = "Start a stopped branch container (local provider)")]
    Start {
        #[arg(help = "Name of the branch to start")]
        branch_name: String,
    },
    #[command(about = "Stop a running branch container (local provider)")]
    Stop {
        #[arg(help = "Name of the branch to stop")]
        branch_name: String,
    },
    #[command(about = "Reset a branch to its parent state (local provider)")]
    Reset {
        #[arg(help = "Name of the branch to reset")]
        branch_name: String,
    },
    #[command(about = "Destroy all branches and data for a service (local provider)")]
    Destroy {
        #[arg(long, help = "Skip confirmation prompt")]
        force: bool,
    },
    #[command(
        about = "Show connection info for a service branch",
        long_about = "Show connection info for a service branch.\n\nOutputs connection details in various formats for use in scripts and configuration.\n\nExamples:\n  devflow service connection feature-auth              # Connection URI\n  devflow service connection feature-auth --format env  # Environment variables\n  devflow service connection feature-auth --format json # JSON object"
    )]
    Connection {
        #[arg(help = "Name of the branch")]
        branch_name: String,
        #[arg(long, help = "Output format: uri, env, or json")]
        format: Option<String>,
    },
    #[command(
        about = "Show container logs for a branch",
        long_about = "Show container logs for a branch.\n\nDisplays stdout/stderr from the Docker container backing a service branch.\nUseful for debugging startup failures, query errors, or crash loops.\n\nExamples:\n  devflow service logs main                    # Last 100 lines from main\n  devflow service logs feature/auth --tail 50  # Last 50 lines\n  devflow service logs main -s analytics       # Logs from a specific service"
    )]
    Logs {
        branch_name: String,
        #[arg(long, help = "Number of lines to show (default: 100)")]
        tail: Option<usize>,
    },
    #[command(
        about = "Seed a branch from an external source",
        long_about = "Seed a branch database from an external source.\n\nLoads data into an existing branch from a PostgreSQL URL, local dump file,\nor S3 URL. The branch must already exist.\n\nExamples:\n  devflow service seed main --from dump.sql                    # Seed from local file\n  devflow service seed feature/auth --from postgresql://...     # Seed from live database\n  devflow service seed main --from s3://bucket/path/dump.sql   # Seed from S3"
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
        #[arg(help = "Plugin service name (as defined in services config)")]
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
    non_interactive: bool,
    database_name: Option<&str>,
) -> Result<()> {
    // TUI command — launch immediately without loading service infrastructure
    #[cfg(feature = "tui")]
    if matches!(cmd, Commands::Tui) {
        return crate::tui::run().await;
    }

    // Commands that need service infrastructure (config loading, state injection)
    let uses_service = matches!(
        cmd,
        Commands::Service { .. }
            | Commands::List
            | Commands::Connection { .. }
            | Commands::Status
            | Commands::Cleanup { .. }
            | Commands::Switch { .. }
            | Commands::GitHook { .. }
            | Commands::WorktreeSetup
            | Commands::Remove { .. }
            | Commands::Merge { .. }
            | Commands::Doctor
    );

    // Check if command requires configuration file
    let requires_config = uses_service;

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
        // Service commands allow no config (will use local provider defaults)
        // This is fine — create_provider_default() handles auto-detection
    }

    // Get the merged configuration for normal operations
    let mut config = effective_config.get_merged_config();

    // Inject services from state (state services take precedence over committed)
    let local_state_for_services = if uses_service {
        LocalStateManager::new().ok()
    } else {
        None
    };
    if let Some(ref state_manager) = local_state_for_services {
        if let Some(ref path) = config_path {
            if let Some(state_services) = state_manager.get_services(path) {
                config.services = Some(state_services);
            }
        }
    }

    // Handle service-based commands
    if uses_service {
        // For GitHook, check if hooks are disabled early
        if matches!(cmd, Commands::GitHook { .. }) && effective_config.should_skip_hooks() {
            log::debug!("Git hooks are disabled via configuration");
            return Ok(());
        }
        // For doctor, run config/git pre-checks before service-specific checks
        if matches!(cmd, Commands::Doctor) && !json_output {
            run_doctor_pre_checks(&config, &config_path);
        }

        return match cmd {
            Commands::Connection {
                branch_name,
                format,
            } => {
                // Top-level alias: delegate to service connection
                let svc_cmd = ServiceCommands::Connection {
                    branch_name,
                    format,
                };
                handle_service_provider_command(
                    svc_cmd,
                    &mut config,
                    json_output,
                    non_interactive,
                    database_name,
                    &config_path,
                )
                .await
            }
            Commands::Status => {
                // Top-level status: show both VCS and service info
                handle_top_level_status(
                    &mut config,
                    json_output,
                    non_interactive,
                    database_name,
                    &config_path,
                )
                .await
            }
            Commands::Service { action } => {
                handle_service_dispatch(
                    action,
                    &mut config,
                    &effective_config,
                    json_output,
                    non_interactive,
                    database_name,
                    &config_path,
                )
                .await
            }
            // Branch management commands that need service context
            _ => {
                handle_branch_command(
                    cmd,
                    &mut config,
                    json_output,
                    non_interactive,
                    database_name,
                    &config_path,
                )
                .await
            }
        };
    }

    match cmd {
        Commands::Init { path, name, force } => {
            // If a path is given, create the directory and work inside it.
            if let Some(ref dir) = path {
                let target = std::path::PathBuf::from(dir);
                if target.exists() {
                    if !target.is_dir() {
                        anyhow::bail!("'{}' exists and is not a directory", target.display());
                    }
                } else {
                    std::fs::create_dir_all(&target).with_context(|| {
                        format!("Failed to create directory '{}'", target.display())
                    })?;
                    if !json_output {
                        println!("Created directory: {}", target.display());
                    }
                }
                std::env::set_current_dir(&target).with_context(|| {
                    format!("Failed to change into directory '{}'", target.display())
                })?;
            }

            let config_path = std::env::current_dir()?.join(".devflow.yml");

            if config_path.exists() && !force {
                anyhow::bail!(
                    "Configuration already exists at {}. Use --force to overwrite.",
                    config_path.display()
                );
            }

            // Resolve the name: explicit --name flag > directory basename from
            // path arg > current directory basename.
            let resolved_name = if let Some(n) = name {
                n
            } else if let Some(ref dir) = path {
                std::path::Path::new(dir)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "default".to_string())
            } else {
                std::env::current_dir()?
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "default".to_string())
            };

            let mut config = Config {
                name: Some(resolved_name.clone()),
                ..Config::default()
            };

            // ── VCS auto-init ──────────────────────────────────────────
            // If no VCS is present, initialize one automatically.
            let vcs_initialized = if vcs::detect_vcs_kind(".").is_none() {
                // Load global config for `default_vcs` preference.
                let global_cfg = GlobalConfig::load()?.unwrap_or_default();
                let preference = global_cfg.default_vcs;
                let interactive = !json_output && !non_interactive;

                match vcs::init_vcs_repository(".", preference, interactive) {
                    Ok(kind) => {
                        if !json_output {
                            println!("No VCS detected — initialized {} repository", kind);
                        }
                        Some(kind)
                    }
                    Err(e) => {
                        if !json_output {
                            println!("Warning: could not auto-initialize VCS: {e}");
                        }
                        None
                    }
                }
            } else {
                None
            };

            // Auto-detect main branch from VCS
            if let Ok(vcs) = vcs::detect_vcs_provider(".") {
                if let Ok(Some(detected_main)) = vcs.default_branch() {
                    config.git.main_branch = detected_main.clone();
                    if !json_output {
                        println!(
                            "Auto-detected main branch: {} ({})",
                            detected_main,
                            vcs.provider_name()
                        );
                    }
                } else if !json_output {
                    println!("Could not auto-detect main branch, using default: main");
                }
            }

            // Propose worktree configuration
            let enable_worktrees = if json_output || non_interactive {
                // Default to enabled in non-interactive / JSON mode
                true
            } else {
                println!();
                inquire::Confirm::new(
                    "Enable worktrees? (isolate each branch in its own directory)",
                )
                .with_default(true)
                .with_help_message(
                    "Recommended. Each branch gets its own working directory via git worktrees.",
                )
                .prompt()
                .unwrap_or(true)
            };

            // Detect CoW filesystem capability (used for both display and JSON output)
            let cow_cap = vcs::cow_worktree::detect_cow_capability(
                &std::env::current_dir().unwrap_or_default(),
            );
            let cow_label = match cow_cap {
                vcs::cow_worktree::CowCapability::Apfs => "apfs",
                vcs::cow_worktree::CowCapability::Reflink => "reflink",
                vcs::cow_worktree::CowCapability::None => "none",
            };

            if enable_worktrees {
                config.worktree = Some(WorktreeConfig::recommended_default());

                if !json_output {
                    match cow_cap {
                        vcs::cow_worktree::CowCapability::Apfs => {
                            println!(
                                "Filesystem: APFS detected — worktrees will use fast copy-on-write cloning"
                            );
                        }
                        vcs::cow_worktree::CowCapability::Reflink => {
                            println!(
                                "Filesystem: reflink support detected — worktrees will use fast copy-on-write cloning"
                            );
                        }
                        vcs::cow_worktree::CowCapability::None => {
                            println!(
                                "Filesystem: copy-on-write not available — worktrees will use standard file copy"
                            );
                        }
                    }
                }
            }

            // Don't write services to committed config — use `devflow service add`
            config.services = None;
            config.save_to_file(&config_path)?;

            // Register the main branch in the devflow branch registry.
            // `get_project_key` expects a *file* path (it calls `.parent()`),
            // so we pass `config_path` (e.g. `/project/.devflow.yml`), not
            // the directory itself.
            if let Ok(mut state_mgr) = LocalStateManager::new() {
                let main_branch = config.git.main_branch.clone();
                if state_mgr.get_branch(&config_path, &main_branch).is_none() {
                    let _ = state_mgr.register_branch(
                        &config_path,
                        DevflowBranch {
                            name: main_branch.clone(),
                            parent: None,
                            worktree_path: None,
                            created_at: chrono::Utc::now(),
                        },
                    );
                    log::debug!(
                        "Registered main branch '{}' in branch registry",
                        main_branch
                    );
                }
            }

            // Derive vcs_initialized label for JSON output
            let vcs_init_label = vcs_initialized.map(|k| k.to_string());

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "action": "init",
                        "name": resolved_name,
                        "config_path": config_path.display().to_string(),
                        "worktree_enabled": enable_worktrees,
                        "cow_capability": cow_label,
                        "vcs_initialized": vcs_init_label,
                    }))?
                );
            } else {
                println!(
                    "Initialized devflow configuration at: {}",
                    config_path.display()
                );

                let gitignore_path = std::env::current_dir()?.join(".gitignore");
                if gitignore_path.exists() {
                    let gitignore_content =
                        std::fs::read_to_string(&gitignore_path).unwrap_or_default();
                    if !gitignore_content.contains(".devflow.local.yml") {
                        println!("\nSuggestion: Add '.devflow.local.yml' to your .gitignore file:");
                        println!("   echo '.devflow.local.yml' >> .gitignore");
                    }
                }

                if enable_worktrees {
                    println!(
                        "\nWorktrees enabled. Each branch will get its own working directory."
                    );
                    println!("  Path template: ../{{repo}}.{{branch}}");
                    println!("  Files copied:  .env, .env.local");
                }

                println!("\nNext steps:");
                if enable_worktrees {
                    println!(
                        "  eval \"$(devflow shell-init)\"  Add to your shell profile for auto-cd into worktrees"
                    );
                }
                println!(
                    "  devflow service add          Add a service provider (interactive wizard)"
                );
                println!(
                    "  devflow install-hooks        Install Git hooks for automatic branching"
                );
                println!("  devflow doctor               Check system health and configuration");
            }
        }
        Commands::Destroy { force } => {
            handle_destroy_project(
                &mut config,
                &config_path,
                force,
                json_output,
                non_interactive,
            )
            .await?;
        }
        Commands::SetupZfs { pool_name, size } => {
            if !cfg!(target_os = "linux") {
                anyhow::bail!("setup-zfs is only supported on Linux");
            }

            #[cfg(not(feature = "service-local"))]
            {
                let _ = (pool_name, size);
                anyhow::bail!("Local provider not compiled. Rebuild with --features service-local");
            }

            #[cfg(feature = "service-local")]
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
            handle_hook_command(action, &config, json_output, non_interactive).await?;
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
        Commands::Capabilities => {
            let payload = serde_json::json!({
                "schema_version": "1.0",
                "json_mode": {
                    "stdout_single_json_document": true,
                    "diagnostics_on_stderr": true,
                },
                "non_interactive": {
                    "prompts_disabled": true,
                    "requires_force_for": ["destroy", "remove"],
                    "hook_unapproved_behavior": "error",
                },
                "orchestration": {
                    "partial_failure_exit_code": "non-zero",
                    "partial_failure_reported_in_json": true,
                },
                "recommended_for_agents": {
                    "global_flags": ["--json", "--non-interactive"],
                    "task_pattern": [
                        "create",
                        "connection",
                        "seed (optional)",
                        "work",
                        "reset (retry)",
                        "delete"
                    ],
                },
            });

            if json_output {
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                println!("Automation capabilities:");
                println!("- JSON mode: single JSON document on stdout (diagnostics on stderr)");
                println!("- Non-interactive: no prompts; --force required for destroy/remove");
                println!("- Multi-service partial failures: command exits non-zero by default");
                println!("- Recommended flags for agents: --json --non-interactive");
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
        Commands::Completions { shell } => {
            use clap::CommandFactory;
            let mut cmd = crate::Cli::command();
            clap_complete::generate(shell, &mut cmd, "devflow", &mut std::io::stdout());
        }
        _ => unreachable!(),
    }

    Ok(())
}

/// Check if ZFS auto-setup should be offered during init (Linux only).
/// Returns `Some(data_root)` if a pool was created or already exists,
/// so the caller can set it on the `LocalServiceConfig`.
#[cfg(feature = "service-local")]
async fn attempt_zfs_auto_setup(non_interactive: bool, quiet_output: bool) -> Option<String> {
    use crate::services::postgres::local::storage::zfs_setup::*;

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

async fn init_local_service_main(
    config: &Config,
    named_cfg: &crate::config::NamedServiceConfig,
    from: Option<&str>,
    quiet_output: bool,
) {
    match services::factory::create_provider_from_named_config(config, named_cfg).await {
        Ok(be) => {
            match be.create_branch("main", None).await {
                Ok(info) => {
                    if !quiet_output {
                        println!("Created main branch");
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
                            println!("Seeding main branch from: {}", source);
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
                        "Warning: could not create main branch for '{}': {}",
                        named_cfg.name, e
                    );
                    eprintln!("  You can create it later with: devflow create main");
                }
            }
        }
        Err(e) => {
            eprintln!(
                "Warning: could not initialize service '{}': {}",
                named_cfg.name, e
            );
            eprintln!("  You can create the main branch later with: devflow create main");
        }
    }
}

/// Print an enriched branch list as a tree, showing git branches, worktree paths, and service status.
///
/// Unifies information from the VCS provider, the service provider, and the
/// branch registry (for parent-child relationships) into a single tree view.
fn print_enriched_branch_list(
    service_branches: &[services::BranchInfo],
    config: &Config,
    config_path: &Option<PathBuf>,
) {
    use std::collections::{HashMap, HashSet};

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
    let service_names: HashSet<&str> = service_branches.iter().map(|b| b.name.as_str()).collect();

    // Build a worktree lookup: branch name -> path
    let wt_lookup: HashMap<String, PathBuf> = worktrees
        .iter()
        .filter_map(|wt| wt.branch.as_ref().map(|b| (b.clone(), wt.path.clone())))
        .collect();

    // Load branch registry + active branch from local state
    let mut registry: HashMap<String, Option<String>> = HashMap::new();
    let mut active_branch: Option<String> = None;
    if let Some(path) = config_path.as_ref() {
        if let Ok(state) = LocalStateManager::new() {
            active_branch = state.get_current_branch(path);
            registry = state
                .get_branches(path)
                .into_iter()
                .map(|b| (b.name, b.parent))
                .collect();
        }
    }

    // Collect all branch names (union of git branches + service branches)
    let mut all_names: Vec<String> = Vec::new();
    let mut seen = HashSet::new();

    for gb in &git_branches {
        if seen.insert(gb.name.clone()) {
            all_names.push(gb.name.clone());
        }
    }
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

    // Build parent map: child_name -> parent_name
    // Sources: 1) service-level parent, 2) registry parent (takes precedence)
    let mut parent_map: HashMap<&str, &str> = HashMap::new();

    for sb in service_branches {
        if let Some(ref parent) = sb.parent_branch {
            if seen.contains(parent.as_str()) {
                parent_map.insert(sb.name.as_str(), parent.as_str());
            }
        }
    }
    for name in &all_names {
        if let Some(Some(ref parent)) = registry.get(name.as_str()) {
            if seen.contains(parent.as_str()) {
                parent_map.insert(name.as_str(), parent.as_str());
            }
        }
    }

    // Build children map
    let mut children_map: HashMap<&str, Vec<&str>> = HashMap::new();
    for (child, parent) in &parent_map {
        children_map.entry(parent).or_default().push(child);
    }
    // Sort children alphabetically for deterministic output
    for kids in children_map.values_mut() {
        kids.sort();
    }

    // Find root nodes (no parent, or parent not in the known set)
    let mut roots: Vec<&str> = all_names
        .iter()
        .filter(|name| !parent_map.contains_key(name.as_str()))
        .map(|s| s.as_str())
        .collect();

    // Sort roots: default branch first, then current, then alphabetical
    roots.sort_by(|a, b| {
        let a_default = git_branches.iter().any(|gb| gb.name == *a && gb.is_default);
        let b_default = git_branches.iter().any(|gb| gb.name == *b && gb.is_default);
        if a_default != b_default {
            return b_default.cmp(&a_default);
        }
        let a_current = current_git.as_deref() == Some(*a);
        let b_current = current_git.as_deref() == Some(*b);
        if a_current != b_current {
            return b_current.cmp(&a_current);
        }
        a.cmp(b)
    });

    if let Some(active) = active_branch.as_deref() {
        let cwd = current_git.as_deref().unwrap_or("unknown");
        let cwd_normalized = current_git
            .as_deref()
            .map(|b| config.get_normalized_branch_name(b));
        let matches_active =
            current_git.as_deref() == Some(active) || cwd_normalized.as_deref() == Some(active);

        if !matches_active {
            let active_path = wt_lookup.get(active).or_else(|| {
                wt_lookup
                    .iter()
                    .find(|(name, _)| config.get_normalized_branch_name(name) == active)
                    .map(|(_, path)| path)
            });

            if let Some(path) = active_path {
                println!(
                    "Context: cwd branch='{}', devflow active branch='{}' ({})",
                    cwd,
                    active,
                    path.display()
                );
            } else {
                println!(
                    "Context: cwd branch='{}', devflow active branch='{}'",
                    cwd, active
                );
            }
            println!(
                "Tip: run `devflow switch {}` here, or `cd` into its worktree.",
                active
            );
        }
    }

    // Recursive tree printer
    fn print_node(
        name: &str,
        prefix: &str,
        connector: &str,
        children_map: &HashMap<&str, Vec<&str>>,
        current_git: &Option<String>,
        service_branches: &[services::BranchInfo],
        service_names: &HashSet<&str>,
        wt_lookup: &HashMap<String, PathBuf>,
        config: &Config,
        #[allow(unused_variables)] git_branches: &[crate::vcs::BranchInfo],
    ) {
        let is_current = current_git.as_deref() == Some(name);
        let marker = if is_current { "* " } else { "  " };

        let normalized = config.get_normalized_branch_name(name);
        let has_service =
            service_names.contains(normalized.as_str()) || service_names.contains(name);

        let service_state = service_branches
            .iter()
            .find(|b| b.name == normalized || b.name == name)
            .and_then(|b| b.state.as_deref());

        let wt_path = wt_lookup.get(name);

        let mut parts = Vec::new();
        if let Some(state) = service_state {
            parts.push(format!("service: {}", state));
        } else if has_service {
            parts.push("service: ok".to_string());
        }
        if let Some(path) = wt_path {
            parts.push(format!("worktree: {}", path.display()));
        }

        let suffix = if parts.is_empty() {
            String::new()
        } else {
            format!("  [{}]", parts.join(", "))
        };

        if connector.is_empty() {
            println!("{}{}{}", marker, name, suffix);
        } else {
            println!("{}{}{}{}", marker, connector, name, suffix);
        }

        if let Some(kids) = children_map.get(name) {
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
                print_node(
                    child,
                    &child_prefix,
                    &child_connector,
                    children_map,
                    current_git,
                    service_branches,
                    service_names,
                    wt_lookup,
                    config,
                    git_branches,
                );
            }
        }
    }

    for root in &roots {
        print_node(
            root,
            "  ",
            "",
            &children_map,
            &current_git,
            service_branches,
            &service_names,
            &wt_lookup,
            config,
            &git_branches,
        );
    }
}

/// Build enriched JSON for the list command, merging git + worktree + service info.
fn enrich_branch_list_json(
    service_branches: &[services::BranchInfo],
    config: &Config,
    config_path: &Option<PathBuf>,
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

    // Load branch registry + active branch from local state
    let mut registry: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    let mut active_branch: Option<String> = None;
    if let Some(path) = config_path.as_ref() {
        if let Ok(state) = LocalStateManager::new() {
            active_branch = state.get_current_branch(path);
            registry = state
                .get_branches(path)
                .into_iter()
                .map(|b| (b.name, b.parent))
                .collect();
        }
    }

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
        let is_active = active_branch.as_deref() == Some(name.as_str())
            || active_branch.as_deref() == Some(normalized.as_str());

        let mut entry = serde_json::json!({
            "name": name,
            "is_current": gb.map(|b| b.is_current).unwrap_or(false),
            "is_default": gb.map(|b| b.is_default).unwrap_or(false),
            "is_active": is_active,
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

        // Parent from registry (preferred) or service
        let parent = registry
            .get(name.as_str())
            .and_then(|p| p.clone())
            .or_else(|| sb.and_then(|s| s.parent_branch.clone()));
        if let Some(parent_name) = parent {
            entry["parent"] = serde_json::Value::String(parent_name);
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

/// Whether the command is being executed through `devflow shell-init` wrapper.
fn shell_integration_enabled() -> bool {
    std::env::var("DEVFLOW_SHELL_INTEGRATION")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn print_manual_cd_hint(target: &std::path::Path) {
    println!(
        "Shell integration not detected. Run: cd \"{}\"",
        target.display()
    );
    println!("Tip: add `eval \"$(devflow shell-init)\"` to your shell profile for auto-cd.");
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
    output="$(DEVFLOW_SHELL_INTEGRATION=1 command devflow "$@")"
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
    output="$(DEVFLOW_SHELL_INTEGRATION=1 command devflow "$@")"
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
    set -l output (env DEVFLOW_SHELL_INTEGRATION=1 command devflow $argv)
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

    // Build service map from all configured services
    let mut service = std::collections::HashMap::new();

    if let Ok(conn_infos) = services::factory::get_all_connection_info(config, branch_name).await {
        for (name, info) in conn_infos {
            let url = info.connection_string.clone().unwrap_or_else(|| {
                // No connection string provided by the provider — build a generic
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

    // Fallback: if no services populated the service map, use legacy config.database
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
    json_output: bool,
    non_interactive: bool,
) -> Result<()> {
    if let Some(ref hooks_config) = config.hooks {
        let working_dir =
            std::env::current_dir().map_err(|e| anyhow::anyhow!("Failed to get cwd: {}", e))?;
        let project_key = working_dir
            .canonicalize()
            .ok()
            .map(|p| p.to_string_lossy().to_string());
        let engine = if non_interactive || json_output {
            HookEngine::new_non_interactive(hooks_config.clone(), working_dir, project_key)
        } else {
            HookEngine::new(hooks_config.clone(), working_dir, project_key)
        }
        .with_quiet_output(json_output);

        let context = build_hook_context(config, branch_name).await;
        if json_output {
            engine.run_phase(&phase, &context).await?;
        } else {
            engine.run_phase_verbose(&phase, &context).await?;
        }
    }
    Ok(())
}

/// Handle `devflow hook` subcommands.
async fn handle_hook_command(
    action: HookCommands,
    config: &Config,
    json_output: bool,
    non_interactive: bool,
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
                non_interactive,
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
            if json_output {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({}))?);
            } else {
                println!("No hooks configured.");
                println!("  Add a 'hooks' section to .devflow.yml to configure lifecycle hooks.");
            }
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
        let mut filtered = serde_json::Map::new();
        for (phase, phase_hooks) in hooks.iter().filter(|(phase, _)| {
            phase_filter_parsed
                .as_ref()
                .is_none_or(|parsed_phase| *phase == parsed_phase)
        }) {
            filtered.insert(phase.to_string(), serde_json::to_value(phase_hooks)?);
        }
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
    _non_interactive: bool,
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
    let engine =
        HookEngine::new_no_approval(effective_config, working_dir).with_quiet_output(json_output);
    let result = if json_output {
        engine.run_phase(&phase, &context).await?
    } else {
        engine.run_phase_verbose(&phase, &context).await?
    };

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
            let mut approved = store.list_approved(&project_key);
            approved.sort_by(|a, b| a.command.cmp(&b.command));

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
            let services = config.resolve_services();
            let plugins: Vec<_> = services
                .iter()
                .filter(|b| b.service_type == "plugin")
                .collect();

            if plugins.is_empty() {
                if json_output {
                    println!("[]");
                } else {
                    println!("No plugin services configured.");
                    println!(
                        "Add a service with service_type: plugin in your .devflow.yml to get started."
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
                println!("Plugin services ({}):", plugins.len());
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
            let services = config.resolve_services();
            let named = services.iter().find(|b| b.name == name).ok_or_else(|| {
                anyhow::anyhow!(
                    "Service '{}' not found in configuration. Available services: {}",
                    name,
                    services
                        .iter()
                        .map(|b| b.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;

            if named.service_type != "plugin" {
                anyhow::bail!(
                    "Service '{}' is not a plugin (service_type: '{}')",
                    name,
                    named.service_type
                );
            }

            let plugin_cfg = named.plugin.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Service '{}' has type 'plugin' but no plugin config section",
                    name
                )
            })?;

            // Try to create the provider and invoke provider_name
            match crate::services::plugin::PluginProvider::new(&name, plugin_cfg) {
                Ok(provider) => {
                    // Try test_connection as a health check
                    match provider.test_connection().await {
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

                            anyhow::bail!("Plugin '{}' is unreachable: {}", name, e);
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

                    anyhow::bail!("Plugin '{}' could not initialize: {}", name, e);
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
#   services:
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
  provider_name)
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
  services:
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

    if method == "provider_name":
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

/// Handle branch management commands that need service context.
async fn handle_branch_command(
    cmd: Commands,
    config: &mut Config,
    json_output: bool,
    non_interactive: bool,
    database_name: Option<&str>,
    config_path: &Option<std::path::PathBuf>,
) -> Result<()> {
    match cmd {
        Commands::List => {
            // List: show combined VCS + service branch info
            let has_multiple_services = config.resolve_services().len() > 1;
            if database_name.is_none() && has_multiple_services {
                return handle_multi_service_aggregation(
                    ServiceAggregation::List,
                    config,
                    json_output,
                    &config_path,
                )
                .await;
            }

            // Try to resolve a service provider; if none is available we
            // still show VCS branches with an empty service branch list.
            let (provider_name, branches) =
                match services::factory::resolve_provider(config, database_name).await {
                    Ok(named) => {
                        let branches = named.provider.list_branches().await?;
                        (named.provider.provider_name().to_string(), branches)
                    }
                    Err(_) => {
                        // No service provider available — still show VCS branches.
                        ("none".to_string(), Vec::new())
                    }
                };

            if json_output {
                let enriched = enrich_branch_list_json(&branches, config, &config_path);
                println!("{}", serde_json::to_string_pretty(&enriched)?);
            } else {
                if provider_name == "none" {
                    println!("Branches (no service configured):");
                } else {
                    println!("Branches ({}):", provider_name);
                }
                print_enriched_branch_list(&branches, config, &config_path);
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
                        let auto_providers: Vec<serde_json::Value> = if !no_services {
                            config
                                .resolve_services()
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
                                "auto_branch_services": auto_providers,
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
                            let auto_providers = config
                                .resolve_services()
                                .into_iter()
                                .filter(|b| b.auto_branch)
                                .collect::<Vec<_>>();
                            if auto_providers.is_empty() {
                                println!(
                                    "  Would not switch any service branches (none configured)"
                                );
                            } else {
                                println!(
                                    "  Would create/switch service branches on {} service(s):",
                                    auto_providers.len()
                                );
                                for b in &auto_providers {
                                    println!("    - {} ({})", b.name, b.service_type);
                                }
                            }
                        }
                        if !no_verify && config.hooks.is_some() {
                            println!("  Would run post-switch hooks");
                        }
                        if let Some(ref cmd) = execute {
                            println!("  Would execute after switch: {}", cmd);
                        }
                    }
                } else {
                    anyhow::bail!("Dry run requires a branch name");
                }
            } else if template {
                handle_switch_to_main(
                    config,
                    config_path,
                    json_output,
                    no_services,
                    no_verify,
                    non_interactive,
                )
                .await?;
            } else if let Some(branch) = branch_name {
                if branch == config.git.main_branch {
                    handle_switch_to_main(
                        config,
                        config_path,
                        json_output,
                        no_services,
                        no_verify,
                        non_interactive,
                    )
                    .await?;
                } else {
                    handle_switch_command(
                        config,
                        &branch,
                        config_path,
                        create,
                        base.as_deref(),
                        no_services,
                        no_verify,
                        json_output,
                        non_interactive,
                    )
                    .await?;
                }
            } else if non_interactive {
                anyhow::bail!(
                    "No branch specified. Use 'devflow switch <branch>' in non-interactive mode."
                );
            } else {
                handle_interactive_switch(config, config_path).await?;
            }

            // Execute post-switch command if requested
            if let Some(ref cmd) = execute {
                if json_output {
                    eprintln!("Running post-switch command: {}", cmd);
                } else {
                    println!("Running post-switch command: {}", cmd);
                }
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
        Commands::Cleanup { max_count } => {
            let named = services::factory::resolve_provider(config, database_name).await?;
            let max = max_count.unwrap_or(config.behavior.max_branches.unwrap_or(10));
            let deleted = named.provider.cleanup_old_branches(max).await?;
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
        Commands::Doctor => {
            // Run pre-checks (VCS, config, hooks) unconditionally — they never fail
            if !json_output {
                run_doctor_pre_checks(config, &config_path);
            }
            let has_multiple_services = config.resolve_services().len() > 1;
            if database_name.is_none() && has_multiple_services {
                return handle_multi_service_aggregation(
                    ServiceAggregation::Doctor,
                    config,
                    json_output,
                    &config_path,
                )
                .await;
            }
            // Service-specific doctor report is optional
            match services::factory::resolve_provider(config, database_name).await {
                Ok(named) => {
                    let report = named.provider.doctor().await?;
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "general": {
                                    "config_path": config_path.as_ref().map(|p| p.display().to_string()),
                                },
                                "service": report,
                            }))?
                        );
                    } else {
                        println!("Service ({}):", named.provider.provider_name());
                        for check in &report.checks {
                            let icon = if check.available { "OK" } else { "FAIL" };
                            println!("  [{}] {}: {}", icon, check.name, check.detail);
                        }
                    }
                }
                Err(_) => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "general": {
                                    "config_path": config_path.as_ref().map(|p| p.display().to_string()),
                                },
                                "services": null,
                            }))?
                        );
                    } else {
                        println!("Services:");
                        println!("  [WARN] No service provider available (run 'devflow service add' to configure one)");
                    }
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
        _ => unreachable!(),
    }

    Ok(())
}

/// Dispatch `devflow service <action>` subcommands.
async fn handle_service_dispatch(
    action: ServiceCommands,
    config: &mut Config,
    _effective_config: &EffectiveConfig,
    json_output: bool,
    non_interactive: bool,
    database_name: Option<&str>,
    config_path: &Option<std::path::PathBuf>,
) -> Result<()> {
    match action {
        ServiceCommands::Add {
            name,
            provider,
            service_type,
            force,
            from,
        } => {
            let config_path_buf = config_path
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap().join(".devflow.yml"));

            // --- Interactive wizard when flags are missing ---

            // 1. Service type selection
            let service_type = if let Some(st) = service_type {
                st
            } else if non_interactive || json_output {
                crate::config::default_service_type()
            } else {
                use inquire::Select;
                let service_types = vec![
                    "postgres    — PostgreSQL database",
                    "clickhouse  — ClickHouse analytics database",
                    "mysql       — MySQL database",
                    "generic     — Generic Docker container",
                    "plugin      — External plugin",
                ];
                let selection = Select::new("What type of service?", service_types)
                    .with_help_message("Use arrow keys to navigate, Enter to select")
                    .prompt();
                match selection {
                    Ok(s) => s
                        .split_whitespace()
                        .next()
                        .unwrap_or("postgres")
                        .to_string(),
                    Err(
                        inquire::InquireError::OperationCanceled
                        | inquire::InquireError::OperationInterrupted,
                    ) => {
                        println!("Cancelled.");
                        return Ok(());
                    }
                    Err(e) => return Err(e.into()),
                }
            };

            // 2. Provider selection (choices depend on service type)
            let provider_type = if let Some(p) = provider {
                p
            } else if non_interactive || json_output {
                "local".to_string()
            } else {
                use inquire::Select;
                let provider_options: Vec<&str> = match service_type.as_str() {
                    "postgres" => vec![
                        "local               — Docker container on this machine",
                        "postgres_template    — Connect to existing PostgreSQL (template-based branching)",
                        "neon                 — Neon serverless Postgres (cloud)",
                        "dblab               — Database Lab Engine (clone-based branching)",
                        "xata                — Xata serverless database (cloud)",
                    ],
                    "clickhouse" => vec![
                        "local               — Docker container on this machine",
                    ],
                    "mysql" => vec![
                        "local               — Docker container on this machine",
                    ],
                    "generic" => vec![
                        "local               — Docker container on this machine",
                    ],
                    "plugin" => vec![
                        "local               — Managed by plugin",
                    ],
                    _ => vec![
                        "local               — Docker container on this machine",
                    ],
                };

                if provider_options.len() == 1 {
                    // Only one option, auto-select but inform the user
                    let only = provider_options[0]
                        .split_whitespace()
                        .next()
                        .unwrap_or("local")
                        .to_string();
                    println!("Provider: {}", only);
                    only
                } else {
                    let selection = Select::new("Which provider?", provider_options)
                        .with_help_message("Use arrow keys to navigate, Enter to select")
                        .prompt();
                    match selection {
                        Ok(s) => s.split_whitespace().next().unwrap_or("local").to_string(),
                        Err(
                            inquire::InquireError::OperationCanceled
                            | inquire::InquireError::OperationInterrupted,
                        ) => {
                            println!("Cancelled.");
                            return Ok(());
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
            };

            // 3. Service name
            let name = if let Some(n) = name {
                n
            } else if non_interactive || json_output {
                anyhow::bail!("Service name is required in non-interactive mode. Usage: devflow service add <name>");
            } else {
                use inquire::Text;
                let default_name = match service_type.as_str() {
                    "clickhouse" => "analytics",
                    "mysql" => "mysql",
                    "generic" => "app",
                    "plugin" => "plugin",
                    _ => "db",
                };
                let input = Text::new("Service name:")
                    .with_default(default_name)
                    .with_help_message("A short identifier for this service (e.g. db, analytics)")
                    .prompt();
                match input {
                    Ok(n) if n.trim().is_empty() => default_name.to_string(),
                    Ok(n) => n.trim().to_string(),
                    Err(
                        inquire::InquireError::OperationCanceled
                        | inquire::InquireError::OperationInterrupted,
                    ) => {
                        println!("Cancelled.");
                        return Ok(());
                    }
                    Err(e) => return Err(e.into()),
                }
            };

            let is_local = services::factory::ProviderType::is_local(&provider_type);
            let is_postgres_template = matches!(
                provider_type.as_str(),
                "postgres_template" | "postgres" | "postgresql"
            );

            // For postgres_template provider, look for Docker Compose files
            if is_postgres_template && !json_output {
                let compose_files = docker::find_docker_compose_files();
                if !compose_files.is_empty() {
                    println!("Found Docker Compose files: {}", compose_files.join(", "));

                    if let Ok(Some(postgres_config)) =
                        docker::parse_postgres_config_from_files(&compose_files)
                    {
                        let use_postgres_config = if non_interactive {
                            false
                        } else {
                            docker::prompt_user_for_config_usage(&postgres_config).unwrap_or(false)
                        };

                        if use_postgres_config {
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

            // Build named service config
            let named_cfg = crate::config::NamedServiceConfig {
                name: name.clone(),
                provider_type: provider_type.clone(),
                service_type: service_type.clone(),
                auto_branch: crate::config::default_auto_branch(),
                default: false,
                local: if is_local {
                    Some(crate::config::LocalServiceConfig {
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
                clickhouse: if service_type == "clickhouse" {
                    Some(crate::config::ClickHouseConfig {
                        image: "clickhouse/clickhouse-server:latest".to_string(),
                        port_range_start: None,
                        data_root: None,
                        user: "default".to_string(),
                        password: None,
                    })
                } else {
                    None
                },
                mysql: if service_type == "mysql" {
                    Some(crate::config::MySQLConfig {
                        image: "mysql:8".to_string(),
                        port_range_start: None,
                        data_root: None,
                        root_password: "dev".to_string(),
                        database: None,
                        user: None,
                        password: None,
                    })
                } else {
                    None
                },
                generic: None,
                plugin: None,
            };

            // Store service in local state
            let mut state = LocalStateManager::new()?;
            state.add_service(&config_path_buf, named_cfg.clone(), force)?;
            if !json_output {
                println!("Added service '{}' to local state", name);
            }

            // Create main branch for local providers
            if is_local {
                // Build a config with the service injected so the factory can find it
                let mut config_with_service = config.clone();
                if let Some(state_services) = state.get_services(&config_path_buf) {
                    config_with_service.services = Some(state_services);
                }

                // On Linux, offer ZFS auto-setup before creating the main branch
                #[cfg(feature = "service-local")]
                if cfg!(target_os = "linux") {
                    if let Some(data_root) =
                        attempt_zfs_auto_setup(non_interactive, json_output).await
                    {
                        let mut updated_cfg = named_cfg.clone();
                        if let Some(ref mut local) = updated_cfg.local {
                            local.data_root = Some(data_root);
                        }
                        if let Err(e) =
                            state.add_service(&config_path_buf, updated_cfg.clone(), true)
                        {
                            log::warn!(
                                "Failed to persist updated service config in local state: {}",
                                e
                            );
                        }
                        if let Some(state_services) = state.get_services(&config_path_buf) {
                            config_with_service.services = Some(state_services);
                        }
                        init_local_service_main(
                            &config_with_service,
                            &updated_cfg,
                            from.as_deref(),
                            json_output,
                        )
                        .await;
                    } else {
                        init_local_service_main(
                            &config_with_service,
                            &named_cfg,
                            from.as_deref(),
                            json_output,
                        )
                        .await;
                    }
                } else {
                    init_local_service_main(
                        &config_with_service,
                        &named_cfg,
                        from.as_deref(),
                        json_output,
                    )
                    .await;
                }
                #[cfg(not(feature = "service-local"))]
                {
                    init_local_service_main(
                        &config_with_service,
                        &named_cfg,
                        from.as_deref(),
                        json_output,
                    )
                    .await;
                }
            }

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "action": "add_service",
                        "name": name,
                        "provider_type": provider_type,
                    }))?
                );
            }
        }
        ServiceCommands::Remove { name } => {
            let config_path_buf = config_path
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap().join(".devflow.yml"));

            let mut state = LocalStateManager::new()?;
            state.remove_service(&config_path_buf, &name)?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "action": "remove_service",
                        "name": name,
                    }))?
                );
            } else {
                println!("Removed service '{}' from local state", name);
            }
        }
        ServiceCommands::List => {
            // List configured services (not branches)
            let has_multiple_services = config.resolve_services().len() > 1;
            if database_name.is_none() && has_multiple_services {
                return handle_multi_service_aggregation(
                    ServiceAggregation::List,
                    config,
                    json_output,
                    &config_path,
                )
                .await;
            }
            let named = services::factory::resolve_provider(config, database_name).await?;
            let branches = named.provider.list_branches().await?;
            if json_output {
                let enriched = enrich_branch_list_json(&branches, config, &config_path);
                println!("{}", serde_json::to_string_pretty(&enriched)?);
            } else {
                println!("Branches ({}):", named.provider.provider_name());
                print_enriched_branch_list(&branches, config, &config_path);
            }
        }
        ServiceCommands::Status => {
            let has_multiple_services = config.resolve_services().len() > 1;
            if database_name.is_none() && has_multiple_services {
                return handle_multi_service_aggregation(
                    ServiceAggregation::Status,
                    config,
                    json_output,
                    &config_path,
                )
                .await;
            }
            let named = services::factory::resolve_provider(config, database_name).await?;
            let provider = named.provider;
            let branches = provider.list_branches().await.unwrap_or_default();
            let running = branches
                .iter()
                .filter(|b| b.state.as_deref() == Some("running"))
                .count();
            let stopped = branches
                .iter()
                .filter(|b| b.state.as_deref() == Some("stopped"))
                .count();
            let project_info = provider.project_info();

            if json_output {
                let mut status = serde_json::json!({
                    "provider": provider.provider_name(),
                    "total_branches": branches.len(),
                    "running": running,
                    "stopped": stopped,
                    "supports_lifecycle": provider.supports_lifecycle(),
                });
                if let Some(ref info) = project_info {
                    status["project"] = serde_json::Value::String(info.name.clone());
                    if let Some(ref storage) = info.storage_driver {
                        status["storage"] = serde_json::Value::String(storage.clone());
                    }
                    if let Some(ref image) = info.image {
                        status["image"] = serde_json::Value::String(image.clone());
                    }
                }
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("Provider: {}", provider.provider_name());
                if let Some(ref info) = project_info {
                    println!("Project: {}", info.name);
                    if let Some(ref storage) = info.storage_driver {
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
                if provider.supports_lifecycle() {
                    println!("Lifecycle: supported (start/stop/reset)");
                }
            }
        }
        // Provider operations: delegate to handle_service_provider_command
        other => {
            return handle_service_provider_command(
                other,
                config,
                json_output,
                non_interactive,
                database_name,
                config_path,
            )
            .await;
        }
    }

    Ok(())
}

/// Handle service provider operations (create, delete, start, stop, reset, destroy, connection, logs, seed).
async fn handle_service_provider_command(
    cmd: ServiceCommands,
    config: &mut Config,
    json_output: bool,
    non_interactive: bool,
    database_name: Option<&str>,
    config_path: &Option<std::path::PathBuf>,
) -> Result<()> {
    // Orchestratable mutation commands: Create and Delete operate on all auto_branch services
    let is_orchestratable_mutation = matches!(
        cmd,
        ServiceCommands::Create { .. } | ServiceCommands::Delete { .. }
    );
    let has_multiple_services = config.resolve_services().len() > 1;

    // For Create/Delete: if there are multiple services and no --service flag,
    // use orchestration to operate on all auto_branch services atomically.
    if is_orchestratable_mutation && database_name.is_none() && has_multiple_services {
        return handle_orchestrated_mutation(cmd, config, json_output, non_interactive).await;
    }

    let named = services::factory::resolve_provider(config, database_name).await?;
    let provider = named.provider;
    let resolved_name = named.name;

    // For non-orchestratable mutation commands with multiple services and no --service, print a note
    if !is_orchestratable_mutation && database_name.is_none() && has_multiple_services {
        eprintln!(
            "note: using default service '{}'. Use --service to target a specific one.",
            resolved_name
        );
    }

    match cmd {
        ServiceCommands::Create { branch_name, from } => {
            // Single-service path (explicit --service or single service)
            let info = provider
                .create_branch(&branch_name, from.as_deref())
                .await?;

            // Execute hooks
            run_hooks(
                config,
                &branch_name,
                HookPhase::PostServiceCreate,
                json_output,
                non_interactive,
            )
            .await?;

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
                if let Ok(conn) = provider.get_connection_info(&branch_name).await {
                    if let Some(ref uri) = conn.connection_string {
                        println!("  Connection: {}", uri);
                    }
                }
            }
        }
        ServiceCommands::Delete { branch_name } => {
            // Single-service path (explicit --service or single service)
            provider.delete_branch(&branch_name).await?;
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
        ServiceCommands::Start { branch_name } => {
            if !provider.supports_lifecycle() {
                anyhow::bail!(
                    "Service '{}' does not support start/stop lifecycle",
                    provider.provider_name()
                );
            }
            provider.start_branch(&branch_name).await?;
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
        ServiceCommands::Stop { branch_name } => {
            if !provider.supports_lifecycle() {
                anyhow::bail!(
                    "Service '{}' does not support start/stop lifecycle",
                    provider.provider_name()
                );
            }
            provider.stop_branch(&branch_name).await?;
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
        ServiceCommands::Reset { branch_name } => {
            if !provider.supports_lifecycle() {
                anyhow::bail!(
                    "Service '{}' does not support reset",
                    provider.provider_name()
                );
            }
            provider.reset_branch(&branch_name).await?;
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
        ServiceCommands::Connection {
            branch_name,
            format,
        } => {
            let conn = provider.get_connection_info(&branch_name).await?;
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
        ServiceCommands::Destroy { force } => {
            if !provider.supports_destroy() {
                anyhow::bail!(
                    "Service '{}' does not support destroy. This command is only available for the local (Docker + CoW) provider.",
                    provider.provider_name()
                );
            }

            let preview = provider.destroy_preview().await?;
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
                            "No project found for service '{}'. Nothing to destroy.",
                            resolved_name
                        );
                    }
                    return Ok(());
                }
            };

            if !force {
                if json_output || non_interactive {
                    anyhow::bail!(
                        "Use --force to confirm destroy in non-interactive or JSON output mode"
                    );
                }

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

            let destroyed = provider.destroy_project().await?;

            // Remove the service entry from local state
            if let Some(ref path) = config_path {
                if let Ok(mut state) = LocalStateManager::new() {
                    if let Err(e) = state.remove_service(path, &resolved_name) {
                        log::warn!(
                            "Failed to remove service '{}' from local state: {}",
                            resolved_name,
                            e
                        );
                    }
                }
            }

            // Also remove from committed config for backward compat (legacy configs)
            config.remove_service(&resolved_name);
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
        ServiceCommands::Logs { branch_name, tail } => {
            let output = provider.logs(&branch_name, tail).await?;
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
        ServiceCommands::Seed { branch_name, from } => {
            if !json_output {
                println!("Seeding branch '{}' from '{}'...", branch_name, from);
            }
            provider.seed_from_source(&branch_name, &from).await?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "seeded": branch_name,
                        "from": from,
                    }))?
                );
            } else {
                println!("Seed complete.");
            }
        }
        // Add, Remove, List, Status are handled by handle_service_dispatch
        _ => unreachable!(),
    }

    Ok(())
}

/// Show top-level status: VCS info + service status.
async fn handle_top_level_status(
    config: &mut Config,
    json_output: bool,
    _non_interactive: bool,
    database_name: Option<&str>,
    config_path: &Option<std::path::PathBuf>,
) -> Result<()> {
    // Show VCS info
    let vcs_info = vcs::detect_vcs_provider(".").ok().and_then(|vcs| {
        let branch = vcs.current_branch().ok()?;
        Some(serde_json::json!({
            "provider": vcs.provider_name(),
            "branch": branch,
        }))
    });

    let active_branch = config_path.as_ref().and_then(|path| {
        LocalStateManager::new()
            .ok()
            .and_then(|state| state.get_current_branch(path))
    });
    let active_differs_from_cwd = |cwd: &str| {
        if let Some(active) = active_branch.as_deref() {
            let normalized_cwd = config.get_normalized_branch_name(cwd);
            active != cwd && active != normalized_cwd
        } else {
            false
        }
    };

    // Show service info — services are optional; show VCS/project info even without them
    let has_multiple_services = config.resolve_services().len() > 1;
    if database_name.is_none() && has_multiple_services {
        let all_providers = services::factory::create_all_providers(config).await?;
        if json_output {
            let mut services_map = serde_json::Map::new();
            for named in &all_providers {
                let branches = named.provider.list_branches().await.unwrap_or_default();
                let running = branches
                    .iter()
                    .filter(|b| b.state.as_deref() == Some("running"))
                    .count();
                let stopped = branches
                    .iter()
                    .filter(|b| b.state.as_deref() == Some("stopped"))
                    .count();
                services_map.insert(
                    named.name.clone(),
                    serde_json::json!({
                        "provider": named.provider.provider_name(),
                        "total_branches": branches.len(),
                        "running": running,
                        "stopped": stopped,
                    }),
                );
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "vcs": vcs_info,
                    "devflow_active_branch": active_branch,
                    "services": services_map,
                }))?
            );
        } else {
            if let Some(ref info) = vcs_info {
                println!(
                    "VCS: {} (branch: {})",
                    info["provider"].as_str().unwrap_or("unknown"),
                    info["branch"].as_str().unwrap_or("unknown")
                );
                if let Some(active) = active_branch.as_deref() {
                    let cwd = info["branch"].as_str().unwrap_or("unknown");
                    if active_differs_from_cwd(cwd) {
                        println!("Devflow active branch: {}", active);
                    }
                }
                println!();
            }
            for named in &all_providers {
                let branches = named.provider.list_branches().await.unwrap_or_default();
                let running = branches
                    .iter()
                    .filter(|b| b.state.as_deref() == Some("running"))
                    .count();
                let stopped = branches
                    .iter()
                    .filter(|b| b.state.as_deref() == Some("stopped"))
                    .count();
                println!("[{}] ({}):", named.name, named.provider.provider_name());
                println!(
                    "  Branches: {} total ({} running, {} stopped)",
                    branches.len(),
                    running,
                    stopped
                );
            }
        }
    } else {
        // Single service or no services — try to resolve, fall back gracefully
        match services::factory::resolve_provider(config, database_name).await {
            Ok(named) => {
                let branches = named.provider.list_branches().await.unwrap_or_default();
                let running = branches
                    .iter()
                    .filter(|b| b.state.as_deref() == Some("running"))
                    .count();
                let stopped = branches
                    .iter()
                    .filter(|b| b.state.as_deref() == Some("stopped"))
                    .count();

                if json_output {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "vcs": vcs_info,
                            "devflow_active_branch": active_branch,
                            "service": {
                                "name": named.name,
                                "provider": named.provider.provider_name(),
                                "total_branches": branches.len(),
                                "running": running,
                                "stopped": stopped,
                            },
                        }))?
                    );
                } else {
                    if let Some(ref info) = vcs_info {
                        println!(
                            "VCS: {} (branch: {})",
                            info["provider"].as_str().unwrap_or("unknown"),
                            info["branch"].as_str().unwrap_or("unknown")
                        );
                        if let Some(active) = active_branch.as_deref() {
                            let cwd = info["branch"].as_str().unwrap_or("unknown");
                            if active_differs_from_cwd(cwd) {
                                println!("Devflow active branch: {}", active);
                            }
                        }
                        println!();
                    } else if let Some(active) = active_branch.as_deref() {
                        println!("Devflow active branch: {}", active);
                        println!();
                    }
                    println!(
                        "Service: {} ({})",
                        named.name,
                        named.provider.provider_name()
                    );
                    println!(
                        "  Branches: {} total ({} running, {} stopped)",
                        branches.len(),
                        running,
                        stopped
                    );
                }
            }
            Err(_) => {
                // No service provider available — show VCS info only
                if json_output {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "vcs": vcs_info,
                            "devflow_active_branch": active_branch,
                            "services": null,
                        }))?
                    );
                } else {
                    if let Some(ref info) = vcs_info {
                        println!(
                            "VCS: {} (branch: {})",
                            info["provider"].as_str().unwrap_or("unknown"),
                            info["branch"].as_str().unwrap_or("unknown")
                        );
                        if let Some(active) = active_branch.as_deref() {
                            let cwd = info["branch"].as_str().unwrap_or("unknown");
                            if active_differs_from_cwd(cwd) {
                                println!("Devflow active branch: {}", active);
                            }
                        }
                        println!();
                    } else if let Some(active) = active_branch.as_deref() {
                        println!("Devflow active branch: {}", active);
                        println!();
                    }
                    println!(
                        "Services: none configured (run 'devflow service add' to configure one)"
                    );
                }
            }
        }
    }

    Ok(())
}

/// Internal enum for multi-service aggregation dispatch.
enum ServiceAggregation {
    List,
    Status,
    Doctor,
}

/// Handle aggregation commands (List, Status, Doctor) across all services.
async fn handle_multi_service_aggregation(
    aggregation: ServiceAggregation,
    config: &Config,
    json_output: bool,
    config_path: &Option<PathBuf>,
) -> Result<()> {
    let all_providers = match services::factory::create_all_providers(config).await {
        Ok(providers) => providers,
        Err(e) => {
            // Service providers unavailable — degrade gracefully
            log::warn!("Failed to create service providers: {}", e);
            match aggregation {
                ServiceAggregation::List => {
                    // Show branch registry info without service data
                    if json_output {
                        let enriched = enrich_branch_list_json(&[], config, config_path);
                        println!("{}", serde_json::to_string_pretty(&enriched)?);
                    } else {
                        println!("Branches (no service providers available):");
                        print_enriched_branch_list(&[], config, config_path);
                    }
                }
                ServiceAggregation::Status => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "error": format!("Failed to create service providers: {}", e),
                                "services": null,
                            }))?
                        );
                    } else {
                        println!("Services: failed to initialize providers ({})", e);
                    }
                }
                ServiceAggregation::Doctor => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "error": format!("Failed to create service providers: {}", e),
                                "services": null,
                            }))?
                        );
                    } else {
                        println!("Services:");
                        println!("  [FAIL] Could not initialize providers: {}", e);
                    }
                }
            }
            return Ok(());
        }
    };

    match aggregation {
        ServiceAggregation::List => {
            // Gather all service branches from all services
            let mut all_service_branches: Vec<services::BranchInfo> = Vec::new();
            if json_output {
                let mut map = serde_json::Map::new();
                for named in &all_providers {
                    let branches = named.provider.list_branches().await.unwrap_or_default();
                    map.insert(
                        named.name.clone(),
                        enrich_branch_list_json(&branches, config, &config_path),
                    );
                }
                println!("{}", serde_json::to_string_pretty(&map)?);
            } else {
                for named in &all_providers {
                    let branches = named.provider.list_branches().await.unwrap_or_default();
                    all_service_branches.extend(branches);
                    println!("[{}] ({}):", named.name, named.provider.provider_name());
                }
                print_enriched_branch_list(&all_service_branches, config, &config_path);
                println!();
            }
        }
        ServiceAggregation::Status => {
            if json_output {
                let mut map = serde_json::Map::new();
                for named in &all_providers {
                    let branches = named.provider.list_branches().await.unwrap_or_default();
                    let running = branches
                        .iter()
                        .filter(|b| b.state.as_deref() == Some("running"))
                        .count();
                    let stopped = branches
                        .iter()
                        .filter(|b| b.state.as_deref() == Some("stopped"))
                        .count();
                    let project_info = named.provider.project_info();

                    let mut status = serde_json::json!({
                        "provider": named.provider.provider_name(),
                        "total_branches": branches.len(),
                        "running": running,
                        "stopped": stopped,
                        "supports_lifecycle": named.provider.supports_lifecycle(),
                    });
                    if let Some(ref info) = project_info {
                        status["project"] = serde_json::Value::String(info.name.clone());
                        if let Some(ref storage) = info.storage_driver {
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
                for named in &all_providers {
                    let branches = named.provider.list_branches().await.unwrap_or_default();
                    let running = branches
                        .iter()
                        .filter(|b| b.state.as_deref() == Some("running"))
                        .count();
                    let stopped = branches
                        .iter()
                        .filter(|b| b.state.as_deref() == Some("stopped"))
                        .count();
                    let project_info = named.provider.project_info();

                    println!("[{}] ({}):", named.name, named.provider.provider_name());
                    if let Some(ref info) = project_info {
                        println!("  Project: {}", info.name);
                        if let Some(ref storage) = info.storage_driver {
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
                    if named.provider.supports_lifecycle() {
                        println!("  Lifecycle: supported (start/stop/reset)");
                    }
                    println!();
                }
            }
        }
        ServiceAggregation::Doctor => {
            if json_output {
                let mut map = serde_json::Map::new();
                for named in &all_providers {
                    let report = named.provider.doctor().await?;
                    map.insert(named.name.clone(), serde_json::to_value(&report)?);
                }
                println!("{}", serde_json::to_string_pretty(&map)?);
            } else {
                for named in &all_providers {
                    let report = named.provider.doctor().await?;
                    println!(
                        "[{}] Doctor report ({}):",
                        named.name,
                        named.provider.provider_name()
                    );
                    for check in &report.checks {
                        let icon = if check.available { "OK" } else { "FAIL" };
                        println!("  [{}] {}: {}", icon, check.name, check.detail);
                    }
                    println!();
                }
            }
        }
    }

    Ok(())
}

/// Handle Create/Delete across all auto-branch services when no specific --service is given.
async fn handle_orchestrated_mutation(
    cmd: ServiceCommands,
    config: &Config,
    json_output: bool,
    non_interactive: bool,
) -> Result<()> {
    match cmd {
        ServiceCommands::Create { branch_name, from } => {
            let results =
                services::factory::orchestrate_create(config, &branch_name, from.as_deref())
                    .await?;
            let success_count = results.iter().filter(|r| r.success).count();
            let fail_count = results.iter().filter(|r| !r.success).count();
            let mut json_payload: Option<serde_json::Value> = None;

            if json_output {
                let json_results: Vec<_> = results
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
                json_payload = Some(serde_json::json!({
                    "operation": "create",
                    "branch": branch_name,
                    "ok": fail_count == 0,
                    "succeeded": success_count,
                    "failed": fail_count,
                    "results": json_results,
                }));
            } else {
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

            if fail_count > 0 {
                if let Some(payload) = json_payload.take() {
                    println!("{}", serde_json::to_string_pretty(&payload)?);
                }
                anyhow::bail!(
                    "Failed to create branch '{}' on {}/{} service(s)",
                    branch_name,
                    fail_count,
                    results.len()
                );
            }

            // Run hooks after all services are created
            run_hooks(
                config,
                &branch_name,
                HookPhase::PostServiceCreate,
                json_output,
                non_interactive,
            )
            .await?;

            if let Some(payload) = json_payload {
                println!("{}", serde_json::to_string_pretty(&payload)?);
            }
        }
        ServiceCommands::Delete { branch_name } => {
            let results = services::factory::orchestrate_delete(config, &branch_name).await?;
            let success_count = results.iter().filter(|r| r.success).count();
            let fail_count = results.iter().filter(|r| !r.success).count();

            if json_output {
                let json_results: Vec<_> = results
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
                        "operation": "delete",
                        "branch": branch_name,
                        "ok": fail_count == 0,
                        "succeeded": success_count,
                        "failed": fail_count,
                        "results": json_results,
                    }))?
                );
            } else {
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

            if fail_count > 0 {
                anyhow::bail!(
                    "Failed to delete branch '{}' on {}/{} service(s)",
                    branch_name,
                    fail_count,
                    results.len()
                );
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
                handle_switch_to_main(config, config_path, false, false, false, true).await?;
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
                        true,  // non_interactive
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
    let mut branch_names = std::collections::BTreeSet::new();

    // 1) VCS branches (authoritative source)
    if let Ok(vcs_repo) = vcs::detect_vcs_provider(".") {
        if let Ok(vcs_branches) = vcs_repo.list_branches() {
            for branch in vcs_branches {
                branch_names.insert(branch.name);
            }
        }
    }

    // 2) Devflow branch registry + active branch from local state
    let mut active_branch: Option<String> = None;
    if let Some(path) = config_path.as_ref() {
        if let Ok(state) = LocalStateManager::new() {
            active_branch = state.get_current_branch(path);
            for branch in state.get_branches(path) {
                branch_names.insert(branch.name);
            }
        }
    }

    // 3) Service branches (best effort)
    if !config.resolve_services().is_empty() {
        if let Ok(providers) = services::factory::create_all_providers(config).await {
            for named in providers {
                if let Ok(service_branches) = named.provider.list_branches().await {
                    for branch in service_branches {
                        branch_names.insert(branch.name);
                    }
                }
            }
        }
    }

    // Always include configured main branch
    branch_names.insert(config.git.main_branch.clone());

    // Detect current cwd branch from VCS
    let current_git = vcs::detect_vcs_provider(".")
        .ok()
        .and_then(|r| r.current_branch().ok().flatten());

    // Create branch items with display info
    let mut branch_items: Vec<BranchItem> = branch_names
        .iter()
        .map(|branch| {
            let is_cwd = current_git.as_deref() == Some(branch.as_str());
            let is_active = active_branch.as_deref() == Some(branch.as_str());

            BranchItem {
                name: branch.clone(),
                display_name: branch.clone(),
                is_cwd,
                is_active,
            }
        })
        .collect();

    // Add a "Create new branch" option at the end
    branch_items.push(BranchItem {
        name: "__create_new__".to_string(),
        display_name: "+ Create new branch".to_string(),
        is_cwd: false,
        is_active: false,
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
                    false, // non_interactive
                )
                .await?;
            } else if selected_branch == config.git.main_branch {
                handle_switch_to_main(config, config_path, false, false, false, false).await?;
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
                    false, // non_interactive
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
    is_cwd: bool,
    is_active: bool,
}

fn run_interactive_selector(items: Vec<BranchItem>) -> Result<String, inquire::InquireError> {
    use inquire::Select;

    if items.is_empty() {
        return Err(inquire::InquireError::InvalidConfiguration(
            "No branches available".to_string(),
        ));
    }

    // Create display options with active/cwd markers.
    // active = devflow's persisted target branch, cwd = branch in current directory.
    let options: Vec<String> = items
        .iter()
        .map(|item| {
            if item.is_active && item.is_cwd {
                format!("{} *", item.display_name)
            } else if item.is_active {
                format!("{} (active)", item.display_name)
            } else if item.is_cwd {
                format!("{} (cwd)", item.display_name)
            } else {
                item.display_name.clone()
            }
        })
        .collect();

    // Prefer active branch as default; fall back to cwd branch.
    let default = items
        .iter()
        .position(|item| item.is_active)
        .or_else(|| items.iter().position(|item| item.is_cwd));

    let mut select = Select::new("Select a branch to switch to:", options.clone())
        .with_help_message(
        "Use arrow keys to navigate, type to filter, Enter to select, Esc to cancel (*=active+cwd)",
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
    non_interactive: bool,
) -> Result<()> {
    let worktree_enabled = config.worktree.as_ref().is_some_and(|wt| wt.enabled);
    let shell_integration = shell_integration_enabled();
    let mut worktree_path: Option<String> = None;
    let mut worktree_created = false;
    let mut branch_created = false;
    let mut json_summary: Option<serde_json::Value> = None;

    // Capture current branch BEFORE any branch creation/checkout so we can
    // use it as the default parent when --base is not specified.
    let current_branch_before_switch: Option<String> = vcs::detect_vcs_provider(".")
        .ok()
        .and_then(|repo| repo.current_branch().ok().flatten());

    // ── Worktree mode ──────────────────────────────────────────────────
    if worktree_enabled {
        let vcs_repo = vcs::detect_vcs_provider(".").context("Failed to open VCS repository")?;

        // Check if a worktree already exists for this branch
        let existing_path = vcs_repo.worktree_path(branch_name)?;

        if let Some(wt_path) = existing_path {
            if !json_output {
                println!("Switching to existing worktree: {}", wt_path.display());
                // Print the path so shell integration can cd to it
                println!("DEVFLOW_CD={}", wt_path.display());
                if !shell_integration {
                    print_manual_cd_hint(&wt_path);
                }
            }
            worktree_path = Some(wt_path.display().to_string());
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
            let branch_exists = vcs_repo.branch_exists(branch_name)?;
            if create || !branch_exists {
                if !json_output {
                    println!(
                        "Creating branch '{}' (base: {})",
                        branch_name,
                        base.unwrap_or("HEAD")
                    );
                }
                vcs_repo.create_branch(branch_name, base).with_context(|| {
                    format!(
                        "Failed to create branch '{}' before worktree creation",
                        branch_name
                    )
                })?;
                branch_created = !branch_exists;
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
                            if count > 0 && !json_output {
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
            if !json_output {
                println!("DEVFLOW_CD={}", wt_path.display());
                if !shell_integration {
                    print_manual_cd_hint(&wt_path);
                }
            }
        }
    } else {
        // ── Classic mode (no worktrees) ────────────────────────────────
        let vcs_repo = vcs::detect_vcs_provider(".").context("Failed to open VCS repository")?;
        let branch_exists = vcs_repo.branch_exists(branch_name)?;
        if create || !branch_exists {
            if !json_output {
                println!(
                    "Creating branch '{}' (base: {})",
                    branch_name,
                    base.unwrap_or("HEAD")
                );
            }
            vcs_repo.create_branch(branch_name, base)?;
            branch_created = !branch_exists;
        }
        // Switch the working directory to the target branch
        if !json_output {
            println!("Checking out branch: {}", branch_name);
        }
        vcs_repo.checkout_branch(branch_name)?;
    }

    // ── Branch registration (unconditional — independent of services) ──
    let normalized_branch = config.get_normalized_branch_name(branch_name);

    if let Some(ref path) = config_path {
        if let Ok(mut state) = LocalStateManager::new() {
            if let Err(e) = state.set_current_branch(path, Some(normalized_branch.clone())) {
                log::warn!("Failed to persist current branch in local state: {}", e);
            }

            let existing = state.get_branch(path, &normalized_branch);

            let parent_branch = existing
                .as_ref()
                .and_then(|b| b.parent.clone())
                .or_else(|| {
                    if branch_created {
                        base.map(|b| config.get_normalized_branch_name(b))
                            .or_else(|| {
                                current_branch_before_switch
                                    .as_deref()
                                    .map(|b| config.get_normalized_branch_name(b))
                            })
                    } else {
                        None
                    }
                });

            let recorded_worktree_path = worktree_path.clone().or_else(|| {
                existing
                    .as_ref()
                    .and_then(|b| b.worktree_path.as_ref().cloned())
            });

            let created_at = existing
                .as_ref()
                .map(|b| b.created_at)
                .unwrap_or_else(chrono::Utc::now);

            let devflow_branch = DevflowBranch {
                name: normalized_branch.clone(),
                parent: parent_branch,
                worktree_path: recorded_worktree_path,
                created_at,
            };
            if let Err(e) = state.register_branch(path, devflow_branch) {
                log::warn!("Failed to register branch in devflow registry: {}", e);
            }
        }
    }

    // ── Service branching (orchestrated across all auto_branch services) ──
    if !no_services {
        // Check if any services are configured before attempting service branching
        let has_services = !config.resolve_services().is_empty();

        if has_services {
            if !json_output {
                println!("Switching service branches: {}", normalized_branch);
            }

            // Orchestrate switch across all auto-branch services
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
                json_summary = Some(serde_json::json!({
                    "branch": normalized_branch,
                    "worktree_path": worktree_path,
                    "worktree_created": worktree_created,
                    "services_switched": success_count,
                    "services_failed": fail_count,
                    "service_results": service_results,
                }));
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

            if fail_count > 0 {
                if let Some(summary) = json_summary.take() {
                    println!("{}", serde_json::to_string_pretty(&summary)?);
                }
                anyhow::bail!(
                    "Failed to switch service branches on {}/{} service(s)",
                    fail_count,
                    results.len()
                );
            }
        } else {
            // No services configured — VCS switch already done above
            if json_output {
                json_summary = Some(serde_json::json!({
                    "branch": normalized_branch,
                    "worktree_path": worktree_path,
                    "worktree_created": worktree_created,
                    "services": "none_configured",
                }));
            } else {
                println!("Switched branch: {}", normalized_branch);
                println!("  (no services configured — use 'devflow service add' to add one)");
            }
        }
    } else {
        // Services skipped (--no-services) — branch registration already done above
        if json_output {
            json_summary = Some(serde_json::json!({
                "branch": normalized_branch,
                "worktree_path": worktree_path,
                "worktree_created": worktree_created,
                "services_skipped": true,
            }));
        } else {
            println!("Switched branch (services skipped): {}", normalized_branch);
        }
    }

    // ── Hooks ──────────────────────────────────────────────────────────
    if !no_verify {
        run_hooks(
            config,
            &normalized_branch,
            HookPhase::PostSwitch,
            json_output,
            non_interactive,
        )
        .await?;
    }

    if let Some(summary) = json_summary {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    }

    Ok(())
}

async fn handle_switch_to_main(
    config: &Config,
    config_path: &Option<std::path::PathBuf>,
    json_output: bool,
    no_services: bool,
    no_verify: bool,
    non_interactive: bool,
) -> Result<()> {
    let main_branch = &config.git.main_branch;
    let shell_integration = shell_integration_enabled();

    if !json_output {
        println!("Switching to main branch: {}", main_branch);
    }

    // ── Switch the git branch / worktree ───────────────────────────────
    let worktree_enabled = config.worktree.as_ref().is_some_and(|wt| wt.enabled);
    let mut worktree_path: Option<String> = None;

    if worktree_enabled {
        let vcs_repo = vcs::detect_vcs_provider(".").context("Failed to open VCS repository")?;
        let target_path = vcs_repo
            .worktree_path(main_branch)?
            .or_else(|| vcs_repo.main_worktree_dir());

        if let Some(wt_path) = target_path {
            // If we're already in the target directory, ensure main is checked out now.
            if std::env::current_dir().ok().as_deref() == Some(wt_path.as_path()) {
                vcs_repo.checkout_branch(main_branch)?;
            }

            if !json_output {
                println!("Switching to main worktree: {}", wt_path.display());
                println!("DEVFLOW_CD={}", wt_path.display());
                if !shell_integration {
                    print_manual_cd_hint(&wt_path);
                }
            }
            worktree_path = Some(wt_path.display().to_string());
        } else {
            vcs_repo.checkout_branch(main_branch)?;
        }
    } else {
        // Classic mode — switch the working directory to the main branch
        let vcs_repo = vcs::detect_vcs_provider(".").context("Failed to open VCS repository")?;
        vcs_repo.checkout_branch(main_branch)?;
    }

    // Update current branch in local state
    if let Some(ref path) = config_path {
        if let Ok(mut state) = LocalStateManager::new() {
            if let Err(e) = state.set_current_branch(path, Some(main_branch.to_string())) {
                log::warn!("Failed to persist current branch in local state: {}", e);
            }

            let existing_main = state.get_branch(path, main_branch);

            let main_record = DevflowBranch {
                name: main_branch.to_string(),
                parent: None,
                worktree_path: worktree_path.clone().or_else(|| {
                    existing_main
                        .as_ref()
                        .and_then(|b| b.worktree_path.as_ref().cloned())
                }),
                created_at: existing_main
                    .as_ref()
                    .map(|b| b.created_at)
                    .unwrap_or_else(chrono::Utc::now),
            };
            if let Err(e) = state.register_branch(path, main_record) {
                log::warn!("Failed to register main branch in devflow registry: {}", e);
            }
        }
    }

    // ── Switch services to main ────────────────────────────────────────
    let has_services = !config.resolve_services().is_empty();
    let mut json_summary: Option<serde_json::Value> = None;

    if no_services {
        if json_output {
            json_summary = Some(serde_json::json!({
                "branch": main_branch,
                "worktree_path": worktree_path,
                "services_skipped": true,
            }));
        } else {
            println!(
                "Switched to main branch (services skipped): {}",
                main_branch
            );
        }
    } else if has_services {
        let results = services::factory::orchestrate_switch(config, main_branch, None).await?;
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
            json_summary = Some(serde_json::json!({
                "branch": main_branch,
                "worktree_path": worktree_path,
                "services_switched": success_count,
                "services_failed": fail_count,
                "service_results": service_results,
            }));
        } else if fail_count == 0 {
            println!(
                "Switched to main branch: {} ({} service(s))",
                main_branch, success_count
            );
        } else {
            println!(
                "Switched to main branch: {} ({}/{} service(s), {} failed)",
                main_branch,
                success_count,
                results.len(),
                fail_count
            );
        }

        if fail_count > 0 {
            if let Some(summary) = json_summary.take() {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            }
            anyhow::bail!(
                "Failed to switch to main on {}/{} service(s)",
                fail_count,
                results.len()
            );
        }
    } else {
        // No services configured
        if json_output {
            json_summary = Some(serde_json::json!({
                "branch": main_branch,
                "worktree_path": worktree_path,
                "services": "none_configured",
            }));
        } else {
            println!("Switched to main branch: {}", main_branch);
            println!("  (no services configured — use 'devflow service add' to add one)");
        }
    }

    // Execute hooks
    if !no_verify {
        run_hooks(
            config,
            main_branch,
            HookPhase::PostSwitch,
            json_output,
            non_interactive,
        )
        .await?;
    }

    if let Some(summary) = json_summary {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    }

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
    // VCS is optional — `remove` must work even without a git/jj repo
    // (e.g. service-only cleanup in a plain directory with .devflow.yml).
    let vcs_repo = vcs::detect_vcs_provider(".").ok();

    // Safety check: don't remove main branch
    if branch_name == config.git.main_branch {
        anyhow::bail!("Cannot remove the main branch '{}'", branch_name);
    }

    // Safety check: don't remove the currently checked-out branch
    if let Some(ref repo) = vcs_repo {
        if let Ok(Some(current)) = repo.current_branch() {
            if current == branch_name {
                anyhow::bail!(
                    "Cannot remove branch '{}' because it is currently checked out. Switch to another branch first.",
                    branch_name
                );
            }
        }
    }

    // Confirm unless --force (skip prompt in JSON/non-interactive mode — require --force)
    if !force {
        if json_output || non_interactive {
            anyhow::bail!("Use --force to confirm removal in non-interactive or JSON output mode");
        }
        println!("This will remove:");
        if vcs_repo.is_some() {
            println!("  - VCS branch: {}", branch_name);
        }
        if let Some(ref repo) = vcs_repo {
            if repo.worktree_path(branch_name)?.is_some() {
                println!("  - Worktree directory");
            }
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
    let mut service_failures = 0usize;
    let mut branch_deleted = false;
    let mut branch_delete_error: Option<String> = None;

    // 1. Remove worktree (if VCS is available and worktree exists)
    if let Some(ref repo) = vcs_repo {
        if let Some(wt_path) = repo.worktree_path(branch_name)? {
            worktree_path_str = Some(wt_path.display().to_string());
            if !json_output {
                println!("Removing worktree at: {}", wt_path.display());
            }
            if let Err(e) = repo.remove_worktree(&wt_path) {
                log::warn!(
                    "Failed to remove worktree, falling back to fs removal: {}",
                    e
                );
                if wt_path.exists() {
                    std::fs::remove_dir_all(&wt_path)
                        .context("Failed to remove worktree directory")?;
                }
            }
            worktree_removed = true;
            if !json_output {
                println!("Worktree removed.");
            }
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
            if !r.success {
                service_failures += 1;
            }
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

    // 3. Delete the VCS branch (if VCS is available)
    if let Some(ref repo) = vcs_repo {
        if !json_output {
            println!("Deleting branch: {}", branch_name);
        }
        if let Err(e) = repo.delete_branch(branch_name) {
            log::warn!("Failed to delete branch '{}': {}", branch_name, e);
            branch_delete_error = Some(e.to_string());
            if !json_output {
                println!("Warning: Failed to delete branch: {}", e);
            }
        } else {
            branch_deleted = true;
            if !json_output {
                println!("Branch deleted: {}", branch_name);
            }
        }
    }

    // 4. Clear local state if this was the current branch + unregister from branch registry
    if let Some(ref path) = config_path {
        if let Ok(state) = LocalStateManager::new() {
            if let Some(current) = state.get_current_branch(path) {
                let normalized = config.get_normalized_branch_name(branch_name);
                if current == normalized {
                    if let Ok(mut state) = LocalStateManager::new() {
                        if let Err(e) = state.set_current_branch(path, None) {
                            log::warn!("Failed to clear current branch from local state: {}", e);
                        }
                    }
                }
            }
        }
        // Unregister the branch from the devflow registry
        if let Ok(mut state) = LocalStateManager::new() {
            let normalized = config.get_normalized_branch_name(branch_name);
            if let Err(e) = state.unregister_branch(path, &normalized) {
                log::warn!("Failed to unregister branch from devflow registry: {}", e);
            }
        }
    }

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": if service_failures == 0 && branch_delete_error.is_none() { "ok" } else { "error" },
                "branch": branch_name,
                "branch_deleted": branch_deleted,
                "branch_delete_error": branch_delete_error.clone(),
                "worktree_removed": worktree_removed,
                "worktree_path": worktree_path_str,
                "services_skipped": keep_services,
                "service_failures": service_failures,
                "service_results": service_results,
            }))?
        );
    } else if service_failures == 0 && branch_delete_error.is_none() {
        println!("Branch '{}' removed successfully.", branch_name);
    } else {
        println!("Branch '{}' removal completed with errors.", branch_name);
    }

    if service_failures > 0 {
        anyhow::bail!(
            "Failed to remove service branches on {}/{} service(s)",
            service_failures,
            service_results.len()
        );
    }

    if let Some(error) = branch_delete_error {
        anyhow::bail!("Failed to delete VCS branch '{}': {}", branch_name, error);
    }

    Ok(())
}

/// Handle `devflow destroy` — tear down the entire devflow project.
///
/// This is the inverse of `devflow init`. It removes:
///   1. All service data (containers, databases, branches) via destroy_project()
///   2. Git worktrees created by devflow
///   3. VCS hooks installed by devflow
///   4. Branch registry and local state for this project
///   5. Hook approvals for this project
///   6. Configuration files (.devflow.yml, .devflow.local.yml)
async fn handle_destroy_project(
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
                println!("    - {} (all branches and data)", svc.name);
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

        println!("  Branch registry: will be cleared");

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
                        Ok(branches) => {
                            if !json_output {
                                println!(
                                    "  Destroyed '{}': {} branch(es) removed",
                                    svc_config.name,
                                    branches.len()
                                );
                            }
                            destroyed_services.push(serde_json::json!({
                                "service": svc_config.name,
                                "success": true,
                                "branches_destroyed": branches,
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
                    // Provider doesn't support destroy — try deleting all branches individually
                    match provider.list_branches().await {
                        Ok(branches) => {
                            let mut deleted = 0;
                            for branch in &branches {
                                if let Err(e) = provider.delete_branch(&branch.name).await {
                                    log::warn!(
                                        "Failed to delete branch '{}' on '{}': {}",
                                        branch.name,
                                        svc_config.name,
                                        e
                                    );
                                } else {
                                    deleted += 1;
                                }
                            }
                            if !json_output {
                                println!(
                                    "  Deleted {}/{} branch(es) from '{}'",
                                    deleted,
                                    branches.len(),
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
                                "Failed to list branches for service '{}': {}",
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

    // 4. Clear local state (branch registry, services, current branch)
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
                    println!("Cleared project state and branch registry.");
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

async fn handle_merge_command(
    config: &Config,
    target: Option<&str>,
    cleanup: bool,
    dry_run: bool,
    json_output: bool,
) -> Result<()> {
    let vcs_repo = vcs::detect_vcs_provider(".").context("Failed to open VCS repository")?;

    if vcs_repo.provider_name() != "git" {
        anyhow::bail!(
            "Merge is currently supported for git repositories only (detected: {}).",
            vcs_repo.provider_name()
        );
    }

    let initial_dir = std::env::current_dir().context("Failed to get current directory")?;

    // Determine source branch (current branch)
    let source = vcs_repo
        .current_branch()?
        .ok_or_else(|| anyhow::anyhow!("Could not determine current branch (detached HEAD?)"))?;

    // Determine target branch
    let target_branch = target.unwrap_or(&config.git.main_branch);

    if !vcs_repo.branch_exists(target_branch)? {
        anyhow::bail!(
            "Target branch '{}' does not exist. Run 'devflow list' to see available branches.",
            target_branch
        );
    }

    if source == target_branch {
        anyhow::bail!("Source and target branch are the same: '{}'", source);
    }

    // If a dedicated worktree already exists for the target branch, perform the
    // merge there to avoid checking out a branch that may be locked elsewhere.
    let merge_dir = vcs_repo
        .worktree_path(target_branch)?
        .unwrap_or_else(|| initial_dir.clone());

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
                    "merge_directory": merge_dir,
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
    if merge_dir == initial_dir {
        // Merge in the current worktree, so we must first move to target branch.
        vcs_repo.checkout_branch(target_branch).with_context(|| {
            format!(
                "Failed to switch to target branch '{}' before merge",
                target_branch
            )
        })?;
    }

    if !json_output {
        println!("\nMerging '{}' into '{}'...", source, target_branch);
        if merge_dir != initial_dir {
            println!("Using target worktree: {}", merge_dir.display());
        }
    }
    let status = tokio::process::Command::new("git")
        .args(["merge", &source, "--no-edit"])
        .current_dir(&merge_dir)
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
            if wt_path == initial_dir {
                log::warn!(
                    "Skipping removal of current worktree '{}'; run cleanup from another directory/worktree",
                    wt_path.display()
                );
                if !json_output {
                    println!(
                        "Warning: Skipping removal of current worktree: {}",
                        wt_path.display()
                    );
                }
            } else {
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
        }

        // Delete VCS branch
        // If this invocation is still on the source branch, detach first so the
        // branch becomes deletable.
        if let Ok(Some(current)) = vcs_repo.current_branch() {
            if current == source {
                let detach_status = tokio::process::Command::new("git")
                    .args(["checkout", "--detach"])
                    .current_dir(&initial_dir)
                    .status()
                    .await;
                match detach_status {
                    Ok(s) if s.success() => {}
                    Ok(s) => {
                        log::warn!(
                            "Failed to detach HEAD before deleting branch '{}': exit code {:?}",
                            source,
                            s.code()
                        );
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to detach HEAD before deleting branch '{}': {}",
                            source,
                            e
                        );
                    }
                }
            }
        }

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

        // Delete service branches across all auto-branch services
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

    // Show service source
    println!("Services:");
    if let Ok(state) = LocalStateManager::new() {
        // Try to find config path to look up state services
        let config_path = Config::find_config_file().ok().flatten();
        let state_services = config_path.as_ref().and_then(|p| state.get_services(p));

        if let Some(ref services) = state_services {
            println!("  Source: local state (~/.config/devflow/local_state.yml)");
            for b in services {
                let default_marker = if b.default { " (default)" } else { "" };
                println!("  - {} [{}]{}", b.name, b.provider_type, default_marker);
            }
        } else {
            let committed_services = effective_config.config.resolve_services();
            if committed_services.is_empty() {
                println!("  (none configured)");
            } else {
                println!("  Source: committed config (.devflow.yml)");
                for b in &committed_services {
                    let default_marker = if b.default { " (default)" } else { "" };
                    println!("  - {} [{}]{}", b.name, b.provider_type, default_marker);
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

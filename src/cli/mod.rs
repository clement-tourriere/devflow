mod agent;
mod commit;
mod config;
mod gc;
mod git_hook;
mod hook;
mod init;
mod plugin;
mod proxy;
mod service;
pub mod workspace;

use std::path::PathBuf;

use devflow_core::config::{Config, GlobalConfig, WorktreeConfig};
use devflow_core::services::{self};

use anyhow::{Context, Result};
use clap::Subcommand;
use devflow_core::state::LocalStateManager;
use devflow_core::vcs;

// Re-export workspace types used by service.rs
pub(crate) use workspace::{
    collect_list_workspace_names, context_matches_branch, ensure_default_workspace_registered,
    load_registry_branches_for_list, resolve_branch_context, BranchContextSource,
};

#[derive(Subcommand)]
pub enum Commands {
    // ── Workspace Management ──
    #[command(about = "List all workspaces (with service + worktree status)")]
    List,
    #[command(
        about = "Render full environment graph",
        long_about = "Render the full environment graph (workspace tree + service states + worktree paths).

This command is designed for both humans and automation:
  - human mode prints an ASCII tree with service workspaces under each workspace
  - --json mode prints a graph document suitable for tools/agents

Examples:
  devflow graph
  devflow --json graph",
        hide = true
    )]
    Graph,
    #[command(
        about = "Link an existing workspace into devflow",
        long_about = "Link an existing workspace into devflow.

This command records the workspace in the devflow registry and materializes
service workspaces when auto-workspace services are configured.

Examples:
  devflow link feature/auth
  devflow link feature/auth --from main",
        hide = true
    )]
    Link {
        #[arg(help = "Workspace name to link")]
        workspace_name: String,
        #[arg(
            short = 'b',
            long = "from",
            visible_alias = "base",
            help = "Parent workspace for lineage/service creation"
        )]
        from: Option<String>,
    },
    #[command(
        about = "Switch to an existing workspace/worktree (use -c to create)",
        long_about = "Switch to an existing workspace/worktree.\n\nWith no arguments, shows an interactive workspace picker with fuzzy search.\nWith a workspace name, switches to that workspace and aligns services/worktrees.\nIf the workspace does not exist, use -c/--create to create it first.\n\nExamples:\n  devflow switch                     # Interactive picker\n  devflow switch feature-auth        # Switch to existing workspace\n  devflow switch -c feature-new      # Create new workspace from current context\n  devflow switch -c feature-new --from release_1_0  # Create from explicit parent\n  devflow switch --template           # Switch to main/template\n  devflow switch feature-auth -x 'npm run migrate'  # Run command after switch"
    )]
    Switch {
        #[arg(
            help = "Workspace name to switch to (optional - if omitted, shows interactive selection)"
        )]
        workspace_name: Option<String>,
        #[arg(short = 'c', long, help = "Create a new workspace before switching")]
        create: bool,
        #[arg(
            short = 'b',
            long = "from",
            visible_alias = "base",
            help = "Parent workspace for new workspace creation (defaults to current context workspace)"
        )]
        from: Option<String>,
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
        #[arg(long, help = "Include gitignored files in worktree (overrides config)")]
        no_respect_gitignore: bool,
    },
    #[command(
        about = "Remove a workspace, its worktree, and associated service workspaces",
        long_about = "Remove a workspace, its worktree, and associated service workspaces.\n\nThis is a comprehensive cleanup command that removes:\n  - The workspace\n  - The worktree directory (if any)\n  - All associated service workspaces (containers, cloud workspaces)\n\nUnlike 'devflow service delete' which only removes service workspaces, 'remove'\ncleans up everything related to the workspace.\n\nExamples:\n  devflow remove feature-auth\n  devflow remove feature-auth --force\n  devflow remove feature-auth --keep-services  # Only remove worktree + workspace"
    )]
    Remove {
        #[arg(help = "Workspace name to remove")]
        workspace_name: String,
        #[arg(long, help = "Skip confirmation prompt")]
        force: bool,
        #[arg(long, help = "Keep service workspaces (only remove worktree)")]
        keep_services: bool,
    },
    #[command(
        about = "Merge current workspace into target (with optional cleanup)",
        long_about = "Merge current workspace into target (with optional cleanup).\n\nPerforms a git merge of the current workspace into the target workspace (defaults to main).\nWith --cleanup, also removes the source workspace, its worktree, and associated service workspaces.\n\nExamples:\n  devflow merge                        # Merge into main\n  devflow merge develop                # Merge into develop\n  devflow merge --cleanup              # Merge and delete source workspace + services\n  devflow merge --dry-run              # Preview without merging",
        hide = true
    )]
    Merge {
        #[arg(help = "Target workspace to merge into (default: main workspace)")]
        target: Option<String>,
        #[arg(long, help = "Delete the source workspace and worktree after merge")]
        cleanup: bool,
        #[arg(long, help = "Simulate merge without actual operations")]
        dry_run: bool,
    },
    #[command(
        about = "Clean up old service workspaces (alias for 'service cleanup')",
        long_about = "Clean up old service workspaces.\n\nRemoves stale service workspaces that no longer have a corresponding workspace.\nOptionally limit the number of workspaces to retain.\n\nExamples:\n  devflow cleanup                  # Remove orphaned service workspaces\n  devflow cleanup --max-count 10   # Keep at most 10 service workspaces",
        hide = true
    )]
    Cleanup {
        #[arg(long, help = "Maximum number of workspaces to keep")]
        max_count: Option<usize>,
    },

    // ── Services ──
    #[command(
        about = "Manage services (create, delete, start, stop, reset, ...)",
        long_about = "Manage service providers and their workspaces.\n\nService commands operate on the configured service providers (local Docker,\nNeon, DBLab, etc.) to create, delete, and manage workspace-isolated environments.\n\nExamples:\n  devflow service add                       # Interactive wizard\n  devflow service add mydb --provider local # Add with explicit options\n  devflow service create feature-auth       # Create service workspace\n  devflow service delete feature-auth       # Delete service workspace\n  devflow service cleanup --max-count 10    # Cleanup old service workspaces\n  devflow service start feature-auth        # Start a stopped container\n  devflow service stop feature-auth         # Stop a running container\n  devflow service reset feature-auth        # Reset to parent state\n  devflow service connection feature-auth   # Show connection info\n  devflow service status                    # Show service status\n  devflow service list                      # List configured services\n  devflow service remove mydb               # Remove a service config\n  devflow service logs feature-auth         # Show container logs\n  devflow service seed main --from dump.sql # Seed from external source\n  devflow service discover                  # Discover running Docker containers\n  devflow service destroy                   # Destroy all data",
        hide = true
    )]
    Service {
        #[command(subcommand)]
        action: ServiceCommands,
    },

    // ── Top-level aliases ──
    #[command(
        about = "Show connection info for a service workspace (alias for 'service connection')",
        long_about = "Show connection info for a service workspace.\n\nOutputs connection details in various formats for use in scripts and configuration.\nThis is an alias for 'devflow service connection'.\n\nExamples:\n  devflow connection feature-auth              # Connection URI\n  devflow connection feature-auth --format env  # Environment variables\n  devflow connection feature-auth --format json # JSON object"
    )]
    Connection {
        #[arg(help = "Name of the workspace")]
        workspace_name: String,
        #[arg(long, help = "Output format: uri, env, or json")]
        format: Option<String>,
    },
    #[command(
        about = "Show current project and service status",
        long_about = "Show current project and service status.\n\nDisplays the current workspace, configured services, their states,\nand connection info. Useful for quick orientation.\n\nExamples:\n  devflow status\n  devflow --json status"
    )]
    Status,

    // ── VCS ──
    #[command(
        about = "Commit staged changes with optional AI-generated message",
        long_about = "Commit staged changes with optional AI-generated message.\n\nWith no flags, opens your editor for a manual commit message.\nWith --ai, generates a commit message using the configured LLM\n(external CLI command preferred, API as fallback).\n\nExamples:\n  devflow commit                      # Open editor\n  devflow commit -m 'fix: typo'       # Direct message\n  devflow commit --ai                 # AI-generated message\n  devflow commit --ai --edit          # AI-generated, then edit\n  devflow commit --ai --dry-run       # Preview AI message only"
    )]
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
        long_about = "Tear down the entire devflow project.\n\nThis is the inverse of 'devflow init'. It permanently destroys:\n  - All service data (containers, databases, workspaces)\n  - Git worktrees created by devflow\n  - VCS hooks installed by devflow\n  - Workspace registry and local state\n  - Hook approvals\n  - Configuration files (.devflow.yml, .devflow.local.yml)\n\nThis is irreversible. Use --force to skip the confirmation prompt.\n\nExamples:\n  devflow destroy              # Interactive confirmation\n  devflow destroy --force      # Skip confirmation",
        hide = true
    )]
    Destroy {
        #[arg(long, help = "Skip confirmation prompt")]
        force: bool,
    },
    #[command(
        about = "Show current configuration",
        long_about = "Show current configuration.\n\nDisplays the merged configuration from .devflow.yml, .devflow.local.yml,\nand environment variable overrides. Use -v for detailed precedence info.\n\nExamples:\n  devflow config              # Show merged config YAML\n  devflow config -v           # Show precedence details + env overrides",
        hide = true
    )]
    Config {
        #[arg(
            short,
            long,
            help = "Show effective configuration with precedence details"
        )]
        verbose: bool,
    },
    #[command(
        about = "Run diagnostics and check system health",
        long_about = "Run diagnostics and check system health.\n\nVerifies that required tools (docker, git/jj) are available, configuration is valid,\nand services are reachable. Reports any issues with suggested fixes.\n\nExamples:\n  devflow doctor\n  devflow --json doctor"
    )]
    Doctor,
    #[command(
        about = "Install Git hooks",
        long_about = "Install Git hooks.\n\nSets up post-checkout and post-commit Git hooks so devflow\nautomatically creates service workspaces and switches environments\non checkout. Safe to re-run.\n\nExamples:\n  devflow install-hooks",
        hide = true
    )]
    InstallHooks,
    #[command(
        about = "Uninstall Git hooks",
        long_about = "Uninstall Git hooks.\n\nRemoves the devflow Git hooks (post-checkout, post-commit).\nYour existing service workspaces and worktrees are not affected.\n\nExamples:\n  devflow uninstall-hooks",
        hide = true
    )]
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
        long_about = "Print shell integration script.\n\nThe shell wrapper enables automatic 'cd' whenever devflow emits DEVFLOW_CD\n(for example: switch to worktrees, open from TUI, init into a new directory).\nWithout it, devflow cannot change your parent shell directory and you must\ncd manually.\n\nAdd to your shell profile:\n  eval \"$(devflow shell-init)\"        # auto-detects shell\n  eval \"$(devflow shell-init bash)\"   # ~/.bashrc\n  eval \"$(devflow shell-init zsh)\"    # ~/.zshrc\n  devflow shell-init fish | source    # ~/.config/fish/config.fish\n\nThis creates a 'devflow' shell wrapper function.",
        hide = true
    )]
    ShellInit {
        #[arg(help = "Shell type: bash, zsh, or fish (auto-detected from $SHELL if omitted)")]
        shell: Option<String>,
    },
    #[command(
        name = "worktree-setup",
        about = "Set up devflow in a Git worktree (copy files, create DB workspace)",
        long_about = "Set up devflow in a Git worktree.\n\nCopies configuration files and creates service workspaces for the current\nworktree. Normally called automatically by Git hooks, but can be run\nmanually if hooks are not installed.\n\nExamples:\n  devflow worktree-setup",
        hide = true
    )]
    WorktreeSetup,
    #[command(
        name = "setup-zfs",
        about = "Set up a file-backed ZFS pool for Copy-on-Write storage (Linux)",
        hide = true
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
  devflow --json capabilities",
        hide = true
    )]
    Capabilities,

    // ── Extensibility ──
    #[command(
        about = "Manage lifecycle hooks",
        long_about = "Manage lifecycle hooks.\n\nHooks are MiniJinja-templated commands that run at specific lifecycle phases\n(post-create, post-switch, pre-merge, etc.). Configure them in .devflow.yml\nunder the 'hooks' section.\n\nExamples:\n  devflow hook show                  # List all configured hooks\n  devflow hook show post-create      # Show hooks for a specific phase\n  devflow hook run post-create       # Run hooks for a phase manually\n  devflow hook approvals             # List approved hooks\n  devflow hook approvals clear       # Clear all approvals",
        hide = true
    )]
    Hook {
        #[command(subcommand)]
        action: HookCommands,
    },
    #[command(
        about = "Manage plugin services",
        long_about = "Manage plugin services.\n\nPlugins extend devflow with custom service providers via JSON-over-stdio protocol.\nAny executable that speaks the protocol can be a provider.\n\nExamples:\n  devflow plugin list                    # List configured plugin services\n  devflow plugin check my-plugin         # Verify a plugin works\n  devflow plugin init my-plugin --lang bash  # Print a plugin scaffold script",
        hide = true
    )]
    Plugin {
        #[command(subcommand)]
        action: PluginCommands,
    },

    // ── AI Agent ──
    #[command(
        about = "AI agent integration (start, status, context, skill)",
        long_about = "AI agent integration.\n\nManage AI coding agents that work in isolated workspace environments.\nLaunch agents into worktrees, track their status, and install\nproject-specific skills.\n\nExamples:\n  devflow agent start fix-login -- 'Fix the login timeout bug'\n  devflow agent status\n  devflow agent context\n  devflow agent skill",
        hide = true
    )]
    Agent {
        #[command(subcommand)]
        action: AgentCommands,
    },

    // ── Proxy ──
    #[command(
        about = "Local reverse proxy (auto-HTTPS for Docker containers)",
        long_about = "Local reverse proxy for Docker containers.\n\nAuto-discovers Docker containers and provides HTTPS access via\n*.localhost domains. Uses a local CA for certificate generation.\n\nExamples:\n  devflow proxy start                # Start the proxy\n  devflow proxy start --daemon       # Start in background\n  devflow proxy stop                 # Stop the proxy\n  devflow proxy status               # Show proxy status\n  devflow proxy list                 # List proxied containers\n  devflow proxy trust install        # Install CA certificate\n  devflow proxy trust verify         # Check if CA is trusted\n  devflow proxy trust remove         # Remove CA from trust store\n  devflow proxy trust info           # Show trust instructions",
        hide = true
    )]
    Proxy {
        #[command(subcommand)]
        action: ProxyCommands,
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

    // ── Garbage Collection ──
    #[command(
        about = "Detect and clean up orphaned projects (missing directory, leftover state)",
        long_about = "Detect and clean up orphaned projects.\n\nScans all state stores (SQLite, local state YAML, Docker containers) for projects\nwhose directories no longer exist on disk. Orphaned resources include stopped/running\nDocker containers, database state, data directories, and registry entries.\n\nBy default, lists orphans and lets you pick which to clean up interactively.\n\nExamples:\n  devflow gc                     # Interactive: list orphans, pick to clean\n  devflow gc --list               # Just list orphans (no cleanup)\n  devflow gc --all                # Clean all orphans (with confirmation)\n  devflow gc --all --force        # Clean all orphans (skip confirmation)\n  devflow --json gc               # Machine-readable orphan list",
        hide = true
    )]
    Gc {
        #[arg(long, help = "Only list orphans, do not clean up")]
        list: bool,
        #[arg(long, help = "Clean up all orphans (with confirmation unless --force)")]
        all: bool,
        #[arg(long, help = "Skip confirmation prompts")]
        force: bool,
    },
}

/// Subcommands for `devflow service`.
#[derive(Subcommand)]
pub enum ServiceCommands {
    #[command(
        about = "Create a new service workspace",
        long_about = "Create a new service workspace.\n\nCreates Docker containers and/or cloud workspaces for the specified workspace name.\n\nExamples:\n  devflow service create feature-auth\n  devflow service create feature-auth --from develop"
    )]
    Create {
        #[arg(help = "Name of the workspace to create")]
        workspace_name: String,
        #[arg(long, help = "Parent workspace to clone from")]
        from: Option<String>,
    },
    #[command(
        about = "Delete a service workspace (keeps workspace and worktree)",
        long_about = "Delete a service workspace (keeps workspace and worktree).\n\nRemoves service workspaces (containers, cloud workspaces) but preserves the workspace\nand any worktree directory. Use 'devflow remove' to delete everything.\n\nExamples:\n  devflow service delete feature-auth"
    )]
    Delete {
        #[arg(help = "Name of the workspace to delete")]
        workspace_name: String,
    },
    #[command(about = "Clean up old workspaces for this service")]
    Cleanup {
        #[arg(long, help = "Maximum number of workspaces to keep")]
        max_count: Option<usize>,
    },
    #[command(about = "List configured services")]
    List,
    #[command(about = "Show service status")]
    Status,
    #[command(about = "Show service capabilities")]
    Capabilities,
    #[command(
        about = "Add a new service provider",
        long_about = "Add a new service provider to the project.\n\nConfigures a new service provider (local Docker, Neon, ClickHouse, etc.) and\nstores it in local state. When run without flags, an interactive wizard guides\nyou through service type, provider, and name selection.\n\nExamples:\n  devflow service add                              # Interactive wizard\n  devflow service add mydb                         # Interactive (name pre-filled)\n  devflow service add mydb --provider neon          # Add Neon cloud provider\n  devflow service add analytics --provider local --service-type clickhouse"
    )]
    Add {
        #[arg(help = "Service name (prompted if omitted)")]
        name: Option<String>,
        #[arg(long, help = "Provider type (local, neon, dblab, xata)")]
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
            help = "Seed main workspace from source (PostgreSQL URL, file path, or s3:// URL)"
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
    #[command(about = "Start a stopped workspace container (local provider)")]
    Start {
        #[arg(help = "Name of the workspace to start")]
        workspace_name: String,
    },
    #[command(about = "Stop a running workspace container (local provider)")]
    Stop {
        #[arg(help = "Name of the workspace to stop")]
        workspace_name: String,
    },
    #[command(about = "Reset a workspace to its parent state (local provider)")]
    Reset {
        #[arg(help = "Name of the workspace to reset")]
        workspace_name: String,
    },
    #[command(about = "Destroy all workspaces and data for a service (local provider)")]
    Destroy {
        #[arg(long, help = "Skip confirmation prompt")]
        force: bool,
    },
    #[command(
        about = "Show connection info for a service workspace",
        long_about = "Show connection info for a service workspace.\n\nOutputs connection details in various formats for use in scripts and configuration.\n\nExamples:\n  devflow service connection feature-auth              # Connection URI\n  devflow service connection feature-auth --format env  # Environment variables\n  devflow service connection feature-auth --format json # JSON object"
    )]
    Connection {
        #[arg(help = "Name of the workspace")]
        workspace_name: String,
        #[arg(long, help = "Output format: uri, env, or json")]
        format: Option<String>,
    },
    #[command(
        about = "Show container logs for a workspace",
        long_about = "Show container logs for a workspace.\n\nDisplays stdout/stderr from the Docker container backing a service workspace.\nUseful for debugging startup failures, query errors, or crash loops.\n\nExamples:\n  devflow service logs main                    # Last 100 lines from main\n  devflow service logs feature/auth --tail 50  # Last 50 lines\n  devflow service logs main -s analytics       # Logs from a specific service"
    )]
    Logs {
        #[arg(help = "Name of the workspace to show logs for")]
        workspace_name: String,
        #[arg(long, help = "Number of lines to show (default: 100)")]
        tail: Option<usize>,
    },
    #[command(
        about = "Seed a workspace from an external source",
        long_about = "Seed a workspace database from an external source.\n\nLoads data into an existing workspace from a PostgreSQL URL, local dump file,\nor S3 URL. The workspace must already exist.\n\nExamples:\n  devflow service seed main --from dump.sql                    # Seed from local file\n  devflow service seed feature/auth --from postgresql://...     # Seed from live database\n  devflow service seed main --from s3://bucket/path/dump.sql   # Seed from S3"
    )]
    Seed {
        #[arg(help = "Name of the workspace to seed")]
        workspace_name: String,
        #[arg(
            long,
            help = "Source to seed from (PostgreSQL URL, file path, or s3:// URL)"
        )]
        from: String,
    },

    #[command(
        about = "Discover running Docker containers matching known service types",
        long_about = "Discover running Docker containers matching known service types.\n\nBy default, results are scoped to Docker Compose containers belonging to the\ncurrent devflow project (matched by compose working directory/config files).\nUse --global to discover across all projects. Always excludes devflow-managed\ncontainers.\n\nExamples:\n  devflow service discover                           # Current project containers only\n  devflow service discover --service-type postgres   # Current project PostgreSQL only\n  devflow service discover --global                  # All projects"
    )]
    Discover {
        #[arg(
            long,
            help = "Filter by service type (postgres, clickhouse, mysql, generic)"
        )]
        service_type: Option<String>,
        #[arg(
            long,
            help = "Discover across all projects (disable current-project scoping)"
        )]
        global: bool,
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
        #[arg(long, help = "Workspace name context (defaults to current workspace)")]
        workspace: Option<String>,
    },
    #[command(about = "Manage hook approvals")]
    Approvals {
        #[command(subcommand)]
        action: ApprovalCommands,
    },
    #[command(
        about = "Explain hook phases and template variables",
        long_about = "Explain hook phases and template variables.\n\nWith no arguments, lists all hook phases with descriptions.\nWith a phase name, shows detailed info and an example.\n\nExamples:\n  devflow hook explain              # List all phases\n  devflow hook explain post-create  # Detailed info for post-create"
    )]
    Explain {
        #[arg(help = "Phase name to explain (e.g. post-create, post-switch)")]
        phase: Option<String>,
    },
    #[command(
        about = "Show available template variables with current values",
        long_about = "Show all template variables available in hook templates.\n\nDisplays the current workspace, repo, services, and all filters\nwith their actual resolved values.\n\nExamples:\n  devflow hook vars\n  devflow hook vars --workspace feature/auth"
    )]
    Vars {
        #[arg(long, help = "Workspace name to use for context (defaults to current)")]
        workspace: Option<String>,
    },
    #[command(
        about = "Render a template string with current context",
        long_about = "Render a MiniJinja template string using the current project context.\n\nUseful for testing templates before adding them to .devflow.yml.\n\nExamples:\n  devflow hook render '{{ service[\"db\"].url }}'\n  devflow hook render 'DATABASE_URL={{ service[\"db\"].url }}'\n  devflow hook render '{{ workspace | sanitize_db }}'"
    )]
    Render {
        #[arg(help = "Template string to render")]
        template: String,
        #[arg(long, help = "Workspace name to use for context (defaults to current)")]
        workspace: Option<String>,
    },
    #[command(
        about = "Show VCS event → devflow phase trigger mapping",
        long_about = "Display the mapping from VCS events (e.g. git post-checkout) to\ndevflow hook phases (e.g. post-switch).\n\nThis mapping can be customized in .devflow.yml under the 'triggers' key.\n\nExamples:\n  devflow hook triggers"
    )]
    Triggers,
    #[command(
        about = "List available built-in hook actions",
        long_about = "Show all built-in action types that can be used in hooks.\n\nActions replace shell commands for common operations like writing env files,\nfind-and-replace in files, copying files, HTTP requests, and notifications.\n\nExamples:\n  devflow hook actions"
    )]
    Actions,
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

/// Subcommands for `devflow agent`.
#[derive(Subcommand)]
pub enum AgentCommands {
    #[command(
        about = "Start an AI agent in a new workspace",
        long_about = "Start an AI agent in a new isolated workspace.\n\nCreates a worktree workspace and launches the configured agent tool.\n\nExamples:\n  devflow agent start fix-login -- 'Fix the login timeout'\n  devflow agent start fix-login --command claude\n  devflow agent start fix-login --dry-run"
    )]
    Start {
        #[arg(help = "Workspace name (will be prefixed with agent/ by default)")]
        workspace: String,
        #[arg(long, help = "Agent command to launch (overrides config)")]
        command: Option<String>,
        #[arg(last = true, help = "Prompt to pass to the agent")]
        prompt: Vec<String>,
        #[arg(long, help = "Show what would be done without executing")]
        dry_run: bool,
    },
    #[command(about = "Show agent status across all workspaces")]
    Status,
    #[command(
        about = "Output project context for the current workspace",
        long_about = "Output structured context for AI agents.\n\nIncludes workspace info, service connections, and project config.\n\nExamples:\n  devflow agent context\n  devflow agent context --format json\n  devflow agent context --workspace feature/auth"
    )]
    Context {
        #[arg(
            long,
            default_value = "markdown",
            help = "Output format: json or markdown"
        )]
        format: String,
        #[arg(long, help = "Workspace to generate context for")]
        workspace: Option<String>,
    },
    #[command(
        about = "Install agent skills for this project",
        long_about = "Install devflow workspace skills into .agents/skills/ (Agent Skills standard).\n\nSkills are automatically available in Claude Code, Cursor, OpenCode,\nand any tool supporting the agentskills.io standard.\n\nExamples:\n  devflow agent skill"
    )]
    Skill,
}

/// Subcommands for `devflow proxy`.
#[derive(Subcommand)]
pub enum ProxyCommands {
    #[command(about = "Start the reverse proxy")]
    Start {
        #[arg(long, help = "Run as a background daemon")]
        daemon: bool,
        #[arg(long, default_value = "443", help = "HTTPS listen port")]
        https_port: u16,
        #[arg(long, default_value = "80", help = "HTTP listen port")]
        http_port: u16,
        #[arg(long, default_value = "2019", help = "API listen port")]
        api_port: u16,
    },
    #[command(about = "Stop the reverse proxy")]
    Stop,
    #[command(about = "Show proxy status")]
    Status,
    #[command(about = "List proxied containers")]
    List,
    #[command(about = "Manage CA certificate trust")]
    Trust {
        #[command(subcommand)]
        action: TrustCommands,
    },
}

/// Subcommands for `devflow proxy trust`.
#[derive(Subcommand)]
pub enum TrustCommands {
    #[command(about = "Install CA certificate to system trust store")]
    Install,
    #[command(about = "Verify CA certificate is trusted")]
    Verify,
    #[command(about = "Remove CA certificate from system trust store")]
    Remove,
    #[command(about = "Show trust installation instructions")]
    Info,
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
        return super::tui::run().await;
    }

    // Proxy commands — no config needed
    if let Commands::Proxy { action } = cmd {
        return proxy::handle_proxy_command(action, json_output).await;
    }

    // Gc command — no config needed, operates on global state
    if let Commands::Gc { list, all, force } = cmd {
        return gc::handle_gc_command(list, all, force, json_output, non_interactive).await;
    }

    // Commands that need service infrastructure (config loading, state injection)
    let uses_service = matches!(
        cmd,
        Commands::Service { .. }
            | Commands::List
            | Commands::Graph
            | Commands::Link { .. }
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
            log::debug!("devflow is disabled for current workspace");
        }
        return Ok(());
    }

    // Check for required config file after checking if disabled
    if requires_config && config_path.is_none() {
        // Service commands allow no config (will use local provider defaults)
        // This is fine — create_provider_default() handles auto-detection
    }

    // Get the merged configuration for normal operations
    let mut config_merged = effective_config.get_merged_config();

    // Inject services from state (state services take precedence over committed)
    let local_state_for_services = if uses_service {
        LocalStateManager::new().ok()
    } else {
        None
    };
    if let Some(ref state_manager) = local_state_for_services {
        if let Some(ref path) = config_path {
            if let Some(state_services) = state_manager.get_services(path) {
                config_merged.services = Some(state_services);
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
        return match cmd {
            Commands::Connection {
                workspace_name,
                format,
            } => {
                // Top-level alias: delegate to service connection
                let svc_cmd = ServiceCommands::Connection {
                    workspace_name,
                    format,
                };
                service::handle_service_provider_command(
                    svc_cmd,
                    &mut config_merged,
                    json_output,
                    non_interactive,
                    database_name,
                    &config_path,
                )
                .await
            }
            Commands::Status => {
                // Top-level status: show both VCS and service info
                service::handle_top_level_status(
                    &mut config_merged,
                    json_output,
                    non_interactive,
                    database_name,
                    &config_path,
                )
                .await
            }
            Commands::Service { action } => {
                service::handle_service_dispatch(
                    action,
                    &mut config_merged,
                    &effective_config,
                    json_output,
                    non_interactive,
                    database_name,
                    &config_path,
                )
                .await
            }
            // Workspace management commands that need service context
            _ => {
                workspace::handle_branch_command(
                    cmd,
                    &mut config_merged,
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
            let mut init_target_dir: Option<std::path::PathBuf> = None;

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
                init_target_dir = Some(std::env::current_dir()?);
            }

            let init_config_path = std::env::current_dir()?.join(".devflow.yml");

            if init_config_path.exists() && !force {
                anyhow::bail!(
                    "Configuration already exists at {}. Use --force to overwrite.",
                    init_config_path.display()
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

            let mut init_config = Config {
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
                // VCS already exists — ensure it has at least one commit so the
                // default workspace is materialised and `list_workspaces` works.
                if let Ok(vcs_provider) = vcs::detect_vcs_provider(".") {
                    let _ = vcs_provider.ensure_initial_commit();
                }
                None
            };

            // Auto-detect main workspace from VCS
            if let Ok(vcs_prov) = vcs::detect_vcs_provider(".") {
                if let Ok(Some(detected_main)) = vcs_prov.default_workspace() {
                    init_config.git.main_workspace = detected_main.clone();
                    if !json_output {
                        println!(
                            "Auto-detected main workspace: {} ({})",
                            detected_main,
                            vcs_prov.provider_name()
                        );
                    }
                } else if !json_output {
                    println!("Could not auto-detect main workspace, using default: main");
                }
            }

            // Propose worktree configuration
            let enable_worktrees = if json_output || non_interactive {
                // Default to enabled in non-interactive / JSON mode
                true
            } else {
                println!();
                inquire::Confirm::new(
                    "Enable worktrees? (isolate each workspace in its own directory)",
                )
                .with_default(true)
                .with_help_message(
                    "Recommended. Each workspace gets its own working directory via git worktrees.",
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
                init_config.worktree = Some(WorktreeConfig::recommended_default());

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
            init_config.services = None;
            init_config.save_to_file(&init_config_path)?;

            // Register the VCS default workspace as devflow root.
            if let Err(e) =
                ensure_default_workspace_registered(&init_config, &Some(init_config_path.clone()))
            {
                log::warn!("Failed to register default workspace in local state: {}", e);
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
                        "config_path": init_config_path.display().to_string(),
                        "cd_path": init_target_dir.as_ref().map(|p| p.display().to_string()),
                        "worktree_enabled": enable_worktrees,
                        "cow_capability": cow_label,
                        "vcs_initialized": vcs_init_label,
                    }))?
                );
            } else {
                println!(
                    "Initialized devflow configuration at: {}",
                    init_config_path.display()
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
                        "\nWorktrees enabled. Each workspace will get its own working directory."
                    );
                    println!("  Path template: ../{{repo}}.{{workspace}}");
                    println!("  Files copied:  .env, .env.local");
                }

                // ── Guided wizard steps (interactive only) ──────────────────

                let interactive = !non_interactive;
                let mut added_services: Vec<String> = Vec::new();
                let mut hooks_installed = false;
                let mut shell_configured = false;

                if interactive {
                    // Step 1: Offer to add services
                    println!();
                    let add_service = inquire::Confirm::new(
                        "Would you like to add a service?",
                    )
                    .with_default(true)
                    .with_help_message("Configure a database or other service (e.g. PostgreSQL, ClickHouse, Redis)")
                    .prompt()
                    .unwrap_or(false);

                    if add_service {
                        loop {
                            let result = service::run_add_service_wizard(
                                &mut init_config,
                                &init_config_path,
                                non_interactive,
                                json_output,
                                None,
                            )
                            .await?;

                            if let Some(cfg) = result {
                                added_services.push(format!(
                                    "{} ({}, {})",
                                    cfg.name, cfg.service_type, cfg.provider_type
                                ));
                            }

                            let add_another = inquire::Confirm::new("Add another service?")
                                .with_default(false)
                                .prompt()
                                .unwrap_or(false);

                            if !add_another {
                                break;
                            }
                        }
                    }

                    // Step 2: Offer Git hooks installation
                    println!();
                    let install_hooks = inquire::Confirm::new(
                        "Install Git hooks? (auto-sync services on checkout)",
                    )
                    .with_default(true)
                    .with_help_message("Recommended. Automatically creates/switches service workspaces on git checkout.")
                    .prompt()
                    .unwrap_or(false);

                    if install_hooks {
                        if let Ok(vcs_prov) = vcs::detect_vcs_provider(".") {
                            match vcs_prov.install_hooks() {
                                Ok(()) => {
                                    hooks_installed = true;
                                    println!("Installed {} hooks", vcs_prov.provider_name());
                                }
                                Err(e) => {
                                    eprintln!("Warning: could not install hooks: {}", e);
                                }
                            }
                        }
                    }

                    // Step 3: Offer shell integration (only if worktrees are enabled)
                    if enable_worktrees {
                        println!();
                        let setup_shell = inquire::Confirm::new(
                            "Enable shell integration? (auto-cd into worktrees)",
                        )
                        .with_default(true)
                        .with_help_message("Adds eval \"$(devflow shell-init)\" to your shell profile for automatic directory switching.")
                        .prompt()
                        .unwrap_or(false);

                        if setup_shell {
                            if let Ok(shell) = config::detect_shell_from_env() {
                                let home = std::env::var("HOME").ok().map(PathBuf::from);
                                let shell_config_path = match shell.as_str() {
                                    "zsh" => home.as_ref().map(|h| h.join(".zshrc")),
                                    "bash" => {
                                        let bashrc = home.as_ref().map(|h| h.join(".bashrc"));
                                        if bashrc.as_ref().is_some_and(|p| p.exists()) {
                                            bashrc
                                        } else {
                                            home.as_ref().map(|h| h.join(".bash_profile"))
                                        }
                                    }
                                    "fish" => home.as_ref().map(|h| {
                                        h.join(".config").join("fish").join("config.fish")
                                    }),
                                    _ => None,
                                };

                                let eval_line = if shell == "fish" {
                                    "devflow shell-init fish | source"
                                } else {
                                    "eval \"$(devflow shell-init)\""
                                };

                                if let Some(ref rc_path) = shell_config_path {
                                    let already_configured = rc_path.exists()
                                        && std::fs::read_to_string(rc_path)
                                            .unwrap_or_default()
                                            .contains("devflow shell-init");

                                    if already_configured {
                                        println!(
                                            "Shell integration already configured in {}",
                                            rc_path.display()
                                        );
                                        shell_configured = true;
                                    } else {
                                        let append = inquire::Confirm::new(&format!(
                                            "Append to {}?",
                                            rc_path.display()
                                        ))
                                        .with_default(true)
                                        .prompt()
                                        .unwrap_or(false);

                                        if append {
                                            use std::io::Write;
                                            if let Ok(mut file) = std::fs::OpenOptions::new()
                                                .append(true)
                                                .create(true)
                                                .open(rc_path)
                                            {
                                                writeln!(file, "\n# devflow shell integration")?;
                                                writeln!(file, "{}", eval_line)?;
                                                println!("Added to {}", rc_path.display());
                                                shell_configured = true;
                                            } else {
                                                println!(
                                                    "Could not write to {}. Add manually:",
                                                    rc_path.display()
                                                );
                                                println!("  {}", eval_line);
                                            }
                                        } else {
                                            println!("Add this to your shell profile:");
                                            println!("  {}", eval_line);
                                        }
                                    }
                                } else {
                                    println!("Add this to your shell profile:");
                                    println!("  {}", eval_line);
                                }
                            }
                        }
                    }

                    // Step 4: Print summary
                    println!();
                    println!("devflow initialized for '{}'", resolved_name);
                    println!();
                    println!("  Config:     {}", init_config_path.display());
                    if !added_services.is_empty() {
                        println!("  Services:   {}", added_services.join(", "));
                    }
                    println!(
                        "  Hooks:      {}",
                        if hooks_installed {
                            "installed"
                        } else {
                            "not installed"
                        }
                    );
                    if enable_worktrees {
                        println!(
                            "  Shell:      {}",
                            if shell_configured {
                                "configured"
                            } else {
                                "not configured (run: eval \"$(devflow shell-init)\")"
                            }
                        );
                        println!("  Worktrees:  enabled (../{{repo}}.{{workspace}})");
                    }
                    println!();
                    println!("Next: devflow switch -c feature/my-feature");
                } else {
                    // Non-interactive: print legacy next steps
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
                    println!(
                        "  devflow doctor               Check system health and configuration"
                    );
                }

                if let Some(target_dir) = init_target_dir.as_ref() {
                    if config::shell_integration_enabled() {
                        println!("DEVFLOW_CD={}", target_dir.display());
                    } else if non_interactive {
                        config::print_manual_cd_hint(target_dir);
                    }
                }
            }
        }
        Commands::Destroy { force } => {
            init::handle_destroy_project(
                &mut config_merged,
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
                use devflow_core::services::postgres::local::storage::zfs_setup::*;

                let pool = pool_name.unwrap_or_else(|| "devflow".to_string());
                let img_size = size.unwrap_or_else(|| "10G".to_string());

                let zfs_config = ZfsPoolSetupConfig {
                    pool_name: pool.clone(),
                    image_path: PathBuf::from(format!("/var/lib/devflow/{}.img", pool)),
                    image_size: img_size.clone(),
                    mountpoint: PathBuf::from("/var/lib/devflow/data"),
                };

                println!("Creating file-backed ZFS pool:");
                println!("  Pool name:  {}", zfs_config.pool_name);
                println!(
                    "  Image:      {} (sparse, {})",
                    zfs_config.image_path.display(),
                    img_size
                );
                println!("  Mountpoint: {}", zfs_config.mountpoint.display());
                println!();

                let data_root = create_file_backed_pool(&zfs_config).await?;
                println!();
                println!("ZFS pool '{}' created successfully", pool);
                println!("Data root: {}", data_root);
                println!();
                println!("Run 'devflow init' to set up a project using this pool.");
            }
        }
        Commands::Hook { action } => {
            hook::handle_hook_command(action, &config_merged, json_output, non_interactive).await?;
        }
        Commands::Plugin { action } => {
            plugin::handle_plugin_command(action, &config_merged, json_output).await?;
        }
        Commands::Agent { action } => {
            agent::handle_agent_command(
                action,
                &config_merged,
                json_output,
                non_interactive,
                &config_path,
            )
            .await?;
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
                            "current_workspace_disabled": effective_config.is_current_workspace_disabled(),
                            "has_local_config": effective_config.local_config.is_some(),
                            "config": serde_json::to_value(&merged_config)?,
                        }))?
                    );
                } else {
                    println!("{}", serde_json::to_string_pretty(&config_merged)?);
                }
            } else if verbose {
                config::show_effective_config(&effective_config)?;
            } else {
                println!("Current configuration:");
                println!("{}", serde_yaml_ng::to_string(&config_merged)?);
            }
        }
        Commands::Capabilities => {
            let mut config_with_state = config_merged.clone();
            if let Some(ref path) = config_path {
                if let Ok(state) = LocalStateManager::new() {
                    if let Some(state_services) = state.get_services(path) {
                        config_with_state.services = Some(state_services);
                    }
                }
            }

            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let cow_capability = match vcs::cow_worktree::detect_cow_capability(&cwd) {
                vcs::cow_worktree::CowCapability::Apfs => "apfs",
                vcs::cow_worktree::CowCapability::Reflink => "reflink",
                vcs::cow_worktree::CowCapability::None => "none",
            };

            let vcs_provider = vcs::detect_vcs_provider(".")
                .ok()
                .map(|v| v.provider_name().to_string());

            let mut service_capabilities = serde_json::Map::new();
            let mut service_capabilities_error: Option<String> = None;
            match services::factory::create_all_providers(&config_with_state).await {
                Ok(providers) => {
                    for named in &providers {
                        service_capabilities.insert(
                            named.name.clone(),
                            serde_json::json!({
                                "provider": named.provider.provider_name(),
                                "capabilities": named.provider.capabilities(),
                            }),
                        );
                    }
                }
                Err(e) => {
                    service_capabilities_error = Some(e.to_string());
                }
            }

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
                "environment": {
                    "vcs_provider": vcs_provider,
                    "worktree_cow": cow_capability,
                },
                "service_capabilities": service_capabilities,
                "service_capabilities_error": service_capabilities_error,
            });

            if json_output {
                println!("{}", serde_json::to_string_pretty(&payload)?);
            } else {
                println!("Automation capabilities:");
                println!("- JSON mode: single JSON document on stdout (diagnostics on stderr)");
                println!("- Non-interactive: no prompts; --force required for destroy/remove");
                println!("- Multi-service partial failures: command exits non-zero by default");
                println!("- Recommended flags for agents: --json --non-interactive");
                println!(
                    "- Environment detection: vcs={}, worktree_cow={}",
                    payload["environment"]["vcs_provider"]
                        .as_str()
                        .unwrap_or("none"),
                    cow_capability
                );
                if !service_capabilities.is_empty() {
                    println!("- Service capabilities:");
                    for (name, details) in &service_capabilities {
                        let caps = &details["capabilities"];
                        println!(
                            "  - {} ({}): lifecycle={} logs={} seed={} destroy={} cleanup={}",
                            name,
                            details["provider"].as_str().unwrap_or("unknown"),
                            config::yes_no(caps["lifecycle"].as_bool()),
                            config::yes_no(caps["logs"].as_bool()),
                            config::yes_no(caps["seed_from_source"].as_bool()),
                            config::yes_no(caps["destroy_project"].as_bool()),
                            config::yes_no(caps["cleanup"].as_bool()),
                        );
                    }
                }
                if let Some(err) = service_capabilities_error {
                    println!("- Service capability probe warning: {}", err);
                }
            }
        }
        Commands::ShellInit { shell } => {
            let detected_shell = match shell {
                Some(s) => s,
                None => config::detect_shell_from_env()?,
            };
            config::print_shell_init(&detected_shell)?;
        }
        Commands::InstallHooks => {
            let vcs_prov = vcs::detect_vcs_provider(".")?;
            vcs_prov.install_hooks()?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "action": "install_hooks",
                        "vcs_provider": vcs_prov.provider_name(),
                    }))?
                );
            } else {
                println!("Installed {} hooks", vcs_prov.provider_name());
            }
        }
        Commands::UninstallHooks => {
            let vcs_prov = vcs::detect_vcs_provider(".")?;
            vcs_prov.uninstall_hooks()?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "action": "uninstall_hooks",
                        "vcs_provider": vcs_prov.provider_name(),
                    }))?
                );
            } else {
                println!("Uninstalled {} hooks", vcs_prov.provider_name());
            }
        }
        Commands::Commit {
            message,
            ai,
            edit,
            dry_run,
        } => {
            commit::handle_commit_command(message, ai, edit, dry_run, json_output, &config_merged)
                .await?;
        }
        Commands::Gc { list, all, force } => {
            gc::handle_gc_command(list, all, force, json_output, non_interactive).await?;
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

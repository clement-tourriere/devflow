use std::path::PathBuf;

use devflow_core::config::{Config, EffectiveConfig, GlobalConfig, WorktreeConfig};
use devflow_core::services::{self, ServiceProvider};

use devflow_core::docker;
use devflow_core::hooks::{
    approval::ApprovalStore, HookContext, HookEngine, HookEntry, HookPhase, IndexMap,
    TemplateEngine,
};
use devflow_core::state::{DevflowBranch, LocalStateManager};
use devflow_core::vcs;
use anyhow::{Context, Result};
use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    // ── Branch Management ──
    #[command(about = "List all branches (with service + worktree status)")]
    List,
    #[command(
        about = "Render full environment graph",
        long_about = "Render the full environment graph (branch tree + service states + worktree paths).

This command is designed for both humans and automation:
  - human mode prints an ASCII tree with service branches under each branch
  - --json mode prints a graph document suitable for tools/agents

Examples:
  devflow graph
  devflow --json graph"
    )]
    Graph,
    #[command(
        about = "Link an existing VCS branch into devflow",
        long_about = "Link an existing VCS branch into devflow.

This command records the branch in the devflow registry and materializes
service branches when auto-branch services are configured.

Examples:
  devflow link feature/auth
  devflow link feature/auth --from main"
    )]
    Link {
        #[arg(help = "Branch name to link")]
        branch_name: String,
        #[arg(
            short = 'b',
            long = "from",
            visible_alias = "base",
            help = "Parent branch for lineage/service creation"
        )]
        from: Option<String>,
    },
    #[command(
        about = "Switch to an existing branch/worktree (use -c to create)",
        long_about = "Switch to an existing branch/worktree.\n\nWith no arguments, shows an interactive branch picker with fuzzy search.\nWith a branch name, switches to that branch and aligns services/worktrees.\nIf the branch does not exist, use -c/--create to create it first.\n\nExamples:\n  devflow switch                     # Interactive picker\n  devflow switch feature-auth        # Switch to existing branch\n  devflow switch -c feature-new      # Create new branch from current context\n  devflow switch -c feature-new --from release_1_0  # Create from explicit parent\n  devflow switch --template           # Switch to main/template\n  devflow switch feature-auth -x 'npm run migrate'  # Run command after switch"
    )]
    Switch {
        #[arg(
            help = "Branch name to switch to (optional - if omitted, shows interactive selection)"
        )]
        branch_name: Option<String>,
        #[arg(short = 'c', long, help = "Create a new branch before switching")]
        create: bool,
        #[arg(
            short = 'b',
            long = "from",
            visible_alias = "base",
            help = "Parent branch for new branch creation (defaults to current context branch)"
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
    #[command(
        about = "Merge current branch into target (with optional cleanup)",
        long_about = "Merge current branch into target (with optional cleanup).\n\nPerforms a git merge of the current branch into the target branch (defaults to main).\nWith --cleanup, also removes the source branch, its worktree, and associated service branches.\n\nExamples:\n  devflow merge                        # Merge into main\n  devflow merge develop                # Merge into develop\n  devflow merge --cleanup              # Merge and delete source branch + services\n  devflow merge --dry-run              # Preview without merging"
    )]
    Merge {
        #[arg(help = "Target branch to merge into (default: main branch)")]
        target: Option<String>,
        #[arg(long, help = "Delete the source branch and worktree after merge")]
        cleanup: bool,
        #[arg(long, help = "Simulate merge without actual operations")]
        dry_run: bool,
    },
    #[command(
        about = "Clean up old service branches (alias for 'service cleanup')",
        long_about = "Clean up old service branches.\n\nRemoves stale service branches that no longer have a corresponding VCS branch.\nOptionally limit the number of branches to retain.\n\nExamples:\n  devflow cleanup                  # Remove orphaned service branches\n  devflow cleanup --max-count 10   # Keep at most 10 service branches"
    )]
    Cleanup {
        #[arg(long, help = "Maximum number of branches to keep")]
        max_count: Option<usize>,
    },

    // ── Services ──
    #[command(
        about = "Manage services (create, delete, start, stop, reset, ...)",
        long_about = "Manage service providers and their branches.\n\nService commands operate on the configured service providers (local Docker,\nNeon, DBLab, etc.) to create, delete, and manage branch-isolated environments.\n\nExamples:\n  devflow service add                       # Interactive wizard\n  devflow service add mydb --provider local # Add with explicit options\n  devflow service create feature-auth       # Create service branch\n  devflow service delete feature-auth       # Delete service branch\n  devflow service cleanup --max-count 10    # Cleanup old service branches\n  devflow service start feature-auth        # Start a stopped container\n  devflow service stop feature-auth         # Stop a running container\n  devflow service reset feature-auth        # Reset to parent state\n  devflow service connection feature-auth   # Show connection info\n  devflow service status                    # Show service status\n  devflow service list                      # List configured services\n  devflow service remove mydb               # Remove a service config\n  devflow service logs feature-auth         # Show container logs\n  devflow service seed main --from dump.sql # Seed from external source\n  devflow service destroy                   # Destroy all data"
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
    #[command(
        about = "Show current project and service status",
        long_about = "Show current project and service status.\n\nDisplays the current branch, configured services, their states,\nand connection info. Useful for quick orientation.\n\nExamples:\n  devflow status\n  devflow --json status"
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
        long_about = "Tear down the entire devflow project.\n\nThis is the inverse of 'devflow init'. It permanently destroys:\n  - All service data (containers, databases, branches)\n  - Git worktrees created by devflow\n  - VCS hooks installed by devflow\n  - Branch registry and local state\n  - Hook approvals\n  - Configuration files (.devflow.yml, .devflow.local.yml)\n\nThis is irreversible. Use --force to skip the confirmation prompt.\n\nExamples:\n  devflow destroy              # Interactive confirmation\n  devflow destroy --force      # Skip confirmation"
    )]
    Destroy {
        #[arg(long, help = "Skip confirmation prompt")]
        force: bool,
    },
    #[command(
        about = "Show current configuration",
        long_about = "Show current configuration.\n\nDisplays the merged configuration from .devflow.yml, .devflow.local.yml,\nand environment variable overrides. Use -v for detailed precedence info.\n\nExamples:\n  devflow config              # Show merged config YAML\n  devflow config -v           # Show precedence details + env overrides"
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
        long_about = "Install Git hooks.\n\nSets up post-checkout and post-commit Git hooks so devflow\nautomatically creates service branches and switches environments\non checkout. Safe to re-run.\n\nExamples:\n  devflow install-hooks"
    )]
    InstallHooks,
    #[command(
        about = "Uninstall Git hooks",
        long_about = "Uninstall Git hooks.\n\nRemoves the devflow Git hooks (post-checkout, post-commit).\nYour existing service branches and worktrees are not affected.\n\nExamples:\n  devflow uninstall-hooks"
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
        long_about = "Print shell integration script.\n\nThe shell wrapper enables automatic 'cd' whenever devflow emits DEVFLOW_CD\n(for example: switch to worktrees, open from TUI, init into a new directory).\nWithout it, devflow cannot change your parent shell directory and you must\ncd manually.\n\nAdd to your shell profile:\n  eval \"$(devflow shell-init)\"        # auto-detects shell\n  eval \"$(devflow shell-init bash)\"   # ~/.bashrc\n  eval \"$(devflow shell-init zsh)\"    # ~/.zshrc\n  devflow shell-init fish | source    # ~/.config/fish/config.fish\n\nThis creates a 'devflow' shell wrapper function."
    )]
    ShellInit {
        #[arg(help = "Shell type: bash, zsh, or fish (auto-detected from $SHELL if omitted)")]
        shell: Option<String>,
    },
    #[command(
        name = "worktree-setup",
        about = "Set up devflow in a Git worktree (copy files, create DB branch)",
        long_about = "Set up devflow in a Git worktree.\n\nCopies configuration files and creates service branches for the current\nworktree. Normally called automatically by Git hooks, but can be run\nmanually if hooks are not installed.\n\nExamples:\n  devflow worktree-setup"
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

    // ── AI Agent ──
    #[command(
        about = "AI agent integration (start, status, context, skill)",
        long_about = "AI agent integration.\n\nManage AI coding agents that work in isolated branch environments.\nLaunch agents into worktrees, track their status, and generate\nproject-specific skills/rules for different AI tools.\n\nExamples:\n  devflow agent start fix-login -- 'Fix the login timeout bug'\n  devflow agent status\n  devflow agent context\n  devflow agent skill\n  devflow agent docs"
    )]
    Agent {
        #[command(subcommand)]
        action: AgentCommands,
    },

    // ── Proxy ──
    #[command(
        about = "Local reverse proxy (auto-HTTPS for Docker containers)",
        long_about = "Local reverse proxy for Docker containers.\n\nAuto-discovers Docker containers and provides HTTPS access via\n*.localhost domains. Uses a local CA for certificate generation.\n\nExamples:\n  devflow proxy start                # Start the proxy\n  devflow proxy start --daemon       # Start in background\n  devflow proxy stop                 # Stop the proxy\n  devflow proxy status               # Show proxy status\n  devflow proxy list                 # List proxied containers\n  devflow proxy trust install        # Install CA certificate\n  devflow proxy trust verify         # Check if CA is trusted\n  devflow proxy trust remove         # Remove CA from trust store\n  devflow proxy trust info           # Show trust instructions"
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
        long_about = "Detect and clean up orphaned projects.\n\nScans all state stores (SQLite, local state YAML, Docker containers) for projects\nwhose directories no longer exist on disk. Orphaned resources include stopped/running\nDocker containers, database state, data directories, and registry entries.\n\nBy default, lists orphans and lets you pick which to clean up interactively.\n\nExamples:\n  devflow gc                     # Interactive: list orphans, pick to clean\n  devflow gc --list               # Just list orphans (no cleanup)\n  devflow gc --all                # Clean all orphans (with confirmation)\n  devflow gc --all --force        # Clean all orphans (skip confirmation)\n  devflow --json gc               # Machine-readable orphan list"
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
    #[command(about = "Clean up old branches for this service")]
    Cleanup {
        #[arg(long, help = "Maximum number of branches to keep")]
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
        #[arg(help = "Name of the branch to show logs for")]
        branch_name: String,
        #[arg(long, help = "Number of lines to show (default: 100)")]
        tail: Option<usize>,
    },
    #[command(
        about = "Seed a branch from an external source",
        long_about = "Seed a branch database from an external source.\n\nLoads data into an existing branch from a PostgreSQL URL, local dump file,\nor S3 URL. The branch must already exist.\n\nExamples:\n  devflow service seed main --from dump.sql                    # Seed from local file\n  devflow service seed feature/auth --from postgresql://...     # Seed from live database\n  devflow service seed main --from s3://bucket/path/dump.sql   # Seed from S3"
    )]
    Seed {
        #[arg(help = "Name of the branch to seed")]
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
        long_about = "Show all template variables available in hook templates.\n\nDisplays the current branch, repo, services, and all filters\nwith their actual resolved values.\n\nExamples:\n  devflow hook vars\n  devflow hook vars --branch feature/auth"
    )]
    Vars {
        #[arg(long, help = "Branch name to use for context (defaults to current)")]
        branch: Option<String>,
    },
    #[command(
        about = "Render a template string with current context",
        long_about = "Render a MiniJinja template string using the current project context.\n\nUseful for testing templates before adding them to .devflow.yml.\n\nExamples:\n  devflow hook render '{{ service[\"db\"].url }}'\n  devflow hook render 'DATABASE_URL={{ service[\"db\"].url }}'\n  devflow hook render '{{ branch | sanitize_db }}'"
    )]
    Render {
        #[arg(help = "Template string to render")]
        template: String,
        #[arg(long, help = "Branch name to use for context (defaults to current)")]
        branch: Option<String>,
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

/// Subcommands for `devflow agent`.
#[derive(Subcommand)]
pub enum AgentCommands {
    #[command(
        about = "Start an AI agent in a new branch",
        long_about = "Start an AI agent in a new isolated branch.\n\nCreates a worktree branch and launches the configured agent tool.\n\nExamples:\n  devflow agent start fix-login -- 'Fix the login timeout'\n  devflow agent start fix-login --command claude\n  devflow agent start fix-login --dry-run"
    )]
    Start {
        #[arg(help = "Branch name (will be prefixed with agent/ by default)")]
        branch: String,
        #[arg(long, help = "Agent command to launch (overrides config)")]
        command: Option<String>,
        #[arg(last = true, help = "Prompt to pass to the agent")]
        prompt: Vec<String>,
        #[arg(long, help = "Show what would be done without executing")]
        dry_run: bool,
    },
    #[command(about = "Show agent status across all branches")]
    Status,
    #[command(
        about = "Output project context for the current branch",
        long_about = "Output structured context for AI agents.\n\nIncludes branch info, service connections, and project config.\n\nExamples:\n  devflow agent context\n  devflow agent context --format json\n  devflow agent context --branch feature/auth"
    )]
    Context {
        #[arg(
            long,
            default_value = "markdown",
            help = "Output format: json or markdown"
        )]
        format: String,
        #[arg(long, help = "Branch to generate context for")]
        branch: Option<String>,
    },
    #[command(
        about = "Generate AI tool skills/rules for this project",
        long_about = "Generate project-specific configuration files for AI tools.\n\nGenerates skills, rules, or configuration for Claude Code, OpenCode,\nCursor, or all tools at once.\n\nExamples:\n  devflow agent skill                   # Generate for all tools\n  devflow agent skill --target claude    # Claude Code only\n  devflow agent skill --target cursor    # Cursor only"
    )]
    Skill {
        #[arg(
            long,
            default_value = "all",
            help = "Target: claude, opencode, cursor, or all"
        )]
        target: String,
    },
    #[command(
        about = "Generate AGENTS.md for this project",
        long_about = "Generate a comprehensive AGENTS.md tailored to this project.\n\nIncludes actual service names, connection patterns, hook phases,\nand project-specific agent workflow examples."
    )]
    Docs,
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
        return handle_proxy_command(action, json_output).await;
    }

    // Gc command — no config needed, operates on global state
    if let Commands::Gc { list, all, force } = cmd {
        return handle_gc_command(list, all, force, json_output, non_interactive).await;
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
                // VCS already exists — ensure it has at least one commit so the
                // default branch is materialised and `list_branches` works.
                if let Ok(vcs_provider) = vcs::detect_vcs_provider(".") {
                    let _ = vcs_provider.ensure_initial_commit();
                }
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

            // Register the VCS default branch as devflow root.
            if let Err(e) = ensure_default_branch_registered(&config, &Some(config_path.clone())) {
                log::warn!("Failed to register default branch in local state: {}", e);
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
                        "cd_path": init_target_dir.as_ref().map(|p| p.display().to_string()),
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

                if let Some(target_dir) = init_target_dir.as_ref() {
                    if shell_integration_enabled() {
                        println!("DEVFLOW_CD={}", target_dir.display());
                    } else {
                        print_manual_cd_hint(target_dir);
                    }
                }
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
                use devflow_core::services::postgres::local::storage::zfs_setup::*;

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
        Commands::Agent { action } => {
            handle_agent_command(action, &config, json_output, non_interactive, &config_path)
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
            let mut config_with_state = config.clone();
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
                            yes_no(caps["lifecycle"].as_bool()),
                            yes_no(caps["logs"].as_bool()),
                            yes_no(caps["seed_from_source"].as_bool()),
                            yes_no(caps["destroy_project"].as_bool()),
                            yes_no(caps["cleanup"].as_bool()),
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
            handle_commit_command(message, ai, edit, dry_run, json_output, &config).await?;
        }
        Commands::Gc { list, all, force } => {
            handle_gc_command(list, all, force, json_output, non_interactive).await?;
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

async fn init_local_service_main(
    config: &Config,
    named_cfg: &devflow_core::config::NamedServiceConfig,
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
                    eprintln!("  You can create it later with: devflow service create main");
                }
            }
        }
        Err(e) => {
            eprintln!(
                "Warning: could not initialize service '{}': {}",
                named_cfg.name, e
            );
            eprintln!("  You can create the main branch later with: devflow service create main");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchContextSource {
    EnvOverride,
    Cwd,
    None,
}

#[derive(Debug, Clone)]
struct BranchContext {
    /// Raw branch used as context (env override or cwd branch).
    context_branch_raw: Option<String>,
    /// Normalized devflow context branch name.
    context_branch: Option<String>,
    /// Raw VCS branch currently checked out in this directory.
    cwd_branch: Option<String>,
    source: BranchContextSource,
}

fn resolve_branch_context(config: &Config) -> BranchContext {
    let cwd_branch = vcs::detect_vcs_provider(".")
        .ok()
        .and_then(|repo| repo.current_branch().ok().flatten());

    if let Ok(env_branch) = std::env::var("DEVFLOW_CONTEXT_BRANCH") {
        let trimmed = env_branch.trim();
        if !trimmed.is_empty() {
            return BranchContext {
                context_branch_raw: Some(trimmed.to_string()),
                context_branch: Some(config.get_normalized_branch_name(trimmed)),
                cwd_branch,
                source: BranchContextSource::EnvOverride,
            };
        }
    }

    if let Some(cwd) = cwd_branch.as_deref() {
        return BranchContext {
            context_branch_raw: Some(cwd.to_string()),
            context_branch: Some(config.get_normalized_branch_name(cwd)),
            cwd_branch,
            source: BranchContextSource::Cwd,
        };
    }

    BranchContext {
        context_branch_raw: None,
        context_branch: None,
        cwd_branch: None,
        source: BranchContextSource::None,
    }
}

fn context_matches_branch(
    config: &Config,
    context_branch: Option<&str>,
    branch_name: &str,
) -> bool {
    let Some(context) = context_branch else {
        return false;
    };
    context == branch_name || context == config.get_normalized_branch_name(branch_name)
}

fn linked_branch_exists(config: &Config, config_path: &Option<PathBuf>, branch_name: &str) -> bool {
    let Some(path) = config_path.as_ref() else {
        return false;
    };

    let normalized = config.get_normalized_branch_name(branch_name);
    LocalStateManager::new()
        .ok()
        .and_then(|state| state.get_branch(path, &normalized))
        .is_some()
}

fn register_branch_in_state(
    config: &Config,
    config_path: &Option<PathBuf>,
    branch_name: &str,
    parent_branch: Option<&str>,
    worktree_path: Option<String>,
) -> Result<()> {
    let Some(path) = config_path.as_ref() else {
        return Ok(());
    };

    let mut state = LocalStateManager::new()?;
    let normalized_branch = config.get_normalized_branch_name(branch_name);
    let normalized_parent = parent_branch.map(|p| config.get_normalized_branch_name(p));

    let existing = state.get_branch(path, &normalized_branch);
    let created_at = existing
        .as_ref()
        .map(|b| b.created_at)
        .unwrap_or_else(chrono::Utc::now);

    let final_parent =
        normalized_parent.or_else(|| existing.as_ref().and_then(|b| b.parent.clone()));
    let final_worktree = worktree_path.or_else(|| {
        existing
            .as_ref()
            .and_then(|b| b.worktree_path.as_ref().cloned())
    });

    state.register_branch(
        path,
        DevflowBranch {
            name: normalized_branch,
            parent: final_parent,
            worktree_path: final_worktree,
            created_at,
            agent_tool: None,
            agent_status: None,
            agent_started_at: None,
        },
    )?;

    Ok(())
}

fn ensure_default_branch_registered(config: &Config, config_path: &Option<PathBuf>) -> Result<()> {
    let main = config.git.main_branch.clone();
    if !linked_branch_exists(config, config_path, &main) {
        register_branch_in_state(config, config_path, &main, None, None)?;
    }
    Ok(())
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
    let git_branches: Vec<devflow_core::vcs::BranchInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_branches().ok())
        .unwrap_or_default();
    let worktrees: Vec<devflow_core::vcs::WorktreeInfo> = vcs_provider
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

    // Load branch registry from local state
    let mut registry: HashMap<String, Option<String>> = HashMap::new();
    if let Some(path) = config_path.as_ref() {
        if let Ok(state) = LocalStateManager::new() {
            registry = state
                .get_branches(path)
                .into_iter()
                .map(|b| (b.name, b.parent))
                .collect();
        }
    }

    let context = resolve_branch_context(config);

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

    // Sort roots: default branch first, then context branch, then cwd, then alphabetical
    roots.sort_by(|a, b| {
        let a_default = git_branches.iter().any(|gb| gb.name == *a && gb.is_default);
        let b_default = git_branches.iter().any(|gb| gb.name == *b && gb.is_default);
        if a_default != b_default {
            return b_default.cmp(&a_default);
        }
        let a_context = context_matches_branch(config, context.context_branch.as_deref(), a);
        let b_context = context_matches_branch(config, context.context_branch.as_deref(), b);
        if a_context != b_context {
            return b_context.cmp(&a_context);
        }
        let a_current = current_git.as_deref() == Some(*a);
        let b_current = current_git.as_deref() == Some(*b);
        if a_current != b_current {
            return b_current.cmp(&a_current);
        }
        a.cmp(b)
    });

    if context.source == BranchContextSource::EnvOverride {
        if let Some(context_branch) = context.context_branch.as_deref() {
            let cwd = context.cwd_branch.as_deref().unwrap_or("unknown");
            println!(
                "Context override: '{}' (from DEVFLOW_CONTEXT_BRANCH), cwd branch='{}'",
                context_branch, cwd
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
        context_branch: Option<&str>,
        service_branches: &[services::BranchInfo],
        service_names: &HashSet<&str>,
        wt_lookup: &HashMap<String, PathBuf>,
        config: &Config,
        #[allow(unused_variables)] git_branches: &[devflow_core::vcs::BranchInfo],
    ) {
        let is_current = current_git.as_deref() == Some(name);
        let marker = if is_current { "* " } else { "  " };
        let is_context = context_matches_branch(config, context_branch, name);

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
        if is_context {
            parts.push("context".to_string());
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
                    context_branch,
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
            context.context_branch.as_deref(),
            service_branches,
            &service_names,
            &wt_lookup,
            config,
            &git_branches,
        );
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct EnvGraphServiceEntry {
    service_name: String,
    provider_name: String,
    state: Option<String>,
    database_name: String,
    parent_branch: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct EnvGraphNode {
    name: String,
    parent: Option<String>,
    is_default: bool,
    is_cwd: bool,
    is_context: bool,
    worktree_path: Option<String>,
    services: Vec<EnvGraphServiceEntry>,
}

async fn handle_environment_graph(
    config: &Config,
    config_path: &Option<PathBuf>,
    json_output: bool,
) -> Result<()> {
    use std::collections::{HashMap, HashSet};

    // VCS view
    let vcs_provider = vcs::detect_vcs_provider(".").ok();
    let vcs_provider_name = vcs_provider
        .as_ref()
        .map(|p| p.provider_name().to_string())
        .unwrap_or_else(|| "none".to_string());
    let git_branches: Vec<devflow_core::vcs::BranchInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_branches().ok())
        .unwrap_or_default();
    let worktrees: Vec<devflow_core::vcs::WorktreeInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_worktrees().ok())
        .unwrap_or_default();
    let cwd_branch = vcs_provider
        .as_ref()
        .and_then(|r| r.current_branch().ok().flatten());

    // Local state view (branch registry only)
    let mut registry: HashMap<String, Option<String>> = HashMap::new();
    if let Some(path) = config_path.as_ref() {
        if let Ok(state) = LocalStateManager::new() {
            registry = state
                .get_branches(path)
                .into_iter()
                .map(|b| (b.name, b.parent))
                .collect();
        }
    }

    let context = resolve_branch_context(config);

    // Service view
    let mut service_entries_by_branch: HashMap<String, Vec<EnvGraphServiceEntry>> = HashMap::new();
    let mut service_probe_warnings: Vec<String> = Vec::new();
    match services::factory::create_all_providers(config).await {
        Ok(all_providers) => {
            for named in &all_providers {
                let provider_name = named.provider.provider_name().to_string();
                match named.provider.list_branches().await {
                    Ok(branches) => {
                        for b in branches {
                            service_entries_by_branch
                                .entry(b.name.clone())
                                .or_default()
                                .push(EnvGraphServiceEntry {
                                    service_name: named.name.clone(),
                                    provider_name: provider_name.clone(),
                                    state: b.state.clone(),
                                    database_name: b.database_name.clone(),
                                    parent_branch: b.parent_branch.clone(),
                                });
                        }
                    }
                    Err(e) => {
                        service_probe_warnings
                            .push(format!("{} ({}): {}", named.name, provider_name, e));
                    }
                }
            }
        }
        Err(e) => {
            service_probe_warnings.push(format!("provider initialization failed: {}", e));
        }
    }

    let wt_lookup: HashMap<String, PathBuf> = worktrees
        .iter()
        .filter_map(|wt| wt.branch.as_ref().map(|b| (b.clone(), wt.path.clone())))
        .collect();

    // Union of all known branch names
    let mut all_names: Vec<String> = Vec::new();
    let mut seen = HashSet::new();

    for gb in &git_branches {
        if seen.insert(gb.name.clone()) {
            all_names.push(gb.name.clone());
        }
    }
    for name in registry.keys() {
        if seen.insert(name.clone()) {
            all_names.push(name.clone());
        }
    }
    for name in service_entries_by_branch.keys() {
        if seen.insert(name.clone()) {
            all_names.push(name.clone());
        }
    }

    if all_names.is_empty() {
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "vcs_provider": vcs_provider_name,
                    "nodes": [],
                    "roots": [],
                    "cwd_branch": cwd_branch,
                    "context_branch": context.context_branch.clone(),
                    "context_source": match context.source {
                        BranchContextSource::EnvOverride => "env",
                        BranchContextSource::Cwd => "cwd",
                        BranchContextSource::None => "none",
                    },
                    "warnings": service_probe_warnings,
                }))?
            );
        } else {
            println!("Environment graph: (empty)");
        }
        return Ok(());
    }

    // Parent map with precedence: registry > service branch parent
    let mut parent_map: HashMap<String, String> = HashMap::new();

    for (name, entries) in &service_entries_by_branch {
        if let Some(parent) = entries.iter().find_map(|e| e.parent_branch.clone()) {
            if seen.contains(parent.as_str()) {
                parent_map.insert(name.clone(), parent);
            }
        }
    }

    for (name, parent) in &registry {
        if let Some(parent_name) = parent {
            if seen.contains(parent_name.as_str()) {
                parent_map.insert(name.clone(), parent_name.clone());
            }
        }
    }

    let mut children_map: HashMap<String, Vec<String>> = HashMap::new();
    for (child, parent) in &parent_map {
        children_map
            .entry(parent.clone())
            .or_default()
            .push(child.clone());
    }
    for kids in children_map.values_mut() {
        kids.sort();
    }

    // Roots
    let mut roots: Vec<String> = all_names
        .iter()
        .filter(|name| !parent_map.contains_key(name.as_str()))
        .cloned()
        .collect();

    let cwd_normalized = cwd_branch
        .as_deref()
        .map(|b| config.get_normalized_branch_name(b));

    roots.sort_by(|a, b| {
        let a_default = git_branches.iter().any(|gb| gb.name == *a && gb.is_default);
        let b_default = git_branches.iter().any(|gb| gb.name == *b && gb.is_default);
        if a_default != b_default {
            return b_default.cmp(&a_default);
        }

        let a_context = context_matches_branch(config, context.context_branch.as_deref(), a);
        let b_context = context_matches_branch(config, context.context_branch.as_deref(), b);
        if a_context != b_context {
            return b_context.cmp(&a_context);
        }

        let a_cwd =
            cwd_branch.as_deref() == Some(a.as_str()) || cwd_normalized.as_deref() == Some(a);
        let b_cwd =
            cwd_branch.as_deref() == Some(b.as_str()) || cwd_normalized.as_deref() == Some(b);
        if a_cwd != b_cwd {
            return b_cwd.cmp(&a_cwd);
        }

        a.cmp(b)
    });

    // Build node map for JSON and human rendering
    let mut node_map: HashMap<String, EnvGraphNode> = HashMap::new();
    for name in &all_names {
        let normalized = config.get_normalized_branch_name(name);

        let mut services = Vec::new();
        if let Some(entries) = service_entries_by_branch.get(name) {
            services.extend(entries.iter().cloned());
        }
        if normalized != *name {
            if let Some(entries) = service_entries_by_branch.get(&normalized) {
                for entry in entries {
                    if !services
                        .iter()
                        .any(|e| e.service_name == entry.service_name)
                    {
                        services.push(entry.clone());
                    }
                }
            }
        }
        services.sort_by(|a, b| a.service_name.cmp(&b.service_name));

        let is_cwd =
            cwd_branch.as_deref() == Some(name.as_str()) || cwd_normalized.as_deref() == Some(name);
        let is_context = context_matches_branch(config, context.context_branch.as_deref(), name);
        let is_default = git_branches
            .iter()
            .any(|gb| gb.name == *name && gb.is_default);

        node_map.insert(
            name.clone(),
            EnvGraphNode {
                name: name.clone(),
                parent: parent_map.get(name).cloned(),
                is_default,
                is_cwd,
                is_context,
                worktree_path: wt_lookup
                    .get(name)
                    .map(|p| p.display().to_string())
                    .or_else(|| {
                        wt_lookup
                            .iter()
                            .find(|(branch, _)| config.get_normalized_branch_name(branch) == *name)
                            .map(|(_, p)| p.display().to_string())
                    }),
                services,
            },
        );
    }

    if json_output {
        let mut nodes: Vec<EnvGraphNode> = node_map.values().cloned().collect();
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "vcs_provider": vcs_provider_name,
                "cwd_branch": cwd_branch,
                "context_branch": context.context_branch.clone(),
                "context_source": match context.source {
                    BranchContextSource::EnvOverride => "env",
                    BranchContextSource::Cwd => "cwd",
                    BranchContextSource::None => "none",
                },
                "roots": roots,
                "nodes": nodes,
                "warnings": service_probe_warnings,
            }))?
        );
        return Ok(());
    }

    println!("Environment graph ({})", vcs_provider_name);
    if let Some(context_branch) = context.context_branch.as_deref() {
        println!("Context branch: {}", context_branch);
    }
    if let Some(cwd) = cwd_branch.as_deref() {
        println!("CWD branch: {}", cwd);
    }
    if !service_probe_warnings.is_empty() {
        println!("Warnings:");
        for warning in &service_probe_warnings {
            println!("  - {}", warning);
        }
    }

    fn print_node(
        name: &str,
        prefix: &str,
        connector: &str,
        children_map: &HashMap<String, Vec<String>>,
        node_map: &HashMap<String, EnvGraphNode>,
    ) {
        let Some(node) = node_map.get(name) else {
            return;
        };

        let marker = if node.is_cwd { "* " } else { "  " };
        let mut tags = Vec::new();
        if node.is_default {
            tags.push("default".to_string());
        }
        if node.is_context {
            tags.push("context".to_string());
        }
        if let Some(path) = &node.worktree_path {
            tags.push(format!("worktree: {}", path));
        }

        if tags.is_empty() {
            println!("{}{}{}", marker, connector, node.name);
        } else {
            println!(
                "{}{}{}  [{}]",
                marker,
                connector,
                node.name,
                tags.join(", ")
            );
        }

        for svc in &node.services {
            let state = svc.state.as_deref().unwrap_or("unknown");
            let mut parts = vec![format!("{}:{}", svc.service_name, state)];
            parts.push(format!("provider: {}", svc.provider_name));
            parts.push(format!("db: {}", svc.database_name));
            if let Some(parent) = &svc.parent_branch {
                parts.push(format!("parent: {}", parent));
            }
            println!("{}   • {}", prefix, parts.join(", "));
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
                    node_map,
                );
            }
        }
    }

    for root in &roots {
        print_node(root, "", "", &children_map, &node_map);
    }

    Ok(())
}

/// Build enriched JSON for the list command, merging git + worktree + service info.
fn enrich_branch_list_json(
    service_branches: &[services::BranchInfo],
    config: &Config,
    config_path: &Option<PathBuf>,
) -> serde_json::Value {
    let vcs_provider = vcs::detect_vcs_provider(".").ok();
    let git_branches: Vec<devflow_core::vcs::BranchInfo> = vcs_provider
        .as_ref()
        .and_then(|r| r.list_branches().ok())
        .unwrap_or_default();
    let worktrees: Vec<devflow_core::vcs::WorktreeInfo> = vcs_provider
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

    // Load branch registry from local state
    let mut registry: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    if let Some(path) = config_path.as_ref() {
        if let Ok(state) = LocalStateManager::new() {
            registry = state
                .get_branches(path)
                .into_iter()
                .map(|b| (b.name, b.parent))
                .collect();
        }
    }

    let context = resolve_branch_context(config);

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
        let is_context = context_matches_branch(config, context.context_branch.as_deref(), name);

        let mut entry = serde_json::json!({
            "name": name,
            "is_current": gb.map(|b| b.is_current).unwrap_or(false),
            "is_default": gb.map(|b| b.is_default).unwrap_or(false),
            "is_context": is_context,
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

fn yes_no(value: Option<bool>) -> &'static str {
    if value.unwrap_or(false) {
        "yes"
    } else {
        "no"
    }
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
    println!("Note: devflow cannot change your parent shell directory without shell integration.");
    println!("Tip: add `eval \"$(devflow shell-init)\"` to your shell profile for auto-cd.");
}

fn resolve_cd_target(path: &std::path::Path) -> Result<std::path::PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

/// Print shell integration script for the given shell type.
///
/// Users should add `eval "$(devflow shell-init bash)"` (or zsh/fish) to their
/// shell profile. This defines a `devflow` wrapper function that:
/// 1. Runs `devflow` normally, preserving stderr
/// 2. Parses `DEVFLOW_CD=<path>` output from commands that request directory changes
/// 3. Automatically `cd`s into the target worktree directory
fn print_shell_init(shell: &str) -> Result<()> {
    let script = match shell {
        "bash" => {
            r#"
# devflow shell integration (bash)
# Wrapper function that auto-cds when devflow emits DEVFLOW_CD
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
# Wrapper function that auto-cds when devflow emits DEVFLOW_CD
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
# Wrapper function that auto-cds when devflow emits DEVFLOW_CD
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
    devflow_core::hooks::build_hook_context(config, branch_name).await
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
        HookCommands::Explain { phase } => {
            handle_hook_explain(phase.as_deref(), json_output)?;
        }
        HookCommands::Vars { branch } => {
            handle_hook_vars(config, branch.as_deref(), json_output).await?;
        }
        HookCommands::Render { template, branch } => {
            handle_hook_render(config, &template, branch.as_deref(), json_output).await?;
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

/// `devflow hook explain [phase]` — show documentation about hook phases.
fn handle_hook_explain(phase: Option<&str>, json_output: bool) -> Result<()> {
    // Static phase documentation: (name, summary, blocking, category, detail)
    let phases: Vec<(&str, &str, bool, &str, &str)> = vec![
        ("pre-switch",           "Before switching branches/worktrees",           true,  "VCS",     "Runs before any branch/worktree switch. Use for saving state or running checks."),
        ("post-create",          "After creating a new branch/worktree",          true,  "VCS",     "Runs after a new branch is created (via `switch -c`). Use for one-time setup: install dependencies, run migrations, write .env files."),
        ("post-start",           "After starting a stopped service container",    false, "VCS",     "Runs after `devflow service start`. Use for warming caches or re-applying state."),
        ("post-switch",          "After switching to a branch/worktree",          false, "VCS",     "Runs every time you switch branches. Use for updating .env files, restarting dev servers."),
        ("pre-remove",           "Before removing a branch",                      true,  "VCS",     "Runs before `devflow remove`. Use for cleanup tasks or archival."),
        ("post-remove",          "After removing a branch",                       false, "VCS",     "Runs after branch removal. Use for notifying external systems."),
        ("pre-commit",           "Before committing",                             true,  "Merge",   "Runs before `devflow commit`. Use for linting, formatting, or test checks."),
        ("pre-merge",            "Before merging branches",                       true,  "Merge",   "Runs before `devflow merge`. Use for running tests or CI checks."),
        ("post-merge",           "After merging branches",                        false, "Merge",   "Runs after a successful merge. Use for cleanup or deployment triggers."),
        ("post-rewrite",         "After rewriting history (rebase, amend)",       false, "Merge",   "Runs after Git history rewrite. Use for re-applying migrations."),
        ("pre-service-create",   "Before creating a service branch",              true,  "Service", "Runs before service provisioning. Use for pre-flight checks."),
        ("post-service-create",  "After creating a service branch",               true,  "Service", "Runs after service provisioning. THE most common hook — use for npm ci, migrations, writing .env files."),
        ("pre-service-delete",   "Before deleting a service branch",              true,  "Service", "Runs before service teardown. Use for data export or backups."),
        ("post-service-delete",  "After deleting a service branch",               false, "Service", "Runs after service teardown. Use for cleanup."),
        ("post-service-switch",  "After services switch to a branch",             false, "Service", "Runs after services switch (not VCS). Use for service-specific reconnection."),
    ];

    if json_output {
        let items: Vec<serde_json::Value> = phases
            .iter()
            .map(|(name, summary, blocking, category, detail)| {
                serde_json::json!({
                    "phase": name,
                    "summary": summary,
                    "blocking": blocking,
                    "category": category,
                    "detail": detail,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }

    if let Some(phase_name) = phase {
        // Show detailed info for one phase
        if let Some((name, summary, blocking, category, detail)) =
            phases.iter().find(|(n, ..)| *n == phase_name)
        {
            println!("{}", name);
            println!("{}", "=".repeat(name.len()));
            println!();
            println!("Category:  {}", category);
            println!(
                "Blocking:  {}",
                if *blocking {
                    "Yes (waits for completion)"
                } else {
                    "No (runs in background)"
                }
            );
            println!("Summary:   {}", summary);
            println!();
            println!("{}", detail);
            println!();
            println!("Example YAML:");
            println!();
            // Show a contextual example based on the phase
            match *name {
                "post-create" | "post-service-create" => {
                    println!("  hooks:");
                    println!("    {}:", name);
                    println!("      install: \"npm ci\"");
                    println!("      env: |");
                    println!("        cat > .env.local << EOF");
                    println!("        DATABASE_URL={{{{ service['db'].url }}}}");
                    println!("        EOF");
                    println!("      migrate: \"npx prisma migrate deploy\"");
                }
                "post-switch" | "post-service-switch" => {
                    println!("  hooks:");
                    println!("    {}:", name);
                    println!("      env: |");
                    println!("        cat > .env.local << EOF");
                    println!("        DATABASE_URL={{{{ service['db'].url }}}}");
                    println!("        EOF");
                }
                "pre-merge" | "pre-commit" => {
                    println!("  hooks:");
                    println!("    {}:", name);
                    println!("      lint: \"npm run lint\"");
                    println!("      test: \"npm test\"");
                }
                _ => {
                    println!("  hooks:");
                    println!("    {}:", name);
                    println!(
                        "      example: \"echo Running {} for {{{{ branch }}}}\"",
                        name
                    );
                }
            }
            println!();
            println!("Available template variables:");
            println!("  {{{{ branch }}}}              Current branch name");
            println!("  {{{{ repo }}}}                Repository name");
            println!("  {{{{ default_branch }}}}      Main branch (e.g. main)");
            println!("  {{{{ worktree_path }}}}       Worktree directory path");
            println!("  {{{{ service['name'].url }}}} Full connection URL for a service");
            println!(
                "  {{{{ service['name'].host/port/database/user/password }}}}"
            );
            println!();
            println!("Available filters:");
            println!("  {{{{ branch | sanitize }}}}     Path-safe (/ -> -)");
            println!("  {{{{ branch | sanitize_db }}}}  DB-safe (lowercase, _, max 63 chars)");
            println!("  {{{{ branch | hash_port }}}}    Deterministic port 10000-19999");
        } else {
            println!("Unknown phase: '{}'", phase_name);
            println!();
            println!("Built-in phases:");
            for (name, summary, blocking, ..) in &phases {
                println!(
                    "  {:<24} {} {}",
                    name,
                    if *blocking {
                        "[blocking]  "
                    } else {
                        "[background]"
                    },
                    summary
                );
            }
        }
    } else {
        // List all phases
        println!("Hook Phases");
        println!("===========");
        println!();
        println!("Which hook should I use?");
        println!("  Setting up a new branch?     -> post-create or post-service-create");
        println!("  Updating env on switch?      -> post-switch");
        println!("  Running tests before merge?  -> pre-merge");
        println!("  Custom setup per service?    -> post-service-create");
        println!();

        let mut current_category = "";
        for (name, summary, blocking, category, _) in &phases {
            if *category != current_category {
                println!();
                println!("{} Lifecycle:", category);
                current_category = category;
            }
            println!(
                "  {:<24} {} {}",
                name,
                if *blocking {
                    "[blocking]  "
                } else {
                    "[background]"
                },
                summary
            );
        }
        println!();
        println!("Use 'devflow hook explain <phase>' for detailed info and examples.");
    }

    Ok(())
}

/// `devflow hook vars` — show available template variables with current values.
async fn handle_hook_vars(
    config: &Config,
    branch_override: Option<&str>,
    json_output: bool,
) -> Result<()> {
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
    let engine = TemplateEngine::new();

    if json_output {
        println!("{}", serde_json::to_string_pretty(&context)?);
        return Ok(());
    }

    println!("Template Variables (current context):");
    println!();
    println!("  {{{{ branch }}}}              = {}", context.branch);
    println!("  {{{{ repo }}}}                = {}", context.repo);
    println!(
        "  {{{{ default_branch }}}}      = {}",
        context.default_branch
    );
    if let Some(ref wt) = context.worktree_path {
        println!("  {{{{ worktree_path }}}}       = {}", wt);
    }
    if let Some(ref commit) = context.commit {
        println!("  {{{{ commit }}}}              = {}", commit);
    }

    if !context.service.is_empty() {
        println!();
        println!("  Services:");
        for (name, svc) in &context.service {
            println!();
            println!(
                "    {{{{ service['{}'].host }}}}     = {}",
                name, svc.host
            );
            println!(
                "    {{{{ service['{}'].port }}}}     = {}",
                name, svc.port
            );
            println!(
                "    {{{{ service['{}'].database }}}} = {}",
                name, svc.database
            );
            println!(
                "    {{{{ service['{}'].user }}}}     = {}",
                name, svc.user
            );
            if let Some(ref pw) = svc.password {
                println!(
                    "    {{{{ service['{}'].password }}}} = {}",
                    name, pw
                );
            }
            println!(
                "    {{{{ service['{}'].url }}}}      = {}",
                name, svc.url
            );
        }
    }

    // Show filter examples
    println!();
    println!("  Filters:");
    let sanitized = engine
        .render("{{ branch | sanitize }}", &context)
        .unwrap_or_default();
    let sanitized_db = engine
        .render("{{ branch | sanitize_db }}", &context)
        .unwrap_or_default();
    let hash_port = engine
        .render("{{ branch | hash_port }}", &context)
        .unwrap_or_default();
    println!("    {{{{ branch | sanitize }}}}      = {}", sanitized);
    println!("    {{{{ branch | sanitize_db }}}}   = {}", sanitized_db);
    println!("    {{{{ branch | hash_port }}}}     = {}", hash_port);

    Ok(())
}

/// `devflow hook render <template>` — render a template string.
async fn handle_hook_render(
    config: &Config,
    template: &str,
    branch_override: Option<&str>,
    json_output: bool,
) -> Result<()> {
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
    let engine = TemplateEngine::new();
    let rendered = engine.render(template, &context)?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "template": template,
                "rendered": rendered,
            }))?
        );
    } else {
        println!("{}", rendered);
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
            match devflow_core::services::plugin::PluginProvider::new(&name, plugin_cfg) {
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
        Commands::Graph => {
            handle_environment_graph(config, config_path, json_output).await?;
        }
        Commands::Link { branch_name, from } => {
            handle_link_command(
                config,
                config_path,
                &branch_name,
                from.as_deref(),
                json_output,
                non_interactive,
            )
            .await?;
        }
        Commands::Switch {
            branch_name,
            create,
            from,
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
                    let context = resolve_branch_context(config);
                    let default_parent = if create {
                        from.clone().or_else(|| context.context_branch_raw.clone())
                    } else {
                        None
                    };
                    let branch_exists = vcs::detect_vcs_provider(".")
                        .ok()
                        .and_then(|repo| repo.branch_exists(&branch).ok());

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
                            let wt_path = resolve_cd_target(&PathBuf::from(
                                path_template
                                    .replace("{repo}", &repo_name)
                                    .replace("{branch}", &branch),
                            ))?;
                            wt_path_value =
                                serde_json::Value::String(wt_path.display().to_string());
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
                                "parent": default_parent,
                                "branch_exists": branch_exists,
                                "services_skipped": no_services,
                                "auto_branch_services": auto_providers,
                                "hooks_skipped": no_verify,
                                "execute": execute,
                                "would_fail_without_create": branch_exists == Some(false) && !create,
                            }))?
                        );
                    } else {
                        println!("Dry run: would switch to branch: {}", normalized_branch);
                        if let Some(ref parent) = default_parent {
                            println!("  Parent branch: {}", parent);
                        }
                        if branch_exists == Some(false) && !create {
                            println!(
                                "  Note: branch does not exist; this would fail (use -c to create it)"
                            );
                        }
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
                            let wt_path = resolve_cd_target(&PathBuf::from(
                                path_template
                                    .replace("{repo}", &repo_name)
                                    .replace("{branch}", &branch),
                            ))?;
                            println!("  Worktree path: {}", wt_path.display());
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
                        from.as_deref(),
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
            // Top-level alias for `devflow service cleanup`
            return handle_service_provider_command(
                ServiceCommands::Cleanup { max_count },
                config,
                json_output,
                non_interactive,
                database_name,
                config_path,
            )
            .await;
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
                devflow_core::config::default_service_type()
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
            let named_cfg = devflow_core::config::NamedServiceConfig {
                name: name.clone(),
                provider_type: provider_type.clone(),
                service_type: service_type.clone(),
                auto_branch: devflow_core::config::default_auto_branch(),
                default: false,
                local: if is_local {
                    Some(devflow_core::config::LocalServiceConfig {
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
                    Some(devflow_core::config::ClickHouseConfig {
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
                    Some(devflow_core::config::MySQLConfig {
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
        ServiceCommands::Capabilities => {
            let has_multiple_services = config.resolve_services().len() > 1;
            if database_name.is_none() && has_multiple_services {
                return handle_multi_service_aggregation(
                    ServiceAggregation::Capabilities,
                    config,
                    json_output,
                    &config_path,
                )
                .await;
            }

            match services::factory::resolve_provider(config, database_name).await {
                Ok(named) => {
                    let caps = named.provider.capabilities();

                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "service": named.name,
                                "provider": named.provider.provider_name(),
                                "capabilities": caps,
                            }))?
                        );
                    } else {
                        println!(
                            "Service: {} ({})",
                            named.name,
                            named.provider.provider_name()
                        );
                        println!("  lifecycle: {}", if caps.lifecycle { "yes" } else { "no" });
                        println!("  logs: {}", if caps.logs { "yes" } else { "no" });
                        println!(
                            "  seed_from_source: {}",
                            if caps.seed_from_source { "yes" } else { "no" }
                        );
                        println!(
                            "  destroy_project: {}",
                            if caps.destroy_project { "yes" } else { "no" }
                        );
                        println!("  cleanup: {}", if caps.cleanup { "yes" } else { "no" });
                        println!(
                            "  template_from_time: {}",
                            if caps.template_from_time { "yes" } else { "no" }
                        );
                        println!("  max_branch_name_length: {}", caps.max_branch_name_length);
                    }
                }
                Err(e) => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&serde_json::json!({
                                "error": e.to_string(),
                                "services": null,
                            }))?
                        );
                    } else {
                        println!("No service provider available: {}", e);
                    }
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
    if matches!(&cmd, ServiceCommands::Cleanup { .. }) && config.resolve_services().is_empty() {
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": "ok",
                    "services": "none_configured",
                    "deleted": [],
                }))?
            );
        } else {
            println!("No services configured. Nothing to clean up.");
        }
        return Ok(());
    }

    // Orchestratable mutation commands: Create and Delete operate on all auto_branch services
    let is_orchestratable_mutation = matches!(
        &cmd,
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
        ServiceCommands::Cleanup { max_count } => {
            if !provider.supports_cleanup() {
                anyhow::bail!(
                    "Service '{}' does not support cleanup",
                    provider.provider_name()
                );
            }

            let max = max_count.unwrap_or(config.behavior.max_branches.unwrap_or(10));
            let deleted = provider.cleanup_old_branches(max).await?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "service": resolved_name,
                        "max_count": max,
                        "deleted": deleted,
                    }))?
                );
            } else if deleted.is_empty() {
                println!("No branches to clean up on service '{}'", resolved_name);
            } else {
                println!(
                    "Cleaned up {} branches on '{}': {}",
                    deleted.len(),
                    resolved_name,
                    deleted.join(", ")
                );
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
    _config_path: &Option<std::path::PathBuf>,
) -> Result<()> {
    // Show VCS info
    let vcs_info = vcs::detect_vcs_provider(".").ok().and_then(|vcs| {
        let branch = vcs.current_branch().ok()?;
        Some(serde_json::json!({
            "provider": vcs.provider_name(),
            "branch": branch,
        }))
    });

    let context = resolve_branch_context(config);
    let context_differs_from_cwd = |cwd: &str| {
        let Some(context_branch) = context.context_branch.as_deref() else {
            return false;
        };
        let normalized_cwd = config.get_normalized_branch_name(cwd);
        context.source == BranchContextSource::EnvOverride
            && context_branch != cwd
            && context_branch != normalized_cwd
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
                    "devflow_context_branch": context.context_branch.clone(),
                    "context_source": match context.source {
                        BranchContextSource::EnvOverride => "env",
                        BranchContextSource::Cwd => "cwd",
                        BranchContextSource::None => "none",
                    },
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
                if let Some(context_branch) = context.context_branch.as_deref() {
                    let cwd = info["branch"].as_str().unwrap_or("unknown");
                    if context_differs_from_cwd(cwd) {
                        println!("Devflow context branch: {}", context_branch);
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
                            "devflow_context_branch": context.context_branch.clone(),
                            "context_source": match context.source {
                                BranchContextSource::EnvOverride => "env",
                                BranchContextSource::Cwd => "cwd",
                                BranchContextSource::None => "none",
                            },
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
                        if let Some(context_branch) = context.context_branch.as_deref() {
                            let cwd = info["branch"].as_str().unwrap_or("unknown");
                            if context_differs_from_cwd(cwd) {
                                println!("Devflow context branch: {}", context_branch);
                            }
                        }
                        println!();
                    } else if let Some(context_branch) = context.context_branch.as_deref() {
                        if context.source == BranchContextSource::EnvOverride {
                            println!("Devflow context branch: {}", context_branch);
                            println!();
                        }
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
                            "devflow_context_branch": context.context_branch.clone(),
                            "context_source": match context.source {
                                BranchContextSource::EnvOverride => "env",
                                BranchContextSource::Cwd => "cwd",
                                BranchContextSource::None => "none",
                            },
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
                        if let Some(context_branch) = context.context_branch.as_deref() {
                            let cwd = info["branch"].as_str().unwrap_or("unknown");
                            if context_differs_from_cwd(cwd) {
                                println!("Devflow context branch: {}", context_branch);
                            }
                        }
                        println!();
                    } else if let Some(context_branch) = context.context_branch.as_deref() {
                        if context.source == BranchContextSource::EnvOverride {
                            println!("Devflow context branch: {}", context_branch);
                            println!();
                        }
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
    Capabilities,
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
                ServiceAggregation::Capabilities => {
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
        ServiceAggregation::Capabilities => {
            if json_output {
                let mut map = serde_json::Map::new();
                for named in &all_providers {
                    map.insert(
                        named.name.clone(),
                        serde_json::json!({
                            "provider": named.provider.provider_name(),
                            "capabilities": named.provider.capabilities(),
                        }),
                    );
                }
                println!("{}", serde_json::to_string_pretty(&map)?);
            } else {
                for named in &all_providers {
                    let caps = named.provider.capabilities();
                    println!("[{}] ({})", named.name, named.provider.provider_name());
                    println!(
                        "  lifecycle={} logs={} seed={} destroy={} cleanup={} template_from_time={} max_branch_name_length={}",
                        if caps.lifecycle { "yes" } else { "no" },
                        if caps.logs { "yes" } else { "no" },
                        if caps.seed_from_source { "yes" } else { "no" },
                        if caps.destroy_project { "yes" } else { "no" },
                        if caps.cleanup { "yes" } else { "no" },
                        if caps.template_from_time { "yes" } else { "no" },
                        caps.max_branch_name_length,
                    );
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
                        None,  // from
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
    let mut vcs_branch_names = std::collections::HashSet::new();

    // 1) VCS branches (authoritative source)
    if let Ok(vcs_repo) = vcs::detect_vcs_provider(".") {
        if let Ok(vcs_branches) = vcs_repo.list_branches() {
            for branch in vcs_branches {
                vcs_branch_names.insert(branch.name.clone());
                branch_names.insert(branch.name);
            }
        }
    }

    // 2) Devflow branch registry
    if let Some(path) = config_path.as_ref() {
        if let Ok(state) = LocalStateManager::new() {
            for branch in state.get_branches(path) {
                if vcs_branch_names.is_empty() || vcs_branch_names.contains(&branch.name) {
                    branch_names.insert(branch.name);
                }
            }
        }
    }

    // 3) Service branches (best effort)
    if !config.resolve_services().is_empty() {
        if let Ok(providers) = services::factory::create_all_providers(config).await {
            for named in providers {
                if let Ok(service_branches) = named.provider.list_branches().await {
                    for branch in service_branches {
                        if vcs_branch_names.is_empty() || vcs_branch_names.contains(&branch.name) {
                            branch_names.insert(branch.name);
                        }
                    }
                }
            }
        }
    }

    // Include configured main branch when visible in VCS (or if VCS probing failed).
    if vcs_branch_names.is_empty() || vcs_branch_names.contains(&config.git.main_branch) {
        branch_names.insert(config.git.main_branch.clone());
    }

    let context = resolve_branch_context(config);
    let current_git = context.cwd_branch.clone();

    // Create branch items with display info
    let mut branch_items: Vec<BranchItem> = branch_names
        .iter()
        .map(|branch| {
            let is_cwd = current_git.as_deref() == Some(branch.as_str());
            let is_context =
                context_matches_branch(config, context.context_branch.as_deref(), branch);

            BranchItem {
                name: branch.clone(),
                display_name: branch.clone(),
                is_cwd,
                is_context,
            }
        })
        .collect();

    // Add a "Create new branch" option at the end
    branch_items.push(BranchItem {
        name: "__create_new__".to_string(),
        display_name: "+ Create new branch".to_string(),
        is_cwd: false,
        is_context: false,
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
                    None,  // from
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
                    None,  // from
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
    is_context: bool,
}

fn run_interactive_selector(items: Vec<BranchItem>) -> Result<String, inquire::InquireError> {
    use inquire::Select;

    if items.is_empty() {
        return Err(inquire::InquireError::InvalidConfiguration(
            "No branches available".to_string(),
        ));
    }

    // Create display options with context/cwd markers.
    let options: Vec<String> = items
        .iter()
        .map(|item| {
            if item.is_context && item.is_cwd {
                format!("{} *", item.display_name)
            } else if item.is_context {
                format!("{} (context)", item.display_name)
            } else if item.is_cwd {
                format!("{} (cwd)", item.display_name)
            } else {
                item.display_name.clone()
            }
        })
        .collect();

    // Prefer context branch as default; fall back to cwd branch.
    let default = items
        .iter()
        .position(|item| item.is_context)
        .or_else(|| items.iter().position(|item| item.is_cwd));

    let mut select = Select::new("Select a branch to switch to:", options.clone())
        .with_help_message(
        "Use arrow keys to navigate, type to filter, Enter to select, Esc to cancel (*=context+cwd)",
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

#[derive(Debug, Clone)]
struct LinkServiceResult {
    service_name: String,
    success: bool,
    message: String,
}

#[derive(Debug, Clone)]
struct LinkBranchResult {
    branch: String,
    parent: Option<String>,
    worktree_path: Option<String>,
    service_results: Vec<LinkServiceResult>,
    services_failed: usize,
}

async fn link_branch_internal(
    config: &Config,
    config_path: &Option<PathBuf>,
    branch_name: &str,
    from: Option<&str>,
    non_interactive: bool,
) -> Result<LinkBranchResult> {
    let _ = non_interactive;
    ensure_default_branch_registered(config, config_path)?;

    let normalized_branch = config.get_normalized_branch_name(branch_name);
    let normalized_main = config.get_normalized_branch_name(&config.git.main_branch);

    let vcs_repo = vcs::detect_vcs_provider(".").context("Failed to open VCS repository")?;
    if !vcs_repo.branch_exists(branch_name)? {
        anyhow::bail!(
            "Branch '{}' does not exist in {}. Create/switch it first, then run `devflow link {}`.",
            branch_name,
            vcs_repo.provider_name(),
            branch_name
        );
    }

    let existing_parent = config_path
        .as_ref()
        .and_then(|path| {
            LocalStateManager::new()
                .ok()
                .and_then(|state| state.get_branch(path, &normalized_branch))
        })
        .and_then(|b| b.parent);

    let mut parent = from
        .map(|p| config.get_normalized_branch_name(p))
        .or(existing_parent);

    if parent.is_none() && normalized_branch != normalized_main {
        parent = Some(normalized_main.clone());
    }

    if let Some(ref parent_branch) = parent {
        if parent_branch != &normalized_main
            && !linked_branch_exists(config, config_path, parent_branch)
        {
            anyhow::bail!(
                "Parent '{}' is not linked in devflow. Run `devflow link {}` first.",
                parent_branch,
                parent_branch
            );
        }
        if parent_branch == &normalized_main {
            ensure_default_branch_registered(config, config_path)?;
        }
    }

    let worktree_path = vcs_repo
        .worktree_path(branch_name)?
        .map(|p| p.display().to_string())
        .or_else(|| {
            if normalized_branch == normalized_main {
                vcs_repo
                    .main_worktree_dir()
                    .map(|p| p.display().to_string())
            } else {
                None
            }
        });

    register_branch_in_state(
        config,
        config_path,
        branch_name,
        parent.as_deref(),
        worktree_path.clone(),
    )?;

    let mut service_results = Vec::new();
    let mut services_failed = 0usize;

    if !config.resolve_services().is_empty() {
        let orchestration =
            services::factory::orchestrate_switch(config, &normalized_branch, parent.as_deref())
                .await?;
        for result in orchestration {
            if !result.success {
                services_failed += 1;
            }
            service_results.push(LinkServiceResult {
                service_name: result.service_name,
                success: result.success,
                message: result.message,
            });
        }
    }

    Ok(LinkBranchResult {
        branch: normalized_branch,
        parent,
        worktree_path,
        service_results,
        services_failed,
    })
}

async fn handle_link_command(
    config: &Config,
    config_path: &Option<PathBuf>,
    branch_name: &str,
    from: Option<&str>,
    json_output: bool,
    non_interactive: bool,
) -> Result<()> {
    let linked =
        link_branch_internal(config, config_path, branch_name, from, non_interactive).await?;

    if json_output {
        let service_results: Vec<serde_json::Value> = linked
            .service_results
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
                "status": if linked.services_failed == 0 { "ok" } else { "error" },
                "branch": linked.branch,
                "parent": linked.parent,
                "worktree_path": linked.worktree_path,
                "services_failed": linked.services_failed,
                "service_results": service_results,
            }))?
        );
    } else {
        println!("Linked devflow branch: {}", linked.branch);
        if let Some(parent) = linked.parent.as_deref() {
            println!("  Parent: {}", parent);
        }
        if let Some(path) = linked.worktree_path.as_deref() {
            println!("  Worktree: {}", path);
        }

        if linked.service_results.is_empty() {
            println!("  Services: none configured");
        } else {
            for r in &linked.service_results {
                if r.success {
                    println!("  [{}] {}", r.service_name, r.message);
                } else {
                    println!("  [{}] Warning: {}", r.service_name, r.message);
                }
            }
        }
    }

    if linked.services_failed > 0 {
        anyhow::bail!(
            "Linked branch '{}' but failed on {}/{} service(s)",
            linked.branch,
            linked.services_failed,
            linked.service_results.len()
        );
    }

    Ok(())
}

async fn resolve_parent_for_branch_creation(
    config: &Config,
    config_path: &Option<PathBuf>,
    target_branch: &str,
    requested_parent: Option<&str>,
    context: &BranchContext,
    json_output: bool,
    non_interactive: bool,
) -> Result<Option<String>> {
    let mut parent = requested_parent
        .map(|p| p.to_string())
        .or_else(|| context.context_branch_raw.clone());

    let Some(parent_name) = parent.as_deref() else {
        return Ok(None);
    };

    let target_normalized = config.get_normalized_branch_name(target_branch);
    let parent_normalized = config.get_normalized_branch_name(parent_name);
    if parent_normalized == target_normalized {
        anyhow::bail!(
            "Parent branch '{}' resolves to the target branch '{}'. Choose a different --from value.",
            parent_name,
            target_branch
        );
    }

    // If we have no project config path, we cannot enforce branch-link checks.
    if config_path.is_none() {
        return Ok(parent);
    }

    if linked_branch_exists(config, config_path, parent_name) {
        return Ok(parent);
    }

    if json_output || non_interactive {
        anyhow::bail!(
            "Parent branch '{}' is not linked in devflow. Run `devflow link {}` first.",
            parent_name,
            parent_name
        );
    }

    let default_branch = config.git.main_branch.clone();
    let options = vec![
        format!("Link '{}' now (recommended)", parent_name),
        format!("Use default branch '{}' as parent", default_branch),
        "Cancel".to_string(),
    ];

    let choice = inquire::Select::new(
        "Parent branch is not linked in devflow. Choose how to proceed:",
        options,
    )
    .with_starting_cursor(0)
    .prompt()?;

    if choice.starts_with("Link '") {
        let linked = link_branch_internal(config, config_path, parent_name, None, false).await?;
        if linked.services_failed > 0 {
            anyhow::bail!(
                "Linked parent '{}' but failed on {}/{} service(s)",
                parent_name,
                linked.services_failed,
                linked.service_results.len()
            );
        }
        return Ok(parent);
    }

    if choice.starts_with("Use default branch") {
        if !linked_branch_exists(config, config_path, &default_branch) {
            match link_branch_internal(config, config_path, &default_branch, None, false).await {
                Ok(linked) if linked.services_failed == 0 => {}
                Ok(linked) => {
                    anyhow::bail!(
                        "Linked default branch '{}' but failed on {}/{} service(s)",
                        default_branch,
                        linked.services_failed,
                        linked.service_results.len()
                    );
                }
                Err(_) => {
                    // Fallback for repos where the default branch is not materialized yet.
                    ensure_default_branch_registered(config, config_path)?;
                }
            }
        }
        parent = Some(default_branch);
        return Ok(parent);
    }

    anyhow::bail!("Cancelled")
}

#[allow(clippy::too_many_arguments)]
async fn handle_switch_command(
    config: &Config,
    branch_name: &str,
    config_path: &Option<std::path::PathBuf>,
    create: bool,
    from: Option<&str>,
    no_services: bool,
    no_verify: bool,
    json_output: bool,
    non_interactive: bool,
) -> Result<()> {
    if let Err(e) = ensure_default_branch_registered(config, config_path) {
        log::warn!("Failed to ensure default branch registration: {}", e);
    }

    let worktree_enabled = config.worktree.as_ref().is_some_and(|wt| wt.enabled);
    let shell_integration = shell_integration_enabled();
    let mut worktree_path: Option<String> = None;
    let mut worktree_created = false;
    let mut branch_created = false;
    let mut parent_for_new_branch: Option<String> = None;
    let mut json_summary: Option<serde_json::Value> = None;

    let context = resolve_branch_context(config);

    // ── Worktree mode ──────────────────────────────────────────────────
    if worktree_enabled {
        let vcs_repo = vcs::detect_vcs_provider(".").context("Failed to open VCS repository")?;

        // Check if a worktree already exists for this branch
        let existing_path = vcs_repo.worktree_path(branch_name)?;

        if let Some(wt_path) = existing_path {
            let wt_path = resolve_cd_target(&wt_path)?;
            let wt_path = std::fs::canonicalize(&wt_path).unwrap_or(wt_path);
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
            let wt_path = resolve_cd_target(&PathBuf::from(&wt_path_str))?;

            // Create branch only when explicitly requested
            let branch_exists = vcs_repo.branch_exists(branch_name)?;
            if !branch_exists {
                if !create {
                    anyhow::bail!(
                        "Branch '{}' does not exist. Use `devflow switch -c {}` to create it.",
                        branch_name,
                        branch_name
                    );
                }

                let parent = resolve_parent_for_branch_creation(
                    config,
                    config_path,
                    branch_name,
                    from,
                    &context,
                    json_output,
                    non_interactive,
                )
                .await?;

                if !json_output {
                    println!(
                        "Creating branch '{}' (parent: {})",
                        branch_name,
                        parent.as_deref().unwrap_or("HEAD")
                    );
                }
                vcs_repo
                    .create_branch(branch_name, parent.as_deref())
                    .with_context(|| {
                        format!(
                            "Failed to create branch '{}' before worktree creation",
                            branch_name
                        )
                    })?;
                branch_created = true;
                parent_for_new_branch = parent;
            } else if create && !json_output {
                println!("Branch '{}' already exists; switching to it", branch_name);
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

            let wt_path_for_output = std::fs::canonicalize(&wt_path).unwrap_or(wt_path.clone());

            if !json_output {
                println!(
                    "Created worktree for '{}' at {}",
                    branch_name,
                    wt_path_for_output.display()
                );
            }
            worktree_path = Some(wt_path_for_output.display().to_string());
            worktree_created = true;
            if !json_output {
                println!("DEVFLOW_CD={}", wt_path_for_output.display());
                if !shell_integration {
                    print_manual_cd_hint(&wt_path_for_output);
                }
            }
        }
    } else {
        // ── Classic mode (no worktrees) ────────────────────────────────
        let vcs_repo = vcs::detect_vcs_provider(".").context("Failed to open VCS repository")?;
        let branch_exists = vcs_repo.branch_exists(branch_name)?;
        if !branch_exists {
            if !create {
                anyhow::bail!(
                    "Branch '{}' does not exist. Use `devflow switch -c {}` to create it.",
                    branch_name,
                    branch_name
                );
            }

            let parent = resolve_parent_for_branch_creation(
                config,
                config_path,
                branch_name,
                from,
                &context,
                json_output,
                non_interactive,
            )
            .await?;

            if !json_output {
                println!(
                    "Creating branch '{}' (parent: {})",
                    branch_name,
                    parent.as_deref().unwrap_or("HEAD")
                );
            }
            vcs_repo.create_branch(branch_name, parent.as_deref())?;
            branch_created = true;
            parent_for_new_branch = parent;
        } else if create && !json_output {
            println!("Branch '{}' already exists; switching to it", branch_name);
        }
        // Switch the working directory to the target branch
        if !json_output {
            println!("Checking out branch: {}", branch_name);
        }
        vcs_repo.checkout_branch(branch_name)?;
    }

    // ── Branch registration (unconditional — independent of services) ──
    let normalized_branch = config.get_normalized_branch_name(branch_name);
    let parent_for_registry = if branch_created {
        parent_for_new_branch.as_deref()
    } else {
        None
    };
    if let Err(e) = register_branch_in_state(
        config,
        config_path,
        branch_name,
        parent_for_registry,
        worktree_path.clone(),
    ) {
        log::warn!("Failed to register branch in devflow registry: {}", e);
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
            let service_parent = if branch_created {
                parent_for_new_branch
                    .as_deref()
                    .map(|p| config.get_normalized_branch_name(p))
            } else {
                config_path.as_ref().and_then(|path| {
                    LocalStateManager::new()
                        .ok()
                        .and_then(|state| state.get_branch(path, &normalized_branch))
                        .and_then(|b| b.parent)
                })
            };
            let results = services::factory::orchestrate_switch(
                config,
                &normalized_branch,
                service_parent.as_deref(),
            )
            .await?;

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
                    "parent": if branch_created {
                        parent_for_new_branch
                            .as_deref()
                            .map(|p| config.get_normalized_branch_name(p))
                    } else {
                        None
                    },
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
                    "parent": if branch_created {
                        parent_for_new_branch
                            .as_deref()
                            .map(|p| config.get_normalized_branch_name(p))
                    } else {
                        None
                    },
                    "worktree_path": worktree_path,
                    "worktree_created": worktree_created,
                    "services": "none_configured",
                }));
            } else {
                if worktree_enabled {
                    println!("Selected branch/worktree: {}", normalized_branch);
                } else {
                    println!("Switched git branch: {}", normalized_branch);
                }
                println!("  (no services configured — use 'devflow service add' to add one)");
            }
        }
    } else {
        // Services skipped (--no-services) — branch registration already done above
        if json_output {
            json_summary = Some(serde_json::json!({
                "branch": normalized_branch,
                "parent": if branch_created {
                    parent_for_new_branch
                        .as_deref()
                        .map(|p| config.get_normalized_branch_name(p))
                } else {
                    None
                },
                "worktree_path": worktree_path,
                "worktree_created": worktree_created,
                "services_skipped": true,
            }));
        } else {
            if worktree_enabled {
                println!(
                    "Selected branch/worktree (services skipped): {}",
                    normalized_branch
                );
            } else {
                println!(
                    "Switched git branch (services skipped): {}",
                    normalized_branch
                );
            }
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

    if let Err(e) = ensure_default_branch_registered(config, config_path) {
        log::warn!("Failed to ensure default branch registration: {}", e);
    }

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

    if let Err(e) = register_branch_in_state(
        config,
        config_path,
        main_branch,
        None,
        worktree_path.clone(),
    ) {
        log::warn!("Failed to register main branch in devflow registry: {}", e);
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

    // 4. Unregister the branch from local devflow registry
    if let Some(ref path) = config_path {
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
    config: &Config,
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
        generate_ai_commit_message(vcs.as_ref(), config, json_output).await?
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
///
/// Prefers external CLI command (e.g., `claude -p`, `llm`, `aichat`) if configured,
/// falling back to the OpenAI-compatible API.
#[cfg(feature = "llm")]
async fn generate_ai_commit_message(
    vcs: &dyn vcs::VcsProvider,
    config: &Config,
    _json_output: bool,
) -> Result<String> {
    use devflow_core::llm;

    let commit_gen_config = config.commit.as_ref().and_then(|c| c.generation.as_ref());
    let llm_config = llm::LlmConfig::from_config_and_env(commit_gen_config);

    // Prefer external CLI command
    if let Some(ref cmd) = llm_config.cli_command {
        let diff = vcs.staged_diff()?;
        let summary = vcs.staged_summary()?;
        eprintln!("Generating commit message via: {}...", cmd);
        return llm::generate_commit_message_via_cli(cmd, &diff, &summary).await;
    }

    // Fallback to API
    if !llm_config.is_configured() {
        anyhow::bail!(
            "LLM not configured. Either:\n\
             1. Set 'commit.generation.command' in .devflow.yml (e.g., \"claude -p --model=haiku\")\n\
             2. Set DEVFLOW_COMMIT_COMMAND env var\n\
             3. Set DEVFLOW_LLM_API_KEY for OpenAI-compatible API"
        );
    }

    let diff = vcs.staged_diff()?;
    let summary = vcs.staged_summary()?;
    eprintln!(
        "Generating commit message with {} ({})...",
        llm_config.model, llm_config.api_url
    );
    llm::generate_commit_message(&diff, &summary).await
}

#[cfg(not(feature = "llm"))]
async fn generate_ai_commit_message(
    _vcs: &dyn vcs::VcsProvider,
    _config: &Config,
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

/// Handle `devflow agent` subcommands.
async fn handle_agent_command(
    action: AgentCommands,
    config: &Config,
    json_output: bool,
    _non_interactive: bool,
    config_path: &Option<PathBuf>,
) -> Result<()> {
    match action {
        AgentCommands::Start {
            branch,
            command,
            prompt,
            dry_run,
        } => {
            let agent_config = config.agent.as_ref();
            let prefix = agent_config
                .map(|a| a.branch_prefix.as_str())
                .unwrap_or("agent/");

            let branch_name = if branch.starts_with(prefix) {
                branch.clone()
            } else {
                format!("{}{}", prefix, branch)
            };

            let agent_cmd = command
                .or_else(|| agent_config.and_then(|a| a.command.clone()))
                .or_else(|| std::env::var("DEVFLOW_AGENT_COMMAND").ok())
                .unwrap_or_else(|| "claude".to_string());

            let prompt_str = prompt.join(" ");

            if dry_run {
                if json_output {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "branch": branch_name,
                            "agent_command": agent_cmd,
                            "prompt": prompt_str,
                        }))?
                    );
                } else {
                    println!("Would create branch: {}", branch_name);
                    println!("Would launch agent:  {}", agent_cmd);
                    if !prompt_str.is_empty() {
                        println!("With prompt:         {}", prompt_str);
                    }
                }
                return Ok(());
            }

            // 1. Create the isolated branch + worktree via the switch handler
            if !json_output {
                println!("Creating isolated branch: {}", branch_name);
            }
            handle_switch_command(
                config,
                &branch_name,
                config_path,
                true,  // create
                None,  // from (defaults to current)
                false, // no_services
                true,  // no_verify (agent branches skip hooks)
                json_output,
                true,  // non_interactive
            )
            .await?;

            // 2. Record agent metadata in state
            if let Some(ref path) = config_path {
                if let Ok(mut state) = LocalStateManager::new() {
                    let normalized = config.get_normalized_branch_name(&branch_name);
                    if let Some(mut branch_state) = state.get_branch(path, &normalized) {
                        branch_state.agent_tool = Some(agent_cmd.clone());
                        branch_state.agent_status = Some("running".to_string());
                        branch_state.agent_started_at = Some(chrono::Utc::now());
                        if let Err(e) = state.register_branch(path, branch_state) {
                            log::warn!("Failed to record agent state: {}", e);
                        }
                    }
                }
            }

            // 3. Resolve the worktree path for the agent to work in
            let work_dir = vcs::detect_vcs_provider(".")
                .ok()
                .and_then(|repo| repo.worktree_path(&branch_name).ok().flatten())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

            // 4. Build the launch command with proper shell escaping
            let escaped_prompt = prompt_str.replace('\'', "'\\''");
            let launch_cmd = if prompt_str.is_empty() {
                agent_cmd.clone()
            } else {
                match agent_cmd.as_str() {
                    "claude" => {
                        format!("claude --dangerously-skip-permissions '{}'", escaped_prompt)
                    }
                    "codex" => format!("codex '{}'", escaped_prompt),
                    _ => format!("{} '{}'", agent_cmd, escaped_prompt),
                }
            };

            // 5. Launch in tmux if available, otherwise direct
            let has_tmux = std::process::Command::new("which")
                .arg("tmux")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);

            if has_tmux {
                let session_name = branch_name.replace('/', "-");
                if !json_output {
                    println!("Launching agent in tmux session: {}", session_name);
                }
                let tmux_status = std::process::Command::new("tmux")
                    .args([
                        "new-session",
                        "-d",
                        "-s",
                        &session_name,
                        "-c",
                        &work_dir.display().to_string(),
                        "sh",
                        "-c",
                        &launch_cmd,
                    ])
                    .status()
                    .context("Failed to launch tmux session")?;
                if !tmux_status.success() {
                    anyhow::bail!(
                        "tmux exited with code {}. Is session '{}' already running? Check: tmux ls",
                        tmux_status.code().unwrap_or(-1),
                        session_name
                    );
                }
                if json_output {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "branch": branch_name,
                            "agent_command": agent_cmd,
                            "tmux_session": session_name,
                            "worktree": work_dir.display().to_string(),
                        }))?
                    );
                } else {
                    println!(
                        "Agent running in tmux session '{}'. Attach with: tmux attach -t {}",
                        session_name, session_name
                    );
                }
            } else {
                if !json_output {
                    println!("Launching agent in: {}", work_dir.display());
                }
                let agent_status = std::process::Command::new("sh")
                    .args(["-c", &launch_cmd])
                    .current_dir(&work_dir)
                    .status()
                    .context("Failed to launch agent")?;
                if json_output {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "branch": branch_name,
                            "agent_command": agent_cmd,
                            "exit_code": agent_status.code(),
                            "worktree": work_dir.display().to_string(),
                        }))?
                    );
                }
            }

            Ok(())
        }

        AgentCommands::Status => {
            let state_manager = LocalStateManager::new()?;
            if let Some(ref path) = config_path {
                let branches = state_manager.get_branches(path);
                let agent_prefix = config
                    .agent
                    .as_ref()
                    .map(|a| a.branch_prefix.as_str())
                    .unwrap_or("agent/");

                let agent_branches: Vec<_> = branches
                    .iter()
                    .filter(|b| b.name.starts_with(agent_prefix))
                    .collect();

                if json_output {
                    let items: Vec<serde_json::Value> = agent_branches
                        .iter()
                        .map(|b| {
                            serde_json::json!({
                                "branch": b.name,
                                "created_at": b.created_at.to_rfc3339(),
                                "worktree_path": b.worktree_path,
                                "agent_tool": b.agent_tool,
                                "agent_status": b.agent_status,
                            })
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&items)?);
                } else if agent_branches.is_empty() {
                    println!("No active agent branches.");
                } else {
                    println!("Agent Branches:");
                    for b in agent_branches {
                        let tool = b
                            .agent_tool
                            .as_deref()
                            .unwrap_or("unknown");
                        let status = b
                            .agent_status
                            .as_deref()
                            .unwrap_or("unknown");
                        println!(
                            "  {} ({}, {}) — created {}",
                            b.name,
                            tool,
                            status,
                            b.created_at.format("%Y-%m-%d %H:%M")
                        );
                    }
                }
            } else {
                println!("No project configuration found.");
            }
            Ok(())
        }

        AgentCommands::Context { format, branch } => {
            let branch_name = if let Some(b) = branch {
                b
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

            let fmt = if json_output { "json" } else { format.as_str() };
            let output = devflow_core::agent::generate_agent_context(config, &branch_name, fmt).await?;
            println!("{}", output);
            Ok(())
        }

        AgentCommands::Skill { target } => {
            let project_dir = std::env::current_dir()?;
            let targets: Vec<&str> = if target == "all" {
                vec!["claude", "opencode", "cursor"]
            } else {
                vec![target.as_str()]
            };

            for t in targets {
                match t {
                    "claude" => {
                        let skill = devflow_core::agent::generate_claude_skill(config, &project_dir)?;
                        let skill_dir =
                            project_dir.join(".claude").join("skills").join("devflow");
                        std::fs::create_dir_all(&skill_dir)?;
                        let skill_path = skill_dir.join("SKILL.md");
                        std::fs::write(&skill_path, &skill)?;
                        if !json_output {
                            println!("Generated: {}", skill_path.display());
                        }
                    }
                    "opencode" => {
                        let content =
                            devflow_core::agent::generate_opencode_config(config, &project_dir)?;
                        let path = project_dir.join("AGENTS.md");
                        std::fs::write(&path, &content)?;
                        if !json_output {
                            println!("Generated: {}", path.display());
                        }
                    }
                    "cursor" => {
                        let rules =
                            devflow_core::agent::generate_cursor_rules(config, &project_dir)?;
                        let rules_dir = project_dir.join(".cursor").join("rules");
                        std::fs::create_dir_all(&rules_dir)?;
                        let rules_path = rules_dir.join("devflow.md");
                        std::fs::write(&rules_path, &rules)?;
                        if !json_output {
                            println!("Generated: {}", rules_path.display());
                        }
                    }
                    _ => {
                        eprintln!(
                            "Unknown target: {}. Use: claude, opencode, cursor, or all",
                            t
                        );
                    }
                }
            }

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({"generated": true}))?
                );
            }
            Ok(())
        }

        AgentCommands::Docs => {
            let project_dir = std::env::current_dir()?;
            let content = devflow_core::agent::generate_opencode_config(config, &project_dir)?;
            let path = project_dir.join("AGENTS.md");
            std::fs::write(&path, &content)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({"path": path.display().to_string()}))?
                );
            } else {
                println!("Generated: {}", path.display());
            }
            Ok(())
        }
    }
}

/// Return the first line of a message (for display).
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

/// Handle `devflow proxy` subcommands.
async fn handle_proxy_command(action: ProxyCommands, json_output: bool) -> Result<()> {
    match action {
        ProxyCommands::Start {
            daemon,
            https_port,
            http_port,
            api_port,
        } => {
            let config = devflow_proxy::ProxyConfig {
                https_port,
                http_port,
                api_port,
                domain_suffix: "localhost".to_string(),
            };

            if daemon {
                // Fork to background
                let exe = std::env::current_exe()?;
                let child = std::process::Command::new(exe)
                    .args([
                        "proxy",
                        "start",
                        "--https-port",
                        &https_port.to_string(),
                        "--http-port",
                        &http_port.to_string(),
                        "--api-port",
                        &api_port.to_string(),
                    ])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .context("Failed to spawn daemon process")?;

                let pid_path = devflow_proxy::ca::default_ca_cert_path()
                    .parent()
                    .unwrap()
                    .join("proxy.pid");
                std::fs::write(&pid_path, child.id().to_string())?;

                if json_output {
                    println!(
                        "{}",
                        serde_json::json!({
                            "status": "started",
                            "pid": child.id(),
                            "https_port": https_port,
                            "http_port": http_port,
                            "api_port": api_port,
                        })
                    );
                } else {
                    println!("Proxy started (pid: {})", child.id());
                    println!("  HTTPS: https://localhost:{}", https_port);
                    println!("  HTTP:  http://localhost:{}", http_port);
                    println!("  API:   http://localhost:{}", api_port);
                }
            } else {
                // Run in foreground
                println!("Starting devflow proxy...");
                println!("  HTTPS: 0.0.0.0:{}", https_port);
                println!("  HTTP:  0.0.0.0:{}", http_port);
                println!("  API:   127.0.0.1:{}", api_port);
                println!("Press Ctrl+C to stop");

                let handle = devflow_proxy::run_proxy(config).await?;

                // Wait for Ctrl+C
                tokio::signal::ctrl_c().await?;
                println!("\nShutting down proxy...");
                handle.stop();
                // Give servers a moment to shut down
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                println!("Proxy stopped.");
            }
        }
        ProxyCommands::Stop => {
            let pid_path = devflow_proxy::ca::default_ca_cert_path()
                .parent()
                .unwrap()
                .join("proxy.pid");

            if pid_path.exists() {
                let pid_str = std::fs::read_to_string(&pid_path)?;
                if let Ok(pid) = pid_str.trim().parse::<i32>() {
                    #[cfg(unix)]
                    {
                        use std::process::Command;
                        let _ = Command::new("kill").arg(pid.to_string()).status();
                    }
                    std::fs::remove_file(&pid_path)?;

                    if json_output {
                        println!("{}", serde_json::json!({"status": "stopped", "pid": pid}));
                    } else {
                        println!("Proxy stopped (pid: {})", pid);
                    }
                } else {
                    anyhow::bail!("Invalid PID file");
                }
            } else {
                if json_output {
                    println!("{}", serde_json::json!({"status": "not_running"}));
                } else {
                    println!("Proxy is not running (no PID file found)");
                }
            }
        }
        ProxyCommands::Status => {
            // Try to query the API
            let api_url = "http://127.0.0.1:2019/api/status";
            match reqwest_get_json(api_url).await {
                Ok(status) => {
                    if json_output {
                        println!("{}", status);
                    } else {
                        let running = status["running"].as_bool().unwrap_or(false);
                        let targets = status["targets"].as_u64().unwrap_or(0);
                        let ca_installed = status["ca_installed"].as_bool().unwrap_or(false);
                        println!("Proxy: {}", if running { "running" } else { "stopped" });
                        println!("Targets: {}", targets);
                        println!(
                            "CA: {}",
                            if ca_installed {
                                "installed"
                            } else {
                                "not installed"
                            }
                        );
                    }
                }
                Err(_) => {
                    if json_output {
                        println!("{}", serde_json::json!({"running": false}));
                    } else {
                        println!("Proxy is not running");
                    }
                }
            }
        }
        ProxyCommands::List => {
            let api_url = "http://127.0.0.1:2019/api/targets";
            match reqwest_get_json(api_url).await {
                Ok(targets) => {
                    if json_output {
                        println!("{}", targets);
                    } else if let Some(arr) = targets.as_array() {
                        if arr.is_empty() {
                            println!("No proxied containers");
                        } else {
                            println!(
                                "{:<40} {:<20} {:<10}",
                                "DOMAIN", "CONTAINER", "UPSTREAM"
                            );
                            for t in arr {
                                let domain = t["domain"].as_str().unwrap_or("-");
                                let name = t["container_name"].as_str().unwrap_or("-");
                                let ip = t["container_ip"].as_str().unwrap_or("-");
                                let port = t["port"].as_u64().unwrap_or(0);
                                println!(
                                    "{:<40} {:<20} {}:{}",
                                    format!("https://{}", domain),
                                    name,
                                    ip,
                                    port,
                                );
                            }
                        }
                    }
                }
                Err(_) => {
                    if json_output {
                        println!("[]");
                    } else {
                        println!("Proxy is not running");
                    }
                }
            }
        }
        ProxyCommands::Trust { action } => match action {
            TrustCommands::Install => {
                let ca = devflow_proxy::ca::CertificateAuthority::load_or_generate()?;
                devflow_proxy::platform::install_system_trust(&ca)?;
                println!("CA certificate installed to system trust store");
            }
            TrustCommands::Verify => {
                let trusted = devflow_proxy::platform::verify_system_trust()?;
                if json_output {
                    println!("{}", serde_json::json!({"trusted": trusted}));
                } else if trusted {
                    println!("CA certificate is trusted by the system");
                } else {
                    println!("CA certificate is NOT trusted. Run: devflow proxy trust install");
                }
            }
            TrustCommands::Remove => {
                devflow_proxy::platform::remove_system_trust()?;
                println!("CA certificate removed from system trust store");
            }
            TrustCommands::Info => {
                println!("{}", devflow_proxy::platform::trust_info());
            }
        },
    }

    Ok(())
}

/// Handle the `devflow gc` command — detect and clean up orphaned projects.
async fn handle_gc_command(
    list: bool,
    all: bool,
    force: bool,
    json_output: bool,
    non_interactive: bool,
) -> Result<()> {
    use devflow_core::services::orphan::{cleanup_orphan, detect_orphans, OrphanInfo, OrphanSource};

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
                    "sqlite_branch_count": o.sqlite_branch_count,
                    "container_names": o.container_names,
                    "local_state_service_count": o.local_state_service_count,
                    "local_state_branch_count": o.local_state_branch_count,
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
            if orphan.sqlite_branch_count > 0 {
                println!(
                    "     SQLite: {} branch{}",
                    orphan.sqlite_branch_count,
                    if orphan.sqlite_branch_count == 1 {
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
            if orphan.local_state_service_count > 0 || orphan.local_state_branch_count > 0 {
                println!(
                    "     Local state: {} service{}, {} branch{}",
                    orphan.local_state_service_count,
                    if orphan.local_state_service_count == 1 {
                        ""
                    } else {
                        "s"
                    },
                    orphan.local_state_branch_count,
                    if orphan.local_state_branch_count == 1 {
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
                anyhow::bail!(
                    "Use --force to confirm cleanup in non-interactive mode"
                );
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
                    if result.containers_removed == 1 { "" } else { "s" }
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
                    if result.data_dirs_removed == 1 { "" } else { "s" }
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
            if o.sqlite_branch_count > 0 {
                details.push(format!("{} sqlite branches", o.sqlite_branch_count));
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
        Err(inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted) => {
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

/// Simple HTTP GET returning JSON (for proxy API queries).
async fn reqwest_get_json(url: &str) -> Result<serde_json::Value> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()?;
    let resp = client.get(url).send().await?.json().await?;
    Ok(resp)
}

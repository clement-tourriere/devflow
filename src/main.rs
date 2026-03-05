use anyhow::Result;
use clap::{CommandFactory, Parser};

mod cli;
#[cfg(feature = "tui")]
mod tui;

use cli::Commands;

/// Full help template shown with `--help-all`.
const FULL_HELP_TEMPLATE: &str = "\
{name} {version}
Isolated dev environments for every workspace — automatically.

{usage-heading} {usage}

Getting Started:
  init                Initialize devflow (guided setup wizard)
  doctor              Check system health and configuration

Daily Use:
  switch              Create or switch workspaces (-c to create new)
  list                List all workspaces with service status
  status              Show current workspace and service info
  connection          Show service connection info
  remove              Remove a workspace and its services
  commit              Commit staged changes (--ai for AI message)

Workspace Management:
  graph               Render full environment graph (workspace tree + services)
  link                Link an existing workspace into devflow
  merge               Merge current workspace into target (with optional cleanup)
  cleanup             Clean up old service workspaces

Services:
  service add         Add a new service provider
  service remove      Remove a service provider configuration
  service list        List configured services
  service status      Show service status
  service capabilities Show service capability matrix
  service create      Create a new service workspace
  service delete      Delete a service workspace
  service cleanup     Clean up old workspaces for a service
  service start       Start a stopped workspace container (local provider)
  service stop        Stop a running workspace container (local provider)
  service reset       Reset a workspace to its parent state (local provider)
  service destroy     Destroy all workspaces and data for a service
  service connection  Show connection info for a service workspace
  service logs        Show container logs for a workspace
  service seed        Seed a workspace from an external source

Setup & Config:
  config              Show current configuration (-v for precedence details)
  destroy             Tear down the entire devflow project (inverse of init)
  install-hooks       Install Git hooks (auto workspace/switch on checkout)
  uninstall-hooks     Uninstall Git hooks
  shell-init          Print shell integration script (enables worktree cd)
  worktree-setup      Set up devflow in a Git worktree
  setup-zfs           Set up a file-backed ZFS pool for CoW storage (Linux)
  gc                  Detect and clean up orphaned projects (leftover state)

Extensibility:
  hook show           Show configured hooks (filter by phase)
  hook run            Run hooks for a phase manually
  hook approvals      Manage hook approvals (list, add, clear)
  hook explain        Explain hook phases and template variables
  hook vars           Show available template variables with current values
  hook render         Render a template string with current context
  plugin              Manage plugin services (list, check, init)

AI Agents:
  agent start         Start an AI agent in a new isolated workspace
  agent status        Show agent status across all workspaces
  agent context       Output project context for current workspace
  agent skill         Generate AI tool skills/rules for this project
  agent docs          Generate AGENTS.md for this project

Proxy:
  proxy start         Start the local reverse proxy (auto-HTTPS)
  proxy stop          Stop the proxy
  proxy status        Show proxy status
  proxy list          List proxied containers with HTTPS URLs
  proxy trust         Manage CA certificate trust (install/verify/remove)

Interactive:
  tui                 Launch the interactive terminal UI dashboard

Options:
{options}";

#[derive(Parser)]
#[command(name = "devflow")]
#[command(about = "Isolated dev environments for every workspace — automatically.")]
#[command(version = "0.4.0")]
#[command(disable_help_subcommand = true)]
#[command(help_template = "\
{name} {version}
Isolated dev environments for every workspace — automatically.

{usage-heading} {usage}

Getting Started:
  init                Initialize devflow (guided setup wizard)
  doctor              Check system health and configuration

Daily Use:
  switch              Create or switch workspaces (-c to create new)
  list                List all workspaces with service status
  status              Show current workspace and service info
  connection          Show service connection info
  remove              Remove a workspace and its services
  commit              Commit staged changes (--ai for AI message)

Interactive:
  tui                 Launch the interactive terminal UI dashboard

Run 'devflow <command> --help' for detailed usage.
Run 'devflow --help-all' for all commands (services, hooks, proxy, agents).

Options:
{options}")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Output in JSON format
    #[arg(long, global = true)]
    json: bool,

    /// Non-interactive mode (skip prompts, use defaults)
    #[arg(long, global = true)]
    non_interactive: bool,

    /// Target a specific named service (from configured services)
    #[arg(short = 's', long, global = true)]
    service: Option<String>,

    /// Show all commands including advanced (services, hooks, proxy, agents)
    #[arg(long = "help-all", global = true)]
    help_all: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let cli = Cli::parse();

    match cli.command {
        Some(cmd) => {
            cli::handle_command(cmd, cli.json, cli.non_interactive, cli.service.as_deref()).await?
        }
        None => {
            if cli.help_all {
                // Show full help with all commands visible
                let mut cmd = Cli::command();
                // Unhide all subcommands for the full help view
                let subcmds: Vec<clap::Command> = cmd
                    .get_subcommands()
                    .map(|s| s.clone().hide(false))
                    .collect();
                for sub in subcmds {
                    cmd = cmd.mut_subcommand(sub.get_name().to_string(), |_| sub.clone());
                }
                cmd = cmd.help_template(FULL_HELP_TEMPLATE);
                cmd.print_help()?;
            } else {
                // Print compact help when no command is provided
                let mut cmd = Cli::command();
                cmd.print_help()?;
            }
        }
    }

    Ok(())
}

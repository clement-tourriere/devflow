use anyhow::Result;
use clap::{CommandFactory, Parser};

mod cli;
#[cfg(feature = "tui")]
mod tui;

use cli::Commands;

#[derive(Parser)]
#[command(name = "devflow")]
#[command(about = "A universal development environment branching tool")]
#[command(version = "0.4.0")]
#[command(disable_help_subcommand = true)]
#[command(help_template = "\
{name} {version}
{about}

{usage-heading} {usage}

Branch Management:
  list                List all branches (with service + worktree status)
  graph               Render full environment graph (branch tree + services)
  link                Link an existing VCS branch into devflow
  switch              Switch to an existing branch/worktree (use -c to create)
  remove              Remove a branch, its worktree, and associated service branches
  merge               Merge current branch into target (with optional cleanup)
  cleanup             Clean up old service branches (alias for service cleanup)

Services:
  service add         Add a new service provider
  service remove      Remove a service provider configuration
  service list        List configured services
  service status      Show service status
  service capabilities Show service capability matrix
  service create      Create a new service branch
  service delete      Delete a service branch
  service cleanup     Clean up old branches for a service
  service start       Start a stopped branch container (local provider)
  service stop        Stop a running branch container (local provider)
  service reset       Reset a branch to its parent state (local provider)
  service destroy     Destroy all branches and data for a service
  service connection  Show connection info for a service branch
  service logs        Show container logs for a branch
  service seed        Seed a branch from an external source

Top-level Aliases:
  connection          Show connection info (alias for 'service connection')
  status              Show current project and service status

VCS:
  commit              Commit staged changes (--ai for AI-generated message)

Setup & Config:
  init                Initialize devflow configuration
  destroy             Tear down the entire devflow project (inverse of init)
  config              Show current configuration (-v for precedence details)
  doctor              Run diagnostics and check system health
  install-hooks       Install Git hooks (auto branch/switch on checkout)
  uninstall-hooks     Uninstall Git hooks
  shell-init          Print shell integration script (enables worktree cd)
  worktree-setup      Set up devflow in a Git worktree
  setup-zfs           Set up a file-backed ZFS pool for CoW storage (Linux)
  gc                  Detect and clean up orphaned projects (leftover state)

Extensibility:
  hook show             Show configured hooks (filter by phase)
  hook run              Run hooks for a phase manually
  hook approvals        Manage hook approvals (list, add, clear)
  hook explain          Explain hook phases and template variables
  hook vars             Show available template variables with current values
  hook render           Render a template string with current context
  plugin                Manage plugin services (list, check, init)

AI Agents:
  agent start         Start an AI agent in a new isolated branch
  agent status        Show agent status across all branches
  agent context       Output project context for current branch
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
            // Print help when no command is provided
            let mut cmd = Cli::command();
            cmd.print_help()?;
        }
    }

    Ok(())
}

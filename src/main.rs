use anyhow::Result;
use clap::{CommandFactory, Parser};

mod cli;
mod config;
#[cfg(feature = "service-postgres-template")]
mod database;
mod docker;
mod hooks;
#[cfg(feature = "llm")]
mod llm;
mod services;
mod state;
#[cfg(feature = "tui")]
mod tui;
mod vcs;

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
  switch              Switch to a branch (creates worktree/service branches if needed)
  remove              Remove a branch, its worktree, and associated service branches
  merge               Merge current branch into target (with optional cleanup)
  cleanup             Clean up old service branches

Services:
  service add         Add a new service provider
  service remove      Remove a service provider configuration
  service list        List configured services
  service status      Show service status
  service capabilities Show service capability matrix
  service create      Create a new service branch
  service delete      Delete a service branch
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

Extensibility:
  hook                Manage lifecycle hooks (show, run, approvals)
  plugin              Manage plugin services (list, check, init)

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

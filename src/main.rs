use anyhow::Result;
use clap::{CommandFactory, Parser};

mod cli;
mod config;
#[cfg(feature = "backend-postgres-template")]
mod database;
mod docker;
mod hooks;
#[cfg(feature = "llm")]
mod llm;
mod services;
mod state;
mod vcs;

use cli::Commands;

#[derive(Parser)]
#[command(name = "devflow")]
#[command(about = "A universal development environment branching tool")]
#[command(version = "0.3.0")]
#[command(disable_help_subcommand = true)]
#[command(help_template = "\
{name} {version}
{about}

{usage-heading} {usage}

Branch Management:
  create              Create a new service branch
  delete              Delete a service branch
  list                List all branches (with service + worktree status)
  switch              Switch to a branch (creates worktree/service branches if needed)
  remove              Remove a branch, its worktree, and associated service branches
  cleanup             Clean up old service branches

Branch Lifecycle (local backend):
  start               Start a stopped branch container
  stop                Stop a running branch container
  reset               Reset a branch to its parent state
  destroy             Destroy all branches and data for a service
  seed                Seed a branch from an external source

VCS:
  merge               Merge current branch into target (with optional cleanup)
  commit              Commit staged changes (--ai for AI-generated message)

Info:
  connection          Show connection info for a branch
  status              Show current project and backend status
  capabilities        Show machine-readable automation capabilities
  logs                Show container logs for a branch

Setup & Config:
  init                Initialize devflow configuration
  config              Show current configuration (-v for precedence details)
  doctor              Run diagnostics and check system health
  install-hooks       Install Git hooks (auto branch/switch on checkout)
  uninstall-hooks     Uninstall Git hooks
  shell-init          Print shell integration script (enables worktree cd)
  worktree-setup      Set up devflow in a Git worktree
  setup-zfs           Set up a file-backed ZFS pool for CoW storage (Linux)

Extensibility:
  hook                Manage lifecycle hooks (show, run, approvals)
  plugin              Manage plugin backends (list, check, init)

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

    /// Target a specific named database (from 'backends' config)
    #[arg(short = 'd', long, global = true)]
    database: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let cli = Cli::parse();

    match cli.command {
        Some(cmd) => {
            cli::handle_command(cmd, cli.json, cli.non_interactive, cli.database.as_deref()).await?
        }
        None => {
            // Print help when no command is provided
            let mut cmd = Cli::command();
            cmd.print_help()?;
        }
    }

    Ok(())
}

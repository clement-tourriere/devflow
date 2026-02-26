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
  create              Create a new database branch
  delete              Delete a database branch
  list                List all database branches
  switch              Switch to a database branch (creates if doesn't exist)
  cleanup             Clean up old database branches

Branch Lifecycle (local backend):
  start               Start a stopped database branch container
  stop                Stop a running database branch container
  reset               Reset a database branch to its parent state
  destroy             Destroy a database and all its branches

Info:
  connection          Show connection info for a database branch
  status              Show current project and backend status

Setup & Config:
  init                Initialize devflow configuration
  config              Show current configuration (-v for precedence details)
  doctor              Run diagnostics and check system health
  install-hooks       Install Git hooks
  uninstall-hooks     Uninstall Git hooks
  worktree-setup      Set up devflow in a Git worktree

VCS:
  commit              Commit staged changes (--ai for AI-generated message)

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

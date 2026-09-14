//! Optional proxy companion with its own small command tree.
mod proxy_commands;
use proxy_commands::{ProxyCommands, TrustCommands};
#[path = "cli/proxy.rs"]
mod proxy;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "devflow-proxy",
    version,
    about = "Optional local reverse proxy for devflow"
)]
struct Cli {
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true)]
    non_interactive: bool,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Proxy {
        #[command(subcommand)]
        action: ProxyCommands,
    },
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let cli = Cli::parse();
    let Command::Proxy { action } = cli.command;
    if let Err(error) = proxy::handle_proxy_command(action, cli.json).await {
        if cli.json {
            println!("{}", serde_json::json!({"error":format!("{error:#}")}));
        } else {
            eprintln!("{error:#}");
        }
        std::process::exit(1);
    }
}

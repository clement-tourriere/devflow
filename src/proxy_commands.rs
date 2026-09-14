use clap::Subcommand;

/// Subcommands for `devflow proxy`.
#[derive(Subcommand)]
pub enum ProxyCommands {
    #[command(about = "Start the reverse proxy")]
    Start {
        #[arg(long, help = "Run as a background daemon")]
        daemon: bool,
        #[arg(long, help = "HTTPS listen port [default: 443]")]
        https_port: Option<u16>,
        #[arg(long, help = "HTTP listen port [default: 80]")]
        http_port: Option<u16>,
        #[arg(long, help = "API listen port [default: 2019]")]
        api_port: Option<u16>,
        #[arg(
            long,
            help = "Domain suffix for auto-discovered containers [default: localhost]"
        )]
        domain_suffix: Option<String>,
        #[arg(
            long,
            help = "Disable auto-connecting containers to shared devflow network"
        )]
        no_auto_network: bool,
        #[arg(
            long,
            help = "Disable mDNS advertising of friendly .local names (macOS)"
        )]
        no_mdns: bool,
    },
    #[command(about = "Stop the reverse proxy")]
    Stop,
    #[command(about = "Show proxy status")]
    Status,
    #[command(about = "List proxied containers and devflow processes")]
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

#[cfg(not(feature = "proxy"))]
impl ProxyCommands {
    pub fn companion_args(&self) -> Vec<String> {
        match self {
            Self::Start {
                daemon,
                https_port,
                http_port,
                api_port,
                domain_suffix,
                no_auto_network,
                no_mdns,
            } => {
                let mut args = vec!["start".into()];
                for (enabled, flag) in [
                    (*daemon, "--daemon"),
                    (*no_auto_network, "--no-auto-network"),
                    (*no_mdns, "--no-mdns"),
                ] {
                    if enabled {
                        args.push(flag.into());
                    }
                }
                for (value, flag) in [
                    (https_port, "--https-port"),
                    (http_port, "--http-port"),
                    (api_port, "--api-port"),
                ] {
                    if let Some(value) = value {
                        args.extend([flag.into(), value.to_string()]);
                    }
                }
                if let Some(value) = domain_suffix {
                    args.extend(["--domain-suffix".into(), value.clone()]);
                }
                args
            }
            Self::Status => vec!["status".into()],
            Self::Stop => vec!["stop".into()],
            Self::List => vec!["list".into()],
            Self::Trust { action } => vec![
                "trust".into(),
                match action {
                    TrustCommands::Install => "install",
                    TrustCommands::Remove => "remove",
                    TrustCommands::Verify => "verify",
                    TrustCommands::Info => "info",
                }
                .into(),
            ],
        }
    }
}

pub mod api;
pub mod ca;
pub mod discovery;
pub mod endpoint;
pub mod mdns;
pub mod monitor;
pub mod network;
pub mod nss;
pub mod platform;
pub mod router;
pub mod server;
pub mod tls;

use anyhow::{Context, Result};
use ca::CertificateCache;
use discovery::{extract_network_domains, extract_proxy_targets};
use monitor::DockerMonitor;
use router::Router;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};

/// Proxy configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    #[serde(default = "default_https_port")]
    pub https_port: u16,
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    #[serde(default = "default_api_port")]
    pub api_port: u16,
    #[serde(default = "default_domain_suffix")]
    pub domain_suffix: String,
    #[serde(default = "default_auto_network")]
    pub auto_network: bool,
    #[serde(default = "default_mdns")]
    pub mdns: bool,
}

fn default_https_port() -> u16 {
    443
}
fn default_http_port() -> u16 {
    80
}
fn default_api_port() -> u16 {
    2019
}
fn default_domain_suffix() -> String {
    // `.local` on every platform, so the SAME name works inside and outside
    // containers:
    // - From the host, the mDNS responder (Bonjour on macOS, Avahi on Linux)
    //   resolves it with no hosts/resolver edits.
    // - Inside containers, Docker's embedded DNS resolves it via the network
    //   aliases devflow registers.
    // `.localhost` cannot satisfy "same name everywhere": RFC 6761 lets client
    // runtimes (musl, Node, browsers, …) resolve `*.localhost` to loopback
    // WITHOUT consulting DNS, so inside a container the name short-circuits to
    // the container itself instead of the target. Use
    // `--domain-suffix localhost` to opt into the loopback-only behavior.
    "local".to_string()
}
fn default_auto_network() -> bool {
    true
}
fn default_mdns() -> bool {
    true
}
impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            https_port: default_https_port(),
            http_port: default_http_port(),
            api_port: default_api_port(),
            domain_suffix: default_domain_suffix(),
            auto_network: default_auto_network(),
            mdns: default_mdns(),
        }
    }
}

/// Handle to a running proxy — can be used to stop it.
pub struct ProxyHandle {
    shutdown_tx: watch::Sender<bool>,
}

impl ProxyHandle {
    /// Stop the proxy gracefully.
    pub fn stop(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

/// Start the proxy and return a handle to control it.
pub async fn run_proxy(config: ProxyConfig) -> Result<ProxyHandle> {
    // Install rustls crypto provider (required by rustls 0.23+)
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Load or generate CA
    let ca = ca::CertificateAuthority::load_or_generate()?;
    let ca = Arc::new(ca);
    let cert_cache = Arc::new(CertificateCache::new(ca.clone()));

    // Create router
    let router = Router::new();

    // Create the mDNS responder so friendly `.local` names resolve from the host
    // (HTTP services -> 127.0.0.1, database endpoints -> container IP).
    let mdns = if config.mdns {
        Some(Arc::new(mdns::MdnsResponder::new()))
    } else {
        None
    };

    // Create shutdown signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Create Docker monitor
    let docker_monitor = DockerMonitor::new()?;

    // Load initial containers
    log::info!("Discovering running containers...");
    let mut initial_containers: Vec<(String, Vec<String>)> = Vec::new();
    match docker_monitor.get_running_containers().await {
        Ok(containers) => {
            for container in &containers {
                let domains = extract_network_domains(container, &config.domain_suffix);
                if !domains.is_empty() {
                    let container_id = container.id.clone().unwrap_or_default();
                    let aliases = network::strip_suffix_aliases(&domains, &config.domain_suffix);
                    initial_containers.push((container_id, aliases));
                }

                let targets = extract_proxy_targets(container, &config.domain_suffix);
                for target in targets {
                    log::info!(
                        "  {} -> {}:{}",
                        target.domain,
                        target.container_ip,
                        target.port
                    );
                    router.upsert(target).await;
                }
            }
            log::info!("Discovered {} containers", containers.len());
        }
        Err(e) => {
            log::warn!("Failed to discover containers: {}", e);
        }
    }

    // Advertise `.local` records for everything discovered so far.
    if let Some(mdns) = &mdns {
        mdns.reconcile(&router.list().await);
    }

    // Set up shared Docker network for container-to-container resolution
    let auto_network = config.auto_network;
    let docker_client = docker_monitor.docker_client();
    if auto_network {
        if let Err(e) = network::ensure_network(&docker_client).await {
            log::warn!("Failed to create devflow network: {}", e);
        }

        // Connect pre-existing containers to the network
        for (container_id, aliases) in &initial_containers {
            if let Err(e) = network::connect_container(&docker_client, container_id, aliases).await
            {
                log::warn!(
                    "Failed to connect {} to devflow network: {}",
                    &container_id[..12.min(container_id.len())],
                    e
                );
            }
        }
    }

    // Start Docker event monitor
    let (events_tx, mut events_rx) = mpsc::channel(100);
    let monitor_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        if let Err(e) = docker_monitor.start(monitor_shutdown, events_tx).await {
            log::error!("Docker monitor error: {}", e);
        }
    });

    // Process container events (update routing table)
    let router_for_events = router.clone();
    let domain_suffix = config.domain_suffix.clone();
    let events_shutdown = shutdown_rx.clone();
    let docker_for_events = docker_client.clone();
    let mdns_for_events = mdns.clone();
    tokio::spawn(async move {
        let mut shutdown = events_shutdown;
        loop {
            tokio::select! {
                event = events_rx.recv() => {
                    match event {
                        Some(event) => {
                            let container_id = event.container.id.clone().unwrap_or_default();

                            match event.action.as_str() {
                                "start" => {
                                    let domains = extract_network_domains(&event.container, &domain_suffix);
                                    let aliases = network::strip_suffix_aliases(&domains, &domain_suffix);

                                    let targets = extract_proxy_targets(&event.container, &domain_suffix);
                                    for target in targets {
                                        log::info!("+ {} -> {}:{}", target.domain, target.container_ip, target.port);
                                        if let Some(mdns) = &mdns_for_events {
                                            mdns.advertise(&target);
                                        }
                                        router_for_events.upsert(target).await;
                                    }
                                    if auto_network && !aliases.is_empty() {
                                        if let Err(e) = network::connect_container(&docker_for_events, &container_id, &aliases).await {
                                            log::warn!("Failed to connect {} to devflow network: {}", &container_id[..12.min(container_id.len())], e);
                                        }
                                    }
                                }
                                "stop" | "die" | "destroy" => {
                                    router_for_events.remove_by_container(&container_id).await;
                                    if let Some(mdns) = &mdns_for_events {
                                        mdns.remove_by_container(&container_id);
                                    }
                                    let name = event.container.name.as_deref().unwrap_or(&container_id);
                                    log::info!("- removed routes for {}", name);
                                }
                                "reconnected" => {
                                    // The Docker event stream was lost and re-established:
                                    // rebuild the routing table from what is actually running.
                                    log::info!("Docker reconnected — reconciling routes");
                                    match monitor::list_running_containers(&docker_for_events).await {
                                        Ok(containers) => {
                                            let mut targets = Vec::new();
                                            for c in &containers {
                                                targets.extend(extract_proxy_targets(c, &domain_suffix));
                                            }
                                            router_for_events.replace_all(targets).await;
                                            if let Some(mdns) = &mdns_for_events {
                                                mdns.reconcile(&router_for_events.list().await);
                                            }
                                            log::info!(
                                                "Reconciled {} route(s) after reconnect",
                                                router_for_events.len().await
                                            );
                                        }
                                        Err(e) => log::warn!("Route reconcile after reconnect failed: {}", e),
                                    }
                                }
                                _ => {}
                            }
                        }
                        None => break,
                    }
                }
                _ = shutdown.changed() => break,
            }
        }
    });

    // Bind all listeners eagerly so callers get a real error (port already
    // in use, permission denied on 443) instead of a successful-looking
    // handle with nothing listening behind it.
    let https_addr: SocketAddr = format!("0.0.0.0:{}", config.https_port).parse()?;
    let https_listener = tokio::net::TcpListener::bind(https_addr)
        .await
        .with_context(|| format!("Failed to bind HTTPS on {}", https_addr))?;
    let http_addr: SocketAddr = format!("0.0.0.0:{}", config.http_port).parse()?;
    let http_listener = tokio::net::TcpListener::bind(http_addr)
        .await
        .with_context(|| format!("Failed to bind HTTP on {}", http_addr))?;
    let api_addr: SocketAddr = format!("127.0.0.1:{}", config.api_port).parse()?;
    let api_listener = tokio::net::TcpListener::bind(api_addr)
        .await
        .with_context(|| format!("Failed to bind API on {}", api_addr))?;

    // Start HTTPS server
    let https_router = router.clone();
    let https_ca = ca.clone();
    let https_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        if let Err(e) =
            server::run_https_server(https_listener, https_router, https_ca, https_shutdown).await
        {
            log::error!("HTTPS server error: {}", e);
        }
    });

    // Start HTTP server
    let http_router = router.clone();
    let http_shutdown = shutdown_rx.clone();
    let https_port = config.https_port;
    tokio::spawn(async move {
        if let Err(e) =
            server::run_http_server(http_listener, https_port, http_router, http_shutdown).await
        {
            log::error!("HTTP server error: {}", e);
        }
    });

    // Start API server
    let api_router = router.clone();
    let api_cache = cert_cache.clone();
    let api_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        if let Err(e) = api::run_api_server(
            api_listener,
            api_router,
            api_cache,
            https_port,
            config.http_port,
            api_shutdown,
        )
        .await
        {
            log::error!("API server error: {}", e);
        }
    });

    log::info!(
        "Proxy started — HTTPS:{} HTTP:{} API:{}{}{}",
        config.https_port,
        config.http_port,
        config.api_port,
        if auto_network {
            format!(" Network:{}", network::DEVFLOW_NETWORK)
        } else {
            String::new()
        },
        if mdns.is_some() {
            format!(" mDNS:*.{}", config.domain_suffix)
        } else {
            String::new()
        },
    );

    Ok(ProxyHandle { shutdown_tx })
}

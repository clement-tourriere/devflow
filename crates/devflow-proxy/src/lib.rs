pub mod api;
pub mod ca;
pub mod discovery;
pub mod monitor;
pub mod platform;
pub mod router;
pub mod server;
pub mod tls;

use anyhow::Result;
use ca::CertificateCache;
use discovery::extract_proxy_targets;
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
    "localhost".to_string()
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            https_port: default_https_port(),
            http_port: default_http_port(),
            api_port: default_api_port(),
            domain_suffix: default_domain_suffix(),
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

    // Create shutdown signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Create Docker monitor
    let docker_monitor = DockerMonitor::new()?;

    // Load initial containers
    log::info!("Discovering running containers...");
    match docker_monitor.get_running_containers().await {
        Ok(containers) => {
            for container in &containers {
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
                                    let targets = extract_proxy_targets(&event.container, &domain_suffix);
                                    for target in targets {
                                        log::info!("+ {} -> {}:{}", target.domain, target.container_ip, target.port);
                                        router_for_events.upsert(target).await;
                                    }
                                }
                                "stop" | "die" => {
                                    router_for_events.remove_by_container(&container_id).await;
                                    let name = event.container.name.as_deref().unwrap_or(&container_id);
                                    log::info!("- removed routes for {}", name);
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

    // Start HTTPS server
    let https_addr: SocketAddr = format!("0.0.0.0:{}", config.https_port).parse()?;
    let https_router = router.clone();
    let https_ca = ca.clone();
    let https_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        if let Err(e) =
            server::run_https_server(https_addr, https_router, https_ca, https_shutdown).await
        {
            log::error!("HTTPS server error: {}", e);
        }
    });

    // Start HTTP server
    let http_addr: SocketAddr = format!("0.0.0.0:{}", config.http_port).parse()?;
    let http_router = router.clone();
    let http_shutdown = shutdown_rx.clone();
    let https_port = config.https_port;
    tokio::spawn(async move {
        if let Err(e) =
            server::run_http_server(http_addr, https_port, http_router, http_shutdown).await
        {
            log::error!("HTTP server error: {}", e);
        }
    });

    // Start API server
    let api_addr: SocketAddr = format!("127.0.0.1:{}", config.api_port).parse()?;
    let api_router = router.clone();
    let api_cache = cert_cache.clone();
    let api_shutdown = shutdown_rx.clone();
    tokio::spawn(async move {
        if let Err(e) = api::run_api_server(
            api_addr,
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
        "Proxy started — HTTPS:{} HTTP:{} API:{}",
        config.https_port,
        config.http_port,
        config.api_port
    );

    Ok(ProxyHandle { shutdown_tx })
}

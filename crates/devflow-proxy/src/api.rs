use crate::ca::{default_ca_cert_path, CertificateCache};
use crate::platform;
use crate::router::Router;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use std::sync::Arc;
use tokio::net::TcpListener;

type BoxBody = Full<Bytes>;

#[derive(Serialize)]
struct StatusResponse {
    running: bool,
    targets: usize,
    https_port: u16,
    http_port: u16,
    ca_installed: bool,
}

#[derive(Serialize)]
struct CaResponse {
    cert_path: String,
    installed: bool,
    info: String,
}

/// Run the API server for proxy management on an already-bound listener.
pub async fn run_api_server(
    listener: TcpListener,
    router: Arc<Router>,
    cert_cache: Arc<CertificateCache>,
    https_port: u16,
    http_port: u16,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    if let Ok(addr) = listener.local_addr() {
        log::info!("API server listening on {}", addr);
    }

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = match result {
                    Ok(pair) => pair,
                    Err(e) => {
                        log::warn!("API accept error: {}", e);
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let router = router.clone();
                let cert_cache = cert_cache.clone();

                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let router = router.clone();
                    let cert_cache = cert_cache.clone();

                    let service = service_fn(move |req: Request<Incoming>| {
                        let router = router.clone();
                        let cert_cache = cert_cache.clone();
                        async move {
                            handle_api(req, &router, &cert_cache, https_port, http_port).await
                        }
                    });

                    if let Err(e) = http1::Builder::new()
                        .serve_connection(io, service)
                        .await
                    {
                        log::debug!("API connection error: {}", e);
                    }
                });
            }
            _ = shutdown.changed() => {
                log::info!("API server shutting down");
                break;
            }
        }
    }

    Ok(())
}

async fn handle_api(
    req: Request<Incoming>,
    router: &Router,
    _cert_cache: &CertificateCache,
    https_port: u16,
    http_port: u16,
) -> Result<Response<BoxBody>, hyper::Error> {
    let path = req.uri().path();

    match path {
        "/api/status" => {
            let targets = router.len().await;
            let ca_installed = platform::verify_system_trust().unwrap_or(false);
            let resp = StatusResponse {
                running: true,
                targets,
                https_port,
                http_port,
                ca_installed,
            };
            json_response(&resp)
        }
        "/api/targets" => {
            let targets = router.list().await;
            json_response(&targets)
        }
        "/api/ca" => {
            let ca_installed = platform::verify_system_trust().unwrap_or(false);
            let resp = CaResponse {
                cert_path: default_ca_cert_path().display().to_string(),
                installed: ca_installed,
                info: platform::trust_info(),
            };
            json_response(&resp)
        }
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(r#"{"error":"not found"}"#)))
            .unwrap()),
    }
}

fn json_response<T: Serialize>(data: &T) -> Result<Response<BoxBody>, hyper::Error> {
    let body = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
    // No `Access-Control-Allow-Origin` header: the API is consumed by local
    // non-browser clients (CLI, Tauri Rust backend), which don't need CORS.
    // Emitting `*` would let any website the user visits enumerate running
    // containers via `fetch('http://127.0.0.1:2019/api/targets')`. If a
    // browser client ever needs access, add a loopback-origin allowlist
    // rather than a blanket `*`.
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

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
use std::net::SocketAddr;
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

/// Run the API server for proxy management.
pub async fn run_api_server(
    addr: SocketAddr,
    router: Arc<Router>,
    cert_cache: Arc<CertificateCache>,
    https_port: u16,
    http_port: u16,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    log::info!("API server listening on {}", addr);

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result?;
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
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .header("Access-Control-Allow-Origin", "*")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

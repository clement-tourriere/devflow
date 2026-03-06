use crate::ca::CertificateAuthority;
use crate::router::Router;
use crate::tls::SnsCertResolver;
use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

type BoxBody = http_body_util::Full<Bytes>;

/// Run the HTTPS reverse proxy server.
pub async fn run_https_server(
    addr: SocketAddr,
    router: Arc<Router>,
    ca: Arc<CertificateAuthority>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let resolver = Arc::new(SnsCertResolver::new(ca));

    // Pre-generate certs for all known routes
    for target in router.list().await {
        if let Err(e) = resolver.ensure_cert(&target.domain) {
            log::warn!("Failed to pre-generate cert for {}: {}", target.domain, e);
        }
    }

    let tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(resolver.clone());

    let tls_acceptor = TlsAcceptor::from(Arc::new(tls_config));

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("Failed to bind HTTPS on {}", addr))?;

    log::info!("HTTPS proxy listening on {}", addr);

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, peer_addr) = result?;
                let tls_acceptor = tls_acceptor.clone();
                let router = router.clone();
                let resolver = resolver.clone();

                tokio::spawn(async move {
                    match tls_acceptor.accept(stream).await {
                        Ok(tls_stream) => {
                            // Extract SNI hostname from the connection
                            let sni_hostname = tls_stream
                                .get_ref()
                                .1
                                .server_name()
                                .map(|s| s.to_string());

                            let io = TokioIo::new(tls_stream);
                            let router = router.clone();
                            let _resolver = resolver.clone();

                            let service = service_fn(move |req| {
                                let router = router.clone();
                                let sni = sni_hostname.clone();
                                async move {
                                    handle_request(req, &router, sni.as_deref(), peer_addr).await
                                }
                            });

                            if let Err(e) = http1::Builder::new()
                                .serve_connection(io, service)
                                .await
                            {
                                log::debug!("HTTPS connection error from {}: {}", peer_addr, e);
                            }
                        }
                        Err(e) => {
                            log::debug!("TLS handshake failed from {}: {}", peer_addr, e);
                        }
                    }
                });
            }
            _ = shutdown.changed() => {
                log::info!("HTTPS server shutting down");
                break;
            }
        }
    }

    Ok(())
}

/// Run the HTTP server (redirects to HTTPS or serves plain).
pub async fn run_http_server(
    addr: SocketAddr,
    https_port: u16,
    router: Arc<Router>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("Failed to bind HTTP on {}", addr))?;

    log::info!("HTTP proxy listening on {}", addr);

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, peer_addr) = result?;
                let router = router.clone();

                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let router = router.clone();

                    let service = service_fn(move |req: Request<Incoming>| {
                        let router = router.clone();
                        async move {
                            // If it has a Host that we know about, redirect to HTTPS
                            let host = req
                                .headers()
                                .get(hyper::header::HOST)
                                .and_then(|h| h.to_str().ok())
                                .map(|h| h.split(':').next().unwrap_or(h).to_string());

                            if let Some(ref hostname) = host {
                                if router.resolve(hostname).await.is_some() {
                                    let port_suffix = if https_port == 443 {
                                        String::new()
                                    } else {
                                        format!(":{}", https_port)
                                    };
                                    let location = format!(
                                        "https://{}{}{}",
                                        hostname,
                                        port_suffix,
                                        req.uri().path_and_query().map(|pq| pq.as_str()).unwrap_or("/")
                                    );
                                    return Ok::<_, hyper::Error>(
                                        Response::builder()
                                            .status(StatusCode::MOVED_PERMANENTLY)
                                            .header(hyper::header::LOCATION, location)
                                            .body(Full::new(Bytes::new()))
                                            .unwrap(),
                                    );
                                }
                            }

                            // Otherwise proxy as HTTP
                            handle_request(req, &router, host.as_deref(), peer_addr).await
                        }
                    });

                    if let Err(e) = http1::Builder::new()
                        .serve_connection(io, service)
                        .await
                    {
                        log::debug!("HTTP connection error from {}: {}", peer_addr, e);
                    }
                });
            }
            _ = shutdown.changed() => {
                log::info!("HTTP server shutting down");
                break;
            }
        }
    }

    Ok(())
}

/// Handle a single proxied request by forwarding to the upstream.
async fn handle_request(
    req: Request<Incoming>,
    router: &Router,
    sni_hostname: Option<&str>,
    _peer_addr: SocketAddr,
) -> Result<Response<BoxBody>, hyper::Error> {
    // Determine the target host from SNI or Host header
    let host = sni_hostname.map(|s| s.to_string()).or_else(|| {
        req.headers()
            .get(hyper::header::HOST)
            .and_then(|h| h.to_str().ok())
            .map(|h| h.split(':').next().unwrap_or(h).to_string())
    });

    let hostname = match host {
        Some(h) => h,
        None => {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Full::new(Bytes::from("Missing Host header")))
                .unwrap());
        }
    };

    let upstream = match router.resolve(&hostname).await {
        Some(u) => u,
        None => {
            return Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Full::new(Bytes::from(format!(
                    "No upstream found for {}",
                    hostname
                ))))
                .unwrap());
        }
    };

    // Forward the request to the upstream container
    let upstream_uri = format!(
        "http://{}:{}{}",
        upstream.ip,
        upstream.port,
        req.uri()
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/")
    );

    // Build a TCP connection to the upstream
    let upstream_addr = format!("{}:{}", upstream.ip, upstream.port);
    let stream = match tokio::net::TcpStream::connect(&upstream_addr).await {
        Ok(s) => s,
        Err(e) => {
            log::warn!("Failed to connect to upstream {}: {}", upstream_addr, e);
            return Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Full::new(Bytes::from(format!(
                    "Failed to connect to upstream: {}",
                    e
                ))))
                .unwrap());
        }
    };

    let io = TokioIo::new(stream);

    let (mut sender, conn) = match hyper::client::conn::http1::handshake(io).await {
        Ok(r) => r,
        Err(e) => {
            log::warn!("Upstream handshake failed for {}: {}", upstream_addr, e);
            return Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Full::new(Bytes::from(format!(
                    "Upstream handshake failed: {}",
                    e
                ))))
                .unwrap());
        }
    };

    tokio::spawn(async move {
        if let Err(e) = conn.await {
            log::debug!("Upstream connection error: {}", e);
        }
    });

    // Build the upstream request
    let (parts, body) = req.into_parts();
    let body_bytes = body.collect().await?.to_bytes();

    let mut upstream_req = Request::builder()
        .method(parts.method)
        .uri(&upstream_uri)
        .body(Full::new(body_bytes))
        .unwrap();

    // Copy headers
    for (key, value) in &parts.headers {
        if key != hyper::header::HOST {
            upstream_req
                .headers_mut()
                .insert(key.clone(), value.clone());
        }
    }
    // Set correct Host header for upstream
    upstream_req.headers_mut().insert(
        hyper::header::HOST,
        format!("{}:{}", upstream.ip, upstream.port)
            .parse()
            .unwrap(),
    );

    match sender.send_request(upstream_req).await {
        Ok(resp) => {
            let (parts, body) = resp.into_parts();
            let body_bytes = body.collect().await?.to_bytes();
            Ok(Response::from_parts(parts, Full::new(body_bytes)))
        }
        Err(e) => {
            log::warn!("Upstream request failed: {}", e);
            Ok(Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Full::new(Bytes::from(format!(
                    "Upstream request failed: {}",
                    e
                ))))
                .unwrap())
        }
    }
}

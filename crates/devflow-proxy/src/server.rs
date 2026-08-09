use crate::ca::CertificateAuthority;
use crate::router::Router;
use crate::tls::SnsCertResolver;
use anyhow::Result;
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

/// Run the HTTPS reverse proxy server on an already-bound listener.
pub async fn run_https_server(
    listener: TcpListener,
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

    if let Ok(addr) = listener.local_addr() {
        log::info!("HTTPS proxy listening on {}", addr);
    }

    loop {
        tokio::select! {
            result = listener.accept() => {
                // Transient accept errors (EMFILE, ECONNABORTED) must not
                // kill the listener while the process keeps running.
                let (stream, peer_addr) = match result {
                    Ok(pair) => pair,
                    Err(e) => {
                        log::warn!("HTTPS accept error: {}", e);
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let tls_acceptor = tls_acceptor.clone();
                let router = router.clone();

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

                            let service = service_fn(move |req| {
                                let router = router.clone();
                                let sni = sni_hostname.clone();
                                async move {
                                    handle_request(req, &router, sni.as_deref(), peer_addr).await
                                }
                            });

                            if let Err(e) = http1::Builder::new()
                                .serve_connection(io, service)
                                .with_upgrades()
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

/// Run the HTTP server (redirects to HTTPS or serves plain) on an
/// already-bound listener.
pub async fn run_http_server(
    listener: TcpListener,
    https_port: u16,
    router: Arc<Router>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    if let Ok(addr) = listener.local_addr() {
        log::info!("HTTP proxy listening on {}", addr);
    }

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, peer_addr) = match result {
                    Ok(pair) => pair,
                    Err(e) => {
                        log::warn!("HTTP accept error: {}", e);
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let router = router.clone();

                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let router = router.clone();

                    let service = service_fn(move |req: Request<Incoming>| {
                        let router = router.clone();
                        async move {
                            let host = req
                                .headers()
                                .get(hyper::header::HOST)
                                .and_then(|h| h.to_str().ok())
                                .map(|h| h.split(':').next().unwrap_or(h).to_string());

                            if let Some(ref hostname) = host {
                                if router.resolve(hostname).await.is_some() {
                                    // Redirect to HTTPS
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
                        .with_upgrades()
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

fn header_has_token(headers: &hyper::HeaderMap, name: &'static str, token: &str) -> bool {
    headers.get_all(name).iter().any(|value| {
        value
            .to_str()
            .map(|value| {
                value
                    .split(',')
                    .any(|part| part.trim().eq_ignore_ascii_case(token))
            })
            .unwrap_or(false)
    })
}

fn is_upgrade_request<B>(req: &Request<B>) -> bool {
    req.headers().contains_key(hyper::header::UPGRADE)
        && header_has_token(req.headers(), hyper::header::CONNECTION.as_str(), "upgrade")
}

async fn tunnel_upgraded_connections(
    client_upgrade: hyper::upgrade::OnUpgrade,
    upstream_upgrade: hyper::upgrade::OnUpgrade,
    hostname: String,
) {
    match tokio::try_join!(client_upgrade, upstream_upgrade) {
        Ok((client, upstream)) => {
            let mut client = TokioIo::new(client);
            let mut upstream = TokioIo::new(upstream);

            match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
                Ok((from_client, from_upstream)) => log::debug!(
                    "Upgrade tunnel for {} closed (client→upstream {} bytes, upstream→client {} bytes)",
                    hostname,
                    from_client,
                    from_upstream
                ),
                Err(e) => log::debug!("Upgrade tunnel for {} failed: {}", hostname, e),
            }
        }
        Err(e) => log::debug!("Upgrade handshake for {} failed: {}", hostname, e),
    }
}

/// Handle a single proxied request by forwarding to the upstream.
async fn handle_request(
    mut req: Request<Incoming>,
    router: &Router,
    sni_hostname: Option<&str>,
    peer_addr: SocketAddr,
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

    let is_upgrade = is_upgrade_request(&req);
    let client_upgrade = is_upgrade.then(|| hyper::upgrade::on(&mut req));

    // Use origin-form URI (path+query only) for the upstream request
    let upstream_path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/")
        .to_string();

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
        if let Err(e) = conn.with_upgrades().await {
            log::debug!("Upstream connection error: {}", e);
        }
    });

    // Build the upstream request. Upgrade requests switch protocols after the
    // response headers, so there is no HTTP body to buffer.
    let (parts, body) = req.into_parts();
    let body_bytes = if is_upgrade {
        Bytes::new()
    } else {
        body.collect().await?.to_bytes()
    };

    let mut upstream_req = Request::builder()
        .method(parts.method)
        .uri(&upstream_path)
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
    // Set Host to the original hostname, not the upstream IP
    upstream_req
        .headers_mut()
        .insert(hyper::header::HOST, hostname.parse().unwrap());
    // Add standard reverse proxy headers
    upstream_req
        .headers_mut()
        .insert("x-forwarded-host", hostname.parse().unwrap());
    upstream_req.headers_mut().insert(
        "x-forwarded-proto",
        if sni_hostname.is_some() {
            "https"
        } else {
            "http"
        }
        .parse()
        .unwrap(),
    );
    upstream_req.headers_mut().insert(
        "x-forwarded-for",
        peer_addr.ip().to_string().parse().unwrap(),
    );

    match sender.send_request(upstream_req).await {
        Ok(mut resp) => {
            if is_upgrade && resp.status() == StatusCode::SWITCHING_PROTOCOLS {
                let upstream_upgrade = hyper::upgrade::on(&mut resp);
                let (parts, _body) = resp.into_parts();

                if let Some(client_upgrade) = client_upgrade {
                    tokio::spawn(tunnel_upgraded_connections(
                        client_upgrade,
                        upstream_upgrade,
                        hostname,
                    ));
                }

                return Ok(Response::from_parts(parts, Full::new(Bytes::new())));
            }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn request_with_headers(headers: &[(&'static str, &'static str)]) -> Request<Full<Bytes>> {
        let mut req = Request::builder().body(Full::new(Bytes::new())).unwrap();
        for (name, value) in headers {
            req.headers_mut().append(*name, value.parse().unwrap());
        }
        req
    }

    #[test]
    fn detects_websocket_upgrade_requests() {
        let req = request_with_headers(&[
            ("connection", "keep-alive, Upgrade"),
            ("upgrade", "websocket"),
        ]);

        assert!(is_upgrade_request(&req));
    }

    #[test]
    fn ignores_upgrade_header_without_connection_token() {
        let req = request_with_headers(&[("upgrade", "websocket")]);

        assert!(!is_upgrade_request(&req));
    }

    #[test]
    fn ignores_plain_http_requests() {
        let req = request_with_headers(&[]);

        assert!(!is_upgrade_request(&req));
    }

    #[tokio::test]
    async fn proxies_request_to_process_upstream() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let upstream = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = seen_tx.send(request);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\nconnection: close\r\n\r\nprocess-ok",
                )
                .await
                .unwrap();
        });

        let router = crate::router::Router::new();
        router
            .upsert(crate::discovery::ProxyTarget {
                domain: "api.main.app.local".to_string(),
                container_ip: "127.0.0.1".to_string(),
                port: upstream_addr.port(),
                container_id: "devflow-process:/repo:main:api".to_string(),
                container_name: "process:main:api".to_string(),
                project: Some("app".to_string()),
                service: Some("api".to_string()),
                workspace: Some("main".to_string()),
            })
            .await;

        let proxy = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let router_for_proxy = router.clone();
        tokio::spawn(async move {
            let (stream, peer_addr) = proxy.accept().await.unwrap();
            let io = TokioIo::new(stream);
            let service = service_fn(move |req| {
                let router = router_for_proxy.clone();
                async move { handle_request(req, &router, None, peer_addr).await }
            });
            http1::Builder::new()
                .serve_connection(io, service)
                .await
                .unwrap();
        });

        let mut client = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
        client
            .write_all(
                b"GET /hello?x=1 HTTP/1.1\r\nHost: api.main.app.local\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(response.contains("200 OK"), "{response}");
        assert!(response.contains("process-ok"), "{response}");

        let upstream_request = seen_rx.await.unwrap();
        let upstream_request_lower = upstream_request.to_ascii_lowercase();
        assert!(upstream_request.starts_with("GET /hello?x=1 HTTP/1.1"));
        assert!(upstream_request_lower.contains("host: api.main.app.local"));
        assert!(upstream_request_lower.contains("x-forwarded-host: api.main.app.local"));
        assert!(upstream_request_lower.contains("x-forwarded-proto: http"));
    }

    #[tokio::test]
    async fn unknown_host_returns_bad_gateway() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let router = crate::router::Router::new();
        let proxy = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, peer_addr) = proxy.accept().await.unwrap();
            let io = TokioIo::new(stream);
            let service = service_fn(move |req| {
                let router = router.clone();
                async move { handle_request(req, &router, None, peer_addr).await }
            });
            http1::Builder::new()
                .serve_connection(io, service)
                .await
                .unwrap();
        });

        let mut client = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: missing.local\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(response.contains("502 Bad Gateway"), "{response}");
        assert!(
            response.contains("No upstream found for missing.local"),
            "{response}"
        );
    }
}

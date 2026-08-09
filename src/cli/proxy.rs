use anyhow::{Context, Result};
use devflow_core::config::GlobalConfig;

pub(super) async fn handle_proxy_command(
    action: super::ProxyCommands,
    json_output: bool,
) -> Result<()> {
    match action {
        super::ProxyCommands::Start {
            daemon,
            https_port,
            http_port,
            api_port,
            domain_suffix,
            no_auto_network,
            no_mdns,
        } => {
            // Load global config for proxy defaults
            let global = GlobalConfig::load()?.unwrap_or_default();
            let proxy_cfg = global.proxy.unwrap_or_default();

            // Merge: CLI flags → global config → hardcoded defaults
            let https_port = https_port.or(proxy_cfg.https_port).unwrap_or(443);
            let http_port = http_port.or(proxy_cfg.http_port).unwrap_or(80);
            let api_port = api_port.or(proxy_cfg.api_port).unwrap_or(2019);
            let domain_suffix = domain_suffix
                .or(proxy_cfg.domain_suffix)
                .unwrap_or_else(devflow_proxy::default_domain_suffix);
            // mDNS on by default; --no-mdns or `proxy.mdns: false` disables it.
            let mdns = !no_mdns && proxy_cfg.mdns.unwrap_or(true);

            // Conflict detection: check if proxy is already running
            if let Ok(status) =
                reqwest_get_json(&format!("http://127.0.0.1:{}/api/status", api_port)).await
            {
                if status["running"].as_bool() == Some(true) {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::json!({
                                "error": "proxy_already_running",
                                "message": "Proxy is already running",
                                "api_port": api_port,
                            })
                        );
                    } else {
                        anyhow::bail!(
                            "Proxy is already running (API responding on port {}). Stop it first with: devflow proxy stop",
                            api_port
                        );
                    }
                    return Ok(());
                }
            }

            let config = devflow_proxy::ProxyConfig {
                https_port,
                http_port,
                api_port,
                domain_suffix: domain_suffix.clone(),
                auto_network: !no_auto_network,
                mdns,
                ..Default::default()
            };

            if daemon {
                // Fork to background
                let exe = std::env::current_exe()?;
                let mut args = vec![
                    "proxy".to_string(),
                    "start".to_string(),
                    "--https-port".to_string(),
                    https_port.to_string(),
                    "--http-port".to_string(),
                    http_port.to_string(),
                    "--api-port".to_string(),
                    api_port.to_string(),
                    "--domain-suffix".to_string(),
                    domain_suffix.clone(),
                ];
                if no_auto_network {
                    args.push("--no-auto-network".to_string());
                }
                if !mdns {
                    args.push("--no-mdns".to_string());
                }

                let child = std::process::Command::new(exe)
                    .args(&args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .context("Failed to spawn daemon process")?;

                let pid_path = devflow_proxy::ca::default_ca_cert_path()
                    .parent()
                    .unwrap()
                    .join("proxy.pid");
                std::fs::write(&pid_path, child.id().to_string())?;

                if json_output {
                    println!(
                        "{}",
                        serde_json::json!({
                            "status": "started",
                            "pid": child.id(),
                            "https_port": https_port,
                            "http_port": http_port,
                            "api_port": api_port,
                            "domain_suffix": domain_suffix,
                        })
                    );
                } else {
                    println!("Proxy started (pid: {})", child.id());
                    println!("  HTTPS: https://localhost:{}", https_port);
                    println!("  HTTP:  http://localhost:{}", http_port);
                    println!("  API:   http://localhost:{}", api_port);
                    println!("  Domain suffix: {}", domain_suffix);
                }
            } else {
                // Run in foreground
                println!("Starting devflow proxy...");
                println!("  HTTPS: 0.0.0.0:{}", https_port);
                println!("  HTTP:  0.0.0.0:{}", http_port);
                println!("  API:   127.0.0.1:{}", api_port);
                println!("  Domain suffix: {}", domain_suffix);
                println!("Press Ctrl+C to stop");

                let handle = devflow_proxy::run_proxy(config).await?;

                // Wait for Ctrl+C
                tokio::signal::ctrl_c().await?;
                println!("\nShutting down proxy...");
                handle.stop();
                // Give servers a moment to shut down
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                println!("Proxy stopped.");
            }
        }
        super::ProxyCommands::Stop => {
            let pid_path = devflow_proxy::ca::default_ca_cert_path()
                .parent()
                .unwrap()
                .join("proxy.pid");

            if pid_path.exists() {
                let pid_str = std::fs::read_to_string(&pid_path)?;
                if let Ok(pid) = pid_str.trim().parse::<i32>() {
                    #[cfg(unix)]
                    {
                        use nix::sys::signal::{kill, Signal};
                        use nix::unistd::Pid;
                        let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
                    }
                    std::fs::remove_file(&pid_path)?;

                    if json_output {
                        println!("{}", serde_json::json!({"status": "stopped", "pid": pid}));
                    } else {
                        println!("Proxy stopped (pid: {})", pid);
                    }
                } else {
                    anyhow::bail!("Invalid PID file");
                }
            } else if json_output {
                println!("{}", serde_json::json!({"status": "not_running"}));
            } else {
                println!("Proxy is not running (no PID file found)");
            }
        }
        super::ProxyCommands::Status => {
            // Try to query the API
            let api_url = format!("http://127.0.0.1:{}/api/status", configured_api_port());
            let api_url = api_url.as_str();
            match reqwest_get_json(api_url).await {
                Ok(status) => {
                    if json_output {
                        println!("{}", status);
                    } else {
                        let running = status["running"].as_bool().unwrap_or(false);
                        let targets = status["targets"].as_u64().unwrap_or(0);
                        let ca_installed = status["ca_installed"].as_bool().unwrap_or(false);
                        println!("Proxy: {}", if running { "running" } else { "stopped" });
                        println!("Targets: {}", targets);
                        println!(
                            "CA: {}",
                            if ca_installed {
                                "installed"
                            } else {
                                "not installed"
                            }
                        );
                    }
                }
                Err(_) => {
                    if json_output {
                        println!("{}", serde_json::json!({"running": false}));
                    } else {
                        println!("Proxy is not running");
                    }
                }
            }
        }
        super::ProxyCommands::List => {
            let api_url = format!("http://127.0.0.1:{}/api/targets", configured_api_port());
            let api_url = api_url.as_str();
            match reqwest_get_json(api_url).await {
                Ok(targets) => {
                    if json_output {
                        println!("{}", targets);
                    } else if let Some(arr) = targets.as_array() {
                        if arr.is_empty() {
                            println!("No discovered endpoints");
                        } else {
                            println!("{:<40} {:<20} {:<10}", "ENDPOINT", "CONTAINER", "UPSTREAM");
                            let mut unresolved_direct = Vec::new();
                            for t in arr {
                                let domain = t["domain"].as_str().unwrap_or("-");
                                let name = t["container_name"].as_str().unwrap_or("-");
                                let ip = t["container_ip"].as_str().unwrap_or("-");
                                let port = t["port"].as_u64().unwrap_or(0);
                                println!(
                                    "{:<40} {:<20} {}:{}",
                                    devflow_proxy::endpoint::display_endpoint(domain, port as u16),
                                    name,
                                    ip,
                                    port,
                                );

                                if devflow_proxy::endpoint::is_direct_endpoint_port(port as u16)
                                    && !hostname_resolves_to(domain, ip)
                                {
                                    unresolved_direct.push((domain.to_string(), ip.to_string()));
                                }
                            }

                            if !unresolved_direct.is_empty() {
                                eprintln!(
                                    "\nWarning: some database endpoint names do not yet resolve to their container IP."
                                );
                                eprintln!(
                                    "These are advertised via mDNS by the running proxy (macOS). If they don't resolve, check that the proxy is running with mDNS enabled, that you're on a platform that routes container IPs (OrbStack/Colima/Linux), or use the UPSTREAM IP shown above."
                                );
                                for (domain, ip) in unresolved_direct {
                                    eprintln!("  {} should resolve to {}", domain, ip);
                                }
                            }
                        }
                    }
                }
                Err(_) => {
                    if json_output {
                        println!("[]");
                    } else {
                        println!("Proxy is not running");
                    }
                }
            }
        }
        super::ProxyCommands::Trust { action } => match action {
            super::TrustCommands::Install => {
                let ca = devflow_proxy::ca::CertificateAuthority::load_or_generate()?;
                devflow_proxy::platform::install_system_trust(&ca)?;
                println!("CA certificate installed to system trust store");
            }
            super::TrustCommands::Verify => {
                let trusted = devflow_proxy::platform::verify_system_trust()?;
                if json_output {
                    println!("{}", serde_json::json!({"trusted": trusted}));
                } else if trusted {
                    println!("CA certificate is trusted by the system");
                } else {
                    println!("CA certificate is NOT trusted. Run: devflow proxy trust install");
                }
            }
            super::TrustCommands::Remove => {
                devflow_proxy::platform::remove_system_trust()?;
                println!("CA certificate removed from system trust store");
            }
            super::TrustCommands::Info => {
                println!("{}", devflow_proxy::platform::trust_info());
            }
        },
    }

    Ok(())
}

/// The API port `proxy status`/`proxy list` should query: the globally
/// configured one, falling back to the built-in default.
fn configured_api_port() -> u16 {
    GlobalConfig::load()
        .ok()
        .flatten()
        .and_then(|g| g.proxy)
        .and_then(|p| p.api_port)
        .unwrap_or(2019)
}

fn hostname_resolves_to(domain: &str, expected_ip: &str) -> bool {
    use std::net::{IpAddr, ToSocketAddrs};

    let Ok(expected_ip) = expected_ip.parse::<IpAddr>() else {
        return false;
    };

    (domain, 0)
        .to_socket_addrs()
        .map(|mut addrs| addrs.any(|addr| addr.ip() == expected_ip))
        .unwrap_or(false)
}

async fn reqwest_get_json(url: &str) -> Result<serde_json::Value> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()?;
    let resp = client.get(url).send().await?.json().await?;
    Ok(resp)
}

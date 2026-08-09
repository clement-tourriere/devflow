//! Shared Docker network for container-to-container DNS resolution.
//!
//! When the proxy starts with `auto_network: true` (the default), it creates a
//! `devflow` bridge network and auto-connects every discovered container with
//! aliases matching its pretty domain names. Docker's embedded DNS then resolves
//! those names for any container on the same network — no system DNS changes or
//! per-container configuration required.
//!
//! # How it works
//!
//! - **Host → container**: `https://web.myapp.local` routes through the proxy
//!   (`.local` is the default suffix; `.localhost` is an opt-in).
//! - **Container → container**: `http://web.myapp.local` resolves via
//!   Docker DNS directly, bypassing the proxy entirely. A suffix-stripped alias
//!   (`web.myapp`) is also registered as a fallback for environments that
//!   special-case `.localhost` before consulting Docker DNS.
//!
//! # Testing
//!
//! ## Setup
//!
//! ```sh
//! # Start the proxy (creates the devflow network automatically)
//! devflow proxy start
//!
//! # Start two test containers
//! docker run -d --name web1 nginx
//! docker run -d --name web2 nginx
//! ```
//!
//! ## Verify network membership
//!
//! ```sh
//! # Both containers should appear under "Containers", each with its aliases
//! docker network inspect devflow
//!
//! # Quick check — list connected container names
//! docker network inspect devflow --format '{{range .Containers}}{{.Name}} {{end}}'
//! ```
//!
//! ## Test from host (goes through the proxy)
//!
//! The examples below assume `--domain-suffix localhost`; with the default
//! `.local` suffix, substitute the suffix accordingly.
//!
//! ```sh
//! # HTTPS via the proxy (requires CA trust installed: devflow proxy trust install)
//! curl -s https://web1.localhost
//!
//! # Or skip certificate verification
//! curl -sk https://web1.localhost
//!
//! # HTTP redirects to HTTPS by default
//! curl -sL http://web1.localhost
//! ```
//!
//! ## Test from inside a container (uses Docker DNS directly)
//!
//! ```sh
//! # From web2, resolve web1 by its localhost alias — no proxy involved
//! docker exec web2 curl -s http://web1.localhost
//!
//! # Verify DNS resolution explicitly
//! docker exec web2 getent hosts web1.localhost
//!
//! # If the container doesn't have curl, use wget or a DNS lookup
//! docker exec web2 wget -qO- http://web1.localhost
//! docker exec web2 nslookup web1.localhost 127.0.0.11   # Docker's embedded DNS
//!
//! # Fallback for environments that special-case .localhost:
//! docker exec web2 curl -s http://web1
//! ```
//!
//! ## Test with Compose services
//!
//! ```sh
//! # Given a compose project "myapp" with services "web" and "api":
//! docker compose -p myapp up -d
//!
//! # From "api", reach "web" by its full devflow localhost alias
//! docker exec myapp-api-1 curl -s http://web.myapp.localhost
//! ```
//!
//! ## Cleanup
//!
//! ```sh
//! docker rm -f web1 web2
//! # The devflow network persists across proxy restarts; remove manually if needed
//! docker network rm devflow
//! ```

use anyhow::{Context, Result};
use bollard::models::{EndpointSettings, NetworkConnectRequest, NetworkCreateRequest};
use bollard::Docker;

pub const DEVFLOW_NETWORK: &str = "devflow";

/// Ensure the "devflow" bridge network exists. Idempotent.
pub async fn ensure_network(docker: &Docker) -> Result<()> {
    let config = NetworkCreateRequest {
        name: DEVFLOW_NETWORK.to_string(),
        driver: Some("bridge".to_string()),
        ..Default::default()
    };

    match docker.create_network(config).await {
        Ok(_) => {
            log::info!("Created Docker network '{}'", DEVFLOW_NETWORK);
        }
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 409, ..
        }) => {
            log::debug!("Docker network '{}' already exists", DEVFLOW_NETWORK);
        }
        Err(e) => {
            return Err(e).context(format!(
                "Failed to create Docker network '{}'",
                DEVFLOW_NETWORK
            ));
        }
    }

    Ok(())
}

/// Produce aliases with both the full domain and a suffix-stripped form.
///
/// The full `.localhost` name is the primary devflow name. The suffix-stripped
/// alias (e.g. `web.myapp` alongside `web.myapp.localhost`) is a compatibility
/// fallback for environments that special-case `.localhost` before consulting
/// Docker's embedded DNS.
pub fn strip_suffix_aliases(domains: &[String], domain_suffix: &str) -> Vec<String> {
    let mut aliases = Vec::with_capacity(domains.len() * 2);
    let dot_suffix = format!(".{}", domain_suffix);

    for domain in domains {
        aliases.push(domain.clone());
        if let Some(stripped) = domain.strip_suffix(&dot_suffix) {
            if !stripped.is_empty() {
                aliases.push(stripped.to_string());
            }
        }
    }

    aliases
}

/// Connect a container to the devflow network with the given aliases.
/// Silently ignores "already connected" errors.
pub async fn connect_container(
    docker: &Docker,
    container_id: &str,
    aliases: &[String],
) -> Result<()> {
    let config = NetworkConnectRequest {
        container: container_id.to_string(),
        endpoint_config: Some(EndpointSettings {
            aliases: Some(aliases.to_vec()),
            ..Default::default()
        }),
    };

    match docker.connect_network(DEVFLOW_NETWORK, config).await {
        Ok(()) => {
            log::info!(
                "Connected {} to '{}' network with aliases: {:?}",
                &container_id[..12.min(container_id.len())],
                DEVFLOW_NETWORK,
                aliases
            );
        }
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 403,
            ref message,
        }) if message.contains("already exists") => {
            log::debug!(
                "Container {} already connected to '{}' network",
                &container_id[..12.min(container_id.len())],
                DEVFLOW_NETWORK
            );
        }
        Err(e) => {
            return Err(e).context(format!(
                "Failed to connect container {} to '{}' network",
                &container_id[..12.min(container_id.len())],
                DEVFLOW_NETWORK
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_localhost_suffix() {
        let domains = vec!["web.myapp.localhost".to_string()];
        let result = strip_suffix_aliases(&domains, "localhost");
        assert_eq!(result, vec!["web.myapp.localhost", "web.myapp"]);
    }

    #[test]
    fn test_strip_custom_suffix() {
        let domains = vec!["web.myapp.test".to_string()];
        let result = strip_suffix_aliases(&domains, "test");
        assert_eq!(result, vec!["web.myapp.test", "web.myapp"]);
    }

    #[test]
    fn test_strip_no_match_suffix() {
        let domains = vec!["web.myapp.example.com".to_string()];
        let result = strip_suffix_aliases(&domains, "localhost");
        assert_eq!(result, vec!["web.myapp.example.com"]);
    }

    #[test]
    fn test_strip_multiple_domains() {
        let domains = vec![
            "web.myapp.localhost".to_string(),
            "api.myapp.localhost".to_string(),
        ];
        let result = strip_suffix_aliases(&domains, "localhost");
        assert_eq!(
            result,
            vec![
                "web.myapp.localhost",
                "web.myapp",
                "api.myapp.localhost",
                "api.myapp",
            ]
        );
    }

    #[test]
    fn test_strip_bare_suffix_no_empty_string() {
        let domains = vec!["localhost".to_string()];
        let result = strip_suffix_aliases(&domains, "localhost");
        assert_eq!(result, vec!["localhost"]);
    }

    #[test]
    fn test_strip_devflow_managed_domain() {
        let domains = vec!["postgres.main.ward-runs-app.localhost".to_string()];
        let result = strip_suffix_aliases(&domains, "localhost");
        assert_eq!(
            result,
            vec![
                "postgres.main.ward-runs-app.localhost",
                "postgres.main.ward-runs-app",
            ]
        );
    }
}

use bollard::models::ContainerInspectResponse;
use std::collections::HashMap;

/// A resolved discovered target: one domain pointing to one container IP:port.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProxyTarget {
    pub domain: String,
    pub container_ip: String,
    pub port: u16,
    pub container_id: String,
    pub container_name: String,
    /// Compose project name (if applicable)
    pub project: Option<String>,
    /// Compose service name (if applicable)
    pub service: Option<String>,
    /// devflow workspace name (if applicable)
    pub workspace: Option<String>,
}

/// Extract discovered targets from a container inspection result.
///
/// HTTP services are reachable through the HTTP(S) proxy. Non-HTTP services
/// such as PostgreSQL are still listed so callers can expose the generated
/// hostname as a direct container-IP endpoint on platforms that support it.
pub fn extract_proxy_targets(
    container: &ContainerInspectResponse,
    domain_suffix: &str,
) -> Vec<ProxyTarget> {
    if !should_discover(container) {
        return Vec::new();
    }

    let domains = extract_domains(container, domain_suffix);
    let container_ip = extract_container_ip(container);
    let port = extract_port(container);

    if port == 0 || container_ip.is_empty() {
        return Vec::new();
    }

    build_proxy_targets(container, domains, container_ip, port)
}

/// Extract pretty domains for Docker-network DNS aliases.
///
/// This intentionally includes non-HTTP services so containers on the shared
/// devflow Docker network can still reach databases by the full domain alias
/// (for example `postgres.my-workspace.my-project.local:5432` with the
/// default `.local` suffix).
pub fn extract_network_domains(
    container: &ContainerInspectResponse,
    domain_suffix: &str,
) -> Vec<String> {
    if !should_discover(container) {
        return Vec::new();
    }

    extract_domains(container, domain_suffix)
}

fn build_proxy_targets(
    container: &ContainerInspectResponse,
    domains: Vec<String>,
    container_ip: String,
    port: u16,
) -> Vec<ProxyTarget> {
    let container_id = container.id.clone().unwrap_or_default();
    let container_name = container
        .name
        .as_deref()
        .unwrap_or("")
        .trim_start_matches('/')
        .to_string();
    let labels = get_labels(container);
    let project = labels
        .get("devflow.project")
        .or_else(|| labels.get("com.docker.compose.project"))
        .cloned();
    let service_name = labels
        .get("devflow.service")
        .or_else(|| labels.get("com.docker.compose.service"))
        .cloned();
    let workspace = labels.get("devflow.workspace").cloned();

    domains
        .into_iter()
        .map(|domain| ProxyTarget {
            domain,
            container_ip: container_ip.clone(),
            port,
            container_id: container_id.clone(),
            container_name: container_name.clone(),
            project: project.clone(),
            service: service_name.clone(),
            workspace: workspace.clone(),
        })
        .collect()
}

fn get_labels(container: &ContainerInspectResponse) -> HashMap<String, String> {
    container
        .config
        .as_ref()
        .and_then(|c| c.labels.clone())
        .unwrap_or_default()
}

fn get_env_vars(container: &ContainerInspectResponse) -> Vec<String> {
    container
        .config
        .as_ref()
        .and_then(|c| c.env.clone())
        .unwrap_or_default()
}

fn should_discover(container: &ContainerInspectResponse) -> bool {
    // Skip if not running
    let running = container
        .state
        .as_ref()
        .and_then(|s| s.running)
        .unwrap_or(false);
    if !running {
        return false;
    }

    let labels = get_labels(container);

    // Skip if explicitly disabled
    if labels.get("devproxy.enabled").map(|v| v.as_str()) == Some("false") {
        return false;
    }

    // Always allow containers with explicit domain labels
    if labels.contains_key("devproxy.domains") || labels.contains_key("devproxy.domain") {
        return true;
    }

    // Always allow containers with VIRTUAL_HOST env var
    if get_env_vars(container)
        .iter()
        .any(|e| e.starts_with("VIRTUAL_HOST="))
    {
        return true;
    }

    // Skip proxy containers themselves
    let name = container
        .name
        .as_deref()
        .unwrap_or("")
        .trim_start_matches('/');
    if name.starts_with("devproxy") || name.starts_with("devflow-proxy") {
        return false;
    }

    true
}

fn extract_domains(container: &ContainerInspectResponse, domain_suffix: &str) -> Vec<String> {
    let labels = get_labels(container);

    // 1. devproxy.domains label (plural) — comma-separated, highest priority
    if let Some(domains) = labels.get("devproxy.domains") {
        let result: Vec<String> = domains
            .split(',')
            .map(|d| d.trim().to_lowercase())
            .filter(|d| !d.is_empty())
            .collect();
        if !result.is_empty() {
            return result;
        }
    }

    // 2. devproxy.domain label (singular) — also support comma-separated for backward compat
    if let Some(domains) = labels.get("devproxy.domain") {
        let result: Vec<String> = domains
            .split(',')
            .map(|d| d.trim().to_lowercase())
            .filter(|d| !d.is_empty())
            .collect();
        if !result.is_empty() {
            return result;
        }
    }

    // 3. VIRTUAL_HOST env var — nginx-proxy compat, comma-separated
    for env in get_env_vars(container) {
        if let Some(hosts) = env.strip_prefix("VIRTUAL_HOST=") {
            let result: Vec<String> = hosts
                .split(',')
                .map(|d| d.trim().to_lowercase())
                .filter(|d| !d.is_empty())
                .collect();
            if !result.is_empty() {
                return result;
            }
        }
    }

    let container_name = container
        .name
        .as_deref()
        .unwrap_or("")
        .trim_start_matches('/')
        .to_string();

    // 4. devflow-managed: service.workspace.project.suffix
    if let (Some(project), Some(workspace), Some(service_name)) = (
        labels.get("devflow.project"),
        labels.get("devflow.workspace"),
        labels.get("devflow.service"),
    ) {
        return vec![format!(
            "{}.{}.{}.{}",
            service_name, workspace, project, domain_suffix
        )
        .to_lowercase()];
    }

    // 5. Compose: service.project.suffix
    if let (Some(project), Some(service_name)) = (
        labels.get("com.docker.compose.project"),
        labels.get("com.docker.compose.service"),
    ) {
        return vec![format!("{}.{}.{}", service_name, project, domain_suffix).to_lowercase()];
    }

    // 6. Standalone: container_name.suffix
    vec![format!("{}.{}", container_name, domain_suffix).to_lowercase()]
}

fn extract_container_ip(container: &ContainerInspectResponse) -> String {
    let networks = container
        .network_settings
        .as_ref()
        .and_then(|ns| ns.networks.as_ref());

    if let Some(networks) = networks {
        // Prefer custom networks over bridge
        for (name, endpoint) in networks {
            if name != "bridge" {
                if let Some(ref ip) = endpoint.ip_address {
                    if !ip.is_empty() {
                        return ip.clone();
                    }
                }
            }
        }

        // Fallback to bridge
        if let Some(bridge) = networks.get("bridge") {
            if let Some(ref ip) = bridge.ip_address {
                if !ip.is_empty() {
                    return ip.clone();
                }
            }
        }
    }

    String::new()
}

fn extract_port(container: &ContainerInspectResponse) -> u16 {
    let labels = get_labels(container);

    // Custom port label
    if let Some(port_str) = labels.get("devproxy.port") {
        if let Ok(port) = port_str.parse::<u16>() {
            return port;
        }
    }

    // Environment variables
    for env in get_env_vars(container) {
        if let Some(port_str) = env.strip_prefix("DEVPROXY_PORT=") {
            if let Ok(port) = port_str.parse::<u16>() {
                return port;
            }
        }
        // nginx-proxy compat
        if let Some(port_str) = env.strip_prefix("VIRTUAL_PORT=") {
            if let Ok(port) = port_str.parse::<u16>() {
                return port;
            }
        }
    }

    // Exposed ports from container config (Vec<String>, e.g. ["80/tcp", "443/tcp"]).
    // bollard deserializes these from a JSON map, so the order is RANDOM per
    // process — pick deterministically: well-known HTTP ports first, then the
    // lowest port, and warn when the choice is ambiguous.
    if let Some(exposed) = container
        .config
        .as_ref()
        .and_then(|c| c.exposed_ports.as_ref())
    {
        let mut ports: Vec<u16> = exposed
            .iter()
            .filter_map(|p| p.split('/').next().and_then(|n| n.parse::<u16>().ok()))
            .collect();
        ports.sort_unstable();
        ports.dedup();
        if !ports.is_empty() {
            const PREFERRED_HTTP_PORTS: [u16; 10] =
                [80, 8080, 3000, 8000, 5173, 4200, 8123, 5000, 9000, 443];
            let chosen = PREFERRED_HTTP_PORTS
                .iter()
                .find(|p| ports.contains(p))
                .copied()
                .unwrap_or(ports[0]);
            if ports.len() > 1 {
                log::warn!(
                    "Container {} exposes multiple ports {:?}; routing to {}. Set the devproxy.port label or VIRTUAL_PORT to override.",
                    container.name.as_deref().unwrap_or("?"),
                    ports,
                    chosen
                );
            }
            return chosen;
        }
    }

    // Common ports heuristic
    80
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_container(name: &str, labels: HashMap<String, String>) -> ContainerInspectResponse {
        make_container_with_env_and_ports(name, labels, Vec::new(), Vec::new())
    }

    fn make_container_with_env_and_ports(
        name: &str,
        labels: HashMap<String, String>,
        env: Vec<String>,
        exposed_ports: Vec<String>,
    ) -> ContainerInspectResponse {
        let mut networks = HashMap::new();
        networks.insert(
            "bridge".to_string(),
            bollard::models::EndpointSettings {
                ip_address: Some("172.17.0.2".to_string()),
                ..Default::default()
            },
        );
        ContainerInspectResponse {
            id: Some("abc123".to_string()),
            name: Some(format!("/{}", name)),
            state: Some(bollard::models::ContainerState {
                running: Some(true),
                ..Default::default()
            }),
            config: Some(bollard::models::ContainerConfig {
                labels: Some(labels),
                env: Some(env),
                exposed_ports: if exposed_ports.is_empty() {
                    None
                } else {
                    Some(exposed_ports)
                },
                ..Default::default()
            }),
            network_settings: Some(bollard::models::NetworkSettings {
                networks: Some(networks),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn test_extract_port_multi_port_is_deterministic_and_http_biased() {
        // nginx exposing 80 + 443: must always pick 80, never the TLS port
        let c = make_container_with_env_and_ports(
            "nginx",
            HashMap::new(),
            Vec::new(),
            vec!["443/tcp".to_string(), "80/tcp".to_string()],
        );
        assert_eq!(extract_port(&c), 80);

        // ClickHouse exposing 8123 + 9000 + 9009: HTTP port 8123 wins
        let c = make_container_with_env_and_ports(
            "clickhouse",
            HashMap::new(),
            Vec::new(),
            vec![
                "9009/tcp".to_string(),
                "9000/tcp".to_string(),
                "8123/tcp".to_string(),
            ],
        );
        assert_eq!(extract_port(&c), 8123);

        // No preferred port: lowest wins (stable across restarts)
        let c = make_container_with_env_and_ports(
            "custom",
            HashMap::new(),
            Vec::new(),
            vec!["7100/tcp".to_string(), "7002/tcp".to_string()],
        );
        assert_eq!(extract_port(&c), 7002);
    }

    #[test]
    fn test_standalone_domain() {
        let container = make_container("nginx", HashMap::new());
        let targets = extract_proxy_targets(&container, "localhost");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].domain, "nginx.localhost");
    }

    #[test]
    fn test_compose_domain() {
        let mut labels = HashMap::new();
        labels.insert(
            "com.docker.compose.project".to_string(),
            "myapp".to_string(),
        );
        labels.insert("com.docker.compose.service".to_string(), "web".to_string());

        let container = make_container("myapp-web-1", labels);
        let targets = extract_proxy_targets(&container, "localhost");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].domain, "web.myapp.localhost");
    }

    #[test]
    fn test_custom_domain_label() {
        let mut labels = HashMap::new();
        labels.insert("devproxy.domain".to_string(), "myapp.test".to_string());

        let container = make_container("something", labels);
        let targets = extract_proxy_targets(&container, "localhost");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].domain, "myapp.test");
    }

    #[test]
    fn test_disabled_container() {
        let mut labels = HashMap::new();
        labels.insert("devproxy.enabled".to_string(), "false".to_string());

        let container = make_container("nginx", labels);
        let targets = extract_proxy_targets(&container, "localhost");
        assert!(targets.is_empty());
    }

    #[test]
    fn test_postgres_port_is_discovered_for_direct_endpoint() {
        let container = make_container_with_env_and_ports(
            "postgres",
            HashMap::new(),
            Vec::new(),
            vec!["5432/tcp".to_string()],
        );

        let targets = extract_proxy_targets(&container, "localhost");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].domain, "postgres.localhost");
        assert_eq!(targets[0].port, 5432);

        let domains = extract_network_domains(&container, "localhost");
        assert_eq!(domains, vec!["postgres.localhost"]);
    }

    fn make_container_with_env(
        name: &str,
        labels: HashMap<String, String>,
        env: Vec<String>,
    ) -> ContainerInspectResponse {
        make_container_with_env_and_ports(name, labels, env, Vec::new())
    }

    #[test]
    fn test_multiple_domains_label() {
        let mut labels = HashMap::new();
        labels.insert(
            "devproxy.domains".to_string(),
            "app.localhost,api.localhost".to_string(),
        );

        let container = make_container("test", labels);
        let targets = extract_proxy_targets(&container, "localhost");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].domain, "app.localhost");
        assert_eq!(targets[1].domain, "api.localhost");
    }

    #[test]
    fn test_virtual_host_env() {
        let container = make_container_with_env(
            "myapp",
            HashMap::new(),
            vec!["VIRTUAL_HOST=myapp.localhost".to_string()],
        );
        let targets = extract_proxy_targets(&container, "localhost");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].domain, "myapp.localhost");
    }

    #[test]
    fn test_virtual_port_env() {
        let container = make_container_with_env(
            "myapp",
            HashMap::new(),
            vec!["VIRTUAL_PORT=8080".to_string()],
        );
        let targets = extract_proxy_targets(&container, "localhost");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].port, 8080);
    }

    #[test]
    fn test_domain_priority() {
        // devproxy.domains takes priority over devproxy.domain
        let mut labels = HashMap::new();
        labels.insert(
            "devproxy.domains".to_string(),
            "first.localhost".to_string(),
        );
        labels.insert(
            "devproxy.domain".to_string(),
            "second.localhost".to_string(),
        );

        let container = make_container("test", labels);
        let targets = extract_proxy_targets(&container, "localhost");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].domain, "first.localhost");
    }

    #[test]
    fn test_domains_trimming() {
        let mut labels = HashMap::new();
        labels.insert(
            "devproxy.domains".to_string(),
            " app.localhost , api.localhost , ".to_string(),
        );

        let container = make_container("test", labels);
        let targets = extract_proxy_targets(&container, "localhost");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].domain, "app.localhost");
        assert_eq!(targets[1].domain, "api.localhost");
    }

    #[test]
    fn test_domains_lowercased() {
        let mut labels = HashMap::new();
        labels.insert(
            "devproxy.domains".to_string(),
            "App.LocalHost,API.LOCALHOST".to_string(),
        );

        let container = make_container("test", labels);
        let targets = extract_proxy_targets(&container, "localhost");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].domain, "app.localhost");
        assert_eq!(targets[1].domain, "api.localhost");
    }

    #[test]
    fn test_singular_domain_comma_separated() {
        let mut labels = HashMap::new();
        labels.insert(
            "devproxy.domain".to_string(),
            "app.localhost,api.localhost".to_string(),
        );

        let container = make_container("test", labels);
        let targets = extract_proxy_targets(&container, "localhost");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].domain, "app.localhost");
        assert_eq!(targets[1].domain, "api.localhost");
    }

    #[test]
    fn test_virtual_host_multiple() {
        let container = make_container_with_env(
            "myapp",
            HashMap::new(),
            vec!["VIRTUAL_HOST=app.localhost,api.localhost".to_string()],
        );
        let targets = extract_proxy_targets(&container, "localhost");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].domain, "app.localhost");
        assert_eq!(targets[1].domain, "api.localhost");
    }
}

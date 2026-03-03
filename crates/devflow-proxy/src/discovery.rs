use bollard::models::ContainerInspectResponse;
use std::collections::HashMap;

/// A resolved proxy target: one domain pointing to one container IP:port.
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

/// Extract proxy targets from a container inspection result.
pub fn extract_proxy_targets(
    container: &ContainerInspectResponse,
    domain_suffix: &str,
) -> Vec<ProxyTarget> {
    if !should_proxy(container) {
        return Vec::new();
    }

    let domains = extract_domains(container, domain_suffix);
    let container_ip = extract_container_ip(container);
    let port = extract_port(container);

    if port == 0 || container_ip.is_empty() {
        return Vec::new();
    }

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

fn should_proxy(container: &ContainerInspectResponse) -> bool {
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
    if labels.contains_key("devproxy.domain") {
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

    // Custom domain label (highest priority)
    if let Some(custom) = labels.get("devproxy.domain") {
        return vec![custom.clone()];
    }

    let container_name = container
        .name
        .as_deref()
        .unwrap_or("")
        .trim_start_matches('/')
        .to_string();

    // devflow-managed: service.workspace.project.suffix
    if let (Some(project), Some(workspace), Some(service_name)) = (
        labels.get("devflow.project"),
        labels.get("devflow.workspace"),
        labels.get("devflow.service"),
    ) {
        return vec![format!(
            "{}.{}.{}.{}",
            service_name, workspace, project, domain_suffix
        )];
    }

    // Compose: service.project.suffix
    if let (Some(project), Some(service_name)) = (
        labels.get("com.docker.compose.project"),
        labels.get("com.docker.compose.service"),
    ) {
        return vec![format!("{}.{}.{}", service_name, project, domain_suffix)];
    }

    // Standalone: container_name.suffix
    vec![format!("{}.{}", container_name, domain_suffix)]
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

    // Environment variable
    for env in get_env_vars(container) {
        if let Some(port_str) = env.strip_prefix("DEVPROXY_PORT=") {
            if let Ok(port) = port_str.parse::<u16>() {
                return port;
            }
        }
    }

    // Exposed ports from container config (Vec<String>, e.g. ["80/tcp", "443/tcp"])
    if let Some(exposed) = container
        .config
        .as_ref()
        .and_then(|c| c.exposed_ports.as_ref())
    {
        for port_str in exposed {
            if let Some(port_num) = port_str.split('/').next() {
                if let Ok(port) = port_num.parse::<u16>() {
                    return port;
                }
            }
        }
    }

    // Common ports heuristic
    80
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_container(name: &str, labels: HashMap<String, String>) -> ContainerInspectResponse {
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
}

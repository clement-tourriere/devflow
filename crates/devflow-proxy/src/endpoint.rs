//! Endpoint classification and display helpers.
//!
//! HTTP services are reached through the proxy (its mDNS record points the name
//! at `127.0.0.1`, where the HTTPS listener terminates TLS and routes by
//! Host/SNI). Direct TCP services such as PostgreSQL are reached *directly*: the
//! proxy advertises the name straight to the container IP, so the client
//! connects to the container at its native port with no proxy in the path. See
//! [`crate::mdns`] for how the names are advertised (no `/etc/hosts` edits).

/// Return the native URL scheme for ports that should be reached directly,
/// bypassing the HTTP(S) proxy.
pub fn direct_endpoint_scheme(port: u16) -> Option<&'static str> {
    match port {
        5432 => Some("postgresql"),
        3306 | 33060 => Some("mysql"),
        6379 | 6380 => Some("redis"),
        11211 => Some("memcached"),
        1433 => Some("sqlserver"),
        1521 => Some("oracle"),
        27017..=27019 => Some("mongodb"),
        9042 | 9160 => Some("cassandra"),
        5671 | 5672 => Some("amqp"),
        1883 | 8883 => Some("mqtt"),
        2181 => Some("zookeeper"),
        4222 => Some("nats"),
        _ => None,
    }
}

/// Whether a target should be shown as a direct TCP endpoint.
pub fn is_direct_endpoint_port(port: u16) -> bool {
    direct_endpoint_scheme(port).is_some()
}

/// Human-friendly endpoint for CLI/TUI display.
pub fn display_endpoint(domain: &str, port: u16) -> String {
    match direct_endpoint_scheme(port) {
        Some(scheme) => format!("{}://{}:{}", scheme, domain, port),
        None => format!("https://{}", domain),
    }
}

/// Loopback-only HTTP names cannot represent a container's direct TCP address.
pub fn display_target_endpoint(domain: &str, port: u16, upstream_ip: &str) -> String {
    if is_direct_endpoint_port(port) && (domain == "localhost" || domain.ends_with(".localhost")) {
        let host = if upstream_ip.contains(':') {
            format!("[{upstream_ip}]")
        } else {
            upstream_ip.into()
        };
        display_endpoint(&host, port)
    } else {
        display_endpoint(domain, port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_postgres_as_direct_endpoint() {
        assert_eq!(
            display_endpoint("postgres.app.local", 5432),
            "postgresql://postgres.app.local:5432"
        );
    }

    #[test]
    fn display_http_as_https_endpoint() {
        assert_eq!(
            display_endpoint("web.app.local", 3000),
            "https://web.app.local"
        );
    }

    #[test]
    fn localhost_names_do_not_misdirect_database_connections() {
        assert_eq!(
            display_target_endpoint("db.project.localhost", 5432, "172.18.0.3"),
            "postgresql://172.18.0.3:5432"
        );
        assert_eq!(
            display_target_endpoint("web.project.localhost", 3000, "172.18.0.3"),
            "https://web.project.localhost"
        );
    }
}

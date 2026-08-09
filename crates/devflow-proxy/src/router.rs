use crate::discovery::ProxyTarget;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Upstream destination for a domain.
#[derive(Debug, Clone)]
pub struct Upstream {
    pub ip: String,
    pub port: u16,
    pub target: ProxyTarget,
}

/// Dynamic routing table: maps Host header values to upstream targets.
pub struct Router {
    routes: RwLock<HashMap<String, Upstream>>,
}

impl Router {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            routes: RwLock::new(HashMap::new()),
        })
    }

    /// Look up the upstream for a given hostname.
    pub async fn resolve(&self, host: &str) -> Option<Upstream> {
        let routes = self.routes.read().await;
        routes.get(host).cloned()
    }

    /// Add or update a route.
    pub async fn upsert(&self, target: ProxyTarget) {
        let upstream = Upstream {
            ip: target.container_ip.clone(),
            port: target.port,
            target: target.clone(),
        };
        let mut routes = self.routes.write().await;
        routes.insert(target.domain.clone(), upstream);
    }

    /// Replace the entire routing table (used after Docker reconnects to
    /// drop routes for containers that died while the event stream was down).
    pub async fn replace_all(&self, targets: Vec<ProxyTarget>) {
        let mut routes = self.routes.write().await;
        routes.clear();
        for target in targets {
            let upstream = Upstream {
                ip: target.container_ip.clone(),
                port: target.port,
                target: target.clone(),
            };
            routes.insert(target.domain.clone(), upstream);
        }
    }

    /// Remove routes for a given container ID.
    pub async fn remove_by_container(&self, container_id: &str) {
        let mut routes = self.routes.write().await;
        routes.retain(|_, v| v.target.container_id != container_id);
    }

    /// Replace routes produced by devflow host process discovery while
    /// preserving Docker/container routes.
    pub async fn replace_process_targets(&self, targets: Vec<ProxyTarget>) {
        let mut routes = self.routes.write().await;
        routes.retain(|_, v| !crate::processes::is_process_target(&v.target));
        for target in targets {
            let upstream = Upstream {
                ip: target.container_ip.clone(),
                port: target.port,
                target: target.clone(),
            };
            routes.insert(target.domain.clone(), upstream);
        }
    }

    /// Get all current routes.
    pub async fn list(&self) -> Vec<ProxyTarget> {
        let routes = self.routes.read().await;
        routes.values().map(|u| u.target.clone()).collect()
    }

    /// Get the number of active routes.
    pub async fn len(&self) -> usize {
        let routes = self.routes.read().await;
        routes.len()
    }

    /// Check if there are no active routes.
    /// No callers today; kept because `len` without `is_empty` trips
    /// `clippy::len_without_is_empty`.
    pub async fn is_empty(&self) -> bool {
        let routes = self.routes.read().await;
        routes.is_empty()
    }
}

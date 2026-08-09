//! ClickHouse local provider — manages workspace-isolated ClickHouse Docker containers.
//!
//! Each workspace gets its own ClickHouse container. Data is stored in bind-mounted
//! directories under `data_root/clickhouse/{service_name}/{workspace_name}/`.
//!
//! The container lifecycle lives in the shared
//! [`LocalEngineBackend`](crate::services::local_engine::LocalEngineBackend);
//! this module only supplies the ClickHouse-specific [`LocalEngineSpec`].

use std::time::Duration;

use crate::config::{ClickHouseConfig, DockerCustomSettings};
use crate::services::local_engine::{LocalEngineBackend, LocalEngineSpec, PortLayout};
use crate::services::ConnectionInfo;

/// Default ClickHouse ports: HTTP 8123, native TCP 9000.
const CLICKHOUSE_HTTP_PORT: u16 = 8123;
const CLICKHOUSE_NATIVE_PORT: u16 = 9000;

/// ClickHouse-specific engine behavior for [`LocalEngineBackend`].
pub struct ClickHouseEngine {
    image: String,
    port_range_start: u16,
    user: String,
    password: Option<String>,
}

impl LocalEngineSpec for ClickHouseEngine {
    fn kind(&self) -> &'static str {
        "clickhouse"
    }

    fn display_name(&self) -> &'static str {
        "ClickHouse"
    }

    fn provider_name(&self) -> &'static str {
        "ClickHouse (Docker)"
    }

    fn image(&self) -> &str {
        &self.image
    }

    fn port_range_start(&self) -> u16 {
        self.port_range_start
    }

    fn data_mount_path(&self) -> &'static str {
        "/var/lib/clickhouse"
    }

    fn port_layout(&self) -> PortLayout {
        // HTTP on the picked host port, native protocol on the next one.
        PortLayout::ConsecutivePair {
            primary_container_port: CLICKHOUSE_HTTP_PORT,
            secondary_container_port: CLICKHOUSE_NATIVE_PORT,
        }
    }

    fn env(&self) -> Vec<String> {
        let mut env = vec![format!("CLICKHOUSE_USER={}", self.user)];
        if let Some(ref password) = self.password {
            env.push(format!("CLICKHOUSE_PASSWORD={password}"));
        }
        env
    }

    fn readiness_command(&self) -> Vec<String> {
        // clickhouse-client --user <u> [--password <p>] --query "SELECT 1"
        let mut cmd = vec![
            "clickhouse-client".to_string(),
            "--user".to_string(),
            self.user.clone(),
        ];
        if let Some(ref password) = self.password {
            cmd.push("--password".to_string());
            cmd.push(password.clone());
        }
        cmd.push("--query".to_string());
        cmd.push("SELECT 1".to_string());
        cmd
    }

    fn restart_ready_timeout(&self) -> Duration {
        Duration::from_secs(60)
    }

    fn connection_info(&self, host_port: u16) -> ConnectionInfo {
        ConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: host_port,
            database: "default".to_string(),
            user: self.user.clone(),
            password: self.password.clone(),
            connection_string: Some(format!(
                "http://{}:{}@127.0.0.1:{host_port}",
                self.user,
                self.password.as_deref().unwrap_or("")
            )),
        }
    }
}

/// Per-workspace ClickHouse provider: the shared local-engine backend
/// specialized with [`ClickHouseEngine`].
pub type ClickHouseLocalProvider = LocalEngineBackend<ClickHouseEngine>;

impl ClickHouseLocalProvider {
    pub fn new(
        project_name: &str,
        service_name: &str,
        config: &ClickHouseConfig,
        docker_settings: Option<&DockerCustomSettings>,
    ) -> anyhow::Result<Self> {
        Self::with_engine(
            project_name,
            service_name,
            config.data_root.as_deref(),
            docker_settings,
            ClickHouseEngine {
                image: config.image.clone(),
                port_range_start: config.port_range_start.unwrap_or(59000),
                user: config.user.clone(),
                password: config.password.clone(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(password: Option<&str>) -> ClickHouseEngine {
        ClickHouseEngine {
            image: "clickhouse/clickhouse-server:latest".to_string(),
            port_range_start: 59000,
            user: "default".to_string(),
            password: password.map(str::to_string),
        }
    }

    #[test]
    fn spec_pins_container_contract() {
        let e = engine(Some("pw"));
        assert_eq!(e.kind(), "clickhouse");
        assert_eq!(e.data_mount_path(), "/var/lib/clickhouse");
        assert_eq!(
            e.env(),
            vec!["CLICKHOUSE_USER=default", "CLICKHOUSE_PASSWORD=pw"]
        );
        assert!(matches!(
            e.port_layout(),
            PortLayout::ConsecutivePair {
                primary_container_port: 8123,
                secondary_container_port: 9000,
            }
        ));
        assert_eq!(e.restart_ready_timeout(), Duration::from_secs(60));
    }

    #[test]
    fn readiness_probe_authenticates_only_when_password_set() {
        assert_eq!(
            engine(None).readiness_command(),
            vec![
                "clickhouse-client",
                "--user",
                "default",
                "--query",
                "SELECT 1"
            ]
        );
        assert_eq!(
            engine(Some("pw")).readiness_command(),
            vec![
                "clickhouse-client",
                "--user",
                "default",
                "--password",
                "pw",
                "--query",
                "SELECT 1"
            ]
        );
    }

    #[test]
    fn connection_string_shape_is_stable() {
        let info = engine(Some("pw")).connection_info(59004);
        assert_eq!(info.port, 59004);
        assert_eq!(info.database, "default");
        assert_eq!(
            info.connection_string.as_deref(),
            Some("http://default:pw@127.0.0.1:59004")
        );
        let no_pw = engine(None).connection_info(59004);
        assert_eq!(
            no_pw.connection_string.as_deref(),
            Some("http://default:@127.0.0.1:59004")
        );
    }
}

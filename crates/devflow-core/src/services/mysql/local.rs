//! MySQL/MariaDB local provider — manages workspace-isolated MySQL Docker containers.
//!
//! Each workspace gets its own MySQL container. Data is stored in bind-mounted
//! directories under `data_root/mysql/{service_name}/{workspace_name}/`.
//!
//! The container lifecycle lives in the shared
//! [`LocalEngineBackend`](crate::services::local_engine::LocalEngineBackend);
//! this module only supplies the MySQL-specific [`LocalEngineSpec`].

use std::time::Duration;

use crate::config::{DockerCustomSettings, MySQLConfig};
use crate::services::local_engine::{LocalEngineBackend, LocalEngineSpec, PortLayout};
use crate::services::ConnectionInfo;

const MYSQL_PORT: u16 = 3306;

/// MySQL-specific engine behavior for [`LocalEngineBackend`].
pub struct MySQLEngine {
    image: String,
    port_range_start: u16,
    root_password: String,
    database: Option<String>,
    user: Option<String>,
    password: Option<String>,
}

impl LocalEngineSpec for MySQLEngine {
    fn kind(&self) -> &'static str {
        "mysql"
    }

    fn display_name(&self) -> &'static str {
        "MySQL"
    }

    fn provider_name(&self) -> &'static str {
        "MySQL (Docker)"
    }

    fn image(&self) -> &str {
        &self.image
    }

    fn port_range_start(&self) -> u16 {
        self.port_range_start
    }

    fn data_mount_path(&self) -> &'static str {
        "/var/lib/mysql"
    }

    fn port_layout(&self) -> PortLayout {
        PortLayout::Single {
            container_port: MYSQL_PORT,
        }
    }

    fn env(&self) -> Vec<String> {
        let mut env = vec![format!("MYSQL_ROOT_PASSWORD={}", self.root_password)];
        if let Some(ref db) = self.database {
            env.push(format!("MYSQL_DATABASE={db}"));
        }
        if let Some(ref user) = self.user {
            env.push(format!("MYSQL_USER={user}"));
        }
        if let Some(ref password) = self.password {
            env.push(format!("MYSQL_PASSWORD={password}"));
        }
        env
    }

    fn readiness_command(&self) -> Vec<String> {
        // mysqladmin ping
        vec![
            "mysqladmin".to_string(),
            "ping".to_string(),
            "-h".to_string(),
            "127.0.0.1".to_string(),
            format!("-p{}", self.root_password),
            "--silent".to_string(),
        ]
    }

    fn restart_ready_timeout(&self) -> Duration {
        Duration::from_secs(120)
    }

    fn connection_info(&self, host_port: u16) -> ConnectionInfo {
        let db = self.database.as_deref().unwrap_or("mysql");
        let user = self.user.as_deref().unwrap_or("root");
        let password = self.password.as_ref().or(Some(&self.root_password));

        ConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: host_port,
            database: db.to_string(),
            user: user.to_string(),
            password: password.cloned(),
            connection_string: Some(format!(
                "mysql://{}:{}@127.0.0.1:{host_port}/{db}",
                user,
                password.map(|p| p.as_str()).unwrap_or(""),
            )),
        }
    }
}

/// Per-workspace MySQL/MariaDB provider: the shared local-engine backend
/// specialized with [`MySQLEngine`].
pub type MySQLLocalProvider = LocalEngineBackend<MySQLEngine>;

impl MySQLLocalProvider {
    pub fn new(
        project_name: &str,
        service_name: &str,
        config: &MySQLConfig,
        docker_settings: Option<&DockerCustomSettings>,
    ) -> anyhow::Result<Self> {
        Self::with_engine(
            project_name,
            service_name,
            config.data_root.as_deref(),
            docker_settings,
            MySQLEngine {
                image: config.image.clone(),
                port_range_start: config.port_range_start.unwrap_or(53306),
                root_password: config.root_password.clone(),
                database: config.database.clone(),
                user: config.user.clone(),
                password: config.password.clone(),
            },
        )
    }
}

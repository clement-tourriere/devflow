use bollard::query_parameters::{InspectContainerOptions, ListContainersOptions};
use bollard::Docker;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const LABEL_DEVFLOW_MANAGED: &str = "devflow.managed";
const LABEL_COMPOSE_PROJECT: &str = "com.docker.compose.project";
const LABEL_COMPOSE_SERVICE: &str = "com.docker.compose.service";
const LABEL_COMPOSE_WORKING_DIR: &str = "com.docker.compose.project.working_dir";
const LABEL_COMPOSE_CONFIG_FILES: &str = "com.docker.compose.project.config_files";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiscoveredServiceType {
    Postgres,
    ClickHouse,
    MySQL,
    Redis,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredContainer {
    pub container_id: String,
    pub container_name: String,
    pub image: String,
    pub service_type: DiscoveredServiceType,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
    pub connection_url: String,
    pub is_compose: bool,
    pub compose_project: Option<String>,
    pub compose_service: Option<String>,
    pub command: Vec<String>,
    pub extra_env: HashMap<String, String>,
    pub restart_policy: Option<String>,
}

/// Discover running Docker containers that match known service types.
///
/// Filters out containers with `devflow.managed=true` label (managed by devflow).
/// Optionally filter by service type: "postgres", "clickhouse", "mysql", "generic" (redis).
///
/// When `project_root` is provided, discovery is scoped to Docker Compose containers
/// that belong to this project (matching compose working directory or compose file path).
/// In scoped mode, non-compose containers are excluded.
pub async fn discover_containers(
    service_type_filter: Option<&str>,
    project_root: Option<&Path>,
) -> anyhow::Result<Vec<DiscoveredContainer>> {
    let docker = Docker::connect_with_local_defaults()?;
    let scoped_project_root = project_root.and_then(normalize_path_for_match);

    let mut filters = HashMap::new();
    filters.insert("status".to_string(), vec!["running".to_string()]);

    let options = ListContainersOptions {
        all: false,
        filters: Some(filters),
        ..Default::default()
    };

    let containers = docker.list_containers(Some(options)).await?;
    let mut discovered = Vec::new();

    for container in containers {
        let image = match container.image.as_deref() {
            Some(img) => img,
            None => continue,
        };

        // Detect service type from image
        let service_type = match detect_service_type_from_image(image) {
            Some(st) => st,
            None => continue,
        };

        // Apply service type filter
        if let Some(filter) = service_type_filter {
            let matches = match filter {
                "postgres" => service_type == DiscoveredServiceType::Postgres,
                "clickhouse" => service_type == DiscoveredServiceType::ClickHouse,
                "mysql" => service_type == DiscoveredServiceType::MySQL,
                "generic" => service_type == DiscoveredServiceType::Redis,
                _ => true,
            };
            if !matches {
                continue;
            }
        }

        // Skip devflow-managed containers
        if let Some(labels) = &container.labels {
            if labels.get(LABEL_DEVFLOW_MANAGED).map(|v| v.as_str()) == Some("true") {
                continue;
            }
        }

        let container_id = match container.id.as_deref() {
            Some(id) => id,
            None => continue,
        };

        // Inspect for full details
        let inspect = match docker
            .inspect_container(container_id, Some(InspectContainerOptions { size: false }))
            .await
        {
            Ok(info) => info,
            Err(_) => continue,
        };

        let container_name = inspect
            .name
            .as_deref()
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_string();

        // Extract environment variables
        let env_vars = extract_env_vars(&inspect);

        // Extract labels
        let labels = inspect
            .config
            .as_ref()
            .and_then(|c| c.labels.clone())
            .unwrap_or_default();

        // Extract host and port
        let default_port = default_port(&service_type);
        let (host, port) = extract_host_port(&inspect, default_port);

        // Extract credentials
        let (username, password, database) = extract_credentials(&env_vars, &service_type);

        // Compose metadata
        let compose_project = labels.get(LABEL_COMPOSE_PROJECT).cloned();
        let compose_service = labels.get(LABEL_COMPOSE_SERVICE).cloned();
        let is_compose = is_compose_container(&labels);

        if let Some(project_root) = scoped_project_root.as_deref() {
            if !is_compose || !container_matches_project_scope(&labels, project_root) {
                continue;
            }
        }

        let connection_url = build_connection_url(
            &service_type,
            &host,
            port,
            username.as_deref(),
            password.as_deref(),
            database.as_deref(),
        );

        let (command, extra_env, restart_policy) =
            extract_docker_settings(&inspect, &service_type);

        discovered.push(DiscoveredContainer {
            container_id: container_id.to_string(),
            container_name,
            image: image.to_string(),
            service_type,
            host,
            port,
            username,
            password,
            database,
            connection_url,
            is_compose,
            compose_project,
            compose_service,
            command,
            extra_env,
            restart_policy,
        });
    }

    Ok(discovered)
}

fn is_compose_container(labels: &HashMap<String, String>) -> bool {
    labels.contains_key(LABEL_COMPOSE_PROJECT)
        || labels.contains_key(LABEL_COMPOSE_SERVICE)
        || labels.contains_key(LABEL_COMPOSE_WORKING_DIR)
        || labels.contains_key(LABEL_COMPOSE_CONFIG_FILES)
}

fn container_matches_project_scope(labels: &HashMap<String, String>, project_root: &Path) -> bool {
    let compose_working_dir = labels
        .get(LABEL_COMPOSE_WORKING_DIR)
        .and_then(|working_dir| normalize_path_for_match(Path::new(working_dir)));

    if let Some(path) = compose_working_dir.as_deref() {
        if path == project_root {
            return true;
        }
    }

    if let Some(config_files) = labels.get(LABEL_COMPOSE_CONFIG_FILES) {
        for config_file in config_files
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            let path = Path::new(config_file);
            let normalized = if path.is_absolute() {
                normalize_path_for_match(path)
            } else if let Some(working_dir) = compose_working_dir.as_ref() {
                normalize_path_for_match(&working_dir.join(path))
            } else {
                normalize_path_for_match(path)
            };

            if let Some(config_path) = normalized {
                if let Some(parent) = config_path.parent() {
                    if parent == project_root {
                        return true;
                    }
                }
            }
        }
    }

    false
}

fn normalize_path_for_match(path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };

    match std::fs::canonicalize(&absolute) {
        Ok(canonical) => Some(canonical),
        Err(_) => Some(absolute),
    }
}

/// Detect service type from Docker image name.
pub fn detect_service_type_from_image(image: &str) -> Option<DiscoveredServiceType> {
    let lower = image.to_lowercase();

    if lower.contains("postgres")
        || lower.contains("pgvector")
        || lower.contains("postgis")
        || lower.contains("timescaledb")
    {
        Some(DiscoveredServiceType::Postgres)
    } else if lower.contains("clickhouse") {
        Some(DiscoveredServiceType::ClickHouse)
    } else if lower.contains("mysql") || lower.contains("mariadb") {
        Some(DiscoveredServiceType::MySQL)
    } else if lower.contains("redis") || lower.contains("valkey") || lower.contains("dragonfly") {
        Some(DiscoveredServiceType::Redis)
    } else {
        None
    }
}

/// Extract credentials from environment variables based on service type.
pub fn extract_credentials(
    env: &HashMap<String, String>,
    service_type: &DiscoveredServiceType,
) -> (Option<String>, Option<String>, Option<String>) {
    match service_type {
        DiscoveredServiceType::Postgres => {
            let user = env
                .get("POSTGRES_USER")
                .or_else(|| env.get("PGUSER"))
                .cloned()
                .or_else(|| Some("postgres".to_string()));
            let password = env
                .get("POSTGRES_PASSWORD")
                .or_else(|| env.get("PGPASSWORD"))
                .cloned();
            let database = env
                .get("POSTGRES_DB")
                .or_else(|| env.get("PGDATABASE"))
                .cloned()
                .or_else(|| user.clone());
            (user, password, database)
        }
        DiscoveredServiceType::MySQL => {
            let user = env
                .get("MYSQL_USER")
                .cloned()
                .or_else(|| Some("root".to_string()));
            let password = env
                .get("MYSQL_PASSWORD")
                .or_else(|| env.get("MYSQL_ROOT_PASSWORD"))
                .cloned();
            let database = env.get("MYSQL_DATABASE").cloned();
            (user, password, database)
        }
        DiscoveredServiceType::ClickHouse => {
            let user = env
                .get("CLICKHOUSE_USER")
                .cloned()
                .or_else(|| Some("default".to_string()));
            let password = env.get("CLICKHOUSE_PASSWORD").cloned();
            // If CLICKHOUSE_SKIP_USER_SETUP=1, no password needed
            let password = if env.get("CLICKHOUSE_SKIP_USER_SETUP").map(|v| v.as_str()) == Some("1")
            {
                None
            } else {
                password
            };
            let database = env.get("CLICKHOUSE_DB").cloned();
            (user, password, database)
        }
        DiscoveredServiceType::Redis => {
            let password = env.get("REDIS_PASSWORD").cloned();
            (None, password, None)
        }
    }
}

/// Extract host and port from container inspection.
///
/// Returns ("localhost", mapped_port) if the container has a published port mapping,
/// otherwise falls back to (container_ip, default_port).
fn extract_host_port(
    inspect: &bollard::models::ContainerInspectResponse,
    default_port: u16,
) -> (String, u16) {
    // Try host port mapping first
    if let Some(network_settings) = &inspect.network_settings {
        if let Some(ports) = &network_settings.ports {
            let port_key = format!("{default_port}/tcp");
            if let Some(Some(bindings)) = ports.get(&port_key) {
                if let Some(binding) = bindings.first() {
                    if let Some(host_port_str) = &binding.host_port {
                        if let Ok(host_port) = host_port_str.parse::<u16>() {
                            return ("localhost".to_string(), host_port);
                        }
                    }
                }
            }
        }

        // Fallback: container IP from networks
        if let Some(networks) = &network_settings.networks {
            for network in networks.values() {
                if let Some(ip) = &network.ip_address {
                    if !ip.is_empty() {
                        return (ip.clone(), default_port);
                    }
                }
            }
        }
    }

    // OrbStack fallback on macOS
    if cfg!(target_os = "macos") {
        let name = inspect
            .name
            .as_deref()
            .unwrap_or_default()
            .trim_start_matches('/');
        if !name.is_empty() {
            // Check for compose domain
            if let Some(config) = &inspect.config {
                if let Some(labels) = &config.labels {
                    if let (Some(project), Some(service)) = (
                        labels.get(LABEL_COMPOSE_PROJECT),
                        labels.get(LABEL_COMPOSE_SERVICE),
                    ) {
                        return (format!("{service}.{project}.orb.local"), default_port);
                    }
                }
            }
            return (format!("{name}.orb.local"), default_port);
        }
    }

    ("localhost".to_string(), default_port)
}

/// Extract environment variables from container inspection.
fn extract_env_vars(
    inspect: &bollard::models::ContainerInspectResponse,
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    if let Some(config) = &inspect.config {
        if let Some(env_list) = &config.env {
            for entry in env_list {
                if let Some((key, value)) = entry.split_once('=') {
                    env.insert(key.to_string(), value.to_string());
                }
            }
        }
    }
    env
}

/// Build a connection URL for the given service type.
pub fn build_connection_url(
    service_type: &DiscoveredServiceType,
    host: &str,
    port: u16,
    username: Option<&str>,
    password: Option<&str>,
    database: Option<&str>,
) -> String {
    match service_type {
        DiscoveredServiceType::Postgres => {
            let user = username.unwrap_or("postgres");
            let db = database.unwrap_or(user);
            match password {
                Some(pass) => format!("postgres://{user}:{pass}@{host}:{port}/{db}"),
                None => format!("postgres://{user}@{host}:{port}/{db}"),
            }
        }
        DiscoveredServiceType::MySQL => {
            let user = username.unwrap_or("root");
            let db_part = database.map(|d| format!("/{d}")).unwrap_or_default();
            match password {
                Some(pass) => format!("mysql://{user}:{pass}@{host}:{port}{db_part}"),
                None => format!("mysql://{user}@{host}:{port}{db_part}"),
            }
        }
        DiscoveredServiceType::ClickHouse => {
            let user = username.unwrap_or("default");
            let db = database.unwrap_or("default");
            match password {
                Some(pass) => format!("clickhouse://{user}:{pass}@{host}:{port}/{db}"),
                None => format!("clickhouse://{user}@{host}:{port}/{db}"),
            }
        }
        DiscoveredServiceType::Redis => match password {
            Some(pass) => format!("redis://:{pass}@{host}:{port}"),
            None => format!("redis://{host}:{port}"),
        },
    }
}

/// Default port for a service type.
pub fn default_port(service_type: &DiscoveredServiceType) -> u16 {
    match service_type {
        DiscoveredServiceType::Postgres => 5432,
        DiscoveredServiceType::MySQL => 3306,
        DiscoveredServiceType::ClickHouse => 8123,
        DiscoveredServiceType::Redis => 6379,
    }
}

/// Extract custom Docker settings (command, extra environment, restart policy) from a container inspect.
fn extract_docker_settings(
    inspect: &bollard::models::ContainerInspectResponse,
    service_type: &DiscoveredServiceType,
) -> (Vec<String>, HashMap<String, String>, Option<String>) {
    // Command
    let command = inspect
        .config
        .as_ref()
        .and_then(|c| c.cmd.clone())
        .unwrap_or_default();

    // Extra environment — filter out well-known keys
    let known_env_keys: HashSet<&str> = match service_type {
        DiscoveredServiceType::Postgres => [
            "POSTGRES_USER",
            "POSTGRES_PASSWORD",
            "POSTGRES_DB",
            "PGUSER",
            "PGPASSWORD",
            "PGDATABASE",
            "PGDATA",
            "PG_MAJOR",
            "PG_VERSION",
            "PG_SHA256",
        ]
        .into_iter()
        .collect(),
        DiscoveredServiceType::MySQL => [
            "MYSQL_ROOT_PASSWORD",
            "MYSQL_USER",
            "MYSQL_PASSWORD",
            "MYSQL_DATABASE",
            "MYSQL_MAJOR",
            "MYSQL_VERSION",
            "MARIADB_ROOT_PASSWORD",
            "MARIADB_USER",
            "MARIADB_PASSWORD",
            "MARIADB_DATABASE",
        ]
        .into_iter()
        .collect(),
        DiscoveredServiceType::ClickHouse => [
            "CLICKHOUSE_USER",
            "CLICKHOUSE_PASSWORD",
            "CLICKHOUSE_DB",
            "CLICKHOUSE_SKIP_USER_SETUP",
        ]
        .into_iter()
        .collect(),
        DiscoveredServiceType::Redis => ["REDIS_PASSWORD"].into_iter().collect(),
    };
    let always_skip: HashSet<&str> = [
        "PATH",
        "GOSU_VERSION",
        "LANG",
        "LANGUAGE",
        "LC_ALL",
        "HOME",
        "HOSTNAME",
    ]
    .into_iter()
    .collect();

    let env_vars = extract_env_vars(inspect);
    let extra_env: HashMap<String, String> = env_vars
        .into_iter()
        .filter(|(k, _)| !known_env_keys.contains(k.as_str()) && !always_skip.contains(k.as_str()))
        .collect();

    // Restart policy
    let restart_policy = inspect
        .host_config
        .as_ref()
        .and_then(|hc| hc.restart_policy.as_ref())
        .and_then(|rp| rp.name.as_ref())
        .and_then(|name| {
            let name_str = name.to_string();
            if name_str.is_empty() || name_str == "no" {
                None
            } else {
                // Append max retry count for on-failure
                if let Some(count) = inspect
                    .host_config
                    .as_ref()
                    .and_then(|hc| hc.restart_policy.as_ref())
                    .and_then(|rp| rp.maximum_retry_count)
                {
                    if name_str == "on-failure" && count > 0 {
                        return Some(format!("on-failure:{count}"));
                    }
                }
                Some(name_str)
            }
        });

    (command, extra_env, restart_policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_compose_paths() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let project = tempfile::tempdir().unwrap();
        let project_root = project.path().to_path_buf();
        let compose_file = project_root.join("docker-compose.yml");
        let override_file = project_root.join("docker-compose.override.yml");

        fs::write(&compose_file, "services: {}\n").unwrap();
        fs::write(&override_file, "services: {}\n").unwrap();

        (project, project_root, compose_file, override_file)
    }

    #[test]
    fn test_detect_service_type_from_image() {
        // Postgres variants
        assert_eq!(
            detect_service_type_from_image("postgres:17"),
            Some(DiscoveredServiceType::Postgres)
        );
        assert_eq!(
            detect_service_type_from_image("pgvector/pgvector:pg16"),
            Some(DiscoveredServiceType::Postgres)
        );
        assert_eq!(
            detect_service_type_from_image("postgis/postgis:16-3.4"),
            Some(DiscoveredServiceType::Postgres)
        );
        assert_eq!(
            detect_service_type_from_image("timescale/timescaledb:latest-pg16"),
            Some(DiscoveredServiceType::Postgres)
        );

        // ClickHouse
        assert_eq!(
            detect_service_type_from_image("clickhouse/clickhouse-server:23.8"),
            Some(DiscoveredServiceType::ClickHouse)
        );
        assert_eq!(
            detect_service_type_from_image("yandex/clickhouse-server:latest"),
            Some(DiscoveredServiceType::ClickHouse)
        );

        // MySQL
        assert_eq!(
            detect_service_type_from_image("mysql:8.0"),
            Some(DiscoveredServiceType::MySQL)
        );
        assert_eq!(
            detect_service_type_from_image("mariadb:10.5"),
            Some(DiscoveredServiceType::MySQL)
        );

        // Redis variants
        assert_eq!(
            detect_service_type_from_image("redis:7"),
            Some(DiscoveredServiceType::Redis)
        );
        assert_eq!(
            detect_service_type_from_image("valkey/valkey:latest"),
            Some(DiscoveredServiceType::Redis)
        );
        assert_eq!(
            detect_service_type_from_image("docker.dragonflydb.io/dragonflydb/dragonfly"),
            Some(DiscoveredServiceType::Redis)
        );

        // Not a known service
        assert_eq!(detect_service_type_from_image("nginx:latest"), None);
        assert_eq!(detect_service_type_from_image("node:20"), None);
    }

    #[test]
    fn test_extract_credentials_postgres() {
        let mut env = HashMap::new();
        env.insert("POSTGRES_USER".to_string(), "myuser".to_string());
        env.insert("POSTGRES_PASSWORD".to_string(), "secret".to_string());
        env.insert("POSTGRES_DB".to_string(), "mydb".to_string());

        let (user, pass, db) = extract_credentials(&env, &DiscoveredServiceType::Postgres);
        assert_eq!(user.as_deref(), Some("myuser"));
        assert_eq!(pass.as_deref(), Some("secret"));
        assert_eq!(db.as_deref(), Some("mydb"));
    }

    #[test]
    fn test_extract_credentials_postgres_defaults() {
        let env = HashMap::new();
        let (user, pass, db) = extract_credentials(&env, &DiscoveredServiceType::Postgres);
        assert_eq!(user.as_deref(), Some("postgres"));
        assert_eq!(pass, None);
        assert_eq!(db.as_deref(), Some("postgres")); // defaults to username
    }

    #[test]
    fn test_extract_credentials_mysql() {
        let mut env = HashMap::new();
        env.insert("MYSQL_ROOT_PASSWORD".to_string(), "rootpass".to_string());
        env.insert("MYSQL_DATABASE".to_string(), "appdb".to_string());

        let (user, pass, db) = extract_credentials(&env, &DiscoveredServiceType::MySQL);
        assert_eq!(user.as_deref(), Some("root"));
        assert_eq!(pass.as_deref(), Some("rootpass"));
        assert_eq!(db.as_deref(), Some("appdb"));
    }

    #[test]
    fn test_extract_credentials_clickhouse_skip_setup() {
        let mut env = HashMap::new();
        env.insert("CLICKHOUSE_USER".to_string(), "default".to_string());
        env.insert("CLICKHOUSE_PASSWORD".to_string(), "ch_pass".to_string());
        env.insert("CLICKHOUSE_SKIP_USER_SETUP".to_string(), "1".to_string());

        let (user, pass, _db) = extract_credentials(&env, &DiscoveredServiceType::ClickHouse);
        assert_eq!(user.as_deref(), Some("default"));
        assert_eq!(pass, None); // skipped due to SKIP_USER_SETUP=1
    }

    #[test]
    fn test_extract_credentials_redis() {
        let mut env = HashMap::new();
        env.insert("REDIS_PASSWORD".to_string(), "redis_secret".to_string());

        let (user, pass, db) = extract_credentials(&env, &DiscoveredServiceType::Redis);
        assert_eq!(user, None);
        assert_eq!(pass.as_deref(), Some("redis_secret"));
        assert_eq!(db, None);
    }

    #[test]
    fn test_build_connection_url_postgres() {
        let url = build_connection_url(
            &DiscoveredServiceType::Postgres,
            "localhost",
            5432,
            Some("myuser"),
            Some("mypass"),
            Some("mydb"),
        );
        assert_eq!(url, "postgres://myuser:mypass@localhost:5432/mydb");
    }

    #[test]
    fn test_build_connection_url_postgres_no_password() {
        let url = build_connection_url(
            &DiscoveredServiceType::Postgres,
            "localhost",
            5432,
            Some("postgres"),
            None,
            Some("postgres"),
        );
        assert_eq!(url, "postgres://postgres@localhost:5432/postgres");
    }

    #[test]
    fn test_build_connection_url_mysql() {
        let url = build_connection_url(
            &DiscoveredServiceType::MySQL,
            "localhost",
            3306,
            Some("root"),
            Some("pass"),
            Some("appdb"),
        );
        assert_eq!(url, "mysql://root:pass@localhost:3306/appdb");
    }

    #[test]
    fn test_build_connection_url_clickhouse() {
        let url = build_connection_url(
            &DiscoveredServiceType::ClickHouse,
            "localhost",
            8123,
            Some("default"),
            None,
            Some("default"),
        );
        assert_eq!(url, "clickhouse://default@localhost:8123/default");
    }

    #[test]
    fn test_build_connection_url_redis() {
        let url = build_connection_url(
            &DiscoveredServiceType::Redis,
            "localhost",
            6379,
            None,
            Some("secret"),
            None,
        );
        assert_eq!(url, "redis://:secret@localhost:6379");
    }

    #[test]
    fn test_build_connection_url_redis_no_auth() {
        let url = build_connection_url(
            &DiscoveredServiceType::Redis,
            "localhost",
            6379,
            None,
            None,
            None,
        );
        assert_eq!(url, "redis://localhost:6379");
    }

    #[test]
    fn test_default_port() {
        assert_eq!(default_port(&DiscoveredServiceType::Postgres), 5432);
        assert_eq!(default_port(&DiscoveredServiceType::MySQL), 3306);
        assert_eq!(default_port(&DiscoveredServiceType::ClickHouse), 8123);
        assert_eq!(default_port(&DiscoveredServiceType::Redis), 6379);
    }

    #[test]
    fn test_container_matches_project_scope_by_working_dir() {
        let (_tmp, project_root, compose_file, _override_file) = temp_compose_paths();
        let normalized_root = normalize_path_for_match(&project_root).unwrap();

        let mut labels = HashMap::new();
        labels.insert(
            LABEL_COMPOSE_WORKING_DIR.to_string(),
            project_root.display().to_string(),
        );
        labels.insert(
            LABEL_COMPOSE_CONFIG_FILES.to_string(),
            compose_file.display().to_string(),
        );

        assert!(container_matches_project_scope(&labels, &normalized_root));
    }

    #[test]
    fn test_container_matches_project_scope_by_config_files() {
        let (_tmp, project_root, compose_file, override_file) = temp_compose_paths();
        let normalized_root = normalize_path_for_match(&project_root).unwrap();

        let mut labels = HashMap::new();
        labels.insert(
            LABEL_COMPOSE_CONFIG_FILES.to_string(),
            format!("{},{}", compose_file.display(), override_file.display()),
        );

        assert!(container_matches_project_scope(&labels, &normalized_root));
    }

    #[test]
    fn test_container_matches_project_scope_rejects_foreign_project() {
        let (_tmp, project_root, _compose_file, _override_file) = temp_compose_paths();
        let foreign = tempfile::tempdir().unwrap();
        let foreign_root = foreign.path().to_path_buf();
        let foreign_compose = foreign_root.join("docker-compose.yml");
        fs::write(&foreign_compose, "services: {}\n").unwrap();

        let normalized_root = normalize_path_for_match(&project_root).unwrap();

        let mut labels = HashMap::new();
        labels.insert(
            LABEL_COMPOSE_WORKING_DIR.to_string(),
            foreign_root.display().to_string(),
        );
        labels.insert(
            LABEL_COMPOSE_CONFIG_FILES.to_string(),
            foreign_compose.display().to_string(),
        );

        assert!(!container_matches_project_scope(&labels, &normalized_root));
    }

    #[test]
    fn test_container_matches_project_scope_with_env_renamed_compose_project() {
        let (_tmp, project_root, compose_file, _override_file) = temp_compose_paths();
        let normalized_root = normalize_path_for_match(&project_root).unwrap();

        let mut labels = HashMap::new();
        labels.insert(
            LABEL_COMPOSE_PROJECT.to_string(),
            "custom_name_from_env".to_string(),
        );
        labels.insert(
            LABEL_COMPOSE_WORKING_DIR.to_string(),
            project_root.display().to_string(),
        );
        labels.insert(
            LABEL_COMPOSE_CONFIG_FILES.to_string(),
            compose_file.display().to_string(),
        );

        assert!(container_matches_project_scope(&labels, &normalized_root));
    }
}

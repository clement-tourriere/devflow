//! Archived review probes: all five fail at commit 16c15243.
//! Copy into crates/devflow-core/tests/review_probe.rs to run explicitly.
//! Only disposable, uniquely named Docker resources are used.
use devflow_core::config::{Config, SharedServiceConfig};
use devflow_core::services::factory::create_provider_from_named_config;
use devflow_core::services::shared::SharedPostgresProvider;
use devflow_core::services::ServiceProvider;

struct ContainerCleanup(String);
impl Drop for ContainerCleanup {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &self.0])
            .output();
        let _ = std::process::Command::new("docker")
            .args(["volume", "rm", &format!("{}-data", self.0)])
            .output();
    }
}

#[tokio::test]
async fn shared_project_cleanup_must_preserve_neighbor_project() {
    let name = format!("devflow-review-prefix-{}", std::process::id());
    let _cleanup = ContainerCleanup(name.clone());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let cfg = SharedServiceConfig {
        container_name: Some(name),
        port: Some(port),
        ..Default::default()
    };
    let a = SharedPostgresProvider::new("review", Some(&cfg)).unwrap();
    let b = SharedPostgresProvider::new("review_child", Some(&cfg)).unwrap();
    a.create_workspace("main", None).await.unwrap();
    b.create_workspace("main", None).await.unwrap();
    let listed = a.list_workspaces().await.unwrap();
    println!("Project review inventory: {:?}", listed);
    let dropped = a.destroy_project().await.unwrap();
    println!("Project review destroy removed: {:?}", dropped);
    assert!(b.workspace_exists("main").await.unwrap(), "destroying review also deleted review_child_main");
}

#[tokio::test]
async fn shared_named_services_must_not_alias() {
    let config: Config = serde_yaml_ng::from_str("name: review\nservices:\n  - name: primary\n    type: shared\n  - name: analytics\n    type: shared\n").unwrap();
    let services = config.resolve_services();
    let a = create_provider_from_named_config(&config, &services[0]).await.unwrap();
    let b = create_provider_from_named_config(&config, &services[1]).await.unwrap();
    let ca = a.get_connection_info("main").await.unwrap();
    let cb = b.get_connection_info("main").await.unwrap();
    assert_ne!(ca.connection_string, cb.connection_string, "separate named services alias one database");
}

#[tokio::test]
async fn shared_backend_must_preserve_distinct_workspace_keys() {
    let config = Config::default();
    let a = config.get_service_workspace_key("main");
    let b = config.get_service_workspace_key("_main");
    assert_ne!(a, b);
    let provider = SharedPostgresProvider::new("review", None).unwrap();
    let ca = provider.get_connection_info(&a).await.unwrap();
    let cb = provider.get_connection_info(&b).await.unwrap();
    assert_ne!(ca.database, cb.database, "main and _main reach the same database");
}

#[tokio::test]
async fn invalid_seed_must_return_error() {
    let name = format!("devflow-review-seed-{}", std::process::id());
    let _cleanup = ContainerCleanup(name.clone());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let cfg = SharedServiceConfig { container_name: Some(name.clone()), port: Some(port), ..Default::default() };
    let provider = SharedPostgresProvider::new("reviewseed", Some(&cfg)).unwrap();
    let info = provider.create_workspace("main", None).await.unwrap();
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let path = temp.path().join("invalid.dump");
    std::fs::write(&path, b"this is not a PostgreSQL dump").unwrap();
    let docker = devflow_core::services::shared::container::connect().unwrap();
    let result = devflow_core::services::postgres::local::seed::seed_branch(
        &docker,
        &devflow_core::services::postgres::local::seed::SeedSource::LocalFile(path),
        &name,
        "postgres",
        &info.database_name,
        "postgres:17",
    ).await;
    println!("Invalid dump restore returned: {:?}", result);
    assert!(result.is_err(), "an invalid dump was reported as a successful restore");
}

struct FixtureEngine;
impl devflow_core::services::local_engine::LocalEngineSpec for FixtureEngine {
    fn kind(&self) -> &'static str { "review" }
    fn display_name(&self) -> &'static str { "Review fixture" }
    fn provider_name(&self) -> &'static str { "Review fixture" }
    fn image(&self) -> &str { "redis:7-alpine" }
    fn port_range_start(&self) -> u16 { 56980 }
    fn data_mount_path(&self) -> &'static str { "/data" }
    fn port_layout(&self) -> devflow_core::services::local_engine::PortLayout {
        devflow_core::services::local_engine::PortLayout::Single { container_port: 6379 }
    }
    fn env(&self) -> Vec<String> { vec![] }
    fn readiness_command(&self) -> Vec<String> { vec!["redis-cli".into(), "ping".into()] }
    fn restart_ready_timeout(&self) -> std::time::Duration { std::time::Duration::from_secs(10) }
    fn connection_info(&self, port: u16) -> devflow_core::services::ConnectionInfo {
        devflow_core::services::ConnectionInfo { host: "127.0.0.1".into(), port, database: "0".into(), user: String::new(), password: None, connection_string: None }
    }
}

#[tokio::test]
async fn local_engine_projects_must_not_share_storage() {
    use devflow_core::services::local_engine::LocalEngineBackend;
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let project_a = format!("reviewa{}", std::process::id());
    let project_b = format!("reviewb{}", std::process::id());
    let name_a = format!("devflow-{project_a}-db-main");
    let name_b = format!("devflow-{project_b}-db-main");
    let _cleanup_a = ContainerCleanup(name_a.clone());
    let _cleanup_b = ContainerCleanup(name_b.clone());
    let a = LocalEngineBackend::with_engine(&project_a, "db", temp.path().to_str(), None, FixtureEngine).unwrap();
    let b = LocalEngineBackend::with_engine(&project_b, "db", temp.path().to_str(), None, FixtureEngine).unwrap();
    a.create_workspace("main", None).await.unwrap();
    b.create_workspace("main", None).await.unwrap();
    let docker = devflow_core::services::shared::container::connect().unwrap();
    let inspect_a = docker.inspect_container(&name_a, None).await.unwrap();
    let inspect_b = docker.inspect_container(&name_b, None).await.unwrap();
    let source_a = inspect_a.mounts.unwrap().into_iter().find(|m| m.destination.as_deref() == Some("/data")).unwrap().source;
    let source_b = inspect_b.mounts.unwrap().into_iter().find(|m| m.destination.as_deref() == Some("/data")).unwrap().source;
    println!("Project A mount: {:?}; project B mount: {:?}", source_a, source_b);
    assert_ne!(source_a, source_b, "different projects mount the same workspace data directory");
}

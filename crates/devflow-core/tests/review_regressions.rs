//! Regression tests use disposable Docker resources only.
#![cfg(feature = "service-local")]
mod support;
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
#[ignore = "requires Docker"]
async fn shared_project_cleanup_must_preserve_neighbor_project() {
    let _environment = support::isolated_config();
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
    assert!(
        b.workspace_exists("main").await.unwrap(),
        "destroying review also deleted review_child_main"
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn shared_named_services_must_not_alias() {
    let _environment = support::isolated_config();
    let config: Config = serde_yaml_ng::from_str("name: review\nservices:\n  - name: primary\n    type: shared\n  - name: analytics\n    type: shared\n").unwrap();
    let mut services = config.resolve_services();
    for service in &mut services {
        service.shared = Some(SharedServiceConfig {
            container_name: Some(format!("devflow-review-unused-{}", std::process::id())),
            ..Default::default()
        });
    }
    let a = create_provider_from_named_config(&config, &services[0])
        .await
        .unwrap();
    let b = create_provider_from_named_config(&config, &services[1])
        .await
        .unwrap();
    let ca = a.get_connection_info("main").await.unwrap();
    let cb = b.get_connection_info("main").await.unwrap();
    assert_ne!(
        ca.connection_string, cb.connection_string,
        "separate named services alias one database"
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn shared_backend_must_preserve_distinct_workspace_keys() {
    let _environment = support::isolated_config();
    let config = Config::default();
    let a = config.get_service_workspace_key("main");
    let b = config.get_service_workspace_key("_main");
    assert_ne!(a, b);
    let cfg = SharedServiceConfig {
        container_name: Some(format!("devflow-review-unused-{}", std::process::id())),
        ..Default::default()
    };
    let provider = SharedPostgresProvider::new("review", Some(&cfg)).unwrap();
    let ca = provider.get_connection_info(&a).await.unwrap();
    let cb = provider.get_connection_info(&b).await.unwrap();
    assert_ne!(
        ca.database, cb.database,
        "main and _main reach the same database"
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn invalid_seed_must_return_error() {
    let _environment = support::isolated_config();
    let name = format!("devflow-review-seed-{}", std::process::id());
    let _cleanup = ContainerCleanup(name.clone());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let cfg = SharedServiceConfig {
        container_name: Some(name.clone()),
        port: Some(port),
        ..Default::default()
    };
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
    )
    .await;
    println!("Invalid dump restore returned: {:?}", result);
    assert!(
        result.is_err(),
        "an invalid dump was reported as a successful restore"
    );

    let sql = temp.path().join("invalid.sql");
    std::fs::write(
        &sql,
        b"CREATE TABLE must_roll_back(id int); SELECT * FROM nonexistent_table;",
    )
    .unwrap();
    assert!(devflow_core::services::postgres::local::seed::seed_branch(
        &docker,
        &devflow_core::services::postgres::local::seed::SeedSource::LocalFile(sql),
        &name,
        "postgres",
        &info.database_name,
        "postgres:17",
    )
    .await
    .is_err());
    let output = devflow_core::services::shared::container::exec_capture(
        &docker,
        &name,
        &[
            "psql",
            "-U",
            "postgres",
            "-d",
            &info.database_name,
            "-Atc",
            "SELECT to_regclass('must_roll_back') IS NULL",
        ],
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        output.stdout.trim(),
        "t",
        "failed SQL seed must roll back its earlier statements"
    );

    let mismatched = SharedServiceConfig {
        port: Some(port + 1),
        ..cfg.clone()
    };
    let wrong = SharedPostgresProvider::new("reviewseed", Some(&mismatched)).unwrap();
    assert!(
        wrong.workspace_exists("main").await.is_err(),
        "observational reads must detect conflicting shared settings"
    );
}

struct FixtureEngine;

#[tokio::test]
#[ignore = "requires Docker"]
async fn generic_containers_preserve_project_and_raw_workspace_identity() {
    let _environment = support::isolated_config();
    let mut config: Config = serde_yaml_ng::from_str("name: reviewgeneric\nservices:\n  - name: cache\n    service_type: generic\n    generic:\n      image: redis:7-alpine\n      port_mapping: '6379:6379'\n      healthcheck: redis-cli ping\n").unwrap();
    config.project_id = Some(format!("generic-a-{}", uuid::Uuid::new_v4()));
    let named = config.resolve_services().remove(0);
    let a = create_provider_from_named_config(&config, &named)
        .await
        .unwrap();
    config.project_id = Some(format!("generic-b-{}", uuid::Uuid::new_v4()));
    let b = create_provider_from_named_config(&config, &named)
        .await
        .unwrap();
    let main = a
        .create_workspace("main", None)
        .await
        .unwrap()
        .database_name;
    let _main_cleanup = ContainerCleanup(main.clone());
    let child = a
        .create_workspace("_main", None)
        .await
        .unwrap()
        .database_name;
    let _child_cleanup = ContainerCleanup(child.clone());
    let neighbor = b
        .create_workspace("main", None)
        .await
        .unwrap()
        .database_name;
    let _neighbor_cleanup = ContainerCleanup(neighbor.clone());
    assert_ne!(main, child);
    assert_ne!(main, neighbor);
    let docker = devflow_core::services::shared::container::connect().unwrap();
    docker.pause_container(&main).await.unwrap();
    a.create_workspace("main", None).await.unwrap();
    assert_eq!(
        devflow_core::services::local_docker::inspect_container_status(&docker, &main)
            .await
            .unwrap(),
        devflow_core::services::local_docker::ContainerStatus::Running
    );
    assert_eq!(a.list_workspaces().await.unwrap().len(), 2);
    assert_eq!(b.list_workspaces().await.unwrap().len(), 1);
    a.destroy_project().await.unwrap();
    assert!(b.workspace_exists("main").await.unwrap());
}

struct LocalProjectCleanup(std::path::PathBuf);
impl Drop for LocalProjectCleanup {
    fn drop(&mut self) {
        use devflow_core::services::postgres::local::state::Store;
        if let Ok(store) = Store::open(&self.0) {
            for project in store.list_projects().unwrap_or_default() {
                for workspace in store.list_workspaces(&project.id).unwrap_or_default() {
                    drop(ContainerCleanup(workspace.container_name));
                }
            }
        }
    }
}

async fn local_sql(store_path: &std::path::Path, workspace: &str, sql: &str) -> String {
    use devflow_core::services::postgres::local::state::Store;
    let store = Store::open(store_path).unwrap();
    let project = store.list_projects().unwrap().remove(0);
    let workspace = store
        .get_workspace_by_name(&project.id, workspace)
        .unwrap()
        .unwrap();
    let docker = devflow_core::services::shared::container::connect().unwrap();
    let output = devflow_core::services::shared::container::exec_capture(
        &docker,
        &workspace.container_name,
        &["psql", "-U", "postgres", "-d", "postgres", "-Atc", sql],
        None,
    )
    .await
    .unwrap();
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    output.stdout.trim().to_string()
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn local_postgres_baseline_clone_reset_and_seed_recovery() {
    let _environment = support::isolated_config();
    use devflow_core::config::LocalServiceConfig;
    use devflow_core::services::postgres::local::LocalProvider;
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let store_path = temp.path().join("state.db");
    let _cleanup = LocalProjectCleanup(store_path.clone());
    let config = Config {
        project_id: Some(format!("review-local-{}", uuid::Uuid::new_v4())),
        ..Config::default()
    };
    let local = LocalServiceConfig {
        data_root: Some(temp.path().to_string_lossy().into_owned()),
        storage: Some("copy".into()),
        image: Some("postgres:17".into()),
        port_range_start: Some(57300),
        ..Default::default()
    };
    let provider = LocalProvider::new("db", &config, Some(&local), None)
        .await
        .unwrap();
    provider.create_workspace("main", None).await.unwrap();
    let seed = temp.path().join("baseline.sql");
    std::fs::write(
        &seed,
        "CREATE TABLE marker(value text); INSERT INTO marker VALUES ('baseline');",
    )
    .unwrap();
    provider
        .seed_from_source("main", seed.to_str().unwrap())
        .await
        .unwrap();

    // Competing clones must serialize access to the parent without corrupting it.
    let (a, b) = tokio::join!(
        provider.create_workspace("_main", Some("main")),
        provider.create_workspace("sibling", Some("main")),
    );
    a.unwrap();
    b.unwrap();
    local_sql(&store_path, "_main", "UPDATE marker SET value = 'child'").await;
    assert_eq!(
        local_sql(&store_path, "main", "SELECT value FROM marker").await,
        "baseline"
    );
    assert_eq!(
        local_sql(&store_path, "sibling", "SELECT value FROM marker").await,
        "baseline"
    );
    provider.reset_workspace("_main").await.unwrap();
    assert_eq!(
        local_sql(&store_path, "_main", "SELECT value FROM marker").await,
        "baseline"
    );

    let invalid = temp.path().join("invalid.sql");
    std::fs::write(&invalid, "SELECT * FROM nonexistent_table;").unwrap();
    assert!(provider
        .seed_from_source("_main", invalid.to_str().unwrap())
        .await
        .is_err());
    provider.stop_workspace("_main").await.unwrap();
    assert_eq!(
        provider
            .list_workspaces()
            .await
            .unwrap()
            .into_iter()
            .find(|w| w.name == "_main")
            .unwrap()
            .state
            .as_deref(),
        Some("failed")
    );
    assert!(provider.start_workspace("_main").await.is_err());
    assert!(provider
        .create_workspace("bad-child", Some("_main"))
        .await
        .is_err());
    let repair = temp.path().join("repair.sql");
    std::fs::write(&repair, "UPDATE marker SET value = 'repaired';").unwrap();
    provider
        .seed_from_source("_main", repair.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        local_sql(&store_path, "_main", "SELECT value FROM marker").await,
        "repaired"
    );
    provider.delete_workspace("sibling").await.unwrap();
    provider.delete_workspace("sibling").await.unwrap();
    provider.destroy_project().await.unwrap();
    assert!(provider.list_workspaces().await.unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn clone_failure_and_cancellation_restore_the_parent() {
    let _environment = support::isolated_config();
    use devflow_core::services::local_docker::{
        inspect_container_status, quiesced_operation, ContainerStatus,
    };
    let name = format!("devflow-review-recovery-{}", std::process::id());
    let _cleanup = ContainerCleanup(name.clone());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let cfg = SharedServiceConfig {
        container_name: Some(name.clone()),
        port: Some(port),
        ..Default::default()
    };
    SharedPostgresProvider::new("recovery", Some(&cfg))
        .unwrap()
        .create_workspace("main", None)
        .await
        .unwrap();
    let docker = devflow_core::services::shared::container::connect().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let lease = temp.path().join("parent.lock");
    let error: anyhow::Result<()> = quiesced_operation(
        docker.clone(),
        name.clone(),
        lease.clone(),
        vec![
            "pg_isready".into(),
            "-h".into(),
            "127.0.0.1".into(),
            "-U".into(),
            "postgres".into(),
        ],
        async { anyhow::bail!("injected copy failure") },
    )
    .await;
    assert!(error.is_err());
    assert_eq!(
        inspect_container_status(&docker, &name).await.unwrap(),
        ContainerStatus::Running
    );
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let future = quiesced_operation(
        docker.clone(),
        name.clone(),
        lease.clone(),
        vec![
            "pg_isready".into(),
            "-h".into(),
            "127.0.0.1".into(),
            "-U".into(),
            "postgres".into(),
        ],
        async move {
            entered_tx.send(()).unwrap();
            finish_rx.await.unwrap();
            Ok(())
        },
    );
    let caller = tokio::spawn(future);
    if entered_rx.await.is_err() {
        panic!("clone failed before copying: {:?}", caller.await);
    }
    assert_ne!(
        inspect_container_status(&docker, &name).await.unwrap(),
        ContainerStatus::Running
    );
    caller.abort();
    finish_tx.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if inspect_container_status(&docker, &name).await.unwrap() == ContainerStatus::Running
                && !lease.with_extension("recovery.json").exists()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("cancelled caller must not cancel parent recovery");
}

impl devflow_core::services::local_engine::LocalEngineSpec for FixtureEngine {
    fn kind(&self) -> &'static str {
        "review"
    }
    fn display_name(&self) -> &'static str {
        "Review fixture"
    }
    fn provider_name(&self) -> &'static str {
        "Review fixture"
    }
    fn image(&self) -> &str {
        "redis:7-alpine"
    }
    fn port_range_start(&self) -> u16 {
        56980
    }
    fn data_mount_path(&self) -> &'static str {
        "/data"
    }
    fn port_layout(&self) -> devflow_core::services::local_engine::PortLayout {
        devflow_core::services::local_engine::PortLayout::Single {
            container_port: 6379,
        }
    }
    fn env(&self) -> Vec<String> {
        vec![]
    }
    fn readiness_command(&self) -> Vec<String> {
        vec!["redis-cli".into(), "ping".into()]
    }
    fn restart_ready_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(10)
    }
    fn connection_info(&self, port: u16) -> devflow_core::services::ConnectionInfo {
        devflow_core::services::ConnectionInfo {
            host: "127.0.0.1".into(),
            port,
            database: "0".into(),
            user: String::new(),
            password: None,
            connection_string: None,
        }
    }
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn local_engine_projects_must_not_share_storage() {
    let _environment = support::isolated_config();
    use devflow_core::services::local_engine::LocalEngineBackend;
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let project_a = format!("reviewa{}", std::process::id());
    let project_b = format!("reviewb{}", std::process::id());
    let a = LocalEngineBackend::with_engine(
        &project_a,
        "db",
        temp.path().to_str(),
        None,
        FixtureEngine,
    )
    .unwrap();
    let b = LocalEngineBackend::with_engine(
        &project_b,
        "db",
        temp.path().to_str(),
        None,
        FixtureEngine,
    )
    .unwrap();
    let name_a = a
        .create_workspace("main", None)
        .await
        .unwrap()
        .database_name;
    let _cleanup_a = ContainerCleanup(name_a.clone());
    let name_b = b
        .create_workspace("main", None)
        .await
        .unwrap()
        .database_name;
    let _cleanup_b = ContainerCleanup(name_b.clone());
    let docker = devflow_core::services::shared::container::connect().unwrap();
    let inspect_a = docker.inspect_container(&name_a, None).await.unwrap();
    let inspect_b = docker.inspect_container(&name_b, None).await.unwrap();
    let source_a = inspect_a
        .mounts
        .unwrap()
        .into_iter()
        .find(|m| m.destination.as_deref() == Some("/data"))
        .unwrap()
        .source;
    let source_b = inspect_b
        .mounts
        .unwrap()
        .into_iter()
        .find(|m| m.destination.as_deref() == Some("/data"))
        .unwrap()
        .source;
    println!(
        "Project A mount: {:?}; project B mount: {:?}",
        source_a, source_b
    );
    assert_ne!(
        source_a, source_b,
        "different projects mount the same workspace data directory"
    );
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn versioned_baselines_seal_data_and_reset_to_the_recorded_generation() {
    let _environment = support::isolated_config();
    let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let store_path = temp.path().join("state.db");
    let _cleanup = LocalProjectCleanup(store_path.clone());
    for args in [
        vec!["init", "-b", "main"],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    ] {
        assert!(std::process::Command::new("git")
            .current_dir(temp.path())
            .args(args)
            .output()
            .unwrap()
            .status
            .success());
    }
    let service:devflow_core::config::NamedServiceConfig=serde_json::from_value(serde_json::json!({"name":"db","type":"local","service_type":"postgres","auto_workspace":true,"default":true,"local":{"data_root":temp.path(),"storage":"copy","image":"postgres:17","port_range_start":57450}})).unwrap();
    let config = Config {
        project_id: Some(format!("baseline-test-{}", uuid::Uuid::new_v4())),
        project_root: Some(temp.path().to_path_buf()),
        services: Some(vec![service]),
        ..Config::default()
    };
    let source = temp.path().join("seed.sql");
    std::fs::write(
        &source,
        "CREATE TABLE marker(value text); INSERT INTO marker VALUES ('v1');",
    )
    .unwrap();
    let first =
        devflow_core::baseline::prepare(&config, temp.path(), "clean", "db", &source, None, None)
            .await
            .unwrap();
    assert_eq!(first.version, 1);
    assert_eq!(first.status, "sealed");
    let provider = devflow_core::services::factory::resolve_provider(&config, Some("db"))
        .await
        .unwrap();
    assert!(provider
        .provider
        .start_workspace(&first.service_key)
        .await
        .is_err());
    assert!(provider
        .provider
        .seed_from_source(&first.service_key, source.to_str().unwrap())
        .await
        .is_err());
    std::fs::write(
        &source,
        "CREATE TABLE marker(value text); INSERT INTO marker VALUES ('v2');",
    )
    .unwrap();
    let second =
        devflow_core::baseline::prepare(&config, temp.path(), "clean", "db", &source, None, None)
            .await
            .unwrap();
    assert_eq!(second.version, 2);
    assert_ne!(first.source_fingerprint, second.source_fingerprint);
    let options = devflow_core::workspace::switch::SwitchOptions {
        create_if_missing: true,
        from_workspace: Some("main".into()),
        data_from_workspace: Some(first.reference.clone()),
        lifecycle: devflow_core::workspace::LifecycleOptions {
            skip_hooks: true,
            skip_processes: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let a = devflow_core::application::switch_workspace(&config, temp.path(), "agent/a", &options)
        .await
        .unwrap();
    assert!(a.ready(), "{a:?}");
    let b = devflow_core::application::switch_workspace(&config, temp.path(), "agent/b", &options)
        .await
        .unwrap();
    assert!(b.ready(), "{b:?}");
    local_sql(
        &store_path,
        &a.service_key,
        "UPDATE marker SET value='changed'",
    )
    .await;
    assert_eq!(
        local_sql(&store_path, &b.service_key, "SELECT value FROM marker").await,
        "v1"
    );
    let reset = devflow_core::application::service_operation(
        &config,
        temp.path(),
        "db",
        "agent/a",
        provider.provider.as_ref(),
        devflow_core::services::ServiceOperation::Reset,
    )
    .await
    .unwrap();
    assert_eq!(
        local_sql(&store_path, &a.service_key, "SELECT value FROM marker").await,
        "v1"
    );
    // Reset must request the sealed image ID, even when the project uses a mutable tag.
    let container = {
        let store =
            devflow_core::services::postgres::local::state::Store::open(&store_path).unwrap();
        let project = store.list_projects().unwrap().remove(0);
        store
            .get_workspace_by_name(&project.id, &a.service_key)
            .unwrap()
            .unwrap()
            .container_name
    };
    let docker = bollard::Docker::connect_with_local_defaults().unwrap();
    let inspected = docker.inspect_container(&container, None).await.unwrap();
    assert_eq!(inspected.config.unwrap().image, first.engine_fingerprint);
    assert!(
        matches!(reset.data_origin,Some(devflow_core::baseline::DataOrigin::Baseline {reference,..}) if reference==first.reference)
    );
    assert!(
        devflow_core::baseline::remove(&config, temp.path(), &first.reference)
            .await
            .is_err()
    );
    let entries = provider.provider.list_workspaces().await.unwrap();
    assert_eq!(
        entries
            .iter()
            .find(|w| w.name == first.service_key)
            .unwrap()
            .state
            .as_deref(),
        Some("stopped")
    );
    devflow_core::baseline::remove(&config, temp.path(), &second.reference)
        .await
        .unwrap();
    let third =
        devflow_core::baseline::prepare(&config, temp.path(), "clean", "db", &source, None, None)
            .await
            .unwrap();
    assert_eq!(
        third.version, 3,
        "removed version numbers must never be reused"
    );
    provider.provider.destroy_project().await.unwrap();
    assert!(provider
        .provider
        .list_workspaces()
        .await
        .unwrap()
        .is_empty());
}

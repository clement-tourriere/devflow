//! Integration tests for the shared (global-container) providers, exercised
//! against a real Docker daemon. Marked `#[ignore]` so normal `cargo test`
//! skips them; run explicitly with:
//!
//!   cargo test -p devflow-core --test shared_integration -- --ignored
//!
//! Each test uses a dedicated container name + non-default port and removes its
//! container/volume before and after, so it never touches a user's real
//! `devflow-shared-*` engines.

#![cfg(feature = "service-local")]

use devflow_core::config::SharedServiceConfig;
use devflow_core::services::shared::{
    RustFsProvider, SharedClickHouseProvider, SharedPostgresProvider, SharedRedisProvider,
};
use devflow_core::services::ServiceProvider;

/// Remove a test container and its data volume, ignoring errors.
fn docker_cleanup(container: &str) {
    let _ = std::process::Command::new("docker")
        .args(["rm", "-f", container])
        .output();
    let _ = std::process::Command::new("docker")
        .args(["volume", "rm", "-f", &format!("{container}-data")])
        .output();
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn shared_postgres_lifecycle() {
    let container = "devflow-itest-postgres";
    docker_cleanup(container);

    let cfg = SharedServiceConfig {
        container_name: Some(container.to_string()),
        port: Some(55990),
        ..Default::default()
    };
    let provider = SharedPostgresProvider::new("itest_proj", "db", Some(&cfg)).unwrap();

    // Create a workspace database.
    let info = provider.create_workspace("main", None).await.unwrap();
    assert_eq!(info.database_name, "itest_proj_main");
    assert!(provider.workspace_exists("main").await.unwrap());

    // Idempotent re-create is a no-op.
    provider.create_workspace("main", None).await.unwrap();

    // Branch-from-parent via TEMPLATE.
    let child = provider
        .create_workspace("feature/x", Some("main"))
        .await
        .unwrap();
    assert_eq!(child.database_name, "itest_proj_feature_x");
    assert!(provider.workspace_exists("feature/x").await.unwrap());

    // Both appear in the listing.
    let names: Vec<String> = provider
        .list_workspaces()
        .await
        .unwrap()
        .into_iter()
        .map(|w| w.database_name)
        .collect();
    assert!(names.contains(&"itest_proj_main".to_string()));
    assert!(names.contains(&"itest_proj_feature_x".to_string()));

    // Connection info points at the fixed port + the right db.
    let conn = provider.get_connection_info("main").await.unwrap();
    assert_eq!(conn.port, 55990);
    assert_eq!(conn.database, "itest_proj_main");

    // Delete one; it disappears.
    provider.delete_workspace("feature/x").await.unwrap();
    assert!(!provider.workspace_exists("feature/x").await.unwrap());

    // Destroy drops the rest.
    let dropped = provider.destroy_project().await.unwrap();
    assert!(dropped.contains(&"itest_proj_main".to_string()));

    docker_cleanup(container);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn shared_redis_lifecycle() {
    let container = "devflow-itest-redis";
    docker_cleanup(container);

    let cfg = SharedServiceConfig {
        container_name: Some(container.to_string()),
        port: Some(56391),
        image: Some("redis:7-alpine".to_string()),
        ..Default::default()
    };
    let provider = SharedRedisProvider::new("itest_proj", "cache", Some(&cfg)).unwrap();

    // Allocate an index for a workspace.
    let info = provider.create_workspace("main", None).await.unwrap();
    let idx: u32 = info.database_name.parse().unwrap();
    assert!((1..=15).contains(&idx));
    assert!(provider.workspace_exists("main").await.unwrap());

    // Re-allocate returns the SAME index (idempotent, atomic).
    let again = provider.create_workspace("main", None).await.unwrap();
    assert_eq!(again.database_name, info.database_name);

    // A second workspace gets a different index.
    let other = provider.create_workspace("feature/x", None).await.unwrap();
    assert_ne!(other.database_name, info.database_name);

    // Listing shows both, project-scoped.
    let mut names: Vec<String> = provider
        .list_workspaces()
        .await
        .unwrap()
        .into_iter()
        .map(|w| w.name)
        .collect();
    names.sort();
    assert_eq!(names, vec!["feature-x".to_string(), "main".to_string()]);

    // Connection string carries the index.
    let conn = provider.get_connection_info("main").await.unwrap();
    assert!(conn
        .connection_string
        .unwrap()
        .ends_with(&format!("/{}", info.database_name)));

    // Delete releases the slot.
    provider.delete_workspace("feature/x").await.unwrap();
    assert!(!provider.workspace_exists("feature/x").await.unwrap());

    docker_cleanup(container);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn shared_clickhouse_lifecycle() {
    let container = "devflow-itest-clickhouse";
    docker_cleanup(container);

    let cfg = SharedServiceConfig {
        container_name: Some(container.to_string()),
        port: Some(58123),
        image: Some("clickhouse/clickhouse-server:25.8".to_string()),
        ..Default::default()
    };
    let provider = SharedClickHouseProvider::new("itest_proj", "olap", Some(&cfg)).unwrap();

    let info = provider.create_workspace("main", None).await.unwrap();
    assert_eq!(info.database_name, "itest_proj_main");
    assert!(provider.workspace_exists("main").await.unwrap());

    // Idempotent.
    provider.create_workspace("main", None).await.unwrap();

    let names: Vec<String> = provider
        .list_workspaces()
        .await
        .unwrap()
        .into_iter()
        .map(|w| w.database_name)
        .collect();
    assert!(names.contains(&"itest_proj_main".to_string()));

    provider.delete_workspace("main").await.unwrap();
    assert!(!provider.workspace_exists("main").await.unwrap());

    docker_cleanup(container);
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn shared_rustfs_lifecycle() {
    let container = "devflow-itest-rustfs";
    docker_cleanup(container);

    let cfg = SharedServiceConfig {
        container_name: Some(container.to_string()),
        port: Some(59000),
        ..Default::default()
    };
    let provider = RustFsProvider::new("itest-proj", "storage", Some(&cfg)).unwrap();

    // Create a bucket for a workspace.
    let info = provider.create_workspace("main", None).await.unwrap();
    assert_eq!(info.database_name, "itest-proj-main");
    assert!(provider.workspace_exists("main").await.unwrap());

    // Idempotent.
    provider.create_workspace("main", None).await.unwrap();

    // A second workspace -> a second bucket.
    provider.create_workspace("feature/x", None).await.unwrap();
    let mut names: Vec<String> = provider
        .list_workspaces()
        .await
        .unwrap()
        .into_iter()
        .map(|w| w.database_name)
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "itest-proj-feature-x".to_string(),
            "itest-proj-main".to_string()
        ]
    );

    // Connection info exposes the endpoint + bucket.
    let conn = provider.get_connection_info("main").await.unwrap();
    assert_eq!(conn.port, 59000);
    assert!(conn
        .connection_string
        .unwrap()
        .ends_with("/itest-proj-main"));

    // Delete removes the bucket.
    provider.delete_workspace("feature/x").await.unwrap();
    assert!(!provider.workspace_exists("feature/x").await.unwrap());

    docker_cleanup(container);
}

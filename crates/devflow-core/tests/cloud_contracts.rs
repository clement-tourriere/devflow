//! Recorded API shapes served over loopback; no live cloud account is required.
#![cfg(any(feature = "service-neon", feature = "service-xata"))]
use devflow_core::services::ServiceProvider;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn server(
    exchanges: Vec<(&'static str, &'static str, Option<Value>, Value)>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        for (method, path, body, response) in exchanges {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            let end = loop {
                let mut buffer = [0; 4096];
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                data.extend_from_slice(&buffer[..count]);
                if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8(data[..end].to_vec()).unwrap();
            assert_eq!(
                headers.lines().next().unwrap(),
                format!("{method} {path} HTTP/1.1")
            );
            let size: usize = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|n| n.trim().parse().unwrap())
                })
                .unwrap_or(0);
            while data.len() < end + size {
                let mut buffer = [0; 4096];
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                data.extend_from_slice(&buffer[..count]);
            }
            if let Some(expected) = body {
                assert_eq!(
                    serde_json::from_slice::<Value>(&data[end..end + size]).unwrap(),
                    expected
                );
            }
            let body = response.to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    (url, task)
}

#[cfg(feature = "service-neon")]
#[tokio::test]
async fn neon_uses_branches_resolves_parent_and_fetches_real_connection_uri() {
    use devflow_core::services::postgres::neon::NeonProvider;
    let branch = json!({"id":"br-main","name":"main","created_at":"2026-01-01T00:00:00Z"});
    let child = json!({"id":"br-child","name":"task","parent_id":"br-main","created_at":"2026-01-02T00:00:00Z"});
    let (url, task) = server(vec![
        ("GET", "/projects/p/branches", None, json!({"branches":[branch.clone()]})),
        ("POST", "/projects/p/branches", Some(json!({"branch":{"name":"task","parent_id":"br-main"},"endpoints":[{"type":"read_write"}]})), json!({"branch":child.clone()})),
        ("GET", "/projects/p/branches", None, json!({"branches":[branch,child]})),
        ("GET", "/projects/p/branches/br-child/databases", None, json!({"databases":[{"name":"app","owner_name":"owner"}]})),
        ("GET", "/projects/p/connection_uri?branch_id=br-child&database_name=app&role_name=owner", None, json!({"uri":"postgresql://owner:p%40ss@ep-real.neon.tech/app?sslmode=require"})),
    ]).await;
    let provider = NeonProvider::new("test".into(), "p".into(), Some(url)).unwrap();
    assert_eq!(
        provider
            .create_workspace("task", Some("main"))
            .await
            .unwrap()
            .parent_workspace
            .as_deref(),
        Some("main")
    );
    let connection = provider.get_connection_info("task").await.unwrap();
    assert_eq!(connection.host, "ep-real.neon.tech");
    assert_eq!(connection.password.as_deref(), Some("p@ss"));
    assert!(connection
        .connection_string
        .unwrap()
        .ends_with("?sslmode=require"));
    task.await.unwrap();
}

#[cfg(feature = "service-xata")]
#[tokio::test]
async fn xata_uses_inherit_mode_and_credentials_without_localhost_fallback() {
    use devflow_core::services::postgres::xata::XataProvider;
    let main = json!({"id":"b-main","name":"main","createdAt":"2026-01-01T00:00:00Z"});
    let child = json!({"id":"b-task","name":"_main","parentID":"b-main"});
    let (url, task) = server(vec![
        ("GET", "/organizations/o/projects/p/branches", None, json!({"branches":[main]})),
        ("POST", "/organizations/o/projects/p/branches", Some(json!({"mode":"inherit","name":"_main","parentID":"b-main"})), child.clone()),
        ("GET", "/organizations/o/projects/p/branches", None, json!({"branches":[child]})),
        ("GET", "/organizations/o/projects/p/branches/b-task/credentials", None, json!({"username":"app","password":"secret","hostname":"real.xata.tech","port":5432,"dbname":"appdb","connectionString":"postgres://app:secret@real.xata.tech/appdb?sslmode=require"})),
    ]).await;
    let provider = XataProvider::new("test".into(), "o".into(), "p".into(), Some(url)).unwrap();
    assert_eq!(
        provider
            .create_workspace("_main", Some("main"))
            .await
            .unwrap()
            .name,
        "_main"
    );
    let connection = provider.get_connection_info("_main").await.unwrap();
    assert_eq!(connection.host, "real.xata.tech");
    assert_eq!(connection.database, "appdb");
    task.await.unwrap();
}

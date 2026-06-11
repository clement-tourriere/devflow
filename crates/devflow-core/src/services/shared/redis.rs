//! Shared Redis provider — one global container, a logical **DB index** per
//! workspace (Redis' built-in numbered databases). Allocations live inside
//! Redis itself (a reserved hash in DB 0), so the engine remains the single
//! source of truth and allocation is atomic via a Lua script.
//!
//! Redis exposes only 16 databases (0–15) and that space is GLOBAL to the
//! instance, so this caps total workspaces across all projects at 15 (DB 0 is
//! reserved for the allocation metadata). For more, a key-prefix mode would be
//! needed — a documented future option.

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bollard::Docker;

use super::container::{self, GlobalContainerSpec};
use super::naming;
use crate::config::SharedServiceConfig;
use crate::services::{
    ConnectionInfo, DoctorCheck, DoctorReport, ProjectInfo, ServiceProvider, WorkspaceInfo,
};

const DEFAULT_IMAGE: &str = "redis:7";
const DEFAULT_PORT: u16 = 6379;
const DEFAULT_CONTAINER: &str = "devflow-shared-redis";
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// DB 0 holds the allocation hash; workspaces get 1..=15.
const ALLOC_HASH: &str = "devflow:dbindex";
const MAX_DB_INDEX: u32 = 15;

/// Atomic allocate-or-get: returns the workspace's existing index, or assigns
/// the lowest free index in 1..=15, or -1 when exhausted. Runs as a single
/// Redis EVAL so concurrent devflow processes can't double-allocate.
const ALLOC_LUA: &str = r#"
local hashkey = KEYS[1]
local field = ARGV[1]
local maxidx = tonumber(ARGV[2])
local existing = redis.call('HGET', hashkey, field)
if existing then return tonumber(existing) end
local used = {}
local all = redis.call('HGETALL', hashkey)
for i = 2, #all, 2 do used[tonumber(all[i])] = true end
for idx = 1, maxidx do
  if not used[idx] then
    redis.call('HSET', hashkey, field, idx)
    return idx
  end
end
return -1
"#;

/// A Redis provider that keeps one global container and gives each workspace
/// its own numbered database.
pub struct SharedRedisProvider {
    project_name: String,
    #[allow(dead_code)]
    service_name: String,
    image: String,
    container_name: String,
    port: u16,
    password: String,
}

impl SharedRedisProvider {
    pub fn new(
        project_name: &str,
        service_name: &str,
        config: Option<&SharedServiceConfig>,
    ) -> Result<Self> {
        let c = config.cloned().unwrap_or_default();
        Ok(Self {
            project_name: project_name.to_string(),
            service_name: service_name.to_string(),
            image: c.image.unwrap_or_else(|| DEFAULT_IMAGE.to_string()),
            container_name: c
                .container_name
                .unwrap_or_else(|| DEFAULT_CONTAINER.to_string()),
            port: c.port.unwrap_or(DEFAULT_PORT),
            password: c.password.unwrap_or_default(),
        })
    }

    fn alloc_field(&self, workspace: &str) -> String {
        naming::redis_alloc_field(&self.project_name, workspace)
    }

    fn container_spec(&self) -> GlobalContainerSpec {
        // Persist with AOF; set a password via the server arg when configured.
        let mut cmd = vec![
            "redis-server".to_string(),
            "--appendonly".to_string(),
            "yes".to_string(),
        ];
        if !self.password.is_empty() {
            cmd.push("--requirepass".to_string());
            cmd.push(self.password.clone());
        }
        GlobalContainerSpec {
            container_name: self.container_name.clone(),
            image: self.image.clone(),
            host_port: self.port,
            container_port: DEFAULT_PORT,
            extra_port: None,
            cmd,
            env: vec![],
            binds: vec![format!("{}-data:/data", self.container_name)],
            labels: vec![
                ("devflow.service-type".to_string(), "redis".to_string()),
                ("devflow.shared".to_string(), "true".to_string()),
            ],
        }
    }

    /// Base `redis-cli` argv for DB `db`, including auth when set.
    fn redis_cli(&self, db: u32) -> Vec<String> {
        let mut v = vec!["redis-cli".to_string(), "-n".to_string(), db.to_string()];
        if !self.password.is_empty() {
            v.push("-a".to_string());
            v.push(self.password.clone());
            v.push("--no-auth-warning".to_string());
        }
        v
    }

    async fn run(&self, docker: &Docker, db: u32, args: &[&str]) -> Result<String> {
        let mut cmd = self.redis_cli(db);
        cmd.extend(args.iter().map(|s| s.to_string()));
        let refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
        let out = container::exec_capture(docker, &self.container_name, &refs, None).await?;
        if !out.ok() {
            anyhow::bail!(
                "redis-cli failed (exit {}): {}",
                out.exit_code,
                out.stderr.trim()
            );
        }
        Ok(out.stdout.trim().to_string())
    }

    async fn ensure_ready(&self, docker: &Docker) -> Result<()> {
        container::ensure_running_container(docker, &self.container_spec()).await?;
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        loop {
            if let Ok(out) = self.run(docker, 0, &["PING"]).await {
                if out.eq_ignore_ascii_case("PONG") {
                    return Ok(());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for Redis readiness");
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    /// Allocate (or fetch) this workspace's DB index via the atomic Lua script.
    async fn allocate_index(&self, docker: &Docker, workspace: &str) -> Result<u32> {
        let field = self.alloc_field(workspace);
        let max = MAX_DB_INDEX.to_string();
        let out = self
            .run(
                docker,
                0,
                &["EVAL", ALLOC_LUA, "1", ALLOC_HASH, &field, &max],
            )
            .await?;
        let idx: i64 = out.trim().parse().unwrap_or(-1);
        if idx < 1 {
            anyhow::bail!(
                "Shared Redis is out of database slots (max {} workspaces across all projects). \
                 Delete unused workspaces or use a dedicated Redis service.",
                MAX_DB_INDEX
            );
        }
        Ok(idx as u32)
    }

    async fn lookup_index(&self, docker: &Docker, workspace: &str) -> Result<Option<u32>> {
        let field = self.alloc_field(workspace);
        let out = self.run(docker, 0, &["HGET", ALLOC_HASH, &field]).await?;
        Ok(out.trim().parse::<u32>().ok())
    }

    fn connection_info_for(&self, index: u32) -> ConnectionInfo {
        let auth = if self.password.is_empty() {
            String::new()
        } else {
            format!(":{}@", self.password)
        };
        ConnectionInfo {
            host: "127.0.0.1".to_string(),
            port: self.port,
            database: index.to_string(),
            user: String::new(),
            password: if self.password.is_empty() {
                None
            } else {
                Some(self.password.clone())
            },
            connection_string: Some(format!("redis://{}127.0.0.1:{}/{}", auth, self.port, index)),
        }
    }
}

#[async_trait]
impl ServiceProvider for SharedRedisProvider {
    async fn create_workspace(
        &self,
        workspace_name: &str,
        _from_workspace: Option<&str>,
    ) -> Result<WorkspaceInfo> {
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;
        let index = self.allocate_index(&docker, workspace_name).await?;
        Ok(WorkspaceInfo {
            name: workspace_name.to_string(),
            created_at: None,
            parent_workspace: None,
            database_name: index.to_string(),
            state: Some("running".to_string()),
        })
    }

    async fn delete_workspace(&self, workspace_name: &str) -> Result<()> {
        let docker = container::connect()?;
        if container::ensure_running_container(&docker, &self.container_spec())
            .await
            .is_err()
        {
            return Ok(());
        }
        if let Some(index) = self.lookup_index(&docker, workspace_name).await? {
            // Clear the workspace's database, then release the allocation.
            let _ = self.run(&docker, index, &["FLUSHDB"]).await;
            let field = self.alloc_field(workspace_name);
            self.run(&docker, 0, &["HDEL", ALLOC_HASH, &field])
                .await
                .with_context(|| format!("failed to release Redis index for '{workspace_name}'"))?;
        }
        Ok(())
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;
        let prefix = naming::redis_project_prefix(&self.project_name);
        // HGETALL returns field/value lines alternating.
        let out = self.run(&docker, 0, &["HGETALL", ALLOC_HASH]).await?;
        let tokens: Vec<&str> = out.lines().map(str::trim).collect();
        let mut workspaces = Vec::new();
        let mut i = 0;
        while i + 1 < tokens.len() {
            let field = tokens[i];
            let index = tokens[i + 1];
            i += 2;
            if let Some(ws) = field.strip_prefix(&prefix) {
                workspaces.push(WorkspaceInfo {
                    name: ws.to_string(),
                    created_at: None,
                    parent_workspace: None,
                    database_name: index.to_string(),
                    state: Some("running".to_string()),
                });
            }
        }
        Ok(workspaces)
    }

    async fn workspace_exists(&self, workspace_name: &str) -> Result<bool> {
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;
        Ok(self.lookup_index(&docker, workspace_name).await?.is_some())
    }

    async fn switch_to_branch(&self, workspace_name: &str) -> Result<WorkspaceInfo> {
        self.create_workspace(workspace_name, None).await
    }

    async fn get_connection_info(&self, workspace_name: &str) -> Result<ConnectionInfo> {
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;
        let index = self.allocate_index(&docker, workspace_name).await?;
        Ok(self.connection_info_for(index))
    }

    fn supports_destroy(&self) -> bool {
        true
    }

    async fn destroy_preview(&self) -> Result<Option<(String, Vec<String>)>> {
        let workspaces = self.list_workspaces().await.unwrap_or_default();
        Ok(Some((
            self.container_name.clone(),
            workspaces
                .into_iter()
                .map(|w| format!("db {}", w.database_name))
                .collect(),
        )))
    }

    async fn destroy_project(&self) -> Result<Vec<String>> {
        let docker = container::connect()?;
        self.ensure_ready(&docker).await?;
        let mut removed = Vec::new();
        for w in self.list_workspaces().await? {
            if let Ok(index) = w.database_name.parse::<u32>() {
                let _ = self.run(&docker, index, &["FLUSHDB"]).await;
                let field = self.alloc_field(&w.name);
                let _ = self.run(&docker, 0, &["HDEL", ALLOC_HASH, &field]).await;
                removed.push(format!("db {index}"));
            }
        }
        Ok(removed)
    }

    async fn doctor(&self) -> Result<DoctorReport> {
        let mut checks = Vec::new();
        match container::connect() {
            Ok(docker) => {
                checks.push(DoctorCheck {
                    name: "docker".to_string(),
                    available: true,
                    detail: "Connected to Docker daemon".to_string(),
                });
                let running = matches!(
                    crate::services::local_docker::inspect_container_status(
                        &docker,
                        &self.container_name
                    )
                    .await,
                    Ok(crate::services::local_docker::ContainerStatus::Running)
                );
                checks.push(DoctorCheck {
                    name: "redis-container".to_string(),
                    available: running,
                    detail: if running {
                        format!("'{}' running on port {}", self.container_name, self.port)
                    } else {
                        format!(
                            "'{}' not running (started on first use)",
                            self.container_name
                        )
                    },
                });
            }
            Err(e) => checks.push(DoctorCheck {
                name: "docker".to_string(),
                available: false,
                detail: format!("Docker unavailable: {e}"),
            }),
        }
        Ok(DoctorReport { checks })
    }

    async fn test_connection(&self) -> Result<()> {
        let docker = container::connect()?;
        self.ensure_ready(&docker).await
    }

    fn provider_name(&self) -> &'static str {
        "shared-redis"
    }

    fn project_info(&self) -> Option<ProjectInfo> {
        Some(ProjectInfo {
            name: self.project_name.clone(),
            storage_driver: Some("shared".to_string()),
            image: Some(self.image.clone()),
        })
    }
}

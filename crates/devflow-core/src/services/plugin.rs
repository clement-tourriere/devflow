//! Plugin service provider.
//!
//! Delegates all `ServiceProvider` operations to an external executable that
//! communicates over JSON on stdin/stdout (one process spawn per method call).
//!
//! ## Protocol
//!
//! **Request** (written to plugin stdin):
//! ```json
//! {
//!   "method": "create_workspace",
//!   "params": { "workspace_name": "feature-xyz", "from_workspace": "main" },
//!   "config": { ... },
//!   "service_name": "my-redis"
//! }
//! ```
//!
//! **Success response** (read from plugin stdout):
//! ```json
//! { "ok": true, "result": { ... } }
//! ```
//!
//! **Error response**:
//! ```json
//! { "ok": false, "error": "something went wrong" }
//! ```
//!
//! Stderr output is captured and logged at `warn` level.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::PluginConfig;
use crate::services::{
    ConnectionInfo, DoctorCheck, DoctorReport, ProjectInfo, ServiceProvider, WorkspaceInfo,
};

/// A service provider that delegates to an external plugin executable.
pub struct PluginProvider {
    /// Logical service name from the config (e.g. "my-redis").
    service_name: String,
    project_root: PathBuf,
    project_id: crate::state::ProjectId,
    operations: super::OperationCapabilities,
    supports_destroy: bool,
    invocation_group: String,
    /// Resolved path to the plugin executable.
    executable: PathBuf,
    /// Per-invocation timeout.
    timeout: Duration,
    /// Opaque config blob forwarded to the plugin on every call.
    plugin_config: Option<serde_json::Value>,
}

impl PluginProvider {
    /// Create a new `PluginProvider` from the service name and plugin config.
    ///
    /// Resolves the executable path from either `path` (direct) or `name`
    /// (looked up as `devflow-plugin-{name}` on `$PATH`).
    pub async fn connect(
        service_name: &str,
        config: &PluginConfig,
        root: &std::path::Path,
    ) -> Result<Self> {
        let executable = Self::resolve_executable(config, root)?;
        let timeout = Duration::from_secs(config.timeout);

        let mut provider = Self {
            service_name: service_name.to_string(),
            project_root: root.to_path_buf(),
            project_id: crate::state::LocalStateManager::new()?.control(root).id,
            operations: Default::default(),
            supports_destroy: false,
            invocation_group: uuid::Uuid::new_v4().to_string(),
            executable,
            timeout,
            plugin_config: config.config.clone(),
        };
        let negotiated = provider
            .invoke("negotiate", json!({"supported_versions":[1]}))
            .await
            .context(
                "plugin must implement protocol v1 negotiation; regenerate or update its adapter",
            )?;
        anyhow::ensure!(
            negotiated.get("protocol_version").and_then(|v| v.as_u64()) == Some(1),
            "plugin protocol version is unsupported"
        );
        provider.operations = serde_json::from_value(
            negotiated
                .get("operations")
                .cloned()
                .unwrap_or_else(|| json!({})),
        )?;
        // Baseline sealing is currently a local PostgreSQL contract.
        provider.operations.snapshot = false;
        provider.supports_destroy = negotiated
            .get("destroy")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        Ok(provider)
    }

    fn require(&self, operation: super::ServiceOperation) -> Result<()> {
        if !self.operations.supports(operation) {
            return Err(super::UnsupportedOperation::new("plugin", operation).into());
        }
        Ok(())
    }

    /// Resolve the plugin executable from config.
    fn resolve_executable(config: &PluginConfig, root: &std::path::Path) -> Result<PathBuf> {
        if let Some(ref path) = config.path {
            let p = PathBuf::from(path);
            // Relative paths belong to the project, independent of the frontend cwd.
            let resolved = if p.is_absolute() { p } else { root.join(p) };
            anyhow::ensure!(
                resolved.exists(),
                "Plugin executable not found at: {}",
                resolved.display()
            );
            Ok(resolved)
        } else if let Some(ref name) = config.name {
            let bin_name = format!("devflow-plugin-{}", name);
            which::which(&bin_name).with_context(|| {
                format!(
                    "Plugin '{}' not found on PATH (looked for '{}')",
                    name, bin_name
                )
            })
        } else {
            anyhow::bail!(
                "Plugin config must specify either 'path' or 'name' to locate the executable"
            )
        }
    }

    /// Invoke the plugin with the given method and params, returning the parsed
    /// JSON result value.
    async fn invoke(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let workspace = params.get("workspace_name").and_then(|v| v.as_str());
        let state = crate::state::LocalStateManager::new()?;
        let registered = state.get_workspaces_by_dir(&self.project_root);
        let raw = workspace
            .and_then(|key| {
                registered
                    .iter()
                    .find(|w| w.service_key == key || w.name == key)
            })
            .map(|w| w.name.as_str())
            .or(workspace);
        let operation = crate::workspace::operation::OperationJournal::pending(&self.project_root)?
            .into_iter()
            .find(|op| {
                op.status == "running"
                    && (Some(op.workspace.as_str()) == raw
                        || (raw.is_none() && op.kind.ends_with(&format!(":{}", self.service_name))))
            });
        let operation_id = operation
            .as_ref()
            .map(|op| op.id.as_str())
            .unwrap_or(&self.invocation_group);
        let resource_id = crate::services::ownership::identity_digest(&[
            &self.project_id.0,
            &self.service_name,
            workspace.unwrap_or("engine"),
        ]);
        let request = json!({
            "protocol_version": 1,
            "project_id": self.project_id,
            "resource_id": resource_id,
            "operation_id": operation_id,
            "idempotency_key": crate::services::ownership::identity_digest(&[operation_id,method,&resource_id]),
            "deadline": chrono::Utc::now() + chrono::Duration::seconds(self.timeout.as_secs() as i64),
            "method": method,
            "params": params,
            "config": self.plugin_config,
            "service_name": self.service_name,
        });

        let request_bytes =
            serde_json::to_vec(&request).context("Failed to serialize plugin request")?;

        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut child = Command::new(&self.executable)
            .current_dir(&self.project_root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "Failed to spawn plugin executable: {}",
                    self.executable.display()
                )
            })?;

        // Write request to stdin
        if let Some(mut stdin) = child.stdin.take() {
            tokio::time::timeout_at(deadline, stdin.write_all(&request_bytes))
                .await
                .context("plugin did not read its request before the deadline")?
                .context("Failed to write to plugin stdin")?;
            // Drop stdin to signal EOF
        }

        // Wait for the process with a timeout
        let output = tokio::time::timeout_at(deadline, child.wait_with_output())
            .await
            .map_err(|_| {
                PluginError {
                    code: "timeout_unknown_completion".into(),
                    message: format!("Plugin '{}' timed out after {}s on '{}'; inspect backend state before retrying", self.service_name, self.timeout.as_secs(), method),
                    retryable: false,
                    operation_id: operation_id.into(),
                }
            })?
            .with_context(|| {
                format!(
                    "Plugin '{}' failed to execute method '{}'",
                    self.service_name, method
                )
            })?;

        // Log stderr if non-empty
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            log::warn!(
                "Plugin '{}' stderr (method '{}'): {}",
                self.service_name,
                method,
                stderr.trim()
            );
        }

        // Check exit code
        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            anyhow::bail!(
                "Plugin '{}' exited with code {} on method '{}'. stderr: {}",
                self.service_name,
                code,
                method,
                stderr.trim()
            );
        }

        // Parse stdout as JSON response
        let response: serde_json::Value =
            serde_json::from_slice(&output.stdout).with_context(|| {
                format!(
                    "Plugin '{}' returned invalid JSON on method '{}'. stdout: {}",
                    self.service_name,
                    method,
                    String::from_utf8_lossy(&output.stdout)
                )
            })?;

        // Check the ok/error protocol
        let ok = response
            .get("ok")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if ok {
            Ok(response
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null))
        } else {
            if let Some(error) = response.get("error").filter(|e| e.is_object()) {
                return Err(PluginError {
                    code: error
                        .get("code")
                        .and_then(|v| v.as_str())
                        .unwrap_or("plugin_error")
                        .into(),
                    message: error
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("plugin operation failed")
                        .into(),
                    retryable: error
                        .get("retryable")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    operation_id: operation_id.into(),
                }
                .into());
            }
            let error_msg = response
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown plugin error");
            anyhow::bail!(
                "Plugin '{}' returned error on method '{}': {}",
                self.service_name,
                method,
                error_msg
            )
        }
    }
}

#[async_trait]
impl ServiceProvider for PluginProvider {
    async fn create_workspace(
        &self,
        workspace_name: &str,
        from_workspace: Option<&str>,
    ) -> Result<WorkspaceInfo> {
        let result = self
            .invoke(
                "create_workspace",
                json!({
                    "workspace_name": workspace_name,
                    "from_workspace": from_workspace,
                }),
            )
            .await?;
        serde_json::from_value(result).context("Failed to parse WorkspaceInfo from plugin response")
    }

    async fn delete_workspace(&self, workspace_name: &str) -> Result<()> {
        self.invoke(
            "delete_workspace",
            json!({ "workspace_name": workspace_name }),
        )
        .await?;
        Ok(())
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        let result = self.invoke("list_workspaces", json!({})).await?;
        serde_json::from_value(result)
            .context("Failed to parse Vec<WorkspaceInfo> from plugin response")
    }

    async fn workspace_exists(&self, workspace_name: &str) -> Result<bool> {
        let result = self
            .invoke(
                "workspace_exists",
                json!({ "workspace_name": workspace_name }),
            )
            .await?;
        result
            .as_bool()
            .ok_or_else(|| anyhow::anyhow!("Plugin did not return a boolean for workspace_exists"))
    }

    async fn switch_to_branch(&self, workspace_name: &str) -> Result<WorkspaceInfo> {
        let result = self
            .invoke(
                "switch_to_branch",
                json!({ "workspace_name": workspace_name }),
            )
            .await?;
        serde_json::from_value(result).context("Failed to parse WorkspaceInfo from plugin response")
    }

    async fn get_connection_info(&self, workspace_name: &str) -> Result<ConnectionInfo> {
        let result = self
            .invoke(
                "get_connection_info",
                json!({ "workspace_name": workspace_name }),
            )
            .await?;
        serde_json::from_value(result)
            .context("Failed to parse ConnectionInfo from plugin response")
    }

    async fn start_workspace(&self, workspace_name: &str) -> Result<()> {
        self.require(super::ServiceOperation::Start)?;
        self.invoke(
            "start_workspace",
            json!({ "workspace_name": workspace_name }),
        )
        .await?;
        Ok(())
    }

    async fn stop_workspace(&self, workspace_name: &str) -> Result<()> {
        self.require(super::ServiceOperation::Stop)?;
        self.invoke(
            "stop_workspace",
            json!({ "workspace_name": workspace_name }),
        )
        .await?;
        Ok(())
    }

    async fn reset_workspace(&self, workspace_name: &str) -> Result<()> {
        self.require(super::ServiceOperation::Reset)?;
        self.invoke(
            "reset_workspace",
            json!({ "workspace_name": workspace_name }),
        )
        .await?;
        Ok(())
    }

    async fn seed_from_source(&self, workspace_name: &str, source: &str) -> Result<()> {
        self.require(super::ServiceOperation::Seed)?;
        self.invoke(
            "seed_from_source",
            json!({"workspace_name": workspace_name, "source": source}),
        )
        .await?;
        Ok(())
    }

    async fn logs(&self, workspace_name: &str, tail: Option<usize>) -> Result<String> {
        self.require(super::ServiceOperation::Logs)?;
        serde_json::from_value(
            self.invoke(
                "logs",
                json!({"workspace_name": workspace_name, "tail": tail}),
            )
            .await?,
        )
        .context("plugin logs result must be a string")
    }

    fn operations(&self) -> super::OperationCapabilities {
        self.operations
    }
    fn supports_destroy(&self) -> bool {
        self.supports_destroy
    }

    async fn destroy_project(&self) -> Result<Vec<String>> {
        anyhow::ensure!(
            self.supports_destroy,
            "plugin does not support project destruction"
        );
        let result = self.invoke("destroy_project", json!({})).await?;
        serde_json::from_value(result).context("Failed to parse Vec<String> from plugin response")
    }

    async fn doctor(&self) -> Result<DoctorReport> {
        let result = self.invoke("doctor", json!({})).await;
        match result {
            Ok(val) => serde_json::from_value(val)
                .context("Failed to parse DoctorReport from plugin response"),
            Err(e) => {
                // If the plugin fails to respond, return a report indicating the failure
                Ok(DoctorReport {
                    checks: vec![DoctorCheck {
                        name: format!("Plugin '{}'", self.service_name),
                        available: false,
                        detail: format!("Plugin health check failed: {}", e),
                    }],
                })
            }
        }
    }

    async fn test_connection(&self) -> Result<()> {
        self.invoke("test_connection", json!({})).await?;
        Ok(())
    }

    fn project_info(&self) -> Option<ProjectInfo> {
        // Sync method — can't invoke async plugin. Return basic info.
        Some(ProjectInfo {
            name: self.service_name.clone(),
            storage_driver: Some("plugin".to_string()),
            image: Some(self.executable.display().to_string()),
        })
    }

    fn provider_name(&self) -> &'static str {
        "plugin"
    }
}

#[derive(Debug, serde::Serialize)]
pub struct PluginError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub operation_id: String,
}
impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} (operation {}, retryable: {})",
            self.code, self.message, self.operation_id, self.retryable
        )
    }
}
impl std::error::Error for PluginError {}

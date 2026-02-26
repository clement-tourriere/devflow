//! Plugin service backend.
//!
//! Delegates all `ServiceBackend` operations to an external executable that
//! communicates over JSON on stdin/stdout (one process spawn per method call).
//!
//! ## Protocol
//!
//! **Request** (written to plugin stdin):
//! ```json
//! {
//!   "method": "create_branch",
//!   "params": { "branch_name": "feature-xyz", "from_branch": "main" },
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
    BranchInfo, ConnectionInfo, DoctorCheck, DoctorReport, ProjectInfo, ServiceBackend,
};

/// A service backend that delegates to an external plugin executable.
pub struct PluginBackend {
    /// Logical service name from the config (e.g. "my-redis").
    service_name: String,
    /// Resolved path to the plugin executable.
    executable: PathBuf,
    /// Per-invocation timeout.
    timeout: Duration,
    /// Opaque config blob forwarded to the plugin on every call.
    plugin_config: Option<serde_json::Value>,
}

impl PluginBackend {
    /// Create a new `PluginBackend` from the service name and plugin config.
    ///
    /// Resolves the executable path from either `path` (direct) or `name`
    /// (looked up as `devflow-plugin-{name}` on `$PATH`).
    pub fn new(service_name: &str, config: &PluginConfig) -> Result<Self> {
        let executable = Self::resolve_executable(config)?;
        let timeout = Duration::from_secs(config.timeout);

        Ok(Self {
            service_name: service_name.to_string(),
            executable,
            timeout,
            plugin_config: config.config.clone(),
        })
    }

    /// Resolve the plugin executable from config.
    fn resolve_executable(config: &PluginConfig) -> Result<PathBuf> {
        if let Some(ref path) = config.path {
            let p = PathBuf::from(path);
            // If relative, resolve against cwd
            let resolved = if p.is_absolute() {
                p
            } else {
                std::env::current_dir()
                    .context("Failed to get current directory")?
                    .join(p)
            };
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
        let request = json!({
            "method": method,
            "params": params,
            "config": self.plugin_config,
            "service_name": self.service_name,
        });

        let request_bytes =
            serde_json::to_vec(&request).context("Failed to serialize plugin request")?;

        let mut child = Command::new(&self.executable)
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
            stdin
                .write_all(&request_bytes)
                .await
                .context("Failed to write to plugin stdin")?;
            // Drop stdin to signal EOF
        }

        // Wait for the process with a timeout
        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Plugin '{}' timed out after {}s on method '{}'",
                    self.service_name,
                    self.timeout.as_secs(),
                    method
                )
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
impl ServiceBackend for PluginBackend {
    async fn create_branch(
        &self,
        branch_name: &str,
        from_branch: Option<&str>,
    ) -> Result<BranchInfo> {
        let result = self
            .invoke(
                "create_branch",
                json!({
                    "branch_name": branch_name,
                    "from_branch": from_branch,
                }),
            )
            .await?;
        serde_json::from_value(result).context("Failed to parse BranchInfo from plugin response")
    }

    async fn delete_branch(&self, branch_name: &str) -> Result<()> {
        self.invoke("delete_branch", json!({ "branch_name": branch_name }))
            .await?;
        Ok(())
    }

    async fn list_branches(&self) -> Result<Vec<BranchInfo>> {
        let result = self.invoke("list_branches", json!({})).await?;
        serde_json::from_value(result)
            .context("Failed to parse Vec<BranchInfo> from plugin response")
    }

    async fn branch_exists(&self, branch_name: &str) -> Result<bool> {
        let result = self
            .invoke("branch_exists", json!({ "branch_name": branch_name }))
            .await?;
        result
            .as_bool()
            .ok_or_else(|| anyhow::anyhow!("Plugin did not return a boolean for branch_exists"))
    }

    async fn switch_to_branch(&self, branch_name: &str) -> Result<BranchInfo> {
        let result = self
            .invoke("switch_to_branch", json!({ "branch_name": branch_name }))
            .await?;
        serde_json::from_value(result).context("Failed to parse BranchInfo from plugin response")
    }

    async fn get_connection_info(&self, branch_name: &str) -> Result<ConnectionInfo> {
        let result = self
            .invoke("get_connection_info", json!({ "branch_name": branch_name }))
            .await?;
        serde_json::from_value(result)
            .context("Failed to parse ConnectionInfo from plugin response")
    }

    async fn start_branch(&self, branch_name: &str) -> Result<()> {
        self.invoke("start_branch", json!({ "branch_name": branch_name }))
            .await?;
        Ok(())
    }

    async fn stop_branch(&self, branch_name: &str) -> Result<()> {
        self.invoke("stop_branch", json!({ "branch_name": branch_name }))
            .await?;
        Ok(())
    }

    async fn reset_branch(&self, branch_name: &str) -> Result<()> {
        self.invoke("reset_branch", json!({ "branch_name": branch_name }))
            .await?;
        Ok(())
    }

    fn supports_lifecycle(&self) -> bool {
        // We can't call an async method from a sync fn, so we default to true
        // and let the plugin return errors for unsupported operations.
        true
    }

    async fn cleanup_old_branches(&self, max_count: usize) -> Result<Vec<String>> {
        let result = self
            .invoke("cleanup_old_branches", json!({ "max_count": max_count }))
            .await?;
        serde_json::from_value(result).context("Failed to parse Vec<String> from plugin response")
    }

    fn supports_destroy(&self) -> bool {
        true
    }

    async fn destroy_project(&self) -> Result<Vec<String>> {
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

    async fn init_project(&self, project_name: &str) -> Result<()> {
        self.invoke("init_project", json!({ "project_name": project_name }))
            .await?;
        Ok(())
    }

    fn project_info(&self) -> Option<ProjectInfo> {
        // Sync method — can't invoke async plugin. Return basic info.
        Some(ProjectInfo {
            name: self.service_name.clone(),
            storage_backend: Some("plugin".to_string()),
            image: Some(self.executable.display().to_string()),
        })
    }

    fn backend_name(&self) -> &'static str {
        "plugin"
    }
}

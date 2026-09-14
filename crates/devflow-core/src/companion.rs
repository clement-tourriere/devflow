//! Optional proxy process adapter. All frontends observe the same loopback API.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyStatus {
    pub available: bool,
    pub running: bool,
    pub https_port: u16,
    pub http_port: u16,
    pub ca_installed: bool,
    pub ca_path: String,
}
impl Default for ProxyStatus {
    fn default() -> Self {
        Self {
            available: false,
            running: false,
            https_port: 443,
            http_port: 80,
            ca_installed: false,
            ca_path: String::new(),
        }
    }
}
fn executable() -> Result<std::path::PathBuf> {
    if let Some(sibling) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("devflow-proxy")))
    {
        if let Ok(path) = which::which(&sibling) {
            return Ok(path);
        }
    }
    which::which("devflow-proxy").context("optional proxy companion is not installed; rerun the installer with DEVFLOW_INSTALL_PROXY=1 or build devflow-proxy with --no-default-features --features proxy")
}
fn api_port() -> u16 {
    crate::config::GlobalConfig::load()
        .ok()
        .flatten()
        .and_then(|g| g.proxy)
        .and_then(|p| p.api_port)
        .unwrap_or(2019)
}
pub async fn query(path: &str) -> Result<serde_json::Value> {
    anyhow::ensure!(
        matches!(path, "status" | "targets" | "ca"),
        "unknown companion endpoint"
    );
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()?
        .get(format!("http://127.0.0.1:{}/api/{path}", api_port()))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .map_err(Into::into)
}
pub async fn status() -> Result<ProxyStatus> {
    let mut status = match query("status").await {
        Ok(value) => serde_json::from_value::<ProxyStatus>(value)?,
        Err(_) => ProxyStatus::default(),
    };
    status.available = status.running || executable().is_ok();
    if let Ok(ca) = query("ca").await {
        status.ca_path = ca["cert_path"].as_str().unwrap_or_default().into();
    }
    Ok(status)
}
fn command_builder(args: &[&str], json: bool) -> Result<tokio::process::Command> {
    let mut command = tokio::process::Command::new(executable()?);
    if json {
        command.arg("--json");
    }
    command
        .args(["--non-interactive", "proxy"])
        .args(args)
        .kill_on_drop(true);
    Ok(command)
}

/// CLI forwarding keeps foreground streams and terminal interaction intact.
pub async fn forward(args: &[&str], json: bool) -> Result<Option<serde_json::Value>> {
    if json {
        return command(args).await.map(Some);
    }
    let status = command_builder(args, false)?.status().await?;
    anyhow::ensure!(status.success(), "proxy companion failed ({status})");
    Ok(None)
}

pub async fn command(args: &[&str]) -> Result<serde_json::Value> {
    let mut command = command_builder(args, true)?;
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .context("proxy companion command timed out")??;
    anyhow::ensure!(
        output.status.success(),
        "proxy companion failed: {}",
        if output.stderr.is_empty() {
            String::from_utf8_lossy(&output.stdout)
        } else {
            String::from_utf8_lossy(&output.stderr)
        }
    );
    serde_json::from_slice(&output.stdout).context("proxy companion returned invalid JSON")
}
pub async fn start() -> Result<ProxyStatus> {
    if status().await?.running {
        return status().await;
    }
    command(&["start", "--daemon"]).await?;
    for _ in 0..40 {
        let status = status().await?;
        if status.running {
            return Ok(status);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    anyhow::bail!("proxy process did not become ready; inspect the companion log")
}
pub async fn stop() -> Result<()> {
    command(&["stop"]).await?;
    Ok(())
}

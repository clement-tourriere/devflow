//! Project process runtime support.
//!
//! This module manages workspace-scoped application processes (web servers,
//! workers, schedulers) natively from devflow. It is intentionally separate
//! from `services`: services model data backends with branch/create/delete
//! semantics, while processes model long-running commands with PID/log/status
//! semantics.

use anyhow::{Context, Result};
use async_trait::async_trait;
use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::process::Command;

use crate::config::Config;
use crate::hooks::{approval::ApprovalStore, build_hook_context, HookContext, TemplateEngine};
use crate::vcs;

const DEFAULT_READY_TIMEOUT_SECS: u64 = 60;
const DEFAULT_STOP_TIMEOUT_SECS: u64 = 3;

/// Top-level process runtime configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProcessesConfig {
    /// Runtime provider. `native` uses devflow's built-in process runner;
    /// `pitchfork` embeds Pitchfork's Rust supervisor/log APIs directly.
    #[serde(default = "default_process_provider")]
    pub provider: String,
    /// Start all configured processes after a successful workspace switch.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_start: bool,
    /// Stop all workspace processes when a workspace is removed.
    #[serde(default = "default_true")]
    pub auto_stop: bool,
    /// Pitchfork-specific integration/reconciliation settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitchfork: Option<PitchforkProcessConfig>,
    /// Process definitions keyed by process name.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub daemons: IndexMap<String, ProcessDaemonConfig>,
}

fn default_process_provider() -> String {
    "native".to_string()
}

fn default_true() -> bool {
    true
}

/// Pitchfork-specific integration/reconciliation settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PitchforkProcessConfig {
    /// How devflow reconciles `.devflow.yml` with Pitchfork config files.
    #[serde(default = "default_pitchfork_config_policy")]
    pub config_policy: String,
    /// How external Pitchfork daemons are surfaced in devflow UX.
    #[serde(default = "default_pitchfork_external_daemons")]
    pub external_daemons: String,
    /// Optional Pitchfork Web UI bridge settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_ui: Option<PitchforkWebUiConfig>,
}

impl Default for PitchforkProcessConfig {
    fn default() -> Self {
        Self {
            config_policy: default_pitchfork_config_policy(),
            external_daemons: default_pitchfork_external_daemons(),
            web_ui: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PitchforkWebUiConfig {
    /// Whether devflow should show/open the Pitchfork Web UI bridge.
    #[serde(default)]
    pub enabled: bool,
    /// Loopback port for Pitchfork's Web UI (default: Pitchfork's 3120).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_port: Option<u16>,
    /// Bind address. Keep this loopback-only for devflow-managed bridges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_address: Option<String>,
    /// How to treat edits made through Pitchfork's Web UI.
    #[serde(default = "default_pitchfork_web_edit_mode")]
    pub edit_mode: String,
}

impl Default for PitchforkWebUiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_port: None,
            bind_address: None,
            edit_mode: default_pitchfork_web_edit_mode(),
        }
    }
}

fn default_pitchfork_config_policy() -> String {
    "devflow-owned".to_string()
}

fn default_pitchfork_external_daemons() -> String {
    "show".to_string()
}

fn default_pitchfork_web_edit_mode() -> String {
    "warn".to_string()
}

fn is_true(value: &bool) -> bool {
    *value
}

/// A configured workspace process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessDaemonConfig {
    /// Shell command to run.
    pub run: String,
    /// Working directory relative to the workspace root/project root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// Environment variables to inject. Values are MiniJinja templates.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub env: IndexMap<String, String>,
    /// Whether a failed start/readiness check should fail lifecycle commands.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub required: bool,
    /// Processes that should be started before this one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends: Vec<String>,
    /// Expected port(s), with optional auto-bump behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<ProcessPortConfig>,
    /// Delay in seconds before considering the process ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_delay: Option<u64>,
    /// TCP port readiness check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_port: Option<u16>,
    /// HTTP readiness URL. Supports `http://host:port/path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_http: Option<String>,
    /// Shell command readiness check. Exit code 0 means ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_cmd: Option<String>,
    /// Regex to wait for in stdout/stderr logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_output: Option<String>,
    /// Override readiness timeout in seconds (default: 60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_timeout: Option<u64>,
    /// Override graceful stop timeout in seconds (default: 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_timeout: Option<u64>,
    /// Unix signal used for graceful shutdown (TERM, INT, HUP, QUIT, KILL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_signal: Option<String>,
    /// File globs polled by the controller daemon for restart-on-change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watch: Vec<String>,
    /// Number of controller-daemon restart attempts after a crash/readiness failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<u32>,
}

/// Process port configuration. Accepts a number, an array, or an object:
///
/// ```yaml
/// port: 3000
/// port: [3000, 3001]
/// port: { expect: [3000], bump: 50 }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessPortConfig {
    pub expect: Vec<u16>,
    pub bump: ProcessPortBump,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProcessPortBump(pub u32);

impl ProcessPortConfig {
    fn resolved_attempts(&self) -> u32 {
        if self.bump.0 == u32::MAX {
            1000
        } else {
            self.bump.0
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProcessPortConfigRaw {
    #[serde(default)]
    expect: Vec<u16>,
    #[serde(default)]
    bump: ProcessPortBump,
}

impl Serialize for ProcessPortConfig {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        if self.bump.0 == 0 {
            if self.expect.len() == 1 {
                serializer.serialize_u16(self.expect[0])
            } else {
                self.expect.serialize(serializer)
            }
        } else {
            ProcessPortConfigRaw {
                expect: self.expect.clone(),
                bump: self.bump,
            }
            .serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for ProcessPortConfig {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ProcessPortConfig;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a port number, array of ports, or { expect, bump } object")
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                let port = u16::try_from(v).map_err(|_| E::custom("port out of range"))?;
                Ok(ProcessPortConfig {
                    expect: vec![port],
                    bump: ProcessPortBump(0),
                })
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                if v < 0 {
                    return Err(E::custom("port cannot be negative"));
                }
                self.visit_u64(v as u64)
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut expect = Vec::new();
                while let Some(port) = seq.next_element::<u16>()? {
                    expect.push(port);
                }
                Ok(ProcessPortConfig {
                    expect,
                    bump: ProcessPortBump(0),
                })
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let raw = ProcessPortConfigRaw::deserialize(
                    serde::de::value::MapAccessDeserializer::new(map),
                )?;
                Ok(ProcessPortConfig {
                    expect: raw.expect,
                    bump: raw.bump,
                })
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl Serialize for ProcessPortBump {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self.0 {
            0 => serializer.serialize_bool(false),
            u32::MAX => serializer.serialize_bool(true),
            n => serializer.serialize_u32(n),
        }
    }
}

impl<'de> Deserialize<'de> for ProcessPortBump {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ProcessPortBump;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a boolean or integer port-bump attempt count")
            }

            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(ProcessPortBump(if v { u32::MAX } else { 0 }))
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                let n = u32::try_from(v).map_err(|_| E::custom("bump count out of range"))?;
                Ok(ProcessPortBump(n))
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                if v < 0 {
                    return Err(E::custom("bump count cannot be negative"));
                }
                self.visit_u64(v as u64)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// How lifecycle auto-start approval is handled for project process commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessApprovalMode {
    /// Prompt before first execution of each configured command template.
    Interactive,
    /// Do not prompt; skip commands that are not pre-approved.
    NonInteractive,
    /// Do not require approval (manual user-invoked starts / GUI actions).
    NoApproval,
}

/// Runtime abstraction for process supervision.
///
/// devflow ships a built-in native runner and a direct Pitchfork-backed runner.
/// The trait keeps lifecycle/CLI code decoupled from the underlying supervisor.
#[async_trait]
pub trait ProcessRuntime: Send + Sync {
    async fn start(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: &str,
        names: &[String],
        force: bool,
    ) -> Result<Vec<ProcessResult>>;

    async fn start_with_approval(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: &str,
        names: &[String],
        force: bool,
        approval_mode: ProcessApprovalMode,
    ) -> Result<Vec<ProcessResult>>;

    async fn stop(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: &str,
        names: &[String],
    ) -> Result<Vec<ProcessResult>>;

    async fn restart(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: &str,
        names: &[String],
    ) -> Result<Vec<ProcessResult>>;

    fn status(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: Option<&str>,
    ) -> Result<Vec<ProcessStatus>>;

    fn logs(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: &str,
        name: &str,
        tail: Option<usize>,
    ) -> Result<String>;

    fn cleanup_workspace_state(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: &str,
    ) -> Result<()>;
}

/// Built-in runtime that supervises host processes directly from devflow.
#[derive(Debug, Default)]
pub struct NativeProcessRuntime;

#[async_trait]
impl ProcessRuntime for NativeProcessRuntime {
    async fn start(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: &str,
        names: &[String],
        force: bool,
    ) -> Result<Vec<ProcessResult>> {
        start_workspace_processes_inner(
            config,
            project_dir,
            workspace,
            names,
            force,
            ProcessApprovalMode::NoApproval,
        )
        .await
    }

    async fn start_with_approval(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: &str,
        names: &[String],
        force: bool,
        approval_mode: ProcessApprovalMode,
    ) -> Result<Vec<ProcessResult>> {
        start_workspace_processes_inner(config, project_dir, workspace, names, force, approval_mode)
            .await
    }

    async fn stop(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: &str,
        names: &[String],
    ) -> Result<Vec<ProcessResult>> {
        stop_workspace_processes_native(config, project_dir, workspace, names).await
    }

    async fn restart(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: &str,
        names: &[String],
    ) -> Result<Vec<ProcessResult>> {
        restart_workspace_processes_native(config, project_dir, workspace, names).await
    }

    fn status(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: Option<&str>,
    ) -> Result<Vec<ProcessStatus>> {
        list_workspace_processes_native(config, project_dir, workspace)
    }

    fn logs(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: &str,
        name: &str,
        tail: Option<usize>,
    ) -> Result<String> {
        process_logs_native(config, project_dir, workspace, name, tail)
    }

    fn cleanup_workspace_state(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: &str,
    ) -> Result<()> {
        cleanup_workspace_process_state_native(config, project_dir, workspace)
    }
}

/// Runtime that embeds Pitchfork's Rust supervisor/log APIs directly.
#[derive(Debug, Default)]
pub struct PitchforkRuntime;

#[async_trait]
impl ProcessRuntime for PitchforkRuntime {
    async fn start(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: &str,
        names: &[String],
        force: bool,
    ) -> Result<Vec<ProcessResult>> {
        start_workspace_processes_pitchfork_inner(
            config,
            project_dir,
            workspace,
            names,
            force,
            ProcessApprovalMode::NoApproval,
        )
        .await
    }

    async fn start_with_approval(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: &str,
        names: &[String],
        force: bool,
        approval_mode: ProcessApprovalMode,
    ) -> Result<Vec<ProcessResult>> {
        start_workspace_processes_pitchfork_inner(
            config,
            project_dir,
            workspace,
            names,
            force,
            approval_mode,
        )
        .await
    }

    async fn stop(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: &str,
        names: &[String],
    ) -> Result<Vec<ProcessResult>> {
        stop_workspace_processes_pitchfork(config, project_dir, workspace, names).await
    }

    async fn restart(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: &str,
        names: &[String],
    ) -> Result<Vec<ProcessResult>> {
        restart_workspace_processes_pitchfork(config, project_dir, workspace, names).await
    }

    fn status(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: Option<&str>,
    ) -> Result<Vec<ProcessStatus>> {
        list_workspace_processes_native(config, project_dir, workspace)
    }

    fn logs(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: &str,
        name: &str,
        tail: Option<usize>,
    ) -> Result<String> {
        process_logs_pitchfork(config, project_dir, workspace, name, tail)
    }

    fn cleanup_workspace_state(
        &self,
        config: &Config,
        project_dir: &Path,
        workspace: &str,
    ) -> Result<()> {
        cleanup_workspace_process_state_native(config, project_dir, workspace)
    }
}

/// Resolve the configured process runtime.
pub fn runtime_for_config(config: &Config) -> Result<Box<dyn ProcessRuntime>> {
    let provider = config
        .processes
        .as_ref()
        .map(|p| p.provider.as_str())
        .unwrap_or("native")
        .to_ascii_lowercase();
    match provider.as_str() {
        "native" => Ok(Box::new(NativeProcessRuntime)),
        "pitchfork" => Ok(Box::new(PitchforkRuntime)),
        other => anyhow::bail!(
            "unknown process provider '{}'; expected native or pitchfork",
            other
        ),
    }
}

/// Process lifecycle result used by workspace and CLI commands.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessResult {
    pub process: String,
    pub success: bool,
    pub message: String,
    /// Whether this result is lifecycle-blocking when it fails.
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<u16>,
}

/// Current process status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessStatus {
    pub process: String,
    pub workspace: String,
    pub pid: Option<u32>,
    pub status: String,
    pub required: bool,
    #[serde(default)]
    pub ports: Vec<u16>,
    #[serde(default)]
    pub urls: Vec<String>,
    pub command: String,
    pub workdir: String,
    pub log_path: String,
    pub retry_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desired_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pitchfork_id: Option<String>,
    /// Whether this process is present in the current `.devflow.yml` process definitions.
    #[serde(default)]
    pub configured: bool,
    /// Human-readable source for GUI/doctor state explanations: `config`,
    /// `config+runtime`, or `runtime_state`.
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProcessStateRecord {
    process: String,
    workspace: String,
    project_key: String,
    project_name: String,
    pid: Option<u32>,
    command: String,
    workdir: String,
    log_path: String,
    ports: Vec<u16>,
    status: String,
    /// Desired state owned by the devflow controller daemon (`running` or `stopped`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    desired_state: Option<String>,
    /// Runtime that produced this actual-state record (`native` or `pitchfork`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runtime: Option<String>,
    /// Pitchfork daemon id (`namespace/name`) when runtime is `pitchfork`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pitchfork_id: Option<String>,
    #[serde(default = "default_true")]
    required: bool,
    started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watch_signature: Option<String>,
    #[serde(default)]
    retry_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
}

#[derive(Debug, Clone)]
struct RenderedProcess {
    command: String,
    workdir: PathBuf,
    env: IndexMap<String, String>,
    ports: Vec<u16>,
    ready_delay: Option<u64>,
    ready_port: Option<u16>,
    ready_http: Option<String>,
    ready_cmd: Option<String>,
    ready_output: Option<String>,
    ready_timeout: Option<u64>,
}

/// Return true when process definitions are configured.
pub fn is_configured(config: &Config) -> bool {
    config
        .processes
        .as_ref()
        .is_some_and(|p| !p.daemons.is_empty())
}

/// Start configured processes for a workspace when `processes.auto_start` is true.
pub async fn auto_start_workspace_processes(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    approval_mode: ProcessApprovalMode,
) -> Vec<ProcessResult> {
    let Some(processes) = config.processes.as_ref() else {
        return Vec::new();
    };
    if !processes.auto_start || processes.daemons.is_empty() {
        return Vec::new();
    }

    let runtime = match runtime_for_config(config) {
        Ok(runtime) => runtime,
        Err(e) => {
            return vec![ProcessResult {
                process: "(process-runtime)".to_string(),
                success: false,
                message: format!("{e:#}"),
                required: true,
                pid: None,
                ports: Vec::new(),
            }]
        }
    };

    match runtime
        .start_with_approval(config, project_dir, workspace, &[], false, approval_mode)
        .await
    {
        Ok(results) => results,
        Err(e) => vec![ProcessResult {
            process: "(process-runtime)".to_string(),
            success: false,
            message: format!("{e:#}"),
            required: true,
            pid: None,
            ports: Vec::new(),
        }],
    }
}

/// Start in `workspace` the processes that are currently desired/running in
/// `parent_workspace`.
///
/// This is used when creating a new isolated workspace: services are branched
/// from the parent, and the process runtime should mirror the parent's active
/// app servers/workers on newly-resolved ports instead of blindly starting every
/// configured daemon.
pub async fn auto_start_workspace_processes_like_parent(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    parent_workspace: &str,
    approval_mode: ProcessApprovalMode,
) -> Vec<ProcessResult> {
    let Some(processes) = config.processes.as_ref() else {
        return Vec::new();
    };
    if !processes.auto_start || processes.daemons.is_empty() {
        return Vec::new();
    }

    let runtime = match runtime_for_config(config) {
        Ok(runtime) => runtime,
        Err(e) => {
            return vec![ProcessResult {
                process: "(process-runtime)".to_string(),
                success: false,
                message: format!("{e:#}"),
                required: true,
                pid: None,
                ports: Vec::new(),
            }];
        }
    };

    let statuses = match runtime.status(config, project_dir, Some(parent_workspace)) {
        Ok(statuses) => statuses,
        Err(e) => {
            return vec![ProcessResult {
                process: "(process-runtime)".to_string(),
                success: false,
                message: format!("failed to inspect parent process state: {e:#}"),
                required: true,
                pid: None,
                ports: Vec::new(),
            }];
        }
    };

    let names: Vec<String> = statuses
        .into_iter()
        .filter(|status| status.configured)
        .filter(|status| {
            status.desired_state.as_deref() == Some("running")
                || matches!(status.status.as_str(), "pending" | "running" | "ready")
        })
        .map(|status| status.process)
        .collect();

    if names.is_empty() {
        return Vec::new();
    }

    match runtime
        .start_with_approval(config, project_dir, workspace, &names, false, approval_mode)
        .await
    {
        Ok(results) => results,
        Err(e) => vec![ProcessResult {
            process: "(process-runtime)".to_string(),
            success: false,
            message: format!("{e:#}"),
            required: true,
            pid: None,
            ports: Vec::new(),
        }],
    }
}

/// Stop workspace processes when a workspace is removed.
pub async fn auto_stop_workspace_processes(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
) -> Vec<ProcessResult> {
    let Some(processes) = config.processes.as_ref() else {
        return Vec::new();
    };
    if !processes.auto_stop || processes.daemons.is_empty() {
        return Vec::new();
    }

    match stop_workspace_processes(config, project_dir, workspace, &[]).await {
        Ok(results) => results,
        Err(e) => vec![ProcessResult {
            process: "(process-runtime)".to_string(),
            success: false,
            message: format!("{e:#}"),
            required: true,
            pid: None,
            ports: Vec::new(),
        }],
    }
}

/// Start selected processes, or all configured processes when `names` is empty.
pub async fn start_workspace_processes(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    names: &[String],
    force: bool,
) -> Result<Vec<ProcessResult>> {
    runtime_for_config(config)?
        .start(config, project_dir, workspace, names, force)
        .await
}

async fn start_workspace_processes_inner(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    names: &[String],
    force: bool,
    approval_mode: ProcessApprovalMode,
) -> Result<Vec<ProcessResult>> {
    let process_names = ordered_process_names(config, names)?;
    let mut results = Vec::with_capacity(process_names.len());
    let mut failed = HashSet::new();
    let mut skipped = HashSet::new();

    for name in process_names {
        let daemon = config
            .processes
            .as_ref()
            .and_then(|p| p.daemons.get(&name))
            .context("process disappeared during start ordering")?;
        if let Some(dep) = daemon.depends.iter().find(|dep| skipped.contains(*dep)) {
            skipped.insert(name.clone());
            results.push(ProcessResult {
                process: name,
                success: true,
                message: format!("skipped because dependency '{}' was skipped", dep),
                required: daemon.required,
                pid: None,
                ports: Vec::new(),
            });
            continue;
        }
        if let Some(dep) = daemon.depends.iter().find(|dep| failed.contains(*dep)) {
            failed.insert(name.clone());
            results.push(ProcessResult {
                process: name,
                success: false,
                message: format!("not started because dependency '{}' failed", dep),
                required: daemon.required,
                pid: None,
                ports: Vec::new(),
            });
            continue;
        }
        if !force && process_already_running(config, project_dir, workspace, &name)? {
            write_desired_state_record(config, project_dir, workspace, &name, "running")?;
            results.push(start_one(config, project_dir, workspace, &name, force).await?);
            continue;
        }
        if let Some(result) =
            check_process_approval(config, project_dir, workspace, &name, approval_mode).await?
        {
            skipped.insert(name.clone());
            results.push(result);
            continue;
        }

        write_desired_state_record(config, project_dir, workspace, &name, "running")?;
        let result = start_one(config, project_dir, workspace, &name, force).await?;
        if !result.success {
            failed.insert(name.clone());
        }
        results.push(result);
    }
    Ok(results)
}

/// Stop selected processes, or all known/configured workspace processes when `names` is empty.
pub async fn stop_workspace_processes(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    names: &[String],
) -> Result<Vec<ProcessResult>> {
    runtime_for_config(config)?
        .stop(config, project_dir, workspace, names)
        .await
}

async fn stop_workspace_processes_native(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    names: &[String],
) -> Result<Vec<ProcessResult>> {
    let names_to_stop = if names.is_empty() {
        let mut names = config
            .processes
            .as_ref()
            .map(|p| p.daemons.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for status in list_workspace_processes(config, project_dir, Some(workspace))? {
            if !names.contains(&status.process) {
                names.push(status.process);
            }
        }
        names.reverse();
        names
    } else {
        names.to_vec()
    };

    let mut results = Vec::with_capacity(names_to_stop.len());
    for name in names_to_stop {
        write_desired_state_record(config, project_dir, workspace, &name, "stopped")?;
        results.push(stop_one(config, project_dir, workspace, &name).await?);
    }
    Ok(results)
}

/// Restart selected processes.
pub async fn restart_workspace_processes(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    names: &[String],
) -> Result<Vec<ProcessResult>> {
    runtime_for_config(config)?
        .restart(config, project_dir, workspace, names)
        .await
}

async fn restart_workspace_processes_native(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    names: &[String],
) -> Result<Vec<ProcessResult>> {
    let _ = stop_workspace_processes_native(config, project_dir, workspace, names).await?;
    start_workspace_processes_inner(
        config,
        project_dir,
        workspace,
        names,
        true,
        ProcessApprovalMode::NoApproval,
    )
    .await
}

async fn start_workspace_processes_pitchfork_inner(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    names: &[String],
    force: bool,
    approval_mode: ProcessApprovalMode,
) -> Result<Vec<ProcessResult>> {
    let process_names = ordered_process_names(config, names)?;
    let mut results = Vec::with_capacity(process_names.len());
    let mut failed = HashSet::new();
    let mut skipped = HashSet::new();

    for name in process_names {
        let daemon = config
            .processes
            .as_ref()
            .and_then(|p| p.daemons.get(&name))
            .context("process disappeared during pitchfork start ordering")?;
        if let Some(dep) = daemon.depends.iter().find(|dep| skipped.contains(*dep)) {
            skipped.insert(name.clone());
            results.push(ProcessResult {
                process: name,
                success: true,
                message: format!("skipped because dependency '{}' was skipped", dep),
                required: daemon.required,
                pid: None,
                ports: Vec::new(),
            });
            continue;
        }
        if let Some(dep) = daemon.depends.iter().find(|dep| failed.contains(*dep)) {
            failed.insert(name.clone());
            results.push(ProcessResult {
                process: name,
                success: false,
                message: format!("not started because dependency '{}' failed", dep),
                required: daemon.required,
                pid: None,
                ports: Vec::new(),
            });
            continue;
        }
        if !force && process_already_running(config, project_dir, workspace, &name)? {
            write_desired_state_record(config, project_dir, workspace, &name, "running")?;
            results.push(start_one_pitchfork(config, project_dir, workspace, &name, force).await?);
            continue;
        }
        if let Some(result) =
            check_process_approval(config, project_dir, workspace, &name, approval_mode).await?
        {
            skipped.insert(name.clone());
            results.push(result);
            continue;
        }

        write_desired_state_record(config, project_dir, workspace, &name, "running")?;
        let result = start_one_pitchfork(config, project_dir, workspace, &name, force).await?;
        if !result.success {
            failed.insert(name.clone());
        }
        results.push(result);
    }
    Ok(results)
}

async fn stop_workspace_processes_pitchfork(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    names: &[String],
) -> Result<Vec<ProcessResult>> {
    let names_to_stop = if names.is_empty() {
        let mut names = config
            .processes
            .as_ref()
            .map(|p| p.daemons.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for status in list_workspace_processes(config, project_dir, Some(workspace))? {
            if !names.contains(&status.process) {
                names.push(status.process);
            }
        }
        names.reverse();
        names
    } else {
        names.to_vec()
    };

    let mut results = Vec::with_capacity(names_to_stop.len());
    for name in names_to_stop {
        write_desired_state_record(config, project_dir, workspace, &name, "stopped")?;
        results.push(stop_one_pitchfork(config, project_dir, workspace, &name).await?);
    }
    Ok(results)
}

async fn restart_workspace_processes_pitchfork(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    names: &[String],
) -> Result<Vec<ProcessResult>> {
    let _ = stop_workspace_processes_pitchfork(config, project_dir, workspace, names).await?;
    start_workspace_processes_pitchfork_inner(
        config,
        project_dir,
        workspace,
        names,
        true,
        ProcessApprovalMode::NoApproval,
    )
    .await
}

/// Return statuses for all process records in this project, optionally scoped to one workspace.
pub fn list_workspace_processes(
    config: &Config,
    project_dir: &Path,
    workspace: Option<&str>,
) -> Result<Vec<ProcessStatus>> {
    runtime_for_config(config)?.status(config, project_dir, workspace)
}

/// Forget one persisted process state record without stopping any OS process.
///
/// This is intended for stale GUI/runtime records left behind after process
/// definitions were removed from `.devflow.yml` or Pitchfork state was cleaned
/// up externally.
pub fn forget_workspace_process_record(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    name: &str,
) -> Result<bool> {
    let paths = runtime_paths(config, project_dir, workspace)?;
    let path = record_path(&paths, name);
    if path.exists() {
        fs::remove_file(path)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn list_workspace_processes_native(
    config: &Config,
    project_dir: &Path,
    workspace: Option<&str>,
) -> Result<Vec<ProcessStatus>> {
    let paths = runtime_paths(config, project_dir, workspace.unwrap_or("main"))?;
    let project_dir = paths.project_root;
    let project_hash = project_hash(&project_dir);
    let state_root = state_root()?.join(project_hash).join("workspaces");

    let mut out = Vec::new();
    let configured_names: HashSet<String> = config
        .processes
        .as_ref()
        .map(|processes| processes.daemons.keys().cloned().collect())
        .unwrap_or_default();
    let workspace_filter = workspace.map(|w| config.get_normalized_workspace_name(w));
    if state_root.exists() {
        for ws_entry in fs::read_dir(state_root)? {
            let ws_entry = ws_entry?;
            if !ws_entry.file_type()?.is_dir() {
                continue;
            }
            let ws_name = ws_entry.file_name().to_string_lossy().to_string();
            if workspace_filter.as_deref().is_some_and(|f| f != ws_name) {
                continue;
            }
            let processes_dir = ws_entry.path().join("processes");
            if !processes_dir.exists() {
                continue;
            }
            for entry in fs::read_dir(processes_dir)? {
                let entry = entry?;
                if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let content = fs::read_to_string(entry.path())?;
                let mut record: ProcessStateRecord = serde_json::from_str(&content)?;
                if record.pid.is_none()
                    && record.desired_state.as_deref() == Some("running")
                    && matches!(record.status.as_str(), "pending" | "running" | "ready")
                {
                    if let Some(pid) = listening_pid_for_ports(&record.ports) {
                        record.pid = Some(pid);
                        record.status = "ready".to_string();
                        record.last_error = None;
                        if let Ok(content) = serde_json::to_string_pretty(&record) {
                            let _ = fs::write(entry.path(), content);
                        }
                    }
                }
                let alive = record.pid.is_some_and(process_alive);
                if !alive && matches!(record.status.as_str(), "running" | "ready") {
                    record.status = "stopped".to_string();
                    record.pid = None;
                    if record.last_error.is_none() {
                        record.last_error = Some("process is not running".to_string());
                    }
                    if let Ok(content) = serde_json::to_string_pretty(&record) {
                        let _ = fs::write(entry.path(), content);
                    }
                }
                let daemon = config
                    .processes
                    .as_ref()
                    .and_then(|processes| processes.daemons.get(&record.process));
                let required = daemon_required(config, &record.process).unwrap_or(record.required);
                let urls = process_urls(config, &record);
                let configured = configured_names.contains(&record.process);
                let command = daemon
                    .map(|daemon| daemon.run.clone())
                    .unwrap_or_else(|| record.command.clone());
                out.push(ProcessStatus {
                    process: record.process,
                    workspace: record.workspace,
                    pid: record.pid,
                    status: record.status,
                    required,
                    ports: record.ports,
                    urls,
                    command,
                    workdir: record.workdir,
                    log_path: record.log_path,
                    retry_count: record.retry_count,
                    last_error: record.last_error,
                    started_at: Some(record.started_at),
                    desired_state: record.desired_state,
                    runtime: record.runtime,
                    pitchfork_id: record.pitchfork_id,
                    configured,
                    source: if configured {
                        "config+runtime".to_string()
                    } else {
                        "runtime_state".to_string()
                    },
                });
            }
        }
    }
    if let Some(workspace_name) = workspace_filter.as_deref() {
        if let Some(processes) = config.processes.as_ref() {
            let paths = runtime_paths(config, project_dir.as_path(), workspace_name)?;
            let provider = processes.provider.to_ascii_lowercase();
            for (name, daemon) in &processes.daemons {
                if out
                    .iter()
                    .any(|status| status.workspace == workspace_name && status.process == *name)
                {
                    continue;
                }
                let workdir = daemon
                    .dir
                    .as_ref()
                    .map(|dir| {
                        let path = PathBuf::from(dir);
                        if path.is_absolute() {
                            path
                        } else {
                            project_dir.join(path)
                        }
                    })
                    .unwrap_or_else(|| project_dir.clone());
                let log_name = if provider == "pitchfork" {
                    format!("{}.pitchfork.log", sanitize_component(name))
                } else {
                    format!("{}.log", sanitize_component(name))
                };
                out.push(ProcessStatus {
                    process: name.clone(),
                    workspace: workspace_name.to_string(),
                    pid: None,
                    status: "not_started".to_string(),
                    required: daemon.required,
                    ports: Vec::new(),
                    urls: Vec::new(),
                    command: daemon.run.clone(),
                    workdir: workdir.display().to_string(),
                    log_path: paths.logs_dir.join(log_name).display().to_string(),
                    retry_count: 0,
                    last_error: None,
                    started_at: None,
                    desired_state: None,
                    runtime: Some(provider.clone()),
                    pitchfork_id: if provider == "pitchfork" {
                        pitchfork_daemon_id(config, project_dir.as_path(), workspace_name, name)
                            .ok()
                            .map(|id| id.qualified())
                    } else {
                        None
                    },
                    configured: true,
                    source: "config".to_string(),
                });
            }
        }
    }

    out.sort_by(|a, b| {
        a.workspace
            .cmp(&b.workspace)
            .then(a.process.cmp(&b.process))
    });
    Ok(out)
}

/// Return log content for a process.
pub fn process_logs(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    name: &str,
    tail: Option<usize>,
) -> Result<String> {
    runtime_for_config(config)?.logs(config, project_dir, workspace, name, tail)
}

fn process_logs_native(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    name: &str,
    tail: Option<usize>,
) -> Result<String> {
    let paths = runtime_paths(config, project_dir, workspace)?;
    let record = read_record(&paths, name)?.with_context(|| {
        format!(
            "process '{}' has not been started in workspace '{}'",
            name, workspace
        )
    })?;
    read_tail(Path::new(&record.log_path), tail)
}

fn process_logs_pitchfork(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    name: &str,
    tail: Option<usize>,
) -> Result<String> {
    let paths = runtime_paths(config, project_dir, workspace)?;
    let record = read_record(&paths, name)?.with_context(|| {
        format!(
            "pitchfork process '{}' has not been started in workspace '{}'",
            name, workspace
        )
    })?;
    let daemon_id = record
        .pitchfork_id
        .as_deref()
        .map(pitchfork_cli::daemon_id::DaemonId::parse)
        .transpose()
        .map_err(|e| anyhow::anyhow!("invalid recorded pitchfork id: {e:#}"))?
        .unwrap_or(pitchfork_daemon_id(config, project_dir, workspace, name)?);
    let logs = pitchfork_logs_for_id(&daemon_id, tail)?;
    let _ = sync_pitchfork_logs_to_file(&daemon_id, Path::new(&record.log_path));
    if logs.trim().is_empty() {
        read_tail(Path::new(&record.log_path), tail)
    } else {
        Ok(logs)
    }
}

/// Remove process records/logs for a deleted workspace.
pub fn cleanup_workspace_process_state(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
) -> Result<()> {
    runtime_for_config(config)?.cleanup_workspace_state(config, project_dir, workspace)
}

fn cleanup_workspace_process_state_native(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
) -> Result<()> {
    let paths = runtime_paths(config, project_dir, workspace)?;
    if let Some(workspace_dir) = paths.processes_dir.parent() {
        if workspace_dir.exists() {
            fs::remove_dir_all(workspace_dir)?;
        }
    }
    Ok(())
}

/// One process reconcile line for the controller daemon.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessReconcileStatus {
    pub project: String,
    pub workspace: String,
    pub process: String,
    pub action: String,
    pub success: bool,
    pub detail: String,
}

/// Reconcile process records for every registered project.
///
/// The controller daemon calls this periodically. It is deliberately polling-
/// based (no platform watcher dependency): running processes with `watch`
/// patterns are restarted when matching files' modification signature changes;
/// crashed processes with `retry > 0` are restarted.
pub async fn reconcile_all_projects_processes() -> Vec<ProcessReconcileStatus> {
    let mut out = Vec::new();
    let Ok(state) = crate::state::LocalStateManager::new() else {
        return out;
    };
    for (project, _) in state.list_all_projects() {
        let project_dir = PathBuf::from(&project);
        if !project_dir.exists() {
            continue;
        }
        let Some(mut config) = load_project_config(&project_dir) else {
            continue;
        };
        if let Some(services) = state.get_services(&project_dir.join(".devflow.yml")) {
            config.services = Some(services);
        }
        if !is_configured(&config) {
            continue;
        }
        match reconcile_project_processes(&config, &project_dir).await {
            Ok(mut lines) => out.append(&mut lines),
            Err(e) => out.push(ProcessReconcileStatus {
                project: project.clone(),
                workspace: "*".to_string(),
                process: "(process-runtime)".to_string(),
                action: "error".to_string(),
                success: false,
                detail: format!("{e:#}"),
            }),
        }
    }
    out
}

async fn reconcile_project_processes(
    config: &Config,
    project_dir: &Path,
) -> Result<Vec<ProcessReconcileStatus>> {
    let mut out = Vec::new();
    let records = read_all_records(config, project_dir)?;
    for mut record in records {
        let Some(daemon) = config
            .processes
            .as_ref()
            .and_then(|p| p.daemons.get(&record.process))
        else {
            continue;
        };

        let alive = record.pid.is_some_and(process_alive);
        match record.desired_state.as_deref() {
            Some("stopped") if alive => {
                let result = stop_workspace_processes(
                    config,
                    project_dir,
                    &record.workspace,
                    &[record.process.clone()],
                )
                .await?
                .into_iter()
                .last()
                .unwrap_or(ProcessResult {
                    process: record.process.clone(),
                    success: false,
                    message: "stop produced no result".to_string(),
                    required: record.required,
                    pid: record.pid,
                    ports: record.ports.clone(),
                });
                out.push(ProcessReconcileStatus {
                    project: project_dir.display().to_string(),
                    workspace: record.workspace.clone(),
                    process: record.process.clone(),
                    action: "desired-stop".to_string(),
                    success: result.success,
                    detail: result.message,
                });
                continue;
            }
            Some("running") if !alive => {
                let result = start_workspace_processes(
                    config,
                    project_dir,
                    &record.workspace,
                    &[record.process.clone()],
                    true,
                )
                .await?
                .into_iter()
                .last()
                .unwrap_or(ProcessResult {
                    process: record.process.clone(),
                    success: false,
                    message: "start produced no result".to_string(),
                    required: record.required,
                    pid: None,
                    ports: Vec::new(),
                });
                out.push(ProcessReconcileStatus {
                    project: project_dir.display().to_string(),
                    workspace: record.workspace.clone(),
                    process: record.process.clone(),
                    action: "desired-start".to_string(),
                    success: result.success,
                    detail: result.message,
                });
                continue;
            }
            _ => {}
        }

        if !alive && record.status != "stopped" && daemon.retry.unwrap_or(0) > record.retry_count {
            let previous_retry_count = record.retry_count;
            let result = start_workspace_processes(
                config,
                project_dir,
                &record.workspace,
                &[record.process.clone()],
                true,
            )
            .await?
            .into_iter()
            .last()
            .unwrap_or(ProcessResult {
                process: record.process.clone(),
                success: false,
                message: "retry produced no result".to_string(),
                required: record.required,
                pid: None,
                ports: Vec::new(),
            });
            let paths = runtime_paths(config, project_dir, &record.workspace)?;
            if let Some(mut latest_record) = read_record(&paths, &record.process)? {
                latest_record.retry_count = previous_retry_count.saturating_add(1);
                latest_record
                    .desired_state
                    .get_or_insert_with(|| "running".to_string());
                if !result.success {
                    latest_record.last_error = Some(result.message.clone());
                }
                write_record(&paths, &latest_record)?;
            }
            out.push(ProcessReconcileStatus {
                project: project_dir.display().to_string(),
                workspace: record.workspace.clone(),
                process: record.process.clone(),
                action: "retry".to_string(),
                success: result.success,
                detail: result.message,
            });
            continue;
        }

        if alive && !daemon.watch.is_empty() {
            let workdir = PathBuf::from(&record.workdir);
            let sig = watch_signature(&workdir, &daemon.watch)?;
            match (record.watch_signature.as_ref(), sig.as_ref()) {
                (Some(old), Some(new)) if old != new => {
                    let result = restart_workspace_processes(
                        config,
                        project_dir,
                        &record.workspace,
                        &[record.process.clone()],
                    )
                    .await?
                    .into_iter()
                    .last()
                    .unwrap_or(ProcessResult {
                        process: record.process.clone(),
                        success: false,
                        message: "restart produced no result".to_string(),
                        required: record.required,
                        pid: None,
                        ports: Vec::new(),
                    });
                    out.push(ProcessReconcileStatus {
                        project: project_dir.display().to_string(),
                        workspace: record.workspace.clone(),
                        process: record.process.clone(),
                        action: "watch-restart".to_string(),
                        success: result.success,
                        detail: result.message,
                    });
                }
                (None, Some(new)) => {
                    record.watch_signature = Some(new.clone());
                    let paths = runtime_paths(config, project_dir, &record.workspace)?;
                    write_record(&paths, &record)?;
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

fn load_project_config(dir: &Path) -> Option<Config> {
    for name in [
        ".devflow.yml",
        ".devflow.yaml",
        ".devflow.toml",
        "devflow.toml",
    ] {
        let path = dir.join(name);
        if path.exists() {
            return Config::from_file(&path).ok();
        }
    }
    None
}

async fn check_process_approval(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    name: &str,
    mode: ProcessApprovalMode,
) -> Result<Option<ProcessResult>> {
    if mode == ProcessApprovalMode::NoApproval || env_auto_approve() {
        return Ok(None);
    }

    let processes = config
        .processes
        .as_ref()
        .context("no processes configured")?;
    let daemon = processes
        .daemons
        .get(name)
        .with_context(|| format!("process '{}' not found in config", name))?;
    let approval_key = &daemon.run;
    let display = render_command_for_approval(config, project_dir, workspace, name).await?;
    let project_key = vcs::resolve_project_root(project_dir)
        .to_string_lossy()
        .to_string();
    let mut store = ApprovalStore::load().unwrap_or_default();
    if store.is_approved(&project_key, approval_key) {
        return Ok(None);
    }

    if mode == ProcessApprovalMode::NonInteractive {
        eprintln!(
            "  Warning: process '{}' skipped — command is not approved for non-interactive mode.",
            name
        );
        eprintln!("    command: {}", display);
        eprintln!(
            "    Approve it once interactively, run `devflow hook approvals add {:?}`, or set DEVFLOW_APPROVE_HOOKS=1 for automated runs.",
            approval_key
        );
        return Ok(Some(ProcessResult {
            process: name.to_string(),
            success: true,
            message: "skipped; requires approval in non-interactive mode".to_string(),
            required: daemon.required,
            pid: None,
            ports: Vec::new(),
        }));
    }

    match prompt_process_approval(name, &display) {
        ProcessApprovalChoice::ApproveAlways => {
            if let Err(e) = store.approve(&project_key, approval_key) {
                log::warn!("Failed to persist process approval: {}", e);
            }
            Ok(None)
        }
        ProcessApprovalChoice::ApproveOnce => Ok(None),
        ProcessApprovalChoice::Deny => Ok(Some(ProcessResult {
            process: name.to_string(),
            success: true,
            message: "skipped; not approved by user".to_string(),
            required: daemon.required,
            pid: None,
            ports: Vec::new(),
        })),
    }
}

async fn render_command_for_approval(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    name: &str,
) -> Result<String> {
    let daemon = config
        .processes
        .as_ref()
        .and_then(|p| p.daemons.get(name))
        .with_context(|| format!("process '{}' not found in config", name))?;
    let context = build_process_context(config, project_dir, workspace).await;
    TemplateEngine::new().render(&daemon.run, &context)
}

fn env_auto_approve() -> bool {
    std::env::var("DEVFLOW_APPROVE_HOOKS")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessApprovalChoice {
    ApproveAlways,
    ApproveOnce,
    Deny,
}

fn prompt_process_approval(name: &str, rendered_command: &str) -> ProcessApprovalChoice {
    eprintln!("\nProcess '{}' wants to run:", name);
    eprintln!("  {}", rendered_command);
    eprint!("Approve? [y]es once / [a]lways / [n]o: ");
    let _ = std::io::stderr().flush();

    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return ProcessApprovalChoice::Deny;
    }
    match input.trim().to_lowercase().as_str() {
        "a" | "always" => ProcessApprovalChoice::ApproveAlways,
        "y" | "yes" | "" => ProcessApprovalChoice::ApproveOnce,
        _ => ProcessApprovalChoice::Deny,
    }
}

fn process_already_running(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    name: &str,
) -> Result<bool> {
    let paths = runtime_paths(config, project_dir, workspace)?;
    Ok(read_record(&paths, name)?
        .and_then(|record| record.pid)
        .is_some_and(process_alive))
}

fn write_desired_state_record(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    name: &str,
    desired_state: &str,
) -> Result<()> {
    let paths = runtime_paths(config, project_dir, workspace)?;
    fs::create_dir_all(&paths.processes_dir)?;
    fs::create_dir_all(&paths.logs_dir)?;
    let provider = config
        .processes
        .as_ref()
        .map(|p| p.provider.to_ascii_lowercase())
        .unwrap_or_else(|| "native".to_string());
    let daemon = config.processes.as_ref().and_then(|p| p.daemons.get(name));
    let mut record = match read_record(&paths, name)? {
        Some(record) => record,
        None => {
            let workdir = daemon
                .and_then(|d| d.dir.as_ref())
                .map(PathBuf::from)
                .map(|path| {
                    if path.is_absolute() {
                        path
                    } else {
                        paths.project_root.join(path)
                    }
                })
                .unwrap_or_else(|| paths.project_root.clone());
            let log_name = if provider == "pitchfork" {
                format!("{}.pitchfork.log", sanitize_component(name))
            } else {
                format!("{}.log", sanitize_component(name))
            };
            ProcessStateRecord {
                process: name.to_string(),
                workspace: paths.workspace.clone(),
                project_key: paths.project_root.display().to_string(),
                project_name: config.project_name(),
                pid: None,
                command: daemon.map(|d| d.run.clone()).unwrap_or_default(),
                workdir: workdir.display().to_string(),
                log_path: paths.logs_dir.join(log_name).display().to_string(),
                ports: Vec::new(),
                status: "pending".to_string(),
                desired_state: None,
                runtime: Some(provider.clone()),
                pitchfork_id: if provider == "pitchfork" {
                    pitchfork_daemon_id(config, project_dir, workspace, name)
                        .ok()
                        .map(|id| id.qualified())
                } else {
                    None
                },
                required: daemon.map(|d| d.required).unwrap_or(true),
                started_at: chrono::Utc::now().to_rfc3339(),
                watch_signature: None,
                retry_count: 0,
                last_error: None,
            }
        }
    };
    record.desired_state = Some(desired_state.to_string());
    if desired_state == "running" && record.pid.is_none() {
        record.status = "pending".to_string();
    } else if desired_state == "stopped" && record.pid.is_none() {
        record.status = "stopped".to_string();
    }
    record.runtime.get_or_insert(provider);
    write_record(&paths, &record)
}

async fn start_one_pitchfork(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    name: &str,
    force: bool,
) -> Result<ProcessResult> {
    let paths = runtime_paths(config, project_dir, workspace)?;
    fs::create_dir_all(&paths.processes_dir)?;
    fs::create_dir_all(&paths.logs_dir)?;
    let required = daemon_required(config, name).unwrap_or(true);

    if let Some(record) = read_record(&paths, name)? {
        if let Some(pid) = record.pid {
            if process_alive(pid) {
                if !force {
                    return Ok(ProcessResult {
                        process: name.to_string(),
                        success: true,
                        message: format!("pitchfork process '{}' already running", name),
                        required: record.required,
                        pid: Some(pid),
                        ports: record.ports,
                    });
                }
                let _ = stop_one_pitchfork(config, project_dir, workspace, name).await;
            }
        }
    }

    let rendered = render_process(config, project_dir, workspace, name).await?;
    if !rendered.workdir.is_dir() {
        anyhow::bail!(
            "working directory for process '{}' does not exist: {}",
            name,
            rendered.workdir.display()
        );
    }

    let daemon_id = pitchfork_daemon_id(config, project_dir, workspace, name)?;
    let log_path = paths
        .logs_dir
        .join(format!("{}.pitchfork.log", sanitize_component(name)));
    append_log_header(&log_path, name, &rendered.command)?;
    let mut run_options = pitchfork_run_options(
        config,
        project_dir,
        workspace,
        name,
        &rendered,
        &log_path,
        force,
    )?;
    // Use Pitchfork for supervision, but keep readiness waiting in devflow.
    // Pitchfork's direct `Supervisor::run(wait_ready=true)` can wait forever
    // for long-running commands whose readiness signal never arrives, which
    // blocks workspace create/switch in every frontend. Starting first and then
    // running devflow's bounded readiness checks gives CLI/TUI/GUI the same
    // behavior as the native runtime.
    run_options.wait_ready = false;

    let response = pitchfork_cli::supervisor::SUPERVISOR
        .run(run_options)
        .await
        .map_err(|e| anyhow::anyhow!("pitchfork supervisor start failed: {e:#}"))?;

    let started_at = chrono::Utc::now().to_rfc3339();
    match response {
        pitchfork_cli::ipc::IpcResponse::DaemonReady { daemon }
        | pitchfork_cli::ipc::IpcResponse::DaemonStart { daemon } => {
            let ports = if daemon.resolved_port.is_empty() {
                rendered.ports.clone()
            } else {
                daemon.resolved_port.clone()
            };
            let mut record = ProcessStateRecord {
                process: name.to_string(),
                workspace: paths.workspace.clone(),
                project_key: paths.project_root.display().to_string(),
                project_name: config.project_name(),
                pid: daemon.pid,
                command: rendered.command.clone(),
                workdir: rendered.workdir.display().to_string(),
                log_path: log_path.display().to_string(),
                ports: ports.clone(),
                status: "running".to_string(),
                desired_state: Some("running".to_string()),
                runtime: Some("pitchfork".to_string()),
                pitchfork_id: Some(daemon_id.qualified()),
                required,
                started_at,
                watch_signature: watch_signature(
                    &rendered.workdir,
                    &daemon_watch_patterns(config, name),
                )
                .ok()
                .flatten(),
                retry_count: 0,
                last_error: None,
            };
            if daemon.pid.is_none() {
                record.status = "failed".to_string();
                record.last_error = Some("pitchfork did not report a pid".to_string());
                write_record(&paths, &record)?;
                let _ = sync_pitchfork_logs_to_file(&daemon_id, &log_path);
                return Ok(ProcessResult {
                    process: name.to_string(),
                    success: false,
                    message: format!(
                        "started pitchfork process '{}' but no pid was reported",
                        name
                    ),
                    required,
                    pid: None,
                    ports,
                });
            }
            write_record(&paths, &record)?;

            let mut wait_rendered = rendered.clone();
            let no_ready_check = wait_rendered.ready_delay.is_none()
                && wait_rendered.ready_output.is_none()
                && wait_rendered.ready_http.is_none()
                && wait_rendered.ready_port.is_none()
                && wait_rendered.ready_cmd.is_none();
            if no_ready_check {
                if let Some(port) = ports.first().copied() {
                    wait_rendered.ready_port = Some(port);
                } else {
                    wait_rendered.ready_delay = Some(0);
                }
            }

            let ready = wait_ready(&wait_rendered, &log_path, record.pid).await;
            match ready {
                Ok(()) => {
                    record.status = "ready".to_string();
                    record.last_error = None;
                    write_record(&paths, &record)?;
                    let _ = sync_pitchfork_logs_to_file(&daemon_id, &log_path);
                    Ok(ProcessResult {
                        process: name.to_string(),
                        success: true,
                        message: format!("started pitchfork process '{}'", name),
                        required,
                        pid: record.pid,
                        ports,
                    })
                }
                Err(e) => {
                    let error = format!("{e:#}");
                    record.status = if record.pid.is_some_and(process_alive) {
                        "running".to_string()
                    } else {
                        "failed".to_string()
                    };
                    record.last_error = Some(error.clone());
                    write_record(&paths, &record)?;
                    let _ = sync_pitchfork_logs_to_file(&daemon_id, &log_path);
                    Ok(ProcessResult {
                        process: name.to_string(),
                        success: false,
                        message: format!(
                            "started pitchfork process '{}' but readiness failed: {}",
                            name, error
                        ),
                        required,
                        pid: record.pid,
                        ports,
                    })
                }
            }
        }
        pitchfork_cli::ipc::IpcResponse::DaemonAlreadyRunning => {
            let existing = read_record(&paths, name)?;
            let ports = existing
                .as_ref()
                .map(|r| r.ports.clone())
                .filter(|ports| !ports.is_empty())
                .unwrap_or_else(|| rendered.ports.clone());
            let pid = existing
                .as_ref()
                .and_then(|r| r.pid)
                .or_else(|| listening_pid_for_ports(&ports));

            if let Some(mut record) = existing {
                record.pid = pid;
                record.ports = ports.clone();
                record.command = rendered.command.clone();
                record.workdir = rendered.workdir.display().to_string();
                record.log_path = log_path.display().to_string();
                record.desired_state = Some("running".to_string());
                record.status = if pid.is_some() {
                    "ready".to_string()
                } else {
                    "pending".to_string()
                };
                record.runtime = Some("pitchfork".to_string());
                record.pitchfork_id = Some(daemon_id.qualified());
                record.last_error = None;
                write_record(&paths, &record)?;
            }

            Ok(ProcessResult {
                process: name.to_string(),
                success: true,
                message: format!("pitchfork process '{}' already running", name),
                required,
                pid,
                ports,
            })
        }
        pitchfork_cli::ipc::IpcResponse::DaemonFailed { error } => {
            write_pitchfork_failure_record(
                config,
                &paths,
                name,
                &rendered,
                &log_path,
                &daemon_id,
                required,
                format!("pitchfork failed to start: {error}"),
            )?;
            Ok(ProcessResult {
                process: name.to_string(),
                success: false,
                message: format!("pitchfork failed to start process '{}': {}", name, error),
                required,
                pid: None,
                ports: rendered.ports,
            })
        }
        pitchfork_cli::ipc::IpcResponse::DaemonFailedWithCode { exit_code } => {
            let message = format!(
                "pitchfork process '{}' exited before ready (exit code {:?})",
                name, exit_code
            );
            write_pitchfork_failure_record(
                config,
                &paths,
                name,
                &rendered,
                &log_path,
                &daemon_id,
                required,
                message.clone(),
            )?;
            Ok(ProcessResult {
                process: name.to_string(),
                success: false,
                message,
                required,
                pid: None,
                ports: rendered.ports,
            })
        }
        pitchfork_cli::ipc::IpcResponse::PortConflict { port, process, pid } => {
            let message = format!(
                "pitchfork port {} is already used by '{}' (pid {})",
                port, process, pid
            );
            write_pitchfork_failure_record(
                config,
                &paths,
                name,
                &rendered,
                &log_path,
                &daemon_id,
                required,
                message.clone(),
            )?;
            Ok(ProcessResult {
                process: name.to_string(),
                success: false,
                message,
                required,
                pid: None,
                ports: Vec::new(),
            })
        }
        pitchfork_cli::ipc::IpcResponse::NoAvailablePort {
            start_port,
            attempts,
        } => {
            let message = format!(
                "pitchfork found no available port after {} attempt(s) starting at {}",
                attempts, start_port
            );
            write_pitchfork_failure_record(
                config,
                &paths,
                name,
                &rendered,
                &log_path,
                &daemon_id,
                required,
                message.clone(),
            )?;
            Ok(ProcessResult {
                process: name.to_string(),
                success: false,
                message,
                required,
                pid: None,
                ports: Vec::new(),
            })
        }
        other => {
            let message = format!(
                "unexpected pitchfork response while starting '{}': {:?}",
                name, other
            );
            write_pitchfork_failure_record(
                config,
                &paths,
                name,
                &rendered,
                &log_path,
                &daemon_id,
                required,
                message.clone(),
            )?;
            Ok(ProcessResult {
                process: name.to_string(),
                success: false,
                message,
                required,
                pid: None,
                ports: rendered.ports,
            })
        }
    }
}

async fn stop_one_pitchfork(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    name: &str,
) -> Result<ProcessResult> {
    let paths = runtime_paths(config, project_dir, workspace)?;
    let required = daemon_required(config, name).unwrap_or(true);
    let Some(mut record) = read_record(&paths, name)? else {
        return Ok(ProcessResult {
            process: name.to_string(),
            success: true,
            message: format!("pitchfork process '{}' not recorded", name),
            required,
            pid: None,
            ports: Vec::new(),
        });
    };
    let pid = record.pid;
    let ports = record.ports.clone();
    let daemon_id = record
        .pitchfork_id
        .as_deref()
        .and_then(|id| pitchfork_cli::daemon_id::DaemonId::parse(id).ok())
        .or_else(|| pitchfork_daemon_id(config, project_dir, workspace, name).ok());

    let mut stopped_by_pitchfork = false;
    if let Some(id) = daemon_id.as_ref() {
        match pitchfork_cli::supervisor::SUPERVISOR.stop(id).await {
            Ok(pitchfork_cli::ipc::IpcResponse::Ok) => {
                stopped_by_pitchfork = true;
            }
            Ok(pitchfork_cli::ipc::IpcResponse::DaemonWasNotRunning)
            | Ok(pitchfork_cli::ipc::IpcResponse::DaemonNotRunning)
            | Ok(pitchfork_cli::ipc::IpcResponse::DaemonNotFound) => {
                // A fresh CLI/GUI process can have a persisted devflow record
                // for a process that Pitchfork's in-memory supervisor no
                // longer knows about. If we still have a live pid, fall back to
                // devflow's process-group termination below instead of treating
                // the Pitchfork "not found" response as a successful stop.
                stopped_by_pitchfork = pid.is_none_or(|pid| !process_alive(pid));
            }
            Ok(pitchfork_cli::ipc::IpcResponse::DaemonStopFailed { error }) => {
                record.last_error = Some(error.clone());
                write_record(&paths, &record)?;
                return Ok(ProcessResult {
                    process: name.to_string(),
                    success: false,
                    message: format!("pitchfork failed to stop process '{}': {}", name, error),
                    required: record.required,
                    pid,
                    ports,
                });
            }
            Ok(other) => {
                log::debug!(
                    "unexpected pitchfork stop response for {}: {:?}",
                    name,
                    other
                );
            }
            Err(e) => {
                log::debug!("pitchfork stop failed for {}: {e:#}", name);
            }
        }
    }

    if !stopped_by_pitchfork {
        if let Some(pid) = pid {
            if process_alive(pid) {
                let stop_timeout = daemon_stop_timeout(config, name);
                let shutdown_signal = daemon_shutdown_signal(config, name);
                terminate_process(pid, stop_timeout, shutdown_signal.as_deref()).await?;
            }
        }
    }

    record.status = "stopped".to_string();
    record.pid = None;
    record.desired_state = Some("stopped".to_string());
    record.last_error = None;
    write_record(&paths, &record)?;
    Ok(ProcessResult {
        process: name.to_string(),
        success: true,
        message: format!("stopped pitchfork process '{}'", name),
        required: record.required,
        pid,
        ports,
    })
}

async fn start_one(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    name: &str,
    force: bool,
) -> Result<ProcessResult> {
    let paths = runtime_paths(config, project_dir, workspace)?;
    fs::create_dir_all(&paths.processes_dir)?;
    fs::create_dir_all(&paths.logs_dir)?;
    let required = daemon_required(config, name).unwrap_or(true);

    if let Some(record) = read_record(&paths, name)? {
        if let Some(pid) = record.pid {
            if process_alive(pid) {
                if !force {
                    return Ok(ProcessResult {
                        process: name.to_string(),
                        success: true,
                        message: format!("process '{}' already running", name),
                        required: record.required,
                        pid: Some(pid),
                        ports: record.ports,
                    });
                }
                let _ = stop_one(config, project_dir, workspace, name).await;
            }
        }
    }

    let rendered = render_process(config, project_dir, workspace, name).await?;
    if !rendered.workdir.is_dir() {
        anyhow::bail!(
            "working directory for process '{}' does not exist: {}",
            name,
            rendered.workdir.display()
        );
    }

    let log_path = paths
        .logs_dir
        .join(format!("{}.log", sanitize_component(name)));
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open process log {}", log_path.display()))?;
    let stderr = stdout.try_clone()?;

    append_log_header(&log_path, name, &rendered.command)?;

    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&rendered.command)
        .current_dir(&rendered.workdir)
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::from(stderr))
        .stdin(std::process::Stdio::null());
    #[cfg(unix)]
    unsafe {
        use nix::unistd::{setpgid, Pid};
        cmd.pre_exec(|| {
            setpgid(Pid::from_raw(0), Pid::from_raw(0))
                .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
            Ok(())
        });
    }
    for (key, value) in &rendered.env {
        cmd.env(key, value);
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn process '{}'", name))?;
    let pid = child.id();
    // Dropping Child without waiting leaves the process running; logs are file-backed.
    drop(child);

    let mut record = ProcessStateRecord {
        process: name.to_string(),
        workspace: paths.workspace.clone(),
        project_key: paths.project_root.display().to_string(),
        project_name: config.project_name(),
        pid,
        command: rendered.command.clone(),
        workdir: rendered.workdir.display().to_string(),
        log_path: log_path.display().to_string(),
        ports: rendered.ports.clone(),
        status: "running".to_string(),
        desired_state: Some("running".to_string()),
        runtime: Some("native".to_string()),
        pitchfork_id: None,
        required,
        started_at: chrono::Utc::now().to_rfc3339(),
        watch_signature: watch_signature(&rendered.workdir, &daemon_watch_patterns(config, name))
            .ok()
            .flatten(),
        retry_count: 0,
        last_error: None,
    };
    write_record(&paths, &record)?;

    let ready = wait_ready(&rendered, &log_path, pid).await;
    match ready {
        Ok(()) => {
            record.status = "ready".to_string();
            write_record(&paths, &record)?;
            Ok(ProcessResult {
                process: name.to_string(),
                success: true,
                message: format!("started process '{}'", name),
                required,
                pid,
                ports: rendered.ports,
            })
        }
        Err(e) => {
            let error = format!("{e:#}");
            record.status = if pid.is_some_and(process_alive) {
                "running".to_string()
            } else {
                "failed".to_string()
            };
            record.last_error = Some(error.clone());
            write_record(&paths, &record)?;
            Ok(ProcessResult {
                process: name.to_string(),
                success: false,
                message: format!("started process '{}' but readiness failed: {}", name, error),
                required,
                pid,
                ports: rendered.ports,
            })
        }
    }
}

async fn stop_one(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    name: &str,
) -> Result<ProcessResult> {
    let paths = runtime_paths(config, project_dir, workspace)?;
    let required = daemon_required(config, name).unwrap_or(true);
    let Some(mut record) = read_record(&paths, name)? else {
        return Ok(ProcessResult {
            process: name.to_string(),
            success: true,
            message: format!("process '{}' not recorded", name),
            required,
            pid: None,
            ports: Vec::new(),
        });
    };
    let pid = record.pid;
    let ports = record.ports.clone();

    if let Some(pid) = pid {
        if process_alive(pid) {
            let stop_timeout = daemon_stop_timeout(config, name);
            let shutdown_signal = daemon_shutdown_signal(config, name);
            terminate_process(pid, stop_timeout, shutdown_signal.as_deref()).await?;
            record.status = "stopped".to_string();
            record.pid = None;
            record.desired_state = Some("stopped".to_string());
            record.last_error = None;
            write_record(&paths, &record)?;
            return Ok(ProcessResult {
                process: name.to_string(),
                success: true,
                message: format!("stopped process '{}'", name),
                required: record.required,
                pid: Some(pid),
                ports,
            });
        }
    }

    record.status = "stopped".to_string();
    record.pid = None;
    record.desired_state = Some("stopped".to_string());
    record.last_error = None;
    write_record(&paths, &record)?;
    Ok(ProcessResult {
        process: name.to_string(),
        success: true,
        message: format!("process '{}' was not running", name),
        required: record.required,
        pid,
        ports,
    })
}

async fn render_process(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    name: &str,
) -> Result<RenderedProcess> {
    let processes = config
        .processes
        .as_ref()
        .context("no processes configured")?;
    let daemon = processes
        .daemons
        .get(name)
        .with_context(|| format!("process '{}' not found in config", name))?;

    let context = build_process_context(config, project_dir, workspace).await;
    let template = TemplateEngine::new();
    let workspace_root = context
        .worktree_path
        .as_ref()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| project_dir.to_path_buf());

    let command = template.render(&daemon.run, &context)?;
    let workdir = match daemon.dir.as_ref() {
        Some(dir) => {
            let rendered = template.render(dir, &context)?;
            let path = PathBuf::from(rendered);
            if path.is_absolute() {
                path
            } else {
                workspace_root.join(path)
            }
        }
        None => workspace_root,
    };

    let ports = resolve_ports(daemon.port.as_ref()).await?;
    let mut env = IndexMap::new();
    for (key, value) in &daemon.env {
        env.insert(key.clone(), template.render(value, &context)?);
    }
    if let Some(port) = ports.first() {
        env.entry("PORT".to_string())
            .or_insert_with(|| port.to_string());
    }

    let ready_port = daemon
        .ready_port
        .map(|p| remap_port(p, daemon.port.as_ref(), &ports));
    let ready_http = daemon
        .ready_http
        .as_ref()
        .map(|s| {
            let rendered = template.render(s, &context)?;
            Ok::<_, anyhow::Error>(remap_ready_http(&rendered, daemon.port.as_ref(), &ports))
        })
        .transpose()?;
    let ready_cmd = daemon
        .ready_cmd
        .as_ref()
        .map(|s| template.render(s, &context))
        .transpose()?;

    Ok(RenderedProcess {
        command,
        workdir,
        env,
        ports,
        ready_delay: daemon.ready_delay,
        ready_port,
        ready_http,
        ready_cmd,
        ready_output: daemon.ready_output.clone(),
        ready_timeout: daemon.ready_timeout,
    })
}

fn pitchfork_daemon_id(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    name: &str,
) -> Result<pitchfork_cli::daemon_id::DaemonId> {
    let project_root = vcs::resolve_project_root(project_dir)
        .canonicalize()
        .unwrap_or_else(|_| vcs::resolve_project_root(project_dir));
    let project_hash = project_hash(&project_root);
    let workspace = dns_label(&config.get_normalized_workspace_name(workspace));
    let namespace = if workspace.is_empty() {
        format!("df{}", &project_hash[..12.min(project_hash.len())])
    } else {
        format!(
            "df{}-{}",
            &project_hash[..12.min(project_hash.len())],
            workspace
        )
    };
    let process_name = dns_label(name);
    pitchfork_cli::daemon_id::DaemonId::try_new(namespace, process_name)
        .map_err(|e| anyhow::anyhow!("invalid pitchfork daemon id for '{}': {e:#}", name))
}

fn pitchfork_run_script(rendered: &RenderedProcess, log_path: &Path) -> String {
    // Directly embedded Pitchfork supervisors live only as long as the devflow
    // CLI/GUI process that started them. If stdout/stderr are only captured by
    // Pitchfork's in-memory reader task, logs disappear when a short-lived CLI
    // command exits. Redirect process output inside the shell command as well,
    // so devflow's per-process log file remains authoritative and searchable.
    // Redirect the shell's own file descriptors with `exec` before running the
    // command; otherwise command-substitution/GUI callers can keep waiting for
    // EOF on inherited pipes while the daemon is still alive.
    // devflow performs bounded readiness checks itself, including ready_output
    // regexes, so the Pitchfork reader does not need to see stdout/stderr.
    format!(
        "exec >> {} 2>&1; {{ {}; }}",
        shell_single_quote(&log_path.display().to_string()),
        rendered.command
    )
}

fn shell_single_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn command_looks_pythonish(command: &str) -> bool {
    command.contains("python") || command.contains("celery") || command.contains("manage.py")
}

fn pitchfork_run_options(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
    name: &str,
    rendered: &RenderedProcess,
    log_path: &Path,
    force: bool,
) -> Result<pitchfork_cli::daemon::RunOptions> {
    let daemon = config
        .processes
        .as_ref()
        .and_then(|p| p.daemons.get(name))
        .with_context(|| format!("process '{}' not found in config", name))?;
    let id = pitchfork_daemon_id(config, project_dir, workspace, name)?;
    let depends = daemon
        .depends
        .iter()
        .map(|dep| pitchfork_daemon_id(config, project_dir, workspace, dep))
        .collect::<Result<Vec<_>>>()?;
    let mut env_map = rendered.env.clone();
    if command_looks_pythonish(&rendered.command) {
        env_map
            .entry("PYTHONUNBUFFERED".to_string())
            .or_insert_with(|| "1".to_string());
    }
    let env = if env_map.is_empty() {
        None
    } else {
        Some(env_map)
    };
    let stop_signal = pitchfork_stop_config(daemon)?;
    let port = pitchfork_cli::pitchfork_toml::PortConfig::from_parts(
        rendered.ports.clone(),
        pitchfork_cli::pitchfork_toml::PortBump(0),
    );

    let no_ready_check = rendered.ready_delay.is_none()
        && rendered.ready_output.is_none()
        && rendered.ready_http.is_none()
        && rendered.ready_port.is_none()
        && rendered.ready_cmd.is_none();
    let implicit_ready_port = no_ready_check
        .then(|| rendered.ports.first().copied())
        .flatten();
    let run_script = pitchfork_run_script(rendered, log_path);

    Ok(pitchfork_cli::daemon::RunOptions {
        id,
        cmd: vec![run_script.clone()],
        run: Some(run_script),
        force,
        shell_pid: None,
        dir: pitchfork_cli::pitchfork_toml::Dir(rendered.workdir.clone()),
        autostop: false,
        cron_schedule: None,
        cron_retrigger: None,
        cron_immediate: None,
        retry: pitchfork_cli::pitchfork_toml::Retry(daemon.retry.unwrap_or(0)),
        retry_count: 0,
        // Pitchfork's direct Supervisor::run waits forever when wait_ready=true
        // and no readiness signal is configured. For portless daemons, a
        // zero-second delay preserves devflow's native semantics. For daemons
        // with expected ports, use the first resolved port as the readiness
        // probe so crashed web servers do not appear "ready".
        ready_delay: rendered
            .ready_delay
            .or_else(|| (no_ready_check && implicit_ready_port.is_none()).then_some(0)),
        ready_output: rendered.ready_output.clone(),
        ready_http: rendered
            .ready_http
            .as_ref()
            .map(|url| pitchfork_cli::pitchfork_toml::ReadyHttp::new(url.clone())),
        ready_port: rendered.ready_port.or(implicit_ready_port),
        ready_cmd: rendered.ready_cmd.clone(),
        port,
        wait_ready: true,
        depends,
        env,
        watch: daemon.watch.clone(),
        watch_mode: pitchfork_cli::pitchfork_toml::WatchMode::Native,
        watch_base_dir: Some(rendered.workdir.clone()),
        mise: None,
        slug: None,
        proxy: Some(false),
        user: None,
        memory_limit: None,
        cpu_limit: None,
        stop_signal,
        on_output_hook: None,
        pty: None,
    })
}

fn pitchfork_stop_config(
    daemon: &ProcessDaemonConfig,
) -> Result<Option<pitchfork_cli::pitchfork_toml::StopConfig>> {
    if daemon.shutdown_signal.is_none() && daemon.stop_timeout.is_none() {
        return Ok(None);
    }
    let signal = daemon
        .shutdown_signal
        .as_ref()
        .map(|signal| pitchfork_cli::pitchfork_toml::StopSignal::try_from(signal.clone()))
        .transpose()
        .map_err(|e| anyhow::anyhow!("invalid shutdown_signal: {e}"))?
        .unwrap_or_default();
    Ok(Some(pitchfork_cli::pitchfork_toml::StopConfig {
        signal,
        timeout: daemon.stop_timeout.map(Duration::from_secs),
    }))
}

#[allow(clippy::too_many_arguments)]
fn write_pitchfork_failure_record(
    config: &Config,
    paths: &RuntimePaths,
    name: &str,
    rendered: &RenderedProcess,
    log_path: &Path,
    daemon_id: &pitchfork_cli::daemon_id::DaemonId,
    required: bool,
    error: String,
) -> Result<()> {
    let record = ProcessStateRecord {
        process: name.to_string(),
        workspace: paths.workspace.clone(),
        project_key: paths.project_root.display().to_string(),
        project_name: config.project_name(),
        pid: None,
        command: rendered.command.clone(),
        workdir: rendered.workdir.display().to_string(),
        log_path: log_path.display().to_string(),
        ports: rendered.ports.clone(),
        status: "failed".to_string(),
        desired_state: Some("running".to_string()),
        runtime: Some("pitchfork".to_string()),
        pitchfork_id: Some(daemon_id.qualified()),
        required,
        started_at: chrono::Utc::now().to_rfc3339(),
        watch_signature: None,
        retry_count: 0,
        last_error: Some(error),
    };
    write_record(paths, &record)
}

fn pitchfork_logs_for_id(
    daemon_id: &pitchfork_cli::daemon_id::DaemonId,
    tail: Option<usize>,
) -> Result<String> {
    use pitchfork_cli::log_store::{LogQuery, LogStore};

    let mut entries = pitchfork_cli::log_store::sqlite::LOG_STORE
        .query(&LogQuery {
            daemon_ids: vec![daemon_id.qualified()],
            limit: tail,
            order_desc: tail.is_some(),
            ..Default::default()
        })
        .map_err(|e| anyhow::anyhow!("failed to read pitchfork logs: {e:#}"))?;
    if tail.is_some() {
        entries.reverse();
    }
    let mut out = String::new();
    for entry in entries {
        out.push_str(&entry.message);
        if !entry.message.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(out)
}

fn sync_pitchfork_logs_to_file(
    daemon_id: &pitchfork_cli::daemon_id::DaemonId,
    log_path: &Path,
) -> Result<()> {
    let logs = pitchfork_logs_for_id(daemon_id, None)?;
    if !logs.trim().is_empty() {
        fs::write(log_path, logs)?;
    }
    Ok(())
}

async fn build_process_context(
    config: &Config,
    project_dir: &Path,
    workspace: &str,
) -> HookContext {
    build_hook_context(config, project_dir, workspace).await
}

async fn resolve_ports(port_config: Option<&ProcessPortConfig>) -> Result<Vec<u16>> {
    let Some(config) = port_config else {
        return Ok(Vec::new());
    };
    if config.expect.is_empty() {
        return Ok(Vec::new());
    }
    let attempts = config.resolved_attempts();
    for offset in 0..=attempts {
        let mut candidate = Vec::with_capacity(config.expect.len());
        let mut ok = true;
        for port in &config.expect {
            let Some(adjusted) = port.checked_add(offset as u16) else {
                ok = false;
                break;
            };
            if !port_available(adjusted).await {
                ok = false;
                break;
            }
            candidate.push(adjusted);
        }
        if ok {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "no available process ports starting at {:?} after {} bump attempt(s)",
        config.expect,
        attempts
    )
}

async fn port_available(port: u16) -> bool {
    tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map(|listener| {
            drop(listener);
            true
        })
        .unwrap_or(false)
}

fn remap_port(port: u16, config: Option<&ProcessPortConfig>, ports: &[u16]) -> u16 {
    let Some(config) = config else {
        return port;
    };
    config
        .expect
        .iter()
        .position(|p| *p == port)
        .and_then(|idx| ports.get(idx).copied())
        .unwrap_or(port)
}

fn remap_ready_http(url: &str, config: Option<&ProcessPortConfig>, ports: &[u16]) -> String {
    let Some(config) = config else {
        return url.to_string();
    };
    let mut out = url.to_string();
    for (expected, resolved) in config.expect.iter().zip(ports.iter()) {
        if expected != resolved {
            out = out.replace(&format!(":{}", expected), &format!(":{}", resolved));
        }
    }
    out
}

async fn wait_ready(process: &RenderedProcess, log_path: &Path, pid: Option<u32>) -> Result<()> {
    if let Some(delay) = process.ready_delay {
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }

    let has_check = process.ready_port.is_some()
        || process.ready_http.is_some()
        || process.ready_cmd.is_some()
        || process.ready_output.is_some();
    if !has_check {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if pid.is_some_and(|pid| !process_alive(pid)) {
            anyhow::bail!("process exited before becoming ready");
        }
        return Ok(());
    }

    let deadline = Instant::now()
        + Duration::from_secs(process.ready_timeout.unwrap_or(DEFAULT_READY_TIMEOUT_SECS));
    let output_regex = process
        .ready_output
        .as_ref()
        .map(|p| Regex::new(p))
        .transpose()
        .context("invalid ready_output regex")?;

    loop {
        if Instant::now() > deadline {
            anyhow::bail!("timed out waiting for readiness");
        }
        if pid.is_some_and(|pid| !process_alive(pid)) {
            anyhow::bail!("process exited before becoming ready");
        }

        let mut ready = true;
        if let Some(port) = process.ready_port {
            ready &= tcp_ready("127.0.0.1", port).await;
        }
        if let Some(url) = process.ready_http.as_ref() {
            ready &= http_ready(url).await;
        }
        if let Some(cmd) = process.ready_cmd.as_ref() {
            ready &= command_ready(cmd, &process.workdir, &process.env).await;
        }
        if let Some(regex) = output_regex.as_ref() {
            ready &= log_matches(log_path, regex)?;
        }
        if ready {
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn tcp_ready(host: &str, port: u16) -> bool {
    tokio::time::timeout(
        Duration::from_millis(500),
        tokio::net::TcpStream::connect((host, port)),
    )
    .await
    .is_ok_and(|r| r.is_ok())
}

async fn http_ready(url: &str) -> bool {
    let Some((host, port, path)) = parse_http_url(url) else {
        return false;
    };
    let addr = match (host.as_str(), port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
    {
        Some(addr) => addr,
        None => return false,
    };
    let mut stream = match tokio::time::timeout(
        Duration::from_millis(750),
        tokio::net::TcpStream::connect(addr),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        _ => return false,
    };
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, host
    );
    if tokio::io::AsyncWriteExt::write_all(&mut stream, req.as_bytes())
        .await
        .is_err()
    {
        return false;
    }
    let mut buf = [0u8; 32];
    match tokio::time::timeout(
        Duration::from_millis(750),
        tokio::io::AsyncReadExt::read(&mut stream, &mut buf),
    )
    .await
    {
        Ok(Ok(n)) if n > 0 => {
            let head = String::from_utf8_lossy(&buf[..n]);
            head.starts_with("HTTP/1.1 2") || head.starts_with("HTTP/1.0 2")
        }
        _ => false,
    }
}

fn parse_http_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (host_port, path) = rest
        .split_once('/')
        .map(|(h, p)| (h, format!("/{p}")))
        .unwrap_or((rest, "/".to_string()));
    let (host, port) = if let Some((h, p)) = host_port.rsplit_once(':') {
        (h.to_string(), p.parse::<u16>().ok()?)
    } else {
        (host_port.to_string(), 80)
    };
    Some((host, port, path))
}

async fn command_ready(cmd: &str, workdir: &Path, env: &IndexMap<String, String>) -> bool {
    let mut command = Command::new("sh");
    command.arg("-c").arg(cmd).current_dir(workdir);
    for (k, v) in env {
        command.env(k, v);
    }
    matches!(tokio::time::timeout(Duration::from_secs(5), command.status()).await, Ok(Ok(status)) if status.success())
}

fn log_matches(path: &Path, regex: &Regex) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let content = fs::read_to_string(path)?;
    Ok(regex.is_match(&content))
}

fn ordered_process_names(config: &Config, requested: &[String]) -> Result<Vec<String>> {
    let processes = config
        .processes
        .as_ref()
        .context("no processes configured")?;
    let requested_set: Option<HashSet<String>> = if requested.is_empty() {
        None
    } else {
        Some(requested.iter().cloned().collect())
    };

    let mut out = Vec::new();
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();

    fn visit(
        name: &str,
        processes: &IndexMap<String, ProcessDaemonConfig>,
        requested_set: &Option<HashSet<String>>,
        out: &mut Vec<String>,
        visiting: &mut HashSet<String>,
        visited: &mut HashSet<String>,
    ) -> Result<()> {
        if visited.contains(name) {
            return Ok(());
        }
        if !processes.contains_key(name) {
            anyhow::bail!("process '{}' not found in config", name);
        }
        if !visiting.insert(name.to_string()) {
            anyhow::bail!("cycle detected in process dependencies at '{}'", name);
        }
        for dep in &processes[name].depends {
            visit(dep, processes, requested_set, out, visiting, visited)?;
        }
        visiting.remove(name);
        visited.insert(name.to_string());
        if requested_set
            .as_ref()
            .map(|s| s.contains(name))
            .unwrap_or(true)
            || requested_set
                .as_ref()
                .is_some_and(|set| depends_on_requested(name, processes, set))
        {
            out.push(name.to_string());
        }
        Ok(())
    }

    if let Some(set) = requested_set.as_ref() {
        for name in set {
            visit(
                name,
                &processes.daemons,
                &requested_set,
                &mut out,
                &mut visiting,
                &mut visited,
            )?;
        }
    } else {
        for name in processes.daemons.keys() {
            visit(
                name,
                &processes.daemons,
                &requested_set,
                &mut out,
                &mut visiting,
                &mut visited,
            )?;
        }
    }
    out.dedup();
    Ok(out)
}

fn depends_on_requested(
    name: &str,
    processes: &IndexMap<String, ProcessDaemonConfig>,
    requested: &HashSet<String>,
) -> bool {
    requested
        .iter()
        .any(|target| dependency_chain_contains(target, name, processes))
}

fn dependency_chain_contains(
    target: &str,
    needle: &str,
    processes: &IndexMap<String, ProcessDaemonConfig>,
) -> bool {
    if target == needle {
        return true;
    }
    processes
        .get(target)
        .map(|p| {
            p.depends
                .iter()
                .any(|dep| dep == needle || dependency_chain_contains(dep, needle, processes))
        })
        .unwrap_or(false)
}

#[derive(Debug)]
struct RuntimePaths {
    project_root: PathBuf,
    workspace: String,
    processes_dir: PathBuf,
    logs_dir: PathBuf,
}

fn runtime_paths(config: &Config, project_dir: &Path, workspace: &str) -> Result<RuntimePaths> {
    let project_root = vcs::resolve_project_root(project_dir)
        .canonicalize()
        .unwrap_or_else(|_| vcs::resolve_project_root(project_dir));
    let workspace = config.get_normalized_workspace_name(workspace);
    let base = state_root()?
        .join(project_hash(&project_root))
        .join("workspaces")
        .join(&workspace);
    Ok(RuntimePaths {
        project_root,
        workspace,
        processes_dir: base.join("processes"),
        logs_dir: base.join("logs"),
    })
}

fn state_root() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("DEVFLOW_PROCESS_STATE_DIR") {
        let p = PathBuf::from(path);
        fs::create_dir_all(&p)?;
        return Ok(p);
    }
    let dir = dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
        .context("failed to resolve user state directory")?
        .join("devflow")
        .join("processes");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn project_hash(path: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.display().to_string().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn record_path(paths: &RuntimePaths, name: &str) -> PathBuf {
    paths
        .processes_dir
        .join(format!("{}.json", sanitize_component(name)))
}

fn read_record(paths: &RuntimePaths, name: &str) -> Result<Option<ProcessStateRecord>> {
    let path = record_path(paths, name);
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(path)?;
    Ok(Some(serde_json::from_str(&content)?))
}

fn write_record(paths: &RuntimePaths, record: &ProcessStateRecord) -> Result<()> {
    fs::create_dir_all(&paths.processes_dir)?;
    let path = record_path(paths, &record.process);
    let content = serde_json::to_string_pretty(record)?;
    fs::write(path, content)?;
    Ok(())
}

fn sanitize_component(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "process".to_string()
    } else {
        out.to_string()
    }
}

fn append_log_header(path: &Path, name: &str, command: &str) -> Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(
        file,
        "\n--- devflow process '{}' start {} ---\n{}\n",
        name,
        chrono::Utc::now().to_rfc3339(),
        command
    )?;
    Ok(())
}

fn read_tail(path: &Path, tail: Option<usize>) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    let file = fs::File::open(path)?;
    if let Some(tail) = tail {
        let lines: Vec<String> = BufReader::new(file)
            .lines()
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let start = lines.len().saturating_sub(tail);
        Ok(lines[start..].join("\n") + if lines.len() > start { "\n" } else { "" })
    } else {
        Ok(fs::read_to_string(path)?)
    }
}

fn daemon_required(config: &Config, name: &str) -> Option<bool> {
    config
        .processes
        .as_ref()
        .and_then(|p| p.daemons.get(name))
        .map(|d| d.required)
}

fn daemon_stop_timeout(config: &Config, name: &str) -> Duration {
    config
        .processes
        .as_ref()
        .and_then(|p| p.daemons.get(name))
        .and_then(|d| d.stop_timeout)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_STOP_TIMEOUT_SECS))
}

fn daemon_shutdown_signal(config: &Config, name: &str) -> Option<String> {
    config
        .processes
        .as_ref()
        .and_then(|p| p.daemons.get(name))
        .and_then(|d| d.shutdown_signal.clone())
}

fn daemon_watch_patterns(config: &Config, name: &str) -> Vec<String> {
    config
        .processes
        .as_ref()
        .and_then(|p| p.daemons.get(name))
        .map(|d| d.watch.clone())
        .unwrap_or_default()
}

fn process_urls(config: &Config, record: &ProcessStateRecord) -> Vec<String> {
    if record.ports.is_empty() {
        return Vec::new();
    }
    let process = dns_label(&record.process);
    let workspace = dns_label(&record.workspace);
    let project = dns_label(&config.project_name());
    let suffix = crate::config::GlobalConfig::load()
        .ok()
        .flatten()
        .and_then(|global| global.proxy.and_then(|proxy| proxy.domain_suffix))
        .unwrap_or_else(|| "local".to_string())
        .trim_start_matches('.')
        .to_ascii_lowercase();
    vec![format!(
        "https://{}.{}.{}.{}",
        process, workspace, project, suffix
    )]
}

fn dns_label(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "process".to_string()
    } else {
        trimmed.to_string()
    }
}

fn read_all_records(config: &Config, project_dir: &Path) -> Result<Vec<ProcessStateRecord>> {
    let paths = runtime_paths(config, project_dir, &config.git.main_workspace)?;
    let project_state_dir = state_root()?
        .join(project_hash(&paths.project_root))
        .join("workspaces");
    if !project_state_dir.exists() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for ws in fs::read_dir(project_state_dir)? {
        let ws = ws?;
        let process_dir = ws.path().join("processes");
        if !process_dir.exists() {
            continue;
        }
        for entry in fs::read_dir(process_dir)? {
            let entry = entry?;
            if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let content = fs::read_to_string(entry.path())?;
            records.push(serde_json::from_str(&content)?);
        }
    }
    Ok(records)
}

fn watch_signature(workdir: &Path, patterns: &[String]) -> Result<Option<String>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut latest: u128 = 0;
    let mut count = 0usize;
    for pattern in patterns {
        let pattern_path = PathBuf::from(pattern);
        let full_pattern = if pattern_path.is_absolute() {
            pattern_path
        } else {
            workdir.join(pattern_path)
        };
        let pattern_string = full_pattern.to_string_lossy().to_string();
        for entry in
            glob::glob(&pattern_string).with_context(|| format!("invalid watch glob: {pattern}"))?
        {
            let path = match entry {
                Ok(path) => path,
                Err(_) => continue,
            };
            if !path.is_file() {
                continue;
            }
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let duration = modified
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            latest = latest.max(duration.as_nanos());
            count += 1;
        }
    }
    Ok(Some(format!("{count}:{latest}")))
}

fn listening_pid_for_ports(ports: &[u16]) -> Option<u32> {
    ports.iter().find_map(|port| listening_pid_for_port(*port))
}

#[cfg(unix)]
fn listening_pid_for_port(port: u16) -> Option<u32> {
    let output = std::process::Command::new("lsof")
        .args(["-nP", &format!("-tiTCP:{port}"), "-sTCP:LISTEN"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().parse::<u32>().ok())
        .map(|pid| process_group_for_pid(pid).unwrap_or(pid))
}

#[cfg(unix)]
fn process_group_for_pid(pid: u32) -> Option<u32> {
    let output = std::process::Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[cfg(not(unix))]
fn listening_pid_for_port(_port: u16) -> Option<u32> {
    None
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    kill(Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    true
}

#[cfg(unix)]
async fn terminate_process(pid: u32, timeout: Duration, signal: Option<&str>) -> Result<()> {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    let pid = Pid::from_raw(pid as i32);
    let group = Pid::from_raw(-pid.as_raw());
    let signal = parse_shutdown_signal(signal).unwrap_or(Signal::SIGTERM);
    let _ = kill(group, signal).or_else(|_| kill(pid, signal));
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !process_alive(pid.as_raw() as u32) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = kill(group, Signal::SIGKILL).or_else(|_| kill(pid, Signal::SIGKILL));
    Ok(())
}

#[cfg(unix)]
fn parse_shutdown_signal(signal: Option<&str>) -> Option<nix::sys::signal::Signal> {
    use nix::sys::signal::Signal;
    match signal?
        .trim()
        .trim_start_matches("SIG")
        .to_ascii_uppercase()
        .as_str()
    {
        "TERM" => Some(Signal::SIGTERM),
        "INT" => Some(Signal::SIGINT),
        "HUP" => Some(Signal::SIGHUP),
        "QUIT" => Some(Signal::SIGQUIT),
        "KILL" => Some(Signal::SIGKILL),
        _ => None,
    }
}

#[cfg(not(unix))]
async fn terminate_process(_pid: u32, _timeout: Duration, _signal: Option<&str>) -> Result<()> {
    anyhow::bail!("process stop is not supported on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    static PROCESS_TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_config() -> Config {
        let yaml = r#"
name: myapp
processes:
  provider: pitchfork
  auto_start: true
  daemons:
    web:
      run: "echo ready; sleep 30"
      ready_output: "ready"
      env:
        DEVFLOW_TEST: "{{ workspace }}"
      port: { expect: [39001], bump: 20 }
    worker:
      run: "sleep 30"
      depends: [web]
"#;
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    #[test]
    fn parses_process_port_forms() {
        let single: ProcessPortConfig = serde_yaml_ng::from_str("3000").unwrap();
        assert_eq!(single.expect, vec![3000]);
        assert_eq!(single.bump.0, 0);

        let object: ProcessPortConfig =
            serde_yaml_ng::from_str("expect: [3000]\nbump: true").unwrap();
        assert_eq!(object.expect, vec![3000]);
        assert_eq!(object.bump.0, u32::MAX);
    }

    #[test]
    fn process_dependency_order_includes_dependencies_first() {
        let config = test_config();
        let order = ordered_process_names(&config, &["worker".to_string()]).unwrap();
        assert_eq!(order, vec!["web".to_string(), "worker".to_string()]);
    }

    #[test]
    fn runtime_provider_accepts_pitchfork_alias_and_rejects_unknown() {
        let mut config = test_config();
        config.processes.as_mut().unwrap().provider = "pitchfork".to_string();
        assert!(runtime_for_config(&config).is_ok());
        config.processes.as_mut().unwrap().provider = "unknown".to_string();
        assert!(runtime_for_config(&config).is_err());
    }

    #[test]
    fn pitchfork_run_script_redirects_output_for_devflow_readiness() {
        let rendered = RenderedProcess {
            command: "python manage.py runserver 127.0.0.1:$PORT".to_string(),
            workdir: PathBuf::from("/tmp/project"),
            env: IndexMap::new(),
            ports: vec![8000],
            ready_delay: None,
            ready_port: None,
            ready_http: None,
            ready_cmd: None,
            ready_output: None,
            ready_timeout: None,
        };
        let script = pitchfork_run_script(&rendered, Path::new("/tmp/log's/app.log"));
        assert_eq!(
            script,
            "exec >> '/tmp/log'\\''s/app.log' 2>&1; { python manage.py runserver 127.0.0.1:$PORT; }"
        );

        let with_output_readiness = RenderedProcess {
            ready_output: Some("ready".to_string()),
            ..rendered
        };
        assert_eq!(
            pitchfork_run_script(&with_output_readiness, Path::new("/tmp/app.log")),
            "exec >> '/tmp/app.log' 2>&1; { python manage.py runserver 127.0.0.1:$PORT; }"
        );
    }

    #[test]
    fn watch_signature_changes_when_watched_file_changes() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("src").join("main.ts");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "one").unwrap();
        let patterns = vec!["src/**/*.ts".to_string()];
        let first = watch_signature(dir.path(), &patterns).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(&file, "two").unwrap();
        let second = watch_signature(dir.path(), &patterns).unwrap();
        assert!(first.is_some());
        assert_ne!(first, second);
    }

    #[tokio::test]
    #[cfg(unix)]
    #[allow(clippy::await_holding_lock)]
    async fn daemon_reconcile_honors_desired_state() {
        let _guard = PROCESS_TEST_ENV_LOCK.lock().unwrap();
        let project = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        std::env::set_var("DEVFLOW_PROCESS_STATE_DIR", state.path());
        let yaml = r#"
name: desired-app
processes:
  daemons:
    web:
      run: "echo desired-ready; sleep 30"
      ready_output: "desired-ready"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let paths = runtime_paths(&config, project.path(), "feature/desired").unwrap();
        let log_path = paths.logs_dir.join("web.log");
        let record = ProcessStateRecord {
            process: "web".to_string(),
            workspace: paths.workspace.clone(),
            project_key: paths.project_root.display().to_string(),
            project_name: config.project_name(),
            pid: None,
            command: "echo desired-ready; sleep 30".to_string(),
            workdir: project.path().display().to_string(),
            log_path: log_path.display().to_string(),
            ports: Vec::new(),
            status: "stopped".to_string(),
            desired_state: Some("running".to_string()),
            runtime: Some("native".to_string()),
            pitchfork_id: None,
            required: true,
            started_at: chrono::Utc::now().to_rfc3339(),
            watch_signature: None,
            retry_count: 0,
            last_error: None,
        };
        write_record(&paths, &record).unwrap();

        let actions = reconcile_project_processes(&config, project.path())
            .await
            .unwrap();
        assert!(actions
            .iter()
            .any(|a| a.action == "desired-start" && a.success));
        let statuses =
            list_workspace_processes(&config, project.path(), Some("feature/desired")).unwrap();
        let web = statuses.iter().find(|s| s.process == "web").unwrap();
        assert!(web.pid.is_some());

        let mut record = read_record(&paths, "web").unwrap().unwrap();
        record.desired_state = Some("stopped".to_string());
        write_record(&paths, &record).unwrap();
        let actions = reconcile_project_processes(&config, project.path())
            .await
            .unwrap();
        assert!(actions
            .iter()
            .any(|a| a.action == "desired-stop" && a.success));
        let statuses =
            list_workspace_processes(&config, project.path(), Some("feature/desired")).unwrap();
        let web = statuses.iter().find(|s| s.process == "web").unwrap();
        assert_eq!(web.status, "stopped");
        std::env::remove_var("DEVFLOW_PROCESS_STATE_DIR");
    }

    #[tokio::test]
    #[cfg(unix)]
    #[allow(clippy::await_holding_lock)]
    async fn pitchfork_readiness_timeout_is_bounded() {
        let _guard = PROCESS_TEST_ENV_LOCK.lock().unwrap();
        let project = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        std::env::set_var("DEVFLOW_PROCESS_STATE_DIR", state.path());
        let unique = format!("devflow-pitchfork-timeout-{}", std::process::id());
        let yaml = format!(
            r#"
name: myapp
processes:
  provider: pitchfork
  daemons:
    slow:
      run: "while true; do sleep 1; done # {}"
      ready_output: "READY_NEVER"
      ready_timeout: 1
"#,
            unique
        );
        let config: Config = serde_yaml_ng::from_str(&yaml).unwrap();

        let results = tokio::time::timeout(
            Duration::from_secs(5),
            start_workspace_processes(
                &config,
                project.path(),
                "feature/timeout",
                &["slow".to_string()],
                true,
            ),
        )
        .await
        .expect("pitchfork readiness must be bounded")
        .unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].success, "{:?}", results[0]);
        assert!(
            results[0].message.contains("readiness failed"),
            "{:?}",
            results[0]
        );

        let _ = stop_workspace_processes(
            &config,
            project.path(),
            "feature/timeout",
            &["slow".to_string()],
        )
        .await;
        std::env::remove_var("DEVFLOW_PROCESS_STATE_DIR");
    }

    #[tokio::test]
    #[cfg(unix)]
    #[allow(clippy::await_holding_lock)]
    async fn start_status_logs_and_stop_process() {
        let _guard = PROCESS_TEST_ENV_LOCK.lock().unwrap();
        let project = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        std::env::set_var("DEVFLOW_PROCESS_STATE_DIR", state.path());
        let mut config = test_config();
        config.worktree = None;

        let results = start_workspace_processes(
            &config,
            project.path(),
            "feature/test",
            &["web".to_string()],
            true,
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].success, "{:?}", results[0]);
        assert!(results[0].pid.is_some());

        let statuses =
            list_workspace_processes(&config, project.path(), Some("feature/test")).unwrap();
        assert_eq!(statuses.len(), 2);
        let web_status = statuses.iter().find(|s| s.process == "web").unwrap();
        assert!(matches!(web_status.status.as_str(), "ready" | "running"));
        let worker_status = statuses.iter().find(|s| s.process == "worker").unwrap();
        assert_eq!(worker_status.status, "not_started");

        let logs = process_logs(&config, project.path(), "feature/test", "web", Some(20)).unwrap();
        assert!(logs.contains("ready"));

        let stop = stop_workspace_processes(
            &config,
            project.path(),
            "feature/test",
            &["web".to_string()],
        )
        .await
        .unwrap();
        assert!(stop[0].success);

        let mut occupied_guard = None;
        for port in 39600..39700u16 {
            if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) {
                if std::net::TcpListener::bind(("127.0.0.1", port + 1)).is_ok() {
                    occupied_guard = Some((port, listener));
                    break;
                }
            }
        }
        let (occupied_port, _guard) = occupied_guard.expect("need two consecutive test ports");
        let bumped_port = occupied_port + 1;
        let bump_yaml = format!(
            r#"
name: myapp
processes:
  daemons:
    api:
      run: "echo port=$PORT; sleep 30"
      port: {{ expect: [{}], bump: 5 }}
"#,
            occupied_port
        );
        let bump_config: Config = serde_yaml_ng::from_str(&bump_yaml).unwrap();
        let bump = start_workspace_processes(
            &bump_config,
            project.path(),
            "feature/bump",
            &["api".to_string()],
            true,
        )
        .await
        .unwrap();
        assert!(bump[0].success, "{:?}", bump[0]);
        assert_eq!(bump[0].ports, vec![bumped_port]);
        let bump_logs = process_logs(
            &bump_config,
            project.path(),
            "feature/bump",
            "api",
            Some(20),
        )
        .unwrap();
        assert!(bump_logs.contains(&format!("port={}", bumped_port)));
        let bump_status =
            list_workspace_processes(&bump_config, project.path(), Some("feature/bump")).unwrap();
        assert_eq!(
            bump_status[0].urls,
            vec!["https://api.feature-bump.myapp.local"]
        );
        let _ = stop_workspace_processes(
            &bump_config,
            project.path(),
            "feature/bump",
            &["api".to_string()],
        )
        .await;

        let optional_yaml = r#"
name: myapp
processes:
  daemons:
    optional:
      run: "sleep 30"
      required: false
      ready_output: "this-will-not-appear"
      ready_timeout: 1
"#;
        let optional_config: Config = serde_yaml_ng::from_str(optional_yaml).unwrap();
        let optional = start_workspace_processes(
            &optional_config,
            project.path(),
            "feature/optional",
            &["optional".to_string()],
            true,
        )
        .await
        .unwrap();
        assert!(!optional[0].success);
        assert!(!optional[0].required);
        let optional_status =
            list_workspace_processes(&optional_config, project.path(), Some("feature/optional"))
                .unwrap();
        assert!(optional_status[0].last_error.is_some());
        let _ = stop_workspace_processes(
            &optional_config,
            project.path(),
            "feature/optional",
            &["optional".to_string()],
        )
        .await;

        cleanup_workspace_process_state(&config, project.path(), "feature/test").unwrap();
        let statuses =
            list_workspace_processes(&config, project.path(), Some("feature/test")).unwrap();
        assert!(statuses.iter().all(|s| s.pid.is_none()));
        assert!(statuses.iter().all(|s| s.status == "not_started"));
        std::env::remove_var("DEVFLOW_PROCESS_STATE_DIR");
    }
}

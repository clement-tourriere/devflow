#[cfg(any(feature = "service-local", test))]
pub(crate) mod copy;
pub mod factory;
pub mod orphan;
pub mod ownership;
pub mod plugin;
pub mod postgres;

#[cfg(feature = "service-local")]
pub mod clickhouse;
#[cfg(feature = "service-local")]
pub mod generic;
#[cfg(feature = "service-local")]
pub mod local_docker;
#[cfg(feature = "service-local")]
pub mod local_engine;
#[cfg(feature = "service-local")]
pub mod mysql;
#[cfg(feature = "service-local")]
pub mod shared;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Metadata about a service workspace instance (e.g. a per-branch database).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub name: String,
    pub created_at: Option<DateTime<Utc>>,
    pub parent_workspace: Option<String>,
    pub database_name: String,
    #[serde(default)]
    pub state: Option<String>,
}

/// Connection details for reaching a service workspace (host, port, credentials).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionInfo {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: Option<String>,
    pub connection_string: Option<String>,
}

/// High-level metadata about a service project (name, storage driver, image).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub name: String,
    pub storage_driver: Option<String>,
    pub image: Option<String>,
}

/// Capability matrix for a service provider, used by CLI/TUI to decide
/// which operations to offer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceCapabilities {
    /// Data behavior when creating a child: empty, physical_copy, template_copy, remote_clone.
    #[serde(default)]
    pub data_strategy: String,
    pub lifecycle: bool,
    pub logs: bool,
    pub destroy_project: bool,
    pub cleanup: bool,
    pub seed_from_source: bool,
    pub operations: OperationCapabilities,
    pub max_workspace_name_length: usize,
}

/// Diagnostic report produced by `ServiceProvider::doctor()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
}

/// A single check within a `DoctorReport`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub name: String,
    pub available: bool,
    pub detail: String,
}

/// ServiceProvider is the core trait for all service providers.
/// (Renamed from DatabaseBranchingBackend as part of the devflow evolution.)
#[async_trait]
pub trait ServiceProvider: Send + Sync {
    // Core branching operations
    async fn create_workspace(
        &self,
        workspace_name: &str,
        from_workspace: Option<&str>,
    ) -> Result<WorkspaceInfo>;
    async fn delete_workspace(&self, workspace_name: &str) -> Result<()>;
    async fn list_workspaces(&self) -> Result<Vec<WorkspaceInfo>>;
    async fn workspace_exists(&self, workspace_name: &str) -> Result<bool>;
    async fn switch_to_branch(&self, workspace_name: &str) -> Result<WorkspaceInfo>;

    // Connection information
    async fn get_connection_info(&self, workspace_name: &str) -> Result<ConnectionInfo>;

    // Backend-specific capabilities
    fn supports_cleanup(&self) -> bool {
        true
    }
    fn max_workspace_name_length(&self) -> usize {
        63
    }

    fn operations(&self) -> OperationCapabilities {
        OperationCapabilities::default()
    }

    async fn start_workspace(&self, _workspace_name: &str) -> Result<()> {
        Err(UnsupportedOperation::new(self.provider_name(), ServiceOperation::Start).into())
    }
    async fn stop_workspace(&self, _workspace_name: &str) -> Result<()> {
        Err(UnsupportedOperation::new(self.provider_name(), ServiceOperation::Stop).into())
    }
    async fn reset_workspace(&self, _workspace_name: &str) -> Result<()> {
        Err(UnsupportedOperation::new(self.provider_name(), ServiceOperation::Reset).into())
    }
    /// Compatibility summary; callers must use the capability for each action.
    fn supports_lifecycle(&self) -> bool {
        self.operations().start && self.operations().stop
    }

    // Project destruction (local provider)
    fn supports_destroy(&self) -> bool {
        false
    }
    async fn destroy_preview(&self) -> Result<Option<(String, Vec<String>)>> {
        Ok(None)
    }
    async fn destroy_project(&self) -> Result<Vec<String>> {
        anyhow::bail!("This provider does not support project destruction")
    }

    // Data seeding
    async fn seed_from_source(&self, _workspace_name: &str, _source: &str) -> Result<()> {
        anyhow::bail!("This provider does not support seeding from external sources")
    }

    /// Seal a dedicated data generation and return its immutable engine identity.
    async fn seal_baseline(&self, _workspace: &str) -> Result<String> {
        Err(UnsupportedOperation::new(self.provider_name(), ServiceOperation::Snapshot).into())
    }
    async fn baseline_fingerprint(&self, _workspace: &str) -> Result<String> {
        Err(UnsupportedOperation::new(self.provider_name(), ServiceOperation::Snapshot).into())
    }
    async fn delete_baseline(&self, _workspace: &str) -> Result<()> {
        Err(UnsupportedOperation::new(self.provider_name(), ServiceOperation::Snapshot).into())
    }

    // Diagnostics
    async fn doctor(&self) -> Result<DoctorReport>;

    // Test connection
    /// Observe connectivity without creating or starting an engine.
    async fn test_connection(&self) -> Result<()>;
    /// Explicit engine reconciliation for controllers and `service up`.
    async fn ensure_engine_ready(&self) -> Result<()> {
        self.test_connection().await
    }

    // Project metadata (optional, implemented by local provider)
    fn project_info(&self) -> Option<ProjectInfo> {
        None
    }

    // Container logs (for Docker-based providers)
    async fn logs(&self, _workspace_name: &str, _tail: Option<usize>) -> Result<String> {
        anyhow::bail!("This provider does not support logs (not a local Docker provider)")
    }

    // Get provider display name
    fn provider_name(&self) -> &'static str;

    fn data_strategy(&self) -> &'static str {
        "empty"
    }

    // Machine-readable capability summary for UI/automation.
    fn capabilities(&self) -> ServiceCapabilities {
        ServiceCapabilities {
            data_strategy: self.data_strategy().into(),
            lifecycle: self.supports_lifecycle(),
            logs: self.operations().logs,
            destroy_project: self.supports_destroy(),
            cleanup: self.supports_cleanup(),
            seed_from_source: self.operations().seed,
            operations: self.operations(),
            max_workspace_name_length: self.max_workspace_name_length(),
        }
    }
}

/// Clone data with native block cloning and an atomic directory replacement.
#[cfg(feature = "service-local")]
pub async fn clone_data_dir(source: &std::path::Path, target: &std::path::Path) -> Result<()> {
    copy::replace_directory(target, |staged| async move {
        let source = source.to_path_buf();
        tokio::task::spawn_blocking(move || {
            crate::fs_copy::copy_directory(&source, &staged, crate::fs_copy::CopyMode::Cow)
        })
        .await?
    })
    .await
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServiceOperation {
    Start,
    Stop,
    Reset,
    Seed,
    Logs,
    Snapshot,
}

impl std::fmt::Display for ServiceOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Start => "start",
                Self::Stop => "stop",
                Self::Reset => "reset",
                Self::Seed => "seed",
                Self::Logs => "logs",
                Self::Snapshot => "snapshot",
            }
        )
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct OperationCapabilities {
    pub start: bool,
    pub stop: bool,
    pub reset: bool,
    pub seed: bool,
    pub logs: bool,
    pub snapshot: bool,
}
impl OperationCapabilities {
    pub fn supports(&self, op: ServiceOperation) -> bool {
        match op {
            ServiceOperation::Start => self.start,
            ServiceOperation::Stop => self.stop,
            ServiceOperation::Reset => self.reset,
            ServiceOperation::Seed => self.seed,
            ServiceOperation::Logs => self.logs,
            ServiceOperation::Snapshot => self.snapshot,
        }
    }
}

#[derive(Debug)]
pub struct UnsupportedOperation {
    pub provider: String,
    pub operation: ServiceOperation,
}
impl UnsupportedOperation {
    pub fn new(provider: &str, operation: ServiceOperation) -> Self {
        Self {
            provider: provider.into(),
            operation,
        }
    }
}
impl std::fmt::Display for UnsupportedOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} does not support {}", self.provider, self.operation)
    }
}
impl std::error::Error for UnsupportedOperation {}

pub mod descriptor;

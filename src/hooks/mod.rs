pub mod approval;
pub mod executor;
pub mod template;

// Re-export hook engine types
pub use executor::HookEngine;
#[allow(unused_imports)] // Public API — used by consumers for advanced template rendering
pub use template::TemplateEngine;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

pub use indexmap::IndexMap;

/// Lifecycle phase at which a hook fires.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookPhase {
    // VCS / worktree lifecycle
    PreSwitch,
    PostCreate,
    PostStart,
    PostSwitch,
    PreRemove,
    PostRemove,

    // Merge lifecycle
    PreCommit,
    PreMerge,
    PostMerge,
    PostRewrite,

    // Service lifecycle
    PreServiceCreate,
    PostServiceCreate,
    PreServiceDelete,
    PostServiceDelete,
    PostServiceSwitch,

    // Custom user-defined phase
    #[serde(untagged)]
    Custom(String),
}

impl fmt::Display for HookPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HookPhase::PreSwitch => write!(f, "pre-switch"),
            HookPhase::PostCreate => write!(f, "post-create"),
            HookPhase::PostStart => write!(f, "post-start"),
            HookPhase::PostSwitch => write!(f, "post-switch"),
            HookPhase::PreRemove => write!(f, "pre-remove"),
            HookPhase::PostRemove => write!(f, "post-remove"),
            HookPhase::PreCommit => write!(f, "pre-commit"),
            HookPhase::PreMerge => write!(f, "pre-merge"),
            HookPhase::PostMerge => write!(f, "post-merge"),
            HookPhase::PostRewrite => write!(f, "post-rewrite"),
            HookPhase::PreServiceCreate => write!(f, "pre-service-create"),
            HookPhase::PostServiceCreate => write!(f, "post-service-create"),
            HookPhase::PreServiceDelete => write!(f, "pre-service-delete"),
            HookPhase::PostServiceDelete => write!(f, "post-service-delete"),
            HookPhase::PostServiceSwitch => write!(f, "post-service-switch"),
            HookPhase::Custom(name) => write!(f, "{}", name),
        }
    }
}

impl HookPhase {
    /// Whether this phase blocks the caller (true) or runs in the background (false).
    pub fn is_blocking(&self) -> bool {
        matches!(
            self,
            HookPhase::PreSwitch
                | HookPhase::PostCreate
                | HookPhase::PreRemove
                | HookPhase::PreCommit
                | HookPhase::PreMerge
                | HookPhase::PreServiceCreate
                | HookPhase::PreServiceDelete
        )
    }
}

impl FromStr for HookPhase {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "pre-switch" => HookPhase::PreSwitch,
            "post-create" => HookPhase::PostCreate,
            "post-start" => HookPhase::PostStart,
            "post-switch" => HookPhase::PostSwitch,
            "pre-remove" => HookPhase::PreRemove,
            "post-remove" => HookPhase::PostRemove,
            "pre-commit" => HookPhase::PreCommit,
            "pre-merge" => HookPhase::PreMerge,
            "post-merge" => HookPhase::PostMerge,
            "post-rewrite" => HookPhase::PostRewrite,
            "pre-service-create" => HookPhase::PreServiceCreate,
            "post-service-create" => HookPhase::PostServiceCreate,
            "pre-service-delete" => HookPhase::PreServiceDelete,
            "post-service-delete" => HookPhase::PostServiceDelete,
            "post-service-switch" => HookPhase::PostServiceSwitch,
            other => HookPhase::Custom(other.to_string()),
        })
    }
}

/// A single named hook within a phase.
///
/// In the config YAML this is one entry in the phase map:
/// ```yaml
/// hooks:
///   post-create:
///     install: "npm ci"
///     env: |
///       cat > .env.local << EOF
///       DATABASE_URL={{ service.app-db.url }}
///       EOF
/// ```
/// Each key-value under a phase becomes a `HookEntry`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HookEntry {
    /// Simple command string (the value is a template-rendered shell command)
    Simple(String),
    /// Extended hook with extra options
    Extended(ExtendedHookEntry),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtendedHookEntry {
    /// The command to execute (MiniJinja template)
    pub command: String,
    /// Working directory (relative to project root)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// Whether to continue on error (default: false for blocking, true for background)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_on_error: Option<bool>,
    /// Condition expression (e.g. "file_exists:.env", "always", "never")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// Extra environment variables (values are MiniJinja templates)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<HashMap<String, String>>,
    /// Run in the background even if the phase is normally blocking
    #[serde(default)]
    pub background: bool,
}

/// Configuration for all hooks, keyed by phase.
///
/// Each phase maps hook-name → HookEntry.
/// Example YAML representation:
/// ```yaml
/// hooks:
///   post-create:
///     install: "npm ci"
///   post-switch:
///     env: "cat > .env.local ..."
/// ```
pub type HooksConfig = IndexMap<HookPhase, IndexMap<String, HookEntry>>;

/// Context variables available to hook templates.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HookContext {
    /// Current branch name
    pub branch: String,
    /// Repository directory name
    pub repo: String,
    /// Worktree path (if in a worktree)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    /// Default branch (main/master)
    pub default_branch: String,
    /// HEAD commit SHA
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Target branch (for merge hooks)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Base branch (for creation hooks)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Service connection info, keyed by service name.
    /// Each service exposes: host, port, database, user, password, url
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub service: HashMap<String, ServiceContext>,
}

/// Connection information for a single service, exposed to templates.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ServiceContext {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Full connection URL
    pub url: String,
}

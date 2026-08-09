use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

mod loading;

pub use loading::workspace_service_key;
pub(crate) use loading::{explicit_main_workspace_for_dir, legacy_normalize_workspace_name};

/// Default AI tool configuration directories to copy into new worktrees.
pub const AI_TOOL_DIRS: &[&str] = &[".claude", ".cursor", ".opencode", ".agents"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Project name (derived from `devflow init <name>` or the directory name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Preferred VCS for this project ("git" or "jj").
    /// Overrides the global `default_vcs` when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_vcs: Option<crate::vcs::VcsKind>,
    #[serde(default)]
    pub git: GitConfig,
    #[serde(default)]
    pub behavior: BehaviorConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<NamedServiceConfig>>,
    /// Worktree materialization settings. Every non-default devflow workspace
    /// is materialized as a Git worktree or jj workspace.
    #[serde(default)]
    pub worktree: WorktreeConfig,
    /// New hook engine configuration (Phase 2).
    /// Maps hook phase names to named hook entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<crate::hooks::HooksConfig>,
    /// VCS event → devflow phase trigger mapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triggers: Option<crate::hooks::triggers::TriggersConfig>,
    /// Workspace-scoped project processes (web servers, workers, schedulers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processes: Option<crate::processes::ProcessesConfig>,
    /// AI agent integration configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentConfig>,
    /// Commit message generation configuration (LLM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<CommitConfig>,
    /// Execute configuration (detach command template, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execute: Option<ExecuteConfig>,
    /// Directory the config file was loaded from (set by `from_file`).
    ///
    /// Long-lived frontends (GUI, daemon) run with an arbitrary process cwd,
    /// so anything identity-related (project name, project path for orphan
    /// detection) must derive from this instead of `std::env::current_dir()`.
    #[serde(skip)]
    pub project_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamedServiceConfig {
    pub name: String,
    #[serde(rename = "type", default = "default_provider_type")]
    pub provider_type: String,
    /// Service type: postgres, clickhouse, mysql, generic (default: postgres)
    #[serde(
        default = "default_service_type",
        skip_serializing_if = "is_default_service_type"
    )]
    pub service_type: String,
    /// Whether to automatically workspace this service when git workspaces are created
    #[serde(
        default = "default_auto_branch",
        alias = "auto_branch",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub auto_workspace: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<LocalServiceConfig>,
    /// Configuration for `type: shared` — logical isolation inside one global
    /// container (e.g. CREATE DATABASE per workspace) instead of one container
    /// per workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared: Option<SharedServiceConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub neon: Option<NeonConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dblab: Option<DBLabConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "xata_lite")]
    pub xata: Option<XataConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clickhouse: Option<ClickHouseConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mysql: Option<MySQLConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generic: Option<GenericDockerConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<PluginConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker: Option<DockerCustomSettings>,
}

/// Where a resolved service definition came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceSource {
    /// Committed project config (`.devflow.yml` / `devflow.toml`).
    Config,
    /// CLI-managed local state (`devflow service add` etc.).
    LocalState,
}

impl ServiceSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            ServiceSource::Config => "config",
            ServiceSource::LocalState => "local_state",
        }
    }
}

/// A service definition tagged with its origin.
#[derive(Debug, Clone)]
pub struct ServiceWithSource {
    pub service: NamedServiceConfig,
    pub source: ServiceSource,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DockerCustomSettings {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub environment: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_policy: Option<String>,
}

impl DockerCustomSettings {
    pub fn is_empty(&self) -> bool {
        self.command.is_empty() && self.environment.is_empty() && self.restart_policy.is_none()
    }
}

fn default_provider_type() -> String {
    "local".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalServiceConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_range_start: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postgres_user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postgres_password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postgres_db: Option<String>,
}

/// Configuration for a shared (global-container, logically-isolated) service.
///
/// One container is kept running per engine and each workspace gets a logical
/// boundary inside it (a database for postgres, a bucket for object storage,
/// a DB index for redis) provisioned on the fly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SharedServiceConfig {
    /// Container image. Defaults per engine (e.g. `postgres:17`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Fixed host port for the global container (e.g. 5432 for postgres).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Override the global container name (default: `devflow-shared-<engine>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_name: Option<String>,
    /// Admin user (postgres). Default: `postgres`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Admin password (postgres). Default: `postgres`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Use `CREATE DATABASE ... TEMPLATE parent` for branch-from-parent
    /// semantics (postgres). Default: true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_branching: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeonConfig {
    pub api_key: String,
    pub project_id: String,
    #[serde(default = "default_neon_base_url")]
    pub base_url: String,
}

fn default_neon_base_url() -> String {
    "https://console.neon.tech/api/v2".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DBLabConfig {
    pub api_url: String,
    pub auth_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XataConfig {
    pub api_key: String,
    pub organization_id: String,
    pub project_id: String,
    #[serde(default = "default_xata_base_url")]
    pub base_url: String,
}

fn default_xata_base_url() -> String {
    "https://api.xata.tech".to_string()
}

pub fn default_service_type() -> String {
    "postgres".to_string()
}

fn is_default_service_type(s: &String) -> bool {
    s == "postgres"
}

pub fn default_auto_branch() -> bool {
    true
}

/// Configuration for a ClickHouse local provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClickHouseConfig {
    /// Docker image (default: clickhouse/clickhouse-server:latest)
    #[serde(default = "default_clickhouse_image")]
    pub image: String,
    /// Start of port range for workspace-specific instances
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_range_start: Option<u16>,
    /// Data root directory for persistent storage
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_root: Option<String>,
    /// Default ClickHouse user
    #[serde(default = "default_clickhouse_user")]
    pub user: String,
    /// Default ClickHouse password
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

fn default_clickhouse_image() -> String {
    "clickhouse/clickhouse-server:latest".to_string()
}

fn default_clickhouse_user() -> String {
    "default".to_string()
}

/// Configuration for a MySQL/MariaDB local provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MySQLConfig {
    /// Docker image (default: mysql:8)
    #[serde(default = "default_mysql_image")]
    pub image: String,
    /// Start of port range for workspace-specific instances
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_range_start: Option<u16>,
    /// Data root directory for persistent storage
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_root: Option<String>,
    /// Root password for MySQL
    #[serde(default = "default_mysql_root_password")]
    pub root_password: String,
    /// Default database name
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Default user
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Default user password
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

fn default_mysql_image() -> String {
    "mysql:8".to_string()
}

fn default_mysql_root_password() -> String {
    "dev".to_string()
}

/// Configuration for a plugin-based service provider.
///
/// Plugin providers delegate all operations to an external executable that
/// communicates over JSON on stdin/stdout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginConfig {
    /// Path to the plugin executable (absolute or relative to project root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Plugin name — resolved as `devflow-plugin-{name}` on PATH.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Timeout in seconds for each plugin invocation (default: 30).
    #[serde(default = "default_plugin_timeout")]
    pub timeout: u64,
    /// Opaque configuration passed to the plugin as JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

fn default_plugin_timeout() -> u64 {
    30
}

/// Configuration for a generic Docker service provider.
///
/// Generic services run arbitrary Docker images and can optionally be "branched"
/// by creating isolated containers per workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenericDockerConfig {
    /// Docker image to run
    pub image: String,
    /// Port mapping in Docker format (e.g. "6379:6379")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_mapping: Option<String>,
    /// Start of port range for workspace-specific instances
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_range_start: Option<u16>,
    /// Environment variables to pass to the container
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub environment: HashMap<String, String>,
    /// Docker volumes to mount (host:container format)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
    /// Custom command to run (overrides image CMD)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Health check command
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<String>,
}

/// Configuration for AI agent integration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentConfig {
    /// Automatically provide project context to the agent on launch.
    #[serde(default = "default_true")]
    pub auto_context: bool,
}

/// Configuration for command execution in workspaces.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExecuteConfig {
    /// Template for detached execution. Placeholders: {session}, {dir}, {cmd}
    /// Default (when tmux available): "tmux new-session -d -s {session} -c {dir} {cmd}"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detach_command: Option<String>,
    /// Preferred multiplexer: "tmux" or "zellij". Auto-detected if not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiplexer: Option<String>,
}

/// Configuration for commit message generation.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommitConfig {
    /// Commit generation settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<CommitGenerationConfig>,
}

/// LLM configuration for generating commit messages.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommitGenerationConfig {
    /// External CLI command to pipe prompts to (e.g., "claude -p --model=haiku").
    /// Takes precedence over the built-in API approach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// OpenAI-compatible API key (fallback when no command is set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// OpenAI-compatible API URL (fallback when no command is set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
    /// Model name (fallback when no command is set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorktreeConfig {
    /// Removed setting: worktrees are always enabled now. Accepted (and
    /// ignored) so configs written by older releases keep parsing; a
    /// deprecation warning is logged in `Config::from_file`.
    #[serde(default, skip_serializing)]
    pub enabled: Option<bool>,
    /// Path template for new worktrees.
    /// Supports `{repo}` and `{workspace}` placeholders. `{workspace}` is the
    /// collision-safe service key; raw VCS names remain user-facing metadata.
    /// Default: `"../{repo}.{workspace}"`
    #[serde(default = "default_worktree_path_template")]
    pub path_template: String,
    /// Files to copy from the main worktree into each new worktree.
    /// Default: `[".env", ".env.local"]` — must match `Default::default()` so a
    /// partial `worktree:` section behaves like an absent one.
    #[serde(default = "default_copy_files")]
    pub copy_files: Vec<String>,
    /// Also copy gitignored dependency/cache directories (e.g. `.venv/`,
    /// `node_modules/`, `target/`). Environment files should usually be listed
    /// explicitly in `copy_files` instead, so the recommended default is false.
    #[serde(default)]
    pub copy_ignored: bool,
    /// Exclude gitignored files from worktrees (both CoW and non-CoW paths).
    /// Default: `true` — saves disk space by removing dirs like `node_modules/`, `target/`.
    #[serde(default = "default_respect_gitignore")]
    pub respect_gitignore: bool,
    /// Whether to copy AI tool config directories (`.claude`, `.cursor`, etc.)
    /// into new worktrees. Default: `true`.
    #[serde(default = "default_true")]
    pub copy_ai_configs: bool,
    /// Additional AI tool directories to copy (beyond the built-in list).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_ai_dirs: Vec<String>,
}

impl WorktreeConfig {
    /// Recommended default worktree configuration for new projects.
    /// Kept as a named constructor for callers that want to be explicit.
    pub fn recommended_default() -> Self {
        Self::default()
    }
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            enabled: None,
            path_template: default_worktree_path_template(),
            copy_files: default_copy_files(),
            copy_ignored: false,
            respect_gitignore: true,
            copy_ai_configs: true,
            extra_ai_dirs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitConfig {
    #[serde(default = "default_true", alias = "auto_create_on_branch")]
    pub auto_create_on_workspace: bool,
    #[serde(default = "default_main_workspace", alias = "main_branch")]
    pub main_workspace: String,
    /// Hook-adopted worktrees only auto-provision services when the raw
    /// branch name matches this (unanchored) regex. Explicit commands
    /// (`devflow switch`, `devflow service create`) are never filtered.
    #[serde(
        default,
        alias = "branch_filter_regex",
        alias = "auto_create_workspace_filter",
        alias = "auto_create_branch_filter",
        skip_serializing_if = "Option::is_none"
    )]
    pub workspace_filter_regex: Option<String>,
    /// Branch names that never auto-provision. Exact names or `*` globs.
    #[serde(default = "default_exclude_workspaces", alias = "exclude_branches")]
    pub exclude_workspaces: Vec<String>,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            auto_create_on_workspace: true,
            main_workspace: "main".to_string(),
            workspace_filter_regex: None,
            exclude_workspaces: vec!["main".to_string(), "master".to_string()],
        }
    }
}

fn default_exclude_workspaces() -> Vec<String> {
    vec!["main".to_string(), "master".to_string()]
}

fn default_true() -> bool {
    true
}

fn default_main_workspace() -> String {
    "main".to_string()
}

fn default_worktree_path_template() -> String {
    "../{repo}.{workspace}".to_string()
}

fn default_copy_files() -> Vec<String> {
    vec![".env".to_string(), ".env.local".to_string()]
}

fn default_respect_gitignore() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehaviorConfig {
    #[serde(
        default,
        alias = "max_branches",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_workspaces: Option<usize>,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            max_workspaces: Some(10),
        }
    }
}

// Local configuration that can override the main config
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LocalConfig {
    pub git: Option<LocalGitConfig>,
    pub behavior: Option<LocalBehaviorConfig>,
    pub disabled: Option<bool>,
    pub disabled_workspaces: Option<Vec<String>>,
    pub worktree: Option<WorktreeConfig>,
    /// Override the project-level `default_vcs` locally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_vcs: Option<crate::vcs::VcsKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LocalGitConfig {
    #[serde(alias = "auto_create_on_branch")]
    pub auto_create_on_workspace: Option<bool>,
    #[serde(alias = "main_branch")]
    pub main_workspace: Option<String>,
    #[serde(
        alias = "branch_filter_regex",
        alias = "auto_create_workspace_filter",
        alias = "auto_create_branch_filter"
    )]
    pub workspace_filter_regex: Option<String>,
    #[serde(alias = "exclude_branches")]
    pub exclude_workspaces: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LocalBehaviorConfig {
    #[serde(alias = "max_branches")]
    pub max_workspaces: Option<usize>,
}

// Environment variable configuration
#[derive(Debug, Clone, Default)]
pub struct EnvConfig {
    pub disabled: Option<bool>,
    pub skip_hooks: Option<bool>,
    pub auto_create: Option<bool>,
    pub workspace_filter_regex: Option<String>,
    pub disabled_workspaces: Option<Vec<String>>,
    pub current_workspace_disabled: Option<bool>,
}

/// Global user-level configuration, stored at `~/.config/devflow/config.yml`.
///
/// This is the lowest-priority layer — project and local configs override it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalConfig {
    /// Default VCS for new projects ("git" or "jj").
    /// Used by `devflow init` when auto-initializing a VCS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_vcs: Option<crate::vcs::VcsKind>,
    /// Global proxy configuration (used by `devflow proxy start`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyGlobalConfig>,
}

/// Global proxy settings stored in `~/.config/devflow/config.yml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProxyGlobalConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain_suffix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub https_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_port: Option<u16>,
    /// Advertise friendly `.local` names via mDNS so they resolve from the host
    /// (macOS only). Default: `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mdns: Option<bool>,
}

impl GlobalConfig {
    /// Load the global config from `~/.config/devflow/config.yml`.
    /// Returns `None` if the file does not exist.
    pub fn load() -> Result<Option<Self>> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read global config: {}", path.display()))?;
        let global: GlobalConfig = serde_yaml_ng::from_str(&content)
            .with_context(|| format!("Failed to parse global config: {}", path.display()))?;

        log::debug!("Loaded global config from: {}", path.display());
        Ok(Some(global))
    }

    /// The canonical path for the global config file.
    pub fn path() -> Result<PathBuf> {
        Ok(crate::paths::devflow_config_dir()?.join("config.yml"))
    }
}

/// Match a workspace name against exact names or simple `*` globs (all other
/// regex metacharacters are escaped to avoid surprising matches). Shared by
/// `exclude_workspaces` and `DEVFLOW_DISABLED_BRANCHES` handling so both
/// knobs accept the same syntax.
pub(crate) fn workspace_matches_patterns(workspace_name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        if pattern.contains('*') {
            let escaped = regex::escape(pattern);
            let regex_pattern = format!("^{}$", escaped.replace("\\*", ".*"));
            match regex::Regex::new(&regex_pattern) {
                Ok(re) => re.is_match(workspace_name),
                Err(_) => false,
            }
        } else {
            workspace_name == pattern
        }
    })
}

// The effective configuration after merging all sources
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub config: Config,
    pub global_config: Option<GlobalConfig>,
    pub local_config: Option<LocalConfig>,
    pub env_config: EnvConfig,
    pub disabled: bool,
    pub skip_hooks: bool,
    pub current_workspace_disabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            name: None,
            default_vcs: None,
            git: GitConfig::default(),
            behavior: BehaviorConfig {
                max_workspaces: Some(10),
            },
            services: None,
            worktree: WorktreeConfig::default(),
            hooks: None,
            triggers: None,
            processes: None,
            agent: None,
            commit: None,
            execute: None,
            project_root: None,
        }
    }
}

impl LocalConfig {
    pub fn load_from_project_dir(project_dir: &Path) -> Result<Option<Self>> {
        let local_config_path = project_dir.join(".devflow.local.yml");

        if !local_config_path.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(&local_config_path).with_context(|| {
            format!(
                "Failed to read local config file: {}",
                local_config_path.display()
            )
        })?;

        let local_config: LocalConfig = serde_yaml_ng::from_str(&content).with_context(|| {
            format!(
                "Failed to parse local config file: {}",
                local_config_path.display()
            )
        })?;

        log::debug!("Loaded local config from: {}", local_config_path.display());
        Ok(Some(local_config))
    }
}

impl EnvConfig {
    pub fn load_from_env() -> Result<Self> {
        let env_config = EnvConfig {
            disabled: Self::parse_bool_env("DEVFLOW_DISABLED")?,
            skip_hooks: Self::parse_bool_env("DEVFLOW_SKIP_HOOKS")?,
            auto_create: Self::parse_bool_env("DEVFLOW_AUTO_CREATE")?,
            current_workspace_disabled: Self::parse_bool_env("DEVFLOW_CURRENT_BRANCH_DISABLED")?,
            workspace_filter_regex: env::var("DEVFLOW_BRANCH_FILTER_REGEX").ok(),
            disabled_workspaces: env::var("DEVFLOW_DISABLED_BRANCHES")
                .ok()
                .map(|s| s.split(',').map(|s| s.trim().to_string()).collect()),
        };

        Ok(env_config)
    }

    fn parse_bool_env(key: &str) -> Result<Option<bool>> {
        match env::var(key) {
            Ok(value) => match value.to_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => Ok(Some(true)),
                "false" | "0" | "no" | "off" => Ok(Some(false)),
                _ => Err(anyhow::anyhow!(
                    "Invalid boolean value for {}: '{}'. Use true/false, 1/0, yes/no, or on/off",
                    key,
                    value
                )),
            },
            Err(_) => Ok(None),
        }
    }
}

impl EffectiveConfig {
    pub fn new(
        config: Config,
        global_config: Option<GlobalConfig>,
        local_config: Option<LocalConfig>,
        env_config: EnvConfig,
    ) -> Result<Self> {
        // Determine global disabled state
        let disabled = env_config.disabled.unwrap_or(
            local_config
                .as_ref()
                .and_then(|c| c.disabled)
                .unwrap_or(false),
        );

        // Determine skip hooks state
        let skip_hooks = env_config.skip_hooks.unwrap_or(false);

        // Determine current workspace disabled state
        let current_workspace_disabled = env_config.current_workspace_disabled.unwrap_or(false);

        Ok(EffectiveConfig {
            config,
            global_config,
            local_config,
            env_config,
            disabled,
            skip_hooks,
            current_workspace_disabled,
        })
    }

    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    pub fn should_skip_hooks(&self) -> bool {
        self.skip_hooks
    }

    pub fn is_current_workspace_disabled(&self) -> bool {
        self.current_workspace_disabled
    }

    pub fn is_workspace_disabled(&self, workspace_name: &str) -> bool {
        // Check environment disabled workspaces
        if let Some(ref disabled_workspaces) = self.env_config.disabled_workspaces {
            if workspace_matches_patterns(workspace_name, disabled_workspaces) {
                return true;
            }
        }

        // Check local config disabled workspaces
        if let Some(ref local_config) = self.local_config {
            if let Some(ref disabled_workspaces) = local_config.disabled_workspaces {
                if workspace_matches_patterns(workspace_name, disabled_workspaces) {
                    return true;
                }
            }
        }

        false
    }

    pub fn check_current_git_workspace_disabled(&self) -> Result<bool> {
        if self.is_current_workspace_disabled() {
            return Ok(true);
        }

        // Get current VCS workspace and check if it's disabled
        match crate::vcs::detect_vcs_provider(".") {
            Ok(vcs_repo) => {
                if let Ok(Some(current_workspace)) = vcs_repo.current_workspace() {
                    Ok(self.is_workspace_disabled(&current_workspace))
                } else {
                    Ok(false)
                }
            }
            Err(_) => Ok(false),
        }
    }

    pub fn should_exit_early(&self) -> Result<bool> {
        if self.is_disabled() {
            return Ok(true);
        }

        self.check_current_git_workspace_disabled()
    }

    pub fn get_merged_config(&self) -> Config {
        let mut merged = self.config.clone();

        // Apply global config as base layer (lowest priority — only fills in
        // fields that are still None after the project config).
        if let Some(ref global) = self.global_config {
            if merged.default_vcs.is_none() {
                merged.default_vcs = global.default_vcs;
            }
        }

        // Apply local config overrides
        if let Some(ref local_config) = self.local_config {
            if let Some(ref local_git) = local_config.git {
                if let Some(auto_create) = local_git.auto_create_on_workspace {
                    merged.git.auto_create_on_workspace = auto_create;
                }
                if let Some(ref main_workspace) = local_git.main_workspace {
                    merged.git.main_workspace = main_workspace.clone();
                }
                if let Some(ref regex) = local_git.workspace_filter_regex {
                    merged.git.workspace_filter_regex = Some(regex.clone());
                }
                if let Some(ref exclude_workspaces) = local_git.exclude_workspaces {
                    merged.git.exclude_workspaces = exclude_workspaces.clone();
                }
            }

            if let Some(ref local_behavior) = local_config.behavior {
                if let Some(max_workspaces) = local_behavior.max_workspaces {
                    merged.behavior.max_workspaces = Some(max_workspaces);
                }
            }

            if let Some(ref worktree) = local_config.worktree {
                merged.worktree = worktree.clone();
            }

            // Local default_vcs overrides both project and global
            if let Some(vcs) = local_config.default_vcs {
                merged.default_vcs = Some(vcs);
            }
        }

        // Apply environment config overrides
        if let Some(auto_create) = self.env_config.auto_create {
            merged.git.auto_create_on_workspace = auto_create;
        }
        if let Some(ref regex) = self.env_config.workspace_filter_regex {
            merged.git.workspace_filter_regex = Some(regex.clone());
        }

        merged
    }
}

#[cfg(test)]
mod tests;

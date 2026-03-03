use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Project name (derived from `devflow init <name>` or the directory name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Preferred VCS for this project ("git" or "jj").
    /// Overrides the global `default_vcs` when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_vcs: Option<crate::vcs::VcsKind>,
    #[serde(default, skip_serializing_if = "DatabaseConfig::is_default")]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub git: GitConfig,
    #[serde(default)]
    pub behavior: BehaviorConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<NamedServiceConfig>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeConfig>,
    /// New hook engine configuration (Phase 2).
    /// Maps hook phase names to named hook entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<crate::hooks::HooksConfig>,
    /// VCS event → devflow phase trigger mapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triggers: Option<crate::hooks::triggers::TriggersConfig>,
    /// AI agent integration configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentConfig>,
    /// Commit message generation configuration (LLM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<CommitConfig>,
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
}

fn default_provider_type() -> String {
    "local".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Command to launch the agent (e.g., "claude", "codex").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Workspace prefix for agent-created workspaces (default: "agent/").
    #[serde(
        default = "default_agent_workspace_prefix",
        alias = "branch_prefix",
        skip_serializing_if = "is_default_agent_workspace_prefix"
    )]
    pub workspace_prefix: String,
    /// Automatically provide project context to the agent on launch.
    #[serde(default = "default_true")]
    pub auto_context: bool,
}

fn default_agent_workspace_prefix() -> String {
    "agent/".to_string()
}

fn is_default_agent_workspace_prefix(s: &String) -> bool {
    s == "agent/"
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorktreeConfig {
    /// Whether worktree mode is enabled (default: false).
    /// When true, `devflow switch` creates Git worktrees instead of `git checkout`.
    #[serde(default)]
    pub enabled: bool,
    /// Path template for new worktrees.
    /// Supports `{repo}` and `{workspace}` placeholders.
    /// Default: `"../{repo}.{workspace}"`
    #[serde(default = "default_worktree_path_template")]
    pub path_template: String,
    /// Files to copy from the main worktree into each new worktree.
    #[serde(default)]
    pub copy_files: Vec<String>,
    /// Also copy files that are git-ignored (e.g. `.env.local`).
    #[serde(default)]
    pub copy_ignored: bool,
    /// Exclude gitignored files from worktrees (both CoW and non-CoW paths).
    /// Default: `true` — saves disk space by removing dirs like `node_modules/`, `target/`.
    #[serde(default = "default_respect_gitignore")]
    pub respect_gitignore: bool,
}

impl WorktreeConfig {
    /// Recommended default worktree configuration for new projects.
    /// Enables worktrees with sensible defaults for common environment files.
    pub fn recommended_default() -> Self {
        WorktreeConfig {
            enabled: true,
            path_template: default_worktree_path_template(),
            copy_files: vec![".env".to_string(), ".env.local".to_string()],
            copy_ignored: true,
            respect_gitignore: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    pub template_database: String,
    pub database_prefix: String,
    pub auth: AuthConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub methods: Vec<AuthMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pgpass_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,
    pub prompt_for_password: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuthMethod {
    #[serde(rename = "password")]
    Password,
    #[serde(rename = "pgpass")]
    Pgpass,
    #[serde(rename = "environment")]
    Environment,
    #[serde(rename = "service")]
    Service,
    #[serde(rename = "prompt")]
    Prompt,
    #[serde(rename = "system")]
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitConfig {
    #[serde(default = "default_true", alias = "auto_create_on_branch")]
    pub auto_create_on_workspace: bool,
    #[serde(default = "default_true", alias = "auto_switch_on_branch")]
    pub auto_switch_on_workspace: bool,
    #[serde(default = "default_main_workspace", alias = "main_branch")]
    pub main_workspace: String,
    #[serde(
        default,
        alias = "auto_create_branch_filter",
        skip_serializing_if = "Option::is_none"
    )]
    pub auto_create_workspace_filter: Option<String>,
    #[serde(
        default,
        alias = "branch_filter_regex",
        skip_serializing_if = "Option::is_none"
    )]
    pub workspace_filter_regex: Option<String>,
    #[serde(default = "default_exclude_workspaces", alias = "exclude_branches")]
    pub exclude_workspaces: Vec<String>,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            auto_create_on_workspace: true,
            auto_switch_on_workspace: true,
            main_workspace: "main".to_string(),
            auto_create_workspace_filter: None,
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

fn default_respect_gitignore() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehaviorConfig {
    #[serde(default)]
    pub auto_cleanup: bool,
    #[serde(
        default,
        alias = "max_branches",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_workspaces: Option<usize>,
    #[serde(default)]
    pub naming_strategy: NamingStrategy,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            auto_cleanup: false,
            max_workspaces: Some(10),
            naming_strategy: NamingStrategy::Prefix,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum NamingStrategy {
    #[serde(rename = "prefix")]
    #[default]
    Prefix,
    #[serde(rename = "suffix")]
    Suffix,
    #[serde(rename = "replace")]
    Replace,
}

// Local configuration that can override the main config
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LocalConfig {
    pub database: Option<LocalDatabaseConfig>,
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
pub struct LocalDatabaseConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub template_database: Option<String>,
    pub database_prefix: Option<String>,
    pub auth: Option<LocalAuthConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LocalAuthConfig {
    pub methods: Option<Vec<AuthMethod>>,
    pub pgpass_file: Option<String>,
    pub service_name: Option<String>,
    pub prompt_for_password: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LocalGitConfig {
    #[serde(alias = "auto_create_on_branch")]
    pub auto_create_on_workspace: Option<bool>,
    #[serde(alias = "auto_switch_on_branch")]
    pub auto_switch_on_workspace: Option<bool>,
    #[serde(alias = "main_branch")]
    pub main_workspace: Option<String>,
    #[serde(alias = "auto_create_branch_filter")]
    pub auto_create_workspace_filter: Option<String>,
    #[serde(alias = "branch_filter_regex")]
    pub workspace_filter_regex: Option<String>,
    #[serde(alias = "exclude_branches")]
    pub exclude_workspaces: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LocalBehaviorConfig {
    pub auto_cleanup: Option<bool>,
    #[serde(alias = "max_branches")]
    pub max_workspaces: Option<usize>,
    pub naming_strategy: Option<NamingStrategy>,
}

// Environment variable configuration
#[derive(Debug, Clone, Default)]
pub struct EnvConfig {
    pub disabled: Option<bool>,
    pub skip_hooks: Option<bool>,
    pub auto_create: Option<bool>,
    pub auto_switch: Option<bool>,
    pub workspace_filter_regex: Option<String>,
    pub disabled_workspaces: Option<Vec<String>>,
    pub current_workspace_disabled: Option<bool>,
    pub database_host: Option<String>,
    pub database_port: Option<u16>,
    pub database_user: Option<String>,
    pub database_password: Option<String>,
    pub database_prefix: Option<String>,
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

    /// Save the global config to `~/.config/devflow/config.yml`.
    #[allow(dead_code)]
    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create dir: {}", parent.display()))?;
        }
        let content =
            serde_yaml_ng::to_string(self).context("Failed to serialize global config")?;
        fs::write(&path, content)
            .with_context(|| format!("Failed to write global config: {}", path.display()))?;
        Ok(())
    }

    /// The canonical path for the global config file.
    pub fn path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .context("Failed to get user config directory")?
            .join("devflow");
        Ok(config_dir.join("config.yml"))
    }
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

impl Default for DatabaseConfig {
    fn default() -> Self {
        DatabaseConfig {
            host: "localhost".to_string(),
            port: 5432,
            user: "postgres".to_string(),
            password: None,
            template_database: "template0".to_string(),
            database_prefix: "devflow".to_string(),
            auth: AuthConfig {
                methods: vec![
                    AuthMethod::Environment,
                    AuthMethod::Pgpass,
                    AuthMethod::Password,
                    AuthMethod::Prompt,
                ],
                pgpass_file: None,
                service_name: None,
                prompt_for_password: false,
            },
        }
    }
}

impl DatabaseConfig {
    pub fn is_default(&self) -> bool {
        let default = DatabaseConfig::default();
        self.host == default.host
            && self.port == default.port
            && self.user == default.user
            && self.password.is_none()
            && self.template_database == default.template_database
            && self.database_prefix == default.database_prefix
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            name: None,
            default_vcs: None,
            database: DatabaseConfig::default(),
            git: GitConfig {
                auto_create_on_workspace: true,
                auto_switch_on_workspace: true,
                main_workspace: "main".to_string(),
                auto_create_workspace_filter: None,
                workspace_filter_regex: None,
                exclude_workspaces: vec!["main".to_string(), "master".to_string()],
            },
            behavior: BehaviorConfig {
                auto_cleanup: false,
                max_workspaces: Some(10),
                naming_strategy: NamingStrategy::Prefix,
            },
            services: None,
            worktree: None,
            hooks: None,
            triggers: None,
            agent: None,
            commit: None,
        }
    }
}

impl Config {
    /// Return the project name, falling back to the directory name of the
    /// config file (or the current working directory).
    pub fn project_name(&self) -> String {
        if let Some(ref name) = self.name {
            return name.clone();
        }
        std::env::current_dir()
            .ok()
            .and_then(|d| d.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "default".to_string())
    }

    pub fn load_with_path_info() -> Result<(Self, Option<std::path::PathBuf>)> {
        if let Some(config_path) = Self::find_config_file()? {
            let config = Self::from_file(&config_path)?;
            Ok((config, Some(config_path)))
        } else {
            log::info!("No .devflow file found, using default configuration");
            Ok((Config::default(), None))
        }
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let config: Config = serde_yaml_ng::from_str(&content)
            .with_context(|| format!("Failed to parse YAML config file: {}", path.display()))?;

        Ok(config)
    }

    pub fn save_to_file(&self, path: &Path) -> Result<()> {
        let content =
            serde_yaml_ng::to_string(self).context("Failed to serialize config to YAML")?;

        fs::write(path, content)
            .with_context(|| format!("Failed to write config file: {}", path.display()))?;

        Ok(())
    }

    pub fn find_config_file() -> Result<Option<PathBuf>> {
        let mut current_dir = std::env::current_dir().context("Failed to get current directory")?;

        loop {
            // Check for YAML format only
            for filename in [".devflow.yml", ".devflow.yaml"] {
                let config_path = current_dir.join(filename);
                if config_path.exists() {
                    return Ok(Some(config_path));
                }
            }

            if let Some(parent) = current_dir.parent() {
                current_dir = parent.to_path_buf();
            } else {
                break;
            }
        }

        Ok(None)
    }

    pub fn get_database_name(&self, workspace_name: &str) -> String {
        // For main workspace marker, use the template database name directly
        if workspace_name == "_main" {
            return self.database.template_database.clone();
        }

        // For excluded workspaces (main/master), use the template database name directly
        if self
            .git
            .exclude_workspaces
            .contains(&workspace_name.to_string())
        {
            return self.database.template_database.clone();
        }

        let sanitized_branch = Self::sanitize_workspace_name(workspace_name);

        let full_name = match self.behavior.naming_strategy {
            NamingStrategy::Prefix => {
                format!("{}_{}", self.database.database_prefix, sanitized_branch)
            }
            NamingStrategy::Suffix => {
                format!("{}_{}", sanitized_branch, self.database.database_prefix)
            }
            NamingStrategy::Replace => sanitized_branch,
        };

        Self::ensure_valid_postgres_name(&full_name)
    }

    fn sanitize_workspace_name(workspace_name: &str) -> String {
        // Convert to lowercase and replace invalid characters with underscores
        let mut sanitized = String::new();

        for ch in workspace_name.to_lowercase().chars() {
            match ch {
                // Valid PostgreSQL identifier characters
                'a'..='z' | '0'..='9' | '_' | '$' => sanitized.push(ch),
                // Replace everything else with underscore
                _ => sanitized.push('_'),
            }
        }

        // Ensure it starts with letter or underscore (not digit)
        if sanitized.starts_with(|c: char| c.is_ascii_digit()) {
            sanitized = format!("_{}", sanitized);
        }

        // Remove consecutive underscores for cleaner names
        while sanitized.contains("__") {
            sanitized = sanitized.replace("__", "_");
        }

        // Remove trailing underscore
        sanitized = sanitized.trim_end_matches('_').to_string();

        // Ensure we have something if everything got removed
        if sanitized.is_empty() {
            sanitized = "workspace".to_string();
        }

        sanitized
    }

    fn ensure_valid_postgres_name(name: &str) -> String {
        const MAX_POSTGRES_NAME_LENGTH: usize = 63;

        if name.len() <= MAX_POSTGRES_NAME_LENGTH {
            return name.to_string();
        }

        // If name is too long, truncate and add hash to avoid collisions
        let hash = Self::calculate_name_hash(name);
        let hash_suffix = format!("_{:x}", hash);
        let max_prefix_len = MAX_POSTGRES_NAME_LENGTH - hash_suffix.len();

        format!("{}{}", &name[..max_prefix_len], hash_suffix)
    }

    fn calculate_name_hash(name: &str) -> u32 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        name.hash(&mut hasher);
        (hasher.finish() as u32) & 0xFFFF // Use 16 bits for shorter hash
    }

    pub fn should_create_workspace(&self, workspace_name: &str) -> bool {
        if !self.git.auto_create_on_workspace {
            return false;
        }

        if self
            .git
            .exclude_workspaces
            .contains(&workspace_name.to_string())
        {
            return false;
        }

        // Prefer the newer workspace_filter_regex, but keep supporting
        // auto_create_workspace_filter for backward compatibility.
        let create_filter = self
            .git
            .workspace_filter_regex
            .as_ref()
            .or(self.git.auto_create_workspace_filter.as_ref());

        if let Some(filter) = create_filter {
            match regex::Regex::new(filter) {
                Ok(re) => re.is_match(workspace_name),
                Err(_) => {
                    log::warn!("Invalid regex filter: {}", filter);
                    false
                }
            }
        } else {
            true
        }
    }

    pub fn should_switch_on_workspace(&self, workspace_name: &str) -> bool {
        if !self.git.auto_switch_on_workspace {
            return false;
        }

        // Always switch to main workspace
        if workspace_name == self.git.main_workspace {
            return true;
        }

        if self
            .git
            .exclude_workspaces
            .contains(&workspace_name.to_string())
        {
            return false;
        }

        if let Some(filter) = &self.git.workspace_filter_regex {
            match regex::Regex::new(filter) {
                Ok(re) => re.is_match(workspace_name),
                Err(_) => {
                    log::warn!("Invalid regex filter: {}", filter);
                    false
                }
            }
        } else {
            true
        }
    }

    pub fn get_normalized_workspace_name(&self, workspace_name: &str) -> String {
        Self::sanitize_workspace_name(workspace_name)
    }

    /// Resolve the list of named services from the `services` config.
    pub fn resolve_services(&self) -> Vec<NamedServiceConfig> {
        if let Some(ref services) = self.services {
            services.clone()
        } else {
            vec![]
        }
    }

    /// Return the name of the default service (the one with `default: true`, or the first).
    #[allow(dead_code)]
    pub fn default_service_name(&self) -> Option<String> {
        let services = self.resolve_services();
        if services.is_empty() {
            return None;
        }
        services
            .iter()
            .find(|b| b.default)
            .or(services.first())
            .map(|b| b.name.clone())
    }

    /// Look up a named service config by name.
    #[allow(dead_code)]
    pub fn get_service_config(&self, name: &str) -> Option<NamedServiceConfig> {
        self.resolve_services().into_iter().find(|b| b.name == name)
    }

    /// Validate the services configuration (no duplicates, at most one default).
    pub fn validate_services(&self) -> Result<()> {
        if let Some(ref services) = self.services {
            // Check for unique names
            let mut seen = std::collections::HashSet::new();
            let mut default_count = 0;
            for b in services {
                if !seen.insert(&b.name) {
                    anyhow::bail!("Duplicate service name: '{}'", b.name);
                }
                if b.default {
                    default_count += 1;
                }
            }
            if default_count > 1 {
                anyhow::bail!(
                    "At most one service can be marked as default, found {}",
                    default_count
                );
            }
        }
        Ok(())
    }

    /// Add a named service. Errors if name exists unless force=true.
    #[allow(dead_code)]
    pub fn add_service(&mut self, named: NamedServiceConfig, force: bool) -> Result<()> {
        let services = self.services.get_or_insert_with(Vec::new);

        if let Some(pos) = services.iter().position(|b| b.name == named.name) {
            if force {
                services[pos] = named;
            } else {
                anyhow::bail!(
                    "Service '{}' already exists. Use --force to overwrite.",
                    services[pos].name
                );
            }
        } else {
            // Set default if it's the first entry
            let mut named = named;
            if services.is_empty() {
                named.default = true;
            }
            services.push(named);
        }

        Ok(())
    }

    pub fn remove_service(&mut self, name: &str) {
        if let Some(ref mut services) = self.services {
            services.retain(|b| b.name != name);
        }
    }

    pub fn load_effective_config_with_path_info(
    ) -> Result<(EffectiveConfig, Option<std::path::PathBuf>)> {
        // Load global user config (~/.config/devflow/config.yml)
        let global_config = GlobalConfig::load()?;

        // Load main config
        let (config, config_path) = Self::load_with_path_info()?;

        // Load local config if it exists - check in current directory if no main config path
        let local_config = if let Some(ref path) = config_path {
            let mut lc = LocalConfig::load_from_project_dir(path.parent().unwrap())?;
            // If no local config found and we're in a worktree, try the main worktree
            if lc.is_none() {
                if let Ok(vcs_repo) = crate::vcs::detect_vcs_provider(".") {
                    if vcs_repo.is_worktree() {
                        if let Some(main_dir) = vcs_repo.main_worktree_dir() {
                            lc = LocalConfig::load_from_project_dir(&main_dir)?;
                        }
                    }
                }
            }
            lc
        } else {
            // No main config found, but check current directory for local config
            LocalConfig::load_from_project_dir(&std::env::current_dir()?)?
        };

        // Load environment config
        let env_config = EnvConfig::load_from_env()?;

        // Create effective config
        let effective_config =
            EffectiveConfig::new(config, global_config, local_config, env_config)?;

        Ok((effective_config, config_path))
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
            auto_switch: Self::parse_bool_env("DEVFLOW_AUTO_SWITCH")?,
            current_workspace_disabled: Self::parse_bool_env("DEVFLOW_CURRENT_BRANCH_DISABLED")?,
            workspace_filter_regex: env::var("DEVFLOW_BRANCH_FILTER_REGEX").ok(),
            database_host: env::var("DEVFLOW_DATABASE_HOST").ok(),
            database_user: env::var("DEVFLOW_DATABASE_USER").ok(),
            database_password: env::var("DEVFLOW_DATABASE_PASSWORD").ok(),
            database_prefix: env::var("DEVFLOW_DATABASE_PREFIX").ok(),
            database_port: env::var("DEVFLOW_DATABASE_PORT")
                .ok()
                .and_then(|s| s.parse().ok()),
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
            if Self::workspace_matches_patterns(workspace_name, disabled_workspaces) {
                return true;
            }
        }

        // Check local config disabled workspaces
        if let Some(ref local_config) = self.local_config {
            if let Some(ref disabled_workspaces) = local_config.disabled_workspaces {
                if Self::workspace_matches_patterns(workspace_name, disabled_workspaces) {
                    return true;
                }
            }
        }

        false
    }

    fn workspace_matches_patterns(workspace_name: &str, patterns: &[String]) -> bool {
        patterns.iter().any(|pattern| {
            if pattern.contains('*') {
                // Simple glob pattern matching (*), with all other regex
                // metacharacters escaped to avoid surprising matches.
                let escaped = regex::escape(pattern);
                let regex_pattern = format!("^{}$", escaped.replace("\\*", ".*"));
                match regex::Regex::new(&regex_pattern) {
                    Ok(re) => re.is_match(workspace_name),
                    Err(_) => false,
                }
            } else {
                // Exact match
                workspace_name == pattern
            }
        })
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
            if let Some(ref local_db) = local_config.database {
                if let Some(ref host) = local_db.host {
                    merged.database.host = host.clone();
                }
                if let Some(port) = local_db.port {
                    merged.database.port = port;
                }
                if let Some(ref user) = local_db.user {
                    merged.database.user = user.clone();
                }
                if let Some(ref password) = local_db.password {
                    merged.database.password = Some(password.clone());
                }
                if let Some(ref template_db) = local_db.template_database {
                    merged.database.template_database = template_db.clone();
                }
                if let Some(ref prefix) = local_db.database_prefix {
                    merged.database.database_prefix = prefix.clone();
                }
                if let Some(ref auth) = local_db.auth {
                    if let Some(ref methods) = auth.methods {
                        merged.database.auth.methods = methods.clone();
                    }
                    if let Some(ref pgpass_file) = auth.pgpass_file {
                        merged.database.auth.pgpass_file = Some(pgpass_file.clone());
                    }
                    if let Some(ref service_name) = auth.service_name {
                        merged.database.auth.service_name = Some(service_name.clone());
                    }
                    if let Some(prompt_for_password) = auth.prompt_for_password {
                        merged.database.auth.prompt_for_password = prompt_for_password;
                    }
                }
            }

            if let Some(ref local_git) = local_config.git {
                if let Some(auto_create) = local_git.auto_create_on_workspace {
                    merged.git.auto_create_on_workspace = auto_create;
                }
                if let Some(auto_switch) = local_git.auto_switch_on_workspace {
                    merged.git.auto_switch_on_workspace = auto_switch;
                }
                if let Some(ref main_workspace) = local_git.main_workspace {
                    merged.git.main_workspace = main_workspace.clone();
                }
                if let Some(ref filter) = local_git.auto_create_workspace_filter {
                    merged.git.auto_create_workspace_filter = Some(filter.clone());
                }
                if let Some(ref regex) = local_git.workspace_filter_regex {
                    merged.git.workspace_filter_regex = Some(regex.clone());
                }
                if let Some(ref exclude_workspaces) = local_git.exclude_workspaces {
                    merged.git.exclude_workspaces = exclude_workspaces.clone();
                }
            }

            if let Some(ref local_behavior) = local_config.behavior {
                if let Some(auto_cleanup) = local_behavior.auto_cleanup {
                    merged.behavior.auto_cleanup = auto_cleanup;
                }
                if let Some(max_workspaces) = local_behavior.max_workspaces {
                    merged.behavior.max_workspaces = Some(max_workspaces);
                }
                if let Some(ref naming_strategy) = local_behavior.naming_strategy {
                    merged.behavior.naming_strategy = naming_strategy.clone();
                }
            }

            if let Some(ref worktree) = local_config.worktree {
                merged.worktree = Some(worktree.clone());
            }

            // Local default_vcs overrides both project and global
            if let Some(vcs) = local_config.default_vcs {
                merged.default_vcs = Some(vcs);
            }
        }

        // Apply environment config overrides
        if let Some(ref host) = self.env_config.database_host {
            merged.database.host = host.clone();
        }
        if let Some(port) = self.env_config.database_port {
            merged.database.port = port;
        }
        if let Some(ref user) = self.env_config.database_user {
            merged.database.user = user.clone();
        }
        if let Some(ref password) = self.env_config.database_password {
            merged.database.password = Some(password.clone());
        }
        if let Some(ref prefix) = self.env_config.database_prefix {
            merged.database.database_prefix = prefix.clone();
        }
        if let Some(auto_create) = self.env_config.auto_create {
            merged.git.auto_create_on_workspace = auto_create;
        }
        if let Some(auto_switch) = self.env_config.auto_switch {
            merged.git.auto_switch_on_workspace = auto_switch;
        }
        if let Some(ref regex) = self.env_config.workspace_filter_regex {
            merged.git.workspace_filter_regex = Some(regex.clone());
        }

        merged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hooks_yaml_parsing_simple() {
        let yaml = r#"
git:
  auto_create_on_workspace: true
  auto_switch_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main, master]
behavior:
  auto_cleanup: false
  naming_strategy: prefix
hooks:
  post-service-create:
    install: "npm ci"
    migrate: "npx prisma migrate deploy"
  post-switch:
    env: "echo DATABASE_URL=postgresql://{{ service.db.user }}@{{ service.db.host }}:{{ service.db.port }}/{{ service.db.database }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");

        let hooks = config.hooks.expect("hooks should be Some");
        assert_eq!(hooks.len(), 2);

        let post_create = hooks
            .get(&crate::hooks::HookPhase::PostServiceCreate)
            .expect("post-service-create phase should exist");
        assert_eq!(post_create.len(), 2);

        // Simple hook entries
        match post_create.get("install").unwrap() {
            crate::hooks::HookEntry::Simple(cmd) => assert_eq!(cmd, "npm ci"),
            _ => panic!("Expected Simple hook entry"),
        }
    }

    #[test]
    fn test_hooks_yaml_parsing_extended() {
        let yaml = r#"
git:
  auto_create_on_workspace: true
  auto_switch_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main]
behavior:
  auto_cleanup: false
  naming_strategy: prefix
hooks:
  post-switch:
    setup:
      command: "npm run setup"
      working_dir: frontend
      condition: "file_exists:frontend/package.json"
      continue_on_error: true
      environment:
        NODE_ENV: development
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");

        let hooks = config.hooks.expect("hooks should be Some");
        let post_switch = hooks
            .get(&crate::hooks::HookPhase::PostSwitch)
            .expect("post-switch phase should exist");

        match post_switch.get("setup").unwrap() {
            crate::hooks::HookEntry::Extended(ext) => {
                assert_eq!(ext.command, "npm run setup");
                assert_eq!(ext.working_dir.as_deref(), Some("frontend"));
                assert_eq!(
                    ext.condition.as_deref(),
                    Some("file_exists:frontend/package.json")
                );
                assert_eq!(ext.continue_on_error, Some(true));
                assert!(ext.environment.is_some());
                assert_eq!(
                    ext.environment.as_ref().unwrap().get("NODE_ENV").unwrap(),
                    "development"
                );
            }
            _ => panic!("Expected Extended hook entry"),
        }
    }

    #[test]
    fn test_no_hooks_parses_as_none() {
        let yaml = r#"
git:
  auto_create_on_workspace: true
  auto_switch_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main]
behavior:
  auto_cleanup: false
  naming_strategy: prefix
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
        assert!(config.hooks.is_none());
    }

    #[test]
    fn test_multi_services_parsing() {
        let yaml = r#"
git:
  auto_create_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main]
behavior:
  auto_cleanup: false
  naming_strategy: prefix
services:
  - name: db
    type: local
    service_type: postgres
    auto_workspace: true
    local:
      image: postgres:16
      port_range_start: 15432
  - name: analytics
    type: local
    service_type: clickhouse
    auto_workspace: true
    clickhouse:
      image: clickhouse/clickhouse-server:24
      port_range_start: 18123
      user: analytics
  - name: legacy-db
    type: local
    service_type: mysql
    auto_workspace: false
    mysql:
      image: mysql:8
      root_password: secret
      database: legacy
      user: app
      password: apppass
  - name: cache
    type: local
    service_type: generic
    auto_workspace: true
    generic:
      image: redis:7
      port_mapping: "6379:6379"
      environment:
        REDIS_MAXMEMORY: "256mb"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");

        let services = config.resolve_services();
        assert_eq!(services.len(), 4);

        // Postgres service
        assert_eq!(services[0].name, "db");
        assert_eq!(services[0].service_type, "postgres");
        assert!(services[0].auto_workspace);
        assert!(services[0].local.is_some());
        assert_eq!(
            services[0].local.as_ref().unwrap().port_range_start,
            Some(15432)
        );

        // ClickHouse service
        assert_eq!(services[1].name, "analytics");
        assert_eq!(services[1].service_type, "clickhouse");
        assert!(services[1].auto_workspace);
        let ch = services[1].clickhouse.as_ref().expect("clickhouse config");
        assert_eq!(ch.image, "clickhouse/clickhouse-server:24");
        assert_eq!(ch.port_range_start, Some(18123));
        assert_eq!(ch.user, "analytics");

        // MySQL service — auto_workspace is false
        assert_eq!(services[2].name, "legacy-db");
        assert_eq!(services[2].service_type, "mysql");
        assert!(!services[2].auto_workspace);
        let mysql = services[2].mysql.as_ref().expect("mysql config");
        assert_eq!(mysql.root_password, "secret");
        assert_eq!(mysql.database.as_deref(), Some("legacy"));
        assert_eq!(mysql.user.as_deref(), Some("app"));
        assert_eq!(mysql.password.as_deref(), Some("apppass"));

        // Generic Docker service
        assert_eq!(services[3].name, "cache");
        assert_eq!(services[3].service_type, "generic");
        assert!(services[3].auto_workspace);
        let generic = services[3].generic.as_ref().expect("generic config");
        assert_eq!(generic.image, "redis:7");
        assert_eq!(generic.port_mapping.as_deref(), Some("6379:6379"));
        assert_eq!(generic.environment.get("REDIS_MAXMEMORY").unwrap(), "256mb");
    }

    #[test]
    fn test_clickhouse_config_defaults() {
        let yaml = r#"
git:
  auto_create_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main]
behavior:
  auto_cleanup: false
  naming_strategy: prefix
services:
  - name: ch
    type: local
    service_type: clickhouse
    clickhouse: {}
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
        let services = config.resolve_services();
        let ch = services[0].clickhouse.as_ref().unwrap();
        assert_eq!(ch.image, "clickhouse/clickhouse-server:latest");
        assert_eq!(ch.user, "default");
        assert!(ch.password.is_none());
        assert!(ch.port_range_start.is_none());
    }

    #[test]
    fn test_mysql_config_defaults() {
        let yaml = r#"
git:
  auto_create_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main]
behavior:
  auto_cleanup: false
  naming_strategy: prefix
services:
  - name: mysql
    type: local
    service_type: mysql
    mysql: {}
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
        let services = config.resolve_services();
        let mysql = services[0].mysql.as_ref().unwrap();
        assert_eq!(mysql.image, "mysql:8");
        assert_eq!(mysql.root_password, "dev");
        assert!(mysql.database.is_none());
        assert!(mysql.user.is_none());
    }

    #[test]
    fn test_generic_docker_config_parsing() {
        let yaml = r#"
git:
  auto_create_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main]
behavior:
  auto_cleanup: false
  naming_strategy: prefix
services:
  - name: mq
    type: local
    service_type: generic
    generic:
      image: rabbitmq:3-management
      port_range_start: 15672
      environment:
        RABBITMQ_DEFAULT_USER: guest
        RABBITMQ_DEFAULT_PASS: guest
      volumes:
        - "/tmp/rabbitmq:/var/lib/rabbitmq"
      command: "rabbitmq-server"
      healthcheck: "rabbitmq-diagnostics -q ping"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
        let services = config.resolve_services();
        let generic = services[0].generic.as_ref().unwrap();
        assert_eq!(generic.image, "rabbitmq:3-management");
        assert_eq!(generic.port_range_start, Some(15672));
        assert_eq!(generic.environment.len(), 2);
        assert_eq!(generic.volumes, vec!["/tmp/rabbitmq:/var/lib/rabbitmq"]);
        assert_eq!(generic.command.as_deref(), Some("rabbitmq-server"));
        assert_eq!(
            generic.healthcheck.as_deref(),
            Some("rabbitmq-diagnostics -q ping")
        );
    }

    #[test]
    fn test_service_type_defaults_to_postgres() {
        let yaml = r#"
git:
  auto_create_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main]
behavior:
  auto_cleanup: false
  naming_strategy: prefix
services:
  - name: mydb
    type: local
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
        let services = config.resolve_services();
        assert_eq!(services[0].service_type, "postgres");
        assert!(services[0].auto_workspace); // default is true
    }

    #[test]
    fn test_auto_branch_filtering() {
        let yaml = r#"
git:
  auto_create_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main]
behavior:
  auto_cleanup: false
  naming_strategy: prefix
services:
  - name: primary
    type: local
    auto_workspace: true
  - name: shared
    type: local
    auto_workspace: false
  - name: analytics
    type: local
    service_type: clickhouse
    auto_workspace: true
    clickhouse: {}
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
        let services = config.resolve_services();
        let auto_branch_services: Vec<_> = services.iter().filter(|b| b.auto_workspace).collect();
        assert_eq!(auto_branch_services.len(), 2);
        assert_eq!(auto_branch_services[0].name, "primary");
        assert_eq!(auto_branch_services[1].name, "analytics");
    }

    #[test]
    fn test_plugin_service_config_parsing() {
        let yaml = r#"
git:
  auto_create_on_workspace: true
  main_workspace: main
  exclude_workspaces: [main]
behavior:
  auto_cleanup: false
  naming_strategy: prefix
services:
  - name: my-redis
    service_type: plugin
    auto_workspace: true
    plugin:
      path: "./plugins/devflow-redis"
      timeout: 45
      config:
        image: "redis:7-alpine"
        port: 16379
  - name: my-cache
    service_type: plugin
    plugin:
      name: memcached
      config:
        memory: 256
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
        let services = config.resolve_services();
        assert_eq!(services.len(), 2);

        // First plugin service
        assert_eq!(services[0].name, "my-redis");
        assert_eq!(services[0].service_type, "plugin");
        assert!(services[0].auto_workspace);
        let plugin = services[0].plugin.as_ref().unwrap();
        assert_eq!(plugin.path.as_deref(), Some("./plugins/devflow-redis"));
        assert!(plugin.name.is_none());
        assert_eq!(plugin.timeout, 45);
        let cfg = plugin.config.as_ref().unwrap();
        assert_eq!(cfg["image"], "redis:7-alpine");
        assert_eq!(cfg["port"], 16379);

        // Second plugin service (name-based resolution)
        assert_eq!(services[1].name, "my-cache");
        assert_eq!(services[1].service_type, "plugin");
        let plugin2 = services[1].plugin.as_ref().unwrap();
        assert!(plugin2.path.is_none());
        assert_eq!(plugin2.name.as_deref(), Some("memcached"));
        assert_eq!(plugin2.timeout, 30); // default
    }

    #[test]
    fn test_should_create_workspace_uses_workspace_filter_regex() {
        let mut config = Config::default();
        config.git.workspace_filter_regex = Some("^feature/.*".to_string());

        assert!(config.should_create_workspace("feature/auth"));
        assert!(!config.should_create_workspace("bugfix/auth"));
    }

    #[test]
    fn test_should_create_workspace_falls_back_to_auto_create_workspace_filter() {
        let mut config = Config::default();
        config.git.workspace_filter_regex = None;
        config.git.auto_create_workspace_filter = Some("^chore/.*".to_string());

        assert!(config.should_create_workspace("chore/deps"));
        assert!(!config.should_create_workspace("feature/deps"));
    }

    #[test]
    fn test_worktree_respect_gitignore_defaults_true() {
        let yaml = r#"
worktree:
  enabled: true
  copy_files: [".env"]
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
        let wt = config.worktree.expect("worktree should be Some");
        assert!(wt.respect_gitignore);
    }

    #[test]
    fn test_worktree_respect_gitignore_explicit_false() {
        let yaml = r#"
worktree:
  enabled: true
  respect_gitignore: false
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse config");
        let wt = config.worktree.expect("worktree should be Some");
        assert!(!wt.respect_gitignore);
    }

    #[test]
    fn test_worktree_recommended_default_includes_respect_gitignore() {
        let wt = WorktreeConfig::recommended_default();
        assert!(wt.respect_gitignore);
    }
}

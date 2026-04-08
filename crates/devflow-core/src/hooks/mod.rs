pub mod actions;
pub mod approval;
pub mod executor;
pub mod recipes;
pub mod template;
pub mod triggers;

// Re-export hook engine types
pub use executor::HookEngine;
#[allow(unused_imports)] // Public API — used by consumers for advanced template rendering
pub use template::TemplateEngine;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
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

    // Rebase lifecycle
    PreRebase,
    PostRebase,

    // Cascade lifecycle
    PostMergeCascade,

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
            HookPhase::PreRebase => write!(f, "pre-rebase"),
            HookPhase::PostRebase => write!(f, "post-rebase"),
            HookPhase::PostMergeCascade => write!(f, "post-merge-cascade"),
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
                | HookPhase::PreRebase
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
            "pre-rebase" => HookPhase::PreRebase,
            "post-rebase" => HookPhase::PostRebase,
            "post-merge-cascade" => HookPhase::PostMergeCascade,
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
/// Deserialization tries `Simple` (string) → `Action` (has `action` key)
/// → `Extended` (has `command` key). This preserves backward compatibility.
///
/// ```yaml
/// hooks:
///   post-create:
///     install: "npm ci"                            # Simple
///     env:                                         # Extended
///       command: "echo {{ workspace }}"
///     write-env:                                   # Action
///       action:
///         type: write-env
///         path: ".env.local"
///         vars:
///           DATABASE_URL: "{{ service['app-db'].url }}"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HookEntry {
    /// Simple command string (the value is a template-rendered shell command)
    Simple(String),
    /// Built-in action (has `action` key — must be tried before Extended)
    Action(ActionHookEntry),
    /// Extended hook with extra options (has `command` key)
    Extended(ExtendedHookEntry),
}

impl HookEntry {
    /// Whether this hook entry requires user approval before execution.
    /// Only shell commands and docker-exec need approval; built-in file/network actions do not.
    pub fn requires_approval(&self) -> bool {
        match self {
            HookEntry::Simple(_) | HookEntry::Extended(_) => true,
            HookEntry::Action(a) => a.action.requires_approval(),
        }
    }
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

/// A hook entry that uses a built-in action instead of a shell command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionHookEntry {
    /// The built-in action to execute
    pub action: HookAction,
    /// Working directory (relative to project root)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// Whether to continue on error
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_on_error: Option<bool>,
    /// Condition expression
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// Extra environment variables (values are MiniJinja templates)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<HashMap<String, String>>,
    /// Run in the background even if the phase is normally blocking
    #[serde(default)]
    pub background: bool,
}

/// Built-in hook actions. All string fields support MiniJinja templates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum HookAction {
    /// Run a shell command (same as Extended, but explicit as an action)
    Shell { command: String },
    /// Find-and-replace in a file
    Replace {
        file: String,
        pattern: String,
        replacement: String,
        #[serde(default)]
        regex: bool,
        #[serde(default)]
        create_if_missing: bool,
    },
    /// Write content to a file
    WriteFile {
        path: String,
        content: String,
        #[serde(default)]
        mode: WriteMode,
    },
    /// Write environment variables to a dotenv file
    WriteEnv {
        path: String,
        vars: IndexMap<String, String>,
        #[serde(default)]
        mode: EnvWriteMode,
    },
    /// Copy a file
    Copy {
        from: String,
        to: String,
        #[serde(default = "default_true")]
        overwrite: bool,
    },
    /// Execute a command inside a Docker container
    DockerExec {
        container: String,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
    },
    /// Make an HTTP request
    Http {
        url: String,
        #[serde(default = "default_http_method")]
        method: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        headers: Option<HashMap<String, String>>,
    },
    /// Send a desktop notification
    Notify {
        title: String,
        message: String,
        #[serde(default)]
        level: NotifyLevel,
    },
}

fn default_true() -> bool {
    true
}

fn default_http_method() -> String {
    "GET".to_string()
}

impl HookAction {
    /// Whether this action requires user approval before execution.
    pub fn requires_approval(&self) -> bool {
        matches!(
            self,
            HookAction::Shell { .. } | HookAction::DockerExec { .. }
        )
    }

    /// Human-readable action type name.
    pub fn type_name(&self) -> &'static str {
        match self {
            HookAction::Shell { .. } => "shell",
            HookAction::Replace { .. } => "replace",
            HookAction::WriteFile { .. } => "write-file",
            HookAction::WriteEnv { .. } => "write-env",
            HookAction::Copy { .. } => "copy",
            HookAction::DockerExec { .. } => "docker-exec",
            HookAction::Http { .. } => "http",
            HookAction::Notify { .. } => "notify",
        }
    }
}

/// Write mode for the write-file action.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WriteMode {
    #[default]
    Overwrite,
    Append,
    CreateOnly,
}

/// Write mode for the write-env action.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnvWriteMode {
    #[default]
    Overwrite,
    Merge,
}

/// Notification level for the notify action.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotifyLevel {
    #[default]
    Info,
    Success,
    Warning,
    Error,
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
    /// Current workspace name
    pub workspace: String,
    /// Project name from config (`name`) or project directory fallback.
    pub name: String,
    /// Repository directory name
    pub repo: String,
    /// Worktree path (if in a worktree)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    /// Default workspace (main/master)
    pub default_workspace: String,
    /// HEAD commit SHA
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Abbreviated HEAD commit SHA
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_commit: Option<String>,
    /// Target workspace (for merge hooks)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Base workspace (for creation hooks)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Previous workspace name (for switch hooks)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_workspace: Option<String>,
    /// What triggered this hook execution: "vcs", "cli", "gui", "auto"
    #[serde(default = "default_trigger_source")]
    pub trigger_source: String,
    /// The VCS event that triggered this hook, if any (e.g. "post-checkout")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vcs_event: Option<String>,
    /// Service connection info, keyed by service name.
    /// Each service exposes: host, port, database, user, password, url
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub service: HashMap<String, ServiceContext>,
}

#[allow(dead_code)] // Used by serde(default)
fn default_trigger_source() -> String {
    "cli".to_string()
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

/// Build a `HookContext` for the given config and workspace.
///
/// Populates service connection info from all configured services.
pub async fn build_hook_context(
    config: &crate::config::Config,
    project_dir: &Path,
    workspace_name: &str,
) -> HookContext {
    let canonical_project_dir = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());

    let repo = canonical_project_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "default".to_string());

    let name = config
        .name
        .as_ref()
        .filter(|n| !n.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| repo.clone());

    // Detect worktree path if we're inside a VCS worktree
    let worktree_path = crate::vcs::detect_vcs_provider(&canonical_project_dir)
        .ok()
        .and_then(|vcs_repo| {
            if vcs_repo.is_worktree() {
                Some(canonical_project_dir.to_string_lossy().to_string())
            } else {
                let normalized = config.get_normalized_workspace_name(workspace_name);
                vcs_repo
                    .worktree_path(workspace_name)
                    .ok()
                    .flatten()
                    .or_else(|| {
                        vcs_repo.list_worktrees().ok().and_then(|worktrees| {
                            worktrees.into_iter().find_map(|wt| {
                                let branch = wt.workspace?;
                                let normalized_branch =
                                    config.get_normalized_workspace_name(&branch);
                                if branch == workspace_name
                                    || normalized_branch == workspace_name
                                    || normalized_branch == normalized
                                {
                                    Some(wt.path)
                                } else {
                                    None
                                }
                            })
                        })
                    })
                    .map(|p| p.to_string_lossy().to_string())
            }
        });

    // Build service map from all configured services
    let mut service = HashMap::new();

    if let Ok(conn_infos) =
        crate::services::factory::get_all_connection_info(config, workspace_name).await
    {
        for (name, info) in conn_infos {
            let url = info
                .connection_string
                .clone()
                .unwrap_or_else(|| format!("{}:{}", info.host, info.port));
            service.insert(
                name,
                ServiceContext {
                    host: info.host,
                    port: info.port,
                    database: info.database,
                    user: info.user,
                    password: info.password,
                    url,
                },
            );
        }
    }

    // Resolve HEAD commit via git2
    let (commit, short_commit) = git2::Repository::discover(&canonical_project_dir)
        .ok()
        .and_then(|repo| {
            repo.head().ok().and_then(|head| head.target()).map(|oid| {
                let sha = oid.to_string();
                let short = sha[..7.min(sha.len())].to_string();
                (Some(sha), Some(short))
            })
        })
        .unwrap_or((None, None));

    HookContext {
        workspace: workspace_name.to_string(),
        name,
        repo,
        worktree_path,
        default_workspace: config.git.main_workspace.clone(),
        commit,
        short_commit,
        target: None,
        base: None,
        previous_workspace: None,
        trigger_source: "cli".to_string(),
        vcs_event: None,
        service,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_simple_hook() {
        let yaml = r#""npm ci""#;
        let entry: HookEntry = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(matches!(entry, HookEntry::Simple(ref s) if s == "npm ci"));
    }

    #[test]
    fn test_deserialize_extended_hook() {
        let yaml = r#"
command: "npm test"
condition: "file_exists:package.json"
continue_on_error: true
background: false
"#;
        let entry: HookEntry = serde_yaml_ng::from_str(yaml).unwrap();
        match entry {
            HookEntry::Extended(ext) => {
                assert_eq!(ext.command, "npm test");
                assert_eq!(ext.condition, Some("file_exists:package.json".to_string()));
                assert_eq!(ext.continue_on_error, Some(true));
                assert!(!ext.background);
            }
            other => panic!("Expected Extended, got {:?}", other),
        }
    }

    #[test]
    fn test_deserialize_action_write_env() {
        let yaml = r#"
action:
  type: write-env
  path: ".env.local"
  vars:
    DATABASE_URL: "{{ service['app-db'].url }}"
    REDIS_URL: "{{ service['cache'].url }}"
"#;
        let entry: HookEntry = serde_yaml_ng::from_str(yaml).unwrap();
        match entry {
            HookEntry::Action(act) => match &act.action {
                HookAction::WriteEnv { path, vars, .. } => {
                    assert_eq!(path, ".env.local");
                    assert_eq!(vars.len(), 2);
                    assert!(vars.contains_key("DATABASE_URL"));
                }
                other => panic!("Expected WriteEnv action, got {:?}", other),
            },
            other => panic!("Expected Action, got {:?}", other),
        }
    }

    #[test]
    fn test_deserialize_action_replace() {
        let yaml = r#"
action:
  type: replace
  file: "config/database.yml"
  pattern: "database: \\w+"
  replacement: "database: mydb"
  regex: true
"#;
        let entry: HookEntry = serde_yaml_ng::from_str(yaml).unwrap();
        match entry {
            HookEntry::Action(act) => match &act.action {
                HookAction::Replace { file, regex, .. } => {
                    assert_eq!(file, "config/database.yml");
                    assert!(*regex);
                }
                other => panic!("Expected Replace action, got {:?}", other),
            },
            other => panic!("Expected Action, got {:?}", other),
        }
    }

    #[test]
    fn test_deserialize_action_shell() {
        let yaml = r#"
action:
  type: shell
  command: "npm ci"
condition: "file_exists:package.json"
"#;
        let entry: HookEntry = serde_yaml_ng::from_str(yaml).unwrap();
        match entry {
            HookEntry::Action(act) => {
                assert!(act.action.requires_approval());
                assert_eq!(act.condition, Some("file_exists:package.json".to_string()));
                match &act.action {
                    HookAction::Shell { command } => assert_eq!(command, "npm ci"),
                    other => panic!("Expected Shell action, got {:?}", other),
                }
            }
            other => panic!("Expected Action, got {:?}", other),
        }
    }

    #[test]
    fn test_deserialize_action_http() {
        let yaml = r#"
action:
  type: http
  url: "https://hooks.slack.com/services/XXX"
  method: POST
  headers:
    Content-Type: "application/json"
  body: '{"text": "hello"}'
"#;
        let entry: HookEntry = serde_yaml_ng::from_str(yaml).unwrap();
        match entry {
            HookEntry::Action(act) => {
                assert!(!act.action.requires_approval());
                match &act.action {
                    HookAction::Http {
                        url,
                        method,
                        headers,
                        body,
                    } => {
                        assert_eq!(url, "https://hooks.slack.com/services/XXX");
                        assert_eq!(method, "POST");
                        assert!(headers.is_some());
                        assert!(body.is_some());
                    }
                    other => panic!("Expected Http action, got {:?}", other),
                }
            }
            other => panic!("Expected Action, got {:?}", other),
        }
    }

    #[test]
    fn test_deserialize_action_notify() {
        let yaml = r#"
action:
  type: notify
  title: "devflow"
  message: "Ready"
  level: success
"#;
        let entry: HookEntry = serde_yaml_ng::from_str(yaml).unwrap();
        match entry {
            HookEntry::Action(act) => match &act.action {
                HookAction::Notify {
                    title,
                    message,
                    level,
                } => {
                    assert_eq!(title, "devflow");
                    assert_eq!(message, "Ready");
                    assert!(matches!(level, NotifyLevel::Success));
                }
                other => panic!("Expected Notify action, got {:?}", other),
            },
            other => panic!("Expected Action, got {:?}", other),
        }
    }

    #[test]
    fn test_deserialize_action_copy() {
        let yaml = r#"
action:
  type: copy
  from: ".env.example"
  to: ".env.local"
"#;
        let entry: HookEntry = serde_yaml_ng::from_str(yaml).unwrap();
        match entry {
            HookEntry::Action(act) => {
                match &act.action {
                    HookAction::Copy {
                        from,
                        to,
                        overwrite,
                    } => {
                        assert_eq!(from, ".env.example");
                        assert_eq!(to, ".env.local");
                        assert!(*overwrite); // default true
                    }
                    other => panic!("Expected Copy action, got {:?}", other),
                }
            }
            other => panic!("Expected Action, got {:?}", other),
        }
    }

    #[test]
    fn test_deserialize_action_docker_exec() {
        let yaml = r#"
action:
  type: docker-exec
  container: myapp-postgres
  command: "psql -U postgres -c 'SELECT 1'"
  user: postgres
"#;
        let entry: HookEntry = serde_yaml_ng::from_str(yaml).unwrap();
        match entry {
            HookEntry::Action(act) => {
                assert!(act.action.requires_approval());
                match &act.action {
                    HookAction::DockerExec {
                        container,
                        command,
                        user,
                    } => {
                        assert_eq!(container, "myapp-postgres");
                        assert_eq!(command, "psql -U postgres -c 'SELECT 1'");
                        assert_eq!(user.as_deref(), Some("postgres"));
                    }
                    other => panic!("Expected DockerExec action, got {:?}", other),
                }
            }
            other => panic!("Expected Action, got {:?}", other),
        }
    }

    #[test]
    fn test_backward_compat_full_hooks_config() {
        let yaml = r#"
post-create:
  install: "npm ci"
  env:
    command: "echo DATABASE_URL=test > .env.local"
    condition: "file_exists:package.json"
  write-env:
    action:
      type: write-env
      path: ".env.local"
      vars:
        DATABASE_URL: "postgresql://localhost/mydb"
post-switch:
  update-env: "echo hello"
"#;
        let config: HooksConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.len(), 2);

        let post_create = config.get(&HookPhase::PostCreate).unwrap();
        assert_eq!(post_create.len(), 3);
        assert!(matches!(
            post_create.get("install"),
            Some(HookEntry::Simple(_))
        ));
        assert!(matches!(
            post_create.get("env"),
            Some(HookEntry::Extended(_))
        ));
        assert!(matches!(
            post_create.get("write-env"),
            Some(HookEntry::Action(_))
        ));

        let post_switch = config.get(&HookPhase::PostSwitch).unwrap();
        assert_eq!(post_switch.len(), 1);
        assert!(matches!(
            post_switch.get("update-env"),
            Some(HookEntry::Simple(s)) if s == "echo hello"
        ));
    }

    #[test]
    fn test_requires_approval() {
        let simple = HookEntry::Simple("echo hi".to_string());
        assert!(simple.requires_approval());

        let extended = HookEntry::Extended(ExtendedHookEntry {
            command: "npm test".to_string(),
            working_dir: None,
            continue_on_error: None,
            condition: None,
            environment: None,
            background: false,
        });
        assert!(extended.requires_approval());

        let action_shell = HookEntry::Action(ActionHookEntry {
            action: HookAction::Shell {
                command: "echo hi".to_string(),
            },
            working_dir: None,
            continue_on_error: None,
            condition: None,
            environment: None,
            background: false,
        });
        assert!(action_shell.requires_approval());

        let action_write_env = HookEntry::Action(ActionHookEntry {
            action: HookAction::WriteEnv {
                path: ".env".to_string(),
                vars: IndexMap::new(),
                mode: EnvWriteMode::Overwrite,
            },
            working_dir: None,
            continue_on_error: None,
            condition: None,
            environment: None,
            background: false,
        });
        assert!(!action_write_env.requires_approval());

        let action_docker = HookEntry::Action(ActionHookEntry {
            action: HookAction::DockerExec {
                container: "test".to_string(),
                command: "echo".to_string(),
                user: None,
            },
            working_dir: None,
            continue_on_error: None,
            condition: None,
            environment: None,
            background: false,
        });
        assert!(action_docker.requires_approval());
    }

    #[test]
    fn test_hook_action_type_name() {
        assert_eq!(
            HookAction::Shell {
                command: "".to_string()
            }
            .type_name(),
            "shell"
        );
        assert_eq!(
            HookAction::Replace {
                file: "".to_string(),
                pattern: "".to_string(),
                replacement: "".to_string(),
                regex: false,
                create_if_missing: false
            }
            .type_name(),
            "replace"
        );
        assert_eq!(
            HookAction::WriteFile {
                path: "".to_string(),
                content: "".to_string(),
                mode: WriteMode::Overwrite
            }
            .type_name(),
            "write-file"
        );
        assert_eq!(
            HookAction::WriteEnv {
                path: "".to_string(),
                vars: IndexMap::new(),
                mode: EnvWriteMode::Overwrite
            }
            .type_name(),
            "write-env"
        );
        assert_eq!(
            HookAction::Copy {
                from: "".to_string(),
                to: "".to_string(),
                overwrite: true
            }
            .type_name(),
            "copy"
        );
        assert_eq!(
            HookAction::DockerExec {
                container: "".to_string(),
                command: "".to_string(),
                user: None
            }
            .type_name(),
            "docker-exec"
        );
        assert_eq!(
            HookAction::Http {
                url: "".to_string(),
                method: "GET".to_string(),
                body: None,
                headers: None
            }
            .type_name(),
            "http"
        );
        assert_eq!(
            HookAction::Notify {
                title: "".to_string(),
                message: "".to_string(),
                level: NotifyLevel::Info
            }
            .type_name(),
            "notify"
        );
    }
}

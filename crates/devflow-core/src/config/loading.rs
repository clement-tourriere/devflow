use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use super::{Config, EffectiveConfig, EnvConfig, GlobalConfig, LocalConfig, NamedServiceConfig};

impl Config {
    /// Return the project name, falling back to the directory name of the
    /// config file (or the current working directory).
    pub fn project_name(&self) -> String {
        if let Some(ref name) = self.name {
            return name.clone();
        }
        // Derive the name from the canonical MAIN-repo root, not the raw cwd:
        // a worktree's directory basename (e.g. `repo.feature-x`) would
        // otherwise become a different project identity than the main repo,
        // spawning a parallel empty project and cross-cloning data.
        // Prefer the directory the config was loaded from — GUI/daemon
        // processes have an arbitrary cwd (e.g. "/" when launched by Finder).
        self.project_root
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .map(|d| crate::vcs::resolve_project_root(&d))
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

        // Parse by extension: TOML for the lightweight `devflow.toml` form,
        // YAML otherwise. Both deserialize into the same Config via serde.
        let is_toml = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("toml"));

        let mut config: Config = if is_toml {
            toml::from_str(&content)
                .with_context(|| format!("Failed to parse TOML config file: {}", path.display()))?
        } else {
            serde_yaml_ng::from_str(&content)
                .with_context(|| format!("Failed to parse YAML config file: {}", path.display()))?
        };

        // Remember where this project lives. `parent()` of a relative path
        // like ".devflow.yml" is "", which must not shadow the cwd fallback.
        config.project_root = path
            .canonicalize()
            .ok()
            .as_deref()
            .unwrap_or(path)
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(Path::to_path_buf);

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
            // YAML first (the full-featured form), then the lightweight TOML
            // form (`devflow.toml` or `.devflow.toml`).
            for filename in [
                ".devflow.yml",
                ".devflow.yaml",
                ".devflow.toml",
                "devflow.toml",
            ] {
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

    pub fn get_normalized_workspace_name(&self, workspace_name: &str) -> String {
        Self::sanitize_workspace_name(workspace_name)
    }

    fn sanitize_workspace_name(workspace_name: &str) -> String {
        normalize_workspace_name(workspace_name)
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

    /// Resolve the list of named services from the `services` config.
    pub fn resolve_services(&self) -> Vec<NamedServiceConfig> {
        if let Some(ref services) = self.services {
            services.clone()
        } else {
            vec![]
        }
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

    pub fn remove_service(&mut self, name: &str) {
        if let Some(ref mut services) = self.services {
            services.retain(|b| b.name != name);
        }
    }

    /// Locate a committed project config file inside `dir` (YAML or TOML).
    pub fn find_config_file_in(dir: &Path) -> Option<PathBuf> {
        [
            ".devflow.yml",
            ".devflow.yaml",
            ".devflow.toml",
            "devflow.toml",
        ]
        .iter()
        .map(|name| dir.join(name))
        .find(|path| path.exists())
    }

    /// Load the full effective configuration for a specific project directory:
    /// committed config + global config + local overrides + env overrides,
    /// with local-state services overlaid (merged by name).
    ///
    /// This is the one entry point every frontend that addresses a project by
    /// path (GUI, daemon, controller) must use, so they cannot drift from the
    /// cwd-discovering CLI/TUI path in `load_effective_config_with_path_info`.
    pub fn load_effective_for_dir(project_dir: &Path) -> Result<Config> {
        let config_path = Self::find_config_file_in(project_dir);
        let config = match config_path.as_deref() {
            Some(path) => Self::from_file(path)?,
            None => Config {
                project_root: Some(
                    project_dir
                        .canonicalize()
                        .unwrap_or_else(|_| project_dir.to_path_buf()),
                ),
                ..Default::default()
            },
        };

        let global_config = GlobalConfig::load()?;

        // Local config lives next to the committed config; in a worktree it
        // falls back to the main worktree (mirrors the cwd-based loader).
        let mut local_config = LocalConfig::load_from_project_dir(project_dir)?;
        if local_config.is_none() {
            if let Ok(vcs_repo) = crate::vcs::detect_vcs_provider(project_dir) {
                if vcs_repo.is_worktree() {
                    if let Some(main_dir) = vcs_repo.main_worktree_dir() {
                        local_config = LocalConfig::load_from_project_dir(&main_dir)?;
                    }
                }
            }
        }

        let env_config = EnvConfig::load_from_env()?;
        let effective = EffectiveConfig::new(config, global_config, local_config, env_config)?;
        let mut merged = effective.get_merged_config();

        let overlay_key = config_path.unwrap_or_else(|| project_dir.join(".devflow.yml"));
        merged.overlay_local_state_services(&overlay_key);
        Ok(merged)
    }

    /// Committed + local-state services, each tagged with its origin.
    ///
    /// Local-state entries (CLI-managed) override committed entries with the
    /// same name; committed entries without an override are kept. At most one
    /// service keeps `default = true`.
    pub fn services_with_sources(&self, config_path: &Path) -> Vec<super::ServiceWithSource> {
        let mut services: Vec<super::ServiceWithSource> = self
            .resolve_services()
            .into_iter()
            .map(|service| super::ServiceWithSource {
                service,
                source: super::ServiceSource::Config,
            })
            .collect();

        if let Ok(state) = crate::state::LocalStateManager::new() {
            if let Some(state_services) = state.get_services(config_path) {
                for service in state_services {
                    if let Some(existing) = services
                        .iter_mut()
                        .find(|entry| entry.service.name == service.name)
                    {
                        existing.service = service;
                        existing.source = super::ServiceSource::LocalState;
                    } else {
                        services.push(super::ServiceWithSource {
                            service,
                            source: super::ServiceSource::LocalState,
                        });
                    }
                }
            }
        }

        let mut seen_default = false;
        for entry in &mut services {
            if entry.service.default {
                if seen_default {
                    entry.service.default = false;
                } else {
                    seen_default = true;
                }
            }
        }
        services
    }

    /// Merge local-state services into `self.services` (see
    /// [`Config::services_with_sources`] for the semantics).
    pub fn overlay_local_state_services(&mut self, config_path: &Path) {
        let services: Vec<NamedServiceConfig> = self
            .services_with_sources(config_path)
            .into_iter()
            .map(|entry| entry.service)
            .collect();
        if !services.is_empty() {
            self.services = Some(services);
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

/// Canonical service-workspace name for a raw VCS branch/workspace name.
///
/// Service backends key their per-workspace state by this normalized form
/// (the switch pipeline normalizes before orchestration). Lookups must apply
/// the same normalization so raw branch names like `feature/foo-bar` resolve
/// to the stored workspace.
pub(crate) fn normalize_workspace_name(workspace_name: &str) -> String {
    // Convert to lowercase and replace invalid characters with underscores
    let mut sanitized = String::new();

    for ch in workspace_name.to_lowercase().chars() {
        match ch {
            'a'..='z' | '0'..='9' | '_' | '$' => sanitized.push(ch),
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

    if sanitized.is_empty() {
        sanitized = "workspace".to_string();
    }

    sanitized
}

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
        std::env::current_dir()
            .ok()
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

        let config: Config = if is_toml {
            toml::from_str(&content)
                .with_context(|| format!("Failed to parse TOML config file: {}", path.display()))?
        } else {
            serde_yaml_ng::from_str(&content)
                .with_context(|| format!("Failed to parse YAML config file: {}", path.display()))?
        };

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

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use super::{Config, EffectiveConfig, EnvConfig, GlobalConfig, LocalConfig, NamedServiceConfig};

#[derive(Debug)]
struct WorkspaceConfigDirs {
    current: PathBuf,
    primary: PathBuf,
}

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
        let current_dir = std::env::current_dir().context("Failed to get current directory")?;
        Self::load_project_config_for_start(&current_dir, true)
    }

    fn load_project_config_for_start(
        start: &Path,
        search_ancestors: bool,
    ) -> Result<(Self, Option<PathBuf>)> {
        if let Some(config_path) = Self::resolve_config_path(start, search_ancestors) {
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

        // Legacy key written by every pre-worktree-always-on release (and by
        // `devflow init` itself). Accept it so committed configs keep parsing,
        // but tell the user it no longer does anything.
        if let Some(enabled) = config.worktree.enabled.take() {
            if enabled {
                log::warn!(
                    "{}: `worktree.enabled` was removed — worktrees are always enabled; delete this line",
                    path.display()
                );
            } else {
                log::warn!(
                    "{}: `worktree.enabled: false` is no longer supported — worktrees are always enabled and this setting is ignored; delete this line",
                    path.display()
                );
            }
        }

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
        let current_dir = std::env::current_dir().context("Failed to get current directory")?;
        Ok(Self::resolve_config_path(&current_dir, true))
    }

    fn find_config_file_upwards(start: &Path) -> Option<PathBuf> {
        let mut current_dir = start.to_path_buf();

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
                    return Some(config_path);
                }
            }

            if let Some(parent) = current_dir.parent() {
                current_dir = parent.to_path_buf();
            } else {
                break;
            }
        }

        None
    }

    /// Search from `start` through `boundary`, without escaping into an
    /// unrelated containing project. Both paths are expected to exist; the
    /// non-canonical fallbacks keep discovery useful for unusual filesystems.
    fn find_config_file_upwards_within(start: &Path, boundary: &Path) -> Option<PathBuf> {
        let mut current_dir = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
        let boundary = boundary
            .canonicalize()
            .unwrap_or_else(|_| boundary.to_path_buf());

        if !current_dir.starts_with(&boundary) {
            return None;
        }

        loop {
            if let Some(config_path) = Self::find_config_file_in(&current_dir) {
                return Some(config_path);
            }
            if current_dir == boundary {
                break;
            }
            let Some(parent) = current_dir.parent() else {
                break;
            };
            if !parent.starts_with(&boundary) {
                break;
            }
            current_dir = parent.to_path_buf();
        }

        None
    }

    /// Resolve the current materialized workspace and the primary checkout.
    ///
    /// The current directory may be nested below a linked worktree, while
    /// `repo_root()` deliberately identifies the primary checkout. Git's
    /// discovery metadata (and provider inventory for other VCSes) gives us
    /// both roots.
    fn workspace_config_dirs(start: &Path) -> Option<WorkspaceConfigDirs> {
        // `GitRepository::new` intentionally opens a worktree root directly,
        // while config discovery also needs to work from arbitrary nested
        // directories. libgit2's discovery API gives us that current root and
        // the shared primary checkout without changing provider semantics.
        if crate::vcs::detect_vcs_kind(start) == Some(crate::vcs::VcsKind::Git) {
            if let Ok(repo) = git2::Repository::discover(start) {
                if repo.is_worktree() {
                    let current = repo.workdir()?.canonicalize().ok()?;
                    let common = repo.commondir();
                    // A worktree of a BARE repo has no primary checkout:
                    // commondir() IS the bare repo directory, so its parent is
                    // whatever directory happens to contain the bare repo —
                    // treating that as "primary" would silently adopt an
                    // unrelated project's config living next to it.
                    if git2::Repository::open(common)
                        .map(|main| main.is_bare())
                        .unwrap_or(false)
                    {
                        return None;
                    }
                    let primary = common.parent()?.canonicalize().ok()?;
                    return Some(WorkspaceConfigDirs { current, primary });
                }
            }
        }

        let repo = crate::vcs::detect_vcs_provider(start).ok()?;
        if !repo.is_worktree() {
            return None;
        }

        let primary = repo.main_worktree_dir()?;
        let primary = primary.canonicalize().unwrap_or(primary);
        let canonical_start = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
        let current = repo
            .list_worktrees()
            .ok()
            .and_then(|worktrees| {
                worktrees
                    .into_iter()
                    .filter(|worktree| !worktree.is_main)
                    .filter_map(|worktree| {
                        let path = worktree.path.canonicalize().unwrap_or(worktree.path);
                        canonical_start.starts_with(&path).then_some(path)
                    })
                    .max_by_key(|path| path.components().count())
            })
            .unwrap_or(canonical_start);

        Some(WorkspaceConfigDirs { current, primary })
    }

    /// Prefer configuration materialized in the current linked workspace.
    /// If it has none, use the primary checkout's project config. This is
    /// especially important immediately after `devflow init`, where the
    /// generated `.devflow.yml` can still be untracked and therefore absent
    /// from newly-created linked worktrees.
    fn resolve_config_path(start: &Path, search_ancestors: bool) -> Option<PathBuf> {
        let workspace_dirs = Self::workspace_config_dirs(start);
        let local = match (search_ancestors, workspace_dirs.as_ref()) {
            // A linked workspace is a project boundary. Searching beyond its
            // root can select an unrelated ancestor config before we get the
            // chance to use this repository's primary-checkout fallback.
            (true, Some(dirs)) => Self::find_config_file_upwards_within(start, &dirs.current),
            (true, None) => Self::find_config_file_upwards(start),
            (false, _) => Self::find_config_file_in(start),
        };
        if local.is_some() {
            return local;
        }

        let dirs = workspace_dirs?;
        let fallback = Self::find_config_file_in(&dirs.primary);
        if let Some(path) = &fallback {
            log::debug!(
                "Using project config from primary worktree: {}",
                path.display()
            );
        }
        fallback
    }

    /// Load the local override that lives beside the discovered project
    /// config first (nested/monorepo layouts), then the workspace root, then
    /// the primary checkout. Outside a linked workspace, preserve the
    /// traditional rule that local config lives beside the project config.
    fn load_local_config_for_start(
        start: &Path,
        config_path: Option<&Path>,
    ) -> Result<Option<LocalConfig>> {
        if let Some(dirs) = Self::workspace_config_dirs(start) {
            // A nested project config (e.g. apps/api/.devflow.yml) keeps its
            // .devflow.local.yml next to it in the primary checkout — the
            // same must hold inside a linked worktree.
            let config_dir = config_path
                .and_then(Path::parent)
                .map(|dir| dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()));
            let mut local = None;
            if let Some(config_dir) = config_dir.as_deref() {
                if config_dir != dirs.current && config_dir != dirs.primary {
                    local = LocalConfig::load_from_project_dir(config_dir)?;
                }
            }
            if local.is_none() {
                local = LocalConfig::load_from_project_dir(&dirs.current)?;
            }
            if local.is_none() && dirs.current != dirs.primary {
                local = LocalConfig::load_from_project_dir(&dirs.primary)?;
            }
            return Ok(local);
        }

        let local_dir = config_path.and_then(Path::parent).unwrap_or(start);
        LocalConfig::load_from_project_dir(local_dir)
    }

    /// Return the collision-resistant key used by service backends and
    /// worktree path templates for a raw VCS workspace name.
    pub fn get_service_workspace_key(&self, workspace_name: &str) -> String {
        workspace_service_key(workspace_name)
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
        let config_path = Self::resolve_config_path(project_dir, false);
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

        let local_config = Self::load_local_config_for_start(project_dir, config_path.as_deref())?;

        let env_config = EnvConfig::load_from_env()?;
        let effective = EffectiveConfig::new(config, global_config, local_config, env_config)?;
        let mut merged = effective.get_merged_config();

        let overlay_key = config_path
            .unwrap_or_else(|| crate::vcs::resolve_project_root(project_dir).join(".devflow.yml"));
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
        let current_dir = std::env::current_dir().context("Failed to get current directory")?;
        Self::load_effective_config_for_start(&current_dir)
    }

    fn load_effective_config_for_start(start: &Path) -> Result<(EffectiveConfig, Option<PathBuf>)> {
        // Load global user config (~/.config/devflow/config.yml)
        let global_config = GlobalConfig::load()?;

        // Load main config
        let (config, config_path) = Self::load_project_config_for_start(start, true)?;

        let local_config = Self::load_local_config_for_start(start, config_path.as_deref())?;

        // Load environment config
        let env_config = EnvConfig::load_from_env()?;

        // Create effective config
        let effective_config =
            EffectiveConfig::new(config, global_config, local_config, env_config)?;

        Ok((effective_config, config_path))
    }
}

/// Read `git.main_workspace` for a project only when the key is literally
/// present in the committed config file (including its legacy `main_branch`
/// alias) or in the local override. `git.main_workspace` is serde-defaulted
/// to "main"; treating that default as an explicit choice would hard-fail
/// every repo whose default branch/bookmark is e.g. `master` or `trunk` as
/// soon as any config file exists.
pub(crate) fn explicit_main_workspace_for_dir(project_root: &Path) -> Result<Option<String>> {
    let Some(config_path) = Config::find_config_file_in(project_root) else {
        return Ok(None);
    };
    let mut configured = explicit_main_workspace(&config_path)?;
    if let Some(local) = LocalConfig::load_from_project_dir(project_root)? {
        if let Some(local_default) = local.git.and_then(|git| git.main_workspace) {
            configured = Some(local_default);
        }
    }
    Ok(configured)
}

fn explicit_main_workspace(config_path: &Path) -> Result<Option<String>> {
    #[derive(Default, serde::Deserialize)]
    struct ProbeGit {
        #[serde(default, alias = "main_branch")]
        main_workspace: Option<String>,
    }
    #[derive(Default, serde::Deserialize)]
    struct Probe {
        #[serde(default)]
        git: Option<ProbeGit>,
    }

    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read config file: {}", config_path.display()))?;
    let is_toml = config_path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("toml"));
    let probe: Probe = if is_toml {
        toml::from_str(&content).with_context(|| {
            format!(
                "Failed to parse TOML config file: {}",
                config_path.display()
            )
        })?
    } else {
        serde_yaml_ng::from_str(&content).with_context(|| {
            format!(
                "Failed to parse YAML config file: {}",
                config_path.display()
            )
        })?
    };
    Ok(probe.git.and_then(|git| git.main_workspace))
}

/// Canonical service-workspace name for a raw VCS branch/workspace name.
///
/// Service backends key their per-workspace state by this normalized form
/// (the switch pipeline normalizes before orchestration). Lookups must apply
/// the same normalization so raw branch names like `feature/foo-bar` resolve
/// to the stored workspace.
pub(crate) fn normalize_workspace_name(workspace_name: &str) -> String {
    workspace_service_key(workspace_name)
}

/// Build a stable service/filesystem key without conflating distinct VCS
/// names. Names already valid for service backends are preserved for
/// backwards compatibility; names requiring normalization receive a short
/// digest of the exact raw name.
pub fn workspace_service_key(workspace_name: &str) -> String {
    use sha2::{Digest, Sha256};

    let normalized = backend_normalize_workspace_name(workspace_name);
    const MAX_SERVICE_KEY_LEN: usize = 63;
    const HASH_HEX_LEN: usize = 12;

    if normalized == workspace_name && normalized.len() <= MAX_SERVICE_KEY_LEN {
        return normalized;
    }

    let digest = Sha256::digest(workspace_name.as_bytes());
    let suffix = digest[..HASH_HEX_LEN / 2]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let prefix_len = MAX_SERVICE_KEY_LEN - 1 - HASH_HEX_LEN;
    let prefix = normalized[..normalized.len().min(prefix_len)].trim_end_matches('_');
    format!("{prefix}_{suffix}")
}

/// Normalize to the intersection accepted by every built-in backend. Keeping
/// this alphabet narrower than any one provider prevents a later database or
/// bucket sanitizer from conflating two already-issued service keys.
fn backend_normalize_workspace_name(workspace_name: &str) -> String {
    let mut sanitized = String::with_capacity(workspace_name.len());
    let mut last_underscore = false;

    for ch in workspace_name.to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            sanitized.push(ch);
            last_underscore = false;
        } else if !last_underscore {
            sanitized.push('_');
            last_underscore = true;
        }
    }

    let mut sanitized = sanitized.trim_end_matches('_').to_string();
    if sanitized.starts_with(|ch: char| ch.is_ascii_digit()) {
        sanitized.insert(0, '_');
    }
    if sanitized.is_empty() {
        sanitized = "workspace".to_string();
    }
    sanitized
}

/// Lossy normalization used only to migrate state written before service keys
/// became collision-resistant.
pub(crate) fn legacy_normalize_workspace_name(workspace_name: &str) -> String {
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

#[cfg(test)]
mod workspace_key_tests {
    use super::workspace_service_key;

    #[test]
    fn service_keys_preserve_safe_names_and_separate_lossy_collisions() {
        assert_eq!(workspace_service_key("feature_auth"), "feature_auth");
        assert_ne!(
            workspace_service_key("feature/auth"),
            workspace_service_key("feature-auth")
        );
        assert_ne!(
            workspace_service_key("Feature/Auth"),
            workspace_service_key("feature/auth")
        );
        assert_ne!(
            workspace_service_key("feature$auth"),
            workspace_service_key("feature_auth")
        );
    }

    #[test]
    fn service_keys_are_deterministic_and_bounded() {
        let raw = format!("feature/{}", "very-long-component-".repeat(8));
        let first = workspace_service_key(&raw);
        let second = workspace_service_key(&raw);

        assert_eq!(first, second);
        assert!(first.len() <= 63, "key was {} bytes: {first}", first.len());
        assert_eq!(first.rsplit('_').next().unwrap().len(), 12);
    }
}

#[cfg(test)]
mod linked_worktree_config_tests {
    use super::Config;
    use crate::vcs::{GitRepository, VcsProvider};

    #[test]
    fn effective_loaders_fall_back_to_primary_and_preserve_linked_overrides() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("primary");
        let linked = temp.path().join("linked");
        std::fs::create_dir_all(&primary).unwrap();

        let repo = GitRepository::init(&primary).unwrap();
        std::fs::write(
            primary.join(".devflow.yml"),
            "name: primary-project\nbehavior:\n  max_workspaces: 10\n",
        )
        .unwrap();
        std::fs::write(
            primary.join(".devflow.local.yml"),
            "behavior:\n  max_workspaces: 11\n",
        )
        .unwrap();
        // A config belonging to a containing project must not win over this
        // linked workspace's primary-checkout fallback.
        std::fs::write(
            temp.path().join(".devflow.yml"),
            "name: unrelated-parent-project\nbehavior:\n  max_workspaces: 99\n",
        )
        .unwrap();
        repo.create_worktree("feature/linked", &linked).unwrap();

        // The primary config was created after the initial commit, so it is
        // intentionally absent from the new linked worktree.
        assert!(!linked.join(".devflow.yml").exists());
        let explicit = Config::load_effective_for_dir(&linked).unwrap();
        assert_eq!(explicit.name.as_deref(), Some("primary-project"));
        assert_eq!(explicit.behavior.max_workspaces, Some(11));

        let nested = linked.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let (cwd_style, path) = Config::load_effective_config_for_start(&nested).unwrap();
        let cwd_style = cwd_style.get_merged_config();
        assert_eq!(cwd_style.name.as_deref(), Some("primary-project"));
        assert_eq!(cwd_style.behavior.max_workspaces, Some(11));
        assert_eq!(
            path.unwrap().canonicalize().unwrap(),
            primary.join(".devflow.yml").canonicalize().unwrap()
        );

        // A linked-worktree local override has higher priority than the
        // primary checkout's local config, even while the project config
        // itself still falls back to the primary checkout.
        std::fs::write(
            linked.join(".devflow.local.yml"),
            "behavior:\n  max_workspaces: 22\n",
        )
        .unwrap();
        let explicit = Config::load_effective_for_dir(&linked).unwrap();
        assert_eq!(explicit.name.as_deref(), Some("primary-project"));
        assert_eq!(explicit.behavior.max_workspaces, Some(22));

        // If the linked worktree materializes its own project config, that
        // config wins over the primary fallback as well.
        std::fs::write(
            linked.join(".devflow.yml"),
            "name: linked-project\nbehavior:\n  max_workspaces: 20\n",
        )
        .unwrap();
        let (cwd_style, path) = Config::load_effective_config_for_start(&nested).unwrap();
        let cwd_style = cwd_style.get_merged_config();
        assert_eq!(cwd_style.name.as_deref(), Some("linked-project"));
        assert_eq!(cwd_style.behavior.max_workspaces, Some(22));
        assert_eq!(
            path.unwrap().canonicalize().unwrap(),
            linked.join(".devflow.yml").canonicalize().unwrap()
        );
    }
}

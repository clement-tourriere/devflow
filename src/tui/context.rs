use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

use devflow_core::config::{Config, NamedServiceConfig};
use devflow_core::hooks::HookEntry;
use devflow_core::services::factory;
use devflow_core::state::{DevflowBranch, LocalStateManager};
use devflow_core::vcs::{self, BranchInfo, VcsProvider, WorktreeInfo};

use super::action::*;

/// Snapshot of VCS data captured synchronously from the main thread.
/// All fields are `Send + Clone` so they can be passed to background tasks.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct VcsSnapshot {
    pub branches: Vec<BranchInfo>,
    pub current_branch: Option<String>,
    pub default_branch: Option<String>,
    pub supports_worktrees: bool,
    pub worktrees: Vec<WorktreeInfo>,
}

/// Shared context that the TUI components use to fetch data.
/// Encapsulates config loading, VCS detection, and provider creation.
///
/// VCS operations run synchronously (they're local + fast).
/// Provider/network operations are exposed as static `_bg()` methods
/// that take a `Config` clone and run on background tasks.
#[allow(dead_code)]
pub struct DevflowContext {
    pub config: Config,
    pub config_path: Option<PathBuf>,
    vcs: Box<dyn VcsProvider>,
    vcs_snapshot: VcsSnapshot,
}

#[allow(dead_code)]
impl DevflowContext {
    /// Load config, inject state services, detect VCS, snapshot VCS data.
    pub fn new() -> Result<Self> {
        let (effective_config, config_path) = Config::load_effective_config_with_path_info()?;
        let mut config = effective_config.get_merged_config();

        // Inject services from local state
        if let Ok(state_manager) = LocalStateManager::new() {
            if let Some(ref path) = config_path {
                if let Some(state_services) = state_manager.get_services(path) {
                    config.services = Some(state_services);
                }
            }
        }

        let vcs = vcs::detect_vcs_provider(".")?;

        // Capture VCS data snapshot (all sync, fast)
        let vcs_snapshot = Self::take_vcs_snapshot(&*vcs);

        Ok(Self {
            config,
            config_path,
            vcs,
            vcs_snapshot,
        })
    }

    /// Take a snapshot of VCS state. All calls here are synchronous.
    fn take_vcs_snapshot(vcs: &dyn VcsProvider) -> VcsSnapshot {
        let branches = vcs.list_branches().unwrap_or_default();
        let current_branch = vcs.current_branch().ok().flatten();
        let default_branch = vcs.default_branch().ok().flatten();
        let supports_worktrees = vcs.supports_worktrees();
        let worktrees = if supports_worktrees {
            vcs.list_worktrees().unwrap_or_default()
        } else {
            Vec::new()
        };
        VcsSnapshot {
            branches,
            current_branch,
            default_branch,
            supports_worktrees,
            worktrees,
        }
    }

    /// Return a clone of the current VCS snapshot for use in background tasks.
    pub fn snapshot_vcs_data(&self) -> VcsSnapshot {
        self.vcs_snapshot.clone()
    }

    /// Snapshot the branch registry (name -> parent) for use in background tasks.
    /// Returns a HashMap<branch_name, Option<parent_name>> from the local state.
    pub fn snapshot_branch_registry(&self) -> HashMap<String, Option<String>> {
        let mut map = HashMap::new();
        if let Ok(state_manager) = LocalStateManager::new() {
            if let Some(ref path) = self.config_path {
                let branches = state_manager.get_branches(path);
                for branch in branches {
                    map.insert(branch.name, branch.parent);
                }
            }
        }
        map
    }

    /// Snapshot the current devflow context branch.
    ///
    /// Resolution order:
    /// 1) DEVFLOW_CONTEXT_BRANCH env override
    /// 2) current VCS branch in this cwd
    pub fn snapshot_context_branch(&self) -> Option<String> {
        if let Ok(override_branch) = std::env::var("DEVFLOW_CONTEXT_BRANCH") {
            let trimmed = override_branch.trim();
            if !trimmed.is_empty() {
                return Some(self.config.get_normalized_branch_name(trimmed));
            }
        }

        self.vcs_snapshot
            .current_branch
            .as_deref()
            .map(|b| self.config.get_normalized_branch_name(b))
    }

    /// Re-capture VCS state after a branch switch/create/delete.
    pub fn refresh_vcs_snapshot(&mut self) {
        self.vcs_snapshot = Self::take_vcs_snapshot(&*self.vcs);
    }

    fn upsert_branch_state(&self, branch_name: &str, parent: Option<&str>) {
        let Some(config_path) = self.config_path.as_ref() else {
            return;
        };

        let normalized_branch = self.config.get_normalized_branch_name(branch_name);
        let normalized_parent = parent.map(|p| self.config.get_normalized_branch_name(p));

        match LocalStateManager::new() {
            Ok(mut state) => {
                let existing = state.get_branch(config_path, &normalized_branch);

                let worktree_path = self
                    .vcs
                    .worktree_path(branch_name)
                    .ok()
                    .flatten()
                    .map(|p| p.display().to_string())
                    .or_else(|| {
                        existing
                            .as_ref()
                            .and_then(|b| b.worktree_path.as_ref().cloned())
                    });

                let created_at = existing
                    .as_ref()
                    .map(|b| b.created_at)
                    .unwrap_or_else(chrono::Utc::now);

                let parent = existing
                    .as_ref()
                    .and_then(|b| b.parent.clone())
                    .or(normalized_parent);

                let branch = DevflowBranch {
                    name: normalized_branch,
                    parent,
                    worktree_path,
                    created_at,
                    agent_tool: None,
                    agent_status: None,
                    agent_started_at: None,
                };

                if let Err(e) = state.register_branch(config_path, branch) {
                    log::warn!("Failed to register branch in local state: {}", e);
                }
            }
            Err(e) => {
                log::warn!("Failed to open local state manager: {}", e);
            }
        }
    }

    fn remove_branch_state(&self, branch_name: &str) {
        let Some(config_path) = self.config_path.as_ref() else {
            return;
        };

        let normalized = self.config.get_normalized_branch_name(branch_name);

        match LocalStateManager::new() {
            Ok(mut state) => {
                if let Err(e) = state.unregister_branch(config_path, &normalized) {
                    log::warn!("Failed to unregister branch from local state: {}", e);
                }
            }
            Err(e) => {
                log::warn!("Failed to open local state manager: {}", e);
            }
        }
    }

    // ── Synchronous VCS operations (fast, run on main thread) ───────

    /// Create a new branch and check it out.
    pub fn create_and_checkout_branch(&mut self, name: &str, from: Option<&str>) -> Result<()> {
        let previous_branch = self
            .vcs
            .current_branch()?
            .map(|b| self.config.get_normalized_branch_name(&b));

        self.vcs.create_branch(name, from)?;
        self.vcs.checkout_branch(name)?;

        let parent = from
            .map(|b| self.config.get_normalized_branch_name(b))
            .or(previous_branch);
        self.upsert_branch_state(name, parent.as_deref());

        self.vcs_snapshot = Self::take_vcs_snapshot(&*self.vcs);
        Ok(())
    }

    /// Delete a VCS branch. Called after service branches are deleted.
    pub fn delete_vcs_branch(&mut self, name: &str) -> Result<()> {
        self.vcs.delete_branch(name)?;
        self.remove_branch_state(name);
        self.vcs_snapshot = Self::take_vcs_snapshot(&*self.vcs);
        Ok(())
    }

    // ── Synchronous data fetchers (local, no network) ───────────────

    /// Get effective config as YAML string.
    pub fn fetch_config_yaml(&self) -> Result<String> {
        let yaml = serde_yaml_ng::to_string(&self.config)?;
        Ok(yaml)
    }

    /// Get hooks data.
    pub fn fetch_hooks(&self) -> HooksData {
        let mut phases = Vec::new();

        if let Some(ref hooks_config) = self.config.hooks {
            for (phase, hooks_map) in hooks_config.iter() {
                let mut hooks = Vec::new();
                for (name, entry) in hooks_map.iter() {
                    let (command, is_extended, background, condition) = match entry {
                        HookEntry::Simple(cmd) => (cmd.clone(), false, false, None),
                        HookEntry::Extended(ext) => (
                            ext.command.clone(),
                            true,
                            ext.background,
                            ext.condition.clone(),
                        ),
                    };
                    hooks.push(HookEntryInfo {
                        name: name.clone(),
                        command,
                        is_extended,
                        background,
                        condition,
                    });
                }
                phases.push(HookPhaseEntry {
                    phase: phase.to_string(),
                    hooks,
                });
            }
        }

        HooksData { phases }
    }

    /// Get service configs.
    pub fn service_configs(&self) -> Vec<NamedServiceConfig> {
        self.config.resolve_services()
    }

    // ── Background task methods (static, take Config, no &self) ─────
    //
    // These are designed to be called from `tokio::spawn` background
    // tasks. They only need a `Config` clone, not the full context.

    /// Fetch enriched branch list: VCS snapshot + service states from providers.
    pub async fn fetch_branches_bg(
        config: &Config,
        vcs: VcsSnapshot,
        branch_registry: HashMap<String, Option<String>>,
        context_branch: Option<String>,
    ) -> Result<BranchesData> {
        // Get all providers and their branches (network calls)
        let providers = factory::create_all_providers(config).await.ok();

        let mut enriched = Vec::with_capacity(vcs.branches.len());

        for branch in &vcs.branches {
            // Find worktree path for this branch
            let worktree_path = vcs
                .worktrees
                .iter()
                .find(|wt| wt.branch.as_deref() == Some(&branch.name))
                .map(|wt| wt.path.display().to_string());

            // Collect service states for this branch
            let mut services = Vec::new();
            if let Some(ref providers) = providers {
                for named in providers {
                    let svc_branches = named.provider.list_branches().await.ok();
                    if let Some(svc_branches) = svc_branches {
                        if let Some(svc_branch) =
                            svc_branches.iter().find(|sb| sb.name == branch.name)
                        {
                            services.push(BranchServiceState {
                                service_name: named.name.clone(),
                                state: svc_branch.state.clone(),
                                database_name: Some(svc_branch.database_name.clone()),
                                parent_branch: svc_branch.parent_branch.clone(),
                                supports_lifecycle: named.provider.supports_lifecycle(),
                            });
                        }
                    }
                }
            }

            // Get parent from the branch registry
            let parent = branch_registry.get(&branch.name).cloned().flatten();

            let normalized = config.get_normalized_branch_name(&branch.name);
            let is_current = context_branch
                .as_deref()
                .map(|active| active == branch.name || active == normalized)
                .unwrap_or(branch.is_current);

            enriched.push(EnrichedBranch {
                name: branch.name.clone(),
                is_current,
                is_default: branch.is_default,
                worktree_path,
                services,
                parent,
            });
        }

        // Include branches from the registry that are not present in VCS
        // (e.g. just-registered main branch on an unborn repo, or branches
        // whose worktrees were pruned).
        let vcs_names: std::collections::HashSet<&str> =
            vcs.branches.iter().map(|b| b.name.as_str()).collect();

        for (reg_name, reg_parent) in &branch_registry {
            if vcs_names.contains(reg_name.as_str()) {
                continue; // already handled above
            }

            let normalized = config.get_normalized_branch_name(reg_name);
            let is_current = context_branch
                .as_deref()
                .map(|active| active == reg_name || active == normalized)
                .unwrap_or(false);

            enriched.push(EnrichedBranch {
                name: reg_name.clone(),
                is_current,
                is_default: vcs.default_branch.as_deref() == Some(reg_name.as_str()),
                worktree_path: None,
                services: Vec::new(),
                parent: reg_parent.clone(),
            });
        }

        Ok(BranchesData {
            branches: enriched,
            current_branch: context_branch.or(vcs.current_branch),
            default_branch: vcs.default_branch,
        })
    }

    /// Fetch all services with their branches.
    pub async fn fetch_services_bg(config: &Config) -> Result<ServicesData> {
        let named_configs = config.resolve_services();
        let mut services = Vec::new();

        for named_config in &named_configs {
            let provider = factory::create_provider_from_named_config(config, named_config)
                .await
                .ok();

            let mut branches = Vec::new();
            let mut project_info = None;

            if let Some(ref provider) = provider {
                if let Ok(svc_branches) = provider.list_branches().await {
                    for b in svc_branches {
                        branches.push(ServiceBranchEntry {
                            name: b.name,
                            state: b.state,
                            parent_branch: b.parent_branch,
                            database_name: b.database_name,
                            created_at: b
                                .created_at
                                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string()),
                        });
                    }
                }

                if let Some(info) = provider.project_info() {
                    project_info = Some(ProjectInfoEntry {
                        name: info.name,
                        storage_driver: info.storage_driver,
                        image: info.image,
                    });
                }
            }

            services.push(ServiceEntry {
                name: named_config.name.clone(),
                provider_type: named_config.provider_type.clone(),
                service_type: named_config.service_type.clone(),
                auto_branch: named_config.auto_branch,
                is_default: named_config.default,
                branches,
                project_info,
            });
        }

        Ok(ServicesData { services })
    }

    /// Fetch capability information for the current environment and all configured services.
    pub async fn fetch_capabilities_bg(config: &Config) -> Result<CapabilitiesData> {
        let vcs_provider = vcs::detect_vcs_provider(".")
            .ok()
            .map(|v| v.provider_name().to_string());

        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let worktree_cow = match vcs::cow_worktree::detect_cow_capability(&cwd) {
            vcs::cow_worktree::CowCapability::Apfs => "apfs",
            vcs::cow_worktree::CowCapability::Reflink => "reflink",
            vcs::cow_worktree::CowCapability::None => "none",
        }
        .to_string();

        let providers = factory::create_all_providers(config).await?;
        let mut services = Vec::with_capacity(providers.len());
        for named in &providers {
            services.push(ServiceCapabilityEntry {
                service_name: named.name.clone(),
                provider_name: named.provider.provider_name().to_string(),
                capabilities: named.provider.capabilities(),
            });
        }
        services.sort_by(|a, b| a.service_name.cmp(&b.service_name));

        Ok(CapabilitiesData {
            vcs_provider,
            worktree_cow,
            services,
        })
    }

    /// Run doctor checks on all services.
    pub async fn fetch_doctor_bg(config: &Config) -> Result<Vec<DoctorEntry>> {
        let providers = factory::create_all_providers(config).await?;
        let mut entries = Vec::new();

        for named in &providers {
            if let Ok(report) = named.provider.doctor().await {
                entries.push(DoctorEntry {
                    service_name: named.name.clone(),
                    checks: report
                        .checks
                        .into_iter()
                        .map(|c| DoctorCheckEntry {
                            name: c.name,
                            available: c.available,
                            detail: c.detail,
                        })
                        .collect(),
                });
            }
        }

        Ok(entries)
    }

    /// Fetch container logs for a service/branch.
    pub async fn fetch_logs_bg(
        config: &Config,
        service_name: &str,
        branch_name: &str,
    ) -> Result<String> {
        let named = factory::resolve_provider(config, Some(service_name)).await?;
        named.provider.logs(branch_name, Some(200)).await
    }

    /// Switch/align services to a branch without changing VCS checkout.
    pub async fn switch_services_bg(config: &Config, branch_name: &str) -> Result<String> {
        if config.resolve_services().is_empty() {
            return Ok("No services configured".to_string());
        }

        let results = factory::orchestrate_switch(config, branch_name, None).await?;
        let failures: Vec<_> = results.iter().filter(|r| !r.success).collect();
        if failures.is_empty() {
            Ok(format!("Aligned services to branch '{}'", branch_name))
        } else {
            let msgs: Vec<_> = failures.iter().map(|f| f.message.as_str()).collect();
            Ok(format!(
                "Aligned services to '{}' (some services failed: {})",
                branch_name,
                msgs.join(", ")
            ))
        }
    }

    /// Create service branches (VCS create+checkout done on main thread before this).
    pub async fn create_branch_bg(
        config: &Config,
        name: &str,
        from: Option<&str>,
    ) -> Result<String> {
        let results = factory::orchestrate_create(config, name, from).await?;
        let failures: Vec<_> = results.iter().filter(|r| !r.success).collect();
        if failures.is_empty() {
            Ok(format!("Created and switched to branch '{}'", name))
        } else {
            let msgs: Vec<_> = failures.iter().map(|f| f.message.as_str()).collect();
            Ok(format!(
                "Created '{}' (some services failed: {})",
                name,
                msgs.join(", ")
            ))
        }
    }

    /// Delete service branches + VCS branch.
    pub async fn delete_branch_bg(config: &Config, name: &str) -> Result<String> {
        let results = factory::orchestrate_delete(config, name).await?;
        let failures: Vec<_> = results.iter().filter(|r| !r.success).collect();
        if failures.is_empty() {
            Ok(format!("Deleted branch '{}'", name))
        } else {
            let msgs: Vec<_> = failures.iter().map(|f| f.message.as_str()).collect();
            Ok(format!(
                "Deleted '{}' (some services failed: {})",
                name,
                msgs.join(", ")
            ))
        }
    }

    /// Start a service branch.
    pub async fn start_service_bg(
        config: &Config,
        service_name: &str,
        branch_name: &str,
    ) -> Result<String> {
        let named = factory::resolve_provider(config, Some(service_name)).await?;
        named.provider.start_branch(branch_name).await?;
        Ok(format!(
            "Started {} on branch '{}'",
            service_name, branch_name
        ))
    }

    /// Stop a service branch.
    pub async fn stop_service_bg(
        config: &Config,
        service_name: &str,
        branch_name: &str,
    ) -> Result<String> {
        let named = factory::resolve_provider(config, Some(service_name)).await?;
        named.provider.stop_branch(branch_name).await?;
        Ok(format!(
            "Stopped {} on branch '{}'",
            service_name, branch_name
        ))
    }

    /// Reset a service branch.
    pub async fn reset_service_bg(
        config: &Config,
        service_name: &str,
        branch_name: &str,
    ) -> Result<String> {
        let named = factory::resolve_provider(config, Some(service_name)).await?;
        named.provider.reset_branch(branch_name).await?;
        Ok(format!(
            "Reset {} on branch '{}'",
            service_name, branch_name
        ))
    }

    /// Delete a service branch.
    pub async fn delete_service_branch_bg(
        config: &Config,
        service_name: &str,
        branch_name: &str,
    ) -> Result<String> {
        let named = factory::resolve_provider(config, Some(service_name)).await?;
        named.provider.delete_branch(branch_name).await?;
        Ok(format!(
            "Deleted branch '{}' on {}",
            branch_name, service_name
        ))
    }
}

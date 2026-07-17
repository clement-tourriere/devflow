use anyhow::Result;
use std::path::PathBuf;

use devflow_core::config::Config;
use devflow_core::hooks::HookEntry;
use devflow_core::services::factory;
use devflow_core::vcs;

use super::action::*;

/// Shared context that the TUI components use to fetch data.
/// Encapsulates config loading, VCS detection, and provider creation.
///
/// VCS operations run synchronously (they're local + fast).
/// Provider/network operations are exposed as static `_bg()` methods
/// that take a `Config` clone and run on background tasks.
pub struct DevflowContext {
    pub config: Config,
    pub config_path: Option<PathBuf>,
    /// The checkout/worktree from which the TUI was launched. This can differ
    /// from `config_path` when a linked worktree falls back to configuration
    /// stored in Git's primary checkout.
    pub project_dir: PathBuf,
}

fn summarize_workspace_switch(
    verb: &str,
    workspace: &str,
    services: &[devflow_core::workspace::ServiceResult],
    processes: &[devflow_core::processes::ProcessResult],
    hooks: &[devflow_core::workspace::LifecycleHookResult],
) -> Result<String> {
    let mut failures: Vec<String> = services
        .iter()
        .filter(|r| !r.success)
        .map(|r| format!("service '{}': {}", r.service_name, r.message))
        .collect();
    failures.extend(
        processes
            .iter()
            .filter(|r| !r.success && r.required)
            .map(|r| format!("process '{}': {}", r.process, r.message)),
    );
    failures.extend(hooks.iter().filter(|hook| hook.failed > 0).map(|hook| {
        let details = if hook.errors.is_empty() {
            String::new()
        } else {
            format!(": {}", hook.errors.join("; "))
        };
        format!(
            "hook phase '{}': {} failed{}",
            hook.phase, hook.failed, details
        )
    }));
    if !failures.is_empty() {
        anyhow::bail!(
            "{} '{}' completed with failures: {}",
            verb,
            workspace,
            failures.join("; ")
        );
    }

    let mut warnings: Vec<String> = processes
        .iter()
        .filter(|r| !r.success && !r.required)
        .map(|r| format!("optional process '{}': {}", r.process, r.message))
        .collect();
    warnings.extend(
        hooks
            .iter()
            .filter(|hook| hook.skipped > 0)
            .map(|hook| format!("hook phase '{}': {} skipped", hook.phase, hook.skipped)),
    );
    if warnings.is_empty() {
        Ok(format!("{} workspace '{}'", verb, workspace))
    } else {
        Ok(format!(
            "{} workspace '{}' (warnings: {})",
            verb,
            workspace,
            warnings.join("; ")
        ))
    }
}

fn ensure_vcs_ref_deleted(workspace: &str, deleted: bool) -> Result<()> {
    if !deleted {
        anyhow::bail!(
            "Deletion of workspace '{}' completed only partially: its VCS ref still exists",
            workspace
        );
    }
    Ok(())
}

impl DevflowContext {
    /// Load config, inject state services, detect VCS, snapshot VCS data.
    pub fn new() -> Result<Self> {
        let project_dir = std::env::current_dir()?;
        let (effective_config, config_path) = Config::load_effective_config_with_path_info()?;
        let mut config = effective_config.get_merged_config();

        // Overlay CLI-managed local-state services (shared core helper).
        if let Some(ref path) = config_path {
            config.overlay_local_state_services(path);
        }

        // Fail early with a useful message when the TUI is opened outside a
        // supported VCS project. Inventory refreshes perform live detection.
        let _ = vcs::detect_vcs_provider(&project_dir)?;

        Ok(Self {
            config,
            config_path,
            project_dir,
        })
    }

    /// Reload committed/local/environment configuration and CLI-managed
    /// local-state services. TUI mutations (notably service add/remove) must
    /// refresh this snapshot before rebuilding the canonical inventory.
    pub fn reload_config(&mut self) -> Result<()> {
        let (effective_config, config_path) = Config::load_effective_config_with_path_info()?;
        let mut config = effective_config.get_merged_config();
        if let Some(ref path) = config_path {
            config.overlay_local_state_services(path);
        }
        self.config = config;
        self.config_path = config_path;
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
                        HookEntry::Action(act) => (
                            format!("action: {}", act.action.type_name()),
                            true,
                            act.background,
                            act.condition.clone(),
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

    // ── Background task methods (static, take Config, no &self) ─────
    //
    // These are designed to be called from `tokio::spawn` background
    // tasks. They only need a `Config` clone, not the full context.

    /// Fetch the canonical core workspace inventory and adapt it for the TUI.
    pub async fn fetch_branches_bg(
        config: &Config,
        project_dir: &std::path::Path,
    ) -> Result<BranchesData> {
        let inventory =
            devflow_core::workspace::inventory::build_workspace_inventory(config, project_dir)
                .await?;
        let roots = inventory.roots;
        let workspaces = inventory
            .workspaces
            .into_iter()
            .map(|workspace| EnrichedBranch {
                name: workspace.name,
                is_current: workspace.is_context,
                is_default: workspace.is_default,
                worktree_path: workspace.worktree_path,
                health: workspace.health,
                services: workspace
                    .services
                    .into_iter()
                    .map(|service| BranchServiceState {
                        service_name: service.name,
                        state: service.state,
                        database_name: service.database_name,
                        provisioned: service.provisioned,
                        supports_lifecycle: service.supports_lifecycle,
                    })
                    .collect(),
                processes: workspace.processes,
                parent: workspace.parent,
                parent_state: workspace.parent_state,
                children: workspace.children,
            })
            .collect();

        Ok(BranchesData {
            roots,
            workspaces,
            warnings: inventory.warnings,
        })
    }

    /// Fetch all services with their workspaces.
    pub async fn fetch_services_bg(
        config: &Config,
        project_dir: &std::path::Path,
    ) -> Result<ServicesData> {
        let inventory =
            devflow_core::workspace::inventory::build_workspace_inventory(config, project_dir)
                .await?;
        let named_configs = config.resolve_services();
        let mut services = Vec::new();

        for named_config in &named_configs {
            let provider = factory::create_provider_from_named_config(config, named_config)
                .await
                .ok();

            let mut workspaces = inventory
                .workspaces
                .iter()
                .filter_map(|workspace| {
                    workspace
                        .services
                        .iter()
                        .find(|service| service.name == named_config.name && service.provisioned)
                        .map(|service| ServiceWorkspaceEntry {
                            name: workspace.name.clone(),
                            state: service.state.clone(),
                            parent_workspace: workspace.parent.clone(),
                            database_name: service.database_name.clone().unwrap_or_default(),
                            supports_lifecycle: service.supports_lifecycle,
                        })
                })
                .collect::<Vec<_>>();
            workspaces.sort_by(|a, b| a.name.cmp(&b.name));
            let mut project_info = None;

            if let Some(ref provider) = provider {
                if let Some(info) = provider.project_info() {
                    project_info = Some(ProjectInfoEntry {
                        storage_driver: info.storage_driver,
                        image: info.image,
                    });
                }
            }

            services.push(ServiceEntry {
                name: named_config.name.clone(),
                provider_type: named_config.provider_type.clone(),
                service_type: named_config.service_type.clone(),
                workspaces,
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

    /// Fetch container logs for a service/workspace.
    pub async fn fetch_logs_bg(
        config: &Config,
        service_name: &str,
        workspace_name: &str,
        project_dir: &std::path::Path,
    ) -> Result<String> {
        let service_key = devflow_core::state::LocalStateManager::new()?
            .resolve_workspace_service_key_by_dir(project_dir, workspace_name)?;
        let named = factory::resolve_provider(config, Some(service_name)).await?;
        named.provider.logs(&service_key, Some(200)).await
    }

    /// Switch a workspace via the shared core lifecycle.
    pub async fn switch_services_bg(
        config: &Config,
        workspace_name: &str,
        project_dir: &std::path::Path,
    ) -> Result<String> {
        let options = devflow_core::workspace::switch::SwitchOptions {
            lifecycle: devflow_core::workspace::LifecycleOptions::default(),
            create_if_missing: false,
            from_workspace: None,
            copy_files: None,
            copy_ignored: None,
        };
        let result = devflow_core::workspace::switch::switch_workspace(
            config,
            project_dir,
            workspace_name,
            &options,
        )
        .await?;
        summarize_workspace_switch(
            "Switched",
            &result.workspace,
            &result.services,
            &result.processes,
            &result.hooks,
        )
    }

    /// Create a workspace via the shared core lifecycle.
    pub async fn create_workspace_bg(
        config: &Config,
        name: &str,
        from: Option<&str>,
        project_dir: &std::path::Path,
    ) -> Result<String> {
        let options = devflow_core::workspace::create::CreateOptions {
            lifecycle: devflow_core::workspace::LifecycleOptions::default(),
            from_workspace: from.map(ToString::to_string),
            copy_files: None,
            copy_ignored: None,
        };
        let result =
            devflow_core::workspace::create::create_workspace(config, project_dir, name, &options)
                .await?;
        summarize_workspace_switch(
            "Created",
            &result.workspace,
            &result.services,
            &result.processes,
            &result.hooks,
        )
    }

    /// Delete a workspace via the shared core lifecycle.
    pub async fn delete_workspace_bg(
        config: &Config,
        name: &str,
        project_dir: &std::path::Path,
        force: bool,
    ) -> Result<String> {
        let options = devflow_core::workspace::delete::DeleteOptions {
            lifecycle: devflow_core::workspace::LifecycleOptions::default(),
            keep_services: false,
            force,
        };
        let result =
            devflow_core::workspace::delete::delete_workspace(config, project_dir, name, &options)
                .await?;
        ensure_vcs_ref_deleted(&result.workspace, result.vcs_ref_deleted)?;
        summarize_workspace_switch(
            "Deleted",
            &result.workspace,
            &result.services,
            &result.processes,
            &result.hooks,
        )
    }

    /// Inspect deletion safety without mutating workspace state.
    pub fn preflight_delete_workspace_bg(
        config: &Config,
        name: &str,
        project_dir: &std::path::Path,
    ) -> Result<devflow_core::workspace::delete::DeleteWorkspacePreflight> {
        devflow_core::workspace::delete::preflight_delete_workspace(config, project_dir, name)
    }

    /// Start a service workspace.
    pub async fn start_service_bg(
        config: &Config,
        service_name: &str,
        workspace_name: &str,
        project_dir: &std::path::Path,
    ) -> Result<String> {
        let service_key = devflow_core::state::LocalStateManager::new()?
            .resolve_workspace_service_key_by_dir(project_dir, workspace_name)?;
        let named = factory::resolve_provider(config, Some(service_name)).await?;
        named.provider.start_workspace(&service_key).await?;
        Ok(format!(
            "Started {} on workspace '{}'",
            service_name, workspace_name
        ))
    }

    /// Stop a service workspace.
    pub async fn stop_service_bg(
        config: &Config,
        service_name: &str,
        workspace_name: &str,
        project_dir: &std::path::Path,
    ) -> Result<String> {
        let service_key = devflow_core::state::LocalStateManager::new()?
            .resolve_workspace_service_key_by_dir(project_dir, workspace_name)?;
        let named = factory::resolve_provider(config, Some(service_name)).await?;
        named.provider.stop_workspace(&service_key).await?;
        Ok(format!(
            "Stopped {} on workspace '{}'",
            service_name, workspace_name
        ))
    }

    /// Reset a service workspace.
    pub async fn reset_service_bg(
        config: &Config,
        service_name: &str,
        workspace_name: &str,
        project_dir: &std::path::Path,
    ) -> Result<String> {
        let service_key = devflow_core::state::LocalStateManager::new()?
            .resolve_workspace_service_key_by_dir(project_dir, workspace_name)?;
        let named = factory::resolve_provider(config, Some(service_name)).await?;
        named.provider.reset_workspace(&service_key).await?;
        Ok(format!(
            "Reset {} on workspace '{}'",
            service_name, workspace_name
        ))
    }

    // ── Proxy background methods ────────────────────────────────────

    /// Fetch proxy status and routing targets from the proxy API.
    pub async fn fetch_proxy_status_bg() -> Result<(
        super::components::proxy_tab::ProxyStatusData,
        Vec<super::components::proxy_tab::ProxyTargetEntry>,
    )> {
        use super::components::proxy_tab::{ProxyStatusData, ProxyTargetEntry};

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()?;

        // Fetch status
        let status_resp: serde_json::Value = client
            .get("http://127.0.0.1:2019/api/status")
            .send()
            .await?
            .json()
            .await?;

        let status = ProxyStatusData {
            running: status_resp["running"].as_bool().unwrap_or(false),
            https_port: status_resp["https_port"].as_u64().unwrap_or(443) as u16,
            http_port: status_resp["http_port"].as_u64().unwrap_or(80) as u16,
            api_port: status_resp["api_port"].as_u64().unwrap_or(2019) as u16,
            ca_installed: status_resp["ca_installed"].as_bool().unwrap_or(false),
        };

        // Fetch targets
        let targets_resp: serde_json::Value = client
            .get("http://127.0.0.1:2019/api/targets")
            .send()
            .await?
            .json()
            .await?;

        let targets = targets_resp
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|t| ProxyTargetEntry {
                        domain: t["domain"].as_str().unwrap_or("-").to_string(),
                        container_name: t["container_name"].as_str().unwrap_or("-").to_string(),
                        container_ip: t["container_ip"].as_str().unwrap_or("-").to_string(),
                        port: t["port"].as_u64().unwrap_or(0) as u16,
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok((status, targets))
    }

    /// Start the proxy in daemon mode.
    pub async fn start_proxy_bg() -> Result<String> {
        // Only the ports are forwarded to the spawned `proxy start`; the suffix
        // and mDNS use the CLI's own defaults.
        let config = devflow_proxy::ProxyConfig {
            https_port: 443,
            http_port: 80,
            api_port: 2019,
            ..Default::default()
        };

        let exe = std::env::current_exe()?;
        let child = std::process::Command::new(exe)
            .args([
                "proxy",
                "start",
                "--https-port",
                &config.https_port.to_string(),
                "--http-port",
                &config.http_port.to_string(),
                "--api-port",
                &config.api_port.to_string(),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;

        let pid_path = devflow_proxy::ca::default_ca_cert_path()
            .parent()
            .unwrap()
            .join("proxy.pid");
        std::fs::write(&pid_path, child.id().to_string())?;

        Ok(format!("Proxy started (pid: {})", child.id()))
    }

    /// Stop the proxy daemon.
    pub async fn stop_proxy_bg() -> Result<String> {
        let pid_path = devflow_proxy::ca::default_ca_cert_path()
            .parent()
            .unwrap()
            .join("proxy.pid");

        if !pid_path.exists() {
            anyhow::bail!("Proxy is not running (no PID file)");
        }

        let pid_str = std::fs::read_to_string(&pid_path)?;
        let pid: i32 = pid_str
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid PID file"))?;

        #[cfg(unix)]
        {
            use nix::sys::signal::{kill, Signal};
            use nix::unistd::Pid;
            let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
        }

        std::fs::remove_file(&pid_path)?;
        Ok(format!("Proxy stopped (pid: {})", pid))
    }
}

#[cfg(test)]
mod tests {
    use super::{ensure_vcs_ref_deleted, summarize_workspace_switch};

    #[test]
    fn undeleted_vcs_ref_is_not_reported_as_tui_success() {
        let error = ensure_vcs_ref_deleted("feature/api", false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("VCS ref still exists"));
        ensure_vcs_ref_deleted("feature/api", true).unwrap();
    }

    #[test]
    fn hook_failures_are_not_reported_as_tui_success() {
        let hooks = vec![devflow_core::workspace::LifecycleHookResult {
            phase: "post-create".to_string(),
            succeeded: 0,
            failed: 1,
            skipped: 0,
            background: 0,
            errors: vec!["migration failed".to_string()],
        }];
        let error = summarize_workspace_switch("Created", "feature/api", &[], &[], &hooks)
            .unwrap_err()
            .to_string();
        assert!(error.contains("post-create"));
        assert!(error.contains("migration failed"));
    }
}

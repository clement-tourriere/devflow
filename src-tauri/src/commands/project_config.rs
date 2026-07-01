use devflow_core::config::{Config, NamedServiceConfig};
use devflow_core::state::LocalStateManager;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ServiceWithSource {
    pub service: NamedServiceConfig,
    pub source: &'static str,
}

pub fn config_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".devflow.yml")
}

pub fn load_project_config(project_dir: &Path) -> Result<Config, String> {
    let path = config_path(project_dir);
    let mut config = if path.exists() {
        Config::from_file(&path).map_err(crate::commands::format_error)?
    } else {
        Config::default()
    };

    // Tauri commands may be invoked from the GUI process cwd, not the project
    // cwd. Keep provider naming stable by deriving the project name from the
    // selected project path when the config does not pin it explicitly.
    if config
        .name
        .as_ref()
        .is_none_or(|name| name.trim().is_empty())
    {
        config.name = project_dir
            .file_name()
            .map(|name| name.to_string_lossy().to_string());
    }

    Ok(config)
}

pub fn list_services_with_sources(project_dir: &Path) -> Result<Vec<ServiceWithSource>, String> {
    let config = load_project_config(project_dir)?;
    Ok(list_services_with_sources_for_config(project_dir, &config))
}

fn list_services_with_sources_for_config(
    project_dir: &Path,
    config: &Config,
) -> Vec<ServiceWithSource> {
    let path = config_path(project_dir);

    let mut services: Vec<ServiceWithSource> = config
        .resolve_services()
        .into_iter()
        .map(|service| ServiceWithSource {
            service,
            source: "config",
        })
        .collect();

    if let Ok(state) = LocalStateManager::new() {
        if let Some(state_services) = state.get_services(&path) {
            for service in state_services {
                if let Some(existing) = services
                    .iter_mut()
                    .find(|entry| entry.service.name == service.name)
                {
                    existing.service = service;
                    existing.source = "local_state";
                } else {
                    services.push(ServiceWithSource {
                        service,
                        source: "local_state",
                    });
                }
            }
        }
    }

    normalize_service_defaults(&mut services);
    services
}

fn normalize_service_defaults(services: &mut [ServiceWithSource]) {
    let mut seen_default = false;
    for entry in services {
        if entry.service.default {
            if seen_default {
                entry.service.default = false;
            } else {
                seen_default = true;
            }
        }
    }
}

/// Load project config and overlay local-state services.
///
/// Local state is where CLI-managed services are stored, so GUI service,
/// workspace, and process commands must use this merged view rather than only
/// reading `.devflow.yml`. Services committed in config remain visible unless a
/// local-state service with the same name overrides them.
pub fn load_project_config_with_local_state(project_dir: &Path) -> Result<Config, String> {
    let mut config = load_project_config(project_dir)?;
    let services = list_services_with_sources_for_config(project_dir, &config)
        .into_iter()
        .map(|entry| entry.service)
        .collect::<Vec<_>>();

    if !services.is_empty() {
        config.services = Some(services);
    }

    Ok(config)
}

pub fn service_source_counts(project_dir: &Path) -> Result<(usize, usize, Vec<String>), String> {
    let entries = list_services_with_sources(project_dir)?;
    let config_count = entries
        .iter()
        .filter(|entry| entry.source == "config")
        .count();
    let local_count = entries
        .iter()
        .filter(|entry| entry.source == "local_state")
        .count();
    let local_names = entries
        .iter()
        .filter(|entry| entry.source == "local_state")
        .map(|entry| entry.service.name.clone())
        .collect();
    Ok((config_count, local_count, local_names))
}

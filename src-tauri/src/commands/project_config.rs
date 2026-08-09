use devflow_core::config::{Config, ServiceWithSource};
use std::path::{Path, PathBuf};

pub fn config_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".devflow.yml")
}

/// Load only the committed project config (for editors that write it back).
fn load_project_config(project_dir: &Path) -> Result<Config, String> {
    let mut config = match Config::find_config_file_in(project_dir) {
        Some(path) => Config::from_file(&path).map_err(crate::commands::format_error)?,
        None => Config::default(),
    };
    // The GUI process cwd is unrelated to the project (Finder launches apps
    // with cwd "/"); pin the project root so core never falls back to cwd.
    config
        .project_root
        .get_or_insert_with(|| project_dir.to_path_buf());
    Ok(config)
}

pub fn list_services_with_sources(project_dir: &Path) -> Result<Vec<ServiceWithSource>, String> {
    let config = load_project_config(project_dir)?;
    Ok(config.services_with_sources(&config_path(project_dir)))
}

/// Full effective config for GUI service/workspace/process commands:
/// committed + global + local + env, with local-state services overlaid.
pub fn load_project_config_with_local_state(project_dir: &Path) -> Result<Config, String> {
    Config::load_effective_for_dir(project_dir).map_err(crate::commands::format_error)
}

pub fn service_source_counts(project_dir: &Path) -> Result<(usize, usize, Vec<String>), String> {
    let entries = list_services_with_sources(project_dir)?;
    let config_count = entries
        .iter()
        .filter(|entry| entry.source == devflow_core::config::ServiceSource::Config)
        .count();
    let local_count = entries
        .iter()
        .filter(|entry| entry.source == devflow_core::config::ServiceSource::LocalState)
        .count();
    let local_names = entries
        .iter()
        .filter(|entry| entry.source == devflow_core::config::ServiceSource::LocalState)
        .map(|entry| entry.service.name.clone())
        .collect();
    Ok((config_count, local_count, local_names))
}

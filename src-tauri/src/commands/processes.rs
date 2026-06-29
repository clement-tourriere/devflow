use devflow_core::config::Config;
use devflow_core::processes::{self, ProcessResult, ProcessStatus};
use serde::Serialize;
use std::path::{Path, PathBuf};

fn load_config(project_dir: &Path) -> Result<Config, String> {
    let config_path = project_dir.join(".devflow.yml");
    if config_path.exists() {
        Config::from_file(&config_path).map_err(crate::commands::format_error)
    } else {
        Ok(Config::default())
    }
}

fn current_workspace(config: &Config, workspace: Option<String>) -> String {
    workspace.unwrap_or_else(|| config.git.main_workspace.clone())
}

#[derive(Debug, Serialize)]
pub struct ProcessOperationResponse {
    pub workspace: String,
    pub results: Vec<ProcessResult>,
}

#[tauri::command]
pub async fn list_processes(
    project_path: String,
    workspace_name: Option<String>,
) -> Result<Vec<ProcessStatus>, String> {
    let project_dir = PathBuf::from(&project_path);
    let config = load_config(&project_dir)?;
    processes::list_workspace_processes(&config, &project_dir, workspace_name.as_deref())
        .map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn start_processes(
    project_path: String,
    workspace_name: Option<String>,
    names: Vec<String>,
    force: bool,
) -> Result<ProcessOperationResponse, String> {
    let project_dir = PathBuf::from(&project_path);
    let config = load_config(&project_dir)?;
    let workspace = current_workspace(&config, workspace_name);
    let results =
        processes::start_workspace_processes(&config, &project_dir, &workspace, &names, force)
            .await
            .map_err(crate::commands::format_error)?;
    Ok(ProcessOperationResponse { workspace, results })
}

#[tauri::command]
pub async fn stop_processes(
    project_path: String,
    workspace_name: Option<String>,
    names: Vec<String>,
) -> Result<ProcessOperationResponse, String> {
    let project_dir = PathBuf::from(&project_path);
    let config = load_config(&project_dir)?;
    let workspace = current_workspace(&config, workspace_name);
    let results = processes::stop_workspace_processes(&config, &project_dir, &workspace, &names)
        .await
        .map_err(crate::commands::format_error)?;
    Ok(ProcessOperationResponse { workspace, results })
}

#[tauri::command]
pub async fn restart_processes(
    project_path: String,
    workspace_name: Option<String>,
    names: Vec<String>,
) -> Result<ProcessOperationResponse, String> {
    let project_dir = PathBuf::from(&project_path);
    let config = load_config(&project_dir)?;
    let workspace = current_workspace(&config, workspace_name);
    let results = processes::restart_workspace_processes(&config, &project_dir, &workspace, &names)
        .await
        .map_err(crate::commands::format_error)?;
    Ok(ProcessOperationResponse { workspace, results })
}

#[tauri::command]
pub fn get_process_logs(
    project_path: String,
    workspace_name: String,
    name: String,
    tail: Option<usize>,
) -> Result<String, String> {
    let project_dir = PathBuf::from(&project_path);
    let config = load_config(&project_dir)?;
    processes::process_logs(&config, &project_dir, &workspace_name, &name, tail)
        .map_err(crate::commands::format_error)
}

use devflow_core::config::Config;
use devflow_core::processes::{self, ProcessResult, ProcessStatus};
use serde::Serialize;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn load_config(project_dir: &Path) -> Result<Config, String> {
    crate::commands::project_config::load_project_config_with_local_state(project_dir)
}

fn current_workspace(config: &Config, workspace: Option<String>) -> String {
    workspace.unwrap_or_else(|| config.git.main_workspace.clone())
}

#[derive(Debug, Serialize)]
pub struct ProcessOperationResponse {
    pub workspace: String,
    pub results: Vec<ProcessResult>,
}

#[derive(Debug, Serialize)]
pub struct PitchforkBridgeInfo {
    pub provider: String,
    pub enabled: bool,
    pub web_ui_enabled: bool,
    pub web_ui_url: String,
    pub web_ui_reachable: bool,
    pub cli_available: bool,
    pub config_policy: String,
    pub external_daemons: String,
    pub edit_mode: String,
}

fn tcp_reachable(host: &str, port: u16) -> bool {
    let Ok(addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(150)).is_ok())
}

fn pitchfork_cli_available() -> bool {
    command_available_on_path("pitchfork")
}

fn command_available_on_path(command: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        command_candidates(&dir, command)
            .into_iter()
            .any(is_executable_file)
    })
}

#[cfg(windows)]
fn command_candidates(dir: &Path, command: &str) -> Vec<PathBuf> {
    if Path::new(command).extension().is_some() {
        return vec![dir.join(command)];
    }
    let pathext = std::env::var_os("PATHEXT")
        .and_then(|v| v.into_string().ok())
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
    pathext
        .split(';')
        .filter(|ext| !ext.is_empty())
        .map(|ext| dir.join(format!("{}{}", command, ext)))
        .collect()
}

#[cfg(not(windows))]
fn command_candidates(dir: &Path, command: &str) -> Vec<PathBuf> {
    vec![dir.join(command)]
}

#[cfg(unix)]
fn is_executable_file(path: PathBuf) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: PathBuf) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file())
        .unwrap_or(false)
}

#[tauri::command]
pub async fn get_pitchfork_bridge_info(
    project_path: String,
) -> Result<PitchforkBridgeInfo, String> {
    let project_dir = PathBuf::from(&project_path);
    let config = load_config(&project_dir)?;
    let provider = config
        .processes
        .as_ref()
        .map(|p| p.provider.to_ascii_lowercase())
        .unwrap_or_else(|| "native".to_string());
    let pitchfork = config.processes.as_ref().and_then(|p| p.pitchfork.as_ref());
    let web_ui = pitchfork.and_then(|p| p.web_ui.as_ref());
    let bind_address = web_ui
        .and_then(|w| w.bind_address.clone())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let bind_port = web_ui.and_then(|w| w.bind_port).unwrap_or(3120);
    let url = format!("http://{}:{}", bind_address, bind_port);
    let enabled = provider == "pitchfork";
    Ok(PitchforkBridgeInfo {
        provider,
        enabled,
        web_ui_enabled: enabled && web_ui.is_some_and(|w| w.enabled),
        web_ui_url: url,
        web_ui_reachable: enabled && tcp_reachable(&bind_address, bind_port),
        cli_available: pitchfork_cli_available(),
        config_policy: pitchfork
            .map(|p| p.config_policy.clone())
            .unwrap_or_else(|| "devflow-owned".to_string()),
        external_daemons: pitchfork
            .map(|p| p.external_daemons.clone())
            .unwrap_or_else(|| "show".to_string()),
        edit_mode: web_ui
            .map(|w| w.edit_mode.clone())
            .unwrap_or_else(|| "warn".to_string()),
    })
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

#[tauri::command]
pub fn forget_process_record(
    project_path: String,
    workspace_name: String,
    name: String,
) -> Result<bool, String> {
    let project_dir = PathBuf::from(&project_path);
    let config = load_config(&project_dir)?;
    processes::forget_workspace_process_record(&config, &project_dir, &workspace_name, &name)
        .map_err(crate::commands::format_error)
}

use crate::state::AppState;
use serde::Serialize;
use std::sync::Arc;
use tauri::{Emitter, State};

#[derive(Clone, Serialize)]
pub struct ProxyStatus {
    pub running: bool,
    pub https_port: u16,
    pub http_port: u16,
    pub ca_installed: bool,
    pub ca_path: String,
}

#[derive(Serialize)]
pub struct ContainerEntry {
    pub domain: String,
    pub container_name: String,
    pub container_ip: String,
    pub port: u16,
    pub project: Option<String>,
    pub service: Option<String>,
    pub branch: Option<String>,
    pub https_url: String,
}

#[tauri::command]
pub async fn start_proxy(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<ProxyStatus, String> {
    let config = state.proxy_config.read().await.clone();

    let handle = devflow_proxy::run_proxy(config.clone())
        .await
        .map_err(|e| e.to_string())?;

    *state.proxy.write().await = Some(Arc::new(handle));

    // Persist auto-start preference
    {
        let mut settings = state.settings.write().await;
        settings.proxy_auto_start = true;
        settings.proxy_config = Some(config.clone());
        let _ = settings.save();
    }

    let ca_installed = devflow_proxy::platform::verify_system_trust().unwrap_or(false);

    let status = ProxyStatus {
        running: true,
        https_port: config.https_port,
        http_port: config.http_port,
        ca_installed,
        ca_path: devflow_proxy::ca::default_ca_cert_path()
            .display()
            .to_string(),
    };

    let _ = app.emit("proxy-status-changed", &status);
    crate::update_tray_menu(&app);

    Ok(status)
}

#[tauri::command]
pub async fn stop_proxy(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mut proxy = state.proxy.write().await;
    if let Some(handle) = proxy.take() {
        handle.stop();
    }

    // Persist auto-start preference
    {
        let mut settings = state.settings.write().await;
        settings.proxy_auto_start = false;
        let _ = settings.save();
    }

    let config = state.proxy_config.read().await;
    let status = ProxyStatus {
        running: false,
        https_port: config.https_port,
        http_port: config.http_port,
        ca_installed: devflow_proxy::platform::verify_system_trust().unwrap_or(false),
        ca_path: devflow_proxy::ca::default_ca_cert_path()
            .display()
            .to_string(),
    };

    let _ = app.emit("proxy-status-changed", &status);
    crate::update_tray_menu(&app);

    Ok(())
}

#[tauri::command]
pub async fn get_proxy_status(state: State<'_, AppState>) -> Result<ProxyStatus, String> {
    let proxy = state.proxy.read().await;
    let running = proxy.is_some();
    let config = state.proxy_config.read().await;
    let ca_installed = devflow_proxy::platform::verify_system_trust().unwrap_or(false);

    Ok(ProxyStatus {
        running,
        https_port: config.https_port,
        http_port: config.http_port,
        ca_installed,
        ca_path: devflow_proxy::ca::default_ca_cert_path()
            .display()
            .to_string(),
    })
}

#[tauri::command]
pub async fn list_containers() -> Result<Vec<ContainerEntry>, String> {
    let monitor = devflow_proxy::monitor::DockerMonitor::new().map_err(|e| e.to_string())?;

    let containers = monitor
        .get_running_containers()
        .await
        .map_err(|e| e.to_string())?;

    let mut entries = Vec::new();
    for container in &containers {
        let targets = devflow_proxy::discovery::extract_proxy_targets(container, "localhost");
        for target in targets {
            entries.push(ContainerEntry {
                https_url: format!("https://{}", target.domain),
                domain: target.domain,
                container_name: target.container_name,
                container_ip: target.container_ip,
                port: target.port,
                project: target.project,
                service: target.service,
                branch: target.branch,
            });
        }
    }

    Ok(entries)
}

#[tauri::command]
pub async fn get_certificate_status() -> Result<serde_json::Value, String> {
    let cert_path = devflow_proxy::ca::default_ca_cert_path();
    let exists = cert_path.exists();
    let installed = devflow_proxy::platform::verify_system_trust().unwrap_or(false);

    Ok(serde_json::json!({
        "exists": exists,
        "installed": installed,
        "path": cert_path.display().to_string(),
        "info": devflow_proxy::platform::trust_info(),
    }))
}

#[tauri::command]
pub async fn install_certificate() -> Result<(), String> {
    let ca =
        devflow_proxy::ca::CertificateAuthority::load_or_generate().map_err(|e| e.to_string())?;
    devflow_proxy::platform::install_system_trust(&ca).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn remove_certificate() -> Result<(), String> {
    devflow_proxy::platform::remove_system_trust().map_err(|e| e.to_string())
}

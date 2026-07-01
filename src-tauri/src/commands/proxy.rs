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
    pub workspace: Option<String>,
    /// Source of this proxy target for UI explanations.
    pub source: String,
    /// Reachable endpoint URL: `https://<domain>` for web services, or a native
    /// scheme such as `postgresql://<domain>:5432` for database/TCP endpoints.
    pub endpoint_url: String,
}

#[tauri::command]
pub async fn start_proxy(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<ProxyStatus, String> {
    let config = state.proxy_config.read().await.clone();

    let handle = devflow_proxy::run_proxy(config.clone())
        .await
        .map_err(crate::commands::format_error)?;

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
pub async fn stop_proxy(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<(), String> {
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
    let ca_installed = devflow_proxy::platform::verify_system_trust().unwrap_or(false);
    let status = ProxyStatus {
        running: false,
        https_port: config.https_port,
        http_port: config.http_port,
        ca_installed,
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
pub async fn list_containers(state: State<'_, AppState>) -> Result<Vec<ContainerEntry>, String> {
    let monitor =
        devflow_proxy::monitor::DockerMonitor::new().map_err(crate::commands::format_error)?;

    let containers = monitor
        .get_running_containers()
        .await
        .map_err(crate::commands::format_error)?;

    // Use the suffix the proxy is configured with (`.local` by default on macOS)
    // so GUI links match the names the proxy actually advertises and routes.
    let domain_suffix = state.proxy_config.read().await.domain_suffix.clone();

    let mut entries = Vec::new();
    for container in &containers {
        let targets = devflow_proxy::discovery::extract_proxy_targets(container, &domain_suffix);
        for target in targets {
            let source =
                if target.workspace.is_some() && target.container_name.starts_with("devflow-") {
                    "devflow-service"
                } else if target.project.is_some() && target.service.is_some() {
                    "docker-compose"
                } else {
                    "standalone-container"
                };
            entries.push(ContainerEntry {
                // Web -> https://<domain>; databases -> postgresql://<domain>:5432, etc.
                endpoint_url: devflow_proxy::endpoint::display_endpoint(
                    &target.domain,
                    target.port,
                ),
                domain: target.domain,
                container_name: target.container_name,
                container_ip: target.container_ip,
                port: target.port,
                project: target.project,
                service: target.service,
                workspace: target.workspace,
                source: source.to_string(),
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
    let ca = devflow_proxy::ca::CertificateAuthority::load_or_generate()
        .map_err(crate::commands::format_error)?;
    devflow_proxy::platform::install_system_trust(&ca).map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn remove_certificate() -> Result<(), String> {
    devflow_proxy::platform::remove_system_trust().map_err(crate::commands::format_error)
}

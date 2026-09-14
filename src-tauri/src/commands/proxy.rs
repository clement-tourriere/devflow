//! Thin adapter for the optional companion; no in-process networking runtime.
use crate::state::AppState;
pub use devflow_core::companion::ProxyStatus;
use tauri::{Emitter, State};
#[tauri::command]
pub async fn start_proxy(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<ProxyStatus, String> {
    let status = devflow_core::companion::start()
        .await
        .map_err(crate::commands::format_error)?;
    let mut settings = state.settings.write().await;
    settings.proxy_auto_start = true;
    settings.save().map_err(crate::commands::format_error)?;
    drop(settings);
    let _ = app.emit("proxy-status-changed", &status);
    crate::update_tray_menu(&app);
    Ok(status)
}
#[tauri::command]
pub async fn stop_proxy(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    devflow_core::companion::stop()
        .await
        .map_err(crate::commands::format_error)?;
    let mut settings = state.settings.write().await;
    settings.proxy_auto_start = false;
    settings.save().map_err(crate::commands::format_error)?;
    drop(settings);
    let status = devflow_core::companion::status()
        .await
        .map_err(crate::commands::format_error)?;
    let _ = app.emit("proxy-status-changed", status);
    crate::update_tray_menu(&app);
    Ok(())
}
#[tauri::command]
pub async fn get_proxy_status() -> Result<ProxyStatus, String> {
    devflow_core::companion::status()
        .await
        .map_err(crate::commands::format_error)
}
#[tauri::command]
pub async fn list_containers() -> Result<serde_json::Value, String> {
    if !devflow_core::companion::status()
        .await
        .map_err(crate::commands::format_error)?
        .running
    {
        return Ok(serde_json::json!([]));
    }
    devflow_core::companion::query("targets")
        .await
        .map_err(crate::commands::format_error)
}
#[tauri::command]
pub async fn get_certificate_status() -> Result<serde_json::Value, String> {
    let ca = devflow_core::companion::query("ca")
        .await
        .unwrap_or_default();
    Ok(
        serde_json::json!({"exists":ca["cert_path"].is_string(),"installed":ca["installed"].as_bool().unwrap_or(false),"path":ca["cert_path"],"info":ca["info"]}),
    )
}

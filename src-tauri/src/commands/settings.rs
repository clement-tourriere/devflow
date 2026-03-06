use crate::state::{AppSettings, AppState};
use devflow_core::config::GlobalConfig;
use tauri::State;

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> Result<AppSettings, String> {
    let settings = state.settings.read().await;
    Ok(settings.clone())
}

#[tauri::command]
pub async fn save_settings(
    state: State<'_, AppState>,
    settings: AppSettings,
) -> Result<(), String> {
    let mut settings = settings;
    if settings.terminal_renderer != "auto"
        && settings.terminal_renderer != "webgpu"
        && settings.terminal_renderer != "webgl2"
    {
        settings.terminal_renderer = "auto".to_string();
    }

    settings.terminal_font_size = settings.terminal_font_size.clamp(11, 24);

    settings.save().map_err(|e| e.to_string())?;

    // Sync smart_merge flag to GlobalConfig so CLI/TUI also respect it
    if let Ok(Some(mut global)) = GlobalConfig::load() {
        global.smart_merge = settings.smart_merge;
        let _ = global.save();
    } else {
        let global = GlobalConfig {
            smart_merge: settings.smart_merge,
            ..Default::default()
        };
        let _ = global.save();
    }

    *state.settings.write().await = settings;
    Ok(())
}

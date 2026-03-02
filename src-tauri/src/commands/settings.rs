use crate::state::{AppSettings, AppState};
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
    settings.save().map_err(|e| e.to_string())?;
    *state.settings.write().await = settings;
    Ok(())
}

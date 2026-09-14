use devflow_core::config::{document::ConfigDocument, Config};
use std::path::Path;

#[tauri::command]
pub async fn get_config_document(project_path: String) -> Result<ConfigDocument, String> {
    ConfigDocument::open(Path::new(&project_path)).map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn get_config_json(project_path: String) -> Result<serde_json::Value, String> {
    let document = get_config_document(project_path).await?;
    serde_json::to_value(document.config).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn save_config_json(
    project_path: String,
    config: serde_json::Value,
    original: Option<serde_json::Value>,
) -> Result<(), String> {
    let document = get_config_document(project_path).await?;
    let config: Config = serde_json::from_value(config).map_err(|e| e.to_string())?;
    if let Some(original) = original {
        let original: Config = serde_json::from_value(original).map_err(|e| e.to_string())?;
        if serde_json::to_value(&original).map_err(|e| e.to_string())?
            != serde_json::to_value(&document.config).map_err(|e| e.to_string())?
        {
            return Err("Configuration changed on disk; reload before saving".into());
        }
    }
    ConfigDocument::write_config(&document.path, &config).map_err(crate::commands::format_error)
}

// Names retained for existing clients; contents use the discovered format.
#[tauri::command]
pub async fn get_config_yaml(project_path: String) -> Result<String, String> {
    Ok(get_config_document(project_path).await?.content)
}

#[tauri::command]
pub async fn save_config_yaml(
    project_path: String,
    content: String,
    original: Option<String>,
) -> Result<(), String> {
    let mut document = get_config_document(project_path).await?;
    if let Some(original) = original {
        document.content = original;
    }
    document
        .save_text(&content)
        .map_err(crate::commands::format_error)
}

#[tauri::command]
pub async fn validate_config_yaml(
    content: String,
    format: Option<String>,
) -> Result<serde_json::Value, String> {
    match ConfigDocument::parse(format.as_deref().unwrap_or("yaml"), &content) {
        Ok(config) => Ok(serde_json::json!({"valid": true,
            "services": config.services.as_ref().map_or(0, Vec::len),
            "hooks": config.hooks.as_ref().map_or(0, |h| h.len())})),
        Err(e) => Ok(serde_json::json!({"valid": false, "error": e.to_string()})),
    }
}

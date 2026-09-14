use devflow_core::config::document::ConfigDocument;
use std::path::Path;

#[derive(serde::Serialize)]
pub struct ConfigTextDocument {
    path: std::path::PathBuf,
    format: String,
    content: String,
}

#[tauri::command]
pub async fn get_config_document(project_path: String) -> Result<ConfigTextDocument, String> {
    let doc =
        ConfigDocument::open(Path::new(&project_path)).map_err(crate::commands::format_error)?;
    Ok(ConfigTextDocument {
        path: doc.path,
        format: doc.format,
        content: doc.content,
    })
}

#[tauri::command]
pub async fn save_config_yaml(
    project_path: String,
    content: String,
    original: Option<String>,
) -> Result<(), String> {
    let mut document =
        ConfigDocument::open(Path::new(&project_path)).map_err(crate::commands::format_error)?;
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

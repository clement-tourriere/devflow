use std::path::Path;

#[tauri::command]
pub async fn get_config_yaml(project_path: String) -> Result<String, String> {
    let config_path = Path::new(&project_path).join(".devflow.yml");
    std::fs::read_to_string(&config_path).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn save_config_yaml(project_path: String, content: String) -> Result<(), String> {
    // Validate YAML first
    serde_yaml_ng::from_str::<devflow_core::config::Config>(&content)
        .map_err(|e| format!("Invalid YAML: {}", e))?;

    let config_path = Path::new(&project_path).join(".devflow.yml");
    std::fs::write(&config_path, &content).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn validate_config_yaml(content: String) -> Result<serde_json::Value, String> {
    match serde_yaml_ng::from_str::<devflow_core::config::Config>(&content) {
        Ok(config) => Ok(serde_json::json!({
            "valid": true,
            "services": config.services.as_ref().map(|s| s.len()).unwrap_or(0),
            "hooks": config.hooks.as_ref().map(|h| h.len()).unwrap_or(0),
        })),
        Err(e) => Ok(serde_json::json!({
            "valid": false,
            "error": e.to_string(),
        })),
    }
}

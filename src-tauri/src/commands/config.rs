use std::path::Path;

#[tauri::command]
pub async fn get_config_json(project_path: String) -> Result<serde_json::Value, String> {
    let config_path = Path::new(&project_path).join(".devflow.yml");
    let content = std::fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
    let config: devflow_core::config::Config =
        serde_yaml_ng::from_str(&content).map_err(|e| format!("Invalid YAML: {}", e))?;
    serde_json::to_value(&config).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn save_config_json(
    project_path: String,
    config: serde_json::Value,
) -> Result<(), String> {
    // Deserialize JSON → Config (validates)
    let config: devflow_core::config::Config =
        serde_json::from_value(config).map_err(|e| format!("Invalid config: {}", e))?;
    // Serialize to YAML
    let yaml = serde_yaml_ng::to_string(&config).map_err(|e| e.to_string())?;
    let config_path = Path::new(&project_path).join(".devflow.yml");
    std::fs::write(&config_path, &yaml).map_err(|e| e.to_string())
}

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

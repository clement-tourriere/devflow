use devflow_core::hooks;
use serde::Serialize;

#[derive(Serialize)]
pub struct HookPhaseEntry {
    pub phase: String,
    pub hooks: Vec<HookInfo>,
}

#[derive(Serialize)]
pub struct HookInfo {
    pub name: String,
    pub command: String,
    pub is_extended: bool,
}

#[tauri::command]
pub async fn list_hooks(project_path: String) -> Result<Vec<HookPhaseEntry>, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(|e| e.to_string())?;

    let hooks_config = config.hooks.unwrap_or_default();
    let mut entries = Vec::new();

    for (phase, hooks_map) in &hooks_config {
        let mut hooks_list = Vec::new();
        for (name, entry) in hooks_map {
            let (command, is_extended) = match entry {
                hooks::HookEntry::Simple(cmd) => (cmd.clone(), false),
                hooks::HookEntry::Extended(ext) => (ext.command.clone(), true),
            };
            hooks_list.push(HookInfo {
                name: name.clone(),
                command,
                is_extended,
            });
        }
        entries.push(HookPhaseEntry {
            phase: phase.to_string(),
            hooks: hooks_list,
        });
    }

    Ok(entries)
}

#[tauri::command]
pub async fn render_template(
    project_path: String,
    template: String,
    branch_name: Option<String>,
) -> Result<String, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(|e| e.to_string())?;

    let branch = branch_name.unwrap_or_else(|| "main".to_string());
    let context = hooks::build_hook_context(&config, &branch).await;

    let engine = hooks::TemplateEngine::new();
    engine
        .render(&template, &context)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_hook_variables(
    project_path: String,
    branch_name: Option<String>,
) -> Result<serde_json::Value, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(|e| e.to_string())?;

    let branch = branch_name.unwrap_or_else(|| "main".to_string());
    let context = hooks::build_hook_context(&config, &branch).await;

    serde_json::to_value(&context).map_err(|e| e.to_string())
}

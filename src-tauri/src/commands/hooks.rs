use devflow_core::hooks;
use serde::Serialize;
use std::path::Path;

fn is_sensitive_key(key: &str) -> bool {
    key.contains("password")
        || key.contains("secret")
        || key.contains("token")
        || key.contains("api_key")
        || key.contains("apikey")
}

fn redact_url_credentials(input: &str) -> String {
    let Some(scheme_pos) = input.find("://") else {
        return input.to_string();
    };

    let rest = &input[(scheme_pos + 3)..];
    let Some(at_pos) = rest.find('@') else {
        return input.to_string();
    };

    let auth = &rest[..at_pos];
    let host_part = &rest[(at_pos + 1)..];

    let Some(colon_pos) = auth.find(':') else {
        return input.to_string();
    };

    let user = &auth[..colon_pos];
    let scheme = &input[..scheme_pos];
    format!("{}://{}:***@{}", scheme, user, host_part)
}

fn redact_hook_variables(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                let key_lower = key.to_ascii_lowercase();

                if is_sensitive_key(&key_lower) {
                    *val = serde_json::Value::String("***".to_string());
                    continue;
                }

                if key_lower == "url" {
                    if let Some(raw) = val.as_str() {
                        *val = serde_json::Value::String(redact_url_credentials(raw));
                    }
                    continue;
                }

                redact_hook_variables(val);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_hook_variables(item);
            }
        }
        _ => {}
    }
}

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

#[derive(Serialize)]
pub struct VcsHooksActionResult {
    pub installed: bool,
    pub detail: String,
}

fn detect_hooks_installed(project_path: &Path, vcs: &dyn devflow_core::vcs::VcsProvider) -> bool {
    let hooks_dir = project_path.join(".git").join("hooks");
    if !hooks_dir.exists() {
        return false;
    }

    let post_checkout = hooks_dir.join("post-checkout");
    let post_merge = hooks_dir.join("post-merge");

    (post_checkout.exists() && vcs.is_devflow_hook(&post_checkout).unwrap_or(false))
        || (post_merge.exists() && vcs.is_devflow_hook(&post_merge).unwrap_or(false))
}

#[tauri::command]
pub async fn list_hooks(project_path: String) -> Result<Vec<HookPhaseEntry>, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config =
        devflow_core::config::Config::from_file(&config_path).map_err(|e| e.to_string())?;

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
    workspace_name: Option<String>,
) -> Result<String, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config =
        devflow_core::config::Config::from_file(&config_path).map_err(|e| e.to_string())?;

    let workspace = workspace_name.unwrap_or_else(|| "main".to_string());
    let context =
        hooks::build_hook_context(&config, std::path::Path::new(&project_path), &workspace).await;

    let engine = hooks::TemplateEngine::new();
    engine
        .render(&template, &context)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_hook_variables(
    project_path: String,
    workspace_name: Option<String>,
) -> Result<serde_json::Value, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config =
        devflow_core::config::Config::from_file(&config_path).map_err(|e| e.to_string())?;

    let workspace = workspace_name.unwrap_or_else(|| "main".to_string());
    let context =
        hooks::build_hook_context(&config, std::path::Path::new(&project_path), &workspace).await;

    let mut value = serde_json::to_value(&context).map_err(|e| e.to_string())?;
    redact_hook_variables(&mut value);
    Ok(value)
}

#[tauri::command]
pub async fn install_vcs_hooks(project_path: String) -> Result<VcsHooksActionResult, String> {
    let vcs = devflow_core::vcs::detect_vcs_provider(&project_path).map_err(|e| e.to_string())?;
    vcs.install_hooks().map_err(|e| e.to_string())?;

    let installed = detect_hooks_installed(Path::new(&project_path), vcs.as_ref());
    Ok(VcsHooksActionResult {
        installed,
        detail: if installed {
            "devflow hooks installed".to_string()
        } else {
            "Hook install completed, but verification did not find managed hooks".to_string()
        },
    })
}

#[tauri::command]
pub async fn uninstall_vcs_hooks(project_path: String) -> Result<VcsHooksActionResult, String> {
    let vcs = devflow_core::vcs::detect_vcs_provider(&project_path).map_err(|e| e.to_string())?;
    vcs.uninstall_hooks().map_err(|e| e.to_string())?;

    let installed = detect_hooks_installed(Path::new(&project_path), vcs.as_ref());
    Ok(VcsHooksActionResult {
        installed,
        detail: if installed {
            "Some managed hooks are still present after uninstall".to_string()
        } else {
            "devflow hooks removed".to_string()
        },
    })
}

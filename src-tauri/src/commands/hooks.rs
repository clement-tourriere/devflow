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
    /// For action-based hooks: the action type name (e.g. "write-env", "replace")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_type: Option<String>,
    /// Condition expression (if any)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// Whether the hook runs in background
    pub background: bool,
    /// Full serialized hook entry for edit support
    pub raw: serde_json::Value,
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
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

    let hooks_config = config.hooks.unwrap_or_default();
    let mut entries = Vec::new();

    for (phase, hooks_map) in &hooks_config {
        let mut hooks_list = Vec::new();
        for (name, entry) in hooks_map {
            let (command, is_extended, action_type, condition, background) = match entry {
                hooks::HookEntry::Simple(cmd) => (cmd.clone(), false, None, None, false),
                hooks::HookEntry::Extended(ext) => (
                    ext.command.clone(),
                    true,
                    None,
                    ext.condition.clone(),
                    ext.background,
                ),
                hooks::HookEntry::Action(act) => (
                    format!("action: {}", act.action.type_name()),
                    false,
                    Some(act.action.type_name().to_string()),
                    act.condition.clone(),
                    act.background,
                ),
            };
            let raw = serde_json::to_value(entry).unwrap_or(serde_json::Value::Null);
            hooks_list.push(HookInfo {
                name: name.clone(),
                command,
                is_extended,
                action_type,
                condition,
                background,
                raw,
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
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

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
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

    let workspace = workspace_name.unwrap_or_else(|| "main".to_string());
    let context =
        hooks::build_hook_context(&config, std::path::Path::new(&project_path), &workspace).await;

    let mut value = serde_json::to_value(&context).map_err(|e| e.to_string())?;
    redact_hook_variables(&mut value);
    Ok(value)
}

#[tauri::command]
pub async fn install_vcs_hooks(project_path: String) -> Result<VcsHooksActionResult, String> {
    let vcs = devflow_core::vcs::detect_vcs_provider(&project_path)
        .map_err(crate::commands::format_error)?;
    vcs.install_hooks().map_err(crate::commands::format_error)?;

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
    let vcs = devflow_core::vcs::detect_vcs_provider(&project_path)
        .map_err(crate::commands::format_error)?;
    vcs.uninstall_hooks()
        .map_err(crate::commands::format_error)?;

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

// ── New hook CRUD + action types ───────────────────────────────────────

/// Description of a built-in action type (for GUI form generation).
#[derive(Serialize)]
pub struct ActionTypeInfo {
    #[serde(rename = "type")]
    pub action_type: String,
    pub label: String,
    pub description: String,
    pub requires_approval: bool,
    pub fields: Vec<ActionFieldInfo>,
}

#[derive(Serialize)]
pub struct ActionFieldInfo {
    pub name: String,
    pub label: String,
    pub field_type: String, // "string", "text", "bool", "select", "key-value"
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>, // For "select" type
    pub template: bool, // Whether this field supports MiniJinja templates
}

#[tauri::command]
pub async fn get_action_types() -> Result<Vec<ActionTypeInfo>, String> {
    Ok(vec![
        ActionTypeInfo {
            action_type: "shell".to_string(),
            label: "Shell Command".to_string(),
            description: "Run a shell command".to_string(),
            requires_approval: true,
            fields: vec![ActionFieldInfo {
                name: "command".to_string(),
                label: "Command".to_string(),
                field_type: "text".to_string(),
                required: true,
                default_value: None,
                options: None,
                template: true,
            }],
        },
        ActionTypeInfo {
            action_type: "replace".to_string(),
            label: "Replace in File".to_string(),
            description: "Find and replace text in a file".to_string(),
            requires_approval: false,
            fields: vec![
                ActionFieldInfo {
                    name: "file".to_string(),
                    label: "File Path".to_string(),
                    field_type: "string".to_string(),
                    required: true,
                    default_value: None,
                    options: None,
                    template: true,
                },
                ActionFieldInfo {
                    name: "pattern".to_string(),
                    label: "Pattern".to_string(),
                    field_type: "string".to_string(),
                    required: true,
                    default_value: None,
                    options: None,
                    template: true,
                },
                ActionFieldInfo {
                    name: "replacement".to_string(),
                    label: "Replacement".to_string(),
                    field_type: "string".to_string(),
                    required: true,
                    default_value: None,
                    options: None,
                    template: true,
                },
                ActionFieldInfo {
                    name: "regex".to_string(),
                    label: "Use Regex".to_string(),
                    field_type: "bool".to_string(),
                    required: false,
                    default_value: Some("false".to_string()),
                    options: None,
                    template: false,
                },
                ActionFieldInfo {
                    name: "create_if_missing".to_string(),
                    label: "Create If Missing".to_string(),
                    field_type: "bool".to_string(),
                    required: false,
                    default_value: Some("false".to_string()),
                    options: None,
                    template: false,
                },
            ],
        },
        ActionTypeInfo {
            action_type: "write-file".to_string(),
            label: "Write File".to_string(),
            description: "Write content to a file".to_string(),
            requires_approval: false,
            fields: vec![
                ActionFieldInfo {
                    name: "path".to_string(),
                    label: "File Path".to_string(),
                    field_type: "string".to_string(),
                    required: true,
                    default_value: None,
                    options: None,
                    template: true,
                },
                ActionFieldInfo {
                    name: "content".to_string(),
                    label: "Content".to_string(),
                    field_type: "text".to_string(),
                    required: true,
                    default_value: None,
                    options: None,
                    template: true,
                },
                ActionFieldInfo {
                    name: "mode".to_string(),
                    label: "Write Mode".to_string(),
                    field_type: "select".to_string(),
                    required: false,
                    default_value: Some("overwrite".to_string()),
                    options: Some(vec![
                        "overwrite".to_string(),
                        "append".to_string(),
                        "create-only".to_string(),
                    ]),
                    template: false,
                },
            ],
        },
        ActionTypeInfo {
            action_type: "write-env".to_string(),
            label: "Write Env File".to_string(),
            description: "Write a .env file with key-value pairs".to_string(),
            requires_approval: false,
            fields: vec![
                ActionFieldInfo {
                    name: "path".to_string(),
                    label: "File Path".to_string(),
                    field_type: "string".to_string(),
                    required: true,
                    default_value: Some(".env.local".to_string()),
                    options: None,
                    template: true,
                },
                ActionFieldInfo {
                    name: "vars".to_string(),
                    label: "Environment Variables".to_string(),
                    field_type: "key-value".to_string(),
                    required: true,
                    default_value: None,
                    options: None,
                    template: true,
                },
                ActionFieldInfo {
                    name: "mode".to_string(),
                    label: "Write Mode".to_string(),
                    field_type: "select".to_string(),
                    required: false,
                    default_value: Some("overwrite".to_string()),
                    options: Some(vec!["overwrite".to_string(), "merge".to_string()]),
                    template: false,
                },
            ],
        },
        ActionTypeInfo {
            action_type: "copy".to_string(),
            label: "Copy File".to_string(),
            description: "Copy a file from one location to another".to_string(),
            requires_approval: false,
            fields: vec![
                ActionFieldInfo {
                    name: "from".to_string(),
                    label: "Source Path".to_string(),
                    field_type: "string".to_string(),
                    required: true,
                    default_value: None,
                    options: None,
                    template: true,
                },
                ActionFieldInfo {
                    name: "to".to_string(),
                    label: "Destination Path".to_string(),
                    field_type: "string".to_string(),
                    required: true,
                    default_value: None,
                    options: None,
                    template: true,
                },
                ActionFieldInfo {
                    name: "overwrite".to_string(),
                    label: "Overwrite".to_string(),
                    field_type: "bool".to_string(),
                    required: false,
                    default_value: Some("true".to_string()),
                    options: None,
                    template: false,
                },
            ],
        },
        ActionTypeInfo {
            action_type: "docker-exec".to_string(),
            label: "Docker Exec".to_string(),
            description: "Execute a command inside a Docker container".to_string(),
            requires_approval: true,
            fields: vec![
                ActionFieldInfo {
                    name: "container".to_string(),
                    label: "Container Name".to_string(),
                    field_type: "string".to_string(),
                    required: true,
                    default_value: None,
                    options: None,
                    template: true,
                },
                ActionFieldInfo {
                    name: "command".to_string(),
                    label: "Command".to_string(),
                    field_type: "text".to_string(),
                    required: true,
                    default_value: None,
                    options: None,
                    template: true,
                },
                ActionFieldInfo {
                    name: "user".to_string(),
                    label: "User".to_string(),
                    field_type: "string".to_string(),
                    required: false,
                    default_value: None,
                    options: None,
                    template: true,
                },
            ],
        },
        ActionTypeInfo {
            action_type: "http".to_string(),
            label: "HTTP Request".to_string(),
            description: "Make an HTTP request".to_string(),
            requires_approval: false,
            fields: vec![
                ActionFieldInfo {
                    name: "url".to_string(),
                    label: "URL".to_string(),
                    field_type: "string".to_string(),
                    required: true,
                    default_value: None,
                    options: None,
                    template: true,
                },
                ActionFieldInfo {
                    name: "method".to_string(),
                    label: "Method".to_string(),
                    field_type: "select".to_string(),
                    required: false,
                    default_value: Some("GET".to_string()),
                    options: Some(vec![
                        "GET".to_string(),
                        "POST".to_string(),
                        "PUT".to_string(),
                        "PATCH".to_string(),
                        "DELETE".to_string(),
                    ]),
                    template: false,
                },
                ActionFieldInfo {
                    name: "headers".to_string(),
                    label: "Headers".to_string(),
                    field_type: "key-value".to_string(),
                    required: false,
                    default_value: None,
                    options: None,
                    template: true,
                },
                ActionFieldInfo {
                    name: "body".to_string(),
                    label: "Body".to_string(),
                    field_type: "text".to_string(),
                    required: false,
                    default_value: None,
                    options: None,
                    template: true,
                },
            ],
        },
        ActionTypeInfo {
            action_type: "notify".to_string(),
            label: "Notification".to_string(),
            description: "Send a desktop notification".to_string(),
            requires_approval: false,
            fields: vec![
                ActionFieldInfo {
                    name: "title".to_string(),
                    label: "Title".to_string(),
                    field_type: "string".to_string(),
                    required: true,
                    default_value: Some("devflow".to_string()),
                    options: None,
                    template: true,
                },
                ActionFieldInfo {
                    name: "message".to_string(),
                    label: "Message".to_string(),
                    field_type: "string".to_string(),
                    required: true,
                    default_value: None,
                    options: None,
                    template: true,
                },
                ActionFieldInfo {
                    name: "level".to_string(),
                    label: "Level".to_string(),
                    field_type: "select".to_string(),
                    required: false,
                    default_value: Some("info".to_string()),
                    options: Some(vec![
                        "info".to_string(),
                        "success".to_string(),
                        "warning".to_string(),
                        "error".to_string(),
                    ]),
                    template: false,
                },
            ],
        },
    ])
}

#[tauri::command]
pub async fn save_hooks(project_path: String, hooks: serde_json::Value) -> Result<(), String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config_content = std::fs::read_to_string(&config_path).map_err(|e| e.to_string())?;

    // Parse existing config as a generic YAML value to preserve other sections
    let mut doc: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&config_content).map_err(|e| e.to_string())?;

    // Validate that the hooks value is valid
    let hooks_yaml: serde_yaml_ng::Value =
        serde_json::from_value::<serde_yaml_ng::Value>(hooks).map_err(|e| e.to_string())?;

    // Update only the hooks section
    if let serde_yaml_ng::Value::Mapping(ref mut map) = doc {
        map.insert(
            serde_yaml_ng::Value::String("hooks".to_string()),
            hooks_yaml,
        );
    }

    let output = serde_yaml_ng::to_string(&doc).map_err(|e| e.to_string())?;
    std::fs::write(&config_path, output).map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
pub async fn validate_hook(
    project_path: String,
    hook: serde_json::Value,
    workspace_name: Option<String>,
) -> Result<serde_json::Value, String> {
    // Try to deserialize the hook entry
    let _entry: hooks::HookEntry =
        serde_json::from_value(hook).map_err(|e| format!("Invalid hook format: {}", e))?;

    // Check if template rendering works
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;
    let workspace = workspace_name.unwrap_or_else(|| "main".to_string());
    let _context =
        hooks::build_hook_context(&config, std::path::Path::new(&project_path), &workspace).await;

    Ok(serde_json::json!({ "valid": true }))
}

#[tauri::command]
pub async fn preview_hook(
    project_path: String,
    hook: serde_json::Value,
    workspace_name: Option<String>,
) -> Result<serde_json::Value, String> {
    let entry: hooks::HookEntry =
        serde_json::from_value(hook).map_err(|e| format!("Invalid hook format: {}", e))?;

    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;
    let workspace = workspace_name.unwrap_or_else(|| "main".to_string());
    let context =
        hooks::build_hook_context(&config, std::path::Path::new(&project_path), &workspace).await;

    let engine = hooks::TemplateEngine::new();

    // Render template fields based on entry type
    let preview = match &entry {
        hooks::HookEntry::Simple(cmd) => {
            let rendered = engine.render(cmd, &context).map_err(|e| e.to_string())?;
            serde_json::json!({
                "type": "simple",
                "rendered_command": rendered,
            })
        }
        hooks::HookEntry::Extended(ext) => {
            let rendered = engine
                .render(&ext.command, &context)
                .map_err(|e| e.to_string())?;
            serde_json::json!({
                "type": "extended",
                "rendered_command": rendered,
            })
        }
        hooks::HookEntry::Action(act) => {
            let type_name = act.action.type_name();
            serde_json::json!({
                "type": "action",
                "action_type": type_name,
                "requires_approval": act.action.requires_approval(),
            })
        }
    };

    Ok(preview)
}

#[tauri::command]
pub async fn run_hook(
    project_path: String,
    phase: String,
    hook_name: String,
    workspace_name: Option<String>,
) -> Result<serde_json::Value, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

    let hooks_config = config.hooks.clone().unwrap_or_default();
    let hook_phase: hooks::HookPhase = phase.parse().unwrap();

    // Find the specific hook
    let phase_hooks = hooks_config
        .get(&hook_phase)
        .ok_or_else(|| format!("No hooks configured for phase: {}", phase))?;
    let _entry = phase_hooks
        .get(&hook_name)
        .ok_or_else(|| format!("Hook '{}' not found in phase '{}'", hook_name, phase))?;

    let workspace = workspace_name.unwrap_or_else(|| {
        devflow_core::vcs::detect_vcs_provider(&project_path)
            .ok()
            .and_then(|vcs| vcs.current_workspace().ok().flatten())
            .unwrap_or_else(|| "main".to_string())
    });

    let mut context =
        hooks::build_hook_context(&config, std::path::Path::new(&project_path), &workspace).await;
    context.trigger_source = "gui".to_string();

    // Build a mini config with just the one hook
    let mut single_hooks = hooks::IndexMap::new();
    let mut single_phase = hooks::IndexMap::new();
    single_phase.insert(hook_name.clone(), _entry.clone());
    single_hooks.insert(hook_phase.clone(), single_phase);

    // Use worktree path as working dir when available (e.g. for `mise trust`)
    let working_dir = context
        .worktree_path
        .as_ref()
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| std::path::PathBuf::from(&project_path));

    let engine = hooks::HookEngine::new_no_approval(single_hooks, working_dir);

    let result = engine
        .run_phase(&hook_phase, &context)
        .await
        .map_err(|e| e.to_string())?;

    Ok(serde_json::json!({
        "succeeded": result.succeeded,
        "failed": result.failed,
        "skipped": result.skipped,
        "background": result.background,
        "errors": result.errors,
    }))
}

// ── Hook recipes ───────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct RecipeInfo {
    pub name: String,
    pub description: String,
    pub category: String,
    pub hooks_preview: Vec<RecipeHookPreview>,
}

#[derive(Serialize)]
pub struct RecipeHookPreview {
    pub phase: String,
    pub hook_name: String,
    pub command_summary: String,
}

#[derive(Serialize)]
pub struct InstallRecipeResult {
    pub hooks_added: usize,
    pub hooks_skipped: usize,
}

#[tauri::command]
pub async fn get_recipes() -> Result<Vec<RecipeInfo>, String> {
    let recipes = devflow_core::hooks::recipes::builtin_recipes();
    Ok(recipes
        .iter()
        .map(|r| {
            let info = r.to_info();
            RecipeInfo {
                name: info.name,
                description: info.description,
                category: info.category,
                hooks_preview: info
                    .hooks_preview
                    .into_iter()
                    .map(|h| RecipeHookPreview {
                        phase: h.phase,
                        hook_name: h.hook_name,
                        command_summary: h.command_summary,
                    })
                    .collect(),
            }
        })
        .collect())
}

#[tauri::command]
pub async fn install_recipe(
    project_path: String,
    recipe_name: String,
) -> Result<InstallRecipeResult, String> {
    let recipe = devflow_core::hooks::recipes::find_recipe(&recipe_name)
        .ok_or_else(|| format!("Recipe '{}' not found", recipe_name))?;

    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

    let mut hooks_config = config.hooks.unwrap_or_default();
    let result =
        devflow_core::hooks::recipes::merge_recipe_into_config(&mut hooks_config, &recipe);

    if result.hooks_added > 0 {
        // Write back to config
        let content = std::fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
        let mut doc: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&content).map_err(|e| e.to_string())?;
        let hooks_yaml =
            serde_yaml_ng::to_value(&hooks_config).map_err(|e| e.to_string())?;
        if let serde_yaml_ng::Value::Mapping(ref mut map) = doc {
            map.insert(
                serde_yaml_ng::Value::String("hooks".to_string()),
                hooks_yaml,
            );
        }
        let output = serde_yaml_ng::to_string(&doc).map_err(|e| e.to_string())?;
        std::fs::write(&config_path, output).map_err(|e| e.to_string())?;
    }

    Ok(InstallRecipeResult {
        hooks_added: result.hooks_added,
        hooks_skipped: result.hooks_skipped,
    })
}

#[tauri::command]
pub async fn install_recipes(
    project_path: String,
    recipe_names: Vec<String>,
) -> Result<InstallRecipeResult, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

    let mut hooks_config = config.hooks.unwrap_or_default();
    let mut total_added = 0;
    let mut total_skipped = 0;

    for name in &recipe_names {
        let recipe = devflow_core::hooks::recipes::find_recipe(name)
            .ok_or_else(|| format!("Recipe '{}' not found", name))?;
        let result =
            devflow_core::hooks::recipes::merge_recipe_into_config(&mut hooks_config, &recipe);
        total_added += result.hooks_added;
        total_skipped += result.hooks_skipped;
    }

    if total_added > 0 {
        let content = std::fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
        let mut doc: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&content).map_err(|e| e.to_string())?;
        let hooks_yaml =
            serde_yaml_ng::to_value(&hooks_config).map_err(|e| e.to_string())?;
        if let serde_yaml_ng::Value::Mapping(ref mut map) = doc {
            map.insert(
                serde_yaml_ng::Value::String("hooks".to_string()),
                hooks_yaml,
            );
        }
        let output = serde_yaml_ng::to_string(&doc).map_err(|e| e.to_string())?;
        std::fs::write(&config_path, output).map_err(|e| e.to_string())?;
    }

    Ok(InstallRecipeResult {
        hooks_added: total_added,
        hooks_skipped: total_skipped,
    })
}

/// Get VCS trigger mappings.
#[tauri::command]
pub async fn get_trigger_mappings(project_path: String) -> Result<serde_json::Value, String> {
    let config_path = std::path::Path::new(&project_path).join(".devflow.yml");
    let config = devflow_core::config::Config::from_file(&config_path)
        .map_err(crate::commands::format_error)?;

    let triggers = config.triggers.unwrap_or_default();
    let mappings = triggers.git_mappings();

    serde_json::to_value(&mappings).map_err(|e| e.to_string())
}

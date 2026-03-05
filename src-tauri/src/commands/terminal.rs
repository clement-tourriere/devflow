use crate::state::AppState;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use devflow_terminal::{SessionMetadata, TerminalSessionConfig, TerminalSessionInfo};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tauri::{Emitter, State};

/// Build shell + args for a sandboxed terminal session.
/// On macOS, wraps the user's shell with `sandbox-exec -f <profile>`.
/// On other platforms, returns defaults (command guard only, no OS wrapping).
fn build_sandboxed_shell(
    working_dir: &str,
    project_path: &Option<String>,
) -> (Option<String>, Option<Vec<String>>) {
    let workspace_dir = Path::new(working_dir);

    // Load sandbox config from project if available
    let sandbox_config = project_path.as_ref().and_then(|pp| {
        let config_path = Path::new(pp).join(".devflow.yml");
        devflow_core::config::Config::from_file(&config_path)
            .ok()
            .and_then(|c| c.sandbox.clone())
    });

    let (extra_read, extra_write): (Vec<PathBuf>, Vec<PathBuf>) = sandbox_config
        .as_ref()
        .and_then(|sc| sc.filesystem.as_ref())
        .map(|fs| {
            (
                fs.extra_read.iter().map(PathBuf::from).collect(),
                fs.extra_write.iter().map(PathBuf::from).collect(),
            )
        })
        .unwrap_or_default();

    #[cfg(target_os = "macos")]
    {
        use devflow_core::sandbox::seatbelt;

        let profile =
            seatbelt::generate_seatbelt_profile(workspace_dir, &extra_read, &extra_write);

        // Write profile to a persistent location (not tempfile, since the terminal lives long)
        let profile_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("devflow")
            .join("sandbox-profiles");
        let _ = std::fs::create_dir_all(&profile_dir);
        let profile_path = profile_dir.join(format!(
            "terminal-{}-{}.sb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        if std::fs::write(&profile_path, &profile).is_err() {
            return (None, None);
        }

        let user_shell =
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());

        (
            Some("sandbox-exec".to_string()),
            Some(vec![
                "-f".to_string(),
                profile_path.display().to_string(),
                user_shell.clone(),
                "-l".to_string(),
            ]),
        )
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (workspace_dir, extra_read, extra_write);
        // On non-macOS, no OS-level shell wrapping (command guard applies to devflow commands)
        (None, None)
    }
}

/// Check if a workspace is sandboxed by looking up its state in the registry.
fn is_workspace_sandboxed(project_path: &str, workspace_name: &str) -> bool {
    let project_dir = Path::new(project_path);
    let Ok(state_mgr) = devflow_core::state::LocalStateManager::new() else {
        return false;
    };
    state_mgr
        .get_workspace_by_dir(project_dir, workspace_name)
        .map(|ws| ws.sandboxed)
        .unwrap_or(false)
}

#[derive(Clone, Serialize)]
struct TerminalOutputEvent {
    session_id: String,
    data: String, // base64-encoded
}

#[derive(Clone, Serialize)]
struct TerminalExitEvent {
    session_id: String,
}

/// Build environment variables for a terminal session by gathering
/// connection info from all configured services.
async fn build_service_env(project_path: &str, workspace_name: &str) -> HashMap<String, String> {
    let mut env = HashMap::new();

    let config_path = Path::new(project_path).join(".devflow.yml");
    let cfg = match devflow_core::config::Config::from_file(&config_path) {
        Ok(c) => c,
        Err(_) => return env,
    };

    let named_services = cfg.resolve_services();
    let default_service = named_services
        .iter()
        .find(|s| s.default)
        .or(named_services.first());

    for svc in &named_services {
        let provider =
            match devflow_core::services::factory::create_provider_from_named_config(&cfg, svc)
                .await
            {
                Ok(p) => p,
                Err(_) => continue,
            };

        let info = match provider.get_connection_info(workspace_name).await {
            Ok(i) => i,
            Err(_) => continue,
        };

        let prefix = svc.name.to_uppercase().replace('-', "_");
        env.insert(format!("DEVFLOW_{}_HOST", prefix), info.host.clone());
        env.insert(format!("DEVFLOW_{}_PORT", prefix), info.port.to_string());
        env.insert(
            format!("DEVFLOW_{}_DATABASE", prefix),
            info.database.clone(),
        );
        env.insert(format!("DEVFLOW_{}_USER", prefix), info.user.clone());
        if let Some(ref pw) = info.password {
            env.insert(format!("DEVFLOW_{}_PASSWORD", prefix), pw.clone());
        }
        if let Some(ref url) = info.connection_string {
            env.insert(format!("DEVFLOW_{}_URL", prefix), url.clone());
        }

        // Set DATABASE_URL for the default service
        if default_service.is_some_and(|d| d.name == svc.name) {
            if let Some(ref url) = info.connection_string {
                env.insert("DATABASE_URL".to_string(), url.clone());
            }
        }
    }

    env
}

#[tauri::command]
pub async fn create_terminal(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    project_path: Option<String>,
    workspace_name: Option<String>,
) -> Result<TerminalSessionInfo, String> {
    // Determine working directory
    let working_dir = if let (Some(ref pp), Some(ref workspace)) = (&project_path, &workspace_name)
    {
        // Try to find worktree path for this workspace
        let vcs = devflow_core::vcs::detect_vcs_provider(pp).ok();
        let worktree_path = vcs
            .as_ref()
            .and_then(|v| v.worktree_path(workspace).ok().flatten());
        worktree_path
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| pp.clone())
    } else if let Some(ref pp) = project_path {
        pp.clone()
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .display()
            .to_string()
    };

    // Build environment
    let mut env = HashMap::new();
    if let Some(ref workspace) = workspace_name {
        env.insert("DEVFLOW_BRANCH".to_string(), workspace.clone());
    }
    if let Some(ref pp) = project_path {
        env.insert("DEVFLOW_PROJECT".to_string(), pp.clone());
    }

    // Inject service connection info
    if let (Some(ref pp), Some(ref workspace)) = (&project_path, &workspace_name) {
        let service_env = build_service_env(pp, workspace).await;
        env.extend(service_env);
    }

    // Build label
    let label = if let (Some(ref pp), Some(ref workspace)) = (&project_path, &workspace_name) {
        let project_name = Path::new(pp)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string());
        format!("{}/{}", project_name, workspace)
    } else if let Some(ref pp) = project_path {
        Path::new(pp)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "terminal".to_string())
    } else {
        "terminal".to_string()
    };

    // Check if this workspace is sandboxed — if so, wrap the shell with sandbox-exec
    let sandbox_state = if let (Some(ref pp), Some(ref workspace)) = (&project_path, &workspace_name)
    {
        is_workspace_sandboxed(pp, workspace)
    } else {
        false
    };

    let (shell, shell_args) = if sandbox_state {
        build_sandboxed_shell(&working_dir, &project_path)
    } else {
        (None, None)
    };

    let config = TerminalSessionConfig {
        working_directory: PathBuf::from(&working_dir),
        environment: env,
        shell,
        shell_args,
        initial_command: None,
        rows: 24,
        cols: 80,
    };

    let metadata = SessionMetadata {
        label: label.clone(),
        project_path: project_path.clone(),
        workspace_name: workspace_name.clone(),
    };

    let (info, mut output_rx) = state
        .terminals
        .create_session(config, metadata)
        .await
        .map_err(|e| e.to_string())?;

    // Spawn task to forward PTY output as events to the frontend
    let session_id = info.id.clone();
    let app_handle = app.clone();
    tokio::spawn(async move {
        while let Some(data) = output_rx.recv().await {
            let event = TerminalOutputEvent {
                session_id: session_id.clone(),
                data: BASE64.encode(&data),
            };
            let _ = app_handle.emit("terminal-output", &event);
        }
        // PTY output ended — emit exit event
        let exit_event = TerminalExitEvent {
            session_id: session_id.clone(),
        };
        let _ = app_handle.emit("terminal-exit", &exit_event);
    });

    Ok(info)
}

#[tauri::command]
pub async fn list_terminals(
    state: State<'_, AppState>,
) -> Result<Vec<TerminalSessionInfo>, String> {
    Ok(state.terminals.list_sessions().await)
}

#[tauri::command]
pub async fn write_terminal(
    state: State<'_, AppState>,
    session_id: String,
    data: String, // base64-encoded
) -> Result<(), String> {
    let bytes = BASE64
        .decode(&data)
        .map_err(|e| format!("Invalid base64: {}", e))?;
    state
        .terminals
        .write_input(&session_id, &bytes)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn resize_terminal(
    state: State<'_, AppState>,
    session_id: String,
    rows: u16,
    cols: u16,
) -> Result<(), String> {
    state
        .terminals
        .resize(&session_id, rows, cols)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn close_terminal(state: State<'_, AppState>, session_id: String) -> Result<(), String> {
    state
        .terminals
        .close_session(&session_id)
        .await
        .map_err(|e| e.to_string())
}

use crate::state::AppState;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use devflow_terminal::{SessionMetadata, TerminalSessionConfig, TerminalSessionInfo};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tauri::{Emitter, State};

#[cfg(target_os = "macos")]
fn sandbox_shell_env(working_dir: &str) -> HashMap<String, String> {
    let mut env = HashMap::new();
    let workspace_dir = PathBuf::from(working_dir);
    let real_home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/Users/unknown"));
    let shell_home = workspace_dir.join(".devflow-shell-home");
    let cache_dir = shell_home.join(".cache");
    let state_dir = shell_home.join(".local/state");
    let data_dir = shell_home.join(".local/share");
    let config_dir = shell_home.join(".config");
    let zdotdir = shell_home.join(".zsh");
    let tmp_dir = std::env::temp_dir().join("devflow-shell");

    env.insert("HOME".to_string(), shell_home.display().to_string());
    env.insert(
        "DEVFLOW_REAL_HOME".to_string(),
        real_home.display().to_string(),
    );
    env.insert(
        "XDG_CACHE_HOME".to_string(),
        cache_dir.display().to_string(),
    );
    env.insert(
        "XDG_STATE_HOME".to_string(),
        state_dir.display().to_string(),
    );
    env.insert(
        "XDG_DATA_HOME".to_string(),
        real_home.join(".local/share").display().to_string(),
    );
    env.insert(
        "XDG_CONFIG_HOME".to_string(),
        real_home.join(".config").display().to_string(),
    );
    env.insert("ZDOTDIR".to_string(), zdotdir.display().to_string());
    env.insert(
        "ZSH_COMPDUMP".to_string(),
        zdotdir.join(".zcompdump").display().to_string(),
    );
    env.insert(
        "STARSHIP_CACHE".to_string(),
        cache_dir.join("starship").display().to_string(),
    );
    env.insert("STARSHIP_LOG".to_string(), "error".to_string());
    env.insert(
        "STARSHIP_CONFIG".to_string(),
        real_home
            .join(".config/starship.toml")
            .display()
            .to_string(),
    );
    env.insert(
        "MISE_CACHE_DIR".to_string(),
        cache_dir.join("mise").display().to_string(),
    );
    env.insert(
        "MISE_STATE_DIR".to_string(),
        state_dir.join("mise").display().to_string(),
    );
    env.insert(
        "MISE_DATA_DIR".to_string(),
        data_dir.join("mise").display().to_string(),
    );
    env.insert(
        "MISE_GLOBAL_CONFIG_FILE".to_string(),
        real_home
            .join(".config/mise/config.toml")
            .display()
            .to_string(),
    );
    env.insert(
        "DOCKER_CONFIG".to_string(),
        config_dir.join("docker").display().to_string(),
    );
    env.insert("TMPDIR".to_string(), tmp_dir.display().to_string());
    env.insert("LC_ALL".to_string(), "en_US.UTF-8".to_string());
    env.insert("LANG".to_string(), "en_US.UTF-8".to_string());

    env
}

#[cfg(target_os = "macos")]
fn prepare_sandbox_shell_home(working_dir: &str) -> Result<HashMap<String, String>, String> {
    let shell_env = sandbox_shell_env(working_dir);
    let home_dir = shell_env
        .get("HOME")
        .cloned()
        .ok_or_else(|| "Missing sandbox HOME".to_string())?;
    let xdg_cache_home = shell_env
        .get("XDG_CACHE_HOME")
        .cloned()
        .ok_or_else(|| "Missing sandbox XDG_CACHE_HOME".to_string())?;
    let xdg_state_home = shell_env
        .get("XDG_STATE_HOME")
        .cloned()
        .ok_or_else(|| "Missing sandbox XDG_STATE_HOME".to_string())?;
    let xdg_data_home = shell_env
        .get("XDG_DATA_HOME")
        .cloned()
        .ok_or_else(|| "Missing sandbox XDG_DATA_HOME".to_string())?;
    let xdg_config_home = shell_env
        .get("XDG_CONFIG_HOME")
        .cloned()
        .ok_or_else(|| "Missing sandbox XDG_CONFIG_HOME".to_string())?;
    let zdotdir = shell_env
        .get("ZDOTDIR")
        .cloned()
        .ok_or_else(|| "Missing sandbox ZDOTDIR".to_string())?;
    let docker_config = shell_env
        .get("DOCKER_CONFIG")
        .cloned()
        .ok_or_else(|| "Missing sandbox DOCKER_CONFIG".to_string())?;
    let tmpdir = shell_env
        .get("TMPDIR")
        .cloned()
        .ok_or_else(|| "Missing sandbox TMPDIR".to_string())?;

    let required_dirs = [
        home_dir.clone(),
        xdg_cache_home.clone(),
        xdg_state_home.clone(),
        xdg_data_home.clone(),
        xdg_config_home.clone(),
        zdotdir.clone(),
        docker_config.clone(),
        tmpdir.clone(),
        Path::new(&xdg_cache_home)
            .join("oh-my-zsh/completions")
            .display()
            .to_string(),
        Path::new(&xdg_cache_home)
            .join("starship")
            .display()
            .to_string(),
        Path::new(&xdg_cache_home)
            .join("mise")
            .display()
            .to_string(),
        Path::new(&xdg_state_home)
            .join("mise")
            .display()
            .to_string(),
        Path::new(&xdg_data_home).join("mise").display().to_string(),
    ];

    for dir in required_dirs {
        if dir.is_empty() {
            continue;
        }
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("Failed to prepare sandbox shell directory '{}': {}", dir, e))?;
    }

    let home =
        dirs::home_dir().ok_or_else(|| "Failed to resolve user home directory".to_string())?;
    let zdotdir = PathBuf::from(zdotdir);

    for (source_name, target_name) in [
        (".zshenv", ".zshenv"),
        (".zprofile", ".zprofile"),
        (".zshrc", ".zshrc"),
        (".profile", ".profile"),
    ] {
        let source = home.join(source_name);
        let target = zdotdir.join(target_name);
        if source.is_file() {
            let mut content = std::fs::read_to_string(&source).map_err(|e| {
                format!(
                    "Failed to read shell config '{}' for sandbox home: {}",
                    source.display(),
                    e
                )
            })?;

            if source_name == ".zshrc" {
                content = content.replace(
                    "export ZSH=$HOME/.oh-my-zsh",
                    &format!("export ZSH={}/.oh-my-zsh", home.display()),
                );
                content = content.replace(
                    "$HOME/.cargo/env",
                    &format!("{}/.cargo/env", home.display()),
                );
                content = content.replace(
                    "$HOME/.local/bin",
                    &format!("{}/.local/bin", home.display()),
                );
                content = content.replace(
                    "source $ZSH/oh-my-zsh.sh",
                    "plugins=(git)\nsource $ZSH/oh-my-zsh.sh",
                );
                content = content.replace("$HOME/.bun", &format!("{}/.bun", home.display()));
                content = content.replace(
                    "autoload -U compinit && compinit",
                    "# devflow sandbox: oh-my-zsh handles compinit",
                );
                // Keep starship init so the sandboxed shell has the user's prompt + colors.
                // STARSHIP_CACHE and STARSHIP_CONFIG env vars are already redirected.
                content = content.replace(
                    "eval \"$(fnm env --use-on-cd --shell zsh)\"",
                    "# devflow sandbox: disabled fnm use-on-cd",
                );
                content =
                    content.replace("eval \"`fnm env`\"", "# devflow sandbox: disabled fnm env");
                content = content.replace(
                    "eval \"$(/Users/ctourriere/.local/bin/mise activate zsh)\"",
                    "# devflow sandbox: disabled mise activate",
                );
                content.push_str(&format!(
                    "\nexport ZSH_CACHE_DIR=\"{}/oh-my-zsh\"\n",
                    xdg_cache_home
                ));
                content.push_str("\nexport ZSH_DISABLE_COMPFIX=true\n");
                content.push_str("export DISABLE_AUTO_UPDATE=true\n");
                content.push_str("export DISABLE_MAGIC_FUNCTIONS=true\n");
                content.push_str("unsetopt correct_all\n");
                content.push_str("alias l='ls -lah'\n");
                content.push_str("alias ll='ls -lh'\n");
                content.push_str("alias la='ls -lAh'\n");
            }

            if source_name == ".zshenv" || source_name == ".profile" {
                content = content.replace(
                    "$HOME/.cargo/env",
                    &format!("{}/.cargo/env", home.display()),
                );
                content = content.replace(
                    "$HOME/.local/bin",
                    &format!("{}/.local/bin", home.display()),
                );
            }

            if source_name == ".zprofile" {
                content = content.replace(
                    "source ~/.orbstack/shell/init.zsh 2>/dev/null || :",
                    "# devflow sandbox: disabled OrbStack init",
                );
            }

            std::fs::write(&target, content).map_err(|e| {
                format!(
                    "Failed to write shell config '{}' into sandbox home: {}",
                    target.display(),
                    e
                )
            })?;
        }
    }

    Ok(shell_env)
}

#[cfg(target_os = "macos")]
fn sandbox_shell_candidates() -> Vec<String> {
    use std::collections::HashSet;
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    if let Ok(shell) = std::env::var("SHELL") {
        let shell_path = Path::new(&shell);
        if shell_path.is_file() && seen.insert(shell.clone()) {
            candidates.push(shell);
        }
    }

    for fallback in ["/bin/zsh", "/bin/bash", "/bin/sh"] {
        let fallback_owned = fallback.to_string();
        if Path::new(fallback).is_file() && seen.insert(fallback_owned.clone()) {
            candidates.push(fallback_owned);
        }
    }

    candidates
}

#[cfg(target_os = "macos")]
fn preflight_sandbox_shell(
    sandbox_exec_path: &Path,
    profile_path: &Path,
    shell_path: &str,
) -> Result<(), String> {
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;

    let output = std::process::Command::new(sandbox_exec_path)
        .arg("-f")
        .arg(profile_path)
        .arg(shell_path)
        .arg("-i")
        .arg("-c")
        .arg("exit 0")
        .output()
        .map_err(|e| {
            format!(
                "Failed to run sandbox shell preflight for '{}': {}",
                shell_path, e
            )
        })?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let details = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else if let Some(signal) = output.status.signal() {
        format!("terminated by signal {}", signal)
    } else {
        format!("exit code {}", output.status.code().unwrap_or(-1))
    };

    Err(format!(
        "Sandbox shell '{}' failed preflight: {}",
        shell_path, details
    ))
}

/// Build shell + args for a sandboxed terminal session.
/// On macOS, wraps the user's shell with `sandbox-exec -f <profile>`.
/// On other platforms, returns defaults (command guard only, no OS wrapping).
fn build_sandboxed_shell(
    working_dir: &str,
    project_path: &Option<String>,
) -> Result<(Option<String>, Option<Vec<String>>), String> {
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

        let profile = seatbelt::generate_seatbelt_profile(workspace_dir, &extra_read, &extra_write);

        // Write profile to a persistent location (not tempfile, since the terminal lives long)
        let profile_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("devflow")
            .join("sandbox-profiles");
        std::fs::create_dir_all(&profile_dir).map_err(|e| {
            format!(
                "Failed to create sandbox profile directory '{}': {}",
                profile_dir.display(),
                e
            )
        })?;
        let profile_path = profile_dir.join(format!(
            "terminal-{}-{}.sb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::write(&profile_path, &profile).map_err(|e| {
            format!(
                "Failed to write sandbox profile '{}': {}",
                profile_path.display(),
                e
            )
        })?;

        let sandbox_exec_path = PathBuf::from("/usr/bin/sandbox-exec");
        if !sandbox_exec_path.is_file() {
            return Err(
                "Sandboxed terminals require '/usr/bin/sandbox-exec', but it was not found"
                    .to_string(),
            );
        }

        let shell_candidates = sandbox_shell_candidates();
        if shell_candidates.is_empty() {
            return Err("No usable shell executable found for sandboxed terminal".to_string());
        }

        let mut selected_shell = None;
        let mut preflight_errors = Vec::new();
        for candidate in shell_candidates {
            match preflight_sandbox_shell(&sandbox_exec_path, &profile_path, &candidate) {
                Ok(()) => {
                    selected_shell = Some(candidate);
                    break;
                }
                Err(e) => preflight_errors.push(e),
            }
        }

        let shell_path = selected_shell.ok_or_else(|| {
            format!(
                "Sandbox shell preflight failed. {}",
                preflight_errors.join(" | ")
            )
        })?;

        Ok((
            Some(sandbox_exec_path.display().to_string()),
            Some(vec![
                "-f".to_string(),
                profile_path.display().to_string(),
                shell_path,
                "-i".to_string(),
            ]),
        ))
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (workspace_dir, extra_read, extra_write);
        // On non-macOS, no OS-level shell wrapping (command guard applies to devflow commands)
        Ok((None, None))
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
    let mut working_dir =
        if let (Some(ref pp), Some(ref workspace)) = (&project_path, &workspace_name) {
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

    if !Path::new(&working_dir).is_dir() {
        if let Some(ref pp) = project_path {
            if Path::new(pp).is_dir() {
                log::warn!(
                    "Terminal target directory '{}' does not exist, falling back to project root '{}'",
                    working_dir,
                    pp
                );
                working_dir = pp.clone();
            }
        }
    }

    if !Path::new(&working_dir).is_dir() {
        return Err(format!(
            "Terminal working directory '{}' does not exist",
            working_dir
        ));
    }

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
    let sandbox_state =
        if let (Some(ref pp), Some(ref workspace)) = (&project_path, &workspace_name) {
            is_workspace_sandboxed(pp, workspace)
        } else {
            false
        };

    #[cfg(target_os = "macos")]
    if sandbox_state {
        env.extend(prepare_sandbox_shell_home(&working_dir)?);
        env.insert("DISABLE_AUTO_UPDATE".to_string(), "true".to_string());
        env.insert("ZSH_DISABLE_COMPFIX".to_string(), "true".to_string());
    }

    let (shell, shell_args) = if sandbox_state {
        build_sandboxed_shell(&working_dir, &project_path)?
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

    let terminal_manager = state.terminals.clone();
    let (info, mut output_rx) = state
        .terminals
        .create_session(config, metadata)
        .await
        .map_err(|e| {
            if sandbox_state {
                format!(
                    "Failed to spawn sandboxed terminal in '{}': {}. Check sandbox profile access and shell path.",
                    working_dir, e
                )
            } else {
                format!("Failed to spawn terminal in '{}': {}", working_dir, e)
            }
        })?;

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
        // PTY closed — notify frontend so it can close the tab
        let _ = terminal_manager.close_session(&session_id).await;
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

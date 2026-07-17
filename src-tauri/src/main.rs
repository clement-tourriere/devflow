#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::{
    menu::{Menu, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder},
    tray::{MouseButton, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, WindowEvent,
};

fn workspace_menu_item_id(project_path: &str, workspace_name: &str) -> String {
    format!(
        "workspace-open:{}|{}",
        urlencoding::encode(project_path),
        urlencoding::encode(workspace_name)
    )
}

fn parse_workspace_menu_payload(id: &str) -> Option<(String, String)> {
    let payload = id.strip_prefix("workspace-open:")?;
    let (encoded_project, encoded_workspace) = payload.split_once('|')?;
    let project_path = urlencoding::decode(encoded_project).ok()?.into_owned();
    let workspace_name = urlencoding::decode(encoded_workspace).ok()?.into_owned();
    Some((project_path, workspace_name))
}

fn workspace_tray_label(workspace: &commands::workspaces::WorkspaceEntry) -> (String, bool) {
    let mut details = Vec::new();
    if let Some(path) = workspace.worktree_path.as_ref() {
        let folder = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string());
        details.push(format!("dir={}", folder));
    }
    if let Some(parent) = workspace.parent.as_ref().filter(|p| !p.trim().is_empty()) {
        details.push(format!("from {}", parent));
    }
    if workspace.is_context {
        details.push("context".to_string());
    }
    if workspace.health != "ready" {
        details.push(workspace.health.clone());
    }

    let suffix = if details.is_empty() {
        String::new()
    } else {
        format!(" ({})", details.join(", "))
    };
    (format!("workspace: {}{}", workspace.name, suffix), false)
}

fn navigate_to_project(app: &tauri::AppHandle, project_path: &str) {
    let encoded = urlencoding::encode(project_path);
    let route = format!("/projects/{}", encoded);
    let _ = app.emit("navigate", route);
    show_window(app);
}

mod commands;
mod state;

use state::AppState;

/// When launched from the macOS Dock/Finder (or a Linux desktop entry), the
/// process inherits launchd's minimal PATH — missing Homebrew, mise shims,
/// docker, npm, etc. Every hook, doctor check, and docker-exec spawned from
/// the GUI then fails with "command not found", while the same operation
/// works from a terminal. Capture the user's real login-shell PATH once at
/// startup and adopt it so spawned subprocesses behave like the CLI.
#[cfg(unix)]
fn bootstrap_login_path() {
    use std::sync::mpsc;
    use std::time::Duration;

    let current = std::env::var("PATH").unwrap_or_default();
    // If a Homebrew bin dir is already present we were almost certainly
    // launched from a shell — skip the (slowish) interactive-shell probe.
    if current.contains("/opt/homebrew/bin") || current.contains("/usr/local/bin") {
        return;
    }

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // -l -i so login + interactive rc files (where mise/direnv/PATH live) run.
        let out = std::process::Command::new(&shell)
            .args(["-l", "-i", "-c", "printf %s \"$PATH\""])
            .output();
        let _ = tx.send(out);
    });

    let login_path = match rx.recv_timeout(Duration::from_secs(4)) {
        Ok(Ok(out)) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        _ => {
            log::warn!("Could not capture login-shell PATH; GUI subprocesses may lack tools");
            return;
        }
    };
    if login_path.is_empty() {
        return;
    }

    // Merge: login PATH first, then anything already present, de-duplicated.
    let mut seen = std::collections::HashSet::new();
    let merged: Vec<&str> = login_path
        .split(':')
        .chain(current.split(':'))
        .filter(|p| !p.is_empty() && seen.insert(*p))
        .collect();
    let merged = merged.join(":");
    log::info!(
        "Adopted login-shell PATH ({} entries)",
        merged.matches(':').count() + 1
    );
    std::env::set_var("PATH", merged);
}

fn main() {
    env_logger::init();
    log::info!("Starting devflow application");

    #[cfg(unix)]
    bootstrap_login_path();

    let app_state = AppState::new();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            // Projects
            commands::projects::list_projects,
            commands::projects::remove_project,
            commands::projects::get_project_detail,
            commands::projects::add_or_init_project,
            commands::projects::destroy_project,
            commands::projects::detect_orphan_projects,
            commands::projects::cleanup_orphan_project,
            commands::projects::detect_vcs_info,
            commands::projects::detect_vcs_workspaces,
            // Workspaces
            commands::workspaces::list_workspaces,
            commands::workspaces::get_connection_info,
            commands::workspaces::create_workspace,
            commands::workspaces::switch_workspace,
            commands::workspaces::preflight_delete_workspace,
            commands::workspaces::delete_workspace,
            commands::workspaces::prune_worktrees,
            // Processes
            commands::processes::get_pitchfork_bridge_info,
            commands::processes::list_processes,
            commands::processes::start_processes,
            commands::processes::stop_processes,
            commands::processes::restart_processes,
            commands::processes::get_process_logs,
            commands::processes::forget_process_record,
            // Services
            commands::services::add_service,
            commands::services::list_services,
            commands::services::list_service_workspaces,
            commands::services::start_service,
            commands::services::stop_service,
            commands::services::run_doctor,
            commands::services::get_service_logs,
            commands::services::reset_service,
            commands::services::delete_service_workspace,
            commands::services::destroy_service,
            commands::services::discover_docker_containers,
            commands::services::install_agent_skills,
            // Hooks
            commands::hooks::list_hooks,
            commands::hooks::render_template,
            commands::hooks::get_hook_variables,
            commands::hooks::install_vcs_hooks,
            commands::hooks::uninstall_vcs_hooks,
            commands::hooks::get_action_types,
            commands::hooks::save_hooks,
            commands::hooks::run_hook,
            commands::hooks::get_trigger_mappings,
            commands::hooks::get_recipes,
            commands::hooks::detect_recipes,
            commands::hooks::preview_recipe,
            commands::hooks::install_recipe,
            commands::hooks::install_recipes,
            // Proxy
            commands::proxy::start_proxy,
            commands::proxy::stop_proxy,
            commands::proxy::get_proxy_status,
            commands::proxy::list_containers,
            commands::proxy::get_certificate_status,
            commands::proxy::install_certificate,
            commands::proxy::remove_certificate,
            // Config
            commands::config::get_config_json,
            commands::config::save_config_json,
            commands::config::get_config_yaml,
            commands::config::save_config_yaml,
            commands::config::validate_config_yaml,
            // Settings
            commands::settings::get_settings,
            commands::settings::save_settings,
            // Terminal
            commands::terminal::create_terminal,
            commands::terminal::list_terminals,
            commands::terminal::write_terminal,
            commands::terminal::resize_terminal,
            commands::terminal::close_terminal,
        ])
        .setup(move |app| {
            log::info!("Application setup complete");

            // Build tray (placeholders only — no Docker/inventory work here)
            let tray = build_tray(app)?;

            // Store tray handle for dynamic updates
            let app_state: &AppState = app.state::<AppState>().inner();
            *app_state.tray.lock().unwrap() = Some(tray);

            // Fill in per-project workspace entries asynchronously.
            update_tray_menu(&app.handle().clone());

            // Auto-start proxy if configured
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state: &AppState = handle.state::<AppState>().inner();
                let should_start = {
                    let settings = state.settings.read().await;
                    settings.proxy_auto_start
                };

                if should_start {
                    log::info!("Auto-starting proxy from saved settings");
                    let config = state.proxy_config.read().await.clone();
                    match devflow_proxy::run_proxy(config.clone()).await {
                        Ok(proxy_handle) => {
                            *state.proxy.write().await = Some(std::sync::Arc::new(proxy_handle));
                            let ca_installed =
                                devflow_proxy::platform::verify_system_trust().unwrap_or(false);
                            let status = commands::proxy::ProxyStatus {
                                running: true,
                                https_port: config.https_port,
                                http_port: config.http_port,
                                ca_installed,
                                ca_path: devflow_proxy::ca::default_ca_cert_path()
                                    .display()
                                    .to_string(),
                            };
                            let _ = handle.emit("proxy-status-changed", &status);
                            update_tray_menu(&handle);
                        }
                        Err(e) => {
                            log::error!("Failed to auto-start proxy: {}", e);
                        }
                    }
                }
            });

            // Show the main window
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                log::info!("Window close requested — hiding to tray");
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let tauri::RunEvent::Exit = event {
                log::info!("Application exiting — cleaning up terminals");
                let state: &AppState = app_handle.state::<AppState>().inner();
                tauri::async_runtime::block_on(state.terminals.close_all());
            }
        });
}

fn build_tray(app: &tauri::App) -> Result<tauri::tray::TrayIcon, Box<dyn std::error::Error>> {
    let show = MenuItemBuilder::with_id("show", "Open Dashboard").build(app)?;
    let separator1 = PredefinedMenuItem::separator(app)?;

    // Proxy section
    let proxy_status_item = MenuItemBuilder::with_id("proxy_status", "Proxy: Stopped")
        .enabled(false)
        .build(app)?;
    let proxy_toggle = MenuItemBuilder::with_id("proxy_toggle", "Start Proxy").build(app)?;
    let separator2 = PredefinedMenuItem::separator(app)?;

    // Projects submenu
    let projects_submenu = {
        let mut builder = SubmenuBuilder::with_id(app, "projects_menu", "Projects");
        let state: &AppState = app.state::<AppState>().inner();
        let settings = tauri::async_runtime::block_on(state.settings.read());
        if settings.projects.is_empty() {
            let empty = MenuItemBuilder::with_id("no_projects", "No projects")
                .enabled(false)
                .build(app)?;
            builder = builder.item(&empty);
        } else {
            for project in &settings.projects {
                let mut project_builder = SubmenuBuilder::with_id(
                    app,
                    format!("project_menu:{}", urlencoding::encode(&project.path)),
                    &project.name,
                );

                let open_item =
                    MenuItemBuilder::with_id(format!("project:{}", project.path), "Open project")
                        .build(app)?;
                project_builder = project_builder.item(&open_item);

                // The initial tray must not block startup on the full
                // workspace inventory (per-service Docker inspections and
                // process probing, up to bollard's 120s timeout per request
                // when the daemon is slow) — the window is only shown after
                // this function returns. Populate a placeholder and let the
                // async `update_tray_menu` pass fill in workspaces.
                let sep = PredefinedMenuItem::separator(app)?;
                project_builder = project_builder.item(&sep);
                let loading = MenuItemBuilder::with_id(
                    format!("workspace-loading:{}", urlencoding::encode(&project.path)),
                    "Loading workspaces…",
                )
                .enabled(false)
                .build(app)?;
                project_builder = project_builder.item(&loading);

                let project_submenu = project_builder.build()?;
                builder = builder.item(&project_submenu);
            }
        }
        builder.build()?
    };
    let separator3 = PredefinedMenuItem::separator(app)?;

    let quit = MenuItemBuilder::with_id("quit", "Quit devflow").build(app)?;

    let menu = Menu::with_items(
        app,
        &[
            &show,
            &separator1,
            &proxy_status_item,
            &proxy_toggle,
            &separator2,
            &projects_submenu,
            &separator3,
            &quit,
        ],
    )?;

    let icon_bytes = include_bytes!("../icons/tray-icon.png");
    let icon = tauri::image::Image::from_bytes(icon_bytes)
        .unwrap_or_else(|_| app.default_window_icon().unwrap().clone());

    let tray = TrayIconBuilder::new()
        .icon(icon)
        .icon_as_template(true)
        .tooltip("devflow")
        .menu(&menu)
        .on_menu_event(|app, event| {
            let id = event.id.as_ref();
            match id {
                "quit" => {
                    log::info!("Quit requested from tray");
                    app.exit(0);
                }
                "show" => {
                    show_window(app);
                }
                "proxy_toggle" => {
                    let handle = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let state: &AppState = handle.state::<AppState>().inner();
                        let is_running = state.proxy.read().await.is_some();
                        if is_running {
                            let mut proxy = state.proxy.write().await;
                            if let Some(h) = proxy.take() {
                                h.stop();
                            }
                            let mut settings = state.settings.write().await;
                            settings.proxy_auto_start = false;
                            let _ = settings.save();
                            let config = state.proxy_config.read().await;
                            let status = commands::proxy::ProxyStatus {
                                running: false,
                                https_port: config.https_port,
                                http_port: config.http_port,
                                ca_installed: devflow_proxy::platform::verify_system_trust()
                                    .unwrap_or(false),
                                ca_path: devflow_proxy::ca::default_ca_cert_path()
                                    .display()
                                    .to_string(),
                            };
                            let _ = handle.emit("proxy-status-changed", &status);
                        } else {
                            let config = state.proxy_config.read().await.clone();
                            if let Ok(proxy_handle) = devflow_proxy::run_proxy(config.clone()).await
                            {
                                *state.proxy.write().await =
                                    Some(std::sync::Arc::new(proxy_handle));
                                let mut settings = state.settings.write().await;
                                settings.proxy_auto_start = true;
                                settings.proxy_config = Some(config.clone());
                                let _ = settings.save();
                                let ca_installed =
                                    devflow_proxy::platform::verify_system_trust().unwrap_or(false);
                                let status = commands::proxy::ProxyStatus {
                                    running: true,
                                    https_port: config.https_port,
                                    http_port: config.http_port,
                                    ca_installed,
                                    ca_path: devflow_proxy::ca::default_ca_cert_path()
                                        .display()
                                        .to_string(),
                                };
                                let _ = handle.emit("proxy-status-changed", &status);
                            }
                        }
                        update_tray_menu(&handle);
                    });
                }
                _ if id.starts_with("project:") => {
                    let project_path = &id["project:".len()..];
                    navigate_to_project(app, project_path);
                }
                _ if id.starts_with("workspace-open:") => {
                    if let Some((project_path, _workspace_name)) = parse_workspace_menu_payload(id)
                    {
                        // Every workspace is already materialized. Selecting a
                        // tray item opens its project view; it must not mutate
                        // another checkout just to establish UI context.
                        navigate_to_project(app, &project_path);
                    }
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } = event
            {
                show_window(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(tray)
}

fn show_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Rebuild the tray menu to reflect current proxy and project state.
pub fn update_tray_menu(app: &tauri::AppHandle) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let state: &AppState = handle.state::<AppState>().inner();
        let is_running = state.proxy.read().await.is_some();
        let config = state.proxy_config.read().await.clone();
        let settings = state.settings.read().await.clone();

        // Build menu items on main thread via app handle
        let show = MenuItemBuilder::with_id("show", "Open Dashboard")
            .build(&handle)
            .unwrap();
        let sep1 = PredefinedMenuItem::separator(&handle).unwrap();

        let proxy_label = if is_running {
            format!("Proxy: Running ({})", config.https_port)
        } else {
            "Proxy: Stopped".to_string()
        };
        let proxy_status_item = MenuItemBuilder::with_id("proxy_status", &proxy_label)
            .enabled(false)
            .build(&handle)
            .unwrap();
        let toggle_label = if is_running {
            "Stop Proxy"
        } else {
            "Start Proxy"
        };
        let proxy_toggle = MenuItemBuilder::with_id("proxy_toggle", toggle_label)
            .build(&handle)
            .unwrap();
        let sep2 = PredefinedMenuItem::separator(&handle).unwrap();

        let mut projects_builder = SubmenuBuilder::with_id(&handle, "projects_menu", "Projects");
        if settings.projects.is_empty() {
            let empty = MenuItemBuilder::with_id("no_projects", "No projects")
                .enabled(false)
                .build(&handle)
                .unwrap();
            projects_builder = projects_builder.item(&empty);
        } else {
            for project in &settings.projects {
                let mut project_builder = SubmenuBuilder::with_id(
                    &handle,
                    format!("project_menu:{}", urlencoding::encode(&project.path)),
                    &project.name,
                );

                let open_item =
                    MenuItemBuilder::with_id(format!("project:{}", project.path), "Open project")
                        .build(&handle)
                        .unwrap();
                project_builder = project_builder.item(&open_item);

                match commands::workspaces::list_workspaces(project.path.clone()).await {
                    Ok(ws) => {
                        let sep = PredefinedMenuItem::separator(&handle).unwrap();
                        project_builder = project_builder.item(&sep);

                        if ws.workspaces.is_empty() {
                            let empty = MenuItemBuilder::with_id(
                                format!("workspace-empty:{}", urlencoding::encode(&project.path)),
                                "No workspaces",
                            )
                            .enabled(false)
                            .build(&handle)
                            .unwrap();
                            project_builder = project_builder.item(&empty);
                        } else {
                            for workspace in &ws.workspaces {
                                let (label, disabled) = workspace_tray_label(workspace);
                                let item = MenuItemBuilder::with_id(
                                    workspace_menu_item_id(&project.path, &workspace.name),
                                    &label,
                                )
                                .enabled(!disabled)
                                .build(&handle)
                                .unwrap();
                                project_builder = project_builder.item(&item);
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to load workspaces for tray project '{}': {}",
                            project.path,
                            e
                        );
                        let sep = PredefinedMenuItem::separator(&handle).unwrap();
                        project_builder = project_builder.item(&sep);
                        let unavailable = MenuItemBuilder::with_id(
                            format!(
                                "workspace-unavailable:{}",
                                urlencoding::encode(&project.path)
                            ),
                            "Workspaces unavailable",
                        )
                        .enabled(false)
                        .build(&handle)
                        .unwrap();
                        project_builder = project_builder.item(&unavailable);
                    }
                }

                let project_submenu = project_builder.build().unwrap();
                projects_builder = projects_builder.item(&project_submenu);
            }
        }
        let projects_submenu = projects_builder.build().unwrap();
        let sep3 = PredefinedMenuItem::separator(&handle).unwrap();
        let quit = MenuItemBuilder::with_id("quit", "Quit devflow")
            .build(&handle)
            .unwrap();

        let menu = Menu::with_items(
            &handle,
            &[
                &show,
                &sep1,
                &proxy_status_item,
                &proxy_toggle,
                &sep2,
                &projects_submenu,
                &sep3,
                &quit,
            ],
        )
        .unwrap();

        // Update tray tooltip
        let tooltip = if is_running {
            format!("devflow — Proxy: Running ({})", config.https_port)
        } else {
            "devflow — Proxy: Stopped".to_string()
        };

        // Clone the handle out of the mutex before touching the tray:
        // set_menu/set_tooltip dispatch to the main thread, and holding the
        // guard across that dispatch would block every other tray update
        // (each on its own tokio worker) if the main thread is busy.
        let tray = state.tray.lock().unwrap().clone();
        if let Some(tray) = tray {
            let _ = tray.set_menu(Some(menu));
            let _ = tray.set_tooltip(Some(&tooltip));
        }
    });
}

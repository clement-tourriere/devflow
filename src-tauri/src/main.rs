#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::{
    menu::{Menu, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder},
    tray::{MouseButton, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, WindowEvent,
};

mod commands;
mod state;

use state::AppState;

fn main() {
    env_logger::init();
    log::info!("Starting devflow application");

    let app_state = AppState::new();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            // Projects
            commands::projects::list_projects,
            commands::projects::add_project,
            commands::projects::remove_project,
            commands::projects::get_project_detail,
            commands::projects::init_project,
            commands::projects::destroy_project,
            commands::projects::detect_orphan_projects,
            commands::projects::cleanup_orphan_project,
            // Branches
            commands::branches::list_branches,
            commands::branches::get_connection_info,
            commands::branches::create_branch,
            commands::branches::switch_branch,
            commands::branches::delete_branch,
            // Services
            commands::services::add_service,
            commands::services::list_services,
            commands::services::start_service,
            commands::services::stop_service,
            commands::services::run_doctor,
            commands::services::get_service_logs,
            commands::services::reset_service,
            commands::services::get_service_status,
            // Hooks
            commands::hooks::list_hooks,
            commands::hooks::render_template,
            commands::hooks::get_hook_variables,
            // Proxy
            commands::proxy::start_proxy,
            commands::proxy::stop_proxy,
            commands::proxy::get_proxy_status,
            commands::proxy::list_containers,
            commands::proxy::get_certificate_status,
            commands::proxy::install_certificate,
            commands::proxy::remove_certificate,
            // Config
            commands::config::get_config_yaml,
            commands::config::save_config_yaml,
            commands::config::validate_config_yaml,
            // Settings
            commands::settings::get_settings,
            commands::settings::save_settings,
        ])
        .setup(move |app| {
            log::info!("Application setup complete");

            // Build tray
            let tray = build_tray(app)?;

            // Store tray handle for dynamic updates
            let app_state: &AppState = app.state::<AppState>().inner();
            *app_state.tray.lock().unwrap() = Some(tray);

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
                            *state.proxy.write().await =
                                Some(std::sync::Arc::new(proxy_handle));
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
        .run(|_app_handle, event| {
            if let tauri::RunEvent::Exit = event {
                log::info!("Application exiting");
            }
        });
}

fn build_tray(app: &tauri::App) -> Result<tauri::tray::TrayIcon, Box<dyn std::error::Error>> {
    let show = MenuItemBuilder::with_id("show", "Open Dashboard").build(app)?;
    let separator1 = PredefinedMenuItem::separator(app)?;

    // Proxy section
    let proxy_status_item =
        MenuItemBuilder::with_id("proxy_status", "Proxy: Stopped").enabled(false).build(app)?;
    let proxy_toggle = MenuItemBuilder::with_id("proxy_toggle", "Start Proxy").build(app)?;
    let separator2 = PredefinedMenuItem::separator(app)?;

    // Projects submenu
    let projects_submenu = {
        let mut builder = SubmenuBuilder::with_id(app, "projects_menu", "Projects");
        let state: &AppState = app.state::<AppState>().inner();
        let settings = tauri::async_runtime::block_on(state.settings.read());
        if settings.projects.is_empty() {
            let empty =
                MenuItemBuilder::with_id("no_projects", "No projects").enabled(false).build(app)?;
            builder = builder.item(&empty);
        } else {
            for project in &settings.projects {
                let item = MenuItemBuilder::with_id(
                    &format!("project:{}", project.path),
                    &project.name,
                )
                .build(app)?;
                builder = builder.item(&item);
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
                    let encoded = urlencoding::encode(project_path);
                    let route = format!("/projects/{}", encoded);
                    let _ = app.emit("navigate", route);
                    show_window(app);
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
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

        let mut projects_builder =
            SubmenuBuilder::with_id(&handle, "projects_menu", "Projects");
        if settings.projects.is_empty() {
            let empty = MenuItemBuilder::with_id("no_projects", "No projects")
                .enabled(false)
                .build(&handle)
                .unwrap();
            projects_builder = projects_builder.item(&empty);
        } else {
            for project in &settings.projects {
                let item = MenuItemBuilder::with_id(
                    &format!("project:{}", project.path),
                    &project.name,
                )
                .build(&handle)
                .unwrap();
                projects_builder = projects_builder.item(&item);
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

        if let Some(tray) = state.tray.lock().unwrap().as_ref() {
            let _ = tray.set_menu(Some(menu));
            let _ = tray.set_tooltip(Some(&tooltip));
        }
    });
}

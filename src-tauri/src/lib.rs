//! MyWallpaper Desktop Application
//!
//! Tauri backend for the MyWallpaper animated wallpaper application.

mod commands;
mod commands_core;
mod tray;
mod window_layer;

use tracing::{info, warn};

fn mw_init_script() -> String {
    format!(
        r#"window.__MW_INIT__ = {{
            isTauri: true,
            platform: "{}",
            arch: "{}",
            appVersion: "{}",
            tauriVersion: "{}",
            debug: {}
        }};"#,
        std::env::consts::OS,
        std::env::consts::ARCH,
        env!("CARGO_PKG_VERSION"),
        tauri::VERSION,
        cfg!(debug_assertions),
    )
}

pub use commands::*;

pub fn main() {
    info!("Starting MyWallpaper Desktop v{}", env!("CARGO_PKG_VERSION"));
    start_with_tauri_webview();
}

fn start_with_tauri_webview() {
    use tauri::webview::PageLoadEvent;
    use tauri::{Emitter, Listener, Manager};

    let app = tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(if cfg!(debug_assertions) { log::LevelFilter::Debug } else { log::LevelFilter::Info })
                .target(tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Webview))
                .target(tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir { file_name: None }))
                .target(tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout))
                .build(),
        )
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, Some(vec!["--minimized"])))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            for arg in args.iter() {
                if arg.starts_with("mywallpaper://") {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.emit("deep-link", arg.clone());
                    }
                }
            }
        }))
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Started {
                let _ = webview.eval(&mw_init_script());
            }
        })
        .setup(|app| {
            info!("Application setup starting...");
            let handle = app.handle().clone();

            if let Err(e) = tray::setup_tray(&handle) {
                tracing::error!("Failed to setup system tray: {}", e);
            }

            let deep_link_handle = handle.clone();
            app.listen("deep-link://new-url", move |event| {
                if let Ok(urls) = serde_json::from_str::<Vec<String>>(event.payload()) {
                    for url in urls.into_iter().filter(|u| u.starts_with("mywallpaper://")) {
                        if let Some(window) = deep_link_handle.get_webview_window("main") {
                            let _ = window.emit("deep-link", url);
                            let _ = window.set_focus();
                        }
                    }
                }
            });

            if let Some(window) = app.get_webview_window("main") {
                use tauri::webview::Color;
                let _ = window.set_background_color(Some(Color(0, 0, 0, 0)));
                let _ = window.set_decorations(false);

                if let Some(monitor) = window.primary_monitor().ok().flatten() {
                    let size = monitor.size();
                    let position = monitor.position();
                    let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(position.x, position.y)));
                    let _ = window.set_size(tauri::Size::Physical(tauri::PhysicalSize::new(size.width, size.height)));
                }

                let _ = window.show();

                // Lancement de l'architecture universelle (Derrière les icônes)
                window_layer::setup_desktop_window(&window);
                
                // Démarrage du Watchdog de performance
                window_layer::visibility_watchdog::start(handle.clone());
            }

            info!("Application setup complete");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_system_info,
            commands::check_for_updates,
            commands::download_and_install_update,
            commands::restart_app,
            commands::open_oauth_in_browser,
            commands::reload_window,
            window_layer::set_desktop_icons_visible, 
        ])
        .build(tauri::generate_context!())
        .expect("Error while building MyWallpaper Desktop");

    // Lancement de l'application avec sécurité de restauration des icônes
    app.run(|_app_handle, event| {
        if let tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit = event {
            window_layer::restore_desktop_icons();
        }
    });
}

//! MyWallpaper Desktop Application
//!
//! Tauri backend for the MyWallpaper animated wallpaper application.

mod commands;
mod commands_core;
mod tray;
mod window_layer;

use log::{debug, error, info};

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
    // Clear previous log files so each run starts fresh
    #[cfg(target_os = "windows")]
    if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") {
        let log_dir = std::path::Path::new(&local_appdata).join("com.mywallpaper.desktop").join("logs");
        if let Ok(entries) = std::fs::read_dir(&log_dir) {
            for entry in entries.flatten() {
                if entry.path().extension().is_some_and(|e| e == "log") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    info!("[main] Starting MyWallpaper Desktop v{}", env!("CARGO_PKG_VERSION"));
    info!("[main] OS: {}, Architecture: {}", std::env::consts::OS, std::env::consts::ARCH);

    start_with_tauri_webview();
}

fn start_with_tauri_webview() {
    use tauri::webview::PageLoadEvent;
    use tauri::{Emitter, Listener, Manager};

    debug!("[start_with_tauri_webview] Building Tauri application instance...");

    let app = tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(if cfg!(debug_assertions) { log::LevelFilter::Debug } else { log::LevelFilter::Info })
                .clear_targets()
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
            debug!("[plugin_single_instance] Single instance callback triggered with args: {:?}", args);
            for arg in args.iter() {
                if arg.starts_with("mywallpaper://") {
                    if let Some(window) = app.get_webview_window("main") {
                        info!("[plugin_single_instance] Emitting deep-link event to frontend.");
                        let _ = window.emit("deep-link", arg.clone());
                    }
                }
            }
        }))
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Started {
                debug!("[on_page_load] Page load started. Injecting mw_init_script...");
                let _ = webview.eval(&mw_init_script());
            }
        })
        .setup(|app| {
            info!("[setup] Tauri Application setup phase starting...");
            let handle = app.handle().clone();

            debug!("[setup] Initializing System Tray...");
            if let Err(e) = tray::setup_tray(&handle) {
                error!("[setup] Failed to setup system tray: {}", e);
            }

            debug!("[setup] Registering Deep Link listeners...");
            let deep_link_handle = handle.clone();
            app.listen("deep-link://new-url", move |event| {
                debug!("[deep-link] Received raw deep link payload: {}", event.payload());
                if let Ok(urls) = serde_json::from_str::<Vec<String>>(event.payload()) {
                    for url in urls.into_iter().filter(|u| u.starts_with("mywallpaper://")) {
                        info!("[deep-link] Routing URL to main window: {}", url);
                        if let Some(window) = deep_link_handle.get_webview_window("main") {
                            let _ = window.emit("deep-link", url);
                        }
                    }
                } else {
                    error!("[deep-link] Failed to parse deep-link payload as JSON array.");
                }
            });

            debug!("[setup] Configuring main window parameters...");
            if let Some(window) = app.get_webview_window("main") {
                use tauri::webview::Color;
                let _ = window.set_background_color(Some(Color(0, 0, 0, 255)));

                // Inject into desktop BEFORE showing to prevent visible flash
                debug!("[setup] Firing Desktop Subsystem Injection...");
                window_layer::setup_desktop_window(&window);

                let _ = window.show();
                debug!("[setup] Main window shown (post-injection).");
            } else {
                error!("[setup] CRITICAL: Main webview window not found during setup phase.");
            }

            info!("[setup] Application setup phase complete.");
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

    debug!("[start_with_tauri_webview] Entering main event loop...");
    app.run(|_app_handle, event| {
        if let tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit = event {
            info!("[app.run] Exit requested. Ensuring desktop icons are restored.");
            window_layer::restore_desktop_icons_and_unhook();
        }
    });
}

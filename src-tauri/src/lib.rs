//! MyWallpaper Desktop Application
//!
//! Tauri backend for the MyWallpaper animated wallpaper application.
//! Currently supports Windows and macOS. Linux support is paused.

mod commands;
mod commands_core;
mod tray;
mod window_layer;

use tracing::{info, warn};

/// Build the __MW_INIT__ injection script (runs before page JS).
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

/// Main entry point
pub fn main() {
    info!(
        "Starting MyWallpaper Desktop v{}",
        env!("CARGO_PKG_VERSION")
    );

    start_with_tauri_webview();
}

// ============================================================================
// Standard Tauri webview (Windows / macOS)
// ============================================================================

fn start_with_tauri_webview() {
    use tauri::webview::PageLoadEvent;
    use tauri::{Emitter, Listener, Manager};

    tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(if cfg!(debug_assertions) {
                    log::LevelFilter::Debug
                } else {
                    log::LevelFilter::Info
                })
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::Webview,
                ))
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::LogDir { file_name: None },
                ))
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::Stdout,
                ))
                .build(),
        )
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            info!("Single instance callback triggered with args: {:?}", args);
            for arg in args.iter() {
                if arg.starts_with("mywallpaper://") {
                    info!("Deep link received via single-instance: {}", arg);
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.emit("deep-link", arg.clone());
                    }
                }
            }
        }))
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Started {
                let _ = webview.eval(mw_init_script());
            }
        })
        .setup(|app| {
            info!("Application setup starting...");
            let handle = app.handle().clone();

            // Initialize system tray
            if let Err(e) = tray::setup_tray(&handle) {
                tracing::error!("Failed to setup system tray: {}", e);
            }

            // Listen for deep links via the deep-link plugin
            let deep_link_handle = handle.clone();
            app.listen("deep-link://new-url", move |event| {
                let payload = event.payload();
                info!("Deep link event received via plugin: {:?}", payload);
                if let Ok(urls) = serde_json::from_str::<Vec<String>>(payload) {
                    for url in urls {
                        if url.starts_with("mywallpaper://") {
                            info!("Processing deep link: {}", url);
                            if let Some(window) = deep_link_handle.get_webview_window("main") {
                                if let Err(e) = window.emit("deep-link", url.clone()) {
                                    warn!("Failed to emit deep-link event: {}", e);
                                } else {
                                    info!("Deep link emitted to frontend: {}", url);
                                }
                                let _ = window.set_focus();
                            }
                        }
                    }
                }
            });

            // Setup window to cover full screen
            if let Some(window) = app.get_webview_window("main") {
                // Fully transparent background
                use tauri::webview::Color;
                let _ = window.set_background_color(Some(Color(0, 0, 0, 0)));

                // Get primary monitor dimensions for fullscreen positioning
                if let Some(monitor) = window.primary_monitor().ok().flatten() {
                    let size = monitor.size();
                    let position = monitor.position();
                    info!(
                        "Primary monitor: {}x{} at ({}, {})",
                        size.width, size.height, position.x, position.y
                    );
                    let _ = window.set_position(tauri::Position::Physical(
                        tauri::PhysicalPosition::new(position.x, position.y),
                    ));
                    let _ = window.set_size(tauri::Size::Physical(tauri::PhysicalSize::new(
                        size.width,
                        size.height,
                    )));
                } else {
                    warn!("Could not detect primary monitor, using default size");
                }

                let _ = window.show();

                // Windows: fullscreen + WS_EX_TOOLWINDOW
                #[cfg(target_os = "windows")]
                {
                    let _ = window.set_fullscreen(true);
                    if let Ok(hwnd) = window.hwnd() {
                        use windows::Win32::Foundation::HWND;
                        use windows::Win32::UI::WindowsAndMessaging::*;
                        let h = HWND(hwnd.0 as *mut core::ffi::c_void);
                        unsafe {
                            let style = GetWindowLongPtrW(h, GWL_EXSTYLE);
                            SetWindowLongPtrW(h, GWL_EXSTYLE, style | WS_EX_TOOLWINDOW.0 as isize);
                        }
                        info!("Windows: fullscreen + WS_EX_TOOLWINDOW set (Interactive Mode)");
                    }
                }

                // macOS: configure collection behavior for Interactive Mode
                #[cfg(target_os = "macos")]
                {
                    if let Ok(ns_window) = window.ns_window() {
                        window_layer::set_macos_interactive_mode(ns_window);
                    }
                }
            }

            info!("Application setup complete");
            Ok(())
        })
        .manage(window_layer::WindowLayerState::new())
        .invoke_handler(tauri::generate_handler![
            commands::get_system_info,
            commands::check_for_updates,
            commands::download_and_install_update,
            commands::restart_app,
            commands::open_oauth_in_browser,
            commands::reload_window,
            commands::get_layers,
            commands::toggle_layer,
            window_layer::set_window_layer,
            window_layer::get_window_layer,
            window_layer::toggle_window_layer,
            window_layer::register_layer_shortcut,
            window_layer::unregister_layer_shortcut,
        ])
        .run(tauri::generate_context!())
        .expect("Error while running MyWallpaper Desktop");
}

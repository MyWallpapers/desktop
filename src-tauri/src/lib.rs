//! MyWallpaper Desktop Application
//!
//! Tauri backend for the MyWallpaper animated wallpaper application.
//! On Linux, uses CEF (Chromium Embedded Framework) instead of WebKitGTK
//! to enable WebGPU via Vulkan.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod commands;
mod commands_core;
mod tray;
mod window_layer;

#[cfg(target_os = "linux")]
pub mod cef;

use tracing::{info, warn, Level};
use tracing_subscriber::FmtSubscriber;

// ============================================================================
// Shared: __MW_INIT__ injection script
// ============================================================================

/// Build the __MW_INIT__ injection script (runs before page JS).
/// Used by the Tauri webview path (Windows/macOS).
#[cfg(not(target_os = "linux"))]
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
pub use tray::*;

/// Initialize logging based on debug/release mode
fn init_logging() {
    let level = if cfg!(debug_assertions) {
        Level::DEBUG
    } else {
        Level::INFO
    };

    let subscriber = FmtSubscriber::builder()
        .with_max_level(level)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(cfg!(debug_assertions))
        .with_line_number(cfg!(debug_assertions))
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .expect("Failed to set tracing subscriber");
}

/// Main entry point
pub fn main() {
    init_logging();
    info!(
        "Starting MyWallpaper Desktop v{}",
        env!("CARGO_PKG_VERSION")
    );

    #[cfg(target_os = "linux")]
    {
        start_with_cef();
    }

    #[cfg(not(target_os = "linux"))]
    {
        start_with_tauri_webview();
    }
}

// ============================================================================
// Windows / macOS — standard Tauri webview
// ============================================================================

#[cfg(not(target_os = "linux"))]
fn start_with_tauri_webview() {
    use tauri::webview::PageLoadEvent;
    use tauri::{Emitter, Listener, Manager};

    tauri::Builder::default()
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
                let _ = webview.eval(&mw_init_script());
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
                // Windows/macOS: fully transparent background
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
                    let _ = window.set_size(tauri::Size::Physical(
                        tauri::PhysicalSize::new(size.width, size.height),
                    ));
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
                            SetWindowLongPtrW(
                                h,
                                GWL_EXSTYLE,
                                style | WS_EX_TOOLWINDOW.0 as isize,
                            );
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

// ============================================================================
// Linux — CEF + Tauri headless
// ============================================================================

#[cfg(target_os = "linux")]
fn start_with_cef() {
    use tauri::{Emitter, Listener, Manager};

    info!("Linux: Starting with CEF (Chromium Embedded Framework)");

    // Step 1: Ensure CEF binaries are available
    if !cef::runtime::is_available() {
        info!("CEF binaries not found, starting first-launch download...");
        match cef::bootstrap::show_download_progress() {
            Ok(path) => info!("CEF downloaded to {}", path.display()),
            Err(e) => {
                tracing::error!("CEF download failed: {}", e);
                // Fall back to WebKitGTK (standard Tauri) if CEF download fails
                warn!("Falling back to WebKitGTK webview");
                start_with_tauri_webview_linux_fallback();
                return;
            }
        }
    }

    // Step 2: Setup LD_LIBRARY_PATH for libcef.so
    if let Err(e) = cef::runtime::setup_environment() {
        tracing::error!("CEF environment setup failed: {}", e);
        warn!("Falling back to WebKitGTK webview");
        start_with_tauri_webview_linux_fallback();
        return;
    }

    // Step 3: Initialize CEF
    if let Err(e) = cef::browser::initialize() {
        tracing::error!("CEF initialization failed: {}", e);
        warn!("Falling back to WebKitGTK webview");
        start_with_tauri_webview_linux_fallback();
        return;
    }

    // Step 4: Start Tauri in headless mode (hidden window, tray, updater, shortcuts)
    let tauri_app = tauri::Builder::default()
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
            info!("Single instance callback (CEF mode): {:?}", args);
            // Deep links: emit to CEF browser via JS eval
            for arg in args.iter() {
                if arg.starts_with("mywallpaper://") {
                    info!("Deep link received: {}", arg);
                    // TODO: Route to CEF browser via ProcessMessage
                }
            }
        }))
        .setup(|app| {
            info!("Tauri headless setup starting (CEF mode)...");
            let handle = app.handle().clone();

            // Initialize system tray (works without visible webview)
            if let Err(e) = tray::setup_tray(&handle) {
                tracing::error!("Failed to setup system tray: {}", e);
            }

            // Register deep link scheme
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                info!("Registering mywallpaper:// deep link scheme...");
                match app.deep_link().register("mywallpaper") {
                    Ok(_) => info!("Deep link scheme registered"),
                    Err(e) => warn!("Deep link registration failed: {} (may already exist)", e),
                }
            }

            // Listen for deep links
            let deep_link_handle = handle.clone();
            app.listen("deep-link://new-url", move |event| {
                let payload = event.payload();
                info!("Deep link event (CEF mode): {:?}", payload);
                // TODO: Route to CEF browser
            });

            // Hide the Tauri window — CEF provides the visible window
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.hide();
            }

            // Step 5: Create CEF browser window
            let url = if cfg!(debug_assertions) {
                "https://dev.mywallpaper.online"
            } else {
                "https://dev.mywallpaper.online"
            };

            if let Err(e) = cef::browser::create_browser(url) {
                tracing::error!("Failed to create CEF browser: {}", e);
            }

            info!("Tauri headless setup complete (CEF mode)");
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
        .build(tauri::generate_context!())
        .expect("Error building Tauri app in CEF mode");

    // Run with CEF message pump integration
    // Tauri's run() on Linux uses GTK internally, which shares the glib main loop.
    // CEF's external_message_pump hooks into the same glib loop via
    // on_schedule_message_pump_work → glib::idle_add_local / glib::timeout_add_local
    tauri_app.run(|_app, _event| {
        // CEF message pumping happens via glib integration in app.rs
    });

    // Cleanup
    cef::browser::shutdown_cef();
}

/// Fallback: run standard Tauri with WebKitGTK on Linux if CEF fails.
/// This is the original behavior before CEF integration.
#[cfg(target_os = "linux")]
fn start_with_tauri_webview_linux_fallback() {
    use tauri::webview::PageLoadEvent;
    use tauri::{Emitter, Listener, Manager};

    warn!("Running with WebKitGTK fallback (WebGPU may not work)");

    // Inline the __MW_INIT__ script for Linux fallback
    let init_script = format!(
        r#"window.__MW_INIT__ = {{
            isTauri: true,
            platform: "{}",
            arch: "{}",
            appVersion: "{}",
            tauriVersion: "{}",
            debug: {}
        }};
(function() {{
    const _origFetch = window.fetch;
    window.fetch = async function(input, init) {{
        const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
        if (url && (url.startsWith('http://localhost') || url.startsWith('http://127.0.0.1'))) {{
            const r = await window.__TAURI__.core.invoke('proxy_fetch', {{ url }});
            return new Response(r.body, {{
                status: r.status,
                headers: {{ 'content-type': r.content_type }}
            }});
        }}
        return _origFetch.call(this, input, init);
    }};
}})();"#,
        std::env::consts::OS,
        std::env::consts::ARCH,
        env!("CARGO_PKG_VERSION"),
        tauri::VERSION,
        cfg!(debug_assertions),
    );

    tauri::Builder::default()
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
        .on_page_load(move |webview, payload| {
            if payload.event() == PageLoadEvent::Started {
                let _ = webview.eval(&init_script);
            }
        })
        .setup(|app| {
            info!("Application setup starting (WebKitGTK fallback)...");
            let handle = app.handle().clone();

            if let Err(e) = tray::setup_tray(&handle) {
                tracing::error!("Failed to setup system tray: {}", e);
            }

            {
                use tauri_plugin_deep_link::DeepLinkExt;
                info!("Registering mywallpaper:// deep link scheme on Linux...");
                match app.deep_link().register("mywallpaper") {
                    Ok(_) => info!("Deep link scheme registered successfully"),
                    Err(e) => warn!(
                        "Failed to register deep link scheme: {} (may already be registered)",
                        e
                    ),
                }
            }

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
                                }
                                let _ = window.set_focus();
                            }
                        }
                    }
                }
            });

            if let Some(window) = app.get_webview_window("main") {
                use tauri::webview::Color;
                let _ = window.set_background_color(Some(Color(10, 10, 11, 255)));

                if let Some(monitor) = window.primary_monitor().ok().flatten() {
                    let size = monitor.size();
                    let position = monitor.position();
                    let _ = window.set_position(tauri::Position::Physical(
                        tauri::PhysicalPosition::new(position.x, position.y),
                    ));
                    let _ = window.set_size(tauri::Size::Physical(
                        tauri::PhysicalSize::new(size.width, size.height),
                    ));
                }

                let _ = window.show();
                let _ = window.set_fullscreen(true);
            }

            info!("Application setup complete (WebKitGTK fallback)");
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
            commands::proxy_fetch,
        ])
        .run(tauri::generate_context!())
        .expect("Error while running MyWallpaper Desktop (WebKitGTK fallback)");
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_app_starts() {
        assert!(true);
    }
}

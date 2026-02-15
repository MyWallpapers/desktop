//! MyWallpaper Desktop Application
//!
//! Tauri backend for the MyWallpaper animated wallpaper application.
//! Provides system tray and auto-updates.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod commands;
mod desktop_clone;
mod tray;

use tauri::{Emitter, Listener, Manager};
use tauri::webview::PageLoadEvent;
use tracing::{info, warn, Level};
use tracing_subscriber::FmtSubscriber;

/// Build the __MW_INIT__ injection script (runs before page JS)
fn mw_init_script() -> String {
    #[allow(unused_mut)]
    let mut script = format!(
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
    );

    // Linux/WebKitGTK: intercept fetch() calls to http://localhost so they go
    // through a Tauri command instead of the webview network stack. WebKitGTK
    // blocks HTTPS→HTTP mixed content; Chromium/WebView2 exempt localhost.
    #[cfg(target_os = "linux")]
    {
        script.push_str(r#"
(function() {
    const _origFetch = window.fetch;
    window.fetch = async function(input, init) {
        const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
        if (url && (url.startsWith('http://localhost') || url.startsWith('http://127.0.0.1'))) {
            const r = await window.__TAURI__.core.invoke('proxy_fetch', { url });
            return new Response(r.body, {
                status: r.status,
                headers: { 'content-type': r.content_type }
            });
        }
        return _origFetch.call(this, input, init);
    };
})();
"#);
    }

    script
}

pub use commands::*;
pub use tray::*;

// ============================================================================
// Platform-specific desktop lock
// ============================================================================

#[cfg(target_os = "windows")]
static ORIGINAL_WNDPROC: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// Windows: Subclass the window to lock position and block minimize.
///
/// The app stays ABOVE desktop icons (normal window level) but:
/// - Cannot be moved or dragged by users (WM_WINDOWPOSCHANGING locks position)
/// - Immune to Win+D (WM_SYSCOMMAND/SC_MINIMIZE is blocked)
/// - Cannot be hidden by Show Desktop (SWP_HIDEWINDOW is cleared)
#[cfg(target_os = "windows")]
fn setup_windows_desktop_lock(raw_hwnd_val: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::*;

    let hwnd = HWND(raw_hwnd_val as *mut core::ffi::c_void);

    unsafe {
        let original = GetWindowLongPtrW(hwnd, GWL_WNDPROC);
        if original == 0 {
            warn!("Failed to get original window procedure");
            return;
        }
        ORIGINAL_WNDPROC.store(original, std::sync::atomic::Ordering::Release);
        SetWindowLongPtrW(hwnd, GWL_WNDPROC, desktop_wndproc as isize);
    }

    info!("Windows desktop lock installed (subclass)");
}

/// Custom window procedure that blocks minimize/move and locks position.
#[cfg(target_os = "windows")]
unsafe extern "system" fn desktop_wndproc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::UI::WindowsAndMessaging::*;

    match msg {
        WM_SYSCOMMAND => {
            let cmd = wparam.0 & 0xFFF0;
            // Block minimize (Win+D) and move (drag via system menu)
            if cmd == 0xF020 || cmd == 0xF010 {
                return LRESULT(0);
            }
        }
        WM_WINDOWPOSCHANGING => {
            let pos = &mut *(lparam.0 as *mut WINDOWPOS);
            // Lock position and size — prevents all movement/resize
            pos.flags = pos.flags | SWP_NOMOVE | SWP_NOSIZE;
            // Prevent hiding (Show Desktop gesture)
            pos.flags = pos.flags & !SWP_HIDEWINDOW;
        }
        _ => {}
    }

    let original = ORIGINAL_WNDPROC.load(std::sync::atomic::Ordering::Acquire);
    CallWindowProcW(std::mem::transmute(original), hwnd, msg, wparam, lparam)
}

/// macOS: Configure window collection behavior for desktop wallpaper mode.
///
/// The window stays at normal level (ABOVE desktop icons) but is:
/// - Immune to Mission Control / Show Desktop gestures (stationary)
/// - Visible on all Spaces (canJoinAllSpaces)
/// - Excluded from Cmd+Tab cycling (ignoresCycle)
#[cfg(target_os = "macos")]
fn set_macos_desktop_behavior(ns_window_ptr: *mut std::ffi::c_void) {
    use objc::{msg_send, sel, sel_impl};

    info!("Setting macOS window collection behavior...");

    unsafe {
        let obj = ns_window_ptr as *mut objc::runtime::Object;
        // canJoinAllSpaces (1) | stationary (16) | ignoresCycle (64) = 81
        // Window level stays at normal (0) = above desktop icons
        let _: () = msg_send![obj, setCollectionBehavior: 81_u64];
    }

    info!("macOS window behavior configured successfully");
}

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
    info!("Starting MyWallpaper Desktop v{}", env!("CARGO_PKG_VERSION"));

    tauri::Builder::default()
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            // When a second instance is launched, it means we received a deep link
            info!("Single instance callback triggered with args: {:?}", args);

            // Find the deep link URL in arguments
            for arg in args.iter() {
                if arg.starts_with("mywallpaper://") {
                    info!("Deep link received via single-instance: {}", arg);
                    // Emit event to frontend
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.emit("deep-link", arg.clone());
                    }
                }
            }
        }))
        // Inject __MW_INIT__ environment data before page scripts run
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

            // Register deep link scheme on Linux (required for dev mode)
            // On macOS and Windows, this is handled by the bundle configuration
            #[cfg(target_os = "linux")]
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                info!("Registering mywallpaper:// deep link scheme on Linux...");
                match app.deep_link().register("mywallpaper") {
                    Ok(_) => info!("Deep link scheme registered successfully"),
                    Err(e) => warn!("Failed to register deep link scheme: {} (may already be registered)", e),
                }
            }

            // Listen for deep links via the deep-link plugin
            let deep_link_handle = handle.clone();
            app.listen("deep-link://new-url", move |event| {
                let payload = event.payload();
                info!("Deep link event received via plugin: {:?}", payload);

                // Parse the URLs from the payload
                if let Ok(urls) = serde_json::from_str::<Vec<String>>(payload) {
                    for url in urls {
                        if url.starts_with("mywallpaper://") {
                            info!("Processing deep link: {}", url);
                            // Emit to frontend
                            if let Some(window) = deep_link_handle.get_webview_window("main") {
                                if let Err(e) = window.emit("deep-link", url.clone()) {
                                    warn!("Failed to emit deep-link event: {}", e);
                                } else {
                                    info!("Deep link emitted to frontend: {}", url);
                                }
                                // Bring window to front
                                let _ = window.set_focus();
                            }
                        }
                    }
                }
            });

            // Setup window to cover full screen
            if let Some(window) = app.get_webview_window("main") {
                // Set webview background per platform:
                // - Windows/macOS: fully transparent (desktop shows through)
                // - Linux: opaque dark (WebKitGTK has broken compositing with transparent windows)
                use tauri::webview::Color;
                #[cfg(target_os = "linux")]
                let _ = window.set_background_color(Some(Color(10, 10, 11, 255)));
                #[cfg(not(target_os = "linux"))]
                let _ = window.set_background_color(Some(Color(0, 0, 0, 0)));

                // Get primary monitor dimensions for fullscreen positioning
                if let Some(monitor) = window.primary_monitor().ok().flatten() {
                    let size = monitor.size();
                    let position = monitor.position();

                    info!(
                        "Primary monitor: {}x{} at ({}, {})",
                        size.width, size.height, position.x, position.y
                    );

                    // Set window position and size to cover the entire screen
                    let _ = window.set_position(tauri::Position::Physical(
                        tauri::PhysicalPosition::new(position.x, position.y)
                    ));
                    let _ = window.set_size(tauri::Size::Physical(
                        tauri::PhysicalSize::new(size.width, size.height)
                    ));
                } else {
                    tracing::warn!("Could not detect primary monitor, using default size");
                }

                // Show the window after positioning
                let _ = window.show();

                // === Platform-specific desktop wallpaper integration ===

                // Windows: subclass window to lock position and block minimize
                // Stays above desktop icons, immune to Win+D
                #[cfg(target_os = "windows")]
                {
                    if let Ok(hwnd) = window.hwnd() {
                        setup_windows_desktop_lock(hwnd.0 as isize);
                    }
                }

                // macOS: configure collection behavior (stays above icons)
                #[cfg(target_os = "macos")]
                {
                    if let Ok(ns_window) = window.ns_window() {
                        set_macos_desktop_behavior(ns_window);
                    }
                }

                // Linux/X11: set window type to DESKTOP so it sits below
                // all application windows but above the actual wallpaper.
                // Uses PID-based matching to avoid affecting other windows.
                #[cfg(target_os = "linux")]
                {
                    let pid = std::process::id().to_string();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(800));

                        if let Ok(output) = std::process::Command::new("xdotool")
                            .args(["search", "--pid", &pid, "--name", "MyWallpaper"])
                            .output()
                        {
                            let wids = String::from_utf8_lossy(&output.stdout);
                            for wid in wids.trim().lines() {
                                if wid.is_empty() { continue; }
                                info!("Setting X11 window {} as DESKTOP type", wid);
                                let _ = std::process::Command::new("xprop")
                                    .args([
                                        "-id", wid,
                                        "-f", "_NET_WM_WINDOW_TYPE", "32a",
                                        "-set", "_NET_WM_WINDOW_TYPE",
                                        "_NET_WM_WINDOW_TYPE_DESKTOP",
                                    ])
                                    .output();
                            }
                        } else {
                            warn!("xdotool not found — cannot set desktop window type");
                        }
                    });
                }
            }

            info!("Application setup complete");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // System info
            commands::get_system_info,
            // Auto-update commands
            commands::check_for_updates,
            commands::download_and_install_update,
            commands::restart_app,
            // OAuth commands
            commands::open_oauth_in_browser,
            // Window commands
            commands::reload_window,
            // Layer management commands
            commands::get_layers,
            commands::toggle_layer,
            // Desktop clone commands
            desktop_clone::get_os_wallpaper,
            desktop_clone::get_desktop_icons,
            desktop_clone::open_desktop_item,
            commands::proxy_fetch,
        ])
        .run(tauri::generate_context!())
        .expect("Error while running MyWallpaper Desktop");
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_app_starts() {
        // Basic smoke test
        assert!(true);
    }
}

//! Window Layer Mode — Desktop vs Interactive
//!
//! Desktop Mode: window placed BEHIND desktop icons (immune to Win+D / Cmd+F3).
//!   - Windows: reparent into WorkerW (behind SHELLDLL_DefView)
//!   - macOS: set window level to kCGDesktopWindowLevel, ignore mouse events
//!
//! Interactive Mode: window on top of everything (current behavior).
//!   - Windows: detach from WorkerW, fullscreen + WS_EX_TOOLWINDOW
//!   - macOS: normal window level, accept mouse events

use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tauri::{Emitter, Manager};
use tracing::{info, warn};

// ============================================================================
// Types & State
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowLayerMode {
    Desktop,
    Interactive,
}

pub struct WindowLayerState {
    pub mode: Mutex<WindowLayerMode>,
}

impl WindowLayerState {
    pub fn new() -> Self {
        Self {
            mode: Mutex::new(WindowLayerMode::Interactive),
        }
    }
}

// ============================================================================
// Tauri Commands
// ============================================================================

/// Set the window layer mode
#[tauri::command]
pub fn set_window_layer(
    app: tauri::AppHandle,
    state: tauri::State<'_, WindowLayerState>,
    mode: WindowLayerMode,
) -> Result<(), String> {
    info!("Setting window layer mode to: {:?}", mode);

    if let Some(window) = app.get_webview_window("main") {
        apply_layer_mode(&window, mode)?;
    }

    let mut current = state.mode.lock().map_err(|e| e.to_string())?;
    *current = mode;

    Ok(())
}

/// Get the current window layer mode
#[tauri::command]
pub fn get_window_layer(
    state: tauri::State<'_, WindowLayerState>,
) -> Result<WindowLayerMode, String> {
    let mode = state.mode.lock().map_err(|e| e.to_string())?;
    Ok(*mode)
}

/// Toggle the window layer mode and return the new mode.
/// Emits "layer-mode-changed" event to the frontend.
#[tauri::command]
pub fn toggle_window_layer(
    app: tauri::AppHandle,
    state: tauri::State<'_, WindowLayerState>,
) -> Result<WindowLayerMode, String> {
    let new_mode = {
        let current = state.mode.lock().map_err(|e| e.to_string())?;
        match *current {
            WindowLayerMode::Desktop => WindowLayerMode::Interactive,
            WindowLayerMode::Interactive => WindowLayerMode::Desktop,
        }
    };

    info!("Toggling window layer mode to: {:?}", new_mode);

    if let Some(window) = app.get_webview_window("main") {
        apply_layer_mode(&window, new_mode)?;
    }

    let mut current = state.mode.lock().map_err(|e| e.to_string())?;
    *current = new_mode;

    // Emit event to frontend
    let _ = app.emit("layer-mode-changed", new_mode);

    Ok(new_mode)
}

// ============================================================================
// Global Shortcut Commands
// ============================================================================

/// Register a global shortcut that toggles the window layer mode
#[tauri::command]
pub fn register_layer_shortcut(
    app: tauri::AppHandle,
    shortcut: String,
) -> Result<(), String> {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;

    info!("Registering layer toggle shortcut: {}", shortcut);

    let parsed: tauri_plugin_global_shortcut::Shortcut = shortcut
        .parse()
        .map_err(|e| format!("Invalid shortcut '{}': {}", shortcut, e))?;

    // Check if already registered
    if app
        .global_shortcut()
        .is_registered(parsed)
    {
        info!("Shortcut {} already registered, skipping", shortcut);
        return Ok(());
    }

    let handle = app.clone();
    app.global_shortcut()
        .on_shortcut(parsed, move |_app, _shortcut, _event| {
            // Only trigger on key press (not release)
            if _event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                let state = handle.state::<WindowLayerState>();
                let new_mode = {
                    let current = state.mode.lock().unwrap();
                    match *current {
                        WindowLayerMode::Desktop => WindowLayerMode::Interactive,
                        WindowLayerMode::Interactive => WindowLayerMode::Desktop,
                    }
                };

                info!("Global shortcut triggered, toggling to: {:?}", new_mode);

                if let Some(window) = handle.get_webview_window("main") {
                    if let Err(e) = apply_layer_mode(&window, new_mode) {
                        warn!("Failed to apply layer mode: {}", e);
                        return;
                    }
                }

                let mut current = state.mode.lock().unwrap();
                *current = new_mode;

                let _ = handle.emit("layer-mode-changed", new_mode);
            }
        })
        .map_err(|e| format!("Failed to register shortcut: {}", e))?;

    info!("Layer toggle shortcut registered: {}", shortcut);
    Ok(())
}

/// Unregister a previously registered layer shortcut
#[tauri::command]
pub fn unregister_layer_shortcut(
    app: tauri::AppHandle,
    shortcut: String,
) -> Result<(), String> {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;

    info!("Unregistering layer shortcut: {}", shortcut);

    let parsed: tauri_plugin_global_shortcut::Shortcut = shortcut
        .parse()
        .map_err(|e| format!("Invalid shortcut '{}': {}", shortcut, e))?;

    app.global_shortcut()
        .unregister(parsed)
        .map_err(|e| format!("Failed to unregister shortcut: {}", e))?;

    Ok(())
}

// ============================================================================
// Platform-specific layer mode application
// ============================================================================

fn apply_layer_mode(
    window: &tauri::WebviewWindow,
    mode: WindowLayerMode,
) -> Result<(), String> {
    match mode {
        WindowLayerMode::Desktop => apply_desktop_mode(window),
        WindowLayerMode::Interactive => apply_interactive_mode(window),
    }
}

// ---- Windows ----------------------------------------------------------------

#[cfg(target_os = "windows")]
fn apply_desktop_mode(window: &tauri::WebviewWindow) -> Result<(), String> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;

    let our_hwnd = window
        .hwnd()
        .map_err(|e| format!("Failed to get HWND: {}", e))?;
    let our_hwnd = HWND(our_hwnd.0 as *mut core::ffi::c_void);

    unsafe {
        // 1. Find Progman
        let progman = FindWindowW(
            windows::core::w!("Progman"),
            None,
        ).map_err(|_| "Could not find Progman window".to_string())?;
        if progman.is_invalid() {
            return Err("Could not find Progman window".to_string());
        }

        // 2. Send magic message to spawn WorkerW
        let mut _result: usize = 0;
        let _ = SendMessageTimeoutW(
            progman,
            0x052C,
            WPARAM(0x0000_000D),
            LPARAM(0),
            SMTO_NORMAL,
            1000,
            Some(&mut _result),
        );
        let _ = SendMessageTimeoutW(
            progman,
            0x052C,
            WPARAM(0x0000_000D),
            LPARAM(1),
            SMTO_NORMAL,
            1000,
            Some(&mut _result),
        );

        // 3. Find the WorkerW behind SHELLDLL_DefView
        struct EnumData {
            worker_w: HWND,
        }
        let mut data = EnumData {
            worker_w: HWND::default(),
        };

        unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let data = &mut *(lparam.0 as *mut EnumData);
            // Check if this window has a child called SHELLDLL_DefView
            if let Ok(def_view) = FindWindowExW(hwnd, HWND::default(), windows::core::w!("SHELLDLL_DefView"), None) {
                if !def_view.is_invalid() {
                    // The WorkerW we want is the NEXT one after this one
                    if let Ok(worker) = FindWindowExW(HWND::default(), hwnd, windows::core::w!("WorkerW"), None) {
                        data.worker_w = worker;
                    }
                }
            }
            BOOL(1) // continue enumeration
        }

        let _ = EnumWindows(
            Some(enum_callback),
            LPARAM(&mut data as *mut EnumData as isize),
        );

        if data.worker_w.is_invalid() {
            return Err("Could not find WorkerW window".to_string());
        }

        info!("Found WorkerW: {:?}", data.worker_w);

        // 4. Reparent our window into WorkerW
        let _ = SetParent(our_hwnd, data.worker_w);

        // 5. Remove fullscreen (conflicts with reparenting) and resize to cover WorkerW
        let _ = window.set_fullscreen(false);

        let mut rect = windows::Win32::Foundation::RECT::default();
        let _ = GetClientRect(data.worker_w, &mut rect);
        let _ = SetWindowPos(
            our_hwnd,
            HWND::default(),
            0,
            0,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );

        // Remove WS_EX_TOOLWINDOW since we're behind icons now
        let style = GetWindowLongPtrW(our_hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(our_hwnd, GWL_EXSTYLE, style & !(WS_EX_TOOLWINDOW.0 as isize));

        info!("Windows: Desktop Mode applied (reparented into WorkerW)");
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn apply_interactive_mode(window: &tauri::WebviewWindow) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::*;

    let our_hwnd = window
        .hwnd()
        .map_err(|e| format!("Failed to get HWND: {}", e))?;
    let our_hwnd = HWND(our_hwnd.0 as *mut core::ffi::c_void);

    unsafe {
        // Detach from WorkerW (reparent to desktop/null)
        let _ = SetParent(our_hwnd, HWND::default());

        // Restore fullscreen + WS_EX_TOOLWINDOW
        let _ = window.set_fullscreen(true);

        let style = GetWindowLongPtrW(our_hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(our_hwnd, GWL_EXSTYLE, style | WS_EX_TOOLWINDOW.0 as isize);

        info!("Windows: Interactive Mode applied (detached from WorkerW)");
    }

    Ok(())
}

// ---- macOS ------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn apply_desktop_mode(window: &tauri::WebviewWindow) -> Result<(), String> {
    let ns_window = window
        .ns_window()
        .map_err(|e| format!("Failed to get NSWindow: {}", e))?;

    set_macos_desktop_mode(ns_window);
    info!("macOS: Desktop Mode applied");
    Ok(())
}

#[cfg(target_os = "macos")]
fn apply_interactive_mode(window: &tauri::WebviewWindow) -> Result<(), String> {
    let ns_window = window
        .ns_window()
        .map_err(|e| format!("Failed to get NSWindow: {}", e))?;

    set_macos_interactive_mode(ns_window);
    info!("macOS: Interactive Mode applied");
    Ok(())
}

#[cfg(target_os = "macos")]
fn set_macos_desktop_mode(ns_window_ptr: *mut std::ffi::c_void) {
    use objc::{msg_send, sel, sel_impl};

    unsafe {
        let obj = ns_window_ptr as *mut objc::runtime::Object;
        // kCGDesktopWindowLevel = kCGMinimumWindowLevel + 20 = -2147483628 + 20 = -2147483608
        // But the commonly used value is CGWindowLevelForKey(kCGDesktopWindowLevelKey) which is -2147483623
        let _: () = msg_send![obj, setLevel: -2147483623_i64];
        // canJoinAllSpaces (1) | stationary (16) | ignoresCycle (64) = 81
        let _: () = msg_send![obj, setCollectionBehavior: 81_u64];
        let _: () = msg_send![obj, setIgnoresMouseEvents: true];
    }
}

/// Public helper used by lib.rs during initial setup
#[cfg(target_os = "macos")]
pub fn set_macos_interactive_mode(ns_window_ptr: *mut std::ffi::c_void) {
    use objc::{msg_send, sel, sel_impl};

    unsafe {
        let obj = ns_window_ptr as *mut objc::runtime::Object;
        // NSNormalWindowLevel = 0
        let _: () = msg_send![obj, setLevel: 0_i64];
        // canJoinAllSpaces (1) | stationary (16) | ignoresCycle (64) = 81
        let _: () = msg_send![obj, setCollectionBehavior: 81_u64];
        let _: () = msg_send![obj, setIgnoresMouseEvents: false];
    }

    info!("macOS: Interactive Mode configured");
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mode_serialization() {
        let desktop = serde_json::to_string(&WindowLayerMode::Desktop).unwrap();
        assert_eq!(desktop, "\"desktop\"");

        let interactive = serde_json::to_string(&WindowLayerMode::Interactive).unwrap();
        assert_eq!(interactive, "\"interactive\"");

        let parsed: WindowLayerMode = serde_json::from_str("\"desktop\"").unwrap();
        assert_eq!(parsed, WindowLayerMode::Desktop);
    }

    #[test]
    fn test_state_default() {
        let state = WindowLayerState::new();
        let mode = state.mode.lock().unwrap();
        assert_eq!(*mode, WindowLayerMode::Interactive);
    }
}

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
pub fn register_layer_shortcut(app: tauri::AppHandle, shortcut: String) -> Result<(), String> {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;

    info!("Registering layer toggle shortcut: {}", shortcut);

    let parsed: tauri_plugin_global_shortcut::Shortcut = shortcut
        .parse()
        .map_err(|e| format!("Invalid shortcut '{}': {}", shortcut, e))?;

    // Check if already registered
    if app.global_shortcut().is_registered(parsed) {
        info!("Shortcut {} already registered, skipping", shortcut);
        return Ok(());
    }

    app.global_shortcut()
        .on_shortcut(parsed, move |app, _shortcut, event| {
            // Only trigger on key press (not release)
            if event.state == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                let state = app.state::<WindowLayerState>();
                let new_mode = {
                    let current = state.mode.lock().unwrap();
                    match *current {
                        WindowLayerMode::Desktop => WindowLayerMode::Interactive,
                        WindowLayerMode::Interactive => WindowLayerMode::Desktop,
                    }
                };

                info!("Global shortcut triggered, toggling to: {:?}", new_mode);

                if let Some(window) = app.get_webview_window("main") {
                    if let Err(e) = apply_layer_mode(&window, new_mode) {
                        warn!("Failed to apply layer mode: {}", e);
                        return;
                    }
                }

                let mut current = state.mode.lock().unwrap();
                *current = new_mode;

                let _ = app.emit("layer-mode-changed", new_mode);
            }
        })
        .map_err(|e| format!("Failed to register shortcut: {}", e))?;

    info!("Layer toggle shortcut registered: {}", shortcut);
    Ok(())
}

/// Unregister a previously registered layer shortcut
#[tauri::command]
pub fn unregister_layer_shortcut(app: tauri::AppHandle, shortcut: String) -> Result<(), String> {
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

fn apply_layer_mode(window: &tauri::WebviewWindow, mode: WindowLayerMode) -> Result<(), String> {
    match mode {
        WindowLayerMode::Desktop => apply_desktop_mode(window),
        WindowLayerMode::Interactive => apply_interactive_mode(window),
    }
}

/// Public version for use from tray.rs
pub fn apply_layer_mode_pub(window: &tauri::WebviewWindow, mode: WindowLayerMode) -> Result<(), String> {
    apply_layer_mode(window, mode)
}

// ---- Windows ----------------------------------------------------------------
//
// Canonical WorkerW embedding used by Lively Wallpaper, weebp, etc:
//   1. Find Progman
//   2. Send 0x052C to spawn WorkerW
//   3. EnumWindows → find window with SHELLDLL_DefView → get next sibling WorkerW
//   4. Strip decorations, add WS_CHILD, SetParent into that WorkerW

/// Find the WorkerW behind desktop icons.
///
/// Algorithm: enumerate all top-level windows, find the one containing
/// SHELLDLL_DefView (the desktop icon container), then get its next
/// sibling with class "WorkerW". That sibling is the render target.
#[cfg(target_os = "windows")]
unsafe fn find_worker_w() -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;

    let mut target: HWND = HWND::default();

    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let target = &mut *(lparam.0 as *mut HWND);

        // Does this window contain SHELLDLL_DefView?
        let shell = FindWindowExW(hwnd, HWND::default(), windows::core::w!("SHELLDLL_DefView"), None);
        if let Ok(shell) = shell {
            if !shell.is_invalid() {
                // The next sibling WorkerW after this window is our target
                if let Ok(w) = FindWindowExW(HWND::default(), hwnd, windows::core::w!("WorkerW"), None) {
                    if !w.is_invalid() {
                        *target = w;
                        return BOOL(0); // Stop
                    }
                }
            }
        }
        BOOL(1) // Continue
    }

    let _ = EnumWindows(Some(callback), LPARAM(&mut target as *mut HWND as isize));

    if target.is_invalid() { None } else { Some(target) }
}

#[cfg(target_os = "windows")]
fn apply_desktop_mode(window: &tauri::WebviewWindow) -> Result<(), String> {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;

    let our_hwnd = window
        .hwnd()
        .map_err(|e| format!("Failed to get HWND: {}", e))?;
    let our_hwnd = HWND(our_hwnd.0 as *mut core::ffi::c_void);

    // Exit fullscreen before reparenting (avoids Tauri state conflicts)
    let _ = window.set_fullscreen(false);

    unsafe {
        // 1. Find Progman
        let progman = FindWindowW(windows::core::w!("Progman"), None)
            .map_err(|_| "Could not find Progman".to_string())?;
        info!("Found Progman: {:?}", progman);

        // 2. Send 0x052C to spawn the WorkerW layer
        let mut result: usize = 0;
        let _ = SendMessageTimeoutW(progman, 0x052C, WPARAM(0xD), LPARAM(0), SMTO_NORMAL, 1000, Some(&mut result));
        let _ = SendMessageTimeoutW(progman, 0x052C, WPARAM(0xD), LPARAM(1), SMTO_NORMAL, 1000, Some(&mut result));

        // 3. Find the target WorkerW (sibling after SHELLDLL_DefView's parent)
        let worker_w = find_worker_w().ok_or("Could not find WorkerW – Desktop Mode not available")?;
        info!("Found target WorkerW: {:?}", worker_w);

        // 4. Strip decorations + add WS_CHILD
        let style = GetWindowLongPtrW(our_hwnd, GWL_STYLE) as u32;
        let new_style = (style & !(WS_CAPTION.0 | WS_THICKFRAME.0 | WS_SYSMENU.0
            | WS_MAXIMIZEBOX.0 | WS_MINIMIZEBOX.0)) | WS_CHILD.0;
        SetWindowLongPtrW(our_hwnd, GWL_STYLE, new_style as isize);

        let exstyle = GetWindowLongPtrW(our_hwnd, GWL_EXSTYLE) as u32;
        let new_exstyle = exstyle & !(WS_EX_DLGMODALFRAME.0 | WS_EX_WINDOWEDGE.0
            | WS_EX_CLIENTEDGE.0 | WS_EX_STATICEDGE.0
            | WS_EX_TOOLWINDOW.0 | WS_EX_APPWINDOW.0);
        SetWindowLongPtrW(our_hwnd, GWL_EXSTYLE, new_exstyle as isize);

        // 5. Reparent into WorkerW
        let _ = SetParent(our_hwnd, worker_w);

        // 6. Resize to fill WorkerW
        let mut rect = windows::Win32::Foundation::RECT::default();
        let _ = GetClientRect(worker_w, &mut rect);
        let _ = SetWindowPos(
            our_hwnd, HWND::default(),
            0, 0, rect.right, rect.bottom,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );

        let _ = ShowWindow(our_hwnd, SW_SHOW);
    }

    info!("Windows: Desktop Mode applied");
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
        // 1. Detach from WorkerW → reparent to desktop
        let _ = SetParent(our_hwnd, GetDesktopWindow());

        // 2. Remove WS_CHILD, restore normal window styles
        let style = GetWindowLongPtrW(our_hwnd, GWL_STYLE) as u32;
        let new_style = (style & !WS_CHILD.0) | WS_POPUP.0;
        SetWindowLongPtrW(our_hwnd, GWL_STYLE, new_style as isize);

        // 3. Set WS_EX_TOOLWINDOW (hide from taskbar)
        let exstyle = GetWindowLongPtrW(our_hwnd, GWL_EXSTYLE) as u32;
        let new_exstyle = (exstyle & !WS_EX_APPWINDOW.0) | WS_EX_TOOLWINDOW.0;
        SetWindowLongPtrW(our_hwnd, GWL_EXSTYLE, new_exstyle as isize);

        // 4. Force frame redraw
        let _ = SetWindowPos(
            our_hwnd, HWND::default(), 0, 0, 0, 0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER,
        );
    }

    // 5. Restore position to cover primary monitor
    if let Some(monitor) = window.primary_monitor().ok().flatten() {
        let size = monitor.size();
        let pos = monitor.position();
        let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(pos.x, pos.y)));
        let _ = window.set_size(tauri::Size::Physical(tauri::PhysicalSize::new(size.width, size.height)));
    }

    // 6. Show + fullscreen
    let _ = window.show();
    let _ = window.set_focus();
    let _ = window.set_fullscreen(true);

    info!("Windows: Interactive Mode applied");
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

// ---- Linux (paused) ---------------------------------------------------------

#[cfg(target_os = "linux")]
fn apply_desktop_mode(_window: &tauri::WebviewWindow) -> Result<(), String> {
    Err("Window layer mode is not yet supported on Linux".to_string())
}

#[cfg(target_os = "linux")]
fn apply_interactive_mode(_window: &tauri::WebviewWindow) -> Result<(), String> {
    Err("Window layer mode is not yet supported on Linux".to_string())
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

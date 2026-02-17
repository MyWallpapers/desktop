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
// Canonical WorkerW embedding (Lively Wallpaper, weebp):
//   1. Find Progman, send 0x052C to spawn WorkerW
//   2. EnumWindows → find window with SHELLDLL_DefView → get next sibling WorkerW
//   3. SetParent(our_hwnd, worker_w) + resize to fill

/// Diagnostic: enumerate all top-level windows and log their classes.
/// This helps debug why WorkerW might not be found.
#[cfg(target_os = "windows")]
unsafe fn log_desktop_hierarchy() {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;

    // Check Progman's direct children
    let progman = FindWindowW(windows::core::w!("Progman"), None);
    if let Ok(progman) = progman {
        // Does Progman directly contain SHELLDLL_DefView?
        let shell_in_progman = FindWindowExW(
            progman, HWND::default(),
            windows::core::w!("SHELLDLL_DefView"), None,
        );
        match shell_in_progman {
            Ok(h) if !h.is_invalid() => info!("[Diag] SHELLDLL_DefView is DIRECT child of Progman"),
            _ => info!("[Diag] SHELLDLL_DefView is NOT a direct child of Progman"),
        }
    }

    // Count WorkerW windows among top-level windows
    struct Counter { count: i32 }
    let mut counter = Counter { count: 0 };

    unsafe extern "system" fn count_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let counter = &mut *(lparam.0 as *mut Counter);
        let mut class_name = [0u16; 256];
        let len = GetClassNameW(hwnd, &mut class_name);
        if len > 0 {
            let name = String::from_utf16_lossy(&class_name[..len as usize]);
            if name == "WorkerW" {
                counter.count += 1;

                // Check if this WorkerW has SHELLDLL_DefView
                let shell = FindWindowExW(
                    hwnd, HWND::default(),
                    windows::core::w!("SHELLDLL_DefView"), None,
                );
                let has_shell = shell.map_or(false, |h| !h.is_invalid());
                tracing::info!(
                    "[Diag] Top-level WorkerW #{}: {:?}, has_SHELLDLL_DefView: {}",
                    counter.count, hwnd, has_shell
                );
            }
        }
        BOOL(1)
    }

    let _ = EnumWindows(Some(count_cb), LPARAM(&mut counter as *mut Counter as isize));
    info!("[Diag] Total top-level WorkerW windows: {}", counter.count);
}

/// Find the WorkerW behind desktop icons.
///
/// Algorithm: enumerate all top-level windows, find the one containing
/// SHELLDLL_DefView (the desktop icon container), then get its next
/// sibling with class "WorkerW". That sibling is the render target.
#[cfg(target_os = "windows")]
unsafe fn find_worker_w() -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;

    struct SearchResult {
        target: HWND,
        shell_parent: HWND,
    }

    let mut result = SearchResult {
        target: HWND::default(),
        shell_parent: HWND::default(),
    };

    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let result = &mut *(lparam.0 as *mut SearchResult);

        // Does this window contain SHELLDLL_DefView?
        let shell = FindWindowExW(hwnd, HWND::default(), windows::core::w!("SHELLDLL_DefView"), None);
        if let Ok(shell) = shell {
            if !shell.is_invalid() {
                result.shell_parent = hwnd;

                // The next sibling WorkerW after this window is our target
                if let Ok(w) = FindWindowExW(HWND::default(), hwnd, windows::core::w!("WorkerW"), None) {
                    if !w.is_invalid() {
                        result.target = w;
                        return BOOL(0); // Stop — found it
                    }
                }
                // SHELLDLL_DefView found but no sibling WorkerW — keep looking
                // (but we recorded shell_parent for diagnostics)
            }
        }
        BOOL(1) // Continue
    }

    let _ = EnumWindows(Some(callback), LPARAM(&mut result as *mut SearchResult as isize));

    if !result.shell_parent.is_invalid() {
        let mut class_name = [0u16; 256];
        let len = GetClassNameW(result.shell_parent, &mut class_name);
        let parent_class = if len > 0 {
            String::from_utf16_lossy(&class_name[..len as usize])
        } else {
            "unknown".to_string()
        };
        info!(
            "[FindWorkerW] SHELLDLL_DefView parent: {:?} (class: {})",
            result.shell_parent, parent_class
        );
    } else {
        warn!("[FindWorkerW] SHELLDLL_DefView not found in any top-level window!");
    }

    if result.target.is_invalid() {
        info!("[FindWorkerW] No sibling WorkerW found after SHELLDLL_DefView parent");
        None
    } else {
        info!("[FindWorkerW] Target WorkerW: {:?}", result.target);
        Some(result.target)
    }
}

#[cfg(target_os = "windows")]
fn apply_desktop_mode(window: &tauri::WebviewWindow) -> Result<(), String> {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;

    let our_hwnd = window
        .hwnd()
        .map_err(|e| format!("Failed to get HWND: {}", e))?;
    let our_hwnd = HWND(our_hwnd.0 as *mut core::ffi::c_void);

    unsafe {
        // Log the current desktop hierarchy before we start
        info!("=== Desktop Mode: starting ===");
        log_desktop_hierarchy();

        // 1. Find Progman
        let progman = FindWindowW(windows::core::w!("Progman"), None)
            .map_err(|_| "Could not find Progman".to_string())?;
        info!("Found Progman: {:?}", progman);

        // 2. Send 0x052C to spawn WorkerW (two variants for compatibility)
        let mut msg_result: usize = 0;

        // Method A: wParam=0xD (Win10+)
        let r1 = SendMessageTimeoutW(progman, 0x052C, WPARAM(0xD), LPARAM(0), SMTO_NORMAL, 1000, Some(&mut msg_result));
        info!("[0x052C] (0xD, 0) → result: {}, lresult: {}", r1.0, msg_result);

        let r2 = SendMessageTimeoutW(progman, 0x052C, WPARAM(0xD), LPARAM(1), SMTO_NORMAL, 1000, Some(&mut msg_result));
        info!("[0x052C] (0xD, 1) → result: {}, lresult: {}", r2.0, msg_result);

        // Method B: wParam=0 (Win11 / alternate)
        let r3 = SendMessageTimeoutW(progman, 0x052C, WPARAM(0), LPARAM(0), SMTO_NORMAL, 1000, Some(&mut msg_result));
        info!("[0x052C] (0, 0) → result: {}, lresult: {}", r3.0, msg_result);

        // 3. Wait for Windows to process the message and create WorkerW
        info!("Waiting 300ms for WorkerW creation...");
        std::thread::sleep(std::time::Duration::from_millis(300));

        // 4. Log hierarchy again after message
        log_desktop_hierarchy();

        // 5. Find the target WorkerW
        let worker_w = find_worker_w()
            .ok_or_else(|| {
                "Could not find WorkerW after 0x052C — check logs for diagnostic info".to_string()
            })?;
        info!("Target WorkerW: {:?}", worker_w);

        // 6. Reparent into WorkerW
        let _ = SetParent(our_hwnd, worker_w);

        // 7. Resize to fill WorkerW
        let mut rect = windows::Win32::Foundation::RECT::default();
        let _ = GetClientRect(worker_w, &mut rect);
        info!("WorkerW client rect: {}x{}", rect.right, rect.bottom);

        let _ = SetWindowPos(
            our_hwnd, HWND::default(),
            0, 0, rect.right, rect.bottom,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }

    info!("=== Desktop Mode: applied ===");
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
        // 1. Detach from WorkerW
        let _ = SetParent(our_hwnd, HWND::default());

        // 2. Ensure WS_EX_TOOLWINDOW (hide from taskbar, same as startup)
        let exstyle = GetWindowLongPtrW(our_hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(our_hwnd, GWL_EXSTYLE, exstyle | WS_EX_TOOLWINDOW.0 as isize);
    }

    // 3. Restore position + fullscreen (same as initial setup in lib.rs)
    if let Some(monitor) = window.primary_monitor().ok().flatten() {
        let size = monitor.size();
        let pos = monitor.position();
        let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition::new(pos.x, pos.y)));
        let _ = window.set_size(tauri::Size::Physical(tauri::PhysicalSize::new(size.width, size.height)));
    }

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

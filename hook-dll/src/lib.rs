//! Companion DLL for MyWallpaper Desktop — suppresses spurious WM_MOUSELEAVE.
//!
//! Loaded by the host app and injected into the WebView2 browser process via
//! SetWindowsHookExW(WH_GETMESSAGE). Intercepts WM_MOUSELEAVE messages for
//! Chrome_RenderWidgetHostHWND to prevent CSS :hover and scroll from breaking.
//!
//! Problem: After each PostMessage'd WM_MOUSEMOVE, Chromium calls
//! TrackMouseEvent(TME_LEAVE). Windows sees the real cursor is NOT over the
//! Chrome HWND (it's over SHELLDLL_DefView) and immediately posts WM_MOUSELEAVE.
//! This kills CSS :hover state and scroll momentum on every mouse move.
//!
//! Solution: This hook intercepts WM_MOUSELEAVE and replaces it with WM_NULL
//! (harmless no-op) unless the host app explicitly flagged it as intentional.
//!
//! Cross-process communication via window properties (SetPropW/GetPropW):
//! - "MWP_T": Target marker — set by host app on Chrome_RenderWidgetHostHWND
//! - "MWP_E": Explicit leave flag — set by host before intentional WM_MOUSELEAVE
//! - "MWP_SC": Suppress count — incremented by DLL each suppression (diagnostic)

#![cfg(target_os = "windows")]

use windows::Win32::Foundation::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

const WM_MOUSELEAVE_U32: u32 = 0x02A3;

/// Returns true if the HANDLE represents a set property (non-null).
/// GetPropW returns NULL (0) when a property doesn't exist.
/// We avoid HANDLE::is_invalid() because it may check for INVALID_HANDLE_VALUE (-1)
/// instead of NULL, which would give wrong results for GetPropW.
#[inline]
fn prop_is_set(h: HANDLE) -> bool {
    !h.0.is_null()
}

/// WH_GETMESSAGE hook procedure — called by Windows in the WebView2 browser process.
/// Intercepts posted messages before they are dispatched to the window procedure.
#[no_mangle]
pub unsafe extern "system" fn mouseleave_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code >= 0 && lparam.0 != 0 {
        let msg = &mut *(lparam.0 as *mut MSG);
        if msg.message == WM_MOUSELEAVE_U32 {
            // Check if this HWND is marked as our target
            let target = GetPropW(msg.hwnd, w!("MWP_T"));
            if prop_is_set(target) {
                // Check if host explicitly sent this WM_MOUSELEAVE
                let explicit = GetPropW(msg.hwnd, w!("MWP_E"));
                if prop_is_set(explicit) {
                    // Explicit leave from host hook — allow through, clear flag
                    let _ = RemovePropW(msg.hwnd, w!("MWP_E"));
                } else {
                    // Spurious WM_MOUSELEAVE from TrackMouseEvent — suppress
                    msg.message = WM_NULL;

                    // Diagnostic: increment suppress count property
                    let prev = GetPropW(msg.hwnd, w!("MWP_SC"));
                    let count = (prev.0 as usize).wrapping_add(1);
                    let _ = SetPropW(msg.hwnd, w!("MWP_SC"), HANDLE(count as *mut _));
                }
            }
        }
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

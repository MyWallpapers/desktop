//! Window Layer System — Web Desktop
//!
//! Windows: Injected into WorkerW (immune to Win+D).
//!          Mouse hook intercepts clicks on empty desktop space and forwards them to WebView.
//!          Native icons (ROLE_SYSTEM_LISTITEM) are ignored and process clicks natively.
//! macOS: kCGDesktopWindowLevel set behind desktop icons. Native icon hiding via Finder defaults.

use log::{info, warn};
use std::sync::atomic::{AtomicBool, Ordering};

// Flag de sécurité pour ne pas spammer le système à la fermeture
static ICONS_RESTORED: AtomicBool = AtomicBool::new(false);

// ============================================================================
// Setup Dispatch
// ============================================================================

pub fn setup_desktop_window(window: &tauri::WebviewWindow) {
    #[cfg(target_os = "windows")]
    if let Err(e) = ensure_in_worker_w(window) {
        warn!("Failed to setup Windows desktop layer: {}", e);
    }

    #[cfg(target_os = "macos")]
    if let Err(e) = setup_macos_desktop(window) {
        warn!("Failed to setup macOS desktop layer: {}", e);
    }
}

// ============================================================================
// UI Mode: Hide/Show Desktop Icons (Windows & macOS)
// ============================================================================

/// Commande Tauri appelée depuis le Frontend (JS/TS) pour masquer les icônes
#[tauri::command]
pub fn set_desktop_icons_visible(visible: bool) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOW};
        use windows::Win32::Foundation::HWND;

        let slv_hwnd = mouse_hook::get_syslistview_hwnd();
        if slv_hwnd != 0 {
            let slv = HWND(slv_hwnd as *mut core::ffi::c_void);
            unsafe {
                let _ = ShowWindow(slv, if visible { SW_SHOW } else { SW_HIDE });
            }
            info!("Windows: Desktop icons visibility set to {}", visible);
        } else {
            warn!("Cannot toggle icons: SysListView32 not found yet.");
        }
    }

    #[cfg(target_os = "macos")]
    {
        let val = if visible { "true" } else { "false" };
        let _ = std::process::Command::new("defaults")
            .args(["write", "com.apple.finder", "CreateDesktop", val])
            .output();
        let _ = std::process::Command::new("killall")
            .arg("Finder")
            .output();
        info!("macOS: Desktop icons visibility set to {}", visible);
    }

    Ok(())
}

/// Sécurité : Appelé automatiquement à la fermeture de l'app pour rendre le bureau
pub fn restore_desktop_icons() {
    // Si on l'a déjà fait, on annule pour éviter le double "killall Finder"
    if ICONS_RESTORED.swap(true, Ordering::SeqCst) {
        return;
    }

    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOW};
        use windows::Win32::Foundation::HWND;

        let slv_hwnd = mouse_hook::get_syslistview_hwnd();
        if slv_hwnd != 0 {
            let slv = HWND(slv_hwnd as *mut core::ffi::c_void);
            unsafe {
                let _ = ShowWindow(slv, SW_SHOW);
            }
            info!("Windows: Desktop icons restored on exit.");
        }
    }

    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("defaults")
            .args(["write", "com.apple.finder", "CreateDesktop", "true"])
            .output();
        let _ = std::process::Command::new("killall")
            .arg("Finder")
            .output();
        info!("macOS: Desktop icons restored on exit.");
    }
}

// ============================================================================
// Windows: Desktop Detection, Injection & Recovery
// ============================================================================

/// Résultat de la détection de la hiérarchie desktop Windows
#[cfg(target_os = "windows")]
struct DesktopDetection {
    is_24h2: bool,
    target_parent: windows::Win32::Foundation::HWND,
    shell_view: windows::Win32::Foundation::HWND,
    os_workerw: windows::Win32::Foundation::HWND,
    syslistview: windows::Win32::Foundation::HWND,
}

/// Détecte l'architecture desktop Windows (24H2 ou Legacy) et retourne tous les HWNDs
#[cfg(target_os = "windows")]
fn detect_desktop() -> Result<DesktopDetection, String> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;

    unsafe {
        let progman = FindWindowW(windows::core::w!("Progman"), None)
            .map_err(|_| "Could not find Progman".to_string())?;

        // Réveiller le bureau (spawn WorkerW si nécessaire)
        let mut msg_result: usize = 0;
        let _ = SendMessageTimeoutW(progman, 0x052C, WPARAM(0), LPARAM(0), SMTO_NORMAL, 1000, Some(&mut msg_result));
        let _ = SendMessageTimeoutW(progman, 0x052C, WPARAM(0x0D), LPARAM(0), SMTO_NORMAL, 1000, Some(&mut msg_result));
        let _ = SendMessageTimeoutW(progman, 0x052C, WPARAM(0x0D), LPARAM(1), SMTO_NORMAL, 1000, Some(&mut msg_result));

        let mut is_24h2 = false;
        let mut target_parent = HWND::default();
        let mut shell_view = HWND::default();
        let mut os_workerw = HWND::default();

        // Boucle de détection (max 2 secondes)
        for _ in 0..40 {
            // TEST A : Architecture Windows 11 24H2+ (Icônes et WorkerW dans Progman)
            let sv_prog = FindWindowExW(progman, HWND::default(), windows::core::w!("SHELLDLL_DefView"), None).unwrap_or(HWND::default());
            let ww_prog = FindWindowExW(progman, HWND::default(), windows::core::w!("WorkerW"), None).unwrap_or(HWND::default());

            if !sv_prog.is_invalid() && !ww_prog.is_invalid() {
                is_24h2 = true;
                target_parent = progman;
                shell_view = sv_prog;
                os_workerw = ww_prog;
                break;
            }

            // TEST B : Architecture Legacy (Windows 10 / 11 anciens)
            struct LegacyData {
                empty_workerw: HWND,
                shell_view: HWND,
            }
            let mut l_data = LegacyData { empty_workerw: HWND::default(), shell_view: HWND::default() };

            unsafe extern "system" fn legacy_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
                let d = &mut *(lparam.0 as *mut LegacyData);
                if let Ok(shell) = FindWindowExW(hwnd, HWND::default(), windows::core::w!("SHELLDLL_DefView"), None) {
                    if !shell.is_invalid() {
                        d.shell_view = shell;
                        if let Ok(w) = FindWindowExW(HWND::default(), hwnd, windows::core::w!("WorkerW"), None) {
                            if !w.is_invalid() { d.empty_workerw = w; }
                        }
                        return BOOL(0);
                    }
                }
                BOOL(1)
            }

            let _ = EnumWindows(Some(legacy_cb), LPARAM(&mut l_data as *mut LegacyData as isize));
            if !l_data.empty_workerw.is_invalid() && !l_data.shell_view.is_invalid() {
                is_24h2 = false;
                target_parent = l_data.empty_workerw;
                shell_view = l_data.shell_view;
                break;
            }

            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        if target_parent.is_invalid() {
            return Err("Desktop detection failed: neither 24H2 nor Legacy architecture found.".to_string());
        }

        // Trouver SysListView32 — scan all descendants (not just direct children)
        // On Win11 24H2+, SysListView32 may be nested deeper under SHELLDLL_DefView
        let mut syslistview = HWND::default();
        unsafe extern "system" fn enum_child_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let mut class_name = [0u16; 256];
            let len = GetClassNameW(hwnd, &mut class_name);
            let name = String::from_utf16_lossy(&class_name[..len as usize]);
            if name == "SysListView32" {
                let ptr = lparam.0 as *mut HWND;
                *ptr = hwnd;
                return BOOL(0); // Stop enumeration
            }
            BOOL(1)
        }
        let _ = EnumChildWindows(
            shell_view,
            Some(enum_child_cb),
            LPARAM(&mut syslistview as *mut _ as isize),
        );

        Ok(DesktopDetection { is_24h2, target_parent, shell_view, os_workerw, syslistview })
    }
}

/// Injecte notre WebView dans la hiérarchie desktop avec le bon Z-order
#[cfg(target_os = "windows")]
fn apply_injection(our_hwnd: windows::Win32::Foundation::HWND, detection: &DesktopDetection) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::*;

    unsafe {
        // Si déjà injecté dans le bon parent, ne rien faire
        if GetParent(our_hwnd) == Ok(detection.target_parent) {
            return;
        }

        // Règle d'or de DWM : Toute fenêtre injectée doit devenir un "Enfant"
        let mut style = GetWindowLongW(our_hwnd, GWL_STYLE);
        style &= !(WS_POPUP.0 as i32);
        style |= WS_CHILD.0 as i32;
        let _ = SetWindowLongW(our_hwnd, GWL_STYLE, style);

        let _ = SetParent(our_hwnd, detection.target_parent);

        if detection.is_24h2 {
            // Z-Order chirurgical dans Progman : Sous les icônes, SUR le vieux WorkerW
            let _ = SetWindowPos(our_hwnd, detection.shell_view, 0, 0, 0, 0, SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOSIZE | SWP_NOMOVE);
            let _ = SetWindowPos(detection.os_workerw, our_hwnd, 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOMOVE);
        } else {
            let _ = SetWindowPos(our_hwnd, HWND::default(), 0, 0, 0, 0, SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOSIZE | SWP_NOMOVE);
        }
    }
}

#[cfg(target_os = "windows")]
fn ensure_in_worker_w(window: &tauri::WebviewWindow) -> Result<(), String> {
    use tauri::Manager;
    use windows::Win32::Foundation::HWND;

    let our_hwnd = window.hwnd().map_err(|e| format!("Failed to get HWND: {}", e))?;
    let our_hwnd = HWND(our_hwnd.0 as *mut core::ffi::c_void);

    let detection = detect_desktop()?;

    // Enregistrer tous les HWNDs de la hiérarchie desktop pour le mouse hook
    mouse_hook::set_webview_hwnd(our_hwnd.0 as isize);
    mouse_hook::set_shell_view_hwnd(detection.shell_view.0 as isize);
    mouse_hook::set_target_parent_hwnd(detection.target_parent.0 as isize);
    if !detection.syslistview.is_invalid() {
        mouse_hook::set_syslistview_hwnd(detection.syslistview.0 as isize);
    }
    mouse_hook::set_app_handle(window.app_handle().clone());

    apply_injection(our_hwnd, &detection);

    // Extract composition controller from wry (stored during WebView2 creation)
    let comp_ptr = wry::get_last_composition_controller_ptr();
    if comp_ptr != 0 {
        mouse_hook::set_comp_controller_ptr(comp_ptr);
        info!("CompositionController ready: 0x{:X}", comp_ptr);
    } else {
        // WebView2 may create the controller slightly after window setup — retry briefly
        std::thread::spawn(move || {
            for _ in 0..60 {
                std::thread::sleep(std::time::Duration::from_millis(50));
                let ptr = wry::get_last_composition_controller_ptr();
                if ptr != 0 {
                    mouse_hook::set_comp_controller_ptr(ptr);
                    log::info!("CompositionController discovered: 0x{:X}", ptr);
                    return;
                }
            }
            log::warn!("CompositionController not found after 3s — mouse forwarding disabled");
        });
    }

    // Create hidden message-only window on the UI thread for dispatching
    // SendMouseInput calls (STA-bound — must run on the thread that created the WebView2)
    mouse_hook::init_dispatch_window();

    if detection.is_24h2 {
        info!("Moteur Windows 11 24H2 activé.");
    } else {
        info!("Moteur Windows Legacy activé.");
    }
    info!("Injected native Webview successfully.");

    mouse_hook::start_hook_thread();
    Ok(())
}

/// Tente de récupérer l'injection après un redémarrage d'Explorer
#[cfg(target_os = "windows")]
pub fn try_refresh_desktop() -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::IsWindow;

    let wv_hwnd = mouse_hook::get_webview_hwnd();
    if wv_hwnd == 0 { return false; }
    let our_hwnd = HWND(wv_hwnd as *mut core::ffi::c_void);

    // Vérifier que notre WebView existe encore (sinon l'app doit redémarrer)
    unsafe {
        if !IsWindow(our_hwnd).as_bool() {
            warn!("WebView HWND destroyed — cannot recover.");
            return false;
        }
    }

    match detect_desktop() {
        Ok(detection) => {
            // Mettre à jour les handles atomiques
            mouse_hook::set_shell_view_hwnd(detection.shell_view.0 as isize);
            mouse_hook::set_target_parent_hwnd(detection.target_parent.0 as isize);
            if !detection.syslistview.is_invalid() {
                mouse_hook::set_syslistview_hwnd(detection.syslistview.0 as isize);
            }
            // Ré-injecter dans la nouvelle hiérarchie
            apply_injection(our_hwnd, &detection);

            // Composition controller COM pointer survives Explorer restarts
            // (it's owned by our process, not Explorer's)

            if detection.is_24h2 {
                info!("Desktop recovered (24H2).");
            } else {
                info!("Desktop recovered (Legacy).");
            }
            true
        }
        Err(e) => {
            warn!("Desktop recovery failed: {}", e);
            false
        }
    }
}

// ============================================================================
// Windows: Mouse Hook
// ============================================================================

#[cfg(target_os = "windows")]
pub mod mouse_hook {
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, AtomicU8, Ordering};
    use std::sync::OnceLock;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

    // COREWEBVIEW2_MOUSE_EVENT_KIND values (from WebView2 IDL)
    const MOUSE_MOVE: i32 = 0x0200;         // WM_MOUSEMOVE
    const MOUSE_LBUTTON_DOWN: i32 = 0x0201; // WM_LBUTTONDOWN
    const MOUSE_LBUTTON_UP: i32 = 0x0202;   // WM_LBUTTONUP
    const MOUSE_RBUTTON_DOWN: i32 = 0x0204; // WM_RBUTTONDOWN
    const MOUSE_RBUTTON_UP: i32 = 0x0205;   // WM_RBUTTONUP
    const MOUSE_MBUTTON_DOWN: i32 = 0x0207; // WM_MBUTTONDOWN
    const MOUSE_MBUTTON_UP: i32 = 0x0208;   // WM_MBUTTONUP
    const MOUSE_WHEEL: i32 = 0x020A;        // WM_MOUSEWHEEL
    const MOUSE_HWHEEL: i32 = 0x020E;       // WM_MOUSEHWHEEL
    const MOUSE_LEAVE: i32 = 0x02A3;        // WM_MOUSELEAVE

    // COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS values
    const VK_NONE: i32 = 0x0;
    const VK_LBUTTON: i32 = 0x1;
    const VK_RBUTTON: i32 = 0x2;
    const VK_MBUTTON: i32 = 0x10;

    static WEBVIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static SYSLISTVIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static SHELL_VIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static TARGET_PARENT_HWND: AtomicIsize = AtomicIsize::new(0);
    static EXPLORER_PID: AtomicU32 = AtomicU32::new(0);

    /// Raw COM pointer to ICoreWebView2CompositionController.
    /// Set from the main thread after WebView2 creation.
    static COMP_CONTROLLER_PTR: AtomicIsize = AtomicIsize::new(0);

    /// Virtual key flag of the button that initiated the current drag
    static DRAG_VK: AtomicIsize = AtomicIsize::new(0);

    /// AppHandle for visibility watchdog events (wallpaper-visibility)
    static APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();

    static HOOK_STATE: AtomicU8 = AtomicU8::new(0);
    const STATE_IDLE: u8 = 0;
    const STATE_NATIVE: u8 = 1;
    const STATE_WEB: u8 = 2;

    /// Tracks whether cursor was over desktop on previous move (for SendMouseInput LEAVE)
    static WAS_OVER_DESKTOP: AtomicBool = AtomicBool::new(false);

    // --- Background COM thread bridge ---
    // The hook writes cursor coords here; a background thread polls and updates OVER_ICON.
    // This keeps accHitTest (cross-process COM RPC) out of the WH_MOUSE_LL hot path.
    static HOVER_X: AtomicI32 = AtomicI32::new(0);
    static HOVER_Y: AtomicI32 = AtomicI32::new(0);
    static NEEDS_ICON_CHECK: AtomicBool = AtomicBool::new(false);

    // --- UI thread dispatch for SendMouseInput (STA-bound) ---
    // SendMouseInput must be called from the thread that created the composition controller.
    // The hook runs on a separate thread, so we PostMessage to a hidden window on the UI thread.
    const WM_MWP_MOUSE: u32 = 0x8000 + 42; // WM_APP + 42
    static DISPATCH_HWND: AtomicIsize = AtomicIsize::new(0);

    pub fn set_webview_hwnd(hwnd: isize) { WEBVIEW_HWND.store(hwnd, Ordering::SeqCst); }
    pub fn set_syslistview_hwnd(hwnd: isize) { SYSLISTVIEW_HWND.store(hwnd, Ordering::SeqCst); }
    pub fn set_shell_view_hwnd(hwnd: isize) { SHELL_VIEW_HWND.store(hwnd, Ordering::SeqCst); }
    pub fn set_target_parent_hwnd(hwnd: isize) {
        TARGET_PARENT_HWND.store(hwnd, Ordering::SeqCst);
        // Cache Explorer PID from the target parent (Progman/WorkerW)
        if hwnd != 0 {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(HWND(hwnd as *mut _), Some(&mut pid)); }
            EXPLORER_PID.store(pid, Ordering::SeqCst);
        }
    }
    pub fn get_webview_hwnd() -> isize { WEBVIEW_HWND.load(Ordering::SeqCst) }
    pub fn get_syslistview_hwnd() -> isize { SYSLISTVIEW_HWND.load(Ordering::SeqCst) }
    pub fn set_app_handle(handle: tauri::AppHandle) { let _ = APP_HANDLE.set(handle); }
    pub fn set_comp_controller_ptr(ptr: isize) { COMP_CONTROLLER_PTR.store(ptr, Ordering::SeqCst); }
    fn get_comp_controller_ptr() -> isize { COMP_CONTROLLER_PTR.load(Ordering::SeqCst) }

    /// Queue a mouse event for dispatch on the UI thread via PostMessage.
    /// SendMouseInput is STA-bound and must be called from the UI thread.
    /// Layout: wparam = [mouse_data:32 | vkeys:16 | event:16], lparam = [y:16 | x:16]
    #[inline]
    unsafe fn send_input(event_kind: i32, virtual_keys: i32, mouse_data: u32, x: i32, y: i32) -> bool {
        let dh = DISPATCH_HWND.load(Ordering::Relaxed);
        if dh == 0 { return false; }
        let wparam = WPARAM(
            (event_kind as u16 as usize)
            | ((virtual_keys as u16 as usize) << 16)
            | ((mouse_data as usize) << 32)
        );
        let lparam = LPARAM(((x as i16 as u16 as u32) | ((y as i16 as u16 as u32) << 16)) as isize);
        PostMessageW(HWND(dh as *mut _), WM_MWP_MOUSE, wparam, lparam).is_ok()
    }

    /// WndProc for the hidden dispatch window — runs on the UI thread.
    /// Unpacks mouse event params and calls SendMouseInput.
    unsafe extern "system" fn dispatch_wnd_proc(
        hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_MWP_MOUSE {
            let event_kind = (wparam.0 & 0xFFFF) as i32;
            let virtual_keys = ((wparam.0 >> 16) & 0xFFFF) as i32;
            let mouse_data = ((wparam.0 >> 32) & 0xFFFFFFFF) as u32;
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;

            // Coalesce consecutive MOUSE_MOVE: if a newer MOVE is already queued, skip this one.
            // This prevents stale position updates from piling up when the UI thread is behind.
            if event_kind == MOUSE_MOVE {
                let mut peek = MSG::default();
                if PeekMessageW(&mut peek, hwnd, WM_MWP_MOUSE, WM_MWP_MOUSE, PM_NOREMOVE).into() {
                    if (peek.wParam.0 & 0xFFFF) as i32 == MOUSE_MOVE {
                        return LRESULT(0);
                    }
                }
            }

            let ptr = get_comp_controller_ptr();
            if ptr != 0 {
                if let Err(e) = wry::send_mouse_input_raw(ptr, event_kind, virtual_keys, mouse_data, x, y) {
                    static LOGGED: AtomicBool = AtomicBool::new(false);
                    if !LOGGED.swap(true, Ordering::Relaxed) {
                        log::warn!("SendMouseInput dispatch failed: {}", e);
                    }
                }
            }
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    /// Create a message-only window for dispatching SendMouseInput calls on the UI thread.
    /// Must be called from the main/UI thread (the thread that created the WebView2).
    pub fn init_dispatch_window() {
        unsafe {
            let class_name = windows::core::w!("MWP_MouseDispatch");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(dispatch_wnd_proc),
                lpszClassName: class_name,
                ..Default::default()
            };
            let _ = RegisterClassW(&wc);
            match CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class_name,
                windows::core::w!(""),
                WINDOW_STYLE(0),
                0, 0, 0, 0,
                HWND_MESSAGE,
                None, None, None,
            ) {
                Ok(h) => {
                    DISPATCH_HWND.store(h.0 as isize, Ordering::SeqCst);
                    log::info!("Mouse dispatch window created: 0x{:X}", h.0 as isize);
                }
                Err(e) => {
                    log::warn!("Failed to create mouse dispatch window: {}", e);
                }
            }
        }
    }

    /// Vérifie que tous les HWNDs desktop sont encore valides (détecte un redémarrage d'Explorer)
    pub fn validate_handles() -> bool {
        let wv = WEBVIEW_HWND.load(Ordering::SeqCst);
        if wv == 0 { return true; } // Pas encore initialisé

        let slv = SYSLISTVIEW_HWND.load(Ordering::SeqCst);
        let sv = SHELL_VIEW_HWND.load(Ordering::SeqCst);
        let tp = TARGET_PARENT_HWND.load(Ordering::SeqCst);

        unsafe {
            // Notre WebView doit toujours exister
            if !IsWindow(HWND(wv as *mut _)).as_bool() { return false; }
            // Les handles desktop doivent être valides (si initialisés)
            if slv != 0 && !IsWindow(HWND(slv as *mut _)).as_bool() { return false; }
            if sv != 0 && !IsWindow(HWND(sv as *mut _)).as_bool() { return false; }
            if tp != 0 && !IsWindow(HWND(tp as *mut _)).as_bool() { return false; }
            true
        }
    }

    unsafe fn is_mouse_over_desktop_icon(x: i32, y: i32) -> bool {
        use windows::Win32::UI::Accessibility::{AccessibleObjectFromWindow, IAccessible};
        use windows::core::Interface;
        use std::cell::RefCell;

        let slv_raw = get_syslistview_hwnd();
        if slv_raw == 0 { return false; }
        let slv = HWND(slv_raw as *mut core::ffi::c_void);

        // Cache the COM proxy thread-locally to avoid expensive cross-process
        // AccessibleObjectFromWindow calls on every mouse move.
        thread_local! {
            static CACHED_ACC: RefCell<Option<(isize, IAccessible)>> = RefCell::new(None);
        }

        CACHED_ACC.with(|cache| {
            let mut cache_mut = cache.borrow_mut();

            // Refresh cache if empty or Explorer restarted (HWND changed)
            if cache_mut.is_none() || cache_mut.as_ref().unwrap().0 != slv_raw {
                let mut raw_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
                let objid_client: u32 = 0xFFFFFFFC; // OBJID_CLIENT
                if AccessibleObjectFromWindow(slv, objid_client, &IAccessible::IID, &mut raw_ptr).is_err()
                    || raw_ptr.is_null()
                {
                    *cache_mut = None;
                    return false;
                }
                let acc: IAccessible = IAccessible::from_raw(raw_ptr);
                *cache_mut = Some((slv_raw, acc));
            }

            if let Some((_, acc)) = cache_mut.as_ref() {
                // accHitTest checks if screen point (x,y) is over a child item (icon).
                // Returns VT_I4(0) = background, VT_I4(>0) = icon child ID, VT_DISPATCH = icon object.
                match acc.accHitTest(x, y) {
                    Ok(hit) => {
                        if let Ok(val) = i32::try_from(&hit) {
                            val > 0
                        } else {
                            // Only VT_DISPATCH (=9) means a child accessible object (icon).
                            // VT_EMPTY, VT_NULL, or any other type = not an icon.
                            let vt = hit.as_raw().Anonymous.Anonymous.vt;
                            vt == 9 // VT_DISPATCH
                        }
                    }
                    Err(_) => {
                        // COM proxy invalid (Explorer restarted?) — clear cache
                        *cache_mut = None;
                        false
                    }
                }
            } else {
                false
            }
        })
    }

    // HWND cache — avoids recomputing is_over_desktop when cursor stays over the same window.
    // Saves ~6 Win32 API calls per mouse event in the common case (cursor over a normal app).
    static CACHED_HWND: AtomicIsize = AtomicIsize::new(0);
    static CACHED_IS_DESKTOP: AtomicBool = AtomicBool::new(false);

    /// Whether the cursor is currently hovering over a desktop icon (continuous tracking in IDLE).
    /// When true, ALL mouse events pass to OS — the "icon owns all inputs" behavior.
    static OVER_ICON: AtomicBool = AtomicBool::new(false);
    /// Tick counter for throttled icon hover checks (every 8th MOUSE_MOVE)
    static ICON_CHECK_TICK: AtomicU32 = AtomicU32::new(0);

    /// Check if hwnd_under is part of the desktop hierarchy, with caching.
    /// Only recomputes when the window under cursor changes.
    #[inline]
    unsafe fn check_is_over_desktop(hwnd_under: HWND, wv: HWND) -> bool {
        let cached = CACHED_HWND.load(Ordering::Relaxed);
        if hwnd_under.0 as isize == cached && cached != 0 {
            return CACHED_IS_DESKTOP.load(Ordering::Relaxed);
        }

        let slv = HWND(get_syslistview_hwnd() as *mut core::ffi::c_void);
        let sv = HWND(SHELL_VIEW_HWND.load(Ordering::Relaxed) as *mut core::ffi::c_void);
        let tp = HWND(TARGET_PARENT_HWND.load(Ordering::Relaxed) as *mut core::ffi::c_void);

        let mut result = hwnd_under == slv
            || hwnd_under == wv
            || hwnd_under == sv
            || hwnd_under == tp
            || IsChild(wv, hwnd_under).as_bool()
            || IsChild(tp, hwnd_under).as_bool();

        // Overlay detection for non-foreground transparent windows (Win11 Widgets/Copilot/Search)
        if !result {
            let fg = GetForegroundWindow();
            if hwnd_under != fg {
                let root = GetAncestor(hwnd_under, GA_ROOT);
                if root != fg && !root.is_invalid() {
                    let ex = GetWindowLongW(root, GWL_EXSTYLE) as u32;
                    let is_overlay = (ex & WS_EX_NOACTIVATE.0) != 0
                        || (ex & WS_EX_TOOLWINDOW.0) != 0
                        || ((ex & WS_EX_LAYERED.0) != 0 && (ex & WS_EX_APPWINDOW.0) == 0);
                    if is_overlay {
                        let explorer_pid = EXPLORER_PID.load(Ordering::Relaxed);
                        let mut overlay_pid = 0u32;
                        GetWindowThreadProcessId(root, Some(&mut overlay_pid));
                        if overlay_pid != explorer_pid && overlay_pid != 0 {
                            result = true;
                        }
                    }
                }
            }
        }

        CACHED_HWND.store(hwnd_under.0 as isize, Ordering::Relaxed);
        CACHED_IS_DESKTOP.store(result, Ordering::Relaxed);
        result
    }

    /// Forward a mouse event to the WebView via SendMouseInput dispatch.
    #[inline]
    unsafe fn forward_to_webview(msg: u32, info: &MSLLHOOKSTRUCT, client_pt: windows::Win32::Foundation::POINT) {
        // Clamp to non-negative — WebView2 rejects negative coords with 0x80070057
        let x = client_pt.x.max(0);
        let y = client_pt.y.max(0);

        match msg {
            WM_MOUSEMOVE => {
                let vk = DRAG_VK.load(Ordering::Relaxed) as i32;
                send_input(MOUSE_MOVE, vk, 0, x, y);
            }
            WM_LBUTTONDOWN => {
                DRAG_VK.store(VK_LBUTTON as isize, Ordering::Relaxed);
                send_input(MOUSE_LBUTTON_DOWN, VK_LBUTTON, 0, x, y);
            }
            WM_LBUTTONUP => {
                DRAG_VK.store(0, Ordering::Relaxed);
                send_input(MOUSE_LBUTTON_UP, VK_NONE, 0, x, y);
            }
            WM_RBUTTONDOWN => {
                DRAG_VK.store(VK_RBUTTON as isize, Ordering::Relaxed);
                send_input(MOUSE_RBUTTON_DOWN, VK_RBUTTON, 0, x, y);
            }
            WM_RBUTTONUP => {
                DRAG_VK.store(0, Ordering::Relaxed);
                send_input(MOUSE_RBUTTON_UP, VK_NONE, 0, x, y);
            }
            WM_MBUTTONDOWN => {
                DRAG_VK.store(VK_MBUTTON as isize, Ordering::Relaxed);
                send_input(MOUSE_MBUTTON_DOWN, VK_MBUTTON, 0, x, y);
            }
            WM_MBUTTONUP => {
                DRAG_VK.store(0, Ordering::Relaxed);
                send_input(MOUSE_MBUTTON_UP, VK_NONE, 0, x, y);
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let delta = (info.mouseData >> 16) as i16 as i32 as u32;
                let kind = if msg == WM_MOUSEWHEEL { MOUSE_WHEEL } else { MOUSE_HWHEEL };
                send_input(kind, VK_NONE, delta, x, y);
            }
            _ => {}
        }
    }

    pub fn start_hook_thread() {
        // Background COM thread — polls HOVER_X/Y at ~60Hz and updates OVER_ICON.
        // This keeps the expensive cross-process accHitTest RPC out of the hook.
        std::thread::spawn(|| {
            unsafe {
                use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            loop {
                if NEEDS_ICON_CHECK.swap(false, Ordering::Relaxed) {
                    let x = HOVER_X.load(Ordering::Relaxed);
                    let y = HOVER_Y.load(Ordering::Relaxed);
                    let over = unsafe { is_mouse_over_desktop_icon(x, y) };
                    OVER_ICON.store(over, Ordering::Relaxed);
                }
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
        });

        // OS hook thread
        std::thread::spawn(|| {
            unsafe {
                use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            }

            unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
                if code >= 0 {
                    let info = *(lparam.0 as *const MSLLHOOKSTRUCT);
                    let pt = info.pt;
                    let msg = wparam.0 as u32;

                    let wv_hwnd = get_webview_hwnd();
                    if wv_hwnd != 0 {
                        let wv = HWND(wv_hwnd as *mut core::ffi::c_void);
                        let is_down = msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN || msg == WM_MBUTTONDOWN;
                        let is_up = msg == WM_LBUTTONUP || msg == WM_RBUTTONUP || msg == WM_MBUTTONUP;
                        let state = HOOK_STATE.load(Ordering::Relaxed);

                        // ── Fast path: STATE_NATIVE — pass to OS, zero processing ──
                        if state == STATE_NATIVE {
                            if is_up { HOOK_STATE.store(STATE_IDLE, Ordering::Relaxed); }
                            return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                        }

                        // ── Fast path: STATE_WEB — forward to WebView, skip desktop detection ──
                        // During active click/drag, we always forward regardless of cursor position.
                        if state == STATE_WEB {
                            use windows::Win32::Graphics::Gdi::ScreenToClient;
                            let mut client_pt = pt;
                            let _ = ScreenToClient(wv, &mut client_pt);
                            forward_to_webview(msg, &info, client_pt);
                            if is_up { HOOK_STATE.store(STATE_IDLE, Ordering::Relaxed); }
                            if msg != WM_MOUSEMOVE { return LRESULT(1); }
                            return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                        }

                        // ── STATE_IDLE — desktop detection with HWND cache ──
                        let hwnd_under = WindowFromPoint(pt);
                        let is_over_desktop = check_is_over_desktop(hwnd_under, wv);

                        if !is_over_desktop {
                            OVER_ICON.store(false, Ordering::Relaxed);
                            if WAS_OVER_DESKTOP.swap(false, Ordering::Relaxed) {
                                send_input(MOUSE_LEAVE, VK_NONE, 0, 0, 0);
                            }
                            return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                        }

                        // Cursor is over desktop in IDLE state
                        if msg == WM_MOUSEMOVE {
                            WAS_OVER_DESKTOP.store(true, Ordering::Relaxed);
                            // Signal background thread for async icon hover check.
                            // No COM call here — hook returns instantly.
                            let tick = ICON_CHECK_TICK.fetch_add(1, Ordering::Relaxed);
                            if tick % 8 == 0 {
                                HOVER_X.store(pt.x, Ordering::Relaxed);
                                HOVER_Y.store(pt.y, Ordering::Relaxed);
                                NEEDS_ICON_CHECK.store(true, Ordering::Relaxed);
                            }
                        }

                        // State transition on mousedown — synchronous check required.
                        // Cannot rely on stale OVER_ICON: user may have moved from empty
                        // space to icon between background polls (16ms window).
                        if is_down {
                            if is_mouse_over_desktop_icon(pt.x, pt.y) {
                                OVER_ICON.store(true, Ordering::Relaxed);
                                HOOK_STATE.store(STATE_NATIVE, Ordering::Relaxed);
                                return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                            }
                            OVER_ICON.store(false, Ordering::Relaxed);
                            HOOK_STATE.store(STATE_WEB, Ordering::Relaxed);
                            WAS_OVER_DESKTOP.store(true, Ordering::Relaxed);
                        }

                        // If hovering over a desktop icon, pass all events to OS
                        if OVER_ICON.load(Ordering::Relaxed) {
                            return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                        }

                        // Forward to WebView (hover moves in IDLE + first click IDLE→WEB)
                        use windows::Win32::Graphics::Gdi::ScreenToClient;
                        let mut client_pt = pt;
                        let _ = ScreenToClient(wv, &mut client_pt);
                        forward_to_webview(msg, &info, client_pt);

                        if msg != WM_MOUSEMOVE { return LRESULT(1); }
                        return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                    }
                }
                CallNextHookEx(HHOOK::default(), code, wparam, lparam)
            }

            unsafe {
                let _h = SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), None, 0);
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, HWND::default(), 0, 0).into() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        });
    }
}

// ============================================================================
// Visibility Watchdog
// ============================================================================

pub mod visibility_watchdog {
    use tauri::AppHandle;

    #[cfg(target_os = "windows")]
    pub fn start(app: AppHandle) {
        use std::sync::OnceLock;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tauri::Emitter;

        static WATCHDOG_APP: OnceLock<AppHandle> = OnceLock::new();
        static WAS_VISIBLE: AtomicBool = AtomicBool::new(true);
        let _ = WATCHDOG_APP.set(app);

        std::thread::spawn(|| {
            use windows::Win32::UI::Accessibility::*;
            use windows::Win32::UI::WindowsAndMessaging::*;
            use windows::Win32::Graphics::Gdi::*;
            use windows::Win32::Foundation::*;

            /// Shared visibility check — called from event hooks and timer.
            unsafe fn check_visibility() {
                let wv_hwnd = super::mouse_hook::get_webview_hwnd();
                if wv_hwnd == 0 { return; }

                let fg = GetForegroundWindow();
                let desk = GetDesktopWindow();

                let is_visible = if fg == desk || fg.is_invalid() {
                    true
                } else {
                    let hmon_fg = MonitorFromWindow(fg, MONITOR_DEFAULTTOPRIMARY);
                    let hmon_wv = MonitorFromWindow(HWND(wv_hwnd as *mut _), MONITOR_DEFAULTTOPRIMARY);
                    if hmon_fg != hmon_wv {
                        true
                    } else {
                        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
                        if GetMonitorInfoW(hmon_fg, &mut mi).as_bool() {
                            let mut fg_rect = RECT::default();
                            let _ = GetWindowRect(fg, &mut fg_rect);
                            !(fg_rect.left <= mi.rcMonitor.left
                                && fg_rect.top <= mi.rcMonitor.top
                                && fg_rect.right >= mi.rcMonitor.right
                                && fg_rect.bottom >= mi.rcMonitor.bottom)
                        } else {
                            true
                        }
                    }
                };

                let was = WAS_VISIBLE.swap(is_visible, Ordering::Relaxed);
                if is_visible != was {
                    if let Some(app) = WATCHDOG_APP.get() {
                        let _ = app.emit("wallpaper-visibility", is_visible);
                    }
                }
            }

            /// Event callback for SetWinEventHook — fires on foreground changes, window moves, etc.
            unsafe extern "system" fn on_event(
                _hook: HWINEVENTHOOK, _event: u32, _hwnd: HWND,
                _obj: i32, _child: i32, _thread: u32, _time: u32,
            ) {
                check_visibility();
            }

            unsafe {
                // React to foreground window changes (Alt-Tab, click other app, Win+D)
                let _h1 = SetWinEventHook(
                    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND,
                    None, Some(on_event), 0, 0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                );
                // React to window move/resize end (maximize, fullscreen toggle)
                let _h2 = SetWinEventHook(
                    EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZEEND,
                    None, Some(on_event), 0, 0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                );
                // React to minimize start/end (restore from taskbar)
                let _h3 = SetWinEventHook(
                    EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND,
                    None, Some(on_event), 0, 0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                );

                // Fallback timer (10s) for Explorer restart detection
                const TIMER_ID: usize = 1;
                let _ = SetTimer(HWND::default(), TIMER_ID, 10_000, None);

                let mut msg = MSG::default();
                while GetMessageW(&mut msg, HWND::default(), 0, 0).into() {
                    if msg.message == WM_TIMER && msg.wParam.0 == TIMER_ID {
                        if !super::mouse_hook::validate_handles() {
                            log::warn!("Desktop handles stale — attempting recovery...");
                            if super::try_refresh_desktop() {
                                log::info!("Desktop hierarchy recovered.");
                            }
                        }
                        continue;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        });
    }

    #[cfg(not(target_os = "windows"))]
    pub fn start(_app: AppHandle) {
        // macOS App Nap handles pause natively
    }
}

// ============================================================================
// macOS Setup
// ============================================================================

#[cfg(target_os = "macos")]
fn setup_macos_desktop(window: &tauri::WebviewWindow) -> Result<(), String> {
    use tauri::Manager;

    // Dans Tauri 2, on récupère le pointeur NSWindow directement de manière sécurisée
    let ns_window = window.ns_window().map_err(|e| e.to_string())? as *mut objc::runtime::Object;

    use objc::{msg_send, sel, sel_impl};
    unsafe {
        // kCGDesktopWindowLevel = -2147483623
        let _: () = msg_send![ns_window, setLevel: -2147483623_isize];
        // CanJoinAllSpaces | Stationary | IgnoresCycle = 81
        let _: () = msg_send![ns_window, setCollectionBehavior: 81_usize];
        // Désactive les interactions directes pour laisser passer les clics au bureau si besoin
        let _: () = msg_send![ns_window, setIgnoresMouseEvents: true];
    }

    info!("macOS: Desktop window setup complete (Behind icons)");
    Ok(())
}

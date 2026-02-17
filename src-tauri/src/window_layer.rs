//! Window Layer System — Web Desktop
//!
//! Windows: Injected into WorkerW (immune to Win+D).
//!          Mouse hook intercepts clicks on empty desktop space and forwards them to WebView.
//!          Native icons (ROLE_SYSTEM_LISTITEM) are ignored and process clicks natively.
//! macOS: kCGDesktopWindowLevel set behind desktop icons. Native icon hiding via Finder defaults.

use tracing::{info, warn};
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
        // Sur macOS, on désactive l'affichage du bureau via le Finder
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
        // Sur macOS, on s'assure que le Finder réaffiche les icônes
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
// Windows: WorkerW Injection & Mouse Hook
// ============================================================================

#[cfg(target_os = "windows")]
fn ensure_in_worker_w(window: &tauri::WebviewWindow) -> Result<(), String> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;

    let our_hwnd = window.hwnd().map_err(|e| format!("Failed to get HWND: {}", e))?;
    let our_hwnd = HWND(our_hwnd.0 as *mut core::ffi::c_void);

    unsafe {
        let progman = FindWindowW(windows::core::w!("Progman"), None)
            .map_err(|_| "Could not find Progman".to_string())?;

        let mut msg_result: usize = 0;
        let _ = SendMessageTimeoutW(progman, 0x052C, WPARAM(0xD), LPARAM(0), SMTO_NORMAL, 1000, Some(&mut msg_result));
        let _ = SendMessageTimeoutW(progman, 0x052C, WPARAM(0xD), LPARAM(1), SMTO_NORMAL, 1000, Some(&mut msg_result));

        std::thread::sleep(std::time::Duration::from_millis(150));

        struct Found {
            worker_w: HWND,
            sys_list_view: HWND,
        }
        let mut found = Found { worker_w: HWND::default(), sys_list_view: HWND::default() };

        unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let found = &mut *(lparam.0 as *mut Found);
            if let Ok(shell) = FindWindowExW(hwnd, HWND::default(), windows::core::w!("SHELLDLL_DefView"), None) {
                if !shell.is_invalid() {
                    if let Ok(slv) = FindWindowExW(shell, HWND::default(), windows::core::w!("SysListView32"), None) {
                        if !slv.is_invalid() { found.sys_list_view = slv; }
                    }
                    if let Ok(w) = FindWindowExW(HWND::default(), hwnd, windows::core::w!("WorkerW"), None) {
                        if !w.is_invalid() {
                            found.worker_w = w;
                            return BOOL(0);
                        }
                    }
                }
            }
            BOOL(1)
        }

        let _ = EnumWindows(Some(callback), LPARAM(&mut found as *mut Found as isize));

        if found.worker_w.is_invalid() {
            return Err("Could not find WorkerW".to_string());
        }

        mouse_hook::set_webview_hwnd(our_hwnd.0 as isize);
        if !found.sys_list_view.is_invalid() {
            mouse_hook::set_syslistview_hwnd(found.sys_list_view.0 as isize);
        }

        let current_parent = GetParent(our_hwnd);
        if current_parent != found.worker_w {
            let _ = SetParent(our_hwnd, found.worker_w);
            let mut rect = windows::Win32::Foundation::RECT::default();
            let _ = GetClientRect(found.worker_w, &mut rect);
            let _ = SetWindowPos(our_hwnd, HWND::default(), 0, 0, rect.right, rect.bottom, SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW);
            info!("Injected into WorkerW ({}x{})", rect.right, rect.bottom);
        }
    }

    mouse_hook::start_hook_thread();
    Ok(())
}

#[cfg(target_os = "windows")]
pub mod mouse_hook {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::WindowsAndMessaging::*;

    static WEBVIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static SYSLISTVIEW_HWND: AtomicIsize = AtomicIsize::new(0);

    pub fn set_webview_hwnd(hwnd: isize) { WEBVIEW_HWND.store(hwnd, Ordering::SeqCst); }
    pub fn set_syslistview_hwnd(hwnd: isize) { SYSLISTVIEW_HWND.store(hwnd, Ordering::SeqCst); }
    pub fn get_webview_hwnd() -> isize { WEBVIEW_HWND.load(Ordering::SeqCst) }
    pub fn get_syslistview_hwnd() -> isize { SYSLISTVIEW_HWND.load(Ordering::SeqCst) }

    unsafe fn is_mouse_over_desktop_icon(x: i32, y: i32) -> bool {
        use windows::Win32::UI::Accessibility::{AccessibleObjectFromPoint, IAccessible};
        use windows::Win32::System::Variant::VARIANT;

        let pt = windows::Win32::Foundation::POINT { x, y };
        let mut p_acc: Option<IAccessible> = None;
        let mut var_child = VARIANT::default();

        if AccessibleObjectFromPoint(pt, &mut p_acc, &mut var_child).is_ok() {
            if let Some(acc) = p_acc {
                let mut role_var = VARIANT::default();
                if acc.get_accRole(&var_child, &mut role_var).is_ok() {
                    let role_val = role_var.Anonymous.Anonymous.Anonymous.lVal as u32;
                    if role_val == 34 { return true; } // 34 = ROLE_SYSTEM_LISTITEM
                }
            }
        }
        false
    }

    pub fn start_hook_thread() {
        std::thread::spawn(|| {
            unsafe {
                use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }

            unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
                if code >= 0 {
                    let info = *(lparam.0 as *const MSLLHOOKSTRUCT);
                    let pt = info.pt;

                    let hwnd_under = WindowFromPoint(pt);
                    let slv = HWND(get_syslistview_hwnd() as *mut core::ffi::c_void);
                    let wv = HWND(get_webview_hwnd() as *mut core::ffi::c_void);

                    if hwnd_under == slv && !wv.is_invalid() {
                        if is_mouse_over_desktop_icon(pt.x, pt.y) {
                            return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                        }

                        let msg = wparam.0 as u32;
                        let mut client_pt = pt;
                        let _ = ScreenToClient(wv, &mut client_pt);
                        let lparam_fw = ((client_pt.y as isize) << 16) | (client_pt.x as isize & 0xFFFF);

                        let mut fw_wparam: usize = 0;
                        if msg == WM_MOUSEWHEEL || msg == WM_MOUSEHWHEEL {
                            fw_wparam = (info.mouseData & 0xFFFF_0000) as usize;
                        }

                        let _ = PostMessageW(wv, msg, WPARAM(fw_wparam), LPARAM(lparam_fw));
                        return LRESULT(1);
                    }
                }
                CallNextHookEx(HHOOK::default(), code, wparam, lparam)
            }

            unsafe {
                if let Ok(h) = SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), windows::Win32::Foundation::HINSTANCE::default(), 0) {
                    tracing::info!("Global mouse hook installed (Hybrid Mode): {:?}", h);
                    let mut msg = MSG::default();
                    while GetMessageW(&mut msg, HWND::default(), 0, 0).into() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
        });
    }
}

// ============================================================================
// Visibility Watchdog
// ============================================================================

pub mod visibility_watchdog {
    use tauri::{AppHandle, Emitter};

    #[cfg(target_os = "windows")]
    pub fn start(app: AppHandle) {
        std::thread::spawn(move || {
            use std::time::Duration;
            use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect, GetDesktopWindow};
            use windows::Win32::Graphics::Gdi::{MonitorFromWindow, GetMonitorInfoW, MONITORINFO, MONITOR_DEFAULTTOPRIMARY};
            use windows::Win32::Foundation::RECT;

            let mut was_visible = true;

            loop {
                std::thread::sleep(Duration::from_secs(2));

                unsafe {
                    let fg_hwnd = GetForegroundWindow();
                    let desk_hwnd = GetDesktopWindow();

                    if fg_hwnd == desk_hwnd || fg_hwnd.is_invalid() {
                        if !was_visible {
                            let _ = app.emit("wallpaper-visibility", true);
                            was_visible = true;
                        }
                        continue;
                    }

                    let hmonitor = MonitorFromWindow(fg_hwnd, MONITOR_DEFAULTTOPRIMARY);
                    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
                    
                    if GetMonitorInfoW(hmonitor, &mut mi).as_bool() {
                        let mut fg_rect = RECT::default();
                        let _ = GetWindowRect(fg_hwnd, &mut fg_rect);

                        let is_fullscreen = fg_rect.left <= mi.rcMonitor.left
                            && fg_rect.top <= mi.rcMonitor.top
                            && fg_rect.right >= mi.rcMonitor.right
                            && fg_rect.bottom >= mi.rcMonitor.bottom;

                        let is_visible = !is_fullscreen;

                        if is_visible != was_visible {
                            was_visible = is_visible;
                            let _ = app.emit("wallpaper-visibility", is_visible);
                        }
                    }
                }
            }
        });
    }

    #[cfg(not(target_os = "windows"))]
    pub fn start(_app: AppHandle) {
        // macOS App Nap gère la pause nativement
    }
}

// ============================================================================
// macOS Setup
// ============================================================================

#[cfg(target_os = "macos")]
fn setup_macos_desktop(window: &tauri::WebviewWindow) -> Result<(), String> {
    let ns_window = window.ns_window().map_err(|e| format!("Failed to get NSWindow: {}", e))? as *mut std::ffi::c_void;

    use objc::{msg_send, sel, sel_impl};
    unsafe {
        let obj = ns_window as *mut objc::runtime::Object;
        
        // PARITÉ EXACTE : On place la fenêtre TOUT AU FOND, derrière les icônes du Mac
        let _: () = msg_send![obj, setLevel: -2147483623_i64];
        let _: () = msg_send![obj, setCollectionBehavior: 81_u64];
        
        // On laisse la souris traverser pour que le Finder puisse détecter les clics sur les icônes
        let _: () = msg_send![obj, setIgnoresMouseEvents: true];
    }

    macos_hook::start_hook_thread(window.app_handle().clone());
    
    info!("macOS: Desktop window setup complete (Behind icons)");
    Ok(())
}

#[cfg(target_os = "macos")]
pub mod macos_hook {
    use tauri::{AppHandle, Emitter};

    pub fn start_hook_thread(app: AppHandle) {
        std::thread::spawn(move || {
            use core_graphics::event::{CGEventTapLocation, CGEventTapPlacement, CGEventTapOptions, CGEventType, CGEventTap, CGEvent};
            use std::time::Duration;

            tracing::info!("macOS: Démarrage du Hook de souris en arrière-plan (Nécessite les droits d'Accessibilité)");

            loop {
                // CORRECTION : Le callback exige 3 arguments (_proxy, cg_type, cg_event)
                let tap_result = CGEventTap::new(
                    CGEventTapLocation::Session,
                    CGEventTapPlacement::HeadInsertEventTap,
                    CGEventTapOptions::ListenOnly,
                    vec![CGEventType::LeftMouseDown],
                    |_proxy, cg_type, cg_event| {
                        if cg_type == CGEventType::LeftMouseDown {
                            let pt = cg_event.location();
                            let _ = app.emit("mac-desktop-click", (pt.x, pt.y));
                        }
                        // CORRECTION : Doit renvoyer une copie de l'event pour macOS
                        Some(cg_event.clone())
                    },
                );

                match tap_result {
                    Ok(tap) => {
                        tracing::info!("macOS: Hook souris attaché avec succès !");
                        let run_loop_source = tap.mach_port.create_runloop_source(0).unwrap();
                        core_foundation::runloop::CFRunLoop::get_current().add_source(&run_loop_source, core_foundation::runloop::kCFRunLoopCommonModes);
                        
                        tap.enable();
                        core_foundation::runloop::CFRunLoop::run_current();
                        break;
                    }
                    Err(_) => {
                        tracing::warn!("macOS: Droits d'accessibilité manquants. En attente de l'autorisation...");
                        std::thread::sleep(Duration::from_secs(3));
                    }
                }
            }
        });
    }
}

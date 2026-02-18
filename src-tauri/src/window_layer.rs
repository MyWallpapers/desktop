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

        // 1. On réveille le bureau (Action requise pour toutes les versions de Windows)
        let mut msg_result: usize = 0;
        let _ = SendMessageTimeoutW(progman, 0x052C, WPARAM(0), LPARAM(0), SMTO_NORMAL, 1000, Some(&mut msg_result));
        let _ = SendMessageTimeoutW(progman, 0x052C, WPARAM(0x0D), LPARAM(0), SMTO_NORMAL, 1000, Some(&mut msg_result));
        let _ = SendMessageTimeoutW(progman, 0x052C, WPARAM(0x0D), LPARAM(1), SMTO_NORMAL, 1000, Some(&mut msg_result));

        // Variables pour mémoriser l'architecture détectée
        let mut is_24h2 = false;
        let mut target_parent = HWND::default();
        let mut shell_view = HWND::default();
        let mut os_workerw = HWND::default(); // Uniquement pour la 24H2

        // 2. BOUCLE DE DÉTECTION INTELLIGENTE (Attente max de 2 secondes)
        for _ in 0..40 {
            // TEST A : Architecture Windows 11 24H2+ (Icônes et WorkerW enfermés dans Progman)
            let sv_prog = FindWindowExW(progman, HWND::default(), windows::core::w!("SHELLDLL_DefView"), None).unwrap_or(HWND::default());
            let ww_prog = FindWindowExW(progman, HWND::default(), windows::core::w!("WorkerW"), None).unwrap_or(HWND::default());
            
            if !sv_prog.is_invalid() && !ww_prog.is_invalid() {
                is_24h2 = true;
                target_parent = progman;
                shell_view = sv_prog;
                os_workerw = ww_prog;
                break; // Détection 24H2 réussie !
            }

            // TEST B : Architecture Legacy (Windows 10 / 11 anciens)
            struct LegacyData {
                empty_workerw: HWND,
                shell_view: HWND,
            }
            let mut l_data = LegacyData { empty_workerw: HWND::default(), shell_view: HWND::default() };
            
            unsafe extern "system" fn legacy_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
                let d = &mut *(lparam.0 as *mut LegacyData);
                // On cherche la fenêtre qui possède les icônes
                if let Ok(shell) = FindWindowExW(hwnd, HWND::default(), windows::core::w!("SHELLDLL_DefView"), None) {
                    if !shell.is_invalid() {
                        d.shell_view = shell;
                        // Dans l'ancien système, le bureau vide est le frère jumeau directement suivant
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
                break; // Détection Legacy réussie !
            }

            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // Si après 2 secondes, aucune architecture n'a répondu
        if target_parent.is_invalid() {
            return Err("Impossible de s'injecter : Ni l'architecture Windows 11 24H2, ni la Legacy n'ont pu être construites.".to_string());
        }

        // Configuration du Hook de la souris
        mouse_hook::set_webview_hwnd(our_hwnd.0 as isize);
        if let Ok(slv) = FindWindowExW(shell_view, HWND::default(), windows::core::w!("SysListView32"), None) {
            if !slv.is_invalid() {
                mouse_hook::set_syslistview_hwnd(slv.0 as isize);
            }
        }

        // 3. INJECTION SELON L'ARCHITECTURE DE L'OS
        let current_parent = GetParent(our_hwnd);
        if current_parent != Ok(target_parent) {
            
            // Règle d'or de DWM : Toute fenêtre injectée doit devenir un "Enfant"
            let mut style = GetWindowLongW(our_hwnd, GWL_STYLE);
            style &= !(WS_POPUP.0 as i32); 
            style |= WS_CHILD.0 as i32;    
            let _ = SetWindowLongW(our_hwnd, GWL_STYLE, style);

            // On s'injecte dans le parent cible défini par notre moteur de détection
            let _ = SetParent(our_hwnd, target_parent);

            if is_24h2 {
                log::info!("Moteur Windows 11 24H2 activé.");
                // Z-Order chirurgical dans Progman : Sous les icônes (shell_view), mais SUR le vieux WorkerW
                let _ = SetWindowPos(our_hwnd, shell_view, 0, 0, 0, 0, SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOSIZE | SWP_NOMOVE);
                let _ = SetWindowPos(os_workerw, our_hwnd, 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOMOVE);
            } else {
                log::info!("Moteur Windows Legacy activé.");
                // Injection classique dans un espace vide, pas de Z-Order complexe
                let _ = SetWindowPos(our_hwnd, HWND::default(), 0, 0, 0, 0, SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOSIZE | SWP_NOMOVE);
            }
            
            log::info!("Injected native Webview successfully.");
        }
    }

    mouse_hook::start_hook_thread();
    Ok(())
}

#[cfg(target_os = "windows")]
pub mod mouse_hook {
    use std::sync::atomic::{AtomicIsize, AtomicBool, Ordering};
    use std::sync::OnceLock;
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::WindowsAndMessaging::*;

    static WEBVIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static SYSLISTVIEW_HWND: AtomicIsize = AtomicIsize::new(0);

    static IS_NATIVE_DRAG: AtomicBool = AtomicBool::new(false);
    static IS_WEB_DRAG: AtomicBool = AtomicBool::new(false);

    pub fn set_webview_hwnd(hwnd: isize) { WEBVIEW_HWND.store(hwnd, Ordering::SeqCst); }
    pub fn set_syslistview_hwnd(hwnd: isize) { SYSLISTVIEW_HWND.store(hwnd, Ordering::SeqCst); }
    
    pub fn get_webview_hwnd() -> isize { WEBVIEW_HWND.load(Ordering::SeqCst) }
    pub fn get_syslistview_hwnd() -> isize { SYSLISTVIEW_HWND.load(Ordering::SeqCst) }

    unsafe fn is_mouse_over_desktop_icon(x: i32, y: i32) -> bool {
        use windows::Win32::UI::Accessibility::{AccessibleObjectFromPoint, IAccessible};
        use windows::core::VARIANT;

        let pt = windows::Win32::Foundation::POINT { x, y };
        let mut p_acc: Option<IAccessible> = None;
        let mut var_child = VARIANT::default();

        if AccessibleObjectFromPoint(pt, &mut p_acc, &mut var_child).is_ok() {
            if let Some(acc) = p_acc {
                if let Ok(role_var) = acc.get_accRole(&var_child) {
                    if let Ok(role_val) = i32::try_from(&role_var) {
                        if role_val == 34 { return true; } 
                    }
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
                    let msg = wparam.0 as u32;

                    let hwnd_under = WindowFromPoint(pt);
                    let slv = HWND(get_syslistview_hwnd() as *mut core::ffi::c_void);
                    let wv = HWND(get_webview_hwnd() as *mut core::ffi::c_void);

                    if (hwnd_under == slv || hwnd_under == wv) && !wv.is_invalid() {
                        
                        let is_down = msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN || msg == WM_MBUTTONDOWN;
                        let is_up = msg == WM_LBUTTONUP || msg == WM_RBUTTONUP || msg == WM_MBUTTONUP;
                        let is_click_event = is_down || is_up || msg == WM_LBUTTONDBLCLK || msg == WM_RBUTTONDBLCLK;

                        if is_down {
                            if is_mouse_over_desktop_icon(pt.x, pt.y) {
                                IS_NATIVE_DRAG.store(true, Ordering::SeqCst);
                                IS_WEB_DRAG.store(false, Ordering::SeqCst);
                                return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                            } else {
                                IS_NATIVE_DRAG.store(false, Ordering::SeqCst);
                                IS_WEB_DRAG.store(true, Ordering::SeqCst);
                            }
                        } else if IS_NATIVE_DRAG.load(Ordering::SeqCst) {
                            if is_up { IS_NATIVE_DRAG.store(false, Ordering::SeqCst); }
                            return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                        } else if IS_WEB_DRAG.load(Ordering::SeqCst) {
                            if is_up { IS_WEB_DRAG.store(false, Ordering::SeqCst); }
                        } else {
                            if is_mouse_over_desktop_icon(pt.x, pt.y) {
                                return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                            }
                        }
                        
                        let mut client_pt = pt;
                        let _ = ScreenToClient(wv, &mut client_pt);
                        
                        // CORRECTION 1 : Formatage mathématique robuste des coordonnées
                        let lparam_fw = ((client_pt.y as u16 as isize) << 16) | (client_pt.x as u16 as isize);

                        let mut fw_wparam: usize = 0;
                        match msg {
                            WM_LBUTTONDOWN | WM_LBUTTONUP | WM_LBUTTONDBLCLK => fw_wparam = 0x0001,
                            WM_RBUTTONDOWN | WM_RBUTTONUP | WM_RBUTTONDBLCLK => fw_wparam = 0x0002,
                            WM_MOUSEMOVE => {
                                // CORRECTION 2 : Si on drag un élément sur le web, on maintient la pression du clic
                                if IS_WEB_DRAG.load(Ordering::SeqCst) {
                                    fw_wparam = 0x0001; 
                                }
                            }
                            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => fw_wparam = (info.mouseData & 0xFFFF_0000) as usize,
                            _ => {}
                        }

                        // CORRECTION 3 : Le Scanner Profond.
                        // On ignore le nom des dossiers parents et on fouille jusqu'à trouver le moteur Chromium.
                        struct SearchData { result: HWND }
                        let mut search = SearchData { result: HWND::default() };
                        unsafe extern "system" fn enum_cb(h: HWND, l: LPARAM) -> BOOL {
                            let mut class_name = [0u16; 256];
                            let len = GetClassNameW(h, &mut class_name);
                            if len > 0 {
                                let name = String::from_utf16_lossy(&class_name[..len as usize]);
                                if name.starts_with("Chrome_RenderWidgetHostHWND") {
                                    let d = &mut *(l.0 as *mut SearchData);
                                    d.result = h;
                                    return BOOL(0); // Moteur trouvé, on arrête !
                                }
                            }
                            BOOL(1) // Continuer de fouiller
                        }
                        let _ = EnumChildWindows(wv, Some(enum_cb), LPARAM(&mut search as *mut SearchData as isize));
                        let target_hwnd = if !search.result.is_invalid() { search.result } else { wv };

                        // On envoie le mouvement et les clics au bon moteur
                        let _ = PostMessageW(target_hwnd, msg, WPARAM(fw_wparam), LPARAM(lparam_fw));

                        if is_click_event {
                            return LRESULT(1);
                        } else {
                            return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                        }
                    }
                }
                CallNextHookEx(HHOOK::default(), code, wparam, lparam)
            }

            unsafe {
                if let Ok(_h) = SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), windows::Win32::Foundation::HINSTANCE::default(), 0) {
                    log::info!("Global mouse hook installed (Scanner profond actif)");
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
    use tauri::AppHandle;

    #[cfg(target_os = "windows")]
    pub fn start(app: AppHandle) {
        use tauri::Emitter;
        
        std::thread::spawn(move || {
            use std::time::Duration;
            use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect, GetDesktopWindow};
            use windows::Win32::Graphics::Gdi::{MonitorFromWindow, GetMonitorInfoW, MONITORINFO, MONITOR_DEFAULTTOPRIMARY};
            use windows::Win32::Foundation::{HWND, RECT};

            let mut was_visible = true;

            loop {
                std::thread::sleep(Duration::from_secs(2));

                unsafe {
                    let fg_hwnd = GetForegroundWindow();
                    let desk_hwnd = GetDesktopWindow();

                    // Si on est sur le bureau, on est forcément visible
                    if fg_hwnd == desk_hwnd || fg_hwnd.is_invalid() {
                        if !was_visible {
                            let _ = app.emit("wallpaper-visibility", true);
                            was_visible = true;
                        }
                        continue;
                    }

                    // --- RAFFINEMENT MULTI-ÉCRANS ---
                    // On récupère le moniteur de la fenêtre au premier plan
                    let hmonitor_fg = MonitorFromWindow(fg_hwnd, MONITOR_DEFAULTTOPRIMARY);
                    
                    // On récupère le moniteur de notre fond d'écran (via son HWND stocké)
                    let wv_hwnd = super::mouse_hook::get_webview_hwnd();
                    if wv_hwnd == 0 { continue; }
                    let hmonitor_wv = MonitorFromWindow(HWND(wv_hwnd as *mut _), MONITOR_DEFAULTTOPRIMARY);

                    // Si la fenêtre au premier plan n'est pas sur le même écran que nous, on reste visible
                    if hmonitor_fg != hmonitor_wv {
                        if !was_visible {
                            let _ = app.emit("wallpaper-visibility", true);
                            was_visible = true;
                        }
                        continue;
                    }

                    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
                    if GetMonitorInfoW(hmonitor_fg, &mut mi).as_bool() {
                        let mut fg_rect = RECT::default();
                        let _ = GetWindowRect(fg_hwnd, &mut fg_rect);

                        // On vérifie si la fenêtre remplit tout le moniteur (Plein écran / Jeu)
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
    use tauri::Manager; // On importe Manager uniquement pour macOS (évite le warning sur Windows)

    let ns_window = window.ns_window().map_err(|e| format!("Failed to get NSWindow: {}", e))? as *mut std::ffi::c_void;

    use objc::{msg_send, sel, sel_impl};
    unsafe {
        let obj = ns_window as *mut objc::runtime::Object;
        
        let _: () = msg_send![obj, setLevel: -2147483623_i64];
        let _: () = msg_send![obj, setCollectionBehavior: 81_u64];
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
            use core_graphics::event::{CGEventTapLocation, CGEventTapPlacement, CGEventTapOptions, CGEventType, CGEventTap};
            use std::time::Duration;

            log::info!("macOS: Démarrage du Hook de souris en arrière-plan (Nécessite les droits d'Accessibilité)");

            loop {
                let tap_result = CGEventTap::new(
                    CGEventTapLocation::Session,
                    CGEventTapPlacement::HeadInsertEventTap,
                    CGEventTapOptions::ListenOnly,
                    vec![CGEventType::LeftMouseDown],
                    |_proxy, cg_type, cg_event| {
                        if matches!(cg_type, CGEventType::LeftMouseDown) {
                            let pt = cg_event.location();
                            let _ = app.emit("mac-desktop-click", (pt.x, pt.y));
                        }
                        Some(cg_event.clone())
                    },
                );

                match tap_result {
                    Ok(tap) => {
                        log::info!("macOS: Hook souris attaché avec succès !");
                        let run_loop_source = tap.mach_port.create_runloop_source(0).unwrap();
                        
                        unsafe {
                            // Block unsafe nécessaire pour appeler les constantes C d'Apple
                            core_foundation::runloop::CFRunLoop::get_current().add_source(
                                &run_loop_source, 
                                core_foundation::runloop::kCFRunLoopCommonModes
                            );
                        }
                        
                        tap.enable();
                        core_foundation::runloop::CFRunLoop::run_current();
                        break;
                    }
                    Err(_) => {
                        log::warn!("macOS: Droits d'accessibilité manquants. En attente de l'autorisation...");
                        std::thread::sleep(Duration::from_secs(3));
                    }
                }
            }
        });
    }
}

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

        // Trouver SysListView32
        let mut syslistview = HWND::default();
        if let Ok(slv) = FindWindowExW(shell_view, HWND::default(), windows::core::w!("SysListView32"), None) {
            if !slv.is_invalid() {
                syslistview = slv;
            }
        }

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
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU8, Ordering};
    use std::sync::OnceLock;
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

    // WM_MOUSELEAVE n'est pas dans WindowsAndMessaging — défini manuellement
    const WM_MOUSELEAVE: u32 = 0x02A3;

    static WEBVIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static SYSLISTVIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static SHELL_VIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static TARGET_PARENT_HWND: AtomicIsize = AtomicIsize::new(0);
    static EXPLORER_PID: AtomicU32 = AtomicU32::new(0);

    /// AppHandle pour émettre des événements Tauri (scroll via JS au lieu de PostMessage)
    static APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();

    static HOOK_STATE: AtomicU8 = AtomicU8::new(0);
    const STATE_IDLE: u8 = 0;
    const STATE_NATIVE: u8 = 1;
    const STATE_WEB: u8 = 2;

    /// Tracks whether cursor was over desktop on previous move (for WM_MOUSELEAVE)
    static WAS_OVER_DESKTOP: AtomicBool = AtomicBool::new(false);

    /// Debug: sampled move counter for diagnostic logging
    static MOVE_DEBUG_COUNTER: AtomicU32 = AtomicU32::new(0);
    /// Debug: log first unknown window encounter
    static UNKNOWN_HWND_LOGGED: AtomicBool = AtomicBool::new(false);

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
        use windows::Win32::UI::Accessibility::{AccessibleObjectFromPoint, IAccessible};
        use windows::core::VARIANT;

        let pt = windows::Win32::Foundation::POINT { x, y };
        let mut p_acc: Option<IAccessible> = None;
        let mut var_child = VARIANT::default();

        if AccessibleObjectFromPoint(pt, &mut p_acc, &mut var_child).is_ok() {
            if let Some(acc) = p_acc {
                if let Ok(role_var) = acc.get_accRole(&var_child) {
                    if let Ok(role_val) = i32::try_from(&role_var) {
                        return role_val == 34; // 34 = ROLE_SYSTEM_LISTITEM
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

                        // On identifie la fenêtre sous le curseur et on vérifie si elle fait partie
                        // de la hiérarchie desktop (Progman/WorkerW/SHELLDLL_DefView/SysListView32/WebView)
                        let hwnd_under = WindowFromPoint(pt);
                        let slv = HWND(get_syslistview_hwnd() as *mut core::ffi::c_void);
                        let sv = HWND(SHELL_VIEW_HWND.load(Ordering::Relaxed) as *mut core::ffi::c_void);
                        let tp = HWND(TARGET_PARENT_HWND.load(Ordering::Relaxed) as *mut core::ffi::c_void);

                        let mut is_over_desktop = hwnd_under == slv
                            || hwnd_under == wv
                            || hwnd_under == sv    // SHELLDLL_DefView
                            || hwnd_under == tp    // Progman (24H2) ou WorkerW (Legacy)
                            || IsChild(wv, hwnd_under).as_bool()   // Chrome_RenderWidgetHostHWND etc.
                            || IsChild(tp, hwnd_under).as_bool();  // Tout enfant du parent desktop

                        // Détection d'overlay système (Win11 Widgets/Copilot/Search)
                        // Ces fenêtres sont transparentes visuellement mais bloquent WindowFromPoint.
                        if !is_over_desktop {
                            let root = GetAncestor(hwnd_under, GA_ROOT);
                            let fg = GetForegroundWindow();

                            // Si la fenêtre est au premier plan → c'est une vraie app, pas d'override
                            // Sinon, vérifier les styles d'overlay + PID non-Explorer
                            if root != fg && hwnd_under != fg && !root.is_invalid() {
                                let ex = GetWindowLongW(root, GWL_EXSTYLE) as u32;
                                let is_overlay_style = (ex & WS_EX_NOACTIVATE.0) != 0
                                    || (ex & WS_EX_TOOLWINDOW.0) != 0
                                    || ((ex & WS_EX_LAYERED.0) != 0 && (ex & WS_EX_APPWINDOW.0) == 0);

                                if is_overlay_style {
                                    let explorer_pid = EXPLORER_PID.load(Ordering::Relaxed);
                                    let mut overlay_pid = 0u32;
                                    GetWindowThreadProcessId(root, Some(&mut overlay_pid));
                                    if overlay_pid != explorer_pid && overlay_pid != 0 {
                                        is_over_desktop = true;
                                    }
                                }
                            }
                        }

                        // Fallback: walk parent chain — IsChild() may fail after SetParent injection
                        if !is_over_desktop {
                            let mut walk = hwnd_under;
                            for _ in 0..5 {
                                walk = GetAncestor(walk, GA_PARENT);
                                if walk.is_invalid() || walk == tp { break; }
                                if walk == wv {
                                    is_over_desktop = true;
                                    break;
                                }
                            }
                        }

                        // Diagnostic logging (sampled: 1/500 moves, all wheel events)
                        if msg == WM_MOUSEMOVE {
                            let count = MOVE_DEBUG_COUNTER.fetch_add(1, Ordering::Relaxed);
                            if count % 500 == 0 {
                                let mut cn = [0u16; 256];
                                let len = GetClassNameW(hwnd_under, &mut cn);
                                let class = String::from_utf16_lossy(&cn[..len as usize]);
                                log::info!("[MOUSE] move #{} hwnd=0x{:X} class='{}' over_desktop={} slv=0x{:X} wv=0x{:X} sv=0x{:X} tp=0x{:X}",
                                    count, hwnd_under.0 as isize, class, is_over_desktop,
                                    slv.0 as isize, wv.0 as isize, sv.0 as isize, tp.0 as isize);
                            }
                        } else if msg == WM_MOUSEWHEEL || msg == WM_MOUSEHWHEEL {
                            let mut cn = [0u16; 256];
                            let len = GetClassNameW(hwnd_under, &mut cn);
                            let class = String::from_utf16_lossy(&cn[..len as usize]);
                            log::info!("[MOUSE] wheel hwnd=0x{:X} class='{}' over_desktop={} delta={}",
                                hwnd_under.0 as isize, class, is_over_desktop,
                                (info.mouseData >> 16) as i16);
                        }

                        let mut state = HOOK_STATE.load(Ordering::SeqCst);

                        // Si la souris n'est PAS sur notre fond d'écran, on LAISSE PASSER LE CLIC AUX AUTRES FENÊTRES
                        if state == STATE_IDLE && !is_over_desktop {
                            // Transition desktop → hors-desktop : envoyer WM_MOUSELEAVE pour reset les :hover CSS
                            if WAS_OVER_DESKTOP.swap(false, Ordering::Relaxed) {
                                if let Some(handle) = APP_HANDLE.get() {
                                    use tauri::Emitter;
                                    let _ = handle.emit("desktop-mouse", serde_json::json!({
                                        "type": "mouseleave",
                                        "x": pt.x, "y": pt.y,
                                        "button": -1,
                                    }));
                                }
                            }
                            return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                        }

                        // Track la présence sur le desktop pour détecter la sortie
                        if msg == WM_MOUSEMOVE {
                            WAS_OVER_DESKTOP.store(true, Ordering::Relaxed);
                        }

                        let is_down = msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN || msg == WM_MBUTTONDOWN;
                        let is_up = msg == WM_LBUTTONUP || msg == WM_RBUTTONUP || msg == WM_MBUTTONUP;

                        // 1. MACHINE À ÉTATS
                        if is_down && state == STATE_IDLE {
                            if is_mouse_over_desktop_icon(pt.x, pt.y) {
                                state = STATE_NATIVE;
                            } else {
                                state = STATE_WEB;
                            }
                            HOOK_STATE.store(state, Ordering::SeqCst);
                        }

                        if state == STATE_NATIVE {
                            if is_up { HOOK_STATE.store(STATE_IDLE, Ordering::SeqCst); }
                            return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                        }

                        // 2. EMIT TAURI EVENT — All desktop mouse interactions go through JS
                        //    PostMessage to Chrome_RenderWidgetHostHWND had coordinate mapping issues.
                        //    Tauri event → JS document.elementFromPoint() → synthetic DOM event.
                        //
                        //    IMPORTANT: WM_MOUSEMOVE is NOT consumed (cursor stays visible) and
                        //    is throttled to ~60fps to avoid IPC flood. Clicks/wheel ARE consumed.
                        if let Some(handle) = APP_HANDLE.get() {
                            use tauri::Emitter;

                            let should_emit = if msg == WM_MOUSEMOVE {
                                // Throttle mousemove to ~60fps (16ms) using GetTickCount64
                                static LAST_MOVE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                                let now_ms = unsafe { windows::Win32::System::SystemInformation::GetTickCount64() };
                                let prev = LAST_MOVE_MS.load(Ordering::Relaxed);
                                if now_ms.wrapping_sub(prev) >= 16 {
                                    LAST_MOVE_MS.store(now_ms, Ordering::Relaxed);
                                    true
                                } else {
                                    false
                                }
                            } else {
                                true // Always emit clicks, wheel, etc.
                            };

                            if should_emit {
                                // Diagnostic logging
                                static EMIT_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                                let n = EMIT_COUNT.fetch_add(1, Ordering::Relaxed);
                                if n == 0 || n % 500 == 0 {
                                    log::info!("[EMIT] desktop-mouse #{} msg=0x{:X} pt=({},{}) state={}", n, msg, pt.x, pt.y, state);
                                }

                                match msg {
                                    WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                                        let delta = (info.mouseData >> 16) as i16;
                                        let horizontal = msg == WM_MOUSEHWHEEL;
                                        let _ = handle.emit("desktop-mouse", serde_json::json!({
                                            "type": "wheel",
                                            "x": pt.x, "y": pt.y,
                                            "button": -1,
                                            "deltaX": if horizontal { delta as i32 } else { 0 },
                                            "deltaY": if horizontal { 0 } else { -(delta as i32) },
                                        }));
                                    }
                                    _ => {
                                        let (event_type, btn) = match msg {
                                            WM_MOUSEMOVE => ("mousemove", -1i32),
                                            WM_LBUTTONDOWN => ("mousedown", 0),
                                            WM_LBUTTONUP => ("mouseup", 0),
                                            WM_RBUTTONDOWN => ("mousedown", 2),
                                            WM_RBUTTONUP => ("mouseup", 2),
                                            WM_MBUTTONDOWN => ("mousedown", 1),
                                            WM_MBUTTONUP => ("mouseup", 1),
                                            _ => ("unknown", -1),
                                        };
                                        let _ = handle.emit("desktop-mouse", serde_json::json!({
                                            "type": event_type,
                                            "x": pt.x, "y": pt.y,
                                            "button": btn,
                                        }));
                                    }
                                }
                            }
                        }

                        if is_up { HOOK_STATE.store(STATE_IDLE, Ordering::SeqCst); }

                        // Only consume clicks and wheel — let WM_MOUSEMOVE pass through
                        // so Windows keeps updating the cursor position (visibility + shape).
                        if msg != WM_MOUSEMOVE {
                            return LRESULT(1);
                        }
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
        use tauri::Emitter;

        std::thread::spawn(move || {
            use std::time::Duration;
            use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect, GetDesktopWindow};
            use windows::Win32::Graphics::Gdi::{MonitorFromWindow, GetMonitorInfoW, MONITORINFO, MONITOR_DEFAULTTOPRIMARY};
            use windows::Win32::Foundation::{HWND, RECT};

            let mut was_visible = true;

            loop {
                std::thread::sleep(Duration::from_secs(2));

                // Détection de stale HWNDs (redémarrage d'Explorer)
                if !super::mouse_hook::validate_handles() {
                    log::warn!("Desktop handles stale (Explorer restart?), attempting recovery...");
                    if super::try_refresh_desktop() {
                        log::info!("Desktop hierarchy recovered successfully.");
                    } else {
                        log::warn!("Desktop recovery failed — will retry next cycle.");
                    }
                    continue; // Skip visibility check this cycle
                }

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

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

    // Discover Chrome_RenderWidgetHostHWND (Chromium creates it after first navigation)
    mouse_hook::start_chrome_widget_discovery(our_hwnd);

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

            // Re-discover Chrome_RenderWidgetHostHWND (may have changed after Explorer restart)
            mouse_hook::set_chrome_widget_hwnd(0);
            mouse_hook::start_chrome_widget_discovery(our_hwnd);

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
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU16, AtomicU32, AtomicU8, Ordering};
    use std::sync::OnceLock;
    use windows::Win32::Foundation::{BOOL, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

    // Constants not re-exported by WindowsAndMessaging — define manually
    const WM_MOUSELEAVE: u32 = 0x02A3;
    const MK_LBUTTON: usize = 0x0001;
    const MK_RBUTTON: usize = 0x0002;
    const MK_MBUTTON: usize = 0x0010;

    static WEBVIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static SYSLISTVIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static SHELL_VIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static TARGET_PARENT_HWND: AtomicIsize = AtomicIsize::new(0);
    static EXPLORER_PID: AtomicU32 = AtomicU32::new(0);

    /// Chrome_RenderWidgetHostHWND — Chromium's internal input HWND inside WebView2.
    /// PostMessage to this HWND goes through Chromium's native input pipeline,
    /// enabling CSS :hover, scroll, click, drag without any JS adaptation.
    static CHROME_WIDGET_HWND: AtomicIsize = AtomicIsize::new(0);

    /// MK_* flag of the button that initiated the current drag (used for WM_MOUSEMOVE wparam)
    static DRAG_BUTTON_MK: AtomicU16 = AtomicU16::new(0);

    /// AppHandle for visibility watchdog events (wallpaper-visibility)
    static APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();

    static HOOK_STATE: AtomicU8 = AtomicU8::new(0);
    const STATE_IDLE: u8 = 0;
    const STATE_NATIVE: u8 = 1;
    const STATE_WEB: u8 = 2;

    /// Tracks whether cursor was over desktop on previous move (for WM_MOUSELEAVE)
    static WAS_OVER_DESKTOP: AtomicBool = AtomicBool::new(false);

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
    fn get_chrome_widget_hwnd() -> isize { CHROME_WIDGET_HWND.load(Ordering::SeqCst) }
    pub fn set_chrome_widget_hwnd(hwnd: isize) { CHROME_WIDGET_HWND.store(hwnd, Ordering::SeqCst); }

    /// Finds Chrome_RenderWidgetHostHWND inside the WebView2 HWND hierarchy.
    /// This is Chromium's internal input HWND — PostMessage to it goes through
    /// the native input pipeline (CSS :hover, scroll, click, drag all work).
    pub fn discover_chrome_widget(webview_hwnd: HWND) -> Option<HWND> {
        struct SearchData { found: HWND }
        let mut data = SearchData { found: HWND::default() };

        unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let mut cn = [0u16; 64];
            let len = GetClassNameW(hwnd, &mut cn);
            let class = String::from_utf16_lossy(&cn[..len as usize]);
            if class == "Chrome_RenderWidgetHostHWND" {
                let d = &mut *(lparam.0 as *mut SearchData);
                d.found = hwnd;
                return BOOL(0); // Stop enumeration
            }
            BOOL(1) // Continue
        }

        unsafe {
            let _ = EnumChildWindows(webview_hwnd, Some(enum_cb), LPARAM(&mut data as *mut SearchData as isize));
        }

        if data.found.is_invalid() { None } else { Some(data.found) }
    }

    /// Installs WH_GETMESSAGE hook on Chromium's UI thread via companion DLL.
    ///
    /// Chrome_RenderWidgetHostHWND is owned by the WebView2 browser process
    /// (msedgewebview2.exe), not our Tauri process. SetWindowsHookExW with
    /// hmod=None only works for same-process threads (ERROR_ACCESS_DENIED).
    /// For cross-process hooks, Windows requires a DLL to inject into the target.
    ///
    /// The DLL (mouseleave_hook.dll) contains the hook proc that suppresses
    /// spurious WM_MOUSELEAVE from TrackMouseEvent. Cross-process communication
    /// uses window properties (SetPropW/GetPropW) stored in the kernel's window
    /// manager — accessible from both processes.
    unsafe fn install_chrome_msg_hook(cw: HWND) {
        use windows::Win32::System::LibraryLoader::{LoadLibraryW, GetProcAddress};
        use windows::core::PCWSTR;
        use std::os::windows::ffi::OsStrExt;

        // Mark Chrome HWND as our target (readable cross-process via GetPropW)
        let prop_ok = SetPropW(cw, windows::core::w!("MWP_T"), HANDLE(1 as *mut _));
        log::info!("SetPropW(MWP_T) on Chrome HWND 0x{:X}: ok={}", cw.0 as isize, prop_ok.as_bool());

        // Find the hook DLL next to the executable
        let dll_path = match std::env::current_exe() {
            Ok(exe) => exe.parent().unwrap_or(std::path::Path::new(".")).join("mouseleave_hook.dll"),
            Err(_) => {
                log::warn!("Cannot determine exe path — hover suppression disabled");
                return;
            }
        };

        match std::fs::metadata(&dll_path) {
            Ok(m) => log::info!("mouseleave_hook.dll found at {:?} ({} bytes)", dll_path, m.len()),
            Err(_) => {
                log::warn!("mouseleave_hook.dll not found at {:?} — hover suppression disabled", dll_path);
                return;
            }
        }

        // Load the DLL into our process (Windows uses the path to load it into WebView2 too)
        let wide: Vec<u16> = dll_path.as_os_str().encode_wide().chain(Some(0)).collect();
        let hmod = match LoadLibraryW(PCWSTR(wide.as_ptr())) {
            Ok(h) => h,
            Err(e) => {
                log::warn!("Failed to load mouseleave_hook.dll: {} — hover suppression disabled", e);
                return;
            }
        };

        // Get the exported hook proc address
        let proc_addr = match GetProcAddress(hmod, windows::core::s!("mouseleave_hook_proc")) {
            Some(addr) => addr,
            None => {
                log::warn!("mouseleave_hook_proc not found in DLL — hover suppression disabled");
                return;
            }
        };

        let hook_proc: HOOKPROC = Some(std::mem::transmute(proc_addr));

        let chrome_tid = GetWindowThreadProcessId(cw, None);
        if chrome_tid == 0 {
            log::warn!("Failed to get Chrome HWND thread ID — hover suppression disabled");
            return;
        }

        // Install WH_GETMESSAGE hook on Chrome's thread.
        // Windows loads the DLL into the WebView2 browser process and calls
        // our hook proc there — suppressing spurious WM_MOUSELEAVE.
        match SetWindowsHookExW(WH_GETMESSAGE, hook_proc, hmod, chrome_tid) {
            Ok(h) => {
                log::info!(
                    "WH_GETMESSAGE hook installed on Chrome thread {} via DLL hook={:?}",
                    chrome_tid, h
                );
                // Diagnostic: monitor suppress count from the DLL
                let cw_raw = cw.0 as isize;
                std::thread::spawn(move || {
                    for i in 0..6 {
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        let cw = HWND(cw_raw as *mut core::ffi::c_void);
                        let sc = GetPropW(cw, windows::core::w!("MWP_SC"));
                        let count = sc.0 as usize;
                        log::info!("[DIAG] hook suppress count after {}s: {} (MWP_T={}, MWP_E={})",
                            (i + 1) * 3, count,
                            !GetPropW(cw, windows::core::w!("MWP_T")).0.is_null(),
                            !GetPropW(cw, windows::core::w!("MWP_E")).0.is_null(),
                        );
                    }
                });
            }
            Err(e) => log::warn!(
                "Failed to install WH_GETMESSAGE hook on Chrome thread {}: {}",
                chrome_tid, e
            ),
        }
    }

    /// Spawns a background thread that polls for Chrome_RenderWidgetHostHWND.
    /// Chrome creates this HWND after the first navigation, so we retry for up to 3 seconds.
    pub fn start_chrome_widget_discovery(webview_hwnd: HWND) {
        let wv_raw = webview_hwnd.0 as isize;
        std::thread::spawn(move || {
            let wv = HWND(wv_raw as *mut core::ffi::c_void);
            for _ in 0..60 { // 60 × 50ms = 3 seconds
                if let Some(cw) = discover_chrome_widget(wv) {
                    set_chrome_widget_hwnd(cw.0 as isize);
                    log::info!("Chrome_RenderWidgetHostHWND discovered: 0x{:X}", cw.0 as isize);
                    unsafe { install_chrome_msg_hook(cw); }
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            log::warn!("Chrome_RenderWidgetHostHWND not found after 3s — PostMessage forwarding disabled until re-discovery.");
        });
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

                        let mut state = HOOK_STATE.load(Ordering::SeqCst);

                        // Si la souris n'est PAS sur notre fond d'écran, on LAISSE PASSER LE CLIC AUX AUTRES FENÊTRES
                        if state == STATE_IDLE && !is_over_desktop {
                            // Transition desktop → hors-desktop : envoyer WM_MOUSELEAVE to reset CSS :hover
                            if WAS_OVER_DESKTOP.swap(false, Ordering::Relaxed) {
                                let cw_raw = get_chrome_widget_hwnd();
                                if cw_raw != 0 {
                                    let cw = HWND(cw_raw as *mut core::ffi::c_void);
                                    // Set property so our DLL hook lets this WM_MOUSELEAVE through
                                    let _ = SetPropW(cw, windows::core::w!("MWP_E"), HANDLE(1 as *mut _));
                                    let _ = PostMessageW(cw, WM_MOUSELEAVE, WPARAM(0), LPARAM(0));
                                    log::info!("[HOOK] cursor left desktop → sent explicit WM_MOUSELEAVE");
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
                                log::info!("[HOOK] state IDLE→NATIVE (click on icon) pt=({},{})", pt.x, pt.y);
                            } else {
                                state = STATE_WEB;
                                log::info!("[HOOK] state IDLE→WEB (click on desktop) pt=({},{})", pt.x, pt.y);
                            }
                            HOOK_STATE.store(state, Ordering::SeqCst);
                        }

                        if state == STATE_NATIVE {
                            if is_up {
                                HOOK_STATE.store(STATE_IDLE, Ordering::SeqCst);
                                log::info!("[HOOK] state NATIVE→IDLE (mouseup)");
                            }
                            return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                        }

                        // 2. POSTMESSAGE TO CHROME — Forward Win32 messages to Chromium's
                        //    internal input HWND. This goes through Chromium's native input
                        //    pipeline: CSS :hover, scroll, click, drag, focus all work natively.
                        //    No JS adaptation needed (approach used by Lively Wallpaper).
                        let cw_raw = get_chrome_widget_hwnd();

                        // Lazy re-discovery: if Chrome widget not found yet, try once
                        // (EnumChildWindows with 2-3 children takes <1ms, safe in hook)
                        let cw_raw = if cw_raw == 0 {
                            if let Some(found) = discover_chrome_widget(wv) {
                                set_chrome_widget_hwnd(found.0 as isize);
                                log::info!("Chrome_RenderWidgetHostHWND lazy-discovered: 0x{:X}", found.0 as isize);
                                found.0 as isize
                            } else {
                                0
                            }
                        } else {
                            cw_raw
                        };

                        if cw_raw != 0 {
                            let cw = HWND(cw_raw as *mut core::ffi::c_void);

                            // Convert screen coords → client coords for Chrome_RenderWidgetHostHWND.
                            // WM_MOUSEMOVE/LBUTTON*/etc. expect client-relative coordinates.
                            // WM_MOUSEWHEEL is the exception — it uses screen coords.
                            use windows::Win32::Graphics::Gdi::ScreenToClient;
                            let mut client_pt = pt;
                            let stc_ok = ScreenToClient(cw, &mut client_pt);
                            let lparam_client = LPARAM(((client_pt.y as u16 as u32) << 16 | (client_pt.x as u16 as u32)) as isize);
                            let lparam_screen = LPARAM(((pt.y as u16 as u32) << 16 | (pt.x as u16 as u32)) as isize);

                            // Sampled move log (every 200), all other events logged
                            static POST_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                            let n = POST_COUNT.fetch_add(1, Ordering::Relaxed);

                            match msg {
                                WM_MOUSEMOVE => {
                                    let mk = DRAG_BUTTON_MK.load(Ordering::Relaxed) as usize;
                                    let _ = PostMessageW(cw, WM_MOUSEMOVE, WPARAM(mk), lparam_client);
                                    if n < 3 || n % 200 == 0 {
                                        log::info!("[POST] #{} WM_MOUSEMOVE screen=({},{}) client=({},{}) stc={} mk={} state={}",
                                            n, pt.x, pt.y, client_pt.x, client_pt.y, stc_ok.as_bool(), mk, state);
                                    }
                                }
                                WM_LBUTTONDOWN => {
                                    DRAG_BUTTON_MK.store(MK_LBUTTON as u16, Ordering::Relaxed);
                                    let r = PostMessageW(cw, WM_LBUTTONDOWN, WPARAM(MK_LBUTTON), lparam_client);
                                    log::info!("[POST] WM_LBUTTONDOWN screen=({},{}) client=({},{}) ok={}", pt.x, pt.y, client_pt.x, client_pt.y, r.is_ok());
                                }
                                WM_LBUTTONUP => {
                                    DRAG_BUTTON_MK.store(0, Ordering::Relaxed);
                                    let r = PostMessageW(cw, WM_LBUTTONUP, WPARAM(0), lparam_client);
                                    log::info!("[POST] WM_LBUTTONUP screen=({},{}) client=({},{}) ok={}", pt.x, pt.y, client_pt.x, client_pt.y, r.is_ok());
                                }
                                WM_RBUTTONDOWN => {
                                    DRAG_BUTTON_MK.store(MK_RBUTTON as u16, Ordering::Relaxed);
                                    let r = PostMessageW(cw, WM_RBUTTONDOWN, WPARAM(MK_RBUTTON), lparam_client);
                                    log::info!("[POST] WM_RBUTTONDOWN screen=({},{}) client=({},{}) ok={}", pt.x, pt.y, client_pt.x, client_pt.y, r.is_ok());
                                }
                                WM_RBUTTONUP => {
                                    DRAG_BUTTON_MK.store(0, Ordering::Relaxed);
                                    let _ = PostMessageW(cw, WM_RBUTTONUP, WPARAM(0), lparam_client);
                                }
                                WM_MBUTTONDOWN => {
                                    DRAG_BUTTON_MK.store(MK_MBUTTON as u16, Ordering::Relaxed);
                                    let _ = PostMessageW(cw, WM_MBUTTONDOWN, WPARAM(MK_MBUTTON), lparam_client);
                                }
                                WM_MBUTTONUP => {
                                    DRAG_BUTTON_MK.store(0, Ordering::Relaxed);
                                    let _ = PostMessageW(cw, WM_MBUTTONUP, WPARAM(0), lparam_client);
                                }
                                WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                                    let delta = info.mouseData & 0xFFFF0000;
                                    let delta_val = (info.mouseData >> 16) as i16;
                                    let r = PostMessageW(cw, msg, WPARAM(delta as usize), lparam_screen);
                                    log::info!("[POST] WM_MOUSEWHEEL delta={} screen=({},{}) ok={} horiz={}",
                                        delta_val, pt.x, pt.y, r.is_ok(), msg == WM_MOUSEHWHEEL);
                                }
                                _ => {}
                            }
                        } else if msg != WM_MOUSEMOVE {
                            log::warn!("[HOOK] Chrome widget HWND=0 — cannot forward msg=0x{:X}", msg);
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

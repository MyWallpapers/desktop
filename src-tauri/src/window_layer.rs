//! Window Layer — Desktop WebView injection + mouse forwarding (Windows only).

#[cfg(target_os = "windows")]
use log::{error, info};
#[cfg(target_os = "windows")]
use std::sync::atomic::AtomicIsize;
use std::sync::atomic::{AtomicBool, Ordering};

static ICONS_RESTORED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "windows")]
static HOOK_HANDLE_GLOBAL: AtomicIsize = AtomicIsize::new(0);
#[cfg(target_os = "windows")]
static KB_HOOK_HANDLE_GLOBAL: AtomicIsize = AtomicIsize::new(0);
#[cfg(target_os = "windows")]
static IS_SESSION_ACTIVE: AtomicBool = AtomicBool::new(true);
#[cfg(target_os = "windows")]
static WATCHDOG_PARENT: AtomicIsize = AtomicIsize::new(0);
#[cfg(target_os = "windows")]
static INTERFACE_MODE: AtomicBool = AtomicBool::new(false);

// ==============================================================================
// Public API
// ==============================================================================

#[allow(unused_variables)]
pub fn setup_desktop_window(window: &tauri::WebviewWindow) {
    #[cfg(target_os = "windows")]
    {
        info!("[window_layer] Starting desktop window setup phase...");
        if let Err(e) = ensure_in_worker_w(window) {
            error!(
                "[window_layer] CRITICAL: Failed to setup desktop layer: {}",
                e
            );
        } else {
            info!("[window_layer] Desktop layer setup completed successfully.");
        }
    }
}

#[tauri::command]
#[allow(unused_variables)]
pub fn set_desktop_icons_visible(visible: bool) -> crate::error::AppResult<()> {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindowLongPtrW, SetWindowLongPtrW, ShowWindow, GWL_EXSTYLE, SW_HIDE, SW_SHOW,
            WS_EX_TRANSPARENT,
        };
        let slv = mouse_hook::get_syslistview_hwnd();
        if slv != 0 {
            unsafe {
                let _ = ShowWindow(HWND(slv as *mut _), if visible { SW_SHOW } else { SW_HIDE });
            }
        }

        // visible=false → interface mode (icons hidden, UI interactable)
        // visible=true  → wallpaper mode (icons shown, passthrough logic)
        let entering_interface = !visible;
        INTERFACE_MODE.store(entering_interface, Ordering::Relaxed);
        info!(
            "[window_layer] Mode switch: {}",
            if entering_interface {
                "INTERFACE"
            } else {
                "WALLPAPER"
            }
        );

        if !entering_interface {
            // Wallpaper mode: re-ajouter WS_EX_TRANSPARENT sur Chrome_RWHH UNIQUEMENT.
            // Chromium retire WS_EX_TRANSPARENT quand Chrome_RWHH reçoit des input (PostMessage
            // en mode interface). Sans WS_EX_TRANSPARENT, WindowFromPoint retourne Chrome_RWHH
            // et les hardware messages n'atteignent jamais SysListView32.
            // NE PAS toucher le WebView HWND (cause disparition).
            // NE PAS retirer en mode interface (PostMessage bypass les styles fenêtre).
            let rwhh = mouse_hook::get_chrome_rwhh_raw();
            if rwhh != 0 {
                unsafe {
                    let h = HWND(rwhh as *mut _);
                    let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
                    let new_ex = ex | (WS_EX_TRANSPARENT.0 as isize);
                    if new_ex != ex {
                        SetWindowLongPtrW(h, GWL_EXSTYLE, new_ex);
                        info!(
                            "[window_layer] Re-added WS_EX_TRANSPARENT on Chrome_RWHH {:#x}",
                            rwhh
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn unhook_global(handle: &AtomicIsize, name: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{UnhookWindowsHookEx, HHOOK};
    let ptr = handle.load(Ordering::SeqCst);
    if ptr != 0 {
        unsafe {
            if let Err(e) = UnhookWindowsHookEx(HHOOK(ptr as *mut _)) {
                error!("[window_layer] Unhook {} failed: {:?}", name, e);
            }
        }
    }
}

pub fn restore_desktop_icons_and_unhook() {
    if !ICONS_RESTORED.swap(true, Ordering::SeqCst) {
        #[cfg(target_os = "windows")]
        {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOW};

            let slv = mouse_hook::get_syslistview_hwnd();
            if slv != 0 {
                unsafe {
                    // ShowWindow returns BOOL (previous visibility state), not Result
                    let _ = ShowWindow(HWND(slv as *mut _), SW_SHOW);
                }
            }

            unhook_global(&HOOK_HANDLE_GLOBAL, "mouse hook");
            unhook_global(&KB_HOOK_HANDLE_GLOBAL, "keyboard hook");

            // Unregister WTS session notification and free process cache
            mouse_hook::unregister_session_notif();
            mouse_hook::invalidate_proc_cache_pub();
        }
    }
}

// ==============================================================================
// Windows: Helper Functions
// ==============================================================================

/// Zero-allocation UTF-16 class name comparison.
/// CRITICAL for mouse hook performance — avoids heap allocations on the
/// global Windows input thread where String::from_utf16_lossy would cause
/// system-wide micro-stutters.
#[cfg(target_os = "windows")]
unsafe fn is_class_name(hwnd: windows::Win32::Foundation::HWND, expected: &str) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    let mut buf = [0u16; 128];
    let len = GetClassNameW(hwnd, &mut buf) as usize;
    if len != expected.len() {
        return false;
    }
    expected
        .encode_utf16()
        .zip(buf[..len].iter())
        .all(|(a, b)| a == *b)
}

// ==============================================================================
// Windows: Desktop Detection
// ==============================================================================

#[cfg(target_os = "windows")]
struct DesktopDetection {
    progman: windows::Win32::Foundation::HWND,
    explorer_pid: u32,
    target_parent: windows::Win32::Foundation::HWND,
    syslistview: windows::Win32::Foundation::HWND,
    /// Sibling window that must stay IN FRONT of target_parent (Z-order).
    /// Win11 24H2+: SHELLDLL_DefView (child of Progman).
    /// Legacy: WorkerW that contains SHELLDLL_DefView.
    zorder_anchor: windows::Win32::Foundation::HWND,
    v_width: i32,
    v_height: i32,
}

#[cfg(target_os = "windows")]
fn detect_desktop() -> Result<DesktopDetection, crate::error::AppError> {
    use crate::error::AppError;
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
    use windows::Win32::UI::WindowsAndMessaging::*;

    unsafe {
        let progman = FindWindowW(windows::core::w!("Progman"), None)
            .map_err(|_| AppError::WindowLayer("Could not find Progman".into()))?;

        let mut explorer_pid: u32 = 0;
        GetWindowThreadProcessId(progman, Some(&mut explorer_pid));

        // Force Windows to spawn the wallpaper WorkerW layer.
        // This is an undocumented Progman message discovered via reverse engineering;
        // it triggers creation of the WorkerW window behind the desktop icons.
        const PROGMAN_SPAWN_WORKERW: u32 = 0x052C;
        let mut msg_result: usize = 0;
        let _ = SendMessageTimeoutW(
            progman,
            PROGMAN_SPAWN_WORKERW,
            WPARAM(0x0D),
            LPARAM(1),
            SMTO_NORMAL,
            1000,
            Some(&mut msg_result),
        );
        std::thread::sleep(std::time::Duration::from_millis(150));

        let mut target_parent;
        let shell_for_slv;

        // 1. Detection Win11 24H2+: SHELLDLL_DefView is direct child of Progman
        let shell_view = FindWindowExW(
            progman,
            HWND::default(),
            windows::core::w!("SHELLDLL_DefView"),
            None,
        )
        .unwrap_or_default();

        if !shell_view.is_invalid() {
            target_parent =
                FindWindowExW(progman, HWND::default(), windows::core::w!("WorkerW"), None)
                    .unwrap_or_default();
            shell_for_slv = shell_view;
        } else {
            // 2. Fallback Win10/Win11
            struct SearchData {
                parent: HWND,
                sv: HWND,
            }
            let mut data = SearchData {
                parent: HWND::default(),
                sv: HWND::default(),
            };

            unsafe extern "system" fn enum_cb(hwnd: HWND, lp: LPARAM) -> BOOL {
                if lp.0 == 0 {
                    return BOOL(0);
                }
                let sv = FindWindowExW(
                    hwnd,
                    HWND::default(),
                    windows::core::w!("SHELLDLL_DefView"),
                    None,
                )
                .unwrap_or_default();
                if !sv.is_invalid() {
                    let d = &mut *(lp.0 as *mut SearchData);
                    d.sv = sv;
                    d.parent =
                        FindWindowExW(HWND::default(), hwnd, windows::core::w!("WorkerW"), None)
                            .unwrap_or_default();
                    return BOOL(0);
                }
                BOOL(1)
            }
            let _ = EnumWindows(Some(enum_cb), LPARAM(&mut data as *mut _ as isize));
            target_parent = data.parent;
            shell_for_slv = data.sv;
        }

        // Compute Z-order anchor: the sibling that must stay IN FRONT
        // of target_parent so WindowFromPoint returns SysListView32.
        let zorder_anchor = if !shell_view.is_invalid() {
            // Win11 24H2+: SHELLDLL_DefView is a direct sibling of WorkerW
            shell_view
        } else if !shell_for_slv.is_invalid() {
            // Legacy: SHELLDLL_DefView is inside WorkerW A; get WorkerW A
            GetParent(shell_for_slv).unwrap_or_default()
        } else {
            HWND::default()
        };

        if target_parent.is_invalid() {
            target_parent = progman;
        }

        let mut syslistview = HWND::default();
        unsafe extern "system" fn find_slv(hwnd: HWND, lp: LPARAM) -> BOOL {
            if lp.0 == 0 {
                return BOOL(0);
            }
            if is_class_name(hwnd, "SysListView32") {
                *(lp.0 as *mut HWND) = hwnd;
                return BOOL(0);
            }
            BOOL(1)
        }
        if !shell_for_slv.is_invalid() {
            let _ = EnumChildWindows(
                shell_for_slv,
                Some(find_slv),
                LPARAM(&mut syslistview as *mut _ as isize),
            );
        }

        // Absolute Physical Bounds
        struct MonitorRects {
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
        }
        let mut m_rects = MonitorRects {
            left: i32::MAX,
            top: i32::MAX,
            right: i32::MIN,
            bottom: i32::MIN,
        };
        unsafe extern "system" fn monitor_enum_cb(
            _hm: HMONITOR,
            _hdc: HDC,
            rect: *mut RECT,
            lparam: LPARAM,
        ) -> BOOL {
            if lparam.0 == 0 || rect.is_null() {
                return BOOL(1);
            }
            let data = &mut *(lparam.0 as *mut MonitorRects);
            let r = rect.read();
            data.left = data.left.min(r.left);
            data.top = data.top.min(r.top);
            data.right = data.right.max(r.right);
            data.bottom = data.bottom.max(r.bottom);
            BOOL(1)
        }
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(monitor_enum_cb),
            LPARAM(&mut m_rects as *mut _ as isize),
        );

        let width = m_rects.right - m_rects.left;
        let height = m_rects.bottom - m_rects.top;
        info!(
            "[detect_desktop] Screen: {}x{}, WorkerW: 0x{:X}, explorer pid={}",
            width, height, target_parent.0 as isize, explorer_pid
        );

        Ok(DesktopDetection {
            progman,
            explorer_pid,
            target_parent,
            syslistview,
            zorder_anchor,
            v_width: width,
            v_height: height,
        })
    }
}

// ==============================================================================
// Windows: Injection Execution
// ==============================================================================

/// WM_NCCALCSIZE subclass: forces zero non-client area so the client rect
/// fills the entire window rect. Without this, DefWindowProc may compute
/// a non-zero non-client inset from residual styles, producing visible
/// border gaps (top/left/right) on Windows 11.
#[cfg(target_os = "windows")]
const NCCALC_SUBCLASS_ID: usize = 0xDEAD_BEE0;

#[cfg(target_os = "windows")]
unsafe extern "system" fn nccalc_subclass_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
    uid_subclass: usize,
    _ref_data: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{WM_NCCALCSIZE, WM_NCDESTROY};

    match msg {
        WM_NCCALCSIZE => LRESULT(0), // Zero non-client area
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(hwnd, Some(nccalc_subclass_proc), uid_subclass);
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        _ => DefSubclassProc(hwnd, msg, wparam, lparam),
    }
}

#[cfg(target_os = "windows")]
fn apply_injection(our_hwnd: windows::Win32::Foundation::HWND, detection: &DesktopDetection) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::*;

    unsafe {
        if GetParent(our_hwnd).unwrap_or_default() == detection.target_parent {
            return;
        }

        // 1. Strip ALL frame / border styles
        let mut style = GetWindowLongW(our_hwnd, GWL_STYLE) as u32;
        style &= !(WS_THICKFRAME.0
            | WS_CAPTION.0
            | WS_SYSMENU.0
            | WS_MAXIMIZEBOX.0
            | WS_MINIMIZEBOX.0
            | WS_POPUP.0
            | WS_BORDER.0
            | WS_DLGFRAME.0);
        style |= WS_CHILD.0 | WS_VISIBLE.0;
        let _ = SetWindowLongW(our_hwnd, GWL_STYLE, style as i32);

        let mut ex_style = GetWindowLongW(our_hwnd, GWL_EXSTYLE) as u32;
        ex_style &= !(WS_EX_LAYERED.0
            | WS_EX_NOACTIVATE.0
            | WS_EX_CLIENTEDGE.0
            | WS_EX_WINDOWEDGE.0
            | WS_EX_DLGMODALFRAME.0
            | WS_EX_STATICEDGE.0);
        let _ = SetWindowLongW(our_hwnd, GWL_EXSTYLE, ex_style as i32);

        // 2. WM_NCCALCSIZE subclass → zero non-client area
        let _ = windows::Win32::UI::Shell::SetWindowSubclass(
            our_hwnd,
            Some(nccalc_subclass_proc),
            NCCALC_SUBCLASS_ID,
            0,
        );

        // 3. Kill DWM border rendering
        use windows::Win32::Graphics::Dwm::*;
        let color_none: u32 = 0xFFFFFFFE; // DWMWA_COLOR_NONE
        let no_round: i32 = 1; // DWMWCP_DONOTROUND
        let _ = DwmSetWindowAttribute(
            our_hwnd,
            DWMWA_BORDER_COLOR,
            &color_none as *const _ as *const _,
            std::mem::size_of::<u32>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            our_hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &no_round as *const _ as *const _,
            std::mem::size_of::<i32>() as u32,
        );

        // 4. Black background brush
        use windows::Win32::Graphics::Gdi::{GetStockObject, BLACK_BRUSH};
        SetClassLongPtrW(
            our_hwnd,
            GCLP_HBRBACKGROUND,
            GetStockObject(BLACK_BRUSH).0 as isize,
        );

        // 5. Reparent into WorkerW (SW_SHOWNA preserves Z-order)
        let _ = ShowWindow(detection.target_parent, SW_SHOWNA);
        let _ = SetParent(our_hwnd, detection.target_parent);

        // 6. Size to full monitor + force frame recalc
        let _ = SetWindowPos(
            our_hwnd,
            HWND::default(),
            0,
            0,
            detection.v_width,
            detection.v_height,
            SWP_FRAMECHANGED | SWP_SHOWWINDOW | SWP_NOZORDER,
        );
        let _ = ShowWindow(our_hwnd, SW_SHOW);

        // 7. Ensure WorkerW is BEHIND the icon layer so WindowFromPoint
        //    returns SysListView32, enabling fully native icon interactions
        //    (drag & drop, double-click, context menus, selection rectangle).
        if !detection.zorder_anchor.is_invalid()
            && detection.zorder_anchor != detection.target_parent
        {
            let _ = SetWindowPos(
                detection.target_parent,
                detection.zorder_anchor,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }

        info!(
            "[apply_injection] Done. Parent=0x{:X}, Size={}x{}",
            detection.target_parent.0 as isize, detection.v_width, detection.v_height
        );
    }
}

// ==============================================================================
// Windows: Initialization
// ==============================================================================

#[cfg(target_os = "windows")]
fn ensure_in_worker_w(window: &tauri::WebviewWindow) -> crate::error::AppResult<()> {
    use windows::Win32::Foundation::HWND;

    let _ = window.set_ignore_cursor_events(false);
    let our_hwnd_raw = window.hwnd()?;
    let our_hwnd = HWND(our_hwnd_raw.0 as *mut _);

    let detection = detect_desktop()?;

    mouse_hook::set_webview_hwnd(our_hwnd.0 as isize);
    mouse_hook::set_target_parent_hwnd(detection.target_parent.0 as isize);
    mouse_hook::set_progman_hwnd(detection.progman.0 as isize);
    mouse_hook::set_explorer_pid(detection.explorer_pid);
    if !detection.syslistview.is_invalid() {
        mouse_hook::set_syslistview_hwnd(detection.syslistview.0 as isize);
    }
    apply_injection(our_hwnd, &detection);
    mouse_hook::init_dispatch_window();

    let (w, h) = (detection.v_width, detection.v_height);
    let our_hwnd_isize = our_hwnd.0 as isize;

    std::thread::spawn(move || {
        use windows::Win32::Foundation::{BOOL, HWND, LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::*;

        let mut found = false;
        for _ in 1..=100 {
            let ptr = wry::get_last_composition_controller_ptr();
            if ptr != 0 {
                mouse_hook::set_comp_controller_ptr(ptr);

                unsafe {
                    let wv_h = HWND(our_hwnd_isize as *mut _);
                    let _ = SetWindowPos(
                        wv_h,
                        HWND::default(),
                        0,
                        0,
                        w,
                        h,
                        SWP_NOZORDER | SWP_SHOWWINDOW | SWP_FRAMECHANGED,
                    );

                    // Fix all child windows: strip borders, set black brush, force full size
                    struct FixData {
                        w: i32,
                        h: i32,
                    }
                    let fd = FixData { w, h };
                    unsafe extern "system" fn enum_fix_children(child: HWND, lp: LPARAM) -> BOOL {
                        if lp.0 == 0 {
                            return BOOL(0);
                        }
                        let d = &*(lp.0 as *const FixData);
                        let mut st = GetWindowLongW(child, GWL_STYLE) as u32;
                        st &= !(WS_BORDER.0 | WS_THICKFRAME.0 | WS_DLGFRAME.0 | WS_CAPTION.0);
                        let _ = SetWindowLongW(child, GWL_STYLE, st as i32);

                        let mut ex = GetWindowLongW(child, GWL_EXSTYLE) as u32;
                        ex &= !(WS_EX_CLIENTEDGE.0
                            | WS_EX_WINDOWEDGE.0
                            | WS_EX_STATICEDGE.0
                            | WS_EX_DLGMODALFRAME.0);
                        let _ = SetWindowLongW(child, GWL_EXSTYLE, ex as i32);

                        use windows::Win32::Graphics::Gdi::{GetStockObject, BLACK_BRUSH};
                        SetClassLongPtrW(
                            child,
                            GCLP_HBRBACKGROUND,
                            GetStockObject(BLACK_BRUSH).0 as isize,
                        );

                        let _ = SetWindowPos(
                            child,
                            HWND::default(),
                            0,
                            0,
                            d.w,
                            d.h,
                            SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                        );
                        BOOL(1)
                    }
                    let _ = EnumChildWindows(
                        wv_h,
                        Some(enum_fix_children),
                        LPARAM(&fd as *const _ as isize),
                    );

                    // Set WebView2 bounds once after all child fixes
                    let dh = mouse_hook::get_dispatch_hwnd();
                    if dh != 0 {
                        let _ = PostMessageW(
                            HWND(dh as *mut _),
                            mouse_hook::WM_MWP_SETBOUNDS_PUB,
                            WPARAM(w as usize),
                            LPARAM(h as isize),
                        );
                    }
                }
                found = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !found {
            error!("[window_layer] Timed out waiting for composition controller (1s)");
        }
    });

    mouse_hook::start_hook_thread();

    // Zombie window watchdog: re-detects desktop if parent HWND becomes stale
    WATCHDOG_PARENT.store(detection.target_parent.0 as isize, Ordering::SeqCst);
    let watchdog_our = our_hwnd.0 as isize;
    std::thread::spawn(move || {
        use std::time::Duration;
        use windows::Win32::UI::WindowsAndMessaging::IsWindow;
        loop {
            std::thread::sleep(Duration::from_secs(5));
            let parent_raw = WATCHDOG_PARENT.load(Ordering::SeqCst);
            if parent_raw == 0 {
                continue;
            }
            unsafe {
                if !IsWindow(HWND(parent_raw as *mut _)).as_bool() {
                    info!("[watchdog] Parent HWND stale, re-detecting desktop...");
                    // Invalidate cached explorer handle (PID may have changed)
                    mouse_hook::invalidate_proc_cache_pub();
                    match detect_desktop() {
                        Ok(d) => {
                            mouse_hook::set_target_parent_hwnd(d.target_parent.0 as isize);
                            mouse_hook::set_progman_hwnd(d.progman.0 as isize);
                            mouse_hook::set_explorer_pid(d.explorer_pid);
                            if !d.syslistview.is_invalid() {
                                mouse_hook::set_syslistview_hwnd(d.syslistview.0 as isize);
                            }
                            apply_injection(HWND(watchdog_our as *mut _), &d);
                            WATCHDOG_PARENT.store(d.target_parent.0 as isize, Ordering::SeqCst);
                            info!("[watchdog] Re-injection done");
                        }
                        Err(e) => error!("[watchdog] Re-detection failed: {}", e),
                    }
                }
            }
        }
    });

    Ok(())
}

// ==============================================================================
// Windows: Mouse & Keyboard Hooks
// ==============================================================================

#[cfg(target_os = "windows")]
pub mod mouse_hook {
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, AtomicU64, Ordering};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;

    const MOUSE_MOVE: i32 = 0x0200;
    const MOUSE_LDOWN: i32 = 0x0201;
    const MOUSE_LUP: i32 = 0x0202;
    const MOUSE_RDOWN: i32 = 0x0204;
    const MOUSE_RUP: i32 = 0x0205;
    const MOUSE_MDOWN: i32 = 0x0207;
    const MOUSE_MUP: i32 = 0x0208;
    const MOUSE_WHEEL: i32 = 0x020A;
    const MOUSE_HWHEEL: i32 = 0x020E;
    const MK_NONE: i32 = 0x0;
    const MK_LBUTTON: i32 = 0x0001;
    const MK_RBUTTON: i32 = 0x0002;
    const MK_MBUTTON: i32 = 0x0010;

    // ListView messages for cross-process icon manipulation
    const LVM_FIRST: u32 = 0x1000;
    const LVM_SETITEMPOSITION: u32 = LVM_FIRST + 15; // 0x100F
    const LVM_GETITEMPOSITION: u32 = LVM_FIRST + 16; // 0x1010
    const LVM_HITTEST: u32 = LVM_FIRST + 18; // 0x1012
    const LVM_GETITEMRECT: u32 = LVM_FIRST + 14; // 0x100E
    const LVM_SETHOTITEM: u32 = LVM_FIRST + 60; // 0x103C

    static WEBVIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static SYSLISTVIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static TARGET_PARENT_HWND: AtomicIsize = AtomicIsize::new(0);
    static PROGMAN_HWND: AtomicIsize = AtomicIsize::new(0);
    static EXPLORER_PID: AtomicU32 = AtomicU32::new(0);
    static DESKTOP_CORE_HWND: AtomicIsize = AtomicIsize::new(0);
    static COMP_CONTROLLER_PTR: AtomicIsize = AtomicIsize::new(0);
    static DRAG_VK: AtomicIsize = AtomicIsize::new(0);
    static DISPATCH_HWND: AtomicIsize = AtomicIsize::new(0);
    static CHROME_RWHH: AtomicIsize = AtomicIsize::new(0);

    // Cached values to avoid syscalls in hook hot path
    static OUR_PID: AtomicU32 = AtomicU32::new(0);
    static DBLCLICK_TIME: AtomicU32 = AtomicU32::new(0);
    static DBLCLICK_CX: AtomicI32 = AtomicI32::new(0);
    static DBLCLICK_CY: AtomicI32 = AtomicI32::new(0);
    static DRAG_THRESHOLD_CX: AtomicI32 = AtomicI32::new(4);
    static DRAG_THRESHOLD_CY: AtomicI32 = AtomicI32::new(4);
    // Left-click drag state (icon repositioning via LVM_SETITEMPOSITION)
    static NATIVE_DRAG: AtomicBool = AtomicBool::new(false);
    static DRAG_START_X: AtomicI32 = AtomicI32::new(0);
    static DRAG_START_Y: AtomicI32 = AtomicI32::new(0);
    static DRAG_ITEM_INDEX: AtomicI32 = AtomicI32::new(-1);
    static DRAG_OFFSET_X: AtomicI32 = AtomicI32::new(0);
    static DRAG_OFFSET_Y: AtomicI32 = AtomicI32::new(0);
    static DRAG_PAST_THRESHOLD: AtomicBool = AtomicBool::new(false);
    static DRAG_GHOST_HIML: AtomicIsize = AtomicIsize::new(0);
    // Right-click state (context menu — PostMessage doesn't trigger native WM_CONTEXTMENU)
    static RCLICK_ON_ICON: AtomicBool = AtomicBool::new(false);
    // Hover tracking — LVM_SETHOTITEM (PostMessage(WM_MOUSEMOVE) doesn't work
    // because ListView's hot-tracking checks real cursor pos via GetCursorPos)
    static CURRENT_HOT_ITEM: AtomicI32 = AtomicI32::new(-1);
    static LAST_HOVER_TICK: AtomicU64 = AtomicU64::new(0);
    // Cached explorer process handle + remote buffer for cross-process LVM ops.
    // Avoids OpenProcess/VirtualAllocEx/VirtualFreeEx/CloseHandle per call.
    static CACHED_PROC_HANDLE: AtomicIsize = AtomicIsize::new(0);
    static CACHED_PROC_PID: AtomicU32 = AtomicU32::new(0);
    static CACHED_REMOTE_BUF: AtomicIsize = AtomicIsize::new(0);
    const CACHED_BUF_SIZE: usize = 256; // enough for any LV struct

    const WM_APP: u32 = 0x8000;
    pub const WM_MWP_SETBOUNDS_PUB: u32 = WM_APP + 43;
    const WM_MWP_MOUSE: u32 = WM_APP + 42;

    pub fn set_webview_hwnd(h: isize) {
        WEBVIEW_HWND.store(h, Ordering::SeqCst);
    }
    pub fn set_syslistview_hwnd(h: isize) {
        SYSLISTVIEW_HWND.store(h, Ordering::SeqCst);
    }
    pub fn set_target_parent_hwnd(h: isize) {
        TARGET_PARENT_HWND.store(h, Ordering::SeqCst);
    }
    pub fn set_progman_hwnd(h: isize) {
        PROGMAN_HWND.store(h, Ordering::SeqCst);
    }
    pub fn set_explorer_pid(pid: u32) {
        EXPLORER_PID.store(pid, Ordering::SeqCst);
    }
    pub fn get_syslistview_hwnd() -> isize {
        SYSLISTVIEW_HWND.load(Ordering::SeqCst)
    }
    pub fn set_comp_controller_ptr(p: isize) {
        COMP_CONTROLLER_PTR.store(p, Ordering::SeqCst);
    }
    pub fn get_comp_controller_ptr() -> isize {
        COMP_CONTROLLER_PTR.load(Ordering::SeqCst)
    }
    pub fn get_dispatch_hwnd() -> isize {
        DISPATCH_HWND.load(Ordering::SeqCst)
    }
    pub fn get_chrome_rwhh_raw() -> isize {
        CHROME_RWHH.load(Ordering::SeqCst)
    }
    pub fn invalidate_proc_cache_pub() {
        unsafe { invalidate_proc_cache() }
    }

    pub fn unregister_session_notif() {
        let dh = DISPATCH_HWND.load(Ordering::SeqCst);
        if dh != 0 {
            unsafe {
                use windows::Win32::System::RemoteDesktop::WTSUnRegisterSessionNotification;
                let _ = WTSUnRegisterSessionNotification(HWND(dh as *mut _));
            }
        }
    }

    /// Pack two i32 screen/client coordinates into a Windows lParam isize.
    #[inline]
    fn make_lparam(x: i32, y: i32) -> isize {
        ((x as i16 as u16 as u32) | ((y as i16 as u16 as u32) << 16)) as isize
    }

    #[inline]
    unsafe fn post_mouse(kind: i32, vk: i32, data: u32, x: i32, y: i32) {
        // Encoding packs 3 fields into a single usize via bit shifts.
        // The <<32 shift requires a 64-bit pointer width; on 32-bit it would silently lose data.
        const _: () = assert!(
            std::mem::size_of::<usize>() >= 8,
            "mouse hook encoding requires 64-bit pointer width"
        );
        let dh = DISPATCH_HWND.load(Ordering::Relaxed);
        if dh == 0 {
            return;
        }
        let wp =
            WPARAM((kind as u16 as usize) | ((vk as u16 as usize) << 16) | ((data as usize) << 32));
        let lp = LPARAM(make_lparam(x, y));
        let _ = PostMessageW(HWND(dh as *mut _), WM_MWP_MOUSE, wp, lp);
    }

    const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
    const WTS_SESSION_LOCK: u32 = 0x7;
    const WTS_SESSION_UNLOCK: u32 = 0x8;
    const WM_DISPLAYCHANGE: u32 = 0x007E;
    const WM_SETTINGCHANGE: u32 = 0x001A;

    /// Reload double-click / drag thresholds from system settings.
    /// Called when WM_SETTINGCHANGE fires (user changed mouse prefs in Control Panel).
    unsafe fn refresh_mouse_metrics() {
        use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXDOUBLECLK, SM_CXDRAG, SM_CYDOUBLECLK, SM_CYDRAG,
        };
        DBLCLICK_TIME.store(GetDoubleClickTime(), Ordering::Relaxed);
        DBLCLICK_CX.store(GetSystemMetrics(SM_CXDOUBLECLK) / 2, Ordering::Relaxed);
        DBLCLICK_CY.store(GetSystemMetrics(SM_CYDOUBLECLK) / 2, Ordering::Relaxed);
        DRAG_THRESHOLD_CX.store(GetSystemMetrics(SM_CXDRAG), Ordering::Relaxed);
        DRAG_THRESHOLD_CY.store(GetSystemMetrics(SM_CYDRAG), Ordering::Relaxed);
        log::info!("[settings] Mouse metrics refreshed");
    }

    /// Resize the WebView and update WebView2 controller bounds after a display change.
    /// Called when WM_DISPLAYCHANGE fires (monitor plug/unplug or resolution change).
    unsafe fn on_display_change() {
        use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
        use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
        use windows::Win32::UI::WindowsAndMessaging::{
            EnumChildWindows, SetWindowPos, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOZORDER,
        };

        // Compute virtual desktop bounds across all monitors
        struct Bounds {
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
        }
        let mut b = Bounds {
            left: i32::MAX,
            top: i32::MAX,
            right: i32::MIN,
            bottom: i32::MIN,
        };
        unsafe extern "system" fn mon_cb(
            _hm: HMONITOR,
            _hdc: HDC,
            rect: *mut RECT,
            lp: LPARAM,
        ) -> BOOL {
            if lp.0 != 0 && !rect.is_null() {
                let r = rect.read();
                let b = &mut *(lp.0 as *mut Bounds);
                b.left = b.left.min(r.left);
                b.top = b.top.min(r.top);
                b.right = b.right.max(r.right);
                b.bottom = b.bottom.max(r.bottom);
            }
            BOOL(1)
        }
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(mon_cb),
            LPARAM(&mut b as *mut _ as isize),
        );

        let w = b.right - b.left;
        let h = b.bottom - b.top;
        if w <= 0 || h <= 0 {
            return;
        }
        log::info!("[display] Display changed: virtual screen {}x{}", w, h);

        // Resize WebView HWND and all its children
        let wv = WEBVIEW_HWND.load(Ordering::Relaxed);
        if wv != 0 {
            let wv_h = HWND(wv as *mut _);
            let _ = SetWindowPos(
                wv_h,
                HWND::default(),
                0,
                0,
                w,
                h,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
            struct S {
                w: i32,
                h: i32,
            }
            let s = S { w, h };
            unsafe extern "system" fn resize_child(child: HWND, lp: LPARAM) -> BOOL {
                if lp.0 != 0 {
                    let s = &*(lp.0 as *const S);
                    let _ = SetWindowPos(
                        child,
                        HWND::default(),
                        0,
                        0,
                        s.w,
                        s.h,
                        SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                    );
                }
                BOOL(1)
            }
            let _ = EnumChildWindows(wv_h, Some(resize_child), LPARAM(&s as *const _ as isize));
        }

        // Update WebView2 composition controller bounds
        let ptr = get_comp_controller_ptr();
        if ptr != 0 {
            let _ = wry::set_controller_bounds_raw(ptr, w, h);
        }
    }

    unsafe extern "system" fn dispatch_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wp: WPARAM,
        lp: LPARAM,
    ) -> LRESULT {
        if msg == WM_MWP_SETBOUNDS_PUB {
            let ptr = get_comp_controller_ptr();
            if ptr != 0 {
                let _ = wry::set_controller_bounds_raw(ptr, wp.0 as i32, lp.0 as i32);
            }
            return LRESULT(0);
        }
        if msg == WM_MWP_MOUSE {
            let ptr = get_comp_controller_ptr();
            if ptr != 0 {
                let kind = (wp.0 & 0xFFFF) as i32;
                let vk = ((wp.0 >> 16) & 0xFFFF) as i32;
                let data = ((wp.0 >> 32) & 0xFFFFFFFF) as u32;
                let x = (lp.0 & 0xFFFF) as i16 as i32;
                let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;

                // Sync cursor position before click-down events
                if kind == MOUSE_LDOWN || kind == MOUSE_RDOWN || kind == MOUSE_MDOWN {
                    let _ = wry::send_mouse_input_raw(ptr, MOUSE_MOVE, vk, 0, x, y);
                }
                let _ = wry::send_mouse_input_raw(ptr, kind, vk, data, x, y);
            }
            return LRESULT(0);
        }
        // WTS session lock/unlock notifications
        if msg == WM_WTSSESSION_CHANGE {
            match wp.0 as u32 {
                WTS_SESSION_LOCK => {
                    crate::window_layer::IS_SESSION_ACTIVE.store(false, Ordering::SeqCst);
                    log::info!("[session] Screen locked, hook paused");
                }
                WTS_SESSION_UNLOCK => {
                    crate::window_layer::IS_SESSION_ACTIVE.store(true, Ordering::SeqCst);
                    log::info!("[session] Screen unlocked, hook resumed");
                }
                _ => {}
            }
            return LRESULT(0);
        }

        // Monitor plug/unplug or resolution change → resize WebView to new virtual desktop
        if msg == WM_DISPLAYCHANGE {
            on_display_change();
            return LRESULT(0);
        }

        // User changed mouse settings in Control Panel → refresh cached metrics
        if msg == WM_SETTINGCHANGE {
            refresh_mouse_metrics();
            return LRESULT(0);
        }

        DefWindowProcW(hwnd, msg, wp, lp)
    }

    pub fn init_dispatch_window() {
        unsafe {
            let cls = windows::core::w!("MWP_MouseDispatch");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(dispatch_wnd_proc),
                lpszClassName: cls,
                ..Default::default()
            };
            let _ = RegisterClassW(&wc);
            if let Ok(h) = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                cls,
                windows::core::w!(""),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                None,
                None,
            ) {
                DISPATCH_HWND.store(h.0 as isize, Ordering::SeqCst);

                // Register for session lock/unlock notifications
                use windows::Win32::System::RemoteDesktop::WTSRegisterSessionNotification;
                const NOTIFY_FOR_THIS_SESSION: u32 = 0;
                let _ = WTSRegisterSessionNotification(h, NOTIFY_FOR_THIS_SESSION);
            }
        }
    }

    #[inline]
    unsafe fn get_parent_process_id(pid: u32) -> Option<u32> {
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };

        struct SnapGuard(windows::Win32::Foundation::HANDLE);
        impl Drop for SnapGuard {
            fn drop(&mut self) {
                unsafe {
                    let _ = windows::Win32::Foundation::CloseHandle(self.0);
                }
            }
        }

        let snap = SnapGuard(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?);
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap.0, &mut entry).is_ok() {
            loop {
                if entry.th32ProcessID == pid {
                    return Some(entry.th32ParentProcessID);
                }
                if Process32NextW(snap.0, &mut entry).is_err() {
                    break;
                }
            }
        }
        None
    }

    #[inline]
    unsafe fn is_over_desktop(hwnd_under: HWND) -> bool {
        let tp = HWND(TARGET_PARENT_HWND.load(Ordering::Relaxed) as *mut _);
        let rwhh = HWND(CHROME_RWHH.load(Ordering::Relaxed) as *mut _);
        let wv = HWND(WEBVIEW_HWND.load(Ordering::Relaxed) as *mut _);
        let pm = HWND(PROGMAN_HWND.load(Ordering::Relaxed) as *mut _);
        let dc = HWND(DESKTOP_CORE_HWND.load(Ordering::Relaxed) as *mut _);
        let slv = HWND(SYSLISTVIEW_HWND.load(Ordering::Relaxed) as *mut _);

        // Fast path: known HWNDs (includes cached desktop CoreWindow + SysListView32)
        if !rwhh.is_invalid() && hwnd_under == rwhh {
            return true;
        }
        if !slv.is_invalid() && hwnd_under == slv {
            return true;
        }
        if !dc.is_invalid() && hwnd_under == dc {
            return true;
        }
        if hwnd_under == tp || hwnd_under == wv || hwnd_under == pm {
            return true;
        }
        if pm.0 as isize != 0 && IsChild(pm, hwnd_under).as_bool() {
            return true;
        }
        // Also check if hwnd_under is a child of the target parent (WorkerW)
        if tp.0 as isize != 0 && IsChild(tp, hwnd_under).as_bool() {
            return true;
        }

        // Slow path: zero-allocation class name checks
        // Win11: XamlExplorerHostIslandWindow is an invisible XAML overlay owned by explorer
        if super::is_class_name(hwnd_under, "XamlExplorerHostIslandWindow") {
            let exp_pid = EXPLORER_PID.load(Ordering::Relaxed);
            if exp_pid != 0 {
                let mut pid: u32 = 0;
                GetWindowThreadProcessId(hwnd_under, Some(&mut pid));
                if pid == exp_pid {
                    return true;
                }
            }
        }
        if super::is_class_name(hwnd_under, "Windows.UI.Core.CoreWindow") {
            let exp_pid = EXPLORER_PID.load(Ordering::Relaxed);
            if exp_pid != 0 {
                let mut pid: u32 = 0;
                GetWindowThreadProcessId(hwnd_under, Some(&mut pid));
                if pid == exp_pid {
                    DESKTOP_CORE_HWND.store(hwnd_under.0 as isize, Ordering::Relaxed);
                    return true;
                }
            }
        }

        // Auto-discover Chrome_RWHH via process tree validation
        if rwhh.is_invalid()
            && !wv.is_invalid()
            && super::is_class_name(hwnd_under, "Chrome_RenderWidgetHostHWND")
        {
            let direct_parent = GetParent(hwnd_under).unwrap_or_default();
            if !direct_parent.is_invalid() {
                let mut browser_pid: u32 = 0;
                GetWindowThreadProcessId(direct_parent, Some(&mut browser_pid));
                let our_pid = OUR_PID.load(Ordering::Relaxed);

                let is_ours = browser_pid == our_pid
                    || get_parent_process_id(browser_pid).is_some_and(|ppid| ppid == our_pid);

                if is_ours {
                    log::info!(
                        "[hook] Chrome_RWHH discovered: 0x{:X}",
                        hwnd_under.0 as isize
                    );
                    CHROME_RWHH.store(hwnd_under.0 as isize, Ordering::Relaxed);
                    return true;
                }
            }
        }
        false
    }

    /// Ensure we have a cached process handle + remote buffer for the given PID.
    /// Returns (HANDLE, remote_ptr) or None on failure.
    /// The cache is invalidated when PID changes (explorer restart).
    unsafe fn ensure_cached_proc(
        pid: u32,
    ) -> Option<(windows::Win32::Foundation::HANDLE, *mut std::ffi::c_void)> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Memory::{
            VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE,
        };
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
        };

        let cached_pid = CACHED_PROC_PID.load(Ordering::Relaxed);
        if cached_pid == pid && cached_pid != 0 {
            let h = CACHED_PROC_HANDLE.load(Ordering::Relaxed);
            let buf = CACHED_REMOTE_BUF.load(Ordering::Relaxed);
            if h != 0 && buf != 0 {
                return Some((
                    windows::Win32::Foundation::HANDLE(h as *mut _),
                    buf as *mut std::ffi::c_void,
                ));
            }
        }

        // Invalidate old cache
        invalidate_proc_cache();

        let proc = OpenProcess(
            PROCESS_VM_OPERATION | PROCESS_VM_READ | PROCESS_VM_WRITE,
            false,
            pid,
        )
        .ok()?;

        let remote = VirtualAllocEx(
            proc,
            None,
            CACHED_BUF_SIZE,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        if remote.is_null() {
            let _ = CloseHandle(proc);
            return None;
        }

        CACHED_PROC_HANDLE.store(proc.0 as isize, Ordering::Relaxed);
        CACHED_REMOTE_BUF.store(remote as isize, Ordering::Relaxed);
        CACHED_PROC_PID.store(pid, Ordering::Relaxed);

        Some((proc, remote))
    }

    /// Invalidate the cached explorer process handle (called on explorer restart).
    unsafe fn invalidate_proc_cache() {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Memory::{VirtualFreeEx, MEM_RELEASE};

        let old_h = CACHED_PROC_HANDLE.swap(0, Ordering::Relaxed);
        let old_buf = CACHED_REMOTE_BUF.swap(0, Ordering::Relaxed);
        CACHED_PROC_PID.store(0, Ordering::Relaxed);
        if old_h != 0 {
            let handle = windows::Win32::Foundation::HANDLE(old_h as *mut _);
            if old_buf != 0 {
                let _ = VirtualFreeEx(handle, old_buf as *mut _, 0, MEM_RELEASE);
            }
            let _ = CloseHandle(handle);
        }
    }

    /// Execute a cross-process ListView message using a cached process handle + remote buffer.
    /// Writes `input` to explorer's address space, sends the message, reads `output`.
    /// Returns the SendMessage result (0 on failure).
    unsafe fn cross_process_lvm_send<T>(
        slv: HWND,
        msg: u32,
        wparam: WPARAM,
        input: &T,
        output: &mut T,
    ) -> usize {
        use windows::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};

        let size = std::mem::size_of::<T>();
        debug_assert!(size <= CACHED_BUF_SIZE);

        let mut pid = 0u32;
        GetWindowThreadProcessId(slv, Some(&mut pid));
        if pid == 0 {
            return 0;
        }

        let (proc, remote) = match ensure_cached_proc(pid) {
            Some(x) => x,
            None => return 0,
        };

        if WriteProcessMemory(proc, remote, input as *const T as _, size, None).is_err() {
            // Handle may be stale (explorer restarted) — invalidate and bail
            invalidate_proc_cache();
            return 0;
        }

        let mut result: usize = 0;
        let _ = SendMessageTimeoutW(
            slv,
            msg,
            wparam,
            LPARAM(remote as isize),
            SMTO_ABORTIFHUNG,
            100,
            Some(&mut result),
        );

        let _ = ReadProcessMemory(proc, remote, output as *mut T as _, size, None);

        result
    }

    /// Returns the item index under screen_pt (-1 if no item).
    unsafe fn get_hit_item_index(slv: HWND, screen_pt: &windows::Win32::Foundation::POINT) -> i32 {
        use windows::Win32::Graphics::Gdi::ScreenToClient;

        let mut pt = *screen_pt;
        let _ = ScreenToClient(slv, &mut pt);

        #[repr(C)]
        struct LVHITTESTINFO {
            pt: windows::Win32::Foundation::POINT,
            flags: u32,
            i_item: i32,
            i_sub_item: i32,
            i_group: i32,
        }

        let input = LVHITTESTINFO {
            pt,
            flags: 0,
            i_item: -1,
            i_sub_item: 0,
            i_group: 0,
        };
        let mut output = LVHITTESTINFO {
            pt: windows::Win32::Foundation::POINT { x: 0, y: 0 },
            flags: 0,
            i_item: -1,
            i_sub_item: 0,
            i_group: 0,
        };

        cross_process_lvm_send(slv, LVM_HITTEST, WPARAM(0), &input, &mut output);
        output.i_item
    }

    /// Get icon position (client coords) via LVM_GETITEMPOSITION.
    unsafe fn get_item_position(
        slv: HWND,
        item_index: i32,
    ) -> Option<windows::Win32::Foundation::POINT> {
        let input = windows::Win32::Foundation::POINT { x: 0, y: 0 };
        let mut output = windows::Win32::Foundation::POINT { x: 0, y: 0 };

        let result = cross_process_lvm_send(
            slv,
            LVM_GETITEMPOSITION,
            WPARAM(item_index as usize),
            &input,
            &mut output,
        );

        (result != 0).then_some(output)
    }

    /// Get item bounding rect (client coords) via LVM_GETITEMRECT.
    unsafe fn get_item_rect(
        slv: HWND,
        item_index: i32,
    ) -> Option<windows::Win32::Foundation::RECT> {
        use windows::Win32::Foundation::RECT;
        // lParam = RECT*, rect.left = part type (LVIR_BOUNDS = 0)
        let input = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let mut output = input;
        let result = cross_process_lvm_send(
            slv,
            LVM_GETITEMRECT,
            WPARAM(item_index as usize),
            &input,
            &mut output,
        );
        (result != 0).then_some(output)
    }

    /// Begin ImageList ghost drag: capture icon area from screen, show as drag overlay.
    unsafe fn start_drag_ghost(
        slv: HWND,
        item_idx: i32,
        cursor: &windows::Win32::Foundation::POINT,
    ) {
        use windows::Win32::Graphics::Gdi::{
            BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC,
            DeleteObject, GetDC, ReleaseDC, SelectObject, HBITMAP, SRCCOPY,
        };
        use windows::Win32::UI::Controls::{
            ImageList_Add, ImageList_BeginDrag, ImageList_Create, ImageList_DragEnter, ILC_COLOR32,
        };
        use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

        let item_rect = match get_item_rect(slv, item_idx) {
            Some(r) => r,
            None => return,
        };
        let w = item_rect.right - item_rect.left;
        let h = item_rect.bottom - item_rect.top;
        if w <= 0 || h <= 0 {
            return;
        }

        // Convert item rect top-left to screen coords
        let mut tl = windows::Win32::Foundation::POINT {
            x: item_rect.left,
            y: item_rect.top,
        };
        let _ = ClientToScreen(slv, &mut tl);

        // Capture icon area from screen DC
        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return;
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        let bmp = CreateCompatibleBitmap(screen_dc, w, h);
        let old = SelectObject(mem_dc, bmp);
        let _ = BitBlt(mem_dc, 0, 0, w, h, screen_dc, tl.x, tl.y, SRCCOPY);
        let _ = SelectObject(mem_dc, old);
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);

        // Create ImageList with captured bitmap
        let himl = ImageList_Create(w, h, ILC_COLOR32, 1, 0);
        if himl.is_invalid() {
            let _ = DeleteObject(bmp);
            return;
        }
        ImageList_Add(himl, bmp, HBITMAP::default());
        let _ = DeleteObject(bmp);

        // Hotspot = cursor offset within captured image
        let hotspot_x = cursor.x - tl.x;
        let hotspot_y = cursor.y - tl.y;

        let _ = ImageList_BeginDrag(himl, 0, hotspot_x, hotspot_y);
        let _ = ImageList_DragEnter(GetDesktopWindow(), cursor.x, cursor.y);
        DRAG_GHOST_HIML.store(himl.0 as isize, Ordering::Relaxed);
        log::debug!(
            "[hook] Ghost drag started: {}x{} hotspot({},{})",
            w,
            h,
            hotspot_x,
            hotspot_y
        );
    }

    /// Move the ghost drag image to follow the cursor.
    #[inline]
    unsafe fn move_drag_ghost(cursor: &windows::Win32::Foundation::POINT) {
        let _ = windows::Win32::UI::Controls::ImageList_DragMove(cursor.x, cursor.y);
    }

    /// End the ghost drag: cleanup ImageList resources.
    unsafe fn end_drag_ghost() {
        use windows::Win32::UI::Controls::{
            ImageList_Destroy, ImageList_DragLeave, ImageList_EndDrag, HIMAGELIST,
        };
        use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

        let _ = ImageList_DragLeave(GetDesktopWindow());
        ImageList_EndDrag();

        let himl_raw = DRAG_GHOST_HIML.swap(0, Ordering::Relaxed);
        if himl_raw != 0 {
            let himl = HIMAGELIST(himl_raw);
            let _ = ImageList_Destroy(himl);
        }
    }

    #[inline]
    unsafe fn forward(msg: u32, info_hook: &MSLLHOOKSTRUCT, cx: i32, cy: i32) {
        match msg {
            WM_MOUSEMOVE => post_mouse(
                MOUSE_MOVE,
                DRAG_VK.load(Ordering::Relaxed) as i32,
                0,
                cx,
                cy,
            ),
            WM_LBUTTONDOWN => {
                DRAG_VK.store(MK_LBUTTON as isize, Ordering::Relaxed);
                post_mouse(MOUSE_LDOWN, MK_LBUTTON, 0, cx, cy);
            }
            WM_LBUTTONUP => {
                DRAG_VK.store(0, Ordering::Relaxed);
                post_mouse(MOUSE_LUP, MK_NONE, 0, cx, cy);
            }
            WM_RBUTTONDOWN => {
                DRAG_VK.store(MK_RBUTTON as isize, Ordering::Relaxed);
                post_mouse(MOUSE_RDOWN, MK_RBUTTON, 0, cx, cy);
            }
            WM_RBUTTONUP => {
                DRAG_VK.store(0, Ordering::Relaxed);
                post_mouse(MOUSE_RUP, MK_NONE, 0, cx, cy);
            }
            WM_MBUTTONDOWN => {
                DRAG_VK.store(MK_MBUTTON as isize, Ordering::Relaxed);
                post_mouse(MOUSE_MDOWN, MK_MBUTTON, 0, cx, cy);
            }
            WM_MBUTTONUP => {
                DRAG_VK.store(0, Ordering::Relaxed);
                post_mouse(MOUSE_MUP, MK_NONE, 0, cx, cy);
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let kind = if msg == WM_MOUSEWHEEL {
                    MOUSE_WHEEL
                } else {
                    MOUSE_HWHEEL
                };
                post_mouse(
                    kind,
                    MK_NONE,
                    (info_hook.mouseData >> 16) as i16 as i32 as u32,
                    cx,
                    cy,
                );
            }
            _ => {}
        }
    }

    pub fn start_hook_thread() {
        std::thread::spawn(|| {
            unsafe {
                use windows::Win32::System::Com::{
                    CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
                };
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                // RAII guard: CoUninitialize when the hook thread exits
                struct ComGuard;
                impl Drop for ComGuard {
                    fn drop(&mut self) {
                        unsafe { CoUninitialize() }
                    }
                }
                let _com_guard = ComGuard;

                // Cache process ID + double-click metrics once at hook startup
                OUR_PID.store(std::process::id(), Ordering::Relaxed);
                use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
                DBLCLICK_TIME.store(GetDoubleClickTime(), Ordering::Relaxed);
                DBLCLICK_CX.store(GetSystemMetrics(SM_CXDOUBLECLK) / 2, Ordering::Relaxed);
                DBLCLICK_CY.store(GetSystemMetrics(SM_CYDOUBLECLK) / 2, Ordering::Relaxed);
                DRAG_THRESHOLD_CX.store(GetSystemMetrics(SM_CXDRAG), Ordering::Relaxed);
                DRAG_THRESHOLD_CY.store(GetSystemMetrics(SM_CYDRAG), Ordering::Relaxed);
            }

            /// Post a mouse event to SysListView32, with double-click synthesis and key state.
            unsafe fn post_to_slv(slv: HWND, msg: u32, info_hook: &MSLLHOOKSTRUCT) {
                use windows::Win32::Graphics::Gdi::ScreenToClient;

                if !IsWindowVisible(slv).as_bool() {
                    return;
                }

                let lp = if msg == WM_MOUSEWHEEL || msg == WM_MOUSEHWHEEL {
                    make_lparam(info_hook.pt.x, info_hook.pt.y)
                } else {
                    let mut slv_cp = info_hook.pt;
                    let _ = ScreenToClient(slv, &mut slv_cp);
                    make_lparam(slv_cp.x, slv_cp.y)
                };

                // Synthesize double-click (PostMessage bypasses native detection)
                let mut out_msg = msg;
                if msg == WM_LBUTTONDOWN {
                    static LAST_DOWN_TIME: AtomicU32 = AtomicU32::new(0);
                    static LAST_DOWN_X: AtomicI32 = AtomicI32::new(0);
                    static LAST_DOWN_Y: AtomicI32 = AtomicI32::new(0);

                    let now = info_hook.time;
                    let dt = now.saturating_sub(LAST_DOWN_TIME.load(Ordering::Relaxed));
                    let dx = (info_hook.pt.x - LAST_DOWN_X.load(Ordering::Relaxed)).abs();
                    let dy = (info_hook.pt.y - LAST_DOWN_Y.load(Ordering::Relaxed)).abs();

                    if dt > 0
                        && dt <= DBLCLICK_TIME.load(Ordering::Relaxed)
                        && dx <= DBLCLICK_CX.load(Ordering::Relaxed)
                        && dy <= DBLCLICK_CY.load(Ordering::Relaxed)
                    {
                        out_msg = WM_LBUTTONDBLCLK;
                        LAST_DOWN_TIME.store(0, Ordering::Relaxed);
                    } else {
                        LAST_DOWN_TIME.store(now, Ordering::Relaxed);
                        LAST_DOWN_X.store(info_hook.pt.x, Ordering::Relaxed);
                        LAST_DOWN_Y.store(info_hook.pt.y, Ordering::Relaxed);
                    }
                }

                let slv_wparam = {
                    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
                    let mut mk: u16 = 0;
                    if out_msg == WM_LBUTTONDOWN
                        || out_msg == WM_LBUTTONDBLCLK
                        || GetAsyncKeyState(0x01) < 0
                    {
                        mk |= 0x0001; // MK_LBUTTON
                    }
                    if out_msg == WM_RBUTTONDOWN || GetAsyncKeyState(0x02) < 0 {
                        mk |= 0x0002; // MK_RBUTTON
                    }
                    if out_msg == WM_MBUTTONDOWN || GetAsyncKeyState(0x04) < 0 {
                        mk |= 0x0010; // MK_MBUTTON
                    }
                    if GetAsyncKeyState(0x10) < 0 {
                        mk |= 0x0004; // MK_SHIFT
                    }
                    if GetAsyncKeyState(0x11) < 0 {
                        mk |= 0x0008; // MK_CONTROL
                    }
                    if out_msg == WM_MOUSEWHEEL || out_msg == WM_MOUSEHWHEEL {
                        let delta = (info_hook.mouseData >> 16) as u16;
                        WPARAM(((delta as usize) << 16) | mk as usize)
                    } else {
                        WPARAM(mk as usize)
                    }
                };
                let _ = PostMessageW(slv, out_msg, slv_wparam, LPARAM(lp));
            }

            unsafe extern "system" fn hook_proc(
                code: i32,
                wparam: WPARAM,
                lparam: LPARAM,
            ) -> LRESULT {
                let hook_h = HHOOK(
                    crate::window_layer::HOOK_HANDLE_GLOBAL.load(Ordering::Relaxed) as *mut _,
                );
                let wv_raw = WEBVIEW_HWND.load(Ordering::Relaxed);

                if code < 0 || wv_raw == 0 {
                    return CallNextHookEx(hook_h, code, wparam, lparam);
                }

                if !crate::window_layer::IS_SESSION_ACTIVE.load(Ordering::Relaxed) {
                    return CallNextHookEx(hook_h, code, wparam, lparam);
                }

                let info_hook = *(lparam.0 as *const MSLLHOOKSTRUCT);
                let hwnd_under = WindowFromPoint(info_hook.pt);
                let msg = wparam.0 as u32;
                let slv_raw = SYSLISTVIEW_HWND.load(Ordering::Relaxed);
                use windows::Win32::Graphics::Gdi::ScreenToClient;

                // ── Right-click on icon: context menu ──
                // Native right-click fails because shell hit-tests via GetCursorPos
                // and sees Chrome_RWHH. Instead: simulate a quick left-click to
                // natively select the item, then send WM_CONTEXTMENU.
                if RCLICK_ON_ICON.load(Ordering::Relaxed) {
                    if msg == WM_RBUTTONUP {
                        RCLICK_ON_ICON.store(false, Ordering::Relaxed);
                        if slv_raw != 0 {
                            let slv_h = HWND(slv_raw as *mut _);
                            // Quick left-click to natively select the icon
                            // (LVM_SETITEMSTATE alone doesn't set all internal shell state)
                            let mut slv_cp = info_hook.pt;
                            let _ = ScreenToClient(slv_h, &mut slv_cp);
                            let lp = make_lparam(slv_cp.x, slv_cp.y);
                            let _ = PostMessageW(
                                slv_h,
                                WM_LBUTTONDOWN,
                                WPARAM(MK_LBUTTON as usize),
                                LPARAM(lp),
                            );
                            let _ = PostMessageW(slv_h, WM_LBUTTONUP, WPARAM(0), LPARAM(lp));
                            // Context menu at actual cursor position
                            let _ = PostMessageW(
                                slv_h,
                                WM_CONTEXTMENU,
                                WPARAM(slv_h.0 as usize),
                                LPARAM(make_lparam(info_hook.pt.x, info_hook.pt.y)),
                            );
                        }
                        return LRESULT(1);
                    } else if msg == WM_MOUSEMOVE {
                        return LRESULT(1);
                    } else {
                        RCLICK_ON_ICON.store(false, Ordering::Relaxed);
                    }
                }

                // ── Left-click drag (icon repositioning with ghost image) ──
                // Ghost follows cursor via ImageList drag APIs. Only a single
                // LVM_SETITEMPOSITION fires at drop time (no grid-jumping).
                if NATIVE_DRAG.load(Ordering::Relaxed) {
                    if msg == WM_LBUTTONUP {
                        NATIVE_DRAG.store(false, Ordering::Relaxed);
                        let was_dragging = DRAG_PAST_THRESHOLD.swap(false, Ordering::Relaxed);

                        if was_dragging {
                            end_drag_ghost();
                            let item_idx = DRAG_ITEM_INDEX.load(Ordering::Relaxed);
                            if slv_raw != 0 && item_idx >= 0 {
                                let slv_h = HWND(slv_raw as *mut _);
                                let mut drop_pt = info_hook.pt;
                                let _ = ScreenToClient(slv_h, &mut drop_pt);
                                let off_x = DRAG_OFFSET_X.load(Ordering::Relaxed);
                                let off_y = DRAG_OFFSET_Y.load(Ordering::Relaxed);
                                drop_pt.x -= off_x;
                                drop_pt.y -= off_y;
                                let lp = make_lparam(drop_pt.x, drop_pt.y);
                                let _ = PostMessageW(
                                    slv_h,
                                    LVM_SETITEMPOSITION,
                                    WPARAM(item_idx as usize),
                                    LPARAM(lp),
                                );
                                log::debug!(
                                    "[hook] DRAG complete: item={} to client({},{})",
                                    item_idx,
                                    drop_pt.x,
                                    drop_pt.y
                                );
                            }
                        } else {
                            // Simple click (no drag) → forward button-up
                            if slv_raw != 0 {
                                post_to_slv(HWND(slv_raw as *mut _), msg, &info_hook);
                            }
                        }
                    } else if msg == WM_MOUSEMOVE {
                        let start_x = DRAG_START_X.load(Ordering::Relaxed);
                        let start_y = DRAG_START_Y.load(Ordering::Relaxed);
                        let dx = (info_hook.pt.x - start_x).abs();
                        let dy = (info_hook.pt.y - start_y).abs();
                        let drag_cx = DRAG_THRESHOLD_CX.load(Ordering::Relaxed);
                        let drag_cy = DRAG_THRESHOLD_CY.load(Ordering::Relaxed);
                        if dx > drag_cx || dy > drag_cy {
                            if !DRAG_PAST_THRESHOLD.load(Ordering::Relaxed) {
                                // First move past threshold → start ghost overlay
                                DRAG_PAST_THRESHOLD.store(true, Ordering::Relaxed);
                                if slv_raw != 0 {
                                    let item_idx = DRAG_ITEM_INDEX.load(Ordering::Relaxed);
                                    if item_idx >= 0 {
                                        start_drag_ghost(
                                            HWND(slv_raw as *mut _),
                                            item_idx,
                                            &info_hook.pt,
                                        );
                                    }
                                }
                            } else {
                                // Subsequent moves → update ghost position
                                move_drag_ghost(&info_hook.pt);
                            }
                        }
                    } else {
                        // Other button events during drag → post
                        if slv_raw != 0 {
                            post_to_slv(HWND(slv_raw as *mut _), msg, &info_hook);
                        }
                    }
                    return CallNextHookEx(hook_h, code, wparam, lparam);
                }

                // ── Not over desktop: pass through ──
                if !is_over_desktop(hwnd_under) {
                    return CallNextHookEx(hook_h, code, wparam, lparam);
                }

                // ── Interface mode: PostMessage direct à Chrome_RWHH ──
                if crate::window_layer::INTERFACE_MODE.load(Ordering::Relaxed) {
                    let rwhh = CHROME_RWHH.load(Ordering::Relaxed);
                    if rwhh != 0 {
                        let rwhh_hwnd = HWND(rwhh as *mut _);
                        match msg {
                            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                                let lp = make_lparam(info_hook.pt.x, info_hook.pt.y);
                                let delta = (info_hook.mouseData >> 16) as i16 as u16;
                                let wp = (delta as usize) << 16;
                                let _ = PostMessageW(rwhh_hwnd, msg, WPARAM(wp), LPARAM(lp));
                            }
                            _ => {
                                let mut cp = info_hook.pt;
                                let _ = ScreenToClient(rwhh_hwnd, &mut cp);
                                let lp = make_lparam(cp.x, cp.y);
                                // Include real modifier / button state so Shift+click,
                                // Ctrl+click and drag-move work correctly in the WebView.
                                use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
                                let mut mk: usize = 0;
                                if msg == WM_LBUTTONDOWN || GetAsyncKeyState(0x01) < 0 {
                                    mk |= 0x0001; // MK_LBUTTON
                                }
                                if msg == WM_RBUTTONDOWN || GetAsyncKeyState(0x02) < 0 {
                                    mk |= 0x0002; // MK_RBUTTON
                                }
                                if msg == WM_MBUTTONDOWN || GetAsyncKeyState(0x04) < 0 {
                                    mk |= 0x0010; // MK_MBUTTON
                                }
                                if GetAsyncKeyState(0x10) < 0 {
                                    mk |= 0x0004; // MK_SHIFT
                                }
                                if GetAsyncKeyState(0x11) < 0 {
                                    mk |= 0x0008; // MK_CONTROL
                                }
                                let _ = PostMessageW(rwhh_hwnd, msg, WPARAM(mk), LPARAM(lp));
                            }
                        }
                    }
                    return CallNextHookEx(hook_h, code, wparam, lparam);
                }

                // ── Wallpaper mode: button-down on icon ──
                // Single get_hit_item_index call (avoids duplicate cross-process op)
                if (msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN) && slv_raw != 0 {
                    let slv_h = HWND(slv_raw as *mut _);
                    let item_idx = get_hit_item_index(slv_h, &info_hook.pt);
                    if item_idx >= 0 {
                        if msg == WM_LBUTTONDOWN {
                            // Left-click: initiate drag tracking
                            NATIVE_DRAG.store(true, Ordering::Relaxed);
                            DRAG_PAST_THRESHOLD.store(false, Ordering::Relaxed);
                            DRAG_START_X.store(info_hook.pt.x, Ordering::Relaxed);
                            DRAG_START_Y.store(info_hook.pt.y, Ordering::Relaxed);
                            DRAG_ITEM_INDEX.store(item_idx, Ordering::Relaxed);
                            if let Some(icon_pos) = get_item_position(slv_h, item_idx) {
                                let mut cursor_client = info_hook.pt;
                                let _ = ScreenToClient(slv_h, &mut cursor_client);
                                DRAG_OFFSET_X
                                    .store(cursor_client.x - icon_pos.x, Ordering::Relaxed);
                                DRAG_OFFSET_Y
                                    .store(cursor_client.y - icon_pos.y, Ordering::Relaxed);
                            }
                            log::debug!(
                                "[hook] NATIVE_DRAG start at ({},{}) item={} offset=({},{})",
                                info_hook.pt.x,
                                info_hook.pt.y,
                                item_idx,
                                DRAG_OFFSET_X.load(Ordering::Relaxed),
                                DRAG_OFFSET_Y.load(Ordering::Relaxed),
                            );
                            post_to_slv(slv_h, msg, &info_hook);
                            return CallNextHookEx(hook_h, code, wparam, lparam);
                        } else {
                            // Right-click: track for context menu
                            RCLICK_ON_ICON.store(true, Ordering::Relaxed);
                            log::debug!(
                                "[hook] RCLICK on icon item={} at ({},{})",
                                item_idx,
                                info_hook.pt.x,
                                info_hook.pt.y
                            );
                            // Eat WM_RBUTTONDOWN — prevents native desktop menu.
                            // Selection + WM_CONTEXTMENU handled on button-up.
                            return LRESULT(1);
                        }
                    }
                }

                // Hover highlight: cross-process LVM_HITTEST → PostMessage LVM_SETHOTITEM (50ms throttle).
                // PostMessage(WM_MOUSEMOVE) fails because ListView hot-tracking calls GetCursorPos.
                if msg == WM_MOUSEMOVE && slv_raw != 0 {
                    let now = windows::Win32::System::SystemInformation::GetTickCount64();
                    let last = LAST_HOVER_TICK.load(Ordering::Relaxed);
                    if now.wrapping_sub(last) >= 50 {
                        LAST_HOVER_TICK.store(now, Ordering::Relaxed);
                        let slv_h = HWND(slv_raw as *mut _);
                        let item = get_hit_item_index(slv_h, &info_hook.pt);
                        let prev = CURRENT_HOT_ITEM.swap(item, Ordering::Relaxed);
                        if item != prev {
                            let _ = PostMessageW(
                                slv_h,
                                LVM_SETHOTITEM,
                                WPARAM(item as i32 as u32 as usize),
                                LPARAM(0),
                            );
                        }
                    }
                }

                let mut cp = info_hook.pt;
                let _ = ScreenToClient(HWND(wv_raw as *mut _), &mut cp);
                forward(msg, &info_hook, cp.x, cp.y);

                CallNextHookEx(hook_h, code, wparam, lparam)
            }

            /// Keyboard hook: forwards key events to Chrome_RWHH in interface mode.
            /// Generates WM_CHAR via ToUnicode for text input in WebView fields.
            unsafe extern "system" fn keyboard_hook_proc(
                code: i32,
                wparam: WPARAM,
                lparam: LPARAM,
            ) -> LRESULT {
                let hook_h = HHOOK(
                    crate::window_layer::KB_HOOK_HANDLE_GLOBAL.load(Ordering::Relaxed) as *mut _,
                );

                if code < 0 || !crate::window_layer::INTERFACE_MODE.load(Ordering::Relaxed) {
                    return CallNextHookEx(hook_h, code, wparam, lparam);
                }

                let rwhh = CHROME_RWHH.load(Ordering::Relaxed);
                if rwhh == 0 {
                    return CallNextHookEx(hook_h, code, wparam, lparam);
                }

                let kb = *(lparam.0 as *const KBDLLHOOKSTRUCT);
                let msg = wparam.0 as u32;
                let rwhh_hwnd = HWND(rwhh as *mut _);

                // Build lParam: repeat(0-15) | scanCode(16-23) | extended(24) |
                //               context(29) | previous(30) | transition(31)
                let extended = if kb.flags.0 & 0x01 != 0 { 1u32 } else { 0 };
                let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
                let is_sys = msg == WM_SYSKEYDOWN || msg == WM_SYSKEYUP;
                let lp = (1u32 // repeat count = 1
                    | ((kb.scanCode & 0xFF) << 16)
                    | (extended << 24)
                    | (if is_sys { 1u32 << 29 } else { 0 })
                    | (if is_up { 1u32 << 30 } else { 0 })
                    | (if is_up { 1u32 << 31 } else { 0 })) as isize;

                let _ = PostMessageW(rwhh_hwnd, msg, WPARAM(kb.vkCode as usize), LPARAM(lp));

                // Generate WM_CHAR for key-down events (text input).
                // Skip when Ctrl is held to avoid double-processing shortcuts (Ctrl+C etc.).
                if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
                    use windows::Win32::UI::Input::KeyboardAndMouse::{
                        GetKeyboardState, ToUnicode,
                    };
                    let mut kb_state = [0u8; 256];
                    let _ = GetKeyboardState(&mut kb_state);
                    if kb_state[0x11] & 0x80 == 0 {
                        // Ctrl not held — generate WM_CHAR for printable input
                        let mut chars = [0u16; 4];
                        let count =
                            ToUnicode(kb.vkCode, kb.scanCode, Some(&kb_state), &mut chars, 0);
                        let char_msg = if is_sys { WM_SYSCHAR } else { WM_CHAR };
                        for i in 0..count.max(0) as usize {
                            let _ = PostMessageW(
                                rwhh_hwnd,
                                char_msg,
                                WPARAM(chars[i] as usize),
                                LPARAM(lp),
                            );
                        }
                    }
                }

                CallNextHookEx(hook_h, code, wparam, lparam)
            }

            unsafe {
                if let Ok(h) = SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), None, 0) {
                    crate::window_layer::HOOK_HANDLE_GLOBAL.store(h.0 as isize, Ordering::SeqCst);
                }
                if let Ok(h) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0)
                {
                    crate::window_layer::KB_HOOK_HANDLE_GLOBAL
                        .store(h.0 as isize, Ordering::SeqCst);
                }
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, HWND::default(), 0, 0).into() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        });
    }
}

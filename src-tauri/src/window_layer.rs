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
static IS_SESSION_ACTIVE: AtomicBool = AtomicBool::new(true);
#[cfg(target_os = "windows")]
static WATCHDOG_PARENT: AtomicIsize = AtomicIsize::new(0);

// ==============================================================================
// Public API
// ==============================================================================

pub fn setup_desktop_window(_window: &tauri::WebviewWindow) {
    #[cfg(target_os = "windows")]
    {
        info!("[window_layer] Starting desktop window setup phase...");
        if let Err(e) = ensure_in_worker_w(_window) {
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
        use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOW};
        let slv = mouse_hook::get_syslistview_hwnd();
        if slv != 0 {
            unsafe {
                // ShowWindow returns BOOL (previous visibility state), not Result
                let _ = ShowWindow(HWND(slv as *mut _), if visible { SW_SHOW } else { SW_HIDE });
            }
        }
    }
    Ok(())
}

pub fn restore_desktop_icons_and_unhook() {
    if !ICONS_RESTORED.swap(true, Ordering::SeqCst) {
        #[cfg(target_os = "windows")]
        {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{
                ShowWindow, UnhookWindowsHookEx, HHOOK, SW_SHOW,
            };

            let slv = mouse_hook::get_syslistview_hwnd();
            if slv != 0 {
                unsafe {
                    // ShowWindow returns BOOL (previous visibility state), not Result
                    let _ = ShowWindow(HWND(slv as *mut _), SW_SHOW);
                }
            }

            let hook_ptr = HOOK_HANDLE_GLOBAL.load(Ordering::SeqCst);
            if hook_ptr != 0 {
                unsafe {
                    if let Err(e) = UnhookWindowsHookEx(HHOOK(hook_ptr as *mut _)) {
                        error!("[window_layer] Unhook mouse hook failed: {:?}", e);
                    }
                }
            }
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
        let mut shell_for_slv;

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
            std::thread::sleep(Duration::from_secs(30));
            let parent_raw = WATCHDOG_PARENT.load(Ordering::SeqCst);
            if parent_raw == 0 {
                continue;
            }
            unsafe {
                if !IsWindow(HWND(parent_raw as *mut _)).as_bool() {
                    info!("[watchdog] Parent HWND stale, re-detecting desktop...");
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
// Windows: Mouse Hook
// ==============================================================================

#[cfg(target_os = "windows")]
pub mod mouse_hook {
    use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
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
    static NATIVE_DRAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static INJECTING_FOR_DRAG: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    #[allow(dead_code)]
    static THREADS_ATTACHED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    // Cached values to avoid syscalls in hook hot path
    static OUR_PID: AtomicU32 = AtomicU32::new(0);
    static DBLCLICK_TIME: AtomicU32 = AtomicU32::new(0);
    static DBLCLICK_CX: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
    static DBLCLICK_CY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

    pub const WM_MWP_SETBOUNDS_PUB: u32 = 0x8000 + 43;
    const WM_MWP_MOUSE: u32 = 0x8000 + 42;

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
        let lp = LPARAM(((x as i16 as u16 as u32) | ((y as i16 as u16 as u32) << 16)) as isize);
        let _ = PostMessageW(HWND(dh as *mut _), WM_MWP_MOUSE, wp, lp);
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
        const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
        const WTS_SESSION_LOCK: u32 = 0x7;
        const WTS_SESSION_UNLOCK: u32 = 0x8;

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
                // NOTIFY_FOR_THIS_SESSION = 0x0
                let _ = WTSRegisterSessionNotification(h, 0);
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
        if !pm.is_invalid() && IsChild(pm, hwnd_under).as_bool() {
            return true;
        }
        // Also check if hwnd_under is a child of the target parent (WorkerW)
        if !tp.is_invalid() && IsChild(tp, hwnd_under).as_bool() {
            return true;
        }

        // Slow path: zero-allocation class name checks
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

    /// Check if screen point is over a desktop icon via LVM_HITTEST.
    /// Cross-process: allocates the LVHITTESTINFO struct in explorer.exe's
    /// address space so SysListView32 can read the pointer from SendMessage.
    unsafe fn hit_test_icon(slv: HWND, screen_pt: &windows::Win32::Foundation::POINT) -> bool {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::Graphics::Gdi::ScreenToClient;
        use windows::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
        use windows::Win32::System::Memory::{
            VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
        };
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
        };

        let mut pt = *screen_pt;
        let _ = ScreenToClient(slv, &mut pt);

        const LVM_FIRST: u32 = 0x1000;
        const LVM_HITTEST: u32 = LVM_FIRST + 18;

        #[repr(C)]
        struct LVHITTESTINFO {
            pt: windows::Win32::Foundation::POINT,
            flags: u32,
            i_item: i32,
            i_sub_item: i32,
            i_group: i32,
        }

        let ht = LVHITTESTINFO {
            pt,
            flags: 0,
            i_item: -1,
            i_sub_item: 0,
            i_group: 0,
        };

        // Open explorer.exe to allocate memory in its address space
        let mut pid = 0u32;
        GetWindowThreadProcessId(slv, Some(&mut pid));
        if pid == 0 {
            log::info!(
                "[hit_test] GetWindowThreadProcessId returned pid=0 for slv=0x{:X}",
                slv.0 as isize
            );
            return false;
        }

        let proc = match OpenProcess(
            PROCESS_VM_OPERATION | PROCESS_VM_READ | PROCESS_VM_WRITE,
            false,
            pid,
        ) {
            Ok(h) => h,
            Err(e) => {
                log::info!("[hit_test] OpenProcess failed for pid={}: {}", pid, e);
                return false;
            }
        };

        let size = std::mem::size_of::<LVHITTESTINFO>();
        let remote = VirtualAllocEx(proc, None, size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if remote.is_null() {
            log::info!("[hit_test] VirtualAllocEx failed (null), pid={}", pid);
            let _ = CloseHandle(proc);
            return false;
        }

        // Write our struct into explorer's memory
        if WriteProcessMemory(proc, remote, &ht as *const _ as _, size, None).is_err() {
            log::info!("[hit_test] WriteProcessMemory failed, pid={}", pid);
            let _ = VirtualFreeEx(proc, remote, 0, MEM_RELEASE);
            let _ = CloseHandle(proc);
            return false;
        }

        // Send hit test (explorer reads the remote pointer)
        let mut msg_result: usize = 0;
        let send_ok = SendMessageTimeoutW(
            slv,
            LVM_HITTEST,
            WPARAM(0),
            LPARAM(remote as isize),
            SMTO_ABORTIFHUNG,
            100, // 100ms — generous for cross-process round-trip
            Some(&mut msg_result),
        );

        // Read back the result
        let mut ht_out = LVHITTESTINFO {
            pt: windows::Win32::Foundation::POINT { x: 0, y: 0 },
            flags: 0,
            i_item: -1,
            i_sub_item: 0,
            i_group: 0,
        };
        let read_ok = ReadProcessMemory(proc, remote, &mut ht_out as *mut _ as _, size, None);

        let _ = VirtualFreeEx(proc, remote, 0, MEM_RELEASE);
        let _ = CloseHandle(proc);

        log::info!(
            "[hit_test] screen=({},{}) client=({},{}) send_ret={} msg_result={} read_ok={} i_item={} flags=0x{:X}",
            screen_pt.x,
            screen_pt.y,
            pt.x,
            pt.y,
            send_ok.0,
            msg_result,
            read_ok.is_ok(),
            ht_out.i_item,
            ht_out.flags,
        );

        ht_out.i_item >= 0
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
                use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

                // Cache process ID + double-click metrics once at hook startup
                OUR_PID.store(std::process::id(), Ordering::Relaxed);
                use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
                DBLCLICK_TIME.store(GetDoubleClickTime(), Ordering::Relaxed);
                DBLCLICK_CX.store(GetSystemMetrics(SM_CXDOUBLECLK) / 2, Ordering::Relaxed);
                DBLCLICK_CY.store(GetSystemMetrics(SM_CYDOUBLECLK) / 2, Ordering::Relaxed);
            }

            /// Post a mouse event to SysListView32, with double-click synthesis and key state.
            unsafe fn post_to_slv(slv: HWND, msg: u32, info_hook: &MSLLHOOKSTRUCT) {
                use windows::Win32::Graphics::Gdi::ScreenToClient;

                if !IsWindowVisible(slv).as_bool() {
                    return;
                }

                let lp = if msg == WM_MOUSEWHEEL || msg == WM_MOUSEHWHEEL {
                    ((info_hook.pt.x as i16 as u16 as u32)
                        | ((info_hook.pt.y as i16 as u16 as u32) << 16))
                        as isize
                } else {
                    let mut slv_cp = info_hook.pt;
                    let _ = ScreenToClient(slv, &mut slv_cp);
                    ((slv_cp.x as i16 as u16 as u32) | ((slv_cp.y as i16 as u16 as u32) << 16))
                        as isize
                };

                // Synthesize double-click (PostMessage bypasses native detection)
                let mut out_msg = msg;
                if msg == WM_LBUTTONDOWN {
                    static LAST_DOWN_TIME: std::sync::atomic::AtomicU32 =
                        std::sync::atomic::AtomicU32::new(0);
                    static LAST_DOWN_X: std::sync::atomic::AtomicI32 =
                        std::sync::atomic::AtomicI32::new(0);
                    static LAST_DOWN_Y: std::sync::atomic::AtomicI32 =
                        std::sync::atomic::AtomicI32::new(0);

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

            /// Attach/detach Chrome_RWHH thread ↔ explorer thread input queues.
            /// Currently unused: full-consume approach makes AttachThreadInput
            /// unnecessary. Kept for potential future OLE drag-to-external use.
            #[allow(dead_code)]
            unsafe fn set_thread_attach(slv_raw: isize, attach: bool) {
                extern "system" {
                    fn AttachThreadInput(idattach: u32, idattachto: u32, fattach: i32) -> i32;
                }

                let rwhh_raw = CHROME_RWHH.load(Ordering::Relaxed);
                if rwhh_raw == 0 || slv_raw == 0 {
                    return;
                }
                let chrome_tid = GetWindowThreadProcessId(HWND(rwhh_raw as *mut _), None);
                let explorer_tid = GetWindowThreadProcessId(HWND(slv_raw as *mut _), None);
                if chrome_tid != 0 && explorer_tid != 0 && chrome_tid != explorer_tid {
                    AttachThreadInput(chrome_tid, explorer_tid, i32::from(attach));
                    THREADS_ATTACHED.store(attach, Ordering::Relaxed);
                    log::info!(
                        "[hook] AttachThreadInput({}, {}, {}) → shared key state",
                        chrome_tid,
                        explorer_tid,
                        attach,
                    );
                }
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

                // ── Active native drag: forward to SysListView32, skip webview ──
                if NATIVE_DRAG.load(Ordering::Relaxed) {
                    // If this event was injected by us via SendInput, let it pass through
                    // to the real input stream (SysListView32 will receive it naturally).
                    if INJECTING_FOR_DRAG.load(Ordering::Relaxed) {
                        return CallNextHookEx(hook_h, code, wparam, lparam);
                    }

                    if msg == WM_LBUTTONUP || msg == WM_RBUTTONUP {
                        NATIVE_DRAG.store(false, Ordering::Relaxed);
                    }

                    // CRITICAL FIX: PostMessageW doesn't update the real cursor position,
                    // so DragDetect() (which calls GetCursorPos internally) sees no movement.
                    // Fix: use SendInput() to re-inject mouse events into the real input
                    // stream. The INJECTING_FOR_DRAG flag prevents re-entrant hook firing.
                    // The re-injected event bypasses our hook (INJECTING=true) and flows
                    // naturally to SysListView32 so DragDetect() and DoDragDrop() work.
                    INJECTING_FOR_DRAG.store(true, Ordering::Relaxed);
                    unsafe {
                        use windows::Win32::UI::Input::KeyboardAndMouse::{
                            SendInput, INPUT, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE,
                            MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE,
                            MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
                        };
                        // Convert screen coords to normalized absolute (0..65535)
                        let screen_w = windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                            windows::Win32::UI::WindowsAndMessaging::SM_CXSCREEN,
                        ) as f64;
                        let screen_h = windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                            windows::Win32::UI::WindowsAndMessaging::SM_CYSCREEN,
                        ) as f64;
                        let nx = ((info_hook.pt.x as f64 + 0.5) * 65536.0 / screen_w) as i32;
                        let ny = ((info_hook.pt.y as f64 + 0.5) * 65536.0 / screen_h) as i32;

                        let flags =
                            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | match msg {
                                WM_LBUTTONDOWN => MOUSEEVENTF_LEFTDOWN,
                                WM_LBUTTONUP => MOUSEEVENTF_LEFTUP,
                                WM_RBUTTONDOWN => MOUSEEVENTF_RIGHTDOWN,
                                WM_RBUTTONUP => MOUSEEVENTF_RIGHTUP,
                                _ => {
                                    windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS(
                                        0,
                                    )
                                }
                            };
                        let mut input = INPUT {
                            r#type: INPUT_MOUSE,
                            ..Default::default()
                        };
                        input.Anonymous.mi.dx = nx;
                        input.Anonymous.mi.dy = ny;
                        input.Anonymous.mi.dwFlags = flags;
                        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
                    }
                    INJECTING_FOR_DRAG.store(false, Ordering::Relaxed);

                    // Consume the original event so Chrome doesn't interfere.
                    return LRESULT(1);
                }

                // ── Not over desktop: pass through ──
                if !is_over_desktop(hwnd_under) {
                    return CallNextHookEx(hook_h, code, wparam, lparam);
                }

                // ── On desktop: check if button-down is on an icon ──
                if (msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN) && slv_raw != 0 {
                    log::info!(
                        "[hook] BTNDOWN pt=({},{}) hwnd=0x{:X} slv=0x{:X}",
                        info_hook.pt.x,
                        info_hook.pt.y,
                        hwnd_under.0 as isize,
                        slv_raw,
                    );

                    if hit_test_icon(HWND(slv_raw as *mut _), &info_hook.pt) {
                        NATIVE_DRAG.store(true, Ordering::Relaxed);
                        log::info!(
                            "[hook] Icon hit → NATIVE_DRAG, pt=({},{})",
                            info_hook.pt.x,
                            info_hook.pt.y,
                        );
                        post_to_slv(HWND(slv_raw as *mut _), msg, &info_hook);
                        // Consume: Chrome must not see the click (SetCapture/
                        // SetFocus would steal the gesture from SysListView32).
                        return LRESULT(1);
                    }
                }

                // ── Normal desktop interaction (wallpaper) ──
                use windows::Win32::Graphics::Gdi::ScreenToClient;
                let mut cp = info_hook.pt;
                let _ = ScreenToClient(HWND(wv_raw as *mut _), &mut cp);
                forward(msg, &info_hook, cp.x, cp.y);

                // PostMessageW fallback when native delivery won't reach SysListView32
                if (hwnd_under.0 as isize) != slv_raw && slv_raw != 0 {
                    post_to_slv(HWND(slv_raw as *mut _), msg, &info_hook);
                }

                CallNextHookEx(hook_h, code, wparam, lparam)
            }

            unsafe {
                if let Ok(h) = SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), None, 0) {
                    crate::window_layer::HOOK_HANDLE_GLOBAL.store(h.0 as isize, Ordering::SeqCst);
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

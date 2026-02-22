//! Window Layer — Desktop WebView injection + mouse forwarding (Windows only).
//!
//! Injects WebView into Progman/WorkerW hierarchy. Low-level mouse hook
//! intercepts events over the desktop and forwards them to WebView2 via
//! SendMouseInput (composition mode).

#[cfg(target_os = "windows")]
use log::{info, warn};
use std::sync::atomic::{AtomicBool, Ordering};

static ICONS_RESTORED: AtomicBool = AtomicBool::new(false);

// ============================================================================
// Public API
// ============================================================================

pub fn setup_desktop_window(_window: &tauri::WebviewWindow) {
    #[cfg(target_os = "windows")]
    let window = _window;
    #[cfg(target_os = "windows")]
    if let Err(e) = ensure_in_worker_w(window) {
        warn!("Failed to setup desktop layer: {}", e);
    }
}

#[tauri::command]
pub fn set_desktop_icons_visible(_visible: bool) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let visible = _visible;
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOW};
        use windows::Win32::Foundation::HWND;
        let slv = mouse_hook::get_syslistview_hwnd();
        if slv != 0 {
            unsafe { let _ = ShowWindow(HWND(slv as *mut _), if visible { SW_SHOW } else { SW_HIDE }); }
        }
    }
    Ok(())
}

pub fn restore_desktop_icons() {
    if ICONS_RESTORED.swap(true, Ordering::SeqCst) { return; }
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOW};
        use windows::Win32::Foundation::HWND;
        let slv = mouse_hook::get_syslistview_hwnd();
        if slv != 0 {
            unsafe { let _ = ShowWindow(HWND(slv as *mut _), SW_SHOW); }
        }
    }
}

// ============================================================================
// Windows: Desktop Detection
// ============================================================================

#[cfg(target_os = "windows")]
struct DesktopDetection {
    is_24h2: bool,
    target_parent: windows::Win32::Foundation::HWND,
    shell_view: windows::Win32::Foundation::HWND,
    os_workerw: windows::Win32::Foundation::HWND,
    syslistview: windows::Win32::Foundation::HWND,
    parent_width: i32,
    parent_height: i32,
}

#[cfg(target_os = "windows")]
fn detect_desktop() -> Result<DesktopDetection, String> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;

    unsafe {
        let progman = FindWindowW(windows::core::w!("Progman"), None)
            .map_err(|_| "Could not find Progman".to_string())?;

        let mut msg_result: usize = 0;
        let _ = SendMessageTimeoutW(progman, 0x052C, WPARAM(0x0D), LPARAM(1), SMTO_NORMAL, 1000, Some(&mut msg_result));

        let mut is_24h2 = false;
        let mut target_parent = HWND::default();
        let mut shell_view = HWND::default();
        let mut os_workerw = HWND::default();

        for _ in 0..40 {
            // 24H2+: SHELLDLL_DefView and WorkerW both inside Progman
            let sv = FindWindowExW(progman, HWND::default(), windows::core::w!("SHELLDLL_DefView"), None).unwrap_or_default();
            let ww = FindWindowExW(progman, HWND::default(), windows::core::w!("WorkerW"), None).unwrap_or_default();
            if !sv.is_invalid() && !ww.is_invalid() {
                is_24h2 = true;
                target_parent = progman;
                shell_view = sv;
                os_workerw = ww;
                break;
            }

            // Legacy: SHELLDLL_DefView inside a top-level WorkerW
            struct LegacyData { workerw: HWND, shell_view: HWND }
            let mut ld = LegacyData { workerw: HWND::default(), shell_view: HWND::default() };
            unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
                let d = &mut *(lp.0 as *mut LegacyData);
                if let Ok(s) = FindWindowExW(hwnd, HWND::default(), windows::core::w!("SHELLDLL_DefView"), None) {
                    if !s.is_invalid() {
                        d.shell_view = s;
                        if let Ok(w) = FindWindowExW(HWND::default(), hwnd, windows::core::w!("WorkerW"), None) {
                            if !w.is_invalid() { d.workerw = w; }
                        }
                        return BOOL(0);
                    }
                }
                BOOL(1)
            }
            let _ = EnumWindows(Some(cb), LPARAM(&mut ld as *mut _ as isize));
            if !ld.workerw.is_invalid() && !ld.shell_view.is_invalid() {
                target_parent = ld.workerw;
                shell_view = ld.shell_view;
                break;
            }

            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        if target_parent.is_invalid() {
            return Err("Desktop detection failed".to_string());
        }

        // Find SysListView32
        let mut syslistview = HWND::default();
        unsafe extern "system" fn find_slv(hwnd: HWND, lp: LPARAM) -> BOOL {
            let mut buf = [0u16; 256];
            let len = GetClassNameW(hwnd, &mut buf);
            if String::from_utf16_lossy(&buf[..len as usize]) == "SysListView32" {
                *(lp.0 as *mut HWND) = hwnd;
                return BOOL(0);
            }
            BOOL(1)
        }
        let _ = EnumChildWindows(shell_view, Some(find_slv), LPARAM(&mut syslistview as *mut _ as isize));

        let mut rect = windows::Win32::Foundation::RECT::default();
        let _ = GetClientRect(target_parent, &mut rect);

        Ok(DesktopDetection {
            is_24h2, target_parent, shell_view, os_workerw, syslistview,
            parent_width: rect.right,
            parent_height: rect.bottom,
        })
    }
}

// ============================================================================
// Windows: Injection
// ============================================================================

#[cfg(target_os = "windows")]
fn apply_injection(our_hwnd: windows::Win32::Foundation::HWND, detection: &DesktopDetection) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::*;

    unsafe {
        if GetParent(our_hwnd) == Ok(detection.target_parent) { return; }

        let mut style = GetWindowLongW(our_hwnd, GWL_STYLE) as u32;
        style &= !(WS_THICKFRAME.0 | WS_CAPTION.0 | WS_SYSMENU.0
                  | WS_MAXIMIZEBOX.0 | WS_MINIMIZEBOX.0 | WS_POPUP.0);
        style |= WS_CHILD.0 | WS_VISIBLE.0;
        let _ = SetWindowLongW(our_hwnd, GWL_STYLE, style as i32);

        let _ = SetParent(our_hwnd, detection.target_parent);

        if detection.is_24h2 {
            let _ = SetWindowPos(our_hwnd, detection.shell_view, 0, 0, 0, 0,
                SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOSIZE | SWP_NOMOVE);
            let _ = SetWindowPos(detection.os_workerw, our_hwnd, 0, 0, 0, 0,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOMOVE);
        }

        let _ = SetWindowPos(our_hwnd, HWND::default(),
            0, 0, detection.parent_width, detection.parent_height,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);

        // Verify injection
        let actual_parent = GetParent(our_hwnd).unwrap_or_default();
        let parent_ok = actual_parent == detection.target_parent;
        let visible = IsWindowVisible(our_hwnd).as_bool();
        info!("[diag] Injected: {}x{} ({}) parent_ok={} visible={} actual_parent=0x{:X}",
            detection.parent_width, detection.parent_height,
            if detection.is_24h2 { "24H2" } else { "Legacy" },
            parent_ok, visible, actual_parent.0 as isize);
    }
}

// ============================================================================
// Windows: State Dump (diagnostic)
// ============================================================================

#[cfg(target_os = "windows")]
fn dump_state(our_hwnd: windows::Win32::Foundation::HWND, det: &DesktopDetection) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::Win32::Foundation::RECT;

    unsafe {
        // Helper: get window info string
        let describe = |hwnd: HWND, label: &str| -> String {
            if hwnd.is_invalid() { return format!("  {} = NULL", label); }
            let mut cls = [0u16; 64];
            let len = GetClassNameW(hwnd, &mut cls);
            let cls_name = String::from_utf16_lossy(&cls[..len as usize]);
            let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
            let exstyle = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            let mut r = RECT::default();
            let _ = GetWindowRect(hwnd, &mut r);
            let visible = IsWindowVisible(hwnd).as_bool();
            let parent = GetParent(hwnd).unwrap_or_default();
            format!("  {} = 0x{:X} class='{}' rect=({},{},{},{}) {}x{} style=0x{:08X} exstyle=0x{:08X} visible={} parent=0x{:X}",
                label, hwnd.0 as isize, cls_name,
                r.left, r.top, r.right, r.bottom, r.right - r.left, r.bottom - r.top,
                style, exstyle, visible, parent.0 as isize)
        };

        // Enumerate children of target_parent for Z-order
        let mut children = Vec::new();
        let mut child = GetWindow(det.target_parent, GW_CHILD).unwrap_or_default();
        for _ in 0..20 {
            if child.is_invalid() { break; }
            let mut cls = [0u16; 64];
            let len = GetClassNameW(child, &mut cls);
            let cls_name = String::from_utf16_lossy(&cls[..len as usize]);
            let visible = IsWindowVisible(child).as_bool();
            children.push(format!("0x{:X}({}{})", child.0 as isize, cls_name, if visible { "" } else { ",hidden" }));
            child = GetWindow(child, GW_HWNDNEXT).unwrap_or_default();
        }

        // Get screen info
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);

        let dump = format!(
            "[diag] === STATE DUMP ===\n\
            [diag] Screen: {}x{}\n\
            {}\n{}\n{}\n{}\n{}\n\
            [diag] Z-order in target_parent (top→bottom): [{}]\n\
            [diag] === END DUMP ===",
            screen_w, screen_h,
            describe(det.target_parent, "target_parent"),
            describe(det.shell_view, "shell_view"),
            describe(det.os_workerw, "os_workerw"),
            describe(det.syslistview, "syslistview"),
            describe(our_hwnd, "our_hwnd"),
            children.join(", "),
        );

        for line in dump.lines() {
            info!("{}", line);
        }
    }
}

// ============================================================================
// Windows: Init
// ============================================================================

#[cfg(target_os = "windows")]
fn ensure_in_worker_w(window: &tauri::WebviewWindow) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;

    let our_hwnd = window.hwnd().map_err(|e| format!("{}", e))?;
    let our_hwnd = HWND(our_hwnd.0 as *mut _);
    let detection = detect_desktop()?;

    info!("[diag] detection: mode={} tp=0x{:X} sv=0x{:X} ww=0x{:X} slv=0x{:X} size={}x{} ourHwnd=0x{:X}",
        if detection.is_24h2 { "24H2" } else { "Legacy" },
        detection.target_parent.0 as isize,
        detection.shell_view.0 as isize,
        detection.os_workerw.0 as isize,
        detection.syslistview.0 as isize,
        detection.parent_width, detection.parent_height,
        our_hwnd.0 as isize);

    mouse_hook::set_webview_hwnd(our_hwnd.0 as isize);
    mouse_hook::set_target_parent_hwnd(detection.target_parent.0 as isize);
    if !detection.syslistview.is_invalid() {
        mouse_hook::set_syslistview_hwnd(detection.syslistview.0 as isize);
    }

    apply_injection(our_hwnd, &detection);

    // State dump: window hierarchy, styles, positions, Z-order
    dump_state(our_hwnd, &detection);

    // WebView2 bounds — poll for controller, then marshal SetBounds to main thread
    let (w, h) = (detection.parent_width, detection.parent_height);
    std::thread::spawn(move || {
        use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
        for _ in 0..60 {
            let ptr = wry::get_last_composition_controller_ptr();
            if ptr != 0 {
                mouse_hook::set_comp_controller_ptr(ptr);
                log::info!("[diag] CompController=0x{:X}, requesting SetBounds {}x{} on main thread", ptr, w, h);
                // Marshal SetBounds to main thread via dispatch window
                let dh = mouse_hook::get_dispatch_hwnd();
                if dh != 0 {
                    unsafe {
                        let _ = PostMessageW(HWND(dh as *mut _),
                            mouse_hook::WM_MWP_SETBOUNDS_PUB,
                            WPARAM(w as usize), LPARAM(h as isize));
                    }
                } else {
                    log::error!("[diag] CompController ready but dispatch window not yet created");
                }
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        log::warn!("CompositionController not found after 3s");
    });

    mouse_hook::init_dispatch_window();
    mouse_hook::start_hook_thread();
    Ok(())
}

// ============================================================================
// Windows: Mouse Hook
// ============================================================================

#[cfg(target_os = "windows")]
pub mod mouse_hook {
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, Ordering};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;

    const MOUSE_MOVE: i32   = 0x0200;
    const MOUSE_LDOWN: i32  = 0x0201;
    const MOUSE_LUP: i32    = 0x0202;
    const MOUSE_RDOWN: i32  = 0x0204;
    const MOUSE_RUP: i32    = 0x0205;
    const MOUSE_MDOWN: i32  = 0x0207;
    const MOUSE_MUP: i32    = 0x0208;
    const MOUSE_WHEEL: i32  = 0x020A;
    const MOUSE_HWHEEL: i32 = 0x020E;

    const VK_NONE: i32    = 0x0;
    const VK_LBUTTON: i32 = 0x1;
    const VK_RBUTTON: i32 = 0x2;
    const VK_MBUTTON: i32 = 0x10;

    static WEBVIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static SYSLISTVIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static TARGET_PARENT_HWND: AtomicIsize = AtomicIsize::new(0);
    static COMP_CONTROLLER_PTR: AtomicIsize = AtomicIsize::new(0);
    static DRAG_VK: AtomicIsize = AtomicIsize::new(0);
    static DISPATCH_HWND: AtomicIsize = AtomicIsize::new(0);

    const STATE_IDLE: u8 = 0;
    const STATE_DRAGGING: u8 = 1;
    static HOOK_STATE: AtomicU8 = AtomicU8::new(STATE_IDLE);

    // Diagnostic: log first N events at each stage
    static DIAG_HOOK: AtomicBool = AtomicBool::new(true);
    static DIAG_DISPATCH: AtomicBool = AtomicBool::new(true);
    static DIAG_MISS_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    static DIAG_MISS_LOGGED: AtomicBool = AtomicBool::new(false);

    const WM_MWP_MOUSE: u32 = 0x8000 + 42;
    const WM_MWP_SETBOUNDS: u32 = 0x8000 + 43;

    pub fn set_webview_hwnd(h: isize)        { WEBVIEW_HWND.store(h, Ordering::SeqCst); }
    pub fn set_syslistview_hwnd(h: isize)    { SYSLISTVIEW_HWND.store(h, Ordering::SeqCst); }
    pub fn set_target_parent_hwnd(h: isize)  { TARGET_PARENT_HWND.store(h, Ordering::SeqCst); }
    pub fn get_syslistview_hwnd() -> isize   { SYSLISTVIEW_HWND.load(Ordering::SeqCst) }
    pub fn set_comp_controller_ptr(p: isize) { COMP_CONTROLLER_PTR.store(p, Ordering::SeqCst); }
    pub fn get_comp_controller_ptr() -> isize { COMP_CONTROLLER_PTR.load(Ordering::SeqCst) }
    pub fn get_dispatch_hwnd() -> isize { DISPATCH_HWND.load(Ordering::SeqCst) }
    pub const WM_MWP_SETBOUNDS_PUB: u32 = WM_MWP_SETBOUNDS;

    static DIAG_POST_FAIL: AtomicBool = AtomicBool::new(true);

    #[inline]
    unsafe fn post_mouse(kind: i32, vk: i32, data: u32, x: i32, y: i32) {
        let dh = DISPATCH_HWND.load(Ordering::Relaxed);
        if dh == 0 {
            if DIAG_POST_FAIL.swap(false, Ordering::Relaxed) {
                log::error!("[diag] post_mouse: DISPATCH_HWND=0, events lost");
            }
            return;
        }
        let wp = WPARAM((kind as u16 as usize) | ((vk as u16 as usize) << 16) | ((data as usize) << 32));
        let lp = LPARAM(((x as i16 as u16 as u32) | ((y as i16 as u16 as u32) << 16)) as isize);
        if let Err(e) = PostMessageW(HWND(dh as *mut _), WM_MWP_MOUSE, wp, lp) {
            if DIAG_POST_FAIL.swap(false, Ordering::Relaxed) {
                log::error!("[diag] PostMessageW FAILED: {} dh=0x{:X}", e, dh);
            }
        }
    }

    unsafe extern "system" fn dispatch_wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_MWP_SETBOUNDS {
            let w = wp.0 as i32;
            let h = lp.0 as i32;
            let ptr = get_comp_controller_ptr();
            if ptr != 0 {
                let result = wry::set_controller_bounds_raw(ptr, w, h);
                log::info!("[diag] SetBounds (main thread): {}x{} result={:?}", w, h, result);
            }
            return LRESULT(0);
        }
        if msg == WM_MWP_MOUSE {
            let kind = (wp.0 & 0xFFFF) as i32;
            let vk = ((wp.0 >> 16) & 0xFFFF) as i32;
            let data = ((wp.0 >> 32) & 0xFFFFFFFF) as u32;
            let x = (lp.0 & 0xFFFF) as i16 as i32;
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let ptr = get_comp_controller_ptr();
            if DIAG_DISPATCH.swap(false, Ordering::Relaxed) {
                log::info!("[diag] dispatch: kind=0x{:X} vk=0x{:X} data={} x={} y={} ptr=0x{:X}", kind, vk, data, x, y, ptr);
            }
            if ptr != 0 {
                if let Err(e) = wry::send_mouse_input_raw(ptr, kind, vk, data, x, y) {
                    log::error!("[diag] SendMouseInput FAILED: {} kind=0x{:X} x={} y={}", e, kind, x, y);
                }
            } else {
                log::error!("[diag] dispatch: ptr=0, no CompController — dropping event");
            }
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }

    pub fn init_dispatch_window() {
        unsafe {
            let cls = windows::core::w!("MWP_MouseDispatch");
            let wc = WNDCLASSW { lpfnWndProc: Some(dispatch_wnd_proc), lpszClassName: cls, ..Default::default() };
            let _ = RegisterClassW(&wc);
            match CreateWindowExW(WINDOW_EX_STYLE(0), cls, windows::core::w!(""),
                WINDOW_STYLE(0), 0, 0, 0, 0, HWND_MESSAGE, None, None, None)
            {
                Ok(h) => {
                    DISPATCH_HWND.store(h.0 as isize, Ordering::SeqCst);
                    log::info!("[diag] dispatch window=0x{:X}", h.0 as isize);
                }
                Err(e) => {
                    log::error!("[diag] dispatch window FAILED: {}", e);
                }
            }
        }
    }

    /// Cursor is over the desktop area. Checks:
    /// 1. Direct hit on target_parent (Progman/WorkerW) or SysListView32
    /// 2. Child of target_parent (SHELLDLL_DefView, our Tauri Window, etc.)
    /// 3. Same-process window (WebView2's Chrome_RenderWidgetHostHWND sits
    ///    outside the HWND hierarchy in composition mode)
    #[inline]
    unsafe fn is_over_desktop(hwnd_under: HWND) -> bool {
        let tp = HWND(TARGET_PARENT_HWND.load(Ordering::Relaxed) as *mut _);
        let slv = HWND(SYSLISTVIEW_HWND.load(Ordering::Relaxed) as *mut _);
        if hwnd_under == tp || hwnd_under == slv || IsChild(tp, hwnd_under).as_bool() {
            return true;
        }
        // Composition mode: Chrome_RenderWidgetHostHWND is detached from Progman tree
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd_under, Some(&mut pid));
        pid != 0 && pid == std::process::id()
    }

    #[inline]
    unsafe fn forward(msg: u32, info: &MSLLHOOKSTRUCT, cx: i32, cy: i32) {
        let x = cx.max(0);
        let y = cy.max(0);
        match msg {
            WM_MOUSEMOVE   => post_mouse(MOUSE_MOVE, DRAG_VK.load(Ordering::Relaxed) as i32, 0, x, y),
            WM_LBUTTONDOWN => { DRAG_VK.store(VK_LBUTTON as isize, Ordering::Relaxed); post_mouse(MOUSE_LDOWN, VK_LBUTTON, 0, x, y); }
            WM_LBUTTONUP   => { DRAG_VK.store(0, Ordering::Relaxed); post_mouse(MOUSE_LUP, VK_NONE, 0, x, y); }
            WM_RBUTTONDOWN => { DRAG_VK.store(VK_RBUTTON as isize, Ordering::Relaxed); post_mouse(MOUSE_RDOWN, VK_RBUTTON, 0, x, y); }
            WM_RBUTTONUP   => { DRAG_VK.store(0, Ordering::Relaxed); post_mouse(MOUSE_RUP, VK_NONE, 0, x, y); }
            WM_MBUTTONDOWN => { DRAG_VK.store(VK_MBUTTON as isize, Ordering::Relaxed); post_mouse(MOUSE_MDOWN, VK_MBUTTON, 0, x, y); }
            WM_MBUTTONUP   => { DRAG_VK.store(0, Ordering::Relaxed); post_mouse(MOUSE_MUP, VK_NONE, 0, x, y); }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let delta = (info.mouseData >> 16) as i16 as i32 as u32;
                let kind = if msg == WM_MOUSEWHEEL { MOUSE_WHEEL } else { MOUSE_HWHEEL };
                post_mouse(kind, VK_NONE, delta, x, y);
            }
            _ => {}
        }
    }

    pub fn start_hook_thread() {
        std::thread::spawn(|| {
            unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
                if code < 0 {
                    return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                }
                let wv_raw = WEBVIEW_HWND.load(Ordering::Relaxed);
                if wv_raw == 0 {
                    return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                }

                let info = *(lparam.0 as *const MSLLHOOKSTRUCT);
                let msg = wparam.0 as u32;
                let wv = HWND(wv_raw as *mut _);
                let is_down = msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN || msg == WM_MBUTTONDOWN;
                let is_up = msg == WM_LBUTTONUP || msg == WM_RBUTTONUP || msg == WM_MBUTTONUP;

                // DRAGGING: forward everything until button up
                if HOOK_STATE.load(Ordering::Relaxed) == STATE_DRAGGING {
                    use windows::Win32::Graphics::Gdi::ScreenToClient;
                    let mut cp = info.pt;
                    let _ = ScreenToClient(wv, &mut cp);
                    forward(msg, &info, cp.x, cp.y);
                    if is_up { HOOK_STATE.store(STATE_IDLE, Ordering::Relaxed); }
                    if msg == WM_MOUSEMOVE { return CallNextHookEx(HHOOK::default(), code, wparam, lparam); }
                    return LRESULT(1);
                }

                // IDLE: check if over desktop
                let hwnd_under = WindowFromPoint(info.pt);
                if !is_over_desktop(hwnd_under) {
                    // Log first 3 misses so we know what HWND is blocking
                    if !DIAG_MISS_LOGGED.load(Ordering::Relaxed) {
                        let count = DIAG_MISS_COUNT.fetch_add(1, Ordering::Relaxed);
                        if count < 3 {
                            let tp = TARGET_PARENT_HWND.load(Ordering::Relaxed);
                            let slv = SYSLISTVIEW_HWND.load(Ordering::Relaxed);
                            let wv = WEBVIEW_HWND.load(Ordering::Relaxed);
                            let mut cls = [0u16; 64];
                            let len = GetClassNameW(hwnd_under, &mut cls);
                            let cls_name = String::from_utf16_lossy(&cls[..len as usize]);
                            log::warn!("[diag] MISS {}/3: under=0x{:X} class='{}' pt=({},{}) tp=0x{:X} slv=0x{:X} wv=0x{:X} isChild={}",
                                count+1, hwnd_under.0 as isize, cls_name, info.pt.x, info.pt.y,
                                tp, slv, wv,
                                IsChild(HWND(tp as *mut _), hwnd_under).as_bool());
                        } else {
                            DIAG_MISS_LOGGED.store(true, Ordering::Relaxed);
                        }
                    }
                    return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                }

                use windows::Win32::Graphics::Gdi::ScreenToClient;
                let mut cp = info.pt;
                let stc_ok = ScreenToClient(wv, &mut cp).as_bool();

                if DIAG_HOOK.swap(false, Ordering::Relaxed) {
                    let tp = TARGET_PARENT_HWND.load(Ordering::Relaxed);
                    let slv = SYSLISTVIEW_HWND.load(Ordering::Relaxed);
                    log::info!("[diag] hook hit: under=0x{:X} tp=0x{:X} slv=0x{:X} wv=0x{:X} screen=({},{}) client=({},{}) stc_ok={} msg=0x{:X}",
                        hwnd_under.0 as isize, tp, slv, wv_raw, info.pt.x, info.pt.y, cp.x, cp.y, stc_ok, msg);
                }
                forward(msg, &info, cp.x, cp.y);
                if is_down { HOOK_STATE.store(STATE_DRAGGING, Ordering::Relaxed); }
                if msg == WM_MOUSEMOVE { return CallNextHookEx(HHOOK::default(), code, wparam, lparam); }
                LRESULT(1)
            }

            unsafe {
                let hook_result = SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), None, 0);
                match &hook_result {
                    Ok(h) => log::info!("[diag] hook installed=0x{:X}", h.0 as isize),
                    Err(e) => log::error!("[diag] hook FAILED: {}", e),
                }
                let _h = hook_result;
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, HWND::default(), 0, 0).into() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        });
    }
}

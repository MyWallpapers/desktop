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

        info!("Injected: {}x{} ({})", detection.parent_width, detection.parent_height,
            if detection.is_24h2 { "24H2" } else { "Legacy" });
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

    mouse_hook::set_webview_hwnd(our_hwnd.0 as isize);
    mouse_hook::set_target_parent_hwnd(detection.target_parent.0 as isize);
    if !detection.syslistview.is_invalid() {
        mouse_hook::set_syslistview_hwnd(detection.syslistview.0 as isize);
    }

    apply_injection(our_hwnd, &detection);

    // WebView2 bounds — always poll (controller may not be ready yet)
    let (w, h) = (detection.parent_width, detection.parent_height);
    std::thread::spawn(move || {
        for _ in 0..60 {
            let ptr = wry::get_last_composition_controller_ptr();
            if ptr != 0 {
                mouse_hook::set_comp_controller_ptr(ptr);
                unsafe { let _ = wry::set_controller_bounds_raw(ptr, w, h); }
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
    use std::sync::atomic::{AtomicIsize, AtomicU8, Ordering};
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

    const WM_MWP_MOUSE: u32 = 0x8000 + 42;

    pub fn set_webview_hwnd(h: isize)        { WEBVIEW_HWND.store(h, Ordering::SeqCst); }
    pub fn set_syslistview_hwnd(h: isize)    { SYSLISTVIEW_HWND.store(h, Ordering::SeqCst); }
    pub fn set_target_parent_hwnd(h: isize)  { TARGET_PARENT_HWND.store(h, Ordering::SeqCst); }
    pub fn get_syslistview_hwnd() -> isize   { SYSLISTVIEW_HWND.load(Ordering::SeqCst) }
    pub fn set_comp_controller_ptr(p: isize) { COMP_CONTROLLER_PTR.store(p, Ordering::SeqCst); }
    pub fn get_comp_controller_ptr() -> isize { COMP_CONTROLLER_PTR.load(Ordering::SeqCst) }

    #[inline]
    unsafe fn post_mouse(kind: i32, vk: i32, data: u32, x: i32, y: i32) {
        let dh = DISPATCH_HWND.load(Ordering::Relaxed);
        if dh == 0 { return; }
        let wp = WPARAM((kind as u16 as usize) | ((vk as u16 as usize) << 16) | ((data as usize) << 32));
        let lp = LPARAM(((x as i16 as u16 as u32) | ((y as i16 as u16 as u32) << 16)) as isize);
        let _ = PostMessageW(HWND(dh as *mut _), WM_MWP_MOUSE, wp, lp);
    }

    unsafe extern "system" fn dispatch_wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_MWP_MOUSE {
            let kind = (wp.0 & 0xFFFF) as i32;
            let vk = ((wp.0 >> 16) & 0xFFFF) as i32;
            let data = ((wp.0 >> 32) & 0xFFFFFFFF) as u32;
            let x = (lp.0 & 0xFFFF) as i16 as i32;
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            let ptr = get_comp_controller_ptr();
            if ptr != 0 { let _ = wry::send_mouse_input_raw(ptr, kind, vk, data, x, y); }
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }

    pub fn init_dispatch_window() {
        unsafe {
            let cls = windows::core::w!("MWP_MouseDispatch");
            let wc = WNDCLASSW { lpfnWndProc: Some(dispatch_wnd_proc), lpszClassName: cls, ..Default::default() };
            let _ = RegisterClassW(&wc);
            if let Ok(h) = CreateWindowExW(WINDOW_EX_STYLE(0), cls, windows::core::w!(""),
                WINDOW_STYLE(0), 0, 0, 0, 0, HWND_MESSAGE, None, None, None)
            {
                DISPATCH_HWND.store(h.0 as isize, Ordering::SeqCst);
            }
        }
    }

    /// Cursor is over the desktop if it hits our target_parent, any of its
    /// children (WebView, SHELLDLL_DefView in 24H2…), or SysListView32
    /// (explicit check needed for Legacy mode where it's outside target_parent).
    #[inline]
    unsafe fn is_over_desktop(hwnd_under: HWND) -> bool {
        let tp = HWND(TARGET_PARENT_HWND.load(Ordering::Relaxed) as *mut _);
        let slv = HWND(SYSLISTVIEW_HWND.load(Ordering::Relaxed) as *mut _);
        hwnd_under == tp || IsChild(tp, hwnd_under).as_bool() || hwnd_under == slv
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
                if !is_over_desktop(WindowFromPoint(info.pt)) {
                    return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                }

                use windows::Win32::Graphics::Gdi::ScreenToClient;
                let mut cp = info.pt;
                let _ = ScreenToClient(wv, &mut cp);
                forward(msg, &info, cp.x, cp.y);
                if is_down { HOOK_STATE.store(STATE_DRAGGING, Ordering::Relaxed); }
                if msg == WM_MOUSEMOVE { return CallNextHookEx(HHOOK::default(), code, wparam, lparam); }
                LRESULT(1)
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

//! Window Layer — Desktop WebView injection + mouse forwarding (Windows only).
//!
//! Injects WebView into Progman/WorkerW hierarchy. Low-level mouse hook
//! intercepts events over the desktop and forwards them to WebView2 via
//! SendMouseInput (composition mode).

#[cfg(target_os = "windows")]
use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicBool, Ordering};

static ICONS_RESTORED: AtomicBool = AtomicBool::new(false);

// ============================================================================
// Public API
// ============================================================================

pub fn setup_desktop_window(_window: &tauri::WebviewWindow) {
    #[cfg(target_os = "windows")]
    {
        info!("[window_layer] Starting desktop window setup...");
        if let Err(e) = ensure_in_worker_w(_window) {
            error!("[window_layer] CRITICAL: Failed to setup desktop layer: {}", e);
        } else {
            info!("[window_layer] Desktop layer setup completed successfully.");
        }
    }
}

#[tauri::command]
pub fn set_desktop_icons_visible(_visible: bool) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOW};
        let slv = mouse_hook::get_syslistview_hwnd();
        debug!("[window_layer] set_desktop_icons_visible requested: visible={}, slv=0x{:X}", _visible, slv);
        if slv != 0 {
            unsafe {
                let _ = ShowWindow(
                    HWND(slv as *mut _),
                    if _visible { SW_SHOW } else { SW_HIDE },
                );
            }
            info!("[window_layer] Desktop icons visibility set to {}", _visible);
        } else {
            warn!("[window_layer] Cannot change icon visibility: SysListView32 HWND is 0");
        }
    }
    Ok(())
}

pub fn restore_desktop_icons() {
    if ICONS_RESTORED.swap(true, Ordering::SeqCst) {
        debug!("[window_layer] restore_desktop_icons called, but already restored.");
        return;
    }
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOW};
        let slv = mouse_hook::get_syslistview_hwnd();
        debug!("[window_layer] Executing safety restore of desktop icons. slv=0x{:X}", slv);
        if slv != 0 {
            unsafe {
                let _ = ShowWindow(HWND(slv as *mut _), SW_SHOW);
            }
            info!("[window_layer] Desktop icons successfully restored on exit.");
        }
    }
}

// ============================================================================
// Windows: Desktop Detection (Modern Win10/Win11)
// ============================================================================

#[cfg(target_os = "windows")]
struct DesktopDetection {
    is_24h2: bool,
    target_parent: windows::Win32::Foundation::HWND,
    shell_view: windows::Win32::Foundation::HWND,
    os_workerw: windows::Win32::Foundation::HWND,
    syslistview: windows::Win32::Foundation::HWND,
    v_x: i32,
    v_y: i32,
    v_width: i32,
    v_height: i32,
}

#[cfg(target_os = "windows")]
fn detect_desktop() -> Result<DesktopDetection, String> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;

    debug!("[detect_desktop] Initiating desktop window detection...");

    unsafe {
        let progman = FindWindowW(windows::core::w!("Progman"), None)
            .map_err(|_| "Could not find Progman. Explorer might be crashed.".to_string())?;

        info!("[detect_desktop] Found Progman HWND: 0x{:X}", progman.0 as isize);

        // Send message to spawn WorkerW if it doesn't exist
        debug!("[detect_desktop] Sending 0x052C message to Progman to trigger WorkerW spawn...");
        let mut msg_result: usize = 0;
        let _ = SendMessageTimeoutW(
            progman,
            0x052C,
            WPARAM(0x0D),
            LPARAM(1),
            SMTO_NORMAL,
            1000,
            Some(&mut msg_result),
        );
        debug!("[detect_desktop] 0x052C message sent. Result: {}", msg_result);

        let mut is_24h2 = false;
        let mut target_parent = HWND::default();
        let mut shell_view = HWND::default();
        let mut os_workerw = HWND::default();

        debug!("[detect_desktop] Polling for hierarchy resolution (max 40 attempts)...");
        for attempt in 1..=40 {
            // Check for Windows 11 24H2+ layout: SHELLDLL_DefView inside Progman
            let sv = FindWindowExW(
                progman,
                HWND::default(),
                windows::core::w!("SHELLDLL_DefView"),
                None,
            ).unwrap_or_default();

            let ww = FindWindowExW(
                progman,
                HWND::default(),
                windows::core::w!("WorkerW"),
                None,
            ).unwrap_or_default();

            if !sv.is_invalid() && !ww.is_invalid() {
                info!("[detect_desktop] Discovered Windows 11 24H2+ architecture on attempt {}", attempt);
                is_24h2 = true;
                target_parent = progman;
                shell_view = sv;
                os_workerw = ww;
                break;
            }

            // Check for Standard Windows 10 / Windows 11 layout: SHELLDLL_DefView inside detached WorkerW
            struct ModernData {
                workerw: HWND,
                shell_view: HWND,
            }
            let mut md = ModernData {
                workerw: HWND::default(),
                shell_view: HWND::default(),
            };

            unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
                let d = &mut *(lp.0 as *mut ModernData);
                if let Ok(s) = FindWindowExW(
                    hwnd,
                    HWND::default(),
                    windows::core::w!("SHELLDLL_DefView"),
                    None,
                ) {
                    if !s.is_invalid() {
                        d.shell_view = s;
                        if let Ok(w) = FindWindowExW(
                            HWND::default(),
                            hwnd,
                            windows::core::w!("WorkerW"),
                            None,
                        ) {
                            if !w.is_invalid() {
                                d.workerw = w;
                            }
                        }
                        return BOOL(0); // Found it, stop enumeration
                    }
                }
                BOOL(1) // Continue enumeration
            }

            let _ = EnumWindows(Some(cb), LPARAM(&mut md as *mut _ as isize));
            if !md.workerw.is_invalid() && !md.shell_view.is_invalid() {
                info!("[detect_desktop] Discovered Standard Win10/Win11 architecture on attempt {}", attempt);
                target_parent = md.workerw;
                shell_view = md.shell_view;
                break;
            }

            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        if target_parent.is_invalid() {
            error!("[detect_desktop] Failed to find target parent after 40 attempts.");
            return Err("Desktop detection failed. Could not locate WorkerW/SHELLDLL_DefView hierarchy.".to_string());
        }

        debug!("[detect_desktop] Finding SysListView32 (Desktop Icons layer)...");
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
        let _ = EnumChildWindows(
            shell_view,
            Some(find_slv),
            LPARAM(&mut syslistview as *mut _ as isize),
        );

        if syslistview.is_invalid() {
            warn!("[detect_desktop] SysListView32 not found! Desktop icons will not be interactable.");
        } else {
            info!("[detect_desktop] Found SysListView32 HWND: 0x{:X}", syslistview.0 as isize);
        }

        debug!("[detect_desktop] Querying Virtual Screen metrics (Multi-Monitor support)...");
        let v_x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let v_y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let v_width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let v_height = GetSystemMetrics(SM_CYVIRTUALSCREEN);

        info!("[detect_desktop] Virtual Screen Metrics: Origin({}, {}), Size({}x{})", v_x, v_y, v_width, v_height);

        Ok(DesktopDetection {
            is_24h2,
            target_parent,
            shell_view,
            os_workerw,
            syslistview,
            v_x,
            v_y,
            v_width,
            v_height,
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

    debug!("[apply_injection] Beginning window injection process for HWND 0x{:X}", our_hwnd.0 as isize);

    unsafe {
        let current_parent = GetParent(our_hwnd).unwrap_or_default();
        if current_parent == detection.target_parent {
            debug!("[apply_injection] Window is already injected into target parent. Skipping.");
            return;
        }

        debug!("[apply_injection] Stripping standard window styles (THICKFRAME, CAPTION, SYSMENU)...");
        let mut style = GetWindowLongW(our_hwnd, GWL_STYLE) as u32;
        style &= !(WS_THICKFRAME.0
            | WS_CAPTION.0
            | WS_SYSMENU.0
            | WS_MAXIMIZEBOX.0
            | WS_MINIMIZEBOX.0
            | WS_POPUP.0);
        style |= WS_CHILD.0 | WS_VISIBLE.0;
        let _ = SetWindowLongW(our_hwnd, GWL_STYLE, style as i32);

        debug!("[apply_injection] Applying WS_EX_NOACTIVATE...");
        let mut ex_style = GetWindowLongW(our_hwnd, GWL_EXSTYLE) as u32;
        ex_style |= WS_EX_NOACTIVATE.0;
        let _ = SetWindowLongW(our_hwnd, GWL_EXSTYLE, ex_style as i32);

        debug!("[apply_injection] Calling SetParent...");
        let _ = SetParent(our_hwnd, detection.target_parent);

        if detection.is_24h2 {
            debug!("[apply_injection] Applying 24H2 specific Z-Order placement...");
            let _ = SetWindowPos(
                our_hwnd,
                detection.shell_view,
                0, 0, 0, 0,
                SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOSIZE | SWP_NOMOVE,
            );
            let _ = SetWindowPos(
                detection.os_workerw,
                our_hwnd,
                0, 0, 0, 0,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOMOVE,
            );
        } else {
            debug!("[apply_injection] Applying standard Win10/11 Z-Order placement (HWND_BOTTOM)...");
            let _ = SetWindowPos(
                our_hwnd,
                HWND_BOTTOM,
                0, 0, 0, 0,
                SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOSIZE | SWP_NOMOVE,
            );
        }

        debug!("[apply_injection] Expanding window to cover Virtual Screen...");
        let _ = SetWindowPos(
            our_hwnd,
            HWND::default(),
            detection.v_x,
            detection.v_y,
            detection.v_width,
            detection.v_height,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );

        let final_parent = GetParent(our_hwnd).unwrap_or_default();
        let is_visible = IsWindowVisible(our_hwnd).as_bool();

        info!(
            "[apply_injection] Injection Complete. State: Parent=0x{:X}, Visible={}, Rect=({}, {}, {}x{})",
            final_parent.0 as isize, is_visible, detection.v_x, detection.v_y, detection.v_width, detection.v_height
        );
    }
}

// ============================================================================
// Windows: Init
// ============================================================================

#[cfg(target_os = "windows")]
fn ensure_in_worker_w(window: &tauri::WebviewWindow) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;

    debug!("[ensure_in_worker_w] Forcing ignore_cursor_events on Tauri window...");
    let _ = window.set_ignore_cursor_events(true);

    let our_hwnd_raw = window.hwnd().map_err(|e| format!("{}", e))?;
    let our_hwnd = HWND(our_hwnd_raw.0 as *mut _);

    info!("[ensure_in_worker_w] Tauri Webview HWND: 0x{:X}", our_hwnd.0 as isize);

    let detection = detect_desktop()?;

    mouse_hook::set_webview_hwnd(our_hwnd.0 as isize);
    mouse_hook::set_target_parent_hwnd(detection.target_parent.0 as isize);
    if !detection.syslistview.is_invalid() {
        mouse_hook::set_syslistview_hwnd(detection.syslistview.0 as isize);
    }

    apply_injection(our_hwnd, &detection);

    debug!("[ensure_in_worker_w] Initializing Mouse Dispatch Proxy Window...");
    mouse_hook::init_dispatch_window();

    let (w, h) = (detection.v_width, detection.v_height);

    debug!("[ensure_in_worker_w] Spawning Wry Composition Controller polling thread...");
    std::thread::spawn(move || {
        use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

        let mut controller_found = false;
        for attempt in 1..=100 {
            let ptr = wry::get_last_composition_controller_ptr();
            if ptr != 0 {
                info!("[wry_poll] Found Composition Controller at 0x{:X} on attempt {}", ptr, attempt);
                mouse_hook::set_comp_controller_ptr(ptr);

                let dh = mouse_hook::get_dispatch_hwnd();
                if dh != 0 {
                    debug!("[wry_poll] Posting WM_MWP_SETBOUNDS_PUB ({}x{}) to dispatch window 0x{:X}", w, h, dh);
                    unsafe {
                        let _ = PostMessageW(
                            HWND(dh as *mut _),
                            mouse_hook::WM_MWP_SETBOUNDS_PUB,
                            WPARAM(w as usize),
                            LPARAM(h as isize),
                        );
                    }
                } else {
                    error!("[wry_poll] Dispatch window is 0! Cannot set bounds.");
                }
                controller_found = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        if !controller_found {
            error!("[wry_poll] CRITICAL: CompositionController not found after 5 seconds of polling! Wallpaper engine will fail to render inputs/bounds.");
        }
    });

    info!("[ensure_in_worker_w] Starting low-level mouse hook thread...");
    mouse_hook::start_hook_thread();

    Ok(())
}

// ============================================================================
// Windows: Mouse Hook
// ============================================================================

#[cfg(target_os = "windows")]
pub mod mouse_hook {
    use log::{debug, error, info, warn};
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, Ordering};
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

    const VK_NONE: i32 = 0x0;
    const VK_LBUTTON: i32 = 0x1;
    const VK_RBUTTON: i32 = 0x2;
    const VK_MBUTTON: i32 = 0x10;

    static WEBVIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static SYSLISTVIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static TARGET_PARENT_HWND: AtomicIsize = AtomicIsize::new(0);
    static COMP_CONTROLLER_PTR: AtomicIsize = AtomicIsize::new(0);
    static DRAG_VK: AtomicIsize = AtomicIsize::new(0);
    static DISPATCH_HWND: AtomicIsize = AtomicIsize::new(0);
    static CHROME_RWHH: AtomicIsize = AtomicIsize::new(0);
    static HOOK_HANDLE: AtomicIsize = AtomicIsize::new(0);

    const STATE_IDLE: u8 = 0;
    const STATE_DRAGGING: u8 = 1;
    const STATE_NATIVE: u8 = 2;
    static HOOK_STATE: AtomicU8 = AtomicU8::new(STATE_IDLE);

    static DIAG_POST_FAIL: AtomicBool = AtomicBool::new(true);

    const WM_MWP_MOUSE: u32 = 0x8000 + 42;
    pub const WM_MWP_SETBOUNDS_PUB: u32 = 0x8000 + 43;

    pub fn set_webview_hwnd(h: isize) { WEBVIEW_HWND.store(h, Ordering::SeqCst); }
    pub fn set_syslistview_hwnd(h: isize) { SYSLISTVIEW_HWND.store(h, Ordering::SeqCst); }
    pub fn set_target_parent_hwnd(h: isize) { TARGET_PARENT_HWND.store(h, Ordering::SeqCst); }
    pub fn get_syslistview_hwnd() -> isize { SYSLISTVIEW_HWND.load(Ordering::SeqCst) }
    pub fn set_comp_controller_ptr(p: isize) { COMP_CONTROLLER_PTR.store(p, Ordering::SeqCst); }
    pub fn get_comp_controller_ptr() -> isize { COMP_CONTROLLER_PTR.load(Ordering::SeqCst) }
    pub fn get_dispatch_hwnd() -> isize { DISPATCH_HWND.load(Ordering::SeqCst) }

    #[inline]
    unsafe fn post_mouse(kind: i32, vk: i32, data: u32, x: i32, y: i32) {
        let dh = DISPATCH_HWND.load(Ordering::Relaxed);
        if dh == 0 {
            if DIAG_POST_FAIL.swap(false, Ordering::Relaxed) {
                error!("[mouse_hook] post_mouse dropped: DISPATCH_HWND is 0");
            }
            return;
        }
        let wp = WPARAM((kind as u16 as usize) | ((vk as u16 as usize) << 16) | ((data as usize) << 32));
        let lp = LPARAM(((x as i16 as u16 as u32) | ((y as i16 as u16 as u32) << 16)) as isize);
        if let Err(e) = PostMessageW(HWND(dh as *mut _), WM_MWP_MOUSE, wp, lp) {
            if DIAG_POST_FAIL.swap(false, Ordering::Relaxed) {
                error!("[mouse_hook] PostMessageW FAILED to dispatch window 0x{:X}: {}", dh, e);
            }
        }
    }

    unsafe extern "system" fn dispatch_wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_MWP_SETBOUNDS_PUB {
            let w = wp.0 as i32;
            let h = lp.0 as i32;
            let ptr = get_comp_controller_ptr();
            debug!("[dispatch_wnd_proc] Received SETBOUNDS: {}x{} for CompPtr 0x{:X}", w, h, ptr);
            if ptr != 0 {
                let result = wry::set_controller_bounds_raw(ptr, w, h);
                info!("[dispatch_wnd_proc] SetBounds Applied. Result: {:?}", result);
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

            if ptr != 0 {
                if let Err(e) = wry::send_mouse_input_raw(ptr, kind, vk, data, x, y) {
                    error!("[dispatch_wnd_proc] WRY send_mouse_input_raw FAILED: {}", e);
                }
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
            match CreateWindowExW(
                WINDOW_EX_STYLE(0),
                cls,
                windows::core::w!(""),
                WINDOW_STYLE(0),
                0, 0, 0, 0,
                HWND_MESSAGE,
                None,
                None,
                None,
            ) {
                Ok(h) => {
                    DISPATCH_HWND.store(h.0 as isize, Ordering::SeqCst);
                    info!("[init_dispatch_window] Proxy window mapped at 0x{:X}", h.0 as isize);
                }
                Err(e) => {
                    error!("[init_dispatch_window] Failed to create proxy window: {}", e);
                }
            }
        }
    }

    #[inline]
    unsafe fn is_over_desktop(hwnd_under: HWND) -> bool {
        let tp = HWND(TARGET_PARENT_HWND.load(Ordering::Relaxed) as *mut _);
        let slv = HWND(SYSLISTVIEW_HWND.load(Ordering::Relaxed) as *mut _);
        let rwhh = HWND(CHROME_RWHH.load(Ordering::Relaxed) as *mut _);
        let wv = HWND(WEBVIEW_HWND.load(Ordering::Relaxed) as *mut _);

        // Fast path 1: Cached Chrome_RenderWidgetHostHWND
        if !rwhh.is_invalid() && hwnd_under == rwhh {
            return true;
        }
        // Fast path 2: Known roots
        if hwnd_under == tp || hwnd_under == slv || hwnd_under == wv {
            return true;
        }
        // Hierarchy check
        if IsChild(tp, hwnd_under).as_bool() || (!wv.is_invalid() && IsChild(wv, hwnd_under).as_bool()) {
            return true;
        }

        // Auto-discovery of Chrome_RenderWidgetHostHWND
        if rwhh.is_invalid() {
            let mut cls = [0u16; 40];
            let len = GetClassNameW(hwnd_under, &mut cls) as usize;
            if len == 31 {
                const EXPECTED: &[u8] = b"Chrome_RenderWidgetHostHWND";
                let matches = cls[..len].iter().zip(EXPECTED.iter()).all(|(&c, &e)| c == e as u16);
                if matches {
                    CHROME_RWHH.store(hwnd_under.0 as isize, Ordering::Relaxed);
                    info!("[is_over_desktop] Auto-discovered Chrome_RWHH at 0x{:X}", hwnd_under.0 as isize);
                    return true;
                }
            }
        }
        false
    }

    #[inline]
    unsafe fn is_mouse_over_desktop_icon(x: i32, y: i32) -> bool {
        use windows::core::VARIANT;
        use windows::Win32::Foundation::POINT;
        use windows::Win32::System::Variant::{VT_DISPATCH, VT_I4};
        use windows::Win32::UI::Accessibility::{AccessibleObjectFromPoint, IAccessible};

        let pt = POINT { x, y };
        let mut p_acc: Option<IAccessible> = None;
        let mut var_child = VARIANT::default();

        if AccessibleObjectFromPoint(pt, &mut p_acc, &mut var_child).is_ok() {
            if let Some(acc) = p_acc {
                match acc.accHitTest(x, y) {
                    Ok(hit) => {
                        let vt = hit.as_raw().Anonymous.Anonymous.vt;
                        if vt == VT_I4.0 as u16 {
                            let val = hit.as_raw().Anonymous.Anonymous.Anonymous.lVal;
                            val > 0
                        } else {
                            vt == VT_DISPATCH.0 as u16
                        }
                    }
                    Err(_) => false,
                }
            } else {
                false
            }
        } else {
            false
        }
    }

    #[inline]
    unsafe fn forward(msg: u32, info: &MSLLHOOKSTRUCT, cx: i32, cy: i32) {
        let x = cx;
        let y = cy;
        match msg {
            WM_MOUSEMOVE => post_mouse(MOUSE_MOVE, DRAG_VK.load(Ordering::Relaxed) as i32, 0, x, y),
            WM_LBUTTONDOWN => {
                DRAG_VK.store(VK_LBUTTON as isize, Ordering::Relaxed);
                post_mouse(MOUSE_LDOWN, VK_LBUTTON, 0, x, y);
            }
            WM_LBUTTONUP => {
                DRAG_VK.store(0, Ordering::Relaxed);
                post_mouse(MOUSE_LUP, VK_NONE, 0, x, y);
            }
            WM_RBUTTONDOWN => {
                DRAG_VK.store(VK_RBUTTON as isize, Ordering::Relaxed);
                post_mouse(MOUSE_RDOWN, VK_RBUTTON, 0, x, y);
            }
            WM_RBUTTONUP => {
                DRAG_VK.store(0, Ordering::Relaxed);
                post_mouse(MOUSE_RUP, VK_NONE, 0, x, y);
            }
            WM_MBUTTONDOWN => {
                DRAG_VK.store(VK_MBUTTON as isize, Ordering::Relaxed);
                post_mouse(MOUSE_MDOWN, VK_MBUTTON, 0, x, y);
            }
            WM_MBUTTONUP => {
                DRAG_VK.store(0, Ordering::Relaxed);
                post_mouse(MOUSE_MUP, VK_NONE, 0, x, y);
            }
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
            debug!("[start_hook_thread] Hook thread spawned. Initializing COM...");
            unsafe {
                use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
                if let Err(e) = CoInitializeEx(None, COINIT_APARTMENTTHREADED) {
                    error!("[start_hook_thread] COM Initialization Failed: {}", e);
                } else {
                    debug!("[start_hook_thread] COM Initialized (COINIT_APARTMENTTHREADED).");
                }
            }

            unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
                let hook_h = HHOOK(HOOK_HANDLE.load(Ordering::Relaxed) as *mut _);

                if code < 0 {
                    return CallNextHookEx(hook_h, code, wparam, lparam);
                }

                let wv_raw = WEBVIEW_HWND.load(Ordering::Relaxed);
                if wv_raw == 0 {
                    return CallNextHookEx(hook_h, code, wparam, lparam);
                }

                let info = *(lparam.0 as *const MSLLHOOKSTRUCT);
                let msg = wparam.0 as u32;
                let wv = HWND(wv_raw as *mut _);
                let is_down = msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN || msg == WM_MBUTTONDOWN;
                let is_up = msg == WM_LBUTTONUP || msg == WM_RBUTTONUP || msg == WM_MBUTTONUP;
                let state = HOOK_STATE.load(Ordering::Relaxed);

                // STATE_NATIVE (Interacting with a real Desktop Icon)
                if state == STATE_NATIVE {
                    if is_up {
                        debug!("[hook_proc] Mouse UP received. Transitioning from NATIVE to IDLE.");
                        HOOK_STATE.store(STATE_IDLE, Ordering::Relaxed);
                    }
                    return CallNextHookEx(hook_h, code, wparam, lparam);
                }

                // STATE_DRAGGING (Holding a click on the WebView, forward everything)
                if state == STATE_DRAGGING {
                    use windows::Win32::Graphics::Gdi::ScreenToClient;
                    let mut cp = info.pt;
                    let _ = ScreenToClient(wv, &mut cp);
                    forward(msg, &info, cp.x, cp.y);
                    if is_up {
                        debug!("[hook_proc] Mouse UP received. Transitioning from DRAGGING to IDLE.");
                        HOOK_STATE.store(STATE_IDLE, Ordering::Relaxed);
                    }
                    if msg == WM_MOUSEMOVE {
                        return CallNextHookEx(hook_h, code, wparam, lparam);
                    }
                    return LRESULT(1);
                }

                // STATE_IDLE
                let hwnd_under = WindowFromPoint(info.pt);
                if !is_over_desktop(hwnd_under) {
                    return CallNextHookEx(hook_h, code, wparam, lparam);
                }

                // Evaluate Desktop Icon Intersection
                if is_down {
                    if is_mouse_over_desktop_icon(info.pt.x, info.pt.y) {
                        debug!("[hook_proc] Icon HitTest TRUE at {},{}. Transitioning IDLE -> NATIVE", info.pt.x, info.pt.y);
                        HOOK_STATE.store(STATE_NATIVE, Ordering::Relaxed);
                        return CallNextHookEx(hook_h, code, wparam, lparam);
                    }
                    debug!("[hook_proc] Icon HitTest FALSE at {},{}. Transitioning IDLE -> DRAGGING", info.pt.x, info.pt.y);
                    HOOK_STATE.store(STATE_DRAGGING, Ordering::Relaxed);
                }

                // Forward Event Coordinates Translated to Client Area
                use windows::Win32::Graphics::Gdi::ScreenToClient;
                let mut cp = info.pt;
                let _ = ScreenToClient(wv, &mut cp);

                forward(msg, &info, cp.x, cp.y);

                // Allow mouse visual to move, but block programmatic click propagation
                if msg == WM_MOUSEMOVE {
                    return CallNextHookEx(hook_h, code, wparam, lparam);
                }
                LRESULT(1)
            }

            unsafe {
                debug!("[start_hook_thread] Installing WH_MOUSE_LL hook...");
                let hook_result = SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), None, 0);
                match &hook_result {
                    Ok(h) => {
                        HOOK_HANDLE.store(h.0 as isize, Ordering::SeqCst);
                        info!("[start_hook_thread] WH_MOUSE_LL Hook installed successfully: 0x{:X}", h.0 as isize);
                    }
                    Err(e) => {
                        error!("[start_hook_thread] WH_MOUSE_LL Hook FAILED: {}", e);
                    }
                }

                debug!("[start_hook_thread] Entering message loop for hook thread.");
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, HWND::default(), 0, 0).into() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                warn!("[start_hook_thread] Message loop exited unexpectedly.");
            }
        });
    }
}

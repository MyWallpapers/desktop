//! Window Layer — Desktop WebView injection + mouse forwarding.
//!
//! Windows: Injects WebView into Progman/WorkerW hierarchy. Low-level mouse hook
//!          intercepts events over the desktop and forwards them to WebView2 via
//!          SendMouseInput (composition mode).
//! macOS:   kCGDesktopWindowLevel behind desktop icons, ignores mouse events.

use log::{info, warn};
use std::sync::atomic::{AtomicBool, Ordering};

static ICONS_RESTORED: AtomicBool = AtomicBool::new(false);

// ============================================================================
// Entry Point
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
// Desktop Icons Visibility (Tauri command — called from frontend)
// ============================================================================

#[tauri::command]
pub fn set_desktop_icons_visible(visible: bool) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOW};
        use windows::Win32::Foundation::HWND;

        let slv = mouse_hook::get_syslistview_hwnd();
        if slv != 0 {
            unsafe { let _ = ShowWindow(HWND(slv as *mut _), if visible { SW_SHOW } else { SW_HIDE }); }
            info!("Desktop icons visibility: {}", visible);
        }
    }

    #[cfg(target_os = "macos")]
    {
        let val = if visible { "true" } else { "false" };
        let _ = std::process::Command::new("defaults")
            .args(["write", "com.apple.finder", "CreateDesktop", val])
            .output();
        let _ = std::process::Command::new("killall").arg("Finder").output();
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
            info!("Desktop icons restored on exit.");
        }
    }

    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("defaults")
            .args(["write", "com.apple.finder", "CreateDesktop", "true"])
            .output();
        let _ = std::process::Command::new("killall").arg("Finder").output();
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

        // Wake desktop (spawn WorkerW if needed)
        let mut msg_result: usize = 0;
        let _ = SendMessageTimeoutW(progman, 0x052C, WPARAM(0), LPARAM(0), SMTO_NORMAL, 1000, Some(&mut msg_result));
        let _ = SendMessageTimeoutW(progman, 0x052C, WPARAM(0x0D), LPARAM(0), SMTO_NORMAL, 1000, Some(&mut msg_result));
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

            // Legacy: SHELLDLL_DefView inside a WorkerW top-level window
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
    use windows::Win32::Foundation::{COLORREF, HWND};
    use windows::Win32::Graphics::Dwm::*;
    use windows::Win32::UI::WindowsAndMessaging::*;

    unsafe {
        if GetParent(our_hwnd) == Ok(detection.target_parent) { return; }

        // Strip all decoration
        let mut style = GetWindowLongW(our_hwnd, GWL_STYLE) as u32;
        style &= !(WS_THICKFRAME.0 | WS_CAPTION.0 | WS_SYSMENU.0
                  | WS_MAXIMIZEBOX.0 | WS_MINIMIZEBOX.0 | WS_POPUP.0);
        style |= WS_CHILD.0 | WS_VISIBLE.0;
        let _ = SetWindowLongW(our_hwnd, GWL_STYLE, style as i32);

        // Strip border artifacts, add layered + toolwindow
        let mut ex = GetWindowLongW(our_hwnd, GWL_EXSTYLE) as u32;
        ex &= !(WS_EX_CLIENTEDGE.0 | WS_EX_WINDOWEDGE.0
              | WS_EX_DLGMODALFRAME.0 | WS_EX_STATICEDGE.0);
        ex |= WS_EX_TOOLWINDOW.0 | WS_EX_LAYERED.0;
        let _ = SetWindowLongW(our_hwnd, GWL_EXSTYLE, ex as i32);

        let _ = SetLayeredWindowAttributes(our_hwnd, COLORREF(0), 255, LWA_ALPHA);

        // DWM: no rounded corners, no border color
        let corner: u32 = 1; // DWMWCP_DONOTROUND
        let _ = DwmSetWindowAttribute(our_hwnd, DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as _, std::mem::size_of::<u32>() as u32);
        let border: u32 = 0xFFFFFFFE; // DWMWA_COLOR_NONE
        let _ = DwmSetWindowAttribute(our_hwnd, DWMWA_BORDER_COLOR,
            &border as *const _ as _, std::mem::size_of::<u32>() as u32);

        // Reparent
        let _ = SetParent(our_hwnd, detection.target_parent);

        // Z-order
        if detection.is_24h2 {
            let _ = SetWindowPos(our_hwnd, detection.shell_view, 0, 0, 0, 0,
                SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOSIZE | SWP_NOMOVE | SWP_FRAMECHANGED);
            let _ = SetWindowPos(detection.os_workerw, our_hwnd, 0, 0, 0, 0,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOMOVE);
        } else {
            let _ = SetWindowPos(our_hwnd, HWND::default(), 0, 0, 0, 0,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOSIZE | SWP_NOMOVE | SWP_FRAMECHANGED);
        }

        // Size to parent
        let _ = SetWindowPos(our_hwnd, HWND::default(),
            0, 0, detection.parent_width, detection.parent_height,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);

        info!("Injection complete: {}x{}", detection.parent_width, detection.parent_height);
    }
}

#[cfg(target_os = "windows")]
fn ensure_in_worker_w(window: &tauri::WebviewWindow) -> Result<(), String> {
    use tauri::Manager;
    use windows::Win32::Foundation::HWND;

    let our_hwnd = window.hwnd().map_err(|e| format!("{}", e))?;
    let our_hwnd = HWND(our_hwnd.0 as *mut _);
    let detection = detect_desktop()?;

    mouse_hook::set_webview_hwnd(our_hwnd.0 as isize);
    mouse_hook::set_shell_view_hwnd(detection.shell_view.0 as isize);
    mouse_hook::set_target_parent_hwnd(detection.target_parent.0 as isize);
    if !detection.syslistview.is_invalid() {
        mouse_hook::set_syslistview_hwnd(detection.syslistview.0 as isize);
        info!("SysListView32: 0x{:X}", detection.syslistview.0 as isize);
    }
    mouse_hook::set_app_handle(window.app_handle().clone());

    apply_injection(our_hwnd, &detection);

    // WebView2 bounds
    let (w, h) = (detection.parent_width, detection.parent_height);
    let comp_ptr = wry::get_last_composition_controller_ptr();
    if comp_ptr != 0 {
        mouse_hook::set_comp_controller_ptr(comp_ptr);
        unsafe {
            match wry::set_controller_bounds_raw(comp_ptr, w, h) {
                Ok(()) => info!("WebView2 bounds: {}x{}", w, h),
                Err(e) => warn!("SetBounds failed: {}", e),
            }
        }
    } else {
        std::thread::spawn(move || {
            for _ in 0..60 {
                std::thread::sleep(std::time::Duration::from_millis(50));
                let ptr = wry::get_last_composition_controller_ptr();
                if ptr != 0 {
                    mouse_hook::set_comp_controller_ptr(ptr);
                    unsafe { let _ = wry::set_controller_bounds_raw(ptr, w, h); }
                    return;
                }
            }
            log::warn!("CompositionController not found after 3s");
        });
    }

    mouse_hook::init_dispatch_window();

    info!("{} engine active.", if detection.is_24h2 { "24H2" } else { "Legacy" });
    mouse_hook::start_hook_thread();
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn try_refresh_desktop() -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::IsWindow;

    let wv = mouse_hook::get_webview_hwnd();
    if wv == 0 { return false; }
    let hwnd = HWND(wv as *mut _);
    unsafe { if !IsWindow(hwnd).as_bool() { return false; } }

    match detect_desktop() {
        Ok(d) => {
            mouse_hook::set_shell_view_hwnd(d.shell_view.0 as isize);
            mouse_hook::set_target_parent_hwnd(d.target_parent.0 as isize);
            if !d.syslistview.is_invalid() {
                mouse_hook::set_syslistview_hwnd(d.syslistview.0 as isize);
            }
            apply_injection(hwnd, &d);
            let ptr = mouse_hook::get_comp_controller_ptr();
            if ptr != 0 {
                unsafe { let _ = wry::set_controller_bounds_raw(ptr, d.parent_width, d.parent_height); }
            }
            info!("Desktop recovered.");
            true
        }
        Err(e) => { warn!("Recovery failed: {}", e); false }
    }
}

// ============================================================================
// Windows: Mouse Hook — minimal composition-mode forwarding
// ============================================================================

#[cfg(target_os = "windows")]
pub mod mouse_hook {
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, AtomicU8, Ordering};
    use std::sync::OnceLock;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::*;

    // WebView2 mouse event constants
    const MOUSE_MOVE: i32     = 0x0200;
    const MOUSE_LDOWN: i32    = 0x0201;
    const MOUSE_LUP: i32      = 0x0202;
    const MOUSE_RDOWN: i32    = 0x0204;
    const MOUSE_RUP: i32      = 0x0205;
    const MOUSE_MDOWN: i32    = 0x0207;
    const MOUSE_MUP: i32      = 0x0208;
    const MOUSE_WHEEL: i32    = 0x020A;
    const MOUSE_HWHEEL: i32   = 0x020E;
    const MOUSE_LEAVE: i32    = 0x02A3;

    const VK_NONE: i32    = 0x0;
    const VK_LBUTTON: i32 = 0x1;
    const VK_RBUTTON: i32 = 0x2;
    const VK_MBUTTON: i32 = 0x10;

    // ── State ──
    static WEBVIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static SYSLISTVIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static SHELL_VIEW_HWND: AtomicIsize = AtomicIsize::new(0);
    static TARGET_PARENT_HWND: AtomicIsize = AtomicIsize::new(0);
    static EXPLORER_PID: AtomicU32 = AtomicU32::new(0);
    static COMP_CONTROLLER_PTR: AtomicIsize = AtomicIsize::new(0);
    static DRAG_VK: AtomicIsize = AtomicIsize::new(0);
    static APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();

    const STATE_IDLE: u8 = 0;
    const STATE_DRAGGING: u8 = 1;
    static HOOK_STATE: AtomicU8 = AtomicU8::new(STATE_IDLE);
    static WAS_OVER_DESKTOP: AtomicBool = AtomicBool::new(false);

    // ── Dispatch window (UI thread) ──
    const WM_MWP_MOUSE: u32 = 0x8000 + 42;
    const WM_MWP_MOVE: u32 = 0x8000 + 43;
    static DISPATCH_HWND: AtomicIsize = AtomicIsize::new(0);
    static PENDING_MOVE_X: AtomicI32 = AtomicI32::new(0);
    static PENDING_MOVE_Y: AtomicI32 = AtomicI32::new(0);
    static MOVE_QUEUED: AtomicBool = AtomicBool::new(false);

    // ── HWND cache ──
    static CACHED_HWND: AtomicIsize = AtomicIsize::new(0);
    static CACHED_RESULT: AtomicBool = AtomicBool::new(false);

    // ── Public API ──
    pub fn set_webview_hwnd(h: isize)     { WEBVIEW_HWND.store(h, Ordering::SeqCst); }
    pub fn set_syslistview_hwnd(h: isize) { SYSLISTVIEW_HWND.store(h, Ordering::SeqCst); }
    pub fn set_shell_view_hwnd(h: isize)  { SHELL_VIEW_HWND.store(h, Ordering::SeqCst); }
    pub fn set_target_parent_hwnd(h: isize) {
        TARGET_PARENT_HWND.store(h, Ordering::SeqCst);
        if h != 0 {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(HWND(h as *mut _), Some(&mut pid)); }
            EXPLORER_PID.store(pid, Ordering::SeqCst);
        }
    }
    pub fn get_webview_hwnd() -> isize      { WEBVIEW_HWND.load(Ordering::SeqCst) }
    pub fn get_syslistview_hwnd() -> isize  { SYSLISTVIEW_HWND.load(Ordering::SeqCst) }
    pub fn set_app_handle(h: tauri::AppHandle) { let _ = APP_HANDLE.set(h); }
    pub fn set_comp_controller_ptr(p: isize) { COMP_CONTROLLER_PTR.store(p, Ordering::SeqCst); }
    pub fn get_comp_controller_ptr() -> isize { COMP_CONTROLLER_PTR.load(Ordering::SeqCst) }

    // ── Dispatch helpers ──

    #[inline]
    unsafe fn post_mouse(kind: i32, vk: i32, data: u32, x: i32, y: i32) {
        let dh = DISPATCH_HWND.load(Ordering::Relaxed);
        if dh == 0 { return; }
        let wp = WPARAM((kind as u16 as usize) | ((vk as u16 as usize) << 16) | ((data as usize) << 32));
        let lp = LPARAM(((x as i16 as u16 as u32) | ((y as i16 as u16 as u32) << 16)) as isize);
        let _ = PostMessageW(HWND(dh as *mut _), WM_MWP_MOUSE, wp, lp);
    }

    #[inline]
    unsafe fn post_move(x: i32, y: i32) {
        PENDING_MOVE_X.store(x, Ordering::Relaxed);
        PENDING_MOVE_Y.store(y, Ordering::Relaxed);
        if !MOVE_QUEUED.swap(true, Ordering::Release) {
            let dh = DISPATCH_HWND.load(Ordering::Relaxed);
            if dh != 0 { let _ = PostMessageW(HWND(dh as *mut _), WM_MWP_MOVE, WPARAM(0), LPARAM(0)); }
        }
    }

    unsafe extern "system" fn dispatch_wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_MWP_MOVE {
            MOVE_QUEUED.store(false, Ordering::Release);
            let x = PENDING_MOVE_X.load(Ordering::Relaxed);
            let y = PENDING_MOVE_Y.load(Ordering::Relaxed);
            let vk = DRAG_VK.load(Ordering::Relaxed) as i32;
            let ptr = get_comp_controller_ptr();
            if ptr != 0 { let _ = wry::send_mouse_input_raw(ptr, MOUSE_MOVE, vk, 0, x, y); }
            return LRESULT(0);
        }
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

    // ── Desktop detection ──

    #[inline]
    unsafe fn is_over_desktop(hwnd_under: HWND, wv: HWND) -> bool {
        let cached = CACHED_HWND.load(Ordering::Relaxed);
        if hwnd_under.0 as isize == cached && cached != 0 {
            return CACHED_RESULT.load(Ordering::Relaxed);
        }

        let slv = HWND(get_syslistview_hwnd() as *mut _);
        let sv = HWND(SHELL_VIEW_HWND.load(Ordering::Relaxed) as *mut _);
        let tp = HWND(TARGET_PARENT_HWND.load(Ordering::Relaxed) as *mut _);

        let mut hit = hwnd_under == slv || hwnd_under == wv || hwnd_under == sv || hwnd_under == tp
            || IsChild(wv, hwnd_under).as_bool() || IsChild(tp, hwnd_under).as_bool();

        // Overlay detection (Win11 Widgets/Copilot/Search — transparent non-foreground windows)
        if !hit {
            let fg = GetForegroundWindow();
            if hwnd_under != fg {
                let root = GetAncestor(hwnd_under, GA_ROOT);
                if root != fg && !root.is_invalid() {
                    let ex = GetWindowLongW(root, GWL_EXSTYLE) as u32;
                    let overlay = (ex & WS_EX_NOACTIVATE.0) != 0
                        || (ex & WS_EX_TOOLWINDOW.0) != 0
                        || ((ex & WS_EX_LAYERED.0) != 0 && (ex & WS_EX_APPWINDOW.0) == 0);
                    if overlay {
                        let epid = EXPLORER_PID.load(Ordering::Relaxed);
                        let mut opid = 0u32;
                        GetWindowThreadProcessId(root, Some(&mut opid));
                        if opid != epid && opid != 0 { hit = true; }
                    }
                }
            }
        }

        CACHED_HWND.store(hwnd_under.0 as isize, Ordering::Relaxed);
        CACHED_RESULT.store(hit, Ordering::Relaxed);
        hit
    }

    // ── Forward to WebView ──

    #[inline]
    unsafe fn forward(msg: u32, info: &MSLLHOOKSTRUCT, cx: i32, cy: i32) {
        let x = cx.max(0);
        let y = cy.max(0);
        match msg {
            WM_MOUSEMOVE  => post_move(x, y),
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

    // ── Handle validation (for Explorer restart recovery) ──

    pub fn validate_handles() -> bool {
        let wv = WEBVIEW_HWND.load(Ordering::SeqCst);
        if wv == 0 { return true; }
        unsafe {
            if !IsWindow(HWND(wv as *mut _)).as_bool() { return false; }
            let slv = SYSLISTVIEW_HWND.load(Ordering::SeqCst);
            if slv != 0 && !IsWindow(HWND(slv as *mut _)).as_bool() { return false; }
            true
        }
    }

    // ── Hook thread ──

    pub fn start_hook_thread() {
        std::thread::spawn(|| {
            unsafe {
                use windows::Win32::System::Threading::{SetThreadPriority, GetCurrentThread, THREAD_PRIORITY_HIGHEST};
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
                use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
                let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            }

            unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
                if code < 0 {
                    return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                }

                let wv_raw = WEBVIEW_HWND.load(Ordering::Relaxed);
                if wv_raw == 0 {
                    return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                }

                let info = *(lparam.0 as *const MSLLHOOKSTRUCT);
                let pt = info.pt;
                let msg = wparam.0 as u32;
                let wv = HWND(wv_raw as *mut _);
                let is_down = msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN || msg == WM_MBUTTONDOWN;
                let is_up = msg == WM_LBUTTONUP || msg == WM_RBUTTONUP || msg == WM_MBUTTONUP;

                // ── DRAGGING: forward everything to WebView until button up ──
                if HOOK_STATE.load(Ordering::Relaxed) == STATE_DRAGGING {
                    use windows::Win32::Graphics::Gdi::ScreenToClient;
                    let mut cp = pt;
                    let _ = ScreenToClient(wv, &mut cp);
                    forward(msg, &info, cp.x, cp.y);
                    if is_up { HOOK_STATE.store(STATE_IDLE, Ordering::Relaxed); }
                    // Consume clicks/scroll, let moves through for cursor updates
                    if msg == WM_MOUSEMOVE {
                        return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                    }
                    return LRESULT(1);
                }

                // ── IDLE: check if over desktop ──
                let hwnd_under = WindowFromPoint(pt);
                if !is_over_desktop(hwnd_under, wv) {
                    if WAS_OVER_DESKTOP.swap(false, Ordering::Relaxed) {
                        post_mouse(MOUSE_LEAVE, VK_NONE, 0, 0, 0);
                    }
                    return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                }

                // Over desktop — forward to WebView
                WAS_OVER_DESKTOP.store(true, Ordering::Relaxed);
                use windows::Win32::Graphics::Gdi::ScreenToClient;
                let mut cp = pt;
                let _ = ScreenToClient(wv, &mut cp);
                forward(msg, &info, cp.x, cp.y);

                if is_down {
                    HOOK_STATE.store(STATE_DRAGGING, Ordering::Relaxed);
                }

                // Consume clicks/scroll, let moves through
                if msg == WM_MOUSEMOVE {
                    return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
                }
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

        static APP: OnceLock<AppHandle> = OnceLock::new();
        static WAS_VISIBLE: AtomicBool = AtomicBool::new(true);
        let _ = APP.set(app);

        std::thread::spawn(|| {
            use windows::Win32::UI::Accessibility::*;
            use windows::Win32::UI::WindowsAndMessaging::*;
            use windows::Win32::Graphics::Gdi::*;
            use windows::Win32::Foundation::*;

            unsafe fn check() {
                let wv = super::mouse_hook::get_webview_hwnd();
                if wv == 0 { return; }

                let fg = GetForegroundWindow();
                let desk = GetDesktopWindow();

                let visible = if fg == desk || fg.is_invalid() {
                    true
                } else {
                    let hm_fg = MonitorFromWindow(fg, MONITOR_DEFAULTTOPRIMARY);
                    let hm_wv = MonitorFromWindow(HWND(wv as *mut _), MONITOR_DEFAULTTOPRIMARY);
                    if hm_fg != hm_wv { true }
                    else {
                        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
                        if GetMonitorInfoW(hm_fg, &mut mi).as_bool() {
                            let mut r = RECT::default();
                            let _ = GetWindowRect(fg, &mut r);
                            !(r.left <= mi.rcMonitor.left && r.top <= mi.rcMonitor.top
                                && r.right >= mi.rcMonitor.right && r.bottom >= mi.rcMonitor.bottom)
                        } else { true }
                    }
                };

                let was = WAS_VISIBLE.swap(visible, Ordering::Relaxed);
                if visible != was {
                    if let Some(a) = APP.get() { let _ = a.emit("wallpaper-visibility", visible); }
                }
            }

            unsafe extern "system" fn on_event(
                _: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32,
            ) { check(); }

            unsafe {
                let _ = SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND,
                    None, Some(on_event), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
                let _ = SetWinEventHook(EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZEEND,
                    None, Some(on_event), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);
                let _ = SetWinEventHook(EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND,
                    None, Some(on_event), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS);

                let _ = SetTimer(HWND::default(), 1, 10_000, None);
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, HWND::default(), 0, 0).into() {
                    if msg.message == WM_TIMER && msg.wParam.0 == 1 {
                        if !super::mouse_hook::validate_handles() {
                            log::warn!("Stale handles — recovering...");
                            if super::try_refresh_desktop() { log::info!("Desktop recovered."); }
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
    pub fn start(_app: AppHandle) {}
}

// ============================================================================
// macOS
// ============================================================================

#[cfg(target_os = "macos")]
fn setup_macos_desktop(window: &tauri::WebviewWindow) -> Result<(), String> {
    use tauri::Manager;
    let ns = window.ns_window().map_err(|e| e.to_string())? as *mut objc::runtime::Object;

    use objc::{msg_send, sel, sel_impl, class};
    unsafe {
        let _: () = msg_send![ns, setLevel: -2147483623_isize];
        let _: () = msg_send![ns, setCollectionBehavior: 81_usize];
        let _: () = msg_send![ns, setIgnoresMouseEvents: true];

        let pi: *mut objc::runtime::Object = msg_send![class!(NSProcessInfo), processInfo];
        let reason: *mut objc::runtime::Object = msg_send![class!(NSString), alloc];
        let reason: *mut objc::runtime::Object = msg_send![reason,
            initWithBytes:b"Wallpaper Animation\0".as_ptr() length:19_usize encoding:4_usize];
        let _: *mut objc::runtime::Object = msg_send![pi, beginActivityWithOptions:0x00FFFFFF_u64 reason:reason];
    }

    info!("macOS desktop layer ready.");
    Ok(())
}

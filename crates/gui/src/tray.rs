use crate::logger::log_line;
use crate::storage::TrayStyle;
use crate::theme::build_app_icon;
use eframe::egui::{self, Pos2};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrayCmd {
    Show,
    ToggleConnect,
    TogglePopover(f64, f64),
    Quit,
}

pub struct TrayHandle {
    tray: TrayIcon,
    use_panel: Arc<AtomicBool>,
}

impl TrayHandle {
    /// Switch between floating popover panel and native OS tray menu without restarting.
    pub fn set_style(&self, style: TrayStyle) {
        let is_panel = style == TrayStyle::Panel;
        self.use_panel.store(is_panel, Ordering::Relaxed);
        self.tray.set_show_menu_on_left_click(false);
        if is_panel {
            self.tray.set_menu(None);
        } else {
            self.tray.set_menu(Some(Box::new(build_native_menu())));
        }
    }
}

fn build_native_menu() -> Menu {
    let item_show = MenuItem::with_id("show", "显示主窗口", true, None);
    let item_toggle = MenuItem::with_id("toggle", "连接 / 断开加速", true, None);
    let item_quit = MenuItem::with_id("quit", "退出 GameAccel", true, None);

    let menu = Menu::new();
    let _ = menu.append(&item_show);
    let _ = menu.append(&item_toggle);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&item_quit);
    menu
}

#[cfg(target_os = "windows")]
static SAVED_WINDOW_RECT: std::sync::Mutex<Option<(i32, i32, i32, i32)>> =
    std::sync::Mutex::new(None);

/// Lifecycle stages for borderless popup viewports (floating card context menu & tray panel).
///
/// Why this exists: on Windows, `winit 0.30` unconditionally gives every top-level window
/// `WS_CAPTION | WS_BORDER | WS_SYSMENU` and re-applies those flags whenever
/// `ViewportCommand::Visible(true)` runs, causing DWM to briefly composite the native
/// `— □ ×` titlebar buttons. By creating the viewport with `.with_visible(false)` at an
/// off-screen coordinate, stripping `WS_CAPTION` -> `WS_POPUP` and disabling DWM non-client
/// rendering/transitions while hidden, letting `eframe` swap a rendered OpenGL frame, and
/// finally revealing via Win32 `SetWindowPos(..., SWP_SHOWWINDOW)` directly (never touching
/// `ViewportCommand::Visible`), `winit` never restores `WS_CAPTION` and zero native chrome
/// can ever flash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PopupStage {
    #[default]
    Closed,
    HiddenSetup,
    HiddenPaint,
    Reveal,
    VisibleWaitRelease,
    VisibleArmed,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PopupRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub corner_diam: i32,
}

/// Strip the Win32 native frame from a top-level window so Windows can never composite the
/// `— □ ×` titlebar buttons over it.
///
/// Why this is needed: `winit 0.30` creates *every* top-level window with
/// `WS_CAPTION | WS_BORDER | WS_SYSMENU` (`window_state.rs::to_window_styles`) and
/// `with_decorations(false)` never clears those bits — it only expands the client rect in
/// `WM_NCCALCSIZE`, i.e. the caption is merely *covered* by our GL surface. Any
/// `WM_NCACTIVATE` (a context menu stealing focus, a `SetForegroundWindow`, …) makes the
/// compositor repaint that non-client area, so the buttons leak through for a frame or two.
///
/// Three layers, because each one alone still leaks in some scenario:
/// 1. `WS_POPUP` instead of an overlapped style — there is no caption to paint.
/// 2. `DWMNCRENDERING_POLICY = DISABLED` — DWM never draws a non-client area, even after
///    `winit` re-applies the styles it still believes in from its internal bookkeeping.
/// 3. `TRANSITIONS_FORCEDISABLED` — no activate/deactivate fade that would expose a stale
///    frame during the switch-over.
///
/// Idempotent and cheap: `GWL_STYLE`/`GWL_EXSTYLE` are read on every call and
/// `SetWindowLongPtrW` + `SWP_FRAMECHANGED` are only issued when something actually
/// differs, so this is safe to run once per frame.
pub(crate) fn strip_native_chrome(hwnd: isize) {
    #[cfg(target_os = "windows")]
    {
        use std::ffi::c_void;

        if hwnd == 0 {
            return;
        }

        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
            fn SetWindowLongPtrW(hwnd: isize, index: i32, new_long: isize) -> isize;
            fn SetWindowPos(
                hwnd: isize,
                insert_after: isize,
                x: i32,
                y: i32,
                cx: i32,
                cy: i32,
                flags: u32,
            ) -> i32;
        }
        #[link(name = "dwmapi")]
        unsafe extern "system" {
            fn DwmSetWindowAttribute(
                hwnd: *mut c_void,
                dw_attribute: u32,
                pv_attribute: *const c_void,
                cb_attribute: u32,
            ) -> i32;
        }

        const GWL_STYLE: i32 = -16;
        const GWL_EXSTYLE: i32 = -20;

        const WS_POPUP: isize = 0x8000_0000u32 as i32 as isize;
        const WS_CLIPCHILDREN: isize = 0x0200_0000;
        const WS_CLIPSIBLINGS: isize = 0x0400_0000;
        const WS_CAPTION: isize = 0x00C0_0000;
        const WS_BORDER: isize = 0x0080_0000;
        const WS_SYSMENU: isize = 0x0008_0000;
        const WS_THICKFRAME: isize = 0x0004_0000;
        const WS_MINIMIZEBOX: isize = 0x0002_0000;
        const WS_MAXIMIZEBOX: isize = 0x0001_0000;

        const WS_EX_DLGMODALFRAME: isize = 0x0000_0001;
        const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
        const WS_EX_WINDOWEDGE: isize = 0x0000_0100;
        const WS_EX_CLIENTEDGE: isize = 0x0000_0200;
        const WS_EX_STATICEDGE: isize = 0x0002_0000;
        const WS_EX_APPWINDOW: isize = 0x0004_0000;

        const SWP_NOSIZE: u32 = 0x0001;
        const SWP_NOMOVE: u32 = 0x0002;
        const SWP_NOZORDER: u32 = 0x0004;
        const SWP_NOACTIVATE: u32 = 0x0010;
        const SWP_FRAMECHANGED: u32 = 0x0020;

        const DWMWA_NCRENDERING_POLICY: u32 = 2;
        const DWMWA_TRANSITIONS_FORCEDISABLED: u32 = 3;
        const DWMNCRP_DISABLED: u32 = 1;

        unsafe {
            // The DWM attributes stick to the HWND, so they only need to be re-applied when the
            // window handle changes (viewport recreation, monitor change, …). Re-sending them
            // every frame would make DWM re-evaluate the window on each compositor pass.
            static DWM_READY: std::sync::atomic::AtomicIsize =
                std::sync::atomic::AtomicIsize::new(0);
            if DWM_READY.load(std::sync::atomic::Ordering::Relaxed) != hwnd {
                let nc_disabled: u32 = DWMNCRP_DISABLED;
                let _ = DwmSetWindowAttribute(
                    hwnd as *mut c_void,
                    DWMWA_NCRENDERING_POLICY,
                    (&nc_disabled as *const u32).cast::<c_void>(),
                    std::mem::size_of::<u32>() as u32,
                );
                let no_transitions: i32 = 1;
                let _ = DwmSetWindowAttribute(
                    hwnd as *mut c_void,
                    DWMWA_TRANSITIONS_FORCEDISABLED,
                    (&no_transitions as *const i32).cast::<c_void>(),
                    std::mem::size_of::<i32>() as u32,
                );
                DWM_READY.store(hwnd, std::sync::atomic::Ordering::Relaxed);
            }

            let mut changed = false;

            let cur_style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            let new_style = (cur_style
                & !(WS_CAPTION
                    | WS_BORDER
                    | WS_SYSMENU
                    | WS_THICKFRAME
                    | WS_MINIMIZEBOX
                    | WS_MAXIMIZEBOX))
                | WS_POPUP
                | WS_CLIPCHILDREN
                | WS_CLIPSIBLINGS;
            if new_style != cur_style {
                let _ = SetWindowLongPtrW(hwnd, GWL_STYLE, new_style);
                changed = true;
            }

            // Keep whatever extended bits the caller already relies on (TOPMOST, LAYERED…),
            // only normalise the frame-ish ones that can draw a border or a taskbar entry.
            let cur_ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let new_ex = (cur_ex
                & !(WS_EX_DLGMODALFRAME
                    | WS_EX_WINDOWEDGE
                    | WS_EX_CLIENTEDGE
                    | WS_EX_STATICEDGE
                    | WS_EX_APPWINDOW))
                | WS_EX_TOOLWINDOW;
            if new_ex != cur_ex {
                let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex);
                changed = true;
            }

            if changed {
                let _ = SetWindowPos(
                    hwnd,
                    0,
                    0,
                    0,
                    0,
                    0,
                    SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
                crate::logger::log_line(
                    "INFO",
                    &format!(
                        "已剥离窗口原生边框 (hwnd=0x{hwnd:X}, style 0x{cur_style:X} -> 0x{new_style:X})"
                    ),
                );
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = hwnd;
    }
}

/// Hide the window, strip `WS_CAPTION`/`WS_SYSMENU`, switch to `WS_POPUP`, disable DWM
/// non-client rendering and window animations, apply a rounded `HRGN`, and position at `rect`
/// while keeping the window hidden so OpenGL can render and swap cleanly before a single
/// pixel is shown.
pub(crate) fn prepare_popup_window(title: &str, rect: PopupRect) {
    #[cfg(target_os = "windows")]
    {
        use std::ffi::c_void;

        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
            fn SetWindowLongPtrW(hwnd: isize, index: i32, new_long: isize) -> isize;
            fn SetWindowRgn(hwnd: isize, hrgn: isize, redraw: i32) -> i32;
            fn SetWindowPos(
                hwnd: isize,
                insert_after: isize,
                x: i32,
                y: i32,
                cx: i32,
                cy: i32,
                flags: u32,
            ) -> i32;
        }
        #[link(name = "gdi32")]
        unsafe extern "system" {
            fn CreateRoundRectRgn(x1: i32, y1: i32, x2: i32, y2: i32, w: i32, h: i32) -> isize;
        }
        #[link(name = "dwmapi")]
        unsafe extern "system" {
            fn DwmSetWindowAttribute(
                hwnd: *mut c_void,
                dw_attribute: u32,
                pv_attribute: *const c_void,
                cb_attribute: u32,
            ) -> i32;
        }

        let hwnd = find_own_window(title);
        if hwnd == 0 || rect.w <= 0 || rect.h <= 0 {
            return;
        }

        const GWLP_HWNDPARENT: i32 = -8;
        const GWL_STYLE: i32 = -16;

        const WS_VISIBLE: isize = 0x1000_0000;

        const HWND_TOPMOST: isize = -1;
        const SWP_NOACTIVATE: u32 = 0x0010;
        const SWP_FRAMECHANGED: u32 = 0x0020;
        const SWP_HIDEWINDOW: u32 = 0x0080;

        const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
        const DWMWCP_ROUND: u32 = 2;

        unsafe {
            let corner_pref: u32 = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd as *mut c_void,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                (&corner_pref as *const u32).cast::<c_void>(),
                std::mem::size_of::<u32>() as u32,
            );

            if GetWindowLongPtrW(hwnd, GWLP_HWNDPARENT) != 0 {
                let _ = SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, 0);
            }

            // Drop `WS_CAPTION`/`WS_SYSMENU` & co. through the shared helper so the popup and
            // the floating card can never drift apart again.
            strip_native_chrome(hwnd);
            let cur_style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            let new_style = cur_style & !WS_VISIBLE;
            if new_style != cur_style {
                let _ = SetWindowLongPtrW(hwnd, GWL_STYLE, new_style);
            }

            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_HIDEWINDOW,
            );

            let diam = rect.corner_diam.max(8);
            let hrgn = CreateRoundRectRgn(0, 0, rect.w + 1, rect.h + 1, diam, diam);
            if hrgn != 0 {
                let _ = SetWindowRgn(hwnd, hrgn, 0);
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (title, rect);
    }
}

/// Reveal an already-painted `WS_POPUP` popup window directly via Win32 and focus it.
pub(crate) fn reveal_popup_window(title: &str, rect: PopupRect, vctx: &egui::Context, pos: Pos2) {
    #[cfg(target_os = "windows")]
    {
        let _ = (vctx, pos);
        #[link(name = "user32")]
        unsafe extern "system" {
            fn SetWindowRgn(hwnd: isize, hrgn: isize, redraw: i32) -> i32;
            fn SetWindowPos(
                hwnd: isize,
                insert_after: isize,
                x: i32,
                y: i32,
                cx: i32,
                cy: i32,
                flags: u32,
            ) -> i32;
            fn ShowWindow(hwnd: isize, n_cmd_show: i32) -> i32;
            fn SetForegroundWindow(hwnd: isize) -> i32;
            fn SetFocus(hwnd: isize) -> isize;
        }
        #[link(name = "gdi32")]
        unsafe extern "system" {
            fn CreateRoundRectRgn(x1: i32, y1: i32, x2: i32, y2: i32, w: i32, h: i32) -> isize;
        }

        let hwnd = find_own_window(title);
        if hwnd == 0 || rect.w <= 0 || rect.h <= 0 {
            return;
        }

        const HWND_TOPMOST: isize = -1;
        const SWP_SHOWWINDOW: u32 = 0x0040;
        const SW_SHOWNORMAL: i32 = 1;

        unsafe {
            let diam = rect.corner_diam.max(8);
            let hrgn = CreateRoundRectRgn(0, 0, rect.w + 1, rect.h + 1, diam, diam);
            if hrgn != 0 {
                let _ = SetWindowRgn(hwnd, hrgn, 1);
            }
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                SWP_SHOWWINDOW,
            );
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
            let _ = SetForegroundWindow(hwnd);
            let _ = SetFocus(hwnd);
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (title, rect);
        vctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
        vctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        vctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }
}

/// Immediately hide a popup window via `SW_HIDE` before eframe drops its viewport.
pub(crate) fn hide_popup_window(title: &str) {
    #[cfg(target_os = "windows")]
    {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn ShowWindow(hwnd: isize, n_cmd_show: i32) -> i32;
        }
        const SW_HIDE: i32 = 0;
        let hwnd = find_own_window(title);
        if hwnd != 0 {
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = title;
    }
}

pub(crate) fn any_mouse_button_down() -> bool {
    #[cfg(target_os = "windows")]
    {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetAsyncKeyState(v_key: i32) -> i16;
        }
        const VK_LBUTTON: i32 = 0x01;
        const VK_RBUTTON: i32 = 0x02;
        const VK_MBUTTON: i32 = 0x04;
        unsafe {
            GetAsyncKeyState(VK_LBUTTON) < 0
                || GetAsyncKeyState(VK_RBUTTON) < 0
                || GetAsyncKeyState(VK_MBUTTON) < 0
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// Returns `true` when the popup lost foreground focus or the user clicked outside its bounds.
pub(crate) fn is_popup_dismissed(title: &str, ever_focused: &mut bool) -> bool {
    #[cfg(target_os = "windows")]
    {
        #[repr(C)]
        struct Point {
            x: i32,
            y: i32,
        }
        #[repr(C)]
        struct WinRect {
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
        }
        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetForegroundWindow() -> *mut std::ffi::c_void;
            fn GetCursorPos(lp_point: *mut Point) -> i32;
            fn GetWindowRect(hwnd: isize, rect: *mut WinRect) -> i32;
        }

        let hwnd = find_own_window(title);
        if hwnd == 0 {
            return false;
        }
        unsafe {
            let fg = GetForegroundWindow() as isize;
            if fg == hwnd {
                *ever_focused = true;
            } else if *ever_focused {
                return true;
            }

            if any_mouse_button_down() {
                let mut pt = Point { x: 0, y: 0 };
                let mut rc = WinRect {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                if GetCursorPos(&mut pt) != 0
                    && GetWindowRect(hwnd, &mut rc) != 0
                    && (pt.x < rc.left || pt.x >= rc.right || pt.y < rc.top || pt.y >= rc.bottom)
                {
                    return true;
                }
            }
        }
        false
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (title, ever_focused);
        false
    }
}

/// Find the top-level window belonging to the current process with the exact `title`.
/// Scoping by PID prevents interfering with another running instance of GameAccel.
#[cfg(target_os = "windows")]
pub(crate) fn find_own_window(title: &str) -> isize {
    use std::ffi::c_void;
    type Hwnd = *mut c_void;
    type EnumProc = unsafe extern "system" fn(Hwnd, isize) -> i32;

    struct SearchCtx<'a> {
        target_pid: u32,
        target_title: &'a str,
        found: isize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcessId() -> u32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumWindows(cb: EnumProc, lparam: isize) -> i32;
        fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
        fn GetWindowTextW(hwnd: Hwnd, buf: *mut u16, max_count: i32) -> i32;
    }

    unsafe extern "system" fn enum_cb(hwnd: Hwnd, lparam: isize) -> i32 {
        unsafe {
            let ctx = &mut *(lparam as *mut SearchCtx<'_>);
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == ctx.target_pid {
                let mut buf = [0u16; 256];
                let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
                if len > 0 {
                    let text = String::from_utf16_lossy(&buf[..len as usize]);
                    if text == ctx.target_title {
                        ctx.found = hwnd as isize;
                        return 0;
                    }
                }
            }
        }
        1
    }

    unsafe {
        let mut ctx = SearchCtx {
            target_pid: GetCurrentProcessId(),
            target_title: title,
            found: 0,
        };
        let _ = EnumWindows(enum_cb, (&mut ctx as *mut SearchCtx<'_>) as isize);
        ctx.found
    }
}

/// Clip the main window to an exact rounded rectangle while floating, and drop the
/// region again while maximized. Clipping at the OS level bounds the surface itself,
/// so the corners outside the radius can never composite as the DWM white fallback.
pub fn ensure_main_window_region(is_maximized: bool, radius_points: f32, ppp: f32) {
    #[cfg(target_os = "windows")]
    {
        use std::sync::atomic::AtomicIsize;

        static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
        static APPLIED: std::sync::Mutex<Option<(i32, i32, i32, bool)>> = std::sync::Mutex::new(None);

        #[repr(C)]
        struct WinRect {
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
        }

        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetClientRect(hwnd: isize, rect: *mut WinRect) -> i32;
            fn SetWindowRgn(hwnd: isize, hrgn: isize, redraw: i32) -> i32;
        }
        #[link(name = "gdi32")]
        unsafe extern "system" {
            fn CreateRoundRectRgn(x1: i32, y1: i32, x2: i32, y2: i32, w: i32, h: i32) -> isize;
        }

        let mut hwnd = MAIN_HWND.load(Ordering::Relaxed);
        if hwnd == 0 {
            hwnd = find_own_window("GameAccel");
            if hwnd == 0 {
                return;
            }
            MAIN_HWND.store(hwnd, Ordering::Relaxed);
        }

        let mut rc = WinRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if unsafe { GetClientRect(hwnd, &mut rc) } == 0 {
            return;
        }
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        if w <= 0 || h <= 0 {
            return;
        }

        let diam = if is_maximized {
            0
        } else {
            (radius_points * 2.0 * ppp).round().max(8.0) as i32
        };

        let mut applied = APPLIED.lock().unwrap_or_else(|p| p.into_inner());
        if *applied == Some((w, h, diam, is_maximized)) {
            return;
        }

        let ok = unsafe {
            if is_maximized {
                // NULL region = plain rectangular window.
                SetWindowRgn(hwnd, 0, 1) != 0
            } else {
                let hrgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, diam, diam);
                hrgn != 0 && SetWindowRgn(hwnd, hrgn, 1) != 0
            }
        };
        if ok {
            *applied = Some((w, h, diam, is_maximized));
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (is_maximized, radius_points, ppp);
    }
}

/// Hide the main window to system tray by removing it from taskbar/Alt+Tab and moving it
/// off-screen while keeping `WS_VISIBLE`, so eframe's event loop stays alive for the
/// tray popover viewport.
pub fn hide_window_to_tray() {
    #[cfg(target_os = "windows")]
    {
        #[repr(C)]
        struct WinRect {
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
        }

        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetWindowRect(hwnd: isize, rect: *mut WinRect) -> i32;
            fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
            fn SetWindowLongPtrW(hwnd: isize, index: i32, new_long: isize) -> isize;
            fn SetWindowPos(
                hwnd: isize,
                insert_after: isize,
                x: i32,
                y: i32,
                cx: i32,
                cy: i32,
                flags: u32,
            ) -> i32;
            fn ShowWindow(hwnd: isize, n_cmd_show: i32) -> i32;
        }

        const GWL_EXSTYLE: i32 = -20;
        const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
        const WS_EX_APPWINDOW: isize = 0x0004_0000;
        const SW_HIDE: i32 = 0;
        const SW_SHOWNOACTIVATE: i32 = 4;
        const SWP_NOSIZE: u32 = 0x0001;
        const SWP_NOZORDER: u32 = 0x0004;
        const SWP_NOACTIVATE: u32 = 0x0010;

        let hwnd = find_own_window("GameAccel");
        if hwnd != 0 {
            unsafe {
                let mut rc = WinRect {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                if GetWindowRect(hwnd, &mut rc) != 0 && rc.left > -10000 && rc.top > -10000 {
                    let w = (rc.right - rc.left).max(780);
                    let h = (rc.bottom - rc.top).max(508);
                    if let Ok(mut guard) = SAVED_WINDOW_RECT.lock() {
                        *guard = Some((rc.left, rc.top, w, h));
                    }
                }
                ShowWindow(hwnd, SW_HIDE);
                let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                let new_ex = (ex | WS_EX_TOOLWINDOW) & !WS_EX_APPWINDOW;
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex);
                SetWindowPos(
                    hwnd,
                    0,
                    -32000,
                    -32000,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
        }
    }
}

/// Immediately restore and focus the tray-hidden Win32 window.
#[cfg(target_os = "windows")]
fn win32_show_and_focus_window(title: &str) {
    #[repr(C)]
    struct WinRect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetWindowRect(hwnd: isize, rect: *mut WinRect) -> i32;
        fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
        fn SetWindowLongPtrW(hwnd: isize, index: i32, new_long: isize) -> isize;
        fn SetWindowPos(
            hwnd: isize,
            insert_after: isize,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
        fn ShowWindow(hwnd: isize, n_cmd_show: i32) -> i32;
        fn SetForegroundWindow(hwnd: isize) -> i32;
        fn BringWindowToTop(hwnd: isize) -> i32;
    }

    const GWL_EXSTYLE: i32 = -20;
    const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
    const WS_EX_APPWINDOW: isize = 0x0004_0000;
    const SW_HIDE: i32 = 0;
    const SW_SHOW: i32 = 5;
    const SW_RESTORE: i32 = 9;
    const SWP_NOZORDER: u32 = 0x0004;

    let hwnd = find_own_window(title);
    if hwnd != 0 {
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let mut rc = WinRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            let _ = GetWindowRect(hwnd, &mut rc);
            let was_tray_hidden =
                (ex & WS_EX_TOOLWINDOW) != 0 || rc.left < -10000 || rc.top < -10000;

            if was_tray_hidden {
                ShowWindow(hwnd, SW_HIDE);
                let new_ex = (ex & !WS_EX_TOOLWINDOW) | WS_EX_APPWINDOW;
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex);
                let (x, y, w, h) = SAVED_WINDOW_RECT
                    .lock()
                    .ok()
                    .and_then(|g| *g)
                    .unwrap_or((160, 90, 840, 548));
                SetWindowPos(hwnd, 0, x, y, w, h, SWP_NOZORDER);
            }
            ShowWindow(hwnd, SW_SHOW);
            ShowWindow(hwnd, SW_RESTORE);
            BringWindowToTop(hwnd);
            SetForegroundWindow(hwnd);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn win32_show_and_focus_window(_title: &str) {}

/// Restore and focus the main window.
///
/// Commands are sent explicitly to `ViewportId::ROOT`: this is also called from the tray
/// thread, and `ctx.send_viewport_cmd` targets whichever viewport the UI thread happens to be
/// rendering at that moment — often the floating card, which would then get
/// `Visible`/`Minimized(false)`/`Focus` and visibly flash.
pub fn wake_window(ctx: &egui::Context) {
    use egui::ViewportId;
    win32_show_and_focus_window("GameAccel");
    ctx.send_viewport_cmd_to(ViewportId::ROOT, egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd_to(ViewportId::ROOT, egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd_to(ViewportId::ROOT, egui::ViewportCommand::Focus);
    ctx.request_repaint_of(ViewportId::ROOT);
}

/// Convert physical cursor coordinates `(px, py)` into logical coordinates clamped inside
/// the active monitor's work area so the popover never overflows off-screen.
pub fn clamp_popover_position(px: f64, py: f64, pop_w: f32, pop_h: f32, ppp: f32) -> Pos2 {
    let ppp = ppp.max(1.0);
    let cursor_x = px as f32 / ppp;
    let cursor_y = py as f32 / ppp;

    #[cfg(target_os = "windows")]
    {
        #[repr(C)]
        struct Point {
            x: i32,
            y: i32,
        }
        #[repr(C)]
        struct Rect {
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
        }
        #[repr(C)]
        struct MonitorInfo {
            cb_size: u32,
            rc_monitor: Rect,
            rc_work: Rect,
            dw_flags: u32,
        }

        #[link(name = "user32")]
        unsafe extern "system" {
            fn MonitorFromPoint(pt: Point, dw_flags: u32) -> isize;
            fn GetMonitorInfoW(hmonitor: isize, lpmi: *mut MonitorInfo) -> i32;
        }

        const MONITOR_DEFAULTTONEAREST: u32 = 2;
        unsafe {
            let hmon = MonitorFromPoint(
                Point {
                    x: px.round() as i32,
                    y: py.round() as i32,
                },
                MONITOR_DEFAULTTONEAREST,
            );
            if hmon != 0 {
                let mut info = MonitorInfo {
                    cb_size: std::mem::size_of::<MonitorInfo>() as u32,
                    rc_monitor: Rect {
                        left: 0,
                        top: 0,
                        right: 0,
                        bottom: 0,
                    },
                    rc_work: Rect {
                        left: 0,
                        top: 0,
                        right: 0,
                        bottom: 0,
                    },
                    dw_flags: 0,
                };
                if GetMonitorInfoW(hmon, &mut info) != 0 {
                    let margin = 10.0_f32;
                    let work_left = info.rc_work.left as f32 / ppp + margin;
                    let work_top = info.rc_work.top as f32 / ppp + margin;
                    let work_right = info.rc_work.right as f32 / ppp - margin;
                    let work_bottom = info.rc_work.bottom as f32 / ppp - margin;

                    let desired_x = cursor_x - pop_w * 0.5;
                    let desired_y = if cursor_y > (work_top + work_bottom) * 0.5 {
                        cursor_y - pop_h - 10.0
                    } else {
                        cursor_y + 10.0
                    };

                    let max_x = (work_right - pop_w).max(work_left);
                    let max_y = (work_bottom - pop_h).max(work_top);
                    return Pos2::new(
                        desired_x.clamp(work_left, max_x),
                        desired_y.clamp(work_top, max_y),
                    );
                }
            }
        }
    }

    Pos2::new(
        (cursor_x - pop_w * 0.5).max(12.0),
        (cursor_y - pop_h - 10.0).max(12.0),
    )
}

pub fn init_tray<F, Q>(
    ctx: egui::Context,
    initial_style: TrayStyle,
    on_cmd: F,
    force_quit_cleanup: Q,
) -> Option<TrayHandle>
where
    F: Fn(TrayCmd) + Send + 'static,
    Q: Fn() + Send + Sync + 'static,
{
    let icon_data = build_app_icon();
    let icon = match Icon::from_rgba(icon_data.rgba, icon_data.width, icon_data.height) {
        Ok(i) => i,
        Err(e) => {
            log_line("ERROR", &format!("托盘图标创建失败: {e}"));
            return None;
        }
    };

    let use_panel = Arc::new(AtomicBool::new(initial_style == TrayStyle::Panel));
    let mut builder = TrayIconBuilder::new()
        .with_tooltip("GameAccel - QUIC/UDP 游戏加速")
        .with_icon(icon)
        .with_menu_on_left_click(false);
    if initial_style == TrayStyle::Menu {
        builder = builder.with_menu(Box::new(build_native_menu()));
    }

    let tray = match builder.build() {
        Ok(t) => {
            log_line("INFO", "系统托盘初始化成功");
            t
        }
        Err(e) => {
            log_line("ERROR", &format!("系统托盘创建失败: {e}"));
            return None;
        }
    };

    let cleanup = Arc::new(force_quit_cleanup);
    let panel_flag = use_panel.clone();

    std::thread::spawn(move || {
        let menu_rx = MenuEvent::receiver();
        let tray_rx = TrayIconEvent::receiver();
        loop {
            while let Ok(ev) = menu_rx.try_recv() {
                log_line("INFO", &format!("收到托盘菜单事件: id={}", ev.id.0));
                match ev.id.0.as_str() {
                    "show" => {
                        wake_window(&ctx);
                        on_cmd(TrayCmd::Show);
                    }
                    "toggle" => {
                        on_cmd(TrayCmd::ToggleConnect);
                        ctx.request_repaint_of(egui::ViewportId::ROOT);
                    }
                    "quit" => {
                        log_line("INFO", "执行托盘退出：开始清理连接、TUN 路由与系统代理...");
                        on_cmd(TrayCmd::Quit);
                        cleanup();
                        ctx.send_viewport_cmd_to(
                            egui::ViewportId::ROOT,
                            egui::ViewportCommand::Close,
                        );
                        ctx.request_repaint_of(egui::ViewportId::ROOT);
                        std::thread::sleep(Duration::from_millis(350));
                        log_line("INFO", "========== GameAccel 正常退出 ==========");
                        crate::hard_exit(0);
                    }
                    _ => {}
                }
            }

            while let Ok(ev) = tray_rx.try_recv() {
                if let TrayIconEvent::Click {
                    button,
                    button_state: MouseButtonState::Up,
                    position,
                    ..
                } = ev
                {
                    match button {
                        MouseButton::Left => {
                            log_line("INFO", "左键点击托盘图标，唤起主窗口");
                            wake_window(&ctx);
                            on_cmd(TrayCmd::Show);
                        }
                        MouseButton::Right if panel_flag.load(Ordering::Relaxed) => {
                            on_cmd(TrayCmd::TogglePopover(position.x, position.y));
                            ctx.request_repaint_of(egui::ViewportId::ROOT);
                        }
                        _ => {}
                    }
                }
            }

            std::thread::sleep(Duration::from_millis(60));
        }
    });

    Some(TrayHandle { tray, use_panel })
}

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

/// Apply Windows 11 native rounded corners to all top-level windows of this process.
pub fn apply_win11_rounded_corners() {
    #[cfg(target_os = "windows")]
    {
        use std::ffi::c_void;
        type Hwnd = *mut c_void;
        type EnumProc = unsafe extern "system" fn(Hwnd, isize) -> i32;

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcessId() -> u32;
        }
        #[link(name = "user32")]
        unsafe extern "system" {
            fn EnumWindows(cb: EnumProc, lparam: isize) -> i32;
            fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
        }
        #[link(name = "dwmapi")]
        unsafe extern "system" {
            fn DwmSetWindowAttribute(
                hwnd: Hwnd,
                dw_attribute: u32,
                pv_attribute: *const c_void,
                cb_attribute: u32,
            ) -> i32;
        }

        unsafe extern "system" fn enum_cb(hwnd: Hwnd, target_pid: isize) -> i32 {
            let mut pid: u32 = 0;
            unsafe {
                GetWindowThreadProcessId(hwnd, &mut pid);
                if pid as isize == target_pid {
                    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
                    const DWMWCP_ROUND: u32 = 2;
                    let pref = DWMWCP_ROUND;
                    let _ = DwmSetWindowAttribute(
                        hwnd,
                        DWMWA_WINDOW_CORNER_PREFERENCE,
                        (&pref as *const u32).cast::<c_void>(),
                        std::mem::size_of::<u32>() as u32,
                    );
                }
            }
            1
        }

        unsafe {
            let my_pid = GetCurrentProcessId() as isize;
            let _ = EnumWindows(enum_cb, my_pid);
        }
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

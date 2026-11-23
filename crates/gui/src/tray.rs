use crate::logger::log_line;
use crate::theme::build_app_icon;
use eframe::egui;
use std::sync::Arc;
use std::time::Duration;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCmd {
    Show,
    ToggleConnect,
    Quit,
}

pub struct TrayHandle {
    _tray: TrayIcon,
}

/// Immediately restore and focus the hidden Win32 window on tray click,
/// bypassing winit's paused event loop when `ViewportCommand::Visible(false)` is active.
#[cfg(target_os = "windows")]
fn win32_show_and_focus_window(title: &str) {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn FindWindowW(lp_class_name: *const u16, lp_window_name: *const u16) -> isize;
        fn ShowWindow(hwnd: isize, n_cmd_show: i32) -> i32;
        fn SetForegroundWindow(hwnd: isize) -> i32;
        fn BringWindowToTop(hwnd: isize) -> i32;
    }

    const SW_SHOW: i32 = 5;
    const SW_RESTORE: i32 = 9;

    let mut wide: Vec<u16> = title.encode_utf16().collect();
    wide.push(0);
    unsafe {
        let hwnd = FindWindowW(std::ptr::null(), wide.as_ptr());
        if hwnd != 0 {
            ShowWindow(hwnd, SW_SHOW);
            ShowWindow(hwnd, SW_RESTORE);
            BringWindowToTop(hwnd);
            SetForegroundWindow(hwnd);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn win32_show_and_focus_window(_title: &str) {}

fn wake_window(ctx: &egui::Context) {
    win32_show_and_focus_window("GameAccel");
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
}

pub fn init_tray<F, Q>(
    ctx: egui::Context,
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

    let item_show = MenuItem::with_id("show", "显示主窗口", true, None);
    let item_toggle = MenuItem::with_id("toggle", "连接 / 断开加速", true, None);
    let item_quit = MenuItem::with_id("quit", "退出 GameAccel", true, None);

    let menu = Menu::new();
    let _ = menu.append(&item_show);
    let _ = menu.append(&item_toggle);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&item_quit);

    let tray = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("GameAccel - QUIC/UDP 游戏加速")
        .with_icon(icon)
        .build()
    {
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
                        ctx.request_repaint();
                    }
                    "quit" => {
                        log_line("INFO", "执行托盘退出：开始清理连接、TUN 路由与系统代理...");
                        on_cmd(TrayCmd::Quit);
                        cleanup();
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        ctx.request_repaint();
                        std::thread::sleep(Duration::from_millis(350));
                        log_line("INFO", "========== GameAccel 正常退出 ==========");
                        std::process::exit(0);
                    }
                    _ => {}
                }
            }

            while let Ok(ev) = tray_rx.try_recv() {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = ev
                {
                    log_line("INFO", "左键点击托盘图标，唤起主窗口");
                    wake_window(&ctx);
                    on_cmd(TrayCmd::Show);
                }
            }

            std::thread::sleep(Duration::from_millis(60));
        }
    });

    Some(TrayHandle { _tray: tray })
}

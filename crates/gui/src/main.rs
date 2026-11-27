#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod admin;
mod admin_actions;
mod admin_view;
mod icons;
mod logger;
mod reconnect;
mod storage;
mod sys_proxy;
mod theme;
mod tray;
mod updater;
mod views;
mod widgets;

use admin::{AdminEvent, AdminState};
use client::ConnectError;
use client::socks5::ClientStats;
use client::telemetry::TelemetrySeries;
use eframe::egui::{self, Align, Color32, CornerRadius, Frame, Layout, Margin, Pos2, Stroke, Vec2};
use protocol::share::{NodeProfile, NodeProtocol};
use reconnect::ReconnectPolicy;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use storage::{AppConfig, load_config};
use theme::{ThemePalette, apply_theme, build_app_icon, configure_fonts};
use tokio::sync::{mpsc, watch};
use tray::{TrayCmd, TrayHandle, init_tray};

pub(crate) const APP_VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NavTab {
    Dashboard,
    Nodes,
    Logs,
    Admin,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnState {
    Disconnected,
    Connecting,
    Connected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogFilter {
    All,
    Success,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChartMode {
    Traffic,
    Rtt,
}

#[derive(Debug, Clone)]
pub(crate) enum ProbeState {
    Probing,
    Ok(u64),
    Err(String),
}

enum AsyncEvent {
    Log(String),
    ConnError(u64, ConnectError),
    /// The client session ended without an error (tunnel dropped or user stop).
    ConnEnded(u64),
    Tray(TrayCmd),
    ProbeResult {
        node_id: String,
        result: Result<u64, String>,
    },
    SubFetched {
        url: String,
        result: Result<Vec<NodeProfile>, String>,
    },
    Admin(AdminEvent),
}

pub(crate) struct NodeEditorState {
    pub(crate) editing_id: Option<String>,
    pub(crate) name: String,
    pub(crate) server: String,
    pub(crate) sni: String,
    pub(crate) token: String,
    pub(crate) protocol: NodeProtocol,
    pub(crate) game_dup: bool,
    pub(crate) insecure: bool,
    pub(crate) brutal_mbps: String,
    pub(crate) show_token: bool,
}

impl NodeEditorState {
    pub(crate) fn new_blank() -> Self {
        Self {
            editing_id: None,
            name: String::new(),
            server: String::new(),
            sni: String::new(),
            token: String::new(),
            protocol: NodeProtocol::Gacl,
            game_dup: true,
            insecure: false,
            brutal_mbps: "0".to_string(),
            show_token: true,
        }
    }

    pub(crate) fn from_node(n: &NodeProfile) -> Self {
        Self {
            editing_id: Some(n.id.clone()),
            name: n.name.clone(),
            server: n.server.clone(),
            sni: n.sni.clone(),
            token: n.token.clone(),
            protocol: n.protocol,
            game_dup: n.game_dup,
            insecure: n.insecure,
            brutal_mbps: n.brutal_mbps.to_string(),
            show_token: false,
        }
    }
}

pub(crate) struct GameAccelApp {
    pub(crate) rt: Arc<tokio::runtime::Runtime>,
    pub(crate) cfg: AppConfig,
    pub(crate) active_tab: NavTab,
    pub(crate) state: ConnState,
    pub(crate) stats: Arc<ClientStats>,
    pub(crate) shutdown_slot: Arc<Mutex<Option<watch::Sender<bool>>>>,
    pub(crate) sys_proxy_flag: Arc<AtomicBool>,
    pub(crate) event_rx: mpsc::UnboundedReceiver<AsyncEvent>,
    pub(crate) event_tx: mpsc::UnboundedSender<AsyncEvent>,
    pub(crate) logs: Vec<String>,
    pub(crate) log_filter: LogFilter,
    pub(crate) log_search: String,
    pub(crate) is_admin: bool,
    pub(crate) last_error: Option<String>,
    pub(crate) toast: Option<(String, Instant)>,
    pub(crate) reconnect: ReconnectPolicy,
    pub(crate) pending_mode_reconnect: Option<Instant>,
    pub(crate) session_id: u64,
    /// Live split-routing mode shared with the running session (no reconnect needed).
    pub(crate) route_mode_cell: Arc<client::rules::RouteModeCell>,

    // Telemetry series (bounded 60s throughput & RTT ring buffer)
    pub(crate) telemetry: TelemetrySeries,
    pub(crate) chart_mode: ChartMode,

    // Nodes & Import UI state
    pub(crate) probes: HashMap<String, ProbeState>,
    pub(crate) node_editor: Option<NodeEditorState>,
    pub(crate) import_panel_open: bool,
    pub(crate) import_text: String,
    pub(crate) sub_panel_open: bool,
    pub(crate) sub_url_input: String,
    pub(crate) sub_loading: bool,

    // Server Admin UI state (in-memory session; passwords are never persisted)
    pub(crate) admin: AdminState,

    // System tray & floating popover
    pub(crate) tray: Option<TrayHandle>,
    pub(crate) window_hidden_to_tray: bool,
    pub(crate) tray_popover_open: bool,
    pub(crate) tray_popover_pos: Pos2,
    pub(crate) tray_popover_stage: tray::PopupStage,
    pub(crate) tray_popover_ever_focused: bool,
    pub(crate) tray_popover_search: String,
    pub(crate) floating_pill_pos_dirty: Option<Instant>,
    pub(crate) floating_pill_raise_frames: u8,
    pub(crate) floating_menu_open: bool,
    pub(crate) floating_menu_pos: Pos2,
    pub(crate) floating_menu_phys_pos: Option<[i32; 2]>,
    pub(crate) floating_menu_stage: tray::PopupStage,
    pub(crate) floating_menu_ever_focused: bool,
    pub(crate) floating_pill_fs_hidden: bool,
    pub(crate) floating_pill_last_fs_check: Instant,
    pub(crate) floating_pill_edge_tucked: bool,
    pub(crate) floating_pill_last_hover: Instant,
    /// Position handed to the floating card's `ViewportBuilder`. Kept constant for the
    /// lifetime of the window: egui turns any change into an `OuterPosition` command,
    /// which would fight with OS drags / edge-tuck moves and make the card flicker.
    pub(crate) floating_pill_spawn_pos: Option<Pos2>,
    /// Speed-reactive LED state: smoothed activity (0..=1), breathing / ripple phases and
    /// the timestamp of the last advance.
    pub(crate) floating_pill_led_k: f32,
    pub(crate) floating_pill_led_phase: f64,
    pub(crate) floating_pill_led_last: f64,
    /// Brand lightning mark hollow-to-solid transition state (`0.0` idle wireframe .. `1.0` solid).
    pub(crate) brand_logo_fill_t: f32,
    pub(crate) brand_logo_pulse: f32,
    pub(crate) brand_logo_last_state: ConnState,
    pub(crate) brand_logo_last_time: f64,
    /// GitHub Releases based auto-updater.
    pub(crate) updater: updater::Updater,
    /// Version we've already toasted about, so the "new version" toast shows once.
    pub(crate) update_notified: Option<String>,
    pub(crate) tun_admin_prompt: Option<TunAdminPrompt>,
    pub(crate) really_quit: bool,
}

impl GameAccelApp {
    fn new(cc: &eframe::CreationContext<'_>, rt: Arc<tokio::runtime::Runtime>) -> Self {
        configure_fonts(&cc.egui_ctx);
        let cfg = load_config();
        let palette = ThemePalette::from_mode(cfg.theme_mode);
        apply_theme(&cc.egui_ctx, palette);

        let is_admin = client::tun_mode::is_elevated();
        logger::log_line(
            "INFO",
            &format!("权限检测: is_admin={is_admin}, mode={:?}", cfg.proxy_mode),
        );
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let shutdown_slot: Arc<Mutex<Option<watch::Sender<bool>>>> = Arc::new(Mutex::new(None));
        let sys_proxy_flag = Arc::new(AtomicBool::new(false));

        let tray_tx = event_tx.clone();
        let quit_shutdown = shutdown_slot.clone();
        let quit_proxy_flag = sys_proxy_flag.clone();
        let tray = init_tray(
            cc.egui_ctx.clone(),
            cfg.tray_style,
            move |cmd| {
                let _ = tray_tx.send(AsyncEvent::Tray(cmd));
            },
            move || {
                if let Ok(mut guard) = quit_shutdown.lock()
                    && let Some(tx) = guard.take()
                {
                    let _ = tx.send(true);
                }
                if quit_proxy_flag.swap(false, Ordering::SeqCst) {
                    let _ = sys_proxy::disable_system_proxy();
                    logger::log_line("INFO", "退出清理：已关闭 Windows 系统代理");
                }
            },
        );

        let default_admin_user = cfg
            .active_admin_server()
            .map(|s| s.username.clone())
            .unwrap_or_else(|| "admin".to_string());
        let admin_state = AdminState {
            login_user: default_admin_user,
            ..AdminState::default()
        };

        let mut app = Self {
            rt,
            route_mode_cell: client::rules::RouteModeCell::new(cfg.route_mode),
            cfg,
            active_tab: NavTab::Dashboard,
            state: ConnState::Disconnected,
            stats: Arc::new(ClientStats::default()),
            shutdown_slot,
            sys_proxy_flag,
            event_rx,
            event_tx,
            logs: Vec::new(),
            log_filter: LogFilter::All,
            log_search: String::new(),
            is_admin,
            last_error: None,
            toast: None,
            reconnect: ReconnectPolicy::default(),
            pending_mode_reconnect: None,
            session_id: 0,
            telemetry: TelemetrySeries::new(),
            chart_mode: ChartMode::Traffic,
            probes: HashMap::new(),
            node_editor: None,
            import_panel_open: false,
            import_text: String::new(),
            sub_panel_open: false,
            sub_url_input: String::new(),
            sub_loading: false,
            admin: admin_state,
            tray,
            window_hidden_to_tray: false,
            tray_popover_open: false,
            tray_popover_pos: Pos2::new(800.0, 300.0),
            tray_popover_stage: tray::PopupStage::Closed,
            tray_popover_ever_focused: false,
            tray_popover_search: String::new(),
            floating_pill_pos_dirty: None,
            floating_pill_raise_frames: 0,
            floating_menu_open: false,
            floating_menu_pos: Pos2::new(600.0, 200.0),
            floating_menu_phys_pos: None,
            floating_menu_stage: tray::PopupStage::Closed,
            floating_menu_ever_focused: false,
            floating_pill_fs_hidden: false,
            floating_pill_last_fs_check: Instant::now(),
            floating_pill_edge_tucked: false,
            floating_pill_last_hover: Instant::now(),
            floating_pill_spawn_pos: None,
            floating_pill_led_k: 0.0,
            floating_pill_led_phase: 0.0,
            floating_pill_led_last: 0.0,
            brand_logo_fill_t: 0.0,
            brand_logo_pulse: 0.0,
            brand_logo_last_state: ConnState::Disconnected,
            brand_logo_last_time: 0.0,
            updater: updater::Updater::default(),
            update_notified: None,
            tun_admin_prompt: None,
            really_quit: false,
        };
        app.push_log(if is_admin {
            "客户端已就绪（管理员权限，TUN 虚拟网卡可用）"
        } else {
            "客户端已就绪，点击「开启加速」启动 QUIC/UDP 通道"
        });
        // Started by `restart_as_admin(auto_connect = true)`: bring TUN up once the previous
        // instance has finished releasing the listener / system proxy.
        if is_admin
            && app.cfg.proxy_mode == storage::ProxyMode::Tun
            && std::env::args().any(|a| a == "--connect")
        {
            app.state = ConnState::Connecting;
            app.pending_mode_reconnect = Some(Instant::now() + Duration::from_millis(1200));
            app.push_log("已以管理员身份重启，正在自动开启 TUN 加速...");
        } else if !is_admin && app.cfg.proxy_mode == storage::ProxyMode::Tun {
            app.tun_admin_prompt = Some(TunAdminPrompt {
                auto_connect: false,
                wake: false,
            });
        }
        app
    }

    fn step_brand_logo_anim(&mut self, ctx: &egui::Context, visible: bool) {
        let now = ctx.input(|i| i.time);
        let dt = if self.brand_logo_last_time > 0.0 {
            (now - self.brand_logo_last_time).clamp(0.0, 0.05) as f32
        } else {
            0.016
        };
        self.brand_logo_last_time = now;

        if self.state != self.brand_logo_last_state {
            if self.state == ConnState::Connected && visible {
                self.brand_logo_pulse = 1.0;
            }
            self.brand_logo_last_state = self.state;
        }

        let target_fill = match self.state {
            ConnState::Disconnected => 0.0,
            ConnState::Connected => 1.0,
            ConnState::Connecting => 0.46 + 0.22 * (now as f32 * 5.2).sin(),
        };

        if !visible {
            self.brand_logo_fill_t = if self.state == ConnState::Connected {
                1.0
            } else {
                0.0
            };
            self.brand_logo_pulse = 0.0;
            return;
        }

        let lerp_k = (dt * 11.0).min(1.0);
        self.brand_logo_fill_t += (target_fill - self.brand_logo_fill_t) * lerp_k;
        if self.state != ConnState::Connecting
            && (target_fill - self.brand_logo_fill_t).abs() < 0.003
        {
            self.brand_logo_fill_t = target_fill;
        }
        self.brand_logo_pulse = (self.brand_logo_pulse - dt * 2.4).max(0.0);

        let animating = self.state == ConnState::Connecting
            || (target_fill - self.brand_logo_fill_t).abs() > 0.002
            || self.brand_logo_pulse > 0.002;
        if animating {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn render_toast_overlay(
        &mut self,
        ctx: &egui::Context,
        right_rect: egui::Rect,
        p: ThemePalette,
    ) {
        let Some((msg, ts)) = self.toast.clone() else {
            return;
        };
        let elapsed_ms = ts.elapsed().as_secs_f32() * 1000.0;
        const TOTAL_MS: f32 = 2700.0;
        const IN_MS: f32 = 160.0;
        const OUT_MS: f32 = 340.0;
        if elapsed_ms >= TOTAL_MS {
            self.toast = None;
            return;
        }

        let (alpha, y_offset) = if elapsed_ms < IN_MS {
            let t = (elapsed_ms / IN_MS).clamp(0.0, 1.0);
            let ease = 1.0 - (1.0 - t) * (1.0 - t);
            ctx.request_repaint();
            (ease, (1.0 - ease) * 8.0)
        } else if elapsed_ms > TOTAL_MS - OUT_MS {
            let t = ((TOTAL_MS - elapsed_ms) / OUT_MS).clamp(0.0, 1.0);
            ctx.request_repaint();
            (t * t, (1.0 - t) * 4.0)
        } else {
            (1.0, 0.0)
        };

        let is_err = msg.contains("失败")
            || msg.contains("异常")
            || msg.contains("错误")
            || msg.contains("无效")
            || msg.contains("断开");
        let is_ok = msg.contains("成功")
            || msg.contains("已切换")
            || msg.contains("已复制")
            || msg.contains("已保存")
            || msg.contains("已添加")
            || msg.contains("已重置");

        let dot_col = if is_err {
            p.danger_text
        } else if is_ok {
            p.success_text
        } else {
            p.accent
        };
        let border_col = if is_err {
            p.danger_text
        } else {
            p.border_strong
        };

        let bg = p.bg_card.gamma_multiply(alpha);
        let border = border_col.gamma_multiply(alpha);
        let dot = dot_col.gamma_multiply(alpha);
        let text_col = p.text_primary.gamma_multiply(alpha);
        let shadow_alpha = ((if p.is_dark { 96.0 } else { 32.0 }) * alpha) as u8;

        let anchor_pos = Pos2::new(right_rect.center().x, right_rect.max.y - 24.0 + y_offset);
        let mut dismissed = false;
        egui::Area::new(egui::Id::new("floating_toast_overlay"))
            .order(egui::Order::Foreground)
            .pivot(egui::Align2::CENTER_BOTTOM)
            .fixed_pos(anchor_pos)
            .show(ctx, |ui| {
                let frame_resp = Frame::new()
                    .fill(bg)
                    .stroke(Stroke::new(1.0_f32, border))
                    .corner_radius(CornerRadius::same(18))
                    .inner_margin(Margin::symmetric(14, 8))
                    .shadow(egui::Shadow {
                        offset: [0, 4],
                        blur: 14,
                        spread: 0,
                        color: Color32::from_black_alpha(shadow_alpha),
                    })
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            let (dot_rect, _) =
                                ui.allocate_exact_size(Vec2::splat(7.0), egui::Sense::hover());
                            ui.painter().circle_filled(dot_rect.center(), 3.5, dot);
                            ui.label(
                                egui::RichText::new(&msg)
                                    .size(theme::FONT_SM)
                                    .color(text_col),
                            );
                        });
                    });
                if frame_resp.response.interact(egui::Sense::click()).clicked() {
                    dismissed = true;
                }
            });
        if dismissed {
            self.toast = None;
        }
    }
}

impl Drop for GameAccelApp {
    fn drop(&mut self) {
        self.stop_connect();
    }
}

impl eframe::App for GameAccelApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        ThemePalette::from_mode(self.cfg.theme_mode)
            .bg_root
            .to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let is_minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        if ctx.input(|i| i.focused) && !is_minimized {
            self.window_hidden_to_tray = false;
        }
        let is_background = self.window_hidden_to_tray || is_minimized;
        let pill_animating = self.cfg.show_floating_pill
            && !self.floating_pill_fs_hidden
            && self.state != ConnState::Disconnected;
        if is_background && !self.tray_popover_open && !self.floating_menu_open && !pill_animating {
            std::thread::sleep(Duration::from_millis(24));
        }

        self.poll_async(ctx);
        self.tick_updater(ctx);

        if ctx.input(|i| i.viewport().close_requested()) && !self.really_quit && self.tray.is_some()
        {
            self.window_hidden_to_tray = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            tray::hide_window_to_tray();
            if self.cfg.show_floating_pill {
                self.floating_pill_raise_frames = 8;
            }
        }

        let p = ThemePalette::from_mode(self.cfg.theme_mode);

        let any_probing = self
            .probes
            .values()
            .any(|v| matches!(v, ProbeState::Probing));
        if pill_animating {
            ctx.request_repaint_after(self.pill_anim_interval());
        } else if self.state != ConnState::Disconnected
            || any_probing
            || self.sub_loading
            || self.admin.busy
            || (self.active_tab == NavTab::Admin && self.admin.session_cookie.is_some())
            || self.toast.is_some()
            || self.tray_popover_open
            || self.floating_menu_open
            || self.cfg.show_floating_pill
            || self.floating_pill_pos_dirty.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        let mut toast_workspace_rect = egui::Rect::NOTHING;

        // While hidden to tray (parked off-screen) / minimized the main window is invisible, but
        // the floating card's animation keeps driving root frames. Skip the (expensive) main UI.
        // Use the real window position rather than `window_hidden_to_tray`: the tray thread
        // restores the window before that flag is cleared, which would show one empty frame.
        let main_offscreen = ctx.input(|i| {
            i.viewport()
                .outer_rect
                .is_some_and(|r| r.min.x < -8000.0 || r.min.y < -8000.0)
        });
        let main_visible = !(is_minimized || main_offscreen);
        self.step_brand_logo_anim(ctx, main_visible || self.tray_popover_open);
        if main_visible {
            // Frameless rounded window container with single crisp 1px outer border (zero double-stroke)
            egui::CentralPanel::default()
                .frame(Frame::NONE)
                .show(ctx, |ui| {
                    let total_rect = ui.max_rect();
                    let is_maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                    let radius = if is_maximized { 0u8 } else { 8u8 };

                    // Prefer Windows 11 native DWM 8px anti-aliased rounding + soft shadow;
                    // only fall back to GDI region + egui outer stroke on Windows 10 / others.
                    let native_dwm_round = tray::ensure_main_window_region(
                        is_maximized,
                        radius as f32,
                        p.border_strong,
                        p.bg_root,
                        ctx.pixels_per_point(),
                    );
                    let paint_radius = if native_dwm_round { 0u8 } else { radius };

                    // Enable native 8-direction edge & corner drag resizing on frameless window
                    if !is_maximized {
                        handle_frameless_resize(ctx, total_rect);
                    }

                    let sidebar_w = 68.0;
                    let split_x = total_rect.min.x + sidebar_w;

                    // Paint 68px icon rail & right studio stage as two non-overlapping halves
                    let sidebar_rect = egui::Rect::from_min_max(
                        total_rect.min,
                        Pos2::new(split_x, total_rect.max.y),
                    );
                    let right_rect = egui::Rect::from_min_max(
                        Pos2::new(split_x, total_rect.min.y),
                        total_rect.max,
                    );
                    toast_workspace_rect = right_rect;
                    ui.painter().rect_filled(
                        sidebar_rect,
                        CornerRadius {
                            nw: paint_radius,
                            sw: paint_radius,
                            ne: 0,
                            se: 0,
                        },
                        p.bg_sidebar,
                    );
                    ui.painter().rect_filled(
                        right_rect,
                        CornerRadius {
                            nw: 0,
                            sw: 0,
                            ne: paint_radius,
                            se: paint_radius,
                        },
                        p.bg_root,
                    );
                    ui.painter().line_segment(
                        [
                            Pos2::new(split_x, total_rect.min.y + 1.0),
                            Pos2::new(split_x, total_rect.max.y - 1.0),
                        ],
                        Stroke::new(1.0_f32, p.border_subtle),
                    );

                    // Split into 68px icon rail + right Bento stage
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;

                        ui.allocate_ui_with_layout(
                            Vec2::new(sidebar_w, total_rect.height()),
                            Layout::top_down(Align::LEFT),
                            |ui| {
                                Frame::new().inner_margin(Margin::symmetric(8, 14)).show(
                                    ui,
                                    |ui| {
                                        ui.set_width(52.0);
                                        ui.set_min_height((total_rect.height() - 28.0).max(200.0));
                                        self.render_sidebar_contents(ui, ctx, p);
                                    },
                                );
                            },
                        );

                        let right_w = (total_rect.width() - sidebar_w).max(460.0);
                        ui.allocate_ui_with_layout(
                            Vec2::new(right_w, total_rect.height()),
                            Layout::top_down(Align::LEFT),
                            |ui| {
                                Frame::new().inner_margin(Margin::symmetric(20, 14)).show(
                                    ui,
                                    |ui| {
                                        ui.set_width((right_w - 40.0).max(400.0));
                                        ui.set_min_height((total_rect.height() - 28.0).max(200.0));
                                        self.render_right_workspace(ui, ctx, p);
                                    },
                                );
                            },
                        );
                    });

                    // Single crisp 1px outer border on top (skipped when maximized or when
                    // Windows 11 DWM already renders the native 1px rounded border).
                    if !is_maximized && !native_dwm_round {
                        ui.painter().rect_stroke(
                            total_rect,
                            CornerRadius::same(radius),
                            Stroke::new(1.0_f32, p.border_strong),
                            egui::StrokeKind::Inside,
                        );
                    }
                });
        }

        if toast_workspace_rect.is_positive() {
            self.render_toast_overlay(ctx, toast_workspace_rect, p);
        }

        self.render_tun_admin_prompt(ctx, p);

        self.render_tray_popover(ctx, p);
        self.render_floating_pill(ctx, p);
    }
}

/// 8-direction edge & corner drag resizing for the frameless window.
/// Grab zones sit strictly inside the outer margins (`6px` edges, `12px` corners) so
/// they never steal clicks from any inner button or control.
fn handle_frameless_resize(ctx: &egui::Context, rect: egui::Rect) {
    let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) else {
        return;
    };
    let edge = 6.0_f32;
    let corner = 12.0_f32;

    let near_left = pos.x <= rect.left() + edge;
    let near_right = pos.x >= rect.right() - edge;
    let near_top = pos.y <= rect.top() + edge;
    let near_bottom = pos.y >= rect.bottom() - edge;

    let in_left_c = pos.x <= rect.left() + corner;
    let in_right_c = pos.x >= rect.right() - corner;
    let in_top_c = pos.y <= rect.top() + corner;
    let in_bottom_c = pos.y >= rect.bottom() - corner;

    let dir = if (near_left && in_top_c) || (near_top && in_left_c) {
        Some(egui::ResizeDirection::NorthWest)
    } else if (near_right && in_top_c) || (near_top && in_right_c) {
        Some(egui::ResizeDirection::NorthEast)
    } else if (near_left && in_bottom_c) || (near_bottom && in_left_c) {
        Some(egui::ResizeDirection::SouthWest)
    } else if (near_right && in_bottom_c) || (near_bottom && in_right_c) {
        Some(egui::ResizeDirection::SouthEast)
    } else if near_left {
        Some(egui::ResizeDirection::West)
    } else if near_right {
        Some(egui::ResizeDirection::East)
    } else if near_top {
        Some(egui::ResizeDirection::North)
    } else if near_bottom {
        Some(egui::ResizeDirection::South)
    } else {
        None
    };

    if let Some(d) = dir {
        let icon = match d {
            egui::ResizeDirection::NorthWest | egui::ResizeDirection::SouthEast => {
                egui::CursorIcon::ResizeNwSe
            }
            egui::ResizeDirection::NorthEast | egui::ResizeDirection::SouthWest => {
                egui::CursorIcon::ResizeNeSw
            }
            egui::ResizeDirection::West | egui::ResizeDirection::East => {
                egui::CursorIcon::ResizeHorizontal
            }
            egui::ResizeDirection::North | egui::ResizeDirection::South => {
                egui::CursorIcon::ResizeVertical
            }
        };
        ctx.set_cursor_icon(icon);
        if ctx.input(|i| i.pointer.primary_pressed()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(d));
        }
    }
}

/// Relaunch this exe elevated through the UAC prompt.
///
/// Returns `Ok` once the elevated process has been created; the caller is responsible for
/// cleaning up and exiting. Returns `Err` (and keeps this instance running) if the user
/// declines the UAC prompt or the launch fails.
pub(crate) fn relaunch_as_admin(args: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "shell32")]
        unsafe extern "system" {
            fn ShellExecuteW(
                hwnd: isize,
                op: *const u16,
                file: *const u16,
                params: *const u16,
                dir: *const u16,
                show: i32,
            ) -> isize;
        }
        let exe = std::env::current_exe().map_err(|e| format!("获取程序路径失败: {e}"))?;
        let wide = |s: &std::ffi::OsStr| s.encode_wide().chain(Some(0)).collect::<Vec<u16>>();
        let op = wide("runas".as_ref());
        let file = wide(exe.as_os_str());
        let params = wide(args.as_ref());
        const SW_SHOWNORMAL: i32 = 1;
        // Blocks until the UAC prompt is answered.
        let rc = unsafe {
            ShellExecuteW(
                0,
                op.as_ptr(),
                file.as_ptr(),
                params.as_ptr(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        match rc {
            r if r > 32 => Ok(()),
            5 => Err("已取消管理员授权".to_string()),
            r => Err(format!("以管理员身份启动失败（错误码 {r}）")),
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = args;
        Err("仅支持 Windows".to_string())
    }
}

/// Pending "TUN needs admin, restart elevated?" confirmation dialog.
pub(crate) struct TunAdminPrompt {
    /// Start the TUN connection automatically after the elevated restart.
    pub(crate) auto_connect: bool,
    /// Bring the main window up first (the request may come from the tray popover).
    pub(crate) wake: bool,
}

fn main() -> eframe::Result<()> {
    logger::init_logger();
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime"),
    );

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("GameAccel")
            .with_decorations(false)
            .with_resizable(true)
            .with_inner_size([840.0, 548.0])
            .with_min_inner_size([780.0, 508.0])
            .with_icon(Arc::new(build_app_icon())),
        centered: true,
        persist_window: false,
        ..Default::default()
    };

    let res = eframe::run_native(
        "GameAccel",
        options,
        Box::new(move |cc| Ok(Box::new(GameAccelApp::new(cc, rt.clone())))),
    );
    if let Err(e) = &res {
        logger::log_line("ERROR", &format!("eframe 异常退出: {e}"));
    }
    // Give async teardown (TUN routes, sockets) a moment to finish, then terminate.
    std::thread::sleep(Duration::from_millis(300));
    logger::log_line("INFO", "========== GameAccel 正常退出 ==========");
    hard_exit(if res.is_ok() { 0 } else { 1 })
}

/// Terminate the process immediately, skipping CRT/DLL teardown.
///
/// A normal `ExitProcess` runs every DLL's detach handler under the loader lock (GL driver,
/// wintun, tray, tokio worker threads still alive...), and this was observed to leave the
/// process hung forever after "彻底退出". All real cleanup (system proxy, routes, config) is
/// done explicitly before we get here.
pub(crate) fn hard_exit(code: u32) -> ! {
    #[cfg(target_os = "windows")]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> isize;
            fn TerminateProcess(process: isize, exit_code: u32) -> i32;
        }
        unsafe {
            TerminateProcess(GetCurrentProcess(), code);
        }
    }
    std::process::exit(code as i32)
}

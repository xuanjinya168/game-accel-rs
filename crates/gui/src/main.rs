#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod icons;
mod logger;
mod storage;
mod sys_proxy;
mod theme;
mod tray;
mod views;
mod widgets;

use chrono::Local;
use client::socks5::ClientStats;
use client::{ClientOptions, probe_node_rtt, run_client_with_shutdown};
use eframe::egui::{self, Align, Color32, CornerRadius, Frame, Layout, Margin, Pos2, Stroke, Vec2};
use protocol::share::{NodeProfile, parse_import_text};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use storage::{AppConfig, ProxyMode, SubscriptionEntry, load_config, save_config};
use theme::{ThemePalette, apply_theme, build_app_icon, configure_fonts};
use tokio::sync::{mpsc, watch};
use tray::{TrayCmd, TrayHandle, init_tray};

pub(crate) const APP_VERSION: &str = "v0.2.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NavTab {
    Dashboard,
    Nodes,
    Logs,
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
    ConnError(String),
    Tray(TrayCmd),
    ProbeResult {
        node_id: String,
        result: Result<u64, String>,
    },
    SubFetched {
        url: String,
        result: Result<Vec<NodeProfile>, String>,
    },
}

pub(crate) struct NodeEditorState {
    pub(crate) editing_id: Option<String>,
    pub(crate) name: String,
    pub(crate) server: String,
    pub(crate) sni: String,
    pub(crate) token: String,
    pub(crate) game_dup: bool,
    pub(crate) insecure: bool,
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
            game_dup: true,
            insecure: false,
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
            game_dup: n.game_dup,
            insecure: n.insecure,
            show_token: false,
        }
    }
}

pub(crate) struct GameAccelApp {
    rt: Arc<tokio::runtime::Runtime>,
    pub(crate) cfg: AppConfig,
    pub(crate) active_tab: NavTab,
    pub(crate) state: ConnState,
    pub(crate) stats: Arc<ClientStats>,
    shutdown_slot: Arc<Mutex<Option<watch::Sender<bool>>>>,
    sys_proxy_flag: Arc<AtomicBool>,
    event_rx: mpsc::UnboundedReceiver<AsyncEvent>,
    event_tx: mpsc::UnboundedSender<AsyncEvent>,
    pub(crate) logs: Vec<String>,
    pub(crate) log_filter: LogFilter,
    pub(crate) log_search: String,
    pub(crate) is_admin: bool,
    pub(crate) last_error: Option<String>,
    pub(crate) toast: Option<(String, Instant)>,

    // Speed & RTT history
    last_sample_at: Instant,
    last_tx_bytes: u64,
    last_rx_bytes: u64,
    pub(crate) tx_speed_bps: f64,
    pub(crate) rx_speed_bps: f64,
    pub(crate) rtt_history: VecDeque<u64>,
    pub(crate) speed_history: VecDeque<(f64, f64)>,
    pub(crate) chart_mode: ChartMode,

    // Nodes & Import UI state
    pub(crate) probes: HashMap<String, ProbeState>,
    pub(crate) node_picker_open: bool,
    pub(crate) node_editor: Option<NodeEditorState>,
    pub(crate) import_panel_open: bool,
    pub(crate) import_text: String,
    pub(crate) sub_panel_open: bool,
    pub(crate) sub_url_input: String,
    pub(crate) sub_loading: bool,

    // System tray
    _tray: Option<TrayHandle>,
    really_quit: bool,
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
            move |cmd| {
                let _ = tray_tx.send(AsyncEvent::Tray(cmd));
            },
            move || {
                if let Ok(mut guard) = quit_shutdown.lock() {
                    if let Some(tx) = guard.take() {
                        let _ = tx.send(true);
                    }
                }
                if quit_proxy_flag.swap(false, Ordering::SeqCst) {
                    let _ = sys_proxy::disable_system_proxy();
                    logger::log_line("INFO", "退出清理：已关闭 Windows 系统代理");
                }
            },
        );

        let mut app = Self {
            rt,
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
            last_sample_at: Instant::now(),
            last_tx_bytes: 0,
            last_rx_bytes: 0,
            tx_speed_bps: 0.0,
            rx_speed_bps: 0.0,
            rtt_history: VecDeque::with_capacity(60),
            speed_history: VecDeque::with_capacity(60),
            chart_mode: ChartMode::Traffic,
            probes: HashMap::new(),
            node_picker_open: false,
            node_editor: None,
            import_panel_open: false,
            import_text: String::new(),
            sub_panel_open: false,
            sub_url_input: String::new(),
            sub_loading: false,
            _tray: tray,
            really_quit: false,
        };
        app.push_log(if is_admin {
            "客户端已就绪（管理员权限，已支持 TUN 全局虚拟网卡模式）"
        } else {
            "客户端已就绪，点击「一键开启加速」启动 QUIC/UDP 通道"
        });
        app
    }

    pub(crate) fn show_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    pub(crate) fn push_log(&mut self, msg: impl AsRef<str>) {
        let s = msg.as_ref();
        let level = if s.contains("异常") || s.contains("失败") || s.contains("无效") {
            "ERROR"
        } else {
            "INFO"
        };
        logger::log_line(level, s);
        let ts = Local::now().format("%H:%M:%S");
        self.logs.push(format!("[{ts}] {s}"));
        if self.logs.len() > 500 {
            let drain_count = self.logs.len() - 500;
            self.logs.drain(0..drain_count);
        }
    }

    pub(crate) fn start_connect(&mut self) {
        self.last_error = None;
        let Some(node) = self.cfg.active_node().cloned() else {
            let err = "请先在「节点与订阅」中添加至少一个节点".to_string();
            self.last_error = Some(err.clone());
            self.push_log(err);
            return;
        };

        let server: SocketAddr = match node.server.trim().parse() {
            Ok(a) => a,
            Err(_) => {
                let err = format!("节点地址格式无效：{}（应为 IP:端口）", node.server);
                self.last_error = Some(err.clone());
                self.push_log(err);
                return;
            }
        };
        let listen: SocketAddr = match self.cfg.listen_addr.trim().parse() {
            Ok(a) => a,
            Err(_) => {
                let err = format!("本地端口格式无效：{}", self.cfg.listen_addr);
                self.last_error = Some(err.clone());
                self.push_log(err);
                return;
            }
        };

        if self.cfg.proxy_mode == ProxyMode::Tun && !self.is_admin {
            let err = "TUN 模式需要管理员权限，请点击「以管理员身份重启」".to_string();
            self.last_error = Some(err.clone());
            self.push_log(err);
            return;
        }

        save_config(&self.cfg);
        self.stats.reset();
        self.rtt_history.clear();
        self.speed_history.clear();
        self.last_tx_bytes = 0;
        self.last_rx_bytes = 0;
        self.tx_speed_bps = 0.0;
        self.rx_speed_bps = 0.0;
        self.state = ConnState::Connecting;

        if self.cfg.proxy_mode == ProxyMode::SystemProxy {
            match sys_proxy::enable_system_proxy(&listen.to_string()) {
                Ok(()) => {
                    self.sys_proxy_flag.store(true, Ordering::SeqCst);
                    self.push_log(format!("已开启 Windows 系统代理: {listen}"));
                }
                Err(e) => {
                    self.push_log(format!("开启系统代理失败: {e:#}"));
                }
            }
        }

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        if let Ok(mut guard) = self.shutdown_slot.lock() {
            *guard = Some(shutdown_tx);
        }

        let (raw_log_tx, mut raw_log_rx) = mpsc::unbounded_channel::<String>();
        let ev_tx_log = self.event_tx.clone();
        self.rt.spawn(async move {
            while let Some(line) = raw_log_rx.recv().await {
                let _ = ev_tx_log.send(AsyncEvent::Log(line));
            }
        });

        let opts = ClientOptions {
            server,
            server_name: node.sni.trim().to_string(),
            listen,
            token: node.token.trim().to_string(),
            game_dup: node.game_dup,
            enable_tun: self.cfg.proxy_mode == ProxyMode::Tun,
            insecure: node.insecure,
        };
        let stats = self.stats.clone();
        let ev_tx = self.event_tx.clone();

        self.rt.spawn(async move {
            if let Err(e) =
                run_client_with_shutdown(opts, stats, Some(raw_log_tx), shutdown_rx).await
            {
                let _ = ev_tx.send(AsyncEvent::ConnError(format!("{e:#}")));
            }
        });
    }

    pub(crate) fn stop_connect(&mut self) {
        if let Ok(mut guard) = self.shutdown_slot.lock() {
            if let Some(tx) = guard.take() {
                let _ = tx.send(true);
            }
        }
        self.state = ConnState::Disconnected;
        self.stats.reset();
        self.rtt_history.clear();
        self.speed_history.clear();
        self.last_tx_bytes = 0;
        self.last_rx_bytes = 0;
        self.tx_speed_bps = 0.0;
        self.rx_speed_bps = 0.0;

        if self.sys_proxy_flag.swap(false, Ordering::SeqCst) {
            match sys_proxy::disable_system_proxy() {
                Ok(()) => {
                    self.push_log("已自动关闭 Windows 系统代理");
                }
                Err(e) => {
                    self.push_log(format!("关闭系统代理失败: {e:#}"));
                }
            }
        }
    }

    pub(crate) fn probe_single_node(&mut self, node: NodeProfile) {
        let Ok(server) = node.server.trim().parse::<SocketAddr>() else {
            self.probes
                .insert(node.id.clone(), ProbeState::Err("地址无效".into()));
            return;
        };
        self.probes.insert(node.id.clone(), ProbeState::Probing);
        let ev_tx = self.event_tx.clone();
        self.rt.spawn(async move {
            let res = probe_node_rtt(server, node.sni.trim(), node.token.trim(), node.insecure)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = ev_tx.send(AsyncEvent::ProbeResult {
                node_id: node.id,
                result: res,
            });
        });
    }

    pub(crate) fn probe_all_nodes(&mut self) {
        let nodes = self.cfg.nodes.clone();
        for n in nodes {
            self.probe_single_node(n);
        }
        self.show_toast("已开始对全部节点进行 QUIC 延迟测速...");
    }

    pub(crate) fn fetch_subscription(&mut self, url: String) {
        let url_trimmed = url.trim().to_string();
        if !url_trimmed.starts_with("http://") && !url_trimmed.starts_with("https://") {
            self.show_toast("订阅地址必须以 http:// 或 https:// 开头");
            return;
        }
        self.sub_loading = true;
        let ev_tx = self.event_tx.clone();
        self.rt.spawn(async move {
            let res: Result<Vec<NodeProfile>, String> = async {
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(12))
                    .build()
                    .map_err(|e| e.to_string())?;
                let body = client
                    .get(&url_trimmed)
                    .send()
                    .await
                    .map_err(|e| e.to_string())?
                    .error_for_status()
                    .map_err(|e| e.to_string())?
                    .text()
                    .await
                    .map_err(|e| e.to_string())?;
                parse_import_text(&body).map_err(|e| format!("{e:#}"))
            }
            .await;

            let _ = ev_tx.send(AsyncEvent::SubFetched {
                url: url_trimmed,
                result: res,
            });
        });
    }

    pub(crate) fn merge_imported_nodes(
        &mut self,
        incoming: Vec<NodeProfile>,
        source: Option<String>,
    ) -> usize {
        let mut added = 0usize;
        for mut n in incoming {
            n.source = source.clone();
            if let Some(existing) = self.cfg.nodes.iter_mut().find(|x| x.id == n.id) {
                *existing = n;
            } else {
                self.cfg.nodes.push(n);
                added += 1;
            }
        }
        if self.cfg.active_node().is_none() {
            if let Some(first) = self.cfg.nodes.first() {
                self.cfg.active_node_id = first.id.clone();
            }
        }
        save_config(&self.cfg);
        added
    }

    fn poll_async(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.event_rx.try_recv() {
            match ev {
                AsyncEvent::Log(line) => self.push_log(line),
                AsyncEvent::ConnError(err) => {
                    self.last_error = Some(err.clone());
                    self.push_log(format!("连接异常: {err}"));
                    self.stop_connect();
                }
                AsyncEvent::Tray(cmd) => match cmd {
                    TrayCmd::Show => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                    TrayCmd::ToggleConnect => match self.state {
                        ConnState::Disconnected => self.start_connect(),
                        ConnState::Connecting | ConnState::Connected => self.stop_connect(),
                    },
                    TrayCmd::Quit => {
                        self.stop_connect();
                        self.really_quit = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                },
                AsyncEvent::ProbeResult { node_id, result } => match result {
                    Ok(ms) => {
                        self.probes.insert(node_id, ProbeState::Ok(ms));
                    }
                    Err(e) => {
                        self.probes.insert(node_id, ProbeState::Err(e));
                    }
                },
                AsyncEvent::SubFetched { url, result } => {
                    self.sub_loading = false;
                    match result {
                        Ok(nodes) => {
                            let total = nodes.len();
                            let added = self.merge_imported_nodes(nodes, Some(url.clone()));
                            let now = Local::now().format("%Y-%m-%d %H:%M").to_string();
                            if let Some(sub) =
                                self.cfg.subscriptions.iter_mut().find(|s| s.url == url)
                            {
                                sub.last_updated = Some(now);
                            } else {
                                self.cfg.subscriptions.push(SubscriptionEntry {
                                    name: format!("订阅 {}", self.cfg.subscriptions.len() + 1),
                                    url,
                                    last_updated: Some(now),
                                });
                            }
                            save_config(&self.cfg);
                            self.show_toast(format!(
                                "订阅同步成功：共 {total} 个节点（新增 {added}）"
                            ));
                            self.push_log(format!("订阅同步成功：共 {total} 个节点"));
                        }
                        Err(e) => {
                            self.show_toast(format!("订阅拉取失败: {e}"));
                            self.push_log(format!("订阅拉取失败: {e}"));
                        }
                    }
                }
            }
        }

        if self.state == ConnState::Connecting && self.stats.connected.load(Ordering::Relaxed) {
            self.state = ConnState::Connected;
        }

        // The tunnel can die without the client returning an error (idle timeout, server
        // restart, network switch). stats.connected is cleared by the client on exit.
        if self.state == ConnState::Connected && !self.stats.connected.load(Ordering::Relaxed) {
            self.push_log("隧道连接已断开，已自动停止加速");
            self.show_toast("连接已断开：隧道已失效");
            self.stop_connect();
        }

        let elapsed = self.last_sample_at.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let secs = elapsed.as_secs_f64();
            let cur_tx = self.stats.tx_bytes.load(Ordering::Relaxed);
            let cur_rx = self.stats.rx_bytes.load(Ordering::Relaxed);
            if self.state == ConnState::Connected {
                self.tx_speed_bps = cur_tx.saturating_sub(self.last_tx_bytes) as f64 / secs;
                self.rx_speed_bps = cur_rx.saturating_sub(self.last_rx_bytes) as f64 / secs;
                let rtt = self.stats.rtt_ms.load(Ordering::Relaxed);
                if self.rtt_history.len() >= 60 {
                    self.rtt_history.pop_front();
                }
                self.rtt_history.push_back(rtt);
                if self.speed_history.len() >= 60 {
                    self.speed_history.pop_front();
                }
                self.speed_history
                    .push_back((self.rx_speed_bps, self.tx_speed_bps));
            } else {
                self.tx_speed_bps = 0.0;
                self.rx_speed_bps = 0.0;
            }
            self.last_tx_bytes = cur_tx;
            self.last_rx_bytes = cur_rx;
            self.last_sample_at = Instant::now();
        }

        if let Some((_, ts)) = &self.toast {
            if ts.elapsed() > Duration::from_millis(3500) {
                self.toast = None;
            }
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
        Color32::TRANSPARENT.to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_async(ctx);

        if ctx.input(|i| i.viewport().close_requested()) && !self.really_quit && self._tray.is_some()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            return;
        }

        let p = ThemePalette::from_mode(self.cfg.theme_mode);

        let any_probing = self
            .probes
            .values()
            .any(|v| matches!(v, ProbeState::Probing));
        if self.state != ConnState::Disconnected
            || any_probing
            || self.sub_loading
            || self.toast.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        // Frameless rounded window container with single crisp 1px outer border (zero double-stroke)
        egui::CentralPanel::default()
            .frame(Frame::NONE)
            .show(ctx, |ui| {
                let total_rect = ui.max_rect();
                let is_maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
                let radius = if is_maximized { 0u8 } else { 14u8 };

                // Enable native 8-direction edge & corner drag resizing on frameless window
                if !is_maximized {
                    handle_frameless_resize(ctx, total_rect);
                }

                let sidebar_w = 192.0;
                let split_x = total_rect.min.x + sidebar_w;

                // Paint left sidebar & right workspace as two non-overlapping rounded halves
                // so the 4 outer corners have zero background bleed or double-ring artifacts.
                let sidebar_rect = egui::Rect::from_min_max(
                    total_rect.min,
                    Pos2::new(split_x, total_rect.max.y),
                );
                let right_rect = egui::Rect::from_min_max(
                    Pos2::new(split_x, total_rect.min.y),
                    total_rect.max,
                );
                ui.painter().rect_filled(
                    sidebar_rect,
                    CornerRadius {
                        nw: radius,
                        sw: radius,
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
                        ne: radius,
                        se: radius,
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

                // Split into left sidebar + right main workspace
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;

                    ui.allocate_ui_with_layout(
                        Vec2::new(sidebar_w, total_rect.height()),
                        Layout::top_down(Align::LEFT),
                        |ui| {
                            Frame::new()
                                .inner_margin(Margin::symmetric(16, 20))
                                .show(ui, |ui| {
                                    ui.set_min_height((total_rect.height() - 40.0).max(200.0));
                                    self.render_sidebar_contents(ui, ctx, p);
                                });
                        },
                    );

                    let right_w = (total_rect.width() - sidebar_w).max(460.0);
                    ui.allocate_ui_with_layout(
                        Vec2::new(right_w, total_rect.height()),
                        Layout::top_down(Align::LEFT),
                        |ui| {
                            Frame::new()
                                .inner_margin(Margin::symmetric(28, 22))
                                .show(ui, |ui| {
                                    ui.set_width((right_w - 56.0).max(360.0));
                                    ui.set_min_height((total_rect.height() - 44.0).max(200.0));
                                    self.render_right_workspace(ui, ctx, p);
                                });
                        },
                    );
                });

                // Single crisp 1px outer border on top (skipped when maximized flush to screen)
                if !is_maximized {
                    ui.painter().rect_stroke(
                        total_rect,
                        CornerRadius::same(radius),
                        Stroke::new(1.0_f32, p.border_strong),
                        egui::StrokeKind::Inside,
                    );
                }
            });
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

pub(crate) fn relaunch_as_admin() {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        if let Ok(exe) = std::env::current_exe() {
            let exe_str = exe.to_string_lossy().replace('\'', "''");
            let ps_cmd = format!("Start-Process -FilePath '{exe_str}' -Verb RunAs");
            if std::process::Command::new("powershell")
                .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &ps_cmd])
                .creation_flags(0x0800_0000)
                .spawn()
                .is_ok()
            {
                std::process::exit(0);
            }
        }
    }
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
            .with_transparent(true)
            .with_resizable(true)
            .with_inner_size([880.0, 640.0])
            .with_min_inner_size([780.0, 580.0])
            .with_icon(Arc::new(build_app_icon())),
        centered: true,
        persist_window: false,
        ..Default::default()
    };

    eframe::run_native(
        "GameAccel",
        options,
        Box::new(move |cc| Ok(Box::new(GameAccelApp::new(cc, rt.clone())))),
    )
}

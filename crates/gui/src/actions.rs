use crate::admin::{AdminApiErr, AdminEvent};
use crate::logger;
use crate::storage::{self, ProxyMode, SubscriptionEntry, save_config};
use crate::sys_proxy;
use crate::tray::TrayCmd;
use crate::{AsyncEvent, ConnState, GameAccelApp, NavTab, ProbeState, relaunch_as_admin};
use chrono::Local;
use client::classify;
use client::socks5::ClientStats;
use client::{ClientOptions, probe_node_rtt, run_client_with_shutdown};
use eframe::egui;
use protocol::share::{NodeProfile, parse_import_text};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

impl GameAccelApp {
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
        self.pending_mode_reconnect = None;
        self.connect(false);
    }

    pub(crate) fn switch_route_mode(&mut self, mode: storage::RouteMode) {
        if self.cfg.route_mode == mode {
            return;
        }
        self.cfg.route_mode = mode;
        self.route_mode_cell.set(mode);
        save_config(&self.cfg);
        let msg = format!("分流模式已切换为「{}」（对新连接立即生效）", mode.label());
        self.push_log(msg.clone());
        self.toast = Some((msg, Instant::now()));
    }

    pub(crate) fn switch_proxy_mode(&mut self, mode: ProxyMode) {
        if self.cfg.proxy_mode == mode {
            return;
        }
        // TUN needs admin rights: ask first, and only switch once the user agrees to restart
        // elevated (the current mode stays untouched if they cancel).
        if mode == ProxyMode::Tun && !self.is_admin {
            self.tun_admin_prompt = Some(crate::TunAdminPrompt {
                auto_connect: self.state != ConnState::Disconnected,
                wake: true,
            });
            return;
        }
        let prev = self.cfg.proxy_mode;
        self.cfg.proxy_mode = mode;
        save_config(&self.cfg);

        if self.state == ConnState::Disconnected {
            return;
        }

        // SystemProxy <-> Manual share the exact same QUIC + local mixed proxy session:
        // hot-toggle Windows system proxy without dropping the tunnel.
        if prev != ProxyMode::Tun && mode != ProxyMode::Tun {
            match mode {
                ProxyMode::SystemProxy => {
                    let listen = self.cfg.listen_addr.trim().to_string();
                    match sys_proxy::enable_system_proxy(&listen) {
                        Ok(()) => {
                            self.sys_proxy_flag.store(true, Ordering::SeqCst);
                            self.push_log(format!("已热切换为系统代理模式: {listen}"));
                            self.show_toast("已切换为系统代理模式");
                        }
                        Err(e) => {
                            self.push_log(format!("开启系统代理失败: {e:#}"));
                            self.show_toast("开启系统代理失败");
                        }
                    }
                }
                ProxyMode::Manual => {
                    if self.sys_proxy_flag.swap(false, Ordering::SeqCst) {
                        match sys_proxy::disable_system_proxy() {
                            Ok(()) => {
                                self.push_log("已热切换为本地端口模式（已关闭系统代理）");
                                self.show_toast("已切换为本地端口模式");
                            }
                            Err(e) => {
                                self.push_log(format!("关闭系统代理失败: {e:#}"));
                                self.show_toast("关闭系统代理失败");
                            }
                        }
                    } else {
                        self.show_toast("已切换为本地端口模式");
                    }
                }
                ProxyMode::Tun => unreachable!(),
            }
            return;
        }

        // Transitions involving TUN require tearing down the old session first so the
        // local listener port and Wintun adapter finish releasing before reconnecting.
        let mode_label = match mode {
            ProxyMode::Tun => "TUN 全局",
            ProxyMode::SystemProxy => "系统代理",
            ProxyMode::Manual => "本地端口",
        };
        self.stop_connect();
        self.state = ConnState::Connecting;
        self.pending_mode_reconnect = Some(Instant::now() + Duration::from_millis(450));
        self.push_log(format!("正在切换为{mode_label}模式并重新建立连接..."));
        self.show_toast(format!("正在切换为{mode_label}模式..."));
    }

    /// (attempt, seconds until it fires) while a reconnect is pending or in flight.
    pub(crate) fn reconnect_status(&self) -> Option<(u32, u64)> {
        self.reconnect.in_progress().then(|| {
            (
                self.reconnect.attempt(),
                self.reconnect.countdown(Instant::now()),
            )
        })
    }

    fn connect(&mut self, reconnect: bool) -> bool {
        self.last_error = None;
        let Some(node) = self.cfg.active_node().cloned() else {
            let err = "请先在「节点与订阅」中添加至少一个节点".to_string();
            self.last_error = Some(err.clone());
            self.push_log(err);
            return false;
        };

        let server = node.server.trim().to_string();
        if let Err(e) = client::validate_server_addr(&server) {
            let err = format!(
                "节点地址格式无效：{}（{e}，支持 IP:端口 或 域名:端口）",
                node.server
            );
            self.last_error = Some(err.clone());
            self.push_log(err);
            return false;
        }
        let listen: SocketAddr = match self.cfg.listen_addr.trim().parse() {
            Ok(a) => a,
            Err(_) => {
                let err = format!("本地端口格式无效：{}", self.cfg.listen_addr);
                self.last_error = Some(err.clone());
                self.push_log(err);
                return false;
            }
        };

        if self.cfg.proxy_mode == ProxyMode::Tun && !self.is_admin {
            if reconnect {
                let err = "TUN 模式需要管理员权限，请以管理员身份重启".to_string();
                self.last_error = Some(err.clone());
                self.push_log(err);
            } else {
                // User clicked connect: offer to restart elevated and connect automatically.
                self.tun_admin_prompt = Some(crate::TunAdminPrompt {
                    auto_connect: true,
                    wake: true,
                });
            }
            return false;
        }

        if !reconnect {
            save_config(&self.cfg);
        }
        self.session_id = self.session_id.wrapping_add(1);
        let session_id = self.session_id;
        // Allocate fresh stats per session so a winding-down previous task cannot
        // reset `connected` or counters of the newly started session.
        self.stats = Arc::new(ClientStats::default());
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
                    if !self.sys_proxy_flag.swap(true, Ordering::SeqCst) {
                        self.push_log(format!("已开启 Windows 系统代理: {listen}"));
                    }
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
            protocol: node.protocol,
            game_dup: node.game_dup,
            enable_tun: self.cfg.proxy_mode == ProxyMode::Tun,
            insecure: node.insecure,
            brutal_mbps: node.brutal_mbps,
            route_mode: self.route_mode_cell.clone(),
            rules_dir: Some(storage::config_dir().join("rules")),
        };
        let stats = self.stats.clone();
        let ev_tx = self.event_tx.clone();

        self.rt.spawn(async move {
            if let Err(e) =
                run_client_with_shutdown(opts, stats, Some(raw_log_tx), shutdown_rx).await
            {
                let _ = ev_tx.send(AsyncEvent::ConnError(session_id, classify(&e)));
            } else {
                let _ = ev_tx.send(AsyncEvent::ConnEnded(session_id));
            }
        });
        true
    }

    pub(crate) fn stop_connect(&mut self) {
        self.reconnect.reset();
        self.pending_mode_reconnect = None;
        self.session_id = self.session_id.wrapping_add(1);
        if let Ok(mut guard) = self.shutdown_slot.lock()
            && let Some(tx) = guard.take()
        {
            let _ = tx.send(true);
        }
        self.state = ConnState::Disconnected;
        self.stats = Arc::new(ClientStats::default());
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
        let server = node.server.trim().to_string();
        if client::validate_server_addr(&server).is_err() {
            self.probes
                .insert(node.id.clone(), ProbeState::Err("地址无效".into()));
            return;
        }
        self.probes.insert(node.id.clone(), ProbeState::Probing);
        let ev_tx = self.event_tx.clone();
        self.rt.spawn(async move {
            let res = probe_node_rtt(
                &server,
                node.sni.trim(),
                node.token.trim(),
                node.insecure,
                node.protocol,
            )
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
        if self.cfg.active_node().is_none()
            && let Some(first) = self.cfg.nodes.first()
        {
            self.cfg.active_node_id = first.id.clone();
        }
        save_config(&self.cfg);
        added
    }

    /// A retryable connect failure, or a non-retryable one outside a reconnect streak.
    fn fail_connect(&mut self, err: &str) {
        self.push_log(format!("连接异常: {err}"));
        self.stop_connect();
    }

    /// An established tunnel dropped (or a reconnect attempt failed): schedule the next
    /// attempt with backoff, or give up once the attempt budget is exhausted.
    fn on_tunnel_lost(&mut self, reason: &str) {
        let now = Instant::now();
        if self.reconnect.dropped(now).is_some() {
            let attempt = self.reconnect.attempt();
            let secs = self.reconnect.countdown(now);
            self.push_log(format!("{reason}，{secs} 秒后自动重连（第 {attempt} 次）"));
            // Keep the system proxy on: the next attempt restores the local listener.
            self.state = ConnState::Connecting;
            self.tx_speed_bps = 0.0;
            self.rx_speed_bps = 0.0;
        } else {
            let attempts = self.reconnect.attempt();
            self.last_error = Some(format!("{reason}，自动重连 {attempts} 次仍未成功"));
            self.push_log(format!(
                "{reason}，已连续 {attempts} 次重连失败，停止自动重连"
            ));
            self.show_toast("自动重连失败，请检查节点后手动重连");
            self.stop_connect();
        }
    }

    pub(crate) fn poll_async(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.event_rx.try_recv() {
            match ev {
                AsyncEvent::Log(line) => self.push_log(line),
                AsyncEvent::ConnError(sid, err) => {
                    if sid != self.session_id {
                        continue;
                    }
                    let msg = err.message();
                    self.last_error = Some(msg.to_string());
                    if err.is_retryable() {
                        match self.state {
                            ConnState::Connected => {
                                self.on_tunnel_lost(&format!("连接中断: {msg}"));
                            }
                            ConnState::Connecting if self.reconnect.is_waiting() => {
                                // Already scheduled by the stats watcher for this same failure
                                self.push_log(format!("连接中断: {msg}"));
                            }
                            ConnState::Connecting if self.reconnect.in_progress() => {
                                self.on_tunnel_lost(&format!("重连失败: {msg}"));
                            }
                            _ => self.fail_connect(msg),
                        }
                    } else {
                        self.fail_connect(msg);
                    }
                }
                AsyncEvent::ConnEnded(sid) => {
                    if sid != self.session_id {
                        continue;
                    }
                    // Normally the stats watcher below handles the drop. This only matters
                    // when the session died before this frame ever observed it as connected.
                    if self.state == ConnState::Connecting && !self.reconnect.is_waiting() {
                        if self.reconnect.in_progress() {
                            self.on_tunnel_lost("重连失败: 连接建立后立即被关闭");
                        } else {
                            self.fail_connect("连接建立后立即被服务端关闭");
                        }
                    }
                }
                AsyncEvent::Tray(cmd) => match cmd {
                    TrayCmd::Show => {
                        self.window_hidden_to_tray = false;
                        self.tray_popover_open = false;
                        crate::tray::wake_window(ctx);
                        if self.cfg.show_floating_pill {
                            self.floating_pill_raise_frames = 8;
                        }
                    }
                    TrayCmd::ToggleConnect => match self.state {
                        ConnState::Disconnected => self.start_connect(),
                        ConnState::Connecting | ConnState::Connected => self.stop_connect(),
                    },
                    TrayCmd::TogglePopover(px, py) => {
                        if self.tray_popover_open {
                            self.tray_popover_open = false;
                            self.tray_popover_ever_focused = false;
                        } else {
                            let ppp = ctx.pixels_per_point().max(1.0);
                            let pop_h =
                                crate::views::tray_popover::desired_popover_height(&self.cfg);
                            self.tray_popover_pos = crate::tray::clamp_popover_position(
                                px,
                                py,
                                crate::views::tray_popover::TRAY_POPOVER_W,
                                pop_h,
                                ppp,
                            );
                            self.tray_popover_just_opened = true;
                            self.tray_popover_ever_focused = false;
                            self.tray_popover_search.clear();
                            self.tray_popover_open = true;
                        }
                    }
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
                AsyncEvent::Admin(aev) => match aev {
                    AdminEvent::LoginResult { server_id, result } => {
                        self.admin.busy = false;
                        if self.cfg.active_admin_server_id != server_id {
                            continue;
                        }
                        match result {
                            Ok(cookie) => {
                                self.admin.logged_in_server_id = Some(server_id);
                                self.admin.session_cookie = Some(cookie);
                                self.admin.login_pass.clear();
                                self.admin.login_error = None;
                                self.show_toast("已登录服务端管理后台");
                                self.admin_refresh();
                            }
                            Err(e) => {
                                self.admin.login_error = Some(e.clone());
                                self.show_toast(format!("登录失败: {e}"));
                            }
                        }
                    }
                    AdminEvent::DashboardData { server_id, result } => {
                        self.admin.busy = false;
                        if !self.admin.is_logged_in_for(&server_id) {
                            continue;
                        }
                        match result {
                            Ok((ov, links)) => {
                                self.admin.overview = Some(ov);
                                self.admin.links = links;
                                self.admin.last_refresh = Some(Instant::now());
                            }
                            Err(AdminApiErr::Unauthorized) => {
                                self.admin.clear_session();
                                self.admin.login_error = Some("会话已过期，请重新登录".to_string());
                                self.show_toast("管理会话已过期，请重新登录");
                            }
                            Err(AdminApiErr::Message(e)) => {
                                self.show_toast(format!("刷新失败: {e}"));
                            }
                        }
                    }
                    AdminEvent::MutationResult {
                        server_id,
                        ok_msg,
                        result,
                    } => {
                        self.admin.busy = false;
                        if !self.admin.is_logged_in_for(&server_id) {
                            continue;
                        }
                        match result {
                            Ok(()) => {
                                self.admin.confirm_action = None;
                                self.show_toast(ok_msg);
                                self.admin_refresh();
                            }
                            Err(AdminApiErr::Unauthorized) => {
                                self.admin.clear_session();
                                self.admin.login_error = Some("会话已过期，请重新登录".to_string());
                                self.show_toast("管理会话已过期，请重新登录");
                            }
                            Err(AdminApiErr::Message(e)) => {
                                self.show_toast(format!("操作失败: {e}"));
                            }
                        }
                    }
                },
            }
        }

        if self.state == ConnState::Connecting && self.stats.connected.load(Ordering::Relaxed) {
            self.state = ConnState::Connected;
            self.last_error = None;
            self.reconnect.connected(Instant::now());
            if self.reconnect.in_progress() {
                self.show_toast("自动重连成功");
            }
        }

        // The tunnel can die without the client returning an error (idle timeout, server
        // restart, network switch). stats.connected is cleared by the client on exit.
        if self.state == ConnState::Connected && !self.stats.connected.load(Ordering::Relaxed) {
            self.on_tunnel_lost("连接已断开");
        }

        if let Some(due) = self.pending_mode_reconnect
            && Instant::now() >= due
        {
            self.pending_mode_reconnect = None;
            if !self.connect(false) {
                self.stop_connect();
            }
        }

        if self.state == ConnState::Connecting
            && self.reconnect.take_due(Instant::now())
            && !self.connect(true)
        {
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

        if let Some((_, ts)) = &self.toast
            && ts.elapsed() > Duration::from_millis(2700)
        {
            self.toast = None;
        }

        if self.active_tab == NavTab::Admin
            && !self.admin.busy
            && self
                .admin
                .is_logged_in_for(&self.cfg.active_admin_server_id)
            && self
                .admin
                .last_refresh
                .is_some_and(|t| t.elapsed() >= Duration::from_secs(5))
        {
            self.admin_refresh();
        }
    }
}

impl GameAccelApp {
    /// Drive periodic update checks and surface a one-time toast for a new version.
    pub(crate) fn tick_updater(&mut self, ctx: &egui::Context) {
        let rt = self.rt.clone();
        self.updater
            .tick_auto_check(self.cfg.auto_check_update, &rt, ctx);
        if let crate::updater::UpdateStatus::Available(info) = self.updater.status()
            && self.update_notified.as_deref() != Some(info.version.as_str())
        {
            self.update_notified = Some(info.version.clone());
            self.push_log(format!(
                "发现新版本 v{}，可在「设置 → 软件更新」中一键升级",
                info.version
            ));
            self.show_toast(format!("发现新版本 v{}，可在设置中一键更新", info.version));
        }
    }

    /// Restore network state, launch the downloaded installer and exit so it can replace files.
    pub(crate) fn install_update(&mut self, installer: &std::path::Path) {
        if self.state != ConnState::Disconnected {
            self.stop_connect();
        }
        save_config(&self.cfg);
        match crate::updater::launch_installer(installer, self.is_admin) {
            Ok(()) => {
                logger::log_line("INFO", "已启动新版本安装程序，退出当前版本以完成升级");
                self.really_quit = true;
                std::thread::spawn(|| {
                    // Leave a moment for async teardown (routes / proxy) before exiting.
                    std::thread::sleep(Duration::from_millis(400));
                    crate::hard_exit(0);
                });
            }
            Err(e) => {
                self.push_log(format!("{e:#}"));
                self.show_toast(format!("{e:#}"));
            }
        }
    }
}

impl GameAccelApp {
    /// Restart this app elevated (UAC). The current session is only torn down once the
    /// elevated instance has actually been started, so cancelling UAC changes nothing.
    pub(crate) fn restart_as_admin(&mut self, auto_connect: bool) {
        save_config(&self.cfg);
        match relaunch_as_admin(if auto_connect { "--connect" } else { "" }) {
            Ok(()) => {
                logger::log_line("INFO", "已以管理员身份启动新实例，当前实例退出");
                if self.state != ConnState::Disconnected {
                    self.stop_connect();
                }
                self.really_quit = true;
                std::thread::spawn(|| {
                    // Let async teardown (listener, routes, system proxy) finish first.
                    std::thread::sleep(Duration::from_millis(400));
                    crate::hard_exit(0);
                });
            }
            Err(e) => {
                self.push_log(format!("以管理员身份重启：{e}"));
                self.show_toast(e);
            }
        }
    }

    /// "TUN needs admin — restart elevated?" confirmation dialog.
    pub(crate) fn render_tun_admin_prompt(&mut self, ctx: &egui::Context, p: crate::ThemePalette) {
        use crate::widgets::{aligned_row, modal_dialog, primary_btn, soft_btn};
        use eframe::egui::{Align, Layout, RichText};

        let Some(prompt) = self.tun_admin_prompt.as_mut() else {
            return;
        };
        if prompt.wake {
            prompt.wake = false;
            self.window_hidden_to_tray = false;
            self.tray_popover_open = false;
            crate::tray::wake_window(ctx);
        }
        let auto_connect = prompt.auto_connect;

        let mut confirm = false;
        let mut cancel = false;
        let dismissed = modal_dialog(
            ctx,
            "tun_admin_prompt",
            "需要管理员权限",
            380.0,
            p,
            |ui| {
                ui.label(
                    RichText::new(if auto_connect {
                        "启用 TUN 全局虚拟网卡需要管理员权限，授权重启后将自动开启加速。"
                    } else {
                        "启用 TUN 全局虚拟网卡需要管理员权限，是否现在重启程序？"
                    })
                    .size(crate::theme::FONT_SM)
                    .color(p.text_secondary),
                );
                ui.add_space(14.0);
                aligned_row(ui, 30.0, |ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if primary_btn(ui, "管理员重启", p).clicked() {
                            confirm = true;
                        }
                        if soft_btn(ui, "取消", p).clicked() {
                            cancel = true;
                        }
                    });
                });
            },
        );

        if confirm {
            self.tun_admin_prompt = None;
            self.cfg.proxy_mode = ProxyMode::Tun;
            self.restart_as_admin(auto_connect);
        } else if cancel || dismissed {
            self.tun_admin_prompt = None;
            if self.cfg.proxy_mode == ProxyMode::Tun {
                self.cfg.proxy_mode = ProxyMode::SystemProxy;
                save_config(&self.cfg);
                self.show_toast("已取消，已改回系统代理");
            } else {
                self.show_toast("已取消，继续使用当前模式");
            }
        }
    }
}

//! Asymmetric Bento Desktop Studio Dashboard (`840 × 548`), matching `v03_cockpit_preview.html`
//! pixel-for-pixel via `bento_card_exact` deterministic geometry locks.

use crate::storage::{ProxyMode, RouteMode, format_bytes, format_speed};
use crate::theme::{FONT_SM, FONT_TITLE, FONT_XS, ThemePalette, font_bold, font_mono_bold};
use crate::widgets::{
    aligned_row, bento_card_exact, chip_btn, draw_hero_action_button, draw_rtt_sparkline,
    draw_traffic_sparkline, elide_text, hairline_divider, segmented_control,
};
use crate::{ChartMode, ConnState, GameAccelApp, NavTab, ProbeState};
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Layout, Margin, Pos2, RichText, ScrollArea, Sense,
    Stroke, Vec2,
};
use protocol::share::NodeProtocol;
use std::sync::atomic::Ordering;

fn format_hms(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

impl GameAccelApp {
    pub(super) fn render_dashboard(&mut self, ui: &mut egui::Ui, p: ThemePalette) {
        let full_w = ui.available_width();
        let gap = 12.0_f32;
        ui.spacing_mut().item_spacing = Vec2::new(gap, gap);

        // =========================================================================
        // ZONE 1: Full-Width Hero Command Deck (`full_w × 114px` exact outer bounds)
        // =========================================================================
        self.render_hero_deck(ui, full_w, p);

        // =========================================================================
        // ZONE 2: Asymmetric 7:5 Bento Split (Left Telemetry | Right Controls+Lines)
        // =========================================================================
        let bottom_h = ui.available_height().max(240.0);
        let right_w = (full_w * (5.0 / 12.0)).round().clamp(268.0, 360.0);
        let left_w = (full_w - right_w - gap).max(280.0);

        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            self.render_telemetry_bento(ui, left_w, bottom_h, p);

            ui.allocate_ui_with_layout(
                Vec2::new(right_w, bottom_h),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.spacing_mut().item_spacing.y = gap;
                    self.render_control_stack(ui, right_w, bottom_h, gap, p);
                },
            );
        });
    }

    fn render_hero_deck(&mut self, ui: &mut egui::Ui, full_w: f32, p: ThemePalette) {
        let active_node = self.cfg.active_node().cloned();
        let has_nodes = !self.cfg.nodes.is_empty();
        let reconnect = self.reconnect_status();
        let is_live = self.state == ConnState::Connected;

        let hero_border = match self.state {
            ConnState::Connected => p.success_text.gamma_multiply(0.38),
            ConnState::Connecting => p.warn_text.gamma_multiply(0.45),
            ConnState::Disconnected => p.border_subtle,
        };

        let mut toggle_clicked = false;
        let mut jump_to_nodes = false;

        bento_card_exact(
            ui,
            Vec2::new(full_w, 114.0),
            Margin::symmetric(20, 16),
            hero_border,
            p,
            |ui| {
                let inner_w = ui.available_width();
                let action_slot_w = 144.0;
                let col_gap = 16.0;
                let runway_w = (inner_w - action_slot_w - col_gap).max(160.0);

                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = col_gap;

                    // Left Anti-Squeeze Text Runway (strictly locked to `runway_w`)
                    ui.allocate_ui_with_layout(
                        Vec2::new(runway_w, 82.0),
                        Layout::top_down(Align::LEFT),
                        |ui| {
                            ui.set_min_width(runway_w);
                            ui.spacing_mut().item_spacing = Vec2::new(8.0, 5.0);

                            // Row 1 (22px): Pure State Capsule + Timer + Protocol Tag
                            aligned_row(ui, 22.0, |ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                let (badge_txt, dot_col, badge_bg, badge_fg) =
                                    match (self.state, reconnect) {
                                        (ConnState::Connected, _) => (
                                            "隧道保护中".to_string(),
                                            p.success_text,
                                            p.success_soft,
                                            p.success_text,
                                        ),
                                        (ConnState::Connecting, Some((attempt, 0))) => (
                                            format!("重连中 #{attempt}…"),
                                            p.warn_text,
                                            p.warn_soft,
                                            p.warn_text,
                                        ),
                                        (ConnState::Connecting, Some((attempt, secs))) => (
                                            format!("重连中 #{attempt} ({secs}s)"),
                                            p.warn_text,
                                            p.warn_soft,
                                            p.warn_text,
                                        ),
                                        (ConnState::Connecting, None) => (
                                            "正在握手…".to_string(),
                                            p.warn_text,
                                            p.warn_soft,
                                            p.warn_text,
                                        ),
                                        (ConnState::Disconnected, _) => (
                                            "待机就绪".to_string(),
                                            p.text_muted,
                                            p.bg_card_alt,
                                            p.text_muted,
                                        ),
                                    };
                                draw_state_capsule(ui, &badge_txt, dot_col, badge_bg, badge_fg);

                                if is_live {
                                    ui.label(
                                        RichText::new(format_hms(self.stats.uptime_secs()))
                                            .size(FONT_SM)
                                            .monospace()
                                            .color(p.text_muted),
                                    );
                                }

                                if let Some(ref node) = active_node {
                                    let proto_lbl = match node.protocol {
                                        NodeProtocol::Gacl => "GACL · 双通道",
                                        NodeProtocol::Hysteria2 => "HY2 · 双通道",
                                    };
                                    draw_mono_tag(ui, proto_lbl, p);
                                }
                            });

                            // Row 2 (28px): Active Node Headline (18px Native Bold, single-line elided)
                            aligned_row(ui, 28.0, |ui| {
                                if let Some(ref node) = active_node {
                                    let shown_title = elide_text(
                                        ui,
                                        &node.name,
                                        font_bold(FONT_TITLE),
                                        runway_w - 4.0,
                                    );
                                    ui.add(egui::Label::new(
                                        RichText::new(shown_title)
                                            .font(font_bold(FONT_TITLE))
                                            .color(p.text_primary),
                                    ))
                                    .on_hover_text(format!("{}  ({})", node.name, node.server));
                                } else {
                                    let resp = ui.add(
                                        egui::Label::new(
                                            RichText::new("尚未配置加速节点 · 点击导入或新建")
                                                .font(font_bold(FONT_TITLE))
                                                .color(p.accent_text),
                                        )
                                        .sense(Sense::click()),
                                    );
                                    if resp.clicked() {
                                        jump_to_nodes = true;
                                    }
                                }
                            });

                            // Row 3 (20px): Explicit System Takeover & Split-Routing Feedback
                            aligned_row(ui, 20.0, |ui| {
                                let (raw_desc, desc_col) = if let Some(err) = &self.last_error {
                                    (format!("连接提示：{err}"), p.danger_text)
                                } else {
                                    (self.plain_takeover_summary(), p.text_secondary)
                                };
                                let shown_desc = elide_text(
                                    ui,
                                    &raw_desc,
                                    FontId::proportional(FONT_SM),
                                    runway_w - 4.0,
                                );
                                let lbl = ui.label(
                                    RichText::new(shown_desc).size(FONT_SM).color(desc_col),
                                );
                                if raw_desc.chars().count() > 36 {
                                    lbl.on_hover_text(raw_desc);
                                }
                            });
                        },
                    );

                    // Right Fixed 144px Action Slot (flush right, vertically centered)
                    ui.allocate_ui_with_layout(
                        Vec2::new(action_slot_w, 82.0),
                        Layout::right_to_left(Align::Center),
                        |ui| {
                            ui.set_min_width(action_slot_w);
                            if draw_hero_action_button(ui, self.state, has_nodes, p).clicked() {
                                toggle_clicked = true;
                            }
                        },
                    );
                });
            },
        );

        if jump_to_nodes {
            self.active_tab = NavTab::Nodes;
            self.import_panel_open = true;
        }
        if toggle_clicked {
            if !has_nodes && self.state == ConnState::Disconnected {
                self.active_tab = NavTab::Nodes;
                self.import_panel_open = true;
                self.show_toast("请先导入节点链接或新建节点");
            } else {
                match self.state {
                    ConnState::Disconnected => self.start_connect(),
                    ConnState::Connecting | ConnState::Connected => self.stop_connect(),
                }
            }
        }
    }

    fn plain_takeover_summary(&self) -> String {
        let routing = match self.cfg.route_mode {
            RouteMode::Rule => "大陆网站直连，海外与游戏流量走加速隧道".to_string(),
            RouteMode::Dual => match self.cfg.domestic_node() {
                Some(dom) => {
                    let same_as_primary = self
                        .cfg
                        .active_node()
                        .is_some_and(|act| act.id == dom.id);
                    if self.state == ConnState::Connected {
                        if same_as_primary {
                            format!("海外与国内均走「{}」", dom.name)
                        } else if self.stats.secondary_connected.load(Ordering::Relaxed) {
                            let rtt = self.stats.secondary_rtt_ms.load(Ordering::Relaxed);
                            if rtt > 0 {
                                format!("海外走主节点 · 国内走「{}」({rtt} ms)", dom.name)
                            } else {
                                format!("海外走主节点 · 国内走「{}」", dom.name)
                            }
                        } else {
                            format!("海外走主节点 · 国内节点「{}」未连接，暂回退直连", dom.name)
                        }
                    } else {
                        format!("海外走主节点 · 国内走「{}」", dom.name)
                    }
                }
                None => "双节点分流（尚未选择国内出口，暂回退直连）".to_string(),
            },
            RouteMode::Global => "全部连接均通过加密隧道转发".to_string(),
            RouteMode::Direct => "全部流量直连本地物理网卡（用于延迟对比）".to_string(),
        };
        match (self.state, self.cfg.proxy_mode) {
            (ConnState::Connected, ProxyMode::Tun) => {
                format!("已接管全局虚拟网卡 (Wintun) · {routing}")
            }
            (ConnState::Connected, ProxyMode::SystemProxy) => {
                format!("已接管 Windows 系统代理 · {routing}")
            }
            (ConnState::Connected, ProxyMode::Manual) => {
                format!("仅监听本地混合端口（未修改系统设置） · {routing}")
            }
            (ConnState::Connecting, _) => "正在与目标节点协商 1-RTT QUIC 双通道加密连接…".to_string(),
            (ConnState::Disconnected, ProxyMode::Tun) => {
                format!("开启后将接管「全局虚拟网卡 (Wintun)」 · {routing}")
            }
            (ConnState::Disconnected, ProxyMode::SystemProxy) => {
                format!("开启后将启用「Windows 系统代理」 · {routing}")
            }
            (ConnState::Disconnected, ProxyMode::Manual) => {
                format!("开启后仅监听本地端口 {} · {routing}", self.cfg.listen_addr)
            }
        }
    }

    fn render_telemetry_bento(
        &mut self,
        ui: &mut egui::Ui,
        card_w: f32,
        card_h: f32,
        p: ThemePalette,
    ) {
        let lq = self.stats.link_quality();
        let tcp = self.stats.active_tcp.load(Ordering::Relaxed);
        let udp = self.stats.active_udp.load(Ordering::Relaxed);
        let tx = self.stats.tx_bytes.load(Ordering::Relaxed);
        let rx = self.stats.rx_bytes.load(Ordering::Relaxed);
        let is_live = self.state == ConnState::Connected;

        bento_card_exact(
            ui,
            Vec2::new(card_w, card_h),
            Margin::same(16),
            p.border_subtle,
            p,
            |ui| {
                ui.spacing_mut().item_spacing.y = 10.0;

                // 1. Clean 2-Line 4-Column Tabular Metric Strip (matches v03_cockpit_preview.html!)
                let strip_w = ui.available_width();
                let (strip_rect, _) =
                    ui.allocate_exact_size(Vec2::new(strip_w, 42.0), Sense::hover());
                let col_w = strip_w / 4.0;

                let (rtt_val, rtt_col) = if is_live && lq.rtt_ms > 0 {
                    let c = if lq.rtt_ms < 80 {
                        p.success_text
                    } else if lq.rtt_ms < 160 {
                        p.warn_text
                    } else {
                        p.danger_text
                    };
                    (format!("{} ms", lq.rtt_ms), c)
                } else {
                    ("--".to_string(), p.text_muted)
                };

                let (jit_val, jit_col) = if is_live && lq.rtt_ms > 0 {
                    let j = lq.jitter_ms();
                    let c = if j < 5.0 {
                        p.text_primary
                    } else if j < 15.0 {
                        p.warn_text
                    } else {
                        p.danger_text
                    };
                    (format!("±{j:.1} ms"), c)
                } else {
                    ("--".to_string(), p.text_muted)
                };

                let (loss_val, loss_col) = if is_live {
                    let pct = lq.loss_percent();
                    if lq.dup_active {
                        (format!("{pct:.1}% 双发"), p.warn_text)
                    } else if pct >= 1.0 {
                        (format!("{pct:.1}%"), p.warn_text)
                    } else {
                        (format!("{pct:.1}%"), p.text_primary)
                    }
                } else {
                    ("--".to_string(), p.text_muted)
                };

                let (conn_val, conn_col) = if is_live {
                    (format!("{}", tcp + udp), p.text_primary)
                } else {
                    ("0".to_string(), p.text_muted)
                };

                let cells = [
                    ("游戏延迟", rtt_val, rtt_col),
                    ("网络抖动", jit_val, jit_col),
                    ("丢包双发", loss_val, loss_col),
                    ("活跃并发", conn_val, conn_col),
                ];

                let painter = ui.painter();
                for (i, (title, val, val_col)) in cells.iter().enumerate() {
                    let x0 = strip_rect.left() + col_w * (i as f32);
                    let cell_rect = egui::Rect::from_min_size(
                        Pos2::new(x0, strip_rect.top()),
                        Vec2::new(col_w, strip_rect.height()),
                    );
                    if i > 0 {
                        painter.line_segment(
                            [
                                Pos2::new(x0, strip_rect.top() + 2.0),
                                Pos2::new(x0, strip_rect.bottom() - 2.0),
                            ],
                            Stroke::new(1.0_f32, p.border_subtle),
                        );
                    }
                    let pad_x = if i == 0 { 0.0 } else { 10.0 };
                    let max_cell_w = (col_w - pad_x - 4.0).max(32.0);
                    painter.text(
                        Pos2::new(cell_rect.left() + pad_x, cell_rect.top() + 7.5),
                        egui::Align2::LEFT_CENTER,
                        *title,
                        FontId::proportional(11.5),
                        p.text_muted,
                    );
                    let val_font = font_mono_bold(17.0);
                    let shown_val = elide_text(ui, val, val_font.clone(), max_cell_w);
                    painter.text(
                        Pos2::new(cell_rect.left() + pad_x, cell_rect.bottom() - 10.0),
                        egui::Align2::LEFT_CENTER,
                        shown_val,
                        val_font,
                        *val_col,
                    );
                }

                hairline_divider(ui, p);

                // 2. Sparkline Header (26px locked height)
                aligned_row(ui, 26.0, |ui| {
                    if let Some(cm) = segmented_control(
                        ui,
                        "bento_chart_seg",
                        self.chart_mode,
                        &[
                            (ChartMode::Traffic, "实时吞吐", None),
                            (ChartMode::Rtt, "延迟曲线", None),
                        ],
                        Vec2::new(138.0, 25.0),
                        11.5,
                        p,
                    ) {
                        self.chart_mode = cm;
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        match self.chart_mode {
                            ChartMode::Traffic => {
                                let (rx_txt, tx_txt) = if is_live {
                                    (
                                        format!("↓ {}", format_speed(self.telemetry.rx_speed_bps)),
                                        format!("↑ {}", format_speed(self.telemetry.tx_speed_bps)),
                                    )
                                } else {
                                    ("↓ 0 B/s".to_string(), "↑ 0 B/s".to_string())
                                };
                                ui.label(
                                    RichText::new(tx_txt)
                                        .size(11.5)
                                        .monospace()
                                        .color(p.text_muted),
                                )
                                .on_hover_text(format!("累计上传 {}", format_bytes(tx)));
                                ui.label(
                                    RichText::new(rx_txt)
                                        .size(11.5)
                                        .monospace()
                                        .color(p.text_secondary),
                                )
                                .on_hover_text(format!("累计下载 {}", format_bytes(rx)));
                            }
                            ChartMode::Rtt => {
                                let sec_live = is_live
                                    && self.cfg.route_mode == RouteMode::Dual
                                    && self.stats.secondary_connected.load(Ordering::Relaxed);
                                let sec_rtt = self.stats.secondary_rtt_ms.load(Ordering::Relaxed);
                                if sec_live
                                    && sec_rtt > 0
                                    && let Some(&last) = self.telemetry.rtt_history.back()
                                {
                                    // Right-to-left layout: render domestic legend first, then primary legend
                                    ui.label(
                                        RichText::new(format!("国内 {sec_rtt} ms"))
                                            .size(11.5)
                                            .monospace()
                                            .color(p.text_secondary),
                                    );
                                    let (d2, _) = ui.allocate_exact_size(
                                        Vec2::splat(7.0),
                                        Sense::hover(),
                                    );
                                    ui.painter().circle_filled(d2.center(), 3.0, p.success_text);
                                    ui.add_space(4.0);
                                    ui.label(
                                        RichText::new(format!("主 {last} ms"))
                                            .size(11.5)
                                            .monospace()
                                            .color(p.text_secondary),
                                    );
                                    let (d1, _) = ui.allocate_exact_size(
                                        Vec2::splat(7.0),
                                        Sense::hover(),
                                    );
                                    ui.painter().circle_filled(d1.center(), 3.0, p.data_primary);
                                } else {
                                    let summary = if is_live
                                        && let Some(&last) = self.telemetry.rtt_history.back()
                                    {
                                        let avg: u64 = self
                                            .telemetry
                                            .rtt_history
                                            .iter()
                                            .sum::<u64>()
                                            / self.telemetry.rtt_history.len().max(1) as u64;
                                        format!("当前 {last} ms · 均值 {avg} ms")
                                    } else {
                                        "1s 实时采样".to_string()
                                    };
                                    ui.label(
                                        RichText::new(summary)
                                            .size(11.5)
                                            .monospace()
                                            .color(p.text_muted),
                                    );
                                }
                            }
                        }
                    });
                });

                // 3. Live Canvas filling exact remaining height
                match self.chart_mode {
                    ChartMode::Traffic => {
                        draw_traffic_sparkline(ui, &self.telemetry.speed_history, is_live, p)
                    }
                    ChartMode::Rtt => {
                        let sec_hist = (self.cfg.route_mode == RouteMode::Dual)
                            .then_some(&self.telemetry.secondary_rtt_history);
                        draw_rtt_sparkline(
                            ui,
                            &self.telemetry.rtt_history,
                            sec_hist,
                            is_live,
                            p,
                        )
                    }
                }
            },
        );
    }

    fn render_control_stack(
        &mut self,
        ui: &mut egui::Ui,
        col_w: f32,
        col_h: f32,
        gap: f32,
        p: ThemePalette,
    ) {
        let is_dual = self.cfg.route_mode == RouteMode::Dual;
        let top_card_h = if is_dual { 132.0_f32 } else { 94.0_f32 };
        let mut picked_proxy: Option<ProxyMode> = None;
        let mut picked_route: Option<RouteMode> = None;
        let mut picked_domestic_node: Option<String> = None;
        let mut domestic_trigger_rect: Option<egui::Rect> = None;
        let domestic_popup_id = egui::Id::new("bento_domestic_node_popup");
        let mut domestic_popup_open = ui
            .ctx()
            .memory(|m| m.data.get_temp::<bool>(domestic_popup_id).unwrap_or(false));
        if !is_dual {
            domestic_popup_open = false;
        }

        // Top Card (94px standard / 132px when Dual mode reveals the inline domestic picker)
        bento_card_exact(
            ui,
            Vec2::new(col_w, top_card_h),
            Margin::symmetric(14, 13),
            p.border_subtle,
            p,
            |ui| {
                ui.spacing_mut().item_spacing.y = 10.0;

                aligned_row(ui, 28.0, |ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    ui.label(
                        RichText::new("接管")
                            .font(font_bold(FONT_SM))
                            .color(p.text_secondary),
                    );
                    let seg_w = ui.available_width().max(140.0);
                    picked_proxy = segmented_control(
                        ui,
                        "bento_proxy_seg",
                        self.cfg.proxy_mode,
                        &[
                            (
                                ProxyMode::Tun,
                                "游戏 TUN",
                                Some("Wintun 虚拟网卡接管全局 TCP/UDP，游戏加速首选"),
                            ),
                            (
                                ProxyMode::SystemProxy,
                                "系统代理",
                                Some("自动配置 Windows 系统代理，适合网页与桌面应用"),
                            ),
                            (
                                ProxyMode::Manual,
                                "仅端口",
                                Some("仅监听本地混合端口，不改动系统网络配置"),
                            ),
                        ],
                        Vec2::new(seg_w, 28.0),
                        FONT_SM,
                        p,
                    );
                });

                aligned_row(ui, 28.0, |ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    ui.label(
                        RichText::new("分流")
                            .font(font_bold(FONT_SM))
                            .color(p.text_secondary),
                    );
                    let seg_w = ui.available_width().max(140.0);
                    picked_route = segmented_control(
                        ui,
                        "bento_route_seg",
                        self.cfg.route_mode,
                        &[
                            (
                                RouteMode::Rule,
                                "大陆直连",
                                Some("大陆网站与 IP 直连，海外与游戏走主节点"),
                            ),
                            (
                                RouteMode::Dual,
                                "双节点",
                                Some("海外走主节点，大陆网站与国内游戏走下方选定节点"),
                            ),
                            (
                                RouteMode::Global,
                                "全局",
                                Some("所有公网流量全部经主节点转发"),
                            ),
                            (
                                RouteMode::Direct,
                                "直连",
                                Some("全部流量直接走本地网卡，方便对比延迟"),
                            ),
                        ],
                        Vec2::new(seg_w, 28.0),
                        11.5,
                        p,
                    );
                });

                if is_dual {
                    aligned_row(ui, 28.0, |ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        ui.label(
                            RichText::new("国内")
                                .font(font_bold(FONT_SM))
                                .color(p.text_secondary),
                        );
                        let picker_w = ui.available_width().max(140.0);
                        let (rect, resp) =
                            ui.allocate_exact_size(Vec2::new(picker_w, 28.0), Sense::click());
                        domestic_trigger_rect = Some(rect);
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if resp.clicked() {
                            domestic_popup_open = !domestic_popup_open;
                        }

                        let painter = ui.painter();
                        let (fill, border) = if domestic_popup_open || resp.hovered() {
                            (p.bg_nav_hover, p.border_strong)
                        } else {
                            (p.bg_card_alt, p.border_subtle)
                        };
                        painter.rect(
                            rect,
                            CornerRadius::same(7),
                            fill,
                            Stroke::new(1.0_f32, border),
                            egui::StrokeKind::Inside,
                        );

                        let dom_node = self.cfg.domestic_node();
                        let same_as_primary = dom_node.is_some_and(|d| {
                            self.cfg.active_node().is_some_and(|a| a.id == d.id)
                        });
                        let dot_col = if self.state == ConnState::Connected {
                            if dom_node.is_some()
                                && (same_as_primary
                                    || self.stats.secondary_connected.load(Ordering::Relaxed))
                            {
                                p.success_text
                            } else {
                                p.warn_text
                            }
                        } else if dom_node.is_some() {
                            p.accent
                        } else {
                            p.border_strong
                        };
                        painter.circle_filled(
                            Pos2::new(rect.left() + 11.0, rect.center().y),
                            3.0,
                            dot_col,
                        );

                        let (raw_label, lbl_col) = match dom_node {
                            Some(n) => (n.name.as_str(), p.text_primary),
                            None => ("选择国内出口节点", p.text_muted),
                        };
                        let shown = elide_text(
                            ui,
                            raw_label,
                            FontId::proportional(FONT_SM),
                            (picker_w - 42.0).max(40.0),
                        );
                        let galley = ui.fonts(|f| {
                            f.layout_no_wrap(shown, FontId::proportional(FONT_SM), lbl_col)
                        });
                        painter.galley(
                            Pos2::new(rect.left() + 20.0, rect.center().y - galley.size().y * 0.5),
                            galley,
                            lbl_col,
                        );

                        crate::icons::draw_chevron(
                            painter,
                            Pos2::new(rect.right() - 12.0, rect.center().y),
                            domestic_popup_open,
                            p.text_muted,
                        );
                    });
                }
            },
        );

        // Inline dropdown popup for picking ANY node as the domestic exit (no page jump!)
        if domestic_popup_open && let Some(trigger_rect) = domestic_trigger_rect {
            let popup_pos = Pos2::new(trigger_rect.left(), trigger_rect.bottom() + 4.0);
            let popup_w = trigger_rect.width().max(200.0);
            let area_resp = egui::Area::new(domestic_popup_id)
                .order(egui::Order::Foreground)
                .fixed_pos(popup_pos)
                .show(ui.ctx(), |ui| {
                    egui::Frame::new()
                        .fill(p.bg_card)
                        .stroke(Stroke::new(1.0_f32, p.border_strong))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(Margin::same(5))
                        .shadow(egui::Shadow {
                            offset: [0, 4],
                            blur: 14,
                            spread: 0,
                            color: Color32::from_black_alpha(if p.is_dark { 100 } else { 28 }),
                        })
                        .show(ui, |ui| {
                            ui.set_width((popup_w - 10.0).max(180.0));
                            if self.cfg.nodes.is_empty() {
                                ui.add_space(4.0);
                                ui.label(
                                    RichText::new("暂无可选节点，请先导入线路")
                                        .size(FONT_SM)
                                        .color(p.text_muted),
                                );
                                ui.add_space(4.0);
                            } else {
                                let row_w = ui.available_width();
                                egui::ScrollArea::vertical()
                                    .max_height(196.0)
                                    .show(ui, |ui| {
                                        ui.spacing_mut().item_spacing.y = 2.0;
                                        for node in &self.cfg.nodes {
                                            let is_sel = self.cfg.domestic_node_id.as_deref()
                                                == Some(node.id.as_str());
                                            let (r, resp) = ui.allocate_exact_size(
                                                Vec2::new(row_w, 28.0),
                                                Sense::click(),
                                            );
                                            if resp.hovered() {
                                                ui.ctx().set_cursor_icon(
                                                    egui::CursorIcon::PointingHand,
                                                );
                                            }
                                            if is_sel {
                                                ui.painter().rect_filled(
                                                    r,
                                                    CornerRadius::same(6),
                                                    p.bg_card_alt,
                                                );
                                            } else if resp.hovered() {
                                                ui.painter().rect_filled(
                                                    r,
                                                    CornerRadius::same(6),
                                                    p.bg_nav_hover,
                                                );
                                            }
                                            ui.painter().circle_filled(
                                                Pos2::new(r.left() + 10.0, r.center().y),
                                                2.8,
                                                if is_sel { p.accent } else { p.border_strong },
                                            );

                                            let (rtt_txt, rtt_col) = match self.probes.get(&node.id)
                                            {
                                                Some(ProbeState::Ok(ms)) => (
                                                    format!("{ms} ms"),
                                                    if *ms < 80 {
                                                        p.success_text
                                                    } else if *ms < 160 {
                                                        p.warn_text
                                                    } else {
                                                        p.danger_text
                                                    },
                                                ),
                                                Some(ProbeState::Probing) => {
                                                    ("…".to_string(), p.text_muted)
                                                }
                                                Some(ProbeState::Err(_)) => {
                                                    ("超时".to_string(), p.danger_text)
                                                }
                                                None => ("--".to_string(), p.text_muted),
                                            };
                                            let rtt_galley = ui.fonts(|f| {
                                                f.layout_no_wrap(
                                                    rtt_txt,
                                                    FontId::monospace(10.5),
                                                    rtt_col,
                                                )
                                            });
                                            let rtt_w = rtt_galley.size().x;
                                            let shown = elide_text(
                                                ui,
                                                &node.name,
                                                FontId::proportional(FONT_SM),
                                                (row_w - rtt_w - 34.0).max(50.0),
                                            );
                                            let name_galley = ui.fonts(|f| {
                                                f.layout_no_wrap(
                                                    shown,
                                                    FontId::proportional(FONT_SM),
                                                    p.text_primary,
                                                )
                                            });
                                            ui.painter().galley(
                                                Pos2::new(
                                                    r.left() + 20.0,
                                                    r.center().y - name_galley.size().y * 0.5,
                                                ),
                                                name_galley,
                                                p.text_primary,
                                            );
                                            ui.painter().galley(
                                                Pos2::new(
                                                    r.right() - 8.0 - rtt_w,
                                                    r.center().y - rtt_galley.size().y * 0.5,
                                                ),
                                                rtt_galley,
                                                rtt_col,
                                            );
                                            if resp.clicked() {
                                                picked_domestic_node = Some(node.id.clone());
                                                domestic_popup_open = false;
                                            }
                                        }
                                    });
                            }
                        });
                });

            let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
            let clicked_outside = ui.input(|i| {
                i.pointer.primary_clicked()
                    && i.pointer.interact_pos().is_some_and(|pos| {
                        !area_resp.response.rect.contains(pos) && !trigger_rect.contains(pos)
                    })
            });
            if esc || clicked_outside {
                domestic_popup_open = false;
            }
        }
        ui.ctx()
            .memory_mut(|m| m.data.insert_temp(domestic_popup_id, domestic_popup_open));

        if let Some(m) = picked_proxy {
            self.switch_proxy_mode(m);
        }
        if let Some(r) = picked_route {
            self.switch_route_mode(r);
        }
        if let Some(dom_id) = picked_domestic_node {
            self.switch_domestic_node(dom_id);
        }

        // Bottom Card: Borderless Quick Node Switcher (`快捷换线` + `全部线路 ›`, matches HTML!)
        let bot_card_h = (col_h - top_card_h - gap).max(120.0);
        let mut picked_node_id: Option<String> = None;
        let mut trigger_probe_all = false;
        let mut open_nodes_tab = false;
        let mut open_import = false;

        bento_card_exact(
            ui,
            Vec2::new(col_w, bot_card_h),
            Margin::symmetric(14, 13),
            p.border_subtle,
            p,
            |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                let node_count = self.cfg.nodes.len();
                let any_probing = self
                    .probes
                    .values()
                    .any(|v| matches!(v, ProbeState::Probing));

                aligned_row(ui, 22.0, |ui| {
                    ui.label(
                        RichText::new("快捷换线")
                            .font(font_bold(FONT_SM))
                            .color(p.text_secondary),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        let all_resp = ui.add(
                            egui::Label::new(
                                RichText::new("全部线路 ›")
                                    .font(font_bold(11.5))
                                    .color(p.accent_text),
                            )
                            .sense(Sense::click()),
                        );
                        if all_resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if all_resp.clicked() {
                            open_nodes_tab = true;
                        }

                        if node_count > 0 {
                            let probe_lbl = if any_probing { "测速中…" } else { "测速" };
                            let pr = ui.add(
                                egui::Label::new(
                                    RichText::new(probe_lbl).size(11.5).color(p.text_muted),
                                )
                                .sense(if any_probing {
                                    Sense::hover()
                                } else {
                                    Sense::click()
                                }),
                            );
                            if !any_probing && pr.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                            if !any_probing && pr.clicked() {
                                trigger_probe_all = true;
                            }
                        }
                    });
                });

                let list_avail_h = ui.available_height().max(64.0);
                if self.cfg.nodes.is_empty() {
                    ui.allocate_ui_with_layout(
                        Vec2::new(ui.available_width(), list_avail_h),
                        Layout::top_down(Align::Center),
                        |ui| {
                            ui.add_space((list_avail_h * 0.5 - 24.0).max(4.0));
                            ui.label(
                                RichText::new("尚未添加加速线路")
                                    .size(FONT_XS)
                                    .color(p.text_muted),
                            );
                            ui.add_space(6.0);
                            if chip_btn(ui, "+ 导入或新建", p, false).clicked() {
                                open_import = true;
                            }
                        },
                    );
                } else {
                    let active_id = self.cfg.active_node_id.clone();
                    let is_live = self.state == ConnState::Connected;
                    let live_rtt = self.stats.rtt_ms.load(Ordering::Relaxed);

                    ScrollArea::vertical()
                        .id_salt("bento_quick_nodes_scroll")
                        .max_height(list_avail_h)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.spacing_mut().item_spacing.y = 4.0;

                            for node in &self.cfg.nodes {
                                let is_selected = node.id == active_id;
                                let row_w = ui.available_width();
                                let (rect, resp) =
                                    ui.allocate_exact_size(Vec2::new(row_w, 34.0), Sense::click());
                                if resp.hovered() {
                                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                }
                                let resp =
                                    resp.on_hover_text(format!("{}\n{}", node.name, node.server));

                                // Exact match to HTML: borderless recessed `--c-well` when selected,
                                // transparent when idle, soft `--c-hover` on pointer hover.
                                let bg = if is_selected {
                                    p.bg_card_alt
                                } else if resp.hovered() {
                                    p.bg_nav_hover
                                } else {
                                    Color32::TRANSPARENT
                                };
                                let painter = ui.painter();
                                if bg != Color32::TRANSPARENT {
                                    painter.rect_filled(rect, CornerRadius::same(8), bg);
                                }

                                let dot_col = if is_selected {
                                    if is_live { p.success_text } else { p.accent }
                                } else {
                                    p.border_strong
                                };
                                painter.circle_filled(
                                    Pos2::new(rect.left() + 12.0, rect.center().y),
                                    3.25,
                                    dot_col,
                                );

                                // Strict 56px right-locked tabular RTT slot
                                let rtt_slot_w = 56.0;
                                let (rtt_txt, rtt_col) = if is_selected && is_live && live_rtt > 0 {
                                    let c = if live_rtt < 80 {
                                        p.success_text
                                    } else if live_rtt < 160 {
                                        p.warn_text
                                    } else {
                                        p.danger_text
                                    };
                                    (format!("{live_rtt} ms"), c)
                                } else {
                                    match self.probes.get(&node.id) {
                                        Some(ProbeState::Probing) => {
                                            ("测速…".to_string(), p.text_muted)
                                        }
                                        Some(ProbeState::Ok(ms)) => {
                                            let c = if *ms < 80 {
                                                p.success_text
                                            } else if *ms < 160 {
                                                p.warn_text
                                            } else {
                                                p.danger_text
                                            };
                                            (format!("{ms} ms"), c)
                                        }
                                        Some(ProbeState::Err(_)) => {
                                            ("超时".to_string(), p.danger_text)
                                        }
                                        None => ("--".to_string(), p.text_muted),
                                    }
                                };

                                let rtt_galley = ui.fonts(|f| {
                                    f.layout_no_wrap(rtt_txt, FontId::monospace(11.5), rtt_col)
                                });
                                painter.galley(
                                    Pos2::new(
                                        rect.right() - 10.0 - rtt_galley.size().x,
                                        rect.center().y - rtt_galley.size().y * 0.5,
                                    ),
                                    rtt_galley,
                                    rtt_col,
                                );

                                let name_font = if is_selected {
                                    font_bold(12.5)
                                } else {
                                    FontId::proportional(12.5)
                                };
                                let max_name_w = (rect.width() - 24.0 - rtt_slot_w - 8.0).max(40.0);
                                let shown_name =
                                    elide_text(ui, &node.name, name_font.clone(), max_name_w);
                                let name_galley = ui.fonts(|f| {
                                    f.layout_no_wrap(shown_name, name_font, p.text_primary)
                                });
                                painter.galley(
                                    Pos2::new(
                                        rect.left() + 24.0,
                                        rect.center().y - name_galley.size().y * 0.5,
                                    ),
                                    name_galley,
                                    p.text_primary,
                                );

                                if resp.clicked() && !is_selected {
                                    picked_node_id = Some(node.id.clone());
                                }
                            }
                        });
                }
            },
        );

        if let Some(id) = picked_node_id {
            self.switch_active_node(id);
        }
        if trigger_probe_all {
            self.probe_all_nodes();
        }
        if open_nodes_tab {
            self.active_tab = NavTab::Nodes;
        }
        if open_import {
            self.active_tab = NavTab::Nodes;
            self.import_panel_open = true;
        }
    }
}

fn draw_state_capsule(
    ui: &mut egui::Ui,
    text: &str,
    dot_col: Color32,
    bg: Color32,
    fg: Color32,
) {
    let font = font_bold(11.5);
    let galley = ui.fonts(|f| f.layout_no_wrap(text.to_string(), font, fg));
    let w = galley.size().x + 25.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 22.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(11), bg);
    painter.circle_filled(Pos2::new(rect.left() + 10.0, rect.center().y), 3.25, dot_col);
    painter.galley(
        Pos2::new(rect.left() + 17.5, rect.center().y - galley.size().y * 0.5),
        galley,
        fg,
    );
}

fn draw_mono_tag(ui: &mut egui::Ui, text: &str, p: ThemePalette) {
    let font = FontId::monospace(FONT_XS);
    let galley = ui.fonts(|f| f.layout_no_wrap(text.to_string(), font, p.tag_text));
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(galley.size().x + 12.0, 20.0),
        Sense::hover(),
    );
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(4), p.tag_bg);
    painter.galley(
        Pos2::new(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        p.tag_text,
    );
}

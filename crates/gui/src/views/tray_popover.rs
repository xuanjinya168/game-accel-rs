use crate::icons;
use crate::logger::log_line;
use crate::storage::{AppConfig, ProxyMode, RouteMode, format_speed, save_config};
use crate::theme::{FONT_BODY, FONT_SM, FONT_XS, ThemePalette};
use crate::tray::{apply_win11_rounded_corners, wake_window};
use crate::widgets::{
    accent_chip_btn, aligned_row, chip_btn, elide_text, hairline_divider, pill, segmented_control,
};
use crate::{ConnState, GameAccelApp, NavTab, ProbeState};
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Frame, Layout, Margin, Pos2, RichText, ScrollArea,
    Sense, Stroke, StrokeKind, Vec2,
};
use std::sync::atomic::Ordering;
use std::time::Duration;

pub(crate) const TRAY_POPOVER_W: f32 = 328.0;

/// Computes exact snug height of the tray popover for the given node count so 1..=5 nodes
/// fit flush with zero dead whitespace above the footer, while 6+ nodes scroll with a search bar.
pub(crate) fn desired_popover_height(cfg: &AppConfig) -> f32 {
    let n = cfg.nodes.len();
    let list_h = match n {
        0 => 42.0,
        1..=5 => n as f32 * 30.0 + (n - 1) as f32 * 5.0,
        _ => 145.0,
    };
    let search_block = if n > 5 { 34.0 } else { 0.0 };
    // Exact sum: outer margin (28) + header (26) + card (104) + proxy (28) + route (28)
    // + 2 dividers (2) + node header (26) + footer (26) + 8 vertical 6px gaps (48) = 316.0
    316.0 + search_block + list_h
}

enum PopoverAction {
    OpenMain(Option<NavTab>),
    ToggleConnect,
    ToggleFloatingPill,
    SwitchProxyMode(ProxyMode),
    SwitchRouteMode(RouteMode),
    SelectNode(String),
    ProbeAll,
    CopyListenAddr,
    ForceQuit,
}

impl GameAccelApp {
    pub(crate) fn render_tray_popover(&mut self, ctx: &egui::Context, p: ThemePalette) {
        if !self.tray_popover_open {
            return;
        }

        let pop_h = desired_popover_height(&self.cfg);
        let mut close_popover = false;
        let mut actions: Vec<PopoverAction> = Vec::new();

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("game_accel_tray_popover"),
            egui::ViewportBuilder::default()
                .with_title("GameAccel 托盘面板")
                .with_decorations(false)
                .with_always_on_top()
                .with_taskbar(false)
                .with_resizable(false)
                .with_inner_size([TRAY_POPOVER_W, pop_h])
                .with_position([self.tray_popover_pos.x, self.tray_popover_pos.y]),
            |vctx, _class| {
                if self.tray_popover_just_opened {
                    apply_win11_rounded_corners();
                    vctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    self.tray_popover_just_opened = false;
                }

                let focused = vctx.input(|i| i.viewport().focused);
                if focused == Some(true) {
                    self.tray_popover_ever_focused = true;
                } else if self.tray_popover_ever_focused && focused == Some(false) {
                    close_popover = true;
                }
                if vctx
                    .input(|i| i.key_pressed(egui::Key::Escape) || i.viewport().close_requested())
                {
                    close_popover = true;
                }

                egui::CentralPanel::default()
                    .frame(
                        Frame::new()
                            .fill(p.bg_card)
                            .stroke(Stroke::new(1.0_f32, p.border_strong))
                            .corner_radius(CornerRadius::same(10))
                            .inner_margin(Margin::same(14)),
                    )
                    .show(vctx, |ui| {
                        ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);

                        // 1. Header bar (26px locked height, redundant × close button removed)
                        aligned_row(ui, 26.0, |ui| {
                            let (logo_rect, _) =
                                ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
                            icons::draw_brand_logo(ui.painter(), logo_rect, p);
                            ui.add_space(2.0);
                            ui.label(
                                RichText::new("GameAccel")
                                    .strong()
                                    .size(FONT_BODY)
                                    .color(p.text_primary),
                            );

                            let dot_col = match self.state {
                                ConnState::Connected => p.success_text,
                                ConnState::Connecting => p.accent,
                                ConnState::Disconnected => p.text_muted,
                            };
                            let (dot_r, _) =
                                ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                            ui.painter().circle_filled(dot_r.center(), 3.4, dot_col);

                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if chip_btn(ui, "主界面", p, false).clicked() {
                                    actions.push(PopoverAction::OpenMain(None));
                                    close_popover = true;
                                }

                                let pill_btn = if self.cfg.show_floating_pill {
                                    accent_chip_btn(ui, "悬浮窗", p, false)
                                } else {
                                    chip_btn(ui, "悬浮窗", p, false)
                                };
                                if pill_btn
                                    .on_hover_text(if self.cfg.show_floating_pill {
                                        "隐藏桌面网速悬浮窗"
                                    } else {
                                        "显示桌面网速悬浮窗"
                                    })
                                    .clicked()
                                {
                                    actions.push(PopoverAction::ToggleFloatingPill);
                                }
                            });
                        });

                        // 2. Core connection status card (exact 104px height)
                        Frame::new()
                            .fill(p.bg_card_alt)
                            .stroke(Stroke::new(1.0_f32, p.border_subtle))
                            .corner_radius(CornerRadius::same(10))
                            .inner_margin(Margin::symmetric(12, 10))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.spacing_mut().item_spacing.y = 6.0;

                                let active_node = self.cfg.active_node().cloned();
                                let reconnect = self.reconnect_status();

                                // Row A: Status + pixel-elided node name + protocol pill (22px locked)
                                aligned_row(ui, 22.0, |ui| {
                                    let (status_txt, status_col) = match (self.state, reconnect) {
                                        (ConnState::Connected, _) => ("已加速", p.success_text),
                                        (ConnState::Connecting, Some(_)) => {
                                            ("重连中…", p.accent_text)
                                        }
                                        (ConnState::Connecting, None) => ("连接中…", p.accent_text),
                                        (ConnState::Disconnected, _) => ("未连接", p.text_muted),
                                    };
                                    ui.label(
                                        RichText::new(status_txt)
                                            .size(FONT_SM)
                                            .strong()
                                            .color(status_col),
                                    );
                                    ui.label(RichText::new("·").size(FONT_SM).color(p.text_muted));

                                    let proto_reserve = if active_node.is_some() { 52.0 } else { 0.0 };
                                    let max_name_w = (ui.available_width() - proto_reserve).max(40.0);
                                    let raw_name = active_node
                                        .as_ref()
                                        .map(|n| n.name.as_str())
                                        .unwrap_or("尚未添加节点");
                                    let shown_name = elide_text(
                                        ui,
                                        raw_name,
                                        FontId::proportional(FONT_SM),
                                        max_name_w,
                                    );
                                    ui.label(
                                        RichText::new(shown_name)
                                            .size(FONT_SM)
                                            .strong()
                                            .color(p.text_primary),
                                    );

                                    if let Some(ref node) = active_node {
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                pill(ui, node.protocol.label(), p.tag_bg, p.tag_text);
                                            },
                                        );
                                    }
                                });

                                // Row B: Vector arrows + baseline-unified metrics (18px locked)
                                aligned_row(ui, 18.0, |ui| {
                                    if self.state == ConnState::Connected {
                                        let rtt = self.stats.rtt_ms.load(Ordering::Relaxed);
                                        let rtt_col = if rtt == 0 {
                                            p.text_muted
                                        } else if rtt < 80 {
                                            p.success_text
                                        } else if rtt < 160 {
                                            p.warn_text
                                        } else {
                                            p.danger_text
                                        };
                                        let rtt_txt = if rtt > 0 {
                                            format!("RTT {rtt}ms")
                                        } else {
                                            "RTT --".to_string()
                                        };
                                        ui.label(
                                            RichText::new(rtt_txt)
                                                .size(FONT_XS)
                                                .monospace()
                                                .strong()
                                                .color(rtt_col),
                                        );
                                        ui.add_space(6.0);

                                        let (up_col, down_col) = if p.is_dark {
                                            (
                                                Color32::from_rgb(245, 166, 86),
                                                Color32::from_rgb(92, 214, 128),
                                            )
                                        } else {
                                            (
                                                Color32::from_rgb(217, 119, 36),
                                                Color32::from_rgb(38, 152, 68),
                                            )
                                        };
                                        let (u_rect, _) = ui.allocate_exact_size(
                                            Vec2::new(10.0, 14.0),
                                            Sense::hover(),
                                        );
                                        icons::draw_flow_arrow(
                                            ui.painter(),
                                            u_rect.center(),
                                            true,
                                            up_col,
                                        );
                                        ui.spacing_mut().item_spacing.x = 3.0;
                                        ui.label(
                                            RichText::new(format_speed(self.tx_speed_bps))
                                                .size(FONT_XS)
                                                .monospace()
                                                .color(p.text_secondary),
                                        );
                                        ui.add_space(5.0);

                                        let (d_rect, _) = ui.allocate_exact_size(
                                            Vec2::new(10.0, 14.0),
                                            Sense::hover(),
                                        );
                                        icons::draw_flow_arrow(
                                            ui.painter(),
                                            d_rect.center(),
                                            false,
                                            down_col,
                                        );
                                        ui.label(
                                            RichText::new(format_speed(self.rx_speed_bps))
                                                .size(FONT_XS)
                                                .monospace()
                                                .color(p.text_secondary),
                                        );
                                    } else if let Some(ref node) = active_node {
                                        let srv = elide_text(
                                            ui,
                                            &node.server,
                                            FontId::monospace(FONT_XS),
                                            ui.available_width(),
                                        );
                                        ui.label(
                                            RichText::new(srv)
                                                .size(FONT_XS)
                                                .monospace()
                                                .color(p.text_muted),
                                        );
                                    } else {
                                        ui.label(
                                            RichText::new("尚未配置节点")
                                                .size(FONT_XS)
                                                .color(p.text_muted),
                                        );
                                    }
                                });

                                // Primary connect/disconnect button (32px height, optical galley center)
                                let has_nodes = !self.cfg.nodes.is_empty();
                                let btn_w = ui.available_width();
                                let (btn_rect, btn_resp) =
                                    ui.allocate_exact_size(Vec2::new(btn_w, 32.0), Sense::click());
                                let (fill, border, fg, lbl) = match self.state {
                                    ConnState::Disconnected => {
                                        let f = if btn_resp.is_pointer_button_down_on() {
                                            p.accent.linear_multiply(0.90)
                                        } else if btn_resp.hovered() {
                                            p.accent.linear_multiply(1.06)
                                        } else {
                                            p.accent
                                        };
                                        let label = if has_nodes {
                                            "开启加速"
                                        } else {
                                            "添加节点"
                                        };
                                        (f, Stroke::NONE, Color32::WHITE, label)
                                    }
                                    ConnState::Connecting => {
                                        let f = if btn_resp.hovered() {
                                            p.accent_soft.linear_multiply(0.95)
                                        } else {
                                            p.accent_soft
                                        };
                                        (f, Stroke::new(1.0_f32, p.accent), p.accent_text, "取消连接")
                                    }
                                    ConnState::Connected => {
                                        let f = if btn_resp.hovered() {
                                            p.danger_soft.linear_multiply(0.95)
                                        } else {
                                            p.danger_soft
                                        };
                                        (
                                            f,
                                            Stroke::new(1.0_f32, p.danger_text),
                                            p.danger_text,
                                            "断开加速",
                                        )
                                    }
                                };
                                ui.painter().rect(
                                    btn_rect,
                                    CornerRadius::same(8),
                                    fill,
                                    border,
                                    StrokeKind::Inside,
                                );
                                let btn_galley = ui.fonts(|f| {
                                    f.layout_no_wrap(
                                        lbl.to_string(),
                                        FontId::proportional(FONT_SM),
                                        fg,
                                    )
                                });
                                ui.painter().galley(
                                    Pos2::new(
                                        btn_rect.center().x - btn_galley.size().x * 0.5,
                                        btn_rect.center().y - btn_galley.size().y * 0.5,
                                    ),
                                    btn_galley,
                                    fg,
                                );
                                if btn_resp.clicked() {
                                    if has_nodes || self.state != ConnState::Disconnected {
                                        actions.push(PopoverAction::ToggleConnect);
                                    } else {
                                        actions.push(PopoverAction::OpenMain(Some(NavTab::Nodes)));
                                        close_popover = true;
                                    }
                                }
                            });

                        // 3. Proxy mode & route mode rows (locked 28px height -> 0px center offset!)
                        aligned_row(ui, 28.0, |ui| {
                            ui.label(RichText::new("代理模式").size(FONT_SM).color(p.text_secondary));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if let Some(m) = segmented_control(
                                    ui,
                                    "pop_proxy_mode",
                                    self.cfg.proxy_mode,
                                    &[
                                        (ProxyMode::Tun, "TUN", None),
                                        (ProxyMode::SystemProxy, "系统代理", None),
                                        (ProxyMode::Manual, "仅端口", None),
                                    ],
                                    Vec2::new(216.0, 26.0),
                                    FONT_XS,
                                    p,
                                ) {
                                    actions.push(PopoverAction::SwitchProxyMode(m));
                                }
                            });
                        });

                        aligned_row(ui, 28.0, |ui| {
                            ui.label(RichText::new("分流策略").size(FONT_SM).color(p.text_secondary));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if let Some(r) = segmented_control(
                                    ui,
                                    "pop_route_mode",
                                    self.cfg.route_mode,
                                    &[
                                        (RouteMode::Rule, "规则", None),
                                        (RouteMode::Global, "全局", None),
                                        (RouteMode::Direct, "直连", None),
                                    ],
                                    Vec2::new(216.0, 26.0),
                                    FONT_XS,
                                    p,
                                ) {
                                    actions.push(PopoverAction::SwitchRouteMode(r));
                                }
                            });
                        });

                        hairline_divider(ui, p);

                        // 4. Quick node list header (26px locked height)
                        let node_count = self.cfg.nodes.len();
                        aligned_row(ui, 26.0, |ui| {
                            ui.label(
                                RichText::new(format!("快捷节点 ({node_count})"))
                                    .size(FONT_SM)
                                    .strong()
                                    .color(p.text_secondary),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let any_probing = self
                                    .probes
                                    .values()
                                    .any(|v| matches!(v, ProbeState::Probing));
                                ui.add_enabled_ui(node_count > 0 && !any_probing, |ui| {
                                    if chip_btn(
                                        ui,
                                        if any_probing {
                                            "测速中…"
                                        } else {
                                            "全部测速"
                                        },
                                        p,
                                        false,
                                    )
                                    .clicked()
                                    {
                                        actions.push(PopoverAction::ProbeAll);
                                    }
                                });
                            });
                        });

                        // Search bar only when > 5 nodes (exact 28px height)
                        if node_count > 5 {
                            Frame::new()
                                .fill(p.bg_input)
                                .stroke(Stroke::new(1.0_f32, p.border_subtle))
                                .corner_radius(CornerRadius::same(7))
                                .inner_margin(Margin::symmetric(8, 5))
                                .show(ui, |ui| {
                                    ui.add_sized(
                                        Vec2::new(ui.available_width(), 18.0),
                                        egui::TextEdit::singleline(&mut self.tray_popover_search)
                                            .frame(false)
                                            .font(FontId::proportional(FONT_SM))
                                            .hint_text("筛选节点…"),
                                    );
                                });
                        }

                        // Snug node list (exact list_h height, pixel-accurate elision)
                        let list_h = match node_count {
                            0 => 42.0,
                            1..=5 => node_count as f32 * 30.0 + (node_count - 1) as f32 * 5.0,
                            _ => 145.0,
                        };
                        let filter_q = self.tray_popover_search.trim().to_lowercase();
                        let active_id = self.cfg.active_node_id.clone();

                        ui.allocate_ui_with_layout(
                            Vec2::new(ui.available_width(), list_h),
                            Layout::top_down(Align::LEFT),
                            |ui| {
                                ui.set_min_height(list_h);
                                ScrollArea::vertical()
                                    .id_salt("tray_popover_nodes_scroll")
                                    .max_height(list_h)
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        ui.spacing_mut().item_spacing.y = 5.0;

                                        if self.cfg.nodes.is_empty() {
                                            aligned_row(ui, 42.0, |ui| {
                                                ui.vertical_centered(|ui| {
                                                    ui.label(
                                                        RichText::new("暂无节点，打开主界面导入或新建")
                                                            .size(FONT_SM)
                                                            .color(p.text_muted),
                                                    );
                                                });
                                            });
                                        } else {
                                            for node in &self.cfg.nodes {
                                                if !filter_q.is_empty()
                                                    && !node.name.to_lowercase().contains(&filter_q)
                                                    && !node
                                                        .server
                                                        .to_lowercase()
                                                        .contains(&filter_q)
                                                {
                                                    continue;
                                                }
                                                let is_active = node.id == active_id;
                                                let row_w = ui.available_width();
                                                let (rect, resp) = ui.allocate_exact_size(
                                                    Vec2::new(row_w, 30.0),
                                                    Sense::click(),
                                                );
                                                let resp = resp.on_hover_text(format!(
                                                    "{}\n{}",
                                                    node.name, node.server
                                                ));
                                                let bg = if is_active {
                                                    p.accent_soft
                                                } else if resp.hovered() {
                                                    p.bg_nav_hover
                                                } else {
                                                    p.bg_card_alt
                                                };
                                                let stroke = if is_active {
                                                    Stroke::new(1.0_f32, p.accent)
                                                } else {
                                                    Stroke::new(1.0_f32, p.border_subtle)
                                                };
                                                ui.painter().rect(
                                                    rect,
                                                    CornerRadius::same(7),
                                                    bg,
                                                    stroke,
                                                    StrokeKind::Inside,
                                                );

                                                let dot_col = if is_active {
                                                    p.accent
                                                } else {
                                                    p.border_strong
                                                };
                                                ui.painter().circle_filled(
                                                    Pos2::new(rect.left() + 11.0, rect.center().y),
                                                    3.0,
                                                    dot_col,
                                                );

                                                // Measure right latency badge first for pixel-exact elision
                                                let (rtt_txt, rtt_col) =
                                                    match self.probes.get(&node.id) {
                                                        Some(ProbeState::Probing) => {
                                                            ("测速…".to_string(), p.text_muted)
                                                        }
                                                        Some(ProbeState::Ok(ms)) => {
                                                            let col = if *ms < 80 {
                                                                p.success_text
                                                            } else if *ms < 160 {
                                                                p.warn_text
                                                            } else {
                                                                p.danger_text
                                                            };
                                                            (format!("{ms}ms"), col)
                                                        }
                                                        Some(ProbeState::Err(_)) => {
                                                            ("超时".to_string(), p.danger_text)
                                                        }
                                                        None => ("--".to_string(), p.text_muted),
                                                    };
                                                let rtt_galley = ui.fonts(|f| {
                                                    f.layout_no_wrap(
                                                        rtt_txt,
                                                        FontId::monospace(FONT_XS),
                                                        rtt_col,
                                                    )
                                                });
                                                let rtt_w = rtt_galley.size().x;
                                                ui.painter().galley(
                                                    Pos2::new(
                                                        rect.right() - 10.0 - rtt_w,
                                                        rect.center().y
                                                            - rtt_galley.size().y * 0.5,
                                                    ),
                                                    rtt_galley,
                                                    rtt_col,
                                                );

                                                let name_col = if is_active {
                                                    p.accent_text
                                                } else {
                                                    p.text_primary
                                                };
                                                let max_name_w =
                                                    (rect.width() - 21.0 - rtt_w - 18.0).max(40.0);
                                                let shown_name = elide_text(
                                                    ui,
                                                    &node.name,
                                                    FontId::proportional(FONT_SM),
                                                    max_name_w,
                                                );
                                                let name_galley = ui.fonts(|f| {
                                                    f.layout_no_wrap(
                                                        shown_name,
                                                        FontId::proportional(FONT_SM),
                                                        name_col,
                                                    )
                                                });
                                                ui.painter().galley(
                                                    Pos2::new(
                                                        rect.left() + 21.0,
                                                        rect.center().y
                                                            - name_galley.size().y * 0.5,
                                                    ),
                                                    name_galley,
                                                    name_col,
                                                );

                                                if resp.clicked() && !is_active {
                                                    actions.push(PopoverAction::SelectNode(
                                                        node.id.clone(),
                                                    ));
                                                }
                                            }
                                        }
                                    });
                            },
                        );

                        hairline_divider(ui, p);

                        // 5. Footer bar (26px locked height -> port & 彻底退出 strictly centered!)
                        aligned_row(ui, 26.0, |ui| {
                            let addr = self.cfg.listen_addr.clone();
                            let addr_resp = ui
                                .add(
                                    egui::Label::new(
                                        RichText::new(&addr)
                                            .size(FONT_XS)
                                            .monospace()
                                            .color(p.text_muted),
                                    )
                                    .sense(Sense::click()),
                                )
                                .on_hover_text("点击复制本地代理地址");
                            if addr_resp.clicked() {
                                actions.push(PopoverAction::CopyListenAddr);
                            }

                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if chip_btn(ui, "彻底退出", p, true).clicked() {
                                    actions.push(PopoverAction::ForceQuit);
                                    close_popover = true;
                                }
                            });
                        });
                    });
            },
        );

        if close_popover {
            self.tray_popover_open = false;
            self.tray_popover_ever_focused = false;
        }

        for act in actions {
            match act {
                PopoverAction::OpenMain(tab) => {
                    if let Some(t) = tab {
                        self.active_tab = t;
                    }
                    self.window_hidden_to_tray = false;
                    wake_window(ctx);
                }
                PopoverAction::ToggleConnect => match self.state {
                    ConnState::Disconnected => self.start_connect(),
                    ConnState::Connecting | ConnState::Connected => self.stop_connect(),
                },
                PopoverAction::ToggleFloatingPill => {
                    self.cfg.show_floating_pill = !self.cfg.show_floating_pill;
                    save_config(&self.cfg);
                    self.show_toast(if self.cfg.show_floating_pill {
                        "已开启桌面网速悬浮窗"
                    } else {
                        "已隐藏桌面网速悬浮窗"
                    });
                }
                PopoverAction::SwitchProxyMode(mode) => {
                    self.switch_proxy_mode(mode);
                }
                PopoverAction::SwitchRouteMode(mode) => {
                    self.switch_route_mode(mode);
                }
                PopoverAction::SelectNode(id) => {
                    if self.cfg.active_node_id != id {
                        self.cfg.active_node_id = id;
                        save_config(&self.cfg);
                        if self.state != ConnState::Disconnected {
                            self.stop_connect();
                            self.start_connect();
                        }
                    }
                }
                PopoverAction::ProbeAll => {
                    self.probe_all_nodes();
                }
                PopoverAction::CopyListenAddr => {
                    ctx.copy_text(self.cfg.listen_addr.clone());
                    self.show_toast("已复制本地代理地址");
                }
                PopoverAction::ForceQuit => {
                    log_line(
                        "INFO",
                        "从托盘面板执行彻底退出：清理连接、TUN 路由与系统代理...",
                    );
                    self.stop_connect();
                    self.really_quit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    std::thread::spawn(|| {
                        std::thread::sleep(Duration::from_millis(300));
                        log_line("INFO", "========== GameAccel 正常退出 ==========");
                        crate::hard_exit(0);
                    });
                }
            }
        }
    }
}

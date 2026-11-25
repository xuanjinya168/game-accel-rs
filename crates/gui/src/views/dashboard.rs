use crate::icons;
use crate::storage::{ProxyMode, RouteMode, format_bytes, format_speed, save_config};
use crate::theme::{FONT_BODY, FONT_SM, FONT_TITLE, FONT_XS, ThemePalette};
use crate::widgets::{
    card_frame, draw_hero_power_button, draw_rtt_sparkline, draw_traffic_sparkline,
    hairline_divider, primary_btn,
};
use crate::{ChartMode, ConnState, GameAccelApp, NavTab, ProbeState, relaunch_as_admin};
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Frame, Layout, Margin, Pos2, RichText, ScrollArea,
    Sense, Stroke, Vec2,
};
use std::sync::atomic::Ordering;

impl GameAccelApp {
    pub(super) fn render_dashboard(&mut self, ui: &mut egui::Ui, p: ThemePalette) {
        ScrollArea::vertical()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                self.render_dashboard_inner(ui, p);
            });
    }

    fn render_dashboard_inner(&mut self, ui: &mut egui::Ui, p: ThemePalette) {
        let rtt = self.stats.rtt_ms.load(Ordering::Relaxed);
        let tcp = self.stats.active_tcp.load(Ordering::Relaxed);
        let udp = self.stats.active_udp.load(Ordering::Relaxed);
        let tx = self.stats.tx_bytes.load(Ordering::Relaxed);
        let rx = self.stats.rx_bytes.load(Ordering::Relaxed);
        let has_nodes = !self.cfg.nodes.is_empty();

        // 1. Primary Connection & Telemetry Card
        card_frame(p).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add_space(8.0);

            ui.vertical_centered(|ui| {
                if draw_hero_power_button(
                    ui,
                    self.state,
                    self.reconnect_status(),
                    has_nodes,
                    p,
                )
                .clicked()
                {
                    if !has_nodes {
                        self.active_tab = NavTab::Nodes;
                        self.import_panel_open = true;
                        self.show_toast("请先导入 gacl:// 链接或手动添加节点");
                    } else {
                        match self.state {
                            ConnState::Disconnected => self.start_connect(),
                            ConnState::Connecting | ConnState::Connected => self.stop_connect(),
                        }
                    }
                }

                ui.add_space(14.0);

                // Node Picker Pill (custom Popover menu)
                let mut pill_rect = egui::Rect::NOTHING;
                let mut trigger_clicked = false;
                ui.horizontal(|ui| {
                    let pill_w = 340.0;
                    let pad = ((ui.available_width() - pill_w) * 0.5).max(0.0);
                    ui.add_space(pad);

                    let pill_stroke = if self.node_picker_open {
                        p.accent
                    } else {
                        p.border_strong
                    };
                    let frame_resp = Frame::new()
                        .fill(p.bg_card_alt)
                        .stroke(Stroke::new(1.0_f32, pill_stroke))
                        .corner_radius(CornerRadius::same(10))
                        .inner_margin(Margin::symmetric(8, 4))
                        .show(ui, |ui| {
                            ui.allocate_ui_with_layout(
                                Vec2::new(pill_w - 16.0, 28.0),
                                Layout::left_to_right(Align::Center),
                                |ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;

                                    let active_name = self
                                        .cfg
                                        .active_node()
                                        .map(|n| n.name.clone())
                                        .unwrap_or_else(|| "尚未添加节点 · 点击前往配置".to_string());

                                    let can_switch = self.state == ConnState::Disconnected;
                                    let has_active = self.cfg.active_node().is_some();
                                    let trigger_w = if has_active {
                                        pill_w - 58.0
                                    } else {
                                        pill_w - 16.0
                                    };
                                    let (t_rect, t_resp) = ui.allocate_exact_size(
                                        Vec2::new(trigger_w, 28.0),
                                        if can_switch {
                                            Sense::click()
                                        } else {
                                            Sense::hover()
                                        },
                                    );
                                    if can_switch && (t_resp.hovered() || self.node_picker_open) {
                                        ui.painter().rect_filled(
                                            t_rect,
                                            CornerRadius::same(7),
                                            p.bg_nav_hover,
                                        );
                                    }

                                    let dot_col = match self.state {
                                        ConnState::Connected => p.success_text,
                                        ConnState::Connecting => p.accent,
                                        ConnState::Disconnected => {
                                            if has_active {
                                                p.text_muted
                                            } else {
                                                p.accent
                                            }
                                        }
                                    };
                                    ui.painter().circle_filled(
                                        Pos2::new(t_rect.left() + 10.0, t_rect.center().y),
                                        3.4,
                                        dot_col,
                                    );

                                    ui.painter().text(
                                        Pos2::new(t_rect.left() + 22.0, t_rect.center().y),
                                        egui::Align2::LEFT_CENTER,
                                        &active_name,
                                        FontId::proportional(FONT_BODY),
                                        if !has_active {
                                            p.accent_text
                                        } else if can_switch {
                                            p.text_primary
                                        } else {
                                            p.text_secondary
                                        },
                                    );

                                    if can_switch && has_active {
                                        icons::draw_chevron(
                                            ui.painter(),
                                            Pos2::new(t_rect.right() - 12.0, t_rect.center().y),
                                            self.node_picker_open,
                                            if self.node_picker_open {
                                                p.accent
                                            } else {
                                                p.text_muted
                                            },
                                        );
                                    }

                                    if can_switch && t_resp.clicked() {
                                        if has_active {
                                            trigger_clicked = true;
                                        } else {
                                            self.active_tab = NavTab::Nodes;
                                            self.import_panel_open = true;
                                        }
                                    }

                                    if let Some(node) = self.cfg.active_node().cloned() {
                                        let (div_r, _) = ui.allocate_exact_size(
                                            Vec2::new(1.0, 16.0),
                                            Sense::hover(),
                                        );
                                        ui.painter().rect_filled(
                                            div_r,
                                            CornerRadius::ZERO,
                                            p.border_subtle,
                                        );

                                        let (s_rect, s_resp) = ui.allocate_exact_size(
                                            Vec2::new(26.0, 26.0),
                                            Sense::click(),
                                        );
                                        if s_resp.hovered() {
                                            ui.painter().rect_filled(
                                                s_rect,
                                                CornerRadius::same(7),
                                                p.bg_nav_active,
                                            );
                                        }
                                        icons::draw_share_icon(
                                            ui.painter(),
                                            s_rect.center(),
                                            if s_resp.hovered() {
                                                p.accent
                                            } else {
                                                p.text_secondary
                                            },
                                        );
                                        if s_resp
                                            .on_hover_text("复制 gacl:// 分享链接")
                                            .clicked()
                                        {
                                            ui.ctx().copy_text(node.to_share_uri());
                                            self.show_toast(format!(
                                                "已复制「{}」的分享链接",
                                                node.name
                                            ));
                                        }
                                    }
                                },
                            );
                        })
                        .response;
                    pill_rect = frame_resp.rect;
                });

                if trigger_clicked {
                    self.node_picker_open = !self.node_picker_open;
                }
                if self.state != ConnState::Disconnected || !has_nodes {
                    self.node_picker_open = false;
                }

                // Popover menu anchored cleanly below the node pill
                if self.node_picker_open {
                    let popup_pos = Pos2::new(pill_rect.left(), pill_rect.bottom() + 6.0);
                    let popup_w = pill_rect.width().max(340.0);
                    let nodes_snapshot = self.cfg.nodes.clone();
                    let mut picked_id: Option<String> = None;

                    let area_resp = egui::Area::new(egui::Id::new("node_picker_popover"))
                        .order(egui::Order::Foreground)
                        .fixed_pos(popup_pos)
                        .show(ui.ctx(), |ui| {
                            Frame::new()
                                .fill(p.bg_card)
                                .stroke(Stroke::new(1.0_f32, p.border_subtle))
                                .corner_radius(CornerRadius::same(12))
                                .inner_margin(Margin::same(6))
                                .shadow(egui::Shadow {
                                    offset: [0, 8],
                                    blur: 20,
                                    spread: 0,
                                    color: Color32::from_black_alpha(if p.is_dark { 85 } else { 24 }),
                                })
                                .show(ui, |ui| {
                                    ui.set_width(popup_w - 12.0);
                                    ui.spacing_mut().item_spacing = Vec2::new(0.0, 3.0);

                                    ScrollArea::vertical()
                                        .max_height(210.0)
                                        .show(ui, |ui| {
                                            for node in &nodes_snapshot {
                                                let selected = self.cfg.active_node_id == node.id;
                                                let (r, resp) = ui.allocate_exact_size(
                                                    Vec2::new(ui.available_width(), 42.0),
                                                    Sense::click(),
                                                );
                                                let bg = if selected {
                                                    p.accent_soft
                                                } else if resp.hovered() {
                                                    p.bg_nav_hover
                                                } else {
                                                    Color32::TRANSPARENT
                                                };
                                                let painter = ui.painter();
                                                painter.rect_filled(r, CornerRadius::same(8), bg);

                                                painter.text(
                                                    Pos2::new(r.left() + 10.0, r.top() + 14.0),
                                                    egui::Align2::LEFT_CENTER,
                                                    &node.name,
                                                    FontId::proportional(FONT_BODY),
                                                    if selected {
                                                        p.accent_text
                                                    } else {
                                                        p.text_primary
                                                    },
                                                );

                                                let sub = format!("{} · {}", node.server, node.sni);
                                                painter.text(
                                                    Pos2::new(r.left() + 10.0, r.bottom() - 11.0),
                                                    egui::Align2::LEFT_CENTER,
                                                    sub,
                                                    FontId::monospace(FONT_XS),
                                                    p.text_muted,
                                                );

                                                let mut right_x = r.right() - 14.0;
                                                if selected {
                                                    icons::draw_check_icon(
                                                        painter,
                                                        Pos2::new(right_x, r.center().y),
                                                        p.accent,
                                                    );
                                                    right_x -= 18.0;
                                                }
                                                if let Some(ProbeState::Ok(ms)) =
                                                    self.probes.get(&node.id)
                                                {
                                                    let c = if *ms < 100 {
                                                        p.success_text
                                                    } else if *ms < 200 {
                                                        p.warn_text
                                                    } else {
                                                        p.danger_text
                                                    };
                                                    painter.text(
                                                        Pos2::new(right_x, r.center().y),
                                                        egui::Align2::RIGHT_CENTER,
                                                        format!("{ms} ms"),
                                                        FontId::monospace(FONT_XS),
                                                        c,
                                                    );
                                                }

                                                if resp.clicked() {
                                                    picked_id = Some(node.id.clone());
                                                }
                                            }
                                        });
                                });
                        })
                        .response;

                    if let Some(id) = picked_id {
                        self.cfg.active_node_id = id;
                        save_config(&self.cfg);
                        self.node_picker_open = false;
                    } else if ui.ctx().input(|i| i.key_pressed(egui::Key::Escape)) {
                        self.node_picker_open = false;
                    } else if !trigger_clicked
                        && ui.ctx().input(|i| i.pointer.any_pressed())
                        && let Some(pos) = ui.ctx().input(|i| i.pointer.interact_pos())
                            && !area_resp.rect.contains(pos) && !pill_rect.contains(pos) {
                                self.node_picker_open = false;
                            }
                }
            });

            ui.add_space(18.0);
            hairline_divider(ui, p);
            ui.add_space(16.0);

            // 4-column telemetry strip with 1px hairline vertical dividers
            let is_live = self.state == ConnState::Connected;
            let metric_col = if is_live { p.text_primary } else { p.text_muted };
            let strip_rect = ui
                .scope(|ui| {
                    ui.columns(4, |cols| {
                        cols[0].vertical_centered(|ui| {
                            let (val, col) = if is_live {
                                let c = if rtt < 100 {
                                    p.success_text
                                } else if rtt < 200 {
                                    p.warn_text
                                } else {
                                    p.danger_text
                                };
                                (format!("{rtt} ms"), c)
                            } else {
                                ("--".to_string(), p.text_muted)
                            };
                            ui.label(
                                RichText::new(val)
                                    .size(FONT_TITLE)
                                    .strong()
                                    .monospace()
                                    .color(col),
                            );
                            ui.label(RichText::new("QUIC 延迟").size(FONT_XS).color(p.text_muted));
                        });

                        cols[1].vertical_centered(|ui| {
                            ui.label(
                                RichText::new(if is_live {
                                    format_speed(self.rx_speed_bps)
                                } else {
                                    "--".to_string()
                                })
                                .size(FONT_TITLE)
                                .strong()
                                .monospace()
                                .color(metric_col),
                            );
                            ui.label(
                                RichText::new(format!("下载 · 共 {}", format_bytes(rx)))
                                    .size(FONT_XS)
                                    .color(p.text_muted),
                            );
                        });

                        cols[2].vertical_centered(|ui| {
                            ui.label(
                                RichText::new(if is_live {
                                    format_speed(self.tx_speed_bps)
                                } else {
                                    "--".to_string()
                                })
                                .size(FONT_TITLE)
                                .strong()
                                .monospace()
                                .color(metric_col),
                            );
                            ui.label(
                                RichText::new(format!("上传 · 共 {}", format_bytes(tx)))
                                    .size(FONT_XS)
                                    .color(p.text_muted),
                            );
                        });

                        cols[3].vertical_centered(|ui| {
                            ui.label(
                                RichText::new(if is_live {
                                    format!("{}", tcp + udp)
                                } else {
                                    "0".to_string()
                                })
                                .size(FONT_TITLE)
                                .strong()
                                .monospace()
                                .color(metric_col),
                            );
                            ui.label(
                                RichText::new(format!("TCP {tcp} / UDP {udp}"))
                                    .size(FONT_XS)
                                    .color(p.text_muted),
                            );
                        });
                    });
                })
                .response
                .rect;

            let col_w = strip_rect.width() / 4.0;
            for i in 1..4 {
                let x = strip_rect.left() + col_w * (i as f32);
                ui.painter().line_segment(
                    [
                        Pos2::new(x, strip_rect.center().y - 12.0),
                        Pos2::new(x, strip_rect.center().y + 12.0),
                    ],
                    Stroke::new(1.0_f32, p.border_subtle),
                );
            }

            if let Some(err) = &self.last_error {
                ui.add_space(12.0);
                Frame::new()
                    .fill(p.danger_soft)
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(err).size(FONT_SM).color(p.danger_text));
                    });
            }

            ui.add_space(6.0);
        });

        ui.add_space(16.0);

        // 2. Concise Segmented Control for Proxy Mode + calm 1-line subtitle underneath
        Frame::new()
            .fill(p.bg_card_alt)
            .stroke(Stroke::new(1.0_f32, p.border_subtle))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(4))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let modes = [
                    (ProxyMode::Tun, "TUN 全局"),
                    (ProxyMode::SystemProxy, "系统代理"),
                    (ProxyMode::Manual, "本地端口"),
                ];
                ui.columns(3, |cols| {
                    for (idx, (mode, title)) in modes.iter().enumerate() {
                        let selected = self.cfg.proxy_mode == *mode;
                        let (rect, resp) = cols[idx].allocate_exact_size(
                            Vec2::new(cols[idx].available_width(), 32.0),
                            Sense::click(),
                        );
                        let fill = if selected {
                            p.bg_card
                        } else if resp.hovered() {
                            p.bg_nav_hover
                        } else {
                            Color32::TRANSPARENT
                        };
                        let stroke = if selected {
                            Stroke::new(1.0_f32, p.border_subtle)
                        } else {
                            Stroke::NONE
                        };
                        let col = if selected {
                            p.text_primary
                        } else {
                            p.text_muted
                        };
                        let painter = cols[idx].painter();
                        painter.rect(
                            rect,
                            CornerRadius::same(6),
                            fill,
                            stroke,
                            egui::StrokeKind::Inside,
                        );
                        painter.text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            *title,
                            FontId::proportional(FONT_BODY),
                            col,
                        );
                        if resp.clicked() {
                            self.switch_proxy_mode(*mode);
                        }
                    }
                });
            });

        ui.add_space(8.0);
        let mode_desc = match self.cfg.proxy_mode {
            ProxyMode::Tun => "虚拟网卡接管全局 TCP / UDP 流量，适合游戏低延迟加速",
            ProxyMode::SystemProxy => "自动接管 Windows 系统代理，适合浏览器与桌面应用",
            ProxyMode::Manual => "仅开启本地混合代理端口（HTTP / SOCKS5），按需手动指向",
        };
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(mode_desc).size(FONT_SM).color(p.text_muted));
        });

        // Split-routing selector (规则 / 全局 / 直连), applied live to new connections.
        ui.add_space(10.0);
        Frame::new()
            .fill(p.bg_card_alt)
            .stroke(Stroke::new(1.0_f32, p.border_subtle))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(4))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let routes = [
                    (RouteMode::Rule, "规则分流"),
                    (RouteMode::Global, "全局代理"),
                    (RouteMode::Direct, "全部直连"),
                ];
                ui.columns(3, |cols| {
                    for (idx, (mode, title)) in routes.iter().enumerate() {
                        let selected = self.cfg.route_mode == *mode;
                        let (rect, resp) = cols[idx].allocate_exact_size(
                            Vec2::new(cols[idx].available_width(), 28.0),
                            Sense::click(),
                        );
                        let fill = if selected {
                            p.bg_card
                        } else if resp.hovered() {
                            p.bg_nav_hover
                        } else {
                            Color32::TRANSPARENT
                        };
                        let stroke = if selected {
                            Stroke::new(1.0_f32, p.border_subtle)
                        } else {
                            Stroke::NONE
                        };
                        let col = if selected { p.text_primary } else { p.text_muted };
                        let painter = cols[idx].painter();
                        painter.rect(rect, CornerRadius::same(6), fill, stroke, egui::StrokeKind::Inside);
                        painter.text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            *title,
                            FontId::proportional(FONT_SM),
                            col,
                        );
                        if resp.clicked() {
                            self.switch_route_mode(*mode);
                        }
                    }
                });
            });
        ui.add_space(6.0);
        let route_desc = match (self.cfg.route_mode, self.cfg.proxy_mode) {
            (RouteMode::Rule, ProxyMode::Tun) => {
                "国内网站直连物理网卡 + 国内 DNS 分流，国外与游戏走加速隧道"
            }
            (RouteMode::Rule, _) => "国内网站与国内 IP 直连，其余流量走加速线路",
            (RouteMode::Global, _) => "所有流量都走加速线路",
            (RouteMode::Direct, _) => "所有流量直连，不经过加速线路（用于对比测试）",
        };
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(route_desc).size(FONT_SM).color(p.text_muted));
        });

        if self.cfg.proxy_mode == ProxyMode::Tun && !self.is_admin {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("TUN 模式需要管理员权限：")
                        .size(FONT_SM)
                        .color(p.accent_text),
                );
                if primary_btn(ui, "以管理员身份重启", p).clicked() {
                    if self.state != ConnState::Disconnected {
                        self.stop_connect();
                    }
                    save_config(&self.cfg);
                    relaunch_as_admin();
                }
            });
        }

        ui.add_space(16.0);

        // 3. Realtime Traffic Monitor / RTT Trend switchable chart
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            for (mode, label) in [
                (ChartMode::Traffic, "实时流量监控"),
                (ChartMode::Rtt, "延迟波动"),
            ] {
                let active = self.chart_mode == mode;
                let text = RichText::new(label)
                    .size(FONT_SM)
                    .color(if active { p.text_primary } else { p.text_muted });
                let resp = ui.add(
                    egui::Label::new(if active { text.strong() } else { text })
                        .sense(Sense::click()),
                );
                if active {
                    ui.painter().line_segment(
                        [
                            Pos2::new(resp.rect.left(), resp.rect.bottom() + 2.0),
                            Pos2::new(resp.rect.right(), resp.rect.bottom() + 2.0),
                        ],
                        Stroke::new(1.5_f32, p.accent),
                    );
                }
                if resp.clicked() {
                    self.chart_mode = mode;
                }
            }
            if let Some(node) = self.cfg.active_node() {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let info = format!(
                        "UDP BBR · MTU 1200 · {}",
                        if node.game_dup { "丢包自适应双发" } else { "普通模式" }
                    );
                    ui.label(RichText::new(info).size(FONT_XS).color(p.text_muted));
                });
            }
        });
        ui.add_space(6.0);
        match self.chart_mode {
            ChartMode::Traffic => {
                draw_traffic_sparkline(ui, &self.speed_history, self.state == ConnState::Connected, p)
            }
            ChartMode::Rtt => {
                draw_rtt_sparkline(ui, &self.rtt_history, self.state == ConnState::Connected, p)
            }
        }
    }

}

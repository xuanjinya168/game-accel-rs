use crate::icons;
use crate::storage::{ProxyMode, RouteMode, format_bytes, format_speed, save_config};
use crate::theme::{FONT_BODY, FONT_SM, FONT_TITLE, FONT_XS, ThemePalette};
use crate::widgets::{
    aligned_row, card_frame, draw_hero_power_button, draw_rtt_sparkline, draw_traffic_sparkline,
    elide_text, hairline_divider, segmented_control,
};
use crate::{ChartMode, ConnState, GameAccelApp, NavTab, ProbeState};
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

        // 1. Primary Connection & 4-Column Telemetry Card
        card_frame(p).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add_space(4.0);

            ui.vertical_centered(|ui| {
                if draw_hero_power_button(ui, self.state, self.reconnect_status(), has_nodes, p)
                    .clicked()
                {
                    if !has_nodes {
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

                ui.add_space(12.0);

                // Node Picker Pill (custom Popover menu)
                let mut pill_rect = egui::Rect::NOTHING;
                let mut trigger_clicked = false;
                ui.horizontal(|ui| {
                    let pill_w = 332.0;
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
                        .inner_margin(Margin::symmetric(8, 3))
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
                                        .unwrap_or_else(|| "尚未添加节点 · 点击配置".to_string());

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

                                    let chevron_reserve =
                                        if can_switch && has_active { 24.0 } else { 8.0 };
                                    let max_lbl_w =
                                        (t_rect.width() - 22.0 - chevron_reserve).max(40.0);
                                    let shown_lbl = elide_text(
                                        ui,
                                        &active_name,
                                        FontId::proportional(FONT_BODY),
                                        max_lbl_w,
                                    );
                                    let fg = if !has_active {
                                        p.accent_text
                                    } else if can_switch {
                                        p.text_primary
                                    } else {
                                        p.text_secondary
                                    };
                                    let galley = ui.fonts(|f| {
                                        f.layout_no_wrap(
                                            shown_lbl,
                                            FontId::proportional(FONT_BODY),
                                            fg,
                                        )
                                    });
                                    ui.painter().galley(
                                        Pos2::new(
                                            t_rect.left() + 22.0,
                                            t_rect.center().y - galley.size().y * 0.5,
                                        ),
                                        galley,
                                        fg,
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
                                        if s_resp.on_hover_text("复制分享链接").clicked() {
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
                    let popup_w = pill_rect.width().max(332.0);
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
                                    color: Color32::from_black_alpha(if p.is_dark {
                                        85
                                    } else {
                                        24
                                    }),
                                })
                                .show(ui, |ui| {
                                    ui.set_width(popup_w - 12.0);
                                    ui.spacing_mut().item_spacing = Vec2::new(0.0, 3.0);

                                    ScrollArea::vertical().max_height(210.0).show(ui, |ui| {
                                        for node in &nodes_snapshot {
                                            let selected = self.cfg.active_node_id == node.id;
                                            let (r, resp) = ui.allocate_exact_size(
                                                Vec2::new(ui.available_width(), 40.0),
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

                                            let mut right_x = r.right() - 12.0;
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
                                                let rtt_galley = ui.fonts(|f| {
                                                    f.layout_no_wrap(
                                                        format!("{ms} ms"),
                                                        FontId::monospace(FONT_XS),
                                                        c,
                                                    )
                                                });
                                                let rw = rtt_galley.size().x;
                                                painter.galley(
                                                    Pos2::new(
                                                        right_x - rw,
                                                        r.center().y - rtt_galley.size().y * 0.5,
                                                    ),
                                                    rtt_galley,
                                                    c,
                                                );
                                                right_x -= rw + 10.0;
                                            }

                                            let max_w = (right_x - r.left() - 14.0).max(60.0);
                                            let title_str = elide_text(
                                                ui,
                                                &node.name,
                                                FontId::proportional(FONT_BODY),
                                                max_w,
                                            );
                                            painter.text(
                                                Pos2::new(r.left() + 10.0, r.top() + 13.0),
                                                egui::Align2::LEFT_CENTER,
                                                title_str,
                                                FontId::proportional(FONT_BODY),
                                                if selected {
                                                    p.accent_text
                                                } else {
                                                    p.text_primary
                                                },
                                            );

                                            let sub_str = elide_text(
                                                ui,
                                                &node.server,
                                                FontId::monospace(FONT_XS),
                                                max_w,
                                            );
                                            painter.text(
                                                Pos2::new(r.left() + 10.0, r.bottom() - 10.5),
                                                egui::Align2::LEFT_CENTER,
                                                sub_str,
                                                FontId::monospace(FONT_XS),
                                                p.text_muted,
                                            );

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
                        && !area_resp.rect.contains(pos)
                        && !pill_rect.contains(pos)
                    {
                        self.node_picker_open = false;
                    }
                }
            });

            ui.add_space(14.0);
            hairline_divider(ui, p);
            ui.add_space(12.0);

            // 4-column telemetry strip with 1px hairline vertical dividers
            let is_live = self.state == ConnState::Connected;
            let metric_col = if is_live {
                p.text_primary
            } else {
                p.text_muted
            };
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
                                RichText::new(format!("下载 · {}", format_bytes(rx)))
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
                                RichText::new(format!("上传 · {}", format_bytes(tx)))
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
                ui.add_space(10.0);
                Frame::new()
                    .fill(p.danger_soft)
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(err).size(FONT_SM).color(p.danger_text));
                    });
            }

            ui.add_space(2.0);
        });

        ui.add_space(4.0);

        // 2. Side-by-Side 2-Column Mode Control Cards (hover tooltips replace bloated subtitle lines)
        let mut picked_proxy: Option<ProxyMode> = None;
        let mut picked_route: Option<RouteMode> = None;
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            ui.columns(2, |cols| {
                card_frame(p)
                    .inner_margin(Margin::symmetric(14, 7))
                    .show(&mut cols[0], |ui| {
                        aligned_row(ui, 30.0, |ui| {
                            ui.label(
                                RichText::new("代理模式")
                                    .size(FONT_SM)
                                    .color(p.text_secondary),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let w = ui.available_width().clamp(150.0, 268.0);
                                picked_proxy = segmented_control(
                                    ui,
                                    "dash_proxy_seg",
                                    self.cfg.proxy_mode,
                                    &[
                                        (
                                            ProxyMode::Tun,
                                            "TUN 全局",
                                            Some("虚拟网卡接管全局 TCP/UDP，适合游戏加速"),
                                        ),
                                        (
                                            ProxyMode::SystemProxy,
                                            "系统代理",
                                            Some("自动设置系统代理，适合浏览器与桌面软件"),
                                        ),
                                        (
                                            ProxyMode::Manual,
                                            "仅端口",
                                            Some("仅监听本地混合端口，不修改系统代理"),
                                        ),
                                    ],
                                    Vec2::new(w, 30.0),
                                    FONT_SM,
                                    p,
                                );
                            });
                        });
                    });

                card_frame(p)
                    .inner_margin(Margin::symmetric(14, 7))
                    .show(&mut cols[1], |ui| {
                        aligned_row(ui, 30.0, |ui| {
                            ui.label(
                                RichText::new("分流策略")
                                    .size(FONT_SM)
                                    .color(p.text_secondary),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let w = ui.available_width().clamp(140.0, 244.0);
                                picked_route = segmented_control(
                                    ui,
                                    "dash_route_seg",
                                    self.cfg.route_mode,
                                    &[
                                        (
                                            RouteMode::Rule,
                                            "规则",
                                            Some("大陆网站与 IP 直连，海外与游戏走加速隧道"),
                                        ),
                                        (
                                            RouteMode::Global,
                                            "全局",
                                            Some("全部连接均通过加速隧道转发"),
                                        ),
                                        (
                                            RouteMode::Direct,
                                            "直连",
                                            Some("全部连接直接走本地网络（用于延迟对比）"),
                                        ),
                                    ],
                                    Vec2::new(w, 30.0),
                                    FONT_SM,
                                    p,
                                );
                            });
                        });
                    });
            });
        });
        if let Some(m) = picked_proxy {
            self.switch_proxy_mode(m);
        }
        if let Some(r) = picked_route {
            self.switch_route_mode(r);
        }

        ui.add_space(4.0);

        // 3. Unified Realtime Monitor Card (Integrated header + sparkline canvas)
        card_frame(p)
            .inner_margin(Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                aligned_row(ui, 26.0, |ui| {
                    if let Some(cm) = segmented_control(
                        ui,
                        "dash_chart_seg",
                        self.chart_mode,
                        &[
                            (ChartMode::Traffic, "实时流量", None),
                            (ChartMode::Rtt, "延迟曲线", None),
                        ],
                        Vec2::new(148.0, 26.0),
                        FONT_XS,
                        p,
                    ) {
                        self.chart_mode = cm;
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        match self.chart_mode {
                            ChartMode::Traffic => {
                                let (rx_txt, tx_txt) = if self.state == ConnState::Connected {
                                    (
                                        format!("下载 {}", format_speed(self.rx_speed_bps)),
                                        format!("上传 {}", format_speed(self.tx_speed_bps)),
                                    )
                                } else {
                                    ("下载".to_string(), "上传".to_string())
                                };
                                ui.label(
                                    RichText::new(tx_txt)
                                        .size(FONT_XS)
                                        .monospace()
                                        .color(p.text_muted),
                                );
                                let (r_tx, _) =
                                    ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                                ui.painter()
                                    .circle_filled(r_tx.center(), 3.2, p.success_text);
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new(rx_txt)
                                        .size(FONT_XS)
                                        .monospace()
                                        .color(p.text_muted),
                                );
                                let (r_rx, _) =
                                    ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                                ui.painter().circle_filled(r_rx.center(), 3.2, p.accent);
                            }
                            ChartMode::Rtt => {
                                let summary = if self.state == ConnState::Connected
                                    && let Some(&last) = self.rtt_history.back()
                                {
                                    let avg: u64 = self.rtt_history.iter().sum::<u64>()
                                        / self.rtt_history.len().max(1) as u64;
                                    format!("当前 {last} ms · 均值 {avg} ms")
                                } else {
                                    "QUIC 延迟".to_string()
                                };
                                ui.label(
                                    RichText::new(summary)
                                        .size(FONT_XS)
                                        .monospace()
                                        .color(p.text_muted),
                                );
                            }
                        }
                    });
                });

                ui.add_space(4.0);
                match self.chart_mode {
                    ChartMode::Traffic => draw_traffic_sparkline(
                        ui,
                        &self.speed_history,
                        self.state == ConnState::Connected,
                        p,
                    ),
                    ChartMode::Rtt => draw_rtt_sparkline(
                        ui,
                        &self.rtt_history,
                        self.state == ConnState::Connected,
                        p,
                    ),
                }
            });
    }
}

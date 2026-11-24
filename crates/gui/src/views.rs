use crate::icons;
use crate::logger;
use crate::storage::{ProxyMode, RouteMode, config_dir, format_bytes, format_speed, save_config};
use crate::theme::{
    FONT_BODY, FONT_SM, FONT_SUB, FONT_TITLE, FONT_XS, ThemeMode, ThemePalette, apply_theme,
};
use crate::widgets::{
    accent_chip_btn, card_frame, chip_btn, draw_hero_power_button, draw_rtt_sparkline,
    draw_status_pill, draw_traffic_sparkline, hairline_divider, pill, primary_btn, soft_btn,
    styled_input, styled_multiline, styled_search_input,
};
use crate::{
    APP_VERSION, ChartMode, ConnState, GameAccelApp, LogFilter, NavTab, NodeEditorState,
    ProbeState, relaunch_as_admin,
};
use chrono::Local;
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Frame, Layout, Margin, PointerButton, Pos2,
    RichText, ScrollArea, Sense, Stroke, Vec2,
};
use protocol::share::{NodeProfile, parse_import_text};
use std::sync::atomic::Ordering;

impl GameAccelApp {
    pub(crate) fn render_sidebar_contents(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        p: ThemePalette,
    ) {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);

        // Optically aligned brand header (also acts as frameless window drag handle)
        let (brand_rect, brand_resp) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), 36.0),
            Sense::click_and_drag(),
        );
        if brand_resp.drag_started_by(PointerButton::Primary) {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }

        let painter = ui.painter();
        let logo_rect = egui::Rect::from_min_size(
            Pos2::new(brand_rect.left(), brand_rect.center().y - 16.0),
            Vec2::new(32.0, 32.0),
        );
        icons::draw_brand_logo(painter, logo_rect, p);

        let text_x = logo_rect.right() + 10.0;
        let title_y = logo_rect.top() + 9.0;
        let sub_y = logo_rect.bottom() - 6.0;

        painter.text(
            Pos2::new(text_x, title_y),
            egui::Align2::LEFT_CENTER,
            "GameAccel",
            FontId::proportional(FONT_SUB),
            p.text_primary,
        );

        let ver_galley = ui.fonts(|f| {
            f.layout_no_wrap(
                APP_VERSION.to_string(),
                FontId::proportional(FONT_XS),
                p.text_muted,
            )
        });
        let ver_w = ver_galley.size().x;
        painter.galley(
            Pos2::new(text_x, sub_y - ver_galley.size().y * 0.5),
            ver_galley,
            p.text_muted,
        );

        let beta_galley = ui.fonts(|f| {
            f.layout_no_wrap(
                "Beta".to_string(),
                FontId::proportional(FONT_XS),
                p.accent_text,
            )
        });
        let pill_rect = egui::Rect::from_min_size(
            Pos2::new(
                text_x + ver_w + 5.0,
                sub_y - (beta_galley.size().y + 2.0) * 0.5,
            ),
            Vec2::new(beta_galley.size().x + 8.0, beta_galley.size().y + 2.0),
        );
        painter.rect_filled(pill_rect, CornerRadius::same(4), p.accent_soft);
        painter.galley(
            Pos2::new(
                pill_rect.left() + 4.0,
                pill_rect.center().y - beta_galley.size().y * 0.5,
            ),
            beta_galley,
            p.accent_text,
        );

        ui.add_space(24.0);

        let mut items = vec![
            (NavTab::Dashboard, 0u8, "加速首页"),
            (NavTab::Nodes, 1u8, "节点与订阅"),
            (NavTab::Logs, 2u8, "连接与日志"),
        ];
        if self.cfg.show_admin_menu {
            items.push((NavTab::Admin, 4u8, "服务端管理"));
        }
        items.push((NavTab::Settings, 3u8, "系统设置"));

        for (tab, icon_kind, label) in items {
            let active = self.active_tab == tab;
            let (rect, resp) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 36.0), Sense::click());
            let bg = if active {
                p.bg_nav_active
            } else if resp.hovered() {
                p.bg_nav_hover
            } else {
                Color32::TRANSPARENT
            };
            let painter = ui.painter();
            painter.rect_filled(rect, CornerRadius::same(8), bg);
            if active {
                painter.rect_stroke(
                    rect,
                    CornerRadius::same(8),
                    Stroke::new(1.0_f32, p.border_subtle),
                    egui::StrokeKind::Inside,
                );
            }
            let icon_color = if active { p.accent } else { p.text_muted };
            let icon_center = Pos2::new(rect.left() + 18.0, rect.center().y);
            icons::draw_nav_icon(painter, icon_center, icon_kind, icon_color);

            painter.text(
                Pos2::new(rect.left() + 36.0, rect.center().y),
                egui::Align2::LEFT_CENTER,
                label,
                FontId::proportional(FONT_BODY),
                if active {
                    p.text_primary
                } else {
                    p.text_secondary
                },
            );
            if resp.clicked() {
                self.active_tab = tab;
            }
            ui.add_space(4.0);
        }

        // Minimalist footer pinned at bottom
        let remaining = ui.available_height();
        if remaining > 48.0 {
            ui.add_space(remaining - 46.0);
        }
        hairline_divider(ui, p);
        ui.add_space(6.0);
        ui.label(
            RichText::new(&self.cfg.listen_addr)
                .size(FONT_SM)
                .monospace()
                .color(p.text_secondary),
        );
        let (perm_txt, perm_col) = if self.is_admin {
            ("● 管理员 · TUN 就绪", p.success_text)
        } else {
            ("○ 普通权限", p.text_muted)
        };
        ui.label(RichText::new(perm_txt).size(FONT_XS).color(perm_col));
    }

    pub(crate) fn render_right_workspace(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        p: ThemePalette,
    ) {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);

        // Integrated frameless title bar with exact dynamic non-overlapping drag handle
        ui.horizontal(|ui| {
            let page_title = match self.active_tab {
                NavTab::Dashboard => "加速首页",
                NavTab::Nodes => "节点与订阅",
                NavTab::Logs => "连接与日志",
                NavTab::Admin => "服务端管理",
                NavTab::Settings => "系统设置",
            };
            let title_resp = ui.add(
                egui::Label::new(
                    RichText::new(page_title)
                        .size(FONT_TITLE)
                        .strong()
                        .color(p.text_primary),
                )
                .sense(Sense::click_and_drag()),
            );
            let is_maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
            if title_resp.drag_started_by(PointerButton::Primary) {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            if title_resp.double_clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!is_maximized));
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;

                // Close button (minimize to system tray)
                let (c_rect, c_resp) =
                    ui.allocate_exact_size(Vec2::new(26.0, 26.0), Sense::click());
                if c_resp.hovered() {
                    ui.painter()
                        .rect_filled(c_rect, CornerRadius::same(7), p.danger_soft);
                }
                icons::draw_win_close_icon(
                    ui.painter(),
                    c_rect.center(),
                    if c_resp.hovered() {
                        p.danger_text
                    } else {
                        p.text_secondary
                    },
                );
                if c_resp.on_hover_text("最小化到系统托盘").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }

                // Maximize / Restore button
                let (x_rect, x_resp) =
                    ui.allocate_exact_size(Vec2::new(26.0, 26.0), Sense::click());
                if x_resp.hovered() {
                    ui.painter()
                        .rect_filled(x_rect, CornerRadius::same(7), p.bg_nav_hover);
                }
                icons::draw_win_max_icon(
                    ui.painter(),
                    x_rect.center(),
                    is_maximized,
                    p.text_secondary,
                );
                if x_resp
                    .on_hover_text(if is_maximized { "还原窗口" } else { "最大化" })
                    .clicked()
                {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!is_maximized));
                }

                // Minimize button
                let (m_rect, m_resp) =
                    ui.allocate_exact_size(Vec2::new(26.0, 26.0), Sense::click());
                if m_resp.hovered() {
                    ui.painter()
                        .rect_filled(m_rect, CornerRadius::same(7), p.bg_nav_hover);
                }
                icons::draw_win_min_icon(ui.painter(), m_rect.center(), p.text_secondary);
                if m_resp.on_hover_text("最小化").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }

                // Theme toggle (Sun / Moon)
                let (t_rect, t_resp) =
                    ui.allocate_exact_size(Vec2::new(26.0, 26.0), Sense::click());
                ui.painter().rect(
                    t_rect,
                    CornerRadius::same(7),
                    if t_resp.hovered() {
                        p.bg_nav_hover
                    } else {
                        p.bg_card
                    },
                    Stroke::new(1.0_f32, p.border_subtle),
                    egui::StrokeKind::Inside,
                );
                icons::draw_theme_icon(ui.painter(), t_rect.center(), p.is_dark, p.text_secondary);
                if t_resp
                    .on_hover_text(if p.is_dark {
                        "切换浅色主题"
                    } else {
                        "切换深色主题"
                    })
                    .clicked()
                {
                    self.cfg.theme_mode = match self.cfg.theme_mode {
                        ThemeMode::Light => ThemeMode::Dark,
                        ThemeMode::Dark => ThemeMode::Light,
                    };
                    apply_theme(ctx, ThemePalette::from_mode(self.cfg.theme_mode));
                    save_config(&self.cfg);
                }

                // Compact status pill
                let reconnect = self.reconnect_status();
                let pill = match (self.state, reconnect) {
                    (ConnState::Connected, _) => "已连接".to_string(),
                    (ConnState::Connecting, Some((attempt, 0))) => {
                        format!("重连中 · 第 {attempt} 次")
                    }
                    (ConnState::Connecting, Some((attempt, secs))) => {
                        format!("重连中 · {secs} 秒后重试（第 {attempt} 次）")
                    }
                    (ConnState::Connecting, None) => "连接中...".to_string(),
                    (ConnState::Disconnected, _) => "未连接".to_string(),
                };
                let (dot_col, bg, fg, stroke_col) = match self.state {
                    ConnState::Connected => (
                        p.success_text,
                        p.success_soft,
                        p.success_text,
                        p.success_text,
                    ),
                    ConnState::Connecting => (p.accent, p.accent_soft, p.accent_text, p.accent),
                    ConnState::Disconnected => (
                        p.text_muted,
                        p.bg_card,
                        p.text_secondary,
                        p.border_subtle,
                    ),
                };
                draw_status_pill(ui, &pill, dot_col, bg, fg, stroke_col);

                // Exact remaining empty space between status pill and title -> drag handle
                let gap_w = ui.available_width();
                if gap_w > 6.0 {
                    let (_, gap_resp) =
                        ui.allocate_exact_size(Vec2::new(gap_w, 28.0), Sense::click_and_drag());
                    if gap_resp.drag_started_by(PointerButton::Primary) {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    if gap_resp.double_clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!is_maximized));
                    }
                }
            });
        });

        ui.add_space(16.0);

        // Responsive max-width centering on very wide/maximized monitors so layouts stay balanced
        let max_content_w = match self.active_tab {
            NavTab::Logs | NavTab::Admin => 1180.0,
            _ => 960.0,
        };
        let avail_w = ui.available_width();
        let avail_h = ui.available_height();
        let content_w = avail_w.min(max_content_w);
        let side_pad = ((avail_w - content_w) * 0.5).max(0.0);

        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if side_pad > 0.0 {
                ui.add_space(side_pad);
            }
            ui.allocate_ui_with_layout(
                Vec2::new(content_w, avail_h),
                Layout::top_down(Align::LEFT),
                |ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
                    ui.set_width(content_w);
                    match self.active_tab {
                        NavTab::Dashboard => self.render_dashboard(ui, p),
                        NavTab::Nodes => self.render_nodes(ui, p),
                        NavTab::Logs => self.render_logs(ui, p),
                        NavTab::Admin => self.render_admin(ui, ctx, p),
                        NavTab::Settings => self.render_settings(ui, ctx, p),
                    }
                },
            );
        });
    }

    fn render_dashboard(&mut self, ui: &mut egui::Ui, p: ThemePalette) {
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
                    {
                        if let Some(pos) = ui.ctx().input(|i| i.pointer.interact_pos()) {
                            if !area_resp.rect.contains(pos) && !pill_rect.contains(pos) {
                                self.node_picker_open = false;
                            }
                        }
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

    fn render_nodes(&mut self, ui: &mut egui::Ui, p: ThemePalette) {
        // Unified top action toolbar
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
            if primary_btn(ui, "+ 新建节点", p).clicked() {
                self.node_editor = Some(NodeEditorState::new_blank());
                self.import_panel_open = false;
                self.sub_panel_open = false;
            }
            if soft_btn(ui, "导入 gacl:// / JSON", p).clicked() {
                self.import_panel_open = !self.import_panel_open;
                self.sub_panel_open = false;
                self.node_editor = None;
            }
            if soft_btn(ui, "订阅 URL 同步", p).clicked() {
                self.sub_panel_open = !self.sub_panel_open;
                self.import_panel_open = false;
                self.node_editor = None;
            }
            ui.add_enabled_ui(!self.cfg.nodes.is_empty(), |ui| {
                if soft_btn(ui, "全部测速", p).clicked() {
                    self.probe_all_nodes();
                }
            });
        });

        ui.add_space(10.0);

        // Panel A: Import gacl:// or JSON text
        if self.import_panel_open {
            card_frame(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new("粘贴 gacl:// 分享链接（支持多行）或订阅 JSON")
                        .size(FONT_BODY)
                        .strong()
                        .color(p.text_primary),
                );
                ui.add_space(6.0);
                styled_multiline(
                    ui,
                    &mut self.import_text,
                    "gacl://your-token@node.example.com:8443?sni=node.example.com&dup=1#主力节点",
                    3,
                    p,
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if primary_btn(ui, "确认导入", p).clicked() {
                        match parse_import_text(&self.import_text) {
                            Ok(nodes) => {
                                let total = nodes.len();
                                let added = self.merge_imported_nodes(nodes, None);
                                self.import_text.clear();
                                self.import_panel_open = false;
                                self.show_toast(format!(
                                    "导入成功：解析 {total} 个节点（新增 {added}）"
                                ));
                            }
                            Err(e) => {
                                self.show_toast(format!("导入失败: {e:#}"));
                            }
                        }
                    }
                    if soft_btn(ui, "取消", p).clicked() {
                        self.import_panel_open = false;
                    }
                });
            });
            ui.add_space(8.0);
        }

        // Panel B: Subscription URL fetch
        if self.sub_panel_open {
            card_frame(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new("订阅链接（支持后端管理面板下发的 JSON 或 gacl:// 列表）")
                        .size(FONT_BODY)
                        .strong()
                        .color(p.text_primary),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 106.0).max(200.0);
                    styled_input(
                        ui,
                        &mut self.sub_url_input,
                        "https://panel.example.com:9090/sub/your-token",
                        w,
                        false,
                        p,
                    );
                    ui.add_enabled_ui(!self.sub_loading, |ui| {
                        let lbl = if self.sub_loading {
                            "拉取中..."
                        } else {
                            "拉取订阅"
                        };
                        if primary_btn(ui, lbl, p).clicked() {
                            let url = self.sub_url_input.clone();
                            self.fetch_subscription(url);
                        }
                    });
                });
                if !self.cfg.subscriptions.is_empty() {
                    ui.add_space(6.0);
                    let subs = self.cfg.subscriptions.clone();
                    for sub in subs {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!(
                                    "{} · {} (更新: {})",
                                    sub.name,
                                    sub.url,
                                    sub.last_updated.as_deref().unwrap_or("-")
                                ))
                                .size(FONT_SM)
                                .color(p.text_secondary),
                            );
                            if chip_btn(ui, "刷新", p, false).clicked() {
                                self.fetch_subscription(sub.url.clone());
                            }
                        });
                    }
                }
            });
            ui.add_space(8.0);
        }

        // Panel C: Node Add/Edit Form
        let mut editor_save: Option<NodeProfile> = None;
        let mut editor_cancel = false;
        if let Some(ed) = &mut self.node_editor {
            card_frame(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let title = if ed.editing_id.is_some() {
                    "编辑节点"
                } else {
                    "新建节点"
                };
                ui.label(
                    RichText::new(title)
                        .size(FONT_SUB)
                        .strong()
                        .color(p.text_primary),
                );
                ui.add_space(8.0);

                egui::Grid::new("node_editor_grid")
                    .num_columns(2)
                    .spacing([14.0, 8.0])
                    .show(ui, |ui| {
                        ui.label(RichText::new("节点名称").size(FONT_BODY).color(p.text_secondary));
                        styled_input(ui, &mut ed.name, "例如：美西游戏专线", 300.0, false, p);
                        ui.end_row();

                        ui.label(
                            RichText::new("服务器 (IP / 域名:端口)")
                                .size(FONT_BODY)
                                .color(p.text_secondary),
                        );
                        styled_input(
                            ui,
                            &mut ed.server,
                            "203.0.113.10:8443 或 hk1.example.com:8443",
                            300.0,
                            false,
                            p,
                        );
                        ui.end_row();

                        ui.label(
                            RichText::new("TLS 域名 (SNI)")
                                .size(FONT_BODY)
                                .color(p.text_secondary),
                        );
                        styled_input(ui, &mut ed.sni, "node.example.com", 300.0, false, p);
                        ui.end_row();

                        ui.label(
                            RichText::new("鉴权 Token")
                                .size(FONT_BODY)
                                .color(p.text_secondary),
                        );
                        ui.horizontal(|ui| {
                            styled_input(
                                ui,
                                &mut ed.token,
                                "粘贴服务器生成的 Token",
                                244.0,
                                !ed.show_token,
                                p,
                            );
                            let lbl = if ed.show_token { "隐藏" } else { "显示" };
                            if chip_btn(ui, lbl, p, false).clicked() {
                                ed.show_token = !ed.show_token;
                            }
                        });
                        ui.end_row();
                    });

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    icons::pill_toggle(ui, &mut ed.game_dup, p);
                    ui.label(
                        RichText::new("丢包自适应双发 (< 200B)")
                            .size(FONT_BODY)
                            .color(p.text_primary),
                    );
                    ui.add_space(16.0);
                    icons::pill_toggle(ui, &mut ed.insecure, p);
                    ui.label(
                        RichText::new("允许自签证书")
                            .size(FONT_BODY)
                            .color(p.text_secondary),
                    );
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if primary_btn(ui, "保存节点", p).clicked() {
                        let id = ed.editing_id.clone().unwrap_or_else(|| {
                            format!("node-{}", Local::now().timestamp_millis())
                        });
                        let name = if ed.name.trim().is_empty() {
                            format!("节点 {}", ed.server.trim())
                        } else {
                            ed.name.trim().to_string()
                        };
                        editor_save = Some(NodeProfile {
                            id,
                            name,
                            server: ed.server.trim().to_string(),
                            sni: ed.sni.trim().to_string(),
                            token: ed.token.trim().to_string(),
                            game_dup: ed.game_dup,
                            insecure: ed.insecure,
                            source: None,
                        });
                    }
                    if soft_btn(ui, "取消", p).clicked() {
                        editor_cancel = true;
                    }
                });
            });
            ui.add_space(8.0);
        }

        if let Some(saved) = editor_save {
            let saved_id = saved.id.clone();
            if let Some(existing) = self.cfg.nodes.iter_mut().find(|n| n.id == saved.id) {
                *existing = saved;
            } else {
                self.cfg.nodes.push(saved);
            }
            if self.cfg.active_node().is_none() {
                self.cfg.active_node_id = saved_id;
            }
            save_config(&self.cfg);
            self.node_editor = None;
            self.show_toast("节点已保存");
        } else if editor_cancel {
            self.node_editor = None;
        }

        // Empty-state onboarding card when no nodes are configured yet
        if self.cfg.nodes.is_empty() {
            card_frame(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add_space(18.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new("还没有配置加速节点")
                            .size(FONT_SUB)
                            .strong()
                            .color(p.text_primary),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "可从服务端管理面板复制 gacl:// 链接导入、拉取订阅 URL，或点击上方「+ 新建节点」手动填写",
                        )
                        .size(FONT_SM)
                        .color(p.text_muted),
                    );
                });
                ui.add_space(18.0);
            });
            return;
        }

        // Node List Cards
        let nodes = self.cfg.nodes.clone();
        let mut select_id: Option<String> = None;
        let mut delete_id: Option<String> = None;
        let mut probe_node: Option<NodeProfile> = None;
        let mut edit_node: Option<NodeProfile> = None;
        let mut share_node: Option<NodeProfile> = None;

        ScrollArea::vertical().show(ui, |ui| {
            for node in &nodes {
                let is_active = self.cfg.active_node_id == node.id;
                let border = if is_active { p.accent } else { p.border_subtle };

                Frame::new()
                    .fill(p.bg_card)
                    .stroke(Stroke::new(if is_active { 1.5_f32 } else { 1.0_f32 }, border))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::symmetric(18, 15))
                    .shadow(p.card_shadow)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);

                        // Row 1: Active dot + Name + RTT pill (left) | Action chips (right)
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 7.0;
                            let (dr, _) =
                                ui.allocate_exact_size(Vec2::new(8.0, 18.0), Sense::hover());
                            ui.painter().circle_filled(
                                dr.center(),
                                3.6,
                                if is_active {
                                    p.accent
                                } else {
                                    p.border_strong
                                },
                            );
                            ui.label(
                                RichText::new(&node.name)
                                    .size(FONT_SUB)
                                    .strong()
                                    .color(p.text_primary),
                            );

                            if let Some(ps) = self.probes.get(&node.id) {
                                match ps {
                                    ProbeState::Probing => {
                                        pill(ui, "测速中...", p.tag_bg, p.text_muted);
                                    }
                                    ProbeState::Ok(ms) => {
                                        let (bg, fg) = if *ms < 100 {
                                            (p.success_soft, p.success_text)
                                        } else if *ms < 200 {
                                            (p.warn_soft, p.warn_text)
                                        } else {
                                            (p.danger_soft, p.danger_text)
                                        };
                                        pill(ui, &format!("{ms} ms"), bg, fg);
                                    }
                                    ProbeState::Err(err_msg) => {
                                        Frame::new()
                                            .fill(p.danger_soft)
                                            .corner_radius(CornerRadius::same(10))
                                            .inner_margin(Margin::symmetric(8, 3))
                                            .show(ui, |ui| {
                                                ui.label(
                                                    RichText::new("超时")
                                                        .size(FONT_XS)
                                                        .strong()
                                                        .color(p.danger_text),
                                                )
                                                .on_hover_text(err_msg);
                                            });
                                    }
                                }
                            }

                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                let can_del = !(is_active && self.state != ConnState::Disconnected);
                                ui.add_enabled_ui(can_del, |ui| {
                                    if chip_btn(ui, "删除", p, true).clicked() {
                                        delete_id = Some(node.id.clone());
                                    }
                                });
                                if chip_btn(ui, "编辑", p, false).clicked() {
                                    edit_node = Some(node.clone());
                                }
                                if chip_btn(ui, "分享", p, false).clicked() {
                                    share_node = Some(node.clone());
                                }
                                if chip_btn(ui, "测速", p, false).clicked() {
                                    probe_node = Some(node.clone());
                                }
                                if !is_active {
                                    ui.add_enabled_ui(
                                        self.state == ConnState::Disconnected,
                                        |ui| {
                                            if accent_chip_btn(ui, "选用", p, false).clicked() {
                                                select_id = Some(node.id.clone());
                                            }
                                        },
                                    );
                                }
                            });
                        });

                        // Row 2: Tabular monospace endpoint + quiet inline meta
                        ui.horizontal(|ui| {
                            ui.add_space(15.0);
                            let mut meta = format!("{}  ·  SNI {}", node.server, node.sni);
                            if !node.insecure {
                                meta.push_str("  ·  TLS 1.3");
                            }
                            if node.game_dup {
                                meta.push_str("  ·  自适应双发");
                            }
                            ui.label(
                                RichText::new(meta)
                                    .size(FONT_SM)
                                    .monospace()
                                    .color(p.text_muted),
                            );
                        });
                    });
                ui.add_space(8.0);
            }
        });

        if let Some(id) = select_id {
            self.cfg.active_node_id = id;
            save_config(&self.cfg);
            self.show_toast("已切换当前节点");
        }
        if let Some(id) = delete_id {
            self.cfg.nodes.retain(|n| n.id != id);
            if self.cfg.active_node_id == id {
                self.cfg.active_node_id = self
                    .cfg
                    .nodes
                    .first()
                    .map(|n| n.id.clone())
                    .unwrap_or_default();
            }
            save_config(&self.cfg);
            self.show_toast("节点已删除");
        }
        if let Some(n) = probe_node {
            self.probe_single_node(n);
        }
        if let Some(n) = edit_node {
            self.node_editor = Some(NodeEditorState::from_node(&n));
            self.import_panel_open = false;
            self.sub_panel_open = false;
        }
        if let Some(n) = share_node {
            ui.ctx().copy_text(n.to_share_uri());
            self.show_toast(format!("已复制「{}」的 gacl:// 分享链接到剪贴板", n.name));
        }
    }

    fn render_logs(&mut self, ui: &mut egui::Ui, p: ThemePalette) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for (f, lbl) in [
                (LogFilter::All, "全部"),
                (LogFilter::Success, "成功/连接"),
                (LogFilter::Error, "警告/错误"),
            ] {
                let active = self.log_filter == f;
                let clicked = if active {
                    accent_chip_btn(ui, lbl, p, true).clicked()
                } else {
                    chip_btn(ui, lbl, p, false).clicked()
                };
                if clicked {
                    self.log_filter = f;
                }
            }

            let search_w = (ui.available_width() - 360.0).clamp(140.0, 320.0);
            styled_search_input(ui, &mut self.log_search, "搜索日志关键字...", search_w, p);

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if chip_btn(ui, "清空", p, true).clicked() {
                    self.logs.clear();
                }
                if chip_btn(ui, "复制全部", p, false).clicked() {
                    ui.ctx().copy_text(self.logs.join("\n"));
                    self.show_toast("已复制全部运行日志");
                }
            });
        });

        ui.add_space(8.0);

        card_frame(p).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let avail_h = ui.available_height().max(260.0);
            ScrollArea::vertical()
                .max_height(avail_h)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let q = self.log_search.trim().to_lowercase();
                    for line in &self.logs {
                        let is_err =
                            line.contains("异常") || line.contains("失败") || line.contains("无效");
                        let is_ok = line.contains("成功")
                            || line.contains("已开启")
                            || line.contains("已启动");
                        let pass_level = match self.log_filter {
                            LogFilter::All => true,
                            LogFilter::Success => is_ok,
                            LogFilter::Error => is_err,
                        };
                        if !pass_level {
                            continue;
                        }
                        if !q.is_empty() && !line.to_lowercase().contains(&q) {
                            continue;
                        }
                        let color = if is_err {
                            p.danger_text
                        } else if is_ok {
                            p.success_text
                        } else {
                            p.text_secondary
                        };
                        // Use Proportional (Microsoft YaHei) for mixed CJK/ASCII log lines so
                        // timestamps, ASCII acronyms (e.g. TUN), and CJK share one baseline.
                        let font = FontId::proportional(FONT_SM);
                        let mut job = egui::text::LayoutJob::default();
                        if line.starts_with('[')
                            && let Some(end) = line.find("] ")
                        {
                            let (ts, rest) = line.split_at(end + 2);
                            job.append(ts, 0.0, egui::TextFormat::simple(font.clone(), p.text_muted));
                            job.append(rest, 0.0, egui::TextFormat::simple(font, color));
                        } else {
                            job.append(line, 0.0, egui::TextFormat::simple(font, color));
                        }
                        ui.label(job);
                    }
                });
        });
    }

    fn render_settings(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, p: ThemePalette) {
        ScrollArea::vertical()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                self.render_settings_inner(ui, ctx, p);
            });
    }

    fn render_settings_inner(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, p: ThemePalette) {
        // Group 1: Network
        ui.label(RichText::new("网络配置").size(FONT_SM).color(p.text_muted));
        ui.add_space(4.0);
        card_frame(p).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        RichText::new("本地聚合代理端口")
                            .size(FONT_BODY)
                            .strong()
                            .color(p.text_primary),
                    );
                    ui.label(
                        RichText::new("单端口同时支持 HTTP / HTTPS / SOCKS5 (TCP + UDP)")
                            .size(FONT_SM)
                            .color(p.text_muted),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_enabled_ui(self.state == ConnState::Disconnected, |ui| {
                        if styled_input(
                            ui,
                            &mut self.cfg.listen_addr,
                            "127.0.0.1:1080",
                            150.0,
                            false,
                            p,
                        )
                        .changed()
                        {
                            save_config(&self.cfg);
                        }
                    });
                });
            });
        });

        ui.add_space(18.0);

        // Group 2: Appearance & Diagnostics
        ui.label(RichText::new("外观与诊断").size(FONT_SM).color(p.text_muted));
        ui.add_space(6.0);
        card_frame(p).show(ui, |ui| {
            ui.set_width(ui.available_width());

            // Row 1: Theme segmented switcher (unified Painter-drawn pill)
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        RichText::new("界面主题")
                            .size(FONT_BODY)
                            .strong()
                            .color(p.text_primary),
                    );
                    ui.label(
                        RichText::new("暖米白浅色 / 暖炭黑深色，实时无刷新切换")
                            .size(FONT_SM)
                            .color(p.text_muted),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (pill_rect, _) =
                        ui.allocate_exact_size(Vec2::new(130.0, 30.0), Sense::hover());
                    ui.painter().rect(
                        pill_rect,
                        CornerRadius::same(9),
                        p.bg_card_alt,
                        Stroke::new(1.0_f32, p.border_subtle),
                        egui::StrokeKind::Inside,
                    );
                    let seg_w = 60.0;
                    let seg_h = 24.0;
                    for (idx, (mode, lbl)) in
                        [(ThemeMode::Light, "浅色"), (ThemeMode::Dark, "深色")]
                            .into_iter()
                            .enumerate()
                    {
                        let x = pill_rect.left() + 3.0 + idx as f32 * (seg_w + 4.0);
                        let seg_rect = egui::Rect::from_min_size(
                            Pos2::new(x, pill_rect.top() + 3.0),
                            Vec2::new(seg_w, seg_h),
                        );
                        let resp = ui.interact(
                            seg_rect,
                            ui.id().with(("theme_seg", idx)),
                            Sense::click(),
                        );
                        let active = self.cfg.theme_mode == mode;
                        let fill = if active {
                            p.bg_card
                        } else if resp.hovered() {
                            p.bg_nav_hover
                        } else {
                            Color32::TRANSPARENT
                        };
                        let stroke = if active {
                            Stroke::new(1.0_f32, p.border_subtle)
                        } else {
                            Stroke::NONE
                        };
                        ui.painter().rect(
                            seg_rect,
                            CornerRadius::same(6),
                            fill,
                            stroke,
                            egui::StrokeKind::Inside,
                        );
                        ui.painter().text(
                            seg_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            lbl,
                            FontId::proportional(FONT_SM),
                            if active {
                                p.text_primary
                            } else {
                                p.text_muted
                            },
                        );
                        if resp.clicked() {
                            self.cfg.theme_mode = mode;
                            apply_theme(ctx, ThemePalette::from_mode(mode));
                            save_config(&self.cfg);
                        }
                    }
                });
            });

            ui.add_space(12.0);
            hairline_divider(ui, p);
            ui.add_space(12.0);

            // Row 1.5: Show/Hide Server Admin Menu
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        RichText::new("服务端管理菜单")
                            .size(FONT_BODY)
                            .strong()
                            .color(p.text_primary),
                    );
                    ui.label(
                        RichText::new("开启后在左侧导航显示「服务端管理」，可配置并管理远程服务端")
                            .size(FONT_SM)
                            .color(p.text_muted),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (pill_rect, _) =
                        ui.allocate_exact_size(Vec2::new(130.0, 30.0), Sense::hover());
                    ui.painter().rect(
                        pill_rect,
                        CornerRadius::same(9),
                        p.bg_card_alt,
                        Stroke::new(1.0_f32, p.border_subtle),
                        egui::StrokeKind::Inside,
                    );
                    let seg_w = 60.0;
                    let seg_h = 24.0;
                    for (idx, (val, lbl)) in [(false, "隐藏"), (true, "显示")].into_iter().enumerate()
                    {
                        let x = pill_rect.left() + 3.0 + idx as f32 * (seg_w + 4.0);
                        let seg_rect = egui::Rect::from_min_size(
                            Pos2::new(x, pill_rect.top() + 3.0),
                            Vec2::new(seg_w, seg_h),
                        );
                        let resp = ui.interact(
                            seg_rect,
                            ui.id().with(("admin_menu_seg", idx)),
                            Sense::click(),
                        );
                        let active = self.cfg.show_admin_menu == val;
                        let fill = if active {
                            p.bg_card
                        } else if resp.hovered() {
                            p.bg_nav_hover
                        } else {
                            Color32::TRANSPARENT
                        };
                        let stroke = if active {
                            Stroke::new(1.0_f32, p.border_subtle)
                        } else {
                            Stroke::NONE
                        };
                        ui.painter().rect(
                            seg_rect,
                            CornerRadius::same(6),
                            fill,
                            stroke,
                            egui::StrokeKind::Inside,
                        );
                        ui.painter().text(
                            seg_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            lbl,
                            FontId::proportional(FONT_SM),
                            if active {
                                p.text_primary
                            } else {
                                p.text_muted
                            },
                        );
                        if resp.clicked() && self.cfg.show_admin_menu != val {
                            self.cfg.show_admin_menu = val;
                            save_config(&self.cfg);
                            if !val && self.active_tab == NavTab::Admin {
                                self.active_tab = NavTab::Settings;
                            }
                            self.show_toast(if val {
                                "已显示「服务端管理」菜单"
                            } else {
                                "已隐藏「服务端管理」菜单"
                            });
                        }
                    }
                });
            });

            ui.add_space(12.0);
            hairline_divider(ui, p);
            ui.add_space(12.0);

            // Row 2: Data & Logs
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        RichText::new("数据与诊断日志")
                            .size(FONT_BODY)
                            .strong()
                            .color(p.text_primary),
                    );
                    ui.label(
                        RichText::new("配置与持久化日志保存在 %APPDATA%\\game-accel")
                            .size(FONT_SM)
                            .color(p.text_muted),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if soft_btn(ui, "打开日志", p).clicked() {
                        let log_path = logger::log_file_path();
                        #[cfg(target_os = "windows")]
                        {
                            let _ = std::process::Command::new("notepad").arg(log_path).spawn();
                        }
                    }
                    if soft_btn(ui, "配置目录", p).clicked() {
                        let dir = config_dir();
                        let _ = std::fs::create_dir_all(&dir);
                        #[cfg(target_os = "windows")]
                        {
                            let _ = std::process::Command::new("explorer").arg(dir).spawn();
                        }
                    }
                });
            });

            ui.add_space(12.0);
            hairline_divider(ui, p);
            ui.add_space(12.0);

            // Row 3: Export & Elevation
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        RichText::new("批量导出与权限")
                            .size(FONT_BODY)
                            .strong()
                            .color(p.text_primary),
                    );
                    ui.label(
                        RichText::new(if self.is_admin {
                            "当前已以管理员身份运行（Wintun 虚拟网卡可用）"
                        } else {
                            "普通权限运行中，使用 TUN 模式前需管理员权限"
                        })
                        .size(FONT_SM)
                        .color(p.text_muted),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if !self.is_admin && primary_btn(ui, "管理员重启", p).clicked() {
                        if self.state != ConnState::Disconnected {
                            self.stop_connect();
                        }
                        save_config(&self.cfg);
                        relaunch_as_admin();
                    }
                    ui.add_enabled_ui(!self.cfg.nodes.is_empty(), |ui| {
                        if soft_btn(ui, "导出全部 gacl://", p).clicked() {
                            let all_uris = self
                                .cfg
                                .nodes
                                .iter()
                                .map(|n| n.to_share_uri())
                                .collect::<Vec<_>>()
                                .join("\n");
                            ctx.copy_text(all_uris);
                            self.show_toast(format!(
                                "已复制 {} 个节点的 gacl:// 分享链接",
                                self.cfg.nodes.len()
                            ));
                        }
                    });
                });
            });
        });
    }
}

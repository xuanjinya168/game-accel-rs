use crate::icons;
use crate::logger::log_line;
use crate::storage::{ProxyMode, RouteMode, format_speed, save_config};
use crate::theme::{FONT_BODY, FONT_SM, FONT_XS, ThemePalette};
use crate::tray::{apply_win11_rounded_corners, wake_window};
use crate::widgets::{accent_chip_btn, chip_btn, hairline_divider, pill};
use crate::{ConnState, GameAccelApp, NavTab, ProbeState};
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Frame, Layout, Margin, Pos2, RichText, ScrollArea,
    Sense, Stroke, StrokeKind, Vec2,
};
use std::sync::atomic::Ordering;
use std::time::Duration;

pub(crate) const TRAY_POPOVER_W: f32 = 328.0;
pub(crate) const TRAY_POPOVER_H: f32 = 428.0;

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
                .with_inner_size([TRAY_POPOVER_W, TRAY_POPOVER_H])
                .with_position([self.tray_popover_pos.x, self.tray_popover_pos.y]),
            |vctx, _class| {
                if self.tray_popover_just_opened {
                    apply_win11_rounded_corners();
                    vctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    self.tray_popover_just_opened = false;
                }

                // Auto-dismiss only after the popover has gained focus at least once.
                let focused = vctx.input(|i| i.viewport().focused);
                if focused == Some(true) {
                    self.tray_popover_ever_focused = true;
                } else if self.tray_popover_ever_focused && focused == Some(false) {
                    close_popover = true;
                }
                if vctx.input(|i| {
                    i.key_pressed(egui::Key::Escape) || i.viewport().close_requested()
                }) {
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

                        // 1. Header bar
                        ui.allocate_ui_with_layout(
                            Vec2::new(ui.available_width(), 26.0),
                            Layout::left_to_right(Align::Center),
                            |ui| {
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
                                ui.painter().circle_filled(dot_r.center(), 3.5, dot_col);

                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    let (x_rect, x_resp) =
                                        ui.allocate_exact_size(Vec2::splat(24.0), Sense::click());
                                    if x_resp.hovered() {
                                        ui.painter().rect_filled(
                                            x_rect,
                                            CornerRadius::same(6),
                                            p.bg_nav_hover,
                                        );
                                    }
                                    icons::draw_win_close_icon(
                                        ui.painter(),
                                        x_rect.center(),
                                        p.text_secondary,
                                    );
                                    if x_resp.on_hover_text("收起面板").clicked() {
                                        close_popover = true;
                                    }

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
                                            "点击隐藏桌面网速悬浮窗"
                                        } else {
                                            "点击显示桌面网速悬浮窗"
                                        })
                                        .clicked()
                                    {
                                        actions.push(PopoverAction::ToggleFloatingPill);
                                    }
                                });
                            },
                        );

                        ui.add_space(4.0);

                        // 2. Core connection status card
                        Frame::new()
                            .fill(p.bg_card_alt)
                            .stroke(Stroke::new(1.0_f32, p.border_subtle))
                            .corner_radius(CornerRadius::same(10))
                            .inner_margin(Margin::symmetric(12, 10))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());

                                let active_node = self.cfg.active_node().cloned();
                                let reconnect = self.reconnect_status();

                                ui.horizontal(|ui| {
                                    let (status_txt, status_col) = match (self.state, reconnect) {
                                        (ConnState::Connected, _) => ("已连接", p.success_text),
                                        (ConnState::Connecting, Some((attempt, _))) => {
                                            let _ = attempt;
                                            ("重连中...", p.accent_text)
                                        }
                                        (ConnState::Connecting, None) => ("连接中...", p.accent_text),
                                        (ConnState::Disconnected, _) => ("未连接", p.text_muted),
                                    };
                                    ui.label(
                                        RichText::new(status_txt)
                                            .size(FONT_SM)
                                            .strong()
                                            .color(status_col),
                                    );
                                    ui.label(RichText::new("·").size(FONT_SM).color(p.text_muted));

                                    let node_label = active_node
                                        .as_ref()
                                        .map(|n| truncate_text(&n.name, 16))
                                        .unwrap_or_else(|| "尚未添加节点".to_string());
                                    ui.label(
                                        RichText::new(node_label)
                                            .size(FONT_SM)
                                            .strong()
                                            .color(p.text_primary),
                                    );

                                    if let Some(ref node) = active_node {
                                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                            pill(ui, node.protocol.label(), p.tag_bg, p.tag_text);
                                        });
                                    }
                                });

                                // Metrics row
                                ui.horizontal(|ui| {
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
                                                .strong()
                                                .color(rtt_col),
                                        );
                                        ui.add_space(6.0);
                                        ui.label(
                                            RichText::new(format!(
                                                "↑ {}",
                                                format_speed(self.tx_speed_bps)
                                            ))
                                            .size(FONT_XS)
                                            .monospace()
                                            .color(p.text_secondary),
                                        );
                                        ui.add_space(4.0);
                                        ui.label(
                                            RichText::new(format!(
                                                "↓ {}",
                                                format_speed(self.rx_speed_bps)
                                            ))
                                            .size(FONT_XS)
                                            .monospace()
                                            .color(p.text_secondary),
                                        );
                                    } else if let Some(ref node) = active_node {
                                        ui.label(
                                            RichText::new(truncate_text(&node.server, 34))
                                                .size(FONT_XS)
                                                .monospace()
                                                .color(p.text_muted),
                                        );
                                    } else {
                                        ui.label(
                                            RichText::new("点击下方按钮前往添加或导入节点")
                                                .size(FONT_XS)
                                                .color(p.text_muted),
                                        );
                                    }
                                });

                                ui.add_space(4.0);

                                // Full-width primary connect/disconnect button
                                let has_nodes = !self.cfg.nodes.is_empty();
                                let btn_w = ui.available_width();
                                let (btn_rect, btn_resp) =
                                    ui.allocate_exact_size(Vec2::new(btn_w, 34.0), Sense::click());
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
                                            "一键开启加速"
                                        } else {
                                            "前往添加节点"
                                        };
                                        (f, Stroke::NONE, Color32::WHITE, label)
                                    }
                                    ConnState::Connecting => {
                                        let f = if btn_resp.hovered() {
                                            p.accent_soft.linear_multiply(0.95)
                                        } else {
                                            p.accent_soft
                                        };
                                        (
                                            f,
                                            Stroke::new(1.0_f32, p.accent),
                                            p.accent_text,
                                            "取消连接",
                                        )
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
                                ui.painter().text(
                                    btn_rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    lbl,
                                    FontId::proportional(FONT_SM),
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

                        ui.add_space(4.0);

                        // 3. Proxy mode & route mode segmented rows
                        if let Some(m) = render_compact_segmented_row(
                            ui,
                            "代理模式",
                            &[
                                (ProxyMode::Tun, "TUN"),
                                (ProxyMode::SystemProxy, "系统代理"),
                                (ProxyMode::Manual, "本地端口"),
                            ],
                            self.cfg.proxy_mode,
                            "pop_proxy_mode",
                            p,
                        ) {
                            actions.push(PopoverAction::SwitchProxyMode(m));
                        }

                        if let Some(r) = render_compact_segmented_row(
                            ui,
                            "分流规则",
                            &[
                                (RouteMode::Rule, "规则"),
                                (RouteMode::Global, "全局"),
                                (RouteMode::Direct, "直连"),
                            ],
                            self.cfg.route_mode,
                            "pop_route_mode",
                            p,
                        ) {
                            actions.push(PopoverAction::SwitchRouteMode(r));
                        }

                        ui.add_space(2.0);
                        hairline_divider(ui, p);
                        ui.add_space(2.0);

                        // 4. Quick node list header
                        let node_count = self.cfg.nodes.len();
                        ui.horizontal(|ui| {
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
                                        if any_probing { "测速中..." } else { "全部测速" },
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

                        // Optional search bar when there are > 5 nodes
                        if node_count > 5 {
                            Frame::new()
                                .fill(p.bg_input)
                                .stroke(Stroke::new(1.0_f32, p.border_subtle))
                                .corner_radius(CornerRadius::same(7))
                                .inner_margin(Margin::symmetric(8, 4))
                                .show(ui, |ui| {
                                    ui.add_sized(
                                        Vec2::new(ui.available_width(), 18.0),
                                        egui::TextEdit::singleline(&mut self.tray_popover_search)
                                            .frame(false)
                                            .font(FontId::proportional(FONT_SM))
                                            .hint_text("筛选节点名称或地址..."),
                                    );
                                });
                        }

                        // Scrollable node list leaving 38px for footer
                        let list_h = (ui.available_height() - 38.0).max(52.0);
                        let filter_q = self.tray_popover_search.trim().to_lowercase();
                        let active_id = self.cfg.active_node_id.clone();

                        ScrollArea::vertical()
                            .id_salt("tray_popover_nodes_scroll")
                            .max_height(list_h)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.spacing_mut().item_spacing.y = 4.0;

                                if self.cfg.nodes.is_empty() {
                                    ui.add_space(16.0);
                                    ui.vertical_centered(|ui| {
                                        ui.label(
                                            RichText::new("暂无节点，打开主界面导入或添加")
                                                .size(FONT_SM)
                                                .color(p.text_muted),
                                        );
                                    });
                                } else {
                                    for node in &self.cfg.nodes {
                                        if !filter_q.is_empty()
                                            && !node.name.to_lowercase().contains(&filter_q)
                                            && !node.server.to_lowercase().contains(&filter_q)
                                        {
                                            continue;
                                        }
                                        let is_active = node.id == active_id;
                                        let row_w = ui.available_width();
                                        let (rect, resp) = ui.allocate_exact_size(
                                            Vec2::new(row_w, 30.0),
                                            Sense::click(),
                                        );
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

                                        // Left selection indicator dot
                                        let dot_col = if is_active { p.accent } else { p.border_strong };
                                        ui.painter().circle_filled(
                                            Pos2::new(rect.left() + 12.0, rect.center().y),
                                            3.0,
                                            dot_col,
                                        );

                                        // Node name
                                        let name_col = if is_active {
                                            p.accent_text
                                        } else {
                                            p.text_primary
                                        };
                                        ui.painter().text(
                                            Pos2::new(rect.left() + 22.0, rect.center().y),
                                            egui::Align2::LEFT_CENTER,
                                            truncate_text(&node.name, 20),
                                            FontId::proportional(FONT_SM),
                                            name_col,
                                        );

                                        // Right probe latency badge
                                        let (rtt_txt, rtt_col) = match self.probes.get(&node.id) {
                                            Some(ProbeState::Probing) => {
                                                ("测速...".to_string(), p.text_muted)
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
                                        ui.painter().text(
                                            Pos2::new(rect.right() - 10.0, rect.center().y),
                                            egui::Align2::RIGHT_CENTER,
                                            rtt_txt,
                                            FontId::proportional(FONT_XS),
                                            rtt_col,
                                        );

                                        if resp.clicked() && !is_active {
                                            actions.push(PopoverAction::SelectNode(node.id.clone()));
                                        }
                                    }
                                }
                            });

                        // 5. Footer bar pinned at bottom
                        ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                            ui.horizontal(|ui| {
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
                            hairline_divider(ui, p);
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
                    log_line("INFO", "从托盘面板执行彻底退出：清理连接、TUN 路由与系统代理...");
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

fn render_compact_segmented_row<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    title: &str,
    options: &[(T, &str)],
    current: T,
    id_salt: &str,
    p: ThemePalette,
) -> Option<T> {
    let mut picked = None;
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(FONT_SM).color(p.text_secondary));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let pill_w = 216.0_f32;
            let pill_h = 26.0_f32;
            let (pill_rect, _) = ui.allocate_exact_size(Vec2::new(pill_w, pill_h), Sense::hover());
            ui.painter().rect(
                pill_rect,
                CornerRadius::same(7),
                p.bg_card_alt,
                Stroke::new(1.0_f32, p.border_subtle),
                StrokeKind::Inside,
            );
            let n = options.len().max(1) as f32;
            let pad = 2.5_f32;
            let gap = 2.0_f32;
            let seg_w = (pill_w - pad * 2.0 - gap * (n - 1.0)) / n;
            let seg_h = pill_h - pad * 2.0;

            for (idx, &(val, lbl)) in options.iter().enumerate() {
                let x = pill_rect.left() + pad + idx as f32 * (seg_w + gap);
                let seg_rect =
                    egui::Rect::from_min_size(Pos2::new(x, pill_rect.top() + pad), Vec2::new(seg_w, seg_h));
                let resp = ui.interact(seg_rect, ui.id().with((id_salt, idx)), Sense::click());
                let active = current == val;
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
                    CornerRadius::same(5),
                    fill,
                    stroke,
                    StrokeKind::Inside,
                );
                ui.painter().text(
                    seg_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    lbl,
                    FontId::proportional(FONT_XS),
                    if active { p.text_primary } else { p.text_muted },
                );
                if resp.clicked() && !active {
                    picked = Some(val);
                }
            }
        });
    });
    picked
}

fn truncate_text(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

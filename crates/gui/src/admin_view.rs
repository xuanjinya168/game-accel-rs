use crate::admin::{
    AdminLinkEditor, AdminServerEditor, AdminSubTab, LinkView, format_uptime,
};
use crate::icons;
use crate::storage::{AdminServerProfile, format_bytes, save_config};
use crate::theme::{FONT_BODY, FONT_SM, FONT_SUB, FONT_TITLE, FONT_XS, ThemePalette};
use crate::widgets::{
    accent_chip_btn, aligned_row, card_frame, chip_btn, dropdown_chip_btn, elide_text,
    galley_center_y, hairline_divider, modal_dialog, pill, primary_btn, segmented_control,
    soft_btn, styled_input, styled_search_input,
};
use crate::{ConnState, GameAccelApp};
use chrono::Local;
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Frame, Layout, Margin, Pos2, RichText, ScrollArea,
    Sense, Stroke, Vec2,
};
use protocol::domain::LinkStatus;

impl GameAccelApp {
    pub(crate) fn render_admin(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, p: ThemePalette) {
        ScrollArea::vertical()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                self.render_admin_inner(ui, ctx, p);
            });
    }

    fn render_admin_inner(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, p: ThemePalette) {
        self.cfg.ensure_default_admin_server();
        let active_srv = self
            .cfg
            .active_admin_server()
            .cloned()
            .unwrap_or_else(AdminServerProfile::default_tunnel);
        let logged_in = self.admin.is_logged_in_for(&active_srv.id);

        // 1. Compact remote server selector & endpoint bar
        card_frame(p)
            .inner_margin(Margin::symmetric(16, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                aligned_row(ui, 28.0, |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let servers = self.cfg.admin_servers.clone();
                    for srv in &servers {
                        let is_active = srv.id == active_srv.id;
                        let clicked = if is_active {
                            accent_chip_btn(ui, &srv.name, p, true).clicked()
                        } else {
                            chip_btn(ui, &srv.name, p, false).clicked()
                        };
                        if clicked {
                            self.admin_select_server(srv.id.clone());
                        }
                    }

                    if active_srv.via_tunnel {
                        pill(ui, "加密隧道", p.success_soft, p.success_text);
                    } else if active_srv.base_url.trim().starts_with("https://") {
                        pill(ui, "HTTPS", p.success_soft, p.success_text);
                    } else {
                        pill(ui, "明文 HTTP", p.danger_soft, p.danger_text);
                    }

                    let max_url_w = (ui.available_width() - 220.0).max(60.0);
                    let url_shown = elide_text(
                        ui,
                        &active_srv.base_url,
                        FontId::monospace(FONT_XS),
                        max_url_w,
                    );
                    ui.label(
                        RichText::new(url_shown)
                            .size(FONT_XS)
                            .monospace()
                            .color(p.text_muted),
                    );

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        if logged_in && chip_btn(ui, "退出", p, true).clicked() {
                            self.admin_logout();
                        }
                        if logged_in {
                            ui.add_enabled_ui(!self.admin.busy, |ui| {
                                if chip_btn(ui, "刷新", p, false).clicked() {
                                    self.admin_refresh();
                                }
                            });
                        }
                        if chip_btn(ui, "+ 添加", p, false).clicked() {
                            self.admin.server_editor = Some(AdminServerEditor::new_blank());
                            self.admin.link_editor = None;
                        }
                        if chip_btn(ui, "编辑", p, false).clicked() {
                            self.admin.server_editor =
                                Some(AdminServerEditor::from_profile(&active_srv));
                            self.admin.link_editor = None;
                        }
                    });
                });
            });

        self.render_admin_server_modal(ctx, p);

        ui.add_space(8.0);

        // 2. Login View vs Authenticated Dashboard
        if !logged_in {
            self.render_admin_login_card(ui, &active_srv, p);
            return;
        }

        // 3. Authenticated: 4 Equal-Height KPI Cards (with public_host + insecure exposed)
        if let Some(ov) = &self.admin.overview {
            ui.columns(4, |cols| {
                let host_sub = if ov.sni.is_empty() || ov.public_host.eq_ignore_ascii_case(&ov.sni)
                {
                    ov.public_host.clone()
                } else {
                    format!("{} · {}", ov.public_host, ov.sni)
                };
                let host_tip = format!(
                    "公网地址: {}\nTLS SNI: {}\n证书校验: {}",
                    ov.public_host,
                    ov.sni,
                    if ov.insecure {
                        "允许跳过（自签证书）"
                    } else {
                        "严格校验"
                    }
                );

                let kpis = [
                    (
                        "监听节点",
                        format!("UDP :{}", ov.udp_port),
                        host_sub,
                        host_tip,
                        ov.insecure,
                    ),
                    (
                        "在线连接",
                        ov.total_online_conns.to_string(),
                        format!("已运行 {}", format_uptime(ov.uptime_secs)),
                        format!("已运行 {}", format_uptime(ov.uptime_secs)),
                        false,
                    ),
                    (
                        "累计流量",
                        format_bytes(ov.total_tx_bytes + ov.total_rx_bytes),
                        format!(
                            "↑ {} · ↓ {}",
                            format_bytes(ov.total_tx_bytes),
                            format_bytes(ov.total_rx_bytes)
                        ),
                        format!(
                            "上传 {} · 下载 {}",
                            format_bytes(ov.total_tx_bytes),
                            format_bytes(ov.total_rx_bytes)
                        ),
                        false,
                    ),
                    (
                        "分享链接",
                        format!("{} / {}", ov.active_links, ov.total_links),
                        "启用 / 总数".to_string(),
                        "启用 / 总数".to_string(),
                        false,
                    ),
                ];

                for (i, (label, val, sub, tooltip, is_insecure)) in kpis.into_iter().enumerate() {
                    card_frame(p)
                        .inner_margin(Margin::symmetric(14, 11))
                        .show(&mut cols[i], |ui| {
                            ui.set_width(ui.available_width());
                            ui.set_min_height(68.0);
                            aligned_row(ui, 20.0, |ui| {
                                ui.label(RichText::new(label).size(FONT_XS).color(p.text_muted));
                                if is_insecure {
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        pill(ui, "自签", p.warn_soft, p.warn_text);
                                    });
                                }
                            });
                            ui.add_space(2.0);
                            ui.add(
                                egui::Label::new(
                                    RichText::new(val)
                                        .size(FONT_TITLE)
                                        .monospace()
                                        .color(p.text_primary),
                                )
                                .truncate(),
                            );
                            ui.add_space(2.0);
                            let sub_shown = elide_text(
                                ui,
                                &sub,
                                FontId::proportional(FONT_XS),
                                ui.available_width(),
                            );
                            ui.label(RichText::new(sub_shown).size(FONT_XS).color(p.text_muted))
                                .on_hover_text(tooltip);
                        });
                }
            });
            ui.add_space(8.0);
        }

        // 4. Sub-Tab Bar (`分享链接 | 访问日志 | 节点与安全`) + Contextual Right Actions
        let mut switch_to_logs = false;
        let mut toggle_logs = false;
        let mut clear_logs = false;

        aligned_row(ui, 30.0, |ui| {
            if let Some(tab) = segmented_control(
                ui,
                "admin_sub_tab_seg",
                self.admin.sub_tab,
                &[
                    (AdminSubTab::Links, "分享链接", None),
                    (AdminSubTab::Logs, "访问日志", None),
                    (AdminSubTab::Settings, "节点与安全", None),
                ],
                Vec2::new(268.0, 30.0),
                FONT_SM,
                p,
            ) {
                self.admin.sub_tab = tab;
                self.admin.open_more_link_id = None;
                self.admin.confirm_action = None;
                self.admin.confirm_clear_logs = false;
                self.admin.log_link_picker_open = false;
                if tab == AdminSubTab::Logs {
                    switch_to_logs = true;
                }
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                match self.admin.sub_tab {
                    AdminSubTab::Links => {
                        if primary_btn(ui, "+ 新建链接", p).clicked() {
                            self.admin.link_editor = Some(AdminLinkEditor::new_blank());
                            self.admin.server_editor = None;
                            self.admin.open_more_link_id = None;
                        }
                    }
                    AdminSubTab::Logs => {
                        let clear_lbl = if self.admin.confirm_clear_logs {
                            "确认清空?"
                        } else {
                            "清空日志"
                        };
                        if chip_btn(ui, clear_lbl, p, true).clicked() {
                            if self.admin.confirm_clear_logs {
                                clear_logs = true;
                            } else {
                                self.admin.confirm_clear_logs = true;
                            }
                        }

                        let toggle_resp = if self.admin.logs_enabled {
                            accent_chip_btn(ui, "● 记录中", p, false)
                        } else {
                            chip_btn(ui, "○ 已暂停", p, false)
                        };
                        if toggle_resp
                            .on_hover_text("点击暂停或恢复服务端主域名访问日志记录")
                            .clicked()
                        {
                            toggle_logs = true;
                        }
                    }
                    AdminSubTab::Settings => {}
                }
            });
        });

        if switch_to_logs {
            self.admin_refresh_logs();
        }
        if toggle_logs {
            self.admin_toggle_logs();
        }
        if clear_logs {
            self.admin_clear_logs();
        }

        self.render_admin_link_modal(ctx, p);

        ui.add_space(6.0);

        match self.admin.sub_tab {
            AdminSubTab::Links => self.render_admin_links_tab(ui, ctx, &active_srv, p),
            AdminSubTab::Logs => self.render_admin_logs_tab(ui, ctx, p),
            AdminSubTab::Settings => self.render_admin_settings_tab(ui, p),
        }
    }

    fn render_admin_login_card(
        &mut self,
        ui: &mut egui::Ui,
        active_srv: &AdminServerProfile,
        p: ThemePalette,
    ) {
        card_frame(p).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(format!("登录「{}」", active_srv.name))
                    .size(FONT_SUB)
                    .color(p.text_primary),
            );
            if active_srv.via_tunnel && self.state != ConnState::Connected {
                ui.add_space(8.0);
                Frame::new()
                    .fill(p.warn_soft)
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            RichText::new("经隧道访问需先在「连接总览」开启加速")
                                .size(FONT_SM)
                                .color(p.warn_text),
                        );
                    });
            }

            ui.add_space(10.0);
            ui.columns(2, |cols| {
                cols[0].label(RichText::new("账号").size(FONT_XS).color(p.text_muted));
                let w0 = cols[0].available_width();
                styled_input(
                    &mut cols[0],
                    &mut self.admin.login_user,
                    "admin",
                    w0,
                    false,
                    p,
                );
                cols[1].label(RichText::new("密码").size(FONT_XS).color(p.text_muted));
                let w1 = cols[1].available_width();
                let pass_resp = styled_input(
                    &mut cols[1],
                    &mut self.admin.login_pass,
                    "输入密码",
                    w1,
                    true,
                    p,
                );
                if pass_resp.lost_focus()
                    && cols[1].input(|i| i.key_pressed(egui::Key::Enter))
                    && !self.admin.busy
                {
                    self.admin_login();
                }
            });

            if let Some(err) = &self.admin.login_error {
                ui.add_space(8.0);
                ui.label(RichText::new(err).size(FONT_SM).color(p.danger_text));
            }

            ui.add_space(12.0);
            ui.add_enabled_ui(!self.admin.busy, |ui| {
                let lbl = if self.admin.busy {
                    "登录中…"
                } else {
                    "登录管理后台"
                };
                if primary_btn(ui, lbl, p).clicked() {
                    self.admin_login();
                }
            });
        });
    }

    fn render_admin_links_tab(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        active_srv: &AdminServerProfile,
        p: ThemePalette,
    ) {
        card_frame(p)
            .inner_margin(Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());

                if self.admin.links.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(18.0);
                        ui.label(
                            RichText::new("暂无分享链接，点击右上角「+ 新建链接」创建")
                                .size(FONT_SM)
                                .color(p.text_muted),
                        );
                        ui.add_space(18.0);
                    });
                    return;
                }

                let links_snapshot = self.admin.links.clone();
                let sub_base = active_srv.base_url.trim_end_matches('/').to_string();
                let mut active_more_anchor: Option<(LinkView, egui::Rect)> = None;
                let mut more_trigger_clicked = false;

                for (idx, l) in links_snapshot.iter().enumerate() {
                    Frame::new()
                        .fill(p.bg_card_alt)
                        .stroke(Stroke::new(1.0_f32, p.border_subtle))
                        .corner_radius(CornerRadius::same(10))
                        .inner_margin(Margin::symmetric(14, 10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());

                            aligned_row(ui, 40.0, |ui| {
                                let right_reserve = if l.hy2_uri.is_empty() { 218.0 } else { 278.0 };
                                let left_w = (ui.available_width() - right_reserve).max(160.0);
                                ui.allocate_ui_with_layout(
                                    Vec2::new(left_w, 40.0),
                                    Layout::top_down(Align::LEFT),
                                    |ui| {
                                        ui.spacing_mut().item_spacing = Vec2::new(6.0, 3.0);
                                        aligned_row(ui, 20.0, |ui| {
                                            let badge_reserve = 120.0
                                                + if l.game_dup { 44.0 } else { 0.0 }
                                                + if l.online_conns > 0 { 54.0 } else { 0.0 };
                                            let max_name_w = (left_w - badge_reserve).max(60.0);
                                            let shown = elide_text(
                                                ui,
                                                &l.name,
                                                FontId::proportional(FONT_BODY),
                                                max_name_w,
                                            );
                                            ui.label(
                                                RichText::new(shown)
                                                    .size(FONT_BODY)
                                                    .color(p.text_primary),
                                            )
                                            .on_hover_text(&l.name);

                                            match l.status {
                                                LinkStatus::Active => {
                                                    pill(ui, "启用中", p.success_soft, p.success_text)
                                                }
                                                LinkStatus::Disabled => {
                                                    pill(ui, "已禁用", p.bg_card, p.text_muted)
                                                }
                                                LinkStatus::Expired => {
                                                    pill(ui, "已过期", p.warn_soft, p.warn_text)
                                                }
                                                LinkStatus::OverLimit => {
                                                    pill(ui, "流量超额", p.danger_soft, p.danger_text)
                                                }
                                            }
                                            if l.game_dup {
                                                pill(ui, "双发", p.bg_card, p.text_secondary);
                                            }
                                            if l.online_conns > 0 {
                                                pill(
                                                    ui,
                                                    &format!("在线 {}", l.online_conns),
                                                    p.success_soft,
                                                    p.success_text,
                                                );
                                            }
                                        });

                                        aligned_row(ui, 17.0, |ui| {
                                            let used = l.tx_bytes + l.rx_bytes;
                                            let limit_str = l
                                                .traffic_limit_gb
                                                .map(|g| format!("{g} GB"))
                                                .unwrap_or_else(|| "不限".to_string());
                                            let exp_str = l.expire_at.as_deref().unwrap_or("长期有效");
                                            ui.label(
                                                RichText::new(format!(
                                                    "{} / {} · {}",
                                                    format_bytes(used),
                                                    limit_str,
                                                    exp_str
                                                ))
                                                .size(FONT_XS)
                                                .monospace()
                                                .color(p.text_muted),
                                            )
                                            .on_hover_text(format!(
                                                "上传 {} · 下载 {}",
                                                format_bytes(l.tx_bytes),
                                                format_bytes(l.rx_bytes)
                                            ));

                                            if let Some(gb) = l.traffic_limit_gb
                                                && gb > 0.0
                                            {
                                                let pct = (used as f64
                                                    / (gb * 1024.0 * 1024.0 * 1024.0))
                                                    .clamp(0.0, 1.0)
                                                    as f32;
                                                let bar_w =
                                                    88.0_f32.min(ui.available_width().max(24.0));
                                                let (bar_rect, _) = ui.allocate_exact_size(
                                                    Vec2::new(bar_w, 5.0),
                                                    Sense::hover(),
                                                );
                                                ui.painter().rect_filled(
                                                    bar_rect,
                                                    CornerRadius::same(3),
                                                    p.border_subtle,
                                                );
                                                if pct > 0.0 {
                                                    let fill_rect = egui::Rect::from_min_size(
                                                        bar_rect.min,
                                                        Vec2::new(
                                                            bar_rect.width() * pct,
                                                            bar_rect.height(),
                                                        ),
                                                    );
                                                    ui.painter().rect_filled(
                                                        fill_rect,
                                                        CornerRadius::same(3),
                                                        if pct >= 0.95 {
                                                            p.danger_text
                                                        } else {
                                                            p.accent
                                                        },
                                                    );
                                                }
                                            }
                                        });
                                    },
                                );

                                // Right: Primary copy actions + crisp vector-chevron `更多` trigger
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    let is_open =
                                        self.admin.open_more_link_id.as_deref() == Some(&l.id);
                                    let more_resp = dropdown_chip_btn(ui, "更多", is_open, p);
                                    if more_resp.clicked() {
                                        more_trigger_clicked = true;
                                        if is_open {
                                            self.admin.open_more_link_id = None;
                                            self.admin.confirm_action = None;
                                        } else {
                                            self.admin.open_more_link_id = Some(l.id.clone());
                                            self.admin.confirm_action = None;
                                        }
                                    }
                                    if self.admin.open_more_link_id.as_deref() == Some(&l.id) {
                                        active_more_anchor = Some((l.clone(), more_resp.rect));
                                    }

                                    if chip_btn(ui, "订阅", p, false).clicked() {
                                        ctx.copy_text(format!("{sub_base}/sub/{}", l.token));
                                        self.show_toast("已复制订阅 URL");
                                    }
                                    if !l.hy2_uri.is_empty()
                                        && chip_btn(ui, "hy2://", p, false).clicked()
                                    {
                                        ctx.copy_text(l.hy2_uri.clone());
                                        self.show_toast("已复制 hysteria2:// 链接");
                                    }
                                    if accent_chip_btn(ui, "复制 gacl://", p, true).clicked() {
                                        ctx.copy_text(l.share_uri.clone());
                                        self.show_toast("已复制 gacl:// 分享链接");
                                    }
                                });
                            });
                        });
                    if idx + 1 < links_snapshot.len() {
                        ui.add_space(6.0);
                    }
                }

                // Floating `更多` action menu
                if let Some((l, btn_rect)) = active_more_anchor {
                    let menu_w = 132.0;
                    let pos = Pos2::new(btn_rect.right() - menu_w, btn_rect.bottom() + 4.0);
                    let mut close_more = false;

                    let area_resp = egui::Area::new(egui::Id::new("admin_link_more_popover"))
                        .order(egui::Order::Foreground)
                        .fixed_pos(pos)
                        .show(ctx, |ui| {
                            Frame::new()
                                .fill(p.bg_card)
                                .stroke(Stroke::new(1.0_f32, p.border_strong))
                                .corner_radius(CornerRadius::same(9))
                                .inner_margin(Margin::same(6))
                                .shadow(egui::Shadow {
                                    offset: [0, 6],
                                    blur: 18,
                                    spread: 0,
                                    color: Color32::from_black_alpha(if p.is_dark {
                                        90
                                    } else {
                                        28
                                    }),
                                })
                                .show(ui, |ui| {
                                    ui.set_width(menu_w - 12.0);
                                    ui.spacing_mut().item_spacing.y = 2.0;

                                    if popover_menu_item(ui, "编辑配额", false, p).clicked() {
                                        self.admin.link_editor =
                                            Some(AdminLinkEditor::from_link(&l));
                                        self.admin.server_editor = None;
                                        close_more = true;
                                    }

                                    let toggle_lbl =
                                        if l.enabled { "禁用链接" } else { "启用链接" };
                                    if popover_menu_item(ui, toggle_lbl, false, p).clicked() {
                                        self.admin_toggle_link(l.id.clone(), !l.enabled);
                                        close_more = true;
                                    }

                                    if popover_menu_item(ui, "清零流量", false, p).clicked() {
                                        self.admin_post_link_action(
                                            l.id.clone(),
                                            "reset-traffic",
                                            "已清零流量统计",
                                        );
                                        close_more = true;
                                    }

                                    hairline_divider(ui, p);

                                    let confirm_reset = self.admin.confirm_action.as_ref()
                                        == Some(&("reset-token".to_string(), l.id.clone()));
                                    let reset_lbl = if confirm_reset {
                                        "确认重置密钥"
                                    } else {
                                        "重置密钥"
                                    };
                                    if popover_menu_item(ui, reset_lbl, confirm_reset, p).clicked()
                                    {
                                        if confirm_reset {
                                            self.admin_post_link_action(
                                                l.id.clone(),
                                                "reset-token",
                                                "密钥已重置，请重新复制链接",
                                            );
                                            close_more = true;
                                        } else {
                                            self.admin.confirm_action =
                                                Some(("reset-token".to_string(), l.id.clone()));
                                        }
                                    }

                                    let confirm_del = self.admin.confirm_action.as_ref()
                                        == Some(&("delete".to_string(), l.id.clone()));
                                    let del_lbl =
                                        if confirm_del { "确认删除" } else { "删除链接" };
                                    if popover_menu_item(ui, del_lbl, true, p).clicked() {
                                        if confirm_del {
                                            self.admin_delete_link(l.id.clone());
                                            close_more = true;
                                        } else {
                                            self.admin.confirm_action =
                                                Some(("delete".to_string(), l.id.clone()));
                                        }
                                    }
                                });
                        })
                        .response;

                    if close_more || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                        self.admin.open_more_link_id = None;
                        self.admin.confirm_action = None;
                    } else if !more_trigger_clicked
                        && ctx.input(|i| i.pointer.any_pressed())
                        && let Some(click_pos) = ctx.input(|i| i.pointer.interact_pos())
                        && !area_resp.rect.contains(click_pos)
                        && !btn_rect.contains(click_pos)
                    {
                        self.admin.open_more_link_id = None;
                        self.admin.confirm_action = None;
                    }
                }
            });
    }

    fn render_admin_logs_tab(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, p: ThemePalette) {
        let mut need_refresh = false;
        let mut picker_anchor: Option<egui::Rect> = None;
        let mut picker_clicked = false;

        card_frame(p)
            .inner_margin(Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 8.0;

                // 1. Filter Toolbar: User Link Picker + Protocol Chips + Search + Count
                aligned_row(ui, 28.0, |ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;

                    let cur_link_lbl = if self.admin.log_link_filter.is_empty() {
                        "全部链接".to_string()
                    } else {
                        self.admin
                            .links
                            .iter()
                            .find(|l| l.id == self.admin.log_link_filter)
                            .map(|l| elide_text(ui, &l.name, FontId::proportional(FONT_SM), 96.0))
                            .unwrap_or_else(|| "指定链接".to_string())
                    };
                    let link_btn = dropdown_chip_btn(
                        ui,
                        &cur_link_lbl,
                        self.admin.log_link_picker_open,
                        p,
                    );
                    if link_btn.clicked() {
                        picker_clicked = true;
                        self.admin.log_link_picker_open = !self.admin.log_link_picker_open;
                    }
                    if self.admin.log_link_picker_open {
                        picker_anchor = Some(link_btn.rect);
                    }

                    for (proto_val, proto_lbl) in [
                        ("", "全部"),
                        ("HTTPS", "HTTPS"),
                        ("UDP", "UDP"),
                        ("TCP", "TCP"),
                        ("AUTH", "拦截"),
                        ("PANEL", "审计"),
                    ] {
                        let active = self.admin.log_proto_filter == proto_val;
                        let clicked = if active {
                            accent_chip_btn(ui, proto_lbl, p, true).clicked()
                        } else {
                            chip_btn(ui, proto_lbl, p, false).clicked()
                        };
                        if clicked && !active {
                            self.admin.log_proto_filter = proto_val.to_string();
                            need_refresh = true;
                        }
                    }

                    let prev_q = self.admin.log_search.clone();
                    let search_w = (ui.available_width() - 88.0).clamp(110.0, 220.0);
                    styled_search_input(
                        ui,
                        &mut self.admin.log_search,
                        "搜索域名 / IP…",
                        search_w,
                        p,
                    );
                    if self.admin.log_search != prev_q {
                        need_refresh = true;
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!(
                                "{} / {} 条",
                                self.admin.logs.len(),
                                self.admin.logs_total_buffered
                            ))
                            .size(FONT_XS)
                            .monospace()
                            .color(p.text_muted),
                        );
                    });
                });

                hairline_divider(ui, p);

                // 2. Anti-Squeeze Fixed-Slot Table Header + Rows
                let row_w = ui.available_width();
                let w_time = 88.0;
                let w_user = 88.0;
                let w_proto = 54.0;
                let w_ip = 98.0;
                let w_io = 114.0;
                let w_status = 44.0;
                let col_gap = 8.0;
                let w_domain = (row_w
                    - w_time
                    - w_user
                    - w_proto
                    - w_ip
                    - w_io
                    - w_status
                    - col_gap * 6.0)
                    .max(96.0);

                aligned_row(ui, 20.0, |ui| {
                    ui.spacing_mut().item_spacing.x = col_gap;
                    table_header_cell(ui, "时间", w_time, p);
                    table_header_cell(ui, "用户链接", w_user, p);
                    table_header_cell(ui, "协议", w_proto, p);
                    table_header_cell(ui, "主域名 / 目标地址", w_domain, p);
                    table_header_cell(ui, "来源 IP", w_ip, p);
                    table_header_cell(ui, "流量 · 耗时", w_io, p);
                    table_header_cell(ui, "状态", w_status, p);
                });

                hairline_divider(ui, p);

                if self.admin.logs.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(20.0);
                        ui.label(
                            RichText::new("暂无匹配的访问记录")
                                .size(FONT_SM)
                                .color(p.text_muted),
                        );
                        ui.add_space(20.0);
                    });
                } else {
                    ScrollArea::vertical()
                        .id_salt("admin_access_logs_scroll")
                        .max_height(260.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.spacing_mut().item_spacing.y = 4.0;

                            for r in &self.admin.logs {
                                let (row_rect, row_resp) = ui.allocate_exact_size(
                                    Vec2::new(row_w, 34.0),
                                    Sense::hover(),
                                );
                                if row_resp.hovered() {
                                    ui.painter().rect_filled(
                                        row_rect,
                                        CornerRadius::same(6),
                                        p.bg_card_alt,
                                    );
                                }
                                row_resp.on_hover_text(format!(
                                    "时间: {}\n链接: {}\n主域名: {}\n目标: {}\n客户端 IP: {}\n上传: {} · 下载: {} · 耗时: {} ms",
                                    r.time,
                                    r.link_name,
                                    r.root_domain,
                                    r.target,
                                    r.client_ip,
                                    format_bytes(r.tx_bytes),
                                    format_bytes(r.rx_bytes),
                                    r.duration_ms
                                ));

                                let painter = ui.painter();
                                let cy = row_rect.center().y;
                                let mut x = row_rect.left();

                                // Col 1: Time (MM-DD HH:MM:SS)
                                let short_time = r.time.get(5..).unwrap_or(&r.time);
                                painter.text(
                                    Pos2::new(x + 2.0, cy),
                                    egui::Align2::LEFT_CENTER,
                                    short_time,
                                    FontId::monospace(FONT_XS),
                                    p.text_muted,
                                );
                                x += w_time + col_gap;

                                // Col 2: Link Name
                                let user_txt = elide_text(
                                    ui,
                                    &r.link_name,
                                    FontId::proportional(FONT_SM),
                                    w_user - 2.0,
                                );
                                painter.text(
                                    Pos2::new(x, cy),
                                    egui::Align2::LEFT_CENTER,
                                    user_txt,
                                    FontId::proportional(FONT_SM),
                                    p.text_primary,
                                );
                                x += w_user + col_gap;

                                // Col 3: Protocol Badge
                                let (p_bg, p_fg) = match r.proto.as_str() {
                                    "HTTPS" => (p.success_soft, p.success_text),
                                    "UDP" => (p.accent_soft, p.accent_text),
                                    "AUTH" | "PANEL" => (p.warn_soft, p.warn_text),
                                    _ => (p.bg_card_alt, p.text_secondary),
                                };
                                draw_cell_badge(painter, ui, Pos2::new(x, cy), &r.proto, p_bg, p_fg);
                                x += w_proto + col_gap;

                                // Col 4: Root Domain + Target (2 crisp stacked lines)
                                let dom_txt = elide_text(
                                    ui,
                                    &r.root_domain,
                                    FontId::proportional(FONT_SM),
                                    w_domain - 4.0,
                                );
                                let tgt_txt = elide_text(
                                    ui,
                                    &r.target,
                                    FontId::monospace(10.5),
                                    w_domain - 4.0,
                                );
                                painter.text(
                                    Pos2::new(x, row_rect.top() + 10.0),
                                    egui::Align2::LEFT_CENTER,
                                    dom_txt,
                                    FontId::proportional(FONT_SM),
                                    p.text_primary,
                                );
                                painter.text(
                                    Pos2::new(x, row_rect.bottom() - 8.5),
                                    egui::Align2::LEFT_CENTER,
                                    tgt_txt,
                                    FontId::monospace(10.5),
                                    p.text_muted,
                                );
                                x += w_domain + col_gap;

                                // Col 5: Client IP
                                let ip_txt = elide_text(
                                    ui,
                                    &r.client_ip,
                                    FontId::monospace(FONT_XS),
                                    w_ip - 2.0,
                                );
                                painter.text(
                                    Pos2::new(x, cy),
                                    egui::Align2::LEFT_CENTER,
                                    ip_txt,
                                    FontId::monospace(FONT_XS),
                                    p.text_secondary,
                                );
                                x += w_ip + col_gap;

                                // Col 6: Traffic + Duration
                                let io_top = elide_text(
                                    ui,
                                    &format!(
                                        "↑{} ↓{}",
                                        format_bytes(r.tx_bytes),
                                        format_bytes(r.rx_bytes)
                                    ),
                                    FontId::monospace(10.5),
                                    w_io - 2.0,
                                );
                                let dur_str = if r.duration_ms >= 1000 {
                                    format!("{:.1}s", r.duration_ms as f64 / 1000.0)
                                } else {
                                    format!("{}ms", r.duration_ms)
                                };
                                painter.text(
                                    Pos2::new(x, row_rect.top() + 10.0),
                                    egui::Align2::LEFT_CENTER,
                                    io_top,
                                    FontId::monospace(10.5),
                                    p.text_secondary,
                                );
                                painter.text(
                                    Pos2::new(x, row_rect.bottom() - 8.5),
                                    egui::Align2::LEFT_CENTER,
                                    dur_str,
                                    FontId::monospace(10.5),
                                    p.text_muted,
                                );
                                x += w_io + col_gap;

                                // Col 7: Status Badge
                                let (st_lbl, st_bg, st_fg) = match r.status.as_str() {
                                    "ok" => ("正常", p.success_soft, p.success_text),
                                    "denied" => ("拒绝", p.danger_soft, p.danger_text),
                                    _ => ("失败", p.warn_soft, p.warn_text),
                                };
                                draw_cell_badge(painter, ui, Pos2::new(x, cy), st_lbl, st_bg, st_fg);
                            }
                        });
                }
            });

        // User Link Filter Popover
        if let Some(btn_rect) = picker_anchor {
            let menu_w = 180.0;
            let pos = Pos2::new(btn_rect.left(), btn_rect.bottom() + 4.0);
            let mut close_picker = false;
            let links_snap = self.admin.links.clone();

            let area_resp = egui::Area::new(egui::Id::new("admin_log_link_picker_popover"))
                .order(egui::Order::Foreground)
                .fixed_pos(pos)
                .show(ctx, |ui| {
                    Frame::new()
                        .fill(p.bg_card)
                        .stroke(Stroke::new(1.0_f32, p.border_strong))
                        .corner_radius(CornerRadius::same(9))
                        .inner_margin(Margin::same(6))
                        .shadow(egui::Shadow {
                            offset: [0, 6],
                            blur: 18,
                            spread: 0,
                            color: Color32::from_black_alpha(if p.is_dark { 90 } else { 28 }),
                        })
                        .show(ui, |ui| {
                            ui.set_width(menu_w - 12.0);
                            ui.spacing_mut().item_spacing.y = 2.0;

                            if popover_menu_item(ui, "全部用户链接", false, p).clicked() {
                                self.admin.log_link_filter.clear();
                                need_refresh = true;
                                close_picker = true;
                            }
                            if !links_snap.is_empty() {
                                hairline_divider(ui, p);
                            }
                            for l in &links_snap {
                                let shown = elide_text(
                                    ui,
                                    &l.name,
                                    FontId::proportional(FONT_SM),
                                    menu_w - 28.0,
                                );
                                if popover_menu_item(ui, &shown, false, p).clicked() {
                                    self.admin.log_link_filter = l.id.clone();
                                    need_refresh = true;
                                    close_picker = true;
                                }
                            }
                        });
                })
                .response;

            if close_picker || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.admin.log_link_picker_open = false;
            } else if !picker_clicked
                && ctx.input(|i| i.pointer.any_pressed())
                && let Some(click_pos) = ctx.input(|i| i.pointer.interact_pos())
                && !area_resp.rect.contains(click_pos)
                && !btn_rect.contains(click_pos)
            {
                self.admin.log_link_picker_open = false;
            }
        }

        if need_refresh {
            self.admin_refresh_logs();
        }
    }

    fn render_admin_settings_tab(&mut self, ui: &mut egui::Ui, p: ThemePalette) {
        let mut save_node = false;
        let mut revert_node = false;
        let mut update_pass = false;

        ui.columns(2, |cols| {
            // Left Card: Node Public Endpoint & TLS Settings
            card_frame(p)
                .inner_margin(Margin::symmetric(16, 14))
                .show(&mut cols[0], |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_min_height(228.0);
                    ui.label(
                        RichText::new("节点对外信息")
                            .size(FONT_SUB)
                            .color(p.text_primary),
                    );
                    ui.label(
                        RichText::new("控制生成分享链接与订阅 URL 时嵌入的公网地址及证书策略")
                            .size(FONT_XS)
                            .color(p.text_muted),
                    );

                    ui.add_space(10.0);
                    ui.label(
                        RichText::new("公网 IP 或域名")
                            .size(FONT_XS)
                            .color(p.text_muted),
                    );
                    if styled_input(
                        ui,
                        &mut self.admin.set_public_host,
                        "203.0.113.10 或 node.example.com",
                        ui.available_width(),
                        false,
                        p,
                    )
                    .changed()
                    {
                        self.admin.node_form_dirty = true;
                    }

                    ui.add_space(8.0);
                    ui.columns(2, |sub| {
                        sub[0].label(
                            RichText::new("QUIC UDP 端口")
                                .size(FONT_XS)
                                .color(p.text_muted),
                        );
                        let w0 = sub[0].available_width();
                        if styled_input(
                            &mut sub[0],
                            &mut self.admin.set_udp_port,
                            "8443",
                            w0,
                            false,
                            p,
                        )
                        .changed()
                        {
                            self.admin.node_form_dirty = true;
                        }

                        sub[1].label(
                            RichText::new("TLS 域名 (SNI)")
                                .size(FONT_XS)
                                .color(p.text_muted),
                        );
                        let w1 = sub[1].available_width();
                        if styled_input(
                            &mut sub[1],
                            &mut self.admin.set_sni,
                            "node.example.com",
                            w1,
                            false,
                            p,
                        )
                        .changed()
                        {
                            self.admin.node_form_dirty = true;
                        }
                    });

                    ui.add_space(10.0);
                    aligned_row(ui, 24.0, |ui| {
                        if icons::pill_toggle(ui, &mut self.admin.set_insecure, p).changed() {
                            self.admin.node_form_dirty = true;
                        }
                        ui.label(
                            RichText::new("允许跳过证书校验（自签证书）")
                                .size(FONT_SM)
                                .color(p.text_primary),
                        );
                    });

                    ui.add_space(12.0);
                    aligned_row(ui, 30.0, |ui| {
                        ui.add_enabled_ui(!self.admin.busy, |ui| {
                            if primary_btn(ui, "保存设置", p).clicked() {
                                save_node = true;
                            }
                            if self.admin.node_form_dirty
                                && self.admin.overview.is_some()
                                && soft_btn(ui, "还原", p).clicked()
                            {
                                revert_node = true;
                            }
                        });
                    });
                });

            // Right Card: Change Admin Password
            card_frame(p)
                .inner_margin(Margin::symmetric(16, 14))
                .show(&mut cols[1], |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_min_height(228.0);
                    ui.label(
                        RichText::new("修改管理密码")
                            .size(FONT_SUB)
                            .color(p.text_primary),
                    );
                    ui.label(
                        RichText::new("更新后所有已登录的管理会话将立即失效并要求重新登录")
                            .size(FONT_XS)
                            .color(p.text_muted),
                    );

                    ui.add_space(10.0);
                    ui.label(RichText::new("原密码").size(FONT_XS).color(p.text_muted));
                    styled_input(
                        ui,
                        &mut self.admin.old_password,
                        "输入当前后台密码",
                        ui.available_width(),
                        true,
                        p,
                    );

                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("新密码（至少 6 位）")
                            .size(FONT_XS)
                            .color(p.text_muted),
                    );
                    let new_resp = styled_input(
                        ui,
                        &mut self.admin.new_password,
                        "输入新管理密码",
                        ui.available_width(),
                        true,
                        p,
                    );
                    if new_resp.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        && !self.admin.busy
                    {
                        update_pass = true;
                    }

                    ui.add_space(44.0);
                    aligned_row(ui, 30.0, |ui| {
                        ui.add_enabled_ui(!self.admin.busy, |ui| {
                            if primary_btn(ui, "更新密码", p).clicked() {
                                update_pass = true;
                            }
                        });
                    });
                });
        });

        if revert_node
            && let Some(ov) = self.admin.overview.clone()
        {
            self.admin.sync_node_form_from(&ov);
        }
        if save_node {
            self.admin_save_node_settings();
        }
        if update_pass {
            self.admin_change_password();
        }
    }

    fn render_admin_server_modal(&mut self, ctx: &egui::Context, p: ThemePalette) {
        let Some(mut ed) = self.admin.server_editor.take() else {
            return;
        };
        let mut keep_open = true;
        let mut save_clicked = false;
        let mut delete_id: Option<String> = None;
        let can_delete = ed.editing_id.is_some() && self.cfg.admin_servers.len() > 1;
        let title = if ed.editing_id.is_some() {
            "编辑服务端"
        } else {
            "添加服务端"
        };
        let dismissed = modal_dialog(ctx, "admin_server_editor_modal", title, 460.0, p, |ui| {
            ui.columns(2, |cols| {
                cols[0].label(RichText::new("名称").size(FONT_XS).color(p.text_muted));
                let w0 = cols[0].available_width();
                styled_input(&mut cols[0], &mut ed.name, "服务端名称", w0, false, p);
                cols[1].label(RichText::new("账号").size(FONT_XS).color(p.text_muted));
                let w1 = cols[1].available_width();
                styled_input(&mut cols[1], &mut ed.username, "admin", w1, false, p);
            });

            ui.add_space(10.0);
            ui.label(RichText::new("面板地址").size(FONT_XS).color(p.text_muted));
            styled_input(
                ui,
                &mut ed.base_url,
                "http://panel.gacl.local",
                ui.available_width(),
                false,
                p,
            );

            ui.add_space(10.0);
            aligned_row(ui, 24.0, |ui| {
                if icons::pill_toggle(ui, &mut ed.via_tunnel, p).changed()
                    && ed.via_tunnel
                    && (ed.base_url.trim().is_empty() || ed.base_url.trim() == "https://")
                {
                    ed.base_url = "http://panel.gacl.local".to_string();
                }
                ui.label(
                    RichText::new("经加密隧道访问")
                        .size(FONT_BODY)
                        .color(p.text_primary),
                );
            });

            ui.add_space(14.0);
            aligned_row(ui, 30.0, |ui| {
                if can_delete
                    && let Some(eid) = &ed.editing_id
                    && soft_btn(ui, "删除服务端", p).clicked()
                {
                    delete_id = Some(eid.clone());
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if primary_btn(ui, "保存", p).clicked() {
                        save_clicked = true;
                    }
                    if soft_btn(ui, "取消", p).clicked() {
                        keep_open = false;
                    }
                });
            });
        });

        if let Some(eid) = delete_id {
            self.cfg.admin_servers.retain(|s| s.id != eid);
            self.cfg.ensure_default_admin_server();
            save_config(&self.cfg);
            self.admin.clear_session();
            if let Some(s) = self.cfg.active_admin_server() {
                self.admin.login_user = s.username.clone();
            }
            self.show_toast("已删除服务端配置");
            keep_open = false;
        } else if save_clicked {
            let name = ed.name.trim().to_string();
            let url = ed.base_url.trim().trim_end_matches('/').to_string();
            let username = if ed.username.trim().is_empty() {
                "admin".to_string()
            } else {
                ed.username.trim().to_string()
            };
            if name.is_empty() || (!url.starts_with("http://") && !url.starts_with("https://")) {
                self.show_toast("请填写名称和以 http:// 或 https:// 开头的地址");
            } else {
                if let Some(eid) = &ed.editing_id {
                    if let Some(existing) =
                        self.cfg.admin_servers.iter_mut().find(|s| s.id == *eid)
                    {
                        existing.name = name;
                        existing.base_url = url;
                        existing.via_tunnel = ed.via_tunnel;
                        existing.username = username.clone();
                    }
                } else {
                    let new_id = format!("srv-{}", Local::now().timestamp_millis());
                    self.cfg.admin_servers.push(AdminServerProfile {
                        id: new_id.clone(),
                        name,
                        base_url: url,
                        via_tunnel: ed.via_tunnel,
                        username: username.clone(),
                    });
                    self.cfg.active_admin_server_id = new_id;
                    self.admin.clear_session();
                }
                self.admin.login_user = username;
                save_config(&self.cfg);
                self.show_toast("服务端配置已保存");
                keep_open = false;
            }
        } else if dismissed {
            keep_open = false;
        }

        if keep_open {
            self.admin.server_editor = Some(ed);
        }
    }

    fn render_admin_link_modal(&mut self, ctx: &egui::Context, p: ThemePalette) {
        let Some(mut ed) = self.admin.link_editor.take() else {
            return;
        };
        let mut keep_open = true;
        let mut save_clicked = false;
        let title = if ed.editing_id.is_some() {
            "编辑分享链接"
        } else {
            "新建分享链接"
        };
        let dismissed = modal_dialog(ctx, "admin_link_editor_modal", title, 460.0, p, |ui| {
            ui.columns(3, |cols| {
                cols[0].label(RichText::new("名称").size(FONT_XS).color(p.text_muted));
                let w0 = cols[0].available_width();
                styled_input(&mut cols[0], &mut ed.name, "备注名称", w0, false, p);

                cols[1].label(
                    RichText::new("流量上限 (GB)")
                        .size(FONT_XS)
                        .color(p.text_muted),
                );
                let w1 = cols[1].available_width();
                styled_input(
                    &mut cols[1],
                    &mut ed.traffic_limit_gb,
                    "留空不限",
                    w1,
                    false,
                    p,
                );

                cols[2].label(RichText::new("到期日期").size(FONT_XS).color(p.text_muted));
                let w2 = cols[2].available_width();
                styled_input(
                    &mut cols[2],
                    &mut ed.expire_at,
                    "YYYY-MM-DD",
                    w2,
                    false,
                    p,
                );
            });
            ui.add_space(10.0);
            aligned_row(ui, 24.0, |ui| {
                icons::pill_toggle(ui, &mut ed.game_dup, p);
                ui.label(
                    RichText::new("游戏小包双发")
                        .size(FONT_BODY)
                        .color(p.text_primary),
                );
            });
            ui.add_space(14.0);
            aligned_row(ui, 30.0, |ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if primary_btn(ui, "保存", p).clicked() {
                        save_clicked = true;
                    }
                    if soft_btn(ui, "取消", p).clicked() {
                        keep_open = false;
                    }
                });
            });
        });

        if save_clicked {
            let name = ed.name.trim().to_string();
            if name.is_empty() {
                self.show_toast("请填写备注名称");
            } else {
                let limit_str = ed.traffic_limit_gb.trim();
                let parsed_limit = if limit_str.is_empty() {
                    Ok(None)
                } else {
                    match limit_str.parse::<f64>() {
                        Ok(v) if v > 0.0 => Ok(Some(v)),
                        Ok(_) => Ok(None),
                        Err(_) => Err(()),
                    }
                };
                match parsed_limit {
                    Ok(limit) => {
                        let exp = ed.expire_at.trim();
                        let expire_at = if exp.is_empty() {
                            None
                        } else {
                            Some(exp.to_string())
                        };
                        self.admin_save_link(
                            ed.editing_id.clone(),
                            name,
                            ed.game_dup,
                            limit,
                            expire_at,
                        );
                        keep_open = false;
                    }
                    Err(()) => {
                        self.show_toast("流量上限格式无效，请输入数字");
                    }
                }
            }
        } else if dismissed {
            keep_open = false;
        }

        if keep_open && self.admin.link_editor.is_none() {
            self.admin.link_editor = Some(ed);
        }
    }
}

fn table_header_cell(ui: &mut egui::Ui, text: &str, width: f32, p: ThemePalette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::hover());
    ui.painter().text(
        Pos2::new(rect.left(), rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        FontId::proportional(FONT_XS),
        p.text_muted,
    );
}

fn draw_cell_badge(
    painter: &egui::Painter,
    ui: &egui::Ui,
    left_center: Pos2,
    text: &str,
    bg: Color32,
    fg: Color32,
) {
    let galley = ui.fonts(|f| f.layout_no_wrap(text.to_string(), FontId::proportional(10.5), fg));
    let rect = egui::Rect::from_min_size(
        Pos2::new(left_center.x, left_center.y - 9.5),
        Vec2::new(galley.size().x + 12.0, 19.0),
    );
    painter.rect_filled(rect, CornerRadius::same(5), bg);
    let ty = galley_center_y(rect, &galley);
    painter.galley(
        Pos2::new(rect.center().x - galley.size().x * 0.5, ty),
        galley,
        fg,
    );
}

fn popover_menu_item(
    ui: &mut egui::Ui,
    label: &str,
    is_danger: bool,
    p: ThemePalette,
) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 27.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(6),
            if is_danger {
                p.danger_soft
            } else {
                p.bg_nav_hover
            },
        );
    }
    let fg = if is_danger {
        p.danger_text
    } else {
        p.text_primary
    };
    let galley =
        ui.fonts(|f| f.layout_no_wrap(label.to_string(), FontId::proportional(FONT_SM), fg));
    let ty = galley_center_y(rect, &galley);
    ui.painter().galley(Pos2::new(rect.left() + 10.0, ty), galley, fg);
    resp
}

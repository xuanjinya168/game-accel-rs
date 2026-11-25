use crate::admin::{AdminLinkEditor, AdminServerEditor, format_uptime};
use crate::storage::{AdminServerProfile, format_bytes, save_config};
use crate::theme::{FONT_BODY, FONT_SM, FONT_SUB, FONT_TITLE, FONT_XS, ThemePalette};
use crate::widgets::{
    accent_chip_btn, card_frame, chip_btn, modal_dialog, pill, primary_btn, soft_btn, styled_input,
};
use crate::{ConnState, GameAccelApp};
use chrono::Local;
use eframe::egui::{
    self, Align, CornerRadius, Frame, Layout, Margin, RichText, ScrollArea, Sense, Stroke, Vec2,
};

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

        // 1. Remote server selector & configuration bar
        card_frame(p).show(ui, |ui| {
            let row_w = ui.available_width();
            ui.set_width(row_w);
            ui.allocate_ui_with_layout(
                Vec2::new(row_w, 26.0),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(
                        RichText::new("服务端")
                            .size(FONT_SM)
                            .strong()
                            .color(p.text_secondary),
                    );
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
                },
            );

            ui.add_space(6.0);
            ui.allocate_ui_with_layout(
                Vec2::new(row_w, 20.0),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if active_srv.via_tunnel {
                        pill(ui, "加密隧道", p.success_soft, p.success_text);
                    } else if active_srv.base_url.trim().starts_with("https://") {
                        pill(ui, "HTTPS", p.success_soft, p.success_text);
                    } else {
                        pill(ui, "明文 HTTP", p.danger_soft, p.danger_text);
                    }
                    ui.add(
                        egui::Label::new(
                            RichText::new(&active_srv.base_url)
                                .size(FONT_SM)
                                .color(p.text_muted),
                        )
                        .truncate(),
                    );
                },
            );
        });

        // Floating Server Editor Modal (Add / Edit Remote Server)
        if let Some(mut ed) = self.admin.server_editor.take() {
            let mut keep_open = true;
            let mut save_clicked = false;
            let mut delete_id: Option<String> = None;
            let can_delete = ed.editing_id.is_some() && self.cfg.admin_servers.len() > 1;
            let title = if ed.editing_id.is_some() {
                "编辑远程服务端配置"
            } else {
                "添加远程服务端"
            };
            let dismissed = modal_dialog(ctx, "admin_server_editor_modal", title, 480.0, p, |ui| {
                ui.columns(2, |cols| {
                    cols[0].add(
                        egui::Label::new(RichText::new("名称").size(FONT_XS).color(p.text_muted))
                            .truncate(),
                    );
                    let w0 = cols[0].available_width();
                    styled_input(&mut cols[0], &mut ed.name, "服务端名称", w0, false, p);
                    cols[1].add(
                        egui::Label::new(RichText::new("账号").size(FONT_XS).color(p.text_muted))
                            .truncate(),
                    );
                    let w1 = cols[1].available_width();
                    styled_input(&mut cols[1], &mut ed.username, "admin", w1, false, p);
                });

                ui.add_space(10.0);
                ui.label(RichText::new("接口地址").size(FONT_XS).color(p.text_muted));
                styled_input(
                    ui,
                    &mut ed.base_url,
                    "http://panel.gacl.local",
                    ui.available_width(),
                    false,
                    p,
                );

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .checkbox(&mut ed.via_tunnel, "经加密隧道访问")
                        .changed()
                        && ed.via_tunnel
                        && (ed.base_url.trim().is_empty() || ed.base_url.trim() == "https://")
                    {
                        ed.base_url = "http://panel.gacl.local".to_string();
                    }
                });

                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if can_delete
                        && let Some(eid) = &ed.editing_id
                        && soft_btn(ui, "删除此服务端", p).clicked()
                    {
                        delete_id = Some(eid.clone());
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if primary_btn(ui, "保存服务端", p).clicked() {
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
                self.show_toast("已删除远程服务端配置");
                keep_open = false;
            } else if save_clicked {
                let name = ed.name.trim().to_string();
                let url = ed.base_url.trim().trim_end_matches('/').to_string();
                let username = if ed.username.trim().is_empty() {
                    "admin".to_string()
                } else {
                    ed.username.trim().to_string()
                };
                if name.is_empty() || (!url.starts_with("http://") && !url.starts_with("https://"))
                {
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

        ui.add_space(12.0);

        // 2. Login View vs Authenticated Dashboard
        if !logged_in {
            card_frame(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(format!("登录「{}」", active_srv.name))
                        .size(FONT_SUB)
                        .strong()
                        .color(p.text_primary),
                );
                if active_srv.via_tunnel && self.state != ConnState::Connected {
                    ui.add_space(8.0);
                    Frame::new()
                        .fill(p.accent_soft)
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(Margin::symmetric(10, 6))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                RichText::new("经隧道访问需先在「加速首页」开启加速")
                                    .size(FONT_SM)
                                    .color(p.accent_text),
                            );
                        });
                }

                ui.add_space(10.0);
                ui.columns(2, |cols| {
                    cols[0].add(
                        egui::Label::new(RichText::new("账号").size(FONT_XS).color(p.text_muted))
                            .truncate(),
                    );
                    let w0 = cols[0].available_width();
                    styled_input(&mut cols[0], &mut self.admin.login_user, "admin", w0, false, p);
                    cols[1].add(
                        egui::Label::new(RichText::new("密码").size(FONT_XS).color(p.text_muted))
                            .truncate(),
                    );
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
                        "正在登录..."
                    } else {
                        "登录管理后台"
                    };
                    if primary_btn(ui, lbl, p).clicked() {
                        self.admin_login();
                    }
                });
            });
            return;
        }

        // Authenticated: 4 Equal-Height KPI Cards
        if let Some(ov) = &self.admin.overview {
            ui.columns(4, |cols| {
                let kpis = [
                    (
                        "监听节点",
                        format!("UDP :{}", ov.udp_port),
                        ov.sni.clone(),
                        format!("{} · {}", ov.public_host, ov.sni),
                    ),
                    (
                        "在线连接",
                        ov.total_online_conns.to_string(),
                        format!("已运行 {}", format_uptime(ov.uptime_secs)),
                        format!("已运行 {}", format_uptime(ov.uptime_secs)),
                    ),
                    (
                        "累计流量",
                        format_bytes(ov.total_tx_bytes + ov.total_rx_bytes),
                        format!(
                            "↑{} ↓{}",
                            format_bytes(ov.total_tx_bytes),
                            format_bytes(ov.total_rx_bytes)
                        ),
                        format!(
                            "上传 {} · 下载 {}",
                            format_bytes(ov.total_tx_bytes),
                            format_bytes(ov.total_rx_bytes)
                        ),
                    ),
                    (
                        "分享链接",
                        format!("{} / {}", ov.active_links, ov.total_links),
                        "启用 / 总数".to_string(),
                        "启用 / 总数".to_string(),
                    ),
                ];
                for (i, (label, val, sub, tooltip)) in kpis.into_iter().enumerate() {
                    card_frame(p).show(&mut cols[i], |ui| {
                        ui.set_width(ui.available_width());
                        ui.set_min_height(72.0);
                        ui.add(
                            egui::Label::new(RichText::new(label).size(FONT_XS).color(p.text_muted))
                                .truncate(),
                        );
                        ui.add_space(4.0);
                        ui.add(
                            egui::Label::new(
                                RichText::new(val)
                                    .size(FONT_TITLE)
                                    .strong()
                                    .color(p.text_primary),
                            )
                            .truncate(),
                        );
                        ui.add_space(4.0);
                        ui.add(
                            egui::Label::new(RichText::new(sub).size(FONT_XS).color(p.text_muted))
                                .truncate(),
                        )
                        .on_hover_text(tooltip);
                    });
                }
            });
            ui.add_space(12.0);
        }

        // Links Management Card
        card_frame(p).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("分享链接")
                        .size(FONT_SUB)
                        .strong()
                        .color(p.text_primary),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if primary_btn(ui, "+ 新建链接", p).clicked() {
                        self.admin.link_editor = Some(AdminLinkEditor::new_blank());
                        self.admin.server_editor = None;
                    }
                });
            });

            // Floating Create / Edit Link Modal
            if let Some(mut ed) = self.admin.link_editor.take() {
                let mut keep_open = true;
                let mut save_clicked = false;
                let title = if ed.editing_id.is_some() {
                    "编辑分享链接"
                } else {
                    "新建分享链接"
                };
                let dismissed =
                    modal_dialog(ctx, "admin_link_editor_modal", title, 480.0, p, |ui| {
                        ui.columns(3, |cols| {
                            cols[0].add(
                                egui::Label::new(
                                    RichText::new("名称").size(FONT_XS).color(p.text_muted),
                                )
                                .truncate(),
                            );
                            let w0 = cols[0].available_width();
                            styled_input(&mut cols[0], &mut ed.name, "备注名称", w0, false, p);

                            cols[1].add(
                                egui::Label::new(
                                    RichText::new("流量上限 (GB)")
                                        .size(FONT_XS)
                                        .color(p.text_muted),
                                )
                                .truncate(),
                            );
                            let w1 = cols[1].available_width();
                            styled_input(
                                &mut cols[1],
                                &mut ed.traffic_limit_gb,
                                "不限",
                                w1,
                                false,
                                p,
                            );

                            cols[2].add(
                                egui::Label::new(
                                    RichText::new("到期日期")
                                        .size(FONT_XS)
                                        .color(p.text_muted),
                                )
                                .truncate(),
                            );
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
                        ui.checkbox(&mut ed.game_dup, "游戏小包双发");
                        ui.add_space(14.0);
                        ui.horizontal(|ui| {
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

            ui.add_space(10.0);
            if self.admin.links.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(16.0);
                    ui.label(
                        RichText::new("暂无分享链接，点击右上角「+ 新建分享链接」创建")
                            .size(FONT_SM)
                            .color(p.text_muted),
                    );
                    ui.add_space(16.0);
                });
            } else {
                let links_snapshot = self.admin.links.clone();
                let sub_base = active_srv.base_url.trim_end_matches('/').to_string();
                for l in &links_snapshot {
                    Frame::new()
                        .fill(p.bg_card_alt)
                        .stroke(Stroke::new(1.0_f32, p.border_subtle))
                        .corner_radius(CornerRadius::same(10))
                        .inner_margin(Margin::symmetric(12, 10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                ui.label(
                                    RichText::new(&l.name)
                                        .size(FONT_BODY)
                                        .strong()
                                        .color(p.text_primary),
                                );
                                match l.status.as_str() {
                                    "active" => pill(ui, "启用中", p.success_soft, p.success_text),
                                    "disabled" => pill(ui, "已禁用", p.bg_card, p.text_muted),
                                    "expired" => pill(ui, "已过期", p.accent_soft, p.accent_text),
                                    _ => pill(ui, "流量超额", p.danger_soft, p.danger_text),
                                }
                                if l.game_dup {
                                    pill(ui, "游戏双发", p.accent_soft, p.accent_text);
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

                            ui.add_space(4.0);
                            let used = l.tx_bytes + l.rx_bytes;
                            let limit_str = l
                                .traffic_limit_gb
                                .map(|g| format!("{g} GB"))
                                .unwrap_or_else(|| "不限".to_string());
                            let exp_str = l.expire_at.as_deref().unwrap_or("永不过期");
                            ui.label(
                                RichText::new(format!(
                                    "流量: {} / {} (↑ {} · ↓ {}) · 到期: {}",
                                    format_bytes(used),
                                    limit_str,
                                    format_bytes(l.tx_bytes),
                                    format_bytes(l.rx_bytes),
                                    exp_str
                                ))
                                .size(FONT_XS)
                                .color(p.text_muted),
                            );

                            if let Some(gb) = l.traffic_limit_gb
                                && gb > 0.0
                            {
                                let pct = (used as f64 / (gb * 1024.0 * 1024.0 * 1024.0))
                                    .clamp(0.0, 1.0) as f32;
                                ui.add_space(4.0);
                                let (bar_rect, _) = ui.allocate_exact_size(
                                    Vec2::new(220.0_f32.min(ui.available_width()), 5.0),
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
                                        Vec2::new(bar_rect.width() * pct, bar_rect.height()),
                                    );
                                    ui.painter().rect_filled(
                                        fill_rect,
                                        CornerRadius::same(3),
                                        if pct >= 0.95 { p.danger_text } else { p.accent },
                                    );
                                }
                            }

                            ui.add_space(6.0);
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing = Vec2::new(6.0, 4.0);
                                if accent_chip_btn(ui, "复制 gacl://", p, true).clicked() {
                                    ctx.copy_text(l.share_uri.clone());
                                    self.show_toast("已复制 gacl:// 分享链接");
                                }
                                if !l.hy2_uri.is_empty() && chip_btn(ui, "复制 hy2://", p, false).clicked() {
                                    ctx.copy_text(l.hy2_uri.clone());
                                    self.show_toast("已复制 hysteria2:// 链接（小火箭/Clash 通用）");
                                }
                                if chip_btn(ui, "复制订阅", p, false).clicked() {
                                    ctx.copy_text(format!("{sub_base}/sub/{}", l.token));
                                    self.show_toast("已复制订阅 URL");
                                }
                                let toggle_lbl = if l.enabled { "禁用" } else { "启用" };
                                if chip_btn(ui, toggle_lbl, p, false).clicked() {
                                    self.admin_toggle_link(l.id.clone(), !l.enabled);
                                }
                                if chip_btn(ui, "编辑", p, false).clicked() {
                                    self.admin.link_editor = Some(AdminLinkEditor::from_link(l));
                                    self.admin.server_editor = None;
                                }

                                let confirm_reset = self.admin.confirm_action.as_ref()
                                    == Some(&("reset-token".to_string(), l.id.clone()));
                                if confirm_reset {
                                    if accent_chip_btn(ui, "确认重置密钥", p, true).clicked() {
                                        self.admin_post_link_action(
                                            l.id.clone(),
                                            "reset-token",
                                            "密钥已重置，请重新复制分享链接",
                                        );
                                    }
                                    if chip_btn(ui, "取消", p, false).clicked() {
                                        self.admin.confirm_action = None;
                                    }
                                } else if chip_btn(ui, "重置密钥", p, false).clicked() {
                                    self.admin.confirm_action =
                                        Some(("reset-token".to_string(), l.id.clone()));
                                }

                                if chip_btn(ui, "清零流量", p, false).clicked() {
                                    self.admin_post_link_action(
                                        l.id.clone(),
                                        "reset-traffic",
                                        "已清零流量统计",
                                    );
                                }

                                let confirm_del = self.admin.confirm_action.as_ref()
                                    == Some(&("delete".to_string(), l.id.clone()));
                                if confirm_del {
                                    if chip_btn(ui, "确认删除", p, true).clicked() {
                                        self.admin_delete_link(l.id.clone());
                                    }
                                    if chip_btn(ui, "取消", p, false).clicked() {
                                        self.admin.confirm_action = None;
                                    }
                                } else if chip_btn(ui, "删除", p, true).clicked() {
                                    self.admin.confirm_action =
                                        Some(("delete".to_string(), l.id.clone()));
                                }
                            });
                        });
                    ui.add_space(6.0);
                }
            }
        });
    }
}

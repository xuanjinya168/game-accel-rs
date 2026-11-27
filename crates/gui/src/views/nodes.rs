use crate::icons;
use crate::storage::save_config;
use crate::theme::{FONT_BODY, FONT_SM, FONT_SUB, FONT_XS, ThemePalette};
use crate::widgets::{
    accent_chip_btn, aligned_row, card_frame, chip_btn, elide_text, modal_dialog, pill, primary_btn,
    soft_btn, styled_input, styled_multiline,
};
use crate::{ConnState, GameAccelApp, NodeEditorState, ProbeState};
use chrono::Local;
use eframe::egui::{
    self, Align, CornerRadius, FontId, Frame, Layout, Margin, RichText, ScrollArea, Sense, Stroke,
    Vec2,
};
use protocol::share::{NodeProfile, NodeProtocol, parse_import_text};

impl GameAccelApp {
    pub(super) fn render_nodes(&mut self, ui: &mut egui::Ui, p: ThemePalette) {
        // Unified top action toolbar (32px locked height; Export relocated here from Settings)
        aligned_row(ui, 32.0, |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if primary_btn(ui, "+ 新建节点", p).clicked() {
                self.node_editor = Some(NodeEditorState::new_blank());
                self.import_panel_open = false;
                self.sub_panel_open = false;
            }
            if soft_btn(ui, "导入链接", p).clicked() {
                self.import_panel_open = !self.import_panel_open;
                self.sub_panel_open = false;
                self.node_editor = None;
            }
            if soft_btn(ui, "订阅管理", p).clicked() {
                self.sub_panel_open = !self.sub_panel_open;
                self.import_panel_open = false;
                self.node_editor = None;
            }
            ui.add_enabled_ui(!self.cfg.nodes.is_empty(), |ui| {
                if soft_btn(ui, "全部测速", p).clicked() {
                    self.probe_all_nodes();
                }
            });

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled_ui(!self.cfg.nodes.is_empty(), |ui| {
                    if soft_btn(ui, "导出全部", p).clicked() {
                        let all_uris = self
                            .cfg
                            .nodes
                            .iter()
                            .map(|n| n.to_share_uri())
                            .collect::<Vec<_>>()
                            .join("\n");
                        ui.ctx().copy_text(all_uris);
                        self.show_toast(format!(
                            "已导出 {} 个节点的分享链接",
                            self.cfg.nodes.len()
                        ));
                    }
                });
            });
        });

        ui.add_space(6.0);

        // Modal A: Import share links or JSON
        if self.import_panel_open {
            let ctx = ui.ctx().clone();
            let mut close_import = false;
            let mut do_import = false;
            let dismissed = modal_dialog(&ctx, "import_nodes_modal", "导入节点链接", 460.0, p, |ui| {
                styled_multiline(
                    ui,
                    &mut self.import_text,
                    "粘贴 gacl://、hysteria2:// 分享链接（支持多行）或订阅 JSON…",
                    5,
                    p,
                );
                ui.add_space(14.0);
                aligned_row(ui, 30.0, |ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if primary_btn(ui, "导入", p).clicked() {
                            do_import = true;
                        }
                        if soft_btn(ui, "取消", p).clicked() {
                            close_import = true;
                        }
                    });
                });
            });
            if do_import {
                match parse_import_text(&self.import_text) {
                    Ok(nodes) => {
                        let total = nodes.len();
                        let added = self.merge_imported_nodes(nodes, None);
                        self.import_text.clear();
                        self.import_panel_open = false;
                        self.show_toast(format!("导入成功：解析 {total} 个节点（新增 {added}）"));
                    }
                    Err(e) => {
                        self.show_toast(format!("导入失败: {e:#}"));
                    }
                }
            } else if dismissed || close_import {
                self.import_panel_open = false;
            }
        }

        // Modal B: Subscription URL management
        if self.sub_panel_open {
            let ctx = ui.ctx().clone();
            let mut fetch_url: Option<String> = None;
            let mut close_sub = false;
            let subs = self.cfg.subscriptions.clone();
            let sub_loading = self.sub_loading;
            let dismissed = modal_dialog(&ctx, "sub_url_modal", "订阅管理", 480.0, p, |ui| {
                aligned_row(ui, 32.0, |ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_enabled_ui(!sub_loading, |ui| {
                            let lbl = if sub_loading { "同步中…" } else { "同步订阅" };
                            if primary_btn(ui, lbl, p).clicked() {
                                fetch_url = Some(self.sub_url_input.clone());
                            }
                        });
                        let w = ui.available_width().max(200.0);
                        styled_input(
                            ui,
                            &mut self.sub_url_input,
                            "https://panel.example.com:9090/sub/token",
                            w,
                            false,
                            p,
                        );
                    });
                });
                if !subs.is_empty() {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new("已保存订阅")
                            .size(FONT_XS)
                            .color(p.text_muted),
                    );
                    ui.add_space(4.0);
                    for sub in &subs {
                        aligned_row(ui, 28.0, |ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if chip_btn(ui, "刷新", p, false).clicked() {
                                    fetch_url = Some(sub.url.clone());
                                }
                                let max_w = ui.available_width().max(80.0);
                                let raw = format!(
                                    "{} · {} ({})",
                                    sub.name,
                                    sub.url,
                                    sub.last_updated.as_deref().unwrap_or("-")
                                );
                                let shown = elide_text(
                                    ui,
                                    &raw,
                                    FontId::proportional(FONT_SM),
                                    max_w,
                                );
                                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                    ui.label(
                                        RichText::new(shown)
                                            .size(FONT_SM)
                                            .color(p.text_secondary),
                                    );
                                });
                            });
                        });
                    }
                }
                ui.add_space(12.0);
                aligned_row(ui, 30.0, |ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if soft_btn(ui, "关闭", p).clicked() {
                            close_sub = true;
                        }
                    });
                });
            });
            if let Some(url) = fetch_url {
                self.fetch_subscription(url);
            }
            if dismissed || close_sub {
                self.sub_panel_open = false;
            }
        }

        // Modal C: Node Add / Edit Dialog
        let mut editor_save: Option<NodeProfile> = None;
        let mut editor_cancel = false;
        if let Some(ed) = &mut self.node_editor {
            let ctx = ui.ctx().clone();
            let title = if ed.editing_id.is_some() {
                "编辑节点"
            } else {
                "新建节点"
            };
            let dismissed = modal_dialog(&ctx, "node_editor_modal", title, 460.0, p, |ui| {
                egui::Grid::new("node_editor_grid")
                    .num_columns(2)
                    .spacing([14.0, 10.0])
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("协议")
                                .size(FONT_BODY)
                                .color(p.text_secondary),
                        );
                        ui.horizontal(|ui| {
                            if accent_chip_btn(ui, "GACL", p, ed.protocol == NodeProtocol::Gacl)
                                .clicked()
                            {
                                ed.protocol = NodeProtocol::Gacl;
                            }
                            if accent_chip_btn(
                                ui,
                                "Hysteria2",
                                p,
                                ed.protocol == NodeProtocol::Hysteria2,
                            )
                            .clicked()
                            {
                                ed.protocol = NodeProtocol::Hysteria2;
                                ed.game_dup = false;
                            }
                        });
                        ui.end_row();

                        ui.label(
                            RichText::new("名称")
                                .size(FONT_BODY)
                                .color(p.text_secondary),
                        );
                        styled_input(ui, &mut ed.name, "例如：香港 BGP 专线", 310.0, false, p);
                        ui.end_row();

                        ui.label(
                            RichText::new("服务器地址")
                                .size(FONT_BODY)
                                .color(p.text_secondary),
                        );
                        styled_input(
                            ui,
                            &mut ed.server,
                            "hk1.example.com:8443",
                            310.0,
                            false,
                            p,
                        );
                        ui.end_row();

                        ui.label(
                            RichText::new("SNI 域名")
                                .size(FONT_BODY)
                                .color(p.text_secondary),
                        );
                        styled_input(
                            ui,
                            &mut ed.sni,
                            "留空默认与服务器域名相同",
                            310.0,
                            false,
                            p,
                        );
                        ui.end_row();

                        ui.label(
                            RichText::new("连接密钥")
                                .size(FONT_BODY)
                                .color(p.text_secondary),
                        );
                        ui.horizontal(|ui| {
                            styled_input(
                                ui,
                                &mut ed.token,
                                "粘贴鉴权 Token",
                                254.0,
                                !ed.show_token,
                                p,
                            );
                            let lbl = if ed.show_token { "隐藏" } else { "显示" };
                            if chip_btn(ui, lbl, p, false).clicked() {
                                ed.show_token = !ed.show_token;
                            }
                        });
                        ui.end_row();

                        ui.label(
                            RichText::new("Brutal 速率")
                                .size(FONT_BODY)
                                .color(p.text_secondary),
                        );
                        styled_input(
                            ui,
                            &mut ed.brutal_mbps,
                            "0 (默认 BBR，单位 Mbps)",
                            310.0,
                            false,
                            p,
                        );
                        ui.end_row();
                    });

                ui.add_space(12.0);
                aligned_row(ui, 24.0, |ui| {
                    if ed.protocol == NodeProtocol::Gacl {
                        icons::pill_toggle(ui, &mut ed.game_dup, p);
                        ui.label(
                            RichText::new("游戏小包双发")
                                .size(FONT_BODY)
                                .color(p.text_primary),
                        );
                        ui.add_space(16.0);
                    }
                    icons::pill_toggle(ui, &mut ed.insecure, p);
                    ui.label(
                        RichText::new("跳过证书校验")
                            .size(FONT_BODY)
                            .color(p.text_secondary),
                    );
                });
                ui.add_space(14.0);
                aligned_row(ui, 30.0, |ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if primary_btn(ui, "保存", p).clicked() {
                            let id = ed.editing_id.clone().unwrap_or_else(|| {
                                format!("node-{}", Local::now().timestamp_millis())
                            });
                            let srv = ed.server.trim().to_string();
                            let name = if ed.name.trim().is_empty() {
                                format!("节点 {srv}")
                            } else {
                                ed.name.trim().to_string()
                            };
                            let sni = if ed.sni.trim().is_empty() {
                                srv.rsplit_once(':')
                                    .map(|(h, _)| h.trim_matches(['[', ']']).to_string())
                                    .unwrap_or_else(|| srv.clone())
                            } else {
                                ed.sni.trim().to_string()
                            };
                            let brutal_mbps =
                                ed.brutal_mbps.trim().parse::<u32>().unwrap_or(0).min(1000);
                            editor_save = Some(NodeProfile {
                                id,
                                name,
                                server: srv,
                                sni,
                                token: ed.token.trim().to_string(),
                                protocol: ed.protocol,
                                game_dup: ed.protocol == NodeProtocol::Gacl && ed.game_dup,
                                insecure: ed.insecure,
                                brutal_mbps,
                                source: None,
                            });
                        }
                        if soft_btn(ui, "取消", p).clicked() {
                            editor_cancel = true;
                        }
                    });
                });
            });
            if dismissed {
                editor_cancel = true;
            }
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

        // Concise Empty-state Card with direct CTA buttons
        if self.cfg.nodes.is_empty() {
            card_frame(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add_space(20.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new("暂无加速节点")
                            .size(FONT_SUB)
                            .strong()
                            .color(p.text_primary),
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        let total_btn_w = 192.0;
                        let pad = ((ui.available_width() - total_btn_w) * 0.5).max(0.0);
                        ui.add_space(pad);
                        if soft_btn(ui, "导入链接", p).clicked() {
                            self.import_panel_open = true;
                        }
                        if primary_btn(ui, "+ 新建节点", p).clicked() {
                            self.node_editor = Some(NodeEditorState::new_blank());
                        }
                    });
                });
                ui.add_space(20.0);
            });
            return;
        }

        // Node List Cards (Whole-card 1-click selection; hot-reconnects if currently connected)
        let nodes = self.cfg.nodes.clone();
        let modal_open =
            self.node_editor.is_some() || self.import_panel_open || self.sub_panel_open;
        let mut select_id: Option<String> = None;
        let mut delete_id: Option<String> = None;
        let mut probe_node: Option<NodeProfile> = None;
        let mut edit_node: Option<NodeProfile> = None;
        let mut share_node: Option<NodeProfile> = None;

        ScrollArea::vertical().show(ui, |ui| {
            for node in &nodes {
                let is_active = self.cfg.active_node_id == node.id;
                let border = if is_active { p.accent } else { p.border_subtle };
                let mut action_interacted = false;

                let frame_resp = Frame::new()
                    .fill(p.bg_card)
                    .stroke(Stroke::new(
                        if is_active { 1.5_f32 } else { 1.0_f32 },
                        border,
                    ))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::symmetric(16, 12))
                    .shadow(p.card_shadow)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing = Vec2::new(7.0, 4.0);

                        // Row 1: Locked 26px height so dot, name, pills & right buttons align at 0px offset
                        aligned_row(ui, 26.0, |ui| {
                            let (dr, _) =
                                ui.allocate_exact_size(Vec2::new(8.0, 18.0), Sense::hover());
                            ui.painter().circle_filled(
                                dr.center(),
                                3.6,
                                if is_active { p.accent } else { p.border_strong },
                            );

                            let max_name_w = (ui.available_width() - 280.0).max(90.0);
                            let shown_name = elide_text(
                                ui,
                                &node.name,
                                FontId::proportional(FONT_SUB),
                                max_name_w,
                            );
                            ui.label(
                                RichText::new(shown_name)
                                    .size(FONT_SUB)
                                    .strong()
                                    .color(p.text_primary),
                            );

                            if node.protocol == NodeProtocol::Hysteria2 {
                                pill(ui, "HY2", p.accent_soft, p.accent_text);
                            } else {
                                pill(ui, "GACL", p.tag_bg, p.text_muted);
                            }
                            if node.insecure {
                                pill(ui, "跳过校验", p.warn_soft, p.warn_text);
                            }

                            if let Some(ps) = self.probes.get(&node.id) {
                                match ps {
                                    ProbeState::Probing => {
                                        pill(ui, "测速中…", p.tag_bg, p.text_muted);
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
                                    let r = chip_btn(ui, "删除", p, true);
                                    action_interacted |= r.hovered() || r.clicked();
                                    if r.clicked() {
                                        delete_id = Some(node.id.clone());
                                    }
                                });
                                let r_edit = chip_btn(ui, "编辑", p, false);
                                action_interacted |= r_edit.hovered() || r_edit.clicked();
                                if r_edit.clicked() {
                                    edit_node = Some(node.clone());
                                }
                                let r_share = chip_btn(ui, "分享", p, false);
                                action_interacted |= r_share.hovered() || r_share.clicked();
                                if r_share.clicked() {
                                    share_node = Some(node.clone());
                                }
                                let r_probe = chip_btn(ui, "测速", p, false);
                                action_interacted |= r_probe.hovered() || r_probe.clicked();
                                if r_probe.clicked() {
                                    probe_node = Some(node.clone());
                                }
                            });
                        });

                        // Row 2: Clean monospace endpoint + meaningful traits only (no TLS 1.3 spam)
                        ui.horizontal(|ui| {
                            ui.add_space(15.0);
                            let host_only = node
                                .server
                                .rsplit_once(':')
                                .map(|(h, _)| h.trim_matches(['[', ']']))
                                .unwrap_or(node.server.as_str());
                            let mut parts = vec![node.server.clone()];
                            if !node.sni.is_empty() && !node.sni.eq_ignore_ascii_case(host_only) {
                                parts.push(format!("SNI {}", node.sni));
                            }
                            if node.protocol == NodeProtocol::Gacl && node.game_dup {
                                parts.push("自适应双发".to_string());
                            }
                            if node.brutal_mbps > 0 {
                                parts.push(format!("Brutal {}M", node.brutal_mbps));
                            }
                            ui.label(
                                RichText::new(parts.join("  ·  "))
                                    .size(FONT_SM)
                                    .monospace()
                                    .color(p.text_muted),
                            );
                        });
                    });

                // Whole-card click to select without overlapping right-side action buttons
                if !modal_open && !is_active && !action_interacted {
                    let card_rect = frame_resp.response.rect;
                    if ui.rect_contains_pointer(card_rect) {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        ui.painter().rect_stroke(
                            card_rect,
                            CornerRadius::same(12),
                            Stroke::new(1.0_f32, p.border_strong),
                            egui::StrokeKind::Inside,
                        );
                        if ui.input(|i| i.pointer.primary_clicked()) {
                            select_id = Some(node.id.clone());
                        }
                    }
                }

                ui.add_space(8.0);
            }
        });

        if let Some(id) = select_id {
            self.switch_active_node(id);
        }
        if let Some(id) = delete_id {
            self.cfg.nodes.retain(|n| n.id != id);
            if self.cfg.domestic_node_id.as_deref() == Some(id.as_str()) {
                self.cfg.domestic_node_id = None;
            }
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
            self.show_toast(format!("已复制「{}」的分享链接", n.name));
        }
    }
}

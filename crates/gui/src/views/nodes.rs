use crate::icons;
use crate::storage::save_config;
use crate::theme::{
    FONT_BODY, FONT_SM, FONT_SUB, FONT_XS, ThemePalette,
};
use crate::widgets::{
    accent_chip_btn, card_frame, chip_btn, modal_dialog, pill, primary_btn, soft_btn,
    styled_input, styled_multiline,
};
use crate::{ConnState, GameAccelApp, NodeEditorState, ProbeState};
use chrono::Local;
use eframe::egui::{
    self, Align, CornerRadius, Frame, Layout, Margin, RichText, ScrollArea, Sense, Stroke, Vec2,
};
use protocol::share::{NodeProfile, NodeProtocol, parse_import_text};

impl GameAccelApp {
    pub(super) fn render_nodes(&mut self, ui: &mut egui::Ui, p: ThemePalette) {
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

        // Panel A: Import gacl:// / hysteria2:// or JSON modal
        if self.import_panel_open {
            let ctx = ui.ctx().clone();
            let mut close_import = false;
            let mut do_import = false;
            let dismissed = modal_dialog(
                &ctx,
                "import_nodes_modal",
                "导入分享链接 / JSON",
                520.0,
                p,
                |ui| {
                    ui.label(
                        RichText::new(
                            "粘贴 gacl:// / hysteria2:// 分享链接（支持多行）或订阅 JSON",
                        )
                        .size(FONT_SM)
                        .color(p.text_secondary),
                    );
                    ui.add_space(8.0);
                    styled_multiline(
                        ui,
                        &mut self.import_text,
                        "gacl://token@host:443?sni=host&dup=1#主力\nhysteria2://password@host:443/?sni=host#Hy2节点",
                        5,
                        p,
                    );
                    ui.add_space(14.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if primary_btn(ui, "确认导入", p).clicked() {
                            do_import = true;
                        }
                        if soft_btn(ui, "取消", p).clicked() {
                            close_import = true;
                        }
                    });
                },
            );
            if do_import {
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
            } else if dismissed || close_import {
                self.import_panel_open = false;
            }
        }

        // Panel B: Subscription URL fetch modal
        if self.sub_panel_open {
            let ctx = ui.ctx().clone();
            let mut fetch_url: Option<String> = None;
            let mut close_sub = false;
            let subs = self.cfg.subscriptions.clone();
            let sub_loading = self.sub_loading;
            let dismissed = modal_dialog(
                &ctx,
                "sub_url_modal",
                "订阅 URL 同步",
                520.0,
                p,
                |ui| {
                    ui.label(
                        RichText::new("支持管理面板 JSON、gacl:// 或 hysteria2:// 列表")
                            .size(FONT_SM)
                            .color(p.text_secondary),
                    );
                    ui.add_space(8.0);
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
                        ui.add_enabled_ui(!sub_loading, |ui| {
                            let lbl = if sub_loading { "拉取中..." } else { "拉取订阅" };
                            if primary_btn(ui, lbl, p).clicked() {
                                fetch_url = Some(self.sub_url_input.clone());
                            }
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
                            ui.horizontal(|ui| {
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if chip_btn(ui, "刷新", p, false).clicked() {
                                        fetch_url = Some(sub.url.clone());
                                    }
                                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(format!(
                                                    "{} · {} (更新: {})",
                                                    sub.name,
                                                    sub.url,
                                                    sub.last_updated.as_deref().unwrap_or("-")
                                                ))
                                                .size(FONT_SM)
                                                .color(p.text_secondary),
                                            )
                                            .truncate(),
                                        );
                                    });
                                });
                            });
                        }
                    }
                    ui.add_space(12.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if soft_btn(ui, "关闭", p).clicked() {
                            close_sub = true;
                        }
                    });
                },
            );
            if let Some(url) = fetch_url {
                self.fetch_subscription(url);
            }
            if dismissed || close_sub {
                self.sub_panel_open = false;
            }
        }

        // Panel C: Node Add/Edit Floating Modal Dialog
        let mut editor_save: Option<NodeProfile> = None;
        let mut editor_cancel = false;
        if let Some(ed) = &mut self.node_editor {
            let ctx = ui.ctx().clone();
            let title = if ed.editing_id.is_some() {
                "编辑节点"
            } else {
                "新建节点"
            };
            let dismissed = modal_dialog(&ctx, "node_editor_modal", title, 500.0, p, |ui| {
                egui::Grid::new("node_editor_grid")
                    .num_columns(2)
                    .spacing([14.0, 10.0])
                    .show(ui, |ui| {
                        ui.label(RichText::new("协议类型").size(FONT_BODY).color(p.text_secondary));
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

                        ui.label(
                            RichText::new("TCP Brutal 目标速率 (Mbps)")
                                .size(FONT_BODY)
                                .color(p.text_secondary),
                        );
                        styled_input(
                            ui,
                            &mut ed.brutal_mbps,
                            "0 = 关闭 (默认 BBR)；仅作用于 TCP 通道",
                            300.0,
                            false,
                            p,
                        );
                        ui.end_row();
                    });

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ed.protocol == NodeProtocol::Gacl {
                        icons::pill_toggle(ui, &mut ed.game_dup, p);
                        ui.label(
                            RichText::new("丢包自适应双发 (< 200B)")
                                .size(FONT_BODY)
                                .color(p.text_primary),
                        );
                        ui.add_space(16.0);
                    }
                    icons::pill_toggle(ui, &mut ed.insecure, p);
                    ui.label(
                        RichText::new("允许自签证书")
                            .size(FONT_BODY)
                            .color(p.text_secondary),
                    );
                });
                ui.add_space(14.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if primary_btn(ui, "保存节点", p).clicked() {
                        let id = ed.editing_id.clone().unwrap_or_else(|| {
                            format!("node-{}", Local::now().timestamp_millis())
                        });
                        let name = if ed.name.trim().is_empty() {
                            format!("节点 {}", ed.server.trim())
                        } else {
                            ed.name.trim().to_string()
                        };
                        let brutal_mbps = ed
                            .brutal_mbps
                            .trim()
                            .parse::<u32>()
                            .unwrap_or(0)
                            .min(1000);
                        editor_save = Some(NodeProfile {
                            id,
                            name,
                            server: ed.server.trim().to_string(),
                            sni: ed.sni.trim().to_string(),
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
                            if node.protocol == NodeProtocol::Hysteria2 {
                                pill(ui, "HY2", p.accent_soft, p.accent);
                            } else {
                                pill(ui, "GACL", p.tag_bg, p.text_muted);
                            }

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
                            if node.protocol == NodeProtocol::Gacl && node.game_dup {
                                meta.push_str("  ·  自适应双发");
                            }
                            if node.brutal_mbps > 0 {
                                meta.push_str(&format!("  ·  Brutal {}M", node.brutal_mbps));
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

}

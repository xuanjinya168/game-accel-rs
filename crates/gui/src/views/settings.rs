use crate::logger;
use crate::storage::{TrayStyle, config_dir, save_config};
use crate::theme::{FONT_BODY, FONT_SM, ThemeMode, ThemePalette, apply_theme};
use crate::widgets::{card_frame, hairline_divider, primary_btn, soft_btn, styled_input};
use crate::{ConnState, GameAccelApp, NavTab, relaunch_as_admin};
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Layout, Pos2, RichText, ScrollArea, Sense, Stroke,
    Vec2,
};

impl GameAccelApp {
    pub(super) fn render_settings(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, p: ThemePalette) {
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

            // Row 1.2: Tray right-click style (floating popover vs native menu)
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        RichText::new("托盘右键样式")
                            .size(FONT_BODY)
                            .strong()
                            .color(p.text_primary),
                    );
                    ui.label(
                        RichText::new("悬浮面板支持快捷切节点/模式与测速，原生菜单为系统极简菜单")
                            .size(FONT_SM)
                            .color(p.text_muted),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (pill_rect, _) =
                        ui.allocate_exact_size(Vec2::new(150.0, 30.0), Sense::hover());
                    ui.painter().rect(
                        pill_rect,
                        CornerRadius::same(9),
                        p.bg_card_alt,
                        Stroke::new(1.0_f32, p.border_subtle),
                        egui::StrokeKind::Inside,
                    );
                    let seg_w = 70.0;
                    let seg_h = 24.0;
                    for (idx, (style, lbl)) in [
                        (TrayStyle::Panel, "悬浮面板"),
                        (TrayStyle::Menu, "原生菜单"),
                    ]
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
                            ui.id().with(("tray_style_seg", idx)),
                            Sense::click(),
                        );
                        let active = self.cfg.tray_style == style;
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
                        if resp.clicked() && self.cfg.tray_style != style {
                            self.cfg.tray_style = style;
                            save_config(&self.cfg);
                            if style == TrayStyle::Menu {
                                self.tray_popover_open = false;
                            }
                            if let Some(ref tray) = self.tray {
                                tray.set_style(style);
                            }
                            self.show_toast(match style {
                                TrayStyle::Panel => "已切换为托盘悬浮面板",
                                TrayStyle::Menu => "已切换为系统原生托盘菜单",
                            });
                        }
                    }
                });
            });

            ui.add_space(12.0);
            hairline_divider(ui, p);
            ui.add_space(12.0);

            // Row 1.3: Desktop floating mini speed pill
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        RichText::new("桌面网速悬浮窗")
                            .size(FONT_BODY)
                            .strong()
                            .color(p.text_primary),
                    );
                    ui.label(
                        RichText::new("置顶极简双行网速小卡片，带呼吸灯与实时上下行网速（可拖动、右键隐藏）")
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
                            ui.id().with(("floating_pill_seg", idx)),
                            Sense::click(),
                        );
                        let active = self.cfg.show_floating_pill == val;
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
                        if resp.clicked() && self.cfg.show_floating_pill != val {
                            self.cfg.show_floating_pill = val;
                            save_config(&self.cfg);
                            self.show_toast(if val {
                                "已开启桌面网速悬浮窗"
                            } else {
                                "已隐藏桌面网速悬浮窗"
                            });
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

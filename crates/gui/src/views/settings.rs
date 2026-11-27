use crate::icons;
use crate::logger;
use crate::storage::{PillLightStyle, TrayStyle, config_dir, save_config};
use crate::theme::{FONT_BODY, FONT_SM, FONT_XS, ThemeMode, ThemePalette, apply_theme};
use crate::widgets::{
    aligned_row, card_frame, elide_text, hairline_divider, pill, primary_btn, segmented_control,
    soft_btn, styled_input,
};
use crate::{ConnState, GameAccelApp, NavTab};
use eframe::egui::{self, Align, FontId, Layout, Margin, RichText, ScrollArea, Vec2};

impl GameAccelApp {
    pub(super) fn render_settings(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        p: ThemePalette,
    ) {
        ScrollArea::vertical()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                self.render_settings_inner(ui, ctx, p);
            });
    }

    fn render_settings_inner(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, p: ThemePalette) {
        let setting_card = |p: ThemePalette| card_frame(p).inner_margin(Margin::symmetric(18, 10));

        // Group 1: Network & Privilege
        ui.label(
            RichText::new("网络与权限")
                .size(FONT_XS)
                .color(p.text_muted),
        );
        ui.add_space(4.0);
        setting_card(p).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;

            aligned_row(ui, 34.0, |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(
                    RichText::new("本地混合端口")
                        .size(FONT_BODY)
                        .strong()
                        .color(p.text_primary),
                );
                pill(ui, "HTTP / SOCKS5", p.tag_bg, p.text_muted);

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_enabled_ui(self.state == ConnState::Disconnected, |ui| {
                        if styled_input(
                            ui,
                            &mut self.cfg.listen_addr,
                            "127.0.0.1:1080",
                            148.0,
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

            hairline_divider(ui, p);

            aligned_row(ui, 34.0, |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(
                    RichText::new("自定义分流规则")
                        .size(FONT_BODY)
                        .strong()
                        .color(p.text_primary),
                );
                let enabled_cnt = self.cfg.custom_rules.iter().filter(|r| r.enabled).count();
                let desc = if enabled_cnt > 0 {
                    format!("已启用 {enabled_cnt} 条（优先级高于内置规则）")
                } else {
                    "可针对慢站点单独指定「直连 / 国内节点 / 主节点」".to_string()
                };
                let max_desc_w = (ui.available_width() - 104.0).max(80.0);
                let shown_desc = elide_text(ui, &desc, FontId::proportional(FONT_SM), max_desc_w);
                ui.label(RichText::new(shown_desc).size(FONT_SM).color(p.text_muted))
                    .on_hover_text(desc);

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if crate::widgets::chip_btn(ui, "管理规则 ›", p, false).clicked() {
                        self.open_custom_rules_modal();
                    }
                });
            });

            hairline_divider(ui, p);

            aligned_row(ui, 34.0, |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(
                    RichText::new("运行权限")
                        .size(FONT_BODY)
                        .strong()
                        .color(p.text_primary),
                );
                let (status_txt, status_col) = if self.is_admin {
                    ("已获管理员权限（支持 TUN 全局网卡）", p.success_text)
                } else {
                    ("普通权限（启用 TUN 需提权）", p.text_muted)
                };
                ui.label(RichText::new(status_txt).size(FONT_SM).color(status_col));

                if !self.is_admin {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if primary_btn(ui, "管理员重启", p).clicked() {
                            self.restart_as_admin(false);
                        }
                    });
                }
            });
        });

        ui.add_space(12.0);

        // Group 2: Appearance & Interaction (Single-line rows + unified switches + progressive disclosure)
        ui.label(
            RichText::new("外观与交互")
                .size(FONT_XS)
                .color(p.text_muted),
        );
        ui.add_space(4.0);
        setting_card(p).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;

            // 2.1 Theme
            aligned_row(ui, 34.0, |ui| {
                ui.label(
                    RichText::new("界面主题")
                        .size(FONT_BODY)
                        .strong()
                        .color(p.text_primary),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(mode) = segmented_control(
                        ui,
                        "set_theme_seg",
                        self.cfg.theme_mode,
                        &[
                            (ThemeMode::Light, "浅色", None),
                            (ThemeMode::Dark, "深色", None),
                        ],
                        Vec2::new(124.0, 28.0),
                        FONT_SM,
                        p,
                    ) {
                        self.cfg.theme_mode = mode;
                        apply_theme(ctx, ThemePalette::from_mode(mode));
                        save_config(&self.cfg);
                    }
                });
            });

            hairline_divider(ui, p);

            // 2.2 Tray Right-Click Menu Style
            aligned_row(ui, 34.0, |ui| {
                ui.label(
                    RichText::new("托盘右键菜单")
                        .size(FONT_BODY)
                        .strong()
                        .color(p.text_primary),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(style) = segmented_control(
                        ui,
                        "set_tray_seg",
                        self.cfg.tray_style,
                        &[
                            (TrayStyle::Panel, "悬浮面板", None),
                            (TrayStyle::Menu, "原生菜单", None),
                        ],
                        Vec2::new(148.0, 28.0),
                        FONT_SM,
                        p,
                    ) {
                        self.cfg.tray_style = style;
                        save_config(&self.cfg);
                        if style == TrayStyle::Menu {
                            self.close_tray_popover();
                        }
                        if let Some(ref tray) = self.tray {
                            tray.set_style(style);
                        }
                        self.show_toast(match style {
                            TrayStyle::Panel => "已切换为托盘悬浮面板",
                            TrayStyle::Menu => "已切换为系统原生菜单",
                        });
                    }
                });
            });

            hairline_divider(ui, p);

            // 2.3 Desktop Floating Speed Pill + Progressive Disclosure of Breathing Light Style
            aligned_row(ui, 34.0, |ui| {
                ui.label(
                    RichText::new("桌面网速悬浮窗")
                        .size(FONT_BODY)
                        .strong()
                        .color(p.text_primary),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if icons::pill_toggle(ui, &mut self.cfg.show_floating_pill, p).changed() {
                        save_config(&self.cfg);
                        self.show_toast(if self.cfg.show_floating_pill {
                            "已开启桌面网速悬浮窗"
                        } else {
                            "已隐藏桌面网速悬浮窗"
                        });
                    }
                });
            });

            if self.cfg.show_floating_pill {
                aligned_row(ui, 32.0, |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.add_space(6.0);
                    // Crisp vector L-branch connector aligned optically to sub-label centerline
                    let (br, _) = ui.allocate_exact_size(Vec2::new(10.0, 18.0), egui::Sense::hover());
                    let s = egui::Stroke::new(1.25_f32, p.border_strong);
                    let x0 = br.left() + 2.0;
                    let cy = br.center().y;
                    ui.painter().line_segment(
                        [egui::Pos2::new(x0, br.top() + 1.0), egui::Pos2::new(x0, cy)],
                        s,
                    );
                    ui.painter().line_segment(
                        [egui::Pos2::new(x0, cy), egui::Pos2::new(br.right(), cy)],
                        s,
                    );
                    ui.label(
                        RichText::new("悬浮窗呼吸灯效")
                            .size(FONT_SM)
                            .color(p.text_secondary),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(style) = segmented_control(
                            ui,
                            "set_light_seg",
                            self.cfg.floating_pill_light,
                            &[
                                (PillLightStyle::Glow, "柔光", None),
                                (PillLightStyle::Ripple, "波纹", None),
                                (PillLightStyle::Orbit, "星轨", None),
                                (PillLightStyle::Bars, "律动", None),
                            ],
                            Vec2::new(228.0, 27.0),
                            FONT_XS,
                            p,
                        ) {
                            self.cfg.floating_pill_light = style;
                            save_config(&self.cfg);
                        }
                    });
                });
            }

            hairline_divider(ui, p);

            // 2.4 Server Admin Sidebar Toggle
            aligned_row(ui, 34.0, |ui| {
                ui.label(
                    RichText::new("显示服务端管理")
                        .size(FONT_BODY)
                        .strong()
                        .color(p.text_primary),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if icons::pill_toggle(ui, &mut self.cfg.show_admin_menu, p).changed() {
                        save_config(&self.cfg);
                        if !self.cfg.show_admin_menu && self.active_tab == NavTab::Admin {
                            self.active_tab = NavTab::Settings;
                        }
                        self.show_toast(if self.cfg.show_admin_menu {
                            "已显示「服务端」导航菜单"
                        } else {
                            "已隐藏「服务端」导航菜单"
                        });
                    }
                });
            });
        });

        ui.add_space(12.0);
        self.render_update_and_data_section(ui, ctx, p);
    }

    /// Group 3: Updates & Data / Logs.
    fn render_update_and_data_section(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        p: ThemePalette,
    ) {
        use crate::updater::{CURRENT_VERSION, UpdateStatus, is_installed_copy, open_url};

        ui.label(
            RichText::new("更新与数据")
                .size(FONT_XS)
                .color(p.text_muted),
        );
        ui.add_space(4.0);
        let status = self.updater.status();
        card_frame(p)
            .inner_margin(Margin::symmetric(18, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 6.0;

                // 3.1 Current Version & One-Click Update
                aligned_row(ui, 34.0, |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(
                        RichText::new(format!("当前版本 v{CURRENT_VERSION}"))
                            .size(FONT_BODY)
                            .strong()
                            .color(p.text_primary),
                    );

                    let (status_line, status_col) = match &status {
                        UpdateStatus::Idle => (String::new(), p.text_muted),
                        UpdateStatus::Checking => ("正在检查…".to_string(), p.text_muted),
                        UpdateStatus::UpToDate { checked_at } => {
                            (format!("已是最新 ({checked_at})"), p.text_muted)
                        }
                        UpdateStatus::Available(info) => {
                            (format!("发现新版本 v{}", info.version), p.success_text)
                        }
                        UpdateStatus::Downloading {
                            info,
                            downloaded,
                            total,
                        } => {
                            let mb = |b: u64| b as f64 / 1024.0 / 1024.0;
                            let line = match total {
                                Some(t) if *t > 0 => format!(
                                    "下载 v{} · {:.0}% ({:.1}/{:.1} MB)",
                                    info.version,
                                    *downloaded as f64 * 100.0 / *t as f64,
                                    mb(*downloaded),
                                    mb(*t)
                                ),
                                _ => format!("下载 v{} · {:.1} MB", info.version, mb(*downloaded)),
                            };
                            (line, p.text_muted)
                        }
                        UpdateStatus::Ready { info, .. } => {
                            (format!("v{} 已下载就绪", info.version), p.success_text)
                        }
                        UpdateStatus::Failed(msg) => (msg.clone(), p.danger_text),
                    };
                    if !status_line.is_empty() {
                        let max_w = (ui.available_width() - 180.0).max(80.0);
                        let shown =
                            elide_text(ui, &status_line, FontId::proportional(FONT_SM), max_w);
                        ui.label(RichText::new(shown).size(FONT_SM).color(status_col));
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        match &status {
                            UpdateStatus::Available(info) => {
                                if primary_btn(ui, "下载更新", p).clicked() {
                                    let rt = self.rt.clone();
                                    self.updater.download(&rt, ctx, info.clone());
                                }
                                if !info.html_url.is_empty()
                                    && soft_btn(ui, "更新说明", p).clicked()
                                {
                                    open_url(&info.html_url);
                                }
                            }
                            UpdateStatus::Ready { info, installer } => {
                                if primary_btn(ui, "安装并重启", p).clicked() {
                                    let installer = installer.clone();
                                    self.install_update(&installer);
                                }
                                if !info.html_url.is_empty()
                                    && soft_btn(ui, "更新说明", p).clicked()
                                {
                                    open_url(&info.html_url);
                                }
                            }
                            UpdateStatus::Checking | UpdateStatus::Downloading { .. } => {
                                ui.add_enabled_ui(false, |ui| soft_btn(ui, "请稍候…", p));
                            }
                            UpdateStatus::Idle
                            | UpdateStatus::UpToDate { .. }
                            | UpdateStatus::Failed(_) => {
                                if soft_btn(ui, "检查更新", p).clicked() {
                                    let rt = self.rt.clone();
                                    self.updater.check(&rt, ctx, false);
                                }
                            }
                        }
                    });
                });

                hairline_divider(ui, p);

                // 3.2 Auto Check Update (unified pill_toggle)
                aligned_row(ui, 34.0, |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(
                        RichText::new("自动检查更新")
                            .size(FONT_BODY)
                            .strong()
                            .color(p.text_primary),
                    );
                    if !is_installed_copy() {
                        ui.label(
                            RichText::new("非安装版升级时将安装至系统目录")
                                .size(FONT_SM)
                                .color(p.text_muted),
                        );
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icons::pill_toggle(ui, &mut self.cfg.auto_check_update, p).changed() {
                            save_config(&self.cfg);
                        }
                    });
                });

                hairline_divider(ui, p);

                // 3.3 Config & Logs Directory Shortcuts
                aligned_row(ui, 34.0, |ui| {
                    ui.label(
                        RichText::new("配置与运行日志")
                            .size(FONT_BODY)
                            .strong()
                            .color(p.text_primary),
                    );
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
            });
    }
}

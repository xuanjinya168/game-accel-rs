mod dashboard;
mod logs;
mod nodes;
mod settings;
pub(crate) mod tray_popover;

use crate::icons;
use crate::storage::save_config;
use crate::theme::{
    FONT_BODY, FONT_SM, FONT_SUB, FONT_TITLE, FONT_XS, ThemeMode, ThemePalette, apply_theme,
};
use crate::widgets::{draw_status_pill, hairline_divider};
use crate::{APP_VERSION, ConnState, GameAccelApp, NavTab};
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Layout, PointerButton, Pos2, RichText, Sense,
    Stroke, Vec2,
};

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

}

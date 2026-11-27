mod dashboard;
pub(crate) mod floating_pill;
mod logs;
mod nodes;
mod settings;
pub(crate) mod tray_popover;

use crate::icons;
use crate::storage::save_config;
use crate::theme::{FONT_XS, ThemeMode, ThemePalette, apply_theme, font_bold};
use crate::widgets::{aligned_row, draw_status_pill};
use crate::{APP_VERSION, ConnState, GameAccelApp, NavTab};
use eframe::egui::{
    self, Align, CornerRadius, FontId, Layout, PointerButton, Pos2, RichText, Sense, Stroke, Vec2,
};

impl GameAccelApp {
    /// Renders the 68px ultra-slim vertical icon rail matching `v03_cockpit_preview.html`:
    /// top Terracotta brand mark, 4 gamer tabs with clean surface card highlight (no orange stripe),
    /// and `运维` cleanly isolated at the bottom.
    pub(crate) fn render_sidebar_contents(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        p: ThemePalette,
    ) {
        ui.spacing_mut().item_spacing = Vec2::new(0.0, 6.0);

        // Brand Bolt Mark (36x36 centered inside 52px rail width, acts as frameless drag handle)
        let (brand_slot, brand_resp) =
            ui.allocate_exact_size(Vec2::new(52.0, 40.0), Sense::click_and_drag());
        if brand_resp.drag_started_by(PointerButton::Primary) {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if brand_resp.clicked() {
            self.brand_logo_pulse = 1.0;
            ctx.request_repaint();
        }
        let hover_t = ctx.animate_bool_responsive(brand_resp.id, brand_resp.hovered());
        let logo_rect = egui::Rect::from_center_size(brand_slot.center(), Vec2::splat(36.0));
        icons::draw_brand_logo(
            ui.painter(),
            logo_rect,
            p,
            icons::BrandLogoVisual {
                fill_t: self.brand_logo_fill_t,
                pulse: self.brand_logo_pulse,
                hover: hover_t,
            },
        );
        let status_hint = match self.state {
            ConnState::Connected => "隧道保护中",
            ConnState::Connecting => "连接协商中",
            ConnState::Disconnected => "待机就绪",
        };
        brand_resp.on_hover_text(format!("GameAccel {APP_VERSION} · {status_hint}"));

        ui.add_space(12.0);

        let gamer_items = [
            (NavTab::Dashboard, 0u8, "加速"),
            (NavTab::Nodes, 1u8, "线路"),
            (NavTab::Logs, 2u8, "日志"),
            (NavTab::Settings, 3u8, "设置"),
        ];

        for (tab, icon_kind, label) in gamer_items {
            self.draw_rail_item(ui, tab, icon_kind, label, p);
        }

        if self.cfg.show_admin_menu {
            let remaining = ui.available_height();
            if remaining > 58.0 {
                ui.add_space(remaining - 56.0);
            }
            let (div_rect, _) = ui.allocate_exact_size(Vec2::new(52.0, 6.0), Sense::hover());
            ui.painter().line_segment(
                [
                    Pos2::new(div_rect.center().x - 14.0, div_rect.center().y),
                    Pos2::new(div_rect.center().x + 14.0, div_rect.center().y),
                ],
                Stroke::new(1.0_f32, p.border_subtle),
            );
            self.draw_rail_item(ui, NavTab::Admin, 4u8, "运维", p);
        }
    }

    fn draw_rail_item(
        &mut self,
        ui: &mut egui::Ui,
        tab: NavTab,
        icon_kind: u8,
        label: &str,
        p: ThemePalette,
    ) {
        let active = self.active_tab == tab;
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(52.0, 46.0), Sense::click());
        let painter = ui.painter();

        if active {
            if p.card_shadow != egui::Shadow::NONE {
                painter.add(p.card_shadow.as_shape(rect, CornerRadius::same(10)));
            }
            painter.rect_filled(rect, CornerRadius::same(10), p.bg_nav_active);
            if p.is_dark {
                painter.rect_stroke(
                    rect,
                    CornerRadius::same(10),
                    Stroke::new(1.0_f32, p.border_subtle),
                    egui::StrokeKind::Inside,
                );
            }
        } else if resp.hovered() {
            painter.rect_filled(rect, CornerRadius::same(10), p.bg_nav_hover);
        }

        let fg = if active {
            p.text_primary
        } else if resp.hovered() {
            p.text_secondary
        } else {
            p.text_muted
        };
        icons::draw_nav_icon(
            painter,
            Pos2::new(rect.center().x, rect.top() + 15.5),
            icon_kind,
            fg,
        );

        let font = if active {
            font_bold(FONT_XS)
        } else {
            FontId::proportional(FONT_XS)
        };
        let galley = ui.fonts(|f| f.layout_no_wrap(label.to_string(), font, fg));
        painter.galley(
            Pos2::new(
                rect.center().x - galley.size().x * 0.5,
                rect.bottom() - 10.5 - galley.size().y * 0.5,
            ),
            galley,
            fg,
        );
        if resp.clicked() {
            self.active_tab = tab;
        }
    }

    pub(crate) fn render_right_workspace(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        p: ThemePalette,
    ) {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);

        // Slim 28px frameless studio topbar matching `v03_cockpit_preview.html`
        aligned_row(ui, 28.0, |ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let page_title = match self.active_tab {
                NavTab::Dashboard => "加速驾驶舱",
                NavTab::Nodes => "线路与订阅",
                NavTab::Logs => "诊断日志",
                NavTab::Admin => "服务端运维",
                NavTab::Settings => "偏好设置",
            };
            let title_resp = ui.add(
                egui::Label::new(
                    RichText::new(page_title)
                        .font(font_bold(16.0))
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

            // Soft recessed well port badge (`127.0.0.1:1080`, click to copy)
            let addr_str = self.cfg.listen_addr.clone();
            let port_galley = ui.fonts(|f| {
                f.layout_no_wrap(
                    addr_str.clone(),
                    FontId::monospace(11.5),
                    p.text_muted,
                )
            });
            let (p_rect, p_resp) = ui.allocate_exact_size(
                Vec2::new(port_galley.size().x + 16.0, 22.0),
                Sense::click(),
            );
            ui.painter().rect_filled(
                p_rect,
                CornerRadius::same(6),
                if p_resp.hovered() {
                    p.bg_nav_hover
                } else {
                    p.bg_card_alt
                },
            );
            ui.painter().galley(
                Pos2::new(
                    p_rect.center().x - port_galley.size().x * 0.5,
                    p_rect.center().y - port_galley.size().y * 0.5,
                ),
                port_galley,
                p.text_muted,
            );
            if p_resp.on_hover_text("点击复制本地混合代理地址").clicked() {
                ctx.copy_text(addr_str);
                self.show_toast("已复制本地代理地址");
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;

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
                        .rect_filled(x_rect, CornerRadius::same(7), p.bg_card_alt);
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
                        .rect_filled(m_rect, CornerRadius::same(7), p.bg_card_alt);
                }
                icons::draw_win_min_icon(ui.painter(), m_rect.center(), p.text_secondary);
                if m_resp.on_hover_text("最小化").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }

                // Uniform borderless Theme toggle (Sun / Moon) matching window controls
                let (t_rect, t_resp) =
                    ui.allocate_exact_size(Vec2::new(26.0, 26.0), Sense::click());
                if t_resp.hovered() {
                    ui.painter()
                        .rect_filled(t_rect, CornerRadius::same(7), p.bg_card_alt);
                }
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

                if self.active_tab != NavTab::Dashboard {
                    let reconnect = self.reconnect_status();
                    let pill_txt = match (self.state, reconnect) {
                        (ConnState::Connected, _) => "隧道保护中".to_string(),
                        (ConnState::Connecting, Some((attempt, 0))) => format!("重连 #{attempt}"),
                        (ConnState::Connecting, Some((attempt, secs))) => {
                            format!("重连 #{attempt} ({secs}s)")
                        }
                        (ConnState::Connecting, None) => "握手中…".to_string(),
                        (ConnState::Disconnected, _) => "待机".to_string(),
                    };
                    let (dot_col, bg, fg, stroke_col) = match self.state {
                        ConnState::Connected => (
                            p.success_text,
                            p.success_soft,
                            p.success_text,
                            p.success_text,
                        ),
                        ConnState::Connecting => {
                            (p.warn_text, p.warn_soft, p.warn_text, p.warn_text)
                        }
                        ConnState::Disconnected => {
                            (p.text_muted, p.bg_card, p.text_secondary, p.border_subtle)
                        }
                    };
                    draw_status_pill(ui, &pill_txt, dot_col, bg, fg, stroke_col);
                }

                // Remaining empty header area -> frameless window drag handle
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

        ui.add_space(6.0);

        let max_content_w = match self.active_tab {
            NavTab::Logs | NavTab::Admin => 1180.0,
            _ => 1040.0,
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

use crate::storage::save_config;
use crate::theme::ThemePalette;
use crate::tray::wake_window;
use crate::{ConnState, GameAccelApp};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Frame, PointerButton, Pos2, Sense, Stroke,
    StrokeKind,
};
use std::time::{Duration, Instant};

const CARD_W: f32 = 98.0;
const CARD_H: f32 = 44.0;

impl GameAccelApp {
    pub(crate) fn render_floating_pill(&mut self, ctx: &egui::Context, p: ThemePalette) {
        if !self.cfg.show_floating_pill {
            return;
        }

        let ppp = ctx.pixels_per_point().max(1.0);
        let init_pos = self
            .cfg
            .floating_pill_pos
            .map(|[x, y]| Pos2::new(x, y))
            .unwrap_or_else(|| default_card_pos(ppp));

        let mut open_main = false;
        let mut hide_card = false;
        let mut latest_outer_pos: Option<[f32; 2]> = None;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("game_accel_floating_pill"),
            egui::ViewportBuilder::default()
                .with_title("GameAccel 网速悬浮窗")
                .with_decorations(false)
                .with_always_on_top()
                .with_taskbar(false)
                .with_resizable(false)
                .with_inner_size([CARD_W, CARD_H])
                .with_position([init_pos.x, init_pos.y]),
            |vctx, _class| {
                // Apply native Windows 11 rounded corners + soft DWM drop shadow to all
                // top-level windows of this process (no brittle title lookup). Run for the
                // first few frames so it applies even if winit finishes showing the child
                // viewport after frame 0.
                let frames_id = egui::Id::new("floating_card_round_frames");
                let applied_frames = vctx.data(|d| d.get_temp::<u8>(frames_id).unwrap_or(0));
                if applied_frames < 5 {
                    crate::tray::apply_win11_rounded_corners();
                    vctx.data_mut(|d| d.insert_temp(frames_id, applied_frames + 1));
                }

                if vctx.input(|i| i.viewport().close_requested()) {
                    hide_card = true;
                }
                if let Some(outer) = vctx.input(|i| i.viewport().outer_rect) {
                    latest_outer_pos = Some([outer.min.x, outer.min.y]);
                }

                // Drive smooth breathing LED animation when active (20fps keeps CPU negligible).
                if self.state != ConnState::Disconnected {
                    vctx.request_repaint_after(Duration::from_millis(50));
                }

                let now_secs = vctx.input(|i| i.time);

                egui::CentralPanel::default()
                    .frame(Frame::new().fill(p.bg_card))
                    .show(vctx, |ui| {
                        let rect = ui.max_rect();
                        let resp = ui.interact(
                            rect,
                            ui.id().with("floating_card_body"),
                            Sense::click_and_drag(),
                        );

                        if resp.drag_started_by(PointerButton::Primary) {
                            vctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                        }
                        if resp.double_clicked_by(PointerButton::Primary) {
                            open_main = true;
                        }
                        if resp.clicked_by(PointerButton::Secondary) {
                            hide_card = true;
                        }

                        let painter = ui.painter();
                        painter.rect_filled(rect, CornerRadius::ZERO, p.bg_card);

                        // Subtle inner border matching Win11 native 8px rounded frame
                        let border_col = if resp.hovered() {
                            p.border_strong
                        } else {
                            p.border_subtle
                        };
                        painter.rect_stroke(
                            rect,
                            CornerRadius::same(8),
                            Stroke::new(1.0_f32, border_col),
                            StrokeKind::Inside,
                        );

                        let cy = rect.center().y;

                        // Semantic upload (warm amber) & download (fresh emerald) colors
                        let (up_col, down_col) = if p.is_dark {
                            (
                                Color32::from_rgb(245, 166, 86),
                                Color32::from_rgb(92, 214, 128),
                            )
                        } else {
                            (
                                Color32::from_rgb(217, 119, 36),
                                Color32::from_rgb(38, 152, 68),
                            )
                        };
                        let led_green = down_col;

                        // Left breathing status LED
                        let led_center = Pos2::new(rect.left() + 12.0, cy);
                        match self.state {
                            ConnState::Connected => {
                                let pulse = ((now_secs * 2.4).sin() * 0.5 + 0.5) as f32;
                                let halo_r = 3.6 + pulse * 3.4;
                                let halo_alpha = (26.0 + (1.0 - pulse) * 78.0) as u8;
                                let halo = Color32::from_rgba_unmultiplied(
                                    led_green.r(),
                                    led_green.g(),
                                    led_green.b(),
                                    halo_alpha,
                                );
                                painter.circle_filled(led_center, halo_r, halo);
                                painter.circle_filled(led_center, 3.0, led_green);
                            }
                            ConnState::Connecting => {
                                let pulse = ((now_secs * 4.8).sin() * 0.5 + 0.5) as f32;
                                let halo_r = 3.4 + pulse * 3.4;
                                let halo_alpha = (28.0 + (1.0 - pulse) * 92.0) as u8;
                                let halo = Color32::from_rgba_unmultiplied(
                                    up_col.r(),
                                    up_col.g(),
                                    up_col.b(),
                                    halo_alpha,
                                );
                                painter.circle_filled(led_center, halo_r, halo);
                                painter.circle_filled(led_center, 3.0, up_col);
                            }
                            ConnState::Disconnected => {
                                painter.circle_filled(led_center, 2.7, p.border_strong);
                            }
                        }

                        // Two crisp stacked rows: upload (top) & download (bottom)
                        let arrow_x = rect.left() + 24.5;
                        let text_x = rect.left() + 33.0;
                        let up_y = cy - 8.5;
                        let down_y = cy + 8.5;

                        let alpha_mul = if self.state == ConnState::Disconnected {
                            0.72
                        } else {
                            1.0
                        };
                        let up_tint = up_col.gamma_multiply(alpha_mul);
                        let down_tint = down_col.gamma_multiply(alpha_mul);

                        draw_arrow(painter, Pos2::new(arrow_x, up_y), true, up_tint);
                        painter.text(
                            Pos2::new(text_x, up_y),
                            Align2::LEFT_CENTER,
                            format_card_speed(self.tx_speed_bps),
                            FontId::proportional(12.5),
                            up_tint,
                        );

                        draw_arrow(painter, Pos2::new(arrow_x, down_y), false, down_tint);
                        painter.text(
                            Pos2::new(text_x, down_y),
                            Align2::LEFT_CENTER,
                            format_card_speed(self.rx_speed_bps),
                            FontId::proportional(12.5),
                            down_tint,
                        );
                    });
            },
        );

        // Debounced position persistence when the user drags the card
        if let Some([nx, ny]) = latest_outer_pos
            && nx > -4000.0
            && ny > -4000.0
        {
            let moved = match self.cfg.floating_pill_pos {
                Some([ox, oy]) => (nx - ox).abs() > 2.0 || (ny - oy).abs() > 2.0,
                None => true,
            };
            if moved {
                self.cfg.floating_pill_pos = Some([nx, ny]);
                self.floating_pill_pos_dirty = Some(Instant::now());
            }
        }
        if let Some(ts) = self.floating_pill_pos_dirty
            && ts.elapsed() >= Duration::from_millis(450)
        {
            self.floating_pill_pos_dirty = None;
            save_config(&self.cfg);
        }

        if open_main {
            self.window_hidden_to_tray = false;
            wake_window(ctx);
        }
        if hide_card {
            self.cfg.show_floating_pill = false;
            self.floating_pill_pos_dirty = None;
            save_config(&self.cfg);
            self.show_toast("已隐藏桌面网速悬浮窗");
        }
    }
}

/// Draw a crisp vector up/down arrow centered at `c`.
fn draw_arrow(painter: &egui::Painter, c: Pos2, up: bool, col: Color32) {
    let s = Stroke::new(1.35_f32, col);
    let half_h = 3.8;
    let wing = 2.6;
    if up {
        let top = Pos2::new(c.x, c.y - half_h);
        let bot = Pos2::new(c.x, c.y + half_h);
        painter.line_segment([bot, top], s);
        painter.line_segment([top, Pos2::new(c.x - wing, top.y + wing)], s);
        painter.line_segment([top, Pos2::new(c.x + wing, top.y + wing)], s);
    } else {
        let top = Pos2::new(c.x, c.y - half_h);
        let bot = Pos2::new(c.x, c.y + half_h);
        painter.line_segment([top, bot], s);
        painter.line_segment([bot, Pos2::new(c.x - wing, bot.y - wing)], s);
        painter.line_segment([bot, Pos2::new(c.x + wing, bot.y - wing)], s);
    }
}

fn format_card_speed(bytes_per_sec: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let v = bytes_per_sec.max(0.0);
    if v >= MB * 100.0 {
        format!("{:.0} MB/s", v / MB)
    } else if v >= MB * 10.0 {
        format!("{:.1} MB/s", v / MB)
    } else if v >= MB {
        format!("{:.2} MB/s", v / MB)
    } else if v >= KB * 100.0 {
        format!("{:.0} KB/s", v / KB)
    } else {
        format!("{:.1} KB/s", v / KB)
    }
}

fn default_card_pos(ppp: f32) -> Pos2 {
    #[cfg(target_os = "windows")]
    {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetSystemMetrics(n_index: i32) -> i32;
        }
        const SM_CXSCREEN: i32 = 0;
        let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        if screen_w > 0 {
            let logical_w = screen_w as f32 / ppp.max(1.0);
            let x = (logical_w - CARD_W - 28.0).max(20.0);
            return Pos2::new(x, 36.0);
        }
    }
    let _ = ppp;
    Pos2::new(960.0, 36.0)
}

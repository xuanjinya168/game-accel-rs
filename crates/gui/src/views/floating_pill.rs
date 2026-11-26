use crate::storage::{PillLightStyle, save_config};
use crate::theme::{FONT_SM, ThemePalette};
use crate::tray::{clamp_popover_position, wake_window};
use crate::widgets::hairline_divider;
use crate::{ConnState, GameAccelApp, NavTab};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Frame, Margin, PointerButton, Pos2, Sense, Stroke,
    StrokeKind, Vec2,
};
use std::time::{Duration, Instant};

const CARD_W: f32 = 98.0;
const CARD_H: f32 = 44.0;
const CARD_RADIUS: f32 = 6.0;

const MENU_W: f32 = 166.0;
const MENU_H: f32 = 236.0;
const MENU_RADIUS: f32 = 8.0;

/// Repaint interval for the floating card's breathing LED animation (~20fps).
pub(crate) const PILL_ANIM_INTERVAL: Duration = Duration::from_millis(50);

/// Map combined throughput to LED activity `0..=1` on a log scale:
/// ≤ 1 KB/s → 0 (calm), 10 MB/s and up → 1 (fast).
fn led_activity(bytes_per_sec: f64) -> f32 {
    let kb = bytes_per_sec.max(0.0) / 1024.0;
    if kb <= 1.0 {
        0.0
    } else {
        (kb.log10() / 4.0).clamp(0.0, 1.0) as f32
    }
}

impl GameAccelApp {
    /// LED repaint interval: bump to ~30fps when it's breathing fast so it stays smooth.
    pub(crate) fn pill_anim_interval(&self) -> Duration {
        if self.floating_pill_led_k > 0.5 {
            Duration::from_millis(33)
        } else {
            PILL_ANIM_INTERVAL
        }
    }
}

enum CardMenuAction {
    ToggleTopmost,
    ToggleFullscreenHide,
    ToggleEdgeHide,
    ToggleLock,
    ToggleConnect,
    OpenDashboard,
    CloseCard,
}

impl GameAccelApp {
    pub(crate) fn render_floating_pill(&mut self, ctx: &egui::Context, p: ThemePalette) {
        if !self.cfg.show_floating_pill {
            self.floating_menu_open = false;
            self.floating_pill_fs_hidden = false;
            self.floating_pill_edge_tucked = false;
            self.floating_pill_spawn_pos = None;
            set_taskbar_guard(false);
            return;
        }

        // Poll fullscreen state every 500ms when enabled
        if self.cfg.floating_pill_hide_fullscreen {
            if self.floating_pill_last_fs_check.elapsed() >= Duration::from_millis(500) {
                self.floating_pill_last_fs_check = Instant::now();
                let fs = is_another_app_fullscreen();
                if fs != self.floating_pill_fs_hidden {
                    self.floating_pill_fs_hidden = fs;
                    if fs {
                        self.floating_menu_open = false;
                    } else {
                        self.floating_pill_raise_frames = 6;
                    }
                }
            }
        } else if self.floating_pill_fs_hidden {
            self.floating_pill_fs_hidden = false;
            self.floating_pill_raise_frames = 6;
        }

        let ppp = ctx.pixels_per_point().max(1.0);
        // Only used when the window is (re)created; must stay constant afterwards.
        let saved_pos = self.cfg.floating_pill_pos;
        let init_pos = *self.floating_pill_spawn_pos.get_or_insert_with(|| {
            saved_pos
                .map(|[x, y]| Pos2::new(x, y))
                .unwrap_or_else(|| default_card_pos(ppp))
        });

        let now_secs = ctx.input(|i| i.time);

        // Speed-reactive breathing LED. Advance phases by `dt * rate` instead of computing
        // `sin(t * rate)`, so a changing rate speeds the LED up/down smoothly with no jump.
        let dt = (now_secs - self.floating_pill_led_last).clamp(0.0, 0.25);
        self.floating_pill_led_last = now_secs;
        let target_k = led_activity(self.telemetry.tx_speed_bps + self.telemetry.rx_speed_bps);
        // Ease activity over ~0.6s so the 1s speed samples don't make the rate step.
        let ease = 1.0 - (-dt / 0.6).exp();
        self.floating_pill_led_k += (target_k - self.floating_pill_led_k) * ease as f32;
        let activity = self.floating_pill_led_k;
        let period = if self.state == ConnState::Connecting {
            1.15
        } else {
            3.0 - 1.5 * f64::from(activity)
        };
        self.floating_pill_led_phase = (self.floating_pill_led_phase + dt / period).fract();
        let phase = self.floating_pill_led_phase as f32;
        let anim_interval = self.pill_anim_interval();
        let force_raise = if self.floating_pill_raise_frames > 0 {
            self.floating_pill_raise_frames -= 1;
            ctx.request_repaint_after(Duration::from_millis(33));
            true
        } else {
            false
        };

        let mut open_main = false;
        let mut open_menu_at: Option<(f64, f64)> = None;
        let mut latest_outer_pos: Option<[f32; 2]> = None;
        let mut is_hovered = false;
        let mut is_dragging = false;

        let always_on_top = self.cfg.floating_pill_always_on_top;
        let locked = self.cfg.floating_pill_locked;
        let fs_hidden = self.floating_pill_fs_hidden;

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
                let vppp = vctx.pixels_per_point().max(1.0);
                set_taskbar_guard(always_on_top && !fs_hidden);
                ensure_floating_card_window(vppp, always_on_top, fs_hidden, force_raise);

                if let Some(outer) = vctx.input(|i| i.viewport().outer_rect) {
                    latest_outer_pos = Some([outer.min.x, outer.min.y]);
                }

                // Drive the breathing LED animation when visible & active (20fps, 30fps when
                // the LED is breathing fast under heavy traffic).
                if !fs_hidden && self.state != ConnState::Disconnected {
                    vctx.request_repaint_after(anim_interval);
                }

                egui::CentralPanel::default()
                    .frame(Frame::new().fill(p.bg_card))
                    .show(vctx, |ui| {
                        let rect = ui.max_rect();
                        let resp = ui.interact(
                            rect,
                            ui.id().with("floating_card_body"),
                            Sense::click_and_drag(),
                        );
                        is_hovered = resp.hovered() || vctx.input(|i| i.pointer.has_pointer());
                        is_dragging = resp.dragged();

                        if !locked && resp.drag_started_by(PointerButton::Primary) {
                            self.floating_menu_open = false;
                            vctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                        }
                        if resp.double_clicked_by(PointerButton::Primary) {
                            self.floating_menu_open = false;
                            open_main = true;
                        }
                        if resp.clicked_by(PointerButton::Secondary) {
                            if let Some((cx, cy)) = win32_cursor_pos() {
                                open_menu_at = Some((cx, cy));
                            } else if let Some(outer) = vctx.input(|i| i.viewport().outer_rect) {
                                open_menu_at = Some((
                                    (outer.right() * vppp) as f64,
                                    (outer.bottom() * vppp) as f64,
                                ));
                            }
                        }

                        let painter = ui.painter();
                        painter.rect_filled(rect, CornerRadius::ZERO, p.bg_card);

                        let border_col = if resp.hovered() {
                            p.border_strong
                        } else {
                            p.border_subtle
                        };
                        painter.rect_stroke(
                            rect,
                            CornerRadius::same(CARD_RADIUS as u8),
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

                        // Left breathing status LED
                        let led_center = Pos2::new(rect.left() + 13.0, cy);
                        if self.state == ConnState::Disconnected {
                            painter.circle_filled(led_center, 2.7, p.border_strong);
                        } else {
                            let connecting = self.state == ConnState::Connecting;
                            draw_light(
                                painter,
                                led_center,
                                self.cfg.floating_pill_light,
                                phase,
                                if connecting { 0.3 } else { activity },
                                if connecting { up_col } else { down_col },
                            );
                        }

                        // Two crisp stacked rows: upload (top) & download (bottom)
                        let arrow_x = rect.left() + 25.5;
                        let text_x = rect.left() + 33.5;
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
                            format_card_speed(self.telemetry.tx_speed_bps),
                            FontId::proportional(12.5),
                            up_tint,
                        );

                        draw_arrow(painter, Pos2::new(arrow_x, down_y), false, down_tint);
                        painter.text(
                            Pos2::new(text_x, down_y),
                            Align2::LEFT_CENTER,
                            format_card_speed(self.telemetry.rx_speed_bps),
                            FontId::proportional(12.5),
                            down_tint,
                        );
                    });
            },
        );

        // Edge auto-hide (dock & tuck to left/right/top screen border)
        if is_hovered || is_dragging || self.floating_menu_open {
            self.floating_pill_last_hover = Instant::now();
        }
        // Set when we move the window ourselves this frame: `latest_outer_pos` was sampled
        // before the move and is stale (e.g. still the tucked position), so don't persist it.
        let mut moved_this_frame = false;
        if !fs_hidden {
            if self.cfg.floating_pill_edge_hide {
                let want_tuck = !is_hovered
                    && !is_dragging
                    && !self.floating_menu_open
                    && self.floating_pill_last_hover.elapsed() >= Duration::from_millis(420);
                if want_tuck && !self.floating_pill_edge_tucked {
                    if let Some([tx, ty]) = compute_tucked_pos(ppp) {
                        move_floating_card_physical(ppp, tx, ty);
                        self.floating_pill_edge_tucked = true;
                        moved_this_frame = true;
                    }
                } else if !want_tuck && self.floating_pill_edge_tucked {
                    if let Some([ux, uy]) = compute_untucked_pos(ppp) {
                        move_floating_card_physical(ppp, ux, uy);
                    }
                    self.floating_pill_edge_tucked = false;
                    moved_this_frame = true;
                }
            } else if self.floating_pill_edge_tucked {
                if let Some([ux, uy]) = compute_untucked_pos(ppp) {
                    move_floating_card_physical(ppp, ux, uy);
                }
                self.floating_pill_edge_tucked = false;
                moved_this_frame = true;
            }
        }

        // Debounced position persistence (skip while edge-tucked so docked anchor stays clean)
        if !self.floating_pill_edge_tucked
            && !moved_this_frame
            && let Some([nx, ny]) = latest_outer_pos
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
            self.floating_pill_raise_frames = 8;
        }

        if let Some((px, py)) = open_menu_at {
            self.floating_menu_pos = clamp_popover_position(px, py, MENU_W, MENU_H, ppp);
            self.floating_menu_just_opened = true;
            self.floating_menu_ever_focused = false;
            self.floating_menu_open = true;
            ctx.request_repaint();
        }

        self.render_floating_card_menu(ctx, p);
    }

    fn render_floating_card_menu(&mut self, ctx: &egui::Context, p: ThemePalette) {
        if !self.floating_menu_open {
            return;
        }

        let mut close_menu = false;
        let mut picked: Option<CardMenuAction> = None;

        let topmost = self.cfg.floating_pill_always_on_top;
        let fs_hide = self.cfg.floating_pill_hide_fullscreen;
        let edge_hide = self.cfg.floating_pill_edge_hide;
        let locked = self.cfg.floating_pill_locked;
        let conn_label = match self.state {
            ConnState::Disconnected => "开启加速",
            ConnState::Connecting => "取消连接",
            ConnState::Connected => "断开加速",
        };

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("game_accel_floating_card_menu"),
            egui::ViewportBuilder::default()
                .with_title("GameAccel 悬浮窗菜单")
                .with_decorations(false)
                .with_always_on_top()
                .with_taskbar(false)
                .with_resizable(false)
                .with_inner_size([MENU_W, MENU_H])
                .with_position([self.floating_menu_pos.x, self.floating_menu_pos.y]),
            |vctx, _class| {
                let vppp = vctx.pixels_per_point().max(1.0);
                ensure_popup_menu_region(vppp);

                if self.floating_menu_just_opened {
                    vctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    self.floating_menu_just_opened = false;
                }

                let focused = vctx.input(|i| i.viewport().focused);
                if focused == Some(true) {
                    self.floating_menu_ever_focused = true;
                } else if self.floating_menu_ever_focused && focused == Some(false) {
                    close_menu = true;
                }
                if vctx
                    .input(|i| i.key_pressed(egui::Key::Escape) || i.viewport().close_requested())
                {
                    close_menu = true;
                }

                egui::CentralPanel::default()
                    .frame(
                        Frame::new()
                            .fill(p.bg_card)
                            .inner_margin(Margin::symmetric(6, 7)),
                    )
                    .show(vctx, |ui| {
                        ui.spacing_mut().item_spacing = Vec2::new(0.0, 2.0);

                        if menu_row(ui, "窗口置顶显示", Some(topmost), false, p).clicked() {
                            picked = Some(CardMenuAction::ToggleTopmost);
                            close_menu = true;
                        }
                        if menu_row(ui, "贴边自动隐藏", Some(edge_hide), false, p).clicked() {
                            picked = Some(CardMenuAction::ToggleEdgeHide);
                            close_menu = true;
                        }
                        if menu_row(ui, "全屏时自动隐藏", Some(fs_hide), false, p).clicked()
                        {
                            picked = Some(CardMenuAction::ToggleFullscreenHide);
                            close_menu = true;
                        }
                        if menu_row(ui, "锁定悬浮窗位置", Some(locked), false, p).clicked() {
                            picked = Some(CardMenuAction::ToggleLock);
                            close_menu = true;
                        }

                        ui.add_space(4.0);
                        hairline_divider(ui, p);
                        ui.add_space(4.0);

                        if menu_row(ui, conn_label, None, false, p).clicked() {
                            picked = Some(CardMenuAction::ToggleConnect);
                            close_menu = true;
                        }
                        if menu_row(ui, "查看流量详情", None, false, p).clicked() {
                            picked = Some(CardMenuAction::OpenDashboard);
                            close_menu = true;
                        }
                        if menu_row(ui, "关闭流量悬浮窗", None, true, p).clicked() {
                            picked = Some(CardMenuAction::CloseCard);
                            close_menu = true;
                        }

                        // Paint crisp 1px rounded border flush inside the viewport bounds so
                        // content height can never push the bottom stroke off-screen.
                        vctx.layer_painter(egui::LayerId::new(
                            egui::Order::Foreground,
                            egui::Id::new("floating_menu_border"),
                        ))
                        .rect_stroke(
                            vctx.screen_rect(),
                            CornerRadius::same(MENU_RADIUS as u8),
                            Stroke::new(1.0_f32, p.border_strong),
                            StrokeKind::Inside,
                        );
                    });
            },
        );

        if close_menu {
            self.floating_menu_open = false;
            self.floating_menu_ever_focused = false;
        }

        if let Some(act) = picked {
            match act {
                CardMenuAction::ToggleTopmost => {
                    self.cfg.floating_pill_always_on_top = !self.cfg.floating_pill_always_on_top;
                    save_config(&self.cfg);
                    self.floating_pill_raise_frames = 4;
                }
                CardMenuAction::ToggleFullscreenHide => {
                    self.cfg.floating_pill_hide_fullscreen =
                        !self.cfg.floating_pill_hide_fullscreen;
                    if !self.cfg.floating_pill_hide_fullscreen {
                        self.floating_pill_fs_hidden = false;
                    }
                    save_config(&self.cfg);
                }
                CardMenuAction::ToggleEdgeHide => {
                    self.cfg.floating_pill_edge_hide = !self.cfg.floating_pill_edge_hide;
                    save_config(&self.cfg);
                }
                CardMenuAction::ToggleLock => {
                    self.cfg.floating_pill_locked = !self.cfg.floating_pill_locked;
                    save_config(&self.cfg);
                }
                CardMenuAction::ToggleConnect => match self.state {
                    ConnState::Disconnected => self.start_connect(),
                    ConnState::Connecting | ConnState::Connected => self.stop_connect(),
                },
                CardMenuAction::OpenDashboard => {
                    self.active_tab = NavTab::Dashboard;
                    self.window_hidden_to_tray = false;
                    wake_window(ctx);
                    self.floating_pill_raise_frames = 8;
                }
                CardMenuAction::CloseCard => {
                    self.cfg.show_floating_pill = false;
                    self.floating_pill_pos_dirty = None;
                    self.floating_pill_edge_tucked = false;
                    save_config(&self.cfg);
                    self.show_toast("已隐藏桌面网速悬浮窗");
                }
            }
        }
    }
}

fn menu_row(
    ui: &mut egui::Ui,
    label: &str,
    checked: Option<bool>,
    is_danger: bool,
    p: ThemePalette,
) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 28.0), Sense::click());
    if resp.hovered() {
        let fill = if is_danger {
            p.danger_soft
        } else {
            p.bg_nav_hover
        };
        ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
    }

    if checked == Some(true) {
        // Crisp warm-amber checkmark matching reference style
        let cx = rect.left() + 14.0;
        let cy = rect.center().y;
        let check_col = if p.is_dark {
            Color32::from_rgb(245, 166, 86)
        } else {
            Color32::from_rgb(217, 119, 36)
        };
        let s = Stroke::new(1.6_f32, check_col);
        let p1 = Pos2::new(cx - 4.0, cy - 0.2);
        let p2 = Pos2::new(cx - 1.2, cy + 2.8);
        let p3 = Pos2::new(cx + 4.5, cy - 3.2);
        ui.painter().line_segment([p1, p2], s);
        ui.painter().line_segment([p2, p3], s);
    }

    let text_col = if is_danger && resp.hovered() {
        p.danger_text
    } else if is_danger {
        p.text_secondary
    } else {
        p.text_primary
    };
    ui.painter().text(
        Pos2::new(rect.left() + 28.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(FONT_SM),
        text_col,
    );
    resp
}

fn draw_light(
    painter: &egui::Painter,
    center: Pos2,
    style: PillLightStyle,
    phase: f32,
    activity: f32,
    color: Color32,
) {
    use std::f32::consts::{PI, TAU};
    let angle = phase * TAU;
    match style {
        PillLightStyle::Glow => {
            let radius = 6.0 + 1.5 * (1.0 - angle.cos());
            for layer in (1..=8).rev() {
                painter.circle_filled(
                    center,
                    radius * layer as f32 / 8.0,
                    color.gamma_multiply(0.085),
                );
            }
            painter.circle_filled(center, 2.7, color);
        }
        PillLightStyle::Ripple => {
            let tint = color.gamma_multiply(0.68 * (PI * phase).sin());
            painter.circle_stroke(center, 3.5 + 5.0 * phase, Stroke::new(1.0_f32, tint));
            painter.circle_filled(center, 2.7, color);
        }
        PillLightStyle::Orbit => {
            painter.circle_stroke(center, 6.0, Stroke::new(1.2_f32, color.gamma_multiply(0.19)));
            let points = (0..=10)
                .map(|i| center + Vec2::angled(angle + 1.7 * i as f32 / 10.0) * 6.0)
                .collect();
            painter.add(egui::Shape::line(points, Stroke::new(1.7_f32, color)));
            painter.circle_filled(center, 1.7, color);
        }
        PillLightStyle::Bars => {
            for i in 0..3 {
                let wave = (1.0 + (angle + i as f32 * 1.4).sin()) * 0.5;
                let height = 3.0 + (3.0 + 4.0 * activity) * wave;
                let pos = center + Vec2::new(4.0 * (i as f32 - 1.0), 0.0);
                painter.rect_filled(
                    egui::Rect::from_center_size(pos, Vec2::new(2.4, height)),
                    1,
                    color,
                );
            }
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

fn win32_cursor_pos() -> Option<(f64, f64)> {
    #[cfg(target_os = "windows")]
    {
        #[repr(C)]
        struct Point {
            x: i32,
            y: i32,
        }
        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetCursorPos(lp_point: *mut Point) -> i32;
        }
        let mut pt = Point { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut pt) } != 0 {
            return Some((pt.x as f64, pt.y as f64));
        }
    }
    None
}

/// Cached HWNDs so we don't `EnumWindows` over every top-level window on each frame.
#[cfg(target_os = "windows")]
static CARD_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
#[cfg(target_os = "windows")]
static MENU_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// Whether the taskbar guard should keep the card above the taskbar.
#[cfg(target_os = "windows")]
static KEEP_ABOVE_TASKBAR: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn set_taskbar_guard(enabled: bool) {
    #[cfg(target_os = "windows")]
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        KEEP_ABOVE_TASKBAR.store(enabled, Ordering::Relaxed);
        if !enabled {
            return;
        }
        static INSTALLED: AtomicBool = AtomicBool::new(false);
        if INSTALLED.swap(true, Ordering::Relaxed) {
            return;
        }
        type WinEventProc = unsafe extern "system" fn(isize, u32, isize, i32, i32, u32, u32);
        #[link(name = "user32")]
        unsafe extern "system" {
            fn SetWinEventHook(
                event_min: u32,
                event_max: u32,
                hmod: isize,
                callback: WinEventProc,
                pid: u32,
                tid: u32,
                flags: u32,
            ) -> isize;
        }
        const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
        const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;
        const WINEVENT_SKIPOWNPROCESS: u32 = 0x0002;

        /// The taskbar is itself a topmost window: when it's activated (e.g. clicking a tray
        /// icon) it jumps above a card placed over it. Re-assert our z-order right away instead
        /// of waiting for the next frame / tray poll, which showed up as a visible blink.
        unsafe extern "system" fn on_foreground(
            _hook: isize,
            _event: u32,
            hwnd: isize,
            _id_object: i32,
            _id_child: i32,
            _thread: u32,
            _time: u32,
        ) {
            if KEEP_ABOVE_TASKBAR.load(Ordering::Relaxed) && is_taskbar_window(hwnd) {
                raise_card_topmost();
            }
        }

        // Out-of-context hooks are delivered through this (UI) thread's message loop.
        unsafe {
            let _ = SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                0,
                on_foreground,
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = enabled;
    }
}

#[cfg(target_os = "windows")]
fn is_taskbar_window(hwnd: isize) -> bool {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetAncestor(hwnd: *mut std::ffi::c_void, flags: u32) -> *mut std::ffi::c_void;
        fn GetClassNameW(hwnd: *mut std::ffi::c_void, buf: *mut u16, max_count: i32) -> i32;
    }
    const GA_ROOT: u32 = 2;
    if hwnd == 0 {
        return false;
    }
    unsafe {
        let hwnd = hwnd as *mut std::ffi::c_void;
        let root = GetAncestor(hwnd, GA_ROOT);
        let root = if !root.is_null() { root } else { hwnd };
        let mut buf = [0u16; 32];
        let n = GetClassNameW(root, buf.as_mut_ptr(), buf.len() as i32);
        if n <= 0 {
            return false;
        }
        let cls = String::from_utf16_lossy(&buf[..n as usize]);
        cls == "Shell_TrayWnd" || cls == "Shell_SecondaryTrayWnd"
    }
}

#[cfg(target_os = "windows")]
fn raise_card_topmost() {
    use std::sync::atomic::Ordering;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn IsWindowVisible(hwnd: *mut std::ffi::c_void) -> i32;
        fn SetWindowPos(
            hwnd: isize,
            insert_after: isize,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
    }
    const HWND_TOPMOST: isize = -1;
    const SWP_NOSIZE: u32 = 0x0001;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOACTIVATE: u32 = 0x0010;
    let hwnd = CARD_HWND.load(Ordering::Relaxed);
    if hwnd == 0 {
        return;
    }
    unsafe {
        if IsWindowVisible(hwnd as *mut std::ffi::c_void) != 0 {
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

#[cfg(target_os = "windows")]
fn find_own_window_by_size(
    cache: &std::sync::atomic::AtomicIsize,
    exp_w: i32,
    exp_h: i32,
) -> Option<(isize, i32, i32, i32, i32)> {
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;
    type Hwnd = *mut c_void;
    type EnumProc = unsafe extern "system" fn(Hwnd, isize) -> i32;

    #[repr(C)]
    struct WinRect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    struct FindCtx {
        pid: u32,
        exp_w: i32,
        exp_h: i32,
        found: Option<(isize, i32, i32, i32, i32)>,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcessId() -> u32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumWindows(cb: EnumProc, lparam: isize) -> i32;
        fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
        fn GetWindowRect(hwnd: isize, rect: *mut WinRect) -> i32;
    }

    /// Returns `(x, y, w, h)` if `hwnd` belongs to `pid` and roughly has the expected size.
    unsafe fn probe(hwnd: Hwnd, pid: u32, exp_w: i32, exp_h: i32) -> Option<(i32, i32, i32, i32)> {
        unsafe {
            let mut wpid: u32 = 0;
            if GetWindowThreadProcessId(hwnd, &mut wpid) == 0 || wpid != pid {
                return None;
            }
            let mut rc = WinRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetWindowRect(hwnd as isize, &mut rc) == 0 {
                return None;
            }
            let w = rc.right - rc.left;
            let h = rc.bottom - rc.top;
            ((w - exp_w).abs() <= 24 && (h - exp_h).abs() <= 24).then_some((rc.left, rc.top, w, h))
        }
    }

    unsafe extern "system" fn enum_cb(hwnd: Hwnd, lparam: isize) -> i32 {
        unsafe {
            let ctx = &mut *(lparam as *mut FindCtx);
            if let Some((x, y, w, h)) = probe(hwnd, ctx.pid, ctx.exp_w, ctx.exp_h) {
                ctx.found = Some((hwnd as isize, x, y, w, h));
                return 0;
            }
        }
        1
    }

    unsafe {
        let pid = GetCurrentProcessId();
        let cached = cache.load(Ordering::Relaxed);
        if cached != 0 {
            if let Some((x, y, w, h)) = probe(cached as Hwnd, pid, exp_w, exp_h) {
                return Some((cached, x, y, w, h));
            }
            cache.store(0, Ordering::Relaxed);
        }

        let mut fctx = FindCtx {
            pid,
            exp_w,
            exp_h,
            found: None,
        };
        let _ = EnumWindows(enum_cb, (&mut fctx as *mut FindCtx) as isize);
        if let Some((hwnd, ..)) = fctx.found {
            cache.store(hwnd, Ordering::Relaxed);
        }
        fctx.found
    }
}

/// Keep the floating card detached from the main window owner, respect topmost/fullscreen
/// visibility settings, and clip its Win32 region to an exact rounded rectangle.
fn ensure_floating_card_window(ppp: f32, always_on_top: bool, fs_hidden: bool, force_raise: bool) {
    #[cfg(target_os = "windows")]
    {
        use std::ffi::c_void;
        type Hwnd = *mut c_void;

        #[repr(C)]
        struct WinRect {
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
        }

        #[link(name = "user32")]
        unsafe extern "system" {
            fn MonitorFromWindow(hwnd: Hwnd, flags: u32) -> isize;
            fn GetAsyncKeyState(key: i32) -> i16;
            fn IsWindowVisible(hwnd: Hwnd) -> i32;
            fn GetForegroundWindow() -> Hwnd;
            fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
            fn SetWindowLongPtrW(hwnd: isize, index: i32, new_long: isize) -> isize;
            fn GetWindowRgnBox(hwnd: isize, lprc: *mut WinRect) -> i32;
            fn SetWindowRgn(hwnd: isize, hrgn: isize, redraw: i32) -> i32;
            fn ShowWindow(hwnd: isize, n_cmd_show: i32) -> i32;
            fn SetWindowPos(
                hwnd: isize,
                insert_after: isize,
                x: i32,
                y: i32,
                cx: i32,
                cy: i32,
                flags: u32,
            ) -> i32;
        }
        #[link(name = "gdi32")]
        unsafe extern "system" {
            fn CreateRoundRectRgn(x1: i32, y1: i32, x2: i32, y2: i32, w: i32, h: i32) -> isize;
        }

        let Some((hwnd, x, y, w, h)) = find_own_window_by_size(
            &CARD_HWND,
            (CARD_W * ppp).round() as i32,
            (CARD_H * ppp).round() as i32,
        ) else {
            return;
        };
        if w <= 0 || h <= 0 {
            return;
        }

        unsafe {
            // Detach owner so hiding/restoring the main window never flickers the floating card.
            const GWLP_HWNDPARENT: i32 = -8;
            if GetWindowLongPtrW(hwnd, GWLP_HWNDPARENT) != 0 {
                let _ = SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, 0);
            }

            const SW_HIDE: i32 = 0;
            const SW_SHOWNOACTIVATE: i32 = 4;
            const GWL_EXSTYLE: i32 = -20;
            const WS_EX_TOPMOST: isize = 0x0000_0008;
            const HWND_TOPMOST: isize = -1;
            const HWND_NOTOPMOST: isize = -2;
            const SWP_NOSIZE: u32 = 0x0001;
            const SWP_NOMOVE: u32 = 0x0002;
            const SWP_NOACTIVATE: u32 = 0x0010;
            const SWP_SHOWWINDOW: u32 = 0x0040;

            const SWP_NOZORDER: u32 = 0x0004;
            let visible = IsWindowVisible(hwnd as Hwnd) != 0;
            if fs_hidden {
                if visible {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                return;
            }

            // A saved position can be entirely off-screen after a monitor is unplugged.
            // Keep the intentional edge-hide strip and allow dragging between monitors.
            if MonitorFromWindow(hwnd as Hwnd, 0) == 0
                && GetAsyncKeyState(1) >= 0
                && let Some((left, top, right, bottom)) = monitor_work_rect_for_hwnd(hwnd)
            {
                let _ = SetWindowPos(
                    hwnd,
                    0,
                    x.clamp(left, (right - w).max(left)),
                    y.clamp(top, (bottom - h).max(top)),
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }

            let cur_topmost = (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST) != 0;
            // Fallback for the taskbar guard hook: while the taskbar is the foreground window,
            // keep re-asserting our position above it.
            let taskbar_active = always_on_top && is_taskbar_window(GetForegroundWindow() as isize);
            if !visible
                || cur_topmost != always_on_top
                || (force_raise && always_on_top)
                || taskbar_active
            {
                if !visible {
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                }
                let insert_after = if always_on_top {
                    HWND_TOPMOST
                } else {
                    HWND_NOTOPMOST
                };
                // Only pass SWP_SHOWWINDOW when actually hidden; re-"showing" an already
                // visible window on every raise frame can make it blink.
                let show_flag = if visible { 0 } else { SWP_SHOWWINDOW };
                let _ = SetWindowPos(
                    hwnd,
                    insert_after,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | show_flag,
                );
            }

            let mut rgn_rc = WinRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            let rgn_type = GetWindowRgnBox(hwnd, &mut rgn_rc);
            if rgn_type == 0
                || (rgn_rc.right - rgn_rc.left - w).abs() > 2
                || (rgn_rc.bottom - rgn_rc.top - h).abs() > 2
            {
                let diam = (CARD_RADIUS * 2.0 * ppp).round().max(8.0) as i32;
                let hrgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, diam, diam);
                if hrgn != 0 {
                    let _ = SetWindowRgn(hwnd, hrgn, 1);
                }
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (ppp, always_on_top, fs_hidden, force_raise);
    }
}

fn ensure_popup_menu_region(ppp: f32) {
    #[cfg(target_os = "windows")]
    {
        #[repr(C)]
        struct WinRect {
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
        }
        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetWindowRgnBox(hwnd: isize, lprc: *mut WinRect) -> i32;
            fn SetWindowRgn(hwnd: isize, hrgn: isize, redraw: i32) -> i32;
        }
        #[link(name = "gdi32")]
        unsafe extern "system" {
            fn CreateRoundRectRgn(x1: i32, y1: i32, x2: i32, y2: i32, w: i32, h: i32) -> isize;
        }

        let Some((hwnd, _x, _y, w, h)) = find_own_window_by_size(
            &MENU_HWND,
            (MENU_W * ppp).round() as i32,
            (MENU_H * ppp).round() as i32,
        ) else {
            return;
        };
        if w <= 0 || h <= 0 {
            return;
        }
        unsafe {
            let mut rgn_rc = WinRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            let rgn_type = GetWindowRgnBox(hwnd, &mut rgn_rc);
            if rgn_type == 0
                || (rgn_rc.right - rgn_rc.left - w).abs() > 2
                || (rgn_rc.bottom - rgn_rc.top - h).abs() > 2
            {
                let diam = (MENU_RADIUS * 2.0 * ppp).round().max(10.0) as i32;
                let hrgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, diam, diam);
                if hrgn != 0 {
                    let _ = SetWindowRgn(hwnd, hrgn, 1);
                }
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = ppp;
    }
}

/// Detect whether another non-desktop foreground window covers its entire monitor.
fn is_another_app_fullscreen() -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::ffi::c_void;
        type Hwnd = *mut c_void;

        #[repr(C)]
        struct WinRect {
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
        }
        #[repr(C)]
        struct MonitorInfo {
            cb_size: u32,
            rc_monitor: WinRect,
            rc_work: WinRect,
            dw_flags: u32,
        }

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcessId() -> u32;
        }
        #[link(name = "shell32")]
        unsafe extern "system" {
            fn SHQueryUserNotificationState(state: *mut i32) -> i32;
        }
        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetForegroundWindow() -> Hwnd;
            fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
            fn GetClassNameW(hwnd: Hwnd, buf: *mut u16, max_count: i32) -> i32;
            fn GetWindowRect(hwnd: isize, rect: *mut WinRect) -> i32;
            fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
            fn IsZoomed(hwnd: Hwnd) -> i32;
            fn MonitorFromWindow(hwnd: Hwnd, dw_flags: u32) -> isize;
            fn GetMonitorInfoW(hmonitor: isize, lpmi: *mut MonitorInfo) -> i32;
        }

        unsafe {
            let fg = GetForegroundWindow();
            if fg.is_null() {
                return false;
            }
            let mut fg_pid: u32 = 0;
            GetWindowThreadProcessId(fg, &mut fg_pid);
            if fg_pid == GetCurrentProcessId() {
                return false;
            }

            // A normal maximized window (title bar present) can cover the whole monitor when the
            // taskbar is auto-hidden; that's not "fullscreen", so don't hide the card for it.
            const GWL_STYLE: i32 = -16;
            const WS_CAPTION: isize = 0x00C0_0000;
            let style = GetWindowLongPtrW(fg as isize, GWL_STYLE);
            if IsZoomed(fg) != 0 && (style & WS_CAPTION) == WS_CAPTION {
                return false;
            }

            let mut cls_buf = [0u16; 64];
            let cls_len = GetClassNameW(fg, cls_buf.as_mut_ptr(), cls_buf.len() as i32);
            if cls_len > 0 {
                let cls = String::from_utf16_lossy(&cls_buf[..cls_len as usize]);
                if matches!(
                    cls.as_str(),
                    "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
                ) {
                    return false;
                }
            }

            let mut wrc = WinRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if GetWindowRect(fg as isize, &mut wrc) == 0 {
                return false;
            }
            const MONITOR_DEFAULTTONEAREST: u32 = 2;
            let hmon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
            if hmon == 0 {
                return false;
            }
            let card = CARD_HWND.load(std::sync::atomic::Ordering::Relaxed) as Hwnd;
            if card.is_null() || MonitorFromWindow(card, MONITOR_DEFAULTTONEAREST) != hmon {
                return false;
            }
            // Exclusive (D3D) fullscreen games may not expose a monitor-sized foreground rect.
            const QUNS_RUNNING_D3D_FULL_SCREEN: i32 = 3;
            let mut quns: i32 = 0;
            if SHQueryUserNotificationState(&mut quns) == 0 && quns == QUNS_RUNNING_D3D_FULL_SCREEN
            {
                return true;
            }

            let mut mi = MonitorInfo {
                cb_size: std::mem::size_of::<MonitorInfo>() as u32,
                rc_monitor: WinRect {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
                rc_work: WinRect {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
                dw_flags: 0,
            };
            if GetMonitorInfoW(hmon, &mut mi) == 0 {
                return false;
            }

            wrc.left <= mi.rc_monitor.left
                && wrc.top <= mi.rc_monitor.top
                && wrc.right >= mi.rc_monitor.right
                && wrc.bottom >= mi.rc_monitor.bottom
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

#[cfg(target_os = "windows")]
fn monitor_work_rect_for_hwnd(hwnd: isize) -> Option<(i32, i32, i32, i32)> {
    use std::ffi::c_void;
    type Hwnd = *mut c_void;

    #[repr(C)]
    struct WinRect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[repr(C)]
    struct MonitorInfo {
        cb_size: u32,
        rc_monitor: WinRect,
        rc_work: WinRect,
        dw_flags: u32,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn MonitorFromWindow(hwnd: Hwnd, dw_flags: u32) -> isize;
        fn GetMonitorInfoW(hmonitor: isize, lpmi: *mut MonitorInfo) -> i32;
    }

    const MONITOR_DEFAULTTONEAREST: u32 = 2;
    unsafe {
        let hmon = MonitorFromWindow(hwnd as Hwnd, MONITOR_DEFAULTTONEAREST);
        if hmon == 0 {
            return None;
        }
        let mut mi = MonitorInfo {
            cb_size: std::mem::size_of::<MonitorInfo>() as u32,
            rc_monitor: WinRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            rc_work: WinRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            dw_flags: 0,
        };
        if GetMonitorInfoW(hmon, &mut mi) == 0 {
            return None;
        }
        Some((
            mi.rc_work.left,
            mi.rc_work.top,
            mi.rc_work.right,
            mi.rc_work.bottom,
        ))
    }
}

/// If the floating card is docked near the left, right, or top screen edge, return its
/// physical tucked `[x, y]` leaving a 6px peek strip visible.
fn compute_tucked_pos(ppp: f32) -> Option<[i32; 2]> {
    #[cfg(target_os = "windows")]
    {
        let (hwnd, x, y, w, h) = find_own_window_by_size(
            &CARD_HWND,
            (CARD_W * ppp).round() as i32,
            (CARD_H * ppp).round() as i32,
        )?;
        let (ml, mt, mr, _mb) = monitor_work_rect_for_hwnd(hwnd)?;
        let snap = (14.0 * ppp).round() as i32;
        let peek = (6.0 * ppp).round().max(5.0) as i32;

        if x <= ml + snap {
            Some([ml - w + peek, y])
        } else if x + w >= mr - snap {
            Some([mr - peek, y])
        } else if y <= mt + snap {
            Some([x, mt - h + peek])
        } else {
            None
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = ppp;
        None
    }
}

/// Restore a tucked card back inside its monitor work area.
fn compute_untucked_pos(ppp: f32) -> Option<[i32; 2]> {
    #[cfg(target_os = "windows")]
    {
        let (hwnd, x, y, w, h) = find_own_window_by_size(
            &CARD_HWND,
            (CARD_W * ppp).round() as i32,
            (CARD_H * ppp).round() as i32,
        )?;
        let (ml, mt, mr, mb) = monitor_work_rect_for_hwnd(hwnd)?;
        let nx = x.clamp(ml, (mr - w).max(ml));
        let ny = y.clamp(mt, (mb - h).max(mt));
        Some([nx, ny])
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = ppp;
        None
    }
}

fn move_floating_card_physical(ppp: f32, x: i32, y: i32) {
    #[cfg(target_os = "windows")]
    {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn SetWindowPos(
                hwnd: isize,
                insert_after: isize,
                x: i32,
                y: i32,
                cx: i32,
                cy: i32,
                flags: u32,
            ) -> i32;
        }
        if let Some((hwnd, _ox, _oy, _w, _h)) = find_own_window_by_size(
            &CARD_HWND,
            (CARD_W * ppp).round() as i32,
            (CARD_H * ppp).round() as i32,
        ) {
            const SWP_NOSIZE: u32 = 0x0001;
            const SWP_NOZORDER: u32 = 0x0004;
            const SWP_NOACTIVATE: u32 = 0x0010;
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    0,
                    x,
                    y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (ppp, x, y);
    }
}

#[cfg(test)]
mod tests {
    use super::led_activity;

    #[test]
    fn led_activity_scale() {
        assert_eq!(led_activity(0.0), 0.0);
        assert_eq!(led_activity(1024.0), 0.0);
        let k_100k = led_activity(100.0 * 1024.0); // ~0.5
        assert!((0.45..0.55).contains(&k_100k), "{k_100k}");
        assert_eq!(led_activity(10.0 * 1024.0 * 1024.0), 1.0);
        assert_eq!(led_activity(1e12), 1.0);
        assert!(led_activity(10.0 * 1024.0) < led_activity(1024.0 * 1024.0));
    }
}

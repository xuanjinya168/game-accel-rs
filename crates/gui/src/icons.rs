use crate::theme::{BRAND_BOLT_LOWER, BRAND_BOLT_UPPER, ThemePalette};
use eframe::egui::{Color32, CornerRadius, Painter, Pos2, Rect, Shape, Stroke, Vec2};
use std::f32::consts::TAU;

/// Draw the 32x32 Terracotta rounded brand mark with a two-triangle vector lightning bolt
/// matching `theme::build_app_icon()` pixel-for-pixel.
pub fn draw_brand_logo(painter: &Painter, rect: Rect, p: ThemePalette) {
    let radius = (rect.width() * 0.25).round() as u8;
    painter.rect_filled(rect, CornerRadius::same(radius), p.accent);

    let c = rect.center();
    let half = rect.width() * 0.5;
    let map_tri = |tri: [(f32, f32); 3]| -> Vec<Pos2> {
        tri.iter()
            .map(|&(nx, ny)| Pos2::new(c.x + nx * half, c.y + ny * half))
            .collect()
    };

    painter.add(Shape::convex_polygon(
        map_tri(BRAND_BOLT_UPPER),
        Color32::WHITE,
        Stroke::NONE,
    ));
    painter.add(Shape::convex_polygon(
        map_tri(BRAND_BOLT_LOWER),
        Color32::WHITE,
        Stroke::NONE,
    ));
}

/// Crisp 16x16 sidebar vector icons (unified 1.5px stroke weight).
pub fn draw_nav_icon(painter: &Painter, center: Pos2, kind: u8, color: Color32) {
    let stroke = Stroke::new(1.5_f32, color);
    match kind {
        // 0: Dashboard (speedometer gauge arc + needle — distinct from the Brand lightning mark)
        0 => {
            let r = 6.0_f32;
            let mut arc = Vec::with_capacity(18);
            // Arc from 145 deg (bottom-left) clockwise over the top to 395 deg (bottom-right)
            let start = 0.40 * TAU;
            let sweep = 0.70 * TAU;
            for i in 0..=16 {
                let a = start + (i as f32 / 16.0) * sweep;
                arc.push(Pos2::new(
                    center.x + a.cos() * r,
                    center.y + 0.8 + a.sin() * r,
                ));
            }
            painter.add(Shape::line(arc, stroke));
            let pivot = Pos2::new(center.x, center.y + 1.6);
            painter.line_segment([pivot, Pos2::new(center.x + 3.4, center.y - 2.2)], stroke);
            painter.circle_filled(pivot, 1.5, color);
        }
        // 1: Nodes (globe with curved meridian)
        1 => {
            let r = 6.0_f32;
            painter.circle_stroke(center, r, stroke);
            // Equator
            painter.line_segment(
                [
                    Pos2::new(center.x - r, center.y),
                    Pos2::new(center.x + r, center.y),
                ],
                Stroke::new(1.2_f32, color),
            );
            // Elliptical meridian loop
            let mut ellipse = Vec::with_capacity(22);
            for i in 0..=20 {
                let a = (i as f32 / 20.0) * TAU;
                ellipse.push(Pos2::new(
                    center.x + a.cos() * (r * 0.48),
                    center.y + a.sin() * r,
                ));
            }
            painter.add(Shape::line(ellipse, Stroke::new(1.2_f32, color)));
        }
        // 2: Logs (rounded terminal with prompt)
        2 => {
            let r = Rect::from_center_size(center, Vec2::new(12.5, 10.5));
            painter.rect_stroke(
                r,
                CornerRadius::same(2),
                stroke,
                eframe::egui::StrokeKind::Middle,
            );
            painter.line_segment(
                [
                    Pos2::new(center.x - 3.4, center.y - 2.0),
                    Pos2::new(center.x - 1.2, center.y),
                ],
                Stroke::new(1.4_f32, color),
            );
            painter.line_segment(
                [
                    Pos2::new(center.x - 1.2, center.y),
                    Pos2::new(center.x - 3.4, center.y + 2.0),
                ],
                Stroke::new(1.4_f32, color),
            );
            painter.line_segment(
                [
                    Pos2::new(center.x + 0.6, center.y + 2.0),
                    Pos2::new(center.x + 3.4, center.y + 2.0),
                ],
                Stroke::new(1.4_f32, color),
            );
        }
        // 4: Server Admin (dual stacked server rack units with status LEDs)
        4 => {
            let top =
                Rect::from_center_size(Pos2::new(center.x, center.y - 3.2), Vec2::new(12.4, 4.6));
            let bot =
                Rect::from_center_size(Pos2::new(center.x, center.y + 3.2), Vec2::new(12.4, 4.6));
            painter.rect_stroke(
                top,
                CornerRadius::same(2),
                stroke,
                eframe::egui::StrokeKind::Middle,
            );
            painter.rect_stroke(
                bot,
                CornerRadius::same(2),
                stroke,
                eframe::egui::StrokeKind::Middle,
            );
            painter.circle_filled(Pos2::new(top.left() + 3.0, top.center().y), 1.1, color);
            painter.circle_filled(Pos2::new(bot.left() + 3.0, bot.center().y), 1.1, color);
            painter.line_segment(
                [
                    Pos2::new(top.right() - 4.8, top.center().y),
                    Pos2::new(top.right() - 2.2, top.center().y),
                ],
                Stroke::new(1.3_f32, color),
            );
            painter.line_segment(
                [
                    Pos2::new(bot.right() - 4.8, bot.center().y),
                    Pos2::new(bot.right() - 2.2, bot.center().y),
                ],
                Stroke::new(1.3_f32, color),
            );
        }
        // 3: Settings (dual horizontal sliders)
        _ => {
            let y1 = center.y - 3.0;
            let y2 = center.y + 3.0;
            painter.line_segment(
                [Pos2::new(center.x - 6.0, y1), Pos2::new(center.x + 6.0, y1)],
                stroke,
            );
            painter.line_segment(
                [Pos2::new(center.x - 6.0, y2), Pos2::new(center.x + 6.0, y2)],
                stroke,
            );
            painter.circle_filled(Pos2::new(center.x - 2.0, y1), 2.1, color);
            painter.circle_filled(Pos2::new(center.x + 2.2, y2), 2.1, color);
        }
    }
}

/// Draw Sun (when dark mode active -> switch to light) or Moon (when light mode active -> switch to dark).
pub fn draw_theme_icon(painter: &Painter, center: Pos2, is_dark: bool, color: Color32) {
    let stroke = Stroke::new(1.4_f32, color);
    if is_dark {
        painter.circle_stroke(center, 3.0, stroke);
        for i in 0..8 {
            let a = (i as f32) * (TAU / 8.0);
            let p1 = Pos2::new(center.x + a.cos() * 4.5, center.y + a.sin() * 4.5);
            let p2 = Pos2::new(center.x + a.cos() * 6.0, center.y + a.sin() * 6.0);
            painter.line_segment([p1, p2], Stroke::new(1.3_f32, color));
        }
    } else {
        let mut pts = Vec::with_capacity(28);
        for i in 0..=14 {
            let a = 0.08 * TAU + (i as f32 / 14.0) * 0.62 * TAU;
            pts.push(Pos2::new(
                center.x + a.cos() * 5.4,
                center.y + a.sin() * 5.4,
            ));
        }
        for i in (0..=10).rev() {
            let a = 0.14 * TAU + (i as f32 / 10.0) * 0.50 * TAU;
            pts.push(Pos2::new(
                center.x + 2.4 + a.cos() * 4.0,
                center.y - 1.2 + a.sin() * 4.0,
            ));
        }
        if let Some(&first) = pts.first() {
            pts.push(first);
        }
        painter.add(Shape::line(pts, Stroke::new(1.4_f32, color)));
    }
}

/// Vector window control icons (Minimize, Maximize/Restore & Close).
pub fn draw_win_min_icon(painter: &Painter, center: Pos2, color: Color32) {
    painter.line_segment(
        [
            Pos2::new(center.x - 4.5, center.y),
            Pos2::new(center.x + 4.5, center.y),
        ],
        Stroke::new(1.4_f32, color),
    );
}

pub fn draw_win_max_icon(painter: &Painter, center: Pos2, is_maximized: bool, color: Color32) {
    let stroke = Stroke::new(1.3_f32, color);
    if is_maximized {
        // Overlapping restore squares
        let front = Rect::from_center_size(
            Pos2::new(center.x - 1.2, center.y + 1.2),
            Vec2::new(6.8, 6.8),
        );
        painter.rect_stroke(
            front,
            CornerRadius::same(1),
            stroke,
            eframe::egui::StrokeKind::Middle,
        );
        let top_left = Pos2::new(center.x - 2.2, center.y - 4.4);
        let top_right = Pos2::new(center.x + 4.4, center.y - 4.4);
        let bot_right = Pos2::new(center.x + 4.4, center.y + 2.2);
        painter.line_segment([top_left, top_right], stroke);
        painter.line_segment([top_right, bot_right], stroke);
    } else {
        let r = Rect::from_center_size(center, Vec2::new(8.4, 8.4));
        painter.rect_stroke(
            r,
            CornerRadius::same(1),
            stroke,
            eframe::egui::StrokeKind::Middle,
        );
    }
}

pub fn draw_win_close_icon(painter: &Painter, center: Pos2, color: Color32) {
    let s = 4.2;
    let stroke = Stroke::new(1.4_f32, color);
    painter.line_segment(
        [
            Pos2::new(center.x - s, center.y - s),
            Pos2::new(center.x + s, center.y + s),
        ],
        stroke,
    );
    painter.line_segment(
        [
            Pos2::new(center.x + s, center.y - s),
            Pos2::new(center.x - s, center.y + s),
        ],
        stroke,
    );
}

/// Crisp vector chevron (down when `up == false`, up when `up == true`), avoiding missing Unicode glyphs.
pub fn draw_chevron(painter: &Painter, center: Pos2, up: bool, color: Color32) {
    let stroke = Stroke::new(1.45_f32, color);
    let dy = if up { -1.8 } else { 1.8 };
    let left = Pos2::new(center.x - 3.4, center.y - dy * 0.55);
    let mid = Pos2::new(center.x, center.y + dy * 0.95);
    let right = Pos2::new(center.x + 3.4, center.y - dy * 0.55);
    painter.line_segment([left, mid], stroke);
    painter.line_segment([mid, right], stroke);
}

/// Power icon for the primary connection button.
pub fn draw_power_icon(painter: &Painter, center: Pos2, color: Color32) {
    let stroke = Stroke::new(1.8_f32, color);
    let r = 6.2;
    let mut pts = Vec::with_capacity(20);
    let start = -0.11 * TAU;
    let sweep = 0.72 * TAU;
    for i in 0..=18 {
        let a = start + (i as f32 / 18.0) * sweep;
        pts.push(Pos2::new(center.x + a.cos() * r, center.y + a.sin() * r));
    }
    painter.add(Shape::line(pts, stroke));
    painter.line_segment(
        [
            Pos2::new(center.x, center.y - r - 1.4),
            Pos2::new(center.x, center.y - 0.6),
        ],
        stroke,
    );
}

/// Magnifying glass icon for search inputs.
pub fn draw_search_icon(painter: &Painter, center: Pos2, color: Color32) {
    let stroke = Stroke::new(1.4_f32, color);
    let c = Pos2::new(center.x - 1.2, center.y - 1.2);
    painter.circle_stroke(c, 4.0, stroke);
    painter.line_segment(
        [
            Pos2::new(c.x + 2.9, c.y + 2.9),
            Pos2::new(center.x + 5.2, center.y + 5.2),
        ],
        stroke,
    );
}

/// Crisp geometric up/down flow arrow centered at `c` (avoids baseline drift of Unicode `↑`/`↓`).
pub fn draw_flow_arrow(painter: &Painter, c: Pos2, up: bool, col: Color32) {
    let s = Stroke::new(1.4_f32, col);
    let half_h = 3.8_f32;
    let wing = 2.6_f32;
    let top = Pos2::new(c.x, c.y - half_h);
    let bot = Pos2::new(c.x, c.y + half_h);
    painter.line_segment([top, bot], s);
    if up {
        painter.line_segment([top, Pos2::new(c.x - wing, top.y + wing)], s);
        painter.line_segment([top, Pos2::new(c.x + wing, top.y + wing)], s);
    } else {
        painter.line_segment([bot, Pos2::new(c.x - wing, bot.y - wing)], s);
        painter.line_segment([bot, Pos2::new(c.x + wing, bot.y - wing)], s);
    }
}

/// Animated pill toggle switch (`34 x 19`).
pub fn pill_toggle(
    ui: &mut eframe::egui::Ui,
    on: &mut bool,
    p: ThemePalette,
) -> eframe::egui::Response {
    let desired = Vec2::new(34.0, 19.0);
    let (rect, mut resp) = ui.allocate_exact_size(desired, eframe::egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let how_on = ui.ctx().animate_bool_responsive(resp.id, *on);
    let track_bg = if *on { p.accent } else { p.border_strong };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(10), track_bg);

    let r = 7.2;
    let cx = eframe::egui::lerp((rect.left() + 9.5)..=(rect.right() - 9.5), how_on);
    let knob = Pos2::new(cx, rect.center().y);
    painter.circle_filled(
        Pos2::new(knob.x, knob.y + 0.8),
        r,
        Color32::from_black_alpha(28),
    );
    painter.circle_filled(knob, r, Color32::WHITE);
    resp
}

use crate::theme::{
    BRAND_BOLT_BRIDGE, BRAND_BOLT_LOWER, BRAND_BOLT_OUTLINE, BRAND_BOLT_UPPER, ThemePalette,
};
use eframe::egui::{Color32, CornerRadius, Mesh, Painter, Pos2, Rect, Shape, Stroke, Vec2};
use std::f32::consts::{FRAC_PI_2, PI, TAU};

/// Visual state for the hollow-to-solid animated terracotta brand mark.
#[derive(Debug, Clone, Copy)]
pub struct BrandLogoVisual {
    /// Bottom-up solid fill ratio (`0.0` = idle wireframe, `1.0` = active solid bolt).
    pub fill_t: f32,
    /// Decaying `1.0 -> 0.0` ripple & pop impulse fired when entering `Connected` or clicked.
    pub pulse: f32,
    /// Smooth `0.0 -> 1.0` hover lift ratio.
    pub hover: f32,
}

/// Clip a convex polygon to the half-plane `y >= y_cut` (bottom-up rising liquid fill).
fn clip_polygon_below_y(poly: &[Pos2], y_cut: f32) -> Vec<Pos2> {
    if poly.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Pos2> = Vec::with_capacity(poly.len() + 1);
    let push_dedup = |dst: &mut Vec<Pos2>, pt: Pos2| {
        if dst.last().is_none_or(|prev| prev.distance_sq(pt) > 1e-6) {
            dst.push(pt);
        }
    };
    let n = poly.len();
    for i in 0..n {
        let a = poly[i];
        let b = poly[(i + 1) % n];
        let a_in = a.y >= y_cut;
        let b_in = b.y >= y_cut;
        if a_in && b_in {
            push_dedup(&mut out, b);
        } else if a_in && !b_in {
            let denom = b.y - a.y;
            if denom.abs() > 1e-6 {
                let t = ((y_cut - a.y) / denom).clamp(0.0, 1.0);
                push_dedup(&mut out, Pos2::new(a.x + (b.x - a.x) * t, y_cut));
            }
        } else if !a_in && b_in {
            let denom = b.y - a.y;
            if denom.abs() > 1e-6 {
                let t = ((y_cut - a.y) / denom).clamp(0.0, 1.0);
                push_dedup(&mut out, Pos2::new(a.x + (b.x - a.x) * t, y_cut));
            }
            push_dedup(&mut out, b);
        }
    }
    if out.len() > 1
        && let (Some(&first), Some(&last)) = (out.first(), out.last())
        && first.distance_sq(last) <= 1e-6
    {
        out.pop();
    }
    out
}

fn draw_terracotta_squircle(
    painter: &Painter,
    rect: Rect,
    radius: f32,
    highlight_alpha: u8,
    p: ThemePalette,
) {
    let r_u8 = radius.round() as u8;
    // Anti-aliased solid base keeps outer rounded edges butter-smooth.
    painter.rect_filled(rect, CornerRadius::same(r_u8), p.accent);

    // Subtle top-lit terracotta gradient (#E38667 -> #CC6848) inset 0.5px inside the AA rim.
    let inner = rect.shrink(0.5);
    let r_in = (radius - 0.5).max(1.0).min(inner.width() * 0.5);
    let top_col = Color32::from_rgb(227, 134, 103);
    let bot_col = Color32::from_rgb(204, 104, 72);
    let lerp_col = |y: f32| -> Color32 {
        let t = ((y - inner.top()) / inner.height().max(1.0)).clamp(0.0, 1.0);
        Color32::from_rgb(
            (top_col.r() as f32 + (bot_col.r() as f32 - top_col.r() as f32) * t).round() as u8,
            (top_col.g() as f32 + (bot_col.g() as f32 - top_col.g() as f32) * t).round() as u8,
            (top_col.b() as f32 + (bot_col.b() as f32 - top_col.b() as f32) * t).round() as u8,
        )
    };

    const SEG: usize = 6;
    let corners = [
        (Pos2::new(inner.right() - r_in, inner.top() + r_in), -FRAC_PI_2, 0.0),
        (Pos2::new(inner.right() - r_in, inner.bottom() - r_in), 0.0, FRAC_PI_2),
        (Pos2::new(inner.left() + r_in, inner.bottom() - r_in), FRAC_PI_2, PI),
        (Pos2::new(inner.left() + r_in, inner.top() + r_in), PI, 1.5 * PI),
    ];
    let mut perimeter = Vec::with_capacity(corners.len() * (SEG + 1));
    for (cc, a0, a1) in corners {
        for i in 0..=SEG {
            let a = a0 + (a1 - a0) * (i as f32 / SEG as f32);
            perimeter.push(Pos2::new(cc.x + a.cos() * r_in, cc.y + a.sin() * r_in));
        }
    }

    let mut mesh = Mesh::default();
    let c = inner.center();
    mesh.colored_vertex(c, lerp_col(c.y));
    for &pt in &perimeter {
        mesh.colored_vertex(pt, lerp_col(pt.y));
    }
    let n = perimeter.len() as u32;
    for i in 0..n {
        mesh.add_triangle(0, 1 + i, 1 + (i + 1) % n);
    }
    painter.add(Shape::mesh(mesh));

    // Crisp 1px inner top-lit rim highlight.
    painter.rect_stroke(
        inner,
        CornerRadius::same(r_in.round() as u8),
        Stroke::new(1.0_f32, Color32::from_white_alpha(highlight_alpha)),
        eframe::egui::StrokeKind::Inside,
    );
}

/// Draw the Terracotta rounded brand mark with hollow-to-solid sleek lightning animation:
/// idle = crisp white wireframe, connecting = gentle charging surge, connected = solid white fill
/// with an outward terracotta pulse ripple on activation.
pub fn draw_brand_logo(painter: &Painter, rect: Rect, p: ThemePalette, visual: BrandLogoVisual) {
    let sz = rect.width();
    let radius = sz * 0.25;
    let fill_t = visual.fill_t.clamp(0.0, 1.0);
    let pulse = visual.pulse.clamp(0.0, 1.0);
    let hover = visual.hover.clamp(0.0, 1.0);

    // Outward terracotta pulse ripple on activation / tactile click
    if pulse > 0.01 {
        let ex = (1.0 - pulse) * (sz * 0.185);
        let ring_rect = rect.expand(ex);
        let ring_r = (radius + ex * 0.6).round() as u8;
        let alpha = (pulse * 0.65 * 255.0).round().clamp(0.0, 255.0) as u8;
        painter.rect_stroke(
            ring_rect,
            CornerRadius::same(ring_r),
            Stroke::new(
                (sz * 0.044).clamp(1.2, 1.7),
                Color32::from_rgba_unmultiplied(p.accent.r(), p.accent.g(), p.accent.b(), alpha),
            ),
            eframe::egui::StrokeKind::Middle,
        );
    }

    let highlight_alpha = ((0.22 + hover * 0.16 + pulse * 0.22) * 255.0)
        .round()
        .clamp(0.0, 255.0) as u8;
    draw_terracotta_squircle(painter, rect, radius, highlight_alpha, p);

    let c = rect.center();
    let half = sz * 0.5;
    let scale = 1.0 + hover * 0.04 + (pulse * PI).sin() * 0.06;
    let map_pts = |pts: &[(f32, f32)]| -> Vec<Pos2> {
        pts.iter()
            .map(|&(nx, ny)| Pos2::new(c.x + nx * half * scale, c.y + ny * half * scale))
            .collect()
    };

    // Bottom-up rising solid white fill controlled by `fill_t`
    if fill_t > 0.02 {
        let y_top = c.y - 0.68 * half * scale;
        let y_bot = c.y + 0.68 * half * scale;
        let y_cut = y_bot - (y_bot - y_top) * fill_t;

        for piece in [
            BRAND_BOLT_UPPER.as_slice(),
            BRAND_BOLT_LOWER.as_slice(),
            BRAND_BOLT_BRIDGE.as_slice(),
        ] {
            let clipped = clip_polygon_below_y(&map_pts(piece), y_cut);
            if clipped.len() >= 3 {
                painter.add(Shape::convex_polygon(clipped, Color32::WHITE, Stroke::NONE));
            }
        }
    }

    // Always-crisp white wireframe outline (defines idle silhouette & keeps active stroke sharp)
    let stroke_w = (sz * 0.044).clamp(1.15, 2.2);
    painter.add(Shape::closed_line(
        map_pts(&BRAND_BOLT_OUTLINE),
        Stroke::new(stroke_w, Color32::from_white_alpha(238)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::point_in_triangle;

    #[test]
    fn bolt_bridge_stays_strictly_inside_lightning_union() {
        for &(x, y) in &BRAND_BOLT_BRIDGE {
            assert!(
                point_in_triangle(x, y, BRAND_BOLT_UPPER)
                    || point_in_triangle(x, y, BRAND_BOLT_LOWER),
                "bridge corner ({x}, {y}) escaped lightning silhouette"
            );
        }
    }

    #[test]
    fn clip_polygon_below_y_handles_empty_partial_and_full_fill() {
        let tri = vec![
            Pos2::new(0.0, 0.0),
            Pos2::new(10.0, 10.0),
            Pos2::new(-10.0, 10.0),
        ];
        assert!(clip_polygon_below_y(&tri, 12.0).is_empty());
        assert_eq!(clip_polygon_below_y(&tri, -2.0).len(), 3);

        let mid = clip_polygon_below_y(&tri, 5.0);
        assert_eq!(mid.len(), 4);
        assert!(mid.iter().all(|p| p.y >= 5.0 - 1e-4));
    }
}

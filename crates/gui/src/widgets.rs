use crate::ConnState;
use crate::icons;
use crate::storage::format_speed;
use crate::theme::{FONT_BODY, FONT_SM, FONT_SUB, FONT_XS, ThemePalette};
use eframe::egui::{
    self, Color32, CornerRadius, FontId, Frame, Margin, Pos2, Sense, Stroke, StrokeKind, Vec2,
};
use std::collections::VecDeque;

pub fn card_frame(p: ThemePalette) -> Frame {
    Frame::new()
        .fill(p.bg_card)
        .stroke(Stroke::new(1.0_f32, p.border_subtle))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(20, 16))
        .shadow(p.card_shadow)
}

/// Crisp 1px horizontal hairline divider replacing raw `ui.separator()`.
pub fn hairline_divider(ui: &mut egui::Ui, p: ThemePalette) {
    let w = ui.available_width().max(1.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
    ui.painter().line_segment(
        [
            Pos2::new(rect.left(), rect.center().y),
            Pos2::new(rect.right(), rect.center().y),
        ],
        Stroke::new(1.0_f32, p.border_subtle),
    );
}

/// Unified primary Terracotta button (consistent hover & press feedback everywhere).
pub fn primary_btn(ui: &mut egui::Ui, text: &str, p: ThemePalette) -> egui::Response {
    let font = FontId::proportional(FONT_SM);
    let galley = ui.fonts(|f| f.layout_no_wrap(text.to_string(), font, Color32::WHITE));
    let w = (galley.size().x + 26.0).max(76.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 30.0), Sense::click());
    let enabled = ui.is_enabled();

    let base = p.accent;
    let fill = if !enabled {
        base.linear_multiply(0.55)
    } else if resp.is_pointer_button_down_on() {
        base.linear_multiply(0.88)
    } else if resp.hovered() {
        base.linear_multiply(1.06)
    } else {
        base
    };

    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(8), fill);
    painter.galley(
        Pos2::new(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        Color32::WHITE,
    );
    resp
}

/// Unified secondary outlined button (consistent hover/active fill across all 4 pages).
pub fn soft_btn(ui: &mut egui::Ui, text: &str, p: ThemePalette) -> egui::Response {
    let font = FontId::proportional(FONT_SM);
    let enabled = ui.is_enabled();
    let fg = if enabled {
        p.text_primary
    } else {
        p.text_muted
    };
    let galley = ui.fonts(|f| f.layout_no_wrap(text.to_string(), font, fg));
    let w = (galley.size().x + 24.0).max(56.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 30.0), Sense::click());

    let (fill, border) = if !enabled {
        (p.bg_card_alt, p.border_subtle)
    } else if resp.is_pointer_button_down_on() {
        (p.bg_nav_active, p.border_strong)
    } else if resp.hovered() {
        (p.bg_nav_hover, p.border_strong)
    } else {
        (p.bg_card, p.border_subtle)
    };

    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(8),
        fill,
        Stroke::new(1.0_f32, border),
        StrokeKind::Inside,
    );
    painter.galley(
        Pos2::new(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        fg,
    );
    resp
}

/// Compact inline action button (`26px` height) for node rows, filters, and log toolbar.
pub fn chip_btn(
    ui: &mut egui::Ui,
    text: &str,
    p: ThemePalette,
    is_danger: bool,
) -> egui::Response {
    let font = FontId::proportional(FONT_SM);
    let enabled = ui.is_enabled();
    let fg = if !enabled {
        p.text_muted
    } else if is_danger {
        p.danger_text
    } else {
        p.text_secondary
    };
    let galley = ui.fonts(|f| f.layout_no_wrap(text.to_string(), font, fg));
    let w = (galley.size().x + 18.0).max(44.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 26.0), Sense::click());

    let (fill, border) = if !enabled {
        (p.bg_card_alt, p.border_subtle)
    } else if is_danger && resp.hovered() {
        (p.danger_soft, p.danger_text)
    } else if resp.is_pointer_button_down_on() {
        (p.bg_nav_active, p.border_strong)
    } else if resp.hovered() {
        (p.bg_nav_hover, p.border_strong)
    } else {
        (p.bg_card_alt, p.border_subtle)
    };

    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(7),
        fill,
        Stroke::new(1.0_f32, border),
        StrokeKind::Inside,
    );
    painter.galley(
        Pos2::new(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        fg,
    );
    resp
}

/// Highlighted compact chip (`选用` / active filter tab).
pub fn accent_chip_btn(
    ui: &mut egui::Ui,
    text: &str,
    p: ThemePalette,
    solid: bool,
) -> egui::Response {
    let font = FontId::proportional(FONT_SM);
    let enabled = ui.is_enabled();
    let fg = if solid {
        Color32::WHITE
    } else if enabled {
        p.accent_text
    } else {
        p.text_muted
    };
    let galley = ui.fonts(|f| f.layout_no_wrap(text.to_string(), font, fg));
    let w = (galley.size().x + 18.0).max(44.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 26.0), Sense::click());

    let (fill, border) = if solid {
        let base = if resp.hovered() {
            p.accent.linear_multiply(1.05)
        } else {
            p.accent
        };
        (base, p.accent)
    } else if !enabled {
        (p.bg_card_alt, p.border_subtle)
    } else if resp.hovered() {
        (p.accent_soft.linear_multiply(0.96), p.accent)
    } else {
        (p.accent_soft, p.accent)
    };

    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(7),
        fill,
        Stroke::new(1.0_f32, border),
        StrokeKind::Inside,
    );
    painter.galley(
        Pos2::new(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        fg,
    );
    resp
}

pub fn pill(ui: &mut egui::Ui, text: &str, bg: Color32, fg: Color32) {
    let font = FontId::proportional(FONT_XS);
    let galley = ui.fonts(|f| f.layout_no_wrap(text.to_string(), font, fg));
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(galley.size().x + 16.0, galley.size().y + 6.0),
        Sense::hover(),
    );
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(10), bg);
    painter.galley(
        Pos2::new(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        fg,
    );
}

pub fn draw_status_pill(
    ui: &mut egui::Ui,
    text: &str,
    dot_col: Color32,
    bg: Color32,
    fg: Color32,
    stroke_col: Color32,
) {
    let font_id = FontId::proportional(FONT_SM);
    let galley = ui.fonts(|f| f.layout_no_wrap(text.to_string(), font_id, fg));
    let w = galley.size().x + 30.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 26.0), Sense::hover());
    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(13),
        bg,
        Stroke::new(1.0_f32, stroke_col),
        StrokeKind::Inside,
    );
    painter.circle_filled(Pos2::new(rect.left() + 12.0, rect.center().y), 3.3, dot_col);
    painter.galley(
        Pos2::new(rect.left() + 20.0, rect.center().y - galley.size().y * 0.5),
        galley,
        fg,
    );
}

/// Primary Hero connection CTA capsule (`236 x 50`).
/// - Disconnected / Connecting: bold Terracotta CTA with soft halo.
/// - Connected: calm sage-tinted outlined capsule matching the top-right status pill so
///   the status color complements rather than overwhelms the Warm Linen & Terracotta palette;
///   hovering gently previews the disconnect action.
pub fn draw_hero_power_button(
    ui: &mut egui::Ui,
    state: ConnState,
    reconnect: Option<(u32, u64)>,
    has_nodes: bool,
    p: ThemePalette,
) -> egui::Response {
    let desired = Vec2::new(236.0, 50.0);
    let (rect, resp) = ui.allocate_exact_size(desired, Sense::click());
    let painter = ui.painter();

    let (fill, border_stroke, halo_col, fg_col, label) = match state {
        ConnState::Disconnected => {
            let base = p.accent;
            let f = if resp.is_pointer_button_down_on() {
                base.linear_multiply(0.90)
            } else if resp.hovered() {
                base.linear_multiply(1.06)
            } else {
                base
            };
            let lbl = if has_nodes {
                "一键开启加速"
            } else {
                "添加节点以开始"
            }
            .to_string();
            (
                f,
                Stroke::new(1.0_f32, Color32::from_white_alpha(40)),
                Color32::from_rgba_unmultiplied(217, 119, 87, if resp.hovered() { 44 } else { 20 }),
                Color32::WHITE,
                lbl,
            )
        }
        ConnState::Connecting => {
            let lbl = match reconnect {
                Some((attempt, _)) => format!("正在自动重连（第 {attempt} 次）..."),
                None => "正在建立加密通道...".to_string(),
            };
            (
                p.accent,
                Stroke::new(1.0_f32, Color32::from_white_alpha(40)),
                Color32::from_rgba_unmultiplied(217, 119, 87, 46),
                Color32::WHITE,
                lbl,
            )
        }
        ConnState::Connected => {
            if resp.hovered() {
                (
                    p.danger_soft,
                    Stroke::new(1.5_f32, p.danger_text),
                    Color32::from_rgba_unmultiplied(198, 63, 59, 26),
                    p.danger_text,
                    "点击断开加速".to_string(),
                )
            } else {
                (
                    p.success_soft,
                    Stroke::new(1.5_f32, p.success_text),
                    Color32::from_rgba_unmultiplied(29, 107, 74, if p.is_dark { 34 } else { 20 }),
                    p.success_text,
                    "加速保护中".to_string(),
                )
            }
        }
    };

    painter.rect_filled(rect.expand(4.0), CornerRadius::same(29), halo_col);
    painter.rect(
        rect,
        CornerRadius::same(25),
        fill,
        border_stroke,
        StrokeKind::Inside,
    );

    let font_id = FontId::proportional(FONT_SUB);
    let galley = ui.fonts(|f| f.layout_no_wrap(label, font_id, fg_col));
    let icon_w = 16.0;
    let gap = 9.0;
    let total_w = icon_w + gap + galley.size().x;
    let start_x = rect.center().x - total_w * 0.5;

    icons::draw_power_icon(
        painter,
        Pos2::new(start_x + icon_w * 0.5, rect.center().y),
        fg_col,
    );
    painter.galley(
        Pos2::new(
            start_x + icon_w + gap,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        fg_col,
    );

    resp
}

pub fn styled_input(
    ui: &mut egui::Ui,
    text: &mut String,
    hint: &str,
    width: f32,
    password: bool,
    p: ThemePalette,
) -> egui::Response {
    let mut inner_resp: Option<egui::Response> = None;
    let has_focus_id = ui.id().with(hint).with(width.to_bits());
    let focused = ui.ctx().memory(|m| m.has_focus(has_focus_id));
    Frame::new()
        .fill(p.bg_input)
        .stroke(Stroke::new(
            if focused { 1.5_f32 } else { 1.0_f32 },
            if focused { p.accent } else { p.border_strong },
        ))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            let r = ui.add(
                egui::TextEdit::singleline(text)
                    .id(has_focus_id)
                    .frame(false)
                    .font(FontId::proportional(FONT_BODY))
                    .password(password)
                    .hint_text(hint)
                    .desired_width((width - 20.0).max(60.0)),
            );
            inner_resp = Some(r);
        });
    inner_resp.expect("styled_input inner response")
}

pub fn styled_multiline(
    ui: &mut egui::Ui,
    text: &mut String,
    hint: &str,
    rows: usize,
    p: ThemePalette,
) -> egui::Response {
    let mut inner_resp: Option<egui::Response> = None;
    let id = ui.id().with("multiline").with(hint);
    let focused = ui.ctx().memory(|m| m.has_focus(id));
    Frame::new()
        .fill(p.bg_input)
        .stroke(Stroke::new(
            if focused { 1.5_f32 } else { 1.0_f32 },
            if focused { p.accent } else { p.border_strong },
        ))
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            let r = ui.add(
                egui::TextEdit::multiline(text)
                    .id(id)
                    .frame(false)
                    .font(FontId::monospace(FONT_SM))
                    .desired_rows(rows)
                    .desired_width(ui.available_width())
                    .hint_text(hint),
            );
            inner_resp = Some(r);
        });
    inner_resp.expect("styled_multiline inner response")
}

pub fn styled_search_input(
    ui: &mut egui::Ui,
    text: &mut String,
    hint: &str,
    width: f32,
    p: ThemePalette,
) {
    let id = ui.id().with("search_input");
    let focused = ui.ctx().memory(|m| m.has_focus(id));
    Frame::new()
        .fill(p.bg_input)
        .stroke(Stroke::new(
            if focused { 1.5_f32 } else { 1.0_f32 },
            if focused { p.accent } else { p.border_subtle },
        ))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let (ir, _) = ui.allocate_exact_size(Vec2::new(14.0, 18.0), Sense::hover());
                icons::draw_search_icon(ui.painter(), ir.center(), p.text_muted);
                ui.add(
                    egui::TextEdit::singleline(text)
                        .id(id)
                        .frame(false)
                        .font(FontId::proportional(FONT_SM))
                        .hint_text(hint)
                        .desired_width((width - 36.0).max(80.0)),
                );
            });
        });
}

pub fn draw_rtt_sparkline(
    ui: &mut egui::Ui,
    history: &VecDeque<u64>,
    is_connected: bool,
    p: ThemePalette,
) {
    let height = ui.available_height().clamp(96.0, 260.0);
    let desired = Vec2::new(ui.available_width(), height);
    let (rect, _resp) = ui.allocate_exact_size(desired, Sense::hover());
    let painter = ui.painter_at(rect);

    painter.rect(
        rect,
        CornerRadius::same(10),
        p.bg_card,
        Stroke::new(1.0_f32, p.border_subtle),
        StrokeKind::Inside,
    );

    if !is_connected || history.len() < 2 {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "连接后显示实时延迟波动曲线",
            FontId::proportional(FONT_SM),
            p.text_muted,
        );
        return;
    }

    let peak = history.iter().copied().max().unwrap_or(100).max(50);
    let max_val = ((peak as f32 * 1.35) / 10.0).ceil() * 10.0;
    let left_pad = 50.0;
    let right_pad = 14.0;
    let top_pad = 28.0;
    let bot_pad = 14.0;
    let inner = egui::Rect::from_min_max(
        Pos2::new(rect.left() + left_pad, rect.top() + top_pad),
        Pos2::new(rect.right() - right_pad, rect.bottom() - bot_pad),
    );
    let step_x = inner.width() / (history.len().saturating_sub(1).max(1) as f32);

    // Subtle 3-tier horizontal gridlines + left Y-axis ticks
    let grid_col = if p.is_dark {
        Color32::from_white_alpha(14)
    } else {
        Color32::from_black_alpha(14)
    };
    for (frac, lbl) in [
        (0.0_f32, format!("{} ms", max_val as u64)),
        (0.5_f32, format!("{} ms", (max_val * 0.5) as u64)),
        (1.0_f32, "0 ms".to_string()),
    ] {
        let y = inner.top() + frac * inner.height();
        painter.line_segment(
            [Pos2::new(inner.left(), y), Pos2::new(inner.right(), y)],
            Stroke::new(1.0_f32, grid_col),
        );
        painter.text(
            Pos2::new(inner.left() - 6.0, y),
            egui::Align2::RIGHT_CENTER,
            lbl,
            FontId::monospace(FONT_XS),
            p.text_muted,
        );
    }

    let points: Vec<Pos2> = history
        .iter()
        .enumerate()
        .map(|(i, &val)| {
            let x = inner.left() + i as f32 * step_x;
            let norm = (val as f32 / max_val).clamp(0.0, 1.0);
            let y = inner.bottom() - norm * inner.height();
            Pos2::new(x, y)
        })
        .collect();

    let mut mesh = egui::Mesh::default();
    let top_col = Color32::from_rgba_unmultiplied(217, 119, 87, if p.is_dark { 58 } else { 42 });
    let bot_col = Color32::from_rgba_unmultiplied(217, 119, 87, 0);
    for (i, pt) in points.iter().enumerate() {
        let base_idx = (i * 2) as u32;
        mesh.colored_vertex(*pt, top_col);
        mesh.colored_vertex(Pos2::new(pt.x, inner.bottom()), bot_col);
        if i > 0 {
            let prev = base_idx - 2;
            mesh.add_triangle(prev, prev + 1, base_idx);
            mesh.add_triangle(prev + 1, base_idx + 1, base_idx);
        }
    }
    painter.add(egui::Shape::mesh(mesh));

    painter.add(egui::Shape::line(points.clone(), Stroke::new(2.0_f32, p.accent)));
    if let Some(&last_pt) = points.last() {
        painter.circle_filled(
            last_pt,
            6.0,
            Color32::from_rgba_unmultiplied(217, 119, 87, 60),
        );
        painter.circle_filled(last_pt, 3.0, p.accent);
    }

    // Header legend (left) & live stats (right)
    painter.circle_filled(Pos2::new(rect.left() + 14.0, rect.top() + 14.0), 3.2, p.accent);
    painter.text(
        Pos2::new(rect.left() + 22.0, rect.top() + 14.0),
        egui::Align2::LEFT_CENTER,
        "QUIC 延迟",
        FontId::proportional(FONT_XS),
        p.text_muted,
    );
    if let Some(&last) = history.back() {
        let avg: u64 = history.iter().sum::<u64>() / history.len() as u64;
        painter.text(
            Pos2::new(rect.right() - 12.0, rect.top() + 14.0),
            egui::Align2::RIGHT_CENTER,
            format!("当前 {last} ms · 均值 {avg} ms"),
            FontId::monospace(FONT_XS),
            p.text_muted,
        );
    }
}

/// Real-time traffic throughput sparkline (Download in Terracotta area fill, Upload in Emerald line).
pub fn draw_traffic_sparkline(
    ui: &mut egui::Ui,
    history: &VecDeque<(f64, f64)>,
    is_connected: bool,
    p: ThemePalette,
) {
    let height = ui.available_height().clamp(96.0, 260.0);
    let desired = Vec2::new(ui.available_width(), height);
    let (rect, _resp) = ui.allocate_exact_size(desired, Sense::hover());
    let painter = ui.painter_at(rect);

    painter.rect(
        rect,
        CornerRadius::same(10),
        p.bg_card,
        Stroke::new(1.0_f32, p.border_subtle),
        StrokeKind::Inside,
    );

    if !is_connected || history.len() < 2 {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "连接后显示实时下载 / 上传速率曲线",
            FontId::proportional(FONT_SM),
            p.text_muted,
        );
        return;
    }

    let peak_bps = history
        .iter()
        .map(|&(rx, tx)| rx.max(tx))
        .fold(0.0_f64, f64::max);
    let scale_max = (peak_bps * 1.30).max(16.0 * 1024.0);

    let left_pad = 68.0;
    let right_pad = 14.0;
    let top_pad = 28.0;
    let bot_pad = 14.0;
    let inner = egui::Rect::from_min_max(
        Pos2::new(rect.left() + left_pad, rect.top() + top_pad),
        Pos2::new(rect.right() - right_pad, rect.bottom() - bot_pad),
    );
    let step_x = inner.width() / (history.len().saturating_sub(1).max(1) as f32);

    // 3-tier horizontal gridlines + Y-axis scale labels
    let grid_col = if p.is_dark {
        Color32::from_white_alpha(14)
    } else {
        Color32::from_black_alpha(14)
    };
    for (frac, lbl) in [
        (0.0_f32, format_speed(scale_max)),
        (0.5_f32, format_speed(scale_max * 0.5)),
        (1.0_f32, "0 B/s".to_string()),
    ] {
        let y = inner.top() + frac * inner.height();
        painter.line_segment(
            [Pos2::new(inner.left(), y), Pos2::new(inner.right(), y)],
            Stroke::new(1.0_f32, grid_col),
        );
        painter.text(
            Pos2::new(inner.left() - 6.0, y),
            egui::Align2::RIGHT_CENTER,
            lbl,
            FontId::monospace(FONT_XS),
            p.text_muted,
        );
    }

    let rx_points: Vec<Pos2> = history
        .iter()
        .enumerate()
        .map(|(i, &(rx, _))| {
            let x = inner.left() + i as f32 * step_x;
            let norm = (rx / scale_max).clamp(0.0, 1.0) as f32;
            let y = inner.bottom() - norm * inner.height();
            Pos2::new(x, y)
        })
        .collect();

    let tx_points: Vec<Pos2> = history
        .iter()
        .enumerate()
        .map(|(i, &(_, tx))| {
            let x = inner.left() + i as f32 * step_x;
            let norm = (tx / scale_max).clamp(0.0, 1.0) as f32;
            let y = inner.bottom() - norm * inner.height();
            Pos2::new(x, y)
        })
        .collect();

    // Terracotta translucent area fill under Download curve
    let mut mesh = egui::Mesh::default();
    let top_col = Color32::from_rgba_unmultiplied(217, 119, 87, if p.is_dark { 58 } else { 42 });
    let bot_col = Color32::from_rgba_unmultiplied(217, 119, 87, 0);
    for (i, pt) in rx_points.iter().enumerate() {
        let base_idx = (i * 2) as u32;
        mesh.colored_vertex(*pt, top_col);
        mesh.colored_vertex(Pos2::new(pt.x, inner.bottom()), bot_col);
        if i > 0 {
            let prev = base_idx - 2;
            mesh.add_triangle(prev, prev + 1, base_idx);
            mesh.add_triangle(prev + 1, base_idx + 1, base_idx);
        }
    }
    painter.add(egui::Shape::mesh(mesh));

    // Upload curve (secondary emerald line) + Download curve (primary terracotta)
    painter.add(egui::Shape::line(
        tx_points,
        Stroke::new(1.5_f32, p.success_text),
    ));
    painter.add(egui::Shape::line(
        rx_points.clone(),
        Stroke::new(2.0_f32, p.accent),
    ));
    if let Some(&last_pt) = rx_points.last() {
        painter.circle_filled(
            last_pt,
            5.5,
            Color32::from_rgba_unmultiplied(217, 119, 87, 56),
        );
        painter.circle_filled(last_pt, 2.8, p.accent);
    }

    // Inline legend (top-left) + live speed summary (top-right)
    let legend_y = rect.top() + 14.0;
    painter.circle_filled(Pos2::new(rect.left() + 14.0, legend_y), 3.2, p.accent);
    painter.text(
        Pos2::new(rect.left() + 22.0, legend_y),
        egui::Align2::LEFT_CENTER,
        "下载",
        FontId::proportional(FONT_XS),
        p.text_muted,
    );
    painter.circle_filled(Pos2::new(rect.left() + 58.0, legend_y), 3.2, p.success_text);
    painter.text(
        Pos2::new(rect.left() + 66.0, legend_y),
        egui::Align2::LEFT_CENTER,
        "上传",
        FontId::proportional(FONT_XS),
        p.text_muted,
    );

    if let Some(&(last_rx, last_tx)) = history.back() {
        painter.text(
            Pos2::new(rect.right() - 12.0, legend_y),
            egui::Align2::RIGHT_CENTER,
            format!(
                "↓ {}  ↑ {}  峰值 {}",
                format_speed(last_rx),
                format_speed(last_tx),
                format_speed(peak_bps)
            ),
            FontId::monospace(FONT_XS),
            p.text_muted,
        );
    }
}

/// Renders a centered floating modal dialog with a dimmed backdrop, Esc/backdrop/`×` close handling,
/// and consistent card styling. Returns `true` if the user requested to close the modal.
pub fn modal_dialog(
    ctx: &egui::Context,
    id: &str,
    title: &str,
    width: f32,
    p: ThemePalette,
    add_contents: impl FnOnce(&mut egui::Ui),
) -> bool {
    let mut close_requested = ctx.input(|i| i.key_pressed(egui::Key::Escape));

    let screen = ctx.screen_rect();
    egui::Area::new(egui::Id::new((id, "backdrop")))
        .order(egui::Order::Middle)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            let resp = ui.allocate_response(screen.size(), Sense::click());
            ui.painter()
                .rect_filled(screen, 0.0, Color32::from_black_alpha(120));
            if resp.clicked() {
                close_requested = true;
            }
        });

    let modal_frame = card_frame(p)
        .inner_margin(Margin::same(20))
        .shadow(egui::Shadow {
            offset: [0, 8],
            blur: 24,
            spread: 0,
            color: Color32::from_black_alpha(70),
        });

    egui::Window::new(title)
        .id(egui::Id::new((id, "window")))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        .collapsible(false)
        .resizable(false)
        .auto_sized()
        .title_bar(false)
        .frame(modal_frame)
        .show(ctx, |ui| {
            ui.set_width(width);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(title)
                        .size(FONT_SUB)
                        .strong()
                        .color(p.text_primary),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if chip_btn(ui, "×", p, false).clicked() {
                        close_requested = true;
                    }
                });
            });
            ui.add_space(12.0);
            add_contents(ui);
        });

    close_requested
}

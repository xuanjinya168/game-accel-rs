use crate::ConnState;
use crate::icons;
use crate::storage::format_speed;
use crate::theme::{FONT_BODY, FONT_SM, FONT_SUB, FONT_XS, ThemePalette, font_bold};
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Frame, Layout, Margin, Pos2, Sense, Stroke,
    StrokeKind, Vec2,
};
use std::collections::VecDeque;

pub fn card_frame(p: ThemePalette) -> Frame {
    Frame::new()
        .fill(p.bg_card)
        .stroke(Stroke::new(1.0_f32, p.border_subtle))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::symmetric(20, 16))
        .shadow(p.card_shadow)
}

/// Allocates an exact-size Bento card (`size` outer pixels, `CornerRadius::same(14)`),
/// guaranteeing 0.0px edge alignment across asymmetric grid rows & columns regardless of
/// how short or long the inner content is.
pub fn bento_card_exact<R>(
    ui: &mut egui::Ui,
    size: Vec2,
    pad: Margin,
    border_col: Color32,
    p: ThemePalette,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let (outer_rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    if p.card_shadow != egui::Shadow::NONE {
        painter.add(p.card_shadow.as_shape(outer_rect, CornerRadius::same(14)));
    }
    painter.rect(
        outer_rect,
        CornerRadius::same(14),
        p.bg_card,
        Stroke::new(1.0_f32, border_col),
        StrokeKind::Inside,
    );

    let inner_rect = egui::Rect::from_min_max(
        Pos2::new(
            outer_rect.min.x + pad.left as f32,
            outer_rect.min.y + pad.top as f32,
        ),
        Pos2::new(
            (outer_rect.max.x - pad.right as f32).max(outer_rect.min.x + pad.left as f32 + 1.0),
            (outer_rect.max.y - pad.bottom as f32).max(outer_rect.min.y + pad.top as f32 + 1.0),
        ),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner_rect)
            .layout(Layout::top_down(Align::LEFT)),
    );
    child.set_clip_rect(outer_rect);
    add_contents(&mut child)
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

/// Deterministic fixed-height horizontal row (`Align::Center`) that locks `max_rect.height()`
/// upfront so left-side text labels and taller right-side controls share the exact same centerline.
pub fn aligned_row<R>(
    ui: &mut egui::Ui,
    height: f32,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let w = ui.available_width().max(1.0);
    ui.allocate_ui_with_layout(
        Vec2::new(w, height),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(w);
            ui.set_min_height(height);
            add_contents(ui)
        },
    )
}

/// Truncates `text` to fit within `max_width` pixels using binary search over glyph metrics,
/// appending `…` only when needed.
pub fn elide_text(ui: &egui::Ui, text: &str, font: FontId, max_width: f32) -> String {
    if max_width <= 8.0 || text.is_empty() {
        return String::new();
    }
    let full_w = ui.fonts(|f| {
        f.layout_no_wrap(text.to_string(), font.clone(), Color32::WHITE)
            .size()
            .x
    });
    if full_w <= max_width {
        return text.to_string();
    }
    let ellipsis = "…";
    let ell_w = ui.fonts(|f| {
        f.layout_no_wrap(ellipsis.to_string(), font.clone(), Color32::WHITE)
            .size()
            .x
    });
    let target_w = (max_width - ell_w).max(0.0);
    let chars: Vec<char> = text.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect();
        let w = ui.fonts(|f| {
            f.layout_no_wrap(candidate, font.clone(), Color32::WHITE)
                .size()
                .x
        });
        if w <= target_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let prefix: String = chars[..lo].iter().collect();
    format!("{prefix}{ellipsis}")
}

/// Borderless recessed-well segmented control matching `v03_cockpit_preview.html`:
/// soft `--c-well` track without outer stroke, crisp white/elevated active pill with bold ink.
pub fn segmented_control<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    id_salt: &str,
    selected: T,
    options: &[(T, &str, Option<&str>)],
    size: Vec2,
    font_size: f32,
    p: ThemePalette,
) -> Option<T> {
    let (pill_rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter()
        .rect_filled(pill_rect, CornerRadius::same(8), p.bg_card_alt);

    let n = options.len().max(1) as f32;
    let pad = 2.5_f32;
    let gap = 2.0_f32;
    let seg_w = ((size.x - pad * 2.0 - gap * (n - 1.0)) / n).max(16.0);
    let seg_h = (size.y - pad * 2.0).max(14.0);

    let mut picked = None;
    for (idx, &(val, lbl, tip)) in options.iter().enumerate() {
        let x = pill_rect.left() + pad + idx as f32 * (seg_w + gap);
        let seg_rect =
            egui::Rect::from_min_size(Pos2::new(x, pill_rect.top() + pad), Vec2::new(seg_w, seg_h));
        let mut resp = ui.interact(seg_rect, ui.id().with((id_salt, idx)), Sense::click());
        if let Some(tip_text) = tip {
            resp = resp.on_hover_text(tip_text);
        }
        let active = selected == val;
        let painter = ui.painter();
        if active {
            if !p.is_dark {
                let sh = egui::Shadow {
                    offset: [0, 1],
                    blur: 2,
                    spread: 0,
                    color: Color32::from_black_alpha(15),
                };
                painter.add(sh.as_shape(seg_rect, CornerRadius::same(6)));
                painter.rect_filled(seg_rect, CornerRadius::same(6), p.bg_card);
            } else {
                painter.rect(
                    seg_rect,
                    CornerRadius::same(6),
                    p.bg_card,
                    Stroke::new(1.0_f32, p.border_subtle),
                    StrokeKind::Inside,
                );
            }
        } else if resp.hovered() {
            painter.rect_filled(seg_rect, CornerRadius::same(6), p.bg_nav_hover);
        }

        let fg = if active { p.text_primary } else { p.text_muted };
        let font = if active {
            font_bold(font_size)
        } else {
            FontId::proportional(font_size)
        };
        let galley = ui.fonts(|f| f.layout_no_wrap(lbl.to_string(), font, fg));
        painter.galley(
            Pos2::new(
                seg_rect.center().x - galley.size().x * 0.5,
                seg_rect.center().y - galley.size().y * 0.5,
            ),
            galley,
            fg,
        );
        if resp.clicked() && !active {
            picked = Some(val);
        }
    }
    picked
}

/// Unified primary Terracotta button (consistent hover & press feedback everywhere).
pub fn primary_btn(ui: &mut egui::Ui, text: &str, p: ThemePalette) -> egui::Response {
    let font = font_bold(FONT_SM);
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

/// Unified secondary outlined button.
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
pub fn chip_btn(ui: &mut egui::Ui, text: &str, p: ThemePalette, is_danger: bool) -> egui::Response {
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

    let fill = if !enabled {
        p.bg_card_alt
    } else if is_danger && resp.hovered() {
        p.danger_soft
    } else if resp.is_pointer_button_down_on() || resp.hovered() {
        p.bg_nav_hover
    } else {
        p.bg_card_alt
    };

    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(6), fill);
    if is_danger && resp.hovered() {
        painter.rect_stroke(
            rect,
            CornerRadius::same(6),
            Stroke::new(1.0_f32, p.danger_text),
            StrokeKind::Inside,
        );
    }
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
    let font = font_bold(FONT_SM);
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
        Vec2::new(galley.size().x + 14.0, galley.size().y + 5.0),
        Sense::hover(),
    );
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(6), bg);
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
    let font_id = font_bold(FONT_XS);
    let galley = ui.fonts(|f| f.layout_no_wrap(text.to_string(), font_id, fg));
    let w = galley.size().x + 26.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 24.0), Sense::hover());
    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(12),
        bg,
        Stroke::new(1.0_f32, stroke_col),
        StrokeKind::Inside,
    );
    painter.circle_filled(Pos2::new(rect.left() + 10.5, rect.center().y), 3.1, dot_col);
    painter.galley(
        Pos2::new(rect.left() + 18.0, rect.center().y - galley.size().y * 0.5),
        galley,
        fg,
    );
}

/// Hero Deck Action Button (`138 x 46` locked slot, `CornerRadius::same(12)`), matching
/// `v03_cockpit_preview.html` pixel-for-pixel (Norman State-vs-Action separation).
pub fn draw_hero_action_button(
    ui: &mut egui::Ui,
    state: ConnState,
    has_nodes: bool,
    p: ThemePalette,
) -> egui::Response {
    let desired = Vec2::new(138.0, 46.0);
    let (rect, resp) = ui.allocate_exact_size(desired, Sense::click());
    let painter = ui.painter();

    let (fill, border_stroke, fg_col, label) = match state {
        ConnState::Disconnected => {
            let base = p.accent;
            let f = if resp.is_pointer_button_down_on() {
                base.linear_multiply(0.90)
            } else if resp.hovered() {
                base.linear_multiply(1.05)
            } else {
                base
            };
            let lbl = if has_nodes { "开启加速" } else { "添加节点" };
            (f, Stroke::new(1.0_f32, base), Color32::WHITE, lbl)
        }
        ConnState::Connecting => {
            let f = if resp.hovered() {
                p.warn_soft.linear_multiply(0.96)
            } else {
                p.warn_soft
            };
            (f, Stroke::new(1.0_f32, p.warn_text), p.warn_text, "取消连接")
        }
        ConnState::Connected => {
            if resp.hovered() {
                (
                    p.danger_soft,
                    Stroke::new(1.0_f32, p.danger_text),
                    p.danger_text,
                    "断开连接",
                )
            } else {
                (
                    p.bg_card_alt,
                    Stroke::new(1.0_f32, p.border_strong),
                    p.text_primary,
                    "断开连接",
                )
            }
        }
    };

    painter.rect(
        rect,
        CornerRadius::same(12),
        fill,
        border_stroke,
        StrokeKind::Inside,
    );

    let font_id = font_bold(14.0);
    let galley = ui.fonts(|f| f.layout_no_wrap(label.to_string(), font_id, fg_col));
    let icon_w = 15.0;
    let gap = 7.5;
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

fn draw_idle_chart_grid(
    painter: &egui::Painter,
    rect: egui::Rect,
    labels: [&str; 3],
    hint: &str,
    p: ThemePalette,
) {
    let left_pad = 48.0;
    let inner = egui::Rect::from_min_max(
        Pos2::new(rect.left() + left_pad, rect.top() + 8.0),
        Pos2::new(rect.right() - 6.0, rect.bottom() - 8.0),
    );
    let grid_col = if p.is_dark {
        Color32::from_white_alpha(12)
    } else {
        Color32::from_black_alpha(11)
    };
    for (i, lbl) in labels.iter().enumerate() {
        let frac = i as f32 * 0.5;
        let y = inner.top() + frac * inner.height();
        painter.line_segment(
            [Pos2::new(inner.left(), y), Pos2::new(inner.right(), y)],
            Stroke::new(1.0_f32, grid_col),
        );
        painter.text(
            Pos2::new(inner.left() - 6.0, y),
            egui::Align2::RIGHT_CENTER,
            *lbl,
            FontId::monospace(FONT_XS),
            p.text_muted.gamma_multiply(0.75),
        );
    }
    painter.text(
        inner.center(),
        egui::Align2::CENTER_CENTER,
        hint,
        FontId::proportional(FONT_SM),
        p.text_muted,
    );
}

pub fn draw_rtt_sparkline(
    ui: &mut egui::Ui,
    history: &VecDeque<u64>,
    is_connected: bool,
    p: ThemePalette,
) {
    let height = ui.available_height().max(72.0);
    let desired = Vec2::new(ui.available_width(), height);
    let (rect, _resp) = ui.allocate_exact_size(desired, Sense::hover());
    let painter = ui.painter_at(rect);

    if !is_connected || history.len() < 2 {
        draw_idle_chart_grid(
            &painter,
            rect,
            ["100 ms", "50 ms", "0 ms"],
            "连接开启后实时绘制延迟曲线",
            p,
        );
        return;
    }

    let peak = history.iter().copied().max().unwrap_or(100).max(50);
    let max_val = ((peak as f32 * 1.35) / 10.0).ceil() * 10.0;
    let left_pad = 46.0;
    let inner = egui::Rect::from_min_max(
        Pos2::new(rect.left() + left_pad, rect.top() + 8.0),
        Pos2::new(rect.right() - 6.0, rect.bottom() - 8.0),
    );
    let step_x = inner.width() / (history.len().saturating_sub(1).max(1) as f32);

    let grid_col = if p.is_dark {
        Color32::from_white_alpha(14)
    } else {
        Color32::from_black_alpha(12)
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

    let line_col = p.success_text;
    let mut mesh = egui::Mesh::default();
    let top_col = line_col.gamma_multiply(if p.is_dark { 0.20 } else { 0.14 });
    let bot_col = line_col.gamma_multiply(0.0);
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

    painter.add(egui::Shape::line(
        points.clone(),
        Stroke::new(2.0_f32, line_col),
    ));
    if let Some(&last_pt) = points.last() {
        painter.circle_filled(last_pt, 5.5, line_col.gamma_multiply(0.22));
        painter.circle_filled(last_pt, 3.0, line_col);
    }
}

/// Real-time traffic throughput sparkline using calm Slate-Ink (`p.data_primary` / `p.data_secondary`).
pub fn draw_traffic_sparkline(
    ui: &mut egui::Ui,
    history: &VecDeque<(f64, f64)>,
    is_connected: bool,
    p: ThemePalette,
) {
    let height = ui.available_height().max(72.0);
    let desired = Vec2::new(ui.available_width(), height);
    let (rect, _resp) = ui.allocate_exact_size(desired, Sense::hover());
    let painter = ui.painter_at(rect);

    if !is_connected || history.len() < 2 {
        draw_idle_chart_grid(
            &painter,
            rect,
            ["16 KB/s", "8 KB/s", "0 B/s"],
            "连接开启后实时绘制链路曲线",
            p,
        );
        return;
    }

    let peak_bps = history
        .iter()
        .map(|&(rx, tx)| rx.max(tx))
        .fold(0.0_f64, f64::max);
    let scale_max = (peak_bps * 1.30).max(16.0 * 1024.0);

    let left_pad = 58.0;
    let inner = egui::Rect::from_min_max(
        Pos2::new(rect.left() + left_pad, rect.top() + 8.0),
        Pos2::new(rect.right() - 6.0, rect.bottom() - 8.0),
    );
    let step_x = inner.width() / (history.len().saturating_sub(1).max(1) as f32);

    let grid_col = if p.is_dark {
        Color32::from_white_alpha(14)
    } else {
        Color32::from_black_alpha(12)
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

    let mut mesh = egui::Mesh::default();
    let top_col = p.data_primary.gamma_multiply(if p.is_dark { 0.22 } else { 0.14 });
    let bot_col = p.data_primary.gamma_multiply(0.0);
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

    painter.add(egui::Shape::line(
        tx_points,
        Stroke::new(1.5_f32, p.data_secondary),
    ));
    painter.add(egui::Shape::line(
        rx_points.clone(),
        Stroke::new(2.0_f32, p.data_primary),
    ));
    if let Some(&last_pt) = rx_points.last() {
        painter.circle_filled(last_pt, 5.2, p.data_primary.gamma_multiply(0.24));
        painter.circle_filled(last_pt, 2.8, p.data_primary);
    }
}

/// Renders a centered floating modal dialog with a dimmed backdrop, Esc/backdrop/close handling.
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
            aligned_row(ui, 28.0, |ui| {
                ui.label(
                    egui::RichText::new(title)
                        .font(font_bold(FONT_SUB))
                        .color(p.text_primary),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (x_rect, x_resp) =
                        ui.allocate_exact_size(Vec2::splat(26.0), Sense::click());
                    if x_resp.hovered() {
                        ui.painter()
                            .rect_filled(x_rect, CornerRadius::same(7), p.bg_nav_hover);
                    }
                    icons::draw_win_close_icon(
                        ui.painter(),
                        x_rect.center(),
                        if x_resp.hovered() {
                            p.text_primary
                        } else {
                            p.text_secondary
                        },
                    );
                    if x_resp.clicked() {
                        close_requested = true;
                    }
                });
            });
            ui.add_space(12.0);
            add_contents(ui);
        });

    close_requested
}

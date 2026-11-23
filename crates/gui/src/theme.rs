use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Shadow, Stroke, Vec2,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Strict integer typography ladder so Microsoft YaHei stays sharp on Windows DPI scaling.
pub const FONT_XS: f32 = 11.0;
pub const FONT_SM: f32 = 12.0;
pub const FONT_BODY: f32 = 13.0;
pub const FONT_SUB: f32 = 15.0;
pub const FONT_TITLE: f32 = 18.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeMode {
    Light,
    Dark,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct ThemePalette {
    pub is_dark: bool,
    pub bg_root: Color32,
    pub bg_sidebar: Color32,
    pub bg_card: Color32,
    pub bg_card_alt: Color32,
    pub bg_input: Color32,
    pub bg_nav_active: Color32,
    pub bg_nav_hover: Color32,
    pub border_subtle: Color32,
    pub border_strong: Color32,
    pub card_shadow: Shadow,
    pub text_primary: Color32,
    pub text_secondary: Color32,
    pub text_muted: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub accent_text: Color32,
    pub success_soft: Color32,
    pub success_text: Color32,
    pub warn_soft: Color32,
    pub warn_text: Color32,
    pub danger: Color32,
    pub danger_soft: Color32,
    pub danger_text: Color32,
    pub tag_bg: Color32,
    pub tag_text: Color32,
}

impl ThemePalette {
    pub fn from_mode(mode: ThemeMode) -> Self {
        match mode {
            // Warm Linen & Terracotta (Light)
            ThemeMode::Light => Self {
                is_dark: false,
                bg_root: Color32::from_rgb(250, 249, 245),
                bg_sidebar: Color32::from_rgb(243, 241, 234),
                bg_card: Color32::from_rgb(255, 255, 255),
                bg_card_alt: Color32::from_rgb(246, 244, 238),
                bg_input: Color32::from_rgb(255, 255, 255),
                bg_nav_active: Color32::from_rgb(233, 230, 221),
                bg_nav_hover: Color32::from_rgb(239, 236, 228),
                border_subtle: Color32::from_rgb(232, 229, 220),
                border_strong: Color32::from_rgb(211, 206, 194),
                card_shadow: Shadow {
                    offset: [0, 2],
                    blur: 8,
                    spread: 0,
                    color: Color32::from_black_alpha(10),
                },
                text_primary: Color32::from_rgb(30, 29, 27),
                text_secondary: Color32::from_rgb(77, 75, 70),
                text_muted: Color32::from_rgb(128, 124, 115),
                accent: Color32::from_rgb(217, 119, 87),
                accent_soft: Color32::from_rgb(248, 234, 227),
                accent_text: Color32::from_rgb(179, 77, 46),
                success_soft: Color32::from_rgb(231, 244, 238),
                success_text: Color32::from_rgb(29, 107, 74),
                warn_soft: Color32::from_rgb(254, 243, 219),
                warn_text: Color32::from_rgb(161, 98, 7),
                danger: Color32::from_rgb(198, 63, 59),
                danger_soft: Color32::from_rgb(252, 235, 234),
                danger_text: Color32::from_rgb(168, 44, 41),
                tag_bg: Color32::from_rgb(239, 236, 228),
                tag_text: Color32::from_rgb(94, 91, 84),
            },
            // Warm Charcoal & Terracotta (Dark)
            ThemeMode::Dark => Self {
                is_dark: true,
                bg_root: Color32::from_rgb(38, 38, 36),
                bg_sidebar: Color32::from_rgb(31, 30, 29),
                bg_card: Color32::from_rgb(47, 46, 43),
                bg_card_alt: Color32::from_rgb(55, 54, 50),
                bg_input: Color32::from_rgb(33, 32, 30),
                bg_nav_active: Color32::from_rgb(54, 53, 49),
                bg_nav_hover: Color32::from_rgb(44, 43, 40),
                border_subtle: Color32::from_rgb(65, 63, 58),
                border_strong: Color32::from_rgb(90, 87, 80),
                card_shadow: Shadow::NONE,
                text_primary: Color32::from_rgb(242, 240, 233),
                text_secondary: Color32::from_rgb(206, 202, 193),
                text_muted: Color32::from_rgb(162, 158, 149),
                accent: Color32::from_rgb(217, 119, 87),
                accent_soft: Color32::from_rgb(74, 48, 39),
                accent_text: Color32::from_rgb(242, 165, 138),
                success_soft: Color32::from_rgb(32, 62, 49),
                success_text: Color32::from_rgb(134, 224, 184),
                warn_soft: Color32::from_rgb(68, 53, 25),
                warn_text: Color32::from_rgb(250, 204, 100),
                danger: Color32::from_rgb(214, 79, 75),
                danger_soft: Color32::from_rgb(76, 36, 36),
                danger_text: Color32::from_rgb(246, 158, 155),
                tag_bg: Color32::from_rgb(60, 59, 55),
                tag_text: Color32::from_rgb(196, 192, 183),
            },
        }
    }
}

/// Shared normalized brand lightning geometry (2 convex triangles in [-1, 1] space)
/// used by both the in-window vector brand mark and the 32x32 taskbar/tray icon.
pub const BRAND_BOLT_UPPER: [(f32, f32); 3] = [(0.16, -0.66), (-0.54, 0.08), (0.08, 0.08)];
pub const BRAND_BOLT_LOWER: [(f32, f32); 3] = [(-0.16, 0.66), (0.54, -0.08), (-0.08, -0.08)];

fn point_in_triangle(px: f32, py: f32, tri: [(f32, f32); 3]) -> bool {
    let (x1, y1) = tri[0];
    let (x2, y2) = tri[1];
    let (x3, y3) = tri[2];
    let d1 = (px - x2) * (y1 - y2) - (x1 - x2) * (py - y2);
    let d2 = (px - x3) * (y2 - y3) - (x2 - x3) * (py - y3);
    let d3 = (px - x1) * (y3 - y1) - (x3 - x1) * (py - y1);
    let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_neg && has_pos)
}

/// Generate a 32x32 anti-aliased RGBA taskbar/tray icon matching `icons::draw_brand_logo` identically.
pub fn build_app_icon() -> egui::IconData {
    let size = 32usize;
    let mut rgba = vec![0u8; size * size * 4];
    let bg = [217.0_f32, 119.0, 87.0];
    let fg = [255.0_f32, 255.0, 255.0];

    // 4x4 supersampling for crisp anti-aliased rounded corners & lightning silhouette
    let half = size as f32 * 0.5;
    let corner_r = size as f32 * 0.25; // matches 8px corner radius on 32px logo
    let inner_box = half - corner_r;

    for y in 0..size {
        for x in 0..size {
            let mut r_acc = 0.0_f32;
            let mut g_acc = 0.0_f32;
            let mut b_acc = 0.0_f32;
            let mut a_acc = 0.0_f32;

            for sy in 0..4 {
                for sx in 0..4 {
                    let fx = x as f32 + (sx as f32 + 0.5) * 0.25;
                    let fy = y as f32 + (sy as f32 + 0.5) * 0.25;
                    let dx = (fx - half).abs() - inner_box;
                    let dy = (fy - half).abs() - inner_box;
                    let dist = dx.max(0.0).hypot(dy.max(0.0));
                    if dist <= corner_r {
                        let nx = (fx - half) / half;
                        let ny = (fy - half) / half;
                        let in_bolt = point_in_triangle(nx, ny, BRAND_BOLT_UPPER)
                            || point_in_triangle(nx, ny, BRAND_BOLT_LOWER);
                        let col = if in_bolt { fg } else { bg };
                        r_acc += col[0];
                        g_acc += col[1];
                        b_acc += col[2];
                        a_acc += 255.0;
                    }
                }
            }

            let idx = (y * size + x) * 4;
            if a_acc > 0.0 {
                let samples = a_acc / 255.0;
                rgba[idx] = (r_acc / samples).round() as u8;
                rgba[idx + 1] = (g_acc / samples).round() as u8;
                rgba[idx + 2] = (b_acc / samples).round() as u8;
                rgba[idx + 3] = (a_acc / 16.0).round() as u8;
            }
        }
    }

    egui::IconData {
        rgba,
        width: size as u32,
        height: size as u32,
    }
}

pub fn configure_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    // Real tabular monospace font first for IP:port columns and log timestamps
    let mono_candidates = [
        r"C:\Windows\Fonts\consola.ttf",
        r"C:\Windows\Fonts\CascadiaMono.ttf",
        r"C:\Windows\Fonts\lucon.ttf",
    ];
    for path in mono_candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let data = Arc::new(FontData::from_owned(bytes));
            fonts.font_data.insert("win_mono".to_owned(), data);
            fonts
                .families
                .entry(FontFamily::Monospace)
                .or_default()
                .insert(0, "win_mono".to_owned());
            break;
        }
    }

    // Microsoft YaHei for proportional UI copy, and appended at the end of Monospace only as CJK fallback
    let cn_candidates = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
    ];
    for path in cn_candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let data = Arc::new(FontData::from_owned(bytes));
            fonts.font_data.insert("cn_font".to_owned(), data);
            fonts
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .insert(0, "cn_font".to_owned());
            fonts
                .families
                .entry(FontFamily::Monospace)
                .or_default()
                .push("cn_font".to_owned());
            break;
        }
    }

    ctx.set_fonts(fonts);
}

pub fn apply_theme(ctx: &egui::Context, p: ThemePalette) {
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.spacing.button_padding = Vec2::new(12.0, 6.0);

    let mut scroll = egui::style::ScrollStyle::floating();
    scroll.bar_width = 6.0;
    scroll.floating_width = 2.5;
    scroll.floating_allocated_width = 0.0;
    scroll.dormant_handle_opacity = 0.12;
    scroll.active_handle_opacity = 0.28;
    scroll.interact_handle_opacity = 0.50;
    style.spacing.scroll = scroll;

    let mut visuals = if p.is_dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    visuals.panel_fill = p.bg_root;
    visuals.window_fill = p.bg_card;
    visuals.extreme_bg_color = p.bg_input;
    visuals.selection.bg_fill = p.accent_soft;
    visuals.selection.stroke = Stroke::new(1.0_f32, p.accent);

    visuals.widgets.noninteractive.bg_fill = p.bg_card;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.border_subtle);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.text_primary);

    // Unified neutral hover/active states matching custom widgets across all pages
    visuals.widgets.inactive.bg_fill = p.bg_card_alt;
    visuals.widgets.inactive.weak_bg_fill = p.bg_card_alt;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, p.border_subtle);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, p.text_primary);
    visuals.widgets.inactive.corner_radius = CornerRadius::same(8);

    visuals.widgets.hovered.bg_fill = p.bg_nav_hover;
    visuals.widgets.hovered.weak_bg_fill = p.bg_nav_hover;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.border_strong);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, p.text_primary);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(8);

    visuals.widgets.active.bg_fill = p.bg_nav_active;
    visuals.widgets.active.weak_bg_fill = p.bg_nav_active;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, p.border_strong);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, p.text_primary);
    visuals.widgets.active.corner_radius = CornerRadius::same(8);

    visuals.widgets.open.bg_fill = p.bg_nav_active;
    visuals.widgets.open.weak_bg_fill = p.bg_nav_active;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, p.border_strong);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0_f32, p.text_primary);
    visuals.widgets.open.corner_radius = CornerRadius::same(8);

    style.visuals = visuals;
    ctx.set_style(style);
}

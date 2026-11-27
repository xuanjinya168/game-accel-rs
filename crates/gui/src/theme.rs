use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Shadow, Stroke,
    Vec2,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Strict typography ladder tuned for Microsoft YaHei & Consolas on Windows DPI scaling.
pub const FONT_XS: f32 = 11.0;
pub const FONT_SM: f32 = 12.0;
pub const FONT_BODY: f32 = 13.0;
pub const FONT_SUB: f32 = 15.0;
pub const FONT_TITLE: f32 = 18.0;

/// Proportional UI font (`msyh.ttc` regular — avoids `msyhbd.ttc` stroke bloat in egui grayscale atlas).
pub fn font_bold(size: f32) -> FontId {
    FontId::proportional(size)
}

/// Tabular monospace font (`consola.ttf` regular — crisp strokes at 11px–18px).
pub fn font_mono_bold(size: f32) -> FontId {
    FontId::monospace(size)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeMode {
    Light,
    Dark,
}

/// Norman 1-to-1 Semantic Color System (*The Design of Everyday Things*),
/// matching `v03_cockpit_preview.html` CSS variables 1-to-1:
/// - `accent` (Terracotta): Brand mark & idle `开启加速` CTA ONLY
/// - `success_*` (Emerald): Connected tunnel & healthy low-latency link ONLY
/// - `warn_*` (Amber): Connecting / Reconnecting / Elevated jitter or latency ONLY
/// - `danger_*` (Vermilion): Fault / Timeout / Disconnect hover / Destructive action ONLY
/// - `data_primary` / `data_secondary` (Slate Ink / Sage): Neutral throughput curves ONLY
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
    pub data_primary: Color32,
    pub data_secondary: Color32,
    pub tag_bg: Color32,
    pub tag_text: Color32,
}

impl ThemePalette {
    pub fn from_mode(mode: ThemeMode) -> Self {
        match mode {
            // Warm Studio Linen (exact match to v03_cockpit_preview.html [data-theme="light"])
            ThemeMode::Light => Self {
                is_dark: false,
                bg_root: Color32::from_rgb(246, 244, 238),    // #F6F4EE
                bg_sidebar: Color32::from_rgb(237, 234, 225), // #EDEAE1
                bg_card: Color32::from_rgb(255, 255, 255),    // #FFFFFF
                bg_card_alt: Color32::from_rgb(239, 236, 228), // #EFECE4 (--c-well)
                bg_input: Color32::from_rgb(255, 255, 255),
                bg_nav_active: Color32::from_rgb(255, 255, 255),
                bg_nav_hover: Color32::from_rgb(231, 227, 216), // #E7E3D8 (--c-hover)
                border_subtle: Color32::from_rgb(231, 228, 219), // rgba(28,27,25,0.08) on canvas
                border_strong: Color32::from_rgb(211, 207, 196), // rgba(28,27,25,0.16)
                card_shadow: Shadow {
                    offset: [0, 1],
                    blur: 4,
                    spread: 0,
                    color: Color32::from_black_alpha(8),
                },
                text_primary: Color32::from_rgb(27, 26, 24),   // #1B1A18
                text_secondary: Color32::from_rgb(81, 78, 72), // #514E48
                text_muted: Color32::from_rgb(111, 106, 96),   // #6F6A60 (>=4.5:1 on canvas/card/tag)
                accent: Color32::from_rgb(217, 119, 87),       // #D97757
                accent_soft: Color32::from_rgb(249, 235, 228), // #F9EBE4
                accent_text: Color32::from_rgb(173, 72, 40),   // #AD4828
                success_soft: Color32::from_rgb(226, 243, 236), // #E2F3EC
                success_text: Color32::from_rgb(21, 122, 80),  // #157A50
                warn_soft: Color32::from_rgb(253, 241, 220),   // #FDF1DC
                warn_text: Color32::from_rgb(180, 105, 14),    // #B4690E
                danger: Color32::from_rgb(196, 59, 55),        // #C43B37
                danger_soft: Color32::from_rgb(252, 234, 233), // #FCEAE9
                danger_text: Color32::from_rgb(196, 59, 55),
                data_primary: Color32::from_rgb(73, 99, 122),    // #49637A
                data_secondary: Color32::from_rgb(127, 161, 142), // #7FA18E
                tag_bg: Color32::from_rgb(239, 236, 228),
                tag_text: Color32::from_rgb(111, 106, 96), // #6F6A60
            },
            // Studio Obsidian (exact match to v03_cockpit_preview.html [data-theme="dark"])
            ThemeMode::Dark => Self {
                is_dark: true,
                bg_root: Color32::from_rgb(29, 29, 27),       // #1D1D1B
                bg_sidebar: Color32::from_rgb(22, 22, 21),    // #161615
                bg_card: Color32::from_rgb(38, 37, 35),       // #262523
                bg_card_alt: Color32::from_rgb(24, 24, 22),   // #181816 (--c-well)
                bg_input: Color32::from_rgb(24, 24, 22),
                bg_nav_active: Color32::from_rgb(38, 37, 35), // #262523 (--c-surface)
                bg_nav_hover: Color32::from_rgb(49, 48, 44),  // #31302C (--c-hover)
                border_subtle: Color32::from_rgb(53, 52, 48),
                border_strong: Color32::from_rgb(74, 72, 66),
                card_shadow: Shadow::NONE,
                text_primary: Color32::from_rgb(243, 241, 235),   // #F3F1EB
                text_secondary: Color32::from_rgb(196, 192, 183), // #C4C0B7
                text_muted: Color32::from_rgb(139, 135, 126),     // #8B877E
                accent: Color32::from_rgb(217, 119, 87),          // #D97757
                accent_soft: Color32::from_rgb(66, 43, 35),       // #422B23
                accent_text: Color32::from_rgb(243, 166, 140),    // #F3A68C
                success_soft: Color32::from_rgb(27, 54, 43),      // #1B362B
                success_text: Color32::from_rgb(71, 209, 149),    // #47D195
                warn_soft: Color32::from_rgb(59, 46, 24),         // #3B2E18
                warn_text: Color32::from_rgb(244, 183, 82),       // #F4B752
                danger: Color32::from_rgb(240, 121, 117),         // #F07975
                danger_soft: Color32::from_rgb(64, 32, 32),       // #402020
                danger_text: Color32::from_rgb(240, 121, 117),
                data_primary: Color32::from_rgb(136, 166, 194),   // #88A6C2
                data_secondary: Color32::from_rgb(104, 184, 148), // #68B894
                tag_bg: Color32::from_rgb(24, 24, 22),
                tag_text: Color32::from_rgb(139, 135, 126),
            },
        }
    }
}

/// Shared normalized sleek brand lightning geometry in `[-1, 1]` space.
/// `BRAND_BOLT_UPPER ∪ BRAND_BOLT_LOWER` has the exact 6-vertex boundary `BRAND_BOLT_OUTLINE`;
/// `BRAND_BOLT_BRIDGE` covers their interior diagonal seam against AA feathering.
pub const BRAND_BOLT_UPPER: [(f32, f32); 3] = [(0.14, -0.68), (-0.50, 0.07), (0.00, 0.07)];
pub const BRAND_BOLT_LOWER: [(f32, f32); 3] = [(-0.14, 0.68), (0.50, -0.07), (0.00, -0.07)];
pub const BRAND_BOLT_BRIDGE: [(f32, f32); 4] = [
    (-0.07, -0.055),
    (0.07, -0.055),
    (0.07, 0.055),
    (-0.07, 0.055),
];
pub const BRAND_BOLT_OUTLINE: [(f32, f32); 6] = [
    (0.14, -0.68),
    (-0.50, 0.07),
    (-0.0261, 0.07),
    (-0.14, 0.68),
    (0.50, -0.07),
    (0.0261, -0.07),
];

pub(crate) fn point_in_triangle(px: f32, py: f32, tri: [(f32, f32); 3]) -> bool {
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

/// Generate a 32x32 anti-aliased RGBA taskbar/tray icon matching `icons::draw_brand_logo`
/// (top-lit terracotta gradient `#E38667 -> #CC6848` + crisp inner highlight + sleek solid bolt).
pub fn build_app_icon() -> egui::IconData {
    let size = 32usize;
    let mut rgba = vec![0u8; size * size * 4];
    let bg_top = [227.0_f32, 134.0, 103.0]; // #E38667
    let bg_bot = [204.0_f32, 104.0, 72.0]; // #CC6848
    let fg = [255.0_f32, 255.0, 255.0];

    let half = size as f32 * 0.5;
    let corner_r = size as f32 * 0.25;
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
                        let col = if in_bolt {
                            fg
                        } else {
                            let ty = (fy / size as f32).clamp(0.0, 1.0);
                            let mut c = [
                                bg_top[0] + (bg_bot[0] - bg_top[0]) * ty,
                                bg_top[1] + (bg_bot[1] - bg_top[1]) * ty,
                                bg_top[2] + (bg_bot[2] - bg_top[2]) * ty,
                            ];
                            // Subtle 1px inner rim highlight matching `draw_brand_logo`
                            let edge_dist = if dx > 0.0 && dy > 0.0 {
                                corner_r - dist
                            } else {
                                (half - (fx - half).abs()).min(half - (fy - half).abs())
                            };
                            if edge_dist <= 1.05 {
                                let rim = 0.20 * (1.0 - 0.35 * ty);
                                c[0] += (255.0 - c[0]) * rim;
                                c[1] += (255.0 - c[1]) * rim;
                                c[2] += (255.0 - c[2]) * rim;
                            }
                            c
                        };
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

    // Real tabular monospace font first for IP:port columns, speeds, RTTs, and log timestamps
    let mono_candidates = [
        r"C:\Windows\Fonts\consola.ttf",
        r"C:\Windows\Fonts\CascadiaMono.ttf",
        r"C:\Windows\Fonts\lucon.ttf",
    ];
    for path in mono_candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("win_mono".to_owned(), Arc::new(FontData::from_owned(bytes)));
            fonts
                .families
                .entry(FontFamily::Monospace)
                .or_default()
                .insert(0, "win_mono".to_owned());
            break;
        }
    }

    // Crisp Microsoft YaHei Regular (`msyh.ttc`) for UI copy + CJK fallback in Monospace
    let cn_candidates = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
    ];
    for path in cn_candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut prop_data = FontData::from_owned(bytes.clone());
            prop_data.tweak.y_offset_factor = -0.10;
            fonts
                .font_data
                .insert("cn_font".to_owned(), Arc::new(prop_data));
            fonts
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .insert(0, "cn_font".to_owned());

            let mut mono_cn = FontData::from_owned(bytes);
            mono_cn.tweak.y_offset_factor = -0.09;
            fonts
                .font_data
                .insert("cn_font_mono".to_owned(), Arc::new(mono_cn));
            fonts
                .families
                .entry(FontFamily::Monospace)
                .or_default()
                .push("cn_font_mono".to_owned());
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
    scroll.bar_width = 5.0;
    scroll.floating_width = 2.5;
    scroll.floating_allocated_width = 0.0;
    scroll.dormant_handle_opacity = 0.12;
    scroll.active_handle_opacity = 0.28;
    scroll.interact_handle_opacity = 0.48;
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

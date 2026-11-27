use crate::icons;
use crate::storage::{CustomRule, CustomRuleAction, RouteMode, config_dir};
use crate::theme::{FONT_SM, FONT_SUB, FONT_XS, ThemePalette, font_bold};
use crate::widgets::{
    aligned_row, card_frame, elide_text, galley_center_y, pill, primary_btn, segmented_control,
    soft_btn, styled_input_exact, styled_multiline,
};
use crate::GameAccelApp;
use client::rules::{
    RoutePreview, RuleData, format_bulk_custom_rules, parse_bulk_custom_rules_checked,
    preview_route_match,
};
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Frame, Layout, Margin, Pos2, RichText, ScrollArea,
    Sense, Stroke, StrokeKind, Vec2,
};
use std::sync::Arc;

/// Inner content width matching the HTML prototype (`.cr-modal` 590–612px outer card).
const INNER_W: f32 = 576.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorTab {
    List,
    Bulk,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HitTestCacheKey {
    query: String,
    mode: RouteMode,
    tab: EditorTab,
    draft_rev: u64,
    bulk_snapshot: String,
}

pub(crate) struct CustomRulesModalState {
    tab: EditorTab,
    draft_rules: Vec<CustomRule>,
    draft_rev: u64,
    new_pattern: String,
    new_action: CustomRuleAction,
    list_error: Option<String>,
    bulk_text: String,
    bulk_error: Option<String>,
    test_input: String,
    rule_data: Arc<RuleData>,
    cached_key: Option<HitTestCacheKey>,
    cached_preview: Result<Option<RoutePreview>, String>,
}

impl CustomRulesModalState {
    pub(crate) fn new(rules: &[CustomRule], route_mode: RouteMode) -> Self {
        let rules_dir = config_dir().join("rules");
        let default_action = if route_mode == RouteMode::Dual {
            CustomRuleAction::Domestic
        } else {
            CustomRuleAction::Direct
        };
        Self {
            tab: EditorTab::List,
            draft_rules: rules.to_vec(),
            draft_rev: 0,
            new_pattern: String::new(),
            new_action: default_action,
            list_error: None,
            bulk_text: format_bulk_custom_rules(rules),
            bulk_error: None,
            test_input: String::new(),
            rule_data: RuleData::shared(Some(&rules_dir)),
            cached_key: None,
            cached_preview: Ok(None),
        }
    }

    fn eval_preview(&mut self, route_mode: RouteMode) -> &Result<Option<RoutePreview>, String> {
        let trimmed = self.test_input.trim();
        if trimmed.is_empty() {
            self.cached_key = None;
            self.cached_preview = Ok(None);
            return &self.cached_preview;
        }
        let key = HitTestCacheKey {
            query: trimmed.to_string(),
            mode: route_mode,
            tab: self.tab,
            draft_rev: self.draft_rev,
            bulk_snapshot: if self.tab == EditorTab::Bulk {
                self.bulk_text.clone()
            } else {
                String::new()
            },
        };
        if self.cached_key.as_ref() != Some(&key) {
            self.cached_preview = if self.tab == EditorTab::Bulk {
                match parse_bulk_custom_rules_checked(&self.bulk_text) {
                    Ok(parsed) => Ok(preview_route_match(
                        trimmed,
                        route_mode,
                        &parsed,
                        &self.rule_data,
                    )),
                    Err(e) => Err(e),
                }
            } else {
                Ok(preview_route_match(
                    trimmed,
                    route_mode,
                    &self.draft_rules,
                    &self.rule_data,
                ))
            };
            self.cached_key = Some(key);
        }
        &self.cached_preview
    }
}

impl GameAccelApp {
    pub(crate) fn render_custom_rules_modal(&mut self, ctx: &egui::Context, p: ThemePalette) {
        let Some(mut modal) = self.custom_rules_modal.take() else {
            return;
        };

        let mut close_modal = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let mut save_requested = false;
        let route_mode = self.cfg.route_mode;

        // Dimmed backdrop
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new(("custom_rules_modal", "backdrop")))
            .order(egui::Order::Middle)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                let resp = ui.allocate_response(screen.size(), Sense::click());
                ui.painter()
                    .rect_filled(screen, 0.0, Color32::from_black_alpha(120));
                if resp.clicked() {
                    close_modal = true;
                }
            });

        let modal_frame = card_frame(p)
            .corner_radius(CornerRadius::same(12))
            .stroke(Stroke::new(1.0_f32, p.border_strong))
            .inner_margin(Margin::symmetric(18, 16))
            .shadow(egui::Shadow {
                offset: [0, 8],
                blur: 24,
                spread: 0,
                color: Color32::from_black_alpha(if p.is_dark { 120 } else { 70 }),
            });

        egui::Window::new("自定义分流规则")
            .id(egui::Id::new(("custom_rules_modal", "window")))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .collapsible(false)
            .resizable(false)
            .auto_sized()
            .title_bar(false)
            .frame(modal_frame)
            .show(ctx, |ui| {
                ui.set_width(INNER_W);
                ui.spacing_mut().item_spacing.y = 10.0;

                // 1. Clean single-row header: Title + count pill (when non-empty) | [列表 | 批量文本] + [×]
                aligned_row(ui, 28.0, |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(
                        RichText::new("自定义分流规则")
                            .font(font_bold(FONT_SUB))
                            .color(p.text_primary),
                    );
                    let enabled_cnt = modal.draft_rules.iter().filter(|r| r.enabled).count();
                    if enabled_cnt > 0 {
                        pill(
                            ui,
                            &format!("{enabled_cnt} 条生效"),
                            p.success_soft,
                            p.success_text,
                        );
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        let (x_rect, x_resp) =
                            ui.allocate_exact_size(Vec2::splat(26.0), Sense::click());
                        if x_resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        ui.painter().rect(
                            x_rect,
                            CornerRadius::same(7),
                            if x_resp.hovered() {
                                p.bg_nav_hover
                            } else {
                                p.bg_card
                            },
                            Stroke::new(
                                1.0_f32,
                                if x_resp.hovered() {
                                    p.border_strong
                                } else {
                                    p.border_subtle
                                },
                            ),
                            StrokeKind::Inside,
                        );
                        icons::draw_win_close_icon(
                            ui.painter(),
                            x_rect.center(),
                            if x_resp.hovered() {
                                p.text_primary
                            } else {
                                p.text_secondary
                            },
                        );
                        if x_resp.on_hover_text("关闭（不保存草稿）").clicked() {
                            close_modal = true;
                        }

                        if let Some(next_tab) = segmented_control(
                            ui,
                            "custom_rules_tab_seg",
                            modal.tab,
                            &[
                                (EditorTab::List, "列表", None),
                                (EditorTab::Bulk, "批量文本", None),
                            ],
                            Vec2::new(132.0, 26.0),
                            FONT_XS,
                            p,
                        ) {
                            if next_tab == EditorTab::Bulk {
                                modal.bulk_text = format_bulk_custom_rules(&modal.draft_rules);
                                modal.bulk_error = None;
                                modal.tab = EditorTab::Bulk;
                            } else {
                                match parse_bulk_custom_rules_checked(&modal.bulk_text) {
                                    Ok(parsed) => {
                                        modal.draft_rules = parsed;
                                        modal.draft_rev = modal.draft_rev.wrapping_add(1);
                                        modal.bulk_error = None;
                                        modal.list_error = None;
                                        modal.tab = EditorTab::List;
                                    }
                                    Err(err) => {
                                        modal.bulk_error = Some(err);
                                    }
                                }
                            }
                        }
                    });
                });

                if matches!(route_mode, RouteMode::Global | RouteMode::Direct) {
                    Frame::new()
                        .fill(p.warn_soft)
                        .corner_radius(CornerRadius::same(7))
                        .inner_margin(Margin::symmetric(10, 5))
                        .show(ui, |ui| {
                            ui.set_width((INNER_W - 20.0).max(200.0));
                            ui.label(
                                RichText::new(format!(
                                    "当前为「{}」模式，规则仅在「大陆直连 / 双节点」下生效",
                                    route_mode.label()
                                ))
                                .size(FONT_XS)
                                .color(p.warn_text),
                            );
                        });
                }

                // 3. Body: List Pane vs Bulk Text Pane
                match modal.tab {
                    EditorTab::List => {
                        let mut do_add = false;
                        let has_input = !modal.new_pattern.trim().is_empty();
                        let kind_badge = has_input.then(|| {
                            let valid =
                                CustomRule::normalize_pattern(&modal.new_pattern).is_some();
                            let lbl = if valid {
                                CustomRule::kind_label(&modal.new_pattern)
                            } else {
                                "格式待完善"
                            };
                            (lbl, valid)
                        });

                        aligned_row(ui, 30.0, |ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            let seg_w = 168.0;
                            let btn_w = 76.0;
                            let inline_kind = kind_badge.map(|(lbl, valid)| {
                                let fg = if valid {
                                    p.text_secondary
                                } else {
                                    p.danger_text
                                };
                                let g = ui.fonts(|f| {
                                    f.layout_no_wrap(
                                        lbl.to_string(),
                                        FontId::proportional(10.5),
                                        fg,
                                    )
                                });
                                let w = g.size().x + 14.0;
                                (g, w, valid, fg)
                            });
                            let extra_kind_w =
                                inline_kind.as_ref().map_or(0.0, |(_, w, _, _)| *w + 6.0);
                            let input_w =
                                (INNER_W - seg_w - btn_w - 12.0 - extra_kind_w).max(140.0);

                            let resp = styled_input_exact(
                                ui,
                                &mut modal.new_pattern,
                                "域名 / full:精确域名 / IP / CIDR",
                                Vec2::new(input_w, 30.0),
                                FONT_SM,
                                false,
                                p,
                            );
                            if resp.changed() {
                                modal.list_error = None;
                            }
                            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                do_add = true;
                            }

                            if let Some((g, w, valid, fg)) = inline_kind {
                                let (kr, _) = ui.allocate_exact_size(
                                    Vec2::new(w, 22.0),
                                    Sense::hover(),
                                );
                                let (bg, bd) = if valid {
                                    (p.bg_card, p.border_subtle)
                                } else {
                                    (p.danger_soft, p.danger_text.gamma_multiply(0.4))
                                };
                                ui.painter().rect(
                                    kr,
                                    CornerRadius::same(11),
                                    bg,
                                    Stroke::new(1.0_f32, bd),
                                    StrokeKind::Inside,
                                );
                                let ty = galley_center_y(kr, &g);
                                ui.painter().galley(
                                    Pos2::new(kr.center().x - g.size().x * 0.5, ty),
                                    g,
                                    fg,
                                );
                            }

                            if let Some(act) = segmented_control(
                                ui,
                                "custom_rule_new_act",
                                modal.new_action,
                                &[
                                    (CustomRuleAction::Direct, "直连", None),
                                    (CustomRuleAction::Domestic, "国内节点", None),
                                    (CustomRuleAction::Primary, "主节点", None),
                                ],
                                Vec2::new(seg_w, 30.0),
                                FONT_XS,
                                p,
                            ) {
                                modal.new_action = act;
                            }

                            if primary_btn(ui, "+ 添加", p).clicked() {
                                do_add = true;
                            }
                        });

                        if do_add {
                            if let Some(pattern) = CustomRule::normalize_pattern(&modal.new_pattern)
                            {
                                if let Some(existing) = modal
                                    .draft_rules
                                    .iter_mut()
                                    .find(|r| r.pattern == pattern)
                                {
                                    existing.action = modal.new_action;
                                    existing.enabled = true;
                                } else {
                                    modal.draft_rules.insert(
                                        0,
                                        CustomRule {
                                            pattern,
                                            action: modal.new_action,
                                            enabled: true,
                                        },
                                    );
                                }
                                modal.new_pattern.clear();
                                modal.list_error = None;
                                modal.draft_rev = modal.draft_rev.wrapping_add(1);
                            } else {
                                modal.list_error = Some(
                                    "域名或 IP 格式无效（示例：steampowered.com、full:a.b.com 或 104.16.0.0/16）"
                                        .into(),
                                );
                            }
                        }

                        if let Some(ref err) = modal.list_error {
                            ui.label(RichText::new(err).size(FONT_XS).color(p.danger_text));
                        }

                        // Single-layer clean rules list matching `.cr-list` / `.cr-row` (no double frame!)
                        let mut toggle_idx: Option<usize> = None;
                        let mut cycle_idx: Option<usize> = None;
                        let mut delete_idx: Option<usize> = None;

                        if modal.draft_rules.is_empty() {
                            Frame::new()
                                .fill(p.bg_card_alt)
                                .stroke(Stroke::new(1.0_f32, p.border_subtle))
                                .corner_radius(CornerRadius::same(8))
                                .inner_margin(Margin::symmetric(12, 22))
                                .show(ui, |ui| {
                                    ui.set_width((INNER_W - 24.0).max(200.0));
                                    ui.vertical_centered(|ui| {
                                        ui.label(
                                            RichText::new("暂无自定义规则")
                                                .size(FONT_SM)
                                                .color(p.text_muted),
                                        );
                                    });
                                });
                        } else {
                            ScrollArea::vertical()
                                .id_salt("custom_rules_list_scroll")
                                .max_height(196.0)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    ui.set_width(INNER_W);
                                    ui.spacing_mut().item_spacing.y = 5.0;
                                    let is_dual = route_mode == RouteMode::Dual;
                                    let row_inner_w = (INNER_W - 20.0).max(200.0);

                                    for (idx, rule) in modal.draft_rules.iter().enumerate() {
                                        let row_fill = if rule.enabled {
                                            p.bg_card_alt
                                        } else {
                                            p.bg_card_alt.gamma_multiply(0.65)
                                        };
                                        Frame::new()
                                            .fill(row_fill)
                                            .stroke(Stroke::new(1.0_f32, p.border_subtle))
                                            .corner_radius(CornerRadius::same(8))
                                            .inner_margin(Margin::symmetric(10, 5))
                                            .show(ui, |ui| {
                                                ui.set_width(row_inner_w);
                                                aligned_row(ui, 24.0, |ui| {
                                                    ui.spacing_mut().item_spacing.x = 8.0;

                                                    // Enable/disable checkbox
                                                    let (cb_r, cb_resp) = ui.allocate_exact_size(
                                                        Vec2::splat(16.0),
                                                        Sense::click(),
                                                    );
                                                    if cb_resp.hovered() {
                                                        ui.ctx().set_cursor_icon(
                                                            egui::CursorIcon::PointingHand,
                                                        );
                                                    }
                                                    let cb_fill = if rule.enabled {
                                                        p.accent
                                                    } else {
                                                        p.bg_input
                                                    };
                                                    let cb_stroke = if rule.enabled {
                                                        p.accent
                                                    } else {
                                                        p.border_strong
                                                    };
                                                    ui.painter().rect(
                                                        cb_r,
                                                        CornerRadius::same(4),
                                                        cb_fill,
                                                        Stroke::new(1.0_f32, cb_stroke),
                                                        StrokeKind::Inside,
                                                    );
                                                    if rule.enabled {
                                                        let c = cb_r.center();
                                                        let s = Stroke::new(1.6_f32, Color32::WHITE);
                                                        ui.painter().line_segment(
                                                            [
                                                                Pos2::new(c.x - 3.6, c.y + 0.2),
                                                                Pos2::new(c.x - 1.0, c.y + 2.8),
                                                            ],
                                                            s,
                                                        );
                                                        ui.painter().line_segment(
                                                            [
                                                                Pos2::new(c.x - 1.0, c.y + 2.8),
                                                                Pos2::new(c.x + 3.8, c.y - 2.6),
                                                            ],
                                                            s,
                                                        );
                                                    }
                                                    if cb_resp
                                                        .on_hover_text(if rule.enabled {
                                                            "点击暂停此规则"
                                                        } else {
                                                            "点击启用此规则"
                                                        })
                                                        .clicked()
                                                    {
                                                        toggle_idx = Some(idx);
                                                    }

                                                    // White-surface pill `.cr-kind` badge
                                                    let kind =
                                                        CustomRule::kind_label(&rule.pattern);
                                                    let kind_fg = if rule.enabled {
                                                        p.text_secondary
                                                    } else {
                                                        p.text_muted
                                                    };
                                                    let kind_galley = ui.fonts(|f| {
                                                        f.layout_no_wrap(
                                                            kind.to_string(),
                                                            FontId::proportional(10.5),
                                                            kind_fg,
                                                        )
                                                    });
                                                    let (kr, _) = ui.allocate_exact_size(
                                                        Vec2::new(
                                                            kind_galley.size().x + 12.0,
                                                            20.0,
                                                        ),
                                                        Sense::hover(),
                                                    );
                                                    ui.painter().rect(
                                                        kr,
                                                        CornerRadius::same(10),
                                                        p.bg_card,
                                                        Stroke::new(1.0_f32, p.border_subtle),
                                                        StrokeKind::Inside,
                                                    );
                                                    let k_ty = galley_center_y(kr, &kind_galley);
                                                    ui.painter().galley(
                                                        Pos2::new(
                                                            kr.center().x
                                                                - kind_galley.size().x * 0.5,
                                                            k_ty,
                                                        ),
                                                        kind_galley,
                                                        kind_fg,
                                                    );

                                                    // Monospace pattern (with strikethrough when paused)
                                                    let text_col = if rule.enabled {
                                                        p.text_primary
                                                    } else {
                                                        p.text_muted
                                                    };
                                                    let max_pat_w =
                                                        (ui.available_width() - 152.0).max(80.0);
                                                    let shown = elide_text(
                                                        ui,
                                                        &rule.pattern,
                                                        FontId::monospace(FONT_SM),
                                                        max_pat_w,
                                                    );
                                                    let mut pat_rt = RichText::new(shown)
                                                        .size(FONT_SM)
                                                        .monospace()
                                                        .color(text_col);
                                                    if !rule.enabled {
                                                        pat_rt = pat_rt.strikethrough();
                                                    }
                                                    ui.label(pat_rt).on_hover_text(&rule.pattern);

                                                    // Right controls: bordered `×` + pill action badge + fallback hint
                                                    ui.with_layout(
                                                        Layout::right_to_left(Align::Center),
                                                        |ui| {
                                                            ui.spacing_mut().item_spacing.x = 6.0;
                                                            let (del_r, del_resp) =
                                                                ui.allocate_exact_size(
                                                                    Vec2::splat(22.0),
                                                                    Sense::click(),
                                                                );
                                                            if del_resp.hovered() {
                                                                ui.ctx().set_cursor_icon(
                                                                    egui::CursorIcon::PointingHand,
                                                                );
                                                            }
                                                            let (d_bg, d_bd, x_col) =
                                                                if del_resp.hovered() {
                                                                    (
                                                                        p.danger_soft,
                                                                        p.danger_text,
                                                                        p.danger_text,
                                                                    )
                                                                } else {
                                                                    (
                                                                        p.bg_card,
                                                                        p.border_subtle,
                                                                        p.text_muted,
                                                                    )
                                                                };
                                                            ui.painter().rect(
                                                                del_r,
                                                                CornerRadius::same(6),
                                                                d_bg,
                                                                Stroke::new(1.0_f32, d_bd),
                                                                StrokeKind::Inside,
                                                            );
                                                            icons::draw_win_close_icon(
                                                                ui.painter(),
                                                                del_r.center(),
                                                                x_col,
                                                            );
                                                            if del_resp
                                                                .on_hover_text("删除规则")
                                                                .clicked()
                                                            {
                                                                delete_idx = Some(idx);
                                                            }

                                                            let (act_bg, act_fg, act_bd) =
                                                                action_style(rule.action, p);
                                                            let act_galley = ui.fonts(|f| {
                                                                f.layout_no_wrap(
                                                                    rule.action
                                                                        .label()
                                                                        .to_string(),
                                                                    FontId::proportional(FONT_XS),
                                                                    act_fg,
                                                                )
                                                            });
                                                            let act_w =
                                                                (act_galley.size().x + 18.0)
                                                                    .max(48.0);
                                                            let (act_r, act_resp) =
                                                                ui.allocate_exact_size(
                                                                    Vec2::new(act_w, 22.0),
                                                                    Sense::click(),
                                                                );
                                                            if act_resp.hovered() {
                                                                ui.ctx().set_cursor_icon(
                                                                    egui::CursorIcon::PointingHand,
                                                                );
                                                            }
                                                            ui.painter().rect(
                                                                act_r,
                                                                CornerRadius::same(11),
                                                                act_bg,
                                                                Stroke::new(1.0_f32, act_bd),
                                                                StrokeKind::Inside,
                                                            );
                                                            let ty =
                                                                galley_center_y(act_r, &act_galley);
                                                            ui.painter().galley(
                                                                Pos2::new(
                                                                    act_r.center().x
                                                                        - act_galley.size().x
                                                                            * 0.5,
                                                                    ty,
                                                                ),
                                                                act_galley,
                                                                act_fg,
                                                            );
                                                            if act_resp
                                                                .on_hover_text(
                                                                    "点击循环切换出口：直连 → 国内节点 → 主节点",
                                                                )
                                                                .clicked()
                                                            {
                                                                cycle_idx = Some(idx);
                                                            }

                                                            if rule.enabled
                                                                && rule.action
                                                                    == CustomRuleAction::Domestic
                                                                && !is_dual
                                                            {
                                                                ui.label(
                                                                    RichText::new("→ 直连")
                                                                        .size(10.5)
                                                                        .color(p.warn_text),
                                                                )
                                                                .on_hover_text(
                                                                    "当前非双节点模式，国内节点规则将自动回落为直连",
                                                                );
                                                            }
                                                        },
                                                    );
                                                });
                                            });
                                    }
                                });
                        }

                        if let Some(i) = toggle_idx
                            && let Some(r) = modal.draft_rules.get_mut(i)
                        {
                            r.enabled = !r.enabled;
                            modal.draft_rev = modal.draft_rev.wrapping_add(1);
                        }
                        if let Some(i) = cycle_idx
                            && let Some(r) = modal.draft_rules.get_mut(i)
                        {
                            r.action = r.action.next();
                            modal.draft_rev = modal.draft_rev.wrapping_add(1);
                        }
                        if let Some(i) = delete_idx
                            && i < modal.draft_rules.len()
                        {
                            modal.draft_rules.remove(i);
                            modal.draft_rev = modal.draft_rev.wrapping_add(1);
                        }
                    }
                    EditorTab::Bulk => {
                        if styled_multiline(
                            ui,
                            &mut modal.bulk_text,
                            "每行一条：目标, direct | domestic | primary（# 注释，! 暂停）\nsteampowered.com, direct\n45.67.89.10/32, domestic",
                            9,
                            p,
                        )
                        .changed()
                        {
                            modal.bulk_error = None;
                        }
                        if let Some(ref err) = modal.bulk_error {
                            ui.label(RichText::new(err).size(FONT_XS).color(p.danger_text));
                        }
                    }
                }

                // 4. Live Route Hit Simulator (`.cr-test-bar`, result-first layout, cached)
                Frame::new()
                    .fill(p.bg_card_alt)
                    .stroke(Stroke::new(1.0_f32, p.border_subtle))
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(10, 7))
                    .show(ui, |ui| {
                        ui.set_width((INNER_W - 20.0).max(200.0));
                        aligned_row(ui, 28.0, |ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            ui.label(
                                RichText::new("命中测试")
                                    .font(font_bold(FONT_XS))
                                    .color(p.text_secondary),
                            );
                            styled_input_exact(
                                ui,
                                &mut modal.test_input,
                                "输入域名或 IP 测出口…",
                                Vec2::new(200.0, 28.0),
                                FONT_SM,
                                false,
                                p,
                            );
                            let avail_res_w = ui.available_width().max(80.0);
                            match modal.eval_preview(route_mode) {
                                Err(err) => {
                                    let shown = elide_text(
                                        ui,
                                        err,
                                        FontId::proportional(FONT_XS),
                                        avail_res_w,
                                    );
                                    ui.label(
                                        RichText::new(shown).size(FONT_XS).color(p.danger_text),
                                    )
                                    .on_hover_text(err);
                                }
                                Ok(Some(res)) => {
                                    let (lbl, col) = if res.domestic_fallback_to_direct {
                                        ("直连（回落）", p.warn_text)
                                    } else {
                                        let (_, fg, _) = action_style(res.action, p);
                                        (res.action.label(), fg)
                                    };
                                    let head = format!("→ {lbl}");
                                    let head_w = ui.fonts(|f| {
                                        f.layout_no_wrap(
                                            head.clone(),
                                            font_bold(FONT_XS),
                                            col,
                                        )
                                        .size()
                                        .x
                                    });
                                    let tail_raw = format!(" · {}", res.source);
                                    let tail_shown = elide_text(
                                        ui,
                                        &tail_raw,
                                        FontId::proportional(FONT_XS),
                                        (avail_res_w - head_w - 4.0).max(40.0),
                                    );
                                    let mut job = egui::text::LayoutJob::default();
                                    job.append(
                                        &head,
                                        0.0,
                                        egui::TextFormat::simple(font_bold(FONT_XS), col),
                                    );
                                    job.append(
                                        &tail_shown,
                                        0.0,
                                        egui::TextFormat::simple(
                                            FontId::proportional(FONT_XS),
                                            p.text_secondary,
                                        ),
                                    );
                                    ui.label(job)
                                        .on_hover_text(format!("{head}{tail_raw}"));
                                }
                                Ok(None) => {
                                    ui.label(
                                        RichText::new(format!("当前：{}", route_mode.label()))
                                            .size(FONT_XS)
                                            .color(p.text_muted),
                                    );
                                }
                            }
                        });
                    });

                // 5. Footer: clean action buttons (quiet tip only when list has rules)
                aligned_row(ui, 30.0, |ui| {
                    if modal.tab == EditorTab::List && !modal.draft_rules.is_empty() {
                        ui.label(
                            RichText::new("点击出口标签可循环切换")
                                .size(FONT_XS)
                                .color(p.text_muted),
                        );
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        if primary_btn(ui, "保存并生效", p).clicked() {
                            save_requested = true;
                        }
                        if soft_btn(ui, "取消", p).clicked() {
                            close_modal = true;
                        }
                    });
                });
            });

        if save_requested {
            let commit_res = if modal.tab == EditorTab::Bulk {
                parse_bulk_custom_rules_checked(&modal.bulk_text)
            } else {
                Ok(modal.draft_rules.clone())
            };
            match commit_res {
                Ok(final_rules) => {
                    let enabled_cnt = final_rules.iter().filter(|r| r.enabled).count();
                    let total_cnt = final_rules.len();
                    self.cfg.custom_rules = final_rules;
                    self.apply_custom_rules();
                    let msg = if enabled_cnt == total_cnt {
                        format!("已应用 {enabled_cnt} 条自定义分流规则（无需重连，热生效）")
                    } else {
                        format!(
                            "已保存 {total_cnt} 条自定义规则（生效 {enabled_cnt} 条，热生效）"
                        )
                    };
                    self.push_log(msg.clone());
                    self.show_toast(msg);
                    close_modal = true;
                }
                Err(err) => {
                    modal.bulk_error = Some(err);
                }
            }
        }

        if !close_modal {
            self.custom_rules_modal = Some(modal);
        }
    }
}

fn action_style(action: CustomRuleAction, p: ThemePalette) -> (Color32, Color32, Color32) {
    match action {
        CustomRuleAction::Direct => (p.bg_card, p.text_primary, p.border_strong),
        CustomRuleAction::Domestic => (
            p.success_soft,
            p.success_text,
            p.success_text.gamma_multiply(0.45),
        ),
        CustomRuleAction::Primary => (
            p.accent_soft,
            p.accent_text,
            p.accent.gamma_multiply(0.45),
        ),
    }
}

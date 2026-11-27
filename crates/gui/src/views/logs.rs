use crate::theme::{FONT_SM, ThemePalette};
use crate::widgets::{accent_chip_btn, aligned_row, card_frame, chip_btn, styled_search_input};
use crate::{GameAccelApp, LogFilter};
use eframe::egui::{self, Align, FontId, Layout, ScrollArea};

impl GameAccelApp {
    pub(super) fn render_logs(&mut self, ui: &mut egui::Ui, p: ThemePalette) {
        aligned_row(ui, 28.0, |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for (f, lbl) in [
                (LogFilter::All, "全部"),
                (LogFilter::Success, "连接"),
                (LogFilter::Error, "异常"),
            ] {
                let active = self.log_filter == f;
                let clicked = if active {
                    accent_chip_btn(ui, lbl, p, true).clicked()
                } else {
                    chip_btn(ui, lbl, p, false).clicked()
                };
                if clicked {
                    self.log_filter = f;
                }
            }

            let search_w = (ui.available_width() - 160.0).clamp(140.0, 280.0);
            styled_search_input(ui, &mut self.log_search, "搜索日志…", search_w, p);

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if chip_btn(ui, "清空", p, true).clicked() {
                    self.logs.clear();
                }
                if chip_btn(ui, "复制", p, false).clicked() {
                    ui.ctx().copy_text(self.logs.join("\n"));
                    self.show_toast("已复制运行日志");
                }
            });
        });

        ui.add_space(6.0);

        card_frame(p).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let avail_h = (ui.available_height() - 4.0).max(240.0);
            ui.set_min_height(avail_h);
            let q = self.log_search.trim().to_lowercase();
            let filtered: Vec<(&String, bool, bool)> = self
                .logs
                .iter()
                .filter_map(|line| {
                    let is_err =
                        line.contains("异常") || line.contains("失败") || line.contains("无效");
                    let is_ok = line.contains("成功")
                        || line.contains("已开启")
                        || line.contains("已启动");
                    let pass_level = match self.log_filter {
                        LogFilter::All => true,
                        LogFilter::Success => is_ok,
                        LogFilter::Error => is_err,
                    };
                    if !pass_level {
                        return None;
                    }
                    if !q.is_empty() && !line.to_lowercase().contains(&q) {
                        return None;
                    }
                    Some((line, is_err, is_ok))
                })
                .collect();

            if filtered.is_empty() {
                ui.allocate_ui_with_layout(
                    egui::Vec2::new(ui.available_width(), avail_h),
                    Layout::top_down(Align::Center),
                    |ui| {
                        ui.add_space((avail_h * 0.5 - 12.0).max(16.0));
                        let hint = if self.logs.is_empty() {
                            "暂无运行日志"
                        } else {
                            "未找到匹配的日志条目"
                        };
                        ui.label(egui::RichText::new(hint).size(FONT_SM).color(p.text_muted));
                    },
                );
            } else {
                ScrollArea::vertical()
                    .max_height(avail_h)
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        for (line, is_err, is_ok) in filtered {
                            let color = if is_err {
                                p.danger_text
                            } else if is_ok {
                                p.success_text
                            } else {
                                p.text_secondary
                            };
                            let font = FontId::proportional(FONT_SM);
                            let mut job = egui::text::LayoutJob::default();
                            if line.starts_with('[')
                                && let Some(end) = line.find("] ")
                            {
                                let (ts, rest) = line.split_at(end + 2);
                                job.append(
                                    ts,
                                    0.0,
                                    egui::TextFormat::simple(font.clone(), p.text_muted),
                                );
                                job.append(rest, 0.0, egui::TextFormat::simple(font, color));
                            } else {
                                job.append(line, 0.0, egui::TextFormat::simple(font, color));
                            }
                            ui.label(job);
                        }
                    });
            }
        });
    }
}

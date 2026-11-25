use crate::theme::{FONT_SM, ThemePalette};
use crate::widgets::{accent_chip_btn, card_frame, chip_btn, styled_search_input};
use crate::{GameAccelApp, LogFilter};
use eframe::egui::{self, Align, FontId, Layout, ScrollArea};

impl GameAccelApp {
    pub(super) fn render_logs(&mut self, ui: &mut egui::Ui, p: ThemePalette) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for (f, lbl) in [
                (LogFilter::All, "全部"),
                (LogFilter::Success, "成功/连接"),
                (LogFilter::Error, "警告/错误"),
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

            let search_w = (ui.available_width() - 360.0).clamp(140.0, 320.0);
            styled_search_input(ui, &mut self.log_search, "搜索日志关键字...", search_w, p);

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if chip_btn(ui, "清空", p, true).clicked() {
                    self.logs.clear();
                }
                if chip_btn(ui, "复制全部", p, false).clicked() {
                    ui.ctx().copy_text(self.logs.join("\n"));
                    self.show_toast("已复制全部运行日志");
                }
            });
        });

        ui.add_space(8.0);

        card_frame(p).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let avail_h = ui.available_height().max(260.0);
            ScrollArea::vertical()
                .max_height(avail_h)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let q = self.log_search.trim().to_lowercase();
                    for line in &self.logs {
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
                            continue;
                        }
                        if !q.is_empty() && !line.to_lowercase().contains(&q) {
                            continue;
                        }
                        let color = if is_err {
                            p.danger_text
                        } else if is_ok {
                            p.success_text
                        } else {
                            p.text_secondary
                        };
                        // Use Proportional (Microsoft YaHei) for mixed CJK/ASCII log lines so
                        // timestamps, ASCII acronyms (e.g. TUN), and CJK share one baseline.
                        let font = FontId::proportional(FONT_SM);
                        let mut job = egui::text::LayoutJob::default();
                        if line.starts_with('[')
                            && let Some(end) = line.find("] ")
                        {
                            let (ts, rest) = line.split_at(end + 2);
                            job.append(ts, 0.0, egui::TextFormat::simple(font.clone(), p.text_muted));
                            job.append(rest, 0.0, egui::TextFormat::simple(font, color));
                        } else {
                            job.append(line, 0.0, egui::TextFormat::simple(font, color));
                        }
                        ui.label(job);
                    }
                });
        });
    }

}

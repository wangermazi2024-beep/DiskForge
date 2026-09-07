//! "分析"类标签页的渲染：文件扩展名分类 / 重复文件候选列表。
//! 这两个都是只读展示——数据在打开标签页的时候算一次（见 app.rs），
//! 这里只管画，不做任何计算，保持"渲染"和"计算"分开。

use egui::{Color32, RichText};
use crate::format::human_size;
use crate::model::{DuplicateGroup, ExtensionStat};

const DIM: Color32 = Color32::from_rgb(0x90, 0x90, 0x90);

pub fn show_extensions(ui: &mut egui::Ui, data: &[ExtensionStat]) {
    if data.is_empty() {
        ui.centered_and_justified(|ui| { ui.label(RichText::new("没有数据").color(DIM)); });
        return;
    }
    let total: u64 = data.iter().map(|e| e.size).sum::<u64>().max(1);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        egui_extras::TableBuilder::new(ui)
            .striped(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(egui_extras::Column::initial(120.0).resizable(true))
            .column(egui_extras::Column::initial(100.0).resizable(true))
            .column(egui_extras::Column::initial(90.0).resizable(true))
            .column(egui_extras::Column::remainder())
            .header(24.0, |mut h| {
                h.col(|ui| { ui.label(RichText::new("扩展名").strong()); });
                h.col(|ui| { ui.label(RichText::new("总大小").strong()); });
                h.col(|ui| { ui.label(RichText::new("文件数").strong()); });
                h.col(|ui| { ui.label(RichText::new("占比").strong()); });
            })
            .body(|body| {
                body.rows(22.0, data.len(), |mut row| {
                    let idx = row.index();
                    let e = &data[idx];
                    let display_ext = if e.ext.starts_with('（') { e.ext.clone() } else { format!(".{}", e.ext) };
                    row.col(|ui| { ui.label(display_ext); });
                    row.col(|ui| { ui.label(human_size(e.size)); });
                    row.col(|ui| { ui.label(format!("{}", e.count)); });
                    row.col(|ui| {
                        let pct = e.size as f32 / total as f32;
                        let rect = ui.available_rect_before_wrap();
                        let p = ui.painter();
                        p.rect_filled(rect, 2.0, Color32::from_rgb(0x33, 0x33, 0x38));
                        let w = rect.width() * pct;
                        if w > 0.5 {
                            p.rect_filled(egui::Rect::from_min_size(rect.min, egui::vec2(w, rect.height())), 2.0, Color32::from_rgb(0x4C, 0x8B, 0xF5));
                        }
                        p.text(rect.left_center() + egui::vec2(4.0, 0.0), egui::Align2::LEFT_CENTER,
                            format!("{:.1}%", pct * 100.0), egui::FontId::proportional(11.0), Color32::WHITE);
                    });
                });
            });
    });
}

pub fn show_duplicates(ui: &mut egui::Ui, data: &[DuplicateGroup]) {
    ui.add_space(4.0);
    ui.label(RichText::new("⚠ 按文件大小分组的候选结果，不是内容比对确认——大小相同不代表内容一定相同，处理前请自行核实。")
        .size(11.0).color(Color32::from_rgb(0xF5, 0xA6, 0x23)));
    ui.add_space(6.0);
    if data.is_empty() {
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("没有发现候选重复文件（需要至少 2 个非空文件大小完全一致）").color(DIM));
        });
        return;
    }
    // 用虚拟化表格代替 CollapsingHeader 列表：候选组数量可能有几千甚至更多
    // （很多同大小的小文件很常见），CollapsingHeader 列表不做虚拟滚动，每一帧都要
    // 布局全部条目，条目一多就会明显卡；换成和"扩展名分类"一样的 TableBuilder +
    // body.rows() 之后，只布局当前可见的那几十行，不管候选组有多少都不会卡。
    // 展开查看完整路径列表的能力用一个可点击的"详情"按钮替代（弹一个小窗口），
    // 而不是每行自带一个会撑高整行、影响虚拟化的可展开区域。
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        egui_extras::TableBuilder::new(ui)
            .striped(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(egui_extras::Column::initial(90.0).resizable(true))
            .column(egui_extras::Column::initial(70.0).resizable(true))
            .column(egui_extras::Column::initial(90.0).resizable(true))
            .column(egui_extras::Column::remainder())
            .header(24.0, |mut h| {
                h.col(|ui| { ui.label(RichText::new("大小").strong()); });
                h.col(|ui| { ui.label(RichText::new("文件数").strong()); });
                h.col(|ui| { ui.label(RichText::new("可省空间").strong()); });
                h.col(|ui| { ui.label(RichText::new("示例路径").strong()); });
            })
            .body(|body| {
                body.rows(22.0, data.len(), |mut row| {
                    let idx = row.index();
                    let g = &data[idx];
                    let wasted = g.size * (g.paths.len() as u64 - 1);
                    row.col(|ui| { ui.label(human_size(g.size)); });
                    row.col(|ui| { ui.label(format!("{}", g.paths.len())); });
                    row.col(|ui| { ui.label(RichText::new(human_size(wasted)).color(Color32::from_rgb(0xF5, 0xA6, 0x23))); });
                    row.col(|ui| {
                        let preview = g.paths.first().map(|s| s.as_str()).unwrap_or("");
                        let extra = if g.paths.len() > 1 { format!("  (+{} 个，悬停查看全部)", g.paths.len() - 1) } else { String::new() };
                        ui.label(format!("{preview}{extra}")).on_hover_ui(|ui| {
                            ui.set_max_width(500.0);
                            for p in &g.paths {
                                ui.label(RichText::new(p).size(11.5).monospace());
                            }
                        });
                    });
                });
            });
    });
}


use egui::{Color32, Rect, RichText, Vec2};
use crate::disk_info::DiskInfo;
use crate::format::human_size;
use crate::model::{CategoryStat, Node};

const DIM: Color32 = Color32::from_rgb(0x90, 0x90, 0x90);

pub enum SidebarAction {
    None,
    OpenExtensions,
    OpenDuplicates,
}

pub fn show(ui: &mut egui::Ui, focused: Option<&Node>, info: Option<&DiskInfo>, categories: Option<&[CategoryStat]>) -> SidebarAction {
    let mut action = SidebarAction::None;
    ui.add_space(10.0);
    ui.label(RichText::new("磁盘概览").strong().size(13.0));
    ui.add_space(8.0);

    match (focused, info) {
        (Some(node), Some(info)) if info.total_bytes > 0 => {
            let used_pct = info.used_bytes as f32 / info.total_bytes as f32;
            ui.vertical_centered(|ui| draw_ring(ui, used_pct));
            ui.add_space(6.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(&node.name).size(12.5).strong());
                ui.label(RichText::new(format!(
                    "已用 {} / 共 {}",
                    human_size(info.used_bytes), human_size(info.total_bytes)
                )).size(11.0).color(DIM));
            });
        }
        (Some(node), _) => {
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(&node.name).size(12.5).strong());
                ui.label(RichText::new(format!("已扫描 {}", human_size(node.logical_size)))
                    .size(11.0).color(DIM));
            });
        }
        _ => {
            ui.vertical_centered(|ui| {
                ui.add_space(20.0);
                ui.label(RichText::new("扫描完成后显示").size(12.0).color(DIM));
            });
        }
    }

    ui.add_space(18.0);
    ui.separator();
    ui.add_space(10.0);
    ui.label(RichText::new("分类统计").strong().size(13.0));
    ui.add_space(8.0);

    if let Some(cats) = categories {
        let total: u64 = cats.iter().map(|c| c.size).sum::<u64>().max(1);
        for c in cats.iter().filter(|c| c.size > 0) {
            let pct = c.size as f32 / total as f32;
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(9.0, 9.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, 2.0, c.color);
                ui.label(RichText::new(c.label).size(11.5));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(human_size(c.size)).size(11.0).color(DIM));
                });
            });
            let (bar_rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), egui::Sense::hover());
            draw_mini_bar(ui, bar_rect, pct, c.color);
            ui.add_space(7.0);
        }
        if cats.iter().all(|c| c.size == 0) {
            ui.vertical_centered(|ui| ui.label(RichText::new("（暂无数据）").size(11.5).color(DIM)));
        }
    } else {
        ui.vertical_centered(|ui| ui.label(RichText::new("扫描完成后显示").size(12.0).color(DIM)));
    }

    ui.add_space(18.0);
    ui.separator();
    ui.add_space(10.0);

    ui.add_enabled_ui(focused.is_some(), |ui| {
        if analysis_button(ui, "🗐 文件扩展名分类", "按具体扩展名（.mp4/.jpg/…）统计占用") {
            action = SidebarAction::OpenExtensions;
        }
        ui.add_space(8.0);
        if analysis_button(ui, "🔍 重复文件查找", "按大小找出相同大小的候选重复文件") {
            action = SidebarAction::OpenDuplicates;
        }
    });
    if focused.is_none() {
        ui.add_space(6.0);
        ui.vertical_centered(|ui| ui.label(RichText::new("扫描完成后可用").size(10.5).color(DIM)));
    }

    action
}

fn analysis_button(ui: &mut egui::Ui, title: &str, desc: &str) -> bool {
    let resp = ui.add(egui::Button::new("").frame(false).min_size(Vec2::new(ui.available_width(), 38.0)));
    let rect = resp.rect;
    ui.painter().rect_filled(rect, 4.0, Color32::from_rgb(0x33, 0x33, 0x38));
    ui.painter().text(rect.left_top() + Vec2::new(8.0, 8.0), egui::Align2::LEFT_TOP, title, egui::FontId::proportional(12.5), Color32::WHITE);
    ui.painter().text(rect.left_top() + Vec2::new(8.0, 24.0), egui::Align2::LEFT_TOP, desc, egui::FontId::proportional(10.0), DIM);
    resp.clicked()
}

fn draw_ring(ui: &mut egui::Ui, used_pct: f32) {
    let size = Vec2::splat(96.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter();
    let center = rect.center();
    let radius = rect.width() / 2.0 - 6.0;
    let stroke_w = 10.0;
    painter.circle_stroke(center, radius, egui::Stroke::new(stroke_w, Color32::from_rgb(0x3A, 0x3A, 0x40)));
    let used_pct = used_pct.clamp(0.0, 1.0);
    if used_pct > 0.002 {
        let start = -std::f32::consts::FRAC_PI_2;
        let end = start + used_pct * std::f32::consts::TAU;
        let n = 64.max((used_pct * 64.0) as usize);
        let points: Vec<egui::Pos2> = (0..=n).map(|i| {
            let t = start + (end - start) * (i as f32 / n as f32);
            center + Vec2::new(t.cos(), t.sin()) * radius
        }).collect();
        painter.add(egui::Shape::line(points, egui::Stroke::new(stroke_w, crate::theme::ACCENT_BLUE)));
    }
    painter.text(center, egui::Align2::CENTER_CENTER, format!("{:.0}%", used_pct * 100.0),
        egui::FontId::proportional(18.0), Color32::WHITE);
}

fn draw_mini_bar(ui: &mut egui::Ui, rect: Rect, pct: f32, color: Color32) {
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, Color32::from_rgb(0x3A, 0x3A, 0x40));
    let w = (rect.width() * pct.clamp(0.0, 1.0)).max(0.0);
    if w > 0.5 {
        painter.rect_filled(Rect::from_min_size(rect.min, Vec2::new(w, rect.height())), 3.0, color);
    }
}

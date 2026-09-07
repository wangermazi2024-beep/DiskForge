
use std::collections::HashSet;
use egui::{Color32, RichText};

pub struct PickerState {
    pub available_drives: Vec<(char, Option<String>)>,
    pub selected_drives: HashSet<char>,
    pub custom_paths: Vec<String>,
}

impl PickerState {
    pub fn new(available_drives: Vec<(char, Option<String>)>) -> Self {
        Self { available_drives, selected_drives: HashSet::new(), custom_paths: Vec::new() }
    }

    pub fn has_selection(&self) -> bool {
        !self.selected_drives.is_empty() || !self.custom_paths.is_empty()
    }
}

pub enum PickerAction {
    None,
    Confirm,
    Cancel,
}

pub fn show(ui: &mut egui::Ui, state: &mut PickerState, show_cancel: bool) -> PickerAction {
    let mut action = PickerAction::None;

    ui.label(RichText::new("选择要扫描的分区，或添加自定义目录").size(16.0).strong());
    ui.label(
        RichText::new("在开始扫描之前不会查询任何分区的容量信息")
            .size(11.5)
            .color(Color32::from_rgb(0x90, 0x90, 0x90)),
    );
    ui.add_space(12.0);

    if state.available_drives.is_empty() {
        ui.label(RichText::new("未检测到固定磁盘分区，可以手动添加目录").color(Color32::from_rgb(0xE0, 0x55, 0x5B)));
    } else {
        ui.label(RichText::new("固定磁盘分区（可多选）").size(12.5).color(Color32::from_rgb(0xC8, 0xC8, 0xC8)));
        ui.add_space(4.0);
        for &(letter, ref label) in &state.available_drives {
            let mut checked = state.selected_drives.contains(&letter);
            let display = match label {
                Some(l) => format!("{l} ({letter}:)"),
                None => format!("本地磁盘 ({letter}:)"),
            };
            if ui.checkbox(&mut checked, display).changed() {
                if checked {
                    state.selected_drives.insert(letter);
                } else {
                    state.selected_drives.remove(&letter);
                }
            }
        }
    }

    ui.add_space(14.0);
    ui.separator();
    ui.add_space(10.0);

    ui.label(RichText::new("自定义目录").size(12.5).color(Color32::from_rgb(0xC8, 0xC8, 0xC8)));
    ui.add_space(4.0);
    let mut remove_idx: Option<usize> = None;
    for (i, p) in state.custom_paths.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(RichText::new(p).size(12.5));
            if ui.small_button("移除").clicked() {
                remove_idx = Some(i);
            }
        });
    }
    if let Some(i) = remove_idx {
        state.custom_paths.remove(i);
    }
    ui.add_space(4.0);
    if ui.button("📁 浏览文件夹…").clicked()
        && let Some(dir) = rfd::FileDialog::new().pick_folder() {
            let s = dir.to_string_lossy().into_owned();
            if !state.custom_paths.contains(&s) {
                state.custom_paths.push(s);
            }
        }

    ui.add_space(20.0);
    ui.horizontal(|ui| {
        let confirm_btn = egui::Button::new(RichText::new("  开始扫描  ").color(Color32::WHITE))
            .fill(crate::theme::ACCENT_BLUE)
            .corner_radius(egui::CornerRadius::same(6));
        if ui.add_enabled(state.has_selection(), confirm_btn).clicked() {
            action = PickerAction::Confirm;
        }
        if show_cancel && ui.button("取消").clicked() {
            action = PickerAction::Cancel;
        }
    });
    if !state.has_selection() {
        ui.label(RichText::new("至少选择一个分区或添加一个目录").size(11.0).color(Color32::from_rgb(0x90, 0x90, 0x90)));
    }

    action
}


use egui::{Color32, RichText};

pub enum TopbarAction {
    None,
    AddScan,
    ExportCsv,
    ToggleShowAll,
    ShowExtensionBreakdown,
    ShowDuplicateFinder,
    OpenFind,
    OpenSearchTab,
    OpenCopyTab,
    OpenAbout,
    #[cfg(windows)]
    RestartAsAdmin,
}

pub struct TopbarState<'a> {
    pub scanning: bool,
    pub scanned_count: u64,
    pub scan_error: Option<&'a str>,
    pub has_result: bool,
    pub show_all_details: bool,
    #[cfg(windows)]
    pub is_admin: bool,
}

pub fn show(ui: &mut egui::Ui, state: TopbarState) -> TopbarAction {
    let mut action = TopbarAction::None;
    egui::Panel::top("menu_bar").exact_size(34.0)
        .frame(egui::Frame::default().fill(Color32::from_rgb(0x2E, 0x2E, 0x32))
            .inner_margin(egui::Margin::symmetric(10, 4)))
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.menu_button("文件", |ui| {
                    if ui.button("添加扫描…").clicked() {
                        action = TopbarAction::AddScan;
                        ui.close();
                    }
                    ui.add_enabled_ui(state.has_result, |ui| {
                        if ui.button("导出 CSV…").clicked() {
                            action = TopbarAction::ExportCsv;
                            ui.close();
                        }
                        if ui.button("📑 复制列表")
                            .on_hover_text("把当前的分区/文件数据复制到一个新标签页，画面和主列表一样能展开、能重新扫描/移除分区，\n但操作的是独立副本，不影响主列表——方便对着同一份数据的不同状态分开查看对比。")
                            .clicked()
                        {
                            action = TopbarAction::OpenCopyTab;
                            ui.close();
                        }
                    });
                });

                ui.menu_button("视图", |ui| {
                    let mut show_all = state.show_all_details;
                    if ui.checkbox(&mut show_all, "显示全部信息（含元数据文件）").changed() {
                        action = TopbarAction::ToggleShowAll;
                        ui.close();
                    }
                });

                ui.menu_button("分析", |ui| {
                    if ui.button("🗐 文件扩展名分类…（全部分区）")
                        .on_hover_text("把当前已扫描的全部分区混在一起按扩展名分类。\n只想看某一个分区的话，右键那个分区选同名菜单项。")
                        .clicked()
                    {
                        action = TopbarAction::ShowExtensionBreakdown;
                        ui.close();
                    }
                    if ui.button("🔍 查找重复文件…（全部分区）")
                        .on_hover_text("把当前已扫描的全部分区混在一起找重复文件，能找出跨盘的重复（比如 C 盘和 D 盘各存了一份一样的安装包）。\n只想看某一个分区的话，右键那个分区选同名菜单项。")
                        .clicked()
                    {
                        action = TopbarAction::ShowDuplicateFinder;
                        ui.close();
                    }
                });

                ui.add_enabled_ui(state.has_result, |ui| {
                    ui.menu_button("查找", |ui| {
                        if ui.button("🔎 查找… (Ctrl+F)")
                            .on_hover_text("在当前标签页里定位某一项——展开+选中+滚动过去，\n不筛选、不改变列表显示的内容，只是帮你跳过去。")
                            .clicked()
                        {
                            action = TopbarAction::OpenFind;
                            ui.close();
                        }
                        if ui.button("🔍 搜索文件…")
                            .on_hover_text("在新标签页里搜索/筛选全部文件（可以用正则），\n结果能排序、能直接右键删除/建符号链接。")
                            .clicked()
                        {
                            action = TopbarAction::OpenSearchTab;
                            ui.close();
                        }
                    });
                });

                ui.menu_button("关于", |ui| {
                    ui.label(RichText::new(format!("{} {}", crate::about::APP_NAME, crate::about::APP_VERSION)).strong());
                    ui.label(RichText::new(format!("由 {} 开发", crate::about::APP_AUTHOR)).size(11.0).color(Color32::from_rgb(0xA0, 0xA0, 0xA0)));
                    ui.separator();
                    if ui.button("ℹ 关于 DiskForge…")
                        .on_hover_text("版本、版权与许可信息；\n如果这个软件对你有帮助，也可以在这里扫码赞助作者。")
                        .clicked()
                    {
                        action = TopbarAction::OpenAbout;
                        ui.close();
                    }
                });

                ui.separator();

                #[cfg(windows)]
                if !state.is_admin {
                    let btn = egui::Button::new(RichText::new("⚡ 以管理员身份运行").color(Color32::from_rgb(0x2A, 0x2A, 0x2E)).strong())
                        .fill(Color32::from_rgb(0xF5, 0xC5, 0x18));
                    if ui.add(btn).on_hover_text("以管理员权限重启后可以用 MFT 直读，扫描速度快很多").clicked() {
                        action = TopbarAction::RestartAsAdmin;
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if state.scanning {
                        ui.add(egui::Spinner::new());
                        ui.label(RichText::new(format!("正在扫描… 已发现 {} 项", state.scanned_count))
                            .color(Color32::from_rgb(0xA0, 0xA0, 0xA0)));
                    } else if let Some(err) = state.scan_error {
                        ui.label(RichText::new(err).color(Color32::from_rgb(0xE0, 0x55, 0x5B)));
                    }
                });
            });
        });
    action
}

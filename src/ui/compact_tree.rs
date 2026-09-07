
use std::cell::Cell;
use std::sync::Arc;
use egui::{Color32, Pos2, Sense};
use crate::format::{format_filetime_local, human_size};
use crate::model::{Node, NodePath};
use crate::search_index::NameIndex;
use crate::ui::TreeAction;

const ROW_H: f32 = 24.0;
const HIDDEN_ACCENT: Color32 = crate::theme::HIDDEN_ACCENT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Modified,
    Path,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    fn toggled(self) -> Self {
        match self {
            SortDir::Asc => SortDir::Desc,
            SortDir::Desc => SortDir::Asc,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortState {
    pub key: SortKey,
    pub dir: SortDir,
}

impl Default for SortState {
    fn default() -> Self {
        Self { key: SortKey::Size, dir: SortDir::Desc }
    }
}

impl SortState {
    pub fn click(&mut self, key: SortKey) {
        if self.key == key {
            self.dir = self.dir.toggled();
        } else {
            self.key = key;
            self.dir = SortDir::Desc;
        }
    }
}

fn compare_nodes(a: &Node, b: &Node, key: SortKey) -> std::cmp::Ordering {
    match key {
        SortKey::Name => cmp_ignore_ascii_case(&a.name, &b.name),
        SortKey::Size => a.logical_size.cmp(&b.logical_size),
        SortKey::Modified => a.modified_ft.cmp(&b.modified_ft),
        SortKey::Path => {
            let pa = a.full_path_override.as_deref().unwrap_or("");
            let pb = b.full_path_override.as_deref().unwrap_or("");
            cmp_ignore_ascii_case(pa, pb)
        }
    }
}

fn cmp_ignore_ascii_case(a: &str, b: &str) -> std::cmp::Ordering {
    a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

fn sorted_child_order(children: &[Node], sort: SortState) -> Vec<usize> {
    let mut order: Vec<usize> = (0..children.len()).collect();
    order.sort_by(|&a, &b| compare_nodes(&children[a], &children[b], sort.key));
    if sort.dir == SortDir::Desc { order.reverse(); }
    order
}

#[derive(Clone)]
struct FlatRow {
    node: *const Node,
    abs_path: NodePath,
    depth: u32,
    is_group: bool,
}

#[derive(Default)]
pub struct ViewState {
    pub sort: SortState,
    pub expand_version: u64,
    cache: Option<(SortState, u64, Vec<FlatRow>)>,
    pub pending_scroll: Option<NodePath>,
    pub index: Option<Arc<NameIndex>>,
    pub index_builder: Option<crate::search_index::IndexBuilder>,
    pub struct_version: u64,
}

const HEADER_COLS: [(&str, SortKey); 4] = [
    ("名称", SortKey::Name),
    ("大小", SortKey::Size),
    ("修改时间", SortKey::Modified),
    ("路径", SortKey::Path),
];

pub fn show(ui: &mut egui::Ui, root: &Node, selected: &Option<NodePath>, view: &mut ViewState, is_duplicates: bool) -> TreeAction {
    let action_cell: Cell<TreeAction> = Cell::new(TreeAction::None);
    let pending_scroll = view.pending_scroll.take();

    let mut scroll_area = egui::ScrollArea::both().auto_shrink([false, false]);
    if pending_scroll.is_some() {
        scroll_area = scroll_area.horizontal_scroll_offset(0.0);
    }
    scroll_area.show(ui, |ui| {
        ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
        let mut builder = egui_extras::TableBuilder::new(ui)
            .striped(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .sense(egui::Sense::click())
            .column(egui_extras::Column::initial(280.0).at_least(120.0).clip(true).resizable(true))
            .column(egui_extras::Column::initial(90.0).clip(true).resizable(true))
            .column(egui_extras::Column::initial(130.0).clip(true).resizable(true))
            .column(egui_extras::Column::remainder().at_least(150.0).clip(true).resizable(true));

        if let Some(target) = &pending_scroll {
            rebuild_view_cache(root, view);
            if let Some((_, _, flat_rows)) = &view.cache
                && let Some(idx) = flat_rows.iter().position(|r| &r.abs_path == target) {
                    builder = builder.scroll_to_row(idx, Some(egui::Align::Center));
                }
        }

        let sort_clicked_cell: Cell<Option<SortKey>> = Cell::new(None);
        let table = builder
            .header(ROW_H, |mut h| {
                for (label, key) in HEADER_COLS {
                    h.col(|ui| {
                        let active = view.sort.key == key;
                        let arrow = if active {
                            if view.sort.dir == SortDir::Asc { " ▲" } else { " ▼" }
                        } else { "" };
                        let color = if active { crate::theme::HEADER_ACTIVE_YELLOW } else { Color32::WHITE };
                        let text = egui::RichText::new(format!("{label}{arrow}")).strong().size(12.0).color(color);
                        let resp = ui.add(egui::Label::new(text).sense(Sense::click()));
                        let resp = resp.on_hover_text("点击排序，再次点击切换升/降序");
                        if resp.hovered() { ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand); }
                        if resp.clicked() { sort_clicked_cell.set(Some(key)); }
                    });
                }
            });
        if let Some(key) = sort_clicked_cell.get() { view.sort.click(key); }

        rebuild_view_cache(root, view);
        let flat_rows = &view.cache.as_ref().unwrap().2;

        table
            .body(|body| {
                let clicked_row: Cell<Option<usize>> = Cell::new(None);
                let secondary_clicked_row: Cell<Option<usize>> = Cell::new(None);
                let action_request: Cell<Option<TreeAction>> = Cell::new(None);
                body.rows(ROW_H, flat_rows.len(), |mut row| {
                    let row_idx = row.index();
                    let fr = &flat_rows[row_idx];
                    let n: &Node = unsafe { &*fr.node };
                    let is_selected = selected.as_ref() == Some(&fr.abs_path);
                    let hidden = n.is_hidden_or_system();
                    let indent = fr.depth as f32 * 16.0 + 4.0;
                    let full_path = || n.full_path_override.clone().unwrap_or_default();
                    let is_group = fr.is_group;

                    let paint_bg_and_sense = |ui: &mut egui::Ui| -> (egui::Rect, egui::Response) {
                        let rect = ui.available_rect_before_wrap();
                        let resp = ui.allocate_rect(rect, Sense::click());
                        if is_selected {
                            ui.painter().rect_filled(rect, 0.0, crate::theme::SELECT_BG);
                        }
                        (rect, resp)
                    };
                    let dim_if_hidden = |ui: &egui::Ui, rect: egui::Rect| {
                        if hidden {
                            ui.painter().rect_filled(rect, 0.0, crate::theme::DIM_OVERLAY);
                        }
                    };
                    let handle_click = |resp: &egui::Response| {
                        if resp.clicked() { clicked_row.set(Some(row_idx)); }
                        if resp.secondary_clicked() { secondary_clicked_row.set(Some(row_idx)); }
                    };

                    row.col(|ui| {
                        let (rect, resp) = paint_bg_and_sense(ui);
                        let guide_color = Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 0x14);
                        for lvl in 0..fr.depth {
                            let x = rect.min.x + lvl as f32 * 16.0 + 10.0;
                            ui.painter().line_segment([Pos2::new(x, rect.min.y), Pos2::new(x, rect.max.y)], egui::Stroke::new(1.0, guide_color));
                        }
                        let p = ui.painter();
                        if is_group {
                            p.text(Pos2::new(rect.min.x + indent, rect.center().y), egui::Align2::LEFT_CENTER,
                                if n.expanded { "▼" } else { "▶" }, egui::FontId::proportional(10.0), Color32::from_rgb(0xAA, 0xCC, 0xFF));
                        }
                        let icon = if is_group { "🗀" } else { "📄" };
                        let tc = if is_selected { Color32::from_rgb(0xFF, 0xFF, 0x80) }
                            else if is_group { Color32::WHITE } else { Color32::from_rgb(0xCC, 0xCC, 0xCC) };
                        let mut text_x = rect.min.x + indent + 16.0;
                        let hidden_badge = if hidden {
                            let badge = egui::Rect::from_min_size(Pos2::new(text_x, rect.center().y - 7.0), egui::vec2(14.0, 14.0));
                            text_x += 18.0;
                            Some(badge)
                        } else { None };
                        p.text(Pos2::new(text_x, rect.center().y), egui::Align2::LEFT_CENTER, format!("{icon} {}", n.name), egui::FontId::proportional(13.0), tc);
                        dim_if_hidden(ui, rect);
                        if let Some(badge) = hidden_badge {
                            ui.painter().rect_filled(badge, 3.0, HIDDEN_ACCENT);
                            ui.painter().text(badge.center(), egui::Align2::CENTER_CENTER, "H", egui::FontId::proportional(9.5), Color32::WHITE);
                        }
                        handle_click(&resp);
                        if is_group {
                            if is_duplicates {
                                resp.context_menu(|ui| context_menu_group(ui, &fr.abs_path, &n.name, &action_request));
                            }
                        } else {
                            resp.context_menu(|ui| context_menu(ui, &n.name, &full_path(), &fr.abs_path, &action_request));
                        }
                    });
                    row.col(|ui| {
                        let (rect, resp) = paint_bg_and_sense(ui);
                        ui.painter().text(rect.left_center() + egui::vec2(4.0, 0.0), egui::Align2::LEFT_CENTER,
                            human_size(n.logical_size), egui::FontId::proportional(12.0), Color32::from_rgb(0xD0, 0xD0, 0xD0));
                        dim_if_hidden(ui, rect);
                        handle_click(&resp);
                        if !is_group { resp.context_menu(|ui| context_menu(ui, &n.name, &full_path(), &fr.abs_path, &action_request)); }
                    });
                    row.col(|ui| {
                        let (rect, resp) = paint_bg_and_sense(ui);
                        if !is_group {
                            ui.painter().text(rect.left_center() + egui::vec2(4.0, 0.0), egui::Align2::LEFT_CENTER,
                                format_filetime_local(n.modified_ft), egui::FontId::proportional(11.0), Color32::from_rgb(0xC0, 0xC0, 0xC0));
                        }
                        dim_if_hidden(ui, rect);
                        handle_click(&resp);
                        if !is_group { resp.context_menu(|ui| context_menu(ui, &n.name, &full_path(), &fr.abs_path, &action_request)); }
                    });
                    row.col(|ui| {
                        let (rect, resp) = paint_bg_and_sense(ui);
                        if !is_group {
                            let dir = n.full_path_override.as_deref()
                                .and_then(|p| p.rsplit_once('\\').map(|(d, _)| d))
                                .unwrap_or("");
                            ui.painter().text(rect.left_center() + egui::vec2(4.0, 0.0), egui::Align2::LEFT_CENTER,
                                dir, egui::FontId::proportional(11.5), Color32::from_rgb(0xA0, 0xA0, 0xA0));
                        }
                        dim_if_hidden(ui, rect);
                        handle_click(&resp);
                        if !is_group { resp.context_menu(|ui| context_menu(ui, &n.name, &full_path(), &fr.abs_path, &action_request)); }
                    });
                });
                if let Some(idx) = clicked_row.get() {
                    let p = flat_rows[idx].abs_path.clone();
                    if flat_rows[idx].is_group {
                        action_cell.set(TreeAction::ToggleExpand(p));
                    } else {
                        action_cell.set(TreeAction::Select(p));
                    }
                } else if let Some(idx) = secondary_clicked_row.get() {
                    action_cell.set(TreeAction::Select(flat_rows[idx].abs_path.clone()));
                }
                if let Some(action) = action_request.into_inner() {
                    action_cell.set(action);
                }
            });
    });

    action_cell.into_inner()
}

fn rebuild_view_cache(root: &Node, view: &mut ViewState) {
    let need_rebuild = view.cache.as_ref()
        .is_none_or(|(s, v, _)| *s != view.sort || *v != view.expand_version);
    if need_rebuild {
        let mut flat_rows: Vec<FlatRow> = Vec::new();
        let rel_path: NodePath = vec![0];
        collect_rows(root, &rel_path, view.sort, &mut flat_rows);
        view.cache = Some((view.sort, view.expand_version, flat_rows));
    }
}

fn collect_rows(root: &Node, rel_path: &NodePath, sort: SortState, rows: &mut Vec<FlatRow>) {
    struct Item<'a> { node: &'a Node, abs_path: NodePath, depth: u32, is_group: bool }
    let order = sorted_child_order(&root.children, sort);
    let mut stack: Vec<Item> = order.into_iter().rev().map(|i| {
        let mut p = rel_path.clone();
        p.push(i);
        Item { node: &root.children[i], abs_path: p, depth: 0, is_group: true }
    }).collect();
    while let Some(item) = stack.pop() {
        rows.push(FlatRow { node: item.node as *const Node, abs_path: item.abs_path.clone(), depth: item.depth, is_group: item.is_group });
        if item.is_group && item.node.expanded {
            let child_order = sorted_child_order(&item.node.children, sort);
            for i in child_order.into_iter().rev() {
                let mut p = item.abs_path.clone();
                p.push(i);
                stack.push(Item { node: &item.node.children[i], abs_path: p, depth: item.depth + 1, is_group: false });
            }
        }
    }
}

#[cfg(windows)]
fn open_in_explorer_select(path: &str) {
    if path.is_empty() { return; }
    use std::os::windows::process::CommandExt;
    let arg = format!("/select,\"{path}\"");
    crate::applog::log(&format!("[compact_tree] 打开资源管理器: explorer {arg}"));
    if let Err(e) = std::process::Command::new("explorer").raw_arg(&arg).spawn() {
        crate::applog::log(&format!("[compact_tree] 打开资源管理器失败 ({path}): {e}"));
    }
}
#[cfg(not(windows))]
fn open_in_explorer_select(_path: &str) {}

fn context_menu_group(ui: &mut egui::Ui, abs_path: &NodePath, name: &str, action_request: &Cell<Option<TreeAction>>) {
    ui.set_min_width(200.0);
    if ui.button("🔗 创建符号链接").clicked() {
        action_request.set(Some(TreeAction::RequestCreateSymlinkGroup { abs_path: abs_path.clone(), name: name.to_string() }));
        ui.close();
    }
    if ui.button("🔍 检测占用（整组）").clicked() {
        action_request.set(Some(TreeAction::RequestCheckLockGroup { abs_path: abs_path.clone(), name: name.to_string() }));
        ui.close();
    }
}

fn context_menu(ui: &mut egui::Ui, name: &str, full_path: &str, abs_path: &NodePath, action_request: &Cell<Option<TreeAction>>) {
    ui.set_min_width(180.0);
    if ui.button("📂 打开所在文件夹").clicked() {
        open_in_explorer_select(full_path);
        ui.close();
    }
    if ui.button("📋 复制路径").clicked() {
        ui.ctx().copy_text(full_path.to_string());
        ui.close();
    }
    if ui.button("📋 复制名称").clicked() {
        ui.ctx().copy_text(name.to_string());
        ui.close();
    }
    ui.separator();
    if ui.button("ℹ 属性").clicked() {
        crate::file_ops::open_properties(full_path);
        ui.close();
    }
    if ui.button("🔍 检测占用").clicked() {
        action_request.set(Some(TreeAction::RequestCheckLock {
            abs_path: abs_path.clone(),
            name: name.to_string(),
            full_path: full_path.to_string(),
            is_folder: false,
        }));
        ui.close();
    }
    if ui.add(egui::Button::new(egui::RichText::new("🗑 删除到回收站").color(crate::theme::STATUS_ERROR_RED))).clicked() {
        action_request.set(Some(TreeAction::RequestDelete {
            abs_path: abs_path.clone(),
            name: name.to_string(),
            full_path: full_path.to_string(),
            is_folder: false,
            index_entry: None,
        }));
        ui.close();
    }
}

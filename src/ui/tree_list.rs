
use std::cell::{Cell, OnceCell};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;

use egui::{Color32, Pos2, Rect, Sense, Vec2};
use crate::disk_info::DiskInfo;
use crate::format::{format_attributes, format_filetime_local as format_filetime, human_size, human_size_compact};
use crate::model::{Node, NodePath};
use crate::search_index::NameIndex;
use super::{SortDir, SortKey, SortState, TreeAction, Matcher};

const ROW_H: f32 = 22.0;
const DISK_ROW_H: f32 = 68.0;

use crate::theme::HIDDEN_ACCENT;
use crate::theme::REPARSE_ACCENT;

#[derive(Clone, Copy)]
pub enum ListSource<'a> {
    Tree {
        partitions: &'a [Node],
        partition_infos: &'a [Option<DiskInfo>],
        root_paths: &'a [String],
    },
    Indexed { index: &'a Arc<NameIndex> },
}

#[derive(Clone)]
enum RowKind {
    Disk { pi: usize },
    Child {
        pi: usize, node: *const Node, abs_path: NodePath, indent: f32, depth: u32, parent_logical: u64,
        dir_path: Option<String>,
    },
}

#[derive(Clone)]
struct FlatRow {
    height: f32,
    kind: RowKind,
}

struct RowData<'a> {
    name: &'a str,
    is_folder: bool,
    expanded: bool,
    logical_size: u64,
    physical_size: u64,
    modified_ft: u64,
    created_ft: u64,
    accessed_ft: u64,
    attributes: u32,
    reparse_tag: u32,
    is_reserved: bool,
    owner: &'a str,
    file_count: u64,
    folder_count: u64,
    path_ref: PathRef<'a>,
    abs_path_cell: OnceCell<NodePath>,
    indent: f32,
    depth: u32,
    parent_logical: u64,
    disk_logical: u64,
    dir_path: Option<&'a str>,
    full_path_source: FullPathSource<'a>,
    full_path_cell: OnceCell<String>,
    index_entry: Option<u32>,
}

enum PathRef<'a> {
    Tree(&'a NodePath),
    Indexed { index: &'a NameIndex, entry: u32 },
}

enum FullPathSource<'a> {
    Tree { partitions: &'a [Node], root_paths: &'a [String] },
    Indexed { dir: &'a str },
}

impl RowData<'_> {
    fn is_hidden_or_system(&self) -> bool {
        use crate::fs_attrs::{FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM};
        self.attributes & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0
    }
    fn is_reparse_point(&self) -> bool {
        use crate::fs_attrs::FILE_ATTRIBUTE_REPARSE_POINT;
        self.reparse_tag != 0 || self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    fn abs_path(&self) -> &NodePath {
        match &self.path_ref {
            PathRef::Tree(p) => p,
            PathRef::Indexed { index, entry } => self
                .abs_path_cell
                .get_or_init(|| index.abs_path_of(*entry)),
        }
    }
    fn full_path(&self) -> &str {
        self.full_path_cell.get_or_init(|| match &self.full_path_source {
            FullPathSource::Tree { partitions, root_paths } => {
                build_full_path(partitions, root_paths, self.abs_path())
            }
            FullPathSource::Indexed { dir } => {
                if dir.is_empty() { self.name.to_string() } else { format!("{dir}\\{}", self.name) }
            }
        })
    }
    fn is_selected(&self, selected: &Option<NodePath>) -> bool {
        let Some(sel) = selected.as_deref() else { return false };
        match &self.path_ref {
            PathRef::Tree(p) => *p == sel,
            PathRef::Indexed { index, entry } => index.abs_path_eq(*entry, sel),
        }
    }
}

const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(180);

struct SearchView {
    for_query: String,
    for_index: usize,
    for_removed: usize,
    base: Vec<u32>,
    order: Vec<u32>,
    sorted_for: Option<SortState>,
}

struct SortJob {
    sort: SortState,
    rx: Receiver<Vec<u32>>,
}

#[derive(Default)]
pub struct ListState {
    pub sort: SortState,
    pub expand_version: u64,
    cache: Option<(CacheKey, Vec<FlatRow>)>,
    pub search_query: String,
    search_query_applied: String,
    search_query_changed_at: Option<std::time::Instant>,
    search_view: Option<SearchView>,
    sort_job: Option<SortJob>,
    pub removed_entries: std::collections::HashSet<u32>,
    pub pending_scroll: Option<NodePath>,
    pub pending_scroll_entry: Option<u32>,
}

impl ListState {
    pub fn mark_search_row_removed(&mut self, entry: u32) {
        self.removed_entries.insert(entry);
        self.search_view = None;
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
struct CacheKey {
    sort: SortState,
    expand_version: u64,
    show_all: bool,
    partitions_len: usize,
    partitions_ptr: usize,
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

fn compare_nodes(a: &Node, b: &Node, key: SortKey) -> std::cmp::Ordering {
    match key {
        SortKey::Name => cmp_ignore_ascii_case(&a.name, &b.name),
        SortKey::Size => a.logical_size.cmp(&b.logical_size),
        SortKey::Physical => a.physical_size.cmp(&b.physical_size),
        SortKey::Modified => a.modified_ft.cmp(&b.modified_ft),
        SortKey::Created => a.created_ft.cmp(&b.created_ft),
        SortKey::Accessed => a.accessed_ft.cmp(&b.accessed_ft),
        SortKey::Items => (a.file_count + a.folder_count).cmp(&(b.file_count + b.folder_count)),
        SortKey::Files => a.file_count.cmp(&b.file_count),
        SortKey::Folders => a.folder_count.cmp(&b.folder_count),
        SortKey::Attributes => a.attributes.cmp(&b.attributes),
        SortKey::Reparse => a.reparse_tag.cmp(&b.reparse_tag),
        SortKey::Reserved => a.is_reserved.cmp(&b.is_reserved),
        SortKey::Owner => cmp_ignore_ascii_case(&a.owner, &b.owner),
        SortKey::Path => std::cmp::Ordering::Equal,
    }
}

const HEADER_COLS: [(&str, SortKey); 16] = [
    ("名称", SortKey::Name),
    ("父占比", SortKey::Size),
    ("总占比", SortKey::Size),
    ("逻辑大小", SortKey::Size),
    ("修改时间", SortKey::Modified),
    ("物理大小", SortKey::Physical),
    ("创建时间", SortKey::Created),
    ("访问时间", SortKey::Accessed),
    ("项目", SortKey::Items),
    ("文件", SortKey::Files),
    ("文件夹", SortKey::Folders),
    ("属性", SortKey::Attributes),
    ("重解析点", SortKey::Reparse),
    ("保留", SortKey::Reserved),
    ("所有者", SortKey::Owner),
    ("路径", SortKey::Path),
];

#[allow(clippy::too_many_arguments)]
fn collect_rows(
    node: &Node, pi: usize, rel_path: &[usize], depth: u32,
    parent_logical: u64, show_reserved: bool, sort: SortState, rows: &mut Vec<FlatRow>,
) {
    struct Item<'a> { node: &'a Node, rel_path: Vec<usize>, depth: u32, parent_logical: u64 }
    let order = sorted_child_order(&node.children, sort);
    let mut stack: Vec<Item> = order.into_iter().rev()
        .filter(|&i| show_reserved || !node.children[i].is_reserved)
        .map(|i| {
            let mut rp = rel_path.to_vec();
            rp.push(i);
            Item { node: &node.children[i], rel_path: rp, depth, parent_logical }
        }).collect();
    while let Some(item) = stack.pop() {
        let mut abs_path = vec![pi];
        abs_path.extend_from_slice(&item.rel_path);
        let indent = (item.depth + 1) as f32 * 16.0 + 2.0;
        rows.push(FlatRow {
            height: ROW_H,
            kind: RowKind::Child {
                pi,
                node: item.node as *const Node,
                abs_path,
                indent,
                depth: item.depth,
                parent_logical: item.parent_logical,
                dir_path: None,
            },
        });
        if item.node.is_folder() && item.node.expanded {
            let child_parent_logical = item.node.logical_size.max(1);
            let child_order = sorted_child_order(&item.node.children, sort);
            for i in child_order.into_iter().rev()
                .filter(|&i| show_reserved || !item.node.children[i].is_reserved)
            {
                let mut rp = item.rel_path.clone();
                rp.push(i);
                stack.push(Item { node: &item.node.children[i], rel_path: rp, depth: item.depth + 1, parent_logical: child_parent_logical });
            }
        }
    }
}

fn rebuild_tree_cache(partitions: &[Node], show_all: bool, state: &mut ListState) {
    let cache_key = CacheKey {
        sort: state.sort,
        expand_version: state.expand_version,
        show_all,
        partitions_len: partitions.len(),
        partitions_ptr: partitions.as_ptr() as usize,
    };
    let need_rebuild = state.cache.as_ref().is_none_or(|(k, _)| *k != cache_key);
    if need_rebuild {
        let mut flat_rows: Vec<FlatRow> = Vec::new();
        let partition_order = sorted_child_order(partitions, state.sort);
        for pi in partition_order {
            let partition = &partitions[pi];
            flat_rows.push(FlatRow { height: DISK_ROW_H, kind: RowKind::Disk { pi } });
            if partition.expanded {
                let rel_path: NodePath = Vec::new();
                collect_rows(partition, pi, &rel_path, 0, partition.logical_size.max(1), show_all, state.sort, &mut flat_rows);
            }
        }
        state.cache = Some((cache_key, flat_rows));
    }
}

pub fn show(
    ui: &mut egui::Ui,
    source: ListSource<'_>,
    selected: &Option<NodePath>,
    show_all: bool,
    state: &mut ListState,
) -> TreeAction {
    let searching = matches!(source, ListSource::Indexed { .. });

    if let ListSource::Indexed { index } = source {
        ui.horizontal(|ui| {
            ui.label("🔍");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut state.search_query)
                    .hint_text("按名称过滤，支持 *.mp4 这类通配符…")
                    .desired_width(320.0),
            );
            if resp.changed() {
                state.search_query_changed_at = Some(std::time::Instant::now());
            }
        });

        match state.search_query_changed_at {
            Some(t) if t.elapsed() >= SEARCH_DEBOUNCE => {
                state.search_query_applied = state.search_query.clone();
                state.search_query_changed_at = None;
            }
            Some(t) => {
                ui.ctx().request_repaint_after(SEARCH_DEBOUNCE - t.elapsed());
            }
            None => {}
        }

        let index_id = Arc::as_ptr(index) as usize;
        let applied = state.search_query_applied.clone();
        let need_rebuild = match &state.search_view {
            Some(v) => v.for_query != applied || v.for_index != index_id || v.for_removed != state.removed_entries.len(),
            None => true,
        };
        if need_rebuild {
            let mut base: Vec<u32> = if applied.trim().is_empty() {
                index.all_file_entries()
            } else {
                Matcher::build_auto(&applied).find_in_index(index)
            };
            if !state.removed_entries.is_empty() {
                base.retain(|e| !state.removed_entries.contains(e));
            }
            let order = base.clone();
            state.search_view = Some(SearchView {
                for_query: applied,
                for_index: index_id,
                for_removed: state.removed_entries.len(),
                base,
                order,
                sorted_for: None,
            });
            state.sort_job = None;
        }
        let sort_needed = state
            .search_view
            .as_ref()
            .map(|v| v.sorted_for != Some(state.sort))
            .unwrap_or(false);
        if sort_needed && state.sort_job.as_ref().map(|j| j.sort) != Some(state.sort) {
            let (tx, rx) = mpsc::channel();
            let index_for_sort = Arc::clone(index);
            let base = state.search_view.as_ref().map(|v| v.base.clone()).unwrap_or_default();
            let key = state.sort;
            std::thread::spawn(move || {
                let mut order = base;
                index_for_sort.sort_order(&mut order, key.key, key.dir);
                let _ = tx.send(order);
            });
            state.sort_job = Some(SortJob { sort: state.sort, rx });
        }
        if let Some(job) = &state.sort_job {
            match job.rx.try_recv() {
                Ok(order) => {
                    if let Some(v) = state.search_view.as_mut() {
                        v.order = order;
                        v.sorted_for = Some(job.sort);
                    }
                    state.sort_job = None;
                }
                Err(TryRecvError::Empty) => {
                    ui.ctx().request_repaint();
                }
                Err(TryRecvError::Disconnected) => {
                    state.sort_job = None;
                }
            }
        }
    }

    let action_cell: Cell<TreeAction> = Cell::new(TreeAction::None);
    let extra_w = |normal: f32| if show_all { normal } else { 0.0 };
    let path_col_w = if searching { 220.0 } else { 0.0 };
    let pending_scroll = state.pending_scroll.take();
    let pending_scroll_entry = state.pending_scroll_entry.take();
    let mut scroll_area = egui::ScrollArea::both().auto_shrink([false, false]);
    if pending_scroll.is_some() {
        scroll_area = scroll_area.horizontal_scroll_offset(0.0);
    }
    scroll_area.show(ui, |ui| {
        ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
        let mut builder = egui_extras::TableBuilder::new(ui)
            .id_salt(("tree_list_table", show_all, searching))
            .striped(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .auto_shrink([false, false])
            .column(egui_extras::Column::initial(200.0).at_least(80.0).clip(true).resizable(true))
            .column(egui_extras::Column::initial(85.0).clip(true).resizable(true))
            .column(egui_extras::Column::initial(85.0).clip(true).resizable(true))
            .column(egui_extras::Column::initial(85.0).clip(true).resizable(true))
            .column(egui_extras::Column::initial(120.0).clip(true).resizable(true))
            .column(egui_extras::Column::initial(85.0).clip(true).resizable(true))
            .column(egui_extras::Column::initial(extra_w(120.0)).clip(true).resizable(show_all))
            .column(egui_extras::Column::initial(extra_w(120.0)).clip(true).resizable(show_all))
            .column(egui_extras::Column::initial(extra_w(55.0)).clip(true).resizable(show_all))
            .column(egui_extras::Column::initial(extra_w(55.0)).clip(true).resizable(show_all))
            .column(egui_extras::Column::initial(extra_w(55.0)).clip(true).resizable(show_all))
            .column(egui_extras::Column::initial(extra_w(50.0)).clip(true).resizable(show_all))
            .column(egui_extras::Column::initial(extra_w(55.0)).clip(true).resizable(show_all))
            .column(egui_extras::Column::initial(extra_w(40.0)).clip(true).resizable(show_all))
            .column(egui_extras::Column::initial(extra_w(80.0)).clip(true).resizable(show_all).at_least(0.0))
            .column(egui_extras::Column::initial(path_col_w).clip(true).resizable(searching).at_least(0.0));

        builder = builder.sense(egui::Sense::click());

        if let Some(target) = &pending_scroll {
            if searching {
                if let ListSource::Indexed { index } = source
                    && let Some(v) = &state.search_view {
                        let hit = if let Some(e) = pending_scroll_entry {
                            v.order.iter().position(|&x| x == e)
                        } else {
                            v.order.iter().position(|&e| index.abs_path_eq(e, target))
                        };
                        if let Some(idx) = hit {
                            builder = builder.scroll_to_row(idx, Some(egui::Align::Center));
                        }
                    }
            } else if let ListSource::Tree { partitions, .. } = source {
                rebuild_tree_cache(partitions, show_all, state);
                if let Some((_, rows)) = &state.cache
                    && let Some(idx) = rows.iter().position(|r| match &r.kind {
                        RowKind::Disk { pi } => target.len() == 1 && target[0] == *pi,
                        RowKind::Child { abs_path, .. } => abs_path == target,
                    }) {
                        builder = builder.scroll_to_row(idx, Some(egui::Align::Center));
                    }
            }
        }

        let sort_clicked_cell: Cell<Option<SortKey>> = Cell::new(None);
        let table = builder.header(ROW_H, |mut h| {
            for (label, key) in HEADER_COLS {
                h.col(|ui| {
                    let active = state.sort.key == key;
                    let arrow = if active {
                        if state.sort.dir == SortDir::Asc { " ▲" } else { " ▼" }
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
        if let Some(key) = sort_clicked_cell.get() { state.sort.click(key); }

        if !searching
            && let ListSource::Tree { partitions, .. } = source {
                rebuild_tree_cache(partitions, show_all, state);
            }
        let empty_rows: Vec<FlatRow> = Vec::new();
        let (tree_rows, order_rows): (&Vec<FlatRow>, &[u32]) = if searching {
            (&empty_rows, state.search_view.as_ref().map(|v| v.order.as_slice()).unwrap_or(&[]))
        } else {
            (state.cache.as_ref().map(|(_, r)| r).unwrap_or(&empty_rows), &[])
        };
        let total_rows = if searching { order_rows.len() } else { tree_rows.len() };
        let heights: Box<dyn ExactSizeIterator<Item = f32>> = if searching {
            Box::new(std::iter::repeat_n(ROW_H, total_rows))
        } else {
            Box::new(tree_rows.iter().map(|r| r.height))
        };

        table
            .body(|body| {
                let mut final_action = TreeAction::None;

                let clicked_row: Cell<usize> = Cell::new(usize::MAX);
                let secondary_clicked_row: Cell<usize> = Cell::new(usize::MAX);
                let action_request: Cell<Option<TreeAction>> = Cell::new(None);

                #[allow(clippy::large_enum_variant)]
                enum Prepared<'a> {
                    Disk { pi: usize },
                    Child(RowData<'a>),
                }

                body.heterogeneous_rows(heights.into_iter(), |mut row| {
                    let row_idx = row.index();
                    let prepared: Option<Prepared<'_>> = match source {
                        ListSource::Tree { partitions, root_paths, .. } => {
                            if row_idx >= tree_rows.len() { return; }
                            match &tree_rows[row_idx].kind {
                                RowKind::Disk { pi } => Some(Prepared::Disk { pi: *pi }),
                                RowKind::Child { pi, node, abs_path, indent, depth, parent_logical, dir_path } => {
                                    let child = unsafe { &**node };
                                    Some(Prepared::Child(RowData {
                                        name: &child.name,
                                        is_folder: child.is_folder(),
                                        expanded: child.expanded,
                                        logical_size: child.logical_size,
                                        physical_size: child.physical_size,
                                        modified_ft: child.modified_ft,
                                        created_ft: child.created_ft,
                                        accessed_ft: child.accessed_ft,
                                        attributes: child.attributes,
                                        reparse_tag: child.reparse_tag,
                                        is_reserved: child.is_reserved,
                                        owner: &child.owner,
                                        file_count: child.file_count,
                                        folder_count: child.folder_count,
                                        path_ref: PathRef::Tree(abs_path),
                                        abs_path_cell: OnceCell::new(),
                                        indent: *indent,
                                        depth: *depth,
                                        parent_logical: *parent_logical,
                                        disk_logical: partitions[*pi].logical_size.max(1),
                                        dir_path: dir_path.as_deref(),
                                        full_path_source: FullPathSource::Tree { partitions, root_paths },
                                        full_path_cell: OnceCell::new(),
                                        index_entry: None,
                                    }))
                                }
                            }
                        }
                        ListSource::Indexed { index } => {
                            if row_idx >= order_rows.len() { return; }
                            let e = order_rows[row_idx] as usize;
                            let entry = index.entry(e);
                            let dir = index.dir_path(e);
                            let name = index.name_orig(e);
                            Some(Prepared::Child(RowData {
                                name,
                                is_folder: !entry.is_file(),
                                expanded: false,
                                logical_size: entry.logical_size,
                                physical_size: entry.physical_size,
                                modified_ft: entry.modified_ft,
                                created_ft: entry.created_ft,
                                accessed_ft: entry.accessed_ft,
                                attributes: entry.attributes,
                                reparse_tag: entry.reparse_tag,
                                is_reserved: entry.is_reserved(),
                                owner: index.owner(e),
                                file_count: entry.file_count as u64,
                                folder_count: entry.folder_count as u64,
                                path_ref: PathRef::Indexed { index, entry: e as u32 },
                                abs_path_cell: OnceCell::new(),
                                indent: 18.0,
                                depth: 0,
                                parent_logical: entry.parent_logical,
                                disk_logical: index.root_logical(entry.pi as usize).max(1),
                                dir_path: Some(dir),
                                full_path_source: FullPathSource::Indexed { dir },
                                full_path_cell: OnceCell::new(),
                                index_entry: Some(e as u32),
                            }))
                        }
                    };

                    if let Some(Prepared::Disk { pi }) = &prepared {
                        let pi = *pi;
                        let ListSource::Tree { partitions, partition_infos, root_paths } = source else { return; };
                        let partition = &partitions[pi];
                        let info = partition_infos.get(pi).and_then(|i| i.as_ref());
                        let part_selected = selected.as_deref() == Some(&[pi]);
                        let total = info.map(|i| i.total_bytes).unwrap_or(partition.logical_size.max(1));
                        let part_pct = if total > 0 { partition.logical_size as f32 / total as f32 } else { 0.0 };
                        let p = partition;
                        let info_ref = info;
                        let root_path = root_paths.get(pi).cloned().unwrap_or_default();

                        row.col(|ui| {
                            let rect = ui.available_rect_before_wrap();
                            let resp = ui.allocate_rect(rect, Sense::click());
                            if part_selected { ui.painter().rect_filled(rect, 0.0, crate::theme::SELECT_BG); }
                            let arrow = if p.expanded { "▼" } else { "▶" };
                            ui.painter().text(Pos2::new(rect.min.x+2.0, rect.min.y+6.0), egui::Align2::LEFT_TOP, arrow, egui::FontId::proportional(10.0), Color32::from_rgb(0xAA,0xCC,0xFF));
                            ui.painter().text(Pos2::new(rect.min.x+18.0, rect.min.y+4.0), egui::Align2::LEFT_TOP, format!("💾 {}", p.name), egui::FontId::proportional(13.0), if part_selected {Color32::from_rgb(0xFF,0xFF,0x80)} else {crate::theme::HEADER_ACTIVE_YELLOW});
                            if let Some(i) = info_ref {
                                ui.painter().text(Pos2::new(rect.min.x+18.0, rect.min.y+22.0), egui::Align2::LEFT_TOP, format!("总: {}  已用: {}  可用: {}", human_size_compact(i.total_bytes), human_size_compact(i.used_bytes), human_size_compact(i.free_bytes)), egui::FontId::proportional(10.0), Color32::from_rgb(0xA0,0xC0,0xE0));
                                ui.painter().text(Pos2::new(rect.min.x+18.0, rect.min.y+36.0), egui::Align2::LEFT_TOP, format!("扫描: 逻辑={}  物理={}", human_size_compact(p.logical_size), human_size_compact(p.physical_size)), egui::FontId::proportional(10.0), Color32::from_rgb(0xA0,0xA0,0xA0));
                            }
                            if resp.clicked() { clicked_row.set(row_idx); }
                            if resp.secondary_clicked() { secondary_clicked_row.set(row_idx); }
                            resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request));
                        });
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } draw_bar(ui.painter(),r,1.0,crate::theme::HEADER_ACTIVE_YELLOW); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } draw_bar(ui.painter(),r,part_pct,crate::theme::ACCENT_BLUE); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,human_size(p.logical_size),egui::FontId::proportional(11.0),crate::theme::ACCENT_BLUE); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } let t=info_ref.map(|i|i.file_system.clone()).filter(|s|!s.is_empty()).unwrap_or_else(||format_filetime(p.modified_ft)); ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,t,egui::FontId::proportional(11.0),Color32::from_rgb(0xA0,0xC0,0xE0)); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,human_size(p.physical_size),egui::FontId::proportional(11.0),Color32::from_rgb(0xF5,0xA6,0x23)); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } let s=format_filetime(p.created_ft); ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,if s.is_empty(){"—".into()}else{s},egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0)); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } let s=format_filetime(p.accessed_ft); ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,if s.is_empty(){"—".into()}else{s},egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0)); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        for val in [p.file_count+p.folder_count, p.file_count, p.folder_count] {
                            row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,format!("{}",val),egui::FontId::proportional(11.0),Color32::from_rgb(0xC0,0xC0,0xC0)); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        }
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,format_attributes(p.attributes),egui::FontId::proportional(11.0),Color32::from_rgb(0xC0,0xC0,0xC0)); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } let t=if p.reparse_tag!=0 {format!("0x{:X}",p.reparse_tag)}else{"—".into()}; ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,t,egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0)); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } let t=if p.is_reserved {"是"}else{"—"}; ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,t,egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0)); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } let t=if p.owner.is_empty(){"—".into()}else{p.owner.clone()}; ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,t,egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0)); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        return;
                    }

                    let Some(Prepared::Child(rd)) = &prepared else { return; };
                    let is_folder = rd.is_folder;
                    let is_selected = rd.is_selected(selected);
                    let pct = if rd.parent_logical > 0 { rd.logical_size as f32 / rd.parent_logical as f32 } else { 0.0 };
                    let total_pct = if rd.disk_logical > 0 { rd.logical_size as f32 / rd.disk_logical as f32 } else { 0.0 };
                    let bar_color = depth_color(rd.depth, is_folder);
                    let hidden = rd.is_hidden_or_system();
                    let is_reparse = rd.is_reparse_point();

                            row.col(|ui| {
                                let rect = ui.available_rect_before_wrap();
                                let resp = ui.allocate_rect(rect, Sense::click());
                                if is_selected { ui.painter().rect_filled(rect, 0.0, crate::theme::SELECT_BG); }
                                let guide_color = Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 0x14);
                                for lvl in 0..=rd.depth {
                                    let x = rect.min.x + lvl as f32 * 16.0 + 10.0;
                                    ui.painter().line_segment(
                                        [Pos2::new(x, rect.min.y), Pos2::new(x, rect.max.y)],
                                        egui::Stroke::new(1.0, guide_color),
                                    );
                                }
                                let p = ui.painter();
                                if !searching && is_folder { p.text(Pos2::new(rect.min.x+rd.indent,rect.center().y),egui::Align2::LEFT_CENTER,if rd.expanded{"▼"}else{"▶"},egui::FontId::proportional(10.0),Color32::from_rgb(0xAA,0xCC,0xFF)); }
                                let icon = if is_folder {"📁"} else {"📄"};
                                let tc = if is_selected {Color32::from_rgb(0xFF,0xFF,0x80)}
                                    else if is_reparse {crate::theme::REPARSE_TEXT}
                                    else if is_folder {Color32::WHITE} else {Color32::from_rgb(0xCC,0xCC,0xCC)};
                                let mut text_x = rect.min.x + rd.indent + 16.0;
                                let hidden_badge = if hidden {
                                    let badge = Rect::from_min_size(Pos2::new(text_x, rect.center().y - 7.0), Vec2::new(14.0, 14.0));
                                    text_x += 18.0;
                                    Some(badge)
                                } else { None };
                                let reparse_badge = if is_reparse {
                                    let badge = Rect::from_min_size(Pos2::new(text_x, rect.center().y - 7.0), Vec2::new(14.0, 14.0));
                                    text_x += 18.0;
                                    Some(badge)
                                } else { None };
                                let name_text = format!("{icon} {}", rd.name);
                                p.text(Pos2::new(text_x,rect.center().y),egui::Align2::LEFT_CENTER,name_text,egui::FontId::proportional(13.0),tc);
                                if hidden {
                                    p.rect_filled(rect, 0.0, crate::theme::DIM_OVERLAY);
                                }
                                if let Some(badge) = hidden_badge {
                                    p.rect_filled(badge, 3.0, HIDDEN_ACCENT);
                                    p.text(badge.center(), egui::Align2::CENTER_CENTER, "H", egui::FontId::proportional(9.5), Color32::WHITE);
                                }
                                if let Some(badge) = reparse_badge {
                                    p.rect_filled(badge, 3.0, REPARSE_ACCENT);
                                    p.text(badge.center(), egui::Align2::CENTER_CENTER, "L", egui::FontId::proportional(9.5), Color32::WHITE);
                                }
                                let resp = if resp.hovered() && (searching || is_reparse) {
                                    let mut hover_lines: Vec<&str> = Vec::new();
                                    if searching { hover_lines.push(rd.full_path()); }
                                    if is_reparse {
                                        hover_lines.push("这是符号链接 / junction / 挂载点，指向磁盘上别的位置——本身不占用这里显示的实际空间");
                                    }
                                    if hover_lines.is_empty() { resp } else { resp.on_hover_text(hover_lines.join("\n")) }
                                } else { resp };
                                if resp.clicked() {clicked_row.set(row_idx);}
                                if resp.secondary_clicked() {secondary_clicked_row.set(row_idx);}
                                resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request));
                            });
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }draw_bar(ui.painter(),r,pct,bar_color);if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }draw_bar(ui.painter(),r,total_pct,crate::theme::ACCENT_BLUE);if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,human_size(rd.logical_size),egui::FontId::proportional(11.0),crate::theme::ACCENT_BLUE);if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }let s=format_filetime(rd.modified_ft);ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,if s.is_empty(){"—".into()}else{s},egui::FontId::proportional(11.0),Color32::from_rgb(0xC0,0xC0,0xC0));if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,human_size(rd.physical_size),egui::FontId::proportional(11.0),Color32::from_rgb(0xF5,0xA6,0x23));if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }let s=format_filetime(rd.created_ft);ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,if s.is_empty(){"—".into()}else{s},egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0));if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }let s=format_filetime(rd.accessed_ft);ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,if s.is_empty(){"—".into()}else{s},egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0));if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            for val in [if is_folder{rd.file_count+rd.folder_count}else{0}, if is_folder{rd.file_count}else{0}, if is_folder{rd.folder_count}else{0}] {
                                row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }let t=if is_folder{format!("{}",val)}else{"—".into()};ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,t,egui::FontId::proportional(11.0),Color32::from_rgb(0xC0,0xC0,0xC0));if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            }
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,format_attributes(rd.attributes),egui::FontId::proportional(11.0),Color32::from_rgb(0xC0,0xC0,0xC0));if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }let t=if rd.reparse_tag!=0{format!("0x{:X}",rd.reparse_tag)}else{"—".into()};ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,t,egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0));if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }let t=if rd.is_reserved{"是"}else{"—"};ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,t,egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0));if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }let t=if rd.owner.is_empty(){"—".into()}else{rd.owner.to_string()};ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,t,egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0));if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }; if let Some(dir) = rd.dir_path { ui.painter().text(r.left_center()+egui::vec2(4.0,0.0),egui::Align2::LEFT_CENTER,dir,egui::FontId::proportional(11.5),Color32::from_rgb(0xA0,0xA0,0xA0)); }; if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                    });

                let clicked_idx = clicked_row.into_inner();
                if clicked_idx != usize::MAX && clicked_idx < total_rows {
                    if searching {
                        if let ListSource::Indexed { index } = source {
                            final_action = TreeAction::Select(index.abs_path_of(order_rows[clicked_idx]));
                        }
                    } else {
                        match &tree_rows[clicked_idx].kind {
                            RowKind::Disk { pi } => {
                                final_action = TreeAction::ToggleExpand(vec![*pi]);
                            }
                            RowKind::Child { node, abs_path, .. } => {
                                let child = unsafe { &**node };
                                let abs = abs_path.clone();
                                final_action = if child.is_folder() {
                                    TreeAction::ToggleExpand(abs)
                                } else {
                                    TreeAction::Select(abs)
                                };
                            }
                        }
                    }
                } else {
                    let right_idx = secondary_clicked_row.into_inner();
                    if right_idx != usize::MAX && right_idx < total_rows {
                        let abs = if searching {
                            if let ListSource::Indexed { index } = source {
                                index.abs_path_of(order_rows[right_idx])
                            } else {
                                Vec::new()
                            }
                        } else {
                            match &tree_rows[right_idx].kind {
                                RowKind::Disk { pi } => vec![*pi],
                                RowKind::Child { abs_path, .. } => abs_path.clone(),
                            }
                        };
                        final_action = TreeAction::Select(abs);
                    }
                }
                let _ = &mut final_action;
                if let Some(action) = action_request.into_inner() {
                    final_action = action;
                }
                action_cell.set(final_action);
            });
    });
    action_cell.into_inner()
}

fn draw_bar(painter: &egui::Painter, cell: Rect, pct: f32, color: Color32) {
    let pad = 4.0; let bar_h = 10.0;
    let bar_w = (cell.width() - pad*2.0).max(0.0);
    let br = Rect::from_min_size(Pos2::new(cell.min.x+pad, cell.center().y-bar_h/2.0), Vec2::new(bar_w, bar_h));
    painter.rect_filled(br, 2.0, Color32::from_rgb(0x48,0x48,0x52));
    let fill_w = (bar_w * pct.clamp(0.0,1.0)).max(0.0);
    if fill_w > 0.5 { painter.rect_filled(Rect::from_min_size(br.min, Vec2::new(fill_w, bar_h)), 2.0, color); }
    painter.text(br.center(), egui::Align2::CENTER_CENTER, format!("{:.2}%", pct*100.0), egui::FontId::proportional(9.5), Color32::WHITE);
}

fn depth_color(depth: u32, is_folder: bool) -> Color32 {
    if !is_folder { return crate::theme::FILE_COLOR; }
    const PAL: [Color32;6] = [crate::theme::ACCENT_BLUE,Color32::from_rgb(0x34,0xC7,0x59),Color32::from_rgb(0xF5,0xA6,0x23),Color32::from_rgb(0xE0,0x55,0x5B),Color32::from_rgb(0x9C,0x6A,0xDE),Color32::from_rgb(0x2E,0xC4,0xB6)];
    PAL[depth as usize % PAL.len()]
}

fn build_full_path(partitions: &[Node], root_paths: &[String], abs_path: &[usize]) -> String {
    let Some(&pi) = abs_path.first() else { return String::new() };
    let mut path = root_paths.get(pi).cloned().unwrap_or_default().trim_end_matches('\\').to_string();
    let Some(mut cur) = partitions.get(pi) else { return path };
    for &i in &abs_path[1..] {
        let Some(n) = cur.children.get(i) else { break };
        cur = n;
        if path.is_empty() { path = cur.name.clone(); } else { path.push('\\'); path.push_str(&cur.name); }
    }
    if let Some(real) = &cur.full_path_override {
        return real.clone();
    }
    path
}

#[cfg(windows)]
fn open_in_explorer(path: &str, select_self: bool) {
    use std::os::windows::process::CommandExt;
    if path.is_empty() { return; }
    let (cmd_desc, result) = if select_self {
        let arg = format!("/select,\"{path}\"");
        let r = std::process::Command::new("explorer").raw_arg(&arg).spawn();
        (format!("explorer {arg}"), r)
    } else {
        let r = std::process::Command::new("explorer").arg(path).spawn();
        (format!("explorer {path}"), r)
    };
    crate::applog::log(&format!("[tree_list] 打开资源管理器: {cmd_desc}"));
    if let Err(e) = result {
        crate::applog::log(&format!("[tree_list] 打开资源管理器失败 ({path}): {e}"));
    }
}
#[cfg(not(windows))]
fn open_in_explorer(_path: &str, _select_self: bool) {}

#[allow(clippy::too_many_arguments)]
fn context_menu(
    ui: &mut egui::Ui, is_folder: bool, is_reparse: bool, name: &str, full_path: &str,
    abs_path: &NodePath, index_entry: Option<u32>, action_request: &Cell<Option<TreeAction>>,
) {
    ui.set_min_width(180.0);
    let open_label = if is_folder { "📂 在资源管理器中打开" } else { "📂 打开所在文件夹" };
    if ui.button(open_label).clicked() {
        open_in_explorer(full_path, !is_folder);
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
    let resolve_btn = egui::Button::new("🎯 定位真实路径");
    let resolve_resp = ui.add_enabled(is_reparse, resolve_btn);
    let resolve_resp = if is_reparse {
        resolve_resp.on_hover_text("解析这个符号链接/junction 指向的真实位置，并在资源管理器里定位它")
    } else {
        resolve_resp.on_disabled_hover_text("只有符号链接/junction/挂载点才有\"真实路径\"，普通文件没有")
    };
    if resolve_resp.clicked() {
        action_request.set(Some(TreeAction::RequestResolveSymlink {
            name: name.to_string(),
            full_path: full_path.to_string(),
        }));
        ui.close();
    }
    if ui.button("🔍 检测占用").clicked() {
        action_request.set(Some(TreeAction::RequestCheckLock {
            abs_path: abs_path.clone(),
            name: name.to_string(),
            full_path: full_path.to_string(),
            is_folder,
        }));
        ui.close();
    }
    let symlink_btn = egui::Button::new("🔗 创建符号链接 / 迁移到其他盘");
    let symlink_resp = ui.add_enabled(!is_reparse, symlink_btn);
    let symlink_resp = if is_reparse {
        symlink_resp.on_disabled_hover_text("这一项本身就是符号链接/junction/挂载点，不支持再次创建/迁移")
    } else {
        symlink_resp
    };
    if symlink_resp.clicked() {
        action_request.set(Some(TreeAction::RequestCreateSymlink {
            abs_path: abs_path.clone(),
            name: name.to_string(),
            full_path: full_path.to_string(),
            is_folder,
        }));
        ui.close();
    }
    if ui.add(egui::Button::new(egui::RichText::new("🗑 删除到回收站").color(crate::theme::STATUS_ERROR_RED))).clicked() {
        action_request.set(Some(TreeAction::RequestDelete {
            abs_path: abs_path.clone(),
            name: name.to_string(),
            full_path: full_path.to_string(),
            is_folder,
            index_entry,
        }));
        ui.close();
    }
}

fn context_menu_disk(ui: &mut egui::Ui, pi: usize, root_path: &str, action_request: &Cell<Option<TreeAction>>) {
    ui.set_min_width(180.0);
    if ui.button("📂 在资源管理器中打开").clicked() {
        open_in_explorer(root_path, false);
        ui.close();
    }
    if ui.button("📋 复制路径").clicked() {
        ui.ctx().copy_text(root_path.to_string());
        ui.close();
    }
    if ui.button("ℹ 属性").clicked() {
        crate::file_ops::open_properties(root_path);
        ui.close();
    }
    ui.separator();
    if ui.button("🗐 文件扩展名分类").on_hover_text("只看这一个分区（顶部菜单的同名功能是全部分区一起看）").clicked() {
        action_request.set(Some(TreeAction::RequestExtensionBreakdown(pi)));
        ui.close();
    }
    if ui.button("🔍 重复文件查找").on_hover_text("只看这一个分区（顶部菜单的同名功能是全部分区一起找，还能找出跨盘的重复文件）").clicked() {
        action_request.set(Some(TreeAction::RequestDuplicateFinder(pi)));
        ui.close();
    }
    ui.separator();
    if ui.button("🔄 重新扫描").on_hover_text("重新扫描这个分区/目录，用最新结果原地替换（不影响列表里的其它分区）").clicked() {
        action_request.set(Some(TreeAction::RequestRescan(pi)));
        ui.close();
    }
    if ui.button("✖ 从列表移除").on_hover_text("只是不在这个列表里显示了，不会删除磁盘上的任何文件；想再看到的话重新扫描一次就行").clicked() {
        action_request.set(Some(TreeAction::RequestRemovePartition(pi)));
        ui.close();
    }
}



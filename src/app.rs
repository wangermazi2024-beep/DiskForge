
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

use egui::{Color32, RichText};

use crate::categorize;
use crate::disk_info::{self, DiskInfo};
use crate::export;
use crate::model::{CategoryStat, Node, NodePath};
use crate::scan::{self, ScanMessage};
use crate::search_index::{BuildStep, IndexBuilder, NameIndex};
use crate::ui::topbar::{self, TopbarAction, TopbarState};
use crate::ui::{sidebar, startup, tree_list, TreeAction};

enum Tab {
    Main,
    Extensions { partition_idx: Option<usize>, title: String, root: Node, selected: Option<NodePath>, view: crate::ui::compact_tree::ViewState },
    Duplicates {
        partition_idx: Option<usize>, title: String, root: Node, selected: Option<NodePath>, view: crate::ui::compact_tree::ViewState,
        loading: Option<(crate::dedup::HashPhase, u64, u64)>,
    },
    SearchList { title: String, index: Option<Arc<NameIndex>>, selected: Option<NodePath>, list_state: tree_list::ListState },
    CopyList {
        title: String,
        index: Option<Arc<NameIndex>>,
        tree: Vec<Node>,
        partition_infos: Vec<Option<DiskInfo>>,
        root_paths: Vec<String>,
        selected: Option<NodePath>,
        list_state: tree_list::ListState,
        loading: Option<CopyLoading>,
    },
}

enum CopyLoading {
    WaitIndex,
    Building(Receiver<Vec<Node>>),
}

struct PendingDelete {
    source: DeleteSource,
    abs_path: NodePath,
    name: String,
    full_path: String,
    is_folder: bool,
    index_entry: Option<u32>,
}

#[derive(Clone, Copy)]
enum DeleteSource {
    Main,
    Tab(usize),
}

struct SymlinkRequest {
    source: DeleteSource,
    abs_path: NodePath,
    name: String,
    full_path: String,
    is_folder: bool,
}

enum SymlinkOutcome {
    Single { target_path: String },
    Group { target_path: String, member_count: usize, total_count: usize },
}

struct StatusMsg {
    text: String,
    is_error: bool,
    set_at: std::time::Instant,
}

impl StatusMsg {
    fn info(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: false, set_at: std::time::Instant::now() }
    }
    fn error(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: true, set_at: std::time::Instant::now() }
    }
}

const STATUS_MSG_TTL: std::time::Duration = std::time::Duration::from_secs(8);

const INDEX_STEP_BUDGET_MAIN: std::time::Duration = std::time::Duration::from_millis(6);
const INDEX_STEP_BUDGET_VIEW: std::time::Duration = std::time::Duration::from_millis(4);

enum ExportMessage {
    Progress { partition: String, rows: u64 },
    Done { ok: usize, total: usize, dir: PathBuf },
}

struct LockCheckResult {
    request: LockCheckRequest,
    name: String,
    is_folder: bool,
    #[allow(dead_code)]
    checked_count: usize,
    #[allow(dead_code)]
    truncated: bool,
    procs: Vec<crate::file_ops::LockingProcess>,
    error: Option<String>,
    action_feedback: Option<String>,
    loading: bool,
    rename_probe: Option<crate::file_ops::FolderOccupancy>,
}

#[derive(Clone)]
struct LockCheckRequest {
    tab_idx: usize,
    #[allow(dead_code)]
    is_view_tab: bool,
    abs_path: NodePath,
    name: String,
    full_path: String,
    is_folder: bool,
    is_group: bool,
}

type LockCheckJob = (LockCheckPending, Receiver<Result<Vec<crate::file_ops::LockingProcess>, String>>);

struct LockCheckPending {
    request: LockCheckRequest,
    action_feedback: Option<String>,
    is_folder: bool,
    checked_count: usize,
    truncated: bool,
    rename_probe: Option<crate::file_ops::FolderOccupancy>,
}

struct LayeredProbe {
    root_full_path: String,
    #[allow(dead_code)]
    root_name: String,
    frontier: Vec<String>,
    layer: usize,
    procs: Vec<crate::file_ops::LockingProcess>,
    exhausted: bool,
    pending: Option<LayeredProbePending>,
    last_layer_stats: Option<LayerStats>,
}

struct LayeredProbePending {
    rx: Receiver<Result<Vec<crate::file_ops::LockingProcess>, String>>,
    next_frontier: Vec<String>,
    layer_just_finished: usize,
    subfolders_checked: usize,
    subfolders_locked: usize,
    files_checked: usize,
}

struct LayerStats {
    layer: usize,
    files_checked: usize,
    subfolders_checked: usize,
    subfolders_locked: usize,
    new_procs_found: usize,
}

fn collect_find_matches_main_shaped(partitions: &[Node], matcher: &crate::ui::Matcher) -> Vec<NodePath> {
    let mut out = Vec::new();
    for (pi, root) in partitions.iter().enumerate() {
        let mut stack: Vec<(NodePath, &Node)> = vec![(vec![pi], root)];
        while let Some((path, node)) = stack.pop() {
            if path.len() > 1 && matcher.is_match(&node.name) {
                out.push(path.clone());
            }
            for (i, child) in node.children.iter().enumerate().rev() {
                let mut p = path.clone();
                p.push(i);
                stack.push((p, child));
            }
        }
    }
    out
}

fn collect_find_matches_view(root: &Node, matcher: &crate::ui::Matcher) -> Vec<NodePath> {
    let mut out = Vec::new();
    for (gi, group) in root.children.iter().enumerate() {
        if matcher.is_match(&group.name) {
            out.push(vec![0, gi]);
        }
        for (ci, child) in group.children.iter().enumerate() {
            if matcher.is_match(&child.name) {
                out.push(vec![0, gi, ci]);
            }
        }
    }
    out
}

fn collect_find_matches_index(
    idx: &Arc<crate::search_index::NameIndex>,
    matcher: &crate::ui::Matcher,
) -> FindMatches {
    let entries = matcher.find_in_index(idx);
    FindMatches::Entries { entries, index: Arc::clone(idx) }
}

enum FindMatches {
    Entries { entries: Vec<u32>, index: Arc<crate::search_index::NameIndex> },
    Paths(Vec<NodePath>),
}

fn reveal_in_main_shaped_tree(
    partitions: &mut [Node],
    list_state: &mut tree_list::ListState,
    selected: &mut Option<NodePath>,
    path: NodePath,
) {
    if let Some(&pi) = path.first()
        && let Some(part) = partitions.get_mut(pi) {
            let mut expanded_changed = !part.expanded;
            part.expanded = true;
            for depth in 1..path.len().saturating_sub(1) {
                let sub = &path[1..=depth];
                let already_expanded = part.navigate(sub).map(|n| n.expanded).unwrap_or(true);
                if !already_expanded {
                    part.toggle_expand(sub);
                    expanded_changed = true;
                }
            }
            if expanded_changed {
                list_state.expand_version += 1;
            }
        }
    list_state.pending_scroll = Some(path.clone());
    *selected = Some(path);
}

fn reveal_in_view_tree(
    root: &mut Node,
    view: &mut crate::ui::compact_tree::ViewState,
    selected: &mut Option<NodePath>,
    path: NodePath,
) {
    let mut expanded_changed = !root.expanded;
    root.expanded = true;
    for depth in 1..path.len().saturating_sub(1) {
        let sub = &path[1..=depth];
        let already_expanded = root.navigate(sub).map(|n| n.expanded).unwrap_or(true);
        if !already_expanded {
            root.toggle_expand(sub);
            expanded_changed = true;
        }
    }
    if expanded_changed {
        view.expand_version += 1;
    }
    view.pending_scroll = Some(path.clone());
    *selected = Some(path);
}

pub struct DiskForgeApp {
    partitions: Vec<Node>,
    partition_infos: Vec<Option<DiskInfo>>,
    partition_categories: Vec<Vec<CategoryStat>>,
    partition_root_paths: Vec<String>,
    selected: Option<NodePath>,

    tabs: Vec<Tab>,
    active_tab: usize,

    scanning: bool,
    scanned_count: u64,
    scan_error: Option<String>,
    scan_rx: Option<Receiver<ScanMessage>>,
    scan_queue: VecDeque<PathBuf>,
    current_scan_path: Option<PathBuf>,
    rescan_target: Option<usize>,

    picker: Option<startup::PickerState>,

    show_all_details: bool,

    list_state: tree_list::ListState,

    pending_delete: Option<PendingDelete>,

    duplicate_rx: Vec<(Option<usize>, Receiver<categorize::DuplicateMessage>)>,

    delete_rx: Option<(PendingDelete, Receiver<Result<(), String>>)>,

    lock_check_result: Option<LockCheckResult>,

    symlink_rx: Option<(SymlinkRequest, Receiver<Result<SymlinkOutcome, String>>)>,

    export_rx: Option<Receiver<ExportMessage>>,

    status_message: Option<StatusMsg>,

    pending_symlink_pick: Option<PendingSymlinkKind>,
    symlink_pick_drives: Vec<(char, Option<String>)>,

    lock_check_rx: Option<LockCheckJob>,

    layered_probe: Option<LayeredProbe>,

    find_open: bool,
    find_focus_requested: bool,
    finds: std::collections::BTreeMap<usize, FindState>,

    main_index: Option<Arc<NameIndex>>,
    main_index_builder: Option<IndexBuilder>,
    main_tree_version: u64,

    about: Option<AboutState>,
    about_textures: AboutTextures,
}

pub struct AboutTextures {
    pub logo: egui::TextureHandle,
    pub wechat: egui::TextureHandle,
    pub alipay: egui::TextureHandle,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SponsorTab {
    WeChat,
    Alipay,
}

struct AboutState {
    tab: SponsorTab,
    countdown: Option<f32>,
    logo_row_w: f32,
    tabs_row_w: f32,
    btns_row_w: f32,
}

impl AboutState {
    fn first_launch() -> Self {
        Self {
            tab: SponsorTab::WeChat,
            countdown: Some(ABOUT_COUNTDOWN_SECS),
            logo_row_w: 0.0,
            tabs_row_w: 0.0,
            btns_row_w: 0.0,
        }
    }
    fn from_menu() -> Self {
        Self {
            tab: SponsorTab::WeChat,
            countdown: None,
            logo_row_w: 0.0,
            tabs_row_w: 0.0,
            btns_row_w: 0.0,
        }
    }
}

const ABOUT_COUNTDOWN_SECS: f32 = 6.0;

enum PendingSymlinkKind {
    Single { source: DeleteSource, abs_path: NodePath, name: String, full_path: String, is_folder: bool },
    Group { tab_idx: usize, abs_path: NodePath, name: String, member_paths: Vec<String> },
}

#[derive(Default)]
struct FindState {
    query: String,
    matches: Vec<NodePath>,
    match_entries: Vec<u32>,
    match_index: Option<Arc<NameIndex>>,
    cursor: usize,
    computed_for: Option<(String, usize)>,
    query_changed_at: Option<std::time::Instant>,
}

impl FindState {
    fn match_count(&self) -> usize {
        if self.match_entries.is_empty() { self.matches.len() } else { self.match_entries.len() }
    }
    fn path_of(&self, cursor: usize) -> Option<NodePath> {
        if let Some(idx) = &self.match_index {
            self.match_entries.get(cursor).map(|&e| idx.abs_path_of(e))
        } else {
            self.matches.get(cursor).cloned()
        }
    }
}

impl DiskForgeApp {
    pub fn new(textures: AboutTextures) -> Self {
        let drives = disk_info::list_fixed_drives_with_labels();
        let about = if crate::about::is_sponsor_suppressed() {
            None
        } else {
            Some(AboutState::first_launch())
        };
        Self {
            partitions: Vec::new(),
            partition_infos: Vec::new(),
            partition_categories: Vec::new(),
            partition_root_paths: Vec::new(),
            selected: None,
            tabs: vec![Tab::Main],
            active_tab: 0,
            scanning: false,
            scanned_count: 0,
            scan_error: None,
            scan_rx: None,
            scan_queue: VecDeque::new(),
            current_scan_path: None,
            rescan_target: None,
            picker: Some(startup::PickerState::new(drives)),
            show_all_details: true,
            list_state: tree_list::ListState::default(),
            pending_delete: None,
            duplicate_rx: Vec::new(),
            delete_rx: None,
            lock_check_result: None,
            symlink_rx: None,
            export_rx: None,
            status_message: None,
            pending_symlink_pick: None,
            symlink_pick_drives: Vec::new(),
            lock_check_rx: None,
            layered_probe: None,
            find_open: false,
            find_focus_requested: false,
            finds: std::collections::BTreeMap::new(),
            main_index: None,
            main_index_builder: None,
            main_tree_version: 0,
            about,
            about_textures: textures,
        }
    }
}

impl eframe::App for DiskForgeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().set_visuals(egui::Visuals::dark());
        self.poll_scan();
        self.poll_duplicate_scan();
        self.poll_delete();
        self.poll_symlink();
        self.poll_export();
        self.poll_lock_check();
        self.poll_layered_probe();
        self.ensure_main_index();
        self.ensure_active_view_index();
        self.poll_snapshot_tabs();

        self.show_main_screen(ui);
        if self.picker.is_some() {
            self.show_picker_modal(ui.ctx());
        }
        if self.pending_delete.is_some() {
            self.show_delete_confirm_modal(ui.ctx());
        }
        if self.pending_symlink_pick.is_some() {
            self.show_symlink_target_picker_modal(ui.ctx());
        }
        if self.lock_check_result.is_some() {
            self.show_lock_check_modal(ui.ctx());
        }
        if self.about.is_some() {
            self.show_about_modal(ui.ctx());
        }

        let snapshot_preparing = self.tabs.iter().any(|t| {
            matches!(
                t,
                Tab::CopyList { loading: Some(_), .. } | Tab::SearchList { index: None, .. }
            )
        });
        if self.scanning || !self.duplicate_rx.is_empty() || self.delete_rx.is_some() || self.symlink_rx.is_some() || self.export_rx.is_some() || snapshot_preparing {
            ui.ctx().request_repaint();
        }
    }
}

impl DiskForgeApp {
    fn show_main_screen(&mut self, ui: &mut egui::Ui) {
        let action = topbar::show(ui, TopbarState {
            scanning: self.scanning,
            scanned_count: self.scanned_count,
            scan_error: self.scan_error.as_deref(),
            has_result: !self.partitions.is_empty(),
            show_all_details: self.show_all_details,
            #[cfg(windows)]
            is_admin: crate::mft_scan::is_elevated(),
        });

        let focused_idx = self.selected.as_ref().and_then(|p| p.first().copied())
            .or(if self.partitions.is_empty() { None } else { Some(0) });

        match action {
            TopbarAction::AddScan => {
                let drives = disk_info::list_fixed_drives_with_labels();
                self.picker = Some(startup::PickerState::new(drives));
            }
            TopbarAction::ExportCsv => self.export_csv(),
            TopbarAction::ToggleShowAll => self.show_all_details = !self.show_all_details,
            TopbarAction::ShowExtensionBreakdown => self.open_extension_tab(None),
            TopbarAction::ShowDuplicateFinder => self.open_duplicate_tab(None),
            TopbarAction::OpenFind => {
                self.find_open = true;
                self.find_focus_requested = true;
            }
            TopbarAction::OpenSearchTab => self.open_search_tab(),
            TopbarAction::OpenCopyTab => self.open_copy_tab(),
            TopbarAction::OpenAbout => {
                self.about = Some(AboutState::from_menu());
            }
            #[cfg(windows)]
            TopbarAction::RestartAsAdmin => self.restart_as_admin(),
            TopbarAction::None => {}
        }

        if ui.ctx().input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::F)) && !self.partitions.is_empty() {
            self.find_open = true;
            self.find_focus_requested = true;
        }

        self.show_branding_bar(ui);
        self.show_tab_bar(ui);

        let background_enabled = self.picker.is_none();
        let focused_node = focused_idx.and_then(|i| self.partitions.get(i));
        let focused_info = focused_idx.and_then(|i| self.partition_infos.get(i)).and_then(|o| o.as_ref());
        let focused_categories = focused_idx.and_then(|i| self.partition_categories.get(i)).map(|v| v.as_slice());

        let mut sidebar_action = sidebar::SidebarAction::None;
        egui::Panel::left("sidebar").exact_size(220.0)
            .frame(egui::Frame::default().fill(Color32::from_rgb(0x2A, 0x2A, 0x2E)).inner_margin(egui::Margin::symmetric(12, 4)))
            .show(ui, |ui| {
                ui.add_enabled_ui(background_enabled, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        sidebar_action = sidebar::show(ui, focused_node, focused_info, focused_categories);
                    });
                });
            });
        match sidebar_action {
            sidebar::SidebarAction::OpenExtensions => { if let Some(pi) = focused_idx { self.open_extension_tab(Some(pi)); } }
            sidebar::SidebarAction::OpenDuplicates => { if let Some(pi) = focused_idx { self.open_duplicate_tab(Some(pi)); } }
            sidebar::SidebarAction::None => {}
        }

        let tab_idx = self.active_tab.min(self.tabs.len().saturating_sub(1));
        let tree_action = egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(Color32::from_rgb(0x24, 0x24, 0x28)).inner_margin(egui::Margin::same(4)))
            .show(ui, |ui| {
                ui.add_enabled_ui(background_enabled, |ui| {
                    match self.tabs.get_mut(tab_idx) {
                        Some(Tab::Main) | None => {
                            tree_list::show(
                                ui,
                                tree_list::ListSource::Tree {
                                    partitions: &self.partitions,
                                    partition_infos: &self.partition_infos,
                                    root_paths: &self.partition_root_paths,
                                },
                                &self.selected,
                                self.show_all_details,
                                &mut self.list_state,
                            )
                        }
                        Some(Tab::Duplicates { loading: Some((phase, done, total)), title, .. }) => {
                            show_duplicate_loading(ui, title, *phase, *done, *total);
                            TreeAction::None
                        }
                        Some(Tab::Extensions { root, selected, view, .. }) => {
                            crate::ui::compact_tree::show(ui, root, selected, view, false)
                        }
                        Some(Tab::Duplicates { root, selected, view, .. }) => {
                            crate::ui::compact_tree::show(ui, root, selected, view, true)
                        }
                        Some(Tab::SearchList { index, selected, list_state, .. }) => {
                            match index {
                                Some(idx) => tree_list::show(
                                    ui,
                                    tree_list::ListSource::Indexed { index: idx },
                                    selected,
                                    true,
                                    list_state,
                                ),
                                None => {
                                    show_snapshot_preparing(ui, "正在准备搜索索引…");
                                    TreeAction::None
                                }
                            }
                        }
                        Some(Tab::CopyList { tree, partition_infos, root_paths, selected, list_state, loading, .. }) => {
                            match loading {
                                Some(_) => {
                                    show_snapshot_preparing(ui, "正在准备列表快照…");
                                    TreeAction::None
                                }
                                None => tree_list::show(
                                    ui,
                                    tree_list::ListSource::Tree {
                                        partitions: tree,
                                        partition_infos,
                                        root_paths,
                                    },
                                    selected,
                                    true,
                                    list_state,
                                ),
                            }
                        }
                    }
                }).inner
            })
            .inner;
        self.apply_tree_action(tree_action);
        self.show_find_window(ui.ctx());
    }

    fn show_branding_bar(&self, ui: &mut egui::Ui) {
        egui::Panel::bottom("branding_bar")
            .exact_size(22.0)
            .frame(egui::Frame::default().fill(Color32::from_rgb(0x2E, 0x2E, 0x32)).inner_margin(egui::Margin::symmetric(10, 3)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let brand = format!("⛁ {} {}", crate::about::APP_NAME, crate::about::APP_VERSION);
                    ui.label(RichText::new(brand).size(11.0).color(Color32::from_rgb(0x6F, 0xA8, 0xFF)))
                        .on_hover_text(format!("{}\n作者：{}　邮箱：{}\n免费软件（MIT 许可证）——详见\"关于\"菜单", crate::about::COPYRIGHT_LINE, crate::about::APP_AUTHOR, crate::about::APP_EMAIL));
                    ui.label(RichText::new(format!("· 由 {} 开发 · 免费软件 (MIT)", crate::about::APP_AUTHOR)).size(10.0).color(Color32::from_rgb(0x80, 0x80, 0x80)));

                    if let Some(status) = &self.status_message {
                        let elapsed = status.set_at.elapsed();
                        if elapsed < STATUS_MSG_TTL {
                            ui.ctx().request_repaint_after(STATUS_MSG_TTL - elapsed);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let color = if status.is_error {
                                    crate::theme::STATUS_ERROR_RED
                                } else {
                                    crate::theme::STATUS_OK_GREEN
                                };
                                let font = egui::FontId::proportional(11.0);
                                let max_w = (ui.available_width() - 8.0).max(40.0);
                                let shown = crate::format::truncate_text(ui.ctx(), &status.text, font, max_w);
                                ui.label(RichText::new(shown).size(11.0).color(color)).on_hover_text(status.text.as_str());
                            });
                        }
                    }
                });
            });
    }

    fn show_find_window(&mut self, ctx: &egui::Context) {
        if !self.find_open {
            return;
        }
        let mut window_open = true;
        let mut go_next = false;
        let mut go_prev = false;
        let modal_open = self.picker.is_some()
            || self.pending_delete.is_some()
            || self.pending_symlink_pick.is_some()
            || self.lock_check_result.is_some()
            || self.layered_probe.is_some()
            || self.about.is_some();
        let esc_pressed = !modal_open && ctx.input(|i| i.key_pressed(egui::Key::Escape));
        egui::Window::new("🔎 查找")
            .id(egui::Id::new("find_window"))
            .collapsible(false)
            .resizable(false)
            .title_bar(true)
            .open(&mut window_open)
            .default_pos(egui::pos2(ctx.content_rect().right() - 320.0, 44.0))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 4.0);
                ui.horizontal(|ui| {
                    ui.label("🔍");
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.find_state().query)
                            .hint_text("名称、或 *.pid 这类通配符…")
                            .desired_width(170.0),
                    );
                    if resp.changed() {
                        self.find_state().query_changed_at = Some(std::time::Instant::now());
                    }
                    if self.find_focus_requested {
                        resp.request_focus();
                        self.find_focus_requested = false;
                    }
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        if ui.input(|i| i.modifiers.shift) { go_prev = true; } else { go_next = true; }
                    }
                    ui.add_space(2.0);
                    if ui.small_button("◀").on_hover_text("上一个 (Shift+Enter)").clicked() { go_prev = true; }
                    if ui.small_button("▶").on_hover_text("下一个 (Enter)").clicked() { go_next = true; }
                });
                let (query_empty, cursor, total) = {
                    let f = self.find_state();
                    (f.query.trim().is_empty(), f.cursor, f.match_count())
                };
                let label = if query_empty {
                    "输入内容开始查找".to_string()
                } else if total == 0 {
                    "没有匹配项".to_string()
                } else {
                    format!("第 {} / {} 项", cursor + 1, total)
                };
                ui.label(RichText::new(label).size(11.0).color(Color32::from_rgb(0xA0, 0xA0, 0xA0)));
            });
        if !window_open || esc_pressed {
            self.find_open = false;
            return;
        }

        const FIND_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(180);
        let changed_at = self.find_state().query_changed_at;
        let debounce_ready = match changed_at {
            Some(t) if t.elapsed() >= FIND_DEBOUNCE => {
                self.find_state().query_changed_at = None;
                true
            }
            Some(t) => {
                ctx.request_repaint_after(FIND_DEBOUNCE - t.elapsed());
                false
            }
            None => true,
        };
        let query = self.find_state().query.trim().to_string();
        let key = (query.to_lowercase(), self.active_tab);
        let already_computed = self.find_state().computed_for.as_ref() == Some(&key);
        if debounce_ready && !already_computed {
            let found = if query.is_empty() {
                FindMatches::Paths(Vec::new())
            } else {
                self.collect_find_matches(&query)
            };
            {
                let find = self.find_state();
                match found {
                    FindMatches::Entries { entries, index } => {
                        find.matches = Vec::new();
                        find.match_entries = entries;
                        find.match_index = Some(index);
                    }
                    FindMatches::Paths(paths) => {
                        find.matches = paths;
                        find.match_entries = Vec::new();
                        find.match_index = None;
                    }
                }
                find.computed_for = Some(key);
                find.cursor = 0;
            }
            let first = {
                let find = self.find_state();
                if find.match_count() > 0 {
                    find.path_of(0).map(|p| (p, find.match_entries.first().copied()))
                } else {
                    None
                }
            };
            if let Some((p, e)) = first {
                self.reveal_find_target(p, e);
            }
        }

        let total = self.find_state().match_count();
        if go_next && total > 0 {
            let jump = {
                let find = self.find_state();
                find.cursor = (find.cursor + 1) % total;
                find.path_of(find.cursor).map(|p| (p, find.match_entries.get(find.cursor).copied()))
            };
            if let Some((p, e)) = jump {
                self.reveal_find_target(p, e);
            }
        }
        if go_prev && total > 0 {
            let jump = {
                let find = self.find_state();
                find.cursor = (find.cursor + total - 1) % total;
                find.path_of(find.cursor).map(|p| (p, find.match_entries.get(find.cursor).copied()))
            };
            if let Some((p, e)) = jump {
                self.reveal_find_target(p, e);
            }
        }
    }

    fn find_state(&mut self) -> &mut FindState {
        self.finds.entry(self.active_tab).or_default()
    }

    fn collect_find_matches(&self, query: &str) -> FindMatches {
        let matcher = crate::ui::Matcher::build_auto(query);
        match self.tabs.get(self.active_tab) {
            Some(Tab::SearchList { index: Some(idx), .. }) | Some(Tab::CopyList { index: Some(idx), .. }) => {
                collect_find_matches_index(idx, &matcher)
            }
            Some(Tab::SearchList { .. }) | Some(Tab::CopyList { .. }) => {
                FindMatches::Paths(Vec::new())
            }
            Some(Tab::Extensions { root, view, .. }) | Some(Tab::Duplicates { root, view, .. }) => {
                if let Some(idx) = &view.index {
                    collect_find_matches_index(idx, &matcher)
                } else {
                    FindMatches::Paths(collect_find_matches_view(root, &matcher))
                }
            }
            _ => {
                if let Some(idx) = self.main_index.as_ref().filter(|i| i.struct_version == self.main_tree_version) {
                    collect_find_matches_index(idx, &matcher)
                } else {
                    FindMatches::Paths(collect_find_matches_main_shaped(&self.partitions, &matcher))
                }
            }
        }
    }

    fn reveal_find_target(&mut self, path: NodePath, index_entry: Option<u32>) {
        match self.tabs.get_mut(self.active_tab) {
            Some(Tab::CopyList { tree, selected, list_state, .. }) => {
                reveal_in_main_shaped_tree(tree, list_state, selected, path);
            }
            Some(Tab::SearchList { selected, list_state, .. }) => {
                list_state.pending_scroll = Some(path.clone());
                list_state.pending_scroll_entry = index_entry;
                *selected = Some(path);
            }
            Some(Tab::Extensions { root, selected, view, .. }) | Some(Tab::Duplicates { root, selected, view, .. }) => {
                reveal_in_view_tree(root, view, selected, path);
            }
            _ => reveal_in_main_shaped_tree(&mut self.partitions, &mut self.list_state, &mut self.selected, path),
        }
    }

    fn show_tab_bar(&mut self, ui: &mut egui::Ui) {
        if self.tabs.len() <= 1 { return; }
        let mut select_idx: Option<usize> = None;
        let mut close_idx: Option<usize> = None;
        egui::Panel::top("tab_bar").exact_size(30.0)
            .frame(egui::Frame::default().fill(Color32::from_rgb(0x26, 0x26, 0x2A)).inner_margin(egui::Margin::symmetric(6, 3)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (i, tab) in self.tabs.iter().enumerate() {
                        let title = match tab {
                            Tab::Main => "📋 主列表".to_string(),
                            Tab::Extensions { title, .. } => format!("🗐 {title}"),
                            Tab::Duplicates { title, .. } => format!("🔍 {title}"),
                            Tab::SearchList { title, .. } => format!("🔎 {title}"),
                            Tab::CopyList { title, .. } => format!("📑 {title}"),
                        };
                        if ui.selectable_label(i == self.active_tab, title).clicked() {
                            select_idx = Some(i);
                        }
                        if !matches!(tab, Tab::Main) && ui.small_button("×").clicked() {
                            close_idx = Some(i);
                        }
                        ui.add_space(4.0);
                    }
                });
            });
        if let Some(i) = select_idx { self.active_tab = i; }
        if let Some(i) = close_idx {
            self.tabs.remove(i);
            self.finds.remove(&i);
            let shifted: Vec<(usize, FindState)> = self.finds.split_off(&(i + 1)).into_iter().collect();
            for (k, v) in shifted {
                self.finds.insert(k - 1, v);
            }
            if self.active_tab >= self.tabs.len() {
                self.active_tab = self.tabs.len().saturating_sub(1);
            } else if self.active_tab > i {
                self.active_tab -= 1;
            }
        }
    }

    fn open_extension_tab(&mut self, scope: Option<usize>) {
        if let Some(idx) = self.tabs.iter().position(|t| matches!(t, Tab::Extensions { partition_idx, .. } if *partition_idx == scope)) {
            self.active_tab = idx;
            return;
        }
        let root = match scope {
            Some(pi) => {
                let Some(node) = self.partitions.get(pi) else { return };
                let root_path = self.partition_root_paths.get(pi).cloned().unwrap_or_default();
                categorize::build_extension_tree(node, &root_path)
            }
            None => {
                if self.partitions.is_empty() { return; }
                let pairs: Vec<(&Node, &str)> = self.partitions.iter()
                    .zip(self.partition_root_paths.iter())
                    .map(|(n, p)| (n, p.as_str()))
                    .collect();
                categorize::build_extension_tree_multi(&pairs)
            }
        };
        let title = match scope {
            Some(pi) => self.partitions.get(pi).map(|n| n.name.clone()).unwrap_or_default(),
            None => format!("全部 {} 个分区", self.partitions.len()),
        };
        crate::applog::log(&format!("[app] 打开扩展名分类标签页: {title}（{} 种扩展名）", root.children.len()));
        self.tabs.push(Tab::Extensions { partition_idx: scope, title, root, selected: None, view: crate::ui::compact_tree::ViewState::default() });
        self.active_tab = self.tabs.len() - 1;
    }

    fn open_duplicate_tab(&mut self, scope: Option<usize>) {
        if let Some(idx) = self.tabs.iter().position(|t| matches!(t, Tab::Duplicates { partition_idx, .. } if *partition_idx == scope)) {
            self.active_tab = idx;
            return;
        }
        let title = match scope {
            Some(pi) => {
                let Some(node) = self.partitions.get(pi) else { return };
                node.name.clone()
            }
            None => {
                if self.partitions.is_empty() { return; }
                format!("全部 {} 个分区", self.partitions.len())
            }
        };
        crate::applog::log(&format!("[app] 开始比对重复文件: {title}"));

        let (tx, rx) = mpsc::channel();
        match scope {
            Some(pi) => {
                let Some(node) = self.partitions.get(pi) else { return };
                let root_path = self.partition_root_paths.get(pi).cloned().unwrap_or_default();
                categorize::spawn_duplicate_scan(node, &root_path, tx);
            }
            None => {
                let pairs: Vec<(&Node, &str)> = self.partitions.iter()
                    .zip(self.partition_root_paths.iter())
                    .map(|(n, p)| (n, p.as_str()))
                    .collect();
                categorize::spawn_duplicate_scan_multi(&pairs, tx);
            }
        }
        self.duplicate_rx.push((scope, rx));

        self.tabs.push(Tab::Duplicates {
            partition_idx: scope, title, root: Node::new_folder("", Color32::WHITE, Vec::new()),
            selected: None, view: crate::ui::compact_tree::ViewState::default(), loading: Some((crate::dedup::HashPhase::Prefilter, 0, 0)),
        });
        self.active_tab = self.tabs.len() - 1;
    }

    fn open_search_tab(&mut self) {
        if self.partitions.is_empty() {
            return;
        }
        let seq = self.tabs.iter().filter(|t| matches!(t, Tab::SearchList { .. })).count() + 1;
        let title = format!("搜索 {seq}");
        crate::applog::log(&format!(
            "[app] 打开搜索标签页: {title}（索引快照{}）",
            if self.main_index.is_some() { "已就绪，O(1) 打开" } else { "构建中，就绪后自动展示" },
        ));
        self.tabs.push(Tab::SearchList {
            title,
            index: self.main_index.clone(),
            selected: None,
            list_state: tree_list::ListState::default(),
        });
        self.active_tab = self.tabs.len() - 1;
    }

    fn open_copy_tab(&mut self) {
        if self.partitions.is_empty() {
            return;
        }
        let seq = self.tabs.iter().filter(|t| matches!(t, Tab::CopyList { .. })).count() + 1;
        let title = format!("列表副本 {seq}");
        let partition_infos = self.partition_infos.clone();
        let root_paths = self.partition_root_paths.clone();
        let index = self.main_index.clone();
        let loading = match &index {
            Some(idx) => {
                let (tx, rx) = mpsc::channel();
                let idx_for_thread = Arc::clone(idx);
                std::thread::spawn(move || {
                    let _ = tx.send(idx_for_thread.rebuild_tree());
                });
                Some(CopyLoading::Building(rx))
            }
            None => Some(CopyLoading::WaitIndex),
        };
        crate::applog::log(&format!(
            "[app] 复制列表到新标签页: {title}（{} 个分区，后台重建树中）", self.partitions.len(),
        ));
        self.tabs.push(Tab::CopyList {
            title,
            index,
            tree: Vec::new(),
            partition_infos,
            root_paths,
            selected: None,
            list_state: tree_list::ListState::default(),
            loading,
        });
        self.active_tab = self.tabs.len() - 1;
    }

    fn ensure_main_index(&mut self) {
        if self.partitions.is_empty() {
            self.main_index = None;
            self.main_index_builder = None;
            return;
        }
        if self.main_index.as_ref().map(|i| i.struct_version) == Some(self.main_tree_version) {
            self.main_index_builder = None;
            return;
        }
        let version_ok = self.main_index_builder.as_ref().map(|b| b.struct_version) == Some(self.main_tree_version);
        if !version_ok {
            self.main_index_builder = Some(IndexBuilder::new_multi(
                &self.partitions,
                &self.partition_root_paths,
                self.main_tree_version,
                true,
            ));
        }
        match self.main_index_builder.as_mut().unwrap().step(INDEX_STEP_BUDGET_MAIN) {
            BuildStep::Done(idx) => {
                crate::applog::log(&format!(
                    "[app] 名字索引构建完成: {} 条目（版本 {}）",
                    idx.len(), idx.struct_version,
                ));
                self.main_index = Some(Arc::new(*idx));
                self.main_index_builder = None;
            }
            BuildStep::Continue => {}
        }
    }

    fn ensure_active_view_index(&mut self) {
        let tab_idx = self.active_tab.min(self.tabs.len().saturating_sub(1));
        let Some(tab) = self.tabs.get_mut(tab_idx) else { return };
        let (root_ref, view) = match tab {
            Tab::Extensions { root, view, .. } | Tab::Duplicates { root, view, .. } => (&*root, view),
            _ => return,
        };
        if view.index.as_ref().map(|i| i.struct_version) == Some(view.struct_version) {
            view.index_builder = None;
            return;
        }
        let version_ok = view.index_builder.as_ref().map(|b| b.struct_version) == Some(view.struct_version);
        if !version_ok {
            view.index_builder = Some(IndexBuilder::new_single_root(root_ref, view.struct_version));
        }
        match view.index_builder.as_mut().unwrap().step(INDEX_STEP_BUDGET_VIEW) {
            BuildStep::Done(idx) => {
                view.index = Some(Arc::new(*idx));
                view.index_builder = None;
            }
            BuildStep::Continue => {}
        }
    }

    fn poll_snapshot_tabs(&mut self) {
        let main_ready = self.main_index.clone();
        for tab_i in 0..self.tabs.len() {
            let wait_index = matches!(
                self.tabs.get(tab_i),
                Some(Tab::SearchList { index: None, .. })
                    | Some(Tab::CopyList { index: None, loading: Some(CopyLoading::WaitIndex), .. })
            );
            if wait_index
                && let Some(idx) = &main_ready {
                    match self.tabs.get_mut(tab_i) {
                        Some(Tab::SearchList { index, .. }) => {
                            *index = Some(Arc::clone(idx));
                        }
                        Some(Tab::CopyList { index, loading, .. }) => {
                            *index = Some(Arc::clone(idx));
                            let (tx, rx) = mpsc::channel();
                            let idx_for_thread = Arc::clone(idx);
                            std::thread::spawn(move || {
                                let _ = tx.send(idx_for_thread.rebuild_tree());
                            });
                            *loading = Some(CopyLoading::Building(rx));
                        }
                        _ => {}
                    }
                }
            let rebuilt = match self.tabs.get(tab_i) {
                Some(Tab::CopyList { loading: Some(CopyLoading::Building(rx)), .. }) => match rx.try_recv() {
                    Ok(tree) => Some(tree),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => Some(Vec::new()),
                },
                _ => None,
            };
            if let Some(tree) = rebuilt
                && let Some(Tab::CopyList { tree: dst, loading, .. }) = self.tabs.get_mut(tab_i) {
                    *dst = tree;
                    *loading = None;
                    crate::applog::log("[app] 列表副本树重建完成");
                }
        }
    }

    fn poll_duplicate_scan(&mut self) {
        if self.duplicate_rx.is_empty() {
            return;
        }
        let mut done_pi: Vec<Option<usize>> = Vec::new();
        for (pi, rx) in &self.duplicate_rx {
            let pi = *pi;
            loop {
                match rx.try_recv() {
                    Ok(msg) => {
                        let tab = self.tabs.iter_mut().find(|t| matches!(t, Tab::Duplicates { partition_idx, .. } if *partition_idx == pi));
                        if let Some(Tab::Duplicates { root, loading, view, .. }) = tab {
                            match msg {
                                categorize::DuplicateMessage::Progress { phase, done, total } => *loading = Some((phase, done, total)),
                                categorize::DuplicateMessage::Done(tree) => {
                                    *root = *tree;
                                    *loading = None;
                                    view.expand_version += 1;
                                    view.struct_version += 1;
                                }
                                categorize::DuplicateMessage::Failed(reason) => {
                                    *loading = None;
                                    crate::applog::log(&format!("[app] 重复文件比对失败 (pi={pi:?}): {reason}"));
                                    self.status_message = Some(StatusMsg::error(reason));
                                }
                            }
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if let Some(Tab::Duplicates { loading, .. }) = self.tabs.iter_mut().find(|t| matches!(t, Tab::Duplicates { partition_idx, .. } if *partition_idx == pi))
                            && loading.is_some() {
                                *loading = None;
                                crate::applog::log("[app] 重复文件比对线程异常退出（通道断开），已恢复标签页");
                                self.status_message = Some(StatusMsg::error("重复文件比对内部错误，已中止（详情见日志）".to_string()));
                            }
                        done_pi.push(pi);
                        break;
                    }
                }
            }
        }
        if !done_pi.is_empty() {
            self.duplicate_rx.retain(|(pi, _)| !done_pi.contains(pi));
        }
    }

    fn show_picker_modal(&mut self, ctx: &egui::Context) {
        let show_cancel = !self.partitions.is_empty() || self.scanning || !self.scan_queue.is_empty();
        let Some(picker) = &mut self.picker else { return };
        let mut picker_action = startup::PickerAction::None;
        egui::Window::new("选择要扫描的分区/目录")
            .id(egui::Id::new("scan_picker_modal"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                picker_action = startup::show(ui, picker, show_cancel);
            });
        match picker_action {
            startup::PickerAction::Confirm => {
                let paths = build_scan_paths(picker);
                self.picker = None;
                self.start_scan_batch(paths);
            }
            startup::PickerAction::Cancel => { self.picker = None; }
            startup::PickerAction::None => {}
        }
    }

    fn start_scan_batch(&mut self, paths: Vec<PathBuf>) {
        self.scan_queue.extend(paths);
        if !self.scanning {
            self.dequeue_next_scan();
        }
    }

    fn dequeue_next_scan(&mut self) {
        if let Some(path) = self.scan_queue.pop_front() {
            crate::applog::log(&format!("[app] 开始扫描: {}", path.display()));
            let (tx, rx) = mpsc::channel();
            scan::spawn_scan(path.clone(), tx);
            self.current_scan_path = Some(path);
            self.scan_rx = Some(rx);
            self.scanning = true;
            self.scanned_count = 0;
            self.scan_error = None;
        }
    }

    fn start_rescan(&mut self, pi: usize) {
        if self.scanning {
            self.status_message = Some(StatusMsg::error("已经有扫描在进行中，请等它完成后再重新扫描。".to_string()));
            return;
        }
        let Some(root_path) = self.partition_root_paths.get(pi).cloned() else { return };
        if root_path.is_empty() {
            return;
        }
        crate::applog::log(&format!("[app] 重新扫描: {root_path}"));
        let path = PathBuf::from(&root_path);
        let (tx, rx) = mpsc::channel();
        scan::spawn_scan(path.clone(), tx);
        self.current_scan_path = Some(path);
        self.scan_rx = Some(rx);
        self.scanning = true;
        self.scanned_count = 0;
        self.scan_error = None;
        self.rescan_target = Some(pi);
    }

    fn remove_partition(&mut self, pi: usize) {
        if pi >= self.partitions.len() {
            return;
        }
        let name = self.partitions[pi].name.clone();
        self.partitions.remove(pi);
        self.partition_infos.remove(pi);
        self.partition_categories.remove(pi);
        self.partition_root_paths.remove(pi);
        self.selected = None;
        self.list_state.expand_version += 1;
        self.main_tree_version += 1;
        crate::applog::log(&format!("[app] 已从列表移除: {name}"));
        self.status_message = Some(StatusMsg::info(format!("已从列表移除: {name}（不会删除磁盘上的文件，重新扫描可以再加回来）")));
    }

    fn poll_scan(&mut self) {
        let Some(rx) = &self.scan_rx else { return };
        let mut finished = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                ScanMessage::Progress(n) => self.scanned_count = n,
                ScanMessage::Done(node, info) => {
                    let node = *node;
                    let path = self.current_scan_path.take()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let categories = categorize::compute_categories(&node);
                    log_scan_summary(&node, info.as_ref(), &categories);
                    if let Some(pi) = self.rescan_target.take().filter(|&pi| pi < self.partitions.len()) {
                        self.partitions[pi] = node;
                        self.partition_infos[pi] = info;
                        self.partition_categories[pi] = categories;
                        crate::applog::log(&format!("[app] 重新扫描完成: {path}"));
                        self.status_message = Some(StatusMsg::info(format!("重新扫描完成: {path}")));
                    } else {
                        self.partitions.push(node);
                        self.partition_infos.push(info);
                        self.partition_categories.push(categories);
                        self.partition_root_paths.push(path);
                    }
                    self.selected = None;
                    self.list_state.expand_version += 1;
                    self.main_tree_version += 1;
                    finished = true;
                }
                ScanMessage::Error(e) => {
                    crate::applog::log(&format!("[app] 扫描失败: {e}"));
                    self.scan_error = Some(e);
                    self.current_scan_path = None;
                    self.rescan_target = None;
                    finished = true;
                }
            }
        }
        if finished {
            self.scan_rx = None;
            self.scanning = false;
            self.dequeue_next_scan();
        }
    }

    fn apply_tree_action(&mut self, action: TreeAction) {
        let tab_idx = self.active_tab.min(self.tabs.len().saturating_sub(1));
        let in_tab_tree = !matches!(self.tabs.get(tab_idx), Some(Tab::Main) | None);
        match action {
            TreeAction::None => {}
            TreeAction::Select(p) | TreeAction::EnterNode(p) => {
                if in_tab_tree {
                    if let Some(tab) = self.tabs.get_mut(tab_idx) {
                        let selected = match tab {
                            Tab::Extensions { selected, .. } | Tab::Duplicates { selected, .. } => selected,
                            Tab::SearchList { selected, .. } | Tab::CopyList { selected, .. } => selected,
                            Tab::Main => return,
                        };
                        *selected = Some(p);
                    }
                } else {
                    self.selected = Some(p);
                }
            }
            TreeAction::ToggleExpand(p) => {
                if in_tab_tree {
                    if let Some(tab) = self.tabs.get_mut(tab_idx) {
                        match tab {
                            Tab::Extensions { root, selected, view, .. } | Tab::Duplicates { root, selected, view, .. } => {
                                root.toggle_expand(&p[1..]);
                                *selected = Some(p);
                                view.expand_version += 1;
                            }
                            Tab::CopyList { tree, selected, list_state, .. } => {
                                if let Some(&pi) = p.first()
                                    && let Some(part) = tree.get_mut(pi) {
                                        part.toggle_expand(&p[1..]);
                                    }
                                list_state.expand_version += 1;
                                *selected = Some(p);
                            }
                            Tab::SearchList { .. } => (),
                            Tab::Main => (),
                        }
                    }
                } else {
                    if let Some(&pi) = p.first()
                        && let Some(part) = self.partitions.get_mut(pi) {
                            part.toggle_expand(&p[1..]);
                        }
                    self.list_state.expand_version += 1;
                    self.selected = Some(p);
                }
            }
            TreeAction::RequestDelete { abs_path, name, full_path, is_folder, index_entry } => {
                let source = if in_tab_tree { DeleteSource::Tab(tab_idx) } else { DeleteSource::Main };
                self.pending_delete = Some(PendingDelete { source, abs_path, name, full_path, is_folder, index_entry });
            }
            TreeAction::RequestCheckLock { abs_path, name, full_path, is_folder } => {
                let request = LockCheckRequest { tab_idx, is_view_tab: in_tab_tree, abs_path, name, full_path, is_folder, is_group: false };
                self.run_lock_check(request);
            }
            TreeAction::RequestCheckLockGroup { abs_path, name } => {
                let request = LockCheckRequest { tab_idx, is_view_tab: true, abs_path, name, full_path: String::new(), is_folder: false, is_group: true };
                self.run_lock_check(request);
            }
            TreeAction::RequestCreateSymlink { abs_path, name, full_path, is_folder } => {
                let source = if in_tab_tree { DeleteSource::Tab(tab_idx) } else { DeleteSource::Main };
                self.start_symlink_single(source, abs_path, name, full_path, is_folder);
            }
            TreeAction::RequestCreateSymlinkGroup { abs_path, name } => {
                self.start_symlink_group(tab_idx, abs_path, name);
            }
            TreeAction::RequestRescan(pi) => {
                if in_tab_tree {
                    self.status_message = Some(StatusMsg::error("这是数据快照，不支持重新扫描——请到主列表操作对应的分区。".to_string()));
                } else {
                    self.start_rescan(pi);
                }
            }
            TreeAction::RequestRemovePartition(pi) => {
                if in_tab_tree {
                    self.status_message = Some(StatusMsg::error("这是数据快照，不支持从这里移除分区——请到主列表操作。".to_string()));
                } else {
                    self.remove_partition(pi);
                }
            }
            TreeAction::RequestExtensionBreakdown(pi) => {
                if in_tab_tree {
                    self.status_message = Some(StatusMsg::error("这是数据快照，暂不支持在这里做扩展名分类——请到主列表操作对应的分区。".to_string()));
                } else {
                    self.open_extension_tab(Some(pi));
                }
            }
            TreeAction::RequestDuplicateFinder(pi) => {
                if in_tab_tree {
                    self.status_message = Some(StatusMsg::error("这是数据快照，暂不支持在这里查找重复文件——请到主列表操作对应的分区。".to_string()));
                } else {
                    self.open_duplicate_tab(Some(pi));
                }
            }
        }
    }

    fn run_lock_check(&mut self, request: LockCheckRequest) {
        self.run_lock_check_with_feedback(request, None);
    }

    fn run_lock_check_with_feedback(&mut self, request: LockCheckRequest, action_feedback: Option<String>) {
        const LOCK_CHECK_LIMIT: usize = 2000;
        let is_folder = request.is_folder;
        self.layered_probe = None;

        if request.is_group {
            let paths: Vec<String> = self.tabs.get(request.tab_idx)
                .and_then(|t| match t {
                    Tab::Duplicates { root, .. } => root.get_at_path(&request.abs_path[1..]),
                    _ => None,
                })
                .map(|group| group.children.iter().filter_map(|c| c.full_path_override.clone()).collect())
                .unwrap_or_default();
            if paths.is_empty() {
                let name = request.name.clone();
                self.lock_check_result = Some(LockCheckResult {
                    request, name, is_folder: false, checked_count: 0, truncated: false, procs: Vec::new(), action_feedback,
                    error: Some("这个分组现在没有可查的文件（可能已经被处理/删除了）。".to_string()),
                    rename_probe: None, loading: false,
                });
                return;
            }
            let truncated = paths.len() >= LOCK_CHECK_LIMIT;
            let checked_count = paths.len();
            let name = request.name.clone();
            self.lock_check_result = Some(LockCheckResult {
                request: request.clone(), name, is_folder: false, checked_count, truncated,
                procs: Vec::new(), error: None, action_feedback: action_feedback.clone(), rename_probe: None, loading: true,
            });
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let capped: Vec<&str> = paths.iter().take(LOCK_CHECK_LIMIT).map(String::as_str).collect();
                let _ = tx.send(crate::file_ops::find_locking_processes(&capped));
            });
            self.lock_check_rx = Some((LockCheckPending { request, action_feedback, is_folder: false, checked_count, truncated, rename_probe: None }, rx));
            return;
        }

        if is_folder {
            let rename_probe = crate::file_ops::check_folder_occupied_by_rename(&request.full_path);
            let name = request.name.clone();
            self.lock_check_result = Some(LockCheckResult {
                request, name, is_folder: true, checked_count: 0, truncated: false, procs: Vec::new(),
                action_feedback, error: None, rename_probe: Some(rename_probe), loading: false,
            });
            return;
        }

        let paths: Vec<String> = vec![request.full_path.clone()];
        let truncated = paths.len() >= LOCK_CHECK_LIMIT;
        let checked_count = paths.len();
        let name = request.name.clone();
        self.lock_check_result = Some(LockCheckResult {
            request: request.clone(), name, is_folder: false, checked_count, truncated,
            procs: Vec::new(), error: None, action_feedback: action_feedback.clone(),
            rename_probe: None, loading: true,
        });

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
            let _ = tx.send(crate::file_ops::find_locking_processes(&refs));
        });
        self.lock_check_rx = Some((LockCheckPending { request, action_feedback, is_folder: false, checked_count, truncated, rename_probe: None }, rx));
    }

    fn poll_lock_check(&mut self) {
        let Some((_, rx)) = &self.lock_check_rx else { return };
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("检测占用线程异常退出".to_string()),
        };
        let (pending, _) = self.lock_check_rx.take().unwrap();
        let (procs, error) = match result {
            Ok(procs) => (procs, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        let name = pending.request.name.clone();
        self.lock_check_result = Some(LockCheckResult {
            request: pending.request, name, is_folder: pending.is_folder,
            checked_count: pending.checked_count, truncated: pending.truncated,
            procs, error, action_feedback: pending.action_feedback, rename_probe: pending.rename_probe, loading: false,
        });
    }

    fn start_layered_probe(&mut self) {
        let Some(result) = &self.lock_check_result else { return };
        if !result.is_folder {
            return;
        }
        let root_full_path = result.request.full_path.clone();
        let root_name = result.name.clone();
        crate::applog::log(&format!("[app] 开始按层排查占用: {root_full_path}"));
        self.layered_probe = Some(LayeredProbe {
            root_full_path: root_full_path.clone(),
            root_name,
            frontier: vec![root_full_path],
            layer: 0,
            procs: Vec::new(),
            exhausted: false,
            pending: None,
            last_layer_stats: None,
        });
        self.advance_layered_probe();
    }

    fn advance_layered_probe(&mut self) {
        let Some(probe) = &mut self.layered_probe else { return };
        if probe.pending.is_some() {
            return;
        }
        let frontier = std::mem::take(&mut probe.frontier);
        if frontier.is_empty() {
            probe.exhausted = true;
            return;
        }

        let mut next_frontier = Vec::new();
        let mut layer_files = Vec::new();
        let mut subfolders_checked = 0usize;
        let mut subfolders_locked = 0usize;

        for dir in &frontier {
            let entries = match std::fs::read_dir(dir) {
                Ok(e) => e,
                Err(e) => {
                    crate::applog::log(&format!("[app] 具体检测：读取目录失败，跳过: {dir}: {e}"));
                    continue;
                }
            };
            for entry in entries.flatten() {
                let path_str = entry.path().to_string_lossy().to_string();
                match entry.file_type() {
                    Ok(ft) if ft.is_dir() => {
                        subfolders_checked += 1;
                        match crate::file_ops::check_folder_occupied_by_rename(&path_str) {
                            crate::file_ops::FolderOccupancy::Free => {}
                            crate::file_ops::FolderOccupancy::Locked
                            | crate::file_ops::FolderOccupancy::Inconclusive(_) => {
                                subfolders_locked += 1;
                                next_frontier.push(path_str);
                            }
                        }
                    }
                    Ok(ft) if ft.is_file() => layer_files.push(path_str),
                    _ => {}
                }
            }
        }

        let layer_just_finished = probe.layer + 1;
        let files_checked = layer_files.len();
        if layer_files.is_empty() {
            probe.frontier = next_frontier;
            probe.layer = layer_just_finished;
            probe.exhausted = probe.frontier.is_empty();
            probe.last_layer_stats = Some(LayerStats {
                layer: layer_just_finished, files_checked: 0, subfolders_checked, subfolders_locked, new_procs_found: 0,
            });
            return;
        }

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let refs: Vec<&str> = layer_files.iter().map(String::as_str).collect();
            let _ = tx.send(crate::file_ops::find_locking_processes(&refs));
        });
        probe.pending = Some(LayeredProbePending {
            rx, next_frontier, layer_just_finished, subfolders_checked, subfolders_locked, files_checked,
        });
    }

    fn poll_layered_probe(&mut self) {
        let Some(probe) = &mut self.layered_probe else { return };
        let Some(pending) = &probe.pending else { return };
        let result = match pending.rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("检测线程异常退出".to_string()),
        };
        let pending = probe.pending.take().unwrap();
        let mut new_procs_found = 0usize;
        if let Ok(found) = result {
            for p in found {
                if !probe.procs.iter().any(|existing| existing.pid == p.pid) {
                    new_procs_found += 1;
                    probe.procs.push(p);
                }
            }
        }
        probe.frontier = pending.next_frontier;
        probe.layer = pending.layer_just_finished;
        probe.exhausted = probe.frontier.is_empty();
        probe.last_layer_stats = Some(LayerStats {
            layer: pending.layer_just_finished, files_checked: pending.files_checked,
            subfolders_checked: pending.subfolders_checked, subfolders_locked: pending.subfolders_locked,
            new_procs_found,
        });
    }

    fn layered_probe_terminate_all_and_recheck(&mut self) {
        let Some(probe) = &self.layered_probe else { return };
        let mut seen = std::collections::HashSet::new();
        for p in &probe.procs {
            if seen.insert(p.pid) {
                let _ = crate::file_ops::terminate_process(p.pid);
            }
        }
        let Some(probe) = &mut self.layered_probe else { return };
        match crate::file_ops::check_folder_occupied_by_rename(&probe.root_full_path) {
            crate::file_ops::FolderOccupancy::Free => {
                let path = probe.root_full_path.clone();
                probe.frontier.clear();
                probe.exhausted = true;
                probe.procs.clear();
                probe.last_layer_stats = None;
                self.status_message = Some(StatusMsg::info(format!("已解除占用: {path}")));
                self.refresh_lock_check(Some("已解除占用，重命名探测通过。".to_string()));
            }
            _ => {
                self.advance_layered_probe();
            }
        }
    }

    fn lock_check_terminate_process(&mut self, pid: u32) {
        if let Some(probe) = &mut self.layered_probe {
            let feedback = match crate::file_ops::terminate_process(pid) {
                Ok(()) => {
                    probe.procs.retain(|p| p.pid != pid);
                    format!("已结束进程 PID {pid}。")
                }
                Err(e) => format!("结束进程失败：{e}"),
            };
            self.status_message = Some(StatusMsg::info(feedback));
            return;
        }
        let feedback = match crate::file_ops::terminate_process(pid) {
            Ok(()) => format!("已结束进程 PID {pid}。"),
            Err(e) => format!("结束进程失败：{e}"),
        };
        self.refresh_lock_check(Some(feedback));
    }

    fn lock_check_terminate_all(&mut self) {
        let Some(result) = &self.lock_check_result else { return };
        let mut seen = std::collections::HashSet::new();
        let mut ok_count = 0usize;
        let mut failures: Vec<String> = Vec::new();
        for p in &result.procs {
            if !seen.insert(p.pid) {
                continue;
            }
            match crate::file_ops::terminate_process(p.pid) {
                Ok(()) => ok_count += 1,
                Err(e) => failures.push(format!("{}（PID {}）: {e}", p.app_name, p.pid)),
            }
        }
        let feedback = if failures.is_empty() {
            format!("已结束全部 {ok_count} 个占用进程。")
        } else {
            format!("已结束 {ok_count} 个进程，{} 个失败——{}", failures.len(), failures.join("；"))
        };
        self.refresh_lock_check(Some(feedback));
    }

    fn lock_check_stop_service(&mut self, service_name: String) {
        if let Some(probe) = &mut self.layered_probe {
            let feedback = match crate::file_ops::stop_service(&service_name) {
                Ok(()) => {
                    probe.procs.retain(|p| p.service_name.as_deref() != Some(service_name.as_str()));
                    format!("已发送停止请求给服务 {service_name}（服务真正停下来可能要几秒）。")
                }
                Err(e) => format!("停止服务失败：{e}"),
            };
            self.status_message = Some(StatusMsg::info(feedback));
            return;
        }
        let feedback = match crate::file_ops::stop_service(&service_name) {
            Ok(()) => format!("已发送停止请求给服务 {service_name}（服务真正停下来可能要几秒，如果刷新后还在列表里，稍等一下再检测一次）。"),
            Err(e) => format!("停止服务失败：{e}"),
        };
        self.refresh_lock_check(Some(feedback));
    }

    fn refresh_lock_check(&mut self, feedback: Option<String>) {
        let Some(prev) = &self.lock_check_result else { return };
        let request = prev.request.clone();
        self.run_lock_check_with_feedback(request, feedback);
    }

    fn show_lock_check_modal(&mut self, ctx: &egui::Context) {
        let Some(result) = &self.lock_check_result else { return };
        let mut close = false;
        let mut terminate_pid: Option<u32> = None;
        let mut stop_service_name: Option<String> = None;
        let mut refresh = false;
        let mut terminate_all = false;
        let mut start_layered = false;
        let mut layered_terminate_all = false;
        let mut layered_advance = false;
        egui::Window::new("检测占用")
            .id(egui::Id::new("lock_check_modal"))
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(420.0);
                let kind = if result.is_folder { "文件夹" } else { "文件" };
                ui.horizontal(|ui| {
                    ui.label(format!("{kind}："));
                    ui.label(egui::RichText::new(&result.name).strong());
                });
                if let Some(fb) = &result.action_feedback {
                    ui.add_space(6.0);
                    ui.colored_label(crate::theme::ACCENT_BLUE, fb);
                }
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
                if result.loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("正在检测占用……（不会卡住界面，可以先做别的事）");
                    });
                } else if let Some(e) = &result.error {
                    ui.colored_label(crate::theme::STATUS_ERROR_RED, format!("检测失败：{e}"));
                } else {
                    match &result.rename_probe {
                        Some(crate::file_ops::FolderOccupancy::Free) => {
                            ui.colored_label(Color32::from_rgb(0x34, 0xC7, 0x59), "✓ 重命名探测：没有被占用，可以放心删除/创建符号链接了。");
                        }
                        Some(crate::file_ops::FolderOccupancy::Locked) => {
                            ui.colored_label(crate::theme::STATUS_ERROR_RED, "⚠ 重命名探测：确定被占用（共享/锁冲突）。");
                        }
                        Some(crate::file_ops::FolderOccupancy::Inconclusive(reason)) => {
                            ui.colored_label(Color32::from_rgb(0xF5, 0xA6, 0x23), format!("❔ 重命名探测：无法确定是否被占用——{reason}"));
                        }
                        None => {}
                    }

                    let folder_not_free = result.is_folder
                        && !matches!(result.rename_probe, Some(crate::file_ops::FolderOccupancy::Free));

                    if folder_not_free && self.layered_probe.is_none() {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.button("🔍 具体检测").on_hover_text(
                                "一层一层往下排查：这一层的子文件夹先用重命名探测，\n通过的直接判定没问题，只有没通过的才继续往下查一层，\n不会像之前那样只查前 2000 个文件导致查不全。"
                            ).clicked() {
                                start_layered = true;
                            }
                            ui.label(egui::RichText::new("找出具体是哪个进程/服务占用的").small().color(Color32::from_rgb(0xA0, 0xA0, 0xA0)));
                        });
                    }

                    if let Some(probe) = &self.layered_probe {
                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(6.0);
                        if probe.pending.is_some() {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(format!("正在检测第 {} 层……", probe.layer + 1));
                            });
                        } else if let Some(stats) = &probe.last_layer_stats {
                            ui.label(format!(
                                "第 {} 层：检查了 {} 个子文件夹（{} 个没通过重命名探测）、{} 个文件，这一层新发现 {} 个占用进程/服务。",
                                stats.layer, stats.subfolders_checked, stats.subfolders_locked, stats.files_checked, stats.new_procs_found,
                            ));
                        }
                        if probe.exhausted && probe.pending.is_none() {
                            if probe.procs.is_empty() {
                                ui.colored_label(
                                    Color32::from_rgb(0xF5, 0xA6, 0x23),
                                    "已经查到最深层，没有发现明确占用的进程/服务——如果仍然打不开/删不掉，更可能是权限不足或者系统保护目录，不是被占用。",
                                );
                            } else {
                                ui.colored_label(crate::theme::STATUS_ERROR_RED, format!("已经查到最深层，共发现 {} 个占用进程/服务：", probe.procs.len()));
                            }
                        } else if !probe.procs.is_empty() {
                            ui.colored_label(Color32::from_rgb(0xF5, 0xA6, 0x23), format!("目前累计发现 {} 个占用进程/服务：", probe.procs.len()));
                        }

                        if !probe.procs.is_empty() {
                            ui.horizontal(|ui| {
                                if ui.add(egui::Button::new(egui::RichText::new("⚡ 结束以上全部后重新检测").color(Color32::WHITE))
                                    .fill(crate::theme::DANGER_BUTTON_RED)).clicked()
                                {
                                    layered_terminate_all = true;
                                }
                                if !probe.exhausted && probe.pending.is_none() && ui.button("▶ 先不结束，继续往下查一层").clicked() {
                                    layered_advance = true;
                                }
                            });
                            ui.add_space(6.0);
                            egui::ScrollArea::vertical().max_height(220.0).id_salt("layered_probe_procs").show(ui, |ui| {
                                for p in &probe.procs {
                                    ui.horizontal(|ui| {
                                        ui.vertical(|ui| {
                                            ui.label(egui::RichText::new(&p.app_name).strong());
                                            let sub = match &p.service_name {
                                                Some(svc) => format!("PID {} · 服务: {svc}", p.pid),
                                                None => format!("PID {}", p.pid),
                                            };
                                            ui.label(egui::RichText::new(sub).small().color(Color32::from_rgb(0xA0, 0xA0, 0xA0)));
                                        });
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            if ui.add(egui::Button::new(egui::RichText::new("结束进程").color(Color32::WHITE))
                                                .fill(crate::theme::DANGER_BUTTON_RED)).clicked()
                                            {
                                                terminate_pid = Some(p.pid);
                                            }
                                            if let Some(svc) = &p.service_name
                                                && ui.button("停止服务").clicked() {
                                                    stop_service_name = Some(svc.clone());
                                                }
                                        });
                                    });
                                    ui.separator();
                                }
                            });
                        } else if !probe.exhausted && probe.pending.is_none()
                            && ui.button("▶ 继续往下查一层").clicked() {
                                layered_advance = true;
                            }
                    }

                    let show_process_list = self.layered_probe.is_none()
                        && !matches!(result.rename_probe, Some(crate::file_ops::FolderOccupancy::Free));
                    if show_process_list {
                        if result.rename_probe.is_some() { ui.add_space(6.0); }
                        if result.procs.is_empty() {
                            if result.rename_probe.is_none() {
                                ui.colored_label(Color32::from_rgb(0x34, 0xC7, 0x59), "✓ 没有检测到占用，可以放心删除/创建符号链接了。");
                            } else {
                                ui.label(egui::RichText::new("Restart Manager 没有在检查过的文件里找到具体占用的进程（可能占用的是没查到的文件，也可能是权限一类的问题）。").small().color(Color32::from_rgb(0xA0, 0xA0, 0xA0)));
                            }
                        } else {
                            ui.horizontal(|ui| {
                                ui.colored_label(Color32::from_rgb(0xF5, 0xA6, 0x23), format!("找到 {} 个进程/服务正在占用，处理完下面这些就能继续了：", result.procs.len()));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.add(egui::Button::new(egui::RichText::new("⚡ 一键结束全部").color(Color32::WHITE))
                                        .fill(crate::theme::DANGER_BUTTON_RED)).clicked()
                                    {
                                        terminate_all = true;
                                    }
                                });
                            });
                            ui.add_space(6.0);
                            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                                for p in &result.procs {
                                    ui.horizontal(|ui| {
                                        ui.vertical(|ui| {
                                            ui.label(egui::RichText::new(&p.app_name).strong());
                                            let sub = match &p.service_name {
                                                Some(svc) => format!("PID {} · 服务: {svc}", p.pid),
                                                None => format!("PID {}", p.pid),
                                            };
                                            ui.label(egui::RichText::new(sub).small().color(Color32::from_rgb(0xA0, 0xA0, 0xA0)));
                                        });
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            if ui.add(egui::Button::new(egui::RichText::new("结束进程").color(Color32::WHITE))
                                                .fill(crate::theme::DANGER_BUTTON_RED)).clicked()
                                            {
                                                terminate_pid = Some(p.pid);
                                            }
                                            if let Some(svc) = &p.service_name
                                                && ui.button("停止服务").clicked() {
                                                    stop_service_name = Some(svc.clone());
                                                }
                                        });
                                    });
                                    ui.separator();
                                }
                            });
                        }
                    }
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("关闭").clicked() { close = true; }
                    ui.add_enabled_ui(!result.loading, |ui| {
                        if ui.button("🔄 重新检测").clicked() { refresh = true; }
                    });
                });
            });
        if let Some(pid) = terminate_pid {
            self.lock_check_terminate_process(pid);
        } else if terminate_all {
            self.lock_check_terminate_all();
        } else if let Some(svc) = stop_service_name {
            self.lock_check_stop_service(svc);
        } else if refresh {
            self.refresh_lock_check(None);
        } else if start_layered {
            self.start_layered_probe();
        } else if layered_terminate_all {
            self.layered_probe_terminate_all_and_recheck();
        } else if layered_advance {
            self.advance_layered_probe();
        } else if close {
            self.lock_check_result = None;
            self.layered_probe = None;
        }
    }

    fn show_about_modal(&mut self, ctx: &egui::Context) {
        {
            let Some(about) = self.about.as_mut() else { return };
            if let Some(remaining) = &mut about.countdown {
                *remaining -= ctx.input(|i| i.stable_dt).max(1.0 / 240.0);
                if *remaining <= 0.0 {
                    about.countdown = None;
                }
                ctx.request_repaint();
            }
        }
        let Some(about) = self.about.as_ref() else { return };
        let mut tab = about.tab;
        let logo_id = self.about_textures.logo.id();
        let wechat_id = self.about_textures.wechat.id();
        let alipay_id = self.about_textures.alipay.id();
        let mut action: Option<bool> = None;
        let countdown_secs = about.countdown.map(|t| t.ceil() as u32);
        let mut logo_row_w = about.logo_row_w;
        let mut tabs_row_w = about.tabs_row_w;
        let mut btns_row_w = about.btns_row_w;

        egui::Window::new(format!("关于 {}", crate::about::APP_NAME))
            .id(egui::Id::new("about_modal"))
            .collapsible(false)
            .resizable(false)
            .default_width(400.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                let centered = egui::Layout::top_down(egui::Align::Center);
                let row = egui::Layout::left_to_right(egui::Align::Center);

                let pad = if logo_row_w > 0.0 { ((ui.available_width() - logo_row_w) / 2.0).max(0.0) } else { 0.0 };
                let logo_resp = ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 60.0),
                    row,
                    |ui| {
                        ui.add_space(pad);
                        ui.add(egui::Image::new(egui::load::SizedTexture::new(logo_id, [52.0, 52.0])));
                        ui.vertical(|ui| {
                            ui.heading(egui::RichText::new(crate::about::APP_NAME).strong().size(24.0));
                            ui.label(egui::RichText::new(format!("版本 {}（测试版）", crate::about::APP_VERSION))
                                .size(12.0).color(Color32::from_rgb(0xA0, 0xA0, 0xA0)));
                        });
                    },
                );
                logo_row_w = (logo_resp.response.rect.width() - pad).max(0.0);
                ui.separator();
                ui.with_layout(centered, |ui| {
                    ui.label("一款免费的 Windows 磁盘空间与文件分析器。");
                });
                ui.with_layout(centered, |ui| {
                    ui.label(egui::RichText::new(crate::about::COPYRIGHT_LINE).strong());
                });
                ui.with_layout(centered, |ui| {
                    ui.label(format!("作者：{}　联系邮箱：{}", crate::about::APP_AUTHOR, crate::about::APP_EMAIL));
                });
                ui.add_space(4.0);
                ui.label(egui::RichText::new(crate::about::LICENSE_NOTICE).size(10.5).weak());
                ui.separator();
                ui.with_layout(centered, |ui| {
                    ui.label(egui::RichText::new(format!("☕ {}", crate::about::SPONSOR_HINT)).size(12.0));
                });
                ui.add_space(2.0);
                let pad = if tabs_row_w > 0.0 { ((ui.available_width() - tabs_row_w) / 2.0).max(0.0) } else { 0.0 };
                let tabs_resp = ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 28.0),
                    row,
                    |ui| {
                        ui.add_space(pad);
                        ui.selectable_value(&mut tab, SponsorTab::WeChat, "微信");
                        ui.selectable_value(&mut tab, SponsorTab::Alipay, "支付宝");
                    },
                );
                tabs_row_w = (tabs_resp.response.rect.width() - pad).max(0.0);
                ui.with_layout(centered, |ui| {
                    let qr_id = match tab {
                        SponsorTab::WeChat => wechat_id,
                        SponsorTab::Alipay => alipay_id,
                    };
                    ui.add(egui::Image::new(egui::load::SizedTexture::new(qr_id, [216.0, 216.0])));
                });
                ui.add_space(4.0);
                let pad = if btns_row_w > 0.0 { ((ui.available_width() - btns_row_w) / 2.0).max(0.0) } else { 0.0 };
                let btns_resp = ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 36.0),
                    row,
                    |ui| {
                        ui.add_space(pad);
                        if ui.button("下次一定")
                            .on_hover_text("关闭弹窗，下次打开软件还会再提示")
                            .clicked()
                        {
                            action = Some(false);
                        }
                        let label = match countdown_secs {
                            Some(s) => format!("不再提醒({s}s)"),
                            None => "不再提醒".to_string(),
                        };
                        let resp = ui.add_enabled(
                            countdown_secs.is_none(),
                            egui::Button::new(egui::RichText::new(label).strong()),
                        );
                        if resp.clicked() {
                            action = Some(true);
                        }
                    },
                );
                btns_row_w = (btns_resp.response.rect.width() - pad).max(0.0);
            });

        match action {
            Some(suppress) => {
                if suppress {
                    crate::about::set_sponsor_suppressed();
                }
                self.about = None;
            }
            None => {
                if let Some(about) = self.about.as_mut() {
                    about.tab = tab;
                    about.logo_row_w = logo_row_w;
                    about.tabs_row_w = tabs_row_w;
                    about.btns_row_w = btns_row_w;
                }
            }
        }
    }

    fn show_delete_confirm_modal(&mut self, ctx: &egui::Context) {
        let Some(pending) = &self.pending_delete else { return };
        let kind = if pending.is_folder { "文件夹" } else { "文件" };
        let mut confirm = false;
        let mut cancel = false;
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
        egui::Window::new("确认删除")
            .id(egui::Id::new("delete_confirm_modal"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.label(format!("确定要把这个{kind}删除到回收站吗？"));
                ui.add_space(4.0);
                ui.label(egui::RichText::new(&pending.name).strong());
                ui.label(egui::RichText::new(&pending.full_path).small().color(egui::Color32::from_rgb(0xA0, 0xA0, 0xA0)));
                if pending.is_folder {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("文件夹里的所有内容都会一起被删除。").small().color(egui::Color32::from_rgb(0xF5, 0xA6, 0x23)));
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("取消").clicked() { cancel = true; }
                    if ui.add(egui::Button::new(egui::RichText::new("删除到回收站").color(egui::Color32::WHITE))
                        .fill(crate::theme::DANGER_BUTTON_RED)).clicked()
                    {
                        confirm = true;
                    }
                });
            });
        if cancel {
            self.pending_delete = None;
        } else if confirm {
            self.execute_pending_delete();
        }
    }

    fn execute_pending_delete(&mut self) {
        let Some(pending) = self.pending_delete.take() else { return };
        let full_path = pending.full_path.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::file_ops::delete_to_recycle_bin_with_retry(&full_path));
        });
        self.delete_rx = Some((pending, rx));
    }

    fn poll_delete(&mut self) {
        let Some((_, rx)) = &self.delete_rx else { return };
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("删除线程异常退出".to_string()),
        };
        let (pending, _) = self.delete_rx.take().unwrap();
        match result {
            Ok(()) => {
                self.remove_node_after_success(pending.source, &pending.abs_path, pending.index_entry);
                self.status_message = Some(StatusMsg::info(format!("已删除到回收站: {}", pending.name)));
            }
            Err(e) => {
                self.status_message = Some(StatusMsg::error(format!("删除失败 ({}): {e}", pending.name)));
            }
        }
    }

    fn remove_node_after_success(&mut self, source: DeleteSource, abs_path: &NodePath, index_entry: Option<u32>) {
        match source {
            DeleteSource::Main => {
                if abs_path.len() >= 2
                    && let Some(&pi) = abs_path.first()
                        && let Some(part) = self.partitions.get_mut(pi) {
                            part.remove_at_path(&abs_path[1..]);
                        }
                if self.selected.as_ref() == Some(abs_path) { self.selected = None; }
                self.list_state.expand_version += 1;
                self.main_tree_version += 1;
            }
            DeleteSource::Tab(tab_idx) => {
                let Some(tab) = self.tabs.get_mut(tab_idx) else { return };
                match tab {
                    Tab::Extensions { root, selected, view, .. } | Tab::Duplicates { root, selected, view, .. } => {
                        if abs_path.len() >= 2 {
                            root.remove_at_path(&abs_path[1..]);
                        }
                        if selected.as_ref() == Some(abs_path) { *selected = None; }
                        view.expand_version += 1;
                        view.struct_version += 1;
                    }
                    Tab::SearchList { list_state, .. } => {
                        if let Some(e) = index_entry {
                            list_state.mark_search_row_removed(e);
                        }
                    }
                    Tab::CopyList { tree, selected, list_state, .. } => {
                        if abs_path.len() >= 2
                            && let Some(&pi) = abs_path.first()
                                && let Some(part) = tree.get_mut(pi) {
                                    part.remove_at_path(&abs_path[1..]);
                                }
                        if selected.as_ref() == Some(abs_path) { *selected = None; }
                        list_state.expand_version += 1;
                    }
                    Tab::Main => (),
                }
            }
        }
    }

    fn start_symlink_single(&mut self, source: DeleteSource, abs_path: NodePath, name: String, full_path: String, is_folder: bool) {
        self.symlink_pick_drives = disk_info::list_fixed_drives_with_labels();
        if abs_path.len() < 2 {
            return;
        }
        self.pending_symlink_pick = Some(PendingSymlinkKind::Single { source, abs_path, name, full_path, is_folder });
    }

    fn start_symlink_group(&mut self, tab_idx: usize, abs_path: NodePath, name: String) {
        self.symlink_pick_drives = disk_info::list_fixed_drives_with_labels();
        let node = self.tabs.get(tab_idx).and_then(|t| match t {
            Tab::Duplicates { root, .. } => root.get_at_path(&abs_path[1..]),
            _ => None,
        });
        let Some(node) = node else { return };
        let member_paths: Vec<String> = node.children.iter().filter_map(|c| c.full_path_override.clone()).collect();
        if member_paths.len() < 2 {
            return;
        }
        self.pending_symlink_pick = Some(PendingSymlinkKind::Group { tab_idx, abs_path, name, member_paths });
    }

    fn launch_symlink_job(&mut self, kind: PendingSymlinkKind, drive: char) {
        let base_dir = crate::file_ops::diskforge_base_dir(drive);
        match kind {
            PendingSymlinkKind::Single { source, abs_path, name, full_path, is_folder } => {
                let full_path_for_thread = full_path.clone();
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let result = if is_folder {
                        crate::file_ops::migrate_folder_to_symlink(&full_path_for_thread, &base_dir)
                    } else {
                        crate::file_ops::migrate_file_to_symlink(&full_path_for_thread, &base_dir)
                    };
                    let _ = tx.send(result.map(|target_path| SymlinkOutcome::Single { target_path }));
                });
                self.symlink_rx = Some((SymlinkRequest { source, abs_path, name, full_path, is_folder }, rx));
            }
            PendingSymlinkKind::Group { tab_idx, abs_path, name, member_paths } => {
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let result = (|| -> Result<SymlinkOutcome, String> {
                        let (first, rest) = member_paths.split_first().expect("已检查长度 >= 2");
                        let target_path = crate::file_ops::migrate_file_to_symlink(first, &base_dir)?;
                        let mut done = 1usize;
                        for p in rest {
                            match crate::file_ops::replace_with_symlink(p, &target_path, false, true) {
                                Ok(()) => done += 1,
                                Err(e) => crate::applog::log(&format!("[app] 重复文件组内替换符号链接失败: {p}: {e}")),
                            }
                        }
                        Ok(SymlinkOutcome::Group { target_path, member_count: done, total_count: 1 + rest.len() })
                    })();
                    let _ = tx.send(result);
                });
                self.symlink_rx = Some((SymlinkRequest { source: DeleteSource::Tab(tab_idx), abs_path, name, full_path: String::new(), is_folder: false }, rx));
            }
        }
    }

    fn show_symlink_target_picker_modal(&mut self, ctx: &egui::Context) {
        let Some(pending) = &self.pending_symlink_pick else { return };
        let name = match pending {
            PendingSymlinkKind::Single { name, .. } | PendingSymlinkKind::Group { name, .. } => name.clone(),
        };
        let mut chosen: Option<char> = None;
        let mut cancel = false;
        egui::Window::new("选择符号链接目标分区")
            .id(egui::Id::new("symlink_target_picker"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(320.0);
                ui.horizontal(|ui| {
                    ui.label("为");
                    ui.label(egui::RichText::new(&name).strong());
                    ui.label("选择符号链接目标分区：");
                });
                ui.label(
                    egui::RichText::new("真实数据会存放在该分区的 DiskForge 目录下（文件按内容归档、文件夹按原路径镜像）")
                        .size(11.0)
                        .color(Color32::from_rgb(0xA0, 0xA0, 0xA0)),
                );
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(6.0);
                let drives = self.symlink_pick_drives.clone();
                if drives.is_empty() {
                    ui.colored_label(crate::theme::STATUS_ERROR_RED, "没有检测到可用的固定分区。");
                }
                for (letter, label) in drives {
                    let text = match label {
                        Some(l) if !l.is_empty() => format!("💽 {letter}:  ({l})"),
                        _ => format!("💽 {letter}:"),
                    };
                    if ui.add(egui::Button::new(text).min_size(egui::vec2(280.0, 26.0))).clicked() {
                        chosen = Some(letter);
                    }
                }
                ui.add_space(6.0);
                ui.separator();
                if ui.button("取消").clicked() { cancel = true; }
            });
        if let Some(drive) = chosen {
            let kind = self.pending_symlink_pick.take().unwrap();
            self.launch_symlink_job(kind, drive);
        } else if cancel {
            self.pending_symlink_pick = None;
        }
    }

    fn poll_symlink(&mut self) {
        let Some((_, rx)) = &self.symlink_rx else { return };
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("创建符号链接线程异常退出".to_string()),
        };
        let (request, _) = self.symlink_rx.take().unwrap();
        match result {
            Ok(outcome) => {
                match &outcome {
                    SymlinkOutcome::Single { .. } => {
                        self.refresh_node_after_symlink(request.source, &request.abs_path, &request.full_path, &request.name, request.is_folder);
                    }
                    SymlinkOutcome::Group { .. } => {
                        self.remove_node_after_success(request.source, &request.abs_path, None);
                    }
                }
                self.status_message = Some(StatusMsg::info(match outcome {
                    SymlinkOutcome::Single { target_path } => format!("已创建符号链接: {} → {target_path}", request.name),
                    SymlinkOutcome::Group { target_path, member_count, total_count } => format!(
                        "{} 共 {total_count} 份，已将其中 {member_count} 份统一指向 {target_path}{}",
                        request.name,
                        if member_count < total_count { "（有部分副本处理失败，详情见日志）" } else { "" },
                    ),
                }));
            }
            Err(e) => {
                crate::applog::log(&format!("[app] 创建符号链接失败 ({}): {e}", request.name));
                self.status_message = Some(StatusMsg::error(format!("创建符号链接失败 ({}): {e}", request.name)));
            }
        }
    }

    fn refresh_node_after_symlink(&mut self, source: DeleteSource, abs_path: &NodePath, full_path: &str, name: &str, is_folder: bool) {
        if abs_path.len() < 2 || full_path.is_empty() {
            return;
        }
        match source {
            DeleteSource::Main => {
                if let Some(&pi) = abs_path.first()
                    && let Some(part) = self.partitions.get_mut(pi) {
                        let old = part.get_at_path(&abs_path[1..]);
                        let new_node = old.map(|old| crate::file_ops::build_refreshed_symlink_node(full_path, name, is_folder, old));
                        if let Some(new_node) = new_node {
                            part.replace_at_path(&abs_path[1..], new_node);
                        }
                    }
                self.list_state.expand_version += 1;
                self.main_tree_version += 1;
            }
            DeleteSource::Tab(tab_idx) => {
                let Some(tab) = self.tabs.get_mut(tab_idx) else { return };
                match tab {
                    Tab::Extensions { root, view, .. } | Tab::Duplicates { root, view, .. } => {
                        let old = root.get_at_path(&abs_path[1..]);
                        let new_node = old.map(|old| crate::file_ops::build_refreshed_symlink_node(full_path, name, is_folder, old));
                        if let Some(new_node) = new_node {
                            root.replace_at_path(&abs_path[1..], new_node);
                        }
                        view.expand_version += 1;
                        view.struct_version += 1;
                    }
                    Tab::CopyList { tree, list_state, .. } => {
                        if let Some(&pi) = abs_path.first()
                            && let Some(part) = tree.get_mut(pi) {
                                let old = part.get_at_path(&abs_path[1..]);
                                let new_node = old.map(|old| crate::file_ops::build_refreshed_symlink_node(full_path, name, is_folder, old));
                                if let Some(new_node) = new_node {
                                    part.replace_at_path(&abs_path[1..], new_node);
                                }
                            }
                        list_state.expand_version += 1;
                    }
                    Tab::SearchList { .. } => {}
                    Tab::Main => {}
                }
            }
        }
    }

    fn export_csv(&mut self) {
        if self.partitions.is_empty() { return; }
        let Some(index) = self.main_index.clone() else {
            self.status_message = Some(StatusMsg::info("名字索引还在后台准备中（通常一两秒），请稍后再导出。".to_string()));
            return;
        };
        let dir = std::env::current_exe().ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(std::env::temp_dir);
        let names: Vec<String> = self.partitions.iter().map(|n| n.name.clone()).collect();
        let root_paths = self.partition_root_paths.clone();
        let (tx, rx) = mpsc::channel::<ExportMessage>();
        std::thread::spawn(move || {
            let mut ok_count = 0usize;
            for (i, name) in names.iter().enumerate() {
                let root_path = root_paths.get(i).cloned().unwrap_or_default();
                let safe_name: String = name.chars()
                    .map(|c| if c.is_alphanumeric() { c } else { '_' })
                    .collect();
                let out = dir.join(format!("diskforge_export_{}_{}.csv", i + 1, safe_name));
                let tx_progress = tx.clone();
                let name_for_progress = name.clone();
                let progress = move |_: &str, rows: u64| {
                    let _ = tx_progress.send(ExportMessage::Progress { partition: name_for_progress.clone(), rows });
                };
                match export::export_index_csv(&index, i, &root_path, &out, name, &progress) {
                    Ok((f, d)) => {
                        ok_count += 1;
                        crate::applog::log(&format!("[app] CSV 导出成功: {} (文件={f}, 文件夹={d})", out.display()));
                    }
                    Err(e) => crate::applog::log(&format!("[app] CSV 导出失败 ({name}): {e}")),
                }
            }
            let _ = tx.send(ExportMessage::Done { ok: ok_count, total: names.len(), dir });
        });
        self.export_rx = Some(rx);
    }

    fn poll_export(&mut self) {
        let Some(rx) = &self.export_rx else { return };
        let mut finished = false;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                ExportMessage::Progress { partition, rows } => {
                    self.status_message = Some(StatusMsg::info(format!(
                        "正在导出 CSV…（{partition}：已写 {rows} 行）"
                    )));
                }
                ExportMessage::Done { ok, total, dir } => {
                    self.status_message = Some(if ok == 0 {
                        StatusMsg::error("CSV 导出失败，详情见日志".to_string())
                    } else {
                        StatusMsg::info(format!(
                            "已导出 {ok}/{total} 个 CSV 文件到 {}",
                            dir.display()
                        ))
                    });
                    finished = true;
                }
            }
        }
        if finished {
            self.export_rx = None;
        }
    }

    #[cfg(windows)]
    fn restart_as_admin(&mut self) {
        use std::os::windows::ffi::OsStrExt;
        let Ok(exe) = std::env::current_exe() else { return };
        let verb: Vec<u16> = "runas".encode_utf16().chain(std::iter::once(0)).collect();
        let file: Vec<u16> = exe.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let result = unsafe {
            windows_sys::Win32::UI::Shell::ShellExecuteW(
                std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(),
                std::ptr::null(), std::ptr::null(), 1,
            )
        };
        if (result as isize) > 32 {
            std::process::exit(0);
        } else {
            crate::applog::log(&format!("[app] 以管理员身份重启失败 (ShellExecuteW={result:?})"));
            self.status_message = Some(StatusMsg::error("以管理员身份重启失败，请手动以管理员运行".to_string()));
        }
    }
}

fn show_snapshot_preparing(ui: &mut egui::Ui, text: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() / 2.0 - 40.0).max(0.0));
        ui.spinner();
        ui.add_space(8.0);
        ui.label(RichText::new(text).size(14.0));
        ui.add_space(2.0);
        ui.label(RichText::new("通常不到一秒，界面这段时间可以正常操作其它标签页。")
            .small().color(Color32::from_rgb(0x90, 0x90, 0x90)));
    });
}

fn show_duplicate_loading(ui: &mut egui::Ui, title: &str, phase: crate::dedup::HashPhase, done: u64, total: u64) {
    let (step_label, step_desc) = match phase {
        crate::dedup::HashPhase::Prefilter => (
            "第 1 步 / 共 2 步：快速预筛",
            "读取每个候选文件开头一小段内容，先排除大小相同但内容一开始就不一样的文件。",
        ),
        crate::dedup::HashPhase::Confirm => (
            "第 2 步 / 共 2 步：逐字节确认",
            "逐字节比较文件内容，确认是不是真的一模一样（不是靠哈希碰巧相同）——这一步的文件数取决于实际重复率，重复率越高这一步越慢。",
        ),
    };
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() / 2.0 - 56.0).max(0.0));
        ui.spinner();
        ui.add_space(8.0);
        ui.label(egui::RichText::new(format!("正在比对内容：{title}")).strong().size(15.0));
        ui.add_space(2.0);
        ui.label(egui::RichText::new(step_label).strong().color(crate::theme::ACCENT_BLUE));
        ui.add_space(4.0);
        if total == 0 {
            ui.label("正在收集候选文件…");
        } else {
            ui.label(format!("已处理 {done} / {total} 个文件"));
            let frac = (done as f32 / total as f32).clamp(0.0, 1.0);
            ui.add(egui::ProgressBar::new(frac).desired_width(320.0));
        }
        ui.add_space(6.0);
        ui.label(egui::RichText::new(step_desc).small().color(Color32::from_rgb(0x90, 0x90, 0x90)));
        ui.add_space(2.0);
        ui.label(egui::RichText::new("界面这段时间可以正常操作其它标签页。")
            .small().color(Color32::from_rgb(0x90, 0x90, 0x90)));
    });
}

fn build_scan_paths(picker: &startup::PickerState) -> Vec<PathBuf> {
    let mut drives: Vec<char> = picker.selected_drives.iter().copied().collect();
    drives.sort_unstable();
    let mut paths: Vec<PathBuf> = drives.iter().map(|&l| PathBuf::from(format!("{l}:\\"))).collect();
    paths.extend(picker.custom_paths.iter().map(PathBuf::from));
    paths
}

fn log_scan_summary(node: &Node, info: Option<&DiskInfo>, categories: &[CategoryStat]) {
    let free = info.map(|i| i.free_bytes);
    let used_by_system = info.map(|i| i.used_bytes);
    crate::applog::log(&format!(
        "[app] 扫描完成: {}\n  逻辑大小: {}\n  物理大小: {}\n  系统已用: {}\n  剩余空间: {}\n  文件: {}  文件夹: {}",
        node.name,
        crate::format::human_size(node.logical_size),
        crate::format::human_size(node.physical_size),
        used_by_system.map(crate::format::human_size).unwrap_or_else(|| "未知".to_string()),
        free.map(crate::format::human_size).unwrap_or_else(|| "未知".to_string()),
        node.file_count, node.folder_count,
    ));
    for c in categories {
        if c.size > 0 {
            crate::applog::log(&format!("  分类[{}]: {}", c.label, crate::format::human_size(c.size)));
        }
    }
}

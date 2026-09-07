//! 文件列表树（全量列，中文表头，全部居中，heterogeneous_rows 虚拟化）。
//!
//! 列顺序：名称 | 父占比 | 总占比 | 逻辑大小 | 修改时间 | 物理大小 | 创建时间 | 访问时间
//!         | 项目 | 文件 | 文件夹 | 属性 | 重解析点 | 保留 | 所有者 | 路径
//!
//! 性能：用 `TableBody::heterogeneous_rows()` 做真正的虚拟滚动——
//! egui_extras 源码确认 `row()` 不虚拟化（每帧渲染所有行），
//! 只有 `rows()` / `heterogeneous_rows()` 才跳过不可见行。
//! 磁盘行和子行合并到同一个 heterogeneous_rows 调用里。
//!
//! ## 行数据源（本次搜索重构的核心）
//!
//! `show()` 支持两种 [`ListSource`]，两种模式共用同一套 15 列渲染代码
//! （统一通过 [`RowData`] 视图结构取数，不再各自摸一遍数据源）：
//!
//! * `Tree`——主列表/"复制列表"的可展开树。行来自 `ListState::cache`
//!   的扁平化缓存（展开状态变了才重算），和重构前完全一样。
//! * `Indexed`——"搜索"标签页的摊平文件列表。行直接来自
//!   `Arc<NameIndex>`（见 search_index.rs）：输入查询词 → `find_plain`
//!   （memmem SIMD 多线程扫连续名字缓冲，几毫秒）或 `find_regex`（逐名
//!   并行）得到命中的条目下标；**空查询不走任何匹配**，直接用索引
//!   预生成的全部文件条目表（恒等视图零计算）——这就是"刚开搜索
//!   标签页就在那里'正在搜索…已找到 N 项'跑半天"这个问题的终点。
//!   排序扔到后台线程（索引自含全部字段，后台排序不碰任何 UI 数据），
//!   大结果集排序也不卡 UI；排序完成前先按先序展示，不闪空窗。
//!
//! 旧的 `SearchJob` 分帧搜索机制（3ms 预算/断点续跑/进度文案）已整体
//! 删除——单线程分帧扫树 + 每节点堆分配匹配的路线被索引直查完全取代，
//! 不存在"需要分帧才能不卡"的计算了。

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

/// 隐藏/系统文件的强调色——原来用的是偏黄的琥珀色，看着比较老旧、还容易让人
/// 联想到"警告"，换成更沉稳的石板蓝灰（slate），风格上更接近现在主流深色
/// UI（VS Code/GitHub 那种）标"次要/已忽略"项常用的中性色调，也和下面符号
/// 链接用的紫色、选中用的亮蓝色三者互不混淆。
// 徽标颜色统一走 theme（这里保留一个本文件内的短名字，调用点不动）。
use crate::theme::HIDDEN_ACCENT;
/// 符号链接/junction/挂载点的强调色——和隐藏色的蓝灰调明显区分开的紫色，
/// 两个徽标（H/L）就算同一行一起出现（一个文件既隐藏又是符号链接）也不会
/// 看着像同一种颜色。
use crate::theme::REPARSE_ACCENT;

/// `show()` 的行数据源，见文件顶部说明。纯引用字段，`Copy`——在渲染
/// 闭包里可以反复 match 而不会被 move。
#[derive(Clone, Copy)]
pub enum ListSource<'a> {
    /// 可展开树（主列表/"复制列表"）。
    Tree {
        partitions: &'a [Node],
        partition_infos: &'a [Option<DiskInfo>],
        root_paths: &'a [String],
    },
    /// "搜索"标签页的索引摊平列表。`index` 是打开标签页那一刻拿到的主
    /// 索引快照（`Arc`），列表要展示/过滤/排序的所有数据都在里面。
    Indexed { index: &'a Arc<NameIndex> },
}

/// 扁平化的可见行（树模式）。磁盘行 + 子行统一处理。
#[derive(Clone)]
enum RowKind {
    Disk { pi: usize },
    Child {
        pi: usize, node: *const Node, abs_path: NodePath, indent: f32, depth: u32, parent_logical: u64,
        /// 只有"搜索"标签页那张摊平的文件列表会填这个（`Some(所在文件夹路径)`）——
        /// 普通树模式下位置已经靠展开层级体现了，不需要重复存一遍，恒为 `None`。
        dir_path: Option<String>,
    },
}

#[derive(Clone)]
struct FlatRow {
    height: f32,
    kind: RowKind,
}

/// 一行的统一视图数据：树模式的 `&Node` 和索引模式的 `IdxEntry` 都能填出
/// 这份结构，15 列渲染代码只读它，两种数据源共用同一段渲染。
///
/// 字段名刻意与 `Node` 的字段名保持一致，渲染代码从 `node.name` 换成
/// `rd.name` 是纯机械替换，不会改错列。
struct RowData<'a> {
    name: &'a str,
    is_folder: bool,
    /// 树模式的展开状态（索引摊平行恒为 false，反正摊平行不画箭头）。
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
    /// 行坐标（树模式=缓存里现成的引用；索引模式=条目下标，按需回溯）。
    path_ref: PathRef<'a>,
    /// 索引模式懒回溯出的坐标缓存（树模式永远不用它）。
    abs_path_cell: OnceCell<NodePath>,
    indent: f32,
    depth: u32,
    parent_logical: u64,
    /// "总占比"列的分母（分区根逻辑大小）。
    disk_logical: u64,
    /// 摊平列表的"路径"列内容（树模式恒为 None）。
    dir_path: Option<&'a str>,
    /// 完整路径的数据来源，见 [`FullPathSource`]。
    full_path_source: FullPathSource<'a>,
    /// 完整路径的懒计算缓存：只在 hover / 右键菜单真正用到的那一行才算一次。
    /// 以前是每个可见行每帧无条件算一遍（沿树拼 / format! 拼目录池），滚动手
    /// 在大列表上就是每秒几千次白干的字符串拼接。
    full_path_cell: OnceCell<String>,
    /// 索引行的条目下标（树模式恒为 None）——右键"删除"成功后
    /// app.rs 用它把这一行从标签页的显示列表里剔除。
    index_entry: Option<u32>,
}

/// 行坐标的两种来源：树模式直接引用缓存行里现成的 `NodePath`（零拷贝）；
/// 索引模式只存条目下标，需要时再沿 parent 链回溯（每行 O(深度) 次整数运算）。
enum PathRef<'a> {
    Tree(&'a NodePath),
    Indexed { index: &'a NameIndex, entry: u32 },
}

/// 完整路径的两种拼法来源（惰性求值用）。
enum FullPathSource<'a> {
    /// 树模式：沿真实树拼（需要分区数组和根路径）。
    Tree { partitions: &'a [Node], root_paths: &'a [String] },
    /// 索引模式：目录池里的所在目录 + 名字。
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
    /// 这一行的树坐标。树模式零成本返回引用；索引模式第一次调用时回溯一次
    /// 并缓存（每行最多算一次、且只在真的需要坐标的动作里发生）。
    fn abs_path(&self) -> &NodePath {
        match &self.path_ref {
            PathRef::Tree(p) => p,
            PathRef::Indexed { index, entry } => self
                .abs_path_cell
                .get_or_init(|| index.abs_path_of(*entry)),
        }
    }
    /// 这一行的完整磁盘路径（右键菜单/hover 用）。第一次调用时才算。
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
    /// 选中态判断。索引模式走 `abs_path_eq` 零分配比对（不回溯坐标）。
    fn is_selected(&self, selected: &Option<NodePath>) -> bool {
        let Some(sel) = selected.as_deref() else { return false };
        match &self.path_ref {
            PathRef::Tree(p) => *p == sel,
            PathRef::Indexed { index, entry } => index.abs_path_eq(*entry, sel),
        }
    }
}

/// 见 `ListState::search_query`/`search_query_applied` 上的说明。
const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(180);

/// 摊平列表的搜索结果视图：`base` 是这次查询的原始命中（先序），`order`
/// 是当前展示顺序（排序完成前与 `base` 相同）。
struct SearchView {
    /// 生成这份视图用的查询词（防抖后的生效值）。
    for_query: String,
    /// 生成这份视图用的索引（`Arc::as_ptr`）——同一个标签页的索引理论上
    /// 不变（快照），校验只是防御。
    for_index: usize,
    /// 生成这份视图时 `removed_entries` 的长度——删除会让视图失效重建。
    for_removed: usize,
    /// 原始命中（索引先序）。
    base: Vec<u32>,
    /// 当前展示顺序；`sorted_for == Some(sort)` 时就是排好序的。
    order: Vec<u32>,
    /// `order` 当前对应的排序状态；`None` 表示还没排（先序展示中）。
    sorted_for: Option<SortState>,
}

/// 后台排序任务：索引自含全部排序字段，`sort_order` 在后台线程对
/// `Arc<NameIndex>` 执行，结果通过通道送回 UI——几十万上百万命中的
/// 排序也不占用任何一帧的渲染时间。
struct SortJob {
    sort: SortState,
    rx: Receiver<Vec<u32>>,
}

#[derive(Default)]
pub struct ListState {
    pub sort: SortState,
    pub expand_version: u64,
    cache: Option<(CacheKey, Vec<FlatRow>)>,
    /// 搜索框里用户当前看到的文字——每敲一下键盘就更新，不直接拿它去
    /// 重建结果（防抖，见 `SEARCH_DEBOUNCE`）。
    pub search_query: String,
    /// 上一次真正应用去重建结果用的文字——只有 `search_query` 停止变化超过
    /// `SEARCH_DEBOUNCE` 之后才会同步过来。
    search_query_applied: String,
    /// 最近一次编辑 `search_query` 发生的时间；`None` 表示还没编辑过/已经
    /// 应用过了。
    search_query_changed_at: Option<std::time::Instant>,
    /// 摊平列表的搜索结果视图（只在 `Indexed` 模式使用）。
    search_view: Option<SearchView>,
    /// 进行中的后台排序任务；`None` 表示没有。
    sort_job: Option<SortJob>,
    /// 本标签页里通过右键"删除到回收站"成功删掉的索引条目——删除成功后
    /// 把这一行从显示里剔除（磁盘上文件已经没了，列表还挂着会误导）。
    /// 只在 `Indexed` 模式使用；树模式（主列表/"复制列表"）的删除是直接
    /// 从树上摘节点，用不到这个。
    pub removed_entries: std::collections::HashSet<u32>,
    /// 由 app.rs 的"查找"功能写入：下一次渲染要把这个节点滚动到可视区域里
    /// （展开路径、选中了，但目标不在当前视口内的话，界面上其实什么变化都
    /// 看不出来）。用一次就清空，不会每帧反复滚动、跟用户之后自己手动滚动打架。
    pub pending_scroll: Option<NodePath>,
    /// 与 `pending_scroll` 配套：目标行在索引摊平列表里的条目下标——"查找"
    /// 在"搜索"标签页定位时由 app.rs 一并写入（查找匹配本身就是索引直查
    /// 拿到的条目，顺手带过来零成本）。有这个值时滚动定位直接做整数比对
    /// （在 `order` 里找这个条目，百万行也就一两毫秒）；没有时退回
    /// `NameIndex::abs_path_eq` 零分配坐标比对。用一次就清空。
    pub pending_scroll_entry: Option<u32>,
}

impl ListState {
    /// "删除到回收站"成功后由 app.rs 调用：把这行从摊平列表里剔除，并让
    /// 搜索视图下一帧重建（过滤掉这条）。
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

/// 大小写不敏感比较，不分配新 String（`to_lowercase()` 每次比较都要堆分配一次，
/// 排序是 O(n log n) 次比较，n 一大——比如上万个文件——每帧都这么分配一遍，
/// 就是列表变卡的主因之一）。
fn cmp_ignore_ascii_case(a: &str, b: &str) -> std::cmp::Ordering {
    a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

/// 按当前排序状态给一层 children 排出显示顺序，返回的是 `children` 里的下标
/// （不改变 children 本身的存储顺序——那是 abs_path 依赖的"真实"下标，
/// 排序只影响遍历/显示时按什么顺序走这些下标）。
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
        // 按路径排序要用 `FlatRow::dir_path`（`Node` 本身不存这个），真正的
        // 比较逻辑在排序调用点单独处理，不会走到这里——见 `dir_path`。
        // 这里随便给个不 panic 的占位实现，单纯是为了让 `match` 覆盖所有分支。
        SortKey::Path => std::cmp::Ordering::Equal,
    }
}

/// 表头文字对应的排序键：父占比/总占比两列本质上和逻辑大小同序，都映射到 `Size`。
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

/// 收集子行（含所有已展开的更深层级）。每一层按 `sort` 现算显示顺序
/// （不改变 `node.children` 的实际存储顺序，abs_path 里存的还是真实下标）。
/// `show_reserved`：为 false 时跳过 NTFS 保留的元数据文件（is_reserved，如 $MFT/$LogFile），
/// 对应"视图 > 显示全部信息"关闭的情况。
/// 迭代版本：用显式栈代替原生递归，栈里存"待处理节点 + 它的相对路径/深度/父 logical_size"，
/// 子节点按倒序入栈，保证出栈顺序（先序、从左到右）和排好的显示顺序完全一致。
#[allow(clippy::too_many_arguments)] // 8 个参数都是必要的遍历上下文，硬拆反而难读
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

/// 只有排序/展开/显示全部信息/分区数组真的变了才重新扫一遍树、重新排序，
/// 避免每一帧都对（可能很大的）子树做 `sort_by`。抽成独立函数是因为
/// "查找"功能触发滚动定位的那一帧需要提前调用它（见 `show()` 里的说明），
/// 平时的调用点又需要在表头之后调用，两处共用同一份逻辑，不能各写一遍。
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

    // ── 搜索框 ──
    // 只有"搜索"标签页（`ListSource::Indexed`，菜单开的那个新标签页）才带
    // 这个搜索框；主列表/"复制列表"不带——那两个配的是"查找"悬浮窗
    // （Ctrl+F/菜单），定位到某一项在树里的位置，不是筛选列表。两个是
    // 分开的功能：
    //   - 查找：不改变列表内容，只是展开+选中+滚动到某一项，"上一个/下一个"
    //     在原来的树形结构里跳。
    //   - 搜索：这张表永远是摊平的全部文件列表（不含文件夹、不分层级），
    //     输入框里的内容实时过滤显示哪些行，复用同一套排序/右键菜单，比如
    //     筛出全部 `.mp4` 之后直接点"逻辑大小"表头按大小排序。
    // 通配符（`*`/`?`）自动识别，不用勾任何开关——见 `Matcher::build_auto`。
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

        // 节流防抖：输入框里的文字停止变化满 `SEARCH_DEBOUNCE` 之后，才把
        // 它同步成"生效"的查询词去真正重建结果。索引直查本身只要几毫秒，
        // 防抖保留不是为了性能兜底，而是让"连续敲字"时表格内容不跟着抖、
        // 观感和原来保持一致。
        match state.search_query_changed_at {
            Some(t) if t.elapsed() >= SEARCH_DEBOUNCE => {
                state.search_query_applied = state.search_query.clone();
                state.search_query_changed_at = None;
            }
            Some(t) => {
                // 还在防抖窗口内——排一次稍后的重绘，到时间了就算用户不再
                // 有任何输入，也能准时把这次修改应用上。
                ui.ctx().request_repaint_after(SEARCH_DEBOUNCE - t.elapsed());
            }
            None => {}
        }

        // ── 搜索结果视图管理 ──
        // 需要重建的判定：查询词变了 / 索引换了 / 删除过行。重建本身只有
        // 两种成本：空查询=预生成文件条目表的一次 clone（几毫秒级内存拷贝）；
        // 非空查询=一次索引直查（memmem SIMD 多线程，几毫秒；正则稍长）。
        // 不存在任何"分帧"环节，这一帧内直接完成。
        let index_id = Arc::as_ptr(index) as usize;
        let applied = state.search_query_applied.clone();
        let need_rebuild = match &state.search_view {
            Some(v) => v.for_query != applied || v.for_index != index_id || v.for_removed != state.removed_entries.len(),
            None => true,
        };
        if need_rebuild {
            let mut base: Vec<u32> = if applied.trim().is_empty() {
                // 空查询 = 恒等视图：全部文件按索引先序列出，零匹配计算。
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
        // 表头排序：把排序扔到后台线程（读 Arc<NameIndex>，不碰任何 UI
        // 状态），完成前先按先序展示，不闪空窗。查询词变化重建视图时旧
        // 任务直接作废（rx 被 drop，后台线程发不出去就静默收尾）。
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
        // 收后台排序结果：到了就替换展示顺序，这一帧先照旧画（下一帧生效）。
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
                    // 排序还在跑——排一次到点的重绘，到了自然刷新，不会
                    // 卡在旧顺序上不动。
                    ui.ctx().request_repaint();
                }
                Err(TryRecvError::Disconnected) => {
                    state.sort_job = None;
                }
            }
        }
    }

    let action_cell: Cell<TreeAction> = Cell::new(TreeAction::None);
    // "关键列"始终保持正常宽度；非关键列在 show_all=false 时收缩到 0 宽度
    // （而不是真的减少 .column()/col() 调用次数）。egui_extras::TableBuilder 要求
    // 表头、每一行声明的列数必须严格一致，三处手写的列数只要有一处漏改就会在运行时
    // 出错/错位，且这里没有编译器能提前发现这种不匹配。用"宽度收缩到 0"来实现
    // "非关键列隐藏"，可以保证列数在任何开关状态下都完全不变，从根上排除这类风险。
    let extra_w = |normal: f32| if show_all { normal } else { 0.0 };
    // "路径"这一列只有搜索模式才用得上（普通树模式下位置已经靠展开层级
    // 体现了）——道理和 `extra_w` 一样，用宽度收缩到 0 来隐藏，不是真的
    // 减少列数。
    let path_col_w = if searching { 220.0 } else { 0.0 };
    // "查找"功能定位过来的目标——只消费一次，见 `ListState::pending_scroll`
    // 上的说明。要在构建 ScrollArea 之前先取出来：一是滚动到目标行需要在
    // 建表头之前就知道要不要调用 `scroll_to_row`（下面会再用到），二是
    // 横向滚动条也要在这里一并复位——不然名字缩进层级深的时候，即使纵向
    // 滚动对了，横向还停留在之前滚到的位置，那一项的名字照样在视野外面。
    let pending_scroll = state.pending_scroll.take();
    // 与 pending_scroll 配套的条目下标（Indexed 模式的快速定位用），同样
    // 只消费一次。要在建 ScrollArea 之前一起取出来。
    let pending_scroll_entry = state.pending_scroll_entry.take();
    let mut scroll_area = egui::ScrollArea::both().auto_shrink([false, false]);
    if pending_scroll.is_some() {
        scroll_area = scroll_area.horizontal_scroll_offset(0.0);
    }
    scroll_area.show(ui, |ui| {
        // 表格默认的 item_spacing 会在行与行、列与列之间留出几像素的间距——这段间距
        // 不属于任何一个单元格，我们手画的高亮背景、手动建的点击感应区都不会覆盖到它，
        // 于是就成了"看着是空白、点了没反应"的死区。这里直接把间距清零，行与行之间
        // 紧挨着，不会再有这种缝隙。
        ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
        let mut builder = egui_extras::TableBuilder::new(ui)
            // 表头/列宽记忆按 (`show_all`, `searching`) 分开存独立的
            // 几份（用 id_salt 区分），不会共用同一份——共用的话，某一种
            // 模式下被强制收缩到 0 宽度的列，这个 0 会被当成"用户拖出来的
            // 宽度"记下来；切到另一种想要这一列正常宽度的模式时，因为
            // 已经有记忆值了，`initial()` 给的新宽度不会生效，列就一直卡在
            // 0 宽度，得手动拖一下才能出来——这是之前真实出现过的 bug，
            // 用分开的记忆从根上避免。
            .id_salt(("tree_list_table", show_all, searching))
            .striped(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .auto_shrink([false, false])
            .column(egui_extras::Column::initial(200.0).at_least(80.0).clip(true).resizable(true))  // 名称
            .column(egui_extras::Column::initial(85.0).clip(true).resizable(true))   // 父占比
            .column(egui_extras::Column::initial(85.0).clip(true).resizable(true))   // 总占比
            .column(egui_extras::Column::initial(85.0).clip(true).resizable(true))   // 逻辑大小
            .column(egui_extras::Column::initial(120.0).clip(true).resizable(true))  // 修改时间
            .column(egui_extras::Column::initial(85.0).clip(true).resizable(true))   // 物理大小
            .column(egui_extras::Column::initial(extra_w(120.0)).clip(true).resizable(show_all))  // 创建时间
            .column(egui_extras::Column::initial(extra_w(120.0)).clip(true).resizable(show_all))  // 访问时间
            .column(egui_extras::Column::initial(extra_w(55.0)).clip(true).resizable(show_all))   // 项目
            .column(egui_extras::Column::initial(extra_w(55.0)).clip(true).resizable(show_all))   // 文件
            .column(egui_extras::Column::initial(extra_w(55.0)).clip(true).resizable(show_all))   // 文件夹
            .column(egui_extras::Column::initial(extra_w(50.0)).clip(true).resizable(show_all))   // 属性
            .column(egui_extras::Column::initial(extra_w(55.0)).clip(true).resizable(show_all))   // 重解析点
            .column(egui_extras::Column::initial(extra_w(40.0)).clip(true).resizable(show_all))   // 保留
            .column(egui_extras::Column::initial(extra_w(80.0)).clip(true).resizable(show_all).at_least(0.0))  // 所有者
            .column(egui_extras::Column::initial(path_col_w).clip(true).resizable(searching).at_least(0.0)); // 路径

        builder = builder.sense(egui::Sense::click());

        // 只有这一帧真的要把某个节点滚动到可视区域时，才需要在建表头之前把
        // flat_rows 先算出来——`TableBuilder::scroll_to_row()` 必须在
        // `.header()` 消费掉 builder 之前调用。这里用的排序还是"点表头之前"
        // 那一刻的状态；如果这一帧恰好又点了表头（极端小概率的巧合），下面
        // 会用新排序重新建一遍缓存，这一帧的滚动目标行号可能对不上新顺序，
        // 但下一帧就会用新排序正确定位——影响小到可以忽略。
        if let Some(target) = &pending_scroll {
            if searching {
                if let ListSource::Indexed { index } = source
                    && let Some(v) = &state.search_view {
                        // 定位有两条路："查找"从索引直查拿到过条目下标的话，
                        // 直接在 order 里做整数比对——百万行视图也就一两毫秒。
                        // 没有条目下标就退回 `abs_path_eq` 零分配坐标比对。
                        // 原来这里对每一行都调 `abs_path_of`（每行堆分配一个
                        // Vec 再逐层回溯），百万行视图一次定位就是几百毫秒——
                        // "搜索"标签页上用查找悬浮窗、每打一个词就卡一下的
                        // 原因就是它。
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
        // 表头点击立刻生效（排序状态在渲染 body 之前就更新），不用等下一帧——
        // 搜索模式的排序任务在下一帧重新评估（上面那段逻辑），树模式直接重建缓存。
        if let Some(key) = sort_clicked_cell.get() { state.sort.click(key); }

        // 树模式：只有排序/展开/显示全部信息/分区数组真的变了才重新扫一遍
        // 树、重新排序（`rebuild_tree_cache` 内部自己判断要不要真的重建，
        // 上面有滚动目标时已经调过一次，这里大多数情况下只是命中缓存）。
        if !searching
            && let ListSource::Tree { partitions, .. } = source {
                rebuild_tree_cache(partitions, show_all, state);
            }
        // 两种模式的行源都在这里解出来（对 `state` 只剩不可变借用，后面
        // 的渲染闭包只读它们）：
        //   树模式 = 缓存好的 `Vec<FlatRow>`；
        //   索引模式 = 搜索视图的展示顺序 `order`（排序任务完成前就是先序）。
        let empty_rows: Vec<FlatRow> = Vec::new();
        let (tree_rows, order_rows): (&Vec<FlatRow>, &[u32]) = if searching {
            (&empty_rows, state.search_view.as_ref().map(|v| v.order.as_slice()).unwrap_or(&[]))
        } else {
            (state.cache.as_ref().map(|(_, r)| r).unwrap_or(&empty_rows), &[])
        };
        let total_rows = if searching { order_rows.len() } else { tree_rows.len() };
        // heights：直接给 `heterogeneous_rows` 一个惰性 ExactSizeIterator，
        // 不再每帧收集一整张 Vec<f32>——搜索页空查询的百万行视图 = 每帧
        // 4MB 堆分配 + memset（60fps 下 240MB/s 的纯浪费流量），树模式同类
        // 问题只是规模小些。行高本来就只有 ROW_H/DISK_ROW_H 两种常量，迭代器
        // 形态零分配；Box 装一下两种模式统一类型（每帧一次小分配）。
        let heights: Box<dyn ExactSizeIterator<Item = f32>> = if searching {
            Box::new(std::iter::repeat_n(ROW_H, total_rows))
        } else {
            Box::new(tree_rows.iter().map(|r| r.height))
        };

        table
            .body(|body| {
                let mut final_action = TreeAction::None;

                // ── 用 heterogeneous_rows 做虚拟化渲染 ──
                let clicked_row: Cell<usize> = Cell::new(usize::MAX);
                // 右键点击只负责"选中"，不负责"展开/折叠"——和左键分开处理
                // （见下面处理点击结果的地方），符合"右键只是为了弹菜单，
                // 不应该顺带改变树的展开状态"这个直觉，但右键点到的这一项
                // 还是应该跟着高亮选中，不然右键弹出菜单时定位不到菜单对应
                // 的是哪一项。
                let secondary_clicked_row: Cell<usize> = Cell::new(usize::MAX);
                // 右键菜单点"删除到回收站"/"检测占用"这类需要直接产出 TreeAction
                // 的操作时，通过这个 Cell 把请求带出 body 闭包（菜单是嵌在 row.col
                // 更深一层的闭包里画的，够不着外层的 final_action）。
                let action_request: Cell<Option<TreeAction>> = Cell::new(None);

                // 每一行的统一准备产物：磁盘行走自己的渲染分支；子行（树模式
                // 的 &Node 与索引模式的 IdxEntry）统一成 `RowData` 后共用同一
                // 段 15 列渲染代码——两种数据源从此不可能在列上出现不一致。
                // large_enum_variant：Child 变体（RowData）比 Disk 大得多，但
                // 这个枚举是"每行构造一次、当帧用完"的栈上临时值，装箱反而
                // 给最热的渲染路径平添一次堆分配——这里允许大小差是刻意的。
                #[allow(clippy::large_enum_variant)]
                enum Prepared<'a> {
                    Disk { pi: usize },
                    Child(RowData<'a>),
                }

                body.heterogeneous_rows(heights.into_iter(), |mut row| {
                    let row_idx = row.index();
                    // ── 行数据准备 ──
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
                                        // 坐标直接引用缓存行里现成的 NodePath（旧实现每个
                                        // 可见行每帧 clone 一次 Vec）；路径也在真的用到时才拼。
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
                            // 完整路径不再每行每帧提前拼（旧实现这里一次 format!），
                            // 挪进 full_path_source 由 hover/右键时懒算。
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
                                // 坐标只存条目下标，需要时才回溯（选中态比对走
                                // abs_path_eq 零分配路径，根本不回溯）。
                                path_ref: PathRef::Indexed { index, entry: e as u32 },
                                abs_path_cell: OnceCell::new(),
                                // 搜索结果是摊平的列表，不体现真实目录层级——中间的
                                // 父文件夹本来就不显示，缩进了也没有参照物，统一用
                                // "顶层文件"同款的缩进（和重构前完全一样）。
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

                    // ── 磁盘行（只有树模式有）──
                    if let Some(Prepared::Disk { pi }) = &prepared {
                        let pi = *pi;
                        // prepared 是 Disk 只可能来自 Tree 分支（见上面的 match），
                        // 这里直接解构，ListSource 是 Copy，不影响后续使用。
                        let ListSource::Tree { partitions, partition_infos, root_paths } = source else { return; };
                        let partition = &partitions[pi];
                        let info = partition_infos.get(pi).and_then(|i| i.as_ref());
                        let part_selected = selected.as_deref() == Some(&[pi]);
                        let total = info.map(|i| i.total_bytes).unwrap_or(partition.logical_size.max(1));
                        let part_pct = if total > 0 { partition.logical_size as f32 / total as f32 } else { 0.0 };
                        let p = partition;
                        let info_ref = info;
                        let root_path = root_paths.get(pi).cloned().unwrap_or_default();

                        // 名称
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
                        // 父占比
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } draw_bar(ui.painter(),r,1.0,crate::theme::HEADER_ACTIVE_YELLOW); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        // 总占比
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } draw_bar(ui.painter(),r,part_pct,crate::theme::ACCENT_BLUE); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        // 逻辑大小
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,human_size(p.logical_size),egui::FontId::proportional(11.0),crate::theme::ACCENT_BLUE); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        // 修改时间
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } let t=info_ref.map(|i|i.file_system.clone()).filter(|s|!s.is_empty()).unwrap_or_else(||format_filetime(p.modified_ft)); ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,t,egui::FontId::proportional(11.0),Color32::from_rgb(0xA0,0xC0,0xE0)); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        // 物理大小
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,human_size(p.physical_size),egui::FontId::proportional(11.0),Color32::from_rgb(0xF5,0xA6,0x23)); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        // 创建时间
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } let s=format_filetime(p.created_ft); ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,if s.is_empty(){"—".into()}else{s},egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0)); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        // 访问时间
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } let s=format_filetime(p.accessed_ft); ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,if s.is_empty(){"—".into()}else{s},egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0)); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        // 项目/文件/文件夹
                        for val in [p.file_count+p.folder_count, p.file_count, p.folder_count] {
                            row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,format!("{}",val),egui::FontId::proportional(11.0),Color32::from_rgb(0xC0,0xC0,0xC0)); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        }
                        // 属性
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,format_attributes(p.attributes),egui::FontId::proportional(11.0),Color32::from_rgb(0xC0,0xC0,0xC0)); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        // 重解析点
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } let t=if p.reparse_tag!=0 {format!("0x{:X}",p.reparse_tag)}else{"—".into()}; ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,t,egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0)); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        // 保留
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } let t=if p.is_reserved {"是"}else{"—"}; ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,t,egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0)); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        // 所有者
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); } let t=if p.owner.is_empty(){"—".into()}else{p.owner.clone()}; ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,t,egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0)); if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        // 路径（磁盘行没有意义，留空——列数必须和 Child 分支完全一致）
                        row.col(|ui| { let r=ui.available_rect_before_wrap(); let resp=ui.allocate_rect(r,Sense::click()); if part_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu_disk(ui, pi, &root_path, &action_request)); });
                        return;
                    }

                    // ── 子行：树模式(&Node) 与索引模式(IdxEntry) 共用的 15 列渲染 ──
                    let Some(Prepared::Child(rd)) = &prepared else { return; };
                    let is_folder = rd.is_folder;
                    let is_selected = rd.is_selected(selected);
                    let pct = if rd.parent_logical > 0 { rd.logical_size as f32 / rd.parent_logical as f32 } else { 0.0 };
                    let total_pct = if rd.disk_logical > 0 { rd.logical_size as f32 / rd.disk_logical as f32 } else { 0.0 };
                    let bar_color = depth_color(rd.depth, is_folder);
                    let hidden = rd.is_hidden_or_system();
                    let is_reparse = rd.is_reparse_point();

                            // 名称
                            row.col(|ui| {
                                let rect = ui.available_rect_before_wrap();
                                let resp = ui.allocate_rect(rect, Sense::click());
                                if is_selected { ui.painter().rect_filled(rect, 0.0, crate::theme::SELECT_BG); }
                                // 缩进参考线：每一层级画一条竖线贯穿整行，展开层级多的时候
                                // 能顺着线看清楚某一项到底属于哪一层，而不是只能数缩进空格数。
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
                                // 符号链接给一个专属的浅紫色文字——之前只有徽标能看出这是符号
                                // 链接，文字颜色和普通文件/文件夹一样，不仔细看很容易漏掉；
                                // 现在的浅紫色和 REPARSE_ACCENT（徽标用的饱和紫）同色系但更浅、
                                // 更适合大段文字阅读，同时和隐藏文件的"整体调暗"效果是两种不同
                                // 的视觉语言（调暗淡出 vs 文字本身换色），两者叠加互不冲突：
                                // 一个文件既隐藏又是符号链接时，紫色文字外面再蒙一层暗，不会被
                                // 误认成同一种状态。
                                let tc = if is_selected {Color32::from_rgb(0xFF,0xFF,0x80)}
                                    else if is_reparse {crate::theme::REPARSE_TEXT}
                                    else if is_folder {Color32::WHITE} else {Color32::from_rgb(0xCC,0xCC,0xCC)};
                                // 徽标（H/L）要留在最后画、盖在"整体调暗"效果之上，保持鲜艳、
                                // 始终看得清楚——但它们占的宽度要先算出来，好让名字文字从
                                // 正确的位置开始画，所以这里先只算 rect、不画。
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
                                // 现在有独立的"路径"列了，名字这里不用再附带路径文字——
                                // 见 `dir_path` 那一列。
                                let name_text = format!("{icon} {}", rd.name);
                                p.text(Pos2::new(text_x,rect.center().y),egui::Align2::LEFT_CENTER,name_text,egui::FontId::proportional(13.0),tc);
                                if hidden {
                                    // 隐藏/系统项："整体调暗"效果：不是单独换个背景色块，而是在这
                                    // 一整格已经画好的内容（缩进线、箭头、名字文字）上面盖一层半
                                    // 透明的、跟应用背景色一样的颜色——视觉上就像蒙了一层灰、透出
                                    // 后面的背景，接近"这一项已经淡出、次要"的直觉，比单独加一块
                                    // 高亮色更不扎眼。徽标（H/L）故意排在这一步*之后*画，不受这层
                                    // 调暗影响，一直保持鲜艳、看得清楚。
                                    p.rect_filled(rect, 0.0, crate::theme::DIM_OVERLAY);
                                }
                                if let Some(badge) = hidden_badge {
                                    p.rect_filled(badge, 3.0, HIDDEN_ACCENT);
                                    p.text(badge.center(), egui::Align2::CENTER_CENTER, "H", egui::FontId::proportional(9.5), Color32::WHITE);
                                }
                                if let Some(badge) = reparse_badge {
                                    // 符号链接/junction/挂载点：磁盘扫描的语义上和 Windows 资源管理器
                                    // 保持一致——它是什么状态就展示成什么状态，不应该"因为不好处理
                                    // 就干脆藏起来不显示"（否则用户在资源管理器里能看到的东西，工具里
                                    // 反而看不到，前后不一致）。用一个紫色 L 徽标标出来，一眼能和普通
                                    // 文件/文件夹区分开、也能和隐藏的 H 徽标区分开（两种颜色刻意选得
                                    // 不一样，一个文件同时命中两者时 HL 两个徽标挨着显示，一眼就能
                                    // 看出这是两种不同的状态叠加，不是同一种）；这个节点本身的大小/
                                    // 时间等其它列该怎么显示还怎么显示，不受这个徽标影响。
                                    p.rect_filled(badge, 3.0, REPARSE_ACCENT);
                                    p.text(badge.center(), egui::Align2::CENTER_CENTER, "L", egui::FontId::proportional(9.5), Color32::WHITE);
                                }
                                // hover 文案只在真的悬停到这一行时才拼（含完整路径的
                                // 懒计算）——以前每行每帧都提前拼好字符串，悬停一次
                                // 都没发生也照付成本。
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
                            // 父占比
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }draw_bar(ui.painter(),r,pct,bar_color);if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            // 总占比
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }draw_bar(ui.painter(),r,total_pct,crate::theme::ACCENT_BLUE);if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            // 逻辑大小
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,human_size(rd.logical_size),egui::FontId::proportional(11.0),crate::theme::ACCENT_BLUE);if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            // 修改时间
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }let s=format_filetime(rd.modified_ft);ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,if s.is_empty(){"—".into()}else{s},egui::FontId::proportional(11.0),Color32::from_rgb(0xC0,0xC0,0xC0));if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            // 物理大小
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,human_size(rd.physical_size),egui::FontId::proportional(11.0),Color32::from_rgb(0xF5,0xA6,0x23));if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            // 创建时间
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }let s=format_filetime(rd.created_ft);ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,if s.is_empty(){"—".into()}else{s},egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0));if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            // 访问时间
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }let s=format_filetime(rd.accessed_ft);ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,if s.is_empty(){"—".into()}else{s},egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0));if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            // 项目/文件/文件夹
                            for val in [if is_folder{rd.file_count+rd.folder_count}else{0}, if is_folder{rd.file_count}else{0}, if is_folder{rd.folder_count}else{0}] {
                                row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }let t=if is_folder{format!("{}",val)}else{"—".into()};ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,t,egui::FontId::proportional(11.0),Color32::from_rgb(0xC0,0xC0,0xC0));if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            }
                            // 属性
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,format_attributes(rd.attributes),egui::FontId::proportional(11.0),Color32::from_rgb(0xC0,0xC0,0xC0));if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            // 重解析点
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }let t=if rd.reparse_tag!=0{format!("0x{:X}",rd.reparse_tag)}else{"—".into()};ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,t,egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0));if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            // 保留
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }let t=if rd.is_reserved{"是"}else{"—"};ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,t,egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0));if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            // 所有者
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }let t=if rd.owner.is_empty(){"—".into()}else{rd.owner.to_string()};ui.painter().text(r.center(),egui::Align2::CENTER_CENTER,t,egui::FontId::proportional(10.0),Color32::from_rgb(0xC0,0xC0,0xC0));if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                            // 路径——只有搜索模式（`dir_path` 有值）才会真的显示文字，
                            // 普通树模式下这一列宽度是 0，画不画字都看不见，但列本身
                            // 还是要有（列数必须和表头/其它行严格一致）。
                            row.col(|ui|{let r=ui.available_rect_before_wrap();let resp=ui.allocate_rect(r,Sense::click()); if is_selected { ui.painter().rect_filled(r, 0.0, crate::theme::SELECT_BG); }; if let Some(dir) = rd.dir_path { ui.painter().text(r.left_center()+egui::vec2(4.0,0.0),egui::Align2::LEFT_CENTER,dir,egui::FontId::proportional(11.5),Color32::from_rgb(0xA0,0xA0,0xA0)); }; if hidden { ui.painter().rect_filled(r, 0.0, crate::theme::DIM_OVERLAY); }; if resp.clicked(){clicked_row.set(row_idx);}; if resp.secondary_clicked(){secondary_clicked_row.set(row_idx);} resp.context_menu(|ui| context_menu(ui, is_folder, is_reparse, rd.name, rd.full_path(), rd.abs_path(), rd.index_entry, &action_request)); });
                    });

                // 处理点击
                let clicked_idx = clicked_row.into_inner();
                if clicked_idx != usize::MAX && clicked_idx < total_rows {
                    if searching {
                        // 索引摊平行：结果是摊平的列表，没有"展开子项"这回事——
                        // 点文件夹匹配项也只是选中它（方便接下来右键操作）。
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
                                // 搜索结果是摊平的列表，没有"展开子项"这回事——点文件夹匹配项
                                // 只是选中它（方便接下来右键操作），不会去动真实树上的展开状态。
                                final_action = if child.is_folder() {
                                    TreeAction::ToggleExpand(abs)
                                } else {
                                    TreeAction::Select(abs)
                                };
                            }
                        }
                    }
                } else {
                    // 左键没点到任何行，再看看是不是右键点到了——右键只选中，
                    // 不触发展开/折叠（不管点到的是不是文件夹），道理见上面
                    // `secondary_clicked_row` 声明处的说明。
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
                // 删除请求优先：正常行点击（左键选中/右键仅弹出菜单）不会同时设置
                // action_request，两者不会真的抢——这里只是给"万一"兜个底。
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

/// 从 abs_path（第一个元素是分区下标，后面是逐层子节点下标）拼出完整文件系统路径。
/// 只在右键菜单打开的那一刻才调用（不是每帧都算），开销可以忽略。
fn build_full_path(partitions: &[Node], root_paths: &[String], abs_path: &[usize]) -> String {
    let Some(&pi) = abs_path.first() else { return String::new() };
    let mut path = root_paths.get(pi).cloned().unwrap_or_default().trim_end_matches('\\').to_string();
    let Some(mut cur) = partitions.get(pi) else { return path };
    for &i in &abs_path[1..] {
        let Some(n) = cur.children.get(i) else { break };
        cur = n;
        if path.is_empty() { path = cur.name.clone(); } else { path.push('\\'); path.push_str(&cur.name); }
    }
    // 分析视图（扩展名分类/重复文件查找）里的合成节点：它们在合成树里的位置和真实磁盘
    // 目录结构对不上，沿祖先名字拼出来的 path 是错的，有 full_path_override 就用它。
    if let Some(real) = &cur.full_path_override {
        return real.clone();
    }
    path
}

/// Windows 下打开资源管理器并选中某个文件/文件夹；`select_self` 为 true 时定位到这一项本身，
/// 否则是"打开这个文件夹"（用于文件的"打开所在文件夹"——选中文件本身，而不是钻进它内部，
/// 因为文件打不开"进入"）。
///
/// 路径必须整体带双引号：`explorer /select,C:\some path\file.txt`（不带引号）在路径带空格时
/// 会静默失败，退化成打开资源管理器的默认位置（很多机器上是"文档"），而不是报错或者什么都
/// 不做——这是 Windows 一个有据可查的老毛病，不是这边逻辑写错了。之前就是漏了这层引号，
/// 导致"有的文件用资源管理器打开会跳到 Documents"。
#[cfg(windows)]
/// 关键点：必须用 `raw_arg` 而不是普通的 `arg`。Rust 标准库的 `Command::arg()` 在
/// Windows 上会对参数里的引号做自己的转义（比如把 `"` 转成 `\"`），这是为了让参数能被
/// 标准的 C 运行时命令行解析器正确还原成"一个完整参数"。但 explorer.exe 对 `/select,`
/// 这种开关根本不走那套标准解析逻辑，它想看到的就是命令行里字面意义上的引号字符。
/// Rust 加了转义之后，explorer.exe 解析不出真正的路径，会静默失败、退化成打开默认位置
/// （很多机器上是"文档"）——这正是"用资源管理器打开却跳到 Documents"的真正原因。
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

/// 文件/文件夹右键菜单。
///
/// `index_entry`：这一行来自索引摊平列表时是它的条目下标（Some）——右键
/// "删除到回收站"成功后，app.rs 用它把这一行从该标签页的显示里剔除；
/// 树模式的行不来自索引，恒为 None。
#[allow(clippy::too_many_arguments)] // 右键菜单需要的 8 项上下文缺一不可
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
    // "属性" 直接调系统原生对话框，不用二次确认——纯只读操作。
    if ui.button("ℹ 属性").clicked() {
        crate::file_ops::open_properties(full_path);
        ui.close();
    }
    // "检测占用"也是纯只读查询，不用二次确认，直接发请求出去；app.rs 收到
    // 之后去查 Restart Manager、弹一个结果窗口（不在这里等结果——查询本身
    // 走的是 Win32 API 调用，交给 app.rs 统一处理，跟"删除"共用同一条
    // action_request 通道）。
    if ui.button("🔍 检测占用").clicked() {
        action_request.set(Some(TreeAction::RequestCheckLock {
            abs_path: abs_path.clone(),
            name: name.to_string(),
            full_path: full_path.to_string(),
            is_folder,
        }));
        ui.close();
    }
    // 去重/迁移到其他盘：这一项本身如果就是符号链接/junction/挂载点，禁用它——
    // 对着一个"指向别处的指针"再迁移一次没有实际意义：文件符号链接会把它指向的
    // 真实内容重新复制一份、建一条新链接；文件夹符号链接同理，会把目标目录的内容
    // 递归复制一份出来。两种情况实际数据都没有减少，反而多占了一份磁盘空间，
    // 也不是用户点这个菜单项时想要的效果，所以直接在入口这里拦掉，比事后靠
    // "虽然不会失败，但效果奇怪"更友好。注意这个判断只看这一项自己是不是
    // reparse point——一个普通文件夹里面部分文件是符号链接完全不受影响，
    // 那些文件本身右键点出来的菜单里这一项才会被禁用，文件夹本身正常可用。
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
    // "删除"只在这里发个请求（红色强调，提醒这是破坏性操作），真正执行前
    // app.rs 会弹一个确认框——回收站虽然能找回，但"点错就没了"这种事故
    // 不该一次点击就直接生效。
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

/// 磁盘/根目录行的右键菜单。删除到回收站不适用于整个分区/扫描根目录，这里不放；
/// "属性"看的是这个根目录本身（比如某个磁盘分区），一样直接调系统对话框。
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



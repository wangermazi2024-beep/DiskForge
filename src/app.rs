//! 应用主状态：启动即显示主界面（空的），弹窗选分区/目录 → 顺序批量扫描 → 结果树。
//! 主区域是标签页：默认"主列表"一个标签，点"文件扩展名分类"/"重复文件查找"
//! 会各自开一个新标签页。这两个分析标签页内部用 `ui::compact_tree` 渲染（和主列表
//! 视觉语言一致——能展开、有缩进参考线、右键菜单——但列不一样：数据先在 categorize.rs
//! 里重新组织成一棵"合成树"，按扩展名/大小分组当文件夹，真实文件当叶子）。

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

/// 主区域的一个标签页。"主列表"永远是第一个、不能关闭；
/// 扩展名分类/重复文件查找是按需打开的，数据（合成树）在打开的那一刻算一次，存在标签页里。
/// 每个分析标签页有自己独立的 `selected`（展开/选中状态），不会和主列表互相干扰。
///
/// `partition_idx: Option<usize>`——`Some(pi)` 表示只针对某一个分区（从
/// 右键那个分区的菜单进入），`None` 表示"全部分区一起看"（从顶部菜单进入）。
/// 两种入口对应不同的作用范围，是刻意区分开的：顶部菜单是"看整个电脑"的
/// 全局视角，右键某个分区是"就想看这一个盘"的局部视角，混在一起互相替代
/// 反而不灵活。
enum Tab {
    Main,
    Extensions { partition_idx: Option<usize>, title: String, root: Node, selected: Option<NodePath>, view: crate::ui::compact_tree::ViewState },
    Duplicates {
        partition_idx: Option<usize>, title: String, root: Node, selected: Option<NodePath>, view: crate::ui::compact_tree::ViewState,
        /// 后台线程还在跑内容哈希比对的时候是 `Some((阶段, done, total))`；
        /// 算完变成 `None`，这时候 `root` 才是真正算好的结果树。在那之前
        /// `root` 只是一棵空占位树（不是 `Option<Node>`，是为了让上面那几处
        /// `Tab::Extensions { root, .. } | Tab::Duplicates { root, .. }` 合并
        /// 匹配的地方不用跟着改类型），UI 层看到 `loading.is_some()` 就知道
        /// 该显示"正在比对内容…"的进度提示，而不是把这棵空树渲染成"一个重复
        /// 文件都没找到"。阶段（`dedup::HashPhase`）单独带着，不能省——两个
        /// 阶段（预筛/最终确认）的 done/total 是分开计数的，各自 0~各自的
        /// 100%，UI 上不分阶段直接展示一条进度会在切换阶段时看起来"卡在
        /// 100% 不动"或者"进度突然归零往回跳"，两种观感都会让人以为程序
        /// 卡死了。
        loading: Option<(crate::dedup::HashPhase, u64, u64)>,
    },
    /// "搜索"/"复制列表"标签页共用的形状：都是"整份克隆当前 `partitions`/
    /// `partition_infos`/`partition_root_paths`，独立于主列表往后各自变化"，
    /// 区别只在 `mode`——
    ///   - `Search`：画面永远是摊平的全部文件列表，输入内容实时过滤
    ///     （见 tree_list.rs 的搜索模式），定位是"翻遍全盘找某个文件"。
    ///   - `Copy`：画面和主列表一模一样的可展开树，可以浏览/选中/删除/建
    ///     符号链接/检测占用（都是对真实文件生效的操作）；但"重新扫描"/
    ///     "从列表移除"/"扩展名分类"/"重复文件查找"这几个跟"管理当前
    ///     活跃数据"绑定更紧，只挂在主列表上，在这里点了会提示、不会
    ///     静默没反应——定位是"独立的静态快照"，不是又一份可以主动管理的
    ///     "活"数据，定位是"对着同一份数据的独立副本各玩各的"——比如一个
    ///     标签页扫完 C 盘就不再动它，另一个标签页（主列表本身）继续扫
    ///     别的盘、删东西、重新扫描，两边互不影响，方便留一份"扫描当时的
    ///     快照"随时回看对比。
    ///
    /// 打开之后主列表继续扫描/删除/建符号链接，都不会影响这里，也不会
    /// 反过来被这里影响——这是有意的取舍：简单、可预期（"打开那一刻的
    /// 状态"），代价是数据可能随时间变旧，需要的话用户可以关掉重新开一个。
    ///
    /// "搜索"标签页：持有打开那一刻的主名字索引快照（`Arc` 克隆，纳秒级，
    /// 不再整树深拷贝）。索引完全自含渲染数据（见 search_index.rs），
    /// 主列表后续增删改都不影响这里——"打开那一刻的快照"语义与原来的
    /// 深拷贝版本完全一致。`index` 为 `None` 表示打开时主索引还在后台
    /// 分帧构建中，`poll_search_tab_index` 会在就绪后填入（多等一两秒的
    /// 极端场景：扫描刚完成就立刻点搜索）。
    SearchList { title: String, index: Option<Arc<NameIndex>>, selected: Option<NodePath>, list_state: tree_list::ListState },
    /// "复制列表"标签页：可操作的树快照（可展开/删除/建符号链接，语义
    /// 与原来的整树深拷贝版本一致）。树在后台线程从索引重建（纯读
    /// `Arc<NameIndex>`，无数据竞争，UI 零卡顿），完成前 `loading` 是
    /// `Some`、显示占位；`partition_infos`/`root_paths` 是每分区一条的
    /// 小数据，同步克隆（微秒级）。`index` 同样是打开那一刻的快照，
    /// 给本标签页的"查找"（Ctrl+F）用，其 `abs_path` 坐标与重建出来的
    /// 树严格一致。
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

/// "复制列表"标签页的树准备状态。
enum CopyLoading {
    /// 打开时主索引还没建好（扫描刚完成的极端场景），等主索引就绪。
    WaitIndex,
    /// 后台线程正在从索引重建树（几百毫秒，UI 全程零卡顿）。
    Building(Receiver<Vec<Node>>),
}

/// 右键菜单点了"删除到回收站"之后、用户在确认框里点"确定"之前的中间状态。
/// 单独存一份 name/full_path/is_folder（而不是等确认时再重新沿 abs_path 走一遍树）是
/// 因为确认框要立刻展示这些信息，且这段时间里树本身理论上可能变（虽然当前 UI 下
/// 用户点了确认框就基本被这个模态挡住了，操作不了别的，但直接存一份更稳妥、也省事）。
///
/// `source` 记的是这个删除请求是从哪棵树发起的：主列表（`self.partitions`）还是
/// 某个分析标签页自己的合成树（`Tab::Extensions`/`Tab::Duplicates` 里的 `root`）。
/// 这两棵树的节点是各自独立的 `Node` 拷贝（`categorize.rs` 建合成树时是克隆的，
/// 不是共享引用），所以"从哪棵树来的就摘哪棵树"，不能用同一份 abs_path 去两边都摘——
/// 下标含义完全不是一回事。已知的权衡：如果同一个文件在主列表和某个分析标签页里
/// 都能看到，从分析标签页删除后，主列表那边在下次重新扫描之前还会显示这个已经不存在
/// 的文件（磁盘上已经真删了，只是内存里那棵没同步更新）——分析标签页本来就是"打开那一刻
/// 拍的快照"，这个限制和它本来的语义是一致的。
struct PendingDelete {
    source: DeleteSource,
    abs_path: NodePath,
    name: String,
    full_path: String,
    is_folder: bool,
    /// 这一行来自索引摊平列表（"搜索"标签页）时是它的条目下标——删除
    /// 成功后用它把这一行从该标签页的显示里剔除（快照没有可摘的树）。
    /// 树模式发起的删除恒为 `None`。
    index_entry: Option<u32>,
}

#[derive(Clone, Copy)]
enum DeleteSource {
    Main,
    Tab(usize),
}

/// "创建符号链接"发起时记下来的上下文，等后台线程算完之后（`poll_symlink`）
/// 还要用它去更新树、拼状态提示——和 `PendingDelete`/`LockCheckRequest`
/// 是同一个模式。
struct SymlinkRequest {
    source: DeleteSource,
    abs_path: NodePath,
    name: String,
    /// 这一项在磁盘上的真实路径——单个文件/文件夹迁移成功后，要用这个路径
    /// 重新读一次磁盘状态、原地刷新树上的节点（见 `poll_symlink`），而不是
    /// 把它整个摘掉。重复文件组的场景不需要刷新单个节点（整个组直接摘掉），
    /// 这个字段留空即可。
    full_path: String,
    is_folder: bool,
}

/// 后台线程算完"创建符号链接"之后的结果。区分单个文件/文件夹和重复文件组，
/// 是因为两种情况给用户看的汇总消息不一样（组要报"共几份、成功几份"）。
enum SymlinkOutcome {
    Single { target_path: String },
    Group { target_path: String, member_count: usize, total_count: usize },
}

/// 底部状态条右侧的"操作结果"提示：删除/创建符号链接/导出 CSV/管理员重启失败……
/// 这些一次性的操作反馈之前全部借用 `scan_error` 在顶部菜单栏展示，跟"正在
/// 扫描"的转圈/进度数字挤在一起，而且不管是成功还是失败都用同一种红色文字，
/// 长路径的提示还会把菜单栏撑得很挤。现在单独开一个字段，固定显示在底部
/// 品牌条最右边，成功/失败用不同颜色区分，并且过一段时间自动消失、
/// 长文本按像素宽度截断——不会影响旁边的品牌文字，也不会把状态条撑高/撑变形。
/// `scan_error` 保留给"这次扫描本身失败了"这一种情况，继续显示在顶部
/// （紧挨着扫描进度，语义上更贴近"当前这次扫描操作的结果"）。
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

/// 底部状态提示自动消失前展示的时长。
const STATUS_MSG_TTL: std::time::Duration = std::time::Duration::from_secs(8);

/// 名字索引分帧构建的每帧预算：主索引 6ms / 分析视图索引 4ms。
/// 都远低于可感知阈值（<10ms）；空闲帧主索引给多一点点，扫描完成后
/// 一两秒内就能默默建完。以前是两个调用点各写一个裸 `from_millis(6/4)`。
const INDEX_STEP_BUDGET_MAIN: std::time::Duration = std::time::Duration::from_millis(6);
const INDEX_STEP_BUDGET_VIEW: std::time::Duration = std::time::Duration::from_millis(4);

/// 后台 CSV 导出线程回传的消息：进度（分区名+已写行数）与收尾汇总。
enum ExportMessage {
    Progress { partition: String, rows: u64 },
    Done { ok: usize, total: usize, dir: PathBuf },
}

/// "检测占用"的结果，`show_lock_check_modal` 展示它、有值就弹窗，`None` 就
/// 不弹——和 `pending_delete` 是同一套"有 Some 就弹窗"的模式。
struct LockCheckResult {
    /// 重新检测要用到的原始请求参数——点了"结束进程"/"停止服务"之后自动
    /// 重新查一遍，好让用户立刻看到"是不是已经解除占用了"，不用自己再手动
    /// 右键点一次"检测占用"。
    request: LockCheckRequest,
    name: String,
    is_folder: bool,
    /// 实际检查了多少个路径（文件本身是 1；文件夹是收集到的子孙文件数，
    /// 受 `LOCK_CHECK_LIMIT` 限制，不一定等于文件夹里的真实文件总数）。
    /// 预留字段：结果窗口后续展示"检查覆盖数"时直接可用。
    #[allow(dead_code)]
    checked_count: usize,
    /// 文件夹里的文件数超过了检查上限，只查了前面一部分——结果窗口要如实
    /// 告诉用户"没查全"，不能让人误以为"查过了、没事"就真的没事。
    /// 预留字段：结果窗口后续展示这个标记时直接可用。
    #[allow(dead_code)]
    truncated: bool,
    procs: Vec<crate::file_ops::LockingProcess>,
    error: Option<String>,
    /// 点了"结束进程"/"停止服务"之后，操作本身的结果提示（成功/失败），
    /// 展示在结果列表上方，和"重新检测出来的最新占用列表"分开——操作反馈
    /// 和查询结果是两回事，操作即使失败了，下面的列表也应该是刷新过的。
    action_feedback: Option<String>,
    /// 真正调用 Restart Manager 查询的那一步在后台线程跑，这期间是
    /// `true`——结果窗口先弹出来、显示"检测中…"，不会让用户觉得点了没反应，
    /// 也不会卡住整个界面（之前是同步查的，路径一多——尤其是大文件夹——
    /// 卡顿甚至像死机一样明显）。
    loading: bool,
    /// 文件夹专属的"重命名探测"结论（文件不适用，恒为 `None`）——比
    /// "查文件夹里最多 2000 个文件"准得多，见 `file_ops::check_folder_occupied_by_rename`
    /// 上的说明。这是比下面 `procs` 列表更可靠的"占用与否"结论，结果窗口
    /// 优先展示它，`procs` 列表作为"尽力找出是谁占用的"补充信息。
    rename_probe: Option<crate::file_ops::FolderOccupancy>,
}

/// 触发一次"检测占用"需要的全部上下文，`execute_pending_delete`/首次检测/
/// 结束进程或服务后的自动刷新，都是同一份逻辑（`run_lock_check`）在跑，
/// 存这么一份方便重复调用，不用每次都重新拆一遍 `TreeAction`。
#[derive(Clone)]
struct LockCheckRequest {
    tab_idx: usize,
    /// 这个请求是从"主列表"还是"分析视图标签页"发出的——现在统一的
    /// 占用检测流程已经不区分来源了（都走真实磁盘路径），字段保留备用。
    #[allow(dead_code)]
    is_view_tab: bool,
    abs_path: NodePath,
    name: String,
    full_path: String,
    is_folder: bool,
    /// 是不是"整组重复文件一起检测占用"这种请求——是的话 `abs_path` 指向
    /// 分组节点本身（不是具体文件/文件夹），要查的路径是组里每一个文件的
    /// 路径（分组的子节点本来就都是文件，不需要像文件夹那样递归收集），
    /// 也不做重命名探测（分组不对应磁盘上的一个真实位置）。见
    /// `run_lock_check_with_feedback` 里的分支。
    is_group: bool,
}

/// "检测占用"后台任务的完整类型：`(请求上下文, 结果通道)`。
type LockCheckJob = (LockCheckPending, Receiver<Result<Vec<crate::file_ops::LockingProcess>, String>>);

/// 后台线程算"检测占用"结果期间保留的上下文——结果回来了（`poll_lock_check`）
/// 要用这些拼出完整的 `LockCheckResult`。
struct LockCheckPending {
    request: LockCheckRequest,
    action_feedback: Option<String>,
    is_folder: bool,
    checked_count: usize,
    truncated: bool,
    rename_probe: Option<crate::file_ops::FolderOccupancy>,
}

/// "具体检测"（按层排查文件夹占用）的状态——见 `start_layered_probe` 上的
/// 说明：不是一次性把文件夹里最多 2000 个文件全丢给 Restart Manager，而是
/// 一层一层来，子文件夹先用重命名探测过一遍，能重命名成功的直接判定
/// "没问题"、不用再往下看，只有重命名失败的才会真的进入下一层继续排查。
///
/// 直接操作磁盘上的真实路径（`std::fs::read_dir`/重命名），不依赖内存里
/// 已经扫描好的树——占用检测本来就该看"这一刻磁盘上的实时状态"，用可能
/// 已经过时的扫描结果反而不准；这样也不用为了这一个功能纠结"这个路径
/// 到底属于主列表的树、还是某个标签页自己克隆的树"这种归属问题。
struct LayeredProbe {
    root_full_path: String,
    /// 根节点展示名（预留字段：界面展示都用 `root_full_path`，这个字段备用）。
    #[allow(dead_code)]
    root_name: String,
    /// 还没处理的文件夹（下一次推进要展开的对象）；第 0 层就是
    /// `[root_full_path]` 自己。
    frontier: Vec<String>,
    /// 已经完整跑完的层数，用于界面上"已检测第 N 层"的提示。
    layer: usize,
    /// 到目前为止全部层汇总的占用进程/服务，按 PID 去重。
    procs: Vec<crate::file_ops::LockingProcess>,
    /// 已经查到底了（`frontier` 空、没有更多文件夹需要往下查）。
    exhausted: bool,
    /// 这一层"检查直属文件"的后台任务——文件不能靠重命名判断占用（重命名
    /// 成功不代表内容没被只读打开），这一步还是要走 Restart Manager；
    /// 子文件夹之间的重命名探测很快，同步做完，不需要异步。
    pending: Option<LayeredProbePending>,
    /// 最近完整跑完的一层的统计，用于展示反馈。
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

/// "查找"功能用：在"主列表形状"的树（`Vec<Node>`，每个顶层元素是一个分区/
/// 快照根，`abs_path[0]` 是分区下标）里按名称做一次大小写不敏感的包含匹配，
/// 收集全部命中，按树的自然先序遍历顺序排列（"上一个/下一个"跳起来才符合
/// 直觉）。用显式栈做深度优先遍历，不用原生递归调用栈，避免目录嵌套极深时
/// 的栈溢出风险——和这个项目里其它遍历大树的地方一个风格。
///
/// 跳过分区/磁盘自身（`path.len() > 1` 才算命中）——找"C:"这种盘符本身不是
/// 用户想要的，表项定位的意义也不大。
fn collect_find_matches_main_shaped(partitions: &[Node], matcher: &crate::ui::Matcher) -> Vec<NodePath> {
    let mut out = Vec::new();
    for (pi, root) in partitions.iter().enumerate() {
        // 栈里存的是"还没访问、待处理"的节点；每次弹出一个节点就处理它、
        // 再把它的子节点按原始顺序反着压回去——这样弹出的顺序就是标准的
        // 先序遍历（父节点先于子节点，同一层按原始下标从左到右）。
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

/// "查找"功能用：在"分析视图形状"的合成树（单一根，`abs_path[0]` 恒为 0，
/// 只有分组→文件两层）里收集匹配项，顺序是"先看这一组本身的名字匹配不
/// 匹配，再看组里每个文件"，和 compact_tree.rs 正常显示的顺序一致。
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

/// 索引直查的公共收尾：条目下标（给"搜索"标签页的滚动定位做整数比对用）
/// 和绝对坐标（给展开/选中用）都要产出，两条列表都是先序序，与展示一致。
///
/// 返回 [`FindMatches::Entries`]：**只存条目下标 + 生成它们的索引快照**，
/// 坐标（abs_path）由 FindState 在"真正要跳转到某一项"时逐条回溯。
/// 以前这里对每个匹配调一次 `abs_path_of`（每次一个 Vec 堆分配），命中
/// 50 万个文件就是 50 万次分配——`*.pi` 这类宽通配符下查找悬浮窗卡顿的
/// "收集环节"元凶（v3 修掉了定位环节，这一个是同类问题）。
fn collect_find_matches_index(
    idx: &Arc<crate::search_index::NameIndex>,
    matcher: &crate::ui::Matcher,
) -> FindMatches {
    let entries = matcher.find_in_index(idx);
    FindMatches::Entries { entries, index: Arc::clone(idx) }
}

/// "查找"的匹配结果：见 [`collect_find_matches_index`] 的说明。
enum FindMatches {
    Entries { entries: Vec<u32>, index: Arc<crate::search_index::NameIndex> },
    /// 同步遍历兑底（索引还没就绪的头几秒才有）：直接带路径，量级有限。
    Paths(Vec<NodePath>),
}

/// "查找"命中之后，在"主列表形状"的树里展开路径上的每一层祖先、选中目标、
/// 并请求下一帧把它滚动到可视区域——主列表（`self.partitions`）和"搜索"
/// 标签页自己克隆的那份快照用的是同一种树形状，共用这份逻辑。
fn reveal_in_main_shaped_tree(
    partitions: &mut [Node],
    list_state: &mut tree_list::ListState,
    selected: &mut Option<NodePath>,
    path: NodePath,
) {
    if let Some(&pi) = path.first()
        && let Some(part) = partitions.get_mut(pi) {
            // 记录这一趟有没有真的改变展开状态——只有真的变了才 bump
            // expand_version 让列表缓存重建。"查找"在已经展开的路径之间
            // 跳来跳去时不改缓存版本，一次定位就是纯查表，不多付一次
            // "重走可见树"的成本。
            let mut expanded_changed = !part.expanded;
            part.expanded = true;
            // 逐层展开路径上的每一层祖先文件夹（不含目标本身——目标要是文件，
            // 展开它没有意义；要是文件夹，定位到它本身就够了）。只在"这一层
            // 还没展开"时才调用 `toggle_expand`——那是个真正的"切换"，对已经
            // 展开的节点再调一次会把它连同子树一起收起来，这里只想要"确保
            // 展开"，不是"切换"。现在树支持同时展开多个分支，展开某一层祖先
            // 不会影响其它已经展开的分支。
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

/// "查找"命中之后，在"分析视图形状"的合成树（单一根）里展开+选中+请求滚动。
fn reveal_in_view_tree(
    root: &mut Node,
    view: &mut crate::ui::compact_tree::ViewState,
    selected: &mut Option<NodePath>,
    path: NodePath,
) {
    // 只在真的改变了展开状态时才 bump expand_version（道理同
    // `reveal_in_main_shaped_tree`：不改变就不用重建行缓存）。
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

pub struct DiskUiApp {
    partitions: Vec<Node>,
    partition_infos: Vec<Option<DiskInfo>>,
    /// 分类统计缓存：扫描完成时算一次存起来，不在每一帧里现算——分类统计要遍历
    /// 整棵树，几十万个文件的情况下每帧都重算一遍是明显能感觉到卡顿的（如果界面按
    /// 60fps 刷新，就是一秒钟内把整棵树重新遍历 60 次），缓存下来后侧边栏只是读一个
    /// 现成的 Vec，不用每帧都现算。
    partition_categories: Vec<Vec<CategoryStat>>,
    /// 和 `partitions`/`partition_infos` 一一对应：这个分区/目录当初是从哪个路径扫的，
    /// 右键菜单拼完整路径、打开扩展名/重复文件分析都要用到。
    partition_root_paths: Vec<String>,
    selected: Option<NodePath>,

    tabs: Vec<Tab>,
    active_tab: usize,

    scanning: bool,
    scanned_count: u64,
    scan_error: Option<String>,
    scan_rx: Option<Receiver<ScanMessage>>,
    /// 一次选了多个分区/目录时，排队按顺序一个个扫，扫完一个再扫下一个。
    scan_queue: VecDeque<PathBuf>,
    current_scan_path: Option<PathBuf>,
    /// 当前这次扫描是不是"重新扫描"某个已有分区（`Some(pi)`）——是的话，扫描
    /// 结果要原地替换 `self.partitions[pi]`，而不是像"新增扫描"那样追加到
    /// 末尾。见 `start_rescan`/`poll_scan`。
    rescan_target: Option<usize>,

    /// 选择分区/目录的弹窗。启动时就是 `Some(..)`（软件一打开就弹出来选），
    /// 背景仍然是（空的）主界面；"文件 > 添加扫描…"也是把这个重新置为 `Some`。
    picker: Option<startup::PickerState>,

    /// 视图 > 显示全部信息：开=全部列 + 元数据文件；关=只留关键列、隐藏元数据文件。
    show_all_details: bool,

    /// 主列表当前的排序列 + 方向 + 展开版本号 + 可见行缓存，点表头改；
    /// 默认和构建时的排序规则一致（按逻辑大小降序），不点表头的话行为和以前完全一样。
    list_state: tree_list::ListState,

    /// 右键菜单"删除到回收站"点了之后、确认框点"确定"/"取消"之前的等待状态；
    /// `None` 时不显示确认框。
    pending_delete: Option<PendingDelete>,

    /// 正在后台跑"重复文件查找"内容哈希比对的分区，`(分区下标, 结果通道)`。
    /// 一个 `Vec` 是因为可能同时有好几个分区的比对在并行跑（用户开了多个
    /// 重复文件标签页）；每帧在 `poll_duplicate_scan` 里收一遍。
    duplicate_rx: Vec<(Option<usize>, Receiver<categorize::DuplicateMessage>)>,

    /// 真正在后台线程执行"删除到回收站"（含占用重试）期间，保留一份
    /// `PendingDelete`（等结果回来了还要用它去更新树）+ 结果通道。
    /// 只会同时有一个在跑——确认框是模态的，没删完之前弹不出第二个。
    delete_rx: Option<(PendingDelete, Receiver<Result<(), String>>)>,

    /// 右键菜单"检测占用"的结果，`Some` 就弹窗展示，见 `show_lock_check_modal`。
    lock_check_result: Option<LockCheckResult>,

    /// 正在后台线程执行"创建符号链接"的请求 + 结果通道。和 `delete_rx` 一样
    /// 只会同时有一个——弹原生文件夹选择框是模态的，没选完/没执行完之前
    /// 发不出第二个请求。
    symlink_rx: Option<(SymlinkRequest, Receiver<Result<SymlinkOutcome, String>>)>,

    /// 后台 CSV 导出线程的进度/结果通道（`export_csv` 发起，`poll_export`
    /// 每帧收）。同一时间最多一个导出在跑——再点导出会再起一个线程，但
    /// 状态条只展示最后一个的结果；CSV 写文件互不冲突（文件名带分区序号）。
    export_rx: Option<Receiver<ExportMessage>>,

    /// 底部状态条右侧展示的一次性操作反馈（删除/符号链接/导出/管理员重启……）。
    /// 见 `StatusMsg` 上的说明。
    status_message: Option<StatusMsg>,

    /// "创建符号链接"发起之后，等用户在弹窗里选好目标分区才真正开始——
    /// 见 `show_symlink_target_picker_modal`。`None` 表示当前没有正在选的。
    pending_symlink_pick: Option<PendingSymlinkKind>,
    /// 弹窗打开那一刻缓存下来的固定分区列表（盘符 + 卷标）。以前是弹窗开着
    /// 的每一帧都重新调一遍 Win32 枚举（GetLogicalDriveStringsW +
    /// GetVolumeInformationW，每个盘一次），纯浪费——盘符列表在弹窗存在的
    /// 几秒钟内不会变，打开时查一次就够了。
    symlink_pick_drives: Vec<(char, Option<String>)>,

    /// "检测占用"改成非阻塞之后的后台线程状态：请求上下文 + 结果通道。
    /// 见 `run_lock_check`/`poll_lock_check` 上的说明。
    lock_check_rx: Option<LockCheckJob>,

    /// "具体检测"（按层排查）的状态，`None` 表示当前没有在跑/没打开。
    layered_probe: Option<LayeredProbe>,

    /// "查找"悬浮窗（菜单"查找 → 查找…"或 Ctrl+F）的窗口级状态：开/关与
    /// "刚打开要把键盘焦点给输入框"。这两个属于悬浮窗本身，不属于任何一个
    /// 标签页；每个标签页各自的查询词/匹配结果/定位在下面的 `finds` 里。
    find_open: bool,
    find_focus_requested: bool,
    /// 每个标签页各自的"查找"状态（key = 标签页在 `tabs` 里的下标，缺省 =
    /// 空白状态）。查找不改变任何列表的内容，只是在当前这个标签页原来的树
    /// 里，把匹配到名字的某一项展开+选中+滚动过去，"上一个/下一个"在匹配
    /// 结果之间循环跳。关键词按标签页隔离：在 A 列表搜 `*.mp4`、切到 B
    /// 列表搜别的，来回切换各自的关键词都还在，不用反复重新手打；新开的
    /// 列表从空白开始，不会带着上一个列表的关键词。关闭标签页时对应状态
    /// 随之回收、后面的下标整体前移一位（见 `show_main_screen` 关标签页的
    /// 分支）。
    finds: std::collections::BTreeMap<usize, FindState>,

    // ── 名字索引（search_index.rs）——查找/搜索共用的核心数据 ──
    /// 主列表树的名字索引（就绪后是 `Some`），查找（主列表）和搜索标签页
    /// 共用它。完全自含数据，放进 `Arc` 共享给各标签页当快照。
    main_index: Option<Arc<NameIndex>>,
    /// 分帧构建中的主索引构建器；就绪后为 `None`。
    main_index_builder: Option<IndexBuilder>,
    /// 主列表树的结构版本号：扫描完成（追加/原地替换）、从列表移除、
    /// 删除节点、符号链接原地刷新——所有会让已有节点内存地址/下标语义
    /// 变化的操作都 +1。展开/折叠不算（只翻 bool，不动内存布局）。
    /// 索引构建器/消费者用它判断"手里这份索引对应的是不是当前这棵树"。
    main_tree_version: u64,

    /// "关于 DiskForge"悬浮窗（版权/许可/赞助）的状态，`None` = 关闭。
    /// 首次启动自动弹出（带 6 秒按钮倒计时，且受"不再提醒"持久化影响）；
    /// "关于"菜单随时可打开（无倒计时、不受持久化影响）。
    about: Option<AboutState>,
    /// "关于"悬浮窗用的三张内置图片纹理（logo + 微信/支付宝赞助码），
    /// main.rs 从 include_bytes! 的 PNG 解码后传入（全部编译进 exe，单文件）。
    about_textures: AboutTextures,
}

/// "关于"悬浮窗的内置图片纹理（main.rs 里从编译进 exe 的 PNG 解码注册）。
pub struct AboutTextures {
    pub logo: egui::TextureHandle,
    pub wechat: egui::TextureHandle,
    pub alipay: egui::TextureHandle,
}

/// 赞助码标签：默认显示微信收款码，点"支付宝"切换到另一张。
#[derive(Clone, Copy, PartialEq, Eq)]
enum SponsorTab {
    WeChat,
    Alipay,
}

/// "关于 DiskForge"悬浮窗状态。首次启动（自动弹、"不再提醒"按钮带 6 秒
/// 倒计时）和"关于"菜单（用户主动看、无倒计时）复用同一个悬浮窗，两种
/// 来源只差 `countdown` 初始值。窗口没有关闭按钮，只能靠底部两个按钮关闭：
/// "下次一定"（仅关窗，下次启动还会弹，随时可点）和"不再提醒"（写入持久化
/// 标记，之后启动不再自动弹；"关于"菜单入口不受影响，始终能打开）。
struct AboutState {
    tab: SponsorTab,
    /// "不再提醒"按钮的倒计时剩余秒数：`Some` 表示还在倒计时（只有这个按钮
    /// 禁用，剩余秒数直接显示在按钮文字上；"下次一定"不受影响，随时可点
    /// ——用户想先走也没什么拦着的）；`None` 表示按钮已可点击。菜单入口恒为 None。
    countdown: Option<f32>,
    /// 三条"组合行"（logo 行 / 微信支付宝标签行 / 底部按钮行）上一帧量出的
    /// 自然内容宽度。egui 对顺序摆放的行没有可靠的"整行左右居中"原语
    /// （水平主轴的对齐只在 main_wrap 换行时生效，行定位实测不稳定），
    /// 所以用"行前留白 = (可用宽 − 上一帧行宽)/2"来居中：首帧宽度未知
    /// 贴左（与旧版一致），第二帧起精确居中；窗口带淡入动画且内容静态，
    /// 这一帧差肉眼不可见。
    logo_row_w: f32,
    tabs_row_w: f32,
    btns_row_w: f32,
}

impl AboutState {
    /// 首次启动自动弹出：6 秒倒计时后才允许点按钮。
    fn first_launch() -> Self {
        Self {
            tab: SponsorTab::WeChat,
            countdown: Some(ABOUT_COUNTDOWN_SECS),
            logo_row_w: 0.0,
            tabs_row_w: 0.0,
            btns_row_w: 0.0,
        }
    }
    /// "关于"菜单打开：无倒计时，按钮立即可点（用户是主动来查看信息的，
    /// 不存在"误点划过重要提示"的问题；也不受"不再提醒"持久化影响）。
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

/// 首次启动赞助悬浮窗的按钮倒计时时长（秒）。
const ABOUT_COUNTDOWN_SECS: f32 = 6.0;

/// "创建符号链接"点了之后、真正选好目标分区之前的中间状态——单个文件/
/// 文件夹和重复文件组要记的上下文不一样，用枚举区分，两边共用同一个
/// 目标分区选择弹窗（`show_symlink_target_picker_modal`）。
enum PendingSymlinkKind {
    Single { source: DeleteSource, abs_path: NodePath, name: String, full_path: String, is_folder: bool },
    Group { tab_idx: usize, abs_path: NodePath, name: String, member_paths: Vec<String> },
}

/// 一个标签页自己的“查找”状态。每个标签页一份（见 `DiskUiApp::finds`），
/// 关键词、匹配结果、当前定位都互不共享：在 A 列表搜 `*.mp4`、切到 B 列表
/// 搜 `报告`，来回切换各自的关键词都还在，不用反复重新手打；新开一个
/// 列表则从空白开始，不会带着上一个列表的关键词。
#[derive(Default)]
struct FindState {
    query: String,
    /// 当前标签页里，按树的自然先后顺序收集到的匹配项路径。
    matches: Vec<NodePath>,
    /// 与 `matches` 一一对应的索引条目下标——匹配是从"当前标签页的名字
    /// 索引"直查拿到的时候才填充（退回同步遍历的兑底路径没有条目下标，
    /// 保持为空）。"搜索"标签页定位滚动时用它做整数比对，避免百万行
    /// 视图逐行回溯坐标（每行一次堆分配，一次定位就是几百毫秒的卡顿，
    /// 是"搜索"页上用查找悬浮窗一打字就卡的元凶，见
    /// `reveal_find_target` 与 tree_list.rs 的 `pending_scroll_entry`）。
    /// 非空时 `matches` 恒为空表（坐标按需从 `match_index` 回溯，不为每个
    /// 匹配提前分配），总数用 [`FindState::match_count`] 取。
    match_entries: Vec<u32>,
    /// 生成 `match_entries` 的那份索引快照——坐标按需回溯的依据。快照语义：
    /// 匹配是哪份索引算出来的，坐标就永远是那份索引的坐标，与标签页展示的
    /// 内容严格一致。
    match_index: Option<Arc<NameIndex>>,
    /// `matches` 里当前定位到第几项；`matches` 为空时这个值没有意义。
    cursor: usize,
    /// 上一次算 `matches` 用的是哪个查询词、哪个标签页——查询词或者标签页
    /// 变了才重新扫一遍，不然每帧都要遍历一次当前标签页的树。
    computed_for: Option<(String, usize)>,
    /// 最近一次编辑 `query` 发生的时间——和"搜索"标签页同一套节流防抖
    /// （见 tree_list.rs 的 `SEARCH_DEBOUNCE`）：连续敲字的这段时间里不去
    /// 重新扫树，停下来一小会儿才真正查一次，不然大盘上每敲一个字都要
    /// 遍历一遍当前标签页的树，容易感觉卡。
    query_changed_at: Option<std::time::Instant>,
}

impl FindState {
    /// 当前匹配总数：索引路径看条目表，兑底路径看路径表（二者互斥，必有一个为空）。
    fn match_count(&self) -> usize {
        if self.match_entries.is_empty() { self.matches.len() } else { self.match_entries.len() }
    }
    /// 第 cursor 项的树坐标：索引路径按需回溯一次（一次一个 Vec 分配，只发生在
    /// 真正跳转的那一项上）；兑底路径直接取预收集的路径。
    fn path_of(&self, cursor: usize) -> Option<NodePath> {
        if let Some(idx) = &self.match_index {
            self.match_entries.get(cursor).map(|&e| idx.abs_path_of(e))
        } else {
            self.matches.get(cursor).cloned()
        }
    }
}

impl DiskUiApp {
    /// 构造应用实例。`textures` 是 main.rs 里从内置 PNG 解码出的"关于"悬浮窗
    /// 图片纹理；首启是否弹赞助悬浮窗由持久化标记（"不再提醒"）决定。
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

impl eframe::App for DiskUiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().set_visuals(egui::Visuals::dark());
        self.poll_scan();
        self.poll_duplicate_scan();
        self.poll_delete();
        self.poll_symlink();
        self.poll_export();
        self.poll_lock_check();
        self.poll_layered_probe();
        // 名字索引的后台维护：主索引分帧构建（扫描完成后 1~2 秒内默默完成，
        // 用户无感）+ 活跃分析标签页的索引 + 快照标签页的等待/收尾。
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

        // 扫描中：进度数字和转圈动画要持续刷新，这时候需要强制重绘。
        // 空闲时不再无条件 request_repaint()：那样等于强制 egui 一直按屏幕刷新率
        // （通常 60Hz）重绘，不管界面有没有变化都要重新布局一遍，是 egui 官方
        // GitHub 讨论区里明确点出来的"不必要 CPU 占用"反模式，鼠标移动/点击/
        // 菜单展开这些交互 egui 自己就会触发重绘，不需要每帧手动催一次。
        // 索引/快照的准备状态（搜索标签页等主索引、复制列表等后台重建树、
        // 搜索结果的后台排序）也需要持续重绘才能就位后自动刷新出来。
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

impl DiskUiApp {
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
                // 菜单入口始终能打开（不受"不再提醒"持久化影响），且不带倒计时。
                self.about = Some(AboutState::from_menu());
            }
            #[cfg(windows)]
            TopbarAction::RestartAsAdmin => self.restart_as_admin(),
            TopbarAction::None => {}
        }

        // Ctrl+F 是"查找"的标准快捷键，菜单里点得到，但键盘直接按更顺手——
        // 和记事本/浏览器的习惯保持一致。
        if ui.ctx().input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::F)) && !self.partitions.is_empty() {
            self.find_open = true;
            self.find_focus_requested = true;
        }

        self.show_branding_bar(ui);
        self.show_tab_bar(ui);

        // 弹窗打开的时候，背景内容（侧边栏 + 主区域）整体禁用，提示用户先处理弹窗——
        // 但仍然可见，不是替换成另一个界面。
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
                                // 索引快照就位：索引摊平列表 + 搜索框（tree_list 内部
                                // 走 memmem SIMD 直查，无任何分帧/进度环节）。
                                Some(idx) => tree_list::show(
                                    ui,
                                    tree_list::ListSource::Indexed { index: idx },
                                    selected,
                                    true,
                                    list_state,
                                ),
                                // 极端场景（扫描刚完成就点搜索）：主索引还在分帧
                                // 构建，显示占位，`poll_snapshot_tabs` 就位后自动切换。
                                None => {
                                    show_snapshot_preparing(ui, "正在准备搜索索引…");
                                    TreeAction::None
                                }
                            }
                        }
                        Some(Tab::CopyList { tree, partition_infos, root_paths, selected, list_state, loading, .. }) => {
                            match loading {
                                // 树在后台线程重建中（几百毫秒，UI 零卡顿），占位等一下。
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

    /// 窗口左下角的小品牌条：软件名 + 开发者；右边额外展示一条最近的操作状态
    /// 提示（删除/符号链接/导出……），见 `StatusMsg` 上的说明。
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

                    // 靠右展示状态提示，长文本按可用宽度截断（不会撑开/挤压左边的品牌文字）。
                    // 用 `with_layout(right_to_left)` 嵌在同一个 `horizontal` 里占满剩余空间——
                    // 和 topbar.rs 里"扫描中"提示放在菜单栏右边是同一个写法。
                    if let Some(status) = &self.status_message {
                        let elapsed = status.set_at.elapsed();
                        if elapsed < STATUS_MSG_TTL {
                            // 到期前只请求一次"定时重绘"，不是无条件每帧重绘——到点了状态
                            // 提示才会准时消失，空闲时不会额外占 CPU。
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

    /// "查找"悬浮窗：菜单"查找 → 查找…"或 Ctrl+F 打开。类似记事本/浏览器的
    /// 查找——不改变当前标签页显示的内容，只是在原来的树里把匹配到名字的
    /// 某一项展开+选中+滚动过去，"上一个/下一个"在匹配结果之间循环跳。
    /// 和"搜索"标签页是两个独立的功能，见 `FindState` 上的说明。
    /// 查询词/匹配结果按标签页各存一份（`finds`）：悬浮窗是同一个，但切到
    /// 哪个标签页就显示/操作哪个标签页自己的关键词，互不串。
    fn show_find_window(&mut self, ctx: &egui::Context) {
        if !self.find_open {
            return;
        }
        let mut window_open = true;
        let mut go_next = false;
        let mut go_prev = false;
        // Escape 只归"当前最上层的 UI"处理：任何一个模态弹窗开着的时候（选分区/
        // 删除确认/符号链接选分区/占用结果/分层排查），查找窗不能抢 Esc——以前
        // 是查找窗无条件下全局吞掉 Esc，删除确认框开着时按 Esc 会把查找窗一起
        // 关掉，而用户按 Esc 显然是想关掉眼前的模态框。弹窗各自的 Esc 语义
        // 由各自的展示函数自己处理（如删除确认框：Esc = 取消）。
        let modal_open = self.picker.is_some()
            || self.pending_delete.is_some()
            || self.pending_symlink_pick.is_some()
            || self.lock_check_result.is_some()
            || self.layered_probe.is_some()
            // "关于"悬浮窗没有关闭按钮，只能点里面的按钮关——Esc 也不能替它关，
            // 所以它开着时 Esc 交给它（什么都不做），查找窗不能抢。
            || self.about.is_some();
        let esc_pressed = !modal_open && ctx.input(|i| i.key_pressed(egui::Key::Escape));
        // 记事本/浏览器那种查找条的样子：一行装下输入框 + 上一个/下一个 +
        // 计数，紧凑不占地方；不用 `.anchor()` 固定位置——那个会让窗口每帧
        // 都被强制归位，等于不能拖动，只给一个 `.default_pos()` 当作第一次
        // 打开时的初始位置，之后想拖到哪都行，egui 会自己记住拖动后的位置。
        egui::Window::new("🔎 查找")
            .id(egui::Id::new("find_window"))
            .collapsible(false)
            .resizable(false)
            .title_bar(true)
            .open(&mut window_open)
            // egui 0.35 把原来的 screen_rect 概念改成了 content_rect()（0.35 版本发布
            // 说明里明确写了"Update all usages of screen_rect to content_rect"），
            // 直接调用即可拿到当前可用区域，用来把窗口默认摆在右上角附近。
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
                    // 记事本/浏览器的习惯：回车 = 下一个，Shift+回车 = 上一个。
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
                    // 必须用 match_count()（索引路径的命中在 match_entries 里），
                    // 不能看 matches——索引直查成功时 matches 恒为空表，看它
                    // 会永远显示"没有匹配项"（实际命中正常、跳转正常）。
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

        // 查询词变了才重新扫一遍——不然每帧都要遍历一次当前标签页的树，
        // 大盘上会很卡。加了一层和"搜索"标签页同款的节流防抖：连续敲字的
        // 这段时间里先不查，等停下来一小会儿（`FIND_DEBOUNCE`）才真正扫一次，
        // 不然之前是"每敲一个字都立刻查一遍"，大标签页上敲字会有肉眼可见
        // 的卡顿。这里特意不转小写——`Matcher::build_auto` 自己会处理大小写
        // 不敏感的匹配，转小写只是给"缓存 key 判断有没有变"用，无所谓大小写。
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
        // computed_for 的 key 带上标签页下标是防御性的：状态本身按标签页隔离
        // （见 `finds`），正常情况下同一份状态只会服务同一个标签页；带上之后
        // 即使将来有哪条路径忘了隔离，最多也是多算一遍，不会拿 A 页的匹配
        // 结果去 B 页上跳。
        let key = (query.to_lowercase(), self.active_tab);
        let already_computed = self.find_state().computed_for.as_ref() == Some(&key);
        if debounce_ready && !already_computed {
            let found = if query.is_empty() {
                FindMatches::Paths(Vec::new())
            } else {
                self.collect_find_matches(&query)
            };
            // 索引路径只存条目下标 + 索引快照（50 万命中 = 50 万次 abs_path_of
            // 堆分配的时代结束了）；坐标在真正跳转某一项时才回溯那一条。
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

    /// 当前活跃标签页自己的"查找"状态（还没有就用空白状态占一个位）。
    /// 关键词按标签页隔离的理由见 `finds` 字段上的说明。
    fn find_state(&mut self) -> &mut FindState {
        self.finds.entry(self.active_tab).or_default()
    }

    /// 在当前激活的标签页里，按名称收集匹配项，顺序是树的自然先序遍历
    /// （每一层按原来的下标顺序），这样"上一个/下一个"跳起来符合直觉，
    /// 不是随机跳。
    ///
    /// 返回值见 [`FindMatches`]：索引直查只带条目下标 + 索引快照（坐标按需
    /// 回溯），同步遍历兑底才带整份路径表。
    ///
    /// 数据源优先走当前标签页的名字索引（主索引/标签页快照索引/合成树
    /// 索引，`Matcher::find_in_index`——memmem SIMD 或逐名并行，几毫秒级，
    /// 不再一帧内同步遍历整棵树）；索引还没就绪时退回旧的全树遍历兑底
    /// （只发生在扫描刚完成后的一两秒内，行为与重构前一致）。
    fn collect_find_matches(&self, query: &str) -> FindMatches {
        let matcher = crate::ui::Matcher::build_auto(query);
        match self.tabs.get(self.active_tab) {
            Some(Tab::SearchList { index: Some(idx), .. }) | Some(Tab::CopyList { index: Some(idx), .. }) => {
                // 快照索引自含全部数据，它的 abs_path 坐标即"打开那一刻"
                // 的树坐标，与标签页展示的内容严格一致。
                collect_find_matches_index(idx, &matcher)
            }
            Some(Tab::SearchList { .. }) | Some(Tab::CopyList { .. }) => {
                // 索引还没就绪（标签页还在占位状态），没有可查的东西。
                FindMatches::Paths(Vec::new())
            }
            Some(Tab::Extensions { root, view, .. }) | Some(Tab::Duplicates { root, view, .. }) => {
                if let Some(idx) = &view.index {
                    collect_find_matches_index(idx, &matcher)
                } else {
                    // 合成树索引还没建好（刚打开标签页的头几秒，索引在分帧
                    // 构建中），退回同步遍历兑底；索引就绪后走上面的快路。
                    FindMatches::Paths(collect_find_matches_view(root, &matcher))
                }
            }
            _ => {
                if let Some(idx) = self.main_index.as_ref().filter(|i| i.struct_version == self.main_tree_version) {
                    collect_find_matches_index(idx, &matcher)
                } else {
                    // 主索引还没建好/刚过期，退回旧的全树遍历兑底（与重构前
                    // 行为一致，只是索引就绪后的常态已经快了两个数量级）。
                    FindMatches::Paths(collect_find_matches_main_shaped(&self.partitions, &matcher))
                }
            }
        }
    }

    /// 把"查找"命中的某一项展开+选中，并请求下一帧把它滚动到可视区域
    /// （`pending_scroll`，见 tree_list.rs/compact_tree.rs 里的说明）。
    ///
    /// `index_entry`：匹配项在"当前标签页名字索引"里的条目下标——只在索引
    /// 直查路径有值。"搜索"标签页定位滚动时优先用它做整数比对
    /// （`pending_scroll_entry`），没有时退回零分配的坐标比对
    /// （`NameIndex::abs_path_eq`）。原来在摊平视图里逐行调
    /// `abs_path_of`（每行一次堆分配）比对，百万行视图一次定位就是几百
    /// 毫秒，是"搜索"页上用查找悬浮窗一打字就卡的元凶。
    fn reveal_find_target(&mut self, path: NodePath, index_entry: Option<u32>) {
        match self.tabs.get_mut(self.active_tab) {
            // "复制列表"的树快照和主列表同形状，展开+选中+滚动逻辑完全共用。
            Some(Tab::CopyList { tree, selected, list_state, .. }) => {
                reveal_in_main_shaped_tree(tree, list_state, selected, path);
            }
            // "搜索"标签页是索引摊平列表：没有"展开祖先"这回事，直接选中
            // + 请求滚动（摊平行按条目下标/坐标对上滚动目标）。
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

    /// 标签栏：只有"主列表"一个标签时不画，省地方（和之前单标签页的观感一致）。
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
            // 每个标签页各自的"查找"状态跟着下标走：被关掉的那个直接丢弃；
            // 它后面的整体前移一位重新对号——不这么做的话，"标签页 3 的关键
            // 词"在 2 号标签页关闭后就错挂在新的 2 号（原来的 3 号挪上来
            // 之前的位置语义已经变了）……总之下标语义必须和 `tabs` 严格
            // 同步，否则关键词会串到别的列表上。split_off 保证升序搬移、
            // 目标位必然是空的（原 i 已被移除）。
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

    /// 打开（或切换到已经开着的）"文件扩展名分类"标签页。`scope`：`Some(pi)`
    /// 只看这一个分区（右键该分区进入）；`None` 把当前已扫描的全部分区混在
    /// 一起分类（顶部菜单进入）。
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

    /// 打开（或切换到已经开着的）"重复文件查找"标签页。`scope` 的含义和
    /// `open_extension_tab` 一致；`None`（全部分区一起找）时还能找出跨盘的
    /// 重复文件（比如 C 盘和 D 盘各存了一份一样的安装包），单分区模式下
    /// 找不到这种情况——这也是"全部分区"和"单个分区"两种入口的意义所在，
    /// 不是简单的"要不要多算几个文件"的区别。
    ///
    /// `categorize::spawn_duplicate_scan[_multi]` 会真的读文件内容算哈希做
    /// 确认（见 dedup.rs 的说明），这一步有实打实的磁盘 I/O——但现在是在
    /// 后台线程上跑的：这里立刻插入一个 `loading: Some(...)` 的占位标签页
    /// 就返回，真正的计算通过 `duplicate_rx` 异步收结果，界面全程可以正常
    /// 操作，不会卡住。
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

    /// 打开一个新的"搜索"标签页：持有主名字索引的 `Arc` 快照（纳秒级，
    /// 不再整树深拷贝），画面复用 tree_list.rs 的 Indexed 模式——永远是
    /// 摊平的全部文件列表，输入内容实时过滤（索引直查，几毫秒出全量
    /// 结果，没有进度文案）。每次点菜单都开一个新的——不像扩展名分类/
    /// 重复文件查找那样"同一个分区只开一个、再点就切过去"，因为搜索
    /// 标签页操作的是"打开那一刻"的快照（手里的 `Arc<NameIndex>` 永远
    /// 不变），用户可能就是想对比不同时间点的数据，或者同时开几个分别
    /// 搜不同的东西，不应该被强制复用同一个。
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

    /// 顶部菜单"复制列表"：开一个新标签页，画面和主列表一模一样的可展开
    /// 树（可浏览/选中/删除/建符号链接/检测占用）；但"重新扫描"/"从列表
    /// 移除"/"扩展名分类"/"重复文件查找"这几个跟"管理当前活跃数据"绑定
    /// 更紧的操作，还是只挂在主列表上（点了会提示）——复制出来的这份
    /// 快照定位就是"独立的、随时能回看对比的静态副本"。
    ///
    /// 打开本身是 O(1)：立即插入一个占位标签页，树在后台线程从名字索引
    /// 重建（纯读 `Arc`，无数据竞争，UI 全程零卡顿），几百毫秒后自动展示。
    /// 旧的整树深拷贝方案要卡 UI 几百毫秒，这正是本次优化消灭的痛点之一。
    fn open_copy_tab(&mut self) {
        if self.partitions.is_empty() {
            return;
        }
        let seq = self.tabs.iter().filter(|t| matches!(t, Tab::CopyList { .. })).count() + 1;
        let title = format!("列表副本 {seq}");
        // infos/root_paths 是每分区一条的小数据，同步克隆（微秒级）。
        let partition_infos = self.partition_infos.clone();
        let root_paths = self.partition_root_paths.clone();
        let index = self.main_index.clone();
        // 索引已就绪：立刻在后台线程重建树；还没就绪：先等主索引建好再重建
        // （两种情况都由 `poll_snapshot_tabs` 收尾，UI 不阻塞）。
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

    /// 每帧调用：保证主列表树的名字索引可用。
    ///
    /// 构建在 **UI 线程分帧**进行（树是 UI 线程的独占数据，不能丢给后台
    /// 线程读）：每帧花几毫秒推进一段显式栈，扫描完成后大约 1~2 秒内
    /// 默默建完，期间每一帧都不会有可感知的卡顿。树结构版本号
    /// （`main_tree_version`）对不上时自动作废重来。
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
                // 摊平搜索需要"所在目录"池；主索引同时服务搜索标签页，要建。
                true,
            ));
        }
        // 空闲时每帧多给一点预算——扫描完成后界面基本无事可做，索引能
        // 更快就绪；有交互时每帧 6ms 也远在可感知阈值之下。
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

    /// 每帧调用：保证**当前激活**的分析标签页（扩展名分类/重复文件查找）
    /// 的合成树索引可用——"查找"（Ctrl+F）在它上面要走索引。合成树通常
    /// 规模有限，几十毫秒内建完；树结构变化（删除/符号链接）后自动重建。
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

    /// 每帧调用：收尾"搜索"/"复制列表"这类快照标签页的准备状态——
    /// 主索引就绪后给等索引的搜索标签页填上快照（一次赋值，之后永不
    /// 更新，快照语义），给等索引的复制列表标签页启动后台重建树、并收
    /// 重建结果。
    fn poll_snapshot_tabs(&mut self) {
        let main_ready = self.main_index.clone();
        for tab_i in 0..self.tabs.len() {
            // 1) 等主索引的搜索/复制标签页：主索引就绪 → 填快照/启动重建。
            let wait_index = matches!(
                self.tabs.get(tab_i),
                Some(Tab::SearchList { index: None, .. })
                    | Some(Tab::CopyList { index: None, loading: Some(CopyLoading::WaitIndex), .. })
            );
            if wait_index
                && let Some(idx) = &main_ready {
                    match self.tabs.get_mut(tab_i) {
                        Some(Tab::SearchList { index, .. }) => {
                            // 快照语义：拿到就锁死，之后主列表再怎么变都与此无关。
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
            // 2) 后台重建树完成 → 落地。
            let rebuilt = match self.tabs.get(tab_i) {
                Some(Tab::CopyList { loading: Some(CopyLoading::Building(rx)), .. }) => match rx.try_recv() {
                    Ok(tree) => Some(tree),
                    Err(mpsc::TryRecvError::Empty) => None,
                    // 后台线程 panic 等异常：给一棵空树兜底（总比永远占位好），
                    // 正常逻辑下 rebuild_tree 总会返回。
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

    /// 每帧调用：收后台"重复文件查找"线程的进度/结果消息，更新对应标签页。
    /// 同一时间可能有好几个分区的比对在并行跑（用户开了好几个不同分区的重复
    /// 文件标签页），所以是一个 `Vec`，不是单个 `Option<Receiver<_>>`。
    ///
    /// 标签页有可能在后台还没算完的时候就被用户关掉了（当前没有做"取消正在跑
    /// 的计算"这件事，关掉标签页只是不再展示结果，后台线程会自己跑到底）——
    /// 这种情况下消息直接丢弃，不去找已经不存在的标签页；`duplicate_rx` 里的
    /// 记录要等对应的发送端彻底断开（后台线程跑完、`tx` 被 drop）才清掉，
    /// 不然会有一条永远不会再收到消息、但也一直留在 `Vec` 里的僵尸记录。
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
                                    // 树被整体替换：旧根节点连同它全部子孙的内存已经
                                    // 释放——视图行缓存里的裸指针、进行中的索引构建器
                                    // 持有的指针全部作废，缓存/索引都要推倒重建；查找
                                    // 索引更要紧：标签页刚打开时树是空的，占位索引那时
                                    // 就已建好，不 bump 版本号它就一直是那份空索引，
                                    // "查找"在重复文件标签页上一辈子搜不到东西。
                                    view.expand_version += 1;
                                    view.struct_version += 1;
                                }
                                categorize::DuplicateMessage::Failed(reason) => {
                                    // 后台线程 panic 的兑底路径（线程内部 catch_unwind
                                    // 已经尽力把原因带出来了）：清掉 loading 让标签页
                                    // 恢复可交互，不要永远停在"正在比对内容…"转圈。
                                    *loading = None;
                                    crate::applog::log(&format!("[app] 重复文件比对失败 (pi={pi:?}): {reason}"));
                                    self.status_message = Some(StatusMsg::error(reason));
                                }
                            }
                        }
                        // 标签页已经被关掉的情况：消息直接丢弃，什么都不做。
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        // 通道断开却没收到 Done/Failed：只会是后台线程异常退出
                        // （panic hook 之后线程直接死掉之类的极端情况）。同样把
                        // 对应标签页的 loading 清掉——以前这里只清了接收器记录，
                        // 标签页的"正在比对内容…"永远转下去，是审计发现的
                        // "panic 防护没闭环"的一部分。
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

    /// 选分区/目录的弹窗。用 `egui::Window` 模拟模态：不可缩放、居中，
    /// 背景内容在 `show_main_screen` 里已经被 `add_enabled_ui(false, ..)` 整体禁用了。
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

    /// 磁盘/根目录行右键"重新扫描"：用同一个路径重新走一遍 `scan::spawn_scan`，
    /// 结果原地替换 `self.partitions[pi]`（`poll_scan` 里看 `rescan_target`），
    /// 不追加成新的一行、不影响列表里的其它分区。
    ///
    /// 如果已经有扫描在跑（新增扫描，或者另一个重新扫描），直接忽略这次请求——
    /// `scan_rx`/`current_scan_path` 全局只有一份，同时跑两个会互相踩。
    /// 大部分场景下用户也不会真的在扫描进行中还去点别的分区的"重新扫描"。
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

    /// 磁盘/根目录行右键"从列表移除"：只从 `self.partitions`（以及三个和它
    /// 一一对应的平行数组）里摘掉，不碰磁盘上的任何文件。已经打开的扩展名
    /// 分类/重复文件查找标签页各自持有自己独立的一份数据快照（不是这里的
    /// 引用），不受影响；"搜索"标签页同理，打开时就整份克隆过去了。
    ///
    /// 摘掉之后，原来排在它后面的分区下标全部往前挪一位——`self.selected`
    /// 存的是"分区下标 + 树内路径"，可能因此指向错误的分区，干脆直接清空，
    /// 比费劲去修正一个可能受影响的下标更省事也更不容易出错。
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
        // 分区数组变了：名字索引作废重建（下一帧 `ensure_main_index`）。
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
                    // 分类统计只算一次：侧边栏缓存、日志摘要共用同一份结果
                    // （以前 log_scan_summary 里又把整棵树重新遍历分类了一遍）。
                    let categories = categorize::compute_categories(&node);
                    log_scan_summary(&node, info.as_ref(), &categories);
                    if let Some(pi) = self.rescan_target.take().filter(|&pi| pi < self.partitions.len()) {
                        // 重新扫描：原地替换，位置不变，root_path 本来就是同一个，
                        // 不用跟着换。
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
                    // push/原地替换都可能让主列表缓存里存的裸指针失效（push 可能
                    // 触发 Vec 扩容搬家；原地替换整个 Node 更是直接让旧指针失效）——
                    // 版本号 +1 强制下一帧重新收集可见行。
                    self.list_state.expand_version += 1;
                    // 树结构变了：名字索引作废，下一帧 `ensure_main_index` 会用
                    // 新版本号重新分帧构建（后台默黙进行，用户无感）。
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

    /// 把展开/选中操作应用到"当前激活的那个标签页"自己的树/选中状态上——
    /// 主列表用 `self.selected` + `self.partitions`，每个分析标签页有自己独立的
    /// `selected` + 合成 `root`，互不干扰（切标签页不会互相影响展开状态）。
    fn apply_tree_action(&mut self, action: TreeAction) {
        let tab_idx = self.active_tab.min(self.tabs.len().saturating_sub(1));
        // `true`：这个 abs_path 应该去 `self.tabs[tab_idx]` 自己带的那棵树上找——
        // 可能是分析标签页的合成树（`root: Node`），也可能是"复制列表"标签页
        // 后台重建出来的树快照（形状和主列表一样）；具体是哪一种，
        // 下面各处用到的地方会再按 `self.tabs[tab_idx]` 的实际变体分别处理。
        // `false`：直接用 `self.partitions`（主列表）。
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
                                // 合成树只有一个根（tree_list 里传的是单元素切片），abs_path[0] 恒为 0，
                                // 真正要用来定位/展开的是 p[1..]。`toggle_expand(&[])`（p.len()==1
                                // 时 p[1..] 就是空切片）在语义上就是"对 root 自己切换"——`navigate_mut`
                                // 空路径返回的正是 root 本身——统一走 `toggle_expand` 而不是手动
                                // `expanded = !expanded`，收起根节点的时候才会连它整棵子树的展开
                                // 状态一起重置（不然只有走到"更深一层"的分支才会重置，根节点直接
                                // 翻转反而漏了这一步，展开过的子文件夹状态会一直留着，之前的 bug
                                // 就是这么来的）。
                                root.toggle_expand(&p[1..]);
                                *selected = Some(p);
                                // 展开状态变了，这个标签页缓存的可见行列表要重算。
                                view.expand_version += 1;
                            }
                            Tab::CopyList { tree, selected, list_state, .. } => {
                                // "复制列表"标签页的树快照和主列表是同一种形状（多个分区根），
                                // abs_path[0] 是分区下标，不是恒为 0——和上面 View 分支的语义
                                // 不一样，不能共用同一段代码。
                                if let Some(&pi) = p.first()
                                    && let Some(part) = tree.get_mut(pi) {
                                        part.toggle_expand(&p[1..]);
                                    }
                                list_state.expand_version += 1;
                                *selected = Some(p);
                            }
                            // "搜索"标签页是索引摊平列表，没有展开概念（渲染层也
                            // 不会对它发 ToggleExpand），防御性忽略。
                            Tab::SearchList { .. } => (),
                            Tab::Main => (),
                        }
                    }
                } else {
                    if let Some(&pi) = p.first()
                        && let Some(part) = self.partitions.get_mut(pi) {
                            part.toggle_expand(&p[1..]);
                        }
                    // 展开状态变了，主列表缓存的可见行列表要重算。
                    self.list_state.expand_version += 1;
                    self.selected = Some(p);
                }
            }
            TreeAction::RequestDelete { abs_path, name, full_path, is_folder, index_entry } => {
                // 只是记下来，真正删除要等用户在确认框里点"确定"——见 show_delete_confirm_modal。
                // in_tab_tree 已经在函数开头算好了：决定这个 abs_path 应该去哪棵树上摘。
                let source = if in_tab_tree { DeleteSource::Tab(tab_idx) } else { DeleteSource::Main };
                self.pending_delete = Some(PendingDelete { source, abs_path, name, full_path, is_folder, index_entry });
            }
            TreeAction::RequestCheckLock { abs_path, name, full_path, is_folder } => {
                let request = LockCheckRequest { tab_idx, is_view_tab: in_tab_tree, abs_path, name, full_path, is_folder, is_group: false };
                self.run_lock_check(request);
            }
            TreeAction::RequestCheckLockGroup { abs_path, name } => {
                // 分组行只会出现在"重复文件查找"标签页，`full_path`/`is_folder`
                // 对分组本身没有意义（组不是磁盘上的一个真实位置），留空/
                // 默认值即可——真正要查的路径列表由 `run_lock_check_with_feedback`
                // 的 `is_group` 分支从组的子节点现查。
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
                // 只对主列表有意义——"搜索"/"复制列表"标签页里的磁盘行对应的
                // 是打开那一刻复制的静态快照，"重新扫描"这个概念在那边没有
                // 清楚的语义（该更新快照本身？还是跳回主列表？），干脆不
                // 响应，但要明确提示一下，不能让用户以为点了没反应/点坏了。
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
                // 同样只对主列表有意义——"搜索"/"复制列表"标签页里的分区是
                // 快照，`pi` 这个下标对不上 `self.partitions`，硬按下去会
                // 分类错分区的数据。
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

    /// "检测占用"：文件夹直接用重命名探测判断，一有结果（没占用/确定被占用/
    /// 无法确定）就立刻停下来展示，不会自动去查文件夹里的文件——想知道具体
    /// 是哪个进程/服务占用的，用户需要自己点结果里的"🔍 具体检测"，才会
    /// 进入 `start_layered_probe` 那套逐层排查逻辑。文件（不是文件夹）则
    /// 直接用 Restart Manager 查它自己，重命名探测对单个文件没有意义。
    ///
    /// 真正调用 Restart Manager 的那一步在后台线程跑（见
    /// `run_lock_check_with_feedback`），不会卡住界面——以前是同步调的，
    /// 文件多的文件夹上卡顿很明显，用户点了"检测占用"感觉像卡死了一样。
    fn run_lock_check(&mut self, request: LockCheckRequest) {
        self.run_lock_check_with_feedback(request, None);
    }

    /// 文件夹分支只做一次同步的重命名探测（几毫秒），结果直接展示，不带
    /// 任何后台任务。文件分支才会用到下面的 `LOCK_CHECK_LIMIT`/后台线程——
    /// 单个文件查询本身很快，这个上限主要是分组（重复文件查找的"检测占用
    /// （整组）"）以及分层探测里"这一层的文件"批量查询时用来兜底，避免
    /// 极端情况下一次性塞进成千上万个资源给 Restart Manager 注册。
    fn run_lock_check_with_feedback(&mut self, request: LockCheckRequest, action_feedback: Option<String>) {
        const LOCK_CHECK_LIMIT: usize = 2000;
        let is_folder = request.is_folder;
        // 任何一次（重新）检测都让"具体检测"回到未开始的状态——避免用户对着
        // 另一个文件/文件夹按层排查到一半的结果，跟这一次全新检测的结论
        // 混在一起看，容易误解。想继续深入排查的话，检测完再点一次
        // "🔍 具体检测"就行，多这一下点击换来状态不会互相串。
        self.layered_probe = None;

        // 分组请求（重复文件查找里的"检测占用（整组）"）：直接从分组节点的
        // 子节点收集路径（都是真实文件，不用递归），没有"重命名探测"这一说
        // （分组不对应磁盘上的一个真实位置），也不走下面文件夹/文件的正常
        // 分支——单独处理完就直接返回。
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

        // 文件夹先用重命名探测——比"查文件夹里最多 2000 个文件"准得多、也
        // 快得多（见 `file_ops::check_folder_occupied_by_rename` 上的详细
        // 说明；旧的按文件查的方式对 `C:\Users`/`C:\Windows` 这类大文件夹
        // 经常给出"没查到占用"的假阴性结论，就是这次要修的问题）。这一步
        // 只是一次改名+立刻改回来，几毫秒的事，不用挪到后台线程。
        //
        // 文件夹分支到这里就结束了——不管探测结果是"没占用"还是"占用/无法
        // 确定"，都直接展示结论、不再额外起后台线程把文件夹里的文件（哪怕
        // 只是内存里已扫描好的树，最多 2000 个）丢给 Restart Manager 查一遍。
        // 之前这里会自动多查一次"供参考"，代价是要等这次注册最多 2000 个
        // 资源的调用跑完（这一步本身就是重、慢的），"🔍 具体检测"按钮又是
        // 等 loading 结束才出现，用户点了"检测占用"之后会经历一段不知道在
        // 等什么的空白期。现在改成：重命名探测一有结果就立刻停下来展示，
        // "占用/无法确定"的情况下按钮马上就能点，要不要再往深一层查完全
        // 交给用户自己决定（点"🔍 具体检测"进入 `start_layered_probe`，
        // 那边才会真正逐层调用 Restart Manager 查文件）。
        if is_folder {
            let rename_probe = crate::file_ops::check_folder_occupied_by_rename(&request.full_path);
            let name = request.name.clone();
            self.lock_check_result = Some(LockCheckResult {
                request, name, is_folder: true, checked_count: 0, truncated: false, procs: Vec::new(),
                action_feedback, error: None, rename_probe: Some(rename_probe), loading: false,
            });
            return;
        }

        // 走到这里说明是单个文件——不是文件夹，重命名探测这一套对文件没有
        // 意义（文件被打开不影响改名，改名成功不代表没被占用），直接用
        // Restart Manager 命令式地查这一个文件被谁占用，跟之前一样挪到
        // 后台线程跑，不卡 UI。
        let paths: Vec<String> = vec![request.full_path.clone()];
        let truncated = paths.len() >= LOCK_CHECK_LIMIT;
        let checked_count = paths.len();
        let name = request.name.clone();
        // 先弹一个"检测中…"的占位结果——点了"检测占用"马上就有反馈，不会
        // 让人以为点了没反应；真正的查询结果由 `poll_lock_check` 收到之后
        // 再替换进去。文件不做重命名探测，`rename_probe` 恒为 None。
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

    /// 每帧调用：收后台"检测占用"线程的结果。
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

    /// "🔍 具体检测"：文件夹本身重命名探测没通过（确定被占用/无法确定）时，
    /// 用户可以选择再往深一层排查，而不是直接一次性把里面最多 2000 个文件
    /// 全丢给 Restart Manager——见 `LayeredProbe` 上的说明。
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

    /// 处理 `frontier` 里这一层的全部文件夹：列出每个文件夹的直属子项，
    /// 子文件夹用重命名探测（快，同步做），子文件（这一层直属的文件，
    /// 不含子文件夹里更深的文件）收集起来交给后台线程用 Restart Manager 查。
    /// 列目录、重命名探测都在 UI 线程同步做——文件夹数量正常情况下不会
    /// 离谱到影响流畅度（不像"查最多 2000 个文件"那样量级可能很大），
    /// 只有"查文件的占用"这一步（真正调用 Restart Manager 的那一步）扔进
    /// 后台线程，不卡界面。
    fn advance_layered_probe(&mut self) {
        let Some(probe) = &mut self.layered_probe else { return };
        if probe.pending.is_some() {
            return; // 上一层还没跑完，不重复触发
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
                    // 读不了这个目录（可能本身就是权限问题，或者恰好被删了）——
                    // 跳过，不让这一个目录的问题挡住其它兄弟目录的排查。
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
                    _ => {} // 符号链接/设备文件之类的特殊项，不参与占用排查
                }
            }
        }

        let layer_just_finished = probe.layer + 1;
        let files_checked = layer_files.len();
        if layer_files.is_empty() {
            // 这一层没有直属文件要查，不用起后台线程，直接推进到下一层。
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

    /// 每帧调用：收"具体检测"当前这一层的文件占用查询结果。
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
                // 同一个进程可能同时占用了好几个正在查的文件，跨层也可能
                // 重复命中——按 PID 去重，汇总列表里不会出现同一个进程好几条。
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

    /// "具体检测"结果里点"⚡ 结束以上全部后重新检测"：先把目前汇总到的
    /// 进程/服务全部结束掉，再重新探测一次顶层文件夹本身——如果确实已经
    /// 解除占用，顶层重命名会直接成功，不需要再继续跑剩下的层；如果还没
    /// 解除，就接着跑当前还没处理完的这一层/下一层（不用整个从头重来，
    /// 已经确认过"重命名成功、没问题"的子文件夹不会平白无故重新被占用，
    /// 没必要浪费时间重新验一遍）。
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

    /// 结果窗口里点了"结束进程"：直接执行（这个功能本来就是主打"一键处理完
    /// 占用就能继续删除/创建符号链接"，按钮上已经明确写着进程名和 PID，
    /// 没有再加一层二次确认弹窗——见 `show_lock_check_modal` 里的取舍说明）。
    /// 执行完不管成功失败都会自动重新检测一次，让用户立刻看到最新状态，
    /// 不用自己再点一次"检测占用"。
    fn lock_check_terminate_process(&mut self, pid: u32) {
        // 在"具体检测"（按层排查）的结果列表里点单个"结束进程"，只处理这一个、
        // 从汇总列表里摘掉就行，不用像简单视图那样强制回到顶层重新整体检测一遍——
        // 那是"⚡ 结束以上全部后重新检测"按钮的职责。
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

    /// "一键结束全部"：浏览器这类程序经常一开一大堆进程，一个个点"结束进程"
    /// 太麻烦，而且逐个结束的这段时间里，先结束掉的进程有可能被还活着的
    /// 主进程/看门狗重新拉起来——干脆一次性把当前列表里的全部进程都杀掉，
    /// 同一个 PID 只杀一次（一个进程可能同时占用了好几个正在检查的文件，
    /// 在列表里出现好几条）。
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

    /// 结果窗口里点了"停止服务"：道理和结束进程一样，直接执行 + 执行后自动刷新。
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

    /// 用上一次的请求参数重新跑一遍"检测占用"，`feedback` 是这次刷新之前
    /// 那个操作（结束进程/停止服务）的结果提示，跑完之后会带到新的结果里
    /// 继续展示——不然重新检测一刷新，"刚才那个操作到底成没成功"这条反馈
    /// 就没地方看了。
    fn refresh_lock_check(&mut self, feedback: Option<String>) {
        let Some(prev) = &self.lock_check_result else { return };
        let request = prev.request.clone();
        self.run_lock_check_with_feedback(request, feedback);
    }

    /// "检测占用"的结果窗口：简单列表 + 每行直接带"结束进程"/"停止服务"按钮，
    /// 目标是新手也能一眼看懂、一键处理完就能回去继续删除/创建符号链接，
    /// 不用先去弄明白"这个进程名对应哪个程序""服务管理器在哪"这些额外知识。
    ///
    /// 特意没有在点按钮之后再弹一层"确定要结束吗"的二次确认——按钮上已经
    /// 完整写着"结束进程 XXX.exe（PID 1234）"，要结束的是谁清清楚楚摆在
    /// 眼前，再加一层确认对着重"一键"体验的新手用户只是多一次点击、没有
    /// 实质上更安全。真正的安全网是"直接、清楚地展示要操作的对象"本身。
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
                    // 文件夹优先看"重命名探测"的结论——这是比"逐个文件查 Restart
                    // Manager"更可靠的结果（见 `check_folder_occupied_by_rename`
                    // 上的说明），不确定的时候如实说"无法确定"，不能因为查文件
                    // 没查到就武断地报告"没有占用"（这正是之前 C:\Users 这类
                    // 大文件夹被误判"没占用"的根因）。
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
                                // 文件（不是文件夹）：没有重命名探测这一说，纯粹看
                                // Restart Manager 查文件本身的结果。
                                ui.colored_label(Color32::from_rgb(0x34, 0xC7, 0x59), "✓ 没有检测到占用，可以放心删除/创建符号链接了。");
                            } else {
                                ui.label(egui::RichText::new("Restart Manager 没有在检查过的文件里找到具体占用的进程（可能占用的是没查到的文件，也可能是权限一类的问题）。").small().color(Color32::from_rgb(0xA0, 0xA0, 0xA0)));
                            }
                        } else {
                            ui.horizontal(|ui| {
                                ui.colored_label(Color32::from_rgb(0xF5, 0xA6, 0x23), format!("找到 {} 个进程/服务正在占用，处理完下面这些就能继续了：", result.procs.len()));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    // 浏览器这类一开一大堆进程的场景，一个个点太麻烦，而且逐个
                                    // 结束的这段时间里先结束掉的可能被还活着的进程重新拉起来——
                                    // 一次性全杀掉更省事也更保险。
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
                                            // 是服务的话优先给"停止服务"（更精确、只影响这一个服务，
                                            // 不会像直接杀掉宿主进程那样可能牵连同一个 svchost.exe
                                            // 进程里跑着的其它无关服务）；同时依然保留"结束进程"，
                                            // 有些占用场景下停服务不一定能真正解除占用（比如占用的
                                            // 其实是进程自己打开的句柄，不是服务本身的），两个选项
                                            // 都给，让用户自己选。
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

    /// "关于 DiskForge"悬浮窗（首次启动的赞助提示与"关于"菜单共用同一套逻辑，
    /// 见 [`AboutState`]）：版本/版权/许可声明 + 微信/支付宝两个赞助码标签 +
    /// 两个关闭按钮。窗口没有关闭按钮（不设 `.open()`），只能点底部按钮消失：
    /// - "下次一定"：只关窗口，下次启动还会弹；
    /// - "不再提醒"：写入持久化标记（%APPDATA%\DiskForge），之后启动不再自动弹。
    ///   首次启动时按钮带 6 秒倒计时（防止弹窗一闪被误点划过）；"关于"菜单入口
    ///   不带倒计时，按钮立即可点。
    fn show_about_modal(&mut self, ctx: &egui::Context) {
        // 倒计时推进：用 stable_dt（固定步进时长，掉帧也不会一帧跳好几秒），
        // 到 0 解锁按钮；倒计时期间每帧请求重绘，按钮上的秒数才会持续走动。
        // 只锁"不再提醒"一个按钮（剩余秒数写在按钮文字上），"下次一定"随时可点。
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
        // Some(true) = 不再提醒（持久化后关闭）；Some(false) = 下次一定（只关闭）。
        let mut action: Option<bool> = None;
        // "不再提醒"按钮的剩余秒数（向上取整：刚弹出的 6.0 显示 6，走到
        // 0.4 也还显示 1，不会出现"显示 0 秒却还点不了"的观感）。
        let countdown_secs = about.countdown.map(|t| t.ceil() as u32);
        // 三条组合行上一帧的自然宽度（首帧为 0，贴左，次帧起居中）。
        let mut logo_row_w = about.logo_row_w;
        let mut tabs_row_w = about.tabs_row_w;
        let mut btns_row_w = about.btns_row_w;

        egui::Window::new(format!("关于 {}", crate::about::APP_NAME))
            .id(egui::Id::new("about_modal"))
            // 注意：不调用 `.open(...)`（那会渲染标题栏关闭按钮）——这个悬浮窗
            // 没有关闭按钮，只有底部两个按钮能触发关闭。
            .collapsible(false)
            .resizable(false)
            .default_width(400.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                // ── 内容左右居中 ──
                // 单控件行（短文本/二维码）用 top_down(Center) 布局：egui 会
                // 把控件水平居中摆放，文字也按居中锚定（实测 100px/216px
                // 定宽控件都精确居中，纵向仍从上往下，不影响窗口尺寸）。
                let centered = egui::Layout::top_down(egui::Align::Center);
                // 组合行（logo+名称、双标签、双按钮）egui 没有可靠的整行居中
                // 原语（顺序行永远从左侧开始摆），用"行前留白 = (可用宽 −
                // 上一帧行宽)/2"居中，实测行宽每帧回写 AboutState。
                let row = egui::Layout::left_to_right(egui::Align::Center);

                // 顶部品牌区：logo + 名称/版本，整行作为一组左右居中。
                let pad = if logo_row_w > 0.0 { ((ui.available_width() - logo_row_w) / 2.0).max(0.0) } else { 0.0 };
                // 注意：这里必须用 allocate_ui_with_layout 并显式给一个有限高度，
                // 不能直接 with_layout(row, ...)——with_layout 生成的子 Ui 会
                // 继承父级"剩余的全部可用高度"作为 max_rect，配合 Align::Center
                // 纵向居中，会把这一整块剩余高度当成本行占用的高度报回去，
                // 导致 52×52 的 logo 撑出一大片空白，把整个悬浮窗高度顶大。
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
                // 许可声明是成段法律文本，保持左对齐（成段居中文字可读性差）。
                ui.label(egui::RichText::new(crate::about::LICENSE_NOTICE).size(10.5).weak());
                ui.separator();
                ui.with_layout(centered, |ui| {
                    ui.label(egui::RichText::new(format!("☕ {}", crate::about::SPONSOR_HINT)).size(12.0));
                });
                ui.add_space(2.0);
                // 标签切换（微信/支付宝）：整行居中。
                let pad = if tabs_row_w > 0.0 { ((ui.available_width() - tabs_row_w) / 2.0).max(0.0) } else { 0.0 };
                // 同上：显式给定高度，避免 with_layout 继承整块剩余高度。
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
                // 二维码：居中显示当前标签对应的收款码。
                ui.with_layout(centered, |ui| {
                    let qr_id = match tab {
                        SponsorTab::WeChat => wechat_id,
                        SponsorTab::Alipay => alipay_id,
                    };
                    ui.add(egui::Image::new(egui::load::SizedTexture::new(qr_id, [216.0, 216.0])));
                });
                ui.add_space(4.0);
                // 底部两个按钮：整行居中。"下次一定"随时可点；"不再提醒"在
                // 倒计时期间禁用，剩余秒数直接写在按钮文字上（不再提醒(6s)
                // → 不再提醒），不用再单独放一行提示文字。
                let pad = if btns_row_w > 0.0 { ((ui.available_width() - btns_row_w) / 2.0).max(0.0) } else { 0.0 };
                // 同上：显式给定高度，避免 with_layout 继承整块剩余高度。
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

    /// "删除到回收站"确认框：点右键菜单只是记了个 `pending_delete`，这里才是用户
    /// 点"确定"之后真正调用 Win32 API 的地方。
    fn show_delete_confirm_modal(&mut self, ctx: &egui::Context) {
        let Some(pending) = &self.pending_delete else { return };
        let kind = if pending.is_folder { "文件夹" } else { "文件" };
        let mut confirm = false;
        let mut cancel = false;
        // Esc = 取消：模态确认框的标准键盘语义（以前按 Esc 没反应，还会被
        // 查找窗的 Esc 处理抢去关掉查找窗，两个都不符合用户预期）。
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

    /// 用户在确认框里点了"删除到回收站"：真正的删除（含占用重试，最多可能
    /// 要等接近 2 秒——见 `file_ops::delete_to_recycle_bin_with_retry`）挪到
    /// 后台线程做，不在 UI 线程上等，弹窗立刻关掉，界面照常能操作；结果通过
    /// `delete_rx` 在 `poll_delete` 里收。
    fn execute_pending_delete(&mut self) {
        let Some(pending) = self.pending_delete.take() else { return };
        let full_path = pending.full_path.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::file_ops::delete_to_recycle_bin_with_retry(&full_path));
        });
        self.delete_rx = Some((pending, rx));
    }

    /// 每帧调用：收后台删除线程的结果。
    fn poll_delete(&mut self) {
        let Some((_, rx)) = &self.delete_rx else { return };
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return, // 还没删完，下一帧再看
            // 发送端断开却没收到消息，理论上只会是后台线程 panic 了——正常
            // 逻辑下 `delete_to_recycle_bin_with_retry` 总会返回一个
            // Ok/Err，不会出现"什么都不发就跑没了"，这里兜个底不留僵尸记录。
            Err(mpsc::TryRecvError::Disconnected) => Err("删除线程异常退出".to_string()),
        };
        let (pending, _) = self.delete_rx.take().unwrap();
        match result {
            Ok(()) => {
                self.remove_node_after_success(pending.source, &pending.abs_path, pending.index_entry);
                self.status_message = Some(StatusMsg::info(format!("已删除到回收站: {}", pending.name)));
            }
            Err(e) => {
                // `delete_to_recycle_bin_with_retry` 失败之后已经尝试查过占用
                // 进程、把结果拼进了错误信息里（见 file_ops.rs），这里直接
                // 展示，不用再单独查一遍。
                self.status_message = Some(StatusMsg::error(format!("删除失败 ({}): {e}", pending.name)));
            }
        }
    }

    /// "删除到回收站"和"创建符号链接"成功之后，都要把对应的项从内存里的树上
    /// 摘掉（同时更新沿途所有祖先的聚合统计、清掉可能指向它的 `selected`、
    /// 让对应视图的缓存失效重算）——两个操作末尾这段收尾逻辑完全一样，抽出来
    /// 共用一份，不用维护两份几乎一样的代码。
    ///
    /// `index_entry`：请求来自"搜索"标签页的索引摊平行时是条目下标——快照
    /// 没有可摘的树，改为把它记进该标签页的剔除列表（这一行从显示里消失，
    /// 与旧版"从快照树里摘掉"的显示效果一致）。
    fn remove_node_after_success(&mut self, source: DeleteSource, abs_path: &NodePath, index_entry: Option<u32>) {
        match source {
            DeleteSource::Main => {
                // abs_path 至少是 [分区下标, 子节点下标, ...]——右键菜单的这两个
                // 操作目前只挂在文件/文件夹行上，不挂在磁盘/根目录行上，所以这里
                // 长度必然 >= 2；如果哪天误传了长度 1 的路径，宁可什么都不做也
                // 不去动 partitions/partition_infos/partition_categories/
                // partition_root_paths 这几个下标必须一一对应的并行数组——只删
                // partitions 一个会把它们全错位。
                if abs_path.len() >= 2
                    && let Some(&pi) = abs_path.first()
                        && let Some(part) = self.partitions.get_mut(pi) {
                            part.remove_at_path(&abs_path[1..]);
                        }
                if self.selected.as_ref() == Some(abs_path) { self.selected = None; }
                self.list_state.expand_version += 1;
                // 主树结构变了：名字索引作废重建（下一帧 `ensure_main_index`）。
                self.main_tree_version += 1;
            }
            DeleteSource::Tab(tab_idx) => {
                // 如果这时候标签页已经不在了、或者被切换成了别的类型（用户在
                // 操作期间关掉了那个标签页），就什么都不做，不去动任何数据。
                let Some(tab) = self.tabs.get_mut(tab_idx) else { return };
                match tab {
                    Tab::Extensions { root, selected, view, .. } | Tab::Duplicates { root, selected, view, .. } => {
                        if abs_path.len() >= 2 {
                            root.remove_at_path(&abs_path[1..]);
                        }
                        if selected.as_ref() == Some(abs_path) { *selected = None; }
                        view.expand_version += 1;
                        // 合成树结构变了：本标签页的查找索引下一帧重建。
                        view.struct_version += 1;
                    }
                    Tab::SearchList { list_state, .. } => {
                        // 快照没有自己的树：把这一行从显示里剔除（效果与旧版
                        // "从快照树里摘掉"一致），索引本身不动（快照语义）。
                        if let Some(e) = index_entry {
                            list_state.mark_search_row_removed(e);
                        }
                    }
                    Tab::CopyList { tree, selected, list_state, .. } => {
                        // "复制列表"的树快照和主列表同形状（多个分区根），
                        // abs_path[0] 是分区下标，删除逻辑照抄 `DeleteSource::Main`
                        // 那一支，只是操作的 `Vec<Node>` 换成这个标签页自己那份树。
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

    /// 右键菜单点了"创建符号链接"（主列表的单个文件/文件夹）：记下上下文，
    /// 弹目标分区选择框（`show_symlink_target_picker_modal`）——真正的迁移
    /// 工作要等用户选完分区才开始，选完之后走 `launch_symlink_job`。
    fn start_symlink_single(&mut self, source: DeleteSource, abs_path: NodePath, name: String, full_path: String, is_folder: bool) {
        self.symlink_pick_drives = disk_info::list_fixed_drives_with_labels();
        if abs_path.len() < 2 {
            return; // 防御：这个操作不应该出现在磁盘/根目录行上
        }
        self.pending_symlink_pick = Some(PendingSymlinkKind::Single { source, abs_path, name, full_path, is_folder });
    }

    /// 重复文件分组行点了"创建符号链接"：先收集分组成员路径、记下上下文，
    /// 弹目标分区选择框，选完之后走 `launch_symlink_job`。
    fn start_symlink_group(&mut self, tab_idx: usize, abs_path: NodePath, name: String) {
        self.symlink_pick_drives = disk_info::list_fixed_drives_with_labels();
        // 分组行只会出现在"重复文件查找"标签页（见 compact_tree.rs 里
        // `is_duplicates` 那个开关），这里直接按 Tab(tab_idx) 处理。
        let node = self.tabs.get(tab_idx).and_then(|t| match t {
            Tab::Duplicates { root, .. } => root.get_at_path(&abs_path[1..]),
            _ => None,
        });
        let Some(node) = node else { return };
        let member_paths: Vec<String> = node.children.iter().filter_map(|c| c.full_path_override.clone()).collect();
        if member_paths.len() < 2 {
            return; // 防御：正常情况下一个重复文件组至少有 2 个成员
        }
        self.pending_symlink_pick = Some(PendingSymlinkKind::Group { tab_idx, abs_path, name, member_paths });
    }

    /// 目标分区选择框里选好了一个分区，`drive` 是选中的盘符：拼出
    /// `{drive}:\DiskForge` 当 base_dir，把真正的迁移工作（复制/校验/删除/
    /// 建链接，涉及真实磁盘 IO，可能要花不少时间）扔到后台线程，不能卡在
    /// UI 线程上。
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
                                // 单个副本失败不影响其它副本继续处理——只记日志，最后的
                                // 汇总消息里会体现"实际处理了几个"，用户能看出跟总数
                                // 是不是对得上。
                                Err(e) => crate::applog::log(&format!("[app] 重复文件组内替换符号链接失败: {p}: {e}")),
                            }
                        }
                        Ok(SymlinkOutcome::Group { target_path, member_count: done, total_count: 1 + rest.len() })
                    })();
                    let _ = tx.send(result);
                });
                // 组场景不需要"原地刷新单个节点"（整个分组直接摘掉），full_path/
                // is_folder 留空/默认值即可，见 `SymlinkRequest` 上的说明。
                self.symlink_rx = Some((SymlinkRequest { source: DeleteSource::Tab(tab_idx), abs_path, name, full_path: String::new(), is_folder: false }, rx));
            }
        }
    }

    /// "创建符号链接"的目标分区选择框：只能从已经检测到的固定分区（C/D/E/F…）
    /// 里选一个，不再是任意文件夹——真实数据统一存在 `{选中分区}:\DiskForge`
    /// 下面，文件按内容哈希分文件夹、文件夹按原始路径镜像存放（见
    /// `file_ops.rs` 里 `migrate_file_to_symlink`/`migrate_folder_to_symlink`
    /// 上的说明），位置固定、可推导，不会因为用户每次随手选了不同文件夹而
    /// 散落得到处都是、事后很难知道真身到底在哪。
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
                // 打开那一刻缓存下来的列表，不再每帧重新枚举 Win32 磁盘。
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

    /// 每帧调用：收后台"创建符号链接"线程的结果。
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
                        // 原地刷新成"磁盘上的最新状态"（现在是个符号链接了），而不是
                        // 把这一项从树上整个摘掉——摘掉的话，在 Windows 资源管理器
                        // 里这个文件/文件夹其实还在（只是变成了链接），列表却凭空
                        // 少一项，容易让人误以为出了问题、操作失败了。
                        self.refresh_node_after_symlink(request.source, &request.abs_path, &request.full_path, &request.name, request.is_folder);
                    }
                    SymlinkOutcome::Group { .. } => {
                        // 分组场景：分组这一条汇总行代表的是"这些文件互为重复"，
                        // 现在已经处理完、都指向同一份真实数据了，摘掉这个分组行
                        // 本身还是合理的（不像单个文件/文件夹那样，摘掉会让人
                        // 以为东西凭空消失——分组毕竟不是磁盘上的一个真实位置）。
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

    /// 创建符号链接成功后，原地刷新这一项（而不是整个摘掉）——见调用点的说明。
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
                // 节点被原地替换（名字可能变、属性变成 reparse point）：名字索引
                // 作废重建（下一帧 `ensure_main_index`）。
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
                    // "搜索"标签页是索引快照（无树），符号链接后的状态变化
                    // 本来就不会追溯到"打开那一刻"的快照里——不处理。
                    Tab::SearchList { .. } => {}
                    Tab::Main => {}
                }
            }
        }
    }

    /// 导出全部已扫描的分区/目录，每个各自一个 CSV 文件。
    ///
    /// 以前在 UI 线程上同步遍历树写文件——百万行的盘会冻结界面几分钟。
    /// 现在从主索引快照（`Arc<NameIndex>`）在后台线程导出：UI 全程零卡顿，
    /// 底部状态条实时显示进度，进度/结果通过 [`ExportMessage`] 通道回来
    /// （`poll_export` 每帧收）。导出的内容是"点击导出那一刻"的快照，
    /// 与搜索/复制列表标签页同一套快照语义。
    fn export_csv(&mut self) {
        if self.partitions.is_empty() { return; }
        // 索引是导出的数据源；还没就绪（扫描刚完成的头几秒）就提示稍后再试，
        // 不静默失败也不阻塞。
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

    /// 每帧调用：收后台 CSV 导出的进度/结果，更新底部状态条。
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
        // ShellExecuteW(hwnd, verb, file, params, dir, show_cmd)
        let result = unsafe {
            windows_sys::Win32::UI::Shell::ShellExecuteW(
                std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(),
                std::ptr::null(), std::ptr::null(), 1,
            )
        };
        // ShellExecuteW 返回值 > 32 表示成功
        if (result as isize) > 32 {
            std::process::exit(0);
        } else {
            crate::applog::log(&format!("[app] 以管理员身份重启失败 (ShellExecuteW={result:?})"));
            self.status_message = Some(StatusMsg::error("以管理员身份重启失败，请手动以管理员运行".to_string()));
        }
    }
}

/// "搜索"/"复制列表"标签页在快照准备期间显示的占位：转圈 + 一句话。
/// 正常情况下这段时间不到一秒（搜索标签页等主索引建完、复制列表等后台
/// 线程把树重建出来），期间 UI 线程完全不被阻塞，其它标签页照常能操作。
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

/// "重复文件查找"标签页在后台线程还没算完时显示的占位内容：转圈 + 进度条。
/// 两个阶段（`dedup::HashPhase::Prefilter`/`Confirm`）分开展示，各自的
/// `done`/`total` 都是这个阶段自己的数字，不用再夹 `min` 防止"超过 100%"——
/// 见 `dedup.rs`/`app.rs` 里 `Tab::Duplicates.loading` 字段上的说明：不分开
/// 展示的话，切换到第二阶段时要么看起来"卡在 100% 不动"要么"进度突然归零
/// 往回跳"，两种都会让人误以为程序卡死了。
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

/// 从选择界面的状态构造出这一批要扫描的路径列表：已选中的固定分区（按盘符排序，保证
/// 每次顺序确定）+ 用户手动添加的自定义目录。
fn build_scan_paths(picker: &startup::PickerState) -> Vec<PathBuf> {
    let mut drives: Vec<char> = picker.selected_drives.iter().copied().collect();
    drives.sort_unstable();
    let mut paths: Vec<PathBuf> = drives.iter().map(|&l| PathBuf::from(format!("{l}:\\"))).collect();
    paths.extend(picker.custom_paths.iter().map(PathBuf::from));
    paths
}

/// 扫描完成时把完整统计打到日志里（替代原来只针对单个分区的侧边栏"空间统计"文字块——
/// 现在可以同时扫多个分区/目录，塞进侧边栏既放不下也不合适，这些数字列表里也都能看到，
/// 日志留一份方便事后核对/排查）。
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

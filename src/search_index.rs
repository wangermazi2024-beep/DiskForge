//! 扁平名字索引 —— "查找"（Ctrl+F）与"搜索文件"共用的核心数据结构。
//!
//! ## 为什么需要它
//!
//! 之前两个功能都是"现遍历树 + 逐节点匹配"：
//!   - "搜索"标签页：UI 线程上分帧遍历整棵树（旧 `SearchJob`），每敲一次
//!     查询词都要把几十万上百万个节点摸一遍，每个节点还要为匹配分配一次
//!     `to_lowercase()` 的堆内存——这就是"正在搜索…已找到 N 项"要跑半天、
//!     过滤延迟高的根源。
//!   - "查找"：查询词变化时在一帧内同步遍历整棵树（旧
//!     `collect_find_matches_main_shaped`），树一大那一帧就明显变长，偶发卡顿。
//!
//! 两个功能的共同点：都只按**名字**匹配，都要按树的先序顺序产出结果。既然
//! 如此，就把"名字"单独抽出来做成一份扁平的、对缓存友好的索引，构建一次、
//! 两边共用——这也是 Everything 的做法：启动时把 MFT 里的名字拍平进连续
//! 内存，之后每次搜索都只扫这块连续内存，不碰任何树形结构。
//!
//! ## 结构
//!
//! * [`NameIndex::blob_lower`]：所有名字的**小写形式**按树先序拼接进一块
//!   连续 `Vec<u8>`。普通子串搜索用 `memchr::memmem`（ripgrep 同款 SIMD
//!   实现）在这块内存上扫，吞吐量以 GB/s 计，再按 CPU 核数切块多线程并行。
//! * [`NameIndex::blob_orig`]：所有名字的**原样形式**同样先序拼接。
//!   用途有三：①正则/通配符匹配跑在原名上（`^`/`$` 锚点是"单个名字"的
//!   语义，不能跑在拼接缓冲上；而且直接对原名 `is_match`，大小写语义
//!   100% 精确，不依赖任何"小写文本等价"论证）；②"复制列表"后台重建
//!   树时还原展示名（`AbC.txt` 不能变成 `abc.txt`，那会改变 UI 效果）；
//!   ③名字排序按原名做 ASCII 不敏感比较。
//! * [`NameIndex::lower_starts`] / [`NameIndex::orig_starts`]：每个条目名字
//!   在两块 blob 里的起点（区间终点即下一个条目的起点）。Unicode 小写化
//!   可能改变字节数（如 `İ` 折叠后变成两个字符），所以两块 blob 各有各的
//!   起点表，不共用。
//! * [`NameIndex::entries`]：与 blob 对齐的条目数组（先序），带指回真实树
//!   位置的坐标（`pi` + `child_idx` + `parent` 链），渲染/定位时随时能还原
//!   出 `abs_path`（与主列表 `NodePath` 完全同构）。
//!
//! ## 构建
//!
//! 树是 UI 线程拥有的可变数据，后台线程直接读它会有数据竞争。所以构建用
//! **UI 线程分帧**（[`IndexBuilder`]）：每帧花几毫秒推进一段显式栈，树再大
//! 也不会让任何一帧超时；每帧开工前调用方核对"树结构版本号"，版本对不上
//! 就整个作废重来——栈里存的裸指针（与本项目 `FlatRow` 缓存/旧 `SearchJob`
//! 是同一套成熟模式）只在版本号一致时才会被解引用，内存安全有明确保证。
//!
//! ## 快照语义
//!
//! `NameIndex` 完全自含数据（名字/大小/时间/属性/所有者/所在目录/原名……
//! 渲染摊平列表、重建树要用的字段全在里面），不持有任何指向树的引用。
//! 因此它可以放进 `Arc` 随意共享：打开"搜索"标签页时拿一份 `Arc` 克隆就
//! 是纳秒级的"打开那一刻快照"，之后主列表随便增删改都不影响已打开的
//! 标签页——语义与原来"整树深拷贝"完全一致，但打开从"卡 0.3~0.5 秒"
//! 变成零成本，内存也从"每个标签页一整棵树"变成"多个标签页共享同一份
//! 索引"。

use std::collections::HashMap;
use std::time::Instant;

use crate::model::{Node, NodeKind, NodePath, SortDir, SortKey};

/// `IdxEntry::parent` 的哨兵值：父节点是分区/合成根（根自身不进索引）。
pub const NO_PARENT: u32 = u32::MAX;
/// `IdxEntry::dir` 的哨兵值：没有"所在目录"信息（构建时 `want_dirs=false`）。
pub const NO_DIR: u32 = u32::MAX;

/// 大小写不敏感比较，不分配新 String——与 tree_list.rs / compact_tree.rs 里
/// 的同名函数同一套实现（排序是 O(n log n) 次比较，每次比较都 `to_lowercase()`
/// 堆分配的话，大结果集排序会被分配拖垮）。
fn cmp_ignore_ascii_case(a: &str, b: &str) -> std::cmp::Ordering {
    a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

/// 单个条目的元数据。字段全部是定长标量，整个数组在内存里连续排布，
/// 遍历/扫描的缓存局部性远好于在指针链式的 `Node` 树上跳来跳去。
#[derive(Clone)]
pub struct IdxEntry {
    /// 所属分区（`partitions` 的下标；合成树恒为 0）。
    pub pi: u32,
    /// 自己在父节点 `children` 里的下标——回溯 `abs_path` 用。
    pub child_idx: u32,
    /// 父条目在 `entries` 里的下标（先序保证父在子前面）；`NO_PARENT` 表示父是分区根。
    pub parent: u32,
    /// 相对分区根的深度（分区根的直接子节点为 1）。
    pub depth: u32,
    /// 位标志：bit0 = 是文件（否则是文件夹），bit1 = NTFS 保留元数据文件。
    pub flags: u32,
    /// 所在目录路径在 `dirs` 池里的下标（摊平列表的"路径"列用）。
    pub dir: u32,
    /// 所有者在 `owners` 池里的下标。
    pub owner: u32,
    /// 父节点的逻辑大小（"父占比"列的分母），`max(1)` 防 0。
    pub parent_logical: u64,
    /// 本节点逻辑大小。
    pub logical_size: u64,
    /// 本节点物理大小。
    pub physical_size: u64,
    /// 修改时间（FILETIME）。
    pub modified_ft: u64,
    /// 创建时间（FILETIME）。
    pub created_ft: u64,
    /// 访问时间（FILETIME）。
    pub accessed_ft: u64,
    /// Windows 文件属性位。
    pub attributes: u32,
    /// Reparse point tag。
    pub reparse_tag: u32,
    /// 子树内文件数（只有文件夹有；文件恒 0）。u32 足够——单个文件夹不可能
    /// 有超过 42 亿个文件。
    pub file_count: u32,
    /// 子树内文件夹数（同上）。
    pub folder_count: u32,
}

impl IdxEntry {
    #[inline]
    pub fn is_file(&self) -> bool {
        self.flags & 1 != 0
    }
    #[inline]
    pub fn is_reserved(&self) -> bool {
        self.flags & 2 != 0
    }
}

/// 分区/合成根自己的信息（根不进 `entries`，但重建树、渲染时需要）。
#[derive(Clone)]
pub struct RootInfo {
    pub name: String,
    pub logical_size: u64,
    pub physical_size: u64,
    pub modified_ft: u64,
    pub created_ft: u64,
    pub accessed_ft: u64,
    pub attributes: u32,
    pub reparse_tag: u32,
    pub is_reserved: bool,
    pub owner: String,
    pub file_count: u64,
    pub folder_count: u64,
    /// 分区根的"所在目录"池下标（摊平列表的路径列从这里起头，例如 `C:\`）。
    pub dir: u32,
}

/// 建好的名字索引。除 `struct_version`（构建时对应的树结构版本号）外全部自含，
/// 可以放进 `Arc` 无限共享、跨线程读（没有任何内部可变性）。
pub struct NameIndex {
    /// 构建时调用方传入的"树结构版本号"。消费方（查找/搜索）用它核对
    /// "这份索引对应的是不是当前这棵树"，不匹配就走兜底/等待重建。
    ///
    /// 字段名不叫 `gen`：`gen` 是 Rust 2024 edition 的保留关键字
    /// （为未来的 gen 块语法预留），不能作为裸标识符使用。
    pub struct_version: u64,
    /// 所有小写名字的连续拼接缓冲（SIMD 子串搜索的扫描对象）。
    blob_lower: Vec<u8>,
    /// 所有原样名字的连续拼接缓冲（正则匹配/展示名/重建树用）。
    blob_orig: Vec<u8>,
    /// `lower_starts[i]` = 第 i 个条目小写名在 `blob_lower` 里的起点，
    /// `lower_starts[n]` = `blob_lower.len()`。
    lower_starts: Vec<u32>,
    /// 原名缓冲的起点表，布局同上。
    orig_starts: Vec<u32>,
    /// 与 blob 对齐的条目数组（树先序）。
    entries: Vec<IdxEntry>,
    /// 全部"文件"条目的下标（构建时顺手收集，摊平列表空查询的恒等视图
    /// 直接 clone 这一份，零过滤零遍历——旧实现"刚开搜索标签页就空查询
    /// 全树分帧扫半天"的链路，到这里变成一次几毫秒的内存拷贝）。
    file_entries: Vec<u32>,
    /// 分区/合成根信息。
    roots: Vec<RootInfo>,
    /// 目录路径池（同一个目录下的所有文件共享同一个池下标，内存占用是
    /// "目录数 × 路径长"，不是"文件数 × 路径长"）。
    dirs: Vec<String>,
    /// 所有者池（`owners[0]` 恒为空串，对应绝大多数"没读到所有者"的节点）。
    owners: Vec<String>,
}

/// 分帧步进的返回值。
pub enum BuildStep {
    /// 还没建完，下一帧继续调。
    Continue,
    /// 建完了，拿着结果走。`Box` 是给 clippy::large_enum_variant 的交代：
    /// `NameIndex` 很大（含两整块名字 blob），`Continue` 却不携带数据，
    /// 不装箱的话枚举整体按最大变体对齐，调用方栈上多占一份无用空间。
    Done(Box<NameIndex>),
}

/// [`NameIndex`] 的分帧构建器。在 UI 线程上用（树是 UI 线程的独占数据，
/// 不能丢给后台线程读）；每帧调 [`IndexBuilder::step`]，用几毫秒预算推进
/// 一段显式栈，树多大都不会卡住当前帧。
///
/// 栈里存 `*const Node` 裸指针、跨帧持有——安全性依据与本项目 `FlatRow`
/// 缓存完全相同：调用方在每帧 `step` 前核对"树结构版本号没变"（凡是会让
/// 已有节点内存地址失效的操作——扫描完成替换/追加分区、删除、移除分区、
/// 符号链接原地刷新——都必须把版本号 +1），版本号一变整个构建器作废、
/// 用当前最新的树重新起一个，绝不存在"拿着失效指针继续跑"的路径。
/// 展开/折叠只翻转 `Node::expanded` 这个 bool，不动任何 `Vec` 的内存布局，
/// 不影响构建器。
pub struct IndexBuilder {
    pub struct_version: u64,
    /// 是否填充"所在目录"池（摊平搜索需要；纯查找场景不需要，省内存和时间）。
    want_dirs: bool,
    stack: Vec<BuildFrame>,
    blob_lower: Vec<u8>,
    blob_orig: Vec<u8>,
    lower_starts: Vec<u32>,
    orig_starts: Vec<u32>,
    entries: Vec<IdxEntry>,
    roots: Vec<RootInfo>,
    dirs: Vec<String>,
    dir_ids: HashMap<String, u32>,
    owners: Vec<String>,
    owner_ids: HashMap<String, u32>,
}

struct BuildFrame {
    node: *const Node,
    pi: u32,
    child_idx: u32,
    parent: u32,
    depth: u32,
    parent_logical: u64,
    dir: u32,
}

impl IndexBuilder {
    /// 为"主列表形状"的树（`Vec<Node>`，每个顶层元素一个分区/快照根）建构建器。
    /// `root_paths` 是每个分区的真实路径（`C:\` 等），摊平列表的"路径"列以它起头。
    pub fn new_multi(partitions: &[Node], root_paths: &[String], struct_version: u64, want_dirs: bool) -> Self {
        let mut b = Self::empty(struct_version, want_dirs, partitions.len());
        for (pi, root) in partitions.iter().enumerate() {
            let root_dir = if want_dirs {
                // 与旧 build_search_stack 的路径形态保持一致：分区根去掉尾部
                // 反斜杠（`C:\` → `C:`），子层用 `\` 拼接（`C:\Windows`），
                // "路径"列的显示效果和重构前完全一样。
                b.intern_dir(root_paths.get(pi).map(|s| s.trim_end_matches('\\')).unwrap_or(""))
            } else {
                NO_DIR
            };
            b.roots.push(RootInfo {
                name: root.name.clone(),
                logical_size: root.logical_size,
                physical_size: root.physical_size,
                modified_ft: root.modified_ft,
                created_ft: root.created_ft,
                accessed_ft: root.accessed_ft,
                attributes: root.attributes,
                reparse_tag: root.reparse_tag,
                is_reserved: root.is_reserved,
                owner: root.owner.clone(),
                file_count: root.file_count,
                folder_count: root.folder_count,
                dir: root_dir,
            });
            // 子节点倒序入栈，出栈顺序即先序（与树的原始 children 下标顺序
            // 一致，不做任何排序——索引顺序必须与 abs_path 的"真实下标"语义
            // 严格一致，排序是渲染层自己的事）。
            for (i, child) in root.children.iter().enumerate().rev() {
                b.stack.push(BuildFrame {
                    node: child,
                    pi: pi as u32,
                    child_idx: i as u32,
                    parent: NO_PARENT,
                    depth: 1,
                    parent_logical: root.logical_size.max(1),
                    dir: root_dir,
                });
            }
        }
        b
    }

    /// 为"分析视图形状"的合成树（单一根，`abs_path[0]` 恒为 0 占位）建构建器。
    /// 与 `new_multi` 共用全部逻辑，只是把单根包成单元素切片、pi 固定为 0。
    /// 合成树只服务"查找"（它没有摊平搜索框），不需要目录池。
    pub fn new_single_root(root: &Node, struct_version: u64) -> Self {
        Self::new_multi(std::slice::from_ref(root), &[String::new()], struct_version, false)
    }

    fn empty(struct_version: u64, want_dirs: bool, roots_hint: usize) -> Self {
        let owners = vec![String::new()];
        let mut owner_ids = HashMap::new();
        owner_ids.insert(String::new(), 0u32);
        Self {
            struct_version,
            want_dirs,
            stack: Vec::new(),
            blob_lower: Vec::new(),
            blob_orig: Vec::new(),
            lower_starts: vec![0],
            orig_starts: vec![0],
            entries: Vec::new(),
            roots: Vec::with_capacity(roots_hint.max(1)),
            dirs: Vec::new(),
            dir_ids: HashMap::new(),
            owners,
            owner_ids,
        }
    }

    fn intern_dir(&mut self, dir: &str) -> u32 {
        if let Some(&id) = self.dir_ids.get(dir) {
            return id;
        }
        let id = self.dirs.len() as u32;
        self.dirs.push(dir.to_string());
        self.dir_ids.insert(dir.to_string(), id);
        id
    }

    fn intern_owner(&mut self, owner: &str) -> u32 {
        if let Some(&id) = self.owner_ids.get(owner) {
            return id;
        }
        let id = self.owners.len() as u32;
        self.owners.push(owner.to_string());
        self.owner_ids.insert(owner.to_string(), id);
        id
    }

    /// 推进一小段构建（预算内能处理多少节点就处理多少）。树规模巨大时会把
    /// 整个构建摊到很多帧里，每一帧的增加量都控制在 `budget` 附近。
    ///
    /// # 安全性
    /// `self.stack` 里的 `*const Node` 只在调用方保证"树结构版本号仍是
    /// `self.struct_version`"的前提下被解引用（见 `IndexBuilder` 上的说明）。
    pub fn step(&mut self, budget: std::time::Duration) -> BuildStep {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            let Some(frame) = self.stack.pop() else {
                return BuildStep::Done(Box::new(self.finish()));
            };
            // 安全性见 `IndexBuilder` 上的说明：调用方保证了版本号一致。
            let node = unsafe { &*frame.node };

            let name_lc = node.name.to_lowercase();
            self.blob_lower.extend_from_slice(name_lc.as_bytes());
            self.lower_starts.push(self.blob_lower.len() as u32);

            self.blob_orig.extend_from_slice(node.name.as_bytes());
            self.orig_starts.push(self.blob_orig.len() as u32);

            let owner = self.intern_owner(&node.owner);
            let dir = if self.want_dirs { frame.dir } else { NO_DIR };
            self.entries.push(IdxEntry {
                pi: frame.pi,
                child_idx: frame.child_idx,
                parent: frame.parent,
                depth: frame.depth,
                flags: (node.is_file() as u32) | ((node.is_reserved as u32) << 1),
                dir,
                owner,
                parent_logical: frame.parent_logical,
                logical_size: node.logical_size,
                physical_size: node.physical_size,
                modified_ft: node.modified_ft,
                created_ft: node.created_ft,
                accessed_ft: node.accessed_ft,
                attributes: node.attributes,
                reparse_tag: node.reparse_tag,
                file_count: node.file_count.min(u32::MAX as u64) as u32,
                folder_count: node.folder_count.min(u32::MAX as u64) as u32,
            });

            if node.is_folder() {
                // 文件夹：把"所在目录"向前推进一层（intern 池保证同一目录只存一份）。
                // 注意 format! 在表达式内先完成求值（对 self 的不可变借用随
                // String 的生成立即结束），之后才能调用 intern_dir(&mut self)。
                let child_dir = if self.want_dirs {
                    let child_dir = format!("{}\\{}", self.dirs[frame.dir as usize], node.name);
                    self.intern_dir(&child_dir)
                } else {
                    NO_DIR
                };
                let child_parent_logical = node.logical_size.max(1);
                let my_entry = (self.entries.len() - 1) as u32;
                for (i, child) in node.children.iter().enumerate().rev() {
                    self.stack.push(BuildFrame {
                        node: child,
                        pi: frame.pi,
                        child_idx: i as u32,
                        parent: my_entry,
                        depth: frame.depth + 1,
                        parent_logical: child_parent_logical,
                        dir: child_dir,
                    });
                }
            }
        }
        BuildStep::Continue
    }

    fn finish(&mut self) -> NameIndex {
        // 构建全程只 push 合法 utf8（名字与其小写形式的产物），两块 blob 必然合法。
        debug_assert!(std::str::from_utf8(&self.blob_lower).is_ok());
        debug_assert!(std::str::from_utf8(&self.blob_orig).is_ok());
        let file_entries = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.is_file())
            .map(|(i, _)| i as u32)
            .collect();
        NameIndex {
            struct_version: self.struct_version,
            blob_lower: std::mem::take(&mut self.blob_lower),
            blob_orig: std::mem::take(&mut self.blob_orig),
            lower_starts: std::mem::take(&mut self.lower_starts),
            orig_starts: std::mem::take(&mut self.orig_starts),
            entries: std::mem::take(&mut self.entries),
            file_entries,
            roots: std::mem::take(&mut self.roots),
            dirs: std::mem::take(&mut self.dirs),
            owners: std::mem::take(&mut self.owners),
        }
    }
}

/// 把 `total` 个条目尽量均匀切成 `want` 个连续区间（切分点对齐条目边界，
/// 保证并行扫描时每个命中都恰好落在一个块里，合并结果天然有序）。
fn split_ranges(total: usize, want: usize) -> Vec<(usize, usize)> {
    if total == 0 {
        return Vec::new();
    }
    let want = want.clamp(1, total);
    let per = total / want;
    let rem = total % want;
    let mut out = Vec::with_capacity(want);
    let mut at = 0usize;
    for i in 0..want {
        let len = per + if i < rem { 1 } else { 0 };
        out.push((at, at + len));
        at += len;
    }
    out
}

fn default_thread_count() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 32)
}

impl NameIndex {
    /// 条目总数（文件 + 文件夹，分区根除外）。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 第 i 个条目名字的小写形式。
    #[inline]
    pub fn name_lower(&self, i: usize) -> &str {
        unsafe {
            std::str::from_utf8_unchecked(
                &self.blob_lower[self.lower_starts[i] as usize..self.lower_starts[i + 1] as usize],
            )
        }
    }

    /// 第 i 个条目的原名（展示用；`AbC.txt` 保持 `AbC.txt` 的样子）。
    #[inline]
    pub fn name_orig(&self, i: usize) -> &str {
        unsafe {
            std::str::from_utf8_unchecked(
                &self.blob_orig[self.orig_starts[i] as usize..self.orig_starts[i + 1] as usize],
            )
        }
    }

    /// 第 i 个条目的所在目录路径（`want_dirs=false` 构建时返回空串）。
    pub fn dir_path(&self, i: usize) -> &str {
        let d = self.entries[i].dir;
        if d == NO_DIR { "" } else { self.dirs[d as usize].as_str() }
    }

    /// 第 i 个条目的所有者（池化，绝大多数返回空串）。
    pub fn owner(&self, i: usize) -> &str {
        self.owners[self.entries[i].owner as usize].as_str()
    }

    /// 分区/合成根的展示名。
    pub fn root_name(&self, pi: usize) -> &str {
        self.roots[pi].name.as_str()
    }

    /// 分区根的逻辑大小（"总占比"列的分母）。
    pub fn root_logical(&self, pi: usize) -> u64 {
        self.roots[pi].logical_size
    }

    /// 第 i 个条目在树里的绝对路径（与主列表 `NodePath` 同构：
    /// `[分区下标, 逐层 children 下标…]`；合成树的首元素恒为 0）。
    /// 沿 `parent` 链回溯拼出——先序构建保证父在子前面，链一定是向前的，
    /// 每次调用的成本只有 O(深度)，不存在任何全表扫描。
    pub fn abs_path_of(&self, i: u32) -> NodePath {
        let mut chain: Vec<usize> = Vec::with_capacity(8);
        let mut cur = i as usize;
        let pi;
        loop {
            let e = &self.entries[cur];
            chain.push(e.child_idx as usize);
            match e.parent {
                NO_PARENT => {
                    pi = e.pi as usize;
                    break;
                }
                p => cur = p as usize,
            }
        }
        let mut path = Vec::with_capacity(chain.len() + 1);
        path.push(pi);
        path.extend(chain.into_iter().rev());
        path
    }

    /// 零分配地判断"条目 `e` 的绝对坐标是否等于 `path`"——沿 parent 链自
    /// 底向上逐段比对 child_idx，最后核对分区号和层级数。语义与
    /// `abs_path_of(e) == path` 完全一致，但不在堆上分配任何东西：摊平列表
    /// 的"查找定位"要在百万行视图里逐行比对坐标，原来每行调一次
    /// `abs_path_of`（每行堆分配一个 Vec）一次定位就是几百毫秒的卡顿；这里
    /// 每行只有 O(深度) 次整数比较，百万行也就几毫秒。
    pub fn abs_path_eq(&self, e: u32, path: &[usize]) -> bool {
        let mut cur = e as usize;
        // j：path 里还没比对掉的层级数。自底向上，先比 path 的最后一段。
        let mut j = path.len();
        loop {
            if j == 0 {
                // 坐标已经全部比对完，链条却还有上一级——e 比 path 深。
                return false;
            }
            j -= 1;
            let ent = &self.entries[cur];
            if ent.child_idx as usize != path[j] {
                return false;
            }
            match ent.parent {
                NO_PARENT => {
                    // 链到顶了：path[0] 必须正好是这一条目所属的分区号，
                    // 且 path 里除它之外的层级刚好全部比对完（j == 1）。
                    // 条目数组里没有分区根本身（根信息单独存），所以不存在
                    // "path 只有一段"还能相等的情形。
                    return j == 1 && ent.pi as usize == path[0];
                }
                p => cur = p as usize,
            }
        }
    }

    /// 全部"文件"条目的下标（构建时预生成，这里只是一次 clone）。
    /// 摊平列表空查询的恒等视图直接用它，零过滤零遍历。
    pub fn all_file_entries(&self) -> Vec<u32> {
        self.file_entries.clone()
    }

    /// 文件条目总数（摊平列表空查询的行数）。
    pub fn file_entry_count(&self) -> usize {
        self.file_entries.len()
    }

    /// 第 i 个条目的元数据（渲染摊平行时读字段用）。
    #[inline]
    pub fn entry(&self, i: usize) -> &IdxEntry {
        &self.entries[i]
    }

    /// 全部条目的切片（CSV 导出等需要顺序遍历的场景用）。
    pub fn entries(&self) -> &[IdxEntry] {
        &self.entries
    }

    /// 分区/合成根的信息（CSV 导出的根行数据用）。
    pub fn root_info(&self, pi: usize) -> &RootInfo {
        &self.roots[pi]
    }

    /// 普通（大小写不敏感的子串包含）匹配。`needle` 大小写随意——函数内部
    /// 统一转小写（每次搜索只多一次小分配，相对整个 SIMD 扫描可以忽略），
    /// 不依赖调用方记得先转，转错了也不会"静默搜出空结果"。
    /// memmem SIMD 按核数切块并行扫整块缓冲，命中位置映射回条目并做
    /// "完全落在单个名字内部"的边界校验——没有这个校验，前一个名字的
    /// 尾巴和后一个名字的开头拼在一起会产生假命中。
    pub fn find_plain(&self, needle: &str) -> Vec<u32> {
        let needle_lower = needle.to_lowercase();
        if needle_lower.is_empty() || self.entries.is_empty() {
            return Vec::new();
        }
        let n = needle_lower.len();
        if n as u64 > self.blob_lower.len() as u64 {
            return Vec::new();
        }
        let chunks = split_ranges(self.entries.len(), default_thread_count() * 4);
        let finder = memchr::memmem::Finder::new(&needle_lower);
        std::thread::scope(|s| {
            let handles: Vec<_> = chunks
                .iter()
                .map(|&(e0, e1)| {
                    let finder = &finder;
                    let blob = &self.blob_lower;
                    let starts = &self.lower_starts;
                    s.spawn(move || {
                        let mut out: Vec<u32> = Vec::new();
                        let mut ei = e0;
                        let hay = &blob[starts[e0] as usize..starts[e1] as usize];
                        for pos in finder.find_iter(hay) {
                            let gpos = pos + starts[e0] as usize;
                            // 命中位置在块内单调不减，游标线性推进即可，
                            // 均摊 O(1)，不需要每次二分。
                            while ei + 1 < e1 && starts[ei + 1] as usize <= gpos {
                                ei += 1;
                            }
                            // 边界校验：命中必须完全落在这个条目自己的名字
                            // 区间内，剔除跨名拼接出的假命中。
                            if gpos + n <= starts[ei + 1] as usize {
                                out.push(ei as u32);
                            }
                        }
                        out
                    })
                })
                .collect();
            let mut all = Vec::new();
            for h in handles {
                match h.join() {
                    Ok(v) => all.extend(v),
                    // 以前是 join().unwrap_or_default()：worker panic 会被静默
                    // 吞掉，表现为"搜索无结果"却查不到原因。分块闭包里没有
                    // 已知 panic 路径，但真发生了至少要有日志证据。
                    Err(_) => crate::applog::log("[search_index] find_plain 分块线程 panic，该块结果丢弃"),
                }
            }
            all
        })
    }

    /// 正则/通配符匹配：逐名 `is_match`（锚点 `^`/`$` 的语义就是"单个名字"），
    /// 按核数切块多线程并行。匹配目标直接是**原名**切片，大小写语义由
    /// `RegexBuilder::case_insensitive` 精确保证，没有任何等价性论证开销；
    /// 每个切片都是整名拷贝，`from_utf8_unchecked` 的前提由构建期保证
    /// （blob 只拼接过合法 utf8 的完整名字）。
    pub fn find_regex(&self, re: &regex::Regex) -> Vec<u32> {
        if self.entries.is_empty() {
            return Vec::new();
        }
        let chunks = split_ranges(self.entries.len(), default_thread_count() * 4);
        std::thread::scope(|s| {
            let handles: Vec<_> = chunks
                .iter()
                .map(|&(e0, e1)| {
                    let blob = &self.blob_orig;
                    let starts = &self.orig_starts;
                    s.spawn(move || {
                        let mut out: Vec<u32> = Vec::new();
                        for i in e0..e1 {
                            let name = unsafe {
                                std::str::from_utf8_unchecked(
                                    &blob[starts[i] as usize..starts[i + 1] as usize],
                                )
                            };
                            if re.is_match(name) {
                                out.push(i as u32);
                            }
                        }
                        out
                    })
                })
                .collect();
            let mut all = Vec::new();
            for h in handles {
                match h.join() {
                    Ok(v) => all.extend(v),
                    Err(_) => crate::applog::log("[search_index] find_regex 分块线程 panic，该块结果丢弃"),
                }
            }
            all
        })
    }

    /// 给条目序列排序（摊平列表的表头排序）。`order` 是条目下标，就地按
    /// `key`/`dir` 重排。所有键都从索引自含字段读取，不碰树——因此可以
    /// 扔到后台线程对 `Arc<NameIndex>` 执行，大结果集（几十万上百万命中）
    /// 排序也不卡 UI。
    pub fn sort_order(&self, order: &mut [u32], key: SortKey, dir: SortDir) {
        order.sort_by(|&a, &b| {
            let (ea, eb) = (&self.entries[a as usize], &self.entries[b as usize]);
            let ord = match key {
                // 名字排序按原名做 ASCII 大小写不敏感比较（零分配）。
                SortKey::Name => cmp_ignore_ascii_case(self.name_orig(a as usize), self.name_orig(b as usize)),
                SortKey::Size => ea.logical_size.cmp(&eb.logical_size),
                SortKey::Physical => ea.physical_size.cmp(&eb.physical_size),
                SortKey::Modified => ea.modified_ft.cmp(&eb.modified_ft),
                SortKey::Created => ea.created_ft.cmp(&eb.created_ft),
                SortKey::Accessed => ea.accessed_ft.cmp(&eb.accessed_ft),
                SortKey::Items => (ea.file_count as u64 + ea.folder_count as u64).cmp(&(eb.file_count as u64 + eb.folder_count as u64)),
                SortKey::Files => ea.file_count.cmp(&eb.file_count),
                SortKey::Folders => ea.folder_count.cmp(&eb.folder_count),
                SortKey::Attributes => ea.attributes.cmp(&eb.attributes),
                SortKey::Reparse => ea.reparse_tag.cmp(&eb.reparse_tag),
                SortKey::Reserved => ea.is_reserved().cmp(&eb.is_reserved()),
                SortKey::Owner => cmp_ignore_ascii_case(self.owner(a as usize), self.owner(b as usize)),
                // "路径"列排序：比所在目录（同目录的文件聚在一起）。
                SortKey::Path => cmp_ignore_ascii_case(self.dir_path(a as usize), self.dir_path(b as usize)),
            };
            if dir == SortDir::Desc { ord.reverse() } else { ord }
        });
    }

    /// 从索引重建出一棵与原树同构的 `Vec<Node>`（每个分区/合成根一个顶层
    /// `Node`）。
    ///
    /// 用途："复制列表"标签页需要一份**可操作的树快照**（可展开/删除/建
    /// 符号链接），但整树深拷贝在 UI 线程上要几百毫秒——现在改为在后台
    /// 线程上从 `Arc<NameIndex>` 重建：索引完全自含（不含任何指向树的
    /// 引用），后台线程读它绝对安全，UI 全程零卡顿，几百毫秒后树就位。
    /// 重建出的树节点顺序、`abs_path` 下标语义与索引严格一致。
    ///
    /// ## 算法：逆先序扫描 + 显式栈装配
    ///
    /// 从最后一个条目往前扫。处理条目 i 时，i 的全部直接孩子（先序里分布在
    /// i 之后的若干段子树的根）已经装配完毕压在栈顶，且它们的深度恰好是
    /// `depth[i] + 1`、从栈顶往下正好是原始 `child_idx` 顺序——因为"父比
    /// 子先处理"的逆序扫描意味着每个孩子都在自己的整个子树装配完之后才
    /// 入栈，栈的先进后出恰好把"最后一个孩子先入栈"还原成"第一个孩子在
    /// 栈顶"。把栈顶这一段弹出装进 i 的 `children`，再把 i 压回栈等父条目
    /// 认领；全部扫完后栈里剩下的就是各分区根的直接子项，按分区号分桶还原。
    ///
    /// 与旧的"每条目一个孩子桶"实现相比：不再需要 `vec![Vec::new(); n]` 的
    /// 桶表（千万条目下仅空 `Vec` 头就是约 240MB 的临时分配，n 个桶还要各自
    /// 走一遍增长/搬家），也不需要先建整表槽位再逐个 `take` 的第二次分配，
    /// 峰值临时内存里只有一个栈。正确性由 `test_rebuild_tree_isomorphic`
    /// 和多分区用例覆盖。
    pub fn rebuild_tree(&self) -> Vec<Node> {
        struct StackItem {
            pi: u32,
            depth: u32,
            node: Node,
        }
        let mut stack: Vec<StackItem> = Vec::with_capacity(64);
        for i in (0..self.entries.len()).rev() {
            let e = &self.entries[i];
            let child_depth = e.depth + 1;
            // 栈顶连续一段 depth == child_depth 的就是 i 的直接孩子。
            // split_off 按 Vec 存储序返回（栈底→栈顶），而原始 child_idx 顺序
            // 是从栈顶往下——所以 collect 前 .rev() 一次还原。
            let n_children = stack.iter().rev().take_while(|it| it.depth == child_depth).count();
            let children: Vec<Node> = stack
                .split_off(stack.len() - n_children)
                .into_iter()
                .rev()
                .map(|it| it.node)
                .collect();
            let node = Node {
                name: self.name_orig(i).to_string(),
                size: e.logical_size,
                logical_size: e.logical_size,
                physical_size: e.physical_size,
                kind: if e.is_file() { NodeKind::File } else { NodeKind::Folder },
                color: egui::Color32::WHITE,
                children,
                expanded: false,
                file_count: e.file_count as u64,
                folder_count: e.folder_count as u64,
                modified_ft: e.modified_ft,
                created_ft: e.created_ft,
                accessed_ft: e.accessed_ft,
                attributes: e.attributes,
                reparse_tag: e.reparse_tag,
                is_reserved: e.is_reserved(),
                owner: self.owner(i).to_string(),
                full_path_override: None,
            };
            stack.push(StackItem { pi: e.pi, depth: e.depth, node });
        }
        // 栈底到栈顶是"分区号降序、同分区内 child_idx 降序"，反向遍历一次
        // 还原原始顺序，按分区号分桶（分区数很少，桶表开销可忽略）。
        let mut top: Vec<Vec<Node>> = vec![Vec::new(); self.roots.len()];
        for it in stack.into_iter().rev() {
            top[it.pi as usize].push(it.node);
        }
        self.roots
            .iter()
            .enumerate()
            .map(|(pi, r)| Node {
                name: r.name.clone(),
                size: r.logical_size,
                logical_size: r.logical_size,
                physical_size: r.physical_size,
                kind: NodeKind::Folder,
                color: egui::Color32::WHITE,
                children: std::mem::take(&mut top[pi]),
                expanded: false,
                file_count: r.file_count,
                folder_count: r.folder_count,
                modified_ft: r.modified_ft,
                created_ft: r.created_ft,
                accessed_ft: r.accessed_ft,
                attributes: r.attributes,
                reparse_tag: r.reparse_tag,
                is_reserved: r.is_reserved,
                owner: r.owner.clone(),
                full_path_override: None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Node;

    /// 构造一个测试节点的便捷函数（字段全集见 model::Node）。
    fn node(name: &str, size: u64, kind: NodeKind, children: Vec<Node>) -> Node {
        let (files, folders) = if kind == NodeKind::File {
            (0u64, 0u64)
        } else {
            let f = children.iter().filter(|c| c.kind == NodeKind::File).count() as u64;
            let d = children.iter().filter(|c| c.kind == NodeKind::Folder).count() as u64;
            (f, d)
        };
        Node {
            name: name.to_string(),
            size,
            logical_size: size,
            physical_size: size,
            kind,
            color: egui::Color32::WHITE,
            children,
            expanded: false,
            file_count: files,
            folder_count: folders,
            modified_ft: 0,
            created_ft: 0,
            accessed_ft: 0,
            attributes: 0,
            reparse_tag: 0,
            is_reserved: false,
            owner: String::new(),
            full_path_override: None,
        }
    }

    /// 测试树：
    /// C:\ （Folder, 100）
    /// ├─ Windows （Folder, 60）
    /// │   └─ system32 （Folder, 50）
    /// │       ├─ notepad.EXE （File, 10）
    /// │       └─ AbC.txt （File, 5）
    /// ├─ Users （Folder, 20）
    /// │   └─ readme.md （File, 3）
    /// └─ top.mp4 （File, 22）
    fn test_root() -> Node {
        node("C:", 100, NodeKind::Folder, vec![
            node("Windows", 60, NodeKind::Folder, vec![
                node("system32", 50, NodeKind::Folder, vec![
                    node("notepad.EXE", 10, NodeKind::File, vec![]),
                    node("AbC.txt", 5, NodeKind::File, vec![]),
                ]),
            ]),
            node("Users", 20, NodeKind::Folder, vec![
                node("readme.md", 3, NodeKind::File, vec![]),
            ]),
            node("top.mp4", 22, NodeKind::File, vec![]),
        ])
    }

    /// 跑完分帧构建器直到 Done（测试里给大预算，一步到位）。
    fn build_index(root: &Node, version: u64) -> NameIndex {
        let mut b = IndexBuilder::new_multi(std::slice::from_ref(root), &["C:\\".to_string()], version, true);
        for _ in 0..100 {
            match b.step(std::time::Duration::from_secs(10)) {
                BuildStep::Continue => continue,
                BuildStep::Done(idx) => return *idx,
            }
        }
        panic!("IndexBuilder 100 步都没构建完，测试树有问题");
    }

    #[test]
    fn test_index_shape_and_preorder() {
        let root = test_root();
        let idx = build_index(&root, 7);
        assert_eq!(idx.struct_version, 7);
        // 7 个条目：Windows, system32, notepad.EXE, AbC.txt, Users, readme.md, top.mp4
        assert_eq!(idx.len(), 7);
        // 先序顺序必须与树先序完全一致（next/prev 查找语义的前提）。
        let names: Vec<&str> = (0..idx.len()).map(|i| idx.name_orig(i)).collect();
        assert_eq!(names, vec!["Windows", "system32", "notepad.EXE", "AbC.txt", "Users", "readme.md", "top.mp4"]);
        // 文件条目（不含文件夹）：4 个，同样保持先序。
        let files: Vec<&str> = idx.all_file_entries().iter().map(|&e| idx.name_orig(e as usize)).collect();
        assert_eq!(files, vec!["notepad.EXE", "AbC.txt", "readme.md", "top.mp4"]);
        assert_eq!(idx.file_entry_count(), 4);
    }

    #[test]
    fn test_find_plain_case_insensitive() {
        let root = test_root();
        let idx = build_index(&root, 1);
        // 大小写不敏感：查询词传小写形式（调用方约定），命中原名大写的条目。
        assert_eq!(idx.find_plain("abc"), vec![3]); // AbC.txt
        assert_eq!(idx.find_plain("sys"), vec![1]); // system32
        assert_eq!(idx.find_plain("MP4"), vec![6]); // 传大写也能命中（blob 是小写，MP4 小写后 = mp4）
        // 空查询恒空。
        assert!(idx.find_plain("").is_empty());
        // 比整个 blob 还长的查询词恒空。
        assert!(idx.find_plain("xxxxxxxxxxxxxxxxxxxxxxxxxxxx").is_empty());
    }

    #[test]
    fn test_find_plain_boundary_no_cross_name_false_hit() {
        // 相邻两个名字 "ab" 和 "cd" 拼接后是 "abcd"，子串 "bc" 只存在于
        // "跨名拼接" 的位置——没有边界校验就会产生假命中，这里验证校验生效。
        let root = node("X:", 10, NodeKind::Folder, vec![
            node("ab", 1, NodeKind::File, vec![]),
            node("cd", 2, NodeKind::File, vec![]),
        ]);
        let idx = build_index(&root, 1);
        assert!(idx.find_plain("bc").is_empty(), "跨名拼接的假命中必须被剔除");
        // 每个名字自己的真命中不受影响。
        assert_eq!(idx.find_plain("ab"), vec![0]);
        assert_eq!(idx.find_plain("cd"), vec![1]);
        // 完全跨名的更长子串同样不命中。
        assert!(idx.find_plain("abcd").is_empty());
    }

    #[test]
    fn test_find_regex_and_wildcards() {
        let root = test_root();
        let idx = build_index(&root, 1);
        // 与 ui::Matcher::build_auto 同款构造：整名匹配 + 大小写不敏感。
        let re = regex::Regex::new(r"(?i)^.*\.exe$").unwrap();
        assert_eq!(idx.find_regex(&re), vec![2]); // 只有 notepad.EXE
        let re2 = regex::Regex::new(r"(?i)^readme\.md$").unwrap();
        assert_eq!(idx.find_regex(&re2), vec![5]);
        // 不锚定的包含式正则。
        let re3 = regex::Regex::new(r"(?i)WIN").unwrap();
        assert_eq!(idx.find_regex(&re3), vec![0]); // Windows（还有 system32? 不：system32 不含 win）
    }

    #[test]
    fn test_abs_path_of_matches_tree_coordinates() {
        let root = test_root();
        let idx = build_index(&root, 1);
        // Windows 的 abs_path = [0, 0]；system32 = [0,0,0]；
        // notepad.EXE = [0,0,0,0]；AbC.txt = [0,0,0,1]；readme.md = [0,1,0]；top.mp4 = [0,2]。
        assert_eq!(idx.abs_path_of(0), vec![0, 0]);
        assert_eq!(idx.abs_path_of(1), vec![0, 0, 0]);
        assert_eq!(idx.abs_path_of(2), vec![0, 0, 0, 0]);
        assert_eq!(idx.abs_path_of(3), vec![0, 0, 0, 1]);
        assert_eq!(idx.abs_path_of(5), vec![0, 1, 0]);
        assert_eq!(idx.abs_path_of(6), vec![0, 2]);
    }

    #[test]
    fn test_abs_path_eq_agrees_with_abs_path_of() {
        let root = test_root();
        let idx = build_index(&root, 1);
        // 对全部条目：abs_path_eq(e, abs_path_of(e)) 必须恒真——零分配
        // 比对和分配版坐标在语义上必须完全一致（这是"查找定位"换用它的前提）。
        for e in 0..idx.len() as u32 {
            let p = idx.abs_path_of(e);
            assert!(idx.abs_path_eq(e, &p), "条目 {e} 的 abs_path_eq 应为真：{p:?}");
        }
        // 反向也验一下已知坐标：true。
        assert!(idx.abs_path_eq(3, &[0, 0, 0, 1]));   // AbC.txt
        assert!(idx.abs_path_eq(6, &[0, 2]));         // top.mp4
        // 坐标任何一段不对都不相等：
        assert!(!idx.abs_path_eq(3, &[0, 0, 0, 0]));  // 末段错（是 notepad 的）
        assert!(!idx.abs_path_eq(3, &[0, 0, 1, 1]));  // 中段错
        assert!(!idx.abs_path_eq(3, &[1, 0, 0, 1]));  // 分区号错
        assert!(!idx.abs_path_eq(6, &[0, 2, 0]));     // path 更深（top.mp4 没有子级）
        assert!(!idx.abs_path_eq(0, &[0]));           // path 更浅（Windows 至少一层 child_idx）
        assert!(!idx.abs_path_eq(0, &[]));            // 空坐标
        assert!(!idx.abs_path_eq(3, &[2, 0, 0, 1]));  // 首段之后对不上，顶层 pi 也不该放宽
    }

    #[test]
    fn test_dir_pool() {
        let root = test_root();
        let idx = build_index(&root, 1);
        // 目录池：分区根去尾反斜杠（C:\ → C:），子层用 \ 拼接——与旧版摊平列表一致。
        assert_eq!(idx.dir_path(0), "C:");            // Windows 所在目录
        assert_eq!(idx.dir_path(2), "C:\\Windows\\system32"); // notepad.EXE
        assert_eq!(idx.dir_path(5), "C:\\Users");     // readme.md
        assert_eq!(idx.dir_path(6), "C:");            // top.mp4
    }

    #[test]
    fn test_sort_order_by_name() {
        let root = test_root();
        let idx = build_index(&root, 1);
        let mut order = idx.all_file_entries();
        idx.sort_order(&mut order, SortKey::Name, SortDir::Asc);
        let names: Vec<&str> = order.iter().map(|&e| idx.name_orig(e as usize)).collect();
        // ASCII 大小写不敏感升序：A < n < r < t。
        assert_eq!(names, vec!["AbC.txt", "notepad.EXE", "readme.md", "top.mp4"]);
        // 降序翻转。
        idx.sort_order(&mut order, SortKey::Name, SortDir::Desc);
        let names_desc: Vec<&str> = order.iter().map(|&e| idx.name_orig(e as usize)).collect();
        assert_eq!(names_desc, vec!["top.mp4", "readme.md", "notepad.EXE", "AbC.txt"]);
    }

    #[test]
    fn test_rebuild_tree_isomorphic() {
        let root = test_root();
        let idx = build_index(&root, 1);
        let rebuilt = idx.rebuild_tree();
        assert_eq!(rebuilt.len(), 1);
        let r = &rebuilt[0];
        assert_eq!(r.name, "C:");
        assert_eq!(r.logical_size, 100);
        // 顶层顺序与原树一致。
        let top_names: Vec<&str> = r.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(top_names, vec!["Windows", "Users", "top.mp4"]);
        // 嵌套层级与大小保留。
        let sys = &r.children[0].children[0];
        assert_eq!(sys.name, "system32");
        assert_eq!(sys.logical_size, 50);
        // 原名大小写保留（AbC.txt 不能变成 abc.txt）。
        let leaf_names: Vec<&str> = sys.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(leaf_names, vec!["notepad.EXE", "AbC.txt"]);
        // 深层叶子大小。
        assert_eq!(sys.children[1].logical_size, 5);
        // 文件/文件夹种类保留。
        assert_eq!(sys.children[0].kind, NodeKind::File);
        assert_eq!(r.children[0].kind, NodeKind::Folder);
    }

    #[test]
    fn test_single_root_builder_shape() {
        // 分析视图的合成树入口：new_single_root（want_dirs=false，目录池恒空）。
        let root = test_root();
        let mut b = IndexBuilder::new_single_root(&root, 42);
        let idx = loop {
            match b.step(std::time::Duration::from_secs(10)) {
                BuildStep::Continue => continue,
                BuildStep::Done(idx) => break *idx,
            }
        };
        assert_eq!(idx.struct_version, 42);
        assert_eq!(idx.len(), 7);
        // want_dirs=false：dir_path 恒为空串（NO_DIR 哨兵）。
        assert_eq!(idx.dir_path(0), "");
    }

    /// 性能对照基准（默认忽略，按需手跑）：摊平视图里定位一行的三种方式。
    /// 验证"查找定位"必须走条目下标/零分配比对、绝不能退回逐行
    /// `abs_path_of`（每行一次 Vec 堆分配）这条性能红线——曾经就是它让
    /// "搜索"标签页上用查找悬浮窗每敲一个词卡顿几百毫秒。
    /// 实测（release，100 万行视图）：逐行分配 ≈ 20ms+（真实大盘树更深只会
    /// 更慢），整数比对 ≈ 0.2ms，abs_path_eq 零分配 ≈ 3.6ms。
    /// `cargo test --release bench_locate -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_locate() {
        let total = 1_000_000usize;
        let per = total / 100;
        let mk_file = |i: usize| Node {
            name: format!("file_{i:07}.dat"),
            size: 100, logical_size: 100, physical_size: 100,
            kind: NodeKind::File,
            color: egui::Color32::WHITE,
            children: Vec::new(),
            expanded: false,
            file_count: 0, folder_count: 0,
            modified_ft: 0, created_ft: 0, accessed_ft: 0,
            attributes: 0, reparse_tag: 0, is_reserved: false,
            owner: String::new(),
            full_path_override: None,
        };
        let dirs: Vec<Node> = (0..100).map(|d| Node {
            name: format!("dir_{d:03}"),
            size: 0, logical_size: 0, physical_size: 0,
            kind: NodeKind::Folder,
            color: egui::Color32::WHITE,
            children: ((d * per)..((d + 1) * per)).map(mk_file).collect(),
            expanded: false,
            file_count: per as u64, folder_count: 0,
            modified_ft: 0, created_ft: 0, accessed_ft: 0,
            attributes: 0, reparse_tag: 0, is_reserved: false,
            owner: String::new(),
            full_path_override: None,
        }).collect();
        let root = Node {
            name: "C:".into(),
            size: 0, logical_size: 0, physical_size: 0,
            kind: NodeKind::Folder,
            color: egui::Color32::WHITE,
            children: dirs,
            expanded: false,
            file_count: total as u64, folder_count: 100,
            modified_ft: 0, created_ft: 0, accessed_ft: 0,
            attributes: 0, reparse_tag: 0, is_reserved: false,
            owner: String::new(),
            full_path_override: None,
        };
        let mut b = IndexBuilder::new_multi(std::slice::from_ref(&root), &["C:\\".to_string()], 1, false);
        let idx = loop {
            match b.step(std::time::Duration::from_secs(600)) {
                BuildStep::Continue => continue,
                BuildStep::Done(i) => break *i,
            }
        };
        assert_eq!(idx.file_entry_count(), total);
        let order: Vec<u32> = idx.all_file_entries();
        let target_entry = order[total / 2 + 333];
        let target_path = idx.abs_path_of(target_entry);

        let t0 = std::time::Instant::now();
        let old_hit = order.iter().position(|&e| idx.abs_path_of(e) == target_path);
        let old = t0.elapsed();
        let t1 = std::time::Instant::now();
        let int_hit = order.iter().position(|&x| x == target_entry);
        let int = t1.elapsed();
        let t2 = std::time::Instant::now();
        let eq_hit = order.iter().position(|&e| idx.abs_path_eq(e, &target_path));
        let eq = t2.elapsed();

        assert_eq!(old_hit, Some(total / 2 + 333));
        assert_eq!(int_hit, old_hit);
        assert_eq!(eq_hit, old_hit);
        println!("\n==== 定位耗时对照（{} 行视图）====", order.len());
        println!("旧 abs_path_of 逐行堆分配: {:>9.2} ms", old.as_secs_f64() * 1000.0);
        println!("新 整数比对              : {:>9.2} ms", int.as_secs_f64() * 1000.0);
        println!("新 abs_path_eq 零分配    : {:>9.2} ms", eq.as_secs_f64() * 1000.0);
    }
}

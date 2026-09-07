pub mod compact_tree;
pub mod sidebar;
pub mod startup;
pub mod topbar;
pub mod tree_list;

use crate::model::NodePath;
use crate::search_index::NameIndex;

#[derive(Debug, Clone)]
pub enum TreeAction {
    None,
    Select(NodePath),
    ToggleExpand(NodePath),
    #[allow(dead_code)]
    EnterNode(NodePath),
    /// 右键菜单点了"删除到回收站"：只是发出请求，真正执行前 app.rs 要弹确认框。
    /// 带上名称/完整路径/是否文件夹，是因为确认框要展示这些信息，而 app.rs
    /// 侧不方便（也没必要）重新从 abs_path 沿树走一遍去拿。
    ///
    /// `index_entry`：这一行来自索引摊平列表（"搜索"标签页）时是它的条目
    /// 下标——删除成功后 app.rs 用它把这一行从该标签页的显示里剔除；
    /// 树模式的行不来自索引，恒为 `None`。
    RequestDelete { abs_path: NodePath, name: String, full_path: String, is_folder: bool, index_entry: Option<u32> },
    /// 右键菜单点了"检测占用"：查一下这个文件/文件夹当前有没有被别的进程/
    /// 服务占用。这是纯只读查询（不会像删除那样需要弹确认框），app.rs 收到
    /// 之后直接查、弹一个结果窗口。
    RequestCheckLock { abs_path: NodePath, name: String, full_path: String, is_folder: bool },
    /// 重复文件分组行右键"检测占用（整组）"：一次性查组里所有文件，不用
    /// 一个一个手动右键检测——见 app.rs 里 `LockCheckRequest::is_group`
    /// 分支的说明。
    RequestCheckLockGroup { abs_path: NodePath, name: String },
    /// 右键菜单点了"创建符号链接"（主列表的单个文件/文件夹）：弹原生文件夹
    /// 选择框选目标位置，然后在后台线程执行真正的迁移+建链接。
    RequestCreateSymlink { abs_path: NodePath, name: String, full_path: String, is_folder: bool },
    /// 重复文件分组行点了"创建符号链接"：只带分组自己的 abs_path，具体组里
    /// 有哪些文件由 app.rs 沿着树查（这个分组节点的直接子节点就是各个副本，
    /// 每个子节点的 `full_path_override` 已经是真实磁盘路径）。
    RequestCreateSymlinkGroup { abs_path: NodePath, name: String },
    /// 磁盘/根目录行右键"重新扫描"：只带分区下标，真正的扫描逻辑复用 app.rs
    /// 里"新增扫描"那一套（`scan::spawn_scan`），结果原地替换而不是追加。
    RequestRescan(usize),
    /// 磁盘/根目录行右键"从列表移除"：只是从 `self.partitions` 里摘掉，不碰
    /// 磁盘上的任何文件，想再看到重新扫描一次就行。
    RequestRemovePartition(usize),
    /// 磁盘/根目录行右键"文件扩展名分类"：只看这一个分区（跟顶部菜单进入的
    /// "全部分区一起看"是两种不同的作用范围，见 app.rs 里 `Tab` 上的说明）。
    RequestExtensionBreakdown(usize),
    /// 磁盘/根目录行右键"重复文件查找"：同上，只看这一个分区。
    RequestDuplicateFinder(usize),
}

/// 搜索框的匹配规则：普通子串包含（大小写不敏感），或者正则表达式（同样大小写
/// 不敏感）。`tree_list.rs`（主列表）和 `compact_tree.rs`（扩展名分类/重复文件
/// 查找）两处搜索框共用同一套规则，不用各写一份、行为还可能悄悄不一致。
/// 逐字符大小写折叠比较（不分配）。`char::to_lowercase` 是流式迭代器，
/// 不像 `str::to_lowercase` 那样每次都堆分配一个新 String。
fn chars_eq_ci(a: char, b: char) -> bool {
    let mut ai = a.to_lowercase();
    let mut bi = b.to_lowercase();
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if x == y => {}
            _ => return false,
        }
    }
}

/// 大小写不敏感的"包含"匹配，零堆分配：在 `name` 上滑动窗口逐字符折叠比对。
/// `needle` 必须已经用 [`fold_lowercase`] 折叠过（`Matcher::Plain` 存的就是
/// 折叠后的形式）。兑底遍历路径（索引未就绪时）会对全树每个节点调一次，
/// 以前用 `name.to_lowercase().contains(q)` 每节点堆分配一个 String，
/// 几十万节点的树上一次搜索就是几十万次分配——这是最后一个漏改的点
/// （tree_list/compact_tree 的排序比较早已是零分配版本）。
fn contains_ignore_case(name: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let needle_chars: Vec<char> = needle.chars().collect();
    let Some(&first) = needle_chars.first() else { return true };
    'outer: for (start, c) in name.char_indices() {
        if !chars_eq_ci(c, first) {
            continue;
        }
        let mut nc = name[start + c.len_utf8()..].chars();
        for &n in &needle_chars[1..] {
            match nc.next() {
                Some(m) if chars_eq_ci(m, n) => {}
                _ => continue 'outer,
            }
        }
        return true;
    }
    false
}

/// 逐字符折叠成小写（与 [`contains_ignore_case`] 的比较语义严格自洽），
/// 只在构建 Matcher 时调一次，不在这个函数的热路径上。
fn fold_lowercase(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

pub enum Matcher {
    Plain(String),
    Regex(regex::Regex),
}

impl Matcher {
    /// `query` 为空字符串时不应该调用这个函数（调用方在外面用
    /// `!query.trim().is_empty()` 判断"是否处于搜索模式"），这里不重复检查。
    ///
    /// 目前所有搜索入口都走 `build_auto`（通配符自动识别），这个显式
    /// 开关版本暂时没有调用方——保留着，将来要在某个列表上恢复
    /// "正则表达式"勾选开关时直接用，索引侧的 `find_regex` 路径也是现成的。
    #[allow(dead_code)]
    pub fn build(query: &str, use_regex: bool) -> Result<Self, String> {
        if use_regex {
            regex::RegexBuilder::new(query)
                .case_insensitive(true)
                .build()
                .map(Matcher::Regex)
                .map_err(|e| format!("正则表达式有误：{e}"))
        } else {
            Ok(Matcher::Plain(fold_lowercase(query)))
        }
    }

    /// "查找"悬浮窗用：不需要用户开任何开关，自动识别通配符——查询词里带
    /// `*`（匹配任意长度的任意字符）或 `?`（匹配单个任意字符）就自动按
    /// 通配符模式匹配整个名字（比如 `*.pid` 只匹配以 `.pid` 结尾的文件），
    /// 没有这两个字符就还是最简单的大小写不敏感包含匹配。这个函数是
    /// 不会失败的（通配符转出来的正则由 `regex::escape` 逐段拼接、`*`/`?`
    /// 换成 `.*`/`.`，构造上保证一定能编译成功），不像 `build()` 那样要处理
    /// 用户手写正则可能写错的情况——"查找"就是要简单，不给用户暴露"正则
    /// 语法错误"这种需要额外 UI 展示的复杂状态。
    pub fn build_auto(query: &str) -> Self {
        if query.contains('*') || query.contains('?') {
            let mut pattern = String::from("(?i)^");
            for ch in query.chars() {
                match ch {
                    '*' => pattern.push_str(".*"),
                    '?' => pattern.push('.'),
                    c => pattern.push_str(&regex::escape(&c.to_string())),
                }
            }
            pattern.push('$');
            match regex::Regex::new(&pattern) {
                Ok(re) => Matcher::Regex(re),
                // 理论上到不了这里（上面的构造方式保证一定合法），万一真的
                // 出了没预料到的问题，退化成普通包含匹配兜底，不让"查找"
                // 直接失效。
                Err(_) => Matcher::Plain(fold_lowercase(query)),
            }
        } else {
            Matcher::Plain(fold_lowercase(query))
        }
    }

    pub fn is_match(&self, name: &str) -> bool {
        match self {
            Matcher::Plain(q) => contains_ignore_case(name, q),
            Matcher::Regex(re) => re.is_match(name),
        }
    }

    /// 直接在名字索引上匹配，返回命中的条目下标（索引先序）。普通子串走
    /// memmem SIMD 多线程扫连续缓冲（search_index.rs），正则/通配符走逐名
    /// 并行 is_match——两条路都不碰树、不逐节点堆分配，这是"查找"和
    /// "搜索"共用的高性能入口。
    pub fn find_in_index(&self, index: &NameIndex) -> Vec<u32> {
        match self {
            Matcher::Plain(q) => index.find_plain(q),
            Matcher::Regex(re) => index.find_regex(re),
        }
    }
}

/// 主列表（tree_list）可排序的字段。定义已下沉到 lib 侧的 model
/// （搜索索引的后台排序线程也在 lib 里，要用同一套键值；lib 引用不到
/// bin 的 `ui` 模块），这里 re-export 保持原有 `super::SortKey` 的引用
/// 方式不变。
pub use crate::model::{SortDir, SortKey, SortState};

//! 按文件类型分类统计。

use egui::Color32;
use std::collections::HashMap;
use crate::model::{Node, NodeKind};

const LABELS: [&str; 6] = ["视频", "压缩包", "程序/exe", "文档", "图片", "其他"];
const COLORS: [Color32; 6] = [
    Color32::from_rgb(0xE0, 0x55, 0x5B), Color32::from_rgb(0xF5, 0xA6, 0x23),
    crate::theme::ACCENT_BLUE, Color32::from_rgb(0x34, 0xC7, 0x59),
    Color32::from_rgb(0x9C, 0x6A, 0xDE), crate::theme::FILE_COLOR,
];

/// 把文件名拆成"无扩展名的主体 + 扩展名"。**没有扩展名时返回 None**——
/// 项目里所有跟"扩展名"有关的地方（侧边栏分类统计、扩展名分类标签页）
/// 都必须用这同一个函数，不能各自再写一份：以前是两套逻辑，`.mp4` 这种
/// "整个名字就是一个以点开头的文件名"的 dotfile，在侧边栏被当成视频
/// （rsplit 后扩展名是 mp4），在扩展名分类页却被归进"（无扩展名）"
/// （主体为空不算有扩展名）——同一个文件在两个视图里归属不一致。
/// Windows 资源管理器对 dotfile 的语义就是"没有扩展名"，这里统一按这个
/// 语义来：主体为空（`.mp4`）、扩展名为空（`abc.`）都算没有扩展名。
pub fn split_extension(name: &str) -> Option<(&str, &str)> {
    let (base, ext) = name.rsplit_once('.')?;
    if base.is_empty() || ext.is_empty() {
        return None;
    }
    Some((base, ext))
}

fn classify(name: &str) -> usize {
    let ext = split_extension(name).map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("mp4"|"mkv"|"avi"|"mov"|"wmv"|"flv") => 0,
        Some("zip"|"rar"|"7z"|"tar"|"gz"|"xz") => 1,
        Some("exe"|"msi"|"dll"|"bat"|"sh"|"app") => 2,
        Some("doc"|"docx"|"pdf"|"txt"|"md"|"xlsx"|"pptx") => 3,
        Some("png"|"jpg"|"jpeg"|"gif"|"bmp"|"webp"|"svg") => 4,
        _ => 5,
    }
}

fn accumulate(node: &Node, totals: &mut [u64; 6]) {
    // 迭代版本：用显式栈代替原生递归，不管扫描到的目录树多深，都不会有栈溢出的可能性。
    let mut stack: Vec<&Node> = vec![node];
    while let Some(cur) = stack.pop() {
        match cur.kind {
            NodeKind::File => totals[classify(&cur.name)] += cur.logical_size,
            NodeKind::Folder => stack.extend(cur.children.iter()),
        }
    }
}

pub fn compute_categories(root: &Node) -> Vec<crate::model::CategoryStat> {
    let mut totals = [0u64; 6];
    accumulate(root, &mut totals);
    (0..6).map(|i| crate::model::CategoryStat { label: LABELS[i], size: totals[i], color: COLORS[i] }).collect()
}

/// 按扩展名分组收集文件（不建 Node 树，只是分组）——单分区/多分区两种场景
/// 共用这一步：多分区场景直接把好几个分区的分组结果合并到同一个
/// `HashMap`（按扩展名累加），而不是每个分区各自先建出一棵完整的树、
/// 再费劲从"`.mp4（123 个文件）`"这种已经拼好计数文字的名字里解析出
/// 纯扩展名合并回去——那样既绕、又脆弱（"（无扩展名）"这个分类的名字
/// 本身也用全角括号，容易和计数后缀的括号搞混）。
fn group_files_by_extension(root: &Node, root_path: &str, groups: &mut HashMap<String, Vec<Node>>) {
    let mut stack: Vec<(&Node, String)> = vec![(root, root_path.trim_end_matches('\\').to_string())];
    while let Some((cur, path)) = stack.pop() {
        match cur.kind {
            NodeKind::File => {
                let ext = split_extension(&cur.name)
                    .map(|(_, e)| e.to_ascii_lowercase())
                    .unwrap_or_else(|| "（无扩展名）".to_string());
                let leaf = cur.clone().with_full_path(path.clone());
                groups.entry(ext).or_default().push(leaf);
            }
            NodeKind::Folder => {
                for child in &cur.children {
                    let child_path = if path.is_empty() { child.name.clone() } else { format!("{path}\\{}", child.name) };
                    stack.push((child, child_path));
                }
            }
        }
    }
}

fn build_extension_tree_from_groups(groups: HashMap<String, Vec<Node>>) -> Node {
    let ext_folders: Vec<Node> = groups.into_iter().map(|(ext, files)| {
        let display_ext = if ext.starts_with('（') { ext.clone() } else { format!(".{ext}") };
        let count = files.len();
        Node::new_folder_with_meta(
            format!("{display_ext}（{count} 个文件）"),
            GROUP_COLOR, files, 0, 0, 0, crate::fs_attrs::FILE_ATTRIBUTE_DIRECTORY, 0, false, String::new(),
        )
    }).collect();
    Node::new_folder_with_meta("按扩展名分类".to_string(), GROUP_COLOR, ext_folders, 0, 0, 0, crate::fs_attrs::FILE_ATTRIBUTE_DIRECTORY, 0, false, String::new())
}

/// 按扩展名分类，建一棵"合成树"：每种扩展名一个虚拟文件夹，下面放这个扩展名的
/// 全部真实文件（克隆自原树，带上 full_path_override 记住它们在磁盘上的真实路径）。
/// 建成 Node 树是为了直接复用 tree_list::show() 渲染——和主列表长得一模一样，
/// 可以展开、可以右键复制路径/打开所在文件夹，而不是另外画一套简化表格。
pub fn build_extension_tree(root: &Node, root_path: &str) -> Node {
    let mut groups = HashMap::new();
    group_files_by_extension(root, root_path, &mut groups);
    build_extension_tree_from_groups(groups)
}

/// 多个分区一起按扩展名分类——从顶部菜单进入"文件扩展名分类"时用这个：
/// 所有分区的文件混在一起、按扩展名分组，而不是每个分区各自建一棵树再
/// 简单拼起来（那样同一个扩展名会在列表里出现好几条、一个分区一条，
/// 反而不直观，也没法看出"这个扩展名总共占了多少空间"）。
pub fn build_extension_tree_multi(roots: &[(&Node, &str)]) -> Node {
    let mut groups = HashMap::new();
    for (root, root_path) in roots {
        group_files_by_extension(root, root_path, &mut groups);
    }
    build_extension_tree_from_groups(groups)
}


/// 遍历树，把可能重复的候选文件累加进 `nodes`/`paths`/`by_size`——单分区/
/// 多分区两种场景共用这一步：多分区场景对每个分区各调一次，文件下标在
/// 同一份 `nodes`/`paths` 里连续累加，`by_size` 也是同一份，大小相同的
/// 文件不管来自哪个分区都会被分进同一组，这样才能找出跨盘的重复文件
/// （比如 C 盘和 D 盘各存了一份一样的视频）。
///
/// 这一步只是在内存里走一遍已经扫描好的树、克隆一些 `Node` 出来，不涉及
/// 任何磁盘 I/O，很快，可以放心地在调用方自己的线程（通常是 UI 线程）上
/// 同步跑；真正慢的"读文件内容算哈希"那部分在 `dedup::find_duplicates`
/// 里，那个函数在后台线程上跑（见下面的 `spawn_duplicate_scan`），不会
/// 卡住界面。
fn collect_duplicate_candidates_into(
    root: &Node, root_path: &str,
    nodes: &mut Vec<Node>, paths: &mut Vec<String>, by_size: &mut HashMap<u64, Vec<usize>>,
) {
    let mut stack: Vec<(&Node, String)> = vec![(root, root_path.trim_end_matches('\\').to_string())];
    while let Some((cur, path)) = stack.pop() {
        match cur.kind {
            NodeKind::File => {
                if cur.logical_size > 0 {
                    // 0 字节文件到处都是、内容比对没有意义（全都一样），跳过，
                    // 避免候选列表被一堆空文件淹没。
                    let idx = nodes.len();
                    // `.with_full_path(path.clone())` 记住真实磁盘路径——合成树里的
                    // 节点是克隆出来的，不再挂在原来的目录结构里，右键菜单的
                    // "打开所在文件夹"/"复制路径"/删除/属性 全靠这个字段才知道
                    // 真实位置在哪。
                    nodes.push(cur.clone().with_full_path(path.clone()));
                    paths.push(path);
                    by_size.entry(cur.logical_size).or_default().push(idx);
                }
            }
            NodeKind::Folder => {
                for child in &cur.children {
                    let child_path = if path.is_empty() { child.name.clone() } else { format!("{path}\\{}", child.name) };
                    stack.push((child, child_path));
                }
            }
        }
    }
}

/// "按大小分组的候选下标表"：`(文件大小, 同大小文件的下标列表)`。
type SizeGroups = Vec<(u64, Vec<usize>)>;

fn collect_duplicate_candidates(root: &Node, root_path: &str) -> (Vec<Node>, Vec<String>, SizeGroups) {
    let mut nodes = Vec::new();
    let mut paths = Vec::new();
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    collect_duplicate_candidates_into(root, root_path, &mut nodes, &mut paths, &mut by_size);
    let size_groups: SizeGroups = by_size.into_iter().filter(|(_, idxs)| idxs.len() >= 2).collect();
    (nodes, paths, size_groups)
}

/// 后台线程算重复文件期间/算完之后回传给 UI 线程的消息。`Progress` 里的
/// `phase`/`done`/`total` 见 `dedup::HashPhase`/`dedup::find_duplicates` 的
/// 说明——两个阶段（预筛/最终确认）各自独立计数，`done`/`total` 都是"当前
/// 这个阶段"的数字，不用再夹 `min` 防止"超过 100%"，UI 上应该按 `phase`
/// 分别展示成"第一步：xxx"/"第二步：xxx"，不要合并成一条进度，不然又会
/// 变回"看起来卡在 100%"的老问题（切换到第二阶段时数字会从 0 重新开始，
/// 不提示清楚"现在换阶段了"的话，用户会以为进度条自己倒退了）。
pub enum DuplicateMessage {
    Progress { phase: crate::dedup::HashPhase, done: u64, total: u64 },
    Done(Box<Node>),
    /// 后台线程内部 panic 了（理论上不该发生，但必须有兑底）：没有这条消息，
    /// UI 端的"正在比对内容…"占位会永远转下去（线程死了、通道断了，UI 端
    /// 却不知道该把 loading 清掉）。收到这条后 UI 清掉 loading、展示错误提示，
    /// 日志里也有 panic 详情（全局 panic hook 会记录）。
    Failed(String),
}

pub fn spawn_duplicate_scan(root: &Node, root_path: &str, tx: std::sync::mpsc::Sender<DuplicateMessage>) {
    let (nodes, paths, size_groups) = collect_duplicate_candidates(root, root_path);
    spawn_duplicate_scan_from(nodes, paths, size_groups, tx);
}

/// 多个分区一起找重复——从顶部菜单进入"重复文件查找"时用这个：所有分区的
/// 候选文件按大小分到同一批组里，大小相同的文件不管来自哪个分区都会被
/// 拿去做内容比对，能找出跨盘的重复文件（比如 C 盘和 D 盘各存了一份一样
/// 的安装包）。
pub fn spawn_duplicate_scan_multi(roots: &[(&Node, &str)], tx: std::sync::mpsc::Sender<DuplicateMessage>) {
    let mut nodes = Vec::new();
    let mut paths = Vec::new();
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for (root, root_path) in roots {
        collect_duplicate_candidates_into(root, root_path, &mut nodes, &mut paths, &mut by_size);
    }
    let size_groups: SizeGroups = by_size.into_iter().filter(|(_, idxs)| idxs.len() >= 2).collect();
    spawn_duplicate_scan_from(nodes, paths, size_groups, tx);
}

/// 单分区/多分区两条入口收集完候选文件之后，共用的"起后台线程做内容比对"
/// 逻辑——分两段：调用方线程（通常是 UI 线程）已经同步做完了收集候选这一步
/// （只是内存里走一遍树，不碰磁盘，很快，不会让界面卡顿），这里再把真正
/// 耗时的哈希比对扔进一个新开的后台线程，通过 `tx` 汇报进度、最后把算好
/// 的树回传——调用方（`app.rs`）拿到 `tx` 对应的 `Receiver` 之后每帧
/// `try_recv()` 一下就行，界面全程可以正常交互，不会被卡住。
fn spawn_duplicate_scan_from(nodes: Vec<Node>, paths: Vec<String>, size_groups: SizeGroups, tx: std::sync::mpsc::Sender<DuplicateMessage>) {
    let total_candidates: usize = size_groups.iter().map(|(_, idxs)| idxs.len()).sum();

    std::thread::spawn(move || {
        // 整个线程体包一层 catch_unwind：不管里面因为什么原因 panic
        // （理论上不该发生，但裸指针/大数运算这些地方不能拍胸脯保证），都必须
        // 给 UI 发一条终结消息——以前 panic 的话 Done 永远发不出去，UI 端
        // 只能靠"通道断开"发现，而那个路径只清了接收器记录、没清标签页的
        // loading，界面就永远停在"正在比对内容…"。现在 panic 时明确发
        // Failed(原因)，UI 端收到后清 loading、给出错误提示；就算 Failed
        // 也发不出去（极端情况），app.rs 的 Disconnected 分支也会兑底清 loading。
        let tx_guard = tx.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let started = std::time::Instant::now();
        let tx_progress = tx.clone();
        let on_progress = move |phase: crate::dedup::HashPhase, done: u64, total: u64| {
            let _ = tx_progress.send(DuplicateMessage::Progress { phase, done, total });
        };
        let groups = crate::dedup::find_duplicates(&paths, size_groups, &on_progress);

        // 每组重复文件的哈希/路径不再整批写日志了——之前那样做（每组一行、
        // `log_batch` 一次性落盘）实测就是"进度条已经走到 100%、界面却还在
        // 卡住不动"的真正原因：确认阶段之后如果还剩下几千甚至上万个分组，
        // 每组的路径列表拼接（`format!`、`join`）全部堆在一起做，是这段时间
        // 里唯一还在跑、但完全不出现在进度条里的工作，看起来就像"卡死了"。
        // 现在只挑第一组的第一个文件记一行日志，纯粹是给"想快速确认一下哈希
        // 对不对"的场景留个样例，不会有性能影响（就一行，不随分组数量增长）。
        // TODO(以后想在界面上看到完整结果的时候)：更好的位置是在重复文件
        // 列表里加一列"哈希"直接展示（`DuplicateGroup.hash_hex` 已经带着这个
        // 值了），可以直接在界面上复制/核对，比翻日志文件好用得多。
        if let Some(first) = groups.first()
            && let Some(&first_file_idx) = first.file_indices.first() {
                crate::applog::log(&format!(
                    "[dedup] 示例（仅记第 1 组第 1 个文件，其余不再逐条记录，逐字节比较确认一致，不是靠哈希碰巧相同）: hash={} size={} 路径={}",
                    first.hash_hex.as_deref().unwrap_or("(文件较大，未缓存，无哈希——判定依据是逐字节比较，不影响结果的确定性)"),
                    first.size, paths[first_file_idx],
                ));
            }

        let wasted_total: u64 = groups.iter().map(|g| g.size * (g.file_indices.len() as u64 - 1)).sum();
        crate::applog::log(&format!(
            "[dedup] 完成: 候选 {total_candidates} 个文件 → 确认 {} 组疑似重复，预计可省 {}，耗时 {:.1}s",
            groups.len(), crate::format::human_size(wasted_total), started.elapsed().as_secs_f32(),
        ));

        // 组好展示用的 Node 树，按"潜在可省空间"从大到小排序，最值得关注的排前面。
        let mut pairs: Vec<(u64, Node)> = groups
            .into_iter()
            .map(|g| {
                let wasted = g.size * (g.file_indices.len() as u64 - 1);
                let count = g.file_indices.len();
                let group_files: Vec<Node> = g.file_indices.iter().map(|&i| nodes[i].clone()).collect();
                let name = format!(
                    "{} × {count} 个文件（逐字节确认一致，可省 {}）",
                    crate::format::human_size(g.size), crate::format::human_size(wasted),
                );
                (wasted, Node::new_folder_with_meta(name, GROUP_COLOR, group_files, 0, 0, 0, crate::fs_attrs::FILE_ATTRIBUTE_DIRECTORY, 0, false, String::new()))
            })
            .collect();
        pairs.sort_by_key(|a| std::cmp::Reverse(a.0));
        let dup_folders: Vec<Node> = pairs.into_iter().map(|(_, n)| n).collect();
        let tree = Node::new_folder_with_meta(
            "重复文件（逐字节确认，可放心用于符号链接/去重）".to_string(), GROUP_COLOR, dup_folders, 0, 0, 0, crate::fs_attrs::FILE_ATTRIBUTE_DIRECTORY, 0, false, String::new(),
        );
        let _ = tx.send(DuplicateMessage::Done(Box::new(tree)));
        }));
        if let Err(payload) = result {
            let msg = payload.downcast_ref::<&str>().map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "未知内部错误".to_string());
            crate::applog::log(&format!("[dedup] 后台比对线程 panic: {msg}"));
            let _ = tx_guard.send(DuplicateMessage::Failed(format!("重复文件比对内部错误（已记录日志）: {msg}")));
        }
    });
}

// 分组行颜色统一走 theme。
use crate::theme::GROUP_COLOR;

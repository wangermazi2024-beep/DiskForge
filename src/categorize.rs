
use egui::Color32;
use std::collections::HashMap;
use crate::model::{Node, NodeKind};

const LABELS: [&str; 6] = ["视频", "压缩包", "程序/exe", "文档", "图片", "其他"];
const COLORS: [Color32; 6] = [
    Color32::from_rgb(0xE0, 0x55, 0x5B), Color32::from_rgb(0xF5, 0xA6, 0x23),
    crate::theme::ACCENT_BLUE, Color32::from_rgb(0x34, 0xC7, 0x59),
    Color32::from_rgb(0x9C, 0x6A, 0xDE), crate::theme::FILE_COLOR,
];

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

pub fn build_extension_tree(root: &Node, root_path: &str) -> Node {
    let mut groups = HashMap::new();
    group_files_by_extension(root, root_path, &mut groups);
    build_extension_tree_from_groups(groups)
}

pub fn build_extension_tree_multi(roots: &[(&Node, &str)]) -> Node {
    let mut groups = HashMap::new();
    for (root, root_path) in roots {
        group_files_by_extension(root, root_path, &mut groups);
    }
    build_extension_tree_from_groups(groups)
}


fn collect_duplicate_candidates_into(
    root: &Node, root_path: &str,
    nodes: &mut Vec<Node>, paths: &mut Vec<String>, by_size: &mut HashMap<u64, Vec<usize>>,
) {
    let mut stack: Vec<(&Node, String)> = vec![(root, root_path.trim_end_matches('\\').to_string())];
    while let Some((cur, path)) = stack.pop() {
        match cur.kind {
            NodeKind::File => {
                if cur.logical_size > 0 {
                    let idx = nodes.len();
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

type SizeGroups = Vec<(u64, Vec<usize>)>;

fn collect_duplicate_candidates(root: &Node, root_path: &str) -> (Vec<Node>, Vec<String>, SizeGroups) {
    let mut nodes = Vec::new();
    let mut paths = Vec::new();
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    collect_duplicate_candidates_into(root, root_path, &mut nodes, &mut paths, &mut by_size);
    let size_groups: SizeGroups = by_size.into_iter().filter(|(_, idxs)| idxs.len() >= 2).collect();
    (nodes, paths, size_groups)
}

pub enum DuplicateMessage {
    Progress { phase: crate::dedup::HashPhase, done: u64, total: u64 },
    Done(Box<Node>),
    Failed(String),
}

pub fn spawn_duplicate_scan(root: &Node, root_path: &str, tx: std::sync::mpsc::Sender<DuplicateMessage>) {
    let (nodes, paths, size_groups) = collect_duplicate_candidates(root, root_path);
    spawn_duplicate_scan_from(nodes, paths, size_groups, tx);
}

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

fn spawn_duplicate_scan_from(nodes: Vec<Node>, paths: Vec<String>, size_groups: SizeGroups, tx: std::sync::mpsc::Sender<DuplicateMessage>) {
    let total_candidates: usize = size_groups.iter().map(|(_, idxs)| idxs.len()).sum();

    std::thread::spawn(move || {
        let tx_guard = tx.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let started = std::time::Instant::now();
        let tx_progress = tx.clone();
        let on_progress = move |phase: crate::dedup::HashPhase, done: u64, total: u64| {
            let _ = tx_progress.send(DuplicateMessage::Progress { phase, done, total });
        };
        let groups = crate::dedup::find_duplicates(&paths, size_groups, &on_progress);

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

use crate::theme::GROUP_COLOR;

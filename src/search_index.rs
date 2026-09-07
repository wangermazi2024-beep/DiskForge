
use std::collections::HashMap;
use std::time::Instant;

use crate::model::{Node, NodeKind, NodePath, SortDir, SortKey};

pub const NO_PARENT: u32 = u32::MAX;
pub const NO_DIR: u32 = u32::MAX;

fn cmp_ignore_ascii_case(a: &str, b: &str) -> std::cmp::Ordering {
    a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

#[derive(Clone)]
pub struct IdxEntry {
    pub pi: u32,
    pub child_idx: u32,
    pub parent: u32,
    pub depth: u32,
    pub flags: u32,
    pub dir: u32,
    pub owner: u32,
    pub parent_logical: u64,
    pub logical_size: u64,
    pub physical_size: u64,
    pub modified_ft: u64,
    pub created_ft: u64,
    pub accessed_ft: u64,
    pub attributes: u32,
    pub reparse_tag: u32,
    pub file_count: u32,
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
    pub dir: u32,
}

pub struct NameIndex {
    pub struct_version: u64,
    blob_lower: Vec<u8>,
    blob_orig: Vec<u8>,
    lower_starts: Vec<u32>,
    orig_starts: Vec<u32>,
    entries: Vec<IdxEntry>,
    file_entries: Vec<u32>,
    roots: Vec<RootInfo>,
    dirs: Vec<String>,
    owners: Vec<String>,
}

pub enum BuildStep {
    Continue,
    Done(Box<NameIndex>),
}

pub struct IndexBuilder {
    pub struct_version: u64,
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
    pub fn new_multi(partitions: &[Node], root_paths: &[String], struct_version: u64, want_dirs: bool) -> Self {
        let mut b = Self::empty(struct_version, want_dirs, partitions.len());
        for (pi, root) in partitions.iter().enumerate() {
            let root_dir = if want_dirs {
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

    pub fn step(&mut self, budget: std::time::Duration) -> BuildStep {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            let Some(frame) = self.stack.pop() else {
                return BuildStep::Done(Box::new(self.finish()));
            };
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
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[inline]
    pub fn name_lower(&self, i: usize) -> &str {
        unsafe {
            std::str::from_utf8_unchecked(
                &self.blob_lower[self.lower_starts[i] as usize..self.lower_starts[i + 1] as usize],
            )
        }
    }

    #[inline]
    pub fn name_orig(&self, i: usize) -> &str {
        unsafe {
            std::str::from_utf8_unchecked(
                &self.blob_orig[self.orig_starts[i] as usize..self.orig_starts[i + 1] as usize],
            )
        }
    }

    pub fn dir_path(&self, i: usize) -> &str {
        let d = self.entries[i].dir;
        if d == NO_DIR { "" } else { self.dirs[d as usize].as_str() }
    }

    pub fn owner(&self, i: usize) -> &str {
        self.owners[self.entries[i].owner as usize].as_str()
    }

    pub fn root_name(&self, pi: usize) -> &str {
        self.roots[pi].name.as_str()
    }

    pub fn root_logical(&self, pi: usize) -> u64 {
        self.roots[pi].logical_size
    }

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

    pub fn abs_path_eq(&self, e: u32, path: &[usize]) -> bool {
        let mut cur = e as usize;
        let mut j = path.len();
        loop {
            if j == 0 {
                return false;
            }
            j -= 1;
            let ent = &self.entries[cur];
            if ent.child_idx as usize != path[j] {
                return false;
            }
            match ent.parent {
                NO_PARENT => {
                    return j == 1 && ent.pi as usize == path[0];
                }
                p => cur = p as usize,
            }
        }
    }

    pub fn all_file_entries(&self) -> Vec<u32> {
        self.file_entries.clone()
    }

    pub fn file_entry_count(&self) -> usize {
        self.file_entries.len()
    }

    #[inline]
    pub fn entry(&self, i: usize) -> &IdxEntry {
        &self.entries[i]
    }

    pub fn entries(&self) -> &[IdxEntry] {
        &self.entries
    }

    pub fn root_info(&self, pi: usize) -> &RootInfo {
        &self.roots[pi]
    }

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
                            while ei + 1 < e1 && starts[ei + 1] as usize <= gpos {
                                ei += 1;
                            }
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
                    Err(_) => crate::applog::log("[search_index] find_plain 分块线程 panic，该块结果丢弃"),
                }
            }
            all
        })
    }

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

    pub fn sort_order(&self, order: &mut [u32], key: SortKey, dir: SortDir) {
        order.sort_by(|&a, &b| {
            let (ea, eb) = (&self.entries[a as usize], &self.entries[b as usize]);
            let ord = match key {
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
                SortKey::Path => cmp_ignore_ascii_case(self.dir_path(a as usize), self.dir_path(b as usize)),
            };
            if dir == SortDir::Desc { ord.reverse() } else { ord }
        });
    }

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
        assert_eq!(idx.len(), 7);
        let names: Vec<&str> = (0..idx.len()).map(|i| idx.name_orig(i)).collect();
        assert_eq!(names, vec!["Windows", "system32", "notepad.EXE", "AbC.txt", "Users", "readme.md", "top.mp4"]);
        let files: Vec<&str> = idx.all_file_entries().iter().map(|&e| idx.name_orig(e as usize)).collect();
        assert_eq!(files, vec!["notepad.EXE", "AbC.txt", "readme.md", "top.mp4"]);
        assert_eq!(idx.file_entry_count(), 4);
    }

    #[test]
    fn test_find_plain_case_insensitive() {
        let root = test_root();
        let idx = build_index(&root, 1);
        assert_eq!(idx.find_plain("abc"), vec![3]);
        assert_eq!(idx.find_plain("sys"), vec![1]);
        assert_eq!(idx.find_plain("MP4"), vec![6]);
        assert!(idx.find_plain("").is_empty());
        assert!(idx.find_plain("xxxxxxxxxxxxxxxxxxxxxxxxxxxx").is_empty());
    }

    #[test]
    fn test_find_plain_boundary_no_cross_name_false_hit() {
        let root = node("X:", 10, NodeKind::Folder, vec![
            node("ab", 1, NodeKind::File, vec![]),
            node("cd", 2, NodeKind::File, vec![]),
        ]);
        let idx = build_index(&root, 1);
        assert!(idx.find_plain("bc").is_empty(), "跨名拼接的假命中必须被剔除");
        assert_eq!(idx.find_plain("ab"), vec![0]);
        assert_eq!(idx.find_plain("cd"), vec![1]);
        assert!(idx.find_plain("abcd").is_empty());
    }

    #[test]
    fn test_find_regex_and_wildcards() {
        let root = test_root();
        let idx = build_index(&root, 1);
        let re = regex::Regex::new(r"(?i)^.*\.exe$").unwrap();
        assert_eq!(idx.find_regex(&re), vec![2]);
        let re2 = regex::Regex::new(r"(?i)^readme\.md$").unwrap();
        assert_eq!(idx.find_regex(&re2), vec![5]);
        let re3 = regex::Regex::new(r"(?i)WIN").unwrap();
        assert_eq!(idx.find_regex(&re3), vec![0]);
    }

    #[test]
    fn test_abs_path_of_matches_tree_coordinates() {
        let root = test_root();
        let idx = build_index(&root, 1);
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
        for e in 0..idx.len() as u32 {
            let p = idx.abs_path_of(e);
            assert!(idx.abs_path_eq(e, &p), "条目 {e} 的 abs_path_eq 应为真：{p:?}");
        }
        assert!(idx.abs_path_eq(3, &[0, 0, 0, 1]));
        assert!(idx.abs_path_eq(6, &[0, 2]));
        assert!(!idx.abs_path_eq(3, &[0, 0, 0, 0]));
        assert!(!idx.abs_path_eq(3, &[0, 0, 1, 1]));
        assert!(!idx.abs_path_eq(3, &[1, 0, 0, 1]));
        assert!(!idx.abs_path_eq(6, &[0, 2, 0]));
        assert!(!idx.abs_path_eq(0, &[0]));
        assert!(!idx.abs_path_eq(0, &[]));
        assert!(!idx.abs_path_eq(3, &[2, 0, 0, 1]));
    }

    #[test]
    fn test_dir_pool() {
        let root = test_root();
        let idx = build_index(&root, 1);
        assert_eq!(idx.dir_path(0), "C:");
        assert_eq!(idx.dir_path(2), "C:\\Windows\\system32");
        assert_eq!(idx.dir_path(5), "C:\\Users");
        assert_eq!(idx.dir_path(6), "C:");
    }

    #[test]
    fn test_sort_order_by_name() {
        let root = test_root();
        let idx = build_index(&root, 1);
        let mut order = idx.all_file_entries();
        idx.sort_order(&mut order, SortKey::Name, SortDir::Asc);
        let names: Vec<&str> = order.iter().map(|&e| idx.name_orig(e as usize)).collect();
        assert_eq!(names, vec!["AbC.txt", "notepad.EXE", "readme.md", "top.mp4"]);
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
        let top_names: Vec<&str> = r.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(top_names, vec!["Windows", "Users", "top.mp4"]);
        let sys = &r.children[0].children[0];
        assert_eq!(sys.name, "system32");
        assert_eq!(sys.logical_size, 50);
        let leaf_names: Vec<&str> = sys.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(leaf_names, vec!["notepad.EXE", "AbC.txt"]);
        assert_eq!(sys.children[1].logical_size, 5);
        assert_eq!(sys.children[0].kind, NodeKind::File);
        assert_eq!(r.children[0].kind, NodeKind::Folder);
    }

    #[test]
    fn test_single_root_builder_shape() {
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
        assert_eq!(idx.dir_path(0), "");
    }

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

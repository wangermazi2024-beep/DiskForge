
use egui::Color32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    File,
    Folder,
}

pub type NodePath = Vec<usize>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Modified,
    Physical,
    Created,
    Accessed,
    Items,
    Files,
    Folders,
    Attributes,
    Reparse,
    Reserved,
    Owner,
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

#[derive(Clone)]
pub struct Node {
    pub name: String,
    pub size: u64,
    pub logical_size: u64,
    pub physical_size: u64,
    pub kind: NodeKind,
    pub color: Color32,
    pub children: Vec<Node>,
    pub expanded: bool,

    pub file_count: u64,
    pub folder_count: u64,
    pub modified_ft: u64,
    pub created_ft: u64,
    pub accessed_ft: u64,
    pub attributes: u32,
    pub reparse_tag: u32,
    pub is_reserved: bool,
    pub owner: String,
    pub full_path_override: Option<String>,
}

impl Node {
    pub fn with_full_path(mut self, path: String) -> Self {
        self.full_path_override = Some(path);
        self
    }
    pub fn new_folder(name: impl Into<String>, color: Color32, children: Vec<Node>) -> Self {
        Self::new_folder_with_meta(name, color, children, 0, 0, 0, 0x10, 0, false, String::new())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_folder_with_meta(
        name: impl Into<String>,
        color: Color32,
        mut children: Vec<Node>,
        modified_ft: u64,
        created_ft: u64,
        accessed_ft: u64,
        attributes: u32,
        reparse_tag: u32,
        is_reserved: bool,
        owner: String,
    ) -> Self {
        children.sort_by(|a, b| {
            b.logical_size.cmp(&a.logical_size)
                .then_with(|| b.is_folder().cmp(&a.is_folder()))
        });
        let logical_size = children.iter().map(|c| c.logical_size).sum();
        let physical_size = children.iter().map(|c| c.physical_size).sum();
        let file_count = children.iter().map(|c| c.file_count).sum::<u64>()
            + children.iter().filter(|c| c.is_file()).count() as u64;
        let folder_count = children.iter().map(|c| c.folder_count).sum::<u64>()
            + children.iter().filter(|c| c.is_folder()).count() as u64;
        let modified_ft = modified_ft.max(children.iter().map(|c| c.modified_ft).max().unwrap_or(0));
        let attributes = if attributes == 0 { 0x10 } else { attributes };
        Self {
            name: name.into(),
            size: logical_size,
            logical_size,
            physical_size,
            kind: NodeKind::Folder,
            color,
            children,
            expanded: false,
            file_count,
            folder_count,
            modified_ft,
            created_ft,
            accessed_ft,
            attributes,
            reparse_tag,
            is_reserved,
            owner,
            full_path_override: None,
        }
    }

    pub fn new_file(name: impl Into<String>, logical: u64, color: Color32) -> Self {
        Self::new_file_with_meta(name, logical, logical, color, 0, 0, 0, 0x80, 0, false, String::new())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_file_with_meta(
        name: impl Into<String>,
        logical_size: u64,
        physical_size: u64,
        color: Color32,
        modified_ft: u64,
        created_ft: u64,
        accessed_ft: u64,
        attributes: u32,
        reparse_tag: u32,
        is_reserved: bool,
        owner: String,
    ) -> Self {
        Self {
            name: name.into(),
            size: logical_size,
            logical_size,
            physical_size,
            kind: NodeKind::File,
            color,
            children: Vec::new(),
            expanded: false,
            file_count: 0,
            folder_count: 0,
            modified_ft,
            created_ft,
            accessed_ft,
            attributes: if attributes == 0 { 0x80 } else { attributes },
            reparse_tag,
            is_reserved,
            owner,
            full_path_override: None,
        }
    }

    pub fn is_folder(&self) -> bool {
        matches!(self.kind, NodeKind::Folder)
    }

    pub fn is_hidden_or_system(&self) -> bool {
        use crate::fs_attrs::{FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM};
        self.attributes & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0
    }

    pub fn is_file(&self) -> bool {
        matches!(self.kind, NodeKind::File)
    }

    pub fn is_reparse_point(&self) -> bool {
        use crate::fs_attrs::FILE_ATTRIBUTE_REPARSE_POINT;
        self.reparse_tag != 0 || self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    pub fn navigate(&self, path: &[usize]) -> Option<&Node> {
        let mut cur = self;
        for &i in path {
            cur = cur.children.get(i)?;
        }
        Some(cur)
    }

    pub fn navigate_mut(&mut self, path: &[usize]) -> Option<&mut Node> {
        let mut cur = self;
        for &i in path {
            cur = cur.children.get_mut(i)?;
        }
        Some(cur)
    }

    pub fn collapse_all(&mut self) {
        let mut stack: Vec<Vec<usize>> = vec![Vec::new()];
        while let Some(rel) = stack.pop() {
            let Some(cur) = (if rel.is_empty() { Some(&mut *self) } else { self.navigate_mut(&rel) }) else { continue };
            let was_expanded = cur.expanded;
            cur.expanded = false;
            if was_expanded {
                for i in 0..cur.children.len() {
                    let mut child_rel = rel.clone();
                    child_rel.push(i);
                    stack.push(child_rel);
                }
            }
        }
    }

    pub fn get_at_path(&self, path: &[usize]) -> Option<&Node> {
        let mut cur = self;
        for &i in path {
            cur = cur.children.get(i)?;
        }
        Some(cur)
    }

    pub fn remove_at_path(&mut self, path: &[usize]) -> Option<Node> {
        let &idx = path.first()?;
        let removed = if path.len() == 1 {
            if idx >= self.children.len() { return None; }
            self.children.remove(idx)
        } else {
            self.children.get_mut(idx)?.remove_at_path(&path[1..])?
        };
        self.logical_size = self.logical_size.saturating_sub(removed.logical_size);
        self.size = self.logical_size;
        self.physical_size = self.physical_size.saturating_sub(removed.physical_size);
        let removed_files = removed.file_count + if removed.is_file() { 1 } else { 0 };
        let removed_folders = removed.folder_count + if removed.is_folder() { 1 } else { 0 };
        self.file_count = self.file_count.saturating_sub(removed_files);
        self.folder_count = self.folder_count.saturating_sub(removed_folders);
        Some(removed)
    }

    pub fn replace_at_path(&mut self, path: &[usize], new_node: Node) -> bool {
        let Some(&idx) = path.first() else { return false };
        if path.len() == 1 {
            let Some(old) = self.children.get(idx) else { return false };
            let old_logical = old.logical_size;
            let old_physical = old.physical_size;
            let old_files = old.file_count + if old.is_file() { 1 } else { 0 };
            let old_folders = old.folder_count + if old.is_folder() { 1 } else { 0 };
            let new_files = new_node.file_count + if new_node.is_file() { 1 } else { 0 };
            let new_folders = new_node.folder_count + if new_node.is_folder() { 1 } else { 0 };
            self.logical_size = self.logical_size.saturating_sub(old_logical).saturating_add(new_node.logical_size);
            self.size = self.logical_size;
            self.physical_size = self.physical_size.saturating_sub(old_physical).saturating_add(new_node.physical_size);
            self.file_count = self.file_count.saturating_sub(old_files).saturating_add(new_files);
            self.folder_count = self.folder_count.saturating_sub(old_folders).saturating_add(new_folders);
            self.children[idx] = new_node;
            true
        } else {
            let Some(child) = self.children.get_mut(idx) else { return false };
            let old_logical = child.logical_size;
            let old_physical = child.physical_size;
            let old_files = child.file_count;
            let old_folders = child.folder_count;
            let ok = child.replace_at_path(&path[1..], new_node);
            if ok {
                self.logical_size = self.logical_size.saturating_sub(old_logical).saturating_add(child.logical_size);
                self.size = self.logical_size;
                self.physical_size = self.physical_size.saturating_sub(old_physical).saturating_add(child.physical_size);
                self.file_count = self.file_count.saturating_sub(old_files).saturating_add(child.file_count);
                self.folder_count = self.folder_count.saturating_sub(old_folders).saturating_add(child.folder_count);
            }
            ok
        }
    }

    pub fn exclusive_toggle(&mut self, path: &[usize]) -> bool {
        if path.is_empty() {
            return false;
        }
        let mut cur = self;
        for &i in &path[..path.len() - 1] {
            match cur.children.get_mut(i) {
                Some(next) => cur = next,
                None => return false,
            }
        }
        let target_idx = path[path.len() - 1];
        let was_expanded = cur.children.get(target_idx).map(|n| n.expanded).unwrap_or(false);
        for child in &mut cur.children {
            child.collapse_all();
        }
        if !was_expanded
            && let Some(target) = cur.children.get_mut(target_idx) {
                target.expanded = true;
                return true;
            }
        false
    }

    pub fn toggle_expand(&mut self, path: &[usize]) -> bool {
        let Some(target) = self.navigate_mut(path) else { return false };
        if target.expanded {
            target.collapse_all();
        } else {
            target.expanded = true;
        }
        true
    }
}

#[derive(Clone)]
pub struct CategoryStat {
    pub label: &'static str,
    pub size: u64,
    pub color: Color32,
}

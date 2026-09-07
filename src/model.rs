//! 核心数据模型：递归的文件/文件夹树（v2 — WinDirStat 风格）。
//!
//! 参考 WinDirStat 的 CItem，每个节点同时保存 Logical Size 和 Physical Size。
//! UI 默认以 Logical Size 为准（和 Explorer / WizTree 一致）。

use egui::Color32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    File,
    Folder,
}

/// 树节点在树中的位置，用"从根节点出发的子节点下标序列"表示。
pub type NodePath = Vec<usize>;

/// 主列表（tree_list）可排序的字段。父占比/总占比两列本质上和逻辑大小同序
/// （同一层级内 parent_logical 相同，全树内 disk_logical 也相同），
/// 所以这两列点击时都映射到 `Size`。
///
/// 定义在 lib 侧（model）而不是 UI 侧，是因为搜索索引的表头排序
/// （`search_index::NameIndex::sort_order`）在后台线程上跑，也要按同一套
/// 键值语义读索引自含字段——lib 里的模块引用不到 bin 里的 `ui` 模块，
/// 这套枚举本来就属于"所有列表共用"的词汇表。
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
    /// 只有"搜索"标签页那张摊平的文件列表才有意义（普通树模式下位置已经
    /// 靠展开层级体现了，不需要单独一列/一种排序方式）——按所在文件夹路径
    /// 排序，方便把同一个目录下的文件排在一起看。
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

/// 当前排序状态：排哪一列 + 升/降序。默认和原来"构建时排序"的规则一致
/// （按逻辑大小降序），保证不带排序状态的旧行为不变。
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
    /// 表头被点击：点同一列切换方向；点新列换到新列，默认降序
    /// （体积/时间/数量类列一般更想先看"最大/最新"的，降序更符合直觉）。
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
    /// Logical Size（逻辑大小 = Explorer "大小" = $DATA.FileSize）。
    /// UI 默认显示这个。向后兼容字段，等于 `logical_size`。
    pub size: u64,
    /// Logical Size（和 `size` 相同，显式保留方便区分）。
    pub logical_size: u64,
    /// Physical Size（物理大小 = Explorer "占用空间" = $DATA.AllocatedLength/Compressed）。
    pub physical_size: u64,
    pub kind: NodeKind,
    pub color: Color32,
    pub children: Vec<Node>,
    pub expanded: bool,

    pub file_count: u64,
    pub folder_count: u64,
    /// 最后修改时间（FILETIME，1601-01-01 起 100ns）。0=未知。
    pub modified_ft: u64,
    /// 创建时间（FILETIME）。0=未知。
    pub created_ft: u64,
    /// 最后访问时间（FILETIME）。0=未知。
    pub accessed_ft: u64,
    /// Windows 文件属性位（FILE_ATTRIBUTE_*）。
    pub attributes: u32,
    /// Reparse point tag（0=普通文件，IO_REPARSE_TAG_*=reparse point）。
    pub reparse_tag: u32,
    /// 是否是 NTFS 保留系统文件（record < 16，如 $MFT/$LogFile/$Bitmap）。
    pub is_reserved: bool,
    /// 所有者（SID 或用户名，可能为空）。
    pub owner: String,
    /// 只给"分析视图"（扩展名分类/重复文件查找）里的合成节点用：这些节点是按扩展名/大小
    /// 重新分组显示的，在合成树里的位置和它们在磁盘上真实的父目录不是一回事，
    /// 沿着树往上拼祖先名字重建出来的路径会是错的。有这个字段就直接用它，
    /// 没有（正常扫描出来的节点）就还是按原来的办法从父级拼。
    pub full_path_override: Option<String>,
}

impl Node {
    /// 给合成节点（分析视图用）标记真实完整路径，链式调用。
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
        // 排序一次：按 logical_size 降序，文件夹优先
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

    /// 是否带有"隐藏"或"系统"属性（FILE_ATTRIBUTE_HIDDEN=0x02 / FILE_ATTRIBUTE_SYSTEM=0x04）。
    /// Windows 资源管理器对这类项目的做法是图标和文字都做半透明/淡化处理，用来提示"这是隐藏项"，
    /// 而不是直接不显示——我们扫描器本来就没有 Explorer 那个"隐藏文件"开关的过滤逻辑，
    /// 所有文件都会显示，所以用同样的"淡化"视觉提示替代"完全不显示"。
    pub fn is_hidden_or_system(&self) -> bool {
        use crate::fs_attrs::{FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM};
        self.attributes & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0
    }

    pub fn is_file(&self) -> bool {
        matches!(self.kind, NodeKind::File)
    }

    /// 是否是 reparse point（符号链接 / junction / 挂载点等，不管是文件还是文件夹）。
    /// 双重判断：`reparse_tag != 0` 是最直接的证据（扫描时读出来的真实 tag 值）；
    /// `FILE_ATTRIBUTE_REPARSE_POINT`（0x400）位是兜底——极少数情况下能拿到属性位
    /// 但由于权限/时序问题没能读到具体 tag（`get_reparse_tag` 内部任何一步失败都会
    /// 静默返回 0），这时候只看 `reparse_tag` 会漏判，两个条件用"或"合起来更保险，
    /// 宁可"多标一个"（把它当 reparse point 处理，UI 上加个标记、右键菜单少一个选项），
    /// 也不要"漏标"（当成普通文件/文件夹，允许对着一个符号链接再建一次符号链接）。
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
        // 迭代版本：和 mft_scan.rs 的 populate_owners 用同一套写法——栈里存相对 NodePath，
        // 每次用 navigate_mut 重新定位，不持有多个 &mut Node 引用，也不用原生递归。
        //
        // 关键优化：只有当前节点"本来就是展开状态"时才继续往它的子节点走。
        // 因为收起一个节点（`toggle_expand`/`exclusive_toggle` 的折叠分支）
        // 用的就是这个函数本身，会把整棵子树都递归收起，所以"某节点
        // expanded==false"就必然意味着它的整棵子树里不可能还有
        // expanded==true 的节点——不满足这个前提就没必要再往下探。
        // 少这一个判断的话，每次展开/收起都要把兄弟节点的整棵子树遍历一遍
        // （哪怕从来没展开过），这才是"展开列表卡 0.3-0.6 秒"的真正原因。
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

    /// 沿 `path` 找到目标节点，只读、不修改树。用于"检测占用"这种需要先拿到
    /// 节点（比如文件夹要递归收集子孙文件路径）再去做别的事情的场景——和
    /// `remove_at_path` 是同一套下标语义，但这个不消耗/修改节点。
    pub fn get_at_path(&self, path: &[usize]) -> Option<&Node> {
        let mut cur = self;
        for &i in path {
            cur = cur.children.get(i)?;
        }
        Some(cur)
    }

    /// 沿 `path` 找到目标节点并从其父节点的 `children` 里移除，同时把它的体积/
    /// 文件数/文件夹数从沿途所有祖先节点的聚合统计里减掉（`file_count`/`folder_count`/
    /// `logical_size`/`physical_size` 都是"整棵子树的合计"，删掉一个节点必须让
    /// 所有祖先跟着更新，不然主列表显示的父目录大小会变成"删除前的旧值"）。
    /// 用于"删除到回收站"成功之后，把这一项从内存里的树上摘掉，不用重新扫描整个分区。
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

    /// 沿 `path` 找到目标节点，原地替换成 `new_node`（同一个位置，`children`
    /// 长度不变，所以同一层其它兄弟节点的下标不会跟着错位——不像
    /// `remove_at_path` 那样会让后面的兄弟全部往前挪一位）。沿途祖先的聚合
    /// 统计（大小/文件数/文件夹数）会先减掉旧节点、再加上新节点的。
    ///
    /// 用于"创建符号链接成功"之后：原地把这一项刷新成磁盘上的最新状态（变成
    /// 一个符号链接），而不是把它整个从树上摘掉——摘掉的话，在 Windows
    /// 资源管理器里这个文件/文件夹其实还在（只是变成了链接），列表却凭空
    /// 少一项，容易让人误以为出了问题、操作失败了。
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
            // 子节点自己的聚合统计变化量还要继续往上传给 self——这个函数只
            // 返回 bool，不返回"变化了多少"，所以用"递归前后的快照差值"反推，
            // 不用关心深处具体改了什么，对任意深度都成立。
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
        // 迭代地走到 path 指向的父节点（除最后一段外都只是导航，和原递归版
        // "path.len()>1 时只是往下一层再调自己"完全等价，只是不再用调用栈）。
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

    /// 展开/折叠 `path` 指向的节点——跟 `exclusive_toggle` 的关键区别：**不会**
    /// 把同一层的兄弟节点收起来，允许同时展开多个分支，跟真正的文件树控件
    /// （资源管理器左侧那种）行为一致，不再是"同一时间只能展开一条分支"。
    ///
    /// 折叠某一项的时候，会把它自己整棵子树的展开状态也一起清空（不保留
    /// "之前展开到哪一层"这种记忆，直接复用 `collapse_all`）——重新展开要
    /// 从头一层层点开。这是刻意的简化：如果要保留展开记忆，折叠一个有几万
    /// 个子孙节点、层级很深的文件夹再重新展开，得一次性恢复一大批节点的
    /// `expanded` 状态，复杂度和潜在的性能开销都不值得，"重新展开要多点
    /// 几次"是完全可以接受的代价。
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

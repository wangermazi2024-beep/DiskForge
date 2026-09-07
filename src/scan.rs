
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashSet;
use egui::Color32;

use crate::disk_info::DiskInfo;
use crate::model::Node;

pub enum ScanMessage {
    Progress(u64),
    Done(Box<Node>, Option<DiskInfo>),
    Error(String),
}

fn folder_color(depth: usize) -> Color32 {
    crate::theme::folder_color(depth)
}
fn file_color() -> Color32 { crate::theme::FILE_COLOR }

fn drive_letter_of(path: &Path) -> Option<char> {
    path.to_string_lossy().chars().next()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase())
}

const SCAN_THREAD_STACK_BYTES: usize = 64 * 1024 * 1024;
const PROGRESS_STEP_ENTRIES: u64 = 5000;

pub fn spawn_scan(root: PathBuf, tx: Sender<ScanMessage>) {
    let err_tx = tx.clone();
    let builder = std::thread::Builder::new()
        .name("diskforge-scan".into())
        .stack_size(SCAN_THREAD_STACK_BYTES);
    let spawn_result = builder.spawn(move || {
        let panic_tx = tx.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let start = SystemTime::now();
        let disk_info = drive_letter_of(&root).and_then(crate::disk_info::query_disk_info);
        crate::dlog!("[scan] 启动: root={}", root.display());

        #[cfg(windows)]
        {
            enable_read_privileges();

            if let Some(drive) = as_drive_root(&root) {
                if crate::mft_scan::is_elevated() {
                    crate::dlog!("[scan] 走 MFT 直读: drive={}", drive);
                    match crate::mft_scan::scan_volume(drive, &tx) {
                        Ok(mut node) => {
                            if let Some(info) = &disk_info { node.name = info.display_name(); }
                            crate::dlog!("[scan] MFT 完成: files={}, folders={}, logical={}, physical={}, 耗时 {:.1}s",
                                node.file_count, node.folder_count,
                                crate::format::human_size(node.logical_size),
                                crate::format::human_size(node.physical_size),
                                start.elapsed().unwrap_or_default().as_secs_f64());
                            if let Some(info) = &disk_info {
                                let ratio = if info.used_bytes > 0 { node.physical_size as f64 / info.used_bytes as f64 * 100.0 } else { 0.0 };
                                crate::dlog!("[scan] 一致性检查: physical={}, 系统已用={}, 比例={:.1}%",
                                    crate::format::human_size(node.physical_size), crate::format::human_size(info.used_bytes), ratio);
                            }
                            let _ = tx.send(ScanMessage::Done(Box::new(node), disk_info));
                            return;
                        }
                        Err(e) => crate::dlog!("[scan] MFT 失败，回退常规遍历: {e}"),
                    }
                } else {
                    crate::dlog!("[scan] 非管理员，走常规遍历: drive={}", drive);
                }
            }
        }

        let counter = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let seen_file_ids: Arc<DashSet<u64>> = Arc::new(DashSet::new());
        let cluster = query_cluster_size(&root);

        match run_scan(&root, &counter, &cancel, &seen_file_ids, cluster, &tx) {
            Ok(mut node) => {
                if let Some(info) = &disk_info {
                    #[cfg(windows)]
                    if as_drive_root(&root).is_some() { node.name = info.display_name(); }
                    #[cfg(not(windows))]
                    { node.name = info.display_name(); }
                }
                crate::dlog!("[scan] 常规遍历完成: files={}, folders={}, logical={}, 耗时 {:.1}s",
                    node.file_count, node.folder_count,
                    crate::format::human_size(node.logical_size),
                    start.elapsed().unwrap_or_default().as_secs_f64());
                let _ = tx.send(ScanMessage::Done(Box::new(node), disk_info));
            }
            Err(e) => {
                crate::dlog!("[scan] 失败: {e}");
                let _ = tx.send(ScanMessage::Error(format!("扫描失败: {e}")));
            }
        }
        }));
        if let Err(payload) = result {
            let msg = payload.downcast_ref::<&str>().map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "未知内部错误".to_string());
            crate::dlog!("[scan] 扫描线程 panic: {msg}");
            let _ = panic_tx.send(ScanMessage::Error(format!("扫描过程中发生内部错误（已记录日志）: {msg}")));
        }
    });
    if let Err(e) = spawn_result {
        crate::dlog!("[scan] 无法创建扫描线程: {e}");
        let _ = err_tx.send(ScanMessage::Error(format!("无法启动扫描线程: {e}")));
    }
}

fn num_cpus_get() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

#[cfg(windows)]
fn as_drive_root(path: &Path) -> Option<char> {
    let s = path.to_string_lossy();
    let b = s.as_bytes();
    if b.len() == 3 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') {
        let c = b[0] as char;
        if c.is_ascii_alphabetic() { return Some(c.to_ascii_uppercase()); }
    }
    None
}

fn system_time_to_filetime(t: Option<SystemTime>) -> u64 {
    let t = match t { Some(t) => t, None => return 0 };
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => {
            const OFFSET: u64 = 11_644_473_600;
            d.as_secs() * 10_000_000 + (d.subsec_nanos() / 100) as u64 + OFFSET * 10_000_000
        }
        Err(_) => 0,
    }
}


struct DirTask {
    name: String,
    color: Color32,
    self_modified: u64,
    self_created: u64,
    self_accessed: u64,
    self_attrs: u32,
    parent: Option<Arc<DirTask>>,
    pending: AtomicUsize,
    children: Mutex<Vec<Node>>,
}

struct WorkItem {
    path: PathBuf,
    depth: usize,
    task: Arc<DirTask>,
}

fn run_scan(
    root: &Path,
    counter: &Arc<AtomicU64>,
    cancel: &Arc<AtomicBool>,
    seen_file_ids: &Arc<DashSet<u64>>,
    cluster: u64,
    tx: &Sender<ScanMessage>,
) -> std::io::Result<Node> {
    let root_name = root.to_string_lossy().into_owned();
    let self_meta = std::fs::metadata(root).ok();
    let self_modified = system_time_to_filetime(self_meta.as_ref().and_then(|m| m.modified().ok()));
    #[cfg(windows)]
    let self_attrs = self_meta.as_ref().map(|m| {
        use std::os::windows::fs::MetadataExt;
        m.file_attributes()
    }).unwrap_or(0x10);
    #[cfg(not(windows))]
    let self_attrs: u32 = 0x10;

    let root_task = Arc::new(DirTask {
        name: root_name,
        color: folder_color(0),
        self_modified,
        self_created: 0,
        self_accessed: 0,
        self_attrs,
        parent: None,
        pending: AtomicUsize::new(0),
        children: Mutex::new(Vec::new()),
    });

    let root_slot: Arc<Mutex<Option<Node>>> = Arc::new(Mutex::new(None));

    let queue: Arc<Mutex<std::collections::VecDeque<WorkItem>>> = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let cvar = Arc::new(std::sync::Condvar::new());
    let outstanding = Arc::new(AtomicUsize::new(1));
    let panicked = Arc::new(AtomicBool::new(false));

    queue.lock().unwrap().push_back(WorkItem { path: root.to_path_buf(), depth: 0, task: root_task });

    let num_threads = num_cpus_get().saturating_mul(2).max(2);
    crate::dlog!("[scan] 工作线程: {} 个", num_threads);

    std::thread::scope(|scope| {
        for _ in 0..num_threads {
            let queue = queue.clone();
            let cvar = cvar.clone();
            let outstanding = outstanding.clone();
            let panicked = panicked.clone();
            let root_slot = root_slot.clone();
            let counter = counter.clone();
            let cancel = cancel.clone();
            let seen_file_ids = seen_file_ids.clone();
            let tx = tx.clone();
            scope.spawn(move || {
                loop {
                    let item = {
                        let mut q = queue.lock().unwrap_or_else(|p| p.into_inner());
                        loop {
                            if let Some(item) = q.pop_front() {
                                break Some(item);
                            }
                            if outstanding.load(Ordering::Acquire) == 0 {
                                break None;
                            }
                            q = match cvar.wait(q) {
                                Ok(q) => q,
                                Err(p) => p.into_inner(),
                            };
                        }
                    };
                    let Some(WorkItem { path, depth, task }) = item else { break };
                    let new_items = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        process_one_dir(
                            &path, depth, &task, &root_slot, &counter, &cancel, &seen_file_ids, cluster, &tx,
                        )
                    })) {
                        Ok(items) => items,
                        Err(_) => {
                            panicked.store(true, Ordering::Relaxed);
                            outstanding.fetch_sub(1, Ordering::AcqRel);
                            cvar.notify_all();
                            break;
                        }
                    };
                    let mut q = queue.lock().unwrap_or_else(|p| p.into_inner());
                    let n_new = new_items.len();
                    for it in new_items {
                        q.push_back(it);
                    }
                    if n_new > 0 {
                        outstanding.fetch_add(n_new, Ordering::AcqRel);
                    }
                    outstanding.fetch_sub(1, Ordering::AcqRel);
                    drop(q);
                    cvar.notify_all();
                }
            });
        }
    });

    if panicked.load(Ordering::Relaxed) {
        return Err(std::io::Error::other(
            "扫描过程中发生内部错误（部分目录未能处理，已记录日志）",
        ));
    }

    match root_slot.lock().unwrap_or_else(|p| p.into_inner()).take() {
        Some(node) => Ok(node),
        None => Ok(Node::new_folder(root.to_string_lossy().into_owned(), folder_color(0), Vec::new())),
    }
}

#[allow(clippy::too_many_arguments)]
fn process_one_dir(
    path: &Path,
    depth: usize,
    task: &Arc<DirTask>,
    root_slot: &Arc<Mutex<Option<Node>>>,
    counter: &Arc<AtomicU64>,
    cancel: &Arc<AtomicBool>,
    seen_file_ids: &Arc<DashSet<u64>>,
    cluster: u64,
    tx: &Sender<ScanMessage>,
) -> Vec<WorkItem> {
    if cancel.load(Ordering::Relaxed) {
        finalize(task.clone(), root_slot);
        return Vec::new();
    }

    let entries = match read_entries(path, cluster) {
        Ok(e) => e,
        Err(e) => {
            if depth <= 3 {
                crate::dlog!("[scan] read_dir 失败 (depth={}, path={}, err={})", depth, path.display(), e);
            }
            finalize(task.clone(), root_slot);
            return Vec::new();
        }
    };

    let n = counter.fetch_add(entries.len() as u64, Ordering::Relaxed);
    if n / PROGRESS_STEP_ENTRIES != (n + entries.len() as u64) / PROGRESS_STEP_ENTRIES {
        let _ = tx.send(ScanMessage::Progress(n));
    }

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    let mut subdirs: Vec<crate::dir_enum::RawDirEntry> = Vec::new();
    let mut leaf_nodes: Vec<Node> = Vec::new();
    for e in entries {
        if e.is_dir {
            if e.attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                #[cfg(windows)]
                let tag = get_reparse_tag(&path.join(&e.name));
                #[cfg(not(windows))]
                let tag = 0u32;
                leaf_nodes.push(Node::new_folder_with_meta(
                    e.name, folder_color(depth + 1), Vec::new(),
                    e.modified_ft, e.created_ft, e.accessed_ft, e.attrs, tag, false, String::new(),
                ));
            } else {
                subdirs.push(e);
            }
        } else {
            let physical_to_use = if e.file_id != 0 {
                if seen_file_ids.insert(e.file_id) {
                    e.physical
                } else {
                    0
                }
            } else {
                e.physical
            };
            let file_reparse_tag = if e.attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                #[cfg(windows)]
                { get_reparse_tag(&path.join(&e.name)) }
                #[cfg(not(windows))]
                { 0u32 }
            } else {
                0u32
            };
            leaf_nodes.push(Node::new_file_with_meta(
                e.name, e.logical, physical_to_use, file_color(),
                e.modified_ft, e.created_ft, e.accessed_ft, e.attrs, file_reparse_tag, false, String::new(),
            ));
        }
    }
    if !leaf_nodes.is_empty() {
        task.children.lock().unwrap_or_else(|p| p.into_inner()).extend(leaf_nodes);
    }

    if subdirs.is_empty() {
        finalize(task.clone(), root_slot);
        return Vec::new();
    }

    task.pending.store(subdirs.len(), Ordering::Release);

    let mut new_items = Vec::with_capacity(subdirs.len());
    for sub in subdirs {
        let child_path = path.join(&sub.name);
        let child_task = Arc::new(DirTask {
            name: sub.name,
            color: folder_color(depth + 1),
            self_modified: sub.modified_ft,
            self_created: sub.created_ft,
            self_accessed: sub.accessed_ft,
            self_attrs: sub.attrs,
            parent: Some(task.clone()),
            pending: AtomicUsize::new(0),
            children: Mutex::new(Vec::new()),
        });
        new_items.push(WorkItem { path: child_path, depth: depth + 1, task: child_task });
    }
    new_items
}

fn finalize(task: Arc<DirTask>, root_slot: &Arc<Mutex<Option<Node>>>) {
    let mut current = task;
    loop {
        let children = std::mem::take(&mut *current.children.lock().unwrap_or_else(|p| p.into_inner()));
        let node = Node::new_folder_with_meta(
            current.name.clone(), current.color, children,
            current.self_modified, current.self_created, current.self_accessed,
            current.self_attrs, 0, false, String::new(),
        );

        match &current.parent {
            None => {
                *root_slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(node);
                return;
            }
            Some(parent) => {
                parent.children.lock().unwrap_or_else(|p| p.into_inner()).push(node);
                let remaining = parent.pending.fetch_sub(1, Ordering::AcqRel) - 1;
                if remaining == 0 {
                    let next = parent.clone();
                    current = next;
                    continue;
                }
                return;
            }
        }
    }
}

fn read_entries(path: &Path, cluster: u64) -> std::io::Result<Vec<crate::dir_enum::RawDirEntry>> {
    #[cfg(windows)]
    {
        if let Ok(v) = crate::dir_enum::enum_dir_batch(path) {
            return Ok(v);
        }
    }
    read_entries_fallback(path, cluster)
}

fn read_entries_fallback(path: &Path, cluster: u64) -> std::io::Result<Vec<crate::dir_enum::RawDirEntry>> {
    let rd = std::fs::read_dir(path)?;
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let meta = match entry.metadata() { Ok(m) => m, Err(_) => continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let modified_ft = system_time_to_filetime(meta.modified().ok());
        let created_ft = system_time_to_filetime(meta.created().ok());
        let accessed_ft = system_time_to_filetime(meta.accessed().ok());
        let is_dir = meta.is_dir();
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            let attrs = meta.file_attributes();
            let logical = meta.len();
            let physical = if is_dir { 0 } else { get_physical_size(&entry.path(), logical, attrs, cluster) };
            out.push(crate::dir_enum::RawDirEntry {
                name, is_dir, logical, physical, attrs, modified_ft, created_ft, accessed_ft, file_id: 0,
            });
        }
        #[cfg(not(windows))]
        {
            let attrs = if is_dir { 0x10 } else { 0x80 };
            let logical = meta.len();
            out.push(crate::dir_enum::RawDirEntry {
                name, is_dir, logical, physical: logical, attrs, modified_ft, created_ft, accessed_ft, file_id: 0,
            });
        }
    }
    Ok(out)
}

#[cfg(windows)]
fn get_reparse_tag(path: &Path) -> u32 {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;

    let wide: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(path.as_os_str())
        .chain(std::iter::once(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(), FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null_mut(), OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return 0;
    }
    let mut buf = [0u8; 16 * 1024];
    let mut returned: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            handle, FSCTL_GET_REPARSE_POINT,
            std::ptr::null(), 0,
            buf.as_mut_ptr() as *mut _, buf.len() as u32,
            &mut returned, std::ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(handle) };
    if ok == 0 || returned < 4 {
        return 0;
    }
    u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])
}

#[cfg(windows)]
fn get_physical_size(path: &Path, logical: u64, attrs: u32, cluster: u64) -> u64 {
    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;

    const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x800;
    const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;

    if attrs & (FILE_ATTRIBUTE_COMPRESSED | FILE_ATTRIBUTE_SPARSE_FILE) != 0 {
        let wide: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(path.as_os_str())
            .chain(std::iter::once(0)).collect();
        let mut high: u32 = 0;
        let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
        if low != 0xFFFFFFFF || unsafe { windows_sys::Win32::Foundation::GetLastError() } == 0 {
            return ((high as u64) << 32) | (low as u64);
        }
    }
    if logical == 0 { 0 } else { logical.div_ceil(cluster) * cluster }
}

#[cfg(windows)]
fn query_cluster_size(root: &Path) -> u64 {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceW;
    const FALLBACK: u64 = 4096;
    let Some(drive) = drive_letter_of(root) else { return FALLBACK };
    let wide: Vec<u16> = format!("{drive}:\\").encode_utf16().chain(std::iter::once(0)).collect();
    let mut sectors_per_cluster = 0u32;
    let mut bytes_per_sector = 0u32;
    let mut free_clusters = 0u32;
    let mut total_clusters = 0u32;
    let ok = unsafe {
        GetDiskFreeSpaceW(
            wide.as_ptr(),
            &mut sectors_per_cluster,
            &mut bytes_per_sector,
            &mut free_clusters,
            &mut total_clusters,
        )
    };
    if ok == 0 || sectors_per_cluster == 0 || bytes_per_sector == 0 {
        crate::dlog!("[scan] GetDiskFreeSpaceW 查询簇大小失败，fallback 用 {FALLBACK} 字节");
        return FALLBACK;
    }
    let cluster = sectors_per_cluster as u64 * bytes_per_sector as u64;
    crate::dlog!("[scan] {drive}: 真实簇大小 = {cluster} 字节 (SectorsPerCluster={sectors_per_cluster}, BytesPerSector={bytes_per_sector})");
    cluster
}
#[cfg(not(windows))]
fn query_cluster_size(_root: &Path) -> u64 { 4096 }

#[cfg(windows)]
fn enable_read_privileges() {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LUID};
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW,
        SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_QUERY,
        TOKEN_PRIVILEGES, LUID_AND_ATTRIBUTES,
        SE_BACKUP_NAME, SE_RESTORE_NAME,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) == 0 {
            return;
        }
        for priv_name in [SE_BACKUP_NAME, SE_RESTORE_NAME] {
            let mut luid = LUID { LowPart: 0, HighPart: 0 };
            if LookupPrivilegeValueW(std::ptr::null(), priv_name, &mut luid) == 0 { continue; }
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
            };
            AdjustTokenPrivileges(
                token, 0, &tp as *const _,
                std::mem::size_of::<TOKEN_PRIVILEGES>() as u32,
                std::ptr::null_mut(), std::ptr::null_mut(),
            );
        }
        CloseHandle(token);
    }
    crate::dlog!("[scan] 已尝试启用 SeBackupPrivilege + SeRestorePrivilege");
}

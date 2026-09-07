#[cfg(windows)]
mod recycle_bin {

//! 删除到回收站：使用现代 IFileOperation COM 接口（微软官方推荐，取代已过时的
//! SHFileOperation）。旧 API 对大文件夹会"进度跑完却静默中止"（0x78=源访问被拒）。
//!
//! 关键设计：
//! 1. FOF_NOERRORUI 而不配 FOFX_EARLYFAILURE 时，某个子项失败（被占用/访问被拒/
//!    路径过长）会被 shell 当作"用户点了忽略"静默跳过，并把 aborted 标志置 true
//!    ——所以 GetAnyOperationsAborted()=true 只代表"至少有一个子项失败"，
//!    不代表用户点了取消。
//! 2. 为了拿到"具体哪个文件、什么错误"，通过 Advise 注册
//!    IFileOperationProgressSink（用官方 windows crate 的 implement 宏实现，
//!    不手写 COM vtable——槽位顺序人肉对照极易出错），PostDeleteItem 回调
//!    收到每个子项的真实删除结果（hrDelete），失败项在这里逐条收集。
//! 3. 无论如何，最终以"源是否真的消失"作为成败标准，成功前不做任何承诺。
use std::path::Path;
use std::sync::{Arc, Mutex};

use windows::core::{implement, Ref, HSTRING, PCWSTR, HRESULT};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{
    FileOperation, IFileOperation, IFileOperationProgressSink, IFileOperationProgressSink_Impl,
    IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH, FOF_NOCONFIRMATION,
    FOF_NOERRORUI, FOFX_RECYCLEONDELETE,
};

/// 进度回调：PostDeleteItem / PostFailedItem 把每个子项的真实结果收集到这里。
/// 字段经 Arc 共享，PerformOperations 结束后主线程直接读取（接口对象随后释放）。
#[implement(IFileOperationProgressSink)]
struct DeleteProgressSink {
    failures: Arc<Mutex<Vec<String>>>,
}

fn record_failure(failures: &Mutex<Vec<String>>, item: Option<&IShellItem>, hr: HRESULT) {
    let Some(item) = item else { return };
    // 文件系统完整路径对用户最有用；拿不到就跳过（回收站里的虚拟项无路径）
    if let Ok(path) = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) } {
        // PWSTR 指向以 null 结尾的 UTF-16，转 String（悬垂风险：PWSTR 不拥有内存，
        // windows 绑定保证 GetDisplayName 返回的缓冲在 Result 存活期内有效）
        let path = unsafe { path.to_string() }.unwrap_or_default();
        if let Ok(mut list) = failures.lock() {
            list.push(format!("{path}（HRESULT 0x{:08X}）", hr.0 as u32));
        }
    }
}

impl IFileOperationProgressSink_Impl for DeleteProgressSink_Impl {
    fn StartOperations(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn FinishOperations(&self, _hrresult: HRESULT) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreRenameItem(&self, _dwflags: u32, _psiitem: Ref<'_, IShellItem>, _psznewname: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostRenameItem(&self, _dwflags: u32, _psiitem: Ref<'_, IShellItem>, _psznewname: &PCWSTR, _hrrename: HRESULT, _psinewlycreated: Ref<'_, IShellItem>) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreMoveItem(&self, _dwflags: u32, _psiitem: Ref<'_, IShellItem>, _psidestinationfolder: Ref<'_, IShellItem>, _psznewname: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostMoveItem(&self, _dwflags: u32, _psiitem: Ref<'_, IShellItem>, _psidestinationfolder: Ref<'_, IShellItem>, _psznewname: &PCWSTR, _hrmove: HRESULT, _psinewlycreated: Ref<'_, IShellItem>) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreCopyItem(&self, _dwflags: u32, _psiitem: Ref<'_, IShellItem>, _psidestinationfolder: Ref<'_, IShellItem>, _psznewname: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostCopyItem(&self, _dwflags: u32, _psiitem: Ref<'_, IShellItem>, _psidestinationfolder: Ref<'_, IShellItem>, _psznewname: &PCWSTR, _hrcopy: HRESULT, _psinewlycreated: Ref<'_, IShellItem>) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreDeleteItem(&self, _dwflags: u32, _psiitem: Ref<'_, IShellItem>) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostDeleteItem(&self, _dwflags: u32, psideleteditem: Ref<'_, IShellItem>, hrdelete: HRESULT, _psinewlycreated: Ref<'_, IShellItem>) -> windows::core::Result<()> {
        if hrdelete.is_err() {
            record_failure(&self.failures, psideleteditem.as_ref(), hrdelete);
        }
        Ok(())
    }
    fn PreNewItem(&self, _dwflags: u32, _psidestinationfolder: Ref<'_, IShellItem>, _psznewname: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostNewItem(&self, _dwflags: u32, _psidestinationfolder: Ref<'_, IShellItem>, _psznewname: &PCWSTR, _psztemplatename: &PCWSTR, _dwfileattributes: u32, _hrnew: HRESULT, _psinewitem: Ref<'_, IShellItem>) -> windows::core::Result<()> {
        Ok(())
    }
    fn UpdateProgress(&self, _iworktotal: u32, _iworksofar: u32) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResetTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
}

/// 失败诊断：从根路径递归收集仍残留的前 `limit` 个条目（相对路径）。
/// 只为写日志，任何 IO 错误都静默跳过；数量到上限即停，避免在大目录上耗时。
fn list_remaining(root: &Path, limit: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let base = root.as_os_str().len();
    walk(root, &mut out, base, limit);
    return out;

    fn walk(dir: &Path, out: &mut Vec<String>, base_len: usize, limit: usize) {
        if out.len() >= limit {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            // 目录本身读不了（权限/占用），把目录自己记为残留
            if out.len() < limit {
                out.push(rel_display(dir, base_len));
            }
            return;
        };
        for e in entries.flatten() {
            if out.len() >= limit {
                return;
            }
            let p = e.path();
            if p.is_dir() {
                walk(&p, out, base_len, limit);
            } else {
                out.push(rel_display(&p, base_len));
            }
        }
    }

    fn rel_display(p: &Path, base_len: usize) -> String {
        p.to_string_lossy()
            .get(base_len..)
            .map(|s| s.trim_start_matches('\\').to_string())
            .unwrap_or_else(|| p.to_string_lossy().into_owned())
    }
}

fn map_err_msg(step: &str, e: windows::core::Error) -> String {
    format!("{step}失败（HRESULT 0x{:08X}，{e}）", e.code().0 as u32)
}

pub fn delete_to_recycle_bin(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("路径为空".to_string());
    }
    // 开始就记一条日志并开始计时：大文件夹删除可能要跑很久，
    // 只有结束才有日志会让人以为程序没开始删
    let start = std::time::Instant::now();
    crate::applog::log(&format!("[file_ops] 开始删除到回收站: {path}"));
    let logged = |result: &Result<(), String>| {
        let secs = start.elapsed().as_secs_f64();
        match result {
            Ok(()) => crate::applog::log(&format!("[file_ops] 删除到回收站完成: {path}（耗时 {secs:.1}s）")),
            Err(e) => crate::applog::log(&format!("[file_ops] 删除到回收站失败: {path}: {e}（耗时 {secs:.1}s）")),
        }
    };

    // 独立删除线程上的 COM 初始化；已初始化（S_FALSE）也无妨，
    // 线程退出时由系统回收，不配对 CoUninitialize 是安全的
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };

    // SAFETY: COM 调用序列按微软文档顺序执行；sink 对象由 Advise 持有引用，
    // 本地 sink_pc 在 Unadvise 之前保持存活。
    // 整个流程包进闭包：内部任何早期 return（组件初始化失败、PerformOperations
    // 报错等）都统一落进 result 再走 logged()——之前 `run?` 直接从函数返回，
    // 这类失败一行日志都不会写，用户只能看到"没有任何日志"
    let result = (|| -> Result<(), String> {
        unsafe {
            let op: IFileOperation = match CoCreateInstance(&FileOperation, None, CLSCTX_ALL) {
                Ok(op) => op,
                Err(e) => return Err(map_err_msg("初始化文件操作组件", e)),
            };
            if let Err(e) = op.SetOperationFlags(FOFX_RECYCLEONDELETE | FOF_NOCONFIRMATION | FOF_NOERRORUI) {
                return Err(map_err_msg("设置操作参数", e));
            }
            let failures = Arc::new(Mutex::new(Vec::new()));
            let sink_pc: IFileOperationProgressSink = DeleteProgressSink { failures: failures.clone() }.into();
            let cookie = match op.Advise(&sink_pc) {
                Ok(c) => c,
                Err(e) => return Err(map_err_msg("注册进度回调", e)),
            };
            let run = (|| -> Result<(), String> {
                let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(path), None)
                    .map_err(|e| format!("无法解析路径 {path}（HRESULT 0x{:08X}，{e}）", e.code().0 as u32))?;
                op.DeleteItem(Some(&item), None).map_err(|e| map_err_msg("加入删除任务", e))?;
                op.PerformOperations().map_err(|e| map_err_msg("执行删除", e))?;
                Ok(())
            })();
            let _ = op.Unadvise(cookie);
            run?;
            drop(sink_pc);

            // 逐项失败明细（PostDeleteItem 回调收集）
            let failures = failures.lock().map(|mut l| std::mem::take(&mut *l)).unwrap_or_default();
            let aborted = op.GetAnyOperationsAborted().is_ok_and(|b| b.as_bool());
            let still_there = Path::new(path).exists();
            if aborted || !failures.is_empty() || still_there {
                // 无论哪条线索指出失败，都补充"还剩哪些文件"帮助定位
                let remaining = list_remaining(Path::new(path), 5);
                let mut msg = String::new();
                if !failures.is_empty() {
                    msg.push_str(&format!("以下子项删除失败: {}；", failures.join("、")));
                }
                if aborted && failures.is_empty() {
                    msg.push_str("操作被中止（进度框被手动取消，或个别子项被占用/访问被拒导致 shell 放弃）；");
                }
                if still_there {
                    msg.push_str("目标仍在原位置");
                    if !remaining.is_empty() {
                        msg.push_str(&format!("，残留条目（最多列 5 个）: {}", remaining.join("、")));
                    }
                }
                while msg.ends_with('；') {
                    msg.pop();
                }
                return Err(msg);
            }
            Ok(())
        }
    })();
    logged(&result);
    result
}
}

#[cfg(windows)]
pub use recycle_bin::delete_to_recycle_bin;


#[cfg(not(windows))]
pub fn delete_to_recycle_bin(_path: &str) -> Result<(), String> {
    Err("仅支持 Windows".to_string())
}

#[cfg(windows)]
pub fn open_properties(path: &str) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW};

    if path.is_empty() {
        return;
    }
    let verb: Vec<u16> = "properties".encode_utf16().chain(std::iter::once(0)).collect();
    let file: Vec<u16> = std::ffi::OsStr::new(path).encode_wide().chain(std::iter::once(0)).collect();

    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_INVOKEIDLIST;
    sei.hwnd = std::ptr::null_mut();
    sei.lpVerb = verb.as_ptr();
    sei.lpFile = file.as_ptr();
    sei.lpParameters = std::ptr::null();
    sei.lpDirectory = std::ptr::null();
    sei.nShow = 1;

    let ok = unsafe { ShellExecuteExW(&mut sei) };
    crate::applog::log(&format!("[file_ops] 打开属性对话框: {path} (ShellExecuteExW={ok})"));
    if ok == 0 {
        let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        crate::applog::log(&format!("[file_ops] 打开属性对话框失败: {path} (GetLastError={err})"));
    }
}

#[cfg(not(windows))]
pub fn open_properties(_path: &str) {}


use std::io::Read;
use std::path::Path;

pub fn hash_file_blake3(path: &str) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("打开文件失败: {e}"))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 256 * 1024];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
            }
            Err(e) => return Err(format!("读取文件失败: {e}")),
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(windows)]
pub fn create_symlink(link_path: &str, target_path: &str, is_dir: bool) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_INVALID_PARAMETER};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateSymbolicLinkW, SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE, SYMBOLIC_LINK_FLAG_DIRECTORY,
    };

    let link_w: Vec<u16> = std::ffi::OsStr::new(link_path).encode_wide().chain(std::iter::once(0)).collect();
    let target_w: Vec<u16> = std::ffi::OsStr::new(target_path).encode_wide().chain(std::iter::once(0)).collect();
    let base_flags: u32 = if is_dir { SYMBOLIC_LINK_FLAG_DIRECTORY } else { 0 };

    unsafe {
        let mut ok = CreateSymbolicLinkW(link_w.as_ptr(), target_w.as_ptr(), base_flags | SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE);
        if ok == 0 && GetLastError() == ERROR_INVALID_PARAMETER {
            ok = CreateSymbolicLinkW(link_w.as_ptr(), target_w.as_ptr(), base_flags);
        }
        if ok == 0 {
            let err = GetLastError();
            let hint = if err == 1314 {
                "（没有权限——请在「设置 > 系统 > 开发者选项」里打开开发者模式，或者以管理员身份运行本程序）"
            } else {
                ""
            };
            return Err(format!("创建符号链接失败（错误码 {err}）{hint}"));
        }
    }
    crate::applog::log(&format!("[file_ops] 创建符号链接: {link_path} -> {target_path}"));
    Ok(())
}

#[cfg(not(windows))]
pub fn create_symlink(_link_path: &str, _target_path: &str, _is_dir: bool) -> Result<(), String> {
    Err("仅支持 Windows".to_string())
}

pub fn replace_with_symlink(path: &str, target: &str, is_dir: bool, verify_content: bool) -> Result<(), String> {
    if verify_content && !is_dir && !crate::dedup::files_identical(path, target) {
        let msg = format!(
            "副本 {path} 与真身 {target} 的内容已经不一致（可能在扫描之后被修改过），为防止丢失新内容已中止对这个副本的替换，原文件保留不动"
        );
        crate::applog::log(&format!("[file_ops] 替换符号链接前复查失败: {msg}"));
        return Err(msg);
    }
    // 安全顺序：先原地改名让出路径（失败=什么都没发生，可直接中止）→ 建链
    // （失败=立刻把改名原件改回原名，一切复原）→ 成功后才删除改名的原件。
    // 不再用"先删到回收站再建链"——回收站还原对系统保护目录（如
    // C:\Program Files\WindowsApps）会被权限拒绝，一旦建链失败文件就等于
    // 拿不回来；而同目录改名/改回只是改父目录的目录项，几乎不会失败。
    let backup_path = rename_aside_for_symlink(path)?;
    if let Err(e) = create_symlink(path, target, is_dir) {
        let name = Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        let msg = if std::fs::rename(&backup_path, path).is_ok() {
            crate::applog::log(&format!("[file_ops] 建链失败，已把改名原件恢复原名: {backup_path} -> {path}"));
            format!("创建符号链接失败，原文件已自动恢复原名（数据未受任何影响），真实数据在 {target}: {e}")
        } else {
            format!("创建符号链接失败，且自动恢复原名失败！原文件保留在 {backup_path}，请手动把它改回「{name}」: {e}")
        };
        crate::applog::log(&format!("[file_ops] {msg}"));
        return Err(msg);
    }
    let del = if is_dir {
        std::fs::remove_dir_all(&backup_path)
    } else {
        std::fs::remove_file(&backup_path)
    };
    if let Err(e) = del {
        crate::applog::log(&format!(
            "[file_ops] 符号链接已创建成功，但清理改名原件失败（不影响链接使用，确认无误后可手动删除）: {backup_path}: {e}"
        ));
    }
    Ok(())
}

/// 给"创建符号链接前的安全改名"生成备份名：`{主名}_{文件名哈希8位}_{时间戳}{扩展名}`。
/// 文件名哈希 + 时间戳双保险，保证几乎不会和目录里现有条目重名（万一重名由
/// 调用方再加序号兜底）。保留扩展名是为了万一需要人工介入时，文件仍然可以直接打开。
fn backup_name_for(path: &str) -> String {
    let p = Path::new(path);
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "item".to_string());
    let mut hasher = blake3::Hasher::new();
    hasher.update(name.as_bytes());
    let hex = hasher.finalize().to_hex().to_string();
    let hash8 = &hex[..8];
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let (stem, ext) = match p.extension() {
        Some(ext) => (
            &name[..name.len() - ext.to_string_lossy().len() - 1],
            format!(".{}", ext.to_string_lossy()),
        ),
        None => (name.as_str(), String::new()),
    };
    format!("{stem}_{hash8}_{ts}{ext}")
}

/// 把 path 原地改名为安全备份名（同目录改名，同盘瞬间完成），让出原路径给
/// 符号链接。改名失败时返回错误——此时磁盘上什么都没变，调用方可以安全中止
/// 整个操作；这比"删到回收站"安全得多：回收站还原对系统保护目录会被权限
/// 拒绝，而同目录改名/改回只是改父目录的目录项，几乎不会失败。
fn rename_aside_for_symlink(path: &str) -> Result<String, String> {
    let p = Path::new(path);
    let parent = p.parent().ok_or_else(|| format!("无法解析上级目录: {path}"))?;
    let base = backup_name_for(path);
    let mut candidate = parent.join(&base);
    let mut n = 1u32;
    while candidate.exists() {
        n += 1;
        candidate = parent.join(format!("{base}_{n}"));
    }
    std::fs::rename(p, &candidate).map_err(|e| format!("原地改名失败（原文件未做任何改动）: {e}"))?;
    let s = candidate.to_string_lossy().into_owned();
    crate::applog::log(&format!("[file_ops] 已安全改名让位: {path} -> {s}"));
    Ok(s)
}

/// 一条"符号链接还原记录"：链接在哪个路径、真实数据在哪里、原来是文件还是文件夹。
pub struct RestoreEntry {
    pub link_path: String,
    pub target_path: String,
    pub is_dir: bool,
}

const RESTORE_STEM_PREFIX: &str = "DiskForgeRestoreLink";

/// 还原脚本三件套（bat/ps1/csv）共用文件主干名：
/// - 文件夹迁移（有 display_name）：`{主名}_{源文件夹名}({目录哈希8位})_{时间戳}`
///   ——多个文件夹的脚本会放在同一个 `{base}\{盘符}\` 目录里，带源名才能一眼辨认
/// - 文件迁移（无 display_name）：`{主名}_{内容哈希8位}_{时间戳}`
///   ——脚本在内容哈希目录里，不同名但内容相同的文件共享同一目录，带某个
///   文件名反而误导；用户直接搜索真实数据文件即可定位
///
/// 同目录同主题固定同名（重复迁移时覆盖复用、CSV 累计去重）。
fn restore_stem(dir: &Path, display_name: Option<&str>) -> String {
    // 清理 Windows 文件名非法字符
    let safe = |s: &str| -> String {
        s.chars()
            .map(|c| if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
            .collect()
    };
    // 同目录已有同类三件套时复用其整个主干名，保证同主题始终只有一套文件
    if let Ok(rd) = std::fs::read_dir(dir) {
        for item in rd.flatten() {
            let name = item.file_name().to_string_lossy().into_owned();
            let Some(rest) = name
                .strip_prefix(RESTORE_STEM_PREFIX)
                .and_then(|r| r.strip_suffix(".bat"))
                .filter(|r| r.starts_with('_'))
            else {
                continue;
            };
            let kind_match = match display_name {
                // 文件夹：只复用"同名文件夹"的那套脚本（名字后面必须紧跟 8 位哈希）
                Some(src) => rest
                    .strip_prefix(&format!("_{}_", safe(src)))
                    .is_some_and(|h| h.len() > 9 && h.as_bytes()[8] == b'_' && h[..8].bytes().all(|b| b.is_ascii_hexdigit())),
                // 文件：只复用不带文件夹名的主干（内容哈希目录），即 _哈希8位_时间戳
                None => rest.len() > 9
                    && rest.as_bytes()[9] == b'_'
                    && rest[1..9].bytes().all(|b| b.is_ascii_hexdigit()),
            };
            if kind_match {
                return format!("{RESTORE_STEM_PREFIX}{rest}");
            }
        }
    }
    let hash8 = &blake3::hash(dir.to_string_lossy().as_bytes()).to_hex()[..8];
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    match display_name {
        Some(src) => format!("{RESTORE_STEM_PREFIX}_{}_{hash8}_{ts}", safe(src)),
        None => format!("{RESTORE_STEM_PREFIX}_{hash8}_{ts}"),
    }
}

/// 根据创建符号链接的记录，在真实数据所在目录（第一个记录的 target 的上级
/// 目录）生成"一键还原"脚本三件套：
///
/// 1. DiskForgeRestoreLink_文件夹名_哈希_*.bat —— 文件夹迁移用（带源名便于辨认）
/// 2. DiskForgeRestoreLink_哈希_*.bat / .ps1 / .csv —— 文件迁移用（不带文件名，
///    因为不同名但内容相同的重复文件共享同一个内容哈希目录）
///    双击入口内容纯 ASCII（任何代码页都不会乱码），自动通过 UAC 申请管理员权限
/// 3. CSV —— UTF-8+BOM 的还原记录（可累计多批，按链接去重），ps1 一次性全部还原
///
/// ps1 会一次性自动还原 CSV 里的全部记录（不用一条条手选），即创建符号链接
/// 的反向操作：删链接 -> 把真实数据复制回原位置。
///
/// 返回 bat 的完整路径。
pub fn write_restore_bat(entries: &[RestoreEntry]) -> Result<String, String> {
    let Some(first) = entries.first() else {
        return Err("没有可写入的还原记录".to_string());
    };
    let dir = Path::new(&first.target_path)
        .parent()
        .ok_or_else(|| "无法解析真实数据所在目录".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("创建还原脚本目录失败: {e}"))?;

    // 1) bat 启动器：纯 ASCII + CRLF，永远不会因为代码页/中文乱码而出错
    // 文件夹迁移带源文件夹名（多套脚本共目录时便于辨认）；文件迁移不带
    //（内容哈希目录里可能有多套不同名重复文件的脚本，避免误导）
    let display_name = if first.is_dir {
        Path::new(&first.link_path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    } else {
        None
    };
    let stem = restore_stem(dir, display_name.as_deref());
    let bat_name = format!("{stem}.bat");
    let ps1_name = format!("{stem}.ps1");
    let csv_name = format!("{stem}.csv");
    let bat_path = dir.join(&bat_name);
    let bat = RESTORE_BAT_CONTENT.replace("__RESTORE_PS1_NAME__", &ps1_name);
    std::fs::write(&bat_path, bat.replace('\n', "\r\n"))
        .map_err(|e| format!("写还原启动脚本 {bat_name} 失败: {e}"))?;
    // 2) ps1 还原脚本：UTF-8 + BOM，Windows PowerShell 5.1 能正确解析中文
    let ps1_path = dir.join(&ps1_name);
    let ps1 = RESTORE_PS1_CONTENT.replace("__RESTORE_CSV_NAME__", &csv_name);
    let mut bytes: Vec<u8> = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(ps1.replace('\n', "\r\n").as_bytes());
    std::fs::write(&ps1_path, bytes).map_err(|e| format!("写还原脚本 {ps1_name} 失败: {e}"))?;
    // 3) CSV 还原记录：按 (链接,目标,类型) 去重后追加，支持多批累计
    append_restore_csv(dir, &csv_name, entries)?;

    let p = bat_path.to_string_lossy().into_owned();
    crate::applog::log(&format!("[file_ops] 已更新一键还原脚本: {p}（本次 {} 条还原记录）", entries.len()));
    Ok(p)
}

fn csv_field(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn csv_line(fields: &[&str]) -> String {
    let mut line = fields.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(",");
    line.push_str("\r\n");
    line
}

fn append_restore_csv(dir: &Path, csv_name: &str, entries: &[RestoreEntry]) -> Result<(), String> {
    use std::collections::HashSet;
    use std::io::Write;

    let csv_path = dir.join(csv_name);
    let mut existing_keys: HashSet<String> = HashSet::new();
    if csv_path.exists() {
        let bytes = std::fs::read(&csv_path).map_err(|e| format!("读取还原记录 CSV 失败: {e}"))?;
        // 我们自己的写入器对每个字段都加了引号，且 Windows 路径不允许出现双引号，
        // 所以按 ","（引号+逗号+引号）分列是安全的
        let content = String::from_utf8_lossy(&bytes);
        for line in content.trim_start_matches('\u{feff}').lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("\"created_at\"") {
                continue;
            }
            let fields: Vec<&str> = line.split("\",\"").collect();
            if fields.len() >= 4 {
                let link = fields[1].trim_start_matches('"');
                let target = fields[2];
                let typ = fields[3].trim_end_matches('"');
                existing_keys.insert(format!("{link}\u{0}{target}\u{0}{typ}"));
            }
        }
    } else {
        let mut bytes: Vec<u8> = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(csv_line(&["created_at", "link_path", "target_path", "type"]).as_bytes());
        std::fs::write(&csv_path, bytes).map_err(|e| format!("创建还原记录 CSV 失败: {e}"))?;
    }

    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut new_lines = String::new();
    for e in entries {
        let typ = if e.is_dir { "dir" } else { "file" };
        let key = format!("{}\u{0}{}\u{0}{}", e.link_path, e.target_path, typ);
        // insert 返回 false 说明已存在（文件里或本批前面已有），跳过避免重复还原
        if !existing_keys.insert(key) {
            continue;
        }
        new_lines.push_str(&csv_line(&[&stamp, &e.link_path, &e.target_path, typ]));
    }
    if new_lines.is_empty() {
        return Ok(());
    }
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&csv_path)
        .map_err(|e| format!("打开还原记录 CSV 失败: {e}"))?;
    f.write_all(new_lines.as_bytes()).map_err(|e| format!("写入还原记录 CSV 失败: {e}"))?;
    Ok(())
}

const RESTORE_BAT_CONTENT: &str = r#"@echo off
rem ============================================================
rem  DiskForge one-click symlink restore (launcher).
rem  This file is ASCII-only on purpose: it can never be broken
rem  by codepage/encoding issues. Double-click to run.
rem  It auto-requests administrator rights via UAC (some links
rem  live in protected folders), then runs restore_symlinks.ps1
rem  located in the same folder.
rem ============================================================
setlocal
fsutil dirty query %systemdrive% >nul 2>&1
if %errorlevel% neq 0 (
    echo Requesting administrator privileges, please confirm the UAC prompt...
    powershell -NoProfile -Command "Start-Process -FilePath '%~f0' -Verb RunAs"
    if not errorlevel 1 exit /b
    echo.
    echo [!] Elevation declined or failed. Continuing WITHOUT admin rights...
    echo     If some items fail later, close this window, right-click
    echo     this bat and choose "Run as administrator".
    echo.
)
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0__RESTORE_PS1_NAME__"
"#;

const RESTORE_PS1_CONTENT: &str = r##"# ============================================================
#  DiskForge 一键还原符号链接脚本（由 DiskForge 自动生成）
#  用法：双击同目录下的 DiskForgeRestoreLink_*.bat（会自动申请管理员权限）
#  作用：把本目录 symlink_records.csv 里记录的符号链接【全部自动】还原成
#        真实文件/文件夹（删除链接 -> 把 DiskForge 里的存档数据复制回原位）
#  说明：还原成功后 DiskForge 里的存档副本仍会保留。请先确认相关软件一切
#        正常，再自行删除对应的存档文件夹来释放空间。
# ============================================================

$ErrorActionPreference = 'Continue'
$csvPath = Join-Path $PSScriptRoot '__RESTORE_CSV_NAME__'

Write-Host ''
Write-Host 'DiskForge 符号链接一键还原' -ForegroundColor Cyan
Write-Host '=========================='

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$isAdmin = ([Security.Principal.WindowsPrincipal]$identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host '[提示] 当前不是管理员权限，还原受保护目录（如 Program Files）可能失败。' -ForegroundColor Yellow
    Write-Host '       建议关闭本窗口，右键本目录下的 .bat 文件选择"以管理员身份运行"。' -ForegroundColor Yellow
}

if (-not (Test-Path -LiteralPath $csvPath)) {
    Write-Host ''
    Write-Host "没有找到符号链接记录文件：$csvPath" -ForegroundColor Yellow
    Read-Host '按回车键退出' | Out-Null
    exit 1
}

$records = @(Import-Csv -LiteralPath $csvPath | Where-Object { $_.link_path -and $_.target_path })
if ($records.Count -eq 0) {
    Write-Host '记录文件是空的，没有需要还原的内容。'
    Read-Host '按回车键退出' | Out-Null
    exit
}

Write-Host ''
Write-Host "共 $($records.Count) 条符号链接记录，将【全部自动还原】（无需逐条选择）。"
Write-Host '还原前请先关闭正在使用这些文件的程序，否则复制可能失败。'
$confirm = Read-Host '确认开始还原? (Y=开始 / N=退出)'
if ($confirm.Trim().ToUpper() -ne 'Y') {
    Write-Host '已取消，未做任何改动。'
    Read-Host '按回车键退出' | Out-Null
    exit
}
Write-Host ''

$ok = 0
$failed = 0
for ($i = 0; $i -lt $records.Count; $i++) {
    $r = $records[$i]
    $link = $r.link_path
    $target = $r.target_path
    $typeName = '文件'
    if ($r.type -eq 'dir') { $typeName = '文件夹' }
    Write-Host "[ $($i + 1)/$($records.Count) ] 还原${typeName}: $link"

    try {
        $item = Get-Item -LiteralPath $link -Force -ErrorAction SilentlyContinue
        if ($null -ne $item) {
            if (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -eq 0) {
                Write-Host '    [跳过] 原位置已存在同名真实文件/文件夹（不是符号链接），为安全起见不覆盖。' -ForegroundColor Yellow
                $failed++
                continue
            }
            # 只删除链接本身（reparse point），绝不会触碰 DiskForge 里的存档数据
            if ($item.PSIsContainer) {
                [System.IO.Directory]::Delete($item.FullName, $false)
            } else {
                [System.IO.File]::Delete($item.FullName)
            }
        }
        if (-not (Test-Path -LiteralPath $target)) {
            Write-Host '    [失败] 找不到 DiskForge 里的存档数据（可能已被移动或删除）。' -ForegroundColor Red
            $failed++
            continue
        }
        if ($r.type -eq 'dir') {
            # robocopy /SL：镜像里的符号链接/junction 子项原样还原成链接（不跟随复制内容）
            robocopy "$target" "$link" /E /SL /R:2 /W:1 /NFL /NDL /NJH /NJS | Out-Null
            if ($LASTEXITCODE -gt 7) { throw "robocopy 复制失败（代码 $LASTEXITCODE）" }
        } else {
            $parent = Split-Path -Parent $link
            if (-not (Test-Path -LiteralPath $parent)) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
            Copy-Item -LiteralPath $target -Destination $link -Force
        }
        Write-Host '    [完成] 已还原。' -ForegroundColor Green
        $ok++
    } catch {
        Write-Host "    [失败] $($_.Exception.Message)" -ForegroundColor Red
        $failed++
    }
}

Write-Host ''
Write-Host '=========================='
Write-Host "结果：成功 $ok 项，失败 $failed 项（共 $($records.Count) 项）。"
if ($failed -eq 0) {
    Write-Host '全部还原成功！DiskForge 里对应的存档副本已不再被链接使用。' -ForegroundColor Green
    Write-Host '请先确认相关软件运行正常，确认无误后可自行删除 DiskForge 里对应的真实数据文件夹来释放空间。'
} else {
    Write-Host '有还原失败的项。最常见原因是权限不够：请右键本目录下的 .bat 文件，选择"以管理员身份运行"后重试。' -ForegroundColor Yellow
    Write-Host '如果以管理员身份运行后仍然失败，请用记事本打开本目录下的 .csv 记录文件，' -ForegroundColor Yellow
    Write-Host '按每行记录的 link_path（链接位置）和 target_path（真实数据位置）手动复制还原。' -ForegroundColor Yellow
}
Read-Host '按回车键退出' | Out-Null
"##;

/// 用 Windows 官方 API 一步解析符号链接/junction/挂载点的最终真实路径。
///
/// 原理（微软文档明确说明"最终路径就是路径被完全解析后的结果"）：
///
/// 1. CreateFileW 打开路径：desiredAccess=0（仅查询元数据，权限要求最低）、
///    FILE_FLAG_BACKUP_SEMANTICS（让目录链接也能打开）、且【不加】
///    FILE_FLAG_OPEN_REPARSE_POINT——这样内核会沿符号链接/junction 链一路
///    跟到底，拿到的句柄就是最终真实目标的句柄，层数由文件系统内核处理
///    （含循环链接检测；Windows 内核对一条路径最多解析 63 个 reparse 点，
///    那是操作系统自身的边界，不是代码里的人为限制）。
/// 2. GetFinalPathNameByHandleW(VOLUME_NAME_DOS) 直接取回最终路径。
///
/// .NET 运行时（System.IO）解析链接目标用的正是同一套调用方式。
/// 打不开（断链/权限不足/目标离线）就直接失败，不做任何凑合的降级。
#[cfg(windows)]
pub fn resolve_symlink_target(path: &str) -> Result<String, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFinalPathNameByHandleW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, VOLUME_NAME_DOS,
    };

    let wide: Vec<u16> = std::ffi::OsStr::new(path).encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0, // 不需要读写内容，只要查询路径元数据
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let err = unsafe { GetLastError() };
        return Err(format!(
            "无法解析这个链接（Win32 错误码 {err}）——链接目标可能已不存在，或没有权限访问"
        ));
    }

    // 缓冲区不设人为长度上限，按 Win32 返回值动态扩容：
    // 返回 0 = 真错误；返回值 >= 缓冲区大小 = 所需大小（含结尾 null），按它
    // 扩容后重试；返回值 < 缓冲区大小 = 成功（实际长度不含结尾 null）。
    // 用闭包包住循环，保证扩容过程中任何提前退出都发生在 CloseHandle 之前。
    let result: Result<String, String> = (|| {
        let mut size: u32 = 0x1000;
        loop {
            let mut buf = vec![0u16; size as usize];
            let ret = unsafe { GetFinalPathNameByHandleW(handle, buf.as_mut_ptr(), size, VOLUME_NAME_DOS) };
            if ret == 0 {
                return Err(format!("获取最终路径失败（Win32 错误码 {}）", unsafe { GetLastError() }));
            }
            if (ret as usize) < buf.len() {
                buf.truncate(ret as usize);
                return Ok(String::from_utf16_lossy(&buf));
            }
            size = ret;
        }
    })();
    unsafe { CloseHandle(handle) };

    let final_path = result?;
    // VOLUME_NAME_DOS 返回的是 \\?\ 前缀路径，转成普通可显示路径
    Ok(if let Some(rest) = final_path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = final_path.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        final_path
    })
}

#[cfg(not(windows))]
pub fn resolve_symlink_target(_path: &str) -> Result<String, String> {
    Err("仅支持 Windows".to_string())
}

pub fn diskforge_base_dir(drive_letter: char) -> String {
    format!("{}:\\DiskForge", drive_letter.to_ascii_uppercase())
}

fn mirrored_folder_target_path(source_path: &str, base_dir: &str) -> Result<String, String> {
    let bytes = source_path.as_bytes();
    if bytes.len() < 3 || bytes[1] != b':' {
        return Err(format!("无法识别的路径格式（不是标准的盘符路径，暂不支持迁移）: {source_path}"));
    }
    let drive = source_path.chars().next().unwrap().to_ascii_uppercase();
    let rest = source_path[2..].trim_start_matches('\\');
    if rest.is_empty() {
        return Err("不支持直接迁移整个分区根目录".to_string());
    }
    Ok(format!("{}\\{drive}\\{rest}", base_dir.trim_end_matches('\\')))
}

pub fn migrate_file_to_symlink(source_path: &str, base_dir: &str) -> Result<String, String> {
    let hash = hash_file_blake3(source_path)?;
    let file_name = Path::new(source_path)
        .file_name()
        .ok_or_else(|| "无法解析文件名".to_string())?
        .to_string_lossy()
        .to_string();
    let target_dir = format!("{}\\{hash}", base_dir.trim_end_matches('\\'));
    let target_path = format!("{target_dir}\\{file_name}");

    if Path::new(&target_path).exists() {
        let existing_hash = hash_file_blake3(&target_path)?;
        if existing_hash != hash {
            let msg = format!("目标位置 {target_path} 已存在但内容不一致（理论上不应该发生），为安全起见中止操作，不会覆盖也不会删除原文件");
            crate::applog::log(&format!("[file_ops] 迁移文件失败: {source_path}: {msg}"));
            return Err(msg);
        }
        crate::applog::log(&format!("[file_ops] 内容寻址目标已存在且校验一致，直接复用: {target_path}"));
    } else {
        std::fs::create_dir_all(&target_dir).map_err(|e| format!("创建目标目录失败: {e}"))?;
        if let Err(e) = std::fs::copy(source_path, &target_path) {
            let _ = std::fs::remove_file(&target_path);
            let _ = std::fs::remove_dir(&target_dir);
            let msg = format!("复制文件失败: {e}");
            crate::applog::log(&format!("[file_ops] 迁移文件失败，已清理残留目标: {source_path} -> {target_path}: {msg}"));
            return Err(msg);
        }
        let copied_hash = hash_file_blake3(&target_path)?;
        if copied_hash != hash {
            let _ = std::fs::remove_file(&target_path);
            let _ = std::fs::remove_dir(&target_dir);
            let msg = "复制后校验失败（内容对不上），已撤销复制，原文件未受影响".to_string();
            crate::applog::log(&format!("[file_ops] 迁移文件校验失败，已清理残留目标: {source_path} -> {target_path}: {msg}"));
            return Err(msg);
        }
    }

    if let Err(e) = replace_with_symlink(source_path, &target_path, false, false) {
        crate::applog::log(&format!("[file_ops] 迁移文件：复制+校验成功，但替换符号链接失败: {source_path} -> {target_path}: {e}"));
        return Err(e);
    }
    crate::applog::log(&format!("[file_ops] 迁移文件成功: {source_path} -> {target_path}"));
    Ok(target_path)
}

/// 迁移整个文件夹为符号链接。
///
/// `reparse_children_ok`：源文件夹里包含符号链接/junction 子项时是否继续
/// （第一次尝试传 false——发现子项会返回 `REPARSE_CONFIRM::` 前缀的错误，
/// 由 UI 弹窗让用户选择；用户确认后传 true 重试，复制时会跳过子链接的
/// 内容并在镜像里重建同样目标的链接，还原时保持"是链接的还原成链接"）。
pub fn migrate_folder_to_symlink(source_path: &str, base_dir: &str, reparse_children_ok: bool) -> Result<String, String> {
    match check_folder_occupied_by_rename(source_path) {
        FolderOccupancy::Free => {}
        FolderOccupancy::Locked => {
            let msg = "文件夹当前被占用（重命名探测：共享/锁冲突），已取消迁移，没有复制任何数据。建议先用右键菜单的\"检测占用\"找到占用的进程，处理完再重试。".to_string();
            crate::applog::log(&format!("[file_ops] 迁移文件夹前置检测发现被占用，已中止: {source_path}"));
            return Err(msg);
        }
        FolderOccupancy::Inconclusive(reason) => {
            crate::applog::log(&format!("[file_ops] 迁移文件夹前置检测结果不确定，继续尝试迁移: {source_path}: {reason}"));
        }
    }

    let target_path = mirrored_folder_target_path(source_path, base_dir)?;
    if Path::new(&target_path).exists() {
        let msg = format!("目标位置 {target_path} 已经存在，可能之前已经迁移过、或者出现了没预料到的情况——为安全起见中止操作，请手动检查这个位置后再重试");
        crate::applog::log(&format!("[file_ops] 迁移文件夹失败: {source_path}: {msg}"));
        return Err(msg);
    }
    if let Some(offender) = find_reparse_entry(Path::new(source_path), source_path)
        .map_err(|e| format!("预扫源文件夹失败（未做任何改动）: {e}"))?
    {
        if !reparse_children_ok {
            // 不直接失败，交给 UI 弹窗让用户决定（继续/终止）
            crate::applog::log(&format!(
                "[file_ops] 迁移文件夹发现 reparse 子项，等待用户确认: {source_path}: {offender}"
            ));
            return Err(format!("REPARSE_CONFIRM::{offender}"));
        }
        crate::applog::log(&format!(
            "[file_ops] 迁移文件夹：用户已确认继续，子链接只重建链接、不复制内容: {source_path}: {offender}"
        ));
    }
    let dst = Path::new(&target_path);
    let Some(parent) = dst.parent() else {
        return Err(format!("无法解析目标路径的上级目录: {target_path}"));
    };
    std::fs::create_dir_all(parent).map_err(|e| format!("创建目标目录失败: {e}"))?;

    let src = Path::new(source_path);
    if let Err(e) = copy_dir_recursive(src, dst, reparse_children_ok) {
        let _ = std::fs::remove_dir_all(dst);
        let msg = format!("复制文件夹失败: {e}");
        crate::applog::log(&format!("[file_ops] 迁移文件夹失败，已清理残留目标: {source_path} -> {target_path}: {msg}"));
        return Err(msg);
    }

    let (src_count, src_size) = count_dir(src).map_err(|e| format!("统计源文件夹失败: {e}"))?;
    let (dst_count, dst_size) = count_dir(dst).map_err(|e| format!("统计目标文件夹失败: {e}"))?;
    if src_count != dst_count || src_size != dst_size {
        let _ = std::fs::remove_dir_all(dst);
        let msg = format!(
            "复制后校验失败（源 {src_count} 个文件/{src_size} 字节，目标 {dst_count} 个文件/{dst_size} 字节），已撤销复制，原文件夹未受影响"
        );
        crate::applog::log(&format!("[file_ops] 迁移文件夹校验失败，已清理残留目标: {source_path} -> {target_path}: {msg}"));
        return Err(msg);
    }

    if let Err(e) = replace_with_symlink(source_path, &target_path, true, false) {
        crate::applog::log(&format!("[file_ops] 迁移文件夹：复制+校验成功，但替换符号链接失败: {source_path} -> {target_path}: {e}"));
        return Err(e);
    }
    crate::applog::log(&format!("[file_ops] 迁移文件夹成功: {source_path} -> {target_path}"));
    Ok(target_path)
}

#[cfg(windows)]
fn find_reparse_entry(root: &Path, root_display: &str) -> Result<Option<String>, std::io::Error> {
    use crate::fs_attrs::FILE_ATTRIBUTE_REPARSE_POINT;
    use std::os::windows::fs::MetadataExt;
    let mut stack: Vec<(std::path::PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let attrs = entry.metadata().map(|m| m.file_attributes()).unwrap_or(0);
            if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                let name = entry.file_name().to_string_lossy().into_owned();
                let full = if rel.is_empty() { name } else { format!("{rel}\\{name}") };
                return Ok(Some(format!("{root_display}\\{full}")));
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                let name = entry.file_name().to_string_lossy().into_owned();
                let child_rel = if rel.is_empty() { name } else { format!("{rel}\\{name}") };
                stack.push((dir.join(entry.file_name()), child_rel));
            }
        }
    }
    Ok(None)
}
#[cfg(not(windows))]
fn find_reparse_entry(_root: &Path, _root_display: &str) -> Result<Option<String>, std::io::Error> {
    Ok(None)
}

fn copy_dir_recursive(src: &Path, dst: &Path, reparse_ok: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    let mut stack: Vec<(std::path::PathBuf, std::path::PathBuf)> = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        for entry in std::fs::read_dir(&src_dir)? {
            let entry = entry?;
            let dst_path = dst_dir.join(entry.file_name());
            #[cfg(windows)]
            {
                // Windows 上符号链接/junction/OneDrive 占位项都是 reparse point。
                // 用户确认后（reparse_ok=true）在镜像里重建同样目标的链接，
                // 不复制链接指向的内容——还原时才能保持"是链接的还原成链接"。
                use std::os::windows::fs::MetadataExt;
                let md = entry.metadata()?;
                if md.file_attributes() & crate::fs_attrs::FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    if reparse_ok {
                        let raw = std::fs::read_link(entry.path())?;
                        let target = raw.to_string_lossy().into_owned();
                        let target = target.strip_prefix(r"\??\").map(str::to_string).unwrap_or(target);
                        let is_dir = md.file_type().is_dir();
                        create_symlink(&dst_path.to_string_lossy(), &target, is_dir)
                            .map_err(std::io::Error::other)?;
                    }
                    continue;
                }
                let file_type = md.file_type();
                if file_type.is_dir() {
                    std::fs::create_dir_all(&dst_path)?;
                    stack.push((entry.path(), dst_path));
                } else if file_type.is_file() {
                    std::fs::copy(entry.path(), &dst_path)?;
                }
                continue;
            }
            #[cfg(not(windows))]
            {
                let file_type = entry.file_type()?;
                if file_type.is_symlink() {
                    continue;
                } else if file_type.is_dir() {
                    std::fs::create_dir_all(&dst_path)?;
                    stack.push((entry.path(), dst_path));
                } else if file_type.is_file() {
                    std::fs::copy(entry.path(), &dst_path)?;
                }
            }
        }
    }
    Ok(())
}

fn count_dir(path: &Path) -> std::io::Result<(u64, u64)> {
    let mut count = 0u64;
    let mut size = 0u64;
    let mut stack: Vec<std::path::PathBuf> = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            #[cfg(windows)]
            {
                // reparse 项（符号链接/junction/占位文件）在源和镜像里都只按
                // 链接本身计一个条目（不深入），两侧统计口径一致，校验才不会误报
                use std::os::windows::fs::MetadataExt;
                let md = entry.metadata()?;
                if md.file_attributes() & crate::fs_attrs::FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    count += 1;
                    continue;
                }
                let file_type = md.file_type();
                if file_type.is_dir() {
                    stack.push(entry.path());
                } else if file_type.is_file() {
                    count += 1;
                    size += md.len();
                }
                continue;
            }
            #[cfg(not(windows))]
            {
                let file_type = entry.file_type()?;
                if file_type.is_symlink() {
                    count += 1;
                    continue;
                } else if file_type.is_dir() {
                    stack.push(entry.path());
                } else if file_type.is_file() {
                    count += 1;
                    size += entry.metadata()?.len();
                }
            }
        }
    }
    Ok((count, size))
}

#[cfg(windows)]
pub fn build_refreshed_symlink_node(path: &str, name: &str, is_dir: bool, old: &crate::model::Node) -> crate::model::Node {
    use crate::model::Node;
    let size = std::fs::symlink_metadata(path).map(|m| m.len()).unwrap_or(0);
    use crate::fs_attrs::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, IO_REPARSE_TAG_SYMLINK};
    let attrs = FILE_ATTRIBUTE_REPARSE_POINT | if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 };
    let node = if is_dir {
        Node::new_folder_with_meta(name, old.color, Vec::new(), old.modified_ft, old.created_ft, old.accessed_ft, attrs, IO_REPARSE_TAG_SYMLINK, false, old.owner.clone())
    } else {
        Node::new_file_with_meta(name, size, size, old.color, old.modified_ft, old.created_ft, old.accessed_ft, attrs, IO_REPARSE_TAG_SYMLINK, false, old.owner.clone())
    };
    match &old.full_path_override {
        Some(_) => node.with_full_path(path.to_string()),
        None => node,
    }
}

#[cfg(not(windows))]
pub fn build_refreshed_symlink_node(_path: &str, _name: &str, _is_dir: bool, old: &crate::model::Node) -> crate::model::Node {
    old.clone()
}


fn to_extended_length_path(path: &str) -> String {
    if path.starts_with(r"\\?\") {
        return path.to_string();
    }
    if let Some(rest) = path.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{rest}")
    } else {
        format!(r"\\?\{path}")
    }
}

#[derive(Clone)]
pub struct LockingProcess {
    pub pid: u32,
    pub app_name: String,
    pub service_name: Option<String>,
}

#[cfg(windows)]
pub fn find_locking_processes(paths: &[&str]) -> Result<Vec<LockingProcess>, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::RestartManager::{
        RmEndSession, RmGetList, RmRegisterResources, RmStartSession, RM_PROCESS_INFO,
    };

    if paths.is_empty() {
        return Ok(Vec::new());
    }

    let mut session_key = [0u16; 64];
    let mut session_handle: u32 = 0;
    let start_ret = unsafe { RmStartSession(&mut session_handle, 0, session_key.as_mut_ptr()) };
    if start_ret != ERROR_SUCCESS {
        return Err(format!("RmStartSession 失败（错误码 {start_ret}）"));
    }
    struct SessionGuard(u32);
    impl Drop for SessionGuard {
        fn drop(&mut self) {
            unsafe { RmEndSession(self.0) };
        }
    }
    let _guard = SessionGuard(session_handle);

    let wide_paths: Vec<Vec<u16>> = paths
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| {
            let extended = to_extended_length_path(p);
            std::ffi::OsStr::new(&extended).encode_wide().chain(std::iter::once(0)).collect()
        })
        .collect();
    if wide_paths.is_empty() {
        return Ok(Vec::new());
    }
    let path_ptrs: Vec<*const u16> = wide_paths.iter().map(|p| p.as_ptr()).collect();

    let reg_ret = unsafe {
        RmRegisterResources(
            session_handle, path_ptrs.len() as u32, path_ptrs.as_ptr(),
            0, std::ptr::null(), 0, std::ptr::null(),
        )
    };
    if reg_ret != ERROR_SUCCESS {
        return Err(format!("RmRegisterResources 失败（错误码 {reg_ret}）"));
    }

    let mut needed: u32 = 0;
    let mut got: u32 = 0;
    let mut reboot_reasons: u32 = 0;
    let first_ret = unsafe { RmGetList(session_handle, &mut needed, &mut got, std::ptr::null_mut(), &mut reboot_reasons) };
    let error_more_data = windows_sys::Win32::Foundation::ERROR_MORE_DATA;
    if first_ret != ERROR_SUCCESS && first_ret != error_more_data {
        return Err(format!("RmGetList（探测数量）失败（错误码 {first_ret}）"));
    }
    if needed == 0 {
        return Ok(Vec::new());
    }

    let mut buf: Vec<RM_PROCESS_INFO> = Vec::with_capacity(needed as usize);
    for _ in 0..needed {
        buf.push(unsafe { std::mem::zeroed() });
    }
    got = needed;
    let second_ret = unsafe { RmGetList(session_handle, &mut needed, &mut got, buf.as_mut_ptr(), &mut reboot_reasons) };
    if second_ret != ERROR_SUCCESS {
        return Err(format!("RmGetList（取列表）失败（错误码 {second_ret}）"));
    }

    let mut result = Vec::with_capacity(got as usize);
    for info in buf.iter().take(got as usize) {
        let app_name = String::from_utf16_lossy(&info.strAppName)
            .trim_end_matches('\0').to_string();
        let service_name = String::from_utf16_lossy(&info.strServiceShortName)
            .trim_end_matches('\0').to_string();
        result.push(LockingProcess {
            pid: info.Process.dwProcessId,
            app_name: if app_name.is_empty() { format!("(PID {})", info.Process.dwProcessId) } else { app_name },
            service_name: if service_name.is_empty() { None } else { Some(service_name) },
        });
    }
    Ok(result)
}

#[cfg(not(windows))]
pub fn find_locking_processes(_paths: &[&str]) -> Result<Vec<LockingProcess>, String> {
    Ok(Vec::new())
}

#[derive(Clone)]
pub enum FolderOccupancy {
    Free,
    Locked,
    Inconclusive(String),
}

#[cfg(windows)]
pub fn check_folder_occupied_by_rename(path: &str) -> FolderOccupancy {
    let p = Path::new(path);
    let Some(parent) = p.parent() else {
        return FolderOccupancy::Inconclusive("无法解析上级目录".to_string());
    };
    let Some(file_name) = p.file_name() else {
        return FolderOccupancy::Inconclusive("无法解析文件夹名".to_string());
    };
    let file_name = file_name.to_string_lossy().to_string();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let temp_path = parent.join(format!("{file_name}.diskforge_lockcheck_{nonce:08x}"));

    match std::fs::rename(p, &temp_path) {
        Ok(()) => {
            if let Err(e) = std::fs::rename(&temp_path, p) {
                let msg = format!(
                    "重命名探测完成后，改回原名失败！文件夹当前的实际路径是: {}，请手动把它改回「{file_name}」: {e}",
                    temp_path.display(),
                );
                crate::applog::log(&format!("[file_ops] {msg}"));
                return FolderOccupancy::Inconclusive(msg);
            }
            FolderOccupancy::Free
        }
        Err(e) => match e.raw_os_error() {
            Some(32) | Some(33) => FolderOccupancy::Locked,
            _ => FolderOccupancy::Inconclusive(format!(
                "重命名探测失败（{e}），可能是权限不足或者这是系统保护的特殊目录，也可能确实被占用，无法进一步确定"
            )),
        },
    }
}

#[cfg(not(windows))]
pub fn check_folder_occupied_by_rename(_path: &str) -> FolderOccupancy {
    FolderOccupancy::Inconclusive("当前平台不支持这项检测".to_string())
}

pub fn describe_locking_processes(procs: &[LockingProcess]) -> String {
    if procs.is_empty() {
        return String::new();
    }
    let names: Vec<String> = procs
        .iter()
        .map(|p| match &p.service_name {
            Some(svc) => format!("{}（服务 {svc}，PID {}）", p.app_name, p.pid),
            None => format!("{}（PID {}）", p.app_name, p.pid),
        })
        .collect();
    format!("被 {} 占用", names.join("、"))
}

pub fn delete_to_recycle_bin_with_lock_check(path: &str) -> Result<(), String> {
    // 不做自动重试：大文件夹删除到回收站可能要跑很久，重试会从头再枚举一遍，
    // 几十秒后再次失败反而浪费时间；删除有系统进度窗口，失败由用户自行重试
    // TODO: 将来可考虑对文件夹失败做"逐项删除降级"（跳过个别被占用的文件，
    //       其余照常删入回收站），等实际需求明确后再实现
    if let Err(e) = delete_to_recycle_bin(path) {
        return match find_locking_processes(&[path]) {
            Ok(procs) if !procs.is_empty() => {
                let detail = describe_locking_processes(&procs);
                // 占用进程分析单独记日志：上面的失败日志在 lock check 之前落盘，
                // 不补这条的话日志文件里就看不到是哪个进程占用
                crate::applog::log(&format!("[file_ops] 删除失败占用分析: {path}: {detail}"));
                Err(format!("{e}；{detail}"))
            }
            _ => Err(e),
        };
    }
    Ok(())
}


#[cfg(windows)]
pub fn terminate_process(pid: u32) -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    if pid == 0 {
        return Err("无效的 PID".to_string());
    }
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            let err = windows_sys::Win32::Foundation::GetLastError();
            return Err(format!("打开进程失败（错误码 {err}，常见原因是权限不够——试试以管理员身份运行）"));
        }
        let ok = TerminateProcess(handle, 1);
        CloseHandle(handle);
        if ok == 0 {
            let err = windows_sys::Win32::Foundation::GetLastError();
            return Err(format!("结束进程失败（错误码 {err}）"));
        }
    }
    crate::applog::log(&format!("[file_ops] 已结束进程 PID {pid}"));
    Ok(())
}

#[cfg(not(windows))]
pub fn terminate_process(_pid: u32) -> Result<(), String> {
    Err("仅支持 Windows".to_string())
}

#[cfg(windows)]
pub fn stop_service(service_name: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, ControlService, OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT,
        SERVICE_CONTROL_STOP, SERVICE_STATUS, SERVICE_STOP,
    };

    if service_name.is_empty() {
        return Err("服务名为空".to_string());
    }
    let name_wide: Vec<u16> = std::ffi::OsStr::new(service_name).encode_wide().chain(std::iter::once(0)).collect();
    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            let err = windows_sys::Win32::Foundation::GetLastError();
            return Err(format!("打开服务控制管理器失败（错误码 {err}，常见原因是权限不够——试试以管理员身份运行）"));
        }
        let svc = OpenServiceW(scm, name_wide.as_ptr(), SERVICE_STOP);
        if svc.is_null() {
            let err = windows_sys::Win32::Foundation::GetLastError();
            CloseServiceHandle(scm);
            return Err(format!("打开服务失败（错误码 {err}）"));
        }
        let mut status: SERVICE_STATUS = std::mem::zeroed();
        let ok = ControlService(svc, SERVICE_CONTROL_STOP, &mut status);
        CloseServiceHandle(svc);
        CloseServiceHandle(scm);
        if ok == 0 {
            let err = windows_sys::Win32::Foundation::GetLastError();
            if err == 1051 {
                return Err("有其它服务依赖这个服务，需要先停掉那些服务（可以打开系统自带的\"服务\"管理器，找到这个服务，在\"依存关系\"标签页里看依赖它的服务有哪些）".to_string());
            }
            return Err(format!("停止服务失败（错误码 {err}）"));
        }
    }
    crate::applog::log(&format!("[file_ops] 已发送停止请求给服务: {service_name}"));
    Ok(())
}

#[cfg(not(windows))]
pub fn stop_service(_service_name: &str) -> Result<(), String> {
    Err("仅支持 Windows".to_string())
}

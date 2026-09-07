
#[cfg(windows)]
pub fn delete_to_recycle_bin(path: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
        FO_DELETE, SHFILEOPSTRUCTW,
    };

    if path.is_empty() {
        return Err("路径为空".to_string());
    }
    let mut from: Vec<u16> = std::ffi::OsStr::new(path).encode_wide().collect();
    from.push(0);
    from.push(0);

    let mut op = SHFILEOPSTRUCTW {
        hwnd: std::ptr::null_mut(),
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        pTo: std::ptr::null(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT) as u16,
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };
    let ret = unsafe { SHFileOperationW(&mut op) };
    crate::applog::log(&format!("[file_ops] 删除到回收站: {path} (ret={ret}, aborted={})", op.fAnyOperationsAborted));
    if ret != 0 {
        return Err(format!("删除失败（错误码 0x{ret:X}）"));
    }
    if op.fAnyOperationsAborted != 0 {
        return Err("操作被取消".to_string());
    }
    Ok(())
}

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
    delete_to_recycle_bin_with_retry(path)?;
    if let Err(e) = create_symlink(path, target, is_dir) {
        let msg = format!("原文件已删除（在回收站里，可以找回），但创建符号链接失败，真实数据在 {target}，请手动处理: {e}");
        crate::applog::log(&format!("[file_ops] {msg}"));
        return Err(msg);
    }
    Ok(())
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

pub fn migrate_folder_to_symlink(source_path: &str, base_dir: &str) -> Result<String, String> {
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
        .map_err(|e| format!("预扫源文件夹失败（未做任何改动）: {e}"))? {
        let msg = format!(
            "文件夹里包含符号链接/junction/OneDrive 占位项（{offender}），这类条目无法被安全地复制到别的盘——为避免迁出一份不完整的镜像，已中止迁移，原文件夹未受任何影响。可以先用资源管理器/检测占用处理这些条目后再迁移剩下的部分"
        );
        crate::applog::log(&format!("[file_ops] 迁移文件夹中止（发现 reparse 子项）: {source_path}: {offender}"));
        return Err(msg);
    }
    let dst = Path::new(&target_path);
    let Some(parent) = dst.parent() else {
        return Err(format!("无法解析目标路径的上级目录: {target_path}"));
    };
    std::fs::create_dir_all(parent).map_err(|e| format!("创建目标目录失败: {e}"))?;

    let src = Path::new(source_path);
    if let Err(e) = copy_dir_recursive(src, dst) {
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

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    let mut stack: Vec<(std::path::PathBuf, std::path::PathBuf)> = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        for entry in std::fs::read_dir(&src_dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let dst_path = dst_dir.join(entry.file_name());
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
    Ok(())
}

fn count_dir(path: &Path) -> std::io::Result<(u64, u64)> {
    let mut count = 0u64;
    let mut size = 0u64;
    let mut stack: Vec<std::path::PathBuf> = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            } else if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                count += 1;
                size += entry.metadata()?.len();
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

pub fn delete_to_recycle_bin_with_retry(path: &str) -> Result<(), String> {
    const RETRIES: u32 = 3;
    let mut last_err = String::new();
    for attempt in 0..=RETRIES {
        match delete_to_recycle_bin(path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = e;
                if attempt < RETRIES {
                    std::thread::sleep(std::time::Duration::from_millis(300 * (attempt as u64 + 1)));
                }
            }
        }
    }
    match find_locking_processes(&[path]) {
        Ok(procs) if !procs.is_empty() => {
            Err(format!("{last_err}；{}", describe_locking_processes(&procs)))
        }
        _ => Err(last_err),
    }
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

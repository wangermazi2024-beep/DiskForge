
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

/// 把原文件/文件夹在同目录内重命名成一个几乎不可能重复的名字：
/// `原名_名字哈希8位_毫秒时间戳`（哈希是文件名的 FNV-1a 32 位十六进制）。
/// 改名后原路径就空出来了，既满足创建符号链接"源必须不存在"的要求，
/// 又保留了一份原件——万一创建符号链接失败，把它改回原名即可完全还原，
/// 不会出现"文件进了回收站却因权限/特殊目录无法还原"的不可挽回局面。
fn quarantine_rename(path: &str) -> Result<std::path::PathBuf, String> {
    let p = Path::new(path);
    let file_name = p
        .file_name()
        .ok_or_else(|| format!("无法解析文件名: {path}"))?
        .to_string_lossy()
        .to_string();
    let parent = p.parent().ok_or_else(|| format!("无法解析上级目录: {path}"))?;
    let mut hash: u32 = 0x811C_9DC5;
    for b in file_name.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut last_err: Option<std::io::Error> = None;
    for attempt in 0..8u32 {
        // 尝试 0 用纯时间戳，之后追加序号，保证即使同一毫秒多次操作也不重名
        let candidate_name = if attempt == 0 {
            format!("{file_name}_{hash:08x}_{ts}")
        } else {
            format!("{file_name}_{hash:08x}_{ts}_{attempt}")
        };
        let candidate = parent.join(&candidate_name);
        if candidate.exists() {
            continue;
        }
        match std::fs::rename(p, &candidate) {
            Ok(()) => {
                crate::applog::log(&format!("[file_ops] 已把原文件改名挪开: {path} -> {}", candidate.display()));
                return Ok(candidate);
            }
            Err(e) if matches!(e.raw_os_error(), Some(32) | Some(33)) => {
                // 共享冲突：文件正被占用，短暂等待后重试
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let e = last_err.map(|e| e.to_string()).unwrap_or_else(|| "重试次数用尽".to_string());
    Err(e)
}

pub fn replace_with_symlink(path: &str, target: &str, is_dir: bool, verify_content: bool) -> Result<(), String> {
    if verify_content && !is_dir && !crate::dedup::files_identical(path, target) {
        let msg = format!(
            "副本 {path} 与真身 {target} 的内容已经不一致（可能在扫描之后被修改过），为防止丢失新内容已中止对这个副本的替换，原文件保留不动"
        );
        crate::applog::log(&format!("[file_ops] 替换符号链接前复查失败: {msg}"));
        return Err(msg);
    }
    // 第一步：先把原文件改名"挪开"（不是删除！），此时任何数据都没有丢失
    let quarantined = match quarantine_rename(path) {
        Ok(q) => q,
        Err(e) => {
            let hint = match find_locking_processes(&[path]) {
                Ok(procs) if !procs.is_empty() => format!("；{}", describe_locking_processes(&procs)),
                _ => String::new(),
            };
            let msg = format!("无法把原文件改名挪开（未做任何改动，原文件完好无损）: {e}{hint}");
            crate::applog::log(&format!("[file_ops] 替换符号链接中止: {msg}"));
            return Err(msg);
        }
    };
    // 第二步：创建符号链接；一旦失败立刻把原件改回原名，全程零损失
    if let Err(e) = create_symlink(path, target, is_dir) {
        return match std::fs::rename(&quarantined, path) {
            Ok(()) => {
                let msg = format!("创建符号链接失败，已自动把原文件改回原位，没有造成任何丢失: {e}");
                crate::applog::log(&format!("[file_ops] {msg}"));
                Err(msg)
            }
            Err(re) => {
                let msg = format!(
                    "创建符号链接失败，且自动还原也失败了！原文件被安全地改名放在: {}，请手动把它改回「{path}」。创建失败原因: {e}；还原失败原因: {re}",
                    quarantined.display(),
                );
                crate::applog::log(&format!("[file_ops] {msg}"));
                Err(msg)
            }
        };
    }
    // 第三步：创建成功，删掉改名后的那份原件（真身已复制/校验存放在 target）
    let cleanup = if is_dir {
        std::fs::remove_dir_all(&quarantined)
    } else {
        std::fs::remove_file(&quarantined)
    };
    if let Err(e) = cleanup {
        crate::applog::log(&format!(
            "[file_ops] 符号链接已创建成功，但清理改名后的原文件失败（不影响使用，可稍后手动删除）: {}: {e}",
            quarantined.display(),
        ));
    }
    // 第四步：把这条符号链接记入还原记录，并确保目标目录下有一键还原脚本
    if let Err(e) = record_symlink_for_restore(path, target, is_dir) {
        crate::applog::log(&format!("[file_ops] 写入符号链接还原记录失败（不影响已创建的符号链接）: {e}"));
    }
    Ok(())
}

pub fn diskforge_base_dir(drive_letter: char) -> String {
    format!("{}:\\DiskForge", drive_letter.to_ascii_uppercase())
}

// ---------------------------------------------------------------------------
// 符号链接还原记录 + 一键还原脚本
//
// 背景：把某些软件的核心文件（例如 msedge.dll）迁移成符号链接后，个别软件会
// 因为不兼容符号链接而运行不正常。为了给用户"反悔/拯救"的机会：
//   1. 每次成功创建符号链接，都把 (符号链接路径, 真实数据路径) 追加写入
//      目标分区的 DiskForge 目录下的 symlink_records.csv；
//   2. 首次记录时自动生成两个不需要任何额外环境的还原脚本：
//      - DiskForge还原符号链接.bat：双击即可启动（内部只是调用 PowerShell）
//      - restore_symlinks.ps1：按 CSV 记录逐条还原（删链接 -> 把真实数据复制回原位）
// ---------------------------------------------------------------------------

const RESTORE_BAT_NAME: &str = "DiskForge还原符号链接.bat";
const RESTORE_PS1_NAME: &str = "restore_symlinks.ps1";
const RESTORE_CSV_NAME: &str = "symlink_records.csv";

static RESTORE_RECORD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 从符号链接目标路径推导 DiskForge 根目录（所有目标都存放在 X:\DiskForge 下）
fn base_dir_from_target(target: &str) -> Option<String> {
    let mut it = target.splitn(3, '\\');
    let drive = it.next()?;
    let root = it.next()?;
    if root.eq_ignore_ascii_case("DiskForge") {
        Some(format!("{drive}\\{root}"))
    } else {
        None
    }
}

fn csv_field(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn csv_line(fields: &[&str]) -> String {
    let mut line = fields.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(",");
    line.push_str("\r\n");
    line
}

const RESTORE_BAT_CONTENT: &str = r#"@echo off
rem =====================================================
rem  DiskForge - one-click symlink restore launcher
rem  Double-click this file to restore the symlinks that
rem  were created by DiskForge. It simply runs the
rem  PowerShell restore script in the same folder.
rem =====================================================
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0restore_symlinks.ps1"
"#;

const RESTORE_PS1_CONTENT: &str = r##"# ============================================================
#  DiskForge 一键还原符号链接脚本（由 DiskForge 自动生成）
#  用法：双击同目录下的 DiskForge还原符号链接.bat 即可启动
#  作用：把 DiskForge 创建的符号链接还原成真实文件/文件夹
#        （删除链接 -> 把 DiskForge 里存档的真实数据复制回原位）
#  说明：还原时默认保留 DiskForge 里的存档副本；确认相关软件
#        一切正常后，可以自行删除整个 DiskForge 目录。
# ============================================================

$ErrorActionPreference = 'Stop'
$csvPath = Join-Path $PSScriptRoot 'symlink_records.csv'

Write-Host ''
Write-Host 'DiskForge 符号链接还原工具' -ForegroundColor Cyan
Write-Host '=========================='
Write-Host '本脚本会把 DiskForge 创建的符号链接还原成真实文件/文件夹。'

if (-not (Test-Path -LiteralPath $csvPath)) {
    Write-Host ''
    Write-Host "没有找到符号链接创建记录：$csvPath" -ForegroundColor Yellow
    Write-Host '可能还没有用 DiskForge 创建过符号链接。'
    Read-Host '按回车键退出' | Out-Null
    exit 1
}

$records = @(Import-Csv -LiteralPath $csvPath)
if ($records.Count -eq 0) {
    Write-Host '记录文件是空的，没有需要还原的内容。'
    Read-Host '按回车键退出' | Out-Null
    exit
}

Write-Host ''
Write-Host "共找到 $($records.Count) 条创建记录。"
Write-Host '每一条都可以选择：  Y=还原  N=跳过  A=还原之后全部  Q=退出'
Write-Host ''

$restoreAll = $false
$quit = $false
$ok = 0
$skipped = 0
$failed = 0
for ($i = 0; $i -lt $records.Count; $i++) {
    $r = $records[$i]
    $link = $r.link_path
    $target = $r.target_path
    $type = $r.type
    $typeName = '文件'
    if ($type -eq 'dir') { $typeName = '文件夹' }
    Write-Host "[$($i + 1)/$($records.Count)] 类型: $typeName"
    Write-Host "  符号链接: $link"
    Write-Host "  真实数据: $target"

    $doRestore = $restoreAll
    if (-not $doRestore) {
        $ans = (Read-Host '  还原这一项吗? (Y/N/A/Q)').Trim().ToUpper()
        if ($ans -eq 'Q') {
            $quit = $true
        } elseif ($ans -eq 'A') {
            $restoreAll = $true
            $doRestore = $true
        } elseif ($ans -eq 'Y') {
            $doRestore = $true
        } else {
            Write-Host '  已跳过。' -ForegroundColor DarkGray
            $skipped++
        }
    }
    if ($quit -or -not $doRestore) { continue }

    try {
        $item = Get-Item -LiteralPath $link -Force -ErrorAction SilentlyContinue
        if ($null -ne $item) {
            if (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -eq 0) {
                Write-Host '  !! 原位置存在同名真实文件/文件夹（不是符号链接），为安全起见跳过。' -ForegroundColor Yellow
                $failed++
                continue
            }
            # 只删除链接本身，不会触碰 DiskForge 里的存档数据
            if ($item.PSIsContainer) {
                [System.IO.Directory]::Delete($item.FullName, $false)
            } else {
                [System.IO.File]::Delete($item.FullName)
            }
        }
        if (-not (Test-Path -LiteralPath $target)) {
            Write-Host '  !! 找不到 DiskForge 里的存档数据（可能已被移动或删除），跳过。' -ForegroundColor Red
            $failed++
            continue
        }
        if ($type -eq 'dir') {
            Copy-Item -LiteralPath $target -Destination $link -Recurse -Force
        } else {
            $parent = Split-Path -Parent $link
            if (-not (Test-Path -LiteralPath $parent)) { New-Item -ItemType Directory -Path $parent -Force | Out-Null }
            Copy-Item -LiteralPath $target -Destination $link -Force
        }
        Write-Host '  已还原（DiskForge 里的存档副本保留）。' -ForegroundColor Green
        $ok++
    } catch {
        Write-Host "  !! 还原失败: $($_.Exception.Message)" -ForegroundColor Red
        $failed++
    }
}

Write-Host ''
Write-Host "完成：成功 $ok，跳过 $skipped，失败 $failed。"
if ($failed -gt 0) {
    Write-Host '有失败项：最常见的原因是权限不够，可以右键 DiskForge还原符号链接.bat，选择"以管理员身份运行"再试。' -ForegroundColor Yellow
}
Read-Host '按回车键退出' | Out-Null
"##;

/// 确保目标分区的 DiskForge 根目录下有一键还原脚本（bat + ps1）和带 BOM 的记录 CSV
fn ensure_restore_scripts(base_dir: &str) -> Result<(), String> {
    std::fs::create_dir_all(base_dir).map_err(|e| format!("创建 DiskForge 根目录失败: {e}"))?;
    let bat_path = Path::new(base_dir).join(RESTORE_BAT_NAME);
    if !bat_path.exists() {
        // bat 内容保持纯 ASCII 并用 CRLF，避免任何代码页/编码问题
        std::fs::write(&bat_path, RESTORE_BAT_CONTENT.replace('\n', "\r\n"))
            .map_err(|e| format!("写还原脚本 {RESTORE_BAT_NAME} 失败: {e}"))?;
        crate::applog::log(&format!("[file_ops] 已生成一键还原脚本: {}", bat_path.display()));
    }
    let ps1_path = Path::new(base_dir).join(RESTORE_PS1_NAME);
    if !ps1_path.exists() {
        // ps1 用 UTF-8 + BOM，Windows PowerShell 5.1 才能正确解析中文
        let mut bytes: Vec<u8> = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(RESTORE_PS1_CONTENT.replace('\n', "\r\n").as_bytes());
        std::fs::write(&ps1_path, bytes).map_err(|e| format!("写还原脚本 {RESTORE_PS1_NAME} 失败: {e}"))?;
        crate::applog::log(&format!("[file_ops] 已生成还原脚本: {}", ps1_path.display()));
    }
    let csv_path = Path::new(base_dir).join(RESTORE_CSV_NAME);
    if !csv_path.exists() {
        let mut bytes: Vec<u8> = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(csv_line(&["created_at", "link_path", "target_path", "type"]).as_bytes());
        std::fs::write(&csv_path, bytes).map_err(|e| format!("创建还原记录 CSV 失败: {e}"))?;
    }
    Ok(())
}

/// 把一次成功创建的符号链接追加写入还原记录，并确保还原脚本存在。
/// 之后用户只要到目标分区 DiskForge 目录双击「DiskForge还原符号链接.bat」就能按记录还原。
pub fn record_symlink_for_restore(link_path: &str, target_path: &str, is_dir: bool) -> Result<(), String> {
    let Some(base_dir) = base_dir_from_target(target_path) else {
        return Err(format!("无法从目标路径推导 DiskForge 根目录: {target_path}"));
    };
    let _guard = RESTORE_RECORD_LOCK.lock().map_err(|_| "还原记录锁异常".to_string())?;
    ensure_restore_scripts(&base_dir)?;
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let line = csv_line(&[&stamp, link_path, target_path, if is_dir { "dir" } else { "file" }]);
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(Path::new(&base_dir).join(RESTORE_CSV_NAME))
        .map_err(|e| format!("打开还原记录 CSV 失败: {e}"))?;
    f.write_all(line.as_bytes()).map_err(|e| format!("写入还原记录 CSV 失败: {e}"))?;
    crate::applog::log(&format!("[file_ops] 已记入还原记录: {link_path} -> {target_path}"));
    Ok(())
}

/// 返回一键还原脚本（bat）的完整路径；目标路径不在 X:\DiskForge 下时返回 None。
/// 供 UI 在创建成功后提示用户"去哪里找后悔药"。
pub fn restore_bat_path_for(target_path: &str) -> Option<String> {
    base_dir_from_target(target_path).map(|base| format!("{base}\\{RESTORE_BAT_NAME}"))
}

/// 在资源管理器中定位（选中）指定路径——"定位真实路径"功能用
#[cfg(windows)]
pub fn locate_in_explorer(path: &str) {
    use std::os::windows::process::CommandExt;
    if path.is_empty() { return; }
    let arg = format!("/select,\"{path}\"");
    crate::applog::log(&format!("[file_ops] 定位真实路径: explorer {arg}"));
    if let Err(e) = std::process::Command::new("explorer").raw_arg(&arg).spawn() {
        crate::applog::log(&format!("[file_ops] 定位真实路径失败 ({path}): {e}"));
    }
}
#[cfg(not(windows))]
pub fn locate_in_explorer(_path: &str) {}

/// 解析符号链接/junction 的真实路径（沿链接链逐级解析，最多 16 层）。
/// 返回 (真实路径, 是否为目录)；起点本身不是符号链接时返回 Err。
pub fn resolve_symlink_target(path: &str) -> Result<(String, bool), String> {
    let mut current = std::path::PathBuf::from(path);
    let mut hops = 0usize;
    loop {
        let meta = std::fs::symlink_metadata(&current)
            .map_err(|e| format!("无法读取 {}: {e}", current.display()))?;
        if !meta.file_type().is_symlink() {
            break;
        }
        hops += 1;
        if hops > 16 {
            return Err("符号链接嵌套超过 16 层（疑似循环链接），已停止解析".to_string());
        }
        let link_target = normalize_reparse_target(
            std::fs::read_link(&current).map_err(|e| format!("读取链接目标失败: {e}"))?,
        );
        let next = if link_target.is_absolute() {
            link_target
        } else {
            let parent = current.parent().map(|p| p.to_path_buf()).unwrap_or_default();
            parent.join(link_target)
        };
        current = next;
    }
    if hops == 0 {
        return Err("这一项本身不是符号链接".to_string());
    }
    let is_dir = std::fs::metadata(&current).map(|m| m.is_dir()).unwrap_or(false);
    Ok((current.to_string_lossy().into_owned(), is_dir))
}

/// 去掉链接目标里的 \\?\ / \??\ 前缀（junction 和命令行创建的链接常见），转成普通可显示路径
fn normalize_reparse_target(p: std::path::PathBuf) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    let s = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else if let Some(rest) = s.strip_prefix(r"\??\") {
        rest.to_string()
    } else {
        s.into_owned()
    };
    std::path::PathBuf::from(s)
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

//! NTFS MFT 直读扫描。
//!
//! ## 算法
//!
//! **核心设计：两阶段 + 哈希表聚合**
//!
//! 1. **第一阶段（load_volume）**：读整张 MFT，对每条记录解析属性，
//!    把属性聚合到 **base record** 的哈希表条目里（扩展记录的属性自动写到 base record）。
//!    - `base_file_records: HashMap<record_number, FileRecordBase>` — 每个文件的属性
//!    - `parent_to_children: HashMap<parent_record_number, Vec<(name, base_record)>>` — 父→子映射
//!
//! 2. **第二阶段（build_tree）**：从根目录（record 5）开始，用 `parent_to_children` 找子项，
//!    用 `base_file_records` 拿属性，构建 Node 树。遍历用显式 Frame 栈迭代
//!    （不用原生递归：后台线程默认栈只有 2MB，超深目录链会把栈打穿）。
//!
//! **关键**：不需要解析 `$ATTRIBUTE_LIST`！因为扩展记录的 `$DATA` 属性在第一阶段
//! 就已经聚合到 base record 的 `FileRecordBase` 里了。
//!
//! ## MFT 物理读取
//! - 打开卷设备 `\\.\C:`（`FILE_READ_DATA | FILE_READ_ATTRIBUTES`，`FILE_FLAG_NO_BUFFERING`）
//! - `FSCTL_GET_NTFS_VOLUME_DATA` 拿卷信息
//! - 打开 `\\.\C:\$MFT::$DATA`（`FILE_READ_ATTRIBUTES`）+ `FSCTL_GET_RETRIEVAL_POINTERS` 拿 MFT 簇映射
//!   （内核禁止对 $MFT 数据流的 FILE_READ_DATA 访问——管理员也一样，所以只能
//!   拿属性访问权读 run 映射，再用卷句柄直读物理簇；详见 `load_volume` 内注释）
//! - 按 run 顺序读 MFT
//!
//! **不需要 SeBackupPrivilege**，只要管理员身份。

#![cfg(windows)]

use std::collections::HashMap;
use std::ptr::null_mut;
use std::sync::mpsc::Sender;

use egui::Color32;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_MORE_DATA, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, SetFilePointerEx, FILE_BEGIN,
    FILE_FLAG_NO_BUFFERING, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_GET_RETRIEVAL_POINTERS, NTFS_VOLUME_DATA_BUFFER,
    RETRIEVAL_POINTERS_BUFFER, STARTING_VCN_INPUT_BUFFER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::model::Node;

/// NTFS 根目录的 MFT 记录号固定是 5。
const NTFS_ROOT_RECORD: u64 = 5;
/// record < 16 是 NTFS 保留系统文件（$MFT/$LogFile/$Bitmap 等）。
const NTFS_RESERVED_MAX: u64 = 16;

/// 属性类型码
const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
#[allow(dead_code)]
const ATTR_INDEX_ALLOCATION: u32 = 0xA0;
const ATTR_REPARSE_POINT: u32 = 0xC0;
const ATTR_END: u32 = 0xFFFF_FFFF;

// FILE_ATTRIBUTE_* / IO_REPARSE_TAG_* 常量统一从 fs_attrs 取（以前本地重定义）。
use crate::fs_attrs::{
    FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    IO_REPARSE_TAG_WOF,
};

pub struct MftError(pub String);
impl std::fmt::Debug for MftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{:?}", self.0) }
}
impl std::fmt::Display for MftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for MftError {}

/// 一个文件的聚合属性（来自 base record + 所有扩展记录）。
#[derive(Clone, Debug, Default)]
pub struct FileRecordBase {
    pub logical_size: u64,
    pub physical_size: u64,
    pub last_modified_ft: u64,
    pub created_ft: u64,
    pub accessed_ft: u64,
    pub attributes: u32,
    pub reparse_tag: u32,
}

/// 一条 $FILE_NAME 属性解析结果（一个文件可能有多个 $FILE_NAME：长名+短名+硬链接）。
#[derive(Clone, Debug)]
pub struct FileRecordName {
    pub name: String,
    pub base_record: u64,
}

/// NTFS 上下文：两个哈希表 + 卷信息。
pub struct NtfsContext {
    pub base_file_records: HashMap<u64, FileRecordBase>,
    pub parent_to_children: HashMap<u64, Vec<FileRecordName>>,
    pub bytes_per_cluster: u32,
    pub bytes_per_record: u32,
    /// USA fixup 失败（校验值对不上，记录被跳过）的次数。只用于日志：
    /// 个别失败意味着个别文件缺失，大量失败意味着读取链路有问题，
    /// 日志里要有证据可查，不能静默吞掉。
    pub fixup_failures: u64,
    /// USA 信息头不完整（offset/count 越界，无法做 fixup）而跳过的记录数。
    pub fixup_unreadable: u64,
}

pub fn is_elevated() -> bool {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// 文件夹/文件颜色统一走 theme（色板唯一定义点）。
fn folder_color(depth: usize) -> Color32 {
    crate::theme::folder_color(depth)
}

fn file_color() -> Color32 {
    crate::theme::FILE_COLOR
}

/// 主入口：扫描一个 NTFS 卷，返回建好的目录树。
pub fn scan_volume(
    drive_letter: char,
    tx: &Sender<crate::scan::ScanMessage>,
) -> Result<Node, MftError> {
    crate::dlog!("[mft_scan] 开始扫描 drive={}", drive_letter);
    let ctx = load_volume(drive_letter, tx)?;
    crate::dlog!(
        "[mft_scan] MFT 加载完成: {} 个文件记录, {} 个父目录",
        ctx.base_file_records.len(),
        ctx.parent_to_children.len()
    );

    let root_name = format!("{}:\\", drive_letter);
    let mut size_counted: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut root_node = build_tree(&ctx, NTFS_ROOT_RECORD, &root_name, 0, &mut size_counted);
    crate::dlog!(
        "[mft_scan] 树构建完成: logical={}, physical={}, files={}, folders={}",
        crate::format::human_size(root_node.logical_size),
        crate::format::human_size(root_node.physical_size),
        root_node.file_count,
        root_node.folder_count
    );

    // 填充根级子项的 Owner（和 WinDirStat 一样用 GetNamedSecurityInfo）
    let root_path = format!("{}:\\", drive_letter);
    populate_owners(&mut root_node, &root_path);
    crate::dlog!("[mft_scan] Owner 填充完成");

    Ok(root_node)
}

/// 递归填充 Owner（用 GetNamedSecurityInfo + LookupAccountSid）。
/// 只填充可见的（已展开的）节点的直接子项，避免全量遍历太慢。
fn populate_owners(node: &mut Node, path: &str) {
    // 迭代版本：栈里存 (相对 node 的 NodePath, 对应的完整文件系统路径)，
    // 每次用 Node::navigate_mut 重新定位到那个节点，不用原生递归也不用裸指针。
    // 目前唯一的调用点在扫描刚结束、所有节点 expanded 都还是 false 时，所以这个函数
    // 实际只会处理 node 的直接子项；写成迭代版是为了彻底消除"以后万一有地方在展开更深
    // 层级后又调用这个函数"时的递归深度风险，而不是依赖"现在用不到所以没关系"。
    let mut stack: Vec<(Vec<usize>, String)> = vec![(Vec::new(), path.to_string())];
    while let Some((rel_path, cur_path)) = stack.pop() {
        let Some(cur) = (if rel_path.is_empty() { Some(&mut *node) } else { node.navigate_mut(&rel_path) }) else { continue };
        for (i, child) in cur.children.iter_mut().enumerate() {
            let child_path = if cur_path.ends_with('\\') {
                format!("{}{}", cur_path, child.name)
            } else {
                format!("{}\\{}", cur_path, child.name)
            };
            child.owner = get_owner(&child_path);
            // 只处理已展开的文件夹
            if child.is_folder() && child.expanded {
                let mut child_rel = rel_path.clone();
                child_rel.push(i);
                stack.push((child_rel, child_path));
            }
        }
    }
}

/// 用 Win32 API 获取文件所有者名称（和 WinDirStat 的 GetOwner 一致）。
/// 必须是 `pub`（不能只是 `pub(crate)`）：调用方 app.rs 在二进制 crate（main.rs 是它的根）里，
/// 是把 diskforge 当依赖库用的，`pub(crate)` 只在 diskforge 这个库 crate 内部可见，跨 crate 用不了。
pub fn get_owner(path: &str) -> String {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::{
        LookupAccountSidW, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SID_NAME_USE,
    };
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let mut sid: PSID = std::ptr::null_mut();
    let ok = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut sid,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if ok != 0 {
        return String::new();
    }

    // 先用 260（绝大多数场景够用）试一次；如果 LookupAccountSidW 因为缓冲区不够失败，
    // 它会把实际需要的长度写回 name_len/domain_len，用这个真实长度重新分配再查一次，
    // 而不是像之前那样直接截断超长的用户名/域名（AD 环境里偶尔会遇到）。
    let mut name_len: u32 = 260;
    let mut domain_len: u32 = 260;
    let mut sid_type: SID_NAME_USE = 0;
    let mut name_buf = vec![0u16; name_len as usize];
    let mut domain_buf = vec![0u16; domain_len as usize];
    let mut ok = unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            sid,
            name_buf.as_mut_ptr(),
            &mut name_len,
            domain_buf.as_mut_ptr(),
            &mut domain_len,
            &mut sid_type,
        )
    };
    if ok == 0 && (name_len as usize > name_buf.len() || domain_len as usize > domain_buf.len()) {
        // 缓冲区不够，用系统告诉我们的真实所需长度重新分配一次
        name_buf = vec![0u16; name_len as usize];
        domain_buf = vec![0u16; domain_len as usize];
        ok = unsafe {
            LookupAccountSidW(
                std::ptr::null(),
                sid,
                name_buf.as_mut_ptr(),
                &mut name_len,
                domain_buf.as_mut_ptr(),
                &mut domain_len,
                &mut sid_type,
            )
        };
    }
    let result = if ok != 0 && name_len > 0 {
        let name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
        if domain_len > 0 {
            let domain = String::from_utf16_lossy(&domain_buf[..domain_len as usize]);
            format!("{}\\{}", domain, name)
        } else {
            name
        }
    } else {
        String::new()
    };

    unsafe {
        if !sd.is_null() {
            LocalFree(sd as *mut _);
        }
    }
    result
}

/// 第一阶段：读 MFT，填充两个哈希表。
fn load_volume(
    drive_letter: char,
    tx: &Sender<crate::scan::ScanMessage>,
) -> Result<NtfsContext, MftError> {
    let vol_path = wide(&format!(r"\\.\{drive_letter}:"));

    // 打开卷设备
    let vol_handle = unsafe {
        let h = CreateFileW(
            vol_path.as_ptr(),
            FILE_READ_DATA | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_NO_BUFFERING,
            null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            return Err(MftError(format!(
                "无法打开卷设备 \\\\.\\{drive_letter}:（需要管理员权限）"
            )));
        }
        h
    };
    crate::dlog!("[mft_scan] 卷设备已打开: \\\\.\\{drive_letter}:");

    // 拿卷信息
    let mut vol_info: NTFS_VOLUME_DATA_BUFFER = unsafe { std::mem::zeroed() };
    let mut bytes_returned: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            vol_handle,
            FSCTL_GET_NTFS_VOLUME_DATA,
            null_mut(),
            0,
            &mut vol_info as *mut _ as *mut _,
            std::mem::size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
            &mut bytes_returned,
            null_mut(),
        )
    };
    if ok == 0 {
        unsafe { CloseHandle(vol_handle); }
        return Err(MftError(format!(
            "FSCTL_GET_NTFS_VOLUME_DATA 失败（{} 可能不是 NTFS）",
            drive_letter
        )));
    }
    let bytes_per_cluster = vol_info.BytesPerCluster;
    if bytes_per_cluster == 0 {
        // 正常 NTFS 卷不会出现这个值，但万一某些虚拟盘/过滤驱动返回了 0，
        // 后面 MftValidDataLength / bytes_per_cluster 会直接除零 panic，
        // 这里提前拦截，转成一个可以被上层 fallback 到常规扫描的错误，而不是让整个程序崩溃。
        unsafe { CloseHandle(vol_handle); }
        return Err(MftError(format!(
            "FSCTL_GET_NTFS_VOLUME_DATA 返回的 BytesPerCluster 为 0（{} 卷信息异常）",
            drive_letter
        )));
    }
    let bytes_per_record = vol_info.BytesPerFileRecordSegment.max(1024);
    // 扇区大小：USA fixup 的步长必须用它，绝不能写死 512——4Kn 原生盘
    // （高级格式化，非 512e 模拟）BytesPerSector = 4096，MFT 记录里每个
    // "扇区末尾 2 字节"的位置是每 4096 字节一处；写死 512 会校验到错误的
    // 偏移，所有记录都 fixup 失败被跳过 → 整卷文件静默丢失，还显示"扫描
    // 成功"。参考 NTFS 文档：USA 的每个条目对应"记录占用的每个扇区"。
    let bytes_per_sector = vol_info.BytesPerSector.max(512) as usize;
    crate::dlog!(
        "[mft_scan] 卷信息: BytesPerCluster={}, BytesPerSector={}, BytesPerFileRecordSegment={}, MftStartLcn={}, MftValidDataLength={}",
        bytes_per_cluster, bytes_per_sector, bytes_per_record, vol_info.MftStartLcn, vol_info.MftValidDataLength
    );

    let mut ctx = NtfsContext {
        base_file_records: HashMap::new(),
        parent_to_children: HashMap::new(),
        bytes_per_cluster,
        bytes_per_record,
        fixup_failures: 0,
        fixup_unreadable: 0,
    };

    // ── 拿 MFT 的物理簇映射（run 列表），用卷句柄按 run 顺序直读 ──
    //
    // 为什么不直接对 $MFT 文件句柄 ReadFile：Windows 内核对 $MFT 数据流的
    // FILE_READ_DATA 访问有硬性限制——管理员身份也一样被拒绝
    // （ERROR_ACCESS_DENIED，Stack Overflow 上有大量同类报告，v0.1.0-beta
    // 首个版本实测踩坑：\"无法打开 $MFT（错误码 5）\"）。被广泛验证的通行
    // 做法（WizTree/Everything 一类 NTFS 直读工具、各开源 MFT 解析器一致）：
    //   1. 以 FILE_READ_ATTRIBUTES 打开 `\\.\C:\$MFT::$DATA`——属性读访问
    //      不在封禁之列，管理员可用；
    //   2. 对该句柄调 FSCTL_GET_RETRIEVAL_POINTERS 拿 $MFT 数据流的完整
    //      物理 run 列表——这是内核给的权威映射，MFT 再碎片化也不怕；
    //   3. 用卷设备句柄 `\\.\C:` 按物理 LCN 顺序 ReadFile 读回 MFT 字节流。
    // 任何一步失败都显式返回 Err、上层自动回退常规目录遍历（慢但永远
    // 正确）——不保留老版本"拿不到 run 就假设 MFT 连续"的静默降级（碎片
    // 卷上会读到垃圾簇，少文件还显示成功，是审计确认过的 bug）。
    let mft_path = wide(&format!(r"\\.\{drive_letter}:\$MFT::$DATA"));
    let mft_handle = unsafe {
        CreateFileW(
            mft_path.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_NO_BUFFERING,
            null_mut(),
        )
    };
    if mft_handle == INVALID_HANDLE_VALUE {
        unsafe { CloseHandle(vol_handle); }
        let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(MftError(format!(
            "无法打开 $MFT::$DATA（错误码 {err}），回退常规遍历"
        )));
    }

    // FSCTL_GET_RETRIEVAL_POINTERS：缓冲不够时返回 ERROR_MORE_DATA，翻倍重试。
    let retrieval_buf = {
        let mut input = STARTING_VCN_INPUT_BUFFER { StartingVcn: 0 };
        let mut buf_size = std::mem::size_of::<RETRIEVAL_POINTERS_BUFFER>() + 32 * 16;
        // 64MB 上限：run 条目每个 16 字节，64MB 对应 400 万个 extent——远超
        // 任何真实 MFT 的碎片程度，只是防异常卷上无限增长耗尽内存。
        const MAX_RETRIEVAL_BUF: usize = 64 * 1024 * 1024;
        let mut buf = vec![0u8; buf_size];
        let ok = loop {
            let mut returned = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    mft_handle,
                    FSCTL_GET_RETRIEVAL_POINTERS,
                    &mut input as *mut _ as *mut _,
                    std::mem::size_of::<STARTING_VCN_INPUT_BUFFER>() as u32,
                    buf.as_mut_ptr() as *mut _,
                    buf_size as u32,
                    &mut returned,
                    null_mut(),
                )
            };
            if ok != 0 {
                break true;
            }
            if unsafe { windows_sys::Win32::Foundation::GetLastError() } != ERROR_MORE_DATA
                || buf_size >= MAX_RETRIEVAL_BUF
            {
                break false;
            }
            buf_size *= 2;
            buf.resize(buf_size, 0);
        };
        // $MFT 属性句柄只为了拿 run 列表，用完即关。
        unsafe { CloseHandle(mft_handle); }
        if ok { Some(buf) } else { None }
    };
    let Some(retrieval_buf) = retrieval_buf else {
        unsafe { CloseHandle(vol_handle); }
        return Err(MftError(
            "FSCTL_GET_RETRIEVAL_POINTERS 失败（拿不到 MFT 物理簇映射），回退常规遍历".to_string(),
        ));
    };
    let mft_runs = parse_retrieval_pointers(&retrieval_buf);
    crate::dlog!("[mft_scan] MFT 有 {} 个物理 run", mft_runs.len());

    let cluster_size = bytes_per_cluster as u64;

    let record_size = bytes_per_record as usize;
    // 要读的总字节数：MftValidDataLength 就是 $MFT 的有效数据长度（记录数 ×
    // 记录大小），比"按卷大小/簇大小估"精确。0 长度直接当异常处理（正常 NTFS
    // 不可能），宁可回退常规遍历也不返回一棵空树冒充"扫描成功"。
    let mft_total_bytes = vol_info.MftValidDataLength as u64;
    if mft_total_bytes == 0 || (mft_total_bytes as usize) < record_size {
        unsafe { CloseHandle(vol_handle); }
        return Err(MftError(format!(
            "$MFT 有效数据长度异常（{mft_total_bytes} 字节），回退常规遍历"
        )));
    }

    // run 列表必须覆盖住整张 MFT 的有效长度，否则按 run 读只会读到一部分
    // （解析异常时不允许带着不完整的映射继续跑：结果是"少一大半文件但
    // 显示成功"，比直接失败更误导）。
    let covered = mft_runs.iter().map(|&(_, _, count)| count).sum::<u64>() * cluster_size;
    if covered < mft_total_bytes {
        unsafe { CloseHandle(vol_handle); }
        return Err(MftError(format!(
            "MFT run 列表只覆盖 {covered}/{mft_total_bytes} 字节（解析异常），回退常规遍历"
        )));
    }

    let mut records_processed: u64 = 0;

    // 按 run 顺序读 MFT：run i 在卷上的物理偏移 = LCN × 簇大小，长度 =
    // 簇数 × 簇大小；MFT 逻辑字节流顺序 = run 的 VCN 顺序（记录号由逻辑
    // 偏移决定，与物理位置无关）。FILE_FLAG_NO_BUFFERING 要求每次读的物理
    // 偏移和长度都是扇区整数倍——物理偏移天然簇对齐（簇 ≥ 扇区）；run
    // 长度是簇整数倍；块长取 min(run 剩余, 4MB)，NTFS 簇是 2 的幂且 ≤ 2MB，
    // 4MB 必然被整除，对齐恒成立。
    const CHUNK_SIZE: usize = 4 * 1024 * 1024;
    let sector = bytes_per_sector;
    let mut chunk_buf: Vec<u8> = vec![0u8; CHUNK_SIZE];
    let mut logical_done: u64 = 0; // MFT 逻辑字节流已读量（决定记录号）

    'read_loop: for &(run_vcn_start, run_lcn, run_clusters) in &mft_runs {
        let run_bytes = run_clusters * cluster_size;
        // run_vcn_start 只用于日志定位；逻辑顺序由 run 列表本身的顺序保证。
        let _ = run_vcn_start;
        // LCN 为负 = sparse：MFT 数据流理论上不会稀疏，防御性跳过（逻辑
        // 偏移照常推进，记录号不能错位；读到的 0 填充会被 FILE 魔法校验拦下）。
        let physical = if run_lcn >= 0 {
            run_lcn as u64 * cluster_size
        } else {
            logical_done += run_bytes;
            continue;
        };
        let mut run_done: u64 = 0;
        while run_done < run_bytes && logical_done < mft_total_bytes {
            let bytes_this = ((run_bytes - run_done) as usize).min(CHUNK_SIZE);
            let mut new_pos: i64 = 0;
            let ok = unsafe {
                SetFilePointerEx(vol_handle, (physical + run_done) as i64, &mut new_pos, FILE_BEGIN)
            };
            if ok == 0 {
                crate::dlog!("[mft_scan] SetFilePointerEx 失败（卷偏移 {}）", physical + run_done);
                break 'read_loop;
            }
            let mut bytes_returned_read: u32 = 0;
            let ok = unsafe {
                ReadFile(vol_handle, chunk_buf.as_mut_ptr(), bytes_this as u32, &mut bytes_returned_read, null_mut())
            };
            if ok == 0 || bytes_returned_read == 0 {
                crate::dlog!("[mft_scan] ReadFile(卷) 结束: ok={ok}, bytes={bytes_returned_read}, 已读 {logical_done}/{mft_total_bytes}");
                break 'read_loop;
            }
            let bytes_read = bytes_returned_read as usize;
            // 逐条记录解析（记录号 = 逻辑偏移 / 记录大小，$MFT 内容就是记录数组）。
            let mut off = 0usize;
            let records_before_chunk = records_processed;
            while off + record_size <= bytes_read {
                let rec = &mut chunk_buf[off..off + record_size];
                let current_record = (logical_done + off as u64) / record_size as u64;
                process_record(rec, current_record, &mut ctx, sector);
                records_processed += 1;
                off += record_size;
            }
            run_done += bytes_read as u64;
            logical_done += bytes_read as u64;

            // 进度上报：本块处理跨越了 50K 关口就报一次（和 scan.rs 的"处理前 /
            // 处理后落在哪个关口区间"是同一种跨区间判断）。注意不能写成
            // `records_processed / STEP != (records_processed - 1) / STEP`——那是
            // "逐条 +1"用的写法，这里每次一批就是几千条，计数值几乎永远不会
            // 恰好踩在 50K 整数倍上（4MB 块 = 4096 条，与 50000 的最小公倍数
            // 是 1280 万），整个扫描一次都触发不了，界面计数会一直是 0。
            const PROGRESS_STEP: u64 = 50_000;
            if records_before_chunk / PROGRESS_STEP != records_processed / PROGRESS_STEP {
                let _ = tx.send(crate::scan::ScanMessage::Progress(records_processed));
            }
        }
    }
    unsafe { CloseHandle(vol_handle); }

    if logical_done < mft_total_bytes {
        // 读到一半失败：继续往下走会把"半张 MFT"当完整结果——树能建出来
        // 但文件系统性缺失，比失败更误导。转成 Err 回退常规遍历。
        crate::dlog!(
            "[mft_scan] $MFT 只读到 {}/{} 字节（中途失败），放弃本次 MFT 直读",
            logical_done, mft_total_bytes
        );
        return Err(MftError(format!(
            "$MFT 读取中断（读到 {logical_done}/{mft_total_bytes} 字节），回退常规遍历"
        )));
    }
    if ctx.fixup_failures > 0 || ctx.fixup_unreadable > 0 {
        crate::dlog!(
            "[mft_scan] USA fixup 异常统计: 校验失败 {} 条（已跳过）, 信息头不完整 {} 条（已跳过）——少量属正常（坏扇区/写入中断），大量请反馈日志",
            ctx.fixup_failures, ctx.fixup_unreadable
        );
    }

    crate::dlog!("[mft_scan] 共处理 {} 条 MFT 记录", records_processed);
    Ok(ctx)
}

/// 解析 FSCTL_GET_RETRIEVAL_POINTERS 的输出为 `(起始 VCN, LCN, 簇数)` 列表。
///
/// 输出结构（MSDN `RETRIEVAL_POINTERS_BUFFER`）：`StartingVcn` 是返回的请求
/// 起始 VCN；每个 Extents 条目的簇数 = 本条目 NextVcn 减去"前一条目的
/// NextVcn（对第一条目则是结构体的 StartingVcn）"——即第一个条目本身就是
/// 第一个真实 extent，不是伪条目。LCN < 0 表示稀疏段（MFT 数据流理论上
/// 不会出现，保留标记交由上层防御性处理）。
///
/// 与老版本不同：解析结果为空时**返回空列表**，由调用方按"覆盖不足整张
/// MFT"处理并回退常规遍历——不再压入 `(0, lcn, 0)` 这种零长度假 run
/// （老版本压进去后一个字节都不读，空树当成功返回，是审计确认的 bug）。
fn parse_retrieval_pointers(buf: &[u8]) -> Vec<(u64, i64, u64)> {
    if buf.len() < std::mem::size_of::<RETRIEVAL_POINTERS_BUFFER>() {
        return Vec::new();
    }
    let rp = unsafe { &*(buf.as_ptr() as *const RETRIEVAL_POINTERS_BUFFER) };
    let extent_count = rp.ExtentCount as usize;
    if extent_count == 0 {
        return Vec::new();
    }
    let mut runs = Vec::with_capacity(extent_count);
    let extents_ptr: *const windows_sys::Win32::System::Ioctl::RETRIEVAL_POINTERS_BUFFER_0 =
        &rp.Extents[0];
    let mut vcn_start = rp.StartingVcn;
    for i in 0..extent_count {
        // ExtentCount 由内核返回，但防御性校验越界（异常卷/驱动 bug 时不 panic）。
        if (i + 1) * std::mem::size_of::<windows_sys::Win32::System::Ioctl::RETRIEVAL_POINTERS_BUFFER_0>()
            > buf.len() - std::mem::offset_of!(RETRIEVAL_POINTERS_BUFFER, Extents)
        {
            break;
        }
        let ext = unsafe { &*extents_ptr.add(i) };
        let vcn_next = ext.NextVcn;
        let lcn = ext.Lcn;
        let count = (vcn_next - vcn_start) as u64;
        if count > 0 {
            runs.push((vcn_start as u64, lcn, count));
        }
        vcn_start = vcn_next;
    }
    runs
}

/// 处理单条 MFT 记录：做 USA fixup，解析属性，聚合到 base record。
///
/// `bytes_per_sector`：fixup 的步长，必须用卷的真实扇区大小（4Kn 原生盘是
/// 4096），绝不能写死 512——见 `load_volume` 里 `bytes_per_sector` 上的说明。
fn process_record(rec: &mut [u8], current_record: u64, ctx: &mut NtfsContext, bytes_per_sector: usize) {
    if rec.len() < 48 || &rec[0..4] != b"FILE" {
        return;
    }
    let usa_offset = u16::from_le_bytes([rec[4], rec[5]]) as usize;
    let usa_count = u16::from_le_bytes([rec[6], rec[7]]) as usize;
    let flags = u16::from_le_bytes([rec[22], rec[23]]);
    let in_use = flags & 0x0001 != 0;
    let is_dir = flags & 0x0002 != 0;
    let first_attr_offset = u16::from_le_bytes([rec[20], rec[21]]) as usize;
    let base_file_record = u64::from_le_bytes(rec[32..40].try_into().unwrap());
    let base_record_index = if base_file_record > 0 {
        base_file_record & 0x0000_FFFF_FFFF_FFFF
    } else {
        current_record
    };

    // USA fixup（多扇区记录保护）：每个扇区末尾 2 字节在写入时被替换成
    // USN，读出来后要先校验、再还原成真实数据，然后才能解析属性。
    //   - 步长 = 卷的真实扇区大小（不写死 512，4Kn 兼容）；
    //   - 信息头不完整（offset/count 越界）时**必须跳过整条记录**：未修复的
    //     记录里夹着 USN 字节，直接解析会把垃圾当数据——以前这里只是跳过
    //     fixup 继续解析，属于审计发现的"静默解析未修复数据"问题；
    //   - 校验值对不上同样跳过整条记录并计数（磁盘坏块/写入中断的正常
    //     表现，大量出现则说明读取链路有问题，收尾时会打日志）。
    if usa_count == 0 {
        ctx.fixup_unreadable += 1;
        return;
    }
    if usa_offset < 8 || usa_offset + usa_count * 2 > rec.len() {
        ctx.fixup_unreadable += 1;
        return;
    }
    let usn = [rec[usa_offset], rec[usa_offset + 1]];
    for i in 1..usa_count {
        let sector_end = i * bytes_per_sector; // 每扇区末尾 2 字节
        if sector_end > rec.len() {
            // usa_count 声称的扇区数比记录能容纳的多：信息头不可信，不处理。
            ctx.fixup_unreadable += 1;
            return;
        }
        let check = &rec[sector_end - 2..sector_end];
        if check != usn {
            ctx.fixup_failures += 1;
            return; // fixup 失败，跳过这条记录，绝不解析未修复的数据
        }
        let orig_off = usa_offset + i * 2;
        rec[sector_end - 2] = rec[orig_off];
        rec[sector_end - 1] = rec[orig_off + 1];
    }

    if !in_use {
        return;
    }

    // 获取或创建 base record 条目
    let base_entry = ctx
        .base_file_records
        .entry(base_record_index)
        .or_default();

    // 遍历属性
    let mut off = first_attr_offset;
    while off + 16 <= rec.len() {
        let attr_type = u32::from_le_bytes(rec[off..off + 4].try_into().unwrap());
        if attr_type == ATTR_END {
            break;
        }
        let attr_len = u32::from_le_bytes(rec[off + 4..off + 8].try_into().unwrap()) as usize;
        if attr_len == 0 || off + attr_len > rec.len() {
            break;
        }
        let non_resident = rec[off + 8] != 0;
        let name_len = rec[off + 9];
        // let name_offset = u16::from_le_bytes([rec[off + 10], rec[off + 11]]) as usize;
        let attr_flags = u16::from_le_bytes([rec[off + 12], rec[off + 13]]);

        if attr_type == ATTR_STANDARD_INFORMATION && !non_resident && off + 22 <= rec.len() {
            // $STANDARD_INFORMATION（resident）
            let value_off = u16::from_le_bytes([rec[off + 20], rec[off + 21]]) as usize;
            let value_len = u32::from_le_bytes([rec[off + 16], rec[off + 17], rec[off + 18], rec[off + 19]]) as usize;
            let content = off + value_off;
            // 布局：CreationTime(8) + LastModificationTime(8) + MftChangeTime(8) + AccessTime(8) + Flags(4)
            if content + 0x24 <= rec.len() && value_len >= 0x24 {
                base_entry.created_ft = u64::from_le_bytes(
                    rec[content..content + 0x08].try_into().unwrap(),
                );
                base_entry.last_modified_ft = u64::from_le_bytes(
                    rec[content + 0x08..content + 0x10].try_into().unwrap(),
                );
                base_entry.accessed_ft = u64::from_le_bytes(
                    rec[content + 0x18..content + 0x20].try_into().unwrap(),
                );
                base_entry.attributes = u32::from_le_bytes(
                    rec[content + 0x20..content + 0x24].try_into().unwrap(),
                );
                if is_dir {
                    base_entry.attributes |= FILE_ATTRIBUTE_DIRECTORY;
                }
                if base_entry.attributes == 0 {
                    base_entry.attributes = FILE_ATTRIBUTE_NORMAL;
                }
            }
        } else if attr_type == ATTR_FILE_NAME && !non_resident && off + 22 <= rec.len() {
            // $FILE_NAME（resident）
            let value_off = u16::from_le_bytes([rec[off + 20], rec[off + 21]]) as usize;
            let value_len = u32::from_le_bytes([rec[off + 16], rec[off + 17], rec[off + 18], rec[off + 19]]) as usize;
            let content = off + value_off;
            if content + 0x42 <= rec.len() && value_len >= 0x42 {
                let parent_ref = u64::from_le_bytes(rec[content..content + 8].try_into().unwrap());
                let parent_dir = parent_ref & 0x0000_FFFF_FFFF_FFFF;
                let ns = rec[content + 0x41]; // namespace
                let name_len_chars = rec[content + 0x40] as usize;
                // 跳过短名（ns==2 = DOS 8.3）
                if ns == 0x02 {
                    off += attr_len;
                    continue;
                }
                let name_bytes_len = name_len_chars * 2;
                if content + 0x42 + name_bytes_len <= rec.len() && name_len_chars > 0 {
                    let name_u16: Vec<u16> = rec[content + 0x42..content + 0x42 + name_bytes_len]
                        .as_chunks::<2>().0.iter()
                        .map(|b| u16::from_le_bytes([b[0], b[1]]))
                        .collect();
                    let name = String::from_utf16_lossy(&name_u16);
                    // 跳过 . 和 ..
                    if name == "." || name == ".." {
                        off += attr_len;
                        continue;
                    }
                    ctx.parent_to_children
                        .entry(parent_dir)
                        .or_default()
                        .push(FileRecordName {
                            name,
                            base_record: base_record_index,
                        });
                }
            }
        } else if attr_type == ATTR_DATA {
            // $DATA
            if name_len > 0 {
                // 命名 $DATA（ADS）：检查 WofCompressedData
                let name_off = u16::from_le_bytes([rec[off + 10], rec[off + 11]]) as usize;
                let name_start = off + name_off;
                if name_start + (name_len as usize) * 2 <= rec.len() {
                    let stream_u16: Vec<u16> = rec[name_start..name_start + (name_len as usize) * 2]
                        .as_chunks::<2>().0.iter()
                        .map(|b| u16::from_le_bytes([b[0], b[1]]))
                        .collect();
                    let stream_name = String::from_utf16_lossy(&stream_u16);
                    if stream_name == "WofCompressedData" {
                        if !non_resident {
                            // resident WofCompressedData：value_len 在 off+16..off+20，
                            // 循环头只保证 off+16<=len，这里要单独再查一次
                            if off + 20 <= rec.len() {
                                let value_len = u32::from_le_bytes([rec[off + 16], rec[off + 17], rec[off + 18], rec[off + 19]]) as u64;
                                base_entry.physical_size = (value_len + 7) & !7;
                            }
                        } else if off + 24 <= rec.len() {
                            // non-resident WofCompressedData：检查 LowestVcn==0
                            let lowest_vcn = u64::from_le_bytes(rec[off + 16..off + 24].try_into().unwrap());
                            if lowest_vcn == 0 && off + 0x30 <= rec.len() {
                                let alloc_len = u64::from_le_bytes(rec[off + 0x28..off + 0x30].try_into().unwrap());
                                base_entry.physical_size = alloc_len;
                            }
                        }
                    }
                }
                off += attr_len;
                continue;
            }
            // 未命名 $DATA
            if !non_resident {
                // resident：ValueLength = logical size，physical = (len+7)&~7
                if off + 20 <= rec.len() {
                    let value_len = u32::from_le_bytes([rec[off + 16], rec[off + 17], rec[off + 18], rec[off + 19]]) as u64;
                    base_entry.logical_size = value_len;
                    base_entry.physical_size = (value_len + 7) & !7;
                }
            } else if off + 24 <= rec.len() {
                // non-resident：只在 LowestVcn==0 时有效
                let lowest_vcn = u64::from_le_bytes(rec[off + 16..off + 24].try_into().unwrap());
                if lowest_vcn == 0 && off + 0x38 <= rec.len() {
                    let file_size = u64::from_le_bytes(rec[off + 0x30..off + 0x38].try_into().unwrap());
                    base_entry.logical_size = file_size;
                    // physical size：压缩/稀疏用 Compressed(0x40)，否则 AllocatedLength(0x28)
                    let is_compressed = attr_flags & 0x0001 != 0;
                    let is_sparse = attr_flags & 0x8000 != 0;
                    let phys = if is_compressed || is_sparse {
                        if off + 0x48 <= rec.len() {
                            u64::from_le_bytes(rec[off + 0x40..off + 0x48].try_into().unwrap())
                        } else {
                            0
                        }
                    } else if off + 0x30 <= rec.len() {
                        u64::from_le_bytes(rec[off + 0x28..off + 0x30].try_into().unwrap())
                    } else {
                        0
                    };
                    if phys > 0 {
                        base_entry.physical_size = phys;
                    }
                }
            }
        } else if attr_type == ATTR_REPARSE_POINT && !non_resident {
            // $REPARSE_POINT（resident）
            if off + 22 <= rec.len() {
                let value_off = u16::from_le_bytes([rec[off + 20], rec[off + 21]]) as usize;
                let content = off + value_off;
                if content + 4 <= rec.len() {
                    base_entry.reparse_tag = u32::from_le_bytes(rec[content..content + 4].try_into().unwrap());
                    if base_entry.reparse_tag == IO_REPARSE_TAG_WOF {
                        base_entry.attributes |= FILE_ATTRIBUTE_COMPRESSED;
                    }
                }
            }
        }
        off += attr_len;
    }
}

/// 第二阶段：从指定 record 建树。
///
/// 用显式栈做迭代式后序遍历，不再依赖原生递归调用栈：
/// 目录里嵌套多深，都只占用堆上的 Vec<Frame>，不会有任何栈溢出的可能性
/// （之前用"给扫描线程分配 64MB 大栈"来兜底，但那终究只是把风险发生的概率
/// 压得很低，不是消除风险；这里换成完全迭代，风险从"极低概率"变成"不存在"）。
///
/// 算法：每个目录对应一个 Frame，Frame 记录"这个目录还有哪些子项没处理"和
/// "已经处理完、可以挂到这个目录节点下的子 Node 列表"。主循环每次只看栈顶 Frame：
/// - 如果它还有没处理的子项：文件就直接原地构造成 Node 塞进 built_children；
///   子目录就再压一个新 Frame 到栈顶，让下一轮循环先去处理这个子目录（对应递归下钻）。
/// - 如果它的子项已经全部处理完：出栈，用它攒好的 built_children 组装出这个目录的
///   Node，再塞进新栈顶（也就是它的父目录）的 built_children 里（对应递归返回）。
///
/// 因为"先把子项都处理完才把自己塞进父项"，子节点在父节点之前构造完成，
/// 和原来的递归版本（先递归子节点、拿到结果再组装父节点）语义完全一致，
/// 兄弟节点之间的先后顺序也和原版一样（不会因为改成迭代就打乱显示顺序）。
fn build_tree(
    ctx: &NtfsContext,
    record: u64,
    display_name: &str,
    depth: usize,
    size_counted: &mut std::collections::HashSet<u64>,
) -> Node {
    struct Frame<'a> {
        display_name: String,
        depth: usize,
        modified_ft: u64,
        created_ft: u64,
        accessed_ft: u64,
        attributes: u32,
        reparse_tag: u32,
        is_reserved: bool,
        child_names: Option<&'a [FileRecordName]>,
        next_child_idx: usize,
        built_children: Vec<Node>,
    }

    // 给定 record，取出它自身的元数据（和原递归版开头那段 match 逻辑一样）。
    let record_meta = |ctx: &NtfsContext, record: u64| -> (bool, u64, u64, u64, u64, u64, u32, u32) {
        let base = ctx.base_file_records.get(&record);
        let (logical, physical, modified_ft, created_ft, accessed_ft, attributes, reparse_tag) = match base {
            Some(b) => (b.logical_size, b.physical_size, b.last_modified_ft, b.created_ft, b.accessed_ft, b.attributes, b.reparse_tag),
            None => (0, 0, 0, 0, 0, FILE_ATTRIBUTE_DIRECTORY, 0),
        };
        let is_dir = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        (is_dir, logical, physical, modified_ft, created_ft, accessed_ft, attributes, reparse_tag)
    };

    let (root_is_dir, root_logical, root_physical, root_mod, root_created, root_accessed, root_attrs, root_reparse) =
        record_meta(ctx, record);
    let root_is_reserved = record < NTFS_RESERVED_MAX;

    if !root_is_dir {
        // 极端情况：传进来的顶层 record 本身不是目录（正常 NTFS 根目录一定是目录，
        // 这里只是为了和原递归版行为完全一致而保留这个分支）。
        let physical_to_use = if size_counted.insert(record) { root_physical } else { 0 };
        return Node::new_file_with_meta(
            display_name.to_string(), root_logical, physical_to_use, file_color(),
            root_mod, root_created, root_accessed, root_attrs, root_reparse, root_is_reserved, String::new(),
        );
    }

    let mut stack: Vec<Frame> = vec![Frame {
        display_name: display_name.to_string(),
        depth,
        modified_ft: root_mod, created_ft: root_created, accessed_ft: root_accessed,
        attributes: root_attrs, reparse_tag: root_reparse, is_reserved: root_is_reserved,
        child_names: ctx.parent_to_children.get(&record).map(|v| v.as_slice()),
        next_child_idx: 0,
        built_children: Vec::new(),
    }];

    loop {
        let top = stack.last_mut().expect("build_tree: 栈不应为空");
        let children = top.child_names.unwrap_or(&[]);
        if top.next_child_idx < children.len() {
            let cn = &children[top.next_child_idx];
            top.next_child_idx += 1;
            let child_depth = top.depth + 1; // 先取出来，避免下面 stack.push(...) 里还借用着 top
            let (c_is_dir, c_logical, c_physical, c_mod, c_created, c_accessed, c_attrs, c_reparse) =
                record_meta(ctx, cn.base_record);
            let c_is_reserved = cn.base_record < NTFS_RESERVED_MAX;
            if c_is_dir {
                // 子目录：压一个新 Frame，下一轮循环先处理它（等价于原来的递归下钻）。
                stack.push(Frame {
                    display_name: cn.name.clone(),
                    depth: child_depth,
                    modified_ft: c_mod, created_ft: c_created, accessed_ft: c_accessed,
                    attributes: c_attrs, reparse_tag: c_reparse, is_reserved: c_is_reserved,
                    child_names: ctx.parent_to_children.get(&cn.base_record).map(|v| v.as_slice()),
                    next_child_idx: 0,
                    built_children: Vec::new(),
                });
            } else {
                // 硬链接去重：只有 physical 去重，logical 不去重
                //（和 WinDirStat 一致：GetSizePhysical() 对硬链接返回 0，GetSizeLogical() 总是返回完整值）
                let cp = if size_counted.insert(cn.base_record) { c_physical } else { 0u64 };
                top.built_children.push(Node::new_file_with_meta(
                    cn.name.clone(), c_logical, cp, file_color(),
                    c_mod, c_created, c_accessed, c_attrs, c_reparse, c_is_reserved, String::new(),
                ));
            }
        } else {
            // 这个目录的子项全处理完了：出栈，组装成 Node。
            let finished = stack.pop().expect("build_tree: 栈不应为空");
            let node = Node::new_folder_with_meta(
                finished.display_name,
                folder_color(finished.depth),
                finished.built_children,
                finished.modified_ft, finished.created_ft, finished.accessed_ft,
                finished.attributes, finished.reparse_tag, finished.is_reserved,
                String::new(),
            );
            match stack.last_mut() {
                Some(parent) => parent.built_children.push(node), // 挂到父目录下，继续处理父目录剩余子项
                None => return node, // 栈空了，这就是最外层（root）的结果
            }
        }
    }
}

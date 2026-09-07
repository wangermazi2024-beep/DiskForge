
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

const NTFS_ROOT_RECORD: u64 = 5;
const NTFS_RESERVED_MAX: u64 = 16;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
#[allow(dead_code)]
const ATTR_INDEX_ALLOCATION: u32 = 0xA0;
const ATTR_REPARSE_POINT: u32 = 0xC0;
const ATTR_END: u32 = 0xFFFF_FFFF;

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

#[derive(Clone, Debug)]
pub struct FileRecordName {
    pub name: String,
    pub base_record: u64,
}

pub struct NtfsContext {
    pub base_file_records: HashMap<u64, FileRecordBase>,
    pub parent_to_children: HashMap<u64, Vec<FileRecordName>>,
    pub bytes_per_cluster: u32,
    pub bytes_per_record: u32,
    pub fixup_failures: u64,
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

fn folder_color(depth: usize) -> Color32 {
    crate::theme::folder_color(depth)
}

fn file_color() -> Color32 {
    crate::theme::FILE_COLOR
}

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

    let root_path = format!("{}:\\", drive_letter);
    populate_owners(&mut root_node, &root_path);
    crate::dlog!("[mft_scan] Owner 填充完成");

    Ok(root_node)
}

fn populate_owners(node: &mut Node, path: &str) {
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
            if child.is_folder() && child.expanded {
                let mut child_rel = rel_path.clone();
                child_rel.push(i);
                stack.push((child_rel, child_path));
            }
        }
    }
}

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

fn load_volume(
    drive_letter: char,
    tx: &Sender<crate::scan::ScanMessage>,
) -> Result<NtfsContext, MftError> {
    let vol_path = wide(&format!(r"\\.\{drive_letter}:"));

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
        unsafe { CloseHandle(vol_handle); }
        return Err(MftError(format!(
            "FSCTL_GET_NTFS_VOLUME_DATA 返回的 BytesPerCluster 为 0（{} 卷信息异常）",
            drive_letter
        )));
    }
    let bytes_per_record = vol_info.BytesPerFileRecordSegment.max(1024);
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

    let retrieval_buf = {
        let mut input = STARTING_VCN_INPUT_BUFFER { StartingVcn: 0 };
        let mut buf_size = std::mem::size_of::<RETRIEVAL_POINTERS_BUFFER>() + 32 * 16;
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
    let mft_total_bytes = vol_info.MftValidDataLength as u64;
    if mft_total_bytes == 0 || (mft_total_bytes as usize) < record_size {
        unsafe { CloseHandle(vol_handle); }
        return Err(MftError(format!(
            "$MFT 有效数据长度异常（{mft_total_bytes} 字节），回退常规遍历"
        )));
    }

    let covered = mft_runs.iter().map(|&(_, _, count)| count).sum::<u64>() * cluster_size;
    if covered < mft_total_bytes {
        unsafe { CloseHandle(vol_handle); }
        return Err(MftError(format!(
            "MFT run 列表只覆盖 {covered}/{mft_total_bytes} 字节（解析异常），回退常规遍历"
        )));
    }

    let mut records_processed: u64 = 0;

    const CHUNK_SIZE: usize = 4 * 1024 * 1024;
    let sector = bytes_per_sector;
    let mut chunk_buf: Vec<u8> = vec![0u8; CHUNK_SIZE];
    let mut logical_done: u64 = 0;

    'read_loop: for &(run_vcn_start, run_lcn, run_clusters) in &mft_runs {
        let run_bytes = run_clusters * cluster_size;
        let _ = run_vcn_start;
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

            const PROGRESS_STEP: u64 = 50_000;
            if records_before_chunk / PROGRESS_STEP != records_processed / PROGRESS_STEP {
                let _ = tx.send(crate::scan::ScanMessage::Progress(records_processed));
            }
        }
    }
    unsafe { CloseHandle(vol_handle); }

    if logical_done < mft_total_bytes {
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
        let sector_end = i * bytes_per_sector;
        if sector_end > rec.len() {
            ctx.fixup_unreadable += 1;
            return;
        }
        let check = &rec[sector_end - 2..sector_end];
        if check != usn {
            ctx.fixup_failures += 1;
            return;
        }
        let orig_off = usa_offset + i * 2;
        rec[sector_end - 2] = rec[orig_off];
        rec[sector_end - 1] = rec[orig_off + 1];
    }

    if !in_use {
        return;
    }

    let base_entry = ctx
        .base_file_records
        .entry(base_record_index)
        .or_default();

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
        let attr_flags = u16::from_le_bytes([rec[off + 12], rec[off + 13]]);

        if attr_type == ATTR_STANDARD_INFORMATION && !non_resident && off + 22 <= rec.len() {
            let value_off = u16::from_le_bytes([rec[off + 20], rec[off + 21]]) as usize;
            let value_len = u32::from_le_bytes([rec[off + 16], rec[off + 17], rec[off + 18], rec[off + 19]]) as usize;
            let content = off + value_off;
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
            let value_off = u16::from_le_bytes([rec[off + 20], rec[off + 21]]) as usize;
            let value_len = u32::from_le_bytes([rec[off + 16], rec[off + 17], rec[off + 18], rec[off + 19]]) as usize;
            let content = off + value_off;
            if content + 0x42 <= rec.len() && value_len >= 0x42 {
                let parent_ref = u64::from_le_bytes(rec[content..content + 8].try_into().unwrap());
                let parent_dir = parent_ref & 0x0000_FFFF_FFFF_FFFF;
                let ns = rec[content + 0x41];
                let name_len_chars = rec[content + 0x40] as usize;
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
            if name_len > 0 {
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
                            if off + 20 <= rec.len() {
                                let value_len = u32::from_le_bytes([rec[off + 16], rec[off + 17], rec[off + 18], rec[off + 19]]) as u64;
                                base_entry.physical_size = (value_len + 7) & !7;
                            }
                        } else if off + 24 <= rec.len() {
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
            if !non_resident {
                if off + 20 <= rec.len() {
                    let value_len = u32::from_le_bytes([rec[off + 16], rec[off + 17], rec[off + 18], rec[off + 19]]) as u64;
                    base_entry.logical_size = value_len;
                    base_entry.physical_size = (value_len + 7) & !7;
                }
            } else if off + 24 <= rec.len() {
                let lowest_vcn = u64::from_le_bytes(rec[off + 16..off + 24].try_into().unwrap());
                if lowest_vcn == 0 && off + 0x38 <= rec.len() {
                    let file_size = u64::from_le_bytes(rec[off + 0x30..off + 0x38].try_into().unwrap());
                    base_entry.logical_size = file_size;
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
        } else if attr_type == ATTR_REPARSE_POINT && !non_resident
            && off + 22 <= rec.len() {
            let value_off = u16::from_le_bytes([rec[off + 20], rec[off + 21]]) as usize;
            let content = off + value_off;
            if content + 4 <= rec.len() {
                base_entry.reparse_tag = u32::from_le_bytes(rec[content..content + 4].try_into().unwrap());
                if base_entry.reparse_tag == IO_REPARSE_TAG_WOF {
                    base_entry.attributes |= FILE_ATTRIBUTE_COMPRESSED;
                }
            }
        }
        off += attr_len;
    }
}

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
            let child_depth = top.depth + 1;
            let (c_is_dir, c_logical, c_physical, c_mod, c_created, c_accessed, c_attrs, c_reparse) =
                record_meta(ctx, cn.base_record);
            let c_is_reserved = cn.base_record < NTFS_RESERVED_MAX;
            if c_is_dir {
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
                let cp = if size_counted.insert(cn.base_record) { c_physical } else { 0u64 };
                top.built_children.push(Node::new_file_with_meta(
                    cn.name.clone(), c_logical, cp, file_color(),
                    c_mod, c_created, c_accessed, c_attrs, c_reparse, c_is_reserved, String::new(),
                ));
            }
        } else {
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
                Some(parent) => parent.built_children.push(node),
                None => return node,
            }
        }
    }
}

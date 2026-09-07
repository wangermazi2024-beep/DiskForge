
use std::path::Path;

#[cfg(windows)]
use windows_sys::Win32::Foundation::ERROR_NO_MORE_FILES;

#[derive(Clone)]
pub struct RawDirEntry {
    pub name: String,
    pub is_dir: bool,
    pub logical: u64,
    pub physical: u64,
    pub attrs: u32,
    pub modified_ft: u64,
    pub created_ft: u64,
    pub accessed_ft: u64,
    pub file_id: u64,
}

#[cfg(windows)]
pub fn enum_dir_batch(path: &Path) -> std::io::Result<Vec<RawDirEntry>> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandleEx, FileIdBothDirectoryRestartInfo,
        FileIdBothDirectoryInfo, FILE_ID_BOTH_DIR_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(path.as_os_str())
        .chain(std::iter::once(0))
        .collect();

    let handle: HANDLE = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }

    const BUF_SIZE: usize = 64 * 1024;
    let mut buf: Vec<u8> = vec![0u8; BUF_SIZE];
    let mut out = Vec::new();
    let mut first_call = true;
    let mut prev_written_max: usize = 0;

    loop {
        let class = if first_call { FileIdBothDirectoryRestartInfo } else { FileIdBothDirectoryInfo };
        if !first_call && prev_written_max > 0 {
            buf[..prev_written_max].fill(0);
        }
        first_call = false;

        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle,
                class,
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
            )
        };
        if ok == 0 {
            let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            unsafe { CloseHandle(handle) };
            if err == ERROR_NO_MORE_FILES {
                return Ok(out);
            }
            if out.is_empty() {
                return Err(std::io::Error::from_raw_os_error(err as i32));
            }
            crate::dlog!("[dir_enum] 批量枚举中途失败 (err={err})，已读出 {} 条丢弃，回退 read_dir", out.len());
            return Err(std::io::Error::from_raw_os_error(err as i32));
        }

        let mut offset: usize = 0;
        let mut parse_end: usize = 0;
        loop {
            let header_size = std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>();
            if offset + header_size > BUF_SIZE {
                break;
            }
            let entry_ptr = unsafe { buf.as_ptr().add(offset) as *const FILE_ID_BOTH_DIR_INFO };
            let entry: &FILE_ID_BOTH_DIR_INFO = unsafe { &*entry_ptr };

            let name_len_bytes = entry.FileNameLength as usize;
            if offset + header_size + name_len_bytes > BUF_SIZE {
                break;
            }
            let name_ptr = entry.FileName.as_ptr();
            let name_u16: &[u16] = unsafe {
                std::slice::from_raw_parts(name_ptr, name_len_bytes / 2)
            };
            let name = String::from_utf16_lossy(name_u16);
            parse_end = (offset + header_size + name_len_bytes + 16).min(BUF_SIZE);

            if name != "." && name != ".." {
                use crate::fs_attrs::FILE_ATTRIBUTE_DIRECTORY;
                let is_dir = entry.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                let file_id = entry.FileId as u64;
                out.push(RawDirEntry {
                    name,
                    is_dir,
                    logical: entry.EndOfFile as u64,
                    physical: entry.AllocationSize as u64,
                    attrs: entry.FileAttributes,
                    modified_ft: filetime_to_u64(entry.LastWriteTime),
                    created_ft: filetime_to_u64(entry.CreationTime),
                    accessed_ft: filetime_to_u64(entry.LastAccessTime),
                    file_id,
                });
            }

            if entry.NextEntryOffset == 0 {
                break;
            }
            offset += entry.NextEntryOffset as usize;
            if offset >= BUF_SIZE {
                break;
            }
        }
        if parse_end == 0 {
            prev_written_max = BUF_SIZE;
        } else {
            prev_written_max = prev_written_max.max(parse_end);
        }
    }
}

#[cfg(windows)]
fn filetime_to_u64(ft: i64) -> u64 {
    if ft < 0 { 0 } else { ft as u64 }
}

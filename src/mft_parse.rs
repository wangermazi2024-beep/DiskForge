//! 纯 MFT 记录解析逻辑，**无 `cfg(windows)` 限制**。
//!
//! 这个模块从 `mft_scan.rs` 抽出来，目的有两个：
//! 1. 让解析逻辑可以在 Linux 上跑单元测试（用合成的 MFT 字节流喂进去），
//!    验证字段提取是否正确、是否丢属性、Fixup 算法是否对。
//! 2. 让 `mft_scan.rs` 只剩 Windows 专有的 I/O 代码（CreateFileW / DeviceIoControl /
//!    AdjustTokenPrivileges），职责更清晰。
//!
//! 解析逻辑本身是纯字节操作，没有任何 Windows API 调用，所以可以跨平台编译。
//! 真正在 Windows 上跑时，`mft_scan::scan_drive_via_mft` 会用这里的
//! `apply_fixup` + `parse_record` 来处理读入内存的 $MFT 字节流。

use std::ffi::OsString;

// 在非 Windows 上也编过——OsString::from_wide 是 Windows 专有的，
// 这里给一个最小的 fallback：把 u16 序列当成 UTF-16 解码成 String，
// 再包装成 OsString。Linux 测试时只会用到这个 fallback 路径。
#[cfg(not(windows))]
mod utf16_fallback {
    use std::ffi::OsString;
    /// 把 u16 序列当成 UTF-16 解码后转成 OsString。
    /// 非 Windows 平台 OsString 内部就是 bytes（Unix）或 WTF-8（Wasm），
    /// 直接 from String 即可。
    pub fn from_wide(u16s: &[u16]) -> OsString {
        let s: String = String::from_utf16_lossy(u16s);
        OsString::from(s)
    }
}

#[cfg(windows)]
fn os_string_from_wide(u16s: &[u16]) -> OsString {
    use std::os::windows::ffi::OsStringExt;
    OsString::from_wide(u16s)
}
#[cfg(not(windows))]
fn os_string_from_wide(u16s: &[u16]) -> OsString {
    utf16_fallback::from_wide(u16s)
}

pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
pub const ATTR_FILE_NAME: u32 = 0x30;
pub const ATTR_DATA: u32 = 0x80;
pub const ATTR_END: u32 = 0xFFFF_FFFF;
pub const ROOT_RECORD_INDEX: u64 = 5; // NTFS 规定根目录固定是第 5 条 MFT 记录。

/// 单条 MFT 记录里我们关心的字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEntry {
    pub parent_record: u64,
    pub name: String,
    pub is_dir: bool,
    pub in_use: bool,
    pub is_base_record: bool,
    pub real_size: u64,
    /// 物理分配大小（allocated_length），与 real_size（逻辑大小）不同。
    /// 普通文件 ≈ real_size 向上取整，稀疏文件可能远小于 real_size。
    pub allocated_size: u64,
    /// 来自 $STANDARD_INFORMATION 的 LastModificationTime（FILETIME）。
    pub modified_ft: u64,
    /// 来自 $STANDARD_INFORMATION 的 Flags（FILE_ATTRIBUTE_*）。
    pub attributes: u32,
    /// **v14 新增**：硬链接场景下一条记录可能有多个 $FILE_NAME，各自指向不同父目录。
    /// 之前只保留"最优"的一个，其余 (parent, name) 直接丢弃，导致文件在除被选中
    /// 目录外的**所有其它硬链接位置彻底消失**（DriverStore\FileRepository 重灾区）。
    /// 这里保留除主链接外的其它链接，扫描阶段把文件同时挂到每个硬链接目录下。
    pub extra_links: Vec<(u64, String)>,
}

/// $STANDARD_INFORMATION 内容布局（resident，NTFS 3.0+ 是 72 字节，1.x 是 48 字节）：
///   +0x00 CreationTime      (FILETIME 8B)
///   +0x08 LastModificationTime (FILETIME 8B)  ← 我们要的修改时间
///   +0x10 LastMftChangeTime (FILETIME 8B)
///   +0x18 LastAccessTime    (FILETIME 8B)
///   +0x20 Flags             (4B, FILE_ATTRIBUTE_*)  ← 注意是 0x20，不是 0x30！
///   +0x24 MaximumVersions   (4B)
///   +0x28 Version           (4B)
///   +0x2C ClassId           (4B)
///   ── 以上 48 字节是 NTFS 1.x ──
///   +0x30 OwnerId           (4B, NTFS 3.0+)
///   +0x34 SecurityId        (4B)
///   +0x38 QuotaCharged      (8B)
///   +0x40 USN               (8B)
///
/// **重要修正**（v11）：之前把 Flags 放在 +0x30 是错的！正确是 +0x20。
/// $STANDARD_INFORMATION 里**没有** AllocatedSize / RealSize 字段（那些在 $FILE_NAME 里）。
/// 来源：ColinFinck/ntfs standard_information.rs + flatcap/linux-ntfs 文档。
pub const STD_INFO_OFFSET_MODIFIED: usize = 0x08;
pub const STD_INFO_OFFSET_FLAGS: usize = 0x20;
pub const STD_INFO_MIN_LEN: usize = 0x24; // 至少要读到 0x20 + 4

/// $ATTRIBUTE_LIST 条目布局（每条 26 字节，8 字节对齐）：
///   +0x00 type              (4B, 属性类型，如 0x80=$DATA)
///   +0x04 length            (2B, 本条目长度)
///   +0x06 name_length       (1B)
///   +0x07 name_offset       (1B)
///   +0x08 starting_vcn      (8B, 即 lowest_vcn)
///   +0x10 base_file_reference (8B, 低 48 位是 MFT 记录号)
///   +0x18 attribute_id      (2B)
/// 来源：flatcap/linux-ntfs attribute_list.html
pub const ATTR_LIST_ENTRY_SIZE: usize = 26;

/// 对单条 MFT 记录做 Fixup 修正。
///
/// NTFS 每个扇区（通常 512 字节）的最后 2 字节在落盘时会被替换成
/// USA（Update Sequence Array）的 USN 值，原值放到 USA 里。读出来时
/// 必须把 USN 检查通过后，再把原值放回去，否则记录的某些字段会读到
/// 错误的 USN 值而不是真实数据。
///
/// `record` 整条记录的字节（含 FILE 头）；`bytes_per_sector` 通常 512。
/// 返回 true 表示 Fixup 通过，可以继续解析；false 表示记录损坏，应丢弃。
pub fn apply_fixup(record: &mut [u8], bytes_per_sector: u32) -> bool {
    if record.len() < 8 {
        return false;
    }
    let usa_offset = u16::from_le_bytes([record[4], record[5]]) as usize;
    let usa_count = u16::from_le_bytes([record[6], record[7]]) as usize; // 含 USN 本身
    if usa_count == 0 || usa_offset + usa_count * 2 > record.len() {
        return false;
    }
    let usn = [record[usa_offset], record[usa_offset + 1]];
    let sector = bytes_per_sector.max(512) as usize;
    for i in 1..usa_count {
        let sector_end = i * sector; // 每个扇区最后 2 字节
        if sector_end > record.len() {
            break;
        }
        let check = &record[sector_end - 2..sector_end];
        if check != usn {
            // 该扇区的 USN 不匹配：记录已损坏/不完整，放弃这条记录。
            return false;
        }
        let orig_off = usa_offset + i * 2;
        record[sector_end - 2] = record[orig_off];
        record[sector_end - 1] = record[orig_off + 1];
    }
    true
}

/// `$ATTRIBUTE_LIST` 的一条条目（指向扩展记录里的属性）。
#[derive(Debug, Clone)]
pub struct AttributeListEntry {
    pub attr_type: u32,
    pub lowest_vcn: u64,
    /// MFT 记录号（已从 base_file_reference 低 48 位提取）
    pub record_number: u64,
    pub attribute_id: u16,
}

/// 解析 `$ATTRIBUTE_LIST` 属性内容（resident 或 non-resident 的 value bytes），
/// 返回所有条目。用来找扩展记录里的 `$DATA` 属性。
///
/// 每条 26 字节，8 字节对齐（实际长度由 entry.length 字段决定）。
pub fn parse_attribute_list(content: &[u8]) -> Vec<AttributeListEntry> {
    let mut entries = Vec::new();
    let mut off = 0usize;
    while off + ATTR_LIST_ENTRY_SIZE <= content.len() {
        let attr_type = u32::from_le_bytes(content[off..off + 4].try_into().unwrap());
        // 0xFFFFFFFF 是结束标记
        if attr_type == ATTR_END {
            break;
        }
        let entry_len = u16::from_le_bytes(content[off + 4..off + 6].try_into().unwrap()) as usize;
        if entry_len < ATTR_LIST_ENTRY_SIZE {
            break;
        }
        let lowest_vcn = u64::from_le_bytes(content[off + 8..off + 16].try_into().unwrap());
        let base_ref = u64::from_le_bytes(content[off + 16..off + 24].try_into().unwrap());
        let record_number = base_ref & 0x0000_FFFF_FFFF_FFFF; // 低 48 位
        let attribute_id = u16::from_le_bytes(content[off + 24..off + 26].try_into().unwrap());
        entries.push(AttributeListEntry {
            attr_type,
            lowest_vcn,
            record_number,
            attribute_id,
        });
        off += entry_len;
        // 8 字节对齐
        off = (off + 7) & !7;
    }
    entries
}

/// 在一条 MFT 记录里找未命名的 `$DATA` 属性（attr_type=0x80, name_len=0），
/// 返回它的逻辑大小（data_size）。
///
/// **v12 关键修正**（搜索微软文档 + ColinFinck/ntfs 确认）：
/// - non-resident $DATA 的 data_size 在 attr+0x30（attr+48）= FileSize = 逻辑大小
/// - **但只有 LowestVcn==0 的 extent 才有有效 data_size！**
///   微软文档原话："FileSize is not valid if LowestVcn is nonzero."
///   continuation extent（LowestVcn!=0）的 data_size 是 0 或垃圾值。
/// - 所以这个函数跳过 LowestVcn!=0 的 non-resident $DATA，只从 LowestVcn==0 的拿大小。
/// - resident $DATA：value_len（attr+0x10）= 文件大小（resident 文件总是单个 extent）
///
/// 可选的 instance_id 匹配：当从 $ATTRIBUTE_LIST 跟到扩展记录时，需要匹配 instance_id
///（attr+0x0E）来确保拿到正确的属性（扩展记录里可能有多个 $DATA extent）。
pub fn find_unnamed_data_size(record: &[u8], want_instance: Option<u16>) -> Option<u64> {
    if record.len() < 48 || &record[0..4] != b"FILE" {
        return None;
    }
    let first_attr_offset = u16::from_le_bytes([record[20], record[21]]) as usize;
    let mut off = first_attr_offset;
    while off + 16 <= record.len() {
        let attr_type = u32::from_le_bytes(record[off..off + 4].try_into().unwrap());
        if attr_type == ATTR_END {
            break;
        }
        let attr_len = u32::from_le_bytes(record[off + 4..off + 8].try_into().unwrap()) as usize;
        if attr_len == 0 || off + attr_len > record.len() {
            break;
        }
        let non_resident = record[off + 8] != 0;
        let name_len = record[off + 9];
        let instance_id = u16::from_le_bytes([record[off + 14], record[off + 15]]);

        // 只看未命名的 $DATA 属性（name_len == 0）
        if attr_type == ATTR_DATA && name_len == 0 {
            // 如果指定了 instance_id，必须匹配（用于扩展记录查找）
            if let Some(want) = want_instance {
                if instance_id != want {
                    off += attr_len;
                    continue;
                }
            }
            if !non_resident {
                // resident $DATA: value_len 在 attr+0x10（总是单个 extent，直接返回）
                return Some(u32::from_le_bytes(record[off + 16..off + 20].try_into().unwrap()) as u64);
            }
            // non-resident $DATA: 检查 LowestVcn（attr+0x10）
            // 只有 LowestVcn==0 的 extent 才有有效 data_size
            if off + 56 > record.len() {
                off += attr_len;
                continue;
            }
            let lowest_vcn = u64::from_le_bytes(record[off + 16..off + 24].try_into().unwrap());
            if lowest_vcn == 0 {
                // data_size 在 attr+0x30（attr+48）= FileSize = 逻辑大小
                return Some(u64::from_le_bytes(record[off + 48..off + 56].try_into().unwrap()));
            }
            // LowestVcn!=0 的 continuation extent：data_size 无效，继续找下一个
        }
        off += attr_len;
    }
    None
}

/// 在一条 MFT 记录里找 `$ATTRIBUTE_LIST` 属性（attr_type=0x20），
/// 返回它的内容字节（resident 的 value，或 non-resident 我们暂时不支持）。
///
/// **注意**：$ATTRIBUTE_LIST 本身也可能是 non-resident（当条目太多时），
/// 这种情况我们暂时返回 None（fallback 到 $FILE_NAME.RealSize，虽然不准但比 0 强）。
/// 大多数文件的 $ATTRIBUTE_LIST 是 resident 的。
pub fn find_attribute_list_content<'a>(record: &'a [u8]) -> Option<&'a [u8]> {
    if record.len() < 48 || &record[0..4] != b"FILE" {
        return None;
    }
    let first_attr_offset = u16::from_le_bytes([record[20], record[21]]) as usize;
    let mut off = first_attr_offset;
    while off + 16 <= record.len() {
        let attr_type = u32::from_le_bytes(record[off..off + 4].try_into().unwrap());
        if attr_type == ATTR_END {
            break;
        }
        let attr_len = u32::from_le_bytes(record[off + 4..off + 8].try_into().unwrap()) as usize;
        if attr_len == 0 || off + attr_len > record.len() {
            break;
        }
        let non_resident = record[off + 8] != 0;
        let value_off = u16::from_le_bytes(record[off + 20..off + 22].try_into().unwrap()) as usize;
        let value_len = u32::from_le_bytes(record[off + 16..off + 20].try_into().unwrap()) as usize;
        let content_start = off + value_off;

        if attr_type == ATTR_ATTRIBUTE_LIST && !non_resident {
            // resident $ATTRIBUTE_LIST：直接返回内容切片
            if content_start + value_len <= record.len() {
                return Some(&record[content_start..content_start + value_len]);
            }
        }
        // non-resident $ATTRIBUTE_LIST 暂不支持（需要读 data runs），返回 None
        off += attr_len;
    }
    None
}

/// 非 resident `$ATTRIBUTE_LIST` 的原始信息：mapping pairs（data runs 编码）+ 属性真实大小。
///
/// **v13 关键修正**：`$ATTRIBUTE_LIST` 本身也可能是 non-resident 的——每条 26 字节，
/// 一条 MFT 记录只有 1024 字节（刨去记录头等，能放下的条目数约 30~40 条），一旦文件
/// 碎片非常多（比如严重碎片化的大文件，$DATA 分布在很多个扩展记录里），条目数超过这个
/// 上限，NTFS 就会把 `$ATTRIBUTE_LIST` 变成 non-resident。
/// 之前 `find_attribute_list_content` 遇到这种情况直接返回 `None`，
/// 调用方就放弃了这个文件的大小解析——real_size 永远卡在 0，这是"扫描汇总比系统已用小"
/// 的主要原因之一（大文件、严重碎片化的文件最容易触发）。
/// 这里手动解析 mapping pairs，让调用方能自己按簇把内容读出来重建。
pub struct NonResidentAttrListInfo {
    /// 属性内容的真实长度（字节），即 data_size。
    pub data_size: u64,
    /// mapping pairs（NTFS data runs 的紧凑变长编码），需要 `parse_data_runs` 解码。
    pub mapping_pairs: Vec<u8>,
}

/// 在一条 MFT 记录里找 non-resident 的 `$ATTRIBUTE_LIST` 属性，返回其 mapping pairs
/// 和真实大小。找不到（没有该属性，或该属性是 resident 的）返回 `None`。
///
/// non-resident 属性头布局（NTFS 标准，来源 flatcap/linux-ntfs concepts/attribute_header.html）：
///   +0x00 type                 (4B)
///   +0x04 length                (4B)
///   +0x08 non_resident_flag     (1B, 非 0)
///   +0x09 name_length           (1B)
///   +0x0A name_offset           (2B)
///   +0x0C flags                 (2B)
///   +0x0E attribute_id          (2B)
///   +0x10 starting_vcn          (8B)
///   +0x18 last_vcn (highest)    (8B)
///   +0x20 mapping_pairs_offset  (2B)
///   +0x22 compression_unit      (2B)
///   +0x24 padding               (4B)
///   +0x28 allocated_size        (8B)
///   +0x30 data_size             (8B)  ← 真实大小
///   +0x38 initialized_size      (8B)
pub fn find_attribute_list_nonresident(record: &[u8]) -> Option<NonResidentAttrListInfo> {
    if record.len() < 48 || &record[0..4] != b"FILE" {
        return None;
    }
    let first_attr_offset = u16::from_le_bytes([record[20], record[21]]) as usize;
    let mut off = first_attr_offset;
    while off + 16 <= record.len() {
        let attr_type = u32::from_le_bytes(record[off..off + 4].try_into().unwrap());
        if attr_type == ATTR_END {
            break;
        }
        let attr_len = u32::from_le_bytes(record[off + 4..off + 8].try_into().unwrap()) as usize;
        if attr_len == 0 || off + attr_len > record.len() {
            break;
        }
        let non_resident = record[off + 8] != 0;
        if attr_type == ATTR_ATTRIBUTE_LIST && non_resident {
            if off + 0x38 > record.len() {
                off += attr_len;
                continue;
            }
            let mapping_pairs_offset =
                u16::from_le_bytes(record[off + 0x20..off + 0x22].try_into().unwrap()) as usize;
            let data_size = u64::from_le_bytes(record[off + 0x30..off + 0x38].try_into().unwrap());
            let mp_start = off + mapping_pairs_offset;
            let mp_end = off + attr_len;
            if mapping_pairs_offset == 0 || mp_start >= mp_end || mp_end > record.len() {
                off += attr_len;
                continue;
            }
            return Some(NonResidentAttrListInfo {
                data_size,
                mapping_pairs: record[mp_start..mp_end].to_vec(),
            });
        }
        off += attr_len;
    }
    None
}

/// 解析 NTFS "Mapping Pairs"（data runs 的紧凑变长编码）。
///
/// 返回一串 `(簇数, 绝对LCN)`：LCN 是靠每个 run 里的有符号增量累加出来的绝对簇号；
/// sparse run（洞，没有物理位置）用 `None` 表示。
///
/// 编码格式：每个 run 一个 header 字节，header 低 4 位 = length 字段字节数，
/// 高 4 位 = offset(LCN 增量) 字段字节数；header == 0x00 表示结束；
/// offset 字段字节数 == 0 表示 sparse run。offset 字段是补码有符号数，需要符号扩展。
/// 来源：flatcap/linux-ntfs concepts/data_runs.html，以及 NTFS-3G runlist.c 的实现。
pub fn parse_data_runs(mapping_pairs: &[u8]) -> Vec<(u64, Option<i64>)> {
    let mut runs = Vec::new();
    let mut off = 0usize;
    let mut lcn: i64 = 0;
    while off < mapping_pairs.len() {
        let header = mapping_pairs[off];
        if header == 0 {
            break;
        }
        let length_size = (header & 0x0F) as usize;
        let offset_size = ((header >> 4) & 0x0F) as usize;
        off += 1;
        if length_size == 0 || off + length_size > mapping_pairs.len() {
            break;
        }
        let mut length_bytes = [0u8; 8];
        length_bytes[..length_size].copy_from_slice(&mapping_pairs[off..off + length_size]);
        let cluster_count = u64::from_le_bytes(length_bytes);
        off += length_size;

        if offset_size == 0 {
            // sparse run：没有物理簇，读取时应视为全 0。
            runs.push((cluster_count, None));
            continue;
        }
        if off + offset_size > mapping_pairs.len() {
            break;
        }
        let mut offset_bytes = [0u8; 8];
        offset_bytes[..offset_size].copy_from_slice(&mapping_pairs[off..off + offset_size]);
        let mut delta = i64::from_le_bytes(offset_bytes);
        // 符号扩展：offset 是补码有符号数，若最高有效字节的最高位是 1，代表负数。
        if offset_size < 8 && (offset_bytes[offset_size - 1] & 0x80) != 0 {
            let sign_ext_mask = !0i64 << (offset_size * 8);
            delta |= sign_ext_mask;
        }
        off += offset_size;
        lcn += delta;
        runs.push((cluster_count, Some(lcn)));
    }
    runs
}

/// **v16：已废弃，不再参与主扫描流程。** 主扫描路径（`mft_scan::scan_drive_via_mft`）
/// 现在 100% 用 `omerbenamram/mft` 这个库来做单条记录级别的解析（fixup、属性遍历、
/// resident/non-resident 判断都是库做的），不再自己手撸字节解析。这个函数只保留
/// 下来当回滚/对照参考，当前没有任何调用点（`mft_scan.rs` 只还在用本文件里的
/// `RawEntry` 数据结构定义和 `parse_data_runs`——后者不是"记录解析算法"，是给
/// Windows 卷设备直读定位 `$MFT` 自身簇碎片用的底层工具，这一步库不管，因为库的
/// 输入就假设你已经拿到了完整的 MFT 字节缓冲区）。
///
/// 解析单条 MFT FILE 记录，提取我们关心的字段。
///
/// 输入是已经过 `apply_fixup` 的字节切片。返回 `None` 表示这条记录
/// 不是有效的 FILE 记录（magic 不对 / 长度不够 / 没有 $FILE_NAME）。
///
/// **v11 修正**：
/// - 不再返回 `<FRN_0>` 占位条目。没 $FILE_NAME 的记录（系统元数据文件 12-15、
///   扩展记录等）返回 None，由调用方跳过——和 WizTree/SpaceSniffer 行为一致。
/// - `real_size` 只从 base record 的未命名 $DATA 拿。如果 base record 没有 $DATA
///   （大文件 $DATA 在扩展记录），这里返回 0，由扫描阶段用 `resolve_file_size`
///   跟 $ATTRIBUTE_LIST 去扩展记录拿真实大小。
/// - 不再用 $FILE_NAME.RealSize（它是 stale 的，只在改名时更新，不可靠）。
/// - `$STANDARD_INFORMATION` 的 Flags 改成 +0x20（之前错误地用 +0x30）。
#[allow(dead_code)]
pub fn parse_record(record: &[u8]) -> Option<RawEntry> {
    if record.len() < 48 || &record[0..4] != b"FILE" {
        return None;
    }
    let flags = u16::from_le_bytes([record[22], record[23]]);
    let in_use = flags & 0x0001 != 0;
    let is_dir = flags & 0x0002 != 0; // is_dir 从 FILE record header flags 拿，最可靠
    let first_attr_offset = u16::from_le_bytes([record[20], record[21]]) as usize;
    let base_record_ref = u64::from_le_bytes(record[32..40].try_into().unwrap());
    let is_base_record = (base_record_ref & 0x0000_FFFF_FFFF_FFFF) == 0;

    // 收集所有 $FILE_NAME 属性，最后按优先级选最好的一个。
    // 每个元素: (namespace, name, parent_ref)
    let mut file_names: Vec<(u8, String, u64)> = Vec::new();
    let mut real_size: u64 = 0; // 来自 base record 的 $DATA（可能为 0，需扫描阶段 resolve）
    let mut modified_ft: u64 = 0;
    let mut attributes: u32 = 0;

    let mut off = first_attr_offset;
    while off + 16 <= record.len() {
        let attr_type = u32::from_le_bytes(record[off..off + 4].try_into().unwrap());
        if attr_type == ATTR_END {
            break;
        }
        let attr_len = u32::from_le_bytes(record[off + 4..off + 8].try_into().unwrap()) as usize;
        if attr_len == 0 || off + attr_len > record.len() {
            break;
        }
        let non_resident = record[off + 8] != 0;
        let value_off = u16::from_le_bytes(record[off + 20..off + 22].try_into().unwrap()) as usize;
        let value_len = u32::from_le_bytes(record[off + 16..off + 20].try_into().unwrap()) as usize;
        let content_start = off + value_off;

        if attr_type == ATTR_STANDARD_INFORMATION && !non_resident {
            // v11 修正：Flags 在 +0x20（不是 +0x30）
            if content_start + STD_INFO_MIN_LEN <= record.len() && value_len >= STD_INFO_MIN_LEN {
                modified_ft = u64::from_le_bytes(
                    record[content_start + STD_INFO_OFFSET_MODIFIED
                        ..content_start + STD_INFO_OFFSET_MODIFIED + 8]
                        .try_into()
                        .unwrap(),
                );
                attributes = u32::from_le_bytes(
                    record[content_start + STD_INFO_OFFSET_FLAGS
                        ..content_start + STD_INFO_OFFSET_FLAGS + 4]
                        .try_into()
                        .unwrap(),
                );
            }
        } else if attr_type == ATTR_FILE_NAME && !non_resident {
            // $FILE_NAME 内容布局（v11 确认）：
            //   +0  ParentReference (8B)
            //   +8  CreationTime (8B)
            //   +16 ModificationTime (8B)
            //   +24 MftChangeTime (8B)
            //   +32 AccessTime (8B)
            //   +40 AllocatedSize (8B)
            //   +48 RealSize (8B)  ← stale！不用来拿大小
            //   +56 Flags (4B)     ← stale！不用来拿属性
            //   +60 ReparseValue (4B)
            //   +64 NameLength (1B)
            //   +65 Namespace (1B)
            //   +66 Name (UTF-16LE)
            if content_start + value_len <= record.len() && value_len >= 66 {
                let c = &record[content_start..content_start + value_len];
                let parent_ref = u64::from_le_bytes(c[0..8].try_into().unwrap());
                let name_len_chars = c[64] as usize;
                let ns = c[65]; // 0=POSIX 1=WIN32 2=DOS(8.3短名) 3=WIN32&DOS
                let name_bytes_len = name_len_chars * 2;
                if 66 + name_bytes_len <= c.len() && name_len_chars > 0 {
                    let u16s: Vec<u16> = c[66..66 + name_bytes_len]
                        .chunks_exact(2)
                        .map(|b| u16::from_le_bytes([b[0], b[1]]))
                        .collect();
                    let name_str = os_string_from_wide(&u16s).to_string_lossy().into_owned();
                    file_names.push((ns, name_str, parent_ref & 0x0000_FFFF_FFFF_FFFF));
                }
            }
        } else if attr_type == ATTR_DATA {
            // 未命名的 $DATA（name_len==0）才有文件主体大小
            let name_len = record[off + 9];
            if name_len == 0 {
                if !non_resident {
                    // resident $DATA: value_len 在 attr+0x10
                    real_size = u32::from_le_bytes(record[off + 16..off + 20].try_into().unwrap()) as u64;
                } else {
                    // v12 关键修正：non-resident $DATA 的 data_size 在 attr+0x30，
                    // 但只有 LowestVcn==0 的 extent 才有效！
                    // continuation extent（LowestVcn!=0）的 data_size 是 0/垃圾。
                    if off + 56 <= record.len() {
                        let lowest_vcn = u64::from_le_bytes(record[off + 16..off + 24].try_into().unwrap());
                        if lowest_vcn == 0 {
                            real_size = u64::from_le_bytes(record[off + 48..off + 56].try_into().unwrap());
                        }
                        // else: continuation extent，跳过（data_size 无效）
                    }
                }
            }
        }

        off += attr_len;
    }

    // v11: 没 $FILE_NAME 的记录返回 None（跳过，不显示）
    // 这些是系统元数据文件（记录 12-15）或扩展记录，WizTree 也不显示。
    if file_names.is_empty() {
        return None;
    }

    // 从收集到的 $FILE_NAME 列表里选最好的一个。
    // 优先级：WIN32(1) > WIN32&DOS(3) > POSIX(0) > DOS(2) > 其他
    let ns_priority = |ns: u8| -> u32 {
        match ns {
            1 => 0, // WIN32 - 最高优先级（长文件名）
            3 => 1, // WIN32&DOS - 第二
            0 => 2, // POSIX - 第三
            2 => 3, // DOS - 最低（8.3 短名，如 PROGRA~1）
            _ => 4,
        }
    };
    // v14: 找到"最优"名字的下标，其余的作为 extra_links 保留（可能是硬链接的
    // 其它目录位置，也可能是同一目录下 WIN32/DOS 两条重复记录——后者会在扫描阶段
    // 按 (parent_record, name) 去重，不会产生幽灵条目）。
    let best_idx = file_names
        .iter()
        .enumerate()
        .min_by_key(|(_, (ns, _, _))| ns_priority(*ns))
        .map(|(i, _)| i)
        .unwrap();
    let name = file_names[best_idx].1.clone();
    let parent_record = file_names[best_idx].2;
    let extra_links: Vec<(u64, String)> = file_names
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != best_idx)
        .map(|(_, (_, n, p))| (*p, n.clone()))
        .filter(|(p, n)| *p != parent_record || *n != name) // 去掉和主链接完全相同的重复
        .collect();

    Some(RawEntry {
        parent_record,
        name,
        is_dir,
        in_use,
        is_base_record,
        real_size,
        allocated_size: real_size, // mft_parse 自解析无 allocated_length 信息，沿用 file_size
        modified_ft,
        attributes,
        extra_links,
    })
}

/// 诊断版：返回 (RawEntry, 解析到的属性类型列表, 是否有 $FILE_NAME)。
/// 用来排查"no_file_name"问题——看那些记录到底有哪些属性。
#[allow(dead_code)]
pub fn parse_record_with_diag(record: &[u8]) -> Option<(RawEntry, Vec<u32>, bool)> {
    if record.len() < 48 || &record[0..4] != b"FILE" {
        return None;
    }
    // 先用标准 parse_record 拿到 entry
    let entry = parse_record(record)?;

    // 再单独扫一遍收集属性类型列表和 has_file_name
    let first_attr_offset = u16::from_le_bytes([record[20], record[21]]) as usize;
    let mut attr_types: Vec<u32> = Vec::new();
    let mut has_file_name = false;
    let mut off = first_attr_offset;
    while off + 16 <= record.len() {
        let attr_type = u32::from_le_bytes(record[off..off + 4].try_into().unwrap());
        if attr_type == ATTR_END {
            break;
        }
        attr_types.push(attr_type);
        if attr_type == ATTR_FILE_NAME {
            has_file_name = true;
        }
        let attr_len = u32::from_le_bytes(record[off + 4..off + 8].try_into().unwrap()) as usize;
        if attr_len == 0 || off + attr_len > record.len() {
            break;
        }
        off += attr_len;
    }
    Some((entry, attr_types, has_file_name))
}

// ─────────────────────────────────────────────────────────────────────────
// 单元测试：用合成的 MFT 字节流验证解析逻辑，**Linux 上也能跑**。
// ─────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    fn put_u64(buf: &mut Vec<u8>, off: usize, v: u64) {
        let bytes = v.to_le_bytes();
        buf[off..off + 8].copy_from_slice(&bytes);
    }
    fn put_u32(buf: &mut Vec<u8>, off: usize, v: u32) {
        let bytes = v.to_le_bytes();
        buf[off..off + 4].copy_from_slice(&bytes);
    }
    fn put_u16(buf: &mut Vec<u8>, off: usize, v: u16) {
        let bytes = v.to_le_bytes();
        buf[off..off + 2].copy_from_slice(&bytes);
    }

    /// 构造一个最小的 MFT FILE 记录字节流。
    fn build_test_record(
        name: &str,
        parent_record: u64,
        is_dir: bool,
        real_size: u64,
        modified_ft: u64,
        attributes: u32,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; 1024];

        // ── FILE 头 ──────────────────────────────────────────
        buf[0..4].copy_from_slice(b"FILE");
        put_u16(&mut buf, 4, 0x30); // usa_offset
        put_u16(&mut buf, 6, 1); // usa_count = 1（只有 USN 本身，不修正扇区）
        put_u16(&mut buf, 0x30, 0x0001); // USN 值
        let flags: u16 = 0x01 | if is_dir { 0x02 } else { 0x00 };
        put_u16(&mut buf, 22, flags);
        put_u16(&mut buf, 20, 0x38); // first_attr_offset
        put_u64(&mut buf, 32, 0); // base_record_ref = 0

        let mut off = 0x38usize;

        // ── $STANDARD_INFORMATION 属性 ─────────────────────
        let std_attr_content_len: usize = 0x48;
        let std_attr_total_len: usize = 24 + std_attr_content_len;
        put_u32(&mut buf, off, ATTR_STANDARD_INFORMATION);
        put_u32(&mut buf, off + 4, std_attr_total_len as u32);
        buf[off + 8] = 0; // resident
        put_u32(&mut buf, off + 16, std_attr_content_len as u32);
        put_u16(&mut buf, off + 20, 24); // value_off
        let content_off = off + 24;
        // v11 $STANDARD_INFORMATION 布局：
        //   +0x00 CreationTime, +0x08 ModificationTime, +0x10 MftChangeTime, +0x18 AccessTime
        //   +0x20 Flags(4B), +0x24 MaxVersions, +0x28 Version, +0x2C ClassId
        put_u64(&mut buf, content_off + 0x00, 0); // CreationTime
        put_u64(&mut buf, content_off + 0x08, modified_ft); // ModificationTime
        put_u64(&mut buf, content_off + 0x10, 0); // MftChangeTime
        put_u64(&mut buf, content_off + 0x18, 0); // AccessTime
        put_u32(&mut buf, content_off + 0x20, attributes); // Flags ← v11 修正位置
        put_u32(&mut buf, content_off + 0x24, 0); // MaxVersions
        put_u32(&mut buf, content_off + 0x28, 0); // Version
        put_u32(&mut buf, content_off + 0x2C, 0); // ClassId
        off += std_attr_total_len;

        // ── $FILE_NAME 属性 ─────────────────────────────────
        let name_u16: Vec<u16> = name.encode_utf16().collect();
        let name_bytes_len = name_u16.len() * 2;
        let fn_content_len: usize = 66 + name_bytes_len;
        let fn_attr_total_len: usize = (24 + fn_content_len + 7) & !7;
        put_u32(&mut buf, off, ATTR_FILE_NAME);
        put_u32(&mut buf, off + 4, fn_attr_total_len as u32);
        buf[off + 8] = 0;
        put_u32(&mut buf, off + 16, fn_content_len as u32);
        put_u16(&mut buf, off + 20, 24);
        let fn_content_off = off + 24;
        put_u64(&mut buf, fn_content_off + 0, parent_record);
        // 时间字段全 0
        for i in (8..64).step_by(8) {
            put_u64(&mut buf, fn_content_off + i, 0);
        }
        buf[fn_content_off + 64] = name_u16.len() as u8; // name_len
        buf[fn_content_off + 65] = 1; // ns = WIN32
        for (i, &c) in name_u16.iter().enumerate() {
            put_u16(&mut buf, fn_content_off + 66 + i * 2, c);
        }
        off += fn_attr_total_len;

        // ── $DATA 属性（resident） ──────────────────────────
        let data_attr_total_len: usize = 32;
        put_u32(&mut buf, off, ATTR_DATA);
        put_u32(&mut buf, off + 4, data_attr_total_len as u32);
        buf[off + 8] = 0; // resident
        buf[off + 9] = 0; // name_len = 0
        put_u32(&mut buf, off + 16, real_size as u32); // value_len
        put_u16(&mut buf, off + 20, 24);
        off += data_attr_total_len;

        // ── 属性结束标记 ───────────────────────────────────
        put_u32(&mut buf, off, ATTR_END);
        put_u32(&mut buf, off + 4, 0);
        off += 8;

        buf.truncate(off);
        while buf.len() < 512 {
            buf.push(0);
        }
        buf
    }

    #[test]
    fn test_parse_file_record() {
        let modified_ft: u64 = 132_000_000_000_000_000;
        let attributes: u32 = 0x20; // ARCHIVE
        let mut record = build_test_record("hello.txt", 42, false, 123_456, modified_ft, attributes);
        assert!(apply_fixup(&mut record, 512), "apply_fixup should succeed");
        let entry = parse_record(&record).expect("parse_record should return Some");
        assert_eq!(entry.name, "hello.txt");
        assert_eq!(entry.parent_record, 42);
        assert!(!entry.is_dir);
        assert!(entry.in_use);
        assert!(entry.is_base_record);
        assert_eq!(entry.real_size, 123_456);
        assert_eq!(entry.modified_ft, modified_ft);
        assert_eq!(entry.attributes, attributes);
    }

    #[test]
    fn test_parse_directory_record() {
        let modified_ft: u64 = 132_500_000_000_000_000;
        let attributes: u32 = 0x10; // DIRECTORY
        let mut record = build_test_record("MyFolder", 5, true, 0, modified_ft, attributes);
        assert!(apply_fixup(&mut record, 512));
        let entry = parse_record(&record).expect("parse_record should return Some");
        assert_eq!(entry.name, "MyFolder");
        assert_eq!(entry.parent_record, 5);
        assert!(entry.is_dir);
        assert_eq!(entry.attributes, 0x10);
    }

    #[test]
    fn test_parse_chinese_name() {
        let mut record = build_test_record("测试文件.txt", 7, false, 999, 0, 0x20);
        assert!(apply_fixup(&mut record, 512));
        let entry = parse_record(&record).expect("parse_record should return Some");
        assert_eq!(entry.name, "测试文件.txt");
        assert_eq!(entry.parent_record, 7);
    }

    #[test]
    fn test_parse_invalid_magic() {
        let mut buf = vec![0u8; 1024];
        buf[0..4].copy_from_slice(b"BAAD");
        let _ = apply_fixup(&mut buf, 512);
        assert!(parse_record(&buf).is_none());
    }

    #[test]
    fn test_parse_too_short() {
        let buf = vec![0u8; 10];
        assert!(parse_record(&buf).is_none());
    }

    #[test]
    fn test_apply_fixup_usn_mismatch() {
        let mut buf = vec![0u8; 1024];
        buf[0..4].copy_from_slice(b"FILE");
        put_u16(&mut buf, 4, 0x30);
        put_u16(&mut buf, 6, 2); // usa_count = 2（检查 1 个扇区）
        put_u16(&mut buf, 0x30, 0x1234); // USN
        put_u16(&mut buf, 510, 0x5678); // 扇区末尾不匹配
        assert!(!apply_fixup(&mut buf, 512));
    }

    #[test]
    fn test_apply_fixup_usn_match() {
        let mut buf = vec![0u8; 1024];
        buf[0..4].copy_from_slice(b"FILE");
        put_u16(&mut buf, 4, 0x30);
        put_u16(&mut buf, 6, 2);
        put_u16(&mut buf, 0x30, 0x1234); // USN
        put_u16(&mut buf, 510, 0x1234); // 扇区末尾匹配
        put_u16(&mut buf, 0x32, 0xABCD); // 原值
        assert!(apply_fixup(&mut buf, 512));
        let restored = u16::from_le_bytes([buf[510], buf[511]]);
        assert_eq!(restored, 0xABCD);
    }

    #[test]
    fn test_parse_multiple_records_no_loss() {
        let records_data = vec![
            ("file_a.txt", 5u64, false, 100u64, 132_000_000_000_000_000u64, 0x20u32),
            ("file_b.log", 5, false, 200, 132_100_000_000_000_000, 0x20),
            ("subdir", 5, true, 0, 132_200_000_000_000_000, 0x10),
            ("file_c.bin", 5, false, 300, 132_300_000_000_000_000, 0xA0),
        ];
        let mut parsed_count = 0;
        for (name, parent, is_dir, size, mft, attrs) in &records_data {
            let mut rec = build_test_record(name, *parent, *is_dir, *size, *mft, *attrs);
            assert!(apply_fixup(&mut rec, 512), "fixup failed for {}", name);
            let entry = parse_record(&rec).expect(&format!("parse failed for {}", name));
            assert_eq!(entry.name, *name);
            assert_eq!(entry.parent_record, *parent);
            assert_eq!(entry.is_dir, *is_dir);
            assert_eq!(entry.real_size, *size);
            assert_eq!(entry.modified_ft, *mft);
            assert_eq!(entry.attributes, *attrs);
            parsed_count += 1;
        }
        assert_eq!(parsed_count, records_data.len(), "应该解析出全部记录");
    }

    /// v11 测试：当 $DATA 不在 base record 里（大文件用扩展记录），
    /// parse_record 应该返回 real_size=0（不再用 $FILE_NAME.RealSize fallback，
    /// 因为它是 stale 的）。真实大小由扫描阶段跟 $ATTRIBUTE_LIST 去扩展记录拿。
    #[test]
    fn test_real_size_zero_when_no_data_in_base_record() {
        let mut buf = vec![0u8; 1024];
        buf[0..4].copy_from_slice(b"FILE");
        put_u16(&mut buf, 4, 0x30); // usa_offset
        put_u16(&mut buf, 6, 1); // usa_count = 1
        put_u16(&mut buf, 0x30, 0x0001); // USN
        put_u16(&mut buf, 22, 0x01); // flags: in_use, not dir
        put_u16(&mut buf, 20, 0x38); // first_attr_offset
        put_u64(&mut buf, 32, 0); // base_record_ref = 0

        let mut off = 0x38usize;

        // $STANDARD_INFORMATION（v11 布局，Flags 在 +0x20）
        let std_len: usize = 0x48;
        let std_total: usize = 24 + std_len;
        put_u32(&mut buf, off, ATTR_STANDARD_INFORMATION);
        put_u32(&mut buf, off + 4, std_total as u32);
        buf[off + 8] = 0;
        put_u32(&mut buf, off + 16, std_len as u32);
        put_u16(&mut buf, off + 20, 24);
        let std_off = off + 24;
        put_u64(&mut buf, std_off + 0x08, 13_200_000_000_000_000_000u64); // ModificationTime
        put_u32(&mut buf, std_off + 0x20, 0x20); // Flags = ARCHIVE
        off += std_total;

        // $FILE_NAME（带 RealSize = 214281216，但 v11 不再用它）
        let fname_size: u64 = 214_281_216;
        let name = "Hermes.exe";
        let name_u16: Vec<u16> = name.encode_utf16().collect();
        let fn_content_len: usize = 66 + name_u16.len() * 2;
        let fn_total: usize = (24 + fn_content_len + 7) & !7;
        put_u32(&mut buf, off, ATTR_FILE_NAME);
        put_u32(&mut buf, off + 4, fn_total as u32);
        buf[off + 8] = 0;
        put_u32(&mut buf, off + 16, fn_content_len as u32);
        put_u16(&mut buf, off + 20, 24);
        let fn_off = off + 24;
        put_u64(&mut buf, fn_off + 0, 5);
        put_u64(&mut buf, fn_off + 48, fname_size); // $FILE_NAME.RealSize（stale，不用）
        buf[fn_off + 64] = name_u16.len() as u8;
        buf[fn_off + 65] = 1; // ns = WIN32
        for (i, &c) in name_u16.iter().enumerate() {
            put_u16(&mut buf, fn_off + 66 + i * 2, c);
        }
        off += fn_total;

        // 没有 $DATA 属性！直接放 ATTR_END
        put_u32(&mut buf, off, ATTR_END);
        put_u32(&mut buf, off + 4, 0);

        assert!(apply_fixup(&mut buf, 512));
        let entry = parse_record(&buf).expect("parse should succeed");
        assert_eq!(entry.name, "Hermes.exe");
        // v11: real_size 应该是 0（$DATA 不在 base record），不是从 $FILE_NAME 拿
        assert_eq!(
            entry.real_size, 0,
            "real_size 应该是 0（$DATA 在扩展记录），扫描阶段再解析"
        );
        // 但 attributes 应该从 $STANDARD_INFORMATION 拿到（ARCHIVE=0x20）
        assert_eq!(entry.attributes, 0x20, "attributes 应该从 $STANDARD_INFORMATION.Flags(+0x20) 拿");
    }

    /// v13 测试：mapping pairs 解码，单个 run，length=0x1000 簇，offset(LCN)=+0x1234。
    #[test]
    fn test_parse_data_runs_single_positive() {
        // header: length_size=2, offset_size=2 => 0x22
        // length = 0x1000 (2 bytes LE: 00 10)；offset = 0x1234 (2 bytes LE: 34 12)
        let mp = vec![0x22, 0x00, 0x10, 0x34, 0x12, 0x00];
        let runs = parse_data_runs(&mp);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0], (0x1000, Some(0x1234)));
    }

    #[test]
    fn test_parse_data_runs_negative_offset() {
        // 第一个 run: length_size=1,offset_size=1 -> length=0x10, offset=0x50 (lcn=0x50)
        // 第二个 run: length=0x20, offset=0xF0 视为 i8 = -16 (lcn = 0x50-16 = 0x40)，
        // 模拟碎片文件里 LCN 往回跳的常见情况。
        let mp = vec![
            0x11, 0x10, 0x50,
            0x11, 0x20, 0xF0,
            0x00,
        ];
        let runs = parse_data_runs(&mp);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0], (0x10, Some(0x50)));
        assert_eq!(runs[1], (0x20, Some(0x50 - 16)));
    }

    #[test]
    fn test_parse_data_runs_sparse() {
        // offset_size=0 表示 sparse run：length=0x30，没有 offset 字段
        let mp = vec![0x01, 0x30, 0x00];
        let runs = parse_data_runs(&mp);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0], (0x30, None));
    }

    /// 测试文件名优先级：WIN32 名应该胜过 DOS 8.3 短名。
    /// 模拟 "Program Files" 目录，同时有 ns=1 (WIN32) 和 ns=2 (DOS "PROGRA~1")。
    #[test]
    fn test_filename_prefers_win32_over_dos() {
        let mut buf = vec![0u8; 1024];
        buf[0..4].copy_from_slice(b"FILE");
        put_u16(&mut buf, 4, 0x30);
        put_u16(&mut buf, 6, 1);
        put_u16(&mut buf, 0x30, 0x0001);
        put_u16(&mut buf, 22, 0x03); // flags: in_use + directory
        put_u16(&mut buf, 20, 0x38);
        put_u64(&mut buf, 32, 0);

        let mut off = 0x38usize;

        // 先放 DOS 名 (ns=2, "PROGRA~1") — 故意放前面，测试优先级
        let dos_name = "PROGRA~1";
        let dos_u16: Vec<u16> = dos_name.encode_utf16().collect();
        let dos_bytes = dos_u16.len() * 2;
        let dos_content = 66 + dos_bytes;
        let dos_total = (24 + dos_content + 7) & !7;
        put_u32(&mut buf, off, ATTR_FILE_NAME);
        put_u32(&mut buf, off + 4, dos_total as u32);
        buf[off + 8] = 0;
        put_u32(&mut buf, off + 16, dos_content as u32);
        put_u16(&mut buf, off + 20, 24);
        let dos_off = off + 24;
        put_u64(&mut buf, dos_off + 0, 5);
        put_u64(&mut buf, dos_off + 48, 0);
        buf[dos_off + 64] = dos_u16.len() as u8;
        buf[dos_off + 65] = 2; // ns = DOS
        for (i, &c) in dos_u16.iter().enumerate() {
            put_u16(&mut buf, dos_off + 66 + i * 2, c);
        }
        off += dos_total;

        // 再放 WIN32 名 (ns=1, "Program Files")
        let win_name = "Program Files";
        let win_u16: Vec<u16> = win_name.encode_utf16().collect();
        let win_bytes = win_u16.len() * 2;
        let win_content = 66 + win_bytes;
        let win_total = (24 + win_content + 7) & !7;
        put_u32(&mut buf, off, ATTR_FILE_NAME);
        put_u32(&mut buf, off + 4, win_total as u32);
        buf[off + 8] = 0;
        put_u32(&mut buf, off + 16, win_content as u32);
        put_u16(&mut buf, off + 20, 24);
        let win_off = off + 24;
        put_u64(&mut buf, win_off + 0, 5);
        put_u64(&mut buf, win_off + 48, 0);
        buf[win_off + 64] = win_u16.len() as u8;
        buf[win_off + 65] = 1; // ns = WIN32
        for (i, &c) in win_u16.iter().enumerate() {
            put_u16(&mut buf, win_off + 66 + i * 2, c);
        }
        off += win_total;

        put_u32(&mut buf, off, ATTR_END);
        put_u32(&mut buf, off + 4, 0);

        assert!(apply_fixup(&mut buf, 512));
        let entry = parse_record(&buf).expect("parse should succeed");
        // 关键断言：应该选 WIN32 名 "Program Files"，不是 DOS 名 "PROGRA~1"
        assert_eq!(
            entry.name, "Program Files",
            "应该选 WIN32 长名，不是 DOS 8.3 短名 PROGRA~1"
        );
    }
}

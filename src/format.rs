//! 通用格式化/文本测量工具。

use egui::{Color32, FontId};

/// 把一个 UTC FILETIME 转换成"本地时区"的日历时间 (year, month, day, hour, minute)。
///
/// **之前的 bug**：直接把 FILETIME（UTC）当成本地时间拆年月日时分，显示的修改时间跟
/// UTC 差了一个时区偏移（比如东八区会显示成比实际早 8 小时）。
///
/// **为什么不用 `FileTimeToLocalFileTime`**：微软文档明确写了——NTFS 时间戳存的是 UTC，
/// 而 `FileTimeToLocalFileTime` 只按"当前"的夏令时状态换算，不是按"文件时间戳那个日期"
/// 该用的夏令时状态换算。如果查看时和文件时间戳所在的季节夏令时状态不一样（比如冬天
/// 查一个夏天改过的文件），会多算/少算 1 小时。微软文档原话："To account for daylight
/// saving time when converting a file time to a local time, use the following sequence
/// of functions instead of using FileTimeToLocalFileTime: FileTimeToSystemTime +
/// SystemTimeToTzSpecificLocalTime"。
///
/// 这里用的是它的加强版 `SystemTimeToTzSpecificLocalTimeEx`（Windows 7 起支持），
/// 用 `DYNAMIC_TIME_ZONE_INFORMATION` 代替旧版的 `TIME_ZONE_INFORMATION`，能正确处理
/// 跨年份的夏令时规则变化（比如美国 2007 年改过夏令时起止日期），传 `NULL` 就是用系统
/// 当前生效的时区设置查表转换——不用手动调 `GetTimeZoneInformation` 自己算偏移量，
/// 系统改时区/夏令时规则更新后这里自动跟着对。
#[cfg(windows)]
fn filetime_to_local_ymdhm(ft: u64) -> Option<(i64, u32, u32, u64, u64)> {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTimeEx};
    if ft == 0 {
        return None;
    }
    let utc_ft = FILETIME {
        dwLowDateTime: (ft & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ft >> 32) as u32,
    };
    let mut utc_st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    if unsafe { FileTimeToSystemTime(&utc_ft, &mut utc_st) } == 0 {
        return None;
    }
    let mut local_st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    // 第一个参数传 null：用系统当前生效的（动态）时区设置。
    if unsafe { SystemTimeToTzSpecificLocalTimeEx(std::ptr::null(), &utc_st, &mut local_st) } == 0
    {
        return None;
    }
    Some((
        local_st.wYear as i64,
        local_st.wMonth as u32,
        local_st.wDay as u32,
        local_st.wHour as u64,
        local_st.wMinute as u64,
    ))
}

/// 非 Windows 平台（单元测试/开发机）没有系统时区 API 可调。这个 crate 实际运行环境
/// （打包出去的 exe）永远是 Windows，这个分支只影响本地跑 `cargo test` 的行为。
#[cfg(not(windows))]
fn filetime_to_local_ymdhm(_ft: u64) -> Option<(i64, u32, u32, u64, u64)> {
    None
}

/// 把字节数格式化成 "12.34 GB" 这种人类可读的字符串（固定 2 位小数，
/// 跟 Windows 属性对话框的显示精度对齐）。
pub fn human_size(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{:.2} {}", v, units[u])
}

/// 紧凑版本："12.34G" / "345.00M"，用于磁盘行/树列表等空间紧张的地方，
/// 同样固定 2 位小数（跟 `human_size` 精度一致，只是不带空格、单位缩成一个字母）。
pub fn human_size_compact(bytes: u64) -> String {
    let units = ["B", "K", "M", "G", "T"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{}{}", v as u64, units[u])
    } else {
        format!("{:.2}{}", v, units[u])
    }
}

/// 把 Windows FILETIME（1601-01-01 起 100ns 单位）格式化成 "YYYY-MM-DD HH:MM"。
///
/// 不依赖 chrono / time 之类的外部 crate，自己用一份经典的日期算法换算。
/// 输入为 0 视为"未知"，返回空串（UI 上就会显示成 "—"）。
pub fn format_filetime(ft: u64) -> String {
    if ft == 0 {
        return String::new();
    }
    const FILETIME_UNIX_OFFSET_SECS: u64 = 11_644_473_600;
    let unix_100ns = ft / 10_000_000;
    if unix_100ns < FILETIME_UNIX_OFFSET_SECS {
        return String::new();
    }
    let secs = unix_100ns - FILETIME_UNIX_OFFSET_SECS;

    let days = (secs / 86400) as i64;
    let secs_of_day = secs % 86400;
    let hour = secs_of_day / 3600;
    let min = (secs_of_day % 3600) / 60;

    let (year, month, day) = days_to_ymd(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        year, month, day, hour, min
    )
}

/// 把 Windows FILETIME（UTC）格式化成本地时区的 "YYYY-MM-DD HH:MM"。
///
/// UI 显示修改时间应该用这个函数，而不是直接用 `format_filetime`（那个是纯 UTC，
/// 只在测试里验证换算算法本身对不对用）。
///
/// 走 Windows 时区 API 失败时（极少见，比如系统时区数据库损坏），退回纯 UTC 换算，
/// 好过显示空白或崩溃——只是这种情况下时间会跟 UTC 差一个时区，属于降级而不是常态。
///
/// ## 为什么要加缓存（性能）
///
/// 这个函数被列表的三个时间列调用：每个可见行 × 每帧 × 3 列，每次都要调
/// `FileTimeToSystemTime` + `SystemTimeToTzSpecificLocalTimeEx` 两个 Win32 API。
/// 实测一屏 60 行就是每帧 180 次 API、60fps 下约每秒 5400 次系统调用，而且
/// 滚动/空闲时反复换算的都是同一批 FILETIME——纯浪费。这里加一个有界的
/// FILETIME→字符串缓存：同一屏反复重绘零 API 调用，滚动时也只对第一次看到的
/// 时间付一次 API 成本。上限 65536 条（覆盖可见行的反复重绘绰绰有余，内存
/// 约 2~3MB），满了整表清空重来；就算系统中途改了时区/夏令时规则，最多
/// 缓存里已有的旧串多显示一小会儿，清空后自动用新时区。输出与不加缓存时
/// 逐字节一致，对 UI 效果零影响。
pub fn format_filetime_local(ft: u64) -> String {
    if ft == 0 {
        return String::new();
    }
    FILETIME_LOCAL_CACHE.with(|cell| {
        let mut map = cell.borrow_mut();
        if let Some(s) = map.get(&ft) {
            return s.clone();
        }
        let s = match filetime_to_local_ymdhm(ft) {
            Some((year, month, day, hour, min)) => {
                format!("{:04}-{:02}-{:02} {:02}:{:02}", year, month, day, hour, min)
            }
            None => format_filetime(ft),
        };
        if map.len() >= FILETIME_LOCAL_CACHE_CAP {
            map.clear();
        }
        map.insert(ft, s.clone());
        s
    })
}

/// 时区格式化缓存容量（条数）。一屏可见行 × 3 个时间列撑死几百条，滚动
/// 场景条目持续新增，到上限整体清空重采，不会无限制增长。
const FILETIME_LOCAL_CACHE_CAP: usize = 65_536;

thread_local! {
    /// FILETIME → 本地时间字符串的有界缓存。只在 UI 线程用（列表渲染是
    /// 单线程的），thread_local 免锁零竞争。
    static FILETIME_LOCAL_CACHE: std::cell::RefCell<std::collections::HashMap<u64, String>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// 把"自 1970-01-01 起的天数"换算成 (year, month, day)。
/// 算法来自 Howard Hinnant 的 date algorithms（civil_from_days）。
fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

/// 把 Windows 文件属性位格式化成一个紧凑的字母串，类似 Unix 的 `rwx`：
///
/// - `R` ReadOnly         (0x01)
/// - `H` Hidden           (0x02)
/// - `S` System           (0x04)
/// - `D` Directory        (0x10)
/// - `A` Archive          (0x20)
/// - `N` Normal           (0x80)
/// - `T` Temporary        (0x100)
/// - `C` Compressed       (0x800)
/// - `I` NotContentIndexed(0x1000)
/// - `X` Encrypted        (0x4000)
pub fn format_attributes(attrs: u32) -> String {
    // 和 WinDirStat 一致：只显示 R/H/S/A/C（不显示 D/N/T/I/X）
    let mut s = String::with_capacity(8);
    if attrs & 0x01 != 0 { s.push('R'); }
    if attrs & 0x02 != 0 { s.push('H'); }
    if attrs & 0x04 != 0 { s.push('S'); }
    if attrs & 0x20 != 0 { s.push('A'); }
    if attrs & 0x800 != 0 { s.push('C'); }
    if s.is_empty() {
        "—".into()
    } else {
        s
    }
}

/// 按可用宽度截断文字，超出部分用省略号代替。
pub fn truncate_text(ctx: &egui::Context, text: &str, font: FontId, max_width: f32) -> String {
    let measure = |s: &str| -> f32 {
        ctx.fonts_mut(|f| f.layout_no_wrap(s.to_owned(), font.clone(), Color32::WHITE).size().x)
    };
    if measure(text) <= max_width {
        return text.to_owned();
    }
    let mut truncated = String::new();
    for ch in text.chars() {
        let candidate = format!("{truncated}{ch}…");
        if measure(&candidate) > max_width {
            break;
        }
        truncated.push(ch);
    }
    if truncated.is_empty() {
        String::new()
    } else {
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_human_size_bytes() {
        assert_eq!(human_size(0), "0.00 B");
        assert_eq!(human_size(1), "1.00 B");
        assert_eq!(human_size(1023), "1023.00 B");
    }

    #[test]
    fn test_human_size_kb_mb_gb() {
        assert_eq!(human_size(1024), "1.00 KB");
        assert_eq!(human_size(1024 * 1024), "1.00 MB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.00 GB");
        assert_eq!(human_size(1024_u64.pow(4)), "1.00 TB");
    }

    #[test]
    fn test_human_size_compact() {
        assert_eq!(human_size_compact(0), "0B");
        assert_eq!(human_size_compact(1024), "1.00K");
        assert_eq!(human_size_compact(1024 * 1024), "1.00M");
        assert_eq!(human_size_compact(1024 * 1024 * 1024), "1.00G");
        assert_eq!(human_size_compact(1024_u64.pow(4)), "1.00T");
    }

    #[test]
    fn test_format_filetime_zero() {
        assert_eq!(format_filetime(0), "");
    }

    #[test]
    fn test_format_filetime_unix_epoch() {
        let ft = 11_644_473_600u64 * 10_000_000;
        let s = format_filetime(ft);
        assert!(s.starts_with("1970-01-01"), "got: {}", s);
    }

    #[test]
    fn test_format_filetime_known_date() {
        // 2024-01-15 10:30:00 UTC, Unix ts = 1705314600
        let ft = 13_349_788_200u64 * 10_000_000;
        let s = format_filetime(ft);
        assert_eq!(s, "2024-01-15 10:30");
    }

    #[test]
    fn test_format_attributes_empty() {
        assert_eq!(format_attributes(0), "—");
    }

    #[test]
    fn test_format_attributes_normal() {
        // NORMAL=0x80 不再显示（和 WinDirStat 一致）
        assert_eq!(format_attributes(0x80), "—");
    }

    #[test]
    fn test_format_attributes_directory() {
        // DIRECTORY=0x10 不再显示（和 WinDirStat 一致）
        assert_eq!(format_attributes(0x10), "—");
    }

    #[test]
    fn test_format_attributes_system_archive() {
        let s = format_attributes(0x24);
        assert!(s.contains('S'), "should contain S: {}", s);
        assert!(s.contains('A'), "should contain A: {}", s);
        assert!(!s.contains('D'), "should not contain D: {}", s);
    }

    #[test]
    fn test_format_attributes_hidden_system_directory() {
        let s = format_attributes(0x16);
        assert!(s.contains('H'));
        assert!(s.contains('S'));
        // D 不再显示
    }
}

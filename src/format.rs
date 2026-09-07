
use egui::{Color32, FontId};

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

#[cfg(not(windows))]
fn filetime_to_local_ymdhm(_ft: u64) -> Option<(i64, u32, u32, u64, u64)> {
    None
}

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

const FILETIME_LOCAL_CACHE_CAP: usize = 65_536;

thread_local! {
    static FILETIME_LOCAL_CACHE: std::cell::RefCell<std::collections::HashMap<u64, String>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

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

pub fn format_attributes(attrs: u32) -> String {
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
        assert_eq!(format_attributes(0x80), "—");
    }

    #[test]
    fn test_format_attributes_directory() {
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
    }
}


use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use log::{LevelFilter, Log, Metadata, Record};

static LOG_FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();

static LOG_BYTES_WRITTEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

const OWN_CRATE_TARGET_PREFIX: &str = "diskforge";

struct DualLogger;

impl Log for DualLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.target().starts_with(OWN_CRATE_TARGET_PREFIX)
    }
    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        write_line(&format!("[{}] {}", record.level(), record.args()));
    }
    fn flush(&self) {
        if let Some(lock) = LOG_FILE.get()
            && let Ok(mut guard) = lock.lock()
                && let Some(f) = guard.as_mut() {
                    let _ = f.flush();
                }
    }
}

fn timestamp() -> String {
    chrono::Local::now().format("%H:%M:%S%.3f").to_string()
}

pub fn today_date_string() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn write_line(msg: &str) {
    let line = format!("[{}] {msg}", timestamp());
    eprintln!("{line}");
    if let Some(lock) = LOG_FILE.get()
        && let Ok(mut guard) = lock.lock() {
            use std::sync::atomic::Ordering;
            let next = LOG_BYTES_WRITTEN.fetch_add(line.len() as u64 + 1, Ordering::Relaxed)
                + line.len() as u64
                + 1;
            if next >= MAX_LOG_BYTES
                && let Ok(new_f) = OpenOptions::new().create(true).write(true).truncate(true).open(log_path())
            {
                *guard = Some(new_f);
                LOG_BYTES_WRITTEN.store(0, Ordering::Relaxed);
                eprintln!("[applog] 日志已超过 5MB，重新开始（运行时轮转）");
            }
            if let Some(f) = guard.as_mut() {
                let _ = writeln!(f, "{line}");
                let _ = f.flush();
            }
        }
}

pub fn log_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("diskforge_log.txt")))
        .unwrap_or_else(|| {
            std::env::temp_dir().join("diskforge_log.txt")
        })
}

/// 禁用控制台"快速编辑"（QuickEdit）模式：Windows 控制台默认开启该模式，
/// 鼠标在控制台里一点一划就进入文本选择状态，此时所有向控制台的输出都被
/// 阻塞，整个程序跟着卡死（按 Esc/回车才恢复）。本程序控制台只是诊断输出，
/// 不值得为误触选择卡住 GUI，启动时直接关掉。
/// 注意：ENABLE_EXTENDED_FLAGS 必须与 QUICK_EDIT 位一起设置才生效（MSDN 要求）。
#[cfg(windows)]
fn disable_console_quickedit() {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_EXTENDED_FLAGS, ENABLE_QUICK_EDIT_MODE,
        STD_INPUT_HANDLE,
    };
    unsafe {
        let stdin = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode: u32 = 0;
        // 没有控制台（GUI 子系统启动/输出重定向）时 GetConsoleMode 返回 0，自然跳过
        if !stdin.is_null() && GetConsoleMode(stdin, &mut mode) != 0 {
            let new_mode = (mode & !ENABLE_QUICK_EDIT_MODE) | ENABLE_EXTENDED_FLAGS;
            if SetConsoleMode(stdin, new_mode) != 0 && mode & ENABLE_QUICK_EDIT_MODE != 0 {
                eprintln!("[applog] 已禁用控制台快速编辑模式（防止鼠标误选卡住程序）");
            }
        }
    }
}

pub fn init() {
    #[cfg(windows)]
    disable_console_quickedit();
    let path = log_path();
    let existing_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let truncate = existing_len >= MAX_LOG_BYTES;
    let file = OpenOptions::new().create(true).append(!truncate).write(truncate).truncate(truncate).open(&path).ok();
    std::sync::atomic::AtomicU64::store(
        &LOG_BYTES_WRITTEN,
        if truncate { 0 } else { existing_len },
        std::sync::atomic::Ordering::Relaxed,
    );
    match &file {
        Some(_) => eprintln!("[applog] 诊断日志会写到: {}{}", path.display(), if truncate { "（已超过 5MB，重新开始）" } else { "" }),
        None => eprintln!("[applog] 打开日志文件失败（仅控制台可见诊断信息）: {}", path.display()),
    }
    let _ = LOG_FILE.set(Mutex::new(file));

    if log::set_boxed_logger(Box::new(DualLogger)).is_ok() {
        log::set_max_level(LevelFilter::Trace);
    } else {
        eprintln!("[applog] log::set_boxed_logger 失败（可能已经被别的代码设置过），标准 log::xxx! 宏不会写进日志文件，但 applog::log()/dlog! 不受影响。");
    }

    log(&format!(
        "==== {} {} 启动 (unix_ts={}) ====",
        crate::about::APP_NAME,
        crate::about::APP_VERSION,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    ));
}

pub fn log(msg: &str) {
    write_line(msg);
}

#[allow(dead_code)]
pub fn log_batch(lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    let ts = timestamp();
    if let Some(lock) = LOG_FILE.get()
        && let Ok(mut guard) = lock.lock()
            && let Some(f) = guard.as_mut() {
                for line in lines {
                    let _ = writeln!(f, "[{ts}] {line}");
                }
                let _ = f.flush();
            }
    eprintln!("[{ts}] (批量写入 {} 行到日志文件，控制台不逐行显示)", lines.len());
}

#[macro_export]
macro_rules! dlog {
    ($($arg:tt)*) => {
        $crate::applog::log(&format!($($arg)*))
    };
}

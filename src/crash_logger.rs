
//! 系统级崩溃日志。
//!
//! `std::panic::set_hook`（见 main.rs 的 install_panic_logger）只能捕获 Rust 的
//! panic（栈展开机制）；而 0xC0000005 访问越界这类 Windows SEH 结构化异常会完全
//! 跳过 panic 机制、由操作系统直接终止进程，导致崩溃时日志戛然而止、原因无从知晓。
//!
//! 这里用 SetUnhandledExceptionFilter 注册最后一道兜底（游戏引擎、浏览器的崩溃
//! 上报用的就是同一套机制）：进程被系统杀死之前，把异常码、发生地址、线程 ID
//! 写进诊断日志并强制 flush 落盘。

#[cfg(windows)]
mod imp {
    use std::fs::OpenOptions;
    use std::io::Write;

    use windows_sys::Win32::Diagnostics::Debug::{
        SetUnhandledExceptionFilter, EXCEPTION_POINTERS, LPTOP_LEVEL_EXCEPTION_FILTER,
    };
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;

    /// 常见 SEH 异常码 -> 面向用户的中文说明
    fn describe(code: u32) -> &'static str {
        match code {
            0xC0000005 => "访问越界（读/写了无效的内存地址）",
            0xC00000FD => "栈溢出（递归过深或单帧局部变量过大）",
            0xC0000094 => "整数除零",
            0xC0000096 => "执行了特权指令",
            0xC0000374 => "堆损坏（内存被重复释放或越界写破坏）",
            0xC0000409 => "栈缓冲区溢出保护触发（安全检查失败）",
            0xC0000135 => "找不到依赖的 DLL",
            0xC0000142 => "DLL 初始化失败",
            0xE06D7363 => "C++ 异常（throw）",
            _ => "未知异常类型",
        }
    }

    /// 崩溃处理器里不能加锁（applog 的日志锁可能正被崩溃的线程持有，会死锁），
    /// 也不能依赖复杂分配（堆可能已损坏），所以这里独立打开日志文件追加写入，
    /// 尽力而为；任何一步失败都静默放弃，绝不在处理器里再炸出第二个异常。
    fn write_crash_log(line: &str) {
        match OpenOptions::new().create(true).append(true).open(crate::applog::log_path()) {
            Ok(mut f) => {
                let _ = writeln!(f, "{line}");
                // 进程几毫秒后就会被系统杀死，必须立刻落盘，不等缓冲区
                let _ = f.flush();
            }
            Err(_) => eprintln!("{line}"),
        }
    }

    /// 返回 EXCEPTION_EXECUTE_HANDLER(1)：写完日志后让进程按默认方式终止。
    unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
        if info.is_null() || (*info).ExceptionRecord.is_null() {
            write_crash_log("==== CRASH 发生系统级异常，但异常信息不可读 ====");
            return 1;
        }
        let rec = &*(*info).ExceptionRecord;
        let code = rec.ExceptionCode as u32;
        let addr = rec.ExceptionAddress as usize;
        let tid = GetCurrentThreadId();
        let mut line = format!(
            "==== CRASH [线程 ID {tid}] 异常 0x{code:08X}（{}），发生地址 0x{addr:016X}",
            describe(code)
        );
        // ACCESS_VIOLATION 额外给出"读还是写、目标地址"（EXCEPTION_RECORD 参数约定）
        if code == 0xC0000005 && rec.NumberParameters >= 2 {
            let mode = match rec.ExceptionInformation[0] {
                0 => "读取",
                1 => "写入",
                8 => "执行（DEP 数据执行保护拦截）",
                _ => "访问",
            };
            line.push_str(&format!(
                "，试图{} 0x{:016X}",
                mode, rec.ExceptionInformation[1]
            ));
        }
        line.push_str(" ====");
        write_crash_log(&line);
        1
    }

    pub fn install() {
        unsafe {
            SetUnhandledExceptionFilter(Some(filter as LPTOP_LEVEL_EXCEPTION_FILTER));
        }
        crate::applog::log(
            "[main] 系统级崩溃日志已安装（panic 之外的 SEH 异常也会写入 diskforge_log.txt）",
        );
    }
}

#[cfg(windows)]
pub fn install() {
    imp::install();
}

#[cfg(not(windows))]
pub fn install() {}

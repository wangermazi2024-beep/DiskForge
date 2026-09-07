//! DiskForge 库入口（NTFS 磁盘空间分析器）。
//!
//! 纯逻辑模块作为 lib target 暴露，让验证工具和单元测试能在无 eframe 环境下编译。

pub mod about;
pub mod applog;
pub mod categorize;
pub mod dedup;
pub mod dir_enum;
pub mod disk_info;
pub mod export;
pub mod file_ops;
pub mod format;
pub mod fs_attrs;
pub mod model;
#[cfg(windows)]
pub mod mft_scan;
pub mod scan;
pub mod search_index;
pub mod theme;

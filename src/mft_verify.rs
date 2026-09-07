//! 抽测校验：MFT 直读法拿到的数据是否和"传统 API 挨个查"一致。
//!
//! 做两件事：
//! 1. 从扫描结果里随机抽 N 个文件，用 `std::fs::metadata` 重新查一次真实大小，
//!    和 MFT 记录里解析出的大小做比对。
//! 2. 用 `GetDiskFreeSpaceExW` 拿系统报告的总容量/已用空间，和扫描结果汇总的
//!    大小做量级对比（不要求严格相等，原因见 `mft_scan.rs` 里的注释）。
//!
//! 只在 Windows 上编译，因为它依赖 `mft_scan` 模块。

#![cfg(windows)]

use rand::seq::SliceRandom;
use std::path::PathBuf;

use crate::mft_scan::get_disk_space;

pub struct SpotCheckReport {
    pub sample_count: usize,
    pub mismatches: Vec<(PathBuf, u64 /* mft size */, u64 /* fs size */)>,
    pub unreadable: usize,
    pub disk_total: Option<u64>,
    pub disk_used: Option<u64>,
    pub scanned_total: u64,
}

impl SpotCheckReport {
    pub fn all_matched(&self) -> bool {
        self.mismatches.is_empty()
    }
}

/// `result`：一次 `scan_drive_via_mft` 的返回值。
/// `mft_sizes`：与 `result.file_paths` 一一对应的、从 MFT 记录里解析出的大小。
///   （由调用方在遍历 file_paths 时顺带收集，这里直接接收已经配好对的列表，
///    避免重新走一遍树。）
pub fn spot_check(
    drive_letter: char,
    file_paths: &[PathBuf],
    mft_sizes: &[u64],
    scanned_total: u64,
    sample_count: usize,
) -> SpotCheckReport {
    let mut rng = rand::thread_rng();
    let mut indices: Vec<usize> = (0..file_paths.len()).collect();
    indices.shuffle(&mut rng);
    indices.truncate(sample_count.min(indices.len()));

    let mut mismatches = Vec::new();
    let mut unreadable = 0usize;

    for &i in &indices {
        let path = &file_paths[i];
        let mft_size = mft_sizes[i];
        match std::fs::metadata(path) {
            Ok(meta) => {
                let fs_size = meta.len();
                if fs_size != mft_size {
                    mismatches.push((path.clone(), mft_size, fs_size));
                }
            }
            Err(_) => {
                // 常见于系统正在使用中的文件路径重命名竞态、权限拒绝等，属于预期内噪声。
                unreadable += 1;
            }
        }
    }

    let (disk_total, disk_used) = match get_disk_space(drive_letter) {
        Some((total, free)) => (Some(total), Some(total.saturating_sub(free))),
        None => (None, None),
    };

    SpotCheckReport {
        sample_count: indices.len(),
        mismatches,
        unreadable,
        disk_total,
        disk_used,
        scanned_total,
    }
}

/// 把校验报告格式化成人类可读的中文摘要，方便直接显示在 UI 或打日志。
pub fn format_report(r: &SpotCheckReport) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "抽测 {} 个文件：{} 个大小一致，{} 个不一致，{} 个当时无法用传统 API 读取（竞态/权限，视为噪声）。\n",
        r.sample_count,
        r.sample_count - r.mismatches.len() - r.unreadable,
        r.mismatches.len(),
        r.unreadable
    ));
    for (p, mft, fs) in r.mismatches.iter().take(10) {
        s.push_str(&format!("  不一致: {} | MFT={mft} FS={fs}\n", p.display()));
    }
    if let (Some(total), Some(used)) = (r.disk_total, r.disk_used) {
        s.push_str(&format!(
            "系统报告总容量 {:.2} GB，已用 {:.2} GB；扫描汇总（文件逻辑大小之和）{:.2} GB。\n",
            total as f64 / 1e9,
            used as f64 / 1e9,
            r.scanned_total as f64 / 1e9,
        ));
        s.push_str(
            "注：扫描汇总天然会略小于“已用空间”，因为不含簇内部碎片、NTFS 元数据、卷影副本等，这是预期差异。\n",
        );
    }
    s
}

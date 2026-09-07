//! CSV 导出 — 和列表列完全一致。
//!
//! 列顺序：路径 | 名称 | 父占比 | 总占比 | 逻辑大小 | 修改时间 | 物理大小 | 创建时间 | 访问时间
//!         | 项目 | 文件 | 文件夹 | 属性 | 重解析点 | 保留 | 所有者
//!
//! ## 数据源：名字索引快照（不是树）
//!
//! 以前直接在 UI 线程上同步遍历 `self.partitions` 写 CSV——百万行的盘会把
//! 界面冻结几分钟。现在从 `Arc<NameIndex>` 快照导出：索引自含渲染/导出要的
//! 全部字段（名字/大小/时间/属性/所有者/所在目录/计数），先序顺序与树一致，
//! 且 `Arc` 跨线程读绝对安全——整个导出跑在后台线程，UI 全程零卡顿，导出的
//! 是"点击导出那一刻"的快照（与搜索/复制列表标签页同一套快照语义）。
//!
//! ## 另外两件这次一起修的事
//!
//! 1. **BufWriter**：以前 `File` 裸写，每行 `writeln!` 都是一轮系统调用，
//!    百万行就是百万次 write syscall；256KB 缓冲后缩减约四个数量级。
//! 2. **CSV 公式注入防御**（OWASP CSV Injection Cheat Sheet 的标准做法）：
//!    文件名/路径/所有者是从文件系统来的"不可信输入"，以 `=`/`+`/`-`/`@`
//!    开头的单元格在 Excel 里会被当公式执行（DDE 下载器攻击的真实载体）。
//!    检测到这类开头就在字段前加一个单引号——Excel/LibreOffice 把单引号当
//!    "文本标记"，显示时自动隐藏，数据保持可读。

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use crate::format::{format_attributes, format_filetime_local as format_filetime};
use crate::search_index::NameIndex;

/// 导出进度回调：`(分区名, 已写行数)`。后台线程按行数节流调用。
pub type ExportProgress<'a> = dyn Fn(&str, u64) + 'a;

/// CSV 字段转义：需要时加引号 + 内部引号翻倍 + 公式注入防御（见模块顶部说明）。
pub fn escape_csv_field(s: &str) -> String {
    let needs_quotes = s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r');
    // OWASP：= + - @ 制表符 回车 开头的单元格都可能被表格软件当公式/控制解析。
    let dangerous = matches!(
        s.chars().next(),
        Some('=') | Some('+') | Some('-') | Some('@') | Some('\t') | Some('\r')
    );
    let body = if dangerous { format!("'{s}") } else { s.to_string() };
    if needs_quotes {
        format!("\"{}\"", body.replace('"', "\"\""))
    } else {
        body
    }
}

/// 从名字索引快照导出一个分区到 CSV。返回 (文件数, 文件夹数)。
///
/// `progress` 每写 [`PROGRESS_ROWS`] 行回调一次；写坏不了调用方的东西，
/// 闭包里通常只是往 channel 发个消息。
pub fn export_index_csv(
    index: &NameIndex,
    pi: usize,
    root_path: &str,
    out_path: &Path,
    partition_name: &str,
    progress: &ExportProgress<'_>,
) -> io::Result<(u64, u64)> {
    const PROGRESS_ROWS: u64 = 4096;

    let f = File::create(out_path)?;
    let mut w = BufWriter::with_capacity(256 * 1024, f);
    w.write_all(&[0xEF, 0xBB, 0xBF])?; // UTF-8 BOM（Excel 打开中文不乱码）
    writeln!(w, "路径,名称,父占比(%),总占比(%),逻辑大小,修改时间,物理大小,创建时间,访问时间,项目,文件,文件夹,属性,重解析点,保留,所有者")?;

    let mut file_count = 0u64;
    let mut folder_count = 0u64;
    let mut rows_written = 0u64;
    let disk_logical = index.root_logical(pi).max(1);

    // 根行（与旧版 export_tree_csv 的根行语义一致：父占比/总占比都是 100%）。
    {
        let root = index.root_info(pi);
        let kind = "文件夹";
        let _ = kind;
        write_row(
            &mut w, root_path, &root.name, 1.0, 1.0,
            root.logical_size, root.physical_size,
            root.modified_ft, root.created_ft, root.accessed_ft,
            root.file_count + root.folder_count, root.file_count, root.folder_count,
            root.attributes, root.reparse_tag, root.is_reserved, &root.owner,
        )?;
        rows_written += 1;
    }

    // 条目行：索引先序与树先序一致，`e.pi == pi` 过滤出本分区
    // （主索引是多分区合一的，导出按分区各出一个文件，与旧行为一致）。
    for i in 0..index.len() {
        let e = index.entry(i);
        if e.pi as usize != pi {
            continue;
        }
        let name = index.name_orig(i);
        let dir = index.dir_path(i);
        // 与目录池的拼法保持一致：池里存的是去尾反斜杠形态（C:\Windows），
        // 路径 = 所在目录 + \ + 名字；目录池缺失（理论不可达，主索引恒建池）
        // 时退化为纯名字，绝不出 "\name" 这种开头带反斜杠的怪路径。
        let path = if dir.is_empty() {
            name.to_string()
        } else {
            format!("{dir}\\{name}")
        };
        let (files, folders) = if e.is_file() { (0, 0) } else { (e.file_count as u64, e.folder_count as u64) };
        let items = if e.is_file() { 0 } else { e.file_count as u64 + e.folder_count as u64 };
        let parent_pct = e.logical_size as f64 / e.parent_logical.max(1) as f64;
        let total_pct = e.logical_size as f64 / disk_logical as f64;
        write_row(
            &mut w, &path, name, parent_pct, total_pct,
            e.logical_size, e.physical_size,
            e.modified_ft, e.created_ft, e.accessed_ft,
            items, files, folders,
            e.attributes, e.reparse_tag, e.is_reserved(), index.owner(i),
        )?;
        if e.is_file() {
            file_count += 1;
        } else {
            folder_count += 1;
        }
        rows_written += 1;
        if rows_written.is_multiple_of(PROGRESS_ROWS) {
            progress(partition_name, rows_written);
        }
    }
    w.flush()?;
    Ok((file_count, folder_count))
}

/// 写一行 CSV。字段转义（含公式注入防御）统一走 [`escape_csv_field`]。
#[allow(clippy::too_many_arguments)]
fn write_row(
    w: &mut BufWriter<File>,
    path: &str, name: &str,
    parent_pct: f64, total_pct: f64,
    logical: u64, physical: u64,
    modified_ft: u64, created_ft: u64, accessed_ft: u64,
    items: u64, files: u64, folders: u64,
    attributes: u32, reparse_tag: u32, is_reserved: bool,
    owner: &str,
) -> io::Result<()> {
    let modified = if modified_ft > 0 { format_filetime(modified_ft) } else { String::new() };
    let created = if created_ft > 0 { format_filetime(created_ft) } else { String::new() };
    let accessed = if accessed_ft > 0 { format_filetime(accessed_ft) } else { String::new() };
    let reparse = if reparse_tag != 0 { format!("0x{reparse_tag:X}") } else { String::new() };
    let reserved = if is_reserved { "是" } else { "" };
    let attrs = format_attributes(attributes);

    writeln!(w, "{},{},{:.2},{:.2},{},{},{},{},{},{},{},{},{},{},{},{}",
        escape_csv_field(path), escape_csv_field(name),
        parent_pct * 100.0, total_pct * 100.0,
        logical, escape_csv_field(&modified),
        physical, escape_csv_field(&created), escape_csv_field(&accessed),
        items, files, folders,
        escape_csv_field(&attrs), escape_csv_field(&reparse), escape_csv_field(reserved), escape_csv_field(owner),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_csv_field_basic() {
        // 普通字段原样。
        assert_eq!(escape_csv_field("hello.txt"), "hello.txt");
        // 含逗号 → 加引号。
        assert_eq!(escape_csv_field("a,b"), "\"a,b\"");
        // 含引号 → 加引号 + 引号翻倍。
        assert_eq!(escape_csv_field("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn test_escape_csv_field_formula_injection() {
        // OWASP：以 = + - @ 开头的字段必须加前缀防公式执行。
        assert_eq!(escape_csv_field("=cmd|'/c calc'!A0"), "'=cmd|'/c calc'!A0");
        assert_eq!(escape_csv_field("+1+1"), "'+1+1");
        assert_eq!(escape_csv_field("-2+3"), "'-2+3");
        assert_eq!(escape_csv_field("@SUM(1)"), "'@SUM(1)");
        // 负数百分比这类"程序自己生成的数字字段"不走这里（格式化后直写），
        // 但万一走到也要安全。
        assert_eq!(escape_csv_field("-1.23"), "'-1.23");
    }
}


use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use crate::format::{format_attributes, format_filetime_local as format_filetime};
use crate::search_index::NameIndex;

pub type ExportProgress<'a> = dyn Fn(&str, u64) + 'a;

pub fn escape_csv_field(s: &str) -> String {
    let needs_quotes = s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r');
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
    w.write_all(&[0xEF, 0xBB, 0xBF])?;
    writeln!(w, "路径,名称,父占比(%),总占比(%),逻辑大小,修改时间,物理大小,创建时间,访问时间,项目,文件,文件夹,属性,重解析点,保留,所有者")?;

    let mut file_count = 0u64;
    let mut folder_count = 0u64;
    let mut rows_written = 0u64;
    let disk_logical = index.root_logical(pi).max(1);

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

    for i in 0..index.len() {
        let e = index.entry(i);
        if e.pi as usize != pi {
            continue;
        }
        let name = index.name_orig(i);
        let dir = index.dir_path(i);
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
        assert_eq!(escape_csv_field("hello.txt"), "hello.txt");
        assert_eq!(escape_csv_field("a,b"), "\"a,b\"");
        assert_eq!(escape_csv_field("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn test_escape_csv_field_formula_injection() {
        assert_eq!(escape_csv_field("=cmd|'/c calc'!A0"), "'=cmd|'/c calc'!A0");
        assert_eq!(escape_csv_field("+1+1"), "'+1+1");
        assert_eq!(escape_csv_field("-2+3"), "'-2+3");
        assert_eq!(escape_csv_field("@SUM(1)"), "'@SUM(1)");
        assert_eq!(escape_csv_field("-1.23"), "'-1.23");
    }
}

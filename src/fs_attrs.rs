//! Windows 文件属性位 / reparse tag 常量的唯一定义点。
//!
//! 以前这些常量在 `mft_scan.rs`/`scan.rs`/`dir_enum.rs`/`file_ops.rs`/
//! `tree_list.rs`/`model.rs` 里各自本地重定义了 14 处——同一个 `0x400`
//! 散落十几个文件，改一处漏一处就是隐性 bug 温床。现在统一在这里定义，
//! 值与 Win32 SDK 头文件（winnt.h）逐一对齐；`windows-sys` 里也有同名
//! 常量，但它绑定在 feature 门控的模块树里，部分绑定版本没有导出全，
//! 为了"所有引用点用同一个来源"，项目内统一 use 本模块。

/// 只读（READONLY）。
pub const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
/// 隐藏（HIDDEN）。
pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
/// 系统（SYSTEM）。
pub const FILE_ATTRIBUTE_SYSTEM: u32 = 0x0000_0004;
/// 目录（DIRECTORY）。
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
/// 归档（ARCHIVE）。
pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;
/// 普通（NORMAL）。
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
/// 临时（TEMPORARY）。
pub const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x0000_0100;
/// 稀疏文件（SPARSE_FILE）。
pub const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x0000_0200;
/// 重解析点（REPARSE_POINT）——符号链接/junction/OneDrive 占位等的公共位。
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
/// 压缩（COMPRESSED）。
pub const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x0000_0800;
/// 非内容索引（NOT_CONTENT_INDEXED）。
pub const FILE_ATTRIBUTE_NOT_CONTENT_INDEXED: u32 = 0x0000_2000;
/// 加密（ENCRYPTED）。
pub const FILE_ATTRIBUTE_ENCRYPTED: u32 = 0x0000_4000;

/// 符号链接的 reparse tag（IO_REPARSE_TAG_SYMLINK）。
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
/// 目录联接的 reparse tag（IO_REPARSE_TAG_MOUNT_POINT，即 junction）。
pub const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
/// Windows 压缩占位（WOFCompressed，系统组件压缩文件用的 tag）。
pub const IO_REPARSE_TAG_WOF: u32 = 0x8000_0017;

//! 构建脚本：把应用图标与版本/版权信息嵌入 exe 的 Windows 资源段。
//!
//! 嵌入后效果：
//! - 资源管理器里 DiskForge.exe 显示定制图标（assets/icon.ico，多尺寸）；
//! - 右键 → 属性 → 详细信息，可见产品名/版本/版权/作者邮箱（文件属性版权信息）。
//!
//! 兼容性说明：资源编译器在 Windows 上优先用 MSVC 的 `rc.exe`，GNU 工具链用
//! `windres`。在两者都不可用的环境（例如 Linux 上做 cargo check/clippy 交叉
//! 检查）里，这里**降级为警告并继续**——绝不让构建脚本失败阻塞编译，只是该
//! 环境产出的二进制没有内嵌资源（正式发布在 Windows 侧编译，资源完整）。
//!
//! 版本号唯一来源是 Cargo.toml 的 `version`（通过 CARGO_PKG_VERSION 环境变量
//! 注入），与 src/about.rs 里 `APP_VERSION` 显示保持同源，避免两处各改各的。

fn main() {
    // 只在目标平台是 Windows 时嵌入资源（交叉检查到非 Windows 目标时跳过）。
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    // FileVersion/ProductVersion 的四段数字版本：0.1.0 → 0.1.0.0。
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.1.0".to_string());
    let numeric = format!("{version}.0");
    res.set("FileVersion", &numeric);
    res.set("ProductVersion", &numeric);
    res.set("ProductName", "DiskForge");
    res.set("FileDescription", "DiskForge - Free NTFS disk space & file analyzer");
    res.set("LegalCopyright", "Copyright (C) 2026 WMS. All rights reserved.");
    res.set("CompanyName", "WMS");
    res.set("LegalTrademarks", "DiskForge");
    res.set("OriginalFilename", "DiskForge.exe");
    res.set(
        "Comments",
        "Free software released under the MIT License. Author: WMS <wumingshifn@gmail.com>",
    );
    // 界面语言是简体中文，资源里标记为中文（0x0804）让资源管理器属性页正确归类。
    res.set_language(0x0804);

    if let Err(err) = res.compile() {
        println!(
            "cargo:warning=DiskForge: 未嵌入 exe 资源（图标/版本信息）：{err}。\
正式发布请在 Windows 侧编译（需要 rc.exe 或 windres）；本次构建继续。"
        );
    }
}

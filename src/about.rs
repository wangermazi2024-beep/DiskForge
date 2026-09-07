//! 产品身份信息（名称/版本/版权/许可声明）与"关于"悬浮窗的持久化。
//!
//! 这里是全项目**唯一**的版本号/版权文案定义点：
//! - 窗口标题、底部品牌条、"关于"悬浮窗、导出文件头等显示处一律引用本模块常量；
//! - exe 文件属性里的版本资源（build.rs）与这里的文案保持同一套，改动时两处同步。

/// 产品名。
pub const APP_NAME: &str = "DiskForge";

/// 版本号（Cargo.toml 的 `version` 是唯一定义点，这里自动拼上前缀；测试版标识后缀）。
pub const APP_VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"), " Beta");

/// 开发作者。
pub const APP_AUTHOR: &str = "WMS";

/// 联系邮箱。
pub const APP_EMAIL: &str = "wumingshifn@gmail.com";

/// 版权行（界面显示用）。
pub const COPYRIGHT_LINE: &str = "版权所有 (C) 2026 WMS";

/// 标准软件许可声明（MIT 许可证核心条款的中文表述），"关于"悬浮窗与 README 共用。
pub const LICENSE_NOTICE: &str = "本软件是免费软件，依据 MIT 许可证授权发布：\
您可以自由地使用、复制、修改及分发本软件，但须保留上述版权声明与许可声明。\
本软件按“现状”提供，不附带任何明示或默示的担保——包括但不限于对适销性\
和特定用途适用性的担保。无论何种情况，作者或版权持有人均不对因使用本软件\
或本软件的其他交易而产生的任何索赔、损害或其他责任承担责任。";

/// "关于"悬浮窗里赞助区的提示语。
pub const SPONSOR_HINT: &str = "如果这个软件帮到了你，欢迎扫码请作者喝杯咖啡（金额随意，不强求）";

// ── "不再提醒"的持久化 ─────────────────────────────────────────
// 用 %APPDATA%\DiskForge\ 下的一个标记文件（注册表对这类"界面偏好"太重，
// 临时目录又会被系统清理；标记文件透明、可手动删除重置，重启软件后依然生效）。

const SPONSOR_FLAG_FILE: &str = "sponsor_suppressed.flag";
const DATA_DIR_NAME: &str = "DiskForge";

/// 持久化目录：优先 `%APPDATA%\DiskForge`；拿不到 APPDATA（理论只在非
/// Windows 调试环境发生）返回 `None`，所有读写安全降级为"不持久化"。
fn data_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("APPDATA")
        .map(|base| std::path::PathBuf::from(base).join(DATA_DIR_NAME))
}

/// 用户是否已经点过"不再提醒"（首启弹赞助悬浮窗前检查；"关于"菜单入口不受它影响）。
pub fn is_sponsor_suppressed() -> bool {
    data_dir()
        .map(|dir| dir.join(SPONSOR_FLAG_FILE).exists())
        .unwrap_or(false)
}

/// 用户点了"不再提醒"：写入标记文件，之后每次启动都不再自动弹赞助悬浮窗。
/// （从"关于"菜单打开不受此影响——用户主动查看时要始终能打开。）
pub fn set_sponsor_suppressed() {
    let Some(dir) = data_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let _ = std::fs::write(
        dir.join(SPONSOR_FLAG_FILE),
        format!(
            "{APP_NAME} {APP_VERSION} 赞助提示已选择\"不再提醒\"。\n写入时间: {stamp}\n删除本文件即可恢复首次启动提示。\n"
        ),
    );
}

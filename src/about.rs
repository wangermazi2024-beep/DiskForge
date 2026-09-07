
pub const APP_NAME: &str = "DiskForge";

pub const APP_VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"), " Beta");

pub const APP_AUTHOR: &str = "WMS";

pub const APP_EMAIL: &str = "wumingshifn@gmail.com";

pub const COPYRIGHT_LINE: &str = "版权所有 (C) 2026 WMS";

pub const LICENSE_NOTICE: &str = "本软件是免费软件，依据 MIT 许可证授权发布：\
您可以自由地使用、复制、修改及分发本软件，但须保留上述版权声明与许可声明。\
本软件按“现状”提供，不附带任何明示或默示的担保——包括但不限于对适销性\
和特定用途适用性的担保。无论何种情况，作者或版权持有人均不对因使用本软件\
或本软件的其他交易而产生的任何索赔、损害或其他责任承担责任。";

pub const SPONSOR_HINT: &str = "如果这个软件帮到了你，欢迎扫码请作者喝杯咖啡（金额随意，不强求）";


const SPONSOR_FLAG_FILE: &str = "sponsor_suppressed.flag";
const DATA_DIR_NAME: &str = "DiskForge";

fn data_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("APPDATA")
        .map(|base| std::path::PathBuf::from(base).join(DATA_DIR_NAME))
}

pub fn is_sponsor_suppressed() -> bool {
    data_dir()
        .map(|dir| dir.join(SPONSOR_FLAG_FILE).exists())
        .unwrap_or(false)
}

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

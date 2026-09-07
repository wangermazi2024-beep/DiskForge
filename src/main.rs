
pub use diskforge::{about, applog, categorize, crash_logger, dedup, disk_info, export, file_ops, format, fs_attrs, model, scan, search_index, theme};
#[cfg(windows)]
pub use diskforge::mft_scan;

mod app;
mod ui;
use app::{AboutTextures, DiskForgeApp};

fn load_png_texture(ctx: &egui::Context, name: &str, bytes: &[u8]) -> egui::TextureHandle {
    let img = image::load_from_memory(bytes)
        .unwrap_or_else(|e| panic!("内置资源 {name} 解码失败（构建产物损坏？）: {e}"))
        .to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    ctx.load_texture(
        name,
        egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()),
        egui::TextureOptions::LINEAR,
    )
}

fn setup_about_textures(ctx: &egui::Context) -> AboutTextures {
    AboutTextures {
        logo: load_png_texture(ctx, "about_logo", include_bytes!("../assets/icon.png")),
        wechat: load_png_texture(ctx, "sponsor_wechat", include_bytes!("../assets/sponsor_wechat.png")),
        alipay: load_png_texture(ctx, "sponsor_alipay", include_bytes!("../assets/sponsor_alipay.png")),
    }
}

fn viewport_icon() -> egui::IconData {
    let img = image::load_from_memory(include_bytes!("../assets/icon.png"))
        .expect("内置图标解码失败（构建产物损坏？）")
        .to_rgba8();
    egui::IconData {
        width: img.width(),
        height: img.height(),
        rgba: img.into_raw(),
    }
}

fn setup_fonts(ctx: &egui::Context) {
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    let dynamic_candidates = [
        format!(r"{system_root}\Fonts\msyh.ttc"),
        format!(r"{system_root}\Fonts\msyh.ttf"),
        format!(r"{system_root}\Fonts\simhei.ttf"),
        format!(r"{system_root}\Fonts\simsun.ttc"),
    ];
    const STATIC_CANDIDATES: &[&str] = &[
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/System/Library/Fonts/PingFang.ttc",
    ];
    let found = dynamic_candidates.iter().map(|s| s.as_str())
        .chain(STATIC_CANDIDATES.iter().copied())
        .find_map(|p| std::fs::read(p).ok().map(|data| (p, data)));
    let Some((path, data)) = found else {
        applog::log(&format!("[main] 未找到可用的中文字体，界面中文可能显示为方块（已尝试: {:?} + Linux/macOS 候选路径）", dynamic_candidates));
        return;
    };
    applog::log(&format!("[main] 使用中文字体: {path}"));
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("cjk".to_owned(), egui::FontData::from_owned(data).into());
    fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "cjk".to_owned());
    fonts.families.entry(egui::FontFamily::Monospace).or_default().push("cjk".to_owned());
    ctx.set_fonts(fonts);
}

fn install_panic_logger() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");
        let location = info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());
        let payload = info.payload();
        let msg = if let Some(s) = payload.downcast_ref::<&str>() { s.to_string() }
            else if let Some(s) = payload.downcast_ref::<String>() { s.clone() }
            else { "<non-string panic payload>".to_string() };
        applog::log(&format!("==== PANIC [线程 {thread_name}] {location}: {msg} ===="));
        default_hook(info);
    }));
}

fn main() -> eframe::Result<()> {
    applog::init();
    install_panic_logger();
    crash_logger::install();
    const WINDOW_SIZE: [f32; 2] = [1200.0, 750.0];
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(WINDOW_SIZE)
            .with_icon(std::sync::Arc::new(viewport_icon())),
        ..Default::default()
    };
    eframe::run_native(about::APP_NAME, options, Box::new(|cc| {
        setup_fonts(&cc.egui_ctx);
        let textures = setup_about_textures(&cc.egui_ctx);
        Ok(Box::new(DiskForgeApp::new(textures)))
    }))
}

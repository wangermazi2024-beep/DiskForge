//! 主题色板——项目里所有 UI 用色的**唯一定义点**。
//!
//! 以前这些颜色散落在 10+ 个文件里重复硬编码（主题蓝 `0x4C8BF5` 一处文件
//! 里就有 29 处、选中底色 28 处、隐藏遮罩 14 处……），想微调主题就得全局
//! 搜索替换，漏一处就出现"同一个语义两种颜色"。现在所有跨文件复用的颜色
//! 都收在这里；只出现一次的局部颜色（比如某个提示文字的灰色）允许留在
//! 原地，不必为了形式主义多一层跳转。

use egui::Color32;

/// 主题蓝（品牌主色）：表头排序态、分类图例、强调文字、选中底色的基色。
pub const ACCENT_BLUE: Color32 = Color32::from_rgb(0x4C, 0x8B, 0xF5);

/// 列表选中行/单元格的半透明蓝底（ACCENT_BLUE 的 25% 透明版）。
pub const SELECT_BG: Color32 = Color32::from_rgba_unmultiplied_const(0x4C, 0x8B, 0xF5, 0x40);

/// 隐藏/系统文件"整体调暗"用的遮罩色（跟应用背景同色、约 61% 透明，
/// 盖在已画好的内容上呈现"淡出"效果）。
pub const DIM_OVERLAY: Color32 = Color32::from_rgba_unmultiplied_const(0x24, 0x24, 0x28, 0x9C);

/// 表头/磁盘名的强调黄（当前排序列文字色）。
pub const HEADER_ACTIVE_YELLOW: Color32 = Color32::from_rgb(0xFF, 0xD7, 0x00);

/// 文件（叶子节点）的中性灰。
pub const FILE_COLOR: Color32 = Color32::from_rgb(0x6C, 0x75, 0x7D);

/// 文件夹按深度轮转的色板（WinDirStat 风格的分层配色）。
pub const FOLDER_PALETTE: [Color32; 6] = [
    Color32::from_rgb(0x4C, 0x8B, 0xF5),
    Color32::from_rgb(0x34, 0xC7, 0x59),
    Color32::from_rgb(0xF5, 0xA6, 0x23),
    Color32::from_rgb(0xE0, 0x55, 0x5B),
    Color32::from_rgb(0x9C, 0x6A, 0xDE),
    Color32::from_rgb(0x2E, 0xC4, 0xB6),
];

/// 按树的深度取文件夹颜色（循环取色板）。
pub fn folder_color(depth: usize) -> Color32 {
    FOLDER_PALETTE[depth % FOLDER_PALETTE.len()]
}

/// 隐藏/系统文件的 H 徽标底色（石板蓝灰，偏"次要/已忽略"的中性色调）。
pub const HIDDEN_ACCENT: Color32 = Color32::from_rgb(0x6B, 0x8A, 0xA8);

/// 符号链接/junction 的 L 徽标底色（饱和紫）。
pub const REPARSE_ACCENT: Color32 = Color32::from_rgb(0x8B, 0x5C, 0xF6);

/// 符号链接文字的浅紫（和 REPARSE_ACCENT 同色系但更浅，适合大段文字）。
pub const REPARSE_TEXT: Color32 = Color32::from_rgb(0xC4, 0xA7, 0xF5);

/// 重复文件/扩展名分组行的橙色（categorize 的"压缩包"色复用）。
pub const GROUP_COLOR: Color32 = Color32::from_rgb(0xF5, 0xA6, 0x23);

/// 底部状态条的错误红（状态提示专用，比分组红偏亮一点）。
pub const STATUS_ERROR_RED: Color32 = Color32::from_rgb(0xE0, 0x60, 0x60);

/// 删除按钮的深红底。
pub const DANGER_BUTTON_RED: Color32 = Color32::from_rgb(0xC0, 0x40, 0x40);

/// 状态提示的成功绿。
pub const STATUS_OK_GREEN: Color32 = Color32::from_rgb(0x6C, 0xC7, 0x8A);

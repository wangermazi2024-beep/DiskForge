
use egui::Color32;

pub const ACCENT_BLUE: Color32 = Color32::from_rgb(0x4C, 0x8B, 0xF5);

pub const SELECT_BG: Color32 = Color32::from_rgba_unmultiplied_const(0x4C, 0x8B, 0xF5, 0x40);

pub const DIM_OVERLAY: Color32 = Color32::from_rgba_unmultiplied_const(0x24, 0x24, 0x28, 0x9C);

pub const HEADER_ACTIVE_YELLOW: Color32 = Color32::from_rgb(0xFF, 0xD7, 0x00);

pub const FILE_COLOR: Color32 = Color32::from_rgb(0x6C, 0x75, 0x7D);

pub const FOLDER_PALETTE: [Color32; 6] = [
    Color32::from_rgb(0x4C, 0x8B, 0xF5),
    Color32::from_rgb(0x34, 0xC7, 0x59),
    Color32::from_rgb(0xF5, 0xA6, 0x23),
    Color32::from_rgb(0xE0, 0x55, 0x5B),
    Color32::from_rgb(0x9C, 0x6A, 0xDE),
    Color32::from_rgb(0x2E, 0xC4, 0xB6),
];

pub fn folder_color(depth: usize) -> Color32 {
    FOLDER_PALETTE[depth % FOLDER_PALETTE.len()]
}

pub const HIDDEN_ACCENT: Color32 = Color32::from_rgb(0x6B, 0x8A, 0xA8);

pub const REPARSE_ACCENT: Color32 = Color32::from_rgb(0x8B, 0x5C, 0xF6);

pub const REPARSE_TEXT: Color32 = Color32::from_rgb(0xC4, 0xA7, 0xF5);

pub const GROUP_COLOR: Color32 = Color32::from_rgb(0xF5, 0xA6, 0x23);

pub const STATUS_ERROR_RED: Color32 = Color32::from_rgb(0xE0, 0x60, 0x60);

pub const DANGER_BUTTON_RED: Color32 = Color32::from_rgb(0xC0, 0x40, 0x40);

pub const STATUS_OK_GREEN: Color32 = Color32::from_rgb(0x6C, 0xC7, 0x8A);

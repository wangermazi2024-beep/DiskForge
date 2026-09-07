//! Treemap 色块视图（SpaceSniffer 风格）
//!
//! - **单击文件夹**：在当前块内展开子色块（inline 嵌套）。
//! - **单击已展开的文件夹**：收起。
//! - **单击文件**：选中。
//! - **双击文件夹**：产生 `TreeAction::EnterNode`，`app.rs` 据此把该节点的父节点
//!   设为新的"当前视图根"（只是切换 `view_path` 这个只读导航索引，不修改、
//!   不复制、也不丢弃树里任何一个节点）。下一帧本视图用 `root.navigate(view_path)`
//!   重新定位到新的视图根，画法和最外层视图完全一样，没有任何特殊分支/边框/
//!   标题条——看起来就跟直接从那个上级目录重新扫描出来的画面一模一样。
//!   往上回退用的是 `app.rs` 里已有的面包屑 / "⬆ 上级目录" 按钮，
//!   不在 treemap 画布里额外加任何"代表当前目录自己"的色块。

use egui::{Color32, CornerRadius, FontId, Pos2, Rect, RichText, Stroke, StrokeKind, Vec2};

use crate::format::{human_size, truncate_text};
use crate::model::{Node, NodePath};
use crate::treemap::compute_treemap;

use super::TreeAction;

const MAX_DEPTH: u32 = 6;
const MIN_RENDER_W: f32 = 6.0;
const MIN_RENDER_H: f32 = 6.0;
const MIN_EXPAND_W: f32 = 36.0;
const MIN_EXPAND_H: f32 = 28.0;
const NEST_TOP: f32 = 14.0;

const FILE_COLOR: Color32 = Color32::from_rgb(0x5A, 0x6B, 0x7C);
const FILE_BORDER: Color32 = Color32::from_rgb(0x6A, 0x7B, 0x8C);

/// 当前悬停到的（最深一层）色块信息，整帧只保留最后一次写入，
/// 最后统一画一个气泡，避免父子多层色块同时弹出好几个气泡叠在一起。
struct HoverInfo {
    name: String,
    size: u64,
    hint: &'static str,
}

pub fn show(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    root: &Node,
    view_path: &[usize],
    selected: &Option<NodePath>,
    // 上一帧"展开/进入/跳转"操作的目标层路径：如果非空，说明这一层的子色块
    // 是刚刚才出现在画面上的，需要自动选中其中"第一个真正被渲染出来"的色块，
    // 避免旧的白色选中边框停留在一个现在看起来毫不相关的位置上。
    auto_select: Option<&NodePath>,
) -> TreeAction {
    let mut action = TreeAction::None;
    let mut hover: Option<HoverInfo> = None;

    // 只读导航到当前视图根：view_path 失效时（比如目标节点被重新扫描后没了）
    // 退回真正的根节点，不 panic、不 crash。
    let (view_root, mut path) = match root.navigate(view_path) {
        Some(n) => (n, view_path.to_vec()),
        None => (root, Vec::new()),
    };

    // 子色块直接铺满整个 rect，跟"顶层视图"用的是同一个函数、同一套布局，
    // 没有任何特殊分支——双击进入某个目录后，画面看起来就跟直接把这个
    // 目录当成起点重新扫描出来的一模一样。
    draw_children(ui, rect, view_root, &mut path, 0, selected, auto_select, &mut action, &mut hover);

    if let Some(h) = hover {
        show_tooltip(ui, h);
    }

    action
}

fn draw_children(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    node: &Node,
    path: &mut NodePath,
    depth: u32,
    selected: &Option<NodePath>,
    auto_select: Option<&NodePath>,
    action: &mut TreeAction,
    hover: &mut Option<HoverInfo>,
) {
    if node.children.is_empty() { return; }
    if rect.width() < 2.0 || rect.height() < 2.0 { return; }

    let sizes: Vec<u64> = node.children.iter().map(|c| c.size).collect();
    let rects = compute_treemap(&sizes, rect);

    // 如果这一层正是刚被"展开/跳转"的目标层，自动选中第一个真正被渲染出来的
    // 色块——不是数据里下标 0 的孩子，因为它可能因为面积太小而被跳过没画出来。
    if matches!(*action, TreeAction::None) {
        if let Some(target) = auto_select {
            if target.as_slice() == path.as_slice() {
                if let Some(i) = rects.iter().position(|r| r.width() >= MIN_RENDER_W && r.height() >= MIN_RENDER_H) {
                    let mut sel = path.clone();
                    sel.push(i);
                    *action = TreeAction::Select(sel);
                }
            }
        }
    }

    for (i, (r, child)) in rects.iter().zip(node.children.iter()).enumerate() {
        if r.width() < MIN_RENDER_W || r.height() < MIN_RENDER_H {
            continue;
        }

        path.push(i);

        let is_file = child.children.is_empty();
        let block_color = if is_file { FILE_COLOR } else { child.color };
        let is_selected = selected.as_deref() == Some(path.as_slice());

        let painter = ui.painter_at(*r);
        painter.rect_filled(*r, CornerRadius::same(2), block_color);
        let border_color = if is_file { FILE_BORDER } else { Color32::from_rgba_unmultiplied(0, 0, 0, 40) };
        painter.rect_stroke(*r, CornerRadius::same(2), Stroke::new(1.0, border_color), StrokeKind::Inside);
        if is_selected {
            painter.rect_stroke(*r, CornerRadius::same(2), Stroke::new(2.0, Color32::WHITE), StrokeKind::Inside);
        }

        let expanded = child.expanded;
        let can_inline_expand = !is_file
            && expanded
            && depth + 1 < MAX_DEPTH
            && r.width() > MIN_EXPAND_W
            && r.height() > MIN_EXPAND_H;

        draw_label(ui, &painter, *r, child);

        let id = ui.id().with(("block", path.clone()));
        let resp = ui.interact(*r, id, egui::Sense::click());

        // 只记录"当前悬停到的最深一层"是谁：同一像素点，父色块和它内部嵌套的
        // 子色块的矩形是重叠包含关系，这里先记父级、递归画子级时如果指针确实
        // 落在子矩形里会再覆盖一次——最终只剩下最深那一层的信息，帧末统一只画
        // 一个气泡，不会叠出两三个。
        if ui.rect_contains_pointer(*r) {
            let too_small_hint = !can_inline_expand && expanded && !is_file;
            *hover = Some(HoverInfo {
                name: child.name.clone(),
                size: child.size,
                hint: if is_file {
                    "文件 · 单击选中"
                } else if too_small_hint {
                    "文件夹 · 块太小，请双击进入"
                } else {
                    "文件夹 · 单击展开/收起 · 双击进入"
                },
            });
        }

        if can_inline_expand {
            let nested = Rect::from_min_max(
                Pos2::new(r.min.x, r.min.y + NEST_TOP),
                r.max,
            );
            if nested.width() > 4.0 && nested.height() > 4.0 {
                draw_children(ui, nested, child, path, depth + 1, selected, auto_select, action, hover);
            }
        }

        // 单击/双击处理（double_clicked 优先）。
        // 注意：如果这一帧已经被"自动选中第一个色块"占用了 action，
        // 这里的 `matches!(*action, TreeAction::None)` 会让真实点击让位——
        // 这种情况只会发生在"刚展开/跳转的那一帧"，概率极低，不影响正常操作。
        if resp.clicked() && matches!(*action, TreeAction::None) {
            if resp.double_clicked() {
                if !child.children.is_empty() {
                    *action = TreeAction::EnterNode(path.clone());
                }
            } else if is_file {
                *action = TreeAction::Select(path.clone());
            } else if r.width() < MIN_EXPAND_W || r.height() < MIN_EXPAND_H {
                *action = TreeAction::Select(path.clone());
            } else {
                *action = TreeAction::ToggleExpand(path.clone());
            }
        }

        path.pop();
    }
}

fn draw_label(ui: &egui::Ui, painter: &egui::Painter, r: egui::Rect, node: &Node) {
    let pad = 3.0;
    let text_max_w = (r.width() - pad * 2.0).max(0.0);
    if r.width() <= 14.0 || text_max_w <= 4.0 { return; }
    let name_font = FontId::proportional(9.0);
    let shown = truncate_text(ui.ctx(), &node.name, name_font.clone(), text_max_w);
    if !shown.is_empty() && r.height() > 11.0 {
        painter.text(
            r.left_top() + Vec2::new(pad, 2.0),
            egui::Align2::LEFT_TOP,
            &shown,
            name_font,
            Color32::from_rgba_unmultiplied(255, 255, 255, 220),
        );
    }
    if r.height() > 22.0 {
        let size_font = FontId::proportional(8.0);
        let sz = truncate_text(ui.ctx(), &human_size(node.size), size_font.clone(), text_max_w);
        if !sz.is_empty() {
            painter.text(
                r.left_bottom() + Vec2::new(pad, -2.0),
                egui::Align2::LEFT_BOTTOM,
                &sz,
                size_font,
                Color32::from_rgba_unmultiplied(255, 255, 255, 180),
            );
        }
    }
}

/// 整帧只调用一次：画出当前悬停目标的气泡，位置经过计算，
/// 保证气泡矩形不会盖住鼠标指针所在的那个点（否则用户想点别处会先点到气泡上）。
fn show_tooltip(ui: &egui::Ui, info: HoverInfo) {
    let mouse = ui.ctx().pointer_latest_pos().unwrap_or_default();
    let line1 = format!("{} · {}", info.name, human_size(info.size));
    let pos = tooltip_pos(ui, mouse, &line1, info.hint);

    egui::Area::new(ui.id().with("treemap_hover_tip"))
        .fixed_pos(pos)
        .order(egui::Order::Tooltip)
        .interactable(false)
        .show(ui.ctx(), |ui| {
            egui::Frame::default()
                .fill(Color32::from_rgb(0x2A, 0x2C, 0x32))
                .stroke(Stroke::new(1.0, Color32::from_rgb(0x55, 0x55, 0x60)))
                .corner_radius(CornerRadius::same(5))
                .inner_margin(egui::Margin::symmetric(8, 5))
                .show(ui, |ui| {
                    ui.label(RichText::new(line1).color(Color32::WHITE));
                    ui.label(
                        RichText::new(info.hint)
                            .size(10.5)
                            .color(Color32::from_rgb(0xA0, 0xA0, 0xA0)),
                    );
                });
        });
}

/// 根据鼠标位置和屏幕可用范围，选一个"背对鼠标"的方向摆放气泡：
/// - 水平方向优先放右边；右边放不下（比如鼠标在屏幕右下角）就放左边。
/// - 垂直方向优先放上边；上边放不下（比如鼠标在屏幕顶部）就放下边。
/// 因为水平方向必定往鼠标 x 坐标的某一侧偏移至少 GAP 像素，
/// 气泡矩形的 x 范围永远不包含鼠标 x 坐标本身，所以不管垂直方向怎么选，
/// 气泡都不可能盖住鼠标指针所在的那个点。
fn tooltip_pos(ui: &egui::Ui, mouse: Pos2, line1: &str, line2: &str) -> Pos2 {
    let screen = ui.ctx().input(|i| i.raw.screen_rect.unwrap_or(
        egui::Rect::from_min_max(egui::pos2(-1e5, -1e5), egui::pos2(1e5, 1e5)),
    ));

    let measure = |s: &str, size: f32| -> f32 {
        let font = FontId::proportional(size);
        ui.ctx().fonts_mut(|f| f.layout_no_wrap(s.to_owned(), font, Color32::WHITE).size().x)
    };
    let w1 = measure(line1, 13.0);
    let w2 = measure(line2, 10.5);
    // 粗略估算气泡尺寸（含 padding），不需要像素级精确，够用来判断往哪边摆就行。
    let est_w = w1.max(w2) + 18.0;
    let est_h = 46.0;
    const GAP: f32 = 16.0;

    let place_right = mouse.x + GAP + est_w <= screen.right();
    let x = if place_right {
        mouse.x + GAP
    } else {
        (mouse.x - GAP - est_w).max(screen.left())
    };

    let place_above = mouse.y - GAP - est_h >= screen.top();
    let y = if place_above {
        mouse.y - GAP - est_h
    } else {
        mouse.y + GAP
    };

    Pos2::new(x, y)
}

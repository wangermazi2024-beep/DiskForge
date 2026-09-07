//! Binary Treemap 算法 — 两遍扫描 + 最小矩形保护
//!
//! 文件和文件夹用完全相同的算法，只是渲染颜色和交互不同。
//! 色块无缝平铺，无多余间隙，不超出父层边界。
//!
//! 参考：Speedy37/streemap-rs Binary 算法 + 两遍扫描思路

/// 间距（像素），设为 0 实现无缝平铺。如果需要在色块间保留细线，
/// 可改为 0.5～1.0，由调用方统一 shrink。
/// 纹布局统一处理，渲染层不再 shrink。

/// 最小可见块边长（像素）。小于此值的块在算法层就被剔除，不占用空间。
const MIN_BLOCK_PX: f32 = 6.0;

/// 二分分割（核心布局），子矩形精确保留在 parent rect 范围内
fn binary_split(
    rect: egui::Rect,
    indices: &mut [usize],
    scaled: &[f32],
    out: &mut [egui::Rect],
) {
    let n = indices.len();
    if n == 0 { return; }
    if n == 1 {
        out[indices[0]] = rect;
        return;
    }

    let mut prefix = Vec::with_capacity(n);
    let mut total = 0.0_f32;
    for &idx in indices.iter() {
        total += scaled[idx];
        prefix.push(total);
    }

    let target = total * 0.5;
    let mid = match prefix.binary_search_by(|p| {
        if *p > target { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less }
    }) {
        Ok(i) => i + 1,
        Err(0) => 1,
        Err(i) => i,
    }
    .min(n - 1);

    let left_frac = if total > 0.0 { prefix[mid - 1] / total } else { 0.5 };

    if rect.width() >= rect.height() {
        // 垂直分割（左右）
        let xm = rect.min.x + rect.width() * left_frac;
        // 精确保留边界，防止浮点溢出
        let xm = xm.clamp(rect.min.x, rect.max.x);
        let left = egui::Rect::from_min_max(rect.min, egui::pos2(xm, rect.max.y));
        let right = egui::Rect::from_min_max(egui::pos2(xm, rect.min.y), rect.max);
        binary_split(left, &mut indices[..mid], scaled, out);
        binary_split(right, &mut indices[mid..], scaled, out);
    } else {
        // 水平分割（上下）
        let ym = rect.min.y + rect.height() * left_frac;
        let ym = ym.clamp(rect.min.y, rect.max.y);
        let top = egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, ym));
        let bottom = egui::Rect::from_min_max(egui::pos2(rect.min.x, ym), rect.max);
        binary_split(top, &mut indices[..mid], scaled, out);
        binary_split(bottom, &mut indices[mid..], scaled, out);
    }
}

/// 计算 treemap 布局（两遍扫描 + 最小矩形保护）。
///
/// 返回的 `Vec<Rect>` 与输入 `sizes` ——对应。
/// - 宽 >= MIN_BLOCK_PX 且 高 >= MIN_BLOCK_PX 的块是有效矩形。
/// - 小于此值的块返回 `Rect::NOTHING`，调用方跳过绘制即可。
///
/// `Rect::NOTHING` 块不占用实际像素空间（其面积已重新分配给剩余块）。
///
/// 文件和文件夹使用完全相同的算法，只有颜色和交互不同。
pub fn compute_treemap(sizes: &[u64], rect: egui::Rect) -> Vec<egui::Rect> {
    let n = sizes.len();
    let mut out = vec![egui::Rect::NOTHING; n];
    if n == 0 || rect.width() <= 1.0 || rect.height() <= 1.0 {
        return out;
    }

    let mut active: Vec<bool> = vec![true; n];

    for _iter in 0..12 {
        let total: f64 = active.iter().enumerate()
            .filter(|(_, a)| **a)
            .map(|(i, _)| sizes[i].max(1) as f64)
            .sum();
        if total <= 0.0 { break; }

        let area = rect.width() as f64 * rect.height() as f64;

        // 大文件压缩
        let max_frac = active.iter().enumerate()
            .filter(|(_, a)| **a)
            .map(|(i, _)| sizes[i].max(1) as f64 / total)
            .fold(0.0_f64, f64::max);

        let scaled: Vec<f32> = if max_frac > 0.5 {
            let pow = 0.72_f64;
            let ct: f64 = sizes.iter().enumerate()
                .map(|(i, _)| if active[i] { (sizes[i].max(1) as f64).powf(pow) } else { 0.0 })
                .sum();
            sizes.iter().enumerate()
                .map(|(i, &s)| if active[i] { ((s.max(1) as f64).powf(pow) / ct * area) as f32 } else { 0.0 })
                .collect()
        } else {
            let scale = area / total;
            sizes.iter().enumerate()
                .map(|(i, &s)| if active[i] { (s.max(1) as f64 * scale) as f32 } else { 0.0 })
                .collect()
        };

        // 降序排列后二分分割
        let mut order: Vec<usize> = (0..n).filter(|i| active[*i]).collect();
        order.sort_by(|a, b| sizes[*b].cmp(&sizes[*a]));
        let mut tmp = vec![egui::Rect::NOTHING; n];
        binary_split(rect, &mut order, &scaled, &mut tmp);

        // 检查最小尺寸，剔除不合格项
        let mut any_small = false;
        for &i in &order {
            let r = tmp[i];
            if r.width() < MIN_BLOCK_PX || r.height() < MIN_BLOCK_PX {
                active[i] = false;
                any_small = true;
            }
        }

        if !any_small {
            for &i in &order { out[i] = tmp[i]; }
            break;
        }
    }

    out
}

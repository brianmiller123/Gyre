//! PNG 帧渲染：`ab_glyph` 栅格化 Liberation Mono 字形（内嵌 TTF）到 8×16 单元格，
//! 白底黑字，`image` crate 编码 PNG。
//!
//! 字体来源：系统 Liberation Mono Regular（SIL Open Font License 1.1 兼容的
//! 自由许可），复制进本 crate 资产（`fonts/LiberationMono-Regular.ttf`）。

// 像素坐标与灰度转换均在图像边界内受控（clamp 后 cast），无需逐处 lint 豁免。
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use ab_glyph::{Font, FontArc, Glyph, OutlinedGlyph, PxScale, ScaleFont};
use image::GrayImage;
use std::io::Cursor;

use crate::shape::Shape;

/// 渲染错误。
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// 内嵌字体加载失败。
    #[error("内嵌字体加载失败: {0}")]
    FontLoad(String),
    /// 字形轮廓获取失败。
    #[error("字形轮廓获取失败: {0}")]
    Outline(String),
    /// PNG 编码失败。
    #[error("PNG 编码失败: {0}")]
    Encode(String),
}

/// 一帧渲染结果：PNG 字节 + 像素尺寸。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// PNG 编码数据（base64 传输时由调用方编码）。
    pub data: Vec<u8>,
    /// 帧宽（px）。
    pub width: u32,
    /// 帧高（px）。
    pub height: u32,
}

/// 一次性加载的内嵌字体（进程级缓存）。
fn font() -> &'static FontArc {
    static FONT: std::sync::OnceLock<FontArc> = std::sync::OnceLock::new();
    FONT.get_or_init(|| {
        FontArc::try_from_vec(include_bytes!("fonts/LiberationMono-Regular.ttf").to_vec())
            .expect("内嵌 Liberation Mono 字体必须可加载")
    })
}

/// 字体 scale：使等宽 `advance` 恰好等于单元格宽度（px）。`ab_glyph` 的
/// `h_scale_factor = scale / height_unscaled`（按行高而非 em 归一化）。
fn scale_for_cell(font: &FontArc, cell_w: u32) -> f32 {
    let height = font.height_unscaled();
    let advance_units = font.h_advance_unscaled(font.glyph_id('M'));
    (cell_w as f32) * height / advance_units
}

/// 渲染一页文本为 PNG 帧。`page` 为 `\n` 连接的行（行数 ≤ [`Shape::rows`]）。
/// 未知字形（如 CJK）折叠为 `?`。
///
/// # Errors
/// 字体或编码失败时返回 [`RenderError`]。
pub fn render_frame(page: &str, shape: &Shape) -> Result<Frame, RenderError> {
    let font = font();
    let scale = PxScale::from(scale_for_cell(font, shape.cell_w));
    let scaled = font.as_scaled(scale);

    let rows = shape.rows();
    let lines: Vec<&str> = page.lines().take(rows).collect();
    let frame_h = (lines.len() as u32).max(1) * shape.cell_h;

    let mut img = GrayImage::from_pixel(shape.frame_size, frame_h, image::Luma([255u8]));

    let ascent = scaled.ascent();
    let descent = scaled.descent();
    let line_box = ascent + descent;
    let baseline_y_base = if line_box < shape.cell_h as f32 {
        (shape.cell_h as f32 - line_box) / 2.0
    } else {
        0.0
    };

    for (row, line) in lines.iter().enumerate() {
        let baseline = (shape.cell_h as f32 * row as f32).mul_add(1.0, baseline_y_base) + ascent;
        let mut pen_x = 0.0f32;
        for ch in line.chars() {
            let mut glyph = font.glyph_id(ch);
            if glyph.0 == 0 {
                // .notdef：折叠为 '?'。
                glyph = font.glyph_id('?');
            }
            let id = glyph;
            let glyph: Glyph =
                glyph.with_scale_and_position(scale, ab_glyph::point(pen_x, baseline));
            if let Some(outlined) = font.outline_glyph(glyph) {
                paint_outlined(&mut img, &outlined);
            }
            pen_x += scaled.h_advance(id);
        }
    }

    // PNG 编码（灰度 8 位）。
    let mut buf = Vec::new();
    {
        let mut cursor = Cursor::new(&mut buf);
        image::DynamicImage::ImageLuma8(img)
            .write_to(&mut cursor, image::ImageFormat::Png)
            .map_err(|e| RenderError::Encode(e.to_string()))?;
    }
    Ok(Frame {
        data: buf,
        width: shape.frame_size,
        height: frame_h,
    })
}

/// 把一个已定位字形绘制到图像（逐像素 alpha 覆盖）。
fn paint_outlined(img: &mut GrayImage, outlined: &OutlinedGlyph) {
    let bounds = outlined.px_bounds();
    let (ox, oy) = (
        i64::from(bounds.min.x as u32),
        i64::from(bounds.min.y as u32),
    );
    outlined.draw(|x, y, cover| {
        let px = ox + i64::from(x);
        let py = oy + i64::from(y);
        if px < 0 || py < 0 || px >= i64::from(img.width()) || py >= i64::from(img.height()) {
            return;
        }
        // 白底黑字：cover=1 全墨 → 0；cover=0 → 255。
        let ink = (255.0 * (1.0 - cover)) as u8;
        let existing = img.get_pixel(px as u32, py as u32)[0];
        // 与已有像素取更黑（多字形重叠处防白点残留）。
        let merged = existing.min(ink);
        img.put_pixel(px as u32, py as u32, image::Luma([merged]));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_size(data: &[u8]) -> (u32, u32) {
        let img = image::load_from_memory(data).expect("PNG 可解码");
        (img.width(), img.height())
    }

    #[test]
    fn renders_single_line_frame() {
        let frame = render_frame("hello", &crate::DEFAULT_SHAPE).unwrap();
        assert_eq!(frame.width, 1568);
        assert_eq!(frame.height, 16, "单行帧高 = 1 行盒");
        assert!(frame.data.starts_with(b"\x89PNG"));
        let (w, h) = png_size(&frame.data);
        assert_eq!((w, h), (1568, 16));
    }

    #[test]
    fn renders_multi_line_frame_height_hugs_content() {
        let frame = render_frame("a\nb\nc", &crate::DEFAULT_SHAPE).unwrap();
        assert_eq!(frame.height, 3 * 16);
    }

    #[test]
    fn caps_lines_at_frame_rows() {
        let page = (0..200)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let frame = render_frame(&page, &crate::DEFAULT_SHAPE).unwrap();
        assert_eq!(frame.height, 98 * 16, "超出行数被裁剪");
    }

    #[test]
    fn unknown_glyphs_fold_to_question_mark() {
        // CJK 应可渲染（折叠为 '?' 而非报错/空白）。
        let frame = render_frame("中文路径 test", &crate::DEFAULT_SHAPE).unwrap();
        assert!(frame.data.starts_with(b"\x89PNG"));
        // 帧有墨迹（非纯白）：'?' 字形已绘制。
        let img = image::load_from_memory(&frame.data).unwrap().to_luma8();
        let dark = img.pixels().filter(|p| p[0] < 128).count();
        assert!(dark > 50, "应绘制出字形像素，实际 {dark}");
    }

    #[test]
    fn ink_pixels_present_on_white_canvas() {
        let frame = render_frame("RX9|_/\\{}", &crate::DEFAULT_SHAPE).unwrap();
        let img = image::load_from_memory(&frame.data).unwrap().to_luma8();
        let dark = img.pixels().filter(|p| p[0] < 128).count();
        assert!(dark > 100, "符号字形应产生墨迹，实际 {dark}");
        let white = img.pixels().filter(|p| p[0] > 200).count();
        assert!(white > 0, "白底保留");
    }

    #[test]
    fn empty_page_still_renders_min_frame() {
        let frame = render_frame("", &crate::DEFAULT_SHAPE).unwrap();
        assert_eq!(frame.height, 16);
    }

    #[test]
    fn monospace_advance_matches_cell_width() {
        let font = font();
        let scale = PxScale::from(scale_for_cell(font, 8));
        let scaled = font.as_scaled(scale);
        // 'M' 与 'i' 等宽（等宽字体保证）。
        let a = scaled.h_advance(font.glyph_id('M'));
        let b = scaled.h_advance(font.glyph_id('i'));
        assert!((a - b).abs() < 0.01, "等宽 advance 应一致: {a} vs {b}");
        assert!((a - 8.0).abs() < 0.5, "advance 应接近 8px: {a}");
    }

    #[test]
    fn frame_is_deterministic() {
        let a = render_frame("same text", &crate::DEFAULT_SHAPE).unwrap();
        let b = render_frame("same text", &crate::DEFAULT_SHAPE).unwrap();
        assert_eq!(a.data, b.data, "同输入同输出（确定性）");
    }
}

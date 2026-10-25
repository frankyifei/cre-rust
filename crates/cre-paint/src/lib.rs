use std::collections::HashMap;
use std::path::Path;

use anyrender::recording::RenderCommand;
use anyrender::{Paint, Scene};
use fontdue::{Font, FontSettings};
use kurbo::{Affine, BezPath, PathEl, Point, Shape, Stroke};
use peniko::{Color, Fill as AnyFill, ImageFormat};
use tiny_skia::{
    FillRule, LineCap, LineJoin, Mask, Paint as SkPaint, PathBuilder, Pixmap, Shader,
    Stroke as SkStroke, Transform,
};

pub struct PageBitmap {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl PageBitmap {
    pub fn new(width: u32, height: u32, background: u8) -> Self {
        Self {
            width,
            height,
            data: vec![background; (width * height) as usize],
        }
    }

    pub fn write_pgm(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let mut out = format!("P5\n{} {}\n255\n", self.width, self.height).into_bytes();
        out.extend_from_slice(&self.data);
        std::fs::write(path, out)
    }
}

pub fn render_page(
    scene: &Scene,
    width: u32,
    height: u32,
    page_top: f32,
    clip_top: f32,
    clip_bottom: f32,
) -> (PageBitmap, usize, usize) {
    let mut pixmap = Pixmap::new(width, height).expect("non-zero page bitmap");
    pixmap.fill(tiny_skia::Color::WHITE);

    let page_transform = Affine::translate((0.0, -page_top as f64));
    let local_top = (clip_top - page_top).clamp(0.0, height as f32);
    let local_bottom = (clip_bottom - page_top).clamp(0.0, height as f32);
    let clip_mask = page_clip_mask(width, height, local_top, local_bottom);
    let (image_count, image_surface) = image_draw_stats(
        scene,
        width,
        height,
        page_transform,
        local_top,
        local_bottom,
    );
    let mut fonts = HashMap::new();

    for command in &scene.commands {
        match command {
            RenderCommand::Fill(cmd) => match &cmd.brush {
                Paint::Solid(color) => {
                    draw_fill(
                        &mut pixmap,
                        &cmd.shape,
                        page_transform * cmd.transform,
                        *color,
                        cmd.fill,
                        clip_mask.as_ref(),
                    );
                }
                Paint::Image(image_brush) => {
                    draw_image_fill(
                        &mut pixmap,
                        &image_brush.image,
                        image_brush.sampler.alpha,
                        &cmd.shape,
                        page_transform * cmd.transform,
                        cmd.brush_transform,
                        local_top,
                        local_bottom,
                        clip_mask.as_ref(),
                    );
                }
                _ => {}
            },
            RenderCommand::Stroke(cmd) => {
                if let Some(color) = solid_color(&cmd.brush) {
                    draw_stroke(
                        &mut pixmap,
                        &cmd.shape,
                        page_transform * cmd.transform,
                        color,
                        &cmd.style,
                        clip_mask.as_ref(),
                    );
                }
            }
            RenderCommand::GlyphRun(cmd) => {
                if let Some(color) = solid_color(&cmd.brush) {
                    draw_glyphs(
                        &mut pixmap,
                        &cmd.font_data,
                        cmd.font_size,
                        &cmd.glyphs,
                        page_transform * cmd.transform,
                        cmd.glyph_transform,
                        color,
                        cmd.brush_alpha,
                        local_top,
                        local_bottom,
                        &mut fonts,
                    );
                }
            }
            RenderCommand::PushLayer(_)
            | RenderCommand::PushClipLayer(_)
            | RenderCommand::PopLayer
            | RenderCommand::BoxShadow(_) => {}
        }
    }

    let mut data = Vec::with_capacity((width * height) as usize);
    for pixel in pixmap.pixels() {
        let r = pixel.red() as u32;
        let g = pixel.green() as u32;
        let b = pixel.blue() as u32;
        let a = pixel.alpha() as u32;
        let luma = (r * 299 + g * 587 + b * 114) / 1000;
        let value = ((luma * a + 255 * (255 - a)) / 255) as u8;
        data.push(value);
    }

    (
        PageBitmap {
            width,
            height,
            data,
        },
        image_count,
        image_surface,
    )
}

fn image_draw_stats(
    scene: &Scene,
    width: u32,
    height: u32,
    page_transform: Affine,
    local_top: f32,
    local_bottom: f32,
) -> (usize, usize) {
    let viewport_width = width as f32;
    let viewport_top = local_top.max(0.0);
    let viewport_bottom = local_bottom.min(height as f32);
    let mut count = 0usize;
    let mut surface = 0usize;
    for command in &scene.commands {
        if let RenderCommand::Fill(cmd) = command
            && matches!(cmd.brush, Paint::Image(_))
            && let Some((left, top, right, bottom)) =
                path_bounds(&cmd.shape, page_transform * cmd.transform)
        {
            let visible_left = left.clamp(0.0, viewport_width);
            let visible_right = right.clamp(0.0, viewport_width);
            let visible_top = top.clamp(viewport_top, viewport_bottom);
            let visible_bottom = bottom.clamp(viewport_top, viewport_bottom);
            if visible_right <= visible_left || visible_bottom <= visible_top {
                continue;
            }
            count += 1;
            let area =
                (visible_right - visible_left) as f64 * (visible_bottom - visible_top) as f64;
            surface += area.clamp(0.0, f64::MAX).ceil() as usize;
        }
    }
    (count, surface)
}

fn path_bounds(shape: &BezPath, transform: Affine) -> Option<(f32, f32, f32, f32)> {
    let max_float = f32::MAX;
    let mut min_x = max_float;
    let mut min_y = max_float;
    let mut max_x = -max_float;
    let mut max_y = -max_float;
    let mut found = false;
    let mut add_point = |p: Point| {
        let p = transform * p;
        let px = p.x as f32;
        let py = p.y as f32;
        if px.is_nan() || py.is_nan() {
            return;
        }
        found = true;
        min_x = min_x.min(px);
        max_x = max_x.max(px);
        min_y = min_y.min(py);
        max_y = max_y.max(py);
    };
    for element in shape.iter() {
        match element {
            PathEl::MoveTo(p) | PathEl::LineTo(p) => add_point(p),
            PathEl::QuadTo(p1, p2) => {
                add_point(p1);
                add_point(p2);
            }
            PathEl::CurveTo(p1, p2, p3) => {
                add_point(p1);
                add_point(p2);
                add_point(p3);
            }
            PathEl::ClosePath => {}
        }
    }
    if found {
        Some((min_x, min_y, max_x, max_y))
    } else {
        None
    }
}

fn solid_color(paint: &Paint) -> Option<Color> {
    match paint {
        Paint::Solid(color) => Some(*color),
        _ => None,
    }
}

fn page_clip_mask(width: u32, height: u32, top: f32, bottom: f32) -> Option<Mask> {
    if top <= 0.0 && bottom >= height as f32 {
        return None;
    }
    let mut mask = Mask::new(width, height)?;
    let mut builder = PathBuilder::new();
    builder.push_rect(tiny_skia::Rect::from_ltrb(
        0.0,
        top.max(0.0),
        width as f32,
        bottom.min(height as f32),
    )?);
    let path = builder.finish()?;
    mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
    Some(mask)
}

fn draw_fill(
    pixmap: &mut Pixmap,
    shape: &BezPath,
    transform: Affine,
    color: Color,
    fill: AnyFill,
    clip_mask: Option<&Mask>,
) {
    let path = to_skia_path(shape, transform);
    let Some(path) = path else { return };
    let paint = skia_paint(color);
    let rule = match fill {
        AnyFill::NonZero => FillRule::Winding,
        AnyFill::EvenOdd => FillRule::EvenOdd,
    };
    pixmap.fill_path(&path, &paint, rule, Transform::identity(), clip_mask);
}

fn draw_stroke(
    pixmap: &mut Pixmap,
    shape: &BezPath,
    transform: Affine,
    color: Color,
    style: &Stroke,
    clip_mask: Option<&Mask>,
) {
    let path = to_skia_path(shape, transform);
    let Some(path) = path else { return };
    let paint = skia_paint(color);
    let stroke = SkStroke {
        width: style.width as f32,
        line_cap: match style.start_cap {
            kurbo::Cap::Butt => LineCap::Butt,
            kurbo::Cap::Round => LineCap::Round,
            kurbo::Cap::Square => LineCap::Square,
        },
        line_join: match style.join {
            kurbo::Join::Bevel => LineJoin::Bevel,
            kurbo::Join::Miter => LineJoin::Miter,
            kurbo::Join::Round => LineJoin::Round,
        },
        miter_limit: style.miter_limit as f32,
        dash: None,
    };
    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), clip_mask);
}

#[allow(clippy::too_many_arguments)]
fn draw_glyphs(
    pixmap: &mut Pixmap,
    font_data: &peniko::FontData,
    font_size: f32,
    glyphs: &[anyrender::Glyph],
    transform: Affine,
    glyph_transform: Option<Affine>,
    color: Color,
    brush_alpha: f32,
    clip_top: f32,
    clip_bottom: f32,
    fonts: &mut HashMap<usize, Font>,
) {
    let key = font_data.data.data().as_ptr() as usize ^ font_data.index as usize;
    let font = fonts.entry(key).or_insert_with(|| {
        Font::from_bytes(font_data.data.data(), FontSettings::default()).expect("parse font data")
    });

    let scale = transform.as_coeffs();
    let scale_x = (scale[0] * scale[0] + scale[1] * scale[1]).sqrt() as f32;
    let scale_y = (scale[2] * scale[2] + scale[3] * scale[3]).sqrt() as f32;
    let glyph_size = font_size * scale_y.max(scale_x);
    if glyph_size <= 0.0 {
        return;
    }

    let rgba = color.to_rgba8();
    let alpha = (rgba.a as f32 / 255.0) * brush_alpha;

    for glyph in glyphs {
        if glyph.id == 0 {
            continue;
        }
        let mut glyph_transform = glyph_transform.unwrap_or(Affine::IDENTITY);
        glyph_transform = transform * glyph_transform;
        let origin = glyph_transform * Point::new(glyph.x as f64, glyph.y as f64);
        if (origin.y as f32) < clip_top - glyph_size || (origin.y as f32) > clip_bottom + glyph_size
        {
            continue;
        }

        let (metrics, bitmap) = font.rasterize_indexed(glyph.id as u16, glyph_size);
        if metrics.width == 0 || metrics.height == 0 {
            continue;
        }

        let x0 = origin.x as f32 + metrics.xmin as f32;
        let y0 = origin.y as f32 - metrics.ymin as f32 - metrics.height as f32;
        if (y0 + metrics.height as f32) < clip_top || y0 > clip_bottom {
            continue;
        }

        for row in 0..metrics.height {
            for col in 0..metrics.width {
                let coverage = bitmap[row * metrics.width + col] as f32 / 255.0;
                if coverage <= 0.0 {
                    continue;
                }
                let x = x0.round() as i32 + col as i32;
                let y = y0.round() as i32 + row as i32;
                if x < 0 || y < 0 || x >= pixmap.width() as i32 || y >= pixmap.height() as i32 {
                    continue;
                }
                if (y as f32) < clip_top || (y as f32) >= clip_bottom {
                    continue;
                }
                blend_pixel(
                    pixmap,
                    x as u32,
                    y as u32,
                    rgba.r,
                    rgba.g,
                    rgba.b,
                    coverage * alpha,
                );
            }
        }
    }
}

fn blend_pixel(pixmap: &mut Pixmap, x: u32, y: u32, r: u8, g: u8, b: u8, alpha: f32) {
    if alpha <= 0.0 {
        return;
    }
    let idx = ((y * pixmap.width() + x) * 4) as usize;
    let data = pixmap.data_mut();
    let src_a = alpha.clamp(0.0, 1.0);
    let dst_a = data[idx + 3] as f32 / 255.0;
    let out_a = src_a + dst_a * (1.0 - src_a);
    if out_a <= 0.0 {
        data[idx] = 255;
        data[idx + 1] = 255;
        data[idx + 2] = 255;
        data[idx + 3] = 0;
        return;
    }
    for (offset, src) in [(0usize, r), (1, g), (2, b)] {
        let dst = data[idx + offset] as f32;
        let out = (src as f32 * src_a + dst * dst_a * (1.0 - src_a)) / out_a;
        data[idx + offset] = out.clamp(0.0, 255.0) as u8;
    }
    data[idx + 3] = (out_a * 255.0) as u8;
}

fn skia_paint(color: Color) -> SkPaint<'static> {
    let rgba = color.to_rgba8();
    SkPaint {
        shader: Shader::SolidColor(tiny_skia::Color::from_rgba8(rgba.r, rgba.g, rgba.b, rgba.a)),
        anti_alias: true,
        ..Default::default()
    }
}

fn to_skia_path(shape: &BezPath, transform: Affine) -> Option<tiny_skia::Path> {
    let mut builder = PathBuilder::new();
    for el in shape.elements() {
        let el = apply_el(*el, transform);
        match el {
            PathEl::MoveTo(p) => builder.move_to(p.x as f32, p.y as f32),
            PathEl::LineTo(p) => builder.line_to(p.x as f32, p.y as f32),
            PathEl::QuadTo(p1, p2) => {
                builder.quad_to(p1.x as f32, p1.y as f32, p2.x as f32, p2.y as f32)
            }
            PathEl::CurveTo(p1, p2, p3) => builder.cubic_to(
                p1.x as f32,
                p1.y as f32,
                p2.x as f32,
                p2.y as f32,
                p3.x as f32,
                p3.y as f32,
            ),
            PathEl::ClosePath => builder.close(),
        }
    }
    builder.finish()
}

fn apply_el(el: PathEl, transform: Affine) -> PathEl {
    match el {
        PathEl::MoveTo(p) => PathEl::MoveTo(transform * p),
        PathEl::LineTo(p) => PathEl::LineTo(transform * p),
        PathEl::QuadTo(p1, p2) => PathEl::QuadTo(transform * p1, transform * p2),
        PathEl::CurveTo(p1, p2, p3) => {
            PathEl::CurveTo(transform * p1, transform * p2, transform * p3)
        }
        PathEl::ClosePath => PathEl::ClosePath,
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_image_fill(
    pixmap: &mut Pixmap,
    image: &peniko::ImageData,
    alpha: f32,
    shape: &BezPath,
    fill_transform: Affine,
    brush_transform: Option<Affine>,
    clip_top: f32,
    clip_bottom: f32,
    clip_mask: Option<&Mask>,
) {
    if alpha <= 0.0 || image.width == 0 || image.height == 0 {
        return;
    }
    let img_data: &[u8] = image.data.data();
    let bpp = 4usize;

    let inv_fill = fill_transform.inverse();
    let brush_inv = brush_transform.unwrap_or(Affine::IDENTITY).inverse();
    let to_image = brush_inv * inv_fill;

    let raw = shape.bounding_box();
    let corners = [
        fill_transform * Point::new(raw.x0, raw.y0),
        fill_transform * Point::new(raw.x1, raw.y0),
        fill_transform * Point::new(raw.x0, raw.y1),
        fill_transform * Point::new(raw.x1, raw.y1),
    ];
    let bx0 = corners.iter().map(|p| p.x).fold(f64::INFINITY, f64::min);
    let by0 = corners.iter().map(|p| p.y).fold(f64::INFINITY, f64::min);
    let bx1 = corners
        .iter()
        .map(|p| p.x)
        .fold(f64::NEG_INFINITY, f64::max);
    let by1 = corners
        .iter()
        .map(|p| p.y)
        .fold(f64::NEG_INFINITY, f64::max);
    let x0 = bx0.max(0.0).min(pixmap.width() as f64) as i32;
    let x1 = bx1.max(0.0).min(pixmap.width() as f64) as i32;
    let y0 = by0
        .max(clip_top as f64)
        .max(0.0)
        .min(pixmap.height() as f64) as i32;
    let y1 = by1
        .min(clip_bottom as f64)
        .max(0.0)
        .min(pixmap.height() as f64) as i32;

    let mask_data = clip_mask.map(|m| m.data());

    for py in y0..y1 {
        for px in x0..x1 {
            if let Some(md) = mask_data {
                let mi = (py as u32 * pixmap.width() + px as u32) as usize;
                if md[mi] == 0 {
                    continue;
                }
            }
            let p = to_image * Point::new(px as f64 + 0.5, py as f64 + 0.5);
            let sx = p.x.floor() as i32;
            let sy = p.y.floor() as i32;
            if sx < 0 || sy < 0 || sx >= image.width as i32 || sy >= image.height as i32 {
                continue;
            }
            let si = ((sy as u32 * image.width + sx as u32) as usize) * bpp;
            if si + 3 >= img_data.len() {
                continue;
            }
            let (r, g, b, a) = match image.format {
                ImageFormat::Rgba8 => (
                    img_data[si],
                    img_data[si + 1],
                    img_data[si + 2],
                    img_data[si + 3],
                ),
                ImageFormat::Bgra8 => (
                    img_data[si + 2],
                    img_data[si + 1],
                    img_data[si],
                    img_data[si + 3],
                ),
                _ => continue,
            };
            let src_a = (a as f32 / 255.0) * alpha;
            if src_a <= 0.0 {
                continue;
            }
            let mask_a = clip_mask.map_or(1.0, |m| {
                m.data()[(py as u32 * pixmap.width() + px as u32) as usize] as f32 / 255.0
            });
            blend_pixel(pixmap, px as u32, py as u32, r, g, b, src_a * mask_a);
        }
    }
}

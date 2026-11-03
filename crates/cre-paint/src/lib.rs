use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyrender::recording::RenderCommand;
use anyrender::{Paint, Scene};
use kurbo::{Affine, BezPath, PathEl, Point, Shape, Stroke};
use peniko::{Color, Extend as PenikoExtend, Fill as AnyFill, Gradient, GradientKind, ImageFormat};
use tiny_skia::{
    FillRule, GradientStop as SkGradientStop, LineCap, LineJoin,
    LinearGradient as SkLinearGradient, Mask, Paint as SkPaint, PathBuilder, Pixmap, PixmapRef,
    RadialGradient as SkRadialGradient, Shader, SpreadMode, Stroke as SkStroke, Transform,
};

mod raster;

/// Rasterized glyph cache. Rasterizing dominates first-page cost on slow
/// targets, so a glyph is turned into a coverage bitmap once per paint.
/// Key: font pointer, glyph id, and the raster size quantized to 1/4 px.
type GlyphKey = (usize, u16, u32);
type GlyphBitmap = Arc<Option<GlyphRaster>>;

/// One rasterized glyph as an 8-bit coverage bitmap, placed relative to the
/// pen position on the baseline. `top` is negative for pixels above it, so
/// both offsets can simply be added to the text origin.
struct GlyphRaster {
    width: u32,
    height: u32,
    left: f32,
    top: f32,
    data: Vec<u8>,
}

/// Collects a glyph outline into a `tiny-skia` path, flipping y (fonts grow
/// upwards, page bitmaps grow downwards) and scaling font units to pixels.
struct OutlinePath {
    builder: PathBuilder,
    scale: f32,
}

impl ttf_parser::OutlineBuilder for OutlinePath {
    fn move_to(&mut self, x: f32, y: f32) {
        self.builder.move_to(x * self.scale, -y * self.scale);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.builder.line_to(x * self.scale, -y * self.scale);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.builder.quad_to(
            x1 * self.scale,
            -y1 * self.scale,
            x * self.scale,
            -y * self.scale,
        );
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.builder.cubic_to(
            x1 * self.scale,
            -y1 * self.scale,
            x2 * self.scale,
            -y2 * self.scale,
            x * self.scale,
            -y * self.scale,
        );
    }

    fn close(&mut self) {
        self.builder.close();
    }
}

/// Rasterizes one glyph outline into a coverage bitmap.
///
/// `ttf-parser` only parses the table directory, so this stays cheap even for
/// a 16 MB CJK font. `fontdue::Font::from_bytes` cannot be used here: it
/// outlines *every* glyph in the file up front, which measured 307 MB of RSS
/// and 0.63 s for NotoSansCJKsc-Regular.otf (16 MB, ~65k glyphs) and kills
/// KOReader on a 256 MB Kindle.
fn rasterize_glyph(face: &ttf_parser::Face<'_>, glyph_id: u16, size: f32) -> Option<GlyphRaster> {
    if glyph_id == 0 || !size.is_finite() || size <= 0.0 {
        return None;
    }
    let units_per_em = f32::from(face.units_per_em());
    if !units_per_em.is_finite() || units_per_em <= 0.0 {
        return None;
    }
    let mut outline = OutlinePath {
        builder: PathBuilder::new(),
        scale: size / units_per_em,
    };
    face.outline_glyph(ttf_parser::GlyphId(glyph_id), &mut outline)?;
    let path = outline.builder.finish()?;
    let bounds = path.bounds();
    if !bounds.width().is_finite() || !bounds.height().is_finite() {
        return None;
    }
    let left = bounds.left().floor();
    let top = bounds.top().floor();
    let width = (bounds.right().ceil() - left) as i32;
    let height = (bounds.bottom().ceil() - top) as i32;
    if width <= 0 || height <= 0 || width > 4096 || height > 4096 {
        return None;
    }
    let mut mask = Mask::new(width as u32, height as u32)?;
    mask.fill_path(
        &path,
        FillRule::Winding,
        true,
        Transform::from_translate(-left, -top),
    );
    Some(GlyphRaster {
        width: width as u32,
        height: height as u32,
        left,
        top,
        data: mask.data().to_vec(),
    })
}

#[derive(Clone)]
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
#[allow(clippy::too_many_arguments)]
pub fn render_page(
    scene: &Scene,
    width: u32,
    height: u32,
    page_top: f32,
    clip_left: f32,
    clip_right: f32,
    clip_top: f32,
    clip_bottom: f32,
    background: u32,
    font_gamma: f32,
) -> (Arc<PageBitmap>, usize, usize) {
    #[cfg(feature = "vello-cpu")]
    if vello_backend_requested() {
        return render_page_vello(
            scene,
            width,
            height,
            page_top,
            clip_left,
            clip_right,
            clip_top,
            clip_bottom,
            background,
        );
    }
    let page_transform = Affine::translate((0.0, -page_top as f64));
    let local_top = (clip_top - page_top).clamp(0.0, height as f32);
    let local_bottom = (clip_bottom - page_top).clamp(0.0, height as f32);
    let clip_mask = page_clip_mask(
        width,
        height,
        clip_left,
        clip_right,
        local_top,
        local_bottom,
    );
    let (rasterizer, image_count, image_surface) = raster::Rasterizer::new(
        scene,
        width,
        height,
        page_transform,
        local_top,
        local_bottom,
        font_gamma,
        background,
        clip_mask,
    );
    let pixmap = rasterizer.run(scene);

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
        Arc::new(PageBitmap {
            width,
            height,
            data,
        }),
        image_count,
        image_surface,
    )
}

#[allow(clippy::type_complexity)]
pub fn image_draw_stats_and_rects_identity(
    scene: &Scene,
    width: u32,
    height: u32,
    local_top: f32,
    local_bottom: f32,
) -> (usize, usize, Vec<(f32, f32, f32, f32)>) {
    image_draw_stats_and_rects(
        scene,
        width,
        height,
        Affine::scale(1.0),
        local_top,
        local_bottom,
    )
}

#[allow(clippy::type_complexity)]
pub fn image_draw_stats_and_rects(
    scene: &Scene,
    width: u32,
    height: u32,
    page_transform: Affine,
    local_top: f32,
    local_bottom: f32,
) -> (usize, usize, Vec<(f32, f32, f32, f32)>) {
    let viewport_width = width as f32;
    let viewport_top = local_top.max(0.0);
    let viewport_bottom = local_bottom.min(height as f32);
    let mut count = 0usize;
    let mut surface = 0usize;
    let mut rects: Vec<(f32, f32, f32, f32)> = Vec::new();
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
            rects.push((
                visible_left.floor(),
                visible_top.floor(),
                visible_right.ceil(),
                visible_bottom.ceil(),
            ));
        }
    }
    (count, surface, rects)
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

fn page_clip_mask(
    width: u32,
    height: u32,
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
) -> Option<Mask> {
    // Clip slightly inward so a fractional page edge cannot expose the layer
    // behind it (for example the red solid under a white pattern).
    let left = left.ceil().max(0.0);
    let right = right.floor().min(width as f32);
    let top = top.ceil().max(0.0);
    let bottom = bottom.floor().min(height as f32);
    if left <= 0.0 && right >= width as f32 && top <= 0.0 && bottom >= height as f32 {
        return None;
    }
    let mut mask = Mask::new(width, height)?;
    let mut builder = PathBuilder::new();
    // An empty rectangle leaves the mask zeroed, which clips everything.
    if let Some(rect) = tiny_skia::Rect::from_ltrb(left, top, right, bottom) {
        builder.push_rect(rect);
        let path = builder.finish()?;
        // A hard edge is important: with anti-aliasing, two stacked layers
        // (a red background and a white pattern) each blend with the page
        // background at the clip boundary, leaving a coloured fringe.
        mask.fill_path(&path, FillRule::Winding, false, Transform::identity());
    }
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
    brush: &Paint,
    brush_transform: Option<Affine>,
    style: &Stroke,
    clip_mask: Option<&Mask>,
) {
    let Some(path) = to_skia_path(shape, transform) else {
        return;
    };
    let dash = if style.dash_pattern.len() >= 2 {
        let array: Vec<f32> = style
            .dash_pattern
            .iter()
            .map(|value| *value as f32)
            .collect();
        tiny_skia::StrokeDash::new(array, style.dash_offset as f32)
    } else {
        None
    };
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
        dash,
    };
    match brush {
        Paint::Solid(color) => {
            let paint = skia_paint(*color);
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), clip_mask);
        }
        Paint::Gradient(gradient) => {
            let brush_transform = brush_transform.unwrap_or(Affine::IDENTITY);
            if let Some(shader) = gradient_shader(gradient, transform * brush_transform) {
                let paint = SkPaint {
                    shader,
                    anti_alias: true,
                    ..Default::default()
                };
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), clip_mask);
            }
        }
        Paint::Image(image) => {
            let brush_transform = brush_transform.unwrap_or(Affine::IDENTITY);
            let sampler = &image.sampler;
            let Some(premultiplied) = premultiplied_rgba(&image.image) else {
                return;
            };
            let Some(image_pixmap) = PixmapRef::from_bytes(
                premultiplied.as_ref(),
                image.image.width,
                image.image.height,
            ) else {
                return;
            };
            let shader = tiny_skia::Pattern::new(
                image_pixmap,
                spread_mode(sampler.x_extend),
                filter_quality(sampler.quality),
                sampler.alpha,
                kurbo_to_skia(transform * brush_transform),
            );
            let paint = SkPaint {
                shader,
                anti_alias: true,
                ..Default::default()
            };
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), clip_mask);
        }
        _ => {}
    }
}

/// Maps glyph coverage bytes through the font-contrast gamma in one place:
/// the composite of black text covers the background proportionally to
/// `1 - coverage`, so post-compositing `out' = out^gamma` is exactly
/// equivalent to painting with the effective coverage `1 - (1 - coverage)^gamma`.
/// Doing it here covers every pixel of the glyph (including anti-aliased edges
/// that extend past line boxes), keeps the ink in glyph space, and never
/// touches images, strokes or fills.
fn glyph_coverage_lut(gamma: f32) -> [u8; 256] {
    let mut lut = [0u8; 256];
    let identity = (gamma - 1.0).abs() <= 1e-6 || !gamma.is_finite();
    for (byte, entry) in lut.iter_mut().enumerate() {
        let coverage = byte as f32 / 255.0;
        let effective = if identity {
            coverage
        } else {
            let gamma = gamma.clamp(0.1, 15.0);
            1.0 - (1.0 - coverage).powf(gamma)
        };
        *entry = ((effective * 255.0) + 0.5).clamp(0.0, 255.0) as u8;
    }
    lut
}
#[allow(clippy::too_many_arguments)]
fn draw_glyphs<'a>(
    pixmap: &mut Pixmap,
    font_data: &'a peniko::FontData,
    font_size: f32,
    glyphs: &[anyrender::Glyph],
    transform: Affine,
    glyph_transform: Option<Affine>,
    color: Color,
    brush_alpha: f32,
    clip_top: f32,
    clip_bottom: f32,
    clip_mask: Option<&Mask>,
    font_gamma: f32,
    faces: &mut HashMap<usize, ttf_parser::Face<'a>>,
    glyph_cache: &mut HashMap<GlyphKey, GlyphBitmap>,
) {
    let rgba = color.to_rgba8();
    let alpha = (rgba.a as f32 / 255.0) * brush_alpha;
    let width = pixmap.width();
    for_each_glyph_pixel(
        width,
        pixmap.height(),
        font_data,
        font_size,
        glyphs,
        transform,
        glyph_transform,
        clip_top,
        clip_bottom,
        font_gamma,
        faces,
        glyph_cache,
        |x, y, coverage| {
            let mask_a = clip_mask.map_or(1.0, |mask| {
                mask.data()[(y * width + x) as usize] as f32 / 255.0
            });
            if mask_a <= 0.0 {
                return;
            }
            blend_pixel(
                pixmap,
                x,
                y,
                rgba.r,
                rgba.g,
                rgba.b,
                coverage * alpha * mask_a,
            );
        },
    );
}

/// Rasterises the glyph coverage once, then fills it with a gradient or image
/// brush. Tiny-skia has no "glyph run with shader" primitive, so the coverage
/// becomes a mask on a full-page rectangle.
#[allow(clippy::too_many_arguments)]
fn draw_glyphs_with_brush<'a>(
    pixmap: &mut Pixmap,
    font_data: &'a peniko::FontData,
    font_size: f32,
    glyphs: &[anyrender::Glyph],
    transform: Affine,
    glyph_transform: Option<Affine>,
    brush: &Paint,
    brush_transform: Option<Affine>,
    brush_alpha: f32,
    clip_top: f32,
    clip_bottom: f32,
    clip_mask: Option<&Mask>,
    font_gamma: f32,
    faces: &mut HashMap<usize, ttf_parser::Face<'a>>,
    glyph_cache: &mut HashMap<GlyphKey, GlyphBitmap>,
) {
    let (width, height) = (pixmap.width(), pixmap.height());
    let Some(mut mask) = Mask::new(width, height) else {
        return;
    };
    for_each_glyph_pixel(
        width,
        height,
        font_data,
        font_size,
        glyphs,
        transform,
        glyph_transform,
        clip_top,
        clip_bottom,
        font_gamma,
        faces,
        glyph_cache,
        |x, y, coverage| {
            let alpha = (coverage * brush_alpha * 255.0).clamp(0.0, 255.0) as u8;
            let slot = &mut mask.data_mut()[(y * width + x) as usize];
            *slot = (*slot).max(alpha);
        },
    );
    let mask = raster::intersect_optional(clip_mask, Some(mask));
    let clip_mask = mask.as_ref();
    let Some(page) = raster::full_page_path(width, height) else {
        return;
    };
    let brush_transform = brush_transform.unwrap_or(Affine::IDENTITY);
    match brush {
        Paint::Gradient(gradient) => {
            let Some(shader) = gradient_shader(gradient, transform * brush_transform) else {
                return;
            };
            let paint = SkPaint {
                shader,
                anti_alias: false,
                ..Default::default()
            };
            pixmap.fill_path(
                &page,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                clip_mask,
            );
        }
        Paint::Image(image) => {
            let sampler = &image.sampler;
            let Some(premultiplied) = premultiplied_rgba(&image.image) else {
                return;
            };
            let Some(image_pixmap) = PixmapRef::from_bytes(
                premultiplied.as_ref(),
                image.image.width,
                image.image.height,
            ) else {
                return;
            };
            let shader = tiny_skia::Pattern::new(
                image_pixmap,
                spread_mode(sampler.x_extend),
                filter_quality(sampler.quality),
                sampler.alpha,
                kurbo_to_skia(transform * brush_transform),
            );
            let paint = SkPaint {
                shader,
                anti_alias: false,
                ..Default::default()
            };
            pixmap.fill_path(
                &page,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                clip_mask,
            );
        }
        _ => {}
    }
}

/// Walks every covered pixel of a glyph run, reporting the gamma-mapped
/// coverage in `0..=1`. Shared by the solid and shader glyph paths so glyph
/// placement and caching stay in one place.
#[allow(clippy::too_many_arguments)]
fn for_each_glyph_pixel<'a>(
    width: u32,
    height: u32,
    font_data: &'a peniko::FontData,
    font_size: f32,
    glyphs: &[anyrender::Glyph],
    transform: Affine,
    glyph_transform: Option<Affine>,
    clip_top: f32,
    clip_bottom: f32,
    font_gamma: f32,
    faces: &mut HashMap<usize, ttf_parser::Face<'a>>,
    glyph_cache: &mut HashMap<GlyphKey, GlyphBitmap>,
    mut emit: impl FnMut(u32, u32, f32),
) {
    let coverage_lut = glyph_coverage_lut(font_gamma);
    let data = font_data.data.data();
    let key = data.as_ptr() as usize ^ font_data.index as usize;
    let face = faces.entry(key).or_insert_with(|| {
        ttf_parser::Face::parse(data, font_data.index).expect("parse font data")
    });

    let scale = transform.as_coeffs();
    let scale_x = (scale[0] * scale[0] + scale[1] * scale[1]).sqrt() as f32;
    let scale_y = (scale[2] * scale[2] + scale[3] * scale[3]).sqrt() as f32;
    let glyph_size = font_size * scale_y.max(scale_x);
    if glyph_size <= 0.0 {
        return;
    }

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

        let cache_key: GlyphKey = (key, glyph.id as u16, (glyph_size * 4.0) as u32);
        let raster = glyph_cache
            .entry(cache_key)
            .or_insert_with(|| Arc::new(rasterize_glyph(face, glyph.id as u16, glyph_size)))
            .clone();
        let Some(raster) = raster.as_ref() else {
            continue;
        };

        let x0 = origin.x as f32 + raster.left;
        let y0 = origin.y as f32 + raster.top;
        if (y0 + raster.height as f32) < clip_top || y0 > clip_bottom {
            continue;
        }

        for row in 0..raster.height {
            for col in 0..raster.width {
                let coverage_byte = raster.data[(row * raster.width + col) as usize];
                if coverage_byte == 0 {
                    continue;
                }
                let coverage = coverage_lut[coverage_byte as usize] as f32 / 255.0;
                let x = x0.round() as i32 + col as i32;
                let y = y0.round() as i32 + row as i32;
                if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
                    continue;
                }
                if (y as f32) < clip_top || (y as f32) >= clip_bottom {
                    continue;
                }
                emit(x as u32, y as u32, coverage);
            }
        }
    }
}

fn blend_pixel(pixmap: &mut Pixmap, x: u32, y: u32, r: u8, g: u8, b: u8, alpha: f32) {
    let width = pixmap.width();
    blend_pixel_data(pixmap.data_mut(), width, x, y, r, g, b, alpha);
}

#[allow(clippy::too_many_arguments)]
fn blend_pixel_data(data: &mut [u8], width: u32, x: u32, y: u32, r: u8, g: u8, b: u8, alpha: f32) {
    if alpha <= 0.0 {
        return;
    }
    let idx = ((y * width + x) * 4) as usize;
    let src_a = alpha.clamp(0.0, 1.0);
    if src_a >= 1.0 {
        data[idx] = r;
        data[idx + 1] = g;
        data[idx + 2] = b;
        data[idx + 3] = 255;
        return;
    }
    let dst_a = data[idx + 3] as f32 / 255.0;
    if dst_a >= 1.0 {
        // Opaque destination: source-over without the divide.
        let inv = 1.0 - src_a;
        for (offset, src) in [(0usize, r), (1, g), (2, b)] {
            let dst = data[idx + offset] as f32;
            let out = src as f32 * src_a + dst * inv;
            data[idx + offset] = out.clamp(0.0, 255.0) as u8;
        }
        return;
    }
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

fn kurbo_to_skia(transform: Affine) -> Transform {
    let c = transform.as_coeffs();
    Transform::from_row(
        c[0] as f32,
        c[1] as f32,
        c[2] as f32,
        c[3] as f32,
        c[4] as f32,
        c[5] as f32,
    )
}

/// Adapts a peniko gradient (the brush format Blitz records) to tiny-skia's
/// shader. All coordinates are in the same space as the path before
/// `transform`, so the transform is applied to the shader as well.
fn gradient_shader(gradient: &Gradient, transform: Affine) -> Option<Shader<'static>> {
    let stops: Vec<SkGradientStop> = gradient
        .stops
        .iter()
        .map(|stop| {
            let rgba = stop
                .color
                .to_alpha_color::<peniko::color::Srgb>()
                .to_rgba8();
            SkGradientStop::new(
                stop.offset,
                tiny_skia::Color::from_rgba8(rgba.r, rgba.g, rgba.b, rgba.a),
            )
        })
        .collect();
    if stops.is_empty() {
        return None;
    }
    let spread = match gradient.extend {
        PenikoExtend::Pad => SpreadMode::Pad,
        PenikoExtend::Repeat => SpreadMode::Repeat,
        PenikoExtend::Reflect => SpreadMode::Reflect,
    };
    let transform = kurbo_to_skia(transform);
    match &gradient.kind {
        GradientKind::Linear(position) => SkLinearGradient::new(
            tiny_skia::Point::from_xy(position.start.x as f32, position.start.y as f32),
            tiny_skia::Point::from_xy(position.end.x as f32, position.end.y as f32),
            stops,
            spread,
            transform,
        ),
        GradientKind::Radial(position) => {
            let start = tiny_skia::Point::from_xy(
                position.start_center.x as f32,
                position.start_center.y as f32,
            );
            let end = tiny_skia::Point::from_xy(
                position.end_center.x as f32,
                position.end_center.y as f32,
            );
            SkRadialGradient::new(
                start,
                end,
                position.end_radius.max(0.0001),
                stops,
                spread,
                transform,
            )
        }
        // tiny-skia has no sweep shader; the caller falls back to
        // `draw_sweep_fill`, which evaluates the angle per pixel.
        GradientKind::Sweep(_) => None,
    }
}

/// Splits a peniko gradient into ordered `(offset, straight RGBA)` stops.
fn gradient_stop_list(gradient: &Gradient) -> Vec<(f32, [u8; 4])> {
    gradient
        .stops
        .iter()
        .map(|stop| {
            let rgba = stop
                .color
                .to_alpha_color::<peniko::color::Srgb>()
                .to_rgba8();
            (stop.offset, [rgba.r, rgba.g, rgba.b, rgba.a])
        })
        .collect()
}

fn interpolate_rgba(from: [u8; 4], to: [u8; 4], t: f32) -> [u8; 4] {
    let lerp = |a: u8, b: u8| {
        (f32::from(a) * (1.0 - t) + f32::from(b) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    [
        lerp(from[0], to[0]),
        lerp(from[1], to[1]),
        lerp(from[2], to[2]),
        lerp(from[3], to[3]),
    ]
}

fn color_at_offset(stops: &[(f32, [u8; 4])], t: f32) -> [u8; 4] {
    let Some(first) = stops.first().copied() else {
        return [0, 0, 0, 0];
    };
    if t <= first.0 {
        return first.1;
    }
    for pair in stops.windows(2) {
        let (start, from) = pair[0];
        let (end, to) = pair[1];
        if t <= end {
            if end <= start {
                return to;
            }
            return interpolate_rgba(from, to, (t - start) / (end - start));
        }
    }
    stops.last().map(|stop| stop.1).unwrap_or([0, 0, 0, 0])
}

/// tiny-skia has no sweep (conic) shader, so conic gradients are filled by
/// evaluating the angle per pixel. Slower than a shader, but conic gradients
/// are rare and the alternative is dropping the fill entirely.
#[allow(clippy::too_many_arguments)]
fn draw_sweep_fill(
    pixmap: &mut Pixmap,
    shape: &BezPath,
    fill: AnyFill,
    fill_transform: Affine,
    gradient: &Gradient,
    brush_transform: Option<Affine>,
    clip_mask: Option<&Mask>,
) {
    let GradientKind::Sweep(sweep) = &gradient.kind else {
        return;
    };
    let stops = gradient_stop_list(gradient);
    if stops.is_empty() {
        return;
    }
    let shape_mask =
        raster::shape_mask(shape, fill_transform, fill, pixmap.width(), pixmap.height());
    let mask = raster::intersect_optional(clip_mask, shape_mask);
    let mask_data = mask.as_ref().map(|mask| mask.data());
    let Some(bounds) = device_bounds(shape, fill_transform, pixmap.width(), pixmap.height()) else {
        return;
    };
    let to_gradient = (fill_transform * brush_transform.unwrap_or(Affine::IDENTITY)).inverse();
    let center = sweep.center;
    let span = sweep.end_angle - sweep.start_angle;
    if !span.is_finite() || span.abs() < 1e-6 {
        return;
    }
    let page_width = pixmap.width();
    let data = pixmap.data_mut();
    for py in bounds.1..bounds.3 {
        let mask_row = mask_data.map(|_| py as usize * page_width as usize);
        for px in bounds.0..bounds.2 {
            let mask_a = match (mask_data, mask_row) {
                (Some(mask), Some(row_start)) => mask[row_start + px as usize] as f32 / 255.0,
                _ => 1.0,
            };
            if mask_a <= 0.0 {
                continue;
            }
            let p = to_gradient * Point::new(px as f64 + 0.5, py as f64 + 0.5);
            let angle = (p.y - center.y).atan2(p.x - center.x) as f32;
            let rel = (angle - sweep.start_angle).rem_euclid(std::f32::consts::TAU);
            let t = rel / span.abs();
            let t = match gradient.extend {
                PenikoExtend::Pad => t,
                PenikoExtend::Repeat => t.rem_euclid(1.0),
                PenikoExtend::Reflect => {
                    let t = t.rem_euclid(2.0);
                    if t > 1.0 { 2.0 - t } else { t }
                }
            };
            let rgba = color_at_offset(&stops, t);
            let alpha = (f32::from(rgba[3]) / 255.0) * mask_a;
            if alpha <= 0.0 {
                continue;
            }
            blend_pixel_data(
                data, page_width, px as u32, py as u32, rgba[0], rgba[1], rgba[2], alpha,
            );
        }
    }
}

/// Device-space integer bounds `(x0, y0, x1, y1)` for a transformed shape,
/// clipped to the page.
fn device_bounds(
    shape: &BezPath,
    transform: Affine,
    width: u32,
    height: u32,
) -> Option<(i32, i32, i32, i32)> {
    let raw = shape.bounding_box();
    let corners = [
        transform * Point::new(raw.x0, raw.y0),
        transform * Point::new(raw.x1, raw.y0),
        transform * Point::new(raw.x0, raw.y1),
        transform * Point::new(raw.x1, raw.y1),
    ];
    let x0 = corners.iter().map(|p| p.x).fold(f64::INFINITY, f64::min);
    let y0 = corners.iter().map(|p| p.y).fold(f64::INFINITY, f64::min);
    let x1 = corners
        .iter()
        .map(|p| p.x)
        .fold(f64::NEG_INFINITY, f64::max);
    let y1 = corners
        .iter()
        .map(|p| p.y)
        .fold(f64::NEG_INFINITY, f64::max);
    let x0 = x0.max(0.0).min(width as f64) as i32;
    let x1 = x1.max(0.0).min(width as f64) as i32;
    let y0 = y0.max(0.0).min(height as f64) as i32;
    let y1 = y1.max(0.0).min(height as f64) as i32;
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    Some((x0, y0, x1, y1))
}

fn draw_fill_shader(
    pixmap: &mut Pixmap,
    shape: &BezPath,
    transform: Affine,
    shader: Shader<'static>,
    fill: AnyFill,
    clip_mask: Option<&Mask>,
) {
    let Some(path) = to_skia_path(shape, transform) else {
        return;
    };
    let paint = SkPaint {
        shader,
        anti_alias: true,
        ..Default::default()
    };
    let rule = match fill {
        AnyFill::NonZero => FillRule::Winding,
        AnyFill::EvenOdd => FillRule::EvenOdd,
    };
    pixmap.fill_path(&path, &paint, rule, Transform::identity(), clip_mask);
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
    brush: &peniko::ImageBrush,
    shape: &BezPath,
    fill: AnyFill,
    fill_transform: Affine,
    brush_transform: Option<Affine>,
    clip_mask: Option<&Mask>,
) {
    let image = &brush.image;
    let sampler = &brush.sampler;
    if sampler.alpha <= 0.0 || image.width == 0 || image.height == 0 {
        return;
    }
    let Some(fill_rect) = device_rect_path(shape, fill_transform, pixmap.width(), pixmap.height())
    else {
        return;
    };
    let shape_mask =
        raster::shape_mask(shape, fill_transform, fill, pixmap.width(), pixmap.height());
    let mask = raster::intersect_optional(clip_mask, shape_mask);
    let brush_transform = brush_transform.unwrap_or(Affine::IDENTITY);
    let transform = fill_transform * brush_transform;

    // tiny-skia's pattern blitter is faster than a per-pixel loop, and covers
    // sampling quality plus pad/repeat/reflect. It only has one spread mode
    // for both axes, so unequal x/y extends fall back to the manual sampler.
    let premultiplied = if sampler.x_extend == sampler.y_extend {
        premultiplied_rgba(image)
    } else {
        None
    };
    if let Some(premultiplied) = premultiplied {
        let Some(image_pixmap) =
            PixmapRef::from_bytes(premultiplied.as_ref(), image.width, image.height)
        else {
            return;
        };
        let shader = tiny_skia::Pattern::new(
            image_pixmap,
            spread_mode(sampler.x_extend),
            filter_quality(sampler.quality),
            sampler.alpha,
            kurbo_to_skia(transform),
        );
        let paint = SkPaint {
            shader,
            anti_alias: true,
            ..Default::default()
        };
        pixmap.fill_path(
            &fill_rect,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            mask.as_ref(),
        );
        return;
    }

    let Some(pixels) = straight_rgba(image) else {
        return;
    };
    let inv_fill = transform.inverse();
    let to_image = inv_fill;

    let mask_data = mask.as_ref().map(|mask| mask.data());
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
    let y0 = by0.max(0.0).min(pixmap.height() as f64) as i32;
    let y1 = by1.max(0.0).min(pixmap.height() as f64) as i32;
    if x0 >= x1 || y0 >= y1 {
        return;
    }

    // Walk rows with an incremental inverse map: one add per pixel instead of
    // a full affine transform.
    let coeffs = to_image.as_coeffs();
    let (image_width, image_height) = (image.width, image.height);
    let page_width = pixmap.width();
    let data = pixmap.data_mut();
    for py in y0..y1 {
        let row = py as f64 + 0.5;
        let mut ix = coeffs[0] * (x0 as f64 + 0.5) + coeffs[2] * row + coeffs[4];
        let mut iy = coeffs[1] * (x0 as f64 + 0.5) + coeffs[3] * row + coeffs[5];
        let mask_row = mask_data.map(|_| py as usize * page_width as usize);
        for px in x0..x1 {
            let mask_a = match (mask_data, mask_row) {
                (Some(mask), Some(row_start)) => mask[row_start + px as usize] as f32 / 255.0,
                _ => 1.0,
            };
            let (r, g, b, a) = sample_image(
                pixels.as_ref(),
                image_width,
                image_height,
                ix as f32,
                iy as f32,
                sampler,
            );
            ix += coeffs[0];
            iy += coeffs[1];
            let src_a = (f32::from(a) / 255.0) * sampler.alpha * mask_a;
            if src_a <= 0.0 {
                continue;
            }
            blend_pixel_data(data, page_width, px as u32, py as u32, r, g, b, src_a);
        }
    }
}

/// Converts an image to straight (non-premultiplied) RGBA8. Borrows when the
/// source already matches, so the common RGBA upload avoids a copy.
fn straight_rgba(image: &peniko::ImageData) -> Option<std::borrow::Cow<'_, [u8]>> {
    let needed = (image.width as usize)
        .checked_mul(image.height as usize)?
        .checked_mul(4)?;
    let raw = image.data.data();
    if raw.len() < needed {
        return None;
    }
    let raw = &raw[..needed];
    let premultiplied = image.alpha_type == peniko::ImageAlphaType::AlphaPremultiplied;
    match image.format {
        ImageFormat::Rgba8 if !premultiplied => Some(std::borrow::Cow::Borrowed(raw)),
        ImageFormat::Rgba8 => {
            let mut out = raw.to_vec();
            unpremultiply(&mut out);
            Some(std::borrow::Cow::Owned(out))
        }
        ImageFormat::Bgra8 => {
            let mut out = raw.to_vec();
            for pixel in out.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
            if premultiplied {
                unpremultiply(&mut out);
            }
            Some(std::borrow::Cow::Owned(out))
        }
        _ => None,
    }
}

fn unpremultiply(data: &mut [u8]) {
    for pixel in data.as_chunks_mut::<4>().0 {
        let alpha = u32::from(pixel[3]);
        if alpha == 0 {
            pixel[0] = 0;
            pixel[1] = 0;
            pixel[2] = 0;
        } else if alpha != 255 {
            for channel in &mut pixel[..3] {
                *channel = (u32::from(*channel) * 255 / alpha).min(255) as u8;
            }
        }
    }
}

fn sample_index(value: i32, len: u32, mode: PenikoExtend) -> u32 {
    let len = len as i32;
    match mode {
        PenikoExtend::Pad => value.clamp(0, len - 1) as u32,
        PenikoExtend::Repeat => value.rem_euclid(len) as u32,
        PenikoExtend::Reflect => {
            let value = value.rem_euclid(2 * len);
            if value >= len {
                (2 * len - 1 - value) as u32
            } else {
                value as u32
            }
        }
    }
}

#[inline]
fn sample_image(
    data: &[u8],
    width: u32,
    height: u32,
    x: f32,
    y: f32,
    sampler: &peniko::ImageSampler,
) -> (u8, u8, u8, u8) {
    let read = |x: i32, y: i32| {
        let ix = sample_index(x, width, sampler.x_extend);
        let iy = sample_index(y, height, sampler.y_extend);
        let index = ((iy * width + ix) as usize) * 4;
        [
            data[index],
            data[index + 1],
            data[index + 2],
            data[index + 3],
        ]
    };
    if sampler.quality == peniko::ImageQuality::Low {
        let p = read(x.floor() as i32, y.floor() as i32);
        return (p[0], p[1], p[2], p[3]);
    }
    let fx = x - 0.5;
    let fy = y - 0.5;
    let x0 = fx.floor() as i32;
    let y0 = fy.floor() as i32;
    let tx = fx - x0 as f32;
    let ty = fy - y0 as f32;
    let p00 = read(x0, y0);
    let p10 = read(x0 + 1, y0);
    let p01 = read(x0, y0 + 1);
    let p11 = read(x0 + 1, y0 + 1);
    let lerp = |a: u8, b: u8, t: f32| {
        (f32::from(a) * (1.0 - t) + f32::from(b) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    let top = [
        lerp(p00[0], p10[0], tx),
        lerp(p00[1], p10[1], tx),
        lerp(p00[2], p10[2], tx),
        lerp(p00[3], p10[3], tx),
    ];
    let bottom = [
        lerp(p01[0], p11[0], tx),
        lerp(p01[1], p11[1], tx),
        lerp(p01[2], p11[2], tx),
        lerp(p01[3], p11[3], tx),
    ];
    (
        lerp(top[0], bottom[0], ty),
        lerp(top[1], bottom[1], ty),
        lerp(top[2], bottom[2], ty),
        lerp(top[3], bottom[3], ty),
    )
}

/// Conservative device-space rectangle around a transformed shape. Used to
/// restrict image fills to the area they can cover instead of the whole page.
fn device_rect_path(
    shape: &BezPath,
    transform: Affine,
    width: u32,
    height: u32,
) -> Option<tiny_skia::Path> {
    let raw = shape.bounding_box();
    let corners = [
        transform * Point::new(raw.x0, raw.y0),
        transform * Point::new(raw.x1, raw.y0),
        transform * Point::new(raw.x0, raw.y1),
        transform * Point::new(raw.x1, raw.y1),
    ];
    let mut x0 = corners.iter().map(|p| p.x).fold(f64::INFINITY, f64::min);
    let mut y0 = corners.iter().map(|p| p.y).fold(f64::INFINITY, f64::min);
    let mut x1 = corners
        .iter()
        .map(|p| p.x)
        .fold(f64::NEG_INFINITY, f64::max);
    let mut y1 = corners
        .iter()
        .map(|p| p.y)
        .fold(f64::NEG_INFINITY, f64::max);
    x0 = x0.max(0.0);
    y0 = y0.max(0.0);
    x1 = x1.min(width as f64);
    y1 = y1.min(height as f64);
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    let mut builder = PathBuilder::new();
    builder.push_rect(tiny_skia::Rect::from_ltrb(
        x0 as f32, y0 as f32, x1 as f32, y1 as f32,
    )?);
    builder.finish()
}

/// Returns the image as premultiplied RGBA8. Borrows when the source already
/// is (opaque straight alpha counts, since premultiplication is a no-op).
fn premultiplied_rgba(image: &peniko::ImageData) -> Option<std::borrow::Cow<'_, [u8]>> {
    let needed = (image.width as usize)
        .checked_mul(image.height as usize)?
        .checked_mul(4)?;
    let raw = image.data.data();
    if raw.len() < needed {
        return None;
    }
    let raw = &raw[..needed];
    let already_premultiplied = image.alpha_type == peniko::ImageAlphaType::AlphaPremultiplied;
    match image.format {
        ImageFormat::Rgba8 if already_premultiplied || is_opaque(raw) => {
            Some(std::borrow::Cow::Borrowed(raw))
        }
        ImageFormat::Rgba8 => {
            let mut out = raw.to_vec();
            premultiply(&mut out);
            Some(std::borrow::Cow::Owned(out))
        }
        ImageFormat::Bgra8 => {
            let mut out = raw.to_vec();
            for pixel in out.as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
            if !already_premultiplied {
                premultiply(&mut out);
            }
            Some(std::borrow::Cow::Owned(out))
        }
        _ => None,
    }
}

fn is_opaque(data: &[u8]) -> bool {
    data.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 255)
}

fn premultiply(data: &mut [u8]) {
    for pixel in data.as_chunks_mut::<4>().0 {
        let alpha = u16::from(pixel[3]);
        if alpha == 255 {
            continue;
        }
        for channel in &mut pixel[..3] {
            *channel = (u16::from(*channel) * alpha / 255) as u8;
        }
    }
}

fn spread_mode(mode: PenikoExtend) -> SpreadMode {
    match mode {
        PenikoExtend::Pad => SpreadMode::Pad,
        PenikoExtend::Repeat => SpreadMode::Repeat,
        PenikoExtend::Reflect => SpreadMode::Reflect,
    }
}

fn filter_quality(quality: peniko::ImageQuality) -> tiny_skia::FilterQuality {
    match quality {
        peniko::ImageQuality::Low => tiny_skia::FilterQuality::Nearest,
        peniko::ImageQuality::Medium | peniko::ImageQuality::High => {
            tiny_skia::FilterQuality::Bilinear
        }
    }
}

#[derive(Clone)]
pub struct ColorPageBitmap {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl ColorPageBitmap {
    pub fn new(width: u32, height: u32, background: u32) -> Self {
        let pixel = [
            ((background >> 16) & 0xff) as u8,
            ((background >> 8) & 0xff) as u8,
            (background & 0xff) as u8,
            0xffu8,
        ];
        let data = pixel
            .iter()
            .copied()
            .cycle()
            .take((width * height * 4) as usize)
            .collect();
        Self {
            width,
            height,
            data,
        }
    }
}
#[allow(clippy::too_many_arguments)]
pub fn render_page_rgba(
    scene: &Scene,
    width: u32,
    height: u32,
    page_top: f32,
    clip_left: f32,
    clip_right: f32,
    clip_top: f32,
    clip_bottom: f32,
    background: u32,
    font_gamma: f32,
) -> (Arc<ColorPageBitmap>, usize, usize) {
    #[cfg(feature = "vello-cpu")]
    if vello_backend_requested() {
        return render_page_rgba_vello(
            scene,
            width,
            height,
            page_top,
            clip_left,
            clip_right,
            clip_top,
            clip_bottom,
            background,
        );
    }
    let page_transform = Affine::translate((0.0, -page_top as f64));
    let local_top = (clip_top - page_top).clamp(0.0, height as f32);
    let local_bottom = (clip_bottom - page_top).clamp(0.0, height as f32);
    let clip_mask = page_clip_mask(
        width,
        height,
        clip_left,
        clip_right,
        local_top,
        local_bottom,
    );
    let (rasterizer, image_count, image_surface) = raster::Rasterizer::new(
        scene,
        width,
        height,
        page_transform,
        local_top,
        local_bottom,
        font_gamma,
        background,
        clip_mask,
    );
    let pixmap = rasterizer.run(scene);

    let mut data: Vec<u8> = Vec::with_capacity((width * height * 4) as usize);
    for pixel in pixmap.pixels() {
        data.push(pixel.red());
        data.push(pixel.green());
        data.push(pixel.blue());
        data.push(pixel.alpha());
    }

    (
        Arc::new(ColorPageBitmap {
            width,
            height,
            data,
        }),
        image_count,
        image_surface,
    )
}

#[cfg(feature = "vello-cpu")]
fn vello_backend_requested() -> bool {
    // A build with the `vello-cpu` feature defaults to the vello backend, so a
    // dedicated device module works without setting an environment variable.
    // Set `CRE_PAINT_BACKEND=tiny` to fall back to tiny-skia at runtime.
    match std::env::var("CRE_PAINT_BACKEND") {
        Ok(value) => value.eq_ignore_ascii_case("vello"),
        Err(_) => true,
    }
}

#[cfg(feature = "vello-cpu")]
fn vello_paint_ref(paint: &Paint) -> anyrender::PaintRef<'_> {
    match paint {
        Paint::Solid(color) => anyrender::Paint::Solid(*color),
        Paint::Gradient(gradient) => anyrender::Paint::Gradient(gradient),
        Paint::Image(image) => anyrender::Paint::Image(image.as_ref()),
        Paint::Resource(id) => anyrender::Paint::Resource(*id),
        Paint::Custom(custom) => anyrender::Paint::Custom(custom.as_ref()),
    }
}

#[cfg(feature = "vello-cpu")]
fn replay_scene(
    painter: &mut anyrender_vello_cpu::VelloCpuScenePainter,
    scene: &Scene,
    scene_transform: Affine,
) {
    use anyrender::PaintScene as _;
    for command in &scene.commands {
        match command {
            RenderCommand::PushLayer(cmd) => painter.push_layer(
                cmd.fill,
                cmd.blend,
                cmd.alpha,
                scene_transform * cmd.transform,
                &cmd.clip,
                cmd.filter.clone(),
                cmd.backdrop_filter.clone(),
            ),
            RenderCommand::PushClipLayer(cmd) => {
                painter.push_clip_layer(cmd.fill, scene_transform * cmd.transform, &cmd.clip);
            }
            RenderCommand::PopLayer => painter.pop_layer(),
            RenderCommand::Stroke(cmd) => painter.stroke(
                &cmd.style,
                scene_transform * cmd.transform,
                vello_paint_ref(&cmd.brush),
                cmd.brush_transform,
                &cmd.shape,
            ),
            RenderCommand::Fill(cmd) => painter.fill(
                cmd.fill,
                scene_transform * cmd.transform,
                vello_paint_ref(&cmd.brush),
                cmd.brush_transform,
                &cmd.shape,
            ),
            RenderCommand::GlyphRun(cmd) => painter.draw_glyphs(
                &cmd.font_data,
                cmd.font_size,
                cmd.hint,
                &cmd.normalized_coords,
                cmd.embolden,
                &cmd.style,
                vello_paint_ref(&cmd.brush),
                cmd.brush_alpha,
                scene_transform * cmd.transform,
                cmd.glyph_transform,
                cmd.glyphs.iter().copied(),
            ),
            RenderCommand::BoxShadow(cmd) => painter.draw_box_shadow(
                scene_transform * cmd.transform,
                cmd.rect,
                cmd.brush,
                cmd.radius,
                cmd.std_dev,
            ),
        }
    }
}

#[cfg(feature = "vello-cpu")]
#[allow(clippy::too_many_arguments)]
fn prepare_vello_scene(
    scene: &Scene,
    width: u32,
    height: u32,
    page_top: f32,
    clip_left: f32,
    clip_right: f32,
    clip_top: f32,
    clip_bottom: f32,
    background: u32,
) -> (vello_cpu::Pixmap, usize, usize) {
    use anyrender::PaintScene as _;
    let mut painter = anyrender_vello_cpu::VelloCpuScenePainter::new(width as u16, height as u16);
    let bg = peniko::Color::from_rgba8(
        ((background >> 16) & 0xff) as u8,
        ((background >> 8) & 0xff) as u8,
        (background & 0xff) as u8,
        0xff,
    );
    painter.fill(
        AnyFill::NonZero,
        Affine::IDENTITY,
        bg,
        None,
        &kurbo::Rect::new(0.0, 0.0, width as f64, height as f64),
    );
    let page_transform = Affine::translate((0.0, -page_top as f64));
    let local_top = (clip_top - page_top).clamp(0.0, height as f32);
    let local_bottom = (clip_bottom - page_top).clamp(0.0, height as f32);
    let clip = kurbo::Rect::new(
        clip_left as f64,
        local_top as f64,
        clip_right as f64,
        local_bottom as f64,
    );
    painter.push_clip_layer(AnyFill::NonZero, Affine::IDENTITY, &clip);
    replay_scene(&mut painter, scene, page_transform);
    painter.pop_layer();
    let (image_count, image_surface) = image_draw_stats(
        scene,
        width,
        height,
        page_transform,
        local_top,
        local_bottom,
    );
    (painter.finish(), image_count, image_surface)
}

#[cfg(feature = "vello-cpu")]
#[allow(clippy::too_many_arguments)]
fn render_page_vello(
    scene: &Scene,
    width: u32,
    height: u32,
    page_top: f32,
    clip_left: f32,
    clip_right: f32,
    clip_top: f32,
    clip_bottom: f32,
    background: u32,
) -> (Arc<PageBitmap>, usize, usize) {
    let (pixmap, image_count, image_surface) = prepare_vello_scene(
        scene,
        width,
        height,
        page_top,
        clip_left,
        clip_right,
        clip_top,
        clip_bottom,
        background,
    );
    let mut data = Vec::with_capacity((width * height) as usize);
    for pixel in pixmap.data() {
        let a = pixel.a as u32;
        let r = pixel.r as u32;
        let g = pixel.g as u32;
        let b = pixel.b as u32;
        let luma = (r * 299 + g * 587 + b * 114) / 1000;
        data.push(((luma * a + 255 * (255 - a)) / 255) as u8);
    }
    (
        Arc::new(PageBitmap {
            width,
            height,
            data,
        }),
        image_count,
        image_surface,
    )
}

#[cfg(feature = "vello-cpu")]
#[allow(clippy::too_many_arguments)]
fn render_page_rgba_vello(
    scene: &Scene,
    width: u32,
    height: u32,
    page_top: f32,
    clip_left: f32,
    clip_right: f32,
    clip_top: f32,
    clip_bottom: f32,
    background: u32,
) -> (Arc<ColorPageBitmap>, usize, usize) {
    let (pixmap, image_count, image_surface) = prepare_vello_scene(
        scene,
        width,
        height,
        page_top,
        clip_left,
        clip_right,
        clip_top,
        clip_bottom,
        background,
    );
    let mut data = Vec::with_capacity((width * height * 4) as usize);
    for pixel in pixmap.data() {
        data.extend_from_slice(&[pixel.r, pixel.g, pixel.b, pixel.a]);
    }
    (
        Arc::new(ColorPageBitmap {
            width,
            height,
            data,
        }),
        image_count,
        image_surface,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyrender::PaintScene;

    #[test]
    fn page_clip_mask_limits_both_axes() {
        let mask = page_clip_mask(10, 10, 2.0, 8.0, 3.0, 7.0).expect("partial clip");
        let data = mask.data();
        let at = |x: usize, y: usize| data[y * 10 + x];
        assert_eq!(at(1, 1), 0);
        assert_eq!(at(2, 3), 255);
        assert_eq!(at(7, 6), 255);
        assert_eq!(at(8, 7), 0);
    }

    #[test]
    fn page_clip_mask_is_none_for_the_full_page() {
        assert!(page_clip_mask(10, 10, 0.0, 10.0, 0.0, 10.0).is_none());
    }

    #[test]
    fn linear_gradient_fill_is_painted() {
        let mut scene = Scene::default();
        let gradient = Gradient::new_linear((0.0, 0.0), (16.0, 0.0)).with_stops([
            (0.0, Color::from_rgba8(0, 0, 0, 255)),
            (1.0, Color::from_rgba8(255, 255, 255, 255)),
        ]);
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Paint::Gradient(&gradient),
            None,
            &kurbo::Rect::new(0.0, 0.0, 16.0, 8.0),
        );
        let (bitmap, _, _) = render_page(&scene, 16, 8, 0.0, 0.0, 16.0, 0.0, 8.0, 0xffffff, 1.0);
        assert!(
            bitmap.data[0] < 32,
            "left pixel should be dark, got {}",
            bitmap.data[0]
        );
        assert!(
            bitmap.data[15] > 200,
            "right pixel should be light, got {}",
            bitmap.data[15]
        );
    }

    #[cfg(feature = "vello-cpu")]
    #[test]
    fn vello_linear_gradient_fill_is_painted() {
        let mut scene = Scene::default();
        let gradient = Gradient::new_linear((0.0, 0.0), (16.0, 0.0)).with_stops([
            (0.0, Color::from_rgba8(0, 0, 0, 255)),
            (1.0, Color::from_rgba8(255, 255, 255, 255)),
        ]);
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Paint::Gradient(&gradient),
            None,
            &kurbo::Rect::new(0.0, 0.0, 16.0, 8.0),
        );
        let (bitmap, _, _) = render_page_vello(&scene, 16, 8, 0.0, 0.0, 16.0, 0.0, 8.0, 0xffffff);
        assert!(
            bitmap.data[0] < 32,
            "left pixel should be dark, got {}",
            bitmap.data[0]
        );
        assert!(
            bitmap.data[15] > 200,
            "right pixel should be light, got {}",
            bitmap.data[15]
        );
    }

    use anyrender::filters::{Filter, FilterEffect};
    use std::sync::Arc;

    fn render_tiny(scene: &Scene, width: u32, height: u32) -> Pixmap {
        let (rasterizer, _, _) = raster::Rasterizer::new(
            scene,
            width,
            height,
            Affine::IDENTITY,
            0.0,
            height as f32,
            1.0,
            0xffffff,
            None,
        );
        rasterizer.run(scene)
    }

    fn pixel(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let index = ((y * pixmap.width() + x) * 4) as usize;
        let data = pixmap.data();
        (
            data[index],
            data[index + 1],
            data[index + 2],
            data[index + 3],
        )
    }

    #[test]
    fn clip_layer_clips_fill() {
        let mut scene = Scene::default();
        scene.push_clip_layer(
            AnyFill::NonZero,
            Affine::IDENTITY,
            &kurbo::Rect::new(0.0, 0.0, 4.0, 4.0),
        );
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Color::from_rgba8(255, 0, 0, 255),
            None,
            &kurbo::Rect::new(0.0, 0.0, 10.0, 10.0),
        );
        scene.pop_layer();
        let pixmap = render_tiny(&scene, 10, 10);
        assert_eq!(pixel(&pixmap, 1, 1), (255, 0, 0, 255));
        assert_eq!(pixel(&pixmap, 8, 8), (255, 255, 255, 255));
    }

    #[test]
    fn layer_opacity_blends_over_background() {
        let mut scene = Scene::default();
        scene.push_layer(
            AnyFill::NonZero,
            peniko::Mix::Normal,
            0.5,
            Affine::IDENTITY,
            &kurbo::Rect::new(0.0, 0.0, 10.0, 10.0),
            None,
            None,
        );
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Color::from_rgba8(255, 0, 0, 255),
            None,
            &kurbo::Rect::new(0.0, 0.0, 10.0, 10.0),
        );
        scene.pop_layer();
        let pixmap = render_tiny(&scene, 10, 10);
        let (r, g, b, _) = pixel(&pixmap, 5, 5);
        assert!(r > 200, "red should stay strong, got {r}");
        assert!((80..=200).contains(&g), "green should be blended, got {g}");
        assert!((80..=200).contains(&b), "blue should be blended, got {b}");
    }

    #[test]
    fn box_shadow_blurs_outside_the_box() {
        let mut scene = Scene::default();
        scene.draw_box_shadow(
            Affine::IDENTITY,
            kurbo::Rect::new(3.0, 3.0, 13.0, 13.0),
            Color::from_rgba8(0, 0, 0, 255),
            0.0,
            2.0,
        );
        let pixmap = render_tiny(&scene, 16, 16);
        let inside = pixel(&pixmap, 8, 8);
        let outside = pixel(&pixmap, 1, 8);
        assert!(inside.0 < 80, "shadow center should be dark: {inside:?}");
        assert!(
            outside.0 < 250,
            "shadow should spread outside the box: {outside:?}"
        );
        assert!(
            outside.0 > inside.0,
            "shadow should fade away from the box: {outside:?} vs {inside:?}"
        );
        assert!(
            pixel(&pixmap, 15, 15).0 > 240,
            "far corner should stay nearly white: {:?}",
            pixel(&pixmap, 15, 15)
        );
    }

    #[test]
    fn dashed_stroke_has_gaps() {
        let mut style = kurbo::Stroke::new(2.0);
        style.dash_pattern = [2.0, 2.0].into_iter().collect();
        let mut scene = Scene::default();
        scene.stroke(
            &style,
            Affine::IDENTITY,
            Color::from_rgba8(0, 0, 0, 255),
            None,
            &kurbo::Line::new((0.0, 6.0), (12.0, 6.0)),
        );
        let pixmap = render_tiny(&scene, 12, 12);
        let dark = (0..12).filter(|x| pixel(&pixmap, *x, 6).0 < 100).count();
        let light = (0..12).filter(|x| pixel(&pixmap, *x, 6).0 > 200).count();
        assert!(dark > 0, "dash should paint some pixels");
        assert!(light > 0, "dash should leave gaps");
    }

    #[test]
    fn filter_brightness_changes_pixels() {
        let filter = Filter::single(FilterEffect::brightness(2.0));
        let mut scene = Scene::default();
        scene.push_layer(
            AnyFill::NonZero,
            peniko::Mix::Normal,
            1.0,
            Affine::IDENTITY,
            &kurbo::Rect::new(0.0, 0.0, 10.0, 10.0),
            Some(Arc::new(filter)),
            None,
        );
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Color::from_rgba8(128, 128, 128, 255),
            None,
            &kurbo::Rect::new(0.0, 0.0, 10.0, 10.0),
        );
        scene.pop_layer();
        let pixmap = render_tiny(&scene, 10, 10);
        assert!(
            pixel(&pixmap, 5, 5).0 > 160,
            "brightness filter should lighten the gray: {:?}",
            pixel(&pixmap, 5, 5)
        );
    }

    #[test]
    fn gradient_stroke_is_painted() {
        let gradient = Gradient::new_linear((0.0, 0.0), (12.0, 0.0)).with_stops([
            (0.0, Color::from_rgba8(0, 0, 0, 255)),
            (1.0, Color::from_rgba8(255, 255, 255, 255)),
        ]);
        let mut scene = Scene::default();
        scene.stroke(
            &kurbo::Stroke::new(3.0),
            Affine::IDENTITY,
            Paint::Gradient(&gradient),
            None,
            &kurbo::Line::new((0.0, 5.0), (12.0, 5.0)),
        );
        let pixmap = render_tiny(&scene, 12, 12);
        let left = pixel(&pixmap, 1, 5).0;
        let right = pixel(&pixmap, 10, 5).0;
        assert!(left < 120, "left of gradient stroke should be dark: {left}");
        assert!(
            right > 120,
            "right of gradient stroke should be light: {right}"
        );
    }

    fn half_red_blue_image() -> peniko::ImageData {
        // 4x2 image: left half red, right half blue, in straight RGBA.
        let mut data = Vec::new();
        for _y in 0..2 {
            for x in 0..4 {
                if x < 2 {
                    data.extend_from_slice(&[255, 0, 0, 255]);
                } else {
                    data.extend_from_slice(&[0, 0, 255, 255]);
                }
            }
        }
        peniko::ImageData {
            data: data.into(),
            format: ImageFormat::Rgba8,
            alpha_type: peniko::ImageAlphaType::Alpha,
            width: 4,
            height: 2,
        }
    }

    #[test]
    fn sweep_gradient_fill_is_painted() {
        let gradient = Gradient::new_sweep((8.0, 8.0), 0.0, std::f32::consts::TAU).with_stops([
            (0.0, Color::from_rgba8(0, 0, 0, 255)),
            (0.5, Color::from_rgba8(255, 255, 255, 255)),
            (1.0, Color::from_rgba8(0, 0, 0, 255)),
        ]);
        let mut scene = Scene::default();
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Paint::Gradient(&gradient),
            None,
            &kurbo::Rect::new(0.0, 0.0, 16.0, 16.0),
        );
        let pixmap = render_tiny(&scene, 16, 16);
        let right = pixel(&pixmap, 14, 8).0;
        let left = pixel(&pixmap, 1, 8).0;
        assert!(right < 80, "sweep start should be dark: {right}");
        assert!(left > 200, "sweep midpoint should be light: {left}");
    }

    #[test]
    fn image_fill_samples_pixels() {
        let image = half_red_blue_image();
        let brush = peniko::ImageBrush {
            image: &image,
            sampler: peniko::ImageSampler::default(),
        };
        let mut scene = Scene::default();
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Paint::Image(brush),
            None,
            &kurbo::Rect::new(0.0, 0.0, 4.0, 2.0),
        );
        let pixmap = render_tiny(&scene, 4, 2);
        assert_eq!(pixel(&pixmap, 0, 0), (255, 0, 0, 255));
        assert_eq!(pixel(&pixmap, 3, 0), (0, 0, 255, 255));
    }

    #[test]
    fn image_fill_repeats_with_sampler_extend() {
        let image = half_red_blue_image();
        let sampler = peniko::ImageSampler {
            x_extend: PenikoExtend::Repeat,
            ..Default::default()
        };
        let brush = peniko::ImageBrush {
            image: &image,
            sampler,
        };
        let mut scene = Scene::default();
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Paint::Image(brush),
            None,
            &kurbo::Rect::new(0.0, 0.0, 8.0, 2.0),
        );
        let pixmap = render_tiny(&scene, 8, 2);
        assert_eq!(pixel(&pixmap, 0, 0), (255, 0, 0, 255));
        assert_eq!(pixel(&pixmap, 3, 0), (0, 0, 255, 255));
        assert_eq!(pixel(&pixmap, 4, 0), (255, 0, 0, 255));
        assert_eq!(pixel(&pixmap, 7, 0), (0, 0, 255, 255));
    }

    #[test]
    fn image_fill_respects_shape_mask_and_clip() {
        let image = half_red_blue_image();
        let brush = peniko::ImageBrush {
            image: &image,
            sampler: peniko::ImageSampler::default(),
        };
        let mut scene = Scene::default();
        scene.push_clip_layer(
            AnyFill::NonZero,
            Affine::IDENTITY,
            &kurbo::Rect::new(0.0, 0.0, 2.0, 2.0),
        );
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Paint::Image(brush),
            None,
            &kurbo::Rect::new(0.0, 0.0, 4.0, 2.0),
        );
        scene.pop_layer();
        let pixmap = render_tiny(&scene, 4, 2);
        assert_eq!(pixel(&pixmap, 0, 0), (255, 0, 0, 255));
        assert_eq!(pixel(&pixmap, 3, 0), (255, 255, 255, 255));
    }

    #[test]
    fn multiply_layer_blend_darkens() {
        let mut scene = Scene::default();
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Color::from_rgba8(255, 0, 0, 255),
            None,
            &kurbo::Rect::new(0.0, 0.0, 10.0, 10.0),
        );
        scene.push_layer(
            AnyFill::NonZero,
            peniko::Mix::Multiply,
            1.0,
            Affine::IDENTITY,
            &kurbo::Rect::new(0.0, 0.0, 10.0, 10.0),
            None,
            None,
        );
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Color::from_rgba8(0, 0, 255, 255),
            None,
            &kurbo::Rect::new(0.0, 0.0, 10.0, 10.0),
        );
        scene.pop_layer();
        let pixmap = render_tiny(&scene, 10, 10);
        assert_eq!(pixel(&pixmap, 5, 5), (0, 0, 0, 255));
    }

    #[test]
    fn nested_clip_layers_restore_outer_clip() {
        let mut scene = Scene::default();
        scene.push_clip_layer(
            AnyFill::NonZero,
            Affine::IDENTITY,
            &kurbo::Rect::new(0.0, 0.0, 4.0, 4.0),
        );
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Color::from_rgba8(255, 0, 0, 255),
            None,
            &kurbo::Rect::new(0.0, 0.0, 10.0, 10.0),
        );
        scene.push_clip_layer(
            AnyFill::NonZero,
            Affine::IDENTITY,
            &kurbo::Rect::new(2.0, 2.0, 6.0, 6.0),
        );
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Color::from_rgba8(0, 255, 0, 255),
            None,
            &kurbo::Rect::new(0.0, 0.0, 10.0, 10.0),
        );
        scene.pop_layer();
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Color::from_rgba8(0, 0, 255, 255),
            None,
            &kurbo::Rect::new(0.0, 0.0, 10.0, 10.0),
        );
        scene.pop_layer();
        let pixmap = render_tiny(&scene, 10, 10);
        assert_eq!(pixel(&pixmap, 1, 1), (0, 0, 255, 255));
        assert_eq!(pixel(&pixmap, 3, 3), (0, 0, 255, 255));
        assert_eq!(pixel(&pixmap, 5, 5), (255, 255, 255, 255));
    }

    #[test]
    fn triangle_clip_is_not_treated_as_full_page() {
        let mut triangle = kurbo::BezPath::new();
        triangle.move_to((0.0, 0.0));
        triangle.line_to((0.0, 10.0));
        triangle.line_to((10.0, 10.0));
        triangle.close_path();
        let mut scene = Scene::default();
        scene.push_clip_layer(AnyFill::NonZero, Affine::IDENTITY, &triangle);
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Color::from_rgba8(255, 0, 0, 255),
            None,
            &kurbo::Rect::new(0.0, 0.0, 10.0, 10.0),
        );
        scene.pop_layer();
        let pixmap = render_tiny(&scene, 10, 10);
        assert_eq!(pixel(&pixmap, 1, 8), (255, 0, 0, 255));
        assert_eq!(pixel(&pixmap, 9, 0), (255, 255, 255, 255));
    }

    #[test]
    fn full_page_clip_layer_is_a_noop() {
        let mut scene = Scene::default();
        scene.push_clip_layer(
            AnyFill::NonZero,
            Affine::IDENTITY,
            &kurbo::Rect::new(0.0, 0.0, 10.0, 10.0),
        );
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Color::from_rgba8(0, 128, 0, 255),
            None,
            &kurbo::Rect::new(2.0, 2.0, 8.0, 8.0),
        );
        scene.pop_layer();
        let pixmap = render_tiny(&scene, 10, 10);
        assert_eq!(pixel(&pixmap, 5, 5), (0, 128, 0, 255));
        assert_eq!(pixel(&pixmap, 0, 0), (255, 255, 255, 255));
    }

    #[test]
    fn bgra_premultiplied_image_is_unpremultiplied() {
        // Straight (0, 0, 255, 128) stored as premultiplied BGRA.
        let data = vec![128u8, 0, 0, 128];
        let image = peniko::ImageData {
            data: data.into(),
            format: ImageFormat::Bgra8,
            alpha_type: peniko::ImageAlphaType::AlphaPremultiplied,
            width: 1,
            height: 1,
        };
        let brush = peniko::ImageBrush {
            image: &image,
            sampler: peniko::ImageSampler {
                quality: peniko::ImageQuality::Low,
                ..Default::default()
            },
        };
        let mut scene = Scene::default();
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Paint::Image(brush),
            None,
            &kurbo::Rect::new(0.0, 0.0, 1.0, 1.0),
        );
        let pixmap = render_tiny(&scene, 1, 1);
        let (r, g, b, a) = pixel(&pixmap, 0, 0);
        assert!(b > 240, "blue should stay saturated, got {b}");
        assert!(
            (100..=160).contains(&r),
            "red should be half blended, got {r}"
        );
        assert_eq!(g, r);
        assert_eq!(a, 255);
    }

    #[test]
    fn drop_shadow_filter_offsets_the_shadow() {
        let filter = Filter::single(FilterEffect::drop_shadow(
            2.0,
            2.0,
            0.0,
            Color::from_rgba8(0, 0, 0, 255),
        ));
        let mut scene = Scene::default();
        scene.push_layer(
            AnyFill::NonZero,
            peniko::Mix::Normal,
            1.0,
            Affine::IDENTITY,
            &kurbo::Rect::new(0.0, 0.0, 10.0, 10.0),
            Some(Arc::new(filter)),
            None,
        );
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Color::from_rgba8(255, 0, 0, 255),
            None,
            &kurbo::Rect::new(1.0, 1.0, 3.0, 3.0),
        );
        scene.pop_layer();
        let pixmap = render_tiny(&scene, 10, 10);
        assert_eq!(pixel(&pixmap, 2, 2), (255, 0, 0, 255));
        assert_eq!(pixel(&pixmap, 4, 4), (0, 0, 0, 255));
        assert_eq!(pixel(&pixmap, 7, 7), (255, 255, 255, 255));
    }

    #[test]
    fn gaussian_blur_filter_spreads_a_rect() {
        let filter = Filter::single(FilterEffect::blur(1.5));
        let mut scene = Scene::default();
        scene.push_layer(
            AnyFill::NonZero,
            peniko::Mix::Normal,
            1.0,
            Affine::IDENTITY,
            &kurbo::Rect::new(0.0, 0.0, 10.0, 10.0),
            Some(Arc::new(filter)),
            None,
        );
        scene.fill(
            AnyFill::NonZero,
            Affine::IDENTITY,
            Color::from_rgba8(0, 0, 0, 255),
            None,
            &kurbo::Rect::new(3.0, 3.0, 7.0, 7.0),
        );
        scene.pop_layer();
        let pixmap = render_tiny(&scene, 10, 10);
        let center = pixel(&pixmap, 5, 5).0;
        let near = pixel(&pixmap, 1, 5).0;
        let far = pixel(&pixmap, 9, 9).0;
        assert!(
            center < 120,
            "blurred rect center should stay dark: {center}"
        );
        assert!(
            near > center,
            "blur should fade from the rect: near={near} center={center}"
        );
        assert!(far > 240, "far pixels should stay white: {far}");
    }
}

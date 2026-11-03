use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyrender::recording::RenderCommand;
use anyrender::{Paint, Scene};
use kurbo::{Affine, BezPath, PathEl, Point, Shape, Stroke};
use peniko::{Color, Extend as PenikoExtend, Fill as AnyFill, Gradient, GradientKind, ImageFormat};
use tiny_skia::{
    FillRule, GradientStop as SkGradientStop, LineCap, LineJoin,
    LinearGradient as SkLinearGradient, Mask, Paint as SkPaint, PathBuilder, Pixmap,
    RadialGradient as SkRadialGradient, Shader, SpreadMode, Stroke as SkStroke, Transform,
};

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
    let mut pixmap = Pixmap::new(width, height).expect("non-zero page bitmap");
    let background_color = tiny_skia::Color::from_rgba8(
        ((background >> 16) & 0xff) as u8,
        ((background >> 8) & 0xff) as u8,
        (background & 0xff) as u8,
        0xff,
    );
    pixmap.fill(background_color);

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
    let (image_count, image_surface) = image_draw_stats(
        scene,
        width,
        height,
        page_transform,
        local_top,
        local_bottom,
    );
    let mut faces: HashMap<usize, ttf_parser::Face<'_>> = HashMap::new();
    let mut glyph_cache: HashMap<GlyphKey, GlyphBitmap> = HashMap::new();
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
                Paint::Gradient(gradient) => {
                    let brush_transform = cmd.brush_transform.unwrap_or(Affine::IDENTITY);
                    let shader =
                        gradient_shader(gradient, page_transform * cmd.transform * brush_transform);
                    if let Some(shader) = shader {
                        draw_fill_shader(
                            &mut pixmap,
                            &cmd.shape,
                            page_transform * cmd.transform,
                            shader,
                            cmd.fill,
                            clip_mask.as_ref(),
                        );
                    }
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
                        font_gamma,
                        &mut faces,
                        &mut glyph_cache,
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
    font_gamma: f32,
    faces: &mut HashMap<usize, ttf_parser::Face<'a>>,
    glyph_cache: &mut HashMap<GlyphKey, GlyphBitmap>,
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
        // tiny-skia has no sweep shader; leave the fill unpainted rather than
        // drawing a wrong color.
        GradientKind::Sweep(_) => None,
    }
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
    let mut pixmap = Pixmap::new(width, height).expect("non-zero page bitmap");
    let background_color = tiny_skia::Color::from_rgba8(
        ((background >> 16) & 0xff) as u8,
        ((background >> 8) & 0xff) as u8,
        (background & 0xff) as u8,
        0xff,
    );
    pixmap.fill(background_color);

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
    let (image_count, image_surface) = image_draw_stats(
        scene,
        width,
        height,
        page_transform,
        local_top,
        local_bottom,
    );
    let mut fonts = HashMap::new();
    let mut glyph_cache: HashMap<GlyphKey, GlyphBitmap> = HashMap::new();

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
                Paint::Gradient(gradient) => {
                    let brush_transform = cmd.brush_transform.unwrap_or(Affine::IDENTITY);
                    let shader =
                        gradient_shader(gradient, page_transform * cmd.transform * brush_transform);
                    if let Some(shader) = shader {
                        draw_fill_shader(
                            &mut pixmap,
                            &cmd.shape,
                            page_transform * cmd.transform,
                            shader,
                            cmd.fill,
                            clip_mask.as_ref(),
                        );
                    }
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
                        font_gamma,
                        &mut fonts,
                        &mut glyph_cache,
                    );
                }
            }
            RenderCommand::PushLayer(_)
            | RenderCommand::PushClipLayer(_)
            | RenderCommand::PopLayer
            | RenderCommand::BoxShadow(_) => {}
        }
    }

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
}

//! Rasterisation of an `anyrender` scene with tiny-skia.
//!
//! `blitz-paint` records a display list that uses clip layers, compositing
//! layers, filters and box shadows.  This module keeps those semantics on top
//! of tiny-skia: a clip-mask stack, an offscreen-layer stack, box-shadow
//! blurring, dashed/gradient strokes and sampler-aware image fills.

use std::sync::Arc;

use anyrender::Paint;
use anyrender::filters::{Filter, FilterEffect};
use anyrender::recording::{BoxShadowCommand, ClipCommand, LayerCommand, RenderCommand};
use kurbo::{Affine, BezPath, RoundedRect, Shape};
use peniko::{BlendMode, Color, Compose, Fill, Mix};
use tiny_skia::{
    BlendMode as SkBlendMode, FillRule, FilterQuality, Mask, PathBuilder, Pixmap, PixmapPaint,
    Rect as SkRect, Transform,
};

use super::{
    AnyFill, ImageCache, Scene, draw_fill, draw_fill_shader, draw_glyphs, draw_glyphs_with_brush,
    draw_image_fill, draw_stroke, draw_sweep_fill, gradient_shader, image_draw_stats, skia_paint,
    solid_color, to_skia_path,
};

struct LayerFrame {
    old_clip: Option<Arc<Mask>>,
    full: Option<FullLayer>,
}

struct FullLayer {
    pixmap: Pixmap,
    clip: Option<Arc<Mask>>,
    alpha: f32,
    blend: SkBlendMode,
    filter: Option<Arc<Filter>>,
    backdrop_filter: Option<Arc<Filter>>,
}

pub(crate) struct Rasterizer<'a> {
    width: u32,
    height: u32,
    page_transform: Affine,
    local_top: f32,
    local_bottom: f32,
    font_gamma: f32,
    base: Pixmap,
    clip: Option<Arc<Mask>>,
    layers: Vec<LayerFrame>,
    faces: std::collections::HashMap<usize, ttf_parser::Face<'a>>,
    glyph_cache: std::collections::HashMap<super::GlyphKey, super::GlyphBitmap>,
    image_cache: ImageCache,
}

impl<'a> Rasterizer<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        scene: &'a Scene,
        width: u32,
        height: u32,
        page_transform: Affine,
        local_top: f32,
        local_bottom: f32,
        font_gamma: f32,
        background: u32,
        initial_clip: Option<Mask>,
    ) -> (Self, usize, usize) {
        let mut base = Pixmap::new(width, height).expect("non-zero page bitmap");
        base.fill(tiny_skia::Color::from_rgba8(
            ((background >> 16) & 0xff) as u8,
            ((background >> 8) & 0xff) as u8,
            (background & 0xff) as u8,
            0xff,
        ));
        let (image_count, image_surface) = image_draw_stats(
            scene,
            width,
            height,
            page_transform,
            local_top,
            local_bottom,
        );
        (
            Self {
                width,
                height,
                page_transform,
                local_top,
                local_bottom,
                font_gamma,
                base,
                clip: initial_clip.map(Arc::new),
                layers: Vec::new(),
                faces: std::collections::HashMap::new(),
                glyph_cache: std::collections::HashMap::new(),
                image_cache: ImageCache::default(),
            },
            image_count,
            image_surface,
        )
    }

    pub(crate) fn run(mut self, scene: &'a Scene) -> Pixmap {
        let stats = std::env::var_os("CRE_RASTER_STATS").is_some();
        let mut acc = [std::time::Duration::ZERO; 8];
        let mut counts = [0usize; 8];
        for command in &scene.commands {
            let started = stats.then(std::time::Instant::now);
            let slot = match command {
                RenderCommand::Fill(_) => 0,
                RenderCommand::Stroke(_) => 1,
                RenderCommand::GlyphRun(_) => 2,
                RenderCommand::PushLayer(_) | RenderCommand::PopLayer => 3,
                RenderCommand::PushClipLayer(_) => 4,
                RenderCommand::BoxShadow(_) => 5,
            };
            match command {
                RenderCommand::Fill(cmd) => self.fill(cmd),
                RenderCommand::Stroke(cmd) => self.stroke(cmd),
                RenderCommand::GlyphRun(cmd) => self.glyph_run(cmd),
                RenderCommand::PushLayer(cmd) => self.push_layer(cmd),
                RenderCommand::PushClipLayer(cmd) => self.push_clip(cmd),
                RenderCommand::PopLayer => self.pop_layer(),
                RenderCommand::BoxShadow(cmd) => self.box_shadow(cmd),
            }
            if let Some(started) = started {
                acc[slot] += started.elapsed();
                counts[slot] += 1;
            }
        }
        while !self.layers.is_empty() {
            self.pop_layer();
        }
        if stats {
            let names = ["fill", "stroke", "glyphs", "layer", "clip", "shadow"];
            for index in 0..names.len() {
                if counts[index] > 0 {
                    eprintln!(
                        "raster {:>6}: {:>4} cmds {:>8.2} ms",
                        names[index],
                        counts[index],
                        acc[index].as_secs_f64() * 1000.0
                    );
                }
            }
        }
        self.base
    }

    fn fill(&mut self, cmd: &anyrender::recording::FillCommand) {
        let transform = self.page_transform * cmd.transform;
        let clip = self.clip.as_deref();
        let pixmap = pixmap_mut(&mut self.base, &mut self.layers);
        match &cmd.brush {
            Paint::Solid(color) => {
                draw_fill(pixmap, &cmd.shape, transform, *color, cmd.fill, clip);
            }
            Paint::Gradient(gradient) => {
                let brush_transform = cmd.brush_transform.unwrap_or(Affine::IDENTITY);
                if let Some(shader) = gradient_shader(gradient, transform * brush_transform) {
                    draw_fill_shader(pixmap, &cmd.shape, transform, shader, cmd.fill, clip);
                } else if matches!(gradient.kind, peniko::GradientKind::Sweep(_)) {
                    draw_sweep_fill(
                        pixmap,
                        &cmd.shape,
                        cmd.fill,
                        transform,
                        gradient,
                        cmd.brush_transform,
                        clip,
                    );
                }
            }
            Paint::Image(image) => {
                draw_image_fill(
                    pixmap,
                    image,
                    &cmd.shape,
                    cmd.fill,
                    transform,
                    cmd.brush_transform,
                    clip,
                    &mut self.image_cache,
                );
            }
            _ => {}
        }
    }

    fn stroke(&mut self, cmd: &anyrender::recording::StrokeCommand) {
        let transform = self.page_transform * cmd.transform;
        let clip = self.clip.as_deref();
        let pixmap = pixmap_mut(&mut self.base, &mut self.layers);
        draw_stroke(
            pixmap, &cmd.shape, transform, &cmd.brush, None, &cmd.style, clip,
        );
    }

    fn glyph_run(&mut self, cmd: &'a anyrender::recording::GlyphRunCommand) {
        let transform = self.page_transform * cmd.transform;
        let local_top = self.local_top;
        let local_bottom = self.local_bottom;
        let font_gamma = self.font_gamma;
        let clip = self.clip.as_deref();
        let pixmap = pixmap_mut(&mut self.base, &mut self.layers);
        if let Some(color) = solid_color(&cmd.brush) {
            draw_glyphs(
                pixmap,
                &cmd.font_data,
                cmd.font_size,
                &cmd.glyphs,
                transform,
                cmd.glyph_transform,
                color,
                cmd.brush_alpha,
                local_top,
                local_bottom,
                clip,
                font_gamma,
                &mut self.faces,
                &mut self.glyph_cache,
            );
            return;
        }
        draw_glyphs_with_brush(
            pixmap,
            &cmd.font_data,
            cmd.font_size,
            &cmd.glyphs,
            transform,
            cmd.glyph_transform,
            &cmd.brush,
            None,
            cmd.brush_alpha,
            local_top,
            local_bottom,
            clip,
            font_gamma,
            &mut self.faces,
            &mut self.glyph_cache,
        );
    }

    fn push_clip(&mut self, cmd: &ClipCommand) {
        let old = self.clip.take();
        let transform = self.page_transform * cmd.transform;
        let mask = shape_mask(&cmd.clip, transform, cmd.fill, self.width, self.height);
        self.clip = push_clip_mask(old.clone(), mask);
        self.layers.push(LayerFrame {
            old_clip: old,
            full: None,
        });
    }

    fn push_layer(&mut self, cmd: &LayerCommand) {
        let old = self.clip.take();
        let transform = self.page_transform * cmd.transform;
        let mask = shape_mask(&cmd.clip, transform, cmd.fill, self.width, self.height);
        let clip = push_clip_mask(old.clone(), mask);
        self.clip = clip.clone();
        let pixmap = Pixmap::new(self.width, self.height).expect("non-zero layer bitmap");
        self.layers.push(LayerFrame {
            old_clip: old,
            full: Some(FullLayer {
                pixmap,
                clip,
                alpha: cmd.alpha.clamp(0.0, 1.0),
                blend: map_blend(cmd.blend),
                filter: cmd.filter.clone(),
                backdrop_filter: cmd.backdrop_filter.clone(),
            }),
        });
    }

    fn pop_layer(&mut self) {
        let Some(frame) = self.layers.pop() else {
            return;
        };
        let old_clip = frame.old_clip;
        if let Some(mut full) = frame.full {
            if let Some(filter) = &full.filter {
                apply_filter(&mut full.pixmap, filter);
            }
            self.clip = old_clip;
            let parent_clip = self.clip.as_deref();
            let (width, height) = (self.width, self.height);
            let parent = pixmap_mut(&mut self.base, &mut self.layers);
            if let Some(backdrop) = &full.backdrop_filter {
                let mut filtered = Pixmap::new(width, height).expect("backdrop bitmap");
                filtered.draw_pixmap(
                    0,
                    0,
                    parent.as_ref(),
                    &PixmapPaint {
                        opacity: 1.0,
                        blend_mode: SkBlendMode::SourceOver,
                        quality: FilterQuality::Nearest,
                    },
                    Transform::identity(),
                    None,
                );
                apply_filter(&mut filtered, backdrop);
                parent.draw_pixmap(
                    0,
                    0,
                    filtered.as_ref(),
                    &PixmapPaint {
                        opacity: 1.0,
                        blend_mode: SkBlendMode::Source,
                        quality: FilterQuality::Nearest,
                    },
                    Transform::identity(),
                    full.clip.as_deref(),
                );
            }
            parent.draw_pixmap(
                0,
                0,
                full.pixmap.as_ref(),
                &PixmapPaint {
                    opacity: full.alpha,
                    blend_mode: full.blend,
                    quality: FilterQuality::Nearest,
                },
                Transform::identity(),
                parent_clip,
            );
        } else {
            self.clip = old_clip;
        }
    }

    fn box_shadow(&mut self, cmd: &BoxShadowCommand) {
        let transform = self.page_transform * cmd.transform;
        let path = RoundedRect::new(
            cmd.rect.x0,
            cmd.rect.y0,
            cmd.rect.x1,
            cmd.rect.y1,
            cmd.radius.max(0.0),
        )
        .to_path(0.1);
        if cmd.std_dev <= 0.5 {
            let clip = self.clip.as_deref();
            let pixmap = pixmap_mut(&mut self.base, &mut self.layers);
            draw_fill(pixmap, &path, transform, cmd.brush, AnyFill::NonZero, clip);
            return;
        }
        let (width, height) = (self.width, self.height);
        let Some(mut mask) = Mask::new(width, height) else {
            return;
        };
        let Some(sk_path) = to_skia_path(&path, transform) else {
            return;
        };
        mask.fill_path(&sk_path, FillRule::Winding, true, Transform::identity());
        blur_mask(&mut mask, cmd.std_dev as f32);
        let mask = push_clip_mask(self.clip.clone(), Some(mask));
        let clip = mask.as_deref();
        let Some(page) = full_page_path(width, height) else {
            return;
        };
        let paint = skia_paint(cmd.brush);
        let pixmap = pixmap_mut(&mut self.base, &mut self.layers);
        pixmap.fill_path(
            &page,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            clip,
        );
    }
}

fn pixmap_mut<'a>(base: &'a mut Pixmap, layers: &'a mut [LayerFrame]) -> &'a mut Pixmap {
    for frame in layers.iter_mut().rev() {
        if let Some(full) = frame.full.as_mut() {
            return &mut full.pixmap;
        }
    }
    base
}

pub(crate) fn shape_mask(
    path: &BezPath,
    transform: Affine,
    fill: Fill,
    width: u32,
    height: u32,
) -> Option<Mask> {
    // A plain axis-aligned rectangle covering the whole page clips nothing.
    // Skipping it avoids allocating a full-page coverage mask for the very
    // common "clip an overflow box that happens to fill the page" case.
    if is_full_page_rect(path, width, height) {
        return None;
    }
    let sk_path = to_skia_path(path, transform)?;
    let mut mask = Mask::new(width, height)?;
    let rule = match fill {
        AnyFill::NonZero => FillRule::Winding,
        AnyFill::EvenOdd => FillRule::EvenOdd,
    };
    mask.fill_path(&sk_path, rule, true, Transform::identity());
    Some(mask)
}

/// True when the path is a closed polygon whose vertices are all corners of a
/// rectangle that covers the whole page. Straight-line-only is required so a
/// curve cannot bulge outside the bounding box.
fn is_full_page_rect(path: &BezPath, width: u32, height: u32) -> bool {
    let bbox = path.bounding_box();
    let (x0, y0, x1, y1) = (bbox.x0, bbox.y0, bbox.x1, bbox.y1);
    if x0 > 0.0 || y0 > 0.0 || x1 < width as f64 || y1 < height as f64 {
        return false;
    }
    let corner = |x: f64, y: f64| {
        ((x - x0).abs() < 0.01 || (x - x1).abs() < 0.01)
            && ((y - y0).abs() < 0.01 || (y - y1).abs() < 0.01)
    };
    let mut vertices = 0;
    let mut seen = [false; 4];
    let mut area: f64 = 0.0;
    let mut first: Option<kurbo::Point> = None;
    let mut previous: Option<kurbo::Point> = None;
    for el in path.elements() {
        match el {
            kurbo::PathEl::MoveTo(p) | kurbo::PathEl::LineTo(p) => {
                if !corner(p.x, p.y) {
                    return false;
                }
                vertices += 1;
                let bits = [
                    (p.x - x0).abs() < 0.01,
                    (p.x - x1).abs() < 0.01,
                    (p.y - y0).abs() < 0.01,
                    (p.y - y1).abs() < 0.01,
                ];
                if bits[0] && bits[2] {
                    seen[0] = true;
                }
                if bits[1] && bits[2] {
                    seen[1] = true;
                }
                if bits[1] && bits[3] {
                    seen[2] = true;
                }
                if bits[0] && bits[3] {
                    seen[3] = true;
                }
                first.get_or_insert(*p);
                if let Some(prev) = previous {
                    area += prev.x * p.y - p.x * prev.y;
                }
                previous = Some(*p);
            }
            kurbo::PathEl::ClosePath => {
                if let (Some(start), Some(prev)) = (first, previous) {
                    area += prev.x * start.y - start.x * prev.y;
                }
            }
            _ => return false,
        }
    }
    // Requiring all four corners plus the full rectangle area rules out
    // triangles and self-intersecting polygons whose bounding box happens to
    // fill the page.
    let page_area = (x1 - x0) * (y1 - y0);
    vertices >= 4
        && seen.iter().all(|flag| *flag)
        && (area.abs() - page_area).abs() <= page_area * 0.01 + 1.0
}

/// Intersects the current clip with a new mask, keeping the clip in an `Arc`
/// so restoring a layer is a pointer clone instead of a full-page copy.
fn push_clip_mask(current: Option<Arc<Mask>>, new: Option<Mask>) -> Option<Arc<Mask>> {
    match (current, new) {
        (Some(current), Some(mut new)) => {
            for (dst, src) in new.data_mut().iter_mut().zip(current.data()) {
                *dst = ((u16::from(*dst) * u16::from(*src)) / 255) as u8;
            }
            Some(Arc::new(new))
        }
        (Some(current), None) => Some(current),
        (None, Some(new)) => Some(Arc::new(new)),
        (None, None) => None,
    }
}

pub(crate) fn intersect_optional(current: Option<&Mask>, new: Option<Mask>) -> Option<Mask> {
    match (current, new) {
        (None, new) => new,
        (Some(current), None) => Some(current.clone()),
        (Some(current), Some(mut new)) => {
            for (dst, src) in new.data_mut().iter_mut().zip(current.data()) {
                *dst = ((u16::from(*dst) * u16::from(*src)) / 255) as u8;
            }
            Some(new)
        }
    }
}

pub(crate) fn full_page_path(width: u32, height: u32) -> Option<tiny_skia::Path> {
    let mut builder = PathBuilder::new();
    builder.push_rect(SkRect::from_ltrb(0.0, 0.0, width as f32, height as f32)?);
    builder.finish()
}

fn map_blend(blend: BlendMode) -> SkBlendMode {
    if blend.compose == Compose::Plus {
        return SkBlendMode::Plus;
    }
    match blend.mix {
        Mix::Normal => SkBlendMode::SourceOver,
        Mix::Multiply => SkBlendMode::Multiply,
        Mix::Screen => SkBlendMode::Screen,
        Mix::Overlay => SkBlendMode::Overlay,
        Mix::Darken => SkBlendMode::Darken,
        Mix::Lighten => SkBlendMode::Lighten,
        Mix::ColorDodge => SkBlendMode::ColorDodge,
        Mix::ColorBurn => SkBlendMode::ColorBurn,
        Mix::HardLight => SkBlendMode::HardLight,
        Mix::SoftLight => SkBlendMode::SoftLight,
        Mix::Difference => SkBlendMode::Difference,
        Mix::Exclusion => SkBlendMode::Exclusion,
        Mix::Hue => SkBlendMode::Hue,
        Mix::Saturation => SkBlendMode::Saturation,
        Mix::Color => SkBlendMode::Color,
        Mix::Luminosity => SkBlendMode::Luminosity,
    }
}

fn blur_mask(mask: &mut Mask, std_dev: f32) {
    let radius = blur_radius(std_dev);
    if radius == 0 {
        return;
    }
    let width = mask.width() as usize;
    let height = mask.height() as usize;
    let data = mask.data_mut();
    box_blur(data, width, height, 1, radius);
}

fn blur_pixmap(pixmap: &mut Pixmap, std_dev: f32) {
    let radius = blur_radius(std_dev);
    if radius == 0 {
        return;
    }
    let width = pixmap.width() as usize;
    let height = pixmap.height() as usize;
    box_blur(pixmap.data_mut(), width, height, 4, radius);
}

fn blur_radius(std_dev: f32) -> usize {
    if !std_dev.is_finite() || std_dev <= 0.0 {
        return 0;
    }
    // Three box passes approximate a Gaussian. A box of width w has variance
    // (w^2 - 1) / 12, so three passes give sigma^2 = (w^2 - 1) / 4, i.e.
    // w = sqrt(4 * sigma^2 + 1). Round to the nearest odd width so the kernel
    // stays symmetric, and never round down to "no blur" for small radii.
    let width = (4.0 * std_dev * std_dev + 1.0).sqrt().round().max(3.0);
    let width = if (width as usize).is_multiple_of(2) {
        width + 1.0
    } else {
        width
    };
    ((width as usize - 1) / 2).max(1)
}

/// Three box-blur passes approximate a Gaussian. Channels are either 1
/// (mask) or 4 (premultiplied RGBA).
fn box_blur(data: &mut [u8], width: usize, height: usize, channels: usize, radius: usize) {
    if width == 0 || height == 0 || radius == 0 {
        return;
    }
    let mut scratch = vec![0u8; data.len()];
    for _ in 0..3 {
        blur_axis(data, &mut scratch, width, height, channels, radius, true);
        blur_axis(&scratch, data, width, height, channels, radius, false);
    }
}

#[allow(clippy::too_many_arguments)]
fn blur_axis(
    src: &[u8],
    dst: &mut [u8],
    width: usize,
    height: usize,
    channels: usize,
    radius: usize,
    horizontal: bool,
) {
    let (lines, len) = if horizontal {
        (height, width)
    } else {
        (width, height)
    };
    let stride = width * channels;
    for line in 0..lines {
        for channel in 0..channels {
            let mut sum = 0u32;
            let index = |pos: usize| {
                if horizontal {
                    line * stride + pos * channels + channel
                } else {
                    pos * stride + line * channels + channel
                }
            };
            for pos in 0..=radius.min(len.saturating_sub(1)) {
                sum += u32::from(src[index(pos)]);
            }
            let window = (radius * 2 + 1) as u32;
            for pos in 0..len {
                dst[index(pos)] = (sum / window) as u8;
                if pos + radius + 1 < len {
                    sum += u32::from(src[index(pos + radius + 1)]);
                }
                if pos >= radius {
                    sum = sum.saturating_sub(u32::from(src[index(pos - radius)]));
                }
            }
        }
    }
}

fn apply_filter(pixmap: &mut Pixmap, filter: &Filter) {
    for node in filter.nodes() {
        match &node.effect {
            FilterEffect::GaussianBlur(blur) => blur_pixmap(pixmap, blur.std_deviation),
            FilterEffect::DropShadow(shadow) => apply_drop_shadow(
                pixmap,
                shadow.dx,
                shadow.dy,
                shadow.std_deviation,
                shadow.color,
            ),
            FilterEffect::ColorMatrix(matrix) => apply_color_matrix(pixmap, &matrix.0),
            FilterEffect::Offset(offset) => {
                offset_pixmap(pixmap, offset.x.round() as i32, offset.y.round() as i32)
            }
            FilterEffect::Flood(color) => {
                let rgba = color.to_rgba8();
                pixmap.fill(tiny_skia::Color::from_rgba8(rgba.r, rgba.g, rgba.b, rgba.a));
            }
            _ => {}
        }
    }
}

fn offset_pixmap(pixmap: &mut Pixmap, dx: i32, dy: i32) {
    let Some(mut out) = Pixmap::new(pixmap.width(), pixmap.height()) else {
        return;
    };
    out.draw_pixmap(
        dx,
        dy,
        pixmap.as_ref(),
        &PixmapPaint {
            opacity: 1.0,
            blend_mode: SkBlendMode::Source,
            quality: FilterQuality::Nearest,
        },
        Transform::identity(),
        None,
    );
    *pixmap = out;
}

fn apply_color_matrix(pixmap: &mut Pixmap, matrix: &[f32; 20]) {
    for pixel in pixmap.data_mut().as_chunks_mut::<4>().0 {
        let a = f32::from(pixel[3]) / 255.0;
        if a <= 0.0 {
            continue;
        }
        let r = f32::from(pixel[0]) / (255.0 * a);
        let g = f32::from(pixel[1]) / (255.0 * a);
        let b = f32::from(pixel[2]) / (255.0 * a);
        let apply = |offset: usize| {
            (matrix[offset] * r
                + matrix[offset + 1] * g
                + matrix[offset + 2] * b
                + matrix[offset + 3] * a
                + matrix[offset + 4])
                .clamp(0.0, 1.0)
        };
        let nr = apply(0);
        let ng = apply(5);
        let nb = apply(10);
        let na = apply(15);
        pixel[0] = (nr * na * 255.0).round() as u8;
        pixel[1] = (ng * na * 255.0).round() as u8;
        pixel[2] = (nb * na * 255.0).round() as u8;
        pixel[3] = (na * 255.0).round() as u8;
    }
}

fn apply_drop_shadow(pixmap: &mut Pixmap, dx: f32, dy: f32, std_dev: f32, color: Color) {
    let (width, height) = (pixmap.width(), pixmap.height());
    let Some(mut shadow) = Pixmap::new(width, height) else {
        return;
    };
    shadow.draw_pixmap(
        0,
        0,
        pixmap.as_ref(),
        &PixmapPaint {
            opacity: 1.0,
            blend_mode: SkBlendMode::Source,
            quality: FilterQuality::Nearest,
        },
        Transform::identity(),
        None,
    );
    let rgba = color.to_rgba8();
    for pixel in shadow.data_mut().as_chunks_mut::<4>().0 {
        let alpha = (u16::from(pixel[3]) * u16::from(rgba.a) / 255) as u8;
        pixel[0] = (u16::from(rgba.r) * u16::from(alpha) / 255) as u8;
        pixel[1] = (u16::from(rgba.g) * u16::from(alpha) / 255) as u8;
        pixel[2] = (u16::from(rgba.b) * u16::from(alpha) / 255) as u8;
        pixel[3] = alpha;
    }
    blur_pixmap(&mut shadow, std_dev);
    let Some(mut out) = Pixmap::new(width, height) else {
        return;
    };
    out.draw_pixmap(
        dx.round() as i32,
        dy.round() as i32,
        shadow.as_ref(),
        &PixmapPaint {
            opacity: 1.0,
            blend_mode: SkBlendMode::SourceOver,
            quality: FilterQuality::Nearest,
        },
        Transform::identity(),
        None,
    );
    out.draw_pixmap(
        0,
        0,
        pixmap.as_ref(),
        &PixmapPaint {
            opacity: 1.0,
            blend_mode: SkBlendMode::SourceOver,
            quality: FilterQuality::Nearest,
        },
        Transform::identity(),
        None,
    );
    *pixmap = out;
}

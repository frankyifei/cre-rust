//! Render a run of pages and print per-page paint time and RSS.
//!
//! Backend is selected with `CRE_PAINT_BACKEND=vello` (requires the
//! `vello-cpu` feature). This is a throwaway benchmark for comparing the
//! tiny-skia and vello_cpu backends on x86 and under qemu-arm.
use std::path::PathBuf;
use std::time::Instant;

use cre_lib::{Document, DocumentOptions};

fn rss_kb() -> u64 {
    let text = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = text
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    pages.saturating_mul(4)
}

fn positive(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value: &u32| *value > 0)
        .unwrap_or(default)
}

fn main() {
    let book = std::env::var("BOOK").expect("BOOK");
    let first = positive("FIRST", 1);
    let count = positive("N", 10);
    let width = positive("W", 758);
    let height = positive("H", 1024);
    let scale: f32 = std::env::var("SCALE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1.0);
    let font_size: f32 = std::env::var("FONT_SIZE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(22.0);
    let font_path = std::env::var("FONT_PATH")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf"));
    let font_family = std::env::var("FONT_FAMILY").ok();
    let mut fallback_font_paths = std::env::var("FALLBACK_FONTS")
        .map(|value| {
            value
                .split(':')
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Ok(math_font) = std::env::var("MATH_FONT") {
        fallback_font_paths.insert(0, PathBuf::from(math_font));
    }
    let options = DocumentOptions {
        width,
        height,
        font_size,
        page_margin: 10.0,
        font_path: Some(font_path),
        font_family,
        fallback_font_paths,
        viewport_scale: scale,
        ..Default::default()
    };
    let mut document = Document::with_options(&book, &options).expect("open book");
    let backend = std::env::var("CRE_PAINT_BACKEND").unwrap_or_else(|_| "tiny-skia".into());
    println!(
        "backend={backend} pages={} size={width}x{height} scale={scale} rss={}KB",
        document.page_count(),
        rss_kb()
    );

    let warm = Instant::now();
    let _ = document.render_page(first as usize);
    println!(
        "warmup page={first} {:.1} ms rss={}KB",
        warm.elapsed().as_secs_f64() * 1000.0,
        rss_kb()
    );

    let mut total = 0.0;
    let mut rendered = 0;
    for index in 0..count {
        let page = first + index;
        if page as usize > document.page_count() {
            break;
        }
        let started = Instant::now();
        document.render_page(page as usize).expect("render page");
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        total += ms;
        rendered += 1;
        println!("page={page} {ms:.1} ms rss={}KB", rss_kb());
    }
    println!(
        "total={total:.1} ms avg={:.1} ms",
        total / rendered.max(1) as f64
    );

    if let Ok(path) = std::env::var("SHOT") {
        let page: usize = std::env::var("SHOT_PAGE")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(first as usize);
        let bitmap = document.render_page(page).expect("render shot");
        let file = std::fs::File::create(&path).expect("create shot");
        let mut encoder = png::Encoder::new(file, bitmap.width, bitmap.height);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&bitmap.data).expect("png data");
        println!("shot={path} page={page}");
    }
}

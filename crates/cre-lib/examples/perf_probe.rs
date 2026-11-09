//! Dev-only benchmark: open a book, render pages, report time and memory.
//!
//! Prints machine-readable lines so runs can be diffed:
//!
//! ```text
//! open_ms=<f64> pages=<n> layout_ms=<f64>
//! render_ms=<f64> rss_kb=<n> peak_kb=<n> hash=<u64>
//! ...
//! ```
//!
//! Environment: `BOOK` (path), `N` (pages to render, default 20),
//! `SKIP` (pages to skip between renders, default 1), `W`, `H`, `FONT_SIZE`.
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
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

fn peak_kb() -> u64 {
    let text = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let number = rest.trim().trim_end_matches(" kB").trim();
            return number.parse().unwrap_or(0);
        }
    }
    0
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
    let count = positive("N", 20);
    let skip = positive("SKIP", 1) as usize;
    let start = positive("START", 1) as usize;
    let width = positive("W", 758);
    let height = positive("H", 1024);
    let font_size: f32 = std::env::var("FONT_SIZE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(22.0);

    let options = DocumentOptions {
        width,
        height,
        font_size,
        page_margin: 10.0,
        font_path: Some(PathBuf::from(
            "/home/frank/koreader/resources/fonts/noto/NotoSans-Regular.ttf",
        )),
        font_family: Some("Noto Sans".to_string()),
        fallback_font_paths: vec![PathBuf::from(
            "/home/frank/koreader/resources/fonts/freefont/FreeSerif.ttf",
        )],
        ..Default::default()
    };

    let started = Instant::now();
    let mut doc = Document::with_options(std::path::Path::new(&book), &options).expect("open");
    let pages = doc.page_count();
    let open_ms = started.elapsed().as_secs_f64() * 1000.0;
    println!(
        "open_ms={open_ms:.1} pages={pages} rss_kb={} peak_kb={}",
        rss_kb(),
        peak_kb()
    );

    let mut total_render = 0.0f64;
    let mut rendered = 0usize;
    let mut page = start.max(1);
    while rendered < count as usize && page <= pages {
        let started = Instant::now();
        let bitmap = doc.render_page(page).expect("render");
        let render_ms = started.elapsed().as_secs_f64() * 1000.0;
        total_render += render_ms;
        rendered += 1;
        let mut hasher = DefaultHasher::new();
        bitmap.data.hash(&mut hasher);
        // Ink pixels separate "rendered a page" from "rendered blank", which is
        // what the compatibility regression watches for.
        let ink = bitmap.data.iter().filter(|value| **value < 128).count();
        println!(
            "page={page} render_ms={render_ms:.1} rss_kb={} peak_kb={} ink={ink} hash={}",
            rss_kb(),
            peak_kb(),
            hasher.finish()
        );
        page += skip;
    }
    println!(
        "summary pages_rendered={rendered} total_ms={total_render:.1} avg_ms={:.2} peak_kb={}",
        if rendered > 0 {
            total_render / rendered as f64
        } else {
            0.0
        },
        peak_kb()
    );
    let memory = doc.memory_breakdown();
    println!(
        "memory source_kb={} source_capacity_kb={} chunks={} laid_out={} pages={} \
         page_meta_kb={} line_entries={} xpointer_kb={} anchors={} glyphs_kb={} \
         bitmaps_kb={} active_lines={} active_nodes={} fonts_kb={} \
         res_cache_kb={} pinned_kb={} svg_cache_kb={} page_map_source_kb={}",
        memory.source_bytes / 1024,
        memory.source_capacity / 1024,
        memory.chunk_count,
        memory.laid_out_chunks,
        memory.page_count,
        memory.page_meta_bytes / 1024,
        memory.line_entries,
        memory.xpointer_bytes / 1024,
        memory.anchor_entries,
        memory.glyph_cache_bytes / 1024,
        memory.page_bitmap_bytes / 1024,
        memory.active_line_boxes,
        memory.active_text_nodes,
        memory.font_bytes / 1024,
        memory.resource_cache_bytes / 1024,
        memory.pinned_bytes / 1024,
        memory.svg_cache_bytes / 1024,
        memory.page_map_source_bytes / 1024,
    );
}

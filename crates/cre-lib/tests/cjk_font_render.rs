//! Regression guard for glyph rasterization cost.
//!
//! The rasterizer used to be `fontdue::Font::from_bytes`, which outlines every
//! glyph in the font while parsing it. For a CJK font (NotoSansCJKsc: 16 MB,
//! ~65k glyphs) that measured 307 MB of RSS and 0.63 s for a single page,
//! which is an instant OOM on a 256 MB Kindle PW2 — the device showed
//! `Out of memory: Kill process ... (reader.lua)` right after the first page
//! that used the CJK fallback font.
//!
//! Rasterization now goes through `ttf-parser` (table directory only, glyphs
//! outlined on demand), so a page of Chinese text must stay far below that.

use std::path::{Path, PathBuf};

use cre_lib::{Document, DocumentOptions};

fn cjk_font() -> Option<PathBuf> {
    [
        "/tmp/ko/koreader/fonts/noto/NotoSansCJKsc-Regular.otf",
        "/usr/share/fonts/truetype/cjk/NotoSansSC-VF.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|path| path.exists())
}

fn peak_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with("VmHWM:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(0)
}

#[test]
fn cjk_page_rasters_without_whole_font_parse() {
    let Some(font) = cjk_font() else {
        eprintln!("skipping: no CJK test font available");
        return;
    };
    let dir = std::env::temp_dir().join("cre-cjk-render");
    std::fs::create_dir_all(&dir).unwrap();
    let html = dir.join("cjk.html");
    std::fs::write(
        &html,
        "<html><body><p>你好世界，这是一个测试。First page of Chinese text, \
         用来确认字形栅格化不会一次性解析整个字体文件。</p></body></html>",
    )
    .unwrap();

    let options = DocumentOptions {
        width: 600,
        height: 800,
        layout_height: 10_000,
        page_margin: 32.0,
        font_size: 20.0,
        font_path: Some(font),
        font_family: None,
        fallback_font_paths: Vec::new(),
        stylesheet: None,
        embedded_styles_enabled: true,
        viewport_scale: 1.0,
    };
    let before = peak_kb();
    let mut doc = Document::with_options(&html, &options).unwrap();
    let bitmap = doc.render_page(1).unwrap();
    let after = peak_kb();

    let ink = bitmap.data.iter().filter(|value| **value < 128).count();
    assert!(ink > 100, "expected rendered glyphs, got {ink} dark pixels");

    let growth = after.saturating_sub(before);
    assert!(
        growth < 120_000,
        "rasterizing one page grew the peak RSS by {growth} kB; \
         the rasterizer must not parse whole fonts up front"
    );
}

#[test]
fn unicode_outlines_still_render_for_the_cff_path() {
    // NotoSansCJKsc is an OpenType/CFF font; make sure outlines from the CFF
    // table end up on the page (fontdue used to only handle TrueType tables
    // well).
    let Some(font) = cjk_font() else {
        eprintln!("skipping: no CJK test font available");
        return;
    };
    let dir = std::env::temp_dir().join("cre-cjk-render");
    std::fs::create_dir_all(&dir).unwrap();
    let html = dir.join("cff.html");
    std::fs::write(&html, "<html><body><p>漢字かなカナ</p></body></html>").unwrap();
    let options = DocumentOptions {
        font_family: None,
        width: 400,
        height: 300,
        layout_height: 10_000,
        page_margin: 16.0,
        font_size: 40.0,
        font_path: Some(font),
        fallback_font_paths: Vec::new(),
        stylesheet: None,
        embedded_styles_enabled: true,
        viewport_scale: 1.0,
    };
    let mut doc = Document::with_options(&html, &options).unwrap();
    let bitmap = doc.render_page(1).unwrap();
    let ink = bitmap.data.iter().filter(|value| **value < 128).count();
    assert!(ink > 200, "expected CFF glyphs on the page, got {ink}");
}

#[test]
fn latin_text_renders_with_the_same_pipeline() {
    let font = Path::new("/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf");
    if !font.exists() {
        eprintln!("skipping: no DejaVu test font");
        return;
    }
    let dir = std::env::temp_dir().join("cre-cjk-render");
    std::fs::create_dir_all(&dir).unwrap();
    let html = dir.join("latin.html");
    std::fs::write(
        &html,
        "<html><body><p>Handgloves WOAH 1234</p></body></html>",
    )
    .unwrap();
    let options = DocumentOptions {
        font_family: None,
        width: 400,
        height: 300,
        layout_height: 10_000,
        page_margin: 16.0,
        font_size: 28.0,
        font_path: Some(font.to_path_buf()),
        fallback_font_paths: Vec::new(),
        stylesheet: None,
        embedded_styles_enabled: true,
        viewport_scale: 1.0,
    };
    let mut doc = Document::with_options(&html, &options).unwrap();
    let bitmap = doc.render_page(1).unwrap();
    let ink = bitmap.data.iter().filter(|value| **value < 128).count();
    assert!(ink > 200, "expected Latin glyphs on the page, got {ink}");
}

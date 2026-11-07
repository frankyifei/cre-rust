use std::path::{Path, PathBuf};

use cre_lib::{Document, PageBitmap};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn require_font() {
    let font = Path::new("/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf");
    assert!(
        font.exists(),
        "missing required test font: {}",
        font.display()
    );
}

fn min_in(bitmap: &PageBitmap, x: u32, y: u32, radius: u32) -> u8 {
    let mut min = u8::MAX;
    for row in y.saturating_sub(radius)..=y + radius {
        for col in x.saturating_sub(radius)..=x + radius {
            min = min.min(bitmap.data[(row * bitmap.width + col) as usize]);
        }
    }
    min
}

fn max_in(bitmap: &PageBitmap, x: u32, y: u32, radius: u32) -> u8 {
    let mut max = u8::MIN;
    for row in y.saturating_sub(radius)..=y + radius {
        for col in x.saturating_sub(radius)..=x + radius {
            max = max.max(bitmap.data[(row * bitmap.width + col) as usize]);
        }
    }
    max
}

#[test]
fn cdata_wrapped_inline_styles_are_applied() {
    require_font();

    let mut doc = Document::open(fixture("cdata-image.epub")).unwrap();
    let bitmap = doc.render_page(1).unwrap();

    assert!(
        min_in(&bitmap, 100, 42, 4) < 64,
        "the CDATA-wrapped rule should paint the black block"
    );
    assert!(
        max_in(&bitmap, 100, 12, 4) > 200,
        "above the styled block the page should stay white"
    );
}

#[test]
fn png_images_are_decoded_and_painted() {
    require_font();

    let mut doc = Document::open(fixture("cdata-image.epub")).unwrap();
    let bitmap = doc.render_page(1).unwrap();

    assert!(
        min_in(&bitmap, 60, 67, 4) < 64,
        "the PNG fixture should be painted inside its layout box"
    );
    assert!(
        max_in(&bitmap, 120, 67, 4) > 200,
        "outside the image box the page should stay white"
    );
}

#[test]
fn svg_images_are_decoded_and_painted() {
    require_font();

    let mut doc = Document::open(fixture("svg-image.epub")).unwrap();
    let bitmap = doc.render_page(1).unwrap();

    assert!(
        min_in(&bitmap, 60, 67, 4) < 64,
        "the SVG fixture should be painted inside its layout box"
    );
    assert!(
        max_in(&bitmap, 120, 67, 4) > 200,
        "outside the SVG image box the page should stay white"
    );
}

/// An inline `<svg>` is handed to usvg on its own, so the document's CSS never
/// reached it (`.fil0 { fill: url(#star) }` was ignored and the shape painted
/// black), and blitz-paint's SVG renderer cannot paint a `url(#pattern)` fill
/// (it draws a translucent red box instead). The fixture must show the green
/// star pattern, so neither a black nor a red result passes.
#[test]
fn inline_svg_uses_document_css_and_pattern_fills() {
    require_font();

    let mut doc = Document::open(fixture("svg-pattern.epub")).unwrap();
    let (bitmap, _, _) = doc.render_page_color_with_stats(1).unwrap();

    let mut green = 0usize;
    let mut red = 0usize;
    for pixel in bitmap.data.as_chunks::<4>().0 {
        let (r, g) = (pixel[0] as i32, pixel[1] as i32);
        if g > r + 16 {
            green += 1;
        }
        if r > g + 16 {
            red += 1;
        }
    }
    assert!(
        green > 500,
        "expected the green star pattern to be painted, found {green} green pixels"
    );
    assert_eq!(red, 0, "no part of the pattern should paint red");
}

#[test]
fn forced_page_breaks_create_new_pages() {
    require_font();

    let doc = Document::open(fixture("pagebreak.epub")).unwrap();
    assert!(
        doc.page_count() >= 3,
        "page-break-before declarations should create extra pages"
    );

    let page1 = doc.page_text(1).unwrap();
    let page2 = doc.page_text(2).unwrap();
    let page3 = doc.page_text(3).unwrap();

    assert!(page1.contains("MARK-A"));
    assert!(!page1.contains("MARK-B"));
    assert!(page2.contains("MARK-B"));
    assert!(!page2.contains("MARK-C"));
    assert!(page3.contains("MARK-C"));
}

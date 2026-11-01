//! Pagination must keep images whole.
//!
//! Pagination used to walk only text line boxes, so a block image taller than
//! the remaining page space was painted once and clipped: the rest of the
//! image never appeared on any page. Images are now pagination units, and an
//! image taller than a page is scaled to fit one (as crengine does).

use std::path::{Path, PathBuf};

use cre_lib::{Document, DocumentOptions};

fn options() -> DocumentOptions {
    DocumentOptions {
        width: 600,
        height: 800,
        layout_height: 100_000,
        page_margin: 20.0,
        font_size: 18.0,
        font_path: None,
        fallback_font_paths: Vec::new(),
        stylesheet: None,
        embedded_styles_enabled: true,
        viewport_scale: 1.0,
    }
}

fn write_page(name: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("cre-image-pages");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, format!("<html><body>{body}</body></html>")).unwrap();
    path
}

fn tall_svg() -> &'static str {
    "<svg width=\"200\" height=\"3000\" viewBox=\"0 0 200 3000\">\
     <rect width=\"200\" height=\"3000\" fill=\"#cccccc\"/>\
     <rect width=\"200\" height=\"40\" fill=\"#000000\"/>\
     <rect y=\"2960\" width=\"200\" height=\"40\" fill=\"#000000\"/>\
     </svg>"
}

fn dark_pixels(bitmap: &cre_lib::PageBitmap, rows: std::ops::Range<u32>) -> usize {
    let mut count = 0;
    for y in rows.start.min(bitmap.height)..rows.end.min(bitmap.height) {
        for x in 0..bitmap.width {
            if bitmap.data[(y * bitmap.width + x) as usize] < 128 {
                count += 1;
            }
        }
    }
    count
}

#[test]
fn tall_image_gets_its_own_page_and_is_not_clipped() {
    let path = write_page(
        "tall.html",
        &format!("<p>before</p>{}<p>after</p>", tall_svg()),
    );
    let mut doc = Document::with_options(Path::new(&path), &options()).unwrap();
    assert!(
        doc.page_count() >= 3,
        "expected the image on its own page, got {} pages",
        doc.page_count()
    );
    assert!(doc.page_text(1).unwrap().contains("before"));

    // The image page has no text but does have ink at both the very top and the
    // very bottom of the page, i.e. the whole image fitted on the page.
    let image_page = (1..=doc.page_count())
        .find(|page| {
            doc.page_text(*page)
                .map(|text| text.trim().is_empty())
                .unwrap_or(false)
        })
        .expect("no image-only page");
    let bitmap = doc.render_page(image_page).unwrap();
    let top_ink = dark_pixels(&bitmap, 0..40);
    let bottom_ink = dark_pixels(&bitmap, bitmap.height - 40..bitmap.height);
    assert!(
        top_ink > 50,
        "image is missing at the top ({top_ink} dark pixels)"
    );
    assert!(
        bottom_ink > 50,
        "image is clipped at the bottom ({bottom_ink} dark pixels)"
    );
}

#[test]
fn small_image_stays_with_the_text() {
    let small = "<svg width=\"40\" height=\"40\" viewBox=\"0 0 40 40\">\
                 <rect width=\"40\" height=\"40\" fill=\"#000000\"/></svg>";
    let path = write_page("small.html", &format!("<p>text</p>{small}<p>more</p>"));
    let doc = Document::with_options(Path::new(&path), &options()).unwrap();
    assert_eq!(
        doc.page_count(),
        1,
        "a small image must not force a page break"
    );
}

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
        font_family: None,
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

/// Image hit-testing gets screen pixels from KOReader, but `getClientBoundingRect`
/// is in CSS pixels. At a HiDPI viewport (scale 2) the image's device rectangle
/// is twice as wide/tall as its layout rectangle. Without dividing the touch
/// point by the scale, tapping the visible image finds nothing.
#[test]
fn image_hit_test_uses_device_coordinates() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/svg-image.epub");
    let mut options = options();
    options.width = 600;
    options.height = 800;
    options.page_margin = 20.0;
    options.viewport_scale = 2.0;
    let mut doc = Document::with_options(&path, &options).unwrap();

    let bitmap = doc.render_page(1).unwrap();
    let mut last_black_row = None;
    for y in (0..bitmap.height).rev() {
        if (0..bitmap.width).any(|x| bitmap.data[(y * bitmap.width + x) as usize] < 128) {
            last_black_row = Some(y);
            break;
        }
    }
    let y = last_black_row.expect("the fixture image should be painted");
    let xs: Vec<u32> = (0..bitmap.width)
        .filter(|x| bitmap.data[(y * bitmap.width + x) as usize] < 128)
        .collect();
    let left = *xs.first().unwrap();
    let right = *xs.last().unwrap();
    assert!(right > left, "expected a painted image row");

    // The bottom-right corner of the painted (device) image is outside the
    // image's CSS rectangle, so it only hits after the scale conversion.
    let position = cre_lib::PagePosition {
        page: 1,
        x: (right - 1) as f32,
        y: (y - 1) as f32,
    };
    assert!(
        doc.image_data_at(position).unwrap().is_some(),
        "device image corner {position:?} did not hit the image"
    );
}

/// `get_client_bounding_rect` returns coordinates relative to the document's
/// current viewport scroll. `render_page` leaves that scroll at the last page
/// rendered. Image hit-testing must reset it for the page it is asked about,
/// otherwise an image on page 1 stops hitting after page 2 has been rendered,
/// and an image on page N can answer for page N-1.
#[test]
fn image_hit_test_uses_the_queried_page_origin() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/image-page-break.epub");
    let mut options = options();
    options.width = 600;
    options.height = 800;
    options.page_margin = 20.0;
    let mut doc = Document::with_options(&path, &options).unwrap();

    // The first page is the picture. Remember a dark point in its painted bbox.
    let bitmap = doc.render_page(1).unwrap();
    let mut point = None;
    'rows: for y in 0..bitmap.height {
        for x in 0..bitmap.width {
            if bitmap.data[(y * bitmap.width + x) as usize] < 128 {
                point = Some((x, y));
                break 'rows;
            }
        }
    }
    let (x, y) = point.expect("the fixture image should be painted");
    let position = cre_lib::PagePosition {
        page: 1,
        x: (x + 2) as f32,
        y: (y + 2) as f32,
    };
    assert!(doc.image_data_at(position).unwrap().is_some());

    // Rendering page 2 moves the viewport scroll. Page 1 must still hit.
    assert!(doc.page_count() > 1);
    doc.render_page(2).unwrap();
    assert!(
        doc.image_data_at(position).unwrap().is_some(),
        "page 1 image missed after page 2 changed the viewport scroll"
    );
}

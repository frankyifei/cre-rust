use std::path::{Path, PathBuf};

use cre_lib::{ColorPageBitmap, Document, DocumentOptions};

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

fn options_with_stylesheet(stylesheet: Option<String>) -> DocumentOptions {
    DocumentOptions {
        width: 600,
        height: 800,
        layout_height: 100_000,
        page_margin: 32.0,
        font_size: 18.0,
        font_path: Some(
            Path::new("/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf").to_path_buf(),
        ),
        fallback_font_paths: Vec::new(),
        stylesheet,
        embedded_styles_enabled: true,
        viewport_scale: 1.0,
    }
}

fn colored_pixel_count(bitmap: &ColorPageBitmap) -> usize {
    let mut colored = 0usize;
    let px_rows = bitmap.data.as_chunks::<4>().0;
    for px in px_rows {
        let (r, g, b) = (px[0], px[1], px[2]);
        let spread = r.max(g).max(b).saturating_sub(r.min(g).min(b));
        if spread > 40 && r.max(g).max(b) > 80 {
            colored += 1;
        }
    }
    colored
}

#[test]
fn color_glyph_gamma_preserves_colored_text_and_borders() {
    require_font();
    let stylesheet = Some(
        "body { background-color: #ffffff; }\n\
         p { color: #ff0000; }\n\
         div { border: 6px solid rgb(0,0,255); margin: 30px; }\n"
            .to_string(),
    );
    let mut doc = Document::with_options(
        fixture("pagebreak.epub"),
        &options_with_stylesheet(stylesheet),
    )
    .unwrap();

    doc.set_font_gamma(1.6);
    let mut total = 0usize;
    for page in 1..=doc.page_count() {
        let (bitmap, _, _) = doc.render_page_color_with_stats(page).unwrap();
        let colored = colored_pixel_count(&bitmap);
        total += colored;
        println!("page {page}: colored={colored}");
    }
    assert!(
        total > 100,
        "expected colored ink (red text, blue borders); got {total} colored pixels with gamma 1.6"
    );

    doc.set_font_gamma(1.0);
    let mut total_one = 0usize;
    for page in 1..=doc.page_count() {
        let (bitmap, _, _) = doc.render_page_color_with_stats(page).unwrap();
        total_one += colored_pixel_count(&bitmap);
    }
    assert!(
        total_one > 100,
        "expected colored ink at gamma 1.0 too; got {total_one}"
    );
}

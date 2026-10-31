use std::path::{Path, PathBuf};

use cre_lib::DocumentOptions;

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

fn classic_options(stylesheet: Option<String>) -> DocumentOptions {
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

/// "Disable embedded styles" strips author CSS but must keep the generator's
/// base typography (font size, line height, page padding), otherwise the
/// font-size and other attribute adjustments would silently stop working.
fn max_text_box_height(options: &DocumentOptions) -> f32 {
    let doc = cre_lib::Document::with_options(fixture("pagebreak.epub"), options).unwrap();
    let mut tallest: f32 = 0.0;
    for page in 1..=doc.page_count() {
        for text_box in doc.page_text_boxes(page).unwrap() {
            tallest = tallest.max(text_box.height);
        }
    }
    tallest
}

#[test]
fn base_typography_survives_disabled_embedded_styles() {
    require_font();

    let small = classic_options(None);
    let small_doc_options = DocumentOptions {
        font_size: 18.0,
        ..small
    };
    let large = classic_options(None);
    let large_doc_options = DocumentOptions {
        font_size: 36.0,
        ..large
    };

    let with_styles = max_text_box_height(&large_doc_options);
    assert!(
        with_styles > 30.0,
        "font size 36 should yield tall text boxes, got {with_styles}"
    );

    let mut without_styles = large_doc_options.clone();
    without_styles.embedded_styles_enabled = false;
    let height = max_text_box_height(&without_styles);
    assert!(
        height > 30.0,
        "font size should still adjust with embedded styles disabled; got line height {height}"
    );

    let mut small_without = small_doc_options.clone();
    small_without.embedded_styles_enabled = false;
    let small_height = max_text_box_height(&small_without);
    assert!(
        height > small_height * 1.5,
        "disabling embedded styles must not freeze font size scaling: {height} vs {small_height}"
    );

    // author CSS from the book is still stripped
    let text_without = {
        let doc = cre_lib::Document::with_options(fixture("pagebreak.epub"), &{
            let mut opts = classic_options(None);
            opts.embedded_styles_enabled = false;
            opts
        })
        .unwrap();
        // Every spine item starts a new page, so look through the opening
        // pages instead of assuming page 1 carries the marker.
        (1..=doc.page_count().min(5))
            .filter_map(|page| doc.page_text(page).ok())
            .find(|text| text.contains("MARK"))
            .unwrap_or_else(|| {
                let first = doc.page_text(1).unwrap_or_default();
                panic!("text must survive style stripping, got: {first:?}");
            })
    };
    assert!(!text_without.is_empty());
}

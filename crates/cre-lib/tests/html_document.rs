use std::path::{Path, PathBuf};

use cre_lib::{Document, DocumentFormat, DocumentOptions};

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

fn options() -> DocumentOptions {
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
        font_family: None,
        stylesheet: None,
        embedded_styles_enabled: true,
        viewport_scale: 1.0,
    }
}

/// Ruby is not EPUB-specific: KF8/MOBI documents carry the same `<ruby>`
/// markup. Blitz has no ruby box, so the shared preprocessing stacks the note
/// above its base; that band makes the paragraph taller, which is what pushes
/// the next paragraph down. Before the conversion the note rendered inline.
#[test]
fn ruby_notes_are_stacked_on_every_format() {
    require_font();
    let plain = Document::with_options(fixture("no-ruby.html"), &options()).unwrap();
    let ruby = Document::with_options(fixture("ruby.html"), &options()).unwrap();

    let second_top = |doc: &Document| -> f32 {
        doc.page_text_boxes(1)
            .unwrap()
            .into_iter()
            .find(|text| text.text.contains("SECOND"))
            .expect("the second paragraph should be on page 1")
            .y
    };
    let plain_top = second_top(&plain);
    let ruby_top = second_top(&ruby);
    assert!(
        ruby_top > plain_top + 2.0,
        "the ruby line should be taller: plain {plain_top}, ruby {ruby_top}"
    );

    let text = ruby.page_text(1).unwrap();
    assert!(
        text.contains("ㄒㄧㄤ"),
        "the note should still be text: {text:?}"
    );
}

#[test]
fn opens_standalone_html_file() {
    require_font();
    let mut doc = Document::with_options(fixture("sample.html"), &options()).unwrap();

    assert_eq!(doc.metadata().format, DocumentFormat::Html);
    assert_eq!(doc.metadata().title.as_deref(), Some("sample"));
    assert!(doc.page_count() >= 1);

    let text = doc.page_text(1).unwrap();
    assert!(text.contains("MARK-HTML-ALPHA"), "page text: {text:?}");
    assert!(text.contains("MARK-HTML-BETA"), "page text: {text:?}");

    // the inline stylesheet is honoured: red text survives into the color path
    let (bitmap, _, _) = doc.render_page_color_with_stats(1).unwrap();
    let mut colored = 0usize;
    for px in bitmap.data.as_chunks::<4>().0 {
        let (r, g, b) = (px[0], px[1], px[2]);
        let spread = r.max(g).max(b).saturating_sub(r.min(g).min(b));
        if spread > 40 && r.max(g).max(b) > 80 {
            colored += 1;
        }
    }
    assert!(colored > 20, "expected red text pixels, got {colored}");
}

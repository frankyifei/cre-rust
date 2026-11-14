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

/// MOBI/KF8 mark their parts with `<section class="cre-spine">`; a part whose
/// only content is an image is a plate page and has to fill the page, like an
/// EPUB spine document that is one picture. A 1x1 image would otherwise stay
/// one pixel.
#[test]
fn picture_only_sections_fill_the_page() {
    require_font();
    let mut doc = Document::with_options(fixture("picture-section.html"), &options()).unwrap();
    let (bitmap, _, _) = doc.render_page_color_with_stats(1).unwrap();
    let pixels = bitmap.data.as_chunks::<4>().0.len() as f32;
    let red = bitmap
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[0] > 180 && pixel[1] < 120 && pixel[2] < 120)
        .count() as f32;
    assert!(
        red > pixels * 0.3,
        "the plate should be scaled to the page, covered {:.0}%",
        red / pixels * 100.0
    );
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

/// A book's CSS often arrives as a linked stylesheet rather than an inline
/// `<style>` block, and an EPUB chapter, a KF8 flow and a standalone HTML all
/// do. The reader-side rewrites - chiefly the absolute `font-size` keywords,
/// which are a size only once the medium (the reader's font size) is known -
/// therefore have to reach the sheet whichever way it is served, or a book's
/// `x-large` chapter title stays at stylo's 16px medium and ignores the
/// setting. This pins the invariant: linked CSS renders exactly like the same
/// rules inline.
#[test]
fn a_linked_stylesheet_renders_like_the_same_rules_inline() {
    require_font();

    let heading_height = |doc: &Document| -> f32 {
        doc.page_text_boxes(1)
            .unwrap()
            .into_iter()
            .find(|text| text.text.contains("Chapter One"))
            .expect("the heading should be on page 1")
            .height
    };

    let mut heights = Vec::new();
    for font_size in [18.0, 26.0] {
        let options = DocumentOptions {
            font_size,
            ..options()
        };
        let linked = Document::with_options(fixture("heading-linked.html"), &options).unwrap();
        let inline = Document::with_options(fixture("heading-inline.html"), &options).unwrap();
        assert_eq!(linked.page_count(), inline.page_count(), "at {font_size}px");
        assert_eq!(
            heading_height(&linked),
            heading_height(&inline),
            "a linked `x-large` heading must resolve like an inline one, at {font_size}px"
        );
        heights.push(heading_height(&linked));
    }
    assert!(
        heights[1] > heights[0] * 1.3,
        "an absolute heading must scale with the setting: {heights:?}"
    );
}

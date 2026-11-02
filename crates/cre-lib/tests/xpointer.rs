use std::path::{Path, PathBuf};

use cre_lib::{Document, parse_text_position};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/koreader-test-data")
        .join(name)
}

fn require_resources() {
    let font = Path::new("/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf");
    assert!(
        font.exists(),
        "missing required test font: {}",
        font.display()
    );
    for name in ["juliet.epub", "sample.txt"] {
        let path = fixture(name);
        assert!(
            path.exists(),
            "missing required fixture: {}",
            path.display()
        );
    }
}

#[test]
fn xpointer_parse_accepts_generated_point_format() {
    let input = "/body/DocFragment[2]/body/p[3]/span[1]/text().42";
    let position = parse_text_position(input).unwrap();
    assert_eq!(position.spine_index, 2);
    assert_eq!(position.codepoint_offset, 42);
    assert_eq!(position.path[0].tag, "p");
    assert_eq!(position.path[0].index, 3);
    assert_eq!(position.path[1].tag, "span");
    assert_eq!(position.path[1].index, 1);
}

#[test]
fn xpointer_parse_accepts_xpointer_scheme() {
    let input = "xpointer(/body/DocFragment[1]/body/text().0)";
    let position = parse_text_position(input).unwrap();
    assert_eq!(position.spine_index, 1);
    assert_eq!(position.path.len(), 0);
    assert_eq!(position.codepoint_offset, 0);
}

#[test]
fn xpointer_parse_rejects_invalid_paths() {
    for input in [
        "not-an-xpointer",
        "/body/DocFragment[0]/body/text().0",
        "/body/text().0",
        "/body/DocFragment[1]/body/p[0]/text().0",
        "/body/DocFragment[1]/body/p[1]/wrong().0",
    ] {
        assert!(parse_text_position(input).is_err(), "accepted {input}");
    }
}

#[test]
fn txt_page_xpointers_round_trip_all_pages() {
    require_resources();

    let mut doc = Document::open(fixture("sample.txt")).unwrap();
    let page_count = doc.page_count();
    assert!(page_count >= 1);

    for page in 1..=page_count {
        let xpointer = doc.xpointer_from_page(page).unwrap();
        assert!(
            xpointer.starts_with("/body/DocFragment[1]/body"),
            "unexpected XPointer for page {page}: {xpointer}"
        );

        let resolved = doc.page_from_xpointer(&xpointer).unwrap();
        assert_eq!(resolved, page, "failed to resolve XPointer for page {page}");
        assert_eq!(doc.go_to_xpointer(&xpointer).unwrap(), page);
        assert_eq!(doc.current_page(), page);
    }

    assert!(doc.xpointer_from_page(0).is_err());
    assert!(doc.xpointer_from_page(page_count + 1).is_err());
    assert!(
        doc.page_from_xpointer("/body/DocFragment[2]/body/text().0")
            .is_err()
    );
}

#[test]
fn current_position_and_position_to_xpointer_round_trip() {
    require_resources();

    let mut doc = Document::open(fixture("sample.txt")).unwrap();
    doc.go_to_page(2).unwrap();

    let generated = doc.xpointer_from_page(2).unwrap();
    let current = doc.current_position().unwrap();
    let from_position = doc.xpointer_from_position(&current).unwrap();

    assert_eq!(generated, from_position);
    assert_eq!(doc.page_from_xpointer(&from_position).unwrap(), 2);
}

#[test]
fn epub_page_xpointers_round_trip_for_selected_pages() {
    require_resources();

    let doc = Document::open(fixture("juliet.epub")).unwrap();
    let page_count = doc.page_count();
    assert!(page_count > 10);

    let pages = [1, page_count / 2, page_count];
    for page in pages {
        let xpointer = doc.xpointer_from_page(page).unwrap();
        assert!(
            xpointer.starts_with("/body/DocFragment["),
            "unexpected XPointer for page {page}: {xpointer}"
        );

        let parsed = parse_text_position(&xpointer).unwrap();
        assert!(parsed.spine_index >= 1);

        let resolved = doc.page_from_xpointer(&xpointer).unwrap();
        assert_eq!(resolved, page, "failed to resolve XPointer for page {page}");
    }
}

#[test]
fn epub_toc_entries_expose_page_and_xpointer_round_trip() {
    require_resources();

    let mut doc = Document::open(fixture("juliet.epub")).unwrap();
    let page_count = doc.page_count();
    let toc = doc.toc().to_vec();
    assert!(!toc.is_empty());
    assert!(toc.iter().all(|entry| entry.page.is_some()));

    for entry in toc {
        let Some(page) = entry.page else {
            continue;
        };
        assert!(page >= 1 && page <= page_count);

        let Some(xpointer) = entry.xpointer.as_ref() else {
            panic!("TOC entry {} has no XPointer", entry.title);
        };
        assert!(
            xpointer.starts_with("/body/DocFragment["),
            "unexpected XPointer for TOC entry {}: {xpointer}",
            entry.title
        );

        let parsed = parse_text_position(xpointer).unwrap();
        assert!(parsed.spine_index >= 1);
        assert_eq!(
            doc.page_from_xpointer(xpointer).unwrap(),
            page,
            "failed to resolve TOC XPointer for {}",
            entry.title
        );
        assert_eq!(
            doc.go_to_xpointer(xpointer).unwrap(),
            page,
            "failed to navigate TOC XPointer for {}",
            entry.title
        );
    }
}

fn html_options() -> cre_lib::DocumentOptions {
    cre_lib::DocumentOptions {
        width: 600,
        height: 800,
        layout_height: 100_000,
        page_margin: 32.0,
        font_size: 18.0,
        font_path: Some(
            Path::new("/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf").to_path_buf(),
        ),
        font_family: None,
        fallback_font_paths: Vec::new(),
        stylesheet: None,
        embedded_styles_enabled: true,
        viewport_scale: 1.0,
    }
}

/// A heading line can be made of several text nodes (`<h1><span>Chapter</span>
/// One</h1>`). The line bounds then start in the span and end in the heading's
/// own text node, which `TextPosition` ordering does not compare in document
/// order; the old code found no line and walked forward until the stack
/// overflowed. This is the regression test for that crash.
#[test]
fn multi_node_line_start_maps_to_a_point() {
    require_resources();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/heading.html");
    assert!(path.exists(), "missing fixture: {}", path.display());
    let doc = Document::with_options(&path, &html_options()).unwrap();
    let xpointer = doc.xpointer_from_page(1).unwrap();
    let (x, y) = doc
        .point_from_xpointer(&xpointer)
        .unwrap()
        .expect("the start of a multi-node line must map to a point");
    let first = doc
        .page_text_boxes(1)
        .unwrap()
        .into_iter()
        .next()
        .expect("page 1 has no text boxes");
    assert_eq!(first.text, "Chapter One");
    assert!(
        (y - first.y).abs() < 1.0,
        "y {y} is not on the heading line at {}",
        first.y
    );
    assert!(x >= first.x && x <= first.x + first.width);
}

fn synthetic_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn small_page_options() -> cre_lib::DocumentOptions {
    cre_lib::DocumentOptions {
        width: 320,
        height: 240,
        layout_height: 100_000,
        page_margin: 10.0,
        font_size: 16.0,
        font_path: Some(
            Path::new("/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf").to_path_buf(),
        ),
        font_family: None,
        fallback_font_paths: Vec::new(),
        stylesheet: None,
        embedded_styles_enabled: true,
        viewport_scale: 1.0,
    }
}

/// A file-level TOC target is a spine document. Its forced page break is
/// applied at the first line that carries text, so a title page with a large
/// top margin has its section top on the previous page. The TOC must use the
/// page of the title, not the page of the section top.
#[test]
fn toc_file_target_uses_the_page_of_the_title() {
    require_resources();
    let path = synthetic_fixture("toc-title-margin.epub");
    assert!(path.exists(), "missing fixture: {}", path.display());
    let options = small_page_options();
    let doc = Document::with_options(&path, &options).unwrap();
    let entry = doc
        .toc()
        .iter()
        .find(|entry| entry.title == "Chapter One")
        .expect("TOC entry")
        .clone();
    let page = entry.page.expect("TOC page");
    assert!(page > 1, "title page should not be the first page");
    let normalize = |text: String| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = normalize(doc.page_text(page).unwrap_or_default());
    assert!(
        text.contains("Unique Chapter Title"),
        "TOC page {page} does not show the title: {text:?}"
    );
    let previous = normalize(doc.page_text(page - 1).unwrap_or_default());
    assert!(
        !previous.contains("Unique Chapter Title"),
        "TOC page {page} is one page late"
    );
    assert_eq!(doc.page_from_href(&entry.href).unwrap(), page);
    let xpointer = entry.xpointer.as_deref().expect("TOC xpointer");
    assert_eq!(doc.page_from_xpointer(xpointer).unwrap(), page);

    let mut doc = Document::with_options(&path, &options).unwrap();
    assert_eq!(doc.go_to_xpointer(xpointer).unwrap(), page);
    assert_eq!(doc.go_to_href(&entry.href).unwrap(), page);
}

fn cover_options() -> cre_lib::DocumentOptions {
    cre_lib::DocumentOptions {
        width: 540,
        height: 720,
        layout_height: 100_000,
        page_margin: 10.0,
        font_size: 22.0,
        font_path: Some(
            Path::new("/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf").to_path_buf(),
        ),
        font_family: None,
        fallback_font_paths: Vec::new(),
        stylesheet: None,
        embedded_styles_enabled: true,
        viewport_scale: 1.0,
    }
}

fn normalized(text: String) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first image is slightly too tall for the space that is left on the
/// page. It must be fitted to the page, not pushed to page 2 (which would
/// leave page 1 blank). The section after it must still start on a new page.
#[test]
fn first_page_image_is_fitted_before_the_section_break() {
    require_resources();
    let path = synthetic_fixture("cover-overflow.epub");
    assert!(path.exists(), "missing fixture: {}", path.display());
    let options = cover_options();
    let mut doc = Document::with_options(&path, &options).unwrap();

    assert_eq!(
        doc.page_from_href("OPS/cover.xhtml").unwrap(),
        1,
        "the cover must be on page 1"
    );
    let ink = doc
        .render_page(1)
        .unwrap()
        .data
        .iter()
        .filter(|value| **value < 128)
        .count();
    assert!(ink > 1000, "page 1 has no cover image (ink={ink})");

    assert_eq!(
        doc.page_from_href("OPS/after.xhtml").unwrap(),
        2,
        "the section after an image-only page must start on a new page"
    );
    let text = normalized(doc.page_text(2).unwrap_or_default());
    assert!(
        text.contains("After cover"),
        "page 2 does not hold the next section: {text:?}"
    );

    let xpointer = doc.xpointer_from_page(2).unwrap();
    assert_eq!(doc.page_from_xpointer(&xpointer).unwrap(), 2);
}

/// A section after an image-only page must start on a new page. The paginator
/// used to count only text lines, so `page-break-before` after an image was
/// skipped and the following text shared the image page.
#[test]
fn section_break_after_an_image_page_starts_a_new_page() {
    require_resources();
    let path = synthetic_fixture("image-page-break.epub");
    assert!(path.exists(), "missing fixture: {}", path.display());
    let options = cover_options();
    let doc = Document::with_options(&path, &options).unwrap();

    assert_eq!(doc.page_from_href("OPS/image.xhtml").unwrap(), 1);
    assert_eq!(
        doc.page_from_href("OPS/after.xhtml").unwrap(),
        2,
        "the section after the image must start on page 2"
    );
    let text = normalized(doc.page_text(2).unwrap_or_default());
    assert!(
        text.contains("After image"),
        "page 2 does not hold the next section: {text:?}"
    );

    // The image page has no text line, so page 2 is the first page with a
    // line-based page-start XPointer. It must round-trip.
    let xpointer = doc.xpointer_from_page(2).unwrap();
    assert_eq!(
        doc.page_from_xpointer(&xpointer).unwrap(),
        2,
        "page 2 xpointer did not round-trip: {xpointer}"
    );
}

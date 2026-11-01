//! Highlights must work on every page, not just the first.
//!
//! `juliet.epub` broke exactly that: it repeats the same sentence in several
//! paragraphs, the line -> source-position mapping attributed one page's first
//! line to a body-level whitespace text node, and that page start then sorted
//! before the previous page. A word on page 1 resolved to page 2, so the boxes
//! for its highlight were computed for the wrong page and came back empty (the
//! dictionary still worked, because hit-testing uses the page's own lines).
//!
//! This reproduces the shape of that book without shipping it: repeated
//! paragraphs, an SVG and an image between them, at the PW2 viewport.

use std::path::PathBuf;

use cre_lib::{Document, DocumentOptions, PagePosition};

const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="80" height="60">
  <rect width="80" height="60" fill="#ccc"/>
</svg>"##;

// A 1x1 GIF, enough for the image path.
const GIF: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0xFF, 0xFF, 0xFF,
    0x00, 0x00, 0x00, 0x21, 0xF9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2C, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3B,
];

fn repeated_paragraphs_book() -> PathBuf {
    let directory = std::env::temp_dir().join("cre-highlight-test");
    std::fs::create_dir_all(&directory).unwrap();
    let image = directory.join("pixel.gif");
    std::fs::write(&image, GIF).unwrap();
    let svg = directory.join("figure.svg");
    std::fs::write(&svg, SVG).unwrap();

    let mut body = String::new();
    // The same short sentence every time, like juliet.epub's repeated title
    // pages, with an SVG and a bitmap between them.
    for index in 0..200 {
        body.push_str("<div>This is a book and this is a paragraph of a book</div>\n");
        if index == 2 {
            body.push_str("<img src=\"figure.svg\" alt=\"figure\"/>\n");
            body.push_str(&format!(
                "<img src=\"{}\" alt=\"pixel\"/>\n",
                image.display()
            ));
        }
    }
    let html = format!("<html><body>{body}</body></html>");
    let path = directory.join("book.html");
    std::fs::write(&path, html).unwrap();
    path
}

#[test]
fn every_page_produces_highlight_boxes_for_its_words() {
    let path = repeated_paragraphs_book();
    let options = DocumentOptions {
        width: 758,
        height: 1024,
        font_size: 22.0,
        ..Default::default()
    };
    let doc = Document::with_options(&path, &options).expect("open");
    assert!(doc.page_count() > 3, "book is too short to test paging");

    let mut checked = 0usize;
    for page in 1..=doc.page_count().min(12) {
        for y in (30..options.height as i32 - 30).step_by(37) {
            let Ok(Some(hit)) = doc.word_at(PagePosition {
                page,
                x: options.width as f32 * 0.45,
                y: y as f32,
            }) else {
                continue;
            };
            checked += 1;
            let start = hit.xpointer_range.start.to_xpointer();
            let end = hit
                .xpointer_range
                .end
                .as_ref()
                .map(|position| position.to_xpointer())
                .unwrap_or_else(|| panic!("word {:?} has no end position", hit.text));
            assert_ne!(
                start, end,
                "word {:?} on page {page} has a zero-length range",
                hit.text
            );
            assert_eq!(
                doc.page_from_xpointer(&start).ok(),
                Some(page),
                "word {:?} on page {page} resolves to another page",
                hit.text
            );
            let boxes = doc.boxes_between_xpointers(&start, &end).unwrap();
            assert!(
                !boxes.is_empty(),
                "word {:?} on page {page} produced no highlight boxes ({start} .. {end})",
                hit.text
            );
            assert!(
                boxes.iter().any(|text_box| text_box.page == page),
                "word {:?} on page {page} only produced boxes on other pages",
                hit.text
            );
        }
    }
    assert!(checked > 50, "only {checked} words were tested");
}

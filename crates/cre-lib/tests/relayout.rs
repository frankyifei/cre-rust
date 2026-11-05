//! A settings change must reuse the chunks the document already has.
//!
//! `cre_lib::Document::relayout` lays out only the chunk holding the current
//! position and keeps every other chunk's pages as a provisional page list,
//! which is how crengine's partial rerendering makes a font change instant on a
//! large book. These tests pin down that the book still ends up with exactly the
//! pages a full layout would produce.

use std::path::{Path, PathBuf};

use cre_lib::{Document, DocumentOptions};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/koreader-test-data")
        .join(name)
}

fn options(font_size: f32) -> DocumentOptions {
    DocumentOptions {
        font_size,
        ..Default::default()
    }
}

#[test]
fn font_change_relayouts_only_the_chunk_being_read() {
    // juliet is split into several chunks, which is what this test needs.
    let path = fixture("juliet.epub");

    let mut doc = Document::with_options(&path, &options(18.0)).unwrap();
    let pages_at_18 = doc.page_count();
    assert!(pages_at_18 > 0);

    // Read into the middle of the book: after the font change there are chunks on
    // both sides that are still provisional.
    doc.go_to_page(pages_at_18 / 2).unwrap();
    let anchor = doc.xpointer_from_page(doc.current_page()).unwrap();

    assert!(
        doc.can_relayout_with(&options(24.0)),
        "an EPUB font change should be applied without reopening the document"
    );
    assert!(doc.relayout(&options(24.0)).unwrap());

    // The reader stays where they were: the xpointer resolves to the page we are
    // on, because that chunk was laid out again.
    doc.go_to_xpointer(&anchor).unwrap();
    assert_eq!(doc.page_from_xpointer(&anchor).unwrap(), doc.current_page());

    // The chunks that have not been laid out still answer with a page count that
    // is close to the truth, not the old pagination.
    let reference = Document::with_options(&path, &options(24.0)).unwrap();
    let expected = reference.page_count();
    let provisional = doc.page_count();
    let error = (provisional as f32 - expected as f32).abs() / expected as f32;
    assert!(
        error < 0.15,
        "provisional page count {provisional} is too far from the real {expected}"
    );

    // Reading the book through lays every chunk out, and the page count settles
    // on exactly what a full layout gives.
    doc.go_to_first_page();
    let mut page = 1;
    while page <= doc.page_count() {
        let bitmap = doc.render_page(page).unwrap();
        assert!(!bitmap.data.is_empty(), "page {page} rendered empty");
        page += 1;
        assert!(page < 10_000, "page count never settled");
    }
    assert_eq!(doc.page_count(), expected);
    assert_eq!(reference.page_count(), expected);
}

#[test]
fn relayout_is_refused_when_the_chunk_html_would_change() {
    // FB2 and plain-text chunks embed the font size in their HTML, so they take
    // the reopen path; a different page size changes the chunk HTML of every
    // format.
    let doc = Document::with_options(fixture("sample.fb2"), &options(18.0)).unwrap();
    assert!(!doc.can_relayout_with(&options(24.0)));

    let epub = Document::with_options(fixture("juliet.epub"), &options(18.0)).unwrap();
    let wider = DocumentOptions {
        width: 700,
        ..options(18.0)
    };
    assert!(!epub.can_relayout_with(&wider));
    assert!(epub.can_relayout_with(&options(18.0)));
}

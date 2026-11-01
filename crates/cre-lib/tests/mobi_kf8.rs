//! MOBI 6 and KF8/AZW3 integration tests.
//!
//! These need a real book of each kind and no such fixture is committed (the
//! ones we test with are copyrighted), so each test skips when the file is not
//! there. Point `CRE_MOBI_TEST_FILE` / `CRE_KF8_TEST_FILE` at one, or rely on
//! `$HOME/can.mobi` and `$HOME/yuan.azw3`.

use std::path::{Path, PathBuf};

use cre_lib::{Document, DocumentFormat};

fn book(env: &str, fallback: &str) -> Option<PathBuf> {
    let path = std::env::var_os(env).map(PathBuf::from).or_else(|| {
        let home = std::env::var_os("HOME")?;
        Some(Path::new(&home).join(fallback))
    })?;
    path.exists().then_some(path)
}

fn check_book(path: &Path, expected: DocumentFormat) {
    let mut doc = Document::open(path).unwrap();
    assert_eq!(doc.metadata().format, expected);
    assert!(
        doc.page_count() > 10,
        "{} pages for {}",
        doc.page_count(),
        path.display()
    );
    assert!(doc.metadata().title.is_some());

    // The table of contents must point at real pages, and those pages must be
    // usable for navigation.
    let toc = doc.toc().to_vec();
    assert!(!toc.is_empty());
    let mapped = toc.iter().filter(|entry| entry.page.is_some()).count();
    assert_eq!(
        mapped,
        toc.len(),
        "{} of {} TOC entries have no page",
        toc.len() - mapped,
        toc.len()
    );
    for entry in &toc {
        let page = entry.page.unwrap();
        assert!(page >= 1 && page <= doc.page_count(), "{entry:?}");
    }

    // Reading through a couple of TOC targets works and produces ink.
    for entry in toc.iter().skip(1).take(3) {
        let page = doc.go_to_href(&entry.href).unwrap();
        assert_eq!(doc.current_page(), page);
        let bitmap = doc.render_page(page).unwrap();
        assert!(
            bitmap.data.iter().any(|value| *value < 128),
            "page {page} of {} rendered blank",
            path.display()
        );
        assert!(!doc.page_text(page).unwrap_or_default().trim().is_empty());
    }
}

#[test]
fn opens_a_mobi6_book_when_available() {
    let Some(path) = book("CRE_MOBI_TEST_FILE", "can.mobi") else {
        eprintln!("skipping: no MOBI test book (set CRE_MOBI_TEST_FILE)");
        return;
    };
    // An `.azw3`-extension book can be handed to either reader, so pick the
    // reader by what the file really is.
    let format = if cre_kf8::Kf8Book::open(&path).is_ok() {
        DocumentFormat::Azw3
    } else {
        DocumentFormat::Mobi
    };
    check_book(&path, format);
}

#[test]
fn opens_a_kf8_book_when_available() {
    let Some(path) = book("CRE_KF8_TEST_FILE", "yuan.azw3") else {
        eprintln!("skipping: no KF8 test book (set CRE_KF8_TEST_FILE)");
        return;
    };
    check_book(&path, DocumentFormat::Azw3);
}

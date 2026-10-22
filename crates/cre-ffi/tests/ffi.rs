use std::ffi::{CStr, CString, c_char};
use std::path::{Path, PathBuf};
use std::ptr;

use cre_ffi::{
    CreDocumentOptions, cre_close, cre_current_page, cre_find_text_count, cre_find_text_page,
    cre_first_page, cre_format, cre_goto_href, cre_goto_page, cre_goto_xpointer, cre_last_error,
    cre_metadata, cre_next_page, cre_open, cre_open_with_options, cre_page_count, cre_page_height,
    cre_page_text, cre_page_width, cre_previous_page, cre_render_page, cre_toc_count, cre_toc_href,
    cre_toc_page, cre_toc_title, cre_xpointer_from_page,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/koreader-test-data")
        .join(name)
}

fn c_string(value: &str) -> CString {
    CString::new(value).unwrap()
}

fn read_string(out: &[c_char], required: isize) -> String {
    assert!(required > 0, "string call failed");
    assert!(
        required as usize <= out.len(),
        "string output buffer was too small"
    );
    unsafe { CStr::from_ptr(out.as_ptr()) }
        .to_str()
        .unwrap()
        .to_string()
}

#[test]
fn ffi_exposes_document_pages_text_search_and_xpointers() {
    let path = c_string(fixture("sample.txt").to_str().unwrap());
    let doc = unsafe { cre_open(path.as_ptr()) };
    assert!(!doc.is_null(), "{}", last_error());

    assert_eq!(cre_format(doc), 3);
    assert!(cre_page_count(doc) > 1);
    assert_eq!(cre_current_page(doc), 1);
    assert_eq!(cre_page_width(doc), 600);
    assert_eq!(cre_page_height(doc), 800);

    let mut pixels = vec![0u8; 600 * 800];
    assert_eq!(
        cre_render_page(doc, 1, pixels.as_mut_ptr(), pixels.len()),
        (600 * 800) as isize
    );
    assert!(pixels.iter().any(|value| *value < 128));

    let mut text = vec![0 as c_char; 4096];
    let required = cre_page_text(doc, 1, text.as_mut_ptr(), text.len());
    let page_text = read_string(&text, required);
    assert!(page_text.contains("Lorem ipsum"));

    let mut metadata = vec![0 as c_char; 256];
    let required = cre_metadata(doc, 1, metadata.as_mut_ptr(), metadata.len());
    assert!(required >= 0);

    let mut xpointer = vec![0 as c_char; 4096];
    let required = cre_xpointer_from_page(doc, 1, xpointer.as_mut_ptr(), xpointer.len());
    let xpointer = read_string(&xpointer, required);
    assert!(!xpointer.is_empty());
    assert_eq!(
        unsafe { cre_goto_xpointer(doc, c_string(&xpointer).as_ptr()) },
        1
    );

    let query = c_string("Lorem ipsum");
    assert!(unsafe { cre_find_text_count(doc, query.as_ptr()) } > 1);
    assert_eq!(unsafe { cre_find_text_page(doc, query.as_ptr(), 0) }, 1);

    assert_eq!(cre_next_page(doc), 2);
    assert_eq!(cre_previous_page(doc), 1);
    assert_eq!(cre_goto_page(doc, 2), 2);
    assert_eq!(cre_first_page(doc), 1);

    unsafe { cre_close(doc) };
}

#[test]
fn ffi_exposes_toc_and_errors() {
    let path = c_string(fixture("links.epub").to_str().unwrap());
    let doc = unsafe { cre_open(path.as_ptr()) };
    assert!(!doc.is_null(), "{}", last_error());

    assert_eq!(cre_format(doc), 1);
    assert_eq!(cre_toc_count(doc), 2);
    let mut title = vec![0 as c_char; 256];
    let required = cre_toc_title(doc, 1, title.as_mut_ptr(), title.len());
    assert_eq!(read_string(&title, required), "Chapter Two");
    assert!(cre_toc_page(doc, 1) >= 1);
    unsafe { cre_close(doc) };

    let missing = c_string(fixture("does-not-exist.txt").to_str().unwrap());
    let doc = unsafe { cre_open(missing.as_ptr()) };
    assert!(doc.is_null());
    assert!(!last_error().is_empty());

    assert_eq!(cre_page_count(ptr::null()), -1);
    assert!(!last_error().is_empty());
}

#[test]
fn ffi_opens_document_with_options() {
    let path = c_string(fixture("sample.txt").to_str().unwrap());
    let options = CreDocumentOptions {
        width: 320,
        height: 480,
        layout_height: 100_000,
        page_margin: 8.0,
        font_path: ptr::null(),
    };

    let doc = unsafe { cre_open_with_options(path.as_ptr(), &options) };
    assert!(!doc.is_null(), "{}", last_error());
    assert_eq!(cre_page_width(doc), 320);
    assert_eq!(cre_page_height(doc), 480);
    assert!(cre_page_count(doc) >= 1);
    unsafe { cre_close(doc) };

    let invalid = CreDocumentOptions {
        width: 0,
        height: 480,
        layout_height: 100_000,
        page_margin: 0.0,
        font_path: ptr::null(),
    };
    let doc = unsafe { cre_open_with_options(path.as_ptr(), &invalid) };
    assert!(doc.is_null());
    assert!(!last_error().is_empty());
}

#[test]
fn ffi_exposes_fb2_toc_and_navigation() {
    let path = c_string(fixture("sample.fb2").to_str().unwrap());
    let doc = unsafe { cre_open(path.as_ptr()) };
    assert!(!doc.is_null(), "{}", last_error());

    assert_eq!(cre_format(doc), 2);
    assert_eq!(cre_toc_count(doc), 1);

    let mut title = vec![0 as c_char; 256];
    let required = cre_toc_title(doc, 0, title.as_mut_ptr(), title.len());
    assert_eq!(read_string(&title, required), "Chapter One");

    let mut href = vec![0 as c_char; 256];
    let required = cre_toc_href(doc, 0, href.as_mut_ptr(), href.len());
    assert_eq!(read_string(&href, required), "#fb2-body-0");

    let page = cre_toc_page(doc, 0);
    assert!(page >= 1);
    assert_eq!(
        unsafe { cre_goto_href(doc, c_string("#fb2-body-0").as_ptr()) },
        page
    );
    unsafe { cre_close(doc) };
}

fn last_error() -> String {
    let mut error = vec![0 as c_char; 2048];
    let required = cre_last_error(error.as_mut_ptr(), error.len());
    if required <= 0 || required as usize > error.len() {
        return String::new();
    }
    unsafe { CStr::from_ptr(error.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

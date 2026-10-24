use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::result::Result as StdResult;

use cre_lib::{Document, DocumentFormat, DocumentOptions};

pub struct CreDocument {
    inner: Document,
}

#[repr(C)]
pub struct CreDocumentOptions {
    pub width: u32,
    pub height: u32,
    pub layout_height: u32,
    pub page_margin: f32,
    pub font_path: *const c_char,
}

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

fn set_error(message: impl AsRef<str>) {
    let bytes = message
        .as_ref()
        .as_bytes()
        .iter()
        .map(|byte| if *byte == 0 { b'?' } else { *byte })
        .collect::<Vec<_>>();
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = CString::new(bytes).unwrap_or_default();
    });
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "panic in cre-ffi".to_string()
    }
}

fn run<T>(default: T, f: impl FnOnce() -> StdResult<T, String>) -> T {
    set_error("");
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            set_error(error);
            default
        }
        Err(payload) => {
            set_error(panic_message(payload));
            default
        }
    }
}

fn document<'a>(doc: *const CreDocument) -> StdResult<&'a Document, String> {
    if doc.is_null() {
        return Err("document is null".to_string());
    }
    // SAFETY: the caller owns a live CreDocument for the duration of the call.
    Ok(unsafe { &(*doc).inner })
}

fn document_mut<'a>(doc: *mut CreDocument) -> StdResult<&'a mut Document, String> {
    if doc.is_null() {
        return Err("document is null".to_string());
    }
    // SAFETY: the caller owns a live CreDocument for the duration of the call.
    Ok(unsafe { &mut (*doc).inner })
}

fn write_bytes(bytes: &[u8], out: *mut u8, out_len: usize) -> isize {
    if !out.is_null() && out_len >= bytes.len() {
        // SAFETY: the caller provided an output buffer of at least bytes.len() bytes.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), out, bytes.len()) };
    }
    bytes.len() as isize
}

fn write_string(value: &str, out: *mut c_char, out_len: usize) -> isize {
    let bytes = value.as_bytes();
    let required = bytes.len() + 1;
    if !out.is_null() && out_len >= required {
        // SAFETY: the caller provided an output buffer of at least required bytes.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), out.cast::<u8>(), bytes.len());
            *out.add(bytes.len()) = 0;
        }
    }
    required as isize
}

fn to_index(value: u32) -> StdResult<usize, String> {
    usize::try_from(value).map_err(|_| "index does not fit usize".to_string())
}

/// # Safety
///
/// `path` must be a valid NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cre_open(path: *const c_char) -> *mut CreDocument {
    run(ptr::null_mut(), || {
        if path.is_null() {
            return Err("path is null".to_string());
        }
        // SAFETY: the caller supplied a NUL-terminated path string.
        let path = unsafe { CStr::from_ptr(path) }
            .to_str()
            .map_err(|error| format!("path is not valid UTF-8: {error}"))?;
        let inner = Document::open(path).map_err(|error| format!("{error:#}"))?;
        Ok(Box::into_raw(Box::new(CreDocument { inner })))
    })
}

/// # Safety
///
/// `path` must be a valid NUL-terminated UTF-8 string. `options` must be null or point to a
/// valid [`CreDocumentOptions`] for the duration of the call; its `font_path` must be null or a
/// valid NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cre_open_with_options(
    path: *const c_char,
    options: *const CreDocumentOptions,
) -> *mut CreDocument {
    run(ptr::null_mut(), || {
        if path.is_null() {
            return Err("path is null".to_string());
        }
        // SAFETY: the caller supplied a NUL-terminated path string.
        let path = unsafe { CStr::from_ptr(path) }
            .to_str()
            .map_err(|error| format!("path is not valid UTF-8: {error}"))?;
        let inner = if options.is_null() {
            Document::open(path).map_err(|error| format!("{error:#}"))?
        } else {
            // SAFETY: the caller supplied a valid options object.
            let options = unsafe { &*options };
            let font_path = if options.font_path.is_null() {
                None
            } else {
                // SAFETY: the caller supplied a NUL-terminated font path string.
                let font_path = unsafe { CStr::from_ptr(options.font_path) }
                    .to_str()
                    .map_err(|error| format!("font path is not valid UTF-8: {error}"))?;
                Some(std::path::PathBuf::from(font_path))
            };
            Document::with_options(
                path,
                &DocumentOptions {
                    width: options.width,
                    height: options.height,
                    layout_height: options.layout_height,
                    page_margin: options.page_margin,
                    font_size: 18.0,
                    font_path,
                    stylesheet: None,
                },
            )
            .map_err(|error| format!("{error:#}"))?
        };
        Ok(Box::into_raw(Box::new(CreDocument { inner })))
    })
}

/// # Safety
///
/// `doc` must be null or a pointer returned by [`cre_open`] or [`cre_open_with_options`] that
/// has not been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cre_close(doc: *mut CreDocument) {
    if doc.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: ownership of a pointer returned by cre_open is transferred back here once.
        drop(unsafe { Box::from_raw(doc) });
    }));
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_last_error(out: *mut c_char, out_len: usize) -> isize {
    LAST_ERROR.with(|slot| {
        let error = slot.borrow();
        write_string(
            error.to_str().unwrap_or("error is not valid UTF-8"),
            out,
            out_len,
        )
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_page_count(doc: *const CreDocument) -> i32 {
    run(-1, || {
        let count = document(doc)?.page_count();
        i32::try_from(count).map_err(|_| "page count does not fit i32".to_string())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_current_page(doc: *const CreDocument) -> i32 {
    run(-1, || {
        let page = document(doc)?.current_page();
        i32::try_from(page).map_err(|_| "page number does not fit i32".to_string())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_goto_page(doc: *mut CreDocument, page: u32) -> i32 {
    run(-1, || {
        let page = document_mut(doc)?
            .go_to_page(to_index(page)?)
            .map_err(|error| format!("{error:#}"))?;
        i32::try_from(page).map_err(|_| "page number does not fit i32".to_string())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_next_page(doc: *mut CreDocument) -> i32 {
    run(-1, || {
        let page = document_mut(doc)?.go_to_next_page();
        i32::try_from(page).map_err(|_| "page number does not fit i32".to_string())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_previous_page(doc: *mut CreDocument) -> i32 {
    run(-1, || {
        let page = document_mut(doc)?.go_to_previous_page();
        i32::try_from(page).map_err(|_| "page number does not fit i32".to_string())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_first_page(doc: *mut CreDocument) -> i32 {
    run(-1, || {
        let page = document_mut(doc)?.go_to_first_page();
        i32::try_from(page).map_err(|_| "page number does not fit i32".to_string())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_last_page(doc: *mut CreDocument) -> i32 {
    run(-1, || {
        let page = document_mut(doc)?.go_to_last_page();
        i32::try_from(page).map_err(|_| "page number does not fit i32".to_string())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_page_width(doc: *const CreDocument) -> u32 {
    run(0, || Ok(document(doc)?.page_width()))
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_page_height(doc: *const CreDocument) -> u32 {
    run(0, || Ok(document(doc)?.page_height()))
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_render_page(
    doc: *mut CreDocument,
    page: u32,
    out: *mut u8,
    out_len: usize,
) -> isize {
    run(-1, || {
        let bitmap = document_mut(doc)?
            .render_page(to_index(page)?)
            .map_err(|error| format!("{error:#}"))?;
        Ok(write_bytes(&bitmap.data, out, out_len))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_page_text(
    doc: *const CreDocument,
    page: u32,
    out: *mut c_char,
    out_len: usize,
) -> isize {
    run(-1, || {
        let text = document(doc)?
            .page_text(to_index(page)?)
            .map_err(|error| format!("{error:#}"))?;
        Ok(write_string(&text, out, out_len))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_format(doc: *const CreDocument) -> i32 {
    run(0, || {
        let format = document(doc)?.metadata().format;
        Ok(match format {
            DocumentFormat::Epub => 1,
            DocumentFormat::Fb2 => 2,
            DocumentFormat::Txt => 3,
        })
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_metadata(
    doc: *const CreDocument,
    field: u32,
    out: *mut c_char,
    out_len: usize,
) -> isize {
    run(-1, || {
        let metadata = &document(doc)?.metadata();
        let value = match field {
            1 => metadata.title.as_deref(),
            2 => metadata.creator.as_deref(),
            3 => metadata.language.as_deref(),
            4 => metadata.series.as_deref(),
            5 => metadata.description.as_deref(),
            6 => metadata.keywords.as_deref(),
            7 => metadata.identifiers.as_deref(),
            _ => return Err("unknown metadata field".to_string()),
        };
        Ok(value.map_or(0, |value| write_string(value, out, out_len)))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_toc_count(doc: *const CreDocument) -> i32 {
    run(-1, || {
        let count = document(doc)?.toc().len();
        i32::try_from(count).map_err(|_| "toc count does not fit i32".to_string())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_toc_title(
    doc: *const CreDocument,
    index: u32,
    out: *mut c_char,
    out_len: usize,
) -> isize {
    run(-1, || {
        let entry = toc_entry(document(doc)?, index)?;
        Ok(write_string(&entry.title, out, out_len))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_toc_href(
    doc: *const CreDocument,
    index: u32,
    out: *mut c_char,
    out_len: usize,
) -> isize {
    run(-1, || {
        let entry = toc_entry(document(doc)?, index)?;
        Ok(write_string(&entry.href, out, out_len))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_toc_page(doc: *const CreDocument, index: u32) -> i32 {
    run(-1, || {
        let entry = toc_entry(document(doc)?, index)?;
        entry
            .page
            .and_then(|page| i32::try_from(page).ok())
            .ok_or_else(|| "toc entry has no page".to_string())
    })
}

/// # Safety
///
/// `doc` must be a live document and `href` must be a valid NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cre_goto_href(doc: *mut CreDocument, href: *const c_char) -> i32 {
    run(-1, || {
        if href.is_null() {
            return Err("href is null".to_string());
        }
        // SAFETY: the caller supplied a NUL-terminated href string.
        let href = unsafe { CStr::from_ptr(href) }
            .to_str()
            .map_err(|error| format!("href is not valid UTF-8: {error}"))?;
        let page = document_mut(doc)?
            .go_to_href(href)
            .map_err(|error| format!("{error:#}"))?;
        i32::try_from(page).map_err(|_| "page number does not fit i32".to_string())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn cre_xpointer_from_page(
    doc: *const CreDocument,
    page: u32,
    out: *mut c_char,
    out_len: usize,
) -> isize {
    run(-1, || {
        let xpointer = document(doc)?
            .xpointer_from_page(to_index(page)?)
            .map_err(|error| format!("{error:#}"))?;
        Ok(write_string(&xpointer, out, out_len))
    })
}

/// # Safety
///
/// `doc` must be a live document and `xpointer` must be a valid NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cre_goto_xpointer(doc: *mut CreDocument, xpointer: *const c_char) -> i32 {
    run(-1, || {
        if xpointer.is_null() {
            return Err("xpointer is null".to_string());
        }
        // SAFETY: the caller supplied a NUL-terminated xpointer string.
        let xpointer = unsafe { CStr::from_ptr(xpointer) }
            .to_str()
            .map_err(|error| format!("xpointer is not valid UTF-8: {error}"))?;
        let page = document_mut(doc)?
            .go_to_xpointer(xpointer)
            .map_err(|error| format!("{error:#}"))?;
        i32::try_from(page).map_err(|_| "page number does not fit i32".to_string())
    })
}

/// # Safety
///
/// `doc` must be a live document and `query` must be a valid NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cre_find_text_count(doc: *const CreDocument, query: *const c_char) -> i32 {
    run(-1, || {
        if query.is_null() {
            return Err("query is null".to_string());
        }
        // SAFETY: the caller supplied a NUL-terminated query string.
        let query = unsafe { CStr::from_ptr(query) }
            .to_str()
            .map_err(|error| format!("query is not valid UTF-8: {error}"))?;
        let hits = document(doc)?
            .find_text(query)
            .map_err(|error| format!("{error:#}"))?;
        i32::try_from(hits.len()).map_err(|_| "hit count does not fit i32".to_string())
    })
}

/// # Safety
///
/// `doc` must be a live document and `query` must be a valid NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cre_find_text_page(
    doc: *const CreDocument,
    query: *const c_char,
    index: u32,
) -> i32 {
    run(-1, || {
        if query.is_null() {
            return Err("query is null".to_string());
        }
        // SAFETY: the caller supplied a NUL-terminated query string.
        let query = unsafe { CStr::from_ptr(query) }
            .to_str()
            .map_err(|error| format!("query is not valid UTF-8: {error}"))?;
        let hits = document(doc)?
            .find_text(query)
            .map_err(|error| format!("{error:#}"))?;
        let hit = hits
            .get(to_index(index)?)
            .ok_or_else(|| "search hit index is out of range".to_string())?;
        i32::try_from(hit.page).map_err(|_| "page number does not fit i32".to_string())
    })
}

fn toc_entry(doc: &Document, index: u32) -> StdResult<&cre_lib::TocEntry, String> {
    doc.toc()
        .get(to_index(index)?)
        .ok_or_else(|| "toc index is out of range".to_string())
}

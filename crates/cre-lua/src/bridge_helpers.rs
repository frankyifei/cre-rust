use cre_lib::{ColorPageBitmap, Document, PageBitmap, PagePosition, TextBox, TextPosition};

pub(crate) fn apply_color_text_highlight(
    bitmap: &mut ColorPageBitmap,
    boxes: &[TextBox],
    current_page: usize,
) {
    for text_box in boxes {
        if text_box.page != current_page {
            continue;
        }
        let left = text_box.x.floor().max(0.0) as u32;
        let top = text_box.y.floor().max(0.0) as u32;
        let right = (text_box.x + text_box.width).ceil().max(0.0) as u32;
        let bottom = (text_box.y + text_box.height).ceil().max(0.0) as u32;
        let right = right.min(bitmap.width);
        let bottom = bottom.min(bitmap.height);
        for y in top..bottom {
            for x in left..right {
                let index = ((y * bitmap.width + x) as usize) * 4;
                for channel in 0..3 {
                    if let Some(value) = bitmap.data.get_mut(index + channel) {
                        *value = 255 - *value;
                    }
                }
            }
        }
    }
}

pub(crate) fn apply_text_highlight(
    bitmap: &mut PageBitmap,
    boxes: &[TextBox],
    current_page: usize,
) {
    for text_box in boxes {
        if text_box.page != current_page {
            continue;
        }
        let left = text_box.x.floor().max(0.0) as u32;
        let top = text_box.y.floor().max(0.0) as u32;
        let right = (text_box.x + text_box.width).ceil().max(0.0) as u32;
        let bottom = (text_box.y + text_box.height).ceil().max(0.0) as u32;
        let right = right.min(bitmap.width);
        let bottom = bottom.min(bitmap.height);
        for y in top..bottom {
            for x in left..right {
                let index = (y * bitmap.width + x) as usize;
                if let Some(pixel) = bitmap.data.get_mut(index) {
                    *pixel = 255 - *pixel;
                }
            }
        }
    }
}

/// `spine_index` is the EPUB spine index, which differs from the XPointer's
/// DocFragment slot when a spine item was split into several parts.
pub(crate) fn cfi_for_position(position: &TextPosition, spine_index: u32) -> String {
    let mut path = String::new();
    for segment in &position.path {
        path.push('/');
        path.push_str(&(segment.index.saturating_mul(2)).to_string());
    }
    format!(
        "epubcfi(/6/{}!/4{}:{})",
        spine_index.saturating_mul(2),
        path,
        position.codepoint_offset
    )
}

pub(crate) fn cfi_range_for_positions(
    start: &TextPosition,
    end: &TextPosition,
    start_spine_index: u32,
) -> String {
    let base = cfi_for_position(start, start_spine_index);
    let base = base
        .strip_prefix("epubcfi(")
        .and_then(|value| value.strip_suffix(')'))
        .unwrap_or(base.as_str());
    format!(
        "epubcfi({},/1:{},/1:{})",
        base, start.codepoint_offset, end.codepoint_offset
    )
}

/// Where the reader is in the document: the top of the page being shown.
///
/// This used to add the page's first line offset, which made the text-box
/// mapping subtract it as well: a word's box came out one line offset too high,
/// and a box belonging to another page was shifted by whole pages. The renderer
/// draws a page from its start, so the page start is the position KOReader's
/// `getCurrentPos`/`gotoPos` round trip must use.
pub(crate) fn current_document_pos(inner: &Document) -> i64 {
    inner.page_start_y(inner.current_page())
}

pub(crate) fn expand_to_visible_word_start(inner: &Document, xpointer: &str) -> String {
    let page = match inner.page_from_xpointer(xpointer).ok() {
        None => return xpointer.to_string(),
        Some(page) => page,
    };
    let (x, y) = match inner.point_from_xpointer(xpointer).ok().unwrap_or(None) {
        None => return xpointer.to_string(),
        Some((x, y)) => (x + 1.0, y),
    };
    let word = match inner
        .word_at(PagePosition { page, x, y })
        .ok()
        .unwrap_or(None)
    {
        None => return xpointer.to_string(),
        Some(word) => word,
    };
    word.xpointer_range.start.to_xpointer()
}

pub(crate) fn expand_to_visible_word_end(inner: &Document, xpointer: &str) -> String {
    let page = match inner.page_from_xpointer(xpointer).ok() {
        None => return xpointer.to_string(),
        Some(page) => page,
    };
    let (x, y) = match inner.point_from_xpointer(xpointer).ok().unwrap_or(None) {
        None => return xpointer.to_string(),
        Some((x, y)) => (x - 1.0, y),
    };
    let word = match inner
        .word_at(PagePosition { page, x, y })
        .ok()
        .unwrap_or(None)
    {
        None => return xpointer.to_string(),
        Some(word) => word,
    };
    word.xpointer_range
        .end
        .as_ref()
        .map(|position| position.to_xpointer())
        .unwrap_or(xpointer.to_string())
}

/// Maps a text box from its page's coordinates onto the window being shown.
///
/// The engine only renders page view (see `doc_set_view_mode`), so the window
/// is always the current page — plus the next one when the reader shows two
/// columns.
pub(crate) fn window_rect_for_page_box(
    visible_page_count: i32,
    inner: &Document,
    text_box: &TextBox,
) -> Option<(f32, f32, f32, f32)> {
    if !text_box.x.is_finite()
        || !text_box.y.is_finite()
        || !text_box.width.is_finite()
        || !text_box.height.is_finite()
    {
        return None;
    }
    if text_box.width <= 0.0 || text_box.height <= 0.0 {
        return None;
    }

    let current = inner.current_page();
    let visible_count = visible_page_count.max(1) as usize;
    if text_box.page < current || text_box.page >= current + visible_count {
        return None;
    }
    let x_offset = if text_box.page == current {
        0.0
    } else {
        inner.page_width() as f32
    };
    Some((
        text_box.x + x_offset,
        text_box.y,
        text_box.x + text_box.width + x_offset,
        text_box.y + text_box.height,
    ))
}

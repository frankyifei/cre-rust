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

pub(crate) fn current_document_pos(inner: &Document) -> i64 {
    let page = inner.current_page();
    let page_y = inner.page_start_y(page);
    if let Some(xpointer) = inner.xpointer_from_page(page).ok()
        && let Some((_, y)) = inner.point_from_xpointer(&xpointer).ok().flatten()
    {
        return page_y + y.round() as i64;
    }
    page_y
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

pub(crate) fn window_rect_for_page_box(
    view_mode: i32,
    visible_page_count: i32,
    view_height: u32,
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

    if view_mode == 1 {
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
    } else {
        let page_top = inner.page_start_y_or_invalid(text_box.page);
        if page_top < 0 {
            return None;
        }
        let current_pos = current_document_pos(inner);
        let y0 = page_top as f32 + text_box.y - current_pos as f32;
        let y1 = y0 + text_box.height;
        let view_height = view_height.max(1) as f32;
        if y1 <= 0.0 || y0 >= view_height {
            return None;
        }
        Some((text_box.x, y0, text_box.x + text_box.width, y1))
    }
}

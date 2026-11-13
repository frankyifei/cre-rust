//! Splitting a generated document into reader chunks.

use crate::html::{find_tag_end, tag_name};

/// Splits a complete HTML document into parts of at most `limit` bytes,
/// repeating the document's head and closing tags in every part. Used for
/// formats that are produced as one document (FB2, TXT, standalone HTML), so
/// they do not have to be laid out in a single giant chunk either.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

struct OpenTag {
    name: String,
    start: String,
}

pub fn split_html_document(html: &str, limit: usize) -> Vec<String> {
    if limit == 0 || html.len() <= limit {
        return vec![html.to_string()];
    }
    let Some((head, body, tail)) = html_body_parts(html) else {
        return vec![html.to_string()];
    };
    let parts = split_html_body_parts(body, limit);
    if parts.len() <= 1 {
        return vec![html.to_string()];
    }
    parts
        .into_iter()
        .map(|part| format!("{head}{part}{tail}"))
        .collect()
}

/// Splits the `<body>` content of a document into parts of at most `limit`
/// bytes, giving every part but the first its own `data-spine-index` range.
///
/// This is [`split_html_document`] without the head and tail, for a caller
/// that already holds the generated body and wants to build one chunk at a
/// time instead of materializing the whole document first.
pub fn split_html_body_parts(body: &str, limit: usize) -> Vec<String> {
    if limit == 0 || body.len() <= limit {
        return vec![body.to_string()];
    }
    let parts = split_body_html(body, limit);
    if parts.len() <= 1 {
        return vec![body.to_string()];
    }
    parts
        .into_iter()
        .enumerate()
        .map(|(index, part)| {
            // A `data-spine-index` names the document fragment an XPointer
            // points into, and the layout maps it to one chunk. A section that
            // straddles a chunk cut would otherwise be named by the same slot
            // in two chunks, so the second one could never resolve; give every
            // part its own slot range. (Only EPUB uses these attributes today,
            // but single-document formats use this splitter.)
            if index == 0 {
                part
            } else {
                renumber_spine_indices(&part, index)
            }
        })
        .collect()
}

/// The `<head>…<body>` prefix, the body content and the `</body>…` tail of a
/// generated document.
///
/// A generator that keeps its body separately can hand its parts here instead
/// of building the whole document, so the complete HTML never exists.
pub fn html_body_parts(html: &str) -> Option<(&str, &str, &str)> {
    let lower = html.to_ascii_lowercase();
    let body_start = lower.find("<body")?;
    let body_open_end = body_start + lower[body_start..].find('>')? + 1;
    let body_close = lower.rfind("</body>")?;
    if body_close < body_open_end {
        return None;
    }
    Some((
        &html[..body_open_end],
        &html[body_open_end..body_close],
        &html[body_close..],
    ))
}

/// Splits a single-document book at its `<section class="cre-spine">` parts.
///
/// MOBI and KF8 keep the whole book in one flow; `cre-mobi`/`cre-kf8` rebuild
/// the original parts (the "HTML files" a converter shows) as sections. When
/// memory forces a book into several layout chunks, cutting at those boundaries
/// keeps every chunk a whole number of parts, so page numbers do not depend on
/// where a byte budget happened to fall and no section is split in two.
///
/// Returns `None` when the body is not a run of sections, or when one section
/// alone is larger than `limit` (the caller then falls back to the generic
/// splitter, which can cut inside a part).
pub fn split_html_document_at_sections(html: &str, limit: usize) -> Option<Vec<String>> {
    split_html_document_at_sections_with(html, limit, usize::MAX, usize::MAX, 128, |section| {
        (section.len(), 0)
    })
}

/// Like [`split_html_document_at_sections`], but a chunk also has to stay
/// within an estimated decoded-image budget and a layout budget.
///
/// `cost` returns the HTML bytes and the estimated decoded bytes of the images
/// of one section; `dom_factor` is how many bytes of DOM one HTML byte is
/// assumed to take, so `layout_limit` can be checked the way `chunk_budgets`
/// computes it for EPUB.
#[allow(clippy::too_many_arguments)]
pub fn split_html_document_at_sections_with<F>(
    html: &str,
    limit: usize,
    image_limit: usize,
    layout_limit: usize,
    dom_factor: usize,
    cost: F,
) -> Option<Vec<String>>
where
    F: Fn(&str) -> (usize, usize),
{
    if limit == 0 {
        return None;
    }
    let lower = html.to_ascii_lowercase();
    let body_start = lower.find("<body")?;
    let body_open_end = lower[body_start..].find('>')? + body_start + 1;
    let body_close = lower.rfind("</body>")?;
    if body_close < body_open_end {
        return None;
    }
    let body = &html[body_open_end..body_close];
    let (prefix_end, sections) = section_ranges_in(body, &lower[body_open_end..body_close])?;
    let head = &html[..body_open_end];
    let tail = &html[body_close..];

    let mut parts: Vec<String> = Vec::new();
    pack_sections(
        body,
        prefix_end,
        &sections,
        head,
        tail,
        limit,
        image_limit,
        layout_limit,
        dom_factor,
        cost,
        |part| parts.push(part),
    )?;
    Some(parts)
}

/// Splits a generated body at its `<section>` parts, handing one finished
/// chunk document to `emit` at a time.
///
/// `head` and `tail` are the parts of the document around the body
/// ([`html_body_parts`]); every chunk is `head + prefix + sections + tail`, so
/// the caller never holds the whole document. Returns `false` when the body is
/// not a run of sections, or when one section alone is larger than `limit`; the
/// caller then falls back to [`split_html_body_parts`].
#[allow(clippy::too_many_arguments)]
pub fn stream_html_document_at_sections_with<F, G>(
    head: &str,
    body: &str,
    tail: &str,
    limit: usize,
    image_limit: usize,
    layout_limit: usize,
    dom_factor: usize,
    cost: F,
    mut emit: G,
) -> bool
where
    F: Fn(&str) -> (usize, usize),
    G: FnMut(String),
{
    if limit == 0 {
        return false;
    }
    let Some((prefix_end, sections)) = section_ranges(body) else {
        return false;
    };
    pack_sections(
        body,
        prefix_end,
        &sections,
        head,
        tail,
        limit,
        image_limit,
        layout_limit,
        dom_factor,
        cost,
        &mut emit,
    )
    .is_some()
}

/// Byte ranges of a body that is a run of `<section>` parts, plus the end of
/// the repeated prefix before the first one.
pub fn section_ranges(body: &str) -> Option<(usize, Vec<std::ops::Range<usize>>)> {
    let lower = body.to_ascii_lowercase();
    section_ranges_in(body, &lower)
}

pub fn section_ranges_in(
    body: &str,
    lower_body: &str,
) -> Option<(usize, Vec<std::ops::Range<usize>>)> {
    let mut sections = Vec::new();
    let mut cursor = 0usize;
    let mut prefix_end = 0usize;
    let mut first = true;
    while let Some(pos) = lower_body[cursor..].find("<section") {
        let start = cursor + pos;
        let gap = &body[cursor..start];
        if first {
            // A KF8 body starts with the book's stylesheet before the first
            // part; every chunk needs it, so it becomes a repeated prefix.
            if !only_style_blocks(gap) {
                return None;
            }
            prefix_end = start;
            first = false;
        } else if !gap.trim().is_empty() {
            return None;
        }
        let close = lower_body[start..].find("</section>")? + start;
        let end = close + "</section>".len();
        sections.push(start..end);
        cursor = end;
    }
    if sections.is_empty() {
        return None;
    }
    if !body[cursor..].trim().is_empty() {
        return None;
    }
    Some((prefix_end, sections))
}

/// Packs sections into chunks and emits one finished document per chunk.
#[allow(clippy::too_many_arguments)]
pub fn pack_sections<F, G>(
    body: &str,
    prefix_end: usize,
    sections: &[std::ops::Range<usize>],
    head: &str,
    tail: &str,
    limit: usize,
    image_limit: usize,
    layout_limit: usize,
    dom_factor: usize,
    cost: F,
    mut emit: G,
) -> Option<()>
where
    F: Fn(&str) -> (usize, usize),
    G: FnMut(String),
{
    // A section too big for the *text* budget has to be cut by the generic
    // splitter, which can cut inside it; the caller falls back then. An image
    // overrun is not a reason to fall back: a section is indivisible here (it
    // carries a spine index), so it simply gets a chunk of its own.
    if sections
        .iter()
        .any(|range| cost(&body[range.clone()]).0 > limit)
    {
        return None;
    }

    let prefix = &body[..prefix_end];
    let mut current = String::new();
    let mut current_images = 0usize;
    for range in sections {
        let section = &body[range.clone()];
        let (text, images) = cost(section);
        let over_text = prefix.len() + current.len() + text > limit;
        let over_images = current_images + images > image_limit;
        let over_layout =
            (prefix.len() + current.len() + text) * dom_factor + current_images + images
                > layout_limit;
        if !current.is_empty() && (over_text || over_images || over_layout) {
            emit(format!("{head}{prefix}{current}{tail}"));
            current.clear();
            current_images = 0;
        }
        current.push_str(section);
        current_images += images;
    }
    if !current.is_empty() {
        emit(format!("{head}{prefix}{current}{tail}"));
    }
    Some(())
}

/// True when a fragment holds only `<style>`/`<script>` blocks and whitespace.
pub fn only_style_blocks(fragment: &str) -> bool {
    let mut rest = fragment;
    loop {
        let trimmed = rest.trim_start();
        let lower = trimmed.to_ascii_lowercase();
        let (open, close) = if lower.starts_with("<style") {
            ("<style", "</style")
        } else if lower.starts_with("<script") {
            ("<script", "</script")
        } else {
            return trimmed.trim().is_empty();
        };
        let _ = open;
        let Some(end) = lower.find(close) else {
            return false;
        };
        let after = lower[end..]
            .find('>')
            .map(|offset| end + offset + 1)
            .unwrap_or(lower.len());
        rest = &trimmed[after..];
    }
}

/// Shifts every `data-spine-index="N"` in a split part by `part * 100_000`
/// so slots stay unique across parts.
pub fn renumber_spine_indices(html: &str, part_index: usize) -> String {
    const NEEDLE: &str = "data-spine-index=\"";
    let base = part_index * 100_000;
    let mut out = String::with_capacity(html.len() + 16);
    let mut at = 0usize;
    while let Some(relative) = html[at..].find(NEEDLE) {
        let value_start = at + relative + NEEDLE.len();
        let Some(length) = html[value_start..].find('"').filter(|length| *length > 0) else {
            break;
        };
        let Ok(value) = html[value_start..value_start + length].parse::<u32>() else {
            break;
        };
        out.push_str(&html[at..value_start]);
        out.push_str(&(value + base as u32).to_string());
        at = value_start + length;
    }
    out.push_str(&html[at..]);
    out
}

/// Splits a spine item's body HTML into parts of at most `limit` bytes,
/// cutting only between elements and re-opening whatever is still open, so
/// every part is well-formed on its own.
pub fn split_body_html(body: &str, limit: usize) -> Vec<String> {
    if limit == 0 || body.len() <= limit {
        return vec![body.to_string()];
    }
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut open: Vec<OpenTag> = Vec::new();
    let mut floor = 0usize; // bytes in `current` that cannot be flushed
    // Whether anything besides re-opened start tags went into `current`.
    let mut dirty = false;

    let flush =
        |parts: &mut Vec<String>, current: &mut String, open: &[OpenTag], dirty: &mut bool| {
            let mut part = String::new();
            part.push_str(current);
            for tag in open.iter().rev() {
                part.push_str("</");
                part.push_str(&tag.name);
                part.push('>');
            }
            parts.push(part);
            current.clear();
            for tag in open {
                current.push_str(&tag.start);
            }
            *dirty = false;
            current.len()
        };

    let mut index = 0usize;
    while index < body.len() {
        let next_tag = body[index..].find('<').map(|offset| index + offset);
        let text_end = next_tag.unwrap_or(body.len());
        if text_end > index {
            let mut text = &body[index..text_end];
            while !text.is_empty() {
                // Byte budget, floored to a character boundary: CJK text has
                // 3-byte characters, so a raw byte offset can land inside one.
                let room = floor_char_boundary(text, limit.saturating_sub(current.len()));
                if text.len() <= room {
                    if !text.trim().is_empty() {
                        dirty = true;
                    }
                    current.push_str(text);
                    break;
                }
                // Not enough room: cut at the last line break (or space) that
                // fits, so a body made of one long text run can still be split
                // into chunks.
                let cut = text[..room]
                    .rfind('\n')
                    .map(|position| position + 1)
                    .or_else(|| text[..room].rfind(' ').map(|position| position + 1))
                    .unwrap_or(room);
                if cut == 0 {
                    current.push_str(text);
                    break;
                }
                let (head, rest) = text.split_at(cut);
                if !head.trim().is_empty() {
                    dirty = true;
                }
                current.push_str(head);
                if current.len() > floor {
                    floor = flush(&mut parts, &mut current, &open, &mut dirty);
                }
                text = rest;
            }
        }
        let Some(tag_start) = next_tag else {
            break;
        };

        // Comments may contain '<'.
        if body[tag_start..].starts_with("<!--") {
            let end = body[tag_start..]
                .find("-->")
                .map(|offset| tag_start + offset + 3)
                .unwrap_or(body.len());
            let raw = &body[tag_start..end];
            if current.len() + raw.len() > limit && current.len() > floor {
                floor = flush(&mut parts, &mut current, &open, &mut dirty);
            }
            dirty = true;
            current.push_str(raw);
            index = end;
            continue;
        }

        let Some(tag_end) = find_tag_end(body, tag_start) else {
            current.push_str(&body[tag_start..]);
            break;
        };
        let raw = &body[tag_start..tag_end];
        let name = tag_name(raw);
        let closing = raw.starts_with("</");
        let self_closing = raw.trim_end().ends_with("/>");

        if closing {
            if let Some(position) = open.iter().rposition(|tag| tag.name == name) {
                open.truncate(position);
            }
            current.push_str(raw);
        } else if self_closing || VOID_ELEMENTS.contains(&name.as_str()) {
            current.push_str(raw);
        } else {
            if current.len() + raw.len() > limit && current.len() > floor {
                floor = flush(&mut parts, &mut current, &open, &mut dirty);
            }
            dirty = true;
            current.push_str(raw);
            if matches!(name.as_str(), "style" | "script" | "textarea" | "title") {
                // Raw-text elements: copy their content verbatim so '<' inside
                // them is not parsed as markup.
                let close = find_closing_tag(body, tag_end, &name).unwrap_or(body.len());
                current.push_str(&body[tag_end..close]);
                let close_end = find_tag_end(body, close).unwrap_or(body.len());
                current.push_str(&body[close..close_end]);
                index = close_end;
                continue;
            }
            open.push(OpenTag {
                name,
                start: raw.to_string(),
            });
        }
        index = tag_end;
    }
    // A trailing part that only re-opened tags has no content of its own; it
    // would render as a blank page between two chapters.
    if !current.is_empty() && (dirty || parts.is_empty()) {
        parts.push(current);
    }
    if parts.is_empty() {
        parts.push(String::new());
    }
    parts
}

pub fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub fn find_closing_tag(body: &str, from: usize, name: &str) -> Option<usize> {
    let needle = format!("</{name}");
    let haystack = body.get(from..)?;
    haystack.find(&needle).map(|offset| from + offset)
}

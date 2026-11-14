//! Splitting a generated document into reader chunks.

use anyhow::Result;

use crate::html::{find_tag_end, tag_name};
use crate::resources::ResourceSource;

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

/// Memory left untouched for KOReader's own post-open work (widgets, caches,
/// thumbnails) and for the page bitmap handed back to it.
///
/// This is the ceiling for [`layout_reserve`]; the chunk planner uses it as
/// is, because a smaller chunk only ever costs a relayout.
pub const LAYOUT_MEMORY_RESERVE: usize = 24 * 1024 * 1024;

pub const MAX_CHUNK_BYTES: usize = 128 * 1024;

/// Free memory and where the figure came from, for the diagnostics page.
pub fn memory_available_detail() -> Option<(u64, &'static str)> {
    // Testing knob: pretend the device only has this much memory, so the
    // chunk planner can be exercised on a desktop.
    if let Ok(value) = std::env::var("CRE_AVAILABLE_MB")
        && let Ok(mb) = value.trim().parse::<u64>()
        && mb > 0
    {
        return Some((mb.saturating_mul(1024 * 1024), "CRE_AVAILABLE_MB"));
    }
    #[cfg(target_os = "linux")]
    {
        let info = std::fs::read_to_string("/proc/meminfo").ok()?;
        let field = |key: &str| -> Option<u64> {
            info.lines()
                .find(|line| line.starts_with(key))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse::<u64>().ok())
        };
        // MemAvailable exists since Linux 3.14; the Kindle PW2 runs 3.0, so
        // fall back to the classic free+buffers+cached estimate there.
        if let Some(available) = field("MemAvailable:") {
            return Some((available.saturating_mul(1024), "MemAvailable"));
        }
        let free = field("MemFree:")?;
        let buffers = field("Buffers:").unwrap_or(0);
        let cached = field("Cached:").unwrap_or(0);
        return Some((
            (free + buffers + cached).saturating_mul(1024),
            "MemFree+Buffers+Cached",
        ));
    }
    #[allow(unreachable_code)]
    None
}

// ---- chunk budgets -------------------------------------------------------
// How large a chunk may be, from the memory the kernel can still hand out.
// Every format uses the same numbers: this estimate is what keeps a large book
// from being killed on a Kindle.

/// Bounds for a layout chunk: below a page's worth of text chunking only adds
/// relayouts; above this a single layout is both slow (blitz's layout cost
/// grows faster than the text: 9.7 s for a 251 kB book as one document, versus
/// ~4.4 s in four chunks) and expensive in memory. Pagination is unaffected as
/// long as chunks end at spine-item boundaries, because every spine item
/// starts a page anyway.
pub const MIN_CHUNK_BYTES: usize = 64 * 1024;

/// Resident cost of keeping a book laid out, per byte of generated HTML
/// (DOM nodes, computed styles and parley line boxes). Measured whole-book
/// layouts: 251 kB of XHTML cost 28 MB, 579 kB cost 44 MB. 128 B/B is the
/// safe upper bound, and chunk *sizing* keeps using it so it errs towards
/// smaller chunks.
pub const LAYOUT_BYTES_PER_HTML_BYTE: usize = 128;

/// Bytes the kernel says it can still hand out, if the platform reports it.
pub fn available_memory_bytes() -> Option<usize> {
    memory_available_detail().map(|(bytes, _)| bytes as usize)
}

/// Text and image budget for one layout chunk, from the memory the kernel can
/// still hand out.
///
/// The text budget uses [`LAYOUT_BYTES_PER_HTML_BYTE`]; the image budget is in
/// estimated decoded bytes, because blitz decodes a chunk's images while
/// parsing it (a dense illustrated book costs far more than its text).
pub fn chunk_budgets(available: Option<usize>) -> (usize, Option<usize>, usize) {
    let Some(available) = available else {
        return (usize::MAX, None, usize::MAX);
    };
    let budget = available
        .saturating_sub(LAYOUT_MEMORY_RESERVE)
        .min(available / 2)
        .max(MIN_CHUNK_BYTES * LAYOUT_BYTES_PER_HTML_BYTE);
    let text = (budget / LAYOUT_BYTES_PER_HTML_BYTE).clamp(MIN_CHUNK_BYTES, MAX_CHUNK_BYTES);
    // Images get 60% of the layout budget when there is plenty of memory, but
    // only a third on a small device: a chunk activation (e.g. jumping to
    // another part of the book) has to fit next to the reader UI and the page
    // cache, not just next to the text layout.
    let image_share = if available < 128 * 1024 * 1024 {
        budget / 3
    } else {
        (budget * 3) / 5
    };
    let images = image_share;
    (text, Some(images), budget)
}

/// Chooses the layout chunk size for `total_html` bytes of generated content.
///
/// `usize::MAX` means "lay the whole book out at once", like crengine: page
/// breaks, page numbers and TOC targets then match the original engine and
/// reading never re-lays out a chunk mid-book. When the book is too large for
/// the memory actually available, the layout is split so that one chunk's DOM
/// fits in roughly half of what the kernel can hand out.
pub fn plan_chunk_bytes(total_html: usize, available: Option<usize>) -> usize {
    let Some(available) = available else {
        // No memory information: trust the device rather than degrading the
        // layout for everyone.
        return usize::MAX;
    };
    // Never plan to use more than half of what is available, and always leave
    // the reserve free: the layout spike happens while KOReader is also
    // allocating for the reader UI, and the kernel needs cache headroom.
    let budget = available
        .saturating_sub(LAYOUT_MEMORY_RESERVE)
        .min(available / 2)
        .max(MIN_CHUNK_BYTES * LAYOUT_BYTES_PER_HTML_BYTE);
    let max_html = budget / LAYOUT_BYTES_PER_HTML_BYTE;
    if total_html <= max_html {
        return usize::MAX;
    }
    max_html.clamp(MIN_CHUNK_BYTES, MAX_CHUNK_BYTES)
}

/// Splits a format that is produced as one HTML document (FB2, TXT, a
/// standalone HTML file) into layout chunks when it would not fit in one,
/// using the same memory budget as EPUB chunking.
pub fn split_single_document(html: String, resources: &ResourceSource) -> Vec<String> {
    let available = available_memory_bytes();
    let target = plan_chunk_bytes(html.len(), available);
    if target == usize::MAX || html.len() <= target {
        return vec![html];
    }
    // The same budgets the EPUB chunker uses: decoded images dominate a
    // chunk's footprint, and they cannot be seen in the HTML size. A KF8
    // photo page can be 100 kB of JPEG and 20 MB decoded.
    let (text_budget, image_budget, layout_budget) = chunk_budgets(available);
    let text_limit = target.min(text_budget).max(MIN_CHUNK_BYTES);
    let image_budget = image_budget.unwrap_or(usize::MAX);
    let cost = |section: &str| -> (usize, usize) {
        let images = crate::images::image_sources_in(section)
            .into_iter()
            .filter_map(|key| resources.estimated_decoded_bytes(&key))
            .sum();
        (section.len(), images)
    };
    // MOBI/KF8 bodies are a run of `<section class="cre-spine">` parts; cutting
    // between them keeps each chunk a whole number of parts. A book whose part
    // is bigger than the budget falls back to the generic splitter, which can
    // only respect the text budget.
    if let Some(parts) = split_html_document_at_sections_with(
        &html,
        text_limit,
        image_budget,
        layout_budget,
        LAYOUT_BYTES_PER_HTML_BYTE,
        cost,
    ) {
        return parts;
    }
    split_html_document(&html, text_limit)
}

/// Streams the chunks of a format generated as one document body (MOBI,
/// AZW3/KF8, FB2).
///
/// The generators keep their body separately, so this takes the document head
/// and body and hands one finished chunk at a time to `emit`; the full
/// generated document never exists. Chunk boundaries are the same as
/// [`split_single_document`] would produce.
pub fn stream_single_document_chunks<F>(
    head: &str,
    body: &str,
    resources: &ResourceSource,
    mut emit: F,
) -> Result<()>
where
    F: FnMut(String) -> Result<()>,
{
    const TAIL: &str = "</body></html>";
    let available = available_memory_bytes();
    let html_len = head.len() + body.len() + TAIL.len();
    let target = plan_chunk_bytes(html_len, available);
    if target == usize::MAX || html_len <= target {
        emit(format!("{head}{body}{TAIL}"))?;
        return Ok(());
    }
    // The same budgets the EPUB chunker uses: decoded images dominate a
    // chunk's footprint, and they cannot be seen in the HTML size. A KF8
    // photo page can be 100 kB of JPEG and 20 MB decoded.
    let (text_budget, image_budget, layout_budget) = chunk_budgets(available);
    let text_limit = target.min(text_budget).max(MIN_CHUNK_BYTES);
    let image_budget = image_budget.unwrap_or(usize::MAX);
    let cost = |section: &str| -> (usize, usize) {
        let images = crate::images::image_sources_in(section)
            .into_iter()
            .filter_map(|key| resources.estimated_decoded_bytes(&key))
            .sum();
        (section.len(), images)
    };
    // MOBI/KF8 bodies are a run of `<section class="cre-spine">` parts; cutting
    // between them keeps each chunk a whole number of parts. A book whose part
    // is bigger than the budget falls back to the generic splitter, which can
    // only respect the text budget.
    let mut failure: Option<anyhow::Error> = None;
    let handled = stream_html_document_at_sections_with(
        head,
        body,
        TAIL,
        text_limit,
        image_budget,
        layout_budget,
        LAYOUT_BYTES_PER_HTML_BYTE,
        cost,
        |chunk| {
            if failure.is_none()
                && let Err(error) = emit(chunk)
            {
                failure = Some(error);
            }
        },
    );
    if let Some(error) = failure {
        return Err(error);
    }
    if handled {
        return Ok(());
    }
    for part in split_html_body_parts(body, text_limit) {
        emit(format!("{head}{part}{TAIL}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod splitting_tests {
    use super::*;

    #[test]
    fn sections_are_packed_whole_into_chunks() {
        let html = "<html><head><title>t</title></head><body>\
                    <section class=\"cre-spine\" data-spine-index=\"1\"><p>one</p></section>\
                    <section class=\"cre-spine\" data-spine-index=\"2\"><p>two</p></section>\
                    <section class=\"cre-spine\" data-spine-index=\"3\"><p>three</p></section>\
                    </body></html>";
        // Room for two sections per chunk.
        let limit = 150;
        let parts = split_html_document_at_sections(html, limit).expect("sections");
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].contains("data-spine-index=\"1\""), "{}", parts[0]);
        assert!(parts[0].contains("data-spine-index=\"2\""), "{}", parts[0]);
        assert!(!parts[0].contains("data-spine-index=\"3\""), "{}", parts[0]);
        assert!(parts[1].contains("data-spine-index=\"3\""), "{}", parts[1]);
        // The head and the closing tags are repeated in every part.
        for part in &parts {
            assert!(part.starts_with("<html><head>"), "{part}");
            assert!(part.ends_with("</body></html>"), "{part}");
        }
    }

    #[test]
    fn a_chunk_stays_within_the_image_budget() {
        let html = "<html><body>\
                    <section class=\"cre-spine\"><p>one</p></section>\
                    <section class=\"cre-spine\"><p>two</p></section>\
                    <section class=\"cre-spine\"><p>three</p></section>\
                    </body></html>";
        // Each section claims 10 bytes of decoded images; the budget fits one.
        let parts =
            split_html_document_at_sections_with(html, 1000, 12, 1_000_000, 128, |section| {
                (section.len(), 10)
            })
            .expect("sections");
        assert_eq!(parts.len(), 3, "{parts:?}");
    }

    #[test]
    fn a_leading_stylesheet_is_repeated_in_every_chunk() {
        // KF8 bodies start with the book's CSS before the first part.
        let html = "<html><body><style>p{color:red}</style>\
                    <section class=\"cre-spine\"><p>one</p></section>\
                    <section class=\"cre-spine\"><p>two</p></section>\
                    </body></html>";
        let parts = split_html_document_at_sections(html, 110).expect("sections");
        assert_eq!(parts.len(), 2, "{parts:?}");
        for part in &parts {
            assert!(part.contains("<style>p{color:red}</style>"), "{part}");
        }
    }

    #[test]
    fn streaming_sections_match_the_collected_split() {
        // The streaming variant is what the MOBI/KF8/FB2 open path uses; it
        // must produce exactly the chunks the collecting splitter would.
        let cases = [
            "<html><head><title>t</title></head><body>\
             <section class=\"cre-spine\" data-spine-index=\"1\"><p>one</p></section>\
             <section class=\"cre-spine\" data-spine-index=\"2\"><p>two</p></section>\
             <section class=\"cre-spine\" data-spine-index=\"3\"><p>three</p></section>\
             </body></html>",
            "<html><body><style>p{color:red}</style>\
             <section class=\"cre-spine\"><p>one</p></section>\
             <section class=\"cre-spine\"><p>two</p></section>\
             </body></html>",
        ];
        for html in cases {
            let (head, body, tail) = html_body_parts(html).expect("body parts");
            for limit in [110, 150, 1000] {
                let collected = split_html_document_at_sections(html, limit).expect("sections");
                let mut streamed = Vec::new();
                assert!(stream_html_document_at_sections_with(
                    head,
                    body,
                    tail,
                    limit,
                    usize::MAX,
                    usize::MAX,
                    128,
                    |section| (section.len(), 0),
                    |chunk| streamed.push(chunk),
                ));
                assert_eq!(collected, streamed, "limit {limit}");
            }
        }
    }

    #[test]
    fn a_body_without_sections_or_with_text_between_them_is_generic() {
        let plain = "<html><body><p>a</p><p>b</p></body></html>";
        assert!(split_html_document_at_sections(plain, 10).is_none());
        let mixed = "<html><body><section class=\"cre-spine\"><p>a</p></section>tail</body></html>";
        assert!(split_html_document_at_sections(mixed, 10).is_none());
        // One section larger than the budget: let the generic splitter cut it.
        let large =
            "<html><body><section class=\"cre-spine\"><p>aaaaaaaaaa</p></section></body></html>";
        assert!(split_html_document_at_sections(large, 20).is_none());
    }

    #[test]
    fn split_body_html_keeps_content_and_balances_tags() {
        let body = format!("<div class=\"wrap\">{}</div>", "<p>alpha</p>".repeat(50));
        let parts = split_body_html(&body, 200);
        assert!(
            parts.len() > 1,
            "expected a split, got {} part(s)",
            parts.len()
        );
        for part in &parts {
            assert_eq!(
                part.matches("<div").count(),
                part.matches("</div>").count(),
                "unbalanced div in {part:?}"
            );
            assert_eq!(
                part.matches("<p>").count(),
                part.matches("</p>").count(),
                "unbalanced p in {part:?}"
            );
        }
        let joined = parts.concat();
        assert_eq!(joined.matches("alpha").count(), 50);
    }

    #[test]
    fn split_body_html_does_not_cut_inside_raw_text_elements() {
        let body = format!(
            "<p>start</p><style>p::before {{ content: '<b>not markup</b>'; }}</style>{}",
            "<p>tail</p>".repeat(20)
        );
        let parts = split_body_html(&body, 120);
        assert!(parts.len() > 1);
        let styles = parts
            .iter()
            .filter(|part| part.contains("<style>"))
            .collect::<Vec<_>>();
        assert_eq!(styles.len(), 1, "style element must not be split");
        assert!(styles[0].contains("'<b>not markup</b>'"));
    }

    #[test]
    fn split_body_html_cuts_cjk_text_on_char_boundaries() {
        let body = format!(
            "<div>{}</div>",
            "这是一个用来测试分页切分的很长的中文句子。".repeat(200)
        );
        // A limit that is very unlikely to fall on a character boundary.
        let parts = split_body_html(&body, 1001);
        assert!(parts.len() > 1);
        let joined = parts.concat();
        assert_eq!(
            joined.matches('中').count(),
            body.matches('中').count(),
            "CJK text must survive splitting byte-exactly"
        );
        for part in &parts {
            assert_eq!(
                part.matches("<div").count(),
                part.matches("</div>").count(),
                "unbalanced div in part"
            );
        }
    }

    #[test]
    fn split_body_html_keeps_small_documents_whole() {
        let body = "<p>only a line</p>";
        assert_eq!(split_body_html(body, 4096), vec![body.to_string()]);
    }
}

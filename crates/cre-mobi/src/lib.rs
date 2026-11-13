//! MOBI 6 (`.mobi`, `.prc`, `.azw`) reader.
//!
//! The payload is a single HTML document stored in PalmDOC-compressed records.
//! What it needs before our renderer can use it:
//!
//! * internal links are `filepos="NNNN"` byte offsets into the decompressed
//!   text, and nothing marks the target, so anchors have to be injected at
//!   those offsets (`cre_pdb::html::insert_anchors`),
//! * images are `recindex="N"` references into the PDB resource records,
//! * `<mbp:pagebreak/>` marks chapter boundaries,
//! * the table of contents lives in an INDX index (when the file has one).
//!
//! Same approach as crengine's `pdbfmt.cpp` (`MobiHtmlWriterFilter` +
//! `preprocessMobiHtmlStream`) and calibre's `mobi6.py`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use cre_pdb::html::{self, AnchorTarget, Tag};
use cre_pdb::{
    BookResources, Encoding, MobiHeader, NULL_INDEX, Pdb, ResourceRange, extract_text,
    image_extension, read_index,
};

/// One table of contents entry. `href` is a fragment in the generated document.
#[derive(Clone, Debug, PartialEq)]
pub struct MobiTocItem {
    pub title: String,
    pub href: String,
    pub depth: usize,
}

/// HTML generation options, mirroring `cre_fb2::Fb2HtmlOptions`.
pub struct MobiHtmlOptions {
    pub line_height: f64,
    pub margin_x: u32,
    pub margin_y: u32,
}

impl Default for MobiHtmlOptions {
    fn default() -> Self {
        Self {
            line_height: 1.5,
            margin_x: 40,
            margin_y: 32,
        }
    }
}

pub struct MobiBook {
    pub title: Option<String>,
    pub creator: Option<String>,
    pub language: Option<String>,
    pub description: Option<String>,
    pub keywords: Option<String>,
    pub identifiers: Option<String>,
    /// EXTH 498: the series the book belongs to, when the publisher set it.
    pub series: Option<String>,
    /// Resource name (`mobi-image-N`) to its range in the PalmDB file.
    ///
    /// The bytes are sliced from the mapped file when a page paints them, so a
    /// 25 MB comic costs no anonymous memory for its pictures.
    pub resources: BookResources,
    pub toc: Vec<MobiTocItem>,
    pub cover_image: Option<Vec<u8>>,
    /// Number of spine sections (chapter breaks) in the assembled document.
    pub section_count: usize,
    body: String,
}

impl MobiBook {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        // `Pdb::open` maps the file, so a book's unused image records never
        // enter the open's RSS.
        Self::from_pdb(Pdb::open(path)?)
    }

    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        Self::from_pdb(Pdb::from_bytes(data)?)
    }

    fn from_pdb(pdb: Pdb) -> Result<Self> {
        if !pdb.is_mobi() {
            bail!(
                "not a MOBI file (type {} creator {})",
                pdb.book_type,
                pdb.creator
            );
        }
        let record0 = pdb.record(0).context("MOBI record 0 is missing")?;
        let header = MobiHeader::parse(record0)?;
        if header.magic != *b"MOBI" && &header.magic != b"\0\0\0\0" {
            bail!("MOBI header has an unknown signature");
        }
        if header.encryption != 0 {
            bail!("DRM-protected MOBI files are not supported");
        }
        if header.is_kf8() {
            bail!("this file is KF8/AZW3; use the KF8 reader");
        }
        let raw = extract_text(&pdb, &header, 1, header.text_record_count)?;
        let raw = clean_text(raw, header.encoding);

        // 1. Resolve every filepos target to an id and inject the anchors.
        let offsets = filepos_references(&raw);
        let mut id_by_offset: HashMap<u32, String> = HashMap::new();
        let mut targets = Vec::with_capacity(offsets.len());
        for offset in &offsets {
            let (id, needs_anchor) = match html::start_tag_at(&raw, *offset as usize) {
                Some(found) => {
                    let existing = found
                        .tag
                        .attribute("filepos-id")
                        .or_else(|| found.tag.attribute("id"))
                        .map(str::to_string);
                    match existing {
                        Some(id) => (id, false),
                        None => (format!("filepos{offset}"), true),
                    }
                }
                None => (format!("filepos{offset}"), true),
            };
            id_by_offset.insert(*offset, id.clone());
            if needs_anchor {
                targets.push(AnchorTarget {
                    offset: *offset as usize,
                    id,
                });
            }
        }
        let with_anchors = html::insert_anchors(&raw, &targets);

        // 2. Rewrite the MOBI-specific attributes and the page breaks.
        let rewritten = html::rewrite_tags(&with_anchors, |tag| rewrite_tag(tag, &id_by_offset));
        // 3. Keep the body only: the payload is normally a full HTML document.
        let body = extract_body(&rewritten).unwrap_or(rewritten);

        // 4. Chapter sections, one per page break, so page breaks and XPointer
        //    `DocFragment` indices work like they do for EPUB spine items.
        let (body, section_count) = wrap_sections(&body);

        // 5. Images and other resources: ranges into the PalmDB, not copies.
        let (resource_ranges, cover_image) = build_resources(&pdb, &header);

        // 6. Metadata.
        let encoding = header.encoding;
        let title = header
            .exth
            .string(503, encoding)
            .or_else(|| {
                let name = header.full_name.trim();
                (!name.is_empty()).then(|| name.to_string())
            })
            .or_else(|| {
                let name = pdb.name.trim();
                (!name.is_empty()).then(|| name.replace('_', " "))
            });
        let creator = header.exth.joined_strings(100, encoding);
        let language = header.exth.string(524, encoding);
        let description = header.exth.string(103, encoding);
        let keywords = header.exth.string(105, encoding);
        let identifiers = header
            .exth
            .string(104, encoding)
            .map(|isbn| format!("ISBN: {isbn}"))
            .or_else(|| header.exth.string(113, encoding));
        let series = header.exth.string(498, encoding);

        // 7. Table of contents: the NCX index when the file has one, otherwise
        //    one entry per chapter section with its heading text.
        let mut toc = ncx_toc(&pdb, &header, &id_by_offset);
        if toc.is_empty() {
            toc = section_toc(&body);
        }

        // 8. Keep the file bytes and the record ranges, so a picture is sliced
        //    out of the file when a page paints it instead of being copied at
        //    open time.
        let mut resources = BookResources::new(pdb.into_backing());
        for (name, range) in resource_ranges {
            resources.insert_range(name, range);
        }
        // The parse just faulted in the text and index records; none of them
        // are needed again, so leave them in the page cache instead of RSS.
        resources.drop_pages();

        Ok(Self {
            title,
            creator,
            language,
            description,
            keywords,
            identifiers,
            series,
            resources,
            toc,
            cover_image,
            section_count,
            body,
        })
    }

    /// The assembled `body` HTML, for tests and diagnostics.
    pub fn body_html(&self) -> &str {
        &self.body
    }

    pub fn to_html(&self, options: &MobiHtmlOptions) -> String {
        self.to_html_with_toc(options).0
    }

    pub fn to_html_with_toc(&self, options: &MobiHtmlOptions) -> (String, Vec<MobiTocItem>) {
        let html = format!("{}{}</body></html>", self.html_head(options), self.body);
        (html, self.toc.clone())
    }

    /// The document prefix through the `<body>` open tag.
    ///
    /// [`Self::body_html`] + this + `"</body></html>"` is exactly
    /// [`Self::to_html_with_toc`], which lets a caller build one chunk at a
    /// time instead of materializing the whole generated document.
    pub fn html_head(&self, options: &MobiHtmlOptions) -> String {
        let css = base_css(options);
        let title = escape_html(self.title.as_deref().unwrap_or(""));
        let language = escape_html(self.language.as_deref().unwrap_or("und"));
        format!(
            "<!DOCTYPE html><html lang=\"{language}\"><head><meta charset=\"utf-8\">\
             <title>{title}</title><style>{css}</style></head><body>"
        )
    }
}

fn base_css(options: &MobiHtmlOptions) -> String {
    format!(
        "html, body {{ margin: 0; padding: 0; }}\n\
         html {{ font-size: {}px; }}\n\
         body {{ font-family: serif; font-size: 1em; line-height: {}; padding: {}px {}px; }}\n\
         h1, h2, h3, h4, h5, h6 {{ font-family: sans-serif; }}\n\
         .cre-spine {{ display: block; page-break-before: always; }}\n\
         .cre-pagebreak {{ page-break-before: always; }}",
        cre_css::FONT_SIZE,
        options.line_height,
        options.margin_y,
        options.margin_x
    )
}

/// Drops the control bytes the MOBI text uses as separators.
fn clean_text(mut raw: Vec<u8>, encoding: Encoding) -> Vec<u8> {
    raw.retain(|byte| *byte != 0);
    if encoding == Encoding::Cp1252 {
        raw.retain(|byte| *byte != 0x1E && *byte != 0x02);
    }
    raw
}

/// Numeric values of every `filepos=` attribute, in ascending order.
fn filepos_references(data: &[u8]) -> Vec<u32> {
    let mut refs = Vec::new();
    let mut at = 0usize;
    while at + 7 < data.len() {
        if !data[at..].starts_with(b"filepos") && !data[at..].starts_with(b"FILEPOS") {
            at += 1;
            continue;
        }
        let mut pos = at + 7;
        while pos < data.len() && data[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if data.get(pos) != Some(&b'=') {
            at += 1;
            continue;
        }
        pos += 1;
        while pos < data.len()
            && (data[pos].is_ascii_whitespace() || data[pos] == b'"' || data[pos] == b'\'')
        {
            pos += 1;
        }
        let start = pos;
        let mut value: u64 = 0;
        while pos < data.len() && data[pos].is_ascii_digit() {
            value = value
                .saturating_mul(10)
                .saturating_add((data[pos] - b'0') as u64);
            pos += 1;
        }
        if pos > start && value > 0 && value <= data.len() as u64 {
            refs.push(value as u32);
        }
        at = pos.max(at + 1);
    }
    refs.sort_unstable();
    refs.dedup();
    refs
}

/// True when the open tag's name is `name` (case-insensitive).
fn tag_name_is(tag: &[u8], name: &[u8]) -> bool {
    let mut index = 1;
    while index < tag.len() && (tag[index] as char).is_ascii_whitespace() {
        index += 1;
    }
    let rest = &tag[index..];
    if rest.len() < name.len() {
        return false;
    }
    if !rest[..name.len()].eq_ignore_ascii_case(name) {
        return false;
    }
    rest.get(name.len())
        .is_none_or(|byte| byte.is_ascii_whitespace() || *byte == b'>' || *byte == b'/')
}

fn rewrite_tag(tag: &[u8], id_by_offset: &HashMap<u32, String>) -> Vec<u8> {
    if tag.starts_with(b"</") {
        // `</mbp:pagebreak>` carries no content.
        let lower = tag.to_ascii_lowercase();
        if lower.starts_with(b"</mbp:") {
            return Vec::new();
        }
        return tag.to_vec();
    }
    let lower = tag.to_ascii_lowercase();
    if lower.starts_with(b"<mbp:pagebreak") || lower.starts_with(b"<mbp:page_break") {
        return b"<div class=\"cre-pagebreak\"></div>".to_vec();
    }
    let Some(mut parsed) = Tag::parse(tag) else {
        return tag.to_vec();
    };
    let mut changed = false;

    if let Some(value) = parsed.attribute("filepos").map(str::to_string) {
        parsed.remove_attribute("filepos");
        let offset: Option<u32> = value.trim().parse().ok();
        let target = offset
            .and_then(|offset| id_by_offset.get(&offset).cloned())
            .or_else(|| offset.map(|offset| format!("filepos{offset}")));
        if let Some(target) = target {
            parsed.set_attribute("href", &format!("#{target}"));
            changed = true;
        }
    }
    if parsed.attribute("filepos-id").is_some() {
        parsed.rename_attribute("filepos-id", "id");
        changed = true;
    }
    if let Some(value) = parsed.attribute("recindex").map(str::to_string)
        && let Ok(index) = value.trim().parse::<u32>()
    {
        parsed.remove_attribute("recindex");
        parsed.set_attribute("src", &format!("mobi-image-{index}"));
        changed = true;
    }
    if parsed.attribute("xmlns:mbp").is_some() {
        parsed.remove_attribute("xmlns:mbp");
        changed = true;
    }
    // MOBI writers put `height="1em" width="2em"` on `<p>` and `<div>` as
    // spacing/indent hints, and the odd cover paragraph says
    // `height="0pt" width="0pt"`. They are not the CSS box dimensions the
    // attributes mean on an image or a table cell, and a zero one would
    // collapse the box it is on, so drop them rather than let a hint rule take
    // them literally. (`align` is a real hint and stays.)
    if tag_name_is(tag, b"p") || tag_name_is(tag, b"div") {
        for name in ["height", "width"] {
            if parsed.attribute(name).is_some() {
                parsed.remove_attribute(name);
                changed = true;
            }
        }
    }
    if changed {
        parsed.to_bytes()
    } else {
        tag.to_vec()
    }
}

/// Returns the inside of the document's `<body>`, when the payload has one.
fn extract_body(data: &[u8]) -> Option<Vec<u8>> {
    let lower = data.to_ascii_lowercase();
    let body_at = lower.windows(5).position(|window| window == b"<body")?;
    let open_end = lower[body_at..].iter().position(|byte| *byte == b'>')? + body_at + 1;
    let close = lower
        .windows(7)
        .rposition(|window| window == b"</body>")
        .unwrap_or(data.len());
    (close >= open_end).then(|| data[open_end..close].to_vec())
}

const PAGEBREAK_MARKER: &[u8] = b"<div class=\"cre-pagebreak\"></div>";

/// Splits the body at page breaks and wraps each part in a spine section.
fn wrap_sections(body: &[u8]) -> (String, usize) {
    let mut html = String::new();
    let mut count = 0usize;
    for part in split_on(body, PAGEBREAK_MARKER) {
        let text = String::from_utf8_lossy(part);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        count += 1;
        // Zero-based, like the EPUB generator: cre-lib adds one to get the
        // 1-based `DocFragment[n]` crengine uses.
        html.push_str(&format!(
            "<section class=\"cre-spine\" data-spine-index=\"{}\" id=\"cre-sec-{count}\">",
            count - 1
        ));
        html.push_str(trimmed);
        html.push_str("</section>\n");
    }
    if count == 0 {
        // No page breaks at all: still give the document one section so
        // XPointers have a DocFragment to name.
        let text = String::from_utf8_lossy(body).trim().to_string();
        html.push_str("<section class=\"cre-spine\" data-spine-index=\"0\" id=\"cre-sec-1\">");
        html.push_str(&text);
        html.push_str("</section>\n");
        count = 1;
    }
    (html, count)
}

fn split_on<'a>(data: &'a [u8], separator: &[u8]) -> Vec<&'a [u8]> {
    let mut parts = Vec::new();
    let mut at = 0usize;
    while at < data.len() {
        match data[at..]
            .windows(separator.len())
            .position(|window| window == separator)
        {
            Some(relative) => {
                parts.push(&data[at..at + relative]);
                at += relative + separator.len();
            }
            None => {
                parts.push(&data[at..]);
                break;
            }
        }
    }
    parts
}

/// Builds the resource map: `mobi-image-N` for record N-1 after the first image
/// record, which is how `recindex` numbers them.
fn build_resources(pdb: &Pdb, header: &MobiHeader) -> (Vec<ResourceRange>, Option<Vec<u8>>) {
    let mut resources = Vec::new();
    let mut cover = None;
    let first = header.first_image_index;
    if first == NULL_INDEX || first as usize >= pdb.records() {
        return (resources, cover);
    }
    let cover_index = header.exth.uint(201);
    for record_index in first as usize..pdb.records() {
        let Some(data) = pdb.record(record_index) else {
            break;
        };
        if cre_pdb::is_known_non_image_record(data) {
            continue;
        }
        if image_extension(data).is_none() {
            continue;
        }
        let Some(range) = pdb.record_range(record_index) else {
            break;
        };
        let index = (record_index - first as usize) + 1;
        let name = format!("mobi-image-{index}");
        if cover_index == Some((index - 1) as u32) {
            // The reader UI asks for the cover as bytes; copy just this one.
            cover = Some(data.to_vec());
        }
        resources.push((name, range));
    }
    (resources, cover)
}

/// Builds the TOC from the NCX INDX index, resolving each entry's byte offset to
/// the anchor injected there.
fn ncx_toc(
    pdb: &Pdb,
    header: &MobiHeader,
    id_by_offset: &HashMap<u32, String>,
) -> Vec<MobiTocItem> {
    if header.ncx_index == NULL_INDEX {
        return Vec::new();
    }
    let Ok(index) = read_index(pdb, header.ncx_index as usize, header.encoding) else {
        return Vec::new();
    };
    let mut toc = Vec::new();
    for entry in &index.entries {
        let Some(filepos) = entry.tag(1) else {
            continue;
        };
        let Some(title_offset) = entry.tag(3) else {
            continue;
        };
        let Some(title) = index.string(title_offset) else {
            continue;
        };
        let title = title.trim();
        if title.is_empty() {
            continue;
        }
        let id = id_by_offset
            .get(&filepos)
            .cloned()
            .unwrap_or_else(|| format!("filepos{filepos}"));
        toc.push(MobiTocItem {
            title: title.to_string(),
            href: format!("#{id}"),
            depth: entry.tag(4).unwrap_or(0) as usize,
        });
    }
    toc
}

/// Fallback table of contents: one entry per chapter section, titled with the
/// section's first heading (or its first line of text).
fn section_toc(body: &str) -> Vec<MobiTocItem> {
    let mut toc = Vec::new();
    for (index, section) in body.split("<section ").skip(1).enumerate() {
        // `split` leaves the section's own start tag in front; drop it so its
        // attributes are not mistaken for text.
        let content = match section.find('>') {
            Some(end) => &section[end + 1..],
            None => section,
        };
        let title = section_heading(content).unwrap_or_else(|| {
            let text = strip_tags(content);
            let text = text.trim();
            text.chars().take(48).collect()
        });
        if title.trim().is_empty() {
            continue;
        }
        toc.push(MobiTocItem {
            title: title.trim().to_string(),
            href: format!("#cre-sec-{}", index + 1),
            depth: 0,
        });
    }
    toc
}

/// The best available title for a chapter section.
///
/// MOBI books rarely use `<h1>`: the chapter title is normally its own
/// paragraph, often centred and bold. So: a real heading if there is one,
/// otherwise the first paragraph that carries text.
fn section_heading(section: &str) -> Option<String> {
    let lower = section.to_ascii_lowercase();
    for level in 1..=6 {
        let open = format!("<h{level}");
        let close = format!("</h{level}>");
        let Some(start) = lower.find(&open) else {
            continue;
        };
        let content_start = lower[start..].find('>')? + start + 1;
        let end = lower[content_start..].find(&close)? + content_start;
        let text = strip_tags(&section[content_start..end]);
        if !text.trim().is_empty() {
            return Some(text.trim().to_string());
        }
    }
    let text = block_texts(section, 6).into_iter().next()?;
    let shortened: String = text.chars().take(40).collect();
    Some(if shortened.chars().count() < text.chars().count() {
        format!("{shortened}…")
    } else {
        shortened
    })
}

/// Text of the first `limit` block elements (`<p>`, `<div>`) in `html`.
fn block_texts(html: &str, limit: usize) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut at = 0usize;
    while out.len() < limit {
        let next_p = lower[at..].find("<p").map(|index| index + at);
        let next_div = lower[at..].find("<div").map(|index| index + at);
        let Some(start) = (match (next_p, next_div) {
            (Some(p), Some(d)) => Some(p.min(d)),
            (Some(p), None) => Some(p),
            (None, Some(d)) => Some(d),
            (None, None) => None,
        }) else {
            break;
        };
        let Some(open_end) = lower[start..].find('>').map(|index| index + start + 1) else {
            break;
        };
        let ends = [
            lower[open_end..].find("</p>"),
            lower[open_end..].find("</div>"),
            lower[open_end..].find("<p"),
            lower[open_end..].find("<div"),
        ];
        let end = ends
            .into_iter()
            .flatten()
            .min()
            .map(|index| index + open_end)
            .unwrap_or(lower.len());
        let text = strip_tags(&html[open_end..end]).trim().to_string();
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if !text.is_empty() {
            out.push(text);
        }
        at = if end > open_end { end } else { open_end };
    }
    out
}

fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for ch in text.chars() {
        match ch {
            '<' => inside = true,
            '>' => inside = false,
            _ if !inside => out.push(ch),
            _ => {}
        }
    }
    out
}

pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal MOBI 6 file: PalmDB container, PalmDOC + MOBI header,
    /// EXTH metadata, one uncompressed text record and one image record.
    fn build_mobi(text: &str, title: &str, author: &str, image: &[u8]) -> Vec<u8> {
        let text_bytes = text.as_bytes();
        let text_records = 1usize;
        let first_image_index = 1 + text_records;

        // --- record 0: PalmDOC header, MOBI header, EXTH, full name ---
        let mut record0 = vec![0u8; 16];
        let mut mobi = vec![0u8; 232];
        mobi[0..4].copy_from_slice(b"MOBI");
        mobi[4..8].copy_from_slice(&232u32.to_be_bytes());
        mobi[8..12].copy_from_slice(&2u32.to_be_bytes()); // book
        mobi[12..16].copy_from_slice(&65001u32.to_be_bytes()); // UTF-8
        mobi[16..20].copy_from_slice(&1u32.to_be_bytes()); // unique id
        mobi[20..24].copy_from_slice(&6u32.to_be_bytes()); // MOBI 6
        for byte in mobi[24..64].iter_mut() {
            *byte = 0xFF;
        }
        // Record-0 offsets 80, 92, 108, 112, 128 are `mobi` offsets 64, 76, 92,
        // 96, 112 (the header starts 16 bytes into record 0).
        mobi[64..68].copy_from_slice(&((1 + text_records) as u32).to_be_bytes());
        mobi[76..80].copy_from_slice(&1033u32.to_be_bytes()); // locale
        mobi[92..96].copy_from_slice(&(first_image_index as u32).to_be_bytes());
        mobi[96..100].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes()); // huffman offset
        mobi[100..104].copy_from_slice(&0u32.to_be_bytes()); // huffman count

        let mut exth = Vec::new();
        exth.extend_from_slice(b"EXTH");
        exth.extend_from_slice(&0u32.to_be_bytes()); // length, patched below
        exth.extend_from_slice(&3u32.to_be_bytes()); // count
        for (tag, value) in [
            (503u32, title.as_bytes()),
            (100u32, author.as_bytes()),
            (201u32, &[0u8, 0, 0, 0]),
        ] {
            exth.extend_from_slice(&tag.to_be_bytes());
            exth.extend_from_slice(&((value.len() + 8) as u32).to_be_bytes());
            exth.extend_from_slice(value);
        }
        let exth_length = exth.len() as u32;
        exth[4..8].copy_from_slice(&exth_length.to_be_bytes());

        let full_name = title.as_bytes();
        let full_name_offset = 16 + mobi.len() + exth.len();
        mobi[84 - 16..88 - 16].copy_from_slice(&(full_name_offset as u32).to_be_bytes());
        mobi[88 - 16..92 - 16].copy_from_slice(&(full_name.len() as u32).to_be_bytes());
        // EXTH flags live at record 0 offset 0x80.
        let flags_at = 0x80 - 16;
        mobi[flags_at..flags_at + 4].copy_from_slice(&0x40u32.to_be_bytes());
        // Extra data flags at record 0 offset 242, NCX index at 244.
        let extra_at = 242 - 16;
        mobi[extra_at..extra_at + 2].copy_from_slice(&0u16.to_be_bytes());
        let ncx_at = 244 - 16;
        mobi[ncx_at..ncx_at + 4].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());

        record0.extend_from_slice(&mobi);
        record0.extend_from_slice(&exth);
        record0.extend_from_slice(full_name);
        record0[0..2].copy_from_slice(&1u16.to_be_bytes()); // no compression
        record0[4..8].copy_from_slice(&(text_bytes.len() as u32).to_be_bytes());
        record0[8..10].copy_from_slice(&(text_records as u16).to_be_bytes());
        record0[10..12].copy_from_slice(&4096u16.to_be_bytes());
        record0[12..14].copy_from_slice(&0u16.to_be_bytes()); // no DRM

        let records: Vec<Vec<u8>> = vec![record0, text_bytes.to_vec(), image.to_vec()];

        // --- PalmDB container ---
        let mut file = Vec::new();
        let mut header = vec![0u8; 78];
        header[..7].copy_from_slice(b"fixture");
        header[60..64].copy_from_slice(b"BOOK");
        header[64..68].copy_from_slice(b"MOBI");
        header[76..78].copy_from_slice(&(records.len() as u16).to_be_bytes());
        file.extend_from_slice(&header);

        let data_start = 78 + records.len() * 8 + 2;
        let mut offset = data_start;
        for record in &records {
            file.extend_from_slice(&(offset as u32).to_be_bytes());
            file.extend_from_slice(&0u32.to_be_bytes());
            offset += record.len();
        }
        file.extend_from_slice(&[0, 0]);
        for record in &records {
            file.extend_from_slice(record);
        }
        file
    }

    /// One 1x1 white GIF, enough to be recognised as an image.
    const PIXEL_GIF: &[u8] = &[
        0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0xFF, 0xFF,
        0xFF, 0x00, 0x00, 0x00, 0x21, 0xF9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2C, 0x00, 0x00,
        0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3B,
    ];

    #[test]
    fn reads_a_synthesised_mobi() {
        let body = "<html><head><title>t</title></head><body>\
                    <p>Chapter One</p>\
                    <p><a filepos=0000045>to chapter two</a></p>\
                    <mbp:pagebreak/>\
                    <h2>Chapter Two</h2><p>text two</p>\
                    <img recindex=\"1\"/>\
                    </body></html>";
        // The link target must point at the second chapter's heading.
        let target = body.find("Chapter Two").unwrap();
        let body = body.replacen("0000045", &format!("{target:07}"), 1);
        let file = build_mobi(&body, "Fixture", "An Author", PIXEL_GIF);

        let book = MobiBook::from_bytes(file).expect("read fixture");
        assert_eq!(book.title.as_deref(), Some("Fixture"));
        assert_eq!(book.creator.as_deref(), Some("An Author"));
        assert_eq!(book.section_count, 2);
        assert!(book.resources.contains_key("mobi-image-1"));
        assert_eq!(
            book.cover_image.as_ref().map(|image| image.len()),
            Some(PIXEL_GIF.len())
        );

        // The link became a fragment link and the target got an anchor.
        let html = book.to_html(&MobiHtmlOptions::default());
        assert!(html.contains("href=\"#filepos"), "{html}");
        assert!(html.contains("id=\"filepos"), "{html}");
        assert!(html.contains("src=\"mobi-image-1\""), "{html}");
        assert!(!html.contains("filepos="), "raw filepos left: {html}");
        assert!(!html.contains("recindex"), "raw recindex left: {html}");

        // With no NCX index the TOC falls back to the chapter headings.
        assert_eq!(book.toc.len(), 2, "{:?}", book.toc);
        assert_eq!(book.toc[0].title, "Chapter One");
        assert_eq!(book.toc[1].title, "Chapter Two");
        assert_eq!(book.toc[1].href, "#cre-sec-2");
    }

    #[test]
    fn filepos_references_finds_offsets() {
        let data = br#"<A FILEPOS=10>a</A> <a filepos="42">b</a> <a href="x">c</a>"#;
        assert_eq!(filepos_references(data), vec![10, 42]);
    }

    #[test]
    fn pagebreaks_become_sections() {
        let body = b"<p>one</p><div class=\"cre-pagebreak\"></div><p>two</p>";
        let (html, sections) = wrap_sections(body);
        assert_eq!(sections, 2);
        // Zero-based: cre-lib adds one for `DocFragment[n]`.
        assert!(html.contains("data-spine-index=\"0\""));
        assert!(html.contains("data-spine-index=\"1\""));
        assert!(html.contains("<p>two</p>"));
    }

    #[test]
    fn attributes_are_rewritten() {
        let mut ids = HashMap::new();
        ids.insert(12u32, "target".to_string());
        let out = rewrite_tag(b"<a filepos=12>", &ids);
        assert_eq!(String::from_utf8(out).unwrap(), "<a href=\"#target\">");
        let out = rewrite_tag(b"<a filepos=99>", &ids);
        assert_eq!(String::from_utf8(out).unwrap(), "<a href=\"#filepos99\">");
        let out = rewrite_tag(b"<img recindex=\"0003\" />", &ids);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "<img src=\"mobi-image-3\"/>"
        );
        let out = rewrite_tag(b"<mbp:pagebreak/>", &ids);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "<div class=\"cre-pagebreak\"></div>"
        );
    }

    #[test]
    fn body_extraction_keeps_inner_markup() {
        let data = b"<html><head><title>x</title></head><body><p>hi</p></body></html>";
        assert_eq!(extract_body(data).unwrap(), b"<p>hi</p>".to_vec());
    }
}

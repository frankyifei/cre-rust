//! KF8 (AZW3) reader.
//!
//! A KF8 file is still a PalmDB/MOBI container, but its text is not one HTML
//! document: the raw text holds *skeletons* (the outer markup of each XHTML
//! file) and *fragments* (the pieces that belong inside them), described by two
//! INDX tables. A third index (FDST) says how the raw text is split into
//! "flows", one of which is the main text and the rest are the original CSS and
//! SVG. Links are expressed as positions (`kindle:pos:fid:…:off:…`) rather than
//! anchors, and images/CSS are `kindle:embed:…`/`kindle:flow:…` URLs.
//!
//! The reconstruction follows calibre's `mobi8.py` (`build_parts`), and the
//! link/anchor handling uses the same trick as crengine's MOBI reader: inject an
//! `id` at the byte offset each link points at, then rewrite the link to a
//! fragment. All numbers in `kindle:` URLs are base 32.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use cre_pdb::html::{self, AnchorTarget, Tag};
use cre_pdb::{
    Index, MobiHeader, NULL_INDEX, Pdb, extract_text, image_extension, is_known_non_image_record,
    read_index,
};

/// One table of contents entry; `href` is a fragment in the generated document.
#[derive(Clone, Debug, PartialEq)]
pub struct Kf8TocItem {
    pub title: String,
    pub href: String,
    pub depth: usize,
}

/// HTML generation options, mirroring `cre_mobi::MobiHtmlOptions`.
pub struct Kf8HtmlOptions {
    pub font_size: f32,
    pub line_height: f64,
    pub margin_x: u32,
    pub margin_y: u32,
}

impl Default for Kf8HtmlOptions {
    fn default() -> Self {
        Self {
            font_size: 18.0,
            line_height: 1.5,
            margin_x: 40,
            margin_y: 32,
        }
    }
}

pub struct Kf8Book {
    pub title: Option<String>,
    pub creator: Option<String>,
    pub language: Option<String>,
    pub description: Option<String>,
    pub keywords: Option<String>,
    pub identifiers: Option<String>,
    /// Resource name (`mobi-image-N`) to image bytes.
    pub resources: HashMap<String, Vec<u8>>,
    pub toc: Vec<Kf8TocItem>,
    pub cover_image: Option<Vec<u8>>,
    /// Number of XHTML parts (spine sections) in the book.
    pub section_count: usize,
    body: String,
}

impl Kf8Book {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        Self::from_bytes(data)
    }

    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        let pdb = Pdb::from_bytes(data)?;
        if !pdb.is_mobi() {
            bail!(
                "not a MOBI/KF8 file (type {} creator {})",
                pdb.book_type,
                pdb.creator
            );
        }
        let record0 = pdb.record(0).context("KF8 record 0 is missing")?;
        let header = MobiHeader::parse(record0)?;
        if header.encryption != 0 {
            bail!("DRM-protected KF8 files are not supported");
        }
        if !header.is_kf8() {
            bail!("this file is MOBI 6, not KF8/AZW3");
        }

        let raw = extract_text(&pdb, &header, 1, header.text_record_count)?;
        let flows = split_flows(&pdb, &header, &raw)?;
        let text = flows.first().cloned().unwrap_or_default();

        let skeleton_index = read_index(&pdb, header.skel_index as usize, header.encoding)?;
        let fragment_index = read_index(&pdb, header.frag_index as usize, header.encoding)?;
        let skeletons = skeletons(&skeleton_index);
        let fragments = fragments(&fragment_index);
        if skeletons.is_empty() {
            bail!("KF8 skeleton table is empty");
        }

        // 1. Rebuild each XHTML part from its skeleton and fragments.
        let mut parts: Vec<Part> = Vec::with_capacity(skeletons.len());
        let mut fragment_at = 0usize;
        for skeleton in &skeletons {
            let Some(skeleton_bytes) = text.get(skeleton.start..skeleton.start + skeleton.length)
            else {
                continue;
            };
            let mut part = Part::new(skeleton_bytes, skeleton.start);
            let mut base = skeleton.start + skeleton.length;
            for _ in 0..skeleton.fragment_count {
                let Some(fragment) = fragments.get(fragment_at) else {
                    break;
                };
                fragment_at += 1;
                if fragment.length == 0 || base >= text.len() {
                    continue;
                }
                let end = (base + fragment.length).min(text.len());
                let data = &text[base..end];
                base = end;
                part.insert(
                    fragment.insert_pos.saturating_sub(skeleton.start),
                    data,
                    base - data.len(),
                );
            }
            parts.push(part);
        }
        if parts.is_empty() {
            bail!("KF8 has no readable parts");
        }

        // 2. Prefix each part's ids: Kindlegen numbers them per file, and all
        //    parts end up in one document (see `prefix_part_ids`).
        for (index, part) in parts.iter_mut().enumerate() {
            let bytes = std::mem::take(&mut part.bytes);
            part.bytes = prefix_part_ids(&bytes, index + 1);
        }

        // 3. Resolve every link target to an anchor id.
        let mut link_targets: Vec<(u32, u32)> = Vec::new();
        for entry in ncx_entries(&pdb, &header) {
            if let Some((fid, off)) = entry.pos_fid() {
                link_targets.push((fid, off));
            }
        }
        for part in &parts {
            collect_pos_links(&part.bytes, &mut link_targets);
        }
        let target_ids = resolve_targets(&mut parts, &fragments, &link_targets);

        // 4. Rewrite the `kindle:` URLs and anchor links in each part.
        let styles = flow_styles(&flows);
        for part in parts.iter_mut() {
            let bytes = std::mem::take(&mut part.bytes);
            let rewritten = html::rewrite_tags(&bytes, |tag| rewrite_tag(tag, &target_ids));
            let rewritten = drop_flow_links(&rewritten);
            part.bytes = rewritten;
        }

        // 5. One spine section per part, so page breaks and XPointer
        //    `DocFragment` indices match the book's own files.
        let mut body = String::new();
        let mut section_count = 0usize;
        for part in &parts {
            let inner = extract_body(&part.bytes).unwrap_or_else(|| part.bytes.clone());
            let text = String::from_utf8_lossy(&inner);
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            section_count += 1;
            body.push_str(&format!(
                "<section class=\"cre-spine\" data-spine-index=\"{section_count}\" id=\"cre-sec-{section_count}\">"
            ));
            body.push_str(trimmed);
            body.push_str("</section>\n");
        }
        if section_count == 0 {
            bail!("KF8 produced no text");
        }

        // 6. Resources and cover.
        let (resources, cover_image) = build_resources(&pdb, &header);

        // 7. Metadata.
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

        // 8. Table of contents from the NCX, else from the part headings.
        let mut toc = kf8_toc(&pdb, &header, &target_ids, &parts);
        if toc.is_empty() {
            toc = section_toc(&body);
        }

        Ok(Self {
            title,
            creator,
            language,
            description,
            keywords,
            identifiers,
            resources,
            toc,
            cover_image,
            section_count,
            body: with_styles(&body, &styles),
        })
    }

    pub fn body_html(&self) -> &str {
        &self.body
    }

    pub fn to_html(&self, options: &Kf8HtmlOptions) -> String {
        self.to_html_with_toc(options).0
    }

    pub fn to_html_with_toc(&self, options: &Kf8HtmlOptions) -> (String, Vec<Kf8TocItem>) {
        let css = base_css(options);
        let title = escape_html(self.title.as_deref().unwrap_or(""));
        let language = escape_html(self.language.as_deref().unwrap_or("und"));
        let html = format!(
            "<!DOCTYPE html><html lang=\"{language}\"><head><meta charset=\"utf-8\">\
             <title>{title}</title><style>{css}</style></head><body>{}</body></html>",
            self.body
        );
        (html, self.toc.clone())
    }
}

fn base_css(options: &Kf8HtmlOptions) -> String {
    format!(
        "html, body {{ margin: 0; padding: 0; }}\n\
         body {{ font-family: serif; font-size: {}px; line-height: {}; padding: {}px {}px; }}\n\
         img {{ max-width: 100%; height: auto; }}\n\
         h1, h2, h3, h4, h5, h6 {{ font-family: sans-serif; }}\n\
         .cre-spine {{ display: block; page-break-before: always; }}",
        options.font_size, options.line_height, options.margin_y, options.margin_x
    )
}

// ---------------------------------------------------------------------------
// Flows and part assembly
// ---------------------------------------------------------------------------

/// Splits the raw text into flows using the FDST record.
fn split_flows(pdb: &Pdb, header: &MobiHeader, raw: &[u8]) -> Result<Vec<Vec<u8>>> {
    if header.fdst_index == NULL_INDEX {
        return Ok(vec![raw.to_vec()]);
    }
    let record = pdb
        .record(header.fdst_index as usize)
        .context("KF8 FDST record is missing")?;
    if record.len() < 12 || &record[..4] != b"FDST" {
        bail!("KF8 FDST record is malformed");
    }
    let section_start = cre_pdb::be32(record, 4)? as usize;
    let count = cre_pdb::be32(record, 8)? as usize;
    if count == 0 || count > 256 || section_start + count * 8 > record.len() {
        bail!("KF8 FDST record has an implausible flow table");
    }
    let mut flows = Vec::with_capacity(count);
    for index in 0..count {
        let start = cre_pdb::be32(record, section_start + index * 8)? as usize;
        let end = cre_pdb::be32(record, section_start + index * 8 + 4)? as usize;
        let end = end.min(raw.len());
        let start = start.min(end);
        flows.push(raw[start..end].to_vec());
    }
    Ok(flows)
}

struct Skeleton {
    fragment_count: usize,
    start: usize,
    length: usize,
}

/// Skeleton table: tag 1 = fragment count, tag 6 = (start, length).
fn skeletons(index: &Index) -> Vec<Skeleton> {
    index
        .entries
        .iter()
        .filter_map(|entry| {
            let (start, length) = entry.tag_pair(6)?;
            Some(Skeleton {
                fragment_count: entry.tag(1).unwrap_or(0) as usize,
                start: start as usize,
                length: length as usize,
            })
        })
        .collect()
}

struct Fragment {
    insert_pos: usize,
    length: usize,
}

/// Fragment ("div") table: the entry ident is the insert position, tag 6 carries
/// (start, length) of the fragment text.
fn fragments(index: &Index) -> Vec<Fragment> {
    index
        .entries
        .iter()
        .filter_map(|entry| {
            let insert_pos = entry.ident_number()? as usize;
            let (_, length) = entry.tag_pair(6)?;
            Some(Fragment {
                insert_pos,
                length: length as usize,
            })
        })
        .collect()
}

/// Raw-text range to assembled-part range mapping for one segment.
struct Segment {
    raw_start: usize,
    part_start: usize,
    len: usize,
}

/// One reconstructed XHTML file.
struct Part {
    bytes: Vec<u8>,
    segments: Vec<Segment>,
}

impl Part {
    fn new(skeleton: &[u8], raw_start: usize) -> Self {
        Self {
            bytes: skeleton.to_vec(),
            segments: vec![Segment {
                raw_start,
                part_start: 0,
                len: skeleton.len(),
            }],
        }
    }

    /// Inserts a fragment at a skeleton-relative position, keeping the raw
    /// position map accurate.
    fn insert(&mut self, at: usize, data: &[u8], raw_start: usize) {
        let at = at.min(self.bytes.len());
        self.bytes.splice(at..at, data.iter().copied());
        for segment in self.segments.iter_mut() {
            if segment.part_start >= at {
                segment.part_start += data.len();
            }
        }
        self.segments.push(Segment {
            raw_start,
            part_start: at,
            len: data.len(),
        });
        self.segments.sort_by_key(|segment| segment.part_start);
    }

    fn part_offset(&self, raw: usize) -> Option<usize> {
        for segment in &self.segments {
            if raw >= segment.raw_start && raw < segment.raw_start + segment.len {
                return Some(segment.part_start + (raw - segment.raw_start));
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

/// Base 32 (digits `0-9A-V`), as used by every `kindle:` URL.
pub fn parse_base32(text: &str) -> Option<u32> {
    let mut value: u64 = 0;
    for ch in text.trim().chars() {
        let digit = match ch {
            '0'..='9' => ch as u64 - '0' as u64,
            'A'..='V' => ch as u64 - 'A' as u64 + 10,
            'a'..='v' => ch as u64 - 'a' as u64 + 10,
            _ => return None,
        };
        value = value.checked_mul(32)?.checked_add(digit)?;
        if value > u32::MAX as u64 {
            return None;
        }
    }
    Some(value as u32)
}

/// Collects `kindle:pos:fid:F:off:O` targets from one part.
fn collect_pos_links(data: &[u8], out: &mut Vec<(u32, u32)>) {
    let text = String::from_utf8_lossy(data);
    let lower = text.to_ascii_lowercase();
    let mut at = 0usize;
    while let Some(relative) = lower[at..].find("kindle:pos:fid:") {
        let start = at + relative + "kindle:pos:fid:".len();
        let rest = &text[start..];
        let Some((fid, rest)) = rest.split_once(":off:") else {
            break;
        };
        let off: String = rest
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric())
            .collect();
        if let (Some(fid), Some(off)) = (parse_base32(fid), parse_base32(&off)) {
            out.push((fid, off));
        }
        at = start;
    }
    out.sort_unstable();
    out.dedup();
}

/// Turns `(fid, off)` link targets into anchor ids, injecting anchors where the
/// target has no id of its own.
fn resolve_targets(
    parts: &mut [Part],
    fragments: &[Fragment],
    targets: &[(u32, u32)],
) -> HashMap<(u32, u32), String> {
    let mut ids = HashMap::new();
    // Group by the part each target lands in.
    // (part offset, raw offset, link key) per part.
    type Target = (usize, usize, (u32, u32));
    let mut per_part: Vec<Vec<Target>> = vec![Vec::new(); parts.len()];
    for &(fid, off) in targets {
        let Some(fragment) = fragments.get(fid as usize) else {
            continue;
        };
        let raw = fragment.insert_pos + off as usize;
        for (part_index, part) in parts.iter().enumerate() {
            if let Some(offset) = part.part_offset(raw) {
                per_part[part_index].push((offset, raw, (fid, off)));
                break;
            }
        }
    }
    for (part_index, mut list) in per_part.into_iter().enumerate() {
        if list.is_empty() {
            continue;
        }
        list.sort_unstable_by_key(|(offset, _, _)| *offset);
        let mut anchors: Vec<AnchorTarget> = Vec::new();
        let mut names: Vec<((u32, u32), String)> = Vec::new();
        for (offset, raw, key) in &list {
            // A tag the offset lands in may already carry an id: use it rather
            // than adding a second `id` attribute (a duplicate would shadow the
            // injected one and the link would go nowhere).
            let own_id = html::start_tag_at(&parts[part_index].bytes, *offset).and_then(|found| {
                found
                    .tag
                    .attribute("id")
                    .or_else(|| found.tag.attribute("name"))
                    .map(str::to_string)
                    .filter(|id| !id.trim().is_empty())
            });
            let id = own_id
                .or_else(|| nearest_anchor(&parts[part_index].bytes, *offset))
                .unwrap_or_else(|| format!("k8-{part_index}-{raw}"));
            let exists = parts[part_index]
                .bytes
                .windows(id.len())
                .any(|window| window == id.as_bytes());
            names.push((*key, id.clone()));
            if !exists {
                anchors.push(AnchorTarget {
                    offset: *offset,
                    id: id.clone(),
                });
            }
        }
        if !anchors.is_empty() {
            let bytes = std::mem::take(&mut parts[part_index].bytes);
            parts[part_index].bytes = html::insert_anchors(&bytes, &anchors);
        }
        for (key, id) in names {
            ids.insert(key, id);
        }
    }
    ids
}

/// The id (or `name`) of the closest start tag before `pos`, if any.
fn nearest_anchor(data: &[u8], pos: usize) -> Option<String> {
    let mut end = pos.min(data.len());
    while end > 0 {
        let gt = data[..end].iter().rposition(|byte| *byte == b'>')?;
        let lt = data[..gt].iter().rposition(|byte| *byte == b'<')?;
        let tag = &data[lt..=gt];
        if tag.starts_with(b"</") {
            end = lt;
            continue;
        }
        if let Some(parsed) = Tag::parse(tag)
            && let Some(id) = parsed.attribute("id").or_else(|| parsed.attribute("name"))
        {
            let id = id.trim();
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
        end = lt;
    }
    None
}

/// Prefixes one part's `id`/`name` attributes and the `href="#…"` links that
/// refer to them, so ids stay unique once all parts are in one document.
fn prefix_part_ids(data: &[u8], part: usize) -> Vec<u8> {
    let prefix = format!("p{part}-");
    html::rewrite_tags(data, |tag| {
        if tag.starts_with(b"</") {
            return tag.to_vec();
        }
        let Some(mut parsed) = Tag::parse(tag) else {
            return tag.to_vec();
        };
        let mut changed = false;
        for name in ["id", "name"] {
            if let Some(value) = parsed.attribute(name).map(str::to_string)
                && !value.trim().is_empty()
            {
                parsed.set_attribute(name, &format!("{prefix}{value}"));
                changed = true;
            }
        }
        if let Some(href) = parsed.attribute("href").map(str::to_string)
            && let Some(fragment) = href.strip_prefix('#')
            && !fragment.is_empty()
        {
            parsed.set_attribute("href", &format!("#{prefix}{fragment}"));
            changed = true;
        }
        // Kindlegen's internal navigation ids: nothing we keep refers to them.
        parsed.remove_attribute("aid");
        parsed.remove_attribute("cid");
        if changed {
            parsed.to_bytes()
        } else {
            tag.to_vec()
        }
    })
}

/// Rewrites the `kindle:` attributes of one tag.
fn rewrite_tag(tag: &[u8], target_ids: &HashMap<(u32, u32), String>) -> Vec<u8> {
    if tag.starts_with(b"</") {
        return tag.to_vec();
    }
    let Some(mut parsed) = Tag::parse(tag) else {
        return tag.to_vec();
    };
    let mut changed = false;
    for name in ["src", "href", "poster", "data", "xlink:href"] {
        let Some(value) = parsed.attribute(name).map(str::to_string) else {
            continue;
        };
        let lower = value.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("kindle:embed:") {
            let number: String = rest
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric())
                .collect();
            if let Some(index) = parse_base32(&number) {
                parsed.set_attribute(name, &format!("mobi-image-{index}"));
                changed = true;
            }
        } else if let Some(rest) = lower.strip_prefix("kindle:pos:fid:") {
            let parts: Vec<&str> = rest.split(":off:").collect();
            if parts.len() == 2 {
                let fid = parse_base32(parts[0]);
                let off: String = parts[1]
                    .chars()
                    .take_while(|ch| ch.is_ascii_alphanumeric())
                    .collect();
                let off = parse_base32(&off);
                if let (Some(fid), Some(off)) = (fid, off) {
                    let target = target_ids
                        .get(&(fid, off))
                        .cloned()
                        .unwrap_or_else(|| format!("k8-{fid}-{off}"));
                    parsed.set_attribute(name, &format!("#{target}"));
                    changed = true;
                }
            }
        }
    }
    if changed {
        parsed.to_bytes()
    } else {
        tag.to_vec()
    }
}

/// Removes `<link>` tags that point at a CSS flow: those stylesheets are inlined
/// in the document head instead.
fn drop_flow_links(data: &[u8]) -> Vec<u8> {
    html::rewrite_tags(data, |tag| {
        let lower = tag.to_ascii_lowercase();
        if lower.starts_with(b"<link") && lower.windows(12).any(|window| window == b"kindle:flow:")
        {
            return Vec::new();
        }
        tag.to_vec()
    })
}

/// Returns the CSS carried by the non-primary flows.
fn flow_styles(flows: &[Vec<u8>]) -> String {
    let mut styles = String::new();
    for flow in flows.iter().skip(1) {
        let text = String::from_utf8_lossy(flow);
        let cleaned = text
            .replace("<![CDATA[", "")
            .replace("]]>", "")
            .trim()
            .to_string();
        if cleaned.is_empty() {
            continue;
        }
        // Style sheets and inline <style> snippets both end up in the head.
        if cleaned.to_ascii_lowercase().contains("<svg") {
            continue;
        }
        styles.push_str(&cleaned);
        styles.push('\n');
    }
    styles
}

fn with_styles(body: &str, styles: &str) -> String {
    if styles.trim().is_empty() {
        return body.to_string();
    }
    format!("<style>{styles}</style>\n{body}")
}

// ---------------------------------------------------------------------------
// TOC, resources and metadata
// ---------------------------------------------------------------------------

/// One NCX entry: either a byte position or a (fragment, offset) pair.
struct NcxEntry {
    pos: Option<u32>,
    text: Option<String>,
    level: usize,
    pos_fid: Option<(u32, u32)>,
}

impl NcxEntry {
    fn pos_fid(&self) -> Option<(u32, u32)> {
        self.pos_fid
    }
}

/// Reads the NCX index (`ncx.py` in calibre: 1 = pos, 3 = text, 4 = level,
/// 6 = (fid, off)).
fn ncx_entries(pdb: &Pdb, header: &MobiHeader) -> Vec<NcxEntry> {
    if header.ncx_index == NULL_INDEX {
        return Vec::new();
    }
    let Ok(index) = read_index(pdb, header.ncx_index as usize, header.encoding) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for entry in &index.entries {
        let text = entry
            .tag(3)
            .and_then(|offset| index.string(offset))
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        entries.push(NcxEntry {
            pos: entry.tag(1),
            text,
            level: entry.tag(4).unwrap_or(0) as usize,
            pos_fid: entry.tag_pair(6),
        });
    }
    entries
}

fn kf8_toc(
    pdb: &Pdb,
    header: &MobiHeader,
    target_ids: &HashMap<(u32, u32), String>,
    parts: &[Part],
) -> Vec<Kf8TocItem> {
    let mut toc = Vec::new();
    for entry in ncx_entries(pdb, header) {
        let Some(title) = entry.text else {
            continue;
        };
        let id = match entry.pos_fid {
            Some(key) => target_ids.get(&key).cloned(),
            // Some NCX entries carry a plain byte position instead of a
            // fragment reference: use the closest anchor in that part.
            None => entry.pos.and_then(|pos| {
                let pos = pos as usize;
                parts.iter().find_map(|part| {
                    part.part_offset(pos)
                        .and_then(|offset| nearest_anchor(&part.bytes, offset))
                })
            }),
        };
        let Some(id) = id else {
            continue;
        };
        toc.push(Kf8TocItem {
            title,
            href: format!("#{id}"),
            depth: entry.level,
        });
    }
    toc
}

fn build_resources(pdb: &Pdb, header: &MobiHeader) -> (HashMap<String, Vec<u8>>, Option<Vec<u8>>) {
    let mut resources = HashMap::new();
    let mut cover = None;
    let start = if header.first_image_index == NULL_INDEX || header.first_image_index == 0 {
        header.text_record_count + 1
    } else {
        header.first_image_index as usize
    };
    if start >= pdb.records() {
        return (resources, cover);
    }
    let cover_offset = header.exth.uint(201);
    let mut index = 0usize;
    for record_index in start..pdb.records() {
        let Some(data) = pdb.record(record_index) else {
            break;
        };
        index += 1;
        if is_known_non_image_record(data) || image_extension(data).is_none() {
            continue;
        }
        if cover_offset == Some((index - 1) as u32) {
            cover = Some(data.to_vec());
        }
        resources.insert(format!("mobi-image-{index}"), data.to_vec());
    }
    (resources, cover)
}

/// Returns the inside of the document's `<body>`, when the part has one.
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

fn section_toc(body: &str) -> Vec<Kf8TocItem> {
    let mut toc = Vec::new();
    for (index, section) in body.split("<section ").skip(1).enumerate() {
        let content = match section.find('>') {
            Some(end) => &section[end + 1..],
            None => section,
        };
        let title = heading_of(content).unwrap_or_else(|| {
            let text = strip_tags(content);
            let text = text.trim();
            text.chars().take(40).collect()
        });
        if title.trim().is_empty() {
            continue;
        }
        toc.push(Kf8TocItem {
            title: title.trim().to_string(),
            href: format!("#cre-sec-{}", index + 1),
            depth: 0,
        });
    }
    toc
}

fn heading_of(section: &str) -> Option<String> {
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
    None
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

    #[test]
    fn base32_matches_kindle_urls() {
        assert_eq!(parse_base32("0001"), Some(1));
        assert_eq!(parse_base32("000A"), Some(10));
        assert_eq!(parse_base32("000V"), Some(31));
        assert_eq!(parse_base32("0010"), Some(32));
        assert_eq!(parse_base32("zz"), None);
    }

    #[test]
    fn pos_links_are_collected() {
        let data = br#"<a href="kindle:pos:fid:0001:off:0000000010">x</a>"#;
        let mut out = Vec::new();
        collect_pos_links(data, &mut out);
        assert_eq!(out, vec![(1, 32)]);
    }

    #[test]
    fn embed_and_flow_urls_are_rewritten() {
        let out = rewrite_tag(
            br#"<img src="kindle:embed:0002?mime=image/jpeg"/>"#,
            &HashMap::new(),
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("src=\"mobi-image-2\""), "{text}");

        let mut ids = HashMap::new();
        ids.insert((1u32, 0u32), "chapter2".to_string());
        let out = rewrite_tag(
            br#"<a href="kindle:pos:fid:0001:off:0000000000">go</a>"#,
            &ids,
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("href=\"#chapter2\""), "{text}");
    }

    #[test]
    fn part_assembly_maps_raw_offsets() {
        let skeleton = b"<p></p>";
        let mut part = Part::new(skeleton, 0);
        part.insert(3, b"hello", 10);
        assert_eq!(part.bytes, b"<p>hello</p>".to_vec());
        // Raw offset 10 is the start of the fragment.
        assert_eq!(part.part_offset(10), Some(3));
        assert_eq!(part.part_offset(0), Some(0));
    }
}

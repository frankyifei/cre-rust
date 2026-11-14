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

use cre_book::html::{escape_html, extract_body, heading_of, strip_tags};
use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use cre_pdb::html::{self, AnchorTarget, Tag};
use cre_pdb::{
    BookResources, Index, MobiHeader, NULL_INDEX, Pdb, ResourceRange, extract_text,
    image_extension, is_font_record, is_known_non_image_record, read_index,
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
    pub line_height: f64,
    pub margin_x: u32,
    pub margin_y: u32,
}

impl Default for Kf8HtmlOptions {
    fn default() -> Self {
        Self {
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
    /// EXTH 498: the series the book belongs to, when the publisher set it.
    pub series: Option<String>,
    /// Resource name (`mobi-image-N`) to its range in the PalmDB file.
    ///
    /// Images and embedded fonts share the naming: the index is the one a
    /// `kindle:embed:N` URL uses, so the name is kept even for a face the
    /// stylesheet references.
    ///
    /// The bytes are sliced from the mapped file when a page paints them, so a
    /// 25 MB comic costs no anonymous memory for its pictures.
    pub resources: BookResources,
    /// The book's stylesheets, one per non-primary flow, under the name the
    /// generated head links them by.
    pub stylesheets: Vec<(String, Vec<u8>)>,
    pub toc: Vec<Kf8TocItem>,
    pub cover_image: Option<String>,
    /// Number of XHTML parts (spine sections) in the book.
    pub section_count: usize,
    body: String,
    /// Attributes of the book's own `<body>` tag, with the leading space.
    body_attrs: String,
}

impl Kf8Book {
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

        // 4. Rewrite the `kindle:` URLs and anchor links in each part. The
        //    stylesheet flows stay separate resources: a part's own `<head>`
        //    is dropped, so the rebuilt head links them instead.
        let stylesheets = flow_stylesheets(&flows);
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
        // The book's `<body>` attributes (`class="calibre"` and friends) are
        // kept on the generated body so the stylesheet's `body.class` rules
        // keep matching; the parts themselves are flattened into sections.
        let mut body_attrs = String::new();
        for part in &parts {
            let inner = extract_body(&part.bytes).unwrap_or_else(|| part.bytes.clone());
            let text = String::from_utf8_lossy(&inner);
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            if body_attrs.is_empty() {
                body_attrs = body_tag_attrs(&part.bytes);
            }
            section_count += 1;
            // A pre-paginated part declares its initial containing block with
            // `<meta name="viewport">`. cre-lib reads the marker and treats the
            // section as one fixed page instead of reflowing its text.
            let fixed = part_viewport(&part.bytes)
                .map(|viewport| format!(" data-cre-fixed-layout=\"{viewport}\""))
                .unwrap_or_default();
            body.push_str(&format!(
                "<section class=\"cre-spine\" data-spine-index=\"{}\" \
                 id=\"cre-sec-{section_count}\"{fixed}>",
                // Zero-based, like the EPUB generator: cre-lib adds one to get
                // the 1-based `DocFragment[n]` crengine uses, so the first part
                // must be `DocFragment[1]`.
                section_count - 1
            ));
            body.push_str(trimmed);
            body.push_str("</section>\n");
        }
        if section_count == 0 {
            bail!("KF8 produced no text");
        }

        // 6. Resources and cover: ranges into the PalmDB, not copies.
        let (resource_ranges, cover_image) = build_resources(&pdb, &header);

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
        let series = header.exth.string(498, encoding);

        // 8. Table of contents from the NCX, else from the part headings.
        let mut toc = kf8_toc(&pdb, &header, &target_ids, &parts);
        if toc.is_empty() {
            toc = section_toc(&body);
        }

        // 9. Keep the file bytes and the record ranges, so a picture is sliced
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
            stylesheets,
            toc,
            cover_image,
            section_count,
            body: rewrite_kindle_urls(&body),
            body_attrs,
        })
    }

    pub fn body_html(&self) -> &str {
        &self.body
    }

    pub fn to_html(&self, options: &Kf8HtmlOptions) -> String {
        self.to_html_with_toc(options).0
    }

    pub fn to_html_with_toc(&self, options: &Kf8HtmlOptions) -> (String, Vec<Kf8TocItem>) {
        let html = format!("{}{}</body></html>", self.html_head(options), self.body);
        (html, self.toc.clone())
    }

    /// The document prefix through the `<body>` open tag.
    ///
    /// [`Self::body_html`] + this + `"</body></html>"` is exactly
    /// [`Self::to_html_with_toc`], which lets a caller build one chunk at a
    /// time instead of materializing the whole generated document.
    pub fn html_head(&self, options: &Kf8HtmlOptions) -> String {
        let css = base_css(options);
        let title = escape_html(self.title.as_deref().unwrap_or(""));
        let language = escape_html(self.language.as_deref().unwrap_or("und"));
        // The book's own sheets come after the base CSS so its rules win, the
        // same order an EPUB keeps.
        let links: String = self
            .stylesheets
            .iter()
            .map(|(name, _)| format!("<link rel=\"stylesheet\" href=\"{name}\">"))
            .collect();
        format!(
            "<!DOCTYPE html><html lang=\"{language}\"><head><meta charset=\"utf-8\">\
             <title>{title}</title><style>{css}</style>{links}</head><body{}>",
            self.body_attrs
        )
    }
}

fn base_css(options: &Kf8HtmlOptions) -> String {
    format!(
        "html, body {{ margin: 0; padding: 0; }}\n\
         html {{ font-size: {}px; }}\n\
         body {{ font-family: serif; font-size: 1em; line-height: {}; padding: {}px {}px; }}\n\
         h1, h2, h3, h4, h5, h6 {{ font-family: sans-serif; }}\n\
         .cre-spine {{ display: block; page-break-before: always; }}",
        cre_book::css::FONT_SIZE,
        options.line_height,
        options.margin_y,
        options.margin_x
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

/// The book's stylesheets: one resource per non-primary flow, named by its
/// flow index (the index a `kindle:flow:N` link uses).
fn flow_stylesheets(flows: &[Vec<u8>]) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for (index, flow) in flows.iter().enumerate().skip(1) {
        let text = String::from_utf8_lossy(flow)
            .replace("<![CDATA[", "")
            .replace("]]>", "");
        let trimmed = text.trim();
        if trimmed.is_empty() || trimmed.to_ascii_lowercase().contains("<svg") {
            continue;
        }
        out.push((
            format!("cre-flow-{index}.css"),
            rewrite_kindle_urls(trimmed).into_bytes(),
        ));
    }
    out
}

/// Rewrites `url(kindle:embed:N…),` inside the book's CSS.
///
/// Kindlegen addresses images and fonts from stylesheets with
/// `url(kindle:embed:XXXX?mime=…)`, which the resource provider cannot resolve:
/// a cover declared as `background-image: url(kindle:embed:0002?mime=image/jpg)`
/// stayed blank. `rewrite_tag` already maps the same form in `src`/`href`
/// attributes to `mobi-image-N`; do the same for stylesheet URLs and drop the
/// `?mime=` query.
fn rewrite_kindle_urls(html: &str) -> String {
    if !contains_kindle_url(html) {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = rest.find("kindle:embed:") {
        // Only URLs are rewritten; the text around them is copied as it is.
        let before = &rest[..pos];
        let Some(open) = before.rfind("url(") else {
            out.push_str(&rest[..pos + "kindle:embed:".len()]);
            rest = &rest[pos + "kindle:embed:".len()..];
            continue;
        };
        let inner = &before[open + 4..];
        if !inner.trim().is_empty() || before[open + 4..].contains(')') {
            out.push_str(&rest[..pos + "kindle:embed:".len()]);
            rest = &rest[pos + "kindle:embed:".len()..];
            continue;
        }
        out.push_str(before);
        let tail = &rest[pos + "kindle:embed:".len()..];
        let number: String = tail
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric())
            .collect();
        let Some(index) = parse_base32(&number) else {
            out.push_str("kindle:embed:");
            rest = tail;
            continue;
        };
        out.push_str(&format!("mobi-image-{index}"));
        // Skip `?mime=…` up to the closing parenthesis.
        let after = tail[number.len()..].trim_start();
        let after = after.strip_prefix('?').map_or(after, |rest| {
            rest.find(')').map_or(rest, |close| &rest[close..])
        });
        rest = after;
    }
    out.push_str(rest);
    out
}

fn contains_kindle_url(html: &str) -> bool {
    html.as_bytes()
        .windows("kindle:embed:".len())
        .any(|window| window.eq_ignore_ascii_case(b"kindle:embed:"))
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

fn build_resources(pdb: &Pdb, header: &MobiHeader) -> (Vec<ResourceRange>, Option<String>) {
    let mut resources = Vec::new();
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
        // Index numbering follows the Kindle's own: `kindle:embed:N` counts
        // every record from the first image one, including those the reader
        // cannot use.
        if resource_name(index, data).is_none() {
            continue;
        }
        let Some(range) = pdb.record_range(record_index) else {
            break;
        };
        if cover_offset == Some((index - 1) as u32) {
            // Only the name: the reader asks the resource table for the bytes.
            cover = Some(format!("mobi-image-{index}"));
        }
        resources.push((format!("mobi-image-{index}"), range));
    }
    (resources, cover)
}

/// Resource name for the record at `index`, when the reader may fetch it.
///
/// Fonts count: a KF8 stylesheet addresses an embedded face with
/// `url(kindle:embed:N?mime=application/x-font-ttf)`, and `rewrite_kindle_urls`
/// maps that to the same `mobi-image-N` name an image gets.
fn resource_name(index: usize, data: &[u8]) -> Option<String> {
    if is_known_non_image_record(data) {
        return None;
    }
    if image_extension(data).is_none() && !is_font_record(data) {
        return None;
    }
    Some(format!("mobi-image-{index}"))
}

/// The `<body>` tag's attributes of one part, with a leading space.
///
/// The generated document has a body of its own, so the book's `body class="…"`
/// would otherwise be lost and every `body.class` rule in its stylesheet would
/// stop matching.
fn body_tag_attrs(part: &[u8]) -> String {
    let lower = part.to_ascii_lowercase();
    let Some(at) = lower.windows(5).position(|window| window == b"<body") else {
        return String::new();
    };
    let Some(end) = lower[at..].iter().position(|byte| *byte == b'>') else {
        return String::new();
    };
    // `<body` itself is 5 bytes; keep everything up to the closing `>`.
    String::from_utf8_lossy(&part[at + 5..at + end]).into_owned()
}

/// The fixed page size a part declares, in the marker cre-lib reads.
///
/// A pre-paginated part carries `<meta name="viewport" content="width=600,
/// height=800">` (or `width=device-width`). Returns `"600x800"` / `"device"` /
/// `None`, i.e. the value of `data-cre-fixed-layout`.
fn part_viewport(part: &[u8]) -> Option<String> {
    let lower = part.to_ascii_lowercase();
    let mut at = 0usize;
    while let Some(found) = find_bytes(&lower[at..], b"<meta") {
        let start = at + found;
        let offset = lower[start..].iter().position(|byte| *byte == b'>')?;
        let end = start + offset;
        if let Some(tag) = html::Tag::parse(&part[start..=end])
            && tag
                .attribute("name")
                .is_some_and(|name| name.eq_ignore_ascii_case("viewport"))
            && let Some(content) = tag.attribute("content")
        {
            return viewport_marker(content);
        }
        at = end + 1;
    }
    None
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// `width=600, height=800` -> `600x800`; `width=device-width` -> `device`.
fn viewport_marker(content: &str) -> Option<String> {
    let mut width = None;
    let mut height = None;
    for part in content.split(',') {
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "width" if value.eq_ignore_ascii_case("device-width") => return Some("device".into()),
            "width" => width = value.parse::<f32>().ok(),
            "height" => height = value.parse::<f32>().ok(),
            _ => {}
        }
    }
    match (width, height) {
        (Some(width), Some(height)) if width > 0.0 && height > 0.0 => {
            Some(format!("{width:.0}x{height:.0}"))
        }
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_flows_become_named_resources() {
        let flows = vec![
            b"main text".to_vec(),
            b"p { color: red }".to_vec(),
            b"<svg></svg>".to_vec(),
            b"<![CDATA[ body { margin: 0 } ]]>".to_vec(),
        ];
        let sheets = flow_stylesheets(&flows);
        assert_eq!(sheets.len(), 2, "{sheets:?}");
        assert_eq!(sheets[0].0, "cre-flow-1.css");
        assert!(String::from_utf8_lossy(&sheets[0].1).contains("color: red"));
        // CDATA wrappers are stripped and the flow keeps its own index.
        assert_eq!(sheets[1].0, "cre-flow-3.css");
        assert!(String::from_utf8_lossy(&sheets[1].1).contains("margin: 0"));
    }

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
    fn stylesheet_embed_urls_are_rewritten() {
        let css = ".cover { background-image: url(kindle:embed:0002?mime=image/jpg); }\n\
                   .plain { background: url(../Images/bg.png); }";
        let out = rewrite_kindle_urls(css);
        assert!(out.contains("url(mobi-image-2)"), "{out}");
        // Unrelated URLs and the surrounding CSS are untouched.
        assert!(out.contains("url(../Images/bg.png)"), "{out}");
        assert!(out.contains(".cover { background-image:"), "{out}");
        assert_eq!(rewrite_kindle_urls("p { margin: 0 }"), "p { margin: 0 }");
    }

    #[test]
    fn a_font_face_url_points_at_the_same_resource_name() {
        // The stylesheet side and the resource index have to agree: the font
        // record is published under the name the rewritten URL asks for.
        let css = "@font-face { src: url(kindle:embed:0005?mime=application/x-font-ttf); \
                   font-family: \"Book\"; }";
        let out = rewrite_kindle_urls(css);
        assert!(out.contains("url(mobi-image-5)"), "{out}");
        assert!(resource_name(5, b"OTTO\x00\x0a\x00\x80").is_some());
    }

    #[test]
    fn resource_names_cover_images_and_fonts() {
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        assert_eq!(resource_name(3, &jpeg).as_deref(), Some("mobi-image-3"));
        // Fonts are ordinary resource records in KF8; the reader has to be able
        // to fetch them or `@font-face` cannot resolve.
        let ttf = [0x00, 0x01, 0x00, 0x00, 0x00, 0x0A, 0x00, 0x80];
        assert_eq!(resource_name(4, &ttf).as_deref(), Some("mobi-image-4"));
        assert_eq!(
            resource_name(5, b"ttcf\x00\x01\x00\x00").as_deref(),
            Some("mobi-image-5")
        );
        // Record types that live in the same area and are not fetchable.
        assert_eq!(resource_name(6, b"FLIS\x00\x00\x00\x08"), None);
        assert_eq!(resource_name(7, b"<html>\n<body>"), None);
    }

    #[test]
    fn a_viewport_meta_becomes_the_fixed_layout_marker() {
        assert_eq!(
            viewport_marker("width=600, height=800").as_deref(),
            Some("600x800")
        );
        assert_eq!(
            viewport_marker("width=device-width, initial-scale=1").as_deref(),
            Some("device")
        );
        // Half a viewport is not a page size.
        assert_eq!(viewport_marker("width=600"), None);
        assert_eq!(viewport_marker("initial-scale=1"), None);

        let fixed = br#"<html><head><meta name="viewport" content="width=1024, height=768"/>
                       </head><body><p>x</p></body></html>"#;
        assert_eq!(part_viewport(fixed).as_deref(), Some("1024x768"));
        // A reflowable part has no viewport; the body is laid out as text.
        let reflowable = br#"<html><head><title>t</title></head><body><p>x</p></body></html>"#;
        assert_eq!(part_viewport(reflowable), None);
    }

    #[test]
    fn the_body_attributes_survive_the_head_rewrite() {
        // `body.class` rules in the book's stylesheet have to keep matching.
        let part = br#"<html><head><title>t</title></head>
                       <body class="calibre" aid="0"><p>x</p></body></html>"#;
        assert_eq!(body_tag_attrs(part), " class=\"calibre\" aid=\"0\"");
        assert_eq!(body_tag_attrs(b"<p>no body here</p>"), "");
    }

    #[test]
    fn base_css_puts_the_reader_size_on_the_root() {
        // A book that sets `body { font-size: 97% }` resolves the percentage
        // against the root, so the reader size has to live on `html` or the
        // setting does nothing. The value itself is a token: it is filled in
        // when the chunk is laid out, so a font change is a relayout.
        let css = base_css(&Kf8HtmlOptions::default());
        assert!(
            css.contains(&format!(
                "html {{ font-size: {}px; }}",
                cre_book::css::FONT_SIZE
            )),
            "{css}"
        );
        assert!(
            css.contains("body { font-family: serif; font-size: 1em;"),
            "{css}"
        );
        assert_eq!(
            cre_book::css::substitute(&css, 26.0, 10.0).as_ref(),
            css.replace(cre_book::css::FONT_SIZE, "26.00")
        );
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

//! Image references in generated HTML, and the size estimates layout needs
//! before anything is decoded.

use std::collections::HashMap;
use std::sync::Arc;

use percent_encoding::percent_decode_str;

use crate::resources::ResourceSource;

#[derive(Clone, Debug)]
pub struct PageFitImage {
    /// Original image resource, painted at draw time.
    pub source: String,
    pub width: u32,
    pub height: u32,
    /// False when the header could not be read: the picture then fills the
    /// page box instead of being contained.
    pub contain: bool,
}

use crate::html::{
    attr_value, base64_encode, contains_ignore_ascii_case, file_extension, find_matching_close_tag,
    find_tag_end, is_external, next_tag_start, normalize_path, parse_attrs, parse_pixels,
    replace_attribute_value, text_without_tags,
};

/// Intrinsic pixel size of a raster image, read from its header.
///
/// The pixels are not decoded: a large photo's header alone gives the layout
/// size.
pub fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Estimated decoded size of one image, for the chunk memory budget.
pub fn estimated_image_bytes(name: &str, bytes: &[u8]) -> usize {
    if let Some((width, height)) = image_dimensions(bytes) {
        return (width * height * 4) as usize;
    }
    estimated_decoded_from_size(name, bytes.len())
}

/// True when the bytes look like a picture the estimator understands.
pub fn is_image_bytes(bytes: &[u8]) -> bool {
    image_dimensions(bytes).is_some() || bytes.starts_with(b"<svg")
}

/// Decoded size from the file size and the picture's format, for bytes whose
/// header could not be read (a compressed entry, an unknown magic).
pub fn estimated_decoded_from_size(name: &str, file_bytes: usize) -> usize {
    let factor = if name.ends_with(".jpg") || name.ends_with(".jpeg") {
        32
    } else if name.ends_with(".png") || name.ends_with(".gif") {
        4
    } else if name.ends_with(".webp") {
        12
    } else if name.ends_with(".svg") {
        return 2 * 1024 * 1024;
    } else {
        8
    };
    file_bytes.saturating_mul(factor)
}

/// Resource names referenced by the `<img>` tags of a fragment.
pub fn image_sources_in(body: &str) -> Vec<String> {
    image_sources(body, "")
}

pub fn image_sources(body: &str, doc_dir: &str) -> Vec<String> {
    // ASCII lowercasing keeps byte offsets, so positions found in `lower` can
    // be used to slice the original text (zip names are case-sensitive).
    let lower = body.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut index = 0usize;
    while let Some(found) = lower[index..].find("<img") {
        let start = index + found;
        let Some(end) = lower[start..].find('>').map(|offset| start + offset) else {
            break;
        };
        if let Some(src_at) = lower[start..end].find("src=") {
            let rest = &body[start + src_at + 4..end];
            let rest = rest.trim_start();
            if let Some(quote @ ('"' | '\'')) = rest.chars().next()
                && let Some(close) = rest[1..].find(quote)
            {
                let value = rest[1..1 + close].split(['#', '?']).next().unwrap_or("");
                // Percent-encoded names must be decoded: that is the key the
                // provider is asked for when the picture is fetched.
                let decoded = percent_decode_str(value).decode_utf8_lossy();
                if let Some(relative) = decoded.strip_prefix("cre://book/") {
                    out.push(normalize_path(relative));
                } else if !decoded.is_empty()
                    && !decoded.starts_with("data:")
                    && !decoded.contains("://")
                {
                    out.push(normalize_path(&format!("{doc_dir}{decoded}")));
                }
            }
        }
        index = end + 1;
    }
    out
}

pub fn image_mime(data: &[u8], path: &str) -> &'static str {
    if data.starts_with(&[0x89, 0x50, 0x4e, 0x47]) {
        return "image/png";
    }
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        return "image/jpeg";
    }
    if data.starts_with(b"GIF8") {
        return "image/gif";
    }
    if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return "image/webp";
    }
    if data.starts_with(b"BM") {
        return "image/bmp";
    }
    match file_extension(path).as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" | "svgz" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// Rewrites the images inside inline SVG blocks to `data:` URIs.
///
/// Blitz parses an inline `<svg>` with usvg's default href resolver, which only
/// understands `data:` URIs; a relative `xlink:href` makes the image node
/// unreadable and the whole SVG is dropped (fixed-layout pages render blank,
/// including any text the SVG draws). Passing the bytes inline lets usvg build
/// the tree, so the image, the text and the link hotspots all survive.
///
/// The rewrite is for the HTML handed to the parser only: storing it would
/// keep a base64 copy of every page image for the whole session.
pub fn inline_svg_images_to_data_uris(
    html: &str,
    fetch: &dyn Fn(&str) -> Option<std::sync::Arc<Vec<u8>>>,
) -> Option<String> {
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    let mut changed = false;
    while let Some(open_start) = next_tag_start(html, cursor, "<svg") {
        let Some((_close_start, close_end)) =
            find_matching_close_tag(html, open_start, "<svg", "</svg")
        else {
            break;
        };
        out.push_str(&html[cursor..open_start]);
        let block = &html[open_start..close_end];
        let (rewritten, block_changed) = svg_block_inline_images(block, fetch);
        changed |= block_changed;
        out.push_str(&rewritten);
        cursor = close_end;
    }
    if !changed {
        return None;
    }
    out.push_str(&html[cursor..]);
    Some(out)
}

/// Replaces the href of every `<image>` in one SVG block with a data URI.
pub fn svg_block_inline_images(
    block: &str,
    fetch: &dyn Fn(&str) -> Option<std::sync::Arc<Vec<u8>>>,
) -> (String, bool) {
    let mut out = String::with_capacity(block.len());
    let mut cursor = 0usize;
    let mut changed = false;
    while let Some(start) = next_tag_start(block, cursor, "<image") {
        let Some(end) = find_tag_end(block, start) else {
            break;
        };
        out.push_str(&block[cursor..start]);
        let tag = &block[start..end];
        let mut replaced = tag.to_string();
        let attrs = parse_attrs(tag);
        if let Some(href) = attrs.get("xlink:href").or_else(|| attrs.get("href")) {
            let href = href.trim();
            let book_url = href.starts_with("cre://book/");
            if !href.is_empty() && !href.starts_with("data:") && (book_url || !is_external(href)) {
                let without_fragment = href.split('#').next().unwrap_or(href);
                let relative = without_fragment
                    .strip_prefix("cre://book/")
                    .unwrap_or(without_fragment);
                let key = normalize_path(&percent_decode_str(relative).decode_utf8_lossy());
                if let Some(bytes) = fetch(&key) {
                    let mime = image_mime(&bytes, &key);
                    let encoded = base64_encode(&bytes);
                    let data_uri = format!("data:{mime};base64,{encoded}");
                    replaced = replace_attribute_value(tag, "xlink:href", &data_uri)
                        .or_else(|| replace_attribute_value(tag, "href", &data_uri))
                        .unwrap_or(replaced);
                    changed = true;
                }
            }
        }
        out.push_str(&replaced);
        cursor = end;
    }
    out.push_str(&block[cursor..]);
    (out, changed)
}

/// The page-sized transparent SVG served for a `cre-page-fit-N` key.
///
/// The same box `page_picture_svg` declares, minus the picture: the pixels are
/// painted by [`collect_placed_images`] from the resource the wrapper names, so
/// the picture is never copied into the chunk HTML (an inlined plate is a
/// base64 copy per page, and a comic pays for it in peak memory).
///
/// The `width`/`height` attributes matter: the wrapper is referenced from an
/// `<img>`, so the document's layout needs an intrinsic size. Without them the
/// element collapses to the CSS default box and the picture sits in a corner.
pub fn page_fit_svg(width: f32, height: f32) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width:.0}\" height=\"{height:.0}\" \
         viewBox=\"0 0 {width:.0} {height:.0}\"></svg>"
    )
}

/// The page-sized SVG that shows `picture` at page size without cropping.
pub fn page_picture_svg(picture: &str, width: f32, height: f32, block: bool) -> String {
    let display = if block { "display:block;" } else { "" };
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {width:.0} {height:.0}\" \
         preserveAspectRatio=\"xMidYMid meet\" style=\"{display}width:100%;height:auto\">\
         <image x=\"0\" y=\"0\" width=\"{width:.0}\" height=\"{height:.0}\" \
         preserveAspectRatio=\"xMidYMid meet\" href=\"{picture}\"/></svg>"
    )
}

/// The `src` of the single image `body` holds, when that is all it holds.
pub fn picture_only_body(body: &str, src: &regex::Regex) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    if lower.matches("<img").count() != 1 {
        return None;
    }
    let start = lower.find("<img")?;
    let end = body[start..].find('>').map(|offset| start + offset + 1)?;
    let captures = src.captures(&body[start..end])?;
    let picture = captures
        .get(1)
        .or_else(|| captures.get(2))?
        .as_str()
        .trim()
        .to_string();
    if picture.is_empty() {
        return None;
    }
    // Anything left after removing the image (tags excluded) is text, so the
    // section is a normal page with a picture in it.
    let rest = format!("{}{}", &body[..start], &body[end..]);
    text_without_tags(&rest)
        .trim()
        .is_empty()
        .then_some(picture)
}

/// `id`/`name` values carried by the tags inside one part.
///
/// They are the book's navigation targets; the plate rewrite replaces the markup
/// around them, so they are re-emitted as empty anchors (an empty `<a>` has no
/// box, and `page_for_node` falls through to the content that follows it).
pub fn section_anchor_ids(section: &str) -> Vec<String> {
    static ATTRS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let attrs = ATTRS.get_or_init(|| {
        regex::Regex::new(r#"(?is)\b(?:id|name)\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
            .expect("anchor attribute regex is valid")
    });
    let mut out: Vec<String> = Vec::new();
    for captures in attrs.captures_iter(section) {
        let Some(value) = captures.get(1).or_else(|| captures.get(2)) else {
            continue;
        };
        let value = value.as_str().trim();
        if value.is_empty() || out.iter().any(|existing| existing == value) {
            continue;
        }
        out.push(value.to_string());
    }
    out
}

/// Bytes of a `data:` URI, when `value` is one.
///
/// A document can inline its pictures instead of shipping files; the painter
/// still needs bytes under a key, so the plate pass decodes them once and puts
/// them in the generated table.
pub fn data_uri_bytes(value: &str) -> Option<Arc<Vec<u8>>> {
    use base64::Engine;
    let rest = value.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    if meta
        .split(';')
        .any(|part| part.eq_ignore_ascii_case("base64"))
    {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(payload.trim())
            .ok()?;
        return Some(Arc::new(bytes));
    }
    let decoded = percent_decode_str(payload).decode_utf8_lossy().into_owned();
    Some(Arc::new(decoded.into_bytes()))
}

/// Gives the book's cover image a page of its own.
///
/// MOBI and KF8 put the cover at the top of the first chapter section, inline
/// with the title text; `cre-mobi`/`cre-kf8` mark that `<img>` with
/// `data-cre-cover` (it is the EXTH cover record). A page-tall inline image
/// makes its line box taller than the page, which pushed the whole first
/// paragraph onto the second page and left page one blank, and its
/// `max-height` cap kept it smaller than the screen. Replacing it with a
/// block-level page-sized SVG makes it fill the page exactly and moves the
/// following text to the next page, like a cover should.
pub fn fit_cover_images(html: &str, width: f32, height: f32) -> String {
    // The cover is the first picture of the first part, with nothing before it:
    // MOBI/KF8 writers put it at the top of chapter one, inline with the title.
    // It has to be a tall picture (its `height` attribute is the only size we
    // have at this point), otherwise a publisher logo on a title page would
    // become a page of its own.
    if !contains_ignore_ascii_case(html, "<img") {
        return html.to_string();
    }
    let lower = html.to_ascii_lowercase();
    // The stylesheet also mentions `.cre-spine`, so look at the tags.
    let mut scan = 0usize;
    let mut open_end = None;
    while let Some(pos) = lower[scan..].find("<section") {
        let start = scan + pos;
        let Some(end) = html[start..].find('>').map(|offset| start + offset + 1) else {
            break;
        };
        if html[start..end].to_ascii_lowercase().contains("cre-spine") {
            open_end = Some(end);
            break;
        }
        scan = end;
    }
    let Some(open_end) = open_end else {
        return html.to_string();
    };
    let Some(picture_start) = lower[open_end..].find("<img").map(|pos| open_end + pos) else {
        return html.to_string();
    };
    let Some(picture_end) = html[picture_start..]
        .find('>')
        .map(|end| picture_start + end + 1)
    else {
        return html.to_string();
    };
    // Anything but whitespace before the picture means this is a normal page
    // that happens to start with an image.
    let before = text_without_tags(&html[open_end..picture_start]);
    if !before.trim().is_empty() {
        return html.to_string();
    }
    let tag = &html[picture_start..picture_end];
    let Some(src) = attr_value(tag, "src") else {
        return html.to_string();
    };
    // A picture this module already turned into a page-sized plate must keep
    // its wrapper (wrapping it again inlines the picture).
    if src.starts_with("cre-page-fit-") {
        return html.to_string();
    }
    let Some(declared_height) = attr_value(tag, "height").and_then(parse_pixels) else {
        return html.to_string();
    };
    if declared_height < height * 0.5 {
        return html.to_string();
    }
    let (width, height) = (width.max(1.0).round(), height.max(1.0).round());
    let mut out = String::with_capacity(html.len() + 256);
    out.push_str(&html[..picture_start]);
    out.push_str(&page_picture_svg(&src, width, height, true));
    out.push_str(&html[picture_end..]);
    out
}

/// Turns a book's picture-only parts into page-sized plates that point at the
/// image instead of inlining it.
///
/// This is the counterpart of `cre_epub::normalize_full_page_images` and works
/// for any source that can answer `get(key)` — a PalmDB resource map, a
/// standalone document's directory, or a `data:` picture already in the markup.
/// A part that is one picture gets a `cre-page-fit-N` wrapper (written into the
/// source's generated table), and its content becomes a single
/// `<img src="cre-page-fit-N">`, so layout sees a page-sized box and painting
/// reads the original bytes through the same table.
pub fn normalize_page_images(
    body: String,
    resources: &ResourceSource,
    available: (f32, f32),
    page_fit: &mut HashMap<String, PageFitImage>,
) -> String {
    static SRC: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let src = SRC.get_or_init(|| {
        regex::Regex::new(r#"(?is)\bsrc\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
            .expect("img src regex is valid")
    });
    let (width, height) = (available.0.max(1.0).round(), available.1.max(1.0).round());
    let closing = "</section>";
    let lower = body.to_ascii_lowercase();
    let mut out = String::with_capacity(body.len());
    let mut cursor = 0usize;
    let mut scan = 0usize;
    let mut counter = 0usize;
    while let Some(pos) = lower[scan..].find("<section") {
        let start = scan + pos;
        let Some(open_end) = body[start..].find('>').map(|end| start + end + 1) else {
            break;
        };
        if !lower[start..open_end].contains("cre-spine") {
            scan = open_end;
            continue;
        }
        let Some(close) = lower[open_end..].find(closing).map(|end| open_end + end) else {
            break;
        };
        let end = close + closing.len();
        scan = end;
        let Some(picture) = picture_only_body(&body[open_end..close], src) else {
            continue;
        };
        if picture.starts_with("cre-page-fit-") {
            continue;
        }
        // A `data:` picture is already in the markup; decode it so the painter
        // can read it back from the generated table like any other resource.
        let (key, bytes) = match data_uri_bytes(&picture) {
            Some(bytes) => {
                let key = format!("cre-data-image-{counter}");
                resources.insert(key.clone(), bytes.clone());
                (key, bytes)
            }
            None => {
                let key = normalize_path(&picture);
                let Some(bytes) = resources.get(&key) else {
                    continue;
                };
                (key, bytes)
            }
        };
        let Some((image_width, image_height)) = image_dimensions(&bytes) else {
            continue;
        };
        // Replace the part's content, but keep the anchors it carried: a KF8
        // part keeps its TOC anchor on the wrapping `<div>` (`id="k8-42-1337"`),
        // and that same element carries the book's own page-box CSS
        // (`div.fs { width:1072px; height:1448px; position:relative }`), which
        // sized the plate to the book's page instead of the reader's. Empty
        // `<a>` elements keep the navigation target without the box.
        let section = &body[open_end..close];
        let name = format!("cre-page-fit-{counter}");
        counter += 1;
        page_fit.insert(
            name.clone(),
            PageFitImage {
                source: key,
                width: image_width,
                height: image_height,
                contain: true,
            },
        );
        resources.insert(
            name.clone(),
            Arc::new(page_fit_svg(width, height).into_bytes()),
        );
        out.push_str(&body[cursor..open_end]);
        for anchor in section_anchor_ids(section) {
            out.push_str(&format!("<a id=\"{anchor}\"></a>"));
        }
        out.push_str(&format!(
            "<img class=\"cre-plate\" src=\"{name}\" \
             style=\"display:block;max-width:100%;height:auto\" alt=\"\"/>"
        ));
        cursor = close;
    }
    out.push_str(&body[cursor..]);
    out
}

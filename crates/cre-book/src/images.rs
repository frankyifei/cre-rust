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
use crate::resources::ResourceProvider;

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

/// What an image's header says: its format, and its intrinsic size when the
/// header carries one (an SVG does not; its size lives in the markup).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageProbe {
    pub mime: &'static str,
    pub size: Option<(u32, u32)>,
}

/// Bytes read to learn an image's format and size, in growing steps.
///
/// PNG's `IHDR` sits at offset 16 and GIF's screen descriptor inside the first
/// 13 bytes; JPEG is why the second step is generous - its `SOF` marker comes
/// after the `APP0`/`APP1` segments, and a cover with a large EXIF thumbnail
/// can push it tens of kilobytes in.
const IMAGE_HEADER_STEPS: [usize; 3] = [8 * 1024, 64 * 1024, 512 * 1024];

/// An image's format and intrinsic size, read from its header.
///
/// The header is a few hundred bytes, so a container that can hand out a
/// prefix ([`ResourceProvider::header`]) never materialises the picture: a zip
/// entry stops inflating at the prefix instead of decompressing 4 MB to learn
/// that the image is 1000x1500.
pub fn probe_image(provider: &dyn ResourceProvider, key: &str) -> Option<ImageProbe> {
    let bytes = image_header(provider, key)?;
    let mime = image_mime(&bytes, key);
    // The fixed-offset parser first (a PNG with a huge colour profile would
    // send a decoder past the prefix), then the decoder for anything else.
    let size = header_dimensions(&bytes).or_else(|| image_dimensions(&bytes));
    Some(ImageProbe { mime, size })
}

/// The bytes of an image resource that hold its format and intrinsic size.
///
/// Reads a prefix and grows it only when the header is incomplete: PNG's
/// `IHDR` is at offset 16 and GIF's screen descriptor inside the first 13
/// bytes, while JPEG's `SOF` marker sits behind its `APP0`/`APP1` segments, so
/// a cover with a large EXIF thumbnail needs more than a few kilobytes. Half a
/// megabyte of header is already pathological; past that the whole resource is
/// read once (an SVG that declares its size near the end, an unknown format).
pub fn image_header(provider: &dyn ResourceProvider, key: &str) -> Option<Vec<u8>> {
    for step in IMAGE_HEADER_STEPS {
        let bytes = provider.header(key, step)?;
        // An SVG has no raster header; its size is read from the markup by the
        // SVG path, and callers treat `size: None` the same way.
        if image_mime(&bytes, key) == "image/svg+xml"
            || header_dimensions(&bytes).is_some()
            // A short read is the whole resource: this format has no size we
            // can read, so a longer prefix would not help either.
            || bytes.len() < step
        {
            return Some(bytes);
        }
    }
    provider.get(key).map(|bytes| (*bytes).to_vec())
}

/// The intrinsic size of the formats whose header puts it at a known place.
///
/// A decoder walks the container first - `image`'s PNG decoder reads chunks up
/// to `IDAT`, and a cover with a large colour profile can push that past half a
/// megabyte - while PNG, GIF, BMP, WebP (extended) and JPEG each give the size
/// in the first kilobytes. Several callers only need the size ([`probe_image`]),
/// so reading it without a decoder is what keeps a header probe cheap.
fn header_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() >= 24 && bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return Some((width, height));
    }
    if bytes.len() >= 10 && bytes.starts_with(b"GIF8") {
        let width = u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32;
        let height = u16::from_le_bytes(bytes[8..10].try_into().ok()?) as u32;
        return Some((width, height));
    }
    if bytes.len() >= 30 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        // The extended format stores the canvas size as 24-bit values, one less
        // than the real size.
        if &bytes[12..16] == b"VP8X" {
            let width = 1 + u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]);
            let height = 1 + u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]);
            return Some((width, height));
        }
        return None;
    }
    if bytes.len() >= 26 && bytes.starts_with(b"BM") {
        let width = i32::from_le_bytes(bytes[18..22].try_into().ok()?).max(0) as u32;
        let height = i32::from_le_bytes(bytes[22..26].try_into().ok()?).unsigned_abs();
        return Some((width, height));
    }
    if bytes.len() >= 4 && bytes[0] == 0xff && bytes[1] == 0xd8 {
        return jpeg_dimensions(bytes);
    }
    None
}

/// The size from a JPEG's `SOF` marker, walking its segment chain.
///
/// `SOF` comes after whichever `APP` segments the file starts with (JFIF,
/// EXIF, ICC profile), which is why a JPEG needs more header than a PNG.
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut index = 2usize;
    while index + 9 <= bytes.len() {
        if bytes[index] != 0xff {
            return None;
        }
        let marker = bytes[index + 1];
        // A fill byte, a marker without a length, or the start of the scan
        // (no `SOF` after it).
        if marker == 0xff {
            index += 1;
            continue;
        }
        if marker == 0x01 || (0xd0..=0xd8).contains(&marker) {
            index += 2;
            continue;
        }
        if marker == 0xda {
            return None;
        }
        let length = u16::from_be_bytes(bytes[index + 2..index + 4].try_into().ok()?) as usize;
        if length < 2 {
            return None;
        }
        // `SOF0`-`SOF15`, except `DHT` (C4), `JPG` (C8) and `DAC` (CC).
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            let height = u16::from_be_bytes(bytes[index + 5..index + 7].try_into().ok()?) as u32;
            let width = u16::from_be_bytes(bytes[index + 7..index + 9].try_into().ok()?) as u32;
            return Some((width, height));
        }
        index += 2 + length;
    }
    None
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
/// The single picture a part consists of, if it is one.
///
/// The `src` comes from the tag's own attributes (`parse_attrs`), which is both
/// cheaper and less surprising than matching `\bsrc=` anywhere in the markup: a
/// `data-src` is not a picture the reader can show.
pub fn picture_only_body(body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    if lower.matches("<img").count() != 1 {
        return None;
    }
    let start = lower.find("<img")?;
    let end = body[start..].find('>').map(|offset| start + offset + 1)?;
    let picture = crate::html::attrs_in_order(&body[start..end])
        .into_iter()
        .find(|(name, _)| name == "src")
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())?;
    // Anything left after removing the image (tags excluded) is text, so the
    // section is a normal page with a picture in it.
    let rest = format!("{}{}", &body[..start], &body[end..]);
    text_without_tags(&rest)
        .trim()
        .is_empty()
        .then_some(picture)
}

/// `id`/`name` values carried by the tags inside one part, in document order.
///
/// They are the book's navigation targets; the plate rewrite replaces the markup
/// around them, so they are re-emitted as empty anchors (an empty `<a>` has no
/// box, and `page_for_node` falls through to the content that follows it).
pub fn section_anchor_ids(section: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut at = 0usize;
    while let Some(offset) = section[at..].find('<') {
        let start = at + offset;
        let Some(end) = crate::html::find_tag_end(section, start) else {
            break;
        };
        for (name, value) in crate::html::attrs_in_order(&section[start..end]) {
            if name != "id" && name != "name" {
                continue;
            }
            let value = value.trim();
            if value.is_empty() || out.iter().any(|existing| existing == value) {
                continue;
            }
            out.push(value.to_string());
        }
        at = end;
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
        let Some(picture) = picture_only_body(&body[open_end..close]) else {
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
        // A picture-only part can hold a small logo; only a picture that
        // covers the part should be scaled to the page.
        let class = if (image_width as f32 * image_height as f32) >= 0.5 * width * height {
            "cre-plate cre-page-picture"
        } else {
            "cre-plate"
        };
        out.push_str(&format!(
            "<img class=\"{class}\" src=\"{name}\" \
             style=\"display:block;max-width:100%;height:auto\" alt=\"\"/>"
        ));
        cursor = close;
    }
    out.push_str(&body[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A provider that records how much of the resource was read.
    struct HeaderOnly {
        bytes: Vec<u8>,
        whole_reads: AtomicUsize,
        largest_prefix: AtomicUsize,
    }

    impl HeaderOnly {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                bytes,
                whole_reads: AtomicUsize::new(0),
                largest_prefix: AtomicUsize::new(0),
            }
        }
    }

    impl ResourceProvider for HeaderOnly {
        fn get(&self, _key: &str) -> Option<Arc<Vec<u8>>> {
            self.whole_reads.fetch_add(1, Ordering::Relaxed);
            Some(Arc::new(self.bytes.clone()))
        }

        fn header(&self, _key: &str, max_bytes: usize) -> Option<Vec<u8>> {
            self.largest_prefix.fetch_max(max_bytes, Ordering::Relaxed);
            let end = self.bytes.len().min(max_bytes);
            Some(self.bytes[..end].to_vec())
        }
    }

    /// A PNG whose header says `width`x`height`, followed by filler: a decoder
    /// would walk the chunks after it, the header parser must not.
    fn png_header(width: u32, height: u32, total: usize) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes.resize(total, 0x7f);
        bytes
    }

    /// A JPEG whose `SOF` sits behind an `APP1` segment of `exif` bytes.
    fn jpeg_header(width: u16, height: u16, exif: usize, total: usize) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xd8];
        // APP1 with a large payload (an EXIF thumbnail, in a real file).
        let length = (exif + 2) as u16;
        bytes.extend_from_slice(&[0xff, 0xe1]);
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes.resize(bytes.len() + exif, 0x00);
        // SOF0: length, precision, height, width, components.
        bytes.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08]);
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&[0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01]);
        bytes.resize(total, 0x00);
        bytes
    }

    #[test]
    fn a_png_is_sized_from_its_first_kilobytes() {
        let provider = HeaderOnly::new(png_header(520, 800, 800 * 1024));
        let probe = probe_image(&provider, "cover.png").expect("probe");
        assert_eq!(probe.mime, "image/png");
        assert_eq!(probe.size, Some((520, 800)));
        assert_eq!(
            provider.whole_reads.load(Ordering::Relaxed),
            0,
            "no whole read"
        );
        assert_eq!(
            provider.largest_prefix.load(Ordering::Relaxed),
            8 * 1024,
            "one small prefix is enough for a PNG"
        );
    }

    #[test]
    fn a_jpeg_sized_behind_a_large_exif_block_grows_the_prefix() {
        // 32 kB of EXIF pushes SOF past the first step but not the second.
        let provider = HeaderOnly::new(jpeg_header(1000, 1500, 32 * 1024, 400 * 1024));
        let probe = probe_image(&provider, "cover.jpg").expect("probe");
        assert_eq!(probe.mime, "image/jpeg");
        assert_eq!(probe.size, Some((1000, 1500)));
        assert_eq!(
            provider.whole_reads.load(Ordering::Relaxed),
            0,
            "no whole read"
        );
        assert_eq!(
            provider.largest_prefix.load(Ordering::Relaxed),
            64 * 1024,
            "the prefix grows once, then stops"
        );
    }

    #[test]
    fn a_svg_is_probed_without_a_size() {
        let mut provider = HeaderOnly::new(Vec::new());
        provider.bytes =
            b"<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 10 10\"/>".to_vec();
        let probe = probe_image(&provider, "drawing.svg").expect("probe");
        assert_eq!(probe.mime, "image/svg+xml");
        assert_eq!(probe.size, None);
        assert_eq!(provider.whole_reads.load(Ordering::Relaxed), 0);
    }
}

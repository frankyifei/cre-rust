//! Image references in generated HTML, and the size estimates layout needs
//! before anything is decoded.

use percent_encoding::percent_decode_str;

use crate::html::{
    base64_encode, file_extension, find_matching_close_tag, find_tag_end, is_external,
    next_tag_start, normalize_path, parse_attrs, replace_attribute_value,
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
                if let Some(relative) = decoded.strip_prefix("epub://book/") {
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
            let book_url = href.starts_with("epub://book/");
            if !href.is_empty() && !href.starts_with("data:") && (book_url || !is_external(href)) {
                let without_fragment = href.split('#').next().unwrap_or(href);
                let relative = without_fragment
                    .strip_prefix("epub://book/")
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

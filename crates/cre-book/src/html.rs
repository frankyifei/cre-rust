//! Markup and URL helpers shared by every format.

use std::collections::HashMap;

use percent_encoding::percent_decode_str;

/// Rewrites `name="value"` (or single quotes) inside one tag.
pub fn replace_attribute_value(tag: &str, name: &str, value: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let at = lower.find(&format!("{name}="))?;
    let rest = &tag[at + name.len() + 1..];
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let close = rest[1..].find(quote)? + 1;
    let mut out = String::with_capacity(tag.len() + value.len());
    out.push_str(&tag[..at + name.len() + 1]);
    out.push(quote);
    out.push_str(value);
    out.push_str(&rest[close..]);
    Some(out)
}

/// Rough decoded size of an image resource, from its file size and format.
/// Estimated decoded size of one image, for the chunk memory budgets.
///
/// Used by the formats whose resources are a plain in-memory map (MOBI/KF8):
/// there is no provider to ask, but the bytes are at hand, so the same
/// dimensions-based estimate the EPUB path uses applies. `name` supplies the
/// format when the magic bytes are not recognised (`mobi-image-3` has no
/// extension).
pub fn tag_name(raw: &str) -> String {
    let rest = raw.trim_start_matches('<').trim_start_matches('/');
    rest.chars()
        .take_while(|ch| !ch.is_whitespace() && *ch != '>' && *ch != '/')
        .collect::<String>()
        .to_ascii_lowercase()
}

pub fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    if needle.is_empty() {
        return true;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

pub fn parse_attrs(tag: &str) -> HashMap<String, String> {
    let mut attrs = HashMap::new();
    let mut i = 0;
    while i < tag.len() {
        let Some(c) = tag[i..].chars().next() else {
            break;
        };
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            i += c.len_utf8();
            while i < tag.len() {
                let ch = tag[i..].chars().next().unwrap();
                if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | ':') {
                    i += ch.len_utf8();
                } else {
                    break;
                }
            }
            let name = tag[start..i].to_ascii_lowercase();
            while i < tag.len() && tag[i..].chars().next().is_some_and(|c| c.is_whitespace()) {
                i += 1;
            }
            if i < tag.len() && tag[i..].starts_with('=') {
                i += 1;
                while i < tag.len() && tag[i..].chars().next().is_some_and(|c| c.is_whitespace()) {
                    i += 1;
                }
                if i < tag.len() {
                    let quote = tag[i..].chars().next().unwrap();
                    if quote == '"' || quote == '\'' {
                        i += 1;
                        let value_start = i;
                        while i < tag.len() {
                            let ch = tag[i..].chars().next().unwrap();
                            if ch == quote {
                                break;
                            }
                            i += ch.len_utf8();
                        }
                        attrs.insert(name, tag[value_start..i].to_string());
                        if i < tag.len() {
                            i += 1;
                        }
                    }
                }
            }
        } else {
            i += c.len_utf8();
        }
    }
    attrs
}

pub fn file_extension(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default()
}

pub fn next_tag_start(text: &str, from: usize, tag_lower: &str) -> Option<usize> {
    if from > text.len() {
        return None;
    }
    let mut from = from;
    while let Some(rel) = text[from..].find(tag_lower) {
        let pos = from + rel;
        if is_tag_boundary(text, pos + tag_lower.len()) {
            return Some(pos);
        }
        from = pos + tag_lower.len();
    }
    None
}

pub fn is_tag_boundary(text: &str, pos: usize) -> bool {
    if pos >= text.len() {
        return true;
    }
    match text[pos..].chars().next() {
        Some(c) => c.is_whitespace() || c == '>' || c == '/',
        None => true,
    }
}

pub fn find_matching_close_tag(
    text: &str,
    open_start: usize,
    open_tag: &str,
    close_tag: &str,
) -> Option<(usize, usize)> {
    let mut depth = 1usize;
    let mut scan = open_start + open_tag.len();

    while scan < text.len() {
        let next_open = next_tag_start(text, scan, open_tag);
        let next_close = next_tag_start(text, scan, close_tag);
        match (next_open, next_close) {
            (None, None) => return None,
            (Some(open_pos), None) => {
                depth += 1;
                scan = find_tag_end(text, open_pos)?;
            }
            (None, Some(close_pos)) => {
                depth -= 1;
                let close_end = find_tag_end(text, close_pos)?;
                if depth == 0 {
                    return Some((close_pos, close_end));
                }
                scan = close_end;
            }
            (Some(open_pos), Some(close_pos)) => {
                if close_pos < open_pos {
                    depth -= 1;
                    let close_end = find_tag_end(text, close_pos)?;
                    if depth == 0 {
                        return Some((close_pos, close_end));
                    }
                    scan = close_end;
                } else {
                    depth += 1;
                    scan = find_tag_end(text, open_pos)?;
                }
            }
        }
    }

    None
}

pub fn find_tag_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if start >= bytes.len() || bytes[start] != b'<' {
        return None;
    }
    let mut i = start + 1;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let byte = bytes[i];
        if let Some(quote_byte) = quote {
            if byte == quote_byte {
                quote = None;
            }
        } else if byte == b'"' || byte == b'\'' {
            quote = Some(byte);
        } else if byte == b'>' {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// Encodes raw bytes as standard base64 (used to build `data:` URIs).
pub fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let value = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((value >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((value >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((value >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(value & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub fn is_external(value: &str) -> bool {
    let value = value.trim();
    value.is_empty()
        || value.starts_with('#')
        || value.starts_with('/')
        || value.starts_with("data:")
        || value.starts_with("mailto:")
        || value.contains("://")
        || value.starts_with("javascript:")
}

pub fn value_without_fragment(value: &str) -> String {
    // A fragment and a query are both metadata: the zip entry is the path.
    value.split(['#', '?']).next().unwrap_or(value).to_string()
}

pub fn resolve_href(base_dir: &str, href: &str) -> String {
    let href = percent_decode_str(href).decode_utf8_lossy();
    let href = href.replace('\\', "/");
    let path = if href.starts_with('/') {
        href.trim_start_matches('/').to_string()
    } else {
        format!("{base_dir}/{href}")
    };
    normalize_path(&path)
}

pub fn normalize_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

pub fn encode_path(path: &str) -> String {
    path.replace('%', "%25")
        .replace(' ', "%20")
        .replace('#', "%23")
        .replace('?', "%3F")
}

/// The value of `name` in one tag, without the quotes.
pub fn attr_value(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let at = lower.find(name)?;
    let rest = tag[at + name.len()..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let end = rest[1..].find(quote)? + 1;
    Some(rest[1..end].to_string())
}

/// A length attribute as pixels, ignoring its unit (`987`, `987px`).
pub fn parse_pixels(value: String) -> Option<f32> {
    let digits: String = value
        .trim()
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    digits.parse::<f32>().ok()
}

/// The text of an HTML fragment with its tags removed.
pub fn text_without_tags(html: &str) -> String {
    let mut text = String::new();
    let mut depth = 0usize;
    for ch in html.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => text.push(ch),
            _ => {}
        }
    }
    text
}

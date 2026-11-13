//! `@font-face` collection and rewriting.

use crate::html::parse_attrs;

/// Family names declared by the `@font-face` rules in `html`.
///
/// The engine does not load these fonts itself: Blitz fetches each `src` through
/// the document's `NetProvider`, decodes TTF/OTF/WOFF/WOFF2 and registers it
/// under the declared family (see `blitz_dom::net::fetch_font_face`). The names
/// are collected so the reader can list the book's fonts and decide whether to
/// use them at all.
///
/// TODO(upstream): this is a scan of the CSS text because `blitz-dom` exposes no
/// query for the `@font-face` families a document declares. Replace it once the
/// font registry can be enumerated.
pub fn font_faces_in(html: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for family in font_face_declarations(html).into_iter().map(|d| d.family) {
        if !out
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&family))
        {
            out.push(family);
        }
    }
    out
}

/// One `@font-face` rule: the family it registers and the `url()` sources it
/// offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontFaceDeclaration {
    pub family: String,
    pub urls: Vec<String>,
}

/// Every `@font-face` rule in `html` (or a stylesheet), in document order.
///
/// TODO(upstream): a scan of the CSS text, because `blitz-dom` exposes no query
/// for the `@font-face` rules a document declares.
pub fn font_face_declarations(html: &str) -> Vec<FontFaceDeclaration> {
    // ASCII lowercasing keeps byte offsets (CSS names are case-sensitive).
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(found) = lower[from..].find("@font-face") {
        let start = from + found;
        let Some(open) = lower[start..].find('{') else {
            break;
        };
        let body_start = start + open + 1;
        let Some(close) = lower[body_start..].find('}') else {
            break;
        };
        let block = &html[body_start..body_start + close];
        if let Some(family) = font_family_of(block) {
            out.push(FontFaceDeclaration {
                family,
                urls: font_urls_of(block),
            });
        }
        from = body_start + close + 1;
    }
    out
}

/// The families whose rules can actually be loaded.
///
/// `has_resource` answers for one `url()` value. A book whose converter dropped
/// the font bytes still leaves the rule in the stylesheet, and listing such a
/// family would offer the reader a font that can never resolve; KF8 is where
/// this shows up because the URLs there are flat resource names
/// (`mobi-image-N`), so the answer is a lookup.
pub fn loadable_font_faces(html: &str, has_resource: impl Fn(&str) -> bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for declaration in font_face_declarations(html) {
        if !declaration.urls.iter().any(|url| has_resource(url)) {
            continue;
        }
        if !out
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&declaration.family))
        {
            out.push(declaration.family);
        }
    }
    out
}

/// `url(...)` values inside one `@font-face` block.
pub fn font_urls_of(block: &str) -> Vec<String> {
    let lower = block.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(found) = lower[from..].find("url(") {
        let start = from + found + 4;
        let Some(close) = block[start..].find(')') else {
            break;
        };
        let value = block[start..start + close]
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .trim();
        if !value.is_empty() {
            out.push(value.to_string());
        }
        from = start + close + 1;
    }
    out
}

/// Removes the `@font-face` rules from `html`, leaving everything else alone.
///
/// The rules are dropped rather than their `font-family` declarations: a name
/// that resolves to nothing falls back to the next family in the list, which is
/// what "do not use the book's fonts" means.
pub fn strip_font_faces(html: &str) -> std::borrow::Cow<'_, str> {
    let lower = html.to_ascii_lowercase();
    if !lower.contains("@font-face") {
        return std::borrow::Cow::Borrowed(html);
    }
    let mut out = String::with_capacity(html.len());
    let mut from = 0usize;
    while let Some(found) = lower[from..].find("@font-face") {
        let start = from + found;
        let Some(open) = lower[start..].find('{') else {
            break;
        };
        let body_start = start + open + 1;
        let Some(close) = lower[body_start..].find('}') else {
            break;
        };
        out.push_str(&html[from..start]);
        from = body_start + close + 1;
    }
    out.push_str(&html[from..]);
    std::borrow::Cow::Owned(out)
}

/// `href`s of the stylesheets a document links, in document order.
///
/// Only `<link rel="stylesheet">` is considered. EPUB chapters are covered by
/// [`font_faces_in_stylesheets`] (which reads the OPF manifest), so this exists
/// for a standalone HTML document, whose linked CSS is served from disk.
pub fn stylesheet_hrefs(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(pos) = lower[cursor..].find("<link") {
        let start = cursor + pos;
        let end = lower[start..]
            .find('>')
            .map(|p| p + start + 1)
            .unwrap_or(html.len());
        let attrs = parse_attrs(&html[start..end]);
        let rel = attrs
            .get("rel")
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        if rel.split_ascii_whitespace().any(|r| r == "stylesheet")
            && let Some(href) = attrs.get("href")
        {
            out.push(href.clone());
        }
        cursor = end;
    }
    out
}

/// `font-family` value inside one `@font-face` block.
pub fn font_family_of(block: &str) -> Option<String> {
    let lower = block.to_ascii_lowercase();
    let at = lower.find("font-family")?;
    let rest = &block[at + "font-family".len()..];
    let rest = &rest[rest.find(':')? + 1..];
    let value = rest
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

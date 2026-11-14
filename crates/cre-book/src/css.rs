//! CSS text transformations.

use crate::html::contains_ignore_ascii_case;

/// Rewrites `body.class`, `body#id` and `body[...]` selectors to the bare
/// selector.
///
/// A spine document's `<body class="cov2">` is rebuilt as a
/// `<section class="cov2">` inside the chunk, because a chunk holds several
/// documents and only one `<body>` can exist. Rules like
/// `body.cov2 { background-image: … }` — the usual way a cover page styles its
/// own background — therefore never matched, and the background was lost.
/// Linked stylesheets are shared by every document and are not scoped, so the
/// one thing that can be done there is to drop the `body` part of the selector;
/// the class still only appears on the document that declared it.
pub fn body_selectors_to_class(css: &str) -> String {
    if !contains_ignore_ascii_case(css, "body") {
        return css.to_string();
    }
    let bytes = css.as_bytes();
    let mut out = String::with_capacity(css.len());
    let mut copied = 0usize;
    let mut index = 0usize;
    while index + 4 <= bytes.len() {
        if bytes[index..index + 4].eq_ignore_ascii_case(b"body") {
            // Not part of a longer name (`tbody`, `.body`, `my-body`).
            let previous_ok = index == 0
                || !matches!(
                    bytes[index - 1],
                    b'-' | b'_' | b'.' | b'#' | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z'
                );
            let follows_ok = matches!(
                bytes.get(index + 4),
                Some(b'.') | Some(b'#') | Some(b'[') | Some(b':')
            );
            if previous_ok && follows_ok {
                out.push_str(&css[copied..index]);
                copied = index + 4;
                index += 4;
                continue;
            }
        }
        index += 1;
    }
    out.push_str(&css[copied..]);
    out
}

// The layout tokens a format's chunk HTML may contain, and their substitution.
//
// A chunk's HTML is generated once, when the book is opened, and is laid out
// again every time the reader changes the font, line spacing or margins.
// Anything layout-dependent written into that HTML would be stale afterwards,
// so the generators do not write the values: they write a token, and
// [`substitute`] fills in the current settings at layout time. That is what
// makes a settings change a *relayout* (the chunk under the reader now, the
// rest on the background worker) instead of a reopen, for every format.
//
// Rules for a format's generated markup:
//
// - write [`FONT_SIZE`] where the base font size in px would go;
// - write [`PAGE_MARGIN`] where the page margin used as padding would go (the
//   standalone-HTML path does this);
// - every other value in the base CSS (line height, padding constants, image
//   cap) must be the same for every settings combination. If one has to follow
//   the reader's settings, give it a token here and fill it in
//   [`substitute`], otherwise the chunk would need regenerating on a change.
//
// The tokens are plain ASCII and only replaced before the HTML is parsed, so
// any remaining occurrence in a book's text is inert.

/// Base font size in CSS px (the reader's font size).
pub const FONT_SIZE: &str = "__CRE_FONT_SIZE_PX__";

/// Page margin in CSS px, already scaled to the layout viewport.
pub const PAGE_MARGIN: &str = "__CRE_PAGE_MARGIN_PX__";

/// Fills the tokens in one chunk's HTML with the current layout settings.
///
/// Returns the input unchanged (borrowed) when the chunk carries no token,
/// which is the case for author HTML and for books already substituted.
pub fn substitute<'a>(
    html: &'a str,
    font_size: f32,
    page_margin: f32,
) -> std::borrow::Cow<'a, str> {
    if !has_tokens(html) {
        return std::borrow::Cow::Borrowed(html);
    }
    let mut out = html.to_string();
    if out.contains(FONT_SIZE) {
        out = out.replace(FONT_SIZE, &format!("{font_size:.2}"));
    }
    if out.contains(PAGE_MARGIN) {
        out = out.replace(PAGE_MARGIN, &format!("{page_margin:.2}"));
    }
    std::borrow::Cow::Owned(out)
}

/// Whether one of the layout tokens appears in `html`.
pub fn has_tokens(html: &str) -> bool {
    html.contains(FONT_SIZE) || html.contains(PAGE_MARGIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_only_what_is_present() {
        let html = "<style>html { font-size: __CRE_FONT_SIZE_PX__px; }</style>";
        let out = substitute(html, 26.0, 10.0);
        assert_eq!(out.as_ref(), "<style>html { font-size: 26.00px; }</style>");
        // No token: borrowed, untouched.
        let plain = "<p>no tokens here</p>";
        let out = substitute(plain, 26.0, 10.0);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), plain);
    }

    #[test]
    fn substitutes_the_page_margin_token() {
        let html = "body { padding: __CRE_PAGE_MARGIN_PX__px; }";
        assert_eq!(
            substitute(html, 18.0, 7.5).as_ref(),
            "body { padding: 7.50px; }"
        );
    }
}

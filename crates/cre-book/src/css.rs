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

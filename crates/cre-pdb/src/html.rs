//! Small byte-level helpers for the HTML payload MOBI and KF8 store.
//!
//! MOBI/KF8 markup is HTML with attributes our renderer cannot use directly
//! (`filepos`, `recindex`, `kindle:pos:fid`…) and with no anchors at the byte
//! offsets internal links point at. Both readers therefore pre-process the
//! markup textually before it ever reaches a parser, which needs: tag
//! enumeration, attribute rewriting and anchor insertion at a byte offset. A
//! real parser would normalise the markup and lose the byte offsets the links
//! are expressed in, which is why this is textual (crengine does the same in
//! `pdbfmt.cpp`, and calibre in `mobi6.py`/`markup.py`).

/// Walks the markup and lets `f` replace each `<…>` tag.
pub fn rewrite_tags(data: &[u8], mut f: impl FnMut(&[u8]) -> Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 256);
    let mut at = 0usize;
    while at < data.len() {
        match data[at..].iter().position(|byte| *byte == b'<') {
            None => {
                out.extend_from_slice(&data[at..]);
                break;
            }
            Some(relative) => {
                let start = at + relative;
                out.extend_from_slice(&data[at..start]);
                let Some(end_relative) = data[start..].iter().position(|byte| *byte == b'>') else {
                    out.extend_from_slice(&data[start..]);
                    break;
                };
                let end = start + end_relative + 1;
                out.extend_from_slice(&f(&data[start..end]));
                at = end;
            }
        }
    }
    out
}

/// A start tag split into its prefix and attributes, so attributes can be
/// rewritten or added without disturbing the rest of the tag.
#[derive(Clone, Debug)]
pub struct Tag {
    /// From `<` through the tag name, e.g. `<img` or `<a`.
    pub prefix: String,
    pub attributes: Vec<(String, String)>,
    pub self_closing: bool,
}

impl Tag {
    /// Parses a start tag; `None` for end tags and comments.
    pub fn parse(tag: &[u8]) -> Option<Self> {
        let text = String::from_utf8_lossy(tag);
        let trimmed = text.trim_end_matches('>');
        let self_closing = trimmed.trim_end().ends_with('/');
        let body = trimmed.trim_end().trim_end_matches('/');
        let mut chars = body.char_indices();
        if chars.next().map(|(_, ch)| ch) != Some('<') {
            return None;
        }
        let mut prefix_end = body.len();
        for (index, ch) in body.char_indices().skip(1) {
            if ch.is_whitespace() {
                prefix_end = index;
                break;
            }
        }
        let prefix = body[..prefix_end].to_string();
        if prefix.len() <= 1 || prefix.starts_with("</") || prefix.starts_with("<!") {
            return None;
        }
        let mut attributes = Vec::new();
        let mut rest = body[prefix_end..].trim_start();
        while !rest.is_empty() {
            let name_end = rest
                .find(|ch: char| ch.is_whitespace() || ch == '=' || ch == '/')
                .unwrap_or(rest.len());
            if name_end == 0 {
                rest = rest[1..].trim_start();
                continue;
            }
            let name = rest[..name_end].to_string();
            let mut after = rest[name_end..].trim_start();
            let mut value = String::new();
            if let Some(stripped) = after.strip_prefix('=') {
                let stripped = stripped.trim_start();
                if let Some(quote) = stripped
                    .chars()
                    .next()
                    .filter(|ch| *ch == '"' || *ch == '\'')
                {
                    let inner = &stripped[1..];
                    let end = inner.find(quote).unwrap_or(inner.len());
                    value = inner[..end].to_string();
                    after = inner.get(end + 1..).unwrap_or("");
                } else {
                    let end = stripped
                        .find(|ch: char| ch.is_whitespace() || ch == '>')
                        .unwrap_or(stripped.len());
                    value = stripped[..end].to_string();
                    after = &stripped[end..];
                }
            }
            attributes.push((name, value));
            rest = after.trim_start();
        }
        Some(Self {
            prefix,
            attributes,
            self_closing,
        })
    }

    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn set_attribute(&mut self, name: &str, value: &str) {
        if let Some(existing) = self
            .attributes
            .iter_mut()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
        {
            existing.1 = value.to_string();
            return;
        }
        self.attributes.push((name.to_string(), value.to_string()));
    }

    pub fn rename_attribute(&mut self, from: &str, to: &str) {
        if let Some(existing) = self
            .attributes
            .iter_mut()
            .find(|(key, _)| key.eq_ignore_ascii_case(from))
        {
            existing.0 = to.to_string();
        }
    }

    pub fn remove_attribute(&mut self, name: &str) {
        self.attributes
            .retain(|(key, _)| !key.eq_ignore_ascii_case(name));
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = String::with_capacity(self.prefix.len() + 32);
        out.push_str(&self.prefix);
        for (name, value) in &self.attributes {
            out.push(' ');
            out.push_str(name);
            out.push_str("=\"");
            out.push_str(&escape_attribute(value));
            out.push('"');
        }
        if self.self_closing {
            out.push('/');
        }
        out.push('>');
        out.into_bytes()
    }
}

fn escape_attribute(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("&quot;"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

/// One start tag and where it sits in the markup.
pub struct StartTag {
    pub start: usize,
    /// Index of the closing `>`.
    pub gt: usize,
    pub tag: Tag,
}

/// Finds the start tag a byte offset belongs to.
///
/// The offset is either inside a start tag (then that tag is returned), or it
/// sits in text (then the start tag that follows it, after whitespace, is
/// returned). Self-closing tags are skipped: an id on them would be useless and
/// crengine does the same (`findStartTagAt` in `pdbfmt.cpp`).
pub fn start_tag_at(data: &[u8], pos: usize) -> Option<StartTag> {
    // Inside a tag?
    if pos < data.len() {
        let mut index = pos;
        let mut inside = false;
        while index > 0 {
            index -= 1;
            match data[index] {
                b'<' => {
                    inside = data.get(index + 1) != Some(&b'/');
                    break;
                }
                b'>' => break,
                _ => {}
            }
        }
        if inside {
            let mut gt = pos;
            while gt < data.len() && data[gt] != b'>' {
                if data[gt] == b'<' {
                    return None;
                }
                gt += 1;
            }
            if gt < data.len() {
                let span = &data[index..=gt];
                if let Some(tag) = Tag::parse(span)
                    && !tag.self_closing
                {
                    return Some(StartTag {
                        start: index,
                        gt,
                        tag,
                    });
                }
            }
        }
    }
    // Otherwise the following start tag, skipping whitespace and self-closing
    // tags.
    let mut at = pos;
    if data.get(at) == Some(&b'>') {
        at += 1;
    }
    loop {
        while at < data.len() && data[at].is_ascii_whitespace() {
            at += 1;
        }
        if data.get(at) != Some(&b'<') || data.get(at + 1) == Some(&b'/') {
            return None;
        }
        let mut gt = at + 1;
        while gt < data.len() && data[gt] != b'>' && data[gt] != b'<' {
            gt += 1;
        }
        if gt >= data.len() || data[gt] != b'>' {
            return None;
        }
        let tag = Tag::parse(&data[at..=gt])?;
        if !tag.self_closing {
            return Some(StartTag { start: at, gt, tag });
        }
        at = gt + 1;
    }
}

/// Inserts an attribute into the start tag ending at `gt`, before the `>` (and
/// before a trailing `/` for self-closing tags).
pub fn insert_attribute_at(gt: usize, data: &[u8], name: &str, value: &str) -> Vec<u8> {
    let mut tag = Vec::with_capacity(64);
    let mut cut = gt;
    while cut > 0 && data[cut - 1].is_ascii_whitespace() {
        cut -= 1;
    }
    let self_closing = cut > 0 && data[cut - 1] == b'/';
    let cut_at = if self_closing { cut - 1 } else { gt };
    tag.extend_from_slice(&data[..cut_at]);
    tag.push(b' ');
    tag.extend_from_slice(name.as_bytes());
    tag.extend_from_slice(b"=\"");
    tag.extend_from_slice(value.as_bytes());
    tag.extend_from_slice(b"\"");
    if self_closing {
        tag.extend_from_slice(&data[cut - 1..gt]);
    }
    tag.extend_from_slice(&data[gt..]);
    tag
}

/// Anchors to insert at a byte offset, resolved to an id.
pub struct AnchorTarget {
    pub offset: usize,
    pub id: String,
}

/// Inserts one anchor per target, in ascending offset order.
///
/// A target whose offset lands in (or immediately before) a start tag gets the
/// id as an attribute on that tag; only targets in the middle of text become a
/// standalone `<a id="…"></a>`, which is what crengine and calibre both do so
/// text nodes are not split unnecessarily.
pub fn insert_anchors(data: &[u8], targets: &[AnchorTarget]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + targets.len() * 24);
    let mut copied = 0usize;
    for target in targets {
        match start_tag_at(data, target.offset) {
            Some(found) => {
                let insert_at = found.start;
                if insert_at < copied {
                    continue;
                }
                out.extend_from_slice(&data[copied..insert_at]);
                let span = &data[insert_at..=found.gt];
                out.extend_from_slice(&insert_attribute_at(span.len() - 1, span, "id", &target.id));
                copied = found.gt + 1;
            }
            None => {
                let at = target.offset.min(data.len());
                if at < copied {
                    continue;
                }
                out.extend_from_slice(&data[copied..at]);
                out.extend_from_slice(b"<a id=\"");
                out.extend_from_slice(target.id.as_bytes());
                out.extend_from_slice(b"\"></a>");
                copied = at;
            }
        }
    }
    out.extend_from_slice(&data[copied.min(data.len())..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_rewrites_attributes() {
        let tag = Tag::parse(b"<a filepos=0000010 class='x'>").unwrap();
        assert_eq!(tag.prefix, "<a");
        assert_eq!(tag.attribute("filepos"), Some("0000010"));
        assert_eq!(tag.attribute("class"), Some("x"));
        let mut tag = tag;
        tag.remove_attribute("filepos");
        tag.set_attribute("href", "#filepos10");
        assert_eq!(
            String::from_utf8(tag.to_bytes()).unwrap(),
            "<a class=\"x\" href=\"#filepos10\">"
        );
    }

    #[test]
    fn finds_the_start_tag_after_an_offset() {
        let data = b"text<div>more</div>";
        let found = start_tag_at(data, 4).unwrap();
        assert_eq!(&data[found.start..=found.gt], b"<div>");
    }

    #[test]
    fn anchors_go_on_the_tag_when_possible() {
        let data = b"x <p>hello</p>";
        let out = insert_anchors(
            data,
            &[AnchorTarget {
                offset: 2,
                id: "filepos2".to_string(),
            }],
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "x <p id=\"filepos2\">hello</p>"
        );
    }

    #[test]
    fn anchors_become_a_marker_in_text() {
        let data = b"<p>hello</p>";
        let out = insert_anchors(
            data,
            &[AnchorTarget {
                offset: 5,
                id: "filepos5".to_string(),
            }],
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "<p>he<a id=\"filepos5\"></a>llo</p>"
        );
    }
}

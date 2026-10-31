use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::Path;

pub struct Fb2Book {
    pub title: Option<String>,
    pub creator: Option<String>,
    pub publisher: Option<String>,
    pub language: Option<String>,
    pub isbn: Option<String>,
    pub description: Option<String>,
    pub binary: HashMap<String, Vec<u8>>,
    pub fb2_xml: String,
}

impl Fb2Book {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let bytes = std::fs::read(path).context("read fb2")?;
        Self::from_bytes(&bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let xml = std::str::from_utf8(bytes).context("fb2 not utf8")?;
        Self::from_xml(xml)
    }

    pub fn from_xml(xml: &str) -> Result<Self> {
        let doc = roxmltree::Document::parse(xml).context("parse fb2 xml")?;
        let root = doc.root_element();
        if root.tag_name().name() != "FictionBook" {
            bail!(
                "not a FictionBook2 document (root element: {})",
                root.tag_name().name()
            );
        }

        let mut title = None;
        let mut creator = None;
        let mut publisher = None;
        let mut language = None;
        let mut isbn = None;
        let mut description = None;
        let mut binary = HashMap::new();

        if let Some(desc) = root.children().find(|n| n.has_tag_name("description")) {
            if let Some(ti) = desc.children().find(|n| n.has_tag_name("title-info")) {
                title = text_of(&ti, "book-title");
                language = text_of(&ti, "lang");
                creator = author_of(&ti);
                publisher = text_of(&desc, "publisher");
                isbn = text_of(&desc, "isbn");
            }
            if let Some(d) = desc.children().find(|n| n.has_tag_name("description")) {
                let html = body_node_to_html(d);
                if !html.trim().is_empty() {
                    description = Some(html);
                }
            }
        }

        for bin in root.children().filter(|n| n.has_tag_name("binary")) {
            let name = bin.attribute("name").unwrap_or_default();
            let content_type = bin
                .attribute("content-type")
                .unwrap_or("application/octet-stream");
            if name.is_empty() {
                continue;
            }
            let b64 = bin
                .text()
                .unwrap_or_default()
                .replace(char::is_whitespace, "");
            if let Ok(data) = base64_decode(&b64) {
                let _ = content_type;
                binary.insert(name.to_string(), data);
            }
        }

        Ok(Self {
            title,
            creator,
            publisher,
            language,
            isbn,
            description,
            binary,
            fb2_xml: xml.to_string(),
        })
    }

    pub fn to_html(&self, opts: &Fb2HtmlOptions) -> Result<String> {
        Ok(self.to_html_with_toc(opts)?.0)
    }

    pub fn to_html_with_toc(&self, opts: &Fb2HtmlOptions) -> Result<(String, Vec<Fb2TocItem>)> {
        let doc = roxmltree::Document::parse(&self.fb2_xml)?;
        let root = doc.root_element();

        let mut body_html = String::new();
        let mut toc = Vec::new();

        if let Some(desc_html) = &self.description {
            body_html.push_str(&format!(
                "<div class=\"cre-annotation\">{desc_html}</div>\n"
            ));
        }

        for (index, body) in root
            .children()
            .filter(|n| n.has_tag_name("body"))
            .enumerate()
        {
            let sect = body.attribute("name").unwrap_or("");
            let section_id = format!("fb2-body-{index}");
            if let Some(title) = first_section_title(body) {
                toc.push(Fb2TocItem {
                    title,
                    href: format!("#{section_id}"),
                    depth: 1,
                });
            }
            body_html.push_str(&format!(
                "<section id=\"{section_id}\" class=\"cre-body\" data-name=\"{sect}\">\n"
            ));
            for child in body.children() {
                let mut heading_level = 0u32;
                body_html.push_str(&node_to_html(child, &self.binary, &mut heading_level));
                body_html.push('\n');
            }
            body_html.push_str("</section>\n");
        }

        let css = format!(
            "/* cre-render-base */\nbody {{ font-family: serif; font-size: {}px; line-height: {}; padding: {}px {}px; background: white; color: black; }}
img {{ max-width: 100%; height: auto; }}
h1,h2,h3,h4,h5,h6 {{ font-family: sans-serif; }}
blockquote {{ border-left: 3px solid #888; padding-left: 1em; margin: 1em 0; font-style: italic; }}
.cre-author {{ font-style: italic; text-align: right; }}
.cre-annotation {{ background: #f0f0f0; padding: 0.5em; margin-bottom: 1em; }}
.poem {{ font-style: normal; }}",
            opts.font_size,
            opts.line_height,
            opts.margin_y,
            opts.margin_x,
        );

        let title_html = escape_html(self.title.as_deref().unwrap_or(""));
        let meta_html = if let Some(ref author) = self.creator {
            format!(
                "<h1 class=\"cre-title\">{title_html}</h1><p class=\"cre-author\">{}</p>",
                escape_html(author)
            )
        } else {
            format!("<h1 class=\"cre-title\">{title_html}</h1>")
        };

        let html = format!(
            "<!DOCTYPE html><html lang=\"{}\"><head><meta charset=\"utf-8\"><title>{title_html}</title><style>{css}</style></head><body>{meta_html}{body_html}</body></html>",
            self.language.as_deref().unwrap_or("und"),
        );
        Ok((html, toc))
    }
}

#[derive(Debug, Clone)]
pub struct Fb2TocItem {
    pub title: String,
    pub href: String,
    pub depth: usize,
}

pub struct Fb2HtmlOptions {
    pub font_size: u32,
    pub line_height: f64,
    pub margin_x: u32,
    pub margin_y: u32,
}

impl Default for Fb2HtmlOptions {
    fn default() -> Self {
        Self {
            font_size: 18,
            line_height: 1.5,
            margin_x: 40,
            margin_y: 32,
        }
    }
}

fn base64_decode(s: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .context("base64 decode")
}

fn text_of(parent: &roxmltree::Node, tag: &str) -> Option<String> {
    parent
        .children()
        .find(|n| n.has_tag_name(tag))
        .and_then(|n| {
            let t = n.text()?.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        })
}

fn author_of(title_info: &roxmltree::Node) -> Option<String> {
    let authors: Vec<String> = title_info
        .children()
        .filter(|n| n.has_tag_name("author"))
        .filter_map(|a| {
            let first = text_of(&a, "first-name").unwrap_or_default();
            let middle = text_of(&a, "middle-name").unwrap_or_default();
            let last = text_of(&a, "last-name").unwrap_or_default();
            let nick = text_of(&a, "nickname").unwrap_or_default();
            let display_name = text_of(&a, "display-name").unwrap_or_default();
            let parts: Vec<String> = [first, middle, last, nick, display_name]
                .iter()
                .filter(|s| !s.is_empty())
                .cloned()
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(" "))
            }
        })
        .collect();
    if authors.is_empty() {
        None
    } else {
        Some(authors.join(", "))
    }
}

fn first_section_title(body: roxmltree::Node<'_, '_>) -> Option<String> {
    let title = body.descendants().find(|n| n.has_tag_name("title"))?;
    let text = title
        .descendants()
        .filter(|n| n.is_text())
        .filter_map(|n| n.text())
        .collect::<Vec<_>>()
        .join(" ");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

fn element_id_attr(node: roxmltree::Node) -> String {
    match node.attribute("id") {
        Some(id) => format!(" id=\"{}\"", escape_html(id)),
        None => String::new(),
    }
}

fn body_node_to_html(node: roxmltree::Node) -> String {
    let mut h = 0u32;
    node.children()
        .map(|c| node_to_html(c, &HashMap::new(), &mut h))
        .collect()
}

fn node_to_html(
    node: roxmltree::Node,
    binary: &HashMap<String, Vec<u8>>,
    h_level: &mut u32,
) -> String {
    if node.is_text() {
        return escape_text(node.text().unwrap_or(""));
    }
    if !node.is_element() {
        return String::new();
    }
    let name = node.tag_name().name();
    match name {
        "p" => format!(
            "<p{}>{}</p>",
            element_id_attr(node),
            inner_html(node, binary, h_level)
        ),
        "empty-line" => "<br/><br/>".to_string(),
        "title" => {
            *h_level = (*h_level + 1).min(6);
            let level = *h_level;
            let inner = inner_html(node, binary, h_level);
            *h_level = (*h_level).saturating_sub(1);
            format!("<h{level}{}>{inner}</h{level}>", element_id_attr(node))
        }
        "subtitle" => {
            *h_level = (*h_level + 1).min(6);
            let level = *h_level;
            let inner = inner_html(node, binary, h_level);
            *h_level = (*h_level).saturating_sub(1);
            format!(
                "<h{level} class=\"cre-subtitle\"{}>{inner}</h{level}>",
                element_id_attr(node)
            )
        }
        "cite" => format!(
            "<blockquote{}>{}</blockquote>",
            element_id_attr(node),
            inner_html(node, binary, h_level)
        ),
        "poem" => format!(
            "<div class=\"poem\"{}>{}</div>",
            element_id_attr(node),
            inner_html(node, binary, h_level)
        ),
        "stanza" => format!(
            "<div class=\"stanza\"{}>{}</div>",
            element_id_attr(node),
            inner_html(node, binary, h_level)
        ),
        "text-author" => format!(
            "<p class=\"cre-author\"{}>{}</p>",
            element_id_attr(node),
            inner_html(node, binary, h_level)
        ),
        "strong" => format!("<strong>{}</strong>", inner_html(node, binary, h_level)),
        "emphasis" => format!("<em>{}</em>", inner_html(node, binary, h_level)),
        "strikethrough" => format!("<s>{}</s>", inner_html(node, binary, h_level)),
        "sub" => format!("<sub>{}</sub>", inner_html(node, binary, h_level)),
        "sup" => format!("<sup>{}</sup>", inner_html(node, binary, h_level)),
        "code" => format!("<code>{}</code>", inner_html(node, binary, h_level)),
        "anchor" => {
            let id = node.attribute("id").unwrap_or("");
            format!("<a id=\"{id}\"></a>")
        }
        "a" => {
            let href = node.attribute("href").unwrap_or("");
            let inner = inner_html(node, binary, h_level);
            format!("<a href=\"{href}\">{inner}</a>")
        }
        "image" => {
            let href = node.attribute("href").unwrap_or("");
            if binary.contains_key(href) {
                format!("<img src=\"fb2://binary/{href}\" alt=\"\">")
            } else {
                format!("<img src=\"fb2://binary/{href}\" alt=\"[missing image: {href}]\">")
            }
        }
        "epigraph" => format!(
            "<blockquote class=\"epigraph\"{}>{}</blockquote>",
            element_id_attr(node),
            inner_html(node, binary, h_level)
        ),
        "annotation" => format!(
            "<div class=\"cre-annotation\">{}</div>",
            inner_html(node, binary, h_level)
        ),
        "section" => format!(
            "<section{}>{}</section>",
            element_id_attr(node),
            inner_html(node, binary, h_level)
        ),
        _ => inner_html(node, binary, h_level),
    }
}

fn inner_html(
    node: roxmltree::Node,
    binary: &HashMap<String, Vec<u8>>,
    h_level: &mut u32,
) -> String {
    node.children()
        .map(|c| node_to_html(c, binary, h_level))
        .collect()
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_fb2() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<FictionBook xmlns="http://www.gribnau.de/Books/FictionBook/2">
  <description>
    <title-info>
      <book-title>Test Book</book-title>
      <lang>en</lang>
      <author><first-name>Jane</first-name><last-name>Doe</last-name></author>
    </title-info>
  </description>
  <body>
    <section>
      <title><p>Chapter 1</p></title>
      <p>Hello <strong>world</strong>.</p>
      <empty-line/>
      <p>Bye</p>
    </section>
  </body>
</FictionBook>"#;
        let b = Fb2Book::from_xml(xml).unwrap();
        assert_eq!(b.title.as_deref(), Some("Test Book"));
        assert_eq!(b.creator.as_deref(), Some("Jane Doe"));
        let html = b.to_html(&Fb2HtmlOptions::default()).unwrap();
        assert!(html.contains("Chapter 1"));
        assert!(html.contains("<strong>world</strong>"));
        assert!(html.contains("Hello"));
    }

    #[test]
    fn parse_fb2_with_image() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<FictionBook xmlns="http://www.gribnau.de/Books/FictionBook/2">
  <description><title-info><book-title>Img Test</book-title><lang>en</lang></title-info></description>
  <body><p>Before<image href="img1.png"/>After</p></body>
  <binary name="img1.png" content-type="image/png">iVBORw0KGgo=</binary>
</FictionBook>"#;
        let b = Fb2Book::from_xml(xml).unwrap();
        assert!(b.binary.contains_key("img1.png"));
        let html = b.to_html(&Fb2HtmlOptions::default()).unwrap();
        assert!(html.contains("fb2://binary/img1.png"));
    }

    #[test]
    fn parses_real_fb2_fixture() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/koreader-test-data/sample.fb2"
        );
        let book = Fb2Book::open(path).unwrap();
        assert_eq!(book.title.as_deref(), Some("FB2 Fixture"));
        assert_eq!(book.creator.as_deref(), Some("Test M. Author"));
        assert_eq!(book.language.as_deref(), Some("en"));
        assert!(book.binary.contains_key("pixel.png"));

        let html = book.to_html(&Fb2HtmlOptions::default()).unwrap();
        assert!(html.len() > 1000);
        assert!(html.contains("fb2://binary/pixel.png"));
        assert!(html.contains("Chapter One"));
    }
}

use anyhow::{Context, Result};
use cre_book::html::escape_html;
use cre_book::text::decode_plain_text;
use std::path::Path;

pub struct TextBook {
    pub title: Option<String>,
    pub text: String,
}

impl TextBook {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let bytes = std::fs::read(path.as_ref()).context("read txt")?;
        let title = path
            .as_ref()
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());
        let text = decode_plain_text(&bytes);
        Ok(Self { title, text })
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            title: None,
            text: decode_plain_text(bytes),
        }
    }

    pub fn to_html(&self, opts: &TextHtmlOptions) -> String {
        let css = format!(
            "/* cre-render-base */\nhtml {{ font-size: {}px; }}\n\
             body {{ font-family: serif; font-size: 1em; line-height: {}; padding: {}px {}px; background: white; color: black; text-align: justify; }}\n\
             .cre-heading {{ font-weight: bold; font-size: 1.2em; }}",
            cre_css::FONT_SIZE,
            opts.line_height,
            opts.margin_y,
            opts.margin_x,
        );
        let title = self.title.as_deref().unwrap_or("");
        let title_html = escape_html(title);

        let mut body_parts: Vec<String> = Vec::new();

        // Split into blank-line-delimited blocks; within a block, join lines with spaces (reflow)
        for block in self.text.split("\n\n").chain(std::iter::once("")) {
            let block = block.trim_end();
            if block.trim().is_empty() {
                continue;
            }
            // Detect all-caps "heading" lines (common in Project Gutenberg)
            let first_line = block.lines().next().unwrap_or("");
            let is_heading = first_line.trim().len() < 80
                && !first_line.trim().is_empty()
                && first_line.trim().chars().all(|c| !c.is_lowercase());
            if is_heading {
                let heading = escape_html(first_line.trim());
                let rest: Vec<&str> = block.lines().skip(1).collect();
                let rest_html = reflow(&rest);
                body_parts.push(format!("<div class=\"cre-heading\">{heading}</div>"));
                if !rest_html.is_empty() {
                    body_parts.push(format!("<p>{rest_html}</p>"));
                }
            } else {
                let lines: Vec<&str> = block.lines().collect();
                let content = reflow(&lines);
                if !content.is_empty() {
                    body_parts.push(format!("<p>{content}</p>"));
                }
            }
        }

        let body_html = body_parts.join("\n");
        format!(
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>{title_html}</title><style>{css}</style></head><body>{body_html}</body></html>"
        )
    }
}

fn reflow(lines: &[&str]) -> String {
    let joined = lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    escape_html(&joined)
}

pub struct TextHtmlOptions {
    pub line_height: f64,
    pub margin_x: u32,
    pub margin_y: u32,
}

impl Default for TextHtmlOptions {
    fn default() -> Self {
        Self {
            line_height: 1.5,
            margin_x: 40,
            margin_y: 32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sample_txt() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/koreader-test-data/sample.txt"
        );
        let b = TextBook::open(path).unwrap();
        assert!(b.title.is_some());
        let html = b.to_html(&TextHtmlOptions::default());
        assert!(html.contains("Lorem ipsum"));
        assert!(html.contains("<p>"));
    }

    #[test]
    fn reflows_paragraph() {
        let b = TextBook {
            title: None,
            text: "line one\nline two\n\nsecond para".into(),
        };
        let html = b.to_html(&TextHtmlOptions::default());
        assert!(html.contains("line one line two"));
        assert!(html.contains("second para"));
    }

    #[test]
    fn strips_bom() {
        let b = TextBook::from_bytes(b"\xef\xbb\xbfHello");
        assert_eq!(b.text, "Hello");
    }
}

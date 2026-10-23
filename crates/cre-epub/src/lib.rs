use std::collections::HashMap;
use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result, bail};
use percent_encoding::percent_decode_str;

const CONTAINER_PATH: &str = "META-INF/container.xml";

#[derive(Clone, Debug)]
pub struct ManifestItem {
    pub id: String,
    pub href: String,
    pub media_type: String,
    pub properties: String,
}

#[derive(Clone, Debug)]
pub struct SpineItem {
    pub idref: String,
    pub href: String,
    pub media_type: String,
    pub linear: bool,
}

#[derive(Clone, Debug)]
pub struct SpineDocument {
    pub index: usize,
    pub id: String,
    pub href: String,
    pub media_type: String,
    pub body_html: String,
    pub css_urls: Vec<String>,
    pub inline_styles: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct TocItem {
    pub title: String,
    /// Resolved resource path with optional `#fragment`.
    pub href: String,
    pub depth: usize,
}

#[derive(Clone, Debug)]
pub struct HtmlOptions {
    pub font_size: f32,
    pub line_height: f32,
    pub margin_x: f32,
    pub margin_y: f32,
}

impl Default for HtmlOptions {
    fn default() -> Self {
        Self {
            font_size: 18.0,
            line_height: 1.5,
            margin_x: 40.0,
            margin_y: 32.0,
        }
    }
}

pub struct EpubBook {
    pub title: Option<String>,
    pub creator: Option<String>,
    pub language: Option<String>,
    pub opf_path: String,
    pub opf_dir: String,
    pub manifest: Vec<ManifestItem>,
    pub spine: Vec<SpineItem>,
    pub toc: Vec<TocItem>,
    pub resources: HashMap<String, Vec<u8>>,
}

impl EpubBook {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let data = std::fs::read(path.as_ref())
            .with_context(|| format!("read epub {}", path.as_ref().display()))?;
        Self::from_bytes(data)
    }

    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        let mut archive = zip::ZipArchive::new(Cursor::new(data)).context("open epub zip")?;
        let mut resources = HashMap::new();

        for i in 0..archive.len() {
            let mut file = archive.by_index(i).context("read zip entry")?;
            if file.is_dir() {
                continue;
            }
            let name = normalize_path(file.name());
            let mut bytes = Vec::with_capacity(file.size() as usize);
            std::io::copy(&mut file, &mut bytes).context("decompress zip entry")?;
            resources.insert(name, bytes);
        }

        let container = resources
            .get(CONTAINER_PATH)
            .context("epub has no META-INF/container.xml")?;
        let opf_path = parse_container(container)?;
        let opf = resources
            .get(&opf_path)
            .with_context(|| format!("epub package document missing: {opf_path}"))?;
        let opf_text = decode_text(opf);
        let opf_dir = parent_dir(&opf_path).to_string();

        let package = roxmltree::Document::parse(&opf_text).context("parse epub package")?;
        let root = package.root_element();

        let title = first_child_text(root, "title");
        let creator = first_child_text(root, "creator");
        let language = first_child_text(root, "language");

        let manifest_node = root
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == "manifest")
            .context("epub package has no manifest")?;
        let mut manifest = Vec::new();
        for item in manifest_node
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "item")
        {
            let id = item.attribute("id").unwrap_or_default().to_string();
            let href = item.attribute("href").unwrap_or_default().to_string();
            let media_type = item.attribute("media-type").unwrap_or_default().to_string();
            let properties = item.attribute("properties").unwrap_or_default().to_string();
            if id.is_empty() || href.is_empty() {
                continue;
            }
            manifest.push(ManifestItem {
                id,
                href: resolve_href(&opf_dir, &href),
                media_type,
                properties,
            });
        }

        let spine_node = root
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == "spine")
            .context("epub package has no spine")?;
        let mut spine = Vec::new();
        for itemref in spine_node
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "itemref")
        {
            let idref = itemref.attribute("idref").unwrap_or_default();
            let linear = itemref.attribute("linear") != Some("no");
            let Some(item) = manifest.iter().find(|item| item.id == idref) else {
                continue;
            };
            spine.push(SpineItem {
                idref: idref.to_string(),
                href: item.href.clone(),
                media_type: item.media_type.clone(),
                linear,
            });
        }

        if spine.is_empty() {
            bail!("epub spine is empty");
        }

        let toc = parse_toc(opf, spine_node, &manifest, &opf_dir, &resources);

        Ok(Self {
            title,
            creator,
            language,
            opf_path,
            opf_dir,
            manifest,
            spine,
            toc,
            resources,
        })
    }

    pub fn resource(&self, path: &str) -> Option<&[u8]> {
        self.resources
            .get(&normalize_path(path))
            .map(|v| v.as_slice())
    }

    pub fn resource_url(&self, path: &str) -> String {
        format!("epub://book/{}", encode_path(&normalize_path(path)))
    }

    pub fn spine_documents(&self) -> Result<Vec<SpineDocument>> {
        let mut docs = Vec::new();
        for (index, item) in self.spine.iter().enumerate() {
            if !item.linear {
                continue;
            }
            let bytes = self
                .resource(&item.href)
                .with_context(|| format!("spine document missing: {}", item.href))?;
            let text = decode_text(bytes);
            let body = extract_body(&text)
                .with_context(|| format!("spine document has no <body>: {}", item.href))?;
            let (css_urls, inline_styles) = extract_styles(&text, &self.opf_dir, &item.href);
            docs.push(SpineDocument {
                index,
                id: item.idref.clone(),
                href: item.href.clone(),
                media_type: item.media_type.clone(),
                body_html: rewrite_urls(&body, parent_dir(&item.href), &self.opf_dir),
                css_urls,
                inline_styles,
            });
        }
        Ok(docs)
    }

    pub fn to_html(&self, options: &HtmlOptions) -> Result<String> {
        let docs = self.spine_documents()?;
        let mut html = self.html_head(options, &docs);
        for doc in &docs {
            push_section_html(&mut html, doc);
        }
        html.push_str("</body></html>");
        Ok(html)
    }

    pub fn to_html_chunks(&self, options: &HtmlOptions, max_bytes: usize) -> Result<Vec<String>> {
        let docs = self.spine_documents()?;
        let head = self.html_head(options, &docs);
        let mut chunks = Vec::new();
        let mut body = String::new();
        for doc in &docs {
            let mut section = String::new();
            push_section_html(&mut section, doc);
            if !body.is_empty() && body.len() + section.len() > max_bytes {
                let mut html = head.clone();
                html.push_str(&body);
                html.push_str("</body></html>");
                chunks.push(html);
                body.clear();
            }
            body.push_str(&section);
        }
        if !body.is_empty() || chunks.is_empty() {
            let mut html = head;
            html.push_str(&body);
            html.push_str("</body></html>");
            chunks.push(html);
        }
        Ok(chunks)
    }

    fn html_head(&self, options: &HtmlOptions, docs: &[SpineDocument]) -> String {
        let mut css_urls = Vec::new();
        let mut inline_styles = Vec::new();
        for doc in docs {
            for url in &doc.css_urls {
                if !css_urls.contains(url) {
                    css_urls.push(url.clone());
                }
            }
            for style in &doc.inline_styles {
                if !inline_styles.contains(style) {
                    inline_styles.push(style.clone());
                }
            }
        }

        let mut html = String::new();
        html.push_str("<!doctype html><html><head><meta charset=\"utf-8\">");
        html.push_str(&format!(
            "<title>{}</title>",
            escape_html(self.title.as_deref().unwrap_or(""))
        ));
        for url in &css_urls {
            html.push_str(&format!(
                "<link rel=\"stylesheet\" href=\"{}\">",
                escape_attr(url)
            ));
        }
        for style in &inline_styles {
            html.push_str("<style>");
            html.push_str(style);
            html.push_str("</style>");
        }
        html.push_str("<style>");
        html.push_str(&format!(
            "html, body {{ margin: 0; padding: 0; }}\n\
             body {{ font-family: serif; font-size: {}px; line-height: {}; padding: {}px {}px; }}\n\
             img {{ max-width: 100%; height: auto; }}\n\
             .cre-spine {{ display: block; }}\n",
            options.font_size, options.line_height, options.margin_y, options.margin_x
        ));
        html.push_str("</style></head><body>");
        html
    }
}

fn push_section_html(out: &mut String, doc: &SpineDocument) {
    out.push_str(&format!(
        "<section class=\"cre-spine\" data-spine-index=\"{}\" data-spine-id=\"{}\" data-spine-href=\"{}\">",
        doc.index,
        escape_attr(&doc.id),
        escape_attr(&normalize_path(&doc.href))
    ));
    out.push_str(&doc.body_html);
    out.push_str("</section>");
}

fn parse_container(bytes: &[u8]) -> Result<String> {
    let text = decode_text(bytes);
    let doc = roxmltree::Document::parse(&text).context("parse META-INF/container.xml")?;
    let rootfile = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "rootfile")
        .context("container.xml has no rootfile")?;
    let path = rootfile
        .attribute("full-path")
        .context("container.xml rootfile has no full-path")?;
    Ok(normalize_path(path))
}

fn first_child_text(root: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    root.descendants()
        .find(|n| n.is_element() && n.tag_name().name() == name)
        .and_then(|n| n.text())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn extract_body(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let body_start = lower.find("<body")?;
    let open_end = lower[body_start..].find('>')? + body_start + 1;
    let close_start = lower.rfind("</body")?;
    if close_start <= open_end {
        return None;
    }
    Some(html[open_end..close_start].to_string())
}

fn extract_styles(html: &str, opf_dir: &str, doc_path: &str) -> (Vec<String>, Vec<String>) {
    let lower = html.to_ascii_lowercase();
    let head_end = lower.find("</head").unwrap_or(html.len());
    let head = &html[..head_end];
    let doc_dir = parent_dir(doc_path);
    let mut css_urls = Vec::new();
    let mut styles = Vec::new();

    let mut cursor = 0;
    while let Some(pos) = head[cursor..].find("<link") {
        let start = cursor + pos;
        let end = head[start..]
            .find('>')
            .map(|p| p + start + 1)
            .unwrap_or(head.len());
        let tag = &head[start..end];
        let attrs = parse_attrs(tag);
        let rel = attrs
            .get("rel")
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        if rel.split_ascii_whitespace().any(|r| r == "stylesheet")
            && let Some(href) = attrs.get("href")
        {
            let path = resolve_href(doc_dir, href);
            css_urls.push(format!(
                "epub://book/{}",
                encode_path(&normalize_path(&path))
            ));
        }
        cursor = end;
    }

    let mut cursor = 0;
    while let Some(pos) = lower[cursor..].find("<style") {
        let start = cursor + pos;
        let text_start = lower[start..]
            .find('>')
            .map(|p| p + start + 1)
            .unwrap_or(start);
        let Some(close) = lower[text_start..].find("</style") else {
            break;
        };
        let text_end = text_start + close;
        let css = clean_css(&html[text_start..text_end]);
        if !css.is_empty() {
            styles.push(css);
        }
        cursor = text_end + 8;
    }

    let _ = opf_dir;
    (css_urls, styles)
}

fn clean_css(raw: &str) -> String {
    let mut css = raw.trim();
    loop {
        let mut changed = false;
        if let Some(rest) = css.strip_prefix("<![CDATA[") {
            css = rest.trim();
            changed = true;
        }
        if let Some(rest) = css.strip_prefix("<!--") {
            css = rest.trim();
            changed = true;
        }
        if let Some(rest) = css.strip_suffix("]]>") {
            css = rest.trim_end();
            changed = true;
        }
        if let Some(rest) = css.strip_suffix("-->") {
            css = rest.trim_end();
            changed = true;
        }
        if !changed {
            break;
        }
    }
    css.to_string()
}

fn rewrite_urls(body: &str, doc_dir: &str, opf_dir: &str) -> String {
    let mut out = String::with_capacity(body.len() + 128);
    let mut i = 0;
    while i < body.len() {
        let Some(rel) = body[i..].find('<') else {
            out.push_str(&body[i..]);
            break;
        };
        let tag_start = i + rel;
        out.push_str(&body[i..tag_start]);
        let Some(rel_end) = body[tag_start..].find('>') else {
            out.push_str(&body[tag_start..]);
            break;
        };
        let tag_end = tag_start + rel_end + 1;
        let tag = &body[tag_start..tag_end];
        out.push_str(&rewrite_tag(tag, doc_dir, opf_dir));
        i = tag_end;
    }
    out
}

fn rewrite_tag(tag: &str, doc_dir: &str, opf_dir: &str) -> String {
    if tag.starts_with("<!") || tag.starts_with("<?") || tag.starts_with("</") {
        return tag.to_string();
    }
    let mut out = String::with_capacity(tag.len() + 64);
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
            let name = &tag[start..i];
            let lower = name.to_ascii_lowercase();
            out.push_str(name);
            if lower == "src" || lower == "href" || lower == "poster" || lower == "data" {
                while i < tag.len() && tag[i..].chars().next().is_some_and(|c| c.is_whitespace()) {
                    out.push(tag[i..].chars().next().unwrap());
                    i += 1;
                }
                if i < tag.len() && tag[i..].starts_with('=') {
                    out.push('=');
                    i += 1;
                    while i < tag.len()
                        && tag[i..].chars().next().is_some_and(|c| c.is_whitespace())
                    {
                        out.push(tag[i..].chars().next().unwrap());
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
                            let value = &tag[value_start..i];
                            let rewritten = if is_external(value) {
                                value.to_string()
                            } else {
                                let path = resolve_href(doc_dir, &value_without_fragment(value));
                                let mut url =
                                    format!("epub://book/{}", encode_path(&normalize_path(&path)));
                                if let Some(fragment) = value.split_once('#').map(|(_, f)| f) {
                                    url.push('#');
                                    url.push_str(fragment);
                                }
                                url
                            };
                            out.push(quote);
                            out.push_str(&escape_attr(&rewritten));
                            out.push(quote);
                            if i < tag.len() {
                                i += 1;
                            }
                        } else {
                            let value_start = i;
                            while i < tag.len() {
                                let ch = tag[i..].chars().next().unwrap();
                                if ch.is_whitespace() || ch == '>' {
                                    break;
                                }
                                i += ch.len_utf8();
                            }
                            let value = &tag[value_start..i];
                            let rewritten = if is_external(value) {
                                value.to_string()
                            } else {
                                let path = resolve_href(doc_dir, &value_without_fragment(value));
                                let mut url =
                                    format!("epub://book/{}", encode_path(&normalize_path(&path)));
                                if let Some(fragment) = value.split_once('#').map(|(_, f)| f) {
                                    url.push('#');
                                    url.push_str(fragment);
                                }
                                url
                            };
                            out.push_str(&escape_attr(&rewritten));
                        }
                    }
                }
            }
        } else {
            out.push(c);
            i += c.len_utf8();
        }
    }
    let _ = opf_dir;
    out
}

fn parse_attrs(tag: &str) -> HashMap<String, String> {
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

fn is_external(value: &str) -> bool {
    let value = value.trim();
    value.is_empty()
        || value.starts_with('#')
        || value.starts_with('/')
        || value.starts_with("data:")
        || value.starts_with("mailto:")
        || value.contains("://")
        || value.starts_with("javascript:")
}

fn value_without_fragment(value: &str) -> String {
    value.split('#').next().unwrap_or(value).to_string()
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

fn parse_toc(
    ncx_bytes_hint: &[u8],
    spine_node: roxmltree::Node<'_, '_>,
    manifest: &[ManifestItem],
    opf_dir: &str,
    resources: &HashMap<String, Vec<u8>>,
) -> Vec<TocItem> {
    let _ = ncx_bytes_hint;
    let _ = opf_dir;
    let mut out = Vec::new();

    if let Some(toc_id) = spine_node.attribute("toc")
        && let Some(item) = manifest.iter().find(|item| item.id == toc_id)
        && let Some(bytes) = resources.get(&item.href)
    {
        parse_ncx(bytes, parent_dir(&item.href), &mut out);
    }

    if out.is_empty() {
        let nav_item = manifest
            .iter()
            .find(|item| item.properties.split_whitespace().any(|prop| prop == "nav"));
        if let Some(item) = nav_item
            && let Some(bytes) = resources.get(&item.href)
        {
            parse_epub3_nav(bytes, parent_dir(&item.href), &mut out);
        }
    }

    out
}

fn parse_ncx(bytes: &[u8], base_dir: &str, out: &mut Vec<TocItem>) {
    let text = decode_text(bytes);
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let doc = match roxmltree::Document::parse_with_options(&text, options) {
        Ok(doc) => doc,
        Err(_) => return,
    };
    let root = doc.root_element();
    if root.tag_name().name() != "ncx" {
        return;
    }
    let Some(nav_map) = root
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "navMap")
    else {
        return;
    };
    for nav_point in nav_map
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "navPoint")
    {
        parse_ncx_point(nav_point, 1, base_dir, out);
    }
}

fn parse_ncx_point(
    node: roxmltree::Node<'_, '_>,
    depth: usize,
    base_dir: &str,
    out: &mut Vec<TocItem>,
) {
    let title = child_text(node, "navLabel").unwrap_or_default();
    let href = node
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "content")
        .and_then(|n| n.attribute("src"))
        .unwrap_or_default();
    let href = resolve_toc_href(base_dir, href);
    if !href.is_empty() {
        out.push(TocItem { title, href, depth });
    }
    for child in node
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "navPoint")
    {
        parse_ncx_point(child, depth + 1, base_dir, out);
    }
}

fn parse_epub3_nav(bytes: &[u8], base_dir: &str, out: &mut Vec<TocItem>) {
    let text = decode_text(bytes);
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let Ok(doc) = roxmltree::Document::parse_with_options(&text, options) else {
        return;
    };
    let root = doc.root_element();
    let Some(nav) = root
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "nav" && has_epub_type(*n, "toc"))
    else {
        return;
    };
    let Some(ol) = nav
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "ol")
    else {
        return;
    };
    parse_nav_ol(ol, 1, base_dir, out);
}

fn parse_nav_ol(ol: roxmltree::Node<'_, '_>, depth: usize, base_dir: &str, out: &mut Vec<TocItem>) {
    for li in ol
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "li")
    {
        if let Some(anchor) = li
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == "a")
        {
            let title = descendant_text(anchor).unwrap_or_default();
            let href = resolve_toc_href(base_dir, anchor.attribute("href").unwrap_or_default());
            if !href.is_empty() {
                out.push(TocItem { title, href, depth });
            }
        }
        for child in li
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "ol")
        {
            parse_nav_ol(child, depth + 1, base_dir, out);
        }
    }
}

fn child_text(node: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == name)
        .and_then(descendant_text)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn descendant_text(node: roxmltree::Node<'_, '_>) -> Option<String> {
    let text: String = node
        .descendants()
        .filter(|n| n.is_text())
        .filter_map(|n| n.text())
        .collect();
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn has_epub_type(node: roxmltree::Node<'_, '_>, value: &str) -> bool {
    node.attributes().any(|attr| {
        let matches = match attr.namespace() {
            Some(uri) => {
                uri == "http://www.idpf.org/2007/ops"
                    && (attr.name() == "type" || attr.name().ends_with(":type"))
            }
            None => attr.name() == "epub:type",
        };
        matches && attr.value().split_ascii_whitespace().any(|v| v == value)
    })
}

fn split_fragment(href: &str) -> (&str, Option<&str>) {
    match href.find('#') {
        Some(index) => (&href[..index], Some(&href[index + 1..])),
        None => (href, None),
    }
}

fn resolve_toc_href(base_dir: &str, href: &str) -> String {
    let href = href.trim();
    let (path, fragment) = split_fragment(href);
    if path.is_empty() {
        return fragment.map_or_else(String::new, |fragment| format!("#{fragment}"));
    }
    let resolved = resolve_href(base_dir, path);
    match fragment {
        Some(fragment) => format!("{resolved}#{fragment}"),
        None => resolved,
    }
}

fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

fn encode_path(path: &str) -> String {
    path.replace('%', "%25")
        .replace(' ', "%20")
        .replace('#', "%23")
        .replace('?', "%3F")
}

fn decode_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attr(value: &str) -> String {
    value.replace('&', "&amp;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn juliet() -> EpubBook {
        EpubBook::open("../../fixtures/koreader-test-data/juliet.epub").unwrap()
    }

    #[test]
    fn parses_juliet_metadata_and_spine() {
        let book = juliet();
        assert_eq!(book.title.as_deref(), Some("Romeo and Juliet"));
        assert!(book.spine.len() > 20);
        assert_eq!(book.spine[0].href, "OPS/cover.xml");
        let docs = book.spine_documents().unwrap();
        assert!(docs.iter().any(|doc| doc.body_html.contains("SAMPSON")));
    }

    #[test]
    fn parses_juliet_ncx_toc() {
        let book = juliet();
        assert!(book.toc.len() > 20);
        assert_eq!(book.toc[0].title, "Title");
        assert_eq!(book.toc[0].href, "OPS/title.xml");
        assert_eq!(book.toc[0].depth, 1);
        assert!(
            book.toc
                .iter()
                .any(|item| item.depth > 1 && item.href.contains("#section_"))
        );
    }

    #[test]
    fn builds_combined_html_with_rewritten_resources() {
        let book = juliet();
        let html = book.to_html(&HtmlOptions::default()).unwrap();
        assert!(html.contains("epub://book/OPS/css/main.css"));
        assert!(html.contains("epub://book/OPS/images/logo-feedbooks-tiny.png"));
        assert!(html.contains("data-spine-index=\"1\""));
        assert!(html.contains("data-spine-href=\"OPS/cover.xml\""));
        assert!(html.contains("Romeo and Juliet"));
    }

    #[test]
    fn parses_epub3_nav_toc_from_synthetic_epub() {
        let book = EpubBook::from_bytes(synthetic_epub3_nav()).unwrap();
        assert_eq!(book.toc.len(), 2);
        assert_eq!(book.toc[0].title, "Chapter One");
        assert_eq!(book.toc[0].href, "sec1.xhtml#sec1");
        assert_eq!(book.toc[0].depth, 1);
        assert_eq!(book.toc[1].title, "Nested");
        assert_eq!(book.toc[1].href, "sec1.xhtml#sub");
        assert_eq!(book.toc[1].depth, 2);
    }

    #[test]
    fn strips_cdata_and_comment_wrappers_from_inline_styles() {
        assert_eq!(
            clean_css("<![CDATA[\n   p { color: red; }\n]]>"),
            "p { color: red; }"
        );
        assert_eq!(clean_css("<!-- p { color: red; } -->"), "p { color: red; }");
        assert_eq!(clean_css("  \n p { a: b } \n "), "p { a: b }");
        assert_eq!(clean_css("<![CDATA[<!-- p { a: b } -->]]>"), "p { a: b }");
        assert_eq!(clean_css(""), "");
    }

    #[test]
    fn combined_html_has_cdata_free_inline_styles() {
        let book = EpubBook::from_bytes(synthetic_cdata_styled_epub()).unwrap();
        let html = book.to_html(&HtmlOptions::default()).unwrap();
        assert!(html.contains("p.cdata-marker { color: #ff0000; }"));
        assert!(!html.contains("<![CDATA["));
        assert!(!html.contains("]]>"));
    }

    fn synthetic_cdata_styled_epub() -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            let options = zip::write::SimpleFileOptions::default();

            write_zip_file(&mut writer, options, "mimetype", "application/epub+zip");
            write_zip_file(
                &mut writer,
                options,
                "META-INF/container.xml",
                r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="pkg.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#,
            );
            write_zip_file(
                &mut writer,
                options,
                "pkg.opf",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:cdata-test</dc:identifier>
    <dc:title>CDATA Test</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="s1" href="sec1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="s1"/>
  </spine>
</package>"#,
            );
            write_zip_file(
                &mut writer,
                options,
                "sec1.xhtml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head>
    <title>Sec1</title>
    <style type="text/css"><![CDATA[
   p.cdata-marker { color: #ff0000; }
]]></style>
  </head>
  <body>
    <p class="cdata-marker">styled paragraph</p>
  </body>
</html>"#,
            );

            writer.finish().unwrap();
        }
        buf.into_inner()
    }

    fn synthetic_epub3_nav() -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            let options = zip::write::SimpleFileOptions::default();

            write_zip_file(&mut writer, options, "mimetype", "application/epub+zip");
            write_zip_file(
                &mut writer,
                options,
                "META-INF/container.xml",
                r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="pkg.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#,
            );
            write_zip_file(
                &mut writer,
                options,
                "pkg.opf",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:test</dc:identifier>
    <dc:title>Nav Test</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="s1" href="sec1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="s1"/>
  </spine>
</package>"#,
            );
            write_zip_file(
                &mut writer,
                options,
                "nav.xhtml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
  <head><title>Nav</title></head>
  <body>
    <nav epub:type="toc">
      <ol>
        <li>
          <a href="sec1.xhtml#sec1">Chapter One</a>
          <ol>
            <li><a href="sec1.xhtml#sub">Nested</a></li>
          </ol>
        </li>
      </ol>
    </nav>
  </body>
</html>"#,
            );
            write_zip_file(
                &mut writer,
                options,
                "sec1.xhtml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head><title>Sec1</title></head>
  <body>
    <h1 id="sec1">Chapter One</h1>
    <h2 id="sub">Nested</h2>
  </body>
</html>"#,
            );

            writer.finish().unwrap();
        }
        buf.into_inner()
    }

    fn write_zip_file(
        writer: &mut zip::ZipWriter<&mut Cursor<Vec<u8>>>,
        options: zip::write::SimpleFileOptions,
        name: &str,
        contents: &str,
    ) {
        use std::io::Write;
        writer.start_file(name, options).unwrap();
        writer.write_all(contents.as_bytes()).unwrap();
    }
}

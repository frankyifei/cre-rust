use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use anyrender::Scene;
use blitz_dom::{
    DocumentConfig, Node, NodeId, Point, StyleThreading, UserAgentStylesheet, build_single_font_ctx,
};
use blitz_html::HtmlDocument;
use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request};
use blitz_traits::shell::{ColorScheme, Viewport};
use cre_epub::{
    EpubBook, HtmlOptions, PageMapItem as RawPageMapItem, TocItem as RawTocItem, normalize_path,
};
use cre_fb2::{Fb2Book, Fb2HtmlOptions};
use cre_paint::render_page as paint_page;

pub use cre_paint::PageBitmap;
use cre_text::{TextBook, TextHtmlOptions};
use percent_encoding::percent_decode_str;

mod page_breaks;
mod position;
mod xpointer;

pub use position::{PathSegment, TextPosition, XPointerRange, parse_text_position};

const SEARCH_FLAGS_CASE_INSENSITIVE: i32 = 1 << 1;
const SEARCH_FLAGS_COMMENTS: i32 = 1 << 2;
const SEARCH_FLAGS_MULTILINE: i32 = 1 << 3;
const SEARCH_FLAGS_LITERAL: i32 = 1 << 4;
const SEARCH_FLAGS_DOTALL: i32 = 1 << 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentFormat {
    Epub,
    Fb2,
    Txt,
}

#[derive(Clone, Debug)]
pub struct DocumentMetadata {
    pub format: DocumentFormat,
    pub title: Option<String>,
    pub creator: Option<String>,
    pub language: Option<String>,
    pub series: Option<String>,
    pub description: Option<String>,
    pub keywords: Option<String>,
    pub identifiers: Option<String>,
    pub item_count: usize,
}

#[derive(Clone, Debug)]
pub struct DocumentOptions {
    pub width: u32,
    pub height: u32,
    pub layout_height: u32,
    pub page_margin: f32,
    pub font_size: f32,
    pub font_path: Option<PathBuf>,
    pub stylesheet: Option<String>,
    pub embedded_styles_enabled: bool,
    pub viewport_scale: f32,
}

impl Default for DocumentOptions {
    fn default() -> Self {
        Self {
            width: 600,
            height: 800,
            layout_height: 100_000,
            page_margin: 32.0,
            font_size: 18.0,
            font_path: None,
            stylesheet: None,
            embedded_styles_enabled: true,
            viewport_scale: 1.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TocEntry {
    pub title: String,
    pub href: String,
    pub depth: usize,
    pub page: Option<usize>,
    pub xpointer: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextBox {
    pub text: String,
    pub page: usize,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PagePosition {
    pub page: usize,
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit {
    pub page: usize,
    pub xpointer_range: XPointerRange,
    pub rects: Vec<TextBox>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HitResult {
    pub page: usize,
    pub text_position: TextPosition,
    pub text: String,
    pub rect: TextBox,
    pub link: Option<String>,
    point: PagePosition,
    line_index: usize,
    byte_offset: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Selection {
    pub text: String,
    pub xpointer_range: XPointerRange,
    pub rects: Vec<TextBox>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WordHit {
    pub text: String,
    pub xpointer_range: XPointerRange,
    pub rects: Vec<TextBox>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PageLink {
    pub href: String,
    pub internal: bool,
    pub rects: Vec<TextBox>,
}

fn render_length_to_i64(value: f32) -> i64 {
    if !value.is_finite() {
        return 0;
    }
    let bounded = value.max(0.0).round();
    bounded as i64
}

fn render_length_to_u64(value: f32) -> u64 {
    if !value.is_finite() {
        return 0;
    }
    let bounded = value.max(0.0).round();
    bounded as u64
}

#[derive(Clone, Debug, PartialEq)]
pub struct PageMapEntry {
    pub page: usize,
    pub xpointer: Option<String>,
    pub doc_y: i64,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bookmark {
    pub page: usize,
    pub xpointer: Option<String>,
}

#[derive(Debug)]
pub(crate) struct LineBox {
    pub(crate) node_id: NodeId,
    pub(crate) text_start: usize,
    pub(crate) text_end: usize,
    pub(crate) source_element_id: Option<NodeId>,
    pub(crate) left: f32,
    pub(crate) right: f32,
    pub(crate) top: f32,
    pub(crate) bottom: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PageStart {
    pub(crate) top: f32,
    pub(crate) clip_top: f32,
}

struct TocAnchorId {
    resource: String,
    fragment: Option<String>,
    node_id: NodeId,
}

const CHUNK_TARGET_BYTES: usize = 256 * 1024;

struct ChunkMeta {
    first_page: usize,
    line_base: usize,
}

struct ActiveChunk {
    index: usize,
    doc: HtmlDocument,
    lines: Vec<LineBox>,
    page_starts: Vec<PageStart>,
}

struct PageSnapshot {
    chunk: usize,
    local_start: PageStart,
    local_clip_bottom: f32,
    doc_top: f32,
    render_height: f32,
    boxes: Vec<SnapshotLine>,
    xpointer: Option<String>,
    page_flow: i64,
}

struct SnapshotLine {
    text: String,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    global_line: usize,
}

pub struct Document {
    metadata: DocumentMetadata,
    width: u32,
    height: u32,
    layout_height: u32,
    page_margin: f32,
    font_css_size: f32,
    viewport_scale: f32,
    stylesheet: Option<String>,
    embedded_styles_enabled: bool,
    sources: Vec<String>,
    base_url: String,
    resources: Arc<HashMap<String, Vec<u8>>>,
    cover_image: Option<Vec<u8>>,
    has_non_linear_flows: bool,
    net: Arc<dyn NetProvider>,
    font_data: Vec<u8>,
    chunks: Vec<ChunkMeta>,
    pages: Vec<PageSnapshot>,
    xpointer_pages: HashMap<String, usize>,
    href_pages: HashMap<(String, Option<String>), usize>,
    spine_to_chunk: HashMap<u32, usize>,
    toc: Vec<TocEntry>,
    page_map_source: String,
    page_map: Vec<PageMapEntry>,
    active: RefCell<Option<ActiveChunk>>,
    current_page: usize,
}

impl Document {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::with_options(path, &DocumentOptions::default())
    }

    pub fn with_options(path: impl AsRef<Path>, options: &DocumentOptions) -> Result<Self> {
        validate_options(options)?;
        let path = path.as_ref();
        let format = detect_format(path);
        match format {
            DocumentFormat::Epub => {
                let mut book = EpubBook::open(path)?;
                let _ = book.normalize_inline_svg_images();
                let raw_toc = std::mem::take(&mut book.toc);
                let raw_page_map_source = std::mem::take(&mut book.page_map_source);
                let raw_page_map = std::mem::take(&mut book.page_map);
                let metadata = DocumentMetadata {
                    format,
                    title: book.title.clone(),
                    creator: book.creator.clone(),
                    language: book.language.clone(),
                    series: book.series.clone(),
                    description: book.description.clone(),
                    keywords: book.keywords.clone(),
                    identifiers: book.identifiers.clone(),
                    item_count: book.spine.len(),
                };
                let html_options = HtmlOptions {
                    font_size: options.font_size,
                    ..HtmlOptions::default()
                };
                let _ = book.normalize_full_page_images(
                    options.width as f32,
                    options.height as f32,
                    html_options.margin_x,
                    html_options.margin_y,
                );
                let htmls = book.to_html_chunks(&html_options, CHUNK_TARGET_BYTES)?;
                let cover_image = epub_cover_image(&book);
                let has_non_linear_flows = book.spine.iter().any(|item| !item.linear);
                let resources = Arc::new(book.resources);
                Self::build(
                    htmls,
                    "epub://book/",
                    Arc::new(EpubNet {
                        resources: resources.clone(),
                    }),
                    resources,
                    cover_image,
                    has_non_linear_flows,
                    metadata,
                    raw_toc,
                    raw_page_map_source,
                    raw_page_map,
                    options,
                )
            }
            DocumentFormat::Fb2 => {
                let book = Fb2Book::open(path)?;
                let metadata = DocumentMetadata {
                    format,
                    title: book.title.clone(),
                    creator: book.creator.clone(),
                    language: book.language.clone(),
                    series: None,
                    description: book.description.clone(),
                    keywords: None,
                    identifiers: book.isbn.clone().map(|isbn| format!("ISBN: {isbn}")),
                    item_count: book.binary.len(),
                };
                let html_options = Fb2HtmlOptions {
                    font_size: options.font_size.max(1.0) as u32,
                    ..Fb2HtmlOptions::default()
                };
                let (html, fb2_toc) = book.to_html_with_toc(&html_options)?;
                let raw_toc = fb2_toc
                    .into_iter()
                    .map(|item| RawTocItem {
                        title: item.title,
                        href: item.href,
                        depth: item.depth,
                    })
                    .collect();
                let cover_image = book.binary.values().next().cloned();
                let resources = Arc::new(book.binary);
                Self::build(
                    vec![html],
                    "fb2://book/",
                    Arc::new(Fb2Net {
                        resources: resources.clone(),
                    }),
                    resources,
                    cover_image,
                    false,
                    metadata,
                    raw_toc,
                    String::new(),
                    Vec::new(),
                    options,
                )
            }
            DocumentFormat::Txt => {
                let book = TextBook::open(path)?;
                let metadata = DocumentMetadata {
                    format,
                    title: book.title.clone(),
                    creator: None,
                    language: None,
                    series: None,
                    description: None,
                    keywords: None,
                    identifiers: None,
                    item_count: 1,
                };
                let html_options = TextHtmlOptions {
                    font_size: options.font_size.max(1.0) as u32,
                    ..TextHtmlOptions::default()
                };
                let html = book.to_html(&html_options);
                Self::build(
                    vec![html],
                    "text://book/",
                    Arc::new(EmptyNet),
                    Arc::new(HashMap::new()),
                    None,
                    false,
                    metadata,
                    Vec::new(),
                    String::new(),
                    Vec::new(),
                    options,
                )
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        htmls: Vec<String>,
        base_url: &str,
        net: Arc<dyn NetProvider>,
        resources: Arc<HashMap<String, Vec<u8>>>,
        cover_image: Option<Vec<u8>>,
        has_non_linear_flows: bool,
        metadata: DocumentMetadata,
        raw_toc: Vec<RawTocItem>,
        page_map_source: String,
        page_map_items: Vec<RawPageMapItem>,
        options: &DocumentOptions,
    ) -> Result<Self> {
        let font_path = resolve_font_path(options)?;
        let font_data = std::fs::read(&font_path)
            .with_context(|| format!("read font {}", font_path.display()))?;

        let mut chunks = Vec::with_capacity(htmls.len());
        let mut pages = Vec::new();
        let mut page_xpointers = Vec::new();
        let mut xpointer_pages = HashMap::new();
        let mut href_pages = HashMap::new();
        let mut spine_to_chunk = HashMap::new();
        let mut line_base = 0usize;
        let mut max_flow = 0i64;
        let mut running_doc_top = 0.0;

        for (chunk_index, html) in htmls.iter().enumerate() {
            let doc = create_document(html, base_url, net.clone(), &font_data, options);
            let root = doc.root_element();
            let lines = collect_lines(root);
            let mut line_indices: HashMap<NodeId, Vec<usize>> = HashMap::new();
            for (local_line, line) in lines.iter().enumerate() {
                line_indices
                    .entry(line.node_id)
                    .or_default()
                    .push(local_line);
            }
            let mut line_flows = lines.iter().map(|_| 0i64).collect::<Vec<i64>>();
            collect_line_flows(root, 0, &mut max_flow, &line_indices, &mut line_flows);
            let forced_breaks = page_breaks::collect_forced_breaks(root);
            let page_starts = paginate(
                &lines,
                &forced_breaks,
                options.height as f32,
                options.page_margin,
                options.page_margin,
            );
            let first_page = pages.len() + 1;
            let mut local_xpointer_pages = HashMap::new();
            let local_page_xpointers = xpointer::page_xpointers(
                root,
                &lines,
                &page_starts,
                options.height,
                options.page_margin,
                &mut local_xpointer_pages,
            );
            let anchors = collect_anchor_maps(root);
            for (key, node_id) in anchors {
                if let Ok(local_page) = page_for_node(&doc, node_id, &page_starts) {
                    href_pages.entry(key).or_insert(first_page + local_page - 1);
                }
            }
            collect_spine_chunks(root, chunk_index, &mut spine_to_chunk);

            let mut previous_page_flow = 0;
            for (local_index, start) in page_starts.iter().enumerate() {
                let clip_bottom = page_clip_bottom(&page_starts, local_index, options);
                let boxes = snapshot_page_boxes(root, &lines, *start, clip_bottom, line_base);
                let page_flow = if let Some(first) = boxes.first() {
                    let local_line = first.global_line.saturating_sub(line_base);
                    line_flows
                        .get(local_line)
                        .copied()
                        .unwrap_or(previous_page_flow)
                } else {
                    previous_page_flow
                };
                previous_page_flow = page_flow;
                let render_height = (clip_bottom - start.top).max(0.0);
                let doc_top = running_doc_top;
                running_doc_top += render_height;
                let xpointer = local_page_xpointers
                    .get(local_index)
                    .and_then(|xpointer| xpointer.clone());
                if let Some(xpointer) = &xpointer {
                    xpointer_pages.insert(xpointer.clone(), first_page + local_index);
                }
                page_xpointers.push(xpointer.clone());
                pages.push(PageSnapshot {
                    chunk: chunk_index,
                    local_start: *start,
                    local_clip_bottom: clip_bottom,
                    doc_top,
                    render_height,
                    boxes,
                    xpointer,
                    page_flow,
                });
            }

            chunks.push(ChunkMeta {
                first_page,
                line_base,
            });
            line_base += lines.len();
        }
        spine_to_chunk.entry(1).or_insert(0);

        let toc = raw_toc
            .into_iter()
            .map(|item| {
                let page = page_for_href(&href_pages, &item.href).ok();
                let xpointer = page.and_then(|page| {
                    page_xpointers
                        .get(page.saturating_sub(1))
                        .and_then(|xpointer| xpointer.clone())
                });
                TocEntry {
                    title: item.title,
                    href: item.href.clone(),
                    depth: item.depth,
                    page,
                    xpointer,
                }
            })
            .collect();

        let mut page_map: Vec<PageMapEntry> = page_map_items
            .into_iter()
            .filter_map(|item| {
                let page = page_for_href(&href_pages, &item.href).ok()?;
                let xpointer = page_xpointers
                    .get(page.saturating_sub(1))
                    .cloned()
                    .flatten();
                let doc_y = pages
                    .get(page.saturating_sub(1))
                    .map(|snapshot| snapshot.doc_top.max(0.0).round() as i64)
                    .unwrap_or(page.saturating_sub(1) as i64 * options.height as i64);
                Some(PageMapEntry {
                    page,
                    xpointer,
                    doc_y,
                    label: item.label,
                })
            })
            .collect();
        page_map.sort_by_key(|entry| (entry.doc_y, entry.page));

        let document = Self {
            metadata,
            width: options.width,
            height: options.height,
            layout_height: options.layout_height,
            page_margin: options.page_margin,
            stylesheet: options.stylesheet.clone(),
            embedded_styles_enabled: options.embedded_styles_enabled,
            font_css_size: options.font_size,
            viewport_scale: options.viewport_scale,
            sources: htmls,
            base_url: base_url.to_string(),
            resources,
            cover_image,
            has_non_linear_flows: has_non_linear_flows || max_flow > 0,
            net,
            font_data,
            chunks,
            pages,
            xpointer_pages,
            href_pages,
            spine_to_chunk,
            toc,
            page_map_source,
            page_map,
            active: RefCell::new(None),
            current_page: 1,
        };
        document.ensure_chunk(0)?;
        Ok(document)
    }

    pub fn metadata(&self) -> &DocumentMetadata {
        &self.metadata
    }

    pub fn page_count(&self) -> usize {
        self.pages.len().max(1)
    }

    pub fn page_width(&self) -> u32 {
        self.width
    }

    pub fn page_height(&self) -> u32 {
        self.height
    }

    pub fn page_margin_y(&self) -> i64 {
        self.page_margin.round() as i64
    }

    pub fn page_count_1based(&self) -> usize {
        self.page_count()
    }

    pub fn has_document_page_map(&self) -> bool {
        !self.page_map.is_empty()
    }

    pub fn document_page_map_source(&self) -> &str {
        &self.page_map_source
    }

    pub fn document_page_map(&self) -> &[PageMapEntry] {
        &self.page_map
    }

    pub fn full_height(&self) -> u64 {
        let mut max_bottom: f32 = 0.0;
        for snapshot in &self.pages {
            max_bottom = max_bottom.max(snapshot.doc_top + snapshot.render_height);
        }
        let calculated = render_length_to_u64(max_bottom);
        let fallback = u64::from(self.height);
        if calculated > fallback {
            calculated
        } else {
            fallback
        }
    }

    pub fn page_start_y(&self, page: usize) -> i64 {
        let index = page.saturating_sub(1);
        self.pages
            .get(index)
            .map(|snapshot| render_length_to_i64(snapshot.doc_top))
            .unwrap_or(0)
    }

    pub fn page_start_y_or_invalid(&self, page: usize) -> i64 {
        let Some(index) = page.checked_sub(1) else {
            return -1;
        };
        self.pages
            .get(index)
            .map(|snapshot| render_length_to_i64(snapshot.doc_top))
            .unwrap_or(-1)
    }

    pub fn page_render_height(&self, page: usize) -> i64 {
        let index = page.saturating_sub(1);
        self.pages
            .get(index)
            .map(|snapshot| render_length_to_i64(snapshot.render_height))
            .unwrap_or(i64::from(self.height))
    }

    pub fn current_pos(&self) -> i64 {
        self.page_start_y(self.current_page)
    }

    pub fn current_page(&self) -> usize {
        self.current_page
    }

    pub fn toc(&self) -> &[TocEntry] {
        &self.toc
    }

    pub fn cover_image(&self) -> Option<&[u8]> {
        self.cover_image.as_deref()
    }

    pub fn has_non_linear_flows(&self) -> bool {
        self.has_non_linear_flows
    }

    pub fn page_flow(&self, page: usize) -> i64 {
        let Some(index) = page.checked_sub(1) else {
            return 0;
        };
        self.pages
            .get(index)
            .map(|snapshot| snapshot.page_flow)
            .unwrap_or(0)
    }

    pub fn resource(&self, path: &str) -> Option<Vec<u8>> {
        let (key, _) = href_key(path);
        let key = key.strip_prefix("binary/").unwrap_or(&key);
        self.resources.get(key).cloned()
    }

    pub fn image_data_at(&self, position: PagePosition) -> Result<Option<Vec<u8>>> {
        let Some(index) = position.page.checked_sub(1) else {
            bail!("page numbers are 1-based");
        };
        let snapshot = self.pages.get(index).context("page snapshot is missing")?;
        let chunk_index = snapshot.chunk;
        let page_top = snapshot.local_start.top;

        self.with_active_chunk(chunk_index, |active| {
            let root = active.doc.root_element();
            Ok(find_image_data_at(
                &active.doc,
                root,
                page_top,
                position.x,
                position.y,
                &self.base_url,
                &self.resources,
            ))
        })
    }

    pub fn html_from_xpointer(&self, xpointer: &str) -> Result<Option<String>> {
        let position = parse_text_position(xpointer)?;
        let chunk_index = self.chunk_for_spine(position.spine_index)?;
        self.with_active_chunk(chunk_index, |active| {
            let root = active.doc.root_element();
            let node_id = xpointer::resolve_element_position(root, &position)?;
            Ok(Some(root.with(node_id).outer_html()))
        })
    }

    pub fn html_between_xpointers(&self, start: &str, end: &str) -> Result<Option<String>> {
        let start_position = parse_text_position(start)?;
        let end_position = parse_text_position(end)?;
        if start_position.spine_index != end_position.spine_index {
            return Ok(None);
        }
        let chunk_index = self.chunk_for_spine(start_position.spine_index)?;
        self.with_active_chunk(chunk_index, |active| {
            let root = active.doc.root_element();
            let start_id = xpointer::resolve_element_position(root, &start_position)?;
            let end_id = xpointer::resolve_element_position(root, &end_position)?;
            let ancestor = common_ancestor(root, start_id, end_id).unwrap_or(start_id);
            Ok(Some(root.with(ancestor).outer_html()))
        })
    }

    fn chunk_for_spine(&self, spine_index: u32) -> Result<usize> {
        self.spine_to_chunk
            .get(&spine_index)
            .copied()
            .or_else(|| {
                sorted_spine_chunks(&self.spine_to_chunk)
                    .into_iter()
                    .filter(|(index, _)| *index <= spine_index)
                    .max_by_key(|(index, _)| *index)
                    .map(|(_, chunk)| chunk)
            })
            .context("XPointer spine fragment is outside the rendered document")
    }

    pub fn render_page_with_stats(&mut self, page: usize) -> Result<(PageBitmap, usize, usize)> {
        let Some(index) = page.checked_sub(1) else {
            bail!("page numbers are 1-based");
        };
        let Some(snapshot) = self.pages.get(index) else {
            bail!("page {page} is outside 1..={}", self.page_count());
        };
        let chunk_index = snapshot.chunk;
        let start = snapshot.local_start;
        let clip_top = start.clip_top - start.top;
        let clip_bottom = snapshot.local_clip_bottom - start.top;

        self.ensure_chunk(chunk_index)?;
        let mut active = self.active.borrow_mut();
        let active = active
            .as_mut()
            .context("active document chunk is missing")?;
        let viewport_scale = self.viewport_scale;
        active.doc.set_viewport_scroll(Point {
            x: 0.0,
            y: f64::from(start.top),
        });
        let mut scene = Scene::default();
        blitz_paint::paint_scene(
            &mut scene,
            &mut active.doc,
            viewport_scale as f64,
            self.width,
            self.height,
            0,
            0,
        );

        Ok(paint_page(
            &scene,
            self.width,
            self.height,
            0.0,
            clip_top * viewport_scale,
            clip_bottom * viewport_scale,
        ))
    }

    pub fn render_page(&mut self, page: usize) -> Result<PageBitmap> {
        Ok(self.render_page_with_stats(page)?.0)
    }

    pub fn render_current_page(&mut self) -> Result<PageBitmap> {
        self.render_page(self.current_page)
    }

    pub fn render_current_page_with_stats(&mut self) -> Result<(PageBitmap, usize, usize)> {
        self.render_page_with_stats(self.current_page)
    }

    fn ensure_chunk(&self, index: usize) -> Result<()> {
        if self
            .active
            .borrow()
            .as_ref()
            .is_some_and(|active| active.index == index)
        {
            return Ok(());
        }
        let html = self
            .sources
            .get(index)
            .context("document chunk is missing")?;
        let doc = create_document(
            html,
            &self.base_url,
            self.net.clone(),
            &self.font_data,
            &DocumentOptions {
                width: self.width,
                height: self.height,
                layout_height: self.layout_height,
                page_margin: self.page_margin,
                font_size: self.font_css_size,
                font_path: None,
                stylesheet: self.stylesheet.clone(),
                embedded_styles_enabled: self.embedded_styles_enabled,
                viewport_scale: self.viewport_scale,
            },
        );
        let lines = collect_lines(doc.root_element());
        let forced_breaks = page_breaks::collect_forced_breaks(doc.root_element());
        let page_starts = paginate(
            &lines,
            &forced_breaks,
            self.height as f32,
            self.page_margin,
            self.page_margin,
        );
        *self.active.borrow_mut() = Some(ActiveChunk {
            index,
            doc,
            lines,
            page_starts,
        });
        Ok(())
    }

    fn with_active_chunk<T>(
        &self,
        index: usize,
        f: impl FnOnce(&mut ActiveChunk) -> Result<T>,
    ) -> Result<T> {
        self.ensure_chunk(index)?;
        let mut active = self.active.borrow_mut();
        let active = active
            .as_mut()
            .context("active document chunk is missing")?;
        f(active)
    }

    pub fn page_text_boxes(&self, page: usize) -> Result<Vec<TextBox>> {
        Ok(self
            .page_text_boxes_with_lines(page)?
            .into_iter()
            .map(|(_, text_box)| text_box)
            .collect())
    }

    pub fn page_text(&self, page: usize) -> Result<String> {
        Ok(self
            .page_text_boxes(page)?
            .into_iter()
            .map(|text_box| text_box.text)
            .collect::<Vec<_>>()
            .join("\n"))
    }

    pub fn text_at(&self, position: PagePosition) -> Result<Option<String>> {
        Ok(self
            .page_text_boxes(position.page)?
            .into_iter()
            .find(|text_box| {
                position.x >= text_box.x
                    && position.x <= text_box.x + text_box.width
                    && position.y >= text_box.y
                    && position.y <= text_box.y + text_box.height
            })
            .map(|text_box| text_box.text))
    }

    pub fn hit_test(&self, position: PagePosition) -> Result<Option<HitResult>> {
        let page_lines = self.page_text_boxes_with_lines(position.page)?;
        let Some((line_index, rect)) = page_lines.into_iter().find(|(_, text_box)| {
            position.x >= text_box.x
                && position.x <= text_box.x + text_box.width
                && position.y >= text_box.y
                && position.y <= text_box.y + text_box.height
        }) else {
            return Ok(None);
        };

        let snapshot = self
            .pages
            .get(position.page - 1)
            .context("page snapshot is missing")?;
        let chunk_index = snapshot.chunk;
        let page_top = snapshot.local_start.top;
        let local_line = line_index - self.chunks[chunk_index].line_base;

        self.with_active_chunk(chunk_index, |active| {
            let line = active
                .lines
                .get(local_line)
                .context("line snapshot is missing")?;
            let root = active.doc.root_element();
            let node = root.with(line.node_id);
            let Some(inline_layout) = node
                .element_data()
                .and_then(|element| element.inline_layout_data.as_ref())
            else {
                return Ok(None);
            };

            let layout = node.unrounded_layout();
            let inset = layout.padding + layout.border;
            let origin = node.absolute_position(inset.left, inset.top);
            let local_x = position.x - origin.x;
            let local_y = position.y + page_top - origin.y;

            let byte_offset = node
                .text_offset_at_point(local_x, local_y)
                .map_or(0, |offset| offset.saturating_sub(line.text_start));
            let max_offset = line.text_end.saturating_sub(line.text_start);
            let byte_offset = byte_offset.min(max_offset).min(inline_layout.text.len());

            let text_position = xpointer::position_for_line_offset(root, line, byte_offset)
                .or_else(|_| xpointer::position_for_line_offset(root, line, 0))?;
            let source_id = source_element_at_line_offset(root, line, byte_offset)
                .or(line.source_element_id)
                .unwrap_or(line.node_id);
            let link = nearest_link(root, source_id);

            Ok(Some(HitResult {
                page: position.page,
                text_position,
                text: rect.text.clone(),
                rect,
                link,
                point: position,
                line_index,
                byte_offset,
            }))
        })
    }

    pub fn word_at(&self, position: PagePosition) -> Result<Option<WordHit>> {
        let Some(hit) = self.hit_test(position)? else {
            return Ok(None);
        };
        let Some((start_byte, end_byte)) = word_byte_range(&hit.text, hit.byte_offset) else {
            return Ok(None);
        };

        let snapshot = self
            .pages
            .get(hit.page.saturating_sub(1))
            .context("page snapshot is missing")?;
        let chunk_index = snapshot.chunk;
        let line_base = self.chunks[chunk_index].line_base;
        let local_line = hit
            .line_index
            .checked_sub(line_base)
            .context("line index is outside its chunk")?;

        let (start_position, end_position) = self.with_active_chunk(chunk_index, |active| {
            let line = active
                .lines
                .get(local_line)
                .context("line snapshot is missing")?;
            let root = active.doc.root_element();
            let start = xpointer::position_for_line_offset(root, line, start_byte)?;
            let end = xpointer::position_for_line_offset(root, line, end_byte)?;
            Ok((start, end))
        })?;

        let text = hit.text[start_byte..end_byte].to_string();
        let char_count = hit.text.chars().count().max(1) as f32;
        let start_chars = hit.text[..start_byte].chars().count() as f32;
        let end_chars = hit.text[..end_byte].chars().count() as f32;
        let x0 = hit.rect.x + hit.rect.width * (start_chars / char_count);
        let x1 = hit.rect.x + hit.rect.width * (end_chars / char_count);
        let rect = TextBox {
            text: text.clone(),
            page: hit.page,
            x: x0,
            y: hit.rect.y,
            width: (x1 - x0).max(1.0),
            height: hit.rect.height,
        };

        Ok(Some(WordHit {
            text,
            xpointer_range: XPointerRange {
                start: start_position,
                end: Some(end_position),
                id: None,
            },
            rects: vec![rect],
        }))
    }

    pub fn nearest_word_at(
        &self,
        position: PagePosition,
        pages: &[usize],
        direction: u32,
    ) -> Result<Option<WordHit>> {
        let mut candidates = Vec::new();
        for page in pages.iter().copied() {
            if page == 0 || page > self.page_count() {
                continue;
            }
            let Some(snapshot) = self.pages.get(page - 1) else {
                continue;
            };
            let chunk_index = snapshot.chunk;
            let line_base = self.chunks[chunk_index].line_base;
            for (global_line, rect) in self.page_text_boxes_with_lines(page)? {
                let Some(local_line) = global_line.checked_sub(line_base) else {
                    continue;
                };
                let ranges = word_ranges(&rect.text);
                self.with_active_chunk(chunk_index, |active| {
                    let Some(line) = active.lines.get(local_line) else {
                        return Ok(());
                    };
                    let root = active.doc.root_element();
                    let char_count = rect.text.chars().count().max(1) as f32;
                    for (start_byte, end_byte) in ranges {
                        let Ok(start_position) =
                            xpointer::position_for_line_offset(root, line, start_byte)
                        else {
                            continue;
                        };
                        let Ok(end_position) =
                            xpointer::position_for_line_offset(root, line, end_byte)
                        else {
                            continue;
                        };
                        let start_chars = rect.text[..start_byte].chars().count() as f32;
                        let end_chars = rect.text[..end_byte].chars().count() as f32;
                        let x0 = rect.x + rect.width * (start_chars / char_count);
                        let x1 = rect.x + rect.width * (end_chars / char_count);
                        candidates.push(WordHit {
                            text: rect.text[start_byte..end_byte].to_string(),
                            xpointer_range: XPointerRange {
                                start: start_position,
                                end: Some(end_position),
                                id: None,
                            },
                            rects: vec![TextBox {
                                text: rect.text[start_byte..end_byte].to_string(),
                                page,
                                x: x0,
                                y: rect.y,
                                width: (x1 - x0).max(1.0),
                                height: rect.height,
                            }],
                        });
                    }
                    Ok(())
                })?;
            }
        }

        let word = candidates
            .into_iter()
            .filter(|word| {
                word.rects
                    .first()
                    .is_some_and(|rect| direction_matches(position, rect, direction))
            })
            .min_by(|left, right| {
                let left = word_sort_key(left, position, direction);
                let right = word_sort_key(right, position, direction);
                left.partial_cmp(&right)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        Ok(word)
    }

    pub fn boxes_between_xpointers(&self, start: &str, end: &str) -> Result<Vec<TextBox>> {
        let mut boxes = Vec::new();
        for (mut rect, slice_start, slice_end) in
            self.clipped_lines_between_xpointers(start, end)?
        {
            let total_chars = rect.text.chars().count().max(1) as f32;
            let start_chars = rect.text[..slice_start].chars().count() as f32;
            let end_chars = rect.text[..slice_end].chars().count() as f32;
            let x0 = rect.x + rect.width * (start_chars / total_chars);
            let x1 = rect.x + rect.width * (end_chars / total_chars);
            rect.text = rect.text[slice_start..slice_end].to_string();
            rect.x = x0;
            rect.width = (x1 - x0).max(1.0);
            boxes.push(rect);
        }
        Ok(boxes)
    }

    pub fn point_from_xpointer(&self, xpointer: &str) -> Result<Option<(f32, f32)>> {
        let target = parse_text_position(xpointer)?;
        let page = self.page_from_xpointer(xpointer)?;
        for (line_index, rect) in self.page_text_boxes_with_lines(page)? {
            let Some((start, end)) = self.line_bounds(page, line_index)? else {
                continue;
            };
            if target < start || target > end {
                continue;
            }
            let chars = if target == end {
                rect.text.chars().count().saturating_sub(1)
            } else {
                (target
                    .codepoint_offset
                    .saturating_sub(start.codepoint_offset)) as usize
            };
            let byte = byte_offset_for_char(&rect.text, chars);
            let char_count = rect.text.chars().count().max(1) as f32;
            let start_chars = rect.text[..byte].chars().count() as f32;
            return Ok(Some((
                rect.x + rect.width * (start_chars / char_count),
                rect.y,
            )));
        }
        if let Some(next) = self.next_visible_char(xpointer)?
            && next != xpointer
        {
            return self.point_from_xpointer(&next);
        }
        Ok(None)
    }

    pub fn page_links(&self, page: usize, internal_only: bool) -> Result<Vec<PageLink>> {
        let Some(snapshot) = self.pages.get(page.saturating_sub(1)) else {
            bail!("page {page} is outside 1..={}", self.page_count());
        };
        let chunk_index = snapshot.chunk;
        let line_base = self.chunks[chunk_index].line_base;
        let lines = self.page_text_boxes_with_lines(page)?;

        let mut links: Vec<PageLink> = Vec::new();
        for (line_index, rect) in lines {
            let Some(local_line) = line_index.checked_sub(line_base) else {
                continue;
            };
            self.with_active_chunk(chunk_index, |active| {
                let Some(line) = active.lines.get(local_line) else {
                    return Ok(());
                };
                let root = active.doc.root_element();
                let char_count = rect.text.chars().count().max(1);
                let mut current: Option<(String, usize, usize)> = None;

                for (char_index, (byte_offset, _)) in rect.text.char_indices().enumerate() {
                    let href = source_element_at_line_offset(root, line, byte_offset)
                        .and_then(|source_id| nearest_link(root, source_id));
                    match (&mut current, href) {
                        (Some((active_href, _, end)), Some(href)) if *active_href == href => {
                            *end = char_index + 1;
                        }
                        (_, href) => {
                            if let Some((href, start, end)) = current.take() {
                                push_link_segment(
                                    &mut links,
                                    &href,
                                    &rect,
                                    start,
                                    end,
                                    char_count,
                                    self.page_from_href(&href).is_ok(),
                                );
                            }
                            current = href.map(|href| (href, char_index, char_index + 1));
                        }
                    }
                }

                if let Some((href, start, end)) = current {
                    push_link_segment(
                        &mut links,
                        &href,
                        &rect,
                        start,
                        end,
                        char_count,
                        self.page_from_href(&href).is_ok(),
                    );
                }
                Ok(())
            })?;
        }

        links.retain(|link| !internal_only || link.internal);
        Ok(links)
    }

    pub fn text_between_xpointers(&self, start: &str, end: &str) -> Result<String> {
        let mut text = String::new();
        for (rect, slice_start, slice_end) in self.clipped_lines_between_xpointers(start, end)? {
            let part = &rect.text[slice_start..slice_end];
            if !part.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(part);
            }
        }
        Ok(text.trim_end().to_string())
    }

    fn clipped_lines_between_xpointers(
        &self,
        start: &str,
        end: &str,
    ) -> Result<Vec<(TextBox, usize, usize)>> {
        let mut start_position = parse_text_position(start)?;
        let mut end_position = parse_text_position(end)?;
        let mut start_page = self.page_from_xpointer(start)?;
        let mut end_page = self.page_from_xpointer(end)?;
        let reversed =
            start_page > end_page || (start_page == end_page && start_position > end_position);
        if reversed {
            std::mem::swap(&mut start_page, &mut end_page);
            std::mem::swap(&mut start_position, &mut end_position);
        }

        let mut lines = Vec::new();
        for page in start_page..=end_page {
            for (line_index, rect) in self.page_text_boxes_with_lines(page)? {
                let Some((line_start, line_end)) = self.line_bounds(page, line_index)? else {
                    continue;
                };
                if line_end <= start_position || line_start >= end_position {
                    continue;
                }

                let mut slice_start = 0;
                let mut slice_end = rect.text.len();
                if page == start_page
                    && line_start < start_position
                    && start_position < line_end
                    && line_start.path == start_position.path
                {
                    let chars =
                        (start_position.codepoint_offset - line_start.codepoint_offset) as usize;
                    slice_start = byte_offset_for_char(&rect.text, chars);
                }
                if page == end_page
                    && line_start < end_position
                    && end_position < line_end
                    && line_start.path == end_position.path
                {
                    let chars =
                        (end_position.codepoint_offset - line_start.codepoint_offset) as usize;
                    slice_end = byte_offset_for_char(&rect.text, chars);
                }
                if slice_start >= slice_end {
                    continue;
                }
                while slice_start < slice_end && !rect.text.is_char_boundary(slice_start) {
                    slice_start += 1;
                }
                while slice_end > slice_start && !rect.text.is_char_boundary(slice_end) {
                    slice_end -= 1;
                }
                if slice_start < slice_end {
                    lines.push((rect, slice_start, slice_end));
                }
            }
        }
        Ok(lines)
    }

    fn line_bounds(
        &self,
        page: usize,
        line_index: usize,
    ) -> Result<Option<(TextPosition, TextPosition)>> {
        let Some(snapshot) = self.pages.get(page.saturating_sub(1)) else {
            return Ok(None);
        };
        let chunk_index = snapshot.chunk;
        let line_base = self.chunks[chunk_index].line_base;
        let Some(local_line) = line_index.checked_sub(line_base) else {
            return Ok(None);
        };

        self.with_active_chunk(chunk_index, |active| {
            let Some(line) = active.lines.get(local_line) else {
                return Ok(None);
            };
            let root = active.doc.root_element();
            let start = xpointer::position_for_line_offset(root, line, 0)?;
            let end = xpointer::position_for_line_offset(
                root,
                line,
                line.text_end.saturating_sub(line.text_start),
            )?;
            Ok(Some((start, end)))
        })
    }

    pub fn selection(&self, start: PagePosition, end: PagePosition) -> Result<Option<Selection>> {
        let Some(mut start_hit) = self.hit_test(start)? else {
            return Ok(None);
        };
        let Some(mut end_hit) = self.hit_test(end)? else {
            return Ok(None);
        };

        if (start_hit.page, start_hit.line_index, start_hit.byte_offset)
            > (end_hit.page, end_hit.line_index, end_hit.byte_offset)
        {
            std::mem::swap(&mut start_hit, &mut end_hit);
        }

        let mut rects = Vec::new();
        let mut text = String::new();
        for page in start_hit.page..=end_hit.page {
            for (line_index, mut rect) in self.page_text_boxes_with_lines(page)? {
                if line_index < start_hit.line_index || line_index > end_hit.line_index {
                    continue;
                }
                let mut slice_start = 0;
                let mut slice_end = rect.text.len();
                if page == start_hit.page && line_index == start_hit.line_index {
                    slice_start = start_hit.byte_offset.min(slice_end);
                    let left = start_hit.point.x.clamp(rect.x, rect.x + rect.width);
                    rect.width = (rect.x + rect.width - left).max(0.0);
                    rect.x = left;
                }
                if page == end_hit.page && line_index == end_hit.line_index {
                    slice_end = end_hit.byte_offset.min(slice_end);
                    let right = end_hit.point.x.clamp(rect.x, rect.x + rect.width);
                    rect.width = (right - rect.x).max(0.0);
                }
                while slice_start < slice_end && !rect.text.is_char_boundary(slice_start) {
                    slice_start += 1;
                }
                while slice_end > slice_start && !rect.text.is_char_boundary(slice_end) {
                    slice_end -= 1;
                }
                if slice_start < slice_end {
                    let part = &rect.text[slice_start..slice_end];
                    if !part.trim().is_empty() {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(part);
                    }
                }
                if rect.width > 0.0 {
                    rects.push(rect);
                }
            }
        }

        Ok(Some(Selection {
            text: text.trim_end().to_string(),
            xpointer_range: XPointerRange {
                start: start_hit.text_position,
                end: Some(end_hit.text_position),
                id: None,
            },
            rects,
        }))
    }

    pub fn find_text(&self, query: &str) -> Result<Vec<SearchHit>> {
        self.find_text_with(query, false, false, None)
    }

    pub fn find_text_with(
        &self,
        query: &str,
        case_insensitive: bool,
        pattern_is_regex: bool,
        max_hits: Option<usize>,
    ) -> Result<Vec<SearchHit>> {
        self.find_text_with_search_flags(query, case_insensitive, pattern_is_regex, 0, max_hits)
    }

    pub fn find_text_with_search_flags(
        &self,
        query: &str,
        case_insensitive: bool,
        pattern_is_regex: bool,
        search_flags: i32,
        max_hits: Option<usize>,
    ) -> Result<Vec<SearchHit>> {
        if max_hits.is_some_and(|max_hits| max_hits == 0) {
            return Ok(Vec::new());
        }
        self.find_text_matches(
            query,
            case_insensitive,
            pattern_is_regex,
            search_flags,
            max_hits,
        )
    }

    pub fn find_text_with_direction(
        &self,
        query: &str,
        case_insensitive: bool,
        pattern_is_regex: bool,
        origin: i32,
        reverse: bool,
        max_hits: Option<usize>,
    ) -> Result<Vec<SearchHit>> {
        self.find_text_with_direction_flags(
            query,
            case_insensitive,
            pattern_is_regex,
            origin,
            reverse,
            0,
            max_hits,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn find_text_with_direction_flags(
        &self,
        query: &str,
        case_insensitive: bool,
        pattern_is_regex: bool,
        origin: i32,
        reverse: bool,
        search_flags: i32,
        max_hits: Option<usize>,
    ) -> Result<Vec<SearchHit>> {
        if max_hits.is_some_and(|max_hits| max_hits == 0) {
            return Ok(Vec::new());
        }
        let mut hits = self.find_text_matches(
            query,
            case_insensitive,
            pattern_is_regex,
            search_flags,
            None,
        )?;
        self.apply_search_direction(&mut hits, origin, reverse);
        if let Some(max_hits) = max_hits {
            hits.truncate(max_hits);
        }
        Ok(hits)
    }

    fn find_text_matches(
        &self,
        query: &str,
        case_insensitive: bool,
        pattern_is_regex: bool,
        search_flags: i32,
        max_hits: Option<usize>,
    ) -> Result<Vec<SearchHit>> {
        if query.is_empty() {
            bail!("search query must not be empty");
        }

        let literal = !pattern_is_regex || (search_flags & SEARCH_FLAGS_LITERAL) != 0;
        let pattern = if literal {
            regex::escape(query)
        } else if (search_flags & SEARCH_FLAGS_COMMENTS) != 0 {
            format!("(?x){query}")
        } else {
            query.to_string()
        };
        let mut builder = regex::RegexBuilder::new(&pattern);
        builder.case_insensitive(
            case_insensitive || (search_flags & SEARCH_FLAGS_CASE_INSENSITIVE) != 0,
        );
        if (search_flags & SEARCH_FLAGS_DOTALL) != 0 {
            builder.dot_matches_new_line(true);
        }
        if (search_flags & SEARCH_FLAGS_MULTILINE) != 0 {
            builder.multi_line(true);
        }
        let re = builder.build()?;

        let mut hits = Vec::new();
        for page in 1..=self.page_count() {
            let snapshot = &self.pages[page - 1];
            let chunk_index = snapshot.chunk;
            let line_base = self.chunks[chunk_index].line_base;
            let mut page_text = String::new();
            let mut spans = Vec::with_capacity(snapshot.boxes.len());
            for line in &snapshot.boxes {
                if !page_text.is_empty() {
                    page_text.push('\n');
                }
                let start = page_text.len();
                page_text.push_str(&line.text);
                spans.push((
                    line.global_line,
                    start,
                    page_text.len(),
                    TextBox {
                        text: line.text.clone(),
                        page,
                        x: line.x,
                        y: line.y,
                        width: line.width,
                        height: line.height,
                    },
                ));
            }

            for mat in re.find_iter(&page_text) {
                let start = mat.start();
                let end = mat.end();
                if start == end {
                    continue;
                }

                let Some(start_index) = spans.iter().position(|(_, span_start, span_end, _)| {
                    start >= *span_start && start < *span_end
                }) else {
                    continue;
                };
                let last = end - 1;
                let Some(end_index) = spans.iter().position(|(_, span_start, span_end, _)| {
                    last >= *span_start && last < *span_end
                }) else {
                    continue;
                };

                let (start_line_index, start_span, _, _) = &spans[start_index];
                let (end_line_index, end_span, _, end_box) = &spans[end_index];
                let local_start = start - start_span;
                let local_end = (end - end_span).min(end_box.text.len());
                let Some(local_start_line) = start_line_index.checked_sub(line_base) else {
                    continue;
                };
                let Some(local_end_line) = end_line_index.checked_sub(line_base) else {
                    continue;
                };

                let Ok((start_position, end_position)) =
                    self.with_active_chunk(chunk_index, |active| {
                        let root = active.doc.root_element();
                        let start_position = xpointer::position_for_line_offset(
                            root,
                            &active.lines[local_start_line],
                            local_start,
                        )?;
                        let end_position = xpointer::position_for_line_offset(
                            root,
                            &active.lines[local_end_line],
                            local_end,
                        )
                        .ok();
                        Ok((start_position, end_position))
                    })
                else {
                    continue;
                };

                hits.push(SearchHit {
                    page,
                    xpointer_range: XPointerRange {
                        start: start_position,
                        end: end_position,
                        id: None,
                    },
                    rects: spans[start_index..=end_index]
                        .iter()
                        .map(|(_, _, _, text_box)| text_box.clone())
                        .collect(),
                });
                if max_hits.is_some_and(|max_hits| hits.len() >= max_hits) {
                    return Ok(hits);
                }
            }
        }
        Ok(hits)
    }

    fn apply_search_direction(&self, hits: &mut Vec<SearchHit>, origin: i32, reverse: bool) {
        let current_page = self.current_page();
        if reverse {
            if origin == -1 {
                hits.retain(|hit| hit.page >= current_page);
            } else {
                hits.retain(|hit| hit.page <= current_page);
            }
            hits.sort_by(|left, right| {
                right
                    .xpointer_range
                    .start
                    .cmp(&left.xpointer_range.start)
                    .then_with(|| right.page.cmp(&left.page))
            });
        } else {
            if origin == -1 {
                hits.retain(|hit| hit.page <= current_page);
            } else {
                hits.retain(|hit| hit.page >= current_page);
            }
            hits.sort_by(|left, right| {
                left.xpointer_range
                    .start
                    .cmp(&right.xpointer_range.start)
                    .then_with(|| left.page.cmp(&right.page))
            });
        }
    }

    pub fn go_to_page(&mut self, page: usize) -> Result<usize> {
        if page < 1 || page > self.page_count() {
            bail!("page {page} is outside 1..={}", self.page_count());
        }
        self.current_page = page;
        Ok(page)
    }

    pub fn set_pos(&mut self, pos: i64) -> Result<usize> {
        let page_count = self.page_count();
        if page_count == 0 {
            bail!("document has no pages");
        }
        let page = if pos <= 0 {
            1
        } else {
            (pos / i64::from(self.height) + 1).min(page_count as i64) as usize
        };
        self.go_to_page(page)
    }

    pub fn go_to_first_page(&mut self) -> usize {
        self.current_page = 1;
        self.current_page
    }

    pub fn go_to_last_page(&mut self) -> usize {
        self.current_page = self.page_count();
        self.current_page
    }

    pub fn go_to_next_page(&mut self) -> usize {
        self.current_page = (self.current_page + 1).min(self.page_count());
        self.current_page
    }

    pub fn go_to_previous_page(&mut self) -> usize {
        self.current_page = self.current_page.saturating_sub(1).max(1);
        self.current_page
    }

    pub fn page_from_href(&self, href: &str) -> Result<usize> {
        page_for_href(&self.href_pages, href)
    }

    pub fn go_to_href(&mut self, href: &str) -> Result<usize> {
        let page = self.page_from_href(href)?;
        self.current_page = page;
        Ok(page)
    }

    pub fn bookmark_page(&self, page: usize) -> Result<Bookmark> {
        if page < 1 || page > self.page_count() {
            bail!("page {page} is outside 1..={}", self.page_count());
        }
        Ok(Bookmark {
            page,
            xpointer: self.xpointer_from_page(page).ok(),
        })
    }

    pub fn bookmark_current_page(&self) -> Result<Bookmark> {
        self.bookmark_page(self.current_page)
    }

    pub fn go_to_bookmark(&mut self, bookmark: &Bookmark) -> Result<usize> {
        let page = bookmark
            .xpointer
            .as_deref()
            .and_then(|xpointer| self.page_from_xpointer(xpointer).ok())
            .unwrap_or(bookmark.page);
        self.go_to_page(page)
    }

    pub fn xpointer_from_page(&self, page: usize) -> Result<String> {
        let Some(index) = page.checked_sub(1) else {
            bail!("page numbers are 1-based");
        };
        let Some(snapshot) = self.pages.get(index) else {
            bail!("page {page} is outside 1..={}", self.page_count());
        };
        if let Some(xpointer) = &snapshot.xpointer {
            return Ok(xpointer.clone());
        }

        let chunk_index = snapshot.chunk;
        let local_page = page - self.chunks[chunk_index].first_page + 1;
        let height = self.height;
        let page_margin = self.page_margin;
        self.with_active_chunk(chunk_index, |active| {
            xpointer::xpointer_from_page(
                &active.doc,
                &active.lines,
                &active.page_starts,
                local_page,
                height,
                page_margin,
            )
        })
    }

    pub fn page_from_xpointer(&self, xpointer: &str) -> Result<usize> {
        let trimmed = xpointer.trim();
        if let Some(page) = self.xpointer_pages.get(trimmed) {
            return Ok(*page);
        }

        if let Ok(position) = parse_text_position(trimmed)
            && let Some(page) = self.xpointer_pages.get(&position.to_xpointer())
        {
            return Ok(*page);
        }

        let position = match parse_text_position(trimmed) {
            Ok(position) => position,
            Err(_) => return self.page_from_href(trimmed),
        };
        let chunk_index = *self
            .spine_to_chunk
            .get(&position.spine_index)
            .with_context(|| {
                format!(
                    "XPointer DocFragment[{}] was not found in the rendered document",
                    position.spine_index
                )
            })?;
        let local_page = self.with_active_chunk(chunk_index, |active| {
            xpointer::page_from_xpointer(&active.doc, &active.page_starts, trimmed)
        })?;
        Ok(self.chunks[chunk_index].first_page + local_page - 1)
    }

    pub fn go_to_xpointer(&mut self, xpointer: &str) -> Result<usize> {
        let page = self.page_from_xpointer(xpointer)?;
        self.current_page = page;
        Ok(page)
    }

    pub fn compare_xpointers(&self, left: &str, right: &str) -> Result<i32> {
        let left = parse_text_position(left)?;
        let right = parse_text_position(right)?;
        Ok(match right.cmp(&left) {
            std::cmp::Ordering::Greater => 1,
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
        })
    }

    pub fn next_visible_char(&self, xpointer: &str) -> Result<Option<String>> {
        self.visible_neighbor(xpointer, Direction::Forward, NeighborUnit::Char)
    }

    pub fn prev_visible_char(&self, xpointer: &str) -> Result<Option<String>> {
        self.visible_neighbor(xpointer, Direction::Backward, NeighborUnit::Char)
    }

    pub fn next_visible_word_start(&self, xpointer: &str) -> Result<Option<String>> {
        self.visible_neighbor(xpointer, Direction::Forward, NeighborUnit::WordStart)
    }

    pub fn next_visible_word_end(&self, xpointer: &str) -> Result<Option<String>> {
        self.visible_neighbor(xpointer, Direction::Forward, NeighborUnit::WordEnd)
    }

    pub fn prev_visible_word_start(&self, xpointer: &str) -> Result<Option<String>> {
        self.visible_neighbor(xpointer, Direction::Backward, NeighborUnit::WordStart)
    }

    pub fn prev_visible_word_end(&self, xpointer: &str) -> Result<Option<String>> {
        self.visible_neighbor(xpointer, Direction::Backward, NeighborUnit::WordEnd)
    }

    fn visible_neighbor(
        &self,
        xpointer: &str,
        direction: Direction,
        unit: NeighborUnit,
    ) -> Result<Option<String>> {
        let position = parse_text_position(xpointer)?;
        let chunk_index = self.chunk_for_spine(position.spine_index)?;
        self.with_active_chunk(chunk_index, |active| {
            let root = active.doc.root_element();
            let current_id = xpointer::resolve_text_position(root, &position)?;
            let flat = flat_text_characters(root);
            let Some(current_start) = flat.iter().position(|entry| entry.node == current_id) else {
                return Ok(None);
            };
            let start_index = (current_start + position.codepoint_offset as usize).min(flat.len());
            let target = match (direction, unit) {
                (Direction::Forward, NeighborUnit::Char) => {
                    (start_index + 1..flat.len()).find(|index| !flat[*index].ch.is_whitespace())
                }
                (Direction::Backward, NeighborUnit::Char) => (0..start_index)
                    .rev()
                    .find(|index| !flat[*index].ch.is_whitespace()),
                (Direction::Forward, NeighborUnit::WordStart) => {
                    next_word_start(&flat, start_index)
                }
                (Direction::Forward, NeighborUnit::WordEnd) => next_word_end(&flat, start_index),
                (Direction::Backward, NeighborUnit::WordStart) => {
                    prev_word_start(&flat, start_index)
                }
                (Direction::Backward, NeighborUnit::WordEnd) => prev_word_end(&flat, start_index),
            };
            let Some(index) = target else {
                return Ok(None);
            };
            let Some(entry) = flat.get(index).or_else(|| flat.last()) else {
                return Ok(None);
            };
            Ok(Some(
                xpointer::position_for_text_node(root, entry.node, entry.offset)?.to_xpointer(),
            ))
        })
    }

    pub fn extend_to_sentence(
        &self,
        start: &str,
        end: &str,
    ) -> Result<Option<(String, String, String)>> {
        let mut start_position = parse_text_position(start)?;
        let mut end_position = parse_text_position(end)?;
        if start_position.spine_index != end_position.spine_index {
            return Ok(None);
        }
        if end_position < start_position {
            std::mem::swap(&mut start_position, &mut end_position);
        }
        let chunk_index = self.chunk_for_spine(start_position.spine_index)?;
        self.with_active_chunk(chunk_index, |active| {
            let root = active.doc.root_element();
            let start_id = xpointer::resolve_text_position(root, &start_position)?;
            let end_id = xpointer::resolve_text_position(root, &end_position)?;
            if start_id != end_id {
                return Ok(None);
            }
            let Some(text) = root
                .with(start_id)
                .text_data()
                .map(|data| data.content.clone())
            else {
                return Ok(None);
            };
            let chars = text.chars().collect::<Vec<_>>();
            let mut start_offset = (start_position.codepoint_offset as usize).min(chars.len());
            let mut end_offset = (end_position.codepoint_offset as usize).min(chars.len());
            let sentence_break = |ch: char| matches!(ch, '.' | '!' | '?' | '。' | '！' | '？');

            let mut sentence_start = 0usize;
            for index in (0..start_offset).rev() {
                if sentence_break(chars[index]) {
                    sentence_start = index + 1;
                    break;
                }
            }
            while sentence_start < start_offset && chars[sentence_start].is_whitespace() {
                sentence_start += 1;
            }
            start_offset = sentence_start;

            let mut sentence_end = chars.len();
            for (index, ch) in chars.iter().enumerate().skip(end_offset) {
                if sentence_break(*ch) {
                    sentence_end = index + 1;
                    break;
                }
            }
            end_offset = sentence_end;
            while end_offset > start_offset && chars[end_offset - 1].is_whitespace() {
                end_offset -= 1;
            }

            start_position.codepoint_offset = start_offset as u32;
            end_position.codepoint_offset = end_offset as u32;
            let start_xpointer = start_position.to_xpointer();
            let end_xpointer = end_position.to_xpointer();
            let text = chars[start_offset..end_offset].iter().collect::<String>();
            Ok(Some((text, start_xpointer, end_xpointer)))
        })
    }

    pub fn text_from_xpointer(&self, xpointer: &str) -> Result<String> {
        let position = parse_text_position(xpointer)?;
        let chunk_index = *self
            .spine_to_chunk
            .get(&position.spine_index)
            .with_context(|| {
                format!(
                    "XPointer DocFragment[{}] was not found in the rendered document",
                    position.spine_index
                )
            })?;
        self.with_active_chunk(chunk_index, |active| {
            let root = active.doc.root_element();
            let text_id = xpointer::resolve_text_position(root, &position)?;
            Ok(root
                .with(text_id)
                .text_data()
                .map(|text| text.content.clone())
                .unwrap_or_default())
        })
    }

    pub fn current_position(&self) -> Result<TextPosition> {
        let xpointer = self.xpointer_from_page(self.current_page)?;
        parse_text_position(&xpointer)
    }

    pub fn xpointer_from_position(&self, position: &TextPosition) -> Result<String> {
        Ok(position.to_xpointer())
    }

    fn page_text_boxes_with_lines(&self, page: usize) -> Result<Vec<(usize, TextBox)>> {
        let Some(index) = page.checked_sub(1) else {
            bail!("page numbers are 1-based");
        };
        let Some(snapshot) = self.pages.get(index) else {
            bail!("page {page} is outside 1..={}", self.page_count());
        };

        Ok(snapshot
            .boxes
            .iter()
            .map(|line| {
                (
                    line.global_line,
                    TextBox {
                        text: line.text.clone(),
                        page,
                        x: line.x,
                        y: line.y,
                        width: line.width,
                        height: line.height,
                    },
                )
            })
            .collect())
    }
}

fn validate_options(options: &DocumentOptions) -> Result<()> {
    if options.width == 0 || options.height == 0 || options.layout_height == 0 {
        bail!("width, height, and layout_height must be positive");
    }
    if !options.font_size.is_finite() || options.font_size <= 0.0 {
        bail!("font_size must be a positive finite number");
    }
    if !options.viewport_scale.is_finite() || options.viewport_scale <= 0.0 {
        bail!("viewport_scale must be a positive finite number");
    }
    Ok(())
}

fn detect_format(path: &Path) -> DocumentFormat {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "fb2" => DocumentFormat::Fb2,
        "txt" => DocumentFormat::Txt,
        _ => DocumentFormat::Epub,
    }
}

fn resolve_font_path(options: &DocumentOptions) -> Result<PathBuf> {
    if let Some(path) = options.font_path.as_ref() {
        return Ok(path.clone());
    }
    Ok(PathBuf::from(
        std::env::var("CRE_FONT_PATH")
            .unwrap_or_else(|_| "/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf".to_string()),
    ))
}

fn collect_lines(root: &Node) -> Vec<LineBox> {
    let mut lines = Vec::new();
    collect_lines_inner(root, &mut lines);
    lines.sort_by(|a, b| a.top.total_cmp(&b.top));
    lines
}

fn collect_lines_inner(node: &Node, out: &mut Vec<LineBox>) {
    if let Some(element) = node.element_data()
        && let Some(inline_layout) = element.inline_layout_data.as_ref()
    {
        let root_layout = node.unrounded_layout();
        let inset = root_layout.padding + root_layout.border;
        let origin_y = inset.top + inline_layout.block_offset;
        for line in inline_layout.layout.lines() {
            let metrics = line.metrics();
            let line_x = metrics.offset + metrics.inline_min_coord;
            let top = node
                .absolute_position(inset.left, origin_y + metrics.block_min_coord)
                .y;
            let bottom = node
                .absolute_position(inset.left, origin_y + metrics.block_max_coord)
                .y;
            let left = node
                .absolute_position(inset.left + line_x, origin_y + metrics.block_min_coord)
                .x;
            let right = node
                .absolute_position(
                    inset.left + metrics.offset + metrics.inline_max_coord,
                    origin_y + metrics.block_min_coord,
                )
                .x;
            let source_element_id = line.runs().find_map(|run| {
                run.clusters()
                    .next()
                    .map(|cluster| cluster.style().brush.id)
            });
            let text_range = line.text_range();
            out.push(LineBox {
                node_id: node.id,
                text_start: text_range.start,
                text_end: text_range.end,
                source_element_id,
                left,
                right,
                top,
                bottom,
            });
        }
    }
    if let Some(layout_children) = node.layout_children.borrow().as_ref() {
        for child in layout_children.iter() {
            collect_lines_inner(node.with(*child), out);
        }
    }
}

fn paginate(
    lines: &[LineBox],
    forced_breaks: &[f32],
    page_height: f32,
    margin_top: f32,
    margin_bottom: f32,
) -> Vec<PageStart> {
    const BREAK_EPSILON: f32 = 0.5;

    let mut starts = vec![PageStart {
        top: 0.0,
        clip_top: margin_top,
    }];
    let mut forced_index = 0usize;
    for line in lines {
        let current = starts.last().unwrap();
        let mut force_break_before = false;
        while forced_index < forced_breaks.len()
            && forced_breaks[forced_index] <= line.top + BREAK_EPSILON
        {
            if forced_breaks[forced_index] > current.clip_top + BREAK_EPSILON
                && line.top > current.clip_top + BREAK_EPSILON
            {
                force_break_before = true;
            }
            forced_index += 1;
        }
        if force_break_before {
            starts.push(PageStart {
                top: line.top - margin_top,
                clip_top: line.top,
            });
        }

        let current = starts.last().unwrap();
        let clip_bottom = current.top + page_height - margin_bottom;
        if line.bottom > clip_bottom && line.top > current.clip_top {
            starts.push(PageStart {
                top: line.top - margin_top,
                clip_top: line.top,
            });
        }
    }
    starts
}

fn collect_anchor_maps(root: &Node) -> HashMap<(String, Option<String>), NodeId> {
    let mut ids = Vec::new();
    collect_toc_anchors(root, None, &mut ids);
    let mut anchors = HashMap::new();
    for anchor in ids {
        anchors
            .entry((anchor.resource, anchor.fragment))
            .or_insert(anchor.node_id);
    }
    anchors
}

fn collect_toc_anchors(node: &Node, current_resource: Option<String>, out: &mut Vec<TocAnchorId>) {
    let mut resource = current_resource;

    if let Some(href) = node_attr(node, "data-spine-href") {
        let href = normalize_path(&percent_decode_str(href).decode_utf8_lossy());
        out.push(TocAnchorId {
            resource: href.clone(),
            fragment: None,
            node_id: node.id,
        });
        resource = Some(href);
    }

    let resource = resource.as_deref().unwrap_or_default();
    if let Some(id) = element_id(node) {
        out.push(TocAnchorId {
            resource: resource.to_string(),
            fragment: Some(id.to_string()),
            node_id: node.id,
        });
    }
    if let Some(name) = node_attr(node, "name") {
        out.push(TocAnchorId {
            resource: resource.to_string(),
            fragment: Some(name.to_string()),
            node_id: node.id,
        });
    }

    let inherited_resource = (!resource.is_empty()).then(|| resource.to_string());
    for child in node.children.iter() {
        collect_toc_anchors(node.with(*child), inherited_resource.clone(), out);
    }
}

fn node_attr<'a>(node: &'a Node, name: &str) -> Option<&'a str> {
    node.attrs()?
        .iter()
        .find(|attr| attr.name.local.to_string() == name)
        .map(|attr| attr.value.as_str())
}

fn collect_line_flows(
    node: &Node,
    current_flow: i64,
    max_flow: &mut i64,
    line_indices: &HashMap<NodeId, Vec<usize>>,
    line_flows: &mut Vec<i64>,
) {
    let local_flow = if node_attr(node, "data-spine-nonlinear").is_some_and(|value| value == "1") {
        *max_flow += 1;
        *max_flow
    } else {
        current_flow
    };
    if let Some(indices) = line_indices.get(&node.id) {
        for index in indices {
            if *index < line_flows.len() {
                line_flows[*index] = local_flow;
            }
        }
    }
    for child in &node.children {
        collect_line_flows(
            node.with(*child),
            local_flow,
            max_flow,
            line_indices,
            line_flows,
        );
    }
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '\'' | '\u{2019}' | '-' | '_')
}

fn push_link_segment(
    links: &mut Vec<PageLink>,
    href: &str,
    rect: &TextBox,
    start: usize,
    end: usize,
    char_count: usize,
    internal: bool,
) {
    if start >= end {
        return;
    }
    let x0 = rect.x + rect.width * (start as f32 / char_count as f32);
    let x1 = rect.x + rect.width * (end as f32 / char_count as f32);
    let segment = TextBox {
        text: String::new(),
        page: rect.page,
        x: x0,
        y: rect.y,
        width: (x1 - x0).max(1.0),
        height: rect.height,
    };
    if let Some(link) = links.iter_mut().find(|link| link.href == href) {
        link.rects.push(segment);
    } else {
        links.push(PageLink {
            href: href.to_string(),
            internal,
            rects: vec![segment],
        });
    }
}

fn byte_offset_for_char(text: &str, char_offset: usize) -> usize {
    text.char_indices()
        .nth(char_offset)
        .map_or(text.len(), |(offset, _)| offset)
}

fn word_byte_range(text: &str, byte_offset: usize) -> Option<(usize, usize)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    if chars.is_empty() {
        return None;
    }

    let mut index = chars
        .iter()
        .position(|(offset, _)| *offset >= byte_offset)
        .unwrap_or(chars.len() - 1);
    if index >= chars.len() {
        index = chars.len() - 1;
    }

    if !is_word_char(chars[index].1) {
        if let Some(next) = chars[index + 1..]
            .iter()
            .position(|(_, ch)| is_word_char(*ch))
        {
            index += next + 1;
        } else {
            index = chars[..index]
                .iter()
                .rposition(|(_, ch)| is_word_char(*ch))?;
        }
    }

    let mut start = index;
    while start > 0 && is_word_char(chars[start - 1].1) {
        start -= 1;
    }

    let mut end = index + 1;
    while end < chars.len() && is_word_char(chars[end].1) {
        end += 1;
    }

    let start_byte = chars[start].0;
    let end_byte = chars.get(end).map_or(text.len(), |(offset, _)| *offset);
    (start_byte < end_byte).then_some((start_byte, end_byte))
}

fn word_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start: Option<usize> = None;
    for (offset, ch) in text.char_indices() {
        if is_word_char(ch) {
            start.get_or_insert(offset);
        } else if let Some(start) = start.take()
            && start < offset
        {
            ranges.push((start, offset));
        }
    }
    if let Some(start) = start
        && start < text.len()
    {
        ranges.push((start, text.len()));
    }
    ranges
}

fn direction_matches(point: PagePosition, rect: &TextBox, direction: u32) -> bool {
    let right = rect.x + rect.width;
    let bottom = rect.y + rect.height;
    match direction {
        0 => point.x >= right - 1.0,
        1 => point.y >= bottom - 1.0,
        2 => point.x <= rect.x + 1.0,
        3 => point.y <= rect.y + 1.0,
        _ => true,
    }
}

fn word_sort_key(
    word: &WordHit,
    point: PagePosition,
    direction: u32,
) -> (f32, f32, f32, f32, String) {
    let Some(rect) = word.rects.first() else {
        return (f32::INFINITY, f32::INFINITY, 0.0, 0.0, String::new());
    };
    let center_x = rect.x + rect.width * 0.5;
    let center_y = rect.y + rect.height * 0.5;
    let (primary, secondary) = match direction {
        0 | 2 => ((center_x - point.x).abs(), (center_y - point.y).abs()),
        1 | 3 => ((center_y - point.y).abs(), (center_x - point.x).abs()),
        _ => ((center_x - point.x).hypot(center_y - point.y), 0.0),
    };
    (primary, secondary, rect.x, rect.y, word.text.clone())
}

fn source_element_at_line_offset(
    root: &Node,
    line: &LineBox,
    byte_offset: usize,
) -> Option<NodeId> {
    let inline_root = root.with(line.node_id);
    let inline_layout = inline_root.element_data()?.inline_layout_data.as_ref()?;
    let line_end = line.text_end.min(inline_layout.text.len());
    let target = (line.text_start + byte_offset)
        .min(line_end.saturating_sub(1))
        .max(line.text_start);

    for layout_line in inline_layout.layout.lines() {
        let text_range = layout_line.text_range();
        if target < text_range.start || target >= text_range.end {
            continue;
        }
        for run in layout_line.runs() {
            for cluster in run.clusters() {
                let cluster_range = cluster.text_range();
                if target >= cluster_range.start && target < cluster_range.end {
                    return Some(cluster.style().brush.id);
                }
            }
        }
    }
    None
}

fn nearest_link(root: &Node, start_id: NodeId) -> Option<String> {
    let mut current = Some(start_id);
    while let Some(id) = current {
        let node = root.with(id);
        if node
            .element_data()
            .is_some_and(|element| element.name.local.to_string() == "a")
        {
            return node_attr(node, "href").map(str::to_string);
        }
        current = node.parent;
    }
    None
}

#[derive(Clone, Copy)]
enum Direction {
    Forward,
    Backward,
}

#[derive(Clone, Copy)]
enum NeighborUnit {
    Char,
    WordStart,
    WordEnd,
}

struct FlatCharacter {
    node: NodeId,
    offset: u32,
    ch: char,
}

fn flat_text_characters(root: &Node) -> Vec<FlatCharacter> {
    let mut out = Vec::new();
    collect_flat_characters(root, &mut out);
    out
}

fn collect_flat_characters(node: &Node, out: &mut Vec<FlatCharacter>) {
    if let Some(text) = node.text_data() {
        for (offset, ch) in text.content.chars().enumerate() {
            out.push(FlatCharacter {
                node: node.id,
                offset: offset as u32,
                ch,
            });
        }
    }
    for child in node.children.iter() {
        collect_flat_characters(node.with(*child), out);
    }
}

fn next_word_start(flat: &[FlatCharacter], start: usize) -> Option<usize> {
    let mut index = start.min(flat.len());
    if flat.get(index).is_some_and(|entry| is_word_char(entry.ch)) {
        while flat.get(index).is_some_and(|entry| is_word_char(entry.ch)) {
            index += 1;
        }
    }
    while index < flat.len() {
        if is_word_char(flat[index].ch) && (index == 0 || !is_word_char(flat[index - 1].ch)) {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn next_word_end(flat: &[FlatCharacter], start: usize) -> Option<usize> {
    let mut index = start.min(flat.len());
    while index < flat.len() && !is_word_char(flat[index].ch) {
        index += 1;
    }
    if index >= flat.len() {
        return None;
    }
    while index + 1 < flat.len() && is_word_char(flat[index + 1].ch) {
        index += 1;
    }
    Some(index + 1)
}

fn prev_word_start(flat: &[FlatCharacter], start: usize) -> Option<usize> {
    let mut index = start.min(flat.len());
    if index > 0 && is_word_char(flat[index - 1].ch) {
        index -= 1;
        while index > 0 && is_word_char(flat[index - 1].ch) {
            index -= 1;
        }
        return Some(index);
    }
    while index > 0 && !is_word_char(flat[index - 1].ch) {
        index -= 1;
    }
    if index == 0 {
        return None;
    }
    index -= 1;
    while index > 0 && is_word_char(flat[index - 1].ch) {
        index -= 1;
    }
    Some(index)
}

fn prev_word_end(flat: &[FlatCharacter], start: usize) -> Option<usize> {
    let mut index = start.min(flat.len());
    while index > 0 && !is_word_char(flat[index - 1].ch) {
        index -= 1;
    }
    if index == 0 {
        return None;
    }
    if index == start && index < flat.len() && is_word_char(flat[index].ch) {
        while index < flat.len() && is_word_char(flat[index].ch) {
            index += 1;
        }
        return Some(index);
    }
    while index > 0 && is_word_char(flat[index - 1].ch) {
        index -= 1;
    }
    let mut end = index;
    while end < flat.len() && is_word_char(flat[end].ch) {
        end += 1;
    }
    Some(end)
}

fn sorted_spine_chunks(map: &HashMap<u32, usize>) -> Vec<(u32, usize)> {
    let mut entries = map
        .iter()
        .map(|(spine, chunk)| (*spine, *chunk))
        .collect::<Vec<_>>();
    entries.sort_unstable_by_key(|(spine, _)| *spine);
    entries
}

fn common_ancestor(root: &Node, left: NodeId, right: NodeId) -> Option<NodeId> {
    let mut ancestors = std::collections::HashSet::new();
    let mut current = Some(left);
    while let Some(id) = current {
        ancestors.insert(id);
        current = root.with(id).parent;
    }
    let mut current = Some(right);
    while let Some(id) = current {
        if ancestors.contains(&id) {
            return Some(id);
        }
        current = root.with(id).parent;
    }
    None
}

fn find_image_data_at(
    doc: &HtmlDocument,
    node: &Node,
    page_top: f32,
    x: f32,
    y: f32,
    base_url: &str,
    resources: &HashMap<String, Vec<u8>>,
) -> Option<Vec<u8>> {
    if node
        .element_data()
        .is_some_and(|element| element.name.local.to_string() == "img")
        && let Some(rect) = doc.get_client_bounding_rect(node.id)
    {
        let local_top = rect.y as f32 - page_top;
        if x >= rect.x as f32
            && x <= (rect.x + rect.width) as f32
            && y >= local_top
            && y <= local_top + rect.height as f32
        {
            let src = node_attr(node, "src")?;
            let resolved = if src.contains("://") {
                src.to_string()
            } else if src.starts_with('/') {
                format!("{}{}", base_url.trim_end_matches('/'), src)
            } else {
                format!("{}{}", base_url, src)
            };
            let (key, _) = href_key(&resolved);
            let key = key.strip_prefix("binary/").unwrap_or(&key);
            if let Some(data) = resources.get(key) {
                return Some(data.clone());
            }
        }
    }

    for child in node.children.iter() {
        if let Some(data) =
            find_image_data_at(doc, node.with(*child), page_top, x, y, base_url, resources)
        {
            return Some(data);
        }
    }
    None
}

fn epub_cover_image(book: &EpubBook) -> Option<Vec<u8>> {
    let item = book
        .manifest
        .iter()
        .find(|item| {
            item.properties
                .split_whitespace()
                .any(|property| property == "cover-image")
        })
        .or_else(|| {
            book.manifest.iter().find(|item| {
                item.id.eq_ignore_ascii_case("cover")
                    || item
                        .href
                        .rsplit('/')
                        .next()
                        .is_some_and(|name| name.to_ascii_lowercase().starts_with("cover"))
            })
        })?;
    let key = normalize_path(&item.href);
    book.resources.get(&key).cloned()
}

/// Rasterizes an SVG document into premultiplied RGBA bytes on a white background.
///
/// When `target_width`/`target_height` are provided the intrinsic size is scaled
/// proportionally to fit inside that box. Returns `None` when the data is not a
/// valid scalable SVG document.
pub fn render_svg_image(
    data: &[u8],
    target_width: Option<u32>,
    target_height: Option<u32>,
) -> Option<(Vec<u8>, u32, u32)> {
    let mut options = resvg::usvg::Options::default();
    let mut fontdb = resvg::usvg::fontdb::Database::new();
    fontdb.load_system_fonts();
    options.fontdb = std::sync::Arc::new(fontdb);

    let tree = resvg::usvg::Tree::from_data(data, &options).ok()?;
    let size = tree.size();
    let (src_width, src_height) = (size.width(), size.height());
    if !(src_width > 0.0 && src_height > 0.0) {
        return None;
    }

    let scale = match (target_width, target_height) {
        (Some(width), Some(height)) => (width as f32 / src_width).min(height as f32 / src_height),
        (Some(width), None) => width as f32 / src_width,
        (None, Some(height)) => height as f32 / src_height,
        (None, None) => 1.0,
    };
    let out_width = ((src_width * scale).round() as u32).max(1);
    let out_height = ((src_height * scale).round() as u32).max(1);

    let mut pixmap = resvg::tiny_skia::Pixmap::new(out_width, out_height)?;
    pixmap.fill(resvg::tiny_skia::Color::WHITE);
    let transform = resvg::tiny_skia::Transform::from_scale(
        out_width as f32 / src_width,
        out_height as f32 / src_height,
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    Some((pixmap.take(), out_width, out_height))
}

fn element_id(node: &Node) -> Option<String> {
    node.element_data()
        .and_then(|element| element.id.as_ref())
        .map(|id| id.to_string())
}

fn split_toc_href(href: &str) -> (&str, Option<&str>) {
    match href.find('#') {
        Some(index) => (&href[..index], Some(&href[index + 1..])),
        None => (href, None),
    }
}

fn create_document(
    html: &str,
    base_url: &str,
    net: Arc<dyn NetProvider>,
    font_data: &[u8],
    options: &DocumentOptions,
) -> HtmlDocument {
    let mut config = DocumentConfig {
        viewport: Some(Viewport::new(
            options.width,
            options.layout_height,
            options.viewport_scale,
            ColorScheme::Light,
        )),
        base_url: Some(base_url.to_string()),
        net_provider: Some(net),
        font_ctx: Some(build_single_font_ctx(font_data)),
        style_threading: StyleThreading::Sequential,
        ..Default::default()
    };

    if let Some(stylesheet) = options
        .stylesheet
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        config.ua_stylesheets = vec![
            UserAgentStylesheet::default_sheet(),
            UserAgentStylesheet::parse(stylesheet),
        ];
    }

    let stripped = if options.embedded_styles_enabled {
        String::new()
    } else {
        strip_author_styles(html)
    };
    let html_source = if options.embedded_styles_enabled {
        html
    } else {
        stripped.as_str()
    };
    let mut doc = HtmlDocument::from_html(html_source, config);
    doc.resolve(0.0);
    for _ in 0..4 {
        doc.handle_messages();
        doc.resolve(0.0);
    }
    doc
}

fn strip_author_styles(html: &str) -> String {
    let bytes = remove_style_blocks(html.as_bytes().to_vec());
    let bytes = remove_link_stylesheets(bytes);
    String::from_utf8(bytes).unwrap_or_else(|_| html.to_string())
}

fn lower_ascii(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    for value in out.iter_mut() {
        if *value >= b'A' && *value <= b'Z' {
            *value += 32;
        }
    }
    out
}

fn find_case_insensitive(haystack: &[u8], needle_lower: &[u8], from: usize) -> Option<usize> {
    if needle_lower.is_empty() {
        return Some(from);
    }
    if from > haystack.len() || from + needle_lower.len() > haystack.len() {
        return None;
    }
    let upper = haystack.len() - needle_lower.len();
    (from..=upper).find(|start| {
        needle_lower
            .iter()
            .enumerate()
            .all(|(offset, needle)| haystack[start + offset].to_ascii_lowercase() == *needle)
    })
}

fn contains_in_range(haystack: &[u8], needle_lower: &[u8], start: usize, end: usize) -> bool {
    find_case_insensitive(haystack, needle_lower, start).is_some_and(|position| position <= end)
}

fn remove_range(bytes: &[u8], start: usize, end: usize) -> Vec<u8> {
    let end = end.min(bytes.len());
    let start = start.min(end);
    let mut out = Vec::with_capacity(bytes.len().saturating_sub(end - start));
    out.extend(bytes[..start].iter());
    out.extend(bytes[end..].iter());
    out
}

fn remove_style_blocks(bytes: Vec<u8>) -> Vec<u8> {
    let mut current = bytes;
    loop {
        let lower = lower_ascii(&current);
        let Some(start) = find_case_insensitive(&lower, b"<style", 0) else {
            break;
        };
        let close = find_case_insensitive(&lower, b"</style>", start + b"<style".len())
            .map(|end| end + b"</style>".len())
            .unwrap_or(current.len());
        current = remove_range(&current, start, close);
    }
    current
}

fn remove_link_stylesheets(bytes: Vec<u8>) -> Vec<u8> {
    let mut current = bytes;
    let mut start = 0usize;
    loop {
        let lower = lower_ascii(&current);
        let Some(tag_start) = find_case_insensitive(&lower, b"<link", start) else {
            break;
        };
        let Some(tag_end) = find_case_insensitive(&lower, b">", tag_start) else {
            break;
        };
        let is_stylesheet_link = contains_in_range(&lower, b"stylesheet", tag_start, tag_end)
            || contains_in_range(&lower, b"text/css", tag_start, tag_end);
        if is_stylesheet_link {
            current = remove_range(&current, tag_start, tag_end + 1);
            continue;
        }
        start = tag_end + 1;
    }
    current
}

fn snapshot_page_boxes(
    root: &Node,
    lines: &[LineBox],
    start: PageStart,
    clip_bottom: f32,
    line_base: usize,
) -> Vec<SnapshotLine> {
    let lower = start.clip_top;
    let upper = clip_bottom;
    let mut boxes = Vec::new();
    for (local_line, line) in lines.iter().enumerate() {
        if line.top + 0.5 < lower || line.top + 0.5 >= upper {
            continue;
        }
        let Some(text) = line_text_from_root(root, line) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }

        let top = line.top.max(lower);
        let bottom = line.bottom.min(upper);
        if bottom <= top {
            continue;
        }

        boxes.push(SnapshotLine {
            text,
            x: line.left,
            y: top - start.top,
            width: (line.right - line.left).max(0.0),
            height: bottom - top,
            global_line: line_base + local_line,
        });
    }
    boxes
}

fn line_text_from_root(root: &Node, line: &LineBox) -> Option<String> {
    let node = root.with(line.node_id);
    let inline_layout = node.element_data()?.inline_layout_data.as_ref()?;
    inline_layout
        .text
        .get(line.text_start..line.text_end)
        .map(str::to_string)
}

fn page_clip_bottom(page_starts: &[PageStart], index: usize, options: &DocumentOptions) -> f32 {
    let start = page_starts[index];
    page_starts.get(index + 1).map_or(
        start.top + options.height as f32 - options.page_margin,
        |next| next.clip_top,
    )
}

fn collect_spine_chunks(node: &Node, chunk_index: usize, out: &mut HashMap<u32, usize>) {
    if let Some(index) = node_attr(node, "data-spine-index").and_then(|value| value.parse().ok()) {
        out.entry(index).or_insert(chunk_index);
    }
    for child in &node.children {
        collect_spine_chunks(node.with(*child), chunk_index, out);
    }
}

fn href_key(href: &str) -> (String, Option<String>) {
    let (path, fragment) = split_toc_href(href);
    let path = if path.contains("://") {
        url::Url::parse(path)
            .map(|url| url.path().trim_start_matches('/').to_string())
            .unwrap_or_else(|_| path.to_string())
    } else {
        percent_decode_str(path).decode_utf8_lossy().into_owned()
    };
    let path = normalize_path(&path);
    let fragment = fragment.map(|fragment| {
        percent_decode_str(fragment)
            .decode_utf8_lossy()
            .into_owned()
    });
    (path, fragment)
}

fn page_for_href(
    href_pages: &HashMap<(String, Option<String>), usize>,
    href: &str,
) -> Result<usize> {
    let (path, fragment) = href_key(href);
    href_pages
        .get(&(path.clone(), fragment))
        .or_else(|| href_pages.get(&(path, None)))
        .copied()
        .context("navigation target was not found in the rendered document")
}

fn page_for_node(doc: &HtmlDocument, node_id: NodeId, page_starts: &[PageStart]) -> Result<usize> {
    let rect = doc
        .get_client_bounding_rect(node_id)
        .context("navigation target has no layout rect")?;
    let top = rect.y as f32;
    let mut page = 1usize;
    for (index, start) in page_starts.iter().enumerate() {
        if top + 1.0 >= start.clip_top {
            page = index + 1;
        } else {
            break;
        }
    }
    Ok(page)
}

struct EpubNet {
    resources: Arc<HashMap<String, Vec<u8>>>,
}

impl NetProvider for EpubNet {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let path = percent_decode_str(request.url.path())
            .decode_utf8_lossy()
            .trim_start_matches('/')
            .to_string();
        let key = normalize_path(&path);
        let bytes = self.resources.get(&key).cloned().unwrap_or_default();
        handler.bytes(request.url.to_string(), Bytes::from(bytes));
    }
}

struct Fb2Net {
    resources: Arc<HashMap<String, Vec<u8>>>,
}

impl NetProvider for Fb2Net {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let path = percent_decode_str(request.url.path())
            .decode_utf8_lossy()
            .trim_start_matches('/')
            .to_string();
        let name = path.strip_prefix("binary/").unwrap_or(&path).to_string();
        let bytes = self.resources.get(&name).cloned().unwrap_or_default();
        handler.bytes(request.url.to_string(), Bytes::from(bytes));
    }
}

struct EmptyNet;

impl NetProvider for EmptyNet {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        handler.bytes(request.url.to_string(), Bytes::from(Vec::new()));
    }
}

#[test]
fn strip_author_styles_removes_embedded_style_and_stylesheet_link() {
    let html = r#"
<html>
<head>
<link rel="stylesheet" href="cover.css"></link>
<style>.hidden { visibility: hidden; }</style>
</head>
<body><p class="hidden">visible</p></body>
</html>
"#;
    let stripped = strip_author_styles(html);
    let bytes = stripped.as_bytes();
    assert!(find_case_insensitive(bytes, b"<style", 0).is_none());
    assert!(find_case_insensitive(bytes, b"<link", 0).is_none());
    assert!(find_case_insensitive(bytes, b"visible", 0).is_some());
}

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use anyrender::Scene;
use blitz_dom::{DocumentConfig, Node, NodeId, Point, StyleThreading, build_single_font_ctx};
use blitz_html::HtmlDocument;
use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request};
use blitz_traits::shell::{ColorScheme, Viewport};
use cre_epub::{EpubBook, HtmlOptions, TocItem as RawTocItem, normalize_path};
use cre_fb2::{Fb2Book, Fb2HtmlOptions};
use cre_paint::render_page as paint_page;

pub use cre_paint::PageBitmap;
use cre_text::{TextBook, TextHtmlOptions};
use percent_encoding::percent_decode_str;

mod position;
mod xpointer;

pub use position::{PathSegment, TextPosition, XPointerRange, parse_text_position};

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
    pub item_count: usize,
}

#[derive(Clone, Debug)]
pub struct DocumentOptions {
    pub width: u32,
    pub height: u32,
    pub layout_height: u32,
    pub page_margin: f32,
    pub font_path: Option<PathBuf>,
}

impl Default for DocumentOptions {
    fn default() -> Self {
        Self {
            width: 600,
            height: 800,
            layout_height: 100_000,
            page_margin: 32.0,
            font_path: None,
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
    pub xpointer_range: XPointerRange,
    pub rects: Vec<TextBox>,
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
    boxes: Vec<SnapshotLine>,
    xpointer: Option<String>,
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
    sources: Vec<String>,
    base_url: String,
    net: Arc<dyn NetProvider>,
    font_data: Vec<u8>,
    chunks: Vec<ChunkMeta>,
    pages: Vec<PageSnapshot>,
    xpointer_pages: HashMap<String, usize>,
    href_pages: HashMap<(String, Option<String>), usize>,
    spine_to_chunk: HashMap<u32, usize>,
    toc: Vec<TocEntry>,
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
                let raw_toc = std::mem::take(&mut book.toc);
                let metadata = DocumentMetadata {
                    format,
                    title: book.title.clone(),
                    creator: book.creator.clone(),
                    language: book.language.clone(),
                    item_count: book.spine.len(),
                };
                let htmls = book.to_html_chunks(&HtmlOptions::default(), CHUNK_TARGET_BYTES)?;
                let resources = Arc::new(book.resources);
                Self::build(
                    htmls,
                    "epub://book/",
                    Arc::new(EpubNet { resources }),
                    metadata,
                    raw_toc,
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
                    item_count: book.binary.len(),
                };
                let html = book.to_html(&Fb2HtmlOptions::default())?;
                let resources = Arc::new(book.binary);
                Self::build(
                    vec![html],
                    "fb2://book/",
                    Arc::new(Fb2Net { resources }),
                    metadata,
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
                    item_count: 1,
                };
                let html = book.to_html(&TextHtmlOptions::default());
                Self::build(
                    vec![html],
                    "text://book/",
                    Arc::new(EmptyNet),
                    metadata,
                    Vec::new(),
                    options,
                )
            }
        }
    }

    fn build(
        htmls: Vec<String>,
        base_url: &str,
        net: Arc<dyn NetProvider>,
        metadata: DocumentMetadata,
        raw_toc: Vec<RawTocItem>,
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

        for (chunk_index, html) in htmls.iter().enumerate() {
            let doc = create_document(html, base_url, net.clone(), &font_data, options);
            let root = doc.root_element();
            let lines = collect_lines(root);
            let page_starts = paginate(
                &lines,
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

            for (local_index, start) in page_starts.iter().enumerate() {
                let clip_bottom = page_clip_bottom(&page_starts, local_index, options);
                let boxes = snapshot_page_boxes(root, &lines, *start, clip_bottom, line_base);
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
                    boxes,
                    xpointer,
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

        let document = Self {
            metadata,
            width: options.width,
            height: options.height,
            layout_height: options.layout_height,
            page_margin: options.page_margin,
            sources: htmls,
            base_url: base_url.to_string(),
            net,
            font_data,
            chunks,
            pages,
            xpointer_pages,
            href_pages,
            spine_to_chunk,
            toc,
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

    pub fn page_count_1based(&self) -> usize {
        self.page_count()
    }

    pub fn current_page(&self) -> usize {
        self.current_page
    }

    pub fn toc(&self) -> &[TocEntry] {
        &self.toc
    }

    pub fn render_page(&mut self, page: usize) -> Result<PageBitmap> {
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
        active.doc.set_viewport_scroll(Point {
            x: 0.0,
            y: f64::from(start.top),
        });
        let mut scene = Scene::default();
        blitz_paint::paint_scene(
            &mut scene,
            &mut active.doc,
            1.0,
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
            clip_top,
            clip_bottom,
        ))
    }

    pub fn render_current_page(&mut self) -> Result<PageBitmap> {
        self.render_page(self.current_page)
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
                font_path: None,
            },
        );
        let lines = collect_lines(doc.root_element());
        let page_starts = paginate(
            &lines,
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
        for page in start_hit.page..=end_hit.page {
            for (line_index, mut rect) in self.page_text_boxes_with_lines(page)? {
                if line_index < start_hit.line_index || line_index > end_hit.line_index {
                    continue;
                }
                if page == start_hit.page && line_index == start_hit.line_index {
                    let left = start_hit.point.x.clamp(rect.x, rect.x + rect.width);
                    rect.width = (rect.x + rect.width - left).max(0.0);
                    rect.x = left;
                }
                if page == end_hit.page && line_index == end_hit.line_index {
                    let right = end_hit.point.x.clamp(rect.x, rect.x + rect.width);
                    rect.width = (right - rect.x).max(0.0);
                }
                if rect.width > 0.0 {
                    rects.push(rect);
                }
            }
        }

        Ok(Some(Selection {
            xpointer_range: XPointerRange {
                start: start_hit.text_position,
                end: Some(end_hit.text_position),
                id: None,
            },
            rects,
        }))
    }

    pub fn find_text(&self, query: &str) -> Result<Vec<SearchHit>> {
        if query.is_empty() {
            bail!("search query must not be empty");
        }

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

            let mut search_start = 0;
            while let Some(relative) = page_text[search_start..].find(query) {
                let start = search_start + relative;
                let end = start + query.len();
                search_start = end;

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
            }
        }
        Ok(hits)
    }

    pub fn go_to_page(&mut self, page: usize) -> Result<usize> {
        if page < 1 || page > self.page_count() {
            bail!("page {page} is outside 1..={}", self.page_count());
        }
        self.current_page = page;
        Ok(page)
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

        let position = parse_text_position(trimmed)?;
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
    page_height: f32,
    margin_top: f32,
    margin_bottom: f32,
) -> Vec<PageStart> {
    let mut starts = vec![PageStart {
        top: 0.0,
        clip_top: margin_top,
    }];
    for line in lines {
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
    let config = DocumentConfig {
        viewport: Some(Viewport::new(
            options.width,
            options.layout_height,
            1.0,
            ColorScheme::Light,
        )),
        base_url: Some(base_url.to_string()),
        net_provider: Some(net),
        font_ctx: Some(build_single_font_ctx(font_data)),
        style_threading: StyleThreading::Sequential,
        ..Default::default()
    };

    let mut doc = HtmlDocument::from_html(html, config);
    doc.resolve(0.0);
    for _ in 0..4 {
        doc.handle_messages();
        doc.resolve(0.0);
    }
    doc
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

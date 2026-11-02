use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use anyrender::Scene;
use blitz_dom::{DocumentConfig, Node, NodeId, Point, StyleThreading};
use blitz_html::HtmlDocument;
use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request};
use blitz_traits::shell::{ColorScheme, Viewport};
use cre_epub::{
    EpubBook, HtmlOptions, LazyResource, PageMapItem as RawPageMapItem, TocItem as RawTocItem,
    normalize_path,
};
use cre_fb2::{Fb2Book, Fb2HtmlOptions};
use cre_kf8::{Kf8Book, Kf8HtmlOptions};
use cre_mobi::{MobiBook, MobiHtmlOptions};
use cre_paint::render_page as paint_page;
use cre_paint::render_page_rgba as paint_page_rgba;

pub use cre_paint::ColorPageBitmap;
pub use cre_paint::PageBitmap;
use cre_text::{TextBook, TextHtmlOptions};
use parley::FontContext;
use parley::editing::{Cursor, Selection as ParleySelection};
use parley::fontique::{Blob, Collection, CollectionOptions, FamilyId, GenericFamily, SourceCache};
type ClippedLine = (usize, usize, TextBox, usize, usize);

use parley::layout::Affinity;
use parley::layout::Cluster;
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
    Html,
    /// MOBI 6 (`.mobi`, `.prc`, and `.azw` files with a MOBI 6 payload).
    Mobi,
    /// KF8 (`.azw3`, and `.azw` files with a KF8 payload).
    Azw3,
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
    /// Family the reader selected. A font file can be a collection (TTC/OTC)
    /// holding several families, so the path alone does not say which face to
    /// use; this name is put first in the generic family lists, which is how the
    /// generators' `serif`/`sans-serif` resolve to it.
    pub font_family: Option<String>,
    pub fallback_font_paths: Vec<PathBuf>,
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
            font_family: None,
            fallback_font_paths: Vec::new(),
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

/// A byte range on one rendered line, used to build its box with parley's
/// selection geometry.
struct WordSpan<'a> {
    page: usize,
    line_index: usize,
    line: &'a TextBox,
    start_byte: usize,
    end_byte: usize,
    text: &'a str,
}

/// A word found while scanning, before its box is built (building the box
/// borrows the active chunk again, which cannot happen inside the scan).
struct PendingWord {
    page: usize,
    line_index: usize,
    line: TextBox,
    start_byte: usize,
    end_byte: usize,
    start: TextPosition,
    end: Option<TextPosition>,
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

#[derive(Debug)]
pub(crate) struct LineBox {
    pub(crate) node_id: NodeId,
    pub(crate) text_start: usize,
    pub(crate) text_end: usize,
    pub(crate) source_element_id: Option<NodeId>,
    pub(crate) left: f32,
    pub(crate) right: f32,
    pub(crate) top: f32,
    pub(crate) visual_top: f32,
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

/// Resident cost of keeping a book laid out, per byte of generated HTML
/// (DOM nodes, computed styles and parley line boxes). Measured whole-book
/// layouts: 251 kB of XHTML cost 28 MB, 579 kB cost 44 MB. 128 B/B is the
/// safe upper bound.
const LAYOUT_BYTES_PER_HTML_BYTE: usize = 128;

/// Memory left untouched for KOReader's own post-open work (widgets, caches,
/// thumbnails) and for the page bitmap handed back to it.
const LAYOUT_MEMORY_RESERVE: usize = 24 * 1024 * 1024;

/// Bounds for a layout chunk: below a page's worth of text chunking only adds
/// relayouts; above this a single layout is both slow (blitz's layout cost
/// grows faster than the text: 9.7 s for a 251 kB book as one document, versus
/// ~4.4 s in four chunks) and expensive in memory. Pagination is unaffected as
/// long as chunks end at spine-item boundaries, because every spine item
/// starts a page anyway.
const MIN_CHUNK_BYTES: usize = 64 * 1024;
const MAX_CHUNK_BYTES: usize = 128 * 1024;

/// Explicit chunk size override (`CRE_CHUNK_BYTES`), for testing and for
/// forcing chunking on a device where the automatic choice misjudges.
fn chunk_target_override() -> Option<usize> {
    std::env::var("CRE_CHUNK_BYTES")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value >= 16 * 1024)
}

/// Bytes the kernel says it can still hand out, if the platform reports it.
fn available_memory_bytes() -> Option<usize> {
    // Testing knob: pretend the device only has this much memory, so the
    // chunk planner can be exercised on a desktop.
    if let Ok(value) = std::env::var("CRE_AVAILABLE_MB")
        && let Ok(mb) = value.trim().parse::<usize>()
        && mb > 0
    {
        return Some(mb.saturating_mul(1024 * 1024));
    }
    #[cfg(target_os = "linux")]
    {
        let info = std::fs::read_to_string("/proc/meminfo").ok()?;
        let field = |key: &str| -> Option<usize> {
            info.lines()
                .find(|line| line.starts_with(key))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse::<usize>().ok())
        };
        // MemAvailable exists since Linux 3.14; the Kindle PW2 runs 3.0, so
        // fall back to the classic free+buffers+cached estimate there.
        if let Some(available) = field("MemAvailable:") {
            return Some(available.saturating_mul(1024));
        }
        let free = field("MemFree:")?;
        let buffers = field("Buffers:").unwrap_or(0);
        let cached = field("Cached:").unwrap_or(0);
        return Some((free + buffers + cached).saturating_mul(1024));
    }
    #[allow(unreachable_code)]
    None
}

/// Splits a format that is produced as one HTML document (FB2, TXT, a
/// standalone HTML file) into layout chunks when it would not fit in one,
/// using the same memory budget as EPUB chunking.
fn split_single_document(html: String) -> Vec<String> {
    let target = match chunk_target_override() {
        Some(target) => target,
        None => plan_chunk_bytes(html.len(), available_memory_bytes()),
    };
    if target == usize::MAX || html.len() <= target {
        return vec![html];
    }
    cre_epub::split_html_document(&html, target)
}

/// Text and image budget for one layout chunk, from the memory the kernel can
/// still hand out.
///
/// The text budget uses [`LAYOUT_BYTES_PER_HTML_BYTE`]; the image budget is in
/// estimated decoded bytes, because blitz decodes a chunk's images while
/// parsing it (a dense illustrated book costs far more than its text).
fn chunk_budgets(available: Option<usize>) -> (usize, Option<usize>) {
    let Some(available) = available else {
        return (usize::MAX, None);
    };
    let budget = available
        .saturating_sub(LAYOUT_MEMORY_RESERVE)
        .min(available / 2)
        .max(MIN_CHUNK_BYTES * LAYOUT_BYTES_PER_HTML_BYTE);
    let text = (budget / LAYOUT_BYTES_PER_HTML_BYTE).clamp(MIN_CHUNK_BYTES, MAX_CHUNK_BYTES);
    // Images get 60% of the layout budget when there is plenty of memory, but
    // only a third on a small device: a chunk activation (e.g. jumping to
    // another part of the book) has to fit next to the reader UI and the page
    // cache, not just next to the text layout.
    let image_share = if available < 128 * 1024 * 1024 {
        budget / 3
    } else {
        (budget * 3) / 5
    };
    let mut images = image_share;
    if let Ok(value) = std::env::var("CRE_IMAGE_BUDGET_MB")
        && let Ok(mb) = value.trim().parse::<usize>()
        && mb > 0
    {
        images = mb * 1024 * 1024;
    }
    (text, Some(images))
}

/// Chooses the layout chunk size for `total_html` bytes of generated content.
///
/// `usize::MAX` means "lay the whole book out at once", like crengine: page
/// breaks, page numbers and TOC targets then match the original engine and
/// reading never re-lays out a chunk mid-book. When the book is too large for
/// the memory actually available, the layout is split so that one chunk's DOM
/// fits in roughly half of what the kernel can hand out.
fn plan_chunk_bytes(total_html: usize, available: Option<usize>) -> usize {
    let Some(available) = available else {
        // No memory information: trust the device rather than degrading the
        // layout for everyone.
        return usize::MAX;
    };
    // Never plan to use more than half of what is available, and always leave
    // the reserve free: the layout spike happens while KOReader is also
    // allocating for the reader UI, and the kernel needs cache headroom.
    let budget = available
        .saturating_sub(LAYOUT_MEMORY_RESERVE)
        .min(available / 2)
        .max(MIN_CHUNK_BYTES * LAYOUT_BYTES_PER_HTML_BYTE);
    let max_html = budget / LAYOUT_BYTES_PER_HTML_BYTE;
    if total_html <= max_html {
        return usize::MAX;
    }
    max_html.clamp(MIN_CHUNK_BYTES, MAX_CHUNK_BYTES)
}

/// Returns freed heap pages to the OS. Building a chunk's DOM/parley layout
/// allocates and frees tens of megabytes; glibc keeps those arenas resident,
/// which matters on a 256 MB device where the next page render can then OOM.
fn trim_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::malloc_trim(0);
    }
}

const PAGE_IMAGE_CACHE_CAPACITY: usize = 2usize;

struct ChunkMeta {
    first_page: usize,
    line_base: usize,
}

struct ActiveChunk {
    index: usize,
    doc: HtmlDocument,
    lines: Vec<LineBox>,
    page_starts: Vec<PageStart>,
    /// Text nodes that took part in a block's inline layout. Hidden nodes
    /// (`display: none`) are not in here, so text navigation never moves into
    /// text that is not rendered.
    rendered_text_nodes: std::collections::HashSet<NodeId>,
}

#[derive(Clone)]
struct PageImage {
    bitmap: Arc<PageBitmap>,
    image_count: usize,
    image_surface: usize,
}

struct PageMeta {
    chunk: usize,
    local_start: PageStart,
    local_clip_bottom: f32,
    doc_top: f32,
    render_height: f32,
    xpointer: Option<String>,
    page_flow: i64,
}

/// One chunk's layout, in the chunk's own coordinates.
///
/// Keeping this per chunk (instead of only the flattened page list) is what
/// makes a settings change cheap: the chunks that are not being read keep their
/// pages as a provisional page list and are laid out again only when they are
/// needed, like crengine's `ldomDocument::partialRender` for a `DocFragment`.
/// One line's place in the book: the text node holding its first character, the
/// offset in it and the 0-based local page. Kept XPointer-free on purpose: a
/// path is only built for the first line of each page.
#[derive(Clone)]
struct LineEntry {
    block: NodeId,
    /// Byte offset of the line's first character in the block's layout text.
    offset: u32,
    page: usize,
}

struct ChunkLayout {
    pages: Vec<LocalPage>,
    /// Every line of the chunk, sorted in document order (by text node id, then
    /// offset) — the key page lookup binary-searches.
    lines: Vec<LineEntry>,
    /// Page-start XPointer per local page.
    xpointers: Vec<Option<String>>,
    /// Anchors found in this chunk, to a 0-based local page.
    href_pages: HashMap<(String, Option<String>), usize>,
    /// (spine slot, real spine index) of every spine item in this chunk.
    spine_slots: Vec<(u32, u32)>,
    line_count: usize,
    max_flow: i64,
}

struct LocalPage {
    start: PageStart,
    clip_bottom: f32,
    render_height: f32,
    page_flow: i64,
}

impl ChunkLayout {
    /// Rescales a provisional page list to `target` pages.
    ///
    /// Used after a settings change so the page count of the chunks that are not
    /// laid out yet stays in the right ballpark; the pages themselves are
    /// redone as soon as the chunk is read. Entries are sampled from the old
    /// list, which keeps their order (and therefore XPointer ordering) intact.
    fn resample(&mut self, target: usize) {
        let current = self.pages.len();
        if current == 0 || target == 0 || target == current {
            return;
        }
        let mut pages = Vec::with_capacity(target);
        let mut xpointers = Vec::with_capacity(target);
        for index in 0..target {
            let source = index * current / target;
            let page = &self.pages[source];
            pages.push(LocalPage {
                start: page.start,
                clip_bottom: page.clip_bottom,
                render_height: page.render_height,
                page_flow: page.page_flow,
            });
            xpointers.push(self.xpointers.get(source).cloned().flatten());
        }
        for local_page in self.href_pages.values_mut() {
            *local_page = (*local_page * target / current).min(target - 1);
        }
        self.pages = pages;
        self.xpointers = xpointers;
    }
}

struct ChunkEntry {
    /// `None` before this chunk has ever been laid out.
    layout: Option<ChunkLayout>,
    /// True when `layout` predates the current settings: its pages still stand
    /// in for real ones, but the chunk is laid out again when it is next used.
    stale: bool,
}

/// Absolute vertical extent of a replaced element (image) inside a chunk's
/// layout, so pagination can keep it on one page instead of clipping it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ContentBox {
    pub(crate) top: f32,
    pub(crate) bottom: f32,
}

#[derive(Clone)]
struct SnapshotLine {
    text: String,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    global_line: usize,
}

pub enum ResourceSource {
    Empty,
    /// Resources carried in memory, by name (FB2 binaries, MOBI/KF8 images).
    Map(Arc<HashMap<String, Vec<u8>>>),
    Epub(Arc<LazyResource>),
}

pub struct Document {
    metadata: DocumentMetadata,
    width: u32,
    height: u32,
    layout_height: u32,
    page_margin: f32,
    font_css_size: f32,
    viewport_scale: f32,
    font_gamma: f32,
    stylesheet: Option<String>,
    /// Family the reader picked inside the font file (a collection can hold
    /// several); `None` means "first family in the file".
    font_family: Option<String>,
    /// The blobs the current font context was built from. Kept (they are
    /// mmapped and shared) so a family change can rebuild the context without
    /// re-reading the files: two faces of a collection live in one file, so the
    /// path list alone does not say whether the context is stale.
    font_blobs: Vec<FontBlob>,
    embedded_styles_enabled: bool,
    sources: Vec<String>,
    base_url: String,
    resources: ResourceSource,
    cover_image: Option<Vec<u8>>,
    has_non_linear_flows: bool,
    net: Arc<dyn NetProvider>,
    font_context: Arc<FontContext>,
    /// Every chunk's own layout, parallel to `sources`.
    entries: Vec<ChunkEntry>,
    /// True when the chunk HTML carries no layout settings of its own, so new
    /// settings can be applied by laying the existing chunks out again instead
    /// of regenerating (and possibly re-chunking) the document.
    layout_independent_sources: bool,
    chunks: Vec<ChunkMeta>,
    pages: Vec<PageMeta>,
    xpointer_pages: HashMap<String, usize>,
    href_pages: HashMap<(String, Option<String>), usize>,
    spine_to_chunk: HashMap<u32, usize>,
    /// Spine slot -> EPUB spine index. They differ when a spine item is split
    /// into several parts, which get their own slot so XPointers stay unique.
    spine_real_index: HashMap<u32, u32>,
    toc: Vec<TocEntry>,
    page_map_source: String,
    page_map: Vec<PageMapEntry>,
    /// Raw TOC/page-map items, kept until the layout can give them pages.
    raw_toc: Vec<RawTocItem>,
    raw_page_map_items: Vec<RawPageMapItem>,
    /// False until the pages have been laid out by [`Document::ensure_layout`].
    layout_done: bool,
    active: RefCell<Option<ActiveChunk>>,
    current_page: usize,
    background_color: u32,
    page_image_cache: RefCell<HashMap<usize, PageImage>>,
    page_cache_order: RefCell<Vec<usize>>,
    page_boxes_cache: RefCell<Option<(usize, Vec<SnapshotLine>)>>,
}

impl Document {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::with_options(path, &DocumentOptions::default())
    }

    pub fn with_options(path: impl AsRef<Path>, options: &DocumentOptions) -> Result<Self> {
        let font_datas = collect_font_datas(options)?;
        Self::with_options_with_font_data(path, options, &font_datas)
    }

    pub fn with_options_with_font_data(
        path: impl AsRef<Path>,
        options: &DocumentOptions,
        font_datas: &[FontBlob],
    ) -> Result<Self> {
        Self::open_inner(path, options, font_datas, false)
    }

    /// Parses the document but defers laying out its pages to
    /// [`Self::ensure_layout`].
    ///
    /// KOReader applies every reading setting between `loadDocument` and
    /// `renderDocument`. Parsing eagerly and laying out lazily means the one
    /// layout that happens uses the final settings, instead of laying the book
    /// out with the defaults and then all over again with the user's font,
    /// margins and so on (~6 s each on a Kindle PW2).
    pub fn with_options_with_font_data_parsed(
        path: impl AsRef<Path>,
        options: &DocumentOptions,
        font_datas: &[FontBlob],
    ) -> Result<Self> {
        Self::open_inner(path, options, font_datas, true)
    }

    fn open_inner(
        path: impl AsRef<Path>,
        options: &DocumentOptions,
        font_datas: &[FontBlob],
        defer_layout: bool,
    ) -> Result<Self> {
        validate_options(options)?;
        let path = path.as_ref();
        let format = detect_format(path);
        // SVG images are rendered at page size: CSS caps them to the page's
        // content box, so this is the largest they can ever be drawn.
        let content_width = ((options.width as f32 - options.page_margin * 2.0)
            * options.viewport_scale)
            .max(1.0) as u32;
        let content_height = ((options.height as f32 - options.page_margin * 2.0)
            * options.viewport_scale)
            .max(1.0) as u32;
        let svg_raster = Arc::new(SvgRaster::new(content_width, content_height));
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
                    options.width as f32 / options.viewport_scale.max(0.1),
                    options.height as f32 / options.viewport_scale.max(0.1),
                    html_options.margin_x,
                    html_options.margin_y,
                );
                let htmls = match chunk_target_override() {
                    Some(target) => book.to_html_chunks(&html_options, target)?,
                    None => {
                        let (text_budget, image_budget) = chunk_budgets(available_memory_bytes());
                        book.to_html_chunks_budgeted(&html_options, text_budget, image_budget)?
                    }
                };
                let cover_image = epub_cover_image(&book);
                let has_non_linear_flows = book.spine.iter().any(|item| !item.linear);
                let resource_provider = Arc::new(book.resource_provider);
                Self::build(
                    htmls,
                    "epub://book/",
                    Arc::new(EpubNet {
                        svg: svg_raster.clone(),
                        resource_provider: resource_provider.clone(),
                    }),
                    ResourceSource::Epub(resource_provider),
                    cover_image,
                    has_non_linear_flows,
                    metadata,
                    raw_toc,
                    raw_page_map_source,
                    raw_page_map,
                    options,
                    font_datas,
                    defer_layout,
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
                let binary = Arc::new(book.binary);
                Self::build(
                    split_single_document(html),
                    "fb2://book/",
                    Arc::new(MapNet {
                        resources: binary.clone(),
                        svg: svg_raster.clone(),
                    }),
                    ResourceSource::Map(binary),
                    cover_image,
                    false,
                    metadata,
                    raw_toc,
                    String::new(),
                    Vec::new(),
                    options,
                    font_datas,
                    defer_layout,
                )
            }
            DocumentFormat::Mobi => {
                let book = MobiBook::open(path)?;
                let metadata = DocumentMetadata {
                    format,
                    title: book.title.clone(),
                    creator: book.creator.clone(),
                    language: book.language.clone(),
                    series: None,
                    description: book.description.clone(),
                    keywords: book.keywords.clone(),
                    identifiers: book.identifiers.clone(),
                    item_count: book.section_count,
                };
                let html_options = MobiHtmlOptions {
                    font_size: options.font_size.max(1.0),
                    ..MobiHtmlOptions::default()
                };
                let (html, toc) = book.to_html_with_toc(&html_options);
                let raw_toc = toc
                    .into_iter()
                    .map(|item| RawTocItem {
                        title: item.title,
                        href: item.href,
                        depth: item.depth,
                    })
                    .collect();
                let cover_image = book.cover_image.clone();
                let resources = Arc::new(book.resources);
                Self::build(
                    split_single_document(html),
                    "mobi://book/",
                    Arc::new(MapNet {
                        resources: resources.clone(),
                        svg: svg_raster.clone(),
                    }),
                    ResourceSource::Map(resources),
                    cover_image,
                    false,
                    metadata,
                    raw_toc,
                    String::new(),
                    Vec::new(),
                    options,
                    font_datas,
                    defer_layout,
                )
            }
            DocumentFormat::Azw3 => {
                let book = Kf8Book::open(path)?;
                let metadata = DocumentMetadata {
                    format,
                    title: book.title.clone(),
                    creator: book.creator.clone(),
                    language: book.language.clone(),
                    series: None,
                    description: book.description.clone(),
                    keywords: book.keywords.clone(),
                    identifiers: book.identifiers.clone(),
                    item_count: book.section_count,
                };
                let html_options = Kf8HtmlOptions {
                    font_size: options.font_size.max(1.0),
                    ..Kf8HtmlOptions::default()
                };
                let (html, toc) = book.to_html_with_toc(&html_options);
                let raw_toc = toc
                    .into_iter()
                    .map(|item| RawTocItem {
                        title: item.title,
                        href: item.href,
                        depth: item.depth,
                    })
                    .collect();
                let cover_image = book.cover_image.clone();
                let resources = Arc::new(book.resources);
                Self::build(
                    split_single_document(html),
                    "azw3://book/",
                    Arc::new(MapNet {
                        resources: resources.clone(),
                        svg: svg_raster.clone(),
                    }),
                    ResourceSource::Map(resources),
                    cover_image,
                    false,
                    metadata,
                    raw_toc,
                    String::new(),
                    Vec::new(),
                    options,
                    font_datas,
                    defer_layout,
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
                    split_single_document(html),
                    "text://book/",
                    Arc::new(EmptyNet),
                    ResourceSource::Empty,
                    None,
                    false,
                    metadata,
                    Vec::new(),
                    String::new(),
                    Vec::new(),
                    options,
                    font_datas,
                    defer_layout,
                )
            }
            DocumentFormat::Html => {
                // A standalone HTML/XHTML file is used as-is; relative
                // resources (images, extra CSS) are loaded from its directory.
                let bytes =
                    std::fs::read(path).with_context(|| format!("read html {}", path.display()))?;
                let mut html = String::from_utf8_lossy(&bytes).into_owned();
                // Mirror the generators: give bare HTML the same base
                // typography and page padding EPUB/FB2 chunks get, so margins
                // and font size behave the same and the first line is not
                // laid out above the page's top margin.
                let (_, _, page_margin_css) = css_page_geometry(
                    options.width,
                    options.height,
                    options.viewport_scale,
                    options.page_margin,
                );
                html.push_str(&format!(
                    "<style>/* cre-render-base */\n\
                     html, body {{ margin: 0; padding: 0; }}\n\
                     body {{ font-family: serif; font-size: {}px; line-height: 1.2; \
                     padding: {:.3}px; }}\n\
                     img {{ max-width: 100%; height: auto; }}</style>",
                    options.font_size,
                    page_margin_css.max(0.0)
                ));
                let directory = path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."));
                let root = std::fs::canonicalize(&directory).unwrap_or(directory);
                let base_url = url::Url::from_directory_path(&root)
                    .map(|url| url.to_string())
                    .unwrap_or_else(|_| "file:///".to_string());
                let title = path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let metadata = DocumentMetadata {
                    format,
                    title: Some(title),
                    creator: None,
                    language: None,
                    series: None,
                    description: None,
                    keywords: None,
                    identifiers: None,
                    item_count: 1,
                };
                Self::build(
                    split_single_document(html),
                    &base_url,
                    Arc::new(FileNet {
                        root: root.clone(),
                        svg: svg_raster.clone(),
                    }),
                    ResourceSource::Empty,
                    None,
                    false,
                    metadata,
                    Vec::new(),
                    String::new(),
                    Vec::new(),
                    options,
                    font_datas,
                    defer_layout,
                )
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        htmls: Vec<String>,
        base_url: &str,
        net: Arc<dyn NetProvider>,
        resources: ResourceSource,
        cover_image: Option<Vec<u8>>,
        has_non_linear_flows: bool,
        metadata: DocumentMetadata,
        raw_toc: Vec<RawTocItem>,
        page_map_source: String,
        page_map_items: Vec<RawPageMapItem>,
        options: &DocumentOptions,
        font_datas: &[FontBlob],
        defer_layout: bool,
    ) -> Result<Self> {
        let font_context = Arc::new(build_font_ctx_with_fallbacks(
            font_datas,
            options.font_family.as_deref(),
        ));
        // Only the EPUB generator emits chunk HTML without the layout settings
        // in it, which is what allows a relayout instead of a reopen.
        let layout_independent_sources = matches!(metadata.format, DocumentFormat::Epub);
        let mut document = Self {
            metadata,
            width: options.width,
            height: options.height,
            layout_height: options.layout_height,
            page_margin: options.page_margin,
            stylesheet: options.stylesheet.clone(),
            embedded_styles_enabled: options.embedded_styles_enabled,
            font_css_size: options.font_size,
            font_family: options.font_family.clone(),
            font_blobs: font_datas.to_vec(),
            viewport_scale: options.viewport_scale,
            font_gamma: 1.0,
            sources: htmls,
            base_url: base_url.to_string(),
            resources,
            cover_image,
            has_non_linear_flows,
            net,
            font_context,
            entries: Vec::new(),
            layout_independent_sources,
            chunks: Vec::new(),
            pages: Vec::new(),
            xpointer_pages: HashMap::new(),
            href_pages: HashMap::new(),
            spine_to_chunk: HashMap::new(),
            spine_real_index: HashMap::new(),
            toc: Vec::new(),
            page_map_source,
            page_map: Vec::new(),
            raw_toc,
            raw_page_map_items: page_map_items,
            layout_done: false,
            active: RefCell::new(None),
            current_page: 1,
            background_color: 0xFF_FFFF,
            page_image_cache: RefCell::new(HashMap::new()),
            page_cache_order: RefCell::new(Vec::new()),
            page_boxes_cache: RefCell::new(None),
        };
        if !defer_layout {
            document.ensure_layout()?;
        }
        Ok(document)
    }

    /// The options used to lay chunk HTML out, built from the current settings.
    fn layout_options(&self) -> DocumentOptions {
        DocumentOptions {
            width: self.width,
            height: self.height,
            layout_height: self.layout_height,
            page_margin: self.page_margin,
            font_size: self.font_css_size,
            font_path: None,
            font_family: self.font_family.clone(),
            fallback_font_paths: Vec::new(),
            stylesheet: self.stylesheet.clone(),
            embedded_styles_enabled: self.embedded_styles_enabled,
            viewport_scale: self.viewport_scale,
        }
    }

    fn apply_options(&mut self, options: &DocumentOptions) {
        self.width = options.width;
        self.height = options.height;
        self.layout_height = options.layout_height;
        self.page_margin = options.page_margin;
        self.font_css_size = options.font_size;
        if self.font_family != options.font_family {
            self.font_family = options.font_family.clone();
            self.rebuild_font_context();
        }
        self.stylesheet = options.stylesheet.clone();
        self.embedded_styles_enabled = options.embedded_styles_enabled;
        self.viewport_scale = options.viewport_scale;
    }

    /// Replaces the fonts used by the next chunk layout. Chunks already laid out
    /// keep their own copy until they are laid out again.
    pub fn set_fonts(&mut self, font_datas: &[FontBlob]) {
        self.font_blobs = font_datas.to_vec();
        self.rebuild_font_context();
    }

    /// Rebuilds the layout font context from the blobs it already holds, for
    /// when the selected family changed but the files did not (a collection
    /// holds several families in one file).
    fn rebuild_font_context(&mut self) {
        self.font_context = Arc::new(build_font_ctx_with_fallbacks(
            &self.font_blobs,
            self.font_family.as_deref(),
        ));
        *self.active.borrow_mut() = None;
    }

    /// Lays out one chunk with the current settings.
    ///
    /// The built DOM is returned too: the caller that is about to draw from this
    /// chunk installs it as the active chunk, instead of parsing the HTML a
    /// second time.
    fn layout_chunk(
        &self,
        chunk_index: usize,
    ) -> Result<(ChunkLayout, HtmlDocument, Vec<LineBox>, Vec<PageStart>)> {
        let started = std::time::Instant::now();
        let html = self
            .sources
            .get(chunk_index)
            .context("document chunk is missing")?
            .clone();
        let options = self.layout_options();
        let doc = create_document(
            &html,
            &self.base_url,
            self.net.clone(),
            self.font_context.clone(),
            &options,
        );
        let root = doc.root_element();
        let lines = collect_lines(root);
        let mut line_indices: HashMap<NodeId, Vec<usize>> = HashMap::new();
        for (local_line, line) in lines.iter().enumerate() {
            line_indices
                .entry(line.node_id)
                .or_default()
                .push(local_line);
        }
        let mut max_flow = 0i64;
        let mut line_flows = lines.iter().map(|_| 0i64).collect::<Vec<i64>>();
        collect_line_flows(root, 0, &mut max_flow, &line_indices, &mut line_flows);
        let forced_breaks = page_breaks::collect_forced_breaks(root);
        let boxes = collect_content_boxes(&doc, root);
        let (page_height_css_u32, page_height_css, page_margin_css) = css_page_geometry(
            options.width,
            options.height,
            options.viewport_scale,
            options.page_margin,
        );
        let page_starts = paginate(
            &lines,
            &boxes,
            &forced_breaks,
            page_height_css,
            page_margin_css,
            page_margin_css,
        );
        // Every line's place in the book: the block that holds it and the byte
        // offset of its first character inside that block's text. Block ids
        // increase with document order, so `(block, offset)` orders positions the
        // way the document does — which `TextPosition` cannot. Recording it is
        // just the line's own fields, so it is cheap.
        let mut line_positions: Vec<LineEntry> = Vec::with_capacity(lines.len());
        let mut page = 0usize;
        for line in lines.iter() {
            while page + 1 < page_starts.len() && line.top + 0.5 >= page_starts[page + 1].clip_top {
                page += 1;
            }
            // The key is the line's block and its own byte offset in that
            // block's layout text. Both sides of the page lookup use
            // `LayoutTextMap`, so they agree for every block (floats,
            // collapsed whitespace, text-transform included).
            line_positions.push(LineEntry {
                block: line.node_id,
                offset: line.text_start.min(u32::MAX as usize) as u32,
                page,
            });
        }
        line_positions.sort_by(|left, right| {
            left.block
                .cmp(&right.block)
                .then(left.offset.cmp(&right.offset))
                .then(left.page.cmp(&right.page))
        });
        line_positions
            .dedup_by(|left, right| left.block == right.block && left.offset == right.offset);
        // Page starts are the first line position of each page, taken from the
        // same table the page lookup uses, so a page start and the words on that
        // page cannot disagree. Only one XPointer per page is built.
        let mut local_xpointer_pages: HashMap<String, usize> = HashMap::new();
        let mut page_positions: Vec<Option<String>> = vec![None; page_starts.len()];
        for entry in line_positions.iter() {
            let Some(slot) = page_positions.get_mut(entry.page) else {
                continue;
            };
            if slot.is_some() {
                continue;
            }
            *slot = xpointer::position_for_layout_offset(
                root,
                root.with(entry.block),
                entry.offset as usize,
            )
            .map(|position| position.to_xpointer());
        }
        // A page that holds only images can have no line with a text offset.
        // Use the same fallback the on-demand lookup uses, and store it in the
        // page table, so `xpointer_from_page` and `page_from_xpointer` agree.
        for (index, slot) in page_positions.iter_mut().enumerate() {
            if slot.is_none()
                && let Ok(xpointer) = xpointer::xpointer_from_page(
                    &doc,
                    &lines,
                    &page_starts,
                    index + 1,
                    page_height_css_u32,
                    page_margin_css,
                )
            {
                *slot = Some(xpointer);
            }
        }
        let xpointers = page_positions;
        for (index, xpointer) in xpointers.iter().enumerate() {
            if let Some(xpointer) = xpointer {
                local_xpointer_pages.insert(xpointer.clone(), index + 1);
            }
        }
        let mut href_pages = HashMap::new();
        for (key, node_id) in collect_anchor_maps(root) {
            // A file-level target is a spine document. Its forced page break is
            // applied at the first line that carries text, not at the section
            // top, so use the first page start at or after the section top.
            // Fragment targets keep the page that contains the node.
            let local_page = if key.1.is_none() {
                page_for_spine_node(&doc, node_id, &lines, &boxes, &page_starts)
            } else {
                page_for_node(&doc, node_id, &page_starts).ok()
            };
            if let Some(local_page) = local_page {
                href_pages.entry(key).or_insert(local_page - 1);
            }
        }
        let mut spine_slots = Vec::new();
        {
            let mut spine_to_chunk = HashMap::new();
            let mut spine_real_index = HashMap::new();
            collect_spine_chunks(
                root,
                chunk_index,
                &mut spine_to_chunk,
                &mut spine_real_index,
            );
            spine_slots.extend(
                spine_to_chunk
                    .keys()
                    .map(|slot| (*slot, *spine_real_index.get(slot).unwrap_or(slot))),
            );
            spine_slots.sort_unstable();
        }
        let mut pages = Vec::with_capacity(page_starts.len());
        let mut previous_page_flow = 0;
        for (local_index, start) in page_starts.iter().enumerate() {
            let clip_bottom =
                page_chunk_clip_bottom(&page_starts, local_index, page_height_css, page_margin_css);
            let page_flow = first_non_blank_line_flow(
                root,
                &lines,
                *start,
                clip_bottom,
                line_flows.as_slice(),
                previous_page_flow,
            );
            previous_page_flow = page_flow;
            let render_height =
                ((clip_bottom - start.top).max(0.0) * options.viewport_scale).max(0.0);
            pages.push(LocalPage {
                start: *start,
                clip_bottom,
                render_height,
                page_flow,
            });
        }
        let layout = ChunkLayout {
            pages,
            lines: line_positions,
            xpointers,
            href_pages,
            spine_slots,
            line_count: lines.len(),
            max_flow,
        };
        if std::env::var_os("CRE_LAYOUT_TIMING").is_some() {
            eprintln!(
                "cre-lib: chunk {chunk_index} laid out in {:.0} ms ({} pages, {} lines)",
                started.elapsed().as_secs_f64() * 1000.0,
                layout.pages.len(),
                layout.line_count
            );
        }
        Ok((layout, doc, lines, page_starts))
    }

    /// Lays out `chunk_index` and keeps its DOM active, ready to draw.
    fn lay_out_chunk_now(&mut self, chunk_index: usize) -> Result<()> {
        let (layout, doc, lines, page_starts) = self.layout_chunk(chunk_index)?;
        if let Some(entry) = self.entries.get_mut(chunk_index) {
            *entry = ChunkEntry {
                layout: Some(layout),
                stale: false,
            };
        }
        *self.active.borrow_mut() = Some(ActiveChunk {
            index: chunk_index,
            rendered_text_nodes: rendered_text_nodes(&doc, &lines),
            doc,
            lines,
            page_starts,
        });
        *self.page_boxes_cache.borrow_mut() = None;
        self.clear_page_image_cache();
        Ok(())
    }

    /// Rebuilds the flattened page list, TOC page numbers and page map from the
    /// per-chunk layouts.
    fn rebuild_view(&mut self) {
        self.pages.clear();
        self.chunks.clear();
        self.xpointer_pages.clear();
        self.href_pages.clear();
        self.spine_to_chunk.clear();
        self.spine_real_index.clear();
        let mut line_base = 0usize;
        let mut running_doc_top = 0.0f32;
        let mut max_flow = 0i64;
        for (chunk_index, entry) in self.entries.iter().enumerate() {
            let first_page = self.pages.len() + 1;
            if let Some(layout) = entry.layout.as_ref() {
                for (local_index, page) in layout.pages.iter().enumerate() {
                    let doc_top = running_doc_top;
                    running_doc_top += page.render_height;
                    let xpointer = layout.xpointers.get(local_index).cloned().flatten();
                    if let Some(xpointer) = xpointer.as_ref() {
                        self.xpointer_pages
                            .insert(xpointer.clone(), first_page + local_index);
                    }
                    self.pages.push(PageMeta {
                        chunk: chunk_index,
                        local_start: page.start,
                        local_clip_bottom: page.clip_bottom,
                        doc_top,
                        render_height: page.render_height,
                        xpointer,
                        page_flow: page.page_flow,
                    });
                }
                for (key, local_page) in layout.href_pages.iter() {
                    self.href_pages
                        .entry(key.clone())
                        .or_insert(first_page + local_page);
                }
                for (slot, real) in layout.spine_slots.iter() {
                    self.spine_to_chunk.insert(*slot, chunk_index);
                    self.spine_real_index.insert(*slot, *real);
                }
                max_flow = max_flow.max(layout.max_flow);
            }
            self.chunks.push(ChunkMeta {
                first_page,
                line_base,
            });
            line_base += entry
                .layout
                .as_ref()
                .map(|layout| layout.line_count)
                .unwrap_or(0);
        }
        self.spine_to_chunk.entry(1).or_insert(0);

        let toc = self
            .raw_toc
            .iter()
            .map(|item| {
                let page = page_for_href(&self.href_pages, &item.href).ok();
                let xpointer = page.and_then(|page| {
                    self.pages
                        .get(page.saturating_sub(1))
                        .and_then(|snapshot| snapshot.xpointer.clone())
                });
                TocEntry {
                    title: item.title.clone(),
                    href: item.href.clone(),
                    depth: item.depth,
                    page,
                    xpointer,
                }
            })
            .collect();
        self.toc = toc;

        let mut page_map: Vec<PageMapEntry> = self
            .raw_page_map_items
            .iter()
            .filter_map(|item| {
                let page = page_for_href(&self.href_pages, &item.href).ok()?;
                let snapshot = self.pages.get(page.saturating_sub(1));
                let xpointer = snapshot.and_then(|snapshot| snapshot.xpointer.clone());
                let doc_y = snapshot
                    .map(|snapshot| snapshot.doc_top.max(0.0).round() as i64)
                    .unwrap_or(page.saturating_sub(1) as i64 * self.height as i64);
                Some(PageMapEntry {
                    page,
                    xpointer,
                    doc_y,
                    label: item.label.clone(),
                })
            })
            .collect();
        page_map.sort_by_key(|entry| (entry.doc_y, entry.page));
        self.page_map = page_map;

        if max_flow > 0 {
            self.has_non_linear_flows = true;
        }
    }

    /// True when these settings can be applied without regenerating the chunk
    /// HTML, i.e. [`Self::relayout`] can do the work.
    pub fn can_relayout_with(&self, options: &DocumentOptions) -> bool {
        self.layout_independent_sources
            && self.layout_done
            && !self.sources.is_empty()
            && !self.entries.is_empty()
            // Image normalisation and the page-size CSS are baked into the chunk
            // HTML, so a different page size needs a reopen.
            && options.width == self.width
            && options.height == self.height
    }

    /// Applies new layout settings without re-parsing the document.
    ///
    /// Only the chunk holding the current position is laid out now; every other
    /// chunk keeps its previous pages as a provisional page list and is laid out
    /// again the first time it is needed (see [`Self::ensure_page_layout`]).
    /// That turns a font change on a large book from "lay out the whole book"
    /// into "lay out the chapter being read", which is what crengine's partial
    /// rerendering does. Returns `false` when the change needs a reopen.
    pub fn relayout(&mut self, options: &DocumentOptions) -> Result<bool> {
        if !self.can_relayout_with(options) {
            return Ok(false);
        }
        // The chunk being read is laid out again straight away, so the page the
        // reader is on is exact; everything else stays provisional.
        let anchor = self
            .current_page
            .saturating_sub(1)
            .min(self.pages.len().saturating_sub(1));
        let (anchor_chunk, anchor_local) = match self.pages.get(anchor) {
            Some(page) => (
                page.chunk,
                anchor + 1
                    - self
                        .chunks
                        .get(page.chunk)
                        .map(|c| c.first_page)
                        .unwrap_or(1),
            ),
            None => (0, 0),
        };
        self.apply_options(options);
        for entry in self.entries.iter_mut() {
            entry.stale = entry.layout.is_some();
        }
        *self.page_boxes_cache.borrow_mut() = None;
        self.clear_page_image_cache();
        let old_anchor_pages = self
            .entries
            .get(anchor_chunk)
            .and_then(|entry| entry.layout.as_ref())
            .map(|layout| layout.pages.len())
            .unwrap_or(0);
        self.lay_out_chunk_now(anchor_chunk)?;
        // The chunk that was laid out says what the new settings do to a page
        // count; apply that ratio to the chunks that keep their old pages, so the
        // total the reader is shown is close to the truth until it settles.
        if old_anchor_pages > 0 {
            let new_anchor_pages = self
                .entries
                .get(anchor_chunk)
                .and_then(|entry| entry.layout.as_ref())
                .map(|layout| layout.pages.len())
                .unwrap_or(0);
            if new_anchor_pages > 0 {
                let ratio = new_anchor_pages as f32 / old_anchor_pages as f32;
                if (ratio - 1.0).abs() > 0.02 {
                    for entry in self.entries.iter_mut() {
                        if entry.stale
                            && let Some(layout) = entry.layout.as_mut()
                        {
                            let target =
                                (layout.pages.len() as f32 * ratio).round().max(1.0) as usize;
                            layout.resample(target);
                        }
                    }
                }
            }
        }
        self.rebuild_view();
        let first_page = self
            .chunks
            .get(anchor_chunk)
            .map(|c| c.first_page)
            .unwrap_or(1);
        let page_count = self
            .entries
            .get(anchor_chunk)
            .and_then(|entry| entry.layout.as_ref())
            .map(|layout| layout.pages.len())
            .unwrap_or(1);
        self.current_page = first_page + anchor_local.min(page_count.saturating_sub(1));
        self.layout_done = true;
        trim_memory();
        Ok(true)
    }

    /// Remaps the page indices that move when chunk `chunk` is laid out again and
    /// its page count changes from `old_count` to `new_count`.
    fn rebase_pages(&mut self, chunk: usize, old_count: usize, new_count: usize) {
        let Some(head) = self.chunks.get(chunk).map(|meta| meta.first_page) else {
            return;
        };
        let page = self.current_page;
        if page >= head + old_count {
            let delta = new_count as i64 - old_count as i64;
            self.current_page = (page as i64 + delta).max(1) as usize;
        } else if page >= head {
            let local = page - head;
            self.current_page = head + local.min(new_count.saturating_sub(1));
        }
    }

    /// Lays out `page`'s chunk if it still holds a provisional page list.
    ///
    /// Returns the page index to use: a chunk whose page count changes moves the
    /// pages after it, including the requested page.
    pub fn ensure_page_layout(&mut self, page: usize) -> Result<usize> {
        let Some(index) = page.checked_sub(1) else {
            return Ok(page);
        };
        let Some(snapshot) = self.pages.get(index) else {
            return Ok(page);
        };
        let chunk = snapshot.chunk;
        if !self.entries.get(chunk).is_some_and(|entry| entry.stale) {
            return Ok(page);
        }
        let old_head = self
            .chunks
            .get(chunk)
            .map(|meta| meta.first_page)
            .unwrap_or(1);
        let old_count = self
            .entries
            .get(chunk)
            .and_then(|entry| entry.layout.as_ref())
            .map(|layout| layout.pages.len())
            .unwrap_or(0);
        let local = page.saturating_sub(old_head);
        self.lay_out_chunk_now(chunk)?;
        self.rebuild_view();
        let new_count = self
            .entries
            .get(chunk)
            .and_then(|entry| entry.layout.as_ref())
            .map(|layout| layout.pages.len())
            .unwrap_or(1);
        self.rebase_pages(chunk, old_count, new_count);
        Ok(old_head + local.min(new_count.saturating_sub(1)))
    }

    /// Lays out the chunk an XPointer points into, if it is still provisional.
    ///
    /// Every XPointer names its `DocFragment` (spine item), and a spine item
    /// belongs to exactly one chunk, so the target chunk is known without the
    /// page list.
    pub fn ensure_xpointer_layout(&mut self, xpointer: &str) -> Result<()> {
        let Ok(position) = parse_text_position(xpointer) else {
            return Ok(());
        };
        let Some(chunk) = self.spine_to_chunk.get(&position.spine_index).copied() else {
            return Ok(());
        };
        if self.entries.get(chunk).is_some_and(|entry| entry.stale) {
            let head = self
                .chunks
                .get(chunk)
                .map(|meta| meta.first_page)
                .unwrap_or(1);
            let _ = self.ensure_page_layout(head)?;
        }
        Ok(())
    }

    /// Lays out every chunk, building the page list, TOC page numbers and page
    /// map. Cheap to call more than once.
    pub fn ensure_layout(&mut self) -> Result<()> {
        if self.layout_done {
            return Ok(());
        }
        self.layout_done = true;
        self.entries = (0..self.sources.len())
            .map(|_| ChunkEntry {
                layout: None,
                stale: false,
            })
            .collect();
        for chunk_index in 0..self.sources.len() {
            let (layout, doc, lines, page_starts) = self.layout_chunk(chunk_index)?;
            self.entries[chunk_index] = ChunkEntry {
                layout: Some(layout),
                stale: false,
            };
            // The chunk's DOM and line boxes are only needed to derive page
            // metadata; release them now so the pass stays flat in RSS instead
            // of accumulating one chunk's arenas per iteration.
            drop(doc);
            drop(lines);
            drop(page_starts);
            trim_memory();
        }
        self.rebuild_view();
        trim_memory();
        self.ensure_chunk(0)?;
        Ok(())
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

    pub fn set_background_color(&mut self, background_color: u32) {
        let color = background_color & 0xFF_FFFF;
        if self.background_color != color {
            self.clear_page_image_cache();
        }
        self.background_color = color;
    }

    pub fn page_height(&self) -> u32 {
        self.height
    }

    pub fn page_margin_y(&self) -> i64 {
        self.page_margin.round() as i64
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
        lookup_resource(&self.resources, key)
    }

    pub fn image_data_at(&self, position: PagePosition) -> Result<Option<Vec<u8>>> {
        // Text wins when an image rectangle overlaps a rendered line. A
        // floating/oversized image can otherwise cover text on this page (or a
        // neighbouring page's rectangle can overlap it), and KOReader checks
        // images before asking for the word: long-press then opens the image
        // viewer instead of selecting the text.
        if self
            .page_text_boxes(position.page)
            .ok()
            .is_some_and(|boxes| {
                boxes.iter().any(|line| {
                    !line.text.trim().is_empty()
                        && position.x >= line.x
                        && position.x <= line.x + line.width
                        && position.y >= line.y
                        && position.y <= line.y + line.height
                })
            })
        {
            return Ok(None);
        }
        let Some(index) = position.page.checked_sub(1) else {
            bail!("page numbers are 1-based");
        };
        let snapshot = self.pages.get(index).context("page snapshot is missing")?;
        let chunk_index = snapshot.chunk;
        let page_top = snapshot.local_start.top;

        self.with_active_chunk(chunk_index, |active| {
            let root = active.doc.root_element();
            // `position` is in rendered (device) pixels, like `TextBox`; the
            // document's layout rectangles are CSS pixels. Without dividing by
            // the viewport scale, a HiDPI view compares device coordinates to
            // CSS rectangles: images from the next page appear to cover text on
            // this page, so long-press opens an image instead of selecting.
            let scale = self.viewport_scale.max(0.1);
            Ok(find_image_data_at(
                &active.doc,
                root,
                page_top,
                position.x / scale,
                position.y / scale,
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

    /// The EPUB spine index behind a spine slot. Equal unless the spine item
    /// was split into parts; used where a real spine index is required (CFI).
    pub fn real_spine_index(&self, spine_slot: u32) -> u32 {
        self.spine_real_index
            .get(&spine_slot)
            .copied()
            .unwrap_or(spine_slot)
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

    pub fn render_page_with_stats(
        &mut self,
        page: usize,
    ) -> Result<(Arc<PageBitmap>, usize, usize)> {
        self.render_page_with_stats_inner(page)
    }

    fn render_page_with_stats_inner(
        &mut self,
        page: usize,
    ) -> Result<(Arc<PageBitmap>, usize, usize)> {
        // A chunk that has not been laid out since the last settings change only
        // has provisional pages; lay it out before trusting its geometry.
        let page = self.ensure_page_layout(page)?;
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

        if let Some(cached) = self.cached_page_image(page) {
            self.ensure_chunk(chunk_index)?;
            return Ok((
                cached.bitmap.clone(),
                cached.image_count,
                cached.image_surface,
            ));
        }

        let (bitmap, image_count, image_surface) = {
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
            let local_top = clip_top * viewport_scale;
            let local_bottom = clip_bottom * viewport_scale;
            let (image_arc, paint_count, paint_surface) = paint_page(
                &scene,
                self.width,
                self.height,
                0.0,
                local_top,
                local_bottom,
                self.background_color,
                self.font_gamma,
            );
            (image_arc, paint_count, paint_surface)
        };
        let image = PageImage {
            bitmap: bitmap.clone(),
            image_count,
            image_surface,
        };
        let out = (bitmap.clone(), image_count, image_surface);
        self.cache_page_image(page, image);
        Ok(out)
    }

    pub fn render_page_color_with_stats(
        &mut self,
        page: usize,
    ) -> Result<(Arc<ColorPageBitmap>, usize, usize)> {
        let page = self.ensure_page_layout(page)?;
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

        let local_top = clip_top * viewport_scale;
        let local_bottom = clip_bottom * viewport_scale;
        let (bitmap, image_count, image_surface) = paint_page_rgba(
            &scene,
            self.width,
            self.height,
            0.0,
            local_top,
            local_bottom,
            self.background_color,
            self.font_gamma,
        );
        Ok((bitmap, image_count, image_surface))
    }

    pub fn render_current_page_color_with_stats(
        &mut self,
    ) -> Result<(Arc<ColorPageBitmap>, usize, usize)> {
        self.render_page_color_with_stats(self.current_page)
    }

    pub fn set_font_gamma(&mut self, gamma: f32) {
        let gamma = if gamma.is_finite() {
            gamma.clamp(0.1, 15.0)
        } else {
            1.0
        };
        if gamma != self.font_gamma {
            self.font_gamma = gamma;
            self.clear_page_image_cache();
        }
    }

    pub fn render_page(&mut self, page: usize) -> Result<PageBitmap> {
        let (bitmap, _, _) = self.render_page_with_stats(page)?;
        Ok(bitmap.as_ref().clone())
    }

    pub fn render_current_page(&mut self) -> Result<PageBitmap> {
        self.render_page(self.current_page)
    }

    pub fn render_current_page_with_stats(&mut self) -> Result<(Arc<PageBitmap>, usize, usize)> {
        self.render_page_with_stats(self.current_page)
    }

    fn cached_page_image(&self, page: usize) -> Option<PageImage> {
        let image = self.page_image_cache.borrow().get(&page).cloned()?;
        self.touch_page_image_cache(page);
        Some(image)
    }

    fn touch_page_image_cache(&self, page: usize) {
        let mut order = self.page_cache_order.borrow_mut();
        order.retain(|cached| *cached != page);
        order.push(page);
    }

    fn cache_page_image(&self, page: usize, image: PageImage) {
        self.touch_page_image_cache(page);
        let mut cache = self.page_image_cache.borrow_mut();
        loop {
            let evicted = {
                let mut order = self.page_cache_order.borrow_mut();
                if order.len() <= PAGE_IMAGE_CACHE_CAPACITY {
                    break;
                }
                order.remove(0)
            };
            cache.remove(&evicted);
        }
        cache.insert(page, image);
    }

    fn clear_page_image_cache(&self) {
        self.page_image_cache.borrow_mut().clear();
        self.page_cache_order.borrow_mut().clear();
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
        let stored = self
            .sources
            .get(index)
            .context("document chunk is missing")?;
        let html = stored.clone();
        // Free the current chunk before building the next one: both being
        // resident at once doubles the layout peak, which matters on a device
        // where a single layout is already tens of megabytes. The cached page
        // bitmaps belong to the old chunk too, so drop them as well.
        drop(self.active.borrow_mut().take());
        self.clear_page_image_cache();
        *self.page_boxes_cache.borrow_mut() = None;
        trim_memory();
        let doc = create_document(
            &html,
            &self.base_url,
            self.net.clone(),
            self.font_context.clone(),
            &self.layout_options(),
        );
        let lines = collect_lines(doc.root_element());
        let forced_breaks = page_breaks::collect_forced_breaks(doc.root_element());
        let boxes = collect_content_boxes(&doc, doc.root_element());
        let (_page_height_css_u32, page_height_css, page_margin_css) = css_page_geometry(
            self.width,
            self.height,
            self.viewport_scale,
            self.page_margin,
        );
        let page_starts = paginate(
            &lines,
            &boxes,
            &forced_breaks,
            page_height_css,
            page_margin_css,
            page_margin_css,
        );
        *self.active.borrow_mut() = Some(ActiveChunk {
            index,
            rendered_text_nodes: rendered_text_nodes(&doc, &lines),
            doc,
            lines,
            page_starts,
        });
        trim_memory();
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

    fn parley_word_byte_range(
        &self,
        page: usize,
        line_index: usize,
        line_byte_offset: usize,
    ) -> Result<Option<(usize, usize)>> {
        let Some(snapshot) = self.pages.get(page.saturating_sub(1)) else {
            return Ok(None);
        };
        let chunk_index = snapshot.chunk;
        let line_base = self.chunks[chunk_index].line_base;
        let Some(local_line) = line_index.checked_sub(line_base) else {
            return Ok(None);
        };

        self.with_active_chunk(chunk_index, |active| {
            let Some(line_box) = active.lines.get(local_line) else {
                return Ok(None);
            };
            let line_start = line_box.text_start;
            let line_end = line_box.text_end;
            if line_start >= line_end {
                return Ok(None);
            }
            let absolute = line_start.saturating_add(line_byte_offset);
            if absolute < line_start || absolute >= line_end {
                return Ok(None);
            }

            let root = active.doc.root_element();
            let node = root.with(line_box.node_id);
            let Some(inline_layout) = node
                .element_data()
                .and_then(|element| element.inline_layout_data.as_ref())
            else {
                return Ok(None);
            };
            let Some(cluster) = Cluster::from_byte_index(&inline_layout.layout, absolute) else {
                return Ok(None);
            };

            let mut word_start_cluster = cluster;
            if !word_start_cluster.is_word_boundary() {
                let Some(prev) = word_start_cluster.previous_logical_word() else {
                    return Ok(None);
                };
                word_start_cluster = prev;
            }
            let start = word_start_cluster.text_range().start;
            let end = word_start_cluster
                .next_logical_word()
                .map(|boundary| boundary.text_range().start)
                .unwrap_or(line_end);

            let rel_start = start
                .saturating_sub(line_start)
                .min(line_end.saturating_sub(line_start));
            let rel_end = end
                .saturating_sub(line_start)
                .min(line_end.saturating_sub(line_start));
            if rel_start >= rel_end {
                return Ok(None);
            }
            Ok(Some((rel_start, rel_end)))
        })
    }

    pub fn word_at(&self, position: PagePosition) -> Result<Option<WordHit>> {
        let Some(hit) = self.hit_test(position)? else {
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

        let fallback = word_byte_range(&hit.text, hit.byte_offset);
        let parley_range = self
            .parley_word_byte_range(hit.page, hit.line_index, hit.byte_offset)
            .unwrap_or(None);
        let use_parley = match (fallback, parley_range) {
            (Some((fallback_start, fallback_end)), Some((parley_start, parley_end))) => {
                parley_start >= fallback_start
                    && parley_end <= fallback_end
                    && parley_start < parley_end
                    && (parley_start, parley_end) != (fallback_start, fallback_end)
            }
            (None, Some((parley_start, parley_end))) => parley_start < parley_end,
            (_, None) => false,
        };
        let Some((start_byte, end_byte)) = (if use_parley { parley_range } else { fallback })
        else {
            return Ok(None);
        };

        let (start_position, end_position) = self.with_active_chunk(chunk_index, |active| {
            let line = active
                .lines
                .get(local_line)
                .context("line snapshot is missing")?;
            let root = active.doc.root_element();
            let start = xpointer::position_for_line_offset(root, line, start_byte)?;
            // The end byte is the boundary after the word. Map it to the
            // position just after the word's own last character, not to the
            // first character of the next text node: a word ending at a link
            // or span boundary otherwise produces a range split across blocks.
            let block = root.with(line.node_id);
            let mut end = xpointer::position_after_layout_offset(
                root,
                block,
                line.text_start.saturating_add(end_byte),
            )
            .unwrap_or(xpointer::position_for_line_offset(root, line, end_byte)?);
            if end == start {
                // The line -> source mapping collapsed (it matches text
                // prefixes, so repeated sentences can resolve to the same
                // position). A zero-length range highlights nothing, so widen
                // it by the word's length inside the node it resolved to.
                let word_chars = hit.text[start_byte..end_byte].chars().count().max(1) as u32;
                let node_chars = xpointer::resolve_text_position(root, &start)
                    .ok()
                    .and_then(|id| {
                        root.with(id)
                            .text_data()
                            .map(|data| data.content.chars().count() as u32)
                    })
                    .unwrap_or(u32::MAX);
                let mut widened = start.clone();
                widened.codepoint_offset = start
                    .codepoint_offset
                    .saturating_add(word_chars)
                    .min(node_chars);
                if widened != start {
                    end = widened;
                }
            }
            Ok((start, end))
        })?;

        let text = hit.text[start_byte..end_byte].to_string();
        let Some(rect) = self.word_box(WordSpan {
            page: hit.page,
            line_index: hit.line_index,
            line: &hit.rect,
            start_byte,
            end_byte,
            text: &text,
        }) else {
            return Ok(None);
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

    /// Box of a byte range on one line, from parley's selection geometry.
    /// Returns `None` when the line has no parley layout; callers omit the box
    /// rather than estimating one.
    fn word_box(&self, span: WordSpan<'_>) -> Option<TextBox> {
        let (x0, x1) = self
            .parley_segment_x_bounds(span.page, span.line_index, span.start_byte, span.end_byte)
            .ok()
            .flatten()?;
        Some(TextBox {
            text: span.text.to_string(),
            page: span.page,
            x: x0,
            y: span.line.y,
            width: (x1 - x0).max(1.0),
            height: span.line.height,
        })
    }

    pub fn nearest_word_at(
        &self,
        position: PagePosition,
        pages: &[usize],
        direction: u32,
    ) -> Result<Option<WordHit>> {
        let mut candidates = Vec::new();
        let mut pending = Vec::new();
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
                    let block = root.with(line.node_id);
                    for (start_byte, end_byte) in ranges {
                        let Ok(start_position) =
                            xpointer::position_for_line_offset(root, line, start_byte)
                        else {
                            continue;
                        };
                        let end_position = xpointer::position_after_layout_offset(
                            root,
                            block,
                            line.text_start.saturating_add(end_byte),
                        )
                        .or_else(|| xpointer::position_for_line_offset(root, line, end_byte).ok());
                        pending.push(PendingWord {
                            page,
                            line_index: global_line,
                            line: rect.clone(),
                            start_byte,
                            end_byte,
                            start: start_position,
                            end: end_position,
                        });
                    }
                    Ok(())
                })?;
            }
        }

        for pending in pending {
            let text = pending.line.text[pending.start_byte..pending.end_byte].to_string();
            let Some(text_box) = self.word_box(WordSpan {
                page: pending.page,
                line_index: pending.line_index,
                line: &pending.line,
                start_byte: pending.start_byte,
                end_byte: pending.end_byte,
                text: &text,
            }) else {
                continue;
            };
            candidates.push(WordHit {
                text,
                xpointer_range: XPointerRange {
                    start: pending.start,
                    end: pending.end,
                    id: None,
                },
                rects: vec![text_box],
            });
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
        for (page, line_index, mut rect, slice_start, slice_end) in
            self.clipped_lines_between_xpointers(start, end)?
        {
            let text_slice = rect.text[slice_start..slice_end].to_string();
            let Some((x0, x1)) = self
                .parley_segment_x_bounds(page, line_index, slice_start, slice_end)
                .unwrap_or(None)
            else {
                continue;
            };
            rect.text = text_slice;
            rect.x = x0;
            rect.width = (x1 - x0).max(1.0);
            boxes.push(rect);
        }
        Ok(boxes)
    }

    fn parley_segment_x_bounds(
        &self,
        page: usize,
        line_index: usize,
        slice_start: usize,
        slice_end: usize,
    ) -> Result<Option<(f32, f32)>> {
        let Some(snapshot) = self.pages.get(page.saturating_sub(1)) else {
            return Ok(None);
        };
        let chunk_index = snapshot.chunk;
        let line_base = self.chunks[chunk_index].line_base;
        let Some(local_line) = line_index.checked_sub(line_base) else {
            return Ok(None);
        };

        self.with_active_chunk(chunk_index, |active| {
            let Some(line_box) = active.lines.get(local_line) else {
                return Ok(None);
            };
            let root = active.doc.root_element();
            let node = root.with(line_box.node_id);
            let Some(inline_layout) = node
                .element_data()
                .and_then(|element| element.inline_layout_data.as_ref())
            else {
                return Ok(None);
            };
            let layout = &inline_layout.layout;

            let Some((target_index, target_line)) = (0..layout.len())
                .map(|line_number| (line_number, layout.get(line_number)))
                .filter_map(|(line_number, line)| line.map(|value| (line_number, value)))
                .find(|(_, line)| {
                    let text_range = line.text_range();
                    text_range.start <= line_box.text_start && text_range.end >= line_box.text_end
                })
            else {
                return Ok(None);
            };
            let line_metrics = target_line.metrics();
            // `bbox` is in the inline layout space, and `line_box.left` is
            // already shifted by the line's inline start. Subtract that start
            // (offset + inline_min_coord) before adding the absolute left.
            // Without the inline_min_coord part, a line next to a float (a
            // drop cap) is shifted right by the float width.
            let line_start = line_metrics.offset + line_metrics.inline_min_coord;

            let start_byte = line_box.text_start.saturating_add(slice_start);
            let end_byte = line_box.text_start.saturating_add(slice_end);
            if start_byte >= end_byte {
                return Ok(None);
            }

            let start_cursor = Cursor::from_byte_index(layout, start_byte, Affinity::Upstream);
            let end_cursor = Cursor::from_byte_index(layout, end_byte, Affinity::Downstream);
            let selection = ParleySelection::new(start_cursor, end_cursor);

            let mut left = f32::INFINITY;
            let mut right = f32::NEG_INFINITY;
            for (bbox, line_idx) in selection.geometry(layout) {
                if line_idx != target_index {
                    continue;
                }
                let x0 = line_box.left + ((bbox.x0 as f32) - line_start);
                let x1 = line_box.left + ((bbox.x1 as f32) - line_start);
                left = left.min(x0);
                left = left.min(x1);
                right = right.max(x0);
                right = right.max(x1);
            }
            if left.is_finite() && right.is_finite() && right > left {
                let scale = self.viewport_scale;
                return Ok(Some((left * scale, right * scale)));
            }
            Ok(None)
        })
    }

    pub fn point_from_xpointer(&self, xpointer: &str) -> Result<Option<(f32, f32)>> {
        // Walk forward to the next visible character when the position itself
        // is not inside a line (an XPointer at a block boundary, or one whose
        // node is not laid out). This used to recurse; a position that never
        // matches a line made the recursion run through the whole book and
        // overflow the stack (a SIGSEGV that killed KOReader while painting a
        // page with a highlight). A few dozen steps are enough for the cases
        // this exists for; anything further is not visible text.
        let mut current = xpointer.to_string();
        for _ in 0..64 {
            let target = parse_text_position(&current)?;
            let page = self.page_from_xpointer(&current)?;
            for (line_index, rect) in self.page_text_boxes_with_lines(page)? {
                if let Some(chars) = self.line_relative_chars(page, line_index, &target)? {
                    let byte = byte_offset_for_char(&rect.text, chars);
                    let next = rect.text[byte..]
                        .chars()
                        .next()
                        .map(|ch| byte + ch.len_utf8())
                        .unwrap_or(byte + 1);
                    if let Ok(Some((x, _))) =
                        self.parley_segment_x_bounds(page, line_index, byte, next)
                    {
                        return Ok(Some((x, rect.y)));
                    }
                    // The line has no parley geometry for this character; try
                    // the next visible one instead of estimating.
                    break;
                }
            }
            let Some(next) = self.next_visible_char(&current)? else {
                return Ok(None);
            };
            if next == current {
                return Ok(None);
            }
            current = next;
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
        for (_, _, rect, slice_start, slice_end) in
            self.clipped_lines_between_xpointers(start, end)?
        {
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

    fn clipped_lines_between_xpointers(&self, start: &str, end: &str) -> Result<Vec<ClippedLine>> {
        let mut start_position = parse_text_position(start)?;
        let mut end_position = parse_text_position(end)?;
        let mut start_page = self.page_from_xpointer(start)?;
        let mut end_page = self.page_from_xpointer(end)?;
        let mut reversed = start_page > end_page;
        if !reversed && start_page == end_page {
            // Same page: compare the two positions in document order. Their
            // XPointer paths cannot be compared directly (a child span sorts
            // after its parent's tag even when its text comes first).
            reversed =
                self.compare_positions_on_page(start_page, &start_position, &end_position)?
                    == Some(std::cmp::Ordering::Greater);
        }
        if reversed {
            std::mem::swap(&mut start_page, &mut end_page);
            std::mem::swap(&mut start_position, &mut end_position);
        }

        let mut lines = Vec::new();
        for page in start_page..=end_page {
            let Some(snapshot) = self.pages.get(page.saturating_sub(1)) else {
                continue;
            };
            let chunk_index = snapshot.chunk;
            let line_base = self.chunks[chunk_index].line_base;
            let page_boxes = self.page_text_boxes_with_lines(page)?;
            let clipped = self.with_active_chunk(chunk_index, |active| {
                let root = active.doc.root_element();
                let mut out = Vec::new();
                for (global_line, rect) in page_boxes.iter() {
                    let Some(local_line) = global_line.checked_sub(line_base) else {
                        continue;
                    };
                    let Some(line) = active.lines.get(local_line) else {
                        continue;
                    };
                    let block = root.with(line.node_id);
                    let Some(layout) = block
                        .element_data()
                        .and_then(|element| element.inline_layout_data.as_ref())
                    else {
                        continue;
                    };
                    let text = layout.text.as_str();
                    let start_byte = line.text_start.min(text.len());
                    let end_byte = line.text_end.clamp(start_byte, text.len());
                    if start_byte >= end_byte {
                        continue;
                    }
                    // Where the two ends of the range fall: inside this line's
                    // block (exact layout offset) or in another block (compare
                    // source nodes in document order). No line-end position is
                    // needed, so a neighbouring inline root cannot make the
                    // line look longer than it is.
                    let start_key = xpointer::block_and_layout_offset(root, &start_position);
                    let end_key = xpointer::block_and_layout_offset(root, &end_position);
                    let line_first = xpointer::position_for_line_offset(root, line, 0)
                        .ok()
                        .and_then(|position| {
                            xpointer::resolve_text_position(root, &position).ok()
                        });
                    let from_byte = match start_key {
                        Some((id, offset)) if id == block.id => {
                            if offset >= end_byte {
                                continue;
                            }
                            offset.max(start_byte)
                        }
                        Some(_) => {
                            let start_text =
                                xpointer::resolve_text_position(root, &start_position).ok();
                            if matches!((start_text, line_first), (Some(start), Some(first)) if start > first)
                            {
                                continue;
                            }
                            start_byte
                        }
                        None => start_byte,
                    };
                    let to_byte = match end_key {
                        Some((id, offset)) if id == block.id => {
                            if offset <= start_byte {
                                continue;
                            }
                            offset.min(end_byte)
                        }
                        Some(_) => {
                            let end_text =
                                xpointer::resolve_text_position(root, &end_position).ok();
                            if matches!((end_text, line_first), (Some(end), Some(first)) if end < first)
                            {
                                continue;
                            }
                            end_byte
                        }
                        None => end_byte,
                    };
                    let line_start_chars = text[..start_byte].chars().count();
                    let from_chars = text[..from_byte].chars().count() - line_start_chars;
                    let to_chars = text[..to_byte].chars().count() - line_start_chars;
                    if from_chars >= to_chars {
                        continue;
                    }
                    let slice_start = byte_offset_for_char(&rect.text, from_chars);
                    let slice_end = byte_offset_for_char(&rect.text, to_chars);
                    if slice_start < slice_end {
                        out.push((page, *global_line, rect.clone(), slice_start, slice_end));
                    }
                }
                Ok(out)
            })?;
            lines.extend(clipped);
        }
        Ok(lines)
    }

    /// Where a position falls inside a line, as a character index from the
    /// line start. Uses the line's inline layout text, so a line made of
    /// several text nodes (a `<span>` inside a heading) is handled correctly;
    /// returns `None` when the position is not on the line or the layout text
    /// no longer matches the DOM.
    fn line_relative_chars(
        &self,
        page: usize,
        line_index: usize,
        target: &TextPosition,
    ) -> Result<Option<usize>> {
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
            let inline_root = root.with(line.node_id);
            let Some(layout) = inline_root
                .element_data()
                .and_then(|element| element.inline_layout_data.as_ref())
            else {
                return Ok(None);
            };
            let start = line.text_start.min(layout.text.len());
            let end = line.text_end.min(layout.text.len());
            if start >= end {
                return Ok(None);
            }
            let line_chars = layout.text[start..end].chars().count();
            let Some(byte) = xpointer::layout_offset_in_block(root, inline_root, target) else {
                return Ok(None);
            };
            if byte < start || byte > end {
                return Ok(None);
            }
            let chars = layout.text[start..byte].chars().count();
            Ok(Some(chars.min(line_chars.saturating_sub(1))))
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
            let meta = &self.pages[page - 1];
            let chunk_index = meta.chunk;
            let line_base = self.chunks[chunk_index].line_base;
            let snapshot = self.page_snapshot_boxes(page)?;
            let mut page_text = String::new();
            let mut spans = Vec::with_capacity(snapshot.len());
            for line in &snapshot {
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
            // `search` appends hits page by page in reading order; reverse is
            // all that is needed. Sorting by `TextPosition` was not document
            // order (it compares path tags).
            hits.reverse();
        } else if origin == -1 {
            hits.retain(|hit| hit.page <= current_page);
        } else {
            hits.retain(|hit| hit.page >= current_page);
        }
    }

    pub fn go_to_page(&mut self, page: usize) -> Result<usize> {
        if page < 1 || page > self.page_count() {
            bail!("page {page} is outside 1..={}", self.page_count());
        }
        // Landing in a provisional chunk lays that chunk out now, so the page
        // number we report is a real one.
        let page = self.ensure_page_layout(page)?;
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
        self.current_page = self.ensure_page_layout(1).unwrap_or(1);
        self.current_page
    }

    pub fn go_to_last_page(&mut self) -> usize {
        let page = self.page_count();
        self.current_page = self.ensure_page_layout(page).unwrap_or(page);
        self.current_page
    }

    pub fn go_to_next_page(&mut self) -> usize {
        let page = (self.current_page + 1).min(self.page_count());
        self.current_page = self.ensure_page_layout(page).unwrap_or(page);
        self.current_page
    }

    pub fn go_to_previous_page(&mut self) -> usize {
        let page = self.current_page.saturating_sub(1).max(1);
        self.current_page = self.ensure_page_layout(page).unwrap_or(page);
        self.current_page
    }

    pub fn page_from_href(&self, href: &str) -> Result<usize> {
        page_for_href(&self.href_pages, href)
    }

    pub fn go_to_href(&mut self, href: &str) -> Result<usize> {
        let page = self.page_from_href(href)?;
        let page = self.ensure_page_layout(page)?;
        self.current_page = page;
        Ok(page)
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
        let line_page = self
            .page_from_text_position_by_line_ranges(&position, chunk_index)
            .unwrap_or(None);
        if let Some(page) = line_page {
            return Ok(page);
        }

        let local_page = self.with_active_chunk(chunk_index, |active| {
            xpointer::page_from_xpointer(&active.doc, &active.page_starts, trimmed)
        })?;
        Ok(self.chunks[chunk_index].first_page + local_page - 1)
    }

    fn page_from_text_position_by_line_ranges(
        &self,
        position: &TextPosition,
        chunk_index: usize,
    ) -> Result<Option<usize>> {
        let Some(entry) = self.entries.get(chunk_index) else {
            return Ok(None);
        };
        let Some(layout) = entry.layout.as_ref() else {
            return Ok(None);
        };
        let head = self
            .chunks
            .get(chunk_index)
            .map(|meta| meta.first_page)
            .unwrap_or(1);
        // The last line whose start is <= the position holds it (a line's text
        // runs to the next line's start). "Is <=" has to be document order, so
        // the key is the resolved text node plus the offset in it — the same key
        // the table was built with. `TextPosition` ordering is not document
        // order (it compares path segments by tag first).
        let lines = &layout.lines;
        self.with_active_chunk(chunk_index, |active| {
            let root = active.doc.root_element();
            let Some(key) = xpointer::block_and_layout_offset(root, position) else {
                return Ok(None);
            };
            let first_greater =
                lines.partition_point(|entry| (entry.block, entry.offset as usize) < key);
            let index = if lines
                .get(first_greater)
                .is_some_and(|entry| (entry.block, entry.offset as usize) == key)
            {
                Some(first_greater)
            } else {
                first_greater.checked_sub(1)
            };
            Ok(index.map(|index| head + lines[index].page))
        })
    }

    pub fn go_to_xpointer(&mut self, xpointer: &str) -> Result<usize> {
        // Lay out the chunk the target lives in first: its provisional pages
        // would otherwise put us on the wrong page for the new settings.
        self.ensure_xpointer_layout(xpointer)?;
        let page = self.page_from_xpointer(xpointer)?;
        self.current_page = page;
        Ok(page)
    }

    fn compare_positions_in_chunk(
        &self,
        chunk_index: usize,
        left: &TextPosition,
        right: &TextPosition,
    ) -> Result<Option<std::cmp::Ordering>> {
        self.with_active_chunk(chunk_index, |active| {
            let root = active.doc.root_element();
            Ok(xpointer::compare_text_positions(root, left, right))
        })
    }

    fn compare_positions_on_page(
        &self,
        page: usize,
        left: &TextPosition,
        right: &TextPosition,
    ) -> Result<Option<std::cmp::Ordering>> {
        let Some(snapshot) = self.pages.get(page.saturating_sub(1)) else {
            return Ok(None);
        };
        self.compare_positions_in_chunk(snapshot.chunk, left, right)
    }

    pub fn compare_xpointers(&self, left: &str, right: &str) -> Result<i32> {
        let left = parse_text_position(left)?;
        let right = parse_text_position(right)?;
        let left_chunk = self.spine_to_chunk.get(&left.spine_index).copied();
        let right_chunk = self.spine_to_chunk.get(&right.spine_index).copied();
        // Return 1 when `right` is after `left`, like cre.cpp's
        // `nodep2.compare(nodep1)`. Resolve both positions to text nodes so the
        // comparison follows document order, not XPointer path strings.
        let right_vs_left = match (left_chunk, right_chunk) {
            (Some(left_chunk), Some(right_chunk)) if left_chunk == right_chunk => {
                self.compare_positions_in_chunk(left_chunk, &right, &left)?
            }
            (Some(left_chunk), Some(right_chunk)) => Some(right_chunk.cmp(&left_chunk)),
            _ => None,
        };
        let ordering = right_vs_left.unwrap_or_else(|| right.cmp(&left));
        Ok(match ordering {
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
            let flat = flat_text_characters(root, &active.rendered_text_nodes);
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
            if chars.is_empty() {
                return Ok(None);
            }
            let start_idx = (start_position.codepoint_offset as usize).min(chars.len());
            let end_idx = (end_position.codepoint_offset as usize).min(chars.len());
            if start_idx > end_idx {
                return Ok(None);
            }

            let mut is_sentence_segment = true;
            let mut new_start = start_idx;
            let mut grabbed_opening = false;
            let mut index = start_idx;
            while index > 0 {
                index -= 1;
                let ch = chars[index];
                if is_space_char(ch) {
                    continue;
                }
                if is_punct_opening_char(ch) {
                    new_start = index;
                    grabbed_opening = true;
                    continue;
                }
                if is_punctuation_char(ch) {
                    break;
                }
                if !grabbed_opening {
                    is_sentence_segment = false;
                }
                break;
            }
            if !is_sentence_segment {
                return Ok(None);
            }

            let mut new_end = end_idx;
            let mut grabbed_closing = false;
            if end_idx < chars.len() {
                index = end_idx;
                while index < chars.len() {
                    let ch = chars[index];
                    if is_space_char(ch) {
                        index += 1;
                        continue;
                    }
                    if is_punctuation_char(ch) && !is_punct_opening_char(ch) {
                        new_end = index + 1;
                        grabbed_closing = true;
                        index += 1;
                        continue;
                    }
                    if !grabbed_closing {
                        is_sentence_segment = false;
                    }
                    break;
                }
            }
            if !is_sentence_segment {
                return Ok(None);
            }

            let out_text = chars[new_start..new_end].iter().collect::<String>();
            if out_text.trim().is_empty() {
                return Ok(None);
            }
            start_position.codepoint_offset = new_start as u32;
            end_position.codepoint_offset = new_end as u32;
            Ok(Some((
                out_text,
                start_position.to_xpointer(),
                end_position.to_xpointer(),
            )))
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

    pub fn xpointer_from_position(&self, position: &TextPosition) -> Result<String> {
        Ok(position.to_xpointer())
    }

    fn page_meta(&self, page: usize) -> Result<&PageMeta> {
        let index = page.checked_sub(1).context("page numbers are 1-based")?;
        self.pages
            .get(index)
            .with_context(|| format!("page {page} is outside 1..={}", self.page_count()))
    }

    fn page_snapshot_boxes(&self, page: usize) -> Result<Vec<SnapshotLine>> {
        if let Some((cached_page, cached)) = self.page_boxes_cache.borrow().as_ref()
            && *cached_page == page
        {
            return Ok(cached.clone());
        }
        let meta = self.page_meta(page)?;
        let (chunk_index, local_start, clip_bottom) =
            (meta.chunk, meta.local_start, meta.local_clip_bottom);
        let line_base = self.chunks[chunk_index].line_base;
        let boxes = self.with_active_chunk(chunk_index, |active| {
            let root = active.doc.root_element();
            Ok(snapshot_page_boxes(
                root,
                &active.lines,
                local_start,
                clip_bottom,
                line_base,
            ))
        })?;
        *self.page_boxes_cache.borrow_mut() = Some((page, boxes.clone()));
        Ok(boxes)
    }

    fn page_text_boxes_with_lines(&self, page: usize) -> Result<Vec<(usize, TextBox)>> {
        let Some(index) = page.checked_sub(1) else {
            bail!("page numbers are 1-based");
        };
        if self.pages.get(index).is_none() {
            bail!("page {page} is outside 1..={}", self.page_count());
        }

        let snapshot = self.page_snapshot_boxes(page)?;
        let scale = self.viewport_scale;
        Ok(snapshot
            .iter()
            .map(|line| {
                (
                    line.global_line,
                    TextBox {
                        text: line.text.clone(),
                        page,
                        x: line.x * scale,
                        y: line.y * scale,
                        width: line.width * scale,
                        height: line.height * scale,
                    },
                )
            })
            .collect())
    }
}

#[allow(clippy::manual_range_contains)]
fn is_space_char(ch: char) -> bool {
    ch.is_whitespace()
}

fn is_punct_opening_char(ch: char) -> bool {
    if ch.is_ascii_punctuation() {
        matches!(ch, '\x28' | '\x5B' | '\x7B')
    } else {
        matches!(
            ch,
            '\u{FF08}'
                | '\u{FF3B}'
                | '\u{FF5B}'
                | '\u{3008}'
                | '\u{300A}'
                | '\u{300C}'
                | '\u{300E}'
                | '\u{3014}'
                | '\u{3018}'
                | '\u{301D}'
                | '\u{301E}'
                | '\u{2018}'
                | '\u{201B}'
                | '\u{201C}'
                | '\u{201E}'
        )
    }
}

#[allow(clippy::manual_range_contains)]
fn is_punctuation_char(ch: char) -> bool {
    if ch.is_ascii_punctuation() {
        return true;
    }
    let code = ch as u32;
    (code >= 0x3000 && code <= 0x303F)
        || (code >= 0x2000 && code <= 0x206F)
        || (code >= 0x2E00 && code <= 0x2E7F)
        || (code >= 0xFF01 && code <= 0xFF0B)
        || (code >= 0xFF1A && code <= 0xFF20)
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
    // Palm Database files are identified by their header, not their extension:
    // `.azw` holds either a MOBI 6 or a KF8 payload, and DFT files are often
    // renamed. `sniff_pdb_format` only reads the header.
    if let Some(format) = sniff_pdb_format(path) {
        return format;
    }
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "fb2" => DocumentFormat::Fb2,
        "txt" => DocumentFormat::Txt,
        "html" | "htm" | "xhtml" | "xml" => DocumentFormat::Html,
        "mobi" | "prc" | "azw" | "pdb" => DocumentFormat::Mobi,
        "azw3" => DocumentFormat::Azw3,
        _ => DocumentFormat::Epub,
    }
}

/// Reads just enough of a Palm Database file to tell MOBI 6 from KF8.
///
/// Both are `BOOKMOBI` containers; the MOBI header's version (8) plus a skeleton
/// table (no `0xFFFFFFFF`) means the payload is KF8/AZW3.
fn sniff_pdb_format(path: &Path) -> Option<DocumentFormat> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0u8; 82];
    file.read_exact(&mut header).ok()?;
    if &header[60..68] != b"BOOKMOBI" && &header[60..68] != b"TEXTREAd" {
        return None;
    }
    if &header[60..68] == b"TEXTREAd" {
        return Some(DocumentFormat::Mobi);
    }
    let record0 = u32::from_be_bytes([header[78], header[79], header[80], header[81]]) as u64;
    file.seek(SeekFrom::Start(record0)).ok()?;
    let mut mobi = [0u8; 0x110];
    let read = file.read(&mut mobi).ok()?;
    let mobi = &mobi[..read];
    if mobi.len() < 0x104 {
        return Some(DocumentFormat::Mobi);
    }
    let version = u32::from_be_bytes([mobi[36], mobi[37], mobi[38], mobi[39]]);
    let skeleton = u32::from_be_bytes([mobi[0xFC], mobi[0xFD], mobi[0xFE], mobi[0xFF]]);
    if version == 8 && skeleton != 0xFFFF_FFFF {
        return Some(DocumentFormat::Azw3);
    }
    Some(DocumentFormat::Mobi)
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

/// Text nodes that took part in a block's inline layout. Text outside every
/// layout (a `display: none` subtree) is absent, so navigation and XPointer
/// lookups stay on text that is actually rendered.
fn rendered_text_nodes(doc: &HtmlDocument, lines: &[LineBox]) -> std::collections::HashSet<NodeId> {
    let root = doc.root_element();
    let mut seen = std::collections::HashSet::new();
    let mut rendered = std::collections::HashSet::new();
    for line in lines {
        if !seen.insert(line.node_id) {
            continue;
        }
        let block = root.with(line.node_id);
        let Some(layout) = block
            .element_data()
            .and_then(|element| element.inline_layout_data.as_ref())
        else {
            continue;
        };
        if let Some(map) = xpointer::LayoutTextMap::new(block, layout) {
            rendered.extend(map.source_nodes());
        }
    }
    rendered
}

fn collect_lines_inner(node: &Node, out: &mut Vec<LineBox>) {
    if let Some(element) = node.element_data()
        && let Some(inline_layout) = element.inline_layout_data.as_ref()
    {
        let root_layout = node.unrounded_layout();
        let inset = root_layout.padding + root_layout.border;
        let origin_y = inset.top + inline_layout.block_offset;
        let has_top_decoration = root_layout.border.top + root_layout.padding.top > 0.0;
        let root_top = node.absolute_position(0.0, 0.0).y;
        for (line_index, line) in inline_layout.layout.lines().enumerate() {
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
            let visual_top = if line_index == 0 && has_top_decoration {
                top.min(root_top)
            } else {
                top
            };
            let text_range = line.text_range();
            out.push(LineBox {
                node_id: node.id,
                text_start: text_range.start,
                text_end: text_range.end,
                source_element_id,
                left,
                right,
                top,
                visual_top,
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
    boxes: &[ContentBox],
    forced_breaks: &[f32],
    page_height: f32,
    margin_top: f32,
    margin_bottom: f32,
) -> Vec<PageStart> {
    const BREAK_EPSILON: f32 = 0.5;

    // The first page starts at the top margin, but never below the first
    // line: documents whose content begins above the configured top margin
    // (e.g. a bare HTML file without the generator's body padding) must not
    // lose their opening line.
    let first_top = lines
        .first()
        .map(|line| line.visual_top)
        .into_iter()
        .chain(boxes.first().map(|box_| box_.top))
        .fold(f32::INFINITY, f32::min);
    let first_clip_top = if first_top.is_finite() {
        first_top.min(margin_top)
    } else {
        margin_top
    };
    let page_content_height = (page_height - margin_top - margin_bottom).max(1.0);
    let mut starts = vec![PageStart {
        top: 0.0,
        clip_top: first_clip_top,
    }];
    let mut forced_index = 0usize;
    let mut pending_break = false;
    // Visible content on the current page: a line with text or an image box.
    // A chunk starts on a fresh page, so a break before its very first content
    // would only add an empty page.
    let mut page_has_content = false;

    // Walk lines and image boxes together, in document order.
    let mut items: Vec<(f32, Option<usize>, Option<usize>)> =
        Vec::with_capacity(lines.len() + boxes.len());
    for (index, line) in lines.iter().enumerate() {
        items.push((line.visual_top, Some(index), None));
    }
    for (index, box_) in boxes.iter().enumerate() {
        items.push((box_.top, None, Some(index)));
    }
    items.sort_by(|left, right| left.0.total_cmp(&right.0));

    for (_, line_slot, box_slot) in items {
        if let Some(box_index) = box_slot {
            let box_ = &boxes[box_index];
            let current = starts.last().unwrap();
            let clip_bottom = current.top + page_height - margin_bottom;
            let height = box_.bottom - box_.top;
            // Keep an image whole: start a new page when it does not fit in
            // the space left, unless it is taller than a page even on its own
            // (then it has to be clipped, which the CSS max-height prevents).
            if box_.bottom > clip_bottom
                && box_.top > current.clip_top
                && height <= page_content_height
            {
                starts.push(PageStart {
                    top: (box_.top - margin_top).max(0.0),
                    clip_top: box_.top,
                });
            }
            page_has_content = true;
            continue;
        }
        let Some(line_index) = line_slot else {
            continue;
        };
        let line = &lines[line_index];
        let current = starts.last().unwrap();
        while forced_index < forced_breaks.len()
            && forced_breaks[forced_index] <= line.visual_top + BREAK_EPSILON
        {
            if forced_breaks[forced_index] > current.clip_top + BREAK_EPSILON
                && line.top > current.clip_top + BREAK_EPSILON
            {
                pending_break = true;
            }
            forced_index += 1;
        }

        // A page that only holds an empty line box is a blank page between two
        // chapters, and it cannot produce an XPointer. Wait for the first line
        // that carries text instead.
        let has_text = line.text_end > line.text_start;
        if pending_break && has_text {
            pending_break = false;
            if page_has_content {
                starts.push(page_start_for_line(line, margin_top));
                // The break already starts a page at this line; running the
                // height check as well pushed a second start for the same line,
                // which made an empty page and a duplicate start XPointer.
                page_has_content = true;
                continue;
            }
        }

        let current = starts.last().unwrap();
        let clip_bottom = current.top + page_height - margin_bottom;
        if has_text && line.bottom > clip_bottom && line.top > current.clip_top {
            starts.push(page_start_for_line(line, margin_top));
        }
        if has_text {
            page_has_content = true;
        }
    }
    starts
}

fn page_start_for_line(line: &LineBox, margin_top: f32) -> PageStart {
    let clip_top = line.visual_top;
    let top = (clip_top - margin_top).max(0.0);
    PageStart { top, clip_top }
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
    let center_x = rect.x + rect.width * 0.5;
    let center_y = rect.y + rect.height * 0.5;
    match direction {
        0 => true,
        1 => center_x < point.x,
        2 => center_x > point.x,
        3 => center_y < point.y,
        4 => center_y > point.y,
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
    let dx = (center_x - point.x).abs();
    let dy = (center_y - point.y).abs();
    let primary = match direction {
        3 | 4 => dx + dy * 100.0,
        _ => dx + dy,
    };
    (primary, 0.0, center_x, center_y, word.text.clone())
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

fn flat_text_characters(
    root: &Node,
    rendered: &std::collections::HashSet<NodeId>,
) -> Vec<FlatCharacter> {
    let mut out = Vec::new();
    collect_flat_characters(root, rendered, &mut out);
    out
}

fn collect_flat_characters(
    node: &Node,
    rendered: &std::collections::HashSet<NodeId>,
    out: &mut Vec<FlatCharacter>,
) {
    if let Some(text) = node.text_data()
        && rendered.contains(&node.id)
    {
        for (offset, ch) in text.content.chars().enumerate() {
            out.push(FlatCharacter {
                node: node.id,
                offset: offset as u32,
                ch,
            });
        }
    }
    for child in node.children.iter() {
        collect_flat_characters(node.with(*child), rendered, out);
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

fn lookup_resource(resources: &ResourceSource, key: &str) -> Option<Vec<u8>> {
    match resources {
        ResourceSource::Empty => None,
        ResourceSource::Map(map) => map.get(key).cloned(),
        ResourceSource::Epub(provider) => {
            let data = provider.get(key)?;
            Some((*data).clone())
        }
    }
}

fn find_image_data_at(
    doc: &HtmlDocument,
    node: &Node,
    page_top: f32,
    x: f32,
    y: f32,
    base_url: &str,
    resources: &ResourceSource,
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
            if let Some(data) = lookup_resource(resources, key) {
                return Some(data);
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
    book.resource(&key)
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
    render_svg_scaled(data, target_width, target_height, true)
}

/// Rasterizes an SVG so it fits within `max_width` x `max_height` without ever
/// enlarging it. Used for page resources: a small icon must stay small, while a
/// cover taller than the page is scaled down to fit.
pub fn render_svg_image_fit(
    data: &[u8],
    max_width: u32,
    max_height: u32,
) -> Option<(Vec<u8>, u32, u32)> {
    render_svg_scaled(data, Some(max_width), Some(max_height), false)
}

fn render_svg_scaled(
    data: &[u8],
    target_width: Option<u32>,
    target_height: Option<u32>,
    allow_upscale: bool,
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

    let mut scale = match (target_width, target_height) {
        (Some(width), Some(height)) => (width as f32 / src_width).min(height as f32 / src_height),
        (Some(width), None) => width as f32 / src_width,
        (None, Some(height)) => height as f32 / src_height,
        (None, None) => 1.0,
    };
    if !allow_upscale {
        scale = scale.min(1.0);
    }
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

pub fn resolve_font_paths(options: &DocumentOptions) -> Result<Vec<PathBuf>> {
    let main_path = resolve_font_path(options)?;
    let mut paths = vec![main_path];
    for fallback_path in options.fallback_font_paths.iter() {
        if !paths.iter().any(|existing| existing == fallback_path) {
            paths.push(fallback_path.clone());
        }
    }
    Ok(paths)
}

/// Shared font file bytes. Font files can be large (CJK fallbacks are tens of
/// megabytes on Kindles), so they are mmapped when possible: the pages stay
/// file-backed and reclaimable instead of being an anonymous heap copy, and
/// fontique can read outlines lazily. The same allocation is shared between
/// the document's cache and the font collection (no `to_vec()` copies).
pub type FontBlob = Arc<dyn AsRef<[u8]> + Send + Sync>;

pub fn load_font_datas(paths: &[PathBuf]) -> Vec<FontBlob> {
    let mut datas: Vec<FontBlob> = Vec::new();
    for path in paths.iter() {
        if let Some(blob) = load_font_blob(path) {
            datas.push(blob);
        }
    }
    datas
}

/// Loads a font file as a shared, mmapped blob when possible.
pub fn load_font_blob(path: &Path) -> Option<FontBlob> {
    if let Ok(file) = std::fs::File::open(path)
        && let Ok(mapped) = unsafe { memmap2::Mmap::map(&file) }
    {
        return Some(Arc::new(mapped) as FontBlob);
    }
    std::fs::read(path)
        .ok()
        .map(|data| Arc::new(data) as FontBlob)
}

fn collect_font_datas(options: &DocumentOptions) -> Result<Vec<FontBlob>> {
    let paths = resolve_font_paths(options)?;
    if let Some(main_path) = paths.first() {
        let main_font = load_font_blob(main_path)
            .with_context(|| format!("read font {}", main_path.display()))?;
        let mut datas = vec![main_font];
        for fallback_path in paths.iter().skip(1) {
            if let Some(data) = load_font_blob(fallback_path) {
                datas.push(data);
            }
        }
        return Ok(datas);
    }
    Ok(Vec::<FontBlob>::new())
}

fn build_font_ctx_with_fallbacks(
    font_datas: &[FontBlob],
    preferred_family: Option<&str>,
) -> FontContext {
    let mut ctx = FontContext {
        source_cache: SourceCache::new_shared(),
        collection: Collection::new(CollectionOptions {
            shared: false,
            system_fonts: false,
        }),
    };
    let family_ids: Vec<FamilyId> = font_datas
        .iter()
        .flat_map(|data| {
            let registered = ctx.collection.register_fonts(Blob::new(data.clone()), None);
            registered
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<FamilyId>>()
        })
        .collect();
    // A collection registers several families from one file. Put the family the
    // reader picked first, so the generators' `serif`/`sans-serif` resolve to
    // that face instead of whichever one happened to be registered first.
    let preferred = preferred_family.and_then(|name| {
        let wanted = normalize_family_name(name);
        let names: Vec<String> = ctx.collection.family_names().map(str::to_string).collect();
        names
            .iter()
            .find(|existing| normalize_family_name(existing) == wanted)
            .and_then(|existing| ctx.collection.family_by_name(existing))
            .map(|family| family.id())
    });
    let mut ordered: Vec<FamilyId> = Vec::with_capacity(family_ids.len());
    if let Some(id) = preferred {
        ordered.push(id);
    }
    ordered.extend(
        family_ids
            .iter()
            .copied()
            .filter(|id| Some(*id) != preferred),
    );
    for generic in [
        GenericFamily::SansSerif,
        GenericFamily::Serif,
        GenericFamily::Monospace,
        GenericFamily::SystemUi,
    ] {
        ctx.collection
            .append_generic_families(generic, ordered.iter().copied());
    }
    ctx
}

/// Family names are compared the way KOReader matches them: case and separators
/// do not matter ("Noto Serif CJK SC" == "NotoSerifCJKsc").
fn normalize_family_name(name: &str) -> String {
    name.chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// block's layout text.
fn create_document(
    html: &str,
    base_url: &str,
    net: Arc<dyn NetProvider>,
    font_context: Arc<FontContext>,
    options: &DocumentOptions,
) -> HtmlDocument {
    let config = DocumentConfig {
        viewport: Some(Viewport::new(
            options.width,
            options.layout_height,
            options.viewport_scale,
            ColorScheme::Light,
        )),
        base_url: Some(base_url.to_string()),
        net_provider: Some(net),
        font_ctx: Some(font_context.as_ref().clone()),
        style_threading: StyleThreading::Sequential,
        ..Default::default()
    };

    let stripped = if options.embedded_styles_enabled {
        String::new()
    } else {
        strip_author_styles(html)
    };
    let base_html = if options.embedded_styles_enabled {
        html
    } else {
        stripped.as_str()
    };
    let mut html_source = base_html.to_string();
    if let Some(stylesheet) = options
        .stylesheet
        .as_deref()
        .filter(|value| !value.is_empty())
        && !stylesheet.contains("</style")
    {
        html_source.push_str("<style>");
        html_source.push_str(stylesheet);
        html_source.push_str("</style>");
    }

    // Keep images whole: cap them to one page's content height (width is
    // already capped), so pagination never has to clip an image. crengine
    // scales oversized images the same way.
    let (_, page_height_css, page_margin_css) = css_page_geometry(
        options.width,
        options.height,
        options.viewport_scale,
        options.page_margin,
    );
    let max_image_height = (page_height_css - page_margin_css * 2.0).max(1.0);
    html_source.push_str(&format!(
        "<style>img, svg, video, canvas, object, embed {{ max-width: 100%; \
         max-height: {max_image_height:.0}px; height: auto; }}</style>"
    ));
    // The generators leave the base font size as a token so the chunk HTML (and
    // the chunk boundaries that page numbers derive from) does not change with
    // it; the layout settings are applied here instead.
    let html_source = if html_source.contains(cre_epub::FONT_SIZE_TOKEN) {
        html_source.replace(
            cre_epub::FONT_SIZE_TOKEN,
            &format!("{:.2}", options.font_size),
        )
    } else {
        html_source
    };
    let mut doc = HtmlDocument::from_html(html_source.as_str(), config);
    doc.resolve(0.0);
    for _ in 0..4 {
        doc.handle_messages();
        doc.resolve(0.0);
    }
    doc
}

fn base_style_block_is_marker(body: &[u8]) -> bool {
    // Cheap check; the marker is our own constant and ASCII.
    body.windows(MARKER_INNER.len())
        .any(|window| window == MARKER_INNER)
}

const MARKER_INNER: &[u8] = b"cre-render-base";

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
    let mut search_from = 0usize;
    loop {
        let lower = lower_ascii(&current);
        let Some(start) = find_case_insensitive(&lower, b"<style", search_from) else {
            break;
        };
        // Style tags in our generated chunks may carry attributes.
        let Some(open_end) = find_case_insensitive(&lower, b">", start) else {
            break;
        };
        let Some(close_tag) = find_case_insensitive(&lower, b"</style>", open_end) else {
            break;
        };
        let close = close_tag + b"</style>".len();
        if base_style_block_is_marker(&current[open_end + 1..close_tag]) {
            // Our generated base typography CSS (font size, line height,
            // page padding) must survive "disable embedded styles": it is
            // our render parameter, not author styling. Keep and continue
            // searching after it.
            search_from = close;
        } else {
            current = remove_range(&current, start, close);
            search_from = 0;
        }
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

fn page_chunk_clip_bottom(
    page_starts: &[PageStart],
    index: usize,
    page_height_css: f32,
    page_margin_css: f32,
) -> f32 {
    let start = page_starts[index];
    page_starts
        .get(index + 1)
        .map_or(start.top + page_height_css - page_margin_css, |next| {
            next.clip_top
        })
}

fn first_non_blank_line_flow(
    root: &Node,
    lines: &[LineBox],
    start: PageStart,
    clip_bottom: f32,
    line_flows: &[i64],
    previous_page_flow: i64,
) -> i64 {
    let lower = start.clip_top;
    let upper = clip_bottom;
    for (local_line, line) in lines.iter().enumerate() {
        if line.top + 0.5 < lower || line.top + 0.5 >= upper {
            continue;
        }
        if let Some(text) = line_text_from_root(root, line)
            && !text.trim().is_empty()
        {
            return line_flows
                .get(local_line)
                .copied()
                .unwrap_or(previous_page_flow);
        }
    }
    previous_page_flow
}

#[cfg(test)]
mod layout_plan_tests {
    use super::*;

    const MB: usize = 1024 * 1024;

    #[test]
    fn small_book_fits_in_one_layout_on_a_kindle() {
        // PW2 with the framework stopped: ~80 MB free, a 250 kB novel.
        assert_eq!(plan_chunk_bytes(250 * 1024, Some(80 * MB)), usize::MAX);
    }

    #[test]
    fn large_book_is_split_to_fit_available_memory() {
        // 80 MB available: budget is min(available - 24 MB, available / 2)
        // = 40 MB of layout, i.e. 320 kB of HTML per layout (also the cap).
        let target = plan_chunk_bytes(4 * MB, Some(80 * MB));
        assert_eq!(target, MAX_CHUNK_BYTES);
        let chunks = (4 * MB).div_ceil(target);
        assert_eq!(chunks, 32, "4 MB should be split into 32 chunks at the cap");
        // Each planned layout stays well inside the budget: 128 kB * 128.
        assert!(target * LAYOUT_BYTES_PER_HTML_BYTE <= 40 * MB);
    }

    #[test]
    fn chunk_size_is_clamped_to_sane_bounds() {
        // Plenty of memory: never go above a megabyte per chunk.
        assert_eq!(plan_chunk_bytes(64 * MB, Some(512 * MB)), MAX_CHUNK_BYTES);
        // Almost nothing left: never go below a page-sized chunk.
        assert_eq!(plan_chunk_bytes(64 * MB, Some(MB)), MIN_CHUNK_BYTES);
    }

    #[test]
    fn unknown_memory_keeps_the_whole_book() {
        assert_eq!(plan_chunk_bytes(4 * MB, None), usize::MAX);
    }
}

/// Collects the absolute rects of replaced elements (images and friends).
fn collect_content_boxes(doc: &HtmlDocument, root: &Node) -> Vec<ContentBox> {
    let mut boxes = Vec::new();
    collect_content_boxes_into(doc, root, &mut boxes);
    boxes.sort_by(|left, right| left.top.total_cmp(&right.top));
    boxes
}

fn collect_content_boxes_into(doc: &HtmlDocument, node: &Node, out: &mut Vec<ContentBox>) {
    if let Some(element) = node.element_data() {
        let tag = element.name.local.to_string();
        if matches!(
            tag.as_str(),
            "img" | "svg" | "video" | "canvas" | "object" | "embed"
        ) && let Some(rect) = doc.get_client_bounding_rect(node.id)
            && rect.height > 0.0
        {
            out.push(ContentBox {
                top: rect.y as f32,
                bottom: (rect.y + rect.height) as f32,
            });
            // No need to descend into a replaced element.
            return;
        }
    }
    for child in &node.children {
        collect_content_boxes_into(doc, node.with(*child), out);
    }
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

fn css_page_geometry(
    width: u32,
    height: u32,
    viewport_scale: f32,
    page_margin: f32,
) -> (u32, f32, f32) {
    let viewport = Viewport::new(width, height, viewport_scale, ColorScheme::Light);
    let (_css_width, css_height) = viewport.logical_size();
    let css_height = css_height.max(1.0);
    let css_margin = page_margin / viewport_scale;
    let css_height_u32 = css_height as u32;
    (css_height_u32, css_height, css_margin)
}

fn collect_spine_chunks(
    node: &Node,
    chunk_index: usize,
    out: &mut HashMap<u32, usize>,
    real: &mut HashMap<u32, u32>,
) {
    if let Some(index) =
        node_attr(node, "data-spine-index").and_then(|value| value.parse::<u32>().ok())
    {
        out.entry(index).or_insert(chunk_index);
        let real_index = node_attr(node, "data-spine-real-index")
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(index);
        real.entry(index).or_insert(real_index);
    }
    for child in &node.children {
        collect_spine_chunks(node.with(*child), chunk_index, out, real);
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

/// Y-position of the first laid-out node in document order at or after `target`.
fn first_rect_after(doc: &HtmlDocument, target: NodeId) -> Option<f32> {
    fn walk(doc: &HtmlDocument, node: &Node, target: NodeId, seen: &mut bool) -> Option<f32> {
        if node.id == target {
            *seen = true;
        } else if *seen
            // Only elements have layout data; text and comment nodes panic.
            && node.element_data().is_some()
            && let Some(rect) = doc.get_client_bounding_rect(node.id)
            && rect.width > 0.0
        {
            return Some(rect.y as f32);
        }
        for child in node.children.iter() {
            if let Some(top) = walk(doc, node.with(*child), target, seen) {
                return Some(top);
            }
        }
        None
    }
    let root = doc.root_element();
    let mut seen = false;
    walk(doc, root, target, &mut seen)
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

/// Page of a spine document target.
///
/// `page-break-before: always` is deferred to the first line that carries text
/// (see [`paginate`]). A title page with a large top margin therefore has its
/// section top on the previous page, while its first content (the title line)
/// is on the next page. Map the href to the page of the section's first
/// content, not to the page that contains the section top edge.
fn page_for_spine_node(
    doc: &HtmlDocument,
    node_id: NodeId,
    lines: &[LineBox],
    boxes: &[ContentBox],
    page_starts: &[PageStart],
) -> Option<usize> {
    let rect = doc.get_client_bounding_rect(node_id)?;
    let top = rect.y as f32;
    let bottom = (rect.y + rect.height) as f32 + 1.0;
    let first_line = lines
        .iter()
        .filter(|line| line.text_end > line.text_start)
        .map(|line| line.visual_top)
        .filter(|y| *y + 1.0 >= top && *y <= bottom)
        .min_by(f32::total_cmp);
    let first_box = boxes
        .iter()
        .map(|box_| box_.top)
        .filter(|y| *y + 1.0 >= top && *y <= bottom)
        .min_by(f32::total_cmp);
    // Use the first content of the section. `top` is only a fallback for an
    // empty section: taking the minimum with it would select the section top
    // again and bring back the previous-page mapping.
    let target = match (first_line, first_box) {
        (Some(line), Some(box_)) => line.min(box_),
        (Some(line), None) => line,
        (None, Some(box_)) => box_,
        (None, None) => top,
    };
    Some(page_containing_top(target, page_starts))
}

fn page_containing_top(top: f32, page_starts: &[PageStart]) -> usize {
    let mut page = 1usize;
    for (index, start) in page_starts.iter().enumerate() {
        if top + 1.0 >= start.clip_top {
            page = index + 1;
        } else {
            break;
        }
    }
    page
}

fn page_for_node(doc: &HtmlDocument, node_id: NodeId, page_starts: &[PageStart]) -> Result<usize> {
    let top = match doc.get_client_bounding_rect(node_id) {
        Some(rect) => rect.y as f32,
        // Empty inline anchors (`<a id="fileposN"></a>`, which is what a MOBI
        // link pointing into the middle of a text run becomes) get no layout
        // box. The position they mean is the content that follows them, so use
        // the first node after it that does have a box.
        None => first_rect_after(doc, node_id).context("navigation target has no layout rect")?,
    };
    Ok(page_containing_top(top, page_starts))
}

/// Rasterizes SVG resources once, at page size, and keeps the PNGs in a
/// byte-bounded cache.
struct SvgRaster {
    max_width: u32,
    max_height: u32,
    cache: std::sync::Mutex<SvgRasterCache>,
}

#[derive(Default)]
struct SvgRasterCache {
    entries: HashMap<String, Arc<Vec<u8>>>,
    order: std::collections::VecDeque<String>,
    bytes: usize,
    limit: usize,
}

impl SvgRaster {
    fn new(width: u32, height: u32) -> Self {
        Self {
            max_width: width.max(1),
            max_height: height.max(1),
            cache: std::sync::Mutex::new(SvgRasterCache {
                limit: 8 * 1024 * 1024,
                ..SvgRasterCache::default()
            }),
        }
    }

    fn rasterize(&self, key: &str, bytes: &[u8]) -> Option<Vec<u8>> {
        if !looks_like_svg(bytes) {
            return None;
        }
        if let Ok(cache) = self.cache.lock()
            && let Some(hit) = cache.entries.get(key)
        {
            return Some((**hit).clone());
        }
        let (rgba, width, height) = render_svg_image_fit(bytes, self.max_width, self.max_height)?;
        let png = encode_png(&rgba, width, height)?;
        if let Ok(mut cache) = self.cache.lock() {
            cache.bytes += png.len();
            cache.entries.insert(key.to_string(), Arc::new(png.clone()));
            cache.order.push_back(key.to_string());
            while cache.bytes > cache.limit {
                let Some(oldest) = cache.order.pop_front() else {
                    break;
                };
                if let Some(removed) = cache.entries.remove(&oldest) {
                    cache.bytes = cache.bytes.saturating_sub(removed.len());
                }
            }
        }
        Some(png)
    }
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(512)];
    let text = String::from_utf8_lossy(head);
    let text = text.trim_start_matches('\u{feff}').trim_start();
    text.starts_with("<svg") || (text.starts_with("<?xml") && text.contains("<svg"))
}

fn encode_png(rgba: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    Some(out)
}

struct EpubNet {
    resource_provider: Arc<LazyResource>,
    svg: Arc<SvgRaster>,
}

impl NetProvider for EpubNet {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let path = percent_decode_str(request.url.path())
            .decode_utf8_lossy()
            .trim_start_matches('/')
            .to_string();
        let key = normalize_path(&path);
        let bytes = self
            .resource_provider
            .get(&key)
            .map(|data| (*data).clone())
            .unwrap_or_default();
        let bytes = self.svg.rasterize(&key, &bytes).unwrap_or(bytes);
        handler.bytes(request.url.to_string(), Bytes::from(bytes));
    }
}

/// Serves resources held in memory (FB2 binaries, MOBI/KF8 images), under
/// `binary/<name>` or a bare `<name>`.
struct MapNet {
    resources: Arc<HashMap<String, Vec<u8>>>,
    svg: Arc<SvgRaster>,
}

impl NetProvider for MapNet {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let path = percent_decode_str(request.url.path())
            .decode_utf8_lossy()
            .trim_start_matches('/')
            .to_string();
        let name = path.strip_prefix("binary/").unwrap_or(&path).to_string();
        let bytes = self.resources.get(&name).cloned().unwrap_or_default();
        let bytes = self.svg.rasterize(&name, &bytes).unwrap_or(bytes);
        handler.bytes(request.url.to_string(), Bytes::from(bytes));
    }
}

/// Serves resources referenced by a standalone HTML document from the
/// document's own directory. Paths are resolved and canonicalized, and must
/// stay inside that directory.
struct FileNet {
    root: PathBuf,
    svg: Arc<SvgRaster>,
}

impl NetProvider for FileNet {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let decoded = percent_decode_str(request.url.path())
            .decode_utf8_lossy()
            .to_string();
        let mut bytes = Vec::new();
        if let Ok(candidate) = std::fs::canonicalize(&decoded)
            && candidate.starts_with(&self.root)
            && candidate.is_file()
        {
            bytes = std::fs::read(&candidate).unwrap_or_default();
        }
        let bytes = self.svg.rasterize(&decoded, &bytes).unwrap_or(bytes);
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
<style type="text/css">/* cre-render-base */
body { font-size: 20px; }
</style>
</head>
<body><p class="hidden">visible</p></body>
</html>
"#;
    let stripped = strip_author_styles(html);
    let bytes = stripped.as_bytes();
    // author styles are dropped
    assert!(find_case_insensitive(bytes, b"hidden { visibility", 0).is_none());
    assert!(find_case_insensitive(bytes, b"<link", 0).is_none());
    // our base typography CSS survives so font size (and page margins)
    // keep working with embedded styles disabled
    assert!(find_case_insensitive(bytes, b"cre-render-base", 0).is_some());
    assert!(find_case_insensitive(bytes, b"font-size: 20px", 0).is_some());
    assert!(find_case_insensitive(bytes, b"visible", 0).is_some());
}

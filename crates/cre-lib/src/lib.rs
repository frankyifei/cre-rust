use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod document;
mod layout;
mod navigate;
mod net;
mod page_table;
mod render;
mod search;
mod text;

use anyhow::{Context, Result, bail};
use anyrender::Scene;
use blitz_dom::{DocumentConfig, Node, NodeId, Point, StyleThreading};
use blitz_html::HtmlDocument;
use blitz_traits::net::NetProvider;
use blitz_traits::shell::{ColorScheme, Viewport};
#[cfg(test)]
use cre_book::chunk::{
    LAYOUT_BYTES_PER_HTML_BYTE, MAX_CHUNK_BYTES, MIN_CHUNK_BYTES, plan_chunk_bytes,
};
use cre_book::chunk::{
    LAYOUT_MEMORY_RESERVE, available_memory_bytes, chunk_budgets, stream_single_document_chunks,
};
use cre_book::html::attr_value;
use cre_book::images::normalize_page_images;
use cre_book::normalize_path;
use cre_book::{GeneratedResources, MapResources, ResourceSource, generated_resource};
use cre_epub::{EpubBook, HtmlOptions, PageMapItem as RawPageMapItem, TocItem as RawTocItem};
use cre_fb2::{Fb2Book, Fb2HtmlOptions};
use cre_kf8::{Kf8Book, Kf8HtmlOptions};
use cre_mobi::{MobiBook, MobiHtmlOptions};
use cre_paint::render_page as paint_page;
use cre_paint::render_page_cached as paint_page_cached;
use cre_paint::render_page_rgba as paint_page_rgba;
use net::{EmptyNet, EpubNet, FileNet, MapNet};
use page_table::{PageMeta, PageTable};

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

mod memory_report;
mod page_breaks;
mod position;
mod xpointer;

pub use memory_report::LowMemory;

pub use position::{PathSegment, TextPosition, XPointerRange, parse_text_position};

const SEARCH_FLAGS_CASE_INSENSITIVE: i32 = 1 << 1;
const SEARCH_FLAGS_COMMENTS: i32 = 1 << 2;
const SEARCH_FLAGS_MULTILINE: i32 = 1 << 3;
const SEARCH_FLAGS_LITERAL: i32 = 1 << 4;
const SEARCH_FLAGS_DOTALL: i32 = 1 << 5;

/// Chunk HTML above which opening lays out the first chunk only.
///
/// The open-time pass is proportional to this. Measured on x86_64 release:
/// 1.7 MB of chunk HTML (18 chunks) lays out in ~0.25 s, 25 MB (268 chunks) in
/// 6.1 s; a PW2 takes roughly 4-6x that. Below this threshold the whole book is
/// laid out at open, which keeps page numbers exact from the first draw and
/// costs at most a second on the device; above it the reader gets the first page
/// in a fraction of the time and the rest arrives in the background.
const LAZY_OPEN_MIN_BYTES: usize = 3 * 1024 * 1024;

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
    /// Kept for the API (and for the reader's cache key): it used to be the
    /// layout viewport's height, which made viewport units resolve against the
    /// whole chunk. The layout viewport is the page now, see
    /// [`create_document`].
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

/// The four page margins, in device pixels.
///
/// `DocumentOptions::page_margin` only carries the top one (it predates the
/// reader telling us all four). The reader installs the real values with
/// [`Document::set_page_margins`]; a caller that never does keeps the uniform
/// margin for every side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageMargins {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    /// Set by [`Document::set_page_margins`]: the caller configured the margins,
    /// so they replace the padding the format generator baked into the chunk.
    explicit: bool,
}

impl PageMargins {
    /// All four sides at `margin`, as a caller that only knows one margin.
    pub fn uniform(margin: f32) -> Self {
        Self {
            left: margin,
            top: margin,
            right: margin,
            bottom: margin,
            explicit: false,
        }
    }

    /// Four margins from a reader, which replace the generated padding.
    pub fn new(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
            explicit: true,
        }
    }

    /// True when the caller configured the margins (rather than them coming
    /// from `DocumentOptions::page_margin`).
    pub fn is_explicit(&self) -> bool {
        self.explicit
    }
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
    /// Word range around the hit point, from parley's `Selection::word_from_point`
    /// (byte offsets relative to the line's layout text).
    word_range: Option<(usize, usize)>,
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
    /// Vertical extent of the element's own box when it is smaller than the
    /// line. A MathML token is an inline-block glyph inside the formula's line,
    /// so its selection box is the glyph, not the whole formula.
    pub(crate) box_top: f32,
    pub(crate) box_bottom: f32,
    /// Inside `<span class="cre-math">`: the tokens of one formula are joined
    /// into one run of text instead of one line each.
    pub(crate) math: bool,
    /// The `.cre-math` block itself: its lines are the formula's real lines.
    pub(crate) math_root: bool,
    /// Ordering key for text extraction: the formula's line for a math token
    /// (whose own line top differs by a few pixels), the line top otherwise.
    pub(crate) sort_top: f32,
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

/// What one laid-out chunk is measured to cost, per byte of generated HTML.
///
/// Chunk-sized layouts measure about 50 B/B (128 KB of plain text -> 7 MB,
/// 512 KB -> 25.8 MB), far below the whole-book 128 B/B: a chunk's computed
/// styles and line boxes do not grow with the book. The low-memory check and
/// the worker estimate use this figure, so a book is not refused because its
/// chunks were priced as whole documents.
const CHUNK_DOM_BYTES_PER_HTML_BYTE: usize = 64;

/// The reserve the low-memory check assumes, scaled to the device.
///
/// The fixed 24 MB was most of a 38 MB budget, so a book that needed 30 MB was
/// refused although it fit. A device with little free memory gets an 8 MB
/// floor instead; a roomy one keeps the old 24 MB.
fn layout_reserve(available: usize) -> usize {
    (available / 8).clamp(8 * 1024 * 1024, LAYOUT_MEMORY_RESERVE)
}

/// Installed memory, where the platform reports it (diagnostics only).
fn memory_total_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let info = std::fs::read_to_string("/proc/meminfo").ok()?;
        info.lines()
            .find(|line| line.starts_with("MemTotal:"))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u64>().ok())
            .map(|kb| kb.saturating_mul(1024))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Decoded image bytes that live next to one chunk's DOM on the in-memory
/// resource path (MOBI, AZW3/KF8, FB2).
///
/// Blitz is served a transparent SVG that carries the intrinsic size for every
/// raster `<img>` (see `docs/IMAGES.md`), so laying a chunk out decodes no
/// image bytes; the pictures are decoded with DCT scaling when they are
/// painted. The map's own bytes are already resident when the memory check
/// runs, so counting them again would refuse books that open with room to
/// spare (measured: an AZW3 refused at 38 MB while its open peaked at 35 MB).
const MAP_CHUNK_IMAGE_BYTES: u64 = 0;

/// Collects what [`Document::build_from_sources`] needs while a format that is
/// generated as one document (MOBI, AZW3/KF8, FB2) streams its chunks.
///
/// This is the same work the EPUB callback does: fixed-layout detection and
/// the `<img>` keys that may become size placeholders.
struct ChunkSink {
    sources: Vec<ChunkSource>,
    fixed_layouts: Vec<Option<FixedLayout>>,
    content_images: Vec<String>,
}

impl ChunkSink {
    fn new() -> Self {
        Self {
            sources: Vec::new(),
            fixed_layouts: Vec::new(),
            content_images: Vec::new(),
        }
    }

    fn push(&mut self, html: String) -> Result<()> {
        self.fixed_layouts.push(parse_fixed_layout(&html));
        self.content_images
            .extend(cre_book::image_sources_in(&html));
        self.sources.push(ChunkSource::new(html));
        Ok(())
    }
}

/// Returns freed heap pages to the OS. Building a chunk's DOM/parley layout
/// allocates and frees tens of megabytes; glibc keeps those arenas resident,
/// which matters on a 256 MB device where the next page render can then OOM.
///
fn trim_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        // `CRE_NO_TRIM=1` isolates the cost of trimming on a device: glibc walks
        // every arena and madvises the free spans.
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *ENABLED.get_or_init(|| std::env::var_os("CRE_NO_TRIM").is_none()) {
            let profile = std::env::var_os("CRE_LAYOUT_PROFILE").is_some();
            let started = std::time::Instant::now();
            libc::malloc_trim(0);
            if profile {
                eprintln!(
                    "profile trim: {:.2}ms rss={}kB",
                    started.elapsed().as_secs_f64() * 1000.0,
                    resident_bytes().unwrap_or(0) / 1024
                );
            }
        }
    }
}

/// Resident set size in bytes, where the platform makes it cheap to read.
fn resident_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let pages: u64 = std::fs::read_to_string("/proc/self/statm")
            .ok()?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()?;
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page_size = if page_size > 0 {
            page_size as u64
        } else {
            4096
        };
        Some(pages * page_size)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

const PAGE_IMAGE_CACHE_CAPACITY: usize = 2usize;

#[derive(Clone, Copy, Debug, PartialEq)]
enum FixedLayout {
    Pixel {
        width: f32,
        height: f32,
    },
    /// `width=device-width, height=device-height`: the initial containing block
    /// is the reader viewport, resolved when the chunk is laid out.
    Device,
}

impl FixedLayout {
    fn size(self, reader_width: f32, reader_height: f32) -> (f32, f32) {
        match self {
            FixedLayout::Pixel { width, height } => (width, height),
            FixedLayout::Device => (reader_width.max(1.0), reader_height.max(1.0)),
        }
    }
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

/// Byte counts for what [`Document`] keeps resident between page renders.
///
/// Diagnostics only: `perf_probe` prints it so memory work can be measured
/// instead of guessed. It does not include the active chunk's Blitz DOM and
/// Parley layout, which the process RSS shows but this cannot enumerate.
#[derive(Debug, Clone, Default)]
pub struct MemoryBreakdown {
    /// Generated chunk HTML kept for relayouts (uncompressed size).
    pub source_bytes: usize,
    /// What the compressed chunk HTML actually holds.
    pub compressed_source_bytes: usize,
    /// Images replaced by a size placeholder (their pixels are painted from
    /// the original at draw time).
    pub placeheld_images: usize,
    pub chunk_count: usize,
    pub laid_out_chunks: usize,
    pub page_count: usize,
    /// Page metadata (start, clip, flow) of the laid-out chunks.
    pub page_meta_bytes: usize,
    /// One entry per laid-out line, used for page lookup.
    pub line_entries: usize,
    /// Page-start XPointers, the bulk of the page index strings.
    pub xpointer_bytes: usize,
    pub anchor_entries: usize,
    pub glyph_cache_bytes: usize,
    pub page_bitmap_bytes: usize,
    /// Line boxes of the chunk currently activated (freed on chunk change).
    pub active_line_boxes: usize,
    pub active_text_nodes: usize,
    /// Font files (mmapped; RSS counts only the pages touched).
    pub font_bytes: usize,
    /// Decompressed resources kept in the provider's LRU (images read to learn
    /// their size, CSS, spine documents).
    pub resource_cache_bytes: usize,
    /// Engine-generated resources pinned for the session (rewritten spine
    /// documents and image-only page wrappers).
    pub pinned_bytes: usize,
    /// Page-sized PNGs of SVG resources.
    pub svg_cache_bytes: usize,
    /// The text the page map was built from.
    pub page_map_source_bytes: usize,
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

/// One chunk's generated HTML, kept compressed.
///
/// A large text book keeps the generated HTML of every chunk so a settings
/// change can re-lay the book out without re-reading and re-splitting it. That
/// copy is about the size of the book's text, which is tens of megabytes for a
/// big novel, so each chunk is stored as an LZ4 block and decompressed only
/// when that chunk is laid out (including a TOC jump straight to it). The
/// compressor is fast enough that a layout does not notice it.
/// A spine section carried by a chunk's HTML, read from the markup rather than
/// from a layout.
///
/// It is the chunk → `DocFragment` map an XPointer needs, and it is available
/// before the chunk has ever been laid out. That matters when a book is opened
/// lazily: the reader restores its position (an XPointer) before the chunk that
/// holds it has been laid out, and the jump would otherwise have no the map.
#[derive(Clone, Debug)]
struct SpineSection {
    /// XPointer section number (`data-spine-index + 1`, see
    /// `xpointer::spine_index_for_node`).
    slot: u32,
    /// Value the XPointer text uses for this section.
    real: u32,
    /// Resource path of the section, used to give a TOC entry a page before its
    /// chunk is laid out.
    href: Option<String>,
}

struct ChunkSource {
    compressed: Vec<u8>,
    /// Uncompressed length, which the layout uses for its estimates.
    text_len: usize,
    /// `<img`/`<image` occurrences, used to pick the probe chunk.
    images: usize,
    /// Sections in this chunk, in document order.
    spine: Vec<SpineSection>,
}

impl ChunkSource {
    fn new(html: String) -> Self {
        let images = html
            .match_indices("<img")
            .count()
            .saturating_add(html.match_indices("<image").count());
        let text_len = html.len();
        let spine = scan_spine_sections(&html);
        let mut compressed = lz4_flex::compress_prepend_size(html.as_bytes());
        compressed.shrink_to_fit();
        Self {
            compressed,
            text_len,
            images,
            spine,
        }
    }

    /// Fails loudly: the block was produced by `new` a moment earlier, so an
    /// error here is a bug, and a silently empty chunk would show as a blank
    /// book instead of a diagnosable failure.
    fn text(&self) -> anyhow::Result<String> {
        let bytes = lz4_flex::decompress_size_prepended(&self.compressed)
            .context("decompress chunk HTML")?;
        String::from_utf8(bytes).context("chunk HTML is not UTF-8")
    }
}

/// Reads the spine sections a chunk's HTML carries, without parsing it.
///
/// The generators wrap every spine document in a `<section>` that names its
/// `DocFragment`; that markup is what an XPointer needs to find the chunk it
/// points into, so it must be known before the chunk has a layout (a lazily
/// opened book restores its position first). Scanning the attributes is the
/// same information [`collect_spine_chunks`] used to gather from the parsed
/// DOM, at string-search cost instead of a layout.
fn scan_spine_sections(html: &str) -> Vec<SpineSection> {
    const NEEDLE: &str = "data-spine-index=\"";
    let mut sections: Vec<SpineSection> = Vec::new();
    let mut rest = html;
    while let Some(position) = rest.find(NEEDLE) {
        let after = &rest[position + NEEDLE.len()..];
        let Some(quote) = after.find('"') else {
            break;
        };
        let Ok(index) = after[..quote].parse::<u32>() else {
            rest = after;
            continue;
        };
        // The other attributes live in the same tag; look them up there.
        let tag_start = rest[..position].rfind('<').unwrap_or(0);
        let tag_end = rest[position..]
            .find('>')
            .map(|offset| position + offset)
            .unwrap_or(rest.len());
        let tag = &rest[tag_start..tag_end];
        let slot = index + 1;
        let real = attr_value(tag, "data-spine-real-index")
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(slot);
        let href = attr_value(tag, "data-spine-href");
        if !sections.iter().any(|section| section.slot == slot) {
            sections.push(SpineSection { slot, real, href });
        }
        rest = &rest[tag_end.max(position + NEEDLE.len())..];
        if rest.is_empty() {
            break;
        }
    }
    sections
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
    /// The visual line the box belongs to; a formula's tokens share it.
    line_top: f32,
    math: bool,
}

/// Where a text box sits for text building: the visual line it belongs to and
/// whether it is part of a formula.
#[derive(Clone, Copy)]
struct PageTextLine {
    line_top: f32,
    math: bool,
}

/// Whether `current` continues the visual line `previous` is on. The tokens of
/// one formula are separate boxes on the same line, so their text is joined
/// instead of being split into one line per glyph.
fn continues_same_line(previous: Option<PageTextLine>, current: PageTextLine) -> bool {
    match previous {
        Some(previous) => {
            current.math && previous.math && (current.line_top - previous.line_top).abs() < 1.0
        }
        None => false,
    }
}

/// Joins page text boxes: a newline per visual line, nothing inside a formula.
fn join_text_boxes<'a>(items: impl Iterator<Item = (&'a str, PageTextLine)>) -> String {
    let mut text = String::new();
    let mut previous: Option<PageTextLine> = None;
    for (part, line) in items {
        if part.trim().is_empty() {
            continue;
        }
        if !text.is_empty() && !continues_same_line(previous, line) {
            text.push('\n');
        }
        text.push_str(part);
        previous = Some(line);
    }
    text
}

pub struct Document {
    metadata: DocumentMetadata,
    width: u32,
    height: u32,
    layout_height: u32,
    page_margin: f32,
    margins: PageMargins,
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
    /// Kept behind an `Arc` so the background layout worker can hold it while
    /// the reader keeps using the document.
    sources: Arc<[ChunkSource]>,
    /// Pre-paginated chunks and their own CSS viewport, parallel to `sources`.
    fixed_layouts: Vec<Option<FixedLayout>>,
    base_url: String,
    resources: ResourceSource,
    /// Page-size SVGs and the raster images replaced by a size placeholder.
    /// Painting reads the original pixels through `resources`.
    images: Arc<ImageRaster>,
    cover_image: Option<String>,
    has_non_linear_flows: bool,
    net: Arc<dyn NetProvider>,
    font_context: Arc<FontContext>,
    /// Every chunk's own layout, parallel to `sources`.
    page_table: PageTable,
    /// Family names from the book's `@font-face` rules, in the order they are
    /// declared. The fonts themselves are loaded by Blitz (see
    /// `cre_book::font_faces_in` / `font_faces_in_stylesheets`); this is the
    /// list the reader shows.
    embedded_fonts: Vec<String>,
    /// Whether the book's own fonts may be used
    /// (`crengine.doc.embedded.fonts` in cre.cpp).
    ///
    /// Off strips the `@font-face` rules: from a chunk's HTML before it is
    /// parsed, and from the stylesheets `EpubNet`/`FileNet` serve (a chapter's
    /// `<link rel="stylesheet">` is fetched through them, so the rules are not
    /// in the chunk HTML at all). An atomic because the network provider holds
    /// the same flag and a relayout copies it into the background worker.
    embedded_fonts_flag: Arc<std::sync::atomic::AtomicBool>,
    /// The reader's font size, as `f32::to_bits`, shared with the net
    /// providers: a served stylesheet's absolute `font-size` keywords are
    /// turned into pixels against it, and a relayout updates it in place.
    css_font_size: Arc<std::sync::atomic::AtomicU32>,
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
    /// What the caller asked for: `Some(true)`/`Some(false)` force the lazy or
    /// the full open, `None` decides by book size. The decision is taken in
    /// [`Document::ensure_layout`], because the reader parses on `loadDocument`
    /// and lays out on `renderDocument` (see `cre-lua`).
    lazy_open: Option<bool>,
    active: RefCell<Option<ActiveChunk>>,
    current_page: usize,
    background_color: u32,
    page_image_cache: RefCell<HashMap<usize, PageImage>>,
    page_cache_order: RefCell<Vec<usize>>,
    page_boxes_cache: RefCell<Option<(usize, Vec<SnapshotLine>)>>,
    /// Glyph masks reused across page renders. Cleared when the font context
    /// changes, because the masks carry the face's outlines.
    paint_cache: RefCell<cre_paint::PaintCache>,
    /// Chunks being laid out again after a settings change, in reading order.
    /// Filled by [`Document::start_background_layout`] and installed by
    /// [`Document::drain_background_layouts`] on the reader's thread.
    background: Option<BackgroundLayout>,
    /// Incremented on every new settings change; a worker with an older value
    /// stops at its next chunk, and dropping the document stops it too.
    background_generation: Arc<std::sync::atomic::AtomicU64>,
    /// Layouts installed by the background worker, so `cre-lua` can report a
    /// changed `getPartialRerenderingsCount()` to KOReader.
    background_installs: i64,
}

/// A background layout job: one worker, laying chunks out in reading order.
///
/// Page numbers are cumulative, so the chunks must be done from the first one;
/// installing them in order keeps every page number exact as soon as its own
/// chunk is done. The worker builds its own Blitz documents (the DOM is not
/// `Send`), so the document it belongs to keeps its active chunk untouched.
struct BackgroundLayout {
    total: usize,
    done: usize,
    receiver: std::sync::mpsc::Receiver<(usize, ChunkLayout)>,
}

impl Drop for Document {
    fn drop(&mut self) {
        // A detached worker would otherwise keep laying the book out after the
        // book was closed.
        self.stop_background_layout();
    }
}

impl Document {
    /// Rebuilds the flattened page list, TOC page numbers and page map from the
    /// per-chunk layouts.
    pub(crate) fn rebuild_view(&mut self) {
        let index = self.page_table.rebuild(&self.sources, self.height as f32);
        self.xpointer_pages = index.xpointers;
        self.href_pages = index.anchors;
        self.spine_to_chunk = index.spine_to_chunk;
        self.spine_real_index = index.spine_real_index;
        // First page of the chunk each spine resource lives in. Only used for
        // navigation targets whose chunk has no layout yet (lazy open).
        let href_first_page = index.first_page_by_href;
        let max_flow = index.max_flow;

        let toc = self
            .raw_toc
            .iter()
            .map(|item| {
                let page = page_for_href(&self.href_pages, &item.href)
                    .ok()
                    .or_else(|| provisional_page_for_href(&href_first_page, &item.href));
                let xpointer = page.and_then(|page| self.page_xpointer(page));
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
                let page = page_for_href(&self.href_pages, &item.href)
                    .ok()
                    .or_else(|| provisional_page_for_href(&href_first_page, &item.href))?;
                let xpointer = self.page_xpointer(page);
                let doc_y = self
                    .page_table
                    .pages()
                    .get(page.saturating_sub(1))
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
    ///
    /// Every format qualifies: the layout settings a chunk needs (font size,
    /// page margin) are tokens filled in when the chunk is laid out, see
    /// `cre-css`. The page *size* is different: image normalisation and the
    /// page-size CSS are baked into the chunk HTML, so it needs a reopen.
    pub fn can_relayout_with(&self, options: &DocumentOptions) -> bool {
        self.layout_done
            && !self.sources.is_empty()
            && !self.page_table.is_empty()
            && options.width == self.width
            && options.height == self.height
    }

    /// Number of layouts installed by the background worker.
    pub fn background_installs(&self) -> i64 {
        self.background_installs
    }

    /// Whether the background worker still has chunks to lay out.
    pub fn has_background_layout(&self) -> bool {
        self.background.is_some()
    }

    /// Stops the background worker, if any (a new settings change or a close).
    fn stop_background_layout(&mut self) {
        self.background = None;
        self.background_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Remaps the page indices that move when chunk `chunk` is laid out again and
    /// its page count changes from `old_count` to `new_count`.
    pub(crate) fn rebase_pages(&mut self, chunk: usize, old_count: usize, new_count: usize) {
        let Some(head) = self
            .page_table
            .chunks()
            .get(chunk)
            .map(|meta| meta.first_page)
        else {
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

    /// The chunk and local page the reader is on, which is what survives page
    /// numbers moving under it (installing a chunk before the current one shifts
    /// every number after it).
    pub(crate) fn current_anchor(&self) -> Option<(usize, usize)> {
        let page = self
            .page_table
            .pages()
            .get(self.current_page.saturating_sub(1))?;
        let first = self.page_table.first_page(page.chunk);
        Some((page.chunk, self.current_page.saturating_sub(first)))
    }

    /// Puts the reader back on the `(chunk, local page)` from
    /// [`Self::current_anchor`], wherever that chunk starts now.
    pub(crate) fn restore_anchor(&mut self, anchor: (usize, usize)) {
        let (chunk, local) = anchor;
        let first = self.page_table.first_page(chunk);
        let count = self.page_table.page_count(chunk).max(1);
        self.current_page = first + local.min(count.saturating_sub(1));
    }

    pub fn metadata(&self) -> &DocumentMetadata {
        &self.metadata
    }

    /// Family names the book declares with `@font-face`, in declaration order.
    ///
    /// The fonts are fetched and registered by Blitz while a chunk is parsed;
    /// this is what the reader lists (and what decides whether its "embedded
    /// fonts" setting is offered at all).
    pub fn embedded_font_list(&self) -> &[String] {
        &self.embedded_fonts
    }

    /// Whether the book's own fonts are used (`crengine.doc.embedded.fonts`).
    ///
    /// Turning them off drops the `@font-face` rules — from a chunk's HTML and
    /// from the stylesheets the network provider serves — so a `font-family`
    /// that no longer resolves falls back to the reader's font. Call
    /// [`Self::relayout`] afterwards to apply it to chunks already laid out.
    pub fn set_embedded_fonts(&mut self, enabled: bool) {
        if enabled == self.embedded_fonts_enabled() {
            return;
        }
        self.embedded_fonts_flag
            .store(enabled, std::sync::atomic::Ordering::SeqCst);
        // Dropping the rules keeps the *next* layout from fetching the fonts,
        // but Blitz registers an `@font-face` font in the document's font
        // context, and that registration outlives the chunk it came from. A
        // fresh context makes the fonts already registered unavailable, which is
        // what crengine's "do not use the document's fonts" does (a `font-family`
        // that resolves to nothing falls back to the reader's font).
        //
        // TODO(upstream): `blitz-dom` has no way to disable or unregister the
        // fonts of a document, so this relies on the context being rebuilt.
        // Drop the rebuild once the font registry can be told to forget a
        // document's faces.
        self.rebuild_font_context();
    }

    pub fn embedded_fonts_enabled(&self) -> bool {
        self.embedded_fonts_flag
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn page_count(&self) -> usize {
        self.page_table.pages().len().max(1)
    }

    /// What the document keeps resident, for diagnostics (`perf_probe`).
    ///
    /// The active chunk's DOM/layout is not counted here: it belongs to Blitz
    /// and Parley, and is measured from the process RSS instead.
    pub fn memory_breakdown(&self) -> MemoryBreakdown {
        let mut breakdown = MemoryBreakdown {
            chunk_count: self.sources.len(),
            page_count: self.page_table.pages().len(),
            font_bytes: self
                .font_blobs
                .iter()
                .map(|blob| blob.as_ref().as_ref().len())
                .sum(),
            glyph_cache_bytes: self.paint_cache.borrow().cached_bytes(),
            ..MemoryBreakdown::default()
        };
        for source in self.sources.iter() {
            breakdown.source_bytes += source.text_len;
            breakdown.compressed_source_bytes += source.compressed.len();
        }
        for layout in self.page_table.layouts() {
            breakdown.laid_out_chunks += 1;
            breakdown.line_entries += layout.lines.len();
            breakdown.anchor_entries += layout.href_pages.len();
            breakdown.page_meta_bytes += layout.pages.len() * std::mem::size_of::<LocalPage>();
            for xpointer in &layout.xpointers {
                breakdown.xpointer_bytes += xpointer.as_ref().map(String::len).unwrap_or(0);
            }
        }
        for image in self.page_image_cache.borrow().values() {
            breakdown.page_bitmap_bytes +=
                image.bitmap.width as usize * image.bitmap.height as usize;
        }
        breakdown.placeheld_images = self.images.placeheld_count();
        breakdown.svg_cache_bytes = self.images.cached_bytes();
        breakdown.page_map_source_bytes = self.page_map_source.len();
        breakdown.resource_cache_bytes = self.resources.cache_bytes();
        breakdown.pinned_bytes = self.resources.pinned_bytes();
        if let Some(active) = self.active.borrow().as_ref() {
            breakdown.active_line_boxes = active.lines.len();
            breakdown.active_text_nodes = active.rendered_text_nodes.len();
        }
        breakdown
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
        for snapshot in self.page_table.pages() {
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
        self.page_table
            .pages()
            .get(index)
            .map(|snapshot| render_length_to_i64(snapshot.doc_top))
            .unwrap_or(0)
    }

    pub fn page_start_y_or_invalid(&self, page: usize) -> i64 {
        let Some(index) = page.checked_sub(1) else {
            return -1;
        };
        self.page_table
            .pages()
            .get(index)
            .map(|snapshot| render_length_to_i64(snapshot.doc_top))
            .unwrap_or(-1)
    }

    pub fn page_render_height(&self, page: usize) -> i64 {
        let index = page.saturating_sub(1);
        self.page_table
            .pages()
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

    /// The book's cover, read through the resource table on demand.
    ///
    /// Only the key is kept: a cover is a few megabytes, and every format used
    /// to copy it at open time just to hand it to the reader once.
    pub fn cover_image(&self) -> Option<Vec<u8>> {
        let key = self.cover_image.as_deref()?;
        self.resources.get(key).map(|bytes| (*bytes).to_vec())
    }

    pub fn has_non_linear_flows(&self) -> bool {
        self.has_non_linear_flows
    }

    pub fn page_flow(&self, page: usize) -> i64 {
        let Some(index) = page.checked_sub(1) else {
            return 0;
        };
        self.page_table
            .pages()
            .get(index)
            .map(|snapshot| snapshot.page_flow)
            .unwrap_or(0)
    }

    pub fn resource(&self, path: &str) -> Option<Vec<u8>> {
        let (key, _) = href_key(path);
        let key = key.strip_prefix("binary/").unwrap_or(&key);
        self.resources.get(key).map(|data| (*data).clone())
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
        let snapshot = self
            .page_table
            .pages()
            .get(index)
            .context("page snapshot is missing")?;
        let chunk_index = snapshot.chunk;
        let page_top = snapshot.local_start.top;
        // The image's document rectangle can extend past this page. Only the
        // part that is actually painted here may answer a tap; otherwise a
        // blank strip at the bottom of page N opens the image that starts on
        // page N+1 (KOReader asks for images before words).
        let page_bottom = (snapshot.local_clip_bottom - page_top).max(0.0);

        self.with_active_chunk(chunk_index, |active| {
            let fixed_layout = self.fixed_layouts.get(chunk_index).and_then(|fixed| *fixed);
            // `get_client_bounding_rect` subtracts the document's current
            // viewport scroll. `render_page` leaves it at this page's top, but
            // image hit-testing may also run before the first render, so make
            // the page origin explicit and treat the returned rects as
            // page-local. Subtracting `page_top` again shifted images by one
            // page and made long-press return the next page's image.
            active.doc.set_viewport_scroll(Point {
                x: 0.0,
                y: f64::from(if fixed_layout.is_some() {
                    0.0
                } else {
                    page_top
                }),
            });
            let root = active.doc.root_element();
            // `position` is in rendered (device) pixels, like `TextBox`; the
            // document's layout rectangles are CSS pixels. Without dividing by
            // the viewport scale, a HiDPI view compares device coordinates to
            // CSS rectangles: images from the next page appear to cover text on
            // this page, so long-press opens an image instead of selecting.
            let scale = self.viewport_scale.max(0.1);
            let (position_x, position_y) = if let Some(fixed) = fixed_layout {
                let (fit, offset_x, offset_y) = self.fixed_page_transform(fixed);
                (
                    (position.x / scale - offset_x) / fit,
                    (position.y / scale - offset_y) / fit,
                )
            } else {
                (position.x / scale, position.y / scale)
            };
            let keys = find_image_keys_at(
                &active.doc,
                root,
                page_bottom,
                position_x,
                position_y,
                &self.base_url,
            );
            // A page-fit plate is a generated transparent SVG that stands in
            // for a picture the renderer paints itself, so long-press has to
            // open the picture: handing the wrapper over opened a blank page.
            Ok(keys.into_iter().find_map(|key| {
                let source = self
                    .images
                    .page_fit_image(&key)
                    .map(|fit| fit.source)
                    .unwrap_or(key);
                self.resources.get(&source).map(|data| (*data).to_vec())
            }))
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

    pub(crate) fn chunk_for_spine(&self, spine_index: u32) -> Result<usize> {
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
    // A formula's tokens are separate inline layouts whose tops differ by a few
    // pixels, so sorting on their own top interleaves them (a subscript would
    // read before its base). Fold every token onto the formula line that holds
    // it, then keep that line's document order, which is the order the formula
    // is written in: `Z = lim ... exp( ... )`.
    let formula_lines: Vec<(f32, f32)> = lines
        .iter()
        .filter(|line| line.math_root)
        .map(|line| (line.top, line.bottom))
        .collect();
    if !formula_lines.is_empty() {
        for line in lines.iter_mut().filter(|line| line.math && !line.math_root) {
            let center = (line.box_top + line.box_bottom) / 2.0;
            if let Some((top, _)) = formula_lines
                .iter()
                .find(|(top, bottom)| center >= *top - 1.0 && center <= *bottom + 1.0)
            {
                line.sort_top = *top;
            }
        }
    }
    lines.sort_by(|a, b| a.sort_top.total_cmp(&b.sort_top));
    lines
}

/// `class` attribute of an element.
fn element_class(node: &Node) -> Option<&str> {
    node.element_data()?
        .attrs()
        .iter()
        .find(|attr| attr.name.local.to_string() == "class")
        .map(|attr| attr.value.as_str())
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
            let class = element_class(node);
            let math = class
                .map(|value| {
                    value
                        .split_whitespace()
                        .any(|name| name.starts_with("cre-m"))
                })
                .unwrap_or(false);
            let math_root = class
                .map(|value| value.split_whitespace().any(|name| name == "cre-math"))
                .unwrap_or(false);
            // A math token's own inline layout is as tall as the formula; the
            // glyph box is what a selection should cover.
            let own_top = node.absolute_position(0.0, 0.0).y;
            let own_bottom = own_top + root_layout.size.height;
            let (box_top, box_bottom) = if math && own_bottom > own_top && own_bottom < bottom {
                (own_top.max(top), own_bottom.max(top))
            } else {
                (top, bottom)
            };
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
                box_top,
                box_bottom,
                math,
                math_root,
                sort_top: top,
            });
        }
    }
    if let Some(layout_children) = node.layout_children.borrow().as_ref() {
        for child in layout_children.iter() {
            collect_lines_inner(node.with(*child), out);
        }
    }
}

/// Dev-only stage timer for `layout_chunk`, enabled by `CRE_LAYOUT_PROFILE=1`.
struct StageTimer {
    enabled: bool,
    stages: Vec<(&'static str, f64)>,
    last: std::time::Instant,
}

impl StageTimer {
    fn new() -> Self {
        Self {
            enabled: std::env::var_os("CRE_LAYOUT_PROFILE").is_some(),
            stages: Vec::new(),
            last: std::time::Instant::now(),
        }
    }

    fn mark(&mut self, stage: &'static str) {
        if !self.enabled {
            return;
        }
        let now = std::time::Instant::now();
        self.stages
            .push((stage, now.duration_since(self.last).as_secs_f64() * 1000.0));
        self.last = now;
    }

    fn report(&self, chunk: usize) {
        if !self.enabled {
            return;
        }
        let mut line = format!("profile chunk {chunk}:");
        for (stage, ms) in &self.stages {
            line.push_str(&format!(" {stage}={ms:.1}"));
        }
        eprintln!("{line}");
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
    // Where the pending forced break starts the page. It is the element's own
    // top, not the first line's: a section that opens with a top margin (a
    // chapter title pushed 40% down, say) keeps that margin on the new page
    // instead of having it trimmed away.
    let mut pending_break_at: Option<f32> = None;
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
            // A spine item can start with an image instead of text (image-only
            // chapters, pre-paginated manga pages). Forced page breaks were
            // only consumed in the text-line branch, so the break at such a
            // section was ignored and two spine items shared a page. Consume
            // the break here too, before deciding whether the image fits.
            while forced_index < forced_breaks.len()
                && forced_breaks[forced_index] <= box_.top + BREAK_EPSILON
            {
                if forced_breaks[forced_index] > starts.last().unwrap().clip_top + BREAK_EPSILON
                    && box_.top > starts.last().unwrap().clip_top + BREAK_EPSILON
                    && page_has_content
                {
                    let clip_top = forced_breaks[forced_index].min(box_.top);
                    starts.push(PageStart {
                        top: (clip_top - margin_top).max(0.0),
                        clip_top,
                    });
                    page_has_content = false;
                }
                forced_index += 1;
            }
            let current_clip_top = starts.last().unwrap().clip_top;
            if pending_break && page_has_content && box_.top > current_clip_top + BREAK_EPSILON {
                starts.push(PageStart {
                    top: (box_.top - margin_top).max(0.0),
                    clip_top: box_.top,
                });
            }
            pending_break = false;
            let current_clip_top = starts.last().unwrap().clip_top;
            let clip_bottom = starts.last().unwrap().top + page_height - margin_bottom;
            let height = box_.bottom - box_.top;
            // Keep an image whole: start a new page when it does not fit in
            // the space left, unless it is taller than a page even on its own
            // (then it has to be clipped, which the CSS max-height prevents).
            if box_.bottom > clip_bottom
                && box_.top > current_clip_top
                && height <= page_content_height
            {
                if page_has_content {
                    starts.push(PageStart {
                        top: (box_.top - margin_top).max(0.0),
                        clip_top: box_.top,
                    });
                } else {
                    // Only blank space (a leading margin or padding) is above
                    // the picture, so moving it down would leave an empty page:
                    // start this page at the picture instead, which is what a
                    // browser does when a page begins with a full-page image.
                    let start = starts.last_mut().unwrap();
                    start.clip_top = box_.top;
                    start.top = (box_.top - margin_top).max(0.0);
                }
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
                pending_break_at = Some(forced_breaks[forced_index]);
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
                starts.push(page_start_for_break(
                    line,
                    pending_break_at.take(),
                    margin_top,
                ));
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

/// Page start for a forced break: the break's own position when it is above
/// the line (so a leading margin stays visible), never below the line.
fn page_start_for_break(line: &LineBox, at: Option<f32>, margin_top: f32) -> PageStart {
    let clip_top = at.unwrap_or(line.visual_top).min(line.visual_top);
    PageStart {
        top: (clip_top - margin_top).max(0.0),
        clip_top,
    }
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

fn find_image_keys_at(
    doc: &HtmlDocument,
    node: &Node,
    page_bottom: f32,
    x: f32,
    y: f32,
    base_url: &str,
) -> Vec<String> {
    if node
        .element_data()
        .is_some_and(|element| element.name.local.to_string() == "img")
    {
        let mut rects = doc.node_client_rects(node.id);
        if rects.is_empty() {
            rects.extend(doc.get_client_bounding_rect(node.id));
        }
        let hit = rects.iter().any(|rect| {
            let local_top = rect.y as f32;
            let visible_top = local_top.max(0.0);
            let visible_bottom = (local_top + rect.height as f32).min(page_bottom);
            visible_bottom > visible_top
                && x >= rect.x as f32
                && x <= (rect.x + rect.width) as f32
                && y >= visible_top
                && y <= visible_bottom
        });
        if hit && let Some(src) = node_attr(node, "src") {
            return image_key_candidates(src, base_url);
        }
    }

    for child in node.children.iter() {
        let keys = find_image_keys_at(doc, node.with(*child), page_bottom, x, y, base_url);
        if !keys.is_empty() {
            return keys;
        }
    }
    Vec::new()
}

/// Every key a `<img src>` may name, most specific first.
///
/// A resource key is what the net provider answers to, and it is not always the
/// URL a document wrote: an EPUB serves `cre://book/<key>`, a standalone
/// document serves a path relative to its own directory, and a generated plate
/// (`cre-page-fit-N`, no scheme at all) is a bare key. The caller tries them in
/// order and takes the first that holds bytes.
fn image_key_candidates(src: &str, base_url: &str) -> Vec<String> {
    let src = src.trim();
    let mut keys = Vec::new();
    if let Some(relative) = src.strip_prefix("cre://book/") {
        keys.push(normalize_path(
            &percent_decode_str(relative).decode_utf8_lossy(),
        ));
    } else if src.contains("://") {
        let (key, _) = href_key(src);
        keys.push(key.strip_prefix("binary/").unwrap_or(&key).to_string());
    } else {
        // The bare name, as the generated table and `MapNet` address it.
        keys.push(normalize_path(src));
        // And the same name resolved against the document, for a real relative
        // path (`../Images/cover.jpg`).
        let resolved = if src.starts_with('/') {
            format!("{}{}", base_url.trim_end_matches('/'), src)
        } else {
            format!("{base_url}{src}")
        };
        let (key, _) = href_key(&resolved);
        let key = key.strip_prefix("binary/").unwrap_or(&key).to_string();
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// Visible device-pixel area of the images the renderer paints itself.
///
/// `paint_page` reports how much of the page an image covers, and KOReader uses
/// that to dither image pages and to request a full refresh. A placeholder is
/// not in the scene, so its area has to be added back.
fn placed_image_area(images: &[cre_paint::PlacedImage], width: u32, height: u32) -> usize {
    let mut area = 0.0f64;
    for image in images {
        let left = f64::from(image.x).max(0.0);
        let top = f64::from(image.y).max(0.0);
        let right = (f64::from(image.x) + f64::from(image.width)).min(f64::from(width));
        let bottom = (f64::from(image.y) + f64::from(image.height)).min(f64::from(height));
        if right > left && bottom > top {
            area += (right - left) * (bottom - top);
        }
    }
    area.ceil() as usize
}

/// Collects the images a page carries only as a size placeholder, with the
/// page-local device rectangle each one occupies.
///
/// The placeholder gave layout the image's exact intrinsic size, so the
/// element box here is the box the real image must fill. The original bytes are
/// read from `resources` (the same data `getImageDataFromPosition` returns).
#[allow(clippy::too_many_arguments)]
fn collect_placed_images(
    doc: &HtmlDocument,
    node: &Node,
    page_bottom: f32,
    scale: f32,
    offset_x: f32,
    offset_y: f32,
    images: &ImageRaster,
    resources: &ResourceSource,
    base_url: &str,
    out: &mut Vec<cre_paint::PlacedImage>,
) {
    if node
        .element_data()
        .is_some_and(|element| element.name.local.to_string() == "img")
        && let Some(src) = node_attr(node, "src")
    {
        let resolved = if src.contains("://") {
            src.to_string()
        } else if src.starts_with('/') {
            format!("{}{}", base_url.trim_end_matches('/'), src)
        } else {
            format!("{base_url}{src}")
        };
        let (key, _) = href_key(&resolved);
        let key = key.strip_prefix("binary/").unwrap_or(&key);
        let key = resources.key_for(key);
        // A plain oversized image is a size placeholder (painted with `fill`);
        // a generated page-fit wrapper is a transparent page-sized SVG whose
        // real picture is painted inside it with `contain`.
        let placed = images
            .placeheld_size(&key)
            .and_then(|(width, height)| {
                resources
                    .get(&key)
                    .map(|bytes| (bytes, width, height, cre_paint::ImageFit::Fill))
            })
            .or_else(|| {
                // A generated key is flat (`cre-page-fit-N`), while a standalone
                // document resolves its `src` against the document's directory,
                // so the key arrives as a full file path there.
                let fit = images.page_fit_image(&key).or_else(|| {
                    key.rsplit('/')
                        .next()
                        .and_then(|name| images.page_fit_image(name))
                })?;
                let bytes = resources.get(&fit.source)?;
                Some((
                    bytes,
                    fit.width,
                    fit.height,
                    if fit.contain {
                        cre_paint::ImageFit::Contain
                    } else {
                        cre_paint::ImageFit::Fill
                    },
                ))
            });
        if let Some((bytes, intrinsic_width, intrinsic_height, fit)) = placed {
            for rect in doc.node_client_rects(node.id) {
                let top = rect.y as f32;
                let bottom = top + rect.height as f32;
                if bottom <= 0.0 || top >= page_bottom || rect.width <= 0.0 || rect.height <= 0.0 {
                    continue;
                }
                out.push(cre_paint::PlacedImage {
                    x: rect.x as f32 * scale + offset_x,
                    y: top * scale + offset_y,
                    width: rect.width as f32 * scale,
                    height: rect.height as f32 * scale,
                    intrinsic_width,
                    intrinsic_height,
                    fit,
                    bytes: bytes.clone(),
                });
            }
        }
    }
    for child in node.children.iter() {
        collect_placed_images(
            doc,
            node.with(*child),
            page_bottom,
            scale,
            offset_x,
            offset_y,
            images,
            resources,
            base_url,
            out,
        );
    }
}

fn epub_cover_image(book: &EpubBook) -> Option<String> {
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
    Some(normalize_path(&item.href))
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
            // A layout clones the context once per chunk; sharing the inner
            // collection turns that into an Arc bump instead of copying the
            // registry of every registered face.
            shared: true,
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

fn column_declaration_regex() -> &'static regex::Regex {
    static REGEX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    REGEX.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(?:-webkit-)?column-(?:count|width)\s*:\s*[^;{}]*;?|(?i)(?:-webkit-)?columns\s*:\s*[^;{}]*;?",
        )
        .expect("column declaration regex is valid")
    })
}

/// ASCII case-insensitive substring search, for quick "can this rule match at
/// all" checks.
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    if needle.is_empty() {
        return true;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

fn strip_multi_column_declarations(css: &str) -> String {
    if !contains_ignore_ascii_case(css, "column") {
        return css.to_string();
    }
    column_declaration_regex().replace_all(css, "").into_owned()
}

/// Turns `<font size="N">` into an inline `font-size`.
///
/// `<font>` is a legacy element whose `size` attribute maps to one of the
/// absolute CSS sizes (1 → x-small … 7 → xxx-large, with `+1`/`-1` counted from
/// 3). Blitz's presentational-hint pass handles `align`, `bgcolor`, `border`,
/// `width`/`height` and `hspace`/`vspace` but not the `font` element, so MOBI
/// and KF8 titles marked up with `<font size="6">` came out at body size.
///
/// TODO(upstream): delete this once blitz-dom maps the `font` element's `size`
/// (and `color`/`face`) to the corresponding properties.
fn font_size_attributes_become_styles(html: &str, base: f32) -> String {
    if !contains_ignore_ascii_case(html, "<font") {
        return html.to_string();
    }
    static TAG: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static SIZE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static STYLE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let tag = TAG.get_or_init(|| {
        regex::Regex::new(r"(?is)<font\b([^>]*)>").expect("font tag regex is valid")
    });
    let size = SIZE.get_or_init(|| {
        regex::Regex::new(r#"(?is)\bsize\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
            .expect("font size regex is valid")
    });
    let style = STYLE.get_or_init(|| {
        regex::Regex::new(r#"(?is)\bstyle\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
            .expect("style attribute regex is valid")
    });
    tag.replace_all(html, |captures: &regex::Captures<'_>| {
        let attrs = &captures[1];
        let Some(size_match) = size.captures(attrs) else {
            return captures[0].to_string();
        };
        let raw = size_match
            .get(1)
            .or_else(|| size_match.get(2))
            .map_or("", |value| value.as_str());
        let Some(level) = font_size_level(raw) else {
            return captures[0].to_string();
        };
        let Some(pixels) = css_font_size_for_level(level, base) else {
            return captures[0].to_string();
        };
        // An author declaration wins over the attribute, like it does in a
        // browser.
        let whole = size_match.get(0).expect("the whole size attribute");
        let mut rest = String::with_capacity(attrs.len());
        rest.push_str(attrs[..whole.start()].trim_end());
        rest.push_str(&attrs[whole.end()..]);
        if contains_ignore_ascii_case(&rest, "font-size") {
            return captures[0].to_string();
        }
        let rebuilt = match style.captures(&rest) {
            Some(existing) => {
                let whole = existing.get(0).expect("the whole style attribute");
                let value = existing
                    .get(1)
                    .or_else(|| existing.get(2))
                    .map_or("", |value| value.as_str());
                let separator = if value.trim().is_empty() || value.trim_end().ends_with(';') {
                    ""
                } else {
                    ";"
                };
                format!(
                    "{}style=\"{}font-size:{pixels}px{}\"{}",
                    &rest[..whole.start()],
                    value,
                    separator,
                    &rest[whole.end()..]
                )
            }
            None => format!("{rest} style=\"font-size:{pixels}px\""),
        };
        format!("<font{rebuilt}>")
    })
    .into_owned()
}

/// The HTML `<font size>` level: absolute 1..7, or `+1`/`-2` from the default
/// of 3.
fn font_size_level(raw: &str) -> Option<i32> {
    let raw = raw.trim();
    let (base, value) = match raw.strip_prefix('+') {
        Some(rest) => (3, rest.parse::<i32>().ok()?),
        None => match raw.strip_prefix('-') {
            Some(rest) => (3, -rest.parse::<i32>().ok()?),
            None => (0, raw.parse::<i32>().ok()?),
        },
    };
    Some(if base == 0 { value } else { base + value }.clamp(1, 7))
}

/// The scale browsers apply to the absolute sizes, xx-small to xxx-large.
const FONT_SIZE_FACTORS: [f32; 8] = [0.6, 0.75, 0.89, 1.0, 1.2, 1.5, 2.0, 3.0];

/// Pixel size for an absolute size level (1 = x-small ... 7 = xxx-large),
/// rounded up.
///
/// The factor applies to the *medium* size, which is the reader's font size
/// here so headings scale with the setting; only the pixels are rounded, which
/// keeps every step distinct (16px: 10, 12, 15, 16, 20, 24, 32, 48).
fn css_font_size_for_level(level: i32, base: f32) -> Option<u32> {
    if !(1..=7).contains(&level) {
        return None;
    }
    // Level 1 is x-small, the second factor; HTML's `size` never means
    // xx-small, so the levels line up with the factors one to one.
    let factor = FONT_SIZE_FACTORS[level as usize];
    Some((base.max(1.0) * factor).ceil() as u32)
}

/// Pixel size for a CSS absolute-size keyword, or `None` for anything else.
fn css_font_size_keyword_px(keyword: &str, base: f32) -> Option<u32> {
    let index = match keyword.to_ascii_lowercase().as_str() {
        "xx-small" => 0,
        "x-small" => 1,
        "small" => 2,
        "medium" => 3,
        "large" => 4,
        "x-large" => 5,
        "xx-large" => 6,
        "xxx-large" => 7,
        _ => return None,
    };
    Some((base.max(1.0) * FONT_SIZE_FACTORS[index]).ceil() as u32)
}

/// Rewrites the absolute `font-size` keywords in a document's CSS to the pixel
/// sizes browsers compute.
///
/// The keywords are absolute: Chrome applies the scale to the *medium* size and
/// ignores the parent (a page whose body is 100px still renders
/// `font-size: large` at 19px). stylo does the same but against a fixed 16px
/// default, while the reader's font size moves with the setting, so a book's
/// `large` heading did not change size with it. Compute the same table from the
/// reader size — 0.6, 0.75, 0.89, 1, 1.2, 1.5, 2, 3, rounded up to whole
/// pixels so the steps stay distinct.
///
/// TODO(upstream): drop this if stylo ever takes the medium size from the
/// document instead of its built-in default.
fn absolute_font_size_keywords_become_px(html: &str, base: f32) -> String {
    if !contains_ignore_ascii_case(html, "font-size") {
        return html.to_string();
    }
    static SIZES: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let sizes = SIZES.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(font-size\s*:\s*)(xx-small|x-small|small|medium|large|x-large|xx-large|xxx-large)\b",
        )
        .expect("font-size keyword regex is valid")
    });
    sizes
        .replace_all(html, |captures: &regex::Captures<'_>| {
            let Some(pixels) = css_font_size_keyword_px(&captures[2], base) else {
                return captures[0].to_string();
            };
            format!("{}{pixels}px", &captures[1])
        })
        .into_owned()
}

/// Blitz only paints borders, padding and background on boxes it builds for
/// block-level and atomic inline elements; a bordered `<span>` gets nothing, so
/// `border: thin dotted black` on an inline element is invisible (and takes no
/// space). Turn such elements into inline blocks, which does paint the border
/// and reserve its width, like a browser does for the same markup.
fn inline_borders_become_blocks(html: &str) -> String {
    if !contains_ignore_ascii_case(html, "style") || !contains_ignore_ascii_case(html, "border") {
        return html.to_string();
    }
    static TAG: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static STYLE_ATTR: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let tag = TAG.get_or_init(|| {
        regex::Regex::new(r"(?is)<([a-zA-Z][a-zA-Z0-9]*)\b([^>]*)>").expect("tag regex is valid")
    });
    let style_attr = STYLE_ATTR.get_or_init(|| {
        regex::Regex::new(r#"(?is)\bstyle\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
            .expect("style attribute regex is valid")
    });
    tag.replace_all(html, |captures: &regex::Captures<'_>| {
        let name = captures[1].to_ascii_lowercase();
        if !matches!(
            name.as_str(),
            "span"
                | "a"
                | "em"
                | "strong"
                | "b"
                | "i"
                | "cite"
                | "code"
                | "abbr"
                | "dfn"
                | "var"
                | "kbd"
                | "samp"
                | "q"
                | "small"
                | "big"
                | "sub"
                | "sup"
                | "u"
                | "s"
                | "mark"
                | "time"
                | "label"
        ) {
            return captures[0].to_string();
        }
        let attrs = &captures[2];
        let Some(style) = style_attr.captures(attrs) else {
            return captures[0].to_string();
        };
        let value = style
            .get(1)
            .or_else(|| style.get(2))
            .map_or("", |m| m.as_str());
        let lower = value.to_ascii_lowercase();
        if !has_border_declaration(&lower) || lower.contains("display") {
            return captures[0].to_string();
        }
        // Rebuild the tag with `display:inline-block` added to its style value.
        // Capture offsets are absolute in the haystack, but the style capture was
        // taken over the attribute text, so shift it by where the attributes start
        // inside the tag (`<` + name).
        let whole_style = style.get(0).unwrap();
        let attrs_off = 1 + captures[1].len();
        let start = attrs_off + whole_style.start();
        let end = attrs_off + whole_style.end();
        let mut out = String::with_capacity(captures[0].len() + 28);
        out.push_str(&captures[0][..start]);
        out.push_str(&format!(
            "style=\"display:inline-block;line-height:inherit;{value}\""
        ));
        out.push_str(&captures[0][end..]);
        out
    })
    .into_owned()
}

/// True when a lower-cased `style` value sets a border on any side.
fn has_border_declaration(style: &str) -> bool {
    style.split(';').any(|declaration| {
        let name = declaration
            .split(':')
            .next()
            .unwrap_or("")
            .trim()
            .trim_start_matches("-webkit-")
            .trim_start_matches("-moz-");
        name == "border" || name.starts_with("border-")
    })
}

/// Blitz's automatic table layout treats a cell's `width`/`height` as exact
/// width, while browsers treat it as a minimum and let the column grow to fit the
/// cell's content. Books that set a small width on every cell (`td { width: 1em }`
/// is common for character tables) therefore collapse their cells onto each other
/// and the text overlaps. Rewriting the declaration to `min-width` gives Blitz the
/// browser behaviour: the declared width stays a floor, the content sets the size.
fn widen_table_cell_widths(html: &str) -> String {
    if !contains_ignore_ascii_case(html, "<style") {
        return html.to_string();
    }
    static STYLE_BLOCK: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let style_block = STYLE_BLOCK.get_or_init(|| {
        regex::Regex::new(r"(?is)(<style\b[^>]*>)(.*?)(</style\s*>)").expect("style regex is valid")
    });
    style_block
        .replace_all(html, |captures: &regex::Captures<'_>| {
            format!(
                "{}{}{}",
                &captures[1],
                widen_cell_dimensions_in_stylesheet(&captures[2]),
                &captures[3]
            )
        })
        .into_owned()
}

/// True when the rule's subject is a table cell (`td`/`th`).
fn selector_targets_cell(selector: &str) -> bool {
    selector.split(',').any(|part| {
        let last = part.split_whitespace().last().unwrap_or("");
        let last = last.trim_start_matches(['>', '+', '~']);
        let name = last
            .split(['.', '#', ':', '['])
            .next()
            .unwrap_or("")
            .trim_end_matches(')');
        name.eq_ignore_ascii_case("td") || name.eq_ignore_ascii_case("th")
    })
}

fn widen_cell_dimensions_in_stylesheet(css: &str) -> String {
    if !contains_ignore_ascii_case(css, "width") && !contains_ignore_ascii_case(css, "height") {
        return css.to_string();
    }
    // `table-layout: fixed` really does use the declared widths; leave it alone.
    if contains_ignore_ascii_case(css, "table-layout")
        && css.to_ascii_lowercase().contains("table-layout: fixed")
    {
        return css.to_string();
    }
    static RULE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static WIDTH: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static HEIGHT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let rule = RULE.get_or_init(|| {
        regex::Regex::new(r"(?is)([^{}]+)\{([^{}]*)\}").expect("CSS rule regex is valid")
    });
    let width = WIDTH.get_or_init(|| {
        regex::Regex::new(r"(?i)(^|;)(\s*)width(\s*):").expect("CSS width regex is valid")
    });
    let height = HEIGHT.get_or_init(|| {
        regex::Regex::new(r"(?i)(^|;)(\s*)height(\s*):").expect("CSS height regex is valid")
    });
    rule.replace_all(css, |captures: &regex::Captures<'_>| {
        let declarations = &captures[2];
        if !selector_targets_cell(&captures[1]) {
            return captures[0].to_string();
        }
        // A table cell's width and height are minima in CSS 2.1; Blitz treats a
        // declared height as the row's exact height, so `td { height: 1em }`
        // collapsed every row of the entity table and the cells painted over
        // the content below. `min-width`/`min-height` keep the floor and let the
        // row/cell grow to its content.
        let mut rewritten = declarations.to_string();
        if width.is_match(&rewritten) {
            rewritten = width
                .replace_all(&rewritten, "${1}${2}min-width${3}:")
                .into_owned();
        }
        if height.is_match(&rewritten) {
            rewritten = height
                .replace_all(&rewritten, "${1}${2}min-height${3}:")
                .into_owned();
        }
        format!("{}{{{}}}", &captures[1], rewritten)
    })
    .into_owned()
}

/// Blitz ignores `<caption>` (the table layout logs and skips it), so the title
/// of a table disappears. Move a leading caption in front of the table as a
/// centred block, which is where the reader would draw it.
fn move_table_captions(html: &str) -> String {
    if !contains_ignore_ascii_case(html, "<caption") {
        return html.to_string();
    }
    static CAPTION: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let caption = CAPTION.get_or_init(|| {
        regex::Regex::new(r"(?is)(<table\b[^>]*>)(\s*)<caption\b[^>]*>(.*?)</caption\s*>")
            .expect("caption regex is valid")
    });
    caption
        .replace_all(html, |captures: &regex::Captures<'_>| {
            format!(
                r#"<div class="cre-caption" style="display:block;text-align:center;">{}</div>{}"#,
                &captures[3], &captures[1]
            )
        })
        .into_owned()
}

fn strip_multi_column_styles(html: &str) -> String {
    // These regexes run over every chunk's HTML; a chunk without a
    // `column-*`/`columns` declaration cannot match and is skipped entirely.
    if !contains_ignore_ascii_case(html, "column") {
        return html.to_string();
    }
    static STYLE_BLOCK: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static STYLE_ATTR: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let style_block = STYLE_BLOCK.get_or_init(|| {
        regex::Regex::new(r"(?is)(<style\b[^>]*>)(.*?)(</style\s*>)").expect("style regex is valid")
    });
    let html = style_block
        .replace_all(html, |captures: &regex::Captures<'_>| {
            format!(
                "{}{}{}",
                &captures[1],
                strip_multi_column_declarations(&captures[2]),
                &captures[3]
            )
        })
        .into_owned();
    let style_attr = STYLE_ATTR.get_or_init(|| {
        regex::Regex::new(r#"(?is)(\sstyle\s*=\s*)(?:"([^"]*)"|'([^']*)')"#)
            .expect("style attribute regex is valid")
    });
    style_attr
        .replace_all(&html, |captures: &regex::Captures<'_>| {
            if let Some(value) = captures.get(2) {
                format!(
                    "{}\"{}\"",
                    &captures[1],
                    strip_multi_column_declarations(value.as_str())
                )
            } else {
                format!(
                    "{}'{}'",
                    &captures[1],
                    strip_multi_column_declarations(captures.get(3).map_or("", |m| m.as_str()))
                )
            }
        })
        .into_owned()
}

/// block's layout text.
fn parse_fixed_layout(html: &str) -> Option<FixedLayout> {
    const MARKER: &str = "data-cre-fixed-layout=\"";
    let start = html.find(MARKER)? + MARKER.len();
    let end = html[start..].find('"')?;
    let value = &html[start..start + end];
    if value.eq_ignore_ascii_case("device") {
        return Some(FixedLayout::Device);
    }
    let (width, height) = value.split_once('x')?;
    let width = width.parse::<f32>().ok()?;
    let height = height.parse::<f32>().ok()?;
    (width > 0.0 && height > 0.0).then_some(FixedLayout::Pixel { width, height })
}

/// Inserts a `<style>` block into an HTML document: before `</head>` when the
/// document has one, else before `</body>`, else at the end.
///
/// Appending after `</html>` (which is what a bare `push_str` does) makes the
/// HTML parser drop the block, so standalone documents lost the base typography
/// and the MathML styles while generated chunk HTML - built with a head of our
/// own - did not.
fn insert_style(html: &mut String, css: &str) {
    let block = format!("<style>{css}</style>");
    let lower = html.to_ascii_lowercase();
    let at = lower
        .rfind("</head>")
        .or_else(|| lower.rfind("</body>"))
        .unwrap_or(html.len());
    html.insert_str(at, &block);
}

fn create_document(
    html: &str,
    base_url: &str,
    net: Arc<dyn NetProvider>,
    font_context: Arc<FontContext>,
    font_datas: &[FontBlob],
    options: &DocumentOptions,
    margins: PageMargins,
) -> HtmlDocument {
    let profile = std::env::var_os("CRE_LAYOUT_PROFILE").is_some();
    let started = std::time::Instant::now();
    let config = DocumentConfig {
        // The Blitz viewport is the reader's page, not the chunk. Its height
        // decides what `vh`/`vmin`/`vmax` mean and what `height: 100%` resolves
        // against, so it has to be the page: with the chunk's height here, a
        // page-level section styled `min-height: 100vh` became as tall as the
        // whole chunk and `background-size: cover` zoomed its picture ~125x,
        // leaving one corner of it on the page. The document is not clipped to
        // the viewport - pagination scrolls through it - so a page-sized
        // viewport only fixes the units. `layout_height` used to be passed here
        // and did exactly that damage.
        viewport: Some(Viewport::new(
            options.width,
            options.height,
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
    let preprocessed_at = started.elapsed().as_secs_f64() * 1000.0;
    let base_html = if options.embedded_styles_enabled {
        html
    } else {
        stripped.as_str()
    };
    // crengine does not implement CSS multi-column layout: column-count and
    // column-width are ignored and text stays in one column. Blitz parses them
    // but lays the container out badly (the following image was pulled above
    // the overflowing text in Juliet's home page). Remove the declarations
    // before layout so both engines behave the same.
    let html_source = strip_multi_column_styles(base_html);
    let html_source = widen_table_cell_widths(&html_source);
    let html_source = inline_borders_become_blocks(&html_source);
    let mut html_source = move_table_captions(&html_source);
    // Ruby is not specific to EPUB: KF8/MOBI documents carry the same `<ruby>`
    // markup, and Blitz has no ruby box. Stack the note over its base here, in
    // the one place every format passes through, rather than in the EPUB
    // generator as well.
    html_source = cre_book::rewrite_ruby(&html_source);
    html_source = font_size_attributes_become_styles(&html_source, options.font_size);
    html_source = absolute_font_size_keywords_become_px(&html_source, options.font_size);
    let stripped_at = started.elapsed().as_secs_f64() * 1000.0;
    let mut math_metrics = None;
    if html_source.to_ascii_lowercase().contains("<math") {
        // Prefer a loaded font with an OpenType MATH table for math content,
        // the way Chromium's `math { font-family: math }` picks the platform
        // math font. KOReader ships FreeSerif (which has a MATH table) in its
        // fallback list; without one the reader font stays in use.
        let math_font = cre_mathml::MathFont::from_font_datas(
            font_datas.iter().map(|blob| blob.as_ref().as_ref()),
        );
        html_source = cre_mathml::rewrite_mathml_with(&html_source, math_font.as_ref());
        insert_style(&mut html_source, &cre_mathml::css(math_font.as_ref()));
        math_metrics = math_font.as_ref().map(|font| font.metrics);
    }
    if let Some(stylesheet) = options
        .stylesheet
        .as_deref()
        .filter(|value| !value.is_empty())
        && !stylesheet.contains("</style")
    {
        insert_style(
            &mut html_source,
            &strip_multi_column_declarations(stylesheet),
        );
    }

    let scale = options.viewport_scale.max(0.1);
    let margin_left_css = (margins.left / scale).max(0.0);
    let margin_right_css = (margins.right / scale).max(0.0);
    let margin_bottom_css = (margins.bottom / scale).max(0.0);
    // Each format bakes one padding (`margin_x`/`margin_y`) into the body rule
    // it generates, because chunk HTML is built before the reader's settings are
    // known. A reader that sets the margins itself replaces that padding: left
    // and right with its own values, top and bottom with zero, since pagination
    // already places the text inside the top and bottom margins.
    if margins.is_explicit() {
        html_source.push_str(&format!(
            "<style>/* cre-page-margins */ body {{ padding: 0 {margin_right_css:.3}px 0 \
             {margin_left_css:.3}px; }}</style>"
        ));
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
    let max_image_height = (page_height_css - page_margin_css - margin_bottom_css).max(1.0);

    // A section that carries the book's `<body>` styling is a page of its own:
    // its background (a KF8 title page's engraving, an EPUB cover's `body.cov`)
    // covers the whole page in crengine and in a browser, where a body
    // background propagates to the canvas. The height is the page's, in pixels
    // rather than `100vh`, so it does not depend on what a document-relative
    // viewport unit resolves to inside the layout engine.
    let page_section_height = (page_height_css - margin_bottom_css).max(1.0);
    html_source.push_str(&format!(
        "<style>.cre-page-body {{ min-height: {page_section_height:.0}px; }}</style>"
    ));

    // A page that holds nothing but a picture is laid out through a generated
    // SVG wrapper (`cre-page-fit-...`) that the painter fills with the picture.
    // Give it the reader's whole text area - the box a fixed-layout page gets -
    // so a cover fills the page instead of sitting inside the generator's
    // 40/32 padding. Only when the reader set the margins: then the body has no
    // padding of its own, so a percentage width is the page's.
    if margins.is_explicit() {
        let margin_top_css = (margins.top / scale).max(0.0);
        let picture_height = (page_height_css - margin_top_css - margin_bottom_css).max(1.0);
        let rule = format!(
            "<style>img.cre-page-picture {{ display: block !important; \
             width: 100% !important; height: {picture_height:.0}px !important; \
             margin: {margin_top_css:.0}px 0 0 0 !important; \
             max-width: none !important; max-height: none !important; }}</style>"
        );
        html_source.push_str(&rule);
    }
    html_source.push_str(&format!(
        "<style>img, svg, video, canvas, object, embed {{ max-width: 100%; \
         max-height: {max_image_height:.0}px; height: auto; }}</style>"
    ));
    if parse_fixed_layout(&html_source).is_none() {
        // Blitz applies fixed body dimensions and overflow from author CSS even
        // in reflow mode. A reflowable spine item must use the reader viewport,
        // not the fixed-layout template's width/height. This is especially
        // visible in EPUBs whose fixed and reflowable spine items share a
        // stylesheet (`body { width: 1200px; height: 600px; overflow: hidden }`).
        html_source.push_str(
            "<style>html, body { width: auto !important; height: auto !important; \
             min-width: 0 !important; min-height: 0 !important; \
             overflow: visible !important; }</style>",
        );
    }
    // An empty heading still reserves a full line box. Converters leave these
    // behind (the first page of zyj.epub is `<h1></h1>` followed by a
    // full-page image), which produced a blank first page. Only headings are
    // hidden: an empty <p> can be layout-critical (for example
    // `<p class="cb"></p>` clearing floats on a title page).
    html_source.push_str(
        "<style>h1:empty, h2:empty, h3:empty, h4:empty, h5:empty, h6:empty \
         { display: none !important; }</style>",
    );
    // The generators leave the layout values as tokens so the chunk HTML (and
    // the chunk boundaries that page numbers derive from) does not change with
    // them; the current settings are applied here instead. That is what lets a
    // font or margin change relayout the existing chunks instead of reopening
    // the book, for every format.
    let page_margin_css = options.page_margin / options.viewport_scale.max(0.1);
    let html_source =
        match cre_book::css::substitute(&html_source, options.font_size, page_margin_css) {
            std::borrow::Cow::Borrowed(_) => html_source,
            std::borrow::Cow::Owned(substituted) => substituted,
        };
    let from_html_at = started.elapsed().as_secs_f64() * 1000.0;
    let mut doc = HtmlDocument::from_html(html_source.as_str(), config);
    let parsed_at = started.elapsed().as_secs_f64() * 1000.0;
    doc.resolve(0.0);
    let first_resolve_at = started.elapsed().as_secs_f64() * 1000.0;
    for _ in 0..4 {
        doc.handle_messages();
        doc.resolve(0.0);
    }
    // Stretched MathML fences were sized from a model of the generated CSS.
    // The content they wrap is in the DOM now, so measure it and replace the
    // estimate with the height the layout actually gave it.
    if let Some(metrics) = math_metrics {
        fit_stretched_fences(&mut doc, metrics);
    }
    if profile {
        eprintln!(
            "  create_document: preprocess={:.2}ms strip={:.2}ms setup={:.2}ms from_html={:.2}ms \
             resolve1={:.2}ms loop={:.2}ms",
            preprocessed_at,
            stripped_at - preprocessed_at,
            from_html_at - stripped_at,
            parsed_at - from_html_at,
            first_resolve_at - parsed_at,
            started.elapsed().as_secs_f64() * 1000.0 - first_resolve_at
        );
    }
    doc
}

/// Sizes stretched MathML fences from the layout instead of an estimate.
///
/// `cre-mathml` emits each stretched fence as `<span class="cre-mo cre-fence"
/// data-cre-fence="{glyph}" data-cre-fence-box="{estimate}">` and wraps the
/// content it encloses in `<span class="cre-fence-body">`. The estimate is what
/// the first layout used; here the content's real box is read back and the
/// glyph is restyled to that height, centred on it. The element keeps the
/// estimated box height (changing it moved a page in `juliet.epub`), so this
/// only corrects what is painted. Before this the brace was off by up to a
/// fifth of its length, and sat low, on tables with scripts in a corner row.
fn fit_stretched_fences(doc: &mut HtmlDocument, metrics: cre_mathml::MathMetrics) -> bool {
    let Ok(fences) = doc.query_selector_all("[data-cre-fence]") else {
        return false;
    };
    if fences.is_empty() {
        return false;
    }
    let glyph_attr = blitz_dom::LocalName::from("data-cre-fence");
    let guess_attr = blitz_dom::LocalName::from("data-cre-fence-box");
    let body_after_attr = blitz_dom::LocalName::from("data-cre-fence-body-after");
    let mut updates: Vec<(NodeId, String)> = Vec::new();
    for fence in fences.iter() {
        let Some(element) = doc.get_node(*fence).and_then(|node| node.element_data()) else {
            continue;
        };
        let Some(glyph) = element.attr(glyph_attr.clone()).map(str::to_string) else {
            continue;
        };
        let body_after = element.attr(body_after_attr.clone()).is_some();
        let Some(body) = fence_body_sibling(doc, *fence, body_after) else {
            continue;
        };
        let Some(guess) = element
            .attr(guess_attr.clone())
            .and_then(|value| value.parse::<f32>().ok())
        else {
            continue;
        };
        // The wrapper is an inline span; when it holds a single element (the
        // common case: one `<mtable>`), that element's box is the content the
        // fence has to cover, and its top edge is where the fence starts.
        let measured = single_element_child(doc, body).unwrap_or(body);
        let (Some(fence_rect), Some(body_rect)) = (
            doc.get_client_bounding_rect(*fence),
            doc.get_client_bounding_rect(measured),
        ) else {
            continue;
        };
        // The fence box is `guess` em tall, which gives the size of one em in
        // layout units; the body's height in the same units is what the glyph
        // has to cover.
        if fence_rect.height.is_nan() || fence_rect.height <= 0.0 || guess <= 0.0 {
            continue;
        }
        let em = fence_rect.height / f64::from(guess);
        let mut target = body_rect.height / em;
        let mut align = (body_rect.y - fence_rect.y) / em;
        if !target.is_finite() || target <= 0.0 || !align.is_finite() {
            continue;
        }
        // A display formula that is wider than the page wraps inside its
        // delimited group, and then this measured body is several lines tall —
        // stretching the parenthesis over all of them painted it across the
        // text (a large font in `juliet.epub`). The estimate `guess` was made
        // for the content on one line, so when the measurement is far taller
        // than it, the group wrapped: keep the one-line size and leave the
        // glyph on its own line.
        if target > f64::from(guess) * 1.6 {
            target = f64::from(guess);
            align = 0.0;
        }
        let style = cre_mathml::fence_style_for_measured(
            &glyph,
            guess,
            target as f32,
            align as f32,
            metrics,
        );
        updates.push((*fence, style));
    }
    if updates.is_empty() {
        return false;
    }
    {
        let mut mutator = doc.mutate();
        for (node, style) in updates {
            mutator.set_attribute(
                node,
                blitz_dom::QualName::new(None, blitz_dom::ns!(), blitz_dom::local_name!("style")),
                &style,
            );
        }
    }
    doc.handle_messages();
    doc.resolve(0.0);
    true
}

/// The `.cre-fence-body` span that belongs to `fence`.
///
/// `cre-mathml` wraps the content a fence covers in that span, in place, and
/// marks on the fence whether the body is the following or the preceding
/// sibling (`data-cre-fence-body-after`). A glyph cannot say it: `|` opens and
/// closes groups, so guessing from the character paired the closing bar of a
/// determinant with the wrong span and left it at its estimated height — the
/// right bar came out taller than the left.
fn fence_body_sibling(doc: &HtmlDocument, fence: NodeId, body_after: bool) -> Option<NodeId> {
    let node = doc.get_node(fence)?;
    let siblings = &doc.get_node(node.parent?)?.children;
    let index = siblings.iter().position(|id| *id == fence)?;
    let preferred = if body_after {
        siblings.get(index + 1)
    } else {
        index.checked_sub(1).and_then(|i| siblings.get(i))
    };
    let fallback = if body_after {
        index.checked_sub(1).and_then(|i| siblings.get(i))
    } else {
        siblings.get(index + 1)
    };
    preferred
        .into_iter()
        .chain(fallback)
        .copied()
        .find(|id| is_fence_body(doc, *id))
}

/// Whether `node` is a `.cre-fence-body` span.
fn is_fence_body(doc: &HtmlDocument, node: NodeId) -> bool {
    let Some(element) = doc.get_node(node).and_then(|node| node.element_data()) else {
        return false;
    };
    element.name.local.to_string() == "span"
        && element.attrs().iter().any(|attr| {
            attr.name.local.to_string() == "class"
                && attr
                    .value
                    .split_whitespace()
                    .any(|name| name == "cre-fence-body")
        })
}

/// The only element child of `node`, if it has exactly one.
fn single_element_child(doc: &HtmlDocument, node: NodeId) -> Option<NodeId> {
    let children = doc.get_node(node)?.children.clone();
    let mut elements = children
        .into_iter()
        .filter(|child| doc.get_node(*child).is_some_and(|node| node.is_element()));
    let first = elements.next()?;
    elements.next().is_none().then_some(first)
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

    #[test]
    fn parses_fixed_layout_chunk_marker() {
        let fixed = parse_fixed_layout(
            "<div class=\"cre-fxl-page\" data-cre-fixed-layout=\"900x600\">x</div>",
        )
        .unwrap();
        assert_eq!(
            fixed,
            FixedLayout::Pixel {
                width: 900.0,
                height: 600.0
            }
        );
        assert_eq!(
            parse_fixed_layout("<div data-cre-fixed-layout=\"device\">x</div>"),
            Some(FixedLayout::Device)
        );
        assert!(parse_fixed_layout("<div>reflow</div>").is_none());
    }

    #[test]
    fn device_fixed_layout_resolves_against_the_reader() {
        let fixed = FixedLayout::Device;
        assert_eq!(fixed.size(768.0, 1024.0), (768.0, 1024.0));
        let pixel = FixedLayout::Pixel {
            width: 900.0,
            height: 600.0,
        };
        assert_eq!(pixel.size(768.0, 1024.0), (900.0, 600.0));
    }

    #[test]
    fn strips_column_declarations_from_inline_and_block_styles() {
        let html = r#"<html><head><style>div { column-count: 3; color: red; }</style></head>
<body><div style="column-count: 3; color: blue; -webkit-column-width: 10px">x</div>
<p style='columns: 2'>y</p></body></html>"#;
        let stripped = strip_multi_column_styles(html);
        assert!(!stripped.contains("column-count"));
        assert!(!stripped.contains("column-width"));
        assert!(!stripped.contains("columns: 2"));
        assert!(stripped.contains("color: red"));
        assert!(stripped.contains("color: blue"));
        assert!(stripped.contains(">x<"));
        assert!(stripped.contains(">y<"));
    }

    const MB: usize = 1024 * 1024;

    #[test]
    fn small_book_fits_in_one_layout_on_a_kindle() {
        // PW2 with the framework stopped: ~80 MB free, a 250 kB novel.
        assert_eq!(plan_chunk_bytes(250 * 1024, Some(80 * MB)), usize::MAX);
    }

    #[test]
    fn spine_sections_are_read_from_the_markup() {
        // The EPUB generator: slot and real index plus the resource path.
        let epub = "<section class=\"calibre\" data-spine-index=\"7\" \
                    data-spine-real-index=\"3\" data-spine-id=\"id7\" \
                    data-spine-href=\"text/part0029.html\"><p>one</p></section>";
        let sections = scan_spine_sections(epub);
        assert_eq!(sections.len(), 1);
        // XPointers number fragments from `data-spine-index + 1`.
        assert_eq!(sections[0].slot, 8);
        assert_eq!(sections[0].real, 3);
        assert_eq!(sections[0].href.as_deref(), Some("text/part0029.html"));

        // MOBI/KF8 emit the slot only, and put other attributes after it.
        let mobi = "<section class=\"cre-spine\" data-spine-index=\"1\" id=\"cre-sec-1\">\
                    <p>x</p></section>";
        let sections = scan_spine_sections(mobi);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].slot, 2);
        assert_eq!(sections[0].real, 2);
        assert_eq!(sections[0].href, None);

        // Several sections in one chunk, and no marker at all (FB2/TXT).
        let many = "<section class=\"cre-spine\" data-spine-index=\"1\"></section>\
                    <section class=\"cre-spine\" data-spine-index=\"2\"></section>";
        assert_eq!(scan_spine_sections(many).len(), 2);
        assert!(scan_spine_sections("<p>no sections</p>").is_empty());
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

/// Rects of the generated math spans, with a flag for block-level formulas.
///
/// Blitz measures a line from the text metrics, so a line that holds a tall
/// formula (fraction, radical, matrix) is reported shorter than it paints.
/// Pagination then believes the line fits and cuts the formula at the page
/// boundary — part of it above, the rest on the next page. The rects collected
/// here let `paginate` see the real extent: block formulas become atomic boxes
/// and the line holding an inline formula grows to cover it.
fn collect_math_boxes(doc: &HtmlDocument, root: &Node) -> Vec<(f32, f32, bool)> {
    let mut boxes = Vec::new();
    collect_math_boxes_into(doc, root, &mut boxes);
    boxes.sort_by(|left, right| left.0.total_cmp(&right.0));
    boxes
}

fn collect_math_boxes_into(doc: &HtmlDocument, node: &Node, out: &mut Vec<(f32, f32, bool)>) {
    if let Some(element) = node.element_data()
        && node_attr(node, "class")
            .is_some_and(|class| class.split_whitespace().any(|name| name == "cre-math"))
        && element.name.local.to_string() == "span"
        && let Some(rect) = doc.get_client_bounding_rect(node.id)
        && rect.height > 0.0
    {
        let display = node_attr(node, "data-display") == Some("block");
        out.push((rect.y as f32, (rect.y + rect.height) as f32, display));
        // A math block can contain nested rows; its rect covers them.
        return;
    }
    for child in &node.children {
        collect_math_boxes_into(doc, node.with(*child), out);
    }
}

/// Grows lines so they cover the formulas drawn on them.
fn extend_lines_for_math(lines: &mut [LineBox], math: &[(f32, f32, bool)]) {
    for (top, bottom, _) in math {
        for line in lines.iter_mut() {
            if line.top < *bottom && line.bottom > *top {
                line.top = line.top.min(*top);
                line.visual_top = line.visual_top.min(*top);
                line.bottom = line.bottom.max(*bottom);
            }
        }
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
        ) {
            let rects = doc.node_client_rects(node.id);
            let mut pushed = false;
            for rect in &rects {
                if rect.height > 0.0 {
                    out.push(ContentBox {
                        top: rect.y as f32,
                        bottom: (rect.y + rect.height) as f32,
                    });
                    pushed = true;
                }
            }
            if !pushed
                && let Some(rect) = doc.get_client_bounding_rect(node.id)
                && rect.height > 0.0
            {
                out.push(ContentBox {
                    top: rect.y as f32,
                    bottom: (rect.y + rect.height) as f32,
                });
            }
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
        // Prose selections cover the line; a formula token only its glyph.
        let box_top = line.box_top.max(top).max(lower);
        let box_bottom = line.box_bottom.min(bottom).min(upper);
        let (box_top, box_bottom) = if box_bottom > box_top {
            (box_top, box_bottom)
        } else {
            (top, bottom)
        };

        boxes.push(SnapshotLine {
            text,
            x: line.left,
            y: box_top - start.top,
            width: (line.right - line.left).max(0.0),
            height: box_bottom - box_top,
            global_line: line_base + local_line,
            line_top: line.sort_top,
            math: line.math,
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

fn href_key(href: &str) -> (String, Option<String>) {
    let (path, fragment) = split_toc_href(href);
    let path = if path.contains("://") {
        url::Url::parse(path)
            .map(|url| {
                percent_decode_str(url.path().trim_start_matches('/'))
                    .decode_utf8_lossy()
                    .into_owned()
            })
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

/// Stand-in page for a navigation target whose chunk has not been laid out yet:
/// the first page of the chunk that holds its resource. Exact as soon as the
/// chunk is laid out (lazy open, before the background pass reaches it).
fn provisional_page_for_href(first_pages: &HashMap<String, usize>, href: &str) -> Option<usize> {
    let (path, _fragment) = href_key(href);
    first_pages.get(&path).copied()
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

pub(crate) fn page_containing_top(top: f32, page_starts: &[PageStart]) -> usize {
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

/// CSS properties that only describe how an SVG drawing is painted.
const SVG_PRESENTATION_PROPERTIES: &[&str] = &[
    "fill",
    "fill-opacity",
    "fill-rule",
    "stroke",
    "stroke-width",
    "stroke-opacity",
    "stroke-linecap",
    "stroke-linejoin",
    "stroke-dasharray",
    "stroke-dashoffset",
    "stop-color",
    "stop-opacity",
    "flood-color",
    "flood-opacity",
    "lighting-color",
    "paint-order",
    "marker",
    "clip-rule",
    "shape-rendering",
    "text-rendering",
    "image-rendering",
    "color-rendering",
    "dominant-baseline",
    "text-anchor",
    "font-family",
    "font-size",
    "font-style",
    "font-weight",
    "letter-spacing",
    "word-spacing",
    "visibility",
    "opacity",
    "color",
];

/// Wraps the picture-only parts of a document into page-fit plates.
///
/// Called before the document is split into chunks, so the generated
/// `cre-page-fit-N` keys are in place for every chunk and the chunk budgets see
/// the small wrapper instead of the picture.
fn prepare_page_images(
    body: String,
    resources: &ResourceSource,
    options: &DocumentOptions,
    svg: &Arc<ImageRaster>,
) -> String {
    let available = (
        options.width as f32 - options.page_margin * 2.0,
        options.height as f32 - options.page_margin * 2.0,
    );
    let mut page_fit = HashMap::new();
    let body = normalize_page_images(body, resources, available, &mut page_fit);
    svg.set_page_fit_images(page_fit);
    body
}

/// Prepares the inline `<svg>` elements of one chunk for Blitz.
///
/// Blitz hands an inline `<svg>` to `usvg` as a standalone image
/// (`BaseDocument::svg_source` -> `SpecialElementData::Image`), so the drawing
/// never sees the HTML document's stylesheets: `.fil0 { fill: green }` in the
/// page `<head>` is ignored and the element keeps the SVG defaults. Rules that
/// describe painting are copied into each element, where `usvg` reads them.
///
/// `anyrender_svg`, which blitz-paint uses to paint the element, has no
/// `<pattern>` support: `usvg::Paint::Pattern` becomes `None` and its error
/// handler paints a translucent red box over the shape. Those drawings are
/// rasterized with resvg instead (which does support patterns) and put back as
/// one `<image>` inside the same `<svg>`, so the element keeps its size.
///
/// TODO(upstream): drop both halves once Blitz passes the document's cascaded
/// `fill`/`stroke` into the inline SVG and `anyrender_svg` renders patterns.
fn prepare_inline_svgs(html: &str, css: &str, width_hint: u32) -> String {
    let styled = apply_document_css_to_inline_svgs(html, &svg_presentation_rules(css));
    rasterize_pattern_svgs(&styled, width_hint)
}

/// Copies the SVG presentation rules of a document into every inline `<svg>`.
fn apply_document_css_to_inline_svgs(html: &str, rules: &str) -> String {
    if rules.is_empty() {
        return html.to_string();
    }
    let ranges = inline_svg_ranges(html);
    if ranges.is_empty() {
        return html.to_string();
    }
    let style = format!("<style type=\"text/css\">{rules}</style>");
    let mut out = String::with_capacity(html.len() + ranges.len() * style.len());
    let mut cursor = 0usize;
    for (start, _end) in ranges {
        let Some(open_end) = html[start..].find('>').map(|end| start + end + 1) else {
            continue;
        };
        out.push_str(&html[cursor..open_end]);
        out.push_str(&style);
        cursor = open_end;
    }
    out.push_str(&html[cursor..]);
    out
}

/// Keeps only the rules whose declarations paint SVG content, with the
/// `[data-spine-id]` scoping prefix stripped (usvg does not understand
/// attribute selectors, and inside the SVG the rules are already local).
fn svg_presentation_rules(css: &str) -> String {
    if !contains_ignore_ascii_case(css, "fill") && !contains_ignore_ascii_case(css, "stroke") {
        return String::new();
    }
    static RULE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static SCOPE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let rule = RULE.get_or_init(|| {
        regex::Regex::new(r"(?is)([^{}]+)\{([^{}]*)\}").expect("CSS rule regex is valid")
    });
    let scope = SCOPE.get_or_init(|| {
        regex::Regex::new(r#"(?i)\[data-spine-id\s*=\s*"[^"]*"\]\s*"#)
            .expect("CSS scope regex is valid")
    });
    let mut out = String::new();
    for captures in rule.captures_iter(css) {
        let selectors = captures[1].trim_start();
        if selectors.starts_with('@') {
            continue;
        }
        let declarations = &captures[2];
        if !svg_declarations_paint(declarations) {
            continue;
        }
        let selectors = scope.replace_all(&captures[1], "");
        out.push_str(selectors.trim());
        out.push('{');
        out.push_str(declarations);
        out.push('}');
    }
    out
}

fn svg_declarations_paint(declarations: &str) -> bool {
    declarations.split(';').any(|declaration| {
        let name = declaration
            .split(':')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        SVG_PRESENTATION_PROPERTIES.contains(&name.as_str())
    })
}

/// Byte ranges of the outermost `<svg>...</svg>` elements.
fn inline_svg_ranges(html: &str) -> Vec<(usize, usize)> {
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut ranges = Vec::new();
    let mut cursor = 0usize;
    while let Some(pos) = lower[cursor..].find("<svg") {
        let start = cursor + pos;
        // `<svgfoo>` is not an svg element.
        if !svg_tag_boundary(bytes.get(start + 4).copied()) {
            cursor = start + 4;
            continue;
        }
        let Some(open_end) = html[start..].find('>').map(|end| start + end + 1) else {
            break;
        };
        let mut depth = 1usize;
        let mut scan = open_end;
        let mut end = html.len();
        while depth > 0 {
            let next_open = lower[scan..].find("<svg").map(|pos| scan + pos);
            let next_close = lower[scan..].find("</svg").map(|pos| scan + pos);
            match (next_open, next_close) {
                (Some(open), Some(close)) if open < close => {
                    if svg_tag_boundary(bytes.get(open + 4).copied()) {
                        depth += 1;
                    }
                    scan = open + 4;
                }
                (_, Some(close)) => {
                    depth -= 1;
                    let close_end = lower[close..]
                        .find('>')
                        .map(|end| close + end + 1)
                        .unwrap_or(html.len());
                    scan = close_end;
                    if depth == 0 {
                        end = close_end;
                    }
                }
                _ => return ranges,
            }
        }
        ranges.push((start, end));
        cursor = end;
    }
    ranges
}

fn svg_tag_boundary(byte: Option<u8>) -> bool {
    matches!(byte, Some(b'>') | Some(b'/')) || byte.is_some_and(|b| b.is_ascii_whitespace())
}

/// Rasterizes inline SVGs that paint with a `<pattern>` and swaps the drawing
/// for the resulting picture. `resvg` supports patterns; `anyrender_svg`, which
/// blitz-paint uses, does not.
fn rasterize_pattern_svgs(html: &str, width_hint: u32) -> String {
    if !contains_ignore_ascii_case(html, "<pattern") {
        return html.to_string();
    }
    let ranges = inline_svg_ranges(html);
    if ranges.is_empty() {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    for (start, end) in ranges {
        let block = &html[start..end];
        if !contains_ignore_ascii_case(block, "<pattern") || !pattern_is_referenced(block) {
            continue;
        }
        let Some(open_end) = html[start..].find('>').map(|pos| start + pos + 1) else {
            continue;
        };
        let open_tag = &html[start..open_end];
        let Some((x, y, width, height)) = svg_view_box(open_tag) else {
            continue;
        };
        // Twice the CSS size of the drawing keeps it sharp on screen without
        // holding a full-page bitmap per picture.
        let target = (width * 2.0).clamp(240.0, width_hint.max(240) as f32 * 2.0);
        let Some((rgba, raster_width, raster_height)) =
            render_svg_image(block.as_bytes(), Some(target as u32), None)
        else {
            continue;
        };
        let Some(png) = encode_png(&rgba, raster_width, raster_height) else {
            continue;
        };
        let encoded = cre_book::base64_encode(&png);
        out.push_str(&html[cursor..open_end]);
        out.push_str(&format!(
            "<image x=\"{x}\" y=\"{y}\" width=\"{width}\" height=\"{height}\" \
             href=\"data:image/png;base64,{encoded}\"/></svg>"
        ));
        cursor = end;
    }
    out.push_str(&html[cursor..]);
    out
}

/// True when the drawing references one of its own `<pattern>` elements.
fn pattern_is_referenced(block: &str) -> bool {
    static PATTERN_ID: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern_id = PATTERN_ID.get_or_init(|| {
        regex::Regex::new(r#"(?is)<pattern\b[^>]*\bid\s*=\s*(?:"([^"]+)"|'([^']+)')"#)
            .expect("pattern id regex is valid")
    });
    let lower = block.to_ascii_lowercase();
    pattern_id.captures_iter(block).any(|captures| {
        let id = captures
            .get(1)
            .or_else(|| captures.get(2))
            .map_or("", |id| id.as_str());
        !id.is_empty() && lower.contains(&format!("url(#{})", id.to_ascii_lowercase()))
    })
}

/// The `viewBox` of the root `<svg>` open tag.
fn svg_view_box(open_tag: &str) -> Option<(f32, f32, f32, f32)> {
    static VIEW_BOX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let view_box = VIEW_BOX.get_or_init(|| {
        regex::Regex::new(r#"(?i)\bviewBox\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
            .expect("viewBox regex is valid")
    });
    let captures = view_box.captures(open_tag)?;
    let value = captures.get(1).or_else(|| captures.get(2))?.as_str();
    let numbers: Vec<f32> = value
        .split(|character: char| character.is_whitespace() || character == ',')
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<f32>().ok())
        .collect();
    let [x, y, width, height] = numbers.as_slice() else {
        return None;
    };
    (width.is_finite() && height.is_finite() && *width > 0.0 && *height > 0.0)
        .then_some((*x, *y, *width, *height))
}

/// The document's CSS, from its `<style>` blocks and linked stylesheets.
fn document_css_for_svg(html: &str, resources: &ResourceSource) -> String {
    if !contains_ignore_ascii_case(html, "<style") && resources.is_empty() {
        return String::new();
    }
    let lower = html.to_ascii_lowercase();
    let head_end = lower.find("</head").unwrap_or(lower.len());
    let head = &html[..head_end];
    let lower_head = &lower[..head_end];
    let mut css = String::new();

    let mut cursor = 0usize;
    while let Some(pos) = lower_head[cursor..].find("<style") {
        let start = cursor + pos;
        let Some(text_start) = lower_head[start..].find('>').map(|end| start + end + 1) else {
            break;
        };
        let Some(close) = lower_head[text_start..].find("</style") else {
            break;
        };
        css.push_str(&head[text_start..text_start + close]);
        css.push('\n');
        cursor = text_start + close + "</style".len();
    }

    static LINK: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static HREF: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static REL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let link = LINK
        .get_or_init(|| regex::Regex::new(r"(?is)<link\b[^>]*>").expect("link tag regex is valid"));
    let href = HREF.get_or_init(|| {
        regex::Regex::new(r#"(?is)\bhref\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
            .expect("href regex is valid")
    });
    let rel = REL.get_or_init(|| {
        regex::Regex::new(r#"(?is)\brel\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
            .expect("rel regex is valid")
    });
    for tag in link.find_iter(head) {
        let tag = tag.as_str();
        let Some(rel_value) = rel
            .captures(tag)
            .and_then(|captures| captures.get(1).or_else(|| captures.get(2)))
        else {
            continue;
        };
        if !rel_value
            .as_str()
            .split_ascii_whitespace()
            .any(|value| value.eq_ignore_ascii_case("stylesheet"))
        {
            continue;
        }
        let Some(href_value) = href
            .captures(tag)
            .and_then(|captures| captures.get(1).or_else(|| captures.get(2)))
        else {
            continue;
        };
        let href_value = href_value.as_str().trim();
        // EPUB chunks point at `cre://book/...`; a standalone document keeps
        // the link relative to its own directory.
        let key = if let Some(path) = href_value.strip_prefix("cre://book/") {
            normalize_path(&percent_decode_str(path).decode_utf8_lossy())
        } else if href_value.contains("://") {
            continue;
        } else {
            normalize_path(&percent_decode_str(href_value).decode_utf8_lossy())
        };
        if let Some(bytes) = resources.get(&key) {
            css.push_str(&String::from_utf8_lossy(&bytes));
            css.push('\n');
        }
    }
    css
}

/// Transparent SVG placeholders, by intrinsic size.
type PlaceholderCache = HashMap<(u32, u32), Arc<Vec<u8>>>;

/// Rasterizes SVG resources once, at page size, and keeps the PNGs in a
/// byte-bounded cache; also replaces oversized raster images with a size
/// placeholder (see [`ImageRaster::placeholder`]).
struct ImageRaster {
    max_width: u32,
    max_height: u32,
    cache: std::sync::Mutex<ImageRasterCache>,
    /// Resource keys referenced by `<img>` elements. Only those may become a
    /// placeholder: a CSS `background-image` has no other painter, so it must
    /// keep its pixels.
    content: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Keys replaced by a placeholder, with the image's real intrinsic size.
    placeheld: std::sync::Mutex<HashMap<String, (u32, u32)>>,
    /// Transparent SVG of a given intrinsic size, used as a layout placeholder.
    placeholders: std::sync::Mutex<PlaceholderCache>,
    /// Generated page-fit wrappers and the original image each stands for.
    page_fit: std::sync::Mutex<HashMap<String, cre_book::images::PageFitImage>>,
}

#[derive(Default)]
struct ImageRasterCache {
    entries: HashMap<String, Arc<Vec<u8>>>,
    order: std::collections::VecDeque<String>,
    bytes: usize,
    limit: usize,
}

impl ImageRaster {
    fn new(width: u32, height: u32) -> Self {
        Self {
            max_width: width.max(1),
            max_height: height.max(1),
            cache: std::sync::Mutex::new(ImageRasterCache {
                limit: 8 * 1024 * 1024,
                ..ImageRasterCache::default()
            }),
            content: std::sync::Mutex::new(std::collections::HashSet::new()),
            placeheld: std::sync::Mutex::new(HashMap::new()),
            placeholders: std::sync::Mutex::new(HashMap::new()),
            page_fit: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Notes which resources `<img>` elements reference.
    fn set_content_images(&self, keys: impl IntoIterator<Item = String>) {
        if let Ok(mut content) = self.content.lock() {
            content.clear();
            content.extend(keys);
        }
    }

    /// Whether this resource is an `<img>` the layout may replace with a size
    /// placeholder, i.e. one whose pixels the painter reads again.
    ///
    /// The net providers use it to read only an image header while laying a
    /// chunk out: a stylesheet, a font or a CSS `background-image` keeps its
    /// whole bytes, a content picture does not need them.
    fn is_content_image(&self, key: &str) -> bool {
        self.content
            .lock()
            .map(|content| content.contains(key))
            .unwrap_or(false)
    }

    /// Images the renderer paints itself from the original resource.
    fn placeheld_count(&self) -> usize {
        self.placeheld.lock().map(|held| held.len()).unwrap_or(0)
    }

    /// Bytes held by the page-sized SVG raster cache.
    fn cached_bytes(&self) -> usize {
        self.cache.lock().map(|cache| cache.bytes).unwrap_or(0)
    }

    /// Records the transparent page-fit wrappers generated at open.
    fn set_page_fit_images(&self, images: HashMap<String, cre_book::images::PageFitImage>) {
        if let Ok(mut page_fit) = self.page_fit.lock() {
            page_fit.extend(images);
        }
    }

    /// The original image a page-fit wrapper paints, with its real size.
    fn page_fit_image(&self, key: &str) -> Option<cre_book::images::PageFitImage> {
        self.page_fit.lock().ok()?.get(key).cloned()
    }

    /// The intrinsic size recorded when `key` was replaced by a placeholder.
    fn placeheld_size(&self, key: &str) -> Option<(u32, u32)> {
        self.placeheld.lock().ok()?.get(key).copied()
    }

    /// Replaces an oversized raster image with a transparent SVG of the same
    /// intrinsic size.
    ///
    /// Layout only needs the image's size and aspect ratio, and the placeholder
    /// declares the exact intrinsic size the raster had, so the element box is
    /// unchanged. blitz therefore holds no natural-size RGBA copy (a 1443x2048
    /// photo is 11 MB decoded) and a page with many small images stops keeping
    /// one decoded copy per image. The pixels are painted from the original
    /// resource in `cre_paint::draw_placed_images`.
    ///
    /// TODO(upstream): blitz's `NetHandler` only accepts bytes and decodes them
    /// privately (`ImageHandler::parse`), so an image's intrinsic size and its
    /// pixels cannot be supplied separately. crengine's `LVImageSource` does
    /// exactly that (declared size + row-by-row decode into the target buffer);
    /// with a similar API this placeholder/draw pair would collapse into one
    /// image source and painting would not need `PlacedImage` at all.
    fn placeholder(&self, key: &str, bytes: &[u8]) -> Option<Vec<u8>> {
        if bytes.is_empty() || looks_like_svg(bytes) {
            return None;
        }
        // A vector resource has already been rasterized at page size by
        // `rasterize`, so its bytes are a small PNG now; it must keep them.
        let lower = key.to_ascii_lowercase();
        if lower.ends_with(".svg") || lower.ends_with(".svgz") {
            return None;
        }
        if !self.content.lock().ok()?.contains(key) {
            return None;
        }
        let (width, height) = image_dimensions(bytes)?;
        if width == 0 || height == 0 {
            return None;
        }
        if let Ok(mut held) = self.placeheld.lock() {
            held.insert(key.to_string(), (width, height));
        }
        let mut placeholders = self.placeholders.lock().ok()?;
        if let Some(cached) = placeholders.get(&(width, height)) {
            return Some((**cached).clone());
        }
        let svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\"></svg>"
        )
        .into_bytes();
        let svg = Arc::new(svg);
        if placeholders.len() >= 64 {
            placeholders.clear();
        }
        placeholders.insert((width, height), svg.clone());
        Some((*svg).clone())
    }

    fn rasterize(&self, key: &str, bytes: &[u8]) -> Option<Vec<u8>> {
        // A page-fit wrapper is already a transparent vector of the page's
        // size; rasterizing it would just cache a blank page.
        if key.starts_with("cre-page-fit-") {
            return None;
        }
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

/// Intrinsic pixel size of a raster image, read from its header. The pixels are
/// not decoded (a large photo's header alone gives the layout size).
fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
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

#[cfg(test)]
mod map_page_image_tests {
    use super::*;

    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([16, 16, 16]));
        let mut out = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80);
        encoder
            .encode(
                image.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgb8,
            )
            .expect("encode jpeg");
        out
    }

    fn book_body(src: &str) -> String {
        format!(
            "<section class=\"cre-spine\" data-spine-index=\"0\" id=\"cre-sec-1\">\
             <div class=\"fs\" id=\"k8-0-326\"><div><img src=\"{src}\" width=\"600\" \
             height=\"800\"/></div></div>\
             </section>\
             <section class=\"cre-spine\" data-spine-index=\"1\" id=\"cre-sec-2\">\
             <p>text</p></section>"
        )
    }

    fn source_with(image: Vec<u8>) -> ResourceSource {
        let mut map = HashMap::new();
        map.insert("page.jpg".to_string(), image);
        ResourceSource::map(MapResources::Owned(Arc::new(map)))
    }

    #[test]
    fn a_picture_part_becomes_a_page_fit_plate() {
        let resources = source_with(jpeg(600, 800));
        let mut page_fit = HashMap::new();
        let out = normalize_page_images(
            book_body("page.jpg"),
            &resources,
            (600.0, 800.0),
            &mut page_fit,
        );
        assert!(out.contains("src=\"cre-page-fit-0\""), "{out}");
        // The picture itself is not copied into the document any more.
        assert!(!out.contains("page.jpg"), "{out}");
        // The part's TOC anchor stays: navigation entries point at `#k8-0-326`,
        // so dropping the element would lose the page. It is re-emitted as an
        // empty anchor, because the `<div class="fs">` around it also carries
        // the book's page-box CSS.
        assert!(out.contains("<a id=\"k8-0-326\"></a>"), "{out}");
        assert!(!out.contains("class=\"fs\""), "{out}");
        // A part with text is left alone.
        assert!(out.contains("<p>text</p>"), "{out}");

        let plate = page_fit.get("cre-page-fit-0").expect("plate");
        assert_eq!(plate.source, "page.jpg");
        assert_eq!((plate.width, plate.height), (600, 800));
        assert!(
            plate.contain,
            "a plate keeps its aspect inside the page box"
        );
        // The wrapper is a generated resource, like every other resource kind:
        // the painter and the net provider read it back through `get`.
        let svg = String::from_utf8(resources.get("cre-page-fit-0").unwrap().to_vec()).unwrap();
        assert!(svg.contains("viewBox=\"0 0 600 800\""), "{svg}");
        assert!(!svg.contains("<image"), "{svg}");
    }

    #[test]
    fn an_inlined_data_uri_becomes_a_plate() {
        // A standalone HTML document can inline its pictures; the painter still
        // needs bytes under a key, so the pass decodes them once.
        let encoded = cre_book::base64_encode(&jpeg(300, 400));
        let src = format!("data:image/jpeg;base64,{encoded}");
        let resources = ResourceSource::empty();
        let mut page_fit = HashMap::new();
        let out = normalize_page_images(book_body(&src), &resources, (600.0, 800.0), &mut page_fit);
        assert!(out.contains("src=\"cre-page-fit-0\""), "{out}");
        assert!(!out.contains("base64"), "{out}");
        let plate = page_fit.get("cre-page-fit-0").expect("plate");
        assert_eq!((plate.width, plate.height), (300, 400));
        // The decoded picture is readable through the same table.
        let bytes = resources.get(&plate.source).expect("decoded picture");
        assert_eq!(image_dimensions(&bytes), Some((300, 400)));
    }

    #[test]
    fn plate_rewriting_is_idempotent() {
        let resources = source_with(jpeg(600, 800));
        let once = normalize_page_images(
            book_body("page.jpg"),
            &resources,
            (600.0, 800.0),
            &mut HashMap::new(),
        );
        let twice = normalize_page_images(
            once.clone(),
            &resources,
            (600.0, 800.0),
            &mut HashMap::new(),
        );
        assert_eq!(twice, once, "a generated plate must not be wrapped again");
    }

    #[test]
    fn a_picture_with_unknown_size_is_left_as_it_is() {
        let resources = ResourceSource::empty();
        let out = normalize_page_images(
            book_body("page.jpg"),
            &resources,
            (600.0, 800.0),
            &mut HashMap::new(),
        );
        assert_eq!(out, book_body("page.jpg"));
    }
}

#[cfg(test)]
mod font_size_tests {
    use super::*;

    #[test]
    fn absolute_font_size_keywords_use_the_browser_ratios() {
        let html = concat!(
            "<p style=\"font-size: x-large\">x</p>",
            "<style>h1 { font-size: large; } h2 { font-size: XX-SMALL; }</style>"
        );
        // 18px medium: x-large 1.5 -> 27, large 1.2 -> 22 (21.6 rounded up),
        // xx-small 0.6 -> 11 (10.8 rounded up).
        let out = absolute_font_size_keywords_become_px(html, 18.0);
        assert!(out.contains("font-size: 27px"), "{out}");
        assert!(out.contains("font-size: 22px"), "{out}");
        assert!(out.contains("font-size: 11px"), "{out}");
        // `smaller`/`larger` are relative keywords and stay as they are.
        let relative = absolute_font_size_keywords_become_px("p { font-size: smaller }", 18.0);
        assert_eq!(relative, "p { font-size: smaller }");
    }

    #[test]
    fn font_tags_get_pixel_sizes_from_the_reader_size() {
        let out = font_size_attributes_become_styles(
            "<font size=\"5\">x</font><font size=\"+1\">y</font><font size=\"1\">z</font>",
            18.0,
        );
        // 5 = x-large 27px, +1 = large 22px, 1 = x-small 14px (13.5 up).
        assert!(out.contains("font-size:27px"), "{out}");
        assert!(out.contains("font-size:22px"), "{out}");
        assert!(out.contains("font-size:14px"), "{out}");
        // An explicit declaration wins, like it does in a browser.
        let styled = font_size_attributes_become_styles(
            "<font size=\"6\" style=\"font-size:10px\">x</font>",
            18.0,
        );
        assert!(styled.contains("font-size:10px"), "{styled}");
        assert!(!styled.contains("font-size:36px"), "{styled}");
    }
}

#[cfg(test)]
mod inline_svg_tests {
    use super::*;

    #[test]
    fn only_painting_rules_are_copied_into_an_svg() {
        let css = r#"[data-spine-id="ch1"] p { margin: 0; }
[data-spine-id="ch1"] .fil0 { fill: url(#star); stroke: green; }
body { font-size: 12px; }
[data-spine-id="ch1"] .hint { color: red; }"#;
        let rules = svg_presentation_rules(css);
        assert!(rules.contains(".fil0{ fill: url(#star); stroke: green; }"));
        assert!(rules.contains(".hint{ color: red; }"));
        assert!(!rules.contains("margin"));
        assert!(!rules.contains("data-spine-id"));
    }

    #[test]
    fn svg_blocks_include_nested_svg_elements() {
        let html =
            r#"<svg viewBox="0 0 10 10"><svg viewBox="0 0 5 5"><path/></svg><path/></svg><p>x</p>"#;
        let ranges = inline_svg_ranges(html);
        assert_eq!(ranges.len(), 1);
        assert_eq!(
            &html[ranges[0].0..ranges[0].1],
            &html[..html.find("<p>").unwrap()]
        );
    }

    #[test]
    fn document_rules_are_inserted_inside_the_svg_element() {
        let html = r#"<p>text</p><svg viewBox="0 0 10 10"><rect class="fil0"/></svg>"#;
        let rules = ".fil0{fill:url(#star)}";
        let out = apply_document_css_to_inline_svgs(html, rules);
        let svg_start = out.find("<svg").unwrap();
        let style = out.find("<style type=\"text/css\">.fil0").unwrap();
        let rect = out.find("<rect").unwrap();
        assert!(svg_start < style && style < rect, "{out}");
        // Nothing outside the element is touched.
        assert!(out.starts_with("<p>text</p><svg"));
    }

    #[test]
    fn patterns_are_rasterized_and_plain_drawings_are_not() {
        let pattern = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20">
                 <defs><pattern id="p" width="10%" height="10%">
                   <rect width="5" height="5" fill="green"/>
                 </pattern></defs>
                 <rect width="20" height="20" fill="url(#p)"/></svg>"#;
        let out = rasterize_pattern_svgs(pattern, 600);
        assert!(
            out.contains("<image x=\"0\" y=\"0\" width=\"20\" height=\"20\""),
            "{out}"
        );
        assert!(out.contains("data:image/png;base64,"));
        assert!(!out.contains("<pattern"));

        let plain = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20"><rect width="5" height="5" fill="green"/></svg>"#;
        assert_eq!(rasterize_pattern_svgs(plain, 600), plain);

        // A `<pattern>` nobody references still paints fine as vectors.
        let unused = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20"><defs><pattern id="p" width="10%" height="10%"/></defs><rect width="5" height="5" fill="green"/></svg>"#;
        assert_eq!(rasterize_pattern_svgs(unused, 600), unused);
    }

    #[test]
    fn view_box_is_read_from_the_root_open_tag_only() {
        assert_eq!(
            svg_view_box(r#"<svg viewBox="0,0 100 50" xmlns="x">"#),
            Some((0.0, 0.0, 100.0, 50.0))
        );
        assert_eq!(svg_view_box(r#"<svg width="10" height="10">"#), None);
        assert_eq!(svg_view_box(r#"<svg viewBox="0 0 0 10">"#), None);
    }
}

#[cfg(test)]
mod image_placeholder_tests {
    use super::*;

    /// A valid RGBA PNG of the given size, all pixels transparent.
    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("png header");
            writer
                .write_image_data(&vec![0u8; (width * height * 4) as usize])
                .expect("png data");
        }
        out
    }

    #[test]
    fn oversized_content_image_becomes_a_size_placeholder() {
        let images = ImageRaster::new(600, 800);
        let bytes = png_bytes(1200, 1600);
        // A CSS background image has no other painter: it keeps its pixels.
        assert!(images.placeholder("background.png", &bytes).is_none());

        images.set_content_images(["photo.png".to_string()]);
        let placeholder = images
            .placeholder("photo.png", &bytes)
            .expect("oversized content image becomes a placeholder");
        let text = String::from_utf8(placeholder).expect("placeholder is SVG");
        assert!(text.contains(r#"width="1200""#), "{text}");
        assert!(text.contains(r#"height="1600""#), "{text}");
        assert_eq!(images.placeheld_size("photo.png"), Some((1200, 1600)));
    }

    #[test]
    fn chunk_html_survives_compression() {
        let html = "<p>中文 mixed with emoji 📚 and 0 bytes</p>".repeat(200);
        let source = ChunkSource::new(html.clone());
        assert_eq!(source.text_len, html.len());
        assert_eq!(source.text().expect("decompress"), html);
    }

    #[test]
    fn an_empty_page_fit_svg_keeps_the_page_size() {
        // The generated wrapper has no content; layout relies on usvg reading
        // its width/height, so a parse failure would collapse the page.
        let (rgba, width, height) = render_svg_image_fit(
            b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"520\" height=\"736\"></svg>",
            520,
            736,
        )
        .expect("an empty SVG with a size must parse");
        assert_eq!((width, height), (520, 736));
        assert_eq!(rgba.len(), 520 * 736 * 4);
    }

    #[test]
    fn small_content_images_keep_their_intrinsic_size() {
        let images = ImageRaster::new(600, 800);
        images.set_content_images(["small.png".to_string()]);
        let bytes = png_bytes(300, 400);
        // The placeholder declares the raster's own size, so a small image is
        // replaced too: layout still sees 300x400.
        let placeholder = images
            .placeholder("small.png", &bytes)
            .expect("small content image becomes a placeholder");
        let text = String::from_utf8(placeholder).expect("placeholder is SVG");
        assert!(text.contains(r#"width="300""#), "{text}");
        assert!(text.contains(r#"height="400""#), "{text}");
        assert_eq!(images.placeheld_size("small.png"), Some((300, 400)));
    }
}

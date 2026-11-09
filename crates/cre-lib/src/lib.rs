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
use cre_paint::render_page_cached as paint_page_cached;
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

/// Bytes the kernel says it can still hand out, if the platform reports it.
fn available_memory_bytes() -> Option<usize> {
    memory_available_detail().map(|(bytes, _)| bytes as usize)
}

/// Free memory and where the figure came from, for the diagnostics page.
fn memory_available_detail() -> Option<(u64, &'static str)> {
    // Testing knob: pretend the device only has this much memory, so the
    // chunk planner can be exercised on a desktop.
    if let Ok(value) = std::env::var("CRE_AVAILABLE_MB")
        && let Ok(mb) = value.trim().parse::<u64>()
        && mb > 0
    {
        return Some((mb.saturating_mul(1024 * 1024), "CRE_AVAILABLE_MB"));
    }
    #[cfg(target_os = "linux")]
    {
        let info = std::fs::read_to_string("/proc/meminfo").ok()?;
        let field = |key: &str| -> Option<u64> {
            info.lines()
                .find(|line| line.starts_with(key))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse::<u64>().ok())
        };
        // MemAvailable exists since Linux 3.14; the Kindle PW2 runs 3.0, so
        // fall back to the classic free+buffers+cached estimate there.
        if let Some(available) = field("MemAvailable:") {
            return Some((available.saturating_mul(1024), "MemAvailable"));
        }
        let free = field("MemFree:")?;
        let buffers = field("Buffers:").unwrap_or(0);
        let cached = field("Cached:").unwrap_or(0);
        return Some((
            (free + buffers + cached).saturating_mul(1024),
            "MemFree+Buffers+Cached",
        ));
    }
    #[allow(unreachable_code)]
    None
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

/// Splits a format that is produced as one HTML document (FB2, TXT, a
/// standalone HTML file) into layout chunks when it would not fit in one,
/// using the same memory budget as EPUB chunking.
fn split_single_document(html: String, resources: &ResourceSource) -> Vec<String> {
    let available = available_memory_bytes();
    let target = plan_chunk_bytes(html.len(), available);
    if target == usize::MAX || html.len() <= target {
        return vec![html];
    }
    // The same budgets the EPUB chunker uses: decoded images dominate a
    // chunk's footprint, and they cannot be seen in the HTML size. A KF8
    // photo page can be 100 kB of JPEG and 20 MB decoded.
    let (text_budget, image_budget, layout_budget) = chunk_budgets(available);
    let text_limit = target.min(text_budget).max(MIN_CHUNK_BYTES);
    let image_budget = image_budget.unwrap_or(usize::MAX);
    let cost = |section: &str| -> (usize, usize) {
        let images = cre_epub::image_sources_in(section)
            .into_iter()
            .filter_map(|key| resource_image_bytes(resources, &key))
            .sum();
        (section.len(), images)
    };
    // MOBI/KF8 bodies are a run of `<section class="cre-spine">` parts; cutting
    // between them keeps each chunk a whole number of parts. A book whose part
    // is bigger than the budget falls back to the generic splitter, which can
    // only respect the text budget.
    if let Some(parts) = cre_epub::split_html_document_at_sections_with(
        &html,
        text_limit,
        image_budget,
        layout_budget,
        LAYOUT_BYTES_PER_HTML_BYTE,
        cost,
    ) {
        return parts;
    }
    cre_epub::split_html_document(&html, text_limit)
}

/// Estimated decoded size of one resource, for the chunk image budget.
fn resource_image_bytes(resources: &ResourceSource, key: &str) -> Option<usize> {
    match resources {
        ResourceSource::Epub(provider) => provider.estimated_decoded_bytes(key),
        ResourceSource::Map(map) => {
            let key = normalize_path(key);
            map.get(&key)
                .map(|bytes| cre_epub::estimated_image_bytes(&key, bytes))
        }
        ResourceSource::Empty => None,
    }
}

/// Text and image budget for one layout chunk, from the memory the kernel can
/// still hand out.
///
/// The text budget uses [`LAYOUT_BYTES_PER_HTML_BYTE`]; the image budget is in
/// estimated decoded bytes, because blitz decodes a chunk's images while
/// parsing it (a dense illustrated book costs far more than its text).
fn chunk_budgets(available: Option<usize>) -> (usize, Option<usize>, usize) {
    let Some(available) = available else {
        return (usize::MAX, None, usize::MAX);
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
    let images = image_share;
    (text, Some(images), budget)
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

/// Trims freed heap pages back to the OS, but only once the resident set has
/// grown by `TRIM_GROWTH_BYTES` since the last trim.
///
/// `malloc_trim` walks the heap and madvises every free span; on a 70-chunk
/// book that cost ~3 ms per chunk, and the trim between two chunks is mostly
/// pointless because the next chunk reallocates the same pages. The guard keeps
/// the slack bounded by one chunk's working set plus `TRIM_GROWTH_BYTES`
/// instead of trimming unconditionally, and peak memory is unaffected.
struct TrimGuard {
    last_rss: Option<u64>,
}

impl TrimGuard {
    /// Slack allowed to build up between trims.
    const TRIM_GROWTH_BYTES: u64 = 8 * 1024 * 1024;

    fn new() -> Self {
        Self {
            last_rss: resident_bytes(),
        }
    }

    fn maybe_trim(&mut self) {
        let Some(rss) = resident_bytes() else {
            // No resident size to compare against: keep the old behaviour.
            trim_memory();
            return;
        };
        let grew = match self.last_rss {
            Some(last) => rss.saturating_sub(last) >= Self::TRIM_GROWTH_BYTES,
            None => true,
        };
        if grew {
            trim_memory();
            self.last_rss = resident_bytes();
        }
    }
}

/// Shared state of the parallel layout pass, so every worker runs the same
/// pull loop and the opening chunk can be laid out by the main thread.
struct ChunkLayoutWork<'a> {
    sources: &'a [ChunkSource],
    base_url: &'a str,
    net: Arc<dyn NetProvider>,
    font_context: Arc<FontContext>,
    font_blobs: &'a [FontBlob],
    options: &'a [DocumentOptions],
    fixed_heights: &'a [Option<f32>],
    results: &'a std::sync::Mutex<Vec<Option<ChunkLayout>>>,
    failure: &'a std::sync::Mutex<Option<anyhow::Error>>,
    stop: &'a std::sync::atomic::AtomicBool,
    next: &'a std::sync::atomic::AtomicUsize,
    gate: &'a LayoutMemoryGate,
    probe_index: usize,
    count: usize,
    resources: &'a ResourceSource,
    margins: &'a [PageMargins],
}

impl ChunkLayoutWork<'_> {
    fn layout(
        &self,
        index: usize,
    ) -> Result<(ChunkLayout, HtmlDocument, Vec<LineBox>, Vec<PageStart>)> {
        layout_chunk_html(
            &self.sources[index].text()?,
            index,
            self.base_url,
            self.net.clone(),
            self.font_context.clone(),
            self.font_blobs,
            &self.options[index],
            self.margins[index],
            self.fixed_heights[index],
            self.resources,
        )
    }

    fn run(&self) {
        let mut trim_guard = TrimGuard::new();
        loop {
            if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            let index = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if index >= self.count {
                break;
            }
            if index == self.probe_index {
                continue;
            }
            self.gate.enter();
            let laid_out = self.layout(index);
            self.gate.leave();
            match laid_out {
                Ok((layout, doc, lines, page_starts)) => {
                    // The DOM belongs to the worker; only the page metadata
                    // has to survive.
                    drop(doc);
                    drop(lines);
                    drop(page_starts);
                    self.results.lock().unwrap()[index] = Some(layout);
                }
                Err(error) => {
                    *self.failure.lock().unwrap() = Some(error);
                    self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
            }
            trim_guard.maybe_trim();
        }
    }
}

/// Keeps the parallel layout pass inside the memory the chunk planner assumed.
///
/// One chunk's layout is planned to fit in the available budget, so running
/// several at once could reach a multiple of it: image-heavy chunks in
/// particular decode tens of megabytes while they lay out. A worker waits
/// before starting a chunk while the resident set is above half of the memory
/// the planner saw, and trims the freed arenas so the pass can resume. The
/// thread that lays out the opening chunk never waits, so the pass always makes
/// progress.
struct LayoutMemoryGate {
    limit: Option<u64>,
    active: std::sync::atomic::AtomicUsize,
}

impl LayoutMemoryGate {
    fn new() -> Self {
        Self {
            // The chunk planner sizes a chunk so its layout fits in
            // `available - LAYOUT_MEMORY_RESERVE`; running several at once must
            // not spend the reserve KOReader keeps for its own widgets.
            limit: available_memory_bytes()
                .map(|bytes| bytes.saturating_sub(LAYOUT_MEMORY_RESERVE) as u64),
            active: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Waits until this worker may start a chunk, then counts it as active.
    fn enter(&self) {
        let Some(limit) = self.limit else {
            self.active
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        };
        let started = std::time::Instant::now();
        loop {
            // Whoever is already laying a chunk out may be holding the memory
            // that pushes the set over the limit, so one layout is always
            // allowed through; otherwise a pass could stall for good.
            let busy = self.active.load(std::sync::atomic::Ordering::Relaxed) > 0;
            let over = resident_bytes().is_some_and(|rss| rss > limit);
            if !over || !busy || started.elapsed() > std::time::Duration::from_millis(500) {
                self.active
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            trim_memory();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn leave(&self) {
        self.active
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// How many chunk layouts to run at once during the open-time pass.
///
/// One chunk's layout costs roughly `LAYOUT_BYTES_PER_HTML_BYTE` per byte of its
/// generated HTML, and the pass now holds that many DOMs at the same time, so
/// the count is bounded by the free memory the chunk planner saw (half of it,
/// leaving room for the page bitmap and the reader). It is also capped at four:
/// past that the pass is bound by memory bandwidth, not cores.
fn layout_worker_count(
    remaining: usize,
    largest_html: usize,
    rss_before: Option<u64>,
    rss_after: Option<u64>,
) -> usize {
    if let Ok(value) = std::env::var("CRE_LAYOUT_WORKERS")
        && let Ok(workers) = value.trim().parse::<usize>()
        && workers > 0
    {
        return workers.min(remaining.max(1));
    }
    let cores = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    if cores < 2 || remaining < 1 {
        return 1;
    }
    let (Some(available), Some(rss_after)) = (available_memory_bytes(), rss_after) else {
        return 1;
    };
    // One chunk's layout is planned to fit in what is left after the reserve
    // the reader keeps; running several at once must not eat into it.
    let usable = available.saturating_sub(LAYOUT_MEMORY_RESERVE);
    let estimated = largest_html.saturating_mul(LAYOUT_BYTES_PER_HTML_BYTE);
    let observed = rss_after.saturating_sub(rss_before.unwrap_or(rss_after)) as usize;
    let per_chunk = estimated.max(observed).max(1);
    let headroom = usable.saturating_sub(rss_after as usize);
    (headroom / per_chunk).clamp(1, cores.min(4)) as usize
}

const PAGE_IMAGE_CACHE_CAPACITY: usize = 2usize;

struct ChunkMeta {
    first_page: usize,
    line_base: usize,
}

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

struct PageMeta {
    chunk: usize,
    local_start: PageStart,
    local_clip_bottom: f32,
    doc_top: f32,
    render_height: f32,
    xpointer: Option<String>,
    page_flow: i64,
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
struct ChunkSource {
    compressed: Vec<u8>,
    /// Uncompressed length, which the layout uses for its estimates.
    text_len: usize,
    /// `<img`/`<image` occurrences, used to pick the probe chunk.
    images: usize,
}

impl ChunkSource {
    fn new(html: String) -> Self {
        let images = html
            .match_indices("<img")
            .count()
            .saturating_add(html.match_indices("<image").count());
        let text_len = html.len();
        let mut compressed = lz4_flex::compress_prepend_size(html.as_bytes());
        compressed.shrink_to_fit();
        Self {
            compressed,
            text_len,
            images,
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
    sources: Vec<ChunkSource>,
    /// Pre-paginated chunks and their own CSS viewport, parallel to `sources`.
    fixed_layouts: Vec<Option<FixedLayout>>,
    base_url: String,
    resources: ResourceSource,
    /// Page-size SVGs and the raster images replaced by a size placeholder.
    /// Painting reads the original pixels through `resources`.
    images: Arc<ImageRaster>,
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
    /// Glyph masks reused across page renders. Cleared when the font context
    /// changes, because the masks carry the face's outlines.
    paint_cache: RefCell<cre_paint::PaintCache>,
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
    /// Builds a document from one in-memory HTML page, used to show the
    /// low-memory report instead of opening a book that cannot fit.
    pub fn from_html_page(
        html: &str,
        options: &DocumentOptions,
        font_datas: &[FontBlob],
    ) -> Result<Self> {
        let metadata = DocumentMetadata {
            format: DocumentFormat::Html,
            title: Some("Not enough memory".to_string()),
            creator: None,
            language: None,
            series: None,
            description: None,
            keywords: None,
            identifiers: None,
            item_count: 1,
        };
        Self::build(
            vec![html.to_string()],
            "",
            Arc::new(EmptyNet),
            ResourceSource::Empty,
            Arc::new(ImageRaster::new(options.width, options.height)),
            None,
            false,
            metadata,
            Vec::new(),
            String::new(),
            Vec::new(),
            options,
            font_datas,
            false,
            html.len() as u64,
            0,
        )
    }

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
        let source_bytes = std::fs::metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let format = detect_format(path);
        // SVG images are rendered at page size: CSS caps them to the page's
        // content box, so this is the largest they can ever be drawn.
        let content_width = ((options.width as f32 - options.page_margin * 2.0)
            * options.viewport_scale)
            .max(1.0) as u32;
        let content_height = ((options.height as f32 - options.page_margin * 2.0)
            * options.viewport_scale)
            .max(1.0) as u32;
        let svg_raster = Arc::new(ImageRaster::new(content_width, content_height));
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
                svg_raster.set_page_fit_images(std::mem::take(&mut book.page_fit_images));
                let (text_budget, image_budget, layout_budget) =
                    chunk_budgets(available_memory_bytes());
                let htmls = book.to_html_chunks_budgeted(
                    &html_options,
                    text_budget,
                    image_budget,
                    layout_budget,
                )?;
                // Decoded images dominate an illustrated book's footprint, and
                // they cannot be seen in the HTML size.
                let chunk_image_bytes = htmls
                    .iter()
                    .map(|html| cre_epub::chunk_image_bytes(&book.resource_provider, html))
                    .max()
                    .unwrap_or(0) as u64;
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
                    svg_raster.clone(),
                    cover_image,
                    has_non_linear_flows,
                    metadata,
                    raw_toc,
                    raw_page_map_source,
                    raw_page_map,
                    options,
                    font_datas,
                    defer_layout,
                    source_bytes,
                    chunk_image_bytes,
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
                let resource_source = ResourceSource::Map(binary.clone());
                Self::build(
                    split_single_document(html, &resource_source),
                    "fb2://book/",
                    Arc::new(MapNet {
                        resources: binary.clone(),
                        svg: svg_raster.clone(),
                    }),
                    resource_source,
                    svg_raster.clone(),
                    cover_image,
                    false,
                    metadata,
                    raw_toc,
                    String::new(),
                    Vec::new(),
                    options,
                    font_datas,
                    defer_layout,
                    source_bytes,
                    0,
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
                let resource_source = ResourceSource::Map(resources.clone());
                Self::build(
                    split_single_document(html, &resource_source),
                    "mobi://book/",
                    Arc::new(MapNet {
                        resources: resources.clone(),
                        svg: svg_raster.clone(),
                    }),
                    resource_source,
                    svg_raster.clone(),
                    cover_image,
                    false,
                    metadata,
                    raw_toc,
                    String::new(),
                    Vec::new(),
                    options,
                    font_datas,
                    defer_layout,
                    source_bytes,
                    0,
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
                let resource_source = ResourceSource::Map(resources.clone());
                Self::build(
                    split_single_document(html, &resource_source),
                    "azw3://book/",
                    Arc::new(MapNet {
                        resources: resources.clone(),
                        svg: svg_raster.clone(),
                    }),
                    resource_source,
                    svg_raster.clone(),
                    cover_image,
                    false,
                    metadata,
                    raw_toc,
                    String::new(),
                    Vec::new(),
                    options,
                    font_datas,
                    defer_layout,
                    source_bytes,
                    0,
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
                    split_single_document(html, &ResourceSource::Empty),
                    "text://book/",
                    Arc::new(EmptyNet),
                    ResourceSource::Empty,
                    Arc::new(ImageRaster::new(options.width, options.height)),
                    None,
                    false,
                    metadata,
                    Vec::new(),
                    String::new(),
                    Vec::new(),
                    options,
                    font_datas,
                    defer_layout,
                    source_bytes,
                    0,
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
                insert_style(
                    &mut html,
                    &format!(
                        "/* cre-render-base */\n\
                         html, body {{ margin: 0; padding: 0; }}\n\
                         body {{ font-family: serif; font-size: {}px; line-height: 1.2; \
                         padding: {:.3}px; }}\n\
                         img {{ max-width: 100%; height: auto; }}",
                        options.font_size,
                        page_margin_css.max(0.0)
                    ),
                );
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
                    split_single_document(html, &ResourceSource::Empty),
                    &base_url,
                    Arc::new(FileNet {
                        root: root.clone(),
                        svg: svg_raster.clone(),
                    }),
                    ResourceSource::Empty,
                    Arc::new(ImageRaster::new(options.width, options.height)),
                    None,
                    false,
                    metadata,
                    Vec::new(),
                    String::new(),
                    Vec::new(),
                    options,
                    font_datas,
                    defer_layout,
                    source_bytes,
                    0,
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
        images: Arc<ImageRaster>,
        cover_image: Option<Vec<u8>>,
        has_non_linear_flows: bool,
        metadata: DocumentMetadata,
        raw_toc: Vec<RawTocItem>,
        page_map_source: String,
        page_map_items: Vec<RawPageMapItem>,
        options: &DocumentOptions,
        font_datas: &[FontBlob],
        defer_layout: bool,
        source_bytes: u64,
        chunk_image_bytes: u64,
    ) -> Result<Self> {
        // A book that cannot fit is refused here, before the layout allocates
        // several copies of it; `cre-lua` turns the error into a page that
        // explains the shortfall instead of letting the reader be killed.
        if let Some(page) = memory_report::low_memory_page(
            &htmls,
            source_bytes,
            chunk_image_bytes,
            metadata.title.as_deref(),
        ) {
            return Err(anyhow::Error::new(LowMemory { page }));
        }
        let font_context = Arc::new(build_font_ctx_with_fallbacks(
            font_datas,
            options.font_family.as_deref(),
        ));
        // Only the EPUB generator emits chunk HTML without the layout settings
        // in it, which is what allows a relayout instead of a reopen.
        let layout_independent_sources = matches!(metadata.format, DocumentFormat::Epub);
        let fixed_layouts: Vec<Option<FixedLayout>> =
            htmls.iter().map(|html| parse_fixed_layout(html)).collect();
        // Only images an `<img>` element references may become a size
        // placeholder: a CSS background image has no other painter, and a
        // fixed-layout page draws its images through its own transform.
        if !matches!(resources, ResourceSource::Empty) {
            images.set_content_images(
                htmls
                    .iter()
                    .flat_map(|html| cre_epub::image_sources_in(html)),
            );
        }
        let mut document = Self {
            metadata,
            width: options.width,
            height: options.height,
            layout_height: options.layout_height,
            page_margin: options.page_margin,
            margins: PageMargins::uniform(options.page_margin),
            stylesheet: options.stylesheet.clone(),
            embedded_styles_enabled: options.embedded_styles_enabled,
            font_css_size: options.font_size,
            font_family: options.font_family.clone(),
            font_blobs: font_datas.to_vec(),
            viewport_scale: options.viewport_scale,
            font_gamma: 1.0,
            sources: {
                let started = std::time::Instant::now();
                let sources: Vec<ChunkSource> = htmls.into_iter().map(ChunkSource::new).collect();
                if std::env::var_os("CRE_LAYOUT_PROFILE").is_some() {
                    eprintln!(
                        "profile chunk html compress: {:.1}ms chunks={}",
                        started.elapsed().as_secs_f64() * 1000.0,
                        sources.len()
                    );
                }
                sources
            },
            fixed_layouts,
            base_url: base_url.to_string(),
            resources,
            images,
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
            paint_cache: RefCell::new(cre_paint::PaintCache::new()),
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
            page_margin: self.margins.top,
            font_size: self.font_css_size,
            font_path: None,
            font_family: self.font_family.clone(),
            fallback_font_paths: Vec::new(),
            stylesheet: self.stylesheet.clone(),
            embedded_styles_enabled: self.embedded_styles_enabled,
            viewport_scale: self.viewport_scale,
        }
    }

    /// The margins a chunk is laid out with: none for a pre-paginated page (it
    /// is fitted to the viewport), otherwise the reader's, or the uniform
    /// `page_margin` when the caller never configured them.
    fn chunk_margins(&self, chunk_index: usize) -> PageMargins {
        if self
            .fixed_layouts
            .get(chunk_index)
            .and_then(|fixed| *fixed)
            .is_some()
        {
            return PageMargins::uniform(0.0);
        }
        if self.margins.is_explicit() {
            return self.margins;
        }
        PageMargins::uniform(self.layout_options_for_chunk(chunk_index).page_margin)
    }

    fn layout_options_for_chunk(&self, chunk_index: usize) -> DocumentOptions {
        let mut options = self.layout_options();
        if let Some(fixed) = self.fixed_layouts.get(chunk_index).and_then(|fixed| *fixed) {
            let scale = self.viewport_scale.max(0.1);
            let (width, height) = self.fixed_size(fixed);
            options.width = (width * scale).round().max(1.0) as u32;
            options.height = (height * scale).round().max(1.0) as u32;
            options.layout_height = options.height;
            options.page_margin = 0.0;
        }
        options
    }

    fn reader_css_size(&self) -> (f32, f32) {
        let scale = self.viewport_scale.max(0.1);
        (self.width as f32 / scale, self.height as f32 / scale)
    }

    fn fixed_size(&self, fixed: FixedLayout) -> (f32, f32) {
        let (reader_width, reader_height) = self.reader_css_size();
        fixed.size(reader_width, reader_height)
    }

    /// CSS scale and center offsets for a pre-paginated page inside the reader
    /// viewport. Offsets are in CSS pixels.
    fn fixed_page_transform(&self, fixed: FixedLayout) -> (f32, f32, f32) {
        let scale = self.viewport_scale.max(0.1);
        let (reader_width, reader_height) = self.reader_css_size();
        let (page_width, page_height) = self.fixed_size(fixed);
        // A device-sized ICB is the whole reader viewport; page margins would
        // leave a blank border, so they are not applied here.
        let margin = if fixed == FixedLayout::Device {
            0.0
        } else {
            (self.page_margin / scale).max(0.0)
        };
        let available_width = (reader_width - margin * 2.0).max(1.0);
        let available_height = (reader_height - margin * 2.0).max(1.0);
        let fit = (available_width / page_width)
            .min(available_height / page_height)
            .max(0.01);
        let offset_x = (reader_width - page_width * fit) / 2.0;
        let offset_y = (reader_height - page_height * fit) / 2.0;
        (fit, offset_x, offset_y)
    }

    /// The FXL page box in device pixels: `(left, top, right, bottom)`.
    ///
    /// Blitz does not clip descendants of `overflow:hidden` boxes that are
    /// absolutely positioned, so a pre-paginated page whose content is larger
    /// than its ICB would otherwise paint outside the fixed page (the red
    /// rectangle in `lay-fxl-xhtml-icb_repeated-in-meta`). The renderer clips
    /// the scene to this rectangle instead.
    fn fixed_page_rect(&self, fixed: FixedLayout) -> (f32, f32, f32, f32) {
        let (fit, offset_x, offset_y) = self.fixed_page_transform(fixed);
        let (page_width, page_height) = self.fixed_size(fixed);
        let scale = self.viewport_scale.max(0.1);
        let left = offset_x * scale;
        let top = offset_y * scale;
        let right = left + page_width * fit * scale;
        let bottom = top + page_height * fit * scale;
        (left, top, right, bottom)
    }

    fn apply_options(&mut self, options: &DocumentOptions) {
        self.width = options.width;
        self.height = options.height;
        self.layout_height = options.layout_height;
        // Follow the options of the chunk being laid out: a fixed-layout chunk
        // zeroes its page margin, and the page transform reads this value.
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

    /// Sets the four page margins (device pixels) for the next layout.
    ///
    /// The reader configures left, top, right and bottom separately. Only the
    /// top one could be expressed before, so moving the left, right or bottom
    /// margin changed a setting without changing the page. Call this and then
    /// [`Self::relayout`].
    pub fn set_page_margins(&mut self, margins: PageMargins) {
        self.margins = PageMargins::new(margins.left, margins.top, margins.right, margins.bottom);
        self.page_margin = margins.top;
    }

    /// The page margins currently in use.
    pub fn page_margins(&self) -> PageMargins {
        self.margins
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
        // The cached glyph masks came from the previous faces.
        self.paint_cache.borrow_mut().clear();
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
        let decompress_started = std::time::Instant::now();
        let html = self
            .sources
            .get(chunk_index)
            .context("document chunk is missing")?
            .text()?;
        if std::env::var_os("CRE_LAYOUT_PROFILE").is_some() {
            eprintln!(
                "profile chunk {chunk_index} decompress: {:.2}ms ({}kB)",
                decompress_started.elapsed().as_secs_f64() * 1000.0,
                html.len() / 1024
            );
        }
        let fixed_height = self
            .fixed_layouts
            .get(chunk_index)
            .and_then(|fixed| *fixed)
            .map(|fixed| self.fixed_size(fixed).1);
        layout_chunk_html(
            &html,
            chunk_index,
            &self.base_url,
            self.net.clone(),
            self.font_context.clone(),
            &self.font_blobs,
            &self.layout_options_for_chunk(chunk_index),
            self.margins,
            fixed_height,
            &self.resources,
        )
    }
}

/// Lays out one chunk's HTML with the given settings.
///
/// Free-standing so the open-time pass can run several chunks at once: the
/// per-chunk work is independent, and only the parsed DOM is not `Send`, so the
/// parallel caller uses the returned layout and lets the worker drop the DOM.
#[allow(clippy::too_many_arguments)]
fn layout_chunk_html(
    html: &str,
    chunk_index: usize,
    base_url: &str,
    net: Arc<dyn NetProvider>,
    font_context: Arc<FontContext>,
    font_blobs: &[FontBlob],
    options: &DocumentOptions,
    margins: PageMargins,
    fixed_height: Option<f32>,
    resources: &ResourceSource,
) -> Result<(ChunkLayout, HtmlDocument, Vec<LineBox>, Vec<PageStart>)> {
    // A caller that never configured page margins keeps the uniform
    // `page_margin` of the options in use for this chunk (a fixed-layout chunk
    // zeroes it), and a pre-paginated page gets no margin at all: it is scaled
    // to the viewport instead.
    let margins = if fixed_height.is_some() {
        PageMargins::uniform(0.0)
    } else if margins.is_explicit() {
        margins
    } else {
        PageMargins::uniform(options.page_margin)
    };
    {
        let mut timer = StageTimer::new();
        timer.last = std::time::Instant::now();
        let fixed_layout = fixed_height.map(|_| true);
        // Images inside an inline `<svg>` are addressed relative to the
        // document; usvg only reads `data:` URIs, so inline them for the parse
        // (and only for the parse: the stored chunk HTML keeps the short href).
        // Every format goes through here, so a KF8 cover wrapped in `<svg>`
        // keeps its picture too.
        let inlined = |key: &str| -> Option<Arc<Vec<u8>>> {
            match resources {
                ResourceSource::Epub(provider) => provider.get(key),
                ResourceSource::Map(map) => map.get(key).map(|data| Arc::new(data.clone())),
                ResourceSource::Empty => None,
            }
        };
        // A section that holds nothing but a picture is a plate page: scale it
        // to the page. MOBI/KF8 are one document with `<section
        // class="cre-spine">` parts, so it has to be done here, per section.
        //
        // EPUB keeps its own version (`cre_epub::normalize_full_page_images`)
        // for two reasons: it rewrites the spine bytes before chunking, so the
        // chunk budgets already account for it, and it points the `<img>` at a
        // generated SVG *resource*. The SVG below is inlined instead, which
        // base64-copies the picture into the chunk HTML — fine for one cover,
        // wasteful for a book of plates. Run before the inliner so the
        // `<image>` this generates is inlined too.
        let fitted = (!matches!(resources, ResourceSource::Epub(_))).then(|| {
            let width = options.width as f32 - margins.left - margins.right;
            let height = options.height as f32 - margins.top - margins.bottom;
            let html = fit_cover_images(html, width, height);
            fit_picture_sections(&html, width, height)
        });
        let html = fitted.as_deref().unwrap_or(html);
        let inlined = cre_epub::inline_svg_images_to_data_uris(html, &inlined);
        let html = inlined.as_deref().unwrap_or(html);
        // Inline SVG needs the document's CSS and a pattern-capable rasterizer;
        // see `prepare_inline_svgs`. Text-only chunks skip the work entirely.
        let prepared_svg = contains_ignore_ascii_case(html, "<svg").then(|| {
            let epub_provider = match resources {
                ResourceSource::Epub(provider) => Some(provider.as_ref()),
                _ => None,
            };
            let css = document_css_for_svg(html, epub_provider);
            prepare_inline_svgs(html, &css, options.width)
        });
        let html = prepared_svg.as_deref().unwrap_or(html);
        let doc = create_document(
            html,
            base_url,
            net,
            font_context,
            font_blobs,
            options,
            margins,
        );
        timer.mark("parse");
        let root = doc.root_element();
        let mut lines = collect_lines(root);
        timer.mark("lines");
        let math_boxes = collect_math_boxes(&doc, root);
        extend_lines_for_math(&mut lines, &math_boxes);
        timer.mark("math");
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
        timer.mark("flows");
        let forced_breaks = page_breaks::collect_forced_breaks(&doc, root);
        let mut boxes = collect_content_boxes(&doc, root);
        timer.mark("boxes");
        for (top, bottom, display) in &math_boxes {
            if *display {
                boxes.push(ContentBox {
                    top: *top,
                    bottom: *bottom,
                });
            }
        }
        boxes.sort_by(|left, right| left.top.total_cmp(&right.top));
        let (page_height_css_u32, page_height_css, page_margin_css) =
            if let Some(height) = fixed_height {
                let height = height.max(1.0);
                (height as u32, height, 0.0)
            } else {
                css_page_geometry(
                    options.width,
                    options.height,
                    options.viewport_scale,
                    options.page_margin,
                )
            };
        let (_, _, page_margin_css_bottom) = css_page_geometry(
            options.width,
            options.height,
            options.viewport_scale,
            margins.bottom,
        );
        let page_starts = if fixed_layout.is_some() {
            vec![PageStart {
                top: 0.0,
                clip_top: 0.0,
            }]
        } else {
            paginate(
                &lines,
                &boxes,
                &forced_breaks,
                page_height_css,
                page_margin_css,
                page_margin_css_bottom,
            )
        };
        timer.mark("paginate");
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
        timer.mark("linepos");
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
        timer.mark("hrefs");
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
            let clip_bottom = page_chunk_clip_bottom(
                &page_starts,
                local_index,
                page_height_css,
                page_margin_css_bottom,
            );
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
        timer.mark("tail");
        timer.report(chunk_index);
        Ok((layout, doc, lines, page_starts))
    }
}

impl Document {
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
        let count = self.sources.len();
        let mut trim_guard = TrimGuard::new();
        if count == 0 {
            self.rebuild_view();
            return Ok(());
        }
        // Chunk 0 is the page the reader opens on, and it is laid out last so
        // its DOM becomes the first active chunk without being parsed twice.
        // Page numbering only depends on chunk indices, so the order the chunks
        // are laid out in is free.
        //
        // The last chunk goes first, as a probe: how much memory one chunk's
        // layout costs is only known once one has run, and that decides how
        // many chunks can be laid out at once on this device.
        // The probe is the chunk that looks most expensive to lay out: the one
        // with the most embedded images, then the largest HTML. That chunk has
        // to be laid out anyway, so measuring it costs nothing, and it keeps
        // the worker count from being decided by a cheap text chunk while an
        // image-heavy one waits behind it.
        // Chunk 0 is never the probe: when the pass runs on one thread its DOM
        // is kept from here to the first page, and laying the other chunks out
        // next to it would add a whole chunk's layout to the peak for nothing.
        let probe_index = (1..count)
            .max_by_key(|&index| {
                let source = &self.sources[index];
                (source.images, source.text_len)
            })
            .unwrap_or(0);
        let pass_started = std::time::Instant::now();
        let rss_before = resident_bytes();
        let (probe_layout, doc, lines, page_starts) = self.layout_chunk(probe_index)?;
        // Measured before the DOM is released: this is what one chunk's layout
        // costs while it is resident.
        let rss_after = resident_bytes();
        self.entries[probe_index] = ChunkEntry {
            layout: Some(probe_layout),
            stale: false,
        };
        // When the probe is the opening chunk (a cover, usually the most
        // image-heavy chunk of all) its DOM is what the first page needs, so
        // keep it instead of dropping it and parsing it again.
        let mut opening = if probe_index == 0 {
            Some((doc, lines, page_starts))
        } else {
            drop(doc);
            drop(lines);
            drop(page_starts);
            None
        };
        trim_memory();
        let workers = layout_worker_count(
            count.saturating_sub(2),
            self.sources
                .iter()
                .map(|source| source.text_len)
                .max()
                .unwrap_or(0),
            rss_before,
            rss_after,
        );
        if std::env::var_os("CRE_LAYOUT_PROFILE").is_some() {
            eprintln!(
                "profile workers={workers} probe_chunk={probe_index} rss_before={}MB rss_after={}MB",
                rss_before.map(|bytes| bytes / (1024 * 1024)).unwrap_or(0),
                rss_after.map(|bytes| bytes / (1024 * 1024)).unwrap_or(0)
            );
        }
        if workers > 1 {
            // Several chunks at once: the per-chunk work is independent, so
            // this is what the open-time pass otherwise leaves on the table.
            // Options are cheap to build here because `Document` itself is not
            // `Send` (it holds `RefCell` caches), so the workers only get the
            // pieces they need.
            let options: Vec<DocumentOptions> = (0..count)
                .map(|index| self.layout_options_for_chunk(index))
                .collect();
            let fixed_heights: Vec<Option<f32>> = (0..count)
                .map(|index| {
                    self.fixed_layouts
                        .get(index)
                        .and_then(|fixed| *fixed)
                        .map(|fixed| self.fixed_size(fixed).1)
                })
                .collect();
            let results: std::sync::Mutex<Vec<Option<ChunkLayout>>> =
                std::sync::Mutex::new((0..count).map(|_| None).collect());
            let failure: std::sync::Mutex<Option<anyhow::Error>> = std::sync::Mutex::new(None);
            let stop = std::sync::atomic::AtomicBool::new(false);
            // Workers take every chunk except the probe and the opening one.
            let next = std::sync::atomic::AtomicUsize::new(1);
            let gate = LayoutMemoryGate::new();
            let chunk_margins: Vec<PageMargins> =
                (0..count).map(|index| self.chunk_margins(index)).collect();
            let work = ChunkLayoutWork {
                sources: &self.sources,
                base_url: &self.base_url,
                net: self.net.clone(),
                font_context: self.font_context.clone(),
                font_blobs: &self.font_blobs,
                options: &options,
                fixed_heights: &fixed_heights,
                results: &results,
                failure: &failure,
                stop: &stop,
                next: &next,
                gate: &gate,
                probe_index,
                count,
                resources: &self.resources,
                margins: &chunk_margins,
            };
            std::thread::scope(|scope| {
                for _ in 1..workers {
                    scope.spawn(|| work.run());
                }
                work.run();
            });
            // The workers' chunks are gone, but glibc keeps their arenas. Trim
            // before the opening chunk is laid out, so that memory is returned
            // to the kernel instead of being pinned by the DOM we must keep.
            trim_memory();
            // The opening chunk is laid out afterwards, on this thread: its DOM
            // stays resident for the first page, so laying it out while the
            // workers are still running would hold it next to theirs.
            let opening = match opening.take() {
                Some(active) => active,
                None => {
                    let (layout, doc, lines, page_starts) = work.layout(0)?;
                    self.entries[0] = ChunkEntry {
                        layout: Some(layout),
                        stale: false,
                    };
                    (doc, lines, page_starts)
                }
            };
            if let Some(error) = failure.into_inner().unwrap() {
                return Err(error);
            }
            for (index, layout) in results.into_inner().unwrap().into_iter().enumerate() {
                if let Some(layout) = layout {
                    self.entries[index] = ChunkEntry {
                        layout: Some(layout),
                        stale: false,
                    };
                }
            }
            let (doc, lines, page_starts) = opening;
            self.rebuild_view();
            trim_memory();
            self.install_active(0, doc, lines, page_starts);
            if std::env::var_os("CRE_LAYOUT_PROFILE").is_some() {
                eprintln!(
                    "profile layout pass: {:.1}ms chunks={count}",
                    pass_started.elapsed().as_secs_f64() * 1000.0
                );
            }
            return Ok(());
        }
        for chunk_index in 1..count {
            if chunk_index == probe_index {
                continue;
            }
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
            trim_guard.maybe_trim();
        }
        let (doc, lines, page_starts) = match opening.take() {
            Some(active) => active,
            None => {
                let (layout, doc, lines, page_starts) = self.layout_chunk(0)?;
                self.entries[0] = ChunkEntry {
                    layout: Some(layout),
                    stale: false,
                };
                (doc, lines, page_starts)
            }
        };
        self.rebuild_view();
        trim_memory();
        self.install_active(0, doc, lines, page_starts);
        if std::env::var_os("CRE_LAYOUT_PROFILE").is_some() {
            eprintln!(
                "profile layout pass: {:.1}ms chunks={count}",
                pass_started.elapsed().as_secs_f64() * 1000.0
            );
        }
        Ok(())
    }

    pub fn metadata(&self) -> &DocumentMetadata {
        &self.metadata
    }

    pub fn page_count(&self) -> usize {
        self.pages.len().max(1)
    }

    /// What the document keeps resident, for diagnostics (`perf_probe`).
    ///
    /// The active chunk's DOM/layout is not counted here: it belongs to Blitz
    /// and Parley, and is measured from the process RSS instead.
    pub fn memory_breakdown(&self) -> MemoryBreakdown {
        let mut breakdown = MemoryBreakdown {
            chunk_count: self.sources.len(),
            page_count: self.pages.len(),
            font_bytes: self
                .font_blobs
                .iter()
                .map(|blob| blob.as_ref().as_ref().len())
                .sum(),
            glyph_cache_bytes: self.paint_cache.borrow().cached_bytes(),
            ..MemoryBreakdown::default()
        };
        for source in &self.sources {
            breakdown.source_bytes += source.text_len;
            breakdown.compressed_source_bytes += source.compressed.len();
        }
        for entry in &self.entries {
            let Some(layout) = entry.layout.as_ref() else {
                continue;
            };
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
        match &self.resources {
            ResourceSource::Epub(provider) => {
                breakdown.resource_cache_bytes = provider.cache_stats().1;
                breakdown.pinned_bytes = provider.pinned_bytes();
            }
            ResourceSource::Map(map) => {
                breakdown.resource_cache_bytes = map.values().map(Vec::len).sum();
            }
            ResourceSource::Empty => {}
        }
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
        lookup_resource(&self.resources, key).map(|data| (*data).clone())
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
            Ok(find_image_data_at(
                &active.doc,
                root,
                page_bottom,
                position_x,
                position_y,
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
        let profile = std::env::var_os("CRE_RENDER_PROFILE").is_some();
        let call_started = std::time::Instant::now();
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
            let fixed_layout = self.fixed_layouts.get(chunk_index).and_then(|fixed| *fixed);
            let mut scene = Scene::default();
            let mut placed: Vec<cre_paint::PlacedImage> = Vec::new();
            if let Some(fixed) = fixed_layout {
                active.doc.set_viewport_scroll(Point { x: 0.0, y: 0.0 });
                let (fit, offset_x, offset_y) = self.fixed_page_transform(fixed);
                let scale = viewport_scale * fit;
                blitz_paint::paint_scene(
                    &mut scene,
                    &mut active.doc,
                    scale as f64,
                    self.width,
                    self.height,
                    (offset_x * viewport_scale).round().max(0.0) as u32,
                    (offset_y * viewport_scale).round().max(0.0) as u32,
                );
                {
                    let root = active.doc.root_element();
                    collect_placed_images(
                        &active.doc,
                        root,
                        f32::MAX,
                        viewport_scale * fit,
                        offset_x * viewport_scale,
                        offset_y * viewport_scale,
                        &self.images,
                        &self.resources,
                        &self.base_url,
                        &mut placed,
                    );
                }
                let (clip_left, clip_top, clip_right, clip_bottom) = self.fixed_page_rect(fixed);
                let (image_arc, paint_count, paint_surface) = paint_page(
                    &scene,
                    self.width,
                    self.height,
                    0.0,
                    clip_left,
                    clip_right,
                    clip_top,
                    clip_bottom,
                    self.background_color,
                    self.font_gamma,
                    &placed,
                );
                (
                    image_arc,
                    paint_count + placed.len(),
                    paint_surface + placed_image_area(&placed, self.width, self.height),
                )
            } else {
                let record_started = std::time::Instant::now();
                active.doc.set_viewport_scroll(Point {
                    x: 0.0,
                    y: f64::from(start.top),
                });
                // Images the page only carries as a size placeholder are not in
                // the scene; decode their originals for this paint.
                {
                    let root = active.doc.root_element();
                    collect_placed_images(
                        &active.doc,
                        root,
                        clip_bottom,
                        viewport_scale,
                        0.0,
                        0.0,
                        &self.images,
                        &self.resources,
                        &self.base_url,
                        &mut placed,
                    );
                }
                blitz_paint::paint_scene(
                    &mut scene,
                    &mut active.doc,
                    viewport_scale as f64,
                    self.width,
                    self.height,
                    0,
                    0,
                );
                if std::env::var_os("CRE_RENDER_PROFILE").is_some() {
                    eprintln!(
                        "render profile page {page}: record={:.2}ms commands={}",
                        record_started.elapsed().as_secs_f64() * 1000.0,
                        scene.commands.len()
                    );
                }
                let local_top = clip_top * viewport_scale;
                let local_bottom = clip_bottom * viewport_scale;
                let paint_started = std::time::Instant::now();
                let (image_arc, paint_count, paint_surface) = paint_page_cached(
                    &scene,
                    self.width,
                    self.height,
                    0.0,
                    0.0,
                    self.width as f32,
                    local_top,
                    local_bottom,
                    self.background_color,
                    self.font_gamma,
                    &placed,
                    &mut self.paint_cache.borrow_mut(),
                );
                let image_count = paint_count + placed.len();
                let image_surface =
                    paint_surface + placed_image_area(&placed, self.width, self.height);
                if std::env::var_os("CRE_RENDER_PROFILE").is_some() {
                    eprintln!(
                        "render profile page {page}: paint={:.2}ms",
                        paint_started.elapsed().as_secs_f64() * 1000.0
                    );
                }
                (image_arc, image_count, image_surface)
            }
        };
        let image = PageImage {
            bitmap: bitmap.clone(),
            image_count,
            image_surface,
        };
        let out = (bitmap.clone(), image_count, image_surface);
        self.cache_page_image(page, image);
        if profile {
            eprintln!(
                "  render total page {page}: {:.2}ms rss={}kB",
                call_started.elapsed().as_secs_f64() * 1000.0,
                resident_bytes().unwrap_or(0) / 1024
            );
        }
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
        let fixed_layout = self.fixed_layouts.get(chunk_index).and_then(|fixed| *fixed);
        let mut scene = Scene::default();
        let mut placed: Vec<cre_paint::PlacedImage> = Vec::new();
        let (clip_left, clip_right, local_top, local_bottom) = if let Some(fixed) = fixed_layout {
            active.doc.set_viewport_scroll(Point { x: 0.0, y: 0.0 });
            let (fit, offset_x, offset_y) = self.fixed_page_transform(fixed);
            let scale = viewport_scale * fit;
            blitz_paint::paint_scene(
                &mut scene,
                &mut active.doc,
                scale as f64,
                self.width,
                self.height,
                (offset_x * viewport_scale).round().max(0.0) as u32,
                (offset_y * viewport_scale).round().max(0.0) as u32,
            );
            {
                let root = active.doc.root_element();
                collect_placed_images(
                    &active.doc,
                    root,
                    f32::MAX,
                    viewport_scale * fit,
                    offset_x * viewport_scale,
                    offset_y * viewport_scale,
                    &self.images,
                    &self.resources,
                    &self.base_url,
                    &mut placed,
                );
            }
            let (left, top, right, bottom) = self.fixed_page_rect(fixed);
            (left, right, top, bottom)
        } else {
            active.doc.set_viewport_scroll(Point {
                x: 0.0,
                y: f64::from(start.top),
            });
            {
                let root = active.doc.root_element();
                collect_placed_images(
                    &active.doc,
                    root,
                    clip_bottom,
                    viewport_scale,
                    0.0,
                    0.0,
                    &self.images,
                    &self.resources,
                    &self.base_url,
                    &mut placed,
                );
            }
            blitz_paint::paint_scene(
                &mut scene,
                &mut active.doc,
                viewport_scale as f64,
                self.width,
                self.height,
                0,
                0,
            );
            (
                0.0,
                self.width as f32,
                clip_top * viewport_scale,
                clip_bottom * viewport_scale,
            )
        };
        let (bitmap, image_count, image_surface) = paint_page_rgba(
            &scene,
            self.width,
            self.height,
            0.0,
            clip_left,
            clip_right,
            local_top,
            local_bottom,
            self.background_color,
            self.font_gamma,
            &placed,
        );
        Ok((
            bitmap,
            image_count + placed.len(),
            image_surface + placed_image_area(&placed, self.width, self.height),
        ))
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

    /// Installs an already parsed chunk as the active one, skipping the second
    /// parse `ensure_chunk` would do.
    fn install_active(
        &self,
        index: usize,
        doc: HtmlDocument,
        lines: Vec<LineBox>,
        page_starts: Vec<PageStart>,
    ) {
        let rendered_text_nodes = rendered_text_nodes(&doc, &lines);
        *self.active.borrow_mut() = Some(ActiveChunk {
            index,
            rendered_text_nodes,
            doc,
            lines,
            page_starts,
        });
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
        let html = stored.text()?;
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
            &self.font_blobs,
            &self.layout_options_for_chunk(index),
            self.chunk_margins(index),
        );
        let mut lines = collect_lines(doc.root_element());
        let math_boxes = collect_math_boxes(&doc, doc.root_element());
        extend_lines_for_math(&mut lines, &math_boxes);
        let forced_breaks = page_breaks::collect_forced_breaks(&doc, doc.root_element());
        let mut boxes = collect_content_boxes(&doc, doc.root_element());
        for (top, bottom, display) in &math_boxes {
            if *display {
                boxes.push(ContentBox {
                    top: *top,
                    bottom: *bottom,
                });
            }
        }
        boxes.sort_by(|left, right| left.top.total_cmp(&right.top));
        let fixed_layout = self.fixed_layouts.get(index).and_then(|fixed| *fixed);
        let (_page_height_css_u32, page_height_css, page_margin_css) =
            if let Some(fixed) = fixed_layout {
                let height = self.fixed_size(fixed).1.max(1.0);
                (height as u32, height, 0.0)
            } else {
                css_page_geometry(
                    self.width,
                    self.height,
                    self.viewport_scale,
                    self.page_margin,
                )
            };
        let page_starts = if fixed_layout.is_some() {
            vec![PageStart {
                top: 0.0,
                clip_top: 0.0,
            }]
        } else {
            paginate(
                &lines,
                &boxes,
                &forced_breaks,
                page_height_css,
                page_margin_css,
                page_margin_css,
            )
        };
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
        let lines = self.page_text_lines(page)?;
        Ok(join_text_boxes(lines.iter().map(|(_, text_box, line)| {
            (text_box.text.as_str(), *line)
        })))
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
            let (position_x, position_y) =
                if let Some(fixed) = self.fixed_layouts.get(chunk_index).and_then(|fixed| *fixed) {
                    let (fit, offset_x, offset_y) = self.fixed_page_transform(fixed);
                    let scale = self.viewport_scale.max(0.1);
                    (
                        (position.x / scale - offset_x) / fit,
                        (position.y / scale - offset_y) / fit,
                    )
                } else {
                    (position.x, position.y)
                };
            let local_x = position_x - origin.x;
            let local_y = position_y + page_top - origin.y;

            let byte_offset = node
                .text_offset_at_point(local_x, local_y)
                .map_or(0, |offset| offset.saturating_sub(line.text_start));
            let max_offset = line.text_end.saturating_sub(line.text_start);
            let byte_offset = byte_offset.min(max_offset).min(inline_layout.text.len());

            // Ask parley for the word under the point, the same way a text
            // editor does (`Selection::word_from_point`), instead of deriving
            // the boundaries from clusters ourselves. The range is relative to
            // the block's layout text; the caller wants it relative to the line.
            let word_range = {
                let scale = inline_layout.layout.scale();
                let selection = ParleySelection::word_from_point(
                    &inline_layout.layout,
                    local_x * scale,
                    (local_y - inline_layout.block_offset) * scale,
                );
                let range = selection.text_range();
                let line_start = line.text_start;
                let line_end = line.text_end;
                let start = range.start.max(line_start).saturating_sub(line_start);
                let end = range.end.min(line_end).saturating_sub(line_start);
                (start < end).then_some((start, end))
            };
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
                word_range,
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
        let parley_range = hit.word_range.or_else(|| {
            self.parley_word_byte_range(hit.page, hit.line_index, hit.byte_offset)
                .unwrap_or(None)
        });
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
        let mut previous: Option<PageTextLine> = None;
        for page in start_hit.page..=end_hit.page {
            for (line_index, mut rect, current) in self.page_text_lines(page)? {
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
                        if !text.is_empty() && !continues_same_line(previous, current) {
                            text.push('\n');
                        }
                        text.push_str(part);
                        previous = Some(current);
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
        // `pos` is a document-space y (see `current_document_pos`), i.e. the
        // top of a page. Staying on the current page when the position falls
        // inside it keeps the reader's frequent `gotoPos(getCurrentPos())`
        // refresh from moving; anything else is looked up by page start, and
        // pages whose start is still provisional (0) are skipped so a lookup
        // never jumps past real pages.
        let current = self.current_page.max(1).min(page_count);
        let start = self.page_start_y(current);
        let height = self.page_render_height(current).max(0);
        if pos >= start && pos < start + height {
            return Ok(current);
        }
        if pos <= 0 {
            return self.go_to_page(1);
        }
        let mut page = 1usize;
        for candidate in 2..=page_count {
            let candidate_start = self.page_start_y(candidate);
            if candidate_start <= 0 {
                continue;
            }
            if candidate_start <= pos {
                page = candidate;
            } else {
                break;
            }
        }
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
        Ok(self
            .page_text_lines(page)?
            .into_iter()
            .map(|(global_line, text_box, _)| (global_line, text_box))
            .collect())
    }

    /// Text boxes with the visual line they sit on and whether they belong to a
    /// formula, so a formula's tokens can be joined into one run of text.
    fn page_text_lines(&self, page: usize) -> Result<Vec<(usize, TextBox, PageTextLine)>> {
        let Some(index) = page.checked_sub(1) else {
            bail!("page numbers are 1-based");
        };
        if self.pages.get(index).is_none() {
            bail!("page {page} is outside 1..={}", self.page_count());
        }

        let snapshot = self.page_snapshot_boxes(page)?;
        let scale = self.viewport_scale;
        let fixed_layout = self
            .fixed_layouts
            .get(self.pages[index].chunk)
            .and_then(|fixed| *fixed);
        let fixed = fixed_layout.map(|fixed| self.fixed_page_transform(fixed));
        Ok(snapshot
            .iter()
            .map(|line| {
                let (x, y, width, height) = if let Some((fit, offset_x, offset_y)) = fixed {
                    (
                        offset_x + line.x * fit,
                        offset_y + line.y * fit,
                        line.width * fit,
                        line.height * fit,
                    )
                } else {
                    (line.x, line.y, line.width, line.height)
                };
                (
                    line.global_line,
                    TextBox {
                        text: line.text.clone(),
                        page,
                        x: x * scale,
                        y: y * scale,
                        width: width * scale,
                        height: height * scale,
                    },
                    PageTextLine {
                        line_top: line.line_top * scale,
                        math: line.math,
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

fn lookup_resource(resources: &ResourceSource, key: &str) -> Option<Arc<Vec<u8>>> {
    match resources {
        ResourceSource::Empty => None,
        ResourceSource::Map(map) => map.get(key).map(|bytes| Arc::new(bytes.clone())),
        // The provider already hands out `Arc`s; copying the bytes here made
        // every page render clone the whole (compressed) picture.
        ResourceSource::Epub(provider) => provider.get(key),
    }
}

fn find_image_data_at(
    doc: &HtmlDocument,
    node: &Node,
    page_bottom: f32,
    x: f32,
    y: f32,
    base_url: &str,
    resources: &ResourceSource,
) -> Option<Vec<u8>> {
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
        if hit {
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
                return Some((*data).clone());
            }
        }
    }

    for child in node.children.iter() {
        if let Some(data) = find_image_data_at(
            doc,
            node.with(*child),
            page_bottom,
            x,
            y,
            base_url,
            resources,
        ) {
            return Some(data);
        }
    }
    None
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
        // A plain oversized image is a size placeholder (painted with `fill`);
        // a generated page-fit wrapper is a transparent page-sized SVG whose
        // real picture is painted inside it with `contain`.
        let placed = images
            .placeheld_size(key)
            .and_then(|(width, height)| {
                lookup_resource(resources, key)
                    .map(|bytes| (bytes, width, height, cre_paint::ImageFit::Fill))
            })
            .or_else(|| {
                let fit = images.page_fit_image(key)?;
                let bytes = lookup_resource(resources, &fit.source)?;
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
    html_source = cre_epub::rewrite_ruby(&html_source);
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
            "<style>/* cre-page-margins */ html, body {{ padding: 0 {margin_right_css:.3}px 0 \
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

fn collect_spine_chunks(
    node: &Node,
    chunk_index: usize,
    out: &mut HashMap<u32, usize>,
    real: &mut HashMap<u32, u32>,
) {
    if let Some(index) =
        node_attr(node, "data-spine-index").and_then(|value| value.parse::<u32>().ok())
    {
        // XPointers number sections with `data-spine-index + 1` (see
        // `xpointer::spine_index_for_node`), so key the chunks the same way:
        // mixing the two numbering schemes sent lookups for the last section
        // of a chunk to the *next* chunk, where the section does not exist.
        let slot = index + 1;
        out.entry(slot).or_insert(chunk_index);
        let real_index = node_attr(node, "data-spine-real-index")
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(slot);
        real.entry(slot).or_insert(real_index);
    }
    for child in &node.children {
        collect_spine_chunks(node.with(*child), chunk_index, out, real);
    }
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

/// Gives the book's cover image a page of its own.
///
/// MOBI and KF8 put the cover at the top of the first chapter section, inline
/// with the title text; `cre-mobi`/`cre-kf8` mark that `<img>` with
/// `data-cre-cover` (it is the EXTH cover record). A page-tall inline image
/// makes its line box taller than the page, which pushed the whole first
/// paragraph onto the second page and left page one blank, and its
/// `max-height` cap kept it smaller than the screen. Replacing it with a
/// block-level page-sized SVG makes it fill the page exactly and moves the
/// following text to the next page, like a cover should.
fn fit_cover_images(html: &str, width: f32, height: f32) -> String {
    // The cover is the first picture of the first part, with nothing before it:
    // MOBI/KF8 writers put it at the top of chapter one, inline with the title.
    // It has to be a tall picture (its `height` attribute is the only size we
    // have at this point), otherwise a publisher logo on a title page would
    // become a page of its own.
    if !contains_ignore_ascii_case(html, "<img") {
        return html.to_string();
    }
    let lower = html.to_ascii_lowercase();
    // The stylesheet also mentions `.cre-spine`, so look at the tags.
    let mut scan = 0usize;
    let mut open_end = None;
    while let Some(pos) = lower[scan..].find("<section") {
        let start = scan + pos;
        let Some(end) = html[start..].find('>').map(|offset| start + offset + 1) else {
            break;
        };
        if html[start..end].to_ascii_lowercase().contains("cre-spine") {
            open_end = Some(end);
            break;
        }
        scan = end;
    }
    let Some(open_end) = open_end else {
        return html.to_string();
    };
    let Some(picture_start) = lower[open_end..].find("<img").map(|pos| open_end + pos) else {
        return html.to_string();
    };
    let Some(picture_end) = html[picture_start..]
        .find('>')
        .map(|end| picture_start + end + 1)
    else {
        return html.to_string();
    };
    // Anything but whitespace before the picture means this is a normal page
    // that happens to start with an image.
    let before = text_without_tags(&html[open_end..picture_start]);
    if !before.trim().is_empty() {
        return html.to_string();
    }
    let tag = &html[picture_start..picture_end];
    let Some(src) = attr_value(tag, "src") else {
        return html.to_string();
    };
    let Some(declared_height) = attr_value(tag, "height").and_then(parse_pixels) else {
        return html.to_string();
    };
    if declared_height < height * 0.5 {
        return html.to_string();
    }
    let (width, height) = (width.max(1.0).round(), height.max(1.0).round());
    let mut out = String::with_capacity(html.len() + 256);
    out.push_str(&html[..picture_start]);
    out.push_str(&page_picture_svg(&src, width, height, true));
    out.push_str(&html[picture_end..]);
    out
}

/// The text of an HTML fragment with its tags removed.
fn text_without_tags(html: &str) -> String {
    let mut text = String::new();
    let mut depth = 0usize;
    for ch in html.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => text.push(ch),
            _ => {}
        }
    }
    text
}

/// The value of `name` in one tag, without the quotes.
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let at = lower.find(name)?;
    let rest = tag[at + name.len()..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let end = rest[1..].find(quote)? + 1;
    Some(rest[1..end].to_string())
}

/// A length attribute as pixels, ignoring its unit (`987`, `987px`).
fn parse_pixels(value: String) -> Option<f32> {
    let digits: String = value
        .trim()
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    digits.parse::<f32>().ok()
}

/// The page-sized SVG that shows `picture` at page size without cropping.
fn page_picture_svg(picture: &str, width: f32, height: f32, block: bool) -> String {
    let display = if block { "display:block;" } else { "" };
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {width:.0} {height:.0}\" \
         preserveAspectRatio=\"xMidYMid meet\" style=\"{display}width:100%;height:auto\">\
         <image x=\"0\" y=\"0\" width=\"{width:.0}\" height=\"{height:.0}\" \
         preserveAspectRatio=\"xMidYMid meet\" href=\"{picture}\"/></svg>"
    )
}

/// Scales a picture-only section to the page.
///
/// MOBI and KF8 keep the whole book in one document and mark each part with
/// `<section class="cre-spine">`. A part whose only content is an image (a
/// cover, a plate) should fill the page the way an EPUB spine document that is
/// one picture does; a plain `<img>` is capped to its own size by
/// `max-width: 100%`, so it sat small in the middle of the page. The picture is
/// wrapped in a page-sized SVG with `preserveAspectRatio="… meet"`, so it is
/// enlarged and never cropped.
fn fit_picture_sections(html: &str, width: f32, height: f32) -> String {
    if !contains_ignore_ascii_case(html, "<img") || !contains_ignore_ascii_case(html, "cre-spine") {
        return html.to_string();
    }
    static SRC: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let src = SRC.get_or_init(|| {
        regex::Regex::new(r#"(?is)\bsrc\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
            .expect("img src regex is valid")
    });
    let (width, height) = (width.max(1.0).round(), height.max(1.0).round());
    let closing = "</section>";
    let mut out = String::with_capacity(html.len() + 256);
    // `cursor` is how far the output has been written, `scan` how far the
    // search has got; a section that is left alone only moves `scan`, so its
    // content is copied with the surrounding text.
    let mut cursor = 0usize;
    let mut scan = 0usize;
    let mut changed = false;
    let lower = html.to_ascii_lowercase();
    while let Some(pos) = lower[scan..].find("<section") {
        let start = scan + pos;
        let Some(open_end) = html[start..].find('>').map(|end| start + end + 1) else {
            break;
        };
        if !html[start..open_end]
            .to_ascii_lowercase()
            .contains("cre-spine")
        {
            scan = open_end;
            continue;
        }
        let Some(close) = lower[open_end..].find(closing).map(|end| open_end + end) else {
            break;
        };
        let end = close + closing.len();
        let Some(picture) = picture_only_body(&html[open_end..close], src) else {
            scan = end;
            continue;
        };
        out.push_str(&html[cursor..open_end]);
        out.push_str(&page_picture_svg(&picture, width, height, true));
        // Keep the section's own closing tag.
        out.push_str(&html[close..end]);
        cursor = end;
        scan = end;
        changed = true;
    }
    if !changed {
        return html.to_string();
    }
    out.push_str(&html[cursor..]);
    out
}

/// The `src` of the single image `body` holds, when that is all it holds.
fn picture_only_body(body: &str, src: &regex::Regex) -> Option<String> {
    let lower = body.to_ascii_lowercase();
    if lower.matches("<img").count() != 1 {
        return None;
    }
    let start = lower.find("<img")?;
    let end = body[start..].find('>').map(|offset| start + offset + 1)?;
    let captures = src.captures(&body[start..end])?;
    let picture = captures
        .get(1)
        .or_else(|| captures.get(2))?
        .as_str()
        .trim()
        .to_string();
    if picture.is_empty() {
        return None;
    }
    // Anything left after removing the image (tags excluded) is text, so the
    // section is a normal page with a picture in it.
    let rest = format!("{}{}", &body[..start], &body[end..]);
    text_without_tags(&rest)
        .trim()
        .is_empty()
        .then_some(picture)
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
        let encoded = cre_epub::base64_encode(&png);
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
fn document_css_for_svg(html: &str, provider: Option<&LazyResource>) -> String {
    if !contains_ignore_ascii_case(html, "<style") && provider.is_none() {
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

    let Some(provider) = provider else {
        return css;
    };
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
        let Some(path) = href_value.strip_prefix("epub://book/") else {
            continue;
        };
        let key = normalize_path(&percent_decode_str(path).decode_utf8_lossy());
        if let Some(bytes) = provider.get(&key) {
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
    page_fit: std::sync::Mutex<HashMap<String, cre_epub::PageFitImage>>,
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

    /// Images the renderer paints itself from the original resource.
    fn placeheld_count(&self) -> usize {
        self.placeheld.lock().map(|held| held.len()).unwrap_or(0)
    }

    /// Bytes held by the page-sized SVG raster cache.
    fn cached_bytes(&self) -> usize {
        self.cache.lock().map(|cache| cache.bytes).unwrap_or(0)
    }

    /// Records the transparent page-fit wrappers generated at open.
    fn set_page_fit_images(&self, images: HashMap<String, cre_epub::PageFitImage>) {
        if let Ok(mut page_fit) = self.page_fit.lock() {
            page_fit.extend(images);
        }
    }

    /// The original image a page-fit wrapper paints, with its real size.
    fn page_fit_image(&self, key: &str) -> Option<cre_epub::PageFitImage> {
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

struct EpubNet {
    resource_provider: Arc<LazyResource>,
    svg: Arc<ImageRaster>,
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
        // A document's `<body class="…">` becomes a `<section class="…">` in
        // the chunk, so the stylesheet's `body.class` rules have to lose the
        // `body` part to keep matching (a cover's `body.cov2 { background }`).
        let bytes = if key.to_ascii_lowercase().ends_with(".css") {
            cre_epub::body_selectors_to_class(&String::from_utf8_lossy(&bytes)).into_bytes()
        } else {
            bytes
        };
        let bytes = self.svg.rasterize(&key, &bytes).unwrap_or(bytes);
        let bytes = self.svg.placeholder(&key, &bytes).unwrap_or(bytes);
        handler.bytes(request.url.to_string(), Bytes::from(bytes));
    }
}

/// Serves resources held in memory (FB2 binaries, MOBI/KF8 images), under
/// `binary/<name>` or a bare `<name>`.
struct MapNet {
    resources: Arc<HashMap<String, Vec<u8>>>,
    svg: Arc<ImageRaster>,
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
        let bytes = self.svg.placeholder(&name, &bytes).unwrap_or(bytes);
        handler.bytes(request.url.to_string(), Bytes::from(bytes));
    }
}

/// Serves resources referenced by a standalone HTML document from the
/// document's own directory. Paths are resolved and canonicalized, and must
/// stay inside that directory.
struct FileNet {
    root: PathBuf,
    svg: Arc<ImageRaster>,
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
        let bytes = self.svg.placeholder(&decoded, &bytes).unwrap_or(bytes);
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
mod picture_section_tests {
    use super::*;

    #[test]
    fn documents_without_a_picture_section_are_untouched() {
        let html = r#"<html><body><section class="cre-spine"><p>text</p><p><img src="mobi-image-1"/></p></section><section class="cre-spine"><p>more</p></section></body></html>"#;
        assert_eq!(fit_picture_sections(html, 600.0, 800.0), html);
    }

    #[test]
    fn a_picture_only_section_is_wrapped_in_a_page_svg() {
        let html = r#"<html><body><p>intro</p><section class="cre-spine"><p><img src="mobi-image-1"/></p></section><section class="cre-spine"><p>after</p></section></body></html>"#;
        let out = fit_picture_sections(html, 600.0, 800.0);
        assert!(out.contains("<p>intro</p>"), "{out}");
        assert!(out.contains("viewBox=\"0 0 600 800\""), "{out}");
        assert!(out.contains("href=\"mobi-image-1\""), "{out}");
        assert!(out.contains("<p>after</p>"), "{out}");
        // The section keeps its tags and the document keeps its tail.
        assert!(out.ends_with("</section></body></html>"), "{out}");
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

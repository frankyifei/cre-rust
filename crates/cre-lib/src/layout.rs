//! Laying a chunk out: the one place a `Document` turns markup into pages.
//!
//! `layout_chunk_html` parses a chunk's HTML and paginates it,
//! `ChunkLayoutWork` runs that on a worker thread under `LayoutMemoryGate`, and
//! `Document` exposes exactly these ways in: `ensure_page_layout`,
//! `ensure_xpointer_layout`, `ensure_layout_complete`, `ensure_layout`,
//! `relayout` for a settings change, and `drain_background_layouts` to install
//! what the worker finished. Everything else here is private to this module, so
//! the rest of the crate cannot lay a chunk out by accident - the compiler
//! rejects it.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use blitz_dom::NodeId;
use blitz_html::HtmlDocument;
use blitz_traits::net::NetProvider;
use parley::FontContext;

use cre_book::ResourceSource;
use cre_book::chunk::available_memory_bytes;
use cre_book::images::fit_cover_images;

use crate::page_breaks;
use crate::xpointer;
use crate::{
    ActiveChunk, BackgroundLayout, CHUNK_DOM_BYTES_PER_HTML_BYTE, ChunkLayout, ChunkSource,
    ContentBox, Document, DocumentOptions, FontBlob, LAYOUT_MEMORY_RESERVE, LayoutPhase, LineBox,
    LineEntry, LocalPage, PageMargins, PageStart, StageTimer, collect_anchor_maps,
    collect_content_boxes, collect_line_flows, collect_lines, collect_math_boxes,
    contains_ignore_ascii_case, create_document, css_page_geometry, document_css_for_svg,
    extend_lines_for_math, first_non_blank_line_flow, page_chunk_clip_bottom, page_for_node,
    page_for_spine_node, paginate, parse_text_position, prepare_inline_svgs, rendered_text_nodes,
    resident_bytes, trim_memory,
};

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

/// One layout pass over the book's chunks: the inputs every chunk layout
/// needs, plus the two memory safeguards.
///
/// Every pass - the parallel open, a single-threaded open, the background
/// worker, and the synchronous whole-book completion - goes through here, so the
/// safeguards cannot drift apart between them. A pass must drop a chunk's DOM
/// and line boxes as soon as the page metadata is extracted, and trim the
/// arenas they leave behind; the background pass once did neither, which is the
/// pass that runs longest on a device.
struct ChunkLayoutPass {
    sources: Arc<[ChunkSource]>,
    base_url: String,
    net: Arc<dyn NetProvider>,
    font_context: Arc<FontContext>,
    font_blobs: Vec<FontBlob>,
    resources: ResourceSource,
    /// Whether the book's own `@font-face` fonts may be used.
    embedded_fonts: bool,
    /// Per-chunk settings, resolved once: a fixed-layout chunk zeroes its page
    /// margin and has its own page height.
    options: Vec<DocumentOptions>,
    margins: Vec<PageMargins>,
    fixed_heights: Vec<Option<f32>>,
    /// Keeps concurrent layouts inside the memory the chunk planner assumed.
    gate: LayoutMemoryGate,
    trim: std::sync::Mutex<TrimGuard>,
}

impl ChunkLayoutPass {
    /// Resolves everything a pass needs from the document, once.
    fn new(document: &Document, count: usize) -> Self {
        Self {
            sources: Arc::clone(&document.sources),
            base_url: document.base_url.clone(),
            net: document.net.clone(),
            font_context: Arc::clone(&document.font_context),
            font_blobs: document.font_blobs.clone(),
            resources: document.resources.clone(),
            embedded_fonts: document
                .embedded_fonts_flag
                .load(std::sync::atomic::Ordering::SeqCst),
            options: (0..count)
                .map(|index| document.layout_options_for_chunk(index))
                .collect(),
            margins: (0..count)
                .map(|index| document.chunk_margins(index))
                .collect(),
            fixed_heights: (0..count)
                .map(|index| {
                    document
                        .fixed_layouts
                        .get(index)
                        .and_then(|fixed| *fixed)
                        .map(|fixed| document.fixed_size(fixed).1)
                })
                .collect(),
            gate: LayoutMemoryGate::new(),
            trim: std::sync::Mutex::new(TrimGuard::new()),
        }
    }

    /// Lays one chunk out and keeps its DOM, for a caller about to draw from it.
    fn layout(
        &self,
        index: usize,
    ) -> Result<(ChunkLayout, HtmlDocument, Vec<LineBox>, Vec<PageStart>)> {
        layout_chunk_html(
            &self.sources[index].text()?,
            index,
            &self.base_url,
            self.net.clone(),
            self.font_context.clone(),
            &self.font_blobs,
            &self.options[index],
            self.margins[index],
            self.fixed_heights[index],
            &self.resources,
            self.embedded_fonts,
        )
    }

    /// Lays one chunk out for the page list and keeps nothing else.
    ///
    /// This is the shape every pass uses: wait for memory, lay out, keep the
    /// page metadata, release the DOM and line boxes, trim what they left.
    fn layout_pages(&self, index: usize) -> Result<ChunkLayout> {
        let mut trim = self
            .trim
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.layout_pages_trimmed(index, &mut trim)
    }

    /// [`Self::layout_pages`] with the caller's own trim guard.
    ///
    /// The parallel pass gives every worker a guard, as it always has: the guard
    /// measures growth against its own last trim, so one guard per worker trims
    /// more often than a shared one and keeps the open-time peak measurably
    /// lower (about 7 MB on a four-worker pass). The single-threaded passes have
    /// nothing to share, so they use the one inside the pass.
    fn layout_pages_trimmed(&self, index: usize, trim: &mut TrimGuard) -> Result<ChunkLayout> {
        self.gate.enter();
        let laid_out = self.layout(index);
        self.gate.leave();
        let (layout, doc, lines, page_starts) = laid_out?;
        // The DOM belongs to the pass; only the page metadata has to survive.
        drop(doc);
        drop(lines);
        drop(page_starts);
        trim.maybe_trim();
        Ok(layout)
    }
}

/// Coordination of the parallel open-time pass: the chunk list is pulled from a
/// shared cursor, so the opening chunk can be laid out by the main thread while
/// the workers take the rest.
struct ChunkLayoutWork<'a> {
    pass: &'a ChunkLayoutPass,
    results: &'a std::sync::Mutex<Vec<Option<ChunkLayout>>>,
    failure: &'a std::sync::Mutex<Option<anyhow::Error>>,
    stop: &'a std::sync::atomic::AtomicBool,
    next: &'a std::sync::atomic::AtomicUsize,
    probe_index: usize,
    count: usize,
}

impl ChunkLayoutWork<'_> {
    fn run(&self) {
        // One guard per worker, so the trims are as frequent as they were
        // before the passes shared their per-chunk work.
        let mut trim = TrimGuard::new();
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
            match self.pass.layout_pages_trimmed(index, &mut trim) {
                Ok(layout) => self.results.lock().unwrap()[index] = Some(layout),
                Err(error) => {
                    *self.failure.lock().unwrap() = Some(error);
                    self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
            }
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
/// One chunk's layout costs roughly `CHUNK_DOM_BYTES_PER_HTML_BYTE` per byte
/// of its generated HTML, and the pass holds that many DOMs at the same time,
/// so the count is bounded by the free memory the chunk planner saw (half of
/// it, leaving room for the page bitmap and the reader). It is also capped at
/// four: past that the pass is bound by memory bandwidth, not cores.
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
    let estimated = largest_html.saturating_mul(CHUNK_DOM_BYTES_PER_HTML_BYTE);
    let observed = rss_after.saturating_sub(rss_before.unwrap_or(rss_after)) as usize;
    let per_chunk = estimated.max(observed).max(1);
    let headroom = usable.saturating_sub(rss_after as usize);
    (headroom / per_chunk).clamp(1, cores.min(4)) as usize
}

impl Document {
    fn apply_options(&mut self, options: &DocumentOptions) {
        self.width = options.width;
        self.height = options.height;
        self.layout_height = options.layout_height;
        // Follow the options of the chunk being laid out: a fixed-layout chunk
        // zeroes its page margin, and the page transform reads this value.
        self.page_margin = options.page_margin;
        self.font_css_size = options.font_size;
        // A served stylesheet resolves its absolute `font-size` keywords
        // against this, so a font change must reach the net provider before the
        // chunk is laid out again.
        self.css_font_size.store(
            options.font_size.to_bits(),
            std::sync::atomic::Ordering::Relaxed,
        );
        if self.font_family != options.font_family {
            self.font_family = options.font_family.clone();
            self.rebuild_font_context();
        }
        self.stylesheet = options.stylesheet.clone();
        self.embedded_styles_enabled = options.embedded_styles_enabled;
        self.viewport_scale = options.viewport_scale;
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
            self.embedded_fonts_flag
                .load(std::sync::atomic::Ordering::Relaxed),
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
    embedded_fonts: bool,
) -> Result<(ChunkLayout, HtmlDocument, Vec<LineBox>, Vec<PageStart>)> {
    // A book's own `@font-face` fonts are fetched by Blitz while it parses the
    // chunk; dropping the rules is therefore the way to ignore them, and a
    // `font-family` that no longer resolves falls back to the next one in the
    // list (the reader's font).
    let html = if embedded_fonts {
        std::borrow::Cow::Borrowed(html)
    } else {
        cre_book::strip_font_faces(html)
    };
    let html = html.as_ref();
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
        let inlined = |key: &str| -> Option<Arc<Vec<u8>>> { resources.get(key) };
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
        let fitted = (!resources.is_archive()).then(|| {
            let width = options.width as f32 - margins.left - margins.right;
            let height = options.height as f32 - margins.top - margins.bottom;
            fit_cover_images(html, width, height)
        });
        let html = fitted.as_deref().unwrap_or(html);
        let inlined = cre_book::images::inline_svg_images_to_data_uris(html, &inlined);
        let html = inlined.as_deref().unwrap_or(html);
        // Inline SVG needs the document's CSS and a pattern-capable rasterizer;
        // see `prepare_inline_svgs`. Text-only chunks skip the work entirely.
        let prepared_svg = contains_ignore_ascii_case(html, "<svg").then(|| {
            let css = document_css_for_svg(html, resources);
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
            line_count: lines.len(),
            max_flow,
        };
        timer.mark("tail");
        timer.report(chunk_index);
        Ok((layout, doc, lines, page_starts))
    }
}
impl Document {
    /// Opens a big book with only its first chunk laid out.
    ///
    /// The full open-time pass costs one layout per chunk before the first page
    /// is shown, which is most of the "opening" a reader waits for on a large
    /// book. Here the opening chunk is laid out (its DOM stays active, so the
    /// first page draws without parsing it again), every other chunk stands in
    /// with a page estimate, and the background worker lays them out in reading
    /// order. The reader's position is restored by XPointer, which
    /// [`Self::ensure_xpointer_layout`] turns into "lay out that one chunk", so
    /// the content is right immediately; the page numbers are estimates until
    /// the pass ends (cre-lua reports that through `hasPendingRerendering`, and
    /// KOReader's plugin refreshes page totals and the TOC then).
    fn begin_lazy_open(&mut self) -> Result<()> {
        self.phase = LayoutPhase::Complete;
        let count = self.sources.len();
        self.page_table.reset(count);
        if count == 0 {
            self.rebuild_view();
            return Ok(());
        }
        let (layout, doc, lines, page_starts) = self.layout_chunk(0)?;
        let opening_pages = layout.pages.len();
        self.page_table.install(0, layout);
        self.estimate_provisional_pages(opening_pages);
        self.rebuild_view();
        trim_memory();
        self.install_active(0, doc, lines, page_starts);
        self.start_background_layout();
        Ok(())
    }

    /// Fills in the page count assumed for every chunk that has no layout yet,
    /// from the pages the opening chunk really got.
    ///
    /// Pages are roughly proportional to the HTML a chunk holds, so the opening
    /// chunk's bytes-per-page scales the rest; a pre-paginated chunk is always
    /// one page. The number only has to be close: it decides the page index, the
    /// total and the progress bar until the worker replaces it, and the reader
    /// is positioned by XPointer, not by page number.
    fn estimate_provisional_pages(&mut self, opening_pages: usize) {
        let bytes_per_page = if opening_pages > 0 {
            self.sources[0].text_len as f32 / opening_pages as f32
        } else {
            0.0
        };
        // Under a few hundred bytes per page the opening chunk is nearly empty
        // (a cover, a single image): too little to scale a whole book from.
        let bytes_per_page = if bytes_per_page >= 256.0 {
            bytes_per_page
        } else {
            2048.0
        };
        for index in 0..self.page_table.len() {
            if self.page_table.layout(index).is_some() {
                continue;
            }
            let pages = if self
                .fixed_layouts
                .get(index)
                .and_then(|fixed| *fixed)
                .is_some()
            {
                1
            } else {
                let pages = self.sources[index].text_len as f32 / bytes_per_page;
                pages.round().max(1.0) as usize
            };
            self.page_table.set_provisional(index, pages);
        }
    }

    /// Lays out `chunk_index` and keeps its DOM active, ready to draw.
    fn lay_out_chunk_now(&mut self, chunk_index: usize) -> Result<()> {
        let (layout, doc, lines, page_starts) = self.layout_chunk(chunk_index)?;
        self.page_table.install(chunk_index, layout);
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
            .min(self.page_table.pages().len().saturating_sub(1));
        let (anchor_chunk, anchor_local) = match self.page_table.pages().get(anchor) {
            Some(page) => (
                page.chunk,
                anchor + 1 - self.page_table.first_page(page.chunk),
            ),
            None => (0, 0),
        };
        self.apply_options(options);
        // Every chunk has to be laid out for the new settings. A chunk that
        // already has a layout keeps it as a provisional page list until it is
        // laid out again; a chunk that never had one keeps its estimate.
        self.page_table.mark_all_stale();
        *self.page_boxes_cache.borrow_mut() = None;
        self.clear_page_image_cache();
        let old_anchor_pages = self
            .page_table
            .layout(anchor_chunk)
            .map(|layout| layout.pages.len())
            .unwrap_or(0);
        self.lay_out_chunk_now(anchor_chunk)?;
        // The chunk that was laid out says what the new settings do to a page
        // count; apply that ratio to the chunks that keep their old pages, so the
        // total the reader is shown is close to the truth until it settles.
        if old_anchor_pages > 0 {
            let new_anchor_pages = self
                .page_table
                .layout(anchor_chunk)
                .map(|layout| layout.pages.len())
                .unwrap_or(0);
            if new_anchor_pages > 0 {
                let ratio = new_anchor_pages as f32 / old_anchor_pages as f32;
                if (ratio - 1.0).abs() > 0.02 {
                    self.page_table.resample_stale(ratio);
                }
            }
        }
        self.rebuild_view();
        let first_page = self.page_table.first_page(anchor_chunk);
        let page_count = self
            .page_table
            .layout(anchor_chunk)
            .map(|layout| layout.pages.len())
            .unwrap_or(1);
        self.current_page = first_page + anchor_local.min(page_count.saturating_sub(1));
        // `can_relayout_with` guarantees a pass has started, so this is the
        // live one (Complete or Completing) and ends up Complete, cancelling a
        // worker that still belongs to the old settings.
        self.stop_background_layout();
        trim_memory();
        // The current chunk is laid out, so the page under the reader is
        // usable; the rest are laid out in the background, in reading order,
        // and installed as they finish.
        self.start_background_layout();
        Ok(true)
    }

    /// Lays the remaining stale chunks out on a worker thread, in reading
    /// order, and hands each layout back over a channel.
    ///
    /// The worker owns a [`ChunkLayoutPass`] (sources, fonts, net, resources,
    /// settings and the memory safeguards), so the document can keep rendering
    /// pages; `Document` itself is not `Send` and is never touched by the
    /// worker.
    fn start_background_layout(&mut self) {
        // Testing knob: keep the old behaviour (only the chunk under the reader
        // is laid out) so the two can be compared on a device.
        if std::env::var_os("CRE_NO_BACKGROUND_LAYOUT").is_some() {
            return;
        }
        let indices: Vec<usize> = self.page_table.pending();
        if indices.is_empty() {
            return;
        }
        // A pass that is still running belongs to the settings that were in
        // force when it started, so it goes now. `stop` leaves the book
        // `Complete`; the new pass replaces that below.
        self.phase.stop();
        let cancel = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let cancel_source = Arc::clone(&cancel);
        // The pass owns everything the worker needs; `Document` itself is not
        // `Send`, and both memory safeguards travel with the pass.
        let pass = ChunkLayoutPass::new(self, self.sources.len());
        let total = indices.len();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for &index in &indices {
                if cancel_source.load(std::sync::atomic::Ordering::SeqCst) != 0 {
                    break;
                }
                let Ok(layout) = pass.layout_pages(index) else {
                    break;
                };
                if sender.send((index, layout)).is_err() {
                    break;
                }
            }
        });
        self.phase = LayoutPhase::Completing(BackgroundLayout {
            total,
            done: 0,
            receiver,
            cancel,
        });
    }

    /// Installs the layouts the background worker has finished.
    ///
    /// The reader's position is kept on the same chunk and local page, so the
    /// page numbers can move under it without the content jumping. Returns
    /// whether the page table changed.
    pub fn drain_background_layouts(&mut self) -> bool {
        let mut received = Vec::new();
        let mut finished = false;
        let none_left;
        {
            let LayoutPhase::Completing(job) = &mut self.phase else {
                return false;
            };
            loop {
                match job.receiver.try_recv() {
                    Ok(item) => received.push(item),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                }
            }
            none_left = finished || job.done >= job.total;
        }
        // Remember where the reader is by (chunk, local page): the page numbers
        // of the earlier chunks change as their layouts are installed.
        let anchor = self
            .page_table
            .pages()
            .get(self.current_page.saturating_sub(1))
            .map(|page| {
                (
                    page.chunk,
                    self.current_page
                        .saturating_sub(self.page_table.first_page(page.chunk)),
                )
            });
        let mut installed = 0usize;
        for (index, layout) in received {
            if !self.page_table.needs_layout(index) {
                continue;
            }
            self.page_table.install(index, layout);
            installed += 1;
        }
        if installed == 0 {
            if none_left {
                self.phase = LayoutPhase::Complete;
            }
            return false;
        }
        self.rebuild_view();
        if let Some((chunk, local)) = anchor {
            let first = self.page_table.first_page(chunk);
            let count = self
                .page_table
                .layout(chunk)
                .map(|layout| layout.pages.len())
                .unwrap_or(1);
            self.current_page = first + local.min(count.saturating_sub(1));
        }
        // KOReader polls this to notice that page numbers changed and to put
        // the reader back on its XPointer (ReaderRolling:handlePartialRerendering).
        self.background_installs = self.background_installs.saturating_add(installed as i64);
        let mut stop = false;
        if let LayoutPhase::Completing(job) = &mut self.phase {
            job.done += installed;
            stop = finished || job.done >= job.total;
        }
        if stop {
            self.phase = LayoutPhase::Complete;
        }
        true
    }

    /// Lays out `page`'s chunk if it still holds a provisional page list.
    ///
    /// Returns the page index to use: a chunk whose page count changes moves the
    /// pages after it, including the requested page.
    pub fn ensure_page_layout(&mut self, page: usize) -> Result<usize> {
        let Some(index) = page.checked_sub(1) else {
            return Ok(page);
        };
        let Some(snapshot) = self.page_table.pages().get(index) else {
            return Ok(page);
        };
        let chunk = snapshot.chunk;
        if !self.page_table.needs_layout(chunk) {
            return Ok(page);
        }
        let old_head = self.page_table.first_page(chunk);
        let old_count = self
            .page_table
            .layout(chunk)
            .map(|layout| layout.pages.len())
            .unwrap_or(0);
        let local = page.saturating_sub(old_head);
        self.lay_out_chunk_now(chunk)?;
        self.rebuild_view();
        let new_count = self
            .page_table
            .layout(chunk)
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
        if self.page_table.needs_layout(chunk) {
            let head = self.page_table.first_page(chunk);
            let _ = self.ensure_page_layout(head)?;
        }
        Ok(())
    }

    /// Lays out every chunk that still lacks a layout for the current settings,
    /// synchronously.
    ///
    /// Whole-book queries that cannot work from estimates call this: full-text
    /// search walks every page. It is exactly the background pass's work, so it
    /// is only worth calling when the answer has to be complete right now.
    pub fn ensure_layout_complete(&mut self) -> Result<()> {
        let missing: Vec<usize> = self.page_table.pending();
        if missing.is_empty() {
            return Ok(());
        }
        let anchor = self.current_anchor();
        let pass = ChunkLayoutPass::new(self, self.sources.len());
        for index in missing {
            // Only the page metadata is needed here; the DOM of the chunk that
            // is on screen stays the active one.
            let layout = pass.layout_pages(index)?;
            self.page_table.install(index, layout);
        }
        self.rebuild_view();
        if let Some(anchor) = anchor {
            self.restore_anchor(anchor);
        }
        Ok(())
    }

    /// Lays out the book, building the page list, TOC page numbers and page
    /// map. Cheap to call more than once.
    ///
    /// This is where the open decides between the full pass and the lazy one:
    /// the reader parses on `loadDocument` and lays out on `renderDocument`
    /// (cre-lua's deferred open), so a decision taken in the constructor would
    /// never run there.
    pub fn ensure_layout(&mut self) -> Result<()> {
        if self.phase.started() {
            return Ok(());
        }
        if self.prefers_lazy_open() {
            return self.begin_lazy_open();
        }
        self.phase = LayoutPhase::Complete;
        self.page_table.reset(self.sources.len());
        let count = self.sources.len();
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
        // Everything the pass needs, resolved once: options, margins, page
        // heights and the two memory safeguards.
        let pass = ChunkLayoutPass::new(self, count);
        let pass_started = std::time::Instant::now();
        let rss_before = resident_bytes();
        let (probe_layout, doc, lines, page_starts) = pass.layout(probe_index)?;
        // Measured before the DOM is released: this is what one chunk's layout
        // costs while it is resident.
        let rss_after = resident_bytes();
        self.page_table.install(probe_index, probe_layout);
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
            let results: std::sync::Mutex<Vec<Option<ChunkLayout>>> =
                std::sync::Mutex::new((0..count).map(|_| None).collect());
            let failure: std::sync::Mutex<Option<anyhow::Error>> = std::sync::Mutex::new(None);
            let stop = std::sync::atomic::AtomicBool::new(false);
            // Workers take every chunk except the probe and the opening one.
            let next = std::sync::atomic::AtomicUsize::new(1);
            let work = ChunkLayoutWork {
                pass: &pass,
                results: &results,
                failure: &failure,
                stop: &stop,
                next: &next,
                probe_index,
                count,
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
                    let (layout, doc, lines, page_starts) = pass.layout(0)?;
                    self.page_table.install(0, layout);
                    (doc, lines, page_starts)
                }
            };
            if let Some(error) = failure.into_inner().unwrap() {
                return Err(error);
            }
            for (index, layout) in results.into_inner().unwrap().into_iter().enumerate() {
                if let Some(layout) = layout {
                    self.page_table.install(index, layout);
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
            // Lays the chunk out and releases its DOM; the page metadata is all
            // the page list needs.
            let layout = pass.layout_pages(chunk_index)?;
            self.page_table.install(chunk_index, layout);
        }
        let (doc, lines, page_starts) = match opening.take() {
            Some(active) => active,
            None => {
                let (layout, doc, lines, page_starts) = pass.layout(0)?;
                self.page_table.install(0, layout);
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
}

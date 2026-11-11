//! Low-memory diagnostics.
//!
//! Opening a book keeps the chunk text (one LZ4 block per chunk), one chunk's
//! decompressed HTML and DOM, and the page index for the chunks laid out so
//! far. On a 256 MB reader that can exceed what the kernel can hand out, and
//! the reader process is then killed. Before the layout starts we can compare
//! the book's measured size against the memory the device reports, and when it
//! cannot fit, show a page that explains why.

use std::fmt;

use crate::{
    CHUNK_DOM_BYTES_PER_HTML_BYTE, ChunkSource, LAYOUT_MEMORY_RESERVE, layout_reserve,
    memory_available_detail, memory_total_bytes, resident_bytes,
};

/// Bytes a page needs in the page index (line entries, page metadata and the
/// first-line XPointer). A line entry is a node id, a byte offset and a page
/// number (24 B); a laid-out page keeps 19-27 of them, plus ~24 B of page
/// metadata and ~40 B of XPointer, i.e. 550-700 B. Measured whole-book
/// layouts: 25.1 MB of English text became 16 808 pages and ~11.4 MB of index,
/// 24.9 MB of Chinese text 15 445 pages and ~8.1 MB.
const PAGE_INDEX_BYTES: u64 = 700;

/// Average bytes of generated HTML per page of text, used only to estimate how
/// many pages the index will hold before the layout ran.
///
/// It depends on the layout settings and the book's markup: at 758x1024 and
/// 22 px, 25.1 MB of English plain text became 16 808 pages (1494 B/page) and
/// 24.9 MB of Chinese text 15 557 pages (1608 B/page), while a tightly set
/// 7.6 MB book took 3030 B/page. 2000 B is a middle value that stays within
/// ~30% of the large books, after the old 3000 B under-estimated plain text by
/// up to a factor of two.
const HTML_BYTES_PER_PAGE: u64 = 2000;

/// Generated HTML per byte of source XHTML: the section wrappers and the head
/// add a little. Repacking `zyj.epub` into a 24 MB text book gave 24.0 MB of
/// XHTML and 24.9 MB of generated HTML (1.04); 1.10 is the upper bound here.
const HTML_PER_SOURCE_BYTE: f64 = 1.10;

/// LZ4 size per byte of generated HTML. Real prose compresses to 0.54-0.57;
/// 0.62 leaves room so an early refusal is never a close call.
const LZ4_PER_HTML_BYTE: f64 = 0.62;

/// The diagnostic page itself is smaller than this; it is never re-checked.
const DIAGNOSTIC_PAGE_LIMIT: usize = 64 * 1024;

/// A document that cannot be laid out on this device, with the page that
/// explains why. `cre-lua` turns it into a document so the reader sees the
/// message instead of a killed process.
#[derive(Debug)]
pub struct LowMemory {
    pub page: String,
}

impl fmt::Display for LowMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not enough memory to open this document")
    }
}

impl std::error::Error for LowMemory {}

/// What a book is expected to cost, and what the device has.
pub(crate) struct MemoryReport {
    pub total: Option<u64>,
    pub available: Option<u64>,
    pub available_source: &'static str,
    pub resident: Option<u64>,
    pub source_bytes: u64,
    /// Spine XHTML size, when the report was estimated before parsing.
    pub source_text_bytes: u64,
    /// Generated HTML before compression, used to estimate the page count.
    pub html_bytes: u64,
    /// Bytes the chunk text actually keeps resident: one LZ4 block per chunk.
    pub stored_bytes: u64,
    pub chunks: usize,
    /// Uncompressed size of the chunk laid out at any one time.
    pub largest_chunk: u64,
    pub dom_bytes: u64,
    /// Decoded images of the heaviest chunk (they live next to its DOM).
    pub image_bytes: u64,
    pub pages: u64,
    pub index_bytes: u64,
    pub reserve: u64,
    pub need: u64,
    /// True when the figures come from the spine size instead of measurement.
    pub estimated: bool,
}

impl MemoryReport {
    pub(crate) fn fits(&self) -> bool {
        match self.available {
            Some(available) => self.need <= available,
            // Without a memory figure, trust the device rather than denying
            // the book.
            None => true,
        }
    }
}

/// Estimates what laying `sources` out costs and what is available.
///
/// The chunk HTML is stored as one LZ4 block per chunk and decompressed one
/// chunk at a time, so the text costs `stored_bytes`, not its uncompressed
/// size; only the largest chunk is counted for the DOM, because that is the
/// only one laid out (and decoded) at a time. The page index stays resident for
/// the whole book. Compressing the chunks before this check makes the stored
/// figure exact instead of a ratio estimate.
pub(crate) fn measure(
    sources: &[ChunkSource],
    source_bytes: u64,
    image_bytes: u64,
) -> MemoryReport {
    let mut html_bytes = 0u64;
    let mut stored_bytes = 0u64;
    let mut largest_chunk = 0u64;
    for source in sources {
        html_bytes += source.text_len as u64;
        stored_bytes += source.compressed.len() as u64;
        largest_chunk = largest_chunk.max(source.text_len as u64);
    }
    let chunks = sources.len();
    // One chunk's DOM at a time: it is dropped as soon as the next chunk is
    // laid out, and the chunk planner already sized it to fit.
    let dom_bytes = largest_chunk.saturating_mul(CHUNK_DOM_BYTES_PER_HTML_BYTE as u64);
    let pages: u64 = html_bytes / HTML_BYTES_PER_PAGE.max(1);
    let index_bytes = pages.saturating_mul(PAGE_INDEX_BYTES);
    let (available, available_source) = match memory_available_detail() {
        Some((bytes, source)) => (Some(bytes), source),
        None => (None, "unavailable"),
    };
    let reserve = available
        .map(|bytes| layout_reserve(bytes as usize) as u64)
        .unwrap_or(LAYOUT_MEMORY_RESERVE as u64);
    // `stored_bytes` is not part of `need`: the chunks were compressed before
    // this check, so their LZ4 blocks are already part of this process (and
    // already missing from `available`). Counting them again made a book that
    // needs ~30 MB look like it needs ~44 MB.
    MemoryReport {
        total: memory_total_bytes(),
        available,
        available_source,
        resident: resident_bytes(),
        source_bytes,
        source_text_bytes: 0,
        html_bytes,
        stored_bytes,
        chunks,
        largest_chunk,
        dom_bytes,
        image_bytes,
        pages,
        index_bytes,
        reserve,
        need: dom_bytes + image_bytes + index_bytes + reserve,
        estimated: false,
    }
}

/// An estimated refusal, before the book is parsed.
///
/// The exact check needs the whole book decompressed, converted to chunk HTML,
/// split and compressed first, which is seconds of waiting before the page
/// appears on a Kindle. The spine's XHTML size is in the zip directory, so a
/// book that cannot possibly fit can be refused immediately.
///
/// The ratios are upper bounds and images are not counted, so only an estimate
/// clearly over the device's memory becomes a refusal; a borderline book still
/// gets the exact check after compression.
pub(crate) fn preflight(
    spine_bytes: u64,
    source_bytes: u64,
    text_budget: usize,
    title: Option<&str>,
) -> Option<String> {
    if spine_bytes == 0 || std::env::var_os("CRE_IGNORE_LOW_MEMORY").is_some() {
        return None;
    }
    let (available, available_source) = memory_available_detail()?;
    let force = std::env::var_os("CRE_FORCE_LOW_MEMORY").is_some();
    preflight_with(
        spine_bytes,
        source_bytes,
        text_budget,
        available,
        available_source,
        title,
        force,
    )
}

#[allow(clippy::too_many_arguments)]
fn preflight_with(
    spine_bytes: u64,
    source_bytes: u64,
    text_budget: usize,
    available: u64,
    available_source: &'static str,
    title: Option<&str>,
    force: bool,
) -> Option<String> {
    let html_bytes = (spine_bytes as f64 * HTML_PER_SOURCE_BYTE) as u64;
    let stored_bytes = (html_bytes as f64 * LZ4_PER_HTML_BYTE) as u64;
    // Chunks are filled up to the text budget, so the largest DOM that will
    // exist is one budget's worth.
    let largest_chunk = text_budget as u64;
    let dom_bytes = largest_chunk.saturating_mul(CHUNK_DOM_BYTES_PER_HTML_BYTE as u64);
    let pages = html_bytes / HTML_BYTES_PER_PAGE.max(1);
    let index_bytes = pages.saturating_mul(PAGE_INDEX_BYTES);
    let reserve = layout_reserve(available as usize) as u64;
    // Nothing is allocated yet, so the compressed text counts here (unlike the
    // exact check, which runs after compression).
    let need = stored_bytes + dom_bytes + index_bytes + reserve;
    // The ratios above over-state the result by roughly 10%; allow another
    // 10% before refusing, so a book that would only just fit is measured
    // exactly instead of turned away by a guess.
    if !force && need <= available.saturating_mul(12) / 10 {
        return None;
    }
    let chunks = (html_bytes / largest_chunk.max(1)).max(1) as usize;
    let report = MemoryReport {
        total: memory_total_bytes(),
        available: Some(available),
        available_source,
        resident: resident_bytes(),
        source_bytes,
        source_text_bytes: spine_bytes,
        html_bytes,
        stored_bytes,
        chunks,
        largest_chunk,
        dom_bytes,
        image_bytes: 0,
        pages,
        index_bytes,
        reserve,
        need,
        estimated: true,
    };
    Some(render_page(&report, title))
}

/// The page that explains that `sources` does not fit, or `None` when it does.
pub(crate) fn low_memory_page(
    sources: &[ChunkSource],
    source_bytes: u64,
    image_bytes: u64,
    title: Option<&str>,
) -> Option<String> {
    // The diagnostic page itself is tiny; never run the check on it, or a
    // device with almost no memory left would loop on it.
    if let [only] = sources
        && only.text_len <= DIAGNOSTIC_PAGE_LIMIT
    {
        return None;
    }
    // Testing knob: let a desktop open a book the device would refuse, so the
    // real peak can be measured with `CRE_AVAILABLE_MB`.
    if std::env::var_os("CRE_IGNORE_LOW_MEMORY").is_some() {
        return None;
    }
    let report = measure(sources, source_bytes, image_bytes);
    if report.fits() && std::env::var_os("CRE_FORCE_LOW_MEMORY").is_none() {
        return None;
    }
    Some(render_page(&report, title))
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

fn kb(bytes: u64) -> String {
    format!("{:.0} KB", bytes as f64 / 1024.0)
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn row(label: &str, value: &str) -> String {
    format!(
        "<tr><td>{}</td><td class=\"value\">{}</td></tr>",
        escape(label),
        escape(value)
    )
}

fn render_page(report: &MemoryReport, title: Option<&str>) -> String {
    let total = report.total.map(mb).unwrap_or_else(|| "-".to_string());
    let available = report.available.map(mb).unwrap_or_else(|| "-".to_string());
    let resident = report.resident.map(mb).unwrap_or_else(|| "-".to_string());
    let title = title.map(escape).unwrap_or_else(|| "-".to_string());
    let shortfall = report
        .available
        .map(|available| report.need.saturating_sub(available))
        .unwrap_or(0);

    let mut html = String::with_capacity(4096);
    html.push_str(
        "<html><head><title>Not enough memory</title><style>\
h1 { font-size: 125%; font-weight: bold; margin: 0 0 0.2em 0; }\
h2 { font-size: 102%; font-weight: bold; margin: 1.1em 0 0.2em 0; }\
p { margin: 0.45em 0; }\
table { border-collapse: collapse; width: 100%; }\
td { padding: 0.12em 0 0.12em 0; vertical-align: top; }\
td.value { text-align: right; }\
.small { font-size: 88%; }\
</style></head><body>",
    );
    html.push_str("<h1>Not enough memory to open this book</h1>");

    html.push_str("<h2>Device</h2><table>");
    html.push_str(&row("Total memory", &total));
    html.push_str(&row(
        "Available",
        &format!("{available} ({})", report.available_source),
    ));
    html.push_str(&row("This process", &resident));
    html.push_str("</table>");

    html.push_str("<h2>This book</h2><table>");
    html.push_str(&row("Title", &title));
    html.push_str(&row("File size", &mb(report.source_bytes)));
    if report.estimated {
        html.push_str(&row(
            "Text (XHTML, from the zip)",
            &mb(report.source_text_bytes),
        ));
    } else {
        html.push_str(&row(
            "Generated HTML",
            &format!(
                "{} ({} chunks, largest {})",
                mb(report.html_bytes),
                report.chunks,
                kb(report.largest_chunk)
            ),
        ));
    }
    html.push_str(&row("Text stored (LZ4)", &mb(report.stored_bytes)));
    html.push_str("</table>");

    let heading = if report.estimated {
        "Estimated need before parsing the book"
    } else {
        "Estimated need when opening"
    };
    html.push_str(&format!("<h2>{heading}</h2><table>"));
    if report.estimated {
        html.push_str(&row("Text (LZ4)", &mb(report.stored_bytes)));
    } else {
        // The chunks were compressed before this check, so their blocks are
        // already resident and already missing from `available`.
        html.push_str(&row(
            "Text (LZ4, already resident)",
            &mb(report.stored_bytes),
        ));
    }
    html.push_str(&row("Page layout (one chunk)", &mb(report.dom_bytes)));
    if report.estimated {
        html.push_str(&row("Images (one chunk, decoded)", "not counted"));
    } else {
        html.push_str(&row("Images (one chunk, decoded)", &mb(report.image_bytes)));
    }
    html.push_str(&row(
        "Page index",
        &format!("{} (~{} pages)", mb(report.index_bytes), report.pages),
    ));
    html.push_str(&row("Reserved for the reader", &mb(report.reserve)));
    html.push_str(&row("Total", &mb(report.need)));
    html.push_str("</table>");

    if report.estimated {
        html.push_str(
            "<p class=\"small\">Estimated from the size of the book's XHTML in the \
EPUB, before the book was read. Images are not counted, so the real need is \
at least this much.</p>",
        );
    }
    html.push_str(
        "<p class=\"small\">The text of every chunk is kept compressed; only the \
chunk being read is decompressed and laid out, and the page index grows as \
chunks are laid out.</p>",
    );

    if shortfall > 0 {
        let at_least = if report.estimated { "at least " } else { "" };
        html.push_str(&format!(
            "<p>Short by {}{}. Free some memory and try again, or open a smaller book.</p>",
            at_least,
            mb(shortfall)
        ));
    } else {
        html.push_str("<p>Memory was forced low for testing.</p>");
    }
    html.push_str("<p class=\"small\">This open did not save a reading position for the book.</p>");
    html.push_str("</body></html>");
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(htmls: &[&str]) -> Vec<ChunkSource> {
        htmls
            .iter()
            .map(|html| ChunkSource::new((*html).to_string()))
            .collect()
    }

    #[test]
    fn a_tiny_single_chunk_is_never_refused() {
        // The diagnostic page itself is this small, and so are small books:
        // they cannot be the thing that exhausts memory.
        let sources = chunks(&["<html><body>hi</body></html>"]);
        assert!(low_memory_page(&sources, 100, 0, Some("tiny")).is_none());
    }

    #[test]
    fn the_estimate_covers_layout_index_and_reserve() {
        let htmls = ["x".repeat(100 * 1024), "x".repeat(120 * 1024)];
        let sources = chunks(&[htmls[0].as_str(), htmls[1].as_str()]);
        let report = measure(&sources, 5000, 0);
        assert_eq!(report.html_bytes, 220 * 1024);
        assert_eq!(report.chunks, 2);
        assert_eq!(report.largest_chunk, 120 * 1024);
        // Only the compressed text stays resident, not the HTML it was made
        // from (this repeated text compresses to almost nothing).
        assert!(report.stored_bytes < report.html_bytes);
        assert_eq!(
            report.stored_bytes,
            sources
                .iter()
                .map(|source| source.compressed.len() as u64)
                .sum()
        );
        // One chunk's DOM is priced at the chunk cost, not a whole book's.
        assert_eq!(
            report.dom_bytes,
            120 * 1024 * CHUNK_DOM_BYTES_PER_HTML_BYTE as u64
        );
        assert_eq!(report.dom_bytes, 7_680 * 1024);
        // The compressed text is already resident when this check runs, so it
        // is not part of what the open still needs.
        assert_eq!(
            report.need,
            report.dom_bytes + report.index_bytes + report.reserve
        );
        assert!(report.need > report.dom_bytes);
    }

    #[test]
    fn the_reserve_scales_with_the_device() {
        const MB: usize = 1024 * 1024;
        assert_eq!(layout_reserve(24 * MB), 8 * MB);
        assert_eq!(layout_reserve(38 * MB), 8 * MB);
        assert_eq!(layout_reserve(100 * MB), 12 * MB + MB / 2);
        assert_eq!(layout_reserve(400 * MB), LAYOUT_MEMORY_RESERVE);
    }

    #[test]
    fn a_book_that_needs_less_than_is_available_fits() {
        // A 43 MB AZW3 measured 34 MB peak at a 38 MB budget: its 112 KB
        // chunks, page index and reserve must stay under that.
        let report = MemoryReport {
            total: Some(256 * 1024 * 1024),
            available: Some(38 * 1024 * 1024),
            available_source: "CRE_AVAILABLE_MB",
            resident: Some(10 * 1024 * 1024),
            source_bytes: 43 * 1024 * 1024,
            source_text_bytes: 0,
            html_bytes: 700 * 1024,
            stored_bytes: 400 * 1024,
            chunks: 25,
            largest_chunk: 112 * 1024,
            dom_bytes: 112 * 1024 * CHUNK_DOM_BYTES_PER_HTML_BYTE as u64,
            image_bytes: 0,
            pages: 286,
            index_bytes: 286 * PAGE_INDEX_BYTES,
            reserve: layout_reserve(38 * 1024 * 1024) as u64,
            need: 112 * 1024 * CHUNK_DOM_BYTES_PER_HTML_BYTE as u64
                + 286 * PAGE_INDEX_BYTES
                + layout_reserve(38 * 1024 * 1024) as u64,
            estimated: false,
        };
        assert!(
            report.fits(),
            "need {} available {:?}",
            report.need,
            report.available
        );
    }

    #[test]
    fn a_big_book_is_refused_from_its_xhtml_size_without_being_parsed() {
        let page = preflight_with(
            24 * 1024 * 1024,
            24 * 1024 * 1024,
            64 * 1024,
            22 * 1024 * 1024,
            "CRE_AVAILABLE_MB",
            Some("Big Book"),
            false,
        )
        .expect("a 24 MB book cannot fit in 22 MB");
        assert!(page.contains("Big Book"));
        assert!(page.contains("Estimated need before parsing the book"));
        assert!(page.contains("Text (XHTML, from the zip)"));
        assert!(page.contains("not counted"));
        assert!(page.contains("Short by at least"));
    }

    #[test]
    fn an_early_estimate_lets_a_book_that_clearly_fits_through() {
        // The estimate exists to avoid reading a book that cannot fit; it
        // must never turn away one that obviously can.
        assert!(
            preflight_with(
                1_000_000,
                1_000_000,
                64 * 1024,
                2048 * 1024 * 1024,
                "CRE_AVAILABLE_MB",
                Some("Small"),
                false,
            )
            .is_none()
        );
    }

    #[test]
    fn the_page_lists_every_part_of_the_estimate() {
        let htmls = ["x".repeat(300 * 1024)];
        let sources = chunks(&[htmls[0].as_str()]);
        let report = measure(&sources, 1_000_000, 0);
        let page = render_page(&report, Some("A Book"));
        assert!(page.contains("Not enough memory to open this book"));
        for label in [
            "Total memory",
            "Available",
            "This process",
            "A Book",
            "Generated HTML",
            "Text stored (LZ4)",
            "Text (LZ4, already resident)",
            "Page layout (one chunk)",
            "Page index",
            "Reserved for the reader",
            "Total",
        ] {
            assert!(page.contains(label), "missing {label}");
        }
        // The page index row names the page estimate it is based on.
        assert!(page.contains("~153 pages"), "{}", report.pages);
    }
}

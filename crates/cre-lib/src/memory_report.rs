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
    ChunkSource, LAYOUT_BYTES_PER_HTML_BYTE, LAYOUT_MEMORY_RESERVE, memory_available_detail,
    memory_total_bytes, resident_bytes,
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
    let dom_bytes = largest_chunk.saturating_mul(LAYOUT_BYTES_PER_HTML_BYTE as u64);
    let pages: u64 = html_bytes / HTML_BYTES_PER_PAGE.max(1);
    let index_bytes = pages.saturating_mul(PAGE_INDEX_BYTES);
    let reserve = LAYOUT_MEMORY_RESERVE as u64;
    let (available, available_source) = match memory_available_detail() {
        Some((bytes, source)) => (Some(bytes), source),
        None => (None, "unavailable"),
    };
    MemoryReport {
        total: memory_total_bytes(),
        available,
        available_source,
        resident: resident_bytes(),
        source_bytes,
        html_bytes,
        stored_bytes,
        chunks,
        largest_chunk,
        dom_bytes,
        image_bytes,
        pages,
        index_bytes,
        reserve,
        need: stored_bytes + dom_bytes + image_bytes + index_bytes + reserve,
    }
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
    html.push_str(&row(
        "Generated HTML",
        &format!(
            "{} ({} chunks, largest {})",
            mb(report.html_bytes),
            report.chunks,
            kb(report.largest_chunk)
        ),
    ));
    html.push_str(&row("Text stored (LZ4)", &mb(report.stored_bytes)));
    html.push_str("</table>");

    html.push_str("<h2>Estimated need when opening</h2><table>");
    html.push_str(&row("Text (LZ4)", &mb(report.stored_bytes)));
    html.push_str(&row("Page layout (one chunk)", &mb(report.dom_bytes)));
    html.push_str(&row("Images (one chunk, decoded)", &mb(report.image_bytes)));
    html.push_str(&row(
        "Page index",
        &format!("{} (~{} pages)", mb(report.index_bytes), report.pages),
    ));
    html.push_str(&row("Reserved for the reader", &mb(report.reserve)));
    html.push_str(&row("Total", &mb(report.need)));
    html.push_str("</table>");

    html.push_str(
        "<p class=\"small\">The text of every chunk is kept compressed; only the \
chunk being read is decompressed and laid out, and the page index grows as \
chunks are laid out.</p>",
    );

    if shortfall > 0 {
        html.push_str(&format!(
            "<p>Short by {}. Free some memory and try again, or open a smaller book.</p>",
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
    fn the_estimate_covers_text_layout_index_and_reserve() {
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
        // Only one chunk's DOM is resident.
        assert_eq!(
            report.dom_bytes,
            120 * 1024 * LAYOUT_BYTES_PER_HTML_BYTE as u64
        );
        assert_eq!(
            report.need,
            report.stored_bytes + report.dom_bytes + report.index_bytes + report.reserve
        );
        assert!(report.need > report.stored_bytes + report.dom_bytes);
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
            "Text (LZ4)",
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

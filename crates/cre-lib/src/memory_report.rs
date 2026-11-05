//! Low-memory diagnostics.
//!
//! Opening a book holds the generated chunk HTML, one chunk's DOM and the page
//! index at the same time. On a 256 MB reader that can exceed what the kernel
//! can hand out, and the reader process is then killed. Before the layout
//! starts we can compare the book's measured size against the memory the device
//! reports, and when it cannot fit, show a page that explains why.

use std::fmt;

use crate::{
    LAYOUT_BYTES_PER_HTML_BYTE, LAYOUT_MEMORY_RESERVE, memory_available_detail, memory_total_bytes,
    resident_bytes,
};

/// Rough bytes a page needs in the page index (start position, line entries and
/// XPointer). Measured from whole-book layouts: a 2381-page book keeps ~1.4 MB
/// of page metadata, i.e. ~600 B per page.
const PAGE_INDEX_BYTES: u64 = 600;

/// Average bytes of generated HTML per page of text, used only to estimate how
/// many pages the index will hold before the layout ran.
const HTML_BYTES_PER_PAGE: u64 = 3000;

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
    pub html_bytes: u64,
    pub chunks: usize,
    pub largest_chunk: u64,
    pub dom_bytes: u64,
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

/// Estimates what laying `htmls` out costs and what is available.
pub(crate) fn measure(htmls: &[String], source_bytes: u64) -> MemoryReport {
    let html_bytes = htmls.iter().map(|html| html.len() as u64).sum();
    let largest_chunk = htmls
        .iter()
        .map(|html| html.len() as u64)
        .max()
        .unwrap_or(0);
    let chunks = htmls.len();
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
        chunks,
        largest_chunk,
        dom_bytes,
        index_bytes,
        reserve,
        need: html_bytes + dom_bytes + index_bytes + reserve,
    }
}

/// The page that explains that `htmls` does not fit, or `None` when it does.
pub(crate) fn low_memory_page(
    htmls: &[String],
    source_bytes: u64,
    title: Option<&str>,
) -> Option<String> {
    // The diagnostic page itself is tiny; never run the check on it, or a
    // device with almost no memory left would loop on it.
    if let [only] = htmls
        && only.len() <= DIAGNOSTIC_PAGE_LIMIT
    {
        return None;
    }
    let report = measure(htmls, source_bytes);
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
    html.push_str("</table>");

    html.push_str("<h2>Estimated need when opening</h2><table>");
    html.push_str(&row("Text (chunk HTML)", &mb(report.html_bytes)));
    html.push_str(&row("Page layout (one chunk)", &mb(report.dom_bytes)));
    html.push_str(&row("Page index", &mb(report.index_bytes)));
    html.push_str(&row("Reserved for the reader", &mb(report.reserve)));
    html.push_str(&row("Total", &mb(report.need)));
    html.push_str("</table>");

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

    #[test]
    fn a_tiny_single_chunk_is_never_refused() {
        // The diagnostic page itself is this small, and so are small books:
        // they cannot be the thing that exhausts memory.
        let htmls = vec!["<html><body>hi</body></html>".to_string()];
        assert!(low_memory_page(&htmls, 100, Some("tiny")).is_none());
    }

    #[test]
    fn the_estimate_covers_text_layout_index_and_reserve() {
        let htmls = vec!["x".repeat(100 * 1024), "x".repeat(120 * 1024)];
        let report = measure(&htmls, 5000);
        assert_eq!(report.html_bytes, 220 * 1024);
        assert_eq!(report.chunks, 2);
        assert_eq!(report.largest_chunk, 120 * 1024);
        // Only one chunk's DOM is resident.
        assert_eq!(
            report.dom_bytes,
            120 * 1024 * LAYOUT_BYTES_PER_HTML_BYTE as u64
        );
        assert_eq!(
            report.need,
            report.html_bytes + report.dom_bytes + report.index_bytes + report.reserve
        );
        assert!(report.need > report.html_bytes + report.dom_bytes);
    }

    #[test]
    fn the_page_lists_every_part_of_the_estimate() {
        let htmls = vec!["x".repeat(300 * 1024)];
        let report = measure(&htmls, 1_000_000);
        let page = render_page(&report, Some("A Book"));
        assert!(page.contains("Not enough memory to open this book"));
        for label in [
            "Total memory",
            "Available",
            "This process",
            "A Book",
            "Generated HTML",
            "Text (chunk HTML)",
            "Page layout (one chunk)",
            "Page index",
            "Reserved for the reader",
            "Total",
        ] {
            assert!(page.contains(label), "missing {label}");
        }
    }
}

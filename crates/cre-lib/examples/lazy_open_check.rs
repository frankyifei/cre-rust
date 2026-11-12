//! Dev-only check: a lazily opened book must converge to exactly the page
//! numbers a full open produces, and keep its reading position meanwhile.
//!
//! ```text
//! CRE_LAZY_OPEN=1 lazy-open-check BOOK [FONT_SIZE]
//! ```
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::time::Instant;

use cre_lib::{Document, DocumentOptions};

fn options(font_size: f32) -> DocumentOptions {
    let env_usize = |name: &str, default: u32| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    };
    DocumentOptions {
        width: env_usize("W", 758),
        height: env_usize("H", 955),
        font_size,
        page_margin: std::env::var("M")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(15.0),
        ..Default::default()
    }
}

fn page_hash(doc: &mut Document, page: usize) -> u64 {
    let bitmap = doc.render_page(page).expect("render");
    let mut hasher = DefaultHasher::new();
    bitmap.data.hash(&mut hasher);
    hasher.finish()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let book = Path::new(
        args.get(1)
            .map(String::as_str)
            .unwrap_or("/home/frank/ebooks/21.epub"),
    );
    let font_size: f32 = args
        .get(2)
        .map(|value| value.parse().unwrap())
        .unwrap_or(26.0);
    let options = options(font_size);

    let started = Instant::now();
    // `CRE_DEFER=1` does what cre-lua does: parse on load, lay out at
    // renderDocument, which is the path every real reader takes.
    let mut doc = if std::env::var_os("CRE_DEFER").is_some() {
        let font_datas =
            cre_lib::load_font_datas(&cre_lib::resolve_font_paths(&options).expect("fonts"));
        let mut document =
            Document::with_options_with_font_data_parsed(book, &options, &font_datas)
                .expect("parse");
        document.ensure_layout().expect("layout");
        document
    } else {
        Document::with_options(book, &options).expect("open")
    };
    let open_ms = started.elapsed().as_secs_f64() * 1000.0;
    println!(
        "open {open_ms:.1}ms pages={} pending={}",
        doc.page_count(),
        doc.has_background_layout()
    );
    // What the reader sees on the first draw: the opening page.
    let first_page = doc.current_page();
    let first_xp = doc.xpointer_from_page(first_page).ok();
    println!("first page {first_page} xp={first_xp:?}");

    let toc_without_page = doc.toc().iter().filter(|item| item.page.is_none()).count();
    println!(
        "toc {} entries, {toc_without_page} without a page yet",
        doc.toc().len()
    );

    // `CRE_RELAYOUT=<font size>` changes the font and checks the relayout path:
    // it must be accepted (no reopen), keep the position, and converge to a
    // fresh open at the new size.
    if let Ok(size) = std::env::var("CRE_RELAYOUT") {
        let new_size: f32 = size.parse().expect("CRE_RELAYOUT");
        let new_options = DocumentOptions {
            font_size: new_size,
            ..options.clone()
        };
        let accepted = doc.relayout(&new_options).expect("relayout");
        println!(
            "relayout {font_size} -> {new_size}: accepted={accepted} pages={} pending={}",
            doc.page_count(),
            doc.has_background_layout()
        );
        while doc.has_background_layout() {
            if !doc.drain_background_layouts() {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        let fresh = Document::with_options_exact(book, &new_options).expect("fresh");
        println!(
            "  converged pages={} fresh pages={} equal={}",
            doc.page_count(),
            fresh.page_count(),
            doc.page_count() == fresh.page_count()
        );
    }

    // Jump to the last page while the background pass is still running: the
    // chunk must be laid out on demand and the page must render.
    let last_page = doc.page_count();
    let jump_started = Instant::now();
    let jumped = doc.go_to_page(last_page).expect("goto last page");
    println!(
        "jumped to {jumped} of {last_page} in {:.1}ms",
        jump_started.elapsed().as_secs_f64() * 1000.0
    );
    let jumped_xp = doc.xpointer_from_page(jumped).ok();
    println!("  xp={jumped_xp:?}");

    let mut drains = 0;
    let mut slowest = 0.0f64;
    while doc.has_background_layout() && drains < 200_000 {
        let call = Instant::now();
        let installed = doc.drain_background_layouts();
        slowest = slowest.max(call.elapsed().as_secs_f64() * 1000.0);
        drains += 1;
        if !installed {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    let converged = doc.page_count();
    println!(
        "converged pages={converged} position={} drains={drains} slowest={slowest:.1}ms total={:.1}ms",
        doc.current_page(),
        started.elapsed().as_secs_f64() * 1000.0
    );
    let toc_without_page = doc.toc().iter().filter(|item| item.page.is_none()).count();
    println!("toc after: {toc_without_page} without a page");

    // Same book, same settings, laid out in full at open: must be identical.
    // The reference must take the full path even when this run forces lazy open.
    // SAFETY: single-threaded at this point; nothing else reads the environment.
    unsafe { std::env::set_var("CRE_LAZY_OPEN", "0") };
    let reference_started = Instant::now();
    let mut reference = Document::with_options(book, &options).expect("reference open");
    let reference_ms = reference_started.elapsed().as_secs_f64() * 1000.0;
    println!(
        "reference open {reference_ms:.1}ms pages={}",
        reference.page_count()
    );
    println!(
        "pages equal: {}",
        doc.page_count() == reference.page_count()
    );
    let probes: Vec<usize> = [1, doc.page_count() / 2, doc.page_count()]
        .into_iter()
        .filter(|page| *page > 0)
        .collect();
    let lazy_hashes: Vec<u64> = probes
        .iter()
        .map(|&page| page_hash(&mut doc, page))
        .collect();
    let reference_hashes: Vec<u64> = probes
        .iter()
        .map(|&page| page_hash(&mut reference, page))
        .collect();
    println!("probe pages {probes:?}");
    println!("lazy      {lazy_hashes:?}");
    println!("reference {reference_hashes:?}");
    println!("hashes equal: {}", lazy_hashes == reference_hashes);
}

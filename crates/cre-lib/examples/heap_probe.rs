//! Dev-only: how much of the process heap is in use vs free inside glibc.
use std::path::PathBuf;

fn rss_kib() -> u64 {
    let text = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = text
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    pages * 4
}

fn report(label: &str) {
    let info = unsafe { libc::mallinfo2() };
    println!(
        "{label}: rss={}kB in_use={}kB free={}kB mmap={}kB",
        rss_kib(),
        info.uordblks as u64 / 1024,
        info.fordblks as u64 / 1024,
        info.hblkhd as u64 / 1024
    );
}

fn trim() {
    unsafe {
        libc::malloc_trim(0);
    }
}

fn main() {
    let book = std::env::var("BOOK").expect("BOOK");
    let options = cre_lib::DocumentOptions {
        width: 758,
        height: 1024,
        font_size: 22.0,
        page_margin: 10.0,
        font_path: Some(PathBuf::from(
            "/home/frank/koreader/resources/fonts/noto/NotoSans-Regular.ttf",
        )),
        font_family: Some("Noto Sans".to_string()),
        fallback_font_paths: vec![PathBuf::from(
            "/home/frank/koreader/resources/fonts/freefont/FreeSerif.ttf",
        )],
        ..Default::default()
    };
    report("before open");
    let mut doc = cre_lib::Document::with_options(&book, &options).expect("open");
    report("after open");
    trim();
    report("after trim");
    for page in (1..=doc.page_count()).step_by(25) {
        let _ = doc.render_page(page);
    }
    report("after reading");
    trim();
    report("after trim 2");
    let _ = doc.page_count();
}

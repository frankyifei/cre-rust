//! Dev-only: full decode vs DCT-scaled decode for a page-sized photo.
use std::io::Cursor;
use std::time::Instant;

fn main() {
    let path = std::env::var("IMG").expect("IMG");
    let target_w: u16 = std::env::var("W")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(760);
    let target_h: u16 = std::env::var("H")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024);
    let bytes = std::fs::read(&path).expect("read");

    let started = Instant::now();
    let full = image::load_from_memory(&bytes)
        .expect("full decode")
        .into_rgba8();
    let full_time = started.elapsed();
    let full_bytes = full.as_raw().len();

    let started = Instant::now();
    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(&bytes));
    decoder.scale(target_w, target_h).expect("scale");
    let pixels = decoder.decode().expect("scaled decode");
    let info = decoder.info().expect("info");
    let scaled_time = started.elapsed();

    println!(
        "full: {:?} {}x{} {:.1}MB | scaled: {:?} {}x{} {:.1}MB | speedup {:.2}x",
        full_time,
        full.width(),
        full.height(),
        full_bytes as f64 / 1e6,
        scaled_time,
        info.width,
        info.height,
        pixels.len() as f64 / 1e6,
        full_time.as_secs_f64() / scaled_time.as_secs_f64().max(1e-9),
    );
}

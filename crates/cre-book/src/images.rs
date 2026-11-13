//! Image header reading and decoded-size estimates.
//!
//! Both are needed before anything is decoded: the chunk budget has to know how
//! much a picture will cost once Blitz decodes it, and layout needs its
//! intrinsic size without the pixels.

/// Intrinsic pixel size of a raster image, read from its header.
///
/// The pixels are not decoded: a large photo's header alone gives the layout
/// size.
pub fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Estimated decoded size of one image, for the chunk memory budget.
pub fn estimated_image_bytes(name: &str, bytes: &[u8]) -> usize {
    if let Some((width, height)) = image_dimensions(bytes) {
        return (width as usize * height as usize * 4).max(64 * 1024);
    }
    estimated_decoded_from_size(name, bytes.len())
}

/// True when the bytes look like a picture the estimator understands.
pub fn is_image_bytes(bytes: &[u8]) -> bool {
    image_dimensions(bytes).is_some() || bytes.starts_with(b"<svg")
}

/// Decoded size from the file size and the picture's format, for bytes whose
/// header could not be read (a compressed entry, an unknown magic).
pub fn estimated_decoded_from_size(name: &str, file_bytes: usize) -> usize {
    let factor = if name.ends_with(".jpg") || name.ends_with(".jpeg") {
        32
    } else if name.ends_with(".png") || name.ends_with(".gif") {
        4
    } else if name.ends_with(".webp") {
        12
    } else if name.ends_with(".svg") {
        return 2 * 1024 * 1024;
    } else {
        8
    };
    file_bytes.saturating_mul(factor)
}

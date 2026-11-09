//! The image placeholder path must find and paint an image whatever form its
//! `src` takes, and must never blank a page when it cannot.
//!
//! Each case builds a tiny EPUB whose chapter holds one `<img>`, paints page 1
//! and checks that the picture is on the page. A blank page (or a panic) is the
//! failure mode that matters: the placeholder is only visible to layout as a
//! transparent SVG, so a key that does not match leaves nothing to paint.

use std::io::Write;
use std::path::{Path, PathBuf};

use cre_lib::{Document, DocumentOptions};

fn options() -> DocumentOptions {
    DocumentOptions {
        width: 600,
        height: 800,
        layout_height: 100_000,
        page_margin: 10.0,
        font_size: 18.0,
        font_path: None,
        font_family: None,
        fallback_font_paths: Vec::new(),
        stylesheet: None,
        embedded_styles_enabled: true,
        viewport_scale: 1.0,
    }
}

/// A valid JPEG of the given size, solid black.
fn black_jpeg(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbImage::from_pixel(width, height, image::Rgb([0, 0, 0]));
    let mut out = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90);
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

fn container_xml() -> &'static str {
    r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#
}

fn package_xml(images: &[(&str, &[u8])]) -> String {
    let mut manifest = String::new();
    for (index, (name, _)) in images.iter().enumerate() {
        manifest.push_str(&format!(
            "<item id=\"img{index}\" href=\"Images/{name}\" media-type=\"image/jpeg\"/>"
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:image-placeholder-test</dc:identifier>
    <dc:title>Image placeholder</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ch" href="Text/ch1.xhtml" media-type="application/xhtml+xml"/>
    {manifest}
  </manifest>
  <spine><itemref idref="ch"/></spine>
</package>"#
    )
}

fn chapter_xml(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head><title>Chapter</title></head>
  <body>{body}</body>
</html>"#
    )
}

fn write_epub(dir: &Path, name: &str, body: &str, images: &[(&str, &[u8])]) -> PathBuf {
    std::fs::create_dir_all(dir).expect("create dir");
    let path = dir.join(name);
    let file = std::fs::File::create(&path).expect("create epub");
    let mut zip = zip::ZipWriter::new(file);
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    zip.start_file("mimetype", stored).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    zip.start_file("META-INF/container.xml", deflated).unwrap();
    zip.write_all(container_xml().as_bytes()).unwrap();
    zip.start_file("OEBPS/content.opf", deflated).unwrap();
    zip.write_all(package_xml(images).as_bytes()).unwrap();
    zip.start_file("OEBPS/Text/ch1.xhtml", deflated).unwrap();
    zip.write_all(chapter_xml(body).as_bytes()).unwrap();
    for (image_name, bytes) in images {
        zip.start_file(format!("OEBPS/Images/{image_name}"), deflated)
            .unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
    path
}

/// Ink on page 1 and the page's text, for one book.
fn page_one(path: &Path) -> (usize, String) {
    let mut document = Document::with_options(path, &options()).expect("open book");
    let bitmap = document.render_page(1).expect("render page");
    let ink = bitmap.data.iter().filter(|value| **value < 128).count();
    let text = document.page_text(1).unwrap_or_default();
    (ink, text)
}

/// A picture that fills the page is mostly ink; a page with only text is not.
const IMAGE_INK: usize = 20_000;

fn assert_image_visible(label: &str, src: &str, image_name: &str) {
    let dir = std::env::temp_dir().join("cre-image-placeholder");
    let body = format!("<p>before</p><img src=\"{src}\" alt=\"\"/><p>after</p>");
    let jpeg = black_jpeg(1200, 1600);
    let path = write_epub(
        &dir,
        &format!("{label}.epub"),
        &body,
        &[(image_name, &jpeg)],
    );
    let mut document = Document::with_options(&path, &options()).expect("open book");
    let bitmap = document.render_page(1).expect("render page");
    let ink = bitmap.data.iter().filter(|value| **value < 128).count();
    let text = document.page_text(1).unwrap_or_default();
    assert!(
        ink >= IMAGE_INK,
        "{label}: expected the picture to be painted, ink={ink} text={text:?}"
    );
    assert!(text.contains("before"), "{label}: text missing: {text:?}");
    assert!(
        document.memory_breakdown().placeheld_images >= 1,
        "{label}: the image should have gone through the placeholder path"
    );
}

#[test]
fn relative_source_is_painted() {
    assert_image_visible("relative", "../Images/plain.jpg", "plain.jpg");
}

#[test]
fn absolute_source_is_painted() {
    assert_image_visible(
        "absolute",
        "epub://book/OEBPS/Images/absolute.jpg",
        "absolute.jpg",
    );
}

#[test]
fn fragment_source_is_painted() {
    assert_image_visible("fragment", "../Images/frag.jpg#note", "frag.jpg");
}

#[test]
fn percent_encoded_space_is_painted() {
    assert_image_visible("space", "../Images/space%20name.jpg", "space name.jpg");
}

#[test]
fn unicode_name_is_painted() {
    assert_image_visible("unicode", "../Images/%E5%9B%BE%E7%89%87.jpg", "图片.jpg");
}

#[test]
fn query_string_source_is_painted() {
    assert_image_visible("query", "../Images/query.jpg?cache=1", "query.jpg");
}

#[test]
fn corrupt_image_does_not_blank_the_page() {
    let dir = std::env::temp_dir().join("cre-image-placeholder");
    let body = "<p>before</p><img src=\"../Images/broken.jpg\" alt=\"\"/><p>after</p>";
    let broken: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0x00];
    let path = write_epub(&dir, "corrupt.epub", body, &[("broken.jpg", &broken)]);
    let (ink, text) = page_one(&path);
    assert!(text.contains("before"), "text missing: {text:?}");
    assert!(
        ink < IMAGE_INK,
        "a broken image must not paint a picture, ink={ink}"
    );
}

#[test]
fn background_image_is_still_painted() {
    let dir = std::env::temp_dir().join("cre-image-placeholder");
    // The stylesheet lives in the chapter so the URL is absolute: a CSS
    // `url(../...)` is not rewritten by the spine rewrite pass.
    let body = "<style>body { background-image: url(epub://book/OEBPS/Images/background.jpg); \
                background-size: 100% 100%; }</style><p>before</p>";
    let jpeg = black_jpeg(1200, 1600);
    let path = write_epub(&dir, "background.epub", body, &[("background.jpg", &jpeg)]);
    let (ink, text) = page_one(&path);
    assert!(text.contains("before"), "text missing: {text:?}");
    assert!(
        ink >= IMAGE_INK,
        "a background image must keep its pixels, ink={ink}"
    );
}

#[test]
fn color_path_paints_the_image() {
    let dir = std::env::temp_dir().join("cre-image-placeholder");
    let body = "<p>before</p><img src=\"../Images/color.jpg\" alt=\"\"/><p>after</p>";
    let jpeg = black_jpeg(1200, 1600);
    let path = write_epub(&dir, "color.epub", body, &[("color.jpg", &jpeg)]);
    let mut document = Document::with_options(&path, &options()).expect("open book");
    let (bitmap, _, _) = document
        .render_page_color_with_stats(1)
        .expect("render color page");
    let dark = bitmap
        .data
        .chunks(4)
        .filter(|pixel| pixel.len() == 4 && pixel[0] < 128 && pixel[1] < 128 && pixel[2] < 128)
        .count();
    assert!(
        dark >= IMAGE_INK,
        "the colour path must paint the picture too, dark={dark}"
    );
}

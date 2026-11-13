//! A book's own fonts are loaded by Blitz from its `@font-face` rules, so the
//! engine has to (a) tell the reader which families the book provides and (b)
//! be able to ignore them. Ignoring them must work on an open document:
//! KOReader toggles the setting from the typeset menu, and a reopen of a large
//! book for it would be a multi-second freeze.
//!
//! The synthetic book embeds a monospace font, which is the clearest possible
//! signal: with the book's font a page holds fewer characters than with the
//! reader's proportional font, so both the page count and the first page's text
//! change.
//!
//! The rules live in a *linked* stylesheet, because that is what Calibre and
//! most publishers produce: the `@font-face` is not in the chapter HTML at all,
//! it is fetched by `EpubNet` when Blitz parses the stylesheet.

use std::io::Write;
use std::path::{Path, PathBuf};

use cre_lib::{Document, DocumentOptions};

const READER_FONT: &str = "/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf";
const EMBEDDED_FONT: &str = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf";
const FAMILY: &str = "Book Mono";

fn options() -> DocumentOptions {
    DocumentOptions {
        width: 600,
        height: 800,
        layout_height: 100_000,
        page_margin: 10.0,
        font_size: 18.0,
        font_path: Some(Path::new(READER_FONT).to_path_buf()),
        font_family: None,
        fallback_font_paths: Vec::new(),
        stylesheet: None,
        embedded_styles_enabled: true,
        viewport_scale: 1.0,
    }
}

fn require_fonts() {
    for font in [READER_FONT, EMBEDDED_FONT] {
        assert!(
            Path::new(font).exists(),
            "missing required test font: {font}"
        );
    }
}

fn container_xml() -> &'static str {
    r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#
}

fn package_xml() -> &'static str {
    r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:cre-embedded-fonts</dc:identifier>
    <dc:title>Embedded fonts</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ch" href="Text/ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="css" href="Styles/book.css" media-type="text/css"/>
    <item id="font" href="Fonts/mono.ttf" media-type="font/ttf"/>
  </manifest>
  <spine><itemref idref="ch"/></spine>
</package>"#
}

/// The `@font-face` and the `font-family` that uses it. `font_url` is resolved
/// against the stylesheet, like a real book's.
fn book_css(font_url: &str) -> String {
    format!(
        r#"@font-face {{ src: url({font_url}); font-family: "{FAMILY}"; }}
p {{ font-family: "{FAMILY}", serif; }}"#
    )
}

fn paragraphs() -> String {
    let paragraph = "<p>the quick brown fox jumps over the lazy dog while the \
                     printing press keeps turning through the long night</p>";
    paragraph.repeat(300)
}

/// The chapter: `<link>`s the stylesheet, or (with `inline_css`) carries the
/// same rules in a `<style>` element.
fn chapter_xml(inline_css: bool) -> String {
    let (head, style) = if inline_css {
        (
            String::new(),
            format!("<style>{}</style>", book_css("../Fonts/mono.ttf")),
        )
    } else {
        (
            r#"<link rel="stylesheet" type="text/css" href="../Styles/book.css"/>"#.to_string(),
            String::new(),
        )
    };
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head><title>Chapter</title>{head}</head>
  <body>{style}{}</body>
</html>"#,
        paragraphs()
    )
}

fn write_epub(dir: &Path, name: &str, inline_css: bool) -> PathBuf {
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
    zip.write_all(package_xml().as_bytes()).unwrap();
    zip.start_file("OEBPS/Text/ch1.xhtml", deflated).unwrap();
    zip.write_all(chapter_xml(inline_css).as_bytes()).unwrap();
    zip.start_file("OEBPS/Styles/book.css", deflated).unwrap();
    zip.write_all(book_css("../Fonts/mono.ttf").as_bytes())
        .unwrap();
    zip.start_file("OEBPS/Fonts/mono.ttf", stored).unwrap();
    zip.write_all(&std::fs::read(EMBEDDED_FONT).expect("read font"))
        .unwrap();
    zip.finish().unwrap();
    path
}

fn characters_on_page(document: &mut Document, page: usize) -> usize {
    document
        .page_text(page)
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_whitespace())
        .count()
}

/// Opens a synthetic book, checks the family list, then toggles the fonts off
/// and back on with a relayout in between — the path KOReader takes from the
/// typeset menu.
fn assert_toggle_relayouts(name: &str, inline_css: bool) {
    require_fonts();
    let path = write_epub(
        &std::env::temp_dir().join("cre-embedded-fonts"),
        name,
        inline_css,
    );
    let mut document = Document::with_options(&path, &options()).expect("open book");

    assert_eq!(
        document.embedded_font_list(),
        [FAMILY.to_string()],
        "the declared family must be reported to the reader"
    );
    assert!(
        document.embedded_fonts_enabled(),
        "embedded fonts are on by default, like crengine"
    );

    let with_book_font = document.page_count();
    let chars_with_book_font = characters_on_page(&mut document, 1);

    document.set_embedded_fonts(false);
    assert!(document.relayout(&options()).expect("relayout"));
    assert_eq!(
        document.embedded_font_list(),
        [FAMILY.to_string()],
        "the list describes what the book declares, not what is in use"
    );
    let without_book_font = document.page_count();
    let chars_without_book_font = characters_on_page(&mut document, 1);

    assert_ne!(
        chars_with_book_font, chars_without_book_font,
        "turning embedded fonts off must change the page: {chars_with_book_font} vs \
         {chars_without_book_font} characters on page 1"
    );
    assert_ne!(
        with_book_font, without_book_font,
        "the monospace book font must change the page count: {with_book_font} vs \
         {without_book_font}"
    );

    // And back on, again without reopening.
    document.set_embedded_fonts(true);
    assert!(document.relayout(&options()).expect("relayout"));
    assert_eq!(document.page_count(), with_book_font);
    assert_eq!(characters_on_page(&mut document, 1), chars_with_book_font);
}

#[test]
fn fonts_linked_from_a_stylesheet_are_listed_and_can_be_switched_off() {
    // The real-world shape: the chapter links its CSS, so the rules are only
    // ever seen by the network provider, never in the chunk HTML.
    assert_toggle_relayouts("linked-fonts.epub", false);
}

#[test]
fn fonts_in_an_inline_style_are_listed_and_can_be_switched_off() {
    assert_toggle_relayouts("inline-fonts.epub", true);
}

#[test]
fn a_book_without_font_face_reports_no_fonts() {
    // The reader's "embedded fonts" option must stay disabled when the table
    // is empty (KOReader tests it with `next() ~= nil`).
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.html");
    let document = Document::with_options(&path, &options()).expect("open document");
    assert!(document.embedded_font_list().is_empty());
}

/// A standalone HTML document goes through `FileNet` instead of `EpubNet`, but
/// the reader expects the same behaviour: the linked stylesheet is where the
/// rules live, so the toggle has to reach that provider too.
///
/// The fallback face the HTML pipeline picks for `serif` happens to have nearly
/// the same advance widths as the embedded monospace one, so the page count does
/// not move here. What has to hold is that "turned off" lays out exactly like a
/// document that never declared the font — that is the observable promise.
#[test]
fn a_standalone_html_document_is_handled_the_same_way() {
    require_fonts();
    let dir = std::env::temp_dir().join("cre-embedded-fonts-html");
    std::fs::create_dir_all(&dir).expect("create dir");
    std::fs::copy(EMBEDDED_FONT, dir.join("mono.ttf")).expect("copy font");
    // Served from the document's own directory, so the font URL is a plain
    // relative path.
    std::fs::write(dir.join("book.css"), book_css("mono.ttf")).expect("write css");
    std::fs::write(
        dir.join("plain.css"),
        format!("p {{ font-family: \"{FAMILY}\", serif; }}"),
    )
    .expect("write plain css");
    let page = |css: &str| {
        format!(
            "<html><head><link rel=\"stylesheet\" href=\"{css}\"/></head>\
             <body>{}</body></html>",
            paragraphs()
        )
    };
    let path = dir.join("book.html");
    std::fs::write(&path, page("book.css")).expect("write html");
    let plain = dir.join("plain.html");
    std::fs::write(&plain, page("plain.css")).expect("write html");

    let mut document = Document::with_options(&path, &options()).expect("open document");
    assert_eq!(document.embedded_font_list(), [FAMILY.to_string()]);
    let chars_with_book_font = characters_on_page(&mut document, 1);

    document.set_embedded_fonts(false);
    assert!(document.relayout(&options()).expect("relayout"));
    let chars_toggled_off = characters_on_page(&mut document, 1);
    assert_ne!(chars_toggled_off, chars_with_book_font);

    let mut reference = Document::with_options(&plain, &options()).expect("open plain");
    assert!(
        reference.embedded_font_list().is_empty(),
        "the reference document declares no font"
    );
    assert_eq!(
        chars_toggled_off,
        characters_on_page(&mut reference, 1),
        "turning the fonts off must lay out like a book without the rules"
    );
}

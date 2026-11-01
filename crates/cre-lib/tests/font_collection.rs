//! Font collections (TTC/OTC): the selected family must be the one used.
//!
//! A collection holds several families in one file, and every generator asks for
//! the *generic* family (`serif`). The generic lists used to be in registration
//! order, so a collection always resolved to its first face: KOReader listed the
//! faces (FreeType does that) but picking one changed nothing, because only the
//! file path was passed down.
//!
//! `DocumentOptions::font_family` now puts the chosen family first. These tests
//! build a two-face collection out of two system fonts and check that
//!
//! * each family renders exactly like the standalone font it came from, and
//! * changing the family on an open document (`relayout`, which is what an EPUB
//!   does when only the face changes) rebuilds the font context even though the
//!   file path stays the same.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use cre_lib::{Document, DocumentOptions};

const SERIF: &str = "/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf";
const SANS: &str = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/koreader-test-data")
        .join(name)
}

/// Builds a TTC from standalone fonts: `ttcf` header, then each font's sfnt
/// directory and tables with the table offsets rebased to the new file.
fn build_collection(paths: &[&str], out: &Path) {
    let datas: Vec<Vec<u8>> = paths
        .iter()
        .map(|path| std::fs::read(path).expect("read font"))
        .collect();
    let header_len = 12 + 4 * datas.len();
    let mut offsets = Vec::new();
    let mut cursor = header_len;
    for data in &datas {
        cursor += (4 - cursor % 4) % 4;
        offsets.push(cursor);
        cursor += data.len();
    }
    let mut blob = vec![0u8; cursor];
    blob[..4].copy_from_slice(b"ttcf");
    blob[4..8].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    blob[8..12].copy_from_slice(&(datas.len() as u32).to_be_bytes());
    for (index, offset) in offsets.iter().enumerate() {
        let at = 12 + 4 * index;
        blob[at..at + 4].copy_from_slice(&(*offset as u32).to_be_bytes());
    }
    for (data, base) in datas.iter().zip(offsets.iter()) {
        blob[*base..*base + data.len()].copy_from_slice(data);
        let tables = u16::from_be_bytes([blob[*base + 4], blob[*base + 5]]) as usize;
        for table in 0..tables {
            let at = base + 12 + 16 * table + 8;
            let old = u32::from_be_bytes([blob[at], blob[at + 1], blob[at + 2], blob[at + 3]]);
            blob[at..at + 4].copy_from_slice(&(old + *base as u32).to_be_bytes());
        }
    }
    std::fs::write(out, blob).expect("write collection");
}

/// The two-face collection shared by both tests, or `None` when the system fonts
/// are missing (the test then skips instead of failing on a bare machine).
fn test_collection() -> Option<PathBuf> {
    if !Path::new(SERIF).exists() || !Path::new(SANS).exists() {
        eprintln!("skipping: DejaVu fonts are not installed");
        return None;
    }
    let directory = std::env::temp_dir().join("cre-font-collection-test");
    std::fs::create_dir_all(&directory).unwrap();
    let collection = directory.join("collection.ttc");
    build_collection(&[SERIF, SANS], &collection);
    Some(collection)
}

fn options_for(font_path: &Path, family: Option<&str>) -> DocumentOptions {
    DocumentOptions {
        width: 758,
        height: 1024,
        font_size: 22.0,
        font_path: Some(font_path.to_path_buf()),
        font_family: family.map(str::to_string),
        ..Default::default()
    }
}

fn document_hash(doc: &mut Document, page: usize) -> u64 {
    let bitmap = doc.render_page(page).expect("render");
    assert!(
        bitmap.data.iter().any(|value| *value < 128),
        "blank page {page}"
    );
    let mut hasher = DefaultHasher::new();
    bitmap.data.hash(&mut hasher);
    hasher.finish()
}

fn render_hash(font_path: &Path, family: Option<&str>) -> (usize, u64) {
    let mut doc = Document::with_options(fixture("sample.txt"), &options_for(font_path, family))
        .expect("open");
    let bitmap = doc.render_page(1).expect("render");
    let ink = bitmap.data.iter().filter(|value| **value < 128).count();
    assert!(ink > 0, "blank page for {family:?}");
    let mut hasher = DefaultHasher::new();
    bitmap.data.hash(&mut hasher);
    (ink, hasher.finish())
}

#[test]
fn collection_faces_are_selectable() {
    let Some(collection) = test_collection() else {
        return;
    };

    let (_, standalone_serif) = render_hash(Path::new(SERIF), None);
    let (_, standalone_sans) = render_hash(Path::new(SANS), None);
    assert_ne!(
        standalone_serif, standalone_sans,
        "the two fonts should not render identically"
    );

    let (_, collection_serif) = render_hash(&collection, Some("DejaVu Serif"));
    let (_, collection_sans) = render_hash(&collection, Some("DejaVu Sans"));
    assert_eq!(
        collection_serif, standalone_serif,
        "selecting the serif face of the collection must use the serif font"
    );
    assert_eq!(
        collection_sans, standalone_sans,
        "selecting the sans face of the collection must use the sans font"
    );
}

#[test]
fn family_change_on_relayout_uses_the_new_face() {
    let Some(collection) = test_collection() else {
        return;
    };

    let serif_options = options_for(&collection, Some("DejaVu Serif"));
    let mut doc = Document::with_options(fixture("leaves.epub"), &serif_options).expect("open");
    // Page 1 is the cover, which ignores the font; page 2 is body text.
    let serif_hash = document_hash(&mut doc, 2);

    // Same file, other face: this is the relayout path an EPUB takes when the
    // reader only changes the font face.
    let sans_options = options_for(&collection, Some("DejaVu Sans"));
    assert!(
        doc.relayout(&sans_options).expect("relayout"),
        "a family change must stay relayoutable"
    );
    let relaid_hash = document_hash(&mut doc, 2);
    assert_ne!(
        relaid_hash, serif_hash,
        "relayout must rebuild the font context when the family changes"
    );

    let mut fresh = Document::with_options(fixture("leaves.epub"), &sans_options).expect("open");
    assert_eq!(
        relaid_hash,
        document_hash(&mut fresh, 2),
        "relayout must land on the same face as opening the document with it"
    );
}

//! The network providers that serve a document's resources to Blitz.
//!
//! One per container: a zip (`EpubNet`), an in-memory or PalmDB map (`MapNet`),
//! a standalone document's directory (`FileNet`), and nothing (`EmptyNet`).
//! They share what changes what a resource *means* - the reader's font size for
//! a stylesheet's absolute sizes, and the "use the book's fonts" flag - which
//! live on the source so a relayout sees the current settings.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request};
use cre_epub::LazyResource;
use percent_encoding::percent_decode_str;

use crate::{
    GeneratedResources, ImageRaster, MapResources, absolute_font_size_keywords_become_px,
    generated_resource, normalize_path, strip_multi_column_declarations,
    widen_cell_dimensions_in_stylesheet,
};

/// The rewrites a stylesheet needs before Blitz parses it, whichever way the
/// book's CSS arrived: a `<style>` block inside a chunk's HTML (where the
/// passes in [`create_document`] see it) or a `<link>`ed sheet served as a
/// resource. Both are the same CSS, so both get the same treatment.
///
/// - absolute `font-size` keywords become the pixels a browser computes from
///   the reader's size, or a book's `x-large` heading stays at stylo's 16px
///   medium whatever the setting (see
///   [`absolute_font_size_keywords_become_px`]);
/// - the multi-column declarations and fixed table-cell sizes Blitz lays out
///   wrongly are removed (see [`strip_multi_column_declarations`] and
///   [`widen_cell_dimensions_in_stylesheet`]).
fn normalise_served_stylesheet(css: &str, font_size: f32) -> String {
    let css = strip_multi_column_declarations(css);
    let css = widen_cell_dimensions_in_stylesheet(&css);
    absolute_font_size_keywords_become_px(&css, font_size)
}

/// The reader's font size a net provider resolves served CSS against.
fn served_font_size(font_size: &std::sync::atomic::AtomicU32) -> f32 {
    f32::from_bits(font_size.load(std::sync::atomic::Ordering::Relaxed))
}

/// The stylesheet bytes a net provider hands to Blitz.
///
/// `fonts` is the reader's "use the book's fonts" setting: an `@font-face`
/// rule is dropped when it is off, which is how the setting reaches a sheet
/// that is linked rather than inlined.
fn served_stylesheet(css: &str, font_size: f32, fonts_enabled: bool) -> String {
    let css = if fonts_enabled {
        std::borrow::Cow::Borrowed(css)
    } else {
        cre_book::fonts::strip_font_faces(css)
    };
    normalise_served_stylesheet(css.as_ref(), font_size)
}

pub(crate) struct EpubNet {
    resource_provider: Arc<LazyResource>,
    svg: Arc<ImageRaster>,
    /// Shared with the `Document`: a relayout must see the reader's current
    /// "use the book's fonts" setting without rebuilding the provider.
    embedded_fonts: Arc<std::sync::atomic::AtomicBool>,
    /// Shared with the `Document` too: a stylesheet's absolute `font-size`
    /// keywords resolve against the reader's size.
    css_font_size: Arc<std::sync::atomic::AtomicU32>,
    /// One buffer per font face, so a chunk's document re-fetches the same
    /// bytes instead of decompressing (and pinning) its own copy.
    font_blobs: std::sync::Mutex<HashMap<String, Bytes>>,
    /// Resources the engine generated (page-fit wrappers, decoded `data:`
    /// pictures); consulted before the book itself.
    generated: GeneratedResources,
}

impl NetProvider for EpubNet {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let path = percent_decode_str(request.url.path())
            .decode_utf8_lossy()
            .trim_start_matches('/')
            .to_string();
        let key = normalize_path(&path);
        if let Some(bytes) = generated_resource(&self.generated, &key) {
            handler.bytes(request.url.to_string(), Bytes::from((*bytes).clone()));
            return;
        }
        // A document's `<body class="…">` becomes a `<section class="…">` in
        // the chunk, so the stylesheet's `body.class` rules have to lose the
        // `body` part to keep matching (a cover's `body.cov2 { background }`).
        //
        // The book's `@font-face` rules usually live here rather than in the
        // chunk HTML (a chapter links its stylesheet), so honouring
        // `crengine.doc.embedded.fonts` means dropping them before Blitz parses
        // the sheet: a family that no longer resolves falls back to the next
        // one in the list, which is the reader's font.
        if key.to_ascii_lowercase().ends_with(".css") {
            let bytes = self
                .resource_provider
                .get(&key)
                .map(|data| (*data).clone())
                .unwrap_or_default();
            let css = cre_book::css::body_selectors_to_class(&String::from_utf8_lossy(&bytes));
            let css = served_stylesheet(
                &css,
                served_font_size(&self.css_font_size),
                self.embedded_fonts
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
            handler.bytes(request.url.to_string(), Bytes::from(css.into_bytes()));
            return;
        }
        // A book's fonts are registered per document, and a document
        // re-fetches them, so hand out one buffer per face: a fresh copy per
        // chunk kept one decompressed copy per chunk alive for the session (a
        // 13 MB book with two TTFs cost 45 MB that way).
        if is_font_key(&key)
            && let Some(bytes) = self.resource_provider.get(&key)
        {
            let mut cache = self.font_blobs.lock().unwrap();
            let blob = match cache.get(&key) {
                Some(blob) => blob.clone(),
                None => {
                    let blob = Bytes::from((*bytes).clone());
                    // A book ships a handful of faces; anything beyond that is
                    // not worth pinning.
                    if cache.len() < 8 {
                        cache.insert(key.clone(), blob.clone());
                    }
                    blob
                }
            };
            drop(cache);
            handler.bytes(request.url.to_string(), blob);
            return;
        }
        // A chunk's pictures are laid out through a transparent SVG carrying
        // their intrinsic size, and the painter reads the pixels of the page
        // being drawn. So the layout only needs the image's header - and a zip
        // entry is one deflate stream, which is why reading the header is the
        // difference between decompressing every picture a chunk mentions and
        // decompressing none of them.
        if self.svg.is_content_image(&key)
            && let Some(header) = cre_book::images::image_header(&*self.resource_provider, &key)
            && let Some(placeholder) = self.svg.placeholder(&key, &header)
        {
            handler.bytes(request.url.to_string(), Bytes::from(placeholder));
            return;
        }
        let bytes = self
            .resource_provider
            .get(&key)
            .map(|data| (*data).clone())
            .unwrap_or_default();
        let bytes = self.svg.rasterize(&key, &bytes).unwrap_or(bytes);
        handler.bytes(request.url.to_string(), Bytes::from(bytes));
    }
}

/// Whether a resource is a font file, which Blitz reads whole.
fn is_font_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    [".ttf", ".otf", ".ttc", ".woff", ".woff2"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// Serves resources held in memory (FB2 binaries, MOBI/KF8 images), under
/// `binary/<name>` or a bare `<name>`.
pub(crate) struct MapNet {
    resources: MapResources,
    svg: Arc<ImageRaster>,
    /// One buffer per font face (see `EpubNet::font_blobs`).
    font_blobs: std::sync::Mutex<HashMap<String, Bytes>>,
    generated: GeneratedResources,
    /// A KF8 stylesheet is a served resource, so the "use the book's fonts"
    /// setting is applied when it is read.
    fonts_enabled: Arc<std::sync::atomic::AtomicBool>,
    /// The reader's size, for the absolute `font-size` keywords in a served
    /// stylesheet (see [`normalise_served_stylesheet`]).
    css_font_size: Arc<std::sync::atomic::AtomicU32>,
}

impl NetProvider for MapNet {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let path = percent_decode_str(request.url.path())
            .decode_utf8_lossy()
            .trim_start_matches('/')
            .to_string();
        let name = path.strip_prefix("binary/").unwrap_or(&path).to_string();
        let key = normalize_path(&name);
        // A book's fonts are registered per document and every chunk's
        // document fetches them again, so they are sliced once and shared
        // instead of copied per chunk.
        if is_font_key(&key)
            && let Some(bytes) = self.resources.get(&key)
        {
            let mut cache = self.font_blobs.lock().unwrap();
            let blob = match cache.get(&key) {
                Some(blob) => blob.clone(),
                None => {
                    let blob = Bytes::from(bytes.to_vec());
                    if cache.len() < 8 {
                        cache.insert(key.clone(), blob.clone());
                    }
                    blob
                }
            };
            drop(cache);
            handler.bytes(request.url.to_string(), blob);
            return;
        }
        // A KF8 book's CSS is a flow of its own, served from this table, so it
        // gets the rewrites a chunk's inline `<style>` gets in
        // `create_document` - and the book's `@font-face` rules are dropped
        // here when the reader turns its fonts off.
        if key.to_ascii_lowercase().ends_with(".css") {
            let raw = generated_resource(&self.generated, &key)
                .map(|bytes| (*bytes).clone())
                .or_else(|| self.resources.get(&key).map(|bytes| bytes.to_vec()))
                .unwrap_or_default();
            let css = served_stylesheet(
                &String::from_utf8_lossy(&raw),
                served_font_size(&self.css_font_size),
                self.fonts_enabled
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
            .into_bytes();
            handler.bytes(request.url.to_string(), Bytes::from(css));
            return;
        }
        // A page-fit wrapper (or a decoded `data:` picture) is generated for the
        // layout, not stored in the book: the plate itself is painted from the
        // resource it names.
        if let Some(bytes) = generated_resource(&self.generated, &key) {
            handler.bytes(request.url.to_string(), Bytes::from((*bytes).clone()));
            return;
        }
        // Slice the resource instead of copying it: the placeholder only needs
        // the header to learn the intrinsic size, and copying the whole record
        // here would fault every picture's pages in during the layout pass.
        let original = self.resources.get(&name);
        let bytes = match original.and_then(|bytes| self.svg.rasterize(&name, bytes)) {
            Some(png) => png,
            None => match original {
                Some(bytes) => self
                    .svg
                    .placeholder(&name, bytes)
                    .unwrap_or_else(|| bytes.to_vec()),
                None => Vec::new(),
            },
        };
        handler.bytes(request.url.to_string(), Bytes::from(bytes));
    }
}

/// Serves resources referenced by a standalone HTML document from the
/// document's own directory. Paths are resolved and canonicalized, and must
/// stay inside that directory.
pub(crate) struct FileNet {
    root: PathBuf,
    svg: Arc<ImageRaster>,
    embedded_fonts: Arc<std::sync::atomic::AtomicBool>,
    css_font_size: Arc<std::sync::atomic::AtomicU32>,
    /// One buffer per font face (see `EpubNet::font_blobs`).
    font_blobs: std::sync::Mutex<HashMap<String, Bytes>>,
    generated: GeneratedResources,
}

impl NetProvider for FileNet {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let decoded = percent_decode_str(request.url.path())
            .decode_utf8_lossy()
            .to_string();
        // Generated resources are keyed relative to the document's directory.
        let relative = decoded
            .strip_prefix(self.root.to_string_lossy().as_ref())
            .map(|rest| rest.trim_start_matches('/'))
            .unwrap_or(&decoded)
            .to_string();
        if let Some(bytes) = generated_resource(&self.generated, &normalize_path(&relative)) {
            handler.bytes(request.url.to_string(), Bytes::from((*bytes).clone()));
            return;
        }
        // Same font rule as `EpubNet`: a chunk's document re-fetches the
        // book's faces, so they are read once and shared.
        if is_font_key(&decoded)
            && let Ok(candidate) = std::fs::canonicalize(&decoded)
            && candidate.starts_with(&self.root)
            && candidate.is_file()
        {
            let mut cache = self.font_blobs.lock().unwrap();
            let key = candidate.to_string_lossy().to_string();
            let blob = match cache.get(&key) {
                Some(blob) => blob.clone(),
                None => {
                    let blob = Bytes::from(std::fs::read(&candidate).unwrap_or_default());
                    if cache.len() < 8 {
                        cache.insert(key, blob.clone());
                    }
                    blob
                }
            };
            drop(cache);
            handler.bytes(request.url.to_string(), blob);
            return;
        }

        // Same header rule as `EpubNet`: a content picture is laid out from its
        // header, so a standalone document's large JPEG is not read into memory
        // just to learn its size. Anything else (an SVG to rasterize, a font, a
        // stylesheet) needs the whole file.
        if self.svg.is_content_image(&relative)
            && let Ok(candidate) = std::fs::canonicalize(&decoded)
            && candidate.starts_with(&self.root)
            && candidate.is_file()
            && let Ok(file) = std::fs::File::open(&candidate)
        {
            let mut header = Vec::new();
            let mut limited = std::io::Read::take(file, 64 * 1024);
            if std::io::Read::read_to_end(&mut limited, &mut header).is_ok()
                && let Some(placeholder) = self.svg.placeholder(&relative, &header)
            {
                handler.bytes(request.url.to_string(), Bytes::from(placeholder));
                return;
            }
        }
        let mut bytes = Vec::new();
        if let Ok(candidate) = std::fs::canonicalize(&decoded)
            && candidate.starts_with(&self.root)
            && candidate.is_file()
        {
            bytes = std::fs::read(&candidate).unwrap_or_default();
        }
        // Same rule as `EpubNet`: a linked stylesheet carries the book's
        // `@font-face` rules, so it is where the font setting is applied, and
        // it gets the rewrites an inline `<style>` block gets.
        if decoded.to_ascii_lowercase().ends_with(".css") {
            bytes = served_stylesheet(
                &String::from_utf8_lossy(&bytes),
                served_font_size(&self.css_font_size),
                self.embedded_fonts
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
            .into_bytes();
        }
        let bytes = self.svg.rasterize(&relative, &bytes).unwrap_or(bytes);
        handler.bytes(request.url.to_string(), Bytes::from(bytes));
    }
}

pub(crate) struct EmptyNet;

impl NetProvider for EmptyNet {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        handler.bytes(request.url.to_string(), Bytes::from(Vec::new()));
    }
}

impl EpubNet {
    pub(crate) fn new(
        resource_provider: Arc<LazyResource>,
        svg: Arc<ImageRaster>,
        embedded_fonts: Arc<std::sync::atomic::AtomicBool>,
        css_font_size: Arc<std::sync::atomic::AtomicU32>,
        generated: GeneratedResources,
    ) -> Self {
        Self {
            resource_provider,
            svg,
            embedded_fonts,
            css_font_size,
            font_blobs: std::sync::Mutex::new(HashMap::new()),
            generated,
        }
    }
}

impl MapNet {
    pub(crate) fn new(
        resources: MapResources,
        svg: Arc<ImageRaster>,
        generated: GeneratedResources,
        fonts_enabled: Arc<std::sync::atomic::AtomicBool>,
        css_font_size: Arc<std::sync::atomic::AtomicU32>,
    ) -> Self {
        Self {
            resources,
            svg,
            font_blobs: std::sync::Mutex::new(HashMap::new()),
            generated,
            fonts_enabled,
            css_font_size,
        }
    }
}

impl FileNet {
    pub(crate) fn new(
        root: PathBuf,
        svg: Arc<ImageRaster>,
        embedded_fonts: Arc<std::sync::atomic::AtomicBool>,
        css_font_size: Arc<std::sync::atomic::AtomicU32>,
        generated: GeneratedResources,
    ) -> Self {
        Self {
            root,
            svg,
            embedded_fonts,
            css_font_size,
            font_blobs: std::sync::Mutex::new(HashMap::new()),
            generated,
        }
    }
}

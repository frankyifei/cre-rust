use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use percent_encoding::percent_decode_str;

const CONTAINER_PATH: &str = "META-INF/container.xml";

#[derive(Clone, Debug)]
pub struct ManifestItem {
    pub id: String,
    pub href: String,
    pub media_type: String,
    pub properties: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpineLayout {
    Reflowable,
    PrePaginated,
}

/// The initial containing block of a pre-paginated spine item.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FixedViewport {
    /// The `width`/`height` from the viewport meta (in CSS pixels).
    Size(f32, f32),
    /// `width=device-width, height=device-height`: resolve against the reader
    /// viewport, which cre-lib knows at layout time.
    Device,
}

#[derive(Clone, Debug)]
pub struct SpineItem {
    pub idref: String,
    pub href: String,
    pub media_type: String,
    pub properties: String,
    pub linear: bool,
}

#[derive(Clone, Debug)]
pub struct SpineDocument {
    pub index: usize,
    pub id: String,
    pub href: String,
    pub media_type: String,
    pub body_html: String,
    /// `class`/`style` of the document's own `<body>`, carried onto the section
    /// wrapper so `body.cov2 { … }` style rules keep matching (the body element
    /// itself does not survive the chunk assembly).
    pub body_class: Option<String>,
    pub body_style: Option<String>,
    pub css_urls: Vec<String>,
    pub inline_styles: Vec<String>,
    pub nonlinear: bool,
    pub layout: SpineLayout,
    pub viewport: Option<FixedViewport>,
}

#[derive(Clone, Debug)]
pub struct TocItem {
    pub title: String,
    /// Resolved resource path with optional `#fragment`.
    pub href: String,
    pub depth: usize,
}

#[derive(Clone, Debug)]
pub struct PageMapItem {
    pub label: String,
    /// Resolved resource path with optional `#fragment`.
    pub href: String,
}

/// Placeholder for the base font size in generated markup.
///
#[derive(Clone, Debug)]
pub struct HtmlOptions {
    pub line_height: f32,
    pub margin_x: f32,
    pub margin_y: f32,
}

impl Default for HtmlOptions {
    fn default() -> Self {
        Self {
            line_height: 1.5,
            margin_x: 40.0,
            margin_y: 32.0,
        }
    }
}

type LazyResourceZipArchive = zip::ZipArchive<std::fs::File>;

fn open_zip_archive(path: PathBuf) -> Option<LazyResourceZipArchive> {
    let file = std::fs::File::open(path).ok()?;
    zip::ZipArchive::new(file).ok()
}

/// How many decompressed resource bytes are kept cached. Images are read
/// during layout to learn their intrinsic size, so without a bound a book's
/// whole image payload would stay resident for the session.
const DEFAULT_RESOURCE_CACHE_BYTES: usize = 4 * 1024 * 1024;

fn resource_cache_bytes() -> usize {
    std::env::var("CRE_RESOURCE_CACHE_KB")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.saturating_mul(1024))
        .unwrap_or(DEFAULT_RESOURCE_CACHE_BYTES)
}

/// Image header reading and cost estimates live in `cre-book` now; the names
/// stay here for the generator and for `cre-lib`'s chunk budget.
// The shared book layer owns these; the names stay reachable here because the
// generator and cre-lib call them through this crate. (S6 moves the callers.)
pub use cre_book::css::*;
pub use cre_book::fonts::*;
pub use cre_book::html::*;
pub use cre_book::images::{
    estimated_image_bytes, image_mime, image_sources, image_sources_in,
    inline_svg_images_to_data_uris, is_image_bytes, svg_block_inline_images,
};
pub use cre_book::ruby::*;

pub struct LazyResource {
    pub epub_path: Option<PathBuf>,
    cache: Arc<Mutex<ResourceCache>>,
    /// Resources the engine itself produced: rewritten spine documents and the
    /// SVG wrappers generated for image-only blocks. They have no zip entry to
    /// read again, so the LRU must never evict them: a book with enough images
    /// pushes all of them out of a byte-bounded cache, and the pages that use
    /// them then render blank.
    pinned: Arc<Mutex<HashMap<String, Arc<Vec<u8>>>>>,
    zip_archive: Arc<Mutex<Option<LazyResourceZipArchive>>>,
}

/// Small byte-bounded LRU over decompressed resources.
#[derive(Default)]
struct ResourceCache {
    entries: HashMap<String, Arc<Vec<u8>>>,
    order: std::collections::VecDeque<String>,
    bytes: usize,
    limit: usize,
}

impl ResourceCache {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    fn get(&mut self, key: &str) -> Option<Arc<Vec<u8>>> {
        let value = self.entries.get(key).cloned()?;
        self.touch(key);
        Some(value)
    }

    fn insert(&mut self, key: String, value: Arc<Vec<u8>>) {
        self.bytes += value.len();
        self.entries.insert(key.clone(), value);
        self.touch(&key);
        while self.bytes > self.limit {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(removed.len());
            }
        }
    }

    fn touch(&mut self, key: &str) {
        if let Some(position) = self.order.iter().position(|entry| entry == key) {
            self.order.remove(position);
        }
        self.order.push_back(key.to_string());
    }
}

/// A zip archive is one kind of [`cre_book::ResourceProvider`]; the trait is all
/// that cre-lib needs to know about it.
impl cre_book::ResourceProvider for LazyResource {
    fn get(&self, key: &str) -> Option<std::sync::Arc<Vec<u8>>> {
        LazyResource::get(self, key)
    }

    fn estimated_decoded_bytes(&self, key: &str) -> Option<usize> {
        LazyResource::estimated_decoded_bytes(self, key)
    }

    fn cache_stats(&self) -> (usize, usize) {
        LazyResource::cache_stats(self)
    }

    fn pinned_bytes(&self) -> usize {
        LazyResource::pinned_bytes(self)
    }
}

impl LazyResource {
    pub fn new(path: impl AsRef<Path>) -> Self {
        let path = PathBuf::from(path.as_ref());
        let zip_archive = open_zip_archive(path.clone());
        LazyResource {
            epub_path: Some(path),
            cache: Arc::new(Mutex::new(ResourceCache::new(resource_cache_bytes()))),
            pinned: Arc::new(Mutex::new(HashMap::new())),
            zip_archive: Arc::new(Mutex::new(zip_archive)),
        }
    }

    /// Opens the archive and reports why when it is not a readable zip.
    ///
    /// Reading the whole file first would put the book in memory just to
    /// validate it, which is exactly the kind of spike a low-memory device
    /// cannot afford; the zip directory is enough.
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = PathBuf::from(path.as_ref());
        let zip_archive = open_zip_archive(path.clone())
            .with_context(|| format!("open epub zip {}", path.display()))?;
        Ok(LazyResource {
            epub_path: Some(path),
            cache: Arc::new(Mutex::new(ResourceCache::new(resource_cache_bytes()))),
            pinned: Arc::new(Mutex::new(HashMap::new())),
            zip_archive: Arc::new(Mutex::new(Some(zip_archive))),
        })
    }

    /// Uncompressed size of a resource without reading it.
    ///
    /// The zip directory has the size of every entry, and resources the engine
    /// generated are already in memory, so the open-time estimate can add up
    /// how much XHTML the spine holds without decompressing the book.
    pub fn uncompressed_size(&self, key: &str) -> Option<u64> {
        let key = normalize_path(key);
        if let Ok(pinned) = self.pinned.lock()
            && let Some(value) = pinned.get(&key)
        {
            return Some(value.len() as u64);
        }
        if let Ok(cache) = self.cache.lock()
            && let Some(value) = cache.entries.get(&key)
        {
            return Some(value.len() as u64);
        }
        let mut maybe_archive = self.zip_archive.lock().ok()?;
        let archive = maybe_archive.as_mut()?;
        let index = (match archive.index_for_name(&key) {
            Some(index) => Some(index),
            None => {
                let mut found: Option<usize> = None;
                for i in 0..archive.len() {
                    let Some(name) = archive.name_for_index(i) else {
                        continue;
                    };
                    if normalize_path(name) == key {
                        found = Some(i);
                        break;
                    }
                }
                found
            }
        })?;
        let entry = archive.by_index_raw(index).ok()?;
        if entry.is_dir() {
            return None;
        }
        Some(entry.size())
    }

    /// Whether a resource exists, without reading (or decompressing) it.
    pub fn contains(&self, key: &str) -> bool {
        self.uncompressed_size(key).is_some()
    }

    pub fn from_map(resources: HashMap<String, Vec<u8>>) -> Self {
        let mut cache = ResourceCache::new(resource_cache_bytes());
        for (key, value) in resources.into_iter() {
            cache.insert(normalize_path(&key), Arc::new(value));
        }
        LazyResource {
            epub_path: None,
            cache: Arc::new(Mutex::new(cache)),
            pinned: Arc::new(Mutex::new(HashMap::new())),
            zip_archive: Arc::new(Mutex::new(None)),
        }
    }

    /// Uncompressed size of a resource, straight from the zip directory.
    /// Used to estimate how much a chunk's images will cost when decoded,
    /// without reading (let alone decoding) them.
    pub fn entry_size(&self, key: &str) -> Option<usize> {
        let key = normalize_path(key);
        if let Ok(pinned) = self.pinned.lock()
            && let Some(hit) = pinned.get(&key)
        {
            return Some(hit.len());
        }
        if let Ok(cache) = self.cache.lock()
            && let Some(hit) = cache.entries.get(&key)
        {
            return Some(hit.len());
        }
        let mut maybe_archive = self.zip_archive.lock().ok()?;
        let archive = maybe_archive.as_mut()?;
        let index = match archive.index_for_name(&key) {
            Some(index) => Some(index),
            None => {
                let mut found = None;
                for index in 0..archive.len() {
                    if let Some(name) = archive.name_for_index(index)
                        && normalize_path(name) == key
                    {
                        found = Some(index);
                        break;
                    }
                }
                found
            }
        }?;
        let entry = archive.by_index(index).ok()?;
        Some(entry.size() as usize)
    }

    /// Estimated decoded (RGBA) size of an image, from its header when the
    /// bytes are available, else from the file size and format.
    pub fn estimated_decoded_bytes(&self, key: &str) -> Option<usize> {
        let key = normalize_path(key);
        if let Some(bytes) = self.get(&key)
            && let Some((width, height)) = image_dimensions(&bytes)
        {
            return Some((width as usize * height as usize * 4).max(64 * 1024));
        }
        self.entry_size(&key)
            .map(|size| cre_book::estimated_decoded_from_size(&key, size))
    }

    /// (entries, bytes) currently cached, for tests and diagnostics.
    pub fn cache_stats(&self) -> (usize, usize) {
        let cache = self.cache.lock().unwrap();
        (cache.entries.len(), cache.bytes)
    }

    pub fn get(&self, key: &str) -> Option<Arc<Vec<u8>>> {
        let key = normalize_path(key);
        if let Ok(pinned) = self.pinned.lock()
            && let Some(hit) = pinned.get(&key)
        {
            return Some(hit.clone());
        }
        if let Some(hit) = self.cache.lock().unwrap().get(&key) {
            return Some(hit);
        }
        let bytes = self.read_zip_entry(&key)?;
        let shared = Arc::new(bytes);
        self.cache.lock().unwrap().insert(key, shared.clone());
        Some(shared)
    }

    /// Stores a resource the engine produced. Unlike zip entries, it cannot be
    /// read back from the archive, so it is pinned for the session instead of
    /// going through the LRU.
    pub fn put(&self, key: &str, value: Arc<Vec<u8>>) {
        self.pinned
            .lock()
            .unwrap()
            .insert(normalize_path(key), value);
    }

    /// Bytes held by pinned engine-generated resources (diagnostics).
    pub fn pinned_bytes(&self) -> usize {
        self.pinned
            .lock()
            .map(|pinned| pinned.values().map(|value| value.len()).sum())
            .unwrap_or(0)
    }

    fn read_zip_entry(&self, key: &str) -> Option<Vec<u8>> {
        let mut maybe_archive = self.zip_archive.lock().unwrap();
        let archive = maybe_archive.as_mut()?;
        let index = (match archive.index_for_name(key) {
            Some(index) => Some(index),
            None => {
                let mut found: Option<usize> = None;
                for i in 0..archive.len() {
                    let Some(name) = archive.name_for_index(i) else {
                        continue;
                    };
                    if normalize_path(name) == key {
                        found = Some(i);
                        break;
                    }
                }
                found
            }
        })?;
        let mut entry = archive.by_index(index).ok()?;
        if entry.is_dir() {
            return None;
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        if std::io::copy(&mut entry, &mut bytes).is_err() {
            return None;
        }
        Some(bytes)
    }
}

pub struct EpubBook {
    pub title: Option<String>,
    pub creator: Option<String>,
    pub language: Option<String>,
    pub series: Option<String>,
    pub description: Option<String>,
    pub keywords: Option<String>,
    pub identifiers: Option<String>,
    pub opf_path: String,
    pub opf_dir: String,
    pub manifest: Vec<ManifestItem>,
    pub spine: Vec<SpineItem>,
    pub toc: Vec<TocItem>,
    pub page_map_source: String,
    pub page_map: Vec<PageMapItem>,
    pub resource_provider: LazyResource,
    pub global_layout: SpineLayout,
    /// Generated page-fit wrappers and the original image each stands for.
    pub page_fit_images: HashMap<String, PageFitImage>,
}

/// A generated page-fit wrapper: a transparent SVG with the page's size whose
/// pixels are painted from `source` by the renderer.
///
/// The wrapper used to inline the whole image as a base64 data URI, which kept
/// a second copy of every picture-only page's payload in memory (12.epub:
/// 10 MB). The layout only needs the wrapper's size, and painting goes through
/// the same placeholder path as any other image.
#[derive(Clone, Debug)]
pub struct PageFitImage {
    /// Original image resource, painted at draw time.
    pub source: String,
    pub width: u32,
    pub height: u32,
    /// False when the header could not be read: the picture then fills the
    /// page box instead of being contained.
    pub contain: bool,
}

impl EpubBook {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_provider(LazyResource::open(path.as_ref())?)
    }

    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        let mut archive = zip::ZipArchive::new(Cursor::new(data)).context("open epub zip")?;
        let mut resources = HashMap::new();

        for i in 0..archive.len() {
            let mut file = archive.by_index(i).context("read zip entry")?;
            if file.is_dir() {
                continue;
            }
            let name = normalize_path(file.name());
            let mut bytes = Vec::with_capacity(file.size() as usize);
            std::io::copy(&mut file, &mut bytes).context("decompress zip entry")?;
            resources.insert(name, bytes);
        }

        Self::from_provider(LazyResource::from_map(resources))
    }

    fn from_provider(provider: LazyResource) -> Result<Self> {
        let container = provider
            .get(CONTAINER_PATH)
            .context("epub has no META-INF/container.xml")?;
        let opf_path = parse_container(&container)?;
        let opf = provider
            .get(&opf_path)
            .with_context(|| format!("epub package document missing: {opf_path}"))?;
        let opf_text = decode_text(&opf);
        let opf_dir = parent_dir(&opf_path).to_string();

        let package = roxmltree::Document::parse(&opf_text).context("parse epub package")?;
        let root = package.root_element();

        let title = first_child_text(root, "title");
        let creator = first_child_text(root, "creator");
        let language = first_child_text(root, "language");
        let series = meta_content(root, "calibre:series");
        let description = first_child_text(root, "description");
        let keywords = element_texts(root, "subject");
        let identifiers = element_texts(root, "identifier");
        let global_layout = match meta_property_text(root, "rendition:layout")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "pre-paginated" => SpineLayout::PrePaginated,
            _ => SpineLayout::Reflowable,
        };

        let manifest_node = root
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == "manifest")
            .context("epub package has no manifest")?;
        let mut manifest = Vec::new();
        for item in manifest_node
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "item")
        {
            let id = item.attribute("id").unwrap_or_default().to_string();
            let href = item.attribute("href").unwrap_or_default().to_string();
            let media_type = item.attribute("media-type").unwrap_or_default().to_string();
            let properties = item.attribute("properties").unwrap_or_default().to_string();
            if id.is_empty() || href.is_empty() {
                continue;
            }
            manifest.push(ManifestItem {
                id,
                href: resolve_href(&opf_dir, &href),
                media_type,
                properties,
            });
        }

        let spine_node = root
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == "spine")
            .context("epub package has no spine")?;
        let mut spine = Vec::new();
        for itemref in spine_node
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "itemref")
        {
            let idref = itemref.attribute("idref").unwrap_or_default();
            let linear = itemref.attribute("linear") != Some("no");
            let Some(item) = manifest.iter().find(|item| item.id == idref) else {
                continue;
            };
            let itemref_props = itemref.attribute("properties").unwrap_or_default();
            let properties = format!("{} {}", item.properties, itemref_props);
            spine.push(SpineItem {
                idref: idref.to_string(),
                href: item.href.clone(),
                media_type: item.media_type.clone(),
                properties,
                linear,
            });
        }

        if spine.is_empty() {
            bail!("epub spine is empty");
        }

        let toc = parse_toc(&opf, spine_node, &manifest, &opf_dir, &provider);
        let (page_map_source, page_map) =
            parse_page_map(spine_node, &manifest, &opf_dir, &provider);

        Ok(Self {
            title,
            creator,
            language,
            series,
            description,
            keywords,
            identifiers,
            opf_path,
            opf_dir,
            manifest,
            spine,
            toc,
            page_map_source,
            page_map,
            resource_provider: provider,
            global_layout,
            page_fit_images: HashMap::new(),
        })
    }

    pub fn resource(&self, path: &str) -> Option<Vec<u8>> {
        let shared = self.resource_provider.get(path)?;
        Some((*shared).clone())
    }

    pub fn resource_url(&self, path: &str) -> String {
        format!("epub://book/{}", encode_path(&normalize_path(path)))
    }

    pub fn normalize_inline_svg_images(&mut self) -> usize {
        let spine = self.spine.clone();
        let mut updates = Vec::new();

        for item in spine.iter().filter(|item| item.linear) {
            if !is_html_resource(&item.media_type, &item.href) {
                continue;
            }
            let doc_key = normalize_path(&item.href);
            let Some(doc_bytes) = self.resource_provider.get(&doc_key) else {
                continue;
            };
            let text = decode_text(&doc_bytes);
            if !text.to_ascii_lowercase().contains("<svg") {
                continue;
            }

            let normalized = rewrite_inline_svg_image_blocks(
                &text,
                parent_dir(&doc_key),
                &self.resource_provider,
            );
            if normalized != text {
                updates.push((doc_key, normalized));
            }
        }

        let mut changed = 0usize;
        for (doc_key, normalized) in updates {
            self.resource_provider
                .put(&doc_key, Arc::new(normalized.into_bytes()));
            changed += 1;
        }
        changed
    }

    pub fn normalize_full_page_images(
        &mut self,
        page_width: f32,
        page_height: f32,
        margin_x: f32,
        margin_y: f32,
    ) -> usize {
        let available_width = page_width - margin_x * 2.0;
        let available_height = page_height - margin_y * 2.0;
        if available_width <= 1.0 || available_height <= 1.0 {
            return 0;
        }

        let spine = self.spine.clone();
        let mut updates = Vec::new();
        let mut generated_counter = 0usize;

        for item in spine.iter() {
            if !is_html_resource(&item.media_type, &item.href) {
                continue;
            }
            // A pre-paginated page is already laid out in its own viewport and
            // is scaled by the renderer. Applying the reflow image cap here
            // would scale the image a second time.
            if spine_layout(&item.properties, self.global_layout) == SpineLayout::PrePaginated {
                continue;
            }
            let doc_key = normalize_path(&item.href);
            let Some(doc_bytes) = self.resource_provider.get(&doc_key) else {
                continue;
            };
            let text = decode_text(&doc_bytes);
            if !text.to_ascii_lowercase().contains("<img") {
                continue;
            }

            // A page that holds nothing but one picture is a picture page: the
            // reader scales it to fill the page even when the file is smaller.
            // Pages with text keep the "never enlarge" behaviour of `max-width`.
            let image_only = document_is_single_image(&text);
            let mut generated = HashMap::new();
            let mut page_fit = HashMap::new();
            let normalized = rewrite_full_page_image_tags(
                &text,
                parent_dir(&doc_key),
                &self.resource_provider,
                &mut generated,
                &mut page_fit,
                &mut generated_counter,
                (available_width, available_height),
                image_only,
            );
            self.page_fit_images.extend(page_fit);
            if !generated.is_empty() || normalized != text {
                updates.push((doc_key, normalized, generated));
            }
        }

        let mut changed = 0usize;
        for (doc_key, normalized, generated) in updates {
            let generated_count = generated.len();
            for (key, value) in generated {
                self.resource_provider.put(&key, value);
            }
            self.resource_provider
                .put(&doc_key, Arc::new(normalized.into_bytes()));
            // A document can change without a generated resource when the
            // image only needs a fit style.
            changed += generated_count.max(1);
        }
        changed
    }

    /// Uncompressed bytes of XHTML in the spine, from the archive directory.
    ///
    /// This is the size the generated chunk HTML is made from, and it is
    /// available without decompressing or parsing anything, so the opening
    /// memory estimate can use it before the book is read.
    pub fn spine_text_bytes(&self) -> u64 {
        self.spine
            .iter()
            .filter(|item| item.linear || item.media_type.to_ascii_lowercase().contains("html"))
            .filter(|item| is_html_resource(&item.media_type, &item.href))
            .filter_map(|item| self.resource_provider.uncompressed_size(&item.href))
            .sum()
    }

    pub fn spine_documents(&self) -> Result<Vec<SpineDocument>> {
        let mut docs = Vec::new();
        for index in 0..self.spine.len() {
            if let Some(doc) = self.spine_document_at(index)? {
                docs.push(doc);
            }
        }
        if docs.is_empty() {
            bail!("no readable spine document");
        }
        Ok(docs)
    }

    /// Decodes one spine item on demand, or `None` when it is not part of the
    /// reading order or is missing from the archive.
    ///
    /// The open path uses this instead of [`Self::spine_documents`] so the
    /// book is never held as one `Vec` of every document's body.
    fn spine_document_at(&self, index: usize) -> Result<Option<SpineDocument>> {
        let item = &self.spine[index];
        let nonlinear = !item.linear;
        let include = item.linear || item.media_type.to_ascii_lowercase().contains("html");
        if !include {
            return Ok(None);
        }
        // A package can list documents that are not in the archive at all
        // (the Readium epub30-test-230 case). Refusing the whole book hides
        // everything else in it, so skip the missing item; its slot keeps
        // its spine index, so the items after it still map correctly.
        let Some(bytes) = self.resource_provider.get(&item.href) else {
            eprintln!("cre-epub: spine document missing: {}", item.href);
            return Ok(None);
        };
        let text = decode_text(&bytes);
        let layout = spine_layout(&item.properties, self.global_layout);
        // A pre-paginated XHTML page declares its initial containing block
        // with `meta name=viewport`; an SVG page uses its `viewBox` (or its
        // width/height) instead. A page with neither falls back to 800x600.
        let viewport = if layout == SpineLayout::PrePaginated {
            let svg_viewport = if is_svg_resource(&item.media_type, &item.href) {
                parse_svg_viewport(&text)
            } else {
                None
            };
            Some(
                parse_viewport(&text)
                    .or(svg_viewport)
                    .unwrap_or(FixedViewport::Size(800.0, 600.0)),
            )
        } else {
            None
        };
        let html_body = extract_body(&text);
        // A spine item does not have to be an XHTML document. EPUB allows
        // SVG and raster images (and, rarely, foreign XML/JSON) directly in
        // the spine. Turning the whole book into an open error because of
        // one such item makes otherwise readable books unusable. Render
        // images as an image page, and fall back to a placeholder for
        // foreign resources so the rest of the publication remains
        // accessible.
        let (mut body, has_html_body) = if let Some(body) = html_body {
            (body, true)
        } else if is_image_resource(&item.media_type, &item.href) {
            let file = item.href.rsplit('/').next().unwrap_or(&item.href);
            // A fixed-layout image page is its own viewport, so the image
            // has to fill it. In reflow the existing fit behaviour is kept.
            let style = if layout == SpineLayout::PrePaginated {
                " style=\"width:100%;height:auto\""
            } else {
                ""
            };
            (
                format!(
                    "<div class=\"cre-image-spine\"><img src=\"{}\"{style} alt=\"\"/></div>",
                    escape_attr(file)
                ),
                false,
            )
        } else if nonlinear {
            return Ok(None);
        } else {
            (
                format!(
                    "<p class=\"cre-unsupported\">Unsupported spine item: {}</p>",
                    escape_html(&item.href)
                ),
                false,
            )
        };
        if let Some(viewport) = viewport {
            // The marker lets cre-lib give this chunk its own layout
            // viewport. The page must remain a single chunk: mixing a
            // pre-paginated page into a reflow chunk would make all its
            // absolutely-positioned children share one containing block.
            let attrs = body_attributes(&text);
            let mut style = attrs.get("style").cloned().unwrap_or_default();
            if !style.trim().is_empty() && !style.trim_end().ends_with(';') {
                style.push(';');
            }
            let (marker, sizing) = match viewport {
                FixedViewport::Size(width, height) => (
                    format!("{width:.0}x{height:.0}"),
                    format!("width:{width:.0}px;height:{height:.0}px;"),
                ),
                FixedViewport::Device => ("device".to_string(), "width:100%;height:100%;".into()),
            };
            style.push_str(&format!(
                "position:absolute;left:0;top:0;{sizing}overflow:hidden;"
            ));
            let mut class = String::from("cre-fxl-page");
            if let Some(original) = attrs.get("class") {
                class.push(' ');
                class.push_str(original);
            }
            let id = attrs
                .get("id")
                .map(|id| format!(" id=\"{}\"", escape_attr(id)))
                .unwrap_or_default();
            body = format!(
                "<div class=\"{}\"{} data-cre-fixed-layout=\"{marker}\" style=\"{}\">{body}</div>",
                escape_attr(&class),
                id,
                escape_attr(&style)
            );
        }
        let (css_urls, inline_styles) = if has_html_body {
            extract_styles(&text, &self.opf_dir, &item.href)
        } else {
            (Vec::new(), Vec::new())
        };
        let body_attrs = body_attributes(&text);
        Ok(Some(SpineDocument {
            index,
            id: item.idref.clone(),
            href: item.href.clone(),
            media_type: item.media_type.clone(),
            body_class: body_attrs.get("class").cloned(),
            body_style: body_attrs.get("style").cloned(),
            body_html: rewrite_urls(&body, parent_dir(&item.href), &self.opf_dir),
            css_urls,
            inline_styles,
            nonlinear,
            layout,
            viewport,
        }))
    }

    pub fn to_html(&self, options: &HtmlOptions) -> Result<String> {
        let docs = self.spine_documents()?;
        let refs: Vec<&SpineDocument> = docs.iter().collect();
        let mut html = self.html_head(options, &refs);
        for doc in &docs {
            push_section_html(&mut html, doc, doc.index as u32 + 1, &doc.body_html, false);
        }
        html.push_str("</body></html>");
        Ok(html)
    }

    pub fn to_html_chunks(&self, options: &HtmlOptions, max_bytes: usize) -> Result<Vec<String>> {
        self.to_html_chunks_budgeted(options, max_bytes, None, usize::MAX)
    }

    /// Like [`Self::to_html_chunks`], but also caps how much image data a
    /// chunk may reference.
    ///
    /// blitz decodes a chunk's images while parsing it, so a text budget
    /// alone cannot bound memory on an illustrated book (measured: a 117 kB
    /// chunk holding 16 JPEGs grew RSS by 57 MB inside `from_html`).
    /// `image_budget` is in *estimated decoded* bytes, estimated from the zip
    /// directory entry size and the image format, without decoding anything.
    pub fn to_html_chunks_budgeted(
        &self,
        options: &HtmlOptions,
        max_bytes: usize,
        image_budget: Option<usize>,
        layout_bytes: usize,
    ) -> Result<Vec<String>> {
        let mut chunks = Vec::new();
        self.to_html_chunks_budgeted_stream(
            options,
            max_bytes,
            image_budget,
            layout_bytes,
            |chunk| {
                chunks.push(chunk);
                Ok(())
            },
        )?;
        Ok(chunks)
    }

    /// Streams the chunk HTML to `emit` one chunk at a time.
    ///
    /// The open path uses this so the book is never held as one `Vec` of every
    /// document's body. Each spine document is decoded on demand; only the
    /// current document and the chunk being assembled are resident before
    /// `emit` takes the chunk (and compresses it).
    pub fn to_html_chunks_budgeted_stream<F>(
        &self,
        options: &HtmlOptions,
        max_bytes: usize,
        image_budget: Option<usize>,
        layout_bytes: usize,
        emit: F,
    ) -> Result<()>
    where
        F: FnMut(String) -> Result<()>,
    {
        let mut builder = ChunkBuilder {
            book: self,
            options,
            max_bytes,
            image_budget,
            layout_bytes,
            body: String::new(),
            body_images: 0,
            chunk_docs: Vec::new(),
            emitted: 0,
            emit,
        };
        let mut next_extra_slot = self.included_spine_count() as u32 + 1;
        let mut any = false;
        for index in 0..self.spine.len() {
            let Some(doc) = self.spine_document_at(index)? else {
                continue;
            };
            any = true;
            let doc = Arc::new(doc);
            let pieces = self.spine_pieces(&doc, max_bytes, layout_bytes, &mut next_extra_slot);
            for (piece, images) in pieces {
                builder.push(&doc, &piece, images)?;
            }
        }
        if !any {
            bail!("no readable spine document");
        }
        builder.finish()
    }

    /// Number of spine documents that will be read, matching the length of
    /// [`Self::spine_documents`] without decompressing the archive.
    fn included_spine_count(&self) -> usize {
        self.spine
            .iter()
            .filter(|item| item.linear || item.media_type.to_ascii_lowercase().contains("html"))
            .filter(|item| self.resource_provider.contains(&item.href))
            .count()
    }

    /// Splits one document into the pieces the chunker packs: itself, or its
    /// text/layout parts when it is larger than a chunk.
    fn spine_pieces<'a>(
        &self,
        doc: &'a SpineDocument,
        max_bytes: usize,
        layout_bytes: usize,
        next_extra_slot: &mut u32,
    ) -> Vec<(Piece<'a>, usize)> {
        let real_slot = doc.index as u32 + 1;
        let mut pieces: Vec<Piece<'a>> = Vec::new();
        if doc.body_html.len() <= max_bytes || doc.layout == SpineLayout::PrePaginated {
            pieces.push(Piece {
                doc,
                body: std::borrow::Cow::Borrowed(doc.body_html.as_str()),
                slot: real_slot,
                continuation: false,
            });
        } else {
            for (part_index, part) in split_body_html(&doc.body_html, max_bytes)
                .into_iter()
                .enumerate()
            {
                let slot = if part_index == 0 {
                    real_slot
                } else {
                    let slot = *next_extra_slot;
                    *next_extra_slot += 1;
                    slot
                };
                pieces.push(Piece {
                    doc,
                    body: std::borrow::Cow::Owned(part),
                    slot,
                    continuation: part_index > 0,
                });
            }
        }

        let mut piece_images: Vec<usize> = pieces
            .iter()
            .map(|piece| self.piece_image_bytes(piece))
            .collect();

        // A piece that is heavy on its own (a gallery page, a comic with
        // several images in one XHTML) is split between its images, which is
        // where a page break already falls.
        if layout_bytes != usize::MAX {
            let mut expanded: Vec<Piece<'a>> = Vec::new();
            let mut expanded_images: Vec<usize> = Vec::new();
            let mut split_pending = false;
            for (piece, images) in pieces.into_iter().zip(piece_images) {
                if piece.body.len() * DOM_BYTES_PER_HTML_BYTE + images > layout_bytes {
                    let dir = parent_dir(&piece.doc.href);
                    let parts = split_body_html_for_layout(
                        &piece.body,
                        dir,
                        &self.resource_provider,
                        layout_bytes,
                        0,
                    );
                    if parts.len() > 1 {
                        for (part_index, part) in parts.into_iter().enumerate() {
                            let slot = if part_index == 0 {
                                piece.slot
                            } else {
                                let slot = *next_extra_slot;
                                *next_extra_slot += 1;
                                slot
                            };
                            split_pending |= part_index == 0;
                            expanded.push(Piece {
                                doc: piece.doc,
                                body: std::borrow::Cow::Owned(part),
                                slot,
                                continuation: piece.continuation || part_index > 0,
                            });
                            expanded_images.push(0);
                        }
                        continue;
                    }
                }
                expanded.push(piece);
                expanded_images.push(images);
            }
            if split_pending {
                for (index, piece) in expanded.iter().enumerate() {
                    if expanded_images[index] == 0 {
                        expanded_images[index] = self.piece_image_bytes(piece);
                    }
                }
            }
            pieces = expanded;
            piece_images = expanded_images;
        }

        pieces.into_iter().zip(piece_images).collect()
    }

    fn piece_image_bytes(&self, piece: &Piece<'_>) -> usize {
        let dir = parent_dir(&piece.doc.href);
        image_sources(&piece.body, dir)
            .iter()
            .filter_map(|key| self.resource_provider.estimated_decoded_bytes(key))
            .sum()
    }

    fn html_head(&self, options: &HtmlOptions, docs: &[&SpineDocument]) -> String {
        let mut css_urls = Vec::new();
        let mut seen_inline: Vec<&str> = Vec::new();
        for doc in docs {
            for url in &doc.css_urls {
                if !css_urls.contains(url) {
                    css_urls.push(url.clone());
                }
            }
        }

        let mut html = String::new();
        html.push_str("<!doctype html><html><head><meta charset=\"utf-8\">");
        html.push_str(&format!(
            "<title>{}</title>",
            escape_html(self.title.as_deref().unwrap_or(""))
        ));
        for url in &css_urls {
            html.push_str(&format!(
                "<link rel=\"stylesheet\" href=\"{}\">",
                escape_attr(url)
            ));
        }
        // A `<style>` block belongs to its own spine document, the way a browser
        // loads each XHTML separately. Chunks hold several documents at once, so
        // leaving these rules global let one document restyle the others: the
        // character-entity table's `td { height: 1em; line-height: 1em }` made
        // every other table in the chunk overlap its own rows.
        //
        // TODO(upstream): Blitz has no subtree-scoped stylesheet.
        // `BaseDocument::add_stylesheet_for_node` only records the owning node
        // (the sheet still applies document-wide) and `make_stylesheet` needs
        // stylo's `Origin`, which blitz-dom does not re-export. Until upstream
        // offers scoped sheets (or we render each spine item as a sub-document),
        // prefixing the selectors is the only way to keep a document's CSS to
        // itself. See docs/blitz-dom-adoption-plan.md, item C3.
        for doc in docs {
            let scope = format!("[data-spine-id=\"{}\"]", escape_attr(&doc.id));
            for style in &doc.inline_styles {
                if seen_inline.contains(&style.as_str()) {
                    continue;
                }
                seen_inline.push(style.as_str());
                html.push_str("<style>");
                html.push_str(&scope_stylesheet(style, &scope));
                html.push_str("</style>");
            }
        }
        // crengine gives every EPUB spine item its own <DocFragment> and its
        // epub.css puts `page-break-before: always` on it, so a chapter always
        // starts a new page. Without this, chapters that share a chunk would
        // flow into each other and page numbers would depend on chunk cuts.
        html.push_str("<style>/* cre-render-base */");
        html.push_str(&format!(
            "html, body {{ margin: 0; padding: 0; }}\n\
             html {{ font-size: {}px; }}\n\
             body {{ font-family: serif; font-size: 1em; line-height: {}; \
             padding: {}px {}px; }}\n\
             .cre-spine {{ display: flow-root; page-break-before: always; }}\n",
            cre_css::FONT_SIZE,
            options.line_height,
            options.margin_y,
            options.margin_x
        ));
        html.push_str("</style></head><body>");
        html
    }
}

/// One chunk-sized unit: a spine document, or one part of a document that is
/// larger than a chunk on its own.
struct Piece<'a> {
    doc: &'a SpineDocument,
    body: std::borrow::Cow<'a, str>,
    slot: u32,
    continuation: bool,
}

/// Greedily packs pieces into chunk HTML and hands each finished chunk to
/// `emit`. Holding the builder (not a `Vec<String>`) is what keeps the open
/// peak at one document plus one chunk.
struct ChunkBuilder<'a, F> {
    book: &'a EpubBook,
    options: &'a HtmlOptions,
    max_bytes: usize,
    image_budget: Option<usize>,
    layout_bytes: usize,
    body: String,
    body_images: usize,
    chunk_docs: Vec<Arc<SpineDocument>>,
    emitted: usize,
    emit: F,
}

impl<F> ChunkBuilder<'_, F>
where
    F: FnMut(String) -> Result<()>,
{
    fn flush(&mut self) -> Result<()> {
        let docs: Vec<&SpineDocument> = self.chunk_docs.iter().map(|doc| doc.as_ref()).collect();
        let mut html = self.book.html_head(self.options, &docs);
        html.push_str(&self.body);
        html.push_str("</body></html>");
        (self.emit)(html)?;
        self.body.clear();
        self.chunk_docs.clear();
        self.emitted += 1;
        Ok(())
    }

    fn push(&mut self, doc: &Arc<SpineDocument>, piece: &Piece<'_>, images: usize) -> Result<()> {
        let fixed_layout = piece.doc.layout == SpineLayout::PrePaginated;
        if fixed_layout && !self.body.is_empty() {
            self.flush()?;
            self.body_images = 0;
        }
        let mut section = String::new();
        let body_html = svg_hrefs_to_book_paths(&piece.body, parent_dir(&piece.doc.href));
        push_section_html(
            &mut section,
            piece.doc,
            piece.slot,
            &body_html,
            piece.continuation,
        );
        if !self.chunk_docs.iter().any(|known| Arc::ptr_eq(known, doc)) {
            self.chunk_docs.push(Arc::clone(doc));
        }
        let over_text = !self.body.is_empty() && self.body.len() + section.len() > self.max_bytes;
        let over_images = self.image_budget.is_some_and(|budget| {
            !self.body.is_empty() && self.body_images > 0 && self.body_images + images > budget
        });
        // The text and image caps are separate, but a chunk holds both at the
        // same time: bound their sum against the layout budget too.
        let over_layout = self.layout_bytes != usize::MAX
            && !self.body.is_empty()
            && (self.body.len() + section.len()) * DOM_BYTES_PER_HTML_BYTE
                + self.body_images
                + images
                > self.layout_bytes;
        if over_text || over_images || over_layout {
            self.flush()?;
            self.body_images = 0;
        }
        self.body.push_str(&section);
        self.body_images += images;
        // A part that was split out is already at the size limit; keep it in
        // its own chunk so the layout budget holds.
        if fixed_layout || piece.continuation || piece.body.len() >= self.max_bytes {
            self.flush()?;
            self.body_images = 0;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        if !self.body.is_empty() || self.emitted == 0 {
            self.flush()?;
        }
        Ok(())
    }
}

fn push_section_html(
    out: &mut String,
    doc: &SpineDocument,
    slot: u32,
    body: &str,
    continuation: bool,
) {
    let nonlinear_attr = if doc.nonlinear {
        " data-spine-nonlinear=\"1\""
    } else {
        ""
    };
    // A continuation part stays inside the same spine document; it must not
    // add another page break (the chunk boundary already starts a page).
    let mut class = if continuation {
        "cre-spine-cont".to_string()
    } else {
        "cre-spine".to_string()
    };
    if let Some(extra) = doc
        .body_class
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        class.push(' ');
        class.push_str(extra);
    }
    let style = doc
        .body_style
        .as_deref()
        .map(str::trim)
        .filter(|style| !style.is_empty())
        .map(|style| format!(" style=\"{}\"", escape_attr(style)))
        .unwrap_or_default();
    out.push_str(&format!(
        "<section class=\"{}\" data-spine-index=\"{slot}\" data-spine-real-index=\"{}\" \
         data-spine-id=\"{}\" data-spine-href=\"{}\"{nonlinear_attr}{style}>",
        escape_attr(&class),
        doc.index as u32 + 1,
        escape_attr(&doc.id),
        escape_attr(&normalize_path(&doc.href)),
    ));
    out.push_str(body);
    out.push_str("</section>");
}

const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

struct OpenTag {
    name: String,
    start: String,
}

/// Splits a complete HTML document into parts of at most `limit` bytes,
/// repeating the document's head and closing tags in every part. Used for
/// formats that are produced as one document (FB2, TXT, standalone HTML), so
/// they do not have to be laid out in a single giant chunk either.
pub fn split_html_document(html: &str, limit: usize) -> Vec<String> {
    if limit == 0 || html.len() <= limit {
        return vec![html.to_string()];
    }
    let Some((head, body, tail)) = html_body_parts(html) else {
        return vec![html.to_string()];
    };
    let parts = split_html_body_parts(body, limit);
    if parts.len() <= 1 {
        return vec![html.to_string()];
    }
    parts
        .into_iter()
        .map(|part| format!("{head}{part}{tail}"))
        .collect()
}

/// Splits the `<body>` content of a document into parts of at most `limit`
/// bytes, giving every part but the first its own `data-spine-index` range.
///
/// This is [`split_html_document`] without the head and tail, for a caller
/// that already holds the generated body and wants to build one chunk at a
/// time instead of materializing the whole document first.
pub fn split_html_body_parts(body: &str, limit: usize) -> Vec<String> {
    if limit == 0 || body.len() <= limit {
        return vec![body.to_string()];
    }
    let parts = split_body_html(body, limit);
    if parts.len() <= 1 {
        return vec![body.to_string()];
    }
    parts
        .into_iter()
        .enumerate()
        .map(|(index, part)| {
            // A `data-spine-index` names the document fragment an XPointer
            // points into, and the layout maps it to one chunk. A section that
            // straddles a chunk cut would otherwise be named by the same slot
            // in two chunks, so the second one could never resolve; give every
            // part its own slot range. (Only EPUB uses these attributes today,
            // but single-document formats use this splitter.)
            if index == 0 {
                part
            } else {
                renumber_spine_indices(&part, index)
            }
        })
        .collect()
}

/// The `<head>…<body>` prefix, the body content and the `</body>…` tail of a
/// generated document.
///
/// A generator that keeps its body separately can hand its parts here instead
/// of building the whole document, so the complete HTML never exists.
pub fn html_body_parts(html: &str) -> Option<(&str, &str, &str)> {
    let lower = html.to_ascii_lowercase();
    let body_start = lower.find("<body")?;
    let body_open_end = body_start + lower[body_start..].find('>')? + 1;
    let body_close = lower.rfind("</body>")?;
    if body_close < body_open_end {
        return None;
    }
    Some((
        &html[..body_open_end],
        &html[body_open_end..body_close],
        &html[body_close..],
    ))
}

/// Splits a single-document book at its `<section class="cre-spine">` parts.
///
/// MOBI and KF8 keep the whole book in one flow; `cre-mobi`/`cre-kf8` rebuild
/// the original parts (the "HTML files" a converter shows) as sections. When
/// memory forces a book into several layout chunks, cutting at those boundaries
/// keeps every chunk a whole number of parts, so page numbers do not depend on
/// where a byte budget happened to fall and no section is split in two.
///
/// Returns `None` when the body is not a run of sections, or when one section
/// alone is larger than `limit` (the caller then falls back to the generic
/// splitter, which can cut inside a part).
pub fn split_html_document_at_sections(html: &str, limit: usize) -> Option<Vec<String>> {
    split_html_document_at_sections_with(html, limit, usize::MAX, usize::MAX, 128, |section| {
        (section.len(), 0)
    })
}

/// Like [`split_html_document_at_sections`], but a chunk also has to stay
/// within an estimated decoded-image budget and a layout budget.
///
/// `cost` returns the HTML bytes and the estimated decoded bytes of the images
/// of one section; `dom_factor` is how many bytes of DOM one HTML byte is
/// assumed to take, so `layout_limit` can be checked the way `chunk_budgets`
/// computes it for EPUB.
#[allow(clippy::too_many_arguments)]
pub fn split_html_document_at_sections_with<F>(
    html: &str,
    limit: usize,
    image_limit: usize,
    layout_limit: usize,
    dom_factor: usize,
    cost: F,
) -> Option<Vec<String>>
where
    F: Fn(&str) -> (usize, usize),
{
    if limit == 0 {
        return None;
    }
    let lower = html.to_ascii_lowercase();
    let body_start = lower.find("<body")?;
    let body_open_end = lower[body_start..].find('>')? + body_start + 1;
    let body_close = lower.rfind("</body>")?;
    if body_close < body_open_end {
        return None;
    }
    let body = &html[body_open_end..body_close];
    let (prefix_end, sections) = section_ranges_in(body, &lower[body_open_end..body_close])?;
    let head = &html[..body_open_end];
    let tail = &html[body_close..];

    let mut parts: Vec<String> = Vec::new();
    pack_sections(
        body,
        prefix_end,
        &sections,
        head,
        tail,
        limit,
        image_limit,
        layout_limit,
        dom_factor,
        cost,
        |part| parts.push(part),
    )?;
    Some(parts)
}

/// Splits a generated body at its `<section>` parts, handing one finished
/// chunk document to `emit` at a time.
///
/// `head` and `tail` are the parts of the document around the body
/// ([`html_body_parts`]); every chunk is `head + prefix + sections + tail`, so
/// the caller never holds the whole document. Returns `false` when the body is
/// not a run of sections, or when one section alone is larger than `limit`; the
/// caller then falls back to [`split_html_body_parts`].
#[allow(clippy::too_many_arguments)]
pub fn stream_html_document_at_sections_with<F, G>(
    head: &str,
    body: &str,
    tail: &str,
    limit: usize,
    image_limit: usize,
    layout_limit: usize,
    dom_factor: usize,
    cost: F,
    mut emit: G,
) -> bool
where
    F: Fn(&str) -> (usize, usize),
    G: FnMut(String),
{
    if limit == 0 {
        return false;
    }
    let Some((prefix_end, sections)) = section_ranges(body) else {
        return false;
    };
    pack_sections(
        body,
        prefix_end,
        &sections,
        head,
        tail,
        limit,
        image_limit,
        layout_limit,
        dom_factor,
        cost,
        &mut emit,
    )
    .is_some()
}

/// Byte ranges of a body that is a run of `<section>` parts, plus the end of
/// the repeated prefix before the first one.
fn section_ranges(body: &str) -> Option<(usize, Vec<std::ops::Range<usize>>)> {
    let lower = body.to_ascii_lowercase();
    section_ranges_in(body, &lower)
}

fn section_ranges_in(body: &str, lower_body: &str) -> Option<(usize, Vec<std::ops::Range<usize>>)> {
    let mut sections = Vec::new();
    let mut cursor = 0usize;
    let mut prefix_end = 0usize;
    let mut first = true;
    while let Some(pos) = lower_body[cursor..].find("<section") {
        let start = cursor + pos;
        let gap = &body[cursor..start];
        if first {
            // A KF8 body starts with the book's stylesheet before the first
            // part; every chunk needs it, so it becomes a repeated prefix.
            if !only_style_blocks(gap) {
                return None;
            }
            prefix_end = start;
            first = false;
        } else if !gap.trim().is_empty() {
            return None;
        }
        let close = lower_body[start..].find("</section>")? + start;
        let end = close + "</section>".len();
        sections.push(start..end);
        cursor = end;
    }
    if sections.is_empty() {
        return None;
    }
    if !body[cursor..].trim().is_empty() {
        return None;
    }
    Some((prefix_end, sections))
}

/// Packs sections into chunks and emits one finished document per chunk.
#[allow(clippy::too_many_arguments)]
fn pack_sections<F, G>(
    body: &str,
    prefix_end: usize,
    sections: &[std::ops::Range<usize>],
    head: &str,
    tail: &str,
    limit: usize,
    image_limit: usize,
    layout_limit: usize,
    dom_factor: usize,
    cost: F,
    mut emit: G,
) -> Option<()>
where
    F: Fn(&str) -> (usize, usize),
    G: FnMut(String),
{
    // A section too big for the *text* budget has to be cut by the generic
    // splitter, which can cut inside it; the caller falls back then. An image
    // overrun is not a reason to fall back: a section is indivisible here (it
    // carries a spine index), so it simply gets a chunk of its own.
    if sections
        .iter()
        .any(|range| cost(&body[range.clone()]).0 > limit)
    {
        return None;
    }

    let prefix = &body[..prefix_end];
    let mut current = String::new();
    let mut current_images = 0usize;
    for range in sections {
        let section = &body[range.clone()];
        let (text, images) = cost(section);
        let over_text = prefix.len() + current.len() + text > limit;
        let over_images = current_images + images > image_limit;
        let over_layout =
            (prefix.len() + current.len() + text) * dom_factor + current_images + images
                > layout_limit;
        if !current.is_empty() && (over_text || over_images || over_layout) {
            emit(format!("{head}{prefix}{current}{tail}"));
            current.clear();
            current_images = 0;
        }
        current.push_str(section);
        current_images += images;
    }
    if !current.is_empty() {
        emit(format!("{head}{prefix}{current}{tail}"));
    }
    Some(())
}

/// True when a fragment holds only `<style>`/`<script>` blocks and whitespace.
fn only_style_blocks(fragment: &str) -> bool {
    let mut rest = fragment;
    loop {
        let trimmed = rest.trim_start();
        let lower = trimmed.to_ascii_lowercase();
        let (open, close) = if lower.starts_with("<style") {
            ("<style", "</style")
        } else if lower.starts_with("<script") {
            ("<script", "</script")
        } else {
            return trimmed.trim().is_empty();
        };
        let _ = open;
        let Some(end) = lower.find(close) else {
            return false;
        };
        let after = lower[end..]
            .find('>')
            .map(|offset| end + offset + 1)
            .unwrap_or(lower.len());
        rest = &trimmed[after..];
    }
}

/// Shifts every `data-spine-index="N"` in a split part by `part * 100_000`
/// so slots stay unique across parts.
fn renumber_spine_indices(html: &str, part_index: usize) -> String {
    const NEEDLE: &str = "data-spine-index=\"";
    let base = part_index * 100_000;
    let mut out = String::with_capacity(html.len() + 16);
    let mut at = 0usize;
    while let Some(relative) = html[at..].find(NEEDLE) {
        let value_start = at + relative + NEEDLE.len();
        let Some(length) = html[value_start..].find('"').filter(|length| *length > 0) else {
            break;
        };
        let Ok(value) = html[value_start..value_start + length].parse::<u32>() else {
            break;
        };
        out.push_str(&html[at..value_start]);
        out.push_str(&(value + base as u32).to_string());
        at = value_start + length;
    }
    out.push_str(&html[at..]);
    out
}

/// Splits a spine item's body HTML into parts of at most `limit` bytes,
/// cutting only between elements and re-opening whatever is still open, so
/// every part is well-formed on its own.
fn split_body_html(body: &str, limit: usize) -> Vec<String> {
    if limit == 0 || body.len() <= limit {
        return vec![body.to_string()];
    }
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut open: Vec<OpenTag> = Vec::new();
    let mut floor = 0usize; // bytes in `current` that cannot be flushed
    // Whether anything besides re-opened start tags went into `current`.
    let mut dirty = false;

    let flush =
        |parts: &mut Vec<String>, current: &mut String, open: &[OpenTag], dirty: &mut bool| {
            let mut part = String::new();
            part.push_str(current);
            for tag in open.iter().rev() {
                part.push_str("</");
                part.push_str(&tag.name);
                part.push('>');
            }
            parts.push(part);
            current.clear();
            for tag in open {
                current.push_str(&tag.start);
            }
            *dirty = false;
            current.len()
        };

    let mut index = 0usize;
    while index < body.len() {
        let next_tag = body[index..].find('<').map(|offset| index + offset);
        let text_end = next_tag.unwrap_or(body.len());
        if text_end > index {
            let mut text = &body[index..text_end];
            while !text.is_empty() {
                // Byte budget, floored to a character boundary: CJK text has
                // 3-byte characters, so a raw byte offset can land inside one.
                let room = floor_char_boundary(text, limit.saturating_sub(current.len()));
                if text.len() <= room {
                    if !text.trim().is_empty() {
                        dirty = true;
                    }
                    current.push_str(text);
                    break;
                }
                // Not enough room: cut at the last line break (or space) that
                // fits, so a body made of one long text run can still be split
                // into chunks.
                let cut = text[..room]
                    .rfind('\n')
                    .map(|position| position + 1)
                    .or_else(|| text[..room].rfind(' ').map(|position| position + 1))
                    .unwrap_or(room);
                if cut == 0 {
                    current.push_str(text);
                    break;
                }
                let (head, rest) = text.split_at(cut);
                if !head.trim().is_empty() {
                    dirty = true;
                }
                current.push_str(head);
                if current.len() > floor {
                    floor = flush(&mut parts, &mut current, &open, &mut dirty);
                }
                text = rest;
            }
        }
        let Some(tag_start) = next_tag else {
            break;
        };

        // Comments may contain '<'.
        if body[tag_start..].starts_with("<!--") {
            let end = body[tag_start..]
                .find("-->")
                .map(|offset| tag_start + offset + 3)
                .unwrap_or(body.len());
            let raw = &body[tag_start..end];
            if current.len() + raw.len() > limit && current.len() > floor {
                floor = flush(&mut parts, &mut current, &open, &mut dirty);
            }
            dirty = true;
            current.push_str(raw);
            index = end;
            continue;
        }

        let Some(tag_end) = find_tag_end(body, tag_start) else {
            current.push_str(&body[tag_start..]);
            break;
        };
        let raw = &body[tag_start..tag_end];
        let name = tag_name(raw);
        let closing = raw.starts_with("</");
        let self_closing = raw.trim_end().ends_with("/>");

        if closing {
            if let Some(position) = open.iter().rposition(|tag| tag.name == name) {
                open.truncate(position);
            }
            current.push_str(raw);
        } else if self_closing || VOID_ELEMENTS.contains(&name.as_str()) {
            current.push_str(raw);
        } else {
            if current.len() + raw.len() > limit && current.len() > floor {
                floor = flush(&mut parts, &mut current, &open, &mut dirty);
            }
            dirty = true;
            current.push_str(raw);
            if matches!(name.as_str(), "style" | "script" | "textarea" | "title") {
                // Raw-text elements: copy their content verbatim so '<' inside
                // them is not parsed as markup.
                let close = find_closing_tag(body, tag_end, &name).unwrap_or(body.len());
                current.push_str(&body[tag_end..close]);
                let close_end = find_tag_end(body, close).unwrap_or(body.len());
                current.push_str(&body[close..close_end]);
                index = close_end;
                continue;
            }
            open.push(OpenTag {
                name,
                start: raw.to_string(),
            });
        }
        index = tag_end;
    }
    // A trailing part that only re-opened tags has no content of its own; it
    // would render as a blank page between two chapters.
    if !current.is_empty() && (dirty || parts.is_empty()) {
        parts.push(current);
    }
    if parts.is_empty() {
        parts.push(String::new());
    }
    parts
}

/// True when the body carries no text of its own, only elements: an image
/// gallery, where splitting between images cannot break a paragraph.
fn body_is_image_only(body: &str) -> bool {
    let mut index = 0usize;
    while index < body.len() {
        let Some(open) = body[index..].find('<').map(|offset| index + offset) else {
            return body[index..].trim().is_empty();
        };
        if !body[index..open].trim().is_empty() {
            return false;
        }
        if body[open..].starts_with("<!--") {
            index = body[open..]
                .find("-->")
                .map(|offset| open + offset + 3)
                .unwrap_or(body.len());
            continue;
        }
        let Some(end) = find_tag_end(body, open) else {
            return false;
        };
        let name = tag_name(&body[open..end]);
        if matches!(name.as_str(), "script" | "style" | "textarea" | "title") {
            let close = find_closing_tag(body, end, &name).unwrap_or(body.len());
            if !body[end..close].trim().is_empty() {
                return false;
            }
            index = find_tag_end(body, close).unwrap_or(body.len());
            continue;
        }
        index = end;
    }
    true
}

/// Splits an image-only body so that each part's text DOM plus decoded images
/// fits `budget`.
///
/// The split points come from `split_body_html`, which cuts at element
/// boundaries and closes/re-opens the tags around the cut, so every part stays
/// valid HTML. Only bodies without text are split: cutting a paragraph would
/// move text onto another page, while an image gallery already puts each image
/// on its own page.
fn split_body_html_for_layout(
    body: &str,
    doc_dir: &str,
    provider: &LazyResource,
    budget: usize,
    depth: usize,
) -> Vec<String> {
    if budget == 0 || depth > 4 || body.len() < 2 || !body_is_image_only(body) {
        return vec![body.to_string()];
    }
    let images: usize = image_sources(body, doc_dir)
        .iter()
        .filter_map(|key| provider.estimated_decoded_bytes(key))
        .sum();
    let cost = body.len() * DOM_BYTES_PER_HTML_BYTE + images;
    if cost <= budget {
        return vec![body.to_string()];
    }
    let parts_wanted = cost.div_ceil(budget).max(1);
    let limit = (body.len() / parts_wanted).max(1);
    let parts = split_body_html(body, limit);
    if parts.len() <= 1 {
        return vec![body.to_string()];
    }
    let mut out = Vec::new();
    for part in parts {
        out.extend(split_body_html_for_layout(
            &part,
            doc_dir,
            provider,
            budget,
            depth + 1,
        ));
    }
    out
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn svg_hrefs_to_book_paths(html: &str, doc_dir: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    while let Some(open_start) = next_tag_start(html, cursor, "<svg") {
        let Some((_close_start, close_end)) =
            find_matching_close_tag(html, open_start, "<svg", "</svg")
        else {
            break;
        };
        out.push_str(&html[cursor..open_start]);
        let block = &html[open_start..close_end];
        let mut block_out = String::with_capacity(block.len());
        let mut inner = 0usize;
        while let Some(start) = next_tag_start(block, inner, "<image") {
            let Some(end) = find_tag_end(block, start) else {
                break;
            };
            block_out.push_str(&block[inner..start]);
            let tag = &block[start..end];
            let attrs = parse_attrs(tag);
            let mut replaced = tag.to_string();
            if let Some(href) = attrs.get("xlink:href").or_else(|| attrs.get("href")) {
                let href = href.trim();
                if !href.is_empty() && !href.starts_with("data:") && !is_external(href) {
                    let resolved = resolve_href(doc_dir, &value_without_fragment(href));
                    let url = format!("epub://book/{}", encode_path(&resolved));
                    replaced = replace_attribute_value(tag, "xlink:href", &url)
                        .or_else(|| replace_attribute_value(tag, "href", &url))
                        .unwrap_or(replaced);
                }
            }
            block_out.push_str(&replaced);
            inner = end;
        }
        block_out.push_str(&block[inner..]);
        out.push_str(&block_out);
        cursor = close_end;
    }
    if cursor == 0 {
        return html.to_string();
    }
    out.push_str(&html[cursor..]);
    out
}

/// Estimated decoded (RGBA) bytes of the images a generated chunk refers to.
///
/// Chunk HTML points at resources through `epub://book/`, so no document
/// directory is needed. `cre-lib` uses this to include the decoded-image cost
/// of a chunk in its low-memory estimate.
fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() > 24 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return Some((width, height));
    }
    if bytes.len() > 10 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        let width = u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32;
        let height = u16::from_le_bytes(bytes[8..10].try_into().ok()?) as u32;
        return Some((width, height));
    }
    if bytes.len() > 4 && bytes.starts_with(b"\xff\xd8") {
        let mut index = 2usize;
        while index + 9 < bytes.len() {
            if bytes[index] != 0xFF {
                index += 1;
                continue;
            }
            let marker = bytes[index + 1];
            if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
                index += 2;
                continue;
            }
            let length = u16::from_be_bytes(bytes[index + 2..index + 4].try_into().ok()?) as usize;
            if matches!(
                marker,
                0xC0 | 0xC1
                    | 0xC2
                    | 0xC3
                    | 0xC5
                    | 0xC6
                    | 0xC7
                    | 0xC9
                    | 0xCA
                    | 0xCB
                    | 0xCD
                    | 0xCE
                    | 0xCF
            ) {
                let height =
                    u16::from_be_bytes(bytes[index + 5..index + 7].try_into().ok()?) as u32;
                let width = u16::from_be_bytes(bytes[index + 7..index + 9].try_into().ok()?) as u32;
                return Some((width, height));
            }
            index += 2 + length;
        }
    }
    None
}

pub fn chunk_image_bytes(provider: &LazyResource, chunk_html: &str) -> usize {
    image_sources(chunk_html, "")
        .iter()
        .filter_map(|key| provider.estimated_decoded_bytes(key))
        .sum()
}

/// Family names declared by the book's stylesheet resources.
///
/// A chapter usually keeps its CSS in `<link rel="stylesheet">` rather than an
/// inline `<style>` (Calibre's output does), so the `@font-face` rules a chunk
/// is laid out with are not in the chunk HTML at all: Blitz fetches them from
/// the stylesheet through `cre_lib::EpubNet`. Reading them here keeps
/// [`EpubBook::manifest`] the single source of truth for what the book brings.
pub fn font_faces_in_stylesheets(book: &EpubBook) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in &book.manifest {
        let is_css = item.media_type.eq_ignore_ascii_case("text/css")
            || item.href.to_ascii_lowercase().ends_with(".css");
        if !is_css {
            continue;
        }
        // `ManifestItem::href` is already resolved against the OPF directory
        // when the package is parsed.
        let key = item.href.clone();
        let Some(bytes) = book.resource_provider.get(&key) else {
            continue;
        };
        for name in font_faces_in(&String::from_utf8_lossy(&bytes)) {
            if !out
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&name))
            {
                out.push(name);
            }
        }
    }
    out
}

/// Memory a chunk's text costs per byte of generated HTML (DOM nodes,
/// computed styles and line boxes). Matches `cre-lib`'s chunk planner.
const DOM_BYTES_PER_HTML_BYTE: usize = 128;

fn find_closing_tag(body: &str, from: usize, name: &str) -> Option<usize> {
    let needle = format!("</{name}");
    let haystack = body.get(from..)?;
    haystack.find(&needle).map(|offset| from + offset)
}

fn parse_container(bytes: &[u8]) -> Result<String> {
    let text = decode_text(bytes);
    let doc = roxmltree::Document::parse(&text).context("parse META-INF/container.xml")?;
    let rootfile = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "rootfile")
        .context("container.xml has no rootfile")?;
    let path = rootfile
        .attribute("full-path")
        .context("container.xml rootfile has no full-path")?;
    Ok(normalize_path(path))
}

fn first_child_text(root: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    root.descendants()
        .find(|n| n.is_element() && n.tag_name().name() == name)
        .and_then(|n| n.text())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn element_texts(root: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    let mut values = Vec::new();
    for node in root
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == name)
    {
        let Some(text) = node
            .text()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        if !values.contains(&text) {
            values.push(text);
        }
    }
    (!values.is_empty()).then(|| values.join(", "))
}

fn meta_content(root: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    root.descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "meta")
        .find(|n| {
            n.attribute("name")
                .is_some_and(|value| value.eq_ignore_ascii_case(name))
        })
        .and_then(|n| n.attribute("content"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn meta_property_text(root: roxmltree::Node<'_, '_>, property: &str) -> Option<String> {
    root.descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "meta")
        .find(|n| {
            n.attribute("property")
                .is_some_and(|value| value.eq_ignore_ascii_case(property))
        })
        .and_then(|n| n.text())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn parse_viewport(html: &str) -> Option<FixedViewport> {
    let lower = html.to_ascii_lowercase();
    let mut search = 0usize;
    while let Some(rel) = lower[search..].find('<') {
        let start = search + rel;
        let Some(rel_end) = lower[start..].find('>') else {
            break;
        };
        let end = start + rel_end;
        let tag = &html[start..=end];
        search = end + 1;
        if !tag.to_ascii_lowercase().starts_with("<meta") {
            continue;
        }
        let attrs = parse_attrs(tag);
        if !attrs
            .get("name")
            .is_some_and(|value| value.eq_ignore_ascii_case("viewport"))
        {
            continue;
        }
        // Only the first viewport declaration in the document is used, even if
        // it is invalid; later declarations must be ignored.
        let content = attrs.get("content")?;
        return parse_viewport_content(content);
    }
    None
}

fn parse_viewport_content(content: &str) -> Option<FixedViewport> {
    let mut width = None;
    let mut height = None;
    let mut has_device_width = false;
    let mut has_device_height = false;
    for part in content.split([',', ';', ' ']) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        // The first occurrence of width/height wins; later duplicates are
        // ignored (EPUB FXL allows repeating a declaration, and the first one
        // is the effective value).
        match key.as_str() {
            "width" if width.is_none() && !has_device_width => {
                if value.eq_ignore_ascii_case("device-width") {
                    has_device_width = true;
                } else {
                    width = parse_css_number(value);
                }
            }
            "height" if height.is_none() && !has_device_height => {
                if value.eq_ignore_ascii_case("device-height") {
                    has_device_height = true;
                } else {
                    height = parse_css_number(value);
                }
            }
            _ => {}
        }
    }
    if has_device_width || has_device_height {
        return Some(FixedViewport::Device);
    }
    if let (Some(width), Some(height)) = (width, height)
        && width > 0.0
        && height > 0.0
    {
        return Some(FixedViewport::Size(width, height));
    }
    None
}

/// Parses the leading numeric part of a CSS length, ignoring units. EPUB FXL
/// viewport declarations are unitless pixels, but some books write `1000px` or
/// even `600mm`; the numeric value is still the intended viewport dimension
/// (`lay-fxl-xhtml-icb_units` expects both pages to look identical).
fn parse_css_number(value: &str) -> Option<f32> {
    let value = value.trim();
    let end = value
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+')))
        .unwrap_or(value.len());
    let number = value[..end].parse::<f32>().ok()?;
    (number.is_finite() && number > 0.0).then_some(number)
}

/// The initial containing block of a standalone SVG spine document is its
/// `viewBox`, falling back to explicit width/height.
fn parse_svg_viewport(svg: &str) -> Option<FixedViewport> {
    let lower = svg.to_ascii_lowercase();
    let start = lower.find("<svg")?;
    let end = start + lower[start..].find('>')?;
    let attrs = parse_attrs(&svg[start..=end]);
    if let Some(view_box) = attrs.get("viewbox") {
        let parts: Vec<f32> = view_box
            .split([' ', ','])
            .filter(|part| !part.trim().is_empty())
            .filter_map(|part| part.trim().parse::<f32>().ok())
            .collect();
        if parts.len() == 4 {
            let width = parts[2].abs();
            let height = parts[3].abs();
            if width > 0.0 && height > 0.0 {
                return Some(FixedViewport::Size(width, height));
            }
        }
    }
    let width = attrs.get("width").and_then(|value| parse_css_number(value));
    let height = attrs
        .get("height")
        .and_then(|value| parse_css_number(value));
    match (width, height) {
        (Some(width), Some(height)) => Some(FixedViewport::Size(width, height)),
        _ => None,
    }
}

fn extract_body(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let body_start = lower.find("<body")?;
    let open_end = lower[body_start..].find('>')? + body_start + 1;
    let close_start = lower.rfind("</body")?;
    if close_start <= open_end {
        return None;
    }
    Some(html[open_end..close_start].to_string())
}

fn body_attributes(html: &str) -> HashMap<String, String> {
    let lower = html.to_ascii_lowercase();
    let Some(body_start) = lower.find("<body") else {
        return HashMap::new();
    };
    let Some(rel_end) = lower[body_start..].find('>') else {
        return HashMap::new();
    };
    let end = body_start + rel_end;
    parse_attrs(&html[body_start..=end])
}

fn extract_styles(html: &str, opf_dir: &str, doc_path: &str) -> (Vec<String>, Vec<String>) {
    let lower = html.to_ascii_lowercase();
    let head_end = lower.find("</head").unwrap_or(html.len());
    let head = &html[..head_end];
    let doc_dir = parent_dir(doc_path);
    let mut css_urls = Vec::new();
    let mut styles = Vec::new();

    let mut cursor = 0;
    while let Some(pos) = head[cursor..].find("<link") {
        let start = cursor + pos;
        let end = head[start..]
            .find('>')
            .map(|p| p + start + 1)
            .unwrap_or(head.len());
        let tag = &head[start..end];
        let attrs = parse_attrs(tag);
        let rel = attrs
            .get("rel")
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        if rel.split_ascii_whitespace().any(|r| r == "stylesheet")
            && let Some(href) = attrs.get("href")
        {
            let path = resolve_href(doc_dir, href);
            css_urls.push(format!(
                "epub://book/{}",
                encode_path(&normalize_path(&path))
            ));
        }
        cursor = end;
    }

    let mut cursor = 0;
    while let Some(pos) = lower[cursor..].find("<style") {
        let start = cursor + pos;
        let text_start = lower[start..]
            .find('>')
            .map(|p| p + start + 1)
            .unwrap_or(start);
        let Some(close) = lower[text_start..].find("</style") else {
            break;
        };
        let text_end = text_start + close;
        let css = clean_css(&html[text_start..text_end]);
        if !css.is_empty() {
            styles.push(rewrite_css_urls(&css, doc_dir));
        }
        cursor = text_end + 8;
    }

    let _ = opf_dir;
    (css_urls, styles)
}

/// Rewrites `url(...)` references inside a stylesheet so they point at the book
/// (`epub://book/...`). Background images declared in a document's CSS are
/// relative to that document otherwise, and the chunk's HTML lives at the book
/// root, so they silently failed to load.
fn rewrite_css_urls(css: &str, doc_dir: &str) -> String {
    let lower = css.to_ascii_lowercase();
    if !lower.contains("url(") {
        return css.to_string();
    }
    let mut out = String::with_capacity(css.len() + 64);
    let mut i = 0usize;
    while let Some(rel) = lower[i..].find("url(") {
        let start = i + rel;
        out.push_str(&css[i..start]);
        let mut j = start + 4;
        while j < css.len() && css.as_bytes()[j].is_ascii_whitespace() {
            j += 1;
        }
        let quote = css
            .as_bytes()
            .get(j)
            .copied()
            .filter(|byte| *byte == b'"' || *byte == b'\'');
        if quote.is_some() {
            j += 1;
        }
        let arg_start = j;
        while j < css.len() {
            let byte = css.as_bytes()[j];
            if quote.is_some_and(|q| byte == q) || (quote.is_none() && byte == b')') {
                break;
            }
            j += 1;
        }
        let arg = css[arg_start..j].trim();
        let mut end = j;
        if quote.is_some() && css.as_bytes().get(end) == quote.as_ref() {
            end += 1;
        }
        while end < css.len() && css.as_bytes()[end].is_ascii_whitespace() {
            end += 1;
        }
        if css.as_bytes().get(end) == Some(&b')') {
            end += 1;
        }
        if arg.is_empty() || is_external(arg) {
            out.push_str(&css[start..end]);
        } else {
            let path = resolve_href(doc_dir, &value_without_fragment(arg));
            out.push_str(&format!(
                "url(\"epub://book/{}\")",
                encode_path(&normalize_path(&path))
            ));
        }
        i = end;
        if i >= css.len() {
            break;
        }
    }
    out.push_str(&css[i..]);
    out
}

fn clean_css(raw: &str) -> String {
    let mut css = raw.trim();
    loop {
        let mut changed = false;
        if let Some(rest) = css.strip_prefix("<![CDATA[") {
            css = rest.trim();
            changed = true;
        }
        if let Some(rest) = css.strip_prefix("<!--") {
            css = rest.trim();
            changed = true;
        }
        if let Some(rest) = css.strip_suffix("]]>") {
            css = rest.trim_end();
            changed = true;
        }
        if let Some(rest) = css.strip_suffix("-->") {
            css = rest.trim_end();
            changed = true;
        }
        if !changed {
            break;
        }
    }
    css.to_string()
}

fn rewrite_urls(body: &str, doc_dir: &str, opf_dir: &str) -> String {
    let mut out = String::with_capacity(body.len() + 128);
    let mut i = 0;
    while i < body.len() {
        let Some(rel) = body[i..].find('<') else {
            out.push_str(&body[i..]);
            break;
        };
        let tag_start = i + rel;
        out.push_str(&body[i..tag_start]);
        let Some(rel_end) = body[tag_start..].find('>') else {
            out.push_str(&body[tag_start..]);
            break;
        };
        let tag_end = tag_start + rel_end + 1;
        let tag = &body[tag_start..tag_end];
        out.push_str(&rewrite_tag(tag, doc_dir, opf_dir));
        i = tag_end;
    }
    out
}

fn rewrite_tag(tag: &str, doc_dir: &str, opf_dir: &str) -> String {
    if tag.starts_with("<!") || tag.starts_with("<?") || tag.starts_with("</") {
        return tag.to_string();
    }
    let mut out = String::with_capacity(tag.len() + 64);
    let mut i = 0;
    while i < tag.len() {
        let Some(c) = tag[i..].chars().next() else {
            break;
        };
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            i += c.len_utf8();
            while i < tag.len() {
                let ch = tag[i..].chars().next().unwrap();
                if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | ':') {
                    i += ch.len_utf8();
                } else {
                    break;
                }
            }
            let name = &tag[start..i];
            let lower = name.to_ascii_lowercase();
            out.push_str(name);
            if matches!(lower.as_str(), "src" | "href" | "poster" | "data" | "style") {
                while i < tag.len() && tag[i..].chars().next().is_some_and(|c| c.is_whitespace()) {
                    out.push(tag[i..].chars().next().unwrap());
                    i += 1;
                }
                if i < tag.len() && tag[i..].starts_with('=') {
                    out.push('=');
                    i += 1;
                    while i < tag.len()
                        && tag[i..].chars().next().is_some_and(|c| c.is_whitespace())
                    {
                        out.push(tag[i..].chars().next().unwrap());
                        i += 1;
                    }
                    if i < tag.len() {
                        let quote = tag[i..].chars().next().unwrap();
                        if quote == '"' || quote == '\'' {
                            i += 1;
                            let value_start = i;
                            while i < tag.len() {
                                let ch = tag[i..].chars().next().unwrap();
                                if ch == quote {
                                    break;
                                }
                                i += ch.len_utf8();
                            }
                            let value = &tag[value_start..i];
                            let rewritten = if lower == "style" {
                                // CSS in a `style` attribute carries its own
                                // `url(...)` references (background images).
                                rewrite_css_urls(value, doc_dir)
                            } else if is_external(value) {
                                value.to_string()
                            } else {
                                let path = resolve_href(doc_dir, &value_without_fragment(value));
                                let mut url =
                                    format!("epub://book/{}", encode_path(&normalize_path(&path)));
                                if let Some(fragment) = value.split_once('#').map(|(_, f)| f) {
                                    url.push('#');
                                    url.push_str(fragment);
                                }
                                url
                            };
                            out.push(quote);
                            out.push_str(&escape_attr(&rewritten));
                            out.push(quote);
                            if i < tag.len() {
                                i += 1;
                            }
                        } else {
                            let value_start = i;
                            while i < tag.len() {
                                let ch = tag[i..].chars().next().unwrap();
                                if ch.is_whitespace() || ch == '>' {
                                    break;
                                }
                                i += ch.len_utf8();
                            }
                            let value = &tag[value_start..i];
                            let rewritten = if lower == "style" {
                                // CSS in a `style` attribute carries its own
                                // `url(...)` references (background images).
                                rewrite_css_urls(value, doc_dir)
                            } else if is_external(value) {
                                value.to_string()
                            } else {
                                let path = resolve_href(doc_dir, &value_without_fragment(value));
                                let mut url =
                                    format!("epub://book/{}", encode_path(&normalize_path(&path)));
                                if let Some(fragment) = value.split_once('#').map(|(_, f)| f) {
                                    url.push('#');
                                    url.push_str(fragment);
                                }
                                url
                            };
                            out.push_str(&escape_attr(&rewritten));
                        }
                    }
                }
            }
        } else {
            out.push(c);
            i += c.len_utf8();
        }
    }
    let _ = opf_dir;
    out
}

fn is_html_resource(media_type: &str, href: &str) -> bool {
    if media_type.to_ascii_lowercase().contains("html") {
        return true;
    }
    matches!(file_extension(href).as_str(), "xhtml" | "html" | "htm")
}

/// True for resources that a reading system can show as an image page when
/// they appear directly in the spine.
fn is_image_resource(media_type: &str, href: &str) -> bool {
    if media_type.to_ascii_lowercase().starts_with("image/") {
        return true;
    }
    matches!(
        file_extension(href).as_str(),
        "svg" | "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
    )
}

fn is_svg_resource(media_type: &str, href: &str) -> bool {
    media_type.eq_ignore_ascii_case("image/svg+xml") || file_extension(href) == "svg"
}

fn spine_layout(properties: &str, global: SpineLayout) -> SpineLayout {
    // When a spine item repeats the layout property, the first occurrence wins
    // (`fxl-spine-overrides_duplicate` overrides to reflowable and then writes
    // pre-paginated again; the document must stay reflowable).
    for property in properties.split_whitespace() {
        match property.to_ascii_lowercase().as_str() {
            "rendition:layout-pre-paginated" | "rendition:layout-pre-paginated-spreads" => {
                return SpineLayout::PrePaginated;
            }
            "rendition:layout-reflowable" => return SpineLayout::Reflowable,
            _ => {}
        }
    }
    global
}

fn rewrite_inline_svg_image_blocks(html: &str, doc_dir: &str, provider: &LazyResource) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;

    while cursor < html.len() {
        let Some(open_start) = next_tag_start(&lower, cursor, "<svg") else {
            out.push_str(&html[cursor..]);
            break;
        };
        out.push_str(&html[cursor..open_start]);

        let Some((close_start, close_end)) =
            find_matching_close_tag(&lower, open_start, "<svg", "</svg")
        else {
            out.push_str(&html[open_start..]);
            break;
        };

        if let Some(replacement) =
            replace_image_only_svg_block(html, open_start, close_start, doc_dir, provider)
        {
            out.push_str(&replacement);
        } else {
            out.push_str(&html[open_start..close_end]);
        }
        cursor = close_end;
    }

    out
}

fn replace_image_only_svg_block(
    html: &str,
    open_start: usize,
    close_start: usize,
    doc_dir: &str,
    provider: &LazyResource,
) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let svg_open_end = find_tag_end(html, open_start)?;
    if svg_open_end >= close_start {
        return None;
    }

    let image_start = next_tag_start(&lower, svg_open_end, "<image")?;
    if next_tag_start(&lower, image_start + "<image".len(), "<image").is_some() {
        return None;
    }
    let image_end = find_tag_end(html, image_start)?;
    if image_end >= close_start {
        return None;
    }
    let overlays_are_invisible = |from: usize, to: usize| -> bool {
        if !has_element_tag(html, from, to) {
            return !has_meaningful_text(&html[from..to]);
        }
        svg_overlays_are_invisible(html, from, to)
    };
    if !overlays_are_invisible(svg_open_end, image_start)
        || !overlays_are_invisible(image_end, close_start)
    {
        return None;
    }

    let svg_tag = &html[open_start..svg_open_end];
    let image_tag = &html[image_start..image_end];
    let svg_attrs = parse_attrs(svg_tag);
    let image_attrs = parse_attrs(image_tag);

    let href = image_attrs
        .get("xlink:href")
        .or_else(|| image_attrs.get("href"))?;
    let href = href.trim();
    if href.is_empty() || is_external(href) {
        return None;
    }

    let image_key = resolve_href(doc_dir, &value_without_fragment(href));
    // The block is an image with a viewBox, not a drawing: point an <img>
    // straight at it instead of duplicating its bytes base64 inside a
    // generated SVG wrapper. That keeps the resource cache free for the real
    // resources and lets the image be decoded like any other <img>.
    provider.get(&image_key)?;

    Some(svg_image_tag(&image_key, &svg_attrs, &image_attrs))
}

/// True when the markup in `from..to` only holds invisible image-map
/// hotspots: `<a>` wrappers and `<rect>`s with no fill.
///
/// The SVG painter this replaces draws such a page's shapes but not its
/// `<image>`, so an image map renders blank; the caller turns the block into an
/// `<img>` instead (the hotspots are invisible, so only their links are lost).
fn svg_overlays_are_invisible(html: &str, from: usize, to: usize) -> bool {
    let mut index = from;
    while index < to {
        let Some(open) = html[index..to].find('<').map(|offset| index + offset) else {
            return html[index..to].trim().is_empty();
        };
        if html[open..].starts_with("<!--") {
            index = html[open..to]
                .find("-->")
                .map(|offset| open + offset + 3)
                .unwrap_or(to);
            continue;
        }
        let Some(end) = find_tag_end(html, open) else {
            return false;
        };
        let raw = &html[open..end];
        let name = tag_name(raw);
        match name.as_str() {
            "a" | "g" | "title" | "desc" | "defs" | "metadata" => {}
            "rect" | "circle" | "ellipse" | "polygon" | "polyline" | "path" => {
                let attrs = parse_attrs(raw);
                let invisible = attrs
                    .get("fill-opacity")
                    .is_some_and(|value| value.trim() == "0" || value.trim() == "0.0")
                    || attrs
                        .get("opacity")
                        .is_some_and(|value| value.trim() == "0" || value.trim() == "0.0")
                    || attrs
                        .get("fill")
                        .is_some_and(|value| value.trim().eq_ignore_ascii_case("none"));
                if !invisible {
                    return false;
                }
            }
            _ => return false,
        }
        index = end;
    }
    true
}

fn svg_image_tag(
    image_key: &str,
    svg_attrs: &HashMap<String, String>,
    image_attrs: &HashMap<String, String>,
) -> String {
    let src = format!("epub://book/{}", encode_path(image_key));
    let mut tag = format!("<img src=\"{}\"", escape_attr(&src));
    if let Some(id) = svg_attrs.get("id").or_else(|| image_attrs.get("id")) {
        tag.push_str(&format!(" id=\"{}\"", escape_attr(id)));
    }
    if let Some(class) = svg_attrs.get("class").or_else(|| image_attrs.get("class")) {
        tag.push_str(&format!(" class=\"{}\"", escape_attr(class)));
    }
    if let Some(alt) = image_attrs.get("alt").or_else(|| svg_attrs.get("alt")) {
        tag.push_str(&format!(" alt=\"{}\"", escape_attr(alt)));
    }
    tag.push_str(" style=\"max-width:100%;height:auto\">");
    tag
}

fn has_element_tag(text: &str, start: usize, end: usize) -> bool {
    let Some(segment) = text.get(start..end) else {
        return true;
    };
    let mut i = 0;
    while let Some(rel) = segment[i..].find('<') {
        let tag_pos = i + rel + 1;
        if tag_pos < segment.len() {
            let first = segment[tag_pos..].chars().next().unwrap();
            if first.is_ascii_alphabetic() {
                return true;
            }
            if first == '/' {
                let after = segment[tag_pos + 1..].chars().next();
                if after.is_some_and(|c| c.is_ascii_alphabetic()) {
                    return true;
                }
            }
        }
        i += rel + 1;
    }
    false
}

fn has_meaningful_text(text: &str) -> bool {
    let mut i = 0;
    while i < text.len() {
        match text[i..].find('<') {
            Some(rel) if text[i + rel..].starts_with("<!--") => {
                let comment_start = i + rel;
                if let Some(comment_end) = text[comment_start..].find("-->") {
                    i = comment_start + comment_end + 3;
                } else {
                    i = text.len();
                }
            }
            Some(_) => return true,
            None => return text[i..].chars().any(|c| !c.is_whitespace()),
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn rewrite_full_page_image_tags(
    html: &str,
    doc_dir: &str,
    provider: &LazyResource,
    generated: &mut HashMap<String, Arc<Vec<u8>>>,
    page_fit: &mut HashMap<String, PageFitImage>,
    generated_counter: &mut usize,
    available: (f32, f32),
    image_only: bool,
) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;

    while let Some(open_start) = next_tag_start(&lower, cursor, "<img") {
        let Some(close_end) = find_tag_end(html, open_start) else {
            out.push_str(&html[cursor..]);
            return out;
        };
        out.push_str(&html[cursor..open_start]);

        if let Some(replacement) = full_page_image_replacement(
            &html[open_start..close_end],
            doc_dir,
            provider,
            generated,
            page_fit,
            generated_counter,
            available,
            image_only,
        ) {
            out.push_str(&replacement);
        } else {
            out.push_str(&html[open_start..close_end]);
        }
        cursor = close_end;
    }

    out.push_str(&html[cursor..]);
    out
}

#[allow(clippy::too_many_arguments)]
fn full_page_image_replacement(
    tag: &str,
    doc_dir: &str,
    provider: &LazyResource,
    generated: &mut HashMap<String, Arc<Vec<u8>>>,
    page_fit: &mut HashMap<String, PageFitImage>,
    generated_counter: &mut usize,
    available: (f32, f32),
    image_only: bool,
) -> Option<String> {
    let (available_width, available_height) = available;
    let attrs = parse_attrs(tag);
    let full_height = image_only
        || attrs
            .get("style")
            .is_some_and(|style| style_has_full_height(style));
    let src = attrs.get("src").map(String::as_str).unwrap_or_default();
    let src = src.trim();
    if src.is_empty() || src.starts_with("data:") {
        return None;
    }

    // `normalize_inline_svg_images` rewrites an SVG-backed image to
    // `epub://book/<key>`. Treat our own scheme like a relative source;
    // rejecting every `://` here skipped the fit style for exactly those
    // full-page images, so a tall image got its own page after a blank one.
    let image_key = if let Some(relative) = src.strip_prefix("epub://book/") {
        let relative = percent_decode_str(relative).decode_utf8_lossy();
        normalize_path(&relative)
    } else {
        if src.contains("://") {
            return None;
        }
        resolve_href(doc_dir, src)
    };
    let image_bytes = provider.get(&image_key)?;
    let image_mime = image_mime(&image_bytes, &image_key);
    let is_svg = image_mime == "image/svg+xml" || image_key.ends_with(".svg");
    if !full_height {
        // A raster image only needs a fit style when its natural width would
        // overflow the content height. SVG dimensions are harder to read
        // reliably here, so cap every SVG image; max-width/max-height never
        // enlarge a small icon.
        if let Some((width, height)) = image_dimensions(&image_bytes) {
            let scaled_height = available_width * height as f32 / width.max(1) as f32;
            if scaled_height <= available_height + 0.5 && !is_svg {
                return None;
            }
        } else if !is_svg {
            return None;
        }
        return Some(image_fit_tag(&attrs, &image_key, available_height));
    }

    if image_mime == "image/svg+xml" {
        return None;
    }
    // The wrapper's size is the page's; the picture is contained in it when
    // its own size is known (the normal case) and fills it otherwise.
    let dimensions = image_dimensions(&image_bytes);
    let (width, height) = dimensions.unwrap_or((
        available_width.round().max(1.0) as u32,
        available_height.round().max(1.0) as u32,
    ));
    *generated_counter += 1;
    let generated_key = generated_full_page_image_key(*generated_counter);
    let svg = full_page_image_svg_document(available_width, available_height);
    generated.insert(generated_key.clone(), Arc::new(svg.into_bytes()));
    page_fit.insert(
        generated_key.clone(),
        PageFitImage {
            source: image_key.clone(),
            width,
            height,
            contain: dimensions.is_some(),
        },
    );

    Some(full_page_image_tag(&generated_key, &attrs))
}

/// True when the document's body holds exactly one image and no text: the
/// "cover page" case, which the reader scales to the screen.
fn document_is_single_image(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    if lower.matches("<img").count() != 1 {
        return false;
    }
    let body_start = lower
        .find("<body")
        .and_then(|at| lower[at..].find('>').map(|o| at + o + 1));
    let body_end = lower.rfind("</body").unwrap_or(lower.len());
    let body = match body_start {
        Some(start) if start <= body_end => &html[start..body_end],
        _ => html,
    };
    let mut text = String::with_capacity(body.len() / 4);
    let mut in_tag = false;
    for ch in body.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }
    text.trim().is_empty()
}

fn generated_full_page_image_key(counter: usize) -> String {
    format!("cre-page-fit-{counter}.svg")
}

/// A transparent SVG of the page's size standing in for an inlined picture.
///
/// The renderer paints the original image into this box with `contain`, the
/// same fit the old `preserveAspectRatio="xMidYMid meet"` wrapper used, so the
/// page looks identical without a base64 copy of the image in memory.
fn full_page_image_svg_document(width: f32, height: f32) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width:.0}\" height=\"{height:.0}\" viewBox=\"0 0 {width:.0} {height:.0}\"></svg>"
    )
}

/// A lone image that is taller than the page. Blitz scales an image with
/// `max-width`/`max-height` and automatic width/height, so a style is enough;
/// no generated SVG or base64 copy is needed.
fn image_fit_tag(
    image_attrs: &HashMap<String, String>,
    image_key: &str,
    available_height: f32,
) -> String {
    let src = format!("epub://book/{}", encode_path(image_key));
    let mut tag = format!("<img src=\"{}\"", escape_attr(&src));
    if let Some(id) = image_attrs.get("id") {
        tag.push_str(&format!(" id=\"{}\"", escape_attr(id)));
    }
    if let Some(class) = image_attrs.get("class") {
        tag.push_str(&format!(" class=\"{}\"", escape_attr(class)));
    }
    if let Some(alt) = image_attrs.get("alt") {
        tag.push_str(&format!(" alt=\"{}\"", escape_attr(alt)));
    }
    let mut style = image_attrs.get("style").cloned().unwrap_or_default();
    if !style.trim().is_empty() && !style.trim_end().ends_with(';') {
        style.push(';');
    }
    style.push_str(&format!(
        "max-width:100%;max-height:{:.0}px;width:auto;height:auto",
        available_height
    ));
    tag.push_str(&format!(" style=\"{}\">", escape_attr(&style)));
    tag
}

fn full_page_image_tag(generated_key: &str, image_attrs: &HashMap<String, String>) -> String {
    let src = format!("epub://book/{}", encode_path(generated_key));
    let mut tag = format!("<img src=\"{}\"", escape_attr(&src));
    if let Some(id) = image_attrs.get("id") {
        tag.push_str(&format!(" id=\"{}\"", escape_attr(id)));
    }
    if let Some(class) = image_attrs.get("class") {
        tag.push_str(&format!(" class=\"{}\"", escape_attr(class)));
    }
    if let Some(alt) = image_attrs.get("alt") {
        tag.push_str(&format!(" alt=\"{}\"", escape_attr(alt)));
    }
    tag.push_str(" style=\"max-width:100%;height:auto\">");
    tag
}

fn style_has_full_height(style: &str) -> bool {
    for declaration in style.split(';') {
        let Some((property, value)) = declaration.split_once(':') else {
            continue;
        };
        if !property.trim().eq_ignore_ascii_case("height") {
            continue;
        }
        let value = value.trim().trim_end_matches('!').trim();
        if value.eq_ignore_ascii_case("100%") {
            return true;
        }
    }
    false
}

fn parse_toc(
    ncx_bytes_hint: &[u8],
    spine_node: roxmltree::Node<'_, '_>,
    manifest: &[ManifestItem],
    opf_dir: &str,
    provider: &LazyResource,
) -> Vec<TocItem> {
    let _ = ncx_bytes_hint;
    let _ = opf_dir;
    let mut out = Vec::new();

    if let Some(toc_id) = spine_node.attribute("toc")
        && let Some(item) = manifest.iter().find(|item| item.id == toc_id)
        && let Some(bytes) = provider.get(&item.href)
    {
        parse_ncx(&bytes, parent_dir(&item.href), &mut out);
    }

    if out.is_empty() {
        let nav_item = manifest
            .iter()
            .find(|item| item.properties.split_whitespace().any(|prop| prop == "nav"));
        if let Some(item) = nav_item
            && let Some(bytes) = provider.get(&item.href)
        {
            parse_epub3_nav(&bytes, parent_dir(&item.href), &mut out);
        }
    }

    out
}

fn parse_ncx(bytes: &[u8], base_dir: &str, out: &mut Vec<TocItem>) {
    let text = decode_text(bytes);
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let doc = match roxmltree::Document::parse_with_options(&text, options) {
        Ok(doc) => doc,
        Err(_) => return,
    };
    let root = doc.root_element();
    if root.tag_name().name() != "ncx" {
        return;
    }
    let Some(nav_map) = root
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "navMap")
    else {
        return;
    };
    for nav_point in nav_map
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "navPoint")
    {
        parse_ncx_point(nav_point, 1, base_dir, out);
    }
}

fn parse_ncx_point(
    node: roxmltree::Node<'_, '_>,
    depth: usize,
    base_dir: &str,
    out: &mut Vec<TocItem>,
) {
    let title = child_text(node, "navLabel").unwrap_or_default();
    let href = node
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "content")
        .and_then(|n| n.attribute("src"))
        .unwrap_or_default();
    let href = resolve_toc_href(base_dir, href);
    if !href.is_empty() {
        out.push(TocItem { title, href, depth });
    }
    for child in node
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "navPoint")
    {
        parse_ncx_point(child, depth + 1, base_dir, out);
    }
}

fn parse_epub3_nav(bytes: &[u8], base_dir: &str, out: &mut Vec<TocItem>) {
    let text = decode_text(bytes);
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let Ok(doc) = roxmltree::Document::parse_with_options(&text, options) else {
        return;
    };
    let root = doc.root_element();
    let Some(nav) = root
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "nav" && has_epub_type(*n, "toc"))
    else {
        return;
    };
    let Some(ol) = nav
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "ol")
    else {
        return;
    };
    parse_nav_ol(ol, 1, base_dir, out);
}

fn parse_nav_ol(ol: roxmltree::Node<'_, '_>, depth: usize, base_dir: &str, out: &mut Vec<TocItem>) {
    for li in ol
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "li")
    {
        if let Some(anchor) = li
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == "a")
        {
            let title = descendant_text(anchor).unwrap_or_default();
            let href = resolve_toc_href(base_dir, anchor.attribute("href").unwrap_or_default());
            if !href.is_empty() {
                out.push(TocItem { title, href, depth });
            }
        }
        for child in li
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "ol")
        {
            parse_nav_ol(child, depth + 1, base_dir, out);
        }
    }
}

fn parse_page_map(
    spine_node: roxmltree::Node<'_, '_>,
    manifest: &[ManifestItem],
    opf_dir: &str,
    provider: &LazyResource,
) -> (String, Vec<PageMapItem>) {
    let _ = opf_dir;
    if let Some(item) = manifest.iter().find(|item| {
        item.properties
            .split_whitespace()
            .any(|property| property == "page-list")
    }) && let Some(bytes) = provider.get(&item.href)
    {
        let mut out = Vec::new();
        parse_epub3_page_list(&bytes, parent_dir(&item.href), &mut out);
        if !out.is_empty() {
            return ("page-list".to_string(), out);
        }
    }

    if let Some(toc_id) = spine_node.attribute("toc")
        && let Some(item) = manifest.iter().find(|item| item.id == toc_id)
        && let Some(bytes) = provider.get(&item.href)
    {
        let mut out = Vec::new();
        parse_ncx_page_map(&bytes, parent_dir(&item.href), &mut out);
        if !out.is_empty() {
            return ("ncx".to_string(), out);
        }
    }

    for item in manifest
        .iter()
        .filter(|item| item.media_type == "application/x-dtbncx+xml")
    {
        if let Some(bytes) = provider.get(&item.href) {
            let mut out = Vec::new();
            parse_ncx_page_map(&bytes, parent_dir(&item.href), &mut out);
            if !out.is_empty() {
                return ("ncx".to_string(), out);
            }
        }
    }

    ("page-list".to_string(), Vec::new())
}

fn parse_epub3_page_list(bytes: &[u8], base_dir: &str, out: &mut Vec<PageMapItem>) {
    let text = decode_text(bytes);
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let Ok(doc) = roxmltree::Document::parse_with_options(&text, options) else {
        return;
    };
    let Some(nav) = doc
        .root_element()
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "nav" && has_epub_type(*n, "page-list"))
    else {
        return;
    };
    let Some(ol) = nav
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "ol")
    else {
        return;
    };
    parse_page_list_ol(ol, base_dir, out);
}

fn parse_page_list_ol(ol: roxmltree::Node<'_, '_>, base_dir: &str, out: &mut Vec<PageMapItem>) {
    for li in ol
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "li")
    {
        if let Some(anchor) = li
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == "a")
        {
            let label = descendant_text(anchor).unwrap_or_default();
            let href = resolve_toc_href(base_dir, anchor.attribute("href").unwrap_or_default());
            if !label.is_empty() && !href.is_empty() {
                out.push(PageMapItem { label, href });
            }
        }
        for child in li
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "ol")
        {
            parse_page_list_ol(child, base_dir, out);
        }
    }
}

fn parse_ncx_page_map(bytes: &[u8], base_dir: &str, out: &mut Vec<PageMapItem>) {
    let text = decode_text(bytes);
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let Ok(doc) = roxmltree::Document::parse_with_options(&text, options) else {
        return;
    };
    let Some(page_map) = doc
        .root_element()
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "pageMap")
    else {
        return;
    };
    parse_ncx_page_map_node(page_map, base_dir, out);
}

fn parse_ncx_page_map_node(
    node: roxmltree::Node<'_, '_>,
    base_dir: &str,
    out: &mut Vec<PageMapItem>,
) {
    for target in node
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "pageTarget")
    {
        let label = child_text(target, "navLabel")
            .or_else(|| target.attribute("playLevel").map(|value| value.to_string()))
            .unwrap_or_default();
        let href = target
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == "content")
            .and_then(|n| n.attribute("src"))
            .unwrap_or_default();
        let href = resolve_toc_href(base_dir, href);
        if !label.is_empty() && !href.is_empty() {
            out.push(PageMapItem { label, href });
        }
        parse_ncx_page_map_node(target, base_dir, out);
    }
}

fn child_text(node: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == name)
        .and_then(descendant_text)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn descendant_text(node: roxmltree::Node<'_, '_>) -> Option<String> {
    let text: String = node
        .descendants()
        .filter(|n| n.is_text())
        .filter_map(|n| n.text())
        .collect();
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn has_epub_type(node: roxmltree::Node<'_, '_>, value: &str) -> bool {
    node.attributes().any(|attr| {
        let matches = match attr.namespace() {
            Some(uri) => {
                uri == "http://www.idpf.org/2007/ops"
                    && (attr.name() == "type" || attr.name().ends_with(":type"))
            }
            None => attr.name() == "epub:type",
        };
        matches && attr.value().split_ascii_whitespace().any(|v| v == value)
    })
}

fn split_fragment(href: &str) -> (&str, Option<&str>) {
    match href.find('#') {
        Some(index) => (&href[..index], Some(&href[index + 1..])),
        None => (href, None),
    }
}

fn resolve_toc_href(base_dir: &str, href: &str) -> String {
    let href = href.trim();
    let (path, fragment) = split_fragment(href);
    if path.is_empty() {
        return fragment.map_or_else(String::new, |fragment| format!("#{fragment}"));
    }
    let resolved = resolve_href(base_dir, path);
    match fragment {
        Some(fragment) => format!("{resolved}#{fragment}"),
        None => resolved,
    }
}

fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

fn decode_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attr(value: &str) -> String {
    value.replace('&', "&amp;").replace('"', "&quot;")
}

/// Rewrites a stylesheet so every rule only applies inside `scope`.
///
/// Each spine document is a `<section data-spine-id="...">` in the chunk, so a
/// document's own `<style>` block has to be prefixed with that section; `html`
/// and `body` refer to the section itself, which is what the browser would style
/// when it renders the document on its own.
fn scope_stylesheet(css: &str, scope: &str) -> String {
    let mut out = String::with_capacity(css.len() + css.len() / 4);
    let mut rest = css;
    while let Some(open) = rest.find('{') {
        // An at-rule without a selector (`@font-face`, `@page`, `@charset`)
        // must stay as it is, and `@media`/`@supports` wrap rules that are
        // scoped individually as the scan reaches them.
        let selector = &rest[..open];
        if selector.trim_start().starts_with('@') {
            out.push_str(selector);
        } else {
            out.push_str(&scope_selector_list(selector, scope));
        }
        out.push('{');
        let Some(close) = rest[open..].find('}') else {
            out.push_str(&rest[open + 1..]);
            return out;
        };
        out.push_str(&rest[open + 1..open + close]);
        out.push('}');
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out
}

fn scope_selector_list(selectors: &str, scope: &str) -> String {
    selectors
        .split(',')
        .map(|part| {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                return String::new();
            }
            let lower = trimmed.to_ascii_lowercase();
            for root in ["html", "body", ":root"] {
                if lower == root {
                    // A rule on the document root itself stays global: our base
                    // typography sets the reader's font size (and margins) on the
                    // chunk's body, and scoping `body { font-size: 75% }` onto a
                    // section would multiply the two instead of overriding.
                    return trimmed.to_string();
                }
                if lower.starts_with(root)
                    && matches!(
                        lower.as_bytes().get(root.len()),
                        Some(b' ') | Some(b'>') | Some(b'+') | Some(b'~')
                    )
                {
                    // `body p` becomes `<section> p`.
                    return format!("{scope}{}", &trimmed[root.len()..]);
                }
                if lower.starts_with(root)
                    && matches!(
                        lower.as_bytes().get(root.len()),
                        Some(b'.') | Some(b'#') | Some(b'[') | Some(b':')
                    )
                {
                    // `body.cov2` matches the section, which carries the
                    // body's own class/style attributes.
                    return format!("{scope}{}", &trimmed[root.len()..]);
                }
            }
            format!("{scope} {trimmed}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_font_face_reports_its_family_and_urls() {
        let css = "\
@font-face { src: url(fonts/00001.ttf); font-family: \"One\"; }
@font-face { font-family: Two; src: local(\"Songti\"), url( 'two.otf' ); }
@font-face { font-family: Local Only; src: local(\"STSong\"); }";
        let declarations = super::font_face_declarations(css);
        assert_eq!(declarations.len(), 3, "{declarations:?}");
        assert_eq!(declarations[0].family, "One");
        assert_eq!(declarations[0].urls, ["fonts/00001.ttf"]);
        assert_eq!(declarations[1].urls, ["two.otf"]);
        assert!(declarations[2].urls.is_empty(), "{declarations:?}");
    }

    #[test]
    fn only_faces_with_a_loadable_source_are_reported() {
        // The shape a stripped AZW3 leaves behind: the rules survive, the font
        // records do not, so the URL is a placeholder that resolves to nothing.
        let css = "\
@font-face { font-family: \"zw\"; src: local(\"Songti\"), url(XXXXXXXXXXXXXXXX); }
@font-face { font-family: \"kt\"; src: url(mobi-image-5); }
@font-face { font-family: \"local-only\"; src: local(\"STSong\"); }";
        let families = super::loadable_font_faces(css, |url| url == "mobi-image-5");
        assert_eq!(families, ["kt"], "{families:?}");
        // With nothing available, nothing is offered.
        assert!(super::loadable_font_faces(css, |_| false).is_empty());
    }

    #[test]
    fn font_faces_are_collected_once_per_family() {
        let css = "\
@font-face { src: url(fonts/00001.ttf); font-family: \"FZBeiWeiKaiShu-S19_GB18030\"; }
@font-face { src: local(\"STSong\"); font-family: 'Song'; }
@font-face { src: url(fonts/00002.ttf); font-family: STXingkai; }
@font-face { src: url(fonts/00003.ttf); font-family: \"Song\"; }";
        assert_eq!(
            super::font_faces_in(css),
            vec![
                "FZBeiWeiKaiShu-S19_GB18030".to_string(),
                "Song".to_string(),
                "STXingkai".to_string(),
            ]
        );
        // HTML without any rule is the common case and must stay free of work.
        assert!(super::font_faces_in("<p>plain</p>").is_empty());
    }

    #[test]
    fn stripping_font_faces_leaves_the_rest_of_the_stylesheet() {
        let css = "\
@page { margin: 5pt; }
@font-face { src: url(fonts/00001.ttf); font-family: One; }
p { font-family: One, serif; }";
        let stripped = super::strip_font_faces(css);
        assert!(!stripped.contains("@font-face"), "{stripped}");
        assert!(stripped.contains("@page { margin: 5pt; }"), "{stripped}");
        assert!(stripped.contains("font-family: One, serif"), "{stripped}");
        // The `font-family` declaration is kept on purpose: a family that no
        // longer resolves falls back to the next one in the list.
        assert_eq!(stripped.matches("font-family").count(), 1, "{stripped}");
        // No rule, no copy.
        assert!(matches!(
            super::strip_font_faces("<p>plain</p>"),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(
            super::strip_font_faces(css).as_ref(),
            stripped.as_ref(),
            "stripping must be idempotent"
        );
    }

    #[test]
    fn sections_are_packed_whole_into_chunks() {
        let html = "<html><head><title>t</title></head><body>\
                    <section class=\"cre-spine\" data-spine-index=\"1\"><p>one</p></section>\
                    <section class=\"cre-spine\" data-spine-index=\"2\"><p>two</p></section>\
                    <section class=\"cre-spine\" data-spine-index=\"3\"><p>three</p></section>\
                    </body></html>";
        // Room for two sections per chunk.
        let limit = 150;
        let parts = super::split_html_document_at_sections(html, limit).expect("sections");
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].contains("data-spine-index=\"1\""), "{}", parts[0]);
        assert!(parts[0].contains("data-spine-index=\"2\""), "{}", parts[0]);
        assert!(!parts[0].contains("data-spine-index=\"3\""), "{}", parts[0]);
        assert!(parts[1].contains("data-spine-index=\"3\""), "{}", parts[1]);
        // The head and the closing tags are repeated in every part.
        for part in &parts {
            assert!(part.starts_with("<html><head>"), "{part}");
            assert!(part.ends_with("</body></html>"), "{part}");
        }
    }

    #[test]
    fn a_chunk_stays_within_the_image_budget() {
        let html = "<html><body>\
                    <section class=\"cre-spine\"><p>one</p></section>\
                    <section class=\"cre-spine\"><p>two</p></section>\
                    <section class=\"cre-spine\"><p>three</p></section>\
                    </body></html>";
        // Each section claims 10 bytes of decoded images; the budget fits one.
        let parts = super::split_html_document_at_sections_with(
            html,
            1000,
            12,
            1_000_000,
            128,
            |section| (section.len(), 10),
        )
        .expect("sections");
        assert_eq!(parts.len(), 3, "{parts:?}");
    }

    #[test]
    fn a_leading_stylesheet_is_repeated_in_every_chunk() {
        // KF8 bodies start with the book's CSS before the first part.
        let html = "<html><body><style>p{color:red}</style>\
                    <section class=\"cre-spine\"><p>one</p></section>\
                    <section class=\"cre-spine\"><p>two</p></section>\
                    </body></html>";
        let parts = super::split_html_document_at_sections(html, 110).expect("sections");
        assert_eq!(parts.len(), 2, "{parts:?}");
        for part in &parts {
            assert!(part.contains("<style>p{color:red}</style>"), "{part}");
        }
    }

    #[test]
    fn streaming_sections_match_the_collected_split() {
        // The streaming variant is what the MOBI/KF8/FB2 open path uses; it
        // must produce exactly the chunks the collecting splitter would.
        let cases = [
            "<html><head><title>t</title></head><body>\
             <section class=\"cre-spine\" data-spine-index=\"1\"><p>one</p></section>\
             <section class=\"cre-spine\" data-spine-index=\"2\"><p>two</p></section>\
             <section class=\"cre-spine\" data-spine-index=\"3\"><p>three</p></section>\
             </body></html>",
            "<html><body><style>p{color:red}</style>\
             <section class=\"cre-spine\"><p>one</p></section>\
             <section class=\"cre-spine\"><p>two</p></section>\
             </body></html>",
        ];
        for html in cases {
            let (head, body, tail) = super::html_body_parts(html).expect("body parts");
            for limit in [110, 150, 1000] {
                let collected =
                    super::split_html_document_at_sections(html, limit).expect("sections");
                let mut streamed = Vec::new();
                assert!(super::stream_html_document_at_sections_with(
                    head,
                    body,
                    tail,
                    limit,
                    usize::MAX,
                    usize::MAX,
                    128,
                    |section| (section.len(), 0),
                    |chunk| streamed.push(chunk),
                ));
                assert_eq!(collected, streamed, "limit {limit}");
            }
        }
    }

    #[test]
    fn a_body_without_sections_or_with_text_between_them_is_generic() {
        let plain = "<html><body><p>a</p><p>b</p></body></html>";
        assert!(super::split_html_document_at_sections(plain, 10).is_none());
        let mixed = "<html><body><section class=\"cre-spine\"><p>a</p></section>tail</body></html>";
        assert!(super::split_html_document_at_sections(mixed, 10).is_none());
        // One section larger than the budget: let the generic splitter cut it.
        let large =
            "<html><body><section class=\"cre-spine\"><p>aaaaaaaaaa</p></section></body></html>";
        assert!(super::split_html_document_at_sections(large, 20).is_none());
    }

    #[test]
    fn inline_svg_images_resolve_any_resource_name() {
        // KF8 names its images `mobi-image-N`; the EPUB path passes
        // `epub://book/…` hrefs. Both have to reach the fetch closure, or
        // usvg drops the picture.
        let mut resources = std::collections::HashMap::new();
        resources.insert("mobi-image-2", b"\x89PNG\r\n\x1a\nrest".to_vec());
        resources.insert("images/one.jpg", b"\xff\xd8\xffrest".to_vec());
        let fetch = |key: &str| {
            resources
                .get(key)
                .map(|data| std::sync::Arc::new(data.clone()))
        };

        let html = r#"<svg viewBox="0 0 10 10"><image xlink:href="mobi-image-2" width="10" height="10"/></svg>"#;
        let out = inline_svg_images_to_data_uris(html, &fetch).expect("a KF8 image should inline");
        assert!(out.contains("data:image/png;base64,"), "{out}");

        let html = r#"<svg viewBox="0 0 10 10"><image href="epub://book/images/one.jpg"/></svg>"#;
        let out =
            inline_svg_images_to_data_uris(html, &fetch).expect("an EPUB image should inline");
        assert!(out.contains("data:image/jpeg;base64,"), "{out}");
    }

    #[test]
    fn zhuyin_ruby_squeezes_the_note_onto_the_character() {
        let cell = ruby_cell("想<rt>ㄒㄧㄤˇ</rt>").unwrap();
        // Tone mark on its own row, flush right over the character.
        assert!(cell.contains("right:0;top:0"), "{cell}");
        assert!(cell.contains(">ˇ</span>"), "{cell}");
        // The three letters are centred over the base and squeezed to fit it.
        assert!(cell.contains("translateX(-50%)"), "{cell}");
        assert!(cell.contains("scaleX("), "{cell}");
        assert!(cell.contains("padding-top:0.82em"), "{cell}");
        // The base still sits in flow, so the cell keeps the character's width.
        assert!(cell.contains(">想</span>"), "{cell}");
    }

    #[test]
    fn kana_ruby_is_squeezed_onto_its_characters() {
        // No tone mark: one note row, the note is still fitted to the base.
        let cell = ruby_cell("東京<rt>とうきょう</rt>").unwrap();
        assert!(cell.contains("scaleX("), "{cell}");
        assert!(cell.contains("padding-top:0.5em"), "{cell}");
        assert!(!cell.contains("right:0;top:0"), "{cell}");
        // The base comes first so the cell reports its baseline.
        let base_at = cell.find(">東京<").unwrap();
        let note_at = cell.find("position:absolute").unwrap();
        assert!(base_at < note_at, "{cell}");
    }

    #[test]
    fn image_budget_splits_chunks() {
        // juliet.epub has a page-fit cover image; with a tiny image budget it
        // must be kept apart from the rest of the text.
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/koreader-test-data/juliet.epub");
        if !path.exists() {
            eprintln!("skipping: fixture missing");
            return;
        }
        let book = EpubBook::open(&path).unwrap();
        let options = HtmlOptions::default();
        let free = book
            .to_html_chunks_budgeted(&options, 8 * 1024 * 1024, None, usize::MAX)
            .unwrap();
        let capped = book
            .to_html_chunks_budgeted(&options, 8 * 1024 * 1024, Some(1024 * 1024), usize::MAX)
            .unwrap();
        assert!(
            capped.len() > free.len(),
            "image budget did not split: {} vs {} chunks",
            capped.len(),
            free.len()
        );
    }

    #[test]
    fn resource_cache_is_byte_bounded() {
        let mut cache = ResourceCache::new(1000);
        for index in 0..10 {
            cache.insert(format!("k{index}"), Arc::new(vec![index as u8; 300]));
        }
        let (entries, bytes) = (cache.entries.len(), cache.bytes);
        assert!(bytes <= 1000, "cache holds {bytes} bytes");
        assert!(entries <= 4, "cache kept {entries} entries");
        // The most recently inserted entry survives, the oldest is evicted.
        assert!(cache.get("k9").is_some());
        assert!(cache.get("k0").is_none());
    }

    #[test]
    fn generated_resources_survive_cache_pressure() {
        let provider = LazyResource {
            epub_path: None,
            cache: Arc::new(Mutex::new(ResourceCache::new(64))),
            pinned: Arc::new(Mutex::new(HashMap::new())),
            zip_archive: Arc::new(Mutex::new(None)),
        };
        provider.put("cre-page-fit-1.svg", Arc::new(vec![7u8; 128]));
        for index in 0..10 {
            provider
                .cache
                .lock()
                .unwrap()
                .insert(format!("img{index}"), Arc::new(vec![index as u8; 32]));
        }
        assert_eq!(
            provider.get("cre-page-fit-1.svg").map(|value| value.len()),
            Some(128),
            "a generated resource must not be evicted by zip entries"
        );
    }

    #[test]
    fn split_body_html_keeps_content_and_balances_tags() {
        let body = format!("<div class=\"wrap\">{}</div>", "<p>alpha</p>".repeat(50));
        let parts = split_body_html(&body, 200);
        assert!(
            parts.len() > 1,
            "expected a split, got {} part(s)",
            parts.len()
        );
        for part in &parts {
            assert_eq!(
                part.matches("<div").count(),
                part.matches("</div>").count(),
                "unbalanced div in {part:?}"
            );
            assert_eq!(
                part.matches("<p>").count(),
                part.matches("</p>").count(),
                "unbalanced p in {part:?}"
            );
        }
        let joined = parts.concat();
        assert_eq!(joined.matches("alpha").count(), 50);
    }

    #[test]
    fn split_body_html_does_not_cut_inside_raw_text_elements() {
        let body = format!(
            "<p>start</p><style>p::before {{ content: '<b>not markup</b>'; }}</style>{}",
            "<p>tail</p>".repeat(20)
        );
        let parts = split_body_html(&body, 120);
        assert!(parts.len() > 1);
        let styles = parts
            .iter()
            .filter(|part| part.contains("<style>"))
            .collect::<Vec<_>>();
        assert_eq!(styles.len(), 1, "style element must not be split");
        assert!(styles[0].contains("'<b>not markup</b>'"));
    }

    #[test]
    fn split_body_html_cuts_cjk_text_on_char_boundaries() {
        let body = format!(
            "<div>{}</div>",
            "这是一个用来测试分页切分的很长的中文句子。".repeat(200)
        );
        // A limit that is very unlikely to fall on a character boundary.
        let parts = split_body_html(&body, 1001);
        assert!(parts.len() > 1);
        let joined = parts.concat();
        assert_eq!(
            joined.matches('中').count(),
            body.matches('中').count(),
            "CJK text must survive splitting byte-exactly"
        );
        for part in &parts {
            assert_eq!(
                part.matches("<div").count(),
                part.matches("</div>").count(),
                "unbalanced div in part"
            );
        }
    }

    #[test]
    fn split_body_html_keeps_small_documents_whole() {
        let body = "<p>only a line</p>";
        assert_eq!(split_body_html(body, 4096), vec![body.to_string()]);
    }

    use super::*;

    fn juliet() -> EpubBook {
        EpubBook::open("../../fixtures/koreader-test-data/juliet.epub").unwrap()
    }

    #[test]
    fn parses_juliet_metadata_and_spine() {
        let book = juliet();
        assert_eq!(book.title.as_deref(), Some("Romeo and Juliet"));
        assert!(book.spine.len() > 20);
        assert_eq!(book.spine[0].href, "OPS/cover.xml");
        let docs = book.spine_documents().unwrap();
        assert!(docs.iter().any(|doc| doc.body_html.contains("SAMPSON")));
    }

    #[test]
    fn parses_juliet_ncx_toc() {
        let book = juliet();
        assert!(book.toc.len() > 20);
        assert_eq!(book.toc[0].title, "Title");
        assert_eq!(book.toc[0].href, "OPS/title.xml");
        assert_eq!(book.toc[0].depth, 1);
        assert!(
            book.toc
                .iter()
                .any(|item| item.depth > 1 && item.href.contains("#section_"))
        );
    }

    #[test]
    fn builds_combined_html_with_rewritten_resources() {
        let book = juliet();
        let html = book.to_html(&HtmlOptions::default()).unwrap();
        assert!(html.contains("epub://book/OPS/css/main.css"));
        assert!(html.contains("epub://book/OPS/images/logo-feedbooks-tiny.png"));
        assert!(html.contains("data-spine-index=\"1\""));
        assert!(html.contains("data-spine-href=\"OPS/cover.xml\""));
        assert!(html.contains("Romeo and Juliet"));
    }

    #[test]
    fn parses_epub3_nav_toc_from_synthetic_epub() {
        let book = EpubBook::from_bytes(synthetic_epub3_nav()).unwrap();
        assert_eq!(book.toc.len(), 2);
        assert_eq!(book.toc[0].title, "Chapter One");
        assert_eq!(book.toc[0].href, "sec1.xhtml#sec1");
        assert_eq!(book.toc[0].depth, 1);
        assert_eq!(book.toc[1].title, "Nested");
        assert_eq!(book.toc[1].href, "sec1.xhtml#sub");
        assert_eq!(book.toc[1].depth, 2);
    }

    #[test]
    fn parses_epub3_page_list_from_synthetic_epub() {
        let book = EpubBook::from_bytes(synthetic_epub3_page_list()).unwrap();
        assert_eq!(book.page_map_source, "page-list");
        assert_eq!(book.page_map.len(), 2);
        assert_eq!(book.page_map[0].label, "page 1");
        assert_eq!(book.page_map[0].href, "sec1.xhtml#p1");
        assert_eq!(book.page_map[1].label, "page 2");
        assert_eq!(book.page_map[1].href, "sec1.xhtml#p2");
    }

    #[test]
    fn parses_ncx_page_map_from_synthetic_epub() {
        let book = EpubBook::from_bytes(synthetic_ncx_page_map_epub()).unwrap();
        assert_eq!(book.page_map_source, "ncx");
        assert_eq!(book.page_map.len(), 2);
        assert_eq!(book.page_map[0].label, "Page 1");
        assert_eq!(book.page_map[0].href, "sec1.xhtml#p1");
        assert_eq!(book.page_map[1].label, "Page 2");
        assert_eq!(book.page_map[1].href, "sec1.xhtml#p2");
    }

    #[test]
    fn strips_cdata_and_comment_wrappers_from_inline_styles() {
        assert_eq!(
            clean_css("<![CDATA[\n   p { color: red; }\n]]>"),
            "p { color: red; }"
        );
        assert_eq!(clean_css("<!-- p { color: red; } -->"), "p { color: red; }");
        assert_eq!(clean_css("  \n p { a: b } \n "), "p { a: b }");
        assert_eq!(clean_css("<![CDATA[<!-- p { a: b } -->]]>"), "p { a: b }");
        assert_eq!(clean_css(""), "");
    }

    #[test]
    fn combined_html_has_cdata_free_inline_styles() {
        let book = EpubBook::from_bytes(synthetic_cdata_styled_epub()).unwrap();
        let html = book.to_html(&HtmlOptions::default()).unwrap();
        // The style is scoped to its spine document (a chunk holds several
        // documents, and their styles must not leak into each other), so the
        // selector is prefixed; the declaration itself is unchanged.
        assert!(html.contains("p.cdata-marker"), "{html}");
        assert!(html.contains("color: #ff0000"), "{html}");
        assert!(!html.contains("<![CDATA["));
        assert!(!html.contains("]]>"));
    }

    #[test]
    fn image_only_svg_blocks_become_plain_images() {
        let mut book = EpubBook::from_bytes(synthetic_inline_svg_epub()).unwrap();
        assert_eq!(book.normalize_inline_svg_images(), 1);

        // An image-only SVG is an image with a viewBox, not a drawing: it must
        // be rewritten to an <img> that points at the image itself, with no
        // generated wrapper carrying a base64 copy.
        assert!(book.resource("cre-inline-svg-1.svg").is_none());

        let html = book.to_html(&HtmlOptions::default()).unwrap();
        assert!(html.contains("epub://book/images/one.jpg"));
        assert!(html.contains("max-width:100%;height:auto"));
        assert!(!html.contains("data:image/jpeg;base64,"));
    }

    #[test]
    fn ignores_complex_inline_svg_blocks() {
        let mut book = EpubBook::from_bytes(synthetic_complex_inline_svg_epub()).unwrap();
        assert_eq!(book.normalize_inline_svg_images(), 0);
        assert!(book.resource("cre-inline-svg-1.svg").is_none());
    }

    #[test]
    fn normalizes_full_page_height_images_to_page_fit_svg() {
        let mut book = EpubBook::from_bytes(synthetic_full_page_image_epub()).unwrap();
        assert_eq!(book.normalize_full_page_images(600.0, 800.0, 40.0, 32.0), 1);

        let generated_key = "cre-page-fit-1.svg";
        let generated_svg = String::from_utf8(book.resource(generated_key).unwrap()).unwrap();
        assert!(generated_svg.contains("viewBox=\"0 0 520 736\""));
        assert!(
            !generated_svg.contains("data:image"),
            "the wrapper must not inline the picture: {generated_svg}"
        );
        let fit = book
            .page_fit_images
            .get(generated_key)
            .expect("wrapper must map to the original image");
        assert!(fit.source.ends_with("images/one.jpg"), "{:?}", fit.source);
        // The fixture image has no readable header, so it falls back to
        // filling the page box; real pictures keep their aspect ratio.
        assert!(!fit.contain);
        assert_eq!((fit.width, fit.height), (520, 736));

        let html = book.to_html(&HtmlOptions::default()).unwrap();
        assert!(html.contains(&format!("epub://book/{generated_key}")));
        assert!(html.contains("style=\"max-width:100%;height:auto\""));
        assert!(!html.contains("height: 100%; max-width: 100%"));
    }

    #[test]
    fn leaves_inline_images_without_full_height_style_unchanged() {
        let mut book = EpubBook::from_bytes(synthetic_normal_image_epub()).unwrap();
        assert_eq!(book.normalize_full_page_images(600.0, 800.0, 40.0, 32.0), 0);
        assert!(book.resource("cre-page-fit-1.svg").is_none());
    }

    #[test]
    fn pre_paginated_images_are_not_rescaled_by_the_reflow_cap() {
        // A pre-paginated page already carries its own viewport and the
        // renderer scales the whole page into the reader. Applying the reflow
        // full-page-image fit here would scale the image a second time (the
        // `15.epub` cover rendered as a small centered picture).
        let container = r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="pkg.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:fixed-image-test</dc:identifier>
    <dc:title>Fixed Image Test</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="s1" href="sec1.xhtml" media-type="application/xhtml+xml"/>
    <item id="img" href="images/one.jpg" media-type="image/jpeg"/>
  </manifest>
  <spine>
    <itemref idref="s1" properties="rendition:layout-pre-paginated"/>
  </spine>
</package>"#;
        let doc = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head>
    <title>Fixed</title>
    <meta name="viewport" content="width=600, height=800"/>
  </head>
  <body>
    <img src="images/one.jpg" alt="Cover" style="height: 100%; max-width: 100%;" />
  </body>
</html>"#;
        let mut book = EpubBook::from_bytes(synthetic_epub_fixture(
            container,
            opf,
            doc,
            &jpg_fixture_bytes(),
        ))
        .unwrap();
        assert_eq!(book.normalize_full_page_images(600.0, 800.0, 40.0, 32.0), 0);
        assert!(book.resource("cre-page-fit-1.svg").is_none());
        let chunks = book
            .to_html_chunks(&HtmlOptions::default(), 1_000_000)
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].contains("data-cre-fixed-layout=\"600x800\""));
    }

    #[test]
    fn fixed_layout_spine_item_gets_its_own_viewport_marker() {
        let container = r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="pkg.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:fixed-layout-test</dc:identifier>
    <dc:title>Fixed Layout Test</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="s1" href="sec1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="s1" properties="rendition:layout-pre-paginated"/>
  </spine>
</package>"#;
        let doc = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head>
    <title>Fixed</title>
    <meta name="viewport" content="width=900, height=600"/>
  </head>
  <body><p>Fixed page.</p></body>
</html>"#;
        let book = EpubBook::from_bytes(synthetic_epub_fixture(
            container,
            opf,
            doc,
            &jpg_fixture_bytes(),
        ))
        .unwrap();
        let docs = book.spine_documents().unwrap();
        assert_eq!(docs[0].layout, SpineLayout::PrePaginated);
        assert_eq!(docs[0].viewport, Some(FixedViewport::Size(900.0, 600.0)));
        let chunks = book
            .to_html_chunks(&HtmlOptions::default(), 1_000_000)
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].contains("data-cre-fixed-layout=\"900x600\""));
    }

    #[test]
    fn viewport_meta_uses_the_first_declaration_and_ignores_units() {
        assert_eq!(
            parse_viewport_content("width=900, height=600, width=1000, height=1000"),
            Some(FixedViewport::Size(900.0, 600.0))
        );
        assert_eq!(
            parse_viewport_content("width=1000px, height=600mm"),
            Some(FixedViewport::Size(1000.0, 600.0))
        );
        assert_eq!(
            parse_viewport_content("width=900, height=600, something, foo"),
            Some(FixedViewport::Size(900.0, 600.0))
        );
        assert_eq!(
            parse_viewport_content("width=device-width, height=device-height"),
            Some(FixedViewport::Device)
        );
        assert_eq!(parse_viewport_content("user-scalable=no"), None);
    }

    #[test]
    fn only_the_first_viewport_meta_is_used() {
        let html = r#"<html><head>
            <meta name="viewport" content="width=1000, height=600"/>
            <meta name="viewport" content="width=500, height=300"/>
        </head><body></body></html>"#;
        assert_eq!(
            parse_viewport(html),
            Some(FixedViewport::Size(1000.0, 600.0))
        );
    }

    #[test]
    fn svg_viewbox_is_the_fixed_layout_viewport() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 600"></svg>"#;
        assert_eq!(
            parse_svg_viewport(svg),
            Some(FixedViewport::Size(1200.0, 600.0))
        );
    }

    #[test]
    fn first_spine_layout_override_wins() {
        assert_eq!(
            spine_layout(
                "rendition:layout-reflowable rendition:layout-pre-paginated",
                SpineLayout::PrePaginated
            ),
            SpineLayout::Reflowable
        );
        assert_eq!(
            spine_layout(
                "rendition:layout-pre-paginated rendition:layout-reflowable",
                SpineLayout::Reflowable
            ),
            SpineLayout::PrePaginated
        );
        assert_eq!(
            spine_layout("", SpineLayout::PrePaginated),
            SpineLayout::PrePaginated
        );
    }

    #[test]
    fn svg_spine_item_uses_its_viewbox_as_the_fixed_viewport() {
        let container = r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="pkg.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:svg-fxl-test</dc:identifier>
    <dc:title>SVG Fixed Layout Test</dc:title>
    <dc:language>en</dc:language>
    <meta property="rendition:layout">pre-paginated</meta>
  </metadata>
  <manifest>
    <item id="s1" href="page_001.xhtml" media-type="application/xhtml+xml"/>
    <item id="s2" href="page_002.svg" media-type="image/svg+xml"/>
  </manifest>
  <spine>
    <itemref idref="s1"/>
    <itemref idref="s2"/>
  </spine>
</package>"#;
        let page = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head>
    <title>Fixed</title>
    <meta name="viewport" content="width=900, height=600"/>
  </head>
  <body><p>Fixed page.</p></body>
</html>"#;
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 600"><rect width="1200" height="600" fill="red"/></svg>"#;
        let mut buf = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            let options = zip::write::SimpleFileOptions::default();
            write_zip_bytes(&mut writer, options, "mimetype", b"application/epub+zip");
            write_zip_bytes(
                &mut writer,
                options,
                "META-INF/container.xml",
                container.as_bytes(),
            );
            write_zip_bytes(&mut writer, options, "pkg.opf", opf.as_bytes());
            write_zip_bytes(&mut writer, options, "page_001.xhtml", page.as_bytes());
            write_zip_bytes(&mut writer, options, "page_002.svg", svg.as_bytes());
            writer.finish().unwrap();
        }
        let book = EpubBook::from_bytes(buf.into_inner()).unwrap();
        let docs = book.spine_documents().unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[1].layout, SpineLayout::PrePaginated);
        assert_eq!(docs[1].viewport, Some(FixedViewport::Size(1200.0, 600.0)));
        let chunks = book
            .to_html_chunks(&HtmlOptions::default(), 1_000_000)
            .unwrap();
        assert_eq!(chunks.len(), 2);
        assert!(chunks[1].contains("data-cre-fixed-layout=\"1200x600\""));
        assert!(chunks[1].contains("width:100%;height:auto"));
    }

    #[test]
    fn image_in_spine_does_not_abort_the_book() {
        let container = r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="pkg.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:image-spine-test</dc:identifier>
    <dc:title>Image Spine Test</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="s1" href="sec1.xhtml" media-type="application/xhtml+xml"/>
    <item id="img" href="images/one.jpg" media-type="image/jpeg"/>
  </manifest>
  <spine>
    <itemref idref="s1"/>
    <itemref idref="img"/>
  </spine>
</package>"#;
        let doc = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head><title>Sec1</title></head>
  <body><p>Text before the image page.</p></body>
</html>"#;
        let book = EpubBook::from_bytes(synthetic_epub_fixture(
            container,
            opf,
            doc,
            &jpg_fixture_bytes(),
        ))
        .unwrap();
        let docs = book.spine_documents().unwrap();
        assert_eq!(docs.len(), 2, "the image spine item must remain a page");
        assert!(
            docs[1].body_html.contains("cre-image-spine"),
            "image spine item was not converted to an image page: {}",
            docs[1].body_html
        );
        let html = book.to_html(&HtmlOptions::default()).unwrap();
        assert!(html.contains("epub://book/images/one.jpg"));
    }

    /// A tall image is not only used as a cover: a chapter can hold one inline.
    /// Such an image must be capped to the page too, or it overflows the page
    /// and paints over the following text. Its own style (borders, margins)
    /// must survive when the fit properties are appended.
    #[test]
    fn caps_a_tall_inline_image_and_keeps_its_style() {
        let mut book = EpubBook::from_bytes(synthetic_tall_image_with_text_epub()).unwrap();
        assert_eq!(book.normalize_full_page_images(600.0, 800.0, 40.0, 32.0), 1);
        let html = book.to_html(&HtmlOptions::default()).unwrap();
        assert!(html.contains("epub://book/images/one.jpg"));
        assert!(html.contains("max-height:736px"));
        assert!(
            html.contains("border: 1px solid red;max-width:100%;max-height:736px"),
            "the original inline style was lost: {html}"
        );
        assert!(!html.contains("height: 100%"));
    }

    /// `normalize_inline_svg_images` turns a lone full-page SVG image into
    /// `<img src="epub://book/...">`. The full-page pass must accept that
    /// scheme too, or a tall cover stays unclipped, gets its own page, and
    /// leaves the preceding page blank (14.epub/15.epub at a 1080x1440 view).
    #[test]
    fn fits_a_full_page_image_with_an_epub_url_source() {
        let mut book = EpubBook::from_bytes(synthetic_epub_url_image_epub()).unwrap();
        assert_eq!(book.normalize_full_page_images(600.0, 800.0, 40.0, 32.0), 1);
        // An image-only page is scaled to the page through a generated SVG.
        assert!(book.resource("cre-page-fit-1.svg").is_some());
        let html = book.to_html(&HtmlOptions::default()).unwrap();
        assert!(html.contains("epub://book/cre-page-fit-1.svg"));
        assert!(!html.contains("height: 100%"));
    }

    #[test]
    fn parses_extended_metadata() {
        let book = EpubBook::from_bytes(synthetic_metadata_epub()).unwrap();
        assert_eq!(book.title.as_deref(), Some("Metadata Test"));
        assert_eq!(book.creator.as_deref(), Some("Test Author"));
        assert_eq!(book.language.as_deref(), Some("zh"));
        assert_eq!(book.series.as_deref(), Some("Test Series"));
        assert_eq!(book.description.as_deref(), Some("Description text"));
        assert_eq!(book.keywords.as_deref(), Some("alpha, beta"));
        assert_eq!(
            book.identifiers.as_deref(),
            Some("urn:uuid:metadata-test, urn:isbn:123")
        );
    }

    fn synthetic_full_page_image_epub() -> Vec<u8> {
        let container = r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="pkg.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:page-image-test</dc:identifier>
    <dc:title>Page Image Test</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="s1" href="sec1.xhtml" media-type="application/xhtml+xml"/>
    <item id="img" href="images/one.jpg" media-type="image/jpeg"/>
  </manifest>
  <spine>
    <itemref idref="s1"/>
  </spine>
</package>"#;
        let doc = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head><title>Sec1</title></head>
  <body>
    <div style="text-align: center; page-break-after: always;">
      <img src="images/one.jpg" alt="Cover" style="height: 100%; max-width: 100%;" />
    </div>
  </body>
</html>"#;
        synthetic_epub_fixture(container, opf, doc, &jpg_fixture_bytes())
    }

    fn synthetic_normal_image_epub() -> Vec<u8> {
        let container = r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="pkg.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:normal-image-test</dc:identifier>
    <dc:title>Normal Image Test</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="s1" href="sec1.xhtml" media-type="application/xhtml+xml"/>
    <item id="img" href="images/one.jpg" media-type="image/jpeg"/>
  </manifest>
  <spine>
    <itemref idref="s1"/>
  </spine>
</package>"#;
        let doc = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head><title>Sec1</title></head>
  <body>
    <p>An inline picture in a page of text.</p>
    <img src="images/one.jpg" alt="Inline" style="max-width: 100%; height: auto;" />
    <p>And the text continues after it.</p>
  </body>
</html>"#;
        synthetic_epub_fixture(container, opf, doc, &jpg_fixture_bytes())
    }

    fn synthetic_tall_image_with_text_epub() -> Vec<u8> {
        let container = r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="pkg.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:tall-inline-image-test</dc:identifier>
    <dc:title>Tall Inline Image Test</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="s1" href="sec1.xhtml" media-type="application/xhtml+xml"/>
    <item id="img" href="images/one.jpg" media-type="image/jpeg"/>
  </manifest>
  <spine>
    <itemref idref="s1"/>
  </spine>
</package>"#;
        let doc = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head><title>Sec1</title></head>
  <body>
    <p>Text before the picture.</p>
    <img src="images/one.jpg" alt="Tall" style="border: 1px solid red;" />
  </body>
</html>"#;
        synthetic_epub_fixture(container, opf, doc, &tall_png_bytes(100, 1000))
    }

    fn synthetic_epub_url_image_epub() -> Vec<u8> {
        let container = r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="pkg.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:epub-url-image-test</dc:identifier>
    <dc:title>EPUB URL Image Test</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="s1" href="sec1.xhtml" media-type="application/xhtml+xml"/>
    <item id="img" href="images/one.jpg" media-type="image/jpeg"/>
  </manifest>
  <spine>
    <itemref idref="s1"/>
  </spine>
</package>"#;
        let doc = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head><title>Cover</title></head>
  <body>
    <img src="epub://book/images/one.jpg" alt="Cover" style="max-width:100%;height:auto" />
  </body>
</html>"#;
        synthetic_epub_fixture(container, opf, doc, &tall_png_bytes(100, 1000))
    }

    /// PNG header with the given dimensions; enough for `image_dimensions`.
    fn tall_png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0, 0]);
        bytes
    }

    fn synthetic_metadata_epub() -> Vec<u8> {
        let container = r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="pkg.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">urn:uuid:metadata-test</dc:identifier>
    <dc:title>Metadata Test</dc:title>
    <dc:creator>Test Author</dc:creator>
    <dc:language>zh</dc:language>
    <dc:subject>alpha</dc:subject>
    <dc:subject>beta</dc:subject>
    <dc:identifier>urn:isbn:123</dc:identifier>
    <dc:description>Description text</dc:description>
    <meta name="calibre:series" content="Test Series"/>
  </metadata>
  <manifest>
    <item id="s1" href="sec1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="s1"/>
  </spine>
</package>"#;
        let doc = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head><title>Sec1</title></head>
  <body><p>metadata test</p></body>
</html>"#;
        synthetic_epub_fixture(container, opf, doc, b"")
    }

    fn synthetic_cdata_styled_epub() -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            let options = zip::write::SimpleFileOptions::default();

            write_zip_file(&mut writer, options, "mimetype", "application/epub+zip");
            write_zip_file(
                &mut writer,
                options,
                "META-INF/container.xml",
                r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="pkg.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#,
            );
            write_zip_file(
                &mut writer,
                options,
                "pkg.opf",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:cdata-test</dc:identifier>
    <dc:title>CDATA Test</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="s1" href="sec1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="s1"/>
  </spine>
</package>"#,
            );
            write_zip_file(
                &mut writer,
                options,
                "sec1.xhtml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head>
    <title>Sec1</title>
    <style type="text/css"><![CDATA[
   p.cdata-marker { color: #ff0000; }
]]></style>
  </head>
  <body>
    <p class="cdata-marker">styled paragraph</p>
  </body>
</html>"#,
            );

            writer.finish().unwrap();
        }
        buf.into_inner()
    }

    fn synthetic_epub3_nav() -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            let options = zip::write::SimpleFileOptions::default();

            write_zip_file(&mut writer, options, "mimetype", "application/epub+zip");
            write_zip_file(
                &mut writer,
                options,
                "META-INF/container.xml",
                r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="pkg.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#,
            );
            write_zip_file(
                &mut writer,
                options,
                "pkg.opf",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:test</dc:identifier>
    <dc:title>Nav Test</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="s1" href="sec1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="s1"/>
  </spine>
</package>"#,
            );
            write_zip_file(
                &mut writer,
                options,
                "nav.xhtml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
  <head><title>Nav</title></head>
  <body>
    <nav epub:type="toc">
      <ol>
        <li>
          <a href="sec1.xhtml#sec1">Chapter One</a>
          <ol>
            <li><a href="sec1.xhtml#sub">Nested</a></li>
          </ol>
        </li>
      </ol>
    </nav>
  </body>
</html>"#,
            );
            write_zip_file(
                &mut writer,
                options,
                "sec1.xhtml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head><title>Sec1</title></head>
  <body>
    <h1 id="sec1">Chapter One</h1>
    <h2 id="sub">Nested</h2>
  </body>
</html>"#,
            );

            writer.finish().unwrap();
        }
        buf.into_inner()
    }

    fn synthetic_epub3_page_list() -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            let options = zip::write::SimpleFileOptions::default();

            write_zip_file(&mut writer, options, "mimetype", "application/epub+zip");
            write_zip_file(
                &mut writer,
                options,
                "META-INF/container.xml",
                r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="pkg.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#,
            );
            write_zip_file(
                &mut writer,
                options,
                "pkg.opf",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:page-list-test</dc:identifier>
    <dc:title>Page List Test</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="pagelist" href="pagelist.xhtml" media-type="application/xhtml+xml" properties="page-list"/>
    <item id="s1" href="sec1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="s1"/>
  </spine>
</package>"#,
            );
            write_zip_file(
                &mut writer,
                options,
                "pagelist.xhtml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
  <head><title>Page List</title></head>
  <body>
    <nav epub:type="page-list">
      <ol>
        <li>
          <a href="sec1.xhtml#p1">page 1</a>
          <ol>
            <li><a href="sec1.xhtml#p2">page 2</a></li>
          </ol>
        </li>
      </ol>
    </nav>
  </body>
</html>"#,
            );
            write_zip_file(
                &mut writer,
                options,
                "sec1.xhtml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head><title>Sec1</title></head>
  <body>
    <h1 id="p1">Page 1</h1>
    <p id="p2">Page 2</p>
  </body>
</html>"#,
            );

            writer.finish().unwrap();
        }
        buf.into_inner()
    }

    fn synthetic_ncx_page_map_epub() -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            let options = zip::write::SimpleFileOptions::default();

            write_zip_file(&mut writer, options, "mimetype", "application/epub+zip");
            write_zip_file(
                &mut writer,
                options,
                "META-INF/container.xml",
                r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="pkg.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#,
            );
            write_zip_file(
                &mut writer,
                options,
                "pkg.opf",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:ncx-page-map-test</dc:identifier>
    <dc:title>NCX Page Map Test</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="s1" href="sec1.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="s1"/>
  </spine>
</package>"#,
            );
            write_zip_file(
                &mut writer,
                options,
                "toc.ncx",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <head>
    <meta name="dtb:uid" content="urn:uuid:ncx-page-map-test"/>
  </head>
  <docTitle><text>NCX Page Map Test</text></docTitle>
  <pageMap>
    <pageTarget playLevel="1">
      <navLabel><text>Page 1</text></navLabel>
      <content src="sec1.xhtml#p1"/>
    </pageTarget>
    <pageTarget playLevel="1">
      <navLabel><text>Page 2</text></navLabel>
      <content src="sec1.xhtml#p2"/>
    </pageTarget>
  </pageMap>
</ncx>"#,
            );
            write_zip_file(
                &mut writer,
                options,
                "sec1.xhtml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
  <head><title>Sec1</title></head>
  <body>
    <h1 id="p1">Page 1</h1>
    <p id="p2">Page 2</p>
  </body>
</html>"#,
            );

            writer.finish().unwrap();
        }
        buf.into_inner()
    }

    fn synthetic_inline_svg_epub() -> Vec<u8> {
        let container = r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="pkg.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:svg-test</dc:identifier>
    <dc:title>Inline SVG Test</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="s1" href="sec1.xhtml" media-type="application/xhtml+xml"/>
    <item id="img" href="images/one.jpg" media-type="image/jpeg"/>
  </manifest>
  <spine>
    <itemref idref="s1"/>
  </spine>
</package>"#;
        let doc = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:xlink="http://www.w3.org/1999/xlink">
  <head><title>Sec1</title></head>
  <body>
    <svg width="100%" height="100%" viewBox="0 0 100 200">
      <image width="100" height="200" xlink:href="images/one.jpg" />
    </svg>
  </body>
</html>"#;
        synthetic_epub_fixture(container, opf, doc, &jpg_fixture_bytes())
    }

    fn synthetic_complex_inline_svg_epub() -> Vec<u8> {
        let container = r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">
  <rootfiles>
    <rootfile full-path="pkg.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:svg-complex-test</dc:identifier>
    <dc:title>Complex Inline SVG Test</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="s1" href="sec1.xhtml" media-type="application/xhtml+xml"/>
    <item id="img" href="images/one.jpg" media-type="image/jpeg"/>
  </manifest>
  <spine>
    <itemref idref="s1"/>
  </spine>
</package>"#;
        let doc = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:xlink="http://www.w3.org/1999/xlink">
  <head><title>Sec1</title></head>
  <body>
    <svg width="100%" height="100%" viewBox="0 0 100 200">
      <rect width="100" height="200" fill="blue" />
      <image width="100" height="200" xlink:href="images/one.jpg" />
      <text x="10" y="50">caption</text>
    </svg>
  </body>
</html>"#;
        synthetic_epub_fixture(container, opf, doc, &jpg_fixture_bytes())
    }

    fn synthetic_epub_fixture(
        container: &str,
        opf: &str,
        doc: &str,
        image_bytes: &[u8],
    ) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            let options = zip::write::SimpleFileOptions::default();

            write_zip_bytes(&mut writer, options, "mimetype", b"application/epub+zip");
            write_zip_bytes(
                &mut writer,
                options,
                "META-INF/container.xml",
                container.as_bytes(),
            );
            write_zip_bytes(&mut writer, options, "pkg.opf", opf.as_bytes());
            write_zip_bytes(&mut writer, options, "sec1.xhtml", doc.as_bytes());
            write_zip_bytes(&mut writer, options, "images/one.jpg", image_bytes);

            writer.finish().unwrap();
        }
        buf.into_inner()
    }

    fn jpg_fixture_bytes() -> Vec<u8> {
        vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46]
    }

    fn write_zip_file(
        writer: &mut zip::ZipWriter<&mut Cursor<Vec<u8>>>,
        options: zip::write::SimpleFileOptions,
        name: &str,
        contents: &str,
    ) {
        write_zip_bytes(writer, options, name, contents.as_bytes());
    }

    fn write_zip_bytes(
        writer: &mut zip::ZipWriter<&mut Cursor<Vec<u8>>>,
        options: zip::write::SimpleFileOptions,
        name: &str,
        contents: &[u8],
    ) {
        use std::io::Write;
        writer.start_file(name, options).unwrap();
        writer.write_all(contents).unwrap();
    }
}

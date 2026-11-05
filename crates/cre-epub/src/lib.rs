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
/// The size is a *layout* setting, not part of the document: leaving it as a
/// token keeps a book's chunk HTML (and therefore its chunk boundaries) byte for
/// byte identical across font-size changes, so a font change can re-lay out the
/// chunks it already has instead of regenerating and re-chunking the book.
/// `cre_lib::create_document` substitutes it.
pub const FONT_SIZE_TOKEN: &str = "__CRE_FONT_SIZE_PX__";

#[derive(Clone, Debug)]
pub struct HtmlOptions {
    pub font_size: f32,
    pub line_height: f32,
    pub margin_x: f32,
    pub margin_y: f32,
}

impl Default for HtmlOptions {
    fn default() -> Self {
        Self {
            font_size: 18.0,
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
            .map(|size| estimated_decoded_image_bytes(&key, size))
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
}

impl EpubBook {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path_ref = path.as_ref();
        let data =
            std::fs::read(path_ref).with_context(|| format!("read epub {}", path_ref.display()))?;
        zip::ZipArchive::new(Cursor::new(data)).context("open epub zip")?;
        Self::from_provider(LazyResource::new(path_ref))
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

            let mut generated = HashMap::new();
            let normalized = rewrite_full_page_image_tags(
                &text,
                parent_dir(&doc_key),
                &self.resource_provider,
                &mut generated,
                &mut generated_counter,
                (available_width, available_height),
            );
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

    pub fn spine_documents(&self) -> Result<Vec<SpineDocument>> {
        let mut docs = Vec::new();
        for (index, item) in self.spine.iter().enumerate() {
            let nonlinear = !item.linear;
            let include = item.linear || item.media_type.to_ascii_lowercase().contains("html");
            if !include {
                continue;
            }
            // A package can list documents that are not in the archive at all
            // (the Readium epub30-test-230 case). Refusing the whole book hides
            // everything else in it, so skip the missing item; its slot keeps
            // its spine index, so the items after it still map correctly.
            let Some(bytes) = self.resource_provider.get(&item.href) else {
                eprintln!("cre-epub: spine document missing: {}", item.href);
                continue;
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
                continue;
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
                    FixedViewport::Device => {
                        ("device".to_string(), "width:100%;height:100%;".into())
                    }
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
            docs.push(SpineDocument {
                index,
                id: item.idref.clone(),
                href: item.href.clone(),
                media_type: item.media_type.clone(),
                body_html: rewrite_ruby(&rewrite_urls(
                    &body,
                    parent_dir(&item.href),
                    &self.opf_dir,
                )),
                css_urls,
                inline_styles,
                nonlinear,
                layout,
                viewport,
            });
        }
        if docs.is_empty() {
            bail!("no readable spine document");
        }
        Ok(docs)
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
        let docs = self.spine_documents()?;

        // One "piece" is a spine document, or one part of a document that is
        // larger than a chunk on its own.
        struct Piece<'a> {
            doc: &'a SpineDocument,
            body: std::borrow::Cow<'a, str>,
            slot: u32,
            continuation: bool,
        }
        let mut pieces: Vec<Piece<'_>> = Vec::new();
        let mut next_extra_slot = docs.len() as u32 + 1;
        for doc in &docs {
            let real_slot = doc.index as u32 + 1;
            if doc.body_html.len() <= max_bytes || doc.layout == SpineLayout::PrePaginated {
                pieces.push(Piece {
                    doc,
                    body: std::borrow::Cow::Borrowed(doc.body_html.as_str()),
                    slot: real_slot,
                    continuation: false,
                });
                continue;
            }
            for (part_index, part) in split_body_html(&doc.body_html, max_bytes)
                .into_iter()
                .enumerate()
            {
                let slot = if part_index == 0 {
                    real_slot
                } else {
                    let slot = next_extra_slot;
                    next_extra_slot += 1;
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

        // Estimated decoded bytes of each piece's images, so a chunk can be
        // capped by image payload as well as by text size.
        let piece_image_bytes = |piece: &Piece<'_>| -> usize {
            let dir = parent_dir(&piece.doc.href);
            image_sources(&piece.body, dir)
                .iter()
                .filter_map(|key| self.resource_provider.estimated_decoded_bytes(key))
                .sum()
        };
        let mut piece_images: Vec<usize> = pieces.iter().map(piece_image_bytes).collect();

        // The caps above only stop documents from being merged. One document
        // that is heavy on its own (a gallery page, a comic with several
        // images in one XHTML) would still be decoded in one go, so split it
        // between its images, which is where a page break already falls.
        if layout_bytes != usize::MAX {
            let mut expanded: Vec<Piece<'_>> = Vec::new();
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
                                let slot = next_extra_slot;
                                next_extra_slot += 1;
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
                        expanded_images[index] = piece_image_bytes(piece);
                    }
                }
            }
            pieces = expanded;
            piece_images = expanded_images;
        }

        let mut chunks = Vec::new();
        let mut body = String::new();
        let mut body_images = 0usize;
        let mut chunk_docs: Vec<&SpineDocument> = Vec::new();
        let flush =
            |body: &mut String, chunks: &mut Vec<String>, chunk_docs: &mut Vec<&SpineDocument>| {
                let head = self.html_head(options, chunk_docs);
                let mut html = head;
                html.push_str(body);
                html.push_str("</body></html>");
                chunks.push(html);
                body.clear();
                chunk_docs.clear();
            };
        for (index, piece) in pieces.iter().enumerate() {
            let fixed_layout = piece.doc.layout == SpineLayout::PrePaginated;
            if fixed_layout && !body.is_empty() {
                flush(&mut body, &mut chunks, &mut chunk_docs);
                body_images = 0;
            }
            let mut section = String::new();
            let body_html = svg_hrefs_to_book_paths(&piece.body, parent_dir(&piece.doc.href));
            let body_html = std::borrow::Cow::Owned(body_html);
            push_section_html(
                &mut section,
                piece.doc,
                piece.slot,
                &body_html,
                piece.continuation,
            );
            if !chunk_docs.iter().any(|doc| std::ptr::eq(*doc, piece.doc)) {
                chunk_docs.push(piece.doc);
            }
            let images = piece_images.get(index).copied().unwrap_or(0);
            let over_text = !body.is_empty() && body.len() + section.len() > max_bytes;
            let over_images = image_budget.is_some_and(|budget| {
                !body.is_empty() && body_images > 0 && body_images + images > budget
            });
            // The text and image caps are separate, but a chunk holds both at
            // the same time: bound their sum against the layout budget too.
            let over_layout = layout_bytes != usize::MAX
                && !body.is_empty()
                && (body.len() + section.len()) * DOM_BYTES_PER_HTML_BYTE + body_images + images
                    > layout_bytes;
            if over_text || over_images || over_layout {
                flush(&mut body, &mut chunks, &mut chunk_docs);
                body_images = 0;
            }
            body.push_str(&section);
            body_images += images;
            // A part that was split out is already at the size limit; keep it
            // in its own chunk so the layout budget holds.
            if fixed_layout || piece.continuation || piece.body.len() >= max_bytes {
                flush(&mut body, &mut chunks, &mut chunk_docs);
                body_images = 0;
            }
        }
        if !body.is_empty() || chunks.is_empty() {
            flush(&mut body, &mut chunks, &mut chunk_docs);
        }
        Ok(chunks)
    }

    fn html_head(&self, options: &HtmlOptions, docs: &[&SpineDocument]) -> String {
        let mut css_urls = Vec::new();
        let mut inline_styles = Vec::new();
        for doc in docs {
            for url in &doc.css_urls {
                if !css_urls.contains(url) {
                    css_urls.push(url.clone());
                }
            }
            for style in &doc.inline_styles {
                if !inline_styles.contains(style) {
                    inline_styles.push(style.clone());
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
        for style in &inline_styles {
            html.push_str("<style>");
            html.push_str(style);
            html.push_str("</style>");
        }
        // crengine gives every EPUB spine item its own <DocFragment> and its
        // epub.css puts `page-break-before: always` on it, so a chapter always
        // starts a new page. Without this, chapters that share a chunk would
        // flow into each other and page numbers would depend on chunk cuts.
        html.push_str("<style>/* cre-render-base */");
        html.push_str(&format!(
            "html, body {{ margin: 0; padding: 0; }}\n\
             body {{ font-family: serif; font-size: {FONT_SIZE_TOKEN}px; line-height: {}; \
             padding: {}px {}px; }}\n\
             img {{ max-width: 100%; height: auto; }}\n\
             .cre-spine {{ display: block; page-break-before: always; }}\n",
            options.line_height, options.margin_y, options.margin_x
        ));
        html.push_str("</style></head><body>");
        html
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
    let class = if continuation {
        "cre-spine-cont"
    } else {
        "cre-spine"
    };
    out.push_str(&format!(
        "<section class=\"{class}\" data-spine-index=\"{slot}\" data-spine-real-index=\"{}\" \
         data-spine-id=\"{}\" data-spine-href=\"{}\"{nonlinear_attr}>",
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
    let lower = html.to_ascii_lowercase();
    let Some(body_start) = lower.find("<body") else {
        return vec![html.to_string()];
    };
    let Some(body_open_end) = lower[body_start..]
        .find('>')
        .map(|end| body_start + end + 1)
    else {
        return vec![html.to_string()];
    };
    let Some(body_close) = lower.rfind("</body>") else {
        return vec![html.to_string()];
    };
    if body_close < body_open_end {
        return vec![html.to_string()];
    }
    let body = &html[body_open_end..body_close];
    let tail = &html[body_close..];
    let parts = split_body_html(body, limit);
    if parts.len() <= 1 {
        return vec![html.to_string()];
    }
    let head = &html[..body_open_end];
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
            let part = if index == 0 {
                part
            } else {
                renumber_spine_indices(&part, index)
            };
            format!("{head}{part}{tail}")
        })
        .collect()
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

/// Points the `<image>` hrefs of inline SVG blocks at `epub://book/...`.
///
/// The parse-time inliner then needs no document directory: chunks can hold
/// documents from several directories.
/// Turns `<ruby>base<rt>ann</rt></ruby>` into two stacked, centred inline
/// blocks.
///
/// Blitz has no ruby layout: it lays the annotation out as ordinary inline text
/// after the base character, so the note sits to the right of its character and
/// the line keeps the annotation's own advance. A ruby cell is a small vertical
/// stack instead - annotation above the base, both centred, the cell as wide as
/// the wider of the two - which is how crengine and browsers draw it.
fn rewrite_ruby(html: &str) -> String {
    if !html
        .as_bytes()
        .windows(5)
        .any(|window| window.eq_ignore_ascii_case(b"<ruby"))
    {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len() + html.len() / 16);
    let mut cursor = 0usize;
    while let Some(open_start) = next_tag_start(html, cursor, "<ruby") {
        let Some((close_start, close_end)) =
            find_matching_close_tag(html, open_start, "<ruby", "</ruby")
        else {
            break;
        };
        out.push_str(&html[cursor..open_start]);
        let inner = &html[find_tag_end(html, open_start).unwrap_or(close_start)..close_start];
        match ruby_cell(inner) {
            Some(cell) => out.push_str(&cell),
            None => out.push_str(&html[open_start..close_end]),
        }
        cursor = close_end;
    }
    out.push_str(&html[cursor..]);
    out
}

/// The two stacked blocks of one ruby cell, or `None` when the markup is not a
/// plain base + annotation pair.
fn ruby_cell(inner: &str) -> Option<String> {
    let lower = inner.to_ascii_lowercase();
    let rt_start = next_tag_start(&lower, 0, "<rt")?;
    let rt_body_start = find_tag_end(inner, rt_start)?;
    let rt_end = next_tag_start(&lower, rt_body_start, "</rt")?;
    // Only the first pair is handled; anything else (multiple annotations,
    // nested markup we do not understand) is left to the normal inline path.
    if next_tag_start(&lower, rt_end + "</rt".len(), "<rt").is_some() {
        return None;
    }
    let base = strip_ruby_wrappers(&inner[..rt_start]);
    let annotation = strip_ruby_wrappers(&inner[rt_body_start..rt_end]);
    if base.trim().is_empty() || annotation.trim().is_empty() {
        return None;
    }
    // The cell keeps the width of the annotated characters, the note is centred
    // above them and squeezed to that width whatever its length, and a zhuyin
    // tone mark sits on a row of its own above the letters, flush right over its
    // character. That is how Chrome typesets ruby, and it is also the only way
    // the characters keep their normal rhythm: sizing the cell to the note
    // instead pushes every character on the line apart.
    Some(ruby_cell_markup(&base, &annotation))
}

/// One ruby cell: the base in flow, the note absolutely positioned above it.
///
/// The base comes first so the inline-block reports its baseline (a block that
/// follows the note would make the character drop below the surrounding text),
/// and the note lives in a box exactly as wide as the base, which is what makes
/// the centring and the flush-right tone mark line up with the character rather
/// than with the cell (the cell also holds the book's letter-spacing).
fn ruby_cell_markup(base: &str, annotation: &str) -> String {
    let (tones, letters) = if is_zhuyin(annotation) {
        split_tone_marks(annotation)
    } else {
        (String::new(), annotation.trim().to_string())
    };
    let has_tones = !tones.trim().is_empty();
    let has_letters = !letters.trim().is_empty();
    let base_em = ruby_em(base).max(0.5);
    // The note is drawn at half size; squeeze it onto the character when it is
    // wider, so a three- or four-letter note still fits over one ideograph.
    let letters_em = ruby_em(&letters) * 0.5;
    let scale = if letters_em > base_em * 1.02 {
        (base_em / letters_em).clamp(0.35, 1.0)
    } else {
        1.0
    };
    let mut note = String::new();
    if has_tones {
        note.push_str(&format!(
            r#"<span style="position:absolute;right:0;top:0;white-space:nowrap;font-size:50%;line-height:1;letter-spacing:normal;">{tones}</span>"#
        ));
    }
    if has_letters {
        let letters = if scale < 0.995 {
            format!(
                r#"<span style="display:inline-block;transform:scaleX({scale:.3});transform-origin:center;">{letters}</span>"#
            )
        } else {
            letters
        };
        let top = if has_tones { "0.5em" } else { "0em" };
        note.push_str(&format!(
            r#"<span style="position:absolute;left:50%;top:{top};transform:translateX(-50%);white-space:nowrap;font-size:50%;line-height:1;letter-spacing:normal;">{letters}</span>"#
        ));
    }
    // Reserve the rows the note occupies, so the line box grows like it did when
    // the note was laid out in flow.
    let pad = if has_tones { "1em" } else { "0.5em" };
    format!(
        r#"<span style="display:inline-block;position:relative;vertical-align:baseline;padding-top:{pad};"><span style="display:block;white-space:nowrap;line-height:1.15;">{base}</span><span style="position:absolute;left:0;top:0;width:{base_em:.3}em;">{note}</span></span>"#
    )
}

/// True when the annotation uses bopomofo (zhuyin) letters.
fn is_zhuyin(text: &str) -> bool {
    text.chars()
        .any(|ch| matches!(ch as u32, 0x3100..=0x312F | 0x31A0..=0x31BF))
}

/// Splits the tone marks of a zhuyin annotation from its letters.
///
/// The tone letters are the spacing modifier letters `ˉ ˊ ˇ ˋ ˙`; a book that
/// stores the tone as a combining mark gets that mark moved up as well.
fn split_tone_marks(annotation: &str) -> (String, String) {
    let mut tones = String::new();
    let mut letters = String::new();
    for ch in annotation.chars() {
        let code = ch as u32;
        let is_tone = matches!(
            code,
            0x02C7 | 0x02C9 | 0x02CA..=0x02CB | 0x02D9 | 0x0300..=0x036F | 0xFE20..=0xFE2F
        );
        if is_tone {
            tones.push(ch);
        } else if !ch.is_whitespace() {
            letters.push(ch);
        }
    }
    (tones, letters)
}

/// Approximate advance of `text` in em, for squeezing a ruby note onto its base.
///
/// Full-width characters count one em, Latin and other narrow ones half; the
/// result only has to be close enough to fit the note into the character.
fn ruby_em(text: &str) -> f32 {
    text.chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '<' && *ch != '>')
        .map(|ch| {
            let code = ch as u32;
            match code {
                0x0300..=0x036F | 0x02C7 | 0x02C9 | 0x02CA..=0x02CB | 0x02D9 | 0xFE20..=0xFE2F => {
                    0.0
                }
                0x2E80..=0xF9FF | 0xFF00..=0xFF60 => 1.0,
                _ => 0.5,
            }
        })
        .sum()
}

/// Drops `<rb>`/`<rp>` wrappers and their closing tags, keeping the text.
fn strip_ruby_wrappers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while let Some(open) = text[cursor..].find('<').map(|offset| cursor + offset) {
        out.push_str(&text[cursor..open]);
        let Some(end) = find_tag_end(text, open) else {
            return out;
        };
        let name = tag_name(&text[open..end]);
        if !matches!(name.as_str(), "rb" | "rp" | "rt") {
            out.push_str(&text[open..end]);
        }
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
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

/// Rewrites the images inside inline SVG blocks to `data:` URIs.
///
/// Blitz parses an inline `<svg>` with usvg's default href resolver, which only
/// understands `data:` URIs; a relative `xlink:href` makes the image node
/// unreadable and the whole SVG is dropped (fixed-layout pages render blank,
/// including any text the SVG draws). Passing the bytes inline lets usvg build
/// the tree, so the image, the text and the link hotspots all survive.
///
/// The rewrite is for the HTML handed to the parser only: storing it would
/// keep a base64 copy of every page image for the whole session.
pub fn inline_svg_images_to_data_uris(html: &str, provider: &LazyResource) -> Option<String> {
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    let mut changed = false;
    while let Some(open_start) = next_tag_start(html, cursor, "<svg") {
        let Some((_close_start, close_end)) =
            find_matching_close_tag(html, open_start, "<svg", "</svg")
        else {
            break;
        };
        out.push_str(&html[cursor..open_start]);
        let block = &html[open_start..close_end];
        let (rewritten, block_changed) = svg_block_inline_images(block, provider);
        changed |= block_changed;
        out.push_str(&rewritten);
        cursor = close_end;
    }
    if !changed {
        return None;
    }
    out.push_str(&html[cursor..]);
    Some(out)
}

/// Replaces the href of every `<image>` in one SVG block with a data URI.
fn svg_block_inline_images(block: &str, provider: &LazyResource) -> (String, bool) {
    let mut out = String::with_capacity(block.len());
    let mut cursor = 0usize;
    let mut changed = false;
    while let Some(start) = next_tag_start(block, cursor, "<image") {
        let Some(end) = find_tag_end(block, start) else {
            break;
        };
        out.push_str(&block[cursor..start]);
        let tag = &block[start..end];
        let mut replaced = tag.to_string();
        let attrs = parse_attrs(tag);
        if let Some(href) = attrs.get("xlink:href").or_else(|| attrs.get("href")) {
            let href = href.trim();
            let book_url = href.starts_with("epub://book/");
            if !href.is_empty() && !href.starts_with("data:") && (book_url || !is_external(href)) {
                let without_fragment = href.split('#').next().unwrap_or(href);
                let relative = without_fragment
                    .strip_prefix("epub://book/")
                    .unwrap_or(without_fragment);
                let key = normalize_path(&percent_decode_str(relative).decode_utf8_lossy());
                if let Some(bytes) = provider.get(&key) {
                    let mime = image_mime(&bytes, &key);
                    let encoded = base64_encode(&bytes);
                    let data_uri = format!("data:{mime};base64,{encoded}");
                    replaced = replace_attribute_value(tag, "xlink:href", &data_uri)
                        .or_else(|| replace_attribute_value(tag, "href", &data_uri))
                        .unwrap_or(replaced);
                    changed = true;
                }
            }
        }
        out.push_str(&replaced);
        cursor = end;
    }
    out.push_str(&block[cursor..]);
    (out, changed)
}

/// Rewrites `name="value"` (or single quotes) inside one tag.
fn replace_attribute_value(tag: &str, name: &str, value: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let at = lower.find(&format!("{name}="))?;
    let rest = &tag[at + name.len() + 1..];
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let close = rest[1..].find(quote)? + 1;
    let mut out = String::with_capacity(tag.len() + value.len());
    out.push_str(&tag[..at + name.len() + 1]);
    out.push(quote);
    out.push_str(value);
    out.push_str(&rest[close..]);
    Some(out)
}

/// Estimated decoded (RGBA) bytes of the images a generated chunk refers to.
///
/// Chunk HTML points at resources through `epub://book/`, so no document
/// directory is needed. `cre-lib` uses this to include the decoded-image cost
/// of a chunk in its low-memory estimate.
pub fn chunk_image_bytes(provider: &LazyResource, chunk_html: &str) -> usize {
    image_sources(chunk_html, "")
        .iter()
        .filter_map(|key| provider.estimated_decoded_bytes(key))
        .sum()
}

/// Root-relative resource paths of every `<img>` in a chunk body. Spine
/// parsing rewrites `src` to `epub://book/...`, so both forms are accepted.
fn image_sources(body: &str, doc_dir: &str) -> Vec<String> {
    // ASCII lowercasing keeps byte offsets, so positions found in `lower` can
    // be used to slice the original text (zip names are case-sensitive).
    let lower = body.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut index = 0usize;
    while let Some(found) = lower[index..].find("<img") {
        let start = index + found;
        let Some(end) = lower[start..].find('>').map(|offset| start + offset) else {
            break;
        };
        if let Some(src_at) = lower[start..end].find("src=") {
            let rest = &body[start + src_at + 4..end];
            let rest = rest.trim_start();
            if let Some(quote @ ('"' | '\'')) = rest.chars().next()
                && let Some(close) = rest[1..].find(quote)
            {
                let value = rest[1..1 + close].split('#').next().unwrap_or("");
                if let Some(relative) = value.strip_prefix("epub://book/") {
                    out.push(normalize_path(relative));
                } else if !value.is_empty() && !value.starts_with("data:") && !value.contains("://")
                {
                    out.push(normalize_path(&format!("{doc_dir}{value}")));
                }
            }
        }
        index = end + 1;
    }
    out
}

/// Width and height from an image header, without decoding the pixels.
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

/// Memory a chunk's text costs per byte of generated HTML (DOM nodes,
/// computed styles and line boxes). Matches `cre-lib`'s chunk planner.
const DOM_BYTES_PER_HTML_BYTE: usize = 128;

/// Rough decoded size of an image resource, from its file size and format.
fn estimated_decoded_image_bytes(name: &str, file_bytes: usize) -> usize {
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

fn tag_name(raw: &str) -> String {
    let rest = raw.trim_start_matches('<').trim_start_matches('/');
    rest.chars()
        .take_while(|ch| !ch.is_whitespace() && *ch != '>' && *ch != '/')
        .collect::<String>()
        .to_ascii_lowercase()
}

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
            styles.push(css);
        }
        cursor = text_end + 8;
    }

    let _ = opf_dir;
    (css_urls, styles)
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
            if lower == "src" || lower == "href" || lower == "poster" || lower == "data" {
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
                            let rewritten = if is_external(value) {
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
                            let rewritten = if is_external(value) {
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

fn parse_attrs(tag: &str) -> HashMap<String, String> {
    let mut attrs = HashMap::new();
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
            let name = tag[start..i].to_ascii_lowercase();
            while i < tag.len() && tag[i..].chars().next().is_some_and(|c| c.is_whitespace()) {
                i += 1;
            }
            if i < tag.len() && tag[i..].starts_with('=') {
                i += 1;
                while i < tag.len() && tag[i..].chars().next().is_some_and(|c| c.is_whitespace()) {
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
                        attrs.insert(name, tag[value_start..i].to_string());
                        if i < tag.len() {
                            i += 1;
                        }
                    }
                }
            }
        } else {
            i += c.len_utf8();
        }
    }
    attrs
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

fn file_extension(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default()
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

fn next_tag_start(text: &str, from: usize, tag_lower: &str) -> Option<usize> {
    if from > text.len() {
        return None;
    }
    let mut from = from;
    while let Some(rel) = text[from..].find(tag_lower) {
        let pos = from + rel;
        if is_tag_boundary(text, pos + tag_lower.len()) {
            return Some(pos);
        }
        from = pos + tag_lower.len();
    }
    None
}

fn is_tag_boundary(text: &str, pos: usize) -> bool {
    if pos >= text.len() {
        return true;
    }
    match text[pos..].chars().next() {
        Some(c) => c.is_whitespace() || c == '>' || c == '/',
        None => true,
    }
}

fn find_matching_close_tag(
    text: &str,
    open_start: usize,
    open_tag: &str,
    close_tag: &str,
) -> Option<(usize, usize)> {
    let mut depth = 1usize;
    let mut scan = open_start + open_tag.len();

    while scan < text.len() {
        let next_open = next_tag_start(text, scan, open_tag);
        let next_close = next_tag_start(text, scan, close_tag);
        match (next_open, next_close) {
            (None, None) => return None,
            (Some(open_pos), None) => {
                depth += 1;
                scan = find_tag_end(text, open_pos)?;
            }
            (None, Some(close_pos)) => {
                depth -= 1;
                let close_end = find_tag_end(text, close_pos)?;
                if depth == 0 {
                    return Some((close_pos, close_end));
                }
                scan = close_end;
            }
            (Some(open_pos), Some(close_pos)) => {
                if close_pos < open_pos {
                    depth -= 1;
                    let close_end = find_tag_end(text, close_pos)?;
                    if depth == 0 {
                        return Some((close_pos, close_end));
                    }
                    scan = close_end;
                } else {
                    depth += 1;
                    scan = find_tag_end(text, open_pos)?;
                }
            }
        }
    }

    None
}

fn find_tag_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if start >= bytes.len() || bytes[start] != b'<' {
        return None;
    }
    let mut i = start + 1;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let byte = bytes[i];
        if let Some(quote_byte) = quote {
            if byte == quote_byte {
                quote = None;
            }
        } else if byte == b'"' || byte == b'\'' {
            quote = Some(byte);
        } else if byte == b'>' {
            return Some(i + 1);
        }
        i += 1;
    }
    None
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

fn rewrite_full_page_image_tags(
    html: &str,
    doc_dir: &str,
    provider: &LazyResource,
    generated: &mut HashMap<String, Arc<Vec<u8>>>,
    generated_counter: &mut usize,
    available: (f32, f32),
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
            generated_counter,
            available,
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

fn full_page_image_replacement(
    tag: &str,
    doc_dir: &str,
    provider: &LazyResource,
    generated: &mut HashMap<String, Arc<Vec<u8>>>,
    generated_counter: &mut usize,
    available: (f32, f32),
) -> Option<String> {
    let (available_width, available_height) = available;
    let attrs = parse_attrs(tag);
    let full_height = attrs
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
    *generated_counter += 1;
    let generated_key = generated_full_page_image_key(*generated_counter);
    let svg =
        full_page_image_svg_document(available_width, available_height, image_mime, &image_bytes);
    generated.insert(generated_key.clone(), Arc::new(svg.into_bytes()));

    Some(full_page_image_tag(&generated_key, &attrs))
}

fn generated_full_page_image_key(counter: usize) -> String {
    format!("cre-page-fit-{counter}.svg")
}

fn full_page_image_svg_document(width: f32, height: f32, mime: &str, image_bytes: &[u8]) -> String {
    let encoded = base64_encode(image_bytes);
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" width=\"{width:.0}\" height=\"{height:.0}\" viewBox=\"0 0 {width:.0} {height:.0}\"><image x=\"0\" y=\"0\" width=\"{width:.0}\" height=\"{height:.0}\" preserveAspectRatio=\"xMidYMid meet\" xlink:href=\"data:{mime};base64,{encoded}\" /></svg>"
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

fn image_mime(data: &[u8], path: &str) -> &'static str {
    if data.starts_with(&[0x89, 0x50, 0x4e, 0x47]) {
        return "image/png";
    }
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        return "image/jpeg";
    }
    if data.starts_with(b"GIF8") {
        return "image/gif";
    }
    if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return "image/webp";
    }
    if data.starts_with(b"BM") {
        return "image/bmp";
    }
    match file_extension(path).as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" | "svgz" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let value = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((value >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((value >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((value >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(value & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn is_external(value: &str) -> bool {
    let value = value.trim();
    value.is_empty()
        || value.starts_with('#')
        || value.starts_with('/')
        || value.starts_with("data:")
        || value.starts_with("mailto:")
        || value.contains("://")
        || value.starts_with("javascript:")
}

fn value_without_fragment(value: &str) -> String {
    value.split('#').next().unwrap_or(value).to_string()
}

pub fn resolve_href(base_dir: &str, href: &str) -> String {
    let href = percent_decode_str(href).decode_utf8_lossy();
    let href = href.replace('\\', "/");
    let path = if href.starts_with('/') {
        href.trim_start_matches('/').to_string()
    } else {
        format!("{base_dir}/{href}")
    };
    normalize_path(&path)
}

pub fn normalize_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
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

fn encode_path(path: &str) -> String {
    path.replace('%', "%25")
        .replace(' ', "%20")
        .replace('#', "%23")
        .replace('?', "%3F")
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

#[cfg(test)]
mod tests {
    #[test]
    fn zhuyin_ruby_squeezes_the_note_onto_the_character() {
        let cell = ruby_cell("想<rt>ㄒㄧㄤˇ</rt>").unwrap();
        // Tone mark on its own row, flush right over the character.
        assert!(cell.contains("right:0;top:0"), "{cell}");
        assert!(cell.contains(">ˇ</span>"), "{cell}");
        // The three letters are centred over the base and squeezed to fit it.
        assert!(cell.contains("translateX(-50%)"), "{cell}");
        assert!(cell.contains("scaleX("), "{cell}");
        assert!(cell.contains("padding-top:1em"), "{cell}");
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
        assert!(html.contains("p.cdata-marker { color: #ff0000; }"));
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
        assert!(generated_svg.contains("preserveAspectRatio=\"xMidYMid meet\""));
        assert!(generated_svg.contains("xlink:href=\"data:image/jpeg;base64,"));

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
        let html = book.to_html(&HtmlOptions::default()).unwrap();
        assert!(html.contains("epub://book/images/one.jpg"));
        assert!(html.contains("max-height:736px"));
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
    <img src="images/one.jpg" alt="Inline" style="max-width: 100%; height: auto;" />
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

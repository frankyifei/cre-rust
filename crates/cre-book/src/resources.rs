//! Where a document's resources come from, and what the engine makes up.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use percent_encoding::percent_decode_str;

/// A container that can hand out its resources by key.
///
/// A zip archive, a PalmDB record index, a directory of files: the format crate
/// decides the key scheme, and everything downstream (layout, painting,
/// long-press, `@font-face`) only uses [`ResourceSource::get`].
pub trait ResourceProvider: Send + Sync {
    fn get(&self, key: &str) -> Option<Arc<Vec<u8>>>;

    /// Estimated decoded image size, for the chunk memory budget.
    fn estimated_decoded_bytes(&self, _key: &str) -> Option<usize> {
        None
    }

    /// Bytes a decompressed-resource cache holds, and how much of it is pinned.
    fn cache_stats(&self) -> (usize, usize) {
        (0, 0)
    }

    /// Bytes that cannot be evicted (generated resources without a backing
    /// entry).
    fn pinned_bytes(&self) -> usize {
        0
    }

    /// Drops the container's pages after the open pass.
    fn drop_pages(&self) {}
}

/// Resources carried in memory or indexed in the book file.
#[derive(Clone)]
pub enum MapResources {
    Owned(Arc<HashMap<String, Vec<u8>>>),
    Mapped(Arc<cre_pdb::BookResources>),
}

impl MapResources {
    pub fn get(&self, key: &str) -> Option<&[u8]> {
        match self {
            Self::Owned(map) => map.get(key).map(Vec::as_slice),
            Self::Mapped(index) => index.get(key),
        }
    }

    pub fn contains_key(&self, key: &str) -> bool {
        match self {
            Self::Owned(map) => map.contains_key(key),
            Self::Mapped(index) => index.contains_key(key),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Owned(map) => map.len(),
            Self::Mapped(index) => index.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes of the resources the index can reach. For a mapped PalmDB these
    /// are file pages, not anonymous memory.
    pub fn bytes(&self) -> usize {
        match self {
            Self::Owned(map) => map.values().map(Vec::len).sum(),
            Self::Mapped(index) => index.bytes(),
        }
    }

    /// Drops the mapped pages the open pass faulted in; a page that paints a
    /// picture faults it back in from the page cache.
    pub fn drop_pages(&self) {
        if let Self::Mapped(index) = self {
            index.drop_pages();
        }
    }
}

/// Resources the engine generates while preparing a book (page-fit wrappers,
/// decoded `data:` pictures). Shared by the document, the net providers and the
/// painter, so a generated key behaves like any resource.
pub type GeneratedResources = Arc<std::sync::Mutex<HashMap<String, Arc<Vec<u8>>>>>;

/// One generated resource, if the key names one.
pub fn generated_resource(generated: &GeneratedResources, key: &str) -> Option<Arc<Vec<u8>>> {
    generated.lock().ok()?.get(key).cloned()
}

/// Where a document's resources come from, plus the ones the engine makes up.
///
/// A standalone HTML document has no archive to index: its pictures and
/// stylesheets sit next to it on disk, and some of them are inlined in the
/// markup as `data:` URIs. Rather than special-casing that per caller, every
/// source answers the same `get(key)`, and [`Self::generated`] holds what the
/// engine synthesises while preparing the book (page-fit wrappers, decoded
/// `data:` pictures) so the net providers and the painter see one namespace.
#[derive(Clone)]
pub struct ResourceSource {
    base: ResourceBase,
    generated: GeneratedResources,
    /// Whether the book's own `@font-face` fonts may be used. It lives here so
    /// the net providers - which hold the source, not the document - apply the
    /// setting when they serve a stylesheet.
    fonts_enabled: Arc<std::sync::atomic::AtomicBool>,
    /// The reader's font size, in CSS pixels, as `f32::to_bits`.
    ///
    /// Like the font flag, this is a setting that changes what a served
    /// stylesheet *means*: a book's `font-size: x-large` is a size only once
    /// the medium is known, and the engine rewrites it when it hands the sheet
    /// to Blitz (see `normalise_served_stylesheet` in `cre-lib`). It lives here
    /// for the same reason: the net provider serves the sheet, not the document.
    css_font_size: Arc<std::sync::atomic::AtomicU32>,
}

#[derive(Clone)]
enum ResourceBase {
    Empty,
    /// Resources carried in memory or indexed in the book file.
    Map(MapResources),
    /// A container that hands out bytes behind a shared provider (a zip).
    Provider(Arc<dyn ResourceProvider>),
    /// A standalone document: resources are files next to it.
    Files(std::path::PathBuf),
}

impl ResourceSource {
    fn new(base: ResourceBase) -> Self {
        Self {
            base,
            generated: Arc::new(std::sync::Mutex::new(HashMap::new())),
            fonts_enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            // 16px is the browser's medium; the document overwrites it with the
            // reader's size before the first layout fetches a stylesheet.
            css_font_size: Arc::new(std::sync::atomic::AtomicU32::new((16.0f32).to_bits())),
        }
    }

    pub fn empty() -> Self {
        Self::new(ResourceBase::Empty)
    }

    pub fn map(map: MapResources) -> Self {
        Self::new(ResourceBase::Map(map))
    }

    pub fn provider(provider: Arc<dyn ResourceProvider>) -> Self {
        Self::new(ResourceBase::Provider(provider))
    }

    pub fn files(root: impl Into<std::path::PathBuf>) -> Self {
        Self::new(ResourceBase::Files(root.into()))
    }

    /// Resource bytes for `key`: generated resources first, then the book.
    pub fn get(&self, key: &str) -> Option<Arc<Vec<u8>>> {
        let key = normalize_key(key);
        if let Some(bytes) = generated_resource(&self.generated, &key) {
            return Some(bytes);
        }
        match &self.base {
            ResourceBase::Empty => None,
            ResourceBase::Map(map) => map.get(&key).map(|bytes| Arc::new(bytes.to_vec())),
            ResourceBase::Provider(provider) => provider.get(&key),
            ResourceBase::Files(root) => read_file_resource(root, &key),
        }
    }

    /// The key a URL resolves to inside this document.
    ///
    /// A standalone document's `src` resolves to a file path, while every other
    /// source keys resources relative to the book; callers that hold a URL (the
    /// painter, long-press) use this so the same table answers for both.
    pub fn key_for(&self, path: &str) -> String {
        let path = normalize_key(path);
        match &self.base {
            ResourceBase::Files(root) => {
                let root = normalize_key(&root.to_string_lossy());
                path.strip_prefix(&root)
                    .map(|rest| rest.trim_start_matches('/').to_string())
                    .unwrap_or(path)
            }
            _ => path,
        }
    }

    /// Registers a resource the engine generated for this document.
    pub fn insert(&self, key: impl Into<String>, bytes: impl Into<Arc<Vec<u8>>>) {
        if let Ok(mut generated) = self.generated.lock() {
            generated.insert(key.into(), bytes.into());
        }
    }

    /// The document's "use the book's fonts" flag, shared with the net
    /// providers and with the document itself.
    pub fn fonts_enabled(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.fonts_enabled)
    }

    /// The generated-resource table, shared with the net providers.
    pub fn generated(&self) -> GeneratedResources {
        Arc::clone(&self.generated)
    }

    /// The reader's font size for the stylesheets served from this source,
    /// shared with the net providers and with the document itself.
    pub fn css_font_size(&self) -> Arc<std::sync::atomic::AtomicU32> {
        Arc::clone(&self.css_font_size)
    }

    /// Records the reader's font size, before a layout resolves the CSS.
    pub fn set_css_font_size(&self, font_size: f32) {
        self.css_font_size
            .store(font_size.to_bits(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Estimated decoded size of an image, for the chunk memory budget.
    pub fn estimated_decoded_bytes(&self, key: &str) -> Option<usize> {
        match &self.base {
            ResourceBase::Empty => None,
            ResourceBase::Provider(provider) => provider.estimated_decoded_bytes(key),
            ResourceBase::Map(map) => {
                let key = normalize_key(key);
                map.get(&key)
                    .map(|bytes| crate::estimated_image_bytes(&key, bytes))
            }
            ResourceBase::Files(root) => {
                let bytes = read_file_resource(root, key)?;
                Some(crate::estimated_image_bytes(key, &bytes))
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self.base, ResourceBase::Empty)
    }

    /// Whether the base is an archive provider (as opposed to a map or files).
    pub fn is_archive(&self) -> bool {
        matches!(self.base, ResourceBase::Provider(_))
    }

    /// Bytes the container's resource cache holds.
    pub fn cache_bytes(&self) -> usize {
        match &self.base {
            ResourceBase::Provider(provider) => provider.cache_stats().1,
            ResourceBase::Map(map) => map.bytes(),
            ResourceBase::Empty | ResourceBase::Files(_) => 0,
        }
    }

    /// Bytes in the container's cache that cannot be evicted.
    pub fn pinned_bytes(&self) -> usize {
        match &self.base {
            ResourceBase::Provider(provider) => provider.pinned_bytes(),
            ResourceBase::Empty | ResourceBase::Map(_) | ResourceBase::Files(_) => 0,
        }
    }

    /// Drops the book file's pages after the open pass; see
    /// [`MapResources::drop_pages`].
    pub fn drop_pages(&self) {
        match &self.base {
            ResourceBase::Map(map) => map.drop_pages(),
            ResourceBase::Provider(provider) => provider.drop_pages(),
            ResourceBase::Empty | ResourceBase::Files(_) => {}
        }
    }
}

/// Reads one resource of a standalone document, refusing to leave its directory.
fn read_file_resource(root: &Path, key: &str) -> Option<Arc<Vec<u8>>> {
    let relative = percent_decode_str(key).decode_utf8_lossy();
    let candidate = std::fs::canonicalize(root.join(relative.as_ref())).ok()?;
    if !candidate.starts_with(root) || !candidate.is_file() {
        return None;
    }
    std::fs::read(&candidate).ok().map(Arc::new)
}

/// `a/./b/../c` -> `a/c`, with backslashes as separators.
pub fn normalize_key(path: &str) -> String {
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

//! What a container has to answer about its resources.
//!
//! Two traits, one per direction the bytes are read from:
//!
//! - [`ResourceProvider`] hands out owned bytes: a zip entry is decompressed, a
//!   directory file is read, and the result is shared as an [`Arc`]. Used by the
//!   net providers and the painter.
//! - [`ResourceIndex`] lends a slice: a memory-mapped container (a PalmDB record
//!   index) already has the bytes in its mapping, and copying a 4 MB picture to
//!   read its header would fault the whole thing in during the layout pass.
//!
//! They live in their own crate so neither side has to know the other: a
//! container crate (`cre-pdb`) implements them without depending on the
//! format-independent book layer, and `cre-book` holds one without depending on
//! any container.

use std::sync::Arc;

/// A container that can hand out its resources by key.
///
/// A zip archive, a directory of files: the format crate decides the key scheme,
/// and everything downstream (layout, painting, long-press, `@font-face`) only
/// uses the key.
pub trait ResourceProvider: Send + Sync {
    fn get(&self, key: &str) -> Option<Arc<Vec<u8>>>;

    /// The first `max_bytes` of a resource.
    ///
    /// One characterisation decides a page: an image's header gives its format
    /// and intrinsic size. A zip entry is one deflate stream and cannot be
    /// seeked into, so materialising a 4 MB picture just to read the twenty
    /// bytes that say "1000x1500" is the expensive way; a container that can
    /// stop early overrides this and reads a prefix instead. The default reads
    /// the resource and truncates it, which is never worse than [`Self::get`].
    fn header(&self, key: &str, max_bytes: usize) -> Option<Vec<u8>> {
        let bytes = self.get(key)?;
        let end = bytes.len().min(max_bytes);
        Some(bytes[..end].to_vec())
    }

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

/// Resources carried in memory or indexed in the book file itself.
///
/// The slice is borrowed on purpose. A mapped container's `get` is an offset
/// range into the mapping, and the readers below it (the image placeholder, the
/// intrinsic-size probe) only look at the first bytes of a picture: copying the
/// record to answer them faults every page of every picture in during the
/// layout pass, which is exactly what the mapping is there to avoid.
pub trait ResourceIndex: Send + Sync {
    fn get(&self, key: &str) -> Option<&[u8]>;

    fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes the index can reach. For a mapped file these are file pages, not
    /// anonymous memory.
    fn bytes(&self) -> usize;

    /// Drops the container's pages after the open pass; a page that paints a
    /// picture faults it back in from the page cache.
    fn drop_pages(&self) {}
}

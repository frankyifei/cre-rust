//! The chunk → page table.
//!
//! A book is laid out one chunk at a time, and a chunk is in exactly one of
//! three states: never laid out (its pages are an estimate), laid out for the
//! current settings, or laid out for settings that no longer apply (its pages
//! stay as a provisional list until it is laid out again). The page list — which
//! chunk page N belongs to, and where inside it — is derived from those states,
//! so [`PageTable`] owns both and only [`PageTable::rebuild`] writes the derived
//! part. The state is a three-way type rather than a `layout`/`stale` pair of
//! fields so the combinations that mean nothing (a fresh chunk with no layout,
//! a never-laid-out chunk with real pages) cannot be written down at all.
//!
//! `layout.rs` decides *when* a chunk is laid out; this module decides what page
//! numbers that produces.

use std::collections::HashMap;

use crate::{ChunkLayout, ChunkSource, PageStart};

/// Where a chunk starts in the page list.
pub(crate) struct ChunkMeta {
    /// 1-based page number of the chunk's first page.
    pub(crate) first_page: usize,
    /// Index of the chunk's first line in the book-wide line list.
    pub(crate) line_base: usize,
}

/// One page, flattened across chunks.
///
/// A page that belongs to a chunk with no real layout carries a stand-in: zero
/// extents and no flow, so drawing it falls back to the top of the chunk.
#[derive(Clone, Copy)]
pub(crate) struct PageMeta {
    pub(crate) chunk: usize,
    pub(crate) local_start: PageStart,
    pub(crate) local_clip_bottom: f32,
    pub(crate) doc_top: f32,
    pub(crate) render_height: f32,
    pub(crate) page_flow: i64,
}

/// What a chunk contributes to the page list.
pub(crate) enum ChunkState {
    /// Never laid out: the page count is the estimate the lazy open made from
    /// the chunk's size. Zero means there is nothing to assume and the chunk
    /// still occupies one stand-in page.
    Unlaid { provisional_pages: usize },
    /// Laid out with the current settings.
    Fresh(ChunkLayout),
    /// Laid out, but for settings that no longer apply.
    ///
    /// The pages stay in the list as a provisional list: they are in the right
    /// order and roughly the right count, and the reader is positioned by
    /// XPointer, so the chunk is only laid out again when it is needed.
    Stale(ChunkLayout),
}

impl ChunkState {
    pub(crate) fn layout(&self) -> Option<&ChunkLayout> {
        match self {
            ChunkState::Unlaid { .. } => None,
            ChunkState::Fresh(layout) | ChunkState::Stale(layout) => Some(layout),
        }
    }

    /// The pages this chunk adds to the page list, before the stand-in page an
    /// unlaid chunk always takes.
    pub(crate) fn page_count(&self) -> usize {
        match self {
            ChunkState::Unlaid { provisional_pages } => *provisional_pages,
            ChunkState::Fresh(layout) | ChunkState::Stale(layout) => layout.pages.len(),
        }
    }

    /// True when the chunk still has to be laid out for the current settings.
    pub(crate) fn needs_layout(&self) -> bool {
        !matches!(self, ChunkState::Fresh(_))
    }

    fn into_stale(self) -> Self {
        match self {
            ChunkState::Fresh(layout) => ChunkState::Stale(layout),
            other => other,
        }
    }
}

/// What a [`PageTable::rebuild`] derives besides the page and chunk lists.
///
/// These are the lookups the reader asks for by name instead of by page number:
/// an XPointer, a TOC href, a spine item.
#[derive(Default)]
pub(crate) struct PageIndex {
    /// Page-start XPointer to page number.
    pub(crate) xpointers: HashMap<String, usize>,
    /// Anchor (resource, fragment) to page number.
    pub(crate) anchors: HashMap<(String, Option<String>), usize>,
    /// Spine slot to the chunk that holds it.
    pub(crate) spine_to_chunk: HashMap<u32, usize>,
    /// Spine slot to the value its XPointer text uses.
    pub(crate) spine_real_index: HashMap<u32, u32>,
    /// Spine resource to the first page of its chunk, for a TOC entry whose
    /// chunk has not been laid out yet.
    pub(crate) first_page_by_href: HashMap<String, usize>,
    /// Highest non-linear flow seen in any layout.
    pub(crate) max_flow: i64,
}

/// The per-chunk states and the page list derived from them.
pub(crate) struct PageTable {
    entries: Vec<ChunkState>,
    chunks: Vec<ChunkMeta>,
    pages: Vec<PageMeta>,
}

impl PageTable {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
            chunks: Vec::new(),
            pages: Vec::new(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The flattened page list, in reading order.
    pub(crate) fn pages(&self) -> &[PageMeta] {
        &self.pages
    }

    pub(crate) fn chunks(&self) -> &[ChunkMeta] {
        &self.chunks
    }

    /// 1-based page number the chunk starts on, or 1 for a chunk that is not in
    /// the table.
    pub(crate) fn first_page(&self, chunk: usize) -> usize {
        self.chunks
            .get(chunk)
            .map(|meta| meta.first_page)
            .unwrap_or(1)
    }

    pub(crate) fn layout(&self, chunk: usize) -> Option<&ChunkLayout> {
        self.entries.get(chunk).and_then(ChunkState::layout)
    }

    /// Every chunk that has a layout, in reading order.
    pub(crate) fn layouts(&self) -> impl Iterator<Item = &ChunkLayout> {
        self.entries.iter().filter_map(ChunkState::layout)
    }

    /// Pages the chunk contributes, before the stand-in page of an unlaid chunk.
    pub(crate) fn page_count(&self, chunk: usize) -> usize {
        self.entries
            .get(chunk)
            .map(ChunkState::page_count)
            .unwrap_or(0)
    }

    /// True when the chunk still has to be laid out for the current settings.
    pub(crate) fn needs_layout(&self, chunk: usize) -> bool {
        self.entries
            .get(chunk)
            .is_some_and(ChunkState::needs_layout)
    }

    /// True when every chunk is laid out for the current settings.
    pub(crate) fn all_fresh(&self) -> bool {
        self.entries.iter().all(|state| !state.needs_layout())
    }

    /// The chunks that still have to be laid out, in reading order.
    pub(crate) fn pending(&self) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, state)| state.needs_layout())
            .map(|(index, _)| index)
            .collect()
    }

    /// Replaces the table with `count` chunks that have never been laid out.
    pub(crate) fn reset(&mut self, count: usize) {
        self.entries = (0..count)
            .map(|_| ChunkState::Unlaid {
                provisional_pages: 0,
            })
            .collect();
    }

    /// Records a finished layout for the current settings.
    pub(crate) fn install(&mut self, chunk: usize, layout: ChunkLayout) {
        if let Some(state) = self.entries.get_mut(chunk) {
            *state = ChunkState::Fresh(layout);
        }
    }

    /// Sets the page estimate of a chunk that has never been laid out.
    pub(crate) fn set_provisional(&mut self, chunk: usize, pages: usize) {
        if let Some(ChunkState::Unlaid { provisional_pages }) = self.entries.get_mut(chunk) {
            *provisional_pages = pages;
        }
    }

    /// Marks every layout as belonging to settings that no longer apply.
    ///
    /// The pages stay in the list, so the reader keeps its page numbers while
    /// the chunks are laid out again one by one.
    pub(crate) fn mark_all_stale(&mut self) {
        for state in &mut self.entries {
            let current = std::mem::replace(
                state,
                ChunkState::Unlaid {
                    provisional_pages: 0,
                },
            );
            *state = current.into_stale();
        }
    }

    /// Rescales the provisional page lists by `ratio`.
    ///
    /// A settings change re-lays one chunk out and measures how the page count
    /// moved; every chunk that keeps its old pages (or its estimate) is scaled
    /// the same way, so the total the reader is shown stays close to the truth
    /// until the layouts settle.
    pub(crate) fn resample_stale(&mut self, ratio: f32) {
        for state in &mut self.entries {
            match state {
                ChunkState::Fresh(_) => {}
                ChunkState::Stale(layout) => {
                    let target = (layout.pages.len() as f32 * ratio).round().max(1.0) as usize;
                    layout.resample(target);
                }
                ChunkState::Unlaid { provisional_pages } if *provisional_pages > 0 => {
                    *provisional_pages =
                        (*provisional_pages as f32 * ratio).round().max(1.0) as usize;
                }
                ChunkState::Unlaid { .. } => {}
            }
        }
    }

    /// Derives the page list, the chunk list and the name lookups from the
    /// states.
    ///
    /// `height` is the reader's page height, which a stand-in page advances the
    /// document offset by so the page map of an unlaid chunk still has a
    /// plausible `doc_y`.
    pub(crate) fn rebuild(&mut self, sources: &[ChunkSource], height: f32) -> PageIndex {
        let mut pages = Vec::new();
        let mut chunks = Vec::with_capacity(self.entries.len());
        let mut index = PageIndex::default();
        let mut line_base = 0usize;
        let mut running_doc_top = 0.0f32;
        for (chunk_index, state) in self.entries.iter().enumerate() {
            let first_page = pages.len() + 1;
            if let Some(layout) = state.layout() {
                for (local_index, page) in layout.pages.iter().enumerate() {
                    let doc_top = running_doc_top;
                    running_doc_top += page.render_height;
                    // The XPointer stays in the chunk layout (one copy as the
                    // reverse-map key); copying it into the flattened page list
                    // as well was a third copy.
                    if let Some(xpointer) = layout.xpointers.get(local_index).cloned().flatten() {
                        index.xpointers.insert(xpointer, first_page + local_index);
                    }
                    pages.push(PageMeta {
                        chunk: chunk_index,
                        local_start: page.start,
                        local_clip_bottom: page.clip_bottom,
                        doc_top,
                        render_height: page.render_height,
                        page_flow: page.page_flow,
                    });
                }
                for (key, local_page) in layout.href_pages.iter() {
                    index
                        .anchors
                        .entry(key.clone())
                        .or_insert(first_page + local_page);
                }
                index.max_flow = index.max_flow.max(layout.max_flow);
            } else {
                // Never laid out (a lazily opened book, or a chunk the worker
                // has not reached yet): stand in with the estimated page count so
                // that page numbers, the total and navigation exist from the
                // first draw. `ensure_page_layout` replaces them with real ones.
                for _ in 0..state.page_count().max(1) {
                    pages.push(PageMeta {
                        chunk: chunk_index,
                        local_start: PageStart {
                            top: 0.0,
                            clip_top: 0.0,
                        },
                        local_clip_bottom: 0.0,
                        doc_top: running_doc_top,
                        render_height: 0.0,
                        page_flow: 0,
                    });
                    running_doc_top += height;
                }
            }
            // The chunk → spine map is read from the chunk's markup, so it is
            // available whether or not the chunk has a layout. That is what lets
            // an XPointer jump into a chunk that has not been laid out yet.
            for section in sources
                .get(chunk_index)
                .map(|source| source.spine.as_slice())
                .unwrap_or(&[])
            {
                index
                    .spine_to_chunk
                    .entry(section.slot)
                    .or_insert(chunk_index);
                index
                    .spine_real_index
                    .entry(section.slot)
                    .or_insert(section.real);
                if let Some(href) = section.href.as_ref() {
                    index
                        .first_page_by_href
                        .entry(href.clone())
                        .or_insert(first_page);
                }
            }
            chunks.push(ChunkMeta {
                first_page,
                line_base,
            });
            line_base += state.layout().map(|layout| layout.line_count).unwrap_or(0);
        }
        // A chunk always opens on section 1, so a book that carries no section
        // markup at all still has a target for `/body/DocFragment[1]`.
        index.spine_to_chunk.entry(1).or_insert(0);
        self.pages = pages;
        self.chunks = chunks;
        index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LocalPage;

    /// A chunk layout with `count` pages, each 10px tall, and no lines.
    fn layout_with_pages(count: usize) -> ChunkLayout {
        ChunkLayout {
            pages: (0..count)
                .map(|index| LocalPage {
                    start: PageStart {
                        top: index as f32 * 10.0,
                        clip_top: index as f32 * 10.0,
                    },
                    clip_bottom: index as f32 * 10.0 + 10.0,
                    render_height: 10.0,
                    page_flow: 0,
                })
                .collect(),
            lines: Vec::new(),
            xpointers: (0..count).map(|_| None).collect(),
            href_pages: HashMap::new(),
            line_count: 0,
            max_flow: 0,
        }
    }

    /// Small xorshift so the test needs no dev-dependency and the sequence is
    /// reproducible from the seed.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: u64) -> usize {
            (self.next() % bound) as usize
        }
    }

    /// The state of one chunk as the test models it. `Laid` covers both the
    /// fresh and the stale layout: they contribute the same pages, and only a
    /// chunk that was *never* laid out takes an estimate.
    #[derive(Clone, Copy)]
    enum Model {
        Unlaid(usize),
        Laid(usize),
    }

    impl Model {
        fn page_count(self) -> usize {
            match self {
                Model::Unlaid(pages) | Model::Laid(pages) => pages,
            }
        }
    }

    /// The page list a whole rebuild has to produce: chunk states in, flattened
    /// pages out, as `(chunk, local page, doc top, height)`.
    ///
    /// Written from the page counts alone, which is how the callers - page
    /// numbers, the progress bar, the page map - reason about the book.
    fn digest_page_list(model: &[Model], height: f32) -> Vec<(usize, usize, f32, f32)> {
        let mut pages = Vec::new();
        let mut doc_top = 0.0f32;
        for (chunk, &state) in model.iter().enumerate() {
            match state {
                Model::Unlaid(pages_estimate) => {
                    // A never-laid-out chunk always takes at least one stand-in
                    // page, and each of those advances the document by a page.
                    for local in 0..pages_estimate.max(1) {
                        pages.push((chunk, local, doc_top, 0.0));
                        doc_top += height;
                    }
                }
                Model::Laid(count) => {
                    for local in 0..count {
                        pages.push((chunk, local, doc_top, 10.0));
                        doc_top += 10.0;
                    }
                }
            }
        }
        pages
    }

    #[test]
    fn rebuild_derives_the_page_list_from_the_chunk_states() {
        let sources: Vec<ChunkSource> = (0..6).map(|_| ChunkSource::new(String::new())).collect();
        let height = 800.0f32;
        for seed in 0..64u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut table = PageTable::new();
            table.reset(sources.len());
            let mut model = vec![Model::Unlaid(0); sources.len()];
            for _ in 0..40 {
                let chunk = rng.below(sources.len() as u64);
                match rng.below(4) {
                    0 => {
                        let pages = 1 + rng.below(5);
                        table.install(chunk, layout_with_pages(pages));
                        model[chunk] = Model::Laid(pages);
                    }
                    1 => {
                        // An estimate only exists on a chunk that was never laid
                        // out; installing a layout replaces it.
                        if matches!(model[chunk], Model::Unlaid(_)) {
                            let pages = rng.below(6);
                            table.set_provisional(chunk, pages);
                            model[chunk] = Model::Unlaid(pages);
                        }
                    }
                    2 => table.mark_all_stale(),
                    _ => {
                        table.reset(sources.len());
                        model.iter_mut().for_each(|state| *state = Model::Unlaid(0));
                    }
                }
                table.rebuild(&sources, height);
                let expected = digest_page_list(&model, height);
                let mut actual = Vec::with_capacity(table.pages().len());
                for (index, page) in table.pages().iter().enumerate() {
                    let local = index + 1 - table.first_page(page.chunk);
                    actual.push((page.chunk, local, page.doc_top, page.render_height));
                }
                assert_eq!(actual, expected, "seed {seed}");
                // Each chunk starts where the previous one ended.
                let mut first = 1;
                for (chunk, meta) in table.chunks().iter().enumerate() {
                    assert_eq!(meta.first_page, first, "seed {seed} chunk {chunk}");
                    assert_eq!(table.page_count(chunk), model[chunk].page_count());
                    first += model[chunk].page_count().max(1);
                }
            }
        }
    }

    #[test]
    fn stale_layouts_keep_their_pages_until_relaid_out() {
        let mut table = PageTable::new();
        table.reset(3);
        table.install(0, layout_with_pages(4));
        table.install(1, layout_with_pages(2));
        table.install(2, layout_with_pages(3));
        table.rebuild(&[], 800.0);
        assert!(!table.needs_layout(1));

        table.mark_all_stale();
        assert!(table.needs_layout(0) && table.needs_layout(2));
        assert_eq!(table.page_count(0), 4, "a stale chunk keeps its page list");
        assert_eq!(table.pages().len(), 9);

        // Laying one chunk out for the new settings replaces only its pages.
        table.install(1, layout_with_pages(5));
        table.rebuild(&[], 800.0);
        assert_eq!(table.pages().len(), 12);
        assert_eq!(table.first_page(2), 10);
        assert_eq!(table.pending(), vec![0, 2]);
    }

    #[test]
    fn resampling_scales_every_provisional_list_but_the_fresh_one() {
        let mut table = PageTable::new();
        table.reset(3);
        table.install(0, layout_with_pages(4));
        table.install(1, layout_with_pages(4));
        table.set_provisional(2, 4);
        table.mark_all_stale();
        table.install(0, layout_with_pages(6));

        table.resample_stale(1.5);
        assert_eq!(table.page_count(0), 6, "the fresh chunk is left alone");
        assert_eq!(table.page_count(1), 6);
        assert_eq!(table.page_count(2), 6);

        // A never-laid-out chunk with no estimate stays with nothing to scale.
        table.set_provisional(2, 0);
        table.resample_stale(2.0);
        assert_eq!(table.page_count(2), 0);
    }
}

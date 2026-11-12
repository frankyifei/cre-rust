# Memory model

The engine has to open big books on a 256 MB Kindle Paperwhite 2, where
KOReader itself already uses most of the memory. This document explains what a
book costs, what stays resident, and how a book that cannot fit is refused
instead of killing the reader.

## Why the book is split

A whole-book Blitz layout costs about **116-128 bytes of resident memory per
byte of generated HTML** (measured: 251 kB of XHTML -> 28 MB, 579 kB -> 44 MB).
Most of that is Stylo's computed style per element, the Taffy layout tree and
Parley's glyph runs, not the text itself.

A 24 MB text book would need roughly 3 GB laid out as one document. Splitting
it into chunks bounds the peak to one chunk's worth of DOM.

When the book does fit, it is still laid out as **one** document, like
crengine: page breaks, page numbers and TOC targets are then identical, and
reading never re-lays out a chunk in the middle of the book.

## Chunk planning

`plan_chunk_bytes` uses the memory the kernel can still hand out
(`/proc/meminfo` `MemAvailable`, or `free + buffers + cached` on the PW2's 3.0
kernel):

```text
budget   = min(available - 24 MB, available / 2)     (at least 8 MB)
max_html = budget / 128                               (the DOM cost above)
```

- If the whole book is smaller than `max_html`, the layout stays whole.
- Otherwise the chunk size is `max_html` clamped to **64-128 KB**.

A chunk is bounded by the **sum** of its text DOM and its decoded images,
because Blitz decodes every image of a chunk while it parses it. The image
share is 60% of the budget, or one third when less than 128 MB is available,
so a chunk activation mid-book stays affordable.

Chunks end at spine-item boundaries, so a chapter normally keeps its own
pagination. A single document that is over the budget on its own (a gallery
page with several full-page images) is split between its images, never between
paragraphs.

## What stays resident

| Item | Per chunk | Notes |
|---|---|---|
| Generated HTML | LZ4 block | decompressed only while that chunk is laid out |
| `ChunkLayout` | 20-60 KB | pages, line entries, XPointers, anchor map |
| Active DOM | one chunk | dropped before the next chunk is built |
| Resource cache | 4 MB total | byte-bounded LRU; generated resources are pinned |
| Page bitmap | ~0.75 MB | reused between renders, two pages cached |

The line table is the largest part of the layout index: a line entry is a node
id, a byte offset and a page number (16 bytes on 32-bit, 24 on 64-bit), and a
page keeps 19-27 of them.
A 24.9 MB Chinese book with 401 chunks and 297 455 lines keeps about 8.5 MB of
layout metadata in total, i.e. ~21 KB per chunk. That is why the *retained*
layout is small while a chunk's *transient* DOM is 10+ MB.

A page-start XPointer is one short heap string per page, and it used to exist
three times: in the chunk layout, copied into the flattened page list, and
again as the reverse map's key. The flattened list no longer copies it
(`page_xpointer` reads the chunk layout), which drops ~1.1 MB on the 24 MB
book (15 557 pages). The reverse map still needs its own key for a constant
time `page_from_xpointer`; dropping it is measured as a separate step.

## Compressed chunk text

The generated HTML of every chunk is kept for the session so a settings change
does not read and split the book again. Each chunk is stored as its own **LZ4
block** and decompressed only when it is laid out: opening, a TOC jump straight
into a chunk and a relayout all decompress exactly one chunk.

### Generation streams into the compressor

The EPUB generator decodes one spine document at a time and the chunker emits
each finished chunk to the compressor; only the current document and the chunk
being assembled exist as plaintext. Before this, `spine_documents()` held every
body, the splitter kept `Cow::Owned` copies, and the whole chunk vector was
returned before compression: 132 MB peak on the 24 MB book at a 38 MB budget.
Streamed, the same book opens with a **64 MB peak** (63 MB steady, 401 chunks,
15 557 pages, the same LZ4 blocks and line table). A single spine document
larger than a chunk is still split as a whole (`split_body_html` returns its
parts), so that case peaks at one document plus its parts, not at the whole
book.

Measured on real books, LZ4 gives 0.54-0.57 of the HTML size (CJK and Latin
alike). The synthetic test book repeats one paragraph and compresses much
better, so do not use it as a ratio.

A block that fails to decompress is an explicit error, never an empty chunk: a
blank book is worse than a diagnosable failure.

Chunk strings are built by appending; streaming keeps only one at a time and
takes the LZ4 block before dropping the plaintext. The earlier 80.7 -> 66.1 MB
change was shrinking the whole chunk vector before it was compressed; that
vector no longer exists on the EPUB path.

### MOBI, AZW3/KF8 and FB2

These formats are generated as one document instead of a spine, and they used
to read the whole file, copy every image, build the whole HTML and then split
it, with all of those alive at once.

- **The PalmDB is mapped, not read.** `Pdb::open` memory-maps the file instead
  of `fs::read`, so only the records the parser touches are faulted in. A
  43 MB AZW3 holds 36 MB of `CRES` image records that this engine does not use;
  they never enter the process's RSS, and the pages that are touched are file
  backed and reclaimable. This cut the open peak at a 38 MB budget from
  **60.8 MB to 34.4 MB**; a 23 MB AZW3 went from 43.0 MB to 37.2 MB.
  `Pdb::from_bytes` keeps the owned-bytes path for callers that already have a
  `Vec<u8>`.
- **Images are ranges, not copies.** `BookResources` keeps the file bytes and
  one `(offset, len)` per image record; `get` slices the mapping on demand.
  The map used to hold a second, anonymous copy of every picture (6.8-9.2 MB
  on these AZW3s, and the whole of a comic): at a 38 MB budget the steady
  document dropped from **31.7 MB to 25.0 MB**, with the peak unchanged. After
  the parse and after the open-time layout pass, `MADV_DONTNEED` drops the
  pages the process faulted in, so the pictures live in the page cache (which
  the kernel reclaims) and the first page that paints one faults it back in.
  `MADV_RANDOM` stops readahead/fault-around from dragging neighbouring
  pictures in: without it the mapping held 25 MB resident for 7 MB of images.
- **Chunks stream out of the generator.** `to_body_html_with_toc` /
  `html_head` / `body_html` give cre-lib the document head and body separately,
  and `stream_single_document_chunks` emits one chunk at a time (the same
  `<section>` packing and budgets as `split_single_document`). The whole
  generated document, its lowercased copy and the split parts no longer exist
  together; each chunk is compressed as soon as it is built, like EPUB.
- The cover is the one image copied at open (the reader UI asks for it as
  bytes), and the parsed book is dropped before the layout starts.

A raster `<img>` is still served to Blitz as a transparent SVG placeholder, so
laying a chunk out reads only the picture's header; the pixels are decoded with
DCT scaling when a page is painted, from a transient copy of the record. The
low-memory estimate counts no decoded image bytes for them
(`MAP_CHUNK_IMAGE_BYTES`).

## The open-time layout pass

Every chunk has to be laid out once to learn the page count. The pass:

1. lays out a **probe** chunk first: the one with the most embedded images,
   then the largest HTML. It has to be laid out anyway, and its resident cost
   is measured for the worker count;
2. starts `(available - reserve - RSS) / cost` workers, capped at the core
   count and at four, with a gate that stops a worker from starting a chunk
   while the resident set is above the plan;
3. keeps the opening chunk (chunk 0) for last, so its DOM is not held next to
   the workers' DOMs;
4. returns the workers' arenas to the OS (`malloc_trim`) **before** the opening
   chunk is laid out.

### A settings change lays the rest out in the background

Font, line-spacing and margin changes only need a relayout, not a re-read: the
chunk HTML does not embed those settings. The chunk under the reader is laid
out immediately, so the page is usable at once; a worker thread then lays out
the remaining chunks **in reading order** and hands each `ChunkLayout` back
through a channel. The reader's thread installs what is ready at the start of
the next page draw, keeps the reading position on the same `(chunk, local
page)`, and rebuilds the page table, so the page index, the TOC page numbers
and the total converge to the real layout.

**Every format qualifies**, not only EPUB. The layout settings a chunk needs are
never written into it: the generators emit `cre-css` tokens (`FONT_SIZE`, and
`PAGE_MARGIN` for the standalone-HTML path) and `layout_chunk_html` fills them in
with the settings in use. A new format gets the relayout path by following that
one rule, and `can_relayout_with` no longer looks at the format at all. Only a
change of *page size* forces a reopen, because image normalisation and the
page-size CSS are baked into the chunks.

Page numbers are a prefix sum of chunk pages, so the chunks must be done from
the first one; a result installed for an earlier chunk shifts the numbers after
it. KOReader notices the changed `getPartialRerenderingsCount()` after a draw
and puts the reader back on its XPointer (`handlePartialRerendering`), so the
content does not jump even though the numbers move. The worker holds its own
`Arc`s (sources, fonts, net, resources) and builds its own Blitz documents;
`Document` itself is not `Send`. A generation counter stops the worker on the
next chunk when the settings change again, and `Drop` stops it on close.

Measured on x86_64 release, `relayout` from 26 px to 30 px (all chunks
converge to exactly the page count and page hashes of a fresh open):

| Book | Chunks | Partial (current chunk only) | Converged | Slowest install |
|---|---|---|---|---|
| 21.epub (1.7 MB HTML) | 18 | 637 -> 779 pages | 801 pages in 260 ms | 0.8 ms |
| 24 MB text book | 268 | 16 719 -> 18 413 pages | 18 639 pages in 5.3 s | 5.3 ms |

The install cost is one `rebuild_view` per draw (the page table and the TOC
are rebuilt); it stays under 6 ms even for 18 000 pages. On a device the worker
takes the same time a full layout would (15-30 s for a 17 MB book), spread over
the reading session instead of blocking it.

### Telling KOReader the pass is over

A page total that changes without a full rerendering is invisible to the Reader
modules: `ReaderFooter` caches `self.pages` and only re-reads it on
`DocumentRerendered` (`TocReset`), and the TOC caches its page numbers the same
way. crengine gets that event from its background subprocess, which reloads the
document; we lay the chunks out in this process, so nothing signals the end.

The immediate total after `relayout` is also only an estimate: the chunks that
keep their old pages are scaled by the ratio the current chunk saw. When the
chunk under the reader keeps its page count (a part-title image page does, at
any font size) the ratio is 1.0 and the estimate is exactly the old total, so
the reader shows no change at all until the chunks are really laid out.

Two small pieces close this:

- `hasPendingRerendering()` (cre-lua) answers whether the worker still has
  chunks to lay out. cre.cpp has no equivalent, hence the extra entry —
  crengine's subprocess makes the question unnecessary.
- `tools/koreader-plugins/crebackgroundlayout.koplugin` uses it from a KOReader
  plugin (no frontend patch, so upstream updates keep working): it re-reads the
  page total on every `PageUpdate`/`PosUpdate` and sends `TocReset` when it
  moved, which is what makes `ReaderFooter` re-read it; and while that flag is
  true it polls once a second (reading the page count installs the ready chunks,
  so the pass also advances while the reader is idle) and sends one
  `DocumentRerendered` when the pass is over, which is the event a full
  rerendering sends — the TOC cache, the progress markers and the book's saved
  page count are rebuilt once.

  Two details make it reliable rather than lucky. The watch is (re)armed on
  every `onReaderReady`, not once: a style change closes and reopens the
  document, and a hook that runs once would leave that pass unwatched (no final
  event, so `ReaderToc` keeps the page numbers it read at layout time). And
  `watch()` reads the page count *before* it trusts `hasPendingRerendering()`,
  because the engine defers a settings change to the next document call: at the
  moment `DocumentRerendered` arrives the worker may not have started, the flag
  reads false, and the whole pass would go unnoticed. `CREBG_LOG=1` logs each
  decision (`page total a -> b`, `pass started`, `pass finished`), which is what
  to look at when a total "did not change".

`hasPendingRerendering()` is reached through `document._document`, because
adding the method to the `CreDocument` wrapper would mean editing
`frontend/document/credocument.lua`, which is what the plugin avoids.

Measured in the x86 emulator (21.epub, font +8 px, background on):

| | Live total | Footer shows |
|---|---|---|
| right after the change (estimate) | 2007 | — |
| 2 of 18 chunks installed | 2003 | `1658 / 2003` |
| 14 of 18 | 1979 | `1659 / 1979` |
| converged | 1977 | `1660 / 1977`, one more `DocumentRerendered` |

Without the plugin the footer stays on 2003/2007 for the rest of the session,
and the TOC keeps the page numbers computed at layout time.

## Opening a big book: first chunk now, the rest in the background

The open-time pass lays out every chunk, because every page number is a prefix
sum over them. That is where most of "opening a book" goes on a large file:
1.7 MB of chunk HTML takes ~0.25 s on x86 and ~1.5 s on a PW2, 25 MB takes 6.1 s
and 25-30 s. Above `LAZY_OPEN_MIN_BYTES` (3 MB of chunk HTML) `open` now lays out
the **opening chunk only**, fills in a page estimate for the rest and hands them
to the same worker a settings change uses. The first page is on screen in ~70 ms
instead of 206 ms for 21.epub, and in ~400 ms instead of 5.6 s for a 24 MB book
(x86; the device divides the layout part by 4-6).

Measured on the device that motivated this: a 13.6 MB, 320-chunk EPUB (27 MB of
generated HTML) needed **1 min 40 s** to show its first page and now shows it in
about **2 s**; the background pass then finishes while the reader reads, and the
page count and TOC converge to exactly what the full pass produced. On x86 the
same book opens in 425 ms instead of 4 766 ms, and in QEMU-on-ARM in 3.4 s
instead of 50.2 s.

The decision is taken in `ensure_layout`, not in the constructor: the reader
parses on `loadDocument` and lays out on `renderDocument` (`cre-lua` opens with
`with_options_with_font_data_parsed`), so a decision made while parsing would
never run and every real open would fall back to the full pass. That was the
case in the first version of this change, and it looked exactly like "the lazy
open does not work on the device": 100 s to the first page and the exact page
count straight away. `deferred_open_takes_the_lazy_path_too` now covers the
deferred path.

The reading position does not depend on that pass:

- KOReader restores `last_xpointer`, and `go_to_xpointer` /
  `ensure_xpointer_layout` lay out exactly the chunk that XPointer points into,
  then position by XPointer. The content is right immediately; only the page
  number is an estimate.
- A TOC jump uses the entry's XPointer for the same reason. A **page-number**
  jump (Go to page, the progress bar) uses the estimate and lands near the
  target until the pass ends; after it, it is exact.
- Bookmarks, highlights and annotation restore all go through XPointers, so
  they land on the right text too.

Restoring a position in the *last* chunk does not make opening proportional to
where the reader stopped: the open lays out the first chunk, and the restore
lays out the one chunk the saved XPointer points into. Measured on x86_64
release, that second chunk costs 7.8 ms on the 24 MB book and 21.7 ms on
21.epub (the latter also rebuilds the page table and re-bases the reading
position), i.e. 0.1-0.3 s on a PW2, on top of an open that is already a fraction
of the old one. What *is* estimated when opening at the end is the page number
shown for that position, until the pass ends.

What holds this together (the invariants worth keeping):

- `ChunkEntry.layout` is the page table for the current settings; while it is
  missing the entry carries `provisional_pages`. `stale` means "the layout, if
  any, is not for the current settings" and is what the worker selects on. No
  other code branches on those states.
- `ensure_page_layout` (one page's chunk) and `ensure_xpointer_layout` (one
  XPointer's chunk) are the only places a chunk is laid out outside the open
  pass and the worker. Every API that needs real geometry goes through them.
- The chunk -> `DocFragment` map is read from the chunk markup
  (`ChunkSource::spine`), not from a layout, because an XPointer has to find its
  chunk before that chunk has ever been laid out. That replaced
  `ChunkLayout::spine_slots`, so the map has one source instead of two.
- Whole-book queries that cannot work from estimates call
  `ensure_layout_complete()`. Full-text search does (from cre-lua): skipping
  unlaid chunks would silently hide matches.

The estimate itself comes from the opening chunk's bytes-per-page scaled by each
chunk's HTML size (a pre-paginated chunk is one page). Measured: within 2% on
21.epub, within 8% on the 24 MB book.

Measured on x86_64 release, same fonts and settings:

| Book | Open before | Open now | Converged |
|---|---|---|---|
| 21.epub (1.7 MB, 18 chunks) | 206 ms | 70 ms (forced) | 637 pages in 305 ms |
| 24 MB text (25 MB, 268 chunks) | 5 627 ms | 396 ms | 16 719 pages in 5.6 s |

The converged page count and the page hashes of the first, middle and last page
are identical to a full open (checked on both books, and by the
`lazy_open_converges_to_the_full_layout` test). By default 21.epub stays on the
exact path; `CRE_LAZY_OPEN=1`/`0` forces either path for A/B runs, and
`Document::with_options_lazy` / `with_options_exact` do it from the API.

The costs, and why they stay contained:

- Page numbers, the progress percentage and the library's page count are an
  estimate for the length of the pass. This is the same degraded state
  crengine's partial rerendering has, and the plugin sends one
  `DocumentRerendered` when the pass ends, so the footer, the TOC and the saved
  `doc_pages` become exact then.
- A TOC entry in a chunk that is not laid out yet points at that chunk's first
  page (a stand-in, better than a page number of 0); the jump still lands in the
  right chunk, and after the pass the entry has its exact page and XPointer.
- The worker competes with page turns for the two cores, exactly as a settings
  change does today.
- Memory does not grow: the open path holds one DOM at a time instead of one per
  worker, and only page metadata is kept per chunk.


Measured on x86_64 release (interleaved runs):

| Book | Open before | Open after | Peak before | Peak after |
|---|---|---|---|---|
| `jk.epub` (70 chunks) | 1500 ms | 645 ms | 54 MB | 95 MB |
| `12.epub` (image pages) | 480 ms | 415 ms | 178 MB | 228 MB |
| `juliet.epub` | 202 ms | 202 ms | 46 MB | 57 MB |

Parallel layout raises the peak, which is the wrong trade on a small device:
`CRE_AVAILABLE_MB=128` gives `jk.epub` two workers (1532 -> 948 ms) and keeps
`12.epub` on one worker, and the memory gate can always fall back to one.

## Trimming

Blitz and Parley allocate and free tens of megabytes per chunk. glibc keeps
those arenas, so the process RSS stays high even when the data is gone.

- `ensure_chunk` drops the old chunk (DOM, line boxes, cached page bitmaps)
  before building the next one, instead of holding two layouts;
- `malloc_trim(0)` runs after a chunk activation and after the open pass;
- it is skipped when the heap has not grown, so page turns do not pay for it;
- `CRE_NO_TRIM=1` disables it for A/B measurements.

Measured on a 738 kB book split into three 256 kB chunks, jumping across
chunks: peak 63.8 -> 46.4 MB. On `zyj.epub`: open RSS 42.5 -> 36.4 MB, peak
73.5 -> 42.4 MB.

## Refusing a book that cannot fit

### Exact check

After compression, the engine knows the exact stored size and estimates:

```text
need = largest chunk x 64             (the one DOM that exists at a time)
     + decoded images of the heaviest chunk
     + pages x 700                    (line table, page metadata, XPointer)
     + reserve
```

The 64 B/HTML byte is the measured cost of a *chunk-sized* layout (~50 B/B,
rounded up); the 128 B/B figure is the whole-book measurement and is only used
to size chunks, where being conservative is free.

The LZ4 text is **not** in `need`: the chunks were compressed before this check,
so their blocks are already resident and already missing from `available`.
Counting them again made a book that needed 30 MB look like it needed 44 MB.

`reserve` scales with the device: `clamp(available / 8, 8 MB, 24 MB)`. The old
fixed 24 MB was most of a 38 MB budget, so a book that fit was refused.

`pages` is estimated from the generated HTML at 2000 bytes per page, the middle
of the measured 1494 (Latin plain text) to 3030 (tightly set Chinese) range.

The page shows every term, the compressed size and the estimated page count,
so a refusal is explainable rather than a generic "out of memory".

### Early estimate

Waiting for a book to be decompressed, converted, split and compressed before
showing the page is unfriendly: on the 24 MB book it took 6 seconds and a
132 MB peak. The EPUB **zip directory already has the uncompressed size of
every XHTML in the spine**, so the same model is evaluated before anything is
read:

```text
generated HTML = XHTML x 1.10        (measured 1.04)
LZ4 text       = HTML x 0.62         (measured 0.54-0.57)
```

Images are not counted, so this estimate is a lower bound. To avoid refusing a
book that would fit, the early check only fires when the estimate is more than
20% over the free memory; anything closer is measured exactly after
compression. It uses the same 64 B/HTML byte chunk cost and the same scaled
reserve as the exact check (the compressed text counts here, because nothing is
allocated yet).

Measured on the 24 MB book with `CRE_AVAILABLE_MB=22`: refusal in **under
10 ms** with 8.4 MB max RSS, instead of 6.0 s and 132 MB. A normal `15.epub`
open is unchanged at 123 ms.

## Measurements

| Change | Before | After |
|---|---|---|
| `15.epub` engine RSS, open / peak | 105 / 127 MB | 33 / 45 MB |
| `15.epub` reader process, open / peak | 153 / 169 MB | 102 / 119 MB |
| 24 MB text book RSS (LZ4) | 66.1 MB | 41.5 MB |
| 24 MB text book, 38 MB budget, open peak | 132 MB | 64 MB |
| 24 MB text book RSS after open (one XPointer per page) | 63.8 MB | 62.0 MB |
| 43 MB AZW3, 38 MB budget, open peak | 60.8 MB | 34.1 MB |
| 43 MB AZW3, 38 MB budget, steady | 31.7 MB | 25.0 MB |
| 23 MB AZW3, 38 MB budget, open peak | 43.0 MB | 37.0 MB |
| `zyj.epub` open / peak (arena trim) | 42.5 / 73.5 MB | 36.4 / 42.4 MB |
| 8 full-page JPEGs, 64 MB budget, peak | 126 MB | 56 MB |
| 60-image book, RSS after open (LRU) | 40.1 MB | 35.6 MB |
| 24 MB book at 22 MB free, time to refuse | 6.0 s | < 10 ms |
| 43 MB AZW3, smallest free memory it opens at (open peak) | 36 MB (33.4 MB) | 20 MB (32.5 MB) |
| 24 MB text book, smallest free memory it opens at (open peak) | 62 MB (65.1 MB) | 32 MB (58.8 MB) |

Open time: compression costs about 60 ms, under 1% of a 6.4 s open pass, and
nothing to a page turn (a chunk is decompressed once when it becomes active).

## Trade-offs

**Pros**

- Peak memory scales with one chunk, not with the book, so large text books
  open on a device that has very little free memory left.
- Only one chunk is decompressed and laid out at a time; a TOC jump does not
  touch the rest of the book.
- The estimates are measured, explainable and fast: a book that cannot fit is
  refused before it is parsed.
- Layout and pagination stay identical to a whole-book layout whenever the
  book fits, so splitting only changes behaviour when it is necessary.

**Cons**

- A chunk's DOM costs ~128 bytes per HTML byte (10-14 MB for a 128 KB chunk),
  far above crengine's 8-byte node handles plus paged storage.
- Blitz has no DOM serialization, so there is no disk cache or swap. A reopen
  repeats the layout, and below roughly 30-40 MB free this engine cannot open
  a book that crengine with `cre_storage_size_factor=1` can.
- The 24 MB reserve the *chunk planner* uses is fixed; the low-memory check
  scales it (8-24 MB), so a book that fits is no longer refused, but chunk
  sizing at very low free memory is still conservative.
- Splitting starts a new page at every chunk boundary, so pagination can
  differ from crengine when a book does not fit whole.
- The page index is kept in RAM for the session; only the chunk text is
  compressed.

## Knobs

| Variable | Effect |
|---|---|
| `CRE_AVAILABLE_MB=N` | Pretend the device has N MB free |
| `CRE_IGNORE_LOW_MEMORY=1` | Skip the refusal (measure a book that would not fit) |
| `CRE_FORCE_LOW_MEMORY=1` | Show the diagnostic page |
| `CRE_LAYOUT_PROFILE=1` | Time chunks, compression, trimming and workers |
| `CRE_LAYOUT_WORKERS=N` | Force the worker count |
| `CRE_NO_TRIM=1` | Disable `malloc_trim` |
| `CRE_RESOURCE_CACHE_KB=N` | Resource cache size |
| `CRE_NO_SCALED_DECODE=1` | Decode images at full size |

## Known limits

- No on-disk cache: the generated HTML, the page index and the layout are
  rebuilt on every open.
- A device with 22 MB free still refuses a book that needs more than the 8 MB
  reserve plus its chunks; small books open, big ones are refused with the
  explanation page.
- Vertical writing and hyphenation do not exist yet, and both would change the
  layout cost model.

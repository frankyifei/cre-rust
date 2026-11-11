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
need = LZ4 text
     + largest chunk x 128            (the one DOM that exists at a time)
     + decoded images of the heaviest chunk
     + pages x 700                    (line table, page metadata, XPointer)
     + 24 MB                          (reserved for KOReader after the open)
```

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
compression.

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
| `zyj.epub` open / peak (arena trim) | 42.5 / 73.5 MB | 36.4 / 42.4 MB |
| 8 full-page JPEGs, 64 MB budget, peak | 126 MB | 56 MB |
| 60-image book, RSS after open (LRU) | 40.1 MB | 35.6 MB |
| 24 MB book at 22 MB free, time to refuse | 6.0 s | < 10 ms |

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
- The 24 MB reserve is fixed: with less free memory than that, every book is
  refused, including small ones. Scaling the reserve with the available memory
  is still open work.
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
- A device with 22 MB free is below the reserve, so the engine refuses every
  book there by design right now.
- Vertical writing and hyphenation do not exist yet, and both would change the
  layout cost model.

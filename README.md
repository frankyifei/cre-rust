# cre-rust

**cre-rust is a ebook reading library in Rust, built on Blitz. It is compatible
to Koreader**

It implements KOReader's `cre` module (`libkoreader-cre.so`) API on
top of the [Blitz](https://github.com/DioxusLabs/blitz) HTML/CSS engine —
Stylo for CSS, Taffy for boxes, Parley for text — instead of the crengine C++
engine. KOReader keeps its Lua API and does not change: drop the resulting
module into `koreader/libs/` and the reader opens EPUB, FB2, MOBI, AZW, HTML and
plain text exactly as before.

cre-rust learns from crengine: its page model, XPointer format, pagination
rules and rendering quirks guided this implementation. The layout model, the
pagination, the painter and the MathML translator are new Rust code written by
DeepSeek V4.1 Flash against existing libraries.

## Status

Tested with KOReader v2026.07.1:

- on a Kindle Paperwhite 2 target (ARMv7, soft-float ABI, glibc 2.12),
- in the QEMU harness in this repository,
- with an x86_64 desktop KOReader build.

The module exposes about 124 document methods and 24 module functions, and the
workspace has 36 test targets.

## Performance highlights

Opening a book used to cost one layout per chunk, because every page number is a
prefix sum over them. On the 13.6 MB, 320-chunk EPUB below that meant **1 min 40 s
on a Kindle PW2** before the first page appeared. The reader now lays out the
opening chunk, draws the first page, and lays the rest out in reading order on a
background worker.

Measured with the same build, the same books, and only the lazy open switched
(`CRE_LAZY_OPEN=0` / `=1`). The x86_64 column is a native build; the ARM column
is the ARMv7 e-ink build under `qemu-user` (`-cpu cortex-a9`):

| Book | Full layout at open | First chunk now | Speed-up | Pages exactly settled |
|---|---|---|---|---|
| 1.7 MB-HTML EPUB (18 chunks), x86_64 | 293 ms | 72 ms | 4.1x | 637 in 0.30 s |
| 13.6 MB / 320 chunks, x86_64 | 4 103 ms | 440 ms | 9.3x | 8 310 in 4.4 s |
| 13.6 MB / 320 chunks, ARMv7 (QEMU) | 43.9 s | 3.7 s | 11.8x | 8 310 in 46 s |
| 24 MB text book, x86_64 | 5 688 ms | 380 ms | 15x | 16 719 in 5.9 s |
| 24 MB text book, ARMv7 (QEMU) | 63.5 s | 3.9 s | 16.4x | 16 719 in 77 s |

The Kindle PW2 that motivated this timed **1 min 40 s** to the first page of the
13.6 MB book before the change; afterwards the same book opens in a few seconds
(observed, not timed to the second). Do not read the QEMU column as the device
number: on this book the device was *slower* than QEMU — 100 s for the full pass
where QEMU takes 44 s — so QEMU is a lower bound for a PW2, not a slower proxy.

The result is not an approximation: the converged page count and the page hashes
are identical to a full layout, on both the 1.7 MB and the 24 MB book. The
reading position is restored by XPointer, so it is exact from the first draw; a
TOC or bookmark jump into a chunk that has not been laid out yet costs one chunk,
not the book. The same machinery makes a font, line-spacing or margin change a
relayout for **every format** (the layout settings are kept out of the chunk
HTML as `cre-css` tokens), instead of reopening and re-parsing the book.

**There is no disk cache.** Every open re-reads the container, converts the
markup and re-chunks the book; the numbers above are that path, made cheap by
laying out only the first chunk. crengine instead keeps a rendered copy on disk
and reloads it, which would make a *second* open of the same book nearly
instant, including for the books that are still expensive here (a 320-chunk
conversion is the floor of the 3.7 s above). Building that cache is a future
option we are deliberately not spending time on yet: the current goal is to make
the open fast without one. Any disk-cache numbers would be a different table.

Details and measurements: [Chunked layout](#chunked-layout-with-partial-relayout),
[Opening a big book](#opening-a-big-book), [docs/MEMORY.md](docs/MEMORY.md).

## Highlights

The interesting work is what Blitz does not provide, or provides too
expensively for a 256 MB e-ink reader. These pieces are ours.

### MathML rendering (`cre-mathml`)

Blitz has no MathML support. `cre-mathml` translates a MathML presentation
subset into plain HTML boxes that the normal layout engine understands:
tokens, `mrow`, `mfenced`, scripts, `mfrac`, `msqrt`/`mroot`, `mtable`,
`mspace`, `mpadded`, `mstyle`, `merror`, `mphantom`, `menclose`,
`mmultiscripts` and `maction`.

This follows the approach of crengine's `MathMLHelper`: turn MathML into
ordinary HTML instead of adding a second math layout engine. We learned the
strategy from crengine, which does the rewrite inside its DOM builder and
stretches operators during line layout; here the rewrite happens before Blitz
parses the chunk, and the measuring pass, the drawn radicals and the OpenType
MATH constants are our own work.

The parts we spent the most time on:

- radical signs are **drawn**, not scaled glyphs: the vinculum is its own
  element that starts exactly where the surd's tip ends, and the surd is two
  thin strokes, so the sign keeps its stroke weight at any height and works
  with any font;
- stretched fences read the **glyph's ink height from the font's MATH table**
  and grow their box to the content height, so a parenthesis, brace or matrix
  bar always covers what it encloses;
- `movablelimits` and `displaystyle` are honoured: `∑`/`∏` stack their limits
  in display style and move them to the side inline, integrals never stack, and
  `mstyle displaystyle`/`scriptlevel` work per formula (as the W3C test suite
  relies on);
- scripts climb to the top or foot of a tall base (a stretched parenthesis, a
  fraction), instead of hanging around the middle;
- empty script slots (`<mrow/>`) are ignored, so `a_k` written as
  `<msubsup>a k <mrow/></msubsup>` does not push its subscript onto the axis;
- `maction` renders only the selected child, so tooltip text from the test
  suites stays out of the formula;
- `mathcolor`, `mathbackground`, `mathsize` and `mathvariant` become CSS on the
  generated boxes, and the author's `class`/`style` is kept, so a book's MathML
  CSS applies (the W3C `mathml-020`/`021` styling tests);
- `mtable` draws its `frame`, `rowlines` and `columnlines` and applies
  `rowalign`/`columnalign`; the rules between cells are 1px gradients because
  Blitz does not paint a border on only one side of a shared cell edge;
- `mpadded` honours its incremental sizes;
- OpenType MATH constants (script scale, shifts, rule thickness, axis height)
  are read from the selected math font; KOReader's `font-family: math` choice
  is honoured, and the math font survives the "ignore publisher fonts" tweak.

### Painting (`cre-paint`)

Rendering is our own scene rasterizer on `tiny-skia`, with an optional
`vello_cpu` backend:

- glyph runs with **arbitrary affine transforms**: stretched fences, synthetic
  oblique, skew and rotated text are resampled per glyph instead of being
  rasterized at their largest axis scale (which used to thicken strokes);
- layers with opacity, blend modes, `filter: blur()`, `filter: drop-shadow()`
  and color-matrix filters, plus backdrop filters;
- `box-shadow` (outset and inset), border radius, gradients, image brushes and
  clip layers;
- grayscale and color page bitmaps with a font-contrast gamma curve for e-ink;
- bounded caches for decoded resources and page images.

### Pagination that understands the content

Pages are built from line boxes and content boxes rather than by slicing a tall
canvas:

- forced breaks (chapters), margins, page flows and page maps;
- images and display formulas are kept whole: an image or a formula that does
  not fit moves to the next page instead of being cut;
- inline formulas grow their line, so a line holding a tall formula moves as a
  unit;
- page starts are recorded so page ↔ XPointer conversion and the header/footer
  flow data are exact.

### Positions, selection and search

- XPointer V2 strings in both directions, page ↔ XPointer, simplified CFI;
- word and sentence lookup at a screen position, including CJK;
- highlight and selection rectangles per line segment, with the source block
  and byte offset tracked through the chunked layout;
- full-text search that returns per-page rectangles.

### Chunked layout with partial relayout

A book is laid out in chunks sized from a memory budget, one DOM at a time.
Changing a font, margin or font-family lays out the current chunk immediately,
then finishes the rest on a background worker in reading order, so the page
index, the page total and the TOC page numbers converge to the real layout
without blocking the reader. This holds for every format: the chunk HTML never
contains the layout settings (they are `cre-css` tokens filled in when a chunk is
laid out), so a settings change re-lays out the chunks a book already has instead
of reopening it. Measured on ARMv7 under `qemu-user` (the e-ink build):

| Book | Full relayout | Partial relayout |
|---|---|---|
| a 0.8 MB novel (a few chunks) | 4.1 s | 0.47 s |
| a large text book (many chunks) | 7.3 s | 0.71 s |

On x86 a 24 MB text book converges in 5.3 s while the reader keeps turning
pages (the install costs 5 ms per draw), and a 1.7 MB book in 260 ms; the page
count and page hashes end up identical to a fresh open.

### Opening a big book

The same worker opens the book: above 3 MB of chunk HTML, `open` lays out the
first chunk only, so the first page is on screen in a fraction of the time while
the rest is laid out in reading order. The reading position is restored by
XPointer, which lays out exactly the chunk it points into, so the content is
right immediately; page numbers settle when the pass ends. Small books (below
the threshold) keep the full pass, which costs about a second and gives exact
numbers from the first draw; `CRE_LAZY_OPEN=1`/`0` forces either path.

| Book | Full layout at open | First chunk now | Pages exactly settled |
|---|---|---|---|
| 1.7 MB-HTML EPUB (18 chunks), x86_64 native | 293 ms | 72 ms | 637 in 0.30 s |
| 13.6 MB / 27 MB HTML / 320 chunks, x86_64 native | 4 103 ms | 440 ms | 8 310 in 4.4 s |
| the same book, ARMv7 under qemu-user | 43.9 s | 3.7 s | 8 310 in 46 s |
| 24 MB text book, x86_64 native | 5 688 ms | 380 ms | 16 719 in 5.9 s |
| 24 MB text book, ARMv7 under qemu-user | 63.5 s | 3.9 s | 16 719 in 77 s |

The 13.6 MB book was also measured on a Kindle PW2: 1 min 40 s to the first page
before the change, a few seconds after it (observed, not timed to the second).
The device is not faster than QEMU on this workload — it took 100 s for the full
pass where QEMU takes 44 s — so the ARM column is a lower bound for the device.

The converged page count and page hashes are identical to a full open (there are
tests for that), and a TOC or bookmark jump into a chunk that has not been laid
out yet costs one chunk, not the book. Restoring a position at the very end of a
book does not cost more than restoring one at the start: the open lays out the
first chunk and the restore lays out the single chunk the XPointer names (+8 to
+22 ms on x86, proportionally more on a PW2). Page-number jumps land near the
target until the pass ends; XPointer jumps are exact from the first draw.

Until it ends, page numbers, the progress percentage and the TOC page numbers
are estimates — the same degraded state crengine's partial rerendering has.
KOReader is told when it ends (see `docs/MEMORY.md`), which is what refreshes
the footer total and the TOC.

### Keeping page numbers in sync (`crebackgroundlayout.koplugin`)

The engine cannot push an event into KOReader: the `cre` API is a set of
questions the reader asks. The reader caches the answers — `ReaderFooter` keeps
the page total until a `TocReset`, `ReaderToc` keeps the page numbers it read at
layout time until a `DocumentRerendered` — so a background pass needs someone to
tell the reader when the numbers moved and when they are final. crengine gets
that from reloading the document after its background subprocess; this engine has
nothing to reload, so the frontend half is a small plugin instead of a patch:
KOReader can keep following upstream.

```sh
cp -r tools/koreader-plugins/crebackgroundlayout.koplugin <koreader>/plugins/
```

While a pass runs it re-reads the page total once a second (which also advances
the pass, so an idle reader still converges) and refreshes the footer; when the
pass ends it sends one `DocumentRerendered`, which drops the TOC cache so the TOC
opens with the final page numbers. It is a plugin because that is the only way to
do it without editing KOReader, and it only uses the public events
(`TocReset`, `DocumentRerendered`) that KOReader already listens to.

[docs/PAGE-NUMBERS.md](docs/PAGE-NUMBERS.md) walks through the mechanism, the two
reliability rules (re-arm on every reader-ready, because a style change reloads
the document; read the page count before trusting the pending flag) and the
diagnostics (`CREBG_LOG=1`).

### Memory: one chunk, kept compressed

A big text book keeps the generated HTML of every chunk so a settings change
does not have to read and split it again. That copy is stored as **one LZ4
block per chunk** and decompressed only when that chunk is laid out — opening,
a TOC jump into another part, and a relayout each decompress one chunk, never
the whole book. Real prose compresses to about 55%, so a book that would keep
25 MB of text resident keeps ~14 MB.

The EPUB generator streams the chunks: it decodes one spine document at a
time and hands each finished chunk to the compressor as soon as it is built,
so the whole book's chunk HTML is never materialized. On a 24 MB text book at
a 38 MB budget this cut the open peak from 132 MB to 64 MB (the steady
document stayed at 63 MB), with the same chunk split and page count.

MOBI, AZW3/KF8 and FB2 get the same treatment: the PalmDB is memory-mapped so
only the records the parser reads are faulted in (an AZW3's unused compressed
image records never enter RSS), images are indexed by record range instead of
copied into a map, and the generator hands out one chunk at a time instead of
building the whole document and then splitting it. A 43 MB AZW3 opened at a
38 MB budget peaked at 34 MB instead of 61 MB and settled at 25 MB instead of
32 MB. See [docs/MEMORY.md](docs/MEMORY.md).

Blitz decodes every image of a chunk while it parses it. A raster `<img>` is
therefore served to Blitz as a transparent SVG that carries the real intrinsic
size, and the real pixels are painted by `cre-paint` at the size they are
actually drawn; JPEGs are decoded with DCT scaling at that size. A full-page
picture goes through a page-fit wrapper that paints the original with
`contain`. Layout never holds the decoded pixels. See
[docs/IMAGES.md](docs/IMAGES.md).

Other bounds, all measured:

- the resource cache is a 4 MB LRU, with engine-generated resources pinned;
- a chunk's budget covers its text DOM **and** its decoded images;
- the open-time layout pass runs chunks on several cores when memory allows,
  but the worker count comes from the measured cost of one chunk, and the
  opening chunk is laid out last;
- the workers' arenas are released with `malloc_trim` before the opening chunk
  is laid out, and trimming only runs when the heap grew.

A book that still cannot fit is refused with a page that explains the
shortfall. The estimate counts one chunk's layout (at the measured ~64 B per
HTML byte, not the whole-book 128), the heaviest chunk's images and the page
index, plus a reserve that scales with the device. The compressed text is not
counted: it is already resident when the check runs. The estimate is computed
from the XHTML size in the EPUB zip **before** the book is read, so a refusal
appears in milliseconds instead of after a full parse. See
[docs/MEMORY.md](docs/MEMORY.md).

### Paint speed on a single-core e-ink device

Text pages render 2-3x faster than the first version. Transparent fills are
dropped, the RGBA-to-gray pass runs in place, pixel-aligned solid fills use
`fill_rect`, glyph masks and the font collection are shared across pages, and
the page pixmap is reused between renders. Minified images are pre-scaled with
a parallel box filter and blitted 1:1; 32-bit ARM has a fixed-point bilinear
sampler for the remaining image fills, which closed most of the gap to the
`vello_cpu` backend. See [docs/PERFORMANCE.md](docs/PERFORMANCE.md).

### Running on the Kindle's glibc 2.12

The Paperwhite 2 ships glibc 2.12 and a soft-float ABI, while the prebuilt
Rust standard library targets glibc 2.17. The module is linked against the
device's 2.12 sysroot, keeps VFP3 instructions without leaving the softfp ABI,
and ships a small `getauxval` shim because Rust's thread setup calls it and
glibc only exported it from 2.16. The build script audits the result: the
highest required `GLIBC_*` symbol is 2.12, no non-libc symbol is left
undefined, and the Lua API symbols stay undefined for the host LuaJIT. Thin
LTO and stripping keep the module around 19.7 MB. See
[docs/KINDLE.md](docs/KINDLE.md).

### Fonts

- TTF, OTF, TTC and OTC files, with face selection inside a collection (Noto
  Serif CJK JP/KR/SC/TC/HK and similar);
- fonts embedded in the book (`@font-face`, TTF/OTF/WOFF/WOFF2) through the
  same resource provider, with the declared family, weight and style;
- the reader's **Embedded Fonts** toggle and the book's font list
  (`getEmbeddedFontList()`): the engine reports the families a book declares and
  can drop them at layout time, without reopening the book — on a 13 MB book
  with two embedded TTFs that is 80 MB → 34.6 MB of RSS and 707 → 679 pages.
  See [`docs/FONTS.md`](docs/FONTS.md);
- generic family lists, fallback chains, font bias, and the reader's
  `font-family: math` slot;
- bold and italic synthesis;
- CJK metrics and vertical punctuation data from the font.

### Formats

| Format | Notes |
|---|---|
| EPUB 2 and EPUB 3 | OPF, spine, TOC, page map, CSS, images, fonts, SVG |
| FB2 | Base64 binaries, inline images |
| TXT and HTML | Plain text, standalone HTML with local resources |
| MOBI 6 (`.mobi`, `.prc`, `.azw`) | PalmDOC decompression, EXTH, `filepos` links, images |
| KF8 / AZW3 | Separate crate, shared PalmDB container; fixed pages, embedded fonts, series |

The format is detected from the file content, not from the extension.

## Pros and cons

**Pros**

- A real CSS engine: Stylo, Taffy and Parley provide selectors, cascade,
  fonts, complex-script shaping and inline layout, so publisher CSS behaves
  like a browser instead of a hand-written subset.
- Memory scales with one chunk, not with the book: a 15-photo book went from
  105 MB to 33 MB of engine RSS at open, and a 24 MB text book from 66 MB to
  41 MB. Text layout is unchanged; image pages keep their ink within ~1%.
- Positions, selection, search and TOC are page-accurate, with rectangles per
  line segment; the XPointer model round-trips.
- Fast paint on e-ink, including an ARM-specific path, and a settings change
  that only re-lays out the current chunk.
- The engine is Rust throughout: no C++ toolchain and no unsafe layout code.
- The same code runs on x86 and ARMv7 (hard and soft float), with 34 test
  targets, a 51-book reading regression used during development, and a QEMU
  harness that drives the real KOReader.
- Runs on a stock Kindle Paperwhite 2: softfp ABI, VFP3 instructions, a
  glibc 2.12 symbol set and a `getauxval` shim, all audited by the build
  script (see [docs/KINDLE.md](docs/KINDLE.md)).

**Cons**

- The Blitz DOM costs about **128 bytes per byte of HTML**. One 128 KB chunk
  needs ~14 MB while it is laid out, so the floor is much higher than
  crengine's paged DOM, which keeps 8-byte node handles in RAM and pages the
  rest to disk.
- Blitz cannot serialize or restore a DOM, so there is no on-disk cache or
  swap. Opening a book again re-lays it out, and at very low free memory this
  engine cannot match crengine with a small `cre_storage_size_factor`.
- Page breaks can differ from crengine when a book has to be split into chunks;
  chunk boundaries always start a page.
- Engine features are missing: vertical writing, `text-shadow`, bidi
  (`direction`/`unicode-bidi`), hyphenation, and full `object-fit` in reflow.
- MathML is a presentation subset; MOBI/KF8 support is partial; EPUB CFI is
  simplified.
- The Lua API is not complete: some `cre.cpp` methods return fixed values (see
  [TODO.md](TODO.md)).
- A very large book still takes seconds to open on a Paperwhite 2; the
  low-memory check scales its reserve with the device (8-24 MB) instead of
  holding a fixed 24 MB, so a book that fits is not refused.
- Blitz does not support complex CJK layout yet, although there are recent pull 
  requests in upstream

## Limits

- Vertical writing (`writing-mode: vertical-rl`) is not supported. Blitz lays
  out horizontal text only.
- BiDi is not supported yet: `direction` and `unicode-bidi` are not honoured
  end-to-end and text is laid out left-to-right. This waits on upstream Blitz.
- Text hyphenation is not applied. The module stores the dictionary name only.
- `text-shadow` is not painted: Blitz does not parse it, so the paint stream
  carries no shadow for glyph runs. `box-shadow`, filters, `opacity` and
  `clip-path` (on block boxes) do work.
- MathML is a presentation subset. A matrix (`mtable`) is kept on one page
  rather than split between rows; huge formulas are clipped, as an inline
  table would be in crengine.
- MOBI and KF8: no DRM, and the CSS support is whatever the flow carries (both
  formats are laid out by the same engine, so a KF8 book gets the same
  typography, images, fixed pages and embedded fonts as an EPUB).
- EPUB CFI output is simplified.
- Two-page view, scroll view, and some page-flow APIs are partial.
- There is no on-disk cache. Opening a book again makes the pages again.
- Some cre.cpp APIs return fixed values. See [TODO.md](TODO.md).

`cre-book` is the format-independent layer below the readers: the resource table
(one namespace for zip entries, PalmDB records, files and generated resources),
chunking and its memory budgets, the HTML/CSS/image/font helpers every generator
needs. A new format implements its container plus a `ResourceProvider` and gets
the rest from there. See [`CRATES.md`](CRATES.md) for the layering and what a
format has to provide.

## Repository layout

| Path | Content |
|---|---|
| `crates/cre-lib` | Engine core: open, layout, pagination, page lookup, positions, images, fonts |
| `crates/cre-book` | Format-independent book layer: resource table, chunking, HTML helpers, page-fit plates, fonts, ruby |
| `crates/cre-epub` | EPUB container, OPF, spine, TOC, HTML chunks |
| `crates/cre-fb2` | FB2 reader |
| `crates/cre-text` | TXT and standalone HTML reader |
| `crates/cre-pdb` | PalmDB container, MOBI header, INDX index |
| `crates/cre-mobi` | MOBI 6 reader |
| `crates/cre-kf8` | KF8 / AZW3 reader |
| `crates/cre-mathml` | MathML to HTML translation and the math stylesheet |
| `crates/cre-paint` | Paint commands, page bitmaps, color bitmaps |
| `crates/cre-lua` | LuaJIT binding (`libkoreader-cre.so`) |
| `crates/cre-render` | Command-line renderer for tests and debugging |
| `scripts` | Host and Kindle build scripts |
| `tools` | QEMU harness and KOReader debug plugin |
| `fixtures` | Test books |

The detailed design is in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Requirements

### All builds

- Linux. WSL works.
- Rust 1.91 or later. The `rust-toolchain.toml` file selects version 1.99.1.
- `git` and `bash`.

### Desktop tests

- DejaVu fonts in `/usr/share/fonts/truetype/dejavu/`.
- Optional: a CJK font for the CJK tests.

### Kindle build

- The KOReader `koxtoolchain` for `arm-kindlepw2-linux-gnueabi`.
- See [koxtoolchain](https://github.com/koreader/koxtoolchain).
- Default install path: `~/x-tools/arm-kindlepw2-linux-gnueabi`.

### QEMU harness

- `qemu-arm` from `qemu-user`.
- An armel sysroot, for example `/usr/arm-linux-gnueabi`.
- `libx11-dev` and an X11 display or WSLg for the window.
- Network access for the first KOReader package download.

## Build for the desktop

Run the host build:

```sh
bash scripts/build-cre-lua-luajit-module.sh
```

The script writes `target/release/libkoreader-cre.so`.

To use it with a desktop KOReader, copy the file to `koreader/libs/`. Keep a
copy of the original module.

## Build for the Kindle Paperwhite 2

### About the target

The Paperwhite 2 has an i.MX6 SoloLite (Cortex-A9). KOReader and its libraries
use the **soft-float ABI**. A hard-float build does not load on the device.

The default build uses VFP3 instructions, but keeps the softfp ABI:

```text
-C target-cpu=cortex-a9 -C target-feature=+vfp3,-soft-float,+strict-align
```

`+strict-align` is necessary. VFP loads fail on unaligned addresses. The
softfp ABI guarantees only 8-byte alignment at public interfaces.

The prebuilt Rust standard library targets glibc 2.17, but the module is
linked against the toolchain's glibc 2.12 sysroot. The one link-time gap,
`getauxval` (Rust calls it during thread setup, and glibc only exports it from
2.16), is covered by the `old-glibc` cargo feature that the Kindle build
enables. The script then checks the highest required `GLIBC_*` version, the
`NEEDED` libraries and the undefined symbols; both the VFP and the
software-float builds stay at `GLIBC_2.4`/`GLIBC_2.12`. See
[docs/KINDLE.md](docs/KINDLE.md) for the full compatibility notes.

### Steps

1. Install koxtoolchain. Use the instructions in the
   [koxtoolchain repository](https://github.com/koreader/koxtoolchain).
2. Set the toolchain path if it is not the default:

   ```sh
   export X_TOOLS="$HOME/x-tools/arm-kindlepw2-linux-gnueabi"
   ```

3. Optional: load the environment helper:

   ```sh
   source scripts/kindle-toolchain-env.sh
   ```

4. Build the VFP module:

   ```sh
   bash scripts/build-cre-lua-luajit-module-arm-kindlepw2.sh
   ```

5. Build the software-float module if the VFP module is not stable on your
   device:

   ```sh
   CRE_NO_VFP=1 bash scripts/build-cre-lua-luajit-module-arm-kindlepw2.sh
   ```

6. The current device build enables NEON; it uses the same script with
   `CRE_NEON=1` and writes to a `-neon` target directory:

   ```sh
   CRE_NEON=1 bash scripts/build-cre-lua-luajit-module-arm-kindlepw2.sh
   ```

The output is:

```text
target-arm-kindlepw2-linux-gnueabi/armv7-unknown-linux-gnueabi/release/libkoreader-cre.so
```

The script prints the maximum GLIBC version. The value must be `GLIBC_2.12` or
lower for the Paperwhite 2.

Reference sizes:

| Variant | Size |
|---|---|
| VFP, strict align | about 19.7 MB |
| NEON (current device build) | about 19.8 MB |
| Software float | about 19 MB |

### Deploy to the device

1. Connect to the device with SSH or mount its storage.
2. Back up the original module:

   ```sh
   cp /mnt/us/koreader/libs/libkoreader-cre.so \
      /mnt/us/koreader/libs/libkoreader-cre.so.orig
   ```

3. Copy the new module:

   ```sh
   cp libkoreader-cre.so /mnt/us/koreader/libs/libkoreader-cre.so
   ```

4. Start KOReader. If KOReader does not start, restore the backup file.

## Test

Run all checks:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The test suite has 34 targets.

Some tests need the DejaVu fonts. Some tests use the books in
`fixtures/koreader-test-data/`. Install the fonts and keep the books before you
run the full suite. A missing resource stops the test that needs it.

### Render pages with the command-line tool

```sh
cargo run -p cre-render -- --toc fixtures/koreader-test-data/leaves.epub /tmp/pages 3 758 1024
```

The tool writes one PGM file for each page. Use ImageMagick or another viewer
to convert the files:

```sh
convert /tmp/pages/page-001.pgm /tmp/page-001.png
```

### Diagnostic environment variables

| Variable | Effect |
|---|---|
| `CRE_AVAILABLE_MB=N` | Pretend the device has N MB free (chunk planner and low-memory gate) |
| `CRE_IGNORE_LOW_MEMORY=1` | Open a book the device would refuse, to measure it |
| `CRE_FORCE_LOW_MEMORY=1` | Show the low-memory diagnostic page |
| `CRE_LAYOUT_PROFILE=1` | Time the open pass: chunk stages, LZ4 compression, trimming, workers |
| `CRE_LAYOUT_WORKERS=N` | Force the number of parallel chunk layouts |
| `CRE_NO_TRIM=1` | Do not return freed heap to the OS (A/B the trim cost) |
| `CRE_RENDER_PROFILE=1` | Split page render time into record / raster / convert |
| `CRE_NO_SCALED_DECODE=1` | Decode images at full size (A/B DCT scaling) |
| `CRE_RESOURCE_CACHE_KB=N` | Resource cache size in KB (default 4096) |
| `CRE_PAINT_BACKEND=tiny\|vello` | Painter backend in a build with `vello-cpu` |
| `CRE_LUA_TRACE_PATH=FILE` | Write a trace of the Lua API calls |

## Run KOReader with QEMU

The harness runs the genuine Kindle KOReader package for ARM under qemu-user.
It shows the e-ink framebuffer in an X11 window. Mouse clicks become touch
events.

### Quick start

```sh
tools/qemu-kindle/run.sh /path/to/book.epub
```

The first run downloads the KOReader package to `~/.cache/cre-qemu-kindle`.
Later runs use the cache.

Run without a window:

```sh
tools/qemu-kindle/run.sh --headless /path/to/book.epub
```

The run log is at `~/.cache/cre-qemu-kindle/reader.log`.

### What the harness does

1. Downloads and extracts the Kindle KOReader package.
2. Builds a small ARM preload shim for the device interfaces.
3. Builds the X11 viewer.
4. Builds this module and installs it as `libs/libkoreader-cre.so`.
5. Starts KOReader under `qemu-arm`.

The original module stays in the package as `libkoreader-cre.so.orig`.

### Configuration

Read [tools/qemu-kindle/README.md](tools/qemu-kindle/README.md) for all options.
The common options are:

| Option | Effect |
|---|---|
| `--no-viewer` | Run without the X11 window |
| `--cre-lib FILE` | Use a prebuilt module |
| `--config FILE` | Use a JSON configuration file |

## Debug plugin

`tools/qemu-kindle/plugin/qkinput.koplugin` has two jobs.

1. It sends viewer mouse events to KOReader as gestures.
2. It loads a Lua script when a book is ready. Use the script to drive KOReader
   without menu clicks.

Set `QK_SCRIPT` to a Lua file. The file must return a function. KOReader calls
the function with the reader UI.

Example: go to page 20 and log the page count.

```lua
local logger = require("logger")
local UIManager = require("ui/uimanager")
local Event = require("ui/event")

return function(ui)
    UIManager:scheduleIn(2, function()
        logger.info("debug: pages", ui.document:getPageCount())
        ui:handleEvent(Event:new("GotoPage", 20))
        logger.info("debug: page", ui.document:getCurrentPage())
    end)
end
```

Run it:

```sh
QK_SCRIPT="$PWD/tools/qemu-kindle/examples/goto-page.lua" \
    tools/qemu-kindle/run.sh fixtures/koreader-test-data/leaves.epub
```

The `logger.info` output goes to the run log. Use this hook for page turns,
TOC jumps, font changes, and timing.

## Project rules

- Keep the Lua API compatible with KOReader.
- Use Blitz, Parley, and other existing libraries before you write new code.
- Do not add a cache or an optimisation that hides a layout bug.
- Add a test for each fixed bug.

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE).

KOReader uses the same license.

## Credits

- [KOReader](https://github.com/koreader/koreader) and crengine.
- [Blitz](https://github.com/DioxusLabs/blitz), [Stylo](https://github.com/servo/stylo),
  [Taffy](https://github.com/DioxusLabs/taffy), and
  [Parley](https://github.com/linebender/parley).
- [koxtoolchain](https://github.com/koreader/koxtoolchain).
- The font, image, and SVG crates in `Cargo.toml`.

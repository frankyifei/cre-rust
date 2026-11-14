# Architecture

This document describes how cre-rust opens a book and makes a page. It also
lists the important design decisions.

## Goal

The goal is a drop-in replacement for the KOReader `cre` module
(`libkoreader-cre.so`). KOReader must not change. The module keeps the Lua API
and the side effects that KOReader expects.

The implementation is Rust code written on existing rendering libraries,
learning from crengine's design and behaviour.

## Non-goals

- A full crengine clone.
- DRM removal.
- JavaScript.
- A web browser.

## Pipeline

```text
book file
  -> format crate (cre-epub, cre-fb2, cre-mobi, cre-kf8, cre-text)
  -> HTML chunks (strings)
  -> Blitz HTML document (one chunk at a time)
  -> line boxes + content boxes
  -> page list
  -> paint commands
  -> grayscale page bitmap
  -> Lua API result for KOReader
```

A book is a sequence of chunks. A chunk is a part of one spine item, or one
spine item. The chunk size comes from a memory budget.

## Crates

| Crate | Job |
|---|---|
| `cre-book` | Format-independent book layer: resource table, chunking and budgets, layout tokens, HTML/CSS/image/font helpers, page-fit plates |
| `cre-epub` | ZIP container, OPF, spine, TOC, page map, CSS collection, image and SVG rewrite |
| `cre-fb2` | FB2 XML, base64 binaries |
| `cre-text` | TXT and standalone HTML |
| `cre-pdb` | PalmDB container, MOBI header, EXTH, INDX/TAGX/CNCX, decompression |
| `cre-mobi` | MOBI 6 links, images, chapter split |
| `cre-kf8` | KF8/AZW3 flows, CSS, links, images |
| `cre-lib` | The engine. Open, layout, paginate, page lookup, fonts, images, paint |
| `cre-mathml` | MathML to HTML translation and the generated math stylesheet |
| `cre-paint` | Scene to pixmap, page bitmaps, font gamma |
| `cre-lua` | LuaJIT C API, document userdata, module functions |
| `cre-render` | Small CLI that renders pages to PGM |

## The page model

A `Document` holds:

- `sources`: the HTML for each chunk.
- `entries`: the layout result for each chunk.
- `pages`: the page list. Each page knows its chunk and the document top.
- `xpointer_pages`: a map from XPointer string to page.
- A per-line table: block id, byte offset in the block, page.

The per-line table is the source of truth for page lookup. A text position is
converted to `(block, byte offset)` and found with a binary search. The table
is in document order. `TextPosition` order is not document order.

Block ids increase with document order. This makes `(block, offset)` a stable
key across chunks.

## Text positions and XPointers

The XPointer format follows crengine `toStringV2`:

```text
/body/DocFragment[15]/body/div[1]/p[3]/span[1]/text().42
```

`DocFragment[N]` is a **spine slot**. A split spine item uses a new slot, and
`data-spine-real-index` keeps the original EPUB index.

Line starts and line ends are converted to XPointers with block byte offsets.
For a line that holds several text nodes, a position is compared in block byte
space, not with `TextPosition` ordering. This is necessary for headings such as
`<h1><span>Chapter</span> One</h1>`.

A page with no text — a picture page, which is every page of a comic — has no
line to anchor to, so the position names the element instead:

```text
/body/DocFragment[3]/body/img[1]
```

That form round-trips like the text one (`page_from_xpointer` resolves it to the
page the element sits on). Without it, `getXPointer()` had nothing to return for
those pages, and the reader lost the last-read position, bookmarks and the
"go to page" dialog; KOReader also picks the XPointer branch over the page when
both are available, so a TOC entry with a page but no XPointer was a menu that
jumped nowhere.

## Layout and pagination

`cre-lib` builds one Blitz document for one chunk. It then collects:

- line boxes,
- content boxes (images, tables, block boxes),
- forced page breaks.

The paginator adds lines and boxes to pages until the page is full. A forced
break starts a new page. A chunk always starts on a new page.

The total page count is known after the first layout. KOReader needs the total
count right after `renderDocument`.

Two content kinds are kept whole even when they would straddle a page
boundary: images and display formulas. `cre-lib` collects their absolute rects
and hands them to the paginator as content boxes (inline formulas instead grow
their line), because Blitz measures a line from the text metrics and reports a
line holding a tall box as much shorter than it paints.

## MathML

Blitz has no MathML layout. `cre-mathml` rewrites each `<math>` subtree into
ordinary HTML for the presentation subset KOReader books use, and `cre-lib`
appends the generated stylesheet once per chunk:

- fences stretch by scaling the glyph, with the box sized to the content height
  and the scale divided by the glyph's ink height from the font's MATH table;
- radicals are drawn (two strokes plus a separate vinculum element), so they do
  not depend on a `√` glyph and keep a constant stroke weight;
- `movablelimits` plus the display style decide whether `msubsup`/`munderover`
  limits stack or stay at the side;
- `mstyle displaystyle` and `scriptlevel` are evaluated per subtree;
- `maction` renders only its selected child.

Presentation attributes (`mathcolor`, `mathbackground`, `mathsize`,
`mathvariant`) and author classes/styles are copied onto the generated HTML, so
book MathML CSS applies; `mpadded` honours incremental sizes. `mtable` keeps its
frame, row/column rules and alignment; rules between cells are drawn as 1px
gradients because Blitz does not paint a border on only one side of a shared
cell edge.

A formula stays inside one line. Display formulas are atomic for pagination; a
matrix is currently not split between rows.

The translate-to-HTML strategy is learned from crengine's `MathMLHelper`:
crengine hooks its DOM builder and stretches operators while laying out a line,
while `cre-mathml` runs before Blitz parses the chunk and gets the measured
content height back from the engine.

## Chunking and memory

The chunk size is a function of the available memory. `CRE_AVAILABLE_MB`
simulates a device for testing. The image payload of a chunk is also bounded,
because Blitz decodes images during layout.

The current chunk is the only resident layout. When the reader moves to
another chunk, the old layout is dropped and the new chunk is laid out. The
generated HTML of every chunk stays resident as one LZ4 block per chunk and is
decompressed when that chunk is laid out.

The EPUB generator streams: it decodes one spine document at a time and hands
each finished chunk to the compressor, so the whole book's chunk HTML is never
materialized before compression. MOBI, AZW3/KF8 and FB2 do the same from their
generated body, and their PalmDB stays mapped: images are indexed by record
range and sliced on demand, so the file's pictures cost page cache instead of
anonymous copies.

Resource caches are byte-bounded. Resources that the engine generates
(rewritten XHTML, page-fit SVG wrappers) are **pinned** for the session. They
have no zip entry to read again.

The cost model, the open-time worker policy, trimming, the low-memory refusal
and its measurements are in [MEMORY.md](MEMORY.md).

## Partial relayout

A settings change must not lay out the whole book.

`Document::relayout(options)`:

1. Marks the other chunks stale.
2. Lays out the chunk that holds the current page.
3. Scales the provisional page counts of the stale chunks.
4. Rebuilds the page list.

When the reader moves into a stale chunk, the engine lays it out and rebases
the page numbers after it. The page count becomes exact as the reader moves
through the book.

The chunk HTML must not contain layout settings. The format crates write a
font-size token instead. `create_document` replaces the token with the current
size. A change of page size needs a full reopen, because image sizes are baked
into the HTML.

## Fonts

The Lua module receives font files from KOReader `registerFont` calls. It
parses the faces with `read-fonts`. A face has a path, an index, a family, a
weight, and a style.

`cre-lib` builds one `parley::FontContext` from the font blobs. Generic family
lists (`serif`, `sans-serif`, `monospace`) contain all faces. The selected
family is first.

A font collection holds several families in one file. The selected family is
passed down as `DocumentOptions::font_family`. If only the family changes, the
engine rebuilds the font context from the same blobs.

Fonts embedded in the book work through Blitz's `@font-face` support: the CSS
`url()` is rewritten to an `epub://book/...` path and the document's net
provider serves the bytes, so the declared family, weight and style are
registered without a separate font list. TTF, OTF, WOFF and WOFF2 are decoded
(the `woff` feature is enabled). `local()` sources are skipped upstream.

The reader can turn those fonts off (`crengine.doc.embedded.fonts.enabled`).
Most books keep the rules in a linked stylesheet rather than in the chapter
HTML, so the setting is applied in two places: `cre_epub::strip_font_faces` on
the chunk HTML, and the same strip on the `.css` bytes served by `EpubNet` and
`FileNet`. Blitz registers a fetched face into the document's `FontContext` and
that registration outlives the chunk, so the flag also rebuilds the context —
otherwise the family would stay resolvable after the rules were dropped. See
[`FONTS.md`](FONTS.md) for the measurements.

## MOBI and KF8

A Kindle file is a PalmDB: the text flow is split into parts by the skeleton and
fragment indices, and `cre-kf8` reassembles each part into one XHTML document.
The whole book becomes one rendered document, with one `<section
class="cre-spine" data-spine-index="N">` per part, so page breaks and
`DocFragment` indices still follow the book's own files.

Resource references are rewritten on the way in: `kindle:embed:N` becomes
`mobi-image-N`, and the picture is sliced out of the memory-mapped file when it
is painted. A pre-paginated part declares its size with `<meta
name="viewport">`; the value is copied onto the section as
`data-cre-fixed-layout`, which is the same marker an EPUB fixed page uses, so
`parse_fixed_layout` handles both. The book's own `<body>` attributes are kept on
the generated body, otherwise every `body.class` rule in its stylesheet would
stop matching, and EXTH 498 provides the series name.

## Images

The format crates rewrite image references to `epub://book/<path>` URLs. The
net provider serves the bytes. SVG files are rasterized at page size with
resvg.

A MOBI/KF8/HTML part that holds nothing but a picture is turned into a
`cre-page-fit-N` plate before the document is chunked, so layout sees a
page-sized box and painting reads the picture from the resource table instead of
a base64 copy inside the HTML (a 48 MB comic: 612 -> 68 MB peak, 3.1 -> 0.03 s
to open). The part's own `<div>` is kept, because that is where a KF8 book puts
its TOC anchors.

## Resources

`ResourceSource` is the one place that answers "give me the bytes for this key":
a zip entry, a PalmDB record range, an in-memory map, or a file next to a
standalone document. It also carries a *generated* table, consulted first, for
what the engine makes up while preparing the book — page-fit wrappers, decoded
`data:` pictures, and a KF8 book's stylesheet flows. Stylesheets are served
rather than inlined, and the cover is only a key, so both go through the same
lookup as a picture. That is what lets a standalone HTML document (which
has no archive to write into) use the same plate path as an EPUB, and it keeps
the painter, the net provider and `getImageDataAt` on one namespace.

An image-only SVG block is rewritten to a plain `<img>`. It is an image, not a
drawing.

A full-page `<img>` with `height:100%` is wrapped in a generated SVG. The SVG
keeps the image aspect ratio and makes the image fill the page box.

A raster `<img>` is served to Blitz as a transparent SVG with the real
intrinsic size, and the original is painted by `cre-paint` at the size it is
drawn (JPEG decoded with DCT scaling). The placeholder, the page-fit wrapper,
the resource cache and the painting paths are in [IMAGES.md](IMAGES.md).

## The Lua bridge

`cre-lua` uses `mlua` FFI. Document methods are registered in a metatable.
Module functions are registered in the `cre` table.

The bridge does three jobs:

- Convert Lua types to Rust types.
- Call the engine.
- Convert errors and missing data to the values that KOReader expects.

Some methods only store settings. KOReader can call them without an error.

## Differences from crengine

- The engine has no on-disk cache.
- The DOM is not the full book. One chunk is resident.
- A chunk's DOM costs ~128 bytes per HTML byte when it is sized, and measures
  ~64 bytes per HTML byte laid out. Very low free memory refuses the book
  instead of swapping to disk; the check scales its reserve (8-24 MB) so a book
  that fits is not turned away.
- `TextPosition` strings are compatible, but the internal position model is
  different.
- EPUB CFI output is simplified.
- Vertical writing is not supported. Blitz does not lay out vertical text.
- BiDi is not supported yet: `direction`/`unicode-bidi` are not honoured
  end-to-end and text is laid out left-to-right. This waits on upstream Blitz.
- Hyphenation is not applied.

## Diagnostics

| Variable | Use |
|---|---|
| `CRE_LAYOUT_PROFILE=1` | Per-chunk stages, compression, trimming, workers |
| `CRE_RENDER_PROFILE=1` | Record / raster / convert split per page |
| `CRE_LUA_TRACE_PATH=FILE` | See which Lua calls KOReader makes |
| `CRE_RESOURCE_CACHE_KB=N` | Test resource cache pressure |
| `CRE_NO_TRIM=1` | A/B the heap trimming |
| `CRE_NO_SCALED_DECODE=1` | A/B scaled image decoding |
| `CRE_AVAILABLE_MB=N` | Simulate the device's free memory |
| `CRE_IGNORE_LOW_MEMORY=1` | Open a book the device would refuse |

The QEMU harness also sets `CRE_LIB_TRACE` for the shim, not for the module.
See [MEMORY.md](MEMORY.md) and [PERFORMANCE.md](PERFORMANCE.md) for the
measured numbers.

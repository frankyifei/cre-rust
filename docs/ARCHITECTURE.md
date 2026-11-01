# Architecture

This document describes how cre-rust opens a book and makes a page. It also
lists the important design decisions.

## Goal

The goal is a drop-in replacement for the KOReader `cre` module
(`libkoreader-cre.so`). KOReader must not change. The module keeps the Lua API
and the side effects that KOReader expects.

The implementation uses Rust and existing rendering libraries. It does not
copy the crengine C++ code.

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
| `cre-epub` | ZIP container, OPF, spine, TOC, page map, CSS collection, image and SVG rewrite |
| `cre-fb2` | FB2 XML, base64 binaries |
| `cre-text` | TXT and standalone HTML |
| `cre-pdb` | PalmDB container, MOBI header, EXTH, INDX/TAGX/CNCX, decompression |
| `cre-mobi` | MOBI 6 links, images, chapter split |
| `cre-kf8` | KF8/AZW3 flows, CSS, links, images |
| `cre-lib` | The engine. Open, layout, paginate, page lookup, fonts, images, paint |
| `cre-paint` | Scene to pixmap, page bitmaps, halftone and gamma |
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

## Layout and pagination

`cre-lib` builds one Blitz document for one chunk. It then collects:

- line boxes,
- content boxes (images, tables, block boxes),
- forced page breaks.

The paginator adds lines and boxes to pages until the page is full. A forced
break starts a new page. A chunk always starts on a new page.

The total page count is known after the first layout. KOReader needs the total
count right after `renderDocument`.

## Chunking and memory

The chunk size is a function of the available memory. `CRE_AVAILABLE_MB` and
`CRE_CHUNK_BYTES` override the estimate. The image payload of a chunk is also
bounded, because Blitz decodes images during layout.

The current chunk is the only resident layout. When the reader moves to
another chunk, the old layout is dropped and the new chunk is laid out.

Resource caches are byte-bounded. Resources that the engine generates
(rewritten XHTML, page-fit SVG wrappers) are **pinned** for the session. They
have no zip entry to read again.

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

## Images

The format crates rewrite image references to `epub://book/<path>` URLs. The
net provider serves the bytes. SVG files are rasterized at page size with
resvg.

An image-only SVG block is rewritten to a plain `<img>`. It is an image, not a
drawing.

A full-page `<img>` with `height:100%` is wrapped in a generated SVG. The SVG
keeps the image aspect ratio and makes the image fill the page box.

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
- `TextPosition` strings are compatible, but the internal position model is
  different.
- EPUB CFI output is simplified.
- Vertical writing is not supported. Blitz does not lay out vertical text.
- Hyphenation is not applied.
- Embedded `@font-face` fonts are not used.

## Diagnostics

| Variable | Use |
|---|---|
| `CRE_LAYOUT_TIMING=1` | Find slow chunks |
| `CRE_LUA_TRACE_PATH=FILE` | See which Lua calls KOReader makes |
| `CRE_SNAPSHOT_DEBUG=1` | See which setting caused a relayout |
| `CRE_RESOURCE_CACHE_KB=N` | Test resource cache pressure |
| `CRE_XP_POINT_DEBUG=1` | Find XPointers that do not map to a line |

The QEMU harness also sets `CRE_LIB_TRACE` for the shim, not for the module.

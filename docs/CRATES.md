# Crates

The workspace is layered so a format crate only implements its container. The
rule is one-directional: a format crate may depend on `cre-book` and `cre-pdb`;
it must never depend on `cre-lib`.

```
cre-resource                               the two resource traits, nothing else
   ↑                    ↑
cre-book            cre-pdb                format-independent layer / PalmDB
   ↑                    ↑
cre-epub  cre-kf8  cre-mobi  cre-fb2  cre-text      container → HTML + metadata
   ↑
cre-lib                                    layout, pagination, XPointers, paint
   ↑
cre-lua                                    the KOReader module
```

`cre-book` and `cre-pdb` do not know each other: `cre-book` holds a
`ResourceIndex` and `cre-pdb` implements it, so the format-independent layer
never learns that a PalmDB exists.

## `cre-book`

What every format shares, and what the layout engine consumes:

| Module | Contents |
|---|---|
| `resources` | `ResourceSource` (provider / `MapResources` / files next to a standalone document, plus the generated table) and `MapResources` (a newtype over the container's `ResourceIndex`). The traits themselves are in `cre-resource`, see below |
| `chunk` | splitting generated HTML at section boundaries, chunk budgets from the free memory, `stream_single_document_chunks` |
| `html` | attribute/tag scanning, `normalize_path`, `resolve_href`, `encode_path`, percent and base64 helpers |
| `images` | `<img>` sources, image header reader, decoded-size estimates, page-fit wrappers, picture-only part detection, `data:` decoding |
| `fonts` | `@font-face` collection, `strip_font_faces`, `loadable_font_faces`, `stylesheet_hrefs` |
| `ruby` | the stacked ruby cell rewriting (Blitz has no ruby box) |
| `css` | the layout tokens (`__CRE_FONT_SIZE_PX__`, `__CRE_PAGE_MARGIN_PX__`) and `substitute`, plus `body_selectors_to_class` |

It depends only on `cre-resource`, `image`, `regex`, `base64`,
`percent-encoding`, `anyhow` and `libc` - never on blitz and never on a
container crate - so a format can be tested on its own.

## `cre-resource`

Two traits, one per direction the bytes are read from:

- `ResourceProvider` hands out owned bytes (`Arc<Vec<u8>>`): a zip entry is
  decompressed, a file is read. `cre-epub` implements it, and so does a
  directory of files.
- `ResourceIndex` lends a slice (`&[u8]`): a memory-mapped container already has
  the bytes in its mapping, and the readers below it (the intrinsic-size probe,
  the image placeholder) only look at the first bytes of a picture. Copying the
  record to answer them faults every page of every picture in during the layout
  pass, which is the whole reason `apo.azw` opens in 85 MB instead of several
  hundred. `cre-pdb` implements it for its record index; an in-memory map is the
  same trait implemented in `cre-book`.

Keeping the traits in their own crate is what lets `cre-book` stay
format-independent while `cre-pdb` gains no dependency on the book layer.

## What a new format implements

1. Container parsing and text extraction.
2. Metadata mapping.
3. Reading order: spine documents, or one HTML body with
   `<section class="cre-spine" data-spine-index="N">` parts.
4. Table of contents as `(title, href, depth)`.
5. A `ResourceProvider` (or a directory of files).
6. The cover as a resource key.
7. Format quirks: URL prefix rewriting, link-to-anchor rewriting, `<body>`
   attributes, per-document style scoping.

Everything else - chunking, lazy open, pagination, page numbers, XPointers,
partial relayout, image placeholders and page-fit plates, embedded fonts, ruby,
MathML, table fixes, full-text search, the Lua API - comes from the shared
layers. See "Extending" in [`ARCHITECTURE.md`](ARCHITECTURE.md#resources).

## Resource table

One namespace for every kind of byte: a zip entry, a PalmDB record range (a
`ResourceIndex`), an in-memory map (`MapResources::owned`), a file next to a
standalone document, or something the engine generated (a page-fit wrapper, a
decoded `data:` picture). The generated table is
consulted first, and the net providers serve it, so a generated key behaves like
a file for layout, painting, `getImageDataAt` and long-press.

Two consequences worth knowing when adding a format:

- a **stylesheet** is served through the table, not inlined into the HTML when
  it can be avoided (a KF8 flow is published as `cre-flow-N.css` and linked from
  the generated head, exactly like an EPUB's manifest CSS), so the "use the
  book's fonts" setting is applied once, where the sheet is read;
- the **cover** is only a key. `Document::cover_image()` reads it from the table
  on demand instead of copying a few megabytes at open.

## Why there are two chunk packers

`stream_single_document_chunks` (with `pack_sections`) packs sections of one
already-generated body: one constant head, a fixed prefix and no per-section
break rule. EPUB packs its spine documents as they are decoded - the whole body
never exists - builds the head of each chunk from the documents that chunk holds,
and gives a pre-paginated page or a split continuation a chunk of its own.

Forcing both through one function would need flags for the prefix and the
break-after rule, and it would move EPUB's chunk boundaries, which the page
numbers are derived from, for no user-visible gain. What *is* shared is what
could drift: the budgets (`chunk_budgets`, `plan_chunk_bytes`,
`LAYOUT_BYTES_PER_HTML_BYTE`, `DOM_BYTES_PER_HTML_BYTE`), the section splitter
(`split_html_document_at_sections_with`) and the per-section cost (an
`image_sources_in` walk over the resource table). S5-6 was left as a documented
decision for that reason.

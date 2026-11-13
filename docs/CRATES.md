# Crates

The workspace is layered so a format crate only implements its container. The
rule is one-directional: a format crate may depend on `cre-css`, `cre-book` and
`cre-pdb`; it must never depend on `cre-lib`.

```
cre-css                                    layout CSS tokens
   ↑
cre-book                                   the format-independent book layer
   ↑
cre-epub  cre-kf8  cre-mobi  cre-fb2  cre-text      container → HTML + metadata
   ↑ (cre-kf8 / cre-mobi also use cre-pdb)
cre-lib                                    layout, pagination, XPointers, paint
   ↑
cre-lua                                    the KOReader module
```

## `cre-book`

What every format shares, and what the layout engine consumes:

| Module | Contents |
|---|---|
| `resources` | `ResourceProvider` (the container side: `get`, decoded-size estimate, cache/pin accounting, `drop_pages`), `ResourceSource` (provider / in-memory map / files next to a standalone document, plus the generated table), `MapResources` |
| `chunk` | splitting generated HTML at section boundaries, chunk budgets from the free memory, `stream_single_document_chunks` |
| `html` | attribute/tag scanning, `normalize_path`, `resolve_href`, `encode_path`, percent and base64 helpers |
| `images` | `<img>` sources, image header reader, decoded-size estimates, page-fit wrappers, picture-only part detection, `data:` decoding |
| `fonts` | `@font-face` collection, `strip_font_faces`, `loadable_font_faces`, `stylesheet_hrefs` |
| `ruby` | the stacked ruby cell rewriting (Blitz has no ruby box) |
| `css` | `body_selectors_to_class` |

It depends only on `cre-pdb`, `image`, `regex`, `base64`, `percent-encoding`,
`anyhow` and `libc` - never on blitz - so a format can be tested on its own.

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

One namespace for every kind of byte: a zip entry, a PalmDB record range, an
in-memory map, a file next to a standalone document, or something the engine
generated (a page-fit wrapper, a decoded `data:` picture). The generated table is
consulted first, and the net providers serve it, so a generated key behaves like
a file for layout, painting, `getImageDataAt` and long-press.

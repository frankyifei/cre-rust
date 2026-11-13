# Image pipeline

Images are the largest single item an illustrated book can hold, and Blitz
decodes them during layout. This document explains how the engine keeps the
decoded pixels out of the layout budget without changing what is painted.

## Why images are special

Blitz's net handler only accepts bytes; Blitz then decodes them into an RGBA
buffer and keeps that buffer in the DOM for as long as the chunk is resident.
A single 1443x2048 photo is 11.8 MB of RGBA. `15.epub` (fifteen such photos)
reached 105 MB of engine RSS at open, 127 MB peak and 153-190 MB inside the
reader before the work below; crengine needs 71-94 MB for the same book.

Layout only needs an image's **intrinsic size and aspect ratio**: our
stylesheet caps every image to the page.

## Size placeholders

An `<img>` raster resource is served to Blitz as a **transparent SVG with the
real intrinsic size**, whether it is large or small: layout only needs the
intrinsic size, and the SVG declares exactly the raster's width and height, so
page breaks do not move. The original pixels are painted by
`cre_paint::draw_placed_images` into the page raster, through the same
`draw_image_fill` path Blitz uses, so downscaling and `object-fit: fill` match.

Keeping small images out of the DOM matters for books built from many
pictures: Blitz keeps every decoded RGBA copy in its per-chunk image cache, so
a chapter with a few hundred ornaments keeps tens of megabytes alive even
though only one page is on screen.

Rules:

- every raster `<img>` resource is replaced (`cre_epub::image_sources_in`);
- SVG resources keep their pixels: they are rasterized to a small page-sized
  PNG first, and replacing that hidden the picture;
- CSS background images and `data:` images keep their pixels, because nothing
  else would paint them;
- a page that is nothing but a picture is turned into a fixed-layout page and
  uses the same fit transform, so the placeholder works there too;
- the original bytes stay in `ResourceSource`, so `getImageDataFromPosition`
  (long-press image viewer) and `getHTMLFromXPointer` are unchanged;
- the colour render path collects placeheld images on fixed pages as well as on
  reflowed pages.

A picture that is painted by the placeholder is counted in the
`(image count, image coverage)` result of `drawCurrentPage`: KOReader uses it
to dither and to request a full refresh on image pages, and a placeholder is no
longer part of the paint scene.

## Page-fit wrappers

A full-page picture (`height: 100%` on an otherwise empty page) is wrapped in a
generated `cre-page-fit-N.svg`. The wrapper is an empty SVG with the page's
size; `PageFitImage { source, width, height, contain }` maps it back to the
original resource, which is painted with `contain` into the page box. The
picture is not cropped, no base64 copy is pinned, and no decoded pixels are
held during layout. This removed 10 MB of pinned base64 copies and a 4.6 MB
page-sized PNG cache on `12.epub` (RSS 77.2 -> 39.5 MB).

MOBI and KF8 get the same wrapper through a different door. Their parts are
`<section class="cre-spine">` blocks inside one document, so
`normalize_map_page_images` rewrites a picture-only part at open time — before
the document is split into chunks — and the wrapper SVG is served from
`ImageRaster` (`page_fit_resources`) because a PalmDB has no writable resource
view. Only the `<img>` tag is replaced: the wrapper `<div>` carries the book's
TOC anchors (`id="k8-42-1337"`), and losing it made every navigation entry fall
back to "no page".

A 48 MB fixed-size comic (184 plates) measured on the native x86_64 release
build with `perf_probe`:

| | Open | Peak RSS | First page |
|---|---|---|---|
| Inlined `<svg><image href="data:…">` | 3 099 ms | 612 MB | 61052 ink |
| `cre-page-fit-N` resource | 26 ms | 68 MB | 61048 ink |

The ink difference is anti-aliasing at the page edge; the page content is the
same. Without the rewrite each plate was a base64 copy inside the chunk, and
`usvg` decoded every one of them during the layout parse.

Two things are needed for the plate to look right in the reader, and both were
found the hard way:

- the wrapper SVG must declare `width`/`height` (not just a `viewBox`). It is
  referenced from an `<img>`, so only its intrinsic size is used: a `viewBox`
  alone leaves the element at the CSS default box and the picture appears small
  in a corner.
- the part's own page-box CSS has to go. Kindle comics size the page with
  `div.fs { width:1072px; height:1448px; position:relative }` and
  `img.singlePage { position:absolute; height:100% }`, i.e. the book's own
  fixed page. The rewrite replaces the part's content with the wrapper, and
  re-emits its `id`/`name` anchors as empty `<a>` elements so navigation still
  lands on the page without the book's box.

## Decoding at the painted size

A photo on a 758x1024 page is painted at roughly a quarter of its pixels.
JPEG supports decoding at 1/1 to 1/8 through DCT scaling, so
`draw_placed_images` decodes at the box it is about to fill:

- `jpeg-decoder` with `scale()` for JPEG;
- the `image` crate for everything else, and for CMYK/16-bit JPEGs that the
  scaled path cannot take;
- the RGBA conversion expands the decoder's buffer in place, so there is no
  second copy.

`CRE_NO_SCALED_DECODE=1` restores the full decode for comparison.

Measured on armel under qemu (760x1024 target):

| Image | Decode before | Decode after | RGBA before | RGBA after |
|---|---|---|---|---|
| 1443x2048 | 194 ms | 138 ms | 11.8 MB | 2.2 MB |
| 1804x2560 | 281 ms | 174 ms | 18.5 MB | 3.5 MB |

In the engine, image pages render in the same time or slightly faster while
the peak drops: `15.epub` 70.2 -> 51.0 MB, `12.epub` 69.2 -> 59.3 MB.

## Painting

- Minified, axis-aligned images are pre-scaled once with a premultiplied box
  filter and then blitted 1:1 with nearest sampling; the box filter removes
  downscale aliasing that bilinear minification leaves.
- The box filter splits destination rows across cores (capped at four, skipped
  below 256K source pixels); band boundaries fall on destination rows, so the
  result is the same integer sum as the sequential filter.
- On 32-bit ARM, the fills that still need per-pixel filtering (upscaled or
  rotated images) use a 16.16 fixed-point bilinear sampler with integer
  blending instead of tiny-skia's scalar f32 pipeline.
- The 1:1 blit is kept because letting tiny-skia sample directly measured
  49-71% slower on ARM.

Measured on x86_64 release: the box filter drops from 6.3-7.8 ms to
2.5-3.5 ms, and `12.epub` image pages from 18.1 ms to 11.5 ms (-36%).

## Resource bytes

- `lookup_resource` returns `Arc<Vec<u8>>`, so a resource shared by several
  consumers is not copied.
- The decompressed-resource cache is a byte-bounded LRU (4 MB by default,
  `CRE_RESOURCE_CACHE_KB` overrides). Evicted images are re-read from the zip
  when they are painted again.
- Resources the engine generated (rewritten XHTML, page-fit wrappers) are
  **pinned**: they have no zip entry to read again, and evicting them would
  make the pages that use them blank.
- Image keys are resolved the way the provider serves them: percent-decoded,
  and with any query string dropped from `src`/`href`.

## Measurements

| Case | Before | After |
|---|---|---|
| `15.epub` engine open / peak | 105 / 127 MB | 33 / 45 MB |
| `15.epub` reader open / peak | 153 / 169 MB | 102 / 119 MB |
| `zyj.epub` (263 images), open / peak | 75.6 / 115.3 MB | 69.3 / 81.5 MB |
| `21.epub` (108 images), open / peak | 50.9 / 83.6 MB | 47.3 / 60.7 MB |
| `12.epub`, page-fit wrappers, RSS / peak | 77.2 / 90.3 MB | 39.5 / 42.9 MB |
| Gallery of 8 full-page JPEGs, 64 MB budget, peak | 126 MB | 56 MB |
| 60-image book, RSS after open | 40.1 MB | 35.6 MB |
| `15.epub` page-turn time | 12.8 ms | 11.9 ms |

Pages keep their layout and their ink within 1%. Scanned-text pages are
slightly softer at half-size IDCT, which is the visible cost of the saving.

## Trade-offs

**Pros**

- Decoded pixels never sit next to the text DOM, so an illustrated book's peak
  is bounded by one image's painted size instead of its natural size.
- Layout is unaffected: the placeholder reports the true intrinsic size, so
  page breaks do not move.
- The long-press viewer and the HTML dump still get the original bytes.
- JPEG decode is faster and allocates less at the scaled size; the box filter
  removes aliasing.

**Cons**

- In reflow, `object-fit` values other than `fill` are not honoured (fixed
  pages use `contain` through the page-fit wrapper). crengine does not
  implement `object-fit` either, but the placeholder makes the difference
  visible.
- A placeholder image is decoded again for each new image page: there is no
  decoded-image cache by design. With scaled decode this is cheap, but it is
  not free.
- Scanned text is slightly softer at half-size IDCT.
- The placeholder path depends on the image being referenced by `<img>` and on
  the source being found by `image_sources_in`; unusual URL forms fall back to
  the pixels-in-DOM path.
- Feeding Blitz a transparent SVG is a workaround. `TODO(upstream)`: Blitz's
  net handler has no way to supply a declared size separately from the pixels;
  an `LVImageSource`-like API would remove both the placeholder and
  `PlacedImage`.

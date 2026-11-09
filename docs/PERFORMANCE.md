# Performance

The target is a single-core Cortex-A9 (Paperwhite 2) with an e-ink screen, so
both open time and page-turn time matter. This document lists the
optimisations and the measurements that justified them.

## Page rendering

The painter is our own scene rasterizer on `tiny-skia`, with an optional
`vello_cpu` backend. Profiling a text page showed the painter doing much more
work than the content needed:

- a fully transparent fill spans the whole chunk (38 395 px tall in one test
  book) and tiny-skia rasterised every scanline: 2.1 ms per page. Transparent
  fills are now dropped, and a background equal to the bitmap's base colour is
  dropped too.
- the RGBA-to-gray conversion ran per pixel through an iterator with two
  divisions and a composite per pixel. It now walks the buffer in place with a
  fast path for opaque pixels (background and glyph cores) and composites only
  anti-aliased edges: 1.5 -> 0.7 ms.
- axis-aligned, pixel-aligned solid fills use tiny-skia's `fill_rect` instead
  of the coverage rasteriser (identical for integral edges).
- glyph masks and the font collection are persistent: `PaintCache` survives
  across page renders (bounded, cleared when the font context changes) and the
  `Collection` is shared, so a per-chunk `FontContext` clone is an Arc bump.
- the multi-column style stripper skips chunks with no `column` declaration
  instead of running two regexes over every chunk.

Measured on x86_64 release, 20 pages per book, mean of 3 runs:

| Book | Page before | Page after |
|---|---|---|
| `juliet.epub` | 5.67 ms | 2.60 ms |
| `jk.epub`, text pages | 4.49 ms | 1.43 ms |
| `jk.epub`, mixed pages | 5.21 ms | 2.27 ms |
| `12.epub`, image pages | 20.59 ms | 17.98 ms |

The 64-page hash corpus and the 51-book regression are checked after each
change. Text pages must render byte-identically; the image work changed only
image pages, and their ink stayed within ~1.5%.

## Reusing the page bitmap

`Rasterizer::new` allocated a page-sized pixmap for every render, and
`Pixmap::new` zeroed several megabytes right before painting the background
over it. The cache now keeps the pixmap and hands it back after a render, so a
page turn skips the allocation and the zeroing.

Measured on `jk.epub` text pages: setup 0.4 -> 0.08 ms, render 1.5 -> 1.2 ms.
The first render of a process still allocates.

## Image fills

Image pages were the one place where tiny-skia was clearly slower than vello on
32-bit ARM: no armv7 SIMD, bilinear only in the f32 highp pipeline, and
per-pixel filtering plus generic stage dispatch dominated paint time.

- Minified, axis-aligned images are pre-scaled to device size with a
  premultiplied box filter, cached per page keyed by image blob and target
  size, then blitted with an integer source-over 1:1 path. This removes
  downscale aliasing and skips the shader pipeline.
- The box filter runs destination rows in bands across the cores (capped at
  four, skipped below 256K source pixels). Band boundaries fall on destination
  rows, so the result is the same integer sum as the sequential filter.

Measured on qemu with `-cpu cortex-a9`, same binary, `CRE_PAINT_BACKEND=tiny`
vs `vello`:

| Book | tiny before | tiny after | vello |
|---|---|---|---|
| `15.epub`, image pages | 1138 ms/page | 320 ms/page | 456 ms/page |
| `juliet.epub`, mixed | - | 56 ms/page | 159 ms/page |

On x86 the two backends are within ~10% for text and image pages; the
paint-feature page is byte-identical.

The 1:1 blit is intentional: letting tiny-skia sample a minified image
directly is 49-71% slower on ARM.

## Open-time layout

Every chunk is laid out once at open to learn the page count. The pass runs up
to four chunks at a time, and how many is decided from measurements, not a
constant:

1. the chunk that looks most expensive (most images, then largest HTML) is
   laid out first as a probe; its resident cost is sampled before and while
   its DOM is alive;
2. `workers = (available - reserve - RSS) / cost`, capped at the core count
   and at four;
3. `LayoutMemoryGate` stops a worker from starting a chunk while RSS is above
   the plan, but always lets one through so the pass cannot stall;
4. the opening chunk is laid out last and its DOM is reused, and the workers'
   arenas are trimmed before it (see [MEMORY.md](MEMORY.md)).

Measured on x86_64 release, interleaved runs:

| Book | Open before | Open after | Peak before | Peak after |
|---|---|---|---|---|
| `jk.epub` (70 chunks) | 1500 ms | 645 ms | 54 MB | 95 MB |
| `12.epub` (image pages) | 480 ms | 415 ms | 178 MB | 228 MB |
| `juliet.epub` | 202 ms | 202 ms | 46 MB | 57 MB |

On a memory-limited device the gate backs off: `CRE_AVAILABLE_MB=128` gives
`jk.epub` two workers (1532 -> 948 ms, 67 -> 79 MB) and keeps `12.epub` on one
worker (peak unchanged). `CRE_AVAILABLE_MB=256` gives `jk.epub` four workers.

## Settings changes

A font, margin or font-family change marks the other chunks stale, lays out
only the chunk that holds the current page, and rescales the provisional page
counts. Measured in the QEMU harness:

| Book | Full relayout | Partial relayout |
|---|---|---|
| `juliet.epub` | 4.1 s | 0.47 s |
| `zyj.epub` | 7.3 s | 0.71 s |

The page count becomes exact again as the reader moves through the book.

## Compression

Chunk HTML is stored as one LZ4 block per chunk. Compression costs about 60 ms,
under 1% of a 6.4 s open pass (the measured open time was 6465 -> 6433 ms,
within run-to-run noise), and nothing to a page turn: a chunk is decompressed
once when it becomes active, and the decode of a 128 KB block is far below the
cost of laying it out.

## Trade-offs

**Pros**

- Text pages render 2-3x faster than the first implementation, with identical
  pixels.
- The page pixmap, glyph masks and font collection are reused across turns.
- Image paint is close to the vector backend on ARM without its memory cost.
- Open time uses spare cores when the device has memory, and backs off to one
  worker when it does not.
- Diagnostics are built in (`CRE_LAYOUT_PROFILE`, `CRE_RENDER_PROFILE`), so
  each change above can be re-measured on the device.

**Cons**

- Parallel layout raises the peak; the worker policy has to be tested against
  the memory gate whenever either changes.
- Image pages are still an order of magnitude slower than text pages; decode
  and resampling dominate, and scanning a large illustration is inherently
  expensive on a Cortex-A9.
- Some paths are architecture-specific (the 32-bit ARM fixed-point sampler),
  so x86 results do not predict ARM results; every change is measured on both.
- The first open of a big book still lays out the whole book. There is no
  on-disk layout cache, so the second open is just as slow (see
  [MEMORY.md](MEMORY.md)).
- Caches are deliberately bounded; a very large book re-reads evicted
  resources and re-decodes images instead of growing RSS.

## Knobs

| Variable | Effect |
|---|---|
| `CRE_LAYOUT_PROFILE=1` | Per-chunk stage timings, compression, trim, worker count |
| `CRE_RENDER_PROFILE=1` | Record / raster / convert split per page |
| `CRE_LAYOUT_WORKERS=N` | Force the number of parallel layouts |
| `CRE_NO_TRIM=1` | Disable `malloc_trim` (A/B the cost) |
| `CRE_NO_SCALED_DECODE=1` | Decode images at full size (A/B DCT scaling) |
| `CRE_PAINT_BACKEND=tiny\|vello` | Painter backend in a `vello-cpu` build |
| `CRE_AVAILABLE_MB=N` | Simulate the device's free memory |

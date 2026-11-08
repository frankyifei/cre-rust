# cre-rust

**cre-rust is a ebook reading library in Rust, built on Blitz. It is compatible
to Koreader**

It implements KOReader's `cre` module (`libkoreader-cre.so`) API on
top of the [Blitz](https://github.com/DioxusLabs/blitz) HTML/CSS engine —
Stylo for CSS, Taffy for boxes, Parley for text — instead of the crengine C++
engine. KOReader keeps its Lua API and does not change: drop the resulting
module into `koreader/libs/` and the reader opens EPUB, FB2, MOBI, AZW, HTML and
plain text exactly as before.

This is not a direct port of crengine. No crengine code is directly used; the 
layout model, the pagination, the painter and the MathML translator are new code written
against Rust libraries. It references crengine heavily for its behaviour.

## Status

Tested with KOReader v2026.07.1:

- on a Kindle Paperwhite 2 target (ARMv7, soft-float ABI, glibc 2.12),
- in the QEMU harness in this repository,
- with an x86_64 desktop KOReader build.

The module exposes about 124 document methods and 24 module functions, and the
workspace has 32 test targets.

## Highlights

The interesting work is the part Blitz does not provide. These pieces are
ours.

### MathML rendering (`cre-mathml`)

Blitz has no MathML support. `cre-mathml` translates a MathML presentation
subset into plain HTML boxes that the normal layout engine understands:
tokens, `mrow`, `mfenced`, scripts, `mfrac`, `msqrt`/`mroot`, `mtable`,
`mspace`, `mstyle`, `merror`, `mphantom`, `menclose`, `mmultiscripts` and
`maction`.

The parts that took real work:

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
Changing a font, margin or font-family lays out only the current chunk, and
the page list is rebased around it. Measured in the QEMU harness:

| Book | Full relayout | Partial relayout |
|---|---|---|
| `juliet.epub` | 4.1 s | 0.47 s |
| `zyj.epub` | 7.3 s | 0.71 s |

### Fonts

- TTF, OTF, TTC and OTC files, with face selection inside a collection (Noto
  Serif CJK JP/KR/SC/TC/HK and similar);
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
| KF8 / AZW3 | Separate crate, shared PalmDB container |

The format is detected from the file content, not from the extension.

## Limits

- Vertical writing (`writing-mode: vertical-rl`) is not supported. Blitz lays
  out horizontal text only.
- RTL text uses Parley bidi reordering. `direction` is partial and
  `unicode-bidi` is not supported.
- Text hyphenation is not applied. The module stores the dictionary name only.
- Embedded `@font-face` fonts are not used. KOReader must register the font
  files.
- `text-shadow` is not painted: Blitz does not parse it, so the paint stream
  carries no shadow for glyph runs. `box-shadow`, filters, `opacity` and
  `clip-path` (on block boxes) do work.
- MathML is a presentation subset. A matrix (`mtable`) is kept on one page
  rather than split between rows; huge formulas are clipped, as an inline
  table would be in crengine.
- MOBI and KF8: no DRM, no embedded fonts, and limited CSS.
- EPUB CFI output is simplified.
- Two-page view, scroll view, and some page-flow APIs are partial.
- There is no on-disk cache. Opening a book again makes the pages again.
- Some cre.cpp APIs return fixed values. See [TODO.md](TODO.md).

## Repository layout

| Path | Content |
|---|---|
| `crates/cre-lib` | Engine core: open, layout, pagination, page lookup, positions, images, fonts |
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

The build also needs glibc 2.12 symbols. The build script checks the maximum
GLIBC symbol version and prints it.

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

The output is:

```text
target-arm-kindlepw2-linux-gnueabi/armv7-unknown-linux-gnueabi/release/libkoreader-cre.so
```

The script prints the maximum GLIBC version. The value must be `GLIBC_2.12` or
lower for the Paperwhite 2.

Reference sizes:

| Variant | Size |
|---|---|
| VFP, strict align | about 18.9 MB |
| Software float | about 18.9 MB |

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

The test suite has 32 targets.

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
| `CRE_LAYOUT_TIMING=1` | Log the time for each chunk layout |
| `CRE_CHUNK_BYTES=N` | Force the chunk size in bytes |
| `CRE_AVAILABLE_MB=N` | Set the memory budget for chunk sizes |
| `CRE_RESOURCE_CACHE_KB=N` | Set the resource cache size |
| `CRE_LUA_TRACE_PATH=FILE` | Write a trace of the Lua API calls |
| `CRE_SNAPSHOT_DEBUG=1` | Log settings that cause a relayout |
| `CRE_XP_POINT_DEBUG=1` | Log XPointer positions that have no line |
| `CRE_MATH_DEBUG=1` | Log the math font that was picked for math content |
| `CRE_PAINT_BACKEND=tiny\|vello` | Choose the paint backend in a build with `vello-cpu` |

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

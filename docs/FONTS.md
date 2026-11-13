# Embedded fonts

A book can ship its own fonts and ask for them in its CSS with `@font-face`.
KOReader exposes that as the **Embedded Fonts** toggle in the typeset menu, and
shows the list of families the book ships when the option is long-pressed. Two
`cre.cpp` entry points back it:

| Lua call | What the engine returns |
|---|---|
| `getEmbeddedFontList()` | `{ ["family name"] = true }`, empty when the book ships none |
| `setIntProperty("crengine.doc.embedded.fonts.enabled", 0/1)` | schedules a relayout, no reopen |

## Loading is Blitz's job

`cre-lib` does not decode or register fonts. Blitz's `FontFaceHandler` parses a
`CssRule::FontFace`, fetches every `src: url(...)` through the document's
`NetProvider` (so `epub://book/fonts/x.ttf` is served from the EPUB and a
`file://` path from the HTML document), sniffs or honours the format hint, and
registers the face under the **CSS-declared** family, weight and style rather
than whatever the file's `name` table says. TTF, OTF, WOFF and WOFF2 are
supported; `local()` sources are skipped upstream, so a book that only points at
a font it expects to be installed falls back normally.

Nothing in this repository has to rewrite the CSS for that to work.

## What the engine does

1. **Collect the family names at open.** A chapter normally links its
   stylesheet (`<link rel="stylesheet" href="../page_styles.css">`), so the
   rules are usually *not* in the chunk HTML at all. Both places are scanned:
   `cre_epub::font_faces_in_stylesheets` walks the OPF manifest's CSS items, and
   the chunk callback runs `cre_epub::font_faces_in` for rules inlined in a
   `<style>` element. The result is `Document::embedded_font_list()`.
2. **Honour the toggle at layout time.** Turning the setting off must not cost a
   reopen. The rules are dropped in three complementary places:
   - `cre_epub::strip_font_faces` on a chunk's HTML before it is parsed;
   - the same strip on every `.css` served by `EpubNet` and `FileNet`;
   - a rebuild of the `FontContext`, because Blitz registers an `@font-face`
     font into it and **that registration outlives the chunk it came from**.

   The third step is the one that matters on a large book: an `@font-face`
   installed while the reader had fonts enabled stays resolvable for the rest of
   the process, so stripping the rules alone changes nothing visible. A fresh
   context makes the families unavailable and the CSS `font-family` list falls
   through to the reader's font.
   `blitz-dom` has no "unregister a document font" API, hence the context
   rebuild; see the self-implemented list in
   `docs/blitz-dom-adoption-plan.md`.

The setting is applied by `Document::relayout`, so the page the reader is on is
re-laid out immediately and the rest converges in the background, exactly like
any other typeset change (see `ARCHITECTURE.md`). It also survives a reopen: a
page-size change rebuilds the document, and `cre-lua` re-applies the setting to
the new one.

## Measurements

Native x86_64 release build, `perf_probe`, 758×1024, font size 22, page margin
10. Book: a 13 MB Chinese-language EPUB, 20 chunks, **three** declared families
(one real TTF pair plus one `local()` alias).

| | Pages | RSS after open | Ink on page 2 |
|---|---|---|---|
| Book's fonts enabled (default) | 707 | 80 MB | 5 208 |
| Toggled off, then relayout | 679 | 56 MB | 0 |
| Rules removed from the CSS before opening | 679 | 34.6 MB | 0 |

Three things follow from the table:

- the fonts really are in use (page count, page bitmap and memory all move);
- the toggle is complete without a reopen, which is the path KOReader takes from
  the menu;
- the switch is worth having on a Kindle: the two embedded TTF files cost about
  **45 MB** once Blitz has registered them (80 MB → 34.6 MB for a fresh open).

The relayout row keeps 56 MB rather than dropping to 34.6 MB in the same
process: chunks already laid out keep their page tables until the background
worker replaces them, and the allocator does not hand the font data's arena
back to the kernel immediately. A fresh open is the steady state; toggling off
mid-book reaches it as the reader moves through the book.

There is **no disk cache** for layout or fonts. Every open re-parses, re-fetches
and re-registers; a cache is a possible future optimisation, not something these
numbers include.

## Which formats

| Format | Font list | Toggle |
|---|---|---|
| EPUB 2/3 | yes — OPF manifest CSS plus any inline `<style>` | yes |
| Standalone HTML/XHTML | yes — the document plus the stylesheets it `<link>`s | yes |
| AZW3 / KF8 | not yet | strips, but there are no rules to strip *and* no font records to load |
| MOBI 6, FB2, TXT | no `@font-face` in practice | — |

KF8 is the one worth adding. Its CSS lives in the flow and is already inlined
into the chunk HTML (so `font_faces_in`/`strip_font_faces` apply unchanged), and
`rewrite_kindle_urls` already turns `url(kindle:embed:N?mime=…)` into the
resource name `mobi-image-N`. What is missing is the other end:
`cre_kf8::build_resources` registers only records that look like images, so a
font record is never exposed and Blitz fetches nothing. Registering the TTF/OTF
records (magic `0x00010000`, `OTTO`, `ttcf`) plus collecting the names in the
KF8 branch would make the table say "yes" for KF8 too.

## Verified in the real reader

The numbers above come from `perf_probe` (native x86_64). The same book was run
through the real **x86 KOReader emulator** with
`tools/koreader-emulator-probe/run-probe.sh <book> fonts`, which calls
`ReaderTypeset:onToggleEmbeddedFonts` — the menu path, not the engine API:

```
fonts list=FZBeiWeiKaiShu-S19_GB18030 | STXingkai | Song pages=1631 current=15
fonts toggled off pages=1631 partial=1 rss=103692KiB
fonts on  t=0.6s pending=false rss=156572KiB
```

So the Lua binding returns the families to KOReader, the toggle goes through
`setIntProperty` and starts one partial rerender, and the reader process gives
the fonts' memory back (146 MB → 104 MB). The emulator uses its own viewport,
CSS and margins, so its page count is not comparable with the table above.

## Kernel of truth: the test

`crates/cre-lib/tests/embedded_fonts.rs` builds a synthetic EPUB that embeds a
monospace font — first behind a linked stylesheet, then in an inline `<style>` —
and asserts that the family is listed, that turning the fonts off changes page 1
and the page count (38 → 28 pages, 644 → 870 non-space characters on page 1),
and that turning them back on restores the original numbers without reopening.
A book without `@font-face` must report an empty list, because KOReader disables
the menu entry with `next(document:getEmbeddedFontList()) ~= nil`.

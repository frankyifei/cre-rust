# KOReader plugins for cre-rust

KOReader does not need to be patched: everything the engine needs on the
frontend side lives in a plugin here, so `git pull` upstream keeps working.

## `crebackgroundlayout.koplugin`

After a font, line-spacing or margin change the engine lays the chunk under the
reader out immediately and the rest of the book on a worker thread, installing
chunks as they finish. The same happens when a big book is opened (only the first
chunk is laid out). Page numbers are a prefix sum of chunk pages, so the page
total and every page number move during that pass.

KOReader caches both: `ReaderFooter` keeps `self.pages` until a `TocReset` /
`DocumentRerendered`, and `ReaderToc` keeps the page numbers it read at layout
time until `DocumentRerendered`. crengine gets that event by reloading the
document from its background-rerendering subprocess; this engine lays the chunks
out in-process, so nothing tells the reader and the footer stays on the number it
had when the settings changed (measured on a 320-chunk EPUB: stuck at 2003 pages
while the layout settled at 1977, and no change at all when the chunk under the
reader keeps its page count).

The plugin closes that hole using only events and the document API:

- on `PageUpdate` / `PosUpdate` it re-reads the page total and sends `TocReset`
  when it moved, which is what makes `ReaderFooter:onTocReset` re-read it, and it
  starts the watch if a pass is running that it has not seen start;
- when the event alone does not update the footer (the footer lives on
  `ReaderView`, and an event that walks the window stack does not always reach
  it), it writes `footer.pages` and repaints exactly as `onTocReset` would. The
  `CREBG_LOG=1` line `publish ... footer before a after event b now c` says which
  of the two paths did the work;
- it watches after `onReaderReady` **and** after `DocumentRerendered`, and the
  reader-ready hook re-arms every time: a style change reloads the document, so a
  hook that runs once leaves that style change unwatched;
- reading the page count is the first thing the watch does, before it trusts
  `hasPendingRerendering()`: the engine defers a settings change to the next
  document call, so the flag can still be false at the moment the event arrives;
- while `_document:hasPendingRerendering()` says the worker still has chunks, it
  polls once a second (reading the page count also installs the ready chunks, so
  the pass advances while the reader is idle) and sends one `DocumentRerendered`
  when it is over, so `ReaderToc` drops its cached page numbers (the footer's
  marker rebuild then fills it with the final ones) and `ReaderUI` saves the
  final page count with the book.

`hasPendingRerendering()` is reached through `document._document`: adding it to
the `CreDocument` wrapper would mean editing `frontend/document/credocument.lua`,
which is exactly what this plugin avoids.

### Install

Copy the directory into KOReader's `plugins/` next to the other `.koplugin`
directories and restart KOReader:

```sh
cp -r tools/koreader-plugins/crebackgroundlayout.koplugin <koreader>/plugins/
```

It shows up in *Tools → Plugin management* and can be disabled there. The engine
keeps working without it; the footer and the TOC just stop following a
background pass.

It needs a `libkoreader-cre.so` that reports `hasPendingRerendering()`
(the 2026.11 builds and later). On an engine without it the plugin is inert.

### Diagnostics

`CREBG_LOG=1` (environment of the reader process) writes one line per decision to
KOReader's log:

```
crebackgroundlayout: page total 2003 -> 1981
crebackgroundlayout: pass started
crebackgroundlayout: pass finished, total 1977
```

If a page total "does not change", those lines show whether the pass was noticed,
whether it was polled, and whether the finishing event was sent.

### Why it is a plugin and not a patch

The engine can only answer questions; it cannot push an event into KOReader. The
plugin supplies the three lines of glue that KOReader's own partial-rerendering
machinery assumes an engine-side reload provides, and it does so through the same
public events (`TocReset`, `DocumentRerendered`) that KOReader already listens
to. `docs/PAGE-NUMBERS.md` in the cre-rust tree walks through the mechanism.

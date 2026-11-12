# Page numbers while the book is still being laid out

A settings change does not reopen the book: the engine lays out the chunk under
the reader immediately and the rest on a worker thread, in reading order (see
"MEMORY.md"). Page numbers are a prefix sum of chunk pages, so **the page total
and every page number move while that pass runs**. This document is about the
other half: how KOReader learns about it, and why that half is a plugin rather
than a patch.

## The engine cannot tell KOReader anything

`libkoreader-cre.so` implements KOReader's `cre` API: every function is a
question KOReader asks (`getPages`, `getTOC`, `getPageFromXPointer`, …). There
is no "callback into the reader" in that API, and adding one would not help
either, because the reader modules do not listen to the engine; they listen to
KOReader events.

So the engine can only be *asked*. The question is which questions KOReader asks
and when.

## What KOReader caches, and when it re-reads it

Two caches matter:

- **`ReaderFooter.pages`** is the total shown as `123 / 4567`. It is read once at
  reader-ready and then re-read **only** in `ReaderFooter:onTocReset()`, which
  runs when the `TocReset` event arrives.
- **`ReaderToc.toc`** holds the TOC entries together with the page each one was
  on. `ReaderToc:fillToc()` fills it from `document:getToc()` the first time the
  menu is opened and then keeps it; `ReaderToc:resetToc()` (called from
  `onDocumentRerendered`) is what throws it away.

`TocReset` is emitted by `ReaderToc:onUpdateToc()`, which `onDocumentRerendered`
calls. And `DocumentRerendered` is emitted by `ReaderRolling:updatePos()` — on a
settings change, once. So:

| Event | Who sends it | What it refreshes |
|---|---|---|
| `DocumentRerendered` | `ReaderRolling:updatePos` at the settings change | TOC cache (through `TocReset`), footer total, saved `doc_pages` |
| `TocReset` | `ReaderToc:onUpdateToc` | footer total, footer progress markers |
| `PageUpdate` | `ReaderRolling:handlePartialRerendering` after a draw | footer's current page (not the total) |

crengine gets a *second* `DocumentRerendered` when its background subprocess
finishes, because it reloads the document. This engine has nothing to reload: the
chunks are already in memory and the pass merely installs them, so without help
the footer keeps the number it happened to read at the settings change, and the
TOC keeps the page numbers it read when it was first opened.

That is not a corner case. Measured on a 320-chunk EPUB: the footer stayed on
2003 pages while the layout settled at 1977 — and when the chunk under the reader
keeps its page count (a part-title image page does, at any font size), the
estimate is exactly the old total and the reader sees no change at all.

## The plugin

`tools/koreader-plugins/crebackgroundlayout.koplugin` supplies the missing
notifications. It uses nothing but events and two document calls
(`getPageCount()`, `hasPendingRerendering()`), so KOReader stays unpatched.

### Asking the engine where it is

`hasPendingRerendering()` (cre-lua) answers whether the worker still has chunks
to lay out. It is reached through `document._document` because adding it to the
`CreDocument` wrapper would mean editing `frontend/document/credocument.lua`,
which is what the plugin exists to avoid.

### Reading the page count is not a passive query

The engine applies a deferred settings change on the next document call, and it
installs the chunk layouts the worker finished on the reader's thread. Both
happen inside a plain `getPageCount()`:

- a deferred relayout is applied, which starts the worker;
- finished chunks are installed, which rebuilds the page table.

That is why polling once a second is enough to make progress even while the
reader is idle, and why the plugin reads the page count **before** it trusts
`hasPendingRerendering()`: at the moment `DocumentRerendered` arrives the worker
may not have started yet, the flag reads `false`, and a watcher that trusted it
would never look again.

### The three behaviours

1. **Follow the total.** On every `PageUpdate` / `PosUpdate`, re-read the page
   total; if it moved, send `TocReset`. `ReaderFooter:onTocReset()` then re-reads
   it, rebuilds its progress markers and repaints.

   That event is the documented path, but it is not enough on every build: the
   footer lives on `ReaderView` (`self.ui.view.footer`), and an event that walks
   the window stack does not always reach it. Reported from a device: the current
   page followed the pass, the total did not. So the plugin also writes
   `footer.pages` and calls `footer:onUpdateFooter()` when the event did not move
   it -- exactly what `onTocReset` would have done. The `CREBG_LOG=1` line
   `publish <total> footer before <a> after event <b> now <c>` shows which path
   did the work.
2. **Watch the pass.** After `onReaderReady` and after `DocumentRerendered`,
   check whether a pass is running (after the page-count read above) and poll
   once a second until it ends. Polling also advances the pass, so a reader who
   just sits there still converges.
3. **Announce the end.** When the pass is over, send exactly one
   `DocumentRerendered`. That is the event a full rerendering sends: `ReaderToc`
   drops its cache (so the TOC opens with final pages), the footer re-reads the
   total through `TocReset`, and `ReaderUI` saves the final page count with the
   book.

### Two details that make it reliable rather than lucky

- **Re-arm on every reader-ready.** A style change reloads the document, which
  fires `onReaderReady` again; a plugin that starts its watcher once leaves that
  style change unwatched — no finishing event, and the TOC keeps the page
  numbers it read at layout time. This was the first bug reported from the
  device ("the TOC pages only update after the open pass").
- **Observe before trusting the flag.** See above: the flag can legitimately be
  `false` when the event arrives. This was the second ("the footer total never
  changes after a relayout").

The plugin also de-duplicates the finishing event with a `pass_seen` flag, so
several settings changes in a row (each restarting the worker) still produce one
announcement per pass.

## What is exact, and what is not

- The **reading position** is an XPointer and is exact from the first draw: a
  restore or a TOC/bookmark jump lays out exactly the chunk it points into.
- The **TOC page numbers** are exact once the pass ends (the cache is dropped and
  refilled from the final layout).
- The **footer total and progress percentage** are estimates until the pass ends,
  which is the same degraded state crengine shows during its partial rerendering.
  The plugin makes them follow the pass instead of freezing.
- A **page-number jump** ("go to page 900") uses the estimate and lands near the
  target until the pass ends. XPointer jumps do not have this limitation.

## Diagnostics

`CREBG_LOG=1` in the reader's environment logs one line per decision:

```
crebackgroundlayout: page total 2003 -> 1981
crebackgroundlayout: pass started
crebackgroundlayout: pass finished, total 1977
```

No `pass started` after a settings change means the pass was not noticed (the
event or the flag); a `pass started` with no `pass finished` means polling
stopped early. Both are visible in KOReader's log without a debugger.

## Rebuilding the bridge without a plugin

If a future KOReader version grows an engine-side hook for this, the plugin can
disappear: the three behaviours above are what the reader needs, and they map
one-to-one onto `hasPendingRerendering()` plus the two events. Until then, the
plugin is the smallest possible patchless integration.

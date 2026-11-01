# TODO

This list is for API parity with `cre.cpp` and for reader features that
KOReader needs.

The method names are complete. The main work is the behavior behind the names.

## P0: behavior that KOReader needs

### Fonts

- Test `getFontFaces`, `getFontFaceFilenameAndFaceIndex`,
  `getFontFaceAvailableWeights`, and `registerFont` with more fonts.
- Keep the face index and the family name together. A collection has several
  families in one file.
- Test `setAsPreferredFontWithBias` and `regularizeRegisteredFontsWeights`
  against cre.cpp output.

### Word selection and dictionary

- `getNearestWordFromPosition`: use the word set of the visible page and the
  direction argument. Return `text`, `pos0`, and `pos1`.
- `getWordFromPosition`: confirm the coordinate frame (screen or document).
- `getTextFromPositions`: support segmented selection and the image
  placeholder character.
- `getWordBoxesFromPositions`: match the cre.cpp word boundary rules and image
  segments.
- `clearSelection`: clear all selection and render state.

### Page map and TOC

- `buildAlternativeToc`: build a real alternative TOC. Do not only set a flag.
- `getToc`: do not invent page 1 when a target cannot be resolved.
- Test document-provided page maps with more books.

### Page flow and visible pages

- `setVisiblePageCount`: change the layout and the page map. Do not only store
  the value.
- `setViewMode`: change the layout mode (page, two-page, scroll).
- `getVisiblePageNumberCount`: match the cre.cpp meaning.

### Coordinates, progress, and CFI

- `getFullHeight`: return the full document layout height, not an estimate.
- `getPosFromXPointer`, `getCurrentPos`, `getPageStartY`, and
  `getPageHeight`: confirm the cre.cpp coordinate frame.
- `getEPubCFI`, `getPageEPubCFI`, and the range functions: produce real EPUB
  CFI strings if KOReader stores them.
- `getNormalizedXPointer`: return a real DOM XPointer.

### Callbacks

- Return the same callback arguments and order as cre.cpp.
- Test `setCallback` and the document close path.

## P1: feature parity

### Embedded fonts and CSS

- Use EPUB embedded fonts (`@font-face`) or report that they are ignored.
- Apply `crengine.doc.embedded.styles.enabled` to the chunk HTML.
- Support more CSS properties as Blitz adds them.

### Links and footnotes

- Test footnote links and internal links.
- Keep link rectangles equal to the painted text.
- Support the image link case.

### Document properties

- Return the same types and defaults as cre.cpp for all properties.
- Test the properties that KOReader saves in the book settings.

### Cache and partial rerender

- Add an optional on-disk cache. A reopen must not paginate the whole book.
- Support the DOM version and cache file APIs, or report "no cache" in a
  consistent way.

### Header, footer, and statistics

- Test the header and footer layout at all screen sizes.
- Return correct statistics and unknown-entity data.

## P2: later work

- Hyphenation. The dictionary name is stored, but no hyphenation is applied.
- Gamma, glyph image replacement, and global font bias.
- Image APIs: page images, image at position, image cache control.
- Text search performance on very large books.

## Engine gaps that Blitz owns

- Vertical writing (`writing-mode`). Blitz supports block-level layout only.
- Full bidi control (`unicode-bidi`). Blitz supports text reordering but not
  all controls.
- Table layout. Blitz emulates tables with CSS Grid.
- Some CSS properties. See <https://blitz.is/status/css>.

## Rules

- Keep the Lua API compatible with KOReader.
- Prefer an existing Blitz, Parley, or Stylo function.
- Do not hide a layout bug with a cache or an optimisation.
- Add a regression test for each bug.

# Test books

These books are test data. Do not use them in production.

| File | Source | Use |
|---|---|---|
| `koreader-test-data/juliet.epub` | *Romeo and Juliet*, Feedbooks, public domain | EPUB with a cover, many small spine items, TOC |
| `koreader-test-data/leaves.epub` | *Leaves of Grass*, Project Gutenberg | Large EPUB with images and chapters |
| `koreader-test-data/links.epub` | Synthetic | Internal links and footnotes |
| `koreader-test-data/sample.fb2` | Synthetic | FB2 |
| `koreader-test-data/sample.txt` | Synthetic | Plain text |

`crates/cre-lib/tests/fixtures/` holds small EPUB and HTML files that the tests
make or copy. They are part of the test code.

The tests look for the books in `fixtures/koreader-test-data/`. Keep the books
in place. A missing book stops the test that needs it. A few unit tests print a
skip message instead.

MOBI and KF8 tests need a real file. Set `CRE_MOBI_TEST_FILE` to use your own
file:

```sh
CRE_MOBI_TEST_FILE=/path/to/book.azw3 cargo test -p cre-lib --test mobi_kf8
```

Check the license of a test book before you publish it again.

use std::path::{Path, PathBuf};

use cre_lib::{Document, DocumentFormat, DocumentOptions, PagePosition};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/koreader-test-data")
        .join(name)
}

fn require_resources() {
    let font = Path::new("/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf");
    assert!(
        font.exists(),
        "missing required test font: {}",
        font.display()
    );
    for name in ["juliet.epub", "links.epub", "sample.fb2", "sample.txt"] {
        let path = fixture(name);
        assert!(
            path.exists(),
            "missing required fixture: {}",
            path.display()
        );
    }
}

fn has_ink(bitmap: &cre_lib::PageBitmap) -> bool {
    bitmap.data.iter().any(|value| *value < 128)
}

#[test]
fn document_exposes_juliet_metadata_pages_and_toc() {
    require_resources();

    let mut doc = Document::open(fixture("juliet.epub")).unwrap();
    let metadata = doc.metadata();
    assert_eq!(metadata.format, DocumentFormat::Epub);
    assert_eq!(metadata.title.as_deref(), Some("Romeo and Juliet"));

    let page_count = doc.page_count();
    assert!(page_count > 10);

    let toc = doc.toc();
    assert!(toc.len() > 20);
    assert!(toc.iter().all(|entry| entry.page.is_some()));

    let first = toc[0].clone();
    let second = toc
        .iter()
        .find(|entry| entry.page != first.page)
        .unwrap()
        .clone();
    let first_xpointer = first.xpointer.clone().unwrap();
    let second_xpointer = second.xpointer.clone().unwrap();
    let resolved = doc.page_from_href(&first.href).unwrap();
    assert_eq!(resolved, first.page.unwrap());
    assert_eq!(doc.go_to_href(&first.href).unwrap(), resolved);
    assert_eq!(
        doc.compare_xpointers(&first_xpointer, &second_xpointer)
            .unwrap(),
        1
    );
    assert_eq!(
        doc.compare_xpointers(&second_xpointer, &first_xpointer)
            .unwrap(),
        -1
    );
    assert_eq!(
        doc.compare_xpointers(&first_xpointer, &first_xpointer)
            .unwrap(),
        0
    );

    let first_bitmap = doc.render_page(1).unwrap();
    assert_eq!(first_bitmap.width, 600);
    assert_eq!(first_bitmap.height, 800);
    assert_eq!(first_bitmap.data.len(), 600 * 800);
    assert!(has_ink(&first_bitmap));

    doc.go_to_page(2).unwrap();
    assert_eq!(doc.current_page(), 2);
    assert_eq!(doc.go_to_previous_page(), 1);
    assert_eq!(doc.go_to_next_page(), 2);
}

#[test]
fn document_exposes_sample_text_pages() {
    require_resources();

    let mut doc = Document::open(fixture("sample.txt")).unwrap();
    assert_eq!(doc.metadata().format, DocumentFormat::Txt);
    assert!(doc.page_count() >= 1);

    let bitmap = doc.render_page(1).unwrap();
    assert_eq!(bitmap.width, 600);
    assert_eq!(bitmap.height, 800);
    assert_eq!(bitmap.data.len(), 600 * 800);
    assert!(has_ink(&bitmap));
}

#[test]
fn document_exposes_fb2_metadata_text_and_images() {
    require_resources();

    let mut doc = Document::open(fixture("sample.fb2")).unwrap();
    let metadata = doc.metadata();
    assert_eq!(metadata.format, DocumentFormat::Fb2);
    assert_eq!(metadata.title.as_deref(), Some("FB2 Fixture"));
    assert_eq!(metadata.creator.as_deref(), Some("Test M. Author"));
    assert_eq!(metadata.language.as_deref(), Some("en"));
    assert_eq!(metadata.item_count, 1);

    assert!(doc.page_count() >= 1);
    let text = doc.page_text(1).unwrap();
    assert!(text.contains("FB2 Fixture"));
    assert!(text.contains("Chapter One"));
    assert!(text.contains("Annotation paragraph"));

    let bitmap = doc.render_page(1).unwrap();
    assert_eq!(bitmap.width, doc.page_width());
    assert_eq!(bitmap.height, doc.page_height());
    assert!(has_ink(&bitmap));
}

#[test]
fn document_resolves_fb2_fragment_links() {
    require_resources();

    let doc = Document::open(fixture("sample.fb2")).unwrap();
    let hits = doc.find_text("Jump").unwrap();
    assert!(!hits.is_empty());

    let hit = &hits[0];
    let text_box = doc
        .page_text_boxes(hit.page)
        .unwrap()
        .into_iter()
        .find(|text_box| text_box.text.contains("Jump"))
        .unwrap();
    let link_hit = doc
        .hit_test(PagePosition {
            page: hit.page,
            x: text_box.x + 1.0,
            y: text_box.y + text_box.height / 2.0,
        })
        .unwrap()
        .unwrap();
    assert_eq!(link_hit.link.as_deref(), Some("#chapter-two"));

    let target_page = doc.page_from_href("#chapter-two").unwrap();
    assert!(
        doc.page_text(target_page)
            .unwrap()
            .contains("Target chapter")
    );
}

#[test]
fn document_exposes_fb2_section_toc() {
    require_resources();

    let mut doc = Document::open(fixture("sample.fb2")).unwrap();
    assert!(!doc.toc().is_empty());

    let (href, entry_page, xpointer) = {
        let entry = doc
            .toc()
            .iter()
            .find(|entry| entry.title == "Chapter One")
            .unwrap();
        assert_eq!(entry.href, "#fb2-body-0");
        assert_eq!(entry.depth, 1);
        (
            entry.href.clone(),
            entry.page.unwrap(),
            entry.xpointer.clone().unwrap(),
        )
    };
    let page = entry_page;
    assert!(page >= 1);
    assert!(doc.page_text(page).unwrap().contains("Chapter One"));

    assert_eq!(doc.page_from_xpointer(&xpointer).unwrap(), page);
    assert_eq!(doc.go_to_href(&href).unwrap(), page);
    assert_eq!(doc.go_to_xpointer(&xpointer).unwrap(), page);
}

#[test]
fn document_extracts_page_text_and_text_boxes() {
    require_resources();

    let doc = Document::open(fixture("sample.txt")).unwrap();
    let text = doc.page_text(1).unwrap();
    assert!(text.contains("Lorem ipsum"));
    assert!(text.contains("foo@bar.com"));

    let boxes = doc.page_text_boxes(1).unwrap();
    assert!(!boxes.is_empty());
    for text_box in &boxes {
        assert_eq!(text_box.page, 1);
        assert!(text_box.width >= 0.0);
        assert!(text_box.height > 0.0);
        assert!(text_box.x >= 0.0);
        assert!(text_box.y >= 0.0);
        assert!(!text_box.text.trim().is_empty());
    }

    let joined = boxes
        .iter()
        .map(|text_box| text_box.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(joined, text);

    let first = &boxes[0];
    let hit = doc
        .text_at(PagePosition {
            page: 1,
            x: first.x + 1.0,
            y: first.y + first.height / 2.0,
        })
        .unwrap();
    assert_eq!(hit.as_deref(), Some(first.text.as_str()));

    let miss = doc
        .text_at(PagePosition {
            page: 1,
            x: -100.0,
            y: -100.0,
        })
        .unwrap();
    assert!(miss.is_none());

    assert!(doc.page_text(0).is_err());
    assert!(doc.page_text(doc.page_count() + 1).is_err());
    assert!(doc.page_text_boxes(0).is_err());
}

#[test]
fn document_hit_tests_text_positions_and_links() {
    require_resources();

    let doc = Document::open(fixture("sample.txt")).unwrap();
    let boxes = doc.page_text_boxes(1).unwrap();
    let first = &boxes[0];

    let hit = doc
        .hit_test(PagePosition {
            page: 1,
            x: first.x + 2.0,
            y: first.y + first.height / 2.0,
        })
        .unwrap()
        .unwrap();
    assert_eq!(hit.page, 1);
    assert_eq!(hit.text, first.text);
    assert_eq!(hit.rect, *first);
    assert!(hit.link.is_none());
    assert_eq!(
        doc.page_from_xpointer(&hit.text_position.to_xpointer())
            .unwrap(),
        1
    );

    let miss = doc
        .hit_test(PagePosition {
            page: 1,
            x: -100.0,
            y: -100.0,
        })
        .unwrap();
    assert!(miss.is_none());

    assert!(
        doc.hit_test(PagePosition {
            page: 0,
            x: 0.0,
            y: 0.0,
        })
        .is_err()
    );
}

#[test]
fn document_extracts_word_at_position_and_range_boxes() {
    require_resources();

    let doc = Document::open(fixture("sample.txt")).unwrap();
    let boxes = doc.page_text_boxes(1).unwrap();

    let mut found = None;
    for text_box in &boxes {
        let y = text_box.y + text_box.height / 2.0;
        let mut x = text_box.x;
        while x <= text_box.x + text_box.width {
            if let Some(word) = doc.word_at(PagePosition { page: 1, x, y }).unwrap()
                && word.text == "Lorem"
            {
                found = Some(word);
                break;
            }
            x += 2.0;
        }
        if found.is_some() {
            break;
        }
    }

    let word = found.expect("word 'Lorem' was not found by hit testing");
    assert!(!word.rects.is_empty());
    assert!(word.rects.iter().all(|rect| rect.page == 1));
    assert!(
        word.rects
            .iter()
            .all(|rect| rect.width > 0.0 && rect.height > 0.0)
    );
    assert_eq!(
        doc.page_from_xpointer(&word.xpointer_range.start.to_xpointer())
            .unwrap(),
        1
    );
    let word_end = word.xpointer_range.end.as_ref().unwrap().to_xpointer();
    assert_eq!(doc.page_from_xpointer(&word_end).unwrap(), 1);
    assert_eq!(
        doc.text_between_xpointers(&word.xpointer_range.start.to_xpointer(), &word_end)
            .unwrap(),
        word.text
    );
    assert!(
        doc.text_from_xpointer(&word.xpointer_range.start.to_xpointer())
            .unwrap()
            .contains("Lorem")
    );

    let range_boxes = doc
        .boxes_between_xpointers(
            &word.xpointer_range.start.to_xpointer(),
            &word.xpointer_range.end.as_ref().unwrap().to_xpointer(),
        )
        .unwrap();
    assert!(!range_boxes.is_empty());
    assert!(range_boxes.iter().all(|rect| rect.page == 1));
}

#[test]
fn document_finds_nearest_word_in_requested_directions() {
    require_resources();

    let doc = Document::open(fixture("sample.txt")).unwrap();
    let boxes = doc.page_text_boxes(1).unwrap();

    let mut target = None;
    for text_box in &boxes {
        let y = text_box.y + text_box.height / 2.0;
        let mut x = text_box.x;
        while x <= text_box.x + text_box.width {
            if let Some(word) = doc.word_at(PagePosition { page: 1, x, y }).unwrap() {
                target = Some(word);
                break;
            }
            x += 2.0;
        }
        if target.is_some() {
            break;
        }
    }

    let word = target.expect("page 1 has no word hit target");
    let rect = word.rects.first().expect("word has no rect");
    let center_y = rect.y + rect.height / 2.0;
    let target_x = rect.x + rect.width / 2.0;

    let left_point_x = rect.x.max(5.0) - 5.0;
    let right_of_target = doc
        .nearest_word_at(
            PagePosition {
                page: 1,
                x: left_point_x,
                y: center_y,
            },
            &[1],
            2,
        )
        .unwrap()
        .expect("no word to the right");
    let right_rect = right_of_target.rects.first().expect("word has no rect");
    assert!(right_rect.x + 1.0 >= left_point_x);
    assert!(
        (right_rect.x + right_rect.width / 2.0 - left_point_x) <= (target_x - left_point_x) + 1.0
    );

    let right_point_x = rect.x + rect.width + 5.0;
    let left_of_target = doc
        .nearest_word_at(
            PagePosition {
                page: 1,
                x: right_point_x,
                y: center_y,
            },
            &[1],
            0,
        )
        .unwrap()
        .expect("no word to the left");
    let left_rect = left_of_target.rects.first().expect("word has no rect");
    assert!(left_rect.x + left_rect.width <= right_point_x + 1.0);
    assert!(
        (right_point_x - (left_rect.x + left_rect.width / 2.0)) <= (right_point_x - target_x) + 1.0
    );

    assert!(
        doc.nearest_word_at(
            PagePosition {
                page: 1,
                x: 0.0,
                y: 0.0,
            },
            &[],
            4,
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn document_maps_xpointer_to_page_coordinates() {
    require_resources();

    let doc = Document::open(fixture("sample.txt")).unwrap();
    let xpointer = doc.xpointer_from_page(1).unwrap();
    let (x, y) = doc
        .point_from_xpointer(&xpointer)
        .unwrap()
        .expect("page xpointer should map to a page coordinate");

    let boxes = doc.page_text_boxes(1).unwrap();
    let first = boxes.first().expect("page 1 has no text boxes");
    assert!(y >= first.y && y < first.y + first.height);
    assert!(x >= first.x && x <= first.x + first.width);
}

#[test]
fn document_hit_tests_internal_epub_links() {
    require_resources();

    let doc = Document::open(fixture("links.epub")).unwrap();
    assert_eq!(doc.metadata().title.as_deref(), Some("Links Test"));

    let boxes = doc.page_text_boxes(1).unwrap();
    let link_box = boxes
        .iter()
        .find(|text_box| text_box.text.contains("Jump target"))
        .unwrap();

    let y = link_box.y + link_box.height / 2.0;
    let mut links = Vec::new();
    let mut x = link_box.x;
    while x <= link_box.x + link_box.width {
        if let Some(hit) = doc.hit_test(PagePosition { page: 1, x, y }).unwrap()
            && let Some(link) = hit.link
        {
            links.push(link);
        }
        x += 1.0;
    }

    assert!(!links.is_empty(), "no links found");
    assert!(
        links.iter().all(|link| link.ends_with("ch2.xhtml#target")),
        "{links:?}"
    );
    assert_eq!(
        doc.page_from_href(&links[0]).unwrap(),
        doc.toc()[1].page.unwrap()
    );

    let page_links = doc.page_links(1, true).unwrap();
    assert!(!page_links.is_empty());
    for link in &page_links {
        assert!(link.internal);
        assert!(link.href.ends_with("ch2.xhtml#target"));
        assert!(!link.rects.is_empty());
        for rect in &link.rects {
            assert_eq!(rect.page, 1);
            assert!(rect.width > 0.0 && rect.height > 0.0);
        }
    }
    assert_eq!(
        doc.page_from_xpointer(&page_links[0].href).unwrap(),
        doc.toc()[1].page.unwrap()
    );
}

#[test]
fn document_handles_large_epub() {
    require_resources();
    let path = fixture("leaves.epub");
    assert!(
        path.exists(),
        "missing required fixture: {}",
        path.display()
    );

    let mut doc = Document::open(&path).unwrap();
    assert_eq!(doc.metadata().format, DocumentFormat::Epub);
    let page_count = doc.page_count();
    assert!(page_count > 100);

    let mut first_text_page = None;
    for page in 1..=page_count {
        if !doc.page_text(page).unwrap().trim().is_empty() {
            first_text_page = Some(page);
            break;
        }
    }
    let first_text_page = first_text_page.expect("large EPUB should contain a text page");

    let mut last_sample = 0;
    for page in [
        first_text_page,
        page_count / 3,
        page_count / 2,
        (page_count * 2) / 3,
        page_count,
    ] {
        let page = page.max(first_text_page);
        if page == last_sample {
            continue;
        }
        last_sample = page;

        let text = doc.page_text(page).unwrap();
        assert!(!text.trim().is_empty(), "page {page} is blank");

        let bitmap = doc.render_page(page).unwrap();
        assert_eq!(bitmap.width, doc.page_width());
        assert_eq!(bitmap.height, doc.page_height());
        assert!(has_ink(&bitmap), "page {page} has no rendered ink");
    }
}

#[test]
fn document_extracts_epub_page_text() {
    require_resources();

    let doc = Document::open(fixture("juliet.epub")).unwrap();
    let page = doc.page_count() / 2;
    let text = doc.page_text(page).unwrap();
    assert!(!text.trim().is_empty());

    let boxes = doc.page_text_boxes(page).unwrap();
    assert!(!boxes.is_empty());
    assert!(boxes.iter().all(|text_box| text_box.page == page));
    assert!(boxes.iter().all(|text_box| text_box.y >= 0.0));
    assert!(
        boxes
            .iter()
            .all(|text_box| text_box.y + text_box.height <= 800.0)
    );
}

#[test]
fn document_finds_text_with_xpointer_ranges_and_rects() {
    require_resources();

    let doc = Document::open(fixture("sample.txt")).unwrap();
    let hits = doc.find_text("foo@bar.com").unwrap();
    assert!(!hits.is_empty());

    let hit = &hits[0];
    assert_eq!(hit.page, 1);
    assert!(!hit.rects.is_empty());
    assert!(hit.rects.iter().all(|text_box| text_box.page == 1));
    assert!(
        hit.rects
            .iter()
            .any(|text_box| text_box.text.contains("foo@bar.com"))
    );
    assert!(hit.xpointer_range.end.is_some());

    let start = hit.xpointer_range.start.to_xpointer();
    assert_eq!(doc.page_from_xpointer(&start).unwrap(), 1);

    let all_lorem = doc.find_text("Lorem ipsum").unwrap();
    assert!(all_lorem.len() >= 2);
    assert!(
        all_lorem
            .windows(2)
            .all(|pair| pair[0].page <= pair[1].page)
    );

    let first_page = doc.page_text_boxes(1).unwrap();
    let spanning = format!("{}\n{}", first_page[0].text, first_page[1].text);
    let spanning_hits = doc.find_text(&spanning).unwrap();
    assert!(!spanning_hits.is_empty());
    assert_eq!(spanning_hits[0].rects.len(), 2);

    assert!(doc.find_text("").is_err());
    assert!(doc.find_text("zzzz-not-in-sample").unwrap().is_empty());
}

#[test]
fn document_finds_text_with_regex_and_case_insensitive_options() {
    require_resources();

    let doc = Document::open(fixture("sample.txt")).unwrap();
    let literal_hits = doc.find_text("foo@bar.com").unwrap();
    assert!(!literal_hits.is_empty());

    let regex_hits = doc
        .find_text_with(r"foo@\w+\.com", false, true, Some(1))
        .unwrap();
    assert_eq!(regex_hits.len(), 1);
    assert_eq!(regex_hits[0].page, literal_hits[0].page);
    assert_eq!(
        regex_hits[0].xpointer_range.start,
        literal_hits[0].xpointer_range.start
    );

    let case_hits = doc
        .find_text_with("FOO@BAR.COM", true, false, Some(1))
        .unwrap();
    assert_eq!(case_hits.len(), 1);
    assert_eq!(case_hits[0].page, literal_hits[0].page);
    assert_eq!(
        case_hits[0].xpointer_range.start,
        literal_hits[0].xpointer_range.start
    );

    assert!(doc.find_text_with("(", false, true, None).is_err());
}

#[test]
fn document_finds_text_with_search_flags() {
    require_resources();

    let doc = Document::open(fixture("sample.txt")).unwrap();

    assert!(
        doc.find_text_with("99..September", false, true, None)
            .unwrap()
            .is_empty()
    );
    assert!(
        !doc.find_text_with_search_flags("99..September", false, true, 32, None)
            .unwrap()
            .is_empty()
    );

    assert!(
        doc.find_text_with("^September", false, true, None)
            .unwrap()
            .is_empty()
    );
    assert!(
        !doc.find_text_with_search_flags("^September", false, true, 8, None)
            .unwrap()
            .is_empty()
    );

    assert!(
        doc.find_text_with(r"foo @bar .com", false, true, None)
            .unwrap()
            .is_empty()
    );
    assert!(
        !doc.find_text_with_search_flags("foo @bar .com", false, true, 4, None)
            .unwrap()
            .is_empty()
    );

    assert!(
        doc.find_text_with("FOO@BAR\\.COM", false, true, None)
            .unwrap()
            .is_empty()
    );
    assert!(
        !doc.find_text_with_search_flags("FOO@BAR\\.COM", false, true, 2, None)
            .unwrap()
            .is_empty()
    );

    assert!(
        !doc.find_text_with("foo.bar", false, true, None)
            .unwrap()
            .is_empty()
    );
    assert!(
        doc.find_text_with_search_flags("foo.bar", false, true, 16, None)
            .unwrap()
            .is_empty()
    );
    assert!(
        doc.find_text_with_search_flags("(", false, true, 16, None)
            .is_ok()
    );
}

#[test]
fn document_find_text_direction_filters_by_current_page() {
    require_resources();

    let options = DocumentOptions {
        width: 320,
        height: 480,
        layout_height: 100_000,
        page_margin: 16.0,
        font_size: 18.0,
        font_path: None,
    };
    let mut doc = Document::with_options(fixture("sample.txt"), &options).unwrap();
    assert!(doc.page_count() > 3);

    let mut selected = None;
    for query in ["et", "at", "ex", "consetetur", "iusto"] {
        let hits = doc.find_text(query).unwrap();
        let mut pages: Vec<usize> = hits.iter().map(|hit| hit.page).collect();
        pages.sort_unstable();
        pages.dedup();
        if pages.len() >= 2 {
            selected = Some((query, hits));
            break;
        }
    }
    let (query, all) = selected.expect("fixture should repeat text on multiple pages");

    let mut pages: Vec<usize> = all.iter().map(|hit| hit.page).collect();
    pages.sort_unstable();
    pages.dedup();
    let boundary = pages[1];

    doc.go_to_page(boundary).unwrap();
    let forward = doc
        .find_text_with_direction(query, false, false, 0, false, None)
        .unwrap();
    assert!(!forward.is_empty());
    assert!(forward.iter().all(|hit| hit.page >= boundary));
    assert!(forward.windows(2).all(|pair| pair[0].page <= pair[1].page));

    let backward = doc
        .find_text_with_direction(query, false, false, 0, true, None)
        .unwrap();
    assert!(!backward.is_empty());
    assert!(backward.iter().all(|hit| hit.page <= boundary));
    assert!(backward.windows(2).all(|pair| pair[0].page >= pair[1].page));

    let before = doc
        .find_text_with_direction(query, false, false, -1, false, None)
        .unwrap();
    assert!(!before.is_empty());
    assert!(before.iter().all(|hit| hit.page <= boundary));

    let after_reverse = doc
        .find_text_with_direction(query, false, false, -1, true, None)
        .unwrap();
    assert!(!after_reverse.is_empty());
    assert!(after_reverse.iter().all(|hit| hit.page >= boundary));
    assert!(
        after_reverse
            .windows(2)
            .all(|pair| pair[0].page >= pair[1].page)
    );

    assert!(
        doc.find_text_with_direction("", false, false, 0, false, None)
            .is_err()
    );
    assert_eq!(
        doc.find_text_with_direction("zzzz-not-in-sample", false, false, 0, false, Some(1))
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn document_bookmarks_survive_reflow() {
    require_resources();

    let narrow_options = DocumentOptions {
        width: 320,
        height: 480,
        layout_height: 100_000,
        page_margin: 16.0,
        font_size: 18.0,
        font_path: None,
    };
    let narrow = Document::with_options(fixture("sample.txt"), &narrow_options).unwrap();
    assert!(narrow.page_count() > 3);
    let page = narrow.page_count() / 2;

    let bookmark = narrow.bookmark_page(page).unwrap();
    assert_eq!(bookmark.page, page);
    assert!(bookmark.xpointer.is_some());
    let page_start = narrow
        .page_text(page)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();

    let wide_options = DocumentOptions {
        width: 700,
        height: 900,
        page_margin: 32.0,
        ..narrow_options
    };
    let mut wide = Document::with_options(fixture("sample.txt"), &wide_options).unwrap();
    let mapped = wide.go_to_bookmark(&bookmark).unwrap();
    assert!(
        wide.page_text(mapped).unwrap().contains(&page_start),
        "bookmark moved to page {mapped} without restoring {page_start:?}"
    );

    let invalid = cre_lib::Bookmark {
        page: mapped,
        xpointer: Some("xpointer(not-a-valid-path)".to_string()),
    };
    assert_eq!(wide.go_to_bookmark(&invalid).unwrap(), mapped);
    assert!(narrow.bookmark_page(0).is_err());
}

#[test]
fn document_selection_spans_pages() {
    require_resources();

    let options = DocumentOptions {
        width: 360,
        height: 480,
        layout_height: 100_000,
        page_margin: 16.0,
        font_size: 18.0,
        font_path: None,
    };
    let doc = Document::with_options(fixture("sample.txt"), &options).unwrap();
    assert!(doc.page_count() > 3);

    let first_page = doc.page_text_boxes(1).unwrap();
    let second_page = doc.page_text_boxes(2).unwrap();
    let start_box = first_page.last().unwrap();
    let end_box = second_page[1].clone();
    let start = PagePosition {
        page: 1,
        x: start_box.x + 1.0,
        y: start_box.y + start_box.height / 2.0,
    };
    let end = PagePosition {
        page: 2,
        x: end_box.x + end_box.width / 2.0,
        y: end_box.y + end_box.height / 2.0,
    };

    let selection = doc.selection(start, end).unwrap().unwrap();
    assert!(!selection.text.trim().is_empty());
    assert_eq!(
        doc.text_between_xpointers(
            &selection.xpointer_range.start.to_xpointer(),
            &selection.xpointer_range.end.as_ref().unwrap().to_xpointer(),
        )
        .unwrap(),
        selection.text
    );
    assert!(selection.rects.iter().any(|text_box| text_box.page == 1));
    assert!(selection.rects.iter().any(|text_box| text_box.page == 2));
    assert_eq!(
        doc.page_from_xpointer(&selection.xpointer_range.start.to_xpointer())
            .unwrap(),
        1
    );
    assert_eq!(
        doc.page_from_xpointer(&selection.xpointer_range.end.as_ref().unwrap().to_xpointer())
            .unwrap(),
        2
    );

    let reversed = doc.selection(end, start).unwrap().unwrap();
    assert_eq!(reversed.xpointer_range, selection.xpointer_range);
    assert_eq!(reversed.rects, selection.rects);
}

#[test]
fn document_rejects_invalid_layout_options() {
    let input = fixture("sample.txt");

    let width_zero = DocumentOptions {
        width: 0,
        ..Default::default()
    };
    assert!(Document::with_options(&input, &width_zero).is_err());

    let height_zero = DocumentOptions {
        height: 0,
        ..Default::default()
    };
    assert!(Document::with_options(&input, &height_zero).is_err());

    let layout_zero = DocumentOptions {
        layout_height: 0,
        ..Default::default()
    };
    assert!(Document::with_options(&input, &layout_zero).is_err());
}

#[test]
fn document_rejects_missing_files() {
    let missing = fixture("does-not-exist.txt");
    assert!(Document::open(&missing).is_err());
}

#[test]
fn document_rejects_invalid_page_numbers() {
    require_resources();

    let mut doc = Document::open(fixture("sample.txt")).unwrap();
    let page_count = doc.page_count();
    assert!(page_count >= 1);

    assert!(doc.render_page(0).is_err());
    assert!(doc.render_page(page_count + 1).is_err());
    assert!(doc.go_to_page(0).is_err());
    assert!(doc.go_to_page(page_count + 1).is_err());

    doc.go_to_last_page();
    assert_eq!(doc.current_page(), page_count);
    doc.go_to_first_page();
    assert_eq!(doc.current_page(), 1);
}

#[test]
fn document_rejects_invalid_xpointers() {
    require_resources();

    let mut doc = Document::open(fixture("sample.txt")).unwrap();
    let invalid_cases = [
        "",
        "not-an-xpointer",
        "/body/DocFragment[999]/body/text().0",
        "xpointer(/body/DocFragment[999]/body/text().0)",
    ];
    for xpointer in invalid_cases {
        assert!(
            doc.page_from_xpointer(xpointer).is_err(),
            "expected {xpointer:?} to be rejected"
        );
        assert!(
            doc.go_to_xpointer(xpointer).is_err(),
            "expected {xpointer:?} navigation to be rejected"
        );
    }
}

#[test]
fn document_rejects_missing_navigation_target() {
    require_resources();

    let mut doc = Document::open(fixture("sample.txt")).unwrap();
    assert!(
        doc.page_from_href("missing-chapter.html#missing-anchor")
            .is_err()
    );
    assert!(
        doc.go_to_href("missing-chapter.html#missing-anchor")
            .is_err()
    );
}

#[test]
fn document_page_xpointers_are_valid_and_round_trip() {
    require_resources();

    let doc = Document::open(fixture("juliet.epub")).unwrap();
    for page in 1..=doc.page_count() {
        let xpointer = doc.xpointer_from_page(page).unwrap();
        assert!(
            !xpointer.contains(&u32::MAX.to_string()),
            "page {page} produced sentinel offset: {xpointer}"
        );
        assert!(
            doc.page_from_xpointer(&xpointer).unwrap() == page,
            "page {page} xpointer did not round-trip: {xpointer}"
        );
    }
}

#[test]
fn document_exposes_resources_cover_and_html() {
    require_resources();

    let doc = Document::open(fixture("juliet.epub")).unwrap();
    assert!(!doc.has_non_linear_flows());
    let cover = doc.cover_image().expect("juliet.epub exposes a cover");
    assert!(!cover.is_empty());

    let image_page_xpointer = doc.xpointer_from_page(2).unwrap();
    assert!(
        doc.text_from_xpointer(&image_page_xpointer)
            .unwrap()
            .trim()
            .is_empty(),
        "page 2 is the image-only About page"
    );
    assert!(
        doc.html_from_xpointer(&image_page_xpointer)
            .unwrap()
            .is_some(),
        "image-only pages still resolve to HTML: {image_page_xpointer}"
    );

    let xpointer = (1..=doc.page_count())
        .map(|page| doc.xpointer_from_page(page).unwrap())
        .find(|xpointer| !doc.text_from_xpointer(xpointer).unwrap().trim().is_empty())
        .expect("juliet.epub has text pages");
    let text = doc.text_from_xpointer(&xpointer).unwrap();
    assert!(!text.trim().is_empty());
    let html = doc.html_from_xpointer(&xpointer).unwrap();
    assert!(html.is_some(), "expected HTML from {xpointer}");

    assert!(doc.next_visible_word_start(&xpointer).unwrap().is_some());
    assert!(doc.prev_visible_word_start(&xpointer).unwrap().is_some());
    assert!(doc.next_visible_char(&xpointer).unwrap().is_some());
    assert!(doc.prev_visible_char(&xpointer).unwrap().is_some());

    let segment = doc.extend_to_sentence(&xpointer, &xpointer).unwrap();
    assert!(segment.is_some(), "expected a sentence segment");
}

#[test]
fn renders_svg_image_to_rgba() {
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20">
        <rect width="10" height="20" fill="red"/>
    </svg>"#;

    let (pixels, width, height) = cre_lib::render_svg_image(svg, None, None).expect("renders SVG");
    assert_eq!((width, height), (10, 20));
    assert_eq!(pixels.len(), 10 * 20 * 4);
    assert!(
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[0] > 200 && pixel[1] < 50 && pixel[2] < 50),
        "expected a red fill"
    );

    let (_, scaled_width, scaled_height) =
        cre_lib::render_svg_image(svg, Some(5), Some(10)).unwrap();
    assert_eq!((scaled_width, scaled_height), (5, 10));

    assert!(cre_lib::render_svg_image(b"not an svg", None, None).is_none());
}

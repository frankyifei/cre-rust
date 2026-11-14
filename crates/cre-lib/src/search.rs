//! Full-text search.
//!
//! `find_text` walks every laid-out chunk's line entries and returns one hit
//! per match, sorted and optionally filtered by direction from the reader's
//! position. It needs the whole book laid out (see `ensure_layout_complete`).

use super::*;

impl Document {
    pub fn find_text(&self, query: &str) -> Result<Vec<SearchHit>> {
        self.find_text_with(query, false, false, None)
    }

    pub fn find_text_with(
        &self,
        query: &str,
        case_insensitive: bool,
        pattern_is_regex: bool,
        max_hits: Option<usize>,
    ) -> Result<Vec<SearchHit>> {
        self.find_text_with_search_flags(query, case_insensitive, pattern_is_regex, 0, max_hits)
    }

    pub fn find_text_with_search_flags(
        &self,
        query: &str,
        case_insensitive: bool,
        pattern_is_regex: bool,
        search_flags: i32,
        max_hits: Option<usize>,
    ) -> Result<Vec<SearchHit>> {
        if max_hits.is_some_and(|max_hits| max_hits == 0) {
            return Ok(Vec::new());
        }
        self.find_text_matches(
            query,
            case_insensitive,
            pattern_is_regex,
            search_flags,
            max_hits,
        )
    }

    pub fn find_text_with_direction(
        &self,
        query: &str,
        case_insensitive: bool,
        pattern_is_regex: bool,
        origin: i32,
        reverse: bool,
        max_hits: Option<usize>,
    ) -> Result<Vec<SearchHit>> {
        self.find_text_with_direction_flags(
            query,
            case_insensitive,
            pattern_is_regex,
            origin,
            reverse,
            0,
            max_hits,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn find_text_with_direction_flags(
        &self,
        query: &str,
        case_insensitive: bool,
        pattern_is_regex: bool,
        origin: i32,
        reverse: bool,
        search_flags: i32,
        max_hits: Option<usize>,
    ) -> Result<Vec<SearchHit>> {
        if max_hits.is_some_and(|max_hits| max_hits == 0) {
            return Ok(Vec::new());
        }
        let mut hits = self.find_text_matches(
            query,
            case_insensitive,
            pattern_is_regex,
            search_flags,
            None,
        )?;
        self.apply_search_direction(&mut hits, origin, reverse);
        if let Some(max_hits) = max_hits {
            hits.truncate(max_hits);
        }
        Ok(hits)
    }

    fn find_text_matches(
        &self,
        query: &str,
        case_insensitive: bool,
        pattern_is_regex: bool,
        search_flags: i32,
        max_hits: Option<usize>,
    ) -> Result<Vec<SearchHit>> {
        if query.is_empty() {
            bail!("search query must not be empty");
        }
        if !self.page_table.all_fresh() {
            // A lazily opened book (or one whose background pass is still
            // running) has no lines for its unlaid chunks, and skipping them
            // would silently hide matches.
            bail!("full-text search needs every chunk laid out; call ensure_layout_complete()");
        }

        let literal = !pattern_is_regex || (search_flags & SEARCH_FLAGS_LITERAL) != 0;
        let pattern = if literal {
            regex::escape(query)
        } else if (search_flags & SEARCH_FLAGS_COMMENTS) != 0 {
            format!("(?x){query}")
        } else {
            query.to_string()
        };
        let mut builder = regex::RegexBuilder::new(&pattern);
        builder.case_insensitive(
            case_insensitive || (search_flags & SEARCH_FLAGS_CASE_INSENSITIVE) != 0,
        );
        if (search_flags & SEARCH_FLAGS_DOTALL) != 0 {
            builder.dot_matches_new_line(true);
        }
        if (search_flags & SEARCH_FLAGS_MULTILINE) != 0 {
            builder.multi_line(true);
        }
        let re = builder.build()?;

        let mut hits = Vec::new();
        for page in 1..=self.page_count() {
            let meta = &self.page_table.pages()[page - 1];
            let chunk_index = meta.chunk;
            let line_base = self.page_table.chunks()[chunk_index].line_base;
            let snapshot = self.page_snapshot_boxes(page)?;
            let mut page_text = String::new();
            let mut spans = Vec::with_capacity(snapshot.len());
            for line in &snapshot {
                if !page_text.is_empty() {
                    page_text.push('\n');
                }
                let start = page_text.len();
                page_text.push_str(&line.text);
                spans.push((
                    line.global_line,
                    start,
                    page_text.len(),
                    TextBox {
                        text: line.text.clone(),
                        page,
                        x: line.x,
                        y: line.y,
                        width: line.width,
                        height: line.height,
                    },
                ));
            }

            for mat in re.find_iter(&page_text) {
                let start = mat.start();
                let end = mat.end();
                if start == end {
                    continue;
                }

                let Some(start_index) = spans.iter().position(|(_, span_start, span_end, _)| {
                    start >= *span_start && start < *span_end
                }) else {
                    continue;
                };
                let last = end - 1;
                let Some(end_index) = spans.iter().position(|(_, span_start, span_end, _)| {
                    last >= *span_start && last < *span_end
                }) else {
                    continue;
                };

                let (start_line_index, start_span, _, _) = &spans[start_index];
                let (end_line_index, end_span, _, end_box) = &spans[end_index];
                let local_start = start - start_span;
                let local_end = (end - end_span).min(end_box.text.len());
                let Some(local_start_line) = start_line_index.checked_sub(line_base) else {
                    continue;
                };
                let Some(local_end_line) = end_line_index.checked_sub(line_base) else {
                    continue;
                };

                let Ok((start_position, end_position)) =
                    self.with_active_chunk(chunk_index, |active| {
                        let root = active.doc.root_element();
                        let start_position = xpointer::position_for_line_offset(
                            root,
                            &active.lines[local_start_line],
                            local_start,
                        )?;
                        let end_position = xpointer::position_for_line_offset(
                            root,
                            &active.lines[local_end_line],
                            local_end,
                        )
                        .ok();
                        Ok((start_position, end_position))
                    })
                else {
                    continue;
                };

                hits.push(SearchHit {
                    page,
                    xpointer_range: XPointerRange {
                        start: start_position,
                        end: end_position,
                        id: None,
                    },
                    rects: spans[start_index..=end_index]
                        .iter()
                        .map(|(_, _, _, text_box)| text_box.clone())
                        .collect(),
                });
                if max_hits.is_some_and(|max_hits| hits.len() >= max_hits) {
                    return Ok(hits);
                }
            }
        }
        Ok(hits)
    }

    fn apply_search_direction(&self, hits: &mut Vec<SearchHit>, origin: i32, reverse: bool) {
        let current_page = self.current_page();
        if reverse {
            if origin == -1 {
                hits.retain(|hit| hit.page >= current_page);
            } else {
                hits.retain(|hit| hit.page <= current_page);
            }
            // `search` appends hits page by page in reading order; reverse is
            // all that is needed. Sorting by `TextPosition` was not document
            // order (it compares path tags).
            hits.reverse();
        } else if origin == -1 {
            hits.retain(|hit| hit.page <= current_page);
        } else {
            hits.retain(|hit| hit.page >= current_page);
        }
    }
}

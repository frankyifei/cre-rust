//! Moving around the book: pages, hrefs and XPointers, and the character- and
//! word-sized steps the reader uses for its cursor.
//!
//! A page number is only a view of the chunk states (see `page_table.rs`); an
//! XPointer is what actually pins a position, so every jump here ends in one.

use super::*;

impl Document {
    pub fn go_to_page(&mut self, page: usize) -> Result<usize> {
        if page < 1 || page > self.page_count() {
            bail!("page {page} is outside 1..={}", self.page_count());
        }
        // Landing in a provisional chunk lays that chunk out now, so the page
        // number we report is a real one.
        let page = self.ensure_page_layout(page)?;
        self.current_page = page;
        Ok(page)
    }

    pub fn set_pos(&mut self, pos: i64) -> Result<usize> {
        let page_count = self.page_count();
        if page_count == 0 {
            bail!("document has no pages");
        }
        // `pos` is a document-space y (see `current_document_pos`), i.e. the
        // top of a page. Staying on the current page when the position falls
        // inside it keeps the reader's frequent `gotoPos(getCurrentPos())`
        // refresh from moving; anything else is looked up by page start, and
        // pages whose start is still provisional (0) are skipped so a lookup
        // never jumps past real pages.
        let current = self.current_page.max(1).min(page_count);
        let start = self.page_start_y(current);
        let height = self.page_render_height(current).max(0);
        if pos >= start && pos < start + height {
            return Ok(current);
        }
        if pos <= 0 {
            return self.go_to_page(1);
        }
        let mut page = 1usize;
        for candidate in 2..=page_count {
            let candidate_start = self.page_start_y(candidate);
            if candidate_start <= 0 {
                continue;
            }
            if candidate_start <= pos {
                page = candidate;
            } else {
                break;
            }
        }
        self.go_to_page(page)
    }

    pub fn go_to_first_page(&mut self) -> usize {
        self.current_page = self.ensure_page_layout(1).unwrap_or(1);
        self.current_page
    }

    pub fn go_to_last_page(&mut self) -> usize {
        let page = self.page_count();
        self.current_page = self.ensure_page_layout(page).unwrap_or(page);
        self.current_page
    }

    pub fn go_to_next_page(&mut self) -> usize {
        let page = (self.current_page + 1).min(self.page_count());
        self.current_page = self.ensure_page_layout(page).unwrap_or(page);
        self.current_page
    }

    pub fn go_to_previous_page(&mut self) -> usize {
        let page = self.current_page.saturating_sub(1).max(1);
        self.current_page = self.ensure_page_layout(page).unwrap_or(page);
        self.current_page
    }

    pub fn page_from_href(&self, href: &str) -> Result<usize> {
        page_for_href(&self.href_pages, href)
    }

    pub fn go_to_href(&mut self, href: &str) -> Result<usize> {
        let page = self.page_from_href(href)?;
        let page = self.ensure_page_layout(page)?;
        self.current_page = page;
        Ok(page)
    }

    pub fn xpointer_from_page(&self, page: usize) -> Result<String> {
        let Some(index) = page.checked_sub(1) else {
            bail!("page numbers are 1-based");
        };
        let Some(snapshot) = self.page_table.pages().get(index) else {
            bail!("page {page} is outside 1..={}", self.page_count());
        };
        if let Some(xpointer) = self.page_xpointer(page) {
            return Ok(xpointer);
        }

        let chunk_index = snapshot.chunk;
        let local_page = page - self.page_table.chunks()[chunk_index].first_page + 1;
        let height = self.height;
        let page_margin = self.page_margin;
        self.with_active_chunk(chunk_index, |active| {
            xpointer::xpointer_from_page(
                &active.doc,
                &active.lines,
                &active.page_starts,
                local_page,
                height,
                page_margin,
            )
        })
    }

    /// The page-start XPointer of a page, read from the chunk layout that owns
    /// it.
    ///
    /// It is stored once per page in the chunk layout; copying it into the
    /// flattened page list as well cost one heap string per page (15 557 pages
    /// and ~1.3 MB on the 24 MB text book). A page that is in the list always
    /// has a laid-out chunk, so this is a lookup, never a layout.
    pub(crate) fn page_xpointer(&self, page: usize) -> Option<String> {
        let snapshot = self.page_table.pages().get(page.checked_sub(1)?)?;
        let first_page = self.page_table.first_page(snapshot.chunk);
        let local = page.checked_sub(first_page)?;
        let layout = self.page_table.layout(snapshot.chunk)?;
        layout.xpointers.get(local).cloned().flatten()
    }

    pub fn page_from_xpointer(&self, xpointer: &str) -> Result<usize> {
        let trimmed = xpointer.trim();
        if let Some(page) = self.xpointer_pages.get(trimmed) {
            return Ok(*page);
        }

        if let Ok(position) = parse_text_position(trimmed)
            && let Some(page) = self.xpointer_pages.get(&position.to_xpointer())
        {
            return Ok(*page);
        }

        let position = match parse_text_position(trimmed) {
            Ok(position) => position,
            Err(_) => return self.page_from_href(trimmed),
        };
        let chunk_index = *self
            .spine_to_chunk
            .get(&position.spine_index)
            .with_context(|| {
                format!(
                    "XPointer DocFragment[{}] was not found in the rendered document",
                    position.spine_index
                )
            })?;
        let line_page = self
            .page_from_text_position_by_line_ranges(&position, chunk_index)
            .unwrap_or(None);
        if let Some(page) = line_page {
            return Ok(page);
        }

        let local_page = self.with_active_chunk(chunk_index, |active| {
            xpointer::page_from_xpointer(&active.doc, &active.page_starts, trimmed)
        })?;
        Ok(self.page_table.chunks()[chunk_index].first_page + local_page - 1)
    }

    fn page_from_text_position_by_line_ranges(
        &self,
        position: &TextPosition,
        chunk_index: usize,
    ) -> Result<Option<usize>> {
        let Some(layout) = self.page_table.layout(chunk_index) else {
            return Ok(None);
        };
        let head = self.page_table.first_page(chunk_index);
        // The last line whose start is <= the position holds it (a line's text
        // runs to the next line's start). "Is <=" has to be document order, so
        // the key is the resolved text node plus the offset in it — the same key
        // the table was built with. `TextPosition` ordering is not document
        // order (it compares path segments by tag first).
        let lines = &layout.lines;
        self.with_active_chunk(chunk_index, |active| {
            let root = active.doc.root_element();
            let Some(key) = xpointer::block_and_layout_offset(root, position) else {
                return Ok(None);
            };
            let first_greater =
                lines.partition_point(|entry| (entry.block, entry.offset as usize) < key);
            let index = if lines
                .get(first_greater)
                .is_some_and(|entry| (entry.block, entry.offset as usize) == key)
            {
                Some(first_greater)
            } else {
                first_greater.checked_sub(1)
            };
            Ok(index.map(|index| head + lines[index].page))
        })
    }

    pub fn go_to_xpointer(&mut self, xpointer: &str) -> Result<usize> {
        // Lay out the chunk the target lives in first: its provisional pages
        // would otherwise put us on the wrong page for the new settings.
        self.ensure_xpointer_layout(xpointer)?;
        let page = self.page_from_xpointer(xpointer)?;
        self.current_page = page;
        Ok(page)
    }

    fn compare_positions_in_chunk(
        &self,
        chunk_index: usize,
        left: &TextPosition,
        right: &TextPosition,
    ) -> Result<Option<std::cmp::Ordering>> {
        self.with_active_chunk(chunk_index, |active| {
            let root = active.doc.root_element();
            Ok(xpointer::compare_text_positions(root, left, right))
        })
    }

    pub(crate) fn compare_positions_on_page(
        &self,
        page: usize,
        left: &TextPosition,
        right: &TextPosition,
    ) -> Result<Option<std::cmp::Ordering>> {
        let Some(snapshot) = self.page_table.pages().get(page.saturating_sub(1)) else {
            return Ok(None);
        };
        self.compare_positions_in_chunk(snapshot.chunk, left, right)
    }

    pub fn compare_xpointers(&self, left: &str, right: &str) -> Result<i32> {
        let left = parse_text_position(left)?;
        let right = parse_text_position(right)?;
        let left_chunk = self.spine_to_chunk.get(&left.spine_index).copied();
        let right_chunk = self.spine_to_chunk.get(&right.spine_index).copied();
        // Return 1 when `right` is after `left`, like cre.cpp's
        // `nodep2.compare(nodep1)`. Resolve both positions to text nodes so the
        // comparison follows document order, not XPointer path strings.
        let right_vs_left = match (left_chunk, right_chunk) {
            (Some(left_chunk), Some(right_chunk)) if left_chunk == right_chunk => {
                self.compare_positions_in_chunk(left_chunk, &right, &left)?
            }
            (Some(left_chunk), Some(right_chunk)) => Some(right_chunk.cmp(&left_chunk)),
            _ => None,
        };
        let ordering = right_vs_left.unwrap_or_else(|| right.cmp(&left));
        Ok(match ordering {
            std::cmp::Ordering::Greater => 1,
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
        })
    }

    pub fn next_visible_char(&self, xpointer: &str) -> Result<Option<String>> {
        self.visible_neighbor(xpointer, Direction::Forward, NeighborUnit::Char)
    }

    pub fn prev_visible_char(&self, xpointer: &str) -> Result<Option<String>> {
        self.visible_neighbor(xpointer, Direction::Backward, NeighborUnit::Char)
    }

    pub fn next_visible_word_start(&self, xpointer: &str) -> Result<Option<String>> {
        self.visible_neighbor(xpointer, Direction::Forward, NeighborUnit::WordStart)
    }

    pub fn next_visible_word_end(&self, xpointer: &str) -> Result<Option<String>> {
        self.visible_neighbor(xpointer, Direction::Forward, NeighborUnit::WordEnd)
    }

    pub fn prev_visible_word_start(&self, xpointer: &str) -> Result<Option<String>> {
        self.visible_neighbor(xpointer, Direction::Backward, NeighborUnit::WordStart)
    }

    pub fn prev_visible_word_end(&self, xpointer: &str) -> Result<Option<String>> {
        self.visible_neighbor(xpointer, Direction::Backward, NeighborUnit::WordEnd)
    }

    fn visible_neighbor(
        &self,
        xpointer: &str,
        direction: Direction,
        unit: NeighborUnit,
    ) -> Result<Option<String>> {
        let position = parse_text_position(xpointer)?;
        let chunk_index = self.chunk_for_spine(position.spine_index)?;
        self.with_active_chunk(chunk_index, |active| {
            let root = active.doc.root_element();
            let current_id = xpointer::resolve_text_position(root, &position)?;
            let flat = flat_text_characters(root, &active.rendered_text_nodes);
            let Some(current_start) = flat.iter().position(|entry| entry.node == current_id) else {
                return Ok(None);
            };
            let start_index = (current_start + position.codepoint_offset as usize).min(flat.len());
            let target = match (direction, unit) {
                (Direction::Forward, NeighborUnit::Char) => {
                    (start_index + 1..flat.len()).find(|index| !flat[*index].ch.is_whitespace())
                }
                (Direction::Backward, NeighborUnit::Char) => (0..start_index)
                    .rev()
                    .find(|index| !flat[*index].ch.is_whitespace()),
                (Direction::Forward, NeighborUnit::WordStart) => {
                    next_word_start(&flat, start_index)
                }
                (Direction::Forward, NeighborUnit::WordEnd) => next_word_end(&flat, start_index),
                (Direction::Backward, NeighborUnit::WordStart) => {
                    prev_word_start(&flat, start_index)
                }
                (Direction::Backward, NeighborUnit::WordEnd) => prev_word_end(&flat, start_index),
            };
            let Some(index) = target else {
                return Ok(None);
            };
            let Some(entry) = flat.get(index).or_else(|| flat.last()) else {
                return Ok(None);
            };
            Ok(Some(
                xpointer::position_for_text_node(root, entry.node, entry.offset)?.to_xpointer(),
            ))
        })
    }

    pub fn extend_to_sentence(
        &self,
        start: &str,
        end: &str,
    ) -> Result<Option<(String, String, String)>> {
        let mut start_position = parse_text_position(start)?;
        let mut end_position = parse_text_position(end)?;
        if start_position.spine_index != end_position.spine_index {
            return Ok(None);
        }
        if end_position < start_position {
            std::mem::swap(&mut start_position, &mut end_position);
        }
        let chunk_index = self.chunk_for_spine(start_position.spine_index)?;
        self.with_active_chunk(chunk_index, |active| {
            let root = active.doc.root_element();
            let start_id = xpointer::resolve_text_position(root, &start_position)?;
            let end_id = xpointer::resolve_text_position(root, &end_position)?;
            if start_id != end_id {
                return Ok(None);
            }
            let Some(text) = root
                .with(start_id)
                .text_data()
                .map(|data| data.content.clone())
            else {
                return Ok(None);
            };
            let chars = text.chars().collect::<Vec<_>>();
            if chars.is_empty() {
                return Ok(None);
            }
            let start_idx = (start_position.codepoint_offset as usize).min(chars.len());
            let end_idx = (end_position.codepoint_offset as usize).min(chars.len());
            if start_idx > end_idx {
                return Ok(None);
            }

            let mut is_sentence_segment = true;
            let mut new_start = start_idx;
            let mut grabbed_opening = false;
            let mut index = start_idx;
            while index > 0 {
                index -= 1;
                let ch = chars[index];
                if is_space_char(ch) {
                    continue;
                }
                if is_punct_opening_char(ch) {
                    new_start = index;
                    grabbed_opening = true;
                    continue;
                }
                if is_punctuation_char(ch) {
                    break;
                }
                if !grabbed_opening {
                    is_sentence_segment = false;
                }
                break;
            }
            if !is_sentence_segment {
                return Ok(None);
            }

            let mut new_end = end_idx;
            let mut grabbed_closing = false;
            if end_idx < chars.len() {
                index = end_idx;
                while index < chars.len() {
                    let ch = chars[index];
                    if is_space_char(ch) {
                        index += 1;
                        continue;
                    }
                    if is_punctuation_char(ch) && !is_punct_opening_char(ch) {
                        new_end = index + 1;
                        grabbed_closing = true;
                        index += 1;
                        continue;
                    }
                    if !grabbed_closing {
                        is_sentence_segment = false;
                    }
                    break;
                }
            }
            if !is_sentence_segment {
                return Ok(None);
            }

            let out_text = chars[new_start..new_end].iter().collect::<String>();
            if out_text.trim().is_empty() {
                return Ok(None);
            }
            start_position.codepoint_offset = new_start as u32;
            end_position.codepoint_offset = new_end as u32;
            Ok(Some((
                out_text,
                start_position.to_xpointer(),
                end_position.to_xpointer(),
            )))
        })
    }

    pub fn text_from_xpointer(&self, xpointer: &str) -> Result<String> {
        let position = parse_text_position(xpointer)?;
        let chunk_index = *self
            .spine_to_chunk
            .get(&position.spine_index)
            .with_context(|| {
                format!(
                    "XPointer DocFragment[{}] was not found in the rendered document",
                    position.spine_index
                )
            })?;
        self.with_active_chunk(chunk_index, |active| {
            let root = active.doc.root_element();
            let text_id = xpointer::resolve_text_position(root, &position)?;
            Ok(root
                .with(text_id)
                .text_data()
                .map(|text| text.content.clone())
                .unwrap_or_default())
        })
    }

    pub fn xpointer_from_position(&self, position: &TextPosition) -> Result<String> {
        Ok(position.to_xpointer())
    }
}

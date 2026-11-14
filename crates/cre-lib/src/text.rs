//! Reading a page back out: text, words, boxes, hit tests and selections.
//!
//! These are the queries KOReader makes with a pen position or an XPointer -
//! "what word is here", "where is this word", "what is selected between these
//! two points". They all run against the same per-page snapshot of line boxes.

use super::*;

impl Document {
    pub fn page_text_boxes(&self, page: usize) -> Result<Vec<TextBox>> {
        Ok(self
            .page_text_boxes_with_lines(page)?
            .into_iter()
            .map(|(_, text_box)| text_box)
            .collect())
    }

    pub fn page_text(&self, page: usize) -> Result<String> {
        let lines = self.page_text_lines(page)?;
        Ok(join_text_boxes(lines.iter().map(|(_, text_box, line)| {
            (text_box.text.as_str(), *line)
        })))
    }

    pub fn text_at(&self, position: PagePosition) -> Result<Option<String>> {
        Ok(self
            .page_text_boxes(position.page)?
            .into_iter()
            .find(|text_box| {
                position.x >= text_box.x
                    && position.x <= text_box.x + text_box.width
                    && position.y >= text_box.y
                    && position.y <= text_box.y + text_box.height
            })
            .map(|text_box| text_box.text))
    }

    pub fn hit_test(&self, position: PagePosition) -> Result<Option<HitResult>> {
        let page_lines = self.page_text_boxes_with_lines(position.page)?;
        let Some((line_index, rect)) = page_lines.into_iter().find(|(_, text_box)| {
            position.x >= text_box.x
                && position.x <= text_box.x + text_box.width
                && position.y >= text_box.y
                && position.y <= text_box.y + text_box.height
        }) else {
            return Ok(None);
        };

        let snapshot = self
            .page_table
            .pages()
            .get(position.page - 1)
            .context("page snapshot is missing")?;
        let chunk_index = snapshot.chunk;
        let page_top = snapshot.local_start.top;
        let local_line = line_index - self.page_table.chunks()[chunk_index].line_base;

        self.with_active_chunk(chunk_index, |active| {
            let line = active
                .lines
                .get(local_line)
                .context("line snapshot is missing")?;
            let root = active.doc.root_element();
            let node = root.with(line.node_id);
            let Some(inline_layout) = node
                .element_data()
                .and_then(|element| element.inline_layout_data.as_ref())
            else {
                return Ok(None);
            };

            let layout = node.unrounded_layout();
            let inset = layout.padding + layout.border;
            let origin = node.absolute_position(inset.left, inset.top);
            let (position_x, position_y) =
                if let Some(fixed) = self.fixed_layouts.get(chunk_index).and_then(|fixed| *fixed) {
                    let (fit, offset_x, offset_y) = self.fixed_page_transform(fixed);
                    let scale = self.viewport_scale.max(0.1);
                    (
                        (position.x / scale - offset_x) / fit,
                        (position.y / scale - offset_y) / fit,
                    )
                } else {
                    (position.x, position.y)
                };
            let local_x = position_x - origin.x;
            let local_y = position_y + page_top - origin.y;

            let byte_offset = node
                .text_offset_at_point(local_x, local_y)
                .map_or(0, |offset| offset.saturating_sub(line.text_start));
            let max_offset = line.text_end.saturating_sub(line.text_start);
            let byte_offset = byte_offset.min(max_offset).min(inline_layout.text.len());

            // Ask parley for the word under the point, the same way a text
            // editor does (`Selection::word_from_point`), instead of deriving
            // the boundaries from clusters ourselves. The range is relative to
            // the block's layout text; the caller wants it relative to the line.
            let word_range = {
                let scale = inline_layout.layout.scale();
                let selection = ParleySelection::word_from_point(
                    &inline_layout.layout,
                    local_x * scale,
                    (local_y - inline_layout.block_offset) * scale,
                );
                let range = selection.text_range();
                let line_start = line.text_start;
                let line_end = line.text_end;
                let start = range.start.max(line_start).saturating_sub(line_start);
                let end = range.end.min(line_end).saturating_sub(line_start);
                (start < end).then_some((start, end))
            };
            let text_position = xpointer::position_for_line_offset(root, line, byte_offset)
                .or_else(|_| xpointer::position_for_line_offset(root, line, 0))?;
            let source_id = source_element_at_line_offset(root, line, byte_offset)
                .or(line.source_element_id)
                .unwrap_or(line.node_id);
            let link = nearest_link(root, source_id);

            Ok(Some(HitResult {
                page: position.page,
                text_position,
                text: rect.text.clone(),
                rect,
                link,
                point: position,
                line_index,
                byte_offset,
                word_range,
            }))
        })
    }

    fn parley_word_byte_range(
        &self,
        page: usize,
        line_index: usize,
        line_byte_offset: usize,
    ) -> Result<Option<(usize, usize)>> {
        let Some(snapshot) = self.page_table.pages().get(page.saturating_sub(1)) else {
            return Ok(None);
        };
        let chunk_index = snapshot.chunk;
        let line_base = self.page_table.chunks()[chunk_index].line_base;
        let Some(local_line) = line_index.checked_sub(line_base) else {
            return Ok(None);
        };

        self.with_active_chunk(chunk_index, |active| {
            let Some(line_box) = active.lines.get(local_line) else {
                return Ok(None);
            };
            let line_start = line_box.text_start;
            let line_end = line_box.text_end;
            if line_start >= line_end {
                return Ok(None);
            }
            let absolute = line_start.saturating_add(line_byte_offset);
            if absolute < line_start || absolute >= line_end {
                return Ok(None);
            }

            let root = active.doc.root_element();
            let node = root.with(line_box.node_id);
            let Some(inline_layout) = node
                .element_data()
                .and_then(|element| element.inline_layout_data.as_ref())
            else {
                return Ok(None);
            };
            let Some(cluster) = Cluster::from_byte_index(&inline_layout.layout, absolute) else {
                return Ok(None);
            };

            let mut word_start_cluster = cluster;
            if !word_start_cluster.is_word_boundary() {
                let Some(prev) = word_start_cluster.previous_logical_word() else {
                    return Ok(None);
                };
                word_start_cluster = prev;
            }
            let start = word_start_cluster.text_range().start;
            let end = word_start_cluster
                .next_logical_word()
                .map(|boundary| boundary.text_range().start)
                .unwrap_or(line_end);

            let rel_start = start
                .saturating_sub(line_start)
                .min(line_end.saturating_sub(line_start));
            let rel_end = end
                .saturating_sub(line_start)
                .min(line_end.saturating_sub(line_start));
            if rel_start >= rel_end {
                return Ok(None);
            }
            Ok(Some((rel_start, rel_end)))
        })
    }

    pub fn word_at(&self, position: PagePosition) -> Result<Option<WordHit>> {
        let Some(hit) = self.hit_test(position)? else {
            return Ok(None);
        };

        let snapshot = self
            .page_table
            .pages()
            .get(hit.page.saturating_sub(1))
            .context("page snapshot is missing")?;
        let chunk_index = snapshot.chunk;
        let line_base = self.page_table.chunks()[chunk_index].line_base;
        let local_line = hit
            .line_index
            .checked_sub(line_base)
            .context("line index is outside its chunk")?;

        let fallback = word_byte_range(&hit.text, hit.byte_offset);
        let parley_range = hit.word_range.or_else(|| {
            self.parley_word_byte_range(hit.page, hit.line_index, hit.byte_offset)
                .unwrap_or(None)
        });
        let use_parley = match (fallback, parley_range) {
            (Some((fallback_start, fallback_end)), Some((parley_start, parley_end))) => {
                parley_start >= fallback_start
                    && parley_end <= fallback_end
                    && parley_start < parley_end
                    && (parley_start, parley_end) != (fallback_start, fallback_end)
            }
            (None, Some((parley_start, parley_end))) => parley_start < parley_end,
            (_, None) => false,
        };
        let Some((start_byte, end_byte)) = (if use_parley { parley_range } else { fallback })
        else {
            return Ok(None);
        };

        let (start_position, end_position) = self.with_active_chunk(chunk_index, |active| {
            let line = active
                .lines
                .get(local_line)
                .context("line snapshot is missing")?;
            let root = active.doc.root_element();
            let start = xpointer::position_for_line_offset(root, line, start_byte)?;
            // The end byte is the boundary after the word. Map it to the
            // position just after the word's own last character, not to the
            // first character of the next text node: a word ending at a link
            // or span boundary otherwise produces a range split across blocks.
            let block = root.with(line.node_id);
            let mut end = xpointer::position_after_layout_offset(
                root,
                block,
                line.text_start.saturating_add(end_byte),
            )
            .unwrap_or(xpointer::position_for_line_offset(root, line, end_byte)?);
            if end == start {
                // The line -> source mapping collapsed (it matches text
                // prefixes, so repeated sentences can resolve to the same
                // position). A zero-length range highlights nothing, so widen
                // it by the word's length inside the node it resolved to.
                let word_chars = hit.text[start_byte..end_byte].chars().count().max(1) as u32;
                let node_chars = xpointer::resolve_text_position(root, &start)
                    .ok()
                    .and_then(|id| {
                        root.with(id)
                            .text_data()
                            .map(|data| data.content.chars().count() as u32)
                    })
                    .unwrap_or(u32::MAX);
                let mut widened = start.clone();
                widened.codepoint_offset = start
                    .codepoint_offset
                    .saturating_add(word_chars)
                    .min(node_chars);
                if widened != start {
                    end = widened;
                }
            }
            Ok((start, end))
        })?;

        let text = hit.text[start_byte..end_byte].to_string();
        let Some(rect) = self.word_box(WordSpan {
            page: hit.page,
            line_index: hit.line_index,
            line: &hit.rect,
            start_byte,
            end_byte,
            text: &text,
        }) else {
            return Ok(None);
        };

        Ok(Some(WordHit {
            text,
            xpointer_range: XPointerRange {
                start: start_position,
                end: Some(end_position),
                id: None,
            },
            rects: vec![rect],
        }))
    }

    /// Box of a byte range on one line, from parley's selection geometry.
    /// Returns `None` when the line has no parley layout; callers omit the box
    /// rather than estimating one.
    fn word_box(&self, span: WordSpan<'_>) -> Option<TextBox> {
        let (x0, x1) = self
            .parley_segment_x_bounds(span.page, span.line_index, span.start_byte, span.end_byte)
            .ok()
            .flatten()?;
        Some(TextBox {
            text: span.text.to_string(),
            page: span.page,
            x: x0,
            y: span.line.y,
            width: (x1 - x0).max(1.0),
            height: span.line.height,
        })
    }

    pub fn nearest_word_at(
        &self,
        position: PagePosition,
        pages: &[usize],
        direction: u32,
    ) -> Result<Option<WordHit>> {
        let mut candidates = Vec::new();
        let mut pending = Vec::new();
        for page in pages.iter().copied() {
            if page == 0 || page > self.page_count() {
                continue;
            }
            let Some(snapshot) = self.page_table.pages().get(page - 1) else {
                continue;
            };
            let chunk_index = snapshot.chunk;
            let line_base = self.page_table.chunks()[chunk_index].line_base;
            for (global_line, rect) in self.page_text_boxes_with_lines(page)? {
                let Some(local_line) = global_line.checked_sub(line_base) else {
                    continue;
                };
                let ranges = word_ranges(&rect.text);
                self.with_active_chunk(chunk_index, |active| {
                    let Some(line) = active.lines.get(local_line) else {
                        return Ok(());
                    };
                    let root = active.doc.root_element();
                    let block = root.with(line.node_id);
                    for (start_byte, end_byte) in ranges {
                        let Ok(start_position) =
                            xpointer::position_for_line_offset(root, line, start_byte)
                        else {
                            continue;
                        };
                        let end_position = xpointer::position_after_layout_offset(
                            root,
                            block,
                            line.text_start.saturating_add(end_byte),
                        )
                        .or_else(|| xpointer::position_for_line_offset(root, line, end_byte).ok());
                        pending.push(PendingWord {
                            page,
                            line_index: global_line,
                            line: rect.clone(),
                            start_byte,
                            end_byte,
                            start: start_position,
                            end: end_position,
                        });
                    }
                    Ok(())
                })?;
            }
        }

        for pending in pending {
            let text = pending.line.text[pending.start_byte..pending.end_byte].to_string();
            let Some(text_box) = self.word_box(WordSpan {
                page: pending.page,
                line_index: pending.line_index,
                line: &pending.line,
                start_byte: pending.start_byte,
                end_byte: pending.end_byte,
                text: &text,
            }) else {
                continue;
            };
            candidates.push(WordHit {
                text,
                xpointer_range: XPointerRange {
                    start: pending.start,
                    end: pending.end,
                    id: None,
                },
                rects: vec![text_box],
            });
        }

        let word = candidates
            .into_iter()
            .filter(|word| {
                word.rects
                    .first()
                    .is_some_and(|rect| direction_matches(position, rect, direction))
            })
            .min_by(|left, right| {
                let left = word_sort_key(left, position, direction);
                let right = word_sort_key(right, position, direction);
                left.partial_cmp(&right)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        Ok(word)
    }

    pub fn boxes_between_xpointers(&self, start: &str, end: &str) -> Result<Vec<TextBox>> {
        let mut boxes = Vec::new();
        for (page, line_index, mut rect, slice_start, slice_end) in
            self.clipped_lines_between_xpointers(start, end)?
        {
            let text_slice = rect.text[slice_start..slice_end].to_string();
            let Some((x0, x1)) = self
                .parley_segment_x_bounds(page, line_index, slice_start, slice_end)
                .unwrap_or(None)
            else {
                continue;
            };
            rect.text = text_slice;
            rect.x = x0;
            rect.width = (x1 - x0).max(1.0);
            boxes.push(rect);
        }
        Ok(boxes)
    }

    fn parley_segment_x_bounds(
        &self,
        page: usize,
        line_index: usize,
        slice_start: usize,
        slice_end: usize,
    ) -> Result<Option<(f32, f32)>> {
        let Some(snapshot) = self.page_table.pages().get(page.saturating_sub(1)) else {
            return Ok(None);
        };
        let chunk_index = snapshot.chunk;
        let line_base = self.page_table.chunks()[chunk_index].line_base;
        let Some(local_line) = line_index.checked_sub(line_base) else {
            return Ok(None);
        };

        self.with_active_chunk(chunk_index, |active| {
            let Some(line_box) = active.lines.get(local_line) else {
                return Ok(None);
            };
            let root = active.doc.root_element();
            let node = root.with(line_box.node_id);
            let Some(inline_layout) = node
                .element_data()
                .and_then(|element| element.inline_layout_data.as_ref())
            else {
                return Ok(None);
            };
            let layout = &inline_layout.layout;

            let Some((target_index, target_line)) = (0..layout.len())
                .map(|line_number| (line_number, layout.get(line_number)))
                .filter_map(|(line_number, line)| line.map(|value| (line_number, value)))
                .find(|(_, line)| {
                    let text_range = line.text_range();
                    text_range.start <= line_box.text_start && text_range.end >= line_box.text_end
                })
            else {
                return Ok(None);
            };
            let line_metrics = target_line.metrics();
            // `bbox` is in the inline layout space, and `line_box.left` is
            // already shifted by the line's inline start. Subtract that start
            // (offset + inline_min_coord) before adding the absolute left.
            // Without the inline_min_coord part, a line next to a float (a
            // drop cap) is shifted right by the float width.
            let line_start = line_metrics.offset + line_metrics.inline_min_coord;

            let start_byte = line_box.text_start.saturating_add(slice_start);
            let end_byte = line_box.text_start.saturating_add(slice_end);
            if start_byte >= end_byte {
                return Ok(None);
            }

            let start_cursor = Cursor::from_byte_index(layout, start_byte, Affinity::Upstream);
            let end_cursor = Cursor::from_byte_index(layout, end_byte, Affinity::Downstream);
            let selection = ParleySelection::new(start_cursor, end_cursor);

            let mut left = f32::INFINITY;
            let mut right = f32::NEG_INFINITY;
            for (bbox, line_idx) in selection.geometry(layout) {
                if line_idx != target_index {
                    continue;
                }
                let x0 = line_box.left + ((bbox.x0 as f32) - line_start);
                let x1 = line_box.left + ((bbox.x1 as f32) - line_start);
                left = left.min(x0);
                left = left.min(x1);
                right = right.max(x0);
                right = right.max(x1);
            }
            if left.is_finite() && right.is_finite() && right > left {
                let scale = self.viewport_scale;
                return Ok(Some((left * scale, right * scale)));
            }
            Ok(None)
        })
    }

    pub fn point_from_xpointer(&self, xpointer: &str) -> Result<Option<(f32, f32)>> {
        // Walk forward to the next visible character when the position itself
        // is not inside a line (an XPointer at a block boundary, or one whose
        // node is not laid out). This used to recurse; a position that never
        // matches a line made the recursion run through the whole book and
        // overflow the stack (a SIGSEGV that killed KOReader while painting a
        // page with a highlight). A few dozen steps are enough for the cases
        // this exists for; anything further is not visible text.
        let mut current = xpointer.to_string();
        for _ in 0..64 {
            let target = parse_text_position(&current)?;
            let page = self.page_from_xpointer(&current)?;
            for (line_index, rect) in self.page_text_boxes_with_lines(page)? {
                if let Some(chars) = self.line_relative_chars(page, line_index, &target)? {
                    let byte = byte_offset_for_char(&rect.text, chars);
                    let next = rect.text[byte..]
                        .chars()
                        .next()
                        .map(|ch| byte + ch.len_utf8())
                        .unwrap_or(byte + 1);
                    if let Ok(Some((x, _))) =
                        self.parley_segment_x_bounds(page, line_index, byte, next)
                    {
                        return Ok(Some((x, rect.y)));
                    }
                    // The line has no parley geometry for this character; try
                    // the next visible one instead of estimating.
                    break;
                }
            }
            let Some(next) = self.next_visible_char(&current)? else {
                return Ok(None);
            };
            if next == current {
                return Ok(None);
            }
            current = next;
        }
        Ok(None)
    }

    pub fn page_links(&self, page: usize, internal_only: bool) -> Result<Vec<PageLink>> {
        let Some(snapshot) = self.page_table.pages().get(page.saturating_sub(1)) else {
            bail!("page {page} is outside 1..={}", self.page_count());
        };
        let chunk_index = snapshot.chunk;
        let line_base = self.page_table.chunks()[chunk_index].line_base;
        let lines = self.page_text_boxes_with_lines(page)?;

        let mut links: Vec<PageLink> = Vec::new();
        for (line_index, rect) in lines {
            let Some(local_line) = line_index.checked_sub(line_base) else {
                continue;
            };
            self.with_active_chunk(chunk_index, |active| {
                let Some(line) = active.lines.get(local_line) else {
                    return Ok(());
                };
                let root = active.doc.root_element();
                let char_count = rect.text.chars().count().max(1);
                let mut current: Option<(String, usize, usize)> = None;

                for (char_index, (byte_offset, _)) in rect.text.char_indices().enumerate() {
                    let href = source_element_at_line_offset(root, line, byte_offset)
                        .and_then(|source_id| nearest_link(root, source_id));
                    match (&mut current, href) {
                        (Some((active_href, _, end)), Some(href)) if *active_href == href => {
                            *end = char_index + 1;
                        }
                        (_, href) => {
                            if let Some((href, start, end)) = current.take() {
                                push_link_segment(
                                    &mut links,
                                    &href,
                                    &rect,
                                    start,
                                    end,
                                    char_count,
                                    self.page_from_href(&href).is_ok(),
                                );
                            }
                            current = href.map(|href| (href, char_index, char_index + 1));
                        }
                    }
                }

                if let Some((href, start, end)) = current {
                    push_link_segment(
                        &mut links,
                        &href,
                        &rect,
                        start,
                        end,
                        char_count,
                        self.page_from_href(&href).is_ok(),
                    );
                }
                Ok(())
            })?;
        }

        links.retain(|link| !internal_only || link.internal);
        Ok(links)
    }

    pub fn text_between_xpointers(&self, start: &str, end: &str) -> Result<String> {
        let mut text = String::new();
        for (_, _, rect, slice_start, slice_end) in
            self.clipped_lines_between_xpointers(start, end)?
        {
            let part = &rect.text[slice_start..slice_end];
            if !part.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(part);
            }
        }
        Ok(text.trim_end().to_string())
    }

    fn clipped_lines_between_xpointers(&self, start: &str, end: &str) -> Result<Vec<ClippedLine>> {
        let mut start_position = parse_text_position(start)?;
        let mut end_position = parse_text_position(end)?;
        let mut start_page = self.page_from_xpointer(start)?;
        let mut end_page = self.page_from_xpointer(end)?;
        let mut reversed = start_page > end_page;
        if !reversed && start_page == end_page {
            // Same page: compare the two positions in document order. Their
            // XPointer paths cannot be compared directly (a child span sorts
            // after its parent's tag even when its text comes first).
            reversed =
                self.compare_positions_on_page(start_page, &start_position, &end_position)?
                    == Some(std::cmp::Ordering::Greater);
        }
        if reversed {
            std::mem::swap(&mut start_page, &mut end_page);
            std::mem::swap(&mut start_position, &mut end_position);
        }

        let mut lines = Vec::new();
        for page in start_page..=end_page {
            let Some(snapshot) = self.page_table.pages().get(page.saturating_sub(1)) else {
                continue;
            };
            let chunk_index = snapshot.chunk;
            let line_base = self.page_table.chunks()[chunk_index].line_base;
            let page_boxes = self.page_text_boxes_with_lines(page)?;
            let clipped = self.with_active_chunk(chunk_index, |active| {
                let root = active.doc.root_element();
                let mut out = Vec::new();
                for (global_line, rect) in page_boxes.iter() {
                    let Some(local_line) = global_line.checked_sub(line_base) else {
                        continue;
                    };
                    let Some(line) = active.lines.get(local_line) else {
                        continue;
                    };
                    let block = root.with(line.node_id);
                    let Some(layout) = block
                        .element_data()
                        .and_then(|element| element.inline_layout_data.as_ref())
                    else {
                        continue;
                    };
                    let text = layout.text.as_str();
                    let start_byte = line.text_start.min(text.len());
                    let end_byte = line.text_end.clamp(start_byte, text.len());
                    if start_byte >= end_byte {
                        continue;
                    }
                    // Where the two ends of the range fall: inside this line's
                    // block (exact layout offset) or in another block (compare
                    // source nodes in document order). No line-end position is
                    // needed, so a neighbouring inline root cannot make the
                    // line look longer than it is.
                    let start_key = xpointer::block_and_layout_offset(root, &start_position);
                    let end_key = xpointer::block_and_layout_offset(root, &end_position);
                    let line_first = xpointer::position_for_line_offset(root, line, 0)
                        .ok()
                        .and_then(|position| {
                            xpointer::resolve_text_position(root, &position).ok()
                        });
                    let from_byte = match start_key {
                        Some((id, offset)) if id == block.id => {
                            if offset >= end_byte {
                                continue;
                            }
                            offset.max(start_byte)
                        }
                        Some(_) => {
                            let start_text =
                                xpointer::resolve_text_position(root, &start_position).ok();
                            if matches!((start_text, line_first), (Some(start), Some(first)) if start > first)
                            {
                                continue;
                            }
                            start_byte
                        }
                        None => start_byte,
                    };
                    let to_byte = match end_key {
                        Some((id, offset)) if id == block.id => {
                            if offset <= start_byte {
                                continue;
                            }
                            offset.min(end_byte)
                        }
                        Some(_) => {
                            let end_text =
                                xpointer::resolve_text_position(root, &end_position).ok();
                            if matches!((end_text, line_first), (Some(end), Some(first)) if end < first)
                            {
                                continue;
                            }
                            end_byte
                        }
                        None => end_byte,
                    };
                    let line_start_chars = text[..start_byte].chars().count();
                    let from_chars = text[..from_byte].chars().count() - line_start_chars;
                    let to_chars = text[..to_byte].chars().count() - line_start_chars;
                    if from_chars >= to_chars {
                        continue;
                    }
                    let slice_start = byte_offset_for_char(&rect.text, from_chars);
                    let slice_end = byte_offset_for_char(&rect.text, to_chars);
                    if slice_start < slice_end {
                        out.push((page, *global_line, rect.clone(), slice_start, slice_end));
                    }
                }
                Ok(out)
            })?;
            lines.extend(clipped);
        }
        Ok(lines)
    }

    /// Where a position falls inside a line, as a character index from the
    /// line start. Uses the line's inline layout text, so a line made of
    /// several text nodes (a `<span>` inside a heading) is handled correctly;
    /// returns `None` when the position is not on the line or the layout text
    /// no longer matches the DOM.
    fn line_relative_chars(
        &self,
        page: usize,
        line_index: usize,
        target: &TextPosition,
    ) -> Result<Option<usize>> {
        let Some(snapshot) = self.page_table.pages().get(page.saturating_sub(1)) else {
            return Ok(None);
        };
        let chunk_index = snapshot.chunk;
        let line_base = self.page_table.chunks()[chunk_index].line_base;
        let Some(local_line) = line_index.checked_sub(line_base) else {
            return Ok(None);
        };
        self.with_active_chunk(chunk_index, |active| {
            let Some(line) = active.lines.get(local_line) else {
                return Ok(None);
            };
            let root = active.doc.root_element();
            let inline_root = root.with(line.node_id);
            let Some(layout) = inline_root
                .element_data()
                .and_then(|element| element.inline_layout_data.as_ref())
            else {
                return Ok(None);
            };
            let start = line.text_start.min(layout.text.len());
            let end = line.text_end.min(layout.text.len());
            if start >= end {
                return Ok(None);
            }
            let line_chars = layout.text[start..end].chars().count();
            let Some(byte) = xpointer::layout_offset_in_block(root, inline_root, target) else {
                return Ok(None);
            };
            if byte < start || byte > end {
                return Ok(None);
            }
            let chars = layout.text[start..byte].chars().count();
            Ok(Some(chars.min(line_chars.saturating_sub(1))))
        })
    }

    pub fn selection(&self, start: PagePosition, end: PagePosition) -> Result<Option<Selection>> {
        let Some(mut start_hit) = self.hit_test(start)? else {
            return Ok(None);
        };
        let Some(mut end_hit) = self.hit_test(end)? else {
            return Ok(None);
        };

        if (start_hit.page, start_hit.line_index, start_hit.byte_offset)
            > (end_hit.page, end_hit.line_index, end_hit.byte_offset)
        {
            std::mem::swap(&mut start_hit, &mut end_hit);
        }

        let mut rects = Vec::new();
        let mut text = String::new();
        let mut previous: Option<PageTextLine> = None;
        for page in start_hit.page..=end_hit.page {
            for (line_index, mut rect, current) in self.page_text_lines(page)? {
                if line_index < start_hit.line_index || line_index > end_hit.line_index {
                    continue;
                }
                let mut slice_start = 0;
                let mut slice_end = rect.text.len();
                if page == start_hit.page && line_index == start_hit.line_index {
                    slice_start = start_hit.byte_offset.min(slice_end);
                    let left = start_hit.point.x.clamp(rect.x, rect.x + rect.width);
                    rect.width = (rect.x + rect.width - left).max(0.0);
                    rect.x = left;
                }
                if page == end_hit.page && line_index == end_hit.line_index {
                    slice_end = end_hit.byte_offset.min(slice_end);
                    let right = end_hit.point.x.clamp(rect.x, rect.x + rect.width);
                    rect.width = (right - rect.x).max(0.0);
                }
                while slice_start < slice_end && !rect.text.is_char_boundary(slice_start) {
                    slice_start += 1;
                }
                while slice_end > slice_start && !rect.text.is_char_boundary(slice_end) {
                    slice_end -= 1;
                }
                if slice_start < slice_end {
                    let part = &rect.text[slice_start..slice_end];
                    if !part.trim().is_empty() {
                        if !text.is_empty() && !continues_same_line(previous, current) {
                            text.push('\n');
                        }
                        text.push_str(part);
                        previous = Some(current);
                    }
                }
                if rect.width > 0.0 {
                    rects.push(rect);
                }
            }
        }

        Ok(Some(Selection {
            text: text.trim_end().to_string(),
            xpointer_range: XPointerRange {
                start: start_hit.text_position,
                end: Some(end_hit.text_position),
                id: None,
            },
            rects,
        }))
    }

    fn page_meta(&self, page: usize) -> Result<&PageMeta> {
        let index = page.checked_sub(1).context("page numbers are 1-based")?;
        self.page_table
            .pages()
            .get(index)
            .with_context(|| format!("page {page} is outside 1..={}", self.page_count()))
    }

    pub(crate) fn page_snapshot_boxes(&self, page: usize) -> Result<Vec<SnapshotLine>> {
        if let Some((cached_page, cached)) = self.page_boxes_cache.borrow().as_ref()
            && *cached_page == page
        {
            return Ok(cached.clone());
        }
        let meta = self.page_meta(page)?;
        let (chunk_index, local_start, clip_bottom) =
            (meta.chunk, meta.local_start, meta.local_clip_bottom);
        let line_base = self.page_table.chunks()[chunk_index].line_base;
        let boxes = self.with_active_chunk(chunk_index, |active| {
            let root = active.doc.root_element();
            Ok(snapshot_page_boxes(
                root,
                &active.lines,
                local_start,
                clip_bottom,
                line_base,
            ))
        })?;
        *self.page_boxes_cache.borrow_mut() = Some((page, boxes.clone()));
        Ok(boxes)
    }

    fn page_text_boxes_with_lines(&self, page: usize) -> Result<Vec<(usize, TextBox)>> {
        Ok(self
            .page_text_lines(page)?
            .into_iter()
            .map(|(global_line, text_box, _)| (global_line, text_box))
            .collect())
    }

    /// Text boxes with the visual line they sit on and whether they belong to a
    /// formula, so a formula's tokens can be joined into one run of text.
    fn page_text_lines(&self, page: usize) -> Result<Vec<(usize, TextBox, PageTextLine)>> {
        let Some(index) = page.checked_sub(1) else {
            bail!("page numbers are 1-based");
        };
        if self.page_table.pages().get(index).is_none() {
            bail!("page {page} is outside 1..={}", self.page_count());
        }

        let snapshot = self.page_snapshot_boxes(page)?;
        let scale = self.viewport_scale;
        let fixed_layout = self
            .fixed_layouts
            .get(self.page_table.pages()[index].chunk)
            .and_then(|fixed| *fixed);
        let fixed = fixed_layout.map(|fixed| self.fixed_page_transform(fixed));
        Ok(snapshot
            .iter()
            .map(|line| {
                let (x, y, width, height) = if let Some((fit, offset_x, offset_y)) = fixed {
                    (
                        offset_x + line.x * fit,
                        offset_y + line.y * fit,
                        line.width * fit,
                        line.height * fit,
                    )
                } else {
                    (line.x, line.y, line.width, line.height)
                };
                (
                    line.global_line,
                    TextBox {
                        text: line.text.clone(),
                        page,
                        x: x * scale,
                        y: y * scale,
                        width: width * scale,
                        height: height * scale,
                    },
                    PageTextLine {
                        line_top: line.line_top * scale,
                        math: line.math,
                    },
                )
            })
            .collect())
    }
}

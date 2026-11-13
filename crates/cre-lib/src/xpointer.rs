use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use blitz_dom::node::TextLayout;
use blitz_dom::{Node, NodeId};
use blitz_html::HtmlDocument;

use crate::position::{PathSegment, TextPosition, parse_text_position};
use crate::{LineBox, PageStart, page_containing_top};

pub(crate) fn xpointer_from_page(
    doc: &HtmlDocument,
    lines: &[LineBox],
    page_starts: &[PageStart],
    page: usize,
    height: u32,
    page_margin: f32,
) -> Result<String> {
    let root = doc.root_element();
    // A page whose only content is a picture has no line to anchor to; point at
    // the element instead, which is what crengine does for those pages and what
    // makes bookmarks and the last-read position work on a comic.
    let position = match position_for_page(root, lines, page_starts, page, height, page_margin) {
        Ok(position) => position,
        Err(text_error) => {
            element_position_for_page(doc, root, page_starts, page, height, page_margin)
                .map_err(|element_error| element_error.context(text_error.to_string()))?
        }
    };
    Ok(position.to_xpointer())
}

pub(crate) fn page_from_xpointer(
    doc: &HtmlDocument,
    page_starts: &[PageStart],
    xpointer: &str,
) -> Result<usize> {
    let position = parse_text_position(xpointer)?;
    let root = doc.root_element();
    if position.element {
        let element_id = resolve_element_position(root, &position)?;
        return page_for_element(doc, root, element_id, page_starts);
    }
    let text_id = resolve_text_position(root, &position)?;
    page_for_text_node(doc, root, text_id, page_starts)
}

fn position_for_page(
    root: &Node,
    lines: &[LineBox],
    page_starts: &[PageStart],
    page: usize,
    height: u32,
    page_margin: f32,
) -> Result<TextPosition> {
    let line = line_for_page(lines, page_starts, page, height, page_margin)?;
    // The same mapping the word positions use: a page start and the words on
    // that page cannot disagree.
    position_for_line_offset(root, line, 0).or_else(|_| fallback_position_for_line(root, line))
}
fn line_for_page<'a>(
    lines: &'a [LineBox],
    page_starts: &[PageStart],
    page: usize,
    height: u32,
    page_margin: f32,
) -> Result<&'a LineBox> {
    let Some(index) = page.checked_sub(1) else {
        bail!("page numbers are 1-based");
    };
    if index >= page_starts.len() {
        bail!("page {page} is outside 1..={}", page_starts.len());
    }

    let start = page_starts[index];
    let lower = start.clip_top;
    let upper = page_starts
        .get(index + 1)
        .map_or(start.top + height as f32 - page_margin, |next| {
            next.clip_top
        });

    // Prefer a line that actually carries text: the first box of a page can be
    // an empty line (e.g. the box of a wrapper element), which has no source
    // position and would leave the page without an XPointer at all.
    let mut fallback = None;
    for line in lines {
        if line.top + 0.5 < lower || line.top + 0.5 >= upper {
            continue;
        }
        if line.text_end > line.text_start {
            return Ok(line);
        }
        fallback = fallback.or(Some(line));
    }
    fallback.context("page has no line box that can produce an XPointer")
}

fn fallback_position_for_line(root: &Node, line: &LineBox) -> Result<TextPosition> {
    let element_id = nearest_real_element_id(root, line.node_id)
        .with_context(|| "page line has no text node or element that can produce an XPointer")?;
    position_for_text_node(root, element_id, 0)
}

/// Exact mapping between a block's inline layout text and its source text
/// nodes, built from parley's runs.
///
/// Every run carries the brush of the element whose style span pushed its
/// text (`Cluster::style().brush`). Replaying the runs in layout order over
/// the text nodes that element owns recovers, for every layout character, the
/// text node and codepoint offset it came from. This is the only mapping the
/// XPointer code uses: it is derived from the same data blitz laid out, so it
/// cannot drift from what is rendered. Characters added by generated content
/// (or changed by a text-transform that cannot be inverted) are stored as
/// `None` and skipped when converting a position.
pub(crate) struct LayoutTextMap {
    /// `(layout byte offset, source position)` per character, in layout order.
    chars: Vec<(usize, Option<(NodeId, u32)>)>,
    /// `(codepoint offset, layout byte offset, byte length)` per text node, in
    /// layout order. Used for the inverse direction.
    nodes: HashMap<NodeId, Vec<(u32, usize, usize)>>,
}

impl LayoutTextMap {
    pub(crate) fn new(root: &Node, inline_layout: &TextLayout) -> Option<Self> {
        let mut pieces: Vec<(std::ops::Range<usize>, NodeId)> = Vec::new();
        for line in inline_layout.layout.lines() {
            for run in line.runs() {
                for cluster in run.clusters() {
                    let brush = cluster.style().brush.id;
                    let range = cluster.text_range();
                    match pieces.last_mut() {
                        Some((last, last_brush))
                            if *last_brush == brush && last.end == range.start =>
                        {
                            last.end = range.end;
                        }
                        _ => pieces.push((range, brush)),
                    }
                }
            }
        }
        let runs = pieces;
        let mut brushes = runs.iter().map(|(_, brush)| *brush).collect::<HashSet<_>>();
        // Out-of-flow boxes (a float such as a drop cap) also own text, but
        // their text is not part of this block's layout text, so they produce
        // no run here. Their inline-box id is still the element that owns the
        // text; stopping at it keeps that text out of the parent's cursor.
        brushes.extend(
            inline_layout
                .layout
                .inline_boxes()
                .map(|inline_box| NodeId::from_u64(inline_box.id)),
        );
        let mut cursors: HashMap<NodeId, RunTextCursor> = HashMap::new();
        let mut chars = Vec::new();
        let mut nodes: HashMap<NodeId, Vec<(u32, usize, usize)>> = HashMap::new();
        for (range, brush) in runs {
            let cursor = cursors
                .entry(brush)
                .or_insert_with(|| RunTextCursor::new(root, brush, &brushes));
            let text = inline_layout.text.get(range.clone())?;
            let mapping = cursor.align(text);
            for (char_index, (byte, character)) in text.char_indices().enumerate() {
                let position = mapping.get(char_index).copied().flatten();
                if let Some((node_id, offset)) = position {
                    nodes.entry(node_id).or_default().push((
                        offset,
                        range.start + byte,
                        character.len_utf8(),
                    ));
                }
                chars.push((range.start + byte, position));
            }
        }
        chars.sort_by_key(|(byte, _)| *byte);
        chars.dedup_by_key(|(byte, _)| *byte);
        for entries in nodes.values_mut() {
            entries.sort_by_key(|(offset, _, _)| *offset);
        }
        Some(Self { chars, nodes })
    }

    /// Text nodes that produced at least one layout character.
    pub(crate) fn source_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.nodes.keys().copied()
    }

    /// Source position for a layout byte offset. A `None` character (generated
    /// content) resolves to the next real character; past the end of the block
    /// it resolves to one past the last source character.
    pub(crate) fn position_at(&self, byte: usize) -> Option<(NodeId, u32)> {
        let index = self.chars.partition_point(|(offset, _)| *offset < byte);
        if let Some((_, Some(position))) = self.chars.get(index) {
            return Some(*position);
        }
        if let Some((_, position)) = self.chars[index..]
            .iter()
            .find(|(_, position)| position.is_some())
        {
            return *position;
        }
        let (_, position) = self.chars[..index]
            .iter()
            .rev()
            .find(|(_, position)| position.is_some())?;
        let (node_id, offset) = (*position)?;
        Some((node_id, offset.saturating_add(1)))
    }

    /// Source position after the last real character before `byte`. Used for a
    /// word's end: the character at `byte` is the boundary after the word and
    /// may belong to the next text node, while the position after the word's
    /// own last character stays in the same node.
    pub(crate) fn position_after(&self, byte: usize) -> Option<(NodeId, u32)> {
        let index = self.chars.partition_point(|(offset, _)| *offset < byte);
        let (_, position) = self.chars[..index]
            .iter()
            .rev()
            .find(|(_, position)| position.is_some())?;
        let (node_id, offset) = (*position)?;
        Some((node_id, offset.saturating_add(1)))
    }

    /// Layout byte offset of a source position. A position inside collapsed
    /// whitespace resolves to the next visible character; at the end of a node
    /// it resolves to the byte after its last character.
    pub(crate) fn layout_offset(&self, node_id: NodeId, codepoint_offset: u32) -> Option<usize> {
        let entries = self.nodes.get(&node_id)?;
        match entries.binary_search_by_key(&codepoint_offset, |(offset, _, _)| *offset) {
            Ok(index) => Some(entries[index].1),
            Err(index) => {
                if let Some((_, byte, _)) = entries.get(index) {
                    Some(*byte)
                } else {
                    let (_, byte, length) = *entries.last()?;
                    Some(byte + length)
                }
            }
        }
    }
}

/// Block and layout byte offset of a source position. This is the page table
/// key: both sides come from the same `LayoutTextMap`, so they always agree.
pub(crate) fn block_and_layout_offset(
    root: &Node,
    position: &TextPosition,
) -> Option<(NodeId, usize)> {
    let text_id = resolve_text_position(root, position).ok()?;
    let text = root.with(text_id);
    let block = text.inline_root_ancestor()?;
    let layout = block.element_data()?.inline_layout_data.as_ref()?;
    let map = LayoutTextMap::new(block, layout)?;
    let offset = map.layout_offset(text_id, position.codepoint_offset)?;
    Some((block.id, offset))
}

/// Layout byte offset of a source position inside `block`, or `None` when the
/// position belongs to another block.
pub(crate) fn layout_offset_in_block(
    root: &Node,
    block: &Node,
    position: &TextPosition,
) -> Option<usize> {
    block_and_layout_offset(root, position)
        .and_then(|(block_id, offset)| (block_id == block.id).then_some(offset))
}

pub(crate) fn position_for_layout_offset(
    root: &Node,
    block: &Node,
    byte_offset: usize,
) -> Option<TextPosition> {
    let layout = block.element_data()?.inline_layout_data.as_ref()?;
    let map = LayoutTextMap::new(block, layout)?;
    let (text_id, codepoint_offset) = map.position_at(byte_offset)?;
    position_for_text_node(root, text_id, codepoint_offset).ok()
}

pub(crate) fn position_after_layout_offset(
    root: &Node,
    block: &Node,
    byte_offset: usize,
) -> Option<TextPosition> {
    let layout = block.element_data()?.inline_layout_data.as_ref()?;
    let map = LayoutTextMap::new(block, layout)?;
    let (text_id, codepoint_offset) = map.position_after(byte_offset)?;
    position_for_text_node(root, text_id, codepoint_offset).ok()
}

pub(crate) fn position_for_line_offset(
    root: &Node,
    line: &LineBox,
    byte_offset: usize,
) -> Result<TextPosition> {
    let block = root.with(line.node_id);
    let layout = block
        .element_data()
        .and_then(|element| element.inline_layout_data.as_ref())
        .context("line has no inline layout")?;
    let target = line
        .text_start
        .saturating_add(byte_offset)
        .min(layout.text.len());
    let target = floor_char_boundary(&layout.text, target);
    let map = LayoutTextMap::new(block, layout).context("line block has no layout map")?;
    let (text_id, codepoint_offset) = map
        .position_at(target)
        .with_context(|| format!("line offset {target} has no source character"))?;
    position_for_text_node(root, text_id, codepoint_offset)
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Walks the text nodes owned by one brush element in document order, matching
/// the runs parley produced for that element.
struct RunTextCursor {
    nodes: Vec<(NodeId, Vec<char>)>,
    node_index: usize,
    char_index: usize,
}

impl RunTextCursor {
    fn new(root: &Node, element: NodeId, brushes: &HashSet<NodeId>) -> Self {
        let mut ids = Vec::new();
        collect_owned_text_nodes(root, element, false, brushes, &mut ids);
        let nodes = ids
            .into_iter()
            .filter_map(|id| {
                let content = root.with(id).text_data()?.content.clone();
                Some((id, content.chars().collect::<Vec<_>>()))
            })
            .collect();
        Self {
            nodes,
            node_index: 0,
            char_index: 0,
        }
    }

    fn position(&self) -> Option<(NodeId, u32)> {
        let (node_id, chars) = self.nodes.get(self.node_index)?;
        Some((*node_id, self.char_index.min(chars.len()) as u32))
    }

    /// Maps each char of `text` to the source character it came from. Chars the
    /// layout added, or transformed beyond recognition, become `None`; they do
    /// not consume source text.
    fn align(&mut self, text: &str) -> Vec<Option<(NodeId, u32)>> {
        if text == "\n" {
            // A `<br>` run: it consumes no character of the surrounding node.
            return vec![self.position()];
        }
        let mut out = Vec::with_capacity(text.chars().count());
        for run_char in text.chars() {
            loop {
                let Some((node_id, chars)) = self.nodes.get(self.node_index) else {
                    out.push(None);
                    break;
                };
                if self.char_index >= chars.len() {
                    self.node_index += 1;
                    self.char_index = 0;
                    continue;
                }
                let node_char = chars[self.char_index];
                if run_char.is_whitespace() {
                    out.push(Some((*node_id, self.char_index as u32)));
                    if node_char.is_whitespace() {
                        while self.char_index < chars.len()
                            && chars[self.char_index].is_whitespace()
                        {
                            self.char_index += 1;
                        }
                    }
                    // Otherwise the space was collapsed in between two nodes.
                    break;
                }
                if node_char.is_whitespace() {
                    // Parley dropped this whitespace when it built the text.
                    while self.char_index < chars.len() && chars[self.char_index].is_whitespace() {
                        self.char_index += 1;
                    }
                    continue;
                }
                if !chars_equal_fold(node_char, run_char) {
                    if self.char_index == 0 {
                        // A text node that did not contribute to the layout
                        // (for example under `display: none`) never matches the
                        // run. Skip it and keep looking in this element's next
                        // text node; the run may continue there.
                        self.node_index += 1;
                        self.char_index = 0;
                        continue;
                    }
                    out.push(None);
                    break;
                }
                out.push(Some((*node_id, self.char_index as u32)));
                self.char_index += 1;
                break;
            }
        }
        out
    }
}

/// Text nodes that belong to `element`'s style span: its text descendants,
/// stopping at elements that opened a span of their own (they are runs with a
/// different brush). Elements that did not push a span (`display: contents`)
/// are transparent, so their text stays with the ancestor.
fn collect_owned_text_nodes(
    root: &Node,
    element: NodeId,
    nested: bool,
    brushes: &HashSet<NodeId>,
    out: &mut Vec<NodeId>,
) {
    for child in root.with(element).children.iter().copied() {
        let node = root.with(child);
        if node.is_text_node() {
            if !nested {
                out.push(child);
            }
        } else if node.element_data().is_some() {
            collect_owned_text_nodes(
                root,
                child,
                nested || brushes.contains(&child),
                brushes,
                out,
            );
        }
    }
}

fn chars_equal_fold(left: char, right: char) -> bool {
    if left == right || left.eq_ignore_ascii_case(&right) {
        return true;
    }
    let mut left = left.to_lowercase();
    let mut right = right.to_lowercase();
    match (left.next(), left.next(), right.next(), right.next()) {
        (Some(left), None, Some(right), None) => left == right,
        _ => false,
    }
}

fn nearest_real_element_id(root: &Node, node_id: NodeId) -> Option<NodeId> {
    let mut current_id = node_id;
    loop {
        let current = root.with(current_id);
        if current.element_data().is_some() && !current.is_anonymous() {
            return Some(current_id);
        }
        current_id = current.parent?;
    }
}

pub(crate) fn position_for_text_node(
    root: &Node,
    target_id: NodeId,
    codepoint_offset: u32,
) -> Result<TextPosition> {
    // Positions must name a valid offset inside the node: a position at the
    // end of a block resolves to one past its last character, and an
    // out-of-range offset makes the range unusable for boxes and selections.
    let codepoint_offset = root
        .with(target_id)
        .text_data()
        .map(|text| text.content.chars().count() as u32)
        .map_or(codepoint_offset, |length| codepoint_offset.min(length));
    let body_id = find_body_id(root).context("document has no body element")?;
    let spine_root_id = find_spine_ancestor_id(root, target_id, body_id).unwrap_or(body_id);
    let spine_index = if spine_root_id == body_id {
        1
    } else {
        spine_index_for_node(root.with(spine_root_id)).unwrap_or(1)
    };

    let path = path_to_node(root, target_id, spine_root_id)?;
    let text_index = text_index_for_node(root, target_id);
    Ok(TextPosition {
        spine_index,
        path,
        text_index,
        codepoint_offset,
        element: false,
    })
}

/// XPointer path of `target_id` below `spine_root_id`.
fn path_to_node(root: &Node, target_id: NodeId, spine_root_id: NodeId) -> Result<Vec<PathSegment>> {
    let mut path = Vec::new();
    let mut current_id = target_id;
    while current_id != spine_root_id {
        let current = root.with(current_id);
        let parent_id = match current.parent {
            Some(parent_id) => parent_id,
            None => break,
        };
        if let Some(tag) = element_tag(current)
            && !current.is_anonymous()
        {
            let index = element_child_index(root.with(parent_id), &tag, current_id)?;
            path.push(PathSegment { tag, index });
        }
        current_id = parent_id;
    }
    path.reverse();
    Ok(path)
}

/// A position for a page that carries no text: the element that starts it.
///
/// Crengine points at the element in that case (`…/body/div[3]/img`), which is
/// what makes `getXPointer`, bookmarks and the last-read position work on a
/// picture page.
fn element_position_for_page(
    doc: &HtmlDocument,
    root: &Node,
    page_starts: &[PageStart],
    page: usize,
    height: u32,
    page_margin: f32,
) -> Result<TextPosition> {
    let index = page.checked_sub(1).context("page numbers are 1-based")?;
    let start = *page_starts
        .get(index)
        .with_context(|| format!("page {page} is outside 1..={}", page_starts.len()))?;
    let upper = page_starts
        .get(index + 1)
        .map_or(start.top + height as f32 - page_margin, |next| {
            next.clip_top
        });

    // The deepest element whose box starts on this page (an `<img>` rather than
    // the `<html>` that also starts there), else the last one overlapping it.
    let mut on_page: Option<(f32, NodeId)> = None;
    let mut overlapping: Option<NodeId> = None;
    collect_element_boxes(doc, root, &mut |id, top, bottom| {
        if bottom <= start.clip_top || top >= upper {
            return;
        }
        if top + 1.0 >= start.clip_top {
            // Later elements win ties: document order puts a child after its
            // parent, so the innermost box that starts here is kept.
            match on_page {
                Some((best_top, _)) if top < best_top - 0.5 => {}
                _ => on_page = Some((top, id)),
            }
        } else {
            overlapping = Some(id);
        }
    });
    let element_id = on_page
        .map(|(_, id)| id)
        .or(overlapping)
        .context("page has no element that can produce an XPointer")?;

    let body_id = find_body_id(root).context("document has no body element")?;
    let spine_root_id = find_spine_ancestor_id(root, element_id, body_id).unwrap_or(body_id);
    let spine_index = if spine_root_id == body_id {
        1
    } else {
        spine_index_for_node(root.with(spine_root_id)).unwrap_or(1)
    };
    Ok(TextPosition {
        spine_index,
        path: path_to_node(root, element_id, spine_root_id)?,
        text_index: None,
        codepoint_offset: 0,
        element: true,
    })
}

/// Calls `visit` with every laid-out element box, in document order.
fn collect_element_boxes(
    doc: &HtmlDocument,
    node: &Node,
    visit: &mut impl FnMut(NodeId, f32, f32),
) {
    if node.element_data().is_some()
        && !node.is_anonymous()
        && let Some(rect) = doc.get_client_bounding_rect(node.id)
    {
        let top = rect.y as f32;
        visit(node.id, top, top + rect.height as f32);
    }
    for child in &node.children {
        collect_element_boxes(doc, node.with(*child), visit);
    }
}

/// 1-based index of a text node among its parent's text children, when the
/// parent has more than one (crengine's `/text()[k]`).
fn text_index_for_node(root: &Node, node_id: NodeId) -> Option<u32> {
    let node = root.with(node_id);
    if !node.is_text_node() {
        return None;
    }
    let parent_id = node.parent?;
    let parent = root.with(parent_id);
    let mut index = 0u32;
    let mut total = 0u32;
    for child in &parent.children {
        let child = root.with(*child);
        if child.is_text_node() {
            total += 1;
            if child.id == node_id {
                index = total;
            }
        }
    }
    (total > 1).then_some(index)
}

/// Document order of two source positions.
///
/// `TextPosition`'s `Ord` compares path segments by tag, so a child span
/// (`span[2]`) sorts after its parent's direct text (`p` -> `text()`), even
/// though the span's text comes first in the document. Resolving both
/// positions to their text nodes and comparing node ids (which increase in
/// document order) gives the order crengine's `ldomXPointer::compare` uses.
pub(crate) fn compare_text_positions(
    root: &Node,
    left: &TextPosition,
    right: &TextPosition,
) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    let left_id = resolve_any_position(root, left).ok()?;
    let right_id = resolve_any_position(root, right).ok()?;
    Some(match left_id.cmp(&right_id) {
        // An element position has no character offset to break a tie with.
        Ordering::Equal if left.element || right.element => Ordering::Equal,
        Ordering::Equal => left.codepoint_offset.cmp(&right.codepoint_offset),
        order => order,
    })
}

/// The node a position names: the element itself, or its text node.
fn resolve_any_position(root: &Node, position: &TextPosition) -> Result<NodeId> {
    if position.element {
        resolve_element_position(root, position)
    } else {
        resolve_text_position(root, position)
    }
}

pub(crate) fn resolve_text_position(root: &Node, position: &TextPosition) -> Result<NodeId> {
    let current_id = resolve_element_position(root, position)?;
    let current = root.with(current_id);
    if let Some(wanted) = position.text_index {
        let mut seen = 0u32;
        for child in &current.children {
            let child = root.with(*child);
            if child.is_text_node() {
                seen += 1;
                if seen == wanted {
                    return Ok(child.id);
                }
            }
        }
    }
    // `/text()` in crengine selects a direct text child of the element, not
    // the first text node anywhere below it (3.epub's drop cap: the direct
    // text of the paragraph starts after a float span, and picking the first
    // descendant text node returned the drop cap letter instead).
    first_direct_text_node(current, false)
        .or_else(|| first_direct_text_node(current, true))
        .or_else(|| first_text_node(current, false))
        .or_else(|| first_text_node(current, true))
        .or(Some(current_id))
        .context("XPointer target element has no text node")
}

fn first_direct_text_node(node: &Node, allow_whitespace: bool) -> Option<NodeId> {
    node.children.iter().copied().find(|child_id| {
        let child = node.with(*child_id);
        child.is_text_node() && text_content_ok(child, allow_whitespace)
    })
}

pub(crate) fn resolve_element_position(root: &Node, position: &TextPosition) -> Result<NodeId> {
    let body_id = find_body_id(root).context("document has no body element")?;
    let section_id = find_spine_section_id(root, body_id, position.spine_index);

    let mut current_id = match (position.spine_index, section_id) {
        (1, Some(section_id)) => section_id,
        (1, None) => body_id,
        (_, Some(section_id)) => section_id,
        (_, None) => bail!(
            "XPointer DocFragment[{}] was not found in the rendered document",
            position.spine_index
        ),
    };

    for segment in &position.path {
        let current = root.with(current_id);
        current_id = find_element_child_id(current, &segment.tag, segment.index)?;
    }

    Ok(current_id)
}

fn find_body_id(node: &Node) -> Option<NodeId> {
    if element_tag(node).as_deref() == Some("body") {
        return Some(node.id);
    }
    for child in node.children.iter() {
        if let Some(id) = find_body_id(node.with(*child)) {
            return Some(id);
        }
    }
    None
}

fn find_spine_ancestor_id(root: &Node, target_id: NodeId, body_id: NodeId) -> Option<NodeId> {
    let mut current_id = target_id;
    loop {
        let current = root.with(current_id);
        let parent_id = current.parent?;
        if parent_id == body_id {
            return None;
        }
        if spine_index_for_node(root.with(parent_id)).is_some() {
            return Some(parent_id);
        }
        current_id = parent_id;
    }
}

fn find_spine_section_id(root: &Node, body_id: NodeId, spine_index: u32) -> Option<NodeId> {
    let body = root.with(body_id);
    find_spine_section_under(body, spine_index)
}

fn find_spine_section_under(node: &Node, spine_index: u32) -> Option<NodeId> {
    if spine_index_for_node(node).is_some_and(|index| index == spine_index) {
        return Some(node.id);
    }
    for child in node.children.iter() {
        if let Some(id) = find_spine_section_under(node.with(*child), spine_index) {
            return Some(id);
        }
    }
    None
}

fn find_element_child_id(parent: &Node, tag: &str, wanted_index: u32) -> Result<NodeId> {
    let mut index = 0;
    for child_id in parent.children.iter() {
        let child = parent.with(*child_id);
        if let Some(child_tag) = element_tag(child)
            && child_tag == tag
        {
            index += 1;
            if index == wanted_index {
                return Ok(*child_id);
            }
        }
    }
    bail!("XPointer path element {tag}[{wanted_index}] was not found in the rendered document")
}

fn element_child_index(parent: &Node, tag: &str, target_id: NodeId) -> Result<u32> {
    let mut index = 0;
    for child_id in parent.children.iter() {
        if let Some(child_tag) = element_tag(parent.with(*child_id))
            && child_tag == tag
        {
            index += 1;
            if *child_id == target_id {
                return Ok(index);
            }
        }
    }
    bail!("XPointer path element {tag} child index could not be found")
}

fn first_text_node(node: &Node, allow_whitespace: bool) -> Option<NodeId> {
    if node.is_text_node() && text_content_ok(node, allow_whitespace) {
        return Some(node.id);
    }
    for child in node.children.iter() {
        if let Some(id) = first_text_node(node.with(*child), allow_whitespace) {
            return Some(id);
        }
    }
    None
}

fn text_content_ok(node: &Node, allow_whitespace: bool) -> bool {
    node.text_data()
        .is_some_and(|data| allow_whitespace || !data.content.trim().is_empty())
}

fn element_tag(node: &Node) -> Option<String> {
    node.element_data()
        .map(|element| element.name.local.to_string())
}

fn element_ancestor_id(root: &Node, node_id: NodeId) -> Option<NodeId> {
    let mut current_id = node_id;
    loop {
        let current = root.with(current_id);
        if current.element_data().is_some() {
            return Some(current_id);
        }
        current_id = current.parent?;
    }
}

fn spine_index_for_node(node: &Node) -> Option<u32> {
    node_attr(node, "data-spine-index")
        .and_then(|value| value.parse::<u32>().ok())
        .map(|value| value + 1)
}

fn node_attr<'a>(node: &'a Node, name: &str) -> Option<&'a str> {
    node.attrs()?
        .iter()
        .find(|attr| attr.name.local.to_string() == name)
        .map(|attr| attr.value.as_str())
}

fn page_for_element(
    doc: &HtmlDocument,
    root: &Node,
    element_id: NodeId,
    page_starts: &[PageStart],
) -> Result<usize> {
    // An empty anchor (`<a id="…"></a>`) has no box; the page it means is the
    // one that holds the content following it.
    let top = match doc.get_client_bounding_rect(element_id) {
        Some(rect) => rect.y as f32,
        None => {
            first_rect_after(doc, root, element_id).context("XPointer target has no layout rect")?
        }
    };
    Ok(page_containing_top(top, page_starts))
}

/// Y of the first laid-out element after `target` in document order.
fn first_rect_after(doc: &HtmlDocument, root: &Node, target: NodeId) -> Option<f32> {
    fn walk(doc: &HtmlDocument, node: &Node, target: NodeId, seen: &mut bool) -> Option<f32> {
        if node.id == target {
            *seen = true;
        } else if *seen
            && node.element_data().is_some()
            && let Some(rect) = doc.get_client_bounding_rect(node.id)
        {
            return Some(rect.y as f32);
        }
        for child in &node.children {
            if let Some(top) = walk(doc, node.with(*child), target, seen) {
                return Some(top);
            }
        }
        None
    }
    let mut seen = false;
    walk(doc, root, target, &mut seen)
}

fn page_for_text_node(
    doc: &HtmlDocument,
    root: &Node,
    text_id: NodeId,
    page_starts: &[PageStart],
) -> Result<usize> {
    let element_id =
        element_ancestor_id(root, text_id).context("XPointer target is not inside an element")?;
    let rect = doc
        .get_client_bounding_rect(element_id)
        .context("XPointer target has no layout rect")?;
    let top = rect.y as f32;
    let mut page = 1usize;
    for (index, start) in page_starts.iter().enumerate() {
        if top + 1.0 >= start.clip_top {
            page = index + 1;
        } else {
            break;
        }
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_folding_matches_ascii_and_single_char_lowercase() {
        assert!(chars_equal_fold('A', 'a'));
        assert!(chars_equal_fold('\u{c9}', '\u{e9}'));
        assert!(!chars_equal_fold('a', 'b'));
    }
}

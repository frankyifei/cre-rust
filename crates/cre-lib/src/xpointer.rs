use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use blitz_dom::{Node, NodeId};
use blitz_html::HtmlDocument;

use crate::position::{PathSegment, TextPosition, parse_text_position};
use crate::{LineBox, PageStart};

pub(crate) fn page_xpointers(
    root: &Node,
    lines: &[LineBox],
    page_starts: &[PageStart],
    height: u32,
    page_margin: f32,
    xpointer_pages: &mut HashMap<String, usize>,
) -> Vec<Option<String>> {
    let mut out = Vec::with_capacity(page_starts.len());
    for page in 1..=page_starts.len() {
        let xpointer = position_for_page_unique(
            root,
            lines,
            page_starts,
            page,
            height,
            page_margin,
            xpointer_pages,
        )
        .map_or_else(|_| None, |position| Some(position.to_xpointer()));
        if let Some(xpointer) = &xpointer {
            xpointer_pages.insert(xpointer.clone(), page);
        }
        out.push(xpointer);
    }
    out
}

pub(crate) fn xpointer_from_page(
    doc: &HtmlDocument,
    lines: &[LineBox],
    page_starts: &[PageStart],
    page: usize,
    height: u32,
    page_margin: f32,
) -> Result<String> {
    let root = doc.root_element();
    let position = position_for_page(root, lines, page_starts, page, height, page_margin)?;
    Ok(position.to_xpointer())
}

pub(crate) fn page_from_xpointer(
    doc: &HtmlDocument,
    page_starts: &[PageStart],
    xpointer: &str,
) -> Result<usize> {
    let position = parse_text_position(xpointer)?;
    let root = doc.root_element();
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
    if let Some(position) = source_position_for_line(root, line) {
        return Ok(position);
    }
    fallback_position_for_line(root, line)
}

#[allow(clippy::too_many_arguments)]
fn position_for_page_unique(
    root: &Node,
    lines: &[LineBox],
    page_starts: &[PageStart],
    page: usize,
    height: u32,
    page_margin: f32,
    used: &HashMap<String, usize>,
) -> Result<TextPosition> {
    let line = line_for_page(lines, page_starts, page, height, page_margin)?;
    if let Some(position) = source_position_for_line(root, line) {
        let xpointer = position.to_xpointer();
        if !used.contains_key(&xpointer) {
            return Ok(position);
        }
    }

    let position = fallback_position_for_line(root, line)?;
    let xpointer = position.to_xpointer();
    if !used.contains_key(&xpointer) {
        return Ok(position);
    }
    Ok(position)
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

    lines
        .iter()
        .find(|line| line.top + 0.5 >= lower && line.top + 0.5 < upper)
        .context("page has no line box that can produce an XPointer")
}

fn fallback_position_for_line(root: &Node, line: &LineBox) -> Result<TextPosition> {
    let text_id = text_node_for_line(root, line)?;
    let content_len = root
        .with(text_id)
        .text_data()
        .map_or(0, |text| text.content.chars().count() as u32);
    let codepoint_offset = u32::try_from(line.text_start)
        .unwrap_or(u32::MAX)
        .min(content_len);
    position_for_text_node(root, text_id, codepoint_offset)
}

fn source_position_for_line(root: &Node, line: &LineBox) -> Option<TextPosition> {
    let (text_id, codepoint_offset) = source_text_position_for_line(root, line)?;
    position_for_text_node(root, text_id, codepoint_offset).ok()
}

pub(crate) fn position_for_line_offset(
    root: &Node,
    line: &LineBox,
    byte_offset: usize,
) -> Result<TextPosition> {
    let inline_root = root.with(line.node_id);
    let inline_layout = inline_root
        .element_data()
        .and_then(|element| element.inline_layout_data.as_ref())
        .context("line has no inline layout")?;
    let text_len = inline_layout.text.len();
    let line_start = line.text_start.min(text_len);
    let line_end = line.text_end.clamp(line_start, text_len);
    let segment = inline_layout
        .text
        .get(line_start..line_end)
        .context("line text range is not a valid string range")?;

    let mut offset = byte_offset.min(segment.len());
    while offset > 0 && !segment.is_char_boundary(offset) {
        offset -= 1;
    }

    if offset < segment.len()
        && !segment[offset..].trim().is_empty()
        && let Some((text_id, codepoint_offset)) =
            map_line_text_position(inline_root, line.source_element_id, &segment[offset..])
    {
        return position_for_text_node(root, text_id, codepoint_offset);
    }

    let (text_id, codepoint_offset) =
        map_line_text_position(inline_root, line.source_element_id, segment)
            .context("line has no text source position")?;
    let char_offset = segment[..offset].chars().count() as u32;
    let content_len = root
        .with(text_id)
        .text_data()
        .map(|text| text.content.chars().count() as u32);
    let final_offset = (codepoint_offset + char_offset).min(content_len.unwrap_or(u32::MAX));
    position_for_text_node(root, text_id, final_offset)
}

fn source_text_position_for_line(root: &Node, line: &LineBox) -> Option<(NodeId, u32)> {
    let inline_root = root.with(line.node_id);
    let inline_layout = inline_root.element_data()?.inline_layout_data.as_ref()?;
    let text_len = inline_layout.text.len();
    let start = line.text_start.min(text_len);
    let end = line.text_end.clamp(start, text_len);
    let segment = inline_layout.text.get(start..end)?;
    if segment.is_empty() {
        return None;
    }
    map_line_text_position(inline_root, line.source_element_id, segment)
}

fn map_line_text_position(
    inline_root: &Node,
    source_element_id: Option<NodeId>,
    segment: &str,
) -> Option<(NodeId, u32)> {
    let source_id = source_element_id.unwrap_or(inline_root.id);
    let source_node = inline_root.with(source_id);
    let text_node_ids = collect_text_node_ids_vec(source_node);
    let candidates = text_node_ids
        .into_iter()
        .filter_map(|id| {
            let node = inline_root.with(id);
            let content = node.text_data()?.content.as_str();
            let (normalized, codepoint_map) = normalize_text_with_codepoint_map(content);
            (!normalized.is_empty()).then_some(NormalizedTextNode {
                id,
                content: content.to_string(),
                normalized,
                codepoint_map,
            })
        })
        .collect::<Vec<_>>();

    let mut bases = vec![segment.trim_end_matches(is_whitespace)];
    let trimmed = segment
        .trim_start_matches(is_whitespace)
        .trim_end_matches(is_whitespace);
    if trimmed != bases[0] {
        bases.push(trimmed);
    }

    for base in bases {
        let char_count = base.chars().count();
        for take in (1..=char_count).rev() {
            let prefix: String = base.chars().take(take).collect();
            for candidate in &candidates {
                if let Some(offset) = find_normalized_text_offset(candidate, &prefix) {
                    return Some((candidate.id, offset));
                }
            }
        }
    }

    let text_id = first_text_node(source_node, false)
        .or_else(|| first_text_node(inline_root, false))
        .or_else(|| first_text_node(source_node, true))?;
    Some((text_id, 0))
}

struct NormalizedTextNode {
    id: NodeId,
    content: String,
    normalized: String,
    codepoint_map: Vec<usize>,
}

fn normalize_text_with_codepoint_map(content: &str) -> (String, Vec<usize>) {
    let mut normalized = String::new();
    let mut codepoint_map = Vec::new();
    let chars = content.chars().collect::<Vec<_>>();
    let mut index = 0usize;
    let mut codepoint_offset = 0usize;

    while index < chars.len() {
        if chars[index].is_whitespace() {
            let start_codepoint = codepoint_offset;
            while index < chars.len() && chars[index].is_whitespace() {
                index += 1;
                codepoint_offset += 1;
            }
            normalized.push(' ');
            codepoint_map.push(start_codepoint);
        } else {
            normalized.push(chars[index]);
            codepoint_map.push(codepoint_offset);
            index += 1;
            codepoint_offset += 1;
        }
    }

    (normalized, codepoint_map)
}

fn find_normalized_text_offset(candidate: &NormalizedTextNode, prefix: &str) -> Option<u32> {
    let byte_pos = candidate.normalized.find(prefix)?;
    let char_pos = candidate.normalized[..byte_pos].chars().count();
    let raw_offset = *candidate.codepoint_map.get(char_pos)?;
    Some(first_non_whitespace_codepoint_offset(
        &candidate.content,
        raw_offset,
    ))
}

fn first_non_whitespace_codepoint_offset(content: &str, start: usize) -> u32 {
    for (offset, ch) in content.chars().enumerate() {
        if offset >= start && !ch.is_whitespace() {
            return offset as u32;
        }
    }
    start as u32
}

fn is_whitespace(ch: char) -> bool {
    ch.is_whitespace()
}

fn collect_text_node_ids(node: &Node, out: &mut Vec<NodeId>) {
    if node.is_text_node() {
        out.push(node.id);
    }
    for child in node.children.iter() {
        collect_text_node_ids(node.with(*child), out);
    }
}

fn collect_text_node_ids_vec(node: &Node) -> Vec<NodeId> {
    let mut out = Vec::new();
    collect_text_node_ids(node, &mut out);
    out
}

fn text_node_for_line(root: &Node, line: &LineBox) -> Result<NodeId> {
    if let Some(text_id) = first_text_node(root.with(line.node_id), false) {
        return Ok(text_id);
    }

    let element_id = nearest_real_element_id(root, line.node_id).unwrap_or(line.node_id);
    if let Some(text_id) = first_text_node(root.with(element_id), true) {
        return Ok(text_id);
    }

    bail!("page line has no text node that can produce an XPointer")
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
    let body_id = find_body_id(root).context("document has no body element")?;
    let spine_root_id = find_spine_ancestor_id(root, target_id, body_id).unwrap_or(body_id);
    let spine_index = if spine_root_id == body_id {
        1
    } else {
        spine_index_for_node(root.with(spine_root_id)).unwrap_or(1)
    };

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
    Ok(TextPosition {
        spine_index,
        path,
        codepoint_offset,
    })
}

pub(crate) fn resolve_text_position(root: &Node, position: &TextPosition) -> Result<NodeId> {
    let current_id = resolve_element_position(root, position)?;
    let current = root.with(current_id);
    first_text_node(current, false)
        .or_else(|| first_text_node(current, true))
        .context("XPointer target element has no text node")
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
    fn normalizes_whitespace_and_maps_unicode_scalar_offsets() {
        let input = "a\u{e9}\u{1f600}  \nb";
        let (normalized, map) = normalize_text_with_codepoint_map(input);
        assert_eq!(normalized, "a\u{e9}\u{1f600} b");
        assert_eq!(map, [0, 1, 2, 3, 6]);
        assert_eq!(first_non_whitespace_codepoint_offset(input, 3), 6);
    }
}

//! CSS page-break support for the pagination pass.
//!
//! `blitz`/`stylo` do not compute the fragmentation properties (`page-break-*`,
//! `break-*`), so these declarations are read here directly. Two sources are
//! supported:
//!
//! * the `style` attribute of an element (the classic EPUB idiom), and
//! * rules inside `<style>` elements of the document, matched with
//!   `BaseDocument::matches_selector`, so combinators, attribute selectors and
//!   pseudo-classes behave like they do in the browser.
//!
//! `page-break-inside: avoid` is not implemented, nor are `left`/`right`
//! forced verso semantics (they behave like `always`).

use blitz_dom::{BaseDocument, Node};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BreakDirection {
    Before,
    After,
}

#[derive(Clone, Debug)]
struct BreakRule {
    /// Selector list as written; matched with `BaseDocument::matches_selector`.
    selectors: Vec<String>,
    direction: BreakDirection,
}

#[derive(Clone, Copy, Default, Debug)]
struct BreakFlags {
    before: Option<bool>,
    after: Option<bool>,
}

pub(crate) fn collect_forced_breaks(doc: &BaseDocument, root: &Node) -> Vec<f32> {
    let mut rules = Vec::new();
    let mut style_sources = Vec::new();
    collect_style_text(root, &mut style_sources);
    for css in &style_sources {
        rules.extend(parse_css_rules(css));
    }

    let mut breaks = Vec::new();
    collect_break_positions(doc, root, &rules, &mut breaks);
    breaks.sort_by(f32::total_cmp);
    breaks.dedup_by(|a, b| (*a - *b).abs() < 0.5);
    breaks
}

fn collect_style_text(node: &Node, out: &mut Vec<String>) {
    if let Some(element) = node.element_data()
        && element.name.local.to_string() == "style"
    {
        let mut text = String::new();
        for child in node.children.iter() {
            if let Some(data) = node.with(*child).text_data() {
                text.push_str(&data.content);
            }
        }
        let cleaned = clean_style_text(&text);
        if !cleaned.trim().is_empty() {
            out.push(cleaned);
        }
    }
    for child in node.children.iter() {
        collect_style_text(node.with(*child), out);
    }
}

fn clean_style_text(text: &str) -> String {
    let without_cdata = text.replace("<![CDATA[", "").replace("]]>", "");
    let mut out = String::with_capacity(without_cdata.len());
    let mut rest = without_cdata.as_str();
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        rest = match rest[start..].find("-->") {
            Some(offset) => &rest[start + offset + 3..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

fn parse_css_rules(css: &str) -> Vec<BreakRule> {
    let css = strip_css_comments(css);
    let mut rules = Vec::new();
    let mut rest = css.as_str();
    while let Some(open) = rest.find('{') {
        let (before, after_open) = rest.split_at(open);
        let selector_text = match before.rfind('}') {
            Some(index) => &before[index + 1..],
            None => before,
        };
        let selector = selector_text.trim();
        if selector.starts_with('@') {
            match balanced_block_end(after_open) {
                Some(end) => rest = &after_open[end + 1..],
                None => break,
            }
            continue;
        }
        let close = match after_open.find('}') {
            Some(offset) => offset,
            None => break,
        };
        let declarations = &after_open[1..close];
        rest = &after_open[close + 1..];
        if let Some(rule) = break_rule_from(selector, declarations) {
            rules.push(rule);
        }
    }
    rules
}

/// Drops the `[data-spine-id="…"]` prefix that scoped spine-document styles
/// carry, so the simple-selector parser below sees the original rule.
fn balanced_block_end(text: &str) -> Option<usize> {
    let mut depth = 1usize;
    for (index, ch) in text.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn strip_css_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        rest = match rest[start..].find("*/") {
            Some(offset) => &rest[start + offset + 2..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

fn break_rule_from(selector_text: &str, declarations: &str) -> Option<BreakRule> {
    let flags = break_flags(declarations);
    let before = flags.before.unwrap_or(false);
    let after = flags.after.unwrap_or(false);
    if !before && !after {
        return None;
    }
    let direction = if before {
        BreakDirection::Before
    } else {
        BreakDirection::After
    };
    let mut selectors = Vec::new();
    for part in selector_text.split(',') {
        let selector = part.trim();
        // An at-rule block (`@media`, `@supports`) wraps rules; its own text is
        // not a selector, the inner rules are scanned separately.
        if selector.is_empty() || selector.starts_with('@') {
            continue;
        }
        selectors.push(selector.to_string());
    }
    if selectors.is_empty() {
        return None;
    }
    Some(BreakRule {
        selectors,
        direction,
    })
}

fn break_flags(declarations: &str) -> BreakFlags {
    let mut flags = BreakFlags::default();
    for declaration in declarations.split(';') {
        let Some((name, value)) = declaration.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "page-break-before" | "break-before" => flags.before = Some(is_forced_value(value)),
            "page-break-after" | "break-after" => flags.after = Some(is_forced_value(value)),
            _ => {}
        }
    }
    flags
}

fn is_forced_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "always" | "page" | "left" | "right"
    )
}

fn node_attr<'a>(node: &'a Node, name: &str) -> Option<&'a str> {
    node.attrs()?
        .iter()
        .find(|attr| attr.name.local.to_string() == name)
        .map(|attr| attr.value.as_str())
}

fn collect_break_positions(
    doc: &BaseDocument,
    node: &Node,
    rules: &[BreakRule],
    out: &mut Vec<f32>,
) {
    if node.element_data().is_some() {
        let layout = node.unrounded_layout();
        let height = layout.size.height;
        if layout.size.width > 0.0 && height > 0.0 {
            let flags = break_flags_for(doc, node, rules);
            let inset = layout.padding + layout.border;
            let origin = node.absolute_position(inset.left, inset.top);
            if flags.before.unwrap_or(false) {
                out.push(origin.y);
            }
            if flags.after.unwrap_or(false) {
                out.push(origin.y + height);
            }
        }
    }
    if let Some(layout_children) = node.layout_children.borrow().as_ref() {
        for child in layout_children.iter() {
            collect_break_positions(doc, node.with(*child), rules, out);
        }
    }
}

fn break_flags_for(doc: &BaseDocument, node: &Node, rules: &[BreakRule]) -> BreakFlags {
    let mut from_rules = BreakFlags::default();
    for rule in rules {
        if rule
            .selectors
            .iter()
            .any(|selector| doc.matches_selector(node.id, selector).unwrap_or(false))
        {
            match rule.direction {
                BreakDirection::Before => from_rules.before = Some(true),
                BreakDirection::After => from_rules.after = Some(true),
            }
        }
    }
    let inline = node_attr(node, "style").map(break_flags);
    BreakFlags {
        before: inline.and_then(|flags| flags.before).or(from_rules.before),
        after: inline.and_then(|flags| flags.after).or(from_rules.after),
    }
}

//! Small MathML presentation-subset translator for the Blitz-based engine.
//!
//! Blitz has no MathML layout. The approach here follows crengine's
//! `MathMLHelper`: transform the MathML tree into ordinary HTML boxes that the
//! existing HTML layout understands (inline blocks, `sup`/`sub`, tables and
//! borders), instead of implementing a separate math layout engine.
//!
//! This is deliberately a presentation subset: tokens, `mrow`, `mfenced`,
//! `msup`/`msub`/`msubsup`, `mfrac`, `msqrt`/`mroot`, `mtable`, `mspace`,
//! `mstyle`, `merror`, `mphantom` and `menclose`. Stretchy-operator shaping,
//! OpenType MATH metrics and the finer mtable/multiscript rules remain future
//! work.

use roxmltree::{Document, Node};

/// CSS needed by the generated HTML. The engine appends this once per chunk.
pub const CSS: &str = r#"
.cre-math { font-family: serif; line-height: normal; white-space: nowrap; }
.cre-math[data-display="block"] { display: block; text-align: center; margin: 0.5em 0; }
.cre-mi { font-style: italic; }
.cre-mn, .cre-mo, .cre-mtext { font-style: normal; }
.cre-mo { margin: 0 0.12em; }
.cre-fence { margin: 0; }
.cre-mrow, .cre-mstyle, .cre-merror, .cre-mphantom, .cre-menclose { display: inline; }
.cre-msup > sup, .cre-msubsup > sup { font-size: 0.72em; line-height: 0; vertical-align: super; }
.cre-msub > sub, .cre-msubsup > sub { font-size: 0.72em; line-height: 0; vertical-align: sub; }
.cre-mfrac { display: inline-block; vertical-align: middle; text-align: center; margin: 0 0.18em; }
.cre-mfrac-num { display: block; border-bottom: 1px solid currentColor; padding: 0 0.15em; }
.cre-mfrac-den { display: block; padding: 0 0.15em; }
.cre-msqrt { display: inline-block; vertical-align: middle; }
.cre-radical { padding-right: 0.04em; }
.cre-msqrt-body { border-top: 1px solid currentColor; padding: 0 0.12em; }
.cre-mroot { display: inline-block; vertical-align: middle; position: relative; }
.cre-mroot-index { font-size: 0.6em; line-height: 0; vertical-align: super; }
.cre-mtable { display: inline-table; border-collapse: collapse; vertical-align: middle; }
.cre-mtable td, .cre-mtable th { padding: 0.05em 0.2em; text-align: center; }
.cre-mphantom { visibility: hidden; }
.cre-menclose { border: 1px solid currentColor; padding: 0.08em 0.12em; }
.cre-merror { color: #b00000; }
.cre-mspace { display: inline-block; }
"#;

/// Rewrite every `<math>...</math>` subtree in an HTML document into ordinary
/// HTML elements. If a fragment cannot be parsed as XML it is left untouched.
pub fn rewrite_mathml(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    while let Some(relative) = lower[cursor..].find("<math") {
        let start = cursor + relative;
        let Some(end) = find_math_end(&lower, start) else {
            break;
        };
        out.push_str(&html[cursor..start]);
        let fragment = &html[start..end];
        match rewrite_fragment(fragment) {
            Some(rewritten) => out.push_str(&rewritten),
            None => out.push_str(fragment),
        }
        cursor = end;
    }
    out.push_str(&html[cursor..]);
    out
}

fn find_math_end(lower: &str, start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut cursor = start;
    while cursor < lower.len() {
        let open = lower[cursor..].find("<math");
        let close = lower[cursor..].find("</math");
        match (open, close) {
            (None, None) => return None,
            (None, Some(close)) => {
                let close_start = cursor + close;
                if depth <= 1 {
                    return lower[close_start..]
                        .find('>')
                        .map(|offset| close_start + offset + 1);
                }
                depth -= 1;
                cursor = close_start + 6;
            }
            (Some(open), Some(close)) if open < close => {
                depth += 1;
                cursor += open + 5;
            }
            (_, Some(close)) => {
                let close_start = cursor + close;
                if depth <= 1 {
                    return lower[close_start..]
                        .find('>')
                        .map(|offset| close_start + offset + 1);
                }
                depth -= 1;
                cursor = close_start + 6;
            }
            (Some(open), None) => {
                depth += 1;
                cursor += open + 5;
            }
        }
    }
    None
}

fn rewrite_fragment(fragment: &str) -> Option<String> {
    let normalized = fragment
        .replace("&nbsp;", "&#160;")
        .replace("&thinsp;", "&#8201;")
        .replace("&ensp;", "&#8194;");
    let doc = Document::parse(&normalized).ok()?;
    let node = doc.root_element();
    Some(render_node(node))
}

fn render_node(node: Node<'_, '_>) -> String {
    if node.is_text() {
        return escape_html(node.text().unwrap_or_default());
    }
    if !node.is_element() {
        return render_children(node);
    }
    let name = node.tag_name().name().to_ascii_lowercase();
    match name.as_str() {
        "math" => {
            let display = node.attribute("display").unwrap_or("inline");
            format!(
                "<span class=\"cre-math\" data-display=\"{}\">{}</span>",
                escape_attr(display),
                render_children(node)
            )
        }
        "semantics" => node
            .children()
            .find(|child| child.is_element())
            .map(render_node)
            .unwrap_or_default(),
        "annotation" | "annotation-xml" | "none" | "mprescripts" => String::new(),
        "mrow" => format!("<span class=\"cre-mrow\">{}</span>", render_children(node)),
        "mi" => format!(
            "<span class=\"cre-mi\">{}</span>",
            escape_html(node.text().unwrap_or_default())
        ),
        "mn" => format!(
            "<span class=\"cre-mn\">{}</span>",
            escape_html(node.text().unwrap_or_default())
        ),
        "mo" => format!(
            "<span class=\"cre-mo\">{}</span>",
            escape_html(node.text().unwrap_or_default())
        ),
        "mtext" | "ms" => format!(
            "<span class=\"cre-mtext\">{}</span>",
            escape_html(node.text().unwrap_or_default())
        ),
        "mspace" => {
            let width = node.attribute("width").unwrap_or("0.5em");
            format!(
                "<span class=\"cre-mspace\" style=\"width:{}\"></span>",
                escape_attr(width)
            )
        }
        "msup" => scripted(node, "sup"),
        "msub" => scripted(node, "sub"),
        "msubsup" => msubsup(node),
        "mfrac" => {
            let mut children = node.children().filter(|child| child.is_element());
            let numerator = children.next().map(render_node).unwrap_or_default();
            let denominator = children.next().map(render_node).unwrap_or_default();
            format!(
                "<span class=\"cre-mfrac\"><span class=\"cre-mfrac-num\">{numerator}</span><span class=\"cre-mfrac-den\">{denominator}</span></span>"
            )
        }
        "msqrt" => format!(
            "<span class=\"cre-msqrt\"><span class=\"cre-radical\">√</span><span class=\"cre-msqrt-body\">{}</span></span>",
            render_children(node)
        ),
        "mroot" => {
            let mut children = node.children().filter(|child| child.is_element());
            let base = children.next().map(render_node).unwrap_or_default();
            let index = children.next().map(render_node).unwrap_or_default();
            format!(
                "<span class=\"cre-mroot\"><span class=\"cre-radical\">√</span><span class=\"cre-msqrt-body\">{base}</span><sup class=\"cre-mroot-index\">{index}</sup></span>"
            )
        }
        "mfenced" => mfenced(node),
        "mtable" => format!(
            "<table class=\"cre-mtable\"><tbody>{}</tbody></table>",
            render_children(node)
        ),
        "mtr" => format!("<tr>{}</tr>", render_children(node)),
        "mtd" | "mth" => format!("<td>{}</td>", render_children(node)),
        "mstyle" => {
            let mut style = String::new();
            for (attribute, property) in [
                ("mathcolor", "color"),
                ("mathbackground", "background-color"),
                ("mathsize", "font-size"),
            ] {
                if let Some(value) = node.attribute(attribute) {
                    if !style.is_empty() {
                        style.push(';');
                    }
                    style.push_str(property);
                    style.push(':');
                    style.push_str(value);
                }
            }
            if style.is_empty() {
                format!(
                    "<span class=\"cre-mstyle\">{}</span>",
                    render_children(node)
                )
            } else {
                format!(
                    "<span class=\"cre-mstyle\" style=\"{}\">{}</span>",
                    escape_attr(&style),
                    render_children(node)
                )
            }
        }
        "merror" => format!(
            "<span class=\"cre-merror\">{}</span>",
            render_children(node)
        ),
        "mphantom" => format!(
            "<span class=\"cre-mphantom\">{}</span>",
            render_children(node)
        ),
        "menclose" => format!(
            "<span class=\"cre-menclose\">{}</span>",
            render_children(node)
        ),
        "munder" => scripted(node, "sub"),
        "mover" => scripted(node, "sup"),
        "munderover" => msubsup(node),
        "mmultiscripts" => render_children(node),
        _ => render_children(node),
    }
}

fn render_children(node: Node<'_, '_>) -> String {
    let mut out = String::new();
    for child in node.children() {
        if child.is_text() && child.text().is_some_and(|text| text.trim().is_empty()) {
            continue;
        }
        out.push_str(&render_node(child));
    }
    out
}

fn scripted(node: Node<'_, '_>, tag: &str) -> String {
    let mut children = node.children().filter(|child| child.is_element());
    let base = children.next().map(render_node).unwrap_or_default();
    let script = children.next().map(render_node).unwrap_or_default();
    let class = if tag == "sup" { "cre-msup" } else { "cre-msub" };
    format!("<span class=\"{class}\">{base}<{tag}>{script}</{tag}></span>")
}

fn msubsup(node: Node<'_, '_>) -> String {
    let mut children = node.children().filter(|child| child.is_element());
    let base = children.next().map(render_node).unwrap_or_default();
    let sub = children.next().map(render_node).unwrap_or_default();
    let sup = children.next().map(render_node).unwrap_or_default();
    format!("<span class=\"cre-msubsup\">{base}<sub>{sub}</sub><sup>{sup}</sup></span>")
}

fn mfenced(node: Node<'_, '_>) -> String {
    let open = node.attribute("open").unwrap_or("(");
    let close = node.attribute("close").unwrap_or(")");
    let separators = node.attribute("separators").unwrap_or(",");
    let separator = separators.chars().next().unwrap_or(',');
    let mut out = String::from("<span class=\"cre-mfenced\">");
    out.push_str(&format!(
        "<span class=\"cre-fence\">{}</span>",
        escape_html(open)
    ));
    let mut first = true;
    for child in node.children().filter(|child| child.is_element()) {
        if !first {
            out.push_str(&format!(
                "<span class=\"cre-fence\">{}</span>",
                escape_html(&separator.to_string())
            ));
        }
        first = false;
        out.push_str(&render_node(child));
    }
    out.push_str(&format!(
        "<span class=\"cre-fence\">{}</span>",
        escape_html(close)
    ));
    out.push_str("</span>");
    out
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attr(value: &str) -> String {
    escape_html(value)
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_superscript_and_fenced_expression() {
        let html = r#"<p>x<math xmlns="http://www.w3.org/1998/Math/MathML"><mrow><mi>x</mi><mo>=</mo><msup><mfenced><mrow><mi>a</mi><mo>+</mo><mi>b</mi></mrow></mfenced><mn>2</mn></msup></mrow></math></p>"#;
        let out = rewrite_mathml(html);
        assert!(out.contains("<sup>"), "{out}");
        assert!(out.contains("cre-mn\">2"), "{out}");
        assert!(out.contains("cre-mfenced"), "{out}");
        assert!(out.contains("<span class=\"cre-mi\">x</span>"), "{out}");
        assert!(!out.contains("<math"), "{out}");
    }

    #[test]
    fn rewrites_fraction_and_root() {
        let html = r#"<math xmlns="http://www.w3.org/1998/Math/MathML"><mfrac><mn>1</mn><mn>2</mn></mfrac><msqrt><mi>x</mi></msqrt></math>"#;
        let out = rewrite_mathml(html);
        assert!(out.contains("cre-mfrac-num"), "{out}");
        assert!(out.contains("cre-mfrac-den"), "{out}");
        assert!(out.contains("cre-msqrt"), "{out}");
    }
}

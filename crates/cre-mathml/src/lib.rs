//! Small MathML presentation-subset translator for the Blitz-based engine.
//!
//! Blitz has no MathML layout. The approach here follows crengine's
//! `MathMLHelper`: transform the MathML tree into ordinary HTML boxes that the
//! existing HTML layout understands (inline blocks, `sup`/`sub`, tables and
//! borders), instead of implementing a separate math layout engine.
//!
//! This is deliberately a presentation subset: tokens, `mrow`, `mfenced`,
//! `msup`/`msub`/`msubsup`, `munder`/`mover`/`munderover`, `mfrac`,
//! `msqrt`/`mroot`, `mtable`, `mspace`, `mstyle`, `merror`, `mphantom` and
//! `menclose`. Scripts are positioned with CSS baseline shifts; fences and
//! radicals are stretched with CSS transforms (which the tiny-skia painter
//! applies as glyph/box transforms); large operators use display-style
//! limits. OpenType MATH metrics and the finer mtable/multiscript rules remain
//! future work.

use roxmltree::{Document, Node};

/// CSS needed by the generated HTML. The engine appends this once per chunk.
pub const CSS: &str = r#"
.cre-math { font-family: serif; line-height: 1.15; white-space: nowrap; }
.cre-math[data-display="block"] { display: block; text-align: center; margin: 0.6em 0; }
/* Blitz computes the advance of italic runs from the upright face, which
   makes the following glyph overlap. Skewing an inline-block token paints the
   same oblique shape without touching the measured advance. */
.cre-mi { display: inline-block; transform: skewX(-10deg); transform-origin: center; }
.cre-mi-upright { display: inline-block; }
.cre-mn, .cre-mo, .cre-mtext { font-style: normal; }
.cre-mrow, .cre-mstyle, .cre-merror, .cre-mphantom, .cre-menclose { display: inline; }
.cre-mo { display: inline-block; margin: 0 0.05em; }
.cre-mo-bin { margin: 0 0.22em; }
.cre-mo-unary { margin: 0 0.05em; }
.cre-mo-rel { margin: 0 0.28em; }
.cre-mo-invisible { margin: 0; }
.cre-mo-large { font-size: 1.35em; line-height: 1; vertical-align: middle; margin: 0 0.08em; }
.cre-fence { margin: 0; display: inline-block; line-height: 1; vertical-align: middle; transform-origin: center; }
.cre-script { white-space: nowrap; }
.cre-sup, .cre-sub { display: inline-block; font-size: 0.7em; line-height: 1; }
.cre-sup { vertical-align: 0.55em; }
.cre-sub { vertical-align: -0.25em; }
.cre-script-stack { display: inline-block; vertical-align: middle; font-size: 0.7em; line-height: 0.95; }
.cre-script-stack > span { display: block; }
.cre-limits { display: inline-block; text-align: center; vertical-align: middle; }
.cre-limits-base { display: block; line-height: 1.1; }
.cre-limits-over, .cre-limits-under { display: block; font-size: 0.7em; line-height: 1; }
.cre-limits-under { padding-top: 0.08em; }
.cre-limits-over { padding-bottom: 0.04em; }
.cre-mfrac { display: inline-block; vertical-align: middle; text-align: center; margin: 0 0.15em; }
.cre-mfrac-num { display: block; padding: 0 0.15em; border-bottom: 1px solid currentColor; line-height: 1.1; }
.cre-mfrac-den { display: block; padding: 0 0.15em; line-height: 1.1; }
.cre-msqrt { display: inline-block; vertical-align: middle; white-space: nowrap; }
.cre-radical { display: inline-block; line-height: 1; vertical-align: middle; transform-origin: center; }
.cre-msqrt-body { display: inline-block; border-top: 1px solid currentColor; padding: 0.02em 0.12em 0 0.08em; }
.cre-mroot { display: inline-block; vertical-align: middle; }
.cre-mroot-index { font-size: 0.6em; line-height: 1; vertical-align: 0.9em; }
.cre-mtable-wrap { display: inline-block; vertical-align: middle; }
/* Book stylesheets commonly style every table (`width: 90%`, cell borders,
   sans-serif 80% cells). Math tables are layout scaffolds: reset those. */
.cre-mtable { border-collapse: collapse !important; border-spacing: 0 !important; margin: 0 !important; border: none !important; width: auto !important; }
.cre-mtable tbody { background: transparent !important; }
.cre-mtable td { border: none !important; background: transparent !important; padding: 0.12em 0.5em !important; text-align: center; vertical-align: middle; line-height: 1.15 !important; font-family: inherit !important; font-size: 1em !important; }
.cre-mphantom { visibility: hidden; }
.cre-menclose { display: inline-block; line-height: 1.2; }
.cre-strike-h, .cre-strike-v, .cre-strike-up, .cre-strike-down { position: absolute; border-top: 1px solid currentColor; }
.cre-strike-h { left: 0; right: 0; top: 50%; }
.cre-strike-v { left: 50%; top: 0; bottom: 0; border-top: none; border-left: 1px solid currentColor; }
.cre-strike-up { left: -20%; right: -20%; top: 50%; transform: rotate(-45deg); transform-origin: center; }
.cre-strike-down { left: -20%; right: -20%; top: 50%; transform: rotate(45deg); transform-origin: center; }
.cre-mmultiscripts { white-space: nowrap; }
.cre-ms-pre { margin-right: 0.05em; }
.cre-merror { color: #b00000; }
.cre-mspace { display: inline-block; }
"#;

/// A math-capable font found among the reader's loaded fonts.
///
/// Chromium's MathML stylesheet sets `font-family: math`, which resolves to the
/// platform math font (Cambria Math on Windows). There is no `math` generic
/// family in Blitz, so `cre-lib` finds a loaded font with an OpenType MATH table
/// (KOReader ships FreeSerif, which has one) and this type carries its family
/// name plus the metrics used by the generated CSS.
#[derive(Clone, Debug)]
pub struct MathFont {
    /// Family name to put in front of the math font stack.
    pub family: String,
    /// Typographic constants read from the same face.
    pub metrics: MathMetrics,
}

impl MathFont {
    /// Loads a font file (or TTC face) and returns it if it has a MATH table.
    pub fn from_font_path(path: &std::path::Path) -> Option<Self> {
        let data = std::fs::read(path).ok()?;
        Self::from_font_data(&data)
    }

    /// Finds the first face of one font blob with an OpenType MATH table.
    pub fn from_font_data(data: &[u8]) -> Option<Self> {
        let face_count = ttf_parser::fonts_in_collection(data).unwrap_or(1);
        for index in 0..face_count {
            let Ok(face) = ttf_parser::Face::parse(data, index) else {
                continue;
            };
            if face.tables().math.is_none() {
                continue;
            }
            let Some(family) = font_family_name(&face) else {
                continue;
            };
            if let Some(metrics) = metrics_for_face(&face) {
                return Some(Self { family, metrics });
            }
        }
        None
    }

    /// Finds the first font with an OpenType MATH table.
    pub fn from_font_datas<'a>(datas: impl IntoIterator<Item = &'a [u8]>) -> Option<Self> {
        datas.into_iter().find_map(Self::from_font_data)
    }
}

fn font_family_name(face: &ttf_parser::Face<'_>) -> Option<String> {
    let names = face.names();
    let unicode_name = |name_id: u16| {
        names
            .into_iter()
            .find(|name| name.name_id == name_id && name.is_unicode())
            .and_then(|name| name.to_string())
    };
    unicode_name(ttf_parser::name_id::TYPOGRAPHIC_FAMILY)
        .or_else(|| unicode_name(ttf_parser::name_id::FAMILY))
}

fn metrics_for_face(face: &ttf_parser::Face<'_>) -> Option<MathMetrics> {
    let units_per_em = f32::from(face.units_per_em());
    if units_per_em <= 0.0 {
        return None;
    }
    let constants = face.tables().math?.constants?;
    let em = |value: i16| value as f32 / units_per_em;
    let script_scale = f32::from(constants.script_percent_scale_down()) / 100.0;
    let mut metrics = MathMetrics::default();
    let sup = em(constants.superscript_shift_up().value);
    if sup > 0.0 {
        metrics.sup_shift_up = sup;
    }
    let sub = em(constants.subscript_shift_down().value);
    if sub > 0.0 {
        metrics.sub_shift_down = sub;
    }
    if (0.5..0.9).contains(&script_scale) {
        metrics.script_scale = script_scale;
    }
    let rule = em(constants.fraction_rule_thickness().value);
    if rule > 0.0 {
        metrics.fraction_rule = rule;
    }
    let radical_rule = em(constants.radical_rule_thickness().value);
    if radical_rule > 0.0 {
        metrics.radical_rule = radical_rule;
    }
    let min_height = f32::from(constants.delimited_sub_formula_min_height()) / units_per_em;
    if min_height >= 0.5 {
        metrics.delimited_min_height = min_height;
    }
    let axis = em(constants.axis_height().value);
    if axis > 0.0 {
        metrics.axis_height = axis;
    }
    Some(metrics)
}

/// Typographic constants, in `em`, read from the font's OpenType MATH table.
///
/// Falls back to the values crengine and Chrome's default math layout use when
/// the reader's font has no MATH table (most text fonts do not; DejaVu does).
#[derive(Clone, Copy, Debug)]
pub struct MathMetrics {
    /// Superscript baseline shift up.
    pub sup_shift_up: f32,
    /// Subscript baseline shift down.
    pub sub_shift_down: f32,
    /// Script font size as a fraction of the base size.
    pub script_scale: f32,
    /// Fraction rule thickness.
    pub fraction_rule: f32,
    /// Radical/overbar rule thickness.
    pub radical_rule: f32,
    /// Minimum height of a delimited sub-formula (the smallest fence).
    pub delimited_min_height: f32,
    /// Axial height of the math axis.
    pub axis_height: f32,
}

impl Default for MathMetrics {
    fn default() -> Self {
        Self {
            sup_shift_up: 0.55,
            sub_shift_down: 0.25,
            script_scale: 0.7,
            fraction_rule: 0.05,
            radical_rule: 0.05,
            delimited_min_height: 1.15,
            axis_height: 0.25,
        }
    }
}

impl MathMetrics {
    /// Reads the MATH constants from the first font that has them.
    pub fn from_font_datas<'a>(datas: impl IntoIterator<Item = &'a [u8]>) -> Option<Self> {
        datas.into_iter().find_map(Self::from_font_data)
    }

    /// Reads the MATH constants from one font file.
    pub fn from_font_data(data: &[u8]) -> Option<Self> {
        let face = ttf_parser::Face::parse(data, 0).ok()?;
        metrics_for_face(&face)
    }
}

/// The math stylesheet. `font` supplies the math family and, through it, the
/// script shifts and rule thickness; pass `None` to keep the built-in defaults.
pub fn css(font: Option<&MathFont>) -> String {
    let metrics = font.map(|font| font.metrics).unwrap_or_default();
    let mut out = String::from(CSS);
    if let Some(font) = font {
        out.push_str(&format!(
            "\n.cre-math {{ font-family: \"{}\", serif !important; }}\n",
            css_string(&font.family)
        ));
    }
    out.push_str(&format!(
        "\n.cre-sup {{ vertical-align: {:.3}em; }}\n\
         .cre-sub {{ vertical-align: -{:.3}em; }}\n\
         .cre-sup, .cre-sub, .cre-script-stack {{ font-size: {:.3}em; }}\n\
         .cre-mfrac-num {{ border-bottom-width: {:.3}em; }}\n\
         .cre-msqrt-body {{ border-top-width: {:.3}em; }}\n",
        metrics.sup_shift_up,
        metrics.sub_shift_down,
        metrics.script_scale,
        metrics.fraction_rule,
        metrics.radical_rule,
    ));
    out
}

fn css_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Rewrite every `<math>...</math>` subtree in an HTML document into ordinary
/// HTML elements. If a fragment cannot be parsed as XML it is left untouched.
pub fn rewrite_mathml(html: &str) -> String {
    rewrite_mathml_with(html, None)
}

/// Like [`rewrite_mathml`], but uses the math font's metrics for script
/// placement and fence sizing.
pub fn rewrite_mathml_with(html: &str, font: Option<&MathFont>) -> String {
    let metrics = font.map(|font| font.metrics).unwrap_or_default();
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
        match rewrite_fragment(fragment, metrics) {
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

fn rewrite_fragment(fragment: &str, metrics: MathMetrics) -> Option<String> {
    let normalized = fragment
        .replace("&nbsp;", "&#160;")
        .replace("&thinsp;", "&#8201;")
        .replace("&ensp;", "&#8194;");
    let doc = Document::parse(&normalized).ok()?;
    let node = doc.root_element();
    Some(render_node(
        node,
        Ctx {
            display: false,
            metrics,
        },
    ))
}

/// Display mode changes large-operator limits and the size of fences; metrics
/// refine script and rule sizes.
#[derive(Clone, Copy)]
struct Ctx {
    display: bool,
    metrics: MathMetrics,
}

fn render_node(node: Node<'_, '_>, ctx: Ctx) -> String {
    if node.is_text() {
        return escape_html(node.text().unwrap_or_default());
    }
    if !node.is_element() {
        return render_children(node, ctx);
    }
    let name = node.tag_name().name().to_ascii_lowercase();
    match name.as_str() {
        "math" => {
            let display = node.attribute("display").unwrap_or("inline") == "block";
            let ctx = Ctx {
                display,
                metrics: ctx.metrics,
            };
            format!(
                "<span class=\"cre-math\" data-display=\"{}\">{}</span>",
                if display { "block" } else { "inline" },
                render_row(node, ctx)
            )
        }
        "semantics" => node
            .children()
            .find(|child| child.is_element())
            .map(|child| render_node(child, ctx))
            .unwrap_or_default(),
        "annotation" | "annotation-xml" | "none" | "mprescripts" => String::new(),
        "mrow" => render_row(node, ctx),
        "mi" => render_mi(node, ctx),
        "mn" => format!(
            "<span class=\"cre-mn\">{}</span>",
            escape_html(&token_text(node))
        ),
        "mo" => render_mo(node, ctx, 1.0, false),
        "mtext" | "ms" => format!(
            "<span class=\"cre-mtext\" style=\"white-space:pre\">{}</span>",
            escape_html(&token_text(node))
        ),
        "mspace" => {
            let width = node.attribute("width").unwrap_or("0.5em");
            format!(
                "<span class=\"cre-mspace\" style=\"width:{}\"></span>",
                escape_attr(width)
            )
        }
        "msup" => render_script(node, ctx, ScriptKind::Sup),
        "msub" => render_script(node, ctx, ScriptKind::Sub),
        "msubsup" => render_scripts(node, ctx),
        "munder" => render_limits(node, ctx, false, true),
        "mover" => render_limits(node, ctx, true, false),
        "munderover" => render_limits(node, ctx, true, true),
        "mfrac" => render_fraction(node, ctx),
        "msqrt" => render_sqrt(node, ctx),
        "mroot" => render_root(node, ctx),
        "mfenced" => render_fenced(node, ctx),
        "mtable" => render_table(node, ctx),
        "mtr" | "mtd" | "mth" => render_children(node, ctx),
        "mstyle" => render_style(node, ctx),
        "merror" => format!(
            "<span class=\"cre-merror\">{}</span>",
            render_children(node, ctx)
        ),
        "mphantom" => format!(
            "<span class=\"cre-mphantom\">{}</span>",
            render_children(node, ctx)
        ),
        "menclose" => render_menclose(node, ctx),
        "mmultiscripts" => render_multiscripts(node, ctx),
        _ => render_children(node, ctx),
    }
}

fn render_children(node: Node<'_, '_>, ctx: Ctx) -> String {
    let mut out = String::new();
    for child in element_children(node) {
        out.push_str(&render_node(child, ctx));
    }
    out
}

/// Row-like container. Fences stretch to the content they enclose: either a
/// matching fence in the same row or, for unmatched fences, the adjacent
/// item (the tall table after a piecewise brace, for example).
fn render_row(node: Node<'_, '_>, ctx: Ctx) -> String {
    let children: Vec<Node> = element_children(node).collect();
    let heights: Vec<f32> = children
        .iter()
        .map(|child| estimate_height(*child))
        .collect();
    let mut out = String::from("<span class=\"cre-mrow\">");
    let mut operator_context = true;
    for (index, child) in children.iter().enumerate() {
        if child.is_element() && child.tag_name().name().eq_ignore_ascii_case("mo") {
            let stretch = fence_stretch(&children, &heights, index);
            let text = token_text(*child);
            let unary = operator_class(&text) == "cre-mo-bin" && operator_context;
            out.push_str(&render_mo(*child, ctx, stretch, unary));
            operator_context = matches!(
                operator_class(&text),
                "cre-mo-bin" | "cre-mo-rel" | "cre-mo-invisible"
            ) || is_open_fence(&text);
        } else {
            out.push_str(&render_node(*child, ctx));
            operator_context = false;
        }
    }
    out.push_str("</span>");
    out
}

fn mo_fence_text(node: Node<'_, '_>) -> Option<String> {
    if !node.is_element() || !node.tag_name().name().eq_ignore_ascii_case("mo") {
        return None;
    }
    let text = token_text(node);
    if is_fence(&text) { Some(text) } else { None }
}

fn fence_stretch(children: &[Node<'_, '_>], heights: &[f32], index: usize) -> f32 {
    let Some(text) = mo_fence_text(children[index]) else {
        return 1.0;
    };
    let max_range =
        |from: usize, to: usize| heights[from..to].iter().copied().fold(1.0_f32, f32::max);
    if text == "|" || text == "‖" {
        let previous = if index > 0 { heights[index - 1] } else { 1.0 };
        let next = heights.get(index + 1).copied().unwrap_or(1.0);
        return previous.max(next);
    }
    if is_open_fence(&text) {
        let mut depth = 0i32;
        for (next, child) in children.iter().enumerate().skip(index + 1) {
            let Some(other) = mo_fence_text(*child) else {
                continue;
            };
            if is_open_fence(&other) {
                depth += 1;
            } else if is_close_fence(&other) {
                if depth == 0 {
                    return max_range(index + 1, next);
                }
                depth -= 1;
            }
        }
        // Unmatched: a piecewise brace applies to the item that follows it.
        return heights.get(index + 1).copied().unwrap_or(1.0).max(1.0);
    }
    if is_close_fence(&text) {
        let mut depth = 0i32;
        for (previous, child) in children.iter().enumerate().take(index).rev() {
            let Some(other) = mo_fence_text(*child) else {
                continue;
            };
            if is_close_fence(&other) {
                depth += 1;
            } else if is_open_fence(&other) {
                if depth == 0 {
                    return max_range(previous + 1, index);
                }
                depth -= 1;
            }
        }
        return if index > 0 { heights[index - 1] } else { 1.0 };
    }
    1.0
}

fn is_open_fence(text: &str) -> bool {
    matches!(text, "(" | "[" | "{" | "⟨" | "⌈" | "⌊" | "⟦")
}

fn is_close_fence(text: &str) -> bool {
    matches!(text, ")" | "]" | "}" | "⟩" | "⌉" | "⌋" | "⟧")
}

fn render_mi(node: Node<'_, '_>, _ctx: Ctx) -> String {
    let text = token_text(node);
    let variant = node.attribute("mathvariant").unwrap_or("");
    let italic = match variant {
        "normal" | "upright" | "bold" | "sans-serif" => false,
        "italic" | "bold-italic" => true,
        _ => text.chars().count() == 1,
    };
    let class = if italic { "cre-mi" } else { "cre-mi-upright" };
    if variant.contains("bold") {
        let skew = if italic {
            "transform:skewX(-10deg);"
        } else {
            ""
        };
        format!(
            "<span class=\"{class}\" style=\"font-weight:bold;{skew}\">{}</span>",
            escape_html(&text)
        )
    } else {
        format!("<span class=\"{class}\">{}</span>", escape_html(&text))
    }
}

fn render_mo(node: Node<'_, '_>, ctx: Ctx, stretch: f32, unary: bool) -> String {
    let text = token_text(node);
    let class = if unary {
        "cre-mo-unary"
    } else {
        operator_class(&text)
    };
    if is_large_operator(&text) {
        let size = if ctx.display { 1.5 } else { 1.2 };
        return format!(
            "<span class=\"cre-mo cre-mo-large\" style=\"font-size:{size}em\">{}</span>",
            escape_html(&text)
        );
    }
    if is_fence(&text) {
        // Fence height follows the enclosed content; the MATH
        // `delimited_sub_formula_min_height` constant is about spacing, not
        // about forcing an unscaled fence taller.
        let scale = stretch.clamp(1.0, 4.5);
        if scale > 1.05 {
            return format!(
                "<span class=\"cre-mo cre-fence\" style=\"transform:scaleY({scale:.3})\">{}</span>",
                escape_html(&text)
            );
        }
        return format!(
            "<span class=\"cre-mo cre-fence\">{}</span>",
            escape_html(&text)
        );
    }
    format!(
        "<span class=\"cre-mo {class}\">{}</span>",
        escape_html(&text)
    )
}

#[derive(Clone, Copy, PartialEq)]
enum ScriptKind {
    Sup,
    Sub,
}

fn render_script(node: Node<'_, '_>, ctx: Ctx, kind: ScriptKind) -> String {
    let mut children = element_children(node);
    let base = children
        .next()
        .map(|child| render_node(child, ctx))
        .unwrap_or_default();
    let script = children
        .next()
        .map(|child| render_node(child, ctx))
        .unwrap_or_default();
    let class = match kind {
        ScriptKind::Sup => "cre-sup",
        ScriptKind::Sub => "cre-sub",
    };
    format!("<span class=\"cre-script\">{base}<span class=\"{class}\">{script}</span></span>")
}

fn render_scripts(node: Node<'_, '_>, ctx: Ctx) -> String {
    let mut children = element_children(node);
    let Some(base_node) = children.next() else {
        return String::new();
    };
    let base = render_node(base_node, ctx);
    let sub = children
        .next()
        .map(|child| render_node(child, ctx))
        .unwrap_or_default();
    let sup = children
        .next()
        .map(|child| render_node(child, ctx))
        .unwrap_or_default();

    // Display-style limits on large operators go above and below the operator;
    // integrals keep their limits at the side, like Chrome's MathML rendering.
    if ctx.display && is_large_operator_node(base_node) && !is_integral(&token_text(base_node)) {
        return format!(
            "<span class=\"cre-limits\"><span class=\"cre-limits-over\">{sup}</span>\
             <span class=\"cre-limits-base\">{base}</span>\
             <span class=\"cre-limits-under\">{sub}</span></span>"
        );
    }

    // Large operators keep their limits at the side; give the stack a little
    // breathing room because the operator glyph's ink is narrower than its em
    // box (the integral especially).
    let stack_style = if is_large_operator_node(base_node) {
        " style=\"margin-left:0.1em\""
    } else {
        ""
    };
    format!(
        "<span class=\"cre-script\">{base}<span class=\"cre-script-stack\"{stack_style}>\
         <span>{sup}</span><span>{sub}</span></span></span>"
    )
}

fn render_limits(node: Node<'_, '_>, ctx: Ctx, over: bool, under: bool) -> String {
    let mut children = element_children(node);
    let base = children
        .next()
        .map(|child| render_node(child, ctx))
        .unwrap_or_default();
    let mut over_html = String::new();
    let mut under_html = String::new();
    if over {
        over_html = children
            .next()
            .map(|child| render_node(child, ctx))
            .unwrap_or_default();
    }
    if under {
        under_html = children
            .next()
            .map(|child| render_node(child, ctx))
            .unwrap_or_default();
    }
    let mut out = String::from("<span class=\"cre-limits\">");
    if !over_html.is_empty() {
        out.push_str(&format!(
            "<span class=\"cre-limits-over\">{over_html}</span>"
        ));
    }
    out.push_str(&format!("<span class=\"cre-limits-base\">{base}</span>"));
    if !under_html.is_empty() {
        out.push_str(&format!(
            "<span class=\"cre-limits-under\">{under_html}</span>"
        ));
    }
    out.push_str("</span>");
    out
}

fn render_fraction(node: Node<'_, '_>, ctx: Ctx) -> String {
    let mut children = element_children(node);
    let numerator = children
        .next()
        .map(|child| render_node(child, ctx))
        .unwrap_or_default();
    let denominator = children
        .next()
        .map(|child| render_node(child, ctx))
        .unwrap_or_default();
    format!(
        "<span class=\"cre-mfrac\"><span class=\"cre-mfrac-num\">{numerator}</span>\
         <span class=\"cre-mfrac-den\">{denominator}</span></span>"
    )
}

fn render_sqrt(node: Node<'_, '_>, ctx: Ctx) -> String {
    let body = render_children(node, ctx);
    let height = estimate_height(node);
    let scale = (height / 1.0).clamp(1.0, 4.0);
    let radical = if scale > 1.15 {
        format!("<span class=\"cre-radical\" style=\"transform:scaleY({scale:.3})\">√</span>")
    } else {
        String::from("<span class=\"cre-radical\">√</span>")
    };
    format!(
        "<span class=\"cre-msqrt\">{radical}<span class=\"cre-msqrt-body\">{body}</span></span>"
    )
}

fn render_root(node: Node<'_, '_>, ctx: Ctx) -> String {
    let mut children = element_children(node);
    let base = children
        .next()
        .map(|child| render_node(child, ctx))
        .unwrap_or_default();
    let index = children
        .next()
        .map(|child| render_node(child, ctx))
        .unwrap_or_default();
    format!(
        "<span class=\"cre-mroot\"><span class=\"cre-radical\">√</span>\
         <span class=\"cre-msqrt-body\">{base}</span>\
         <span class=\"cre-mroot-index\">{index}</span></span>"
    )
}

fn render_fenced(node: Node<'_, '_>, ctx: Ctx) -> String {
    let open = node.attribute("open").unwrap_or("(");
    let close = node.attribute("close").unwrap_or(")");
    let separators: Vec<char> = node
        .attribute("separators")
        .unwrap_or(",")
        .chars()
        .collect();
    let children: Vec<Node> = element_children(node).collect();
    let height = children
        .iter()
        .map(|child| estimate_height(*child))
        .fold(1.0_f32, f32::max);
    let scale = height.clamp(1.0, 4.5);
    let fence_style = if scale > 1.05 {
        format!(" style=\"transform:scaleY({scale:.3})\"")
    } else {
        String::new()
    };
    let mut out = String::from("<span class=\"cre-mrow\">");
    out.push_str(&format!(
        "<span class=\"cre-mo cre-fence\"{fence_style}>{}</span>",
        escape_html(open)
    ));
    for (index, child) in children.iter().enumerate() {
        if index > 0 {
            let separator = separators
                .get(index - 1)
                .or_else(|| separators.last())
                .copied()
                .unwrap_or(',');
            out.push_str(&format!(
                "<span class=\"cre-mo cre-fence\">{}</span>",
                escape_html(&separator.to_string())
            ));
        }
        out.push_str(&render_node(*child, ctx));
    }
    out.push_str(&format!(
        "<span class=\"cre-mo cre-fence\"{fence_style}>{}</span>",
        escape_html(close)
    ));
    out.push_str("</span>");
    out
}

fn render_table(node: Node<'_, '_>, ctx: Ctx) -> String {
    let alignments: Vec<String> = node
        .attribute("columnalign")
        .unwrap_or("")
        .split_whitespace()
        .map(|value| value.to_string())
        .collect();
    let row_spacing = node.attribute("rowspacing");
    let column_spacing = node.attribute("columnspacing");
    let mut out =
        String::from("<span class=\"cre-mtable-wrap\"><table class=\"cre-mtable\"><tbody>");
    for row in node
        .children()
        .filter(|child| child.is_element())
        .filter(|child| child.tag_name().name().eq_ignore_ascii_case("mtr"))
    {
        out.push_str("<tr>");
        for (column, cell) in row
            .children()
            .filter(|child| child.is_element())
            .filter(|child| {
                let name = child.tag_name().name();
                name.eq_ignore_ascii_case("mtd") || name.eq_ignore_ascii_case("mth")
            })
            .enumerate()
        {
            let align = alignments
                .get(column)
                .or_else(|| alignments.last())
                .map(|value| match value.as_str() {
                    "left" => "left",
                    "right" => "right",
                    _ => "center",
                })
                .unwrap_or("center");
            let mut style = format!("text-align:{align}");
            if let Some(spacing) = column_spacing {
                style.push_str(&format!(";padding-left:{spacing};padding-right:{spacing}"));
            }
            if let Some(spacing) = row_spacing {
                style.push_str(&format!(";padding-top:{spacing};padding-bottom:{spacing}"));
            }
            out.push_str(&format!(
                "<td style=\"{style}\">{}</td>",
                render_children(cell, ctx)
            ));
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table></span>");
    out
}

/// `menclose` with the common notations: box/roundedbox/circle, individual
/// sides, and the horizontal/vertical/diagonal strikes. Other notations
/// (`longdiv`, `phasorangle`, ...) keep the box.
fn render_menclose(node: Node<'_, '_>, ctx: Ctx) -> String {
    let notation = node.attribute("notation").unwrap_or("box");
    let mut style = String::from("position:relative;");
    let mut strikes = String::new();
    for token in notation.split_whitespace() {
        match token {
            "box" => style.push_str("border:1px solid currentColor;padding:0.08em 0.12em;"),
            "roundedbox" => style.push_str(
                "border:1px solid currentColor;border-radius:0.3em;padding:0.08em 0.12em;",
            ),
            "circle" => style
                .push_str("border:1px solid currentColor;border-radius:50%;padding:0.18em 0.35em;"),
            "left" => style.push_str("border-left:1px solid currentColor;padding-left:0.12em;"),
            "right" => style.push_str("border-right:1px solid currentColor;padding-right:0.12em;"),
            "top" => style.push_str("border-top:1px solid currentColor;padding-top:0.08em;"),
            "bottom" => {
                style.push_str("border-bottom:1px solid currentColor;padding-bottom:0.08em;")
            }
            "horizontalstrike" => strikes.push_str("<span class=\"cre-strike-h\"></span>"),
            "verticalstrike" => strikes.push_str("<span class=\"cre-strike-v\"></span>"),
            "updiagonalstrike" => strikes.push_str("<span class=\"cre-strike-up\"></span>"),
            "downdiagonalstrike" => strikes.push_str("<span class=\"cre-strike-down\"></span>"),
            _ => {}
        }
    }
    format!(
        "<span class=\"cre-menclose\" style=\"{style}\">{strikes}{}</span>",
        render_children(node, ctx)
    )
}

/// `mmultiscripts`: a base with stacked sub/sup pairs after it and, after
/// `mprescripts`, stacked pairs before it. `<none/>` leaves a slot empty.
fn render_multiscripts(node: Node<'_, '_>, ctx: Ctx) -> String {
    let children: Vec<Node> = element_children(node).collect();
    let Some(base_node) = children.first().copied() else {
        return String::new();
    };
    let base = render_node(base_node, ctx);
    let mut post: Vec<(String, String)> = Vec::new();
    let mut pre: Vec<(String, String)> = Vec::new();
    let mut prescripts = false;
    let mut index = 1;
    while index < children.len() {
        let child = children[index];
        if child.tag_name().name().eq_ignore_ascii_case("mprescripts") {
            prescripts = true;
            index += 1;
            continue;
        }
        let sub = render_node(child, ctx);
        let mut sup = String::new();
        if let Some(next) = children.get(index + 1)
            && !next.tag_name().name().eq_ignore_ascii_case("mprescripts")
        {
            sup = render_node(*next, ctx);
            index += 1;
        }
        if prescripts {
            pre.push((sub, sup));
        } else {
            post.push((sub, sup));
        }
        index += 1;
    }
    let stack = |pair: &(String, String), extra: &str| {
        format!(
            "<span class=\"cre-script-stack{extra}\"><span>{}</span><span>{}</span></span>",
            pair.1, pair.0
        )
    };
    let mut out = String::from("<span class=\"cre-mmultiscripts\">");
    for pair in pre.iter().rev() {
        out.push_str(&stack(pair, " cre-ms-pre"));
    }
    out.push_str(&base);
    for pair in &post {
        out.push_str(&stack(pair, ""));
    }
    out.push_str("</span>");
    out
}

fn render_style(node: Node<'_, '_>, ctx: Ctx) -> String {
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
            render_children(node, ctx)
        )
    } else {
        format!(
            "<span class=\"cre-mstyle\" style=\"{}\">{}</span>",
            escape_attr(&style),
            render_children(node, ctx)
        )
    }
}

/// Rough content height in `em`, used to stretch fences and radicals. It only
/// needs to be close enough: Chrome/crengine use real font metrics, but the
/// heights here differ by a fraction of an em.
fn estimate_height(node: Node<'_, '_>) -> f32 {
    if node.is_text() {
        return 1.0;
    }
    if !node.is_element() {
        return 1.0;
    }
    let name = node.tag_name().name().to_ascii_lowercase();
    let children: Vec<Node> = element_children(node).collect();
    let max_child = |children: &[Node]| {
        children
            .iter()
            .map(|child| estimate_height(*child))
            .fold(1.0_f32, f32::max)
    };
    match name.as_str() {
        "math" | "mrow" | "mstyle" | "merror" | "mphantom" | "menclose" | "semantics" => {
            max_child(&children)
        }
        "mfrac" => {
            let numerator = children.first().map(|c| estimate_height(*c)).unwrap_or(1.0);
            let denominator = children.get(1).map(|c| estimate_height(*c)).unwrap_or(1.0);
            (numerator + denominator + 0.5).max(1.7)
        }
        "msup" | "msub" => {
            let base = children.first().map(|c| estimate_height(*c)).unwrap_or(1.0);
            let script = children.get(1).map(|c| estimate_height(*c)).unwrap_or(1.0);
            base.max(script * 0.7 + 0.55)
        }
        "msubsup" | "munderover" => {
            let base = children.first().map(|c| estimate_height(*c)).unwrap_or(1.0);
            base.max(1.2)
        }
        "munder" | "mover" => {
            let base = children.first().map(|c| estimate_height(*c)).unwrap_or(1.0);
            base + 0.55
        }
        "msqrt" | "mroot" => max_child(&children) + 0.2,
        "mfenced" => max_child(&children),
        "mtable" => {
            let mut total = 0.0;
            for row in &children {
                let cells: Vec<Node> = element_children(*row).collect();
                total += max_child(&cells) + 0.22;
            }
            total.max(1.0)
        }
        "mtr" | "mtd" | "mth" => max_child(&children),
        _ => max_child(&children),
    }
}

fn element_children<'a, 'input>(node: Node<'a, 'input>) -> impl Iterator<Item = Node<'a, 'input>> {
    node.children().filter(|child| child.is_element())
}

fn token_text(node: Node<'_, '_>) -> String {
    let mut out = String::new();
    for child in node.children() {
        if child.is_text() {
            out.push_str(child.text().unwrap_or_default());
        }
    }
    out
}

fn is_large_operator_node(node: Node<'_, '_>) -> bool {
    node.is_element() && node.tag_name().name().eq_ignore_ascii_case("mo") && {
        let text = token_text(node);
        is_large_operator(&text)
    }
}

fn is_large_operator(text: &str) -> bool {
    matches!(
        text,
        "∫" | "∬"
            | "∭"
            | "∮"
            | "∯"
            | "∰"
            | "∑"
            | "∏"
            | "∐"
            | "⋃"
            | "⋂"
            | "⋁"
            | "⋀"
            | "⨁"
            | "⨂"
            | "⨀"
            | "⨄"
            | "⨆"
    )
}

fn is_integral(text: &str) -> bool {
    matches!(text, "∫" | "∬" | "∭" | "∮" | "∯" | "∰")
}

fn is_fence(text: &str) -> bool {
    matches!(
        text,
        "(" | ")"
            | "["
            | "]"
            | "{"
            | "}"
            | "|"
            | "‖"
            | "⟨"
            | "⟩"
            | "⌈"
            | "⌉"
            | "⌊"
            | "⌋"
            | "⟦"
            | "⟧"
    )
}

fn operator_class(text: &str) -> &'static str {
    if text == "\u{2062}" || text == "\u{2063}" || text.trim().is_empty() {
        return "cre-mo-invisible";
    }
    if matches!(
        text,
        "+" | "−"
            | "-"
            | "±"
            | "∓"
            | "×"
            | "⋅"
            | "·"
            | "*"
            | "∗"
            | "/"
            | "÷"
            | "∪"
            | "∩"
            | "∧"
            | "∨"
            | "⊕"
            | "⊗"
            | "⊙"
    ) {
        return "cre-mo-bin";
    }
    if matches!(
        text,
        "=" | "≠"
            | "≈"
            | "≡"
            | "<"
            | ">"
            | "≤"
            | "≥"
            | "→"
            | "←"
            | "↔"
            | "⇒"
            | "⇐"
            | "⇔"
            | "∈"
            | "∉"
            | "⊂"
            | "⊃"
            | "⊆"
            | "⊇"
            | "∼"
            | "≃"
            | "≅"
            | "∝"
    ) {
        return "cre-mo-rel";
    }
    if matches!(text, "," | ";" | ":") {
        return "cre-mo-punct";
    }
    "cre-mo-op"
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
        assert!(out.contains("cre-sup"), "{out}");
        assert!(out.contains("cre-mn\">2"), "{out}");
        assert!(out.contains("cre-fence"), "{out}");
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

    #[test]
    fn scripts_stack_under_and_over() {
        let html = r#"<math><msubsup><mo>∫</mo><mi>a</mi><mi>b</mi></msubsup><munder><mo>lim</mo><mi>x</mi></munder></math>"#;
        let out = rewrite_mathml(html);
        assert!(out.contains("cre-script-stack"), "{out}");
        assert!(out.contains("cre-limits-under"), "{out}");
        assert!(out.contains("cre-mo-large"), "{out}");
    }

    #[test]
    fn fences_stretch_to_tall_content() {
        let html = r#"<math><mfenced open="[" close="]"><mtable><mtr><mtd><mi>x</mi></mtd></mtr><mtr><mtd><mi>y</mi></mtd></mtr></mtable></mfenced></math>"#;
        let out = rewrite_mathml(html);
        assert!(out.contains("scaleY("), "{out}");
        assert!(out.contains("cre-mtable"), "{out}");
    }

    #[test]
    fn multi_character_identifiers_are_upright() {
        let single = rewrite_mathml(r#"<math><mi>x</mi></math>"#);
        let multi = rewrite_mathml(r#"<math><mi>lim</mi></math>"#);
        assert!(single.contains("class=\"cre-mi\""), "{single}");
        assert!(multi.contains("cre-mi-upright"), "{multi}");
    }

    #[test]
    fn unary_minus_uses_tight_spacing() {
        let unary = rewrite_mathml(
            r#"<math><mrow><mo>−</mo><msup><mi>x</mi><mn>2</mn></msup></mrow></math>"#,
        );
        assert!(unary.contains("cre-mo-unary"), "{unary}");
        let binary = rewrite_mathml(r#"<math><mrow><mi>x</mi><mo>+</mo><mn>4</mn></mrow></math>"#);
        assert!(binary.contains("cre-mo-bin"), "{binary}");
    }

    #[test]
    fn display_limits_differ_for_integrals_and_sums() {
        let integral = rewrite_mathml(
            r#"<math display="block"><msubsup><mo>∫</mo><mi>a</mi><mi>b</mi></msubsup></math>"#,
        );
        assert!(integral.contains("cre-script-stack"), "{integral}");
        assert!(!integral.contains("cre-limits"), "{integral}");
        let sum = rewrite_mathml(
            r#"<math display="block"><msubsup><mo>∑</mo><mi>i</mi><mi>n</mi></msubsup></math>"#,
        );
        assert!(sum.contains("cre-limits"), "{sum}");
    }

    #[test]
    fn fences_stretch_only_to_their_content() {
        let out = rewrite_mathml(
            r#"<math><mrow><mi>f</mi><mo>(</mo><mi>x</mi><mo>)</mo><mo>=</mo><mo>(</mo><mfrac><mn>1</mn><mn>2</mn></mfrac><mo>)</mo></mrow></math>"#,
        );
        // The first pair encloses a single `x`, so it stays at normal size.
        let first_open = out.find("cre-fence").unwrap();
        let first_end = out[first_open..].find('>').unwrap() + first_open;
        assert!(!out[first_open..first_end].contains("scaleY"), "{out}");
        // The second pair encloses the fraction and stretches.
        assert!(out.contains("scaleY("), "{out}");
    }

    #[test]
    fn table_css_resets_book_table_styles() {
        assert!(CSS.contains("width: auto !important"), "{CSS}");
        assert!(CSS.contains(".cre-mtable td"), "{CSS}");
        assert!(CSS.contains("border: none !important"), "{CSS}");
    }

    #[test]
    fn menclose_notations_add_sides_and_strikes() {
        let out = rewrite_mathml(
            r#"<math><menclose notation="box updiagonalstrike"><mi>x</mi></menclose></math>"#,
        );
        assert!(out.contains("border:1px solid currentColor"), "{out}");
        assert!(out.contains("cre-strike-up"), "{out}");
        let diag = rewrite_mathml(
            r#"<math><menclose notation="downdiagonalstrike"><mi>x</mi></menclose></math>"#,
        );
        assert!(diag.contains("cre-strike-down"), "{diag}");
    }

    #[test]
    fn multiscripts_render_pre_and_post_pairs() {
        let out = rewrite_mathml(
            r#"<math><mmultiscripts><mi>X</mi><mn>1</mn><mn>2</mn><mprescripts/><mn>3</mn><mn>4</mn></mmultiscripts></math>"#,
        );
        assert!(out.contains("cre-mmultiscripts"), "{out}");
        assert!(out.contains("cre-ms-pre"), "{out}");
        // post pair 1/2 and pre pair 3/4 both appear as stacked columns.
        assert_eq!(out.matches("cre-script-stack").count(), 2, "{out}");
        let pre = out.find("cre-ms-pre").unwrap();
        let base = out.find(">X<").unwrap();
        assert!(pre < base, "pre-scripts must come before the base: {out}");
    }

    #[test]
    fn metrics_override_script_size_and_rules() {
        let font = MathFont {
            family: "Test Math".to_string(),
            metrics: MathMetrics {
                script_scale: 0.8,
                fraction_rule: 0.04,
                sup_shift_up: 0.6,
                ..MathMetrics::default()
            },
        };
        let css = css(Some(&font));
        assert!(css.contains("font-size: 0.800em"), "{css}");
        assert!(css.contains("vertical-align: 0.600em"), "{css}");
        assert!(css.contains("border-bottom-width: 0.040em"), "{css}");
        assert!(css.contains("font-family: \"Test Math\", serif"), "{css}");
    }
}

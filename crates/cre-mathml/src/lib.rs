//! Small MathML presentation-subset translator for the Blitz-based engine.
//!
//! Blitz has no MathML layout. The approach here follows crengine's
//! `MathMLHelper`: transform the MathML tree into ordinary HTML boxes that the
//! existing HTML layout understands (inline blocks, `sup`/`sub`, tables and
//! borders), instead of implementing a separate math layout engine. We learned
//! the approach from crengine, which does the rewrite inside its DOM builder
//! and stretches operators while laying out a line; here the rewrite runs
//! before Blitz parses the chunk and the engine reports the measured content
//! height back.
//!
//! This is deliberately a presentation subset: tokens, `mrow`, `mfenced`,
//! `msup`/`msub`/`msubsup`, `munder`/`mover`/`munderover`, `mfrac`,
//! `msqrt`/`mroot`, `mtable`, `mspace`, `mpadded`, `mstyle`, `merror`,
//! `mphantom`, `menclose`, `mmultiscripts` and `maction`.
//!
//! Scripts are positioned with CSS baseline shifts computed from the font's
//! OpenType MATH constants, fences are measured against the content they cover
//! and stretched with CSS transforms (the tiny-skia painter applies those as
//! glyph transforms), and radical signs are drawn from strokes plus a separate
//! vinculum element, so they do not depend on the font's `√` glyph. Large
//! operators use display-style limits. [`MathMetrics`] reads the MATH table of
//! the selected math font and falls back to the crengine/Chrome defaults when
//! there is none.
//!
//! Presentation attributes (`mathcolor`, `mathbackground`, `mathsize`,
//! `mathvariant`) and the author's `class`/`style` are carried onto the
//! generated HTML, so a book's MathML CSS applies to the rewritten boxes.
//!
//! Remaining limits: absolute `mpadded` sizes (only increments are honoured),
//! a full MathML operator dictionary, multi-glyph stretchy composition and the
//! finest mtable/multiscript placement are not implemented; operator spacing
//! uses CSS margins by operator class.

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
/* A delimited group is one inline-block: the line breaker moves `( ... )` to
   the next line as a unit instead of leaving the opening fence at the end of
   the previous line with its content below. */
.cre-fence-group { display: inline-block; vertical-align: baseline; white-space: nowrap; }
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
.cre-mo-hstretch { display: block; text-align: center; }
.cre-limits-over { padding-bottom: 0.04em; }
.cre-mfrac { display: inline-block; vertical-align: middle; text-align: center; margin: 0 0.15em; }
.cre-mfrac-num { display: block; padding: 0 0.15em 0.06em 0.15em; border-bottom: 1px solid currentColor; line-height: 1.1; }
.cre-mfrac-den { display: block; padding: 0.12em 0.15em 0 0.15em; line-height: 1.1; }
/* A radical is drawn, not scaled: the vinculum is the wrapper's top border and
   the surd is two thin strokes, so the sign keeps its stroke weight at any
   height and always meets the bar. A stretched `√` glyph cannot do either
   (scaling thickens its strokes and its ink does not line up with the bar). */
.cre-msqrt { display: inline-block; position: relative; vertical-align: middle; white-space: nowrap; padding: 0.12em 0 0 0; }
/* The overbar starts where the drawn surd's tip ends, not at the wrapper's
   left edge (which is the radical's own width). */
.cre-vinculum { position: absolute; top: 0; height: 0; border-top: 1px solid currentColor; }
.cre-surd { position: absolute; left: 0; top: 0; width: 0.62em; }
.cre-surd-stroke { position: absolute; background: currentColor; }
.cre-msqrt-body { display: inline-block; }
.cre-mroot { display: inline-block; }
.cre-mroot-index { position: absolute; left: 0.02em; top: 0.08em; width: 0.34em; font-size: 0.55em; line-height: 1; text-align: right; }
.cre-mtable-wrap { display: inline-block; vertical-align: middle; }
/* Book stylesheets commonly style every table (`width: 90%`, cell borders,
   sans-serif 80% cells). Math tables are layout scaffolds: reset those. */
.cre-mtable { border-collapse: collapse !important; border-spacing: 0 !important; margin: 0 !important; border: none !important; width: auto !important; }
.cre-mtable tbody { background: transparent !important; }
.cre-mtable td { border: none !important; background: transparent !important; padding: 0.12em 0.5em !important; text-align: center; vertical-align: middle; line-height: 1.15 !important; font-family: inherit !important; font-size: 1em !important; }
.cre-mphantom { visibility: hidden; }
.cre-mpadded { display: inline-block; }
.cre-menclose { display: inline-block; line-height: 1.2; }
.cre-strike-h, .cre-strike-v, .cre-strike-diag { position: absolute; background: currentColor; }
.cre-strike-h { left: 0; right: 0; height: 1px; top: 50%; }
.cre-strike-v { top: 0; bottom: 0; width: 1px; left: 50%; }
.cre-strike-diag { left: 50%; top: 50%; height: 1px; transform-origin: center; }
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
    let ascent = f32::from(face.ascender()) / units_per_em;
    let descent = -f32::from(face.descender()) / units_per_em;
    if (0.5..2.0).contains(&ascent) && (0.0..1.0).contains(&descent) {
        metrics.ascent = ascent;
        metrics.descent = descent;
    }
    // How tall each fence glyph's ink actually is, in `em`, and where its
    // centre sits: a stretched fence must not be scaled by the content height
    // alone (it would end up short by the glyph's side bearings), and scaling
    // it about the box centre moves its ink off-centre unless the glyph's own
    // centre is taken into account.
    for (slot, ch) in FENCE_CHARS.iter().enumerate() {
        let Some(glyph) = face.glyph_index(*ch) else {
            continue;
        };
        let Some(bounds) = face.glyph_bounding_box(glyph) else {
            continue;
        };
        let ink = (f32::from(bounds.y_max) - f32::from(bounds.y_min)) / units_per_em;
        if ink > 0.3 && ink < 2.0 {
            metrics.fence_ink[slot] = ink;
            metrics.fence_center[slot] =
                (f32::from(bounds.y_max) + f32::from(bounds.y_min)) / 2.0 / units_per_em;
        }
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
    /// Ink height of each fence glyph (see [`FENCE_CHARS`]), in `em`.
    pub fence_ink: [f32; FENCE_CHARS.len()],
    /// Height of each fence glyph's ink centre above the baseline, in `em`.
    pub fence_center: [f32; FENCE_CHARS.len()],
    /// Font ascent above the baseline, in `em`.
    pub ascent: f32,
    /// Font descent below the baseline, in `em`.
    pub descent: f32,
}

/// Fence glyphs whose ink height is read from the MATH font, in the order of
/// [`MathMetrics::fence_ink`].
pub const FENCE_CHARS: [char; 12] = ['(', ')', '[', ']', '{', '}', '|', '‖', '⟨', '⟩', '⌈', '⌊'];

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
            fence_ink: [0.78; FENCE_CHARS.len()],
            fence_center: [0.29; FENCE_CHARS.len()],
            ascent: 0.9,
            descent: 0.25,
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
         .cre-vinculum {{ border-top-width: {:.3}em; }}\n",
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
            script_level: 0,
            variant: None,
        },
    ))
}

/// Display mode changes large-operator limits and the size of fences; metrics
/// refine script and rule sizes.
#[derive(Clone, Copy)]
struct Ctx {
    display: bool,
    metrics: MathMetrics,
    /// How deep inside scripts this subtree sits, so `mstyle scriptlevel` can
    /// size its content relative to it instead of double-shrinking it.
    script_level: u8,
    /// `mathvariant` inherited from an enclosing `mstyle`.
    variant: Option<MathVariant>,
}

/// The `mathvariant` values that change text weight or slope. The other
/// MathML variants (double-struck, script, fraktur, ...) need glyphs the
/// reader's fonts may not have and are left to the font.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MathVariant {
    Normal,
    Bold,
    Italic,
    BoldItalic,
}

impl MathVariant {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "normal" | "upright" | "sans-serif" => Some(Self::Normal),
            "bold" => Some(Self::Bold),
            "italic" => Some(Self::Italic),
            "bold-italic" => Some(Self::BoldItalic),
            _ => None,
        }
    }

    fn css(self) -> &'static str {
        match self {
            Self::Normal => "font-weight:normal;font-style:normal",
            Self::Bold => "font-weight:bold",
            Self::Italic => "font-style:italic",
            Self::BoldItalic => "font-weight:bold;font-style:italic",
        }
    }
}

/// The CSS `mathvariant` implies for a token, taking an enclosing `mstyle`
/// into account.
fn variant_css(node: Node<'_, '_>, ctx: Ctx) -> &'static str {
    node.attribute("mathvariant")
        .and_then(MathVariant::parse)
        .or(ctx.variant)
        .map(MathVariant::css)
        .unwrap_or("")
}

fn push_decl(style: &mut String, property: &str, value: Option<&str>) {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };
    if !style.is_empty() {
        style.push(';');
    }
    style.push_str(property);
    style.push(':');
    style.push_str(value);
}

/// Class and style for the HTML element that replaces a MathML element: the
/// engine's own class, the author's classes (so book CSS keeps matching), and
/// the presentation attributes MathML uses in place of CSS. The author's
/// `style` goes last so it wins.
fn element_attrs(node: Node<'_, '_>, cre_class: &str) -> (String, String) {
    let mut classes = String::from(cre_class);
    if let Some(author) = node.attribute("class") {
        for token in author.split_whitespace() {
            classes.push(' ');
            classes.push_str(token);
        }
    }
    let mut style = String::new();
    push_decl(&mut style, "color", node.attribute("mathcolor"));
    push_decl(
        &mut style,
        "background-color",
        node.attribute("mathbackground"),
    );
    push_decl(&mut style, "font-size", node.attribute("mathsize"));
    if let Some(author) = node.attribute("style") {
        let author = author.trim();
        if !author.is_empty() {
            if !style.is_empty() {
                style.push(';');
            }
            style.push_str(author);
        }
    }
    (classes.trim().to_string(), style)
}

fn format_span(
    node: Node<'_, '_>,
    classes: String,
    style: &str,
    extra_attrs: &str,
    content: &str,
) -> String {
    let mut attrs = String::new();
    if !classes.is_empty() {
        attrs.push_str(&format!("class=\"{}\"", escape_attr(&classes)));
    }
    if let Some(id) = node
        .attribute("id")
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        if !attrs.is_empty() {
            attrs.push(' ');
        }
        attrs.push_str(&format!("id=\"{}\"", escape_attr(id)));
    }
    if !extra_attrs.is_empty() {
        if !attrs.is_empty() {
            attrs.push(' ');
        }
        attrs.push_str(extra_attrs);
    }
    if !style.is_empty() {
        if !attrs.is_empty() {
            attrs.push(' ');
        }
        attrs.push_str(&format!("style=\"{}\"", escape_attr(style)));
    }
    format!("<span {attrs}>{content}</span>")
}

/// A span that carries `cre_class`, the author's classes and the MathML
/// presentation attributes. `extra_style` is appended to those declarations.
fn span_with(
    node: Node<'_, '_>,
    cre_class: &str,
    extra_style: &str,
    extra_attrs: &str,
    content: &str,
) -> String {
    let (classes, mut style) = element_attrs(node, cre_class);
    if !extra_style.is_empty() {
        if !style.is_empty() {
            style.push(';');
        }
        style.push_str(extra_style);
    }
    format_span(node, classes, &style, extra_attrs, content)
}

impl Ctx {
    /// The context for content inside a script, which the stylesheet already
    /// draws one level down.
    fn script(self) -> Self {
        Self {
            script_level: (self.script_level + 1).min(3),
            ..self
        }
    }
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
            let ctx = Ctx { display, ..ctx };
            // Author CSS (a class, inline style or background) turns the
            // formula into a box so its border and padding are laid out; an
            // unstyled formula keeps the plain inline box it always had.
            let styled = node.attribute("class").is_some()
                || node.attribute("style").is_some()
                || node.attribute("mathbackground").is_some();
            let style = if styled && !display {
                "display:inline-block;vertical-align:baseline"
            } else {
                ""
            };
            span_with(
                node,
                "cre-math",
                style,
                &format!(
                    "data-display=\"{}\"",
                    if display { "block" } else { "inline" }
                ),
                &render_row(node, ctx),
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
        "mn" => span_with(
            node,
            "cre-mn",
            variant_css(node, ctx),
            "",
            &escape_html(&token_text(node)),
        ),
        "mo" => render_mo(node, ctx, 1.0, 1.0, false, true),
        "mtext" | "ms" => {
            let variant = variant_css(node, ctx);
            let style = if variant.is_empty() {
                "white-space:pre".to_string()
            } else {
                format!("white-space:pre;{variant}")
            };
            span_with(
                node,
                "cre-mtext",
                &style,
                "",
                &escape_html(&token_text(node)),
            )
        }
        "mspace" => render_mspace(node),
        "mpadded" => render_mpadded(node, ctx),
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
        // MathML renders the selected child only (default: the first). The
        // others are tooltips/toggle alternatives, and drawing them would put
        // their text inline in the formula.
        "maction" => {
            let selection = node
                .attribute("selection")
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(1)
                .max(1);
            node.children()
                .filter(|child| child.is_element())
                .nth(selection - 1)
                .or_else(|| node.children().find(|child| child.is_element()))
                .map(|child| render_node(child, ctx))
                .unwrap_or_default()
        }
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
    // `heights` is what the content's line boxes reserve (and therefore what
    // the fence's own box must be, to keep pagination), `ink_heights` is what
    // the stretched glyph has to cover.
    let heights: Vec<f32> = children
        .iter()
        .map(|child| estimate_height(*child, ctx.display))
        .collect();
    let ink_heights: Vec<f32> = children
        .iter()
        .map(|child| estimate_ink_height(*child, ctx.display))
        .collect();
    // A stretched fence wraps the siblings it covers in a `.cre-fence-body`
    // marker, so the layout pass can measure that content and replace the
    // estimated height with the real one (see `fence_style_for_height`).
    let mut bodies: Vec<(usize, usize)> = Vec::new();
    // The same range also names the atomic group `( open fence, content,
    // close fence )` that the line breaker must not split.
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for (index, child) in children.iter().enumerate() {
        if !child.is_element() || !child.tag_name().name().eq_ignore_ascii_case("mo") {
            continue;
        }
        if fence_stretch(&children, &ink_heights, index) <= 1.25 {
            continue;
        }
        if let Some(range) = fence_covered(&children, index) {
            bodies.push(range);
            let (start, end) = range;
            let open = start.saturating_sub(1);
            // The range ends at the closing fence when there is one; an
            // unmatched fence (a piecewise `{`) has no closer, so the group
            // ends with the item it covers.
            let closes = children
                .get(end)
                .and_then(|child| mo_fence_text(*child))
                .is_some_and(|text| is_close_fence(&text) || text == "|" || text == "‖");
            let close = if closes { end } else { end - 1 };
            if close >= open && !groups.contains(&(open, close)) {
                groups.push((open, close));
            }
        }
    }
    let mut out = String::from("<span class=\"cre-mrow\">");
    let mut opened: Vec<usize> = Vec::new();
    let mut open_groups: Vec<usize> = Vec::new();
    let mut operator_context = true;
    for (index, child) in children.iter().enumerate() {
        while opened.last() == Some(&index) {
            out.push_str("</span>");
            opened.pop();
        }
        // A group opens in front of its fence, outer ranges first.
        let mut group_starts: Vec<(usize, usize)> = groups
            .iter()
            .copied()
            .filter(|(open, _)| *open == index)
            .collect();
        group_starts.sort_by_key(|(_, end)| std::cmp::Reverse(*end));
        for (_, end) in group_starts {
            out.push_str("<span class=\"cre-fence-group\">");
            open_groups.push(end);
        }
        let mut starting: Vec<(usize, usize)> = bodies
            .iter()
            .copied()
            .filter(|(start, _)| *start == index)
            .collect();
        starting.sort_by_key(|(_, end)| std::cmp::Reverse(*end));
        for (_, end) in starting {
            out.push_str("<span class=\"cre-fence-body\">");
            opened.push(end);
        }
        if child.is_element() && child.tag_name().name().eq_ignore_ascii_case("mo") {
            let stretch = fence_stretch(&children, &heights, index);
            let target = fence_stretch(&children, &ink_heights, index);
            let text = token_text(*child);
            let unary = operator_class(&text) == "cre-mo-bin" && operator_context;
            let body_follows = fence_covered(&children, index)
                .map(|(start, end)| start > index || end > index + 1)
                .unwrap_or(true);
            out.push_str(&render_mo(
                *child,
                ctx,
                stretch,
                target,
                unary,
                body_follows,
            ));
            operator_context = matches!(
                operator_class(&text),
                "cre-mo-bin" | "cre-mo-rel" | "cre-mo-invisible"
            ) || is_open_fence(&text);
        } else {
            out.push_str(&render_node(*child, ctx));
            operator_context = false;
        }
        // The group closes after its closing fence, which is this child.
        while open_groups.last() == Some(&index) {
            out.push_str("</span>");
            open_groups.pop();
        }
    }
    while opened.pop().is_some() {
        out.push_str("</span>");
    }
    while open_groups.pop().is_some() {
        out.push_str("</span>");
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
    match fence_covered(children, index) {
        Some((start, end)) => heights[start..end].iter().copied().fold(1.0_f32, f32::max),
        None => 1.0,
    }
}

/// Sibling range (`start..end`) that the fence at `index` encloses: the items
/// between a matching pair, the neighbouring items for a `|`, or the single
/// item that follows (or precedes) an unmatched fence.
fn fence_covered(children: &[Node<'_, '_>], index: usize) -> Option<(usize, usize)> {
    let text = mo_fence_text(children[index])?;
    if text == "|" || text == "‖" {
        // Bars come in pairs (`|x|`, `⟨0|A|x⟩`) and as one-sided delimiters
        // (`\left. A \right|`). A bar that has an earlier bar of the same kind
        // before it closes that group, so both bars cover the content between
        // them: sizing each bar to its own neighbouring item made the closing
        // bar taller than the opening one whenever those items differed height
        // (a large font in `juliet.epub`).
        if let Some(open) = children
            .iter()
            .take(index)
            .rposition(|child| mo_fence_text(*child).as_deref() == Some(text.as_str()))
            && open + 1 < index
        {
            return Some((open + 1, index));
        }
        // Opening bar: the item that follows it.
        if index + 1 < children.len() {
            return Some((index + 1, index + 2));
        }
        // Lone `\right|`: everything before it in the row.
        return (index > 0).then_some((0, index));
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
                    return (index + 1 < next).then_some((index + 1, next));
                }
                depth -= 1;
            }
        }
        // Unmatched: a piecewise brace applies to the item that follows it.
        return (index + 1 < children.len()).then_some((index + 1, index + 2));
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
                    return (previous + 1 < index).then_some((previous + 1, index));
                }
                depth -= 1;
            }
        }
        return (index > 0).then_some((index - 1, index));
    }
    None
}

fn is_open_fence(text: &str) -> bool {
    matches!(text, "(" | "[" | "{" | "⟨" | "⌈" | "⌊" | "⟦")
}

fn is_close_fence(text: &str) -> bool {
    matches!(text, ")" | "]" | "}" | "⟩" | "⌉" | "⌋" | "⟧")
}

fn render_mi(node: Node<'_, '_>, ctx: Ctx) -> String {
    let text = token_text(node);
    let variant = node
        .attribute("mathvariant")
        .and_then(MathVariant::parse)
        .or(ctx.variant);
    let italic = match variant {
        Some(MathVariant::Normal | MathVariant::Bold) => false,
        Some(MathVariant::Italic | MathVariant::BoldItalic) => true,
        None => text.chars().count() == 1,
    };
    let class = if italic { "cre-mi" } else { "cre-mi-upright" };
    // The `cre-mi` class draws the italic as a skew, so only the weight is
    // added here; `font-style` would slant the glyph twice.
    let style = match variant {
        Some(MathVariant::Bold | MathVariant::BoldItalic) => "font-weight:bold",
        Some(MathVariant::Normal) => "font-weight:normal",
        Some(MathVariant::Italic) | None => "",
    };
    span_with(node, class, style, "", &escape_html(&text))
}

fn render_mo(
    node: Node<'_, '_>,
    ctx: Ctx,
    stretch: f32,
    target: f32,
    unary: bool,
    body_follows: bool,
) -> String {
    let text = token_text(node);
    let class = if unary {
        "cre-mo-unary"
    } else {
        operator_class(&text)
    };
    let variant = variant_css(node, ctx);
    if is_large_operator(&text) {
        let size = if ctx.display { 1.5 } else { 1.2 };
        // An explicit mathsize wins over the display-style size.
        let style = if node.attribute("mathsize").is_some() {
            variant.to_string()
        } else if variant.is_empty() {
            format!("font-size:{size}em")
        } else {
            format!("{variant};font-size:{size}em")
        };
        return span_with(node, "cre-mo cre-mo-large", &style, "", &escape_html(&text));
    }
    if is_fence(&text) {
        // The fence's box is the height its content's line boxes reserve; the
        // glyph is stretched to cover everything the content paints, scripts
        // included. The MATH `delimited_sub_formula_min_height` constant is
        // about spacing, not about forcing an unscaled fence taller.
        let content = stretch.clamp(1.0, 12.0);
        let target = target.clamp(content, 12.0);
        if target > 1.25 {
            return stretched_fence_html(&text, content, target, ctx.metrics, body_follows);
        }
        return span_with(node, "cre-mo cre-fence", variant, "", &escape_html(&text));
    }
    span_with(
        node,
        &format!("cre-mo {class}"),
        variant,
        "",
        &escape_html(&text),
    )
}

#[derive(Clone, Copy, PartialEq)]
enum ScriptKind {
    Sup,
    Sub,
}

fn render_script(node: Node<'_, '_>, ctx: Ctx, kind: ScriptKind) -> String {
    let mut children = element_children(node);
    let Some(base_node) = children.next() else {
        return String::new();
    };
    let script_node = children.next();
    let base = render_node(base_node, ctx);
    let script = script_node
        .map(|child| render_node(child, ctx.script()))
        .unwrap_or_default();
    let class = match kind {
        ScriptKind::Sup => "cre-sup",
        ScriptKind::Sub => "cre-sub",
    };
    // On a tall base (a stretched fence, a fraction, a matrix) the script has
    // to climb to the base's top or drop to its foot; a fixed shift would leave
    // it hanging around the middle, which is what Chrome does not do. Short
    // bases keep the font's usual script shift.
    let (sup_shift, sub_shift) =
        script_shifts(estimate_height(base_node, ctx.display), ctx.metrics);
    let shift = match kind {
        ScriptKind::Sup => sup_shift,
        ScriptKind::Sub => sub_shift,
    };
    let shift_style = match kind {
        ScriptKind::Sup => format!(" style=\"vertical-align:{shift:.2}em\""),
        ScriptKind::Sub => format!(" style=\"vertical-align:-{shift:.2}em\""),
    };

    // A script on a large operator sits at that operator's lower (or upper)
    // end instead of around its middle, so `∫_ℝᴺ` hangs off the foot of the
    // sign.
    if is_large_operator_node(base_node) {
        let size = large_operator_size(ctx);
        let shift = if matches!(kind, ScriptKind::Sub) {
            -(0.55 * size - 0.30)
        } else {
            0.5 * size - 0.25
        };
        return format!(
            "<span class=\"cre-script\">{base}<span class=\"cre-script-stack\" \
             style=\"margin-left:0.05em;vertical-align:{shift:.2}em\"><span>{script}</span>\
             </span></span>"
        );
    }
    format!(
        "<span class=\"cre-script\">{base}<span class=\"{class}\"{shift_style}>{script}</span></span>"
    )
}

/// True when a script slot holds nothing that paints: `<mrow/>`, `<none/>` and
/// whitespace-only text are common placeholders in hand-written MathML.
fn is_empty_math(node: Node<'_, '_>) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        for child in current.children() {
            if child.is_text() {
                if !child.text().unwrap_or_default().trim().is_empty() {
                    return false;
                }
            } else if child.is_element() {
                let name = child.tag_name().name().to_ascii_lowercase();
                if name == "mspace" || name == "mglyph" {
                    return false;
                }
                stack.push(child);
            }
        }
    }
    true
}

/// MathML's `movablelimits`: an operator's scripts stack above/below in display
/// style and sit at the side otherwise. `∑`/`∏`/`lim` default to true and the
/// integrals to false; the attribute overrides either way.
fn movable_limits(node: Node<'_, '_>) -> bool {
    if !node.is_element() || !node.tag_name().name().eq_ignore_ascii_case("mo") {
        return false;
    }
    if let Some(attribute) = node.attribute("movablelimits") {
        return attribute.trim().eq_ignore_ascii_case("true");
    }
    is_movable_operator(&token_text(node))
}

fn is_movable_operator(text: &str) -> bool {
    matches!(
        text.trim(),
        "∑" | "∏"
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
            | "lim"
            | "liminf"
            | "limsup"
            | "max"
            | "min"
            | "sup"
            | "inf"
            | "det"
            | "gcd"
    )
}

/// One base with its two scripts at the side (the non-display-style case for a
/// large operator, and the normal case for everything else).
fn side_scripts(
    base_node: Node<'_, '_>,
    base: String,
    sub: String,
    sup: String,
    has_sub: bool,
    has_sup: bool,
    ctx: Ctx,
) -> String {
    // Large operators keep their side scripts, but the pair is dropped towards
    // the operator's foot and spread out, so `∫_a^b` flanks the sign instead
    // of hugging its middle.
    if is_large_operator_node(base_node) {
        let size = large_operator_size(ctx);
        let line_height = 1.2 * size;
        let mut lines = String::new();
        if has_sup {
            lines.push_str(&format!("<span>{sup}</span>"));
        }
        if has_sub {
            lines.push_str(&format!("<span>{sub}</span>"));
        }
        return format!(
            "<span class=\"cre-script\">{base}<span class=\"cre-script-stack\" \
             style=\"margin-left:0.05em;line-height:{line_height:.2}\">{lines}</span></span>"
        );
    }

    // A lone script on a tall base climbs to the top or drops to the foot
    // instead of sitting on the axis.
    let (sup_shift, sub_shift) =
        script_shifts(estimate_height(base_node, ctx.display), ctx.metrics);

    match (has_sup, has_sub) {
        (true, true) => format!(
            "<span class=\"cre-script\">{base}<span class=\"cre-script-stack\">\
             <span>{sup}</span><span>{sub}</span></span></span>"
        ),
        (true, false) => format!(
            "<span class=\"cre-script\">{base}\
             <span class=\"cre-sup\" style=\"vertical-align:{sup_shift:.2}em\">{sup}</span></span>"
        ),
        (false, true) => format!(
            "<span class=\"cre-script\">{base}\
             <span class=\"cre-sub\" style=\"vertical-align:-{sub_shift:.2}em\">{sub}</span></span>"
        ),
        (false, false) => format!("<span class=\"cre-script\">{base}</span>"),
    }
}

fn render_scripts(node: Node<'_, '_>, ctx: Ctx) -> String {
    let mut children = element_children(node);
    let Some(base_node) = children.next() else {
        return String::new();
    };
    let sub_node = children.next();
    let sup_node = children.next();
    let base = render_node(base_node, ctx);
    let sub = sub_node
        .map(|child| render_node(child, ctx.script()))
        .unwrap_or_default();
    let sup = sup_node
        .map(|child| render_node(child, ctx.script()))
        .unwrap_or_default();
    // `msqrt`-style empty slots are common in hand-written MathML
    // (`<msubsup><mi>a</mi><mi>k</mi><mrow/></msubsup>` means `a_k`): they must
    // not take part in the stacked pair, or the remaining script ends up
    // hanging on the math axis instead of above/below the base.
    let has_sub = sub_node.is_some_and(|child| !is_empty_math(child));
    let has_sup = sup_node.is_some_and(|child| !is_empty_math(child));

    // Display-style limits on movable-limit operators go above and below the
    // operator; everything else keeps its scripts at the side.
    if ctx.display && movable_limits(base_node) {
        return format!(
            "<span class=\"cre-limits\">\
             <span class=\"cre-limits-over\">{}</span>\
             <span class=\"cre-limits-base\">{base}</span>\
             <span class=\"cre-limits-under\">{}</span></span>",
            if has_sup { sup.as_str() } else { "" },
            if has_sub { sub.as_str() } else { "" }
        );
    }

    side_scripts(base_node, base, sub, sup, has_sub, has_sup, ctx)
}

/// Vertical scale that makes a fence's ink cover `content` em.
///
/// The fence box is given the content's height (and line-height), so the two
/// boxes always line up whatever the engine does with inline-block
/// `vertical-align`. The glyph sits centred in that box, and its ink only
/// fills about 0.86 em per em of font size, so the transform has to make up
/// the difference: scaling by the content height alone leaves every stretched
/// fence short of the material it is supposed to enclose.
fn fence_slot(text: &str) -> Option<usize> {
    FENCE_CHARS.iter().position(|ch| ch.to_string() == text)
}

/// Style that stretches a fence glyph to `content` em and centres it on the
/// content it encloses.
///
/// `scaleY` pivots on the box centre, but a glyph's ink is not centred in its
/// line box: the baseline sits `ascent` below the box top (once the leading is
/// split evenly) and the ink's own centre is `fence_center` above the baseline.
/// Scaling therefore pushes the ink down by `scale` times that difference; the
/// shift takes it back so the stretched glyph is centred on its own box, which
/// the content fills. The box stays `vertical-align: top` — switching it to
/// `middle` grew the line box and added a page to `juliet.epub`.
/// Markup of a fence glyph stretched to cover `target` em.
///
/// The `data-cre-fence`/`data-cre-fence-box` attributes let the layout pass
/// find the element again and replace the estimate with the height it measured
/// for the `.cre-fence-body` wrapper (see [`fence_style_for_height`]).
fn stretched_fence_html(
    text: &str,
    content: f32,
    target: f32,
    metrics: MathMetrics,
    body_follows: bool,
) -> String {
    let style = stretched_fence_style(text, content, target, 0.0, metrics);
    let body = if body_follows {
        " data-cre-fence-body-after=\"1\""
    } else {
        ""
    };
    format!(
        "<span class=\"cre-mo cre-fence\" data-cre-fence=\"{}\" \
         data-cre-fence-box=\"{content:.3}\"{body} style=\"{style}\">{}</span>",
        escape_html(text),
        escape_html(text)
    )
}

/// Declarations for a fence whose covered height the layout pass measured.
///
/// `box` is the height the element already had (the estimate the first layout
/// used) — keeping it leaves pagination untouched — `target` is the measured
/// height of the content, and `align` (in em) moves the glyph onto the
/// content's top edge when the two boxes do not line up.
pub fn fence_style_for_measured(
    text: &str,
    box_height: f32,
    target: f32,
    align: f32,
    metrics: MathMetrics,
) -> String {
    stretched_fence_style(text, box_height, target, align, metrics)
}

fn stretched_fence_style(
    text: &str,
    content: f32,
    target: f32,
    align: f32,
    metrics: MathMetrics,
) -> String {
    let scale = fence_scale(text, target, metrics);
    let offset = (metrics.ascent - metrics.descent) / 2.0 - fence_center(text, metrics);
    // The ink is centred on the box it is drawn in, so moving it from the
    // box's centre to the middle of what it has to cover takes half the
    // difference; `scaleY` also moves the glyph's own off-centre ink, which is
    // taken back here. `align` moves the box onto the content's top edge when
    // the measured layout says they do not line up.
    let shift = (target - content) / 2.0 - scale * offset + align;
    format!(
        "height:{content:.3}em;line-height:{content:.3}em;\
         vertical-align:top;transform:translateY({shift:.3}em) scaleY({scale:.3})"
    )
}

fn fence_ink(text: &str, metrics: MathMetrics) -> f32 {
    fence_slot(text)
        .map(|slot| metrics.fence_ink[slot])
        .filter(|ink| *ink > 0.3)
        .unwrap_or(0.86)
}

fn fence_center(text: &str, metrics: MathMetrics) -> f32 {
    fence_slot(text)
        .map(|slot| metrics.fence_center[slot])
        .unwrap_or(0.29)
}

fn fence_scale(text: &str, content: f32, metrics: MathMetrics) -> f32 {
    (content / fence_ink(text, metrics)).max(1.0)
}

/// Baseline shifts for a superscript/subscript on a base `height` em tall, in
/// the script's own `em` (scripts are drawn at `metrics.script_scale`).
///
/// A fixed shift is right for a normal glyph but leaves a script hovering
/// around the middle of a tall base: MathML (and Chrome) push it to the base's
/// top/bottom edges instead.
fn script_shifts(height: f32, metrics: MathMetrics) -> (f32, f32) {
    let tall = 0.68 * height;
    let sup = (tall - 0.05).max(metrics.sup_shift_up);
    let sub = (tall - 0.45).max(metrics.sub_shift_down);
    (sup, sub)
}

/// The font-size multiplier `render_mo` gives a large operator.
fn large_operator_size(ctx: Ctx) -> f32 {
    if ctx.display { 1.5 } else { 1.2 }
}

fn render_limits(node: Node<'_, '_>, ctx: Ctx, over: bool, under: bool) -> String {
    let mut children = element_children(node);
    let Some(base_node) = children.next() else {
        return String::new();
    };
    let base = render_node(base_node, ctx);
    // MathML orders the scripts as base, underscript, overscript (unlike
    // `msubsup`, where the subscript comes first too, but `mover`/`munder`
    // carry only one of them).
    let under_node = under.then(|| children.next()).flatten();
    let over_node = over.then(|| children.next()).flatten();
    let under_html = under_node
        .map(|child| render_node(child, ctx.script()))
        .unwrap_or_default();
    let over_html = over_node
        .map(|child| render_node(child, ctx.script()))
        .unwrap_or_default();
    // A brace or arrow over/under an expression is stretched along x to the
    // base's width (MathML "horizontal stretch"), centred on it. The vertical
    // case above scales y; here the glyph is scaled about its centre and the
    // factor comes from the same width estimate the layout uses.
    let under_html = horizontal_stretch(under_html, under_node, base_node);
    let over_html = horizontal_stretch(over_html, over_node, base_node);
    // Outside display style a `movablelimits` operator moves its limits back to
    // the side (MathML core), so `∑` inside a paragraph keeps side scripts even
    // when the author wrote `munderover`.
    if !ctx.display && movable_limits(base_node) {
        let has_under = !under_html.is_empty();
        let has_over = !over_html.is_empty();
        return side_scripts(
            base_node, base, under_html, over_html, has_under, has_over, ctx,
        );
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

/// True for the operators MathML stretches horizontally over/under content.
fn is_horizontal_stretchy(text: &str) -> bool {
    text.chars().any(|ch| {
        matches!(
            ch,
            '\u{23DE}'
                | '\u{23DF}'
                | '\u{23DC}'
                | '\u{23DD}'
                | '\u{23B4}'
                | '\u{23B5}'
                | '\u{203E}'
                | '\u{005F}'
                | '\u{2190}'
                | '\u{2192}'
                | '\u{2194}'
                | '\u{27F5}'
                | '\u{27F6}'
                | '\u{27F7}'
                | '\u{21D0}'
                | '\u{21D2}'
                | '\u{21D4}'
        )
    })
}

/// Wraps a horizontal over/under operator so it spans its base.
fn horizontal_stretch(html: String, script: Option<Node<'_, '_>>, base: Node<'_, '_>) -> String {
    let Some(script) = script else {
        return html;
    };
    if html.is_empty() || !is_horizontal_stretchy(&token_text(script)) {
        return html;
    }
    if script
        .attribute("stretchy")
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("false"))
    {
        return html;
    }
    let base_width = estimate_width(base).max(0.5);
    let operator_width = estimate_width(script).max(0.2);
    let scale = (base_width / operator_width).clamp(1.0, 8.0);
    if scale <= 1.02 {
        return html;
    }
    format!(
        "<span class=\"cre-mo-hstretch\" style=\"display:block;text-align:center;         transform:scaleX({scale:.2});\">{html}</span>"
    )
}

/// `mspace`: a horizontal gap the author sized explicitly.
fn render_mspace(node: Node<'_, '_>) -> String {
    let width = estimate_mspace_width(node);
    let height = node
        .attribute("height")
        .and_then(parse_length_em)
        .unwrap_or(0.0);
    let depth = node
        .attribute("depth")
        .and_then(parse_length_em)
        .unwrap_or(0.0);
    let mut style = format!("display:inline-block;width:{width:.3}em;");
    if height > 0.0 {
        style.push_str(&format!("height:{:.3}em;", height + depth));
        style.push_str("vertical-align:");
        style.push_str(&format!("-{depth:.3}em;"));
    }
    format!("<span class=\"cre-mspace\" style=\"{style}\"></span>")
}

fn render_mpadded(node: Node<'_, '_>, ctx: Ctx) -> String {
    let (classes, mut style) = element_attrs(node, "cre-mpadded");
    if !style.is_empty() {
        style.push(';');
    }
    style.push_str("display:inline-block");
    // Only the incremental form (`width="+0.2em"`) is honoured: an absolute
    // size needs the content's measured size, which the converter does not
    // know. Positive increments become padding, negative ones margins.
    for (attribute, grow, shrink) in [
        ("height", "padding-top", "margin-top"),
        ("depth", "padding-bottom", "margin-bottom"),
        ("width", "padding-right", "margin-right"),
    ] {
        let Some(value) = node.attribute(attribute).map(str::trim) else {
            continue;
        };
        let (direction, length) = if let Some(rest) = value.strip_prefix('+') {
            (1, rest.trim())
        } else if let Some(rest) = value.strip_prefix('-') {
            (-1, rest.trim())
        } else {
            continue;
        };
        if length.is_empty() {
            continue;
        }
        style.push(';');
        if direction > 0 {
            style.push_str(&format!("{grow}:{length}"));
        } else {
            style.push_str(&format!("{shrink}:-{length}"));
        }
    }
    format_span(node, classes, &style, "", &render_children(node, ctx))
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
    let height = estimate_height(node, ctx.display);
    let (width, surd) = surd_markup(height);
    format!(
        "<span class=\"cre-msqrt\" style=\"padding-left:{width:.3}em\">{surd}\
         <span class=\"cre-vinculum\" style=\"left:{width:.3}em;right:0\"></span>\
         <span class=\"cre-msqrt-body\">{body}</span></span>"
    )
}

/// Width of the drawn radical sign in `em`; also the wrapper's left padding.
const SURD_WIDTH: f32 = 0.62;
/// How far the surd's tail reaches below the radicand, in `em`.
const SURD_DEPTH: f32 = 0.22;
/// Stroke thickness of the surd, in `em`.
const SURD_RULE: f32 = 0.062;
/// Height of the short up-stroke at the bottom left of the sign, in `em`.
const SURD_TICK: f32 = 0.52;

/// Draws the radical sign for content that is `height` em tall.
///
/// The sign sits in an absolutely positioned box anchored to the wrapper's
/// content top, so its top-right tip always meets the vinculum (the wrapper's
/// top border). Only its length depends on the estimated content height; if
/// the estimate is slightly off the tail simply ends a little above or below
/// the radicand, which is how a radical sign's tail behaves anyway.
fn surd_markup(height: f32) -> (f32, String) {
    let h = height.max(1.0) + SURD_DEPTH;
    // The sign grows a little wider as it grows taller, like a font's radical
    // size variants, instead of turning into a nearly vertical stroke.
    let w = (SURD_WIDTH + 0.07 * (h - 1.2).max(0.0)).min(0.92);
    // The tick keeps its size; the long stroke grows with the radicand.
    let tick = SURD_TICK.min(h * 0.6);
    let vertex = (0.62 * w, h);
    let tick_top = (0.04 * w, h - tick);
    let tip = (w, 0.0);
    (
        w,
        format!(
            "<span class=\"cre-surd\" style=\"height:{h:.3}em;width:{w:.3}em\">{}{}</span>",
            surd_stroke(tick_top, vertex),
            surd_stroke(vertex, tip)
        ),
    )
}

fn surd_stroke(from: (f32, f32), to: (f32, f32)) -> String {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let length = (dx * dx + dy * dy).sqrt();
    let angle = dy.atan2(dx).to_degrees();
    let center_x = (from.0 + to.0) / 2.0;
    let center_y = (from.1 + to.1) / 2.0;
    format!(
        "<span class=\"cre-surd-stroke\" style=\"left:{:.3}em;top:{:.3}em;\
         width:{length:.3}em;height:{SURD_RULE:.3}em;transform:rotate({angle:.2}deg)\"></span>",
        center_x - length / 2.0,
        center_y - SURD_RULE / 2.0
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
    let height = estimate_height(node, ctx.display);
    let (width, surd) = surd_markup(height);
    format!(
        "<span class=\"cre-msqrt cre-mroot\" style=\"padding-left:{width:.3}em\">{surd}\
         <span class=\"cre-vinculum\" style=\"left:{width:.3}em;right:0\"></span>\
         <span class=\"cre-mroot-index\">{index}</span>\
         <span class=\"cre-msqrt-body\">{base}</span></span>"
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
        .map(|child| estimate_height(*child, ctx.display))
        .fold(1.0_f32, f32::max);
    let ink_height = children
        .iter()
        .map(|child| estimate_ink_height(*child, ctx.display))
        .fold(1.0_f32, f32::max);
    let content = height.clamp(1.0, 12.0);
    let target = ink_height.clamp(content, 12.0);
    let fence_html = |text: &str, body_follows: bool| -> Option<String> {
        if target <= 1.25 || text.is_empty() {
            return None;
        }
        Some(stretched_fence_html(
            text,
            content,
            target,
            ctx.metrics,
            body_follows,
        ))
    };
    let open_html = fence_html(open, true);
    let close_html = fence_html(close, false);
    let mut out = String::from("<span class=\"cre-mrow\">");
    // `mfenced` is a delimited group like a bracket pair: keep it on one line.
    if open_html.is_some() || close_html.is_some() {
        out.push_str("<span class=\"cre-fence-group\">");
    }
    match &open_html {
        Some(html) => out.push_str(html),
        None => out.push_str(&format!(
            "<span class=\"cre-mo cre-fence\">{}</span>",
            escape_html(open)
        )),
    }
    // Each stretched fence wraps the children it covers, so the layout pass can
    // measure them (`render_row` does the same for `<mrow>`).
    let bodies = usize::from(open_html.is_some()) + usize::from(close_html.is_some());
    for _ in 0..bodies {
        out.push_str("<span class=\"cre-fence-body\">");
    }
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
    for _ in 0..bodies {
        out.push_str("</span>");
    }
    match &close_html {
        Some(html) => out.push_str(html),
        None => out.push_str(&format!(
            "<span class=\"cre-mo cre-fence\">{}</span>",
            escape_html(close)
        )),
    }
    if open_html.is_some() || close_html.is_some() {
        out.push_str("</span>");
    }
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
    let (classes, mut wrapper_style) = element_attrs(node, "cre-mtable-wrap");
    // MathML `frame` draws a border around the whole table; `rowlines` and
    // `columnlines` are not implemented.
    if let Some(frame) = node.attribute("frame").map(str::trim)
        && matches!(frame, "solid" | "dashed")
    {
        if !wrapper_style.is_empty() {
            wrapper_style.push(';');
        }
        wrapper_style.push_str(&format!("border:1px {frame}"));
    }

    let mut out = String::from("<table class=\"cre-mtable\"><tbody>");
    for row in node
        .children()
        .filter(|child| child.is_element())
        .filter(|child| child.tag_name().name().eq_ignore_ascii_case("mtr"))
    {
        let (row_classes, row_style) = element_attrs(row, "");
        let mut row_attrs = String::new();
        if !row_classes.is_empty() {
            row_attrs.push_str(&format!(" class=\"{}\"", escape_attr(&row_classes)));
        }
        if let Some(id) = row
            .attribute("id")
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            row_attrs.push_str(&format!(" id=\"{}\"", escape_attr(id)));
        }
        if !row_style.is_empty() {
            row_attrs.push_str(&format!(" style=\"{}\"", escape_attr(&row_style)));
        }
        out.push_str(&format!("<tr{row_attrs}>"));
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
            let (cell_classes, cell_style) = element_attrs(cell, "");
            if !cell_style.is_empty() {
                style.push(';');
                style.push_str(&cell_style);
            }
            let mut cell_attrs = format!(" style=\"{}\"", escape_attr(&style));
            if !cell_classes.is_empty() {
                cell_attrs.insert_str(0, &format!(" class=\"{}\"", escape_attr(&cell_classes)));
            }
            if let Some(id) = cell
                .attribute("id")
                .map(str::trim)
                .filter(|id| !id.is_empty())
            {
                cell_attrs.push_str(&format!(" id=\"{}\"", escape_attr(id)));
            }
            out.push_str(&format!(
                "<td{cell_attrs}>{}</td>",
                render_children(cell, ctx)
            ));
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table>");
    format_span(node, classes, &wrapper_style, "", &out)
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
            "updiagonalstrike" | "downdiagonalstrike" => {}
            _ => {}
        }
    }
    // The diagonal strikes run corner to corner, so their angle follows the
    // content's estimated aspect ratio instead of a fixed 45 degrees (which
    // overshoots badly for a wide fraction).
    let body = render_children(node, ctx);
    let boxed = notation
        .split_whitespace()
        .any(|token| matches!(token, "box" | "roundedbox" | "circle"));
    let width = estimate_width(node) + if boxed { 0.24 } else { 0.0 };
    let height = estimate_height(node, ctx.display) + if boxed { 0.16 } else { 0.0 };
    let has_up = notation.split_whitespace().any(|t| t == "updiagonalstrike");
    let has_down = notation
        .split_whitespace()
        .any(|t| t == "downdiagonalstrike");
    if has_up || has_down {
        let length = (width * width + height * height).sqrt();
        let angle = height.atan2(width).to_degrees();
        for (wanted, degrees) in [(has_up, -angle), (has_down, angle)] {
            if !wanted {
                continue;
            }
            strikes.push_str(&format!(
                "<span class=\"cre-strike-diag\" style=\"width:{length:.3}em;\
                 margin-left:{:.3}em;margin-top:-0.5px;transform:rotate({degrees:.2}deg)\"></span>",
                -length / 2.0
            ));
        }
    }
    format!("<span class=\"cre-menclose\" style=\"{style}\">{strikes}{body}</span>")
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
    // `displaystyle` decides whether limits stack (see `movable_limits`); a
    // test-suite formula keeps its `∑` inline unless this is honoured.
    let display = match node.attribute("displaystyle").map(str::trim) {
        Some("true") => true,
        Some("false") => false,
        _ => ctx.display,
    };
    // `scriptlevel` is absolute ("1") or relative ("+1"), and sizes its content
    // relative to the level it already sits at.
    let mut level = ctx.script_level;
    let mut level_delta = 0i32;
    if let Some(value) = node.attribute("scriptlevel").map(str::trim) {
        let relative = value.starts_with('+') || value.starts_with('-');
        if let Ok(parsed) = value.parse::<i32>() {
            let target = if relative {
                i32::from(level) + parsed
            } else {
                parsed
            }
            .clamp(0, 3);
            level_delta = target - i32::from(level);
            level = u8::try_from(target).unwrap_or(level);
        }
    }

    let (classes, mut style) = element_attrs(node, "cre-mstyle");
    // An inline box whose only children are inline-blocks does not get its
    // background painted; give a styled `mstyle` a box of its own.
    if node.attribute("mathbackground").is_some() {
        if !style.is_empty() {
            style.push(';');
        }
        style.push_str("display:inline-block");
    }
    if level_delta != 0 && node.attribute("mathsize").is_none() {
        let scale = ctx.metrics.script_scale.powi(level_delta);
        if !style.is_empty() {
            style.push(';');
        }
        style.push_str(&format!("font-size:{scale:.3}em"));
    }

    let variant = node
        .attribute("mathvariant")
        .and_then(MathVariant::parse)
        .or(ctx.variant);
    let ctx = Ctx {
        display,
        script_level: level,
        variant,
        ..ctx
    };
    format_span(node, classes, &style, "", &render_children(node, ctx))
}

/// Rough content height in `em`, used to stretch fences and radicals. It only
/// needs to be close enough: Chrome/crengine use real font metrics, but the
/// heights here differ by a fraction of an em.
fn estimate_height(node: Node<'_, '_>, display: bool) -> f32 {
    estimate_extent(node, display, false)
}

/// Height a stretched fence has to cover.
///
/// Same model as [`estimate_height`] — which is what the generated line boxes
/// reserve — but a fence must also cover what sticks out of a row: a
/// superscript rises `0.55 + 0.7*script` above the baseline plus the font's
/// descent below it, and a subscript hangs 0.25em below the baseline. Sizing
/// the fence with the line-box figure left `(x²)` and the top row of a
/// piecewise table uncovered.
fn estimate_ink_height(node: Node<'_, '_>, display: bool) -> f32 {
    estimate_extent(node, display, true)
}

fn estimate_extent(node: Node<'_, '_>, display: bool, ink: bool) -> f32 {
    if node.is_text() {
        return 1.0;
    }
    if !node.is_element() {
        return 1.0;
    }
    let name = node.tag_name().name().to_ascii_lowercase();
    let children: Vec<Node> = element_children(node).collect();
    // `render_mo` enlarges a large operator, so it is taller than a token.
    if name == "mo" && is_large_operator(&token_text(node)) {
        return if display { 1.5 } else { 1.2 };
    }
    let max_child = |children: &[Node]| {
        children
            .iter()
            .map(|child| estimate_extent(*child, display, ink))
            .fold(1.0_f32, f32::max)
    };
    match name.as_str() {
        "math" | "mrow" | "mstyle" | "merror" | "mphantom" | "menclose" | "semantics" => {
            max_child(&children)
        }
        "mfrac" => {
            let numerator = children
                .first()
                .map(|c| estimate_extent(*c, display, ink))
                .unwrap_or(1.0);
            let denominator = children
                .get(1)
                .map(|c| estimate_extent(*c, display, ink))
                .unwrap_or(1.0);
            // A stacked fraction adds the rule and the gaps around it, but its
            // two lines overlap for the part that straddles the baseline, so
            // adding both heights overestimates tall nests badly.
            (numerator.max(denominator) + 0.9).max(1.7)
        }
        "msup" => {
            let base = children
                .first()
                .map(|c| estimate_extent(*c, display, ink))
                .unwrap_or(1.0);
            let script = children
                .get(1)
                .map(|c| estimate_extent(*c, display, ink))
                .unwrap_or(1.0);
            // The script is drawn at 0.7em and raised 0.55em, so it reaches
            // `0.55 + 0.7*script` above the baseline; a fence also keeps the
            // font's 0.25em descent below the base.
            let overhang = if ink { 0.8 } else { 0.55 };
            base.max(script * 0.7 + overhang)
        }
        "msub" => {
            let base = children
                .first()
                .map(|c| estimate_extent(*c, display, ink))
                .unwrap_or(1.0);
            let script = children
                .get(1)
                .map(|c| estimate_extent(*c, display, ink))
                .unwrap_or(1.0);
            // A subscript hangs 0.25em below the baseline, inside the base's
            // own descent, so it rarely makes the row taller; the line-box
            // figure keeps the old (larger) value so pagination stays put.
            let overhang = if ink { 0.25 } else { 0.55 };
            base.max(script * 0.7 + overhang)
        }
        "msubsup" => {
            let base_node = children.first();
            let base = base_node
                .map(|c| estimate_extent(*c, display, ink))
                .unwrap_or(1.0);
            // Display style stacks the limits of a large operator above and
            // below it (see `render_scripts`); otherwise the scripts sit at the
            // side and only stick out a little.
            let stacked = display
                && base_node.is_some_and(|node| {
                    is_large_operator_node(*node) && !is_integral(&token_text(*node))
                });
            if stacked { base + 1.1 } else { base.max(1.2) }
        }
        "munderover" => {
            // `render_limits` always stacks the two scripts.
            let base = children
                .first()
                .map(|c| estimate_extent(*c, display, ink))
                .unwrap_or(1.0);
            base + 1.1
        }
        "munder" | "mover" => {
            let base = children
                .first()
                .map(|c| estimate_extent(*c, display, ink))
                .unwrap_or(1.0);
            base + 0.55
        }
        "msqrt" | "mroot" => max_child(&children) + 0.05,
        "mfenced" => max_child(&children),
        "mtable" => {
            // A row is its content's line box plus the cell padding
            // (`.cre-mtable td` uses 0.12em top and bottom); summing only the
            // content heights left fences around matrices short.
            let mut total = 0.0;
            for row in &children {
                let cells: Vec<Node> = element_children(*row).collect();
                total += max_child(&cells) + 0.3;
            }
            total.max(1.0)
        }
        "mtr" | "mtd" | "mth" => max_child(&children),
        _ => max_child(&children),
    }
}

/// Rough content width in `em`. Only used to angle `menclose` diagonal strikes
/// corner to corner, so a coarse token-width model is enough.
/// The width an `mspace` asks for, in em (default 0.25em per MathML core).
fn estimate_mspace_width(node: Node<'_, '_>) -> f32 {
    node.attribute("width")
        .and_then(parse_length_em)
        .unwrap_or(0.25)
        .clamp(0.0, 40.0)
}

/// `1.5em` / `2em` / `12px` / `0.3` (unitless is em) to em.
fn parse_length_em(value: &str) -> Option<f32> {
    let value = value.trim();
    let (number, unit) = value.split_at(
        value
            .find(|ch: char| !(ch.is_ascii_digit() || ch == '.' || ch == '-' || ch == '+'))
            .unwrap_or(value.len()),
    );
    let number: f32 = number.parse().ok()?;
    if !number.is_finite() || number < 0.0 {
        return None;
    }
    Some(match unit.trim() {
        "em" | "" => number,
        "ex" => number * 0.5,
        "px" => number / 16.0,
        "pt" => number / 12.0,
        "cm" => number / 0.42,
        "mm" => number / 4.2,
        "in" => number * 6.0,
        _ => number,
    })
}

fn estimate_width(node: Node<'_, '_>) -> f32 {
    if node.is_text() {
        return 0.5;
    }
    if !node.is_element() {
        return 0.5;
    }
    let name = node.tag_name().name().to_ascii_lowercase();
    let children: Vec<Node> = element_children(node).collect();
    let sum = |children: &[Node]| -> f32 {
        children
            .iter()
            .map(|child| estimate_width(*child))
            .sum::<f32>()
    };
    let max = |children: &[Node]| -> f32 {
        children
            .iter()
            .map(|child| estimate_width(*child))
            .fold(0.0_f32, f32::max)
    };
    match name.as_str() {
        "math" | "mrow" | "mstyle" | "merror" | "mphantom" | "semantics" | "mtr" | "mtd"
        | "mth" => sum(&children),
        "mspace" => estimate_mspace_width(node),
        "menclose" => sum(&children) + 0.24,
        "mfrac" => max(&children) + 0.3,
        "msup" | "msub" => {
            let base = children.first().map(|c| estimate_width(*c)).unwrap_or(0.5);
            let script = children.get(1).map(|c| estimate_width(*c)).unwrap_or(0.5);
            base + script * 0.6
        }
        "msubsup" | "munderover" | "mmultiscripts" => {
            let base = children.first().map(|c| estimate_width(*c)).unwrap_or(0.5);
            base + children
                .iter()
                .skip(1)
                .map(|c| estimate_width(*c))
                .fold(0.0_f32, f32::max)
                * 0.7
        }
        "munder" | "mover" => {
            let base = children.first().map(|c| estimate_width(*c)).unwrap_or(0.5);
            let script = children.get(1).map(|c| estimate_width(*c)).unwrap_or(0.5);
            base.max(script)
        }
        "msqrt" => sum(&children) + 0.86,
        "mroot" => sum(&children) + 1.12,
        "mfenced" => sum(&children) + 0.6,
        "mtable" => {
            // Column widths are not tracked; approximate a row's width by the
            // widest row's summed cells.
            let mut widest = 0.0_f32;
            for row in &children {
                let cells: Vec<Node> = element_children(*row).collect();
                widest = widest.max(
                    cells
                        .iter()
                        .map(|cell| estimate_width(*cell) + 0.5)
                        .sum::<f32>(),
                );
            }
            widest
        }
        _ => {
            let text = token_text(node);
            let chars = text.chars().count();
            if chars == 0 {
                sum(&children).max(0.5)
            } else {
                // Digits and letters are around half an em wide; punctuation
                // and operators a little less.
                let wide = text
                    .chars()
                    .filter(|c| c.is_alphanumeric() || *c > '\u{2000}')
                    .count();
                let narrow = chars - wide;
                (wide as f32 * 0.55 + narrow as f32 * 0.4).max(0.45)
            }
        }
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
    fn horizontal_operators_stretch_to_their_base() {
        let html = r#"<math><mover><mrow><mi>a</mi><mo>+</mo><mi>b</mi></mrow><mo>&#x23DE;</mo></mover></math>"#;
        let out = super::rewrite_mathml(html);
        assert!(
            out.contains("cre-mo-hstretch"),
            "overbrace should be stretched: {out}"
        );
        assert!(
            out.contains("scaleX("),
            "expected a horizontal scale: {out}"
        );

        // A non-stretchy script is left alone.
        let plain = r#"<math><mover><mi>a</mi><mi>b</mi></mover></math>"#;
        let out = super::rewrite_mathml(plain);
        assert!(!out.contains("cre-mo-hstretch"), "plain mover: {out}");
    }

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
        let html = r#"<math display="block"><msubsup><mi>x</mi><mi>i</mi><mn>2</mn></msubsup><munder><mo>lim</mo><mi>x</mi></munder></math>"#;
        let out = rewrite_mathml(html);
        assert!(out.contains("cre-script-stack"), "{out}");
        assert!(out.contains("cre-limits-under"), "{out}");
    }

    #[test]
    fn large_operator_scripts_hang_off_the_operator_ends() {
        // A lone script on `∫` is shifted off the middle of the sign: down for
        // a subscript, up for a superscript.
        let sub = rewrite_mathml(
            r#"<math><msub><mo>∫</mo><msup><mi>ℝ</mi><mi>N</mi></msup></msub></math>"#,
        );
        assert!(sub.contains("cre-script-stack"), "{sub}");
        assert!(sub.contains("vertical-align:-"), "{sub}");
        let sup = rewrite_mathml(r#"<math><msup><mo>∫</mo><mi>b</mi></msup></math>"#);
        assert!(sup.contains("vertical-align:"), "{sup}");
        assert!(!sup.contains("vertical-align:-"), "{sup}");

        // A pair is spread apart so the superscript reaches the top of the
        // operator and the subscript its foot.
        let both =
            rewrite_mathml(r#"<math><msubsup><mo>∫</mo><mi>a</mi><mi>b</mi></msubsup></math>"#);
        assert!(both.contains("line-height:"), "{both}");
        assert!(both.contains("cre-script-stack"), "{both}");
    }

    #[test]
    fn ordinary_scripts_keep_glyph_relative_offsets() {
        let sub = rewrite_mathml(r#"<math><msub><mi>x</mi><mi>i</mi></msub></math>"#);
        assert!(sub.contains("cre-sub"), "{sub}");
        assert!(!sub.contains("cre-op-scripts"), "{sub}");
        let pair =
            rewrite_mathml(r#"<math><msubsup><mi>x</mi><mi>i</mi><mn>2</mn></msubsup></math>"#);
        assert!(pair.contains("cre-script-stack"), "{pair}");
        assert!(!pair.contains("cre-op-scripts"), "{pair}");
    }

    #[test]
    fn munderover_keeps_the_mathml_script_order() {
        // MathML orders the scripts as base, underscript, overscript.
        let out = rewrite_mathml(
            r#"<math display="block"><munderover><mo>∑</mo><mrow><mi>k</mi><mo>=</mo><mn>1</mn></mrow><mi>∞</mi></munderover></math>"#,
        );
        let over = out
            .split("cre-limits-over")
            .nth(1)
            .expect("over limit")
            .split("cre-limits-base")
            .next()
            .expect("base");
        let under = out.split("cre-limits-under").nth(1).expect("under limit");
        assert!(
            over.contains("cre-mi\">∞<"),
            "the overscript must be above: {out}"
        );
        assert!(
            under.contains("cre-mi\">k<"),
            "the underscript must be below: {out}"
        );
    }

    #[test]
    fn fences_stretch_to_tall_content() {
        let html = r#"<math><mfenced open="[" close="]"><mtable><mtr><mtd><mi>x</mi></mtd></mtr><mtr><mtd><mi>y</mi></mtd></mtr></mtable></mfenced></math>"#;
        let out = rewrite_mathml(html);
        assert!(out.contains("scaleY("), "{out}");
        assert!(out.contains("cre-mtable"), "{out}");
        // The fence box takes the content height, so the stretched glyph has
        // something to line up with, and the scale makes up for the glyph's
        // own ink height.
        assert!(out.contains("height:"), "{out}");
        assert!(out.contains("line-height:"), "{out}");
    }

    #[test]
    fn fences_cover_stacked_limits_and_table_rows() {
        // `(∑_{k=1}^{n} ...)`: the fence has to span the limits above and below
        // the operator, not just the operator's own line.
        let limits = rewrite_mathml(
            r#"<math display="block"><mrow><mo>(</mo><munderover><mo>∑</mo><mrow><mi>k</mi><mo>=</mo><mn>1</mn></mrow><mi>n</mi></munderover><mi>a</mi><mo>)</mo></mrow></math>"#,
        );
        let height = limits
            .split("height:")
            .nth(1)
            .and_then(|rest| rest.split("em").next())
            .and_then(|value| value.parse::<f32>().ok())
            .expect("fence height");
        assert!(height >= 2.0, "fence height {height} in {limits}");

        // A four-row determinant needs bars tall enough for every row.
        let matrix = rewrite_mathml(
            r#"<math><mrow><mo>|</mo><mtable><mtr><mtd><mn>1</mn></mtd></mtr><mtr><mtd><mn>2</mn></mtd></mtr><mtr><mtd><mn>3</mn></mtd></mtr><mtr><mtd><mn>4</mn></mtd></mtr></mtable><mo>|</mo></mrow></math>"#,
        );
        let height = matrix
            .split("height:")
            .nth(1)
            .and_then(|rest| rest.split("em").next())
            .and_then(|value| value.parse::<f32>().ok())
            .expect("bar height");
        assert!(height >= 4.5, "bar height {height} in {matrix}");
    }

    #[test]
    fn mstyle_displaystyle_and_scriptlevel_apply() {
        // The W3C test suite raises the display style per formula with
        // `<mstyle displaystyle="true">`; without honouring it every `∑` kept
        // side scripts.
        let stacked = rewrite_mathml(
            r#"<math><mstyle displaystyle="true"><msubsup><mo>∑</mo><mi>i</mi><mi>n</mi></msubsup></mstyle></math>"#,
        );
        assert!(stacked.contains("cre-limits"), "{stacked}");
        let side = rewrite_mathml(
            r#"<math display="block"><mstyle displaystyle="false"><msubsup><mo>∑</mo><mi>i</mi><mi>n</mi></msubsup></mstyle></math>"#,
        );
        assert!(!side.contains("cre-limits"), "{side}");

        // `scriptlevel` sizes its content relative to the level it sits at.
        let small = rewrite_mathml(r#"<math><mstyle scriptlevel="1"><mi>x</mi></mstyle></math>"#);
        assert!(small.contains("font-size:0.700em"), "{small}");
        let same = rewrite_mathml(r#"<math><mstyle scriptlevel="0"><mi>x</mi></mstyle></math>"#);
        assert!(!same.contains("font-size"), "{same}");
    }

    #[test]
    fn maction_renders_only_the_selected_child() {
        // The W3C test suite puts tooltip text in a second `maction` child;
        // drawing it would spill prose into the formula.
        let out = rewrite_mathml(
            r#"<math><maction actiontype="statusline"><mi>P</mi><mtext>Probability of event E</mtext></maction></math>"#,
        );
        assert!(out.contains("cre-mi"), "{out}");
        assert!(!out.contains("Probability"), "{out}");
        let second =
            rewrite_mathml(r#"<math><maction selection="2"><mi>a</mi><mi>b</mi></maction></math>"#);
        assert!(second.contains(">b<"), "{second}");
        assert!(!second.contains(">a<"), "{second}");
    }

    #[test]
    fn movable_limits_follow_the_display_style() {
        // `∑` has movablelimits=true: display style stacks the scripts, inline
        // keeps them at the side — for `msubsup` and `munderover` alike.
        let display = rewrite_mathml(
            r#"<math display="block"><msubsup><mo>∑</mo><mrow><mi>i</mi></mrow><mi>n</mi></msubsup></math>"#,
        );
        assert!(display.contains("cre-limits"), "{display}");
        let inline = rewrite_mathml(
            r#"<math><msubsup><mo>∑</mo><mrow><mi>i</mi></mrow><mi>n</mi></msubsup></math>"#,
        );
        assert!(!inline.contains("cre-limits"), "{inline}");
        assert!(inline.contains("cre-script-stack"), "{inline}");
        let inline_under = rewrite_mathml(
            r#"<math><munderover><mo>∑</mo><mi>i</mi><mi>n</mi></munderover></math>"#,
        );
        assert!(!inline_under.contains("cre-limits"), "{inline_under}");

        // The attribute overrides the operator's default in both directions.
        let fixed = rewrite_mathml(
            r#"<math display="block"><msubsup><mo movablelimits="false">∑</mo><mi>i</mi><mi>n</mi></msubsup></math>"#,
        );
        assert!(!fixed.contains("cre-limits"), "{fixed}");
        let integral = rewrite_mathml(
            r#"<math><munderover><mo>∫</mo><mi>a</mi><mi>b</mi></munderover></math>"#,
        );
        assert!(integral.contains("cre-limits"), "{integral}");
    }

    #[test]
    fn scripts_climb_a_tall_base() {
        // On a stretched fence the superscript belongs near the base's top;
        // a fixed shift would leave it halfway up (Chrome puts it at the top).
        let tall = rewrite_mathml(
            r#"<math><msup><mrow><mo>(</mo><mfrac><mn>1</mn><mn>2</mn></mfrac><mo>)</mo></mrow><mn>2</mn></msup></math>"#,
        );
        let shift = tall
            .split("cre-sup")
            .nth(1)
            .and_then(|rest| rest.split("vertical-align:").nth(1))
            .and_then(|rest| rest.split("em").next())
            .and_then(|value| value.parse::<f32>().ok())
            .expect("superscript shift");
        assert!(shift > 1.0, "shift {shift} in {tall}");

        // A plain glyph keeps the font's usual shift.
        let flat = rewrite_mathml(r#"<math><msup><mi>x</mi><mn>2</mn></msup></math>"#);
        let shift = flat
            .split("cre-sup")
            .nth(1)
            .and_then(|rest| rest.split("vertical-align:").nth(1))
            .and_then(|rest| rest.split("em").next())
            .and_then(|value| value.parse::<f32>().ok())
            .expect("superscript shift");
        assert!(shift < 0.8, "shift {shift} in {flat}");
    }

    #[test]
    fn empty_script_slots_do_not_shift_the_other_script() {
        // `<msubsup><mi>a</mi><mi>k</mi><mrow/></msubsup>` is `a_k`: the empty
        // superscript must not make the subscript hang on the axis.
        let sub =
            rewrite_mathml(r#"<math><msubsup><mi>a</mi><mi>k</mi><mrow></mrow></msubsup></math>"#);
        assert!(sub.contains("cre-sub"), "{sub}");
        assert!(!sub.contains("cre-script-stack"), "{sub}");

        // ...and `<mrow/><mn>2</mn>` leaves a superscript.
        let sup =
            rewrite_mathml(r#"<math><msubsup><mi>x</mi><mrow></mrow><mn>2</mn></msubsup></math>"#);
        assert!(sup.contains("cre-sup"), "{sup}");
        assert!(!sup.contains("cre-script-stack"), "{sup}");

        // Both present keeps the stacked pair.
        let both =
            rewrite_mathml(r#"<math><msubsup><mi>x</mi><mi>i</mi><mn>2</mn></msubsup></math>"#);
        assert!(both.contains("cre-script-stack"), "{both}");
    }

    #[test]
    fn short_fences_stay_on_the_baseline() {
        // `(v_j - v_i)` next to a tall `∏`: a token-high fence must not be
        // stretched (and pinned to the line top), or it drifts away from the
        // expression it belongs to.
        let out = rewrite_mathml(
            r#"<math><mrow><munder><mo>∏</mo><mrow><mi>i</mi></mrow></munder><mo>(</mo><msub><mi>v</mi><mi>j</mi></msub><mo>)</mo></mrow></math>"#,
        );
        assert!(!out.contains("scaleY("), "{out}");
    }

    #[test]
    fn vinculum_starts_at_the_surd_tip() {
        // The overbar is its own element anchored at the drawn surd's right
        // edge, so it cannot stick out to the left of the diagonal.
        let out = rewrite_mathml(r#"<math><msqrt><mi>x</mi></msqrt></math>"#);
        assert!(out.contains("cre-vinculum"), "{out}");
        let style = out
            .split("cre-vinculum")
            .nth(1)
            .and_then(|rest| rest.split("style=\"").nth(1))
            .and_then(|rest| rest.split('"').next())
            .expect("vinculum style");
        assert!(style.contains("left:"), "{style}");
        assert!(style.contains("right:0"), "{style}");
        assert!(
            !out.contains("border-top:1px solid currentColor; padding:0.12em"),
            "{out}"
        );
    }

    #[test]
    fn fence_scale_makes_up_for_the_glyph_ink_height() {
        // A `(` paints about 0.86 em of ink, so a 2 em fence needs ~2.3x.
        let mut metrics = MathMetrics::default();
        let slot = FENCE_CHARS.iter().position(|ch| *ch == '(').unwrap();
        metrics.fence_ink[slot] = 0.86;
        let scale = fence_scale("(", 2.0, metrics);
        assert!((scale - 2.326).abs() < 0.01, "scale {scale}");
        // A glyph with full-height ink needs no correction.
        metrics.fence_ink[slot] = 1.0;
        assert!((fence_scale("(", 2.0, metrics) - 2.0).abs() < 0.001);
    }

    #[test]
    fn stretched_fences_are_centred_on_the_content() {
        // A glyph whose ink sits low in the em: scaling about the box centre
        // would push it further down, so the style has to lift it back.
        let mut metrics = MathMetrics::default();
        let slot = FENCE_CHARS.iter().position(|ch| *ch == '{').unwrap();
        metrics.fence_ink[slot] = 0.8;
        metrics.fence_center[slot] = 0.1;
        metrics.ascent = 0.9;
        metrics.descent = 0.25;
        // offset = (0.9 - 0.25) / 2 - 0.1 = 0.225; scale = 4.5 / 0.8 = 5.625
        let style = stretched_fence_style("{", 4.5, 4.5, 0.0, metrics);
        assert!(style.contains("height:4.500em"), "{style}");
        assert!(style.contains("vertical-align:top"), "{style}");
        assert!(style.contains("scaleY(5.625)"), "{style}");
        assert!(style.contains("translateY(-1.266em)"), "{style}");

        // An ink centre that already matches the baseline centre needs no
        // shift at all.
        metrics.fence_center[slot] = (metrics.ascent - metrics.descent) / 2.0;
        let centred = stretched_fence_style("{", 4.5, 4.5, 0.0, metrics);
        assert!(centred.contains("translateY(0.000em)"), "{centred}");

        // A fence whose content paints above its line box (a superscript) has
        // to grow past the box while the box keeps the line height.
        metrics.fence_center[slot] = 0.1;
        let grown = stretched_fence_style("{", 1.0, 1.5, 0.0, metrics);
        assert!(grown.contains("height:1.000em"), "{grown}");
        assert!(grown.contains("scaleY(1.875)"), "{grown}");
        assert!(grown.contains("translateY(-0.172em)"), "{grown}");
    }

    #[test]
    fn mfenced_uses_the_same_stretch_and_centring() {
        let out = rewrite_mathml(
            r#"<math><mfenced open="{"><mtable><mtr><mtd><mn>1</mn></mtd></mtr><mtr><mtd><mn>2</mn></mtd></mtr></mtable></mfenced></math>"#,
        );
        assert!(
            out.contains("vertical-align:top;transform:translateY("),
            "{out}"
        );
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
        assert!(out.contains("cre-strike-diag"), "{out}");
        // The strike rises left to right.
        assert!(out.contains("transform:rotate(-"), "{out}");
        let diag = rewrite_mathml(
            r#"<math><menclose notation="downdiagonalstrike"><mi>x</mi></menclose></math>"#,
        );
        assert!(diag.contains("transform:rotate("), "{diag}");
        assert!(diag.contains("cre-strike-diag"), "{diag}");
    }

    #[test]
    fn menclose_diagonal_follows_the_content_aspect() {
        // A wide radicand inside a fraction gives a shallow strike, not 45
        // degrees, so it lands on the corners of the box.
        let out = rewrite_mathml(
            r#"<math><menclose notation="updiagonalstrike"><mfrac><msup><mi>x</mi><mn>2</mn></msup><msqrt><mrow><mn>1</mn><mo>+</mo><msup><mi>x</mi><mn>4</mn></msup></mrow></msqrt></mfrac></menclose></math>"#,
        );
        let angle = out
            .split("transform:rotate(-")
            .nth(1)
            .and_then(|rest| rest.split("deg").next())
            .and_then(|value| value.parse::<f32>().ok())
            .expect("strike angle");
        assert!(angle < 42.0, "angle {angle} in {out}");
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

    #[test]
    fn presentation_attributes_and_author_classes_are_kept() {
        // The W3C `mathml-020` styling test and the `mathml-021` CSS test.
        let html = r##"<math class="basic"><mtable mathbackground="#FFFFE0" frame="dashed"><mtr><mtd><mpadded width="+0.2em" height="+0.2em" depth="+0.2em"><mstyle mathcolor="#000000"><mrow><mn>2</mn><mo mathsize="150%">+</mo><mstyle mathbackground="green" mathcolor="white" mathvariant="bold"><mi class="test2">x</mi></mstyle></mrow></mstyle></mpadded></mtd></mtr></mtable></math>"##;
        let out = rewrite_mathml(html);
        assert!(out.contains("class=\"cre-math basic\""), "{out}");
        assert!(out.contains("background-color:#FFFFE0"), "{out}");
        assert!(out.contains("border:1px dashed"), "{out}");
        assert!(out.contains("padding-right:0.2em"), "{out}");
        assert!(out.contains("padding-top:0.2em"), "{out}");
        assert!(out.contains("padding-bottom:0.2em"), "{out}");
        assert!(out.contains("font-size:150%"), "{out}");
        assert!(out.contains("background-color:green"), "{out}");
        assert!(out.contains("color:white"), "{out}");
        assert!(out.contains("font-weight:bold"), "{out}");
        assert!(out.contains("class=\"cre-mi-upright test2\""), "{out}");
    }

    #[test]
    fn author_classes_on_operators_survive() {
        let out = rewrite_mathml(
            r#"<math><mi>a</mi><mo class="test1">+</mo><mi class="test2">x</mi></math>"#,
        );
        assert!(out.contains("class=\"cre-mo cre-mo-bin test1\""), "{out}");
        assert!(out.contains("class=\"cre-mi test2\""), "{out}");
    }
}

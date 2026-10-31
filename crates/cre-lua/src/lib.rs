use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{c_char, c_int};
use std::hash::{Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use cre_lib::{Document, DocumentFormat, DocumentOptions};
use mlua::ffi as lua;
use mlua::lua_State;
use read_fonts::TableProvider;

mod bridge_helpers;
use bridge_helpers::*;

const CREDOC: &[u8] = b"credocument\0";
const CREIMAGE: &[u8] = b"creimage\0";
const INDEX: &[u8] = b"__index\0";

#[repr(C)]
struct BlitBuffer {
    w: u32,
    pixel_stride: u32,
    h: u32,
    stride: usize,
    data: *mut u8,
    config: u8,
}

#[derive(Clone)]
struct PageMapEntry {
    page: usize,
    xpointer: String,
    doc_y: i64,
    label: String,
}

struct Doc {
    inner: Option<Document>,
    width: u32,
    height: u32,
    layout_height: u32,
    margins: (i32, i32, i32, i32),
    font_face: String,
    font_path: Option<PathBuf>,
    font_size: i32,
    path: Option<PathBuf>,
    last_range: Option<(String, String)>,
    last_boxes: Vec<cre_lib::TextBox>,
    properties: HashMap<String, String>,
    alt_properties: HashMap<String, String>,
    view_mode: i32,
    visible_page_count: i32,
    header_height: i32,
    interline: i32,
    stylesheet: String,
    embedded_stylesheet_enabled: bool,
    background_color: String,
    background_color_value: u32,
    background_image: String,
    battery: i32,
    partial_rerendering: bool,
    rerender_count: usize,
    history: Vec<String>,
    history_index: usize,
    page_map: Vec<PageMapEntry>,
    page_map_source: String,
    page_map_chars: usize,
    render_dpi: u32,
    scale_font_with_dpi: bool,
    highlight: Option<(String, String)>,
    callback: Option<c_int>,
    render_requested: bool,
    font_data_cache: Vec<cre_lib::FontBlob>,
    font_cache_paths: Vec<PathBuf>,
    render_state_snapshot: String,
}

impl Doc {
    fn new(width: u32, height: u32, view_mode: i32) -> Self {
        Self {
            inner: None,
            width: width.max(1),
            height: height.max(1),
            layout_height: 100_000,
            margins: (0, 0, 0, 0),
            font_face: "Noto Serif".to_string(),
            font_path: None,
            font_size: 22,
            path: None,
            last_range: None,
            last_boxes: Vec::new(),
            properties: HashMap::new(),
            alt_properties: HashMap::new(),
            view_mode,
            visible_page_count: 1,
            header_height: 0,
            interline: 100,
            stylesheet: String::new(),
            embedded_stylesheet_enabled: true,
            background_color: String::new(),
            background_color_value: 0xFF_FFFF,
            background_image: String::new(),
            battery: 0,
            partial_rerendering: false,
            rerender_count: 0,
            history: Vec::new(),
            history_index: 0,
            page_map: Vec::new(),
            page_map_source: String::new(),
            page_map_chars: 0,
            render_dpi: 96,
            scale_font_with_dpi: false,
            highlight: None,
            callback: None,
            render_requested: false,
            font_data_cache: Vec::new(),
            font_cache_paths: Vec::new(),
            render_state_snapshot: String::new(),
        }
    }

    fn render_viewport_scale(&self) -> f32 {
        if self.render_dpi == 0 || self.render_dpi == 96 {
            1.0
        } else {
            (self.render_dpi as f32) / 96.0
        }
    }

    fn effective_font_size(&self) -> i32 {
        let requested = self.font_size.max(1);
        if !self.scale_font_with_dpi {
            return requested;
        }
        let scale = self.render_viewport_scale();
        if scale == 1.0 {
            return requested;
        }
        let scaled = ((requested as f32) * scale).round() as i32;
        scaled.clamp(1, 512)
    }

    fn render_font_css_size(&self) -> f32 {
        let requested = self.font_size.max(1) as f32;
        let scale = self.render_viewport_scale();
        if self.scale_font_with_dpi || scale == 1.0 {
            return requested;
        }
        requested / scale
    }

    fn default_font_feature_css(&self) -> String {
        r#"body, div, p, span, li, td, th, h1, h2, h3, h4, h5, h6 { font-feature-settings: "halt" 1, "chws" 1; }"#.to_string()
    }

    fn settings_stylesheet(&self) -> String {
        let mut rules: Vec<String> = Vec::new();
        let default_font_feature_css = self.default_font_feature_css();
        if !default_font_feature_css.is_empty() {
            rules.push(default_font_feature_css);
        }
        if self.interline != 100 {
            let percent = self.interline.clamp(1, 1000);
            let line_height = (((percent as f32) / 100.0) * 1.2).clamp(0.2, 10.0);
            rules.push(format!(
                "body, div, p, span, li, td, th, h1, h2, h3, h4, h5, h6 {{ line-height: {:.3} !important; }}",
                line_height,
            ));
        }
        for name in ["font.face.base.weight", "font.face.weight.embolden"] {
            if let Some(value) = self.properties.get(name)
                && let Ok(weight) = value.trim().parse::<i64>()
            {
                if (1_i64..=1000_i64).contains(&weight) && weight != 400 {
                    rules.push(format!(
                        "body, div, p, span, li, td, th, h1, h2, h3, h4, h5, h6 {{ font-weight: {} !important; }}",
                        weight,
                    ));
                }
                break;
            }
        }
        let settings_css = rules.join("\n");
        if self.stylesheet.is_empty() {
            return settings_css;
        }
        if settings_css.is_empty() {
            return self.stylesheet.clone();
        }
        format!("{}\n{}", settings_css, self.stylesheet)
    }

    fn render_font_gamma(&self) -> f32 {
        if let Some(value) = self.properties.get("font.gamma")
            && let Ok(gamma) = value.trim().parse::<f32>()
        {
            return gamma.clamp(0.5, 2.0);
        }
        if let Some(value) = self.properties.get("font.gamma.index")
            && let Ok(index) = value.trim().parse::<i64>()
        {
            let index = index.clamp(0, 30) as f32;
            return (0.65 + (index * 0.7 / 30.0)).clamp(0.5, 2.0);
        }
        let global_gamma = gamma_level_for_index(GAMMA_INDEX.load(Ordering::Relaxed));
        if global_gamma.is_finite() && global_gamma != 1.0 {
            return (global_gamma as f32).clamp(0.1, 15.0);
        }
        1.0
    }

    fn render_state_snapshot(&self, path: PathBuf) -> String {
        let mut properties = self.properties.iter().collect::<Vec<(&String, &String)>>();
        properties.sort_by(|left, right| left.0.cmp(right.0));
        let mut alt_properties = self
            .alt_properties
            .iter()
            .collect::<Vec<(&String, &String)>>();
        alt_properties.sort_by(|left, right| left.0.cmp(right.0));
        let global_gamma = GAMMA_INDEX.load(Ordering::Relaxed);
        format!(
            "{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}",
            path,
            self.width,
            self.height,
            self.layout_height,
            self.margins,
            self.font_face,
            self.font_path,
            self.font_size,
            self.view_mode,
            self.visible_page_count,
            self.header_height,
            self.interline,
            self.stylesheet,
            self.embedded_stylesheet_enabled,
            self.render_dpi,
            self.scale_font_with_dpi,
            properties,
            alt_properties,
            global_gamma,
        )
    }

    fn open(&mut self, path: PathBuf) -> anyhow::Result<()> {
        let snapshot = self.render_state_snapshot(path.clone());
        if self.inner.is_some() && self.render_state_snapshot == snapshot {
            return Ok(());
        }
        self.render_state_snapshot = snapshot;
        let effective_stylesheet = self.settings_stylesheet();
        let options = DocumentOptions {
            width: self.width,
            height: self.height,
            layout_height: self.layout_height,
            page_margin: self.margins.1.max(0) as f32,
            font_size: self.render_font_css_size(),
            font_path: self.font_path.clone(),
            fallback_font_paths: self.fallback_font_paths(),
            stylesheet: (!effective_stylesheet.is_empty()).then(|| effective_stylesheet.clone()),
            embedded_styles_enabled: self.embedded_stylesheet_enabled,
            viewport_scale: self.render_viewport_scale(),
        };
        let font_paths = cre_lib::resolve_font_paths(&options)?;
        if self.font_cache_paths != font_paths {
            self.font_data_cache = cre_lib::load_font_datas(&font_paths);
            self.font_cache_paths = font_paths.clone();
        }
        let mut opened =
            Document::with_options_with_font_data(&path, &options, &self.font_data_cache)?;
        opened.set_font_gamma(self.render_font_gamma());
        opened.set_background_color(self.background_color_value);
        self.inner = Some(opened);
        self.path = Some(path);
        cre_lib::trace_memory("doc open");
        self.load_document_page_map();
        Ok(())
    }

    fn fallback_font_paths(&self) -> Vec<PathBuf> {
        let value = self
            .properties
            .get("crengine.font.fallback.faces")
            .cloned()
            .unwrap_or(String::new());
        let mut paths: Vec<PathBuf> = Vec::new();
        for name in value.split('|') {
            if let Some(face) = find_font_face(name, false, false) {
                paths.push(face.path.clone());
            }
        }
        paths
    }

    fn two_visible_pages_count_as_one(&self) -> bool {
        match self
            .properties
            .get("window.pages.two.visible.as.one.page.number")
        {
            Some(value) => value == "1",
            None => self.visible_page_count.max(1) > 1,
        }
    }

    fn visible_page_number_count(&self) -> i64 {
        if self.two_visible_pages_count_as_one() {
            1
        } else {
            i64::from(self.visible_page_count.max(1))
        }
    }

    fn relayout(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let xpointer = self.inner.as_ref().and_then(|inner| {
            inner
                .xpointer_from_page(inner.current_page())
                .ok()
                .filter(|xp| !xp.is_empty())
        });
        let old = self.inner.take();
        if self.open(path).is_err() {
            self.inner = old;
            return;
        }
        if let (Some(xpointer), Some(inner)) = (xpointer, self.inner.as_mut()) {
            let _ = inner.go_to_xpointer(&xpointer);
        }
        self.load_document_page_map();
    }

    fn current_xpointer(&self) -> Option<String> {
        self.inner.as_ref().and_then(|inner| {
            inner
                .xpointer_from_page(inner.current_page())
                .ok()
                .filter(|xpointer| !xpointer.is_empty())
        })
    }

    fn load_document_page_map(&mut self) {
        self.page_map.clear();
        self.page_map_source.clear();
        self.page_map_chars = 0;
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        self.page_map_source = inner.document_page_map_source().to_string();
        self.page_map = inner
            .document_page_map()
            .iter()
            .map(|entry| PageMapEntry {
                page: entry.page,
                xpointer: entry.xpointer.clone().unwrap_or_default(),
                doc_y: entry.doc_y,
                label: entry.label.clone(),
            })
            .collect();
    }

    fn record_history(&mut self) {
        let Some(xpointer) = self.current_xpointer() else {
            return;
        };
        if self.history.get(self.history_index) == Some(&xpointer) {
            return;
        }
        self.history.truncate(self.history_index + 1);
        if self.history.last() == Some(&xpointer) {
            return;
        }
        self.history.push(xpointer);
        self.history_index = self.history.len().saturating_sub(1);
    }

    fn build_synthetic_page_map(&mut self, chars_per_page: usize) {
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        let chars_per_page = chars_per_page.max(1);
        let mut entries = Vec::new();
        let mut characters = 0usize;
        let mut next_label_at = 0usize;
        let mut label = 1usize;
        for page in 1..=inner.page_count() {
            if (characters >= next_label_at || page == 1)
                && let Ok(xpointer) = inner.xpointer_from_page(page)
            {
                entries.push(PageMapEntry {
                    page,
                    xpointer,
                    doc_y: (page.saturating_sub(1) * self.height as usize) as i64,
                    label: label.to_string(),
                });
                label += 1;
                next_label_at = characters + chars_per_page;
            }
            characters += inner.page_text(page).map_or(0, |text| text.chars().count());
        }
        self.page_map = entries;
        self.page_map_chars = chars_per_page;
        self.page_map_source = "synthetic".to_string();
    }

    fn page_map_index_for_doc_y_below(&self, y: i64) -> Option<usize> {
        if self.page_map.is_empty() {
            return None;
        }
        let index = self
            .page_map
            .partition_point(|entry| entry.doc_y < y)
            .saturating_sub(1);
        Some(index)
    }

    fn page_map_index_for_doc_y_at_or_below(&self, y: i64) -> Option<usize> {
        if self.page_map.is_empty() {
            return None;
        }
        let index = self
            .page_map
            .partition_point(|entry| entry.doc_y <= y)
            .saturating_sub(1);
        Some(index)
    }

    fn page_map_first_index_at_or_above(&self, y: i64) -> Option<usize> {
        if self.page_map.is_empty() {
            return None;
        }
        Some(self.page_map.partition_point(|entry| entry.doc_y < y))
    }

    fn current_page(&self) -> usize {
        self.inner.as_ref().map_or(1, Document::current_page)
    }

    fn page_count(&self) -> usize {
        self.inner.as_ref().map_or(0, Document::page_count)
    }
}

#[derive(Clone)]
struct FontFace {
    family: String,
    full_name: String,
    postscript_name: String,
    path: PathBuf,
    index: i32,
    weight: i32,
    italic: bool,
    monospace: bool,
    math: bool,
    color: bool,
    bitmap: bool,
}

struct CreImage {
    image: Option<(Vec<u8>, u32, u32)>,
}

impl CreImage {
    fn from_svg(data: &[u8]) -> Option<Self> {
        let (_, width, height) = cre_lib::render_svg_image(data, None, None)?;
        Some(Self {
            image: Some((data.to_vec(), width, height)),
        })
    }

    fn free(&mut self) {
        self.image = None;
    }
}

static REGISTERED_FONTS: OnceLock<Mutex<Vec<FontFace>>> = OnceLock::new();
static FONT_BIAS: OnceLock<Mutex<HashMap<String, i32>>> = OnceLock::new();
/// Approximate original crengine `gammatbl.h` table used by `setGammaIndex`.
const CRE_GAMMA_LEVELS: [f64; 57] = [
    0.300000, 0.350000, 0.400000, 0.450000, 0.500000, 0.550000, 0.600000, 0.650000, 0.700000,
    0.750000, 0.800000, 0.850000, 0.900000, 0.950000, 0.980000, 1.000000, 1.020000, 1.050000,
    1.100000, 1.150000, 1.200000, 1.250000, 1.300000, 1.350000, 1.400000, 1.450000, 1.500000,
    1.600000, 1.700000, 1.800000, 1.900000, 2.000000, 2.100000, 2.200000, 2.300000, 2.400000,
    2.500000, 2.600000, 2.700000, 2.800000, 2.900000, 3.000000, 3.500000, 4.000000, 4.500000,
    5.000000, 5.500000, 6.000000, 7.000000, 8.000000, 9.000000, 10.000000, 11.000000, 12.000000,
    13.000000, 14.000000, 15.000000,
];
static GAMMA_INDEX: AtomicI32 = AtomicI32::new(15);
fn gamma_level_for_index(index: i32) -> f64 {
    CRE_GAMMA_LEVELS[index.clamp(0, 56) as usize]
}

static IMAGE_REPLACEMENT_CHAR: AtomicU32 = AtomicU32::new(' ' as u32);
const REGEX_SEARCH_ERROR_NONE: i32 = 0;
const REGEX_SEARCH_ERROR_INVALID_PATTERN: i32 = 1;
static REGEX_SEARCH_ERROR: AtomicI32 = AtomicI32::new(REGEX_SEARCH_ERROR_NONE);

fn font_dir_state() -> &'static Mutex<String> {
    static STATE: OnceLock<Mutex<String>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(String::new()))
}

fn hyph_state() -> &'static Mutex<String> {
    static STATE: OnceLock<Mutex<String>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(String::new()))
}

fn user_hyph_state() -> &'static Mutex<HashSet<String>> {
    static STATE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(HashSet::new()))
}

fn registered_font_faces() -> Vec<FontFace> {
    let registered = REGISTERED_FONTS.get_or_init(|| Mutex::new(Vec::new()));
    match registered.lock() {
        Ok(fonts) => {
            let mut sorted = fonts.clone();
            sort_font_faces(&mut sorted);
            sorted
        }
        Err(_) => Vec::new(),
    }
}

fn sort_font_faces(faces: &mut [FontFace]) {
    faces.sort_by(|left, right| {
        left.family
            .to_lowercase()
            .cmp(&right.family.to_lowercase())
            .then_with(|| left.weight.cmp(&right.weight))
            .then_with(|| left.italic.cmp(&right.italic))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.index.cmp(&right.index))
    });
}

fn parse_font_metadata(path: &Path) -> Vec<FontFace> {
    // Fonts can be tens of megabytes (CJK system fonts) and this runs for
    // every registered face during startup; mmap them instead of reading a
    // fresh heap copy each time.
    let Some(blob) = cre_lib::load_font_blob(path) else {
        return Vec::new();
    };
    let data: &[u8] = (*blob).as_ref();
    read_fonts::FontRef::fonts(data)
        .enumerate()
        .filter_map(|(index, font)| {
            font.ok()
                .and_then(|font| parse_font_face(path, &font, index))
        })
        .collect()
}

fn parse_font_face(path: &Path, font: &read_fonts::FontRef<'_>, index: usize) -> Option<FontFace> {
    let subfamily = font_name(font, read_fonts::types::NameId::TYPOGRAPHIC_SUBFAMILY_NAME)
        .or_else(|| font_name(font, read_fonts::types::NameId::SUBFAMILY_NAME))
        .unwrap_or_default();
    let full_name = font_name(font, read_fonts::types::NameId::FULL_NAME).unwrap_or_default();
    let postscript_name =
        font_name(font, read_fonts::types::NameId::POSTSCRIPT_NAME).unwrap_or_default();
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let family = font_name(font, read_fonts::types::NameId::WWS_FAMILY_NAME)
        .or_else(|| font_name(font, read_fonts::types::NameId::TYPOGRAPHIC_FAMILY_NAME))
        .or_else(|| font_name(font, read_fonts::types::NameId::FAMILY_NAME))
        .filter(|value| !value.is_empty())
        .or_else(|| infer_family(&full_name, &postscript_name))
        .filter(|value| !value.is_empty())
        .unwrap_or(stem);
    if family.is_empty() {
        return None;
    }

    let mut weight = infer_weight(&format!("{family} {subfamily} {postscript_name}"));
    let mut italic = contains_style_word(&subfamily, "italic")
        || contains_style_word(&subfamily, "oblique")
        || contains_style_word(&postscript_name, "italic")
        || contains_style_word(&postscript_name, "oblique");
    if let Ok(os2) = font.os2() {
        weight = i32::from(os2.us_weight_class());
        italic |= os2
            .fs_selection()
            .intersects(read_fonts::tables::os2::SelectionFlags::ITALIC);
    }
    let mut monospace = false;
    if let Ok(post) = font.post() {
        italic |= post.italic_angle().to_f64() != 0.0;
        monospace |= post.is_fixed_pitch() != 0;
    }
    if let Ok(os2) = font.os2() {
        monospace |= os2.panose_10().first().is_some_and(|family| *family == 5);
    }
    let math = font
        .data_for_tag(read_fonts::types::Tag::new(b"MATH"))
        .is_some();
    let color = font
        .data_for_tag(read_fonts::types::Tag::new(b"COLR"))
        .is_some();
    let bitmap = font
        .data_for_tag(read_fonts::types::Tag::new(b"CBDT"))
        .is_some()
        || font
            .data_for_tag(read_fonts::types::Tag::new(b"sbix"))
            .is_some();

    Some(FontFace {
        family,
        full_name,
        postscript_name,
        path: path.to_path_buf(),
        index: index as i32,
        weight,
        italic,
        monospace,
        math,
        color,
        bitmap,
    })
}

fn font_name(font: &read_fonts::FontRef<'_>, name_id: read_fonts::types::NameId) -> Option<String> {
    let name_table = font.name().ok()?;
    let string_data = name_table.string_data();
    let mut best: Option<(u32, String)> = None;
    for record in name_table.name_record() {
        if record.name_id() != name_id || !record.is_unicode() {
            continue;
        }
        let score = match record.platform_id() {
            3 => 2,
            0 | 1 => 1,
            _ => 0,
        } + if record.language_id() == 0x0409 || record.language_id() == 0 {
            4
        } else {
            0
        };
        if score == 0 {
            continue;
        }
        let Ok(value) = record.string(string_data) else {
            continue;
        };
        let decoded = value.to_string();
        if decoded.is_empty() {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|(best_score, _)| score > *best_score)
        {
            best = Some((score, decoded));
        }
    }
    best.map(|(_, value)| value)
}

fn infer_family(full_name: &str, postscript_name: &str) -> Option<String> {
    let source = if !full_name.is_empty() {
        full_name
    } else if !postscript_name.is_empty() {
        postscript_name
    } else {
        return None;
    };
    let mut value = source.to_string();
    for style in [
        " Bold Italic",
        " Italic",
        " Bold",
        " Oblique",
        "-Bold Italic",
        "-Italic",
        "-Bold",
        "-Oblique",
        " BoldItalic",
        "-BoldItalic",
    ] {
        if let Some((family, _)) = value.rsplit_once(style) {
            value = family.to_string();
            break;
        }
    }
    (!value.is_empty()).then_some(value)
}

fn infer_weight(text: &str) -> i32 {
    let text = text.to_lowercase();
    for (word, weight) in [
        ("extrablack", 950),
        ("ultrablack", 950),
        ("black", 900),
        ("extrabold", 800),
        ("ultrabold", 800),
        ("demibold", 600),
        ("semibold", 600),
        ("bold", 700),
        ("extralight", 200),
        ("ultralight", 200),
        ("semilight", 350),
        ("demilight", 350),
        ("light", 300),
        ("medium", 500),
        ("thin", 100),
    ] {
        if text.contains(word) {
            return weight;
        }
    }
    if text.contains("regular") || text.contains("roman") || text.contains("book") {
        return 400;
    }
    400
}

fn contains_style_word(text: &str, word: &str) -> bool {
    let text = text.to_lowercase();
    let mut start = 0;
    while let Some(offset) = text[start..].find(word) {
        let end = start + offset + word.len();
        let before_ok =
            start == 0 || !text[start - 1..].starts_with(|ch: char| ch.is_alphanumeric());
        let after_ok =
            end >= text.len() || !text[end..].starts_with(|ch: char| ch.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        start = end;
    }
    false
}

fn normalize_face_name(name: &str) -> String {
    name.chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '-' && *ch != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

fn font_matches(face: &FontFace, normalized: &str) -> bool {
    [
        face.family.as_str(),
        face.full_name.as_str(),
        face.postscript_name.as_str(),
    ]
    .iter()
    .any(|value| normalize_face_name(value) == normalized)
}

fn find_font_face(name: &str, bold: bool, italic: bool) -> Option<FontFace> {
    let faces = registered_font_faces();
    select_font_face(&faces, name, bold, italic)
}

fn select_font_face(faces: &[FontFace], name: &str, bold: bool, italic: bool) -> Option<FontFace> {
    let normalized = normalize_face_name(name);
    let matches: Vec<&FontFace> = faces
        .iter()
        .filter(|face| font_matches(face, &normalized))
        .collect();
    let exact_family = matches
        .iter()
        .filter(|face| normalize_face_name(&face.family) == normalized)
        .copied()
        .collect::<Vec<_>>();
    let candidates: &[&FontFace] = if exact_family.is_empty() {
        &matches
    } else {
        &exact_family
    };
    candidates
        .iter()
        .copied()
        .min_by_key(|face| {
            let italic_penalty = usize::from(face.italic != italic);
            let weight_penalty = if bold {
                (face.weight - 700).abs()
            } else {
                (face.weight - 400).abs()
            };
            (
                face.family.to_lowercase(),
                italic_penalty,
                weight_penalty,
                face.path.clone(),
                face.index,
            )
        })
        .cloned()
}

fn font_families() -> Vec<String> {
    let mut families = BTreeMap::new();
    for face in registered_font_faces() {
        if face.family.is_empty() {
            continue;
        }
        families
            .entry(normalize_face_name(&face.family))
            .or_insert_with(|| face.family.clone());
    }
    let bias = font_bias();
    let mut families = families
        .into_iter()
        .map(|(normalized, family)| {
            let bias = bias.get(&normalized).copied().unwrap_or(0);
            (bias, family)
        })
        .collect::<Vec<_>>();
    families.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    families.into_iter().map(|(_, family)| family).collect()
}

fn font_bias() -> HashMap<String, i32> {
    let state = FONT_BIAS.get_or_init(|| Mutex::new(HashMap::new()));
    match state.lock() {
        Ok(bias) => bias.clone(),
        Err(_) => HashMap::new(),
    }
}

unsafe fn doc_ref<'a>(l: *mut lua_State) -> Option<&'a Doc> {
    // SAFETY: methods registered in the `credocument` metatable always receive our userdata.
    let ptr = unsafe { lua::lua_touserdata(l, 1) }.cast::<Doc>();
    // SAFETY: the userdata was initialized by `new_doc_view`.
    unsafe { ptr.as_ref() }
}

unsafe fn doc_mut<'a>(l: *mut lua_State) -> Option<&'a mut Doc> {
    // SAFETY: methods registered in the `credocument` metatable always receive our userdata.
    let ptr = unsafe { lua::lua_touserdata(l, 1) }.cast::<Doc>();
    // SAFETY: the userdata was initialized by `new_doc_view`.
    unsafe { ptr.as_mut() }
}

unsafe fn push_str(l: *mut lua_State, value: &str) {
    // SAFETY: `value` is valid for the duration of the call and Lua copies it.
    unsafe { lua::lua_pushlstring_(l, value.as_ptr().cast(), value.len()) };
}

unsafe fn push_bytes(l: *mut lua_State, value: &[u8]) {
    // SAFETY: `value` is valid for the duration of the call and Lua copies it.
    unsafe { lua::lua_pushlstring_(l, value.as_ptr().cast(), value.len()) };
}

unsafe fn push_malloc_bytes(l: *mut lua_State, value: &[u8]) -> bool {
    // SAFETY: `malloc` is matched by the caller's `free` (KOReader ffi.C.free).
    unsafe {
        let buffer = libc::malloc(value.len().max(1)).cast::<u8>();
        if buffer.is_null() {
            return false;
        }
        ptr::copy_nonoverlapping(value.as_ptr(), buffer, value.len());
        lua::lua_pushlightuserdata(l, buffer.cast());
        push_int(l, value.len() as i64);
    }
    true
}

unsafe fn arg_str(l: *mut lua_State, index: c_int) -> Option<String> {
    let mut len = 0;
    // SAFETY: `index` is a valid Lua stack index for this call.
    let ptr = unsafe { lua::lua_tolstring(l, index, &mut len) };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: Lua guarantees `ptr` points to `len` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
    Some(String::from_utf8_lossy(bytes).into_owned())
}

unsafe fn arg_bytes(l: *mut lua_State, index: c_int) -> Option<Vec<u8>> {
    let mut len = 0;
    // SAFETY: `index` is a valid Lua stack index for this call.
    let ptr = unsafe { lua::lua_tolstring(l, index, &mut len) };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: Lua guarantees `ptr` points to `len` readable bytes.
    Some(unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) }.to_vec())
}

#[allow(clippy::useless_conversion)]
unsafe fn arg_int(l: *mut lua_State, index: c_int) -> i64 {
    // SAFETY: `index` is a valid Lua stack index for this call.
    let raw = unsafe { lua::lua_tointeger_(l, index) };
    i64::try_from(raw).unwrap_or(0)
}

unsafe fn arg_bool(l: *mut lua_State, index: c_int) -> bool {
    // SAFETY: `index` is a valid Lua stack index for this call.
    unsafe { lua::lua_toboolean(l, index) != 0 }
}

unsafe fn push_image(l: *mut lua_State, value: CreImage) {
    // SAFETY: `l` is the active Lua state; the userdata is initialized below and
    // is owned by Lua once its metatable is installed.
    unsafe {
        let ptr = lua::lua_newuserdata(l, std::mem::size_of::<CreImage>()).cast::<CreImage>();
        ptr::write(ptr, value);
        lua::luaL_getmetatable(l, CREIMAGE.as_ptr().cast());
        lua::lua_setmetatable(l, -2);
    }
}

unsafe fn image_mut<'a>(l: *mut lua_State, index: c_int) -> Option<&'a mut CreImage> {
    // SAFETY: `l` is the active Lua state; the caller supplies a valid stack index.
    unsafe {
        let ptr = lua::lua_touserdata(l, index).cast::<CreImage>();
        if ptr.is_null() { None } else { Some(&mut *ptr) }
    }
}

unsafe fn push_bool(l: *mut lua_State, value: bool) {
    // SAFETY: Lua takes the boolean by value.
    unsafe { lua::lua_pushboolean(l, c_int::from(value)) };
}

unsafe fn push_int(l: *mut lua_State, value: i64) {
    // SAFETY: Lua takes the integer by value.
    let value = lua::lua_Integer::try_from(value).unwrap_or(lua::lua_Integer::MAX);
    unsafe { lua::lua_pushinteger(l, value) };
}

unsafe fn copy_page_bitmap_to_blit(bitmap: &cre_lib::PageBitmap, color: bool, bb: &BlitBuffer) {
    let rows = bitmap.height.min(bb.h) as usize;
    let cols = bitmap.width.min(bb.w) as usize;
    let dst =
        unsafe { std::slice::from_raw_parts_mut(bb.data, bb.stride.saturating_mul(bb.h as usize)) };
    if color {
        for y in 0..rows {
            let src = &bitmap.data[y * bitmap.width as usize..];
            let dst_row = &mut dst[y * bb.stride..];
            for (x, &value) in src.iter().take(cols).enumerate() {
                let offset = x * 4;
                dst_row[offset] = value;
                dst_row[offset + 1] = value;
                dst_row[offset + 2] = value;
                dst_row[offset + 3] = 0xFF;
            }
        }
    } else {
        for y in 0..rows {
            let src = &bitmap.data[y * bitmap.width as usize..];
            let dst_row = &mut dst[y * bb.stride..];
            dst_row[..cols].copy_from_slice(&src[..cols]);
        }
    }
}

unsafe fn copy_color_page_bitmap_to_blit(bitmap: &cre_lib::ColorPageBitmap, bb: &BlitBuffer) {
    let rows = bitmap.height.min(bb.h) as usize;
    let cols = bitmap.width.min(bb.w) as usize;
    let dst =
        unsafe { std::slice::from_raw_parts_mut(bb.data, bb.stride.saturating_mul(bb.h as usize)) };
    for y in 0..rows {
        let src = &bitmap.data[y * bitmap.width as usize * 4..];
        let dst_row = &mut dst[y * bb.stride..];
        for x in 0..cols {
            let src_offset = x * 4;
            for channel in 0..3 {
                if let Some(value) = src.get(src_offset + channel) {
                    dst_row[x * 4 + channel] = *value;
                }
            }
            dst_row[x * 4 + 3] = 0xFF;
        }
    }
}

unsafe fn format_name(format: DocumentFormat) -> &'static str {
    match format {
        DocumentFormat::Epub => "EPUB",
        DocumentFormat::Fb2 => "FB2",
        DocumentFormat::Txt => "TXT",
        DocumentFormat::Html => "HTML",
    }
}

unsafe extern "C-unwind" fn stub(_l: *mut lua_State) -> c_int {
    0
}

unsafe extern "C-unwind" fn module_init_cache(_l: *mut lua_State) -> c_int {
    0
}

unsafe extern "C-unwind" fn module_init_hyph_dict(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let path = arg_str(l, 1).unwrap_or_default();
        if let Ok(mut state) = font_dir_state().lock() {
            *state = path;
        }
    }
    0
}

unsafe extern "C-unwind" fn module_get_font_faces(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let families = font_families();
        lua::lua_createtable(l, families.len() as c_int, 0);
        for (index, family) in families.iter().enumerate() {
            push_str(l, family);
            lua::lua_rawseti_(l, -2, (index + 1) as c_int);
        }
    }
    1
}

unsafe extern "C-unwind" fn module_font_face_filename(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(name) = arg_str(l, 1) else {
            return 0;
        };
        let bold = lua::lua_isboolean(l, 2) != 0 && arg_bool(l, 2);
        let italic = lua::lua_isboolean(l, 3) != 0 && arg_bool(l, 3);
        let Some(face) = find_font_face(&name, bold, italic) else {
            return 0;
        };
        push_str(l, &face.path.to_string_lossy());
        push_int(l, i64::from(face.index));
        push_bool(l, face.monospace);
        push_bool(l, face.math);
        push_bool(l, face.color || face.bitmap);
    }
    5
}

unsafe extern "C-unwind" fn module_font_face_weights(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(name) = arg_str(l, 1) else {
            return 0;
        };
        let faces = registered_font_faces();
        let Some(anchor) = select_font_face(&faces, &name, false, false) else {
            lua::lua_newtable(l);
            return 1;
        };
        let family = normalize_face_name(&anchor.family);
        let mut weights = faces
            .iter()
            .filter(|face| normalize_face_name(&face.family) == family)
            .map(|face| face.weight)
            .collect::<Vec<_>>();
        weights.sort_unstable();
        weights.dedup();
        lua::lua_createtable(l, weights.len() as c_int, 0);
        for (index, weight) in weights.into_iter().enumerate() {
            push_int(l, i64::from(weight));
            lua::lua_rawseti_(l, -2, (index + 1) as c_int);
        }
    }
    1
}

unsafe extern "C-unwind" fn module_get_gamma_level(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        lua::lua_pushnumber(
            l,
            gamma_level_for_index(GAMMA_INDEX.load(Ordering::Relaxed)),
        )
    };
    1
}

unsafe extern "C-unwind" fn module_get_gamma_index(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { push_int(l, i64::from(GAMMA_INDEX.load(Ordering::Relaxed))) };
    1
}

unsafe extern "C-unwind" fn module_set_gamma_index(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let index = arg_int(l, 1).clamp(0, 56) as i32;
        GAMMA_INDEX.store(index, Ordering::Relaxed);
    }
    0
}

unsafe extern "C-unwind" fn module_register_font(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(path) = arg_str(l, 1) else {
            return 0;
        };
        let path = PathBuf::from(&path);
        let faces = parse_font_metadata(&path);
        if faces.is_empty() {
            return 0;
        }
        let registered = REGISTERED_FONTS.get_or_init(|| Mutex::new(Vec::new()));
        if let Ok(mut fonts) = registered.lock() {
            for face in faces {
                if !fonts
                    .iter()
                    .any(|font| font.path == face.path && font.index == face.index)
                {
                    fonts.push(face);
                }
            }
        }
    }
    0
}

fn regularize_registered_font_weights(print_updates: bool) {
    let registered = REGISTERED_FONTS.get_or_init(|| Mutex::new(Vec::new()));
    let mut fonts = match registered.lock() {
        Ok(fonts) => fonts,
        Err(_) => return,
    };
    if fonts.is_empty() {
        return;
    }
    let snapshot = fonts.clone();
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, face) in snapshot.iter().enumerate() {
        let key = normalize_face_name(&face.family);
        if key.is_empty() {
            continue;
        }
        groups.entry(key).or_default().push(index);
    }
    let mut updates: Vec<(usize, i32, i32, String, String)> = Vec::new();
    for (family, indexes_ref) in groups.iter() {
        let indexes: Vec<usize> = indexes_ref.to_vec();
        for italic in [false, true] {
            let regular_index: Option<usize> = {
                let mut found: Option<usize> = None;
                let mut best_cost = i32::MAX;
                for index_ref in &indexes {
                    let index = *index_ref;
                    let face = &snapshot[index];
                    if face.italic != italic || face.weight >= 700 {
                        continue;
                    }
                    let cost = (face.weight - 401).abs();
                    if cost < best_cost {
                        found = Some(index);
                        best_cost = cost;
                    }
                }
                found
            };
            if let Some(index) = regular_index {
                let face = &snapshot[index];
                if face.weight != 400 {
                    updates.push((
                        index,
                        face.weight,
                        400,
                        family.clone(),
                        "regular".to_string(),
                    ));
                }
            }
            let bold_index: Option<usize> = {
                let mut found: Option<usize> = None;
                let mut best_weight = i32::MAX;
                for index_ref in &indexes {
                    let index = *index_ref;
                    let face = &snapshot[index];
                    if face.italic != italic || face.weight < 700 {
                        continue;
                    }
                    if face.weight < best_weight {
                        found = Some(index);
                        best_weight = face.weight;
                    }
                }
                found
            };
            if let Some(index) = bold_index {
                let face = &snapshot[index];
                if face.weight != 700 {
                    updates.push((index, face.weight, 700, family.clone(), "bold".to_string()));
                }
            }
        }
    }
    if updates.is_empty() {
        return;
    }
    if print_updates {
        for (_, old_weight, new_weight, family, label) in updates.iter() {
            println!(
                "CRE: font {} {}: updated weight from {} to {}",
                family, label, old_weight, new_weight,
            );
        }
    }
    for (index, _, new_weight, _, _) in updates {
        fonts[index].weight = new_weight;
    }
    sort_font_faces(fonts.as_mut());
}

unsafe extern "C-unwind" fn module_regularize_registered_font_weights(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let print_updates = lua::lua_isboolean(l, 1) != 0 && arg_bool(l, 1);
        regularize_registered_font_weights(print_updates);
    }
    0
}

unsafe extern "C-unwind" fn module_set_preferred_font(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(face) = arg_str(l, 1) else {
            return 0;
        };
        let bias = arg_int(l, 2).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        let clear_other_bias = if lua::lua_isboolean(l, 3) != 0 {
            arg_bool(l, 3)
        } else {
            true
        };
        let normalized = normalize_face_name(&face);
        let state = FONT_BIAS.get_or_init(|| Mutex::new(HashMap::new()));
        if let Ok(mut biases) = state.lock() {
            if clear_other_bias {
                biases.clear();
            }
            if bias == 0 {
                biases.remove(&normalized);
            } else {
                biases.insert(normalized, bias);
            }
        }
    }
    0
}

unsafe extern "C-unwind" fn module_get_hyph_dict_list(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { lua::lua_newtable(l) };
    1
}

unsafe extern "C-unwind" fn module_get_selected_hyph_dict(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let dict = hyph_state()
            .lock()
            .map(|state| state.clone())
            .unwrap_or_default();
        push_str(l, &dict);
        push_int(l, 2);
        push_int(l, 2);
    }
    3
}

unsafe extern "C-unwind" fn module_set_hyph_dict(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let dict = arg_str(l, 1).unwrap_or_default();
        if let Ok(mut state) = hyph_state().lock() {
            *state = dict;
        }
    }
    0
}

unsafe extern "C-unwind" fn module_get_text_lang_status(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let dict = hyph_state()
            .lock()
            .map(|state| state.clone())
            .unwrap_or_default();
        push_str(l, "en");
        push_str(l, &dict);
        lua::lua_createtable(l, 0, 1);
        push_str(l, "en");
        lua::lua_createtable(l, 0, 3);
        push_str(l, "hyph_dict_name");
        push_str(l, &dict);
        lua::lua_rawset(l, -3);
        push_str(l, "lang_tag");
        push_str(l, "en");
        lua::lua_rawset(l, -3);
        push_str(l, "hyph_left_min");
        push_int(l, 2);
        lua::lua_rawset(l, -3);
        lua::lua_rawset(l, -3);
    }
    3
}

unsafe extern "C-unwind" fn module_dom_version(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { push_int(l, 1) };
    1
}

unsafe extern "C-unwind" fn module_set_user_hyph(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(filename) = arg_str(l, 1) else {
            return 0;
        };
        let ok = Path::new(&filename).exists();
        if ok && let Ok(mut files) = user_hyph_state().lock() {
            files.insert(filename);
        }
        push_int(l, i64::from(ok));
    }
    1
}

unsafe extern "C-unwind" fn module_get_hyphenation(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let word = arg_str(l, 1).unwrap_or_default();
        push_str(l, &word);
    }
    1
}

fn soft_hyphenate(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_inclusive(char::is_whitespace) {
        let chars = word.chars().collect::<Vec<_>>();
        let letters = chars.iter().filter(|ch| ch.is_alphabetic()).count();
        if letters < 6 {
            out.push_str(word);
            continue;
        }
        let mut letters_seen = 0usize;
        for ch in chars {
            out.push(ch);
            if ch.is_alphabetic() {
                letters_seen += 1;
                if letters_seen.is_multiple_of(3) && letters_seen + 2 < letters {
                    out.push('\u{00AD}');
                }
            }
        }
    }
    out
}

unsafe extern "C-unwind" fn module_soft_hyphenate(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let text = arg_str(l, 2).unwrap_or_default();
        push_str(l, &soft_hyphenate(&text));
    }
    1
}

unsafe extern "C-unwind" fn module_render_image_data(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state; the image data comes from Lua and is
    // either a lightuserdata buffer with an explicit size or a string.
    unsafe {
        let data = if lua::lua_islightuserdata(l, 1) != 0 {
            let size = arg_int(l, 2).max(0) as usize;
            let ptr = lua::lua_touserdata(l, 1).cast::<u8>();
            if ptr.is_null() {
                return 0;
            }
            std::slice::from_raw_parts(ptr, size).to_vec()
        } else if let Some(bytes) = arg_bytes(l, 1) {
            bytes
        } else {
            return 0;
        };
        let target_width = (lua::lua_isnumber(l, 3) != 0).then(|| arg_int(l, 3).max(0) as u32);
        let target_height = (lua::lua_isnumber(l, 4) != 0).then(|| arg_int(l, 4).max(0) as u32);
        let Some((pixels, width, height)) =
            cre_lib::render_svg_image(&data, target_width, target_height)
        else {
            return 0;
        };
        let buffer = libc::malloc(pixels.len().max(1)).cast::<u8>();
        if buffer.is_null() {
            return 0;
        }
        ptr::copy_nonoverlapping(pixels.as_ptr(), buffer, pixels.len());
        lua::lua_pushlightuserdata(l, buffer.cast());
        push_int(l, i64::from(width));
        push_int(l, i64::from(height));
    }
    3
}

unsafe extern "C-unwind" fn image_render_scaled(l: *mut lua_State) -> c_int {
    let rendered = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `l` is the active Lua state; index 1 is checked below.
        unsafe {
            let image = image_mut(l, 1)?.image.as_ref()?.clone();
            let (data, width, height) = image;
            if width == 0 || height == 0 {
                return None;
            }
            let scale = if lua::lua_isnumber(l, 2) != 0 {
                lua::lua_tonumber(l, 2)
            } else {
                1.0
            };
            let scale = if scale <= 0.0 {
                let request_width = arg_int(l, 3).max(1) as f64;
                let request_height = arg_int(l, 4).max(1) as f64;
                (request_width / f64::from(width)).min(request_height / f64::from(height))
            } else {
                scale
            };
            if !scale.is_finite() || scale <= 0.0 {
                return None;
            }
            let target_width = (f64::from(width) * scale).clamp(1.0, f64::from(u32::MAX)) as u32;
            let target_height = (f64::from(height) * scale).clamp(1.0, f64::from(u32::MAX)) as u32;
            let (pixels, width, height) =
                cre_lib::render_svg_image(&data, Some(target_width), Some(target_height))?;
            let buffer = libc::malloc(pixels.len().max(1)).cast::<u8>();
            if buffer.is_null() {
                return None;
            }
            ptr::copy_nonoverlapping(pixels.as_ptr(), buffer, pixels.len());
            Some((buffer, width, height, scale))
        }
    }));
    match rendered {
        Ok(Some((buffer, width, height, scale))) => {
            // SAFETY: `l` is the active Lua state and the buffer is malloc-owned.
            unsafe {
                lua::lua_pushlightuserdata(l, buffer.cast());
                push_int(l, i64::from(width));
                push_int(l, i64::from(height));
                lua::lua_pushnumber(l, scale);
            }
            4
        }
        _ => 0,
    }
}

unsafe extern "C-unwind" fn image_free(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state; index 1 is checked below.
    unsafe {
        if let Some(image) = image_mut(l, 1) {
            image.free();
        }
    }
    0
}

unsafe extern "C-unwind" fn image_gc(l: *mut lua_State) -> c_int {
    // SAFETY: `__gc` receives a userdata initialized by `push_image`.
    unsafe {
        let ptr = lua::lua_touserdata(l, 1).cast::<CreImage>();
        if !ptr.is_null() {
            ptr::drop_in_place(ptr);
        }
    }
    0
}

unsafe extern "C-unwind" fn module_get_balanced_html(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(html) = arg_str(l, 1) else {
            return 0;
        };
        push_str(l, &html);
    }
    1
}

unsafe extern "C-unwind" fn module_smooth_scale(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state; the BlitBuffer comes from Lua cdata.
    unsafe {
        let bb = lua::lua_topointer(l, 1).cast::<BlitBuffer>();
        let dw = arg_int(l, 2).clamp(1, 16_384) as u32;
        let dh = arg_int(l, 3).clamp(1, 16_384) as u32;
        if bb.is_null() || (*bb).data.is_null() || (*bb).w == 0 || (*bb).h == 0 {
            return 0;
        }
        let bb = &*bb;
        let width = bb.w;
        let height = bb.h;
        let stride = bb.stride;
        let source = std::slice::from_raw_parts(bb.data, stride.saturating_mul(height as usize));
        let size = (dw as usize) * (dh as usize) * 4;
        let buffer = libc::malloc(size).cast::<u8>();
        if buffer.is_null() {
            return 0;
        }
        let output = std::slice::from_raw_parts_mut(buffer, size);
        for y in 0..dh {
            let src_y = if dh > 1 {
                (y as f64) * ((height - 1) as f64) / ((dh - 1) as f64)
            } else {
                0.0
            };
            for x in 0..dw {
                let src_x = if dw > 1 {
                    (x as f64) * ((width - 1) as f64) / ((dw - 1) as f64)
                } else {
                    0.0
                };
                let sample = |sx: u32, sy: u32| -> [u8; 4] {
                    let offset = sy as usize * stride + sx as usize * 4;
                    [
                        source.get(offset).copied().unwrap_or(255),
                        source.get(offset + 1).copied().unwrap_or(255),
                        source.get(offset + 2).copied().unwrap_or(255),
                        source.get(offset + 3).copied().unwrap_or(255),
                    ]
                };
                let x0 = src_x.floor() as u32;
                let y0 = src_y.floor() as u32;
                let x1 = (x0 + 1).min(width - 1);
                let y1 = (y0 + 1).min(height - 1);
                let fx = (src_x - x0 as f64) as f32;
                let fy = (src_y - y0 as f64) as f32;
                let a = sample(x0, y0);
                let b = sample(x1, y0);
                let c = sample(x0, y1);
                let d = sample(x1, y1);
                let offset = (y as usize * dw as usize + x as usize) * 4;
                for channel in 0..4 {
                    let top = a[channel] as f32 * (1.0 - fx) + b[channel] as f32 * fx;
                    let bottom = c[channel] as f32 * (1.0 - fx) + d[channel] as f32 * fx;
                    output[offset + channel] =
                        (top * (1.0 - fy) + bottom * fy).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
        lua::lua_pushlightuserdata(l, buffer.cast());
        push_int(l, size as i64);
    }
    2
}

unsafe extern "C-unwind" fn module_set_image_replacement_char(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let value = arg_int(l, 1).clamp(0, i64::from(u32::MAX)) as u32;
        IMAGE_REPLACEMENT_CHAR.store(value, Ordering::Relaxed);
    }
    0
}

unsafe extern "C-unwind" fn module_noop(_l: *mut lua_State) -> c_int {
    0
}

unsafe extern "C-unwind" fn new_doc_view(l: *mut lua_State) -> c_int {
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `l` is the active Lua state.
        unsafe {
            let width = arg_int(l, 1).max(1) as u32;
            let height = arg_int(l, 2).max(1) as u32;
            let view_mode = arg_int(l, 3) as i32;
            let ptr = lua::lua_newuserdata(l, std::mem::size_of::<Doc>()).cast::<Doc>();
            ptr::write(ptr, Doc::new(width, height, view_mode));
            lua::luaL_getmetatable(l, CREDOC.as_ptr().cast());
            lua::lua_setmetatable(l, -2);
        }
        1
    }));
    result.unwrap_or_else(|_| {
        // SAFETY: push nil to signal failure.
        unsafe { lua::lua_pushnil(l) };
        1
    })
}

unsafe extern "C-unwind" fn doc_gc(l: *mut lua_State) -> c_int {
    // SAFETY: `__gc` receives a userdata initialized by `new_doc_view`.
    unsafe {
        let ptr = lua::lua_touserdata(l, 1).cast::<Doc>();
        if !ptr.is_null() {
            if let Some(reference) = (*ptr).callback {
                lua::luaL_unref(l, lua::LUA_REGISTRYINDEX, reference);
            }
            (*ptr).callback = None;
            ptr::drop_in_place(ptr);
        }
    }
    0
}

unsafe fn call_document_callback(l: *mut lua_State, event: &str) {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(reference) = doc_ref(l).and_then(|doc| doc.callback) else {
            return;
        };
        lua::lua_rawgeti(l, lua::LUA_REGISTRYINDEX, lua::lua_Integer::from(reference));
        push_str(l, event);
        if lua::lua_pcall(l, 1, 0, 0) != 0 {
            lua::lua_pop(l, 1);
        }
    }
}

unsafe fn call_document_callback_str(l: *mut lua_State, event: &str, value: &str) {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(reference) = doc_ref(l).and_then(|doc| doc.callback) else {
            return;
        };
        lua::lua_rawgeti(l, lua::LUA_REGISTRYINDEX, lua::lua_Integer::from(reference));
        push_str(l, event);
        push_str(l, value);
        if lua::lua_pcall(l, 2, 0, 0) != 0 {
            lua::lua_pop(l, 1);
        }
    }
}

unsafe fn call_document_callback_int(l: *mut lua_State, event: &str, value: i64) {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(reference) = doc_ref(l).and_then(|doc| doc.callback) else {
            return;
        };
        lua::lua_rawgeti(l, lua::LUA_REGISTRYINDEX, lua::lua_Integer::from(reference));
        push_str(l, event);
        push_int(l, value);
        if lua::lua_pcall(l, 2, 0, 0) != 0 {
            lua::lua_pop(l, 1);
        }
    }
}

unsafe extern "C-unwind" fn doc_load(l: *mut lua_State) -> c_int {
    let loaded = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `l` is the active Lua state.
        unsafe {
            let Some(doc) = doc_mut(l) else {
                return false;
            };
            let Some(path) = arg_str(l, 2) else {
                return false;
            };
            call_document_callback_str(l, "OnLoadFileStart", &path);
            let opened = doc.open(PathBuf::from(path));
            if opened.is_ok() {
                doc.record_history();
                let format = doc_ref(l)
                    .and_then(|doc| doc.inner.as_ref())
                    .map(|inner| format_name(inner.metadata().format))
                    .unwrap_or("TXT");
                call_document_callback_str(l, "OnLoadFileFormatDetected", format);
                call_document_callback_int(l, "OnLoadFileProgress", 100);
                call_document_callback(l, "OnLoadFileEnd");
                call_document_callback(l, "OnFormatStart");
                call_document_callback(l, "OnFormatEnd");
                call_document_callback(l, "OnDocumentReady");
                true
            } else {
                let message = opened
                    .err()
                    .map(|err| err.to_string())
                    .unwrap_or_else(|| "document load failed".to_string());
                call_document_callback_str(l, "OnLoadFileError", &message);
                false
            }
        }
    }))
    .unwrap_or(false);
    // SAFETY: `l` is the active Lua state.
    unsafe { push_bool(l, loaded) };
    1
}

unsafe extern "C-unwind" fn doc_close(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            if let Some(reference) = doc.callback {
                lua::luaL_unref(l, lua::LUA_REGISTRYINDEX, reference);
            }
            doc.callback = None;
            doc.inner = None;
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_render(l: *mut lua_State) -> c_int {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `l` is the active Lua state.
        unsafe {
            if let Some(doc) = doc_mut(l)
                && let Some(inner) = doc.inner.as_mut()
            {
                let _ = inner.render_current_page();
            }
        }
    }));
    0
}

unsafe extern "C-unwind" fn doc_format(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let name = doc_ref(l)
            .and_then(|doc| doc.inner.as_ref())
            .map(|inner| format_name(inner.metadata().format))
            .unwrap_or("TXT");
        push_str(l, name);
    }
    1
}

unsafe extern "C-unwind" fn doc_props(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        lua::lua_createtable(l, 0, 7);
        let mut props = [
            ("title", ""),
            ("authors", ""),
            ("language", ""),
            ("series", ""),
            ("description", ""),
            ("keywords", ""),
            ("identifiers", ""),
        ];
        if let Some(inner) = doc_ref(l).and_then(|doc| doc.inner.as_ref()) {
            let meta = inner.metadata();
            props[0].1 = meta.title.as_deref().unwrap_or("");
            props[1].1 = meta.creator.as_deref().unwrap_or("");
            props[2].1 = meta.language.as_deref().unwrap_or("");
            props[3].1 = meta.series.as_deref().unwrap_or("");
            props[4].1 = meta.description.as_deref().unwrap_or("");
            props[5].1 = meta.keywords.as_deref().unwrap_or("");
            props[6].1 = meta.identifiers.as_deref().unwrap_or("");
        }
        for (key, value) in props {
            push_str(l, key);
            push_str(l, value);
            lua::lua_rawset(l, -3);
        }
    }
    1
}

unsafe extern "C-unwind" fn doc_pages(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let pages = doc_ref(l).map_or(0, |doc| doc.page_count());
        push_int(l, pages as i64);
    }
    1
}

unsafe extern "C-unwind" fn doc_current_page(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let page = doc_ref(l).map_or(1, |doc| doc.current_page());
        push_int(l, page as i64);
    }
    1
}

unsafe extern "C-unwind" fn doc_goto_page(l: *mut lua_State) -> c_int {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `l` is the active Lua state.
        unsafe {
            let Some(doc) = doc_mut(l) else {
                return;
            };
            let Some(inner) = doc.inner.as_mut() else {
                return;
            };
            let page = arg_int(l, 2).max(1) as usize;
            let _ = inner.go_to_page(page);
        }
    }));
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            doc.record_history();
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_goto_percent(l: *mut lua_State) -> c_int {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `l` is the active Lua state.
        unsafe {
            let Some(doc) = doc_mut(l) else {
                return;
            };
            let Some(inner) = doc.inner.as_mut() else {
                return;
            };
            if inner.page_count() == 0 {
                return;
            }
            let percent = arg_int(l, 2).clamp(0, 10_000);
            let full_height = i64::try_from(inner.full_height()).unwrap_or(i64::MAX);
            let pos = percent.saturating_mul(full_height) / 10_000;
            let _ = inner.set_pos(pos);
        }
    }));
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            doc.record_history();
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_goto_pos(l: *mut lua_State) -> c_int {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `l` is the active Lua state.
        unsafe {
            let Some(doc) = doc_mut(l) else {
                return;
            };
            let Some(inner) = doc.inner.as_mut() else {
                return;
            };
            if inner.page_count() == 0 {
                return;
            }
            let pos = arg_int(l, 2).max(0);
            let _ = inner.set_pos(pos);
        }
    }));
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            doc.record_history();
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_goto_xpointer(l: *mut lua_State) -> c_int {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `l` is the active Lua state.
        unsafe {
            let Some(doc) = doc_mut(l) else {
                return;
            };
            let Some(xpointer) = arg_str(l, 2) else {
                return;
            };
            if let Some(inner) = doc.inner.as_mut() {
                let _ = inner.go_to_xpointer(&xpointer);
            }
        }
    }));
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            doc.record_history();
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_xpointer(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let xpointer = doc_ref(l)
            .and_then(|doc| doc.inner.as_ref())
            .and_then(|inner| inner.xpointer_from_page(inner.current_page()).ok())
            .unwrap_or_default();
        push_str(l, &xpointer);
    }
    1
}

unsafe extern "C-unwind" fn doc_page_xpointer(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let page = arg_int(l, 2).max(1) as usize;
        let xpointer = doc_ref(l)
            .and_then(|doc| doc.inner.as_ref())
            .and_then(|inner| inner.xpointer_from_page(page).ok())
            .unwrap_or_default();
        push_str(l, &xpointer);
    }
    1
}

unsafe extern "C-unwind" fn doc_page_from_xpointer(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let page = arg_str(l, 2)
            .and_then(|xp| {
                doc_ref(l)
                    .and_then(|doc| doc.inner.as_ref())
                    .and_then(|inner| inner.page_from_xpointer(&xp).ok())
            })
            .unwrap_or(1);
        push_int(l, page as i64);
    }
    1
}

unsafe extern "C-unwind" fn doc_xpointer_in_document(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let found = arg_str(l, 2).is_some_and(|xp| {
            doc_ref(l)
                .and_then(|doc| doc.inner.as_ref())
                .and_then(|inner| inner.page_from_xpointer(&xp).ok())
                .is_some()
        });
        push_bool(l, found);
    }
    1
}

unsafe extern "C-unwind" fn doc_xpointer_in_current_page(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let found = arg_str(l, 2).is_some_and(|xp| {
            doc_ref(l)
                .and_then(|doc| doc.inner.as_ref())
                .is_some_and(|inner| {
                    inner
                        .page_from_xpointer(&xp)
                        .ok()
                        .is_some_and(|page| page == inner.current_page())
                })
        });
        push_bool(l, found);
    }
    1
}

unsafe extern "C-unwind" fn doc_pos_from_xpointer(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(xp) = arg_str(l, 2) else {
            push_int(l, 0);
            push_int(l, 0);
            return 2;
        };
        let Some(inner) = doc_ref(l).and_then(|doc| doc.inner.as_ref()) else {
            push_int(l, 0);
            push_int(l, 0);
            return 2;
        };
        let mut page = inner.page_from_xpointer(&xp).ok();
        let mut point = inner.point_from_xpointer(&xp).ok().flatten();
        if page.is_none() || point.is_none() {
            let next = inner
                .next_visible_char(&xp)
                .ok()
                .flatten()
                .filter(|next| *next != xp);
            if let Some(next) = next {
                if page.is_none() {
                    page = inner.page_from_xpointer(&next).ok();
                }
                if point.is_none() {
                    point = inner.point_from_xpointer(&next).ok().flatten();
                }
            }
        }
        let Some(page) = page else {
            push_int(l, 0);
            push_int(l, 0);
            return 2;
        };
        let Some((x, y)) = point else {
            push_int(l, 0);
            push_int(l, 0);
            return 2;
        };
        push_int(l, inner.page_start_y(page) + y.round() as i64);
        push_int(l, x.round() as i64);
    }
    2
}

unsafe extern "C-unwind" fn doc_current_pos(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let pos = doc_ref(l)
            .and_then(|doc| doc.inner.as_ref())
            .map(current_document_pos)
            .unwrap_or(0);
        push_int(l, pos);
    }
    1
}

unsafe extern "C-unwind" fn doc_current_percent(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let percent = doc_ref(l)
            .and_then(|doc| doc.inner.as_ref())
            .map_or(0, |inner| {
                let full_height = i64::try_from(inner.full_height()).unwrap_or(i64::MAX);
                if full_height == 0 {
                    0
                } else {
                    (current_document_pos(inner) * 10_000 / full_height).clamp(0, 9_999)
                }
            });
        push_int(l, percent);
    }
    1
}

unsafe extern "C-unwind" fn doc_full_height(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let height = doc_ref(l)
            .and_then(|doc| doc.inner.as_ref().map(|inner| inner.full_height() as i64))
            .unwrap_or(0);
        push_int(l, height);
    }
    1
}

unsafe extern "C-unwind" fn doc_page_start_y(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let page = arg_int(l, 2);
        let y = doc_ref(l)
            .and_then(|doc| doc.inner.as_ref())
            .map(|inner| inner.page_start_y_or_invalid(usize::try_from(page).unwrap_or(0)))
            .unwrap_or(-1);
        push_int(l, y);
    }
    1
}

unsafe extern "C-unwind" fn doc_page_height(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let page = arg_int(l, 2);
        let height = doc_ref(l)
            .and_then(|doc| doc.inner.as_ref())
            .map(|inner| {
                let page_idx = usize::try_from(page).unwrap_or(0);
                if page_idx < 1 {
                    -1
                } else {
                    inner.page_render_height(page_idx)
                }
            })
            .unwrap_or(-1);
        push_int(l, height);
    }
    1
}

unsafe extern "C-unwind" fn doc_font_size(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let size = doc_ref(l).map_or(22, |doc| doc.effective_font_size());
        push_int(l, i64::from(size));
    }
    1
}

unsafe extern "C-unwind" fn doc_font_face(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let face = doc_ref(l).map_or("", |doc| doc.font_face.as_str());
        push_str(l, face);
    }
    1
}

unsafe extern "C-unwind" fn doc_page_margins(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let (left, top, right, bottom) = doc_ref(l).map_or((0, 0, 0, 0), |doc| doc.margins);
        lua::lua_createtable(l, 0, 4);
        for (key, value) in [
            ("left", left),
            ("top", top),
            ("right", right),
            ("bottom", bottom),
        ] {
            push_str(l, key);
            push_int(l, i64::from(value));
            lua::lua_rawset(l, -3);
        }
    }
    1
}

unsafe extern "C-unwind" fn doc_toc(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(inner) = doc_ref(l).and_then(|doc| doc.inner.as_ref()) else {
            lua::lua_newtable(l);
            return 1;
        };
        lua::lua_createtable(l, inner.toc().len() as c_int, 0);
        for (index, item) in inner.toc().iter().enumerate() {
            lua::lua_createtable(l, 0, 4);
            push_str(l, "page");
            push_int(l, item.page.unwrap_or(0) as i64);
            lua::lua_rawset(l, -3);
            push_str(l, "xpointer");
            push_str(l, item.xpointer.as_deref().unwrap_or_default());
            lua::lua_rawset(l, -3);
            push_str(l, "depth");
            push_int(l, item.depth as i64);
            lua::lua_rawset(l, -3);
            push_str(l, "title");
            push_str(l, &item.title);
            lua::lua_rawset(l, -3);
            lua::lua_rawseti_(l, -2, (index + 1) as c_int);
        }
    }
    1
}

unsafe extern "C-unwind" fn doc_draw_current_page(l: *mut lua_State) -> c_int {
    let image_stats = catch_unwind(AssertUnwindSafe(|| -> (usize, f64) {
        // SAFETY: `l` is the active Lua state. The BlitBuffer pointer is supplied by KOReader
        // through `lua_topointer`, matching cre.cpp.
        unsafe {
            let Some(doc) = doc_mut(l) else {
                return (0, 0.0);
            };
            let gamma = doc.render_font_gamma();
            let Some(inner) = doc.inner.as_mut() else {
                return (0, 0.0);
            };
            let bb = lua::lua_topointer(l, 2).cast::<BlitBuffer>();
            if bb.is_null() || (*bb).data.is_null() {
                return (0, 0.0);
            }
            let color = arg_bool(l, 3);
            let highlight = doc.highlight.clone();
            let current_page = inner.current_page();
            inner.set_font_gamma(gamma);
            if color {
                let Ok((image_arc, image_count, image_surface)) =
                    inner.render_current_page_color_with_stats()
                else {
                    return (0, 0.0);
                };
                let total_pixels = image_arc.width.saturating_mul(image_arc.height).max(1);
                let stats = (image_count, image_surface as f64 / total_pixels as f64);
                let bb = &*bb;
                if let Some((start, end)) = highlight
                    && let Ok(boxes) = inner.boxes_between_xpointers(&start, &end)
                {
                    let mut bitmap = image_arc.as_ref().clone();
                    apply_color_text_highlight(&mut bitmap, &boxes, current_page);
                    copy_color_page_bitmap_to_blit(&bitmap, bb);
                } else {
                    copy_color_page_bitmap_to_blit(image_arc.as_ref(), bb);
                }
                stats
            } else {
                let Ok((bitmap_arc, image_count, image_surface)) =
                    inner.render_current_page_with_stats()
                else {
                    return (0, 0.0);
                };
                let total_pixels = bitmap_arc
                    .width
                    .saturating_mul(bitmap_arc.height)
                    .saturating_mul(1)
                    .max(1);
                let stats = (image_count, image_surface as f64 / total_pixels as f64);
                let bb = &*bb;
                if let Some((start, end)) = highlight
                    && let Ok(boxes) = inner.boxes_between_xpointers(&start, &end)
                {
                    let mut bitmap = bitmap_arc.as_ref().clone();
                    apply_text_highlight(&mut bitmap, &boxes, current_page);
                    copy_page_bitmap_to_blit(&bitmap, false, bb);
                } else {
                    copy_page_bitmap_to_blit(bitmap_arc.as_ref(), false, bb);
                }
                stats
            }
        }
    }));
    let (image_count, image_fraction) = image_stats.unwrap_or((0, 0.0));
    // SAFETY: `l` is the active Lua state.
    unsafe {
        push_int(l, i64::min(i64::MAX, image_count as i64));
        lua::lua_pushnumber(l, image_fraction);
    }
    2
}

unsafe extern "C-unwind" fn doc_find_text(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(query) = arg_str(l, 2) else {
            return 0;
        };
        let origin = arg_int(l, 3) as i32;
        let reverse = arg_bool(l, 4);
        let case_insensitive = arg_bool(l, 5);
        let pattern_is_regex = arg_bool(l, 6);
        let requested = arg_int(l, 7);
        let search_flags = arg_int(l, 8) as i32;
        let Some(inner) = doc_ref(l).and_then(|doc| doc.inner.as_ref()) else {
            return 0;
        };
        let Some(hits) = find_text_document_hits(
            inner,
            &query,
            case_insensitive,
            pattern_is_regex,
            origin,
            reverse,
            search_flags,
            requested,
        ) else {
            return 0;
        };
        if hits.is_empty() {
            return 0;
        }
        let limit = if requested <= 0 {
            hits.len()
        } else {
            hits.len().min(requested as usize)
        };
        lua::lua_createtable(l, limit as c_int, 0);
        for (index, hit) in hits.iter().take(limit).enumerate() {
            push_match(l, hit, None);
            lua::lua_rawseti_(l, -2, (index + 1) as c_int);
        }
        push_int(l, limit as i64);
    }
    2
}

#[allow(clippy::too_many_arguments)]
fn find_text_document_hits(
    inner: &cre_lib::Document,
    query: &str,
    case_insensitive: bool,
    pattern_is_regex: bool,
    origin: i32,
    reverse: bool,
    search_flags: i32,
    requested: i64,
) -> Option<Vec<cre_lib::SearchHit>> {
    if query.is_empty() {
        return Some(Vec::new());
    }
    let max_hits = (requested > 0).then_some(requested as usize);
    match inner.find_text_with_direction_flags(
        query,
        case_insensitive,
        pattern_is_regex,
        origin,
        reverse,
        search_flags,
        max_hits,
    ) {
        Ok(hits) => Some(hits),
        Err(_) => {
            if pattern_is_regex {
                REGEX_SEARCH_ERROR.store(REGEX_SEARCH_ERROR_INVALID_PATTERN, Ordering::Relaxed);
            }
            None
        }
    }
}

fn find_all_document_hits(
    inner: &cre_lib::Document,
    query: &str,
    case_insensitive: bool,
    pattern_is_regex: bool,
    search_flags: i32,
    requested: i64,
) -> Option<Vec<cre_lib::SearchHit>> {
    if query.is_empty() {
        return Some(Vec::new());
    }
    let max_hits = (requested > 0).then_some(requested as usize);
    match inner.find_text_with_search_flags(
        query,
        case_insensitive,
        pattern_is_regex,
        search_flags,
        max_hits,
    ) {
        Ok(hits) => Some(hits),
        Err(_) => {
            if pattern_is_regex {
                REGEX_SEARCH_ERROR.store(REGEX_SEARCH_ERROR_INVALID_PATTERN, Ordering::Relaxed);
            }
            None
        }
    }
}

unsafe fn push_match(l: *mut lua_State, hit: &cre_lib::SearchHit, matched_text: Option<&str>) {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        lua::lua_createtable(l, 0, 7);
        push_str(l, "start");
        push_str(l, &hit.xpointer_range.start.to_xpointer());
        lua::lua_rawset(l, -3);
        push_str(l, "end");
        let end = hit.xpointer_range.end.as_ref().map_or_else(
            || hit.xpointer_range.start.to_xpointer(),
            |end| end.to_xpointer(),
        );
        push_str(l, &end);
        lua::lua_rawset(l, -3);
        if let Some(text) = matched_text {
            push_str(l, "matched_text");
            push_str(l, text);
            lua::lua_rawset(l, -3);
            push_str(l, "prev_text");
            push_str(l, "");
            lua::lua_rawset(l, -3);
            push_str(l, "matched_word_prefix");
            push_str(l, "");
            lua::lua_rawset(l, -3);
            push_str(l, "matched_word_suffix");
            push_str(l, "");
            lua::lua_rawset(l, -3);
        }
    }
}

unsafe extern "C-unwind" fn doc_find_all_text(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(query) = arg_str(l, 2) else {
            return 0;
        };
        let case_insensitive = arg_bool(l, 3);
        let pattern_is_regex = arg_bool(l, 4);
        let requested = arg_int(l, 5);
        let get_matched_text = arg_bool(l, 6);
        let search_flags = arg_int(l, 8) as i32;
        let Some(inner) = doc_ref(l).and_then(|doc| doc.inner.as_ref()) else {
            return 0;
        };
        let Some(hits) = find_all_document_hits(
            inner,
            &query,
            case_insensitive,
            pattern_is_regex,
            search_flags,
            requested,
        ) else {
            return 0;
        };
        if hits.is_empty() {
            return 0;
        }
        let limit = if requested <= 0 {
            hits.len()
        } else {
            hits.len().min(requested as usize)
        };
        lua::lua_createtable(l, limit as c_int, 0);
        for (index, hit) in hits.iter().take(limit).enumerate() {
            push_match(l, hit, get_matched_text.then_some(query.as_str()));
            lua::lua_rawseti_(l, -2, (index + 1) as c_int);
        }
        push_int(l, limit as i64);
    }
    2
}

unsafe extern "C-unwind" fn doc_word_from_position(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let x = arg_int(l, 2) as f32;
        let y = arg_int(l, 3) as f32;
        let word = doc_ref(l).and_then(|doc| {
            let inner = doc.inner.as_ref()?;
            inner
                .word_at(cre_lib::PagePosition {
                    page: inner.current_page(),
                    x,
                    y,
                })
                .ok()
                .flatten()
        });
        let Some(word) = word else {
            return 0;
        };
        let rect = word.rects.first();
        lua::lua_createtable(l, 0, 6);
        push_str(l, "word");
        push_str(l, &word.text);
        lua::lua_rawset(l, -3);
        push_str(l, "text");
        push_str(l, &word.text);
        lua::lua_rawset(l, -3);
        push_str(l, "pos0");
        push_str(l, &word.xpointer_range.start.to_xpointer());
        lua::lua_rawset(l, -3);
        push_str(l, "pos1");
        let end = word
            .xpointer_range
            .end
            .as_ref()
            .map_or_else(String::new, |position| position.to_xpointer());
        push_str(l, &end);
        lua::lua_rawset(l, -3);
        for (key, value) in rect.map_or(
            [("x0", 0.0), ("y0", 0.0), ("x1", 0.0), ("y1", 0.0)],
            |rect| {
                [
                    ("x0", rect.x),
                    ("y0", rect.y),
                    ("x1", rect.x + rect.width),
                    ("y1", rect.y + rect.height),
                ]
            },
        ) {
            push_str(l, key);
            push_int(l, value.round() as i64);
            lua::lua_rawset(l, -3);
        }
    }
    1
}

unsafe extern "C-unwind" fn doc_nearest_word_from_position(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let x = arg_int(l, 2) as f32;
        let y = arg_int(l, 3) as f32;
        let direction = if lua::lua_isnoneornil(l, 4) != 0 {
            0
        } else {
            arg_int(l, 4).clamp(0, 4) as u32
        };
        let word = doc_ref(l).and_then(|doc| {
            let inner = doc.inner.as_ref()?;
            let page_count = inner.page_count();
            let start = inner.current_page();
            let pages = (start..=(start + doc.visible_page_count.max(1) as usize - 1))
                .filter(|page| *page >= 1 && *page <= page_count)
                .collect::<Vec<_>>();
            inner
                .nearest_word_at(
                    cre_lib::PagePosition { page: start, x, y },
                    &pages,
                    direction,
                )
                .ok()
                .flatten()
        });
        let Some(word) = word else {
            return 0;
        };
        lua::lua_createtable(l, 0, 3);
        push_str(l, "text");
        push_str(l, &word.text);
        lua::lua_rawset(l, -3);
        push_str(l, "pos0");
        push_str(l, &word.xpointer_range.start.to_xpointer());
        lua::lua_rawset(l, -3);
        push_str(l, "pos1");
        let end = word
            .xpointer_range
            .end
            .as_ref()
            .map_or_else(String::new, |position| position.to_xpointer());
        push_str(l, &end);
        lua::lua_rawset(l, -3);
    }
    1
}

unsafe extern "C-unwind" fn doc_text_from_positions(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let x0 = arg_int(l, 2) as f32;
        let y0 = arg_int(l, 3) as f32;
        let x1 = arg_int(l, 4) as f32;
        let y1 = arg_int(l, 5) as f32;
        let draw_selection = if lua::lua_isboolean(l, 6) != 0 {
            arg_bool(l, 6)
        } else {
            true
        };
        let draw_segmented = if lua::lua_isboolean(l, 7) != 0 {
            arg_bool(l, 7)
        } else {
            true
        };
        let include_images = lua::lua_isboolean(l, 8) != 0 && arg_bool(l, 8);
        let grab_words = if lua::lua_isboolean(l, 9) != 0 {
            arg_bool(l, 9)
        } else {
            true
        };
        let is_single_point = (x0 - x1).abs() < 1.0 && (y0 - y1).abs() < 1.0;
        let (sx, sy, ex, ey) = if y1 < y0 || (y1 == y0 && x1 < x0) {
            (x1, y1, x0, y0)
        } else {
            (x0, y0, x1, y1)
        };
        let Some((text, pos0, pos1)) = doc_mut(l).and_then(|doc| {
            doc.last_range = None;
            doc.last_boxes.clear();
            let mut result = {
                let inner = doc.inner.as_ref()?;
                let page = inner.current_page();
                let start = cre_lib::PagePosition { page, x: sx, y: sy };
                let end = cre_lib::PagePosition { page, x: ex, y: ey };
                if is_single_point {
                    let word = inner.word_at(start).ok().flatten()?;
                    let start_pos = word.xpointer_range.start.to_xpointer();
                    let end_pos = word.xpointer_range.end.as_ref()?.to_xpointer();
                    Some((
                        word.text,
                        start_pos.clone(),
                        end_pos.clone(),
                        (start_pos, end_pos),
                        word.rects,
                    ))
                } else {
                    let selection = inner.selection(start, end).ok().flatten()?;
                    let start_pos = selection.xpointer_range.start.to_xpointer();
                    let end_pos = selection
                        .xpointer_range
                        .end
                        .as_ref()
                        .map_or_else(String::new, |position| position.to_xpointer());
                    Some((
                        selection.text,
                        start_pos.clone(),
                        end_pos.clone(),
                        (start_pos, end_pos),
                        selection.rects,
                    ))
                }
            };
            if let Some((_, _, _, range, boxes)) = &result {
                doc.last_range = Some(range.clone());
                doc.last_boxes = boxes.clone();
                if draw_selection {
                    doc.highlight = Some(range.clone());
                }
            }
            if grab_words && !is_single_point {
                let extension = if let Some(inner) = doc.inner.as_ref() {
                    if let Some(start_word) = inner
                        .word_at(cre_lib::PagePosition {
                            page: inner.current_page(),
                            x: sx,
                            y: sy,
                        })
                        .ok()
                        .flatten()
                    {
                        if let Some(start_word_end) = start_word.xpointer_range.end.as_ref() {
                            let use_start = start_word.xpointer_range.start.to_xpointer();
                            let use_start_end = start_word_end.to_xpointer();
                            if use_start != use_start_end {
                                if let Some(end_word) = inner
                                    .word_at(cre_lib::PagePosition {
                                        page: inner.current_page(),
                                        x: ex,
                                        y: ey,
                                    })
                                    .ok()
                                    .flatten()
                                {
                                    if let Some(end_pos_ref) = end_word.xpointer_range.end.as_ref()
                                    {
                                        let use_end = end_pos_ref.to_xpointer();
                                        let new_text = inner
                                            .text_between_xpointers(&use_start, &use_end)
                                            .ok()
                                            .unwrap_or_default();
                                        if !new_text.trim().is_empty() {
                                            let new_boxes = inner
                                                .boxes_between_xpointers(&use_start, &use_end)
                                                .ok()
                                                .unwrap_or_default();
                                            if !new_boxes.is_empty() {
                                                let new_range =
                                                    (use_start.clone(), use_end.clone());
                                                Some((
                                                    new_text, use_start, use_end, new_range,
                                                    new_boxes,
                                                ))
                                            } else {
                                                None
                                            }
                                        } else {
                                            None
                                        }
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some((new_text, use_start, use_end, range, boxes)) = extension {
                    let range_for_highlight = range.clone();
                    let boxes_for_cache = boxes.clone();
                    result = Some((new_text, use_start, use_end, range, boxes));
                    doc.last_range = Some(range_for_highlight.clone());
                    doc.last_boxes = boxes_for_cache;
                    if draw_selection {
                        doc.highlight = Some(range_for_highlight.clone());
                    }
                }
            }
            let _ = draw_segmented;
            let _ = include_images;
            result.map(|(text, start_pos, end_pos, _, _)| (text, start_pos, end_pos))
        }) else {
            return 0;
        };

        lua::lua_createtable(l, 0, 3);
        push_str(l, "text");
        push_str(l, &text);
        lua::lua_rawset(l, -3);
        push_str(l, "pos0");
        push_str(l, &pos0);
        lua::lua_rawset(l, -3);
        push_str(l, "pos1");
        push_str(l, &pos1);
        lua::lua_rawset(l, -3);
    }
    1
}

unsafe extern "C-unwind" fn doc_word_boxes_from_positions(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(start) = arg_str(l, 2) else {
            lua::lua_newtable(l);
            return 1;
        };
        let Some(end) = arg_str(l, 3) else {
            lua::lua_newtable(l);
            return 1;
        };
        let get_segments = lua::lua_isboolean(l, 4) != 0 && arg_bool(l, 4);
        let _include_images = lua::lua_isboolean(l, 5) != 0 && arg_bool(l, 5);
        let Some(doc) = doc_ref(l) else {
            lua::lua_newtable(l);
            return 1;
        };
        let Some(inner) = doc.inner.as_ref() else {
            lua::lua_newtable(l);
            return 1;
        };
        if let Some((last_start, last_end)) = doc.last_range.as_ref()
            && start == last_start.as_str()
            && end == last_end.as_str()
        {
            let cached_boxes = doc.last_boxes.clone();
            lua::lua_newtable(l);
            let mut cached_index = 0;
            for text_box in cached_boxes.iter() {
                if let Some((x0, y0, x1, y1)) = window_rect_for_page_box(
                    doc.view_mode,
                    doc.visible_page_count,
                    doc.height,
                    inner,
                    text_box,
                ) {
                    cached_index += 1;
                    lua::lua_createtable(l, 0, 4);
                    for (key, value) in [("x0", x0), ("y0", y0), ("x1", x1), ("y1", y1)] {
                        push_str(l, key);
                        push_int(l, value.round() as i64);
                        lua::lua_rawset(l, -3);
                    }
                    lua::lua_rawseti_(l, -2, cached_index as c_int);
                }
            }
            return 1;
        }
        let use_start = if get_segments {
            start.clone()
        } else {
            expand_to_visible_word_start(inner, &start)
        };
        let use_end = if get_segments {
            end.clone()
        } else {
            expand_to_visible_word_end(inner, &end)
        };
        let Some(raw_boxes) = inner.boxes_between_xpointers(&use_start, &use_end).ok() else {
            return 0;
        };
        lua::lua_newtable(l);
        let mut index = 0;
        for text_box in raw_boxes.iter() {
            if let Some((x0, y0, x1, y1)) = window_rect_for_page_box(
                doc.view_mode,
                doc.visible_page_count,
                doc.height,
                inner,
                text_box,
            ) {
                index += 1;
                lua::lua_createtable(l, 0, 4);
                for (key, value) in [("x0", x0), ("y0", y0), ("x1", x1), ("y1", y1)] {
                    push_str(l, key);
                    push_int(l, value.round() as i64);
                    lua::lua_rawset(l, -3);
                }
                lua::lua_rawseti_(l, -2, index as c_int);
            }
        }
    }
    1
}

unsafe extern "C-unwind" fn doc_text_from_xpointers(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(start) = arg_str(l, 2) else {
            return 0;
        };
        let Some(end) = arg_str(l, 3) else {
            return 0;
        };
        let draw_selection = lua::lua_isboolean(l, 4) != 0 && arg_bool(l, 4);
        let draw_segmented = if lua::lua_isboolean(l, 5) != 0 {
            arg_bool(l, 5)
        } else {
            draw_selection
        };
        let include_images = lua::lua_isboolean(l, 6) != 0 && arg_bool(l, 6);
        let text = doc_ref(l).and_then(|doc| {
            let inner = doc.inner.as_ref()?;
            inner.text_between_xpointers(&start, &end).ok()
        });
        if draw_selection && let Some(doc) = doc_mut(l) {
            doc.highlight = Some((start.clone(), end.clone()));
            doc.last_range = Some((start.clone(), end.clone()));
            let _ = draw_segmented;
            let _ = include_images;
        }
        push_str(l, text.as_deref().unwrap_or_default());
    }
    1
}

unsafe extern "C-unwind" fn doc_link_from_position(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let x = arg_int(l, 2) as f32;
        let y = arg_int(l, 3) as f32;
        let hit = doc_ref(l).and_then(|doc| {
            let inner = doc.inner.as_ref()?;
            inner
                .hit_test(cre_lib::PagePosition {
                    page: inner.current_page(),
                    x,
                    y,
                })
                .ok()
                .flatten()
        });
        if let Some(hit) = hit {
            let Some(inner) = doc_ref(l).and_then(|doc| doc.inner.as_ref()) else {
                push_str(l, "");
                return 1;
            };
            let link = hit.link.unwrap_or_default();
            if !link.is_empty() {
                let target_xp = inner
                    .page_from_href(&link)
                    .ok()
                    .and_then(|page| inner.xpointer_from_page(page).ok());
                push_str(l, target_xp.unwrap_or(link).as_ref());
                let source_xp = inner.xpointer_from_position(&hit.text_position).ok();
                if let Some(source_xp) = source_xp {
                    push_str(l, source_xp.as_ref());
                } else {
                    push_str(l, "");
                }
                return 2;
            }
        }
        push_str(l, "");
    }
    1
}

unsafe extern "C-unwind" fn doc_set_view_dimen(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            let width = arg_int(l, 2).max(1) as u32;
            let height = arg_int(l, 3).max(1) as u32;
            if (width, height) != (doc.width, doc.height) {
                doc.width = width;
                doc.height = height;
                if doc.inner.is_some() {
                    doc.relayout();
                }
            }
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_set_page_margins(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            let margins = (
                arg_int(l, 2) as i32,
                arg_int(l, 3) as i32,
                arg_int(l, 4) as i32,
                arg_int(l, 5) as i32,
            );
            if margins != doc.margins {
                doc.margins = margins;
                if doc.inner.is_some() {
                    doc.relayout();
                }
            }
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_set_font_face(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            let face = arg_str(l, 2).unwrap_or_default();
            let font_path = find_font_face(&face, false, false).map(|font| font.path.clone());
            let changed = doc.font_face != face || doc.font_path != font_path;
            doc.font_face = face;
            doc.font_path = font_path;
            if changed && doc.inner.is_some() {
                doc.relayout();
            }
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_set_font_size(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            let font_size = arg_int(l, 2).clamp(1, 512) as i32;
            if font_size != doc.font_size {
                doc.font_size = font_size;
                if doc.inner.is_some() {
                    doc.relayout();
                }
            }
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_get_int_property(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(name) = arg_str(l, 2) else {
            return 0;
        };
        let Some(doc) = doc_ref(l) else {
            return 0;
        };
        let (left, top, right, bottom) = doc.margins;
        let value = match name.as_str() {
            "font.size" => i64::from(doc.font_size),
            "view.mode" => i64::from(doc.view_mode),
            "crengine.page.margin.left" => i64::from(left),
            "crengine.page.margin.top" => i64::from(top),
            "crengine.page.margin.right" => i64::from(right),
            "crengine.page.margin.bottom" => i64::from(bottom),
            "crengine.interline.space" => i64::from(doc.interline),
            "crengine.page.header.height" => i64::from(doc.header_height),
            "window.pages.two.visible.as.one.page.number" => {
                i64::from(doc.two_visible_pages_count_as_one())
            }
            "window.status.battery.percent" => i64::from(doc.battery),
            "crengine.render.dpi" => i64::from(doc.render_dpi),
            "crengine.render.scale.font.with.dpi" => {
                if doc.scale_font_with_dpi {
                    1_i64
                } else {
                    0_i64
                }
            }
            _ => doc
                .properties
                .get(&name)
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(0),
        };
        push_int(l, value);
    }
    1
}

unsafe extern "C-unwind" fn doc_get_string_property(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(name) = arg_str(l, 2) else {
            return 0;
        };
        let Some(doc) = doc_ref(l) else {
            return 0;
        };
        let value = match name.as_str() {
            "font.face.default" | "font.face" => doc.font_face.clone(),
            "crengine.textlang.main.lang" => {
                let fallback = doc
                    .inner
                    .as_ref()
                    .and_then(|inner| inner.metadata().language.clone())
                    .unwrap_or_default();
                doc.properties
                    .get(&name)
                    .filter(|value| !value.is_empty())
                    .cloned()
                    .unwrap_or(fallback)
            }
            _ => doc.properties.get(&name).cloned().unwrap_or_default(),
        };
        push_str(l, &value);
    }
    1
}

unsafe extern "C-unwind" fn doc_set_int_property(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(name) = arg_str(l, 2) else {
            return 0;
        };
        let value = arg_int(l, 3);
        let Some(doc) = doc_mut(l) else {
            return 0;
        };
        doc.properties.insert(name.clone(), value.to_string());
        let mut relayout = false;
        match name.as_str() {
            "font.size" => {
                let size = value.clamp(1, 512) as i32;
                if size != doc.font_size {
                    doc.font_size = size;
                    relayout = true;
                }
            }
            "view.mode" => {
                if value == 0 || value == 1 {
                    doc.view_mode = value as i32;
                }
            }
            "crengine.page.margin.left" => {
                doc.margins.0 = value as i32;
                relayout = true;
            }
            "crengine.page.margin.top" => {
                doc.margins.1 = value as i32;
                relayout = true;
            }
            "crengine.page.margin.right" => {
                doc.margins.2 = value as i32;
                relayout = true;
            }
            "crengine.page.margin.bottom" => {
                doc.margins.3 = value as i32;
                relayout = true;
            }
            "crengine.interline.space" => {
                let interline = value.clamp(0, 1000) as i32;
                if interline != doc.interline {
                    doc.interline = interline;
                    relayout = true;
                }
            }
            "window.status.battery.percent" => doc.battery = value as i32,
            "crengine.doc.embedded.styles.enabled" => {
                let enabled = value != 0;
                if doc.embedded_stylesheet_enabled != enabled {
                    doc.embedded_stylesheet_enabled = enabled;
                    relayout = true;
                }
            }
            "crengine.render.dpi" => {
                let dpi = value.clamp(0, i64::from(u32::MAX)) as u32;
                if dpi != doc.render_dpi {
                    doc.render_dpi = dpi;
                    relayout = true;
                }
            }
            "crengine.render.scale.font.with.dpi" => {
                let enabled = value != 0;
                if enabled != doc.scale_font_with_dpi {
                    doc.scale_font_with_dpi = enabled;
                    relayout = true;
                }
            }
            "crengine.doc.embedded.fonts.enabled" => {}
            "font.gamma"
            | "font.gamma.index"
            | "font.face.base.weight"
            | "font.face.weight.embolden" => {
                relayout = true;
            }
            _ => {}
        }
        if relayout && doc.inner.is_some() {
            doc.relayout();
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_set_string_property(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(name) = arg_str(l, 2) else {
            return 0;
        };
        let value = arg_str(l, 3).unwrap_or_default();
        let Some(doc) = doc_mut(l) else {
            return 0;
        };
        let previous = doc.properties.get(&name).cloned().unwrap_or(String::new());
        doc.properties.insert(name.clone(), value.clone());
        match name.as_str() {
            "font.face.default" | "font.face" => {
                let font_path = find_font_face(&value, false, false).map(|font| font.path.clone());
                let changed = doc.font_face != value || doc.font_path != font_path;
                doc.font_face = value;
                doc.font_path = font_path;
                if changed && doc.inner.is_some() {
                    doc.relayout();
                }
            }
            "crengine.highlight.selection.color" => {}
            "crengine.font.fallback.faces" if previous != value && doc.inner.is_some() => {
                doc.relayout();
            }
            "font.gamma"
            | "font.gamma.index"
            | "font.face.base.weight"
            | "font.face.weight.embolden"
                if previous != value && doc.inner.is_some() =>
            {
                doc.relayout();
            }
            _ => {}
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_request_render(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            doc.render_requested = true;
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_set_alt_prop(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(name) = arg_str(l, 2) else {
            return 0;
        };
        if let Some(doc) = doc_mut(l) {
            if let Some(value) = arg_str(l, 3) {
                doc.alt_properties.insert(name, value);
            } else {
                doc.alt_properties.remove(&name);
            }
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_can_partial_rerender(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { push_bool(l, doc_ref(l).is_some_and(|doc| doc.inner.is_some())) };
    1
}

unsafe extern "C-unwind" fn doc_is_partial_enabled(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { push_bool(l, doc_ref(l).is_some_and(|doc| doc.partial_rerendering)) };
    1
}

unsafe extern "C-unwind" fn doc_enable_partial(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let enabled = arg_bool(l, 2);
        if let Some(doc) = doc_mut(l) {
            doc.partial_rerendering = enabled;
        }
        push_bool(
            l,
            enabled && doc_ref(l).is_some_and(|doc| doc.inner.is_some()),
        );
    }
    1
}

unsafe extern "C-unwind" fn doc_partial_count(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let count = doc_ref(l).map_or(0, |doc| doc.rerender_count as i64);
        push_int(l, count);
    }
    1
}

unsafe extern "C-unwind" fn doc_rerender_delayed(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { push_bool(l, false) };
    1
}

unsafe extern "C-unwind" fn doc_set_view_mode(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let mode = arg_int(l, 2);
        if let Some(doc) = doc_mut(l)
            && (mode == 0 || mode == 1)
        {
            doc.view_mode = mode as i32;
        }
    }
    0
}
unsafe extern "C-unwind" fn doc_set_header_info(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let info = arg_int(l, 2);
        if let Some(doc) = doc_mut(l) {
            doc.properties
                .insert("header.info".to_string(), info.to_string());
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_set_page_info_override(_l: *mut lua_State) -> c_int {
    0
}

unsafe extern "C-unwind" fn doc_set_header_font(_l: *mut lua_State) -> c_int {
    0
}

unsafe extern "C-unwind" fn doc_set_header_progress(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            let pages = arg_int(l, 2).max(1);
            let mut bounds = vec![0];
            if lua::lua_istable(l, 3) != 0 {
                let len = lua::lua_objlen(l, 3);
                for index in 1..=len {
                    lua::lua_rawgeti(
                        l,
                        3,
                        lua::lua_Integer::try_from(index).unwrap_or(lua::lua_Integer::MAX),
                    );
                    if lua::lua_isnumber(l, -1) != 0 {
                        let page = arg_int(l, -1);
                        bounds.push(10_000 * (page - 1) / pages);
                    }
                    lua::lua_pop(l, 1);
                }
            }
            bounds.push(10_000);
            doc.properties.insert(
                "header.progress.marks".to_string(),
                bounds
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_set_default_interline(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            let value = arg_int(l, 2).clamp(0, 1000) as i32;
            if value != doc.interline {
                doc.interline = value;
                doc.properties
                    .insert("crengine.interline.space".to_string(), value.to_string());
                if doc.inner.is_some() {
                    doc.relayout();
                }
            }
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_set_stylesheet(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(doc) = doc_mut(l) else {
            return 0;
        };
        let mut stylesheet = String::new();
        if let Some(path) = arg_str(l, 2)
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            stylesheet.push_str(&content);
        }
        if let Some(content) = arg_str(l, 3) {
            if !stylesheet.is_empty() {
                stylesheet.push_str("\r\n");
            }
            stylesheet.push_str(&content);
        }
        let changed = stylesheet != doc.stylesheet;
        doc.stylesheet = stylesheet.clone();
        doc.properties.insert("stylesheet".to_string(), stylesheet);
        if changed && doc.inner.is_some() {
            doc.relayout();
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_set_embedded_stylesheet(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let enabled = arg_int(l, 2) != 0;
        let value = if enabled {
            "1".to_string()
        } else {
            "0".to_string()
        };
        if let Some(doc) = doc_mut(l) {
            let changed = doc.embedded_stylesheet_enabled != enabled;
            doc.embedded_stylesheet_enabled = enabled;
            doc.properties
                .insert("crengine.doc.embedded.styles.enabled".to_string(), value);
            if changed {
                doc.relayout();
            }
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_set_embedded_fonts(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let enabled = arg_int(l, 2);
        if let Some(doc) = doc_mut(l) {
            doc.properties.insert(
                "crengine.doc.embedded.fonts.enabled".to_string(),
                enabled.to_string(),
            );
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_set_background_image(_l: *mut lua_State) -> c_int {
    0
}

unsafe extern "C-unwind" fn doc_set_visible_page_count(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            let count = arg_int(l, 2);
            let only_if_sane = if lua::lua_isboolean(l, 3) != 0 {
                arg_bool(l, 3)
            } else {
                true
            };
            let next = count.clamp(1, 4) as i32;
            if count == i64::from(next) || !only_if_sane {
                doc.visible_page_count = next;
            }
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_adjust_font_sizes(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let dpi_value = arg_int(l, 2);
        if let Some(doc) = doc_mut(l) {
            doc.properties
                .insert("crengine.render.dpi".to_string(), dpi_value.to_string());
            let dpi = dpi_value.clamp(0, i64::from(u32::MAX)) as u32;
            if dpi != doc.render_dpi {
                doc.render_dpi = dpi;
                if doc.inner.is_some() {
                    doc.relayout();
                }
            }
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_set_battery_state(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let battery = arg_int(l, 2);
        if let Some(doc) = doc_mut(l) {
            doc.battery = battery as i32;
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_set_callback(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state. The Lua callback is stored in the
    // registry, matching `cre.cpp`'s `setCallback` behavior.
    unsafe {
        if lua::lua_isfunction(l, 2) != 0 {
            if let Some(doc) = doc_mut(l) {
                if let Some(previous) = doc.callback {
                    lua::luaL_unref(l, lua::LUA_REGISTRYINDEX, previous);
                }
                lua::lua_pushvalue(l, 2);
                doc.callback = Some(lua::luaL_ref(l, lua::LUA_REGISTRYINDEX));
            } else {
                let reference = lua::luaL_ref(l, lua::LUA_REGISTRYINDEX);
                lua::luaL_unref(l, lua::LUA_REGISTRYINDEX, reference);
            }
        } else {
            if let Some(doc) = doc_mut(l) {
                if let Some(previous) = doc.callback {
                    lua::luaL_unref(l, lua::LUA_REGISTRYINDEX, previous);
                }
                doc.callback = None;
            }
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_cache_false(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { push_bool(l, false) };
    1
}

unsafe extern "C-unwind" fn doc_cache_path(_l: *mut lua_State) -> c_int {
    0
}

unsafe extern "C-unwind" fn doc_invalidate_cache(_l: *mut lua_State) -> c_int {
    0
}

unsafe extern "C-unwind" fn doc_update_toc(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l)
            && doc.inner.is_some()
        {
            doc.relayout();
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_statistics(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(inner) = doc_ref(l).and_then(|doc| doc.inner.as_ref()) else {
            push_str(l, "");
            return 1;
        };
        let mut characters = 0usize;
        let mut words = 0usize;
        for page in 1..=inner.page_count() {
            if let Ok(text) = inner.page_text(page) {
                characters += text.chars().count();
                words += text.split_whitespace().count();
            }
        }
        push_str(
            l,
            &format!(
                "characters: {characters}, words: {words}, pages: {}",
                inner.page_count()
            ),
        );
    }
    1
}

unsafe extern "C-unwind" fn doc_unknown_entities(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        push_str(l, "");
        push_str(l, "");
        push_str(l, "");
    }
    3
}

unsafe extern "C-unwind" fn doc_build_alternative_toc(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            doc.properties
                .insert("toc.alternative".to_string(), "1".to_string());
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_is_alternative_toc(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let value = doc_ref(l)
            .and_then(|doc| doc.properties.get("toc.alternative"))
            .is_some_and(|value| value == "1");
        push_bool(l, value);
    }
    1
}

unsafe extern "C-unwind" fn doc_get_page_flow(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let page = arg_int(l, 2);
        let value = doc_ref(l).and_then(|doc| {
            let inner = doc.inner.as_ref()?;
            let page_idx = usize::try_from(page).ok()?;
            if page < 1 || page_idx > inner.page_count() {
                return Some(-1);
            }
            Some(inner.page_flow(page_idx))
        });
        push_int(l, value.unwrap_or(-1));
    }
    1
}

unsafe extern "C-unwind" fn doc_get_page_offset_x(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let page = arg_int(l, 2);
        let offset = doc_ref(l).and_then(|doc| {
            let inner = doc.inner.as_ref()?;
            if page < 1 || usize::try_from(page).ok()? > inner.page_count() {
                return Some(0);
            }
            let count = i64::from(doc.visible_page_count.max(1));
            let width = i64::from(inner.page_width());
            Some((page - 1).rem_euclid(count) * width)
        });
        push_int(l, offset.unwrap_or(0));
    }
    1
}

unsafe extern "C-unwind" fn doc_get_header_height(_l: *mut lua_State) -> c_int {
    unsafe { push_int(_l, 0) };
    1
}

unsafe extern "C-unwind" fn doc_get_embedded_font_list(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe { lua::lua_newtable(l) };
    1
}

unsafe extern "C-unwind" fn doc_get_lang_tag(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let x = arg_int(l, 2) as f32;
        let y = arg_int(l, 3) as f32;
        let Some(inner) = doc_ref(l).and_then(|doc| doc.inner.as_ref()) else {
            return 0;
        };
        let position = cre_lib::PagePosition {
            page: inner.current_page(),
            x,
            y,
        };
        let has_text =
            inner.text_at(position).ok().is_some() || inner.word_at(position).ok().is_some();
        let Some(language) = inner.metadata().language.clone() else {
            return 0;
        };
        if !has_text {
            return 0;
        }
        push_str(l, &language);
    }
    1
}

unsafe extern "C-unwind" fn doc_get_document_file_content(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(path) = arg_str(l, 2) else {
            return 0;
        };
        let data = doc_ref(l)
            .and_then(|doc| doc.inner.as_ref())
            .and_then(|inner| inner.resource(&path));
        let Some(data) = data else {
            return 0;
        };
        push_bytes(l, &data);
    }
    1
}

unsafe extern "C-unwind" fn doc_get_html_from_xpointer(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let html = arg_str(l, 2).and_then(|xpointer| {
            doc_ref(l)
                .and_then(|doc| doc.inner.as_ref())
                .and_then(|inner| inner.html_from_xpointer(&xpointer).ok().flatten())
        });
        let Some(html) = html else {
            return 0;
        };
        push_str(l, &html);
        lua::lua_newtable(l);
        push_str(l, "");
    }
    3
}

unsafe extern "C-unwind" fn doc_get_html_from_xpointers(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(start) = arg_str(l, 2) else {
            return 0;
        };
        let Some(end) = arg_str(l, 3) else {
            return 0;
        };
        let html = doc_ref(l).and_then(|doc| {
            doc.inner
                .as_ref()?
                .html_between_xpointers(&start, &end)
                .ok()
                .flatten()
        });
        let Some(html) = html else {
            return 0;
        };
        push_str(l, &html);
        lua::lua_newtable(l);
        push_str(l, "");
    }
    3
}

unsafe extern "C-unwind" fn doc_get_stylesheets_matching_rulesets(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(doc) = doc_ref(l) else {
            lua::lua_newtable(l);
            return 1;
        };
        let with_user_stylesheet = if lua::lua_isboolean(l, 3) != 0 {
            arg_bool(l, 3)
        } else {
            true
        };
        lua::lua_newtable(l);
        if with_user_stylesheet && !doc.stylesheet.is_empty() {
            push_str(l, "1");
            push_str(l, &doc.stylesheet);
            lua::lua_rawseti_(l, -2, 1);
        }
    }
    1
}

unsafe extern "C-unwind" fn doc_extend_sentence(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(start) = arg_str(l, 2) else {
            return 0;
        };
        let Some(end) = arg_str(l, 3) else {
            return 0;
        };
        let extended = doc_ref(l).and_then(|doc| {
            doc.inner
                .as_ref()?
                .extend_to_sentence(&start, &end)
                .ok()
                .flatten()
        });
        let Some((text, start, end)) = extended else {
            return 0;
        };
        lua::lua_createtable(l, 0, 3);
        push_str(l, "text");
        push_str(l, &text);
        lua::lua_rawset(l, -3);
        push_str(l, "pos0");
        push_str(l, &start);
        lua::lua_rawset(l, -3);
        push_str(l, "pos1");
        push_str(l, &end);
        lua::lua_rawset(l, -3);
    }
    1
}

unsafe extern "C-unwind" fn doc_is_link_to_footnote(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(source) = arg_str(l, 2) else {
            return 0;
        };
        let Some(target) = arg_str(l, 3) else {
            return 0;
        };
        let max_text_size = if arg_int(l, 5) > 0 {
            arg_int(l, 5) as usize
        } else {
            10_000
        };
        let Some(inner) = doc_ref(l).and_then(|doc| doc.inner.as_ref()) else {
            return 0;
        };
        let source_page = inner.page_from_xpointer(&source).ok();
        let target_page = inner.page_from_xpointer(&target).ok();
        let target_text = inner.text_from_xpointer(&target).unwrap_or_default();
        let is_footnote = target_page.is_some()
            && target_text.chars().count() <= max_text_size
            && source_page
                .zip(target_page)
                .is_some_and(|(source, target)| target.abs_diff(source) <= 1);
        push_bool(l, is_footnote);
        push_str(
            l,
            if is_footnote {
                "target is short and close to the source"
            } else {
                "target is not a short nearby text block"
            },
        );
    }
    2
}

unsafe extern "C-unwind" fn doc_highlight(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(doc) = doc_mut(l) else {
            return 0;
        };
        let xpointer = arg_str(l, 2);
        let highlight = match xpointer {
            None => None,
            Some(xpointer) => {
                let range = doc.inner.as_ref().and_then(|inner| {
                    let page = inner.page_from_xpointer(&xpointer).ok()?;
                    let (x, y) = inner.point_from_xpointer(&xpointer).ok()??;
                    let word = inner
                        .word_at(cre_lib::PagePosition { page, x, y })
                        .ok()
                        .flatten()?;
                    let end = word
                        .xpointer_range
                        .end
                        .as_ref()
                        .map_or_else(String::new, |position| position.to_xpointer());
                    (!end.is_empty()).then(|| (word.xpointer_range.start.to_xpointer(), end))
                });
                Some(range.unwrap_or_else(|| (xpointer.clone(), xpointer)))
            }
        };
        doc.highlight = highlight;
        push_bool(l, true);
    }
    1
}

unsafe extern "C-unwind" fn doc_get_cover_image_data(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let data = doc_ref(l)
            .and_then(|doc| doc.inner.as_ref())
            .and_then(|inner| inner.cover_image())
            .map(<[u8]>::to_vec);
        let Some(data) = data else {
            return 0;
        };
        if !push_malloc_bytes(l, &data) {
            return 0;
        }
    }
    2
}

unsafe extern "C-unwind" fn doc_get_image_data(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let x = arg_int(l, 2) as f32;
        let y = arg_int(l, 3) as f32;
        let accept_cre_scalable_image = arg_bool(l, 4);
        let data = doc_ref(l).and_then(|doc| {
            let inner = doc.inner.as_ref()?;
            inner
                .image_data_at(cre_lib::PagePosition {
                    page: inner.current_page(),
                    x,
                    y,
                })
                .ok()
                .flatten()
        });
        let Some(data) = data else {
            return 0;
        };
        if accept_cre_scalable_image && let Some(image) = CreImage::from_svg(&data) {
            push_bool(l, false);
            push_int(l, 0);
            push_image(l, image);
            return 3;
        }
        if !push_malloc_bytes(l, &data) {
            return 0;
        }
    }
    2
}

unsafe extern "C-unwind" fn doc_goto_link(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(link) = arg_str(l, 2) else {
            return 0;
        };
        if let Some(doc) = doc_mut(l) {
            let moved = doc.inner.as_mut().is_some_and(|inner| {
                inner.go_to_xpointer(&link).is_ok() || inner.go_to_href(&link).is_ok()
            });
            if moved {
                doc.record_history();
            }
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_go_back(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l)
            && doc.history_index > 0
            && doc.history_index < doc.history.len()
        {
            let target = doc.history[doc.history_index - 1].clone();
            if let Some(inner) = doc.inner.as_mut() {
                let _ = inner.go_to_xpointer(&target);
                doc.history_index -= 1;
            }
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_go_forward(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l)
            && doc.history_index + 1 < doc.history.len()
        {
            let target = doc.history[doc.history_index + 1].clone();
            if let Some(inner) = doc.inner.as_mut() {
                let _ = inner.go_to_xpointer(&target);
                doc.history_index += 1;
            }
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_clear_selection(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            doc.last_range = None;
            doc.last_boxes.clear();
            doc.highlight = None;
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_next_visible_word_start(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let value = arg_str(l, 2).and_then(|xp| {
            doc_ref(l)
                .and_then(|doc| doc.inner.as_ref())
                .and_then(|inner| inner.next_visible_word_start(&xp).ok().flatten())
        });
        let Some(value) = value else {
            return 0;
        };
        push_str(l, &value);
    }
    1
}

unsafe extern "C-unwind" fn doc_next_visible_word_end(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let value = arg_str(l, 2).and_then(|xp| {
            doc_ref(l)
                .and_then(|doc| doc.inner.as_ref())
                .and_then(|inner| inner.next_visible_word_end(&xp).ok().flatten())
        });
        let Some(value) = value else {
            return 0;
        };
        push_str(l, &value);
    }
    1
}

unsafe extern "C-unwind" fn doc_prev_visible_word_start(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let value = arg_str(l, 2).and_then(|xp| {
            doc_ref(l)
                .and_then(|doc| doc.inner.as_ref())
                .and_then(|inner| inner.prev_visible_word_start(&xp).ok().flatten())
        });
        let Some(value) = value else {
            return 0;
        };
        push_str(l, &value);
    }
    1
}

unsafe extern "C-unwind" fn doc_prev_visible_word_end(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let value = arg_str(l, 2).and_then(|xp| {
            doc_ref(l)
                .and_then(|doc| doc.inner.as_ref())
                .and_then(|inner| inner.prev_visible_word_end(&xp).ok().flatten())
        });
        let Some(value) = value else {
            return 0;
        };
        push_str(l, &value);
    }
    1
}

unsafe extern "C-unwind" fn doc_next_visible_char(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let value = arg_str(l, 2).and_then(|xp| {
            doc_ref(l)
                .and_then(|doc| doc.inner.as_ref())
                .and_then(|inner| inner.next_visible_char(&xp).ok().flatten())
        });
        let Some(value) = value else {
            return 0;
        };
        push_str(l, &value);
    }
    1
}

unsafe extern "C-unwind" fn doc_prev_visible_char(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let value = arg_str(l, 2).and_then(|xp| {
            doc_ref(l)
                .and_then(|doc| doc.inner.as_ref())
                .and_then(|inner| inner.prev_visible_char(&xp).ok().flatten())
        });
        let Some(value) = value else {
            return 0;
        };
        push_str(l, &value);
    }
    1
}

unsafe extern "C-unwind" fn doc_epub_cfi(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let cfi = doc_ref(l)
            .and_then(|doc| doc.inner.as_ref())
            .and_then(|inner| inner.xpointer_from_page(inner.current_page()).ok())
            .and_then(|xp| cre_lib::parse_text_position(&xp).ok())
            .map(|position| cfi_for_position(&position));
        let Some(cfi) = cfi else {
            return 0;
        };
        push_str(l, &cfi);
    }
    1
}

unsafe extern "C-unwind" fn doc_page_epub_cfi(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let page = arg_int(l, 2).max(1) as usize;
        let cfi = doc_ref(l)
            .and_then(|doc| doc.inner.as_ref())
            .and_then(|inner| inner.xpointer_from_page(page).ok())
            .and_then(|xp| cre_lib::parse_text_position(&xp).ok())
            .map(|position| cfi_for_position(&position));
        let Some(cfi) = cfi else {
            return 0;
        };
        push_str(l, &cfi);
    }
    1
}

unsafe extern "C-unwind" fn doc_epub_cfi_from_xpointer(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let cfi = arg_str(l, 2)
            .and_then(|xp| cre_lib::parse_text_position(&xp).ok())
            .map(|position| cfi_for_position(&position));
        let Some(cfi) = cfi else {
            return 0;
        };
        push_str(l, &cfi);
    }
    1
}

unsafe extern "C-unwind" fn doc_epub_cfi_range(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(start) = arg_str(l, 2) else {
            return 0;
        };
        let Some(end) = arg_str(l, 3) else {
            return 0;
        };
        let (Ok(start), Ok(end)) = (
            cre_lib::parse_text_position(&start),
            cre_lib::parse_text_position(&end),
        ) else {
            return 0;
        };
        push_str(l, &cfi_range_for_positions(&start, &end));
    }
    1
}

unsafe extern "C-unwind" fn doc_has_page_map_document_provided(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let value = doc_ref(l)
            .and_then(|doc| doc.inner.as_ref())
            .is_some_and(Document::has_document_page_map);
        push_bool(l, value);
    }
    1
}

unsafe extern "C-unwind" fn doc_is_page_map_document_provided(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let value = doc_ref(l).is_some_and(|doc| {
            !doc.page_map.is_empty()
                && doc
                    .inner
                    .as_ref()
                    .is_some_and(Document::has_document_page_map)
                && doc.page_map_source != "synthetic"
        });
        push_bool(l, value);
    }
    1
}

unsafe extern "C-unwind" fn doc_has_non_linear_flows(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let value = doc_ref(l)
            .and_then(|doc| doc.inner.as_ref())
            .is_some_and(Document::has_non_linear_flows);
        push_bool(l, value);
    }
    1
}

unsafe extern "C-unwind" fn doc_build_synthetic_page_map(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let chars = arg_int(l, 2).max(1) as usize;
        if let Some(doc) = doc_mut(l) {
            doc.build_synthetic_page_map(chars);
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_build_synthetic_if_none(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let chars = arg_int(l, 2).max(1) as usize;
        if let Some(doc) = doc_mut(l)
            && doc.page_map.is_empty()
        {
            doc.build_synthetic_page_map(chars);
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_is_page_map_synthetic(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let synthetic = doc_ref(l)
            .is_some_and(|doc| !doc.page_map.is_empty() && doc.page_map_source == "synthetic");
        push_bool(l, synthetic);
    }
    1
}

unsafe extern "C-unwind" fn doc_get_synthetic_chars(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let chars = doc_ref(l).map_or(0, |doc| doc.page_map_chars as i64);
        push_int(l, chars);
    }
    1
}

unsafe extern "C-unwind" fn doc_has_page_map(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let has_map = doc_ref(l).is_some_and(|doc| !doc.page_map.is_empty());
        push_bool(l, has_map);
    }
    1
}

unsafe extern "C-unwind" fn doc_get_page_map(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(doc) = doc_ref(l) else {
            return 0;
        };
        if doc.page_map.is_empty() {
            return 0;
        }
        lua::lua_createtable(l, doc.page_map.len() as c_int, 0);
        for (index, entry) in doc.page_map.iter().enumerate() {
            lua::lua_createtable(l, 0, 4);
            push_str(l, "page");
            push_int(l, entry.page as i64);
            lua::lua_rawset(l, -3);
            push_str(l, "xpointer");
            push_str(l, &entry.xpointer);
            lua::lua_rawset(l, -3);
            push_str(l, "doc_y");
            push_int(l, entry.doc_y);
            lua::lua_rawset(l, -3);
            push_str(l, "label");
            push_str(l, &entry.label);
            lua::lua_rawset(l, -3);
            lua::lua_rawseti_(l, -2, (index + 1) as c_int);
        }
    }
    1
}

unsafe extern "C-unwind" fn doc_get_page_map_source(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(doc) = doc_ref(l) else {
            return 0;
        };
        if doc.page_map_source.is_empty() {
            return 0;
        }
        push_str(l, &doc.page_map_source);
    }
    1
}

unsafe extern "C-unwind" fn doc_get_page_map_first(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let label = doc_ref(l)
            .and_then(|doc| doc.page_map.first())
            .map(|entry| entry.label.clone());
        let Some(label) = label else {
            return 0;
        };
        push_str(l, &label);
    }
    1
}

unsafe extern "C-unwind" fn doc_get_page_map_last(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let label = doc_ref(l)
            .and_then(|doc| doc.page_map.last())
            .map(|entry| entry.label.clone());
        let Some(label) = label else {
            return 0;
        };
        push_str(l, &label);
    }
    1
}

unsafe extern "C-unwind" fn doc_get_page_map_current(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(doc) = doc_ref(l) else {
            return 0;
        };
        let Some(inner) = doc.inner.as_ref() else {
            return 0;
        };
        let max_y = current_document_pos(inner)
            + i64::from(inner.page_height()) * i64::from(doc.visible_page_count.max(1) as u32);
        let Some(index) = doc.page_map_index_for_doc_y_below(max_y) else {
            return 0;
        };
        push_str(l, &doc.page_map[index].label);
        push_int(l, (index + 1) as i64);
        push_int(l, doc.page_map.len() as i64);
    }
    3
}

unsafe extern "C-unwind" fn doc_get_page_map_xpointer(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(xpointer) = arg_str(l, 2) else {
            return 0;
        };
        let Some(doc) = doc_ref(l) else {
            return 0;
        };
        let Some(inner) = doc.inner.as_ref() else {
            return 0;
        };
        let page = match inner.page_from_xpointer(&xpointer) {
            Ok(page) => page,
            Err(_) => return 0,
        };
        let point = inner.point_from_xpointer(&xpointer).ok().flatten();
        let (x, y) = point.unwrap_or((0.0, 0.0));
        let _ = x;
        let xp_y = (inner.page_start_y(page) + y.round() as i64).max(0);
        let Some(index) = doc.page_map_index_for_doc_y_at_or_below(xp_y) else {
            return 0;
        };
        push_str(l, &doc.page_map[index].label);
    }
    1
}

unsafe extern "C-unwind" fn doc_get_page_map_visible(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(doc) = doc_ref(l) else {
            return 0;
        };
        if doc.page_map.is_empty() {
            return 0;
        }
        let Some(inner) = doc.inner.as_ref() else {
            return 0;
        };
        let page_count = inner.page_count();
        let start_page = inner.current_page();
        let visible_count = doc.visible_page_count.max(1) as usize;
        let end_page = (start_page + visible_count).min(page_count + 1);
        let min_y = current_document_pos(inner);
        let max_y = min_y + i64::from(inner.page_height()) * i64::from(visible_count as u32);
        let offset_y = if doc.view_mode == 1 {
            i64::from(doc.header_height.max(0)) + inner.page_margin_y()
        } else {
            0
        };
        let start_index = doc
            .page_map_first_index_at_or_above(min_y)
            .unwrap_or(doc.page_map.len());
        let entries = doc
            .page_map
            .iter()
            .skip(start_index)
            .take_while(|entry| entry.doc_y < max_y)
            .filter(|entry| entry.page >= start_page && entry.page < end_page)
            .collect::<Vec<_>>();
        lua::lua_createtable(l, entries.len() as c_int, 0);
        for (index, entry) in entries.iter().enumerate() {
            lua::lua_createtable(l, 0, 6);
            push_str(l, "screen_page");
            push_int(l, (entry.page - start_page + 1) as i64);
            lua::lua_rawset(l, -3);
            push_str(l, "screen_y");
            push_int(l, entry.doc_y - inner.page_start_y(entry.page) + offset_y);
            lua::lua_rawset(l, -3);
            push_str(l, "page");
            push_int(l, entry.page as i64);
            lua::lua_rawset(l, -3);
            push_str(l, "xpointer");
            push_str(l, &entry.xpointer);
            lua::lua_rawset(l, -3);
            push_str(l, "doc_y");
            push_int(l, entry.doc_y);
            lua::lua_rawset(l, -3);
            push_str(l, "label");
            push_str(l, &entry.label);
            lua::lua_rawset(l, -3);
            lua::lua_rawseti_(l, -2, (index + 1) as c_int);
        }
    }
    1
}

unsafe extern "C-unwind" fn doc_get_page_links(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let internal_only = arg_bool(l, 2);
        let links = doc_ref(l).and_then(|doc| {
            let inner = doc.inner.as_ref()?;
            inner.page_links(inner.current_page(), internal_only).ok()
        });
        let Some(links) = links else {
            lua::lua_newtable(l);
            return 1;
        };

        lua::lua_createtable(l, links.len() as c_int, 0);
        for (index, link) in links.iter().enumerate() {
            let start_x = link
                .rects
                .iter()
                .map(|rect| rect.x)
                .fold(f32::INFINITY, f32::min);
            let start_y = link
                .rects
                .iter()
                .map(|rect| rect.y)
                .fold(f32::INFINITY, f32::min);
            let end_x = link
                .rects
                .iter()
                .map(|rect| rect.x + rect.width)
                .fold(f32::NEG_INFINITY, f32::max);
            let end_y = link
                .rects
                .iter()
                .map(|rect| rect.y + rect.height)
                .fold(f32::NEG_INFINITY, f32::max);

            lua::lua_createtable(l, 0, 6);
            for (key, value) in [
                ("start_x", start_x),
                ("start_y", start_y),
                ("end_x", end_x),
                ("end_y", end_y),
            ] {
                push_str(l, key);
                push_int(l, value.round() as i64);
                lua::lua_rawset(l, -3);
            }
            push_str(l, if link.internal { "section" } else { "uri" });
            push_str(l, &link.href);
            lua::lua_rawset(l, -3);

            push_str(l, "segments");
            lua::lua_createtable(l, link.rects.len() as c_int, 0);
            for (segment_index, rect) in link.rects.iter().enumerate() {
                lua::lua_createtable(l, 0, 4);
                for (key, value) in [
                    ("x0", rect.x),
                    ("y0", rect.y),
                    ("x1", rect.x + rect.width),
                    ("y1", rect.y + rect.height),
                ] {
                    push_str(l, key);
                    push_int(l, value.round() as i64);
                    lua::lua_rawset(l, -3);
                }
                lua::lua_rawseti_(l, -2, (segment_index + 1) as c_int);
            }
            lua::lua_rawset(l, -3);
            lua::lua_rawseti_(l, -2, (index + 1) as c_int);
        }
    }
    1
}

unsafe extern "C-unwind" fn doc_get_text_from_xpointer(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let text = arg_str(l, 2).and_then(|xpointer| {
            doc_ref(l)
                .and_then(|doc| doc.inner.as_ref())
                .and_then(|inner| inner.text_from_xpointer(&xpointer).ok())
        });
        let Some(text) = text else {
            return 0;
        };
        push_str(l, &text);
    }
    1
}

unsafe extern "C-unwind" fn doc_normalized_xpointer(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(xpointer) = arg_str(l, 2) else {
            push_bool(l, false);
            return 1;
        };
        let normalized =
            cre_lib::parse_text_position(&xpointer).map(|position| position.to_xpointer());
        if let Ok(normalized) = normalized {
            push_str(l, &normalized);
        } else {
            push_bool(l, false);
        }
    }
    1
}

unsafe extern "C-unwind" fn doc_compare_xpointers(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(left) = arg_str(l, 2) else {
            return 0;
        };
        let Some(right) = arg_str(l, 3) else {
            return 0;
        };
        let result = doc_ref(l)
            .and_then(|doc| doc.inner.as_ref())
            .and_then(|inner| inner.compare_xpointers(&left, &right).ok())
            .unwrap_or(0);
        push_int(l, i64::from(result));
    }
    1
}

unsafe extern "C-unwind" fn doc_visible_page_count(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let count = doc_ref(l).map_or(1, |doc| i64::from(doc.visible_page_count.max(1)));
        push_int(l, count);
    }
    1
}

unsafe extern "C-unwind" fn doc_get_visible_page_number_count(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let count = doc_ref(l).map_or(1, |doc| doc.visible_page_number_count());
        push_int(l, count);
    }
    1
}

unsafe extern "C-unwind" fn doc_render_hash(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        if let Some(doc) = doc_ref(l) {
            doc.width.hash(&mut hasher);
            doc.height.hash(&mut hasher);
            doc.font_size.hash(&mut hasher);
            doc.font_face.hash(&mut hasher);
            doc.margins.hash(&mut hasher);
            doc.view_mode.hash(&mut hasher);
            doc.embedded_stylesheet_enabled.hash(&mut hasher);
            doc.stylesheet.hash(&mut hasher);
            doc.path.hash(&mut hasher);
            doc.layout_height.hash(&mut hasher);
            doc.font_path.hash(&mut hasher);
            doc.visible_page_count.hash(&mut hasher);
            doc.header_height.hash(&mut hasher);
            doc.interline.hash(&mut hasher);
            doc.background_color.hash(&mut hasher);
            doc.background_color_value.hash(&mut hasher);
            doc.background_image.hash(&mut hasher);
            doc.render_dpi.hash(&mut hasher);
            doc.scale_font_with_dpi.hash(&mut hasher);
            doc.effective_font_size().hash(&mut hasher);
            let mut selected_property_keys = doc
                .properties
                .keys()
                .filter(|name| {
                    name.starts_with("crengine.")
                        || name.starts_with("font.")
                        || name.starts_with("header.")
                        || name.starts_with("window.pages.")
                })
                .cloned()
                .collect::<Vec<String>>();
            selected_property_keys.sort();
            for key in selected_property_keys {
                let value = doc.properties.get(&key).cloned().unwrap_or_default();
                (key, value).hash(&mut hasher);
            }
        }
        push_int(l, hasher.finish() as i64);
    }
    1
}

fn hex_digit(value: u8) -> u32 {
    match value {
        b'0'..=b'9' => (value - b'0') as u32,
        b'a'..=b'f' => (value - b'a') as u32 + 10,
        b'A'..=b'F' => (value - b'A') as u32 + 10,
        _ => 0,
    }
}

fn parse_lua_color(text: String) -> u32 {
    let raw = text.trim();
    let digits = if let Some(stripped) = raw.strip_prefix("#") {
        stripped.to_string()
    } else if let Some(stripped) = raw.strip_prefix("0x") {
        stripped.to_string()
    } else if let Some(stripped) = raw.strip_prefix("0X") {
        stripped.to_string()
    } else {
        raw.to_string()
    };
    let digits = if let Some(stripped) = digits.strip_prefix("#") {
        stripped.to_string()
    } else {
        digits
    };
    let bytes = digits.as_bytes();
    if bytes.len() == 3 && bytes.iter().all(|byte| byte.is_ascii_hexdigit()) {
        let r = hex_digit(bytes[0]);
        let g = hex_digit(bytes[1]);
        let b = hex_digit(bytes[2]);
        return (((r * 0x11) << 16) | ((g * 0x11) << 8) | (b * 0x11)) & 0xFF_FFFF;
    }
    if bytes.len() == 6 && bytes.iter().all(|byte| byte.is_ascii_hexdigit()) {
        return bytes
            .iter()
            .fold(0_u32, |acc, byte| (acc << 4) | hex_digit(*byte))
            & 0xFF_FFFF;
    }
    0xFF_FFFF
}

fn arg_color(l: *mut lua_State, index: c_int) -> u32 {
    unsafe {
        if lua::lua_isnoneornil(l, index) != 0 {
            return 0xFF_FFFF;
        }
        if lua::lua_isstring(l, index) != 0 {
            return parse_lua_color(arg_str(l, index).unwrap_or_default());
        }
        (arg_int(l, index) & 0xFF_FFFFi64) as u32
    }
}

unsafe extern "C-unwind" fn doc_set_background_color(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        if let Some(doc) = doc_mut(l) {
            doc.background_color_value = arg_color(l, 2);
            if let Some(inner) = doc.inner.as_mut() {
                inner.set_background_color(doc.background_color_value);
            }
        }
    }
    0
}

unsafe extern "C-unwind" fn doc_check_regex(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    unsafe {
        let Some(pattern) = arg_str(l, 2) else {
            REGEX_SEARCH_ERROR.store(REGEX_SEARCH_ERROR_INVALID_PATTERN, Ordering::Relaxed);
            push_int(l, i64::from(REGEX_SEARCH_ERROR_INVALID_PATTERN));
            return 1;
        };
        let error = match regex::Regex::new(&pattern) {
            Ok(_) => REGEX_SEARCH_ERROR_NONE,
            Err(_) => REGEX_SEARCH_ERROR_INVALID_PATTERN,
        };
        REGEX_SEARCH_ERROR.store(error, Ordering::Relaxed);
        push_int(l, i64::from(error));
    }
    1
}

unsafe extern "C-unwind" fn doc_get_and_clear_regex_search_error(l: *mut lua_State) -> c_int {
    // SAFETY: `l` is the active Lua state.
    let error = REGEX_SEARCH_ERROR.swap(REGEX_SEARCH_ERROR_NONE, Ordering::Relaxed);
    unsafe { push_int(l, i64::from(error)) };
    1
}

unsafe extern "C-unwind" fn doc_read_defaults(_l: *mut lua_State) -> c_int {
    0
}

unsafe extern "C-unwind" fn doc_save_defaults(_l: *mut lua_State) -> c_int {
    0
}

macro_rules! reg {
    ($name:literal, $func:path) => {
        lua::luaL_Reg {
            name: concat!($name, "\0").as_ptr().cast::<c_char>(),
            func: $func,
        }
    };
}

macro_rules! regs {
    ($(($name:literal, $func:path)),* $(,)?) => {
        [
            $(reg!($name, $func),)*
            lua::luaL_Reg {
                name: ptr::null(),
                func: stub,
            },
        ]
    };
}

const IMAGE_METHODS: &[lua::luaL_Reg] = &regs![
    ("renderScaled", image_render_scaled),
    ("free", image_free),
    ("__gc", image_gc),
];

const DOC_METHODS: &[lua::luaL_Reg] = &regs![
    ("loadDocument", doc_load),
    ("renderDocument", doc_render),
    ("requestRender", doc_request_render),
    ("getIntProperty", doc_get_int_property),
    ("getStringProperty", doc_get_string_property),
    ("getDocumentFormat", doc_format),
    ("getDocumentProps", doc_props),
    ("setAltDocumentProp", doc_set_alt_prop),
    ("getDocumentRenderingHash", doc_render_hash),
    ("canBePartiallyRerendered", doc_can_partial_rerender),
    ("isPartialRerenderingEnabled", doc_is_partial_enabled),
    ("enablePartialRerendering", doc_enable_partial),
    ("getPartialRerenderingsCount", doc_partial_count),
    ("isRerenderingDelayed", doc_rerender_delayed),
    ("getPages", doc_pages),
    ("getCurrentPage", doc_current_page),
    ("getPageFlow", doc_get_page_flow),
    ("getPageFromXPointer", doc_page_from_xpointer),
    ("getPosFromXPointer", doc_pos_from_xpointer),
    ("getCurrentPos", doc_current_pos),
    ("getCurrentPercent", doc_current_percent),
    ("getXPointer", doc_xpointer),
    ("getPageXPointer", doc_page_xpointer),
    ("getEPubCFI", doc_epub_cfi),
    ("getPageEPubCFI", doc_page_epub_cfi),
    ("getEPubCFIFromXPointer", doc_epub_cfi_from_xpointer),
    ("getEPubCFIRangeFromXPointers", doc_epub_cfi_range),
    ("getPageOffsetX", doc_get_page_offset_x),
    ("getPageStartY", doc_page_start_y),
    ("getPageHeight", doc_page_height),
    ("getFullHeight", doc_full_height),
    ("getFontSize", doc_font_size),
    ("getFontFace", doc_font_face),
    ("getEmbeddedFontList", doc_get_embedded_font_list),
    ("getPageMargins", doc_page_margins),
    ("getHeaderHeight", doc_get_header_height),
    ("getToc", doc_toc),
    ("getVisiblePageCount", doc_visible_page_count),
    (
        "getVisiblePageNumberCount",
        doc_get_visible_page_number_count
    ),
    ("getNextVisibleWordStart", doc_next_visible_word_start),
    ("getNextVisibleWordEnd", doc_next_visible_word_end),
    ("getPrevVisibleWordStart", doc_prev_visible_word_start),
    ("getPrevVisibleWordEnd", doc_prev_visible_word_end),
    ("getPrevVisibleChar", doc_prev_visible_char),
    ("getNextVisibleChar", doc_next_visible_char),
    ("getTextFromXPointers", doc_text_from_xpointers),
    ("compareXPointers", doc_compare_xpointers),
    ("setIntProperty", doc_set_int_property),
    ("setStringProperty", doc_set_string_property),
    ("setViewMode", doc_set_view_mode),
    ("setViewDimen", doc_set_view_dimen),
    ("setHeaderInfo", doc_set_header_info),
    ("setPageInfoOverride", doc_set_page_info_override),
    ("setHeaderFont", doc_set_header_font),
    ("setHeaderProgressMarks", doc_set_header_progress),
    ("setFontFace", doc_set_font_face),
    ("setFontSize", doc_set_font_size),
    ("setDefaultInterlineSpace", doc_set_default_interline),
    ("setStyleSheet", doc_set_stylesheet),
    ("setEmbeddedStyleSheet", doc_set_embedded_stylesheet),
    ("setEmbeddedFonts", doc_set_embedded_fonts),
    ("setBackgroundColor", doc_set_background_color),
    ("setBackgroundImage", doc_set_background_image),
    ("setPageMargins", doc_set_page_margins),
    ("setVisiblePageCount", doc_set_visible_page_count),
    ("adjustFontSizes", doc_adjust_font_sizes),
    ("setBatteryState", doc_set_battery_state),
    ("setCallback", doc_set_callback),
    ("isBuiltDomStale", doc_cache_false),
    ("hasCacheFile", doc_cache_false),
    ("isCacheFileStale", doc_cache_false),
    ("invalidateCacheFile", doc_invalidate_cache),
    ("getCacheFilePath", doc_cache_path),
    ("updateTocAndPageMap", doc_update_toc),
    ("getStatistics", doc_statistics),
    ("getUnknownEntities", doc_unknown_entities),
    ("buildAlternativeToc", doc_build_alternative_toc),
    ("isTocAlternativeToc", doc_is_alternative_toc),
    ("gotoPage", doc_goto_page),
    ("gotoPercent", doc_goto_percent),
    ("gotoPos", doc_goto_pos),
    ("gotoXPointer", doc_goto_xpointer),
    ("zoomFont", doc_font_size),
    ("drawCurrentPage", doc_draw_current_page),
    ("findText", doc_find_text),
    ("findAllText", doc_find_all_text),
    ("isXPointerInCurrentPage", doc_xpointer_in_current_page),
    ("isXPointerInDocument", doc_xpointer_in_document),
    ("getLinkFromPosition", doc_link_from_position),
    ("getWordFromPosition", doc_word_from_position),
    ("getTextFromPositions", doc_text_from_positions),
    ("extendXPointersToSentenceSegment", doc_extend_sentence),
    ("getWordBoxesFromPositions", doc_word_boxes_from_positions),
    ("getImageDataFromPosition", doc_get_image_data),
    ("getNearestWordFromPosition", doc_nearest_word_from_position),
    ("getLangTagForTextFromPosition", doc_get_lang_tag),
    ("getDocumentFileContent", doc_get_document_file_content),
    ("getTextFromXPointer", doc_get_text_from_xpointer),
    ("getHTMLFromXPointer", doc_get_html_from_xpointer),
    ("getHTMLFromXPointers", doc_get_html_from_xpointers),
    (
        "getStylesheetsMatchingRulesets",
        doc_get_stylesheets_matching_rulesets
    ),
    ("getPageLinks", doc_get_page_links),
    ("isLinkToFootnote", doc_is_link_to_footnote),
    ("highlightXPointer", doc_highlight),
    ("getNormalizedXPointer", doc_normalized_xpointer),
    ("getCoverPageImageData", doc_get_cover_image_data),
    ("gotoLink", doc_goto_link),
    ("goBack", doc_go_back),
    ("goForward", doc_go_forward),
    ("clearSelection", doc_clear_selection),
    ("hasPageMap", doc_has_page_map),
    ("getPageMap", doc_get_page_map),
    ("getPageMapSource", doc_get_page_map_source),
    ("getPageMapFirstPageLabel", doc_get_page_map_first),
    ("getPageMapLastPageLabel", doc_get_page_map_last),
    ("getPageMapCurrentPageLabel", doc_get_page_map_current),
    ("getPageMapXPointerPageLabel", doc_get_page_map_xpointer),
    ("getPageMapVisiblePageLabels", doc_get_page_map_visible),
    (
        "buildSyntheticPageMapIfNoneDocumentProvided",
        doc_build_synthetic_if_none
    ),
    ("isPageMapSynthetic", doc_is_page_map_synthetic),
    ("buildSyntheticPageMap", doc_build_synthetic_page_map),
    ("getSyntheticPageMapCharsPerPage", doc_get_synthetic_chars),
    (
        "hasPageMapDocumentProvided",
        doc_has_page_map_document_provided
    ),
    (
        "isPageMapDocumentProvided",
        doc_is_page_map_document_provided
    ),
    ("hasNonLinearFlows", doc_has_non_linear_flows),
    ("checkRegex", doc_check_regex),
    (
        "getAndClearRegexSearchError",
        doc_get_and_clear_regex_search_error
    ),
    ("readDefaults", doc_read_defaults),
    ("saveDefaults", doc_save_defaults),
    ("close", doc_close),
    ("__gc", doc_gc),
];

const MODULE_FUNCS: &[lua::luaL_Reg] = &regs![
    ("initCache", module_init_cache),
    ("initHyphDict", module_init_hyph_dict),
    ("newDocView", new_doc_view),
    ("getFontFaces", module_get_font_faces),
    ("getFontFaceFilenameAndFaceIndex", module_font_face_filename),
    ("getFontFaceAvailableWeights", module_font_face_weights),
    ("getGammaLevel", module_get_gamma_level),
    ("getGammaIndex", module_get_gamma_index),
    ("setGammaIndex", module_set_gamma_index),
    ("registerFont", module_register_font),
    (
        "regularizeRegisteredFontsWeights",
        module_regularize_registered_font_weights
    ),
    ("setAsPreferredFontWithBias", module_set_preferred_font),
    ("getHyphDictList", module_get_hyph_dict_list),
    ("getSelectedHyphDict", module_get_selected_hyph_dict),
    ("setHyphDictionary", module_set_hyph_dict),
    ("getTextLangStatus", module_get_text_lang_status),
    ("getLatestDomVersion", module_dom_version),
    ("getDomVersionWithNormalizedXPointers", module_dom_version),
    ("setUserHyphenationDict", module_set_user_hyph),
    ("getHyphenationForWord", module_get_hyphenation),
    ("softHyphenateText", module_soft_hyphenate),
    ("renderImageData", module_render_image_data),
    ("getBalancedHTML", module_get_balanced_html),
    ("smoothScaleBlitBuffer", module_smooth_scale),
    ("setImageReplacementChar", module_set_image_replacement_char),
    ("setSkipTearDown", module_noop),
];

/// Opens the `libkoreader-cre` Lua module.
///
/// # Safety
///
/// `l` must be a valid `lua_State` supplied by the LuaJIT module loader.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn luaopen_cre(l: *mut lua_State) -> c_int {
    // Baseline of the whole interpreter before any book is opened; the
    // difference to "[doc open]" is the memory our module is responsible for.
    cre_lib::trace_memory("module load");
    // SAFETY: `l` is a valid Lua state supplied by the LuaJIT module loader.
    unsafe {
        lua::luaL_newmetatable_(l, CREDOC.as_ptr().cast());
        lua::lua_pushvalue(l, -1);
        lua::lua_setfield(l, -2, INDEX.as_ptr().cast());
        lua::luaL_register(l, ptr::null(), DOC_METHODS.as_ptr());
        lua::lua_pop(l, 1);

        lua::luaL_newmetatable_(l, CREIMAGE.as_ptr().cast());
        lua::lua_pushvalue(l, -1);
        lua::lua_setfield(l, -2, INDEX.as_ptr().cast());
        lua::luaL_register(l, ptr::null(), IMAGE_METHODS.as_ptr());
        lua::lua_pop(l, 1);

        lua::lua_newtable(l);
        lua::luaL_register(l, ptr::null(), MODULE_FUNCS.as_ptr());
        install_call_trace(l);
    }
    1
}

/// Optional call breadcrumb for diagnosing a hang/kill on a device.
///
/// With CRE_LUA_TRACE_PATH=<file> (or CRE_LUA_TRACE=1, default
/// /tmp/cre-lua-calls.log), every exported doc/image/module function is
/// wrapped by a Lua shim that appends its name to the file before calling
/// through. The last line before a SIGKILL names the call that was running,
/// which stderr/stdout logs cannot show once the process is killed.
fn install_call_trace(l: *mut lua_State) {
    unsafe {
        let Ok(path) =
            std::env::var("CRE_LUA_TRACE_PATH").or_else(|_| std::env::var("CRE_LUA_TRACE"))
        else {
            return;
        };
        let path = if path.is_empty() || path == "1" {
            "/tmp/cre-lua-calls.log".to_string()
        } else {
            path
        };
        const SHIM: &str = r#"
local path, cre, doc_mt, img_mt = ...
local function wrap(tbl, tag)
  if type(tbl) ~= "table" then return end
  local names = {}
  for k, v in pairs(tbl) do
    if type(k) == "string" and type(v) == "function" and k:sub(1, 2) ~= "__" then
      names[#names + 1] = k
    end
  end
  for _, k in ipairs(names) do
    local inner = tbl[k]
    tbl[k] = function(...)
      local fh = io.open(path, "a")
      if fh then fh:write(tag, " ", k, "\n"); fh:close() end
      return inner(...)
    end
  end
end
wrap(cre, "cre")
wrap(doc_mt, "doc")
wrap(img_mt, "img")
"#;
        let Ok(chunk) = std::ffi::CString::new(SHIM) else {
            return;
        };
        eprintln!("cre-lua: call trace enabled -> {}", path);
        let module_index = lua::lua_gettop(l);
        if lua::luaL_loadstring(l, chunk.as_ptr()) != 0 {
            let mut len = 0usize;
            let msg = lua::lua_tolstring(l, -1, &mut len);
            let text = if msg.is_null() {
                "unknown".to_string()
            } else {
                std::ffi::CStr::from_ptr(msg).to_string_lossy().into_owned()
            };
            eprintln!("cre-lua: call trace chunk failed to load: {text}");
            lua::lua_pop(l, 1);
            return;
        }
        push_str(l, &path);
        lua::lua_pushvalue(l, module_index);
        lua::luaL_getmetatable(l, CREDOC.as_ptr().cast());
        lua::luaL_getmetatable(l, CREIMAGE.as_ptr().cast());
        if lua::lua_pcall(l, 4, 0, 0) != 0 {
            eprintln!("cre-lua: call trace install failed");
            lua::lua_pop(l, 1);
        }
    }
}

/// Compatibility fallback for devices whose libc predates glibc 2.16
/// (e.g. Kindle Paperwhite 2, glibc 2.12).
///
/// Rust std calls `getauxval(AT_MINSIGSTKSZ)` from `sigstack_size()` while
/// setting up the alternate signal stack for every thread, but glibc only
/// started exporting `getauxval` in 2.16. Without this definition the module
/// cannot be loaded on such devices ("undefined symbol: getauxval").
///
/// Only compiled with the `old-glibc` feature (Kindle cross builds). The
/// implementation matches glibc semantics (0 when the entry is absent), so
/// exposing it harmlessly shadows the newer libc function on newer devices.
#[cfg(feature = "old-glibc")]
mod old_glibc {
    use std::os::raw::c_ulong;

    /// Reads the process auxiliary vector, mirroring glibc's `getauxval`.
    ///
    /// # Safety
    /// Matches `getauxval`'s C contract; no pointers are dereferenced.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn getauxval(kind: c_ulong) -> c_ulong {
        // AUXV entries are pairs of native-word values terminated by AT_NULL.
        // This shim is only built for 32-bit ARM Linux (ARMv7) targets.
        let Ok(bytes) = std::fs::read("/proc/self/auxv") else {
            return 0;
        };
        let word = std::mem::size_of::<c_ulong>();
        let mut offset = 0usize;
        while offset + word * 2 <= bytes.len() {
            let mut a_type = [0u8; std::mem::size_of::<c_ulong>()];
            let mut a_val = [0u8; std::mem::size_of::<c_ulong>()];
            a_type.copy_from_slice(&bytes[offset..offset + word]);
            a_val.copy_from_slice(&bytes[offset + word..offset + 2 * word]);
            let entry_type = c_ulong::from_ne_bytes(a_type);
            if entry_type == 0 {
                break;
            }
            if entry_type == kind {
                return c_ulong::from_ne_bytes(a_val);
            }
            offset += word * 2;
        }
        0
    }
}

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use anyrender::recording::RenderCommand;
use anyrender::{Paint, Scene};
use blitz_dom::{DocumentConfig, Node, NodeId, StyleThreading, build_single_font_ctx};
use blitz_html::HtmlDocument;
use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request};
use blitz_traits::shell::{ColorScheme, Viewport};
use cre_epub::{EpubBook, HtmlOptions, TocItem, normalize_path};
use cre_fb2::{Fb2Book, Fb2HtmlOptions};
use cre_paint::render_page;
use cre_text::{TextBook, TextHtmlOptions};
use percent_encoding::percent_decode_str;

#[derive(Debug)]
struct Args {
    input: PathBuf,
    out_dir: PathBuf,
    max_pages: usize,
    width: u32,
    height: u32,
    toc: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Format {
    Epub,
    Fb2,
    Txt,
}

fn detect_format(path: &Path) -> Format {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "epub" => Format::Epub,
        "fb2" => Format::Fb2,
        "txt" => Format::Txt,
        _ => Format::Epub,
    }
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let format = detect_format(&args.input);
    let mut toc_entries: Vec<TocItem> = Vec::new();

    let (html, base_url, net, title, creator): (
        String,
        String,
        Arc<dyn NetProvider>,
        String,
        String,
    ) = match format {
        Format::Epub => {
            let book = EpubBook::open(&args.input)?;
            let t = book.title.clone().unwrap_or_else(|| "<untitled>".into());
            let c = book.creator.clone().unwrap_or_else(|| "<unknown>".into());
            println!("format: epub");
            println!("title: {t}");
            println!("creator: {c}");
            println!("spine items: {}", book.spine.len());
            let html = book.to_html(&HtmlOptions::default())?;
            let res = Arc::new(book.resources.clone());
            toc_entries = book.toc.clone();
            (
                html,
                "epub://book/".into(),
                Arc::new(EpubNet { resources: res }),
                t,
                c,
            )
        }
        Format::Fb2 => {
            let book = Fb2Book::open(&args.input)?;
            let t = book.title.clone().unwrap_or_else(|| "<untitled>".into());
            let c = book.creator.clone().unwrap_or_else(|| "<unknown>".into());
            println!("format: fb2");
            println!("title: {t}");
            println!("creator: {c}");
            println!("binary items: {}", book.binary.len());
            let html = book.to_html(&Fb2HtmlOptions::default())?;
            let res = Arc::new(book.binary.clone());
            (
                html,
                "fb2://book/".into(),
                Arc::new(Fb2Net { resources: res }),
                t,
                c,
            )
        }
        Format::Txt => {
            let book = TextBook::open(&args.input)?;
            let t = book.title.clone().unwrap_or_else(|| "<untitled>".into());
            println!("format: txt");
            println!("title: {t}");
            let html = book.to_html(&TextHtmlOptions::default());
            (
                html,
                "text://book/".into(),
                Arc::new(EmptyNet),
                t,
                String::new(),
            )
        }
    };

    let _ = (&title, &creator);
    println!("html bytes: {}", html.len());

    let font_data = std::fs::read(
        std::env::var("CRE_FONT_PATH")
            .unwrap_or_else(|_| "/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf".to_string()),
    )
    .context("read font")?;

    let layout_height = 100_000u32;
    let config = DocumentConfig {
        viewport: Some(Viewport::new(
            args.width,
            layout_height,
            1.0,
            ColorScheme::Light,
        )),
        base_url: Some(base_url),
        net_provider: Some(net),
        font_ctx: Some(build_single_font_ctx(&font_data)),
        style_threading: StyleThreading::Sequential,
        ..Default::default()
    };

    let mut doc = HtmlDocument::from_html(&html, config);
    doc.resolve(0.0);
    println!("layout resolved");

    // Process async resource messages (images, stylesheets) by running extra resolve passes.
    for _pass in 0..4 {
        doc.handle_messages();
        doc.resolve(0.0);
    }

    let lines = collect_lines(doc.root_element());
    println!("line boxes: {}", lines.len());
    if let (Some(first), Some(last)) = (lines.first(), lines.last()) {
        println!("content y range: {:.1}..{:.1}", first.top, last.bottom);
    }

    let page_starts = paginate(&lines, args.height as f32, 32.0, 32.0);
    println!("pages: {}", page_starts.len());

    if args.toc {
        print_toc_pages(&doc, &toc_entries, &page_starts);
    }

    std::fs::create_dir_all(&args.out_dir)
        .with_context(|| format!("create {}", args.out_dir.display()))?;

    let mut scene = Scene::default();
    blitz_paint::paint_scene(&mut scene, &mut doc, 1.0, args.width, layout_height, 0, 0);
    let fill_count = scene
        .commands
        .iter()
        .filter(|c| matches!(c, RenderCommand::Fill(_)))
        .count();
    let img_count = scene
        .commands
        .iter()
        .filter(|c| matches!(c, RenderCommand::Fill(cmd) if matches!(&cmd.brush, Paint::Image(_))))
        .count();
    let glyph_count = scene
        .commands
        .iter()
        .filter(|c| matches!(c, RenderCommand::GlyphRun(_)))
        .count();
    println!(
        "paint commands: {} (fills: {} images: {} glyphs: {})",
        scene.commands.len(),
        fill_count,
        img_count,
        glyph_count
    );

    let pages_to_render = args.max_pages.min(page_starts.len());
    for (index, page) in page_starts.iter().take(pages_to_render).enumerate() {
        let clip_bottom = page_starts
            .get(index + 1)
            .map(|next| next.clip_top)
            .unwrap_or(page.top + args.height as f32 - 32.0);
        let bitmap = render_page(
            &scene,
            args.width,
            args.height,
            page.top,
            page.clip_top,
            clip_bottom,
        );
        let path = args.out_dir.join(format!("page-{:03}.pgm", index + 1));
        bitmap.write_pgm(&path)?;
        println!("rendered page {} -> {}", index + 1, path.display());
    }

    Ok(())
}

fn parse_args() -> Result<Args> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let toc = raw.iter().any(|arg| arg == "--toc");
    let mut it = raw.into_iter().filter(|arg| arg != "--toc");
    let input = it
        .next()
        .map(PathBuf::from)
        .context("usage: cre-render [--toc] <file> <out-dir> [max-pages] [width] [height]")?;
    let out_dir = it
        .next()
        .map(PathBuf::from)
        .context("usage: cre-render [--toc] <file> <out-dir> [max-pages] [width] [height]")?;
    let max_pages = it
        .next()
        .map(|s| s.parse())
        .transpose()
        .context("max-pages must be integer")?
        .unwrap_or(3);
    let width = it
        .next()
        .map(|s| s.parse())
        .transpose()
        .context("width must be integer")?
        .unwrap_or(600u32);
    let height = it
        .next()
        .map(|s| s.parse())
        .transpose()
        .context("height must be integer")?
        .unwrap_or(800u32);
    if width == 0 || height == 0 {
        bail!("width and height must be positive");
    }
    Ok(Args {
        input,
        out_dir,
        max_pages,
        width,
        height,
        toc,
    })
}

struct LineBox {
    top: f32,
    bottom: f32,
}

fn collect_lines(root: &Node) -> Vec<LineBox> {
    let mut lines = Vec::new();
    collect_lines_inner(root, &mut lines);
    lines.sort_by(|a, b| a.top.total_cmp(&b.top));
    lines
}

fn collect_lines_inner(node: &Node, out: &mut Vec<LineBox>) {
    if let Some(element) = node.element_data()
        && let Some(inline_layout) = element.inline_layout_data.as_ref()
    {
        let root_layout = node.unrounded_layout();
        let inset = root_layout.padding + root_layout.border;
        let origin_y = inset.top + inline_layout.block_offset;
        for line in inline_layout.layout.lines() {
            let metrics = line.metrics();
            let top = node
                .absolute_position(inset.left, origin_y + metrics.block_min_coord)
                .y;
            let bottom = node
                .absolute_position(inset.left, origin_y + metrics.block_max_coord)
                .y;
            out.push(LineBox { top, bottom });
        }
    }
    if let Some(layout_children) = node.layout_children.borrow().as_ref() {
        for child in layout_children.iter() {
            collect_lines_inner(node.with(*child), out);
        }
    }
}

struct PageStart {
    top: f32,
    clip_top: f32,
}

fn paginate(
    lines: &[LineBox],
    page_height: f32,
    margin_top: f32,
    margin_bottom: f32,
) -> Vec<PageStart> {
    let mut starts = vec![PageStart {
        top: 0.0,
        clip_top: margin_top,
    }];
    for line in lines {
        let current = starts.last().unwrap();
        let clip_bottom = current.top + page_height - margin_bottom;
        if line.bottom > clip_bottom && line.top > current.clip_top {
            starts.push(PageStart {
                top: line.top - margin_top,
                clip_top: line.top,
            });
        }
    }
    starts
}

struct TocAnchorId {
    resource: String,
    fragment: Option<String>,
    node_id: NodeId,
}

fn print_toc_pages(doc: &HtmlDocument, toc: &[TocItem], page_starts: &[PageStart]) {
    println!("toc entries: {}", toc.len());
    if toc.is_empty() {
        return;
    }

    let mut ids = Vec::new();
    collect_toc_anchors(doc.root_element(), None, &mut ids);
    let mut anchors = HashMap::<(String, Option<String>), NodeId>::new();
    for anchor in ids {
        anchors
            .entry((anchor.resource, anchor.fragment))
            .or_insert(anchor.node_id);
    }

    for item in toc {
        let (path, fragment) = split_toc_href(&item.href);
        let path = normalize_path(&percent_decode_str(path).decode_utf8_lossy());
        let fragment = fragment.map(|fragment| {
            percent_decode_str(fragment)
                .decode_utf8_lossy()
                .into_owned()
        });
        let node_id = anchors
            .get(&(path.clone(), fragment.clone()))
            .or_else(|| anchors.get(&(path.clone(), None)))
            .copied();
        let page = node_id.and_then(|node_id| page_for_node(doc, node_id, page_starts));

        println!(
            "depth={} page={} target={} title={}",
            item.depth,
            page.map_or_else(|| "?".to_string(), |page| page.to_string()),
            item.href,
            item.title
        );
    }
}

fn collect_toc_anchors(node: &Node, current_resource: Option<String>, out: &mut Vec<TocAnchorId>) {
    let mut resource = current_resource;

    if let Some(href) = node_attr(node, "data-spine-href") {
        let href = normalize_path(&percent_decode_str(href).decode_utf8_lossy());
        out.push(TocAnchorId {
            resource: href.clone(),
            fragment: None,
            node_id: node.id,
        });
        resource = Some(href);
    }

    if let Some(resource) = resource.as_deref() {
        if let Some(id) = element_id(node) {
            out.push(TocAnchorId {
                resource: resource.to_string(),
                fragment: Some(id),
                node_id: node.id,
            });
        }
        if let Some(name) = node_attr(node, "name") {
            out.push(TocAnchorId {
                resource: resource.to_string(),
                fragment: Some(name.to_string()),
                node_id: node.id,
            });
        }
    }

    for child in node.children.iter() {
        collect_toc_anchors(node.with(*child), resource.clone(), out);
    }
}

fn node_attr<'a>(node: &'a Node, name: &str) -> Option<&'a str> {
    node.attrs()?
        .iter()
        .find(|attr| attr.name.local.to_string() == name)
        .map(|attr| attr.value.as_str())
}

fn element_id(node: &Node) -> Option<String> {
    node.element_data()
        .and_then(|element| element.id.as_ref())
        .map(|id| id.to_string())
}

fn split_toc_href(href: &str) -> (&str, Option<&str>) {
    match href.find('#') {
        Some(index) => (&href[..index], Some(&href[index + 1..])),
        None => (href, None),
    }
}

fn page_for_node(doc: &HtmlDocument, node_id: NodeId, page_starts: &[PageStart]) -> Option<usize> {
    let top = doc.get_client_bounding_rect(node_id)?.y as f32;
    let mut page = 1usize;
    for (index, start) in page_starts.iter().enumerate() {
        if top + 1.0 >= start.clip_top {
            page = index + 1;
        } else {
            break;
        }
    }
    Some(page)
}

struct EpubNet {
    resources: Arc<HashMap<String, Vec<u8>>>,
}

impl NetProvider for EpubNet {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let path = percent_decode_str(request.url.path())
            .decode_utf8_lossy()
            .trim_start_matches('/')
            .to_string();
        let key = cre_epub::normalize_path(&path);
        let bytes = self.resources.get(&key).cloned().unwrap_or_default();
        handler.bytes(request.url.to_string(), Bytes::from(bytes));
    }
}

struct Fb2Net {
    resources: Arc<HashMap<String, Vec<u8>>>,
}

impl NetProvider for Fb2Net {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        // FB2 images use fb2://binary/NAME URLs
        let path = percent_decode_str(request.url.path())
            .decode_utf8_lossy()
            .trim_start_matches('/')
            .to_string();
        let name = path.strip_prefix("binary/").unwrap_or(&path).to_string();
        let bytes = self.resources.get(&name).cloned().unwrap_or_default();
        handler.bytes(request.url.to_string(), Bytes::from(bytes));
    }
}

struct EmptyNet;

impl NetProvider for EmptyNet {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        handler.bytes(request.url.to_string(), Bytes::from(Vec::new()));
    }
}

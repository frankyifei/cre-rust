use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use anyrender::recording::RenderCommand;
use anyrender::{Paint, Scene};
use blitz_dom::{DocumentConfig, Node, StyleThreading, build_single_font_ctx};
use blitz_html::HtmlDocument;
use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request};
use blitz_traits::shell::{ColorScheme, Viewport};
use cre_epub::{EpubBook, HtmlOptions};
use cre_paint::render_page;
use percent_encoding::percent_decode_str;

#[derive(Debug)]
struct Args {
    epub: PathBuf,
    out_dir: PathBuf,
    max_pages: usize,
    width: u32,
    height: u32,
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let book = EpubBook::open(&args.epub)?;
    println!("title: {}", book.title.as_deref().unwrap_or("<untitled>"));
    println!(
        "creator: {}",
        book.creator.as_deref().unwrap_or("<unknown>")
    );
    println!("spine items: {}", book.spine.len());

    let html = book.to_html(&HtmlOptions::default())?;
    println!("html bytes: {}", html.len());

    let resources = Arc::new(book.resources.clone());
    let net = Arc::new(EpubNet { resources });
    let font_data = std::fs::read(
        std::env::var("CRE_FONT_PATH")
            .unwrap_or_else(|_| "/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf".to_string()),
    )
    .context("read font")?;

    let layout_height = 100_000;
    let config = DocumentConfig {
        viewport: Some(Viewport::new(
            args.width,
            layout_height,
            1.0,
            ColorScheme::Light,
        )),
        base_url: Some("epub://book/".to_string()),
        net_provider: Some(net),
        font_ctx: Some(build_single_font_ctx(&font_data)),
        style_threading: StyleThreading::Sequential,
        ..Default::default()
    };

    let mut doc = HtmlDocument::from_html(&html, config);
    doc.resolve(0.0);
    println!("layout resolved");

    // Images are fetched during resolve_stylist() in the first resolve(); their results
    // arrive via the message channel and are processed only in the next resolve().
    // Loop until no new pending image messages are generated.
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

    std::fs::create_dir_all(&args.out_dir)
        .with_context(|| format!("create {}", args.out_dir.display()))?;

    let mut scene = Scene::default();
    blitz_paint::paint_scene(&mut scene, &mut doc, 1.0, args.width, layout_height, 0, 0);
    println!("paint commands: {}", scene.commands.len());
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
    println!("  fills: {fill_count} (images: {img_count}), glyph_runs: {glyph_count}");

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
        println!(
            "rendered page {} at y={:.1} (content from {:.1}) -> {}",
            index + 1,
            page.top,
            page.clip_top,
            path.display()
        );
    }

    Ok(())
}

fn parse_args() -> Result<Args> {
    let mut args = std::env::args().skip(1);
    let epub = args
        .next()
        .map(PathBuf::from)
        .context("usage: cre-render <epub> <out-dir> [max-pages] [width] [height]")?;
    let out_dir = args
        .next()
        .map(PathBuf::from)
        .context("usage: cre-render <epub> <out-dir> [max-pages] [width] [height]")?;
    let max_pages = args
        .next()
        .map(|s| s.parse())
        .transpose()
        .context("max-pages must be an integer")?
        .unwrap_or(3);
    let width = args
        .next()
        .map(|s| s.parse())
        .transpose()
        .context("width must be an integer")?
        .unwrap_or(600);
    let height = args
        .next()
        .map(|s| s.parse())
        .transpose()
        .context("height must be an integer")?
        .unwrap_or(800);
    if width == 0 || height == 0 {
        bail!("width and height must be positive");
    }
    Ok(Args {
        epub,
        out_dir,
        max_pages,
        width,
        height,
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

    // Walk layout tree (includes anonymous blocks that wrap bare text nodes)
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

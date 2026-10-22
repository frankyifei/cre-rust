use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use cre_lib::{Document, DocumentFormat, DocumentOptions};

#[derive(Debug)]
struct Args {
    input: PathBuf,
    out_dir: PathBuf,
    max_pages: usize,
    width: u32,
    height: u32,
    toc: bool,
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let options = DocumentOptions {
        width: args.width,
        height: args.height,
        ..Default::default()
    };
    let mut doc = Document::with_options(&args.input, &options)?;
    let metadata = doc.metadata();

    println!("format: {}", format_name(metadata.format));
    println!(
        "title: {}",
        metadata.title.as_deref().unwrap_or("<untitled>")
    );
    println!(
        "creator: {}",
        metadata.creator.as_deref().unwrap_or("<unknown>")
    );
    println!("items: {}", metadata.item_count);
    println!("pages: {}", doc.page_count());

    if args.toc {
        println!("toc entries: {}", doc.toc().len());
        for entry in doc.toc() {
            println!(
                "depth={} page={} target={} title={}",
                entry.depth,
                entry
                    .page
                    .map_or_else(|| "?".to_string(), |page| page.to_string()),
                entry.href,
                entry.title
            );
        }
    }

    std::fs::create_dir_all(&args.out_dir)
        .with_context(|| format!("create {}", args.out_dir.display()))?;

    for page in 1..=doc.page_count().min(args.max_pages) {
        let bitmap = doc.render_page(page)?;
        let path = args.out_dir.join(format!("page-{:03}.pgm", page));
        bitmap.write_pgm(&path)?;
        println!("rendered page {} -> {}", page, path.display());
    }

    Ok(())
}

fn format_name(format: DocumentFormat) -> &'static str {
    match format {
        DocumentFormat::Epub => "epub",
        DocumentFormat::Fb2 => "fb2",
        DocumentFormat::Txt => "txt",
    }
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

//! Building and configuring a `Document`.
//!
//! `Document` itself, and the free functions it is built from, stay in the
//! crate root; this module is the constructor half - opening a container,
//! streaming its chunks, and applying the reader's settings.

use super::*;

impl Document {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::with_options(path, &DocumentOptions::default())
    }

    pub fn with_options(path: impl AsRef<Path>, options: &DocumentOptions) -> Result<Self> {
        let font_datas = collect_font_datas(options)?;
        Self::with_options_with_font_data(path, options, &font_datas)
    }

    pub fn with_options_with_font_data(
        path: impl AsRef<Path>,
        options: &DocumentOptions,
        font_datas: &[FontBlob],
    ) -> Result<Self> {
        Self::open_inner(path, options, font_datas, false, None)
    }

    /// Opens a book with only its first chunk laid out and finishes the rest on
    /// the background worker, whatever the book's size.
    ///
    /// [`Self::with_options`] already does this for big books; this forces it,
    /// which is what the tests and the A/B runs use. Until
    /// [`Self::has_background_layout`] turns false, the page numbers are an
    /// estimate (the reader's position is an XPointer, so it is exact either
    /// way).
    pub fn with_options_lazy(path: impl AsRef<Path>, options: &DocumentOptions) -> Result<Self> {
        let font_datas = collect_font_datas(options)?;
        Self::open_inner(path, options, &font_datas, false, Some(true))
    }

    /// Opens a book and lays every chunk out before returning, so page numbers
    /// are exact from the first draw even for a book big enough to open lazily
    /// by default.
    pub fn with_options_exact(path: impl AsRef<Path>, options: &DocumentOptions) -> Result<Self> {
        let font_datas = collect_font_datas(options)?;
        Self::open_inner(path, options, &font_datas, false, Some(false))
    }

    /// Parses the document but defers laying out its pages to
    /// [`Self::ensure_layout`].
    ///
    /// KOReader applies every reading setting between `loadDocument` and
    /// `renderDocument`. Parsing eagerly and laying out lazily means the one
    /// layout that happens uses the final settings, instead of laying the book
    /// out with the defaults and then all over again with the user's font,
    /// margins and so on (~6 s each on a Kindle PW2).
    /// Builds a document from one in-memory HTML page, used to show the
    /// low-memory report instead of opening a book that cannot fit.
    pub fn from_html_page(
        html: &str,
        options: &DocumentOptions,
        font_datas: &[FontBlob],
    ) -> Result<Self> {
        let metadata = DocumentMetadata {
            format: DocumentFormat::Html,
            title: Some("Not enough memory".to_string()),
            creator: None,
            language: None,
            series: None,
            description: None,
            keywords: None,
            identifiers: None,
            item_count: 1,
        };
        Self::build(
            vec![html.to_string()],
            "",
            Arc::new(EmptyNet),
            ResourceSource::empty(),
            Arc::new(ImageRaster::new(options.width, options.height)),
            None,
            false,
            metadata,
            Vec::new(),
            String::new(),
            Vec::new(),
            options,
            font_datas,
            false,
            None,
            html.len() as u64,
            0,
        )
    }

    pub fn with_options_with_font_data_parsed(
        path: impl AsRef<Path>,
        options: &DocumentOptions,
        font_datas: &[FontBlob],
    ) -> Result<Self> {
        Self::open_inner(path, options, font_datas, true, None)
    }

    /// `lazy`: `Some(true)`/`Some(false)` force the lazy/full open path, `None`
    /// decides by book size (see [`Self::prefers_lazy_open`]). The decision is
    /// taken in [`Self::ensure_layout`], because a reader that parses on load
    /// and lays out later (cre-lua) only gets there then.
    fn open_inner(
        path: impl AsRef<Path>,
        options: &DocumentOptions,
        font_datas: &[FontBlob],
        defer_layout: bool,
        lazy: Option<bool>,
    ) -> Result<Self> {
        validate_options(options)?;
        let path = path.as_ref();
        let source_bytes = std::fs::metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let format = detect_format(path);
        // SVG images are rendered at page size: CSS caps them to the page's
        // content box, so this is the largest they can ever be drawn.
        let content_width = ((options.width as f32 - options.page_margin * 2.0)
            * options.viewport_scale)
            .max(1.0) as u32;
        let content_height = ((options.height as f32 - options.page_margin * 2.0)
            * options.viewport_scale)
            .max(1.0) as u32;
        let svg_raster = Arc::new(ImageRaster::new(content_width, content_height));
        match format {
            DocumentFormat::Epub => {
                let phase_started = std::time::Instant::now();
                let mut book = EpubBook::open(path)?;
                let open_book_ms = phase_started.elapsed().as_secs_f64() * 1000.0;
                let (text_budget, _image_budget, layout_budget) =
                    chunk_budgets(available_memory_bytes());
                // The zip directory says how much XHTML the spine holds, so a
                // book that cannot fit is refused before it is decompressed,
                // converted, split and compressed. The exact check still runs
                // after compression for books in the grey zone.
                if let Some(page) = memory_report::preflight(
                    book.spine_text_bytes(),
                    source_bytes,
                    text_budget,
                    book.title.as_deref(),
                ) {
                    return Err(anyhow::Error::new(LowMemory { page }));
                }
                let _ = book.normalize_inline_svg_images();
                let svg_ms = phase_started.elapsed().as_secs_f64() * 1000.0;
                let raw_toc = std::mem::take(&mut book.toc);
                let raw_page_map_source = std::mem::take(&mut book.page_map_source);
                let raw_page_map = std::mem::take(&mut book.page_map);
                let metadata = DocumentMetadata {
                    format,
                    title: book.title.clone(),
                    creator: book.creator.clone(),
                    language: book.language.clone(),
                    series: book.series.clone(),
                    description: book.description.clone(),
                    keywords: book.keywords.clone(),
                    identifiers: book.identifiers.clone(),
                    item_count: book.spine.len(),
                };
                let html_options = HtmlOptions::default();
                let images_started = std::time::Instant::now();
                let _ = book.normalize_full_page_images(
                    options.width as f32 / options.viewport_scale.max(0.1),
                    options.height as f32 / options.viewport_scale.max(0.1),
                    html_options.margin_x,
                    html_options.margin_y,
                );
                let images_ms = images_started.elapsed().as_secs_f64() * 1000.0;
                svg_raster.set_page_fit_images(std::mem::take(&mut book.page_fit_images));
                // Stream the chunks through the compressor instead of returning
                // every chunk's HTML. On a 24 MB book the old path held the
                // whole book two or three times before compression.
                let mut sources: Vec<ChunkSource> = Vec::new();
                let mut fixed_layouts: Vec<Option<FixedLayout>> = Vec::new();
                let mut content_images: Vec<String> = Vec::new();
                let mut embedded_fonts: Vec<String> = Vec::new();
                let mut chunk_image_bytes = 0u64;
                let compression_started = std::time::Instant::now();
                // No image budget: a chunk's pictures are laid out from their
                // headers (see `EpubNet`), so the layout does not pay for the
                // bytes a chunk mentions. Splitting on text alone is what every
                // other format does, and the same section structure now gives
                // the same chunk boundaries everywhere.
                book.to_html_chunks_budgeted_stream(
                    &html_options,
                    text_budget,
                    None,
                    layout_budget,
                    |html| {
                        fixed_layouts.push(parse_fixed_layout(&html));
                        content_images.extend(cre_book::image_sources_in(&html));
                        for name in cre_book::font_faces_in(&html) {
                            if !embedded_fonts
                                .iter()
                                .any(|existing: &String| existing.eq_ignore_ascii_case(&name))
                            {
                                embedded_fonts.push(name);
                            }
                        }
                        chunk_image_bytes =
                            chunk_image_bytes
                                .max(cre_epub::chunk_image_bytes(&book.resource_provider, &html)
                                    as u64);
                        sources.push(ChunkSource::new(html));
                        Ok(())
                    },
                )?;
                if std::env::var_os("CRE_LAYOUT_PROFILE").is_some() {
                    eprintln!(
                        "profile epub open: book={open_book_ms:.1}ms svg={:.1}ms images={images_ms:.1}ms \
                         stream+compress={:.1}ms chunks={}",
                        svg_ms - open_book_ms,
                        compression_started.elapsed().as_secs_f64() * 1000.0,
                        sources.len()
                    );
                }
                let cover_image = epub_cover_image(&book);
                let has_non_linear_flows = book.spine.iter().any(|item| !item.linear);
                // The chapter's `<link rel="stylesheet">` is fetched through
                // `EpubNet`, so the stylesheet is where most books declare their
                // fonts. Scan both places: this covers a stylesheet-only book
                // and one that inlines the rules.
                cre_book::fonts::extend_unique(
                    &mut embedded_fonts,
                    cre_epub::font_faces_in_stylesheets(&book),
                );
                let embedded_fonts_flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
                let resource_provider = Arc::new(book.resource_provider);
                let provider: Arc<dyn cre_book::ResourceProvider> = resource_provider.clone();
                let resources = ResourceSource::provider(provider);
                let mut document = Self::build_from_sources(
                    sources,
                    fixed_layouts,
                    content_images,
                    "cre://book/",
                    Arc::new(EpubNet::new(
                        resource_provider.clone(),
                        svg_raster.clone(),
                        embedded_fonts_flag.clone(),
                        resources.css_font_size(),
                        resources.generated(),
                    )),
                    resources,
                    svg_raster.clone(),
                    cover_image,
                    has_non_linear_flows,
                    metadata,
                    raw_toc,
                    raw_page_map_source,
                    raw_page_map,
                    options,
                    font_datas,
                    defer_layout,
                    lazy,
                    source_bytes,
                    chunk_image_bytes,
                )?;
                document.embedded_fonts = embedded_fonts;
                document.embedded_fonts_flag = embedded_fonts_flag;
                Ok(document)
            }
            DocumentFormat::Fb2 => {
                let mut book = Fb2Book::open(path)?;
                let metadata = DocumentMetadata {
                    format,
                    title: book.title.clone(),
                    creator: book.creator.clone(),
                    language: book.language.clone(),
                    series: None,
                    description: book.description.clone(),
                    keywords: None,
                    identifiers: book.isbn.clone().map(|isbn| format!("ISBN: {isbn}")),
                    item_count: book.binary.len(),
                };
                let html_options = Fb2HtmlOptions::default();
                let (body, fb2_toc) = book.to_body_html_with_toc(&html_options)?;
                let raw_toc = fb2_toc
                    .into_iter()
                    .map(|item| RawTocItem {
                        title: item.title,
                        href: item.href,
                        depth: item.depth,
                    })
                    .collect();
                let head = book.html_head(&html_options);
                // Move the pictures and the cover out instead of cloning a
                // second copy of every base64-decoded image.
                let cover_image = book.binary.keys().next().cloned();
                let binary = Arc::new(std::mem::take(&mut book.binary));
                let resources = MapResources::Owned(binary);
                let resource_source = ResourceSource::map(resources.clone());
                let mut sink = ChunkSink::new();
                stream_single_document_chunks(&head, &body, &resource_source, |html| {
                    sink.push(html)
                })?;
                // The parsed XML is no longer needed: drop it before the
                // layout so the book is not resident next to its chunks.
                drop(book);
                drop(body);
                Self::build_from_sources(
                    sink.sources,
                    sink.fixed_layouts,
                    sink.content_images,
                    "cre://book/",
                    Arc::new(MapNet::new(
                        resources.clone(),
                        svg_raster.clone(),
                        resource_source.generated(),
                        resource_source.fonts_enabled(),
                        resource_source.css_font_size(),
                    )),
                    resource_source,
                    svg_raster.clone(),
                    cover_image,
                    false,
                    metadata,
                    raw_toc,
                    String::new(),
                    Vec::new(),
                    options,
                    font_datas,
                    defer_layout,
                    lazy,
                    source_bytes,
                    MAP_CHUNK_IMAGE_BYTES,
                )
            }
            DocumentFormat::Mobi => {
                let mut book = MobiBook::open(path)?;
                let metadata = DocumentMetadata {
                    format,
                    title: book.title.clone(),
                    creator: book.creator.clone(),
                    language: book.language.clone(),
                    series: book.series.clone(),
                    description: book.description.clone(),
                    keywords: book.keywords.clone(),
                    identifiers: book.identifiers.clone(),
                    item_count: book.section_count,
                };
                let html_options = MobiHtmlOptions::default();
                let toc = book.toc.clone();
                let raw_toc = toc
                    .into_iter()
                    .map(|item| RawTocItem {
                        title: item.title,
                        href: item.href,
                        depth: item.depth,
                    })
                    .collect();
                let head = book.html_head(&html_options);
                let cover_image = book.cover_image.take();
                let resources = MapResources::Mapped(Arc::new(std::mem::take(&mut book.resources)));
                let resource_source = ResourceSource::map(resources.clone());
                let body = prepare_page_images(
                    book.body_html().to_string(),
                    &resource_source,
                    options,
                    &svg_raster,
                );
                let mut sink = ChunkSink::new();
                stream_single_document_chunks(&head, &body, &resource_source, |html| {
                    sink.push(html)
                })?;
                drop(book);
                Self::build_from_sources(
                    sink.sources,
                    sink.fixed_layouts,
                    sink.content_images,
                    "cre://book/",
                    Arc::new(MapNet::new(
                        resources.clone(),
                        svg_raster.clone(),
                        resource_source.generated(),
                        resource_source.fonts_enabled(),
                        resource_source.css_font_size(),
                    )),
                    resource_source,
                    svg_raster.clone(),
                    cover_image,
                    false,
                    metadata,
                    raw_toc,
                    String::new(),
                    Vec::new(),
                    options,
                    font_datas,
                    defer_layout,
                    lazy,
                    source_bytes,
                    MAP_CHUNK_IMAGE_BYTES,
                )
            }
            DocumentFormat::Azw3 => {
                let mut book = Kf8Book::open(path)?;
                let metadata = DocumentMetadata {
                    format,
                    title: book.title.clone(),
                    creator: book.creator.clone(),
                    language: book.language.clone(),
                    series: book.series.clone(),
                    description: book.description.clone(),
                    keywords: book.keywords.clone(),
                    identifiers: book.identifiers.clone(),
                    item_count: book.section_count,
                };
                let html_options = Kf8HtmlOptions::default();
                let toc = book.toc.clone();
                let raw_toc = toc
                    .into_iter()
                    .map(|item| RawTocItem {
                        title: item.title,
                        href: item.href,
                        depth: item.depth,
                    })
                    .collect();
                let head = book.html_head(&html_options);
                let cover_image = book.cover_image.take();
                let resources = MapResources::Mapped(Arc::new(std::mem::take(&mut book.resources)));
                let resource_source = ResourceSource::map(resources.clone());
                let body = prepare_page_images(
                    book.body_html().to_string(),
                    &resource_source,
                    options,
                    &svg_raster,
                );
                for (name, bytes) in &book.stylesheets {
                    resource_source.insert(name.clone(), Arc::new(bytes.clone()));
                }
                let style_texts: Vec<String> = book
                    .stylesheets
                    .iter()
                    .map(|(_, bytes)| String::from_utf8_lossy(bytes).into_owned())
                    .collect();
                let mut sink = ChunkSink::new();
                // The book's `@font-face` rules live in its stylesheet flows,
                // which are served resources now.
                let check = |url: &str| resources.contains_key(url);
                let mut embedded_fonts: Vec<String> =
                    cre_book::fonts::collect(style_texts.iter().map(String::as_str), Some(&check));
                stream_single_document_chunks(&head, &body, &resource_source, |html| {
                    // Only families whose rule points at a record the book
                    // really has: a converter that drops the font bytes leaves
                    // the rule behind with a placeholder URL.
                    for name in
                        cre_book::loadable_font_faces(&html, |url| resources.contains_key(url))
                    {
                        if !embedded_fonts
                            .iter()
                            .any(|existing| existing.eq_ignore_ascii_case(&name))
                        {
                            embedded_fonts.push(name);
                        }
                    }
                    sink.push(html)
                })?;
                drop(book);
                let mut document = Self::build_from_sources(
                    sink.sources,
                    sink.fixed_layouts,
                    sink.content_images,
                    "cre://book/",
                    Arc::new(MapNet::new(
                        resources.clone(),
                        svg_raster.clone(),
                        resource_source.generated(),
                        resource_source.fonts_enabled(),
                        resource_source.css_font_size(),
                    )),
                    resource_source,
                    svg_raster.clone(),
                    cover_image,
                    false,
                    metadata,
                    raw_toc,
                    String::new(),
                    Vec::new(),
                    options,
                    font_datas,
                    defer_layout,
                    lazy,
                    source_bytes,
                    MAP_CHUNK_IMAGE_BYTES,
                )?;
                document.embedded_fonts = embedded_fonts;
                Ok(document)
            }
            DocumentFormat::Txt => {
                let book = TextBook::open(path)?;
                let metadata = DocumentMetadata {
                    format,
                    title: book.title.clone(),
                    creator: None,
                    language: None,
                    series: None,
                    description: None,
                    keywords: None,
                    identifiers: None,
                    item_count: 1,
                };
                let html_options = TextHtmlOptions::default();
                let html = book.to_html(&html_options);
                let mut sink = ChunkSink::new();
                match cre_book::chunk::html_body_parts(&html) {
                    Some((head, body, _tail)) => cre_book::chunk::stream_single_document_chunks(
                        head,
                        body,
                        &ResourceSource::empty(),
                        |chunk| sink.push(chunk),
                    )?,
                    None => sink.push(html.clone())?,
                }
                Self::build_from_sources(
                    sink.sources,
                    sink.fixed_layouts,
                    sink.content_images,
                    "cre://book/",
                    Arc::new(EmptyNet),
                    ResourceSource::empty(),
                    Arc::new(ImageRaster::new(options.width, options.height)),
                    None,
                    false,
                    metadata,
                    Vec::new(),
                    String::new(),
                    Vec::new(),
                    options,
                    font_datas,
                    defer_layout,
                    lazy,
                    source_bytes,
                    0,
                )
            }
            DocumentFormat::Html => {
                // A standalone HTML/XHTML file is used as-is; relative
                // resources (images, extra CSS) are loaded from its directory.
                let bytes =
                    std::fs::read(path).with_context(|| format!("read html {}", path.display()))?;
                let mut html = String::from_utf8_lossy(&bytes).into_owned();
                // Mirror the generators: give bare HTML the same base
                // typography and page padding EPUB/FB2 chunks get, so margins
                // and font size behave the same and the first line is not
                // laid out above the page's top margin. Both values are layout
                // settings, so they go in as tokens and are filled in when the
                // chunk is laid out (a font or margin change is then a
                // relayout, not a reopen).
                insert_style(
                    &mut html,
                    &format!(
                        "/* cre-render-base */\n\
                         html, body {{ margin: 0; padding: 0; }}\n\
                         body {{ font-family: serif; font-size: {}px; line-height: 1.2; \
                         padding: {}px; }}\n\
                         img {{ max-width: 100%; height: auto; }}",
                        cre_book::css::FONT_SIZE,
                        cre_book::css::PAGE_MARGIN
                    ),
                );
                let directory = path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."));
                let root = std::fs::canonicalize(&directory).unwrap_or(directory);
                let base_url = url::Url::from_directory_path(&root)
                    .map(|url| url.to_string())
                    .unwrap_or_else(|_| "file:///".to_string());
                let title = path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let metadata = DocumentMetadata {
                    format,
                    title: Some(title),
                    creator: None,
                    language: None,
                    series: None,
                    description: None,
                    keywords: None,
                    identifiers: None,
                    item_count: 1,
                };
                let mut embedded_fonts: Vec<String> = Vec::new();
                let mut font_sources: Vec<String> = vec![html.clone()];
                // A standalone document keeps its rules in the linked
                // stylesheets, not in the document text; `FileNet` serves them
                // from this directory, so read the same files for the list.
                for href in cre_book::fonts::stylesheet_hrefs(&html) {
                    let href = href.split(['#', '?']).next().unwrap_or("");
                    if href.is_empty() || href.contains("://") {
                        continue;
                    }
                    let decoded = percent_decode_str(href).decode_utf8_lossy();
                    let Ok(css) = std::fs::read_to_string(root.join(decoded.as_ref())) else {
                        continue;
                    };
                    font_sources.push(css);
                }
                cre_book::fonts::extend_unique(
                    &mut embedded_fonts,
                    cre_book::fonts::collect(font_sources.iter().map(String::as_str), None),
                );
                let embedded_fonts_flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
                // A standalone document has no archive to index: its pictures
                // and stylesheets are files next to it, and some are inlined as
                // `data:` URIs. The resource source answers both, so the plate
                // pass and the painter use the same namespace as every other
                // format.
                let resources = ResourceSource::files(root.clone());
                let images = Arc::new(ImageRaster::new(options.width, options.height));
                let html = prepare_page_images(html, &resources, options, &images);
                // Stream the chunks through the compressor instead of holding
                // every chunk's HTML at once; a standalone document chunks the
                // same way a book body does.
                let mut sink = ChunkSink::new();
                match cre_book::chunk::html_body_parts(&html) {
                    Some((head, body, _tail)) => cre_book::chunk::stream_single_document_chunks(
                        head,
                        body,
                        &resources,
                        |chunk| sink.push(chunk),
                    )?,
                    None => sink.push(html.clone())?,
                }
                let mut document = Self::build_from_sources(
                    sink.sources,
                    sink.fixed_layouts,
                    sink.content_images,
                    &base_url,
                    Arc::new(FileNet::new(
                        root.clone(),
                        images.clone(),
                        embedded_fonts_flag.clone(),
                        resources.css_font_size(),
                        resources.generated(),
                    )),
                    resources,
                    images,
                    None,
                    false,
                    metadata,
                    Vec::new(),
                    String::new(),
                    Vec::new(),
                    options,
                    font_datas,
                    defer_layout,
                    lazy,
                    source_bytes,
                    0,
                )?;
                document.embedded_fonts = embedded_fonts;
                document.embedded_fonts_flag = embedded_fonts_flag;
                Ok(document)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build(
        htmls: Vec<String>,
        base_url: &str,
        net: Arc<dyn NetProvider>,
        resources: ResourceSource,
        images: Arc<ImageRaster>,
        cover_image: Option<String>,
        has_non_linear_flows: bool,
        metadata: DocumentMetadata,
        raw_toc: Vec<RawTocItem>,
        page_map_source: String,
        page_map_items: Vec<RawPageMapItem>,
        options: &DocumentOptions,
        font_datas: &[FontBlob],
        defer_layout: bool,
        lazy: Option<bool>,
        source_bytes: u64,
        chunk_image_bytes: u64,
    ) -> Result<Self> {
        let fixed_layouts: Vec<Option<FixedLayout>> =
            htmls.iter().map(|html| parse_fixed_layout(html)).collect();
        // Only images an `<img>` element references may become a size
        // placeholder: a CSS background image has no other painter, and a
        // fixed-layout page draws its images through its own transform.
        let content_images: Vec<String> = if resources.is_empty() {
            Vec::new()
        } else {
            htmls
                .iter()
                .flat_map(|html| cre_book::image_sources_in(html))
                .collect()
        };
        // The chunks are compressed before the memory check so it can count the
        // text the document actually keeps resident (one LZ4 block per chunk)
        // instead of the generated HTML it is made from. The compression pass
        // runs on every open anyway, so this adds no work to a book that fits.
        let compression_started = std::time::Instant::now();
        let sources: Vec<ChunkSource> = htmls.into_iter().map(ChunkSource::new).collect();
        if std::env::var_os("CRE_LAYOUT_PROFILE").is_some() {
            eprintln!(
                "profile chunk html compress: {:.1}ms chunks={}",
                compression_started.elapsed().as_secs_f64() * 1000.0,
                sources.len()
            );
        }
        Self::build_from_sources(
            sources,
            fixed_layouts,
            content_images,
            base_url,
            net,
            resources,
            images,
            cover_image,
            has_non_linear_flows,
            metadata,
            raw_toc,
            page_map_source,
            page_map_items,
            options,
            font_datas,
            defer_layout,
            lazy,
            source_bytes,
            chunk_image_bytes,
        )
    }

    /// Builds a document from already compressed chunks.
    ///
    /// The EPUB path fills `sources`, `fixed_layouts` and `content_images` while
    /// the chunker streams, so the whole book's generated HTML never exists.
    #[allow(clippy::too_many_arguments)]
    fn build_from_sources(
        sources: Vec<ChunkSource>,
        fixed_layouts: Vec<Option<FixedLayout>>,
        content_images: Vec<String>,
        base_url: &str,
        net: Arc<dyn NetProvider>,
        resources: ResourceSource,
        images: Arc<ImageRaster>,
        cover_image: Option<String>,
        has_non_linear_flows: bool,
        metadata: DocumentMetadata,
        raw_toc: Vec<RawTocItem>,
        page_map_source: String,
        page_map_items: Vec<RawPageMapItem>,
        options: &DocumentOptions,
        font_datas: &[FontBlob],
        defer_layout: bool,
        lazy: Option<bool>,
        source_bytes: u64,
        chunk_image_bytes: u64,
    ) -> Result<Self> {
        let font_context = Arc::new(build_font_ctx_with_fallbacks(
            font_datas,
            options.font_family.as_deref(),
        ));
        if !content_images.is_empty() {
            images.set_content_images(content_images);
        }
        // A book that cannot fit is refused here, before the layout allocates
        // several copies of it; `cre-lua` turns the error into a page that
        // explains the shortfall instead of letting the reader be killed.
        if let Some(page) = memory_report::low_memory_page(
            &sources,
            source_bytes,
            chunk_image_bytes,
            metadata.title.as_deref(),
        ) {
            return Err(anyhow::Error::new(LowMemory { page }));
        }
        // The document and its net providers share one font setting; take the
        // handle before `resources` is moved into the struct.
        let embedded_fonts_flag = resources.fonts_enabled();
        // The font size a served stylesheet resolves its absolute sizes
        // against, recorded before the first layout fetches one.
        let css_font_size = resources.css_font_size();
        resources.set_css_font_size(options.font_size);
        let mut document = Self {
            metadata,
            width: options.width,
            height: options.height,
            layout_height: options.layout_height,
            page_margin: options.page_margin,
            margins: PageMargins::uniform(options.page_margin),
            stylesheet: options.stylesheet.clone(),
            embedded_styles_enabled: options.embedded_styles_enabled,
            font_css_size: options.font_size,
            font_family: options.font_family.clone(),
            font_blobs: font_datas.to_vec(),
            viewport_scale: options.viewport_scale,
            font_gamma: 1.0,
            sources: Arc::from(sources),
            fixed_layouts,
            base_url: base_url.to_string(),
            resources,
            images,
            cover_image,
            has_non_linear_flows,
            net,
            font_context,
            page_table: PageTable::new(),
            xpointer_pages: HashMap::new(),
            href_pages: HashMap::new(),
            spine_to_chunk: HashMap::new(),
            spine_real_index: HashMap::new(),
            toc: Vec::new(),
            page_map_source,
            page_map: Vec::new(),
            raw_toc,
            raw_page_map_items: page_map_items,
            embedded_fonts: Vec::new(),
            embedded_fonts_flag,
            css_font_size,
            phase: LayoutPhase::Unstarted,
            lazy_open: lazy,
            active: RefCell::new(None),
            current_page: 1,
            background_color: 0xFF_FFFF,
            page_image_cache: RefCell::new(HashMap::new()),
            page_cache_order: RefCell::new(Vec::new()),
            page_boxes_cache: RefCell::new(None),
            paint_cache: RefCell::new(cre_paint::PaintCache::new()),
            background_installs: 0,
        };
        if !defer_layout {
            document.ensure_layout()?;
        }
        document.resources.drop_pages();
        Ok(document)
    }

    /// Whether this book is worth opening lazily: laying out every chunk before
    /// the first page is shown costs time the reader would rather spend reading,
    /// and the background pass produces exactly the same pages.
    ///
    /// Only big books take the lazy path: their full pass is the expensive one,
    /// and the page numbers in between are an estimate. A small book lays out in
    /// well under a second, so it keeps exact numbers from the first draw.
    pub(crate) fn prefers_lazy_open(&self) -> bool {
        if let Some(requested) = self.lazy_open {
            return requested;
        }
        match std::env::var("CRE_LAZY_OPEN").ok().as_deref() {
            Some("0") => return false,
            Some("1") => return true,
            _ => {}
        }
        let bytes: usize = self.sources.iter().map(|source| source.text_len).sum();
        bytes >= LAZY_OPEN_MIN_BYTES
    }

    /// Overrides the lazy/full open policy for a document that has not been laid
    /// out yet.
    ///
    /// `None` is the default (decide by size, or by `CRE_LAZY_OPEN`);
    /// `Some(false)` forces the full pass, which is what a caller wanting exact
    /// page numbers from the first draw asks for.
    pub fn set_lazy_open(&mut self, lazy: Option<bool>) {
        if !self.phase.started() {
            self.lazy_open = lazy;
        }
    }

    /// The options used to lay chunk HTML out, built from the current settings.
    fn layout_options(&self) -> DocumentOptions {
        DocumentOptions {
            width: self.width,
            height: self.height,
            layout_height: self.layout_height,
            page_margin: self.margins.top,
            font_size: self.font_css_size,
            font_path: None,
            font_family: self.font_family.clone(),
            fallback_font_paths: Vec::new(),
            stylesheet: self.stylesheet.clone(),
            embedded_styles_enabled: self.embedded_styles_enabled,
            viewport_scale: self.viewport_scale,
        }
    }

    /// The margins a chunk is laid out with: none for a pre-paginated page (it
    /// is fitted to the viewport), otherwise the reader's, or the uniform
    /// `page_margin` when the caller never configured them.
    pub(crate) fn chunk_margins(&self, chunk_index: usize) -> PageMargins {
        if self
            .fixed_layouts
            .get(chunk_index)
            .and_then(|fixed| *fixed)
            .is_some()
        {
            return PageMargins::uniform(0.0);
        }
        if self.margins.is_explicit() {
            return self.margins;
        }
        PageMargins::uniform(self.layout_options_for_chunk(chunk_index).page_margin)
    }

    pub(crate) fn layout_options_for_chunk(&self, chunk_index: usize) -> DocumentOptions {
        let mut options = self.layout_options();
        if let Some(fixed) = self.fixed_layouts.get(chunk_index).and_then(|fixed| *fixed) {
            let scale = self.viewport_scale.max(0.1);
            let (width, height) = self.fixed_size(fixed);
            options.width = (width * scale).round().max(1.0) as u32;
            options.height = (height * scale).round().max(1.0) as u32;
            options.layout_height = options.height;
            options.page_margin = 0.0;
        }
        options
    }

    fn reader_css_size(&self) -> (f32, f32) {
        let scale = self.viewport_scale.max(0.1);
        (self.width as f32 / scale, self.height as f32 / scale)
    }

    pub(crate) fn fixed_size(&self, fixed: FixedLayout) -> (f32, f32) {
        let (reader_width, reader_height) = self.reader_css_size();
        fixed.size(reader_width, reader_height)
    }

    /// CSS scale and center offsets for a pre-paginated page inside the reader
    /// viewport. Offsets are in CSS pixels.
    pub(crate) fn fixed_page_transform(&self, fixed: FixedLayout) -> (f32, f32, f32) {
        let scale = self.viewport_scale.max(0.1);
        let (reader_width, reader_height) = self.reader_css_size();
        let (page_width, page_height) = self.fixed_size(fixed);
        // A device-sized ICB is the whole reader viewport; page margins would
        // leave a blank border, so they are not applied here.
        let margin = if fixed == FixedLayout::Device {
            0.0
        } else {
            (self.page_margin / scale).max(0.0)
        };
        let available_width = (reader_width - margin * 2.0).max(1.0);
        let available_height = (reader_height - margin * 2.0).max(1.0);
        let fit = (available_width / page_width)
            .min(available_height / page_height)
            .max(0.01);
        let offset_x = (reader_width - page_width * fit) / 2.0;
        let offset_y = (reader_height - page_height * fit) / 2.0;
        (fit, offset_x, offset_y)
    }

    /// The FXL page box in device pixels: `(left, top, right, bottom)`.
    ///
    /// Blitz does not clip descendants of `overflow:hidden` boxes that are
    /// absolutely positioned, so a pre-paginated page whose content is larger
    /// than its ICB would otherwise paint outside the fixed page (the red
    /// rectangle in `lay-fxl-xhtml-icb_repeated-in-meta`). The renderer clips
    /// the scene to this rectangle instead.
    pub(crate) fn fixed_page_rect(&self, fixed: FixedLayout) -> (f32, f32, f32, f32) {
        let (fit, offset_x, offset_y) = self.fixed_page_transform(fixed);
        let (page_width, page_height) = self.fixed_size(fixed);
        let scale = self.viewport_scale.max(0.1);
        let left = offset_x * scale;
        let top = offset_y * scale;
        let right = left + page_width * fit * scale;
        let bottom = top + page_height * fit * scale;
        (left, top, right, bottom)
    }

    /// Sets the four page margins (device pixels) for the next layout.
    ///
    /// The reader configures left, top, right and bottom separately. Only the
    /// top one could be expressed before, so moving the left, right or bottom
    /// margin changed a setting without changing the page. Call this and then
    /// [`Self::relayout`].
    pub fn set_page_margins(&mut self, margins: PageMargins) {
        self.margins = PageMargins::new(margins.left, margins.top, margins.right, margins.bottom);
        self.page_margin = margins.top;
    }

    /// The page margins currently in use.
    pub fn page_margins(&self) -> PageMargins {
        self.margins
    }

    /// Replaces the fonts used by the next chunk layout. Chunks already laid out
    /// keep their own copy until they are laid out again.
    pub fn set_fonts(&mut self, font_datas: &[FontBlob]) {
        self.font_blobs = font_datas.to_vec();
        self.rebuild_font_context();
    }

    /// Rebuilds the layout font context from the blobs it already holds, for
    /// when the selected family changed but the files did not (a collection
    /// holds several families in one file).
    pub(crate) fn rebuild_font_context(&mut self) {
        self.font_context = Arc::new(build_font_ctx_with_fallbacks(
            &self.font_blobs,
            self.font_family.as_deref(),
        ));
        *self.active.borrow_mut() = None;
        // The cached glyph masks came from the previous faces.
        self.paint_cache.borrow_mut().clear();
    }
}

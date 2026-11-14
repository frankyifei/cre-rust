//! Drawing pages.
//!
//! A page render turns the page's chunk into a live DOM, paints the visible
//! slice into a bitmap and caches the last few pages so a scroll does not
//! repaint. Everything here is about the pixels: the page *numbers* live in
//! `page_table.rs`, the page *text* in `text.rs`.

use super::*;

impl Document {
    pub fn render_page_with_stats(
        &mut self,
        page: usize,
    ) -> Result<(Arc<PageBitmap>, usize, usize)> {
        self.render_page_with_stats_inner(page)
    }

    fn render_page_with_stats_inner(
        &mut self,
        page: usize,
    ) -> Result<(Arc<PageBitmap>, usize, usize)> {
        // Page numbers may have moved while the background worker laid the
        // rest of the book out; install what is ready before using them.
        self.drain_background_layouts();
        let profile = std::env::var_os("CRE_RENDER_PROFILE").is_some();
        let call_started = std::time::Instant::now();
        // A chunk that has not been laid out since the last settings change only
        // has provisional pages; lay it out before trusting its geometry.
        let page = self.ensure_page_layout(page)?;
        let Some(index) = page.checked_sub(1) else {
            bail!("page numbers are 1-based");
        };
        let Some(snapshot) = self.page_table.pages().get(index) else {
            bail!("page {page} is outside 1..={}", self.page_count());
        };
        let chunk_index = snapshot.chunk;
        let start = snapshot.local_start;
        let clip_top = start.clip_top - start.top;
        let clip_bottom = snapshot.local_clip_bottom - start.top;

        if let Some(cached) = self.cached_page_image(page) {
            self.ensure_chunk(chunk_index)?;
            return Ok((
                cached.bitmap.clone(),
                cached.image_count,
                cached.image_surface,
            ));
        }

        let (bitmap, image_count, image_surface) = {
            self.ensure_chunk(chunk_index)?;
            let mut active = self.active.borrow_mut();
            let active = active
                .as_mut()
                .context("active document chunk is missing")?;
            let viewport_scale = self.viewport_scale;
            let fixed_layout = self.fixed_layouts.get(chunk_index).and_then(|fixed| *fixed);
            let mut scene = Scene::default();
            let mut placed: Vec<cre_paint::PlacedImage> = Vec::new();
            if let Some(fixed) = fixed_layout {
                active.doc.set_viewport_scroll(Point { x: 0.0, y: 0.0 });
                let (fit, offset_x, offset_y) = self.fixed_page_transform(fixed);
                let scale = viewport_scale * fit;
                blitz_paint::paint_scene(
                    &mut scene,
                    &mut active.doc,
                    scale as f64,
                    self.width,
                    self.height,
                    (offset_x * viewport_scale).round().max(0.0) as u32,
                    (offset_y * viewport_scale).round().max(0.0) as u32,
                );
                {
                    let root = active.doc.root_element();
                    collect_placed_images(
                        &active.doc,
                        root,
                        f32::MAX,
                        viewport_scale * fit,
                        offset_x * viewport_scale,
                        offset_y * viewport_scale,
                        &self.images,
                        &self.resources,
                        &self.base_url,
                        &mut placed,
                    );
                }
                let (clip_left, clip_top, clip_right, clip_bottom) = self.fixed_page_rect(fixed);
                let (image_arc, paint_count, paint_surface) = paint_page(
                    &scene,
                    self.width,
                    self.height,
                    0.0,
                    clip_left,
                    clip_right,
                    clip_top,
                    clip_bottom,
                    self.background_color,
                    self.font_gamma,
                    &placed,
                );
                (
                    image_arc,
                    paint_count + placed.len(),
                    paint_surface + placed_image_area(&placed, self.width, self.height),
                )
            } else {
                let record_started = std::time::Instant::now();
                active.doc.set_viewport_scroll(Point {
                    x: 0.0,
                    y: f64::from(start.top),
                });
                // Images the page only carries as a size placeholder are not in
                // the scene; decode their originals for this paint.
                {
                    let root = active.doc.root_element();
                    collect_placed_images(
                        &active.doc,
                        root,
                        clip_bottom,
                        viewport_scale,
                        0.0,
                        0.0,
                        &self.images,
                        &self.resources,
                        &self.base_url,
                        &mut placed,
                    );
                }
                blitz_paint::paint_scene(
                    &mut scene,
                    &mut active.doc,
                    viewport_scale as f64,
                    self.width,
                    self.height,
                    0,
                    0,
                );
                if std::env::var_os("CRE_RENDER_PROFILE").is_some() {
                    eprintln!(
                        "render profile page {page}: record={:.2}ms commands={}",
                        record_started.elapsed().as_secs_f64() * 1000.0,
                        scene.commands.len()
                    );
                }
                let local_top = clip_top * viewport_scale;
                let local_bottom = clip_bottom * viewport_scale;
                let paint_started = std::time::Instant::now();
                let (image_arc, paint_count, paint_surface) = paint_page_cached(
                    &scene,
                    self.width,
                    self.height,
                    0.0,
                    0.0,
                    self.width as f32,
                    local_top,
                    local_bottom,
                    self.background_color,
                    self.font_gamma,
                    &placed,
                    &mut self.paint_cache.borrow_mut(),
                );
                let image_count = paint_count + placed.len();
                let image_surface =
                    paint_surface + placed_image_area(&placed, self.width, self.height);
                if std::env::var_os("CRE_RENDER_PROFILE").is_some() {
                    eprintln!(
                        "render profile page {page}: paint={:.2}ms",
                        paint_started.elapsed().as_secs_f64() * 1000.0
                    );
                }
                (image_arc, image_count, image_surface)
            }
        };
        let image = PageImage {
            bitmap: bitmap.clone(),
            image_count,
            image_surface,
        };
        let out = (bitmap.clone(), image_count, image_surface);
        self.cache_page_image(page, image);
        if profile {
            eprintln!(
                "  render total page {page}: {:.2}ms rss={}kB",
                call_started.elapsed().as_secs_f64() * 1000.0,
                resident_bytes().unwrap_or(0) / 1024
            );
        }
        Ok(out)
    }

    pub fn render_page_color_with_stats(
        &mut self,
        page: usize,
    ) -> Result<(Arc<ColorPageBitmap>, usize, usize)> {
        let page = self.ensure_page_layout(page)?;
        let Some(index) = page.checked_sub(1) else {
            bail!("page numbers are 1-based");
        };
        let Some(snapshot) = self.page_table.pages().get(index) else {
            bail!("page {page} is outside 1..={}", self.page_count());
        };
        let chunk_index = snapshot.chunk;
        let start = snapshot.local_start;
        let clip_top = start.clip_top - start.top;
        let clip_bottom = snapshot.local_clip_bottom - start.top;

        self.ensure_chunk(chunk_index)?;
        let mut active = self.active.borrow_mut();
        let active = active
            .as_mut()
            .context("active document chunk is missing")?;
        let viewport_scale = self.viewport_scale;
        let fixed_layout = self.fixed_layouts.get(chunk_index).and_then(|fixed| *fixed);
        let mut scene = Scene::default();
        let mut placed: Vec<cre_paint::PlacedImage> = Vec::new();
        let (clip_left, clip_right, local_top, local_bottom) = if let Some(fixed) = fixed_layout {
            active.doc.set_viewport_scroll(Point { x: 0.0, y: 0.0 });
            let (fit, offset_x, offset_y) = self.fixed_page_transform(fixed);
            let scale = viewport_scale * fit;
            blitz_paint::paint_scene(
                &mut scene,
                &mut active.doc,
                scale as f64,
                self.width,
                self.height,
                (offset_x * viewport_scale).round().max(0.0) as u32,
                (offset_y * viewport_scale).round().max(0.0) as u32,
            );
            {
                let root = active.doc.root_element();
                collect_placed_images(
                    &active.doc,
                    root,
                    f32::MAX,
                    viewport_scale * fit,
                    offset_x * viewport_scale,
                    offset_y * viewport_scale,
                    &self.images,
                    &self.resources,
                    &self.base_url,
                    &mut placed,
                );
            }
            let (left, top, right, bottom) = self.fixed_page_rect(fixed);
            (left, right, top, bottom)
        } else {
            active.doc.set_viewport_scroll(Point {
                x: 0.0,
                y: f64::from(start.top),
            });
            {
                let root = active.doc.root_element();
                collect_placed_images(
                    &active.doc,
                    root,
                    clip_bottom,
                    viewport_scale,
                    0.0,
                    0.0,
                    &self.images,
                    &self.resources,
                    &self.base_url,
                    &mut placed,
                );
            }
            blitz_paint::paint_scene(
                &mut scene,
                &mut active.doc,
                viewport_scale as f64,
                self.width,
                self.height,
                0,
                0,
            );
            (
                0.0,
                self.width as f32,
                clip_top * viewport_scale,
                clip_bottom * viewport_scale,
            )
        };
        let (bitmap, image_count, image_surface) = paint_page_rgba(
            &scene,
            self.width,
            self.height,
            0.0,
            clip_left,
            clip_right,
            local_top,
            local_bottom,
            self.background_color,
            self.font_gamma,
            &placed,
        );
        Ok((
            bitmap,
            image_count + placed.len(),
            image_surface + placed_image_area(&placed, self.width, self.height),
        ))
    }

    pub fn render_current_page_color_with_stats(
        &mut self,
    ) -> Result<(Arc<ColorPageBitmap>, usize, usize)> {
        self.render_page_color_with_stats(self.current_page)
    }

    pub fn set_font_gamma(&mut self, gamma: f32) {
        let gamma = if gamma.is_finite() {
            gamma.clamp(0.1, 15.0)
        } else {
            1.0
        };
        if gamma != self.font_gamma {
            self.font_gamma = gamma;
            self.clear_page_image_cache();
        }
    }

    pub fn render_page(&mut self, page: usize) -> Result<PageBitmap> {
        let (bitmap, _, _) = self.render_page_with_stats(page)?;
        Ok(bitmap.as_ref().clone())
    }

    pub fn render_current_page(&mut self) -> Result<PageBitmap> {
        self.render_page(self.current_page)
    }

    pub fn render_current_page_with_stats(&mut self) -> Result<(Arc<PageBitmap>, usize, usize)> {
        self.render_page_with_stats(self.current_page)
    }

    fn cached_page_image(&self, page: usize) -> Option<PageImage> {
        let image = self.page_image_cache.borrow().get(&page).cloned()?;
        self.touch_page_image_cache(page);
        Some(image)
    }

    fn touch_page_image_cache(&self, page: usize) {
        let mut order = self.page_cache_order.borrow_mut();
        order.retain(|cached| *cached != page);
        order.push(page);
    }

    fn cache_page_image(&self, page: usize, image: PageImage) {
        self.touch_page_image_cache(page);
        let mut cache = self.page_image_cache.borrow_mut();
        loop {
            let evicted = {
                let mut order = self.page_cache_order.borrow_mut();
                if order.len() <= PAGE_IMAGE_CACHE_CAPACITY {
                    break;
                }
                order.remove(0)
            };
            cache.remove(&evicted);
        }
        cache.insert(page, image);
    }

    pub(crate) fn clear_page_image_cache(&self) {
        self.page_image_cache.borrow_mut().clear();
        self.page_cache_order.borrow_mut().clear();
    }

    /// Installs an already parsed chunk as the active one, skipping the second
    /// parse `ensure_chunk` would do.
    pub(crate) fn install_active(
        &self,
        index: usize,
        doc: HtmlDocument,
        lines: Vec<LineBox>,
        page_starts: Vec<PageStart>,
    ) {
        let rendered_text_nodes = rendered_text_nodes(&doc, &lines);
        *self.active.borrow_mut() = Some(ActiveChunk {
            index,
            rendered_text_nodes,
            doc,
            lines,
            page_starts,
        });
    }

    fn ensure_chunk(&self, index: usize) -> Result<()> {
        if self
            .active
            .borrow()
            .as_ref()
            .is_some_and(|active| active.index == index)
        {
            return Ok(());
        }
        let stored = self
            .sources
            .get(index)
            .context("document chunk is missing")?;
        let html = stored.text()?;
        // Free the current chunk before building the next one: both being
        // resident at once doubles the layout peak, which matters on a device
        // where a single layout is already tens of megabytes. The cached page
        // bitmaps belong to the old chunk too, so drop them as well.
        drop(self.active.borrow_mut().take());
        self.clear_page_image_cache();
        *self.page_boxes_cache.borrow_mut() = None;
        trim_memory();
        let doc = create_document(
            &html,
            &self.base_url,
            self.net.clone(),
            self.font_context.clone(),
            &self.font_blobs,
            &self.layout_options_for_chunk(index),
            self.chunk_margins(index),
        );
        let mut lines = collect_lines(doc.root_element());
        let math_boxes = collect_math_boxes(&doc, doc.root_element());
        extend_lines_for_math(&mut lines, &math_boxes);
        let forced_breaks = page_breaks::collect_forced_breaks(&doc, doc.root_element());
        let mut boxes = collect_content_boxes(&doc, doc.root_element());
        for (top, bottom, display) in &math_boxes {
            if *display {
                boxes.push(ContentBox {
                    top: *top,
                    bottom: *bottom,
                });
            }
        }
        boxes.sort_by(|left, right| left.top.total_cmp(&right.top));
        let fixed_layout = self.fixed_layouts.get(index).and_then(|fixed| *fixed);
        let (_page_height_css_u32, page_height_css, page_margin_css) =
            if let Some(fixed) = fixed_layout {
                let height = self.fixed_size(fixed).1.max(1.0);
                (height as u32, height, 0.0)
            } else {
                css_page_geometry(
                    self.width,
                    self.height,
                    self.viewport_scale,
                    self.page_margin,
                )
            };
        let page_starts = if fixed_layout.is_some() {
            vec![PageStart {
                top: 0.0,
                clip_top: 0.0,
            }]
        } else {
            paginate(
                &lines,
                &boxes,
                &forced_breaks,
                page_height_css,
                page_margin_css,
                page_margin_css,
            )
        };
        *self.active.borrow_mut() = Some(ActiveChunk {
            index,
            rendered_text_nodes: rendered_text_nodes(&doc, &lines),
            doc,
            lines,
            page_starts,
        });
        trim_memory();
        Ok(())
    }

    pub(crate) fn with_active_chunk<T>(
        &self,
        index: usize,
        f: impl FnOnce(&mut ActiveChunk) -> Result<T>,
    ) -> Result<T> {
        self.ensure_chunk(index)?;
        let mut active = self.active.borrow_mut();
        let active = active
            .as_mut()
            .context("active document chunk is missing")?;
        f(active)
    }
}

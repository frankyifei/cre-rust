use anyrender::Scene;
use blitz_dom::{DocumentConfig, build_single_font_ctx};
use blitz_html::HtmlDocument;
use blitz_traits::shell::{ColorScheme, Viewport};

fn main() {
    let html = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<style>
  html, body { margin: 0; padding: 0; }
  body { font-family: serif; font-size: 20px; line-height: 1.4; padding: 20px; }
  h1 { font-size: 28px; margin: 0 0 0.6em; }
  p { margin: 0 0 1em; text-align: justify; }
  .note { font-size: 0.8em; color: #555; }
</style>
</head>
<body>
  <h1>Blitz WSL spike</h1>
  <p>This document is parsed, styled, and laid out by Blitz DOM inside WSL.</p>
  <p class="note">If this prints paint commands, the headless pipeline works.</p>
</body>
</html>"#;

    let font_data = std::fs::read("/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf")
        .expect("read DejaVuSerif.ttf");
    let config = DocumentConfig {
        viewport: Some(Viewport::new(600, 800, 1.0, ColorScheme::Light)),
        font_ctx: Some(build_single_font_ctx(&font_data)),
        ..Default::default()
    };

    let mut doc = HtmlDocument::from_html(html, config);
    doc.resolve(0.0);

    let root = doc.root_element();
    println!("root layout: {:?}", root.final_layout().size);

    let mut scene = Scene::default();
    blitz_paint::paint_scene(&mut scene, &mut doc, 1.0, 600, 800, 0, 0);

    println!("paint commands: {}", scene.commands.len());
}

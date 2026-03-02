use anyhow::{Context, Result, anyhow};
use resvg::tiny_skia::{Pixmap, Transform};
use tray_icon::Icon;
use usvg::Options;

pub fn load_svg_icon(data: &[u8], size: u32) -> Result<Icon> {
    let options = Options::default();
    let tree =
        usvg::Tree::from_data(data, &options).with_context(|| "parsing svg data".to_string())?;

    let mut pixmap = Pixmap::new(size, size).context("allocating icon pixmap")?;
    let svg_size = tree.size();
    let width = svg_size.width().max(1.0);
    let height = svg_size.height().max(1.0);
    let scale = (size as f32 / width).min(size as f32 / height);
    let scaled_w = width * scale;
    let scaled_h = height * scale;
    let translate_x = ((size as f32 - scaled_w) / 2.0).max(0.0);
    let translate_y = ((size as f32 - scaled_h) / 2.0).max(0.0);
    let transform = Transform::from_translate(translate_x, translate_y).post_scale(scale, scale);

    let mut surface = pixmap.as_mut();
    resvg::render(&tree, transform, &mut surface);

    let data = pixmap.take();
    // Removed the loop that forced pixels to white.
    // Now we just pass the RGBA data directly to the Icon.

    Icon::from_rgba(data, size, size).map_err(|e| anyhow!(e))
}

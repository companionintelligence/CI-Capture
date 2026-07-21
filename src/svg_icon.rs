use anyhow::{Context, Result, anyhow};
use image::imageops::FilterType;
use tray_icon::Icon;

pub fn load_png_icon(data: &[u8], size: u32) -> Result<Icon> {
    let img = image::load_from_memory(data).context("decoding png icon")?;
    let resized = image::imageops::resize(&img.to_rgba8(), size, size, FilterType::Lanczos3);
    let mut rgba = resized.into_raw();

    #[cfg(target_os = "macos")]
    normalize_template_icon(&mut rgba);

    Icon::from_rgba(rgba, size, size).map_err(|e| anyhow!(e))
}

#[cfg(target_os = "macos")]
fn normalize_template_icon(data: &mut [u8]) {
    for pixel in data.chunks_exact_mut(4) {
        if pixel[3] == 0 {
            continue;
        }

        // macOS template status-bar icons render most reliably as a
        // monochrome alpha mask rather than preserving source RGB values.
        pixel[0] = 255;
        pixel[1] = 255;
        pixel[2] = 255;
    }
}

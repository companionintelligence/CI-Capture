use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use image::{ColorType, ImageEncoder, RgbaImage, codecs::png::PngEncoder};
use screenshots::Screen;

#[derive(Clone)]
pub struct CapturedImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>, // RGBA
}

impl CapturedImage {
    pub fn to_png_bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut bytes);
        let encoder = PngEncoder::new(&mut cursor);
        encoder
            .write_image(&self.data, self.width, self.height, ColorType::Rgba8.into())
            .context("encoding png")?;
        Ok(bytes)
    }

    pub fn to_image(&self) -> Result<RgbaImage> {
        RgbaImage::from_raw(self.width, self.height, self.data.clone())
            .context("converting screenshot buffer")
    }
}

pub fn capture_primary() -> Result<CapturedImage> {
    let screens = Screen::all().context("enumerating displays")?;
    let screen = screens.into_iter().next().context("no displays detected")?;
    let image = screen.capture().context("capturing display")?;
    let width = image.width();
    let height = image.height();
    let data = image.into_raw();
    Ok(CapturedImage {
        width,
        height,
        data,
    })
}

pub fn diff_percent(prev: &CapturedImage, current: &CapturedImage) -> f32 {
    if prev.width != current.width || prev.height != current.height {
        return 100.0;
    }

    let total_pixels = (prev.width * prev.height) as usize;
    if total_pixels == 0 {
        return 0.0;
    }

    let mut changed_pixels = 0usize;
    for (px_prev, px_curr) in prev.data.chunks_exact(4).zip(current.data.chunks_exact(4)) {
        if px_prev != px_curr {
            changed_pixels += 1;
        }
    }

    (changed_pixels as f32 / total_pixels as f32) * 100.0
}

pub fn save_to_path(image: &CapturedImage, path: &Path) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating screenshot directory {}", parent.display()))?;
    }
    let rgba = image.to_image()?;
    rgba.save(path)
        .with_context(|| format!("saving screenshot to {}", path.display()))?;
    Ok(path.to_path_buf())
}

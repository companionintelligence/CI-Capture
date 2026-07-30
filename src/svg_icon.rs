//! Tray-icon rasterization.
//!
//! The status-bar glyph is an SVG rendered at runtime via resvg rather than a
//! baked PNG, so it stays crisp at every platform's icon size without shipping
//! a size ladder.
//!
//! **macOS template images.** The tray is built with
//! `with_icon_as_template(true)`, which makes AppKit discard RGB entirely and
//! draw the icon from its **alpha channel**, auto-inverting for light and dark
//! menu bars. Two consequences drive this module:
//!
//! 1. Source artwork must carry its shape in *coverage*. A glyph that is a
//!    solid opaque field with colour-only detail — as the old
//!    `packaging/icons/icon.png` was, 52% fully opaque — collapses to a
//!    featureless blob. `assets/tray-mark.svg` is filled geometry for exactly
//!    this reason.
//! 2. Rewriting RGB to white while leaving alpha alone (the old
//!    `normalize_template_icon`) accomplishes nothing under `setTemplate:YES`,
//!    because those channels are never sampled. It has been removed rather
//!    than kept as reassuring dead code.

use anyhow::{Context, Result, anyhow};
use resvg::tiny_skia::{Pixmap, Transform};
use tray_icon::Icon;
use usvg::Options;

/// The tray glyph, rasterized per-platform at [`tray_icon_size`].
const TRAY_MARK: &[u8] = include_bytes!("../assets/tray-mark.svg");

/// Native status-bar icon size, in pixels.
///
/// - **macOS** renders at a fixed 18pt; 36px covers Retina at 2×.
/// - **Windows** draws the notification-area icon at `SM_CXSMICON` (16px at
///   100% DPI) and `CreateIcon` does *not* resample, so anything else gets
///   scaled by the shell with visible artefacts.
/// - **Linux** AppIndicator/StatusNotifier panels are typically 22px.
pub const fn tray_icon_size() -> u32 {
    #[cfg(target_os = "macos")]
    {
        36
    }
    #[cfg(target_os = "windows")]
    {
        16
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        22
    }
}

/// Rasterize the embedded tray glyph at the platform's native size.
pub fn load_tray_icon() -> Result<Icon> {
    load_svg_icon(TRAY_MARK, tray_icon_size())
}

/// Rasterize `data` into a square `size`×`size` tray [`Icon`], aspect-fit and
/// centred.
pub fn load_svg_icon(data: &[u8], size: u32) -> Result<Icon> {
    let tree = usvg::Tree::from_data(data, &Options::default()).context("parsing svg icon")?;

    let mut pixmap = Pixmap::new(size, size).context("allocating icon pixmap")?;
    let svg_size = tree.size();
    let width = svg_size.width().max(1.0);
    let height = svg_size.height().max(1.0);
    let scale = (size as f32 / width).min(size as f32 / height);
    let translate_x = ((size as f32 - width * scale) / 2.0).max(0.0);
    let translate_y = ((size as f32 - height * scale) / 2.0).max(0.0);

    resvg::render(
        &tree,
        Transform::from_translate(translate_x, translate_y).post_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    Icon::from_rgba(pixmap.take(), size, size).map_err(|e| anyhow!(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rasterize(size: u32) -> Vec<u8> {
        let tree = usvg::Tree::from_data(TRAY_MARK, &Options::default()).unwrap();
        let mut pixmap = Pixmap::new(size, size).unwrap();
        let s = tree.size();
        let scale = (size as f32 / s.width()).min(size as f32 / s.height());
        resvg::render(
            &tree,
            Transform::from_translate(
                (size as f32 - s.width() * scale) / 2.0,
                (size as f32 - s.height() * scale) / 2.0,
            )
            .post_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        pixmap.take()
    }

    #[test]
    fn embedded_tray_mark_rasterizes_at_every_platform_size() {
        for size in [16, 18, 22, 32, 36, 44] {
            assert!(
                load_svg_icon(TRAY_MARK, size).is_ok(),
                "tray mark failed to rasterize at {size}px"
            );
        }
    }

    /// The whole point of the glyph: it must be a silhouette, not a filled
    /// field, or macOS's template rendering flattens it to a blob. Guards
    /// against someone swapping in artwork with a solid background.
    #[test]
    fn tray_mark_is_a_silhouette_not_a_solid_field() {
        let size = tray_icon_size();
        let px = rasterize(size);
        let total = px.len() / 4;
        let opaque = px.chunks_exact(4).filter(|p| p[3] > 200).count();
        let clear = px.chunks_exact(4).filter(|p| p[3] < 16).count();
        let coverage = opaque as f32 / total as f32;

        // A recognisable mark inks a meaningful minority of its box and leaves
        // real negative space. The old PNG was 52% opaque / 40% clear and read
        // as a disc; these bounds would have caught it.
        assert!(
            (0.10..=0.45).contains(&coverage),
            "tray mark inks {:.0}% of its box — too {} to read at {size}px",
            coverage * 100.0,
            if coverage > 0.45 { "solid" } else { "sparse" }
        );
        assert!(
            clear as f32 / total as f32 > 0.35,
            "tray mark has too little transparent negative space to read as a template image"
        );
    }

    /// The ring must survive the smallest size we ship at (Windows, 16px).
    /// If it ever thins below a device pixel the glyph reads as a dot.
    #[test]
    fn tray_mark_ring_survives_the_smallest_platform_size() {
        let px = rasterize(16);
        // Scan the middle row: ring, gap, core, gap, ring => 3 inked runs.
        let row = 8usize;
        let mut runs = 0;
        let mut inside = false;
        for x in 0..16usize {
            let inked = px[(row * 16 + x) * 4 + 3] > 96;
            if inked && !inside {
                runs += 1;
            }
            inside = inked;
        }
        assert_eq!(
            runs, 3,
            "expected ring/core/ring across the middle row at 16px, found {runs} inked runs"
        );
    }
}

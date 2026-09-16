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
//!
//! **Everywhere else the RGB *is* sampled.** `with_icon_as_template` is a
//! macOS-only hint; the Windows notification area and Linux
//! StatusNotifier/AppIndicator panels draw the icon's actual colours. The
//! artwork is a solid `#000000` silhouette — right for a template, but a black
//! blob on the dark taskbar Windows 10 ships with and on GNOME's always-dark
//! top bar. So off macOS the rasterised pixmap is recoloured to suit the panel
//! ([`tray_glyph_colour`]) before it becomes an [`Icon`]: alpha is left alone
//! (it *is* the shape) and only RGB is rewritten, which keeps the one SVG
//! source and the macOS path byte-for-byte unchanged.
//!
//! **Alpha convention.** `tiny_skia` hands back *premultiplied* RGBA, but
//! every `tray-icon` backend that samples RGB treats `Icon::from_rgba` data as
//! *straight* alpha: the Linux backend PNG-encodes the buffer verbatim
//! (`png::ColorType::Rgba`, straight by spec) for AppIndicator/GdkPixbuf, and
//! the Windows backend swaps to BGRA and feeds `CreateIcon` a 32bpp bitmap,
//! which is straight ARGB. The recolour therefore writes straight ink
//! ([`recolour_rgba`]); the black artwork never exposed the mismatch because
//! `0 × α` is `0` either way.

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

    #[cfg(target_os = "macos")]
    let rgba = pixmap.take();
    #[cfg(not(target_os = "macos"))]
    let rgba = {
        let mut rgba = pixmap.take();
        let colour = tray_glyph_colour();
        log::info!(
            "Tray glyph: recolouring to {} for the {} panel",
            colour.name(),
            match colour {
                GlyphColour::White => "dark",
                GlyphColour::Black => "light",
            }
        );
        recolour_rgba(&mut rgba, colour);
        rgba
    };

    Icon::from_rgba(rgba, size, size).map_err(|e| anyhow!(e))
}

/// The two inks a monochrome panel glyph can sensibly be.
///
/// Only reached from tests on macOS, where the template path skips
/// recolouring entirely; the pixel helper still compiles and is tested there
/// so a regression cannot hide until the Windows/Linux runners see it.
#[cfg_attr(target_os = "macos", allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphColour {
    /// For dark panels.
    White,
    /// For light panels.
    Black,
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
impl GlyphColour {
    /// Lower-case ink name for log lines.
    fn name(self) -> &'static str {
        match self {
            GlyphColour::White => "white",
            GlyphColour::Black => "black",
        }
    }

    /// The RGB triple this ink rewrites into every pixel.
    const fn rgb(self) -> [u8; 3] {
        match self {
            GlyphColour::White => [255, 255, 255],
            GlyphColour::Black => [0, 0, 0],
        }
    }
}

/// Rewrite the RGB of every inked pixel in an RGBA buffer to `colour` as
/// **straight** (un-premultiplied) ink, preserving alpha.
///
/// The input comes from `Pixmap::take()`, which `tiny_skia` documents as
/// premultiplied — a half-covered edge pixel of the black glyph is
/// `(0, 0, 0, 128)`. The consumer, however, is not premultiplied: `tray-icon`
/// 0.11 hands `Icon::from_rgba` bytes straight to a PNG encoder on Linux
/// (`png::ColorType::Rgba`, loaded by AppIndicator via GdkPixbuf) and to
/// `CreateIcon` as a 32bpp bitmap on Windows, and both read alpha as
/// straight. Writing premultiplied white (`(128, 128, 128, 128)`) there is
/// decoded as 50% grey at 50% opacity, which composites to a dark fringe and a
/// visibly thinner glyph; the correct straight value is `(255, 255, 255, 128)`.
/// Black is the degenerate case (`0 × α` is still `0`), which is why the
/// original artwork never exposed this. Fully transparent pixels stay
/// `(0, 0, 0, 0)`.
///
/// Pure pixel arithmetic so it can be tested on every CI runner; the
/// platform-specific part is only *which* colour to ask for.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn recolour_rgba(rgba: &mut [u8], colour: GlyphColour) {
    let [r, g, b] = colour.rgb();
    for px in rgba.chunks_exact_mut(4) {
        if px[3] > 0 {
            px[0] = r;
            px[1] = g;
            px[2] = b;
        } else {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
        }
    }
}

/// Pick the glyph ink for the panel this process's tray will sit in.
///
/// The question is "is the *panel* dark?", which is not quite the question
/// the theme APIs answer, so the sources are consulted in order of how well
/// they approximate it:
///
/// - **Windows:** the taskbar follows `SystemUsesLightTheme` under
///   `HKCU\…\Themes\Personalize`, which is a separate switch from the
///   `AppsUseLightTheme` that `dark-light` reads. Windows 10's default is
///   "Windows mode: dark, app mode: light" — a dark taskbar under light apps —
///   so trusting the apps switch would paint a black glyph on a dark taskbar
///   for every default Windows 10 install, i.e. the exact bug this fixes. The
///   value only exists on 1903+; older builds have an unconditionally dark
///   taskbar, so a key that opens but lacks the value means *dark*. Only when
///   the key itself cannot be opened do we fall through to `dark-light`.
/// - **Linux/BSD:** `dark-light` asks the XDG settings portal for
///   `org.freedesktop.appearance color-scheme`. KDE and GNOME both publish it.
///
/// **`Unspecified` and errors resolve to white, not black.** GNOME reports
/// `Unspecified` (`color-scheme = default`) in its light theme, and its top bar
/// is dark in that theme regardless; a session with no portal at all (i3,
/// sway without `xdg-desktop-portal-wlr`, bare X11) most often carries a dark
/// bar too. Only an explicit *Light* answer earns black ink.
#[cfg(not(target_os = "macos"))]
pub fn tray_glyph_colour() -> GlyphColour {
    #[cfg(windows)]
    {
        if let Some(colour) = windows_taskbar_glyph_colour() {
            return colour;
        }
    }
    match dark_light::detect() {
        Ok(dark_light::Mode::Light) => GlyphColour::Black,
        Ok(dark_light::Mode::Dark) => GlyphColour::White,
        Ok(dark_light::Mode::Unspecified) => {
            log::info!(
                "Tray glyph: system reports no light/dark preference; assuming a dark panel"
            );
            GlyphColour::White
        }
        Err(err) => {
            log::warn!(
                "Tray glyph: could not read the system theme ({err}); assuming a dark panel"
            );
            GlyphColour::White
        }
    }
}

/// The Windows taskbar's own light/dark switch — see [`tray_glyph_colour`]
/// for why this is *not* left to `dark-light`. `None` only when the
/// `Personalize` key itself cannot be opened; a key without the value is a
/// pre-1903 build whose taskbar is always dark.
#[cfg(windows)]
fn windows_taskbar_glyph_colour() -> Option<GlyphColour> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

    let personalize = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(
            r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
            KEY_READ,
        )
        .ok()?;
    let colour = match personalize.get_value::<u32, _>("SystemUsesLightTheme") {
        Ok(0) => GlyphColour::White,
        Ok(_) => GlyphColour::Black,
        Err(err) => {
            log::info!(
                "Tray glyph: SystemUsesLightTheme unreadable ({err}); assuming a dark taskbar"
            );
            GlyphColour::White
        }
    };
    Some(colour)
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

    /// Off macOS the panel samples RGB, so the glyph must actually be
    /// repainted — and repainted as *straight* alpha, because tray-icon
    /// PNG-encodes the buffer on Linux and hands it to `CreateIcon` on
    /// Windows, neither of which un-premultiplies. A premultiplied edge would
    /// read as grey-at-half-opacity and dim the anti-aliased rim.
    #[test]
    fn recolour_rewrites_rgb_straight_alpha_and_leaves_alpha_alone() {
        // Opaque black, half-covered black edge, fully clear.
        let mut px = vec![0, 0, 0, 255, 0, 0, 0, 128, 0, 0, 0, 0];
        recolour_rgba(&mut px, GlyphColour::White);
        assert_eq!(&px[0..4], &[255, 255, 255, 255], "opaque pixel");
        assert_eq!(
            &px[4..8],
            &[255, 255, 255, 128],
            "edge pixel is straight white at its original coverage"
        );
        assert_eq!(&px[8..12], &[0, 0, 0, 0], "clear pixel stays clear");

        // And back: black is the degenerate case, alpha untouched.
        recolour_rgba(&mut px, GlyphColour::Black);
        assert_eq!(px, vec![0, 0, 0, 255, 0, 0, 0, 128, 0, 0, 0, 0]);
    }

    /// A stray premultiplied-looking input (grey RGB under partial alpha) must
    /// be normalised to the ink, not scaled — the function overwrites RGB, it
    /// does not multiply it.
    #[test]
    fn recolour_overwrites_rather_than_scales_existing_rgb() {
        let mut px = vec![128, 128, 128, 128, 7, 9, 11, 1, 200, 200, 200, 0];
        recolour_rgba(&mut px, GlyphColour::White);
        assert_eq!(&px[0..4], &[255, 255, 255, 128]);
        assert_eq!(&px[4..8], &[255, 255, 255, 1], "alpha 1 is still inked");
        assert_eq!(&px[8..12], &[0, 0, 0, 0], "alpha 0 is scrubbed to clear");
    }

    /// The shape lives in alpha; recolouring must not move a single pixel of
    /// coverage or the ring/core test above would silently start measuring a
    /// different glyph on Windows and Linux.
    #[test]
    fn recolouring_the_rasterised_mark_preserves_its_coverage() {
        let size = tray_icon_size();
        let before = rasterize(size);
        let mut after = before.clone();
        recolour_rgba(&mut after, GlyphColour::White);
        let alpha = |px: &[u8]| px.chunks_exact(4).map(|p| p[3]).collect::<Vec<_>>();
        assert_eq!(alpha(&before), alpha(&after));
        // Every inked pixel is now pure straight white regardless of coverage;
        // every clear pixel is (0, 0, 0, 0).
        for p in after.chunks_exact(4) {
            let expected = if p[3] > 0 { [255, 255, 255] } else { [0, 0, 0] };
            assert_eq!([p[0], p[1], p[2]], expected, "pixel {p:?}");
        }
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

//! Companion Intelligence canonical design tokens as egui colours.
//!
//! **Source of truth:** `@companionintelligence/tokens` (CI-Common `styles/tokens/src/tokens.json`,
//! the one hand-edited token file), consumed as the generated Rust target in the brand bundle
//! (`vendor/brand/tokens/tokens.rs`, `include!`d by [`crate::brand`]). This module is only an
//! adapter: it keeps the `tokens::dark::PRIMARY`-style API the UI uses and turns each `#rrggbb`
//! constant into a `Color32` at compile time. Nothing here is typed by hand any more — the
//! previous version of this file was a transcription of v0.2.1 that carried the retired teal.
//!
//! Semantic naming (`PANEL`, `TEXT`, `TEAL`, …) lives in [`crate::theme`].

#![allow(dead_code)]

use crate::brand;
use eframe::egui::Color32;

/// `"#rrggbb"` (a generated constant) → `Color32`, in a `const` context.
const fn c(hex: &str) -> Color32 {
    let brand::Rgb(r, g, b) = brand::rgb(hex);
    Color32::from_rgb(r, g, b)
}

/// Dark mode (`tokens.dark`).
pub mod dark {
    use super::{brand, c, Color32};

    pub const BACKGROUND: Color32 = c(brand::DARK_BACKGROUND);
    pub const FOREGROUND: Color32 = c(brand::DARK_FOREGROUND);
    pub const CARD: Color32 = c(brand::DARK_CARD);
    pub const CARD_FOREGROUND: Color32 = c(brand::DARK_CARD_FOREGROUND);
    pub const POPOVER: Color32 = c(brand::DARK_POPOVER);
    pub const POPOVER_FOREGROUND: Color32 = c(brand::DARK_POPOVER_FOREGROUND);
    pub const PRIMARY: Color32 = c(brand::DARK_PRIMARY);
    pub const PRIMARY_FOREGROUND: Color32 = c(brand::DARK_PRIMARY_FOREGROUND);
    pub const SECONDARY: Color32 = c(brand::DARK_SECONDARY);
    pub const SECONDARY_FOREGROUND: Color32 = c(brand::DARK_SECONDARY_FOREGROUND);
    pub const MUTED: Color32 = c(brand::DARK_MUTED);
    pub const MUTED_FOREGROUND: Color32 = c(brand::DARK_MUTED_FOREGROUND);
    pub const ACCENT: Color32 = c(brand::DARK_ACCENT);
    pub const ACCENT_FOREGROUND: Color32 = c(brand::DARK_ACCENT_FOREGROUND);
    pub const DESTRUCTIVE: Color32 = c(brand::DARK_DESTRUCTIVE);
    pub const DESTRUCTIVE_FOREGROUND: Color32 = c(brand::DARK_DESTRUCTIVE_FOREGROUND);
    pub const BORDER: Color32 = c(brand::DARK_BORDER);
    pub const INPUT: Color32 = c(brand::DARK_INPUT);
    pub const RING: Color32 = c(brand::DARK_RING);
    pub const CHART_1: Color32 = c(brand::DARK_CHART_1);
    pub const CHART_2: Color32 = c(brand::DARK_CHART_2);
    pub const CHART_3: Color32 = c(brand::DARK_CHART_3);
    pub const CHART_4: Color32 = c(brand::DARK_CHART_4);
    pub const CHART_5: Color32 = c(brand::DARK_CHART_5);
    pub const SIDEBAR: Color32 = c(brand::DARK_SIDEBAR);
    pub const SIDEBAR_FOREGROUND: Color32 = c(brand::DARK_SIDEBAR_FOREGROUND);
    pub const SIDEBAR_PRIMARY: Color32 = c(brand::DARK_SIDEBAR_PRIMARY);
    pub const SIDEBAR_PRIMARY_FOREGROUND: Color32 = c(brand::DARK_SIDEBAR_PRIMARY_FOREGROUND);
    pub const SIDEBAR_ACCENT: Color32 = c(brand::DARK_SIDEBAR_ACCENT);
    pub const SIDEBAR_ACCENT_FOREGROUND: Color32 = c(brand::DARK_SIDEBAR_ACCENT_FOREGROUND);
    pub const SIDEBAR_BORDER: Color32 = c(brand::DARK_SIDEBAR_BORDER);
    pub const SIDEBAR_RING: Color32 = c(brand::DARK_SIDEBAR_RING);
    /// Semantic status (tokens.status.dark).
    pub const SUCCESS: Color32 = c(brand::STATUS_DARK_SUCCESS);
    pub const SUCCESS_FOREGROUND: Color32 = c(brand::STATUS_DARK_SUCCESS_FOREGROUND);
    pub const WARNING: Color32 = c(brand::STATUS_DARK_WARNING);
    pub const WARNING_FOREGROUND: Color32 = c(brand::STATUS_DARK_WARNING_FOREGROUND);
    pub const INFO: Color32 = c(brand::STATUS_DARK_INFO);
    pub const INFO_FOREGROUND: Color32 = c(brand::STATUS_DARK_INFO_FOREGROUND);
}

/// Light mode (`tokens.light`).
pub mod light {
    use super::{brand, c, Color32};

    pub const BACKGROUND: Color32 = c(brand::LIGHT_BACKGROUND);
    pub const FOREGROUND: Color32 = c(brand::LIGHT_FOREGROUND);
    pub const CARD: Color32 = c(brand::LIGHT_CARD);
    pub const CARD_FOREGROUND: Color32 = c(brand::LIGHT_CARD_FOREGROUND);
    pub const POPOVER: Color32 = c(brand::LIGHT_POPOVER);
    pub const POPOVER_FOREGROUND: Color32 = c(brand::LIGHT_POPOVER_FOREGROUND);
    pub const PRIMARY: Color32 = c(brand::LIGHT_PRIMARY);
    pub const PRIMARY_FOREGROUND: Color32 = c(brand::LIGHT_PRIMARY_FOREGROUND);
    pub const SECONDARY: Color32 = c(brand::LIGHT_SECONDARY);
    pub const SECONDARY_FOREGROUND: Color32 = c(brand::LIGHT_SECONDARY_FOREGROUND);
    pub const MUTED: Color32 = c(brand::LIGHT_MUTED);
    pub const MUTED_FOREGROUND: Color32 = c(brand::LIGHT_MUTED_FOREGROUND);
    pub const ACCENT: Color32 = c(brand::LIGHT_ACCENT);
    pub const ACCENT_FOREGROUND: Color32 = c(brand::LIGHT_ACCENT_FOREGROUND);
    pub const DESTRUCTIVE: Color32 = c(brand::LIGHT_DESTRUCTIVE);
    pub const DESTRUCTIVE_FOREGROUND: Color32 = c(brand::LIGHT_DESTRUCTIVE_FOREGROUND);
    pub const BORDER: Color32 = c(brand::LIGHT_BORDER);
    pub const INPUT: Color32 = c(brand::LIGHT_INPUT);
    pub const RING: Color32 = c(brand::LIGHT_RING);
    pub const CHART_1: Color32 = c(brand::LIGHT_CHART_1);
    pub const CHART_2: Color32 = c(brand::LIGHT_CHART_2);
    pub const CHART_3: Color32 = c(brand::LIGHT_CHART_3);
    pub const CHART_4: Color32 = c(brand::LIGHT_CHART_4);
    pub const CHART_5: Color32 = c(brand::LIGHT_CHART_5);
    pub const SIDEBAR: Color32 = c(brand::LIGHT_SIDEBAR);
    pub const SIDEBAR_FOREGROUND: Color32 = c(brand::LIGHT_SIDEBAR_FOREGROUND);
    pub const SIDEBAR_PRIMARY: Color32 = c(brand::LIGHT_SIDEBAR_PRIMARY);
    pub const SIDEBAR_PRIMARY_FOREGROUND: Color32 = c(brand::LIGHT_SIDEBAR_PRIMARY_FOREGROUND);
    pub const SIDEBAR_ACCENT: Color32 = c(brand::LIGHT_SIDEBAR_ACCENT);
    pub const SIDEBAR_ACCENT_FOREGROUND: Color32 = c(brand::LIGHT_SIDEBAR_ACCENT_FOREGROUND);
    pub const SIDEBAR_BORDER: Color32 = c(brand::LIGHT_SIDEBAR_BORDER);
    pub const SIDEBAR_RING: Color32 = c(brand::LIGHT_SIDEBAR_RING);
    /// Semantic status (tokens.status.light).
    pub const SUCCESS: Color32 = c(brand::STATUS_LIGHT_SUCCESS);
    pub const SUCCESS_FOREGROUND: Color32 = c(brand::STATUS_LIGHT_SUCCESS_FOREGROUND);
    pub const WARNING: Color32 = c(brand::STATUS_LIGHT_WARNING);
    pub const WARNING_FOREGROUND: Color32 = c(brand::STATUS_LIGHT_WARNING_FOREGROUND);
    pub const INFO: Color32 = c(brand::STATUS_LIGHT_INFO);
    pub const INFO_FOREGROUND: Color32 = c(brand::STATUS_LIGHT_INFO_FOREGROUND);
}

/// The brand teal ramp (`tokens.ramp`), mode-independent. `T600`/`T200` are ramp rungs,
/// not the primaries — the primaries are `dark::PRIMARY` / `light::PRIMARY`.
pub mod teal {
    use super::{brand, c, Color32};

    pub const T50: Color32 = c(brand::RAMP_TEAL_50);
    pub const T100: Color32 = c(brand::RAMP_TEAL_100);
    pub const T200: Color32 = c(brand::RAMP_TEAL_200);
    pub const T300: Color32 = c(brand::RAMP_TEAL_300);
    pub const T400: Color32 = c(brand::RAMP_TEAL_400);
    pub const T500: Color32 = c(brand::RAMP_TEAL_500);
    pub const T600: Color32 = c(brand::RAMP_TEAL_600);
    pub const T700: Color32 = c(brand::RAMP_TEAL_700);
    pub const T800: Color32 = c(brand::RAMP_TEAL_800);
    pub const T900: Color32 = c(brand::RAMP_TEAL_900);
    pub const T950: Color32 = c(brand::RAMP_TEAL_950);
}

/// Accent hues (`tokens.accent`), mode-independent.
pub mod accent {
    use super::{brand, c, Color32};

    pub const AQUA: Color32 = c(brand::ACCENT_AQUA);
    pub const AQUA_LIGHT: Color32 = c(brand::ACCENT_AQUA_LIGHT);
    /// The canon has no separate "mint"; the bright green-teal bloom is `aqua-light`.
    pub const MINT: Color32 = c(brand::ACCENT_AQUA_LIGHT);
    pub const TEAL_ACCENT: Color32 = c(brand::ACCENT_TEAL_ACCENT);
    pub const TEAL_MID: Color32 = c(brand::ACCENT_TEAL_MID);
    pub const PHTHALO: Color32 = c(brand::ACCENT_PHTHALO);
}

pub mod radius {
    pub const SM: u8 = 6;
    pub const MD: u8 = 8;
    pub const LG: u8 = 10;
    pub const XL: u8 = 14;
    pub const XXL: u8 = 20;
}

/// Type scale, in points.
///
/// CI-Common tokenizes **no** numeric type scale — `UI-STYLE-GUIDE.md` defers
/// to Tailwind's defaults and adds only `xxs`/`tiny` for micro-labels. These
/// are the applet's own scale, chosen against Tailwind's steps and sized for a
/// desktop window read at arm's length.
pub mod text {
    pub const DISPLAY: f32 = 30.0;
    pub const HEADING: f32 = 22.0;
    pub const TITLE: f32 = 18.0;
    pub const BODY: f32 = 16.0;
    pub const BUTTON: f32 = 15.0;
    pub const LABEL: f32 = 14.0;
    pub const SMALL: f32 = 13.0;
    pub const MONO: f32 = 14.0;
    /// Floor for anything that must stay legible. Nothing should go below this.
    pub const MIN: f32 = 12.0;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The adapter must track the vendored canon exactly — the previous hand transcription
    /// drifted for a month. Compares the egui colours against the generated string constants.
    #[test]
    fn primaries_are_the_canon() {
        let brand::Rgb(r, g, b) = brand::rgb(brand::LIGHT_PRIMARY);
        assert_eq!(light::PRIMARY, Color32::from_rgb(r, g, b));
        let brand::Rgb(r, g, b) = brand::rgb(brand::DARK_PRIMARY);
        assert_eq!(dark::PRIMARY, Color32::from_rgb(r, g, b));
        // and never a retired value
        for (hex, _) in brand::RETIRED {
            let brand::Rgb(r, g, b) = brand::rgb(hex);
            assert_ne!(light::PRIMARY, Color32::from_rgb(r, g, b), "light primary is retired {hex}");
            assert_ne!(dark::PRIMARY, Color32::from_rgb(r, g, b), "dark primary is retired {hex}");
        }
    }
}

//! Companion Intelligence canonical design tokens, transcribed for Rust.
//!
//! **Source of truth:** `@companionintelligence/tokens` v0.2.1 —
//! `CI-Common/packages/tokens/src/tokens.json` (which in turn mirrors
//! `CI-Hub/packages/frontend/src/styles/globals.css`, authored in oklch).
//!
//! CI-Common publishes to GitHub Packages under the `@companionintelligence`
//! npm scope and ships no Rust crate, so there is no way to *depend* on it from
//! Cargo. These constants are therefore a hand-transcription. When CI-Common
//! bumps its token version, re-transcribe this file and update the version in
//! the doc comment above — do not drift the values locally.
//!
//! Only the tokens the applet actually renders are transcribed. Both `dark`
//! and `light` are wired into real `Visuals` in [`crate::theme::Palette`],
//! which follows the OS appearance.
//!
//! Semantic naming (`PANEL`, `TEXT`, `TEAL`, …) lives in [`crate::theme`];
//! this module stays a faithful mirror of the upstream token names.

#![allow(dead_code)]

use eframe::egui::Color32;

/// `#rrggbb` → `Color32`, so the constants below read like the source JSON.
const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// Dark mode.
pub mod dark {
    use super::{Color32, hex};

    pub const BACKGROUND: Color32 = hex(0x041620);
    pub const FOREGROUND: Color32 = hex(0x88a29e);
    pub const CARD: Color32 = hex(0x0a222e);
    pub const CARD_FOREGROUND: Color32 = hex(0x88a29e);
    pub const POPOVER: Color32 = hex(0x0a222e);
    pub const POPOVER_FOREGROUND: Color32 = hex(0x88a29e);
    /// `oklch(84.195% 0.043 203.701)`
    pub const PRIMARY: Color32 = hex(0xabd4d8);
    pub const PRIMARY_FOREGROUND: Color32 = hex(0x041620);
    pub const SECONDARY: Color32 = hex(0x0a222e);
    /// The high-contrast near-white. Body copy that must be maximally legible
    /// uses this rather than `FOREGROUND` (a ~6:1 desaturated sage).
    pub const SECONDARY_FOREGROUND: Color32 = hex(0xf5f8fa);
    pub const MUTED: Color32 = hex(0x0a222e);
    pub const MUTED_FOREGROUND: Color32 = hex(0x8f98a3);
    pub const ACCENT: Color32 = hex(0x2c676d);
    pub const ACCENT_FOREGROUND: Color32 = hex(0xf5f8fa);
    pub const DESTRUCTIVE: Color32 = hex(0xef7070);
    pub const DESTRUCTIVE_FOREGROUND: Color32 = hex(0xf5f8fa);
    pub const BORDER: Color32 = hex(0x2c676d);
    pub const INPUT: Color32 = hex(0x6992a2);
    pub const RING: Color32 = hex(0x58ebbf);

    pub const CHART_1: Color32 = hex(0x58ebbf);
    pub const CHART_2: Color32 = hex(0x409b9b);
    pub const CHART_3: Color32 = hex(0x079d99);
    pub const CHART_4: Color32 = hex(0x70a6af);
    pub const CHART_5: Color32 = hex(0x2c676d);

    pub const SIDEBAR: Color32 = hex(0x041620);
    pub const SIDEBAR_FOREGROUND: Color32 = hex(0xf5f8fa);
    pub const SIDEBAR_PRIMARY: Color32 = hex(0xabd4d8);
    pub const SIDEBAR_ACCENT: Color32 = hex(0x1d293d);
    pub const SIDEBAR_BORDER: Color32 = hex(0x2c676d);
    pub const SIDEBAR_RING: Color32 = hex(0x58ebbf);

    // Status (tokens.json → status.dark).
    pub const SUCCESS: Color32 = hex(0x4ade80);
    pub const SUCCESS_FOREGROUND: Color32 = hex(0x052e16);
    pub const WARNING: Color32 = hex(0xfbbf24);
    pub const WARNING_FOREGROUND: Color32 = hex(0x422006);
    pub const INFO: Color32 = hex(0x38bdf8);
    pub const INFO_FOREGROUND: Color32 = hex(0x082f49);
}

/// Light mode.
pub mod light {
    use super::{Color32, hex};

    pub const BACKGROUND: Color32 = hex(0xf3f3f3);
    pub const FOREGROUND: Color32 = hex(0x010f16);
    pub const CARD: Color32 = hex(0xf7f7f9);
    pub const CARD_FOREGROUND: Color32 = hex(0x041620);
    /// `oklch(50.2043% 0.0822 204.9225)`
    pub const PRIMARY: Color32 = hex(0x0f717a);
    pub const PRIMARY_FOREGROUND: Color32 = hex(0xf8fafc);
    pub const SECONDARY_FOREGROUND: Color32 = hex(0x0a222e);
    pub const MUTED_FOREGROUND: Color32 = hex(0x62748e);
    pub const ACCENT_FOREGROUND: Color32 = hex(0x0a222e);
    pub const DESTRUCTIVE: Color32 = hex(0xee3533);
    pub const BORDER: Color32 = hex(0xe2e8f0);
    pub const INPUT: Color32 = hex(0xe2e8f0);
    pub const RING: Color32 = hex(0x90a1b9);

    pub const SUCCESS: Color32 = hex(0x16a34a);
    pub const WARNING: Color32 = hex(0xd97706);
    pub const INFO: Color32 = hex(0x0284c7);

    pub const CHART_1: Color32 = hex(0xf54900);
    pub const CHART_2: Color32 = hex(0x009689);
    pub const CHART_3: Color32 = hex(0x104e64);
    pub const CHART_4: Color32 = hex(0xffb900);
    pub const CHART_5: Color32 = hex(0xfe9a00);

    pub const SIDEBAR: Color32 = hex(0xf8fafc);
    pub const SIDEBAR_FOREGROUND: Color32 = hex(0x020618);
    pub const SIDEBAR_PRIMARY: Color32 = hex(0x0f172b);
    pub const SIDEBAR_ACCENT: Color32 = hex(0xf1f5f9);
    pub const SIDEBAR_BORDER: Color32 = hex(0xe2e8f0);
    pub const SIDEBAR_RING: Color32 = hex(0x70a6af);
}

/// Brand teal ramp — mode-independent.
///
/// Per `CI-Common/docs/UI-STYLE-GUIDE.md`: "Prefer a ramp step over a magic hex
/// for hover states, borders, and fills."
pub mod teal {
    use super::{Color32, hex};

    pub const T50: Color32 = hex(0xeef7f8);
    pub const T100: Color32 = hex(0xd3e9ec);
    /// Also `dark.primary`.
    pub const T200: Color32 = hex(0xabd4d8);
    pub const T300: Color32 = hex(0x7fbcc2);
    pub const T400: Color32 = hex(0x4c9aa3);
    pub const T500: Color32 = hex(0x1c8791);
    /// Also `light.primary`.
    pub const T600: Color32 = hex(0x0f717a);
    pub const T700: Color32 = hex(0x0d5f67);
    pub const T800: Color32 = hex(0x0e4a52);
    /// Also `dark.card` — the Hub's dark surface.
    pub const T900: Color32 = hex(0x0a222e);
    /// Also `dark.background` — the Hub's dark canvas.
    pub const T950: Color32 = hex(0x041620);
}

/// Accent hues — mode-independent.
pub mod accent {
    use super::{Color32, hex};

    /// Bright secondary / CTAs.
    pub const AQUA: Color32 = hex(0x00a9a7);
    pub const AQUA_LIGHT: Color32 = hex(0x72ddd4);
    /// Mint highlight / focus ring.
    pub const MINT: Color32 = hex(0x58ebbf);
    /// Dark accent / border.
    pub const TEAL_ACCENT: Color32 = hex(0x2c676d);
    /// Mid teal (dark charts).
    pub const TEAL_MID: Color32 = hex(0x409b9b);
}

/// Corner radii, in points.
///
/// `tokens.json` gives `--radius: 0.625rem` (10px) with the rest derived by
/// `calc()`; those are resolved here.
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

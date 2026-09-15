//! Companion Intelligence design system for the applet's egui windows.
//!
//! This is the single source of truth for the applet's look: colour tokens,
//! fonts, global `Visuals`, and a small library of reusable widgets (themed
//! text fields, cards, status dots/badges, capture-lane rows, toggles, buttons,
//! sidebar nav items, file-type chips). Both the Settings and Upload windows
//! style themselves exclusively through this module so colours and widget code
//! live in exactly one place.
//!
//! The palette is the canonical Companion Intelligence one, transcribed from
//! `@companionintelligence/tokens` into [`crate::tokens`]. This module maps
//! those upstream token names onto the applet's semantic roles; it should not
//! introduce colours of its own. egui has no CSS-gradient primitive for
//! widgets, so CI-Common's gradients are approximated with solid accent fills.
//!
//! **Both light and dark are real, live themes**, not a dark palette with a
//! light palette bolted on for completeness. [`apply_visuals`] sets
//! `ThemePreference::System` and registers a full `Visuals` for each; the
//! applet follows the OS appearance and updates live if it changes while a
//! window is open (see [`sync`]). Every semantic colour below (`TEXT()`,
//! `CARD()`, …) is a zero-argument function rather than a constant for exactly
//! this reason — a `const` is fixed at compile time and cannot switch with the
//! theme, so these read from a per-frame [`Palette`] instead. Call sites are
//! unaffected: `TEXT` just became `TEXT()`.

use std::cell::Cell;

use anyhow::{Context as _, Result};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontFamily, FontId, Response, Sense, Stroke, StrokeKind,
    TextureHandle, Ui, Vec2,
};

use crate::tokens::{self, accent, dark, teal};

// ── Palette ─────────────────────────────────────────────────────────────────
//
// Semantic role  ←  CI-Common token. Every field below is a re-export, never a
// fresh hex; add new roles by mapping another token, not by inventing a colour.
// The two derived greens are the one deliberate exception — see their comment.

/// One theme's full set of resolved semantic colours.
#[derive(Debug, Clone, Copy)]
struct Palette {
    /// App canvas — the deepest surface. Windows are fully covered by panels,
    /// so this is rarely drawn directly.
    bg: Color32,
    /// Window / panel surface.
    panel: Color32,
    /// Elevated chrome (titlebars, footers). Bluish slate in dark mode so it
    /// sits with `sidebar-primary` / the primary button, not the green T800
    /// ramp step that used to make the shell look off-brand.
    panel_hi: Color32,
    /// Nav sidebar surface — CI-Common `sidebar`.
    sidebar: Color32,
    /// CI-Common `sidebar-foreground` — primary ink on the rail (dark in light
    /// mode, near-white in dark). Matches Companion Planning / shadcn sidebar.
    sidebar_fg: Color32,
    /// CI-Common `sidebar-accent` — selected / hover row fill.
    sidebar_accent: Color32,
    /// CI-Common `sidebar-accent-foreground`.
    sidebar_accent_fg: Color32,
    /// Recessed surface behind cards / inputs. Deliberately *darker* than
    /// `panel` so text fields read as wells rather than raised chips.
    input: Color32,
    /// Card fill (sits on top of `panel`).
    card: Color32,
    /// Hairline borders.
    border: Color32,
    border_hi: Color32,
    /// Primary accent. CI-Common's `primary`.
    teal: Color32,
    teal_dim: Color32,
    blue: Color32,
    purple: Color32,
    /// Focus ring / live highlight. CI-Common's `ring`. Mode-independent per
    /// CI-Common's own accent-hue docs.
    mint: Color32,
    // Status.
    green: Color32,
    amber: Color32,
    red: Color32,
    // Text.
    /// Primary copy.
    text: Color32,
    muted: Color32,
    faint: Color32,
    /// Text drawn on top of a teal fill (primary buttons) — the token pairing
    /// flips direction per mode (dark text on the pale dark-mode teal, light
    /// text on the deep light-mode teal), so this must come from the palette
    /// and never be assumed to be one fixed colour.
    on_teal: Color32,
    /// Capture-lane accent colours, drawn from the token chart ramp so the
    /// four lanes stay distinguishable and on-brand. `CHART_3` was the one
    /// ramp step still unused when the device-activity lane arrived, so it
    /// took it rather than reshuffling the three colours users already know.
    lane_shot: Color32,
    lane_activity: Color32,
    lane_file: Color32,
    lane_knowledge: Color32,
}

/// `SUCCESS` darkened for use as a fill/background rather than small-dot text.
/// The raw token (`#4ade80` dark, `#16a34a` light) is tuned for a status dot or
/// text colour; used as a badge *background* it read as neon against the teal
/// surfaces. Not a CI-Common token — a local adjustment, kept explicit here
/// rather than silently baked into `tokens.rs`.
const GREEN_DARK: Color32 = Color32::from_rgb(0x35, 0xa0, 0x5c);
const GREEN_LIGHT: Color32 = Color32::from_rgb(0x13, 0x8b, 0x3f);

impl Palette {
    const fn dark() -> Self {
        Self {
            bg: dark::BACKGROUND,
            panel: dark::CARD,
            panel_hi: dark::SIDEBAR_ACCENT,
            sidebar: dark::SIDEBAR,
            sidebar_fg: dark::SIDEBAR_FOREGROUND,
            sidebar_accent: dark::SIDEBAR_ACCENT,
            sidebar_accent_fg: dark::SIDEBAR_ACCENT_FOREGROUND,
            input: dark::BACKGROUND,
            card: teal::T900,
            border: dark::BORDER,
            border_hi: accent::TEAL_MID,
            teal: dark::PRIMARY,
            teal_dim: accent::TEAL_MID,
            blue: accent::AQUA_LIGHT,
            purple: accent::AQUA,
            mint: accent::MINT,
            green: GREEN_DARK,
            amber: dark::WARNING,
            red: dark::DESTRUCTIVE,
            text: dark::SECONDARY_FOREGROUND,
            muted: dark::MUTED_FOREGROUND,
            faint: dark::FOREGROUND,
            on_teal: dark::PRIMARY_FOREGROUND,
            lane_shot: dark::CHART_4,
            lane_activity: dark::CHART_2,
            lane_file: dark::CHART_1,
            lane_knowledge: dark::CHART_3,
        }
    }

    const fn light() -> Self {
        Self {
            bg: tokens::light::BACKGROUND,
            panel: tokens::light::CARD,
            panel_hi: tokens::light::SIDEBAR_ACCENT,
            sidebar: tokens::light::SIDEBAR,
            sidebar_fg: tokens::light::SIDEBAR_FOREGROUND,
            sidebar_accent: tokens::light::SIDEBAR_ACCENT,
            sidebar_accent_fg: tokens::light::SIDEBAR_ACCENT_FOREGROUND,
            input: tokens::light::BACKGROUND,
            card: tokens::light::CARD,
            border: tokens::light::BORDER,
            border_hi: accent::TEAL_MID,
            teal: tokens::light::PRIMARY,
            teal_dim: accent::TEAL_MID,
            blue: accent::AQUA_LIGHT,
            purple: accent::AQUA,
            mint: accent::MINT,
            green: GREEN_LIGHT,
            amber: tokens::light::WARNING,
            red: tokens::light::DESTRUCTIVE,
            text: tokens::light::FOREGROUND,
            muted: tokens::light::MUTED_FOREGROUND,
            faint: tokens::light::MUTED_FOREGROUND,
            on_teal: tokens::light::PRIMARY_FOREGROUND,
            lane_shot: tokens::light::CHART_4,
            lane_activity: tokens::light::CHART_2,
            lane_file: tokens::light::CHART_1,
            lane_knowledge: tokens::light::CHART_3,
        }
    }
}

thread_local! {
    // Each window is its own OS process/thread (see the crate's process model
    // doc in main.rs), so a thread-local is exactly the right scope: no window
    // ever needs to see another window's palette, and there is no cross-thread
    // synchronisation to get wrong.
    static PALETTE: Cell<Palette> = const { Cell::new(Palette::dark()) };
}

/// Re-resolve the active palette for this frame from the OS/egui theme.
///
/// Call once per `update()`, before drawing anything — every colour accessor
/// in this module (`TEXT()`, `CARD()`, …) reads whatever this last set, so a
/// missed or late call draws a frame with the wrong theme. `apply_visuals`
/// calls this once at window creation; each window's `update()` calls it again
/// so a live OS theme change (toggled in System Settings while the window is
/// open) is picked up without a restart, matching how egui itself re-resolves
/// `Visuals` every frame.
pub fn sync(ctx: &egui::Context) {
    let p = match ctx.theme() {
        egui::Theme::Dark => Palette::dark(),
        egui::Theme::Light => Palette::light(),
    };
    PALETTE.with(|cell| cell.set(p));
}

#[allow(non_snake_case)]
pub fn BG() -> Color32 {
    PALETTE.with(|p| p.get().bg)
}
#[allow(non_snake_case)]
pub fn PANEL() -> Color32 {
    PALETTE.with(|p| p.get().panel)
}
#[allow(non_snake_case)]
pub fn PANEL_HI() -> Color32 {
    PALETTE.with(|p| p.get().panel_hi)
}
#[allow(non_snake_case)]
pub fn SIDEBAR() -> Color32 {
    PALETTE.with(|p| p.get().sidebar)
}
#[allow(non_snake_case)]
pub fn SIDEBAR_FG() -> Color32 {
    PALETTE.with(|p| p.get().sidebar_fg)
}
#[allow(non_snake_case)]
pub fn SIDEBAR_ACCENT() -> Color32 {
    PALETTE.with(|p| p.get().sidebar_accent)
}
#[allow(non_snake_case)]
pub fn SIDEBAR_ACCENT_FG() -> Color32 {
    PALETTE.with(|p| p.get().sidebar_accent_fg)
}
#[allow(non_snake_case)]
pub fn INPUT() -> Color32 {
    PALETTE.with(|p| p.get().input)
}
#[allow(non_snake_case)]
pub fn CARD() -> Color32 {
    PALETTE.with(|p| p.get().card)
}
#[allow(non_snake_case)]
pub fn BORDER() -> Color32 {
    PALETTE.with(|p| p.get().border)
}
#[allow(non_snake_case)]
pub fn BORDER_HI() -> Color32 {
    PALETTE.with(|p| p.get().border_hi)
}
#[allow(non_snake_case)]
pub fn TEAL() -> Color32 {
    PALETTE.with(|p| p.get().teal)
}
#[allow(non_snake_case)]
pub fn TEAL_DIM() -> Color32 {
    PALETTE.with(|p| p.get().teal_dim)
}
#[allow(non_snake_case)]
pub fn BLUE() -> Color32 {
    PALETTE.with(|p| p.get().blue)
}
#[allow(non_snake_case)]
pub fn PURPLE() -> Color32 {
    PALETTE.with(|p| p.get().purple)
}
#[allow(non_snake_case)]
pub fn MINT() -> Color32 {
    PALETTE.with(|p| p.get().mint)
}
#[allow(non_snake_case)]
pub fn GREEN() -> Color32 {
    PALETTE.with(|p| p.get().green)
}
#[allow(non_snake_case)]
pub fn AMBER() -> Color32 {
    PALETTE.with(|p| p.get().amber)
}
#[allow(non_snake_case)]
pub fn RED() -> Color32 {
    PALETTE.with(|p| p.get().red)
}
#[allow(non_snake_case)]
pub fn TEXT() -> Color32 {
    PALETTE.with(|p| p.get().text)
}
#[allow(non_snake_case)]
pub fn MUTED() -> Color32 {
    PALETTE.with(|p| p.get().muted)
}
#[allow(non_snake_case)]
pub fn FAINT() -> Color32 {
    PALETTE.with(|p| p.get().faint)
}
#[allow(non_snake_case)]
pub fn ON_TEAL() -> Color32 {
    PALETTE.with(|p| p.get().on_teal)
}
#[allow(non_snake_case)]
pub fn LANE_SHOT() -> Color32 {
    PALETTE.with(|p| p.get().lane_shot)
}
#[allow(non_snake_case)]
pub fn LANE_ACTIVITY() -> Color32 {
    PALETTE.with(|p| p.get().lane_activity)
}
#[allow(non_snake_case)]
pub fn LANE_FILE() -> Color32 {
    PALETTE.with(|p| p.get().lane_file)
}
/// Accent for the device-activity lane (`crate::knowledge`).
#[allow(non_snake_case)]
pub fn LANE_KNOWLEDGE() -> Color32 {
    PALETTE.with(|p| p.get().lane_knowledge)
}

/// Return `color` at the given 0–255 alpha so it blends with what's behind it.
pub fn tint(color: Color32, alpha: u8) -> Color32 {
    let [r, g, b, _] = color.to_array();
    Color32::from_rgba_unmultiplied(r, g, b, alpha)
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let [ar, ag, ab, aa] = a.to_array();
    let [br, bg, bb, ba] = b.to_array();
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(l(ar, br), l(ag, bg), l(ab, bb), l(aa, ba))
}

// ── Fonts ─────────────────────────────────────────────────────────────────--

// Manrope is the Companion Intelligence sans (Montserrat was retired at tokens v0.3.0). egui
// loads one fixed weight per family, so these are the static instances the brand bundle
// ships under fonts/static/ (vendor/brand, refreshed by scripts/vendor-brand.sh).
const MANROPE: &[u8] = include_bytes!("../vendor/brand/fonts/static/manrope-400.ttf");
const MANROPE_LIGHT: &[u8] = include_bytes!("../vendor/brand/fonts/static/manrope-300.ttf");
const MANROPE_SEMIBOLD: &[u8] = include_bytes!("../vendor/brand/fonts/static/manrope-600.ttf");
const SOURCE_CODE_PRO: &[u8] = include_bytes!("../assets/fonts/SourceCodePro-Regular.ttf");

/// Named font families for the non-default Manrope weights.
pub fn family_light() -> FontFamily {
    FontFamily::Name("Manrope-Light".into())
}
pub fn family_semibold() -> FontFamily {
    FontFamily::Name("Manrope-SemiBold".into())
}

/// `FontId` helpers used throughout the windows.
pub fn light(size: f32) -> FontId {
    FontId::new(size, family_light())
}
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, family_semibold())
}
pub fn body(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// Register Manrope (proportional) and Source Code Pro (monospace). Call
/// once from each window's eframe creation closure.
pub fn install_fonts(ctx: &egui::Context) {
    use egui::{FontData, FontDefinitions};
    use std::sync::Arc;

    let mut fonts = FontDefinitions::default();
    let mut add = |name: &str, bytes: &'static [u8]| {
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    };
    add("Manrope", MANROPE);
    add("Manrope-Light", MANROPE_LIGHT);
    add("Manrope-SemiBold", MANROPE_SEMIBOLD);
    add("SourceCodePro", SOURCE_CODE_PRO);

    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "Manrope".to_owned());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, "SourceCodePro".to_owned());

    // Weight variants get their own families, falling back to regular Manrope.
    fonts.families.insert(
        FontFamily::Name("Manrope-Light".into()),
        vec!["Manrope-Light".to_owned(), "Manrope".to_owned()],
    );
    fonts.families.insert(
        FontFamily::Name("Manrope-SemiBold".into()),
        vec!["Manrope-SemiBold".to_owned(), "Manrope".to_owned()],
    );

    ctx.set_fonts(fonts);
}

/// Build a full `Visuals` from a resolved `Palette`.
///
/// `base_dark` seeds sensible defaults for the fields this module doesn't
/// override (scrollbar visuals, `code_bg_color`, etc.) — `Visuals::dark()` or
/// `::light()` — before the semantic colours above are painted on top.
fn build_visuals(p: Palette, base_dark: bool) -> egui::Visuals {
    use egui::style::WidgetVisuals;

    let mut v = if base_dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    v.panel_fill = p.panel;
    v.window_fill = p.panel;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(14);
    v.extreme_bg_color = p.input;
    v.faint_bg_color = p.card;
    v.override_text_color = Some(p.text);
    v.hyperlink_color = p.teal;
    v.selection.bg_fill = tint(p.teal, 60);
    v.selection.stroke = Stroke::new(1.0, p.teal);

    let widget = |bg: Color32, weak: Color32, stroke: Color32, fg: Color32| WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: weak,
        bg_stroke: Stroke::new(1.0, stroke),
        fg_stroke: Stroke::new(1.0, fg),
        corner_radius: CornerRadius::same(10),
        expansion: 0.0,
    };
    v.widgets.noninteractive = widget(p.panel, p.panel, p.border, p.muted);
    v.widgets.inactive = widget(p.input, p.input, p.border, p.text);
    v.widgets.hovered = widget(p.input, p.input, p.border_hi, p.text);
    v.widgets.active = widget(p.input, p.input, p.teal, p.text);
    v.widgets.open = widget(p.input, p.input, p.border_hi, p.text);
    v
}

/// Apply the theme (visuals, spacing, text styles) for both light and dark,
/// and follow the OS appearance.
pub fn apply_visuals(ctx: &egui::Context) {
    use egui::TextStyle;

    let mut base_style = (*ctx.style()).clone();
    base_style.spacing.item_spacing = Vec2::new(9.0, 9.0);
    base_style.spacing.button_padding = Vec2::new(13.0, 7.0);
    base_style.spacing.interact_size.y = 30.0;
    base_style.spacing.window_margin = egui::Margin::same(0);
    base_style.text_styles = [
        (TextStyle::Heading, light(tokens::text::HEADING)),
        (TextStyle::Body, body(tokens::text::BODY)),
        (TextStyle::Button, body(tokens::text::BUTTON)),
        (TextStyle::Small, body(tokens::text::SMALL)),
        (TextStyle::Monospace, mono(tokens::text::MONO)),
    ]
    .into();

    let mut dark_style = base_style.clone();
    dark_style.visuals = build_visuals(Palette::dark(), true);
    let mut light_style = base_style;
    light_style.visuals = build_visuals(Palette::light(), false);

    // `Context::set_style` writes only the *currently active* theme's slot, so
    // registering just one style (as an earlier version of this code did)
    // silently discarded everything the moment the host was in the other mode.
    // Explicit per-theme registration, plus `System` rather than a pinned
    // preference, is what makes following the OS appearance actually correct.
    ctx.set_theme(egui::ThemePreference::System);
    ctx.set_style_of(egui::Theme::Dark, dark_style);
    ctx.set_style_of(egui::Theme::Light, light_style);

    // Resolve this module's own Palette to match, so the very first frame's
    // procedural drawing (capture_flow, status dots, …) is correct before the
    // window's own update() loop starts calling `sync` each frame.
    sync(ctx);
}

/// Centre a window on screen.
///
/// Call from `update` behind a once-flag: the viewport reports no `outer_rect`
/// or `monitor_size` until the first pass, so this cannot run from the eframe
/// creation closure (where `NativeOptions::centered` runs instead).
///
/// Caveat: egui derives the position from the current monitor's *size* but
/// does not offset by that monitor's origin in the virtual desktop, so on a
/// secondary display the window lands centred-but-offset. Good enough for a
/// setup wizard; exact multi-head placement needs the `window_builder` hook.
pub fn center_once(ctx: &egui::Context, done: &mut bool) {
    if !*done {
        *done = true;
        if let Some(cmd) = egui::ViewportCommand::center_on_screen(ctx) {
            ctx.send_viewport_cmd(cmd);
        }
    }
}

/// A 0.4–1.0 breathing factor (~2s period) for live status dots. Requests a
/// repaint so the animation keeps running.
pub fn pulse(ctx: &egui::Context) -> f32 {
    ctx.request_repaint();
    let t = ctx.input(|i| i.time);
    0.4 + 0.6 * (0.5 + 0.5 * (t * std::f64::consts::PI).sin()) as f32
}

// ── Explainer animation ─────────────────────────────────────────────────────

/// An animated "what this app does" diagram: this computer on the left, three
/// capture lanes flowing right into your Companion Memory.
///
/// Drawn procedurally with [`egui::Painter`] rather than shipped as a video or
/// GIF. That is a deliberate trade: it adds no dependency (the crate graph has
/// no video codec, no Lottie, and `image` is compiled with only `png`), it
/// recolours for free from [`crate::tokens`], and it stays crisp at any DPI or
/// zoom — which matters because this same window also has to satisfy "larger,
/// easier-to-read text". `usvg` has no SMIL support, so animated SVG was never
/// an option either.
///
/// Returns the [`egui::Rect`] consumed. Requests repaints while visible.
pub fn capture_flow(ui: &mut Ui, height: f32) -> egui::Rect {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return rect;
    }

    // ~9s loop: long enough to read, short enough to see it repeat.
    ui.ctx().request_repaint();
    let t = ui.ctx().input(|i| i.time) as f32;

    let p = ui.painter_at(rect);
    let mid_y = rect.center().y;
    p.rect_filled(rect, CornerRadius::same(tokens::radius::XL), BG());

    // ── endpoints ───────────────────────────────────────────────────────────
    let device_w = (width * 0.20).clamp(74.0, 128.0);
    let device_h = device_w * 0.66;
    let device = egui::Rect::from_center_size(
        egui::pos2(rect.left() + device_w * 0.62 + 8.0, mid_y),
        Vec2::new(device_w, device_h),
    );
    let core_r = (width * 0.075).clamp(28.0, 46.0);
    let core_c = egui::pos2(rect.right() - core_r - 12.0, mid_y);

    // ── the three lanes ─────────────────────────────────────────────────────
    // Computed before the endpoints are drawn so their departure/arrival
    // pulses (below) can react to how close a packet is to each end.
    // Same keys as the lane cards the onboarding step draws underneath: the
    // diagram and the list must say the same word in every locale.
    let lanes: [(Color32, String); 3] = [
        (
            LANE_SHOT(),
            rust_i18n::t!("onboarding.welcome.lane.screens.title").to_string(),
        ),
        (
            LANE_ACTIVITY(),
            rust_i18n::t!("onboarding.welcome.lane.activity.title").to_string(),
        ),
        (
            LANE_FILE(),
            rust_i18n::t!("onboarding.welcome.lane.files.title").to_string(),
        ),
    ];
    let x0 = device.right() + 14.0;
    let x1 = core_c.x - core_r - 10.0;
    let span = (x1 - x0).max(1.0);
    let lane_gap = (height * 0.19).clamp(16.0, 30.0);

    // Closeness (0..1) of the nearest packet to each endpoint, across every
    // lane — drives the departure ripple on the device and the arrival flash
    // on Memory so both actually track a real packet crossing, rather than
    // pulsing on a generic beat that happens to be close.
    let mut depart_pulse = 0.0f32;
    let mut arrive_pulse = 0.0f32;

    for (i, (color, label)) in lanes.iter().enumerate() {
        let y = mid_y + (i as f32 - 1.0) * lane_gap;

        // Track.
        p.line_segment(
            [egui::pos2(x0, y), egui::pos2(x1, y)],
            Stroke::new(1.0, tint(*color, 40)),
        );
        p.text(
            egui::pos2(x0 + 2.0, y - 6.0),
            Align2::LEFT_BOTTOM,
            label,
            body(tokens::text::MIN),
            tint(*color, 210),
        );

        // Two packets per lane, staggered per-lane so it reads as continuous
        // flow rather than a marching row.
        for k in 0..2 {
            let phase = (t * 0.34 + i as f32 * 0.21 + k as f32 * 0.5).fract();
            let x = x0 + span * phase;
            // Fade in and out at the ends so packets don't pop.
            let edge = (phase / 0.12).min((1.0 - phase) / 0.12).clamp(0.0, 1.0);
            let alpha = (edge * 235.0) as u8;
            // A faint bob gives the stream a little life instead of a dead-flat
            // line — small enough it never reads as "off the track".
            let bob = (t * 2.3 + i as f32 + k as f32 * 1.7).sin() * 1.2;
            p.circle_filled(egui::pos2(x, y + bob), 3.4, tint(*color, alpha));
            // Short comet tail.
            p.line_segment(
                [egui::pos2(x - 13.0, y + bob), egui::pos2(x, y + bob)],
                Stroke::new(2.0, tint(*color, alpha / 3)),
            );

            depart_pulse = depart_pulse.max((1.0 - phase / 0.1).clamp(0.0, 1.0));
            arrive_pulse = arrive_pulse.max((1.0 - (1.0 - phase) / 0.14).clamp(0.0, 1.0));
        }
    }

    // This computer: a simple laptop silhouette. A soft ring blooms from it
    // each time a packet actually departs, so the flow visibly starts here
    // rather than materialising mid-air.
    if depart_pulse > 0.0 {
        p.rect_stroke(
            device.expand(6.0 * depart_pulse),
            CornerRadius::same(6),
            Stroke::new(1.5, tint(TEAL(), (depart_pulse * 130.0) as u8)),
            StrokeKind::Outside,
        );
    }
    p.rect_filled(device, CornerRadius::same(6), CARD());
    p.rect_stroke(
        device,
        CornerRadius::same(6),
        Stroke::new(1.5, BORDER_HI()),
        StrokeKind::Inside,
    );
    p.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(device.left() - 6.0, device.bottom() + 2.0),
            egui::pos2(device.right() + 6.0, device.bottom() + 5.0),
        ),
        CornerRadius::same(2),
        BORDER_HI(),
    );
    // A small blinking indicator on the "screen" — reads as "actively
    // recording" without needing a literal camera icon.
    let rec_alpha = (0.35 + 0.65 * (0.5 + 0.5 * (t * 1.6).sin())) * 255.0;
    p.circle_filled(
        egui::pos2(device.left() + 10.0, device.top() + 10.0),
        2.6,
        tint(LANE_SHOT(), rec_alpha as u8),
    );
    p.text(
        egui::pos2(device.center().x, device.bottom() + 18.0),
        Align2::CENTER_TOP,
        rust_i18n::t!("onboarding.welcome.flow.this_computer").to_string(),
        body(tokens::text::SMALL),
        MUTED(),
    );

    // Your Memory: concentric ring + dot, echoing the tray glyph so the two
    // read as the same product. An ambient breathing glow stays visible at
    // all times; the arrival flash punches through it on top of that.
    let breathe = 0.85 + 0.15 * (t * 1.1).sin();
    p.circle_filled(core_c, core_r * (1.05 + 0.12 * breathe), tint(TEAL(), 14));
    p.circle_filled(core_c, core_r * breathe, tint(TEAL(), 26));
    p.circle_stroke(core_c, core_r * 0.72, Stroke::new(2.5, TEAL()));
    p.circle_filled(core_c, core_r * 0.26, TEAL());
    if arrive_pulse > 0.0 {
        p.circle_stroke(
            core_c,
            core_r * (0.72 + 0.5 * arrive_pulse),
            Stroke::new(1.5, tint(MINT(), (arrive_pulse * 150.0) as u8)),
        );
    }
    p.text(
        egui::pos2(core_c.x, core_c.y + core_r + 10.0),
        Align2::CENTER_TOP,
        rust_i18n::t!("onboarding.welcome.flow.your_memory").to_string(),
        body(tokens::text::SMALL),
        MUTED(),
    );

    rect
}

// ── Logo ────────────────────────────────────────────────────────────────────

const CI_MARK_WHITE: &[u8] = include_bytes!("../assets/ci-mark-white.svg");

/// Rasterise the CI mark (white) into a texture. Tint at draw time via
/// [`logo`]. Load once (e.g. in the eframe creation closure) and reuse.
pub fn load_logo(ctx: &egui::Context) -> TextureHandle {
    let image = rasterize_svg(CI_MARK_WHITE, 128)
        .unwrap_or_else(|_| egui::ColorImage::from_rgba_unmultiplied([1, 1], &[0, 0, 0, 0]));
    ctx.load_texture("ci-mark", image, egui::TextureOptions::LINEAR)
}

fn rasterize_svg(data: &[u8], size: u32) -> Result<egui::ColorImage> {
    use resvg::tiny_skia::{Pixmap, Transform};
    use usvg::{Options, Tree};

    let tree = Tree::from_data(data, &Options::default()).context("parsing svg")?;
    let mut pixmap = Pixmap::new(size, size).context("allocating pixmap")?;
    let svg_size = tree.size();
    let scale = (size as f32 / svg_size.width()).min(size as f32 / svg_size.height());
    let tx = (size as f32 - svg_size.width() * scale) / 2.0;
    let ty = (size as f32 - svg_size.height() * scale) / 2.0;
    resvg::render(
        &tree,
        Transform::from_translate(tx, ty).post_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    // tiny-skia stores premultiplied RGBA, which is exactly what egui wants.
    Ok(egui::ColorImage::from_rgba_premultiplied(
        [size as usize, size as usize],
        pixmap.data(),
    ))
}

/// Draw the CI mark at `size`, tinted to `color`.
pub fn logo(ui: &mut Ui, tex: &TextureHandle, size: f32, color: Color32) {
    let sized = egui::load::SizedTexture::new(tex.id(), Vec2::splat(size));
    ui.add(egui::Image::new(sized).tint(color));
}

// ── Text helpers ──────────────────────────────────────────────────────────--

/// Big light heading (page title).
pub fn heading(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).font(light(22.0)).color(TEXT()));
}

/// Muted descriptive subtitle under a heading.
pub fn subtitle(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).font(body(14.0)).color(MUTED()));
}

/// Uppercase, slightly bold field/section label.
pub fn field_label(ui: &mut Ui, text: &str) {
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .font(semibold(12.0))
            .color(MUTED()),
    );
}

/// Small faint caption / helper text.
pub fn caption(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).font(body(13.0)).color(FAINT()));
}

/// Monospace value (config paths, IDs, tags).
pub fn mono_label(ui: &mut Ui, text: &str, color: Color32) {
    ui.label(egui::RichText::new(text).font(mono(13.0)).color(color));
}

// ── Inputs ────────────────────────────────────────────────────────────────--

/// Options for [`text_field`].
#[derive(Default, Clone, Copy)]
pub struct FieldOpts {
    pub password: bool,
    pub monospace: bool,
}

impl FieldOpts {
    pub fn password() -> Self {
        Self {
            password: true,
            monospace: true,
        }
    }
    pub fn monospace() -> Self {
        Self {
            monospace: true,
            ..Self::default()
        }
    }
}

/// A consistently styled single-line text field. Returns whether it changed.
pub fn text_field(ui: &mut Ui, value: &mut String, hint: &str, opts: FieldOpts) -> bool {
    let font = if opts.monospace {
        mono(15.0)
    } else {
        body(15.0)
    };
    let edit = egui::TextEdit::singleline(value)
        .hint_text(egui::RichText::new(hint).color(FAINT()))
        .password(opts.password)
        .font(egui::FontSelection::FontId(font))
        .margin(egui::Margin::symmetric(12, 9))
        .desired_width(f32::INFINITY);
    ui.add(edit).changed()
}

// ── Cards ─────────────────────────────────────────────────────────────────--

/// A rounded card surface (fill = CARD(), faint border).
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD())
        .stroke(Stroke::new(1.0, BORDER()))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(egui::Margin::same(15))
        .show(ui, add)
        .inner
}

// ── Status indicators ─────────────────────────────────────────────────────--

/// A small filled dot with a soft glow; optionally breathing.
pub fn status_dot(ui: &mut Ui, color: Color32, animate: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(11.0), Sense::hover());
    let a = if animate { pulse(ui.ctx()) } else { 1.0 };
    let p = ui.painter();
    p.circle_filled(rect.center(), 5.5, tint(color, (70.0 * a) as u8));
    p.circle_filled(rect.center(), 3.5, color.gamma_multiply(a.max(0.6)));
}

/// A status badge: tinted background, leading dot, label. Same rounded-rect
/// profile and height as [`ghost_button`] / the Test connection control (32px).
pub fn status_badge(ui: &mut Ui, text: &str, color: Color32) -> Response {
    egui::Frame::new()
        .fill(tint(color, 28))
        .corner_radius(CornerRadius::same(tokens::radius::LG))
        .inner_margin(egui::Margin::symmetric(11, 0))
        .show(ui, |ui| {
            ui.set_height(32.0);
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.set_height(32.0);
                ui.spacing_mut().item_spacing.x = 7.0;
                let (r, _) = ui.allocate_exact_size(Vec2::splat(6.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 3.0, color);
                ui.label(egui::RichText::new(text).font(semibold(13.0)).color(color));
            });
        })
        .response
}

// ── Capture-lane row ──────────────────────────────────────────────────────--

/// A capture-lane card: glowing accent dot, title + subtitle, trailing mono tag.
pub fn lane_row(ui: &mut Ui, color: Color32, title: &str, subtitle: &str, tag: &str) {
    egui::Frame::new()
        .fill(tint(Color32::BLACK, 46))
        .stroke(Stroke::new(1.0, BORDER()))
        .corner_radius(CornerRadius::same(11))
        .inner_margin(egui::Margin::symmetric(15, 13))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 13.0;
                let (r, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 4.5, color);
                ui.painter().circle_filled(r.center(), 7.5, tint(color, 45));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(egui::RichText::new(title).font(body(15.0)).color(TEXT()));
                    ui.label(
                        egui::RichText::new(subtitle)
                            .font(body(13.0))
                            .color(MUTED()),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    mono_label(ui, tag, MUTED());
                });
            });
        });
}

// ── Toggle switch ─────────────────────────────────────────────────────────--

/// iOS-style toggle. egui has no built-in switch; this is adapted from the
/// egui demo `toggle_ui`. Track turns teal when on.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
    let size = Vec2::new(42.0, 24.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let how = ui.ctx().animate_bool(resp.id, *on);
    let radius = rect.height() / 2.0;
    let track = mix(tint(Color32::WHITE, 33), TEAL(), how);
    let painter = ui.painter();
    painter.rect_filled(rect, radius, track);
    let cx = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how);
    painter.circle_filled(
        egui::pos2(cx, rect.center().y),
        radius - 3.0,
        Color32::WHITE,
    );
    resp
}

/// A toggle row: switch on the left, title + subtitle on the right, inside a card.
pub fn toggle_row(ui: &mut Ui, on: &mut bool, title: &str, subtitle: &str) -> bool {
    let mut changed = false;
    egui::Frame::new()
        .fill(tint(Color32::BLACK, 46))
        .stroke(Stroke::new(1.0, BORDER()))
        .corner_radius(CornerRadius::same(11))
        .inner_margin(egui::Margin::symmetric(16, 14))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 13.0;
                changed = toggle(ui, on).changed();
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(egui::RichText::new(title).font(body(15.0)).color(TEXT()));
                    ui.label(
                        egui::RichText::new(subtitle)
                            .font(body(13.0))
                            .color(MUTED()),
                    );
                });
            });
        });
    changed
}

// ── Buttons ───────────────────────────────────────────────────────────────--
//
// `Button::fill`/`stroke` set a FIXED colour, which bypasses egui's automatic
// hover/press styling entirely — a plain `.fill(TEAL())` button is visually
// inert under the cursor. Each of these draws a light overlay on top of the
// already-painted widget, animated via `animate_bool` so it eases rather than
// snapping. egui has no blur/shadow primitive, so "lift" here means a
// brightness wash plus a soft outward ring, not a literal drop shadow.

/// How strongly hovered/pressed `resp` currently is, eased (0 = rest, up to
/// ~1.15 while the mouse button is down, for a small extra "pressed" kick).
fn press_amount(ui: &Ui, resp: &Response) -> f32 {
    let hovered = ui
        .ctx()
        .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let pressed = ui.ctx().animate_bool_with_time(
        resp.id.with("press"),
        resp.is_pointer_button_down_on(),
        0.08,
    );
    hovered * (1.0 + 0.15 * pressed)
}

/// Solid teal primary button with dark text (approximates the mockup gradient).
pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    let btn = egui::Button::new(
        egui::RichText::new(text)
            .font(semibold(15.0))
            .color(ON_TEAL()),
    )
    .fill(TEAL())
    .corner_radius(CornerRadius::same(10))
    .min_size(Vec2::new(0.0, 32.0));
    let resp = ui.add(btn);
    let hover = press_amount(ui, &resp);
    if hover > 0.0 {
        ui.painter().rect_filled(
            resp.rect,
            CornerRadius::same(10),
            tint(Color32::WHITE, (hover * 22.0) as u8),
        );
        ui.painter().rect_stroke(
            resp.rect.expand(1.5 * hover),
            CornerRadius::same(10),
            Stroke::new(1.0, tint(TEAL(), (hover * 100.0) as u8)),
            StrokeKind::Outside,
        );
    }
    resp
}

/// Outlined "ghost" button.
pub fn ghost_button(ui: &mut Ui, text: &str) -> Response {
    let btn = egui::Button::new(egui::RichText::new(text).font(body(15.0)).color(TEXT()))
        .fill(Color32::TRANSPARENT)
        .stroke(Stroke::new(1.0, BORDER_HI()))
        .corner_radius(CornerRadius::same(10))
        .min_size(Vec2::new(0.0, 32.0));
    let resp = ui.add(btn);
    let hover = press_amount(ui, &resp);
    if hover > 0.0 {
        ui.painter().rect_filled(
            resp.rect,
            CornerRadius::same(10),
            tint(TEXT(), (hover * 16.0) as u8),
        );
        ui.painter().rect_stroke(
            resp.rect,
            CornerRadius::same(10),
            Stroke::new(1.0, tint(BORDER_HI(), (hover * 255.0).min(255.0) as u8)),
            StrokeKind::Inside,
        );
    }
    resp
}

// ── Segmented control ─────────────────────────────────────────────────────--

/// One segment of a segmented control. Returns true when clicked.
pub fn segment(ui: &mut Ui, text: &str, selected: bool) -> bool {
    let (fill, fg) = if selected {
        (TEAL(), ON_TEAL())
    } else {
        (Color32::TRANSPARENT, MUTED())
    };
    let font = if selected { semibold(14.0) } else { body(14.0) };
    let btn = egui::Button::new(egui::RichText::new(text).font(font).color(fg))
        .fill(fill)
        .corner_radius(CornerRadius::same(8))
        .min_size(Vec2::new(0.0, 30.0));
    let resp = ui.add(btn);
    if !selected {
        let hover = press_amount(ui, &resp);
        if hover > 0.0 {
            ui.painter().rect_filled(
                resp.rect,
                CornerRadius::same(8),
                tint(TEXT(), (hover * 18.0) as u8),
            );
        }
    }
    resp.clicked()
}

// ── Sidebar nav item ──────────────────────────────────────────────────────--

/// A sidebar navigation entry matching CI-Common / shadcn `SidebarMenuButton`
/// (same pattern Companion Planning uses): `sidebar-foreground` ink on the rail,
/// `sidebar-accent` + `sidebar-accent-foreground` when selected or hovered.
/// Returns true when clicked.
pub fn nav_item(ui: &mut Ui, accent: Color32, title: &str, subtitle: &str, selected: bool) -> bool {
    let desired = Vec2::new(ui.available_width(), 46.0);
    let (rect, resp) = ui.allocate_exact_size(desired, Sense::click());
    let painter = ui.painter();
    let active = selected || resp.hovered();

    if active {
        painter.rect_filled(
            rect,
            CornerRadius::same(tokens::radius::LG),
            SIDEBAR_ACCENT(),
        );
    }
    if selected {
        let bar = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 2.0, rect.center().y - 11.0),
            Vec2::new(3.0, 22.0),
        );
        painter.rect_filled(bar, CornerRadius::same(2), TEAL());
    }

    // Accent chip.
    let chip = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 26.0, rect.center().y),
        Vec2::splat(20.0),
    );
    painter.rect_filled(
        chip,
        CornerRadius::same(tokens::radius::SM),
        tint(accent, if selected { 70 } else { 40 }),
    );
    painter.circle_filled(chip.center(), 3.0, accent);

    let title_color = if active {
        SIDEBAR_ACCENT_FG()
    } else {
        SIDEBAR_FG()
    };
    painter.text(
        egui::pos2(rect.left() + 46.0, rect.center().y - 8.0),
        Align2::LEFT_CENTER,
        title,
        body(15.0),
        title_color,
    );
    painter.text(
        egui::pos2(rect.left() + 46.0, rect.center().y + 8.0),
        Align2::LEFT_CENTER,
        subtitle,
        body(13.0),
        MUTED(),
    );

    resp.clicked()
}

// ── File-type chip ────────────────────────────────────────────────────────--

/// Accent colour for a file extension, matching the mockup.
pub fn ext_color(ext: &str) -> Color32 {
    match ext.to_lowercase().as_str() {
        "pdf" => BLUE(),
        "md" | "markdown" | "txt" => TEAL_DIM(),
        "docx" | "doc" => PURPLE(),
        _ => MUTED(),
    }
}

/// A small rounded monospace chip showing a file extension (e.g. "PDF").
pub fn file_badge(ui: &mut Ui, ext: &str) {
    let color = ext_color(ext);
    let label = ext.to_uppercase();
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(8), tint(color, 40));
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        mono(12.0),
        color,
    );
}

//! Companion Intelligence design system for the applet's egui windows.
//!
//! This is the single source of truth for the applet's look: colour tokens,
//! fonts, global `Visuals`, and a small library of reusable widgets (themed
//! text fields, cards, status dots/badges, capture-lane rows, toggles, buttons,
//! sidebar nav items, file-type chips). Settings, Upload, and Onboarding style
//! themselves exclusively through this module so colours and widget code live
//! in exactly one place.
//!
//! Palette tokens track `@companionintelligence/tokens` (ci-common) dark mode
//! (`tokens.css` / `.dark`). egui has no CSS-gradient primitive for widgets, so
//! brand gradients are approximated with solid accent fills.

use anyhow::{Context as _, Result};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontFamily, FontId, Response, Sense, Stroke, StrokeKind,
    TextureHandle, Ui, Vec2,
};

// ── Colour tokens (ci-common `@companionintelligence/tokens` dark) ──────────

/// App canvas — `--background` (#041620).
#[allow(dead_code)]
pub const BG: Color32 = Color32::from_rgb(0x04, 0x16, 0x20);
/// Window / panel surface — `--sidebar` / deep ground.
pub const PANEL: Color32 = Color32::from_rgb(0x04, 0x16, 0x20);
/// Slightly lifted surface for titlebars / sidebar — `--card`.
pub const PANEL_HI: Color32 = Color32::from_rgb(0x0a, 0x22, 0x2e);
/// Recessed surface behind cards / inputs.
pub const INPUT: Color32 = Color32::from_rgb(0x07, 0x1c, 0x26);
/// Card fill — `--card` (#0a222e).
pub const CARD: Color32 = Color32::from_rgb(0x0a, 0x22, 0x2e);

/// Hairline borders — `--border` (#2c676d).
pub const BORDER: Color32 = Color32::from_rgb(0x2c, 0x67, 0x6d);
pub const BORDER_HI: Color32 = Color32::from_rgb(0x40, 0x9b, 0x9b);

// Accents — `--primary` / brand teal ramp.
pub const TEAL: Color32 = Color32::from_rgb(0xab, 0xd4, 0xd8);
pub const TEAL_DIM: Color32 = Color32::from_rgb(0x40, 0x9b, 0x9b);
pub const BLUE: Color32 = Color32::from_rgb(0x38, 0xbd, 0xf8);
pub const PURPLE: Color32 = Color32::from_rgb(0x7f, 0xbc, 0xc2);

// Status — enriched dark tokens.
pub const GREEN: Color32 = Color32::from_rgb(0x4a, 0xde, 0x80);
pub const AMBER: Color32 = Color32::from_rgb(0xfb, 0xbf, 0x24);
pub const RED: Color32 = Color32::from_rgb(0xef, 0x70, 0x70);

// Text — `--secondary-foreground` / `--muted-foreground`.
pub const TEXT: Color32 = Color32::from_rgb(0xf5, 0xf8, 0xfa);
pub const MUTED: Color32 = Color32::from_rgb(0x8f, 0x98, 0xa3);
pub const FAINT: Color32 = Color32::from_rgb(0x70, 0xa6, 0xaf);
/// Dark text on primary teal fills — `--primary-foreground`.
pub const ON_TEAL: Color32 = Color32::from_rgb(0x04, 0x16, 0x20);

/// Capture-lane accent colours.
pub const LANE_SHOT: Color32 = BLUE;
pub const LANE_ACTIVITY: Color32 = PURPLE;
pub const LANE_FILE: Color32 = TEAL_DIM;

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

const MONTSERRAT: &[u8] = include_bytes!("../assets/fonts/Montserrat-Regular.ttf");
const MONTSERRAT_LIGHT: &[u8] = include_bytes!("../assets/fonts/Montserrat-Light.ttf");
const MONTSERRAT_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/Montserrat-SemiBold.ttf");
const SOURCE_CODE_PRO: &[u8] = include_bytes!("../assets/fonts/SourceCodePro-Regular.ttf");

/// Named font families for the non-default Montserrat weights.
pub fn family_light() -> FontFamily {
    FontFamily::Name("Montserrat-Light".into())
}
pub fn family_semibold() -> FontFamily {
    FontFamily::Name("Montserrat-SemiBold".into())
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

/// Register Montserrat (proportional) and Source Code Pro (monospace). Call
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
    add("Montserrat", MONTSERRAT);
    add("Montserrat-Light", MONTSERRAT_LIGHT);
    add("Montserrat-SemiBold", MONTSERRAT_SEMIBOLD);
    add("SourceCodePro", SOURCE_CODE_PRO);

    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "Montserrat".to_owned());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, "SourceCodePro".to_owned());

    // Weight variants get their own families, falling back to regular Montserrat.
    fonts.families.insert(
        FontFamily::Name("Montserrat-Light".into()),
        vec!["Montserrat-Light".to_owned(), "Montserrat".to_owned()],
    );
    fonts.families.insert(
        FontFamily::Name("Montserrat-SemiBold".into()),
        vec!["Montserrat-SemiBold".to_owned(), "Montserrat".to_owned()],
    );

    ctx.set_fonts(fonts);
}

/// Apply the global dark theme (visuals, spacing, text styles).
pub fn apply_visuals(ctx: &egui::Context) {
    use egui::TextStyle;
    use egui::style::WidgetVisuals;

    let mut style = (*ctx.style()).clone();
    let mut v = egui::Visuals::dark();

    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = CornerRadius::same(14);
    v.extreme_bg_color = INPUT;
    v.faint_bg_color = CARD;
    v.override_text_color = Some(TEXT);
    v.hyperlink_color = TEAL;
    v.selection.bg_fill = tint(TEAL, 60);
    v.selection.stroke = Stroke::new(1.0, TEAL);

    let widget = |bg: Color32, weak: Color32, stroke: Color32, fg: Color32| WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: weak,
        bg_stroke: Stroke::new(1.0, stroke),
        fg_stroke: Stroke::new(1.0, fg),
        corner_radius: CornerRadius::same(10),
        expansion: 0.0,
    };
    v.widgets.noninteractive = widget(PANEL, PANEL, BORDER, MUTED);
    v.widgets.inactive = widget(INPUT, INPUT, BORDER, TEXT);
    v.widgets.hovered = widget(INPUT, INPUT, BORDER_HI, TEXT);
    v.widgets.active = widget(INPUT, INPUT, TEAL, TEXT);
    v.widgets.open = widget(INPUT, INPUT, BORDER_HI, TEXT);

    style.visuals = v;
    style.spacing.item_spacing = Vec2::new(10.0, 10.0);
    style.spacing.button_padding = Vec2::new(16.0, 10.0);
    style.spacing.interact_size.y = 36.0;
    style.spacing.window_margin = egui::Margin::same(0);

    // Larger body copy for readability (portal/hub login density).
    style.text_styles = [
        (TextStyle::Heading, light(22.0)),
        (TextStyle::Body, body(15.0)),
        (TextStyle::Button, body(15.0)),
        (TextStyle::Small, body(13.0)),
        (TextStyle::Monospace, mono(13.0)),
    ]
    .into();

    ctx.set_style(style);
}

/// A 0.4–1.0 breathing factor (~2s period) for live status dots. Requests a
/// repaint so the animation keeps running.
pub fn pulse(ctx: &egui::Context) -> f32 {
    ctx.request_repaint();
    let t = ctx.input(|i| i.time);
    0.4 + 0.6 * (0.5 + 0.5 * (t * std::f64::consts::PI).sin()) as f32
}

// ── Logo ────────────────────────────────────────────────────────────────────

/// Official CI logomark (teal palette) from ci-docs `public/logo.svg` — readable
/// on both light and dark surfaces without a multiply tint.
const CI_LOGO: &[u8] = include_bytes!("../assets/logo.svg");

/// Rasterise the CI mark into a texture. Draw with [`logo`] (no tint) so the
/// brand teal fills stay intact. Load once per window and reuse.
pub fn load_logo(ctx: &egui::Context) -> TextureHandle {
    let image = rasterize_svg(CI_LOGO, 128).unwrap_or_else(|_| {
        egui::ColorImage::from_rgba_unmultiplied([1, 1], &[0, 0, 0, 0])
    });
    ctx.load_texture("ci-logo", image, egui::TextureOptions::LINEAR)
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

/// Draw the CI mark at `size`. Pass [`Color32::WHITE`] to keep brand colours;
/// any other colour multiply-tints the glyph (useful for monochrome variants).
pub fn logo(ui: &mut Ui, tex: &TextureHandle, size: f32, color: Color32) {
    let sized = egui::load::SizedTexture::new(tex.id(), Vec2::splat(size));
    ui.add(egui::Image::new(sized).tint(color));
}

// ── Text helpers ──────────────────────────────────────────────────────────--

/// Big light heading (page title).
pub fn heading(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).font(light(24.0)).color(TEXT));
}

/// Muted descriptive subtitle under a heading.
pub fn subtitle(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).font(body(15.0)).color(MUTED));
}

/// Uppercase, slightly bold field/section label.
pub fn field_label(ui: &mut Ui, text: &str) {
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .font(semibold(12.0))
            .color(MUTED),
    );
}

/// Small faint caption / helper text.
pub fn caption(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).font(body(13.0)).color(FAINT));
}

/// Monospace value (config paths, IDs, tags).
pub fn mono_label(ui: &mut Ui, text: &str, color: Color32) {
    ui.label(egui::RichText::new(text).font(mono(12.5)).color(color));
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
        Self { password: true, monospace: true }
    }
    pub fn monospace() -> Self {
        Self { monospace: true, ..Self::default() }
    }
}

/// A consistently styled single-line text field. Returns whether it changed.
pub fn text_field(ui: &mut Ui, value: &mut String, hint: &str, opts: FieldOpts) -> bool {
    let font = if opts.monospace { mono(14.0) } else { body(15.0) };
    let edit = egui::TextEdit::singleline(value)
        .hint_text(egui::RichText::new(hint).color(FAINT))
        .password(opts.password)
        .font(egui::FontSelection::FontId(font))
        .margin(egui::Margin::symmetric(14, 11))
        .desired_width(f32::INFINITY);
    ui.add(edit).changed()
}

// ── Cards ─────────────────────────────────────────────────────────────────--

/// A rounded card surface (fill = CARD, faint border).
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
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

/// A pill badge: tinted background, leading dot, label. Used for connection
/// state and per-file upload status.
pub fn status_badge(ui: &mut Ui, text: &str, color: Color32) -> Response {
    egui::Frame::new()
        .fill(tint(color, 28))
        .corner_radius(CornerRadius::same(255))
        .inner_margin(egui::Margin::symmetric(11, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 7.0;
                let (r, _) = ui.allocate_exact_size(Vec2::splat(6.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 3.0, color);
                ui.label(egui::RichText::new(text).font(semibold(11.0)).color(color));
            });
        })
        .response
}

// ── Capture-lane row ──────────────────────────────────────────────────────--

/// A capture-lane card: glowing accent dot, title + subtitle, trailing mono tag.
pub fn lane_row(ui: &mut Ui, color: Color32, title: &str, subtitle: &str, tag: &str) {
    egui::Frame::new()
        .fill(tint(Color32::BLACK, 46))
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(11))
        .inner_margin(egui::Margin::symmetric(15, 13))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 13.0;
                let (r, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 4.5, color);
                ui.painter()
                    .circle_filled(r.center(), 7.5, tint(color, 45));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(egui::RichText::new(title).font(body(13.0)).color(TEXT));
                    ui.label(
                        egui::RichText::new(subtitle).font(body(11.5)).color(MUTED),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    mono_label(ui, tag, MUTED);
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
    let track = mix(tint(Color32::WHITE, 33), TEAL, how);
    let painter = ui.painter();
    painter.rect_filled(rect, radius, track);
    let cx = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how);
    painter.circle_filled(egui::pos2(cx, rect.center().y), radius - 3.0, Color32::WHITE);
    resp
}

/// A toggle row: switch on the left, title + subtitle on the right, inside a card.
pub fn toggle_row(ui: &mut Ui, on: &mut bool, title: &str, subtitle: &str) -> bool {
    let mut changed = false;
    egui::Frame::new()
        .fill(tint(Color32::BLACK, 46))
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(11))
        .inner_margin(egui::Margin::symmetric(16, 14))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 13.0;
                changed = toggle(ui, on).changed();
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(egui::RichText::new(title).font(body(13.0)).color(TEXT));
                    ui.label(
                        egui::RichText::new(subtitle).font(body(11.5)).color(MUTED),
                    );
                });
            });
        });
    changed
}

// ── Buttons ───────────────────────────────────────────────────────────────--

/// Solid teal primary button with dark text (ci-common `--primary`).
pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    let btn = egui::Button::new(egui::RichText::new(text).font(semibold(15.0)).color(ON_TEAL))
        .fill(TEAL)
        .corner_radius(CornerRadius::same(10))
        .min_size(Vec2::new(0.0, 40.0));
    ui.add(btn)
}

/// Full-width primary button (portal / hub login CTAs).
pub fn primary_button_wide(ui: &mut Ui, text: &str) -> Response {
    let btn = egui::Button::new(egui::RichText::new(text).font(semibold(15.0)).color(ON_TEAL))
        .fill(TEAL)
        .corner_radius(CornerRadius::same(10))
        .min_size(Vec2::new(ui.available_width(), 44.0));
    ui.add(btn)
}

/// Outlined "ghost" button.
pub fn ghost_button(ui: &mut Ui, text: &str) -> Response {
    let btn = egui::Button::new(egui::RichText::new(text).font(body(15.0)).color(TEXT))
        .fill(Color32::TRANSPARENT)
        .stroke(Stroke::new(1.0, BORDER_HI))
        .corner_radius(CornerRadius::same(10))
        .min_size(Vec2::new(0.0, 40.0));
    ui.add(btn)
}

// ── Segmented control ─────────────────────────────────────────────────────--

/// One segment of a segmented control. Returns true when clicked.
pub fn segment(ui: &mut Ui, text: &str, selected: bool) -> bool {
    let (fill, fg) = if selected {
        (TEAL, ON_TEAL)
    } else {
        (Color32::TRANSPARENT, MUTED)
    };
    let font = if selected { semibold(12.5) } else { body(12.5) };
    let btn = egui::Button::new(egui::RichText::new(text).font(font).color(fg))
        .fill(fill)
        .corner_radius(CornerRadius::same(8))
        .min_size(Vec2::new(0.0, 30.0));
    ui.add(btn).clicked()
}

// ── Sidebar nav item ──────────────────────────────────────────────────────--

/// A sidebar navigation entry: leading accent chip, title + subtitle, active
/// background and left accent bar. Returns true when clicked.
pub fn nav_item(
    ui: &mut Ui,
    accent: Color32,
    title: &str,
    subtitle: &str,
    selected: bool,
) -> bool {
    let desired = Vec2::new(ui.available_width(), 46.0);
    let (rect, resp) = ui.allocate_exact_size(desired, Sense::click());
    let painter = ui.painter();

    if selected {
        painter.rect_filled(rect, CornerRadius::same(10), tint(TEAL, 22));
        painter.rect_stroke(
            rect,
            CornerRadius::same(10),
            Stroke::new(1.0, tint(TEAL, 60)),
            StrokeKind::Inside,
        );
        let bar = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 2.0, rect.center().y - 11.0),
            Vec2::new(3.0, 22.0),
        );
        painter.rect_filled(bar, CornerRadius::same(2), TEAL);
    } else if resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(10), tint(Color32::WHITE, 12));
    }

    // Accent chip.
    let chip = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 26.0, rect.center().y),
        Vec2::splat(20.0),
    );
    painter.rect_filled(chip, CornerRadius::same(6), tint(accent, if selected { 70 } else { 40 }));
    painter.circle_filled(chip.center(), 3.0, accent);

    let title_color = if selected { TEXT } else { Color32::from_rgb(0xc4, 0xcb, 0xd8) };
    painter.text(
        egui::pos2(rect.left() + 46.0, rect.center().y - 8.0),
        Align2::LEFT_CENTER,
        title,
        body(13.5),
        title_color,
    );
    painter.text(
        egui::pos2(rect.left() + 46.0, rect.center().y + 8.0),
        Align2::LEFT_CENTER,
        subtitle,
        body(11.0),
        MUTED,
    );

    resp.clicked()
}

// ── File-type chip ────────────────────────────────────────────────────────--

/// Accent colour for a file extension, matching the mockup.
pub fn ext_color(ext: &str) -> Color32 {
    match ext.to_lowercase().as_str() {
        "pdf" => BLUE,
        "md" | "markdown" | "txt" => TEAL_DIM,
        "docx" | "doc" => PURPLE,
        _ => MUTED,
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
        mono(9.0),
        color,
    );
}

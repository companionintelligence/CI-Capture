# theme.rs

## Purpose
Single source of truth for the Companion Intelligence visual system used by the
applet's egui windows (Settings, Upload, Onboarding). Holds colour tokens, font
installation, the global `Visuals`/`Style`, and a library of reusable themed
widgets. Windows style themselves exclusively through this module so colours
and widget code live in exactly one place (DRY).

Palette tracks `@companionintelligence/tokens` (ci-common) dark mode. egui has
no CSS-gradient primitive for widgets, so brand gradients are approximated with
solid accent fills.

## Components

### Colour tokens (`BG`, `PANEL`, `INPUT`, `CARD`, `TEAL`, `BLUE`, `PURPLE`, `GREEN`, `AMBER`, `RED`, `TEXT`, `MUTED`, `FAINT`, lane colours…)
- **Does**: Name every colour in the palette as a `Color32` const. `tint(color, alpha)` returns a translucent variant for blending over a surface.
- **Rationale**: No raw hex/`oklch` values anywhere else in the UI.

### `install_fonts(ctx)` / `apply_visuals(ctx)`
- **Does**: Register Montserrat (proportional) + Source Code Pro (monospace), embedded via `include_bytes!` from `assets/fonts/`; then build the dark `Visuals`, per-widget styling, spacing, and text styles.
- **Interacts with**: Called once from each window's eframe creation closure (`cc.egui_ctx`).
- **Rationale**: Light/Regular/SemiBold Montserrat weights are exposed as named font families (`light`, `body`, `semibold`, `mono` `FontId` helpers).

### `load_logo(ctx)` / `logo(ui, tex, size, color)`
- **Does**: Rasterise `assets/logo.svg` (ci-docs brand mark) to a texture; draw with `Color32::WHITE` to keep teal fills, or tint for monochrome.

### Widget helpers
- **`text_field`, `card`, `status_dot`, `status_badge`, `lane_row`, `toggle`/`toggle_row`, `primary_button`/`ghost_button`, `segment`, `nav_item`, `file_badge`** — themed building blocks. `pulse(ctx)` drives the breathing status dot.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `settings_ui.rs`, `upload_ui.rs`, `onboarding_ui.rs` | These helpers + tokens exist and apply the dark theme | Renaming/removing helpers or tokens |
| Build | `assets/fonts/*.ttf` and `assets/logo.svg` present (compiled in via `include_bytes!`) | Moving/removing those asset files |

## Notes
- Fonts and the logo are embedded in the binary, so installers need no extra
  resource config to ship them. `assets/fonts/OFL-*.txt` carry the (OFL)
  licences for redistribution compliance.
- The tray dropdown is a native OS menu (`tray-icon`) and cannot use this theme;
  only the applet's two egui windows are styled here.

# svg_icon.rs

## Purpose
Renders the embedded SVG mark into tray-icon RGBA bytes so the applet can ship one vector source and derive a native tray image at runtime.

## Components

### `load_svg_icon`
- **Does**: Parses SVG bytes, scales them into a square pixmap, and converts the rasterized bytes into `tray_icon::Icon`.
- **Interacts with**: `resvg`, `usvg`, and `main.rs`.

### `normalize_template_icon`
- **Does**: On macOS only, rewrites non-transparent pixels to white while preserving alpha so the icon behaves correctly when marked as a template status-bar image.
- **Interacts with**: The macOS tray path in `main.rs` where `.with_icon_as_template(true)` is set.

## Contracts

| Dependent | Expects | Breaking changes |
|-----------|---------|------------------|
| `main.rs` | Successful renders return an icon sized to the requested square dimensions | Changing scaling behavior or returning multi-image icon formats |
| macOS tray rendering | Template icons preserve silhouette via alpha even if the source SVG uses dark fills | Removing template normalization while keeping `with_icon_as_template(true)` |

## Notes
- The SVG source stays platform-agnostic; only the macOS raster output is normalized because template status-bar icons are rendered from their mask, not their original colors.

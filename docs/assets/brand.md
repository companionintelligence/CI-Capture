<p align="center">
  <img src="icon.svg" alt="Companion Capture icon" width="96" height="96">
</p>

# Companion Capture — Brand & assets

Visual identity for the CI-Capture repo. Part of the **Companion Intelligence**
family; defer to the Companion Intelligence canon in CI-Common (`styles/UI-STYLE-GUIDE.md`, `@companionintelligence/tokens`); this app consumes it as the brand bundle (`vendor/brand/`, `scripts/vendor-brand.sh`) for anything beyond this repo.

## Logo

The canonical mark is the **CI lens mark** — [`/2024 CI Logo Mark.svg`](../../2024%20CI%20Logo%20Mark.svg)
at the repo root. It ships as the macOS tray icon (rendered as a monochrome
*template* image, so macOS tints it for light/dark menu bars).

| Asset | Use |
|-------|-----|
| [`icon.svg`](icon.svg) | App / product icon — white mark on a teal squircle. Favicons, store tiles, social. |
| [`banner.svg`](banner.svg) | README hero — mark + wordmark + tagline + lane chips. |
| `/2024 CI Logo Mark.svg` | Source mark (monochrome). The tray icon and the recolored marks above derive from it. |

**Don't** re-draw the mark by hand — scale the source paths. **Do** recolor it
white on dark surfaces and keep clear space ≈ 25% of its height around it.

## Palette

The app renders the **CI-Common canonical tokens** (`@companionintelligence/tokens`,
currently vendored at `vendor/brand/.version`), transcribed in
[`src/tokens.rs`](../../src/tokens.rs) and given semantic names in
[`src/theme.rs`](../../src/theme.rs). Both modes are live and follow the OS
appearance — there is no pinned mode.

> Treat `src/tokens.rs` as the source of truth. The table below is a reading aid;
> if the two disagree, the Rust is right. Don't introduce a hex here that isn't a
> token — see CI-Common's `UI-STYLE-GUIDE.md`: prefer a ramp step over a magic hex.

### Surfaces & text

| Role | Dark | Light |
|------|------|-------|
| Canvas | `#041620` | `#f3f3f3` |
| Panel | `#0a222e` | `#f7f7f9` |
| Border | `#2c676d` | `#e2e8f0` |
| Primary | `#c5e8dc` (sage) | `#0a6358` (phthalo) — from `vendor/brand/tokens/tokens.rs`; the `#abd4d8`/`#0f717a` teal was retired at tokens v0.3.0 |
| Body text | `#f5f8fa` | `#010f16` |
| Muted text | `#8f98a3` | `#62748e` |

### Capture lanes

Each of the three capture lanes owns a color, used consistently across the
banner, diagrams, and the in-app flow animation.

| Lane | Dark | Light |
|------|------|-------|
| **Screenshots** | `#70a6af` | `#ffb900` |
| **Activity** | `#409b9b` | `#0a6358` |
| **File edits** | `#c5e8dc` | `#ff3f00` |

### Status

| Role | Dark | Light |
|------|------|-------|
| Success / connected | `#35a05c` | `#138b3f` |
| Warning | `#fbbf24` | `#d97706` |
| Error | `#ef7070` | `#ee3533` |

The greens are the one deliberate departure from the raw tokens: CI-Common's
`success` (`#4ade80` / `#16a34a`) reads as neon against these teal surfaces, so
`theme.rs` darkens both (`GREEN_DARK` / `GREEN_LIGHT`). Keep them in sync there,
not here.

Brand teal ramp (`T50`–`T950`) and the accent hues (aqua, mint) are mode-independent;
see `tokens::teal` and `tokens::accent`.

## Typography

- **Display / wordmark:** Archivo Black (`--font-display` in the canon); UI is Manrope. Falls back to a tight
  `Helvetica Neue` in SVGs so they render anywhere.
- **Mono / code / terminals:** Source Code Pro → `SF Mono`, Menlo, Consolas.
- In-app scale lives in `tokens::text` (display 30 → min 12pt); nothing renders
  below `text::MIN`.
- Aesthetic: clean, technical authority — generous spacing, restrained color,
  one accent gradient.

## Rendering the SVGs

All assets are SVG (crisp at any size, diff-able). To export PNGs:

```bash
rsvg-convert -w 1200 docs/assets/banner.svg      -o banner.png
rsvg-convert -w 512  docs/assets/icon.svg        -o icon.png
rsvg-convert -w 400  docs/assets/tray-menu.svg   -o tray-menu.png
rsvg-convert -w 760  docs/assets/check-server.svg -o check-server.png
```

## Screenshots
<a id="screenshots"></a>

Keep these two categories apart — a hand-drawn panel that *looks* like a capture
will state whatever it said the day it was drawn, long after the UI moved on.

| Asset | Kind |
|-------|------|
| [`screenshot-first-run.png`](screenshot-first-run.png) | **Capture** — the setup assistant. |
| [`screenshot-settings-connection.png`](screenshot-settings-connection.png) | **Capture** — Settings → Connection. |
| [`check-server.svg`](check-server.svg) | **Render of real output** — a terminal card drawn from verbatim `--check-server` stdout. |
| [`tray-menu.svg`](tray-menu.svg) | **Illustration.** Redrawn from `main.rs`'s `append_items` — all 16 items in build order, with the labels `set_text` actually produces. Re-check it when the menu changes; it can't be captured (see below). |

### Taking a window capture

Capture by *window id*, not by cropping the desktop: the result has no
background, no window shadow, and none of the rest of the screen in frame — so
it needs no cropping and can't leak anything.

```bash
# 1. Launch the window on its own. Each UI is a standalone entry point, so no
#    capture lane starts and nothing is sent anywhere.
./target/debug/companion-capture --settings-ui     # or --onboarding-ui, --upload-ui

# 2. Resolve its window id (CGWindowListCopyWindowInfo; needs Screen Recording,
#    not Accessibility), then capture just that window.
screencapture -x -o -l "$WINDOW_ID" shot.png
```

Downscale to ~1700px wide and save with PNG optimisation — that's 2× for the
README's display width and lands each shot comfortably under 400 KB.

Two things to check before committing a shot:

- **The build matches the branding.** Window titles live in `settings_ui.rs` /
  `onboarding_ui.rs`; this product has been renamed twice
  (CI-Applet → Companion Memory → Companion Capture), and a capture taken
  across a rename is wrong the moment it lands.
- **Nothing configured is on screen.** Capture from an install with no
  credential: the windows then show placeholder state, and no portal URL,
  token, or device id can end up in the repo.

### Capture both modes

Light and dark are both shipping modes, and the app follows the OS. Flip the
system appearance between shots rather than forcing a mode in code — the
`ThemePreference::System` path is what users actually get, and a forced-mode
capture can hide a token that only resolves wrong under the real one.

### The tray menu

It can't be captured from a script: a macOS menu bar extra only opens on a real
click, and `osascript` has no assistive access here (`-1719`), so the click
can't be synthesised. Open the menu by hand, then capture the menu's own window
id the same way — poll for a window owned by `companion-capture` whose layer is
above 0, which is what a menu is.

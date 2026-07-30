<p align="center">
  <img src="icon.svg" alt="CI-Applet icon" width="96" height="96">
</p>

# CI-Applet — Brand & assets

Visual identity for the CI-Applet repo. Part of the **Companion Intelligence**
family; defer to CI-Marketing's master brand for anything beyond this repo.

## Logo

The canonical mark is the **CI lens mark** — [`/2024 CI Logo Mark.svg`](../../2024%20CI%20Logo%20Mark.svg)
at the repo root. It ships as the macOS tray icon (rendered as a monochrome
*template* image, so macOS tints it for light/dark menu bars).

| Asset | Use |
|-------|-----|
| [`icon.svg`](icon.svg) | App / product icon — white mark on a blue→violet squircle. Favicons, store tiles, social. |
| [`banner.svg`](banner.svg) | README hero — mark + wordmark + tagline + lane chips. |
| `/2024 CI Logo Mark.svg` | Source mark (monochrome). The tray icon and the recolored marks above derive from it. |

**Don't** re-draw the mark by hand — scale the source paths. **Do** recolor it
white on dark surfaces and keep clear space ≈ 25% of its height around it.

## Palette

| Token | Hex | Role |
|-------|-----|------|
| CI Blue | `#5C8DFF` | Primary accent · **Screenshots** lane |
| Violet | `#7C6BFF` | Gradient partner · **Activity** lane |
| Teal | `#2DD4BF` | Secondary accent · **File-tracking** lane |
| Amber | `#F59E0B` | Highlight / warning |
| Ink | `#1A1A1A` | Logo / dark text |
| Surface (dark) | `#0D1117` / `#161B22` | Panels, terminals |
| Text muted | `#8B949E` | Secondary copy on dark |

The three capture lanes each own a color — blue / violet / teal — used
consistently across the banner, diagrams, and the tray mockup.

Signature gradient: `#5C8DFF → #7C6BFF → #2DD4BF` (left→right).

## Typography

- **Display / wordmark:** Abel (CI master brand). Falls back to a tight
  `Helvetica Neue` in SVGs so they render anywhere.
- **Mono / code / terminals:** Source Code Pro → `SF Mono`, Menlo, Consolas.
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
| [`tray-menu.svg`](tray-menu.svg) | **Illustration — stale.** Says "Quit CI-Applet" and "Login via configured issuer"; the real menu has six more items. Replace with a capture. |

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

### The tray menu

It can't be captured from a script: a macOS menu bar extra only opens on a real
click, and `osascript` has no assistive access here (`-1719`), so the click
can't be synthesised. Open the menu by hand, then capture the menu's own window
id the same way — poll for a window owned by `companion-capture` whose layer is
above 0, which is what a menu is.

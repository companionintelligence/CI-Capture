# CI-Applet on Linux

This tarball contains:

- `CI-Applet` — the binary
- `CI-Applet.desktop` — a menu entry
- `CI-Applet.svg` — the icon

## Requirements

- An **X11** session (the screenshot lane uses X11; Wayland capture is not
  supported yet).
- A system-tray host that speaks **StatusNotifierItem / AppIndicator**
  (GNOME needs the *AppIndicator* extension; KDE, XFCE, and most others work
  out of the box).
- Runtime libraries: GTK 3, `libayatana-appindicator3`, `libxdo`, and X11/xcb.
  On Debian/Ubuntu:

  ```bash
  sudo apt-get install -y libgtk-3-0 libayatana-appindicator3-1 libxdo3 libxcb1
  ```

## Install (per-user)

```bash
install -Dm755 CI-Applet            ~/.local/bin/CI-Applet
install -Dm644 CI-Applet.svg        ~/.local/share/icons/hicolor/scalable/apps/CI-Applet.svg
install -Dm644 CI-Applet.desktop    ~/.local/share/applications/CI-Applet.desktop
update-desktop-database ~/.local/share/applications 2>/dev/null || true
```

Then launch **CI-Applet** from your app menu, or run `~/.local/bin/CI-Applet`.

Check the server connection any time:

```bash
~/.local/bin/CI-Applet --check-server
```

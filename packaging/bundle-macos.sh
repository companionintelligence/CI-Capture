#!/usr/bin/env bash
#
# Assemble CI-Applet.app from a built binary.
#
#   packaging/bundle-macos.sh <path-to-binary> [version]
#
# Run from the repo root. Produces ./CI-Applet.app next to where you run it.
# Used by .github/workflows/release.yml and works the same locally.
set -euo pipefail

BIN="${1:?usage: bundle-macos.sh <binary> [version]}"
VERSION="${2:-0.0.0}"
VERSION="${VERSION#v}" # strip a leading v from tags like v0.1.0

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP="CI-Applet.app"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$BIN" "$APP/Contents/MacOS/CI-Applet"
chmod +x "$APP/Contents/MacOS/CI-Applet"
cp "$HERE/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"
cp "$HERE/Info.plist" "$APP/Contents/Info.plist"

# Stamp the version into the bundle.
plutil -replace CFBundleShortVersionString -string "$VERSION" "$APP/Contents/Info.plist"
plutil -replace CFBundleVersion -string "$VERSION" "$APP/Contents/Info.plist"

# Ad-hoc sign so macOS keeps a stable TCC identity for this build. A real
# Developer ID signature + notarization needs secrets — see release.yml.
codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || \
  echo "warning: ad-hoc codesign failed (continuing unsigned)"

echo "built $APP (version $VERSION)"

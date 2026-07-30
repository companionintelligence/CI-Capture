#!/usr/bin/env bash
# Upload Companion Capture desktop installers + manifest.json + latest.json to the
# shared dl.ci.computer R2 bucket, namespaced under R2_PREFIX so this product
# doesn't collide with Companion Hub's uploads at the bucket root.
#
# Required env:
#   R2_BUCKET          — e.g. dl-prod or dl-dev
#   R2_PREFIX           — e.g. applet
#   DOWNLOAD_BASE_URL  — public HTTPS origin, e.g. https://dl.ci.computer
#   RELEASE_TAG        — e.g. v0.1.0
#   CLOUDFLARE_API_TOKEN, CLOUDFLARE_ACCOUNT_ID — wrangler auth
set -euo pipefail

: "${R2_BUCKET:?R2_BUCKET is required}"
: "${R2_PREFIX:?R2_PREFIX is required}"
: "${DOWNLOAD_BASE_URL:?DOWNLOAD_BASE_URL is required}"
: "${RELEASE_TAG:?RELEASE_TAG is required}"

DOWNLOAD_BASE_URL="${DOWNLOAD_BASE_URL%/}"
R2_PREFIX="${R2_PREFIX%/}"
VERSION="${RELEASE_TAG}"
SEMVER="${VERSION#v}"
DATE="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

upload() {
  wrangler r2 object put --remote "${R2_BUCKET}/${R2_PREFIX}/$1" --file "$2"
}

echo "Uploading Companion Capture ${VERSION} to R2 bucket ${R2_BUCKET}/${R2_PREFIX} (CDN ${DOWNLOAD_BASE_URL}/${R2_PREFIX})"

# macOS
for f in artifacts/companion-capture-macos-arm64/*.dmg; do
  [ -f "$f" ] && upload "${VERSION}/macos/arm/$(basename "$f")" "$f"
done
for f in artifacts/companion-capture-macos-intel/*.dmg; do
  [ -f "$f" ] && upload "${VERSION}/macos/intel/$(basename "$f")" "$f"
done

# Windows x64 (NSIS setup.exe — cargo-packager doesn't produce an .msi here)
for f in artifacts/companion-capture-windows-x64/*.exe; do
  [ -f "$f" ] && upload "${VERSION}/windows/x64/$(basename "$f")" "$f"
done

# Linux x64
for f in artifacts/companion-capture-linux-x64/*.deb; do
  [ -f "$f" ] && upload "${VERSION}/linux/deb/x64/$(basename "$f")" "$f"
done
for f in artifacts/companion-capture-linux-x64/*.AppImage; do
  [ -f "$f" ] && upload "${VERSION}/linux/appimage/x64/$(basename "$f")" "$f"
done
for f in artifacts/companion-capture-linux-x64/*.pkg.tar.*; do
  [ -f "$f" ] && upload "${VERSION}/linux/pacman/x64/$(basename "$f")" "$f"
done

export VERSION SEMVER DATE DOWNLOAD_BASE_URL R2_PREFIX
node <<'NODE'
const fs = require('fs');
const path = require('path');
const crypto = require('crypto');

const { VERSION, SEMVER, DATE, DOWNLOAD_BASE_URL, R2_PREFIX } = process.env;
const manifest = {
  // Frozen wire value — consumers key off this, so it did NOT change with the
  // Companion Capture rename. Same reasoning as R2_PREFIX=applet above.
  product: 'ci-applet',
  version: SEMVER,
  date: DATE,
  platforms: {},
};

const mappings = [
  ['macos-aarch64', 'companion-capture-macos-arm64', { dmg: '.dmg' }, 'macos/arm'],
  ['macos-x86_64', 'companion-capture-macos-intel', { dmg: '.dmg' }, 'macos/intel'],
  ['windows-x86_64', 'companion-capture-windows-x64', { exe: '.exe' }, 'windows/x64'],
  ['linux-x86_64', 'companion-capture-linux-x64', { deb: '.deb', appimage: '.AppImage' }, null],
];

for (const [platform, artifactDir, formats, fixedR2Path] of mappings) {
  const dir = path.join('artifacts', artifactDir);
  if (!fs.existsSync(dir)) continue;
  const files = fs.readdirSync(dir);
  const platformData = {};
  for (const [fmt, ext] of Object.entries(formats)) {
    const file = files.find((f) => f.toLowerCase().endsWith(ext.toLowerCase()));
    if (!file) continue;
    const filePath = path.join(dir, file);
    const stat = fs.statSync(filePath);
    const sha256 = crypto.createHash('sha256').update(fs.readFileSync(filePath)).digest('hex');
    const encodedFile = encodeURIComponent(file);
    let r2Path = fixedR2Path;
    if (!r2Path) r2Path = fmt === 'deb' ? 'linux/deb/x64' : 'linux/appimage/x64';
    platformData[fmt] = {
      url: `${DOWNLOAD_BASE_URL}/${R2_PREFIX}/${VERSION}/${r2Path}/${encodedFile}`,
      size: stat.size,
      sha256,
    };
  }
  // Linux pacman package (*.pkg.tar.*) isn't a fixed extension, so match separately.
  if (platform === 'linux-x86_64') {
    const pkgFile = files.find((f) => /\.pkg\.tar\.[a-z0-9]+$/i.test(f));
    if (pkgFile) {
      const filePath = path.join(dir, pkgFile);
      const stat = fs.statSync(filePath);
      const sha256 = crypto.createHash('sha256').update(fs.readFileSync(filePath)).digest('hex');
      platformData.pacman = {
        url: `${DOWNLOAD_BASE_URL}/${R2_PREFIX}/${VERSION}/linux/pacman/x64/${encodeURIComponent(pkgFile)}`,
        size: stat.size,
        sha256,
      };
    }
  }
  if (Object.keys(platformData).length > 0) {
    manifest.platforms[platform] = platformData;
  }
}

fs.writeFileSync('/tmp/applet-manifest.json', JSON.stringify(manifest, null, 2));
console.log(JSON.stringify(manifest, null, 2));
NODE

wrangler r2 object put --remote "${R2_BUCKET}/${R2_PREFIX}/${VERSION}/manifest.json" --file /tmp/applet-manifest.json

echo "{\"version\":\"${VERSION}\",\"date\":\"${DATE}\"}" > /tmp/applet-latest.json
wrangler r2 object put --remote "${R2_BUCKET}/${R2_PREFIX}/latest.json" --file /tmp/applet-latest.json

echo "Done. Feed: ${DOWNLOAD_BASE_URL}/${R2_PREFIX}/latest.json"

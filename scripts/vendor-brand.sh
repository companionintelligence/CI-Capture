#!/usr/bin/env bash
# vendor-brand.sh — pull the pinned Companion Intelligence brand bundle into vendor/brand/.
#
# CI-Capture is a Rust/egui app, no npm: it consumes the canon as the brand bundle
# CI-Common attaches to every @companionintelligence/tokens release (CI-Common
# CONSUMING.md, Route B). tokens/tokens.rs is include!()d by src/brand.rs; the Manrope
# TTF is include_bytes!()d by src/theme.rs; the mark SVGs feed the tray icon.
#
#   scripts/vendor-brand.sh [version]   # default: vendor/brand/.version
#   scripts/vendor-brand.sh --check     # verify against SHA256SUMS (CI)
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="$HERE/../vendor/brand"
if [ "${1:-}" = "--check" ]; then
  ( cd "$OUT" && shasum -a 256 -c SHA256SUMS --quiet ) && echo "✔ brand assets match ci-brand $(cat "$OUT/.version")"
  exit $?
fi
VERSION="${1:-$(cat "$OUT/.version")}"
TAG="@companionintelligence/tokens@${VERSION}"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
gh release download "$TAG" --repo companionintelligence/CI-Common --pattern "ci-brand-${VERSION}.tar.gz*" --dir "$tmp"
( cd "$tmp" && shasum -a 256 -c "ci-brand-${VERSION}.tar.gz.sha256" --quiet )
tar -xzf "$tmp/ci-brand-${VERSION}.tar.gz" -C "$tmp"
B="$tmp/ci-brand-${VERSION}"
mkdir -p "$OUT/tokens" "$OUT/fonts" "$OUT/logos"
cp "$B/tokens/tokens.rs" "$B/tokens/tokens.json" "$OUT/tokens/"
cp "$B/fonts/manrope-var.ttf" "$B/fonts/LICENSE-Manrope.txt" "$OUT/fonts/"
mkdir -p "$OUT/fonts/static"; cp "$B"/fonts/static/manrope-{300,400,600}.ttf "$OUT/fonts/static/"
cp "$B/logos/mark.svg" "$B/logos/mark-mono.svg" "$OUT/logos/"
grep -E 'tokens/tokens\.(rs|json)|fonts/manrope-var\.ttf|fonts/static/manrope-(300|400|600)\.ttf|LICENSE-Manrope|logos/(mark|mark-mono)\.svg' "$B/SHA256SUMS" > "$OUT/SHA256SUMS"
echo "$VERSION" > "$OUT/.version"
( cd "$OUT" && shasum -a 256 -c SHA256SUMS --quiet )
echo "vendored ci-brand ${VERSION} into vendor/brand"

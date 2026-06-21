#!/bin/sh

set -eu

REPO="${VZ_REPO:-MLTQ/vizier}"
VERSION="${VZ_VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Linux)
      case "$arch" in
        x86_64|amd64) echo "x86_64-unknown-linux-gnu" ;;
        *)
          echo "Unsupported Linux architecture: $arch" >&2
          exit 1
          ;;
      esac
      ;;
    Darwin)
      case "$arch" in
        arm64|aarch64) echo "aarch64-apple-darwin" ;;
        x86_64)
          echo "Unsupported macOS architecture: $arch (Apple Silicon builds only)" >&2
          exit 1
          ;;
        *)
          echo "Unsupported macOS architecture: $arch" >&2
          exit 1
          ;;
      esac
      ;;
    *)
      echo "Unsupported operating system: $os" >&2
      exit 1
      ;;
  esac
}

download_url() {
  target="$1"
  archive="vz-${target}.tar.gz"

  if [ "$VERSION" = "latest" ]; then
    printf '%s\n' "https://github.com/${REPO}/releases/latest/download/${archive}"
  else
    printf '%s\n' "https://github.com/${REPO}/releases/download/${VERSION}/${archive}"
  fi
}

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Missing required command: $1" >&2
    exit 1
  fi
}

need_cmd curl
need_cmd tar

target="$(detect_target)"
url="$(download_url "$target")"
archive="vz-${target}.tar.gz"
tmpdir="$(mktemp -d)"

cleanup() {
  rm -rf "$tmpdir"
}

trap cleanup EXIT INT TERM

echo "Downloading ${url}"
curl -fsSL "$url" -o "${tmpdir}/${archive}"

mkdir -p "$INSTALL_DIR"
tar -xzf "${tmpdir}/${archive}" -C "$tmpdir"
install -m 0755 "${tmpdir}/vz" "${INSTALL_DIR}/vz"

case ":$PATH:" in
  *":${INSTALL_DIR}:"*)
    ;;
  *)
    echo "Installed to ${INSTALL_DIR}/vz"
    echo "${INSTALL_DIR} is not on PATH in this shell."
    echo "Add: export PATH=\"${INSTALL_DIR}:\$PATH\""
    exit 0
    ;;
esac

echo "Installed ${INSTALL_DIR}/vz"

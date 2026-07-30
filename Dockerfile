# syntax=docker/dockerfile:1
#
# Linux container image for Companion Memory App, published to GitHub Packages (ghcr.io).
#
# Headless use works out of the box:
#   docker run --rm ghcr.io/companionintelligence/ci-applet --check-server
#
# The tray GUI needs an X11 display forwarded in:
#   docker run --rm -e DISPLAY=$DISPLAY -v /tmp/.X11-unix:/tmp/.X11-unix \
#     -v ~/.config/com.compint:/home/app/.config/com.compint \
#     ghcr.io/companionintelligence/ci-applet

# ─── builder ──────────────────────────────────────────────────────────────────
FROM rust:1-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
      libgtk-3-dev libxdo-dev libayatana-appindicator3-dev \
      libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxcb-randr0-dev \
      libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# Cache the cargo registry + target dir across builds (BuildKit), then copy the
# binary out of the (non-persisted) target mount into a real image layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --features rich-extractors \
    && cp target/release/companion-memory-app /usr/local/bin/companion-memory-app \
    && (strip /usr/local/bin/companion-memory-app || true)

# ─── runtime ──────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

# Runtime closure for the GTK tray + X11/xcb screen capture + GL (GUI mode).
RUN apt-get update && apt-get install -y --no-install-recommends \
      libgtk-3-0 libayatana-appindicator3-1 libxdo3 \
      libx11-6 libxcb1 libxcb-render0 libxcb-shape0 libxcb-xfixes0 libxcb-randr0 \
      libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 \
      libgl1 libegl1 ca-certificates \
  && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home --uid 10001 app
USER app

COPY --from=builder /usr/local/bin/companion-memory-app /usr/local/bin/companion-memory-app

LABEL org.opencontainers.image.title="Companion Memory App" \
      org.opencontainers.image.description="Companion Intelligence capture applet — screen, activity & file capture for CI-Server" \
      org.opencontainers.image.source="https://github.com/companionintelligence/CI-Applet" \
      org.opencontainers.image.licenses="LicenseRef-CI-Commercial-1.0"

ENTRYPOINT ["/usr/local/bin/companion-memory-app"]

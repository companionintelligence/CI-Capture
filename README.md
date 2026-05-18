# CI-Applet

A lightweight, cross-platform desktop tray app that captures user activity (screen + machine state, and — in upcoming phases — file activity + extracted documents) and ships it to the user's personal [Companion Intelligence](https://github.com/companionintelligence) digital-memory server.

CI-Applet is one of the on-device sensors in the CI stack:

| Repo | Role |
|---|---|
| [companionintelligence/CI-Server](https://github.com/companionintelligence/CI-Server) | "Digital Memory" — Postgres + MinIO + GraphQL backend that stores everything this applet captures. |
| [companionintelligence/CI-Portal](https://github.com/companionintelligence/CI-Portal) | "Portal" — browser-side auth proxy. The applet pairs through it. |
| [companionintelligence/CI-Marketplace](https://github.com/companionintelligence/CI-Marketplace) | "App store" for CI. CI-Applet is one of several distributable apps. |
| [companionintelligence/CI-Applet](https://github.com/companionintelligence/CI-Applet) | **This repo.** Desktop tray client. |

For the longer story:

- **Architecture overview** → [architecture.md](architecture.md)
- **Roadmap** → [roadmap.md](roadmap.md)
- **Per-phase plans** → [docs/plan/](docs/plan/)

---

## What it does today

- Runs in the system tray (macOS / Windows / Linux).
- Captures the primary display every ≥30 s (skipped while the screensaver is active on macOS).
- Asks a vision LLM (OpenAI-compatible) to summarize what the user is doing in each frame.
- In lockstep with each screen capture, takes a `vizier` observation of machine state and diffs it against the previous one.
- Sends both streams to the user's CI-Server.
- Authenticates via a browser-based pairing handshake against CI-Portal.

Upcoming (see roadmap):

- Tauri v2 shell ([#1](https://github.com/companionintelligence/CI-Applet/issues/1))
- Native file watcher + activity-event ingest ([#4](https://github.com/companionintelligence/CI-Applet/issues/4))
- Markdown conversion pipeline for changed documents ([#3](https://github.com/companionintelligence/CI-Applet/issues/3))

---

## Build

Prereqs:

- Rust toolchain (edition 2024 — `rustup default stable` recent enough to support it).
- Local clone of the sibling [vizier](https://github.com/companionintelligence) crate at `../../vizier` (CI-Applet depends on it via a path dependency).

```bash
cargo build --release
```

The release binary is `target/release/CI-Applet` (or `CI-Applet.exe` on Windows).

## Run

```bash
./target/release/CI-Applet                  # tray app
./target/release/CI-Applet --settings-ui    # open settings window only
./target/release/CI-Applet --upload-ui      # open file-upload window only
```

On first launch:

1. Click the tray icon → **Sign in via Portal**.
2. A browser tab opens to your CI-Portal session. Confirm the pairing there.
3. The applet receives a bearer token and starts ingesting captures to your CI-Server.

If you're running a fully local CI stack, the default portal URL is `https://memory.ci.localhost`. Override it under **Settings → Server**.

## Configuration

Settings live in `config.json` in the OS-standard app-config directory:

| OS | Path |
|---|---|
| macOS | `~/Library/Application Support/CI-Applet/config.json` |
| Linux | `~/.config/CI-Applet/config.json` |
| Windows | `%APPDATA%\CI-Applet\config.json` |

See [src/config.md](src/config.md) for field-level docs.

## Project layout

```
src/
├── main.rs            tray, pairing entrypoint, sub-window dispatch
├── worker.rs          background capture + analysis loop
├── capture.rs         primary-display screenshot + diff
├── analysis.rs        vision-LLM client
├── server_auth.rs     portal pairing GraphQL
├── server_ingest.rs   post-pairing GraphQL/REST ingest
├── server_upload.rs   one-off file upload helpers
├── server_common.rs   shared URL helpers
├── settings_ui.rs     egui settings window (--settings-ui)
├── upload_ui.rs       egui upload window (--upload-ui)
├── config.rs          AppConfig schema + ConfigManager
├── db.rs              LEGACY direct-Postgres path
├── minio.rs           LEGACY direct-MinIO path
└── svg_icon.rs        tray icon rasterizer
```

Every `.rs` file has a sibling `.md` describing its purpose, components, and contracts. Start there when you read the code.

## Contributing

Pick up issues at [github.com/companionintelligence/CI-Applet/issues](https://github.com/companionintelligence/CI-Applet/issues). Each phase plan in [docs/plan/](docs/plan/) is a self-contained spec for a piece of upcoming work.

## License

See organization-wide licensing in the [companionintelligence](https://github.com/companionintelligence) org.

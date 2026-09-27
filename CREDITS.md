# Credits

Companion Intelligence is built on the work of the open-source community.
This page names the projects we depend on directly, in rough order of how much
of this repository stands on them.

## 1. Foundation

**CI-Capture is derived from [Screenpipe](https://github.com/mediar-ai/screenpipe)**, and would not exist
without it. See [NOTICE.md](NOTICE.md) for the licence terms that govern that
derivation.

## 3. Runtime dependencies

| | Project | Licence | What it does |
| :-: | --- | --- | --- |
| <img src="https://github.com/dtolnay.png?size=40" width="20" height="20" alt=""> | [`anyhow`](https://github.com/dtolnay/anyhow) | MIT OR Apache-2.0 | Flexible concrete Error type built on std::error::Error |
| <img src="https://github.com/zzzgydi.png?size=40" width="20" height="20" alt=""> | [`auto-launch`](https://github.com/zzzgydi/auto-launch) | MIT | Auto launch any application or executable at startup. Supports Windows, macOS, and Linux. |
| <img src="https://github.com/smithy-lang.png?size=40" width="20" height="20" alt=""> | [`aws-config`](https://github.com/smithy-lang/smithy-rs) | Apache-2.0 | AWS SDK config and credential provider implementations. |
| <img src="https://github.com/smithy-lang.png?size=40" width="20" height="20" alt=""> | [`aws-credential-types`](https://github.com/smithy-lang/smithy-rs) | Apache-2.0 | Types for AWS SDK credentials. |
| <img src="https://github.com/awslabs.png?size=40" width="20" height="20" alt=""> | [`aws-sdk-s3`](https://github.com/awslabs/aws-sdk-rust) | Apache-2.0 | AWS SDK for Amazon Simple Storage Service |
| <img src="https://github.com/marshallpierce.png?size=40" width="20" height="20" alt=""> | [`base64`](https://github.com/marshallpierce/rust-base64) | MIT OR Apache-2.0 | encodes and decodes base64 as bytes or utf8 |
| <img src="https://github.com/chronotope.png?size=40" width="20" height="20" alt=""> | [`chrono`](https://github.com/chronotope/chrono) | MIT OR Apache-2.0 | Date and time library for Rust |
| <img src="https://github.com/clap-rs.png?size=40" width="20" height="20" alt=""> | [`clap`](https://github.com/clap-rs/clap) | MIT OR Apache-2.0 | A simple to use, efficient, and full-featured Command Line Argument Parser |
| <img src="https://github.com/servo.png?size=40" width="20" height="20" alt=""> | [`core-foundation`](https://github.com/servo/core-foundation-rs) | MIT OR Apache-2.0 | Bindings to Core Foundation for macOS |
| <img src="https://github.com/servo.png?size=40" width="20" height="20" alt=""> | [`core-graphics`](https://github.com/servo/core-foundation-rs) | MIT OR Apache-2.0 | Bindings to Core Graphics for macOS |
| <img src="https://github.com/BurntSushi.png?size=40" width="20" height="20" alt=""> | [`csv`](https://github.com/BurntSushi/rust-csv) | Unlicense/MIT | Fast CSV parsing with support for serde. |
| <img src="https://github.com/rust-dark-light.png?size=40" width="20" height="20" alt=""> | [`dark-light`](https://github.com/rust-dark-light/rust-dark-light) | MIT/Apache-2.0 | Detect if dark mode or light mode is enabled |
| <img src="https://github.com/deadpool-rs.png?size=40" width="20" height="20" alt=""> | [`deadpool-postgres`](https://github.com/deadpool-rs/deadpool) | MIT OR Apache-2.0 | Dead simple async pool for tokio-postgres |
| <img src="https://github.com/soc.png?size=40" width="20" height="20" alt=""> | [`directories`](https://github.com/soc/directories-rs) | MIT OR Apache-2.0 | A tiny mid-level library that provides platform-specific standard locations of directories f |
|  | [`dirs`](https://codeberg.org/dirs/dirs-rs) | MIT OR Apache-2.0 | A tiny low-level library that provides platform-specific standard locations of directories f |
| <img src="https://github.com/emilk.png?size=40" width="20" height="20" alt=""> | [`eframe`](https://github.com/emilk/egui) | MIT OR Apache-2.0 | egui framework - write GUI apps that compiles to web and/or natively |
| <img src="https://github.com/emilk.png?size=40" width="20" height="20" alt=""> | [`egui`](https://github.com/emilk/egui) | MIT OR Apache-2.0 | An easy-to-use immediate mode GUI that runs on both web and native |
| <img src="https://github.com/rust-cli.png?size=40" width="20" height="20" alt=""> | [`env_logger`](https://github.com/rust-cli/env_logger) | MIT OR Apache-2.0 | A logging implementation for log which is configured via an environment variable. |
| <img src="https://github.com/danigm.png?size=40" width="20" height="20" alt=""> | [`epub`](https://github.com/danigm/epub-rs) | GPL-3.0 | Library to support the reading of epub files. |
| <img src="https://github.com/yoshuawuyts.png?size=40" width="20" height="20" alt=""> | [`fd-lock`](https://github.com/yoshuawuyts/fd-lock) | MIT OR Apache-2.0 | Advisory cross-platform lock on a file using a file descriptor to it. |
| <img src="https://github.com/BurntSushi.png?size=40" width="20" height="20" alt=""> | [`globset`](https://github.com/BurntSushi/ripgrep) | Unlicense OR MIT | Cross platform single glob and glob set matching. Glob set matching is the process of matchi |
|  | [`html2md`](https://gitlab.com/Kanedias/html2md) | GPL-3.0-or-later | Library and binary to convert simple html documents into markdown |
| <img src="https://github.com/messense.png?size=40" width="20" height="20" alt=""> | [`if-addrs`](https://github.com/messense/if-addrs) | MIT OR BSD-3-Clause | Return interface IP addresses on Posix and windows systems |
| <img src="https://github.com/image-rs.png?size=40" width="20" height="20" alt=""> | [`image`](https://github.com/image-rs/image) | MIT OR Apache-2.0 | Imaging library. Provides basic image processing and encoders/decoders for common image form |
| <img src="https://github.com/idubrov.png?size=40" width="20" height="20" alt=""> | [`json-patch`](https://github.com/idubrov/json-patch) | MIT/Apache-2.0 | RFC 6902, JavaScript Object Notation (JSON) Patch |
| <img src="https://github.com/rust-lang.png?size=40" width="20" height="20" alt=""> | [`libc`](https://github.com/rust-lang/libc) | MIT OR Apache-2.0 | Raw FFI bindings to platform libraries like libc. |
| <img src="https://github.com/rust-lang.png?size=40" width="20" height="20" alt=""> | [`log`](https://github.com/rust-lang/log) | MIT OR Apache-2.0 | A lightweight logging facade for Rust |
| <img src="https://github.com/stalwartlabs.png?size=40" width="20" height="20" alt=""> | [`mail-parser`](https://github.com/stalwartlabs/mail-parser) | Apache-2.0 OR MIT | Fast and robust e-mail parsing library for Rust |
| <img src="https://github.com/notify-rs.png?size=40" width="20" height="20" alt=""> | [`notify`](https://github.com/notify-rs/notify) | CC0-1.0 | Cross-platform filesystem notification library |
| <img src="https://github.com/SSheldon.png?size=40" width="20" height="20" alt=""> | [`objc`](http://github.com/SSheldon/rust-objc) | MIT | Objective-C Runtime bindings and wrapper for Rust. |
| <img src="https://github.com/Amanieu.png?size=40" width="20" height="20" alt=""> | [`parking_lot`](https://github.com/Amanieu/parking_lot) | MIT OR Apache-2.0 | More compact and efficient implementations of the standard synchronization primitives. |
| <img src="https://github.com/seanmonstar.png?size=40" width="20" height="20" alt=""> | [`reqwest`](https://github.com/seanmonstar/reqwest) | MIT OR Apache-2.0 | higher level HTTP client library |
| <img src="https://github.com/linebender.png?size=40" width="20" height="20" alt=""> | [`resvg`](https://github.com/linebender/resvg) | Apache-2.0 OR MIT | An SVG rendering library. |
| <img src="https://github.com/rusqlite.png?size=40" width="20" height="20" alt=""> | [`rusqlite`](https://github.com/rusqlite/rusqlite) | MIT | Ergonomic wrapper for SQLite |
| <img src="https://github.com/longbridge.png?size=40" width="20" height="20" alt=""> | [`rust-i18n`](https://github.com/longbridge/rust-i18n) | MIT | Rust I18n is use Rust codegen for load YAML file storage translations on compile time, and g |
| <img src="https://github.com/nashaofu.png?size=40" width="20" height="20" alt=""> | [`screenshots`](https://github.com/nashaofu/screenshots-rs) | Apache-2.0 | Move to XCap |
| <img src="https://github.com/getsentry.png?size=40" width="20" height="20" alt=""> | [`sentry`](https://sentry.io/welcome/) | MIT | Sentry (sentry.io) client for Rust. |
| <img src="https://github.com/serde-rs.png?size=40" width="20" height="20" alt=""> | [`serde`](https://serde.rs) | MIT OR Apache-2.0 | A generic serialization/deserialization framework |
| <img src="https://github.com/serde-rs.png?size=40" width="20" height="20" alt=""> | [`serde_json`](https://github.com/serde-rs/json) | MIT OR Apache-2.0 | A JSON serialization file format |
| <img src="https://github.com/RustCrypto.png?size=40" width="20" height="20" alt=""> | [`sha2`](https://github.com/RustCrypto/hashes) | MIT OR Apache-2.0 | Pure Rust implementation of the SHA-2 hash function family including SHA-224, SHA-256, SHA-3 |
| <img src="https://github.com/1Password.png?size=40" width="20" height="20" alt=""> | [`sys-locale`](https://github.com/1Password/sys-locale) | MIT OR Apache-2.0 | Small and lightweight library to obtain the active system locale |
| <img src="https://github.com/GuillaumeGomez.png?size=40" width="20" height="20" alt=""> | [`sysinfo`](https://github.com/GuillaumeGomez/sysinfo) | MIT | Library to get system information such as processes, CPUs, disks, components and networks |
| <img src="https://github.com/tauri-apps.png?size=40" width="20" height="20" alt=""> | [`tao`](https://github.com/tauri-apps/tao) | Apache-2.0 | Cross-platform window manager library. |
| <img src="https://github.com/dtolnay.png?size=40" width="20" height="20" alt=""> | [`thiserror`](https://github.com/dtolnay/thiserror) | MIT OR Apache-2.0 | derive(Error) |
| <img src="https://github.com/jdm.png?size=40" width="20" height="20" alt=""> | [`tinyfiledialogs`](https://github.com/jdm/tinyfiledialogs-rs) | zlib-acknowledgement | High-level Rust binding for the tinyfiledialogs C library. |
| <img src="https://github.com/tokio-rs.png?size=40" width="20" height="20" alt=""> | [`tokio`](https://tokio.rs) | MIT | An event-driven, non-blocking I/O platform for writing asynchronous I/O backed applications. |
| <img src="https://github.com/rust-postgres.png?size=40" width="20" height="20" alt=""> | [`tokio-postgres`](https://github.com/rust-postgres/rust-postgres) | MIT OR Apache-2.0 | A native, asynchronous PostgreSQL client |
| <img src="https://github.com/tauri-apps.png?size=40" width="20" height="20" alt=""> | [`tray-icon`](https://github.com/tauri-apps/tray-icon) | MIT OR Apache-2.0 | Create tray icons for desktop applications |
| <img src="https://github.com/algesten.png?size=40" width="20" height="20" alt=""> | [`ureq`](https://github.com/algesten/ureq) | MIT OR Apache-2.0 | Simple, safe HTTP client |
| <img src="https://github.com/linebender.png?size=40" width="20" height="20" alt=""> | [`usvg`](https://github.com/linebender/resvg) | Apache-2.0 OR MIT | An SVG simplification library. |
| <img src="https://github.com/uuid-rs.png?size=40" width="20" height="20" alt=""> | [`uuid`](https://github.com/uuid-rs/uuid) | Apache-2.0 OR MIT | A library to generate and parse UUIDs. |
| <img src="https://github.com/vizier-lab.png?size=40" width="20" height="20" alt=""> | [`vizier`](https://github.com/vizier-lab/vizier) | MIT | 21st Century Digital Steward; Right hand agent for you majesty |
| <img src="https://github.com/BurntSushi.png?size=40" width="20" height="20" alt=""> | [`walkdir`](https://github.com/BurntSushi/walkdir) | Unlicense/MIT | Recursively walk a directory. |
| <img src="https://github.com/ardaku.png?size=40" width="20" height="20" alt=""> | [`whoami`](https://github.com/ardaku/whoami/releases) | Apache-2.0 OR BSL-1.0 OR MIT | Rust library for getting information about the current user and environment |
| <img src="https://github.com/microsoft.png?size=40" width="20" height="20" alt=""> | [`windows-sys`](https://github.com/microsoft/windows-rs) | MIT OR Apache-2.0 | Rust for Windows |
| <img src="https://github.com/rust-windowing.png?size=40" width="20" height="20" alt=""> | [`winit`](https://github.com/rust-windowing/winit) | Apache-2.0 | Cross-platform window creation library. |
| <img src="https://github.com/gentoo90.png?size=40" width="20" height="20" alt=""> | [`winreg`](https://github.com/gentoo90/winreg-rs) | MIT | Rust bindings to MS Windows Registry API |
| <img src="https://github.com/z-galaxy.png?size=40" width="20" height="20" alt=""> | [`zbus`](https://github.com/z-galaxy/zbus) | MIT | API for D-Bus communication |

## 4. Build and tooling

| | Project | Licence | What it does |
| :-: | --- | --- | --- |
| <img src="https://github.com/Stebalien.png?size=40" width="20" height="20" alt=""> | [`tempfile`](https://stebalien.com/projects/tempfile-rs/) | MIT OR Apache-2.0 | A library for managing temporary files and directories. |

---

Thank you to everyone who maintains these projects.

---

_Generated by [`scripts/generate-attribution.mjs`](https://github.com/companionintelligence/CI-Engineering/blob/main/scripts/generate-attribution.mjs) for `CI-Capture`. Regenerate rather than editing generated sections by hand._

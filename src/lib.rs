//! CI-Applet library crate.
//!
//! Every module that used to be declared in `main.rs` lives here so the
//! upcoming Tauri shell ([`docs/plan/01-tauri-shell.md`](../../docs/plan/01-tauri-shell.md))
//! can consume them from `src-tauri/`. The existing binary at `src/main.rs`
//! also depends on this library — see the binary target in `Cargo.toml`.
//!
//! No public surface is rearranged in this commit; only re-homed. See the
//! per-module `.md` siblings for component-level documentation.

pub mod analysis;
pub mod capture;
pub mod config;
pub mod db;
pub mod minio;
pub mod server_auth;
pub mod server_common;
pub mod server_ingest;
pub mod server_upload;
pub mod settings_ui;
pub mod svg_icon;
pub mod upload_ui;
pub mod worker;

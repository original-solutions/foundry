//! The web UI, built into `web/dist` by `npm --prefix web run build` and compiled into the binary.
//!
//! `web/dist` is not tracked, so run that build before `cargo build` or the binary serves no UI.
//! `allow_missing` lets the crate compile without it, for `cargo check` and rust-analyzer on a
//! fresh clone, and `build.rs` rebuilds the crate once the folder appears. `tests/web_assets.rs`
//! fails until the built page is embedded, so the gate still proves the embed.

/// Every file under `web/dist`, keyed by its path relative to that folder.
#[derive(rust_embed::Embed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
pub struct WebAssets;

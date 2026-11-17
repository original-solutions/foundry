//! The web UI, built into `web/dist` by `npm --prefix web run build` and compiled into the binary.

/// Every file under `web/dist`, keyed by its path relative to that folder.
#[derive(rust_embed::Embed)]
#[folder = "../../web/dist"]
pub struct WebAssets;

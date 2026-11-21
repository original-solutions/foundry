//! The web UI, built into `web/dist` by `npm --prefix web run build` and compiled into the binary.
//!
//! `web/dist` is not tracked, so run that build before `cargo build` or the binary serves no UI.
//! `allow_missing` lets the crate compile without it, for `cargo check` and rust-analyzer on a
//! fresh clone, and `build.rs` rebuilds the crate once the folder appears. `tests/web_assets.rs`
//! fails until the built page is embedded, so the gate still proves the embed.
//!
//! [`serve`] answers every path the API has no route for, so the daemon's own address opens the UI.

use axum::http::{HeaderValue, Method, Uri, header};
use axum::response::{IntoResponse as _, Response};

/// Every file under `web/dist`, keyed by its path relative to that folder.
#[derive(rust_embed::Embed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
pub struct WebAssets;

/// What the API answers for a path it has no route for: the UI. A file under `web/dist` by its
/// path, `index.html` at `/`, and `index.html` for any other path whose last part has no
/// extension, so a page the UI routes loads the app. A path under `/api`, a missing file, or a
/// method other than `GET` or `HEAD` stays the API's `404`.
pub(super) async fn serve(method: Method, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let api = path == "api" || path.starts_with("api/");
    if api || (method != Method::GET && method != Method::HEAD) {
        return super::no_route().await;
    }
    let file = if path.is_empty() { "index.html" } else { path };
    if let Some(asset) = WebAssets::get(file) {
        return reply(file, asset.data.into_owned());
    }
    let page = !path.rsplit('/').next().unwrap_or("").contains('.');
    match WebAssets::get("index.html").filter(|_| page) {
        Some(index) => reply("index.html", index.data.into_owned()),
        None => super::no_route().await,
    }
}

fn reply(file: &str, body: Vec<u8>) -> Response {
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(file)),
    );
    response
}

/// The content type of a file Vite puts in `web/dist`, by its extension.
fn content_type(file: &str) -> &'static str {
    match file.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        _ => "application/octet-stream",
    }
}

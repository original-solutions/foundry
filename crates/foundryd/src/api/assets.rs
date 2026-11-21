//! The web UI, built into `web/dist` by `npm --prefix web run build` and compiled into the binary.
//!
//! `web/dist` is not tracked, so run that build before `cargo build` or the binary serves no UI.
//! `allow_missing` lets the crate compile without it, for `cargo check` and rust-analyzer on a
//! fresh clone, and `build.rs` rebuilds the crate once the folder appears. `tests/web_assets.rs`
//! fails until the built page is embedded, so the gate still proves the embed.
//!
//! [`serve`] answers every path the API has no route for, so the daemon's own address opens the UI.

use std::borrow::Cow;

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
        return reply(file, asset.data);
    }
    let page = !path.rsplit('/').next().unwrap_or("").contains('.');
    match WebAssets::get("index.html").filter(|_| page) {
        Some(index) => reply("index.html", index.data),
        None => super::no_route().await,
    }
}

/// A file of the UI with its type, how long a browser may keep it, and a refusal to be framed.
/// Vite names every file under `assets/` by a hash of its content, so a browser keeps those for
/// good; any other file, `index.html` among them, it asks for again each time. No other site may
/// show the UI in a frame: a click there would act on this machine without the user seeing it.
fn reply(file: &str, body: Cow<'static, [u8]>) -> Response {
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(file)),
    );
    let cache = if file.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_of_file_vite_writes_gets_its_type_and_any_other_is_bytes() {
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(
            content_type("assets/index-1a2b.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            content_type("assets/index-1a2b.css"),
            "text/css; charset=utf-8"
        );
        assert_eq!(content_type("assets/logo-1a2b.svg"), "image/svg+xml");
        assert_eq!(content_type("assets/blob.wasm"), "application/octet-stream");
        assert_eq!(content_type("README"), "application/octet-stream");
    }
}

//! `POST /api/terminals/{id}/files?name=<name>`: keeps the body, a file dropped or pasted onto a
//! live terminal of this machine, in the terminal's files folder, and answers `201 {path}`. The
//! UI then types the path into the terminal, as dragging a file into a desktop terminal does, so
//! Claude Code can read it, a screenshot as an image. Nothing is typed by the daemon.
//!
//! The name is cut to letters, digits, `.`, `_` and `-`, so the path needs no quoting, and a
//! name already kept gets `-2`, `-3`… so a path already typed is never overwritten. A body over
//! [`MAX_FILE`] is `413`; a terminal that is not live here is `404`. The folder goes with the
//! terminal when it is closed.

use std::io::{self, Write as _};
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use foundry_proto::ids::TerminalId;
use serde::Deserialize;

use super::{Api, ApiError, error, parse_id};

/// The largest file a terminal is sent: a full-screen screenshot is a few MB.
pub const MAX_FILE: usize = 25 * 1024 * 1024;

#[derive(Deserialize)]
pub(super) struct Named {
    #[serde(default)]
    name: String,
}

pub(super) async fn keep(
    State(api): State<Api>,
    UrlPath(id): UrlPath<String>,
    Query(named): Query<Named>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id, "terminal", TerminalId)?;
    let folder = api.terminals.files(id)?;
    let bytes = match body {
        Ok(bytes) => bytes,
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return Ok(error(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("a file sent to a terminal is at most {} MB", MAX_FILE >> 20),
            ));
        }
        Err(e) => return Err(ApiError::Rejected(e.status(), e.body_text())),
    };
    let name = plain_name(&named.name);
    let kept = tokio::task::spawn_blocking(move || write_new(&folder, &name, &bytes))
        .await
        .map_err(|e| ApiError::Rejected(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match kept {
        Ok(path) => Ok((
            StatusCode::CREATED,
            Json(serde_json::json!({ "path": path.to_string_lossy() })),
        )
            .into_response()),
        Err(e) => {
            tracing::warn!("api: a file for terminal {id} could not be kept: {e}");
            Ok(error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("the file could not be kept: {e}"),
            ))
        }
    }
}

/// `name`'s last path part with every run of other characters than letters, digits, `.`, `_`
/// and `-` made one `-`, its extension kept, at most 80 characters before it. Empty is `file`.
fn plain_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let (stem, ext) = match base.rfind('.') {
        Some(dot) if dot > 0 => (&base[..dot], Some(&base[dot + 1..])),
        _ => (base, None),
    };
    let mut plain = String::new();
    for c in stem.chars() {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '.' | '_') {
            c
        } else {
            '-'
        };
        if c != '-' || !plain.ends_with('-') {
            plain.push(c);
        }
    }
    let plain: String = plain
        .trim_matches(['-', '.'])
        .chars()
        .take(80)
        .collect::<String>();
    let plain = if plain.is_empty() {
        "file".to_owned()
    } else {
        plain
    };
    let ext: String = ext
        .unwrap_or_default()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(10)
        .collect();
    if ext.is_empty() {
        plain
    } else {
        format!("{plain}.{ext}")
    }
}

/// Writes `bytes` to `folder/name`, or to `name` with `-2`, `-3`… before its extension when that
/// is taken, and returns the path written. Never overwrites a file. The folder is made readable
/// by this user only: a screenshot can show anything.
fn write_new(folder: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(folder)?;
    let (stem, ext) = match name.rfind('.') {
        Some(dot) => (&name[..dot], &name[dot..]),
        None => (name, ""),
    };
    for n in 1.. {
        let path = if n == 1 {
            folder.join(name)
        } else {
            folder.join(format!("{stem}-{n}{ext}"))
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(bytes)?;
                return Ok(path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    unreachable!("some name is free")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_cut_to_a_plain_one_that_needs_no_quoting() {
        for (given, plain) in [
            (
                "Screenshot 2026-11-21 at 10.02.11.png",
                "Screenshot-2026-11-21-at-10.02.11.png",
            ),
            ("image.png", "image.png"),
            ("../../etc/passwd", "passwd"),
            ("C:\\Users\\me\\notes.txt", "notes.txt"),
            ("rm -rf $(x);`y`.sh", "rm-rf-x-y.sh"),
            (".env", "env"),
            ("", "file"),
            ("***", "file"),
            ("report.tar.gz", "report.tar.gz"),
            ("photo.JP G", "photo.JPG"),
        ] {
            assert_eq!(plain_name(given), plain, "{given:?}");
        }
        assert_eq!(plain_name(&format!("{}.png", "a".repeat(200))).len(), 84);
    }
}

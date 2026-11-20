//! The terminal WebSocket, `GET /api/terminals/{id}`. A terminal that is not live here, but
//! whose record names another machine, is carried from that machine over a relay link
//! ([`crate::sync::relay`]); the socket behaves the same either way.
//!
//! The first frame is binary: the terminal's kept screen and scrollback. Each binary frame
//! after it is the child's live output. A binary frame from the UI is typed input, and a text
//! frame `{"type":"resize","cols":C,"rows":R}` resizes the terminal. Input the terminal refuses
//! is answered with a text frame `{"type":"error","error":"..."}` and the socket stays open.
//!
//! The socket closes with code 1000 once the child has exited and its last byte is sent, 1013
//! when the holder cut this client off for falling behind (attach again), and 1011 with the
//! reason for any other end. A client that attaches again after a 1013 gets the whole kept
//! screen again as its first frame, so it must reset its screen before writing that frame. A
//! terminal that is not live here and is not another machine's is `404` before the upgrade.
//!
//! For another machine's terminal, the socket also closes with 1013 and the reason `the link
//! to <name> is down` when the link to that machine goes, and an attach while it is down is
//! `503` with that reason before the upgrade. An attach the owner refuses is `502` with its
//! reason.

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{self, CloseFrame, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use bytes::Bytes;
use foundry_proto::ids::TerminalId;
use futures::StreamExt as _;
use serde::Deserialize;

use super::{Api, ApiError, error, parse_id, redact};
use crate::sync::relay::{self, RelayError, Remote};
use crate::terminals::{Output, Snapshot, TerminalError};

/// The longest close reason a WebSocket close frame carries, in bytes.
const REASON_AT_MOST: usize = 123;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Control {
    Resize { cols: u16, rows: u16 },
}

pub(super) async fn socket(
    State(api): State<Api>,
    Path(id): Path<String>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    let upgrade = upgrade?;
    let id = parse_id(&id, "terminal", TerminalId)?;
    let (snapshot, end) = match api.terminals.attach(id).await {
        Ok((snapshot, output)) => (snapshot, End::Here(output)),
        Err(TerminalError::Unknown(_)) => {
            let Some(owner) = api.peers.owner_of(id).await? else {
                return Err(TerminalError::Unknown(id).into());
            };
            match api.peers.open_terminal(owner, id).await {
                Ok((snapshot, remote)) => (snapshot, End::There(remote)),
                Err(refused) => return Ok(not_opened(&refused)),
            }
        }
        Err(e) => return Err(e.into()),
    };
    Ok(upgrade.on_upgrade(move |socket| carry(api, id, snapshot, end, socket)))
}

/// The answer to an attach another machine's terminal could not open for.
fn not_opened(refused: &RelayError) -> Response {
    let status = match refused {
        RelayError::Down(_) => StatusCode::SERVICE_UNAVAILABLE,
        RelayError::Refused(_) => StatusCode::BAD_GATEWAY,
    };
    let message = redact(&refused.to_string());
    tracing::info!("api: a terminal on another machine did not open: {message}");
    error(status, message)
}

/// Where a socket's terminal is: live here, or another machine's through a relay.
enum End {
    Here(Output),
    There(Remote),
}

/// What the terminal's end says to the socket.
enum Said {
    Output(Bytes),
    /// Input the terminal refused; the socket stays open.
    Refused(String),
    Close(u16, String),
}

impl End {
    async fn next(&mut self) -> Said {
        match self {
            Self::Here(output) => match output.next().await {
                Some(Ok(bytes)) => Said::Output(bytes),
                None => Said::Close(1000, "exited".to_owned()),
                Some(Err(error @ TerminalError::Detached(_))) => {
                    Said::Close(1013, error.to_string())
                }
                Some(Err(error)) => Said::Close(1011, error.to_string()),
            },
            Self::There(remote) => match remote.next().await {
                relay::Event::Output(bytes) => Said::Output(bytes),
                relay::Event::Refused(reason) => Said::Refused(reason),
                relay::Event::Closed(closed) => Said::Close(closed.code, closed.reason),
            },
        }
    }

    async fn input(&mut self, api: &Api, id: TerminalId, bytes: Bytes) -> Option<Said> {
        match self {
            Self::Here(_) => here(api.terminals.input(id, bytes).await),
            Self::There(remote) => there(remote.input(bytes).await),
        }
    }

    /// Applies a text frame from the UI. One that is not a control message is ignored.
    async fn control(&mut self, api: &Api, id: TerminalId, text: &str) -> Option<Said> {
        let Ok(Control::Resize { cols, rows }) = serde_json::from_str(text) else {
            return None;
        };
        match self {
            Self::Here(_) => here(api.terminals.resize(id, cols, rows).await),
            Self::There(remote) => there(remote.resize(cols, rows).await),
        }
    }
}

fn here(done: Result<(), TerminalError>) -> Option<Said> {
    match done {
        Ok(()) => None,
        Err(TerminalError::Refused { reason, .. }) => Some(Said::Refused(reason)),
        Err(error) => Some(Said::Close(1011, error.to_string())),
    }
}

fn there(done: Result<(), relay::Closed>) -> Option<Said> {
    done.err()
        .map(|closed| Said::Close(closed.code, closed.reason))
}

/// Sends the snapshot, then carries output to the UI and input to the terminal until either
/// ends.
async fn carry(api: Api, id: TerminalId, snapshot: Snapshot, mut end: End, mut socket: WebSocket) {
    if socket
        .send(ws::Message::Binary(snapshot.bytes))
        .await
        .is_err()
    {
        return;
    }
    loop {
        let said = tokio::select! {
            said = end.next() => said,
            incoming = socket.recv() => {
                let said = match incoming {
                    Some(Ok(ws::Message::Binary(bytes))) => end.input(&api, id, bytes).await,
                    Some(Ok(ws::Message::Text(text))) => end.control(&api, id, text.as_str()).await,
                    Some(Ok(ws::Message::Close(_)) | Err(_)) | None => return,
                    Some(Ok(_)) => None,
                };
                let Some(said) = said else { continue };
                said
            }
        };
        let sent = match said {
            Said::Output(bytes) => socket.send(ws::Message::Binary(bytes)).await,
            Said::Refused(reason) => {
                let text = serde_json::json!({"type": "error", "error": redact(&reason)});
                socket
                    .send(ws::Message::Text(text.to_string().into()))
                    .await
            }
            Said::Close(code, reason) => return close(socket, code, reason).await,
        };
        if sent.is_err() {
            return;
        }
    }
}

async fn close(mut socket: WebSocket, code: u16, reason: String) {
    let frame = CloseFrame {
        code,
        reason: close_reason(&reason).into(),
    };
    let _ = socket.send(ws::Message::Close(Some(frame))).await;
}

/// `reason` as a close frame carries it: redacted first, so a cut never leaves part of a
/// password with no `@` to find it by, then cut to [`REASON_AT_MOST`] bytes on a character
/// boundary.
pub(super) fn close_reason(reason: &str) -> String {
    let mut reason = redact(reason);
    let mut end = reason.len().min(REASON_AT_MOST);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason.truncate(end);
    reason
}

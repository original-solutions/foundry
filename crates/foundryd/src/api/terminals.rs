//! The terminal WebSocket, `GET /api/terminals/{id}`, for terminals on this machine. Terminals
//! on peers come with plan task 16.
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
//! terminal that is not live here is `404` before the upgrade.

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{self, CloseFrame, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::Response;
use foundry_proto::ids::TerminalId;
use futures::StreamExt as _;
use serde::Deserialize;

use super::{Api, ApiError, parse_id, redact};
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
    let (snapshot, output) = api.terminals.attach(id).await?;
    Ok(upgrade.on_upgrade(move |socket| relay(api, id, snapshot, output, socket)))
}

/// Sends the snapshot, then carries output to the UI and input to the terminal until either
/// ends.
async fn relay(
    api: Api,
    id: TerminalId,
    snapshot: Snapshot,
    mut output: Output,
    mut socket: WebSocket,
) {
    if socket
        .send(ws::Message::Binary(snapshot.bytes))
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            item = output.next() => {
                let bytes = match item {
                    Some(Ok(bytes)) => bytes,
                    None => return close(socket, 1000, "exited".to_owned()).await,
                    Some(Err(error @ TerminalError::Detached(_))) => {
                        return close(socket, 1013, error.to_string()).await;
                    }
                    Some(Err(error)) => return close(socket, 1011, error.to_string()).await,
                };
                if socket.send(ws::Message::Binary(bytes)).await.is_err() {
                    return;
                }
            }
            incoming = socket.recv() => {
                let done = match incoming {
                    Some(Ok(ws::Message::Binary(bytes))) => api.terminals.input(id, bytes).await,
                    Some(Ok(ws::Message::Text(text))) => control(&api, id, text.as_str()).await,
                    Some(Ok(ws::Message::Close(_)) | Err(_)) | None => return,
                    Some(Ok(_)) => Ok(()),
                };
                match done {
                    Ok(()) => {}
                    Err(TerminalError::Refused { reason, .. }) => {
                        let text = serde_json::json!({"type": "error", "error": redact(&reason)});
                        if socket.send(ws::Message::Text(text.to_string().into())).await.is_err() {
                            return;
                        }
                    }
                    Err(error) => return close(socket, 1011, error.to_string()).await,
                }
            }
        }
    }
}

/// Applies a text frame from the UI. One that is not a control message is ignored.
async fn control(api: &Api, id: TerminalId, text: &str) -> Result<(), TerminalError> {
    match serde_json::from_str(text) {
        Ok(Control::Resize { cols, rows }) => api.terminals.resize(id, cols, rows).await,
        Err(_) => Ok(()),
    }
}

async fn close(mut socket: WebSocket, code: u16, reason: String) {
    let mut reason = redact(&reason);
    let mut end = reason.len().min(REASON_AT_MOST);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason.truncate(end);
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    let _ = socket.send(ws::Message::Close(Some(frame))).await;
}

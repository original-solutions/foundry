//! `POST /hooks/{session}`: where Claude Code's hooks, and the fake agent's, post their events.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use foundry_proto::ids::SessionId;

use super::state::{Event, Hook};
use super::{Inner, RETRY_AFTER, RETRY_AT_MOST};

/// How long a hook whose record cannot be written is tried again for, with a growing wait.
/// Inside the 5 seconds the hook command waits, so curl still gets the answer.
const KEEP_TRYING_FOR: Duration = Duration::from_secs(3);

/// Takes one hook call. Only loopback callers, and only with the session's own token, change
/// anything: a caller from another address, an unknown session and a wrong token all get 403,
/// and none of them is told which it was. Warnings name the session or the peer address, never
/// a token. A hook the store does not take is tried again for [`KEEP_TRYING_FOR`], then is 500.
pub(super) async fn hook(
    State(inner): State<Arc<Inner>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(session): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    if !peer.ip().to_canonical().is_loopback() {
        tracing::warn!(peer = %peer, "hook refused: not from this machine");
        return StatusCode::FORBIDDEN;
    }
    let Some(live) = session
        .parse::<uuid::Uuid>()
        .ok()
        .and_then(|id| inner.live(SessionId(id)))
    else {
        tracing::warn!(session = %session, "hook refused: no such session or wrong token");
        return StatusCode::FORBIDDEN;
    };
    let wanted = format!("Bearer {}", live.token);
    let presented = headers.get(AUTHORIZATION).map_or(&[][..], |v| v.as_bytes());
    if !ct_eq(presented, wanted.as_bytes()) {
        tracing::warn!(session = %session, "hook refused: no such session or wrong token");
        return StatusCode::FORBIDDEN;
    }
    let hook = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|payload| Hook::from_payload(&payload));
    let Some(hook) = hook else {
        return StatusCode::BAD_REQUEST;
    };
    let give_up_at = tokio::time::Instant::now() + KEEP_TRYING_FOR;
    let mut backoff = RETRY_AFTER;
    loop {
        match inner.event(&live, Event::Hook(hook)).await {
            Ok(_) => return StatusCode::NO_CONTENT,
            Err(error) if tokio::time::Instant::now() + backoff < give_up_at => {
                tracing::warn!(session = %session, %error, "hook not recorded; trying again");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RETRY_AT_MOST);
            }
            Err(error) => {
                tracing::error!(session = %session, %error, "hook not recorded");
                return StatusCode::INTERNAL_SERVER_ERROR;
            }
        }
    }
}

/// Whether `a` and `b` are equal bytes, taking the same time for any two of one length.
pub(super) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_is_true_only_for_equal_bytes() {
        assert!(ct_eq(b"", b""));
        assert!(ct_eq(b"Bearer abc", b"Bearer abc"));
        assert!(!ct_eq(b"Bearer abc", b"Bearer abd"));
        assert!(!ct_eq(b"Bearer abc", b"Bearer ab"));
        assert!(!ct_eq(b"Bearer ab", b"Bearer abc"));
        assert!(!ct_eq(b"a", b""));
    }
}

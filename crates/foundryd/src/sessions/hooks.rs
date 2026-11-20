//! `POST /hooks/{session}`: where Claude Code's hooks, and the fake agent's, post their events.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use foundry_proto::ids::SessionId;

use super::state::{Event, Hook};
use super::{Backoff, Inner, Live, lock};

/// How long a hook whose record cannot be written is tried again for, with a growing wait. No
/// try starts after it, so the answer usually reaches the hook command within the 5 seconds it
/// waits; a store that is slow to answer can make it later.
const KEEP_TRYING_FOR: Duration = Duration::from_secs(3);

/// Takes one hook call. Only loopback callers, and only with the session's own token, change
/// anything: a caller from another address, an unknown session and a wrong token all get 403,
/// and none of them is told which it was. Warnings name the session or the peer address, never
/// a token. A hook the store does not take is tried again for [`KEEP_TRYING_FOR`], then is 500.
/// The hook is applied in a task of its own: a caller that stops waiting drops this request,
/// and that must not stop a write between the record and the state kept in memory.
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
    let payload = serde_json::from_slice::<serde_json::Value>(&body).ok();
    let Some((payload, hook)) = payload.and_then(|p| Hook::from_payload(&p).map(|hook| (p, hook)))
    else {
        return StatusCode::BAD_REQUEST;
    };
    note(&inner, &live, &payload);
    tokio::spawn(async move { apply(&inner, &live, hook).await })
        .await
        .unwrap_or_else(|error| {
            tracing::error!(session = %session, %error, "hook not recorded");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

/// Takes `payload` into the session's brief, with the gateway key and every live session's
/// token hidden in what it keeps.
fn note(inner: &Inner, live: &Live, payload: &serde_json::Value) {
    let tokens: Vec<String> = lock(&inner.live)
        .values()
        .map(|l| l.token.clone())
        .collect();
    let mut secrets: Vec<&str> = tokens.iter().map(String::as_str).collect();
    secrets.push(&inner.config.gateway.key);
    lock(&live.brief).note(payload, &secrets);
}

/// Applies `hook` to the session, trying again for [`KEEP_TRYING_FOR`] while the store does not
/// take it, and gives the answer for the hook command.
async fn apply(inner: &Inner, live: &Arc<Live>, hook: Hook) -> StatusCode {
    let give_up_at = tokio::time::Instant::now() + KEEP_TRYING_FOR;
    let mut backoff = Backoff::new();
    loop {
        let error = match inner.event(live, Event::Hook(hook)).await {
            Ok(_) => return StatusCode::NO_CONTENT,
            Err(error) => error,
        };
        let wait = backoff.next_wait();
        if tokio::time::Instant::now() + wait >= give_up_at {
            tracing::error!(session = %live.id, %error, "hook not recorded");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
        if backoff.first_failure() {
            tracing::warn!(session = %live.id, %error, "hook not recorded; trying again");
        }
        tokio::time::sleep(wait).await;
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

//! The layer that carries a call to the machine that owns what it acts on, before any route
//! runs, so no handler knows about other machines.
//!
//! - `POST /api/workspaces` whose `machine` names another machine.
//! - `POST /api/workspaces/{id}/sessions`, `/shells` and `/archive` for a workspace another
//!   machine owns.
//! - `POST /api/terminals/{id}/close` for a terminal another machine owns.
//!
//! Each goes to its owner over [`Peers::call`](crate::sync::Peers::call), its body passed as
//! bytes, and the owner's status and body come back unchanged. A call for a machine not in the
//! machine list is `404`; one for a machine that is offline, whose link is down, or that runs
//! an older Foundry is `503` and names it. A request marked [`CARRIED`] arrived from another
//! machine: it runs here, where a handler refuses anything this machine does not own, and is
//! never carried on. Every other request, and one that names nothing elsewhere, runs here.

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use foundry_proto::ids::{MachineId, TerminalId, WorkspaceId};
use serde::Deserialize;

use super::{Api, ApiError, error, on_store, redact};
use crate::sync::call::{Answer, CARRIED, CallError};

/// The most bytes of a body carried, as axum's own default body limit.
const BODY_LIMIT: usize = 2 * 1024 * 1024;

/// What a carried route acts on.
enum Target {
    Create,
    Workspace(WorkspaceId),
    Terminal(TerminalId),
}

/// The target of a `POST` to `path`, when it is a route this layer carries.
fn target(path: &str) -> Option<Target> {
    let parts: Vec<&str> = path.strip_prefix("/api/")?.split('/').collect();
    match parts.as_slice() {
        ["workspaces"] => Some(Target::Create),
        ["workspaces", id, "sessions" | "shells" | "archive"] => {
            Some(Target::Workspace(WorkspaceId(id.parse().ok()?)))
        }
        ["terminals", id, "close"] => Some(Target::Terminal(TerminalId(id.parse().ok()?))),
        _ => None,
    }
}

pub(super) async fn carry(State(api): State<Api>, request: Request, next: Next) -> Response {
    if request.method() != Method::POST || request.headers().contains_key(CARRIED) {
        return next.run(request).await;
    }
    let Some(target) = target(request.uri().path()) else {
        return next.run(request).await;
    };
    let (parts, body) = request.into_parts();
    let Ok(body) = axum::body::to_bytes(body, BODY_LIMIT).await else {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "the body could not be read whole".to_owned(),
        );
    };
    let owner = match owner(&api, target, &body).await {
        Ok(Some(owner)) => owner,
        Ok(None) => return next.run(Request::from_parts(parts, Body::from(body))).await,
        Err(e) => return e.into_response(),
    };
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    match api
        .peers
        .call(owner, parts.uri.path(), content_type, &body)
        .await
    {
        Ok(answer) => answered(answer),
        Err(refused) => not_carried(&refused),
    }
}

/// The other machine `target` belongs to, or `None` when it runs here.
async fn owner(api: &Api, target: Target, body: &Bytes) -> Result<Option<MachineId>, ApiError> {
    let me = api.peers.me();
    match target {
        Target::Create => {
            /// Only the field that picks the machine; the route reads the rest.
            #[derive(Deserialize)]
            struct Picked {
                #[serde(default)]
                machine: Option<MachineId>,
            }
            // A body that does not read runs here, where the route refuses it with its reason.
            Ok(serde_json::from_slice::<Picked>(body)
                .ok()
                .and_then(|picked| picked.machine)
                .filter(|machine| *machine != me))
        }
        Target::Workspace(id) => Ok(on_store(&api.store, move |store| store.workspace(id))
            .await?
            .filter(|record| !record.deleted && record.owner != me)
            .map(|record| record.owner)),
        Target::Terminal(id) => Ok(api.peers.owner_of(id).await?),
    }
}

/// The owner's answer, as it gave it.
fn answered(answer: Answer) -> Response {
    let Ok(status) = StatusCode::from_u16(answer.status) else {
        return error(
            StatusCode::BAD_GATEWAY,
            format!("the owner answered with status {}", answer.status),
        );
    };
    if answer.body.is_empty() {
        return status.into_response();
    }
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        answer.body,
    )
        .into_response()
}

/// The answer to a call that its owner did not answer.
fn not_carried(refused: &CallError) -> Response {
    let status = match refused {
        CallError::NotListed(_) => StatusCode::NOT_FOUND,
        CallError::Offline(_) | CallError::Older(_) | CallError::Down(_) | CallError::Lost(_) => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        CallError::Refused(_) => StatusCode::BAD_GATEWAY,
    };
    let message = redact(&refused.to_string());
    tracing::info!("api: a call for another machine was not carried: {message}");
    error(status, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_carried_routes_have_a_target() {
        let id = uuid::Uuid::new_v4();
        assert!(matches!(target("/api/workspaces"), Some(Target::Create)));
        for action in ["sessions", "shells", "archive"] {
            let path = format!("/api/workspaces/{id}/{action}");
            assert!(matches!(target(&path), Some(Target::Workspace(w)) if w.0 == id));
        }
        let path = format!("/api/terminals/{id}/close");
        assert!(matches!(target(&path), Some(Target::Terminal(t)) if t.0 == id));
        for other in [
            "/api/state".to_owned(),
            format!("/api/workspaces/{id}/changes"),
            format!("/api/workspaces/{id}"),
            "/api/workspaces/not-an-id/shells".to_owned(),
            format!("/api/terminals/{id}"),
            format!("/elsewhere/workspaces/{id}/shells"),
        ] {
            assert!(target(&other).is_none(), "{other}");
        }
    }
}

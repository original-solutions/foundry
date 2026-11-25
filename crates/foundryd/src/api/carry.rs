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
//! an older Foundry is `503` and names it, and was never sent. One whose link went down after it
//! was sent may have run there, and is `504`. A request marked [`CARRIED`] arrived from another
//! machine: it runs here, where a handler refuses anything this machine does not own, and is
//! never carried on. Every other request, and one that names nothing elsewhere, runs here.
//!
//! The factory's worker routes ([`super::factory`]) are carried too, but never by this layer: a
//! request to one runs here, where it is `404` unless marked. A run carries its own call with
//! [`super::factory::call`], which finds the owner as this layer does: the machine a factory
//! create names, and the owner of the workspace, the terminal or, through its session record,
//! the session the path names.
//!
//! A create carried to a machine over its limits is still made there, and its `201` gets the
//! `warning` this machine gives of it, as a create made here does.

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use foundry_proto::ids::{MachineId, SessionId, TerminalId, WorkspaceId};
use serde::Deserialize;

use super::{Api, ApiError, error, on_store, redact};
use crate::store::StoreError;
use crate::sync::call::{Answer, CARRIED, CallError};

/// The most bytes of a body carried, as axum's own default body limit.
const BODY_LIMIT: usize = 2 * 1024 * 1024;

/// What a carried route acts on.
enum Target {
    Create,
    Workspace(WorkspaceId),
    Terminal(TerminalId),
    /// A factory create, for the machine it names.
    FactoryCreate,
    /// A factory session start or end in a workspace.
    FactoryWorkspace(WorkspaceId),
    /// A message to a factory session, or a read of its turn.
    Session(SessionId),
    /// A read of a terminal's tail.
    Tail(TerminalId),
}

impl Target {
    /// Whether it is a factory worker route, which only a run carries.
    fn is_worker(&self) -> bool {
        matches!(
            self,
            Target::FactoryCreate
                | Target::FactoryWorkspace(_)
                | Target::Session(_)
                | Target::Tail(_)
        )
    }
}

/// Whether a `POST` to `path` is a route a carried call may name: one this layer carries, or a
/// worker route a run carries.
pub(crate) fn carries(path: &str) -> bool {
    target(path).is_some()
}

/// The target of a `POST` to `path`, when it is a route a carried call may name.
fn target(path: &str) -> Option<Target> {
    let parts: Vec<&str> = path.strip_prefix("/api/")?.split('/').collect();
    match parts.as_slice() {
        ["workspaces"] => Some(Target::Create),
        ["workspaces", id, "sessions" | "shells" | "archive"] => {
            Some(Target::Workspace(WorkspaceId(id.parse().ok()?)))
        }
        ["terminals", id, "close"] => Some(Target::Terminal(TerminalId(id.parse().ok()?))),
        ["factory", "workspaces"] => Some(Target::FactoryCreate),
        ["workspaces", id, "factory", "session" | "end"] => {
            Some(Target::FactoryWorkspace(WorkspaceId(id.parse().ok()?)))
        }
        ["sessions", id, "factory", "message" | "turn"] => {
            Some(Target::Session(SessionId(id.parse().ok()?)))
        }
        ["terminals", id, "tail"] => Some(Target::Tail(TerminalId(id.parse().ok()?))),
        _ => None,
    }
}

pub(super) async fn carry(State(api): State<Api>, request: Request, next: Next) -> Response {
    if request.method() != Method::POST || request.headers().contains_key(CARRIED) {
        return next.run(request).await;
    }
    let Some(target) = target(request.uri().path()).filter(|target| !target.is_worker()) else {
        return next.run(request).await;
    };
    let create = matches!(target, Target::Create);
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
        Err(e) => return ApiError::from(e).into_response(),
    };
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let warning = if create {
        match api.placements.warning(&api, owner).await {
            Ok(warning) => warning,
            Err(e) => return ApiError::from(e).into_response(),
        }
    } else {
        None
    };
    match api
        .peers
        .call(owner, parts.uri.path(), content_type, &body)
        .await
    {
        Ok(answer) => answered(warned(answer, warning)),
        Err(refused) => not_carried(&refused),
    }
}

/// `answer` with `warning` beside its fields, when it is a `201` with a JSON object.
fn warned(answer: Answer, warning: Option<String>) -> Answer {
    let Some(warning) = warning.filter(|_| answer.status == 201) else {
        return answer;
    };
    let Ok(serde_json::Value::Object(mut fields)) = serde_json::from_slice(&answer.body) else {
        return answer;
    };
    fields.insert("warning".to_owned(), warning.into());
    Answer {
        status: answer.status,
        body: serde_json::Value::Object(fields).to_string().into(),
    }
}

/// Whether a `POST` to `path` is a factory worker route.
pub(super) fn is_worker(path: &str) -> bool {
    target(path).is_some_and(|target| target.is_worker())
}

/// The other machine that owns what the worker route `path` names, with `body`, or `None` when
/// it runs here. A path that is no worker route is `None`.
pub(super) async fn worker_owner(
    api: &Api,
    path: &str,
    body: &Bytes,
) -> Result<Option<MachineId>, StoreError> {
    match target(path).filter(Target::is_worker) {
        Some(target) => owner(api, target, body).await,
        None => Ok(None),
    }
}

/// The other machine `target` belongs to, or `None` when it runs here.
async fn owner(api: &Api, target: Target, body: &Bytes) -> Result<Option<MachineId>, StoreError> {
    let me = api.peers.me();
    match target {
        Target::Create | Target::FactoryCreate => {
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
        Target::Workspace(id) | Target::FactoryWorkspace(id) => {
            Ok(on_store(&api.store, move |store| store.workspace(id))
                .await?
                .filter(|record| !record.deleted && record.owner != me)
                .map(|record| record.owner))
        }
        // The store reads sessions by owner only, so each other owner's are looked through.
        Target::Session(id) => Ok(on_store(&api.store, move |store| {
            for owner in store.versions()?.into_keys().filter(|owner| *owner != me) {
                if store.sessions_of(owner)?.iter().any(|s| s.body.id == id) {
                    return Ok(Some(owner));
                }
            }
            Ok(None)
        })
        .await?),
        Target::Terminal(id) | Target::Tail(id) => Ok(api.peers.owner_of(id).await?),
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
pub(super) fn not_carried(refused: &CallError) -> Response {
    let status = match refused {
        CallError::NotListed(_) => StatusCode::NOT_FOUND,
        CallError::Offline(_) | CallError::Older(_) | CallError::Down(_) => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        // Sent, so it may have run: not the 503 of a call that changed nothing.
        CallError::Lost(_) => StatusCode::GATEWAY_TIMEOUT,
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
    fn only_a_call_that_may_have_run_is_504_and_the_refusals_before_any_link_are_404_or_503() {
        let id = MachineId(uuid::Uuid::new_v4());
        let name = || "peer".to_owned();
        let cases = [
            (CallError::NotListed(id), 404),
            (CallError::Offline(name()), 503),
            (CallError::Older(name()), 503),
            (CallError::Down(name()), 503),
            (CallError::Lost(name()), 504),
            (CallError::Refused("no".into()), 502),
        ];
        for (refused, status) in cases {
            assert_eq!(not_carried(&refused).status().as_u16(), status, "{refused}");
        }
        let lost = CallError::Lost(name()).to_string();
        assert!(lost.contains("may have run"), "{lost}");
    }

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
        assert!(matches!(
            target("/api/factory/workspaces"),
            Some(Target::FactoryCreate)
        ));
        for action in ["session", "end"] {
            let path = format!("/api/workspaces/{id}/factory/{action}");
            assert!(matches!(target(&path), Some(Target::FactoryWorkspace(w)) if w.0 == id));
        }
        for action in ["message", "turn"] {
            let path = format!("/api/sessions/{id}/factory/{action}");
            assert!(matches!(target(&path), Some(Target::Session(s)) if s.0 == id));
        }
        let path = format!("/api/terminals/{id}/tail");
        assert!(matches!(target(&path), Some(Target::Tail(t)) if t.0 == id));
        for other in [
            "/api/state".to_owned(),
            format!("/api/workspaces/{id}/changes"),
            format!("/api/workspaces/{id}"),
            "/api/workspaces/not-an-id/shells".to_owned(),
            format!("/api/terminals/{id}"),
            format!("/elsewhere/workspaces/{id}/shells"),
            "/api/factory".to_owned(),
            format!("/api/factory/runs/{id}/stop"),
            format!("/api/workspaces/{id}/factory"),
            format!("/api/workspaces/{id}/factory/archive"),
            format!("/api/sessions/{id}/trail"),
            format!("/api/sessions/{id}/factory"),
            "/api/sessions/not-an-id/factory/turn".to_owned(),
            format!("/api/terminals/{id}/tail/more"),
        ] {
            assert!(target(&other).is_none(), "{other}");
        }
    }

    #[test]
    fn the_worker_routes_are_carried_only_by_a_run_and_p1_routes_by_this_layer() {
        let id = uuid::Uuid::new_v4();
        let workers = [
            "/api/factory/workspaces".to_owned(),
            format!("/api/workspaces/{id}/factory/session"),
            format!("/api/workspaces/{id}/factory/end"),
            format!("/api/sessions/{id}/factory/message"),
            format!("/api/sessions/{id}/factory/turn"),
            format!("/api/terminals/{id}/tail"),
        ];
        for path in &workers {
            assert!(carries(path), "{path} is taken from a peer");
            assert!(target(path).unwrap().is_worker(), "{path}");
        }
        for path in [
            "/api/workspaces".to_owned(),
            format!("/api/workspaces/{id}/sessions"),
            format!("/api/workspaces/{id}/shells"),
            format!("/api/workspaces/{id}/archive"),
            format!("/api/terminals/{id}/close"),
        ] {
            assert!(!target(&path).unwrap().is_worker(), "{path}");
        }
    }
}

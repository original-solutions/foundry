//! The layer that carries a call to the machine that owns what it acts on, before any route
//! runs, so no handler knows about other machines.
//!
//! - `POST /api/workspaces` whose `machine` names another machine.
//! - `POST /api/workspaces/{id}/sessions`, `/shells` and `/archive` for a workspace another
//!   machine owns.
//! - `POST /api/terminals/{id}/close` for a terminal another machine owns.
//! - `POST /api/sessions/{id}/rename` for a session another machine owns.
//!
//! Each goes to its owner over [`Peers::call`](crate::sync::Peers::call), its body passed as
//! bytes, and the owner's status and body come back unchanged. A call for a machine not in the
//! machine list is `404`; one for a machine that is offline, whose link is down, or that runs
//! an older Foundry is `503` and names it, and was never sent. One whose link went down after it
//! was sent may have run there, and is `504`. A request marked [`CARRIED`] arrived from another
//! machine: it runs here, where a handler refuses anything this machine does not own, and is
//! never carried on. Every other request, and one that names nothing elsewhere, runs here.
//!
//! Two Factory view routes are taken from a peer too, and carried by their own handlers
//! ([`super::factory::view_router`]), never by this layer: `POST /api/factory/runs/{id}/stop`,
//! to the machine believed to hold the run's lease, and `POST /api/projects/{id}/factory`, to
//! the acting coordinator when it must fill in its default login.
//!
//! The factory's worker routes ([`super::factory`]) are carried too, but never by this layer: a
//! request to one runs here, where it is `404` unless marked. A run carries its own call with
//! [`super::factory::call`], which finds the owner with [`worker_owner`]: the machine a factory
//! create names, and the owner of the workspace, the terminal or, through its session record,
//! the session the path names. This file is the one place a worker route's path is read
//! ([`worker`]) and listed ([`WORKER_ROUTES`]).
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
    Session(SessionId),
    /// A factory worker route, which only a run carries.
    Worker(Worker),
    /// A Factory view route whose handler carries it: a stop, to the lease holder, and a
    /// project's factory edit, to the acting coordinator when it must fill in its default
    /// login.
    View,
}

/// A factory worker route, with what its path names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Worker {
    /// A factory create, for the machine its body names.
    Create,
    /// A factory session start in a workspace.
    Start(WorkspaceId),
    /// The end of a workspace's factory sessions.
    End(WorkspaceId),
    /// A message to a factory session.
    Message(SessionId),
    /// A read of a factory session's turn.
    Turn(SessionId),
    /// A read of a terminal's tail.
    Tail(TerminalId),
}

/// The worker routes' paths, as the router mounts them. [`target`] reads each of them as its
/// own [`Worker`].
pub(super) const WORKER_ROUTES: [&str; 6] = [
    "/api/factory/workspaces",
    "/api/workspaces/{id}/factory/session",
    "/api/workspaces/{id}/factory/end",
    "/api/sessions/{id}/factory/message",
    "/api/sessions/{id}/factory/turn",
    "/api/terminals/{id}/tail",
];

/// Whether a `POST` to `path` is a route a carried call may name: one this layer carries, a
/// worker route a run carries, or a Factory view route its own handler carries.
pub(crate) fn carries(path: &str) -> bool {
    target(path).is_some()
}

/// The worker route a `POST` to `path` is, when it is one.
pub(super) fn worker(path: &str) -> Option<Worker> {
    match target(path)? {
        Target::Worker(worker) => Some(worker),
        _ => None,
    }
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
        ["sessions", id, "rename"] => Some(Target::Session(SessionId(id.parse().ok()?))),
        ["factory", "workspaces"] => Some(Target::Worker(Worker::Create)),
        ["workspaces", id, "factory", "session"] => {
            Some(Target::Worker(Worker::Start(WorkspaceId(id.parse().ok()?))))
        }
        ["workspaces", id, "factory", "end"] => {
            Some(Target::Worker(Worker::End(WorkspaceId(id.parse().ok()?))))
        }
        ["sessions", id, "factory", "message"] => {
            Some(Target::Worker(Worker::Message(SessionId(id.parse().ok()?))))
        }
        ["sessions", id, "factory", "turn"] => {
            Some(Target::Worker(Worker::Turn(SessionId(id.parse().ok()?))))
        }
        ["terminals", id, "tail"] => {
            Some(Target::Worker(Worker::Tail(TerminalId(id.parse().ok()?))))
        }
        ["factory", "runs", run, "stop"] => {
            run.parse::<foundry_proto::factory::RunId>().ok()?;
            Some(Target::View)
        }
        ["projects", id, "factory"] => {
            id.parse::<uuid::Uuid>().ok()?;
            Some(Target::View)
        }
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
    if matches!(target, Target::Worker(_) | Target::View) {
        return next.run(request).await;
    }
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

/// Where a run's call to a worker route is answered.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Owner {
    /// On this machine: it owns what the call names, or the workspace or terminal it names is
    /// gone, which this machine answers as well as any.
    Here,
    /// On the other machine that owns it.
    There(MachineId),
    /// Nowhere yet: no record of what the call names has reached this machine, so its owner is
    /// not known. A deleted session record names no owner either.
    Unknown,
}

impl Owner {
    /// Where a call for the record `found`, its owner and whether it is deleted, is answered.
    fn of(found: Option<(MachineId, bool)>, me: MachineId) -> Owner {
        match found {
            None => Owner::Unknown,
            Some((owner, deleted)) if owner == me || deleted => Owner::Here,
            Some((owner, _)) => Owner::There(owner),
        }
    }
}

/// Where a run's call to `worker`, with `body`, is answered: the machine a create names, or
/// the owner of the record of the workspace, the session or the terminal it names.
pub(super) async fn worker_owner(
    api: &Api,
    worker: Worker,
    body: &Bytes,
) -> Result<Owner, StoreError> {
    let me = api.peers.me();
    let found = match worker {
        Worker::Create => return Ok(named(body, me).map_or(Owner::Here, Owner::There)),
        Worker::Start(id) | Worker::End(id) => {
            on_store(&api.store, move |store| store.workspace(id))
                .await?
                .map(|record| (record.owner, record.deleted))
        }
        // A deleted session record names no owner, as it never did: the call is refused until
        // a live record names one, and is sent nowhere.
        Worker::Message(id) | Worker::Turn(id) => {
            on_store(&api.store, move |store| store.session(id))
                .await?
                .filter(|record| !record.deleted)
                .map(|record| (record.owner, false))
        }
        Worker::Tail(id) => on_store(&api.store, move |store| store.terminal(id))
            .await?
            .map(|record| (record.owner, record.deleted)),
    };
    Ok(Owner::of(found, me))
}

/// The other machine a create's `body` names, or `None` when it names this one or none. A body
/// that does not read names none, and runs here, where the route refuses it with its reason.
fn named(body: &Bytes, me: MachineId) -> Option<MachineId> {
    /// Only the field that picks the machine; the route reads the rest.
    #[derive(Deserialize)]
    struct Picked {
        #[serde(default)]
        machine: Option<MachineId>,
    }
    serde_json::from_slice::<Picked>(body)
        .ok()
        .and_then(|picked| picked.machine)
        .filter(|machine| *machine != me)
}

/// The other machine `target` belongs to, or `None` when it runs here.
async fn owner(api: &Api, target: Target, body: &Bytes) -> Result<Option<MachineId>, StoreError> {
    let me = api.peers.me();
    match target {
        Target::Create => Ok(named(body, me)),
        Target::Workspace(id) => Ok(on_store(&api.store, move |store| store.workspace(id))
            .await?
            .filter(|record| !record.deleted && record.owner != me)
            .map(|record| record.owner)),
        Target::Terminal(id) => Ok(api.peers.owner_of(id).await?),
        Target::Session(id) => Ok(on_store(&api.store, move |store| store.session(id))
            .await?
            .filter(|record| !record.deleted && record.owner != me)
            .map(|record| record.owner)),
        // Never carried by this layer: a run's call finds its owner with `worker_owner`, and
        // a Factory view route's handler carries it itself.
        Target::Worker(_) | Target::View => Ok(None),
    }
}

/// The owner's answer, as it gave it.
pub(super) fn answered(answer: Answer) -> Response {
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
        let path = format!("/api/sessions/{id}/rename");
        assert!(matches!(target(&path), Some(Target::Session(s)) if s.0 == id));
        let at = |path: String| worker(&path);
        assert_eq!(worker("/api/factory/workspaces"), Some(Worker::Create));
        let (w, s, t) = (WorkspaceId(id), SessionId(id), TerminalId(id));
        let session = at(format!("/api/workspaces/{id}/factory/session"));
        assert_eq!(session, Some(Worker::Start(w)));
        let end = at(format!("/api/workspaces/{id}/factory/end"));
        assert_eq!(end, Some(Worker::End(w)));
        let message = at(format!("/api/sessions/{id}/factory/message"));
        assert_eq!(message, Some(Worker::Message(s)));
        let turn = at(format!("/api/sessions/{id}/factory/turn"));
        assert_eq!(turn, Some(Worker::Turn(s)));
        assert_eq!(
            at(format!("/api/terminals/{id}/tail")),
            Some(Worker::Tail(t))
        );
        for other in [
            "/api/state".to_owned(),
            format!("/api/workspaces/{id}/changes"),
            format!("/api/workspaces/{id}"),
            "/api/workspaces/not-an-id/shells".to_owned(),
            format!("/api/terminals/{id}"),
            format!("/elsewhere/workspaces/{id}/shells"),
            "/api/factory".to_owned(),
            format!("/api/factory/runs/{id}"),
            format!("/api/workspaces/{id}/factory"),
            format!("/api/workspaces/{id}/factory/archive"),
            format!("/api/sessions/{id}/trail"),
            format!("/api/sessions/{id}/factory"),
            "/api/sessions/not-an-id/factory/turn".to_owned(),
            "/api/sessions/not-an-id/rename".to_owned(),
            format!("/api/sessions/{id}/rename/more"),
            format!("/api/terminals/{id}/tail/more"),
        ] {
            assert!(target(&other).is_none(), "{other}");
        }
    }

    #[test]
    fn each_mounted_worker_route_is_read_as_its_own_worker_and_carried_only_by_a_run() {
        let id = uuid::Uuid::new_v4();
        let mut read = Vec::new();
        for route in WORKER_ROUTES {
            let path = route.replace("{id}", &id.to_string());
            assert!(carries(&path), "{path} is taken from a peer");
            let worker = worker(&path).unwrap_or_else(|| panic!("{route} is read"));
            assert!(!read.contains(&worker), "{route} is read as another route");
            read.push(worker);
        }
        for path in [
            "/api/workspaces".to_owned(),
            format!("/api/workspaces/{id}/sessions"),
            format!("/api/workspaces/{id}/shells"),
            format!("/api/workspaces/{id}/archive"),
            format!("/api/terminals/{id}/close"),
            format!("/api/sessions/{id}/rename"),
        ] {
            assert!(carries(&path), "{path}");
            assert!(worker(&path).is_none(), "{path}");
        }
    }

    #[test]
    fn the_factory_views_carried_routes_are_taken_from_a_peer_and_carried_by_their_handlers() {
        let project = uuid::Uuid::new_v4();
        for path in [
            "/api/factory/runs/rawphp__softfac__8__1/stop".to_owned(),
            format!("/api/projects/{project}/factory"),
        ] {
            assert!(carries(&path), "{path} is taken from a peer");
            assert!(matches!(target(&path), Some(Target::View)), "{path}");
            assert!(worker(&path).is_none(), "{path}");
        }
        for path in [
            "/api/factory/runs/not-a-run/stop".to_owned(),
            "/api/factory/runs/rawphp__softfac__8__1".to_owned(),
            "/api/projects/not-an-id/factory".to_owned(),
            format!("/api/projects/{project}/remove"),
            format!("/api/projects/{project}/factory/more"),
            "/api/factory/settings".to_owned(),
        ] {
            assert!(!carries(&path), "{path}");
        }
    }

    #[test]
    fn a_runs_call_is_answered_by_the_owner_of_a_live_record_and_here_for_its_own_or_a_gone_one() {
        let (me, other) = (
            MachineId(uuid::Uuid::new_v4()),
            MachineId(uuid::Uuid::new_v4()),
        );
        assert_eq!(Owner::of(None, me), Owner::Unknown);
        assert_eq!(Owner::of(Some((other, false)), me), Owner::There(other));
        assert_eq!(Owner::of(Some((other, true)), me), Owner::Here);
        assert_eq!(Owner::of(Some((me, false)), me), Owner::Here);
        assert_eq!(Owner::of(Some((me, true)), me), Owner::Here);
    }
}

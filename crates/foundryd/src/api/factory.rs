//! The factory's routes: the worker routes carried between machines, and the Factory view's.
//!
//! The worker routes are what a run asks of the machine that holds its workspace or session.
//! Each is a `POST` that answers only a call another machine carried here, marked
//! [`CARRIED`]. Any other request is `404` as for no route, before its body is read: one with no
//! mark, on any machine, and one a browser sent, which says so with an `Origin` or `Sec-Fetch-`
//! header that a page cannot leave out. The carry layer never carries these routes. A run makes
//! its call with [`call`], which sends it to the machine that owns what it names, or answers it
//! here, by the same code a carried call runs, when this machine owns it. A call for something
//! no record on the run's machine names yet is refused, and sent nowhere. A carried call for
//! something this machine does not own is `404`, except a message, which is `409` with the
//! reason `no_such_session`, a turn, which is `410` as for a session that no longer runs, and a
//! create naming another machine, which is `421`.
//!
//! - `POST /api/factory/workspaces` `{machine, project, name, checkout, setup, tag}`: makes a
//!   workspace on `machine` with
//!   [`Workspaces::create_with`](crate::workspaces::Workspaces::create_with), `201 {id}`. A
//!   second create with the same `tag` and `name` answers with the first workspace. A workspace
//!   made whose setup could not be started is still `201`, with an `error` beside the `id`.
//! - `POST /api/workspaces/{id}/factory/session` `{project, login, model, effort, message}`:
//!   starts a factory session in the workspace on `message`, `201 {id}`. While a factory session
//!   of the project runs in the workspace, it is `200 {id}` with that session, and nothing
//!   starts, so a start sent again after its answer was lost starts no second session. A login
//!   this machine holds no token for is `422`.
//! - `POST /api/workspaces/{id}/factory/end`: ends each factory session of the workspace,
//!   `204`. A forced archive is P1's carried archive route.
//! - `POST /api/sessions/{id}/factory/message` `{message, only_if_prompts}`: sends `message` to
//!   the session, `200 {prompts}` with its count of prompts before the send, or `409 {error,
//!   reason}` with why it was not sent, and `now`, the count, when it rose. A `failed` send may
//!   have typed its line, as a terminal error can come after the terminal took it: its answer
//!   has `prompts`, the count the send read before it typed, which a resend names as
//!   `only_if_prompts`, or `null` when it read none or did not finish, so no count can guard a
//!   resend.
//! - `POST /api/sessions/{id}/factory/turn` `{turn}`: `200 {message, question}`, the turn's
//!   final message and the question that waits, `404` while the turn is not written and the
//!   session runs, or `410` when it is not written and the session no longer runs here, so it
//!   never will be.
//! - `POST /api/terminals/{id}/tail`: the end of the terminal's output as plain text, at most
//!   [`TAIL_LIMIT`] bytes. Only a workspace's setup terminal, where a setup or a gate runs, has
//!   a tail. Any other is `404`: a session's, whose child holds a token, is read through its
//!   turns.
//!
//! A message is `{line, body}`: one line under [`LINE_LIMIT`] characters with no control
//! character, and a body of at most [`BODY_LIMIT`] bytes with no NUL, or `null`. Anything else is
//! `422`, and nothing is typed.
//!
//! No answer holds a token. A token is only in a session's child's environment, and only a
//! setup terminal, which is never a session's, has a tail. A turn's message is kept with the
//! known secrets hidden. A setup or gate terminal holds no token, and its tail, like every error,
//! has the passwords of URLs hidden by [`redact`] before it is cut.
//!
//! The Factory view's routes ([`view_router`]) are the UI's, on every machine, and each answer
//! is drawn from this machine's store, so it is the same wherever it is asked:
//!
//! - `GET /api/factory`: `{settings, projects: [{id, name, factory, writer, status}]}`, the
//!   fleet's factory settings (their defaults while unset), and for each project its factory
//!   setting (off while unset) and the status believed of it ([`status::believed`]) with the
//!   machine that wrote it, `null` when none is believed.
//! - `GET /api/factory/runs/{id}`: `{run, events}`, the run's events in order, by lease
//!   generation then the writer's factory version. A run with none is `404`.
//! - `POST /api/factory/runs/{id}/stop`: asks the lease holder to stop a building run. A machine
//!   whose [`Leases`] holds the run's project under a lease it still holds
//!   ([`Leased::holding`]) records it with [`stop::request`] at that lease's generation, `200
//!   {stopped, already, message}`, and the run goes stuck on the holder's next pass; a run that
//!   is not building is `409` with the runner's words, and one with no ledger `404`. Any other
//!   machine carries it to the machine whose believed status holds the lease, and answers as it
//!   answers. A stop carried here that finds no lease held, or asked where no holder is
//!   believed, is `409` with the reason, and nothing is written.
//! - `POST /api/factory/settings` with every field of the fleet's factory settings: sent
//!   through the coordinator as every shared setting is, `200 {settings}`. A field left out or
//!   one it does not have, a value out of range (`poll_seconds` from 10 to 60), or one Claude
//!   Code would not take as one word (`model`, `effort`), is `422`.
//! - `POST /api/projects/{id}/factory` `{on, paused, login}`: the project's factory setting,
//!   sent through the coordinator, `200 {factory}`. `login` may be left out, `null` or blank,
//!   which names none. Turning a project on is `422` unless its repo URL starts
//!   `https://github.com/` and names a repo: SSH and other hosts are out of scope. A login is
//!   letters, digits and `-`. Turning a project on with no login named takes the acting
//!   coordinator's default login at that moment (`foundryd github default`), and is `422` when
//!   it names none, `503` when its secret store cannot be read. Only that machine's secret
//!   store holds it, so such a request is carried there and decided there, against its copy as
//!   the request reads it; one carried to a machine that no longer acts is `503`, never carried
//!   on. A project that is on acts as its login, so an edit that keeps it on and names none is
//!   `422`, and changes nothing: a turn-on sent again after its answer was lost gets that `422`
//!   when the first was made, which `GET /api/factory` then shows. Any other edit goes through
//!   [`Coordinator::edit`], which sends it to whichever machine acts.
//!
//!   These rules are checked when the request is read, not when the edit is numbered, which
//!   [`Coordinator::edit`] may do a little later, once the coordinator has caught up: an edit
//!   numbered in between, such as a turn-on naming a login, is not seen. Checking them as the
//!   edit is numbered is the coordinator's to do.
//!
//! Both edits are values sent whole, as P1's edits are, so an edit never merges a field with a
//! copy that may be older: the last one numbered wins, as with any shared setting.
//!
//! Every answer is `503` when the secret store cannot be read, since its strings must have the
//! known secrets hidden. A write is refused so before it is made; one made before the store
//! stopped being readable is still `200`, with its strings given as unreadable.
//!
//! Every string these answers hold is passed through [`secrets::hide_known`] first, so a token
//! a writer left unhidden in a status or a run event is not shown.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use foundry_proto::factory::{FactorySettings, FactoryStatus, ProjectFactory, RunId};
use foundry_proto::ids::{MachineId, ProjectId, SessionId, TerminalId, WorkspaceId};
use foundry_proto::settings::{Edit, SharedSettings};
use foundry_proto::state::SessionState;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use super::carry::{self, Owner, Worker};
use super::{
    Api, ApiError, created, edit_status, error, no_route, on_store, parse_id, unprocessable,
};
use crate::coordinator::Coordinator;
use crate::factory::ledger::Ledger;
use crate::factory::messages::{BODY_LIMIT, LINE_LIMIT, Message};
use crate::factory::runs::RunError;
use crate::factory::runs::stop::{self, StopAnswer};
use crate::factory::worker::Guard;
use crate::factory::{Leased, Leases, github_repo, status};
use crate::git_auth::NoToken;
use crate::secrets::{self, Secrets, redact};
use crate::sessions::SessionError;
use crate::sessions::factory::{FactoryStart, SendRefused};
use crate::sessions::turns::TurnText;
use crate::store::{Store, StoreError};
use crate::sync::Peers;
use crate::sync::call::{Answer, CARRIED, CallError};
use crate::workspaces::{Checkout, FactoryTag, Setup, WorkspaceError};

/// The most bytes of a terminal's tail answered.
pub const TAIL_LIMIT: usize = 64 * 1024;

/// The factory's routes, merged into [`super::router`].
pub fn router() -> Router<Api> {
    Router::new().merge(worker())
}

/// The routes a run's calls take, [`carry::WORKER_ROUTES`], answered only when carried, each by
/// [`here`].
fn worker() -> Router<Api> {
    let mut router = Router::new();
    for path in carry::WORKER_ROUTES {
        router = router.route(path, post(serve).fallback(no_route));
    }
    router.route_layer(middleware::from_fn(only_carried))
}

/// A worker route's path whose id is no id is no route.
async fn serve(State(api): State<Api>, uri: Uri, body: Bytes) -> Response {
    match carry::worker(uri.path()) {
        Some(worker) => here(&api, worker, body).await,
        None => no_route().await,
    }
}

/// Answers the worker route `worker` with `body` on this machine: what a carried call gets, and
/// what [`call`] gives for this machine's own records.
async fn here(api: &Api, worker: Worker, body: Bytes) -> Response {
    let answered = match worker {
        Worker::Create => create(api, &body).await,
        Worker::Start(id) => start(api, id, &body).await,
        Worker::End(id) => end(api, id).await,
        Worker::Message(id) => message(api, id, &body).await,
        Worker::Turn(id) => turn(api, id, &body).await,
        Worker::Tail(id) => tail(api, id).await,
    };
    answered.unwrap_or_else(IntoResponse::into_response)
}

/// `body` as the JSON a route takes: `422` when it is JSON of another shape, `400` when it is
/// no JSON.
fn read<T: DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| {
        let status = if e.is_data() {
            StatusCode::UNPROCESSABLE_ENTITY
        } else {
            StatusCode::BAD_REQUEST
        };
        ApiError::Rejected(
            status,
            format!("the body is not what this route takes: {e}"),
        )
    })
}

/// Lets through only a request marked [`CARRIED`] that no browser sent; any other is answered
/// as no route. A carried call is made by the daemon's own HTTP client, which sends no `Origin`
/// and no `Sec-Fetch-` header; a browser sends them with every `POST`, and a page cannot remove
/// them.
async fn only_carried(request: Request, next: Next) -> Response {
    let headers = request.headers();
    let from_browser = headers.contains_key(header::ORIGIN)
        || headers
            .keys()
            .any(|name| name.as_str().starts_with("sec-fetch-"));
    if headers.contains_key(CARRIED) && !from_browser {
        next.run(request).await
    } else {
        no_route().await
    }
}

/// A run's call to the worker route `path`, with `body` as JSON when it has one, answered by the
/// machine that owns what the path names: the machine a create names, or the owner of the
/// workspace's, the session's or the terminal's record. It is carried there over
/// [`Peers::call`](crate::sync::Peers::call), or, when that is this machine, answered here as a
/// carried call is, so a run gets the same answers and the same refusals wherever its workspace
/// is. A record that is gone is answered here, as gone. A path that is no worker route, or that
/// names something no record here names yet, as a workspace made on another machine before its
/// record arrives, is refused, and sent nowhere: a run asks again later.
pub async fn call(api: &Api, path: &str, body: &[u8]) -> Result<Answer, CallError> {
    let Some(worker) = carry::worker(path) else {
        return Err(CallError::Refused(format!(
            "{path:?} is not a worker route"
        )));
    };
    let body = Bytes::copy_from_slice(body);
    let owner = carry::worker_owner(api, worker, &body).await.map_err(|e| {
        CallError::Refused(format!(
            "the owner could not be read: {}",
            redact(&e.to_string())
        ))
    })?;
    match owner {
        Owner::Here => {
            let response = here(api, worker, body).await;
            let status = response.status().as_u16();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap_or_default();
            Ok(Answer { status, body })
        }
        Owner::There(owner) => {
            let content_type = (!body.is_empty()).then_some("application/json");
            api.peers.call(owner, path, content_type, &body).await
        }
        Owner::Unknown => Err(CallError::Refused(format!(
            "the record of what {path} names has not reached this machine yet, so its owner \
             is not known; nothing was sent"
        ))),
    }
}

/// A message as a call carries it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sent {
    line: String,
    #[serde(default)]
    body: Option<String>,
}

impl Sent {
    /// The message, when its line is one line and its body fits.
    fn checked(self) -> Result<Message, ApiError> {
        let Sent { line, body } = self;
        if line.trim().is_empty()
            || line.chars().any(char::is_control)
            || line.chars().count() >= LINE_LIMIT
        {
            return Err(unprocessable(format!(
                "a message's line is one line of text under {LINE_LIMIT} characters, with no \
                 control character"
            )));
        }
        if body
            .as_ref()
            .is_some_and(|body| body.len() > BODY_LIMIT || body.contains('\0'))
        {
            return Err(unprocessable(format!(
                "a message's body is at most {BODY_LIMIT} bytes, with no NUL"
            )));
        }
        Ok(Message { line, body })
    }
}

// --- workspaces ---

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    /// The machine to make it on, which a call carried here names as this one.
    machine: MachineId,
    project: ProjectId,
    name: String,
    checkout: Checkout,
    setup: Setup,
    #[serde(default)]
    tag: Option<FactoryTag>,
}

async fn create(api: &Api, body: &Bytes) -> Result<Response, ApiError> {
    let create: Create = read(body)?;
    if create.machine != api.peers.me() {
        return Ok(error(
            StatusCode::MISDIRECTED_REQUEST,
            format!(
                "this call names machine {}, not this one, and is not carried on",
                create.machine
            ),
        ));
    }
    let made = api
        .workspaces
        .create_with(
            create.project,
            &create.name,
            create.checkout,
            create.setup,
            create.tag,
        )
        .await;
    match made {
        Ok(id) => Ok(created(id)),
        // The workspace exists, so the caller needs its id: a retry would answer with it too.
        Err(
            e @ (WorkspaceError::SetupNotStarted { workspace, .. }
            | WorkspaceError::SetupNotRecorded { workspace, .. }),
        ) => {
            let message = redact(&e.to_string());
            tracing::warn!("api: {message}");
            Ok((
                StatusCode::CREATED,
                Json(serde_json::json!({ "id": workspace.to_string(), "error": message })),
            )
                .into_response())
        }
        Err(e) => Err(e.into()),
    }
}

/// A word Claude Code takes as an option's value: not blank, no white space or control
/// character, and not starting with `-`, so it is never read as an option of its own.
fn one_word(what: &str, text: &str) -> Result<(), ApiError> {
    if text.is_empty()
        || text.starts_with('-')
        || text.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(unprocessable(format!(
            "{what} is one word that does not start with \"-\""
        )));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    project: ProjectId,
    login: String,
    model: String,
    effort: String,
    message: Sent,
}

async fn start(api: &Api, id: WorkspaceId, body: &Bytes) -> Result<Response, ApiError> {
    if let Some(said) = &api.sessions_refused {
        return Ok(error(StatusCode::SERVICE_UNAVAILABLE, said.clone()));
    }
    let start: Start = read(body)?;
    one_word("a model", &start.model)?;
    one_word("an effort", &start.effort)?;
    let first = start.message.checked()?;
    let factory = FactoryStart {
        project: start.project,
        login: start.login,
        model: start.model,
        effort: start.effort,
    };
    let (sessions, store, me) = (api.sessions.clone(), api.store.clone(), api.peers.me());
    // The workspace's lifecycle lock is held from the look to the start, so two starts of one
    // workspace never both find none running.
    let started = api
        .workspaces
        .start_in(id, move |worktree| async move {
            // Only the project's factory sessions that run here are read, each by its id, and
            // the one with the lowest version answers.
            let running = sessions.factory_sessions(factory.project);
            let found = on_store(&store, move |store| {
                let mut found: Option<(u64, SessionId)> = None;
                for session in running {
                    let Some(record) = store.session(session)? else {
                        continue;
                    };
                    let here = record.owner == me
                        && !record.deleted
                        && record.body.workspace == id
                        && !matches!(
                            record.body.state,
                            SessionState::Finished | SessionState::Failed
                        );
                    if here && found.is_none_or(|(version, _)| record.version < version) {
                        found = Some((record.version, record.body.id));
                    }
                }
                Ok(found.map(|(_, session)| session))
            })
            .await?;
            if let Some(session) = found {
                return Ok(Started::Running(session));
            }
            sessions
                .start_factory(id, &worktree, factory, first)
                .await
                .map(Started::New)
                .map_err(start_refused)
        })
        .await?;
    Ok(match started {
        Started::New(session) => created(session),
        Started::Running(session) => {
            Json(serde_json::json!({ "id": session.to_string() })).into_response()
        }
    })
}

/// What a factory start did.
enum Started {
    New(SessionId),
    /// A factory session of the project already ran in the workspace.
    Running(SessionId),
}

/// Why a factory start failed, as the caller is told it.
fn start_refused(e: SessionError) -> ApiError {
    let status = match &e {
        SessionError::NoToken(NoToken::Unset { .. }) | SessionError::BodyWithoutPath => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        SessionError::Ended => StatusCode::CONFLICT,
        _ => return e.into(),
    };
    ApiError::Rejected(status, e.to_string())
}

async fn end(api: &Api, id: WorkspaceId) -> Result<Response, ApiError> {
    let me = api.peers.me();
    let owned = on_store(&api.store, move |store| store.workspace(id))
        .await?
        .is_some_and(|record| record.owner == me);
    if !owned {
        return Err(ApiError::NotFound(format!("no workspace {id} here")));
    }
    api.sessions.end_factory(id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// --- sessions ---

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sending {
    message: Sent,
    #[serde(default)]
    only_if_prompts: Option<u64>,
}

async fn message(api: &Api, id: SessionId, body: &Bytes) -> Result<Response, ApiError> {
    let send: Sending = read(body)?;
    let message = send.message.checked()?;
    match api
        .sessions
        .send_factory(id, message, send.only_if_prompts)
        .await
    {
        Ok(prompts) => Ok(Json(serde_json::json!({ "prompts": prompts })).into_response()),
        Err(refused) => Ok(not_sent(&refused)),
    }
}

/// `409` with why the message was not sent: the reason as text, as a word the caller can
/// match, the count of prompts when it rose, and, for `failed`, whose line may have been
/// typed, the count the send read before it typed (`null` when it read none or did not
/// finish), which a resend names as `only_if_prompts`.
fn not_sent(refused: &SendRefused) -> Response {
    let reason = match refused {
        SendRefused::NotIdle => "not_idle",
        SendRefused::CountRose { .. } => "count_rose",
        SendRefused::NoSuchSession => "no_such_session",
        SendRefused::BodyWithoutPath => "body_without_path",
        SendRefused::SecretsUnreadable => "secrets_unreadable",
        SendRefused::Failed { .. } => "failed",
    };
    let text = redact(&refused.to_string());
    if matches!(
        refused,
        SendRefused::SecretsUnreadable | SendRefused::Failed { .. }
    ) {
        tracing::warn!("api: a factory message was refused or failed: {text}");
    }
    let mut answer = serde_json::json!({ "error": text, "reason": reason });
    match refused {
        SendRefused::CountRose { now } => answer["now"] = (*now).into(),
        SendRefused::Failed { prompts, .. } => answer["prompts"] = (*prompts).into(),
        _ => {}
    }
    (StatusCode::CONFLICT, Json(answer)).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Turn {
    turn: u64,
}

async fn turn(api: &Api, id: SessionId, body: &Bytes) -> Result<Response, ApiError> {
    let asked: Turn = read(body)?;
    let answer = |text: TurnText| {
        Json(serde_json::json!({ "message": text.message, "question": text.question }))
            .into_response()
    };
    if let Some(text) = api.sessions.turn_text(id, asked.turn).await {
        return Ok(answer(text));
    }
    let me = api.peers.me();
    let runs = on_store(&api.store, move |store| store.session(id))
        .await?
        .is_some_and(|record| {
            record.owner == me
                && !record.deleted
                && !matches!(
                    record.body.state,
                    SessionState::Finished | SessionState::Failed
                )
        });
    if runs {
        return Err(ApiError::NotFound(format!(
            "turn {} of factory session {id} is not written yet",
            asked.turn
        )));
    }
    // The session may have written the turn just before it ended.
    if let Some(text) = api.sessions.turn_text(id, asked.turn).await {
        return Ok(answer(text));
    }
    Err(ApiError::Rejected(
        StatusCode::GONE,
        format!(
            "factory session {id} does not run here, so its turn {} will not be written",
            asked.turn
        ),
    ))
}

// --- terminals ---

/// Only a terminal this machine owns that its workspace's record names as its setup has a tail.
/// The setup terminal is named only once it has started, and a session's terminal never is, so
/// a session's terminal has no tail even before its session's record is written.
async fn tail(api: &Api, id: TerminalId) -> Result<Response, ApiError> {
    let me = api.peers.me();
    let a_setup = on_store(&api.store, move |store| {
        let Some(terminal) = store.terminal(id)?.filter(|record| record.owner == me) else {
            return Ok(false);
        };
        Ok(store
            .workspace(terminal.body.workspace)?
            .is_some_and(|workspace| workspace.owner == me && workspace.body.setup == Some(id)))
    })
    .await?;
    if !a_setup {
        return Err(ApiError::NotFound(format!(
            "terminal {id} is no workspace's setup here: only a setup's tail is read, and a \
             session is read through its turns"
        )));
    }
    let fetched = api.terminals.tail(id, 2 * TAIL_LIMIT).await?;
    let text = shown_tail(&fetched);
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        text,
    )
        .into_response())
}

/// The tail answered of `fetched`, a terminal's last `2 * TAIL_LIMIT` bytes at most: the
/// passwords of URLs hidden, then cut to its last [`TAIL_LIMIT`] bytes. Hiding before the cut
/// sees each URL whole unless its `://` is more than [`TAIL_LIMIT`] bytes before its password.
/// Hiding can also lengthen the text, as `a@` becomes `***@`.
fn shown_tail(fetched: &str) -> String {
    last_bytes(&redact(fetched), TAIL_LIMIT).to_owned()
}

/// The last `max` bytes of `text` at most, cut on a character boundary.
fn last_bytes(text: &str, max: usize) -> &str {
    let mut at = text.len().saturating_sub(max);
    while !text.is_char_boundary(at) {
        at += 1;
    }
    &text[at..]
}

// --- the Factory view ---

/// The start every repo URL a factory may be turned on for has.
pub const GITHUB_HTTPS: &str = "https://github.com/";

/// What the Factory view's routes act on: this machine's store and peers, the coordinator that
/// sends each settings edit, the secret store that holds the default login and the secrets to
/// hide, and the leases of the projects this machine runs.
#[derive(Clone)]
pub struct View {
    pub store: Arc<Mutex<Store>>,
    pub peers: Peers,
    pub coordinator: Arc<Coordinator>,
    pub secrets: Arc<dyn Secrets>,
    pub leases: Arc<Leases>,
}

/// The Factory view's routes. The caller guards them as it guards the rest of the API.
pub fn view_router(view: View) -> Router {
    Router::new()
        .route("/api/factory", get(overview))
        .route("/api/factory/runs/{id}", get(run_events))
        .route("/api/factory/runs/{id}/stop", post(stop_run))
        .route("/api/factory/settings", post(fleet_settings))
        .route("/api/projects/{id}/factory", post(project_factory))
        .with_state(Arc::new(view))
}

/// The status believed of each project of `settings`, with the machine that wrote it, read from
/// `store`, with `online` the machines heard from: see [`status::believed`]. A project none is
/// believed of is left out.
pub(super) fn believed_statuses(
    store: &Store,
    settings: &SharedSettings,
    online: &BTreeSet<MachineId>,
) -> Result<BTreeMap<ProjectId, (MachineId, FactoryStatus)>, StoreError> {
    let acting = crate::coordinator::acting(&settings.priority, online);
    let mut believed = BTreeMap::new();
    for project in &settings.projects {
        let statuses = store.statuses(project.id)?;
        if let Some((writer, status)) = status::believed(&statuses, online, acting) {
            believed.insert(project.id, (writer, status.clone()));
        }
    }
    Ok(believed)
}

/// What joins the strings of a value into the one text [`hide_strings`] hides: a line break on
/// each side, so the password of a URL is read within its own string, and between them a
/// character that is no gap and in no secret, so no secret is matched across two strings.
pub(super) const JOINT: &str = "\n\u{E000}\n";

/// The character between the line breaks of [`JOINT`].
pub(super) const JOINT_MARK: char = '\u{E000}';

/// The secret store could not be read, so nothing that was to be hidden may be shown.
#[derive(Debug)]
pub(super) struct Unreadable;

/// Every string of `value` in order, its objects' keys left out.
fn strings_of<'a>(value: &'a mut Value, texts: &mut Vec<&'a mut String>) {
    match value {
        Value::String(text) => texts.push(text),
        Value::Array(items) => {
            for item in items {
                strings_of(item, texts);
            }
        }
        Value::Object(fields) => {
            for field in fields.values_mut() {
                strings_of(field, texts);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Every string in `value` passed through `hide`, which reads the secret store each time it is
/// called: the strings are joined by [`JOINT`] and hidden in one call, or each in a call of its
/// own when one of them holds the joint's middle character or the hidden text no longer splits
/// into as many. The
/// keys of its objects stay as they are. A `hide` that answers
/// [`HIDDEN_UNREADABLE`](secrets::HIDDEN_UNREADABLE) for text that did not say so is a store
/// that could not be read: [`Unreadable`], and `value` is left as it was, never half hidden.
pub(super) fn hide_strings(
    value: &mut Value,
    hide: &dyn Fn(&str) -> String,
) -> Result<(), Unreadable> {
    let mut texts = Vec::new();
    strings_of(value, &mut texts);
    texts.retain(|text| !text.is_empty());
    if texts.is_empty() {
        return Ok(());
    }
    let unread = |text: &str, hidden: &str| {
        hidden == secrets::HIDDEN_UNREADABLE && text != secrets::HIDDEN_UNREADABLE
    };
    // A string with the joint's middle in it is hidden on its own: one that ended with the
    // joint's start would split back at the wrong place.
    if !texts.iter().any(|text| text.contains(JOINT_MARK)) {
        let joined = texts
            .iter()
            .map(|text| text.as_str())
            .collect::<Vec<_>>()
            .join(JOINT);
        let hidden = hide(&joined);
        if unread(&joined, &hidden) {
            return Err(Unreadable);
        }
        let parts: Vec<&str> = hidden.split(JOINT).collect();
        if parts.len() == texts.len() {
            for (text, part) in texts.into_iter().zip(parts) {
                *text = part.to_owned();
            }
            return Ok(());
        }
    }
    let mut hidden = Vec::with_capacity(texts.len());
    for text in &texts {
        let one = hide(text);
        if unread(text, &one) {
            return Err(Unreadable);
        }
        hidden.push(one);
    }
    for (text, one) in texts.into_iter().zip(hidden) {
        *text = one;
    }
    Ok(())
}

/// Runs `work` on the blocking pool, as a read of the secret store may block.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work).await.map_err(|e| {
        ApiError::Rejected(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the work did not finish: {e}"),
        )
    })
}

impl View {
    /// `value` with every string in it hidden as [`secrets::hide_known`] hides it, or `503`
    /// when the secret store cannot be read, so nothing of it is shown.
    async fn hidden(&self, mut value: Value) -> Result<Value, ApiError> {
        let secrets = self.secrets.clone();
        blocking(move || {
            hide_strings(&mut value, &|text| secrets::hide_known(text, &*secrets)).map(|()| value)
        })
        .await?
        .map_err(|Unreadable| {
            ApiError::Rejected(
                StatusCode::SERVICE_UNAVAILABLE,
                "the secret store could not be read, so the answer, which must have the known \
                 secrets hidden, is not shown"
                    .to_owned(),
            )
        })
    }

    /// `value` hidden, as a `200` JSON answer.
    async fn answer(&self, value: Value) -> Result<Response, ApiError> {
        Ok(Json(self.hidden(value).await?).into_response())
    }

    /// `503`, before anything is written, when the secret store cannot be read: the answer to
    /// a write must have the known secrets hidden, and a `503` on a write route then always
    /// means nothing was done.
    async fn readable(&self) -> Result<(), ApiError> {
        let probe = "the secret store is read";
        if self.hide(probe.to_owned()).await == secrets::HIDDEN_UNREADABLE {
            return Err(ApiError::Rejected(
                StatusCode::SERVICE_UNAVAILABLE,
                "the secret store could not be read, so nothing was done: the answer must have \
                 the known secrets hidden"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// The `200` answer to a write that was made: `value` hidden, or, should the secret store
    /// stop being readable since [`View::readable`], with every string of it given as
    /// [`HIDDEN_UNREADABLE`](secrets::HIDDEN_UNREADABLE), so it is never answered as not made.
    async fn answer_made(&self, value: Value) -> Result<Response, ApiError> {
        let mut shown = value.clone();
        match self.hidden(value).await {
            Ok(hidden) => Ok(Json(hidden).into_response()),
            Err(_) => {
                let mut texts = Vec::new();
                strings_of(&mut shown, &mut texts);
                for text in texts {
                    *text = secrets::HIDDEN_UNREADABLE.to_owned();
                }
                Ok(Json(shown).into_response())
            }
        }
    }

    /// `text` with the known secrets hidden.
    async fn hide(&self, text: String) -> String {
        let secrets = self.secrets.clone();
        blocking(move || secrets::hide_known(&text, &*secrets))
            .await
            .unwrap_or_else(|_| secrets::HIDDEN_UNREADABLE.to_owned())
    }

    /// Where an edit that needs the acting coordinator's own secret store is made: on that
    /// machine. `None` there. On any other, the acting coordinator's answer to the request,
    /// `path` with `body`, carried there; a request carried here is `503`, never carried on.
    async fn at_coordinator(
        &self,
        headers: &HeaderMap,
        path: &str,
        body: &Bytes,
    ) -> Result<Option<Response>, ApiError> {
        let acting = self.coordinator.acting().await?;
        if acting == self.peers.me() {
            return Ok(None);
        }
        if headers.contains_key(CARRIED) {
            return Err(ApiError::Rejected(
                StatusCode::SERVICE_UNAVAILABLE,
                "this machine no longer acts as coordinator, so the edit was not made here; send \
                 it again"
                    .to_owned(),
            ));
        }
        Ok(Some(
            self.carry_to(acting, path, Some("application/json"), body)
                .await,
        ))
    }

    /// `machine`'s answer to `path` with `body`, carried there, or why it could not be carried.
    async fn carry_to(
        &self,
        machine: MachineId,
        path: &str,
        content_type: Option<&str>,
        body: &[u8],
    ) -> Response {
        match self.peers.call(machine, path, content_type, body).await {
            Ok(answer) => carry::answered(answer),
            Err(refused) => carry::not_carried(&refused),
        }
    }

    /// Sends `edit` through the coordinator, answering a failure as every shared-settings route
    /// does.
    async fn edit(&self, edit: Edit) -> Result<(), ApiError> {
        match self.coordinator.edit(edit).await {
            Ok(_) => Ok(()),
            Err(e) => Err(ApiError::Rejected(edit_status(&e), e.to_string())),
        }
    }
}

/// `value` as JSON, or the server error that says it could not be.
fn to_json(value: &impl serde::Serialize) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|e| {
        ApiError::Rejected(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the answer could not be written: {e}"),
        )
    })
}

async fn overview(State(view): State<Arc<View>>) -> Result<Response, ApiError> {
    let online = view.peers.online();
    let (settings, believed) = on_store(&view.store, move |store| {
        let settings = store.settings()?;
        let believed = believed_statuses(store, &settings, &online)?;
        Ok((settings, believed))
    })
    .await?;
    let mut projects = Vec::new();
    for project in &settings.projects {
        let factory = settings
            .project_factory
            .get(&project.id)
            .cloned()
            .unwrap_or_default();
        let (writer, status) = match believed.get(&project.id) {
            Some((writer, status)) => (Some(*writer), Some(status)),
            None => (None, None),
        };
        projects.push(serde_json::json!({
            "id": project.id,
            "name": project.name,
            "factory": to_json(&factory)?,
            "writer": writer,
            "status": to_json(&status)?,
        }));
    }
    let fleet = settings.factory.clone().unwrap_or_default();
    view.answer(serde_json::json!({ "settings": to_json(&fleet)?, "projects": projects }))
        .await
}

/// The run a path names, or `404` for one that is no run id.
fn run_named(id: &str) -> Result<RunId, ApiError> {
    id.parse()
        .map_err(|_| ApiError::NotFound(format!("no run {id:?} here")))
}

async fn run_events(
    State(view): State<Arc<View>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let run = run_named(&id)?;
    let read = run.clone();
    let events = on_store(&view.store, move |store| store.run_events(&read)).await?;
    if events.is_empty() {
        return Err(ApiError::NotFound(format!("no run {run} here")));
    }
    view.answer(serde_json::json!({ "run": run.to_string(), "events": to_json(&events)? }))
        .await
}

fn conflict(message: String) -> ApiError {
    ApiError::Rejected(StatusCode::CONFLICT, message)
}

async fn stop_run(
    State(view): State<Arc<View>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let run = run_named(&id)?;
    let held = view
        .leases
        .of_repo(run.repo())
        .and_then(|(_, leased): (ProjectId, Leased)| leased.holding());
    if let Some((guard, generation)) = held {
        view.readable().await?;
        return stop_here(&view, run, guard, generation).await;
    }
    let repo = run.repo().to_owned();
    // A carried stop is answered where it landed, and never carried on.
    if headers.contains_key(CARRIED) {
        return Err(conflict(format!(
            "this machine does not hold the lease of {repo}, so the stop was not written: only \
             the lease holder writes it"
        )));
    }
    let (me, online) = (view.peers.me(), view.peers.online());
    let wanted = repo.clone();
    let holder = on_store(&view.store, move |store| {
        let settings = store.settings()?;
        let believed = believed_statuses(store, &settings, &online)?;
        Ok(settings
            .projects
            .iter()
            // A run id folds its repo, as GitHub does; a project's URL need not.
            .filter(|project| {
                github_repo(&project.repo).is_some_and(|repo| repo.eq_ignore_ascii_case(&wanted))
            })
            .filter_map(|project| believed.get(&project.id))
            .find(|(_, status)| status.holds)
            .map(|(writer, _)| *writer))
    })
    .await?;
    match holder {
        Some(holder) if holder != me => {
            let path = format!("/api/factory/runs/{run}/stop");
            Ok(view.carry_to(holder, &path, None, &[]).await)
        }
        Some(_) => Err(conflict(format!(
            "this machine's status says it holds the lease of {repo}, but it no longer does, \
             so the stop was not written; try again once the lease is held"
        ))),
        None => Err(conflict(format!(
            "no machine is known to hold the lease of {repo}, so the stop was not written; try \
             again once its factory holds its lease"
        ))),
    }
}

/// Records a stop of `run` on this machine, which holds its project's lease under `guard` at
/// `generation`.
async fn stop_here(
    view: &View,
    run: RunId,
    guard: Arc<dyn Guard>,
    generation: u64,
) -> Result<Response, ApiError> {
    let secrets = view.secrets.clone();
    let hide: crate::factory::ledger::Hide =
        Arc::new(move |text: &str| secrets::hide_known(text, &*secrets));
    let ledger = Ledger::new(view.store.clone(), view.peers.me(), generation, hide);
    let id = run.to_string();
    let asked = blocking(move || stop::request(&ledger, &*guard, &run)).await?;
    match asked {
        Ok(request) => match request.answer(&id) {
            StopAnswer::Stopped { already, message } => {
                view.answer_made(serde_json::json!({
                    "stopped": true,
                    "already": already,
                    "message": message,
                }))
                .await
            }
            StopAnswer::Refused { message } => Err(conflict(view.hide(message).await)),
        },
        Err(RunError::Invalid(said)) => Err(ApiError::NotFound(view.hide(said).await)),
        Err(RunError::LeaseLost(lost)) => Err(conflict(
            view.hide(format!(
                "the lease was lost before the stop was written, so it was not: {lost}"
            ))
            .await,
        )),
        Err(e) => Err(ApiError::Rejected(
            StatusCode::INTERNAL_SERVER_ERROR,
            view.hide(format!("the stop could not be recorded: {e}"))
                .await,
        )),
    }
}

async fn fleet_settings(State(view): State<Arc<View>>, body: Bytes) -> Result<Response, ApiError> {
    let sent: Map<String, Value> = read(&body)?;
    // The value is sent whole, as every shared setting is: a field left out is not taken for
    // its default, nor for the held one.
    let Value::Object(fields) = to_json(&FactorySettings::default())? else {
        return Err(ApiError::Rejected(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the factory settings are not an object".to_owned(),
        ));
    };
    if let Some(name) = sent.keys().find(|name| !fields.contains_key(*name)) {
        return Err(unprocessable(format!(
            "the factory settings have no field {name:?}"
        )));
    }
    let missing: Vec<&str> = fields
        .keys()
        .filter(|name| !sent.contains_key(*name))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return Err(unprocessable(format!(
            "the factory settings are sent whole; this edit leaves out {}",
            missing.join(", ")
        )));
    }
    let settings: FactorySettings = serde_json::from_value(Value::Object(sent))
        .map_err(|e| unprocessable(format!("the factory settings are not taken: {e}")))?;
    one_word("a model", &settings.model)?;
    one_word("an effort", &settings.effort)?;
    view.readable().await?;
    view.edit(Edit::SetFactory {
        settings: settings.clone(),
    })
    .await?;
    view.answer_made(serde_json::json!({ "settings": to_json(&settings)? }))
        .await
}

/// Whether a project on `repo` may have its factory on: an `https://github.com/` URL that
/// names a repo.
fn on_github(repo: &str) -> bool {
    repo.starts_with(GITHUB_HTTPS) && github_repo(repo).is_some()
}

/// A project's factory setting as an edit sends it: whole, but for a login it may leave out.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectEdit {
    on: bool,
    paused: bool,
    /// Absent, `null` or blank names none.
    #[serde(default)]
    login: Option<String>,
}

async fn project_factory(
    State(view): State<Arc<View>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let project = parse_id(&id, "project", ProjectId)?;
    let edit: ProjectEdit = read(&body)?;
    let named = edit
        .login
        .map(|login| login.trim().to_owned())
        .filter(|login| !login.is_empty());
    if let Some(login) = &named
        && !secrets::is_login(login)
    {
        return Err(unprocessable(format!("a login is {}", secrets::LOGIN_RULE)));
    }
    let settings = on_store(&view.store, |store| store.settings()).await?;
    let Some(held_project) = settings.projects.iter().find(|p| p.id == project) else {
        return Err(ApiError::NotFound(format!("no project {project} here")));
    };
    let refused_url = || {
        unprocessable(format!(
            "the factory of {:?} cannot be turned on: its repo URL does not start \
             {GITHUB_HTTPS} and name a repo there, and a factory reaches GitHub only over HTTPS",
            held_project.name
        ))
    };
    let held_on = settings
        .project_factory
        .get(&project)
        .is_some_and(|factory| factory.on);
    let login = match named {
        Some(login) => {
            if edit.on && !held_on && !on_github(&held_project.repo) {
                return Err(refused_url());
            }
            login
        }
        None if !edit.on => String::new(),
        None => {
            // On with no login: only a turn-on takes one, the acting coordinator's default at
            // that moment (F4), which only that machine's secret store holds. So the edit is
            // decided there, against its copy as read now.
            let path = format!("/api/projects/{project}/factory");
            if let Some(answer) = view.at_coordinator(&headers, &path, &body).await? {
                return Ok(answer);
            }
            if held_on {
                return Err(unprocessable(format!(
                    "the factory of {:?} is on, and acts as its login: an edit of it names the \
                     login, as GET /api/factory gives it (a turn-on sent again after a lost \
                     answer finds the first one made)",
                    held_project.name
                )));
            }
            if !on_github(&held_project.repo) {
                return Err(refused_url());
            }
            // Read here and not through `secrets::github_default`, which takes a store that
            // cannot be read for one with no default.
            let secrets = view.secrets.clone();
            match blocking(move || secrets.get(secrets::GITHUB_DEFAULT)).await? {
                Ok(Some(login)) if secrets::is_login(&login) => login,
                Ok(_) => {
                    return Err(unprocessable(
                        "no login was named and the coordinator names no default login: name \
                         the project's login, or set the coordinator's default with `foundryd \
                         github default <login>`"
                            .to_owned(),
                    ));
                }
                Err(_) => {
                    return Err(ApiError::Rejected(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "the coordinator's secret store could not be read, so its default login \
                         is not known and nothing was done"
                            .to_owned(),
                    ));
                }
            }
        }
    };
    let factory = ProjectFactory {
        on: edit.on,
        login,
        paused: edit.paused,
    };
    view.readable().await?;
    view.edit(Edit::SetProjectFactory {
        project,
        factory: factory.clone(),
    })
    .await?;
    view.answer_made(serde_json::json!({ "factory": to_json(&factory)? }))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(line: &str, body: Option<&str>) -> Sent {
        Sent {
            line: line.to_owned(),
            body: body.map(str::to_owned),
        }
    }

    #[test]
    fn a_message_is_one_line_under_the_limit_and_a_body_that_fits() {
        let line = "/factory-build o/r#1 {body}";
        let message = sent(line, Some("the brief\n")).checked().ok().unwrap();
        assert_eq!(message.line, line);
        assert_eq!(message.body.as_deref(), Some("the brief\n"));
        let longest = "x".repeat(LINE_LIMIT - 1);
        assert!(sent(&longest, None).checked().is_ok());
        let full = "x".repeat(BODY_LIMIT);
        assert!(sent("{body}", Some(&full)).checked().is_ok());

        let too_long = "x".repeat(LINE_LIMIT);
        let too_big = "x".repeat(BODY_LIMIT + 1);
        for (line, body) in [
            (too_long.as_str(), None),
            ("", None),
            ("   ", None),
            ("one\ntwo", None),
            ("one\rtwo", None),
            ("nul\0", None),
            ("escape \u{1b}[2J", None),
            ("{body}", Some(too_big.as_str())),
            ("{body}", Some("nul\0")),
        ] {
            assert!(
                sent(line, body).checked().is_err(),
                "{line:?} {:?}",
                body.map(str::len)
            );
        }
    }

    #[test]
    fn a_model_or_effort_that_could_be_read_as_an_option_is_refused() {
        for good in ["high", "claude-opus-5-5", "max"] {
            assert!(one_word("an effort", good).is_ok(), "{good}");
        }
        for bad in ["", "-x", "--dangerously", "two words", "tab\there", "nl\n"] {
            assert!(one_word("an effort", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn every_refused_send_is_409_with_its_reason_and_the_count_when_it_rose_or_was_read() {
        let cases = [
            (SendRefused::NotIdle, "not_idle"),
            (SendRefused::CountRose { now: 7 }, "count_rose"),
            (SendRefused::NoSuchSession, "no_such_session"),
            (SendRefused::BodyWithoutPath, "body_without_path"),
            (SendRefused::SecretsUnreadable, "secrets_unreadable"),
            (
                SendRefused::Failed {
                    reason: "https://u:secret@example.com/x".into(),
                    prompts: Some(4),
                },
                "failed",
            ),
            (
                SendRefused::Failed {
                    reason: "the count was not read".into(),
                    prompts: None,
                },
                "failed",
            ),
        ];
        for (refused, reason) in cases {
            let response = not_sent(&refused);
            assert_eq!(response.status(), StatusCode::CONFLICT, "{refused}");
            let body = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
                .unwrap();
            let answer: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(answer["reason"], reason, "{answer}");
            assert!(!answer.to_string().contains("secret@"), "{answer}");
            match refused {
                SendRefused::CountRose { now } => assert_eq!(answer["now"], now),
                _ => assert!(answer.get("now").is_none(), "{answer}"),
            }
            // A failed send names the count it read before it typed, or null when it read
            // none, so a resend can name it; no other refusal has one.
            match refused {
                SendRefused::Failed { prompts, .. } => {
                    assert_eq!(answer.get("prompts"), Some(&serde_json::json!(prompts)));
                }
                _ => assert!(answer.get("prompts").is_none(), "{answer}"),
            }
        }
    }

    #[test]
    fn a_url_whose_scheme_falls_just_before_the_cut_keeps_its_password_hidden() {
        // The URL's `https://` ends right where a cut to the last TAIL_LIMIT bytes would
        // start, so a tail cut first and hidden after would begin `u:s3cret@...`.
        let after = "u:s3cret@example.com/r.git\n";
        let end = "more output\n";
        let padding = "y".repeat(TAIL_LIMIT - after.len() - end.len());
        let filler = "x".repeat(TAIL_LIMIT);
        let fetched = format!("{filler}\nfatal: https://{after}{padding}{end}");
        assert_eq!(&fetched[fetched.len() - TAIL_LIMIT..][..after.len()], after);
        let shown = shown_tail(&fetched);
        assert!(!shown.contains("s3cret"), "{}", &shown[..80]);
        assert!(shown.len() <= TAIL_LIMIT);
        assert!(shown.ends_with("more output\n"));
        assert_eq!(shown_tail("short\n"), "short\n");
    }

    #[test]
    fn a_start_refused_for_what_its_caller_sent_or_an_end_is_4xx_and_any_other_stays_a_server_error()
     {
        let cases = [
            (
                SessionError::NoToken(NoToken::Unset {
                    login: "nobody".into(),
                }),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                SessionError::BodyWithoutPath,
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (SessionError::Ended, StatusCode::CONFLICT),
            (SessionError::NoAskpass, StatusCode::INTERNAL_SERVER_ERROR),
            (
                SessionError::SecretsUnreadable,
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (error, status) in cases {
            let said = error.to_string();
            assert_eq!(
                start_refused(error).into_response().status(),
                status,
                "{said}"
            );
        }
    }

    #[test]
    fn strings_are_hidden_in_one_read_each_on_its_own_and_an_unreadable_store_hides_the_whole() {
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let token = "ghp_0123456789abcdefghij";
        let hide = |text: &str| {
            reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            text.replace(token, "***")
        };
        let mut value = serde_json::json!({
            "a": format!("x {token} y"),
            "b": ["", "plain", {"c": token}],
            "n": 3,
        });
        hide_strings(&mut value, &hide).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"a": "x *** y", "b": ["", "plain", {"c": "***"}], "n": 3})
        );
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one read"
        );

        // A string that holds the joint, or any of its middle, is hidden on its own, so no
        // text moves from one string to the next.
        let mut value = serde_json::json!([format!("a{JOINT}b {token}"), "c"]);
        hide_strings(&mut value, &hide).unwrap();
        assert_eq!(value, serde_json::json!([format!("a{JOINT}b ***"), "c"]));
        assert_eq!(JOINT, format!("\n{JOINT_MARK}\n"));
        let mut value = serde_json::json!([format!("a\n{JOINT_MARK}"), format!("b {token}")]);
        hide_strings(&mut value, &hide).unwrap();
        assert_eq!(
            value,
            serde_json::json!([format!("a\n{JOINT_MARK}"), "b ***"])
        );

        let unreadable = |_: &str| crate::secrets::HIDDEN_UNREADABLE.to_owned();
        let mut value = serde_json::json!({"a": "one", "b": "two"});
        assert!(hide_strings(&mut value, &unreadable).is_err());
        let mut value = serde_json::json!({"a": "one"});
        assert!(hide_strings(&mut value, &unreadable).is_err());
        // Text that already says so is not taken for a store that cannot be read.
        let mut value = serde_json::json!({"a": crate::secrets::HIDDEN_UNREADABLE});
        hide_strings(&mut value, &|text: &str| text.to_owned()).unwrap();
        let mut value = serde_json::json!({"a": "", "n": 1});
        hide_strings(&mut value, &unreadable).unwrap();
    }

    #[test]
    fn a_tail_is_cut_to_its_last_bytes_on_a_character_boundary() {
        let text = format!("é{}", "x".repeat(10));
        assert_eq!(last_bytes(&text, 11), "x".repeat(10));
        assert_eq!(last_bytes(&text, 12), text);
        assert_eq!(last_bytes("short", TAIL_LIMIT), "short");
    }
}

/// The Factory view's routes, served for one or more machines on loopback ports over peers
/// linked in memory, and called the way the UI calls them.
#[cfg(test)]
mod view_tests {
    use std::collections::BTreeMap;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use foundry_proto::factory::{
        FactoryBody, FactoryRecord, FactorySettings, FactoryStatus, NotHolding, ProjectFactory,
        RunId,
    };
    use foundry_proto::ids::{MachineId, ProjectId};
    use foundry_proto::settings::{Machine, Project, SharedSettings};
    use futures::future::BoxFuture;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::task::JoinHandle;

    use super::{View, view_router};
    use crate::coordinator::Coordinator;
    use crate::factory::lease::LeaseState;
    use crate::factory::ledger::Ledger;
    use crate::factory::ledger::events::{Cause, CauseStep, Event};
    use crate::factory::lifecycle::State;
    use crate::factory::runs::stop::{already_requested_message, requested_message};
    use crate::factory::testing::Fixed;
    use crate::factory::worker::{AlwaysHeld, Guard};
    use crate::factory::{Leased, Leases, Leasing};
    use crate::secrets::{GITHUB_DEFAULT, Secrets};
    use crate::store::Store;
    use crate::sync::call::CARRIED;
    use crate::sync::{Conn, Dialer, Peers};

    const WAIT: Duration = Duration::from_secs(20);
    const REPO: &str = "rawphp/softfac";
    const TOKEN: &str = "ghp_view0test0token0that0must0never0show";

    /// The peers of every machine of a test, by id, which each machine's dialer links to.
    #[derive(Default)]
    struct Net(Mutex<BTreeMap<MachineId, Peers>>);

    struct NetDial(Arc<Net>);

    impl Dialer for NetDial {
        fn dial(&self, peer: &Machine) -> BoxFuture<'static, std::io::Result<Conn>> {
            let target = self.0.0.lock().unwrap().get(&peer.id).cloned();
            Box::pin(async move {
                let target = target.ok_or_else(|| std::io::Error::other("no such machine"))?;
                let (ours, theirs) = Conn::pair();
                tokio::spawn(async move { target.accept(theirs).await });
                Ok(ours)
            })
        }
    }

    use crate::api::test_secrets::Switch;

    /// One machine: its store, peers, coordinator, secret store and leases, with the view's
    /// routes served on a loopback port that its peers also carry calls to.
    struct Served {
        me: MachineId,
        store: Arc<Mutex<Store>>,
        peers: Peers,
        secrets: Arc<Switch>,
        leases: Arc<Leases>,
        addr: SocketAddr,
        tasks: Vec<JoinHandle<()>>,
        _dir: TempDir,
    }

    impl Drop for Served {
        fn drop(&mut self) {
            for task in &self.tasks {
                task.abort();
            }
        }
    }

    fn project_id() -> ProjectId {
        ProjectId(uuid::Uuid::from_u128(0x77))
    }

    fn run_id() -> RunId {
        RunId::new(REPO, 8, 1).unwrap()
    }

    /// `n` machines that list each other, the first acting, holding a project on `repo` whose
    /// factory is `factory`, linked and each heard by the others.
    async fn fleet(n: usize, repo: &str, factory: Option<ProjectFactory>) -> Vec<Served> {
        fleet_of(n, &[(project_id(), repo, factory)]).await
    }

    fn other_project() -> ProjectId {
        ProjectId(uuid::Uuid::from_u128(0x78))
    }

    /// [`fleet`] with each of `projects`: its id, its repo and its factory.
    async fn fleet_of(
        n: usize,
        projects: &[(ProjectId, &str, Option<ProjectFactory>)],
    ) -> Vec<Served> {
        let ids: Vec<MachineId> = (0..n).map(|_| MachineId::random()).collect();
        let mut settings = SharedSettings {
            seq: 1,
            machines: ids
                .iter()
                .enumerate()
                .map(|(i, id)| Machine {
                    id: *id,
                    name: format!("machine-{i}"),
                    address: format!("100.64.0.{}", i + 1),
                    key: String::new(),
                })
                .collect(),
            priority: ids.clone(),
            ..SharedSettings::default()
        };
        for (i, (id, repo, factory)) in projects.iter().enumerate() {
            settings.projects.push(Project {
                id: *id,
                name: format!("softfac{i}"),
                repo: (*repo).into(),
            });
            if let Some(factory) = factory {
                settings.project_factory.insert(*id, factory.clone());
            }
        }
        let net = Arc::new(Net::default());
        let mut fleet = Vec::new();
        for me in &ids {
            let dir = tempfile::tempdir().unwrap();
            let mut store = Store::open(dir.path().join("foundry.db")).unwrap();
            store.apply_settings(settings.clone()).unwrap();
            let store = Arc::new(Mutex::new(store));
            let peers = Peers::new(*me, store.clone(), Arc::new(NetDial(net.clone())));
            let coordinator = Arc::new(Coordinator::new(*me, store.clone(), peers.clone()));
            peers.coordinate(coordinator.clone());
            let secrets = Arc::new(Switch::default());
            secrets.set("github:softfac-bot", TOKEN).unwrap();
            let leases = Arc::new(Leases::default());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            peers.serve_calls(addr);
            let app = view_router(View {
                store: store.clone(),
                peers: peers.clone(),
                coordinator: coordinator.clone(),
                secrets: secrets.clone(),
                leases: leases.clone(),
            });
            let mut tasks = vec![tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            })];
            let running = peers.clone();
            tasks.push(tokio::spawn(async move { running.run().await }));
            // Held for the daemon's life: the peers hold it weakly.
            let keep = coordinator.clone();
            tasks.push(tokio::spawn(async move {
                let _coordinator = keep;
                std::future::pending::<()>().await;
            }));
            net.0.lock().unwrap().insert(*me, peers.clone());
            fleet.push(Served {
                me: *me,
                store,
                peers,
                secrets,
                leases,
                addr,
                tasks,
                _dir: dir,
            });
        }
        for machine in &fleet {
            until(|| ids.iter().all(|id| machine.peers.online().contains(id))).await;
        }
        fleet
    }

    async fn until(mut done: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !done() {
            assert!(tokio::time::Instant::now() < deadline, "not in time");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    impl Served {
        /// `(status, body)` of `method path`, with `body` as JSON when there is one, and the
        /// headers `extra`.
        async fn send(
            &self,
            method: &str,
            path: &str,
            body: Option<Value>,
            extra: &[(&str, &str)],
        ) -> (u16, String) {
            let mut stream = tokio::net::TcpStream::connect(self.addr).await.unwrap();
            let body = body.map(|b| b.to_string()).unwrap_or_default();
            let headers: String = extra
                .iter()
                .map(|(name, value)| format!("{name}: {value}\r\n"))
                .collect();
            let request = format!(
                "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n{headers}\
                 Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                self.addr,
                body.len()
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut response = Vec::new();
            tokio::time::timeout(WAIT, stream.read_to_end(&mut response))
                .await
                .expect("the routes answer in time")
                .unwrap();
            let response = String::from_utf8(response).unwrap();
            let (head, body) = response.split_once("\r\n\r\n").unwrap();
            let status = head.split(' ').nth(1).unwrap().parse().unwrap();
            // A chunked body is read as its one chunk, which is all these answers have.
            let body = if head
                .to_ascii_lowercase()
                .contains("transfer-encoding: chunked")
            {
                body.split_once("\r\n")
                    .map(|(_, rest)| rest.rsplit_once("\r\n0\r\n").map_or(rest, |(b, _)| b))
                    .unwrap_or(body)
                    .trim_end_matches("\r\n")
                    .to_owned()
            } else {
                body.to_owned()
            };
            (status, body)
        }

        async fn get(&self, path: &str) -> (u16, Value) {
            let (status, body) = self.send("GET", path, None, &[]).await;
            (status, serde_json::from_str(&body).unwrap_or(Value::Null))
        }

        async fn post(&self, path: &str, body: Option<Value>) -> (u16, Value) {
            let (status, body) = self.send("POST", path, body, &[]).await;
            (status, serde_json::from_str(&body).unwrap_or(Value::Null))
        }

        fn settings(&self) -> SharedSettings {
            self.store.lock().unwrap().settings().unwrap()
        }

        /// Writes `status` as this machine's own status record.
        fn write_status(&self, status: FactoryStatus) {
            let mut store = self.store.lock().unwrap();
            let version = store.own_next_factory_version().unwrap();
            let record = FactoryRecord {
                owner: self.me,
                version,
                body: FactoryBody::Status(status),
            };
            store.apply_factory(&record, self.me).unwrap();
        }

        fn statuses(&self) -> Vec<(MachineId, FactoryStatus)> {
            self.store.lock().unwrap().statuses(project_id()).unwrap()
        }

        fn statuses_of(&self, project: ProjectId) -> Vec<(MachineId, FactoryStatus)> {
            self.store.lock().unwrap().statuses(project).unwrap()
        }

        /// This machine's ledger at `generation`, hiding nothing, as a writer that did not
        /// hide would leave it.
        fn ledger(&self, generation: u64) -> Ledger {
            Ledger::new(
                self.store.clone(),
                self.me,
                generation,
                Arc::new(|text: &str| text.to_owned()),
            )
        }

        /// Puts a lease of the project in this machine's handle, whose newest tick said
        /// `state` and whose guard is `guard`.
        fn hold(&self, state: LeaseState, guard: Arc<dyn Guard>) {
            self.hold_of(project_id(), REPO, state, guard);
        }

        /// [`Served::hold`] for `project`, whose repo the service named `repo`.
        fn hold_of(
            &self,
            project: ProjectId,
            repo: &str,
            state: LeaseState,
            guard: Arc<dyn Guard>,
        ) {
            let lease: Arc<dyn Leasing> = Arc::new(Fixed {
                state: state.clone(),
                guard,
            });
            self.leases.put(
                project,
                Leased {
                    repo: repo.into(),
                    lease,
                    state: Arc::new(Mutex::new(Some(state))),
                },
            );
        }
    }

    fn on() -> Option<ProjectFactory> {
        Some(ProjectFactory {
            on: true,
            login: "softfac-bot".into(),
            paused: false,
        })
    }

    fn holding(generation: u64, pass: u64) -> FactoryStatus {
        FactoryStatus {
            project: project_id(),
            pass,
            holds: true,
            generation,
            not_holding: None,
            idle: None,
            owner_ignored: false,
            ready: Vec::new(),
            feedback_waiting: Vec::new(),
            runs: Vec::new(),
            held_up: Vec::new(),
            gate_infra: Vec::new(),
            finished: Vec::new(),
            foreign: Vec::new(),
        }
    }

    fn not_holding(holder: MachineId, generation: u64) -> FactoryStatus {
        FactoryStatus {
            holds: false,
            not_holding: Some(NotHolding::HeldBy {
                machine: holder,
                generation,
            }),
            ..holding(generation, 1)
        }
    }

    /// Writes a building run of the project in `machine`'s ledger at `generation`.
    fn building_run(machine: &Served, generation: u64) {
        let ledger = machine.ledger(generation);
        let cause = Cause::new(CauseStep::Pass, "claimed");
        for event in [
            Event::Claimed {
                machine: "machine-0".into(),
                github_user: "softfac-bot".into(),
            },
            Event::StateChanged {
                from: State::Ready,
                to: State::Triage,
            },
            Event::StateChanged {
                from: State::Triage,
                to: State::Building,
            },
        ] {
            ledger.write(&run_id(), &event, &cause).unwrap();
        }
    }

    fn stop_path() -> String {
        format!("/api/factory/runs/{}/stop", run_id())
    }

    fn stops_held(machine: &Served) -> usize {
        machine
            .store
            .lock()
            .unwrap()
            .run_events(&run_id())
            .unwrap()
            .iter()
            .filter(|event| event.event == "stop_requested")
            .count()
    }

    /// The `stop_requested` events of the run in `machine`'s store that `writer` wrote, so a
    /// copy that sync brought from another writer is never counted.
    fn stops_written_by(machine: &Served, writer: MachineId) -> usize {
        machine
            .store
            .lock()
            .unwrap()
            .run_events_by(&run_id(), writer, None)
            .unwrap()
            .iter()
            .filter(|(_, event)| event.event == "stop_requested")
            .count()
    }

    // ----- GET /api/factory -----

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_view_on_a_machine_that_is_not_the_coordinator_gives_the_holders_status() {
        let fleet = fleet(2, &format!("https://github.com/{REPO}"), on()).await;
        let (coordinator, other) = (&fleet[0], &fleet[1]);
        // The second machine holds the lease; the coordinator's own status says so.
        other.write_status(holding(3, 9));
        coordinator.write_status(not_holding(other.me, 3));
        until(|| coordinator.statuses().len() == 2 && other.statuses().len() == 2).await;

        for asked in [coordinator, other] {
            let (status, view) = asked.get("/api/factory").await;
            assert_eq!(status, 200, "{view}");
            assert_eq!(
                view["settings"],
                serde_json::to_value(FactorySettings::default()).unwrap(),
                "the fleet's settings, default while unset"
            );
            let projects = view["projects"].as_array().unwrap();
            assert_eq!(projects.len(), 1, "{view}");
            let project = &projects[0];
            assert_eq!(project["id"], json!(project_id()));
            assert_eq!(project["factory"], serde_json::to_value(on()).unwrap());
            assert_eq!(project["writer"], json!(other.me), "{view}");
            assert_eq!(
                project["status"],
                serde_json::to_value(holding(3, 9)).unwrap(),
                "{view}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_view_believes_the_coordinator_when_no_online_writer_holds() {
        let fleet = fleet(1, &format!("https://github.com/{REPO}"), on()).await;
        let alone = &fleet[0];
        // A holder that is not heard from is not believed.
        let gone = MachineId::random();
        {
            let mut store = alone.store.lock().unwrap();
            let record = FactoryRecord {
                owner: gone,
                version: 1,
                body: FactoryBody::Status(holding(5, 40)),
            };
            store.apply_factory(&record, gone).unwrap();
        }
        let (_, view) = alone.get("/api/factory").await;
        assert_eq!(view["projects"][0]["status"], Value::Null, "{view}");
        assert_eq!(view["projects"][0]["writer"], Value::Null, "{view}");
        let said = FactoryStatus {
            holds: false,
            not_holding: Some(NotHolding::NoToken {
                login: "softfac-bot".into(),
            }),
            ..holding(5, 2)
        };
        alone.write_status(said.clone());
        let (_, view) = alone.get("/api/factory").await;
        assert_eq!(view["projects"][0]["writer"], json!(alone.me), "{view}");
        assert_eq!(
            view["projects"][0]["status"],
            serde_json::to_value(said).unwrap()
        );
    }

    // ----- GET /api/factory/runs/{id} -----

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_runs_events_come_in_order_and_an_unknown_run_is_not_found() {
        let fleet = fleet(1, &format!("https://github.com/{REPO}"), on()).await;
        let alone = &fleet[0];
        building_run(alone, 2);
        alone
            .ledger(1)
            .write(
                &run_id(),
                &Event::StepChanged {
                    step: "earlier".into(),
                },
                &Cause::new(CauseStep::Pass, "test"),
            )
            .unwrap();
        let (status, run) = alone.get(&format!("/api/factory/runs/{}", run_id())).await;
        assert_eq!(status, 200, "{run}");
        assert_eq!(run["run"], json!(run_id().to_string()));
        let names: Vec<&str> = run["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["event"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["step_changed", "claimed", "state_changed", "state_changed"],
            "generation 1 before generation 2"
        );
        let unknown = RunId::new(REPO, 9, 1).unwrap();
        let (status, _) = alone.get(&format!("/api/factory/runs/{unknown}")).await;
        assert_eq!(status, 404);
        let (status, said) = alone.get("/api/factory/runs/not-a-run").await;
        assert_eq!(status, 404, "{said}");
    }

    // ----- POST /api/factory/runs/{id}/stop -----

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_stop_sent_to_a_machine_that_is_not_the_holder_reaches_the_holder() {
        let fleet = fleet(2, &format!("https://github.com/{REPO}"), on()).await;
        let (holder, other) = (&fleet[0], &fleet[1]);
        holder.hold(LeaseState::Held { generation: 3 }, Arc::new(AlwaysHeld(3)));
        holder.write_status(holding(3, 4));
        building_run(holder, 3);
        until(|| other.statuses().len() == 1).await;

        let (status, answer) = other.post(&stop_path(), None).await;
        assert_eq!(status, 200, "{answer}");
        assert_eq!(answer["stopped"], json!(true), "{answer}");
        assert_eq!(answer["already"], json!(false), "{answer}");
        assert_eq!(
            answer["message"],
            json!(requested_message(&run_id().to_string()))
        );
        let events = holder.store.lock().unwrap().run_events(&run_id()).unwrap();
        let stop = events.last().unwrap();
        assert_eq!(stop.event, "stop_requested");
        assert_eq!(stop.generation, 3, "written at the holder's generation");
        assert_eq!(
            stops_written_by(other, other.me),
            0,
            "the machine asked writes nothing itself"
        );

        let (status, answer) = other.post(&stop_path(), None).await;
        assert_eq!(status, 200, "{answer}");
        assert_eq!(answer["already"], json!(true), "{answer}");
        assert_eq!(
            answer["message"],
            json!(already_requested_message(&run_id().to_string()))
        );
        assert_eq!(stops_held(holder), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_stop_on_the_holder_itself_is_written_there() {
        let fleet = fleet(1, &format!("https://github.com/{REPO}"), on()).await;
        let holder = &fleet[0];
        holder.hold(LeaseState::Held { generation: 2 }, Arc::new(AlwaysHeld(2)));
        building_run(holder, 2);
        let (status, answer) = holder.post(&stop_path(), None).await;
        assert_eq!(status, 200, "{answer}");
        assert_eq!(stops_held(holder), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_stop_where_no_lease_is_held_is_refused_with_the_reason_and_writes_nothing() {
        let fleet = fleet(2, &format!("https://github.com/{REPO}"), on()).await;
        let (first, second) = (&fleet[0], &fleet[1]);
        building_run(first, 1);
        // No machine is believed to hold the lease.
        let (status, answer) = second.post(&stop_path(), None).await;
        assert_eq!(status, 409, "{answer}");
        assert!(
            answer["error"].as_str().unwrap().contains("lease"),
            "{answer}"
        );
        // A carried stop is answered where it lands, and never carried on: the machine believed
        // to hold the lease, which does hold it, is not asked.
        second.write_status(holding(1, 1));
        second.hold(LeaseState::Held { generation: 1 }, Arc::new(AlwaysHeld(1)));
        until(|| first.statuses().len() == 1).await;
        let (status, body) = first
            .send("POST", &stop_path(), None, &[(CARRIED, "1")])
            .await;
        assert_eq!(status, 409, "{body}");
        assert!(body.contains("lease"), "{body}");
        assert_eq!(stops_held(second), 0, "the carried stop went no further");
        // A lease whose tick did not hold, or whose guard fails, holds nothing.
        first.hold(
            LeaseState::HeldBy {
                machine: second.me,
                generation: 1,
            },
            Arc::new(AlwaysHeld(1)),
        );
        let (status, _) = first
            .send("POST", &stop_path(), None, &[(CARRIED, "1")])
            .await;
        assert_eq!(status, 409);
        first.hold(
            LeaseState::Held { generation: 1 },
            Arc::new(crate::factory::worker::Lost),
        );
        let (status, _) = first
            .send("POST", &stop_path(), None, &[(CARRIED, "1")])
            .await;
        assert_eq!(status, 409);
        assert_eq!(stops_held(first), 0);
        assert_eq!(stops_held(second), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_stop_is_never_carried_to_a_machine_whose_status_does_not_hold() {
        let fleet = fleet(2, &format!("https://github.com/{REPO}"), on()).await;
        let (coordinator, asked) = (&fleet[0], &fleet[1]);
        building_run(coordinator, 1);
        // The believed status is the coordinator's, and it says another machine holds the
        // lease: nobody believed holds it, so the stop goes nowhere.
        coordinator.write_status(not_holding(MachineId::random(), 4));
        until(|| asked.statuses().len() == 1).await;
        let (status, answer) = asked.post(&stop_path(), None).await;
        assert_eq!(status, 409, "{answer}");
        assert!(
            answer["error"]
                .as_str()
                .unwrap()
                .contains("no machine is known to hold the lease"),
            "{answer}"
        );
        assert_eq!(stops_held(coordinator), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_stop_of_a_run_that_is_not_building_or_has_no_ledger_is_refused() {
        let fleet = fleet(1, &format!("https://github.com/{REPO}"), on()).await;
        let holder = &fleet[0];
        holder.hold(LeaseState::Held { generation: 1 }, Arc::new(AlwaysHeld(1)));
        let (status, answer) = holder.post(&stop_path(), None).await;
        assert_eq!(status, 404, "no ledger: {answer}");
        holder
            .ledger(1)
            .write(
                &run_id(),
                &Event::Claimed {
                    machine: "machine-0".into(),
                    github_user: "softfac-bot".into(),
                },
                &Cause::new(CauseStep::Pass, "claimed"),
            )
            .unwrap();
        let (status, answer) = holder.post(&stop_path(), None).await;
        assert_eq!(status, 409, "{answer}");
        assert!(
            answer["error"]
                .as_str()
                .unwrap()
                .contains("is not building"),
            "{answer}"
        );
        let (status, _) = holder.post("/api/factory/runs/nonsense/stop", None).await;
        assert_eq!(status, 404);
        assert_eq!(stops_held(holder), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_stop_reaches_the_holder_of_its_runs_repo_and_not_the_coordinator_or_another_projects_holder()
     {
        // The other project is listed first, so only the run's repo picks its holder.
        let projects = [
            (other_project(), "https://github.com/rawphp/other", on()),
            (project_id(), "https://github.com/rawphp/softfac", on()),
        ];
        let fleet = fleet_of(3, &projects).await;
        let (coordinator, asked, holder) = (&fleet[0], &fleet[1], &fleet[2]);
        // The coordinator holds the other project's lease; the third machine holds the run's.
        coordinator.hold_of(
            other_project(),
            "rawphp/other",
            LeaseState::Held { generation: 7 },
            Arc::new(AlwaysHeld(7)),
        );
        coordinator.write_status(FactoryStatus {
            project: other_project(),
            ..holding(7, 1)
        });
        holder.hold(LeaseState::Held { generation: 3 }, Arc::new(AlwaysHeld(3)));
        holder.write_status(holding(3, 1));
        building_run(holder, 3);
        until(|| asked.statuses().len() == 1 && asked.statuses_of(other_project()).len() == 1)
            .await;
        let (status, answer) = asked.post(&stop_path(), None).await;
        assert_eq!(status, 200, "{answer}");
        let events = holder.store.lock().unwrap().run_events(&run_id()).unwrap();
        let stop = events.last().unwrap();
        assert_eq!(stop.event, "stop_requested");
        assert_eq!(stop.generation, 3, "written by the run's holder");
        assert_eq!(stops_held(holder), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_stop_finds_the_lease_of_a_repo_whose_url_has_capitals() {
        let fleet = fleet(2, "https://github.com/RawPHP/Softfac", on()).await;
        let (holder, other) = (&fleet[0], &fleet[1]);
        // The service names the repo as its URL does; a run id folds it.
        holder.hold_of(
            project_id(),
            "RawPHP/Softfac",
            LeaseState::Held { generation: 2 },
            Arc::new(AlwaysHeld(2)),
        );
        holder.write_status(holding(2, 1));
        building_run(holder, 2);
        until(|| other.statuses().len() == 1).await;
        let (status, answer) = other.post(&stop_path(), None).await;
        assert_eq!(status, 200, "carried to the holder: {answer}");
        let (status, answer) = holder.post(&stop_path(), None).await;
        assert_eq!(status, 200, "on the holder: {answer}");
        assert_eq!(answer["already"], json!(true), "{answer}");
        assert_eq!(stops_held(holder), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_stop_on_a_machine_whose_own_status_holds_but_whose_lease_lapsed_is_refused() {
        let fleet = fleet(1, &format!("https://github.com/{REPO}"), on()).await;
        let alone = &fleet[0];
        alone.write_status(holding(1, 1));
        building_run(alone, 1);
        let (status, answer) = alone.post(&stop_path(), None).await;
        assert_eq!(status, 409, "{answer}");
        assert!(
            answer["error"].as_str().unwrap().contains("no longer"),
            "{answer}"
        );
        assert_eq!(stops_held(alone), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn turning_a_project_on_again_with_no_login_takes_the_coordinators_default_now() {
        let held = ProjectFactory {
            on: false,
            login: "old-bot".into(),
            paused: false,
        };
        let fleet = fleet(2, &format!("https://github.com/{REPO}"), Some(held)).await;
        let (coordinator, other) = (&fleet[0], &fleet[1]);
        coordinator
            .secrets
            .set(GITHUB_DEFAULT, "coord-bot")
            .unwrap();
        let (status, answer) = other
            .post(
                &project_path(),
                Some(json!({ "on": true, "paused": false })),
            )
            .await;
        assert_eq!(status, 200, "{answer}");
        let wanted = ProjectFactory {
            on: true,
            login: "coord-bot".into(),
            paused: false,
        };
        until(|| factory_of(other).as_ref() == Some(&wanted)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_project_already_on_keeps_taking_edits_and_only_turning_on_checks_its_url() {
        let ssh = fleet(1, "git@github.com:rawphp/softfac.git", on()).await;
        let alone = &ssh[0];
        let (status, answer) = alone
            .post(
                &project_path(),
                Some(json!({ "on": true, "paused": true, "login": "softfac-bot" })),
            )
            .await;
        assert_eq!(status, 200, "{answer}");
        assert_eq!(
            factory_of(alone),
            Some(ProjectFactory {
                paused: true,
                ..on().unwrap()
            })
        );
        // A URL that names no repo is no GitHub repo.
        let bare = fleet(1, "https://github.com/", None).await;
        let (status, answer) = bare[0]
            .post(
                &project_path(),
                Some(json!({ "on": true, "paused": false, "login": "softfac-bot" })),
            )
            .await;
        assert_eq!(status, 422, "{answer}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_turn_on_carried_to_a_machine_that_does_not_act_is_refused_and_sent_nowhere() {
        let fleet = fleet(2, &format!("https://github.com/{REPO}"), None).await;
        let (coordinator, other) = (&fleet[0], &fleet[1]);
        coordinator
            .secrets
            .set(GITHUB_DEFAULT, "coord-bot")
            .unwrap();
        other.secrets.set(GITHUB_DEFAULT, "other-bot").unwrap();
        let (status, said) = other
            .send(
                "POST",
                &project_path(),
                Some(json!({ "on": true, "paused": false })),
                &[(CARRIED, "1")],
            )
            .await;
        assert_eq!(status, 503, "{said}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        for machine in [coordinator, other] {
            assert_eq!(factory_of(machine), None);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_write_is_refused_before_it_is_made_when_the_secret_store_cannot_be_read() {
        let fleet = fleet(1, &format!("https://github.com/{REPO}"), on()).await;
        let alone = &fleet[0];
        alone.hold(LeaseState::Held { generation: 1 }, Arc::new(AlwaysHeld(1)));
        building_run(alone, 1);
        alone.secrets.readable(false);
        let (status, said) = alone
            .post("/api/factory/settings", Some(fleet_body(|_| {})))
            .await;
        assert_eq!(status, 503, "{said}");
        let (status, said) = alone
            .post(
                &project_path(),
                Some(json!({ "on": true, "paused": true, "login": "softfac-bot" })),
            )
            .await;
        assert_eq!(status, 503, "{said}");
        let (status, said) = alone.post(&stop_path(), None).await;
        assert_eq!(status, 503, "{said}");
        assert_eq!(alone.settings().factory, None);
        assert_eq!(factory_of(alone), on());
        assert_eq!(stops_held(alone), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_turn_on_that_needs_the_default_is_503_while_the_coordinators_store_cannot_be_read() {
        let fleet = fleet(2, &format!("https://github.com/{REPO}"), None).await;
        let (coordinator, other) = (&fleet[0], &fleet[1]);
        coordinator
            .secrets
            .set(GITHUB_DEFAULT, "coord-bot")
            .unwrap();
        coordinator.secrets.readable(false);
        for asked in [other, coordinator] {
            let (status, answer) = asked
                .post(
                    &project_path(),
                    Some(json!({ "on": true, "paused": false })),
                )
                .await;
            assert_eq!(status, 503, "{answer}");
        }
        // The read of the default itself fails: still 503, never "no default login".
        coordinator.secrets.readable_for(0);
        let (status, answer) = coordinator
            .post(
                &project_path(),
                Some(json!({ "on": true, "paused": false })),
            )
            .await;
        assert_eq!(status, 503, "{answer}");
        assert_eq!(factory_of(coordinator), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_write_made_before_the_store_stopped_reading_is_200_with_its_strings_unreadable() {
        let fleet = fleet(1, &format!("https://github.com/{REPO}"), None).await;
        let alone = &fleet[0];
        // The store reads for the check before the write, and not for the answer after it.
        let body = fleet_body(|body| {
            body.insert("model".into(), json!(TOKEN));
        });
        alone.secrets.readable_for(1);
        let (status, said) = alone
            .send("POST", "/api/factory/settings", Some(body), &[])
            .await;
        assert_eq!(status, 200, "{said}");
        assert!(!said.contains(TOKEN), "{said}");
        let answer: Value = serde_json::from_str(&said).unwrap();
        assert_eq!(
            answer["settings"]["model"],
            json!(crate::secrets::HIDDEN_UNREADABLE)
        );
        assert_eq!(
            alone.settings().factory.map(|f| f.model),
            Some(TOKEN.to_owned()),
            "the edit was made"
        );
        alone.secrets.readable_for(1);
        let (status, said) = alone
            .send(
                "POST",
                &project_path(),
                Some(json!({ "on": true, "paused": false, "login": "softfac-bot" })),
                &[],
            )
            .await;
        assert_eq!(status, 200, "{said}");
        let answer: Value = serde_json::from_str(&said).unwrap();
        assert_eq!(
            answer["factory"]["login"],
            json!(crate::secrets::HIDDEN_UNREADABLE)
        );
        assert_eq!(
            factory_of(alone).map(|f| f.login),
            Some("softfac-bot".into())
        );

        // A stop recorded on the holder, the store failing after the check.
        alone.hold(LeaseState::Held { generation: 1 }, Arc::new(AlwaysHeld(1)));
        building_run(alone, 1);
        alone.secrets.readable_for(1);
        let (status, said) = alone.send("POST", &stop_path(), None, &[]).await;
        assert_eq!(status, 200, "{said}");
        let answer: Value = serde_json::from_str(&said).unwrap();
        assert_eq!(answer["stopped"], json!(true), "{answer}");
        assert_eq!(answer["message"], json!(crate::secrets::HIDDEN_UNREADABLE));
        assert_eq!(stops_held(alone), 1, "the stop was recorded");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_answer_is_refused_whole_when_the_secret_store_cannot_be_read() {
        let fleet = fleet(1, &format!("https://github.com/{REPO}"), on()).await;
        let alone = &fleet[0];
        alone.write_status(holding(1, 1));
        building_run(alone, 1);
        alone.secrets.readable(false);
        for path in [
            "/api/factory".to_owned(),
            format!("/api/factory/runs/{}", run_id()),
        ] {
            let (status, body) = alone.send("GET", &path, None, &[]).await;
            assert_eq!(status, 503, "{path}: {body}");
            assert!(!body.contains(&project_id().to_string()), "{body}");
        }
    }

    // ----- POST /api/projects/{id}/factory -----

    fn project_path() -> String {
        format!("/api/projects/{}/factory", project_id())
    }

    fn factory_of(machine: &Served) -> Option<ProjectFactory> {
        machine
            .settings()
            .project_factory
            .get(&project_id())
            .cloned()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn turning_on_a_project_whose_repo_is_not_on_github_over_https_is_422_with_the_reason() {
        for repo in [
            "git@github.com:rawphp/softfac.git",
            "ssh://git@github.com/rawphp/softfac.git",
            "https://gitlab.com/rawphp/softfac",
            "http://github.com/rawphp/softfac",
        ] {
            let fleet = fleet(2, repo, None).await;
            let (alone, other) = (&fleet[0], &fleet[1]);
            alone.secrets.set(GITHUB_DEFAULT, "coord-bot").unwrap();
            // With a login named, and with none, which takes the coordinator's default: asked
            // of the coordinator, and carried to it.
            for (asked, body) in [
                (
                    alone,
                    json!({ "on": true, "paused": false, "login": "softfac-bot" }),
                ),
                (alone, json!({ "on": true, "paused": false })),
                (other, json!({ "on": true, "paused": false })),
            ] {
                let (status, answer) = asked.post(&project_path(), Some(body.clone())).await;
                assert_eq!(status, 422, "{repo} {body}: {answer}");
                let said = answer["error"].as_str().unwrap();
                assert!(said.contains("https://github.com/"), "{said}");
            }
            assert_eq!(factory_of(alone), None, "{repo}: nothing was sent");
            // Off, or a pause, is no turning on.
            let (status, answer) = alone
                .post(
                    &project_path(),
                    Some(json!({ "on": false, "paused": true })),
                )
                .await;
            assert_eq!(status, 200, "{repo}: {answer}");
            assert_eq!(
                factory_of(alone),
                Some(ProjectFactory {
                    on: false,
                    login: String::new(),
                    paused: true,
                })
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn turning_on_with_no_login_takes_the_acting_coordinators_default() {
        let fleet = fleet(2, &format!("https://github.com/{REPO}"), None).await;
        let (coordinator, other) = (&fleet[0], &fleet[1]);
        coordinator
            .secrets
            .set(GITHUB_DEFAULT, "coord-bot")
            .unwrap();
        other.secrets.set(GITHUB_DEFAULT, "other-bot").unwrap();
        let (status, answer) = other
            .post(
                &project_path(),
                Some(json!({ "on": true, "paused": false })),
            )
            .await;
        assert_eq!(status, 200, "{answer}");
        let wanted = ProjectFactory {
            on: true,
            login: "coord-bot".into(),
            paused: false,
        };
        assert_eq!(answer["factory"], serde_json::to_value(&wanted).unwrap());
        until(|| factory_of(other).as_ref() == Some(&wanted)).await;
        assert_eq!(factory_of(coordinator), Some(wanted));

        // A login named is written as named, through the coordinator from any machine.
        let (status, answer) = other
            .post(
                &project_path(),
                Some(json!({ "on": true, "paused": true, "login": "named-bot" })),
            )
            .await;
        assert_eq!(status, 200, "{answer}");
        let wanted = ProjectFactory {
            on: true,
            login: "named-bot".into(),
            paused: true,
        };
        until(|| factory_of(coordinator).as_ref() == Some(&wanted)).await;
        // A project that is on acts as its login: an edit of it that names none is refused,
        // from any machine, and changes nothing.
        for login in [json!(null), json!(""), json!("  ")] {
            for asked in [other, coordinator] {
                let (status, answer) = asked
                    .post(
                        &project_path(),
                        Some(json!({ "on": true, "paused": false, "login": login })),
                    )
                    .await;
                assert_eq!(status, 422, "{login}: {answer}");
                assert!(
                    answer["error"].as_str().unwrap().contains("login"),
                    "{answer}"
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(factory_of(coordinator), Some(wanted));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_blank_login_names_none_and_takes_the_default_when_turning_on() {
        let fleet = fleet(2, &format!("https://github.com/{REPO}"), None).await;
        let (coordinator, other) = (&fleet[0], &fleet[1]);
        coordinator
            .secrets
            .set(GITHUB_DEFAULT, "coord-bot")
            .unwrap();
        let (status, answer) = other
            .post(
                &project_path(),
                Some(json!({ "on": true, "paused": false, "login": "  " })),
            )
            .await;
        assert_eq!(status, 200, "{answer}");
        let wanted = ProjectFactory {
            on: true,
            login: "coord-bot".into(),
            paused: false,
        };
        until(|| factory_of(other).as_ref() == Some(&wanted)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn turning_on_with_no_login_where_the_coordinator_has_no_default_is_refused() {
        let fleet = fleet(2, &format!("https://github.com/{REPO}"), None).await;
        let (coordinator, other) = (&fleet[0], &fleet[1]);
        // Only a machine that is not the coordinator names a default: it is not used.
        other.secrets.set(GITHUB_DEFAULT, "other-bot").unwrap();
        let (status, answer) = other
            .post(
                &project_path(),
                Some(json!({ "on": true, "paused": false })),
            )
            .await;
        assert_eq!(status, 422, "{answer}");
        assert!(
            answer["error"].as_str().unwrap().contains("login"),
            "{answer}"
        );
        assert_eq!(factory_of(coordinator), None);
        assert_eq!(factory_of(other), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_project_edit_that_is_not_what_the_route_takes_is_refused() {
        let fleet = fleet(1, &format!("https://github.com/{REPO}"), None).await;
        let alone = &fleet[0];
        for (body, status) in [
            (
                json!({ "on": true, "paused": false, "login": "bad/login" }),
                422,
            ),
            (
                json!({ "on": true, "paused": false, "login": "x".repeat(40) }),
                422,
            ),
            (json!({ "on": true, "paused": false, "login": 7 }), 422),
            (json!({ "on": "yes", "paused": false }), 422),
            (json!({ "on": true }), 422),
            (json!({ "paused": true }), 422),
            (
                json!({ "on": false, "paused": false, "colour": "red" }),
                422,
            ),
            (json!(["on"]), 422),
        ] {
            let (said, answer) = alone.post(&project_path(), Some(body.clone())).await;
            assert_eq!(said, status, "{body}: {answer}");
        }
        let (status, _) = alone.send("POST", &project_path(), None, &[]).await;
        assert_eq!(status, 400, "no JSON");
        let unknown = ProjectId(uuid::Uuid::from_u128(0x99));
        let (status, _) = alone
            .post(
                &format!("/api/projects/{unknown}/factory"),
                Some(json!({ "on": false, "paused": false })),
            )
            .await;
        assert_eq!(status, 404);
        assert_eq!(factory_of(alone), None);
    }

    // ----- POST /api/factory/settings -----

    /// The fleet's default factory settings as a body, changed by `change`.
    fn fleet_body(change: impl FnOnce(&mut serde_json::Map<String, Value>)) -> Value {
        let mut body = serde_json::to_value(FactorySettings::default()).unwrap();
        change(body.as_object_mut().unwrap());
        body
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_fleets_settings_go_through_the_coordinator_whole_and_a_value_out_of_range_is_refused()
     {
        let fleet = fleet(2, &format!("https://github.com/{REPO}"), None).await;
        let (coordinator, other) = (&fleet[0], &fleet[1]);
        let body = fleet_body(|body| {
            body.insert("paused".into(), json!(true));
            body.insert("max_runs".into(), json!(5));
        });
        let (status, answer) = other.post("/api/factory/settings", Some(body)).await;
        assert_eq!(status, 200, "{answer}");
        let wanted = FactorySettings {
            paused: true,
            max_runs: 5,
            ..FactorySettings::default()
        };
        assert_eq!(answer["settings"], serde_json::to_value(&wanted).unwrap());
        until(|| coordinator.settings().factory.as_ref() == Some(&wanted)).await;
        let wanted = FactorySettings {
            poll_seconds: 15,
            ..wanted
        };
        let (status, _) = coordinator
            .post(
                "/api/factory/settings",
                Some(serde_json::to_value(&wanted).unwrap()),
            )
            .await;
        assert_eq!(status, 200);
        until(|| other.settings().factory.as_ref() == Some(&wanted)).await;

        let set = |name: &str, value: Value| {
            let name = name.to_owned();
            fleet_body(move |body| {
                body.insert(name, value);
            })
        };
        for body in [
            set("poll_seconds", json!(5)),
            set("poll_seconds", json!(61)),
            set("max_runs", json!(-1)),
            set("model", json!("-x")),
            set("effort", json!("two words")),
            set("colour", json!("red")),
            fleet_body(|body| {
                body.remove("model");
            }),
            json!({ "paused": true }),
        ] {
            let (status, answer) = other
                .post("/api/factory/settings", Some(body.clone()))
                .await;
            assert_eq!(status, 422, "{body}: {answer}");
        }
        let (_, answer) = other
            .post("/api/factory/settings", Some(set("poll_seconds", json!(5))))
            .await;
        let said = answer["error"].as_str().unwrap();
        assert!(said.contains("10") && said.contains("60"), "{said}");
        let (_, answer) = other
            .post(
                "/api/factory/settings",
                Some(fleet_body(|body| {
                    body.remove("model");
                })),
            )
            .await;
        let said = answer["error"].as_str().unwrap();
        assert!(said.contains("model"), "{said}");
        assert_eq!(other.settings().factory, Some(wanted));
    }

    // ----- secrets -----

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn no_answer_holds_a_token() {
        let fleet = fleet(2, &format!("https://github.com/{REPO}"), on()).await;
        let (holder, other) = (&fleet[0], &fleet[1]);
        other.secrets.set("github:softfac-bot", TOKEN).unwrap();
        // A writer that did not hide what it wrote.
        holder.write_status(FactoryStatus {
            idle: Some(format!("gh said: bad credentials {TOKEN}")),
            ..holding(1, 1)
        });
        holder.hold(LeaseState::Held { generation: 1 }, Arc::new(AlwaysHeld(1)));
        building_run(holder, 1);
        holder
            .ledger(1)
            .write(
                &run_id(),
                &Event::StepChanged {
                    step: format!("pushing with {TOKEN}"),
                },
                &Cause::new(CauseStep::Pass, "test"),
            )
            .unwrap();
        // The status and each ledger event reach the other machine as records of their own,
        // so both answers there wait for the step that holds the token.
        until(|| other.statuses().len() == 1).await;
        until(|| {
            other
                .store
                .lock()
                .unwrap()
                .run_events(&run_id())
                .unwrap()
                .len()
                == 4
        })
        .await;
        for machine in [holder, other] {
            let answers = [
                machine.send("GET", "/api/factory", None, &[]).await,
                machine
                    .send("GET", &format!("/api/factory/runs/{}", run_id()), None, &[])
                    .await,
            ];
            for (status, body) in answers {
                assert_eq!(status, 200, "{body}");
                assert!(!body.contains(TOKEN), "{body}");
                assert!(body.contains("***"), "{body}");
            }
        }
    }
}

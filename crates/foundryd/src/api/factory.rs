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
//!   reason}` with why nothing was sent, and `now`, the count, when it rose.
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

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use foundry_proto::ids::{MachineId, ProjectId, SessionId, TerminalId, WorkspaceId};
use foundry_proto::state::SessionState;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::carry::{self, Owner, Worker};
use super::{Api, ApiError, created, error, no_route, on_store, unprocessable};
use crate::factory::messages::{BODY_LIMIT, LINE_LIMIT, Message};
use crate::git_auth::NoToken;
use crate::secrets::redact;
use crate::sessions::SessionError;
use crate::sessions::factory::{FactoryStart, SendRefused};
use crate::sessions::turns::TurnText;
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
            let running = sessions.factory_sessions(factory.project);
            let records = on_store(&store, move |store| store.sessions_of(me)).await?;
            let found = records.into_iter().find(|record| {
                record.body.workspace == id
                    && running.contains(&record.body.id)
                    && !matches!(
                        record.body.state,
                        SessionState::Finished | SessionState::Failed
                    )
            });
            if let Some(record) = found {
                return Ok(Started::Running(record.body.id));
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

/// `409` with why nothing was sent: the reason as text, as a word the caller can match, the
/// count of prompts when it rose, and, for `failed`, the count the send read before it typed
/// (`null` when it read none), which a resend names as `only_if_prompts`.
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
        tracing::warn!("api: a factory message was not sent: {text}");
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
    let runs = on_store(&api.store, move |store| store.sessions_of(me))
        .await?
        .iter()
        .any(|record| {
            record.body.id == id
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
    fn a_tail_is_cut_to_its_last_bytes_on_a_character_boundary() {
        let text = format!("é{}", "x".repeat(10));
        assert_eq!(last_bytes(&text, 11), "x".repeat(10));
        assert_eq!(last_bytes(&text, 12), text);
        assert_eq!(last_bytes("short", TAIL_LIMIT), "short");
    }
}

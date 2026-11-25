//! The factory's routes: the worker routes carried between machines, and the Factory view's.
//!
//! The worker routes are what a run asks of the machine that holds its workspace or session.
//! Each is a `POST` that answers only a call another machine carried here, marked
//! [`CARRIED`]; any other request, a browser's included, is `404` as for no route, before its
//! body is read. Called on a machine that does not own what it names, a route is carried to its
//! owner by [`carry`](super::carry), and arrives there marked. A carried call for something this
//! machine does not own is refused as P1's carried routes refuse one: `404`.
//!
//! - `POST /api/factory/workspaces` `{machine, project, name, checkout, setup, tag}`: makes a
//!   workspace on `machine` with
//!   [`Workspaces::create_with`](crate::workspaces::Workspaces::create_with), `201 {id}`. A
//!   second create with the same `tag` and `name` answers with the first workspace. A workspace
//!   made whose setup could not be started is still `201`, with an `error` beside the `id`.
//! - `POST /api/workspaces/{id}/factory/session` `{project, login, model, effort, message}`:
//!   starts a factory session in the workspace on `message`, `201 {id}`. A login this machine
//!   holds no token for is `422`.
//! - `POST /api/workspaces/{id}/factory/end`: ends each factory session of the workspace,
//!   `204`. A forced archive is P1's carried archive route.
//! - `POST /api/sessions/{id}/factory/message` `{message, only_if_prompts}`: sends `message` to
//!   the session, `200 {prompts}` with its count of prompts before the send, or `409 {error,
//!   reason}` with why nothing was sent, and `now`, the count, when it rose.
//! - `POST /api/sessions/{id}/factory/turn` `{turn}`: `200 {message, question}`, the turn's
//!   final message and the question that waits, or `404` while the turn is not written.
//! - `POST /api/terminals/{id}/tail`: the end of the terminal's output as plain text, at most
//!   [`TAIL_LIMIT`] bytes.
//!
//! A message is `{line, body}`: one line under [`LINE_LIMIT`] characters with no control
//! character, and a body of at most [`BODY_LIMIT`] bytes with no NUL, or `null`. Anything else is
//! `422`, and nothing is typed.
//!
//! No answer holds a token: a session's token is only in its child's environment, a turn's
//! message is kept with known secrets hidden, and errors and the tail go through [`redact`].

use axum::Json;
use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use foundry_proto::ids::{MachineId, ProjectId, SessionId, TerminalId, WorkspaceId};
use serde::Deserialize;

use super::{Api, ApiError, Body, created, error, no_route, on_store, parse_id};
use crate::factory::messages::{BODY_LIMIT, LINE_LIMIT, Message};
use crate::git_auth::NoToken;
use crate::secrets::redact;
use crate::sessions::SessionError;
use crate::sessions::factory::{FactoryStart, SendRefused};
use crate::sync::call::CARRIED;
use crate::workspaces::{Checkout, FactoryTag, Setup, WorkspaceError};

/// The most bytes of a terminal's tail answered.
pub const TAIL_LIMIT: usize = 64 * 1024;

/// The factory's routes, merged into [`super::router`].
pub fn router() -> Router<Api> {
    Router::new().merge(worker())
}

/// The routes a run's calls take, answered only when carried.
fn worker() -> Router<Api> {
    Router::new()
        .route("/api/factory/workspaces", post(create).fallback(no_route))
        .route(
            "/api/workspaces/{id}/factory/session",
            post(start).fallback(no_route),
        )
        .route(
            "/api/workspaces/{id}/factory/end",
            post(end).fallback(no_route),
        )
        .route(
            "/api/sessions/{id}/factory/message",
            post(message).fallback(no_route),
        )
        .route(
            "/api/sessions/{id}/factory/turn",
            post(turn).fallback(no_route),
        )
        .route("/api/terminals/{id}/tail", post(tail).fallback(no_route))
        .route_layer(middleware::from_fn(only_carried))
}

/// Lets through only a request marked [`CARRIED`]; any other is answered as no route.
async fn only_carried(request: Request, next: Next) -> Response {
    if request.headers().contains_key(CARRIED) {
        next.run(request).await
    } else {
        no_route().await
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

fn unprocessable(message: String) -> ApiError {
    ApiError::Rejected(StatusCode::UNPROCESSABLE_ENTITY, message)
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

async fn create(State(api): State<Api>, Body(create): Body<Create>) -> Result<Response, ApiError> {
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

async fn start(
    State(api): State<Api>,
    Path(id): Path<String>,
    Body(start): Body<Start>,
) -> Result<Response, ApiError> {
    if let Some(said) = &api.sessions_refused {
        return Ok(error(StatusCode::SERVICE_UNAVAILABLE, said.clone()));
    }
    let id = parse_id(&id, "workspace", WorkspaceId)?;
    one_word("a model", &start.model)?;
    one_word("an effort", &start.effort)?;
    let first = start.message.checked()?;
    let factory = FactoryStart {
        project: start.project,
        login: start.login,
        model: start.model,
        effort: start.effort,
    };
    let sessions = api.sessions.clone();
    let session = api
        .workspaces
        .start_in(id, move |worktree| async move {
            sessions
                .start_factory(id, &worktree, factory, first)
                .await
                .map_err(start_refused)
        })
        .await?;
    Ok(created(session))
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

async fn end(State(api): State<Api>, Path(id): Path<String>) -> Result<Response, ApiError> {
    let id = parse_id(&id, "workspace", WorkspaceId)?;
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

async fn message(
    State(api): State<Api>,
    Path(id): Path<String>,
    Body(send): Body<Sending>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id, "session", SessionId)?;
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

/// `409` with why nothing was sent: the reason as text, as a word the caller can match, and
/// the count of prompts when it rose.
fn not_sent(refused: &SendRefused) -> Response {
    let reason = match refused {
        SendRefused::NotIdle => "not_idle",
        SendRefused::CountRose { .. } => "count_rose",
        SendRefused::NoSuchSession => "no_such_session",
        SendRefused::BodyWithoutPath => "body_without_path",
        SendRefused::SecretsUnreadable => "secrets_unreadable",
        SendRefused::Failed(_) => "failed",
    };
    let text = redact(&refused.to_string());
    if matches!(
        refused,
        SendRefused::SecretsUnreadable | SendRefused::Failed(_)
    ) {
        tracing::warn!("api: a factory message was not sent: {text}");
    }
    let mut answer = serde_json::json!({ "error": text, "reason": reason });
    if let SendRefused::CountRose { now } = refused {
        answer["now"] = (*now).into();
    }
    (StatusCode::CONFLICT, Json(answer)).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Turn {
    turn: u64,
}

async fn turn(
    State(api): State<Api>,
    Path(id): Path<String>,
    Body(asked): Body<Turn>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id, "session", SessionId)?;
    let Some(text) = api.sessions.turn_text(id, asked.turn).await else {
        return Err(ApiError::NotFound(format!(
            "no turn {} of a factory session {id} here",
            asked.turn
        )));
    };
    Ok(
        Json(serde_json::json!({ "message": text.message, "question": text.question }))
            .into_response(),
    )
}

// --- terminals ---

async fn tail(State(api): State<Api>, Path(id): Path<String>) -> Result<Response, ApiError> {
    let id = parse_id(&id, "terminal", TerminalId)?;
    let text = redact(&api.terminals.tail(id, TAIL_LIMIT).await?);
    // Hiding a URL's password can lengthen the text, as `a@` becomes `***@`.
    let text = last_bytes(&text, TAIL_LIMIT).to_owned();
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        text,
    )
        .into_response())
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
    fn every_refused_send_is_409_with_its_reason_and_the_count_when_it_rose() {
        let cases = [
            (SendRefused::NotIdle, "not_idle"),
            (SendRefused::CountRose { now: 7 }, "count_rose"),
            (SendRefused::NoSuchSession, "no_such_session"),
            (SendRefused::BodyWithoutPath, "body_without_path"),
            (SendRefused::SecretsUnreadable, "secrets_unreadable"),
            (
                SendRefused::Failed("https://u:secret@example.com/x".into()),
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

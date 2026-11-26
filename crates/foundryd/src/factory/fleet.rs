//! The real worker, over local calls and peer calls.
//!
//! [`FleetWorker`] is the [`Worker`] the daemon's passes use. Each call is a `POST` to one of the
//! worker routes of task 18 through [`api::factory::call`], which answers it here, by the same
//! code a carried call runs, when this machine owns what the call names, and otherwise carries
//! it to the owner over [`Peers::call`](crate::sync::Peers::call). A forced archive is P1's
//! archive route, `POST /api/workspaces/{id}/archive` `{"force": true}`: run here through
//! [`Workspaces::archive`](crate::workspaces::Workspaces::archive) for a workspace this machine
//! owns, else carried to the owner the workspace's record names.
//!
//! The trait is blocking, as the passes run on blocking threads: each call waits on the
//! runtime the worker was made on. A call that could not reach its machine, or whose owner is
//! not known here yet, is [`WorkerError::Unreachable`], which a run asks again next pass; a
//! machine that answered with a refusal is [`WorkerError::Failed`], with the reason the answer
//! gives. A turn not written yet while its session runs (`404`) is unreachable for now; one
//! that will never be written (`410`) is [`WorkerError::NotWritten`], which is what the resume
//! step reads as an interruption. Any other refusal of a turn read, a `5xx` included, is failed.
//! No answer holds a token, and every reason is passed through
//! [`redact`](crate::secrets::redact) before it is kept.

use foundry_proto::ids::{MachineId, SessionId, TerminalId, WorkspaceId};
use serde_json::Value;

use super::messages::Message;
use super::worker::{CreateWorkspace, Worker, WorkerError};
use crate::api::{self, Api};
use crate::secrets::redact;
use crate::sessions::factory::{FactoryStart, SendRefused};
use crate::sessions::turns::TurnText;
use crate::sync::call::{Answer, CallError};

/// The worker that calls the fleet.
#[derive(Clone)]
pub struct FleetWorker {
    api: Api,
    runtime: tokio::runtime::Handle,
}

impl FleetWorker {
    /// The worker over `api`, whose calls wait on `runtime`. Call it from a blocking thread.
    pub fn new(api: Api, runtime: tokio::runtime::Handle) -> Self {
        Self { api, runtime }
    }
}

/// What a call that was not answered by its owner is to a run: one that never reached the
/// machine, or may not have, is unreachable and asked again next pass; a machine that is not in
/// the list, or runs a Foundry too old for the call, will not take it.
fn not_answered(error: &CallError) -> WorkerError {
    let said = redact(&error.to_string());
    match error {
        CallError::NotListed(_) | CallError::Older(_) => WorkerError::Failed(said),
        CallError::Offline(_) | CallError::Down(_) | CallError::Lost(_) | CallError::Refused(_) => {
            WorkerError::Unreachable(said)
        }
    }
}

/// The reason an answer gives: its `error` when it is JSON with one, else its text, with the
/// passwords of URLs hidden.
fn reason(answer: &Answer) -> String {
    let text = String::from_utf8_lossy(&answer.body);
    let said = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| text.trim().to_owned());
    let said = if said.is_empty() {
        format!("the machine answered {}", answer.status)
    } else {
        said
    };
    redact(&said)
}

fn success(answer: &Answer) -> bool {
    (200..300).contains(&answer.status)
}

fn json(answer: &Answer) -> Result<Value, WorkerError> {
    serde_json::from_slice(&answer.body)
        .map_err(|e| WorkerError::Failed(format!("the machine's answer is not JSON: {e}")))
}

/// The id a create or a session start answered with. A create whose setup could not be started
/// is still made, and answers its id beside an `error`.
fn created_id(answer: &Answer) -> Result<uuid::Uuid, WorkerError> {
    if !success(answer) {
        return Err(WorkerError::Failed(reason(answer)));
    }
    json(answer)?
        .get("id")
        .and_then(Value::as_str)
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
        .ok_or_else(|| WorkerError::Failed("the machine's answer names no id".into()))
}

/// What a send's answer says: the count of prompts before the send, or why it was not sent. A
/// call that did not reach the machine, or an answer that is no refusal of the route's, is
/// failed with no count, so nothing guards a resend by one.
fn sent(answer: Result<Answer, CallError>) -> Result<u64, SendRefused> {
    let failed = |reason: String| SendRefused::Failed {
        reason,
        prompts: None,
    };
    let answer = answer.map_err(|error| failed(redact(&error.to_string())))?;
    let value: Option<Value> = serde_json::from_slice(&answer.body).ok();
    let field = |name: &str| {
        value
            .as_ref()
            .and_then(|v| v.get(name))
            .and_then(Value::as_u64)
    };
    if success(&answer) {
        return field("prompts")
            .ok_or_else(|| failed("the machine's answer names no count".into()));
    }
    if answer.status != 409 {
        return Err(failed(reason(&answer)));
    }
    let said = value
        .as_ref()
        .and_then(|v| v.get("reason"))
        .and_then(Value::as_str);
    Err(match said {
        Some("not_idle") => SendRefused::NotIdle,
        Some("count_rose") => match field("now") {
            Some(now) => SendRefused::CountRose { now },
            None => failed(reason(&answer)),
        },
        Some("no_such_session") => SendRefused::NoSuchSession,
        Some("body_without_path") => SendRefused::BodyWithoutPath,
        Some("secrets_unreadable") => SendRefused::SecretsUnreadable,
        Some("failed") => SendRefused::Failed {
            reason: reason(&answer),
            prompts: field("prompts"),
        },
        _ => failed(reason(&answer)),
    })
}

/// What a turn read's answer says. `404` is a turn not written yet while its session runs: asked
/// again. `410` is a turn that will never be written. Any other refusal, a `5xx` included, is
/// failed.
fn turn_read(answer: &Answer) -> Result<TurnText, WorkerError> {
    if answer.status == 404 {
        return Err(WorkerError::Unreachable(reason(answer)));
    }
    if answer.status == 410 {
        return Err(WorkerError::NotWritten(reason(answer)));
    }
    if !success(answer) {
        return Err(WorkerError::Failed(reason(answer)));
    }
    let value = json(answer)?;
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| WorkerError::Failed("the machine's answer has no message".into()))?
        .to_owned();
    let question = value
        .get("question")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(TurnText { message, question })
}

/// An answer with no body to read: done when 2xx, else failed with the owner's words, which a
/// run reads (an archive's "is not a live workspace of this machine" counts as archived).
fn done(answer: &Answer) -> Result<(), WorkerError> {
    if success(answer) {
        Ok(())
    } else {
        Err(WorkerError::Failed(reason(answer)))
    }
}

fn message_json(message: &Message) -> Value {
    serde_json::json!({ "line": message.line, "body": message.body })
}

/// A call to a worker route: its path and its body, with the fields the route takes.
#[derive(Debug, PartialEq)]
struct Request {
    path: String,
    body: Option<Value>,
}

fn create_request(machine: MachineId, request: &CreateWorkspace) -> Request {
    Request {
        path: "/api/factory/workspaces".into(),
        body: Some(serde_json::json!({
            "machine": machine,
            "project": request.project,
            "name": request.name,
            "checkout": request.checkout,
            "setup": request.setup,
            "tag": request.tag,
        })),
    }
}

fn start_request(workspace: WorkspaceId, start: &FactoryStart, first: &Message) -> Request {
    Request {
        path: format!("/api/workspaces/{workspace}/factory/session"),
        body: Some(serde_json::json!({
            "project": start.project,
            "login": start.login,
            "model": start.model,
            "effort": start.effort,
            "message": message_json(first),
        })),
    }
}

fn send_request(session: SessionId, message: &Message, only_if_prompts: Option<u64>) -> Request {
    Request {
        path: format!("/api/sessions/{session}/factory/message"),
        body: Some(serde_json::json!({
            "message": message_json(message),
            "only_if_prompts": only_if_prompts,
        })),
    }
}

fn turn_request(session: SessionId, n: u64) -> Request {
    Request {
        path: format!("/api/sessions/{session}/factory/turn"),
        body: Some(serde_json::json!({ "turn": n })),
    }
}

fn tail_request(terminal: TerminalId) -> Request {
    Request {
        path: format!("/api/terminals/{terminal}/tail"),
        body: None,
    }
}

fn end_request(workspace: WorkspaceId) -> Request {
    Request {
        path: format!("/api/workspaces/{workspace}/factory/end"),
        body: None,
    }
}

/// Where a forced archive of a workspace is run, from the owner its record names.
#[derive(Debug, PartialEq, Eq)]
enum ArchiveAt {
    /// The record has not reached this machine: asked again next pass, never taken as done.
    NotKnown,
    /// This machine owns the workspace: P1's archive runs here.
    Here,
    /// Carried to the owner.
    Owner(MachineId),
}

fn archive_at(owner: Option<MachineId>, me: MachineId) -> ArchiveAt {
    match owner {
        None => ArchiveAt::NotKnown,
        Some(owner) if owner == me => ArchiveAt::Here,
        Some(owner) => ArchiveAt::Owner(owner),
    }
}

/// P1's archive route, forced, as a carried call to the workspace's owner sends it.
fn archive_request(workspace: WorkspaceId) -> Request {
    Request {
        path: format!("/api/workspaces/{workspace}/archive"),
        body: Some(serde_json::json!({ "force": true })),
    }
}

impl FleetWorker {
    /// A `POST` of `body` to the worker route `path`, answered by its owner.
    fn call(&self, request: Request) -> Result<Answer, CallError> {
        let bytes = request
            .body
            .map(|b| b.to_string().into_bytes())
            .unwrap_or_default();
        self.runtime
            .block_on(api::factory::call(&self.api, &request.path, &bytes))
    }

    fn answered(&self, request: Request) -> Result<Answer, WorkerError> {
        self.call(request).map_err(|error| not_answered(&error))
    }
}

impl Worker for FleetWorker {
    fn create(
        &self,
        machine: MachineId,
        request: CreateWorkspace,
    ) -> Result<WorkspaceId, WorkerError> {
        let answer = self.answered(create_request(machine, &request))?;
        created_id(&answer).map(WorkspaceId)
    }

    fn start_session(
        &self,
        workspace: WorkspaceId,
        start: FactoryStart,
        first: Message,
    ) -> Result<SessionId, WorkerError> {
        let answer = self.answered(start_request(workspace, &start, &first))?;
        created_id(&answer).map(SessionId)
    }

    fn send(
        &self,
        session: SessionId,
        message: Message,
        only_if_prompts: Option<u64>,
    ) -> Result<u64, SendRefused> {
        sent(self.call(send_request(session, &message, only_if_prompts)))
    }

    fn turn(&self, session: SessionId, n: u64) -> Result<TurnText, WorkerError> {
        let answer = self.answered(turn_request(session, n))?;
        turn_read(&answer)
    }

    fn tail(&self, terminal: TerminalId) -> Result<String, WorkerError> {
        let answer = self.answered(tail_request(terminal))?;
        if !success(&answer) {
            return Err(WorkerError::Failed(reason(&answer)));
        }
        let text = String::from_utf8_lossy(&answer.body);
        let mut at = text.len().saturating_sub(api::factory::TAIL_LIMIT);
        while !text.is_char_boundary(at) {
            at += 1;
        }
        Ok(text[at..].to_owned())
    }

    fn end_sessions(&self, workspace: WorkspaceId) -> Result<(), WorkerError> {
        let answer = self.answered(end_request(workspace))?;
        done(&answer)
    }

    fn archive(&self, workspace: WorkspaceId) -> Result<(), WorkerError> {
        let api = self.api.clone();
        self.runtime.block_on(async move {
            let store = api.store.clone();
            let owner = tokio::task::spawn_blocking(move || {
                store
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .workspace(workspace)
                    .map(|record| record.map(|record| record.owner))
            })
            .await
            .map_err(|e| WorkerError::Failed(format!("the store read did not finish: {e}")))?
            .map_err(|e| {
                WorkerError::Failed(redact(&format!("the store could not be read: {e}")))
            })?;
            match archive_at(owner, api.peers.me()) {
                ArchiveAt::NotKnown => Err(WorkerError::Unreachable(format!(
                    "the record of workspace {workspace} has not reached this machine yet"
                ))),
                ArchiveAt::Here => api
                    .workspaces
                    .archive(workspace, true)
                    .await
                    .map_err(|e| WorkerError::Failed(redact(&e.to_string()))),
                ArchiveAt::Owner(owner) => {
                    let request = archive_request(workspace);
                    let body = request.body.map(|b| b.to_string()).unwrap_or_default();
                    let answer = api
                        .peers
                        .call(
                            owner,
                            &request.path,
                            Some("application/json"),
                            body.as_bytes(),
                        )
                        .await
                        .map_err(|error| not_answered(&error))?;
                    done(&answer)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Bytes;

    use super::*;

    fn answer(status: u16, body: &str) -> Answer {
        Answer {
            status,
            body: Bytes::copy_from_slice(body.as_bytes()),
        }
    }

    #[test]
    fn a_forced_archive_runs_here_for_this_machines_workspace_and_is_carried_to_any_other_owner() {
        let me = MachineId(uuid::Uuid::from_u128(1));
        let other = MachineId(uuid::Uuid::from_u128(2));
        assert_eq!(archive_at(Some(me), me), ArchiveAt::Here);
        assert_eq!(archive_at(Some(other), me), ArchiveAt::Owner(other));
        assert_eq!(
            archive_at(None, me),
            ArchiveAt::NotKnown,
            "a workspace whose record has not come is not taken as archived"
        );
    }

    #[test]
    fn a_call_that_never_reached_its_machine_is_unreachable_and_one_it_refused_is_failed() {
        for error in [
            CallError::Offline("mac".into()),
            CallError::Down("mac".into()),
            CallError::Lost("mac".into()),
            CallError::Refused("the owner is not known yet".into()),
        ] {
            assert!(
                matches!(not_answered(&error), WorkerError::Unreachable(_)),
                "{error}"
            );
        }
        for error in [
            CallError::NotListed(MachineId(uuid::Uuid::from_u128(7))),
            CallError::Older("mac".into()),
        ] {
            assert!(
                matches!(not_answered(&error), WorkerError::Failed(_)),
                "{error}"
            );
        }
    }

    #[test]
    fn a_create_or_a_start_answers_its_id_also_beside_an_error() {
        let id = uuid::Uuid::from_u128(0x55);
        assert_eq!(
            created_id(&answer(201, &format!(r#"{{"id":"{id}"}}"#))).unwrap(),
            id
        );
        assert_eq!(
            created_id(&answer(201, &format!(r#"{{"id":"{id}","error":"setup"}}"#))).unwrap(),
            id
        );
        assert_eq!(
            created_id(&answer(200, &format!(r#"{{"id":"{id}"}}"#))).unwrap(),
            id
        );
        let refused = created_id(&answer(422, r#"{"error":"no token for x"}"#)).unwrap_err();
        assert_eq!(refused, WorkerError::Failed("no token for x".into()));
        assert!(matches!(
            created_id(&answer(201, "not json")),
            Err(WorkerError::Failed(_))
        ));
    }

    #[test]
    fn a_send_answers_the_count_or_the_refusal_with_its_reason() {
        assert_eq!(sent(Ok(answer(200, r#"{"prompts":3}"#))), Ok(3));
        assert_eq!(
            sent(Ok(answer(409, r#"{"error":"x","reason":"not_idle"}"#))),
            Err(SendRefused::NotIdle)
        );
        assert_eq!(
            sent(Ok(answer(
                409,
                r#"{"error":"x","reason":"count_rose","now":5}"#
            ))),
            Err(SendRefused::CountRose { now: 5 })
        );
        assert_eq!(
            sent(Ok(answer(
                409,
                r#"{"error":"x","reason":"no_such_session"}"#
            ))),
            Err(SendRefused::NoSuchSession)
        );
        assert_eq!(
            sent(Ok(answer(
                409,
                r#"{"error":"x","reason":"body_without_path"}"#
            ))),
            Err(SendRefused::BodyWithoutPath)
        );
        assert_eq!(
            sent(Ok(answer(
                409,
                r#"{"error":"x","reason":"secrets_unreadable"}"#
            ))),
            Err(SendRefused::SecretsUnreadable)
        );
        assert_eq!(
            sent(Ok(answer(
                409,
                r#"{"error":"typed?","reason":"failed","prompts":2}"#
            ))),
            Err(SendRefused::Failed {
                reason: "typed?".into(),
                prompts: Some(2)
            })
        );
        // A call that did not reach the machine typed nothing that a count could guard.
        assert!(matches!(
            sent(Err(CallError::Lost("mac".into()))),
            Err(SendRefused::Failed { prompts: None, .. })
        ));
        assert!(matches!(
            sent(Ok(answer(500, "boom"))),
            Err(SendRefused::Failed { prompts: None, .. })
        ));
    }

    #[test]
    fn a_turn_not_written_yet_is_asked_again_and_only_a_gone_one_is_not_written() {
        let read = turn_read(&answer(
            200,
            r#"{"message":"FACTORY-DONE: x","question":null}"#,
        ));
        assert_eq!(
            read.unwrap(),
            TurnText {
                message: "FACTORY-DONE: x".into(),
                question: None
            }
        );
        assert!(matches!(
            turn_read(&answer(404, r#"{"error":"not yet"}"#)),
            Err(WorkerError::Unreachable(_))
        ));
        assert_eq!(
            turn_read(&answer(410, r#"{"error":"will not be written"}"#)),
            Err(WorkerError::NotWritten("will not be written".into()))
        );
        for status in [500, 502, 503] {
            assert_eq!(
                turn_read(&answer(status, r#"{"error":"store"}"#)),
                Err(WorkerError::Failed("store".into())),
                "{status}"
            );
        }
    }

    #[test]
    fn an_end_or_an_archive_is_done_on_2xx_and_keeps_the_owner_s_words_otherwise() {
        assert_eq!(done(&answer(204, "")), Ok(()));
        let said = r#"{"error":"workspace x is not a live workspace of this machine"}"#;
        assert_eq!(
            done(&answer(404, said)),
            Err(WorkerError::Failed(
                "workspace x is not a live workspace of this machine".into()
            ))
        );
        assert_eq!(reason(&answer(500, "plain words")), "plain words");
    }

    #[test]
    fn a_reason_has_the_password_of_a_url_hidden() {
        let said = reason(&answer(
            500,
            "fetch https://user:hunter2@github.com/o/r failed",
        ));
        assert!(!said.contains("hunter2"), "{said}");
    }
    #[test]
    fn each_call_names_its_worker_route_and_the_fields_the_route_takes() {
        use crate::workspaces::{Checkout, FactoryTag, Setup};
        let id = uuid::Uuid::from_u128(0x55);
        let machine = MachineId(uuid::Uuid::from_u128(0x2));
        let keys = |request: &Request| -> Vec<String> {
            let mut keys: Vec<String> = request
                .body
                .as_ref()
                .and_then(Value::as_object)
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            keys.sort();
            keys
        };
        let create = create_request(
            machine,
            &CreateWorkspace {
                project: foundry_proto::ids::ProjectId(id),
                name: "fac/5-1".into(),
                checkout: Checkout::Commit {
                    sha: "0123456789abcdef0123456789abcdef01234567".into(),
                },
                setup: Setup::None,
                tag: FactoryTag {
                    project: foundry_proto::ids::ProjectId(id),
                    run: "rawphp__softfac__5__1".into(),
                },
            },
        );
        assert_eq!(create.path, "/api/factory/workspaces");
        assert_eq!(
            keys(&create),
            ["checkout", "machine", "name", "project", "setup", "tag"]
        );
        assert_eq!(
            create.body.as_ref().unwrap()["machine"],
            machine.to_string()
        );
        let message = Message {
            line: "/factory-build o/r#5 {body}".into(),
            body: Some("brief".into()),
        };
        let start = start_request(
            WorkspaceId(id),
            &FactoryStart {
                project: foundry_proto::ids::ProjectId(id),
                login: "bot".into(),
                model: "opus".into(),
                effort: "high".into(),
            },
            &message,
        );
        assert_eq!(start.path, format!("/api/workspaces/{id}/factory/session"));
        assert_eq!(
            keys(&start),
            ["effort", "login", "message", "model", "project"]
        );
        assert_eq!(
            start.body.as_ref().unwrap()["message"],
            serde_json::json!({"line": message.line, "body": "brief"})
        );
        let send = send_request(SessionId(id), &message, Some(3));
        assert_eq!(send.path, format!("/api/sessions/{id}/factory/message"));
        assert_eq!(keys(&send), ["message", "only_if_prompts"]);
        assert_eq!(send.body.as_ref().unwrap()["only_if_prompts"], 3);
        let turn = turn_request(SessionId(id), 4);
        assert_eq!(turn.path, format!("/api/sessions/{id}/factory/turn"));
        assert_eq!(turn.body, Some(serde_json::json!({"turn": 4})));
        assert_eq!(
            tail_request(TerminalId(id)),
            Request {
                path: format!("/api/terminals/{id}/tail"),
                body: None
            }
        );
        assert_eq!(
            end_request(WorkspaceId(id)),
            Request {
                path: format!("/api/workspaces/{id}/factory/end"),
                body: None
            }
        );
        assert_eq!(
            archive_request(WorkspaceId(id)),
            Request {
                path: format!("/api/workspaces/{id}/archive"),
                body: Some(serde_json::json!({"force": true}))
            }
        );
    }
}

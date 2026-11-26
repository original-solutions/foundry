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
//! that will never be written (`410`) is failed, which is what the resume step reads as an
//! interruption. No answer holds a token, and every reason is passed through
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
/// again. `410`, a turn that will never be written, and any other refusal are failed.
fn turn_read(answer: &Answer) -> Result<TurnText, WorkerError> {
    if answer.status == 404 {
        return Err(WorkerError::Unreachable(reason(answer)));
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

impl FleetWorker {
    /// A `POST` of `body` to the worker route `path`, answered by its owner.
    fn call(&self, path: &str, body: Option<Value>) -> Result<Answer, CallError> {
        let bytes = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
        self.runtime
            .block_on(api::factory::call(&self.api, path, &bytes))
    }

    fn answered(&self, path: &str, body: Option<Value>) -> Result<Answer, WorkerError> {
        self.call(path, body).map_err(|error| not_answered(&error))
    }
}

impl Worker for FleetWorker {
    fn create(
        &self,
        machine: MachineId,
        request: CreateWorkspace,
    ) -> Result<WorkspaceId, WorkerError> {
        let body = serde_json::json!({
            "machine": machine,
            "project": request.project,
            "name": request.name,
            "checkout": request.checkout,
            "setup": request.setup,
            "tag": request.tag,
        });
        let answer = self.answered("/api/factory/workspaces", Some(body))?;
        created_id(&answer).map(WorkspaceId)
    }

    fn start_session(
        &self,
        workspace: WorkspaceId,
        start: FactoryStart,
        first: Message,
    ) -> Result<SessionId, WorkerError> {
        let body = serde_json::json!({
            "project": start.project,
            "login": start.login,
            "model": start.model,
            "effort": start.effort,
            "message": message_json(&first),
        });
        let answer = self.answered(
            &format!("/api/workspaces/{workspace}/factory/session"),
            Some(body),
        )?;
        created_id(&answer).map(SessionId)
    }

    fn send(
        &self,
        session: SessionId,
        message: Message,
        only_if_prompts: Option<u64>,
    ) -> Result<u64, SendRefused> {
        let body = serde_json::json!({
            "message": message_json(&message),
            "only_if_prompts": only_if_prompts,
        });
        sent(self.call(
            &format!("/api/sessions/{session}/factory/message"),
            Some(body),
        ))
    }

    fn turn(&self, session: SessionId, n: u64) -> Result<TurnText, WorkerError> {
        let answer = self.answered(
            &format!("/api/sessions/{session}/factory/turn"),
            Some(serde_json::json!({ "turn": n })),
        )?;
        turn_read(&answer)
    }

    fn tail(&self, terminal: TerminalId) -> Result<String, WorkerError> {
        let answer = self.answered(&format!("/api/terminals/{terminal}/tail"), None)?;
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
        let answer = self.answered(&format!("/api/workspaces/{workspace}/factory/end"), None)?;
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
            match owner {
                None => Err(WorkerError::Unreachable(format!(
                    "the record of workspace {workspace} has not reached this machine yet"
                ))),
                Some(owner) if owner == api.peers.me() => api
                    .workspaces
                    .archive(workspace, true)
                    .await
                    .map_err(|e| WorkerError::Failed(redact(&e.to_string()))),
                Some(owner) => {
                    let answer = api
                        .peers
                        .call(
                            owner,
                            &format!("/api/workspaces/{workspace}/archive"),
                            Some("application/json"),
                            br#"{"force":true}"#,
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
    fn a_turn_not_written_yet_is_asked_again_and_one_never_written_is_failed() {
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
            Err(WorkerError::Failed("will not be written".into()))
        );
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
}

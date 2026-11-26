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

/// What a call that was not answered by its owner is to a run.
fn not_answered(error: &CallError) -> WorkerError {
    let _ = error;
    todo!()
}

/// The reason an answer gives: its `error` when it is JSON with one, else its text.
fn reason(answer: &Answer) -> String {
    let _ = answer;
    todo!()
}

/// The id a create or a session start answered with.
fn created_id(answer: &Answer) -> Result<uuid::Uuid, WorkerError> {
    let _ = answer;
    todo!()
}

/// What a send's answer says.
fn sent(answer: Result<Answer, CallError>) -> Result<u64, SendRefused> {
    let _ = answer;
    todo!()
}

/// What a turn read's answer says.
fn turn_read(answer: &Answer) -> Result<TurnText, WorkerError> {
    let _ = answer;
    todo!()
}

/// An answer with no body to read: done when 2xx.
fn done(answer: &Answer) -> Result<(), WorkerError> {
    let _ = answer;
    todo!()
}

fn message_json(message: &Message) -> Value {
    serde_json::json!({ "line": message.line, "body": message.body })
}

impl Worker for FleetWorker {
    fn create(
        &self,
        machine: MachineId,
        request: CreateWorkspace,
    ) -> Result<WorkspaceId, WorkerError> {
        let _ = (machine, request, &self.api, &self.runtime, message_json, api::factory::TAIL_LIMIT);
        todo!()
    }

    fn start_session(
        &self,
        workspace: WorkspaceId,
        start: FactoryStart,
        first: Message,
    ) -> Result<SessionId, WorkerError> {
        let _ = (workspace, start, first);
        todo!()
    }

    fn send(
        &self,
        session: SessionId,
        message: Message,
        only_if_prompts: Option<u64>,
    ) -> Result<u64, SendRefused> {
        let _ = (session, message, only_if_prompts);
        todo!()
    }

    fn turn(&self, session: SessionId, n: u64) -> Result<TurnText, WorkerError> {
        let _ = (session, n);
        todo!()
    }

    fn tail(&self, terminal: TerminalId) -> Result<String, WorkerError> {
        let _ = terminal;
        todo!()
    }

    fn end_sessions(&self, workspace: WorkspaceId) -> Result<(), WorkerError> {
        let _ = workspace;
        todo!()
    }

    fn archive(&self, workspace: WorkspaceId) -> Result<(), WorkerError> {
        let _ = (workspace, redact);
        todo!()
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
            assert!(matches!(not_answered(&error), WorkerError::Failed(_)), "{error}");
        }
    }

    #[test]
    fn a_create_or_a_start_answers_its_id_also_beside_an_error() {
        let id = uuid::Uuid::from_u128(0x55);
        assert_eq!(created_id(&answer(201, &format!(r#"{{"id":"{id}"}}"#))).unwrap(), id);
        assert_eq!(
            created_id(&answer(201, &format!(r#"{{"id":"{id}","error":"setup"}}"#))).unwrap(),
            id
        );
        assert_eq!(created_id(&answer(200, &format!(r#"{{"id":"{id}"}}"#))).unwrap(), id);
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
            sent(Ok(answer(409, r#"{"error":"x","reason":"count_rose","now":5}"#))),
            Err(SendRefused::CountRose { now: 5 })
        );
        assert_eq!(
            sent(Ok(answer(409, r#"{"error":"x","reason":"no_such_session"}"#))),
            Err(SendRefused::NoSuchSession)
        );
        assert_eq!(
            sent(Ok(answer(409, r#"{"error":"x","reason":"body_without_path"}"#))),
            Err(SendRefused::BodyWithoutPath)
        );
        assert_eq!(
            sent(Ok(answer(409, r#"{"error":"x","reason":"secrets_unreadable"}"#))),
            Err(SendRefused::SecretsUnreadable)
        );
        assert_eq!(
            sent(Ok(answer(409, r#"{"error":"typed?","reason":"failed","prompts":2}"#))),
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
        let read = turn_read(&answer(200, r#"{"message":"FACTORY-DONE: x","question":null}"#));
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
        let said = reason(&answer(500, "fetch https://user:hunter2@github.com/o/r failed"));
        assert!(!said.contains("hunter2"), "{said}");
    }
}

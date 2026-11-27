//! Vims in memory, for tests.
//!
//! [`FakeVims`] answers each call from a script kept per command ([`FakeVims::on`]), in the
//! order scripted. A call nothing answers could not run. It keeps what it answered a call that
//! carries an idempotency key, as Vims keeps an operation by its key: a repeat of the key with
//! the same request is answered as the first call was, without spending the script, and the
//! key with a different request is refused with `IDEMPOTENCY_KEY_REUSED`. A reply can be
//! [`Reply::dropped`]: the call was taken and kept by its key, and its answer never came back.
//! `operation list` with a key, when nothing is scripted for it, finds what the key made. Its
//! answers go through [`finish`], so a scripted failure reads as the real `vims`'s would. Every
//! call is recorded, in order.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::{Value, json};

use super::cli::{Outcome, finish};
use super::{Call, Vims, VimsError};

/// What the fake answers one call with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    outcome: Outcome,
    dropped: bool,
}

impl Reply {
    /// Vims answered with `stdout`.
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self::exit(0, stdout, "")
    }

    /// Vims refused the call with `code`.
    pub fn refused(code: &str, message: &str) -> Self {
        let error = json!({"error": {"code": code, "message": message}});
        Self::exit(1, "", error.to_string())
    }

    /// `vims` exited with `status`, `stdout` and `stderr`.
    pub fn exit(status: i32, stdout: impl Into<String>, stderr: impl Into<String>) -> Self {
        Self::outcome(Outcome::Exit {
            status: Some(status),
            stdout: stdout.into(),
            stderr: stderr.into(),
        })
    }

    /// No answer within the limit, from a call Vims never took.
    pub fn timeout() -> Self {
        Self::outcome(Outcome::TimedOut)
    }

    /// `vims` could not run.
    pub fn not_run(reason: impl Into<String>) -> Self {
        Self::outcome(Outcome::NotStarted(reason.into()))
    }

    /// Any outcome.
    pub fn outcome(outcome: Outcome) -> Self {
        Self {
            outcome,
            dropped: false,
        }
    }

    /// The same reply taken by Vims and kept by the call's key, with the answer lost on its
    /// way back: the call itself gets no answer within the limit.
    pub fn dropped(mut self) -> Self {
        self.dropped = true;
        self
    }
}

/// Vims that answers from a script and records every call.
#[derive(Debug, Default)]
pub struct FakeVims {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// Replies for each command, each answered in order.
    script: HashMap<String, VecDeque<Reply>>,
    /// What each key was taken with, and what it was answered.
    taken: HashMap<String, (Call, Outcome)>,
    calls: Vec<Call>,
}

impl FakeVims {
    /// Vims with nothing scripted.
    pub fn new() -> Self {
        Self::default()
    }

    /// Answers the next call of `command`, such as `operation show`, with `reply`. Several
    /// replies for one command answer its calls in the order they were scripted.
    pub fn on(&self, command: &str, reply: Reply) -> &Self {
        self.lock()
            .script
            .entry(command.to_owned())
            .or_default()
            .push_back(reply);
        self
    }

    /// Every call so far, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    fn next(&mut self, command: &str) -> Option<Reply> {
        self.script.get_mut(command).and_then(VecDeque::pop_front)
    }

    /// The answer to `call`, or `None` when nothing answers it.
    fn answer(&mut self, call: &Call) -> Option<Outcome> {
        if let Some(key) = call.key() {
            if let Some((first, outcome)) = self.taken.get(key) {
                if first.same_request(call) {
                    return Some(outcome.clone());
                }
                return Some(
                    Reply::refused(
                        "IDEMPOTENCY_KEY_REUSED",
                        &format!("the key {key} was used with a different request"),
                    )
                    .outcome,
                );
            }
            let reply = self.next(call.command())?;
            // Vims keeps what it answered; a call that never reached it holds no key.
            if reply.dropped || matches!(reply.outcome, Outcome::Exit { .. }) {
                self.taken
                    .insert(key.to_owned(), (call.clone(), reply.outcome.clone()));
            }
            return Some(if reply.dropped {
                Outcome::TimedOut
            } else {
                reply.outcome
            });
        }
        if let Some(reply) = self.next(call.command()) {
            return Some(if reply.dropped {
                Outcome::TimedOut
            } else {
                reply.outcome
            });
        }
        (call.command() == "operation list").then(|| self.operations(call))
    }

    /// `operation list` answered from the keys taken: the operation the key in `call` made, or
    /// none.
    fn operations(&self, call: &Call) -> Outcome {
        let argv = call.argv();
        let key = argv
            .iter()
            .position(|arg| arg == "--idempotency-key")
            .and_then(|at| argv.get(at + 1));
        let found: Vec<Value> = key
            .and_then(|key| self.taken.get(key))
            .and_then(|(_, outcome)| match outcome {
                Outcome::Exit {
                    status: Some(0),
                    stdout,
                    ..
                } => serde_json::from_str(stdout).ok(),
                _ => None,
            })
            .into_iter()
            .collect();
        Reply::ok(Value::Array(found).to_string()).outcome
    }
}

impl Vims for FakeVims {
    fn call(&self, call: &Call) -> Result<String, VimsError> {
        let outcome = {
            let mut state = self.lock();
            state.calls.push(call.clone());
            state.answer(call)
        };
        let outcome = outcome.unwrap_or_else(|| {
            Outcome::NotStarted(format!("no answer scripted for `{}`", call.command()))
        });
        finish(call, outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::vims::{Egress, Exec, Limit, OpState, Outcome, Vims, VimsError};

    const NEW_OP: &str = r#"{"id":"op_1","status":"running","workspaceId":"wsp_1"}"#;

    fn exec(argv: &[&str], stdin: &[u8]) -> Exec {
        Exec {
            cwd: Some("/workspace/repo".into()),
            timeout_seconds: Some(1800),
            no_wait: true,
            argv: argv.iter().map(|part| part.to_string()).collect(),
            stdin: stdin.to_vec(),
        }
    }

    #[test]
    fn a_call_is_answered_by_the_script_for_its_command() {
        let fake = FakeVims::new();
        fake.on(
            "operation show",
            Reply::ok(r#"{"id":"op_9","status":"succeeded","result":{"exitCode":0}}"#),
        )
        .on("new", Reply::ok(NEW_OP));
        let shown = fake.operation("op_9").unwrap();
        assert_eq!(shown.id, "op_9");
        assert_eq!(shown.exit_code, Some(0));
        let made = fake
            .new_workspace("golden:abc", Egress::Open, "k1")
            .unwrap();
        assert_eq!(made.workspace.as_deref(), Some("wsp_1"));
    }

    #[test]
    fn answers_for_one_command_come_in_the_order_scripted() {
        let fake = FakeVims::new();
        fake.on(
            "operation show",
            Reply::ok(r#"{"id":"op_1","status":"running"}"#),
        )
        .on(
            "operation show",
            Reply::ok(r#"{"id":"op_1","status":"succeeded"}"#),
        );
        assert_eq!(fake.operation("op_1").unwrap().state, OpState::Running);
        assert_eq!(fake.operation("op_1").unwrap().state, OpState::Finished);
    }

    #[test]
    fn a_call_nothing_answers_could_not_run() {
        let fake = FakeVims::new();
        let error = fake.operation("op_1").unwrap_err();
        assert!(matches!(error, VimsError::NotRun { .. }), "{error:?}");
        assert!(error.to_string().contains("no answer scripted"), "{error}");
    }

    #[test]
    fn a_repeated_key_is_answered_with_the_first_operation_and_spends_no_script() {
        let fake = FakeVims::new();
        fake.on("new", Reply::ok(NEW_OP)).on(
            "new",
            Reply::ok(r#"{"id":"op_2","status":"running","workspaceId":"wsp_2"}"#),
        );
        let first = fake
            .new_workspace("golden:abc", Egress::Open, "k1")
            .unwrap();
        let again = fake
            .new_workspace("golden:abc", Egress::Open, "k1")
            .unwrap();
        assert_eq!(again, first);
        let next = fake
            .new_workspace("golden:abc", Egress::Open, "k2")
            .unwrap();
        assert_eq!(next.id, "op_2");
        assert_eq!(fake.calls().len(), 3);
    }

    #[test]
    fn a_reused_key_with_a_different_request_is_refused() {
        let fake = FakeVims::new();
        fake.on("new", Reply::ok(NEW_OP));
        fake.new_workspace("golden:abc", Egress::Open, "k1")
            .unwrap();
        let error = fake
            .new_workspace("golden:abc", Egress::Closed, "k1")
            .unwrap_err();
        let refusal = error.refusal().expect("a refusal");
        assert_eq!(refusal.code, "IDEMPOTENCY_KEY_REUSED");
        assert_eq!(refusal.limit, None);
        // The first request still holds the key.
        assert_eq!(
            fake.new_workspace("golden:abc", Egress::Open, "k1")
                .unwrap()
                .id,
            "op_1"
        );
    }

    #[test]
    fn standard_input_is_part_of_the_request_a_key_holds() {
        let fake = FakeVims::new();
        fake.on("exec", Reply::ok(r#"{"id":"op_5","status":"running"}"#));
        fake.exec("wsp_1", &exec(&["cat"], b"one"), "k1").unwrap();
        assert_eq!(
            fake.exec("wsp_1", &exec(&["cat"], b"one"), "k1")
                .unwrap()
                .id,
            "op_5"
        );
        let error = fake
            .exec("wsp_1", &exec(&["cat"], b"two"), "k1")
            .unwrap_err();
        assert_eq!(error.code(), Some("IDEMPOTENCY_KEY_REUSED"));
    }

    #[test]
    fn a_reused_key_with_another_command_or_argv_is_refused() {
        let fake = FakeVims::new();
        fake.on("pause", Reply::ok("{}"))
            .on("exec", Reply::ok(r#"{"id":"op_5","status":"running"}"#));
        fake.pause("wsp_1", "k1").unwrap();
        assert_eq!(
            fake.start("wsp_1", "k1").unwrap_err().code(),
            Some("IDEMPOTENCY_KEY_REUSED")
        );
        fake.exec("wsp_1", &exec(&["true"], b""), "k2").unwrap();
        assert_eq!(
            fake.exec("wsp_1", &exec(&["false"], b""), "k2")
                .unwrap_err()
                .code(),
            Some("IDEMPOTENCY_KEY_REUSED")
        );
    }

    #[test]
    fn a_failure_vims_never_answered_holds_no_key() {
        let fake = FakeVims::new();
        fake.on("new", Reply::exit(1, "", "fetch failed: ECONNRESET"))
            .on(
                "new",
                Reply::outcome(Outcome::Exit {
                    status: None,
                    stdout: String::new(),
                    stderr: String::new(),
                }),
            )
            .on("new", Reply::ok(NEW_OP));
        for _ in 0..2 {
            assert!(matches!(
                fake.new_workspace("golden:abc", Egress::Open, "k1"),
                Err(VimsError::Unknown { .. })
            ));
            assert_eq!(fake.operations_by_key("k1").unwrap(), vec![]);
        }
        assert_eq!(
            fake.new_workspace("golden:abc", Egress::Open, "k1")
                .unwrap()
                .id,
            "op_1"
        );
    }

    #[test]
    fn a_dropped_answer_is_no_answer_but_the_call_was_taken() {
        let fake = FakeVims::new();
        fake.on("new", Reply::ok(NEW_OP).dropped());
        let error = fake
            .new_workspace("golden:abc", Egress::Open, "k1")
            .unwrap_err();
        assert!(
            matches!(error, VimsError::Timeout { seconds: 30, .. }),
            "{error:?}"
        );
        let found = fake.operations_by_key("k1").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "op_1");
        assert_eq!(found[0].workspace.as_deref(), Some("wsp_1"));
        // Sent again with the same key, the call is answered with what Vims took.
        assert_eq!(
            fake.new_workspace("golden:abc", Egress::Open, "k1")
                .unwrap()
                .id,
            "op_1"
        );
    }

    #[test]
    fn a_key_never_sent_finds_no_operation() {
        let fake = FakeVims::new();
        fake.on("new", Reply::ok(NEW_OP));
        fake.new_workspace("golden:abc", Egress::Open, "k1")
            .unwrap();
        assert_eq!(fake.operations_by_key("k9").unwrap(), vec![]);
    }

    #[test]
    fn a_call_that_never_reached_vims_holds_no_key() {
        let fake = FakeVims::new();
        fake.on("new", Reply::timeout())
            .on("new", Reply::ok(NEW_OP));
        assert!(matches!(
            fake.new_workspace("golden:abc", Egress::Open, "k1"),
            Err(VimsError::Timeout { .. })
        ));
        assert_eq!(fake.operations_by_key("k1").unwrap(), vec![]);
        assert_eq!(
            fake.new_workspace("golden:abc", Egress::Open, "k1")
                .unwrap()
                .id,
            "op_1"
        );
    }

    #[test]
    fn a_refusal_is_kept_by_its_key_and_the_next_attempt_is_a_new_call() {
        let fake = FakeVims::new();
        fake.on(
            "new",
            Reply::refused("QUOTA_EXCEEDED", "no room for 4 vCPU"),
        )
        .on("new", Reply::ok(NEW_OP));
        let refused = fake
            .new_workspace("golden:abc", Egress::Open, "k1")
            .unwrap_err();
        assert_eq!(refused.refusal().unwrap().limit, Some(Limit::Quota));
        assert_eq!(
            fake.new_workspace("golden:abc", Egress::Open, "k1")
                .unwrap_err(),
            refused
        );
        assert_eq!(
            fake.new_workspace("golden:abc", Egress::Open, "k2")
                .unwrap()
                .id,
            "op_1"
        );
    }

    #[test]
    fn a_scripted_operation_list_answers_before_the_kept_operations() {
        let fake = FakeVims::new();
        fake.on("operation list", Reply::ok("[]"))
            .on("new", Reply::ok(NEW_OP).dropped());
        let _ = fake.new_workspace("golden:abc", Egress::Open, "k1");
        assert_eq!(fake.operations_by_key("k1").unwrap(), vec![]);
        assert_eq!(fake.operations_by_key("k1").unwrap().len(), 1);
    }

    #[test]
    fn every_call_is_recorded_in_order_with_its_input() {
        let fake = FakeVims::new();
        fake.on("exec", Reply::ok(r#"{"id":"op_5","status":"running"}"#));
        fake.exec("wsp_1", &exec(&["cat"], b"body"), "k1").unwrap();
        let _ = fake.show("wsp_1");
        let calls = fake.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].command(), "exec");
        assert_eq!(calls[0].stdin(), b"body");
        assert_eq!(calls[0].key(), Some("k1"));
        assert_eq!(calls[1].command(), "show");
        assert_eq!(calls[1].argv(), ["show", "wsp_1", "--json"]);
    }

    #[test]
    fn a_fake_is_vims_other_tasks_can_share() {
        let fake = std::sync::Arc::new(FakeVims::new());
        let vims: std::sync::Arc<dyn Vims> = fake.clone();
        fake.on("show", Reply::ok(r#"{"id":"wsp_1","status":"running"}"#));
        assert_eq!(vims.show("wsp_1").unwrap().id, "wsp_1");
        assert_eq!(fake.calls().len(), 1);
    }
}

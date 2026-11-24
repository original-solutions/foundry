//! The events a run's ledger holds.
//!
//! Ported from the runner's `src/ledger/events.ts`. Each event is a typed value that writes
//! the runner's `event` and `data`: the same names, the same fields in the same order, `null`
//! where the runner writes `null`, and a field the runner leaves out when absent left out here
//! too. The runner's `docs/reference/ledger.md` (softfac's) is the spec of both.
//!
//! The store keeps an event's `data` and cause as JSON objects whose keys are sorted (the
//! `run_event` record says so). The runner's key order is [`FIELDS`], [`RESUMED_FROM_FIELDS`],
//! [`CHECK_FIELDS`] and [`CAUSE_FIELDS`], and
//! [`read::runner_line`](super::read::runner_line) writes a stored event in it.
//!
//! A [`Cause`] says which part of the factory wrote an event and why. Its text is cleaned and
//! cut as the runner's `makeCause` and `causeObject` do, so no text from a comment, a prompt
//! or a session reaches a cause whole; the ledger hides secrets in it first.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use foundry_proto::factory::{PrEnd, RunOutcome as Outcome};

use crate::factory::lifecycle::State;

/// Which part of the factory wrote an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CauseStep {
    Start,
    Watch,
    Forward,
    Advance,
    Gate,
    Pass,
    Retro,
    Operator,
}

/// What an `error` event was about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ErrorKind {
    Github,
    Tracker,
    T3,
    Git,
    Gate,
    Ledger,
    Config,
}

/// One required check in a `checks_read` event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRead {
    pub name: String,
    pub state: String,
}

/// The earlier run and branch a launch carried on from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumedFrom {
    pub run: String,
    pub branch: String,
}

/// A provider option a launch sent with its model, such as a reasoning effort.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOption {
    pub id: String,
    pub value: OptionValue,
}

/// The value of a [`ModelOption`]: text or a flag.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OptionValue {
    Text(String),
    Flag(bool),
}

/// Something that happened in a run, written as the runner's `{ event, data }`. The variants
/// and their fields are in the runner's order, which is the order they are written in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum Event {
    Claimed {
        machine: String,
        github_user: String,
    },
    TriageResult {
        passed: bool,
        reasons: Vec<String>,
    },
    QuestionsPosted {
        questions: String,
    },
    AnswerReceived {
        comment_id: Option<String>,
        body_edited: bool,
    },
    SessionLaunched {
        thread_id: String,
        worktree: Option<String>,
        branch: Option<String>,
        model_instance: Option<String>,
        model: Option<String>,
        model_options: Option<Vec<ModelOption>>,
        resumed_from: Option<ResumedFrom>,
        /// The branch the run lands on. Left out when absent, as the runner's older lines are.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
        /// The issue's title at the launch. Left out when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    CommentForwarded {
        comment_id: String,
    },
    SessionInterrupted {
        resumes: u32,
        relaunch: bool,
        run_ordinal: Option<u32>,
        branch: Option<String>,
        t3_error: Option<String>,
        ended_in_pass: u64,
    },
    SessionRateLimited {
        run_ordinal: u32,
        branch: Option<String>,
        resumes: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    SessionPaused {
        run_ordinal: Option<u32>,
    },
    StopRequested {
        by: String,
    },
    StepChanged {
        step: String,
    },
    PrOpened {
        pr: u64,
        url: String,
    },
    PrLeft {
        pr: u64,
        url: String,
    },
    PrAdopted {
        pr: u64,
        branch: String,
        run: String,
    },
    BranchBound {
        pr: u64,
        branch: String,
    },
    PrSetDraft {
        pr: u64,
    },
    PrReadyOutside {
        pr: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
    GateCommand {
        attempt: u32,
        command: String,
        exit_code: i32,
        duration_ms: u64,
    },
    GateSentBack {
        attempt: u32,
        run_ordinal: u32,
    },
    GateCheckout {
        attempt: u32,
        dir: String,
        sha: String,
        base_sha: String,
    },
    GateFinished {
        attempt: u32,
        total_ms: u64,
    },
    GateRegate {
        sha: String,
        base_sha: String,
        previous_base_sha: String,
        /// GitHub said the pull request conflicts with its base. Written only when true.
        #[serde(default, skip_serializing_if = "is_false")]
        conflict: bool,
    },
    RegateRelaunching {
        attempt: u32,
        branch: String,
        message: String,
    },
    FeedbackRelaunching {
        n: u32,
        branch: String,
        items: Vec<String>,
        message: String,
    },
    FeedbackForwarding {
        n: u32,
        run_ordinal: Option<u32>,
        items: Vec<String>,
        message: String,
    },
    GateCleanupFailed {
        dir: String,
        error: String,
    },
    WorktreeRemoved {
        path: String,
    },
    WorktreeRemoveFailed {
        path: String,
        error: String,
    },
    ChecksRead {
        sha: String,
        checks: Vec<CheckRead>,
    },
    StateChanged {
        #[serde(with = "state_name")]
        from: State,
        #[serde(with = "state_name")]
        to: State,
    },
    Stuck {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commits_ahead: Option<u32>,
    },
    Error {
        step: CauseStep,
        kind: ErrorKind,
        message: String,
    },
    /// The run's last event. The ledger fills in `total_ms` and `gate_attempts` itself.
    Finished {
        outcome: Outcome,
        total_ms: u64,
        gate_attempts: u32,
        review_cycles: Option<u32>,
        pr_url: Option<String>,
        pr_end: Option<PrEnd>,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// A [`State`] written by its name, `needs-info`, as the runner writes a state.
mod state_name {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::factory::lifecycle::State;

    pub fn serialize<S: Serializer>(state: &State, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(state.name())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<State, D::Error> {
        let name = String::deserialize(d)?;
        State::from_name(&name)
            .ok_or_else(|| serde::de::Error::custom(format!("{name:?} is not a state")))
    }
}

/// The names of the events that the ledger's readers look for, each spelled once. [`Event::name`]
/// and [`FIELDS`] use them, so a rename reaches every reader.
pub mod names {
    pub const QUESTIONS_POSTED: &str = "questions_posted";
    pub const SESSION_LAUNCHED: &str = "session_launched";
    pub const STEP_CHANGED: &str = "step_changed";
    pub const PR_OPENED: &str = "pr_opened";
    pub const GATE_COMMAND: &str = "gate_command";
    pub const GATE_SENT_BACK: &str = "gate_sent_back";
    pub const CHECKS_READ: &str = "checks_read";
    pub const STATE_CHANGED: &str = "state_changed";
    pub const STUCK: &str = "stuck";
    pub const FINISHED: &str = "finished";
}

impl Event {
    /// The event's name, as it is written.
    pub const fn name(&self) -> &'static str {
        match self {
            Event::Claimed { .. } => "claimed",
            Event::TriageResult { .. } => "triage_result",
            Event::QuestionsPosted { .. } => names::QUESTIONS_POSTED,
            Event::AnswerReceived { .. } => "answer_received",
            Event::SessionLaunched { .. } => names::SESSION_LAUNCHED,
            Event::CommentForwarded { .. } => "comment_forwarded",
            Event::SessionInterrupted { .. } => "session_interrupted",
            Event::SessionRateLimited { .. } => "session_rate_limited",
            Event::SessionPaused { .. } => "session_paused",
            Event::StopRequested { .. } => "stop_requested",
            Event::StepChanged { .. } => names::STEP_CHANGED,
            Event::PrOpened { .. } => names::PR_OPENED,
            Event::PrLeft { .. } => "pr_left",
            Event::PrAdopted { .. } => "pr_adopted",
            Event::BranchBound { .. } => "branch_bound",
            Event::PrSetDraft { .. } => "pr_set_draft",
            Event::PrReadyOutside { .. } => "pr_ready_outside",
            Event::GateCommand { .. } => names::GATE_COMMAND,
            Event::GateSentBack { .. } => names::GATE_SENT_BACK,
            Event::GateCheckout { .. } => "gate_checkout",
            Event::GateFinished { .. } => "gate_finished",
            Event::GateRegate { .. } => "gate_regate",
            Event::RegateRelaunching { .. } => "regate_relaunching",
            Event::FeedbackRelaunching { .. } => "feedback_relaunching",
            Event::FeedbackForwarding { .. } => "feedback_forwarding",
            Event::GateCleanupFailed { .. } => "gate_cleanup_failed",
            Event::WorktreeRemoved { .. } => "worktree_removed",
            Event::WorktreeRemoveFailed { .. } => "worktree_remove_failed",
            Event::ChecksRead { .. } => names::CHECKS_READ,
            Event::StateChanged { .. } => names::STATE_CHANGED,
            Event::Stuck { .. } => names::STUCK,
            Event::Error { .. } => "error",
            Event::Finished { .. } => names::FINISHED,
        }
    }

    /// The event's name and its `data`, as the ledger stores them: `data` is a JSON object,
    /// whose keys the store keeps sorted. [`read::runner_line`](super::read::runner_line)
    /// writes them in the runner's order again.
    pub fn parts(&self) -> (&'static str, Value) {
        let data = match serde_json::to_value(self) {
            Ok(Value::Object(mut whole)) => whole.remove("data").unwrap_or(Value::Null),
            _ => unreachable!("an event is an object of strings, numbers, flags and lists"),
        };
        (self.name(), data)
    }

    /// The event a stored name and `data` hold, or `None` when this build does not know the
    /// name or the data is not that event's.
    pub fn from_parts(name: &str, data: &Value) -> Option<Event> {
        let whole = serde_json::json!({ "event": name, "data": data });
        serde_json::from_value(whole).ok()
    }
}

/// The 33 events, each with the names of its `data` fields in the order the runner writes them.
pub const FIELDS: [(&str, &[&str]); 33] = [
    ("claimed", &["machine", "github_user"]),
    ("triage_result", &["passed", "reasons"]),
    (names::QUESTIONS_POSTED, &["questions"]),
    ("answer_received", &["comment_id", "body_edited"]),
    (
        names::SESSION_LAUNCHED,
        &[
            "thread_id",
            "worktree",
            "branch",
            "model_instance",
            "model",
            "model_options",
            "resumed_from",
            "base",
            "title",
        ],
    ),
    ("comment_forwarded", &["comment_id"]),
    (
        "session_interrupted",
        &[
            "resumes",
            "relaunch",
            "run_ordinal",
            "branch",
            "t3_error",
            "ended_in_pass",
        ],
    ),
    (
        "session_rate_limited",
        &["run_ordinal", "branch", "resumes", "model"],
    ),
    ("session_paused", &["run_ordinal"]),
    ("stop_requested", &["by"]),
    (names::STEP_CHANGED, &["step"]),
    (names::PR_OPENED, &["pr", "url"]),
    ("pr_left", &["pr", "url"]),
    ("pr_adopted", &["pr", "branch", "run"]),
    ("branch_bound", &["pr", "branch"]),
    ("pr_set_draft", &["pr"]),
    ("pr_ready_outside", &["pr", "by"]),
    (
        names::GATE_COMMAND,
        &["attempt", "command", "exit_code", "duration_ms"],
    ),
    (names::GATE_SENT_BACK, &["attempt", "run_ordinal"]),
    ("gate_checkout", &["attempt", "dir", "sha", "base_sha"]),
    ("gate_finished", &["attempt", "total_ms"]),
    (
        "gate_regate",
        &["sha", "base_sha", "previous_base_sha", "conflict"],
    ),
    ("regate_relaunching", &["attempt", "branch", "message"]),
    ("feedback_relaunching", &["n", "branch", "items", "message"]),
    (
        "feedback_forwarding",
        &["n", "run_ordinal", "items", "message"],
    ),
    ("gate_cleanup_failed", &["dir", "error"]),
    ("worktree_removed", &["path"]),
    ("worktree_remove_failed", &["path", "error"]),
    (names::CHECKS_READ, &["sha", "checks"]),
    (names::STATE_CHANGED, &["from", "to"]),
    (names::STUCK, &["reason", "branch", "commits_ahead"]),
    ("error", &["step", "kind", "message"]),
    (
        names::FINISHED,
        &[
            "outcome",
            "total_ms",
            "gate_attempts",
            "review_cycles",
            "pr_url",
            "pr_end",
        ],
    ),
];

/// The fields of a `session_launched` event's `resumed_from`, in the runner's order.
pub const RESUMED_FROM_FIELDS: [&str; 2] = ["run", "branch"];

/// The fields of each check in a `checks_read` event, in the runner's order.
pub const CHECK_FIELDS: [&str; 2] = ["name", "state"];

/// The fields of a cause, in the runner's order.
pub const CAUSE_FIELDS: [&str; 6] = ["step", "reason", "actor", "command", "attempt", "label"];

/// The `data` fields of the event called `name` in the runner's order, or `None` for a name the
/// runner does not list.
pub fn fields(name: &str) -> Option<&'static [&'static str]> {
    FIELDS
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, fields)| *fields)
}

/// The longest `reason`, `actor` or `label` a cause keeps, in characters.
const CAUSE_FIELD_LIMIT: usize = 100;

/// The longest `command` a cause keeps, in characters.
const CAUSE_COMMAND_LIMIT: usize = 1_000;

/// Why an event was written: the part of the factory that wrote it, a reason code from the
/// runner's lists, and, only when they apply, who did something, which gate command, which
/// attempt and which label. None of it is text from a comment, a prompt or a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cause {
    pub step: CauseStep,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Cause {
    /// A cause with no extras.
    pub fn new(step: CauseStep, reason: impl Into<String>) -> Self {
        Self {
            step,
            reason: reason.into(),
            actor: None,
            command: None,
            attempt: None,
            label: None,
        }
    }

    /// Who did it. Kept as given; [`cleaned`](Self::cleaned) cuts it at 100 characters.
    pub fn actor(mut self, actor: &str) -> Self {
        self.actor = Some(actor.to_owned());
        self
    }

    /// Which gate command. Kept as given; [`cleaned`](Self::cleaned) cuts it at 1000
    /// characters.
    pub fn command(mut self, command: &str) -> Self {
        self.command = Some(command.to_owned());
        self
    }

    /// Which attempt.
    pub fn attempt(mut self, attempt: u32) -> Self {
        self.attempt = Some(attempt);
        self
    }

    /// Which label. Kept as given; [`cleaned`](Self::cleaned) cuts it at 100 characters.
    pub fn label(mut self, label: &str) -> Self {
        self.label = Some(label.to_owned());
        self
    }

    /// The cause as it is written: its text on one line with no control or bidi characters,
    /// and cut, so a cause cannot put session or comment text in the ledger whole. `reason`,
    /// `actor` and `label` are cut at 100 characters, `command` at 1000.
    pub fn cleaned(&self) -> Cause {
        self.cleaned_with(&|text: &str| text.to_owned())
    }

    /// [`cleaned`](Self::cleaned), with every text through `hide` once, as given, before it is
    /// stripped or cut, so neither can leave part of a secret that `hide` would no longer
    /// recognise. A control character, a space or a line break inside a secret is `hide`'s to
    /// see past (`secrets::hide_known` matches a held value across such gaps). What `hide` does
    /// not match across a gap, the strip can still join: with `hide_known`, a held value too
    /// short for it to match across gaps, or one split by a longer gap than it allows, broken
    /// only by characters the strip removes, is stored whole.
    pub fn cleaned_with(&self, hide: &dyn Fn(&str) -> String) -> Cause {
        let clean = |text: &str, limit: usize| -> String {
            strip(&hide(text)).chars().take(limit).collect()
        };
        let cut = |text: &Option<String>, limit| text.as_deref().map(|text| clean(text, limit));
        Cause {
            step: self.step,
            reason: clean(&self.reason, CAUSE_FIELD_LIMIT),
            actor: cut(&self.actor, CAUSE_FIELD_LIMIT),
            command: cut(&self.command, CAUSE_COMMAND_LIMIT),
            attempt: self.attempt,
            label: cut(&self.label, CAUSE_FIELD_LIMIT),
        }
    }
}

/// A character that must not reach a terminal as itself: C0 and C1 controls, DEL, and the bidi
/// controls that can make text read in another order than it is stored.
fn is_unsafe(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// `text` on one line with no control or bidi characters.
fn strip(text: &str) -> String {
    text.chars().filter(|c| !is_unsafe(*c)).collect()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::factory::ledger::testing::samples;

    /// The keys of `data` in the order `text` writes them.
    fn keys_in_order(text: &str) -> Vec<String> {
        // The data object is the last value of `{"event":..,"data":{..}}`, and every sample's
        // values are flat or nested one level, so its keys are those at depth 2.
        let mut keys = Vec::new();
        let mut depth = 0;
        let mut chars = text.char_indices().peekable();
        let mut in_string = false;
        let mut start = 0;
        let mut after_brace_or_comma = false;
        while let Some((i, c)) = chars.next() {
            if in_string {
                match c {
                    '\\' => {
                        chars.next();
                    }
                    '"' => {
                        in_string = false;
                        if depth == 2 && after_brace_or_comma {
                            keys.push(text[start..i].to_owned());
                        }
                        after_brace_or_comma = false;
                    }
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => {
                    in_string = true;
                    start = i + 1;
                }
                '{' | '[' => {
                    depth += 1;
                    after_brace_or_comma = c == '{';
                }
                '}' | ']' => depth -= 1,
                ',' => after_brace_or_comma = true,
                _ => {}
            }
        }
        keys
    }

    #[test]
    fn event_name_is_the_snake_case_type_and_data_holds_exactly_the_field_list_in_order() {
        for (event, data) in samples() {
            let text = serde_json::to_string(&event).unwrap();
            let (name, written) = event.parts();
            assert_eq!(text, format!(r#"{{"event":"{name}","data":{data}}}"#));
            assert_eq!(
                written,
                serde_json::from_str::<Value>(&data).unwrap(),
                "{name}"
            );
            let fields = fields(name).unwrap();
            // A field that is left out is not written; every other one is, in order.
            let expected: Vec<String> = fields
                .iter()
                .filter(|field| written.get(**field).is_some())
                .map(|field| (*field).to_owned())
                .collect();
            assert_eq!(keys_in_order(&text), expected, "{name}");
            assert_eq!(written.as_object().unwrap().len(), expected.len(), "{name}");
        }
    }

    #[test]
    fn append_writes_the_fields_in_the_doc_order_whatever_order_the_caller_built_the_object_in() {
        // The runner reorders a hand-built object; here the type has one order, and the nested
        // shapes keep the runner's order too.
        let event: Event = serde_json::from_value(json!({
            "event": "session_launched",
            "data": {
                "resumed_from": {"branch": "t3code/abc", "run": "r"},
                "model_options": null,
                "model": "m",
                "model_instance": "i",
                "branch": "b",
                "worktree": "w",
                "thread_id": "t",
            },
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"event":"session_launched","data":{"thread_id":"t","worktree":"w","branch":"b","model_instance":"i","model":"m","model_options":null,"resumed_from":{"run":"r","branch":"t3code/abc"}}}"#
        );
        let checks: Event = serde_json::from_value(json!({
            "event": "checks_read",
            "data": {"checks": [{"state": "passed", "name": "gate"}], "sha": "abc"},
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_string(&checks).unwrap(),
            r#"{"event":"checks_read","data":{"sha":"abc","checks":[{"name":"gate","state":"passed"}]}}"#
        );
        assert_eq!(RESUMED_FROM_FIELDS, ["run", "branch"]);
        assert_eq!(CHECK_FIELDS, ["name", "state"]);
    }

    #[test]
    fn every_event_reads_back_from_its_name_and_data_as_itself() {
        for (event, _) in samples() {
            let (name, data) = event.parts();
            assert_eq!(Event::from_parts(name, &data), Some(event), "{name}");
        }
        assert_eq!(Event::from_parts("merged", &json!({"sha": "abc"})), None);
        assert_eq!(Event::from_parts("claimed", &json!({"machine": 1})), None);
    }

    #[test]
    fn the_field_table_names_the_33_events_once_each() {
        assert_eq!(FIELDS.len(), 33);
        let mut names: Vec<&str> = FIELDS.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 33);
        assert_eq!(fields("finished").unwrap(), FIELDS[32].1);
        assert_eq!(fields("merged"), None);
        assert_eq!(
            CAUSE_FIELDS,
            ["step", "reason", "actor", "command", "attempt", "label"]
        );
    }

    #[test]
    fn a_cause_cleans_actor_and_label_of_control_and_bidi_characters_and_cuts_them_at_100_and_cuts_a_command_at_1000()
     {
        // The builders keep the text as given, so the ledger can hide a secret in it before
        // it is cut; the cause as written is `cleaned`.
        let actor = Cause::new(CauseStep::Start, "approved")
            .actor("al\u{0007}i\u{202e}ce\u{2066}\u{0000}\n\u{007f}\u{0085}")
            .cleaned();
        assert_eq!(actor.actor.as_deref(), Some("alice"));
        let label = Cause::new(CauseStep::Start, "approved")
            .label(&"x".repeat(150))
            .cleaned();
        assert_eq!(label.label, Some("x".repeat(100)));
        // Characters, not bytes.
        let emoji = Cause::new(CauseStep::Start, "approved")
            .actor(&"\u{1f600}".repeat(150))
            .cleaned();
        assert_eq!(emoji.actor.unwrap().chars().count(), 100);
        let command = Cause::new(CauseStep::Gate, "gate-command-ran")
            .command(&"c".repeat(1500))
            .cleaned();
        assert_eq!(command.command, Some("c".repeat(1000)));
        // A command is cleaned the same way as the others: one line, no control characters.
        assert_eq!(
            Cause::new(CauseStep::Gate, "gate-command-ran")
                .command("a\nb\tc")
                .cleaned()
                .command
                .as_deref(),
            Some("abc")
        );
        // Nothing else goes in a cause, and an unset extra is left out.
        assert_eq!(
            serde_json::to_string(&Cause::new(CauseStep::Pass, "claimed")).unwrap(),
            r#"{"step":"pass","reason":"claimed"}"#
        );
    }

    #[test]
    fn a_cause_is_hidden_before_it_is_cut() {
        let cause = Cause::new(CauseStep::Gate, "gate-command-ran")
            .command(&format!("{}SECRET-VALUE", "c".repeat(995)));
        let hide = |text: &str| text.replace("SECRET-VALUE", "***");
        assert_eq!(
            cause.cleaned_with(&hide).command,
            Some(format!("{}***", "c".repeat(995)))
        );
        assert_eq!(
            cause.cleaned().command,
            Some(format!("{}SECRE", "c".repeat(995)))
        );
    }

    #[test]
    fn a_cause_gives_each_text_to_its_hider_once_before_it_is_stripped_or_cut() {
        let seen = std::cell::RefCell::new(Vec::new());
        let hide = |text: &str| {
            seen.borrow_mut().push(text.to_owned());
            text.replace("SECRET\nVALUE", "***")
        };
        let long = format!("{}SECRET\nVALUE", "c".repeat(995));
        let cause = Cause::new(CauseStep::Gate, "a\u{7}b")
            .actor("SECRET\nVALUE")
            .command(&long);
        let cleaned = cause.cleaned_with(&hide);
        assert_eq!(cleaned.reason, "ab");
        assert_eq!(cleaned.actor.as_deref(), Some("***"));
        assert_eq!(cleaned.command, Some(format!("{}***", "c".repeat(995))));
        assert_eq!(
            *seen.borrow(),
            ["a\u{7}b".to_owned(), "SECRET\nVALUE".to_owned(), long]
        );
    }

    #[test]
    fn every_event_names_itself_as_it_is_written_and_is_in_the_field_table() {
        for (event, _) in samples() {
            let name = event.name();
            assert_eq!(event.parts().0, name);
            assert_eq!(serde_json::to_value(&event).unwrap()["event"], json!(name));
            assert!(fields(name).is_some(), "{name}");
        }
    }

    #[test]
    fn a_cause_reason_is_cleaned_and_cut_at_100_on_the_way_to_the_file() {
        let cause = Cause {
            step: CauseStep::Pass,
            reason: format!("a\u{202e}\n{}", "r".repeat(150)),
            actor: None,
            command: None,
            attempt: Some(u32::MAX),
            label: None,
        };
        assert_eq!(
            serde_json::to_value(cause.cleaned()).unwrap(),
            json!({"step": "pass", "reason": format!("a{}", "r".repeat(99)), "attempt": 4_294_967_295_u64})
        );
    }

    #[test]
    fn steps_and_kinds_are_written_as_the_runner_writes_them() {
        let steps = [
            (CauseStep::Start, "start"),
            (CauseStep::Watch, "watch"),
            (CauseStep::Forward, "forward"),
            (CauseStep::Advance, "advance"),
            (CauseStep::Gate, "gate"),
            (CauseStep::Pass, "pass"),
            (CauseStep::Retro, "retro"),
            (CauseStep::Operator, "operator"),
        ];
        for (step, name) in steps {
            assert_eq!(serde_json::to_value(step).unwrap(), json!(name));
        }
        let kinds = [
            (ErrorKind::Github, "github"),
            (ErrorKind::Tracker, "tracker"),
            (ErrorKind::T3, "t3"),
            (ErrorKind::Git, "git"),
            (ErrorKind::Gate, "gate"),
            (ErrorKind::Ledger, "ledger"),
            (ErrorKind::Config, "config"),
        ];
        for (kind, name) in kinds {
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(name));
        }
        assert_eq!(
            serde_json::to_value(OptionValue::Flag(true)).unwrap(),
            json!(true)
        );
    }
}

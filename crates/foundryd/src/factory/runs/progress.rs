//! A run's progress through its steps: what its ledger already records, for a step that must
//! not do twice what is done.
//!
//! Ported from the runner's `src/runner/progress.ts`. [`read_progress`] reads a run's events,
//! in the order the ledger gives them (lease generation, then the writer's factory version),
//! into a [`Seen`]. Nothing here orders anything by `ts` or `ms`; those only measure time.
//!
//! Where it differs from the runner:
//!
//! - `worked_ms` counts the time in `triage` and `building`, less the time a gate ran (from its
//!   `gate_checkout`) and the time the required checks were waited for (from the `gate-passed`
//!   step), through [`elapsed_ms`](crate::factory::text::elapsed_ms), so a clock that stepped
//!   back adds nothing. The runner stops counting once a pull request is reported; Foundry
//!   does not (spec, `limits.minutes`), so a session that hangs on a gate failure sent back
//!   still counts.
//! - A model is read as the launch line records it, `model_instance`, `model` and
//!   `model_options`, with no T3 thread behind it.
//! - [`run_names`] does not read a branch back from a worktree path: a Foundry launch line
//!   names its branch.

use std::collections::BTreeSet;

use foundry_proto::factory::{RunEvent, RunId};
use serde_json::Value;

use crate::factory::github::read::{CheckState, CheckStatus};
use crate::factory::ledger::events::{ModelOption, names};
use crate::factory::ledger::read::{field, text};
use crate::factory::lifecycle::State;
use crate::factory::text::{elapsed_ms, is_commit_id};

/// The ledger step that says a launch call is about to be sent.
pub const LAUNCH_SENDING: &str = "launch-sending";
/// The start of the `launch-sending` step of a launch that resumes from an earlier run's branch: `launch-sending resumed-from <branch>`.
pub const LAUNCH_SENDING_RESUMED: &str = "launch-sending resumed-from ";
/// The step written just before `launch-sending resumed-from <branch>` when the launch continues
/// an earlier attempt's open pull request: `launch-pull-request <branch> <url>`. A step of its own,
/// so a version that does not know it reads the `launch-sending` step as a resume from the branch.
pub const LAUNCH_PULL_REQUEST: &str = "launch-pull-request ";
/// The step written before `launch-sending` when the run's base is not the repo's: `launch-base <branch>`.
pub const LAUNCH_BASE: &str = "launch-base ";
/// The step written before `launch-sending` when the launch runs on a fallback: `launch-model <model>`.
pub const LAUNCH_MODEL: &str = "launch-model ";
/// The step the run is approved on: `approved <time>`.
pub const APPROVED: &str = "approved ";
/// The ledger step that says the issue text was checked again after the launch.
pub const APPROVAL_RECHECKED: &str = "approval-rechecked";
/// The ledger step written before the reminder is sent: `result-line-reminding <k>`, `k` the ordinal of the run that ended.
pub const RESULT_LINE_REMINDING: &str = "result-line-reminding";
/// The ledger step written after the reminder was sent.
pub const RESULT_LINE_REMINDED: &str = "result-line-reminded";
/// The ledger step written once the resume of a `session_interrupted` line was sent: `interruption-sent <resumes>`.
pub const INTERRUPTION_SENT: &str = "interruption-sent";
/// The ledger step written once the resume of a `session_rate_limited` line was sent: `rate-limited-sent <run ordinal>`.
pub const RATE_LIMITED_SENT: &str = "rate-limited-sent";
/// The ledger step written once a run whose agent process died was sent the resume message: `agent-died-resumed <k>`, `k` the ordinal of that run.
pub const AGENT_DIED_RESUMED: &str = "agent-died-resumed";
/// The ledger step that waits for a run newer than `k` (a forwarded comment); read here for the steps that write it.
pub const FORWARD_WAIT: &str = "forward-wait";
/// The ledger step written when every message sent under a `forward-wait <k>` was refused, so no run was started and the wait is over: `forward-wait-cancelled <k>`.
pub const FORWARD_WAIT_CANCELLED: &str = "forward-wait-cancelled";
/// The ledger step written as soon as a comment sent under an idle `forward-wait` was refused: `comment-refused <id>`.
pub const COMMENT_REFUSED: &str = "comment-refused";
/// The ledger step written as soon as a body edit sent under an idle `forward-wait` was refused: `body-edit-refused <edited_at>`.
pub const BODY_EDIT_REFUSED: &str = "body-edit-refused";
/// The ledger step written once the issue was told that an edit of the body was refused: `body-edit-dropped <edited_at>`; the edit counts as handled.
pub const BODY_EDIT_DROPPED: &str = "body-edit-dropped";
/// The ledger step written once an edit of the issue body is in the session: `body-edit-forwarded <edited_at>`.
pub const BODY_EDIT_FORWARDED: &str = "body-edit-forwarded";
/// The ledger step written once the issue was told that an instructor's comment was not forwarded: `comment-dropped <id>`.
pub const COMMENT_DROPPED: &str = "comment-dropped";
/// The ledger step written before a failed attempt is sent: `gate-sending <attempt> <ordinal>`.
pub const GATE_SENDING: &str = "gate-sending";
/// The ledger step that records the decision that an attempt passed: `gate-passed <sha> <n>`.
pub const GATE_PASSED: &str = "gate-passed";
/// The cause reason of a re-gate's `gate-passed` step with nothing cut short before it to repair.
pub const GATE_PASSED_CLEAN: &str = "gate-passed-clean";
/// The cause reason of the `checks_read` line written when the wait for the checks ran out.
pub const CHECKS_TIMED_OUT: &str = "checks-timed-out";
/// The ledger step that says the gate results are on the pull request.
pub const GATE_RESULTS_POSTED: &str = "gate-results-posted";

/// A passing attempt as the ledger records it: the commands that ran, and the commit they were gated on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PassRecord {
    pub attempt: u32,
    /// The commit the commands ran on, in lower case.
    pub sha: String,
    /// The tip of the base branch the attempt ran against, from its `gate_checkout` line; empty when the ledger has none.
    pub base_sha: String,
    /// When the `gate-passed` step was written, in Unix epoch milliseconds.
    pub at_ms: u64,
    /// Command, exit code and duration of each line of the attempt, in order.
    pub lines: Vec<CommandLine>,
    /// How many of the last `lines` the marker says the passing run wrote.
    pub count: u32,
}

/// One `gate_command` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandLine {
    pub attempt: u32,
    pub command: String,
    pub exit_code: i64,
    pub duration_ms: u64,
}

/// A pull request a line names: its number and the URL as the session wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub pr: u64,
    pub url: String,
}

/// The model a session runs on, as a launch line records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    /// The configured provider instance, e.g. `claudeAgent`.
    pub instance_id: String,
    /// The model id.
    pub model: String,
    /// The provider options the selection carries (a reasoning effort, say), when it has any.
    pub options: Option<Vec<ModelOption>>,
}

/// A `session_interrupted` or `session_rate_limited` line as [`read_progress`] keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interruption {
    /// How many interruptions the run has been through, this one included.
    pub resumes: u32,
    /// The session could not be resumed: the next attempt is started instead.
    pub relaunch: bool,
    /// The ordinal of the run that ended, for a resume.
    pub run_ordinal: Option<u32>,
    /// The line's branch: the session's for a resume, where the next attempt resumes from for a relaunch; `None` when it names none.
    pub branch: Option<String>,
    /// The `interruption-sent <resumes>` (or `rate-limited-sent <run ordinal>`) step for this resume is in the ledger.
    pub sent: bool,
    /// A `session_rate_limited` line: its resume is sent after the cooldown and counts as none.
    pub rate_limited: bool,
    /// The model the resume moves the session to, by id on the same instance; `None` to keep its own.
    pub model: Option<String>,
}

/// What started a re-gate, for the run's line. A moved head (with or without the base) is `Head`
/// or `Both`; a head that did not move leaves only `Conflict`: GitHub says the pull request
/// conflicts with the base branch, whatever the base did. A moved base alone starts no gate.
/// `Earlier` is a re-gate an older factory began with the head unchanged, whose line does not
/// say why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Moved {
    Head,
    Both,
    Conflict,
    Earlier,
}

/// What a relaunch fixes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fixes {
    /// A failed re-gate of a ready pull request: the gate attempt.
    Regate { attempt: u32 },
    /// Instructor feedback: the number of the feedback relaunch.
    Feedback { n: u32 },
}

/// The decision of a `regate_relaunching` or `feedback_relaunching` line: what a new session
/// needs to fix a failed re-gate of a ready pull request (or to act on instructor feedback),
/// kept so that a call that finds it with no `session_launched` after it can send the launch
/// again without running the gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relaunch {
    pub fixes: Fixes,
    /// The pull request's branch, which the session starts from and pushes to.
    pub branch: String,
    /// The failure text (or the feedback block) the session is sent after the launch's own words.
    pub message: String,
}

/// The `gate-sending <attempt> <ordinal>` step: the attempt, and the ordinal of the session run it was gated on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sending {
    pub attempt: u32,
    pub run_ordinal: u32,
}

/// A `session_paused` line: a person stopped the session's run, and this is its ordinal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Paused {
    pub run_ordinal: Option<u32>,
}

/// The newest read of the checks that had a failed check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChecksFailed {
    /// The lower-case sha of the read.
    pub sha: String,
    /// The checks that failed, with no result or link.
    pub failed: Vec<CheckStatus>,
}

/// The read the wait for the checks running out wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChecksTimedOut {
    pub sha: String,
    /// A check never appeared.
    pub missing: bool,
}

/// A run's progress, read from its events.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Seen {
    pub claimed: bool,
    /// When the first `claimed` line was written, in Unix epoch milliseconds; `None` before the claim.
    pub claimed_ms: Option<u64>,
    /// The `github_user` of the first `claimed` line; `None` when there is none or it is not text.
    pub github_user: Option<String>,
    /// The moment in the first `approved <time>` step: when an instructor applied the label the run was approved on.
    pub approved_at: Option<String>,
    /// The instructor on that step's cause (`actor`), who applied the label; `None` when the line names none.
    pub approved_by: Option<String>,
    /// From the last `state_changed`; `None` before any.
    pub state: Option<State>,
    /// The `session_launched` line's thread.
    pub thread_id: Option<String>,
    /// The base branch the newest `session_launched` line that names one records: the run's base,
    /// read from the ledger on every later pass and never from the issue again.
    pub base: Option<String>,
    /// The model the session runs on: the newest `session_launched` line's, moved by any
    /// rate-limited resume since. `None` for a launch that recorded none.
    pub model: Option<Model>,
    /// The `launch-sending` step: a launch call was about to be sent, so a session may exist.
    pub launch_sent: bool,
    /// The branch in a `launch-sending resumed-from <branch>` step: the decision to resume, made
    /// before the first launch call. `None` for a plain `launch-sending` or none.
    pub launch_resumed_from: Option<String>,
    /// The pull request of a `launch-pull-request <branch> <url>` step written just before that
    /// `launch-sending resumed-from <branch>` step: the decision to continue an earlier attempt's
    /// open pull request on that branch. `None` for a resume from a branch alone, or none.
    pub launch_resumed_pr: Option<String>,
    /// The branch in a `launch-base <branch>` step: the base the first launch call was decided on,
    /// when it is not the repo's.
    pub launch_base: Option<String>,
    /// The model id in a `launch-model <model>` step: the model the first launch call was decided
    /// on, when it is not the project's.
    pub launch_model: Option<String>,
    /// The `approval-rechecked` step: the issue text was checked again after the launch.
    pub rechecked: bool,
    /// The `session_launched` line's worktree path.
    pub worktree: Option<String>,
    /// A `worktree_removed` or `worktree_remove_failed` line follows the last `session_launched`.
    pub worktree_done: bool,
    /// The reason on the `stuck` line: the comment is on the issue.
    pub stuck: Option<String>,
    /// A `stop_requested` line: an operator asked for this run to be stopped by hand.
    pub stop_requested: bool,
    pub finished: bool,
    /// Every pull request a `pr_opened` line names, once each, in the order first named.
    pub opened: Vec<u64>,
    /// The newest `pr_opened`.
    pub last_pr: Option<PullRequest>,
    /// Every `pr_left` line, in order: a pull request the session left open on its branch when the
    /// run ended short of the gate, for the next attempt to continue. The session did not report
    /// it, so it is in none of `opened`, `last_pr` and `pr_reported`.
    pub left: Vec<PullRequest>,
    /// A `questions_posted` since the run last entered `building`: this stop's questions are on the issue.
    pub questions_posted: bool,
    /// A `pr_opened` since the run last entered `building` or had a gate sent back: the session's pull request waits for the gate.
    pub pr_reported: bool,
    /// The ordinal in the newest `result-line-reminding` step: a reminder send was started.
    pub reminding: Option<u32>,
    /// A `result-line-reminded` step: the session has had its one reminder.
    pub reminded: bool,
    /// The session is working until a run newer than this ordinal has ended: the reminder's, a
    /// forward's or a gate send-back's wait marker. Never goes down. A send-back's marker is
    /// dropped by the next `gate_command`, which shows the gate ran; a forward's only by a newer
    /// ended run, or by `forward-wait-cancelled` when nothing it covered went in.
    pub waiting_after: Option<u32>,
    /// The ordinal in the newest wait marker of either kind, kept after the marker is dropped:
    /// `forward` writes no second marker for the same ordinal.
    pub last_wait: Option<u32>,
    /// The id of every comment a `comment_forwarded` line names: it is in the session.
    pub forwarded: BTreeSet<String>,
    /// The time of every `body-edit-forwarded` or `body-edit-dropped` step: that edit is in the session, or the issue was told it was not sent.
    pub body_edits: BTreeSet<String>,
    /// The ordinal in the newest `forward-wait` step that no `comment_forwarded` line or
    /// `body-edit-forwarded` step follows and that was not cancelled: nothing sent under it is
    /// known to have gone in.
    pub idle_forward_wait: Option<u32>,
    /// `idle_forward_wait` when the ledger proves every send under it was refused: each send has
    /// its own `forward-wait <k>` line naming its command id, and a `comment-refused` or
    /// `body-edit-refused` step names the same id. One result is kept per command id, so a
    /// resend's refusal answers for a send whose own was lost. No run started, so `forward`
    /// cancels it.
    pub refused_forward_wait: Option<u32>,
    /// The id of every comment a `comment-dropped <id>` step names: the issue was told it was not forwarded.
    pub dropped: BTreeSet<String>,
    /// The newest `session_paused` line.
    pub paused: Option<Paused>,
    /// The newest `session_interrupted` or `session_rate_limited` line, until a new session is launched: the decision to resume or to relaunch.
    pub interruption: Option<Interruption>,
    /// The newest branch a `session_launched`, `session_interrupted` or `session_rate_limited` line names: where the run's work is.
    pub last_branch: Option<String>,
    /// Every branch a `branch_bound` line names: a branch the session made, whose pull request `advance` bound.
    pub bound_branches: Vec<String>,
    /// Milliseconds spent in `triage` and `building` in stints that have ended, less the time a
    /// gate ran and the time the required checks were waited for.
    pub worked_ms: u64,
    /// When the stint that is running began; `None` when the run is not working, or a gate or a
    /// wait for checks is running.
    pub working_since: Option<u64>,
    /// The attempts that have a `gate_command` line.
    pub command_attempts: BTreeSet<u32>,
    /// The attempts that were sent back into the session.
    pub sent_back_attempts: BTreeSet<u32>,
    /// The attempts whose `gate-passed` step is in the ledger and that were not sent back or checked out again after it.
    pub passed_attempts: BTreeSet<u32>,
    /// The `gate-passed <sha> <n>` step: the decision that the gate passed, made before the pull request was marked ready.
    pub pass: Option<PassRecord>,
    /// The newest `gate-sending` step.
    pub sending: Option<Sending>,
    /// A `gate-results-posted` step: the passing gate's results are on the pull request.
    pub results_posted: bool,
    /// The directories a `gate_cleanup_failed` line already names.
    pub cleanup_failed: BTreeSet<String>,
    /// A `pr_ready_outside` line: someone else marked the pull request ready while a gate ran, and it is noted.
    pub ready_outside: bool,
    /// The lower-case sha of the newest `checks_read` line whose checks are non-empty and all
    /// `passed`; `None` when the newest read decided otherwise, there is none, or a pass was
    /// recorded after it.
    pub checks_passed: Option<String>,
    /// The newest `checks_read` line with a failed check, since the last recorded pass: the
    /// decision that the pass's checks failed, which a later read does not undo.
    pub checks_failed: Option<ChecksFailed>,
    /// A `gate_regate` line since the run last changed state or posted the results of a pass: a re-gate is under way.
    pub regating: bool,
    /// What started the re-gate the newest `gate_regate` line begins, judged against the pass on record when it was written.
    pub regate_moved: Option<Moved>,
    /// That re-gate recorded its pass (`gate-passed`), and the results are not posted yet.
    pub regate_passed: bool,
    /// The newest `checks_read` line of the pass on record that the wait running out wrote (its
    /// cause says `checks-timed-out`): the decision to go `stuck`, which stands as a recorded
    /// failure does.
    pub checks_timed_out: Option<ChecksTimedOut>,
    /// Finishing the pass on record sets the ready state again, since a stuck path an earlier
    /// call did not finish may have drafted the pull request or relabelled the issue. False only
    /// for a pass whose cause is `gate-passed-clean`, written when a re-gate began itself.
    pub regate_pass_reassert: bool,
    /// That re-gate's failure was recorded as sent back, and the state has not changed yet.
    pub regate_sent_back: bool,
    /// The newest `regate_relaunching` or `feedback_relaunching` line, until the state changes.
    pub relaunch: Option<Relaunch>,
    /// A `session_launched` line follows the newest `gate-sending` step or relaunch decision.
    pub launched_since_sending: bool,
    /// The branch the newest `session_launched` line says its worktree was made from (a relaunch
    /// for a failed re-gate or feedback, or an attempt resumed onto an earlier one's work);
    /// `None` for a first launch.
    pub resumed_from: Option<String>,
    /// The ids named by any `feedback_relaunching` or `feedback_forwarding` line: each is acted on at most once.
    pub feedback_sent: BTreeSet<String>,
    /// The number on the newest `feedback_relaunching` line, 0 before any: how many feedback relaunches the run has had.
    pub feedback_relaunches: u32,
    /// How many items the newest `feedback_relaunching` line names.
    pub feedback_last_items: usize,
    /// When the run first reached `pr-open` since its last feedback relaunch (since it began, if
    /// none), in epoch milliseconds: the gate pass that took it there. Feedback is read from
    /// here. A feedback relaunch resets it to `None` and the next `pr-open` sets it; a trip
    /// through `building` and back, or a re-gate pass on a moved base, does not move it.
    pub pr_open_since: Option<u64>,
    /// The run on that `resumed_from`: this run's own id for a session that pushes to a pull
    /// request's branch (a re-gate's or feedback's, or one resumed onto an earlier attempt's open
    /// pull request), the earlier attempt's for a resume from its branch.
    pub resumed_from_run: Option<String>,
}

/// The branches and pull requests a run's ledger names as its work ([`run_names`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunNames {
    pub branches: BTreeSet<String>,
    pub prs: BTreeSet<u64>,
}

pub fn moved_since(
    passed_head: &str,
    passed_against: &str,
    head: &str,
    tip: &str,
) -> Option<Moved> {
    let _ = (passed_head, passed_against, head, tip);
    todo!()
}

pub fn without_check_reads(seen: Seen) -> Seen {
    let _ = seen;
    todo!()
}

pub fn regate_attempt(relaunch: &Relaunch) -> Option<u32> {
    let _ = relaunch;
    todo!()
}

pub fn relaunch_command_id(relaunch: &Relaunch, run: &RunId) -> String {
    let _ = (relaunch, run);
    todo!()
}

pub fn regate_branch<'a>(seen: &'a Seen, run: &RunId) -> Option<&'a str> {
    let _ = (seen, run);
    todo!()
}

pub fn pending_relaunch(seen: &Seen) -> Option<&Relaunch> {
    let _ = seen;
    todo!()
}

pub fn committing_feedback(seen: &Seen) -> Option<&Relaunch> {
    let _ = seen;
    todo!()
}

pub fn read_progress(events: &[RunEvent]) -> Seen {
    let _ = events;
    todo!()
}

pub fn next_gate_attempt(seen: &Seen) -> u32 {
    let _ = seen;
    todo!()
}

pub fn attempts_since_pass(seen: &Seen, attempt: u32) -> u32 {
    let _ = (seen, attempt);
    todo!()
}

pub fn run_names(events: &[RunEvent], seen: &Seen) -> RunNames {
    let _ = (events, seen);
    todo!()
}

#[cfg(test)]
mod tests {
    use foundry_proto::ids::MachineId;
    use serde_json::{Value, json};

    use super::*;
    use crate::factory::ledger::events::Event;
    use crate::factory::ledger::read::events;
    use crate::factory::ledger::testing::{
        T0, ledger, open_store, put, run as run_of, scripted, test_cause,
    };

    const MINUTE: u64 = 60_000;

    /// One event of a run as a test writes it: what the runner's tests call an `Entry`.
    #[derive(Clone)]
    struct Line {
        ms: u64,
        event: String,
        data: Value,
        cause: Option<Value>,
    }

    fn line(ms: u64, event: &str, data: Value) -> Line {
        Line {
            ms,
            event: event.into(),
            data,
            cause: None,
        }
    }

    fn with_cause(mut line: Line, cause: Value) -> Line {
        line.cause = Some(cause);
        line
    }

    fn step(ms: u64, text: &str) -> Line {
        line(ms, "step_changed", json!({ "step": text }))
    }

    fn moved(ms: u64, from: &str, to: &str) -> Line {
        line(ms, "state_changed", json!({ "from": from, "to": to }))
    }

    fn launched(ms: u64) -> Line {
        line(
            ms,
            "session_launched",
            json!({
                "thread_id": "thread-1",
                "worktree": "/t3/wt",
                "branch": "t3code/abc",
                "model_instance": null,
                "model": null,
                "resumed_from": null,
            }),
        )
    }

    /// The lines as the ledger holds them: each stored under the next version of one writer, so
    /// they are read back in the order given, whatever their `ms`.
    fn read_lines(lines: &[Line]) -> Vec<RunEvent> {
        let (_dir, store) = open_store();
        let owner = MachineId::random();
        let id = run_of(8);
        for (version, line) in (1..).zip(lines) {
            put(
                &store,
                owner,
                version,
                &id,
                1,
                line.ms,
                &line.event,
                line.data.clone(),
                line.cause.clone(),
            );
        }
        events(&store.lock().unwrap(), &id).unwrap()
    }

    /// The runner's `readProgress` of the lines.
    fn seen_of(lines: &[Line]) -> Seen {
        read_progress(&read_lines(lines))
    }

    fn plus(base: &[Line], more: Vec<Line>) -> Vec<Line> {
        [base, &more].concat()
    }

    mod the_pull_request {
        use super::*;

        const URL: &str = "https://github.com/rawphp/softfac/pull/12";

        #[test]
        fn keeps_every_pull_request_the_run_reported_and_the_newest_one_with_its_url() {
            let seen = seen_of(&[
                line(
                    1,
                    "pr_opened",
                    json!({"pr": 12, "url": "https://github.com/rawphp/softfac/pull/12"}),
                ),
                line(
                    2,
                    "pr_opened",
                    json!({"pr": 14, "url": "https://github.com/rawphp/softfac/pull/14"}),
                ),
            ]);
            assert_eq!(seen.opened, [12, 14]);
            assert_eq!(
                seen.last_pr,
                Some(PullRequest {
                    pr: 14,
                    url: "https://github.com/rawphp/softfac/pull/14".into()
                })
            );
        }

        #[test]
        fn has_none_before_the_first() {
            let seen = seen_of(&[]);
            assert_eq!(seen.opened, Vec::<u64>::new());
            assert_eq!(seen.last_pr, None);
            assert!(!seen.pr_reported);
        }

        #[test]
        fn keeps_a_pull_request_a_stopping_session_left_apart_from_the_ones_the_run_reported() {
            // A `pr_left` line is written by a stuck exit, for the next attempt to continue: the
            // session did not report it for the gate, so the run still holds its slot and its
            // time limit.
            let seen = seen_of(&[line(1, "pr_left", json!({"pr": 12, "url": URL}))]);
            assert_eq!(
                seen.left,
                [PullRequest {
                    pr: 12,
                    url: URL.into()
                }]
            );
            assert_eq!(seen.opened, Vec::<u64>::new());
            assert_eq!(seen.last_pr, None);
            assert!(!seen.pr_reported);
        }

        #[test]
        fn reports_a_pull_request_the_session_reports_after_a_stuck_exit_recorded_it_as_left() {
            let seen = seen_of(&[
                line(1, "pr_left", json!({"pr": 12, "url": URL})),
                line(2, "pr_opened", json!({"pr": 12, "url": URL})),
            ]);
            assert_eq!(seen.opened, [12]);
            assert_eq!(
                seen.last_pr,
                Some(PullRequest {
                    pr: 12,
                    url: URL.into()
                })
            );
            assert!(seen.pr_reported);
        }

        #[test]
        fn keeps_a_pull_request_reported_again_after_a_send_back_once_and_reported() {
            let seen = seen_of(&[
                line(1, "pr_opened", json!({"pr": 12, "url": URL})),
                line(2, "gate_sent_back", json!({"attempt": 1, "run_ordinal": 1})),
                line(3, "pr_opened", json!({"pr": 12, "url": URL})),
            ]);
            assert_eq!(seen.opened, [12]);
            assert!(seen.pr_reported);
        }

        #[test]
        fn is_reported_from_pr_opened_until_the_run_is_building_again() {
            let opened = line(1, "pr_opened", json!({"pr": 12, "url": URL}));
            assert!(seen_of(&[opened.clone()]).pr_reported);
            assert!(seen_of(&[opened.clone(), moved(2, "building", "pr-open")]).pr_reported);
            assert!(
                !seen_of(&[
                    opened,
                    moved(2, "building", "pr-open"),
                    moved(3, "pr-open", "building")
                ])
                .pr_reported
            );
        }
    }

    mod the_questions {
        use super::*;

        #[test]
        fn are_posted_from_questions_posted_until_the_run_is_building_again() {
            let posted = line(1, "questions_posted", json!({"questions": "q"}));
            assert!(seen_of(&[posted.clone()]).questions_posted);
            assert!(
                !seen_of(&[posted.clone(), moved(2, "needs-info", "building")]).questions_posted
            );
            assert!(seen_of(&[posted, moved(2, "building", "needs-info")]).questions_posted);
        }
    }

    mod the_reminder_and_the_wait_for_a_newer_run {
        use super::*;

        #[test]
        fn reads_the_ordinal_of_a_reminder_that_was_started_and_that_it_was_sent() {
            let started = seen_of(&[step(1, "result-line-reminding 3")]);
            assert_eq!(started.reminding, Some(3));
            assert!(!started.reminded);
            let sent = seen_of(&[
                step(1, "result-line-reminding 3"),
                step(2, "result-line-reminded"),
            ]);
            assert_eq!(sent.reminding, Some(3));
            assert!(sent.reminded);
        }

        #[test]
        fn waits_for_a_run_newer_than_the_reminders_or_a_forwards() {
            assert_eq!(
                seen_of(&[step(1, "result-line-reminding 3")]).waiting_after,
                Some(3)
            );
            assert_eq!(seen_of(&[step(1, "forward-wait 2")]).waiting_after, Some(2));
            assert_eq!(seen_of(&[]).waiting_after, None);
        }

        #[test]
        fn never_waits_for_an_older_run_than_before() {
            assert_eq!(
                seen_of(&[
                    step(1, "forward-wait 4"),
                    step(2, "result-line-reminding 2")
                ])
                .waiting_after,
                Some(4)
            );
            assert_eq!(
                seen_of(&[
                    step(1, "forward-wait 2"),
                    step(2, "result-line-reminding 2")
                ])
                .waiting_after,
                Some(2)
            );
        }

        #[test]
        fn does_not_read_a_forwards_wait_as_a_reminder() {
            let seen = seen_of(&[step(1, "forward-wait 1")]);
            assert_eq!(seen.reminding, None);
            assert!(!seen.reminded);
        }

        #[test]
        fn ignores_steps_whose_number_is_not_one() {
            let seen = seen_of(&[
                step(1, "result-line-reminding"),
                step(2, "result-line-reminding x"),
                step(3, "result-line-reminding 01"),
                step(4, "result-line-reminding 1 2"),
            ]);
            assert_eq!(seen.reminding, None);
            assert_eq!(seen.waiting_after, None);
        }

        #[test]
        fn starts_again_for_a_new_session_each_thread_has_one_reminder_and_its_own_run_numbers() {
            let seen = seen_of(&[
                step(1, "result-line-reminding 3"),
                step(2, "result-line-reminded"),
                line(3, "session_paused", json!({"run_ordinal": 3})),
                launched(4),
            ]);
            assert_eq!(seen.reminding, None);
            assert!(!seen.reminded);
            assert_eq!(seen.waiting_after, None);
            assert_eq!(seen.paused, None);
        }
    }

    mod a_stop_by_hand {
        use super::*;

        #[test]
        fn keeps_the_newest_pause_with_the_run_it_stopped() {
            assert_eq!(seen_of(&[]).paused, None);
            assert_eq!(
                seen_of(&[line(1, "session_paused", json!({"run_ordinal": 2}))]).paused,
                Some(Paused {
                    run_ordinal: Some(2)
                })
            );
            assert_eq!(
                seen_of(&[
                    line(1, "session_paused", json!({"run_ordinal": 2})),
                    line(2, "session_paused", json!({"run_ordinal": null})),
                ])
                .paused,
                Some(Paused { run_ordinal: None })
            );
        }

        #[test]
        fn is_requested_from_the_stop_requested_line_on_whatever_follows() {
            assert!(!seen_of(&[]).stop_requested);
            let requested = line(1, "stop_requested", json!({"by": "operator"}));
            assert!(seen_of(&[requested.clone()]).stop_requested);
            assert!(
                seen_of(&[
                    requested,
                    step(2, "launch-sending"),
                    moved(3, "triage", "building")
                ])
                .stop_requested
            );
        }
    }

    mod the_time_worked {
        use super::*;

        const URL: &str = "https://github.com/rawphp/softfac/pull/12";

        fn opened(ms: u64) -> Line {
            line(ms, "pr_opened", json!({"pr": 12, "url": URL}))
        }

        fn sent_back(ms: u64) -> Line {
            line(
                ms,
                "gate_sent_back",
                json!({"attempt": 1, "run_ordinal": 1}),
            )
        }

        #[test]
        fn counts_from_the_first_line_while_the_run_is_in_triage_or_building() {
            let seen = seen_of(&[
                line(1000, "claimed", json!({})),
                moved(1000, "ready", "triage"),
            ]);
            assert_eq!(seen.worked_ms, 0);
            assert_eq!(seen.working_since, Some(1000));
        }

        #[test]
        fn is_zero_and_not_counting_for_a_ledger_with_no_lines() {
            let seen = seen_of(&[]);
            assert_eq!(seen.worked_ms, 0);
            assert_eq!(seen.working_since, None);
        }

        #[test]
        fn adds_each_stint_when_the_state_changes_and_starts_the_next_in_triage_or_building() {
            let seen = seen_of(&[
                line(0, "claimed", json!({})),
                moved(0, "ready", "triage"),
                moved(MINUTE, "triage", "building"),
                moved(10 * MINUTE, "building", "needs-info"),
            ]);
            assert_eq!(seen.worked_ms, 10 * MINUTE);
            assert_eq!(seen.working_since, None);
        }

        #[test]
        fn stops_counting_when_the_run_is_finished() {
            let seen = seen_of(&[
                line(0, "claimed", json!({})),
                moved(0, "ready", "triage"),
                line(5 * MINUTE, "finished", json!({})),
            ]);
            assert_eq!(seen.working_since, None);
        }

        #[test]
        fn does_not_charge_the_build_again_when_a_pull_request_that_was_open_comes_back_to_building()
         {
            let seen = seen_of(&[
                line(0, "claimed", json!({})),
                moved(0, "ready", "triage"),
                moved(MINUTE, "triage", "building"),
                moved(30 * MINUTE, "building", "pr-open"),
                moved(500 * MINUTE, "pr-open", "building"),
            ]);
            assert_eq!(seen.worked_ms, 0);
            assert_eq!(seen.working_since, Some(500 * MINUTE));
        }

        /// The runner's case; Foundry keeps counting after a pull request is reported (spec,
        /// `limits.minutes`): with no gate event between, the time to the send-back counts.
        #[test]
        fn does_not_charge_the_factorys_gate_from_the_reported_pull_request_until_it_is_sent_back()
        {
            let to_pr = vec![
                line(0, "claimed", json!({})),
                moved(0, "ready", "triage"),
                moved(MINUTE, "triage", "building"),
                opened(20 * MINUTE),
            ];
            let held = seen_of(&to_pr);
            assert_eq!(held.worked_ms, MINUTE);
            assert_eq!(held.working_since, Some(MINUTE));
            let back = seen_of(&plus(&to_pr, vec![sent_back(90 * MINUTE)]));
            assert_eq!(back.worked_ms, 90 * MINUTE);
            assert_eq!(back.working_since, Some(90 * MINUTE));
        }

        /// The runner's case; Foundry keeps counting after a pull request is reported (spec,
        /// `limits.minutes`): every stint to a send-back counts.
        #[test]
        fn charges_only_the_sessions_stints_across_two_gates() {
            let seen = seen_of(&[
                line(0, "claimed", json!({})),
                moved(0, "triage", "building"),
                opened(10 * MINUTE),
                sent_back(50 * MINUTE),
                opened(60 * MINUTE),
                sent_back(100 * MINUTE),
            ]);
            assert_eq!(seen.worked_ms, 100 * MINUTE);
            assert_eq!(seen.working_since, Some(100 * MINUTE));
        }

        #[test]
        fn keeps_counting_through_a_pr_opened_line_it_does_not_accept() {
            let seen = seen_of(&[
                line(0, "claimed", json!({})),
                moved(0, "triage", "building"),
                line(20 * MINUTE, "pr_opened", json!({"pr": "12"})),
            ]);
            assert_eq!(seen.working_since, Some(0));
        }

        #[test]
        fn restarts_a_clock_already_running_at_a_gate_sent_back_keeping_the_time_before_it() {
            let seen = seen_of(&[
                line(0, "claimed", json!({})),
                moved(0, "triage", "building"),
                sent_back(10 * MINUTE),
            ]);
            assert_eq!(seen.worked_ms, 10 * MINUTE);
            assert_eq!(seen.working_since, Some(10 * MINUTE));
        }

        #[test]
        fn never_counts_a_pull_request_reported_before_the_stint_began() {
            let seen = seen_of(&[
                line(0, "claimed", json!({})),
                moved(30 * MINUTE, "triage", "building"),
                opened(10 * MINUTE),
            ]);
            assert_eq!(seen.worked_ms, 30 * MINUTE);
        }

        #[test]
        fn leaves_a_gate_sent_back_while_the_pull_request_is_open_to_the_next_state_change() {
            let seen = seen_of(&[
                line(0, "claimed", json!({})),
                moved(0, "triage", "building"),
                opened(10 * MINUTE),
                moved(30 * MINUTE, "building", "pr-open"),
                sent_back(50 * MINUTE),
            ]);
            assert_eq!(seen.working_since, None);
        }

        #[test]
        fn never_counts_a_backwards_clock() {
            let seen = seen_of(&[
                line(10 * MINUTE, "claimed", json!({})),
                moved(5 * MINUTE, "ready", "triage"),
            ]);
            assert_eq!(seen.worked_ms, 0);
        }

        const HEAD: &str = "4f1c2d3e4a5b60718293a4b5c6d7e8f901234567";

        fn checkout(ms: u64) -> Line {
            line(
                ms,
                "gate_checkout",
                json!({"attempt": 1, "dir": "/d", "sha": HEAD, "base_sha": ""}),
            )
        }

        fn building_to_pr() -> Vec<Line> {
            vec![
                line(0, "claimed", json!({})),
                moved(0, "ready", "triage"),
                moved(0, "triage", "building"),
                opened(10 * MINUTE),
            ]
        }

        #[test]
        fn does_not_charge_the_time_a_gate_ran_or_the_required_checks_were_waited_for() {
            // Building to 12 minutes; the gate from its checkout to its pass at 25; the checks
            // waited for to 40, when one failed and the gate was sent back; 10 more minutes of
            // the session's.
            let seen = seen_of(&plus(
                &building_to_pr(),
                vec![
                    checkout(12 * MINUTE),
                    line(
                        20 * MINUTE,
                        "gate_command",
                        json!({"attempt": 1, "command": "cargo test", "exit_code": 0, "duration_ms": 5}),
                    ),
                    step(25 * MINUTE, &format!("gate-passed {HEAD} 1")),
                    sent_back(40 * MINUTE),
                    moved(50 * MINUTE, "building", "needs-info"),
                ],
            ));
            assert_eq!(seen.worked_ms, 22 * MINUTE);
            assert_eq!(seen.working_since, None);
        }

        #[test]
        fn does_not_charge_a_gate_that_is_still_running() {
            let seen = seen_of(&plus(&building_to_pr(), vec![checkout(12 * MINUTE)]));
            assert_eq!(seen.worked_ms, 12 * MINUTE);
            assert_eq!(seen.working_since, None);
        }

        #[test]
        fn does_not_charge_the_wait_for_checks_that_ends_in_pr_open() {
            let seen = seen_of(&plus(
                &building_to_pr(),
                vec![
                    checkout(12 * MINUTE),
                    step(25 * MINUTE, &format!("gate-passed {HEAD} 1")),
                    step(26 * MINUTE, "gate-results-posted"),
                    moved(40 * MINUTE, "building", "pr-open"),
                ],
            ));
            assert_eq!(seen.worked_ms, 12 * MINUTE);
            assert_eq!(seen.working_since, None);
        }

        #[test]
        fn counts_a_session_that_hangs_on_a_gate_failure_sent_back() {
            // The runner stops counting at the reported pull request until the send-back and
            // then counts again, so its stints agree here; what differs is the time after the
            // pull request with no gate running, which the runner does not count.
            let seen = seen_of(&plus(&building_to_pr(), vec![sent_back(30 * MINUTE)]));
            assert_eq!(seen.worked_ms, 30 * MINUTE);
            assert_eq!(seen.working_since, Some(30 * MINUTE));
            // A session that then hangs for 10 hours is counted for all of it.
            let hung = seen_of(&plus(
                &building_to_pr(),
                vec![
                    sent_back(30 * MINUTE),
                    moved(630 * MINUTE, "building", "stuck"),
                ],
            ));
            assert_eq!(hung.worked_ms, 630 * MINUTE);
        }

        #[test]
        fn counts_the_time_after_a_reported_pull_request_while_no_gate_runs() {
            let seen = seen_of(&plus(
                &building_to_pr(),
                vec![moved(60 * MINUTE, "building", "stuck")],
            ));
            assert_eq!(seen.worked_ms, 60 * MINUTE);
        }

        #[test]
        fn a_wall_clock_that_steps_back_adds_no_worked_time() {
            // Written by the ledger, the clock reads 10, 5, 3 and 3 minutes after T0: every
            // line is stamped earlier than the one before it.
            let (_dir, store) = open_store();
            let ledger = ledger(
                &store,
                MachineId::random(),
                1,
                scripted(&[
                    T0 + 10 * MINUTE,
                    T0 + 5 * MINUTE,
                    T0 + 3 * MINUTE,
                    T0 + 3 * MINUTE,
                ]),
            );
            let id = run_of(8);
            let cause = test_cause();
            let writes = [
                Event::Claimed {
                    machine: "mac-mini".into(),
                    github_user: "rawphp".into(),
                },
                Event::StateChanged {
                    from: State::Ready,
                    to: State::Triage,
                },
                Event::StateChanged {
                    from: State::Triage,
                    to: State::Building,
                },
                Event::StateChanged {
                    from: State::Building,
                    to: State::NeedsInfo,
                },
            ];
            for event in &writes {
                ledger.write(&id, event, &cause).unwrap();
            }
            let read = ledger.read(&id).unwrap();
            // No event order changes: they read back as written, though their times go down.
            let order: Vec<(&str, u64)> = read.iter().map(|e| (e.event.as_str(), e.ms)).collect();
            assert_eq!(
                order,
                [
                    ("claimed", T0 + 10 * MINUTE),
                    ("state_changed", T0 + 5 * MINUTE),
                    ("state_changed", T0 + 3 * MINUTE),
                    ("state_changed", T0 + 3 * MINUTE),
                ]
            );
            let seen = read_progress(&read);
            assert_eq!(seen.worked_ms, 0);
            assert_eq!(seen.working_since, None);
            assert_eq!(seen.state, Some(State::NeedsInfo));
        }

        #[test]
        fn a_wall_clock_that_steps_back_changes_no_event_order() {
            // The newest line by version carries the oldest time: it is still the newest.
            let seen = seen_of(&[
                moved(20 * MINUTE, "ready", "triage"),
                moved(15 * MINUTE, "triage", "building"),
                moved(10 * MINUTE, "building", "needs-info"),
            ]);
            assert_eq!(seen.state, Some(State::NeedsInfo));
            let order = seen_of(&[
                line(9 * MINUTE, "pr_opened", json!({"pr": 14, "url": "u"})),
                line(1 * MINUTE, "pr_opened", json!({"pr": 12, "url": "v"})),
            ]);
            assert_eq!(order.opened, [14, 12]);
            assert_eq!(
                order.last_pr,
                Some(PullRequest {
                    pr: 12,
                    url: "v".into()
                })
            );
        }
    }

    mod forwarded_comments_and_body_edits {
        use super::*;

        #[test]
        fn keeps_the_id_of_every_comment_forwarded_line() {
            assert!(seen_of(&[]).forwarded.is_empty());
            let seen = seen_of(&[
                line(1, "comment_forwarded", json!({"comment_id": "102"})),
                line(2, "comment_forwarded", json!({"comment_id": "101"})),
                line(3, "comment_forwarded", json!({"comment_id": "102"})),
            ]);
            assert_eq!(
                seen.forwarded.into_iter().collect::<Vec<_>>(),
                ["101", "102"]
            );
        }

        #[test]
        fn keeps_the_time_of_every_body_edit_forwarded_step_whatever_follows_the_space() {
            let seen = seen_of(&[
                step(1, "body-edit-forwarded 2026-10-04T10:15:00Z"),
                step(2, "body-edit-forwarded x"),
            ]);
            assert_eq!(
                seen.body_edits.into_iter().collect::<Vec<_>>(),
                ["2026-10-04T10:15:00Z", "x"]
            );
        }

        #[test]
        fn keeps_the_time_of_every_body_edit_dropped_step_with_the_forwarded_ones() {
            let seen = seen_of(&[
                step(1, "body-edit-forwarded A"),
                step(2, "body-edit-dropped B"),
                step(3, "body-edit-droppedC"),
            ]);
            assert_eq!(seen.body_edits.into_iter().collect::<Vec<_>>(), ["A", "B"]);
        }

        #[test]
        fn does_not_count_a_step_that_only_looks_like_a_forwarded_edit() {
            let seen = seen_of(&[
                step(1, "body-edit-forwarded2026-10-04T10:15:00Z"),
                step(2, "body-edit-forward 2026-10-04T10:15:00Z"),
                step(3, "Body-edit-forwarded 2026-10-04T10:15:00Z"),
                step(4, "xbody-edit-forwarded 2026-10-04T10:15:00Z"),
            ]);
            assert!(seen.body_edits.is_empty());
        }

        #[test]
        fn keeps_the_id_of_every_comment_dropped_step() {
            assert!(seen_of(&[]).dropped.is_empty());
            let seen = seen_of(&[
                step(1, "comment-dropped 8"),
                step(2, "comment-dropped IC_kwDO1"),
                step(3, "comment-dropped 8"),
            ]);
            assert_eq!(
                seen.dropped.into_iter().collect::<Vec<_>>(),
                ["8", "IC_kwDO1"]
            );
        }

        #[test]
        fn a_comment_dropped_id_is_one_plain_token() {
            let texts = [
                "comment-dropped ",
                "comment-dropped a b",
                "comment-dropped a\tb",
                "comment-dropped a\u{7}b",
                "comment-dropped a\n",
                "comment-dropped7",
            ];
            let lines: Vec<Line> = (1..).zip(texts).map(|(i, t)| step(i, t)).collect();
            assert!(seen_of(&lines).dropped.is_empty());
        }
    }

    mod the_last_wait_marker {
        use super::*;

        fn sent_back(ms: u64, ordinal: u32) -> Line {
            line(
                ms,
                "gate_sent_back",
                json!({"attempt": 1, "run_ordinal": ordinal}),
            )
        }

        fn command(ms: u64) -> Line {
            line(
                ms,
                "gate_command",
                json!({"attempt": 1, "command": "cargo test", "exit_code": 0, "duration_ms": 5}),
            )
        }

        #[test]
        fn is_none_before_any_marker() {
            assert_eq!(seen_of(&[launched(1)]).last_wait, None);
        }

        #[test]
        fn is_the_ordinal_of_a_forward_wait_a_reminder_or_a_send_back_whichever_is_the_newest() {
            assert_eq!(seen_of(&[step(1, "forward-wait 3")]).last_wait, Some(3));
            assert_eq!(
                seen_of(&[step(1, "result-line-reminding 2")]).last_wait,
                Some(2)
            );
            assert_eq!(seen_of(&[sent_back(1, 4)]).last_wait, Some(4));
            assert_eq!(
                seen_of(&[step(1, "forward-wait 1"), sent_back(2, 2)]).last_wait,
                Some(2)
            );
        }

        #[test]
        fn never_goes_down_when_a_marker_for_an_older_run_is_written_later() {
            let seen = seen_of(&[step(1, "forward-wait 3"), sent_back(2, 2)]);
            assert_eq!(seen.last_wait, Some(3));
            assert_eq!(seen.waiting_after, Some(3));
        }

        #[test]
        fn is_kept_after_a_gate_command_drops_a_send_backs_marker() {
            let seen = seen_of(&[sent_back(1, 2), command(2)]);
            assert_eq!(seen.waiting_after, None);
            assert_eq!(seen.last_wait, Some(2));
        }

        #[test]
        fn starts_again_with_a_new_session() {
            let seen = seen_of(&[step(1, "forward-wait 5"), launched(2)]);
            assert_eq!(seen.last_wait, None);
            assert_eq!(seen.waiting_after, None);
        }

        #[test]
        fn ignores_a_step_that_only_looks_like_a_wait_marker() {
            for bad in [
                "forward-wait 007",
                "forward-wait -1",
                "forward-wait 1 ",
                "forward-wait x",
            ] {
                let seen = seen_of(&[step(1, bad)]);
                assert_eq!(seen.last_wait, None, "{bad}");
                assert_eq!(seen.waiting_after, None, "{bad}");
            }
        }
    }

    mod an_interruption {
        use super::*;

        fn interrupted_with(ms: u64, over: Value) -> Line {
            let mut data = json!({
                "resumes": 1,
                "relaunch": false,
                "run_ordinal": 2,
                "branch": "t3code/abc",
                "t3_error": null,
                "ended_in_pass": 2,
            });
            for (key, value) in over.as_object().unwrap() {
                data[key] = value.clone();
            }
            line(ms, "session_interrupted", data)
        }

        fn interrupted(ms: u64) -> Line {
            interrupted_with(ms, json!({}))
        }

        #[test]
        fn is_none_before_the_first_line() {
            let seen = seen_of(&[launched(1)]);
            assert_eq!(seen.interruption, None);
            assert_eq!(seen.last_branch.as_deref(), Some("t3code/abc"));
        }

        #[test]
        fn keeps_the_newest_line_owed_its_send_until_the_matching_step() {
            let owed = seen_of(&[launched(1), interrupted(2)]);
            assert_eq!(
                owed.interruption,
                Some(Interruption {
                    resumes: 1,
                    relaunch: false,
                    run_ordinal: Some(2),
                    branch: Some("t3code/abc".into()),
                    sent: false,
                    rate_limited: false,
                    model: None,
                })
            );
            let sent = seen_of(&[launched(1), interrupted(2), step(3, "interruption-sent 1")]);
            assert!(sent.interruption.unwrap().sent);
            // A step for another count is not this line's.
            let other = seen_of(&[launched(1), interrupted(2), step(3, "interruption-sent 2")]);
            assert!(!other.interruption.unwrap().sent);
        }

        #[test]
        fn is_a_wait_marker_for_the_run_that_ended_as_a_forwards_is() {
            let seen = seen_of(&[launched(1), interrupted(2)]);
            assert_eq!(seen.waiting_after, Some(2));
            assert_eq!(seen.last_wait, Some(2));
            // A relaunch waits for nothing: the session is gone.
            let relaunch = seen_of(&[
                launched(1),
                interrupted_with(2, json!({"relaunch": true, "run_ordinal": null})),
            ]);
            assert_eq!(relaunch.waiting_after, None);
            assert!(relaunch.interruption.unwrap().relaunch);
        }

        #[test]
        fn names_the_newest_branch_a_launch_or_an_interruption_line_carries() {
            let seen = seen_of(&[
                launched(1),
                interrupted_with(2, json!({"branch": "t3code/def"})),
                interrupted_with(3, json!({"branch": null})),
            ]);
            assert_eq!(seen.last_branch.as_deref(), Some("t3code/def"));
            // The line's own branch is kept apart: this one names none.
            assert_eq!(seen.interruption.unwrap().branch, None);
        }

        #[test]
        fn carries_a_relaunchs_branch_and_count_which_the_next_attempt_reads() {
            let seen = seen_of(&[
                launched(1),
                interrupted_with(
                    2,
                    json!({"resumes": 2, "relaunch": true, "run_ordinal": null, "branch": "t3code/def"}),
                ),
            ]);
            assert_eq!(
                seen.interruption,
                Some(Interruption {
                    resumes: 2,
                    relaunch: true,
                    run_ordinal: None,
                    branch: Some("t3code/def".into()),
                    sent: false,
                    rate_limited: false,
                    model: None,
                })
            );
            // A count that is not a number is no line at all, so the older decision stands.
            let odd = seen_of(&[
                launched(1),
                interrupted_with(2, json!({"relaunch": true, "run_ordinal": null})),
                interrupted_with(
                    3,
                    json!({"resumes": "two", "relaunch": true, "run_ordinal": null, "branch": "t3code/x"}),
                ),
            ]);
            let kept = odd.interruption.unwrap();
            assert_eq!(kept.resumes, 1);
            assert_eq!(kept.branch.as_deref(), Some("t3code/abc"));
            assert_eq!(odd.last_branch.as_deref(), Some("t3code/x"));
        }

        #[test]
        fn owes_nothing_to_a_new_session_its_thread_has_its_own_runs() {
            let seen = seen_of(&[launched(1), interrupted(2), launched(3)]);
            assert!(seen.interruption.unwrap().sent);
            assert_eq!(seen.waiting_after, None);
        }
    }

    mod the_approval {
        use super::*;

        fn approved(ms: u64, actor: Option<&str>) -> Line {
            let mut cause = json!({"step": "start", "reason": "approved"});
            if let Some(actor) = actor {
                cause["actor"] = json!(actor);
            }
            with_cause(step(ms, "approved 2026-10-04T10:00:00Z"), cause)
        }

        #[test]
        fn names_the_moment_and_the_instructor_on_the_first_approved_step() {
            let seen = seen_of(&[approved(1, Some("tomkaczocha")), approved(2, Some("other"))]);
            assert_eq!(seen.approved_at.as_deref(), Some("2026-10-04T10:00:00Z"));
            assert_eq!(seen.approved_by.as_deref(), Some("tomkaczocha"));
        }

        #[test]
        fn names_no_instructor_when_the_cause_has_none_or_has_an_empty_one() {
            assert_eq!(seen_of(&[approved(1, None)]).approved_by, None);
            assert_eq!(seen_of(&[approved(1, Some(""))]).approved_by, None);
            assert_eq!(seen_of(&[]).approved_by, None);
        }
    }

    mod the_branch_a_launch_resumed_from {
        use super::*;

        fn the_run() -> RunId {
            RunId::new("rawphp/softfac", 10, 2).unwrap()
        }

        fn resumed_launch(ms: u64, run: &str, branch: &str) -> Line {
            line(
                ms,
                "session_launched",
                json!({
                    "thread_id": "thread-2",
                    "worktree": "/t3/wt2",
                    "branch": "t3code/own",
                    "model_instance": null,
                    "model": null,
                    "resumed_from": {"run": run, "branch": branch},
                }),
            )
        }

        #[test]
        fn is_none_for_a_plain_launch() {
            let seen = seen_of(&[launched(1)]);
            assert_eq!(seen.resumed_from, None);
            assert_eq!(seen.resumed_from_run, None);
            assert_eq!(regate_branch(&seen, &the_run()), None);
        }

        #[test]
        fn names_the_run_and_the_branch_of_the_launch_lines_resumed_from() {
            let seen = seen_of(&[resumed_launch(1, "rawphp__softfac__10__1", "t3code/old")]);
            assert_eq!(seen.resumed_from.as_deref(), Some("t3code/old"));
            assert_eq!(
                seen.resumed_from_run.as_deref(),
                Some("rawphp__softfac__10__1")
            );
            assert_eq!(seen.last_branch.as_deref(), Some("t3code/own"));
        }

        #[test]
        fn is_the_pull_requests_branch_only_for_a_re_gate_session_which_resumed_from_the_run_itself()
         {
            // A re-gate of run 2 launched a session from its pull request's branch: that is
            // where it pushes.
            let regate = seen_of(&[resumed_launch(1, "rawphp__softfac__10__2", "feat/pr-12")]);
            assert_eq!(regate_branch(&regate, &the_run()), Some("feat/pr-12"));
            // A session resumed from attempt 1's branch works on its own branch.
            let resumed = seen_of(&[resumed_launch(1, "rawphp__softfac__10__1", "t3code/old")]);
            assert_eq!(regate_branch(&resumed, &the_run()), None);
        }

        #[test]
        fn is_the_newest_launch_lines() {
            let seen = seen_of(&[
                resumed_launch(1, "rawphp__softfac__10__2", "feat/pr-12"),
                launched(2),
            ]);
            assert_eq!(seen.resumed_from, None);
            assert_eq!(seen.resumed_from_run, None);
            assert_eq!(regate_branch(&seen, &the_run()), None);
        }
    }

    mod the_launch_sending_step_of_a_resume {
        use super::*;

        #[test]
        fn reads_the_branch_of_a_resume_from_an_earlier_runs_branch_with_no_pull_request() {
            let seen = seen_of(&[step(1, "launch-sending resumed-from t3code/abc")]);
            assert!(seen.launch_sent);
            assert_eq!(seen.launch_resumed_from.as_deref(), Some("t3code/abc"));
            assert_eq!(seen.launch_resumed_pr, None);
        }

        #[test]
        fn reads_the_pull_request_of_a_resume_onto_an_open_pull_request_from_the_step_before() {
            let seen = seen_of(&[
                step(
                    1,
                    "launch-pull-request t3code/abc https://github.com/rawphp/softfac/pull/12",
                ),
                step(2, "launch-sending resumed-from t3code/abc"),
            ]);
            assert_eq!(seen.launch_resumed_from.as_deref(), Some("t3code/abc"));
            assert_eq!(
                seen.launch_resumed_pr.as_deref(),
                Some("https://github.com/rawphp/softfac/pull/12")
            );
        }

        #[test]
        fn takes_no_pull_request_decided_for_another_branch_or_with_no_launch_sending_step_after_it()
         {
            // A decision a crash cut short before its `launch-sending` step, then another decision.
            let other = seen_of(&[
                step(
                    1,
                    "launch-pull-request t3code/abc https://github.com/rawphp/softfac/pull/12",
                ),
                step(2, "launch-sending resumed-from t3code/def"),
            ]);
            assert_eq!(other.launch_resumed_from.as_deref(), Some("t3code/def"));
            assert_eq!(other.launch_resumed_pr, None);
            let unsent = seen_of(&[step(
                1,
                "launch-pull-request t3code/abc https://github.com/rawphp/softfac/pull/12",
            )]);
            assert!(!unsent.launch_sent);
            assert_eq!(unsent.launch_resumed_pr, None);
        }

        #[test]
        fn is_none_for_a_plain_launch() {
            let seen = seen_of(&[step(1, "launch-sending")]);
            assert!(seen.launch_sent);
            assert_eq!(seen.launch_resumed_from, None);
            assert_eq!(seen.launch_resumed_pr, None);
        }
    }

    mod a_cancelled_forward_wait {
        use super::*;

        fn sent_back(ms: u64, ordinal: u32) -> Line {
            line(
                ms,
                "gate_sent_back",
                json!({"attempt": 1, "run_ordinal": ordinal}),
            )
        }

        #[test]
        fn a_forward_wait_no_send_follows_is_idle() {
            assert_eq!(
                seen_of(&[step(1, "forward-wait 2")]).idle_forward_wait,
                Some(2)
            );
            assert_eq!(
                seen_of(&[
                    step(1, "forward-wait 2"),
                    line(2, "comment_forwarded", json!({"comment_id": "5"}))
                ])
                .idle_forward_wait,
                None
            );
            assert_eq!(
                seen_of(&[step(1, "forward-wait 2"), step(2, "body-edit-forwarded x")])
                    .idle_forward_wait,
                None
            );
            assert_eq!(
                seen_of(&[step(1, "forward-wait 2"), step(2, "comment-dropped 7")])
                    .idle_forward_wait,
                Some(2)
            );
        }

        #[test]
        fn a_cancel_for_the_idle_marker_clears_the_wait_and_restores_the_last_wait() {
            let seen = seen_of(&[
                step(1, "forward-wait 2"),
                step(2, "forward-wait-cancelled 2"),
            ]);
            assert_eq!(seen.waiting_after, None);
            assert_eq!(seen.last_wait, None);
            assert_eq!(seen.idle_forward_wait, None);

            let after = seen_of(&[
                sent_back(1, 1),
                step(2, "forward-wait 2"),
                step(3, "forward-wait-cancelled 2"),
            ]);
            assert_eq!(after.waiting_after, None);
            assert_eq!(after.last_wait, Some(1));
        }

        #[test]
        fn a_cancel_does_not_undo_a_marker_a_send_used() {
            let seen = seen_of(&[
                step(1, "forward-wait 2"),
                line(2, "comment_forwarded", json!({"comment_id": "5"})),
                step(3, "forward-wait-cancelled 2"),
            ]);
            assert_eq!(seen.waiting_after, Some(2));
            assert_eq!(seen.last_wait, Some(2));
        }

        #[test]
        fn a_cancel_for_another_ordinal_or_one_that_only_looks_like_one_changes_nothing() {
            let other = seen_of(&[
                step(1, "forward-wait 2"),
                step(2, "forward-wait-cancelled 3"),
            ]);
            assert_eq!(other.waiting_after, Some(2));
            for bad in [
                "forward-wait-cancelled 02",
                "forward-wait-cancelled x",
                "forward-wait-cancelled 2 ",
            ] {
                let seen = seen_of(&[step(1, "forward-wait 2"), step(2, bad)]);
                assert_eq!(seen.waiting_after, Some(2), "{bad}");
                assert_eq!(seen.last_wait, Some(2), "{bad}");
            }
        }

        #[test]
        fn an_idle_marker_is_proven_refused_only_when_every_send_under_it_has_a_refused_line() {
            // Each line as `forward` writes it: the step, and the command id of the send in its cause.
            fn sending(text: &str, command: Option<&str>) -> Line {
                let mut line = step(1, text);
                line.cause = command.map(
                    |command| json!({"step": "forward", "reason": "seed", "command": command}),
                );
                line
            }
            const A: &str = "softfac:rawphp/softfac#10:comment:7";
            const B: &str = "softfac:rawphp/softfac#10:body:x";
            let proven = |lines: Vec<Line>| seen_of(&lines).refused_forward_wait;
            // The one send under it has no outcome yet: it may have gone in.
            assert_eq!(proven(vec![sending("forward-wait 2", Some(A))]), None);
            assert_eq!(
                proven(vec![
                    sending("forward-wait 2", Some(A)),
                    sending("comment-refused 7", Some(A))
                ]),
                Some(2)
            );
            assert_eq!(
                proven(vec![
                    sending("forward-wait 2", Some(B)),
                    sending("body-edit-refused x", Some(B))
                ]),
                Some(2)
            );
            // Each further send writes the marker again before it goes out.
            assert_eq!(
                proven(vec![
                    sending("forward-wait 2", Some(A)),
                    sending("comment-refused 7", Some(A)),
                    sending("forward-wait 2", Some(B)),
                ]),
                None
            );
            assert_eq!(
                proven(vec![
                    sending("forward-wait 2", Some(A)),
                    sending("comment-refused 7", Some(A)),
                    sending("forward-wait 2", Some(B)),
                    sending("body-edit-refused x", Some(B)),
                ]),
                Some(2)
            );
            // A send with no outcome stays open until a refusal names its command id: one
            // result is kept per command id, so refusing the resend proves the lost send never
            // went in.
            assert_eq!(
                proven(vec![
                    sending("forward-wait 2", Some(A)),
                    sending("forward-wait 2", Some(B)),
                    sending("body-edit-refused x", Some(B)),
                ]),
                None
            );
            assert_eq!(
                proven(vec![
                    sending("forward-wait 2", Some(A)),
                    sending("forward-wait 2", Some(A)),
                    sending("comment-refused 7", Some(A)),
                ]),
                Some(2)
            );
            assert_eq!(
                proven(vec![
                    sending("forward-wait 2", Some(A)),
                    sending("comment-refused 7", Some(B))
                ]),
                None
            );
            // A marker that names no send (an older build's) or a refusal that names none
            // proves nothing.
            assert_eq!(
                proven(vec![
                    sending("forward-wait 2", None),
                    sending("comment-refused 7", Some(A))
                ]),
                None
            );
            assert_eq!(
                proven(vec![
                    sending("forward-wait 2", None),
                    sending("forward-wait 2", Some(A)),
                    sending("comment-refused 7", Some(A)),
                ]),
                None
            );
            assert_eq!(
                proven(vec![
                    sending("forward-wait 2", Some(A)),
                    sending("comment-refused 7", None)
                ]),
                None
            );
            // A drop notice is not an outcome of a send.
            assert_eq!(
                proven(vec![
                    sending("forward-wait 2", Some(A)),
                    sending("comment-dropped 7", Some(A))
                ]),
                None
            );
            assert_eq!(
                proven(vec![
                    sending("forward-wait 2", Some(A)),
                    line(2, "comment_forwarded", json!({"comment_id": "5"})),
                    sending("comment-refused 7", Some(A)),
                ]),
                None
            );
            assert_eq!(
                proven(vec![
                    sending("forward-wait 2", Some(A)),
                    sending("comment-refused 7", Some(A)),
                    step(3, "forward-wait-cancelled 2"),
                ]),
                None
            );
            assert_eq!(proven(vec![sending("comment-refused 7", Some(A))]), None);
        }

        #[test]
        fn a_cancel_after_markers_written_again_restores_the_wait_from_before_the_first() {
            let seen = seen_of(&[
                sent_back(1, 1),
                step(2, "forward-wait 2"),
                step(3, "comment-refused 7"),
                step(4, "forward-wait 2"),
                step(5, "body-edit-refused x"),
                step(6, "forward-wait-cancelled 2"),
            ]);
            assert_eq!(seen.waiting_after, None);
            assert_eq!(seen.last_wait, Some(1));
            assert_eq!(seen.idle_forward_wait, None);
        }

        #[test]
        fn a_new_session_forgets_an_idle_marker() {
            let seen = seen_of(&[step(1, "forward-wait 2"), launched(2)]);
            assert_eq!(seen.idle_forward_wait, None);
        }
    }

    mod the_checks_read {
        use super::*;

        const SHA: &str = "0123456789ABCDEF0123456789abcdef01234567";

        fn checks_read(ms: u64, checks: &[(&str, &str)]) -> Line {
            let checks: Vec<Value> = checks
                .iter()
                .map(|(name, state)| json!({"name": name, "state": state}))
                .collect();
            line(ms, "checks_read", json!({"sha": SHA, "checks": checks}))
        }

        fn failed_check(name: &str) -> CheckStatus {
            CheckStatus {
                name: name.into(),
                state: CheckState::Failed,
                conclusion: String::new(),
                url: None,
            }
        }

        #[test]
        fn checks_passed_is_the_newest_all_passed_checks_reads_lower_case_sha_and_null_for_any_other_read()
         {
            let mixed = checks_read(1, &[("gate", "passed"), ("lint", "failed")]);
            let all_passed = checks_read(2, &[("gate", "passed"), ("lint", "passed")]);
            let later_mixed = checks_read(3, &[("gate", "passed"), ("lint", "pending")]);
            assert_eq!(seen_of(&[mixed.clone()]).checks_passed, None);
            assert_eq!(
                seen_of(&[mixed.clone(), all_passed.clone()]).checks_passed,
                Some(SHA.to_lowercase())
            );
            assert_eq!(
                seen_of(&[mixed, all_passed.clone(), later_mixed]).checks_passed,
                None
            );
            assert_eq!(
                seen_of(&[all_passed, checks_read(4, &[])]).checks_passed,
                None
            );
        }

        #[test]
        fn a_pass_recorded_after_a_read_clears_it_the_new_passs_checks_are_read_afresh() {
            let all_passed = checks_read(1, &[("gate", "passed")]);
            let failed = checks_read(1, &[("gate", "failed")]);
            let passed_again = step(2, &format!("gate-passed {} 2", SHA.to_lowercase()));
            assert_eq!(
                seen_of(&[all_passed, passed_again.clone()]).checks_passed,
                None
            );
            assert_eq!(seen_of(&[failed, passed_again]).checks_failed, None);
        }

        #[test]
        fn checks_failed_names_the_failed_checks_of_the_newest_failing_read_and_a_later_read_keeps_it()
         {
            let failing = checks_read(
                1,
                &[("gate", "failed"), ("lint", "pending"), ("docs", "failed")],
            );
            let want = ChecksFailed {
                sha: SHA.to_lowercase(),
                failed: vec![failed_check("gate"), failed_check("docs")],
            };
            assert_eq!(
                seen_of(&[failing.clone()]).checks_failed,
                Some(want.clone())
            );
            let later_passed = checks_read(
                2,
                &[("gate", "passed"), ("lint", "passed"), ("docs", "passed")],
            );
            assert_eq!(seen_of(&[failing, later_passed]).checks_failed, Some(want));
            assert_eq!(
                seen_of(&[checks_read(1, &[("gate", "pending")])]).checks_failed,
                None
            );
        }

        #[test]
        fn checks_timed_out_is_the_read_the_wait_running_out_wrote_and_no_other_unfinished_read() {
            let pending = [("gate", "passed"), ("lint", "pending")];
            let timed_out = |checks: &[(&str, &str)]| {
                with_cause(
                    checks_read(1, checks),
                    json!({"step": "gate", "reason": "checks-timed-out", "attempt": 1}),
                )
            };
            assert_eq!(
                seen_of(&[timed_out(&pending)]).checks_timed_out,
                Some(ChecksTimedOut {
                    sha: SHA.to_lowercase(),
                    missing: false
                })
            );
            assert_eq!(
                seen_of(&[timed_out(&[("gate", "missing")])]).checks_timed_out,
                Some(ChecksTimedOut {
                    sha: SHA.to_lowercase(),
                    missing: true
                })
            );
            // A read with checks still running that the wait did not end decides nothing.
            assert_eq!(seen_of(&[checks_read(1, &pending)]).checks_timed_out, None);
            let recorded = with_cause(
                checks_read(1, &pending),
                json!({"step": "gate", "reason": "progress"}),
            );
            assert_eq!(seen_of(&[recorded]).checks_timed_out, None);
        }
    }

    mod gating_again {
        use super::*;

        const HEAD: &str = "4f1c2d3e4a5b60718293a4b5c6d7e8f901234567";
        const PUSHED: &str = "7e6d5c4b3a291807f6e5d4c3b2a190817263a4b5";
        const BASE: &str = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";
        const MOVED: &str = "9f8e7d6c5b4a39281706f5e4d3c2b1a098765432";

        fn the_run() -> RunId {
            RunId::new("rawphp/softfac", 10, 1).unwrap()
        }

        fn checkout(ms: u64, base: &str) -> Line {
            line(
                ms,
                "gate_checkout",
                json!({"attempt": 1, "dir": "/d", "sha": HEAD, "base_sha": base}),
            )
        }

        fn command(ms: u64) -> Line {
            line(
                ms,
                "gate_command",
                json!({"attempt": 1, "command": "cargo test", "exit_code": 0, "duration_ms": 5}),
            )
        }

        fn passed(ms: u64) -> Line {
            with_cause(
                step(ms, &format!("gate-passed {HEAD} 1")),
                json!({"step": "gate", "reason": "gate-passed", "attempt": 1}),
            )
        }

        /// A pass on `HEAD` against `BASE`, then `pr-open`.
        fn pass() -> Vec<Line> {
            vec![
                launched(1),
                line(
                    2,
                    "pr_opened",
                    json!({"pr": 12, "url": "https://github.com/rawphp/softfac/pull/12"}),
                ),
                checkout(3, BASE),
                command(4),
                passed(5),
                step(6, "gate-results-posted"),
                moved(7, "building", "pr-open"),
            ]
        }

        fn regated(ms: u64, sha: &str, base: &str, previous: &str) -> Line {
            line(
                ms,
                "gate_regate",
                json!({"sha": sha, "base_sha": base, "previous_base_sha": previous, "conflict": true}),
            )
        }

        /// A line an older factory wrote: no `conflict` field, whatever started it.
        fn regated_before(ms: u64, sha: &str, base: &str, previous: &str) -> Line {
            line(
                ms,
                "gate_regate",
                json!({"sha": sha, "base_sha": base, "previous_base_sha": previous}),
            )
        }

        fn relaunching(ms: u64) -> Line {
            line(
                ms,
                "regate_relaunching",
                json!({"attempt": 2, "branch": "t3code/abc", "message": "boom"}),
            )
        }

        #[test]
        fn has_no_re_gate_under_way_before_a_gate_regate_line() {
            let seen = seen_of(&pass());
            assert!(!seen.regating);
            assert_eq!(seen.regate_moved, None);
            assert!(!seen.regate_passed);
            assert!(!seen.regate_sent_back);
            assert_eq!(seen.relaunch, None);
        }

        #[test]
        fn is_under_way_from_a_gate_regate_line_and_says_what_started_it() {
            let seen = seen_of(&plus(&pass(), vec![regated(8, HEAD, MOVED, BASE)]));
            assert!(seen.regating);
            // The head did not move: the base alone starts no gate, so only a conflict is left.
            assert_eq!(seen.regate_moved, Some(Moved::Conflict));
            assert_eq!(
                seen_of(&plus(&pass(), vec![regated(8, PUSHED, BASE, BASE)])).regate_moved,
                Some(Moved::Head)
            );
            assert_eq!(
                seen_of(&plus(&pass(), vec![regated(8, PUSHED, MOVED, BASE)])).regate_moved,
                Some(Moved::Both)
            );
        }

        #[test]
        fn says_a_conflict_when_a_pass_is_on_record_and_nothing_moved_and_nothing_with_no_pass() {
            assert_eq!(
                seen_of(&plus(&pass(), vec![regated(8, HEAD, BASE, BASE)])).regate_moved,
                Some(Moved::Conflict)
            );
            assert_eq!(
                seen_of(&[launched(1), regated(2, HEAD, BASE, BASE)]).regate_moved,
                None
            );
        }

        #[test]
        fn says_an_earlier_factory_began_it_for_an_older_line_with_the_head_unchanged() {
            // A base move or a conflict under the older rule: the line does not say which.
            assert_eq!(
                seen_of(&plus(&pass(), vec![regated_before(8, HEAD, MOVED, BASE)])).regate_moved,
                Some(Moved::Earlier)
            );
            assert_eq!(
                seen_of(&plus(&pass(), vec![regated_before(8, HEAD, BASE, BASE)])).regate_moved,
                Some(Moved::Earlier)
            );
            // A moved head is read from the shas, marker or not.
            assert_eq!(
                seen_of(&plus(&pass(), vec![regated_before(8, PUSHED, MOVED, BASE)])).regate_moved,
                Some(Moved::Both)
            );
            assert_eq!(
                seen_of(&[launched(1), regated_before(2, HEAD, BASE, BASE)]).regate_moved,
                None
            );
        }

        #[test]
        fn keeps_the_newest_lines_answer_when_another_line_follows() {
            let seen = seen_of(&plus(
                &pass(),
                vec![
                    regated(8, HEAD, MOVED, BASE),
                    regated(9, PUSHED, BASE, BASE),
                ],
            ));
            assert_eq!(seen.regate_moved, Some(Moved::Head));
        }

        #[test]
        fn records_a_pass_of_the_re_gate_and_drops_it_with_the_posted_results() {
            let again = plus(
                &pass(),
                vec![regated(8, HEAD, MOVED, BASE), command(9), passed(10)],
            );
            assert!(seen_of(&again).regate_passed);
            assert!(seen_of(&again).regating);
            let posted = seen_of(&plus(&again, vec![step(11, "gate-results-posted")]));
            assert!(!posted.regating);
            assert!(!posted.regate_passed);
            // A pass that is not a re-gate's is not a regate pass.
            assert!(!seen_of(&[launched(1), command(2), passed(3)]).regate_passed);
        }

        #[test]
        fn a_re_gate_pass_owes_the_ready_state_again_unless_its_cause_says_it_is_clean() {
            let before = plus(&pass(), vec![regated(8, HEAD, MOVED, BASE), command(9)]);
            let clean = with_cause(
                passed(10),
                json!({"step": "gate", "reason": "gate-passed-clean", "attempt": 1}),
            );
            assert!(!seen_of(&plus(&before, vec![clean])).regate_pass_reassert);
            // `gate-passed` is what a resumed call writes, and what every pass said before the
            // clean reason existed, when every resumed pass set the ready state again.
            assert!(seen_of(&plus(&before, vec![passed(10)])).regate_pass_reassert);
            let mut no_cause = passed(10);
            no_cause.cause = None;
            assert!(seen_of(&plus(&before, vec![no_cause])).regate_pass_reassert);
        }

        #[test]
        fn records_a_failure_of_the_re_gate_as_sent_back_until_the_state_changes() {
            let sent = plus(
                &pass(),
                vec![
                    regated(8, HEAD, MOVED, BASE),
                    line(9, "gate_sent_back", json!({"attempt": 2, "run_ordinal": 0})),
                ],
            );
            assert!(seen_of(&sent).regate_sent_back);
            let back = seen_of(&plus(&sent, vec![moved(10, "pr-open", "building")]));
            assert!(!back.regate_sent_back);
            assert!(!back.regating);
            assert!(
                !seen_of(&plus(
                    &pass(),
                    vec![line(
                        8,
                        "gate_sent_back",
                        json!({"attempt": 2, "run_ordinal": 3})
                    )]
                ))
                .regate_sent_back
            );
        }

        #[test]
        fn holds_a_relaunch_from_its_decision_until_the_state_changes() {
            let seen = seen_of(&plus(
                &pass(),
                vec![regated(8, HEAD, MOVED, BASE), relaunching(9)],
            ));
            assert_eq!(
                seen.relaunch,
                Some(Relaunch {
                    fixes: Fixes::Regate { attempt: 2 },
                    branch: "t3code/abc".into(),
                    message: "boom".into(),
                })
            );
            assert_eq!(
                seen.sending,
                Some(Sending {
                    attempt: 2,
                    run_ordinal: 0
                })
            );
            assert!(!seen.launched_since_sending);
            assert_eq!(pending_relaunch(&seen), seen.relaunch.as_ref());
            assert_eq!(
                seen_of(&plus(
                    &pass(),
                    vec![relaunching(9), moved(10, "pr-open", "building")]
                ))
                .relaunch,
                None
            );
        }

        #[test]
        fn is_no_longer_pending_once_a_session_is_launched_after_the_decision() {
            let seen = seen_of(&plus(&pass(), vec![relaunching(9), launched(10)]));
            assert!(seen.launched_since_sending);
            assert_eq!(pending_relaunch(&seen), None);
            assert!(seen.relaunch.is_some());
            assert_eq!(pending_relaunch(&seen_of(&pass())), None);
        }

        #[test]
        fn forgets_a_launch_when_a_later_gate_sending_step_or_relaunch_decision_comes() {
            let launched_after = vec![launched(1), step(2, "gate-sending 1 1"), launched(3)];
            assert!(seen_of(&launched_after).launched_since_sending);
            assert!(
                !seen_of(&plus(&launched_after, vec![step(4, "gate-sending 2 1")]))
                    .launched_since_sending
            );
            assert!(!seen_of(&plus(&launched_after, vec![relaunching(4)])).launched_since_sending);
        }

        #[test]
        fn reads_a_feedback_relaunch_which_is_no_attempt_and_no_gate_sending() {
            let seen = seen_of(&plus(
                &pass(),
                vec![line(
                    8,
                    "feedback_relaunching",
                    json!({"n": 3, "branch": "t3code/abc", "items": ["review:1"], "message": "fix it"}),
                )],
            ));
            let relaunch = seen.relaunch.clone().unwrap();
            assert_eq!(
                relaunch,
                Relaunch {
                    fixes: Fixes::Feedback { n: 3 },
                    branch: "t3code/abc".into(),
                    message: "fix it".into(),
                }
            );
            assert_eq!(seen.sending, None);
            assert_eq!(regate_attempt(&relaunch), None);
            assert_eq!(
                relaunch_command_id(&relaunch, &the_run()),
                "softfac:rawphp__softfac__10__1:feedback:3"
            );
        }

        #[test]
        fn names_a_re_gate_relaunchs_attempt_and_command_id() {
            let seen = seen_of(&plus(&pass(), vec![relaunching(8)]));
            let relaunch = seen.relaunch.unwrap();
            assert_eq!(regate_attempt(&relaunch), Some(2));
            assert_eq!(
                relaunch_command_id(&relaunch, &the_run()),
                "softfac:rawphp__softfac__10__1:regate:2"
            );
        }

        mod instructor_feedback {
            use super::*;

            fn deciding(ms: u64, n: u32, items: &[&str]) -> Line {
                line(
                    ms,
                    "feedback_relaunching",
                    json!({"n": n, "branch": "t3code/abc", "items": items, "message": "Feedback 1 of 1"}),
                )
            }

            #[test]
            fn is_a_relaunch_that_is_pending_until_the_session_is_recorded_then_only_the_transition_is_left()
             {
                let decided = seen_of(&plus(
                    &pass(),
                    vec![deciding(8, 1, &["review:11", "issue-comment:42"])],
                ));
                assert_eq!(
                    pending_relaunch(&decided),
                    Some(&Relaunch {
                        fixes: Fixes::Feedback { n: 1 },
                        branch: "t3code/abc".into(),
                        message: "Feedback 1 of 1".into(),
                    })
                );
                assert_eq!(committing_feedback(&decided), None);
                // The items are named for good, and the relaunch is counted.
                assert_eq!(
                    decided.feedback_sent.iter().cloned().collect::<Vec<_>>(),
                    ["issue-comment:42", "review:11"]
                );
                assert_eq!(decided.feedback_relaunches, 1);
                assert_eq!(decided.feedback_last_items, 2);

                let launched_seen = seen_of(&plus(
                    &pass(),
                    vec![
                        deciding(8, 1, &["review:11", "issue-comment:42"]),
                        launched(9),
                    ],
                ));
                assert_eq!(pending_relaunch(&launched_seen), None);
                assert_ne!(committing_feedback(&launched_seen), None);

                let committed = seen_of(&plus(
                    &pass(),
                    vec![
                        deciding(8, 1, &["review:11", "issue-comment:42"]),
                        launched(9),
                        moved(10, "pr-open", "building"),
                    ],
                ));
                assert_eq!(pending_relaunch(&committed), None);
                assert_eq!(committing_feedback(&committed), None);
                assert_eq!(committed.feedback_relaunches, 1);
                assert_eq!(committed.feedback_sent.len(), 2);
            }

            #[test]
            fn is_not_committing_for_a_re_gate_relaunch() {
                let seen = seen_of(&plus(&pass(), vec![relaunching(8), launched(9)]));
                assert_eq!(committing_feedback(&seen), None);
            }

            #[test]
            fn counts_the_highest_relaunch_and_takes_the_item_count_of_the_newest() {
                let seen = seen_of(&plus(
                    &pass(),
                    vec![
                        deciding(8, 1, &["review:1", "review:2"]),
                        deciding(9, 2, &["review:3"]),
                    ],
                ));
                assert_eq!(seen.feedback_relaunches, 2);
                assert_eq!(seen.feedback_last_items, 1);
                assert_eq!(seen.feedback_sent.len(), 3);
                assert_eq!(seen_of(&pass()).feedback_relaunches, 0);
                assert_eq!(seen_of(&pass()).feedback_last_items, 0);
            }

            #[test]
            fn names_the_items_a_forward_sent_into_a_paused_session_as_sent_too() {
                let seen = seen_of(&plus(
                    &pass(),
                    vec![line(
                        8,
                        "feedback_forwarding",
                        json!({"n": 1, "run_ordinal": 2, "items": ["issue-comment:7"], "message": "m"}),
                    )],
                ));
                assert_eq!(
                    seen.feedback_sent.into_iter().collect::<Vec<_>>(),
                    ["issue-comment:7"]
                );
                assert_eq!(seen.feedback_relaunches, 0);
            }

            #[test]
            fn is_no_gate_attempt_and_no_re_gate() {
                let before = seen_of(&pass());
                let seen = seen_of(&plus(
                    &pass(),
                    vec![
                        deciding(8, 1, &["review:11"]),
                        launched(9),
                        moved(10, "pr-open", "building"),
                    ],
                ));
                assert_eq!(seen.sending, before.sending);
                assert_eq!(seen.sent_back_attempts, before.sent_back_attempts);
                assert_eq!(seen.command_attempts, before.command_attempts);
                assert_eq!(next_gate_attempt(&seen), next_gate_attempt(&before));
                assert!(!(seen.regating || seen.regate_passed || seen.regate_sent_back));
            }

            #[test]
            fn closes_a_passed_attempt_so_the_next_gate_is_a_new_attempt() {
                let seen = seen_of(&pass());
                assert_eq!(seen.pass.as_ref().map(|p| p.attempt), Some(1));
                assert_eq!(next_gate_attempt(&seen), 2);
            }

            #[test]
            fn counts_the_retry_budget_from_the_last_pass_before_the_attempt() {
                let seen = seen_of(&pass());
                assert_eq!(attempts_since_pass(&seen, 1), 1);
                assert_eq!(attempts_since_pass(&seen, 2), 1);
                assert_eq!(attempts_since_pass(&seen, 3), 2);
            }

            #[test]
            fn reads_feedback_from_the_pass_the_run_reached_pr_open_on() {
                let seen = seen_of(&pass());
                assert_eq!(seen.pass.as_ref().map(|p| p.at_ms), Some(5));
                assert_eq!(seen.pr_open_since, Some(5));
                assert_eq!(seen_of(&[]).pr_open_since, None);
                // A pr-open with no pass on record is the line's own time.
                assert_eq!(
                    seen_of(&[moved(40, "triage", "pr-open")]).pr_open_since,
                    Some(40)
                );
            }

            #[test]
            fn keeps_the_cursor_through_a_re_gate_on_a_moved_base_and_through_a_trip_via_building()
            {
                let regated = plus(
                    &pass(),
                    vec![
                        line(
                            8,
                            "gate_regate",
                            json!({"sha": HEAD, "base_sha": MOVED, "previous_base_sha": BASE}),
                        ),
                        command(9),
                        passed(10),
                        step(11, "gate-results-posted"),
                    ],
                );
                assert_eq!(seen_of(&regated).pr_open_since, Some(5));
                let tripped = plus(
                    &regated,
                    vec![
                        moved(12, "pr-open", "building"),
                        moved(13, "building", "pr-open"),
                    ],
                );
                assert_eq!(seen_of(&tripped).pr_open_since, Some(5));
            }

            #[test]
            fn resets_the_cursor_on_a_feedback_relaunch_and_the_next_pr_open_sets_it_again() {
                let decided = plus(&pass(), vec![deciding(8, 1, &["review:11"])]);
                assert_eq!(seen_of(&decided).pr_open_since, None);
                let building = plus(
                    &decided,
                    vec![launched(9), moved(10, "pr-open", "building")],
                );
                assert_eq!(seen_of(&building).pr_open_since, None);
                let back = plus(&building, vec![moved(5_000, "building", "pr-open")]);
                assert_eq!(seen_of(&back).pr_open_since, Some(5_000));
                // Later trips through building and back (a failed re-gate) do not move it.
                let tripped = plus(
                    &back,
                    vec![
                        moved(9_000, "pr-open", "building"),
                        moved(12_000, "building", "pr-open"),
                    ],
                );
                assert_eq!(seen_of(&tripped).pr_open_since, Some(5_000));
            }

            #[test]
            fn launches_under_a_command_id_of_what_the_relaunch_fixes() {
                let regate = Relaunch {
                    fixes: Fixes::Regate { attempt: 2 },
                    branch: "t3code/abc".into(),
                    message: "boom".into(),
                };
                let feedback = Relaunch {
                    fixes: Fixes::Feedback { n: 3 },
                    ..regate.clone()
                };
                assert_eq!(
                    relaunch_command_id(&regate, &the_run()),
                    "softfac:rawphp__softfac__10__1:regate:2"
                );
                assert_eq!(
                    relaunch_command_id(&feedback, &the_run()),
                    "softfac:rawphp__softfac__10__1:feedback:3"
                );
            }
        }
    }

    mod moved_since_whether_the_head_moved_since_the_recorded_pass {
        use super::*;

        const HEAD: &str = "4f1c2d3e4a5b60718293a4b5c6d7e8f901234567";
        const PUSHED: &str = "7e6d5c4b3a291807f6e5d4c3b2a190817263a4b5";
        const BASE: &str = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";
        const MOVED: &str = "9f8e7d6c5b4a39281706f5e4d3c2b1a098765432";

        #[test]
        fn is_nothing_without_a_pass_on_record_whatever_the_head_and_tip_are() {
            assert_eq!(moved_since("", "", HEAD, BASE), None);
        }

        #[test]
        fn is_nothing_when_only_the_tip_differs_with_or_without_a_head_on_record() {
            assert_eq!(moved_since("", BASE, HEAD, MOVED), None);
            assert_eq!(moved_since(HEAD, BASE, HEAD, MOVED), None);
        }

        #[test]
        fn is_the_head_when_only_the_head_differs_and_no_base_is_on_record() {
            assert_eq!(moved_since(HEAD, "", PUSHED, BASE), Some(Moved::Head));
            assert_eq!(moved_since(HEAD, BASE, PUSHED, BASE), Some(Moved::Head));
        }

        #[test]
        fn is_both_when_both_differ() {
            assert_eq!(moved_since(HEAD, BASE, PUSHED, MOVED), Some(Moved::Both));
        }

        #[test]
        fn is_nothing_for_a_pass_with_a_head_and_no_base_when_the_head_did_not_move() {
            assert_eq!(moved_since(HEAD, "", HEAD, MOVED), None);
            assert_eq!(moved_since(HEAD, "", HEAD, BASE), None);
        }

        #[test]
        fn compares_commit_ids_without_regard_to_case() {
            assert_eq!(
                moved_since(HEAD, BASE, &HEAD.to_uppercase(), &BASE.to_uppercase()),
                None
            );
        }
    }

    // The runner's tests do not cover the rest of what `Seen` reads; these do.
    mod the_rest_of_the_read {
        use super::*;

        #[test]
        fn reads_the_claim_the_launch_and_the_ledger_end() {
            let seen = seen_of(&[
                line(
                    5,
                    "claimed",
                    json!({"machine": "mac-mini", "github_user": "rawphp"}),
                ),
                line(
                    6,
                    "claimed",
                    json!({"machine": "other", "github_user": "late"}),
                ),
                line(
                    7,
                    "session_launched",
                    json!({
                        "thread_id": "t1", "worktree": "/w", "branch": "fac/10-1",
                        "model_instance": "claudeAgent", "model": "claude-sonnet-5-5",
                        "model_options": [{"id": "effort", "value": "high"}],
                        "resumed_from": null, "base": "main"
                    }),
                ),
                line(8, "worktree_removed", json!({"path": "/w"})),
                line(9, "stuck", json!({"reason": "no result"})),
                line(10, "branch_bound", json!({"pr": 12, "branch": "feat/x"})),
                line(11, "branch_bound", json!({"pr": 12, "branch": "feat/x"})),
                line(12, "finished", json!({})),
            ]);
            assert!(seen.claimed);
            assert_eq!(seen.claimed_ms, Some(5));
            assert_eq!(seen.github_user.as_deref(), Some("rawphp"));
            assert_eq!(seen.thread_id.as_deref(), Some("t1"));
            assert_eq!(seen.base.as_deref(), Some("main"));
            assert_eq!(seen.worktree.as_deref(), Some("/w"));
            assert!(seen.worktree_done);
            assert_eq!(seen.stuck.as_deref(), Some("no result"));
            assert_eq!(seen.bound_branches, ["feat/x"]);
            assert!(seen.finished);
            let model = seen.model.unwrap();
            assert_eq!(model.instance_id, "claudeAgent");
            assert_eq!(model.model, "claude-sonnet-5-5");
            assert_eq!(model.options.unwrap().len(), 1);
        }

        #[test]
        fn a_rate_limited_resume_moves_the_model_on_the_same_instance() {
            let seen = seen_of(&[
                line(
                    1,
                    "session_launched",
                    json!({
                        "thread_id": "t1", "worktree": "/w", "branch": "fac/10-1",
                        "model_instance": "claudeAgent", "model": "a", "resumed_from": null
                    }),
                ),
                line(
                    2,
                    "session_rate_limited",
                    json!({"run_ordinal": 3, "branch": "fac/10-1", "resumes": 1, "model": "b"}),
                ),
            ]);
            assert_eq!(seen.model.unwrap().model, "b");
            let limited = seen.interruption.unwrap();
            assert!(limited.rate_limited);
            assert_eq!(limited.model.as_deref(), Some("b"));
            assert_eq!(seen.waiting_after, Some(3));
            let sent = seen_of(&[
                line(
                    2,
                    "session_rate_limited",
                    json!({"run_ordinal": 3, "branch": null, "resumes": 1}),
                ),
                step(3, "rate-limited-sent 3"),
            ]);
            assert!(sent.interruption.unwrap().sent);
        }

        #[test]
        fn reads_the_launch_decisions_and_the_gate_steps() {
            let seen = seen_of(&[
                step(1, "launch-base release"),
                step(2, "launch-model claude-x"),
                step(3, "approval-rechecked"),
                step(4, "gate-sending 1 2"),
                step(5, "gate-sending 1"),
                step(6, "gate-passed nothex 1"),
                step(7, &format!("gate-passed {} 0", "a".repeat(40))),
            ]);
            assert_eq!(seen.launch_base.as_deref(), Some("release"));
            assert_eq!(seen.launch_model.as_deref(), Some("claude-x"));
            assert!(seen.rechecked);
            assert_eq!(
                seen.sending,
                Some(Sending {
                    attempt: 1,
                    run_ordinal: 2
                })
            );
            assert_eq!(seen.pass, None);
        }

        #[test]
        fn a_pass_keeps_its_commands_its_base_and_its_lower_case_sha() {
            let sha = "ABCDEF0123456789ABCDEF0123456789ABCDEF01";
            let seen = seen_of(&[
                line(
                    1,
                    "gate_checkout",
                    json!({"attempt": 1, "dir": "/d", "sha": sha, "base_sha": "b1"}),
                ),
                line(
                    2,
                    "gate_command",
                    json!({"attempt": 1, "command": "make", "exit_code": 0, "duration_ms": 9}),
                ),
                with_cause(
                    step(3, &format!("gate-passed {sha} 1")),
                    json!({"step": "gate", "reason": "gate-passed", "attempt": 1}),
                ),
            ]);
            assert_eq!(
                seen.pass,
                Some(PassRecord {
                    attempt: 1,
                    sha: sha.to_lowercase(),
                    base_sha: "b1".into(),
                    at_ms: 3,
                    lines: vec![CommandLine {
                        attempt: 1,
                        command: "make".into(),
                        exit_code: 0,
                        duration_ms: 9
                    }],
                    count: 1,
                })
            );
            assert_eq!(seen.passed_attempts.into_iter().collect::<Vec<_>>(), [1]);
        }

        #[test]
        fn without_check_reads_drops_what_the_checks_said() {
            let sha = "0123456789abcdef0123456789abcdef01234567";
            let seen = seen_of(&[line(
                1,
                "checks_read",
                json!({"sha": sha, "checks": [{"name": "gate", "state": "passed"}]}),
            )]);
            assert!(seen.checks_passed.is_some());
            let cleared = without_check_reads(seen);
            assert_eq!(cleared.checks_passed, None);
            assert_eq!(cleared.checks_failed, None);
            assert_eq!(cleared.checks_timed_out, None);
        }

        #[test]
        fn a_gate_attempt_with_commands_that_was_never_sent_back_or_passed_is_made_again() {
            let command = |ms, attempt: u32| {
                line(
                    ms,
                    "gate_command",
                    json!({"attempt": attempt, "command": "c", "exit_code": 1, "duration_ms": 1}),
                )
            };
            assert_eq!(next_gate_attempt(&seen_of(&[])), 1);
            assert_eq!(next_gate_attempt(&seen_of(&[command(1, 1)])), 1);
            assert_eq!(
                next_gate_attempt(&seen_of(&[
                    command(1, 1),
                    line(2, "gate_sent_back", json!({"attempt": 1, "run_ordinal": 1})),
                ])),
                2
            );
        }

        #[test]
        fn names_every_branch_and_pull_request_the_ledger_names() {
            let lines = [
                step(1, "launch-sending resumed-from old/branch"),
                line(
                    2,
                    "session_launched",
                    json!({
                        "thread_id": "t", "worktree": "/w", "branch": "fac/10-1",
                        "model_instance": null, "model": null,
                        "resumed_from": {"run": "r", "branch": "earlier"}
                    }),
                ),
                line(3, "stuck", json!({"reason": "x", "branch": "fac/10-1-r1"})),
                line(4, "pr_opened", json!({"pr": 12, "url": "u"})),
                line(5, "pr_left", json!({"pr": 9, "url": "u"})),
                line(6, "branch_bound", json!({"pr": 12, "branch": "feat/x"})),
            ];
            let read = read_lines(&lines);
            let names = run_names(&read, &read_progress(&read));
            assert_eq!(
                names.branches.into_iter().collect::<Vec<_>>(),
                ["earlier", "fac/10-1", "fac/10-1-r1", "feat/x", "old/branch"]
            );
            assert_eq!(names.prs.into_iter().collect::<Vec<_>>(), [9, 12]);
        }
    }
}

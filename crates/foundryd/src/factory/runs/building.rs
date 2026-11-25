//! A run while its session builds.
//!
//! Ported from the result, reminder, questions and time-limit parts of the runner's
//! `src/runner/building.ts`. [`advance`] is one pass over a run whose session builds: it reads
//! the session's newest turn once, and acts on its result line. `Done` binds the session's pull
//! request to the run and hands it to the gate, `NeedsInfo` posts the questions, ends the
//! session, archives the build workspace by force and ends the run, and any other stop is
//! stuck. A turn with no result line is sent the one reminder first. [`time_limit`] ends a run
//! in `triage` or `building` that has worked longer than `limits.minutes`.
//!
//! A turn is acted on once. The last line written for a turn that leaves the run going is the
//! step `turn-read <n>` ([`TURN_READ`]); every write before it carries its own marker, so a
//! pass cut short repeats nothing. A session that idles through a gate has its newest turn
//! read already and is sent nothing.
//!
//! Where it differs from the runner:
//!
//! - A session is read by its turn, not by a thread's run status. The fleet's session state
//!   and newest turn say what the runner's `status.run` did: `starting` and `working` are the
//!   runner's active runs, `idle` is a run that ended, `finished` and `failed` are a session
//!   whose process ended (an interruption, which `resume` decides: nothing is written, and only
//!   a `Done` result in the newest unread turn is acted on, as the runner's `endedBadlyNow`
//!   lets a done message win). A turn number stands where the runner's run ordinal did: the
//!   reminder is owed for the turn that ended without a result line (`result-line-reminding
//!   <n>`), and a run whose reminder was begun reads a newer turn as the reminder having
//!   arrived. The runner's `paused` run (a person stopped it in T3), its `agent died` resume,
//!   rate limits and the rolled-back status have no counterpart: P1 marks a session `stuck`
//!   after 10 minutes of silence, and the factory leaves it alone (`limits.minutes` ends it).
//! - A session that waits for a person is stuck at once, with its question as the session text
//!   ([`SESSION_WAITING`]): permissions are skipped, so nothing should ask.
//! - A run with no session recorded is `start`'s while it is in `triage` ([`BuildStep::Running`]);
//!   in any later state the ledger is broken and the pass is a [`RunError::Invalid`], where the
//!   runner's `NoSessionError` stood.
//! - A run in a state another step owns is [`BuildStep::Skipped`], where the runner returned a
//!   `NotBuildingError`: a finished run, a stop asked for (`stop` owns it), an interruption or
//!   a relaunch decided (`resume` owns it), and `pr-open` (gating and feedback own it).
//! - A pull request is bound to the run by its branch: the launch line's, the one it resumed
//!   from, and any `branch_bound` line. Not ported: `madeBySession` (a pull request found on a
//!   worktree's HEAD), the launch-branch of a worktree path, a Paradev Cloud launch branch,
//!   `earlierAttempts` and `otherRunHas`. A Foundry workspace is made on its branch, so a
//!   session has no other.
//! - The pull request the session leaves when it stops or asks (`recordLeftPullRequest`, the
//!   `pr_left` line) is not written: the next attempt's launch finds the open pull request
//!   itself (task 22).
//! - The reminder is sent with the turn's prompt count, so a reminder whose line was typed is
//!   refused with the count risen instead of typed twice (the runner's command id). A refusal
//!   that typed nothing and will be refused again (`BodyWithoutPath`, `SecretsUnreadable`) is
//!   final and stuck; a send that failed may have typed and is an error, sent again next pass
//!   under the same count.
//! - `limits.minutes` counts a run that has reported its pull request: the runner's
//!   `pr-reported` skip is dropped (the spec), and [`Seen::worked_ms`] leaves out the gate and
//!   the checks. The time is taken from the timestamps the events carry, as the spec says:
//!   the running stint ends at the system's wall clock, the base the ledger stamps with
//!   ([`time_limit`] reads it; [`time_limit_at`] is given it), and `ctx.clock`, the
//!   sleep-counting clock, is not read at all, since its readings are never compared with a
//!   stored record. [`elapsed_ms`] never counts a negative span. A begun stuck path or
//!   questions comment is `advance`'s to finish, not taken over with the time-limit words.
//!   The runner asked the thread for how recently the session was active and said so in the
//!   reason; Foundry has no such read, so the reason is the plain [`messages::TIME_LIMIT`] and
//!   the runner's cases about a busy or an idle session, a thread archived or unreadable, and
//!   a run of another repo (`WrongRepoError`) have no counterpart. The branch in the stop is
//!   the launch line's, `Stop::branch` prepared as the stuck path's other texts are.
//! - A reminder is one send at a time like the rest: while another send to the session may have
//!   typed ([`SendTries::pending`]) the pass writes and sends nothing. The turn is recorded
//!   read before the reminder is recorded sent (`result-line-reminded`), so a pass cut between
//!   the two neither hangs nor goes stuck on the turn it reminded. A reminder the worker
//!   refused as `NotIdle` is sent again while the session's turn is the same; a newer turn
//!   answers a begun reminder whichever message started it, as in the runner, and one that
//!   ends with no line is stuck with the run's own reason.
//! - Putting the session's pull request back to draft fails as the runner has it only when
//!   `gh` ran and the pull request cannot be made a draft (`OtherKind::Failed`, `NotFound`):
//!   stuck, in the runner's words. Any other failure (a rate limit, a timeout, a refused
//!   login, `gh` not starting, an answer that cannot be read) is an error with nothing more
//!   written, and the next pass asks again.
//! - The runner's case of a run with no ordinal and no result line, stuck at once, has no
//!   counterpart: a turn always has a number. Its cases about a run a person stopped
//!   (`stop-run.sh`), a run whose agent process died, `refused before anything is read`, the
//!   pull request a session leaves, a branch an earlier attempt's session made and a Paradev
//!   Cloud launch branch are the `stop` and `resume` steps', or T3's (tasks 24 and 28); the
//!   runner's `watch` is the text check, which another task owns. A merged or closed pull
//!   request and a closed issue (`advance: a closed issue`) are carried out by the shared
//!   [`end_run`], [`abandon`] and [`end_own_merge`]; the cases
//!   here are Foundry's own.

use foundry_proto::factory::{RunEvent, RunId, SessionTurn};
use foundry_proto::ids::SessionId;
use foundry_proto::state::SessionState;

use super::progress::{
    RESULT_LINE_REMINDED, RESULT_LINE_REMINDING, Seen, SendTries, TIME_LIMIT as TIME_LIMIT_CODE,
    pending_relaunch, read_progress,
};
use super::{
    ActionPayload, Closing, Progress, RunContext, RunError, Stop, StuckStop, abandon,
    apply_actions, closed_issue, end_own_merge, end_run, finish_building, go_stuck, progress_from,
    recorded_reason, session_id, stop_session, workspace_id,
};
use crate::factory::github::{IssueDetail, OtherKind, PullRequest, TrackerError};
use crate::factory::ledger::events::{Cause, CauseStep, Event, Outcome, names};
use crate::factory::lifecycle::result_line::{
    self, NO_MESSAGE, NO_RESULT_LINE, SAID_STUCK, SessionResult,
};
use crate::factory::lifecycle::{Event as LifecycleEvent, State, transition};
use crate::factory::messages::{
    self, SESSION_GONE, WRONG_ACCOUNT, WRONG_ACCOUNT_PREFIX, prepare_session_text,
    time_limit_reason,
};
use crate::factory::text::elapsed_ms;
use crate::sessions::factory::SendRefused;
use crate::sessions::turns::TurnText;

/// The ledger step written last when a turn was acted on and the run goes on: `turn-read <n>`.
/// A turn number only rises, so the newest one read since the session was launched is the
/// highest.
pub const TURN_READ: &str = "turn-read ";

/// The public reason of a session that stopped to wait for a person.
pub const SESSION_WAITING: &str = "the session stopped to wait for a person";

/// The code of the stuck path of a session that waited for a person.
const WAITING_CODE: &str = "session-waiting";
/// The code of the stuck path of a session that reported a pull request that is not its own.
const NOT_BOUND_CODE: &str = "pr-not-bound";
/// The code of the stuck path of any other stop the session reported.
const STUCK_CODE: &str = "session-stuck";
/// The code of the stuck path of a session whose `gh` acted as another account.
const WRONG_ACCOUNT_CODE: &str = "session-wrong-account";
/// The code of the stuck path of a session that is gone when the reminder is sent.
const GONE_CODE: &str = "session-gone";

/// What one pass of [`advance`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildStep {
    /// The session works, waits for a person to act on nothing, or its newest turn was read
    /// already. Nothing decided.
    Running,
    /// The newest turn ended with no result line and the one reminder was sent.
    Reminded,
    /// The session's pull request is bound to the run and waits for the gate (also when a
    /// recorded gate pass is not finished: the gate finishes it).
    ReadyForGate { pr: u64, url: String },
    /// Questions on the issue, session ended, workspace archived by force, run ended
    /// `needs-info`.
    NeedsInfo,
    /// The run went stuck; `reason` is what the ledger records.
    Stuck { reason: String },
    /// The session's process ended, or the session is gone, before a result line was read: an
    /// interruption, which `resume` decides. Nothing written.
    Interrupted,
    /// Not this step's run: nothing written.
    Skipped { reason: String },
    /// A person closed the issue: the run is ended.
    Abandoned,
    /// The run's pull request was merged or closed (or its own merge closed the issue): the run
    /// is ended.
    Ended,
}

/// What [`time_limit`] decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LimitStep {
    /// The run has not worked longer than its limit.
    Within,
    /// The run passed its limit and went stuck.
    Stopped { reason: String },
    /// Not a run the limit applies to: nothing was written.
    Skipped { reason: String },
}

/// One pass over `run`, whose session builds. See the module's words.
pub fn advance(ctx: &RunContext<'_>, run: &RunId) -> Result<BuildStep, RunError> {
    let issue = run.issue();
    let events = ctx.ledger.read(run)?;
    let seen = read_progress(&events);
    if let Some(reason) = not_this_steps(run, &seen) {
        return Ok(BuildStep::Skipped { reason });
    }
    // A closed issue is a person's move, or the run's own merged pull request, which ends the
    // run here unless a recorded pass has the gate finish it.
    let (closing, detail) = closed_issue(ctx, run, &seen)?;
    match closing {
        Closing::PersonsMove => {
            abandon(ctx, run, &seen, CauseStep::Advance)?;
            return Ok(BuildStep::Abandoned);
        }
        Closing::OwnMerge(pull) if seen.pass.is_none() => {
            end_own_merge(ctx, run, &seen, CauseStep::Advance, &pull)?;
            return Ok(BuildStep::Ended);
        }
        Closing::OwnMerge(_) | Closing::Open => {}
    }
    let mut at = progress_from(run, CauseStep::Advance, &seen, State::Triage);
    // A stuck decision an older version recorded for a run it left in `needs-info` is not
    // carried out: that run is ended below and the issue stays `needs-info`.
    if at.state != State::NeedsInfo && (seen.stuck.is_some() || seen.stuck_stop.is_some()) {
        return finish_begun_stuck(ctx, run, &seen, &mut at);
    }
    // `state_changed` is the commit marker: a run in `needs-info` has had every write done, and
    // a person may have changed the issue since, so it is ended and nothing is written on GitHub.
    if at.state == State::NeedsInfo {
        release_old_claim(ctx, issue, detail.as_ref())?;
        end_needs_info(ctx, &at)?;
        return Ok(BuildStep::NeedsInfo);
    }
    let Some(thread) = seen.thread_id.clone() else {
        // The launch is `start`'s while the run is in `triage`; a building run with no session
        // is a ledger that cannot be right.
        return if at.state == State::Triage {
            Ok(BuildStep::Running)
        } else {
            Err(RunError::Invalid(format!(
                "run {run} is {} with no session recorded",
                at.state
            )))
        };
    };
    // The gate has the pull request: it finishes a recorded pass, and nothing here looks at
    // the session or the pull request again.
    if seen.pass.is_some()
        && let Some(last) = &seen.last_pr
    {
        return Ok(BuildStep::ReadyForGate {
            pr: last.pr,
            url: last.url.clone(),
        });
    }
    if at.state == State::Triage {
        // `start` looks at the issue text again after the launch. A launch it has not done that
        // for is left to it, so this cannot give the run its `building` label around the check.
        if !seen.rechecked {
            return Ok(BuildStep::Running);
        }
        finish_building(ctx, issue, &mut at)?;
    }
    let session = session_id(&thread)?;
    let Some(state) = ctx.view.session_state(session) else {
        return Ok(BuildStep::Interrupted);
    };
    let turn = ctx.view.session_turn(session).cloned();
    let read = newest_turn_read(&events);
    let unread = |turn: &SessionTurn| turn.turn > 0 && read.is_none_or(|read| turn.turn > read);
    let waiting = |turn: &Option<SessionTurn>| turn.as_ref().is_some_and(|turn| turn.question);
    let step = Step {
        ctx,
        run,
        seen: &seen,
        session,
    };
    match state {
        // P1 marks a session stuck after 10 minutes of silence, and a long build is silent for
        // longer: the factory does nothing, and `limits.minutes` ends the run. A machine that
        // cannot be reached is `resume`'s to measure.
        SessionState::Unreachable
        | SessionState::Stuck
        | SessionState::Starting
        | SessionState::Working => Ok(BuildStep::Running),
        SessionState::WaitingForYou => step.waiting(&mut at, turn.as_ref()),
        SessionState::Idle => {
            if waiting(&turn) {
                return step.waiting(&mut at, turn.as_ref());
            }
            let Some(turn) = turn.filter(|turn| turn.turn > 0) else {
                // No turn ended yet: the launch is `start`'s to send again.
                return Ok(BuildStep::Running);
            };
            if !unread(&turn) {
                return Ok(step.read_already());
            }
            step.act(&mut at, &turn)
        }
        SessionState::Finished | SessionState::Failed => {
            let Some(turn) = turn.filter(unread) else {
                return Ok(step.read_already_or(BuildStep::Interrupted));
            };
            // The session's process ended, which `resume` decides, unless its last words are
            // the result: a done message wins over the way the session ended.
            let message = read_turn(ctx, session, turn.turn)?.message;
            match result_line::read(Some(&message), ctx.repo()) {
                SessionResult::Done { pr, url } => step.done(&mut at, &turn, pr, &url, &message),
                _ => Ok(BuildStep::Interrupted),
            }
        }
    }
}

/// Why a run is not this step's, or `None` for one that is.
fn not_this_steps(run: &RunId, seen: &Seen) -> Option<String> {
    let reason = if seen.finished {
        "is finished"
    } else if seen.stop_requested {
        "was asked to stop, which the stop step carries out"
    } else if seen
        .interruption
        .as_ref()
        .is_some_and(|interruption| interruption.relaunch || !interruption.sent)
        || pending_relaunch(seen).is_some()
    {
        "has an interruption or a relaunch decided, which the resume step carries out"
    } else {
        return match seen.state {
            Some(State::Triage | State::Building | State::NeedsInfo) => None,
            Some(State::Stuck) if seen.stuck.is_some() || seen.stuck_stop.is_some() => None,
            state => Some(format!(
                "run {run} is in state {}, which another step owns",
                state.map_or("none", State::name)
            )),
        };
    };
    Some(format!("run {run} {reason}"))
}

/// The newest turn read since the session was launched: the highest `turn-read <n>` step after
/// the newest `session_launched` line.
fn newest_turn_read(events: &[RunEvent]) -> Option<u64> {
    let mut newest = None;
    for event in events {
        if event.event == names::SESSION_LAUNCHED {
            newest = None;
        } else if event.event == names::STEP_CHANGED
            && let Some(n) = event
                .data
                .get("step")
                .and_then(serde_json::Value::as_str)
                .and_then(|step| step.strip_prefix(TURN_READ))
                .and_then(canonical)
        {
            newest = Some(newest.map_or(n, |read: u64| read.max(n)));
        }
    }
    newest
}

/// `text` as a number written the way this step writes one: digits only, no sign, no leading
/// zero except `0` itself.
fn canonical(text: &str) -> Option<u64> {
    let plain = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    if plain { text.parse().ok() } else { None }
}

/// `text` prepared as session text, or `None` when nothing is left of it.
fn prepared(text: &str) -> Option<String> {
    let text = prepare_session_text(text);
    (!text.is_empty()).then_some(text)
}

/// The final message and the question of turn `n` of `session`.
fn read_turn(ctx: &RunContext<'_>, session: SessionId, n: u64) -> Result<TurnText, RunError> {
    ctx.held()?;
    Ok(ctx.worker.turn(session, n)?)
}

/// Whether the first non-empty line of what a stuck session said is the one the launch message
/// asks for when `gh` acts as another account.
fn says_wrong_account(session_text: Option<&str>) -> bool {
    session_text
        .and_then(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
        .is_some_and(|line| line.starts_with(WRONG_ACCOUNT_PREFIX))
}

/// Whether the session ended without any result line: no message, or a last line that is none
/// of the three. A malformed `FACTORY-DONE`, or questions with no questions, is not this.
fn lacks_result_line(reason: &str) -> bool {
    reason == NO_MESSAGE || reason == NO_RESULT_LINE
}

/// Whether a send to the session may have typed and is not known typed: no other send goes
/// until it is settled (the rule of `send_once`, which the reminder is outside of).
fn send_pending(seen: &Seen) -> bool {
    seen.sends.values().any(SendTries::pending)
}

/// Gives back an issue that an older version's claim left `factory:building` and assigned to
/// the login for a run now left in `needs-info`. Only that exact shape is touched; anything else
/// is a person's.
fn release_old_claim(
    ctx: &RunContext<'_>,
    issue: u64,
    read: Option<&IssueDetail>,
) -> Result<(), RunError> {
    let fetched;
    let detail = match read {
        Some(detail) => detail,
        None => {
            fetched = ctx.tracker.issue(ctx.repo(), issue)?;
            &fetched
        }
    };
    let ours =
        detail.assignees.len() == 1 && detail.assignees[0].eq_ignore_ascii_case(&ctx.project.login);
    if detail.state == Some(State::Building) && ours {
        ctx.set_state(issue, State::NeedsInfo)?;
        ctx.unassign(issue)?;
    }
    Ok(())
}

/// Ends a run whose questions are on the issue: the session is ended, its workspace archived by
/// force, and the ledger finished as `needs-info`. Nothing is written on GitHub: the questions,
/// the label and the release were done before `state_changed`, which is what says so. An archive
/// that fails is an error with nothing written, so the next call ends the run.
fn end_needs_info(ctx: &RunContext<'_>, at: &Progress) -> Result<(), RunError> {
    stop_session(ctx, at)?;
    if let Some(workspace) = &at.worktree {
        ctx.archive(workspace_id(workspace)?)?;
    }
    ctx.ledger.finish(
        &at.run,
        Outcome::NeedsInfo,
        None,
        None,
        &Cause::new(CauseStep::Advance, "questions-asked"),
        None,
    )?;
    Ok(())
}

/// Carries out the stuck path the ledger shows begun, with the words it recorded. The session's
/// text is not in the decision (its turn's, read again here); a read that fails posts the reason
/// alone.
fn finish_begun_stuck(
    ctx: &RunContext<'_>,
    run: &RunId,
    seen: &Seen,
    at: &mut Progress,
) -> Result<BuildStep, RunError> {
    let issue = run.issue();
    if let Some(reason) = &seen.stuck {
        go_stuck(
            ctx,
            issue,
            at,
            "stuck-resumed",
            Stop::new(reason.clone()),
            None,
        )?;
        return Ok(BuildStep::Stuck {
            reason: reason.clone(),
        });
    }
    let Some(raw) = &seen.stuck_stop else {
        return Err(RunError::Invalid(format!("run {run} has no stuck path")));
    };
    let decision: StuckStop = serde_json::from_str(raw)
        .map_err(|error| RunError::Invalid(format!("the stuck decision of {run}: {error}")))?;
    let mut stop = decision.stop;
    stop.detail = seen.stuck_detail.clone();
    stop.session_text = session_text_again(ctx, seen, &decision.code)?;
    let reason = recorded_reason(&stop);
    go_stuck(ctx, issue, at, &decision.code, stop, None)?;
    Ok(BuildStep::Stuck { reason })
}

/// What the session said that a stuck path with `code` quotes, read again from the session's
/// newest turn and prepared. `None` when the code quotes none, or the turn cannot be read.
fn session_text_again(
    ctx: &RunContext<'_>,
    seen: &Seen,
    code: &str,
) -> Result<Option<String>, RunError> {
    if !matches!(
        code,
        WAITING_CODE | NOT_BOUND_CODE | STUCK_CODE | WRONG_ACCOUNT_CODE
    ) {
        return Ok(None);
    }
    let Some(thread) = &seen.thread_id else {
        return Ok(None);
    };
    let session = session_id(thread)?;
    let Some(turn) = ctx.view.session_turn(session) else {
        return Ok(None);
    };
    let text = match read_turn(ctx, session, turn.turn) {
        Ok(text) => text,
        Err(error @ RunError::LeaseLost(_)) => return Err(error),
        Err(_) => return Ok(None),
    };
    let said = if code == WAITING_CODE {
        text.question
    } else if code == NOT_BOUND_CODE {
        Some(text.message)
    } else {
        match result_line::read(Some(&text.message), ctx.repo()) {
            SessionResult::Stuck { session_text, .. } => session_text,
            _ => Some(text.message),
        }
    };
    Ok(said.as_deref().and_then(prepared))
}

/// Why a pull request is not the session's own. `public` is one of the factory's fixed lines;
/// `detail` is set when the public line must leave out something that came from the pull
/// request, and the ledger gets it instead.
#[derive(Debug, PartialEq, Eq)]
struct Unbound {
    public: String,
    detail: Option<String>,
}

/// Binds a pull request to a run: it is in this repo, not a fork, merges into `base`, and its
/// head is one of the run's `branches`. The text comes from a session that issue text can steer,
/// so it must not be able to name someone else's pull request.
fn check_binding(
    pull: Option<&PullRequest>,
    pr: u64,
    base: &str,
    branches: &[&str],
) -> Option<Unbound> {
    let reported = format!("the session reported PR #{pr} but");
    let plain = |public: String| Unbound {
        public,
        detail: None,
    };
    let Some(pull) = pull else {
        return Some(plain(format!("{reported} it does not exist")));
    };
    if pull.cross_repository {
        return Some(plain(format!("{reported} it comes from a fork")));
    }
    if pull.base_ref != base {
        let run_base = prepare_session_text(base);
        return Some(Unbound {
            public: format!("{reported} its base is not the base branch {run_base}"),
            detail: Some(format!(
                "{reported} its base {} is not the base branch {run_base}",
                prepare_session_text(&pull.base_ref)
            )),
        });
    }
    if branches.is_empty() {
        return Some(plain(format!("{reported} the session has no branch")));
    }
    if !branches.contains(&pull.head_ref.as_str()) {
        return Some(Unbound {
            public: format!("{reported} its branch is not this session's branch"),
            detail: Some(format!(
                "{reported} its branch {} is not this session's branch",
                prepare_session_text(&pull.head_ref)
            )),
        });
    }
    None
}

/// What a pass over one session knows.
struct Step<'a, 'b> {
    ctx: &'a RunContext<'b>,
    run: &'a RunId,
    seen: &'a Seen,
    session: SessionId,
}

impl Step<'_, '_> {
    fn cause(&self, reason: &str) -> Cause {
        Cause::new(CauseStep::Advance, reason)
    }

    /// The turn acted on, recorded as the last line of acting on it.
    fn mark_read(&self, n: u64) -> Result<(), RunError> {
        self.ctx.record_step(
            self.run,
            &format!("{TURN_READ}{n}"),
            &self.cause("turn-read"),
        )
    }

    /// The step of a session whose newest turn was read already: a pull request waiting for
    /// its gate stays so, and nothing is sent to a session that idles through it.
    fn read_already(&self) -> BuildStep {
        self.read_already_or(BuildStep::Running)
    }

    fn read_already_or(&self, otherwise: BuildStep) -> BuildStep {
        match &self.seen.last_pr {
            Some(last) if self.seen.pr_reported => BuildStep::ReadyForGate {
                pr: last.pr,
                url: last.url.clone(),
            },
            _ => otherwise,
        }
    }

    /// The run goes stuck under `code`, with `stop`.
    fn stuck(&self, at: &mut Progress, code: &str, stop: Stop) -> Result<BuildStep, RunError> {
        let reason = recorded_reason(&stop);
        go_stuck(self.ctx, self.run.issue(), at, code, stop, None)?;
        Ok(BuildStep::Stuck { reason })
    }

    /// A session that waits for a person is stuck at once, with its question.
    fn waiting(
        &self,
        at: &mut Progress,
        turn: Option<&SessionTurn>,
    ) -> Result<BuildStep, RunError> {
        let question = match turn {
            Some(turn) => read_turn(self.ctx, self.session, turn.turn)?.question,
            None => None,
        };
        let stop = Stop {
            reason: SESSION_WAITING.into(),
            session_text: question.as_deref().and_then(prepared),
            ..Stop::default()
        };
        self.stuck(at, WAITING_CODE, stop)
    }

    /// Reads the turn that ended and acts on its result line.
    fn act(&self, at: &mut Progress, turn: &SessionTurn) -> Result<BuildStep, RunError> {
        let seen = self.seen;
        // A reminder begun and not recorded sent: a newer turn means it arrived, and the step is
        // made good; the same turn means it may not have, and it is sent again under the count.
        let mut reminded = seen.reminded;
        if let Some(k) = seen.reminding
            && !reminded
        {
            if turn.turn > u64::from(k) {
                self.ctx.record_step(
                    self.run,
                    RESULT_LINE_REMINDED,
                    &self.cause("result-line-reminded"),
                )?;
                reminded = true;
            } else if send_pending(seen) {
                return Ok(BuildStep::Running);
            } else {
                return self.send_reminder(at, turn, None);
            }
        }
        let message = read_turn(self.ctx, self.session, turn.turn)?.message;
        match result_line::read(Some(&message), self.ctx.repo()) {
            SessionResult::Done { pr, url } => self.done(at, turn, pr, &url, &message),
            SessionResult::NeedsInfo { questions } => {
                let questions = prepare_session_text(&questions);
                let (_, actions) =
                    transition(at.state, &LifecycleEvent::QuestionsRaised { questions })?;
                apply_actions(
                    self.ctx,
                    self.run.issue(),
                    at,
                    &actions,
                    &self.cause("questions-asked"),
                    &ActionPayload::default(),
                )?;
                end_needs_info(self.ctx, at)?;
                Ok(BuildStep::NeedsInfo)
            }
            SessionResult::Stuck {
                reason,
                session_text,
            } => {
                if reason == SAID_STUCK && says_wrong_account(session_text.as_deref()) {
                    let stop = Stop {
                        reason: WRONG_ACCOUNT.into(),
                        session_text: session_text.as_deref().and_then(prepared),
                        ..Stop::default()
                    };
                    return self.stuck(at, WRONG_ACCOUNT_CODE, stop);
                }
                if lacks_result_line(reason) && !reminded {
                    // One message at a time to a session: another send that may have typed is
                    // settled first, and nothing is written meanwhile.
                    if send_pending(seen) {
                        return Ok(BuildStep::Running);
                    }
                    self.ctx.record_step(
                        self.run,
                        &format!("{RESULT_LINE_REMINDING} {}", turn.turn),
                        &self.cause("result-line-reminding"),
                    )?;
                    return self.send_reminder(at, turn, Some(&message));
                }
                let stop = Stop {
                    reason: reason.into(),
                    session_text: session_text.as_deref().and_then(prepared),
                    ..Stop::default()
                };
                self.stuck(at, STUCK_CODE, stop)
            }
        }
    }

    /// Sends the session its one reminder to end with a result line, under the turn's count of
    /// prompts, and records it. `message` is the turn's, when it was read already.
    fn send_reminder(
        &self,
        at: &mut Progress,
        turn: &SessionTurn,
        message: Option<&str>,
    ) -> Result<BuildStep, RunError> {
        let sent = self
            .ctx
            .send(self.session, messages::reminder(), Some(turn.prompts));
        match sent {
            // A count that rose means the line went in, or a person typed: either way the
            // session takes another turn, and typing again would be a second reminder.
            // The turn is recorded read first: a pass cut between the two lines finds the turn
            // read (the session is given time to answer, and is not reminded twice), and its
            // newer turn is the answer, which records `result-line-reminded` first. Reminded
            // first would read the same turn again as ended with no line and go stuck at once.
            Ok(_) | Err(RunError::Send(SendRefused::CountRose { .. })) => {
                self.mark_read(turn.turn)?;
                self.ctx.record_step(
                    self.run,
                    RESULT_LINE_REMINDED,
                    &self.cause("result-line-reminded"),
                )?;
                Ok(BuildStep::Reminded)
            }
            // The next pass looks again.
            Err(RunError::Send(SendRefused::NotIdle)) => Ok(BuildStep::Running),
            Err(RunError::Send(SendRefused::NoSuchSession)) => {
                self.stuck(at, GONE_CODE, Stop::new(SESSION_GONE))
            }
            // The same message is refused again: the run is stuck at once.
            Err(RunError::Send(
                refused @ (SendRefused::BodyWithoutPath | SendRefused::SecretsUnreadable),
            )) => {
                let message = match message {
                    Some(message) => message.to_owned(),
                    None => read_turn(self.ctx, self.session, turn.turn)?.message,
                };
                let reason = match result_line::read(Some(&message), self.ctx.repo()) {
                    SessionResult::Stuck { reason, .. } if lacks_result_line(reason) => reason,
                    _ => NO_RESULT_LINE,
                };
                let stop = Stop {
                    reason: reason.into(),
                    detail: Some(format!("{reason}; Foundry refused the reminder: {refused}")),
                    session_text: prepared(&message),
                    ..Stop::default()
                };
                self.stuck(at, STUCK_CODE, stop)
            }
            // A send that may have typed, or a lost lease: an error, and the next pass sends
            // again under the same count.
            Err(error) => Err(error),
        }
    }

    /// The session reported pull request `pr`: bound to the run, set to draft, recorded once
    /// and handed to the gate, or the run ends with it.
    fn done(
        &self,
        at: &mut Progress,
        turn: &SessionTurn,
        pr: u64,
        url: &str,
        message: &str,
    ) -> Result<BuildStep, RunError> {
        let ctx = self.ctx;
        let seen = self.seen;
        let pull = ctx.tracker.pull_request(ctx.repo(), pr)?;
        let base = seen.base.as_deref().unwrap_or(&ctx.config.base);
        let branches: Vec<&str> = seen
            .last_branch
            .iter()
            .chain(&seen.resumed_from)
            .chain(&seen.launch_resumed_from)
            .chain(&seen.bound_branches)
            .map(String::as_str)
            .collect();
        if let Some(unbound) = check_binding(pull.as_ref(), pr, base, &branches) {
            let stop = Stop {
                reason: unbound.public,
                detail: unbound.detail,
                session_text: prepared(message),
                ..Stop::default()
            };
            return self.stuck(at, NOT_BOUND_CODE, stop);
        }
        let pull = pull.ok_or_else(|| RunError::Invalid("a bound pull request is gone".into()))?;
        // The session's pull request that is open and ready for review goes back to draft:
        // only the gate marks it ready. A call that fails leaves it as it is, and stuck.
        if pull.open && !pull.is_draft {
            match ctx.mark_pr_draft(pr) {
                Ok(()) => ctx.record(
                    self.run,
                    Event::PrSetDraft { pr },
                    &self.cause("pr-set-draft"),
                )?,
                // Only an answer that says `gh` ran and the pull request cannot be made a draft
                // is stuck, in the runner's words for this case. A rate limit, a timeout, a
                // refused login, `gh` not starting or an answer that cannot be read say nothing
                // about the pull request: an error with nothing more written, asked again next
                // pass.
                Err(RunError::Tracker(
                    TrackerError::NotFound { .. }
                    | TrackerError::Other {
                        kind: OtherKind::Failed,
                        ..
                    },
                )) => {
                    let stop = Stop {
                        reason: format!("the session reported PR #{pr} but it is not a draft"),
                        session_text: prepared(message),
                        ..Stop::default()
                    };
                    return self.stuck(at, NOT_BOUND_CODE, stop);
                }
                Err(error) => return Err(error),
            }
        }
        // Once per hand-off to the gate: the first report, and the report after each send-back.
        if !seen.opened.contains(&pr) || !seen.pr_reported {
            ctx.record(
                self.run,
                Event::PrOpened {
                    pr,
                    url: url.to_owned(),
                },
                &self.cause("pr-opened"),
            )?;
        }
        // A pull request a person merged or closed before the gate looked at it ends the run:
        // nothing is left to gate.
        if !pull.open {
            end_run(ctx, at, url, Some(&pull), CauseStep::Advance)?;
            return Ok(BuildStep::Ended);
        }
        self.mark_read(turn.turn)?;
        Ok(BuildStep::ReadyForGate {
            pr,
            url: url.to_owned(),
        })
    }
}

/// Stops `run` when it has worked longer than `limits.minutes`, with "now" the system's wall
/// clock on the ledger's stamping base ([`system_ms`](crate::factory::ledger::system_ms)).
/// Called by the pass before any step. The time is taken from the timestamps the events carry
/// (spec, `limits.minutes`), so the running stint ends at the wall clock, not at the
/// sleep-counting clock of [`RunContext::clock`], whose readings are never compared with a
/// stored record. See [`time_limit_at`].
pub fn time_limit(ctx: &RunContext<'_>, run: &RunId) -> Result<LimitStep, RunError> {
    time_limit_at(ctx, run, crate::factory::ledger::system_ms())
}

/// [`time_limit`] with "now" given, in Unix epoch milliseconds on the ledger's stamping base:
/// what the running stint's start (`working_since`) is on. A "now" before the start adds no
/// time ([`elapsed_ms`]), as a wall clock that stepped back would give. Skips a finished run, a
/// relaunch decided, a run that is not working, one stuck or asked to stop, one whose stuck
/// path or questions comment the advance step began (it finishes them with the words it
/// decided on), and a launch sent with no session recorded. A pull request that was reported
/// does not skip the run (spec, `limits.minutes`): the time a gate ran and the checks were
/// waited for are already left out of [`Seen::worked_ms`].
pub fn time_limit_at(
    ctx: &RunContext<'_>,
    run: &RunId,
    now_ms: u64,
) -> Result<LimitStep, RunError> {
    let seen = ctx.seen(run)?;
    let skipped = |why: &str| {
        Ok(LimitStep::Skipped {
            reason: format!("run {run} {why}"),
        })
    };
    if seen.finished {
        return skipped("is finished");
    }
    if seen
        .interruption
        .as_ref()
        .is_some_and(|interruption| interruption.relaunch)
    {
        return skipped("has a relaunch decided, which the resume step carries out");
    }
    let working = matches!(
        seen.state,
        None | Some(State::Ready | State::Triage | State::Building)
    );
    if !working || seen.stuck.is_some() || seen.stop_requested || seen.questions_posted {
        return skipped("is not running");
    }
    // A decision to go stuck, or the questions' comment, that the advance step began: it
    // finishes it, and the reason it decided on is the one the run ends with.
    if seen.stuck_stop.is_some() || seen.questions_commenting.is_some() {
        return skipped("has a stuck path or a questions comment begun, which advance finishes");
    }
    // The launch was sent and no session recorded: `resume` names its session.
    if seen.thread_id.is_none() && seen.launch_sent {
        return skipped("has a launch that was sent and no session recorded");
    }
    let worked = seen.worked_ms
        + seen
            .working_since
            .map_or(0, |since| elapsed_ms(since, now_ms));
    if worked <= ctx.config.limits.minutes.saturating_mul(60_000) {
        return Ok(LimitStep::Within);
    }
    // Only a run that has reported a pull request can have been closed by its own merge: no read
    // for any other.
    if !seen.opened.is_empty()
        && let (Closing::OwnMerge(_), _) = closed_issue(ctx, run, &seen)?
    {
        return skipped("was ended by its own merged pull request");
    }
    let stop = Stop {
        reason: time_limit_reason(worked, None),
        branch: seen.last_branch.as_deref().and_then(prepared),
        commits_ahead: None,
        ..Stop::default()
    };
    let reason = recorded_reason(&stop);
    // Called by the pass before any step, so the pass is what decided.
    let mut at = progress_from(run, CauseStep::Pass, &seen, State::Triage);
    go_stuck(ctx, run.issue(), &mut at, TIME_LIMIT_CODE, stop, None)?;
    Ok(LimitStep::Stopped { reason })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use foundry_proto::factory::SessionTurn;
    use foundry_proto::ids::{SessionId, TerminalId, WorkspaceId};
    use foundry_proto::record::{Record, Session};
    use foundry_proto::state::SessionState;

    use super::super::testing::*;
    use super::*;
    use crate::factory::github::{Hiding, Tracker, TrackerError};
    use crate::factory::ledger::events::{Event, ResumedFrom};
    use crate::factory::ledger::testing::T0;
    use crate::factory::lifecycle::State;
    use crate::factory::lifecycle::result_line::{
        MALFORMED_URL, NO_MESSAGE, NO_QUESTIONS, NO_RESULT_LINE, NOT_ON_REPO, SAID_STUCK,
    };
    use crate::factory::messages::{
        self, SESSION_GONE, TIME_LIMIT, WRONG_ACCOUNT, WorkLeft, reminder,
    };
    use crate::factory::worker::{Call, FleetView, WorkerError};
    use crate::sessions::factory::{FactoryStart, SendRefused};
    use crate::sessions::turns::TurnText;

    const SESSION: &str = "00000000-0000-0000-0000-0000000005e5";
    const WORKSPACE: &str = "00000000-0000-0000-0000-000000000a11";
    const BRANCH: &str = "fac/8-1";
    const URL12: &str = "https://github.com/rawphp/softfac/pull/12";
    const DONE_MESSAGE: &str =
        "Opened it.\nFACTORY-DONE: https://github.com/rawphp/softfac/pull/12";
    const WORKING_MESSAGE: &str = "I am still working on it.";
    const QUESTIONS: &str = "1. Which database?\n2. Which port?";
    const MINUTE: u64 = 60_000;
    const LIMIT_MS: u64 = 90 * MINUTE;

    fn session() -> SessionId {
        SessionId(uuid::Uuid::from_u128(0x5e5))
    }

    fn workspace() -> WorkspaceId {
        WorkspaceId(uuid::Uuid::from_u128(0xa11))
    }

    fn machine() -> foundry_proto::ids::MachineId {
        foundry_proto::ids::MachineId(uuid::Uuid::from_u128(0x2))
    }

    /// The session's launch line, on `branch`.
    fn launch(branch: Option<&str>, resumed: Option<&str>, base: Option<&str>) -> Event {
        Event::SessionLaunched {
            thread_id: SESSION.into(),
            worktree: Some(WORKSPACE.into()),
            branch: branch.map(str::to_owned),
            model_instance: None,
            model: Some("opus".into()),
            model_options: None,
            resumed_from: resumed.map(|branch| ResumedFrom {
                run: "rawphp__softfac__7__1".into(),
                branch: branch.into(),
            }),
            base: base.map(str::to_owned),
            title: Some("Make it faster".into()),
        }
    }

    /// A run that was claimed and launched, in `triage`; its issue text was checked again when
    /// `rechecked`.
    fn claimed_and_launched(rig: &Rig, run: &RunId, launch: Event, rechecked: bool) {
        rig.step(run, &format!("approved {LABELED_AT}"));
        rig.write(
            run,
            Event::Claimed {
                machine: machine().to_string(),
                github_user: LOGIN.into(),
            },
        );
        rig.moved(run, State::Ready, State::Triage);
        rig.step(run, &format!("workspace-created {WORKSPACE}"));
        if rechecked {
            rig.step(run, "approval-rechecked");
        }
        rig.write(run, launch);
        rig.sim.set_labels(&["factory:triage"]);
        rig.sim.set_assignees(&[LOGIN]);
    }

    /// `claimed_and_launched`, rechecked: what start leaves before it moves the label.
    fn started_with(rig: &Rig, run: &RunId, launch: Event) {
        claimed_and_launched(rig, run, launch, true);
    }

    /// A run that is building: `started_with`, moved to `building`.
    fn building_with(rig: &Rig, run: &RunId, launch: Event) {
        started_with(rig, run, launch);
        rig.moved(run, State::Triage, State::Building);
        rig.sim.set_labels(&["factory:building"]);
    }

    fn building(rig: &Rig, run: &RunId) {
        building_with(rig, run, launch(Some(BRANCH), None, Some("main")));
    }

    fn draft_on(head: &str) -> SimPull {
        SimPull {
            state: "OPEN",
            draft: true,
            head: head.into(),
            base: "main".into(),
            cross: false,
        }
    }

    /// The worker's session started, as the start step did, so that it takes a send as a live
    /// session does.
    fn start_the_session(rig: &Rig) {
        rig.worker.on_start_session(Ok(session()));
        rig.ctx()
            .start_session(
                workspace(),
                FactoryStart {
                    project: project_id(),
                    login: LOGIN.into(),
                    model: "opus".into(),
                    effort: "high".into(),
                },
                reminder(),
            )
            .unwrap();
    }

    /// A building run whose pull request #12 is an open draft on the session's branch.
    fn built() -> (Rig, RunId) {
        let rig = Rig::new();
        let run = run_id(1);
        building(&rig, &run);
        rig.sim.set_pull(12, draft_on(BRANCH));
        start_the_session(&rig);
        (rig, run)
    }

    fn session_record(state: SessionState) -> Record<Session> {
        Record {
            owner: machine(),
            version: 1,
            deleted: false,
            body: Session {
                id: session(),
                workspace: workspace(),
                terminal: TerminalId(uuid::Uuid::from_u128(0x5e6)),
                model: "opus".into(),
                state,
            },
        }
    }

    /// The fleet shows the session in `state`, with the newest turn `turn` after `prompts`
    /// prompts (and a question waiting when `question`). `turn` 0 shows no turn record.
    fn show_with(rig: &mut Rig, state: SessionState, turn: u64, prompts: u64, question: bool) {
        let mut view = FleetView::builder()
            .online(rig.machine)
            .session(session_record(state));
        if turn > 0 {
            view = view.turn(SessionTurn {
                session: session(),
                workspace: workspace(),
                project: project_id(),
                turn,
                prompts,
                question,
            });
        }
        rig.view = view.build();
    }

    fn show(rig: &mut Rig, state: SessionState, turn: u64, prompts: u64) {
        show_with(rig, state, turn, prompts, false);
    }

    /// What the worker will say the next turn read ended with.
    fn say(rig: &Rig, message: &str) {
        rig.worker.on_turn(Ok(TurnText {
            message: message.into(),
            question: None,
        }));
    }

    /// The session is idle after turn `turn` (and as many prompts), which ended with `message`.
    fn end_turn(rig: &mut Rig, turn: u64, message: &str) {
        show(rig, SessionState::Idle, turn, turn);
        say(rig, message);
    }

    fn pass(rig: &Rig, run: &RunId) -> BuildStep {
        advance(&rig.ctx(), run).unwrap()
    }

    /// The calls the run made to the worker, not counting the test's own start of the session.
    fn calls(rig: &Rig) -> Vec<Call> {
        rig.worker
            .calls()
            .into_iter()
            .filter(|call| !matches!(call, Call::StartSession { .. }))
            .collect()
    }

    fn turn_reads(rig: &Rig) -> usize {
        calls(rig)
            .iter()
            .filter(|call| matches!(call, Call::Turn { .. }))
            .count()
    }

    fn sends(rig: &Rig) -> Vec<Call> {
        calls(rig)
            .into_iter()
            .filter(|call| matches!(call, Call::Send { .. }))
            .collect()
    }

    fn reminder_sent(prompts: u64) -> Call {
        Call::Send {
            session: session(),
            message: reminder(),
            only_if_prompts: Some(prompts),
        }
    }

    fn ended_workspace() -> Call {
        Call::EndSessions {
            workspace: workspace(),
        }
    }

    fn archived() -> Call {
        Call::Archive {
            workspace: workspace(),
        }
    }

    /// The names of the events after the first `from`.
    fn names_after(rig: &Rig, run: &RunId, from: usize) -> Vec<String> {
        rig.names(run).split_off(from)
    }

    /// The `step_changed` texts after the first `from` events.
    fn steps_after(rig: &Rig, run: &RunId, from: usize) -> Vec<String> {
        rig.events(run)
            .into_iter()
            .skip(from)
            .filter(|e| e.event == "step_changed")
            .filter_map(|e| e.data["step"].as_str().map(str::to_owned))
            .collect()
    }

    fn cause_of_last(rig: &Rig, run: &RunId) -> (String, String) {
        let events = rig.events(run);
        let cause = events.last().and_then(|e| e.cause.clone()).unwrap();
        (
            cause["step"].as_str().unwrap().to_owned(),
            cause["reason"].as_str().unwrap().to_owned(),
        )
    }

    fn last_event(rig: &Rig, run: &RunId) -> foundry_proto::factory::RunEvent {
        rig.events(run).pop().unwrap()
    }

    /// No write on GitHub was made.
    fn no_writes(rig: &Rig) {
        assert_eq!(rig.sim.writes(), Vec::<String>::new());
    }

    /// How many times the whole issue was read (not just its labels, which the label moves read).
    fn issue_reads(rig: &Rig) -> usize {
        rig.sim
            .calls()
            .iter()
            .filter(|call| {
                call.first().map(String::as_str) == Some("issue")
                    && call.get(1).map(String::as_str) == Some("view")
                    && !call.iter().any(|arg| arg == "labels")
            })
            .count()
    }

    /// The run went stuck under `code`, as the stuck path of every step records it.
    #[track_caller]
    fn assert_stuck(
        rig: &Rig,
        run: &RunId,
        step: &BuildStep,
        code: &str,
        public: &str,
        detail: Option<&str>,
        text: Option<&str>,
    ) {
        let prepared = text.map(messages::prepare_session_text);
        let words = detail.unwrap_or(public);
        let recorded = match &prepared {
            Some(text) => format!("{words}\n\n{text}"),
            None => words.to_owned(),
        };
        assert_eq!(
            step,
            &BuildStep::Stuck {
                reason: recorded.clone()
            }
        );
        let comment = rig
            .sim
            .comments()
            .into_iter()
            .find(|comment| comment.contains("could not finish"))
            .expect("a stuck comment");
        assert!(
            comment.starts_with(&messages::stuck(public, prepared.as_deref(), None)),
            "{comment}"
        );
        let events = rig.events(run);
        let stuck = events
            .iter()
            .find(|e| e.event == "stuck")
            .expect("a stuck line");
        assert_eq!(stuck.data["reason"], recorded);
        let last = events.last().unwrap();
        assert_eq!(last.event, "finished");
        assert_eq!(last.data["outcome"], "stuck");
        assert_eq!(last.cause.as_ref().unwrap()["reason"], code);
        assert_eq!(rig.sim.labels(), ["factory:stuck"]);
        assert!(rig.sim.assignees().is_empty());
        assert!(calls(rig).contains(&ended_workspace()));
        assert!(!calls(rig).contains(&archived()));
    }

    /// A run whose reminder was sent for turn 1: the turn was read, and the session is idle
    /// after turn 2 (and two prompts), which ended with `message`.
    fn reminded_run(message: &str) -> (Rig, RunId) {
        let (mut rig, run) = built();
        rig.step(&run, "result-line-reminding 1");
        rig.step(&run, "result-line-reminded");
        rig.step(&run, "turn-read 1");
        end_turn(&mut rig, 2, message);
        (rig, run)
    }

    fn waiting_in_needs_info() -> (Rig, RunId) {
        let rig = Rig::new();
        let run = run_id(1);
        building(&rig, &run);
        rig.write(
            &run,
            Event::QuestionsPosted {
                questions: "Which one?".into(),
            },
        );
        rig.moved(&run, State::Building, State::NeedsInfo);
        rig.sim.set_labels(&["factory:needs-info"]);
        rig.sim.set_assignees(&[]);
        (rig, run)
    }

    // ----- the runner's `advance: while the session works` -----

    mod advance_while_the_session_works {
        use super::*;

        /// The runner's run statuses `preparing`, `queued`, `starting`, `running`, `waiting`
        /// are the fleet's `starting` and `working`; a session the fleet does not show at all
        /// is an interruption (`resume` decides), not a run in progress.
        #[test]
        fn does_nothing_while_the_run_is_active_even_for_a_message_that_looks_final() {
            for state in [
                SessionState::Starting,
                SessionState::Working,
                SessionState::Unreachable,
            ] {
                let (mut rig, run) = built();
                show(&mut rig, state, 1, 1);
                say(&rig, DONE_MESSAGE);
                let before = rig.events(&run).len();
                assert_eq!(pass(&rig, &run), BuildStep::Running, "{state:?}");
                no_writes(&rig);
                assert_eq!(calls(&rig), vec![], "{state:?}");
                assert_eq!(rig.events(&run).len(), before, "{state:?}");
            }
        }

        #[test]
        fn leaves_a_run_in_triage_to_start_until_the_text_was_checked_again() {
            let mut rig = Rig::new();
            let run = run_id(1);
            claimed_and_launched(&rig, &run, launch(Some(BRANCH), None, Some("main")), false);
            show(&mut rig, SessionState::Working, 1, 1);
            let before = rig.events(&run).len();
            assert_eq!(pass(&rig, &run), BuildStep::Running);
            no_writes(&rig);
            assert_eq!(calls(&rig), vec![]);
            assert_eq!(rig.events(&run).len(), before);
        }

        #[test]
        fn moves_a_rechecked_run_in_triage_to_building_then_reads_the_session() {
            let mut rig = Rig::new();
            let run = run_id(1);
            started_with(&rig, &run, launch(Some(BRANCH), None, Some("main")));
            show(&mut rig, SessionState::Working, 1, 1);
            let before = rig.events(&run).len();
            assert_eq!(pass(&rig, &run), BuildStep::Running);
            assert_eq!(rig.sim.labels(), ["factory:building"]);
            assert_eq!(names_after(&rig, &run, before), ["state_changed"]);
            assert_eq!(
                cause_of_last(&rig, &run),
                ("advance".into(), "session-started".into())
            );
        }
    }

    // ----- the runner's `advance: done` -----

    /// A pull request GitHub shows as `state`, on `head` into `base`.
    fn pull(state: &'static str, draft: bool, head: &str, base: &str, cross: bool) -> SimPull {
        SimPull {
            state,
            draft,
            head: head.into(),
            base: base.into(),
            cross,
        }
    }

    /// What the run records of an unbound pull request: the factory's words with the pull
    /// request's own detail, then the session's text.
    fn joined(words: &str, text: &str) -> String {
        format!("{words}\n\n{}", messages::prepare_session_text(text))
    }

    mod advance_done {
        use super::*;

        #[test]
        fn with_an_open_draft_pull_request_on_the_sessions_branch_is_ready_for_the_gate() {
            let (mut rig, run) = built();
            let before = rig.events(&run).len();
            end_turn(&mut rig, 1, DONE_MESSAGE);
            assert_eq!(
                pass(&rig, &run),
                BuildStep::ReadyForGate {
                    pr: 12,
                    url: URL12.into()
                }
            );
            // Only reads: the label stays building and the gate moves it on.
            no_writes(&rig);
            assert_eq!(
                names_after(&rig, &run, before),
                ["pr_opened", "step_changed"]
            );
            assert_eq!(steps_after(&rig, &run, before), ["turn-read 1"]);
            let opened = &rig.events(&run)[before];
            assert_eq!(opened.data["pr"], 12);
            assert_eq!(opened.data["url"], URL12);
            assert_eq!(opened.cause.as_ref().unwrap()["reason"], "pr-opened");
            assert_eq!(rig.sim.labels(), ["factory:building"]);
        }

        #[test]
        fn again_does_not_record_the_pull_request_twice() {
            let (mut rig, run) = built();
            end_turn(&mut rig, 1, DONE_MESSAGE);
            pass(&rig, &run);
            let after_first = rig.events(&run).len();
            assert_eq!(
                pass(&rig, &run),
                BuildStep::ReadyForGate {
                    pr: 12,
                    url: URL12.into()
                }
            );
            assert_eq!(rig.events(&run).len(), after_first);
            assert_eq!(rig.ctx().seen(&run).unwrap().opened, [12]);
        }

        /// The runner's failed run status is a session that ended: the fleet's `failed` and
        /// `finished`, where a done message in the newest turn still wins.
        #[test]
        fn counts_with_a_failed_status_when_the_pull_request_is_open() {
            for state in [SessionState::Failed, SessionState::Finished] {
                let (mut rig, run) = built();
                show(&mut rig, state, 1, 1);
                say(&rig, DONE_MESSAGE);
                assert!(
                    matches!(pass(&rig, &run), BuildStep::ReadyForGate { pr: 12, .. }),
                    "{state:?}"
                );
            }
        }

        #[test]
        fn with_a_pull_request_that_is_not_a_draft_puts_it_back_to_draft_and_goes_on() {
            let (mut rig, run) = built();
            rig.sim
                .set_pull(12, pull("OPEN", false, BRANCH, "main", false));
            let before = rig.events(&run).len();
            end_turn(&mut rig, 1, DONE_MESSAGE);
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
            let writes = rig.sim.writes();
            assert_eq!(writes.len(), 1, "{writes:?}");
            assert!(writes[0].starts_with("pr ready 12") && writes[0].ends_with("--undo"));
            assert_eq!(
                names_after(&rig, &run, before),
                ["pr_set_draft", "pr_opened", "step_changed"]
            );
            let reasons: Vec<String> = rig
                .events(&run)
                .iter()
                .skip(before)
                .map(|e| {
                    e.cause.as_ref().unwrap()["reason"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect();
            assert_eq!(reasons, ["pr-set-draft", "pr-opened", "turn-read"]);
        }

        #[test]
        fn with_a_pull_request_that_is_not_a_draft_and_cannot_be_drafted_is_stuck() {
            // `gh` ran and failed, and the pull request is still open: the runner's words, "it is
            // not a draft", for exactly this case.
            let (mut rig, run) = built();
            rig.sim
                .set_pull(12, pull("OPEN", false, BRANCH, "main", false));
            rig.sim.fail("--undo", 1);
            end_turn(&mut rig, 1, DONE_MESSAGE);
            let step = pass(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "pr-not-bound",
                "the session reported PR #12 but it is not a draft",
                None,
                Some(DONE_MESSAGE),
            );
        }

        /// A tracker whose answer to putting a pull request back to draft is `error`, whatever
        /// the pull request says: a transient failure, which is an error and writes nothing.
        struct Failing(Arc<Sim>, fn() -> TrackerError);

        impl Tracker for Failing {
            fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
                if args.iter().any(|arg| arg == "--undo") {
                    return Err((self.1)());
                }
                self.0.gh(args)
            }
        }

        /// The rule: only an answer that says `gh` ran and the pull request cannot be made a
        /// draft (`OtherKind::Failed`, `NotFound`) is stuck, with the runner's words. A rate
        /// limit, a timeout, an authentication refusal, `gh` not starting and an answer that
        /// cannot be read say nothing about the pull request: an error with nothing written, and
        /// the next pass asks again.
        #[test]
        fn with_a_rate_limit_on_putting_it_back_to_draft_is_an_error_not_stuck() {
            let transient: [fn() -> TrackerError; 5] = [
                || TrackerError::RateLimited {
                    command: "gh pr ready".into(),
                    stderr: "slow down".into(),
                },
                || TrackerError::Timeout {
                    command: "gh pr ready".into(),
                    seconds: 30,
                },
                || TrackerError::Auth {
                    command: "gh pr ready".into(),
                    stderr: "bad credentials".into(),
                },
                || TrackerError::Other {
                    kind: crate::factory::github::OtherKind::NotStarted,
                    message: "gh could not be started".into(),
                },
                || TrackerError::Other {
                    kind: crate::factory::github::OtherKind::Parse,
                    message: "gh answered with nonsense".into(),
                },
            ];
            for make in transient {
                let (mut rig, run) = built();
                rig.sim
                    .set_pull(12, pull("OPEN", false, BRANCH, "main", false));
                let sim = rig.sim.clone();
                rig.tracker = Hiding::new(
                    Arc::new(Failing(sim.clone(), make)),
                    Arc::new(|t: &str| t.into()),
                );
                end_turn(&mut rig, 1, DONE_MESSAGE);
                let before = rig.events(&run).len();
                let error = advance(&rig.ctx(), &run).unwrap_err();
                assert!(matches!(error, RunError::Tracker(_)), "{error}");
                // Nothing final was written: the run is still building and the next pass asks
                // again.
                assert_eq!(rig.events(&run).len(), before, "{error}");
                assert_eq!(rig.sim.labels(), ["factory:building"]);
                rig.tracker = Hiding::new(sim, Arc::new(|t: &str| t.into()));
                say(&rig, DONE_MESSAGE);
                assert!(matches!(
                    pass(&rig, &run),
                    BuildStep::ReadyForGate { pr: 12, .. }
                ));
                assert_eq!(names_after(&rig, &run, before)[0], "pr_set_draft");
            }
        }

        #[test]
        fn with_someone_elses_pull_request_that_is_not_a_draft_does_not_draft_it() {
            let (mut rig, run) = built();
            rig.sim
                .set_pull(12, pull("OPEN", false, "someone/else", "main", false));
            end_turn(&mut rig, 1, DONE_MESSAGE);
            let BuildStep::Stuck { reason } = pass(&rig, &run) else {
                panic!("expected a stuck run");
            };
            assert!(
                reason.starts_with(
                    "the session reported PR #12 but its branch someone/else is not this session's branch"
                ),
                "{reason}"
            );
            assert!(
                !rig.sim
                    .writes()
                    .iter()
                    .any(|write| write.contains("--undo"))
            );
        }

        #[test]
        fn with_a_merged_pull_request_on_someone_elses_branch_is_still_stuck() {
            let (mut rig, run) = built();
            rig.sim
                .set_pull(12, pull("MERGED", false, "someone/else", "main", false));
            end_turn(&mut rig, 1, DONE_MESSAGE);
            assert_eq!(
                pass(&rig, &run),
                BuildStep::Stuck {
                    reason: joined(
                        "the session reported PR #12 but its branch someone/else is not this session's branch",
                        DONE_MESSAGE
                    )
                }
            );
            assert_eq!(last_event(&rig, &run).event, "finished");
            assert_eq!(last_event(&rig, &run).data["outcome"], "stuck");
        }

        #[test]
        fn with_a_pull_request_that_does_not_exist_is_stuck() {
            let (mut rig, run) = built();
            // The simulator has no pull request 13.
            let message = "Done.\nFACTORY-DONE: https://github.com/rawphp/softfac/pull/13";
            end_turn(&mut rig, 1, message);
            let step = pass(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "pr-not-bound",
                "the session reported PR #13 but it does not exist",
                None,
                Some(message),
            );
        }

        #[test]
        fn with_someone_elses_pull_request_is_stuck_and_the_branch_stays_out_of_the_comment() {
            for head in ["someone/else", "main", "T3CODE/ABC", "t3code/abc2", ""] {
                let (mut rig, run) = built();
                rig.sim
                    .set_pull(12, pull("OPEN", true, head, "main", false));
                end_turn(&mut rig, 1, DONE_MESSAGE);
                let step = pass(&rig, &run);
                let detail = format!(
                    "the session reported PR #12 but its branch {head} is not this session's branch"
                );
                assert_stuck(
                    &rig,
                    &run,
                    &step,
                    "pr-not-bound",
                    "the session reported PR #12 but its branch is not this session's branch",
                    Some(&detail),
                    Some(DONE_MESSAGE),
                );
                assert!(!rig.names(&run).contains(&"pr_opened".to_owned()), "{head}");
            }
        }

        #[test]
        fn with_a_pull_request_from_a_fork_is_stuck() {
            let (mut rig, run) = built();
            rig.sim
                .set_pull(12, pull("OPEN", true, BRANCH, "main", true));
            end_turn(&mut rig, 1, DONE_MESSAGE);
            let step = pass(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "pr-not-bound",
                "the session reported PR #12 but it comes from a fork",
                None,
                Some(DONE_MESSAGE),
            );
        }

        #[test]
        fn with_a_pull_request_into_another_base_is_stuck_and_the_base_stays_out_of_the_comment() {
            for base in [
                "release/1.0",
                "develop",
                "MAIN",
                "main2",
                "release/main",
                "@octocat",
            ] {
                let (mut rig, run) = built();
                rig.sim
                    .set_pull(12, pull("OPEN", true, BRANCH, base, false));
                end_turn(&mut rig, 1, DONE_MESSAGE);
                let step = pass(&rig, &run);
                let shown = base.replace('@', "@\u{200b}");
                let detail = format!(
                    "the session reported PR #12 but its base {shown} is not the base branch main"
                );
                assert_stuck(
                    &rig,
                    &run,
                    &step,
                    "pr-not-bound",
                    "the session reported PR #12 but its base is not the base branch main",
                    Some(&detail),
                    Some(DONE_MESSAGE),
                );
            }
        }

        #[test]
        fn compares_the_base_with_the_configured_base_not_with_main() {
            let rig = Rig::new();
            let run = run_id(1);
            building_with(&rig, &run, launch(Some(BRANCH), None, None));
            let mut rig = rig;
            rig.config.base = "develop".into();
            rig.sim
                .set_pull(12, pull("OPEN", true, BRANCH, "develop", false));
            end_turn(&mut rig, 1, DONE_MESSAGE);
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
        }

        #[test]
        fn binds_to_the_base_the_runs_launch_recorded_not_the_projects() {
            let rig = Rig::new();
            let run = run_id(1);
            building_with(&rig, &run, launch(Some(BRANCH), None, Some("feature/x")));
            let mut rig = rig;
            rig.sim
                .set_pull(12, pull("OPEN", true, BRANCH, "feature/x", false));
            end_turn(&mut rig, 1, DONE_MESSAGE);
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));

            let other = Rig::new();
            let run = run_id(1);
            building_with(&other, &run, launch(Some(BRANCH), None, Some("feature/x")));
            let mut other = other;
            other
                .sim
                .set_pull(12, pull("OPEN", true, BRANCH, "main", false));
            end_turn(&mut other, 1, DONE_MESSAGE);
            assert_eq!(
                pass(&other, &run),
                BuildStep::Stuck {
                    reason: joined(
                        "the session reported PR #12 but its base main is not the base branch feature/x",
                        DONE_MESSAGE
                    )
                }
            );
            let comment = other
                .sim
                .comments()
                .into_iter()
                .find(|comment| comment.contains("could not finish"))
                .unwrap();
            assert!(comment.contains(
                "Reason: the session reported PR #12 but its base is not the base branch feature/x\n"
            ));
        }

        #[test]
        fn reports_a_fork_before_a_wrong_base_or_branch() {
            let (mut rig, run) = built();
            rig.sim
                .set_pull(12, pull("OPEN", true, "other", "release", true));
            end_turn(&mut rig, 1, DONE_MESSAGE);
            let step = pass(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "pr-not-bound",
                "the session reported PR #12 but it comes from a fork",
                None,
                Some(DONE_MESSAGE),
            );
        }

        #[test]
        fn when_the_session_has_no_branch_fails_closed() {
            let rig = Rig::new();
            let run = run_id(1);
            building_with(&rig, &run, launch(None, None, Some("main")));
            let mut rig = rig;
            rig.sim.set_pull(12, draft_on(BRANCH));
            end_turn(&mut rig, 1, DONE_MESSAGE);
            let step = pass(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "pr-not-bound",
                "the session reported PR #12 but the session has no branch",
                None,
                Some(DONE_MESSAGE),
            );
        }

        #[test]
        fn is_accepted_when_open_a_draft_and_on_the_sessions_branch() {
            let rig = Rig::new();
            let run = run_id(1);
            building_with(&rig, &run, launch(Some("fac/xyz"), None, Some("main")));
            let mut rig = rig;
            rig.sim.set_pull(12, draft_on("fac/xyz"));
            end_turn(&mut rig, 1, DONE_MESSAGE);
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
        }

        #[test]
        fn with_an_unusable_pull_request_url_is_stuck_without_asking_github() {
            let cases = [
                ("https://github.com/rawphp/other/pull/12", NOT_ON_REPO),
                (
                    "https://github.com/rawphp/softfac/pull/12?x=1",
                    MALFORMED_URL,
                ),
                (
                    "https://github.com/rawphp/softfac/pull/12 and more",
                    MALFORMED_URL,
                ),
                ("https://gitlab.com/rawphp/softfac/pull/12", MALFORMED_URL),
                ("https://github.com/rawphp/softfac/pull/0", MALFORMED_URL),
                (
                    "https://github.com/rawphp/softfac/pull/twelve",
                    MALFORMED_URL,
                ),
            ];
            for (url, reason) in cases {
                let (mut rig, run) = built();
                let message = format!("Done.\nFACTORY-DONE: {url}");
                end_turn(&mut rig, 1, &message);
                let step = pass(&rig, &run);
                assert_stuck(
                    &rig,
                    &run,
                    &step,
                    "session-stuck",
                    reason,
                    None,
                    Some(&message),
                );
                assert!(
                    !rig.sim
                        .calls()
                        .iter()
                        .any(|call| call.first().map(String::as_str) == Some("pr")),
                    "{url}"
                );
            }
        }

        #[test]
        fn is_a_runner_error_that_writes_nothing_when_the_pull_request_cannot_be_read() {
            let (mut rig, run) = built();
            rig.sim.fail("pr view", 1);
            end_turn(&mut rig, 1, DONE_MESSAGE);
            let before = rig.events(&run).len();
            let error = advance(&rig.ctx(), &run).unwrap_err();
            assert!(matches!(error, RunError::Tracker(_)), "{error}");
            assert_eq!(rig.events(&run).len(), before);
            // The next pass reads the turn again and goes on.
            say(&rig, DONE_MESSAGE);
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
        }
    }

    // ----- the runner's `advance: a resumed run's pull request` -----

    mod advance_a_resumed_runs_pull_request {
        use super::*;

        fn resumed() -> (Rig, RunId) {
            let rig = Rig::new();
            let run = run_id(1);
            building_with(
                &rig,
                &run,
                launch(Some(BRANCH), Some("fac/7-1"), Some("main")),
            );
            (rig, run)
        }

        #[test]
        fn is_accepted_from_the_branch_the_run_resumed_from() {
            let (mut rig, run) = resumed();
            rig.sim.set_pull(12, draft_on("fac/7-1"));
            let before = rig.events(&run).len();
            end_turn(&mut rig, 1, DONE_MESSAGE);
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
            assert_eq!(
                names_after(&rig, &run, before),
                ["pr_opened", "step_changed"]
            );
        }

        #[test]
        fn is_still_accepted_from_the_runs_own_branch() {
            let (mut rig, run) = resumed();
            rig.sim.set_pull(12, draft_on(BRANCH));
            end_turn(&mut rig, 1, DONE_MESSAGE);
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
        }

        #[test]
        fn is_refused_from_a_third_branch() {
            let (mut rig, run) = resumed();
            rig.sim.set_pull(12, draft_on("someone/else"));
            end_turn(&mut rig, 1, DONE_MESSAGE);
            let BuildStep::Stuck { reason } = pass(&rig, &run) else {
                panic!("expected a stuck run");
            };
            assert!(
                reason.starts_with(
                    "the session reported PR #12 but its branch someone/else is not this session's branch"
                ),
                "{reason}"
            );
        }
    }

    // ----- the runner's `advance: questions` -----

    /// What the run's calls to the worker are once it ended `needs-info`: the session read,
    /// the workspace's sessions ended, the workspace archived by force.
    fn needs_info_calls() -> Vec<Call> {
        vec![
            Call::Turn {
                session: session(),
                n: 1,
            },
            ended_workspace(),
            archived(),
        ]
    }

    fn questions_comments(rig: &Rig) -> usize {
        rig.sim
            .comments()
            .iter()
            .filter(|comment| comment.contains("The factory needs answers"))
            .count()
    }

    mod advance_questions {
        use super::*;

        #[test]
        fn posts_them_releases_the_issue_and_ends_the_run() {
            let (mut rig, run) = built();
            let before = rig.events(&run).len();
            end_turn(
                &mut rig,
                1,
                &format!("{QUESTIONS}\nFACTORY-STOP: needs-info"),
            );
            assert_eq!(pass(&rig, &run), BuildStep::NeedsInfo);
            let comments = rig.sim.comments();
            assert!(
                comments[0].starts_with(&messages::questions(&messages::prepare_session_text(
                    QUESTIONS
                ))),
                "{}",
                comments[0]
            );
            assert_eq!(rig.sim.labels(), ["factory:needs-info"]);
            assert!(rig.sim.assignees().is_empty());
            // The session is read, then ended, then its workspace archived by force.
            assert_eq!(calls(&rig), needs_info_calls());
            assert_eq!(
                names_after(&rig, &run, before),
                [
                    "step_changed",
                    "questions_posted",
                    "state_changed",
                    "finished"
                ]
            );
            let finished = last_event(&rig, &run);
            assert_eq!(finished.data["outcome"], "needs-info");
            assert_eq!(
                cause_of_last(&rig, &run),
                ("advance".into(), "questions-asked".into())
            );
        }

        #[test]
        fn with_no_questions_is_stuck() {
            let (mut rig, run) = built();
            end_turn(&mut rig, 1, "FACTORY-STOP: needs-info");
            let step = pass(&rig, &run);
            assert_stuck(&rig, &run, &step, "session-stuck", NO_QUESTIONS, None, None);
        }

        #[test]
        fn are_not_asked_by_a_run_that_ended_badly() {
            for state in [SessionState::Failed, SessionState::Finished] {
                let (mut rig, run) = built();
                show(&mut rig, state, 1, 1);
                say(&rig, "Why?\nFACTORY-STOP: needs-info");
                let before = rig.events(&run).len();
                // `resume` decides about a session that ended: nothing is written.
                assert_eq!(pass(&rig, &run), BuildStep::Interrupted, "{state:?}");
                no_writes(&rig);
                assert_eq!(rig.events(&run).len(), before, "{state:?}");
                assert!(sends(&rig).is_empty());
            }
        }

        #[test]
        fn fail_with_the_comment_as_a_runner_error_that_a_retry_can_finish() {
            let (mut rig, run) = built();
            let message = "Which one?\nFACTORY-STOP: needs-info";
            end_turn(&mut rig, 1, message);
            rig.sim.fail("issue comment", 1);
            let before = rig.events(&run).len();
            assert!(matches!(
                advance(&rig.ctx(), &run).unwrap_err(),
                RunError::Tracker(_)
            ));
            // Only the intent to comment was written: no questions, no end.
            assert_eq!(names_after(&rig, &run, before), ["step_changed"]);
            assert_eq!(questions_comments(&rig), 0);
            say(&rig, message);
            assert_eq!(pass(&rig, &run), BuildStep::NeedsInfo);
            assert_eq!(questions_comments(&rig), 1);
        }

        #[test]
        fn already_posted_are_not_posted_again_by_the_next_call() {
            let (mut rig, run) = built();
            let message = "Which one?\nFACTORY-STOP: needs-info";
            end_turn(&mut rig, 1, message);
            // The label call fails after the comment was posted and recorded.
            rig.sim.fail("--add-label", 1);
            assert!(matches!(
                advance(&rig.ctx(), &run).unwrap_err(),
                RunError::Tracker(_)
            ));
            assert_eq!(questions_comments(&rig), 1);
            say(&rig, message);
            assert_eq!(pass(&rig, &run), BuildStep::NeedsInfo);
            assert_eq!(questions_comments(&rig), 1);
        }
    }

    // ----- the runner's `advance: a run left in needs-info` -----

    mod advance_a_run_left_in_needs_info {
        use super::*;

        #[test]
        fn is_ended_without_posting_the_questions_again() {
            let (rig, run) = waiting_in_needs_info();
            let before = rig.events(&run).len();
            assert_eq!(pass(&rig, &run), BuildStep::NeedsInfo);
            // Nothing is written on GitHub: the label stays `factory:needs-info`.
            no_writes(&rig);
            assert_eq!(issue_reads(&rig), 1);
            assert_eq!(calls(&rig), vec![ended_workspace(), archived()]);
            assert_eq!(names_after(&rig, &run, before), ["finished"]);
            assert_eq!(last_event(&rig, &run).data["outcome"], "needs-info");
            assert_eq!(
                cause_of_last(&rig, &run),
                ("advance".into(), "questions-asked".into())
            );
        }

        #[test]
        fn is_not_carried_out_as_stuck_by_a_decision_an_older_version_recorded() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.write(
                &run,
                Event::QuestionsPosted {
                    questions: "Which one?".into(),
                },
            );
            rig.write(
                &run,
                Event::Stuck {
                    reason: "the questions went unanswered".into(),
                    branch: None,
                    commits_ahead: None,
                },
            );
            rig.moved(&run, State::Building, State::NeedsInfo);
            rig.sim.set_labels(&["factory:needs-info"]);
            rig.sim.set_assignees(&[]);
            assert_eq!(pass(&rig, &run), BuildStep::NeedsInfo);
            no_writes(&rig);
            assert_eq!(issue_reads(&rig), 1);
            assert_eq!(last_event(&rig, &run).data["outcome"], "needs-info");
        }

        #[test]
        fn whose_issue_a_person_changed_is_ended_and_the_issue_left_alone() {
            let cases: [(&[&str], &[&str]); 6] = [
                (&["factory:building"], &["someone"]),
                (&["factory:building"], &[LOGIN, "someone"]),
                (&["factory:building"], &[]),
                (&["factory:ready"], &[]),
                (&["factory:stuck"], &[LOGIN]),
                (&[], &[LOGIN]),
            ];
            for (labels, assignees) in cases {
                let (rig, run) = waiting_in_needs_info();
                rig.sim.set_labels(labels);
                rig.sim.set_assignees(assignees);
                assert_eq!(pass(&rig, &run), BuildStep::NeedsInfo, "{labels:?}");
                no_writes(&rig);
                assert_eq!(issue_reads(&rig), 1, "{labels:?} {assignees:?}");
                assert_eq!(last_event(&rig, &run).data["outcome"], "needs-info");
            }
        }

        #[test]
        fn that_an_older_version_claimed_back_before_its_marker_is_given_the_issue_back() {
            let (rig, run) = waiting_in_needs_info();
            rig.sim.set_labels(&["factory:building"]);
            rig.sim.set_assignees(&["SoftFac-Bot"]);
            assert_eq!(pass(&rig, &run), BuildStep::NeedsInfo);
            assert_eq!(rig.sim.labels(), ["factory:needs-info"]);
            // The login is matched without regard to case; the simulator's own assignee list is
            // exact, so what is checked is the call, which names the project's login.
            assert!(
                rig.sim
                    .writes()
                    .iter()
                    .any(|write| write.contains(&format!("--remove-assignee {LOGIN}"))),
                "{:?}",
                rig.sim.writes()
            );
            assert_eq!(calls(&rig), vec![ended_workspace(), archived()]);
            assert_eq!(last_event(&rig, &run).data["outcome"], "needs-info");
        }

        #[test]
        fn that_is_ended_is_not_ended_again() {
            let (rig, run) = waiting_in_needs_info();
            pass(&rig, &run);
            let events = rig.events(&run).len();
            let worker = calls(&rig).len();
            let github = rig.sim.calls().len();
            assert!(matches!(pass(&rig, &run), BuildStep::Skipped { .. }));
            assert_eq!(calls(&rig).len(), worker);
            assert_eq!(rig.sim.calls().len(), github);
            assert_eq!(rig.events(&run).len(), events);
        }

        #[test]
        fn stays_open_for_the_next_call_when_the_archive_fails() {
            let (rig, run) = waiting_in_needs_info();
            rig.worker
                .on_archive(Err(WorkerError::Failed("the machine said no".into())));
            assert!(matches!(
                advance(&rig.ctx(), &run).unwrap_err(),
                RunError::Worker(_)
            ));
            assert!(!rig.finished(&run));
            assert_eq!(pass(&rig, &run), BuildStep::NeedsInfo);
            assert!(rig.finished(&run));
        }
    }

    // ----- the runner's `advance: stuck` -----

    mod advance_stuck {
        use super::*;

        #[test]
        fn carries_out_a_decision_already_in_the_ledger_without_asking_the_worker_or_posting_again()
        {
            let (rig, run) = built();
            rig.write(
                &run,
                Event::Stuck {
                    reason: "gave up".into(),
                    branch: None,
                    commits_ahead: None,
                },
            );
            let before = rig.events(&run).len();
            assert_eq!(
                pass(&rig, &run),
                BuildStep::Stuck {
                    reason: "gave up".into()
                }
            );
            assert_eq!(rig.sim.labels(), ["factory:stuck"]);
            assert!(rig.sim.assignees().is_empty());
            // The session is ended, never read, and nothing is posted again.
            assert_eq!(calls(&rig), vec![ended_workspace()]);
            assert!(
                rig.sim
                    .comments()
                    .iter()
                    .all(|c| !c.contains("could not finish"))
            );
            assert_eq!(
                names_after(&rig, &run, before),
                ["state_changed", "finished"]
            );
            assert_eq!(
                cause_of_last(&rig, &run),
                ("advance".into(), "stuck-resumed".into())
            );
        }

        #[test]
        fn only_finishes_a_run_whose_label_a_person_changed_before_it_was_finished() {
            let (rig, run) = built();
            let reason = format!("{NO_RESULT_LINE}\n\nno marker");
            rig.write(
                &run,
                Event::Stuck {
                    reason: reason.clone(),
                    branch: None,
                    commits_ahead: None,
                },
            );
            rig.moved(&run, State::Building, State::Stuck);
            rig.sim.set_labels(&["factory:ready"]);
            rig.sim.set_assignees(&[]);
            let before = rig.events(&run).len();
            assert_eq!(pass(&rig, &run), BuildStep::Stuck { reason });
            no_writes(&rig);
            assert_eq!(rig.sim.labels(), ["factory:ready"]);
            assert_eq!(calls(&rig), vec![]);
            assert_eq!(names_after(&rig, &run, before), ["finished"]);
            assert_eq!(last_event(&rig, &run).data["outcome"], "stuck");
        }

        #[test]
        fn puts_the_session_text_in_the_code_block_not_in_the_reason_line() {
            let (mut rig, run) = built();
            let text = "The gate cannot run here.\nIt needs a database.";
            end_turn(&mut rig, 1, &format!("{text}\nFACTORY-STOP: stuck"));
            let step = pass(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "session-stuck",
                SAID_STUCK,
                None,
                Some(text),
            );
        }

        #[test]
        fn prepares_the_session_text_once_for_the_comment_the_ledger_and_the_outcome() {
            let (mut rig, run) = built();
            let message = format!(
                "ping @octocat <!-- softfac -->\u{202e}\n```\n{}\nFACTORY-STOP: stuck",
                "x".repeat(6100)
            );
            end_turn(&mut rig, 1, &message);
            let BuildStep::Stuck { reason } = pass(&rig, &run) else {
                panic!("expected a stuck run");
            };
            // The mention and the marker are disarmed, the bidi mark is a space, the text is cut.
            assert!(
                reason.starts_with(&format!(
                    "{SAID_STUCK}\n\nping @\u{200b}octocat <\u{200b}!-- softfac --> \n"
                )),
                "{reason}"
            );
            assert!(reason.ends_with("x\n[cut]"));
            let comment = rig
                .sim
                .comments()
                .into_iter()
                .find(|comment| comment.contains("could not finish"))
                .unwrap();
            assert!(
                comment.contains("\n\n````text\nping @\u{200b}octocat"),
                "{comment}"
            );
            assert!(
                comment.contains("x\n[cut]\n````\n\nFix the cause"),
                "{comment}"
            );
            let stuck = rig
                .events(&run)
                .into_iter()
                .find(|e| e.event == "stuck")
                .unwrap();
            assert_eq!(stuck.data["reason"], reason);
        }

        #[test]
        fn without_a_reason() {
            let (mut rig, run) = built();
            end_turn(&mut rig, 1, "FACTORY-STOP: stuck");
            let step = pass(&rig, &run);
            assert_stuck(&rig, &run, &step, "session-stuck", SAID_STUCK, None, None);
        }

        #[test]
        fn ends_as_session_wrong_account_when_the_first_line_names_another_gh_account() {
            let (mut rig, run) = built();
            let text = "the session's gh account is tomkaczocha, not rawphp\nIt cannot see rawphp/softfac.";
            end_turn(&mut rig, 1, &format!("{text}\nFACTORY-STOP: stuck"));
            let step = pass(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "session-wrong-account",
                WRONG_ACCOUNT,
                None,
                Some(text),
            );
        }

        #[test]
        fn counts_the_wrong_account_line_after_blank_lines_and_alone() {
            for message in [
                "\n  \nthe session's gh account is x, not rawphp\nmore\nFACTORY-STOP: stuck",
                "the session's gh account is x, not rawphp\nFACTORY-STOP: stuck",
            ] {
                let (mut rig, run) = built();
                end_turn(&mut rig, 1, message);
                let BuildStep::Stuck { reason } = pass(&rig, &run) else {
                    panic!("expected a stuck run: {message}");
                };
                assert!(reason.starts_with(WRONG_ACCOUNT), "{message}");
                assert_eq!(
                    last_event(&rig, &run).cause.unwrap()["reason"],
                    "session-wrong-account"
                );
            }
        }

        #[test]
        fn stays_session_stuck_when_the_wrong_account_words_are_anywhere_but_the_first_line() {
            for message in [
                "The gate cannot run.\nthe session's gh account is x, not rawphp\nFACTORY-STOP: stuck",
                "I think the session's gh account is x, not rawphp\nFACTORY-STOP: stuck",
                "FACTORY-STOP: stuck",
            ] {
                let (mut rig, run) = built();
                end_turn(&mut rig, 1, message);
                let BuildStep::Stuck { reason } = pass(&rig, &run) else {
                    panic!("expected a stuck run: {message}");
                };
                assert!(reason.starts_with(SAID_STUCK), "{message}");
                assert_eq!(
                    last_event(&rig, &run).cause.unwrap()["reason"],
                    "session-stuck"
                );
            }
        }

        #[test]
        fn counts_the_wrong_account_words_only_with_the_stuck_line() {
            let (mut rig, run) = built();
            end_turn(
                &mut rig,
                1,
                "the session's gh account is x, not rawphp\nFACTORY-DONE: not a url",
            );
            let BuildStep::Stuck { reason } = pass(&rig, &run) else {
                panic!("expected a stuck run");
            };
            assert!(reason.starts_with(MALFORMED_URL), "{reason}");
            assert_eq!(
                last_event(&rig, &run).cause.unwrap()["reason"],
                "session-stuck"
            );
        }

        #[test]
        fn when_the_message_has_no_usable_marker_after_the_one_reminder() {
            for message in [
                "I changed some files.",
                "FACTORY-STOP: stuck\nThanks for waiting.",
                "FACTORY-STOP: stuck, sorry",
                "FACTORY-STOP: needs-info\n1. Why?",
                &format!("FACTORY-DONE: {URL12}\nbye"),
            ] {
                // The first ended turn got the reminder, the second goes stuck.
                let (rig, run) = reminded_run(message);
                let step = pass(&rig, &run);
                assert_stuck(
                    &rig,
                    &run,
                    &step,
                    "session-stuck",
                    NO_RESULT_LINE,
                    None,
                    Some(message),
                );
                assert!(steps_after(&rig, &run, 0).contains(&"result-line-reminded".to_owned()));
            }
        }

        #[test]
        fn when_there_is_no_final_message_after_the_one_reminder() {
            for message in ["", "  \n"] {
                let (rig, run) = reminded_run(message);
                let step = pass(&rig, &run);
                assert_stuck(&rig, &run, &step, "session-stuck", NO_MESSAGE, None, None);
            }
        }

        /// The runner's run statuses: a run that ended on its own with no result line is
        /// reminded, and one that ended badly is the pass's to decide. Foundry's states: `idle`,
        /// and `finished` and `failed` (the session's process ended: `resume` decides).
        #[test]
        fn counts_only_the_known_terminal_statuses_as_ended() {
            for state in [
                SessionState::Idle,
                SessionState::Finished,
                SessionState::Failed,
                SessionState::Working,
            ] {
                let (mut rig, run) = built();
                show(&mut rig, state, 1, 1);
                say(&rig, "no marker");
                let step = pass(&rig, &run);
                match state {
                    SessionState::Idle => assert_eq!(step, BuildStep::Reminded),
                    SessionState::Working => assert_eq!(step, BuildStep::Running),
                    _ => assert_eq!(step, BuildStep::Interrupted, "{state:?}"),
                }
            }
        }

        #[test]
        fn when_the_run_ended_badly_with_its_status_and_no_reminder() {
            for state in [SessionState::Failed, SessionState::Finished] {
                let (mut rig, run) = built();
                show(&mut rig, state, 1, 1);
                say(&rig, "it crashed");
                let before = rig.events(&run).len();
                assert_eq!(pass(&rig, &run), BuildStep::Interrupted);
                no_writes(&rig);
                assert!(sends(&rig).is_empty());
                assert_eq!(rig.events(&run).len(), before);
            }
        }

        /// The runner's thread that T3 no longer has: a session the fleet does not show.
        #[test]
        fn when_the_thread_no_longer_exists() {
            let (mut rig, run) = built();
            rig.view = FleetView::builder().online(rig.machine).build();
            let before = rig.events(&run).len();
            assert_eq!(pass(&rig, &run), BuildStep::Interrupted);
            no_writes(&rig);
            assert_eq!(calls(&rig), vec![]);
            assert_eq!(rig.events(&run).len(), before);
        }

        #[test]
        fn is_never_decided_by_a_transient_t3_error_which_writes_nothing() {
            for (state, question) in [
                (SessionState::Idle, false),
                (SessionState::Finished, false),
                (SessionState::WaitingForYou, true),
            ] {
                let (mut rig, run) = built();
                show_with(&mut rig, state, 1, 1, question);
                rig.worker
                    .on_turn(Err(WorkerError::Unreachable("the machine is away".into())));
                let before = rig.events(&run).len();
                let error = advance(&rig.ctx(), &run).unwrap_err();
                assert!(matches!(error, RunError::Worker(_)), "{state:?}: {error}");
                no_writes(&rig);
                assert_eq!(rig.events(&run).len(), before, "{state:?}");
                assert!(sends(&rig).is_empty(), "{state:?}");
                if state == SessionState::Idle {
                    say(&rig, DONE_MESSAGE);
                    assert!(matches!(
                        pass(&rig, &run),
                        BuildStep::ReadyForGate { pr: 12, .. }
                    ));
                }
            }
        }
    }

    // ----- the runner's `advance: the result-line reminder` -----

    mod advance_the_result_line_reminder {
        use super::*;

        #[test]
        fn is_sent_once_as_one_fixed_message_to_a_run_that_ended_with_no_result_line() {
            let (mut rig, run) = built();
            let before = rig.events(&run).len();
            end_turn(&mut rig, 1, WORKING_MESSAGE);
            assert_eq!(pass(&rig, &run), BuildStep::Reminded);
            assert_eq!(sends(&rig), vec![reminder_sent(1)]);
            // Nothing on GitHub: the issue is still building and still ours.
            no_writes(&rig);
            assert_eq!(
                steps_after(&rig, &run, before),
                [
                    "result-line-reminding 1",
                    "turn-read 1",
                    "result-line-reminded"
                ]
            );
            let reasons: Vec<String> = rig
                .events(&run)
                .iter()
                .skip(before)
                .map(|e| {
                    e.cause.as_ref().unwrap()["reason"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect();
            assert_eq!(
                reasons,
                ["result-line-reminding", "turn-read", "result-line-reminded"]
            );
        }

        #[test]
        fn writes_the_wait_marker_before_the_send_and_the_step_after_it() {
            let (mut rig, run) = built();
            let before = rig.events(&run).len();
            end_turn(&mut rig, 1, WORKING_MESSAGE);
            rig.worker.on_send(Err(SendRefused::Failed {
                reason: "the terminal did not answer".into(),
                prompts: Some(1),
            }));
            assert!(matches!(
                advance(&rig.ctx(), &run).unwrap_err(),
                RunError::Send(SendRefused::Failed { .. })
            ));
            assert_eq!(steps_after(&rig, &run, before), ["result-line-reminding 1"]);
        }

        #[test]
        fn is_not_sent_again_while_the_session_works_on_it() {
            let (mut rig, run) = built();
            end_turn(&mut rig, 1, WORKING_MESSAGE);
            assert_eq!(pass(&rig, &run), BuildStep::Reminded);
            let events = rig.events(&run).len();
            // The fleet has not shown the session working yet: its turn is still the one read.
            assert_eq!(pass(&rig, &run), BuildStep::Running);
            show(&mut rig, SessionState::Working, 1, 2);
            assert_eq!(pass(&rig, &run), BuildStep::Running);
            assert_eq!(sends(&rig), vec![reminder_sent(1)]);
            assert_eq!(rig.events(&run).len(), events);
        }

        #[test]
        fn is_not_sent_for_a_run_that_is_still_active() {
            for state in [
                SessionState::Starting,
                SessionState::Working,
                SessionState::Unreachable,
            ] {
                let (mut rig, run) = built();
                show(&mut rig, state, 1, 1);
                say(&rig, WORKING_MESSAGE);
                let before = rig.events(&run).len();
                assert_eq!(pass(&rig, &run), BuildStep::Running, "{state:?}");
                assert!(sends(&rig).is_empty(), "{state:?}");
                assert_eq!(rig.events(&run).len(), before, "{state:?}");
            }
        }

        #[test]
        fn is_followed_by_a_newer_run_that_ends_with_done_which_goes_to_the_gate() {
            let (rig, run) = reminded_run(DONE_MESSAGE);
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
            assert!(sends(&rig).is_empty());
            assert_eq!(rig.sim.labels(), ["factory:building"]);
            assert_eq!(rig.names(&run).last().unwrap(), "step_changed");
            assert!(!calls(&rig).contains(&archived()));
        }

        #[test]
        fn is_followed_by_a_newer_run_that_ends_with_questions_which_posts_them() {
            let (rig, run) = reminded_run(&format!("{QUESTIONS}\nFACTORY-STOP: needs-info"));
            assert_eq!(pass(&rig, &run), BuildStep::NeedsInfo);
            assert_eq!(rig.sim.labels(), ["factory:needs-info"]);
            assert_eq!(questions_comments(&rig), 1);
            assert!(sends(&rig).is_empty());
        }

        #[test]
        fn is_followed_by_a_second_run_with_no_result_line_which_goes_stuck_with_no_second_reminder()
         {
            let (rig, run) = reminded_run("Still working.");
            let step = pass(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "session-stuck",
                NO_RESULT_LINE,
                None,
                Some("Still working."),
            );
            assert!(sends(&rig).is_empty());
        }

        #[test]
        fn waits_for_the_second_run_in_the_exact_live_sequence_and_releases_nothing() {
            let (mut rig, run) = built();
            end_turn(&mut rig, 1, WORKING_MESSAGE);
            assert_eq!(pass(&rig, &run), BuildStep::Reminded);
            // The session wakes up and finishes: working, then idle with the result line.
            show(&mut rig, SessionState::Working, 1, 2);
            assert_eq!(pass(&rig, &run), BuildStep::Running);
            show(&mut rig, SessionState::Idle, 2, 2);
            say(&rig, &format!("Opened it.\nFACTORY-DONE: {URL12}"));
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
            assert_eq!(rig.sim.labels(), ["factory:building"]);
            assert_eq!(rig.sim.assignees(), [LOGIN]);
            assert!(rig.sim.comments().is_empty());
            assert!(!calls(&rig).contains(&archived()));
            assert!(!rig.names(&run).contains(&"stuck".to_owned()));
        }

        #[test]
        fn treats_a_missing_message_like_one_with_no_result_line() {
            for message in ["", "   \n"] {
                let (mut rig, run) = built();
                end_turn(&mut rig, 1, message);
                assert_eq!(pass(&rig, &run), BuildStep::Reminded);
                assert_eq!(sends(&rig), vec![reminder_sent(1)]);
            }
        }

        #[test]
        fn is_not_sent_for_a_result_line_that_is_present_but_unusable() {
            for message in [
                "FACTORY-DONE: nonsense",
                "FACTORY-DONE: https://github.com/someone/else/pull/3",
                "FACTORY-STOP: needs-info",
            ] {
                let (mut rig, run) = built();
                end_turn(&mut rig, 1, message);
                assert!(
                    matches!(pass(&rig, &run), BuildStep::Stuck { .. }),
                    "{message}"
                );
                assert!(sends(&rig).is_empty(), "{message}");
            }
        }

        #[test]
        fn is_not_sent_for_a_run_that_ended_badly() {
            for state in [SessionState::Failed, SessionState::Finished] {
                let (mut rig, run) = built();
                show(&mut rig, state, 1, 1);
                say(&rig, WORKING_MESSAGE);
                assert_eq!(pass(&rig, &run), BuildStep::Interrupted, "{state:?}");
                assert!(sends(&rig).is_empty(), "{state:?}");
            }
        }

        #[test]
        fn cut_short_before_its_send_is_sent_on_the_next_call_under_one_reminding_step() {
            let (mut rig, run) = built();
            rig.step(&run, "result-line-reminding 1");
            let before = rig.events(&run).len();
            end_turn(&mut rig, 1, WORKING_MESSAGE);
            assert_eq!(pass(&rig, &run), BuildStep::Reminded);
            assert_eq!(sends(&rig), vec![reminder_sent(1)]);
            assert_eq!(
                steps_after(&rig, &run, before),
                ["turn-read 1", "result-line-reminded"]
            );
        }

        #[test]
        fn whose_step_is_missing_is_not_sent_again_when_the_session_answered_it() {
            let (mut rig, run) = built();
            rig.step(&run, "result-line-reminding 1");
            rig.step(&run, "turn-read 1");
            let before = rig.events(&run).len();
            end_turn(&mut rig, 2, DONE_MESSAGE);
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
            assert!(sends(&rig).is_empty());
            // The send is taken to have arrived, and the step is made good.
            assert_eq!(
                steps_after(&rig, &run, before),
                ["result-line-reminded", "turn-read 2"]
            );
            assert_eq!(rig.names(&run)[before + 1], "pr_opened");
        }

        #[test]
        fn sent_again_after_a_crash_between_the_send_and_its_step_uses_the_same_count() {
            for delivered in [false, true] {
                let (mut rig, run) = built();
                let before = rig.events(&run).len();
                end_turn(&mut rig, 1, WORKING_MESSAGE);
                rig.worker.on_send(Err(SendRefused::Failed {
                    reason: "no answer".into(),
                    prompts: Some(1),
                }));
                assert!(advance(&rig.ctx(), &run).is_err());
                // The second try: the message went in, or it did not.
                rig.worker.on_send(if delivered {
                    Err(SendRefused::CountRose { now: 2 })
                } else {
                    Ok(1)
                });
                say(&rig, WORKING_MESSAGE);
                assert_eq!(
                    pass(&rig, &run),
                    BuildStep::Reminded,
                    "delivered {delivered}"
                );
                assert_eq!(
                    sends(&rig),
                    vec![reminder_sent(1), reminder_sent(1)],
                    "delivered {delivered}"
                );
                assert_eq!(
                    steps_after(&rig, &run, before),
                    [
                        "result-line-reminding 1",
                        "turn-read 1",
                        "result-line-reminded"
                    ],
                    "delivered {delivered}"
                );
            }
        }

        #[test]
        fn with_a_lost_step_and_a_newer_run_that_has_no_line_goes_stuck_and_does_not_hang() {
            let (mut rig, run) = built();
            rig.step(&run, "result-line-reminding 1");
            rig.step(&run, "turn-read 1");
            end_turn(&mut rig, 2, "Nothing yet.");
            assert!(matches!(pass(&rig, &run), BuildStep::Stuck { .. }));
            assert!(sends(&rig).is_empty());
        }

        /// What a reminder the session refused records and says: the run's own reason, with the
        /// refusal in the ledger only. A refusal is final: it is refused again.
        #[track_caller]
        fn assert_refused_reminder_is_stuck(rig: &Rig, run: &RunId, step: &BuildStep) {
            let error = SendRefused::BodyWithoutPath.to_string();
            let detail = format!("{NO_RESULT_LINE}; Foundry refused the reminder: {error}");
            assert_stuck(
                rig,
                run,
                step,
                "session-stuck",
                NO_RESULT_LINE,
                Some(&detail),
                Some(WORKING_MESSAGE),
            );
            assert!(
                rig.steps(run)
                    .contains(&"result-line-reminding 1".to_owned())
            );
            assert!(!rig.steps(run).contains(&"result-line-reminded".to_owned()));
            // The run is finished: another pass sends nothing.
            let sent = sends(rig).len();
            assert!(matches!(pass(rig, run), BuildStep::Skipped { .. }));
            assert_eq!(sends(rig).len(), sent);
        }

        #[test]
        fn goes_stuck_at_once_with_the_marker_written_when_the_worker_refuses_the_message() {
            let (mut rig, run) = built();
            end_turn(&mut rig, 1, WORKING_MESSAGE);
            rig.worker.on_send(Err(SendRefused::BodyWithoutPath));
            let step = pass(&rig, &run);
            assert_refused_reminder_is_stuck(&rig, &run, &step);
        }

        #[test]
        fn goes_stuck_at_once_when_the_worker_refuses_the_message_of_a_reminder_cut_short() {
            let (mut rig, run) = built();
            rig.step(&run, "result-line-reminding 1");
            end_turn(&mut rig, 1, WORKING_MESSAGE);
            rig.worker.on_send(Err(SendRefused::BodyWithoutPath));
            let step = pass(&rig, &run);
            assert_refused_reminder_is_stuck(&rig, &run, &step);
        }

        /// The runner's `thread-gone`: the session is gone at the send.
        #[test]
        fn goes_stuck_as_thread_gone_when_the_worker_no_longer_has_the_session_at_the_send() {
            let (mut rig, run) = built();
            end_turn(&mut rig, 1, WORKING_MESSAGE);
            rig.worker.on_send(Err(SendRefused::NoSuchSession));
            let step = pass(&rig, &run);
            assert_stuck(&rig, &run, &step, "session-gone", SESSION_GONE, None, None);
        }

        /// A send is one at a time per session (`send_once`'s rule): a forward whose try may have
        /// typed holds every other send, the reminder's included, and the pass writes nothing.
        #[test]
        fn is_not_sent_while_another_send_to_the_session_may_have_typed() {
            for begun in [false, true] {
                let (mut rig, run) = built();
                rig.step(&run, "send-intent forward-comment 77 1");
                if begun {
                    rig.step(&run, "result-line-reminding 1");
                }
                end_turn(&mut rig, 1, WORKING_MESSAGE);
                let before = rig.events(&run).len();
                assert_eq!(pass(&rig, &run), BuildStep::Running, "begun {begun}");
                assert!(sends(&rig).is_empty(), "begun {begun}");
                assert_eq!(rig.events(&run).len(), before, "begun {begun}");
                no_writes(&rig);
            }
        }

        /// The turn is recorded read before the reminder is recorded sent: a pass cut between
        /// the two reads the turn as read (it is not reminded twice), and a newer turn is the
        /// answer to the reminder, which the step then records. Cut the other way (reminded
        /// first) the same turn is read again as ended with no line and goes stuck at once,
        /// with the session never given the time to answer.
        #[test]
        fn cut_between_its_turn_read_and_its_reminded_step_neither_hangs_nor_sticks_at_once() {
            let (mut rig, run) = built();
            rig.step(&run, "result-line-reminding 1");
            rig.step(&run, "turn-read 1");
            // The turn is read: its text is not asked for again.
            show(&mut rig, SessionState::Idle, 1, 1);
            let before = rig.events(&run).len();
            assert_eq!(pass(&rig, &run), BuildStep::Running);
            assert_eq!(turn_reads(&rig), 0);
            assert!(sends(&rig).is_empty());
            assert_eq!(rig.events(&run).len(), before);
            // The session answers with a result: no hang.
            end_turn(&mut rig, 2, DONE_MESSAGE);
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
            assert!(steps_after(&rig, &run, before).contains(&"result-line-reminded".to_owned()));
            // Or without one: stuck, once, with no second reminder.
            let (mut rig, run) = built();
            rig.step(&run, "result-line-reminding 1");
            rig.step(&run, "turn-read 1");
            end_turn(&mut rig, 2, "Still nothing.");
            let step = pass(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "session-stuck",
                NO_RESULT_LINE,
                None,
                Some("Still nothing."),
            );
            assert!(sends(&rig).is_empty());
        }

        /// The worker found the session not idle at the send, so nothing was typed. The step
        /// that says the reminder was begun is already written, and the pass says nothing more.
        #[test]
        fn that_finds_the_session_not_idle_is_sent_again_when_the_turn_is_still_the_same() {
            let (mut rig, run) = built();
            let before = rig.events(&run).len();
            end_turn(&mut rig, 1, WORKING_MESSAGE);
            rig.worker.on_send(Err(SendRefused::NotIdle));
            assert_eq!(pass(&rig, &run), BuildStep::Running);
            assert_eq!(steps_after(&rig, &run, before), ["result-line-reminding 1"]);
            no_writes(&rig);
            // The reminder is still owed: the next pass sends it under the same count.
            assert_eq!(pass(&rig, &run), BuildStep::Reminded);
            assert_eq!(sends(&rig), vec![reminder_sent(1), reminder_sent(1)]);
            assert_eq!(
                steps_after(&rig, &run, before),
                [
                    "result-line-reminding 1",
                    "turn-read 1",
                    "result-line-reminded"
                ]
            );
        }

        /// The runner's rule is kept: a newer turn answers a begun reminder, whichever message
        /// started it (a person typing into the session is as good as the reminder). A turn that
        /// ends with no line after a reminder that was refused as `NotIdle` is stuck with the
        /// run's own reason, which claims no reminder was typed, and nothing more is sent.
        #[test]
        fn that_finds_the_session_not_idle_and_a_newer_turn_with_no_line_is_stuck_with_the_runs_own_reason()
         {
            let (mut rig, run) = built();
            end_turn(&mut rig, 1, WORKING_MESSAGE);
            rig.worker.on_send(Err(SendRefused::NotIdle));
            assert_eq!(pass(&rig, &run), BuildStep::Running);
            end_turn(&mut rig, 2, "Still nothing.");
            let step = pass(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "session-stuck",
                NO_RESULT_LINE,
                None,
                Some("Still nothing."),
            );
            assert_eq!(sends(&rig), vec![reminder_sent(1)]);
        }
    }

    // ----- the runner's `timeLimit` -----

    thread_local! {
        /// The wall clock of the test's ledger: the cell that stamps its lines, which
        /// `time_limit_at` is given as "now". The context's own clock is left at its base.
        static WALL: std::cell::RefCell<Option<Arc<AtomicU64>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// The run's own clock: the ledger stamps each line with what the returned cell holds, so a
    /// test says when a line was written, in milliseconds after [`T0`]. [`clock_to`] moves the
    /// same cell, and [`limit`] reads "now" from it.
    fn timed() -> (Rig, Arc<AtomicU64>) {
        let mut rig = Rig::new();
        let (dir, store) = crate::factory::ledger::testing::open_store();
        let cell = Arc::new(AtomicU64::new(T0));
        let wall = cell.clone();
        rig.ledger = crate::factory::ledger::testing::ledger(
            &store,
            foundry_proto::ids::MachineId(uuid::Uuid::from_u128(0x1)),
            1,
            Arc::new(move || wall.load(Ordering::SeqCst)),
        );
        rig._dir = dir;
        WALL.with(|wall| *wall.borrow_mut() = Some(cell.clone()));
        (rig, cell)
    }

    /// The next lines are stamped `ms` after [`T0`].
    fn stamp(cell: &AtomicU64, ms: u64) {
        cell.store(T0 + ms, Ordering::SeqCst);
    }

    /// The wall clock reads `ms` after [`T0`]: the ledger's stamps and the "now" of [`limit`].
    /// It may step back. The context's sleep-counting clock is not touched.
    fn clock_to(_rig: &Rig, ms: u64) {
        WALL.with(|wall| {
            wall.borrow()
                .as_ref()
                .expect("a timed rig")
                .store(T0 + ms, Ordering::SeqCst);
        });
    }

    fn wall_now() -> u64 {
        WALL.with(|wall| {
            wall.borrow()
                .as_ref()
                .expect("a timed rig")
                .load(Ordering::SeqCst)
        })
    }

    fn try_limit(rig: &Rig, run: &RunId) -> Result<LimitStep, RunError> {
        time_limit_at(&rig.ctx(), run, wall_now())
    }

    /// A run building since [`T0`], with its pull request #12 an open draft and its session
    /// started.
    fn timed_built() -> (Rig, RunId, Arc<AtomicU64>) {
        let (rig, cell) = timed();
        let run = run_id(1);
        building(&rig, &run);
        rig.sim.set_pull(12, draft_on(BRANCH));
        start_the_session(&rig);
        (rig, run, cell)
    }

    fn limit(rig: &Rig, run: &RunId) -> LimitStep {
        try_limit(rig, run).unwrap()
    }

    fn stopped(reason: &str) -> LimitStep {
        LimitStep::Stopped {
            reason: reason.into(),
        }
    }

    mod time_limit {
        use super::*;

        #[test]
        fn is_within_the_limit_up_to_and_including_the_last_millisecond_of_it() {
            let (rig, run, _) = timed_built();
            let before = rig.events(&run).len();
            clock_to(&rig, 1);
            assert_eq!(limit(&rig, &run), LimitStep::Within);
            clock_to(&rig, LIMIT_MS);
            assert_eq!(limit(&rig, &run), LimitStep::Within);
            no_writes(&rig);
            assert_eq!(calls(&rig), vec![]);
            assert_eq!(rig.events(&run).len(), before);
            clock_to(&rig, LIMIT_MS + 1);
            assert_eq!(limit(&rig, &run), stopped(TIME_LIMIT));
        }

        #[test]
        fn stops_a_run_past_its_limit_and_names_the_branch_without_a_count_of_commits() {
            let (rig, run, _) = timed_built();
            clock_to(&rig, LIMIT_MS + 1);
            assert_eq!(limit(&rig, &run), stopped(TIME_LIMIT));
            let comments = rig.sim.comments();
            let comment = comments
                .iter()
                .find(|comment| comment.contains("could not finish"))
                .unwrap();
            assert!(
                comment.starts_with(&messages::stuck(
                    TIME_LIMIT,
                    None,
                    Some(&WorkLeft {
                        branch: BRANCH.into(),
                        commits_ahead: None,
                        base: "main".into()
                    })
                )),
                "{comment}"
            );
            assert!(comment.contains("The work so far is on branch `fac/8-1`."));
            let events = rig.events(&run);
            let stuck = events.iter().find(|e| e.event == "stuck").unwrap();
            assert_eq!(stuck.data["reason"], TIME_LIMIT);
            assert_eq!(stuck.data["branch"], BRANCH);
            assert!(stuck.data.get("commits_ahead").is_none());
            assert_eq!(rig.sim.labels(), ["factory:stuck"]);
            assert!(rig.sim.assignees().is_empty());
            // The session ends, and its workspace stays for the next attempt.
            assert_eq!(calls(&rig), vec![ended_workspace()]);
            let last = events.last().unwrap();
            assert_eq!(last.data["outcome"], "stuck");
            let cause = last.cause.as_ref().unwrap();
            assert_eq!(
                (cause["step"].as_str(), cause["reason"].as_str()),
                (Some("pass"), Some("time-limit"))
            );
        }

        #[test]
        fn stops_a_run_with_no_session_thread_with_the_plain_reason() {
            let (rig, cell) = timed();
            let run = run_id(1);
            rig.step(&run, &format!("approved {LABELED_AT}"));
            rig.write(
                &run,
                Event::Claimed {
                    machine: machine().to_string(),
                    github_user: LOGIN.into(),
                },
            );
            rig.moved(&run, State::Ready, State::Triage);
            rig.sim.set_labels(&["factory:triage"]);
            rig.sim.set_assignees(&[LOGIN]);
            stamp(&cell, 0);
            clock_to(&rig, LIMIT_MS + 1);
            assert_eq!(limit(&rig, &run), stopped(TIME_LIMIT));
            let events = rig.events(&run);
            let stuck = events.iter().find(|e| e.event == "stuck").unwrap();
            assert_eq!(stuck.data["reason"], TIME_LIMIT);
            assert!(stuck.data.get("branch").is_none());
            assert_eq!(calls(&rig), vec![]);
        }

        /// The runner's run that "never launched anything" is the same case.
        #[test]
        fn stops_a_run_that_never_launched_anything_with_the_plain_reason() {
            let (rig, _) = timed();
            let run = run_id(1);
            rig.write(
                &run,
                Event::Claimed {
                    machine: machine().to_string(),
                    github_user: LOGIN.into(),
                },
            );
            rig.moved(&run, State::Ready, State::Triage);
            rig.sim.set_labels(&["factory:triage"]);
            rig.sim.set_assignees(&[LOGIN]);
            clock_to(&rig, LIMIT_MS + 1);
            assert_eq!(limit(&rig, &run), stopped(TIME_LIMIT));
            assert_eq!(calls(&rig), vec![]);
        }

        #[test]
        fn is_an_error_with_nothing_written_when_the_archive_fails_and_the_next_call_decides_again()
        {
            let (rig, run, _) = timed_built();
            clock_to(&rig, LIMIT_MS + 1);
            // The session is ended before anything is written: the workspace's sessions are
            // what a stop ends.
            rig.worker
                .on_end_sessions(Err(WorkerError::Failed("the machine said no".into())));
            let before = rig.events(&run).len();
            assert!(matches!(
                try_limit(&rig, &run).unwrap_err(),
                RunError::Worker(_)
            ));
            assert_eq!(rig.events(&run).len(), before);
            no_writes(&rig);
            assert_eq!(limit(&rig, &run), stopped(TIME_LIMIT));
        }

        #[test]
        fn skips_a_finished_run_and_a_run_that_is_waiting_stuck_or_in_the_middle_of_either() {
            let stuck = Event::Stuck {
                reason: "gave up".into(),
                branch: None,
                commits_ahead: None,
            };
            let questions = Event::QuestionsPosted {
                questions: "1. Which?".into(),
            };
            let cases: Vec<(&str, Vec<Event>)> = vec![
                ("finished", vec![]),
                (
                    "needs-info",
                    vec![
                        questions.clone(),
                        Event::StateChanged {
                            from: State::Building,
                            to: State::NeedsInfo,
                        },
                    ],
                ),
                (
                    "stuck",
                    vec![
                        stuck.clone(),
                        Event::StateChanged {
                            from: State::Building,
                            to: State::Stuck,
                        },
                    ],
                ),
                (
                    "pr-open",
                    vec![Event::StateChanged {
                        from: State::Building,
                        to: State::PrOpen,
                    }],
                ),
                ("half stuck", vec![stuck]),
                ("half needs-info", vec![questions]),
            ];
            for (name, more) in cases {
                let (rig, run, _) = timed_built();
                for event in more {
                    rig.write(&run, event);
                }
                if name == "finished" {
                    rig.ledger
                        .finish(
                            &run,
                            crate::factory::ledger::events::Outcome::Stuck,
                            None,
                            None,
                            &crate::factory::ledger::events::Cause::new(
                                crate::factory::ledger::events::CauseStep::Start,
                                "test",
                            ),
                            None,
                        )
                        .unwrap();
                }
                clock_to(&rig, 10 * LIMIT_MS);
                let events = rig.events(&run).len();
                assert!(
                    matches!(limit(&rig, &run), LimitStep::Skipped { .. }),
                    "{name}"
                );
                no_writes(&rig);
                assert_eq!(calls(&rig), vec![], "{name}");
                assert_eq!(rig.events(&run).len(), events, "{name}");
            }
        }

        #[test]
        fn leaves_a_triage_run_whose_launch_was_sent_and_not_recorded_to_resume_which_names_its_session()
         {
            let (rig, _) = timed();
            let run = run_id(1);
            rig.write(
                &run,
                Event::Claimed {
                    machine: machine().to_string(),
                    github_user: LOGIN.into(),
                },
            );
            rig.moved(&run, State::Ready, State::Triage);
            rig.step(&run, "launch-sending");
            clock_to(&rig, 10 * LIMIT_MS);
            let events = rig.events(&run).len();
            assert!(matches!(limit(&rig, &run), LimitStep::Skipped { .. }));
            no_writes(&rig);
            assert_eq!(calls(&rig), vec![]);
            assert_eq!(rig.events(&run).len(), events);
        }

        /// The runner's case, kept under its name, with its expectation inverted on purpose: the
        /// runner skips a run that reported its pull request (`pr-reported`) and so charges
        /// nothing for the wait between the report and the send-back, and this test says the wait
        /// is not charged. Foundry charges it (docs/specs/P2-factory.md, `limits.minutes`: "this
        /// spec does not copy that"): a reported pull request stops no count, and only a gate that
        /// runs or checks that are waited for are left out. So the run is `Stopped` here.
        #[test]
        fn does_not_charge_the_wait_for_the_gate_once_the_session_reported_its_pull_request() {
            let (rig, run, cell) = timed_built();
            stamp(&cell, 20 * MINUTE);
            rig.write(
                &run,
                Event::PrOpened {
                    pr: 12,
                    url: URL12.into(),
                },
            );
            // No gate has begun: the wait is the factory's, but it is counted.
            clock_to(&rig, 10 * LIMIT_MS);
            assert_eq!(
                limit(&rig, &run),
                stopped(TIME_LIMIT),
                "Foundry charges the wait for the gate; the runner's name says it does not"
            );
        }

        #[test]
        fn does_not_charge_the_factorys_gate_to_a_run_it_sent_back() {
            let (rig, run, cell) = timed_built();
            stamp(&cell, 20 * MINUTE);
            rig.write(
                &run,
                Event::PrOpened {
                    pr: 12,
                    url: URL12.into(),
                },
            );
            rig.write(
                &run,
                Event::GateCheckout {
                    attempt: 1,
                    dir: "/d".into(),
                    sha: SHA.into(),
                    base_sha: String::new(),
                },
            );
            stamp(&cell, 90 * MINUTE);
            rig.write(
                &run,
                Event::GateSentBack {
                    attempt: 1,
                    run_ordinal: 1,
                },
            );
            let before = rig.events(&run).len();
            // 20 minutes built, the gate ran for 70, and the session has had 10 since.
            clock_to(&rig, 100 * MINUTE);
            assert_eq!(limit(&rig, &run), LimitStep::Within);
            no_writes(&rig);
            assert_eq!(calls(&rig), vec![]);
            assert_eq!(rig.events(&run).len(), before);
            // 70 minutes since the send-back is 90 minutes worked, and one more is past it.
            clock_to(&rig, 160 * MINUTE);
            assert_eq!(limit(&rig, &run), LimitStep::Within);
            clock_to(&rig, 161 * MINUTE);
            assert_eq!(limit(&rig, &run), stopped(TIME_LIMIT));
        }

        #[test]
        fn limits_a_run_again_once_it_comes_back_to_building_from_pr_open() {
            let (rig, run, cell) = timed_built();
            rig.moved(&run, State::Building, State::PrOpen);
            stamp(&cell, 2 * MINUTE);
            rig.moved(&run, State::PrOpen, State::Building);
            clock_to(&rig, 2 * MINUTE + LIMIT_MS + 1);
            assert_eq!(limit(&rig, &run), stopped(TIME_LIMIT));
        }

        #[test]
        fn names_a_hostile_branch_in_the_comment_and_the_ledger_only_as_prepared_text() {
            let (rig, cell) = timed();
            let run = run_id(1);
            stamp(&cell, 0);
            building_with(
                &rig,
                &run,
                launch(Some("x@octocat<!-- softfac -->"), None, Some("main")),
            );
            start_the_session(&rig);
            clock_to(&rig, LIMIT_MS + 1);
            assert_eq!(limit(&rig, &run), stopped(TIME_LIMIT));
            let prepared = "x@\u{200b}octocat<\u{200b}!-- softfac -->";
            let events = rig.events(&run);
            let stuck = events.iter().find(|e| e.event == "stuck").unwrap();
            assert_eq!(stuck.data["branch"], prepared);
            let comments = rig.sim.comments();
            let comment = comments
                .iter()
                .find(|comment| comment.contains("could not finish"))
                .unwrap();
            // `messages::stuck` cleans and escapes the branch it is given once more, so the
            // prepared text (whose zero-width space is an invisible character) shows with a space
            // where the runner's comment showed the prepared text as it was. The comment is
            // exactly what `messages::stuck` renders for it.
            let expected = messages::stuck(
                TIME_LIMIT,
                None,
                Some(&WorkLeft {
                    branch: prepared.into(),
                    commits_ahead: None,
                    base: "main".into(),
                }),
            );
            assert!(comment.starts_with(&expected), "{comment}\n{expected}");
            assert!(!comment.contains("x@octocat"), "{comment}");
            assert!(!comment.contains("@octocat"), "{comment}");
            assert!(!comment.contains("octocat<!--"), "{comment}");
        }

        #[test]
        fn reads_the_wall_clock_on_the_ledgers_base_not_the_sleep_counting_clock() {
            use crate::clock::Clock;
            let (rig, run, _) = timed_built();
            // The context's clock reads near zero, as a daemon that booted a moment ago does,
            // while the ledger's stamps are epoch milliseconds.
            assert!(rig.clock.now() < std::time::Duration::from_secs(1));
            clock_to(&rig, LIMIT_MS);
            assert_eq!(limit(&rig, &run), LimitStep::Within);
            clock_to(&rig, LIMIT_MS + 1);
            assert_eq!(limit(&rig, &run), stopped(TIME_LIMIT));
        }

        #[test]
        fn takes_its_now_from_the_system_wall_clock_the_ledger_stamps_with() {
            let now = crate::factory::ledger::system_ms();
            for (ago, stops) in [(LIMIT_MS + 10 * MINUTE, true), (10 * MINUTE, false)] {
                let (rig, cell) = timed();
                cell.store(now - ago, Ordering::SeqCst);
                let run = run_id(1);
                building(&rig, &run);
                start_the_session(&rig);
                let step = time_limit(&rig.ctx(), &run).unwrap();
                if stops {
                    assert_eq!(step, stopped(TIME_LIMIT));
                } else {
                    assert_eq!(step, LimitStep::Within);
                }
            }
        }

        #[test]
        fn does_not_take_over_a_stuck_path_or_a_questions_post_the_advance_step_began() {
            // A stuck decision recorded, its comment not yet posted.
            let (mut rig, run, _) = timed_built();
            let message = "Done.\nFACTORY-DONE: https://github.com/rawphp/softfac/pull/13";
            end_turn(&mut rig, 1, message);
            rig.sim.fail("issue comment", 1);
            assert!(advance(&rig.ctx(), &run).is_err());
            assert!(rig.ctx().seen(&run).unwrap().stuck_stop.is_some());
            let events = rig.events(&run).len();
            let writes = rig.sim.writes();
            clock_to(&rig, 10 * LIMIT_MS);
            assert!(matches!(limit(&rig, &run), LimitStep::Skipped { .. }));
            assert_eq!(rig.events(&run).len(), events);
            assert_eq!(rig.sim.writes(), writes);
            // The next advance finishes it with the reason it decided on.
            say(&rig, message);
            assert_eq!(
                pass(&rig, &run),
                BuildStep::Stuck {
                    reason: joined("the session reported PR #13 but it does not exist", message)
                }
            );
            assert_eq!(
                last_event(&rig, &run).cause.unwrap()["reason"],
                "pr-not-bound"
            );

            // The questions' comment begun and not posted.
            let (mut rig, run, _) = timed_built();
            let message = "Which one?\nFACTORY-STOP: needs-info";
            end_turn(&mut rig, 1, message);
            rig.sim.fail("issue comment", 1);
            assert!(advance(&rig.ctx(), &run).is_err());
            assert!(rig.ctx().seen(&run).unwrap().questions_commenting.is_some());
            let events = rig.events(&run).len();
            let writes = rig.sim.writes();
            clock_to(&rig, 10 * LIMIT_MS);
            assert!(matches!(limit(&rig, &run), LimitStep::Skipped { .. }));
            assert_eq!(rig.events(&run).len(), events);
            assert_eq!(rig.sim.writes(), writes);
            say(&rig, message);
            assert_eq!(pass(&rig, &run), BuildStep::NeedsInfo);
            assert_eq!(questions_comments(&rig), 1);
        }

        #[test]
        fn leaves_a_run_that_resume_has_decided_to_relaunch_to_it() {
            let (rig, run, _) = timed_built();
            rig.write(
                &run,
                Event::SessionInterrupted {
                    resumes: 1,
                    relaunch: true,
                    run_ordinal: Some(1),
                    branch: Some(BRANCH.into()),
                    t3_error: None,
                    ended_in_pass: 1,
                },
            );
            clock_to(&rig, 10 * LIMIT_MS);
            let events = rig.events(&run).len();
            assert!(matches!(limit(&rig, &run), LimitStep::Skipped { .. }));
            assert_eq!(rig.events(&run).len(), events);
            no_writes(&rig);
        }

        /// The gate passes, the required checks are waited for, a failed check is sent back to
        /// the session, and the session then hangs: neither the gate nor the wait for the checks
        /// is charged, and the run is ended once the session's own time passes the limit.
        #[test]
        fn ends_a_session_that_hangs_on_a_check_failure_sent_back() {
            let (rig, run, cell) = timed_built();
            stamp(&cell, 10 * MINUTE);
            rig.write(
                &run,
                Event::PrOpened {
                    pr: 12,
                    url: URL12.into(),
                },
            );
            stamp(&cell, 12 * MINUTE);
            rig.write(
                &run,
                Event::GateCheckout {
                    attempt: 1,
                    dir: "/d".into(),
                    sha: SHA.into(),
                    base_sha: String::new(),
                },
            );
            stamp(&cell, 20 * MINUTE);
            rig.write(
                &run,
                Event::GateCommand {
                    attempt: 1,
                    command: "cargo test".into(),
                    exit_code: 0,
                    duration_ms: 5,
                },
            );
            stamp(&cell, 25 * MINUTE);
            rig.step(&run, &format!("gate-passed {SHA} 1"));
            // The checks are waited for until 40 minutes, when one failed and was sent back.
            stamp(&cell, 40 * MINUTE);
            rig.write(
                &run,
                Event::GateSentBack {
                    attempt: 1,
                    run_ordinal: 1,
                },
            );
            let seen = rig.ctx().seen(&run).unwrap();
            assert_eq!(seen.worked_ms, 12 * MINUTE);
            assert_eq!(seen.working_since, Some(T0 + 40 * MINUTE));
            clock_to(&rig, 40 * MINUTE + 78 * MINUTE);
            assert_eq!(limit(&rig, &run), LimitStep::Within);
            clock_to(&rig, 40 * MINUTE + 78 * MINUTE + 1);
            assert_eq!(limit(&rig, &run), stopped(TIME_LIMIT));
        }
    }

    // ----- the runner's `watch and timeLimit: the end of a run` -----

    mod watch_and_time_limit_the_end_of_a_run {
        use super::*;

        #[test]
        fn the_time_limit_leaves_a_stopped_run_to_advance() {
            let (rig, run, _) = timed_built();
            rig.write(
                &run,
                Event::StopRequested {
                    by: "operator".into(),
                },
            );
            clock_to(&rig, LIMIT_MS + 1);
            assert!(matches!(limit(&rig, &run), LimitStep::Skipped { .. }));
            no_writes(&rig);
            assert_eq!(calls(&rig), vec![]);
        }

        fn sent_back_after_a_pull_request(rig: &Rig, run: &RunId) {
            rig.write(
                run,
                Event::PrOpened {
                    pr: 12,
                    url: URL12.into(),
                },
            );
            rig.write(
                run,
                Event::GateSentBack {
                    attempt: 1,
                    run_ordinal: 1,
                },
            );
        }

        #[test]
        fn the_time_limit_skips_a_run_past_its_limit_whose_own_merge_closed_the_issue() {
            let (rig, run, _) = timed_built();
            sent_back_after_a_pull_request(&rig, &run);
            rig.sim.set_closed(true);
            rig.sim
                .set_pull(12, pull("MERGED", false, BRANCH, "main", false));
            clock_to(&rig, LIMIT_MS + 1);
            assert!(matches!(limit(&rig, &run), LimitStep::Skipped { .. }));
            no_writes(&rig);
            assert_eq!(calls(&rig), vec![]);
        }

        #[test]
        fn the_time_limit_goes_on_to_stop_a_run_past_its_limit_whose_issue_a_person_closed() {
            let (rig, run, _) = timed_built();
            sent_back_after_a_pull_request(&rig, &run);
            rig.sim.set_closed(true);
            rig.sim
                .set_pull(12, pull("CLOSED", false, BRANCH, "main", false));
            clock_to(&rig, LIMIT_MS + 1);
            assert!(matches!(limit(&rig, &run), LimitStep::Stopped { .. }));
        }

        #[test]
        fn the_time_limit_asks_about_the_issue_only_past_the_limit_and_with_a_pull_request() {
            let (within, run, _) = timed_built();
            sent_back_after_a_pull_request(&within, &run);
            within.sim.set_closed(true);
            clock_to(&within, 1);
            assert_eq!(limit(&within, &run), LimitStep::Within);
            assert_eq!(issue_reads(&within), 0);

            let (no_pr, run, _) = timed_built();
            no_pr.sim.set_closed(true);
            clock_to(&no_pr, LIMIT_MS + 1);
            assert!(matches!(limit(&no_pr, &run), LimitStep::Stopped { .. }));
            // The issue itself is not read: no pull request was reported.
            assert_eq!(issue_reads(&no_pr), 0);
        }
    }

    // ----- the runner's `advance: a merged or closed pull request ends the run` -----

    mod advance_a_merged_or_closed_pull_request_ends_the_run {
        use super::*;

        /// The runner's `pr-open` cases of this describe are the gate's and the feedback step's:
        /// a run in `pr-open` is [`BuildStep::Skipped`] here.
        #[test]
        fn done_with_a_merged_pull_request_ends_the_run_as_merged() {
            let (mut rig, run) = built();
            let before = rig.events(&run).len();
            rig.sim
                .set_pull(12, pull("MERGED", false, BRANCH, "main", false));
            end_turn(&mut rig, 1, DONE_MESSAGE);
            assert_eq!(pass(&rig, &run), BuildStep::Ended);
            // Nothing is posted and no label moves; the session is read, then ended.
            no_writes(&rig);
            assert_eq!(
                calls(&rig),
                vec![
                    Call::Turn {
                        session: session(),
                        n: 1
                    },
                    ended_workspace()
                ]
            );
            assert_eq!(names_after(&rig, &run, before), ["pr_opened", "finished"]);
            let finished = last_event(&rig, &run);
            assert_eq!(finished.data["outcome"], "pr-open");
            assert_eq!(finished.data["pr_end"], "merged");
            assert_eq!(finished.data["pr_url"], URL12);
            assert_eq!(rig.sim.labels(), ["factory:building"]);
            assert_eq!(
                cause_of_last(&rig, &run),
                ("advance".into(), "pull-request-ended".into())
            );
            // A run that is ended is not ended again.
            let worker = calls(&rig).len();
            assert!(matches!(pass(&rig, &run), BuildStep::Skipped { .. }));
            assert_eq!(calls(&rig).len(), worker);
        }

        #[test]
        fn done_with_a_pull_request_closed_without_a_merge_ends_the_run_as_closed() {
            let (mut rig, run) = built();
            rig.sim
                .set_pull(12, pull("CLOSED", false, BRANCH, "main", false));
            end_turn(&mut rig, 1, DONE_MESSAGE);
            assert_eq!(pass(&rig, &run), BuildStep::Ended);
            no_writes(&rig);
            let finished = last_event(&rig, &run);
            assert_eq!(finished.data["pr_end"], "closed");
            assert_eq!(
                cause_of_last(&rig, &run),
                ("advance".into(), "pull-request-ended".into())
            );
        }

        #[test]
        fn done_with_a_merged_pull_request_that_a_failed_archive_cannot_stop_writes_nothing() {
            let (mut rig, run) = built();
            let before = rig.events(&run).len();
            rig.sim
                .set_pull(12, pull("MERGED", false, BRANCH, "main", false));
            end_turn(&mut rig, 1, DONE_MESSAGE);
            // The session cannot be ended.
            rig.worker
                .on_end_sessions(Err(WorkerError::Failed("the machine said no".into())));
            assert!(matches!(
                advance(&rig.ctx(), &run).unwrap_err(),
                RunError::Worker(_)
            ));
            // The pull request is recorded; the end is not.
            assert_eq!(names_after(&rig, &run, before), ["pr_opened"]);
            say(&rig, DONE_MESSAGE);
            assert_eq!(pass(&rig, &run), BuildStep::Ended);
            assert_eq!(last_event(&rig, &run).data["pr_end"], "merged");
            assert_eq!(
                rig.events(&run)
                    .iter()
                    .filter(|e| e.event == "pr_opened")
                    .count(),
                1
            );
        }
    }

    // ----- the runner's `advance: a closed issue`, the cases the runner's names are kept for -----

    mod advance_a_closed_issue {
        use super::*;

        /// The recorded pass of a gate that died before the move to `pr-open`.
        fn pass_recorded(rig: &Rig, run: &RunId) {
            rig.write(
                run,
                Event::PrOpened {
                    pr: 12,
                    url: URL12.into(),
                },
            );
            rig.write(
                run,
                Event::GateCommand {
                    attempt: 1,
                    command: "cargo test".into(),
                    exit_code: 0,
                    duration_ms: 5,
                },
            );
            rig.step(run, &format!("gate-passed {SHA} 1"));
            assert!(rig.ctx().seen(run).unwrap().pass.is_some());
        }

        #[test]
        fn closed_by_a_merge_after_a_recorded_pass_leaves_the_end_to_the_gate() {
            let (rig, run) = built();
            pass_recorded(&rig, &run);
            rig.sim.set_closed(true);
            rig.sim
                .set_pull(12, pull("MERGED", false, BRANCH, "main", false));
            let before = rig.events(&run).len();
            assert_eq!(
                pass(&rig, &run),
                BuildStep::ReadyForGate {
                    pr: 12,
                    url: URL12.into()
                }
            );
            no_writes(&rig);
            assert_eq!(calls(&rig), vec![]);
            assert_eq!(rig.events(&run).len(), before);
            assert!(!rig.finished(&run));
        }

        /// A recorded pass is the gate's: the session is not read, and a turn that ended with no
        /// result line is neither read nor sent a reminder.
        #[test]
        fn a_recorded_pass_with_an_unread_turn_that_has_no_result_line_is_the_gates() {
            let (mut rig, run) = built();
            pass_recorded(&rig, &run);
            end_turn(&mut rig, 1, WORKING_MESSAGE);
            let before = rig.events(&run).len();
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
            assert!(sends(&rig).is_empty());
            assert_eq!(turn_reads(&rig), 0);
            assert_eq!(rig.events(&run).len(), before);
            no_writes(&rig);
        }
    }

    // ----- what Foundry decides where the runner did not -----

    mod foundry {
        use super::*;

        #[test]
        fn a_turn_number_already_recorded_is_not_acted_on_again_after_a_done() {
            let (mut rig, run) = built();
            end_turn(&mut rig, 1, DONE_MESSAGE);
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
            assert_eq!(turn_reads(&rig), 1);
            let events = rig.events(&run).len();
            for _ in 0..3 {
                assert_eq!(
                    pass(&rig, &run),
                    BuildStep::ReadyForGate {
                        pr: 12,
                        url: URL12.into()
                    }
                );
            }
            // No second read of the turn, nothing written, nothing sent.
            assert_eq!(turn_reads(&rig), 1);
            assert_eq!(rig.events(&run).len(), events);
            assert!(sends(&rig).is_empty());
            assert!(rig.sim.writes().is_empty());
        }

        #[test]
        fn a_turn_number_already_recorded_is_not_acted_on_again_after_a_reminder() {
            let (mut rig, run) = built();
            end_turn(&mut rig, 1, WORKING_MESSAGE);
            assert_eq!(pass(&rig, &run), BuildStep::Reminded);
            let events = rig.events(&run).len();
            for _ in 0..3 {
                assert_eq!(pass(&rig, &run), BuildStep::Running);
            }
            assert_eq!(turn_reads(&rig), 1);
            assert_eq!(sends(&rig), vec![reminder_sent(1)]);
            assert_eq!(rig.events(&run).len(), events);
        }

        #[test]
        fn a_turn_read_is_recorded_last_so_a_pass_cut_short_acts_on_it_again_without_repeating_a_write()
         {
            let (mut rig, run) = built();
            end_turn(&mut rig, 1, DONE_MESSAGE);
            // The pull request is recorded, and the pass dies before `turn-read 1`.
            rig.write(
                &run,
                Event::PrOpened {
                    pr: 12,
                    url: URL12.into(),
                },
            );
            let before = rig.events(&run).len();
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
            assert_eq!(steps_after(&rig, &run, before), ["turn-read 1"]);
            // No second `pr_opened`: the only line written is the step.
            assert_eq!(names_after(&rig, &run, before), ["step_changed"]);
            assert_eq!(rig.ctx().seen(&run).unwrap().opened, [12]);
        }

        #[test]
        fn a_build_session_idle_through_a_whole_gate_is_sent_nothing() {
            let (mut rig, run) = built();
            end_turn(&mut rig, 1, DONE_MESSAGE);
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
            let step = |rig: &Rig| {
                for _ in 0..4 {
                    assert_eq!(
                        pass(rig, &run),
                        BuildStep::ReadyForGate {
                            pr: 12,
                            url: URL12.into()
                        }
                    );
                }
            };
            // The gate is placed, its commands run, and it passes: the session idles all along.
            rig.write(
                &run,
                Event::GateCheckout {
                    attempt: 1,
                    dir: "/d".into(),
                    sha: SHA.into(),
                    base_sha: String::new(),
                },
            );
            step(&rig);
            rig.write(
                &run,
                Event::GateCommand {
                    attempt: 1,
                    command: "cargo test".into(),
                    exit_code: 0,
                    duration_ms: 5,
                },
            );
            step(&rig);
            let events = rig.events(&run).len();
            rig.step(&run, &format!("gate-passed {SHA} 1"));
            step(&rig);
            assert_eq!(rig.events(&run).len(), events + 1);
            assert!(sends(&rig).is_empty());
            assert_eq!(turn_reads(&rig), 1);
            no_writes(&rig);
        }

        /// The runner counts a run in `building` from its ledger too: it stops at the reported
        /// pull request (and skips the run while `pr-reported` holds) and starts again at the
        /// send-back, which also clears `pr-reported`. So at the instant tested here the runner
        /// has not ended the run either: it charges the 10 minutes before the report and the
        /// time since the send-back (88 minutes at the first instant below), and ends it two
        /// minutes later. What Foundry changes (spec, `limits.minutes`) is that the 2 minutes
        /// the pull request waited for its gate slot are charged, so the same ledger is ended
        /// at the first millisecond past 90 minutes worked, where the runner's rule is still
        /// within its limit. Where the runner never ends a session is a pull request reported
        /// again and then left with no gate (its `pr-reported` skip); Foundry counts that too
        /// ([`does_not_charge_the_wait_for_the_gate_once_the_session_reported_its_pull_request`]).
        #[test]
        fn a_session_that_hangs_on_a_gate_failure_sent_back_is_ended_by_limits_minutes() {
            let (mut rig, run, cell) = timed_built();
            end_turn(&mut rig, 1, DONE_MESSAGE);
            stamp(&cell, 10 * MINUTE);
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
            // The gate waited for a slot for 2 minutes, then ran for 13 and failed.
            stamp(&cell, 12 * MINUTE);
            rig.write(
                &run,
                Event::GateCheckout {
                    attempt: 1,
                    dir: "/d".into(),
                    sha: SHA.into(),
                    base_sha: String::new(),
                },
            );
            stamp(&cell, 25 * MINUTE);
            rig.write(
                &run,
                Event::GateSentBack {
                    attempt: 1,
                    run_ordinal: 1,
                },
            );
            // The session hangs: it never ends the turn it was sent back into.
            show(&mut rig, SessionState::Working, 1, 2);
            let seen = rig.ctx().seen(&run).unwrap();
            assert!(!seen.pr_reported);
            assert_eq!(seen.worked_ms, 12 * MINUTE);
            assert_eq!(seen.working_since, Some(T0 + 25 * MINUTE));
            clock_to(&rig, 25 * MINUTE + 78 * MINUTE);
            assert_eq!(limit(&rig, &run), LimitStep::Within);
            assert_eq!(pass(&rig, &run), BuildStep::Running);
            clock_to(&rig, 25 * MINUTE + 78 * MINUTE + 1);
            assert_eq!(limit(&rig, &run), stopped(TIME_LIMIT));
            assert_eq!(rig.sim.labels(), ["factory:stuck"]);
            assert!(calls(&rig).contains(&ended_workspace()));
        }

        #[test]
        fn a_session_p1_marks_stuck_is_left_alone_and_is_ended_by_limits_minutes() {
            let (mut rig, run, _) = timed_built();
            show(&mut rig, SessionState::Stuck, 1, 1);
            say(&rig, DONE_MESSAGE);
            clock_to(&rig, LIMIT_MS);
            let events = rig.events(&run).len();
            assert_eq!(pass(&rig, &run), BuildStep::Running);
            assert_eq!(limit(&rig, &run), LimitStep::Within);
            no_writes(&rig);
            assert_eq!(calls(&rig), vec![]);
            assert_eq!(rig.events(&run).len(), events);
            clock_to(&rig, LIMIT_MS + 1);
            assert_eq!(limit(&rig, &run), stopped(TIME_LIMIT));
            assert_eq!(rig.sim.labels(), ["factory:stuck"]);
        }

        #[test]
        fn a_session_waiting_for_a_person_goes_stuck_at_once_with_its_question() {
            let (mut rig, run) = built();
            show_with(&mut rig, SessionState::WaitingForYou, 1, 1, true);
            rig.worker.on_turn(Ok(TurnText {
                message: String::new(),
                question: Some("Which database should I use?\n@octocat".into()),
            }));
            let step = pass(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "session-waiting",
                SESSION_WAITING,
                None,
                Some("Which database should I use?\n@octocat"),
            );
            let comment = rig
                .sim
                .comments()
                .into_iter()
                .find(|comment| comment.contains("could not finish"))
                .unwrap();
            assert!(comment.contains("Which database should I use?\n@\u{200b}octocat"));
        }

        #[test]
        fn a_question_that_waits_on_an_idle_session_is_stuck_too() {
            let (mut rig, run) = built();
            show_with(&mut rig, SessionState::Idle, 1, 1, true);
            rig.worker.on_turn(Ok(TurnText {
                message: "Working.".into(),
                question: Some("May I delete the table?".into()),
            }));
            let step = pass(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "session-waiting",
                SESSION_WAITING,
                None,
                Some("May I delete the table?"),
            );
            assert!(sends(&rig).is_empty());
        }

        #[test]
        fn a_stuck_path_begun_is_finished_with_the_session_text_read_again() {
            let (mut rig, run) = built();
            let message = "Done.\nFACTORY-DONE: https://github.com/rawphp/softfac/pull/13";
            end_turn(&mut rig, 1, message);
            // The comment fails after the decision was recorded.
            rig.sim.fail("issue comment", 1);
            assert!(advance(&rig.ctx(), &run).is_err());
            let seen = rig.ctx().seen(&run).unwrap();
            assert!(seen.stuck_stop.is_some() && seen.stuck.is_none());
            // The decision holds no session text: the next pass reads the turn again.
            say(&rig, message);
            let step = pass(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "pr-not-bound",
                "the session reported PR #13 but it does not exist",
                None,
                Some(message),
            );
            assert_eq!(turn_reads(&rig), 2);
            assert_eq!(
                rig.sim
                    .comments()
                    .iter()
                    .filter(|comment| comment.contains("could not finish"))
                    .count(),
                1
            );
        }

        #[test]
        fn a_stuck_path_begun_whose_turn_cannot_be_read_posts_the_reason_alone() {
            let (mut rig, run) = built();
            let message = "Done.\nFACTORY-DONE: https://github.com/rawphp/softfac/pull/13";
            end_turn(&mut rig, 1, message);
            rig.sim.fail("issue comment", 1);
            assert!(advance(&rig.ctx(), &run).is_err());
            rig.worker
                .on_turn(Err(WorkerError::Unreachable("the machine is away".into())));
            let step = pass(&rig, &run);
            assert_stuck(
                &rig,
                &run,
                &step,
                "pr-not-bound",
                "the session reported PR #13 but it does not exist",
                None,
                None,
            );
        }

        #[test]
        fn a_person_closing_the_issue_abandons_the_run() {
            let (mut rig, run) = built();
            end_turn(&mut rig, 1, DONE_MESSAGE);
            rig.sim.set_closed(true);
            assert_eq!(pass(&rig, &run), BuildStep::Abandoned);
            no_writes(&rig);
            assert_eq!(calls(&rig), vec![ended_workspace()]);
            assert_eq!(last_event(&rig, &run).data["outcome"], "stuck");
            assert_eq!(
                cause_of_last(&rig, &run),
                ("advance".into(), "issue-closed".into())
            );
        }

        #[test]
        fn the_runs_own_merge_closing_the_issue_ends_the_run() {
            let (mut rig, run) = built();
            rig.write(
                &run,
                Event::PrOpened {
                    pr: 12,
                    url: URL12.into(),
                },
            );
            rig.sim.set_closed(true);
            rig.sim
                .set_pull(12, pull("MERGED", false, BRANCH, "main", false));
            show(&mut rig, SessionState::Idle, 1, 1);
            assert_eq!(pass(&rig, &run), BuildStep::Ended);
            assert_eq!(last_event(&rig, &run).data["pr_end"], "merged");
        }

        #[test]
        fn a_run_another_step_owns_is_skipped_and_nothing_is_written() {
            let cases: Vec<(&str, Vec<Event>)> = vec![
                (
                    "stop asked for",
                    vec![Event::StopRequested {
                        by: "operator".into(),
                    }],
                ),
                (
                    "interruption decided",
                    vec![Event::SessionInterrupted {
                        resumes: 1,
                        relaunch: false,
                        run_ordinal: Some(1),
                        branch: None,
                        t3_error: None,
                        ended_in_pass: 1,
                    }],
                ),
                (
                    "pr-open",
                    vec![Event::StateChanged {
                        from: State::Building,
                        to: State::PrOpen,
                    }],
                ),
            ];
            for (name, more) in cases {
                let (mut rig, run) = built();
                for event in more {
                    rig.write(&run, event);
                }
                end_turn(&mut rig, 1, DONE_MESSAGE);
                let events = rig.events(&run).len();
                assert!(
                    matches!(pass(&rig, &run), BuildStep::Skipped { .. }),
                    "{name}"
                );
                no_writes(&rig);
                assert_eq!(calls(&rig), vec![], "{name}");
                assert_eq!(rig.events(&run).len(), events, "{name}");
            }
            let (rig, run) = built();
            rig.ledger
                .finish(
                    &run,
                    crate::factory::ledger::events::Outcome::Stuck,
                    None,
                    None,
                    &crate::factory::ledger::events::Cause::new(
                        crate::factory::ledger::events::CauseStep::Start,
                        "test",
                    ),
                    None,
                )
                .unwrap();
            assert!(matches!(pass(&rig, &run), BuildStep::Skipped { .. }));
        }

        #[test]
        fn a_run_in_triage_with_no_session_recorded_is_starts_to_launch() {
            let rig = Rig::new();
            let run = run_id(1);
            rig.step(&run, &format!("approved {LABELED_AT}"));
            rig.write(
                &run,
                Event::Claimed {
                    machine: machine().to_string(),
                    github_user: LOGIN.into(),
                },
            );
            rig.moved(&run, State::Ready, State::Triage);
            rig.sim.set_labels(&["factory:triage"]);
            rig.sim.set_assignees(&[LOGIN]);
            let events = rig.events(&run).len();
            assert_eq!(pass(&rig, &run), BuildStep::Running);
            no_writes(&rig);
            assert_eq!(rig.events(&run).len(), events);
        }

        #[test]
        fn a_building_run_with_no_session_recorded_is_an_error() {
            let rig = Rig::new();
            let run = run_id(1);
            rig.step(&run, &format!("approved {LABELED_AT}"));
            rig.write(
                &run,
                Event::Claimed {
                    machine: machine().to_string(),
                    github_user: LOGIN.into(),
                },
            );
            rig.moved(&run, State::Ready, State::Triage);
            rig.moved(&run, State::Triage, State::Building);
            let error = advance(&rig.ctx(), &run).unwrap_err();
            assert!(matches!(error, RunError::Invalid(_)), "{error}");
        }

        #[test]
        fn a_session_with_no_turn_ended_yet_is_left_to_start() {
            let (mut rig, run) = built();
            show(&mut rig, SessionState::Idle, 0, 0);
            let events = rig.events(&run).len();
            assert_eq!(pass(&rig, &run), BuildStep::Running);
            assert_eq!(calls(&rig), vec![]);
            assert_eq!(rig.events(&run).len(), events);
        }

        /// The wall clock steps back by minutes while a run builds (review focus 5): the stint
        /// began at 500 minutes by the ledger's stamps and "now" reads 0, so no negative span is
        /// counted, even against a limit of nothing.
        #[test]
        fn a_clock_that_reads_before_the_stint_began_adds_no_worked_time() {
            let (mut rig, cell) = timed();
            let run = run_id(1);
            stamp(&cell, 500 * MINUTE);
            building(&rig, &run);
            rig.config.limits.minutes = 0;
            clock_to(&rig, 500 * MINUTE);
            assert_eq!(limit(&rig, &run), LimitStep::Within);
            clock_to(&rig, 0);
            assert_eq!(limit(&rig, &run), LimitStep::Within);
            assert_eq!(
                rig.ctx().seen(&run).unwrap().working_since,
                Some(T0 + 500 * MINUTE)
            );
            // One millisecond past the stint's start is past a limit of nothing.
            clock_to(&rig, 500 * MINUTE + 1);
            assert_eq!(limit(&rig, &run), stopped(TIME_LIMIT));
        }

        #[test]
        fn a_second_report_after_a_gate_failure_sent_back_is_recorded_again() {
            let (mut rig, run) = built();
            end_turn(&mut rig, 1, DONE_MESSAGE);
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
            rig.write(
                &run,
                Event::GateSentBack {
                    attempt: 1,
                    run_ordinal: 1,
                },
            );
            assert!(!rig.ctx().seen(&run).unwrap().pr_reported);
            // The session fixes it and reports the same pull request again, on turn 2.
            end_turn(&mut rig, 2, DONE_MESSAGE);
            let before = rig.events(&run).len();
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
            assert_eq!(
                names_after(&rig, &run, before),
                ["pr_opened", "step_changed"]
            );
            assert_eq!(steps_after(&rig, &run, before), ["turn-read 2"]);
            let seen = rig.ctx().seen(&run).unwrap();
            assert!(seen.pr_reported);
            assert_eq!(seen.opened, [12]);
            let after = rig.events(&run).len();
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
            assert_eq!(rig.events(&run).len(), after);
        }

        #[test]
        fn questions_with_hostile_text_are_posted_and_recorded_as_prepared_text() {
            let (mut rig, run) = built();
            let text = format!(
                "ping @octocat <!-- softfac -->\u{202e}\n```\n{}",
                "x".repeat(6100)
            );
            end_turn(&mut rig, 1, &format!("{text}\nFACTORY-STOP: needs-info"));
            assert_eq!(pass(&rig, &run), BuildStep::NeedsInfo);
            let prepared = messages::prepare_session_text(text.trim());
            assert!(prepared.ends_with("\n[cut]"));
            let comments = rig.sim.comments();
            assert!(
                comments[0].starts_with(&messages::questions(&prepared)),
                "{}",
                comments[0]
            );
            assert!(!comments[0].contains("@octocat"));
            assert!(!comments[0].contains("octocat <!--"));
            let posted = rig
                .events(&run)
                .into_iter()
                .find(|e| e.event == "questions_posted")
                .unwrap();
            assert_eq!(posted.data["questions"], prepared);
        }

        #[test]
        fn a_turn_read_before_the_session_was_launched_again_is_not_a_turn_read_by_the_new_one() {
            let (mut rig, run) = built();
            rig.step(&run, "turn-read 3");
            rig.write(&run, launch(Some(BRANCH), None, Some("main")));
            end_turn(&mut rig, 1, DONE_MESSAGE);
            let before = rig.events(&run).len();
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
            assert_eq!(steps_after(&rig, &run, before), ["turn-read 1"]);
        }

        #[test]
        fn a_pull_request_on_the_branch_a_launch_resumed_from_binds() {
            let (mut rig, run) = built();
            rig.step(&run, "launch-sending resumed-from fac/9-1");
            assert_eq!(
                rig.ctx().seen(&run).unwrap().launch_resumed_from.as_deref(),
                Some("fac/9-1")
            );
            rig.sim.set_pull(12, draft_on("fac/9-1"));
            end_turn(&mut rig, 1, DONE_MESSAGE);
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
        }

        #[test]
        fn a_pull_request_on_a_branch_the_run_bound_binds() {
            let (mut rig, run) = built();
            rig.write(
                &run,
                Event::BranchBound {
                    pr: 12,
                    branch: "fac/bound".into(),
                },
            );
            rig.sim.set_pull(12, draft_on("fac/bound"));
            end_turn(&mut rig, 1, DONE_MESSAGE);
            assert!(matches!(
                pass(&rig, &run),
                BuildStep::ReadyForGate { pr: 12, .. }
            ));
        }

        #[test]
        fn a_session_whose_process_ended_after_its_pull_request_was_reported_waits_for_the_gate() {
            for state in [SessionState::Finished, SessionState::Failed] {
                let (mut rig, run) = built();
                end_turn(&mut rig, 1, DONE_MESSAGE);
                assert!(matches!(
                    pass(&rig, &run),
                    BuildStep::ReadyForGate { pr: 12, .. }
                ));
                show(&mut rig, state, 1, 1);
                let events = rig.events(&run).len();
                assert!(
                    matches!(pass(&rig, &run), BuildStep::ReadyForGate { pr: 12, .. }),
                    "{state:?}"
                );
                assert_eq!(rig.events(&run).len(), events);
                assert_eq!(turn_reads(&rig), 1);
            }
            // With nothing reported, it is an interruption for `resume`.
            let (mut rig, run) = built();
            rig.step(&run, "turn-read 1");
            show(&mut rig, SessionState::Finished, 1, 1);
            assert_eq!(pass(&rig, &run), BuildStep::Interrupted);
        }

        #[test]
        fn a_relaunch_decided_for_a_gate_failure_is_resumes() {
            let (rig, run) = built();
            rig.write(
                &run,
                Event::RegateRelaunching {
                    attempt: 1,
                    branch: BRANCH.into(),
                    message: "fix it".into(),
                },
            );
            let events = rig.events(&run).len();
            assert!(matches!(pass(&rig, &run), BuildStep::Skipped { .. }));
            assert_eq!(rig.events(&run).len(), events);
            assert_eq!(calls(&rig), vec![]);
        }
    }
}

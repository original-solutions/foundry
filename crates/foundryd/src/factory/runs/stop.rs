//! How a run ends: stopped by hand, stopped by a person's move, or done.
//!
//! Ported from `src/runner/stop.ts`, the stop-run part of `src/control.ts`, the runner's `watch`
//! (the text check of `src/runner/building.ts`, which `building` leaves to this module) and the
//! ends of a run `src/pass.ts` calls (`closedStuck`, and the merged or closed pull request of a
//! `pr-open` run, which `gating` leaves to this module). Three steps:
//!
//! - [`request`] records a stop asked for by hand. The runner left a file for the pass to take
//!   into the ledger; Foundry has no data dir, and a Stop from the Factory view is carried to
//!   the lease holder (spec, "When things go wrong"), which calls this with its ledger and its
//!   guard. A finished run is refused, then a run that is not `building` (so a stale line of a
//!   run that left `building` is never answered "already"), then a stop already on record,
//!   else the `stop_requested` line is written (cause `operator`, `stop-requested`) after the
//!   lease is checked. [`not_building_message`], [`requested_message`],
//!   [`already_requested_message`] and [`StopRequest::answer`] are the words of `stop.ts` and
//!   `control.ts` for the route that calls it.
//! - [`advance`] carries out a stop on the next pass: a building run goes stuck with
//!   `stop-requested` and [`STOPPED_BY_HAND`] (the runner's code and words), its session ended
//!   and its workspace kept, and a stuck path cut short is finished from the ledger with the
//!   words it recorded. A closed issue is a person's move or the run's own merge and ends the
//!   run first, as the runner's `advance` does. While a feedback round is decided and not
//!   launched (`feedback::round_pending`) the session to end is the round's, in its workspace
//!   (none before it has one): the build's ended at the gate and its workspace is archived.
//! - [`reconcile`] is the runner's `watch` and what the spec says of a person's edit and of the
//!   end of a run: see below.
//!
//! What the pass (task 30) must do with them:
//!
//! - Call [`advance`] for a run in `building` (or a `stuck` one not finished) whose ledger has
//!   `stop_requested`, BEFORE `resume::advance`, `building::advance` and `start::advance`:
//!   all three skip such a run ("stop owns it"). It answers [`StopStep::GateHolds`] while a
//!   gate is in flight or a pass is recorded and not yet finished, and writes nothing: the
//!   run is then `gating::advance`'s, which sticks it itself (it must be called for such a
//!   run though `building` skips it), and the stop is dropped once the run is `pr-open`
//!   ([`StopStep::Skipped`] `not-building`), as the runner's `work_runs` and `prOpenRuns` do.
//!   A pass that finds [`StopStep::Stuck`] says "stopped by hand"; wake the loop after a
//!   [`request`] that answered `Recorded`.
//! - Call [`reconcile`] for every run of the project that is not `triage` or `needs-info`,
//!   before `building::advance` and after `resume::advance` (as the runner calls `watch`
//!   before `advance`): for a `building` run, a `pr-open` run (every pass: its pull request
//!   and issue are read), a run whose stuck path this step began, and a finished run, which
//!   costs no read of GitHub unless the fleet still shows a workspace of a stuck one. A stuck
//!   run's issue is read only while a workspace of it is kept, and the pass is to read it no
//!   more often than the runner's `STUCK_READ_EVERY_MS` (10 minutes): nothing here remembers
//!   the last read. A run [`ReconcileStep::Skipped`] or [`ReconcileStep::Unchanged`] is for
//!   its other steps; [`ReconcileStep::Stopped`], [`ReconcileStep::Ended`] and
//!   [`ReconcileStep::Abandoned`] end the pass for it (say so; no other step runs on that
//!   run this pass); [`ReconcileStep::Archived`] says what was archived;
//!   [`ReconcileStep::Waiting`] is a machine that did not answer, asked again next pass.
//!   `resume::orphans` stays the backstop for a machine heard from again later.
//!
//! What [`reconcile`] decides, in order:
//!
//! 1. A finished run: its workspaces are swept. One that did not go stuck has none to
//!    keep (a closed issue's abandon is not a kept stuck run); a stuck one's are kept while its
//!    issue is open and labelled `factory:stuck`, and archived by force once the issue is
//!    closed, gone, or no longer so labelled. The ledger of a finished run takes no line, so
//!    this is the one place a workspace is archived with nothing recorded first: the fleet no
//!    longer showing it says it is done, and "not a live workspace" counts as archived.
//! 2. A stuck path begun with this step's codes is finished with the words it recorded;
//!    another step's is its own (`Skipped`, `not-building`).
//! 3. A `pr-open` run: a pull request merged, closed or gone ends it (`end_run`, as merged or
//!    closed); an open one with a closed issue abandons it. Nothing else is judged: a person's
//!    work on a pull request is feedback.
//! 4. A `building` run, as the runner's `watch`: skipped when a relaunch or resume is decided,
//!    with no session, with a stuck decision, a stop, questions posted, the gate holding it or a
//!    pass recorded. Then the issue text since the approval: changed by someone who is not an
//!    instructor (`text-changed-during-build`), a history that cannot be judged
//!    (`edits-unverifiable`) or no approval on record (`approval-unconfirmed`, no read) stops
//!    the run, its session ended and its workspace kept; a run that reported a pull request
//!    whose merge closed the issue is skipped (`pr-merged`) for `advance` to end. Then the
//!    issue itself, one read: a person's close abandons the run, the run's own merge ends it,
//!    and otherwise the label and the assignee are judged (see below).
//!
//! The end of a run (a merged or closed pull request, a closed issue) winds its workspaces up
//! BEFORE the run is finished, because the ledger takes no line after: the session is ended
//! (`reconcile-session-ended`), then each workspace the fleet shows under a name of the run
//! (`fac/<N>-<attempt>`, `-r<k>`, `-f<k>`, `-g<k>`) is archived by force, `reconcile-archiving
//! <id>` recorded before the call and `reconcile-archived <id>` after, and only then `end_run`,
//! `abandon` or `end_own_merge` writes the summary line. A workspace already answered is not
//! asked again, and a pass cut short carries on. A machine the fleet shows unreachable for as
//! long as `resume::UNREACHABLE_FOR` (15 minutes; a gate's for `gate::MACHINE_GONE_AFTER`, 5)
//! is recorded left (`reconcile-left <id> its machine is gone`) with no call; one that does not
//! answer a call yet is waited for. A failure of a call is an error with the run not finished,
//! and the next pass carries on.
//!
//! Where it differs from the runner:
//!
//! - A removed label and an assignee. The runner at the pinned commit has no rule for a
//!   building run's label or assignee (it checks `factory:triage` and the assignee only for a
//!   triage run, in `start`). The spec says "A person who edits the title, removes the label or
//!   assigns someone stops the run at the next reconcile, as with the runner", and this build
//!   reads it so (the orchestrator's decision): a `building` run, not a `triage` one (`start`
//!   owns the claim's label and assignee), whose open issue no longer has `factory:building`
//!   ([`LABEL_REMOVED`], code `label-removed`), or has an assignee other than the project's
//!   login ([`ASSIGNEE_CHANGED`], code `assignee-changed`; nobody assigned is not a person's
//!   assigning), is stopped as a stop by hand is: stuck, its session ended, its workspace kept.
//!   The reasons are fixed words that quote nothing from the issue. Neither is judged while a
//!   transition of the factory's own is in flight (a stuck decision, questions begun or
//!   posted, an approval withdrawn, a stop, a relaunch or resume, a recorded pass, a gate in
//!   flight): the label moves before the ledger's `state_changed`, so those would read as a
//!   person's move.
//! - The session is ended and the workspace kept for a stop (the spec's stuck workspace); the
//!   runner archived the thread and removed the worktree, and wrote `worktree_removed`, which
//!   Foundry never does. A machine unreachable for 15 minutes is not asked to end its session
//!   (it ends its own, G10); one unreachable for less is an error and asked again.
//! - `watch` is not called while the gate holds the run, as the runner's `work_runs` does not.
//!   A closed issue and a merged pull request while a gate runs are `gating`'s.
//! - `watch` is one step with the closed-issue and label judgement here, and a `pr-open` run
//!   ends here: the runner's `regate` ended it, and `gating` leaves it to this module (its doc
//!   says "the feedback step's"; the feedback step reads comments, and this one the pull
//!   request). The runner's `feedback` also ended a run whose pull request was merged when
//!   feedback existed, which this covers without reading comments.
//! - A stop by hand is a ledger line written by the lease holder, not a file taken by the pass,
//!   and the state is checked before a stop already on record (the runner's `leaveStopRequest`
//!   order, not `requestStop`'s). A stale `stop_requested` line stays in a run's ledger after
//!   the gate passed: `progress::read_progress` never clears it. The feedback step does not look
//!   at it for a run in `pr-open` (the runner's does not either), so a round starts; once the
//!   round has returned the run to `building`, this step reads the line as a stop and the run
//!   goes stuck as stopped by hand, its session ended in the round's workspace (the runner has
//!   the same edge).
//! - The pull request a stopped session left (`recordLeftPullRequest`, `pr_left`) is not
//!   written (`building`'s doc says so too).
//!
//! Runner cases with no counterpart here:
//!
//! - `takeStopRequest`, `leaveStopRequest` and `stopRequestPath` (every case of the
//!   `takeStopRequest` group of `stop.test.ts`, and the cases of `leaveStopRequest` that are
//!   about a file): a directory of request files per run, a file left, a race on the file, a
//!   file that cannot be written and a file kept when the ledger cannot be read. `request`
//!   writes the ledger line itself. The cases of `leaveStopRequest` that are about the state
//!   are ported: `refuses a pr-open run with a stale stop line, as not building` under its
//!   name (`refuses_a_pr_open_run_with_a_stale_stop_line_as_not_building`), `is already-requested
//!   when a building run's ledger has the stop line, and leaves no file` as the second ask of
//!   `request_stop::records_a_stop_for_a_building_run_once` (there is no file to leave), and
//!   `refuses a finished run and a run with no state yet` as `writes_nothing_to_a_run_that_finished`
//!   and `writes_nothing_to_a_run_with_no_state_yet`.
//! - `control.ts`'s claims pause and resume, `showRun` and `parseId` (and `stopRun`'s
//!   `not-found` and `invalid` answers: a route has a `RunId`): settings and ledger reads, other
//!   tasks'.
//! - `watch refuses a run of another repo before touching anything`: ported as
//!   `a_run_of_another_repo::is_skipped_before_anything_is_read_or_touched`, a skip
//!   (`other-repo`) where the runner's `WrongRepoError` stood.
//! - `watch does not let a thread T3 no longer knows stop the stop`, and the cases of the
//!   thread's archive (T3 threads): the session is the fleet's, and a session the fleet does
//!   not show has nothing to end.
//! - `is recorded when the watch stops the run, from the branch the thread reports` and `in the
//!   watch, stops the run even when GitHub cannot list them` (`pr_left`, see above), and the
//!   cases of `building.test.ts` about the time limit, which `building` has.
//! - `does nothing for a run that is finished ...` is split: a finished run is swept
//!   (`does_nothing_for_a_run_that_is_finished_and_holds_no_workspace`).
//!
//! Runner cases ported with another outcome, because Foundry's behaviour changes it:
//!
//! - `is an error with nothing written when the archive fails and the next pass retries`: the
//!   call that fails is the end of the session (the workspace is kept, not archived).
//! - `watch skips a run whose own merged pull request closed the issue` skips as the runner's
//!   does, for `building::advance` to end; the end of a run found by the label check (a closed
//!   issue with the text unchanged) is this step's, and ends it.

use std::collections::BTreeSet;

use foundry_proto::factory::{RunEvent, RunId};
use foundry_proto::ids::{MachineId, WorkspaceId};
use serde_json::Value;

use super::feedback;
use super::gating::{self, GATE_REQUESTED};
use super::progress::{Seen, pending_relaunch, read_progress};
use super::resume::UNREACHABLE_FOR;
use super::{
    Closing, Progress, RunContext, RunError, Stop, StuckStop, abandon, closed_issue, end_own_merge,
    end_run, go_stuck, progress_from, recorded_reason, workspace_id,
};
use crate::factory::gate;
use crate::factory::github::TrackerError;
use crate::factory::ledger::Ledger;
use crate::factory::ledger::events::{Cause, CauseStep, Event, names};
use crate::factory::lifecycle::State;
use crate::factory::lifecycle::approval::TextCheck;
use crate::factory::messages::{
    CANNOT_CONFIRM, CANNOT_VERIFY_DURING_BUILD, CHANGED_DURING_BUILD, STOPPED_BY_HAND,
};
use crate::factory::worker::{Guard, WorkerError};

/// What [`request`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopOutcome {
    Recorded,
    AlreadyRequested,
    NotBuilding,
    Finished,
}

/// A stop asked for by hand, with the progress it was decided from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StopRequest {
    pub outcome: StopOutcome,
    pub seen: Seen,
}

/// Records a stop by hand for `run`, as the runner's `requestStop` does, with the progress the
/// decision was made from. The state is checked before a stop already on record (as the
/// runner's `leaveStopRequest` does), so a stale line of a run that left `building` is refused
/// as not building and never answered "already". The lease is checked only before the one
/// write.
pub fn request(ledger: &Ledger, guard: &dyn Guard, run: &RunId) -> Result<StopRequest, RunError> {
    let events = ledger.read(run)?;
    if events.is_empty() {
        return Err(RunError::Invalid(format!("run {run} has no ledger")));
    }
    let seen = read_progress(&events);
    let outcome = if seen.finished {
        StopOutcome::Finished
    } else if seen.state != Some(State::Building) {
        StopOutcome::NotBuilding
    } else if seen.stop_requested {
        StopOutcome::AlreadyRequested
    } else {
        guard.check()?;
        ledger.write(
            run,
            &Event::StopRequested {
                by: "operator".into(),
            },
            &Cause::new(CauseStep::Operator, "stop-requested"),
        )?;
        StopOutcome::Recorded
    };
    Ok(StopRequest { outcome, seen })
}

/// Why a stop by hand is refused for a run that is not building, in the words of the runner's
/// `stop-run.sh`: `seen` is the run's progress, and the tail says how the run ends instead.
pub fn not_building_message(id: &str, seen: &Seen) -> String {
    let why = if seen.finished {
        "it is finished".to_owned()
    } else {
        match seen.state {
            None => "it has no state yet".to_owned(),
            Some(state) => format!("its state is {}", state.name()),
        }
    };
    let tail = if seen.finished {
        ""
    } else {
        match seen.state {
            None | Some(State::Triage | State::NeedsInfo) => " To end it, close its issue.",
            Some(State::PrOpen) => " It ends when its pull request is merged or closed.",
            Some(_) => "",
        }
    };
    format!("run {id} is not building: {why}. Only a building run is stopped this way.{tail}")
}

/// What `control.ts`'s `stopRun` says of a stop it left.
pub fn requested_message(id: &str) -> String {
    format!("stop requested: {id}; the issue goes to factory:stuck on the next pass")
}

/// What `stopRun` says of a stop already on record.
pub fn already_requested_message(id: &str) -> String {
    format!("stop already requested: {id}; the issue goes to factory:stuck on the next pass")
}

/// What a route answers to a stop: `control.ts`'s `stopped` (with `already`) and `refused`. The
/// caller wakes the loop when the stop is new.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopAnswer {
    Stopped { already: bool, message: String },
    Refused { message: String },
}

impl StopRequest {
    /// The answer for run `id`, in the words of `control.ts`.
    pub fn answer(&self, id: &str) -> StopAnswer {
        match self.outcome {
            StopOutcome::Recorded => StopAnswer::Stopped {
                already: false,
                message: requested_message(id),
            },
            StopOutcome::AlreadyRequested => StopAnswer::Stopped {
                already: true,
                message: already_requested_message(id),
            },
            StopOutcome::NotBuilding | StopOutcome::Finished => StopAnswer::Refused {
                message: not_building_message(id, &self.seen),
            },
        }
    }
}

/// What one pass of [`advance`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopStep {
    /// The run went stuck as stopped by hand; `reason` is what the ledger records.
    Stuck { reason: String },
    /// A person closed the issue: the run is ended.
    Abandoned,
    /// The run's own merged pull request closed its issue: the run is ended.
    Ended,
    /// The gate holds the run, and its own step carries out the stop.
    GateHolds,
    /// Not this step's run: nothing was written.
    Skipped { why: &'static str },
}

/// The code of the stuck path of a stop by hand, the runner's.
const STOP_CODE: &str = "stop-requested";

/// One pass over a run a stop was asked for: see the module's words.
pub fn advance(ctx: &RunContext<'_>, run: &RunId) -> Result<StopStep, RunError> {
    if !run.repo().eq_ignore_ascii_case(ctx.repo()) {
        return Ok(StopStep::Skipped { why: "other-repo" });
    }
    let events = ctx.ledger.read(run)?;
    let seen = read_progress(&events);
    if seen.finished {
        return Ok(StopStep::Skipped { why: "finished" });
    }
    if !seen.stop_requested {
        return Ok(StopStep::Skipped {
            why: "no-stop-requested",
        });
    }
    // A line that is there for a run past `building` is stale: the gate passed, and the stop is
    // dropped. A stuck path begun is carried out, whoever began it: `building` skips this run.
    let running = matches!(seen.state, Some(State::Building | State::Stuck));
    if !running {
        return Ok(StopStep::Skipped {
            why: "not-building",
        });
    }
    // While a round is under way the session to end is the round's, in its workspace: the
    // build's ended at the gate and its workspace is archived.
    let round = feedback::stopping_seen(&events, &seen);
    let seen = round.unwrap_or(seen);
    let mut at = progress_from(run, CauseStep::Advance, &seen, State::Building);
    if begun(&seen) {
        forget_unreachable_session(ctx, run, &events, &mut at);
        let reason = finish_begun(ctx, run, &seen, &mut at)?;
        return Ok(StopStep::Stuck { reason });
    }
    if seen.state != Some(State::Building) {
        return Ok(StopStep::Skipped {
            why: "not-building",
        });
    }
    // The gate decides first, as the runner's `work_runs` leaves the run to it: its own step
    // sticks the run, and a stop asked for while it ran is dropped once it passed.
    if gating::gate_running(&events) || seen.pass.is_some() {
        return Ok(StopStep::GateHolds);
    }
    // A closed issue is a person's move, or the run's own merge, before the stop is carried out.
    let (closing, _) = closed_issue(ctx, run, &seen)?;
    let unreachable = session_unreachable(ctx, run, &events, &at);
    let bare;
    let session_seen = if unreachable {
        bare = without_session(&seen);
        &bare
    } else {
        &seen
    };
    match closing {
        Closing::PersonsMove => {
            abandon(ctx, run, session_seen, CauseStep::Advance)?;
            return Ok(StopStep::Abandoned);
        }
        Closing::OwnMerge(pull) => {
            end_own_merge(ctx, run, session_seen, CauseStep::Advance, &pull)?;
            return Ok(StopStep::Ended);
        }
        Closing::Open => {}
    }
    if unreachable {
        at.worktree = None;
    }
    go_stuck(
        ctx,
        run.issue(),
        &mut at,
        STOP_CODE,
        Stop::new(STOPPED_BY_HAND),
        None,
    )?;
    Ok(StopStep::Stuck {
        reason: STOPPED_BY_HAND.into(),
    })
}

/// A stuck decision is on record and the run is not finished.
fn begun(seen: &Seen) -> bool {
    seen.stuck.is_some() || seen.stuck_stop.is_some()
}

/// Carries out the stuck path the ledger shows begun, with the words it recorded, and answers
/// the reason it records. The session's text is not in the decision and is not quoted again.
fn finish_begun(
    ctx: &RunContext<'_>,
    run: &RunId,
    seen: &Seen,
    at: &mut Progress,
) -> Result<String, RunError> {
    let issue = run.issue();
    if let Some(raw) = &seen.stuck_stop {
        let decision: StuckStop = serde_json::from_str(raw)
            .map_err(|error| RunError::Invalid(format!("the stuck decision of {run}: {error}")))?;
        let mut stop = decision.stop;
        stop.detail = seen.stuck_detail.clone();
        let reason = recorded_reason(&stop);
        go_stuck(ctx, issue, at, &decision.code, stop, None)?;
        return Ok(reason);
    }
    let Some(reason) = &seen.stuck else {
        return Err(RunError::Invalid(format!("run {run} has no stuck path")));
    };
    go_stuck(
        ctx,
        issue,
        at,
        "stuck-resumed",
        Stop::new(reason.clone()),
        None,
    )?;
    Ok(reason.clone())
}

/// `seen` as of a run whose session cannot be asked to end: the steps that end one find none.
fn without_session(seen: &Seen) -> Seen {
    Seen {
        thread_id: None,
        launch_sent: false,
        ..seen.clone()
    }
}

/// Whether the machine of the workspace the run's session is in has been unreachable for as
/// long as its machine's workspaces are given up on: nothing is asked of it, and it ends its
/// own factory sessions when it is heard from again.
fn session_unreachable(
    ctx: &RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    at: &Progress,
) -> bool {
    let Some(id) = at
        .worktree
        .as_deref()
        .and_then(|worktree| workspace_id(worktree).ok())
    else {
        return false;
    };
    live_workspaces(ctx, run, events)
        .iter()
        .any(|live| live.id == id && live.gone(ctx))
}

/// Stops the stuck path asking a machine that is given up on to end the session.
fn forget_unreachable_session(
    ctx: &RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    at: &mut Progress,
) {
    if session_unreachable(ctx, run, events, at) {
        at.worktree = None;
    }
}

/// A workspace of a run that the fleet shows.
#[derive(Clone, Debug)]
pub(super) struct Live {
    pub id: WorkspaceId,
    pub machine: MachineId,
    /// A gate's workspace: its machine is given up on sooner.
    pub gate: bool,
}

impl Live {
    /// Whether its machine has been unreachable for as long as the factory waits for it.
    pub fn gone(&self, ctx: &RunContext<'_>) -> bool {
        let after = if self.gate {
            gate::MACHINE_GONE_AFTER
        } else {
            UNREACHABLE_FOR
        };
        ctx.view.unreachable_for(self.machine) >= after
    }
}

/// The text of a `step_changed` event.
fn step_text(event: &RunEvent) -> Option<&str> {
    if event.event != names::STEP_CHANGED {
        return None;
    }
    event.data.get("step").and_then(Value::as_str)
}

/// The highest number `field` holds in the `event` lines of `events`, 0 for none.
fn highest(events: &[RunEvent], event: &str, field: &str) -> u32 {
    events
        .iter()
        .filter(|line| line.event == event)
        .filter_map(|line| line.data.get(field).and_then(Value::as_u64))
        .max()
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0)
}

/// Every workspace the fleet shows under a name the run can have: its build workspace, a
/// resume's (`-r<k>`), a feedback round's (`-f<k>`) and a gate's (`-g<k>`), one past the
/// highest count its own ledger names, so a workspace being asked for is among them. A
/// workspace is listed once, in the order of the names.
pub(super) fn live_workspaces(ctx: &RunContext<'_>, run: &RunId, events: &[RunEvent]) -> Vec<Live> {
    let base = format!("fac/{}-{}", run.issue(), run.attempt());
    let gates = events
        .iter()
        .filter_map(step_text)
        .filter_map(|step| step.strip_prefix(GATE_REQUESTED))
        .filter_map(|rest| rest.split(' ').nth(1)?.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    let mut named = vec![(base.clone(), false)];
    named.extend(
        (1..=highest(events, "session_interrupted", "resumes").saturating_add(1))
            .map(|k| (format!("{base}-r{k}"), false)),
    );
    named.extend(
        (1..=highest(events, "feedback_relaunching", "n").saturating_add(1))
            .map(|k| (format!("{base}-f{k}"), false)),
    );
    named.extend((1..=gates.saturating_add(1)).map(|k| (gate::name(run, k), true)));
    let mut found: Vec<Live> = Vec::new();
    for (name, is_gate) in named {
        for record in ctx.view.workspaces_named(ctx.project.id, &name) {
            if found.iter().all(|live| live.id != record.body.id) {
                found.push(Live {
                    id: record.body.id,
                    machine: record.owner,
                    gate: is_gate,
                });
            }
        }
    }
    found
}

/// The public reason of a run a person took `factory:building` off.
pub const LABEL_REMOVED: &str = "a person removed factory:building from the issue";

/// The public reason of a run a person assigned someone else on.
pub const ASSIGNEE_CHANGED: &str = "a person assigned someone else to the issue";

/// What one pass of [`reconcile`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconcileStep {
    /// Nothing to do, nothing written.
    Unchanged,
    /// Not a run this step judges: nothing written.
    Skipped { why: &'static str },
    /// The run went stuck; `reason` is the public reason.
    Stopped { reason: String },
    /// The run's pull request was merged or closed, or its own merge closed its issue.
    Ended,
    /// A person closed the issue.
    Abandoned,
    /// Workspaces of a finished run were archived by force.
    Archived { workspaces: Vec<WorkspaceId> },
    /// A machine does not answer yet: nothing is finished, and the next pass asks again.
    Waiting { workspaces: Vec<WorkspaceId> },
}

/// The codes of the stuck paths this step decides, the runner's `watch` and its own.
const TEXT_CODE: &str = "text-changed-during-build";
const UNVERIFIED_CODE: &str = "edits-unverifiable";
const UNCONFIRMED_CODE: &str = "approval-unconfirmed";
const LABEL_CODE: &str = "label-removed";
const ASSIGNEE_CODE: &str = "assignee-changed";
/// The codes `feedback` also begins, for the same text checks, while it sets a round up.
const SHARED_CODES: [&str; 3] = [TEXT_CODE, UNVERIFIED_CODE, UNCONFIRMED_CODE];
const OWN_CODES: [&str; 5] = [
    TEXT_CODE,
    UNVERIFIED_CODE,
    UNCONFIRMED_CODE,
    LABEL_CODE,
    ASSIGNEE_CODE,
];

/// The step written once the run's session was ended for its end.
pub const SESSION_ENDED: &str = "reconcile-session-ended";
/// The step written before a workspace's archive is asked for: `reconcile-archiving <id>`.
pub const ARCHIVING: &str = "reconcile-archiving ";
/// The step written once it is answered: `reconcile-archived <id>`.
pub const ARCHIVED: &str = "reconcile-archived ";
/// The step written for a workspace not asked for: `reconcile-left <id> <why>`.
pub const LEFT: &str = "reconcile-left ";

/// What a person's move and the end of a run mean for it: see the module's words.
pub fn reconcile(ctx: &RunContext<'_>, run: &RunId) -> Result<ReconcileStep, RunError> {
    if !run.repo().eq_ignore_ascii_case(ctx.repo()) {
        return Ok(ReconcileStep::Skipped { why: "other-repo" });
    }
    let events = ctx.ledger.read(run)?;
    let seen = read_progress(&events);
    if seen.finished {
        return sweep(ctx, run, &events, &seen);
    }
    if begun(&seen) {
        // A stuck path begun is its owner's to finish: this step's own is finished here.
        let own = seen
            .stuck_stop
            .as_deref()
            .and_then(|raw| serde_json::from_str::<StuckStop>(raw).ok())
            .is_some_and(|decision| OWN_CODES.contains(&decision.code.as_str()));
        // While a feedback round is decided and not launched, the stuck path of the codes
        // `feedback` shares is its own: it finishes it from the round's workspace.
        let shared = seen
            .stuck_stop
            .as_deref()
            .and_then(|raw| serde_json::from_str::<StuckStop>(raw).ok())
            .is_some_and(|decision| SHARED_CODES.contains(&decision.code.as_str()));
        if own && shared && feedback::round_pending(&events) {
            return Ok(ReconcileStep::Skipped { why: "relaunching" });
        }
        if own && matches!(seen.state, Some(State::Building | State::Stuck)) {
            let mut at = progress_from(run, CauseStep::Watch, &seen, State::Building);
            forget_unreachable_session(ctx, run, &events, &mut at);
            let reason = finish_begun(ctx, run, &seen, &mut at)?;
            return Ok(ReconcileStep::Stopped { reason });
        }
        return Ok(ReconcileStep::Skipped {
            why: "not-building",
        });
    }
    match seen.state {
        Some(State::PrOpen) => {
            // A round decided and not launched ends in its own workspace, not the build's.
            let seen = feedback::stopping_seen(&events, &seen).unwrap_or(seen);
            pr_open(ctx, run, &events, &seen)
        }
        Some(State::Building) => building(ctx, run, &events, &seen),
        _ => Ok(ReconcileStep::Skipped {
            why: "not-building",
        }),
    }
}

/// What a `pr-open` run's pull request and issue say: a pull request that is merged, closed or
/// gone ends it, and so does a closed issue. The label, the assignee and the text are not
/// judged: the gate passed, and a person's work on the pull request is feedback.
fn pr_open(
    ctx: &RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    seen: &Seen,
) -> Result<ReconcileStep, RunError> {
    let Some(last) = &seen.last_pr else {
        return Ok(ReconcileStep::Skipped {
            why: "no-pull-request",
        });
    };
    let pull = ctx.tracker.pull_request(ctx.repo(), last.pr)?;
    if pull.as_ref().is_none_or(|pull| !pull.open) {
        if let Wound::Waiting(workspaces) = wind_up(ctx, run, events, seen, CauseStep::Watch)? {
            return Ok(ReconcileStep::Waiting { workspaces });
        }
        let bare = without_session(seen);
        let at = progress_from(run, CauseStep::Watch, &bare, State::PrOpen);
        end_run(ctx, &at, &last.url, pull.as_ref(), CauseStep::Watch)?;
        return Ok(ReconcileStep::Ended);
    }
    if ctx.tracker.issue(ctx.repo(), run.issue())?.closed {
        return abandoned(ctx, run, events, seen);
    }
    Ok(ReconcileStep::Unchanged)
}

/// The runner's `watch` for a building run, and what a person's label or assignee means.
fn building(
    ctx: &RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    seen: &Seen,
) -> Result<ReconcileStep, RunError> {
    let skipped = |why| Ok(ReconcileStep::Skipped { why });
    if seen
        .interruption
        .as_ref()
        .is_some_and(|interruption| interruption.relaunch || !interruption.sent)
        || pending_relaunch(seen).is_some()
        || feedback::round_pending(events)
    {
        return skipped("relaunching");
    }
    if seen.thread_id.is_none() {
        return skipped("no-session");
    }
    if seen.stop_requested || seen.questions_posted {
        return skipped("not-building");
    }
    // The gate has the run: nothing else is done for it, and its own step decides.
    if gating::gate_running(events) {
        return skipped("gate-running");
    }
    // The gate checks the text itself before it records a pass, and after one the session is done.
    if seen.pass.is_some() {
        return skipped("gate-passed");
    }
    if let Some((code, reason)) = text_stop(ctx, run.issue(), seen)? {
        // Only a run that has reported a pull request can have been closed by its own merge: no
        // read for any other. `advance` ends that run; a closed issue otherwise changes nothing
        // here.
        if !seen.opened.is_empty()
            && let (Closing::OwnMerge(_), _) = closed_issue(ctx, run, seen)?
        {
            return skipped("pr-merged");
        }
        return stuck(ctx, run, events, seen, code, reason);
    }
    let (closing, detail) = closed_issue(ctx, run, seen)?;
    match closing {
        Closing::PersonsMove => return abandoned(ctx, run, events, seen),
        Closing::OwnMerge(pull) => {
            if let Wound::Waiting(workspaces) = wind_up(ctx, run, events, seen, CauseStep::Watch)? {
                return Ok(ReconcileStep::Waiting { workspaces });
            }
            end_own_merge(ctx, run, &without_session(seen), CauseStep::Watch, &pull)?;
            return Ok(ReconcileStep::Ended);
        }
        Closing::Open => {}
    }
    let Some(detail) = detail else {
        return Err(RunError::Invalid(format!("run {run}'s issue was not read")));
    };
    // A move of the factory's own, begun and not finished, is not a person's.
    if !committed(seen) {
        return Ok(ReconcileStep::Unchanged);
    }
    if !detail
        .labels
        .iter()
        .any(|label| label == State::Building.label())
    {
        return stuck(ctx, run, events, seen, LABEL_CODE, LABEL_REMOVED);
    }
    if detail
        .assignees
        .iter()
        .any(|assignee| !assignee.eq_ignore_ascii_case(&ctx.project.login))
    {
        return stuck(ctx, run, events, seen, ASSIGNEE_CODE, ASSIGNEE_CHANGED);
    }
    Ok(ReconcileStep::Unchanged)
}

/// Whether no transition of the factory's own is in flight for a building run: no questions
/// begun, no approval being withdrawn, no re-gate, no relaunch decided and not launched (one
/// whose session is recorded is over: the run is building and a person's move counts again),
/// and no stuck comment begun.
fn committed(seen: &Seen) -> bool {
    seen.questions_commenting.is_none()
        && !seen.withdrawn
        && !seen.regating
        && pending_relaunch(seen).is_none()
        && seen.stuck_commenting.is_none()
}

/// Why the issue text no longer stands for a building run, as the code and the public reason, or
/// `None`. A run with no `approved` step fails closed without a read; otherwise every rename and
/// body edit since the approval must be an instructor's. A history that cannot be judged counts
/// as changed; a failed read is an error, never an answer.
fn text_stop(
    ctx: &RunContext<'_>,
    issue: u64,
    seen: &Seen,
) -> Result<Option<(&'static str, &'static str)>, RunError> {
    let Some(approved_at) = &seen.approved_at else {
        return Ok(Some((UNCONFIRMED_CODE, CANNOT_CONFIRM)));
    };
    let check = ctx
        .tracker
        .text_check(ctx.repo(), issue, approved_at, &ctx.config.instructors)?;
    Ok(match check {
        TextCheck::Unchanged => None,
        TextCheck::Changed => Some((TEXT_CODE, CHANGED_DURING_BUILD)),
        TextCheck::Unjudged => Some((UNVERIFIED_CODE, CANNOT_VERIFY_DURING_BUILD)),
    })
}

/// The run goes stuck with `reason`, its session ended and its workspace kept.
fn stuck(
    ctx: &RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    seen: &Seen,
    code: &str,
    reason: &str,
) -> Result<ReconcileStep, RunError> {
    let mut at = progress_from(run, CauseStep::Watch, seen, State::Building);
    forget_unreachable_session(ctx, run, events, &mut at);
    go_stuck(ctx, run.issue(), &mut at, code, Stop::new(reason), None)?;
    Ok(ReconcileStep::Stopped {
        reason: reason.into(),
    })
}

/// A person closed the issue: the run's workspaces are wound up, then the run is ended.
fn abandoned(
    ctx: &RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    seen: &Seen,
) -> Result<ReconcileStep, RunError> {
    if let Wound::Waiting(workspaces) = wind_up(ctx, run, events, seen, CauseStep::Watch)? {
        return Ok(ReconcileStep::Waiting { workspaces });
    }
    abandon(ctx, run, &without_session(seen), CauseStep::Watch)?;
    Ok(ReconcileStep::Abandoned)
}

/// What winding up the workspaces of a run that ends came to.
enum Wound {
    /// Every workspace is archived or left, and the session is ended.
    Done,
    /// A machine did not answer: asked again next pass, nothing finished.
    Waiting(Vec<WorkspaceId>),
}

/// What the ledger records of a run's winding up.
#[derive(Default)]
struct Recorded {
    session_ended: bool,
    /// The workspaces whose archive was about to be asked for.
    archiving: BTreeSet<String>,
    /// The workspaces archived or left.
    done: BTreeSet<String>,
}

impl Recorded {
    fn read(events: &[RunEvent]) -> Self {
        let mut recorded = Self::default();
        for step in events.iter().filter_map(step_text) {
            if step == SESSION_ENDED {
                recorded.session_ended = true;
            } else if let Some(id) = step.strip_prefix(ARCHIVING) {
                recorded.archiving.insert(id.to_owned());
            } else if let Some(id) = step.strip_prefix(ARCHIVED) {
                recorded.done.insert(id.to_owned());
            } else if let Some(rest) = step.strip_prefix(LEFT) {
                recorded
                    .done
                    .insert(rest.split(' ').next().unwrap_or(rest).to_owned());
            }
        }
        recorded
    }
}

/// The run's session ended, then every workspace of it the fleet shows archived by force, each
/// recorded before it is asked and each once. A machine unreachable for as long as its
/// workspaces are kept for is left with no call; one that does not answer yet is waited for.
/// A failed call is an error, and the next pass carries on from the ledger.
fn wind_up(
    ctx: &RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    seen: &Seen,
    step: CauseStep,
) -> Result<Wound, RunError> {
    ctx.held()?;
    let live = live_workspaces(ctx, run, events);
    let recorded = Recorded::read(events);
    if !recorded.session_ended {
        let at = progress_from(run, step, seen, State::Triage);
        let session = at.thread_id.is_some() || seen.launch_sent;
        let in_it = at
            .worktree
            .as_deref()
            .and_then(|worktree| workspace_id(worktree).ok())
            .and_then(|id| live.iter().find(|live| live.id == id));
        if let (true, Some(live)) = (session, in_it)
            && !live.gone(ctx)
        {
            match ctx.end_sessions(live.id) {
                Ok(()) => {}
                Err(RunError::Worker(WorkerError::Failed(said))) if not_live(&said) => {}
                Err(RunError::Worker(WorkerError::Unreachable(_))) => {
                    return Ok(Wound::Waiting(vec![live.id]));
                }
                Err(error) => return Err(error),
            }
        }
        ctx.record_step(
            run,
            SESSION_ENDED,
            &Cause::new(step, "reconcile-session-ended"),
        )?;
    }
    let mut waiting = Vec::new();
    for workspace in &live {
        if archive_one(ctx, run, workspace, Some((&recorded, step)))? == Archived::Waiting {
            waiting.push(workspace.id);
        }
    }
    Ok(if waiting.is_empty() {
        Wound::Done
    } else {
        Wound::Waiting(waiting)
    })
}

/// What asking for a workspace's archive came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Archived {
    Done,
    Left,
    Waiting,
}

/// Whether the worker's answer to an archive is P1's "is not a live workspace of this machine".
fn not_live(said: &str) -> bool {
    said.contains("is not a live workspace of this machine")
}

/// One workspace archived by force. With `log` (a run not finished) the intent is recorded
/// before the call and the answer after, and one already answered is not asked again; without
/// it (a finished run's ledger takes no line) the call is asked while the fleet shows the
/// workspace, and a workspace the worker no longer knows counts as archived.
fn archive_one(
    ctx: &RunContext<'_>,
    run: &RunId,
    live: &Live,
    log: Option<(&Recorded, CauseStep)>,
) -> Result<Archived, RunError> {
    let id = live.id.0.to_string();
    if log.is_some_and(|(recorded, _)| recorded.done.contains(&id)) {
        return Ok(Archived::Done);
    }
    if live.gone(ctx) {
        if let Some((_, step)) = log {
            let line = format!("{LEFT}{id} its machine is gone");
            ctx.record_step(run, &line, &Cause::new(step, "reconcile-left"))?;
        }
        return Ok(Archived::Left);
    }
    if let Some((recorded, step)) = log
        && !recorded.archiving.contains(&id)
    {
        let line = format!("{ARCHIVING}{id}");
        ctx.record_step(run, &line, &Cause::new(step, "reconcile-archiving"))?;
    }
    match ctx.archive(live.id) {
        Ok(()) => {}
        Err(RunError::Worker(WorkerError::Failed(said))) if not_live(&said) => {}
        Err(RunError::Worker(WorkerError::Unreachable(_))) => return Ok(Archived::Waiting),
        Err(error) => return Err(error),
    }
    if let Some((_, step)) = log {
        let line = format!("{ARCHIVED}{id}");
        ctx.record_step(run, &line, &Cause::new(step, "reconcile-archived"))?;
    }
    Ok(Archived::Done)
}

/// The workspaces of a run that is finished: a run that did not go stuck has none to keep, and
/// a stuck run's are kept until its issue leaves `factory:stuck`, or is closed or gone. The
/// ledger of a finished run takes no line, so nothing is recorded: the fleet no longer showing
/// a workspace is what says it was archived, and a call asked twice before it does is answered
/// "not a live workspace".
fn sweep(
    ctx: &RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    seen: &Seen,
) -> Result<ReconcileStep, RunError> {
    let live = live_workspaces(ctx, run, events);
    if live.is_empty() {
        return Ok(ReconcileStep::Unchanged);
    }
    if seen.state == Some(State::Stuck) || begun(seen) {
        match ctx.tracker.issue(ctx.repo(), run.issue()) {
            Ok(detail)
                if !detail.closed
                    && detail
                        .labels
                        .iter()
                        .any(|label| label == State::Stuck.label()) =>
            {
                return Ok(ReconcileStep::Unchanged);
            }
            Ok(_) | Err(TrackerError::NotFound { .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let (mut archived, mut waiting) = (Vec::new(), Vec::new());
    for workspace in &live {
        match archive_one(ctx, run, workspace, None)? {
            Archived::Done => archived.push(workspace.id),
            Archived::Left => {}
            Archived::Waiting => waiting.push(workspace.id),
        }
    }
    Ok(if !waiting.is_empty() {
        ReconcileStep::Waiting {
            workspaces: waiting,
        }
    } else if archived.is_empty() {
        ReconcileStep::Unchanged
    } else {
        ReconcileStep::Archived {
            workspaces: archived,
        }
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use foundry_proto::ids::{MachineId, WorkspaceId};
    use foundry_proto::record::{Record, Workspace};

    use super::*;
    use crate::factory::ledger::events::{Cause, CauseStep, Event, Outcome};
    use crate::factory::lifecycle::State;
    use crate::factory::messages::STOPPED_BY_HAND;
    use crate::factory::runs::testing::*;
    use crate::factory::worker::{Call, FleetView, Lost};

    const SESSION: &str = "00000000-0000-0000-0000-0000000005e5";
    const WS: &str = "00000000-0000-0000-0000-000000000a11";
    const BRANCH: &str = "fac/8-1";

    fn ws() -> WorkspaceId {
        WorkspaceId(uuid::Uuid::from_u128(0xa11))
    }

    fn workspace_record(owner: MachineId, id: WorkspaceId, name: &str) -> Record<Workspace> {
        Record {
            owner,
            version: 1,
            deleted: false,
            body: Workspace {
                id,
                project: project_id(),
                name: name.into(),
                branch: name.into(),
                base: "main".into(),
                setup: None,
            },
        }
    }

    /// The fleet with the build machine online and the build workspace on it.
    fn fleet(rig: &Rig) -> FleetView {
        FleetView::builder()
            .online(rig.machine)
            .workspace(workspace_record(rig.machine, ws(), BRANCH))
            .build()
    }

    /// The build machine has been unreachable for `span`, its workspace as it left it.
    fn cut_off(rig: &Rig, span: Duration) -> FleetView {
        FleetView::builder()
            .workspace(workspace_record(rig.machine, ws(), BRANCH))
            .unreachable(rig.machine, span)
            .build()
    }

    /// A run as `start` leaves it building: approved, claimed, launched, labelled and assigned.
    fn built(rig: &mut Rig, run: &RunId) {
        rig.step(run, &format!("approved {LABELED_AT}"));
        rig.write(
            run,
            Event::Claimed {
                machine: rig.machine.to_string(),
                github_user: LOGIN.into(),
            },
        );
        rig.moved(run, State::Ready, State::Triage);
        rig.step(run, &format!("workspace-created {WS}"));
        rig.step(run, "approval-rechecked");
        rig.write(
            run,
            Event::SessionLaunched {
                thread_id: SESSION.into(),
                worktree: Some(WS.into()),
                branch: Some(BRANCH.into()),
                model_instance: None,
                model: Some("opus".into()),
                model_options: None,
                resumed_from: None,
                base: Some("main".into()),
                title: Some("Make it faster".into()),
            },
        );
        rig.moved(run, State::Triage, State::Building);
        rig.sim.set_labels(&["factory:building"]);
        rig.sim.set_assignees(&[LOGIN]);
        rig.view = fleet(rig);
    }

    fn calls(rig: &Rig) -> Vec<Call> {
        rig.worker.calls()
    }

    fn stuck_comments(rig: &Rig) -> usize {
        rig.sim
            .comments()
            .iter()
            .filter(|body| body.contains("could not finish this issue"))
            .count()
    }

    fn seed(rig: &Rig, run: &RunId, events: &[Event]) {
        for event in events {
            rig.write(run, event.clone());
        }
    }

    fn claimed() -> Event {
        Event::Claimed {
            machine: "mac-mini".into(),
            github_user: LOGIN.into(),
        }
    }

    fn moved(from: State, to: State) -> Event {
        Event::StateChanged { from, to }
    }

    fn building() -> Vec<Event> {
        vec![
            claimed(),
            moved(State::Ready, State::Triage),
            moved(State::Triage, State::Building),
        ]
    }

    fn finish(rig: &Rig, run: &RunId) {
        rig.ledger
            .finish(
                run,
                Outcome::Stuck,
                None,
                None,
                &Cause::new(CauseStep::Start, "seed"),
                None,
            )
            .unwrap();
    }

    fn stop_line() -> Event {
        Event::StopRequested {
            by: "operator".into(),
        }
    }

    fn ask(rig: &Rig, run: &RunId) -> StopRequest {
        request(&rig.ledger, rig.guard.as_ref(), run).unwrap()
    }

    fn event_count(rig: &Rig, run: &RunId) -> usize {
        rig.events(run).len()
    }

    // ----- the runner's `requestStop` -----

    mod request_stop {
        use super::*;

        #[test]
        fn records_a_stop_for_a_building_run_once() {
            let rig = Rig::new();
            let run = run_id(1);
            seed(&rig, &run, &building());
            assert_eq!(ask(&rig, &run).outcome, StopOutcome::Recorded);
            let last = rig.events(&run).pop().unwrap();
            assert_eq!(last.event, "stop_requested");
            assert_eq!(last.data, serde_json::json!({"by": "operator"}));
            assert_eq!(
                last.cause,
                Some(serde_json::json!({"step": "operator", "reason": "stop-requested"}))
            );
            let before = event_count(&rig, &run);
            assert_eq!(ask(&rig, &run).outcome, StopOutcome::AlreadyRequested);
            assert_eq!(event_count(&rig, &run), before);
        }

        #[test]
        fn writes_nothing_to_a_run_that_finished() {
            let rig = Rig::new();
            let run = run_id(1);
            seed(&rig, &run, &building());
            finish(&rig, &run);
            let before = event_count(&rig, &run);
            assert_eq!(ask(&rig, &run).outcome, StopOutcome::Finished);
            assert_eq!(event_count(&rig, &run), before);
        }

        #[test]
        fn writes_nothing_to_a_run_in_triage_needs_info_or_pr_open() {
            for state in [State::Triage, State::NeedsInfo, State::PrOpen] {
                let rig = Rig::new();
                let run = run_id(1);
                seed(&rig, &run, &[claimed(), moved(State::Ready, state)]);
                let before = event_count(&rig, &run);
                assert_eq!(ask(&rig, &run).outcome, StopOutcome::NotBuilding, "{state}");
                assert_eq!(event_count(&rig, &run), before, "{state}");
            }
        }

        #[test]
        fn writes_nothing_to_a_run_with_no_state_yet() {
            let rig = Rig::new();
            let run = run_id(1);
            seed(&rig, &run, &[claimed()]);
            assert_eq!(ask(&rig, &run).outcome, StopOutcome::NotBuilding);
            assert_eq!(event_count(&rig, &run), 1);
        }

        #[test]
        fn fails_for_a_run_with_no_ledger() {
            let rig = Rig::new();
            let error = request(&rig.ledger, rig.guard.as_ref(), &run_id(1)).unwrap_err();
            assert!(matches!(error, RunError::Invalid(_)), "{error}");
        }

        #[test]
        fn returns_the_progress_it_read_so_a_refusal_needs_no_second_read() {
            let rig = Rig::new();
            let run = run_id(1);
            let mut events = building();
            events.push(moved(State::Building, State::PrOpen));
            seed(&rig, &run, &events);
            let asked = ask(&rig, &run);
            assert_eq!(asked.outcome, StopOutcome::NotBuilding);
            assert_eq!(asked.seen.state, Some(State::PrOpen));
            assert!(not_building_message("id", &asked.seen).contains("its state is pr-open"));
        }

        // The runner's `leaveStopRequest` and `control.ts`'s `stopRun` check the state before
        // a stop already on record, so a stale line never reads as "already".
        #[test]
        fn refuses_a_pr_open_run_with_a_stale_stop_line_as_not_building() {
            let rig = Rig::new();
            let run = run_id(1);
            let mut events = building();
            events.push(stop_line());
            events.push(moved(State::Building, State::PrOpen));
            seed(&rig, &run, &events);
            let asked = ask(&rig, &run);
            assert_eq!(asked.outcome, StopOutcome::NotBuilding);
            assert!(
                matches!(asked.answer("ID"), StopAnswer::Refused { message } if message.contains("its state is pr-open"))
            );
        }

        #[test]
        fn is_not_written_when_the_lease_is_lost() {
            let mut rig = Rig::new();
            let run = run_id(1);
            seed(&rig, &run, &building());
            rig.guard = Box::new(Lost);
            let before = event_count(&rig, &run);
            let error = request(&rig.ledger, rig.guard.as_ref(), &run).unwrap_err();
            assert!(matches!(error, RunError::LeaseLost(_)), "{error}");
            assert_eq!(event_count(&rig, &run), before);
            // A refusal needs no lease: nothing is written for it.
            let refused = Rig::new();
            seed(&refused, &run, &[claimed()]);
            assert_eq!(
                request(&refused.ledger, &Lost, &run).unwrap().outcome,
                StopOutcome::NotBuilding
            );
        }
    }

    // ----- the runner's `notBuildingMessage` -----

    mod not_building_message {
        use super::*;

        fn message_for(events: &[Event]) -> String {
            let rig = Rig::new();
            let run = run_id(1);
            seed(&rig, &run, events);
            not_building_message("ID", &rig.ctx().seen(&run).unwrap())
        }

        fn in_state(state: State) -> Vec<Event> {
            vec![claimed(), moved(State::Ready, state)]
        }

        #[test]
        fn says_a_finished_run_is_finished_with_no_way_out() {
            let rig = Rig::new();
            let run = run_id(1);
            seed(&rig, &run, &building());
            finish(&rig, &run);
            assert_eq!(
                not_building_message("ID", &rig.ctx().seen(&run).unwrap()),
                "run ID is not building: it is finished. Only a building run is stopped this way."
            );
        }

        #[test]
        fn says_how_a_run_in_another_state_ends_instead() {
            let base = "run ID is not building: its state is";
            let tail = "Only a building run is stopped this way.";
            assert_eq!(
                message_for(&in_state(State::PrOpen)),
                format!(
                    "{base} pr-open. {tail} It ends when its pull request is merged or closed."
                )
            );
            assert_eq!(
                message_for(&in_state(State::Triage)),
                format!("{base} triage. {tail} To end it, close its issue.")
            );
            assert_eq!(
                message_for(&in_state(State::NeedsInfo)),
                format!("{base} needs-info. {tail} To end it, close its issue.")
            );
            assert_eq!(
                message_for(&in_state(State::Stuck)),
                format!("{base} stuck. {tail}")
            );
        }

        #[test]
        fn says_a_run_with_no_state_has_none_yet() {
            assert_eq!(
                message_for(&[claimed()]),
                "run ID is not building: it has no state yet. Only a building run is stopped this way. To end it, close its issue."
            );
        }
    }

    // ----- `control.ts`'s `stopRun` words -----

    mod stop_run {
        use super::*;

        #[test]
        fn leaves_a_stop_for_a_building_run_once_and_says_so() {
            let rig = Rig::new();
            let run = run_id(1);
            seed(&rig, &run, &building());
            assert_eq!(
                ask(&rig, &run).answer("ID"),
                StopAnswer::Stopped {
                    already: false,
                    message: "stop requested: ID; the issue goes to factory:stuck on the next pass"
                        .into(),
                }
            );
            assert_eq!(
                ask(&rig, &run).answer("ID"),
                StopAnswer::Stopped {
                    already: true,
                    message:
                        "stop already requested: ID; the issue goes to factory:stuck on the next pass"
                            .into(),
                }
            );
            assert_eq!(
                requested_message("X"),
                "stop requested: X; the issue goes to factory:stuck on the next pass"
            );
            assert_eq!(
                already_requested_message("X"),
                "stop already requested: X; the issue goes to factory:stuck on the next pass"
            );
        }

        #[test]
        fn refuses_a_run_waiting_on_its_pull_request_and_says_how_it_ends() {
            let rig = Rig::new();
            let run = run_id(1);
            let mut events = building();
            events.push(moved(State::Building, State::PrOpen));
            seed(&rig, &run, &events);
            let before = event_count(&rig, &run);
            assert_eq!(
                ask(&rig, &run).answer("ID"),
                StopAnswer::Refused {
                    message: "run ID is not building: its state is pr-open. Only a building run is stopped this way. It ends when its pull request is merged or closed.".into(),
                }
            );
            assert_eq!(event_count(&rig, &run), before);
        }

        #[test]
        fn refuses_a_finished_run_and_a_run_in_triage_with_the_way_to_end_it() {
            let rig = Rig::new();
            let run = run_id(1);
            seed(&rig, &run, &building());
            finish(&rig, &run);
            assert_eq!(
                ask(&rig, &run).answer("ID"),
                StopAnswer::Refused {
                    message: "run ID is not building: it is finished. Only a building run is stopped this way.".into(),
                }
            );
            let triage = Rig::new();
            seed(
                &triage,
                &run,
                &[claimed(), moved(State::Ready, State::Triage)],
            );
            assert!(matches!(
                ask(&triage, &run).answer("ID"),
                StopAnswer::Refused { message } if message.contains("To end it, close its issue.")
            ));
        }
    }

    // ----- a requested stop is stuck on the next pass -----

    mod advance_a_requested_stop {
        use super::*;

        fn pass(rig: &Rig, run: &RunId) -> StopStep {
            advance(&rig.ctx(), run).unwrap()
        }

        fn requested() -> (Rig, RunId) {
            let mut rig = Rig::new();
            let run = run_id(1);
            built(&mut rig, &run);
            rig.write(&run, stop_line());
            (rig, run)
        }

        #[test]
        fn is_stuck_on_the_next_pass_with_its_session_ended_and_its_workspace_kept() {
            let (rig, run) = requested();
            assert_eq!(
                pass(&rig, &run),
                StopStep::Stuck {
                    reason: STOPPED_BY_HAND.into()
                }
            );
            assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
            assert_eq!(rig.sim.assignees(), Vec::<String>::new());
            assert_eq!(stuck_comments(&rig), 1);
            assert!(
                rig.sim
                    .comments()
                    .iter()
                    .any(|c| c.contains(STOPPED_BY_HAND))
            );
            assert_eq!(calls(&rig), vec![Call::EndSessions { workspace: ws() }]);
            assert!(rig.finished(&run));
            let causes: Vec<_> = rig
                .events(&run)
                .into_iter()
                .skip_while(|e| e.event != "stop_requested")
                .filter(|e| ["stuck", "state_changed", "finished"].contains(&e.event.as_str()))
                .map(|e| e.cause.unwrap())
                .collect();
            assert_eq!(causes.len(), 3);
            assert!(causes.iter().all(|cause| cause
                == &serde_json::json!({"step": "advance", "reason": "stop-requested"})));
        }

        const ROUND_WS: &str = "00000000-0000-0000-0000-000000000f11";

        fn round_ws() -> WorkspaceId {
            WorkspaceId(uuid::Uuid::from_u128(0xf11))
        }

        /// A run the gate passed and a feedback round took back to `building`: decided and not
        /// launched, the build's workspace archived and a stop asked for. `steps` are what the
        /// round recorded of its set-up.
        fn round_being_set_up(steps: &[String]) -> (Rig, RunId) {
            let (mut rig, run) = (Rig::new(), run_id(1));
            built(&mut rig, &run);
            rig.write(
                &run,
                Event::PrOpened {
                    pr: 12,
                    url: "https://github.com/rawphp/softfac/pull/12".into(),
                },
            );
            rig.moved(&run, State::Building, State::PrOpen);
            let decision = Event::FeedbackRelaunching {
                n: 1,
                branch: BRANCH.into(),
                items: vec!["review:11".into()],
                message: "Please rename it.".into(),
            };
            rig.write(&run, decision.clone());
            rig.moved(&run, State::PrOpen, State::Building);
            rig.write(&run, decision);
            for step in steps {
                rig.step(&run, step);
            }
            rig.write(&run, stop_line());
            rig.sim.set_labels(&["factory:building"]);
            rig.sim.set_assignees(&[LOGIN]);
            // The build's workspace is archived; the round's shows once it was made.
            let mut view = FleetView::builder().online(rig.machine);
            if steps
                .iter()
                .any(|step| step.starts_with("feedback-workspace"))
            {
                view = view.workspace(workspace_record(rig.machine, round_ws(), "fac/8-1-f1"));
            }
            rig.view = view.build();
            (rig, run)
        }

        #[test]
        fn during_a_rounds_set_up_ends_the_sessions_of_the_rounds_workspace_not_the_old_one() {
            let (rig, run) = round_being_set_up(&[
                "feedback-requested 1 fac/8-1-f1".into(),
                format!("feedback-workspace 1 {ROUND_WS}"),
                "feedback-sending 1".into(),
            ]);
            assert_eq!(
                pass(&rig, &run),
                StopStep::Stuck {
                    reason: STOPPED_BY_HAND.into()
                }
            );
            assert_eq!(
                calls(&rig),
                vec![Call::EndSessions {
                    workspace: round_ws()
                }]
            );
            assert!(rig.finished(&run));
            assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
        }

        #[test]
        fn before_a_round_has_a_workspace_ends_nothing_and_still_goes_stuck() {
            let (rig, run) = round_being_set_up(&["feedback-requested 1 fac/8-1-f1".into()]);
            assert_eq!(
                pass(&rig, &run),
                StopStep::Stuck {
                    reason: STOPPED_BY_HAND.into()
                }
            );
            assert!(calls(&rig).is_empty(), "{:?}", calls(&rig));
            assert!(rig.finished(&run));
            assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
        }

        #[test]
        fn is_never_stuck_twice() {
            let (rig, run) = requested();
            pass(&rig, &run);
            let events = rig.events(&run).len();
            let writes = rig.sim.writes().len();
            assert_eq!(pass(&rig, &run), StopStep::Skipped { why: "finished" });
            assert_eq!(rig.events(&run).len(), events);
            assert_eq!(rig.sim.writes().len(), writes);
            assert_eq!(calls(&rig).len(), 1);
        }

        #[test]
        fn cut_short_is_finished_once_from_the_ledger() {
            let (rig, run) = requested();
            // The stuck comment fails: the decision is recorded, nothing finished.
            rig.sim.fail("could not finish this issue", 1);
            assert!(advance(&rig.ctx(), &run).is_err());
            assert!(
                rig.steps(&run)
                    .iter()
                    .any(|step| step.starts_with("stuck-stop"))
            );
            assert!(!rig.finished(&run));
            assert_eq!(
                pass(&rig, &run),
                StopStep::Stuck {
                    reason: STOPPED_BY_HAND.into()
                }
            );
            assert!(rig.finished(&run));
            assert_eq!(stuck_comments(&rig), 1);
            assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
            // And a third pass has nothing left.
            assert_eq!(pass(&rig, &run), StopStep::Skipped { why: "finished" });
            assert_eq!(stuck_comments(&rig), 1);
        }

        #[test]
        fn finishes_the_stuck_decision_another_step_began_with_its_words() {
            let (rig, run) = requested();
            rig.step(
                &run,
                "stuck-stop {\"code\":\"session-stuck\",\"stop\":{\"reason\":\"the session said it was stuck\"}}",
            );
            assert_eq!(
                pass(&rig, &run),
                StopStep::Stuck {
                    reason: "the session said it was stuck".into()
                }
            );
            assert!(rig.finished(&run));
            assert_eq!(stuck_comments(&rig), 1);
        }

        #[test]
        fn is_left_to_gating_while_the_gate_holds_the_run() {
            let (rig, run) = requested();
            rig.step(
                &run,
                &format!("gate-requested 1 1 {SHA} {SHA} {}", rig.machine),
            );
            let events = rig.events(&run).len();
            assert_eq!(pass(&rig, &run), StopStep::GateHolds);
            assert_eq!(rig.events(&run).len(), events);
            assert!(calls(&rig).is_empty());
            assert!(rig.sim.writes().is_empty());
        }

        #[test]
        fn is_left_to_gating_while_a_recorded_pass_is_finished() {
            let (rig, run) = requested();
            rig.ledger
                .write(
                    &run,
                    &Event::StepChanged {
                        step: format!("gate-passed {SHA} 1"),
                    },
                    &Cause::new(CauseStep::Gate, "gate-passed").attempt(1),
                )
                .unwrap();
            assert_eq!(pass(&rig, &run), StopStep::GateHolds);
            assert!(calls(&rig).is_empty());
            assert!(!rig.finished(&run));
        }

        #[test]
        fn is_dropped_once_the_gate_passed_and_the_run_is_in_pr_open() {
            let (rig, run) = requested();
            rig.moved(&run, State::Building, State::PrOpen);
            let events = rig.events(&run).len();
            assert_eq!(
                pass(&rig, &run),
                StopStep::Skipped {
                    why: "not-building"
                }
            );
            assert_eq!(rig.events(&run).len(), events);
            assert!(calls(&rig).is_empty());
            assert!(rig.sim.writes().is_empty());
        }

        #[test]
        fn skips_a_run_nobody_asked_to_stop() {
            let mut rig = Rig::new();
            let run = run_id(1);
            built(&mut rig, &run);
            assert_eq!(
                pass(&rig, &run),
                StopStep::Skipped {
                    why: "no-stop-requested"
                }
            );
            assert!(calls(&rig).is_empty());
        }

        #[test]
        fn wins_over_an_interruption_owed_to_the_run() {
            let (rig, run) = requested();
            rig.write(
                &run,
                Event::SessionInterrupted {
                    resumes: 1,
                    relaunch: false,
                    run_ordinal: None,
                    branch: Some(BRANCH.into()),
                    t3_error: None,
                    ended_in_pass: 1,
                },
            );
            assert_eq!(
                pass(&rig, &run),
                StopStep::Stuck {
                    reason: STOPPED_BY_HAND.into()
                }
            );
            assert!(rig.finished(&run));
        }

        #[test]
        fn a_person_closing_the_issue_abandons_the_run_before_the_stop() {
            let (rig, run) = requested();
            rig.sim.set_closed(true);
            assert_eq!(pass(&rig, &run), StopStep::Abandoned);
            assert!(rig.finished(&run));
            assert!(rig.sim.writes().is_empty());
            assert_eq!(calls(&rig), vec![Call::EndSessions { workspace: ws() }]);
        }

        #[test]
        fn the_runs_own_merged_pull_request_that_closed_the_issue_ends_it() {
            let (rig, run) = requested();
            rig.write(
                &run,
                Event::PrOpened {
                    pr: 12,
                    url: "https://github.com/rawphp/softfac/pull/12".into(),
                },
            );
            rig.sim.set_closed(true);
            rig.sim.set_pull(
                12,
                SimPull {
                    state: "MERGED",
                    draft: false,
                    head: BRANCH.into(),
                    base: "main".into(),
                    cross: false,
                },
            );
            assert_eq!(pass(&rig, &run), StopStep::Ended);
            assert!(rig.finished(&run));
            assert!(rig.sim.writes().is_empty());
        }

        #[test]
        fn asks_nothing_of_a_machine_unreachable_for_fifteen_minutes() {
            let (mut rig, run) = requested();
            rig.view = cut_off(&rig, crate::factory::runs::resume::UNREACHABLE_FOR);
            assert_eq!(
                pass(&rig, &run),
                StopStep::Stuck {
                    reason: STOPPED_BY_HAND.into()
                }
            );
            assert!(calls(&rig).is_empty());
            assert!(rig.finished(&run));
        }

        #[test]
        fn asks_a_machine_unreachable_for_less_and_is_cut_short_when_it_cannot_be_reached() {
            let (mut rig, run) = requested();
            rig.view = cut_off(&rig, Duration::from_secs(60));
            rig.worker
                .on_end_sessions(Err(crate::factory::worker::WorkerError::Unreachable(
                    "timed out".into(),
                )));
            let events = rig.events(&run).len();
            assert!(advance(&rig.ctx(), &run).is_err());
            assert_eq!(rig.events(&run).len(), events);
            assert!(rig.sim.writes().is_empty());
            // The next pass asks again and finishes.
            assert!(matches!(pass(&rig, &run), StopStep::Stuck { .. }));
            assert_eq!(calls(&rig).len(), 2);
        }

        #[test]
        fn does_nothing_when_the_lease_is_lost() {
            let (mut rig, run) = requested();
            rig.guard = Box::new(Lost);
            let events = rig.events(&run).len();
            let error = advance(&rig.ctx(), &run).unwrap_err();
            assert!(matches!(error, RunError::LeaseLost(_)), "{error}");
            assert_eq!(rig.events(&run).len(), events);
            assert!(calls(&rig).is_empty());
        }
    }

    // ----- the runner's `watch`, and what a person's move and the end of a run mean -----

    mod reconcile_a_run {
        use super::*;
        use crate::factory::messages::{
            CANNOT_CONFIRM, CANNOT_VERIFY_DURING_BUILD, CHANGED_DURING_BUILD,
        };
        use crate::factory::runs::testing::approval_answer;

        const URL12: &str = "https://github.com/rawphp/softfac/pull/12";
        const FEEDBACK_WS: &str = "00000000-0000-0000-0000-000000000f11";

        fn pass(rig: &Rig, run: &RunId) -> ReconcileStep {
            reconcile(&rig.ctx(), run).unwrap()
        }

        fn building_run() -> (Rig, RunId) {
            let mut rig = Rig::new();
            let run = run_id(1);
            built(&mut rig, &run);
            (rig, run)
        }

        fn pr_opened() -> Event {
            Event::PrOpened {
                pr: 12,
                url: URL12.into(),
            }
        }

        fn pull(state: &'static str) -> SimPull {
            SimPull {
                state,
                draft: true,
                head: BRANCH.into(),
                base: "main".into(),
                cross: false,
            }
        }

        fn fws() -> WorkspaceId {
            WorkspaceId(uuid::Uuid::from_u128(0xf11))
        }

        /// A run the gate passed: `pr-open`, its pull request open, its build workspace archived.
        fn pr_open_run() -> (Rig, RunId) {
            let (mut rig, run) = building_run();
            rig.write(&run, pr_opened());
            rig.moved(&run, State::Building, State::PrOpen);
            rig.sim.set_labels(&["factory:pr-open"]);
            rig.sim.set_assignees(&[]);
            rig.sim.set_pull(12, pull("OPEN"));
            rig.view = FleetView::builder().online(rig.machine).build();
            (rig, run)
        }

        /// A `pr-open` run whose pull request GitHub does not have.
        fn pr_open_run_without_its_pull_request() -> (Rig, RunId) {
            let (mut rig, run) = (Rig::new(), run_id(1));
            built(&mut rig, &run);
            rig.write(&run, pr_opened());
            rig.moved(&run, State::Building, State::PrOpen);
            rig.sim.set_labels(&["factory:pr-open"]);
            rig.sim.set_assignees(&[]);
            rig.view = FleetView::builder().online(rig.machine).build();
            (rig, run)
        }

        /// The fleet shows a feedback round's workspace, which that round's session is in.
        fn with_feedback_workspace(rig: &mut Rig, span: Option<Duration>) {
            let builder =
                FleetView::builder().workspace(workspace_record(rig.machine, fws(), "fac/8-1-f1"));
            rig.view = match span {
                Some(span) => builder.unreachable(rig.machine, span).build(),
                None => builder.online(rig.machine).build(),
            };
        }

        const ROUND_SESSION: &str = "00000000-0000-0000-0000-0000000005e9";

        /// A feedback round's decision, as `feedback` records it.
        fn round_decision() -> Event {
            Event::FeedbackRelaunching {
                n: 1,
                branch: BRANCH.into(),
                items: vec!["review:11".into()],
                message: "Please rename it.".into(),
            }
        }

        /// The round's session, launched in the round's workspace.
        fn round_session_launched(run: &RunId) -> Event {
            Event::SessionLaunched {
                thread_id: ROUND_SESSION.into(),
                worktree: Some(FEEDBACK_WS.into()),
                branch: Some(BRANCH.into()),
                model_instance: None,
                model: Some("opus".into()),
                model_options: None,
                resumed_from: Some(crate::factory::ledger::events::ResumedFrom {
                    run: run.to_string(),
                    branch: BRANCH.into(),
                }),
                base: Some("main".into()),
                title: None,
            }
        }

        /// A run whose feedback round moved it back to `building` (the decision written again
        /// after the move) and whose session is not launched yet.
        fn round_moved() -> (Rig, RunId) {
            let (mut rig, run) = pr_open_run();
            rig.write(&run, round_decision());
            rig.moved(&run, State::PrOpen, State::Building);
            rig.write(&run, round_decision());
            rig.sim.set_labels(&["factory:building"]);
            rig.sim.set_assignees(&[LOGIN]);
            with_feedback_workspace(&mut rig, None);
            (rig, run)
        }

        /// [`round_moved`] with the round's session launched and working.
        fn round_launched() -> (Rig, RunId) {
            let (rig, run) = round_moved();
            rig.write(&run, round_session_launched(&run));
            (rig, run)
        }

        fn stranger_edited() -> String {
            approval_answer(
                "2026-10-04T12:00:00Z",
                "tom",
                &[("2026-10-04T10:30:00Z", "mallory")],
            )
        }

        fn history_that_cannot_be_judged() -> String {
            let times: Vec<String> = (0..100)
                .map(|i| format!("2026-10-04T10:{:02}:{:02}Z", 1 + i / 60, i % 60))
                .collect();
            let edits: Vec<(&str, &str)> = times.iter().map(|t| (t.as_str(), "tom")).collect();
            approval_answer(LABELED_AT, "tom", &edits)
        }

        fn text_reads(rig: &Rig) -> usize {
            rig.sim
                .calls()
                .iter()
                .filter(|call| {
                    call.first().is_some_and(|a| a == "api")
                        && call.get(1).is_some_and(|a| a == "graphql")
                })
                .count()
        }

        fn issue_reads(rig: &Rig) -> usize {
            rig.sim
                .calls()
                .iter()
                .filter(|call| {
                    call.first().is_some_and(|a| a == "issue")
                        && call.get(1).is_some_and(|a| a == "view")
                        && call.iter().any(|a| a.contains("title"))
                })
                .count()
        }

        fn steps_starting(rig: &Rig, run: &RunId, prefix: &str) -> Vec<String> {
            rig.steps(run)
                .into_iter()
                .filter(|step| step.starts_with(prefix))
                .collect()
        }

        fn nothing_asked(rig: &Rig) {
            assert!(calls(rig).is_empty(), "{:?}", calls(rig));
            assert!(rig.sim.writes().is_empty(), "{:?}", rig.sim.writes());
        }

        // ----- the runner's `watch` -----

        mod watch {
            use super::*;

            #[test]
            fn leaves_a_run_alone_when_the_text_is_unchanged_since_the_approval() {
                let (rig, run) = building_run();
                let events = rig.events(&run).len();
                assert_eq!(pass(&rig, &run), ReconcileStep::Unchanged);
                assert_eq!(text_reads(&rig), 1);
                nothing_asked(&rig);
                assert_eq!(rig.events(&run).len(), events);
                // And again: the same read each pass, never a step that says it passed.
                assert_eq!(pass(&rig, &run), ReconcileStep::Unchanged);
                assert_eq!(text_reads(&rig), 2);
                assert_eq!(rig.events(&run).len(), events);
            }

            #[test]
            fn checks_since_the_approval_the_ledger_records() {
                // An edit by a stranger at 13:00 is after an approval at 12:34 and before none.
                let (rig, run) = building_run_approved_at("2026-10-04T12:34:56Z");
                rig.sim.set_approval(approval_answer(
                    LABELED_AT,
                    "tom",
                    &[("2026-10-04T10:30:00Z", "mallory")],
                ));
                assert_eq!(pass(&rig, &run), ReconcileStep::Unchanged);
                rig.sim.set_approval(approval_answer(
                    LABELED_AT,
                    "tom",
                    &[("2026-10-04T13:00:00Z", "mallory")],
                ));
                assert_eq!(
                    pass(&rig, &run),
                    ReconcileStep::Stopped {
                        reason: CHANGED_DURING_BUILD.into()
                    }
                );
            }

            fn building_run_approved_at(at: &str) -> (Rig, RunId) {
                let (mut rig, run) = (Rig::new(), run_id(1));
                rig.step(&run, &format!("approved {at}"));
                rig.write(
                    &run,
                    Event::Claimed {
                        machine: rig.machine.to_string(),
                        github_user: LOGIN.into(),
                    },
                );
                rig.moved(&run, State::Ready, State::Triage);
                rig.step(&run, &format!("workspace-created {WS}"));
                rig.step(&run, "approval-rechecked");
                rig.write(&run, launched());
                rig.moved(&run, State::Triage, State::Building);
                rig.sim.set_labels(&["factory:building"]);
                rig.sim.set_assignees(&[LOGIN]);
                rig.view = fleet(&rig);
                (rig, run)
            }

            fn launched() -> Event {
                Event::SessionLaunched {
                    thread_id: SESSION.into(),
                    worktree: Some(WS.into()),
                    branch: Some(BRANCH.into()),
                    model_instance: None,
                    model: Some("opus".into()),
                    model_options: None,
                    resumed_from: None,
                    base: Some("main".into()),
                    title: Some("Make it faster".into()),
                }
            }

            fn assert_stopped_for(rig: &Rig, run: &RunId, code: &str, reason: &str) {
                assert_eq!(
                    pass(rig, run),
                    ReconcileStep::Stopped {
                        reason: reason.into()
                    }
                );
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
                assert_eq!(rig.sim.assignees(), Vec::<String>::new());
                assert_eq!(stuck_comments(rig), 1);
                assert!(rig.sim.comments().iter().any(|c| c.contains(reason)));
                // The session is ended and the workspace kept.
                assert_eq!(calls(rig), vec![Call::EndSessions { workspace: ws() }]);
                assert!(rig.finished(run));
                let causes: Vec<_> = rig
                    .events(run)
                    .into_iter()
                    .filter(|e| ["stuck", "state_changed", "finished"].contains(&e.event.as_str()))
                    .skip(2)
                    .map(|e| e.cause.unwrap())
                    .collect();
                assert_eq!(causes.len(), 3);
                assert!(causes.iter().all(
                    |cause| cause == &serde_json::json!({"step": "watch", "reason": code})
                ));
            }

            #[test]
            fn stops_a_run_whose_text_someone_else_changed() {
                let (rig, run) = building_run();
                rig.sim.set_approval(stranger_edited());
                assert_stopped_for(
                    &rig,
                    &run,
                    "text-changed-during-build",
                    CHANGED_DURING_BUILD,
                );
            }

            #[test]
            fn stops_a_run_whose_history_cannot_be_judged_without_blaming_anyone() {
                let (rig, run) = building_run();
                rig.sim.set_approval(history_that_cannot_be_judged());
                assert_stopped_for(&rig, &run, "edits-unverifiable", CANNOT_VERIFY_DURING_BUILD);
                assert!(!rig.sim.comments().iter().any(|c| c.contains("mallory")));
            }

            #[test]
            fn finishes_a_stopped_run_and_nothing_polls_it_again() {
                let (rig, run) = building_run();
                rig.sim.set_approval(stranger_edited());
                pass(&rig, &run);
                let reads = text_reads(&rig);
                let asked = calls(&rig).len();
                // The workspace is kept while the issue is `factory:stuck`: the issue is read, the
                // text is not, and nothing is asked of the worker.
                assert_eq!(pass(&rig, &run), ReconcileStep::Unchanged);
                assert_eq!(text_reads(&rig), reads);
                assert_eq!(calls(&rig).len(), asked);
                assert_eq!(
                    advance(&rig.ctx(), &run).unwrap(),
                    StopStep::Skipped { why: "finished" }
                );
            }

            #[test]
            fn leaves_the_draft_pull_request_open_and_does_not_mark_it_ready() {
                let (rig, run) = building_run();
                rig.write(&run, pr_opened());
                rig.sim.set_pull(12, pull("OPEN"));
                rig.sim.set_approval(stranger_edited());
                assert!(matches!(pass(&rig, &run), ReconcileStep::Stopped { .. }));
                assert!(
                    !rig.sim
                        .calls()
                        .iter()
                        .any(|call| call.first().is_some_and(|a| a == "pr")
                            && call.get(1).is_some_and(|a| a == "ready"))
                );
            }

            #[test]
            fn does_nothing_for_a_run_that_has_no_session_or_is_not_building() {
                let questions = Event::QuestionsPosted {
                    questions: "1. Which?".into(),
                };
                let stuck = Event::Stuck {
                    reason: "gave up".into(),
                    branch: None,
                    commits_ahead: None,
                };
                let cases = [
                    ("no session", "no-session"),
                    ("triage", "not-building"),
                    ("needs-info", "not-building"),
                    ("stuck", "not-building"),
                    ("half stuck", "not-building"),
                    ("half needs-info", "not-building"),
                ];
                for (name, why) in cases {
                    let rig = Rig::new();
                    let run = run_id(1);
                    let claim = Event::Claimed {
                        machine: rig.machine.to_string(),
                        github_user: LOGIN.into(),
                    };
                    rig.step(&run, &format!("approved {LABELED_AT}"));
                    rig.write(&run, claim);
                    rig.moved(&run, State::Ready, State::Triage);
                    match name {
                        "no session" => rig.moved(&run, State::Triage, State::Building),
                        "triage" => {}
                        _ => {
                            rig.write(&run, launched());
                            rig.moved(&run, State::Triage, State::Building);
                            match name {
                                "needs-info" => {
                                    rig.write(&run, questions.clone());
                                    rig.moved(&run, State::Building, State::NeedsInfo);
                                }
                                "stuck" => {
                                    rig.write(&run, stuck.clone());
                                    rig.moved(&run, State::Building, State::Stuck);
                                }
                                "half stuck" => rig.write(&run, stuck.clone()),
                                _ => rig.write(&run, questions.clone()),
                            }
                        }
                    }
                    rig.sim.set_approval(stranger_edited());
                    let events = rig.events(&run).len();
                    assert_eq!(pass(&rig, &run), ReconcileStep::Skipped { why }, "{name}");
                    assert!(rig.sim.calls().is_empty(), "{name}");
                    assert!(calls(&rig).is_empty(), "{name}");
                    assert_eq!(rig.events(&run).len(), events, "{name}");
                }
            }

            #[test]
            fn does_nothing_for_a_run_that_is_finished_and_holds_no_workspace() {
                let (mut rig, run) = building_run();
                rig.sim.set_approval(stranger_edited());
                finish(&rig, &run);
                rig.view = FleetView::builder().online(rig.machine).build();
                assert_eq!(pass(&rig, &run), ReconcileStep::Unchanged);
                assert!(rig.sim.calls().is_empty());
                assert!(calls(&rig).is_empty());
            }

            #[test]
            fn is_an_error_never_a_stop_or_a_pass_when_the_text_cannot_be_read() {
                let (rig, run) = building_run();
                rig.sim.set_approval(stranger_edited());
                rig.sim.fail("graphql", 1);
                let events = rig.events(&run).len();
                let error = reconcile(&rig.ctx(), &run).unwrap_err();
                assert!(matches!(error, RunError::Tracker(_)), "{error}");
                assert!(calls(&rig).is_empty(), "nothing ended");
                assert_eq!(rig.events(&run).len(), events, "nothing written");
                assert_eq!(rig.sim.labels(), vec!["factory:building"]);
                // The next pass reads again and, the text being changed, stops the run.
                assert!(matches!(pass(&rig, &run), ReconcileStep::Stopped { .. }));
            }

            #[test]
            fn is_an_error_with_nothing_written_when_the_archive_fails_and_the_next_pass_retries() {
                let (rig, run) = building_run();
                rig.sim.set_approval(stranger_edited());
                for _ in 0..2 {
                    rig.worker
                        .on_end_sessions(Err(crate::factory::worker::WorkerError::Failed(
                            "refused".into(),
                        )));
                }
                let events = rig.events(&run).len();
                for pass_number in 1..=2 {
                    let error = reconcile(&rig.ctx(), &run).unwrap_err();
                    assert!(matches!(error, RunError::Worker(_)), "{error}");
                    assert_eq!(rig.events(&run).len(), events, "pass {pass_number}");
                    assert_eq!(rig.sim.labels(), vec!["factory:building"]);
                    assert!(rig.sim.writes().is_empty());
                }
                assert!(matches!(pass(&rig, &run), ReconcileStep::Stopped { .. }));
                assert_eq!(stuck_comments(&rig), 1);
            }

            #[test]
            fn fails_closed_for_a_run_with_no_approval_step() {
                let (mut rig, run) = (Rig::new(), run_id(1));
                rig.write(
                    &run,
                    Event::Claimed {
                        machine: rig.machine.to_string(),
                        github_user: LOGIN.into(),
                    },
                );
                rig.moved(&run, State::Ready, State::Triage);
                rig.step(&run, &format!("workspace-created {WS}"));
                rig.write(&run, launched());
                rig.moved(&run, State::Triage, State::Building);
                rig.sim.set_labels(&["factory:building"]);
                rig.sim.set_assignees(&[LOGIN]);
                rig.view = fleet(&rig);
                assert_stopped_for(&rig, &run, "approval-unconfirmed", CANNOT_CONFIRM);
                // No read: there is no moment to read since.
                assert_eq!(text_reads(&rig), 0);
            }

            #[test]
            fn is_an_error_with_nothing_written_for_a_run_with_no_approval_step_and_a_failing_archive()
             {
                let (mut rig, run) = (Rig::new(), run_id(1));
                rig.write(
                    &run,
                    Event::Claimed {
                        machine: rig.machine.to_string(),
                        github_user: LOGIN.into(),
                    },
                );
                rig.moved(&run, State::Ready, State::Triage);
                rig.write(&run, launched());
                rig.moved(&run, State::Triage, State::Building);
                rig.view = fleet(&rig);
                rig.worker
                    .on_end_sessions(Err(crate::factory::worker::WorkerError::Failed(
                        "refused".into(),
                    )));
                let events = rig.events(&run).len();
                assert!(reconcile(&rig.ctx(), &run).is_err());
                assert_eq!(rig.events(&run).len(), events);
                assert!(rig.sim.calls().is_empty());
            }

            #[test]
            fn leaves_a_stopped_run_to_advance() {
                let (rig, run) = building_run();
                rig.write(&run, stop_line());
                rig.sim.set_approval(stranger_edited());
                assert_eq!(
                    pass(&rig, &run),
                    ReconcileStep::Skipped {
                        why: "not-building"
                    }
                );
                assert!(rig.sim.calls().is_empty());
                assert!(calls(&rig).is_empty());
            }

            #[test]
            fn skips_a_run_whose_own_merged_pull_request_closed_the_issue() {
                let (rig, run) = building_run();
                rig.write(&run, pr_opened());
                rig.sim.set_approval(stranger_edited());
                rig.sim.set_closed(true);
                rig.sim.set_pull(12, pull("MERGED"));
                let events = rig.events(&run).len();
                assert_eq!(
                    pass(&rig, &run),
                    ReconcileStep::Skipped { why: "pr-merged" }
                );
                assert_eq!(rig.events(&run).len(), events);
                assert!(calls(&rig).is_empty());
                assert!(rig.sim.writes().is_empty());
            }

            #[test]
            fn goes_on_to_stop_a_run_whose_issue_a_person_closed() {
                let (rig, run) = building_run();
                rig.write(&run, pr_opened());
                rig.sim.set_approval(stranger_edited());
                rig.sim.set_closed(true);
                rig.sim.set_pull(12, pull("CLOSED"));
                assert!(matches!(pass(&rig, &run), ReconcileStep::Stopped { .. }));
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
            }

            #[test]
            fn asks_about_the_issue_only_for_a_run_that_reported_a_pull_request() {
                let (rig, run) = building_run();
                rig.sim.set_approval(stranger_edited());
                rig.sim.set_closed(true);
                assert!(matches!(pass(&rig, &run), ReconcileStep::Stopped { .. }));
                assert_eq!(issue_reads(&rig), 0);
            }
        }

        // ----- a merged or closed pull request, a closed issue -----

        mod the_end_of_a_run {
            use super::*;

            fn archive_calls(rig: &Rig) -> Vec<WorkspaceId> {
                calls(rig)
                    .into_iter()
                    .filter_map(|call| match call {
                        Call::Archive { workspace } => Some(workspace),
                        _ => None,
                    })
                    .collect()
            }

            fn outcome(rig: &Rig, run: &RunId) -> serde_json::Value {
                rig.events(run)
                    .into_iter()
                    .find(|e| e.event == "finished")
                    .unwrap()
                    .data
            }

            #[test]
            fn an_open_pull_request_and_an_open_issue_leave_a_pr_open_run_alone() {
                let (rig, run) = pr_open_run();
                let events = rig.events(&run).len();
                assert_eq!(pass(&rig, &run), ReconcileStep::Unchanged);
                assert_eq!(rig.events(&run).len(), events);
                assert!(calls(&rig).is_empty());
                // Not the text, the label or the assignee: only a building run is judged for them.
                rig.sim.set_labels(&[]);
                rig.sim.set_assignees(&["mallory"]);
                rig.sim.set_approval(stranger_edited());
                assert_eq!(pass(&rig, &run), ReconcileStep::Unchanged);
                assert_eq!(text_reads(&rig), 0);
            }

            #[test]
            fn a_merged_pull_request_ends_a_pr_open_run_with_a_round_decided_without_ending_the_old_session()
             {
                let (mut rig, run) = pr_open_run();
                rig.write(&run, round_decision());
                // The build's workspace still shows, as its archive may not have gone through.
                rig.view = FleetView::builder()
                    .online(rig.machine)
                    .workspace(workspace_record(rig.machine, ws(), BRANCH))
                    .build();
                rig.sim.set_pull(12, pull("MERGED"));
                assert_eq!(pass(&rig, &run), ReconcileStep::Ended);
                assert!(rig.finished(&run));
                // The build's session ended at the gate; the round has none yet.
                assert!(
                    !calls(&rig)
                        .iter()
                        .any(|call| matches!(call, Call::EndSessions { .. })),
                    "{:?}",
                    calls(&rig)
                );
            }

            #[test]
            fn a_merged_pull_request_ends_a_pr_open_run_and_archives_a_feedback_workspace() {
                let (mut rig, run) = pr_open_run();
                with_feedback_workspace(&mut rig, None);
                rig.sim.set_pull(12, pull("MERGED"));
                assert_eq!(pass(&rig, &run), ReconcileStep::Ended);
                assert_eq!(archive_calls(&rig), vec![fws()]);
                assert!(rig.finished(&run));
                let finished = outcome(&rig, &run);
                assert_eq!(finished["outcome"], "pr-open");
                assert_eq!(finished["pr_end"], "merged");
                assert_eq!(finished["pr_url"], URL12);
                // Recorded before it was asked, once.
                assert_eq!(
                    steps_starting(&rig, &run, "reconcile-"),
                    vec![
                        "reconcile-session-ended".to_owned(),
                        format!("reconcile-archiving {FEEDBACK_WS}"),
                        format!("reconcile-archived {FEEDBACK_WS}"),
                    ]
                );
                // Nothing is posted: the pull request's merge is the end.
                assert!(rig.sim.writes().is_empty());
                // The fleet no longer shows it: nothing more is asked.
                rig.view = FleetView::builder().online(rig.machine).build();
                assert_eq!(pass(&rig, &run), ReconcileStep::Unchanged);
                assert_eq!(archive_calls(&rig), vec![fws()]);
            }

            #[test]
            fn a_closed_pull_request_ends_a_pr_open_run_as_closed() {
                let (mut rig, run) = pr_open_run();
                with_feedback_workspace(&mut rig, None);
                rig.sim.set_pull(12, pull("CLOSED"));
                assert_eq!(pass(&rig, &run), ReconcileStep::Ended);
                assert_eq!(outcome(&rig, &run)["pr_end"], "closed");
                assert_eq!(archive_calls(&rig), vec![fws()]);
            }

            #[test]
            fn a_pull_request_that_is_gone_ends_a_pr_open_run_as_closed() {
                let (rig, run) = pr_open_run_without_its_pull_request();
                assert_eq!(pass(&rig, &run), ReconcileStep::Ended);
                assert_eq!(outcome(&rig, &run)["pr_end"], "closed");
            }

            #[test]
            fn a_closed_issue_ends_a_pr_open_run_that_nobody_merged() {
                let (mut rig, run) = pr_open_run();
                with_feedback_workspace(&mut rig, None);
                rig.sim.set_closed(true);
                assert_eq!(pass(&rig, &run), ReconcileStep::Abandoned);
                assert_eq!(archive_calls(&rig), vec![fws()]);
                assert!(rig.finished(&run));
                assert!(rig.sim.writes().is_empty());
            }

            #[test]
            fn a_closed_issue_abandons_a_building_run_and_archives_every_workspace_of_it() {
                let (mut rig, run) = building_run();
                let gate = WorkspaceId(uuid::Uuid::from_u128(0x911));
                let resume = WorkspaceId(uuid::Uuid::from_u128(0x5a1));
                rig.step(
                    &run,
                    &format!("gate-requested 1 1 {SHA} {SHA} {}", rig.machine),
                );
                rig.view = FleetView::builder()
                    .online(rig.machine)
                    .workspace(workspace_record(rig.machine, ws(), BRANCH))
                    .workspace(workspace_record(rig.machine, gate, "fac/8-1-g1"))
                    .workspace(workspace_record(rig.machine, resume, "fac/8-1-r1"))
                    .build();
                rig.sim.set_closed(true);
                // The gate holds the run while it runs: the gate's step decides.
                assert_eq!(
                    pass(&rig, &run),
                    ReconcileStep::Skipped {
                        why: "gate-running"
                    }
                );
                rig.step(&run, "gate-archived 1");
                assert_eq!(pass(&rig, &run), ReconcileStep::Abandoned);
                let mut archived = archive_calls(&rig);
                archived.sort_by_key(|id| id.0);
                let mut expected = vec![ws(), gate, resume];
                expected.sort_by_key(|id| id.0);
                assert_eq!(archived, expected);
                assert!(rig.finished(&run));
                assert!(rig.sim.writes().is_empty());
                // The session was ended once, before the archives.
                assert_eq!(
                    calls(&rig).first(),
                    Some(&Call::EndSessions { workspace: ws() })
                );
            }

            #[test]
            fn the_runs_own_merged_pull_request_that_closed_its_issue_ends_a_building_run() {
                let (rig, run) = building_run();
                rig.write(&run, pr_opened());
                rig.sim.set_closed(true);
                rig.sim.set_pull(12, pull("MERGED"));
                assert_eq!(pass(&rig, &run), ReconcileStep::Ended);
                assert_eq!(outcome(&rig, &run)["pr_end"], "merged");
                assert_eq!(archive_calls(&rig), vec![ws()]);
            }

            #[test]
            fn a_workspace_already_archived_is_not_asked_for_again() {
                let (mut rig, run) = pr_open_run();
                with_feedback_workspace(&mut rig, None);
                rig.sim.set_pull(12, pull("MERGED"));
                // The archive was asked and answered, and the pass was cut short before the end.
                rig.step(&run, &format!("reconcile-archiving {FEEDBACK_WS}"));
                rig.step(&run, &format!("reconcile-archived {FEEDBACK_WS}"));
                rig.step(&run, "reconcile-session-ended");
                assert_eq!(pass(&rig, &run), ReconcileStep::Ended);
                assert!(archive_calls(&rig).is_empty());
            }

            #[test]
            fn an_archive_the_worker_calls_not_live_counts_as_done() {
                let (mut rig, run) = pr_open_run();
                with_feedback_workspace(&mut rig, None);
                rig.sim.set_pull(12, pull("MERGED"));
                rig.worker
                    .on_archive(Err(crate::factory::worker::WorkerError::Failed(
                        "workspace is not a live workspace of this machine".into(),
                    )));
                assert_eq!(pass(&rig, &run), ReconcileStep::Ended);
                assert_eq!(steps_starting(&rig, &run, "reconcile-archived").len(), 1);
            }

            #[test]
            fn a_machine_unreachable_for_fifteen_minutes_is_left_with_no_call() {
                let (mut rig, run) = pr_open_run();
                with_feedback_workspace(
                    &mut rig,
                    Some(crate::factory::runs::resume::UNREACHABLE_FOR),
                );
                rig.sim.set_pull(12, pull("MERGED"));
                assert_eq!(pass(&rig, &run), ReconcileStep::Ended);
                assert!(calls(&rig).is_empty());
                assert_eq!(
                    steps_starting(&rig, &run, "reconcile-left"),
                    vec![format!("reconcile-left {FEEDBACK_WS} its machine is gone")]
                );
            }

            #[test]
            fn a_machine_that_does_not_answer_yet_is_waited_for_and_asked_again() {
                let (mut rig, run) = pr_open_run();
                with_feedback_workspace(&mut rig, Some(Duration::from_secs(60)));
                rig.sim.set_pull(12, pull("MERGED"));
                rig.worker
                    .on_archive(Err(crate::factory::worker::WorkerError::Unreachable(
                        "timed out".into(),
                    )));
                assert_eq!(
                    pass(&rig, &run),
                    ReconcileStep::Waiting {
                        workspaces: vec![fws()]
                    }
                );
                assert!(!rig.finished(&run));
                // It is asked again, and the recorded intent is not written twice.
                assert_eq!(pass(&rig, &run), ReconcileStep::Ended);
                assert_eq!(archive_calls(&rig), vec![fws(), fws()]);
                assert_eq!(steps_starting(&rig, &run, "reconcile-archiving").len(), 1);
            }

            #[test]
            fn a_lost_lease_ends_nothing_and_archives_nothing() {
                let (mut rig, run) = pr_open_run();
                with_feedback_workspace(&mut rig, None);
                rig.sim.set_pull(12, pull("MERGED"));
                rig.guard = Box::new(Lost);
                let error = reconcile(&rig.ctx(), &run).unwrap_err();
                assert!(matches!(error, RunError::LeaseLost(_)), "{error}");
                assert!(!rig.finished(&run));
                assert!(calls(&rig).is_empty());
            }

            #[test]
            fn a_tracker_failure_leaves_nothing_half_done_and_the_next_pass_ends_the_run_once() {
                let (mut rig, run) = pr_open_run();
                with_feedback_workspace(&mut rig, None);
                rig.sim.set_pull(12, pull("MERGED"));
                rig.sim.fail("pr view 12", 1);
                let events = rig.events(&run).len();
                let error = reconcile(&rig.ctx(), &run).unwrap_err();
                assert!(matches!(error, RunError::Tracker(_)), "{error}");
                assert_eq!(rig.events(&run).len(), events);
                assert!(calls(&rig).is_empty());
                assert_eq!(pass(&rig, &run), ReconcileStep::Ended);
                assert_eq!(
                    rig.events(&run)
                        .iter()
                        .filter(|e| e.event == "finished")
                        .count(),
                    1
                );
            }

            #[test]
            fn an_archive_that_fails_in_the_middle_is_carried_on_and_each_is_asked_once() {
                let (mut rig, run) = pr_open_run();
                let resume = WorkspaceId(uuid::Uuid::from_u128(0x5a1));
                rig.view = FleetView::builder()
                    .online(rig.machine)
                    .workspace(workspace_record(rig.machine, resume, "fac/8-1-r1"))
                    .workspace(workspace_record(rig.machine, fws(), "fac/8-1-f1"))
                    .build();
                rig.sim.set_pull(12, pull("MERGED"));
                // The first archive answers, the second is refused.
                rig.worker.on_archive(Ok(()));
                rig.worker
                    .on_archive(Err(crate::factory::worker::WorkerError::Failed(
                        "busy".into(),
                    )));
                let error = reconcile(&rig.ctx(), &run).unwrap_err();
                assert!(matches!(error, RunError::Worker(_)), "{error}");
                assert!(!rig.finished(&run));
                assert_eq!(steps_starting(&rig, &run, "reconcile-archived").len(), 1);
                assert_eq!(pass(&rig, &run), ReconcileStep::Ended);
                // The one answered is not asked again; the one refused is, once more.
                assert_eq!(archive_calls(&rig), vec![resume, fws(), fws()]);
                assert_eq!(steps_starting(&rig, &run, "reconcile-archived").len(), 2);
                assert_eq!(steps_starting(&rig, &run, "reconcile-archiving").len(), 2);
            }
        }

        // ----- a person's edit, a removed label, an assignee -----

        mod a_persons_move {
            use super::*;

            fn stopped(rig: &Rig, run: &RunId, reason: &str) {
                assert_eq!(
                    pass(rig, run),
                    ReconcileStep::Stopped {
                        reason: reason.into()
                    }
                );
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
                assert!(rig.finished(run));
                // The session is ended and the workspace kept.
                assert_eq!(calls(rig), vec![Call::EndSessions { workspace: ws() }]);
                assert_eq!(stuck_comments(rig), 1);
                assert!(rig.sim.comments().iter().any(|c| c.contains(reason)));
            }

            #[test]
            fn a_removed_label_stops_a_building_run() {
                let (rig, run) = building_run();
                rig.sim.set_labels(&[]);
                stopped(&rig, &run, LABEL_REMOVED);
            }

            #[test]
            fn an_assignee_other_than_the_login_stops_a_building_run() {
                let (rig, run) = building_run();
                rig.sim.set_assignees(&[LOGIN, "mallory"]);
                stopped(&rig, &run, ASSIGNEE_CHANGED);
                assert!(!rig.sim.comments().iter().any(|c| c.contains("mallory")));
            }

            /// What a stop of the round's session says: ended in the round's workspace, which is
            /// kept, and not in the build's.
            fn stopped_in_the_round(rig: &Rig, run: &RunId, reason: &str) {
                assert_eq!(
                    pass(rig, run),
                    ReconcileStep::Stopped {
                        reason: reason.into()
                    }
                );
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
                assert!(rig.finished(run));
                assert_eq!(calls(rig), vec![Call::EndSessions { workspace: fws() }]);
                assert_eq!(stuck_comments(rig), 1);
            }

            #[test]
            fn a_removed_label_stops_a_run_whose_feedback_round_has_launched() {
                let (rig, run) = round_launched();
                assert_eq!(pass(&rig, &run), ReconcileStep::Unchanged);
                rig.sim.set_labels(&[]);
                stopped_in_the_round(&rig, &run, LABEL_REMOVED);
            }

            #[test]
            fn an_assignee_other_than_the_login_stops_a_run_whose_feedback_round_has_launched() {
                let (rig, run) = round_launched();
                rig.sim.set_assignees(&[LOGIN, "mallory"]);
                stopped_in_the_round(&rig, &run, ASSIGNEE_CHANGED);
            }

            #[test]
            fn a_text_stop_begun_while_a_round_is_set_up_is_left_to_feedback() {
                let (rig, run) = round_moved();
                rig.step(
                    &run,
                    "stuck-stop {\"code\":\"text-changed-during-build\",\"stop\":{\"reason\":\"said so\"}}",
                );
                let events = rig.events(&run).len();
                assert_eq!(
                    pass(&rig, &run),
                    ReconcileStep::Skipped { why: "relaunching" }
                );
                assert_eq!(rig.events(&run).len(), events);
                assert!(calls(&rig).is_empty());
                assert!(rig.sim.writes().is_empty());
            }

            #[test]
            fn the_projects_own_login_assigned_is_what_a_claim_leaves() {
                let (rig, run) = building_run();
                rig.sim.set_assignees(&["Softfac-Bot"]);
                assert_eq!(pass(&rig, &run), ReconcileStep::Unchanged);
                // Nobody assigned is not a person's assigning.
                rig.sim.set_assignees(&[]);
                assert_eq!(pass(&rig, &run), ReconcileStep::Unchanged);
                assert!(rig.sim.writes().is_empty());
            }

            #[test]
            fn a_transition_in_flight_is_not_judged() {
                type Arrange = fn(&Rig, &RunId);
                let cases: Vec<(&str, Arrange)> = vec![
                    ("questions being posted", |rig, run| {
                        rig.step(run, "questions-commenting 0")
                    }),
                    ("questions posted", |rig, run| {
                        rig.write(
                            run,
                            Event::QuestionsPosted {
                                questions: "1. Which?".into(),
                            },
                        )
                    }),
                    ("stuck decided by another step", |rig, run| {
                        rig.step(
                            run,
                            "stuck-stop {\"code\":\"session-stuck\",\"stop\":{\"reason\":\"said so\"}}",
                        )
                    }),
                    ("approval withdrawn", |rig, run| {
                        rig.step(run, "approval-withdrawn")
                    }),
                    ("stop requested", |rig, run| rig.write(run, stop_line())),
                    ("a resume owed", |rig, run| {
                        rig.write(
                            run,
                            Event::SessionInterrupted {
                                resumes: 1,
                                relaunch: false,
                                run_ordinal: None,
                                branch: Some(BRANCH.into()),
                                t3_error: None,
                                ended_in_pass: 1,
                            },
                        )
                    }),
                    ("a feedback round decided and not launched", |rig, run| {
                        rig.write(run, pr_opened());
                        rig.moved(run, State::Building, State::PrOpen);
                        rig.write(run, round_decision());
                        rig.moved(run, State::PrOpen, State::Building);
                        rig.write(run, round_decision());
                    }),
                    (
                        "a feedback round whose move to building was written and whose decision was not written again",
                        |rig, run| {
                            rig.write(run, pr_opened());
                            rig.moved(run, State::Building, State::PrOpen);
                            rig.write(run, round_decision());
                            rig.moved(run, State::PrOpen, State::Building);
                        },
                    ),
                    ("a re-gate relaunch decided and not launched", |rig, run| {
                        rig.write(run, pr_opened());
                        rig.write(
                            run,
                            Event::RegateRelaunching {
                                attempt: 1,
                                branch: BRANCH.into(),
                                message: "The gate failed.".into(),
                            },
                        );
                    }),
                    ("a gate pass recorded", |rig, run| {
                        rig.ledger
                            .write(
                                run,
                                &Event::StepChanged {
                                    step: format!("gate-passed {SHA} 1"),
                                },
                                &Cause::new(CauseStep::Gate, "gate-passed").attempt(1),
                            )
                            .unwrap();
                    }),
                ];
                for (name, arrange) in cases {
                    let (rig, run) = building_run();
                    arrange(&rig, &run);
                    // The label moves as the transition does, and someone else is assigned.
                    rig.sim.set_labels(&["factory:pr-open"]);
                    rig.sim.set_assignees(&["mallory"]);
                    let events = rig.events(&run).len();
                    let step = pass(&rig, &run);
                    assert!(
                        matches!(
                            step,
                            ReconcileStep::Skipped { .. } | ReconcileStep::Unchanged
                        ),
                        "{name}: {step:?}"
                    );
                    assert!(!rig.finished(&run), "{name}");
                    assert_eq!(rig.events(&run).len(), events, "{name}");
                    assert!(calls(&rig).is_empty(), "{name}");
                    assert!(rig.sim.writes().is_empty(), "{name}");
                }
            }

            #[test]
            fn a_run_in_triage_is_not_judged_for_its_label_or_assignee() {
                let rig = Rig::new();
                let run = run_id(1);
                rig.step(&run, &format!("approved {LABELED_AT}"));
                rig.write(
                    &run,
                    Event::Claimed {
                        machine: rig.machine.to_string(),
                        github_user: LOGIN.into(),
                    },
                );
                rig.moved(&run, State::Ready, State::Triage);
                rig.sim.set_labels(&["factory:triage"]);
                rig.sim.set_assignees(&["mallory"]);
                assert_eq!(
                    pass(&rig, &run),
                    ReconcileStep::Skipped {
                        why: "not-building"
                    }
                );
                assert!(rig.sim.calls().is_empty());
            }

            #[test]
            fn a_closed_issue_is_a_closing_and_not_a_removed_label() {
                let (rig, run) = building_run();
                rig.sim.set_labels(&[]);
                rig.sim.set_closed(true);
                assert_eq!(pass(&rig, &run), ReconcileStep::Abandoned);
                assert!(rig.sim.writes().is_empty());
            }

            #[test]
            fn a_stop_cut_short_is_finished_once_from_the_ledger() {
                let (rig, run) = building_run();
                rig.sim.set_labels(&[]);
                rig.sim.fail("could not finish this issue", 1);
                assert!(reconcile(&rig.ctx(), &run).is_err());
                assert!(!rig.finished(&run));
                assert_eq!(
                    pass(&rig, &run),
                    ReconcileStep::Stopped {
                        reason: LABEL_REMOVED.into()
                    }
                );
                assert_eq!(stuck_comments(&rig), 1);
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
                assert_eq!(
                    rig.events(&run)
                        .iter()
                        .filter(|e| e.event == "finished")
                        .count(),
                    1
                );
            }

            #[test]
            fn a_failed_read_of_the_issue_is_an_error_with_nothing_written() {
                let (rig, run) = building_run();
                rig.sim.set_labels(&["bug"]);
                rig.sim.fail("title", 1);
                let events = rig.events(&run).len();
                assert!(reconcile(&rig.ctx(), &run).is_err());
                assert_eq!(rig.events(&run).len(), events);
                assert!(calls(&rig).is_empty());
                assert!(rig.sim.writes().is_empty());
            }

            #[test]
            fn a_lost_lease_stops_nothing() {
                let (mut rig, run) = building_run();
                rig.sim.set_labels(&["bug"]);
                rig.guard = Box::new(Lost);
                let error = reconcile(&rig.ctx(), &run).unwrap_err();
                assert!(matches!(error, RunError::LeaseLost(_)), "{error}");
                assert!(!rig.finished(&run));
                assert!(calls(&rig).is_empty());
            }
        }

        // ----- a stuck run's workspace is kept until the issue leaves `stuck` -----

        mod a_stuck_runs_workspace {
            use super::*;

            fn stuck_run() -> (Rig, RunId) {
                let (rig, run) = building_run();
                rig.write(&run, stop_line());
                advance(&rig.ctx(), &run).unwrap();
                assert!(rig.finished(&run));
                (rig, run)
            }

            fn archives(rig: &Rig) -> usize {
                calls(rig)
                    .iter()
                    .filter(|call| matches!(call, Call::Archive { .. }))
                    .count()
            }

            #[test]
            fn is_kept_while_the_issue_is_labelled_stuck() {
                let (rig, run) = stuck_run();
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
                for _ in 0..3 {
                    assert_eq!(pass(&rig, &run), ReconcileStep::Unchanged);
                }
                assert_eq!(archives(&rig), 0);
            }

            #[test]
            fn is_archived_by_force_once_the_label_leaves() {
                let (mut rig, run) = stuck_run();
                let events = rig.events(&run).len();
                rig.sim.set_labels(&["factory:ready"]);
                assert_eq!(
                    pass(&rig, &run),
                    ReconcileStep::Archived {
                        workspaces: vec![ws()]
                    }
                );
                assert_eq!(archives(&rig), 1);
                // The ledger of a finished run takes no line.
                assert_eq!(rig.events(&run).len(), events);
                // The fleet no longer shows it: nothing is asked and the issue is not read.
                rig.view = FleetView::builder().online(rig.machine).build();
                let reads = issue_reads(&rig);
                assert_eq!(pass(&rig, &run), ReconcileStep::Unchanged);
                assert_eq!(archives(&rig), 1);
                assert_eq!(issue_reads(&rig), reads);
            }

            #[test]
            fn is_archived_once_the_issue_is_closed() {
                let (rig, run) = stuck_run();
                rig.sim.set_closed(true);
                assert_eq!(
                    pass(&rig, &run),
                    ReconcileStep::Archived {
                        workspaces: vec![ws()]
                    }
                );
            }

            #[test]
            fn is_archived_when_the_issue_is_gone() {
                use crate::factory::github::{Answer, FakeTracker, Hiding};
                let (rig, run) = stuck_run();
                let gone = Hiding::new(
                    std::sync::Arc::new(FakeTracker::answering([Answer::fail(
                        1,
                        "HTTP 404: Not Found",
                    )])),
                    std::sync::Arc::new(|text: &str| text.to_owned()),
                );
                let ctx = RunContext {
                    tracker: &gone,
                    ..rig.ctx()
                };
                assert_eq!(
                    reconcile(&ctx, &run).unwrap(),
                    ReconcileStep::Archived {
                        workspaces: vec![ws()]
                    }
                );
            }

            #[test]
            fn a_refused_read_of_the_issue_is_an_error_and_keeps_it() {
                let (rig, run) = stuck_run();
                rig.sim.refuse("issue view");
                assert!(reconcile(&rig.ctx(), &run).is_err());
                assert_eq!(archives(&rig), 0);
            }

            #[test]
            fn a_failed_read_of_the_issue_keeps_it() {
                let (rig, run) = stuck_run();
                rig.sim.set_labels(&["factory:ready"]);
                rig.sim.fail("title", 1);
                assert!(reconcile(&rig.ctx(), &run).is_err());
                assert_eq!(archives(&rig), 0);
                assert!(matches!(pass(&rig, &run), ReconcileStep::Archived { .. }));
            }

            #[test]
            fn is_not_asked_of_a_machine_given_up_on_and_waits_for_one_that_does_not_answer() {
                let (mut rig, run) = stuck_run();
                rig.sim.set_labels(&["factory:ready"]);
                rig.view = cut_off(&rig, crate::factory::runs::resume::UNREACHABLE_FOR);
                assert_eq!(pass(&rig, &run), ReconcileStep::Unchanged);
                assert_eq!(archives(&rig), 0);
                rig.view = cut_off(&rig, Duration::from_secs(60));
                rig.worker
                    .on_archive(Err(crate::factory::worker::WorkerError::Unreachable(
                        "timed out".into(),
                    )));
                assert_eq!(
                    pass(&rig, &run),
                    ReconcileStep::Waiting {
                        workspaces: vec![ws()]
                    }
                );
            }

            #[test]
            fn a_run_ended_by_closing_its_issue_is_not_a_kept_stuck_run() {
                let (rig, run) = building_run();
                rig.sim.set_closed(true);
                assert_eq!(pass(&rig, &run), ReconcileStep::Abandoned);
                // The workspace is archived with the run, and nothing is left to sweep.
                assert_eq!(archives(&rig), 1);
            }

            #[test]
            fn a_finished_run_that_did_not_go_stuck_has_its_leftovers_archived() {
                let (mut rig, run) = pr_open_run();
                with_feedback_workspace(&mut rig, None);
                finish(&rig, &run);
                assert_eq!(
                    pass(&rig, &run),
                    ReconcileStep::Archived {
                        workspaces: vec![fws()]
                    }
                );
                assert_eq!(rig.sim.calls().len(), 0, "no read of GitHub for it");
            }
        }
    }

    // ----- a run of another repo -----

    mod a_run_of_another_repo {
        use super::*;

        #[test]
        fn is_skipped_before_anything_is_read_or_touched() {
            let mut rig = Rig::new();
            let own = run_id(1);
            built(&mut rig, &own);
            rig.write(&own, stop_line());
            let other = RunId::new("rawphp/other", 8, 1).unwrap();
            let reads = rig.sim.calls().len();
            assert_eq!(
                advance(&rig.ctx(), &other).unwrap(),
                StopStep::Skipped { why: "other-repo" }
            );
            assert_eq!(
                reconcile(&rig.ctx(), &other).unwrap(),
                ReconcileStep::Skipped { why: "other-repo" }
            );
            assert_eq!(rig.sim.calls().len(), reads);
            assert!(calls(&rig).is_empty());
            assert!(rig.events(&other).is_empty());
        }

        #[test]
        fn is_judged_by_its_repo_whatever_the_case_of_the_name() {
            let mut rig = Rig::new();
            let run = RunId::new("RawPHP/SoftFac", 8, 1).unwrap();
            built(&mut rig, &run);
            rig.write(&run, stop_line());
            assert!(matches!(
                advance(&rig.ctx(), &run).unwrap(),
                StopStep::Stuck { .. }
            ));
        }
    }
}

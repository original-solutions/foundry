//! Interruptions and resumes of a run.
//!
//! Ported from the runner's `src/runner/relaunch.ts` and the interruption parts of
//! `src/runner/building.ts` (`interrupted`, `resumesUsed`, `relaunchBranchOf`,
//! `carryOutRelaunch`, `branchRefused`), with `interruption.test.ts`.
//!
//! A session whose process ended before a result line (it is gone from the fleet, or it
//! finished or failed and its newest turn is not a `FACTORY-DONE`), or whose machine has been
//! unreachable for 15 minutes ([`UNREACHABLE_FOR`]), is an interruption. [`advance`] is one pass
//! over a run and never waits on a worker:
//!
//! 1. The first pass decides. With the cap used (`limits.interruption_resumes`) the run goes
//!    stuck. Otherwise the branch the resume continues on is decided now, before anything is
//!    asked: the open pull request's head, else the run's own branch when it is on `origin`,
//!    else none (a new branch from the base). A name that is not a plain branch name (`-x`, a
//!    space, `..`, a non-ASCII character) is refused as the runner's `branchRefused` refuses
//!    it, before any workspace is asked for. Then `session_interrupted` is written with the
//!    count and the branch, then the step `resume-archiving <k> <workspace>`, and then the
//!    earlier workspace's archive is asked for. The pass ends there ([`ResumeStep::Archiving`]):
//!    the archive and the new workspace are never asked for in one pass.
//! 2. A later pass goes on once the earlier workspace shows archived (no record of it that is
//!    not deleted) or its machine has been unreachable for 15 minutes. It returns
//!    [`ResumeStep::NeedsMachine`] until a machine is recorded: placement needs the shared
//!    settings, which a [`RunContext`] does not carry, so the pass places the run with
//!    `placing::place_run` and calls [`placed`], which records the machine as a step before it
//!    acts. The workspace is `fac/<N>-<attempt>-r<k>`, on the pull request's branch (the
//!    checkout `Branch`, which on the machine that built the run moves the local branch to
//!    `origin`'s) or on a new branch of its own name from the base. Its create is recorded
//!    (`resume-requested`) before it is asked for and its answer (`resume-workspace`) after;
//!    a create whose answer is recorded and whose record has not arrived is waited for.
//! 3. Then the setup terminal is waited for, a session the fleet already shows in the new
//!    workspace (one with a turn record) is adopted, and otherwise the session starts with
//!    [`messages::resumed`] (or [`messages::launch`] for a new branch), recorded before it is
//!    asked for (`resume-sending`) and in `session_launched` after, which also marks the
//!    interruption sent. The status comment names the machine and the session. The first
//!    message is asked for again by count when the session is idle with no prompt a pass
//!    later, under `resume-resending <k> <count>`.
//!
//! A claim whose named machine has been unreachable for 15 minutes with no workspace is carried
//! on the same way ([`Placing::MovedClaim`]): [`placed`] records `claim-moved <machine>`, updates
//! the status comment, asks for `fac/<N>-<attempt>` on the new machine with the checkout
//! `start` decided, and records `workspace-created <id>`, which `start` then finds by id. No
//! resume is spent.
//!
//! [`orphans`] lists the workspaces of a run that has moved on, on machines that are heard from
//! again, for the pass to archive by force.
//!
//! What the pass (task 30) must do with this step:
//!
//! - Call [`advance`] BEFORE `building::advance` and `start::advance`, every pass, for a run in
//!   `triage` or `building`, and for a run in `stuck` whose stuck path is not finished while a
//!   resume is owed: while an interruption is owed `building` skips the run, a claim that is
//!   being moved must not be asked of its old machine by `start`, and `building` would
//!   otherwise finish a decision of this step's codes without its rules for a machine that is
//!   gone and for the session's text.
//! - Leave a run whose newest launch is a resume to this step's resend: `start::advance`'s
//!   resend of the launch would send the build's launch words.
//! - Place with `placing::place_run` when [`NeedsMachine`](ResumeStep::NeedsMachine) is
//!   returned, and call [`placed`] with the machine, or call nothing when none qualifies.
//! - Not call this for a run the gate holds: a run with a recorded pass, or a pull request
//!   reported since it last entered `building`, is the gate's (`gating`, task 27, also owns a
//!   session that died after its `FACTORY-DONE`). The one exception is what
//!   [`takes_from_the_gate`] says (#367): a session with its pull request reported and no
//!   recorded pass, shown unreachable and not at rest (`gating::at_rest`) on a machine
//!   unreachable for 15 minutes, may be at work there and is an interruption as any other.
//!   The pass calls [`advance`] for it before it polls a gate in flight. The resume that
//!   interruption owes, the resumed session until a turn of it is read for anything but its
//!   reminder (its first message asked again, its end an interruption of its own), and a stuck
//!   path it began stay this step's.
//!   A gate in flight is marked could not run (`gate-could-not-run`) before the interruption
//!   is recorded, since the gate reads no gate while the run is this step's: on a machine gone
//!   for `gate::MACHINE_GONE_AFTER` in the gate's own words, and on one that is not gone with
//!   [`GATE_CUT_OFF`], its workspace archived by force. A session at rest when last seen stays
//!   the gate's (#294).
//!
//! Where it differs from the runner:
//!
//! - The runner relaunches in the next attempt (a new run, a new ledger, `resume`) or sends the
//!   same thread a message. Foundry resumes in a new workspace of the same run, so the runner's
//!   attempt+1 mechanics (`carryOutRelaunch`, `endUnbegunRelaunch`, `carryStopToNext`, a worktree
//!   removed with `worktree_removed`, the next attempt's approval) have no counterpart. The
//!   count on a `session_interrupted` line (`resumes`, which names `-r<k>`) is the run's whole
//!   count, while the cap counts only the lines after the newest `feedback_relaunching`: a
//!   feedback round starts `interruption_resumes` again (task 28), and no name is reused.
//! - A session is read by the fleet, not by a thread's status: there is no T3 evidence (a T3
//!   error in the machine log, `endedInPass`, T3 starts and stops), so a session that ended is
//!   resumed up to the cap with none, where the runner needed evidence or a second end in one
//!   pass. A session the view does not show is gone only when its workspace is not shown either:
//!   a session record syncs apart from its workspace's, so one that has not arrived yet is not
//!   an interruption (the 15-minute rule still applies through its machine). A turn record
//!   syncs apart from its session's too, and its machine can write it after the session ended:
//!   a session that finished or failed with no unread turn shown is first asked of the worker
//!   for the turn after the newest read. A turn the worker answers is written and not shown yet,
//!   so nothing is decided until it shows (`building` reads it then); one the worker says was
//!   never written (`NotWritten`) is the interruption; any other error of the call decides
//!   nothing, and the next pass asks again. A call that answers `Failed` without a break for
//!   [`TURN_FAILING_FOR`] (an owner that runs an older Foundry, or one whose answer is not
//!   JSON, fails the same way on every pass) sends the run stuck for a person
//!   ([`TURN_UNREADABLE`]): no resume is spent, and nothing more is asked of the owner (no
//!   archive, no sessions ended, since the session has ended), which would refuse it the same
//!   way. The workspace is kept, as any stuck run's is, until the issue leaves `stuck`. Its first
//!   failure is recorded before the error is returned (`resume-turn-failing <session> <turn>
//!   <generation> <ms>`), on the sleep-counting clock as gating's `checks-waiting` is, so a
//!   daemon restart keeps it and a takeover or a reset clock starts the count again. Any other
//!   answer to the same probe, or the fleet showing the session unreachable, clears it
//!   (`resume-turn-answered <session> <turn>`).
//! - Before a resumed session starts, the issue text is checked again against the approval the
//!   ledger recorded, as the runner's `textStop` does: a text changed by someone who is not an
//!   instructor, or whose edits cannot be judged, ends the run stuck and starts nothing. The
//!   approval itself is not read again, as `start` does before a launch: a building issue never
//!   has `factory:ready` in force (the claim took it off), so the read would refuse every
//!   resume. An issue text edited since the approval is not resumed from a branch, as `start`'s
//!   own `resume_from` does (an open pull request's head still is). The refusal is stuck, not
//!   `start`'s withdraw: the resume workspace is kept, as a stuck run's is.
//!
//! - A run with no recorded approval goes stuck as `start` does ([`messages::CANNOT_CONFIRM`])
//!   in place of the runner's `thread-gone`, since a resume starts a session; the session's
//!   end is the detail of the stop in the ledger.
//! - A pull request from a fork is not resumed from, and the resume continues from the run's own
//!   branch or the base; a same-repo pull request whose head is not a plain branch name is
//!   refused (review focus 2), where the runner started from the base. The runner's
//!   `pr_left` line, which recorded a pull request a stuck run left, is not written (building's
//!   doc says so too).
//! - A resume with no branch (a new branch from the base) starts with [`messages::launch`], not
//!   [`messages::resumed`]: the resumed paragraph tells the session "the worktree already holds
//!   its commits", which is false for a new branch from the base. This is the runner's
//!   behaviour too.
//! - A session on a machine that stays unreachable cannot be asked to end its sessions: a run
//!   stuck, abandoned or ended for it names its machine ([`MACHINE_UNREACHABLE`]) and asks
//!   nothing of it. The machine ends its own factory sessions (G10) when it is heard from again,
//!   and [`orphans`] archives its workspaces. While the earlier workspace shows on a machine
//!   that is heard from, the archive is asked again every pass, and the new workspace is never
//!   asked for in a pass that asked it.
//! - A machine recorded for a resume or a moved claim that is unreachable for 15 minutes before
//!   the session started is given up (`resume-machine-given-up`, `claim-moved-given-up`) and
//!   placement picks another, spending no resume. [`placed`] refuses a machine given up only
//!   while it is not heard from: placement cannot be told to leave it out, so once it is online
//!   again it is accepted, and the workspace it made of that name is found by its owner. A claim
//!   whose own machine is heard from again after a target was given up is left to `start`.
//! - A stuck path this step began and that was cut short is finished here, from the ledger, as
//!   building finishes its own, at any cut point after its decision (`stuck-stop`), up to a
//!   `state_changed` to stuck with no `finished`; a lone `stuck-detail` decided nothing and is
//!   decided again. It keeps the first try's words (the detail, the setup's tail, the session's
//!   last turn read again) and ends the sessions of the workspace the decision used: the
//!   resume workspace when one was made, else the old one, and none on a machine unreachable
//!   for 15 minutes. Ownership: resume finishes a decision of its own codes (named for this step
//!   and shared with no other: `start`'s are `launch-failed` and `approval-unconfirmed`) and, while
//!   a resume is owed, any decision, since `building` skips such a run; otherwise the step that
//!   began it does.
//! - A stuck run's workspace is kept for a person to look at, so [`orphans`] does not list it.
//! - The moved claim's lines are written with the cause step `advance`, as the rest of this
//!   module's: `start` reads no cause step.
//! - The resume and feedback names looked for in the fleet are one past the highest the run's own
//!   ledger names (`session_interrupted`'s count, `feedback_relaunching`'s number), so the cost
//!   follows what the run did and no configured cap widens it.
//!
//! The runner cases of `interruption.test.ts` that have no Rust test of the same name, each with
//! its reason. The cases not listed are ported under their names.
//!
//! `advance: a session that ended badly` (building.rs ports none of these):
//! - `still sends a rolled back run to stuck by itself`: no `rolled_back` status in P1.
//! - `sends an ended badly run with no ordinal to stuck at once, since it cannot be waited
//!   for`: Foundry has no run ordinals; a turn always has a number.
//!
//! `advance: a pause by hand` (a person pausing a T3 thread; P1 has no such status, and a stop
//! asked for is the stop step's):
//! - `pauses an interrupted run by hand and does not call it ended badly`
//! - `does not decide a paused run as an interruption even with T3 trouble`
//! - `reads a stopped run that a person resumes in T3 as any other`
//! - `pauses a second stop after a resume again with its own line`
//! - `lifts the pause with an instructor's issue comment through forward`
//!
//! `interrupted: a resume`:
//! - `resumes two sessions that ended badly in one pass without any T3 trouble`: no T3
//!   evidence; one end is enough (the first case covers it).
//! - `sends a lone failed session with T3 healthy to stuck with the old words`, `sends a
//!   cancelled session with T3 healthy to stuck as cancelled`: the runner's T3 evidence rule.
//! - `records the pull request a session that ended badly left on its branch`, `ends a session
//!   that ended badly as stuck even when GitHub cannot list its pull requests`: `pr_left` is
//!   not written.
//!
//! `interrupted: the cap`:
//! - `used up records the pull request a gone thread left, by the ledger's branch`: `pr_left`.
//!
//! `interrupted: a T3 restart` (T3 server starts and stops and its machine log are the runner's
//! evidence; Foundry has no T3):
//! - `resumes a lone cancelled run that was running when the server started`
//! - `records the machine log's error over the start when it has one`
//! - `resumes a run a graceful shutdown ended just before the next start`
//! - `resumes a run an earlier start cut off when the server has started again since`
//! - `sends a run that ended more than ten minutes before a start with no stop on record to
//!   stuck`
//! - `resumes a run the shutdown of a server that stayed down for long cut off`
//! - `resumes a run cut off by a shutdown written more than a day before the start`
//! - `does not count a stop that belongs to an earlier restart`
//! - `sends a run that started after the server did to stuck: a person or the provider ended
//!   it`
//! - `sends a run that ended before the server started to stuck`
//! - `resumes a run the shutdown cancelled before it had started`
//! - `sends a run requested and cancelled after the server started to stuck`
//! - `sends a run that failed before it had started to stuck, even just before a shutdown`
//! - `does not relaunch a thread that is gone on the start alone`
//!
//! `interrupted: a send cut short`:
//! - `is sent again under the same command id`: there is no resume message and no command id;
//!   the cut-short archive is `a_decision_cut_short_before_its_archive_asks_it_on_the_next_pass_without_deciding_again`.
//!
//! `interrupted: a thread that is gone`:
//! - `gives the next attempt this run's approval, with the instructor who made it`: no next
//!   attempt.
//! - `with no evidence goes stuck as thread-gone`, `twice in one pass is relaunched without any
//!   T3 trouble`: the runner's T3 evidence rule.
//! - `found by a send into it relaunches and counts the resume once`: no resume message is
//!   sent.
//! - `does not pass the resumes of a stuck run to a new attempt a person relabelled`: each
//!   attempt has a ledger of its own, which this step reads alone; nothing here can carry a
//!   count over.
//!
//! `a decided relaunch` (a relaunch's next attempt, its label and the time limit, the watch and
//! forward skipping it; `building` skips a run with an interruption owed, and the stop step owns
//! a stop by hand):
//! - `is left to advance by the time limit`
//! - `is left to advance by the watch even when the text changed`
//! - `is left to advance by forward`
//! - `is finished by the pass's steps in order on a run past its limit`
//! - `does not reset the label of a next attempt that already started`
//! - `overtaken by a stop by hand ends the next attempt it made, so it holds no slot`
//! - `overtaken by a stop by hand leaves a next attempt that already started alone`
//!
//! `a relaunch finds the branch` (T3's thread and `worktree_removed`; Foundry's workspace is
//! made on the branch):
//! - `an archived thread still reports when the ledger names none`
//! - `and removes the worktree an archived thread still reports`
//!
//! `interrupted: a run with no recorded approval`:
//! - `whose thread is gone after a T3 outage goes stuck as thread-gone, not relaunched`,
//!   `found gone by its resume send goes stuck as thread-gone too`: Foundry's counterpart is
//!   `a_gone_session_with_no_recorded_approval_is_stuck_unconfirmed_naming_its_end` (stuck as
//!   unconfirmed, with the session's end as the detail), since the stuck reason differs.
//! - `with a relaunch line in its ledger is an error for advance, with nothing written`: the
//!   runner's next-attempt error; a ledger with a relaunch line is skipped.
//!
//! `the time limit`:
//! - `stops a run past limits.minutes, and the next attempt resumes from the branch it
//!   recorded`: the limit is `building::time_limit`'s (its tests are named for Foundry's
//!   words) and the continuation is `start`'s (task 22).
//!
//! `interrupted: a stop by hand`:
//! - `that lands between the ledger read and the thread read is not a relaunch`: one read of the
//!   ledger and one of the fleet; a stop that lands later is the next case.
//!
//! `a provider rate limit` (rate limits, cooldowns, `retry-after` and moving a thread to another
//! model are the runner's; P1 marks a session stuck and the factory leaves it to
//! `limits.minutes`, as building's doc says):
//! - `is told apart from a session that ended badly, and nothing is written`
//! - `is resumed after the cooldown without using an interruption resume`
//! - `keeps the count of an earlier interruption on its line`
//! - `cut short before its send is sent again under the same command id`
//! - `is not resumed once the thread has moved on, and nothing is written`
//! - `moved to another model sends the resume on it, and the run is on that model after`
//! - `moved to another model and cut short before its send is sent again on that model`
//! - `takes the provider's retry-after from the error`
//! - `with no end from T3 is placed at its start, else long ago, never at the clock`
//! - `is not resumed once a stop by hand is requested, and nothing is written`
//! - `into a thread that is gone writes nothing, and its send gone leaves the resume owed`
//! - `owed when its thread is gone charges the relaunch nothing`
//! - `moved to another model keeps the session's provider options`
//! - `is not resumed once a relaunch is decided, and nothing is written`

use std::collections::BTreeSet;
use std::time::Duration;

use foundry_proto::factory::{RunEvent, RunId};
use foundry_proto::ids::{MachineId, ProjectId, SessionId, WorkspaceId};
use foundry_proto::record::{Record, Workspace};
use foundry_proto::state::{SessionState, TerminalState};
use serde_json::Value;

use super::gating::{self, GATE_COULD_NOT_RUN, GATE_CREATED};
use super::progress::{
    GATE_REQUESTED, RESULT_LINE_REMINDING, STUCK_DETAIL, STUCK_STOP, Seen, TEXT_CODE, TURN_READ,
    UNREACHABLE_FOR, UNVERIFIED_CODE, WORKSPACE_CREATED, WORKSPACE_REQUESTED, build_name,
    feedback_name, highest, numbered, pending_relaunch, read_progress, resume_name, step_text,
};
use super::start::{FIRST_PROMPT_COUNT, SETUP_FAILED};
use super::{
    Closing, Progress, RunContext, RunError, Stop, StuckStop, abandon, closed_issue, end_own_merge,
    finish_building, go_stuck, progress_from, recorded_reason, session_id, workspace_id,
};
use crate::factory::gate;
use crate::factory::github::read::is_safe_branch_name;
use crate::factory::github::{OtherKind, TrackerError};
use crate::factory::ledger::events::{
    Cause, CauseStep, Event, ModelOption, OptionValue, ResumedFrom, names,
};
use crate::factory::lifecycle::State;
use crate::factory::lifecycle::approval::TextCheck;
use crate::factory::lifecycle::result_line::{self, SessionResult};
use crate::factory::messages::{
    self, CANNOT_CONFIRM, CANNOT_VERIFY_DURING_BUILD, CHANGED_DURING_BUILD, LAUNCH_FAILED, Message,
    ResumeFrom, SESSION_GONE, prepare_output_tail, prepare_session_text,
};
use crate::factory::text;
use crate::factory::worker::{self, CreateWorkspace, FleetView, Target, WorkerError};
use crate::sessions::factory::{BODY_PATH, FactoryStart, SendRefused};
use crate::workspaces::{Checkout, FactoryTag, Setup};

/// The public reason of a run whose session's machine stayed unreachable.
pub const MACHINE_UNREACHABLE: &str = "the session's machine has been unreachable for 15 minutes";

/// The public reason of a run whose session ended and whose next turn could not be read for
/// [`TURN_FAILING_FOR`]. The call's error stays in the ledger.
pub const TURN_UNREADABLE: &str =
    "the session's last turn could not be read from its machine for 15 minutes";

/// The detail of the stop of a run whose pull request's branch is not a plain branch name: the
/// runner's `branchRefused` words.
pub const BRANCH_REFUSED: &str = "the pull request's branch is not a plain branch name";

/// The step written before the earlier workspace's archive is asked for:
/// `resume-archiving <k> <workspace id or none>`.
pub const ARCHIVING: &str = "resume-archiving ";
/// The step written once the archive was asked for and answered, or the machine could not be
/// reached: `resume-archive-asked <k>`.
pub const ARCHIVE_ASKED: &str = "resume-archive-asked ";
/// The step that records the machine the resume workspace is made on: `resume-machine <k> <id>`.
pub const MACHINE: &str = "resume-machine ";
/// The step written before the create: `resume-requested <k> <name>`.
pub const REQUESTED: &str = "resume-requested ";
/// The step written once the worker answered the create: `resume-workspace <k> <id>`.
pub const WORKSPACE: &str = "resume-workspace ";
/// The step written before the sessions of the new workspace are ended ahead of a second start:
/// `resume-ending-sessions <k>`.
pub const ENDING_SESSIONS: &str = "resume-ending-sessions ";
/// The step written before the session is started: `resume-sending <k>`.
pub const SENDING: &str = "resume-sending ";
/// The step written before the first message is asked for again: `resume-resending <k> <count>`.
pub const RESENDING: &str = "resume-resending ";
/// The step written when the machine recorded for the resume was unreachable for 15 minutes
/// before the session started: `resume-machine-given-up <k> <id>`. Nothing ran, so no resume is
/// spent, and placement picks another.
pub const MACHINE_GIVEN_UP: &str = "resume-machine-given-up ";
/// The step that records the machine a claim moved to: `claim-moved <id>`.
pub const CLAIM_MOVED: &str = "claim-moved ";
/// The step written when the machine a claim moved to was unreachable for 15 minutes before its
/// workspace was answered: `claim-moved-given-up <id>`.
pub const CLAIM_GIVEN_UP: &str = "claim-moved-given-up ";

/// Why a gate in flight on a machine that is not gone could not run when the interruption of
/// its run's session stopped it: the words of its `gate-could-not-run` step.
pub const GATE_CUT_OFF: &str = "the build session was interrupted";

/// The step written before a turn probe's first `Failed` answer is acted on:
/// `resume-turn-failing <session> <turn> <generation> <ms>`, with the lease generation and the
/// sleep-counting clock's reading it was read under. A probe that keeps failing for
/// [`TURN_FAILING_FOR`] from it sends the run stuck ([`TURN_UNREADABLE`]).
pub const TURN_FAILING: &str = "resume-turn-failing ";
/// The step written when the probe answers anything but `Failed`, or the fleet shows the
/// session unreachable, while a first failure is on record, which clears it:
/// `resume-turn-answered <session> <turn>`.
pub const TURN_ANSWERED: &str = "resume-turn-answered ";
/// How long a turn probe answers `Failed` without a break before the run goes stuck for a
/// person: the 15 minutes an unreachable machine is waited for, on the same
/// sleep-counting clock. An owner that runs an older Foundry, or answers with a body that is
/// not JSON, fails the same way on every pass, and the run would otherwise never be decided.
pub const TURN_FAILING_FOR: Duration = UNREACHABLE_FOR;

/// What one pass of [`advance`] or [`placed`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeStep {
    /// Not this step's run, or no interruption: nothing was written.
    Skipped { reason: String },
    /// The interruption is recorded and the earlier workspace's archive was asked for. The
    /// new workspace is asked for in a later pass.
    Archiving {
        resumes: u32,
        workspace: Option<WorkspaceId>,
    },
    /// Something is not there yet: look again next pass.
    Waiting(Wait),
    /// The pass must place the run and call [`placed`].
    NeedsMachine(Placing),
    /// The session runs in the resume workspace.
    Started { resumes: u32, session: SessionId },
    /// The first message was asked for again by count.
    Resent { session: SessionId },
    /// A claim was launched on another machine: its workspace was asked for, and `start`
    /// carries on from it.
    Moved {
        machine: MachineId,
        workspace: WorkspaceId,
    },
    /// The run went stuck; `reason` is the public one.
    Stuck { reason: String },
    /// A person closed the issue: the run is ended.
    Abandoned,
    /// The run's own merged pull request closed the issue: the run is ended.
    Ended,
}

/// What a pass that returned waiting is waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// The earlier workspace still shows, and its machine has not been unreachable for long.
    Archive,
    /// The worker answered the create and the fleet does not show the record yet.
    Workspace,
    /// The setup terminal is running, or its exit is not known.
    Setup,
}

/// What the pass places a run for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placing {
    /// The k-th resume workspace.
    Resume { resumes: u32 },
    /// The build workspace of a claim whose machine is unreachable.
    MovedClaim { unreachable: MachineId },
}

/// One pass over `run`, which builds or is claimed. See the module's words.
pub fn advance(ctx: &RunContext<'_>, run: &RunId) -> Result<ResumeStep, RunError> {
    pass(ctx, run, None)
}

/// [`advance`] after placement picked `target` for what [`ResumeStep::NeedsMachine`] asked.
/// The target is recorded as a step before anything is asked of it, and a target already
/// recorded for the resume stands. A call when nothing needs a machine is skipped, and a claim
/// is never moved to the machine it is unreachable on.
pub fn placed(ctx: &RunContext<'_>, run: &RunId, target: Target) -> Result<ResumeStep, RunError> {
    pass(ctx, run, Some(target))
}

/// The workspaces of `run` that it has moved on from, which `view` shows on machines that are
/// online now: the pass has each archived by force, for a machine that is heard from again.
///
/// It takes the project and the run's events, not the run alone, since which workspace is the
/// active one is in the ledger: the newest session's (`session_launched`), the build workspace
/// before a session starts (`workspace-created`) and the resume workspace being made. A name
/// whose workspace is being asked for (its create requested, its answer not recorded) is
/// never an orphan. The names are the build's, the resumes' and the feedback rounds'; a gate's
/// is the gate's. A feedback workspace is an orphan only when a higher-numbered one shows,
/// since which one is active is the feedback step's (task 28) to record. Every workspace of a
/// run that ended is an orphan, except those of a stuck run, which are kept to be looked at.
/// It reads no worker and no clock.
pub fn orphans(
    view: &FleetView,
    project: ProjectId,
    run: &RunId,
    events: &[RunEvent],
) -> Vec<WorkspaceId> {
    if events.is_empty() {
        return Vec::new();
    }
    let seen = read_progress(events);
    let round = read_round(events);
    let everything = seen.finished && !is_stuck(&seen);
    let mut active = BTreeSet::new();
    let mut being_asked: BTreeSet<String> = BTreeSet::new();
    if !everything {
        active.extend(
            seen.worktree
                .as_deref()
                .and_then(|id| workspace_id(id).ok()),
        );
        if seen.thread_id.is_none() {
            match seen
                .workspace
                .as_deref()
                .and_then(|id| workspace_id(id).ok())
            {
                Some(id) => {
                    active.insert(id);
                }
                None => {
                    being_asked.extend(seen.workspace_requested.clone());
                }
            }
        }
        if let (Some(interruption), Some(round)) = (&seen.interruption, &round)
            && !interruption.sent
            && !interruption.relaunch
        {
            match round.workspace {
                Some(id) => {
                    active.insert(id);
                }
                None if round.requested => {
                    being_asked.insert(resume_name(run, round.k));
                }
                None => {}
            }
        }
    }
    let scan = scan_of(events);
    let named = named_workspaces(view, project, run, scan);
    let newest_feedback = named
        .iter()
        .filter_map(|(kind, _)| match kind {
            Kind::Feedback(k) => Some(*k),
            _ => None,
        })
        .max();
    named
        .into_iter()
        .filter(|(kind, record)| {
            let id = record.body.id;
            let superseded = match kind {
                Kind::Feedback(k) => {
                    everything || newest_feedback.is_some_and(|newest| *k < newest)
                }
                Kind::Build | Kind::Resume(_) => true,
            };
            superseded
                && !active.contains(&id)
                && !being_asked.contains(&record.body.name)
                && view.online(record.owner)
        })
        .map(|(_, record)| record.body.id)
        .collect()
}

/// A workspace the run names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Build,
    Resume(u32),
    Feedback(u32),
}

/// Every workspace of the project the fleet shows that is not archived and has one of the
/// run's build, resume or feedback names, in the order of the names.
fn named_workspaces<'a>(
    view: &'a FleetView,
    project: ProjectId,
    run: &RunId,
    scan: Scan,
) -> Vec<(Kind, &'a Record<Workspace>)> {
    let mut found = Vec::new();
    found.extend(
        view.workspaces_named(project, &build_name(run))
            .into_iter()
            .map(|record| (Kind::Build, record)),
    );
    for k in 1..=scan.resumes {
        found.extend(
            view.workspaces_named(project, &resume_name(run, k))
                .into_iter()
                .map(|record| (Kind::Resume(k), record)),
        );
    }
    for k in 1..=scan.feedback {
        found.extend(
            view.workspaces_named(project, &feedback_name(run, k))
                .into_iter()
                .map(|record| (Kind::Feedback(k), record)),
        );
    }
    found
}

/// The record of the workspace `id` of the run, when the fleet shows it and it is not archived.
fn find_workspace<'a>(
    view: &'a FleetView,
    project: ProjectId,
    run: &RunId,
    scan: Scan,
    id: WorkspaceId,
) -> Option<&'a Record<Workspace>> {
    named_workspaces(view, project, run, scan)
        .into_iter()
        .map(|(_, record)| record)
        .find(|record| record.body.id == id)
}

/// The record of the resume workspace `round` recorded the answer of, when the fleet shows it.
fn resume_record<'a>(
    view: &'a FleetView,
    project: ProjectId,
    run: &RunId,
    round: &Round,
) -> Option<&'a Record<Workspace>> {
    let id = round.workspace?;
    view.workspaces_named(project, &resume_name(run, round.k))
        .into_iter()
        .find(|record| record.body.id == id)
}

/// Whether the run ended stuck, or is stuck: its workspace is kept.
fn is_stuck(seen: &Seen) -> bool {
    seen.state == Some(State::Stuck) || seen.stuck.is_some() || seen.stuck_stop.is_some()
}

/// How many resume and feedback names of a run are looked for in the fleet, which finds a
/// workspace by name: one past the highest count the run's own ledger names, so the newest
/// resume's (or round's) workspace is always among them. Nothing configured widens it.
#[derive(Clone, Copy)]
struct Scan {
    resumes: u32,
    feedback: u32,
}

/// The names of `events`' run that can exist.
fn scan_of(events: &[RunEvent]) -> Scan {
    Scan {
        resumes: highest(events, names::SESSION_INTERRUPTED, "resumes").saturating_add(1),
        feedback: highest(events, names::FEEDBACK_RELAUNCHING, "n").saturating_add(1),
    }
}

/// What a session that is not running was found to be.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Ended {
    /// The fleet shows no such session.
    Gone,
    /// Its process ended as `what` says, with `text` of its newest unread turn.
    Process {
        what: &'static str,
        text: Option<String>,
    },
    /// Its machine has been unreachable for 15 minutes.
    Unreachable,
    /// Its process ended and the turn after the newest read could not be read for
    /// [`TURN_FAILING_FOR`]: the call's newest error.
    TurnUnreadable(String),
}

impl Ended {
    /// The code of the stuck path and what the run says when the cap is used.
    fn stop(&self) -> (&'static str, Stop) {
        match self {
            Ended::Gone => (STUCK_GONE, Stop::new(SESSION_GONE)),
            Ended::Process { what, text } => (
                STUCK_ENDED,
                Stop {
                    reason: format!("the session run ended as {what}"),
                    session_text: text.clone(),
                    ..Stop::default()
                },
            ),
            Ended::Unreachable => (STUCK_MACHINE, Stop::new(MACHINE_UNREACHABLE)),
            Ended::TurnUnreadable(error) => (
                STUCK_TURN,
                Stop {
                    reason: TURN_UNREADABLE.into(),
                    detail: Some(format!("{TURN_UNREADABLE}: {error}")),
                    ..Stop::default()
                },
            ),
        }
    }
}

/// Where the branch of a resume comes from.
enum Branch {
    /// Continue on this branch, or on a new branch from the base when none.
    Use(Option<String>),
    /// The pull request's branch is not one a command can be handed.
    Refused,
}

/// What the ledger records of the resume the newest `session_interrupted` line owes.
#[derive(Debug, Default)]
pub(super) struct Round {
    pub(super) k: u32,
    /// The workspace the archive is of, as written: an id or `none`.
    archiving: Option<String>,
    archive_asked: bool,
    /// The target recorded for the resume and not given up.
    target: Option<Target>,
    /// The machines recorded for it that were given up.
    gave_up: Vec<MachineId>,
    requested: bool,
    pub(super) workspace: Option<WorkspaceId>,
    /// `resume-sending <k>` is recorded since the machine.
    pub(super) sending: bool,
    /// `resume-ending-sessions <k>` is recorded since the machine: a start may have gone out,
    /// as `stop` reads it.
    pub(super) ending_sessions: bool,
    /// A session was launched since, and no launch came after it.
    launched: bool,
}

fn parse_machine(text: &str) -> Option<MachineId> {
    uuid::Uuid::parse_str(text).ok().map(MachineId)
}

/// The newest `session_interrupted` line that is a resume, and the steps after it.
pub(super) fn read_round(events: &[RunEvent]) -> Option<Round> {
    let at = events.iter().rposition(|event| {
        event.event == names::SESSION_INTERRUPTED
            && event.data.get("relaunch") != Some(&Value::Bool(true))
    })?;
    let k = events[at]
        .data
        .get("resumes")
        .and_then(Value::as_u64)
        .and_then(|k| u32::try_from(k).ok())?;
    let mut round = Round {
        k,
        ..Round::default()
    };
    let (mut first, mut last) = (None, None);
    for (index, event) in events.iter().enumerate().skip(at + 1) {
        if event.event == names::SESSION_LAUNCHED {
            first.get_or_insert(index);
            last = Some(index);
            continue;
        }
        let Some(step) = step_text(event) else {
            continue;
        };
        if let Some(rest) = numbered(step, ARCHIVING, k) {
            round.archiving = Some(rest.to_owned());
        } else if numbered(step, ARCHIVE_ASKED, k).is_some() {
            round.archive_asked = true;
        } else if let Some(rest) = numbered(step, MACHINE_GIVEN_UP, k) {
            // What the machine was asked is its own: the next machine starts the round again.
            round.gave_up.extend(parse_machine(rest));
            round.target = None;
            round.requested = false;
            round.workspace = None;
            round.sending = false;
            round.ending_sessions = false;
        } else if let Some(rest) = numbered(step, MACHINE, k) {
            round.target = rest.parse().ok();
            round.requested = false;
            round.workspace = None;
            round.sending = false;
            round.ending_sessions = false;
        } else if numbered(step, REQUESTED, k).is_some() {
            round.requested = true;
        } else if let Some(rest) = numbered(step, WORKSPACE, k) {
            round.workspace = uuid::Uuid::parse_str(rest).ok().map(WorkspaceId);
        } else if numbered(step, SENDING, k).is_some() {
            round.sending = true;
        } else if numbered(step, ENDING_SESSIONS, k).is_some() {
            round.ending_sessions = true;
        }
    }
    round.launched = first.is_some() && first == last;
    Some(round)
}

/// The machine a claim moved to since its workspace was last asked for, and the machines it
/// was moved to and given up.
fn moved_to(events: &[RunEvent]) -> (Option<MachineId>, Vec<MachineId>) {
    let (mut moved, mut gave_up) = (None, Vec::new());
    for step in events.iter().filter_map(step_text) {
        if step.starts_with(WORKSPACE_REQUESTED) {
            moved = None;
            gave_up.clear();
        } else if let Some(machine) = step.strip_prefix(CLAIM_MOVED) {
            moved = parse_machine(machine);
        } else if let Some(machine) = step.strip_prefix(CLAIM_GIVEN_UP)
            && let Some(machine) = parse_machine(machine)
        {
            gave_up.push(machine);
            if moved == Some(machine) {
                moved = None;
            }
        }
    }
    (moved, gave_up)
}

/// `text` prepared as session text, or `None` when nothing is left of it.
fn prepared(text: &str) -> Option<String> {
    let text = prepare_session_text(text);
    (!text.is_empty()).then_some(text)
}

/// Whether `machine` has been unreachable for 15 minutes.
fn unreachable_long(ctx: &RunContext<'_>, machine: MachineId) -> bool {
    ctx.view.unreachable_for(machine) >= UNREACHABLE_FOR
}

fn skipped(run: &RunId, why: &str) -> Result<ResumeStep, RunError> {
    Ok(ResumeStep::Skipped {
        reason: format!("run {run} {why}"),
    })
}

/// A step recorded, with the lease checked first so that a lost lease writes nothing.
fn note(ctx: &RunContext<'_>, run: &RunId, step: &str, reason: &str) -> Result<(), RunError> {
    ctx.held()?;
    ctx.record_step(run, step, &Cause::new(CauseStep::Advance, reason))
}

/// Why a run is not this step's, or `None` for one that is.
fn not_this_steps(ctx: &RunContext<'_>, run: &RunId, seen: &Seen) -> Option<String> {
    let reason = if run.repo() != ctx.repo() {
        "is of another repo"
    } else if seen.finished {
        "is finished"
    } else if seen.stop_requested {
        "was asked to stop, which the stop step carries out"
    } else if seen.questions_posted || seen.questions_commenting.is_some() || seen.withdrawn {
        "has questions posted or an approval withdrawn, which building and start finish"
    } else if seen
        .interruption
        .as_ref()
        .is_some_and(|interruption| interruption.relaunch || interruption.rate_limited)
        || pending_relaunch(seen).is_some()
    {
        "has a relaunch or a rate limit decided, which are the runner's"
    } else {
        return match seen.state {
            Some(State::Triage | State::Building) => None,
            // A stuck path cut short after its state change is finished here, or left to the
            // step that began it.
            Some(State::Stuck) if seen.stuck.is_some() || seen.stuck_stop.is_some() => None,
            state => Some(format!(
                "run {run} is in state {}, which another step owns",
                state.map_or("none", State::name)
            )),
        };
    };
    Some(format!("run {run} {reason}"))
}

/// One pass over `run`, with the machine placement picked when it was asked for one.
fn pass(ctx: &RunContext<'_>, run: &RunId, placed: Option<Target>) -> Result<ResumeStep, RunError> {
    let events = ctx.ledger.read(run)?;
    let seen = read_progress(&events);
    if let Some(reason) = not_this_steps(ctx, run, &seen) {
        return Ok(ResumeStep::Skipped { reason });
    }
    let mut at = progress_from(run, CauseStep::Advance, &seen, State::Building);
    let resume_owed = seen
        .interruption
        .as_ref()
        .is_some_and(|interruption| !interruption.sent);
    // A stuck path begun: the step that began it finishes it. Mine are finished here from the
    // ledger, as building finishes its own.
    // A decision is a `stuck-stop` line: a lone `stuck-detail` decided nothing, and is decided
    // again.
    if seen.stuck.is_some() || seen.stuck_stop.is_some() {
        if let Some(step) = finish_own_stuck(ctx, run, &events, &seen, &mut at, resume_owed)? {
            return Ok(step);
        }
        if !resume_owed {
            return skipped(
                run,
                "has a stuck path begun, which the step that began it finishes",
            );
        }
    }
    // A run in state stuck is never carried on with a workspace or a session.
    if seen.state == Some(State::Stuck) {
        return skipped(run, "is stuck, which the step that made it so finishes");
    }
    // A resume owed: the interruption is recorded and its launch is not.
    if let Some(owed) = seen
        .interruption
        .as_ref()
        .filter(|interruption| !interruption.sent)
    {
        return carry_on(
            ctx,
            run,
            &events,
            &seen,
            &mut at,
            owed.resumes,
            owed.branch.as_deref(),
            placed,
        );
    }
    if seen.thread_id.is_none() {
        return moved_claim(ctx, run, &events, &seen, placed);
    }
    if seen.state == Some(State::Triage) && !seen.rechecked {
        return skipped(run, "is launching, which start carries out");
    }
    // Nothing is owed, so no machine is needed.
    if placed.is_some() {
        return skipped(run, "needs no machine");
    }
    // The gate has the pull request the session reported, unless the session may be at work
    // on a machine cut off for 15 minutes: that is an interruption all the same (#367). A
    // session resumed since the report with no turn read yet (but for its reminder) is this
    // step's too.
    let with_the_gate = (seen.pr_reported && !resumed_unread(&events))
        || (seen.pass.is_some() && seen.last_pr.is_some());
    let ended = if !with_the_gate || cut_off_at_work(ctx, &seen) {
        ended(ctx, run, &events, &seen)?
    } else {
        None
    };
    match ended {
        Some(ended) => {
            // A gate in flight for the reported pull request, whoever holds the run now.
            if seen.pr_reported {
                gate_cut_off(ctx, run, &events)?;
            }
            decide(ctx, run, &events, &seen, &mut at, &ended)
        }
        None => resend(ctx, run, &events, &seen),
    }
}

/// Whether this step has a run whose pull request is reported, which the gate holds otherwise
/// (#367): its session may be at work on a machine unreachable for 15 minutes, it was
/// interrupted since the report (the resume is owed, or no turn of the resumed session has
/// been read but for its reminder, so its first message and its end are this step's, as for a
/// run with no pull request reported), or a stuck path this step began is to be finished. The pass calls [`advance`]
/// for it, and polls no gate in flight first, since one is not read while the session may be
/// at work. It reads no worker and writes nothing.
pub fn takes_from_the_gate(ctx: &RunContext<'_>, events: &[RunEvent]) -> bool {
    let seen = read_progress(events);
    let own_stuck = seen.stuck_stop.as_deref().is_some_and(|raw| {
        serde_json::from_str::<StuckStop>(raw)
            .is_ok_and(|decision| OWN_STUCK_CODES.contains(&decision.code.as_str()))
    });
    let resumed = seen.pr_reported && seen.pass.is_none() && resumed_unread(events);
    resumed || own_stuck || cut_off_at_work(ctx, &seen)
}

/// Whether a resume's `session_interrupted` line follows the newest `pr_opened` with no turn
/// of the session resumed since read for anything but its reminder: the session that reported
/// the pull request was interrupted, and the resumed one has not reported it. The run's
/// `pr_reported` stays set through the interruption, as `progress` reads the ledger, and
/// `building` writes no second `pr_opened` when the resumed session reports the same pull
/// request, only the turn read. A turn read for the reminder to end with a result line (its
/// `result-line-reminding <n>` written first) reports nothing. From any other turn read the run
/// is the gate's again, as any run whose session reported its pull request: one that put the
/// run in `needs-info` or `stuck` is `building`'s own.
fn resumed_unread(events: &[RunEvent]) -> bool {
    let newest = |found: &dyn Fn(&RunEvent) -> bool| events.iter().rposition(found);
    let Some(interrupted) = newest(&|event| {
        event.event == names::SESSION_INTERRUPTED
            && event.data.get("relaunch") != Some(&Value::Bool(true))
    }) else {
        return false;
    };
    let reported = newest(&|event| event.event == names::PR_OPENED);
    if reported.is_none_or(|reported| reported > interrupted) {
        return false;
    }
    let since: Vec<&str> = events[interrupted..].iter().filter_map(step_text).collect();
    let reminded: BTreeSet<&str> = since
        .iter()
        .filter_map(|step| step.strip_prefix(RESULT_LINE_REMINDING))
        .map(str::trim)
        .collect();
    !since
        .iter()
        .filter_map(|step| step.strip_prefix(TURN_READ))
        .any(|n| !reminded.contains(n.trim()))
}

/// Whether the session of a run whose pull request is reported and has no recorded pass may be
/// at work on a machine unreachable for 15 minutes: the fleet shows it unreachable, and not
/// at rest as the gate reads one (`gating::at_rest`), whose reported pull request the gate
/// takes (#294). A recorded pass is the gate's to finish, with no session.
fn cut_off_at_work(ctx: &RunContext<'_>, seen: &Seen) -> bool {
    if !seen.pr_reported || seen.pass.is_some() {
        return false;
    }
    let Some(session) = seen
        .thread_id
        .as_deref()
        .and_then(|thread| session_id(thread).ok())
    else {
        return false;
    };
    ctx.view.session_state(session) == Some(SessionState::Unreachable)
        && !gating::at_rest(seen, ctx.view.session_turn(session))
        && session_owner(ctx, seen).is_some_and(|machine| unreachable_long(ctx, machine))
}

/// The machine a session shown in the fleet runs on: its build workspace's session's owner,
/// else the machine the run was claimed on.
fn session_owner(ctx: &RunContext<'_>, seen: &Seen) -> Option<MachineId> {
    seen.worktree
        .as_deref()
        .and_then(|id| workspace_id(id).ok())
        .and_then(|workspace| ctx.view.session_of(workspace))
        .map(|record| record.owner)
        .or_else(|| seen.machine.as_deref().and_then(parse_machine))
}

/// A gate in flight when the session of a run whose pull request is reported is interrupted
/// (cut off at work, or a resumed session that ends while a gate placed for it runs) is marked
/// could not run before the interruption is recorded, with the gate's count of
/// failures to run for its attempt. The gate reads no gate while the session may be at work,
/// nor while the run is this step's after it, so one left in flight would hold its slot
/// unread. On a machine gone for `gate::MACHINE_GONE_AFTER` that is the gate's own rule, with
/// its words. On a machine that is not gone the gate may still be running its commands: it is
/// marked with [`GATE_CUT_OFF`] and its workspace (the one recorded, else the one the fleet
/// shows under the gate's name) is archived by force, which stops them. An archive that fails
/// is no error: the gate archives a gate that could not run, by its own rules, before it
/// places another. A fifth failure to run is the gate's stuck. A pass cut short after the step
/// decides the interruption again, and finds no gate in flight.
fn gate_cut_off(ctx: &RunContext<'_>, run: &RunId, events: &[RunEvent]) -> Result<(), RunError> {
    if !gating::gate_running(events) {
        return Ok(());
    }
    let steps = || events.iter().filter_map(step_text);
    let Some(placed) = steps()
        .filter_map(|step| step.strip_prefix(GATE_REQUESTED))
        .next_back()
    else {
        return Ok(());
    };
    let mut part = placed.split(' ');
    let (Some(attempt), Some(k)) = (
        part.next().and_then(|n| n.parse::<u32>().ok()),
        part.next().and_then(|n| n.parse::<u32>().ok()),
    ) else {
        return Err(RunError::Invalid(format!("the gate step {placed:?}")));
    };
    let gone = part
        .nth(2)
        .and_then(parse_machine)
        .and_then(|machine| gate::could_not_run(None, ctx.view.unreachable_for(machine), None));
    let failures = steps()
        .filter_map(|step| step.strip_prefix(GATE_COULD_NOT_RUN))
        .filter(|rest| rest.split(' ').next() == Some(attempt.to_string().as_str()))
        .count();
    let failures = u32::try_from(failures)
        .unwrap_or(u32::MAX)
        .saturating_add(1);
    let wait = gate::backoff(failures).map_or_else(|| "-".to_owned(), |wait| wait.to_string());
    let reason = gone.as_deref().unwrap_or(GATE_CUT_OFF);
    ctx.held()?;
    ctx.record_step(
        run,
        &format!("{GATE_COULD_NOT_RUN}{attempt} {k} {failures} {wait} {reason}"),
        &Cause::new(CauseStep::Gate, "gate-could-not-run").attempt(attempt),
    )?;
    if gone.is_some() {
        return Ok(());
    }
    let created = steps()
        .filter_map(|step| step.strip_prefix(GATE_CREATED))
        .filter_map(|rest| rest.split_once(' '))
        .filter(|(n, _)| n.parse::<u32>().ok() == Some(k))
        .filter_map(|(_, id)| workspace_id(id).ok())
        .next_back()
        .or_else(|| {
            ctx.view
                .workspace_named(ctx.project.id, &gate::name(run, k))
                .map(|record| record.body.id)
        });
    if let Some(workspace) = created {
        match ctx.archive(workspace) {
            Ok(()) => {}
            Err(error @ RunError::LeaseLost(_)) => return Err(error),
            Err(_) => {}
        }
    }
    Ok(())
}

/// The codes of the stuck paths this step begins. None is one `start` or `building` writes:
/// the two that look alike are named for this step.
const STUCK_GONE: &str = "thread-gone";
const STUCK_ENDED: &str = "session-ended-badly";
const STUCK_MACHINE: &str = "machine-unreachable";
const STUCK_UNCONFIRMED: &str = "resume-approval-unconfirmed";
const STUCK_LAUNCH: &str = "resume-launch-failed";
const STUCK_SETUP: &str = "resume-setup-failed";
const STUCK_TURN: &str = "turn-unreadable";
const OWN_STUCK_CODES: [&str; 9] = [
    STUCK_GONE,
    STUCK_ENDED,
    STUCK_MACHINE,
    STUCK_TURN,
    STUCK_UNCONFIRMED,
    STUCK_LAUNCH,
    STUCK_SETUP,
    TEXT_CODE,
    UNVERIFIED_CODE,
];

/// The machine that owns the run's newest workspace: its record's owner, else the claim's
/// machine.
fn old_owner(
    ctx: &RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    seen: &Seen,
) -> Option<MachineId> {
    seen.worktree
        .as_deref()
        .or(seen.workspace.as_deref())
        .and_then(|id| workspace_id(id).ok())
        .and_then(|id| find_workspace(ctx.view, ctx.project.id, run, scan_of(events), id))
        .map(|record| record.owner)
        .or_else(|| seen.machine.as_deref().and_then(parse_machine))
}

/// Whether the old workspace's machine has been unreachable for 15 minutes: it cannot be asked
/// to end its sessions, and ends its own (G10).
fn old_machine_gone(ctx: &RunContext<'_>, run: &RunId, events: &[RunEvent], seen: &Seen) -> bool {
    old_owner(ctx, run, events, seen).is_some_and(|machine| unreachable_long(ctx, machine))
}

/// The newest turn of `session` that was not read since its launch: newer than the turn read
/// that `building` recorded, as `seen` holds it.
fn unread_turn(
    ctx: &RunContext<'_>,
    seen: &Seen,
    session: SessionId,
) -> Option<foundry_proto::factory::SessionTurn> {
    let read = seen.turn_read;
    ctx.view
        .session_turn(session)
        .filter(|turn| turn.turn > 0 && read.is_none_or(|read| turn.turn > read))
        .cloned()
}

/// The detail written for the newest decision: the `stuck-detail` step right before its
/// `stuck-stop`, which `go_stuck` writes one after the other under the same cause, so that
/// of the decision's `code`. A lone detail of an earlier decision that was decided again, and
/// has another code, is not it.
fn decision_detail(events: &[RunEvent], code: &str) -> Option<String> {
    let stop = events
        .iter()
        .rposition(|event| step_text(event).is_some_and(|step| step.starts_with(STUCK_STOP)))?;
    let before = events.get(stop.checked_sub(1)?)?;
    let same_cause = before
        .cause
        .as_ref()
        .and_then(|cause| cause.get("reason"))
        .and_then(Value::as_str)
        == Some(code);
    if !same_cause {
        return None;
    }
    step_text(before)?
        .strip_prefix(STUCK_DETAIL)
        .map(str::to_owned)
}

/// A stuck path this step began and was cut short, finished from the ledger as building's
/// `finish_begun_stuck` finishes its own: the decision's code and words, the detail the ledger
/// keeps, and for a setup that failed the tail read again. A path begun by another step is
/// left to it (`None`), unless a resume is owed, which that step skips the run for.
fn finish_own_stuck(
    ctx: &RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    seen: &Seen,
    at: &mut Progress,
    resume_owed: bool,
) -> Result<Option<ResumeStep>, RunError> {
    let Some(raw) = &seen.stuck_stop else {
        // A `stuck` line with no decision, while a resume is owed (`building` skips such a run):
        // finished with its recorded reason, as building does.
        if resume_owed && let Some(reason) = &seen.stuck {
            if old_machine_gone(ctx, run, events, seen) {
                at.worktree = None;
            }
            go_stuck(
                ctx,
                run.issue(),
                at,
                "stuck-resumed",
                Stop::new(reason.clone()),
                None,
            )?;
            return Ok(Some(ResumeStep::Stuck {
                reason: reason.clone(),
            }));
        }
        return Ok(None);
    };
    let decision: StuckStop = serde_json::from_str(raw)
        .map_err(|error| RunError::Invalid(format!("the stuck decision of {run}: {error}")))?;
    if !resume_owed && !OWN_STUCK_CODES.contains(&decision.code.as_str()) {
        return Ok(None);
    }
    let mut stop = decision.stop;
    stop.detail = decision_detail(events, &decision.code);
    let round = read_round(events).filter(|_| resume_owed);
    // The resume workspace the decision was made in, when one was recorded.
    let resume_record = round
        .as_ref()
        .and_then(|round| resume_record(ctx.view, ctx.project.id, run, round));
    if decision.code == STUCK_SETUP
        && let Some(terminal) = resume_record.and_then(|record| record.body.setup)
    {
        match ctx.tail(terminal) {
            Ok(tail) => {
                stop.session_text = Some(prepare_output_tail(&tail));
                stop.recorded_text = Some(text::sanitize(&tail, true));
            }
            Err(error @ RunError::LeaseLost(_)) => return Err(error),
            Err(_) => {}
        }
    }
    // What the session said, read again from its newest unread turn as the first try read it.
    if decision.code == STUCK_ENDED
        && let Some(thread) = &seen.thread_id
    {
        let session = session_id(thread)?;
        if let Some(turn) = unread_turn(ctx, seen, session) {
            ctx.held()?;
            if let Ok(said) = ctx.worker.turn(session, turn.turn) {
                stop.session_text = prepared(&said.message);
            }
        }
    }
    // The sessions ended are those of the workspace the decision used: the resume's when one
    // was made, else the old one, and neither when its machine is gone for good.
    at.worktree = match (&round, resume_record) {
        (Some(round), record) if round.workspace.is_some() => record
            .filter(|record| !unreachable_long(ctx, record.owner))
            .map(|record| record.body.id.to_string()),
        _ if decision.code == STUCK_MACHINE
            || decision.code == STUCK_TURN
            || old_machine_gone(ctx, run, events, seen) =>
        {
            None
        }
        _ => at.worktree.take(),
    };
    let reason = stop.reason.clone();
    go_stuck(ctx, run.issue(), at, &decision.code, stop, None)?;
    Ok(Some(ResumeStep::Stuck { reason }))
}

/// How the session ended, when it has: gone from the fleet, finished or failed with no result
/// line to act on, or on a machine unreachable for 15 minutes. A session that finished with a
/// `FACTORY-DONE` is `building`'s.
fn ended(
    ctx: &RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    seen: &Seen,
) -> Result<Option<Ended>, RunError> {
    let Some(thread) = &seen.thread_id else {
        return Ok(None);
    };
    let session = session_id(thread)?;
    let Some(state) = ctx.view.session_state(session) else {
        // A session record syncs apart from its workspace's: one the view does not show while
        // its workspace still does is not gone, only not here yet. A machine that stays away
        // for 15 minutes ends it all the same.
        let shown = seen
            .worktree
            .as_deref()
            .and_then(|id| workspace_id(id).ok())
            .and_then(|id| find_workspace(ctx.view, ctx.project.id, run, scan_of(events), id));
        return Ok(match shown {
            None => Some(Ended::Gone),
            Some(record) => unreachable_long(ctx, record.owner).then_some(Ended::Unreachable),
        });
    };
    match state {
        SessionState::Unreachable => {
            // The probe is not asked while the owner is away, which breaks its failures.
            let next = seen.turn_read.map_or(1, |read| read + 1);
            probe_answered(
                ctx,
                run,
                failing_since(events, session, next),
                session,
                next,
            )?;
            Ok(session_owner(ctx, seen)
                .filter(|machine| unreachable_long(ctx, *machine))
                .map(|_| Ended::Unreachable))
        }
        SessionState::Finished | SessionState::Failed => {
            let what = if state == SessionState::Failed {
                "failed"
            } else {
                "finished"
            };
            let text = match unread_turn(ctx, seen, session) {
                // A turn record syncs apart from its session's, and can be written after the
                // session ended: the turn after the newest read is asked of the worker. One it
                // answers is written and not shown yet, so nothing is decided until it shows. One
                // it says was never written means the process ended before a result line. Any
                // other error decides nothing: the next pass asks again, unless the call has
                // failed without a break for `TURN_FAILING_FOR` (`probe_failed`), which sends the
                // run stuck.
                None => {
                    let next = seen.turn_read.map_or(1, |read| read + 1);
                    let generation = ctx.held()?;
                    let since = failing_since(events, session, next);
                    match ctx.worker.turn(session, next) {
                        Ok(_) => {
                            probe_answered(ctx, run, since, session, next)?;
                            return Ok(None);
                        }
                        Err(WorkerError::NotWritten(_)) => {
                            probe_answered(ctx, run, since, session, next)?;
                            None
                        }
                        Err(error @ WorkerError::Failed(_)) => {
                            if !probe_failed(ctx, run, since, generation, session, next)? {
                                return Err(error.into());
                            }
                            return Ok(Some(Ended::TurnUnreadable(error.to_string())));
                        }
                        Err(error) => {
                            probe_answered(ctx, run, since, session, next)?;
                            return Err(error.into());
                        }
                    }
                }
                Some(turn) => {
                    ctx.held()?;
                    let message = ctx.worker.turn(session, turn.turn)?.message;
                    // A done message wins over the way the session ended.
                    if matches!(
                        result_line::read(Some(&message), ctx.repo()),
                        SessionResult::Done { .. }
                    ) {
                        return Ok(None);
                    }
                    prepared(&message)
                }
            };
            Ok(Some(Ended::Process { what, text }))
        }
        SessionState::Starting
        | SessionState::Working
        | SessionState::Idle
        | SessionState::WaitingForYou
        | SessionState::Stuck => Ok(None),
    }
}

/// The lease generation and clock reading of the first `Failed` answer to the probe of turn
/// `turn` of `session` that is still on record: the newest `resume-turn-failing` line of that
/// turn with no `resume-turn-answered` line of it after, in the ledger's order.
fn failing_since(events: &[RunEvent], session: SessionId, turn: u64) -> Option<(u64, u64)> {
    let key = format!("{session} {turn}");
    let mut since = None;
    for step in events.iter().filter_map(step_text) {
        if let Some(rest) = step.strip_prefix(TURN_FAILING) {
            let parts: Vec<&str> = rest.split(' ').collect();
            if let [id, n, generation, ms] = parts.as_slice()
                && format!("{id} {n}") == key
                && let (Ok(generation), Ok(ms)) = (generation.parse(), ms.parse())
            {
                since = Some((generation, ms));
            }
        } else if step.strip_prefix(TURN_ANSWERED) == Some(key.as_str()) {
            since = None;
        }
    }
    since
}

/// A `Failed` answer to the probe of turn `turn` of `session`: whether the call has failed
/// without a break for [`TURN_FAILING_FOR`], counted on the sleep-counting clock from the
/// first failure on record (`since`). With none, or one of another lease generation or above
/// the clock now (a takeover, or a reboot that reset the clock), the first failure is recorded
/// now, before the error is returned: a takeover can lengthen the wait, never end it early. A
/// daemon restart keeps the generation and the clock, and so the first failure's time.
fn probe_failed(
    ctx: &RunContext<'_>,
    run: &RunId,
    since: Option<(u64, u64)>,
    generation: u64,
    session: SessionId,
    turn: u64,
) -> Result<bool, RunError> {
    let now = u64::try_from(ctx.clock.now().as_millis()).unwrap_or(u64::MAX);
    if let Some(waited) = super::time_waited(since, generation, now) {
        return Ok(waited >= TURN_FAILING_FOR);
    }
    note(
        ctx,
        run,
        &format!("{TURN_FAILING}{session} {turn} {generation} {now}"),
        "resume-turn-failing",
    )?;
    Ok(false)
}

/// An answer to the probe of turn `turn` of `session` that is not `Failed` breaks the
/// failures: a first failure on record (`since`) is cleared. With none, nothing is written.
fn probe_answered(
    ctx: &RunContext<'_>,
    run: &RunId,
    since: Option<(u64, u64)>,
    session: SessionId,
    turn: u64,
) -> Result<(), RunError> {
    if since.is_none() {
        return Ok(());
    }
    note(
        ctx,
        run,
        &format!("{TURN_ANSWERED}{session} {turn}"),
        "resume-turn-answered",
    )
}

/// The workspace the run's newest session works in.
fn old_workspace(seen: &Seen) -> Result<Option<WorkspaceId>, RunError> {
    seen.worktree
        .as_deref()
        .or(seen.workspace.as_deref())
        .map(workspace_id)
        .transpose()
}

/// The run's base: the one its launch recorded, else the project's.
fn run_base(ctx: &RunContext<'_>, seen: &Seen) -> String {
    seen.base
        .clone()
        .or_else(|| seen.launch_base.clone())
        .unwrap_or_else(|| ctx.config.base.clone())
}

/// The first message of a resume: on `branch` the resumed launch, else the plain launch.
fn launch_message(ctx: &RunContext<'_>, run: &RunId, seen: &Seen, branch: Option<&str>) -> Message {
    let base = run_base(ctx, seen);
    let named = (base != ctx.config.base).then_some(base.as_str());
    match branch {
        Some(branch) => messages::resumed(
            ctx.repo(),
            run.issue(),
            &ctx.project.login,
            &ResumeFrom::Interrupted {
                run: run.clone(),
                branch: branch.to_owned(),
            },
            named,
            BODY_PATH,
        ),
        None => messages::launch(
            ctx.repo(),
            run.issue(),
            &ctx.project.login,
            named,
            BODY_PATH,
        ),
    }
}

/// The step an issue that was closed ends the run with, when it was. The sessions ended are
/// those of the newest workspace a session may run in: the resume workspace once its create was
/// answered (its start may have gone out with no answer, and the earlier workspace's archive
/// ended the earlier session), else the earlier one. Neither is asked when its machine has been
/// unreachable for 15 minutes: it ends its own sessions (G10), and it cannot be asked.
fn closing_step(
    ctx: &RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    seen: &Seen,
    closing: Closing,
) -> Result<Option<ResumeStep>, RunError> {
    if closing == Closing::Open {
        return Ok(None);
    }
    let mut ending = seen.clone();
    let owed = seen
        .interruption
        .as_ref()
        .filter(|interruption| !interruption.sent);
    let round = owed.and_then(|owed| read_round(events).filter(|round| round.k == owed.resumes));
    if let Some(round) = round.filter(|round| round.workspace.is_some()) {
        // A record not shown yet ran no session: a session is started only once it shows.
        ending.worktree = resume_record(ctx.view, ctx.project.id, run, &round)
            .filter(|record| !unreachable_long(ctx, record.owner))
            .map(|record| record.body.id.to_string());
        ending.workspace = None;
    } else if old_machine_gone(ctx, run, events, seen) {
        ending.worktree = None;
        ending.workspace = None;
    }
    // `abandon`'s answer, a build launch sent with no session recorded, is `start`'s to report:
    // this step sends no build launch, and a resume's start that may have gone out is ended
    // above.
    match closing {
        Closing::Open => Ok(None),
        Closing::PersonsMove => {
            abandon(ctx, run, &ending, CauseStep::Advance)?;
            Ok(Some(ResumeStep::Abandoned))
        }
        Closing::OwnMerge(pull) => {
            end_own_merge(ctx, run, &ending, CauseStep::Advance, &pull)?;
            Ok(Some(ResumeStep::Ended))
        }
    }
}

/// A run that went to `triage` with a session recorded is moved to `building`.
fn ensure_building(ctx: &RunContext<'_>, issue: u64, at: &mut Progress) -> Result<(), RunError> {
    if at.state == State::Triage {
        finish_building(ctx, issue, at)?;
    }
    Ok(())
}

/// The run goes stuck under `code`, with `stop`.
fn stuck(
    ctx: &RunContext<'_>,
    run: &RunId,
    at: &mut Progress,
    code: &str,
    stop: Stop,
) -> Result<ResumeStep, RunError> {
    let reason = stop.reason.clone();
    go_stuck(ctx, run.issue(), at, code, stop, None)?;
    Ok(ResumeStep::Stuck { reason })
}

/// Where the resume continues: the open pull request's head, else the run's own branch when it
/// is on `origin`, else none. A pull request from a fork is not resumed from. A head that is
/// not a plain branch name is refused.
fn choose_branch(ctx: &RunContext<'_>, issue: u64, seen: &Seen) -> Result<Branch, RunError> {
    if let Some(last) = &seen.last_pr
        && let Some(pull) = ctx.tracker.pull_request(ctx.repo(), last.pr)?
        && pull.open
        && !pull.cross_repository
    {
        return Ok(if is_safe_branch_name(&pull.head_ref) {
            Branch::Use(Some(pull.head_ref))
        } else {
            Branch::Refused
        });
    }
    // As `start` resumes a launch: an issue text edited since the approval is not continued
    // from a branch, and the resume starts from the base.
    let edited = match &seen.approved_at {
        Some(approved_at) => ctx
            .tracker
            .text_edited_since(ctx.repo(), issue, approved_at)?,
        None => true,
    };
    if edited {
        return Ok(Branch::Use(None));
    }
    let mut tried: Vec<&str> = Vec::new();
    for candidate in [seen.last_branch.as_deref(), seen.resumed_from.as_deref()]
        .into_iter()
        .flatten()
    {
        if tried.contains(&candidate) || !is_safe_branch_name(candidate) {
            continue;
        }
        tried.push(candidate);
        match ctx.tracker.branch_tip(ctx.repo(), candidate) {
            Ok(_) => return Ok(Branch::Use(Some(candidate.to_owned()))),
            Err(
                TrackerError::NotFound { .. }
                | TrackerError::Other {
                    kind: OtherKind::InvalidArgument,
                    ..
                },
            ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(Branch::Use(None))
}

/// How many resumes count against `limits.interruption_resumes`: the `session_interrupted`
/// lines after the newest `feedback_relaunching`, since a feedback round starts the cap again,
/// less a relaunch's (`relaunch: true`), which is no resume. The `-r<k>` names keep the run's
/// whole count, so no name is reused.
fn resumes_this_round(events: &[RunEvent]) -> u64 {
    let round = events
        .iter()
        .rposition(|event| event.event == names::FEEDBACK_RELAUNCHING)
        .map_or(0, |at| at + 1);
    events[round..]
        .iter()
        .filter(|event| {
            event.event == names::SESSION_INTERRUPTED
                && event.data.get("relaunch") != Some(&Value::Bool(true))
        })
        .count() as u64
}

/// The first pass: the cap, the branch, the decision, and the archive of the earlier workspace.
fn decide(
    ctx: &RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    seen: &Seen,
    at: &mut Progress,
    ended: &Ended,
) -> Result<ResumeStep, RunError> {
    let issue = run.issue();
    let (closing, _) = closed_issue(ctx, run, seen)?;
    if let Some(step) = closing_step(ctx, run, events, seen, closing)? {
        return Ok(step);
    }
    ensure_building(ctx, issue, at)?;
    // The names count across the whole run; the cap counts this round's resumes only.
    let resumes = seen
        .interruption
        .as_ref()
        .map_or(0, |interruption| interruption.resumes);
    let used = resumes_this_round(events);
    let (code, stop) = ended.stop();
    // The machine is gone: nothing can be asked of it, and it ends its own sessions.
    if *ended == Ended::Unreachable {
        at.worktree = None;
    }
    // The owner fails every call the same way: the run is a person's, with no resume spent
    // and nothing more asked of it. The session has ended, so there is none to end.
    if matches!(ended, Ended::TurnUnreadable(_)) {
        at.worktree = None;
        return stuck(ctx, run, at, code, stop);
    }
    if used >= ctx.config.limits.interruption_resumes {
        return stuck(ctx, run, at, code, stop);
    }
    // A resume starts a session, which the approval must stand behind.
    if seen.approved_at.is_none() {
        let stop = Stop {
            reason: CANNOT_CONFIRM.into(),
            detail: Some(recorded_reason(&stop)),
            ..Stop::default()
        };
        return stuck(ctx, run, at, STUCK_UNCONFIRMED, stop);
    }
    let branch = match choose_branch(ctx, issue, seen)? {
        Branch::Use(branch) => branch,
        Branch::Refused => {
            let stop = Stop {
                reason: LAUNCH_FAILED.into(),
                detail: Some(BRANCH_REFUSED.into()),
                ..Stop::default()
            };
            return stuck(ctx, run, at, STUCK_LAUNCH, stop);
        }
    };
    let k = resumes + 1;
    ctx.held()?;
    ctx.record(
        run,
        Event::SessionInterrupted {
            resumes: k,
            relaunch: false,
            run_ordinal: None,
            branch,
            t3_error: None,
            ended_in_pass: 0,
        },
        &Cause::new(CauseStep::Advance, "session-interrupted"),
    )?;
    archive_old(ctx, run, old_workspace(seen)?, k, Records::Both)
}

/// What [`archive_old`] records of the ask.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Records {
    /// The step that names the workspace, and that the archive was asked.
    Both,
    /// That the archive was asked.
    Asked,
    /// Nothing: the archive is asked again of a machine that is heard from.
    Nothing,
}

/// The earlier workspace's archive: recorded (as `records` says), then asked for. A machine
/// that cannot be reached is no error: the archive is asked again once it is heard from, or
/// the resume goes on when it has been unreachable for 15 minutes. Any other failure is asked
/// again next pass.
fn archive_old(
    ctx: &RunContext<'_>,
    run: &RunId,
    old: Option<WorkspaceId>,
    k: u32,
    records: Records,
) -> Result<ResumeStep, RunError> {
    if records == Records::Both {
        let named = old.map_or_else(|| "none".to_owned(), |id| id.to_string());
        note(
            ctx,
            run,
            &format!("{ARCHIVING}{k} {named}"),
            "resume-archiving",
        )?;
    }
    if let Some(old) = old {
        match ctx.archive(old) {
            Ok(()) | Err(RunError::Worker(WorkerError::Unreachable(_))) => {}
            Err(error) => return Err(error),
        }
    }
    if records != Records::Nothing {
        note(
            ctx,
            run,
            &format!("{ARCHIVE_ASKED}{k}"),
            "resume-archive-asked",
        )?;
    }
    Ok(ResumeStep::Archiving {
        resumes: k,
        workspace: old,
    })
}

/// A resume owed, `k`-th: the archive, the wait for it, the machine, the workspace, its setup
/// and the session, each from where the ledger shows it stopped. A call with a machine placed
/// ([`placed`]) acts only where a machine is needed.
#[allow(clippy::too_many_arguments)]
fn carry_on(
    ctx: &RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    seen: &Seen,
    at: &mut Progress,
    k: u32,
    branch: Option<&str>,
    placed: Option<Target>,
) -> Result<ResumeStep, RunError> {
    let issue = run.issue();
    let (closing, detail) = closed_issue(ctx, run, seen)?;
    if let Some(step) = closing_step(ctx, run, events, seen, closing)? {
        return Ok(step);
    }
    let round = read_round(events)
        .filter(|round| round.k == k)
        .ok_or_else(|| RunError::Invalid(format!("run {run} owes resume {k} with no decision")))?;
    let scan = scan_of(events);
    ensure_building(ctx, issue, at)?;
    let needs_no_machine = || skipped(run, "needs no machine yet");
    let Some(archiving) = &round.archiving else {
        if placed.is_some() {
            return needs_no_machine();
        }
        return archive_old(ctx, run, old_workspace(seen)?, k, Records::Both);
    };
    let old = match archiving.as_str() {
        "none" => None,
        text => Some(workspace_id(text)?),
    };
    if !round.archive_asked {
        if placed.is_some() {
            return needs_no_machine();
        }
        return archive_old(ctx, run, old, k, Records::Asked);
    }
    // Once it shows archived, or its machine is gone for good. While it shows on a machine that
    // is heard from, the archive is asked again: an answer of unreachable may have been a
    // timeout, and the archive is safe to repeat.
    if let Some(old) = old
        && let Some(record) = find_workspace(ctx.view, ctx.project.id, run, scan, old)
        && !unreachable_long(ctx, record.owner)
    {
        if placed.is_some() {
            return needs_no_machine();
        }
        if ctx.view.online(record.owner) {
            return archive_old(ctx, run, Some(old), k, Records::Nothing);
        }
        return Ok(ResumeStep::Waiting(Wait::Archive));
    }
    let target = match (round.target, placed) {
        (Some(target), _) => target,
        (None, Some(target)) => {
            // Placement puts a resume on the fleet: a host is refused as the fleet worker
            // refuses one.
            let machine = worker::fleet_machine(target)?;
            if round.gave_up.contains(&machine) && !ctx.view.online(machine) {
                return Err(RunError::Invalid(format!(
                    "run {run} gave up on machine {machine} for resume {k}"
                )));
            }
            note(
                ctx,
                run,
                &format!("{MACHINE}{k} {target}"),
                "resume-machine",
            )?;
            target
        }
        (None, None) => return Ok(ResumeStep::NeedsMachine(Placing::Resume { resumes: k })),
    };
    // A machine that stays away for 15 minutes before the session started is given up: nothing
    // ran, so no resume is spent, and what it made is for [`orphans`] when it is heard from.
    if unreachable_long(ctx, worker::fleet_machine(target)?) {
        note(
            ctx,
            run,
            &format!("{MACHINE_GIVEN_UP}{k} {target}"),
            "resume-machine-given-up",
        )?;
        return Ok(ResumeStep::NeedsMachine(Placing::Resume { resumes: k }));
    }
    let name = resume_name(run, k);
    let named = ctx.view.workspaces_named(ctx.project.id, &name);
    let record = match round.workspace {
        Some(id) => match named.iter().find(|record| record.body.id == id) {
            Some(record) => (*record).clone(),
            // The worker answered, and the fleet does not show the record yet: wait.
            None => return Ok(ResumeStep::Waiting(Wait::Workspace)),
        },
        None => {
            if !round.requested {
                note(
                    ctx,
                    run,
                    &format!("{REQUESTED}{k} {name}"),
                    "resume-requested",
                )?;
            }
            // A record the fleet shows with its setup started is the first create's answer.
            let id = match named.iter().find(|record| {
                Target::Machine(record.owner) == target && record.body.setup.is_some()
            }) {
                Some(record) => record.body.id,
                None => {
                    let checkout = match branch {
                        Some(branch) => Checkout::Branch {
                            name: branch.to_owned(),
                        },
                        None => Checkout::NewBranch {
                            base: run_base(ctx, seen),
                            branch: Some(name.clone()),
                        },
                    };
                    ctx.create(
                        target,
                        CreateWorkspace {
                            project: ctx.project.id,
                            name: name.clone(),
                            checkout,
                            setup: Setup::Own,
                            tag: FactoryTag {
                                project: ctx.project.id,
                                run: run.to_string(),
                            },
                        },
                    )?
                }
            };
            note(
                ctx,
                run,
                &format!("{WORKSPACE}{k} {id}"),
                "resume-workspace",
            )?;
            return Ok(ResumeStep::Waiting(Wait::Workspace));
        }
    };
    let workspace = record.body.id;
    at.worktree = Some(workspace.to_string());
    if let Some(terminal) = record.body.setup {
        match ctx.view.terminal(terminal) {
            Some((TerminalState::Exited, Some(0))) => {}
            Some((TerminalState::Exited, _)) => {
                let stop = match ctx.tail(terminal) {
                    Ok(tail) => Stop {
                        reason: SETUP_FAILED.into(),
                        session_text: Some(prepare_output_tail(&tail)),
                        recorded_text: Some(text::sanitize(&tail, true)),
                        ..Stop::default()
                    },
                    Err(RunError::Worker(WorkerError::Failed(_))) => Stop::new(SETUP_FAILED),
                    Err(error) => return Err(error),
                };
                return stuck(ctx, run, at, STUCK_SETUP, stop);
            }
            _ => {
                ctx.advance(workspace)?;
                return Ok(ResumeStep::Waiting(Wait::Setup));
            }
        }
    }
    let title = detail.map(|detail| detail.title);
    let launched = Launched {
        run,
        seen,
        record: &record,
        target,
        branch,
        title,
        k,
    };
    // A factory session the fleet already shows in the workspace (one with a turn record) for a
    // start that may have gone out: adopted, never started twice.
    if let Some(found) = ctx.view.session_of(workspace)
        && ctx.view.session_turn(found.body.id).is_some()
    {
        return launched.record(ctx, at, found.body.id);
    }
    // Before every start, the text since the approval, as the runner's `textStop` does: a
    // session that would act on text no instructor approved is not started. The approval is not
    // read again: a building issue never has `factory:ready` in force, as the claim took it off.
    let Some(approved_at) = &seen.approved_at else {
        return Err(RunError::Invalid(format!("run {run} records no approval")));
    };
    match ctx
        .tracker
        .text_check(ctx.repo(), issue, approved_at, &ctx.config.instructors)?
    {
        TextCheck::Unchanged => {}
        TextCheck::Changed => {
            let stop = Stop::new(CHANGED_DURING_BUILD);
            return stuck(ctx, run, at, TEXT_CODE, stop);
        }
        TextCheck::Unjudged => {
            let stop = Stop::new(CANNOT_VERIFY_DURING_BUILD);
            return stuck(ctx, run, at, UNVERIFIED_CODE, stop);
        }
    }
    let first = launch_message(ctx, run, seen, branch);
    let start = FactoryStart {
        project: ctx.project.id,
        login: ctx.project.login.clone(),
        model: model_of(ctx, seen),
        effort: ctx.settings.fleet.effort.clone(),
    };
    ctx.held()?;
    // A start that was sent and has no session recorded may have been taken whose answer was
    // lost, and the worker's start is not idempotent: the workspace's sessions are ended,
    // recorded before they are, and only then is it started again.
    if round.sending {
        note(
            ctx,
            run,
            &format!("{ENDING_SESSIONS}{k}"),
            "resume-ending-sessions",
        )?;
        ctx.end_sessions(workspace)?;
    } else {
        note(ctx, run, &format!("{SENDING}{k}"), "resume-sending")?;
    }
    match ctx.start_session(workspace, start, first) {
        Ok(session) => launched.record(ctx, at, session),
        // The worker refused: stuck, with the detail in the ledger only. A machine that cannot
        // be reached is no refusal: the next pass starts again.
        Err(RunError::Worker(WorkerError::Failed(what))) => {
            let note = if round.sending {
                messages::session_may_exist_note(&what, &format!("foundry:{run}:resume-{k}"))
            } else {
                what
            };
            let stop = Stop {
                reason: LAUNCH_FAILED.into(),
                detail: Some(note),
                ..Stop::default()
            };
            stuck(ctx, run, at, STUCK_LAUNCH, stop)
        }
        Err(error) => Err(error),
    }
}

/// The model a resumed session starts on: the one the run's launch decided, else the fleet's.
fn model_of(ctx: &RunContext<'_>, seen: &Seen) -> String {
    seen.launch_model
        .clone()
        .unwrap_or_else(|| ctx.settings.fleet.model.clone())
}

/// What a `session_launched` line of a resume is written from.
struct Launched<'a> {
    run: &'a RunId,
    seen: &'a Seen,
    record: &'a Record<Workspace>,
    target: Target,
    branch: Option<&'a str>,
    title: Option<String>,
    k: u32,
}

impl Launched<'_> {
    /// The `session_launched` line, then the status comment that names the machine and the
    /// session.
    fn record(
        &self,
        ctx: &RunContext<'_>,
        at: &mut Progress,
        session: SessionId,
    ) -> Result<ResumeStep, RunError> {
        let model = model_of(ctx, self.seen);
        ctx.held()?;
        ctx.record(
            self.run,
            Event::SessionLaunched {
                thread_id: session.to_string(),
                worktree: Some(self.record.body.id.to_string()),
                branch: Some(
                    self.branch
                        .map_or_else(|| self.record.body.branch.clone(), str::to_owned),
                ),
                model_instance: None,
                model: Some(model),
                model_options: Some(vec![ModelOption {
                    id: "effort".into(),
                    value: OptionValue::Text(ctx.settings.fleet.effort.clone()),
                }]),
                resumed_from: self.branch.map(|branch| ResumedFrom {
                    run: self.run.to_string(),
                    branch: branch.to_owned(),
                }),
                base: Some(run_base(ctx, self.seen)),
                title: self.title.clone(),
            },
            &Cause::new(
                CauseStep::Advance,
                if self.branch.is_some() {
                    "resumed-from-branch"
                } else {
                    "session-launched"
                },
            ),
        )?;
        at.thread_id = Some(session.to_string());
        ctx.upsert_status(
            self.run.issue(),
            &messages::status(
                State::Building,
                &self.target.to_string(),
                Some(&session.to_string()),
                at.pr,
            ),
        )?;
        Ok(ResumeStep::Started {
            resumes: self.k,
            session,
        })
    }
}

/// A session whose newest launch is a resume, idle with no prompt a pass later: the first
/// message is asked for again by count, with the words the first had.
fn resend(
    ctx: &RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    seen: &Seen,
) -> Result<ResumeStep, RunError> {
    let Some(round) = read_round(events).filter(|round| round.launched) else {
        return skipped(run, "has no resume to carry on");
    };
    let (Some(interruption), Some(thread)) = (&seen.interruption, &seen.thread_id) else {
        return skipped(run, "has no resume to carry on");
    };
    if interruption.resumes != round.k {
        return skipped(run, "has no resume to carry on");
    }
    let session = session_id(thread)?;
    // The launch is the session's first prompt, so its count before the send is none. A session
    // that is idle with no turn ended, or whose turn record shows no prompt, has not taken it.
    let idle = ctx.view.session_state(session) == Some(SessionState::Idle);
    let untouched = ctx
        .view
        .session_turn(session)
        .is_none_or(|turn| turn.turn == 0 && turn.prompts == FIRST_PROMPT_COUNT);
    if !(idle && untouched) {
        return skipped(run, "has a session that took its first message");
    }
    let message = launch_message(ctx, run, seen, interruption.branch.as_deref());
    note(
        ctx,
        run,
        &format!("{RESENDING}{} {FIRST_PROMPT_COUNT}", round.k),
        "resume-resend",
    )?;
    match ctx.send(session, message, Some(FIRST_PROMPT_COUNT)) {
        Ok(_) => Ok(ResumeStep::Resent { session }),
        // The worker decides: its count rose, so the message was typed; or the session is not
        // idle yet, and the next pass looks again.
        Err(RunError::Send(SendRefused::CountRose { .. } | SendRefused::NotIdle)) => {
            skipped(run, "has a session that took its first message")
        }
        Err(error) => Err(error),
    }
}

/// A claim whose named machine has been unreachable for 15 minutes with no workspace is launched
/// on another machine and spends no resume. `start` carries on from the workspace by its id.
fn moved_claim(
    ctx: &RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    seen: &Seen,
    placed: Option<Target>,
) -> Result<ResumeStep, RunError> {
    let issue = run.issue();
    // Placement puts a claim on the fleet: a host is refused as the fleet worker refuses it.
    let placed = placed.map(worker::fleet_machine).transpose()?;
    if seen.state != Some(State::Triage) {
        return skipped(run, "is not claimed and waiting for its workspace");
    }
    // `start` decides first, and writes its decision before it asks.
    let Some(name) = seen.workspace_requested.clone() else {
        return skipped(run, "has no decision of start's to carry out");
    };
    if seen.workspace.is_some() {
        return skipped(run, "has its build workspace");
    }
    let Some(claimed_on) = seen.machine.as_deref().and_then(parse_machine) else {
        return skipped(run, "names no machine");
    };
    let (moved, gave_up) = moved_to(events);
    // Not yet moved: only a claim whose machine is gone for good and which made nothing is,
    // after a target was given up as before. A workspace a given-up machine made is not the
    // claim's: it is an orphan for when that machine is heard from again.
    let needs_a_machine = ResumeStep::NeedsMachine(Placing::MovedClaim {
        unreachable: claimed_on,
    });
    if moved.is_none() {
        if ctx
            .view
            .workspaces_named(ctx.project.id, &name)
            .iter()
            .any(|record| !gave_up.contains(&record.owner))
        {
            return skipped(run, "has a build workspace");
        }
        if !unreachable_long(ctx, claimed_on) {
            return skipped(run, "has a machine that is not unreachable for long");
        }
    }
    if moved.is_none() && placed.is_none() {
        return Ok(needs_a_machine);
    }
    if let Some(machine) = placed
        && moved.is_none()
        && (machine == claimed_on || gave_up.contains(&machine))
        && !ctx.view.online(machine)
    {
        return Err(RunError::Invalid(format!(
            "run {run} cannot move to machine {machine}, which it was or is unreachable on"
        )));
    }
    let (closing, _) = closed_issue(ctx, run, seen)?;
    if let Some(step) = closing_step(ctx, run, events, seen, closing)? {
        return Ok(step);
    }
    let target = match (moved, placed) {
        (Some(machine), _) => machine,
        (None, Some(machine)) => {
            note(ctx, run, &format!("{CLAIM_MOVED}{machine}"), "claim-moved")?;
            machine
        }
        (None, None) => return Ok(needs_a_machine),
    };
    // The machine it moved to stays away for 15 minutes before it answered: given up, and the
    // pass places another.
    if unreachable_long(ctx, target) {
        note(
            ctx,
            run,
            &format!("{CLAIM_GIVEN_UP}{target}"),
            "claim-moved-given-up",
        )?;
        return Ok(needs_a_machine);
    }
    ctx.upsert_status(
        issue,
        &messages::status(State::Triage, &target.to_string(), None, None),
    )?;
    let checkout = match &seen.launch_resumed_from {
        Some(branch) => Checkout::Branch {
            name: branch.clone(),
        },
        None => Checkout::NewBranch {
            base: seen
                .launch_base
                .clone()
                .unwrap_or_else(|| ctx.config.base.clone()),
            branch: Some(name.clone()),
        },
    };
    // A create whose answer is lost is asked again: the worker answers with the first by tag
    // and name.
    let workspace = ctx.create(
        Target::Machine(target),
        CreateWorkspace {
            project: ctx.project.id,
            name,
            checkout,
            setup: Setup::Own,
            tag: FactoryTag {
                project: ctx.project.id,
                run: run.to_string(),
            },
        },
    )?;
    note(
        ctx,
        run,
        &format!("{WORKSPACE_CREATED}{workspace}"),
        "workspace-created",
    )?;
    Ok(ResumeStep::Moved {
        machine: target,
        workspace,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use foundry_proto::factory::{RunEvent, SessionTurn};
    use foundry_proto::ids::{MachineId, SessionId, TerminalId, WorkspaceId};
    use foundry_proto::record::{Record, Session, Terminal, Workspace};
    use foundry_proto::state::{SessionState, TerminalState};
    use serde_json::Value;

    use super::super::testing::*;
    use super::*;
    use crate::factory::ledger::events::{Event, ResumedFrom};
    use crate::factory::lifecycle::State;
    use crate::factory::messages::{self, LAUNCH_FAILED, ResumeFrom, SESSION_GONE};
    use crate::factory::worker::{
        Call, CreateWorkspace, FleetView, FleetViewBuilder, HostCall, Lost, Target, WorkerError,
    };
    use crate::sessions::factory::BODY_PATH;
    use crate::sessions::turns::TurnText;
    use crate::workspaces::{Checkout, FactoryTag, Setup};

    const SESSION: &str = "00000000-0000-0000-0000-0000000005e5";
    const OLD: &str = "00000000-0000-0000-0000-000000000a11";
    const BRANCH: &str = "fac/8-1";
    const PR_BRANCH: &str = "feat/pr-12";
    const URL12: &str = "https://github.com/rawphp/softfac/pull/12";
    const DONE_MESSAGE: &str =
        "Opened it.\nFACTORY-DONE: https://github.com/rawphp/softfac/pull/12";

    fn session() -> SessionId {
        SessionId(uuid::Uuid::from_u128(0x5e5))
    }

    /// The session a resume starts.
    fn resumed_session() -> SessionId {
        SessionId(uuid::Uuid::from_u128(0x5e7))
    }

    fn old() -> WorkspaceId {
        WorkspaceId(uuid::Uuid::from_u128(0xa11))
    }

    /// The resume workspace the worker answers with.
    fn fresh() -> WorkspaceId {
        WorkspaceId(uuid::Uuid::from_u128(0xb22))
    }

    /// The machine that built the run.
    fn m1() -> MachineId {
        MachineId(uuid::Uuid::from_u128(0x2))
    }

    /// The machine a resume is placed on.
    fn m2() -> MachineId {
        MachineId(uuid::Uuid::from_u128(0x3))
    }

    fn setup_terminal() -> TerminalId {
        TerminalId(uuid::Uuid::from_u128(0x7e1))
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// The session's launch line, on `branch`.
    fn launch(branch: Option<&str>, base: Option<&str>) -> Event {
        Event::SessionLaunched {
            thread_id: SESSION.into(),
            worktree: Some(OLD.into()),
            branch: branch.map(str::to_owned),
            model_instance: None,
            model: Some("opus".into()),
            model_options: None,
            resumed_from: None::<ResumedFrom>,
            base: base.map(str::to_owned),
            title: Some("Make it faster".into()),
        }
    }

    /// A run that was claimed, launched and is building, as `start` leaves it.
    fn building_with(rig: &Rig, run: &RunId, launch: Event) {
        rig.step(run, &format!("approved {LABELED_AT}"));
        rig.write(
            run,
            Event::Claimed {
                machine: m1().to_string(),
                github_user: LOGIN.into(),
            },
        );
        rig.moved(run, State::Ready, State::Triage);
        rig.step(run, "launch-model opus");
        rig.step(run, &format!("workspace-requested {BRANCH}"));
        rig.step(run, &format!("workspace-created {OLD}"));
        rig.step(run, "approval-rechecked");
        rig.write(run, launch);
        rig.moved(run, State::Triage, State::Building);
        rig.sim.set_labels(&["factory:building"]);
        rig.sim.set_assignees(&[LOGIN]);
    }

    fn building(rig: &Rig, run: &RunId) {
        building_with(rig, run, launch(Some(BRANCH), Some("main")));
    }

    fn workspace_record(
        owner: MachineId,
        id: WorkspaceId,
        name: &str,
        setup: Option<TerminalId>,
    ) -> Record<Workspace> {
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
                setup,
            },
        }
    }

    fn session_record(
        id: SessionId,
        workspace: WorkspaceId,
        state: SessionState,
    ) -> Record<Session> {
        Record {
            owner: m1(),
            version: 1,
            deleted: false,
            body: Session {
                id,
                workspace,
                terminal: TerminalId(uuid::Uuid::from_u128(0x5e6)),
                model: "opus".into(),
                state,
                name: None,
            },
        }
    }

    fn turn_of(session: SessionId, workspace: WorkspaceId, turn: u64, prompts: u64) -> SessionTurn {
        SessionTurn {
            session,
            workspace,
            project: project_id(),
            turn,
            prompts,
            question: false,
        }
    }

    /// The fleet with the build machine online, the run's build workspace on it and the build
    /// session in `state` (none: the fleet shows no such session).
    fn fleet(state: Option<SessionState>) -> FleetViewBuilder {
        let builder = FleetView::builder()
            .online(m1())
            .online(m2())
            .workspace(workspace_record(m1(), old(), BRANCH, None));
        match state {
            Some(state) => builder.session(session_record(session(), old(), state)),
            None => builder,
        }
    }

    /// The session's process ended with no turn: an interruption.
    fn failed() -> FleetView {
        fleet(Some(SessionState::Failed)).build()
    }

    /// The session is gone: the fleet shows no session, and no record of its workspace.
    fn gone() -> FleetView {
        FleetView::builder().online(m1()).online(m2()).build()
    }

    /// The build machine has been unreachable for `span`, with its workspace and session as it
    /// left them.
    fn cut_off(span: Duration) -> FleetView {
        FleetView::builder()
            .online(m2())
            .workspace(workspace_record(m1(), old(), BRANCH, None))
            .session(session_record(session(), old(), SessionState::Working))
            .unreachable(m1(), span)
            .build()
    }

    /// The earlier workspace is archived: no record shows.
    fn archived() -> FleetView {
        FleetView::builder().online(m1()).online(m2()).build()
    }

    /// The resume workspace shows on `m2`, with `setup` as its setup terminal.
    fn with_resume_workspace(name: &str, setup: Option<TerminalId>) -> FleetViewBuilder {
        FleetView::builder()
            .online(m1())
            .online(m2())
            .workspace(workspace_record(m2(), fresh(), name, setup))
    }

    fn terminal_record(state: TerminalState, exit_code: Option<i32>) -> Record<Terminal> {
        Record {
            owner: m2(),
            version: 1,
            deleted: false,
            body: Terminal {
                id: setup_terminal(),
                workspace: fresh(),
                state,
                exit_code,
            },
        }
    }

    fn pass(rig: &Rig, run: &RunId) -> ResumeStep {
        advance(&rig.ctx(), run).unwrap()
    }

    fn placed_on(rig: &Rig, run: &RunId, machine: MachineId) -> ResumeStep {
        placed(&rig.ctx(), run, Target::Machine(machine)).unwrap()
    }

    fn is_skipped(step: &ResumeStep) -> bool {
        matches!(step, ResumeStep::Skipped { .. })
    }

    /// Every `session_interrupted` line of `run`, as its data.
    fn interruptions(rig: &Rig, run: &RunId) -> Vec<Value> {
        rig.events(run)
            .into_iter()
            .filter(|e| e.event == "session_interrupted")
            .map(|e| e.data)
            .collect()
    }

    fn create_calls(rig: &Rig) -> Vec<Call> {
        rig.worker
            .calls()
            .into_iter()
            .filter(|call| matches!(call, Call::Create { .. }))
            .collect()
    }

    fn archive_calls(rig: &Rig) -> Vec<Call> {
        rig.worker
            .calls()
            .into_iter()
            .filter(|call| matches!(call, Call::Archive { .. }))
            .collect()
    }

    fn start_calls(rig: &Rig) -> Vec<Call> {
        rig.worker
            .calls()
            .into_iter()
            .filter(|call| matches!(call, Call::StartSession { .. }))
            .collect()
    }

    fn sends(rig: &Rig) -> Vec<Call> {
        rig.worker
            .calls()
            .into_iter()
            .filter(|call| matches!(call, Call::Send { .. }))
            .collect()
    }

    /// The steps of the resume protocol after the first `from` events.
    fn resume_steps(rig: &Rig, run: &RunId) -> Vec<String> {
        rig.steps(run)
            .into_iter()
            .filter(|step| step.starts_with("resume-") || step.starts_with("claim-moved"))
            .collect()
    }

    fn event_count(rig: &Rig, run: &RunId) -> usize {
        rig.events(run).len()
    }

    fn archived_call() -> Call {
        Call::Archive { workspace: old() }
    }

    /// The worker asked for turn `n` of the build session: the turn after the newest read, which
    /// a pass that decides asks for.
    fn probe(n: u64) -> Call {
        Call::Turn {
            session: session(),
            n,
        }
    }

    /// A building run whose session failed with no turn: nothing is recorded of a resume yet.
    fn interrupted() -> (Rig, RunId) {
        let (rig, run) = failed_with_turns_unasked();
        never_written(&rig);
        (rig, run)
    }

    /// [`interrupted`], with no answer queued for the turn the worker is asked for: for a test
    /// that queues the turns itself.
    fn failed_with_turns_unasked() -> (Rig, RunId) {
        let mut rig = Rig::new();
        let run = run_id(1);
        building(&rig, &run);
        rig.view = failed();
        (rig, run)
    }

    /// The worker says the next turn asked for was never written: a session whose process ended
    /// with no turn after the newest read, as a pass that decides asks.
    fn never_written(rig: &Rig) {
        rig.worker.on_turn(Err(WorkerError::NotWritten(
            "the turn will not be written".into(),
        )));
    }

    /// The first pass of an interruption: recorded, archive asked.
    fn pass_one(rig: &Rig, run: &RunId) {
        assert_eq!(
            pass(rig, run),
            ResumeStep::Archiving {
                resumes: 1,
                workspace: Some(old()),
            }
        );
    }

    /// The run after its first pass, the earlier workspace archived and a machine needed.
    fn archived_and_waiting_for_a_machine() -> (Rig, RunId) {
        let (mut rig, run) = interrupted();
        pass_one(&rig, &run);
        rig.view = archived();
        assert_eq!(
            pass(&rig, &run),
            ResumeStep::NeedsMachine(Placing::Resume { resumes: 1 })
        );
        (rig, run)
    }

    /// The resume workspace is asked for on `m2`, and its record shows with no setup.
    fn resume_workspace_made(rig: &mut Rig, run: &RunId) {
        rig.worker.on_create(Ok(fresh()));
        assert_eq!(
            placed_on(rig, run, m2()),
            ResumeStep::Waiting(Wait::Workspace)
        );
        rig.view = with_resume_workspace("fac/8-1-r1", None).build();
    }

    /// A resumed session started in the new workspace.
    fn resumed(rig: &mut Rig, run: &RunId) -> ResumeStep {
        rig.worker.on_start_session(Ok(resumed_session()));
        pass(rig, run)
    }

    fn pr_on(head: &str, cross: bool) -> SimPull {
        SimPull {
            state: "OPEN",
            draft: true,
            head: head.into(),
            base: "main".into(),
            cross,
        }
    }

    fn pr_opened(rig: &Rig, run: &RunId) {
        rig.write(
            run,
            Event::PrOpened {
                pr: 12,
                url: URL12.into(),
            },
        );
    }

    /// What the worker will say the next turn read ended with.
    fn say(rig: &Rig, message: &str) {
        rig.worker.on_turn(Ok(TurnText {
            message: message.into(),
            question: None,
        }));
    }

    fn resumed_message(run: &RunId, branch: &str) -> messages::Message {
        messages::resumed(
            REPO,
            ISSUE,
            LOGIN,
            &ResumeFrom::Interrupted {
                run: run.clone(),
                branch: branch.into(),
            },
            None,
            BODY_PATH,
        )
    }

    fn plain_message() -> messages::Message {
        messages::launch(REPO, ISSUE, LOGIN, None, BODY_PATH)
    }

    fn create_request(rig: &Rig, call: &Call) -> CreateWorkspace {
        let _ = rig;
        match call {
            Call::Create { request, .. } => request.clone(),
            other => panic!("expected a create, got {other:?}"),
        }
    }

    fn the_stuck_comment(rig: &Rig) -> String {
        rig.sim
            .comments()
            .into_iter()
            .find(|body| body.contains("could not finish this issue"))
            .expect("a stuck comment")
    }

    /// The runner's `interruption.test.ts`, "interrupted: a resume". A session whose process
    /// ended before a result line is an interruption that Foundry resumes in a new workspace of
    /// the same run, where the runner sent the same session a message.
    mod interrupted_a_resume {
        use super::*;

        /// The runner's case: "resumes a session that failed right after a T3 timeout in its
        /// thread and keeps it building". Foundry: a failed session is an interruption with no
        /// evidence needed; the decision is recorded with its count, the archive of the earlier
        /// workspace is asked for, and the issue stays `building` with no stuck comment.
        #[test]
        fn resumes_a_session_that_failed_right_after_a_t3_timeout_in_its_thread_and_keeps_it_building()
         {
            let (rig, run) = interrupted();
            let before = event_count(&rig, &run);
            pass_one(&rig, &run);
            let lines = interruptions(&rig, &run);
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0]["resumes"], 1);
            assert_eq!(lines[0]["relaunch"], false);
            assert_eq!(
                resume_steps(&rig, &run),
                vec![
                    format!("resume-archiving 1 {OLD}"),
                    "resume-archive-asked 1".to_string()
                ]
            );
            assert_eq!(rig.worker.calls(), vec![probe(1), archived_call()]);
            assert_eq!(rig.sim.labels(), vec!["factory:building"]);
            assert!(rig.sim.comments().is_empty());
            assert!(!rig.finished(&run));
            let causes: Vec<String> = rig
                .events(&run)
                .into_iter()
                .skip(before)
                .map(|e| e.cause.unwrap()["reason"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(
                causes,
                vec![
                    "session-interrupted",
                    "resume-archiving",
                    "resume-archive-asked"
                ]
            );
        }

        /// The runner's case: "resumes a cancelled session after a T3 timeout too". Foundry: a
        /// session that finished with no result line (its process exited) resumes as a failed
        /// one does.
        #[test]
        fn resumes_a_cancelled_session_after_a_t3_timeout_too() {
            let (mut rig, run) = interrupted();
            rig.view = fleet(Some(SessionState::Finished)).build();
            pass_one(&rig, &run);
            assert_eq!(interruptions(&rig, &run).len(), 1);
            assert_eq!(rig.sim.labels(), vec!["factory:building"]);
        }

        /// The runner's case: "records the old reason code for a stuck decision of an ended
        /// badly run". Foundry: the cap's stuck path records `session-ended-badly`.
        #[test]
        fn records_the_old_reason_code_for_a_stuck_decision_of_an_ended_badly_run() {
            let (mut rig, run) = interrupted();
            rig.config.limits.interruption_resumes = 0;
            assert!(matches!(pass(&rig, &run), ResumeStep::Stuck { .. }));
            let stuck = rig
                .events(&run)
                .into_iter()
                .find(|e| e.event == "stuck")
                .unwrap();
            let cause = stuck.cause.unwrap();
            assert_eq!(cause["step"], "advance");
            assert_eq!(cause["reason"], "session-ended-badly");
        }

        /// The runner's case: "leaves a thread that moved on since advance looked alone".
        /// Foundry: a session that works again (or idles) is no interruption; nothing is written
        /// and nothing is asked of the worker.
        #[test]
        fn leaves_a_thread_that_moved_on_since_advance_looked_alone() {
            let (mut rig, run) = interrupted();
            rig.view = fleet(Some(SessionState::Working)).build();
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            rig.view = fleet(Some(SessionState::Idle))
                .turn(turn_of(session(), old(), 1, 1))
                .build();
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
            assert!(rig.worker.calls().is_empty());
        }

        /// The runner's case: "makes a resumed run wait for a newer run and then read it as any
        /// other". Foundry: while the resume is owed, every pass carries it on and `building`
        /// leaves the run alone; once the new session is launched the run is read as any other:
        /// a working session is skipped, and a session that is no longer interrupted is no
        /// interruption.
        #[test]
        fn makes_a_resumed_run_wait_for_a_newer_run_and_then_read_it_as_any_other() {
            let (mut rig, run) = interrupted();
            pass_one(&rig, &run);
            // The earlier workspace still shows on a machine that is not heard from: the
            // resume waits for it.
            rig.view = cut_off(secs(60));
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Archive));
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Archive));
            assert_eq!(interruptions(&rig, &run).len(), 1);
            rig.view = archived();
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::NeedsMachine(Placing::Resume { resumes: 1 })
            );
            resume_workspace_made(&mut rig, &run);
            assert!(matches!(
                resumed(&mut rig, &run),
                ResumeStep::Started { resumes: 1, .. }
            ));
            rig.view = with_resume_workspace("fac/8-1-r1", None)
                .session(session_record(
                    resumed_session(),
                    fresh(),
                    SessionState::Working,
                ))
                .turn(turn_of(resumed_session(), fresh(), 1, 1))
                .build();
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
        }
    }

    /// The runner's `interruption.test.ts`, "interrupted: the cap".
    mod interrupted_the_cap {
        use super::*;

        /// Interrupts the run's current session the way the fleet shows it: the session `thread`
        /// failed, in the workspace `workspace`.
        fn fail_the_session(rig: &mut Rig, thread: SessionId, workspace: WorkspaceId) {
            rig.view = FleetView::builder()
                .online(m1())
                .online(m2())
                .workspace(workspace_record(m2(), workspace, "w", None))
                .session(session_record(thread, workspace, SessionState::Failed))
                .build();
            never_written(rig);
        }

        /// One whole resume of a run: archived, placed, created, started. Returns the new session.
        fn resume_once(
            rig: &mut Rig,
            run: &RunId,
            k: u32,
            thread: SessionId,
            ws: WorkspaceId,
        ) -> SessionId {
            fail_the_session(rig, thread, ws);
            let step = pass(rig, run);
            assert!(
                matches!(step, ResumeStep::Archiving { resumes, .. } if resumes == k),
                "resume {k}: {step:?}"
            );
            rig.view = archived();
            assert_eq!(
                pass(rig, run),
                ResumeStep::NeedsMachine(Placing::Resume { resumes: k })
            );
            let made = WorkspaceId(uuid::Uuid::from_u128(0xc00 + u128::from(k)));
            let next = SessionId(uuid::Uuid::from_u128(0xd00 + u128::from(k)));
            rig.worker.on_create(Ok(made));
            placed_on(rig, run, m2());
            let name = format!("fac/8-1-r{k}");
            rig.view = FleetView::builder()
                .online(m1())
                .online(m2())
                .workspace(workspace_record(m2(), made, &name, None))
                .build();
            rig.worker.on_start_session(Ok(next));
            assert!(matches!(pass(rig, run), ResumeStep::Started { .. }));
            next
        }

        /// The runner's case: "is two resumes, and the third ended badly goes stuck". Foundry:
        /// each resume has its own line and count, and the third interruption of a cap of two
        /// ends the run stuck with the runner's words.
        #[test]
        fn is_two_resumes_and_the_third_ended_badly_goes_stuck() {
            let (mut rig, run) = failed_with_turns_unasked();
            let first = resume_once(&mut rig, &run, 1, session(), old());
            let second = resume_once(
                &mut rig,
                &run,
                2,
                first,
                WorkspaceId(uuid::Uuid::from_u128(0xc01)),
            );
            fail_the_session(&mut rig, second, WorkspaceId(uuid::Uuid::from_u128(0xc02)));
            let step = pass(&rig, &run);
            assert_eq!(
                step,
                ResumeStep::Stuck {
                    reason: "the session run ended as failed".into()
                }
            );
            let resumes: Vec<u64> = interruptions(&rig, &run)
                .iter()
                .map(|line| line["resumes"].as_u64().unwrap())
                .collect();
            assert_eq!(resumes, vec![1, 2]);
            assert!(rig.finished(&run));
            assert!(the_stuck_comment(&rig).contains("the session run ended as failed"));
        }

        /// The runner's case: "is read from the project limits".
        #[test]
        fn is_read_from_the_project_limits() {
            let (mut rig, run) = failed_with_turns_unasked();
            rig.config.limits.interruption_resumes = 3;
            let mut thread = session();
            let mut ws = old();
            for k in 1..=3 {
                thread = resume_once(&mut rig, &run, k, thread, ws);
                ws = WorkspaceId(uuid::Uuid::from_u128(0xc00 + u128::from(k)));
            }
            fail_the_session(&mut rig, thread, ws);
            assert!(matches!(pass(&rig, &run), ResumeStep::Stuck { .. }));
            assert_eq!(interruptions(&rig, &run).len(), 3);
        }

        /// The runner's case: "of zero sends an interrupted session to stuck at once".
        #[test]
        fn of_zero_sends_an_interrupted_session_to_stuck_at_once() {
            let (mut rig, run) = interrupted();
            rig.config.limits.interruption_resumes = 0;
            assert!(matches!(pass(&rig, &run), ResumeStep::Stuck { .. }));
            assert!(interruptions(&rig, &run).is_empty());
            assert!(archive_calls(&rig).is_empty());
            assert!(create_calls(&rig).is_empty());
            assert!(rig.finished(&run));
        }

        /// The runner's case: "used up sends a thread that is gone to stuck as thread-gone".
        #[test]
        fn used_up_sends_a_thread_that_is_gone_to_stuck_as_thread_gone() {
            let (mut rig, run) = interrupted();
            rig.config.limits.interruption_resumes = 0;
            rig.view = gone();
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Stuck {
                    reason: SESSION_GONE.into()
                }
            );
            let stuck = rig
                .events(&run)
                .into_iter()
                .find(|e| e.event == "stuck")
                .unwrap();
            assert_eq!(stuck.cause.unwrap()["reason"], "thread-gone");
        }

        /// The round's workspace, its session and its name, as `feedback` makes them.
        fn round_workspace(n: u32) -> WorkspaceId {
            WorkspaceId(uuid::Uuid::from_u128(0xe00 + u128::from(n)))
        }

        fn round_session(n: u32) -> SessionId {
            SessionId(uuid::Uuid::from_u128(0xf00 + u128::from(n)))
        }

        /// A run in `pr-open`: feedback round `n` moved it back to `building` and
        /// started its session, with the lines `feedback` writes for it.
        fn a_feedback_round(rig: &mut Rig, run: &RunId, n: u32) -> SessionId {
            let relaunching = || Event::FeedbackRelaunching {
                n,
                branch: BRANCH.into(),
                items: vec![format!("comment-{n}")],
                message: "Address the review.".into(),
            };
            rig.write(run, relaunching());
            rig.step(run, &format!("feedback-machine {n} {}", m2()));
            rig.moved(run, State::PrOpen, State::Building);
            rig.write(run, relaunching());
            rig.step(run, &format!("feedback-requested {n} fac/8-1-f{n}"));
            rig.step(
                run,
                &format!("feedback-workspace {n} {}", round_workspace(n)),
            );
            rig.step(run, &format!("feedback-sending {n}"));
            rig.write(
                run,
                Event::SessionLaunched {
                    thread_id: round_session(n).to_string(),
                    worktree: Some(round_workspace(n).to_string()),
                    branch: Some(BRANCH.into()),
                    model_instance: None,
                    model: Some("opus".into()),
                    model_options: None,
                    resumed_from: Some(ResumedFrom {
                        run: run.to_string(),
                        branch: BRANCH.into(),
                    }),
                    base: Some("main".into()),
                    title: Some("Make it faster".into()),
                },
            );
            round_session(n)
        }

        /// Acceptance (task 28): a feedback round starts `interruption_resumes` again. A run
        /// that used both of its resumes before `pr-open` is resumed, not stuck, on the first
        /// interruption of the round's session, and its resume workspace is named one past the
        /// run's highest (`-r3`), so no name is reused.
        #[test]
        fn a_feedback_round_starts_the_resumes_again() {
            let (mut rig, run) = failed_with_turns_unasked();
            let first = resume_once(&mut rig, &run, 1, session(), old());
            resume_once(
                &mut rig,
                &run,
                2,
                first,
                WorkspaceId(uuid::Uuid::from_u128(0xc01)),
            );
            rig.moved(&run, State::Building, State::PrOpen);
            let thread = a_feedback_round(&mut rig, &run, 1);
            let next = resume_once(&mut rig, &run, 3, thread, round_workspace(1));
            let names: Vec<String> = create_calls(&rig)
                .into_iter()
                .filter_map(|call| match call {
                    Call::Create { request, .. } => Some(request.name),
                    _ => None,
                })
                .collect();
            assert_eq!(names, vec!["fac/8-1-r1", "fac/8-1-r2", "fac/8-1-r3"]);
            assert!(!rig.finished(&run));
            // The round has one resume left of its two, and then the cap ends it.
            let next = resume_once(
                &mut rig,
                &run,
                4,
                next,
                WorkspaceId(uuid::Uuid::from_u128(0xc03)),
            );
            fail_the_session(&mut rig, next, WorkspaceId(uuid::Uuid::from_u128(0xc04)));
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Stuck {
                    reason: "the session run ended as failed".into()
                }
            );
            let resumes: Vec<u64> = interruptions(&rig, &run)
                .iter()
                .map(|line| line["resumes"].as_u64().unwrap())
                .collect();
            assert_eq!(resumes, vec![1, 2, 3, 4]);
        }

        /// Only the newest round's interruptions count: two rounds each get the whole cap, and a
        /// cap of zero still ends the round's first interruption stuck.
        #[test]
        fn only_the_interruptions_after_the_newest_round_count() {
            let (mut rig, run) = failed_with_turns_unasked();
            rig.config.limits.interruption_resumes = 1;
            resume_once(&mut rig, &run, 1, session(), old());
            rig.moved(&run, State::Building, State::PrOpen);
            let thread = a_feedback_round(&mut rig, &run, 1);
            resume_once(&mut rig, &run, 2, thread, round_workspace(1));
            rig.moved(&run, State::Building, State::PrOpen);
            let thread = a_feedback_round(&mut rig, &run, 2);
            resume_once(&mut rig, &run, 3, thread, round_workspace(2));
            assert!(!rig.finished(&run));

            let (mut rig, run) = failed_with_turns_unasked();
            rig.config.limits.interruption_resumes = 0;
            rig.moved(&run, State::Building, State::PrOpen);
            let thread = a_feedback_round(&mut rig, &run, 1);
            fail_the_session(&mut rig, thread, round_workspace(1));
            assert!(matches!(pass(&rig, &run), ResumeStep::Stuck { .. }));
            assert!(interruptions(&rig, &run).is_empty());
        }

        /// A relaunch's interruption line (`relaunch: true`) is no resume, as `read_round` and
        /// the newest `resumes` read it: it does not count against the cap.
        #[test]
        fn a_relaunch_interruption_does_not_count_against_the_cap() {
            let (rig, run) = failed_with_turns_unasked();
            let line = |resumes, relaunch| Event::SessionInterrupted {
                resumes,
                relaunch,
                run_ordinal: None,
                branch: None,
                t3_error: None,
                ended_in_pass: 0,
            };
            rig.write(&run, line(0, true));
            rig.write(&run, line(1, false));
            assert_eq!(resumes_this_round(&rig.events(&run)), 1);
        }
    }

    /// The runner's `interruption.test.ts`, "interrupted: a send cut short". Foundry has no
    /// resume message to send: the steps a pass can be cut short between are the archive, the
    /// create and the start, and each is carried on from the ledger.
    mod interrupted_a_send_cut_short {
        use super::*;

        /// Foundry's own (the runner's "is sent again under the same command id" has no command
        /// id): a pass cut short after the decision and before the archive records the archive
        /// and asks for it on the next pass, with no second decision; while the workspace still
        /// shows on a machine that is heard from, a later pass asks for the archive again and
        /// never for the workspace.
        #[test]
        fn a_decision_cut_short_before_its_archive_asks_it_on_the_next_pass_without_deciding_again()
        {
            let (rig, run) = interrupted();
            rig.write(
                &run,
                Event::SessionInterrupted {
                    resumes: 1,
                    relaunch: false,
                    run_ordinal: None,
                    branch: None,
                    t3_error: None,
                    ended_in_pass: 0,
                },
            );
            pass_one(&rig, &run);
            assert_eq!(interruptions(&rig, &run).len(), 1);
            assert_eq!(rig.worker.calls(), vec![archived_call()]);
            assert_eq!(
                resume_steps(&rig, &run),
                vec![
                    format!("resume-archiving 1 {OLD}"),
                    "resume-archive-asked 1".to_string()
                ]
            );
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Archiving {
                    resumes: 1,
                    workspace: Some(old())
                }
            );
            assert_eq!(rig.worker.calls(), vec![archived_call(), archived_call()]);
            assert_eq!(interruptions(&rig, &run).len(), 1);
        }

        /// The runner's case: "that reached the thread only gets its step". Foundry: a session
        /// that was started and whose `session_launched` was not written is found in the new
        /// workspace and recorded, not started a second time.
        #[test]
        fn that_reached_the_thread_only_gets_its_step() {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            resume_workspace_made(&mut rig, &run);
            rig.step(&run, "resume-sending 1");
            rig.view = with_resume_workspace("fac/8-1-r1", None)
                .session(session_record(
                    resumed_session(),
                    fresh(),
                    SessionState::Working,
                ))
                .turn(turn_of(resumed_session(), fresh(), 0, 1))
                .build();
            assert!(matches!(
                pass(&rig, &run),
                ResumeStep::Started { resumes: 1, session } if session == resumed_session()
            ));
            assert!(start_calls(&rig).is_empty());
            let launched: Vec<RunEvent> = rig
                .events(&run)
                .into_iter()
                .filter(|e| e.event == "session_launched")
                .collect();
            assert_eq!(launched.len(), 2);
            assert_eq!(launched[1].data["thread_id"], resumed_session().to_string());
            assert_eq!(launched[1].data["worktree"], fresh().to_string());
        }

        /// The runner's case: "that fails leaves the decision in the ledger and the next advance
        /// sends again". Foundry: an archive the worker failed leaves the decision recorded, and
        /// the next pass asks again; the attempt that failed was one resume, not two.
        #[test]
        fn that_fails_leaves_the_decision_in_the_ledger_and_the_next_advance_sends_again() {
            let (rig, run) = interrupted();
            rig.worker
                .on_archive(Err(WorkerError::Failed("disk full".into())));
            let error = advance(&rig.ctx(), &run).unwrap_err();
            assert!(matches!(error, RunError::Worker(WorkerError::Failed(_))));
            assert_eq!(interruptions(&rig, &run).len(), 1);
            assert!(!resume_steps(&rig, &run).contains(&"resume-archive-asked 1".to_string()));
            pass_one(&rig, &run);
            assert_eq!(archive_calls(&rig).len(), 2);
            assert_eq!(interruptions(&rig, &run).len(), 1);
            assert_eq!(
                resume_steps(&rig, &run),
                vec![
                    format!("resume-archiving 1 {OLD}"),
                    "resume-archive-asked 1".to_string()
                ]
            );
        }

        /// The runner's case: "into a thread that is gone is the thread-gone case". Foundry: a
        /// resume owed whose earlier session is gone carries on, with no second decision.
        #[test]
        fn into_a_thread_that_is_gone_is_the_thread_gone_case() {
            let (mut rig, run) = interrupted();
            pass_one(&rig, &run);
            rig.view = gone();
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::NeedsMachine(Placing::Resume { resumes: 1 })
            );
            assert_eq!(interruptions(&rig, &run).len(), 1);
        }

        /// The runner's case: "before its send is not charged twice when the thread is gone",
        /// for a cap of 2 and of 1.
        #[test]
        fn before_its_send_is_not_charged_twice_when_the_thread_is_gone() {
            for limit in [2, 1] {
                let (mut rig, run) = interrupted();
                rig.config.limits.interruption_resumes = limit;
                rig.write(
                    &run,
                    Event::SessionInterrupted {
                        resumes: 1,
                        relaunch: false,
                        run_ordinal: None,
                        branch: None,
                        t3_error: None,
                        ended_in_pass: 0,
                    },
                );
                rig.view = gone();
                pass_one(&rig, &run);
                assert_eq!(interruptions(&rig, &run).len(), 1, "limit {limit}");
                assert!(!rig.finished(&run), "limit {limit}");
            }
        }
    }

    /// The runner's `interruption.test.ts`, "interrupted: a thread that is gone".
    mod interrupted_a_thread_that_is_gone {
        use super::*;

        /// The first message the resume starts its session with.
        fn first_message(rig: &Rig) -> messages::Message {
            match start_calls(rig).pop() {
                Some(Call::StartSession { first, .. }) => first,
                other => panic!("expected a start, got {other:?}"),
            }
        }

        /// Drives a gone session to a started resume, the pull request's head or the run's own
        /// branch being on origin as the test set.
        fn resume_to_the_end(rig: &mut Rig, run: &RunId) {
            pass_one(rig, run);
            rig.view = archived();
            assert!(matches!(pass(rig, run), ResumeStep::NeedsMachine(_)));
            resume_workspace_made(rig, run);
            assert!(matches!(resumed(rig, run), ResumeStep::Started { .. }));
        }

        /// The runner's case: "after a T3 outage is relaunched from its branch and not stuck".
        /// Foundry: a session gone from the fleet is resumed on the branch the run pushed, the
        /// issue stays `building`, no stuck comment is posted, and the launch line names the run
        /// and the branch it resumed from.
        #[test]
        fn after_a_t3_outage_is_relaunched_from_its_branch_and_not_stuck() {
            let (mut rig, run) = interrupted();
            rig.view = gone();
            rig.sim.add_branch(BRANCH);
            resume_to_the_end(&mut rig, &run);
            assert_eq!(rig.sim.labels(), vec!["factory:building"]);
            assert!(!the_comments_have_a_stuck_one(&rig));
            let request = create_request(&rig, &create_calls(&rig)[0]);
            assert_eq!(
                request.checkout,
                Checkout::Branch {
                    name: BRANCH.into()
                }
            );
            assert_eq!(first_message(&rig), resumed_message(&run, BRANCH));
            let launched = rig
                .events(&run)
                .into_iter()
                .rfind(|e| e.event == "session_launched")
                .unwrap();
            assert_eq!(launched.data["resumed_from"]["branch"], BRANCH);
            assert_eq!(launched.data["resumed_from"]["run"], run.to_string());
            assert_eq!(launched.cause.unwrap()["reason"], "resumed-from-branch");
        }

        fn the_comments_have_a_stuck_one(rig: &Rig) -> bool {
            rig.sim
                .comments()
                .iter()
                .any(|body| body.contains("could not finish this issue"))
        }

        /// The runner's case: "has the interrupted paragraph in the factory's fixed words".
        #[test]
        fn has_the_interrupted_paragraph_in_the_factorys_fixed_words() {
            let (mut rig, run) = interrupted();
            rig.view = gone();
            rig.sim.add_branch(BRANCH);
            resume_to_the_end(&mut rig, &run);
            let message = first_message(&rig);
            let paragraph =
                "A previous attempt of this issue (rawphp__softfac__8__1) was interrupted when";
            assert!(
                message
                    .body
                    .as_deref()
                    .unwrap_or_default()
                    .contains(paragraph),
                "{message:?}"
            );
            assert!(!format!("{message:?}").contains("time limit"));
        }

        /// The runner's case: "relaunched without a recorded branch starts the next attempt from
        /// base". Foundry: a run with no pushed branch and no pull request resumes on a new
        /// branch from the base, named for the resume workspace, with the plain launch words.
        #[test]
        fn relaunched_without_a_recorded_branch_starts_the_next_attempt_from_base() {
            let mut rig = Rig::new();
            let run = run_id(1);
            building_with(&rig, &run, launch(None, Some("main")));
            rig.view = gone();
            resume_to_the_end(&mut rig, &run);
            let request = create_request(&rig, &create_calls(&rig)[0]);
            assert_eq!(
                request.checkout,
                Checkout::NewBranch {
                    base: "main".into(),
                    branch: Some("fac/8-1-r1".into()),
                }
            );
            assert_eq!(first_message(&rig), plain_message());
            assert_eq!(interruptions(&rig, &run)[0]["branch"], Value::Null);
        }

        /// The runner's case: "relaunched without a branch starts the next attempt from the
        /// run's own base".
        #[test]
        fn relaunched_without_a_branch_starts_the_next_attempt_from_the_runs_own_base() {
            let mut rig = Rig::new();
            let run = run_id(1);
            building_with(&rig, &run, launch(None, Some("feature/x")));
            rig.view = gone();
            resume_to_the_end(&mut rig, &run);
            let request = create_request(&rig, &create_calls(&rig)[0]);
            assert_eq!(
                request.checkout,
                Checkout::NewBranch {
                    base: "feature/x".into(),
                    branch: Some("fac/8-1-r1".into()),
                }
            );
            assert_eq!(
                first_message(&rig),
                messages::launch(REPO, ISSUE, LOGIN, Some("feature/x"), BODY_PATH)
            );
            let launched = rig
                .events(&run)
                .into_iter()
                .rfind(|e| e.event == "session_launched")
                .unwrap();
            assert_eq!(launched.data["base"], "feature/x");
        }

        /// The runner's case: "relaunched starts from base when the branch is not on origin".
        #[test]
        fn relaunched_starts_from_base_when_the_branch_is_not_on_origin() {
            let (mut rig, run) = interrupted();
            rig.view = gone();
            // `fac/8-1` was never pushed.
            resume_to_the_end(&mut rig, &run);
            assert_eq!(interruptions(&rig, &run)[0]["branch"], Value::Null);
            assert_eq!(first_message(&rig), plain_message());
        }

        /// The runner's case: "relaunched keeps the interrupted paragraph when its launch is sent
        /// again". Foundry: the first message asked for again by count has the resume's words.
        #[test]
        fn relaunched_keeps_the_interrupted_paragraph_when_its_launch_is_sent_again() {
            let (mut rig, run) = interrupted();
            rig.view = gone();
            rig.sim.add_branch(BRANCH);
            resume_to_the_end(&mut rig, &run);
            rig.view = with_resume_workspace("fac/8-1-r1", None)
                .session(session_record(
                    resumed_session(),
                    fresh(),
                    SessionState::Idle,
                ))
                .build();
            rig.worker.on_send(Ok(0));
            assert!(matches!(pass(&rig, &run), ResumeStep::Resent { .. }));
            assert_eq!(
                sends(&rig),
                vec![Call::Send {
                    session: resumed_session(),
                    message: resumed_message(&run, BRANCH),
                    only_if_prompts: Some(0),
                }]
            );
            assert!(
                rig.steps(&run)
                    .contains(&"resume-resending 1 0".to_string())
            );
        }

        /// The runner's case: "carries the counter over a relaunch so the new attempt has the
        /// rest of the cap". Foundry: the counter is the ledger's `resumes`, so the second
        /// interruption of a run is the second resume, and the cap's rest is what remains.
        #[test]
        fn carries_the_counter_over_a_relaunch_so_the_new_attempt_has_the_rest_of_the_cap() {
            let mut rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.write(
                &run,
                Event::SessionInterrupted {
                    resumes: 1,
                    relaunch: false,
                    run_ordinal: None,
                    branch: None,
                    t3_error: None,
                    ended_in_pass: 0,
                },
            );
            rig.write(&run, launch(Some("fac/8-1-r1"), Some("main")));
            rig.view = failed();
            never_written(&rig);
            assert!(matches!(
                pass(&rig, &run),
                ResumeStep::Archiving { resumes: 2, .. }
            ));
            assert_eq!(
                interruptions(&rig, &run)
                    .iter()
                    .map(|line| line["resumes"].as_u64().unwrap())
                    .collect::<Vec<_>>(),
                vec![1, 2]
            );
        }

        /// The runner's case: "cut short is finished by advance without a new decision".
        #[test]
        fn cut_short_is_finished_by_advance_without_a_new_decision() {
            let (mut rig, run) = interrupted();
            rig.view = gone();
            pass_one(&rig, &run);
            rig.view = archived();
            assert!(matches!(pass(&rig, &run), ResumeStep::NeedsMachine(_)));
            // The create is cut short: asked, its answer lost.
            rig.worker
                .on_create(Err(WorkerError::Unreachable("timed out".into())));
            assert!(advance(&rig.ctx(), &run).is_ok());
            let error = placed(&rig.ctx(), &run, Target::Machine(m2())).unwrap_err();
            assert!(matches!(
                error,
                RunError::Worker(WorkerError::Unreachable(_))
            ));
            rig.worker.on_create(Ok(fresh()));
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Workspace));
            assert_eq!(interruptions(&rig, &run).len(), 1);
            assert_eq!(create_calls(&rig).len(), 2);
        }

        /// The runner's case: "refuses a finished run and a run of another repo".
        #[test]
        fn refuses_a_finished_run_and_a_run_of_another_repo() {
            let (rig, run) = interrupted();
            let foreign = RunId::new("rawphp/other", 8, 1).unwrap();
            assert!(is_skipped(&pass(&rig, &foreign)));
            assert!(rig.events(&foreign).is_empty());
            rig.ledger
                .finish(
                    &run,
                    crate::factory::ledger::events::Outcome::Stuck,
                    None,
                    None,
                    &crate::factory::ledger::events::Cause::new(
                        crate::factory::ledger::events::CauseStep::Advance,
                        "test",
                    ),
                    None,
                )
                .unwrap();
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
            assert!(rig.worker.calls().is_empty());
        }
    }

    /// The runner's `interruption.test.ts`, "a relaunch finds the branch": where the resume
    /// continues from.
    mod a_relaunch_finds_the_branch {
        use super::*;

        fn decided_branch(rig: &Rig, run: &RunId) -> Value {
            interruptions(rig, run)[0]["branch"].clone()
        }

        /// The runner's case: "takes the pull request's branch of a re-gate session over T3's
        /// own". Foundry: the open pull request's head wins over the run's own branch.
        #[test]
        fn takes_the_pull_requests_branch_of_a_re_gate_session_over_t3s_own() {
            let (rig, run) = interrupted();
            pr_opened(&rig, &run);
            rig.sim.set_pull(12, pr_on(PR_BRANCH, false));
            rig.sim.add_branch(BRANCH);
            // The pull request is reported, so the gate owns it: put the gate's send-back first.
            rig.write(
                &run,
                Event::GateSentBack {
                    attempt: 1,
                    run_ordinal: 1,
                },
            );
            pass_one(&rig, &run);
            assert_eq!(decided_branch(&rig, &run), PR_BRANCH);
        }

        /// The runner's case: "takes T3's own branch for a session that resumed from an earlier
        /// attempt's". Foundry: with no pull request, the branch the session worked on, when it
        /// is on origin.
        #[test]
        fn takes_t3s_own_branch_for_a_session_that_resumed_from_an_earlier_attempts() {
            let (rig, run) = interrupted();
            rig.sim.add_branch(BRANCH);
            pass_one(&rig, &run);
            assert_eq!(decided_branch(&rig, &run), BRANCH);
        }

        /// The runner's case: "falls back to the branch it resumed from when origin lacks T3's
        /// own".
        #[test]
        fn falls_back_to_the_branch_it_resumed_from_when_origin_lacks_t3s_own() {
            let mut rig = Rig::new();
            let run = run_id(1);
            building_with(
                &rig,
                &run,
                Event::SessionLaunched {
                    thread_id: SESSION.into(),
                    worktree: Some(OLD.into()),
                    branch: Some(BRANCH.into()),
                    model_instance: None,
                    model: Some("opus".into()),
                    model_options: None,
                    resumed_from: Some(ResumedFrom {
                        run: "rawphp__softfac__8__0".into(),
                        branch: "t3code/old".into(),
                    }),
                    base: Some("main".into()),
                    title: None,
                },
            );
            rig.view = failed();
            never_written(&rig);
            rig.sim.add_branch("t3code/old");
            pass_one(&rig, &run);
            assert_eq!(decided_branch(&rig, &run), "t3code/old");
        }

        /// The runner's case: "takes the head of the run's pull request when nothing else names
        /// one".
        #[test]
        fn takes_the_head_of_the_runs_pull_request_when_nothing_else_names_one() {
            let mut rig = Rig::new();
            let run = run_id(1);
            building_with(&rig, &run, launch(None, Some("main")));
            rig.view = failed();
            never_written(&rig);
            pr_opened(&rig, &run);
            rig.sim.set_pull(12, pr_on("t3code/pr-head", false));
            rig.write(
                &run,
                Event::GateSentBack {
                    attempt: 1,
                    run_ordinal: 1,
                },
            );
            pass_one(&rig, &run);
            assert_eq!(decided_branch(&rig, &run), "t3code/pr-head");
        }

        /// The runner's case: "does not resume from a pull request from a fork or with an odd
        /// head". Foundry: a pull request from a fork is not resumed from, and the resume starts
        /// from base; an odd head is refused before anything is asked (see the Foundry cases).
        #[test]
        fn does_not_resume_from_a_pull_request_from_a_fork_or_with_an_odd_head() {
            let mut rig = Rig::new();
            let run = run_id(1);
            building_with(&rig, &run, launch(None, Some("main")));
            rig.view = failed();
            never_written(&rig);
            pr_opened(&rig, &run);
            rig.write(
                &run,
                Event::GateSentBack {
                    attempt: 1,
                    run_ordinal: 1,
                },
            );
            rig.sim.set_pull(12, pr_on("t3code/pr-head", true));
            pass_one(&rig, &run);
            assert_eq!(decided_branch(&rig, &run), Value::Null);
            for head in ["--upload-pack=x", "a..b"] {
                let mut rig = Rig::new();
                let run = run_id(1);
                building_with(&rig, &run, launch(None, Some("main")));
                rig.view = failed();
                never_written(&rig);
                pr_opened(&rig, &run);
                rig.write(
                    &run,
                    Event::GateSentBack {
                        attempt: 1,
                        run_ordinal: 1,
                    },
                );
                rig.sim.set_pull(12, pr_on(head, false));
                let step = pass(&rig, &run);
                assert!(matches!(step, ResumeStep::Stuck { .. }), "{head} {step:?}");
                assert!(
                    rig.worker
                        .calls()
                        .iter()
                        .all(|call| matches!(call, Call::EndSessions { .. } | Call::Turn { .. })),
                    "{head}"
                );
            }
        }

        /// The runner's case: "starts the next attempt from base with no branch anywhere".
        #[test]
        fn starts_the_next_attempt_from_base_with_no_branch_anywhere() {
            let mut rig = Rig::new();
            let run = run_id(1);
            building_with(&rig, &run, launch(None, Some("main")));
            rig.view = failed();
            never_written(&rig);
            pass_one(&rig, &run);
            assert_eq!(decided_branch(&rig, &run), Value::Null);
        }

        /// The runner's case: "that cannot be read is an error before anything is written".
        #[test]
        fn that_cannot_be_read_is_an_error_before_anything_is_written() {
            let (rig, run) = interrupted();
            pr_opened(&rig, &run);
            rig.write(
                &run,
                Event::GateSentBack {
                    attempt: 1,
                    run_ordinal: 1,
                },
            );
            rig.sim.set_pull(12, pr_on(PR_BRANCH, false));
            rig.sim.fail("pr view 12", 1);
            let before = event_count(&rig, &run);
            assert!(advance(&rig.ctx(), &run).is_err());
            assert_eq!(event_count(&rig, &run), before);
            assert_eq!(rig.worker.calls(), vec![probe(1)]);
            // The next pass reads again and goes on.
            never_written(&rig);
            pass_one(&rig, &run);
        }
    }

    /// The runner's `interruption.test.ts`, "interrupted: a stop by hand".
    mod interrupted_a_stop_by_hand {
        use super::*;

        fn stop(rig: &Rig, run: &RunId) {
            rig.write(
                run,
                Event::StopRequested {
                    by: "operator".into(),
                },
            );
        }

        /// The runner's case: "is not relaunched when the thread is gone, whatever else ended
        /// badly". Foundry: a stop asked for belongs to `stop`: this step writes and asks
        /// nothing, however the session ended.
        #[test]
        fn is_not_relaunched_when_the_thread_is_gone_whatever_else_ended_badly() {
            let (mut rig, run) = interrupted();
            rig.view = gone();
            stop(&rig, &run);
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
            assert!(rig.worker.calls().is_empty());
        }

        /// The runner's case: "whose session ended badly is stuck by hand and not an
        /// interruption".
        #[test]
        fn whose_session_ended_badly_is_stuck_by_hand_and_not_an_interruption() {
            let (rig, run) = interrupted();
            stop(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert!(interruptions(&rig, &run).is_empty());
            assert!(rig.worker.calls().is_empty());
        }

        /// The runner's case: "that lands before the resume is sent is not a relaunch".
        /// Foundry: a stop after the decision and the archive stops the resume before its
        /// workspace is asked for.
        #[test]
        fn that_lands_before_the_resume_is_sent_is_not_a_relaunch() {
            let (mut rig, run) = interrupted();
            pass_one(&rig, &run);
            rig.view = archived();
            stop(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert!(create_calls(&rig).is_empty());
            assert_eq!(interruptions(&rig, &run).len(), 1);
        }
    }

    /// The criteria and rules of the issue that have no runner counterpart.
    mod foundry {
        use super::*;

        /// Acceptance: the archive and the new workspace are never asked for in one pass.
        #[test]
        fn the_archive_and_the_new_workspace_are_never_asked_for_in_one_pass() {
            let (mut rig, run) = interrupted();
            rig.sim.add_branch(BRANCH);
            // The first pass: the archive, and no create.
            pass_one(&rig, &run);
            assert_eq!(rig.worker.calls(), vec![probe(1), archived_call()]);
            // While the earlier workspace shows on a machine that is heard from, the archive is
            // asked for again, and the workspace is not.
            assert!(matches!(
                pass(&rig, &run),
                ResumeStep::Archiving { resumes: 1, .. }
            ));
            assert!(create_calls(&rig).is_empty());
            // The pass that sees it archived asks for a machine, and creates nothing.
            rig.view = archived();
            assert!(matches!(pass(&rig, &run), ResumeStep::NeedsMachine(_)));
            assert!(create_calls(&rig).is_empty());
            // The pass that places asks for the workspace and never for an archive.
            rig.worker.on_create(Ok(fresh()));
            placed_on(&rig, &run, m2());
            assert_eq!(create_calls(&rig).len(), 1);
            assert_eq!(archive_calls(&rig).len(), 2);
        }

        /// Acceptance (review focus 2): a pull request branch named `-x` is refused before any
        /// workspace is asked for.
        #[test]
        fn a_pull_request_branch_named_dash_x_is_refused_before_any_workspace_is_asked_for() {
            for head in [
                "-x",
                "has space",
                "ünï",
                "a..b",
                "--upload-pack=x",
                "a;b",
                "",
            ] {
                let (rig, run) = interrupted();
                pr_opened(&rig, &run);
                rig.write(
                    &run,
                    Event::GateSentBack {
                        attempt: 1,
                        run_ordinal: 1,
                    },
                );
                rig.sim.set_pull(12, pr_on(head, false));
                let step = pass(&rig, &run);
                assert_eq!(
                    step,
                    ResumeStep::Stuck {
                        reason: LAUNCH_FAILED.into()
                    },
                    "{head:?}"
                );
                // Nothing was asked: no archive, no create, no start, no decision.
                assert!(
                    rig.worker
                        .calls()
                        .iter()
                        .all(|call| matches!(call, Call::EndSessions { .. } | Call::Turn { .. })),
                    "{head:?}"
                );
                assert!(interruptions(&rig, &run).is_empty(), "{head:?}");
                assert!(rig.finished(&run), "{head:?}");
                let stuck = rig
                    .events(&run)
                    .into_iter()
                    .find(|e| e.event == "stuck")
                    .unwrap();
                assert_eq!(stuck.cause.unwrap()["reason"], "resume-launch-failed");
                assert!(
                    rig.steps(&run)
                        .iter()
                        .any(|step| step.contains(BRANCH_REFUSED)),
                    "{head:?}"
                );
            }
        }

        /// Acceptance: a resume on the machine that built the run moves the local branch to
        /// `origin`'s: it asks for the branch itself, never a new branch of an existing name.
        #[test]
        fn a_resume_on_the_machine_that_built_the_run_moves_the_local_branch_to_origins() {
            let (mut rig, run) = interrupted();
            rig.sim.add_branch(BRANCH);
            pass_one(&rig, &run);
            rig.view = archived();
            assert!(matches!(pass(&rig, &run), ResumeStep::NeedsMachine(_)));
            rig.worker.on_create(Ok(fresh()));
            // The same machine that built the run.
            assert_eq!(
                placed_on(&rig, &run, m1()),
                ResumeStep::Waiting(Wait::Workspace)
            );
            let calls = create_calls(&rig);
            assert_eq!(calls.len(), 1);
            let Call::Create { target, request } = &calls[0] else {
                panic!("a create");
            };
            assert_eq!(*target, Target::Machine(m1()));
            assert_eq!(
                request.checkout,
                Checkout::Branch {
                    name: BRANCH.into()
                }
            );
            assert_eq!(request.name, "fac/8-1-r1");
            assert_eq!(request.setup, Setup::Own);
            assert_eq!(
                request.tag,
                FactoryTag {
                    project: project_id(),
                    run: run.to_string()
                }
            );
        }

        /// Acceptance: the cap ends the run stuck, and a cap of zero resumes nothing.
        #[test]
        fn the_cap_ends_the_run_stuck() {
            for cap in [0u64, 1] {
                let (mut rig, run) = interrupted();
                rig.config.limits.interruption_resumes = cap;
                if cap == 1 {
                    rig.write(
                        &run,
                        Event::SessionInterrupted {
                            resumes: 1,
                            relaunch: false,
                            run_ordinal: None,
                            branch: None,
                            t3_error: None,
                            ended_in_pass: 0,
                        },
                    );
                    // The resume launched a session that failed too.
                    rig.step(&run, "resume-sending 1");
                    rig.write(&run, launch(Some("fac/8-1-r1"), Some("main")));
                }
                let step = pass(&rig, &run);
                assert_eq!(
                    step,
                    ResumeStep::Stuck {
                        reason: "the session run ended as failed".into()
                    },
                    "cap {cap}"
                );
                assert!(rig.finished(&run));
                assert!(archive_calls(&rig).is_empty());
                assert!(create_calls(&rig).is_empty());
                assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
                assert!(the_stuck_comment(&rig).contains("the session run ended as failed"));
            }
        }

        /// The stuck path keeps the workspace and ends the session.
        #[test]
        fn the_cap_keeps_the_workspace_and_ends_its_sessions() {
            let (mut rig, run) = interrupted();
            rig.config.limits.interruption_resumes = 0;
            pass(&rig, &run);
            assert_eq!(
                rig.worker.calls(),
                vec![probe(1), Call::EndSessions { workspace: old() }]
            );
        }

        /// A session of a machine unreachable for 15 minutes is an interruption; 14:59 is not.
        #[test]
        fn a_session_whose_machine_is_unreachable_for_15_minutes_is_interrupted() {
            let (mut rig, run) = interrupted();
            rig.view = cut_off(secs(14 * 60 + 59));
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
            assert!(rig.worker.calls().is_empty());
            rig.view = cut_off(secs(15 * 60));
            pass_one(&rig, &run);
            assert_eq!(interruptions(&rig, &run).len(), 1);
        }

        /// The cap on a machine that cannot be reached still ends the run, with the words that
        /// say why, and asks no session to end on it.
        #[test]
        fn the_cap_on_an_unreachable_machine_ends_the_run_with_its_words() {
            let (mut rig, run) = interrupted();
            rig.config.limits.interruption_resumes = 0;
            rig.view = cut_off(secs(15 * 60));
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Stuck {
                    reason: MACHINE_UNREACHABLE.into()
                }
            );
            assert!(rig.worker.calls().is_empty());
        }

        /// The earlier workspace's archive is waited for until it shows archived or its machine
        /// has been unreachable for 15 minutes: 14:59 waits and 15:00 goes on.
        #[test]
        fn the_archive_wait_ends_at_15_minutes_of_unreachable() {
            let (mut rig, run) = interrupted();
            rig.view = cut_off(secs(15 * 60));
            rig.worker
                .on_archive(Err(WorkerError::Unreachable("gone".into())));
            pass_one(&rig, &run);
            // The machine is gone: an answer of unreachable is no error, and nothing is asked
            // again of it.
            assert_eq!(archive_calls(&rig).len(), 1);
            rig.view = cut_off(secs(15 * 60 - 1));
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Archive));
            rig.view = cut_off(secs(15 * 60));
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::NeedsMachine(Placing::Resume { resumes: 1 })
            );
            assert_eq!(archive_calls(&rig).len(), 1);
        }

        /// A machine that answers unreachable to the archive is no error for the pass.
        #[test]
        fn an_archive_asked_of_an_unreachable_machine_is_no_error() {
            let (rig, run) = interrupted();
            rig.worker
                .on_archive(Err(WorkerError::Unreachable("gone".into())));
            pass_one(&rig, &run);
            assert!(resume_steps(&rig, &run).contains(&"resume-archive-asked 1".to_string()));
        }

        /// `placed` records the machine as a step before it acts, and a later pass uses it.
        #[test]
        fn placed_records_the_machine_before_it_creates_the_workspace() {
            let (rig, run) = archived_and_waiting_for_a_machine();
            rig.worker
                .on_create(Err(WorkerError::Unreachable("x".into())));
            let _ = placed(&rig.ctx(), &run, Target::Machine(m2()));
            let steps = resume_steps(&rig, &run);
            assert!(
                steps.contains(&format!("resume-machine 1 {}", m2())),
                "{steps:?}"
            );
            assert!(steps.contains(&"resume-requested 1 fac/8-1-r1".to_string()));
            // The machine stands: the next pass asks the same machine again.
            rig.worker.on_create(Ok(fresh()));
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Workspace));
            let calls = create_calls(&rig);
            assert_eq!(calls.len(), 2);
            assert!(
                calls
                    .iter()
                    .all(|call| matches!(call, Call::Create { target, .. } if *target == Target::Machine(m2())))
            );
        }

        /// A create whose answer was recorded and whose record has not arrived is waited for,
        /// never asked twice.
        #[test]
        fn a_create_whose_answer_is_recorded_is_never_asked_twice() {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            rig.worker.on_create(Ok(fresh()));
            placed_on(&rig, &run, m2());
            rig.view = archived();
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Workspace));
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Workspace));
            assert_eq!(create_calls(&rig).len(), 1);
            assert!(resume_steps(&rig, &run).contains(&format!("resume-workspace 1 {}", fresh())));
        }

        /// A pass cut after the create was asked and before its answer was recorded finds the
        /// record in the fleet and takes it, with no second ask.
        #[test]
        fn a_create_cut_short_before_its_answer_was_recorded_takes_the_record_the_fleet_shows() {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            rig.step(&run, &format!("resume-machine 1 {}", m2()));
            rig.step(&run, "resume-requested 1 fac/8-1-r1");
            rig.view = with_resume_workspace("fac/8-1-r1", Some(setup_terminal()))
                .terminal(terminal_record(TerminalState::Running, None))
                .build();
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Workspace));
            assert!(create_calls(&rig).is_empty());
            assert!(resume_steps(&rig, &run).contains(&format!("resume-workspace 1 {}", fresh())));
            // Its setup is running: waited for.
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Setup));
        }

        /// A create cut short with no record in the fleet is asked again, and the worker answers
        /// with the first by tag and name.
        #[test]
        fn a_create_cut_short_with_no_record_is_asked_again() {
            let (rig, run) = archived_and_waiting_for_a_machine();
            rig.step(&run, &format!("resume-machine 1 {}", m2()));
            rig.step(&run, "resume-requested 1 fac/8-1-r1");
            rig.worker.on_create(Ok(fresh()));
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Workspace));
            assert_eq!(create_calls(&rig).len(), 1);
            assert_eq!(
                resume_steps(&rig, &run)
                    .iter()
                    .filter(|step| step.starts_with("resume-requested"))
                    .count(),
                1
            );
        }

        /// A failed setup is stuck with the terminal's tail, as `start` does.
        #[test]
        fn a_failed_setup_is_stuck_with_the_tail() {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            resume_workspace_made(&mut rig, &run);
            rig.view = with_resume_workspace("fac/8-1-r1", Some(setup_terminal()))
                .terminal(terminal_record(TerminalState::Exited, Some(1)))
                .build();
            rig.worker.on_tail(Ok("npm ERR! missing script".into()));
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Stuck {
                    reason: crate::factory::runs::start::SETUP_FAILED.into()
                }
            );
            assert!(the_stuck_comment(&rig).contains("npm ERR! missing script"));
            assert!(start_calls(&rig).is_empty());
        }

        /// A setup that is running is waited for, and the session is not started.
        #[test]
        fn a_setup_that_is_running_is_waited_for() {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            resume_workspace_made(&mut rig, &run);
            rig.view = with_resume_workspace("fac/8-1-r1", Some(setup_terminal()))
                .terminal(terminal_record(TerminalState::Running, None))
                .build();
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Setup));
            assert!(start_calls(&rig).is_empty());
        }

        /// A setup that is running is advanced each pass with its workspace; one that exited
        /// cleanly is not.
        #[test]
        fn a_setup_that_is_running_is_advanced_and_one_that_exited_is_not() {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            resume_workspace_made(&mut rig, &run);
            rig.view = with_resume_workspace("fac/8-1-r1", Some(setup_terminal()))
                .terminal(terminal_record(TerminalState::Running, None))
                .build();
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Setup));
            let advanced: Vec<_> = rig
                .worker
                .host_calls()
                .into_iter()
                .map(|(_, c)| c)
                .collect();
            assert_eq!(advanced, [HostCall::Advance { workspace: fresh() }]);
            rig.view = with_resume_workspace("fac/8-1-r1", Some(setup_terminal()))
                .terminal(terminal_record(TerminalState::Exited, Some(0)))
                .build();
            assert!(matches!(pass(&rig, &run), ResumeStep::Started { .. }));
            assert_eq!(rig.worker.host_calls().len(), 1);
        }

        /// The session starts with the resume's first message on the pull request's branch, and
        /// the launch line and the status comment name what started.
        #[test]
        fn the_resumed_session_is_started_recorded_and_named_in_the_status() {
            let mut rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.view = failed();
            never_written(&rig);
            pr_opened(&rig, &run);
            rig.write(
                &run,
                Event::GateSentBack {
                    attempt: 1,
                    run_ordinal: 1,
                },
            );
            rig.sim.set_pull(12, pr_on(PR_BRANCH, false));
            pass_one(&rig, &run);
            rig.view = archived();
            pass(&rig, &run);
            resume_workspace_made(&mut rig, &run);
            let started = resumed(&mut rig, &run);
            assert_eq!(
                started,
                ResumeStep::Started {
                    resumes: 1,
                    session: resumed_session()
                }
            );
            assert_eq!(
                start_calls(&rig),
                vec![Call::StartSession {
                    workspace: fresh(),
                    project: project_id(),
                    login: LOGIN.into(),
                    model: "opus".into(),
                    effort: rig.settings.fleet.effort.clone(),
                    first: resumed_message(&run, PR_BRANCH),
                }]
            );
            let events = rig.names(&run);
            let sending_at = events.iter().rposition(|n| n == "step_changed").unwrap();
            assert!(
                sending_at
                    < events
                        .iter()
                        .rposition(|n| n == "session_launched")
                        .unwrap()
            );
            let launched = rig
                .events(&run)
                .into_iter()
                .rfind(|e| e.event == "session_launched")
                .unwrap();
            assert_eq!(launched.data["thread_id"], resumed_session().to_string());
            assert_eq!(launched.data["worktree"], fresh().to_string());
            assert_eq!(launched.data["branch"], PR_BRANCH);
            assert_eq!(launched.data["base"], "main");
            assert_eq!(launched.data["resumed_from"]["branch"], PR_BRANCH);
            let status = rig.sim.comments().pop().unwrap();
            assert!(status.contains("**Factory status:** building"), "{status}");
            assert!(status.contains(&m2().to_string()), "{status}");
            // A comment hides what looks like a token, so the id itself is not shown.
            assert!(!status.contains("Session: none"), "{status}");
            // Once launched the interruption is sent: a later pass is skipped.
            rig.view = with_resume_workspace("fac/8-1-r1", None)
                .session(session_record(
                    resumed_session(),
                    fresh(),
                    SessionState::Working,
                ))
                .turn(turn_of(resumed_session(), fresh(), 0, 1))
                .build();
            assert!(is_skipped(&pass(&rig, &run)));
        }

        /// A session started whose answer was lost, before its `session_launched` was written,
        /// is ended and started again, as `start` does, never left running beside the new one.
        #[test]
        fn a_start_cut_short_with_no_session_in_the_fleet_ends_the_workspaces_sessions_first() {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            resume_workspace_made(&mut rig, &run);
            rig.step(&run, "resume-sending 1");
            rig.worker.on_start_session(Ok(resumed_session()));
            assert!(matches!(pass(&rig, &run), ResumeStep::Started { .. }));
            let calls = rig.worker.calls();
            let ended = calls
                .iter()
                .position(|c| matches!(c, Call::EndSessions { workspace } if *workspace == fresh()))
                .unwrap();
            let started = calls
                .iter()
                .position(|c| matches!(c, Call::StartSession { .. }))
                .unwrap();
            assert!(ended < started);
            assert!(resume_steps(&rig, &run).contains(&"resume-ending-sessions 1".to_string()));
        }

        /// A worker that refuses the start ends the run stuck as a refused launch.
        #[test]
        fn a_start_the_worker_refused_is_stuck() {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            resume_workspace_made(&mut rig, &run);
            rig.worker
                .on_start_session(Err(WorkerError::Failed("no model".into())));
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Stuck {
                    reason: LAUNCH_FAILED.into()
                }
            );
        }

        /// The first message is asked for again by count only for a session that is idle with
        /// no prompt, and not for one that has taken it.
        #[test]
        fn the_first_message_is_resent_only_to_an_idle_session_that_has_not_taken_it() {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            resume_workspace_made(&mut rig, &run);
            resumed(&mut rig, &run);
            // Idle with a turn that shows a prompt: it has taken the message.
            rig.view = with_resume_workspace("fac/8-1-r1", None)
                .session(session_record(
                    resumed_session(),
                    fresh(),
                    SessionState::Idle,
                ))
                .turn(turn_of(resumed_session(), fresh(), 1, 1))
                .build();
            assert!(is_skipped(&pass(&rig, &run)));
            assert!(sends(&rig).is_empty());
            // Idle with no turn: asked for again under the first count.
            rig.view = with_resume_workspace("fac/8-1-r1", None)
                .session(session_record(
                    resumed_session(),
                    fresh(),
                    SessionState::Idle,
                ))
                .build();
            rig.worker.on_send(Ok(0));
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Resent {
                    session: resumed_session()
                }
            );
            assert_eq!(sends(&rig).len(), 1);
        }

        /// A claim whose named machine is unreachable for 15 minutes with no workspace is
        /// launched on another machine, spends no resume and tells the status comment.
        #[test]
        fn a_claim_whose_machine_is_unreachable_is_launched_on_another_and_spends_no_resume() {
            let mut rig = Rig::new();
            let run = run_id(1);
            claimed_not_built(&rig, &run);
            rig.view = FleetView::builder()
                .online(m2())
                .unreachable(m1(), secs(15 * 60))
                .build();
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::NeedsMachine(Placing::MovedClaim { unreachable: m1() })
            );
            assert_eq!(event_count(&rig, &run), 5);
            rig.worker.on_create(Ok(fresh()));
            assert_eq!(
                placed_on(&rig, &run, m2()),
                ResumeStep::Moved {
                    machine: m2(),
                    workspace: fresh()
                }
            );
            assert!(interruptions(&rig, &run).is_empty());
            let steps = resume_steps(&rig, &run);
            assert_eq!(steps, vec![format!("claim-moved {}", m2())]);
            assert!(
                rig.steps(&run)
                    .contains(&format!("workspace-created {}", fresh()))
            );
            // The step that names the machine comes before the create's answer.
            let rigged = rig.steps(&run);
            let moved_at = rigged
                .iter()
                .position(|s| s.starts_with("claim-moved"))
                .unwrap();
            let created_at = rigged
                .iter()
                .position(|s| s.starts_with("workspace-created"))
                .unwrap();
            assert!(moved_at < created_at);
            let calls = create_calls(&rig);
            let Call::Create { target, request } = &calls[0] else {
                panic!("a create");
            };
            assert_eq!(*target, Target::Machine(m2()));
            assert_eq!(request.name, "fac/8-1");
            assert_eq!(
                request.checkout,
                Checkout::NewBranch {
                    base: "main".into(),
                    branch: Some("fac/8-1".into())
                }
            );
            let status = rig.sim.comments().pop().unwrap();
            assert!(status.contains("**Factory status:** triage"), "{status}");
            assert!(status.contains(&m2().to_string()), "{status}");
            // No resume was spent, and the run goes on as a start.
            assert!(is_skipped(&pass(&rig, &run)));
        }

        /// The moved claim's workspace is found by `start` by its id.
        #[test]
        fn a_moved_claim_is_carried_on_by_start_from_the_workspace_by_id() {
            let mut rig = Rig::new();
            let run = run_id(1);
            claimed_not_built(&rig, &run);
            rig.view = FleetView::builder()
                .online(m2())
                .unreachable(m1(), secs(15 * 60))
                .build();
            rig.worker.on_create(Ok(fresh()));
            pass(&rig, &run);
            placed_on(&rig, &run, m2());
            rig.view = FleetView::builder()
                .online(m2())
                .unreachable(m1(), secs(15 * 60))
                .workspace(workspace_record(m2(), fresh(), "fac/8-1", None))
                .build();
            rig.worker.on_start_session(Ok(resumed_session()));
            let step = crate::factory::runs::start::advance(&rig.ctx(), &run).unwrap();
            assert!(
                matches!(step, crate::factory::runs::start::StartStep::Started { .. }),
                "{step:?}"
            );
            // It was started in the workspace the move made, and the machine was never asked.
            assert_eq!(start_calls(&rig).len(), 1);
            assert_eq!(create_calls(&rig).len(), 1);
        }

        /// A claim is moved only after 15 minutes, only when no workspace of its name shows, and
        /// never onto the machine that is unreachable.
        #[test]
        fn a_claim_is_moved_only_when_nothing_of_it_shows_and_its_machine_is_long_gone() {
            let mut rig = Rig::new();
            let run = run_id(1);
            claimed_not_built(&rig, &run);
            rig.view = FleetView::builder()
                .online(m2())
                .unreachable(m1(), secs(15 * 60 - 1))
                .build();
            assert!(is_skipped(&pass(&rig, &run)));
            rig.view = FleetView::builder()
                .online(m2())
                .unreachable(m1(), secs(15 * 60))
                .workspace(workspace_record(m1(), fresh(), "fac/8-1", None))
                .build();
            assert!(is_skipped(&pass(&rig, &run)));
            rig.view = FleetView::builder()
                .online(m2())
                .unreachable(m1(), secs(15 * 60))
                .build();
            assert!(matches!(
                pass(&rig, &run),
                ResumeStep::NeedsMachine(Placing::MovedClaim { .. })
            ));
            assert!(placed(&rig.ctx(), &run, Target::Machine(m1())).is_err());
            assert!(create_calls(&rig).is_empty());
            // Online again, the machine is start's to ask.
            rig.view = FleetView::builder().online(m1()).online(m2()).build();
            assert!(is_skipped(&pass(&rig, &run)));
        }

        /// A claim whose build was asked of a machine and answered is not moved.
        #[test]
        fn a_claim_that_has_its_workspace_recorded_is_not_moved() {
            let mut rig = Rig::new();
            let run = run_id(1);
            claimed_not_built(&rig, &run);
            rig.step(&run, &format!("workspace-created {OLD}"));
            rig.view = FleetView::builder()
                .online(m2())
                .unreachable(m1(), secs(60 * 60))
                .build();
            assert!(is_skipped(&pass(&rig, &run)));
        }

        /// A claim with no decision of `start` yet is start's: nothing is written.
        #[test]
        fn a_claim_start_has_not_decided_is_start_s() {
            let mut rig = Rig::new();
            let run = run_id(1);
            rig.step(&run, &format!("approved {LABELED_AT}"));
            rig.write(
                &run,
                Event::Claimed {
                    machine: m1().to_string(),
                    github_user: LOGIN.into(),
                },
            );
            rig.moved(&run, State::Ready, State::Triage);
            rig.view = FleetView::builder()
                .online(m2())
                .unreachable(m1(), secs(60 * 60))
                .build();
            assert!(is_skipped(&pass(&rig, &run)));
        }

        /// The ledger of a claim `start` decided and asked its machine for.
        pub(super) fn claimed_not_built(rig: &Rig, run: &RunId) {
            rig.step(run, &format!("approved {LABELED_AT}"));
            rig.write(
                run,
                Event::Claimed {
                    machine: m1().to_string(),
                    github_user: LOGIN.into(),
                },
            );
            rig.moved(run, State::Ready, State::Triage);
            rig.step(run, "launch-model opus");
            rig.step(run, &format!("workspace-requested {BRANCH}"));
            rig.sim.set_labels(&["factory:triage"]);
            rig.sim.set_assignees(&[LOGIN]);
        }

        /// A turn that was read already, and whose session then ended, is an interruption with
        /// no read of it again.
        #[test]
        fn a_turn_read_already_is_not_read_again() {
            let (mut rig, run) = interrupted();
            rig.step(&run, "turn-read 1");
            rig.view = fleet(Some(SessionState::Failed))
                .turn(turn_of(session(), old(), 1, 1))
                .build();
            pass_one(&rig, &run);
            assert!(
                rig.worker
                    .calls()
                    .iter()
                    .all(|call| !matches!(call, Call::Turn { n: 1, .. }))
            );
            assert!(rig.worker.calls().contains(&probe(2)));
        }

        /// A run whose pull request waits for the gate is the gate's.
        #[test]
        fn a_run_waiting_for_the_gate_is_not_resumes() {
            let (rig, run) = interrupted();
            pr_opened(&rig, &run);
            rig.sim.set_pull(12, pr_on(PR_BRANCH, false));
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
            assert!(rig.worker.calls().is_empty());
        }

        /// A run that is not building, or has a relaunch or a rate limit decided, is not this
        /// step's.
        #[test]
        fn a_run_in_another_state_or_with_a_runners_relaunch_is_not_resumes() {
            let (rig, run) = interrupted();
            rig.write(
                &run,
                Event::SessionInterrupted {
                    resumes: 1,
                    relaunch: true,
                    run_ordinal: None,
                    branch: None,
                    t3_error: None,
                    ended_in_pass: 0,
                },
            );
            assert!(is_skipped(&pass(&rig, &run)));
            let (rig, run) = interrupted();
            rig.write(
                &run,
                Event::SessionRateLimited {
                    run_ordinal: 1,
                    branch: None,
                    resumes: 0,
                    model: None,
                },
            );
            assert!(is_skipped(&pass(&rig, &run)));
            let (rig, run) = interrupted();
            rig.moved(&run, State::Building, State::PrOpen);
            assert!(is_skipped(&pass(&rig, &run)));
            assert!(rig.worker.calls().is_empty());
        }

        /// A person closed the issue while the run was interrupted: the run is ended, and nothing
        /// is asked of a worker.
        #[test]
        fn a_closed_issue_ends_an_interrupted_run() {
            let (rig, run) = interrupted();
            rig.sim.set_closed(true);
            assert_eq!(pass(&rig, &run), ResumeStep::Abandoned);
            assert!(rig.finished(&run));
            assert!(interruptions(&rig, &run).is_empty());
            assert!(archive_calls(&rig).is_empty());
            assert!(create_calls(&rig).is_empty());
        }

        /// Acceptance: a lost lease writes nothing more and asks nothing.
        #[test]
        fn a_lost_lease_writes_nothing() {
            let (mut rig, run) = interrupted();
            rig.guard = Box::new(Lost);
            let before = event_count(&rig, &run);
            let error = advance(&rig.ctx(), &run).unwrap_err();
            assert!(matches!(error, RunError::LeaseLost(_)));
            assert_eq!(event_count(&rig, &run), before);
            assert!(rig.worker.calls().is_empty());
            // A lease lost in the middle of a pass: the decision is written and the archive is
            // not asked, and the pass that holds the lease again finishes it.
            let (mut rig, run) = interrupted();
            rig.guard = Box::new(HeldFor::new(3));
            assert!(advance(&rig.ctx(), &run).is_err());
            assert_eq!(rig.worker.calls(), vec![probe(1)]);
            let mid = event_count(&rig, &run);
            rig.guard = Box::new(crate::factory::worker::AlwaysHeld(1));
            pass_one(&rig, &run);
            assert_eq!(interruptions(&rig, &run).len(), 1);
            assert!(event_count(&rig, &run) > mid);
        }

        /// `orphans`: the workspaces of a run that has moved on, for a machine that is heard
        /// from again.
        mod orphans_of_a_run {
            use super::*;

            /// The run after its resume started: the resume workspace is the active one.
            fn resumed_run() -> (Rig, RunId) {
                let (mut rig, run) = archived_and_waiting_for_a_machine();
                resume_workspace_made(&mut rig, &run);
                resumed(&mut rig, &run);
                (rig, run)
            }

            fn orphan_list(rig: &Rig, run: &RunId, view: &FleetView) -> Vec<WorkspaceId> {
                orphans(view, project_id(), run, &rig.events(run))
            }

            #[test]
            fn lists_the_earlier_workspace_of_a_machine_that_is_heard_from_again() {
                let (rig, run) = resumed_run();
                let view = with_resume_workspace("fac/8-1-r1", None)
                    .workspace(workspace_record(m1(), old(), BRANCH, None))
                    .build();
                assert_eq!(orphan_list(&rig, &run, &view), vec![old()]);
            }

            #[test]
            fn never_lists_the_active_workspace() {
                let (rig, run) = resumed_run();
                let view = with_resume_workspace("fac/8-1-r1", None).build();
                assert_eq!(orphan_list(&rig, &run, &view), vec![]);
            }

            #[test]
            fn does_not_list_a_workspace_of_a_machine_that_is_offline() {
                let (rig, run) = resumed_run();
                let view = FleetView::builder()
                    .online(m2())
                    .unreachable(m1(), secs(60 * 60))
                    .workspace(workspace_record(m2(), fresh(), "fac/8-1-r1", None))
                    .workspace(workspace_record(m1(), old(), BRANCH, None))
                    .build();
                assert_eq!(orphan_list(&rig, &run, &view), vec![]);
            }

            #[test]
            fn does_not_list_an_archived_workspace_or_another_runs() {
                let (rig, run) = resumed_run();
                let mut archived_record = workspace_record(m1(), old(), BRANCH, None);
                archived_record.deleted = true;
                let other = WorkspaceId(uuid::Uuid::from_u128(0xe01));
                let view = with_resume_workspace("fac/8-1-r1", None)
                    .workspace(archived_record)
                    .workspace(workspace_record(m1(), other, "fac/9-1", None))
                    .workspace(workspace_record(m1(), other, "fac/8-2", None))
                    .build();
                assert_eq!(orphan_list(&rig, &run, &view), vec![]);
            }

            #[test]
            fn does_not_list_a_gate_workspace() {
                let (rig, run) = resumed_run();
                let gate = WorkspaceId(uuid::Uuid::from_u128(0xe02));
                let view = with_resume_workspace("fac/8-1-r1", None)
                    .workspace(workspace_record(m1(), gate, "fac/8-1-g1", None))
                    .build();
                assert_eq!(orphan_list(&rig, &run, &view), vec![]);
            }

            #[test]
            fn lists_every_workspace_of_a_run_that_ended() {
                let (rig, run) = resumed_run();
                rig.ledger
                    .finish(
                        &run,
                        crate::factory::ledger::events::Outcome::PrOpen,
                        None,
                        None,
                        &crate::factory::ledger::events::Cause::new(
                            crate::factory::ledger::events::CauseStep::Advance,
                            "test",
                        ),
                        None,
                    )
                    .unwrap();
                let view = with_resume_workspace("fac/8-1-r1", None)
                    .workspace(workspace_record(m1(), old(), BRANCH, None))
                    .build();
                let mut listed = orphan_list(&rig, &run, &view);
                listed.sort();
                let mut wanted = vec![old(), fresh()];
                wanted.sort();
                assert_eq!(listed, wanted);
            }

            #[test]
            fn keeps_the_workspace_a_stuck_run_is_kept_for() {
                let (rig, run) = resumed_run();
                rig.write(
                    &run,
                    Event::Stuck {
                        reason: "gone stuck".into(),
                        branch: None,
                        commits_ahead: None,
                    },
                );
                rig.moved(&run, State::Building, State::Stuck);
                rig.ledger
                    .finish(
                        &run,
                        crate::factory::ledger::events::Outcome::Stuck,
                        None,
                        None,
                        &crate::factory::ledger::events::Cause::new(
                            crate::factory::ledger::events::CauseStep::Advance,
                            "test",
                        ),
                        None,
                    )
                    .unwrap();
                let view = with_resume_workspace("fac/8-1-r1", None).build();
                assert_eq!(orphan_list(&rig, &run, &view), vec![]);
            }

            #[test]
            fn does_not_list_a_resume_workspace_whose_answer_is_not_recorded_yet() {
                let (mut rig, run) = interrupted();
                pass_one(&rig, &run);
                rig.view = archived();
                pass(&rig, &run);
                rig.step(&run, &format!("resume-machine 1 {}", m2()));
                rig.step(&run, "resume-requested 1 fac/8-1-r1");
                let view = with_resume_workspace("fac/8-1-r1", None)
                    .workspace(workspace_record(m1(), old(), BRANCH, None))
                    .build();
                assert_eq!(orphan_list(&rig, &run, &view), vec![]);
            }

            #[test]
            fn lists_the_workspace_the_first_machine_made_of_a_moved_claim() {
                let rig = Rig::new();
                let run = run_id(1);
                rig.step(&run, &format!("approved {LABELED_AT}"));
                rig.write(
                    &run,
                    Event::Claimed {
                        machine: m1().to_string(),
                        github_user: LOGIN.into(),
                    },
                );
                rig.moved(&run, State::Ready, State::Triage);
                rig.step(&run, &format!("workspace-requested {BRANCH}"));
                rig.step(&run, &format!("claim-moved {}", m2()));
                rig.step(&run, &format!("workspace-created {}", fresh()));
                let view = FleetView::builder()
                    .online(m1())
                    .online(m2())
                    .workspace(workspace_record(m2(), fresh(), BRANCH, None))
                    .workspace(workspace_record(m1(), old(), BRANCH, None))
                    .build();
                assert_eq!(orphan_list(&rig, &run, &view), vec![old()]);
            }

            #[test]
            fn lists_an_older_feedback_workspace_and_keeps_the_newest() {
                let (rig, run) = resumed_run();
                let f1 = WorkspaceId(uuid::Uuid::from_u128(0xe11));
                let f2 = WorkspaceId(uuid::Uuid::from_u128(0xe12));
                for n in [1, 2] {
                    rig.write(
                        &run,
                        Event::FeedbackRelaunching {
                            n,
                            branch: PR_BRANCH.into(),
                            items: vec![],
                            message: String::new(),
                        },
                    );
                }
                let view = with_resume_workspace("fac/8-1-r1", None)
                    .workspace(workspace_record(m1(), f1, "fac/8-1-f1", None))
                    .workspace(workspace_record(m1(), f2, "fac/8-1-f2", None))
                    .build();
                assert_eq!(orphan_list(&rig, &run, &view), vec![f1]);
            }

            #[test]
            fn lists_nothing_for_a_run_with_no_events() {
                let rig = Rig::new();
                let view = fleet(None).build();
                assert_eq!(orphans(&view, project_id(), &run_id(1), &[]), vec![]);
                let _ = rig;
            }
        }
    }

    /// The runner's `interruption.test.ts`, "advance: a session that ended badly". Foundry's
    /// `advance` is the step that decides what `building` leaves to it, so each case keeps its
    /// name and asserts what this step does with such a session.
    mod advance_a_session_that_ended_badly {
        use super::*;

        /// Nothing but the decision is written: no GitHub write, no stuck, and nothing asked of
        /// the worker but the archive (a session that ended is first asked for its next turn,
        /// a read).
        fn writes_only_the_decision(rig: &Rig, run: &RunId) {
            assert_eq!(rig.sim.writes(), Vec::<String>::new());
            let names = rig.names(run);
            assert!(!names.contains(&"stuck".to_string()), "{names:?}");
            assert_eq!(interruptions(rig, run).len(), 1);
            let asked: Vec<Call> = rig
                .worker
                .calls()
                .into_iter()
                .filter(|call| *call != probe(1))
                .collect();
            assert_eq!(asked, vec![archived_call()]);
        }

        /// The runner's case: "writes nothing for a failed or cancelled run and says it ended
        /// badly". Foundry: a failed or finished session is said to be an interruption, and
        /// the pass writes only its decision and the archive step, nothing on GitHub.
        #[test]
        fn writes_nothing_for_a_failed_or_cancelled_run_and_says_it_ended_badly() {
            for state in [SessionState::Failed, SessionState::Finished] {
                let (mut rig, run) = interrupted();
                rig.view = fleet(Some(state)).build();
                pass_one(&rig, &run);
                writes_only_the_decision(&rig, &run);
            }
        }

        /// The runner's case: "says a thread that no longer exists ended badly and writes
        /// nothing".
        #[test]
        fn says_a_thread_that_no_longer_exists_ended_badly_and_writes_nothing() {
            let (mut rig, run) = interrupted();
            rig.view = gone();
            pass_one(&rig, &run);
            writes_only_the_decision(&rig, &run);
        }

        /// The runner's case: "reads a failed, interrupted or cancelled run that ended with a
        /// done line as done". Foundry: a finished or failed session whose newest unread turn
        /// ends with the result line is `building`'s: nothing is written and nothing asked.
        #[test]
        fn reads_a_failed_interrupted_or_cancelled_run_that_ended_with_a_done_line_as_done() {
            for state in [SessionState::Failed, SessionState::Finished] {
                let (mut rig, run) = failed_with_turns_unasked();
                rig.view = fleet(Some(state))
                    .turn(turn_of(session(), old(), 1, 1))
                    .build();
                say(&rig, DONE_MESSAGE);
                let before = event_count(&rig, &run);
                assert!(is_skipped(&pass(&rig, &run)));
                assert_eq!(event_count(&rig, &run), before);
                assert!(interruptions(&rig, &run).is_empty());
            }
        }

        /// The runner's case (from "a relaunch finds the branch"): "of a thread that is gone
        /// and never named a worktree still finishes". Foundry: a gone session whose launch
        /// named no workspace records the archive of none and asks for no archive.
        #[test]
        fn of_a_thread_that_is_gone_and_never_named_a_worktree_still_finishes() {
            let mut rig = Rig::new();
            let run = run_id(1);
            rig.step(&run, &format!("approved {LABELED_AT}"));
            rig.write(
                &run,
                Event::Claimed {
                    machine: m1().to_string(),
                    github_user: LOGIN.into(),
                },
            );
            rig.moved(&run, State::Ready, State::Triage);
            rig.step(&run, "approval-rechecked");
            rig.write(
                &run,
                Event::SessionLaunched {
                    thread_id: SESSION.into(),
                    worktree: None,
                    branch: Some(BRANCH.into()),
                    model_instance: None,
                    model: Some("opus".into()),
                    model_options: None,
                    resumed_from: None,
                    base: Some("main".into()),
                    title: None,
                },
            );
            rig.moved(&run, State::Triage, State::Building);
            rig.sim.set_labels(&["factory:building"]);
            rig.sim.set_assignees(&[LOGIN]);
            rig.view = gone();
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Archiving {
                    resumes: 1,
                    workspace: None
                }
            );
            assert_eq!(
                resume_steps(&rig, &run),
                vec![
                    "resume-archiving 1 none".to_string(),
                    "resume-archive-asked 1".to_string()
                ]
            );
            assert!(archive_calls(&rig).is_empty());
        }
    }

    /// Cases of the first review cycle.
    mod review_cycle_1 {
        use super::foundry::claimed_not_built;
        use super::*;

        fn m3() -> MachineId {
            MachineId(uuid::Uuid::from_u128(0x4))
        }

        fn fresh3() -> WorkspaceId {
            WorkspaceId(uuid::Uuid::from_u128(0xb33))
        }

        /// A resume workspace made on m2 and shown with no setup, ready for its session.
        fn at_the_start() -> (Rig, RunId) {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            resume_workspace_made(&mut rig, &run);
            (rig, run)
        }

        fn session_owned_by(
            owner: MachineId,
            id: SessionId,
            workspace: WorkspaceId,
            state: SessionState,
        ) -> Record<Session> {
            Record {
                owner,
                ..session_record(id, workspace, state)
            }
        }

        fn approval_edited_by_a_stranger() -> String {
            approval_answer(
                "2026-10-04T12:00:00Z",
                "tom",
                &[("2026-10-04T10:30:00Z", "mallory")],
            )
        }

        // B1: the archive is asked again while the workspace shows on a machine heard from.

        #[test]
        fn an_archive_answered_unreachable_is_asked_again_once_the_machine_is_heard_from() {
            let (mut rig, run) = interrupted();
            rig.worker
                .on_archive(Err(WorkerError::Unreachable("timed out".into())));
            pass_one(&rig, &run);
            // The machine is online with the workspace still showing: asked again, and no
            // workspace in the same pass.
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Archiving {
                    resumes: 1,
                    workspace: Some(old())
                }
            );
            assert_eq!(archive_calls(&rig).len(), 2);
            assert!(create_calls(&rig).is_empty());
            // Once it shows archived the next pass goes on.
            rig.view = archived();
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::NeedsMachine(Placing::Resume { resumes: 1 })
            );
            assert_eq!(archive_calls(&rig).len(), 2);
        }

        #[test]
        fn a_later_archive_that_fails_is_an_error_for_that_pass_and_asked_again() {
            let (rig, run) = interrupted();
            pass_one(&rig, &run);
            rig.worker
                .on_archive(Err(WorkerError::Failed("busy".into())));
            let error = advance(&rig.ctx(), &run).unwrap_err();
            assert!(matches!(error, RunError::Worker(WorkerError::Failed(_))));
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Archiving {
                    resumes: 1,
                    workspace: Some(old())
                }
            );
            assert_eq!(archive_calls(&rig).len(), 3);
            assert!(create_calls(&rig).is_empty());
        }

        #[test]
        fn a_machine_that_is_offline_for_a_while_is_waited_for_with_no_archive_asked() {
            let (mut rig, run) = interrupted();
            pass_one(&rig, &run);
            rig.view = cut_off(secs(60));
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Archive));
            assert_eq!(archive_calls(&rig).len(), 1);
        }

        // B2: a terminal path begun by another step is that step's.

        #[test]
        fn a_begun_stuck_path_buildings_is_not_an_interruption() {
            let (rig, run) = interrupted();
            rig.step(
                &run,
                "stuck-stop {\"code\":\"session-stuck\",\"stop\":{\"reason\":\"said stuck\"}}",
            );
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
            assert!(rig.worker.calls().is_empty());
        }

        #[test]
        fn a_begun_stuck_path_the_time_limits_is_not_an_interruption() {
            let (rig, run) = interrupted();
            rig.step(
                &run,
                "stuck-stop {\"code\":\"time-limit\",\"stop\":{\"reason\":\"the run ran past limits.minutes\"}}",
            );
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
            assert!(rig.worker.calls().is_empty());
        }

        #[test]
        fn a_stuck_comment_that_landed_before_its_line_is_not_an_interruption() {
            let (rig, run) = interrupted();
            rig.write(
                &run,
                Event::Stuck {
                    reason: "said stuck".into(),
                    branch: None,
                    commits_ahead: None,
                },
            );
            assert!(is_skipped(&pass(&rig, &run)));
            assert!(rig.worker.calls().is_empty());
        }

        #[test]
        fn a_questions_post_or_a_withdraw_begun_is_not_an_interruption() {
            for step in ["questions-commenting 0", "approval-withdrawn"] {
                let (rig, run) = interrupted();
                rig.step(&run, step);
                assert!(is_skipped(&pass(&rig, &run)), "{step}");
                assert!(rig.worker.calls().is_empty(), "{step}");
            }
            let (rig, run) = interrupted();
            rig.write(
                &run,
                Event::QuestionsPosted {
                    questions: "1. Which?".into(),
                },
            );
            assert!(is_skipped(&pass(&rig, &run)));
        }

        // B3: the approval and the text are read again before a session starts.

        #[test]
        fn a_body_edited_since_the_approval_is_stuck_and_starts_no_session() {
            let (rig, run) = at_the_start();
            rig.sim.set_approval(approval_edited_by_a_stranger());
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Stuck {
                    reason: messages::CHANGED_DURING_BUILD.into()
                }
            );
            assert!(start_calls(&rig).is_empty());
            assert!(!resume_steps(&rig, &run).contains(&"resume-sending 1".to_string()));
            assert!(rig.finished(&run));
        }

        /// The runner's case: "relaunched starts from base when the issue text was edited
        /// since the approval". Foundry: the run's own branch is not resumed from, and the
        /// resume starts from the base.
        #[test]
        fn relaunched_starts_from_base_when_the_issue_text_was_edited_since_the_approval() {
            let (mut rig, run) = interrupted();
            rig.view = gone();
            rig.sim.add_branch(BRANCH);
            rig.sim.set_approval(approval_answer(
                LABELED_AT,
                "tom",
                &[("2026-10-04T10:30:00Z", "tom")],
            ));
            pass_one(&rig, &run);
            assert_eq!(interruptions(&rig, &run)[0]["branch"], Value::Null);
        }

        // A1: a machine given up before the session started.

        #[test]
        fn a_machine_unreachable_for_15_minutes_before_the_session_started_is_given_up() {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            rig.worker.on_create(Ok(fresh()));
            placed_on(&rig, &run, m2());
            rig.view = FleetView::builder()
                .online(m1())
                .online(m3())
                .unreachable(m2(), secs(15 * 60 - 1))
                .build();
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Workspace));
            rig.view = FleetView::builder()
                .online(m1())
                .online(m3())
                .unreachable(m2(), secs(15 * 60))
                .build();
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::NeedsMachine(Placing::Resume { resumes: 1 })
            );
            assert!(
                resume_steps(&rig, &run).contains(&format!("resume-machine-given-up 1 {}", m2()))
            );
            // Nothing ran: the same resume, no second decision.
            assert_eq!(interruptions(&rig, &run).len(), 1);
            assert!(placed(&rig.ctx(), &run, Target::Machine(m2())).is_err());
            rig.worker.on_create(Ok(fresh3()));
            assert_eq!(
                placed_on(&rig, &run, m3()),
                ResumeStep::Waiting(Wait::Workspace)
            );
            let calls = create_calls(&rig);
            assert_eq!(calls.len(), 2);
            assert!(matches!(&calls[1], Call::Create { target, request }
                if *target == Target::Machine(m3()) && request.name == "fac/8-1-r1"));
            // When the given-up machine is heard from again its workspace is an orphan.
            let view = FleetView::builder()
                .online(m1())
                .online(m2())
                .online(m3())
                .workspace(workspace_record(m2(), fresh(), "fac/8-1-r1", None))
                .workspace(workspace_record(m3(), fresh3(), "fac/8-1-r1", None))
                .build();
            assert_eq!(
                orphans(&view, project_id(), &run, &rig.events(&run)),
                vec![fresh()]
            );
        }

        /// A resume placed at a host is refused as the fleet worker refuses a host, naming its
        /// kind, before anything is recorded or asked.
        #[test]
        fn a_resume_placed_at_a_host_is_refused_naming_the_kind_and_records_nothing() {
            let (rig, run) = archived_and_waiting_for_a_machine();
            let before = resume_steps(&rig, &run);
            let host = Target::Host(foundry_proto::factory::HostKind::Vims);
            let Err(RunError::Worker(WorkerError::Failed(said))) = placed(&rig.ctx(), &run, host)
            else {
                panic!("a host is refused");
            };
            assert!(said.contains("vims"), "{said}");
            assert_eq!(resume_steps(&rig, &run), before);
            assert!(create_calls(&rig).is_empty());
        }

        /// A resume whose recorded target is a host is refused the same way, before the
        /// machine's 15 minutes are judged.
        #[test]
        fn a_resume_recorded_at_a_host_is_refused_naming_the_kind() {
            let (rig, run) = archived_and_waiting_for_a_machine();
            rig.step(&run, "resume-machine 1 vims");
            let Err(RunError::Worker(WorkerError::Failed(said))) = advance(&rig.ctx(), &run) else {
                panic!("a host is refused");
            };
            assert!(said.contains("vims"), "{said}");
            assert!(create_calls(&rig).is_empty());
        }

        /// A claim that must move, placed at a host, is refused as the fleet worker refuses a
        /// host, naming its kind, before the move is recorded or anything is asked.
        #[test]
        fn a_moved_claim_placed_at_a_host_is_refused_naming_the_kind_and_records_nothing() {
            let mut rig = Rig::new();
            let run = run_id(1);
            claimed_not_built(&rig, &run);
            rig.view = FleetView::builder()
                .online(m3())
                .unreachable(m1(), secs(15 * 60))
                .build();
            assert!(matches!(
                pass(&rig, &run),
                ResumeStep::NeedsMachine(Placing::MovedClaim { .. })
            ));
            let before = resume_steps(&rig, &run);
            let host = Target::Host(foundry_proto::factory::HostKind::Vims);
            let Err(RunError::Worker(WorkerError::Failed(said))) = placed(&rig.ctx(), &run, host)
            else {
                panic!("a host is refused");
            };
            assert!(said.contains("vims"), "{said}");
            assert_eq!(resume_steps(&rig, &run), before);
            assert!(create_calls(&rig).is_empty());
        }

        #[test]
        fn a_claims_target_unreachable_for_15_minutes_before_its_create_is_given_up() {
            let mut rig = Rig::new();
            let run = run_id(1);
            claimed_not_built(&rig, &run);
            rig.view = FleetView::builder()
                .online(m3())
                .unreachable(m1(), secs(15 * 60))
                .build();
            rig.worker
                .on_create(Err(WorkerError::Unreachable("timed out".into())));
            assert!(placed(&rig.ctx(), &run, Target::Machine(m2())).is_err());
            assert!(resume_steps(&rig, &run).contains(&format!("claim-moved {}", m2())));
            rig.view = FleetView::builder()
                .online(m3())
                .unreachable(m1(), secs(15 * 60))
                .unreachable(m2(), secs(15 * 60))
                .build();
            assert!(matches!(
                pass(&rig, &run),
                ResumeStep::NeedsMachine(Placing::MovedClaim { .. })
            ));
            assert!(resume_steps(&rig, &run).contains(&format!("claim-moved-given-up {}", m2())));
            assert!(placed(&rig.ctx(), &run, Target::Machine(m2())).is_err());
            rig.worker.on_create(Ok(fresh3()));
            assert_eq!(
                placed_on(&rig, &run, m3()),
                ResumeStep::Moved {
                    machine: m3(),
                    workspace: fresh3()
                }
            );
            assert!(interruptions(&rig, &run).is_empty());
            // The target that was given up made a workspace of the name: an orphan when heard.
            let view = FleetView::builder()
                .online(m2())
                .online(m3())
                .workspace(workspace_record(m2(), fresh(), "fac/8-1", None))
                .workspace(workspace_record(m3(), fresh3(), "fac/8-1", None))
                .build();
            assert_eq!(
                orphans(&view, project_id(), &run, &rig.events(&run)),
                vec![fresh()]
            );
        }

        // A2: a closed issue ends a run whose old machine is gone, with that machine never asked.

        #[test]
        fn a_closed_issue_ends_an_owed_resume_whose_machine_is_gone_without_asking_it() {
            let (mut rig, run) = interrupted();
            rig.view = cut_off(secs(15 * 60));
            pass_one(&rig, &run);
            rig.sim.set_closed(true);
            rig.worker
                .on_end_sessions(Err(WorkerError::Unreachable("gone".into())));
            assert_eq!(pass(&rig, &run), ResumeStep::Abandoned);
            assert!(
                rig.worker
                    .calls()
                    .iter()
                    .all(|call| !matches!(call, Call::EndSessions { .. }))
            );
            assert!(rig.finished(&run));
            assert!(create_calls(&rig).is_empty());
        }

        #[test]
        fn an_own_merge_ends_an_owed_resume_whose_machine_is_gone_without_asking_it() {
            let (mut rig, run) = interrupted();
            rig.view = cut_off(secs(15 * 60));
            pass_one(&rig, &run);
            pr_opened(&rig, &run);
            rig.sim.set_pull(
                12,
                SimPull {
                    state: "MERGED",
                    ..pr_on(PR_BRANCH, false)
                },
            );
            rig.sim.set_closed(true);
            rig.worker
                .on_end_sessions(Err(WorkerError::Unreachable("gone".into())));
            assert_eq!(pass(&rig, &run), ResumeStep::Ended);
            assert!(
                rig.worker
                    .calls()
                    .iter()
                    .all(|call| !matches!(call, Call::EndSessions { .. }))
            );
            assert!(rig.finished(&run));
        }

        // testing-6: a closing after the first pass stops the resume.

        #[test]
        fn a_closed_issue_after_the_first_pass_makes_no_workspace_and_starts_no_session() {
            let (mut rig, run) = interrupted();
            pass_one(&rig, &run);
            rig.sim.set_closed(true);
            rig.view = archived();
            assert_eq!(pass(&rig, &run), ResumeStep::Abandoned);
            assert!(create_calls(&rig).is_empty());
            assert!(start_calls(&rig).is_empty());
        }

        #[test]
        fn a_closed_issue_stops_a_resume_whose_machine_is_placed() {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            rig.sim.set_closed(true);
            assert_eq!(placed_on(&rig, &run, m2()), ResumeStep::Abandoned);
            assert!(create_calls(&rig).is_empty());
            rig.view = archived();
        }

        #[test]
        fn an_own_merge_after_the_first_pass_ends_the_run() {
            let (mut rig, run) = interrupted();
            pass_one(&rig, &run);
            pr_opened(&rig, &run);
            rig.sim.set_pull(
                12,
                SimPull {
                    state: "MERGED",
                    ..pr_on(PR_BRANCH, false)
                },
            );
            rig.sim.set_closed(true);
            rig.view = archived();
            assert_eq!(pass(&rig, &run), ResumeStep::Ended);
            assert!(create_calls(&rig).is_empty());
        }

        #[test]
        fn a_closed_issue_stops_a_moved_claim() {
            let mut rig = Rig::new();
            let run = run_id(1);
            claimed_not_built(&rig, &run);
            rig.view = FleetView::builder()
                .online(m2())
                .unreachable(m1(), secs(15 * 60))
                .build();
            rig.sim.set_closed(true);
            assert_eq!(placed_on(&rig, &run, m2()), ResumeStep::Abandoned);
            assert!(create_calls(&rig).is_empty());
            assert!(
                !resume_steps(&rig, &run)
                    .iter()
                    .any(|step| step.starts_with("claim-moved"))
            );
        }

        // A4: a session the view does not show yet.

        #[test]
        fn the_pass_right_after_a_start_with_the_session_not_shown_yet_writes_and_asks_nothing() {
            let (mut rig, run) = at_the_start();
            resumed(&mut rig, &run);
            rig.view = with_resume_workspace("fac/8-1-r1", None).build();
            let before = event_count(&rig, &run);
            let calls = rig.worker.calls().len();
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
            assert_eq!(rig.worker.calls().len(), calls);
            assert_eq!(interruptions(&rig, &run).len(), 1);
        }

        #[test]
        fn a_session_not_shown_on_a_machine_gone_for_15_minutes_is_an_interruption() {
            let (mut rig, run) = at_the_start();
            resumed(&mut rig, &run);
            rig.view = FleetView::builder()
                .online(m1())
                .unreachable(m2(), secs(15 * 60 - 1))
                .workspace(workspace_record(m2(), fresh(), "fac/8-1-r1", None))
                .build();
            assert!(is_skipped(&pass(&rig, &run)));
            rig.view = FleetView::builder()
                .online(m1())
                .unreachable(m2(), secs(15 * 60))
                .workspace(workspace_record(m2(), fresh(), "fac/8-1-r1", None))
                .build();
            assert!(matches!(
                pass(&rig, &run),
                ResumeStep::Archiving { resumes: 2, .. }
            ));
        }

        // testing-7: the session's own machine, not the claim's, is measured.

        #[test]
        fn a_resumed_session_on_a_machine_gone_for_15_minutes_is_an_interruption() {
            let (mut rig, run) = at_the_start();
            resumed(&mut rig, &run);
            rig.view = FleetView::builder()
                .online(m1())
                .unreachable(m2(), secs(15 * 60))
                .workspace(workspace_record(m2(), fresh(), "fac/8-1-r1", None))
                .session(session_owned_by(
                    m2(),
                    resumed_session(),
                    fresh(),
                    SessionState::Working,
                ))
                .build();
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Archiving {
                    resumes: 2,
                    workspace: Some(fresh())
                }
            );
        }

        // testing-2: the record comes before the act.

        #[test]
        fn a_start_whose_answer_is_lost_is_recorded_and_ended_before_the_second_start() {
            let (rig, run) = at_the_start();
            rig.worker
                .on_start_session(Err(WorkerError::Unreachable("timed out".into())));
            assert!(advance(&rig.ctx(), &run).is_err());
            assert!(resume_steps(&rig, &run).contains(&"resume-sending 1".to_string()));
            assert_eq!(start_calls(&rig).len(), 1);
            rig.worker.on_start_session(Ok(resumed_session()));
            assert!(matches!(pass(&rig, &run), ResumeStep::Started { .. }));
            let calls = rig.worker.calls();
            let ended = calls
                .iter()
                .rposition(
                    |c| matches!(c, Call::EndSessions { workspace } if *workspace == fresh()),
                )
                .unwrap();
            let second = calls
                .iter()
                .rposition(|c| matches!(c, Call::StartSession { .. }))
                .unwrap();
            assert!(ended < second);
        }

        #[test]
        fn a_moved_claims_machine_is_recorded_before_its_create_is_asked() {
            let mut rig = Rig::new();
            let run = run_id(1);
            claimed_not_built(&rig, &run);
            rig.view = FleetView::builder()
                .online(m2())
                .unreachable(m1(), secs(15 * 60))
                .build();
            rig.worker
                .on_create(Err(WorkerError::Unreachable("timed out".into())));
            assert!(placed(&rig.ctx(), &run, Target::Machine(m2())).is_err());
            assert_eq!(
                resume_steps(&rig, &run),
                vec![format!("claim-moved {}", m2())]
            );
            // A later pass asks the machine it recorded, and places nothing new.
            rig.worker.on_create(Ok(fresh()));
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Moved {
                    machine: m2(),
                    workspace: fresh()
                }
            );
            let calls = create_calls(&rig);
            assert_eq!(calls.len(), 2);
            assert!(
                calls
                    .iter()
                    .all(|call| matches!(call, Call::Create { target, .. } if *target == Target::Machine(m2())))
            );
            assert_eq!(
                resume_steps(&rig, &run),
                vec![format!("claim-moved {}", m2())]
            );
        }

        // testing-3: a claim start decided to resume from a branch.

        #[test]
        fn a_moved_claim_keeps_the_branch_and_the_base_start_decided() {
            let mut rig = Rig::new();
            let run = run_id(1);
            claimed_not_built(&rig, &run);
            rig.step(&run, "launch-resuming feat/x");
            rig.view = FleetView::builder()
                .online(m2())
                .unreachable(m1(), secs(15 * 60))
                .build();
            rig.worker.on_create(Ok(fresh()));
            placed_on(&rig, &run, m2());
            let calls = create_calls(&rig);
            let Call::Create { request, .. } = &calls[0] else {
                panic!("a create");
            };
            assert_eq!(
                request.checkout,
                Checkout::Branch {
                    name: "feat/x".into()
                }
            );

            let mut rig = Rig::new();
            let run = run_id(1);
            claimed_not_built(&rig, &run);
            rig.step(&run, "launch-base feature/x");
            rig.view = FleetView::builder()
                .online(m2())
                .unreachable(m1(), secs(15 * 60))
                .build();
            rig.worker.on_create(Ok(fresh()));
            placed_on(&rig, &run, m2());
            let calls = create_calls(&rig);
            let Call::Create { request, .. } = &calls[0] else {
                panic!("a create");
            };
            assert_eq!(
                request.checkout,
                Checkout::NewBranch {
                    base: "feature/x".into(),
                    branch: Some("fac/8-1".into())
                }
            );
        }

        // testing-4: the setup that succeeded.

        #[test]
        fn a_setup_that_exited_zero_goes_on_to_the_session() {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            rig.worker.on_create(Ok(fresh()));
            placed_on(&rig, &run, m2());
            rig.view = with_resume_workspace("fac/8-1-r1", Some(setup_terminal()))
                .terminal(terminal_record(TerminalState::Exited, Some(0)))
                .build();
            assert!(matches!(
                resumed(&mut rig, &run),
                ResumeStep::Started { resumes: 1, .. }
            ));
            assert_eq!(start_calls(&rig).len(), 1);
        }

        // testing-5: orphans never list a workspace that is being made.

        #[test]
        fn orphans_keep_a_resume_workspace_during_its_setup() {
            let (rig, run) = at_the_start();
            let view = with_resume_workspace("fac/8-1-r1", Some(setup_terminal()))
                .workspace(workspace_record(m1(), old(), BRANCH, None))
                .build();
            // The session is not launched: the earlier workspace is still the run's active one
            // and the resume workspace is being set up.
            assert_eq!(
                orphans(&view, project_id(), &run, &rig.events(&run)),
                vec![]
            );
        }

        #[test]
        fn orphans_keep_a_build_workspace_whose_answer_is_not_recorded_yet() {
            let rig = Rig::new();
            let run = run_id(1);
            claimed_not_built(&rig, &run);
            let view = FleetView::builder()
                .online(m1())
                .workspace(workspace_record(m1(), fresh(), BRANCH, None))
                .build();
            assert_eq!(
                orphans(&view, project_id(), &run, &rig.events(&run)),
                vec![]
            );
        }

        #[test]
        fn orphans_see_a_resume_workspace_past_the_64th_name() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.write(
                &run,
                Event::SessionInterrupted {
                    resumes: 66,
                    relaunch: false,
                    run_ordinal: None,
                    branch: None,
                    t3_error: None,
                    ended_in_pass: 0,
                },
            );
            rig.write(
                &run,
                Event::SessionLaunched {
                    thread_id: resumed_session().to_string(),
                    worktree: Some(fresh().to_string()),
                    branch: Some("fac/8-1-r66".into()),
                    model_instance: None,
                    model: None,
                    model_options: None,
                    resumed_from: None,
                    base: None,
                    title: None,
                },
            );
            let view = FleetView::builder()
                .online(m2())
                .workspace(workspace_record(m2(), fresh(), "fac/8-1-r66", None))
                .workspace(workspace_record(m2(), old(), "fac/8-1-r65", None))
                .build();
            assert_eq!(
                orphans(&view, project_id(), &run, &rig.events(&run)),
                vec![old()]
            );
        }

        /// The names looked for run one past the highest resume the ledger names: a workspace
        /// under the next resume's name is the run's too. A cut ledger: the factory writes a
        /// resume's `session_interrupted` line before it asks for that resume's workspace.
        #[test]
        fn orphans_see_a_resume_workspace_one_past_the_highest_resume_named() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.write(
                &run,
                Event::SessionInterrupted {
                    resumes: 1,
                    relaunch: false,
                    run_ordinal: None,
                    branch: None,
                    t3_error: None,
                    ended_in_pass: 0,
                },
            );
            rig.write(
                &run,
                Event::SessionLaunched {
                    thread_id: resumed_session().to_string(),
                    worktree: Some(fresh().to_string()),
                    branch: Some("fac/8-1-r1".into()),
                    model_instance: None,
                    model: None,
                    model_options: None,
                    resumed_from: None,
                    base: None,
                    title: None,
                },
            );
            let view = FleetView::builder()
                .online(m2())
                .workspace(workspace_record(m2(), fresh(), "fac/8-1-r1", None))
                .workspace(workspace_record(m2(), old(), "fac/8-1-r2", None))
                .build();
            assert_eq!(
                orphans(&view, project_id(), &run, &rig.events(&run)),
                vec![old()]
            );
        }

        // testing-11: what is adopted must be the run's.

        #[test]
        fn a_record_on_another_machine_is_not_taken_for_the_creates_answer() {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            rig.step(&run, &format!("resume-machine 1 {}", m2()));
            rig.step(&run, "resume-requested 1 fac/8-1-r1");
            rig.view = FleetView::builder()
                .online(m1())
                .online(m2())
                .workspace(workspace_record(
                    m1(),
                    fresh3(),
                    "fac/8-1-r1",
                    Some(setup_terminal()),
                ))
                .build();
            rig.worker.on_create(Ok(fresh()));
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Workspace));
            assert_eq!(create_calls(&rig).len(), 1);
            assert!(resume_steps(&rig, &run).contains(&format!("resume-workspace 1 {}", fresh())));
        }

        #[test]
        fn a_session_with_no_turn_record_in_the_workspace_is_not_adopted() {
            let (mut rig, run) = at_the_start();
            rig.view = with_resume_workspace("fac/8-1-r1", None)
                .session(session_record(
                    resumed_session(),
                    fresh(),
                    SessionState::Working,
                ))
                .build();
            rig.worker
                .on_start_session(Ok(SessionId(uuid::Uuid::from_u128(0x5e8))));
            assert!(matches!(pass(&rig, &run), ResumeStep::Started { .. }));
            assert_eq!(start_calls(&rig).len(), 1);
        }

        // the `placed` nit: nothing that needs no machine is acted on.

        #[test]
        fn placed_when_nothing_needs_a_machine_is_skipped_and_writes_nothing() {
            let (rig, run) = interrupted();
            let before = event_count(&rig, &run);
            assert!(is_skipped(&placed_on(&rig, &run, m2())));
            assert_eq!(event_count(&rig, &run), before);
            assert!(rig.worker.calls().is_empty());
            pass_one(&rig, &run);
            let before = event_count(&rig, &run);
            assert!(is_skipped(&placed_on(&rig, &run, m2())));
            assert_eq!(event_count(&rig, &run), before);
            assert_eq!(archive_calls(&rig).len(), 1);
        }

        /// An interruption with a message that no result line ends is an interruption.
        #[test]
        fn a_finished_session_whose_newest_turn_has_no_result_line_is_an_interruption() {
            let (mut rig, run) = failed_with_turns_unasked();
            rig.view = fleet(Some(SessionState::Finished))
                .turn(turn_of(session(), old(), 1, 1))
                .build();
            say(&rig, "I am still working on it.");
            pass_one(&rig, &run);
        }

        /// A gone session with no approval recorded is stuck as unconfirmed, and the ledger's
        /// detail names the session's end.
        #[test]
        fn a_gone_session_with_no_recorded_approval_is_stuck_unconfirmed_naming_its_end() {
            let mut rig = Rig::new();
            let run = run_id(1);
            rig.write(
                &run,
                Event::Claimed {
                    machine: m1().to_string(),
                    github_user: LOGIN.into(),
                },
            );
            rig.moved(&run, State::Ready, State::Triage);
            rig.step(&run, "approval-rechecked");
            rig.write(&run, launch(Some(BRANCH), Some("main")));
            rig.moved(&run, State::Triage, State::Building);
            rig.sim.set_labels(&["factory:building"]);
            rig.sim.set_assignees(&[LOGIN]);
            rig.view = gone();
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Stuck {
                    reason: messages::CANNOT_CONFIRM.into()
                }
            );
            assert!(
                rig.steps(&run)
                    .iter()
                    .any(|step| step.starts_with("stuck-detail") && step.contains(SESSION_GONE)),
                "{:?}",
                rig.steps(&run)
            );
            assert!(interruptions(&rig, &run).is_empty());
        }
    }

    /// Cases of the second review cycle.
    mod review_cycle_2 {
        use serde_json::json;

        use super::foundry::claimed_not_built;
        use super::*;

        fn m3() -> MachineId {
            MachineId(uuid::Uuid::from_u128(0x4))
        }

        fn fresh3() -> WorkspaceId {
            WorkspaceId(uuid::Uuid::from_u128(0xb33))
        }

        fn at_the_start() -> (Rig, RunId) {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            resume_workspace_made(&mut rig, &run);
            (rig, run)
        }

        /// The approval answer of an issue that is `building`: the instructor's label, then the
        /// factory's own removal of `factory:ready` when it claimed the issue.
        fn a_building_issues_timeline() -> String {
            let mut answer: Value =
                serde_json::from_str(&approval_answer(LABELED_AT, "tom", &[])).unwrap();
            let issue = &mut answer["data"]["repository"]["issue"];
            issue["timelineItems"]["nodes"]
                .as_array_mut()
                .unwrap()
                .push(json!({
                    "__typename": "UnlabeledEvent",
                    "createdAt": "2026-10-04T10:00:05Z",
                    "actor": {"__typename": "User", "login": LOGIN},
                    "label": {"name": "factory:ready"},
                }));
            issue["timelineItems"]["totalCount"] = json!(2);
            answer.to_string()
        }

        /// A full page of edits by instructors since the approval: the text cannot be judged.
        fn a_history_that_cannot_be_judged() -> String {
            let times: Vec<String> = (0..100)
                .map(|i| format!("2026-10-04T10:{:02}:{:02}Z", 1 + i / 60, i % 60))
                .collect();
            let edits: Vec<(&str, &str)> = times.iter().map(|t| (t.as_str(), "tom")).collect();
            approval_answer(LABELED_AT, "tom", &edits)
        }

        fn a_stranger_edited_the_text() -> String {
            approval_answer(
                "2026-10-04T12:00:00Z",
                "tom",
                &[("2026-10-04T10:30:00Z", "mallory")],
            )
        }

        // adversarial-3: a building issue has no `factory:ready` in force, so the approval is
        // not read again; the text is.

        #[test]
        fn a_building_issues_timeline_does_not_stop_the_resume() {
            let (rig, run) = at_the_start();
            rig.sim.set_approval(a_building_issues_timeline());
            assert!(matches!(
                resumed_in(&rig, &run),
                ResumeStep::Started { resumes: 1, .. }
            ));
            assert_eq!(start_calls(&rig).len(), 1);
        }

        fn resumed_in(rig: &Rig, run: &RunId) -> ResumeStep {
            rig.worker.on_start_session(Ok(resumed_session()));
            pass(rig, run)
        }

        // testing-12.

        #[test]
        fn a_history_that_cannot_be_judged_is_stuck_and_starts_no_session() {
            let (rig, run) = at_the_start();
            rig.sim.set_approval(a_history_that_cannot_be_judged());
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Stuck {
                    reason: messages::CANNOT_VERIFY_DURING_BUILD.into()
                }
            );
            assert!(start_calls(&rig).is_empty());
            assert!(!resume_steps(&rig, &run).contains(&"resume-sending 1".to_string()));
            assert!(rig.finished(&run));
        }

        /// Every session ended through the worker was of the resume workspace, never the old,
        /// archived one.
        fn ended_only_the_resume_workspace(rig: &Rig) {
            let ended: Vec<WorkspaceId> = rig
                .worker
                .calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::EndSessions { workspace } => Some(workspace),
                    _ => None,
                })
                .collect();
            // The first try's call and the finishing pass's, both of the resume workspace.
            assert_eq!(ended.len(), 2, "{ended:?}");
            assert!(
                ended.iter().all(|workspace| *workspace == fresh()),
                "{ended:?}"
            );
        }

        /// The stuck event's reason, which keeps the detail the ledger holds.
        fn stuck_reason(rig: &Rig, run: &RunId) -> String {
            rig.events(run)
                .into_iter()
                .find(|e| e.event == "stuck")
                .and_then(|e| e.data["reason"].as_str().map(str::to_owned))
                .unwrap()
        }

        // blind-reader-6: a stuck path resume began is finished by resume, from the ledger.

        /// Runs `pass` with the stuck comment failing once: the path is begun and cut short,
        /// then the next pass finishes it.
        fn cut_short_then_finished(rig: &Rig, run: &RunId, reason: &str) {
            rig.sim.fail("could not finish this issue", 1);
            assert!(advance(&rig.ctx(), run).is_err());
            assert!(
                rig.steps(run)
                    .iter()
                    .any(|step| step.starts_with("stuck-stop")),
                "{:?}",
                rig.steps(run)
            );
            assert!(!rig.finished(run));
            assert_eq!(
                pass(rig, run),
                ResumeStep::Stuck {
                    reason: reason.into()
                }
            );
            assert!(rig.finished(run));
            assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
        }

        #[test]
        fn a_stuck_path_cut_short_after_a_setup_that_failed_is_finished() {
            let (mut rig, run) = at_the_start();
            rig.view = with_resume_workspace("fac/8-1-r1", Some(setup_terminal()))
                .terminal(terminal_record(TerminalState::Exited, Some(1)))
                .build();
            rig.worker.on_tail(Ok("npm ERR! missing script".into()));
            rig.worker.on_tail(Ok("npm ERR! missing script".into()));
            cut_short_then_finished(&rig, &run, crate::factory::runs::start::SETUP_FAILED);
            assert!(the_stuck_comment(&rig).contains("npm ERR! missing script"));
            ended_only_the_resume_workspace(&rig);
        }

        #[test]
        fn a_stuck_path_cut_short_after_a_text_that_changed_is_finished() {
            let (rig, run) = at_the_start();
            rig.sim.set_approval(a_stranger_edited_the_text());
            cut_short_then_finished(&rig, &run, messages::CHANGED_DURING_BUILD);
            ended_only_the_resume_workspace(&rig);
        }

        #[test]
        fn a_stuck_path_cut_short_after_a_text_that_could_not_be_judged_is_finished() {
            let (rig, run) = at_the_start();
            rig.sim.set_approval(a_history_that_cannot_be_judged());
            cut_short_then_finished(&rig, &run, messages::CANNOT_VERIFY_DURING_BUILD);
            ended_only_the_resume_workspace(&rig);
        }

        #[test]
        fn a_stuck_path_cut_short_after_a_start_the_worker_refused_is_finished() {
            let (rig, run) = at_the_start();
            rig.worker
                .on_start_session(Err(WorkerError::Failed("no model".into())));
            cut_short_then_finished(&rig, &run, LAUNCH_FAILED);
            ended_only_the_resume_workspace(&rig);
            // The worker's own words are kept for the ledger.
            assert!(stuck_reason(&rig, &run).contains("no model"));
        }

        #[test]
        fn a_stuck_path_cut_short_at_the_cap_is_finished() {
            let (mut rig, run) = failed_with_turns_unasked();
            rig.config.limits.interruption_resumes = 0;
            // The session's last words, which the stuck comment quotes, are read again.
            rig.view = fleet(Some(SessionState::Failed))
                .turn(turn_of(session(), old(), 1, 1))
                .build();
            say(&rig, "it crashed at the linker");
            say(&rig, "it crashed at the linker");
            cut_short_then_finished(&rig, &run, "the session run ended as failed");
            assert!(the_stuck_comment(&rig).contains("it crashed at the linker"));
        }

        #[test]
        fn a_stuck_path_cut_short_for_a_branch_that_was_refused_is_finished() {
            let (rig, run) = interrupted();
            pr_opened(&rig, &run);
            rig.write(
                &run,
                Event::GateSentBack {
                    attempt: 1,
                    run_ordinal: 1,
                },
            );
            rig.sim.set_pull(12, pr_on("-x", false));
            cut_short_then_finished(&rig, &run, LAUNCH_FAILED);
            assert!(stuck_reason(&rig, &run).contains(BRANCH_REFUSED));
        }

        // blind-reader-7: a decision made because the machine was gone never asks it.

        #[test]
        fn a_stuck_path_cut_short_for_a_machine_that_was_gone_is_finished_without_asking_it() {
            let (mut rig, run) = interrupted();
            rig.config.limits.interruption_resumes = 0;
            rig.view = cut_off(secs(15 * 60));
            rig.worker
                .on_end_sessions(Err(WorkerError::Unreachable("gone".into())));
            cut_short_then_finished(&rig, &run, MACHINE_UNREACHABLE);
            assert!(
                rig.worker
                    .calls()
                    .iter()
                    .all(|call| !matches!(call, Call::EndSessions { .. }))
            );
        }

        // blind-reader-8: a machine given up is accepted once it is heard from again.

        #[test]
        fn a_machine_given_up_is_accepted_again_once_it_is_online() {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            rig.worker.on_create(Ok(fresh()));
            placed_on(&rig, &run, m2());
            rig.view = FleetView::builder()
                .online(m1())
                .unreachable(m2(), secs(15 * 60))
                .build();
            assert!(matches!(pass(&rig, &run), ResumeStep::NeedsMachine(_)));
            assert!(placed(&rig.ctx(), &run, Target::Machine(m2())).is_err());
            // Heard from again: placement may pick it, and its earlier workspace of the name is
            // found and used.
            rig.view = with_resume_workspace("fac/8-1-r1", Some(setup_terminal()))
                .terminal(terminal_record(TerminalState::Running, None))
                .build();
            rig.worker.on_create(Ok(fresh()));
            assert_eq!(
                placed_on(&rig, &run, m2()),
                ResumeStep::Waiting(Wait::Workspace)
            );
            let machine_steps = resume_steps(&rig, &run)
                .into_iter()
                .filter(|step| *step == format!("resume-machine 1 {}", m2()))
                .count();
            assert_eq!(machine_steps, 2);
            // Its earlier workspace of the name was found: no second create.
            assert_eq!(create_calls(&rig).len(), 1);
        }

        #[test]
        fn a_claim_whose_machine_is_back_is_left_to_start_and_a_given_up_target_is_accepted_when_online()
         {
            let mut rig = Rig::new();
            let run = run_id(1);
            claimed_not_built(&rig, &run);
            rig.view = FleetView::builder()
                .online(m3())
                .unreachable(m1(), secs(15 * 60))
                .build();
            rig.worker
                .on_create(Err(WorkerError::Unreachable("timed out".into())));
            assert!(placed(&rig.ctx(), &run, Target::Machine(m2())).is_err());
            rig.view = FleetView::builder()
                .online(m3())
                .unreachable(m1(), secs(15 * 60))
                .unreachable(m2(), secs(15 * 60))
                .build();
            assert!(matches!(
                pass(&rig, &run),
                ResumeStep::NeedsMachine(Placing::MovedClaim { .. })
            ));
            assert!(placed(&rig.ctx(), &run, Target::Machine(m2())).is_err());
            // The target is back: placement may pick it again.
            rig.view = FleetView::builder()
                .online(m2())
                .online(m3())
                .unreachable(m1(), secs(15 * 60))
                .build();
            rig.worker.on_create(Ok(fresh()));
            assert_eq!(
                placed_on(&rig, &run, m2()),
                ResumeStep::Moved {
                    machine: m2(),
                    workspace: fresh()
                }
            );
        }

        #[test]
        fn a_given_up_claim_whose_own_machine_is_back_is_left_to_start() {
            let mut rig = Rig::new();
            let run = run_id(1);
            claimed_not_built(&rig, &run);
            rig.view = FleetView::builder()
                .online(m3())
                .unreachable(m1(), secs(15 * 60))
                .build();
            rig.worker
                .on_create(Err(WorkerError::Unreachable("timed out".into())));
            assert!(placed(&rig.ctx(), &run, Target::Machine(m2())).is_err());
            rig.view = FleetView::builder()
                .online(m3())
                .unreachable(m1(), secs(15 * 60))
                .unreachable(m2(), secs(15 * 60))
                .build();
            assert!(matches!(pass(&rig, &run), ResumeStep::NeedsMachine(_)));
            // The claim's own machine is heard from again: `start` carries on there.
            rig.view = FleetView::builder()
                .online(m1())
                .online(m3())
                .unreachable(m2(), secs(15 * 60))
                .build();
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
        }

        // C1, testing-14: a decision made on a gone machine never asks it, whatever its code.

        #[test]
        fn a_cut_short_path_of_another_code_on_a_gone_machine_is_finished_without_asking_it() {
            let mut rig = Rig::new();
            let run = run_id(1);
            // No approval is recorded, so the decision is `resume-approval-unconfirmed`.
            rig.write(
                &run,
                Event::Claimed {
                    machine: m1().to_string(),
                    github_user: LOGIN.into(),
                },
            );
            rig.moved(&run, State::Ready, State::Triage);
            rig.step(&run, "approval-rechecked");
            rig.write(&run, launch(Some(BRANCH), Some("main")));
            rig.moved(&run, State::Triage, State::Building);
            rig.sim.set_labels(&["factory:building"]);
            rig.sim.set_assignees(&[LOGIN]);
            rig.view = cut_off(secs(15 * 60));
            rig.worker
                .on_end_sessions(Err(WorkerError::Unreachable("gone".into())));
            cut_short_then_finished(&rig, &run, messages::CANNOT_CONFIRM);
            assert!(
                rig.worker
                    .calls()
                    .iter()
                    .all(|call| !matches!(call, Call::EndSessions { .. }))
            );
        }

        // C2: every cut point of a stuck path while a resume is owed.

        #[test]
        fn a_lone_stuck_detail_is_no_decision_and_the_resume_goes_on() {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            rig.step(&run, "stuck-detail no model");
            rig.worker.on_create(Ok(fresh()));
            assert_eq!(
                placed_on(&rig, &run, m2()),
                ResumeStep::Waiting(Wait::Workspace)
            );
            rig.view = with_resume_workspace("fac/8-1-r1", None).build();
            assert!(matches!(
                resumed(&mut rig, &run),
                ResumeStep::Started { .. }
            ));
            // Launched, a later interruption is read: the lone detail does not skip the run.
            rig.view = with_resume_workspace("fac/8-1-r1", None)
                .session(session_record(
                    resumed_session(),
                    fresh(),
                    SessionState::Failed,
                ))
                .build();
            never_written(&rig);
            assert!(matches!(
                pass(&rig, &run),
                ResumeStep::Archiving { resumes: 2, .. }
            ));
        }

        #[test]
        fn a_lone_stuck_detail_with_no_resume_owed_is_decided_again() {
            let (rig, run) = interrupted();
            rig.step(&run, "stuck-detail no model");
            pass_one(&rig, &run);
        }

        #[test]
        fn a_stuck_path_cut_short_after_its_state_change_is_finished_while_a_resume_is_owed() {
            let (rig, run) = archived_and_waiting_for_a_machine();
            rig.step(
                &run,
                "stuck-stop {\"code\":\"resume-setup-failed\",\"stop\":{\"reason\":\"the build workspace's setup failed\"}}",
            );
            rig.write(
                &run,
                Event::Stuck {
                    reason: "the build workspace's setup failed".into(),
                    branch: None,
                    commits_ahead: None,
                },
            );
            rig.moved(&run, State::Building, State::Stuck);
            assert!(!rig.finished(&run));
            assert!(matches!(pass(&rig, &run), ResumeStep::Stuck { .. }));
            assert!(rig.finished(&run));
            assert!(create_calls(&rig).is_empty());
        }

        #[test]
        fn a_stuck_path_cut_short_after_its_state_change_is_finished_when_none_is_owed() {
            let (rig, run) = interrupted();
            rig.step(
                &run,
                "stuck-stop {\"code\":\"session-ended-badly\",\"stop\":{\"reason\":\"the session run ended as failed\"}}",
            );
            rig.write(
                &run,
                Event::Stuck {
                    reason: "the session run ended as failed".into(),
                    branch: None,
                    commits_ahead: None,
                },
            );
            rig.moved(&run, State::Building, State::Stuck);
            assert!(matches!(pass(&rig, &run), ResumeStep::Stuck { .. }));
            assert!(rig.finished(&run));
        }

        // testing-17: a decision of another step is finished by resume only while a resume is
        // owed, since building skips such a run.

        #[test]
        fn a_foreign_stuck_decision_is_finished_while_a_resume_is_owed() {
            let (rig, run) = archived_and_waiting_for_a_machine();
            rig.step(
                &run,
                "stuck-stop {\"code\":\"time-limit\",\"stop\":{\"reason\":\"the run ran past limits.minutes\"}}",
            );
            assert!(matches!(pass(&rig, &run), ResumeStep::Stuck { .. }));
            assert!(rig.finished(&run));
            assert!(create_calls(&rig).is_empty());
            assert!(start_calls(&rig).is_empty());
        }

        // C4: start's begun paths in triage are start's.

        #[test]
        fn a_stuck_path_start_began_in_triage_is_left_to_start() {
            for code in ["approval-unconfirmed", "launch-failed", "setup-failed"] {
                let mut rig = Rig::new();
                let run = run_id(1);
                claimed_not_built(&rig, &run);
                rig.view = FleetView::builder().online(m1()).build();
                rig.step(
                    &run,
                    &format!("stuck-stop {{\"code\":\"{code}\",\"stop\":{{\"reason\":\"x\"}}}}"),
                );
                let before = event_count(&rig, &run);
                assert!(is_skipped(&pass(&rig, &run)), "{code}");
                assert_eq!(event_count(&rig, &run), before, "{code}");
            }
            // A build that already launched, stuck by start's code with no resume owed.
            let (rig, run) = interrupted();
            rig.step(
                &run,
                "stuck-stop {\"code\":\"launch-failed\",\"stop\":{\"reason\":\"x\"}}",
            );
            assert!(is_skipped(&pass(&rig, &run)));
        }

        // correctness-10: the claim's own machine is moved again only under the 15-minute rule.

        #[test]
        fn a_claim_whose_machine_blips_after_a_target_was_given_up_is_not_moved() {
            let mut rig = Rig::new();
            let run = run_id(1);
            claimed_not_built(&rig, &run);
            rig.view = FleetView::builder()
                .online(m3())
                .unreachable(m1(), secs(15 * 60))
                .build();
            rig.worker
                .on_create(Err(WorkerError::Unreachable("timed out".into())));
            assert!(placed(&rig.ctx(), &run, Target::Machine(m2())).is_err());
            rig.view = FleetView::builder()
                .online(m3())
                .unreachable(m1(), secs(15 * 60))
                .unreachable(m2(), secs(15 * 60))
                .build();
            assert!(matches!(pass(&rig, &run), ResumeStep::NeedsMachine(_)));
            // m1 is back, then away for half a minute with a workspace of the name showing.
            rig.view = FleetView::builder()
                .online(m3())
                .unreachable(m1(), secs(30))
                .workspace(workspace_record(m1(), fresh3(), "fac/8-1", None))
                .build();
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            // Away for less than 15 minutes with no workspace showing: not moved either.
            rig.view = FleetView::builder()
                .online(m3())
                .unreachable(m1(), secs(30))
                .build();
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
        }

        // blind-reader-11: a `stuck` line with no decision, while a resume is owed.

        #[test]
        fn a_stuck_line_with_no_decision_is_finished_while_a_resume_is_owed() {
            let (rig, run) = archived_and_waiting_for_a_machine();
            rig.write(
                &run,
                Event::Stuck {
                    reason: "said stuck".into(),
                    branch: None,
                    commits_ahead: None,
                },
            );
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Stuck {
                    reason: "said stuck".into()
                }
            );
            assert!(rig.finished(&run));
            assert!(create_calls(&rig).is_empty());
            assert!(start_calls(&rig).is_empty());
        }

        #[test]
        fn a_run_in_state_stuck_never_reaches_a_workspace_or_a_start() {
            let (rig, run) = archived_and_waiting_for_a_machine();
            rig.write(
                &run,
                Event::Stuck {
                    reason: "said stuck".into(),
                    branch: None,
                    commits_ahead: None,
                },
            );
            rig.moved(&run, State::Building, State::Stuck);
            assert!(matches!(pass(&rig, &run), ResumeStep::Stuck { .. }));
            assert!(create_calls(&rig).is_empty());
            assert!(start_calls(&rig).is_empty());
        }

        // reliability-9: the detail of a decision is the one written for it.

        #[test]
        fn a_stale_detail_of_an_earlier_decision_is_not_the_details_of_a_later_one() {
            let (mut rig, run) = interrupted();
            rig.config.limits.interruption_resumes = 0;
            rig.step(&run, "stuck-detail stale words of an earlier decision");
            cut_short_then_finished(&rig, &run, "the session run ended as failed");
            assert!(!stuck_reason(&rig, &run).contains("stale"));
        }

        // testing-19: the resume workspace of a machine gone for 15 minutes is not asked.

        #[test]
        fn a_cut_short_path_whose_resume_workspace_machine_is_gone_does_not_ask_it() {
            let (mut rig, run) = at_the_start();
            rig.view = with_resume_workspace("fac/8-1-r1", Some(setup_terminal()))
                .terminal(terminal_record(TerminalState::Exited, Some(1)))
                .build();
            rig.sim.fail("could not finish this issue", 1);
            assert!(advance(&rig.ctx(), &run).is_err());
            let ended = |rig: &Rig| {
                rig.worker
                    .calls()
                    .iter()
                    .filter(|call| matches!(call, Call::EndSessions { .. }))
                    .count()
            };
            let asked = ended(&rig);
            rig.view = FleetView::builder()
                .online(m1())
                .unreachable(m2(), secs(15 * 60))
                .workspace(workspace_record(
                    m2(),
                    fresh(),
                    "fac/8-1-r1",
                    Some(setup_terminal()),
                ))
                .terminal(terminal_record(TerminalState::Exited, Some(1)))
                .build();
            rig.worker
                .on_end_sessions(Err(WorkerError::Unreachable("gone".into())));
            assert!(matches!(pass(&rig, &run), ResumeStep::Stuck { .. }));
            assert_eq!(ended(&rig), asked);
            assert!(rig.finished(&run));
        }

        // testing-20: a workspace a given-up target made is not the claim's.

        #[test]
        fn a_workspace_a_given_up_target_made_does_not_keep_the_claim_from_moving() {
            let mut rig = Rig::new();
            let run = run_id(1);
            claimed_not_built(&rig, &run);
            rig.view = FleetView::builder()
                .online(m3())
                .unreachable(m1(), secs(15 * 60))
                .build();
            rig.worker
                .on_create(Err(WorkerError::Unreachable("timed out".into())));
            assert!(placed(&rig.ctx(), &run, Target::Machine(m2())).is_err());
            rig.view = FleetView::builder()
                .online(m3())
                .unreachable(m1(), secs(15 * 60))
                .unreachable(m2(), secs(15 * 60))
                .build();
            assert!(matches!(pass(&rig, &run), ResumeStep::NeedsMachine(_)));
            // The target made the workspace before it went away.
            rig.view = FleetView::builder()
                .online(m3())
                .unreachable(m1(), secs(15 * 60))
                .unreachable(m2(), secs(15 * 60))
                .workspace(workspace_record(m2(), fresh(), "fac/8-1", None))
                .build();
            assert!(matches!(
                pass(&rig, &run),
                ResumeStep::NeedsMachine(Placing::MovedClaim { .. })
            ));
        }

        // maintainability-4, reliability-6: the lookups follow the ledger, not the config.

        #[test]
        fn a_huge_configured_cap_does_not_make_the_pass_probe_that_many_names() {
            let mut rig = Rig::new();
            rig.config.limits.interruption_resumes = 20_000_000;
            let run = run_id(1);
            building(&rig, &run);
            rig.write(
                &run,
                Event::SessionInterrupted {
                    resumes: 65,
                    relaunch: false,
                    run_ordinal: None,
                    branch: None,
                    t3_error: None,
                    ended_in_pass: 0,
                },
            );
            rig.write(
                &run,
                Event::SessionLaunched {
                    thread_id: resumed_session().to_string(),
                    worktree: Some(fresh().to_string()),
                    branch: Some("fac/8-1-r65".into()),
                    model_instance: None,
                    model: None,
                    model_options: None,
                    resumed_from: None,
                    base: Some("main".into()),
                    title: None,
                },
            );
            rig.view = FleetView::builder()
                .online(m1())
                .online(m2())
                .workspace(workspace_record(m2(), fresh(), "fac/8-1-r65", None))
                .session(session_record(
                    resumed_session(),
                    fresh(),
                    SessionState::Failed,
                ))
                .build();
            never_written(&rig);
            let started = std::time::Instant::now();
            assert!(matches!(
                pass(&rig, &run),
                ResumeStep::Archiving { resumes: 66, .. }
            ));
            // Asked again while it shows: found by its name among the ledger's.
            assert!(matches!(
                pass(&rig, &run),
                ResumeStep::Archiving { resumes: 66, .. }
            ));
            assert!(started.elapsed() < Duration::from_secs(3));
        }

        // testing-11.

        #[test]
        fn a_record_with_no_setup_on_the_recorded_machine_is_not_the_creates_answer() {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            rig.step(&run, &format!("resume-machine 1 {}", m2()));
            rig.step(&run, "resume-requested 1 fac/8-1-r1");
            rig.view = with_resume_workspace("fac/8-1-r1", None).build();
            rig.worker.on_create(Ok(fresh3()));
            assert_eq!(pass(&rig, &run), ResumeStep::Waiting(Wait::Workspace));
            let calls = create_calls(&rig);
            assert_eq!(calls.len(), 1);
            assert!(
                matches!(&calls[0], Call::Create { target, .. } if *target == Target::Machine(m2()))
            );
            assert!(resume_steps(&rig, &run).contains(&format!("resume-workspace 1 {}", fresh3())));
        }

        // testing-13: the claim machine is gone and the session's own machine is away only a
        // minute: the session's machine decides.

        #[test]
        fn a_session_whose_machine_is_away_a_minute_is_not_interrupted_by_the_claim_machine() {
            let (mut rig, run) = at_the_start();
            resumed(&mut rig, &run);
            rig.view = FleetView::builder()
                .unreachable(m2(), secs(60))
                .unreachable(m1(), secs(60 * 60))
                .workspace(workspace_record(m2(), fresh(), "fac/8-1-r1", None))
                .session(Record {
                    owner: m2(),
                    ..session_record(resumed_session(), fresh(), SessionState::Working)
                })
                .build();
            let before = interruptions(&rig, &run).len();
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(interruptions(&rig, &run).len(), before);
        }
    }

    /// The review of the pull request, round 1.
    mod review_round_1 {
        use super::*;

        fn at_the_start() -> (Rig, RunId) {
            let (mut rig, run) = archived_and_waiting_for_a_machine();
            resume_workspace_made(&mut rig, &run);
            (rig, run)
        }

        fn ended_workspaces(rig: &Rig) -> Vec<WorkspaceId> {
            rig.worker
                .calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::EndSessions { workspace } => Some(workspace),
                    _ => None,
                })
                .collect()
        }

        /// A resume whose start went out and whose answer was lost: `resume-sending 1` is
        /// recorded and no `session_launched`, so a session may run in the resume workspace.
        fn a_start_answer_lost() -> (Rig, RunId) {
            let (rig, run) = at_the_start();
            rig.worker
                .on_start_session(Err(WorkerError::Unreachable("timed out".into())));
            assert!(advance(&rig.ctx(), &run).is_err());
            assert!(resume_steps(&rig, &run).contains(&"resume-sending 1".to_string()));
            (rig, run)
        }

        // f2, f8: a closing mid-resume ends the sessions of the resume workspace.

        #[test]
        fn a_closed_issue_after_a_lost_resume_start_ends_the_resume_workspaces_sessions() {
            let (rig, run) = a_start_answer_lost();
            rig.sim.set_closed(true);
            assert_eq!(pass(&rig, &run), ResumeStep::Abandoned);
            assert_eq!(ended_workspaces(&rig), vec![fresh()]);
            assert!(rig.finished(&run));
        }

        #[test]
        fn an_own_merge_after_a_lost_resume_start_ends_the_resume_workspaces_sessions() {
            let (rig, run) = a_start_answer_lost();
            pr_opened(&rig, &run);
            rig.sim.set_pull(
                12,
                SimPull {
                    state: "MERGED",
                    ..pr_on(PR_BRANCH, false)
                },
            );
            rig.sim.set_closed(true);
            assert_eq!(pass(&rig, &run), ResumeStep::Ended);
            assert_eq!(ended_workspaces(&rig), vec![fresh()]);
            assert!(rig.finished(&run));
        }

        #[test]
        fn a_closed_issue_after_a_lost_resume_start_on_a_machine_gone_asks_it_nothing() {
            let (mut rig, run) = a_start_answer_lost();
            rig.view = FleetView::builder()
                .online(m1())
                .workspace(workspace_record(m2(), fresh(), "fac/8-1-r1", None))
                .unreachable(m2(), secs(15 * 60))
                .build();
            rig.sim.set_closed(true);
            assert_eq!(pass(&rig, &run), ResumeStep::Abandoned);
            assert_eq!(ended_workspaces(&rig), Vec::<WorkspaceId>::new());
            assert!(rig.finished(&run));
        }

        // f3: a run with a recorded gate pass is the gate's, even with no pull request reported
        // since.

        #[test]
        fn a_run_with_a_recorded_gate_pass_is_left_to_the_gate() {
            let (rig, run) = interrupted();
            pr_opened(&rig, &run);
            rig.write(
                &run,
                Event::GateSentBack {
                    attempt: 1,
                    run_ordinal: 1,
                },
            );
            rig.ledger
                .write(
                    &run,
                    &Event::StepChanged {
                        step: format!("gate-passed {} 1", "a".repeat(40)),
                    },
                    &Cause::new(CauseStep::Gate, "gate-passed").attempt(1),
                )
                .unwrap();
            let seen = read_progress(&rig.events(&run));
            assert!(seen.pass.is_some() && !seen.pr_reported);
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
            assert_eq!(rig.worker.calls(), vec![]);
        }

        // f4: an interruption of a run that `start` left in triage moves it to building first.

        #[test]
        fn an_interrupted_run_still_in_triage_is_moved_to_building_first() {
            let mut rig = Rig::new();
            let run = run_id(1);
            rig.step(&run, &format!("approved {LABELED_AT}"));
            rig.write(
                &run,
                Event::Claimed {
                    machine: m1().to_string(),
                    github_user: LOGIN.into(),
                },
            );
            rig.moved(&run, State::Ready, State::Triage);
            rig.step(&run, "launch-model opus");
            rig.step(&run, &format!("workspace-requested {BRANCH}"));
            rig.step(&run, &format!("workspace-created {OLD}"));
            rig.step(&run, "approval-rechecked");
            rig.write(&run, launch(Some(BRANCH), Some("main")));
            rig.sim.set_labels(&["factory:triage"]);
            rig.sim.set_assignees(&[LOGIN]);
            rig.view = failed();
            never_written(&rig);
            pass_one(&rig, &run);
            assert!(rig.events(&run).iter().any(|e| {
                e.event == "state_changed"
                    && e.data["from"] == "triage"
                    && e.data["to"] == "building"
            }));
            assert_eq!(rig.sim.labels(), vec!["factory:building".to_string()]);
        }

        // f5: a branch GitHub fails to read is an error, never a resume from the base.

        #[test]
        fn a_branch_github_fails_to_read_decides_nothing_and_the_next_pass_takes_it() {
            let (rig, run) = interrupted();
            rig.sim.add_branch(BRANCH);
            rig.sim.fail("git/ref/heads/fac/8-1", 1);
            assert!(advance(&rig.ctx(), &run).is_err());
            assert!(interruptions(&rig, &run).is_empty());
            assert_eq!(rig.worker.calls(), vec![probe(1)]);
            never_written(&rig);
            pass_one(&rig, &run);
            assert_eq!(interruptions(&rig, &run)[0]["branch"], BRANCH);
        }

        // f9: a session that ended with its turn record behind is not decided until the
        // record shows, or the worker says the turn was never written.

        #[test]
        fn a_finished_session_whose_turn_record_is_behind_is_not_decided_until_the_turn_is_refused()
        {
            let (mut rig, run) = failed_with_turns_unasked();
            rig.step(&run, "turn-read 2");
            rig.view = fleet(Some(SessionState::Finished))
                .turn(turn_of(session(), old(), 2, 2))
                .build();
            // Turn 3 is written on the machine and its record has not synced: wait.
            say(&rig, DONE_MESSAGE);
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
            assert_eq!(
                rig.worker.calls(),
                vec![Call::Turn {
                    session: session(),
                    n: 3
                }]
            );
            // The worker says turn 3 was never written: the process ended before a result line.
            rig.worker.on_turn(Err(WorkerError::NotWritten(
                "turn 3 will not be written".into(),
            )));
            pass_one(&rig, &run);
        }

        #[test]
        fn a_turn_the_worker_cannot_be_reached_for_is_an_error_and_decides_nothing() {
            let (rig, run) = failed_with_turns_unasked();
            rig.worker
                .on_turn(Err(WorkerError::Unreachable("timed out".into())));
            let before = event_count(&rig, &run);
            assert!(advance(&rig.ctx(), &run).is_err());
            assert_eq!(event_count(&rig, &run), before);
            assert!(archive_calls(&rig).is_empty());
        }

        #[test]
        fn a_turn_call_that_failed_decides_nothing_and_a_later_not_written_decides() {
            let (rig, run) = failed_with_turns_unasked();
            rig.worker.on_turn(Err(WorkerError::Failed(
                "the store could not be read".into(),
            )));
            let before = event_count(&rig, &run);
            let error = advance(&rig.ctx(), &run).unwrap_err();
            assert!(
                matches!(error, RunError::Worker(WorkerError::Failed(_))),
                "{error}"
            );
            assert_eq!(rig.worker.calls(), vec![probe(1)]);
            // The one line written is the first failure's record.
            assert_eq!(event_count(&rig, &run), before + 1);
            assert!(interruptions(&rig, &run).is_empty());
            assert!(archive_calls(&rig).is_empty());
            never_written(&rig);
            pass_one(&rig, &run);
            assert!(!interruptions(&rig, &run).is_empty());
        }

        // f6 (a): a resend the worker refuses is left for the next pass, with no error.

        #[test]
        fn a_resend_the_worker_refuses_is_skipped() {
            for refused in [SendRefused::CountRose { now: 1 }, SendRefused::NotIdle] {
                let (mut rig, run) = at_the_start();
                resumed(&mut rig, &run);
                rig.view = with_resume_workspace("fac/8-1-r1", None)
                    .session(session_record(
                        resumed_session(),
                        fresh(),
                        SessionState::Idle,
                    ))
                    .build();
                rig.worker.on_send(Err(refused));
                assert!(is_skipped(&pass(&rig, &run)));
                assert_eq!(sends(&rig).len(), 1);
            }
        }

        // f6 (b): a second start the worker refuses says a session may still run.

        #[test]
        fn a_refused_second_start_says_a_session_may_exist() {
            let (rig, run) = a_start_answer_lost();
            rig.worker
                .on_start_session(Err(WorkerError::Failed("no model".into())));
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Stuck {
                    reason: LAUNCH_FAILED.into()
                }
            );
            let detail = rig
                .steps(&run)
                .into_iter()
                .find(|step| step.starts_with(STUCK_DETAIL))
                .unwrap();
            assert!(
                detail.contains(&messages::session_may_exist_note(
                    "no model",
                    &format!("foundry:{run}:resume-1")
                )),
                "{detail}"
            );
        }
    }

    /// #268: a turn probe that answers `Failed` without a break for longer than
    /// [`TURN_FAILING_FOR`] sends the run stuck for a person: no resume is spent, and nothing
    /// more is asked of the owner, which refuses its calls the same way.
    mod a_turn_probe_that_keeps_failing {
        use std::sync::{Arc, Mutex};

        use super::*;
        use crate::clock::FakeClock;
        use crate::factory::ledger::testing::{T0, ticking};
        use crate::factory::worker::AlwaysHeld;
        use crate::store::Store;

        fn call_failed(rig: &Rig) {
            rig.worker.on_turn(Err(WorkerError::Failed(
                "the owner runs an older Foundry".into(),
            )));
        }

        /// A pass whose probe fails: an error, and no interruption.
        fn fails(rig: &Rig, run: &RunId) {
            call_failed(rig);
            let error = advance(&rig.ctx(), run).unwrap_err();
            assert!(
                matches!(error, RunError::Worker(WorkerError::Failed(_))),
                "{error}"
            );
            assert!(interruptions(rig, run).is_empty());
            assert!(archive_calls(rig).is_empty());
        }

        /// The pass past the limit: the probe fails once more and the run goes stuck for a
        /// person. No resume is spent, and the owner is asked for nothing but the probe: no
        /// archive, no workspace, no sessions ended.
        fn goes_stuck(rig: &Rig, run: &RunId) {
            call_failed(rig);
            assert_eq!(
                pass(rig, run),
                ResumeStep::Stuck {
                    reason: TURN_UNREADABLE.into()
                }
            );
            assert!(interruptions(rig, run).is_empty());
            assert!(
                rig.worker
                    .calls()
                    .iter()
                    .all(|call| matches!(call, Call::Turn { .. })),
                "{:?}",
                rig.worker.calls()
            );
            assert!(rig.finished(run));
            assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
            let stuck = rig
                .events(run)
                .into_iter()
                .find(|e| e.event == "stuck")
                .unwrap();
            assert_eq!(stuck.cause.unwrap()["reason"], STUCK_TURN);
            // The call's error is kept in the ledger, never in the comment.
            assert!(stuck.data.to_string().contains("older Foundry"));
            let comment = the_stuck_comment(rig);
            assert!(comment.contains(TURN_UNREADABLE), "{comment}");
            assert!(!comment.contains("older Foundry"), "{comment}");
        }

        fn minutes(rig: &Rig, n: u64) {
            rig.clock.advance(secs(n * 60));
        }

        /// The first-failure and answered lines of `run`, in order.
        fn records(rig: &Rig, run: &RunId) -> Vec<String> {
            rig.steps(run)
                .into_iter()
                .filter(|step| step.starts_with(TURN_FAILING) || step.starts_with(TURN_ANSWERED))
                .collect()
        }

        fn failing(turn: u64, generation: u64, ms: u64) -> String {
            format!("{TURN_FAILING}{SESSION} {turn} {generation} {ms}")
        }

        fn answered(turn: u64) -> String {
            format!("{TURN_ANSWERED}{SESSION} {turn}")
        }

        #[test]
        fn the_limit_is_the_unreachable_limit() {
            assert_eq!(TURN_FAILING_FOR, UNREACHABLE_FOR);
        }

        #[test]
        fn the_first_failure_is_recorded_on_the_clock_before_the_error_is_returned() {
            let (rig, run) = failed_with_turns_unasked();
            minutes(&rig, 3);
            fails(&rig, &run);
            assert_eq!(records(&rig, &run), [failing(1, 1, 180_000)]);
            // A second failure keeps the first one's time.
            minutes(&rig, 1);
            fails(&rig, &run);
            assert_eq!(records(&rig, &run), [failing(1, 1, 180_000)]);
        }

        #[test]
        fn a_probe_that_keeps_failing_past_the_limit_goes_stuck_for_a_person() {
            let (rig, run) = failed_with_turns_unasked();
            fails(&rig, &run);
            minutes(&rig, 14);
            fails(&rig, &run);
            minutes(&rig, 1);
            goes_stuck(&rig, &run);
            assert_eq!(rig.worker.calls(), vec![probe(1), probe(1), probe(1)]);
            // The stuck run is not carried on: a later pass asks nothing.
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(rig.worker.calls().len(), 3);
        }

        #[test]
        fn a_probe_short_of_the_limit_is_still_an_error() {
            let (rig, run) = failed_with_turns_unasked();
            fails(&rig, &run);
            minutes(&rig, 14);
            fails(&rig, &run);
            assert!(!rig.finished(&run));
            assert!(rig.sim.comments().is_empty());
        }

        #[test]
        fn a_never_written_answer_clears_the_record() {
            let (rig, run) = failed_with_turns_unasked();
            fails(&rig, &run);
            minutes(&rig, 10);
            never_written(&rig);
            pass_one(&rig, &run);
            assert_eq!(records(&rig, &run), [failing(1, 1, 0), answered(1)]);
        }

        #[test]
        fn the_fleet_showing_the_session_unreachable_breaks_the_failures() {
            let (mut rig, run) = failed_with_turns_unasked();
            fails(&rig, &run);
            minutes(&rig, 10);
            // The owner drops off the fleet for a minute: its session shows unreachable and
            // the turn is not asked for.
            rig.view = fleet(Some(SessionState::Unreachable))
                .unreachable(m1(), secs(60))
                .build();
            let _ = advance(&rig.ctx(), &run);
            assert_eq!(rig.worker.calls(), vec![probe(1)]);
            assert_eq!(records(&rig, &run), [failing(1, 1, 0), answered(1)]);
            // Back with the session failed: the count starts again from the next failure.
            rig.view = failed();
            minutes(&rig, 10);
            fails(&rig, &run);
            assert_eq!(
                records(&rig, &run),
                [failing(1, 1, 0), answered(1), failing(1, 1, 1_200_000)]
            );
        }

        #[test]
        fn a_failure_recorded_for_another_session_does_not_count() {
            let (rig, run) = failed_with_turns_unasked();
            let other = format!("{TURN_FAILING}{} 1 1 0", resumed_session());
            rig.step(&run, &other);
            minutes(&rig, 20);
            fails(&rig, &run);
            assert_eq!(records(&rig, &run), [other, failing(1, 1, 1_200_000)]);
            assert!(!rig.finished(&run));
        }

        #[test]
        fn a_stuck_path_cut_short_is_finished_asking_the_owner_nothing() {
            let (rig, run) = failed_with_turns_unasked();
            fails(&rig, &run);
            minutes(&rig, 15);
            call_failed(&rig);
            rig.sim.fail("could not finish this issue", 1);
            assert!(advance(&rig.ctx(), &run).is_err());
            assert!(!rig.finished(&run));
            // The next pass finishes the path from the ledger, without asking the turn again.
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Stuck {
                    reason: TURN_UNREADABLE.into()
                }
            );
            assert!(rig.finished(&run));
            assert_eq!(rig.sim.labels(), vec!["factory:stuck"]);
            assert_eq!(rig.worker.calls(), vec![probe(1), probe(1)]);
        }

        #[test]
        fn a_probe_that_recovers_before_the_limit_decides_nothing_and_clears_the_record() {
            let (rig, run) = failed_with_turns_unasked();
            fails(&rig, &run);
            minutes(&rig, 10);
            // The turn is written and its record has not synced: nothing is decided.
            say(&rig, DONE_MESSAGE);
            assert!(is_skipped(&pass(&rig, &run)));
            assert!(interruptions(&rig, &run).is_empty());
            assert_eq!(records(&rig, &run), [failing(1, 1, 0), answered(1)]);
            // A failure after the read counts from itself, not from the first.
            minutes(&rig, 10);
            fails(&rig, &run);
            assert_eq!(
                records(&rig, &run),
                [failing(1, 1, 0), answered(1), failing(1, 1, 1_200_000)]
            );
            minutes(&rig, 14);
            fails(&rig, &run);
            minutes(&rig, 1);
            goes_stuck(&rig, &run);
        }

        #[test]
        fn a_read_with_no_failure_recorded_writes_nothing() {
            let (rig, run) = failed_with_turns_unasked();
            say(&rig, DONE_MESSAGE);
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
        }

        #[test]
        fn an_unreachable_answer_breaks_the_failures() {
            let (rig, run) = failed_with_turns_unasked();
            fails(&rig, &run);
            minutes(&rig, 10);
            rig.worker
                .on_turn(Err(WorkerError::Unreachable("timed out".into())));
            assert!(advance(&rig.ctx(), &run).is_err());
            assert_eq!(records(&rig, &run), [failing(1, 1, 0), answered(1)]);
            minutes(&rig, 10);
            fails(&rig, &run);
            assert_eq!(records(&rig, &run).len(), 3);
        }

        #[test]
        fn a_restart_of_the_daemon_keeps_the_first_failure_time() {
            let (mut rig, run) = failed_with_turns_unasked();
            fails(&rig, &run);
            minutes(&rig, 10);
            // The daemon restarts: the ledger is opened again from its store, the lease is
            // renewed at the same generation, and the clock has counted on.
            let store = Store::open(rig._dir.path().join("foundry.db")).unwrap();
            rig.ledger = crate::factory::ledger::testing::ledger(
                &Arc::new(Mutex::new(store)),
                MachineId(uuid::Uuid::from_u128(0x1)),
                1,
                ticking(T0 + 3_600_000, 1000),
            );
            assert_eq!(records(&rig, &run), [failing(1, 1, 0)]);
            minutes(&rig, 4);
            fails(&rig, &run);
            minutes(&rig, 1);
            goes_stuck(&rig, &run);
        }

        #[test]
        fn a_takeover_starts_the_count_again_and_never_ends_it_early() {
            let (mut rig, run) = failed_with_turns_unasked();
            fails(&rig, &run);
            minutes(&rig, 10);
            rig.guard = Box::new(AlwaysHeld(2));
            // 20 minutes since the first failure, but the count is a new coordinator's.
            minutes(&rig, 10);
            fails(&rig, &run);
            assert_eq!(
                records(&rig, &run),
                [failing(1, 1, 0), failing(1, 2, 1_200_000)]
            );
            minutes(&rig, 14);
            fails(&rig, &run);
            minutes(&rig, 1);
            goes_stuck(&rig, &run);
        }

        #[test]
        fn a_clock_reading_below_the_record_starts_the_count_again() {
            let (mut rig, run) = failed_with_turns_unasked();
            minutes(&rig, 30);
            fails(&rig, &run);
            // A reboot reset the clock.
            rig.clock = FakeClock::new();
            minutes(&rig, 5);
            fails(&rig, &run);
            assert_eq!(
                records(&rig, &run),
                [failing(1, 1, 1_800_000), failing(1, 1, 300_000)]
            );
            minutes(&rig, 15);
            goes_stuck(&rig, &run);
        }

        #[test]
        fn a_failure_of_another_turn_does_not_count() {
            let (rig, run) = failed_with_turns_unasked();
            fails(&rig, &run);
            // Building read turn 1 since: the probe asks for turn 2 now.
            rig.step(&run, "turn-read 1");
            minutes(&rig, 20);
            fails(&rig, &run);
            assert_eq!(
                records(&rig, &run),
                [failing(1, 1, 0), failing(2, 1, 1_200_000)]
            );
            assert_eq!(rig.worker.calls(), vec![probe(1), probe(2)]);
        }
    }

    // #367: a session that may be at work on a machine cut off for 15 minutes, with its pull
    // request reported, is an interruption as one with none reported is.
    mod a_reported_pull_request_cut_off_at_work {
        use super::*;
        use crate::factory::gate::{self, MACHINE_GONE_AFTER};
        use crate::factory::runs::gating::{GATE_COULD_NOT_RUN, gate_running};
        use crate::factory::runs::progress::GATE_REQUESTED;

        const HEAD: &str = "1111111111111111111111111111111111111111";
        const BASE_TIP: &str = "2222222222222222222222222222222222222222";

        /// The gate's workspace on the build machine.
        fn gate_workspace() -> WorkspaceId {
            WorkspaceId(uuid::Uuid::from_u128(0x6a7))
        }

        /// A building run whose session read turn 1 and reported its pull request, which is
        /// open on its own branch.
        fn reported() -> (Rig, RunId) {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.step(&run, "turn-read 1");
            pr_opened(&rig, &run);
            rig.sim.set_pull(12, pr_on(PR_BRANCH, false));
            (rig, run)
        }

        /// The build machine unreachable for `span`, its session shown unreachable with its
        /// newest turn 1 and `prompts` prompts typed: 2 is a prompt after that turn, so the
        /// session may be at work; 1 is at rest.
        fn away(span: Duration, prompts: u64) -> FleetView {
            FleetView::builder()
                .online(m2())
                .workspace(workspace_record(m1(), old(), BRANCH, None))
                .session(session_record(session(), old(), SessionState::Unreachable))
                .turn(turn_of(session(), old(), 1, prompts))
                .unreachable(m1(), span)
                .build()
        }

        fn at_work(span: Duration) -> FleetView {
            away(span, 2)
        }

        /// A gate placed on the build machine and created, in flight.
        fn gate_in_flight(rig: &Rig, run: &RunId) {
            rig.step(
                run,
                &format!("{GATE_REQUESTED}1 1 {HEAD} {BASE_TIP} {}", m1()),
            );
            rig.step(run, &format!("gate-created 1 {}", gate_workspace()));
            assert!(gate_running(&rig.events(run)));
        }

        #[test]
        fn is_interrupted_at_15_minutes_and_resumed_on_the_pull_requests_branch() {
            let (mut rig, run) = reported();
            rig.view = at_work(UNREACHABLE_FOR);
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Archiving {
                    resumes: 1,
                    workspace: Some(old()),
                }
            );
            let lines = interruptions(&rig, &run);
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0]["branch"], PR_BRANCH);
            assert_eq!(
                resume_steps(&rig, &run),
                [
                    format!("resume-archiving 1 {}", old()),
                    "resume-archive-asked 1".to_string(),
                ]
            );
            // It goes on as a run with no pull request reported does.
            rig.view = archived();
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::NeedsMachine(Placing::Resume { resumes: 1 })
            );
            resume_workspace_made(&mut rig, &run);
            assert_eq!(
                resumed(&mut rig, &run),
                ResumeStep::Started {
                    resumes: 1,
                    session: resumed_session(),
                }
            );
            let Some(Call::Create { request, .. }) = create_calls(&rig).pop() else {
                panic!("no create");
            };
            assert_eq!(request.name, "fac/8-1-r1");
            assert_eq!(
                request.checkout,
                Checkout::Branch {
                    name: PR_BRANCH.into()
                }
            );
        }

        #[test]
        fn records_nothing_and_stays_building_short_of_15_minutes() {
            let (mut rig, run) = reported();
            rig.view = at_work(UNREACHABLE_FOR - Duration::from_secs(1));
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
            assert!(rig.worker.calls().is_empty());
            assert_eq!(rig.ctx().seen(&run).unwrap().state, Some(State::Building));
        }

        /// #294's rule stands: a session at rest when last seen is not at work, and its
        /// reported pull request is the gate's.
        #[test]
        fn a_session_at_rest_when_last_seen_is_left_to_the_gate() {
            let (mut rig, run) = reported();
            rig.view = away(UNREACHABLE_FOR * 2, 1);
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
            assert!(interruptions(&rig, &run).is_empty());
            assert!(rig.worker.calls().is_empty());
        }

        /// A recorded pass is the gate's to finish: no session is needed for it.
        #[test]
        fn a_run_with_a_recorded_pass_is_left_to_the_gate() {
            let (mut rig, run) = reported();
            rig.ledger
                .write(
                    &run,
                    &Event::StepChanged {
                        step: format!("gate-passed {HEAD} 1"),
                    },
                    &Cause::new(CauseStep::Gate, "gate-passed").attempt(1),
                )
                .unwrap();
            rig.view = at_work(UNREACHABLE_FOR);
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
        }

        /// The gate in flight is not read while the session may be at work, so it does not
        /// hide the interruption: it is marked could not run, with the gate's own words for a
        /// machine that is gone, before the interruption is recorded, and its slot is free.
        #[test]
        fn a_gate_in_flight_is_marked_could_not_run_before_the_interruption() {
            let (mut rig, run) = reported();
            gate_in_flight(&rig, &run);
            rig.view = at_work(UNREACHABLE_FOR);
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Archiving {
                    resumes: 1,
                    workspace: Some(old()),
                }
            );
            let events = rig.events(&run);
            assert!(!gate_running(&events));
            let reason = gate::could_not_run(None, MACHINE_GONE_AFTER, None).unwrap();
            let expected = format!("gate-could-not-run 1 1 1 1 {reason}");
            let could_not_run = events
                .iter()
                .position(|e| step_text(e) == Some(expected.as_str()))
                .expect("the gate could not run");
            let interrupted = events
                .iter()
                .position(|e| e.event == "session_interrupted")
                .expect("the interruption");
            assert!(could_not_run < interrupted);
            // Nothing is asked of the gate's machine, which is gone.
            assert_eq!(rig.worker.calls(), vec![archived_call()]);
        }

        /// A gate in flight on a machine that is not gone may still run its commands there,
        /// and nothing reads it while the run is the resume's: it is marked could not run, as
        /// the interruption stopped it, and archived by force, so it holds no slot unread.
        #[test]
        fn a_gate_in_flight_on_a_machine_that_is_not_gone_is_stopped_and_could_not_run() {
            let (mut rig, run) = reported();
            rig.step(
                &run,
                &format!("{GATE_REQUESTED}1 1 {HEAD} {BASE_TIP} {}", m2()),
            );
            rig.step(&run, &format!("gate-created 1 {}", gate_workspace()));
            rig.view = at_work(UNREACHABLE_FOR);
            pass_one(&rig, &run);
            let events = rig.events(&run);
            assert!(!gate_running(&events));
            let expected = format!("{GATE_COULD_NOT_RUN}1 1 1 1 {GATE_CUT_OFF}");
            let could_not_run = events
                .iter()
                .position(|e| step_text(e) == Some(expected.as_str()))
                .expect("the gate could not run");
            let interrupted = events
                .iter()
                .position(|e| e.event == "session_interrupted")
                .expect("the interruption");
            assert!(could_not_run < interrupted);
            assert_eq!(
                rig.worker.calls(),
                vec![
                    Call::Archive {
                        workspace: gate_workspace()
                    },
                    archived_call()
                ]
            );
        }

        /// The gate's failures to run are counted for the attempt in flight, as the gate counts
        /// them, and its wait is the gate's for that count.
        #[test]
        fn the_gates_failures_to_run_are_counted_for_its_attempt() {
            let (mut rig, run) = reported();
            let gone = gate::could_not_run(None, MACHINE_GONE_AFTER, None).unwrap();
            for (attempt, k) in [(1, 1), (2, 2)] {
                rig.step(
                    &run,
                    &format!("{GATE_REQUESTED}{attempt} {k} {HEAD} {BASE_TIP} {}", m1()),
                );
                rig.step(
                    &run,
                    &format!("{GATE_COULD_NOT_RUN}{attempt} {k} 1 1 {gone}"),
                );
            }
            rig.step(
                &run,
                &format!("{GATE_REQUESTED}2 3 {HEAD} {BASE_TIP} {}", m1()),
            );
            assert!(gate_running(&rig.events(&run)));
            rig.view = at_work(UNREACHABLE_FOR);
            pass_one(&rig, &run);
            let wait = gate::backoff(2).unwrap();
            assert!(
                rig.steps(&run)
                    .contains(&format!("{GATE_COULD_NOT_RUN}2 3 2 {wait} {gone}")),
                "{:?}",
                rig.steps(&run)
            );
        }

        /// The run cut off at work, interrupted, and its resume started in `fac/8-1-r1`.
        fn resumed_after_cut_off() -> (Rig, RunId) {
            let (mut rig, run) = reported();
            rig.view = at_work(UNREACHABLE_FOR);
            pass_one(&rig, &run);
            rig.view = archived();
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::NeedsMachine(Placing::Resume { resumes: 1 })
            );
            resume_workspace_made(&mut rig, &run);
            assert!(matches!(
                resumed(&mut rig, &run),
                ResumeStep::Started { resumes: 1, .. }
            ));
            (rig, run)
        }

        fn resumed_in(state: SessionState) -> FleetView {
            with_resume_workspace("fac/8-1-r1", None)
                .session(session_record(resumed_session(), fresh(), state))
                .build()
        }

        /// The resumed session has reported nothing since the interruption: the resume step
        /// keeps the run, so its end is an interruption of its own, as for a run with no pull
        /// request reported, and its first message is asked for again.
        #[test]
        fn a_resumed_session_stays_the_resume_steps_until_it_reports_again() {
            let (mut rig, run) = resumed_after_cut_off();
            rig.view = resumed_in(SessionState::Idle);
            assert!(takes_from_the_gate(&rig.ctx(), &rig.events(&run)));
            rig.worker.on_send(Ok(0));
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Resent {
                    session: resumed_session()
                }
            );
            rig.view = resumed_in(SessionState::Failed);
            never_written(&rig);
            assert!(takes_from_the_gate(&rig.ctx(), &rig.events(&run)));
            assert!(matches!(
                pass(&rig, &run),
                ResumeStep::Archiving { resumes: 2, .. }
            ));
            assert_eq!(interruptions(&rig, &run).len(), 2);
        }

        /// The resumed session's `FACTORY-DONE` for the same pull request writes no second
        /// `pr_opened` (building writes it once per hand-off), only the turn read: once a turn
        /// of the resumed session is read, the run is the gate's again.
        #[test]
        fn a_resumed_session_whose_turn_is_read_is_the_gates() {
            let (mut rig, run) = resumed_after_cut_off();
            rig.step(&run, "turn-read 1");
            rig.view = resumed_in(SessionState::Failed);
            assert!(!takes_from_the_gate(&rig.ctx(), &rig.events(&run)));
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
        }

        /// A turn read for the reminder to end with a result line is no report: the resumed
        /// session stays this step's, and its end is an interruption of its own.
        #[test]
        fn a_resumed_session_read_for_its_reminder_stays_the_resume_steps() {
            let (mut rig, run) = resumed_after_cut_off();
            rig.step(&run, "result-line-reminding 1");
            rig.step(&run, "turn-read 1");
            rig.step(&run, "result-line-reminded");
            rig.view = resumed_in(SessionState::Failed);
            never_written(&rig);
            assert!(takes_from_the_gate(&rig.ctx(), &rig.events(&run)));
            assert!(matches!(
                pass(&rig, &run),
                ResumeStep::Archiving { resumes: 2, .. }
            ));
        }

        /// The reminder's answer, a turn read of its own, reports the pull request again: the
        /// run is the gate's.
        #[test]
        fn a_resumed_session_read_after_its_reminder_is_the_gates() {
            let (mut rig, run) = resumed_after_cut_off();
            rig.step(&run, "result-line-reminding 1");
            rig.step(&run, "turn-read 1");
            rig.step(&run, "result-line-reminded");
            rig.step(&run, "turn-read 2");
            rig.view = resumed_in(SessionState::Failed);
            assert!(!takes_from_the_gate(&rig.ctx(), &rig.events(&run)));
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
        }

        /// A reminder of the session before the interruption is not the resumed session's: its
        /// turn read is a report.
        #[test]
        fn a_reminder_before_the_interruption_does_not_hide_the_resumed_report() {
            let rig = Rig::new();
            let run = run_id(1);
            building(&rig, &run);
            rig.step(&run, "result-line-reminding 1");
            rig.step(&run, "turn-read 1");
            rig.step(&run, "result-line-reminded");
            rig.step(&run, "turn-read 2");
            pr_opened(&rig, &run);
            rig.sim.set_pull(12, pr_on(PR_BRANCH, false));
            let mut rig = rig;
            rig.view = FleetView::builder()
                .online(m2())
                .workspace(workspace_record(m1(), old(), BRANCH, None))
                .session(session_record(session(), old(), SessionState::Unreachable))
                .turn(turn_of(session(), old(), 2, 3))
                .unreachable(m1(), UNREACHABLE_FOR)
                .build();
            pass_one(&rig, &run);
            rig.view = archived();
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::NeedsMachine(Placing::Resume { resumes: 1 })
            );
            resume_workspace_made(&mut rig, &run);
            assert!(matches!(
                resumed(&mut rig, &run),
                ResumeStep::Started { resumes: 1, .. }
            ));
            assert!(takes_from_the_gate(&rig.ctx(), &rig.events(&run)));
            rig.step(&run, "turn-read 1");
            assert!(!takes_from_the_gate(&rig.ctx(), &rig.events(&run)));
        }

        /// A gate placed for a resumed session read only for its reminder is in flight when
        /// that session is interrupted: it is marked could not run before the interruption.
        #[test]
        fn a_gate_in_flight_when_a_resumed_session_is_interrupted_could_not_run() {
            let (mut rig, run) = resumed_after_cut_off();
            rig.step(&run, "result-line-reminding 1");
            rig.step(&run, "turn-read 1");
            rig.step(&run, "result-line-reminded");
            rig.step(
                &run,
                &format!("{GATE_REQUESTED}1 1 {HEAD} {BASE_TIP} {}", m2()),
            );
            rig.step(&run, &format!("gate-created 1 {}", gate_workspace()));
            assert!(gate_running(&rig.events(&run)));
            rig.view = resumed_in(SessionState::Failed);
            never_written(&rig);
            assert!(matches!(
                pass(&rig, &run),
                ResumeStep::Archiving { resumes: 2, .. }
            ));
            let events = rig.events(&run);
            assert!(!gate_running(&events));
            let expected = format!("{GATE_COULD_NOT_RUN}1 1 1 1 {GATE_CUT_OFF}");
            let could_not_run = events
                .iter()
                .position(|e| step_text(e) == Some(expected.as_str()))
                .expect("the gate could not run");
            let interrupted = events
                .iter()
                .rposition(|e| e.event == "session_interrupted")
                .expect("the interruption");
            assert!(could_not_run < interrupted);
        }

        /// A gate on a machine that is not gone whose create was not answered is found in the
        /// fleet by the gate's name, and an earlier gate's workspace is not the one archived.
        #[test]
        fn a_gate_with_no_recorded_workspace_is_found_by_its_name() {
            let (mut rig, run) = reported();
            let earlier = WorkspaceId(uuid::Uuid::from_u128(0x6a6));
            rig.step(
                &run,
                &format!("{GATE_REQUESTED}1 1 {HEAD} {BASE_TIP} {}", m2()),
            );
            rig.step(&run, &format!("gate-created 1 {earlier}"));
            rig.step(&run, "gate-archived 1");
            rig.step(
                &run,
                &format!("{GATE_REQUESTED}2 2 {HEAD} {BASE_TIP} {}", m2()),
            );
            assert!(gate_running(&rig.events(&run)));
            rig.view = FleetView::builder()
                .online(m2())
                .workspace(workspace_record(m1(), old(), BRANCH, None))
                .workspace(workspace_record(m2(), gate_workspace(), "fac/8-1-g2", None))
                .session(session_record(session(), old(), SessionState::Unreachable))
                .turn(turn_of(session(), old(), 1, 2))
                .unreachable(m1(), UNREACHABLE_FOR)
                .build();
            pass_one(&rig, &run);
            assert!(
                rig.steps(&run)
                    .contains(&format!("{GATE_COULD_NOT_RUN}2 2 1 1 {GATE_CUT_OFF}"))
            );
            assert_eq!(
                rig.worker.calls(),
                vec![
                    Call::Archive {
                        workspace: gate_workspace()
                    },
                    archived_call()
                ]
            );
        }

        /// A gate archive the worker refuses is no error: the interruption is recorded in the
        /// same pass, and the gate archives a gate that could not run before it places another.
        #[test]
        fn a_gate_archive_that_fails_does_not_hold_the_interruption() {
            let (mut rig, run) = reported();
            rig.step(
                &run,
                &format!("{GATE_REQUESTED}1 1 {HEAD} {BASE_TIP} {}", m2()),
            );
            rig.step(&run, &format!("gate-created 1 {}", gate_workspace()));
            rig.view = at_work(UNREACHABLE_FOR);
            rig.worker
                .on_archive(Err(WorkerError::Failed("refused".into())));
            pass_one(&rig, &run);
            assert_eq!(interruptions(&rig, &run).len(), 1);
            assert!(!gate_running(&rig.events(&run)));
        }

        /// A stuck path this step began for a cut-off run (its resumes used) stays this step's
        /// to finish once the fleet no longer shows the session cut off.
        #[test]
        fn a_stuck_path_begun_for_a_cut_off_run_stays_the_resume_steps() {
            let (mut rig, run) = reported();
            rig.config.limits.interruption_resumes = 0;
            rig.view = at_work(UNREACHABLE_FOR);
            rig.sim.fail("could not finish this issue", 1);
            assert!(advance(&rig.ctx(), &run).is_err());
            assert!(interruptions(&rig, &run).is_empty());
            rig.view = archived();
            assert!(takes_from_the_gate(&rig.ctx(), &rig.events(&run)));
            assert_eq!(
                pass(&rig, &run),
                ResumeStep::Stuck {
                    reason: MACHINE_UNREACHABLE.into()
                }
            );
        }

        /// Once the resumed session reports its pull request again, the gate has the run.
        #[test]
        fn a_resumed_session_that_reports_again_is_the_gates() {
            let (mut rig, run) = resumed_after_cut_off();
            rig.view = resumed_in(SessionState::Idle);
            pr_opened(&rig, &run);
            assert!(!takes_from_the_gate(&rig.ctx(), &rig.events(&run)));
            rig.view = resumed_in(SessionState::Failed);
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
        }

        /// Short of 15 minutes the gate in flight is left to the gate's own rules.
        #[test]
        fn a_gate_in_flight_short_of_15_minutes_is_left_running() {
            let (mut rig, run) = reported();
            gate_in_flight(&rig, &run);
            rig.view = at_work(UNREACHABLE_FOR - Duration::from_secs(1));
            let before = event_count(&rig, &run);
            assert!(is_skipped(&pass(&rig, &run)));
            assert_eq!(event_count(&rig, &run), before);
            assert!(gate_running(&rig.events(&run)));
        }
    }
}

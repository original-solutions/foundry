//! Instructor feedback on a `pr-open` run, and instructors' comments forwarded to a building one.
//!
//! Ported from the runner's `src/runner/feedback.ts` with `feedback.test.ts`. [`advance`] is one
//! pass over a run the gate left in `pr-open`: every `feedback_seconds` it reads what the
//! instructors wrote on the pull request and on the issue, and starts a round on it. A round
//! moves the label back to `building`, makes a workspace named `fac/<N>-<attempt>-f<k>` on the
//! pull request's branch and starts a session in it with the resumed message
//! ([`messages::feedback`]) and the feedback. No pass waits on a worker. Every step is recorded
//! before it is done and a step already recorded is not done again, so a pass cut short and
//! repeated carries on and asks nothing twice. The lease is checked, through `RunContext`'s
//! methods, before every GitHub change and every worker call.
//!
//! What a pass does, in order:
//!
//! 1. **Not this step's.** A run with no events (an issue the runner left in `pr-open`, which
//!    Foundry never ran), a finished run, one with no session or pull request recorded, one in
//!    another repo, one that is stuck or whose stop was asked for, and one with a re-gate under
//!    way (`gate_regate`, or a `regate_relaunching` decision of the runner's) is left alone:
//!    [`FeedbackStep::None`], with no call to GitHub and nothing written.
//! 2. **A round under way** is carried on before anything is read: see 5 and 6.
//! 3. **The read.** Only a run in `pr-open` that has a window (`Seen::pr_open_since`, the pass
//!    that took it there) is read, and only when a read is due. The window is kept in the
//!    ledger, measured on `ctx.clock` as gating's `checks-waiting` is: every read writes one
//!    step, `feedback-read <generation> <ms>`, with ` clean` after it when the read found
//!    nothing new. A read is due when no such step is on record, when the newest is of another
//!    lease generation or above the clock now (a takeover, or a clock that was reset: never
//!    later), or when `feedback_seconds` have passed since it. A pass that finds nothing due
//!    writes and reads nothing. A read that fails writes its step too, so it waits for the
//!    next window. GitHub is asked from one second before the run reached `pr-open` (the gate's
//!    pass), and the reviews are paged back only to the newest clean read less a minute (the
//!    wall time that step carries as an event), and never before that moment. An item the
//!    ledger names (`feedback_sent`: relaunched or forwarded as feedback; `comment_forwarded`
//!    and `comment-dropped` for an issue comment) is never read as new, and an edit keeps its
//!    id. Items are in time order.
//! 4. **A round starts only when it can.** With feedback found, a run slot must be free
//!    (`slots.runs_free > 0`, which the round takes: two runs in one pass do not both take the
//!    last) and `placing::place_run` must have a machine (which the round reserves in
//!    `reserved`). Otherwise the pass is [`FeedbackStep::Waiting`] with the reason ([`NO_SLOT`],
//!    or the placement's own, or [`NO_MACHINE`]), and nothing is written, on GitHub or in the
//!    ledger, not even the read's step: the issue stays `pr-open`, the wait adds nothing to
//!    `Seen::worked_ms` (which counts `triage` and `building` only), and the feedback is read
//!    again on the next pass, so a slot that frees is used at once. (The items are not kept:
//!    the read costs two calls on each pass while a round with feedback waits.) Then, as the
//!    runner does, the pull request is read (merged or closed ends the run with `end_run`), the
//!    issue (closed: `abandon`), the issue text against the approval (a text a person who is not
//!    an instructor changed, one that cannot be judged and a run with no recorded approval are
//!    stuck, as `building`'s are) and the pull request's branch: one that is not a plain branch
//!    name (`a..b`, a leading `-`, a space, a non-ASCII character) is stuck with
//!    [`BRANCH_REFUSED`] before any machine is placed or workspace asked for (review focus 2).
//! 5. **The decision.** `feedback_relaunching` (the number `n` of the round, one past the
//!    highest, the branch, the ids of the items and the body of the first message), then the
//!    step `feedback-machine <n> <id>`, then the move of the lifecycle's `pr-open` and
//!    `instructor-feedback` (the pull request a draft again, the issue assigned, the label
//!    `building`, the status comment, and `state_changed` last), then the decision again, and
//!    the earlier workspace's archive. The label moves in the pass the round starts, not when
//!    the session starts as the runner's did, because `ledger::list::holds_slot` counts a run's
//!    slot from `state_changed` to `building` and the spec has the round's session hold a
//!    slot. `Seen` clears a decision at a state change, so the decision is written again after
//!    the move: until the session is recorded, `pending_relaunch` is `Some`, which is what
//!    `building::advance` and `resume::advance` skip a run for (a `building` run whose newest
//!    session is the old, ended one would otherwise be read as an interruption).
//!    `minutes`, `gate_retries` and `interruption_resumes` start again: see below.
//! 6. **The workspace and the session**, a step to a pass, as `resume` does. The earlier
//!    workspace (the run's newest, when the fleet still shows it: the gate archived it at
//!    `pr-open`, so normally it does not) is archived first: `feedback-archiving <n> <id>`, the
//!    call, `feedback-archive-asked <n>`, and the pass ends there; it is asked again each pass
//!    while it shows on a machine that is heard from, and the round goes on once it shows
//!    archived or its machine has been unreachable for 15 minutes. The new workspace is never
//!    asked for in a pass that asked for the archive. Then `feedback-requested <n> <name>`,
//!    the create on the machine the decision placed (`Checkout::Branch` of the pull request's
//!    head and the worktree's own setup), and `feedback-workspace <n> <id>`; a record the fleet
//!    shows under that name with its setup started is the create's answer. The setup terminal
//!    is waited for (a failed setup is stuck, with its tail), a session the fleet already shows
//!    in the workspace with a turn record is adopted, the text is checked once more against the
//!    approval, and the session starts: `feedback-sending <n>` before, or, when a start may
//!    already have gone out, `feedback-ending-sessions <n>` and the workspace's sessions are
//!    ended first. A start the worker refuses is stuck (and says a session may exist when an
//!    earlier try may have gone out); a machine that cannot be reached is tried again next
//!    pass. `session_launched` (with `resumed_from` the run and the branch, the run's base) is
//!    written after, and the status comment names the machine and the session. A machine
//!    unreachable for 15 minutes before that is given up (`feedback-machine-given-up`) and
//!    placement picks another. A round that is under way and whose issue was closed (or whose
//!    own merge closed it) ends the run and ends the sessions of its workspace; one whose pull
//!    request was merged is `Ended`.
//! 7. **The first message again.** A session idle with no prompt a pass later has the first
//!    message asked for again by count (`feedback-resending <n> 0`), which the worker types only
//!    when its count has not risen, as `start`'s and `resume`'s resends are.
//!
//! What the pass (task 30) must do with this step:
//!
//! - Call [`advance`] for a run in `pr-open` and for a run in `building` whose newest session
//!   is a round's (`pending_relaunch` is `Some`, or the newest `session_launched` follows the
//!   newest `feedback_relaunching` and shows no turn yet), every pass, with the `slots` and the
//!   `reserved` of the pass, after the claims' placement and before they take the slots left.
//! - Not call `building::advance` or `resume::advance` for a run while `pending_relaunch` is
//!   `Some`: they skip it. Not call `start::advance`, nor `gating::advance`, for it either:
//!   `start`'s resend would send the build's launch words to the round's session, and `gating`
//!   reads the old, ended session; `gating` is for a run `building::advance` said
//!   `ReadyForGate` for. Once the round's session shows a turn, `building::advance` follows it
//!   as any building session, and its pull request is gated again.
//! - Call `stop::reconcile` before this step: a merged pull request or a closed issue ends the
//!   run there, and what both write is idempotent through the ledger.
//! - Call [`forward`] for a run in `building` every pass, before or after `building::advance`
//!   (it holds back while the session's newest turn is unread). It sends at most one message
//!   per run per pass.
//!
//! [`forward`] ports the runner's `forward` (`src/runner/building.ts`) with `forward.test.ts`:
//! each new instructor comment on the issue and each edit of its body goes into the building
//! run's session once, in order, and is recorded; a send the worker refuses for good
//! (`BodyWithoutPath`, `SecretsUnreadable`) is dropped, and the issue is told once, in fixed
//! words ([`messages::dropped_comment`], [`messages::REFUSED_BODY_EDIT`]); a comment the
//! activity read drops (edited by someone who is not an instructor, or an edit history that
//! cannot be judged) is reported once. The send is the shared `send_once` through
//! `apply_actions`: `send-intent`, `send-refused` and `send-typed` steps under the key
//! `forward-comment <id>` or `forward-body-edit <step>`, the prompt count the session's turn
//! shows, then `comment_forwarded` or `body-edit-forwarded <edited_at>`. A run is skipped (no
//! read, no write) when it is finished, has a relaunch, an interruption or a rate limit decided,
//! has no session or no events, is not `building` (a gate pass, a stop asked for, a stuck path
//! or questions posted, begun or done), or has a gate failure being sent into its session. A
//! closed issue abandons the run, unless the run's own merge closed it. The activity is asked
//! for from the run's first line cut to whole seconds, never a moving cursor.
//!
//! One message goes in per pass. The worker types a message only when the session is idle and
//! its count of prompts is the one it is sent under, and the count is read from a fleet view
//! that shows a typed message a pass later, so a second send in the pass would be refused. What
//! is left goes in the next pass. Nothing is typed (and nothing written for it) while the
//! session is not idle, shows no turn to count by, has a question waiting, or has a newest turn
//! `building` has not read yet: a message typed then would start a turn that hides the unread
//! one, and a turn is acted on once. The pass shows that as [`ForwardStep::held_back`].
//! `forward` can be called before or after `building::advance` for that reason. The text of
//! what is sent is a body file named by a fixed line ([`FORWARD_COMMENT_LINE`],
//! [`FORWARD_BODY_LINE`]), since a typed line is one short line.
//!
//! The three budgets start again for the round because of what the round writes:
//!
//! - `minutes`: `Seen::worked_ms` is zeroed at a `state_changed` from `pr-open` to `building`,
//!   and the stint runs from it, so `building::time_limit` counts from the round.
//! - `gate_retries`: `gating` reads a round as beginning at each `state_changed` to `building`
//!   (its `Log::read`), so its first gate has the whole budget.
//! - `interruption_resumes`: **not reset by anything this step can write.** `resume::decide`
//!   counts from `Seen::interruption`, the newest `session_interrupted` line of the run, which a
//!   round does not clear (the runner's did not either). A run that used its resumes before
//!   `pr-open` is stuck on the first interruption of the round's session. The fix is in
//!   `progress.rs` (the `feedback_relaunching` arm clears `seen.interruption`) or in
//!   `resume.rs` (`decide` counts the lines after the newest `feedback_relaunching`); neither
//!   is this issue's file. A `session_interrupted` or `session_rate_limited` line written here
//!   would be read as an owed resume, a relaunch or a rate limit, which `resume` and `building`
//!   skip a run for.
//!
//! Where it differs from the runner:
//!
//! - The runner launched the new session first and committed the label after; Foundry commits
//!   the label first (above), so the slot is held from the pass the round starts.
//! - The runner's relaunch was a `relaunchSession` of T3 (a thread launched under a command id,
//!   a worktree path, host and cloud messages); Foundry makes a workspace and starts a session
//!   in it, as `resume` does, and the runner's command id has become the workspace name and
//!   the recorded steps. A refused start names `foundry:<run>:feedback-<n>` where the runner
//!   named its launch command id.
//! - The runner kept the time of the last read and of the last clean read in a file under the
//!   data dir; Foundry has no such directory, so each read is a ledger step, on the
//!   sleep-counting clock under the lease generation.
//! - The old session is not stopped here: the gate ended it and archived its workspace at
//!   `pr-open` (the runner's `worktree_removed` has no counterpart). A workspace that still
//!   shows is archived by force before the round's is made.
//! - A pull request from a fork is not refused here, as the runner did not refuse it.
//!
//! Runner cases of `feedback.test.ts` that have no Rust test of the same name, each with its
//! reason. The cases not listed are ported under their names, or under the name noted.
//!
//! `feedback: the window`:
//! - `a failed regate does not move the start of the feedback window`: no re-gate; the case
//!   that pins the start of the window is `the_window_starts_at_the_pass_that_took_the_run_to_pr_open`.
//!
//! `feedback: the step`:
//! - `a run that reached pr-open before a pass removed worktrees has its old session stopped
//!   before the relaunch`: the gate ends the session and archives the workspace at `pr-open`;
//!   a workspace that still shows is the case `a_run_that_reached_pr_open_with_its_build_workspace_still_showing_has_it_archived_before_the_relaunch`.
//! - `a time of the last read that cannot be written does not stop the read` and `a feedback
//!   directory nobody can write does not stop the read`: no file; a ledger step that cannot be
//!   written is an error, as every step's is.
//! - `the time of a new read keeps the clean time on the next line`: each read is a step of its
//!   own; the case is `the_time_of_a_new_read_keeps_the_clean_time_of_the_read_before`.
//! - `a relaunch names the login its host resolves to: the host's key, or a repo's override`,
//!   `a relaunch sends the cloud message, with the same resume and feedback, and no login
//!   check`: T3's host and cloud launch messages; a session starts as the project's login.
//! - `a crash after the decision sends the launch again under the same command id`: the case is
//!   `a_crash_after_the_decision_starts_the_session_once`.
//! - `regate leaves a pending feedback relaunch to the feedback step` and `regate leaves a
//!   feedback relaunch whose session is recorded to the feedback step`: there is no `regate`;
//!   what they protect (nothing else acts on a round being set up) is
//!   `building_and_resume_leave_a_round_being_set_up_alone`.
//! - `a feedback relaunch that is not finished is never read as unchanged by the pass`: the
//!   runner's `lastPass`; the gate's pass of a run in `building` is not read.
//! - `a refused launch goes stuck and says the feedback command id` and `a refused resend says
//!   a session may exist under the feedback command id`: the cases are `a_refused_start_goes_stuck_without_saying_a_session_may_exist`
//!   and `a_refused_second_start_says_a_session_may_exist_under_the_feedback_name`.
//! - `a run of another repo is an error and nothing is touched`: the case is
//!   `a_run_of_another_repo_is_left_alone_and_nothing_is_touched` (a run id names its repo).
//! - `a re-gate relaunch with no session recorded is left to the gate`: the runner's regate.
//! - `a conflict after a pass and a feedback relaunch gets the full send-back budget`: the
//!   gate's own cases (`review_a_round_has_its_own_budget`); here
//!   `a_round_is_a_return_to_building_after_pr_open_which_is_where_the_gate_starts_its_budget_again`.
//! - `forward does not send an issue comment again that went as feedback`: ported with `forward`.
//! - `feedback: a run whose launch recorded a base other than the project's`: the case is
//!   `relaunches_on_a_run_base_and_records_it`; the gate's read of that base's tip is the gate's.

//!
//! Runner cases of `forward.test.ts` that have no Rust test of the same name, each with its
//! reason. The cases not listed are ported under their names, or under the name noted.
//!
//! `forward: the actions` (`send a forwarded comment and then record it`, `cannot forward a
//! comment with no thread or message`, `send a forwarded body edit and then record its step`,
//! `cannot forward a body edit with no thread or step`): `apply_actions` is task 22's and is
//! tested in `runs/mod.rs` (`a_forward_goes_to_the_session_and_is_recorded`,
//! `a_forward_without_its_payload_is_an_invalid_transition`).
//!
//! `forward: guards and failures`:
//! - `on a run with no file is a ledger error`: a run's events are a read of the store, and a
//!   run with none is one Foundry never ran; the case is `on_a_run_with_no_events_is_skipped_as_having_no_session`.
//! - `a failed send (transport) stops the call, and the rest is sent in order under the same
//!   ids`: ported under that name with the count a send is guarded by in place of a command id.
//!
//! `forward: waiting for the run a message starts` (every case): the runner's wait markers
//! (`forward-wait <k>`, `forward-wait-cancelled`) and T3's run ordinals and thread status make
//! the gate wait for the run a forwarded message starts in an idle thread. Foundry has no run
//! ordinals; a session that is working takes no message, a typed message is known by the count
//! of prompts, and the gate reads the session's state and turn. What the cases protect (a
//! message is never typed into an idle session over a turn nobody read) is
//! `a_newest_turn_that_building_has_not_read_is_read_first`, `a_session_at_work_holds_what_is_to_be_sent_back_and_writes_nothing`
//! and `a_session_the_fleet_shows_no_turn_for_is_waited_for`; `a thread that no longer exists
//! skips the call and sends nothing` is ported under its name (the fleet shows no such session)
//! and `an archived thread counts as one T3 no longer has` has no counterpart.
//!
//! `forward: a send T3 refuses`: the cases about a wait marker that a refusal cancels, keeps or
//! leaves after a crash (`a refused comment followed by a sendable edit sends the edit and
//! keeps the wait`, `a call whose every send was refused holds no gate and leaves no wait`,
//! `a refused send under a marker another message used keeps the wait`, `a marker whose refusal
//! was recorded before a crash is cancelled on the next call`, `a marker whose refusal was
//! recorded is cancelled when its comment has since gone`, `a recorded refusal is cancelled
//! before the issue hears its comment was dropped`, `a marker with a send whose outcome was
//! never recorded keeps the wait`, `a refusal of another comment does not close a send a crash
//! left open`, `a send lost to a transport failure and refused when sent again is cancelled`,
//! `a send a crash left without an outcome and refused when sent again is cancelled`, `the
//! marker and the refusal of a send name its command id in their cause` and `a send lost to a
//! transport failure that goes in when sent again waits for its run`): there are no markers. A refusal is `send-refused <key>` of
//! `send_once`; the cases that port without a marker are
//! `a_refused_comment_followed_by_a_sendable_edit_sends_the_edit` and
//! `a_call_whose_every_send_was_refused_sends_nothing_and_tells_the_issue_of_each`.

use foundry_proto::factory::{RunEvent, RunId};
use foundry_proto::ids::{MachineId, SessionId, WorkspaceId};
use foundry_proto::record::{Record, Workspace};
use foundry_proto::settings::SharedSettings;
use foundry_proto::state::{SessionState, TerminalState};
use serde_json::Value;

use super::progress::{
    BODY_EDIT_DROPPED, BODY_EDIT_FORWARDED, COMMENT_DROPPED, Fixes, STUCK_DETAIL, STUCK_STOP, Seen,
    pending_relaunch, read_progress,
};
use super::resume::{BRANCH_REFUSED, UNREACHABLE_FOR};
use super::start::{FIRST_PROMPT_COUNT, SETUP_FAILED};
use super::{
    ActionPayload, Closing, ForwardPayload, Progress, RunContext, RunError, Stop, StuckStop,
    abandon, apply_actions, closed_issue, end_own_merge, end_run, go_stuck, moved_on_by_closing,
    pending_send, progress_from, recorded_reason, session_id, workspace_id,
};
use crate::factory::github::read::{
    Activity, BodyEdit, Comment, DropReason, Feedback as Item, FeedbackKind as ItemKind,
    InlineComment as ItemInline, is_safe_branch_name,
};
use crate::factory::github::{OtherKind, TrackerError};
use crate::factory::ledger::events::{
    Cause, CauseStep, Event, ModelOption, OptionValue, ResumedFrom, names,
};
use crate::factory::lifecycle::approval::TextCheck;
use crate::factory::lifecycle::{Event as LifecycleEvent, State, transition};
use crate::factory::messages::{
    self, CANNOT_CONFIRM, CANNOT_VERIFY_DURING_BUILD, CHANGED_DURING_BUILD, CommentDropReason,
    LAUNCH_FAILED, Message, prepare_output_tail,
};
use crate::factory::placing::{self, Reserved, Slots};
use crate::factory::text;
use crate::factory::worker::{CreateWorkspace, WorkerError};
use crate::sessions::factory::{BODY_PATH, FactoryStart, SendRefused};
use crate::workspaces::{Checkout, FactoryTag, Setup};

/// Why a round that has feedback does not start when every run slot is held.
pub const NO_SLOT: &str = "no run slot is free";
/// Why a round does not start when placement gives no reason of its own.
pub const NO_MACHINE: &str = "no machine can take the round";

/// The step written for every read of the feedback: `feedback-read <generation> <ms>`, the
/// generation the lease was held at and the sleep-counting clock's reading (milliseconds) at
/// the read, with ` clean` after it when the read found nothing new.
pub const READ: &str = "feedback-read ";
/// The step that records the machine the round's workspace is made on: `feedback-machine <n> <id>`.
pub const MACHINE: &str = "feedback-machine ";
/// The step written when the machine recorded for the round was unreachable for 15 minutes
/// before the session started: `feedback-machine-given-up <n> <id>`.
pub const MACHINE_GIVEN_UP: &str = "feedback-machine-given-up ";
/// The step written before the earlier workspace's archive is asked for:
/// `feedback-archiving <n> <workspace id>`.
pub const ARCHIVING: &str = "feedback-archiving ";
/// The step written once the archive was asked for: `feedback-archive-asked <n>`.
pub const ARCHIVE_ASKED: &str = "feedback-archive-asked ";
/// The step written before the create: `feedback-requested <n> <name>`.
pub const REQUESTED: &str = "feedback-requested ";
/// The step written once the worker answered the create: `feedback-workspace <n> <id>`.
pub const WORKSPACE: &str = "feedback-workspace ";
/// The step written before the sessions of the round's workspace are ended ahead of a second
/// start: `feedback-ending-sessions <n>`.
pub const ENDING_SESSIONS: &str = "feedback-ending-sessions ";
/// The step written before the session is started: `feedback-sending <n>`.
pub const SENDING: &str = "feedback-sending ";
/// The step written before the first message is asked for again: `feedback-resending <n> <count>`.
pub const RESENDING: &str = "feedback-resending ";

/// How far before the start of the last clean read the next read pages back. GitHub's clock can
/// differ from ours and a read can lag behind a write, so a review submitted just before that
/// start may not have been there to read.
const CLEAN_READ_MARGIN_MS: u64 = 60_000;

/// The codes of the stuck paths this step begins once a round is decided.
const STUCK_SETUP: &str = "feedback-setup-failed";
const STUCK_LAUNCH: &str = "feedback-launch-failed";
const STUCK_TEXT: &str = "text-changed-during-build";
const STUCK_UNVERIFIED: &str = "edits-unverifiable";
const STUCK_UNCONFIRMED: &str = "approval-unconfirmed";
const STUCK_BRANCH: &str = "launch-failed";

/// What a pass of [`advance`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeedbackStep {
    /// Nothing to do: the run is not waiting in `pr-open` (finished, stuck, stopped, no session
    /// or pull request, a re-gate under way), no read is due, or no instructor wrote anything
    /// new. Nothing was written or asked of a worker.
    None,
    /// New feedback is waiting and no round can start now: no run slot is free, or placement
    /// has no machine. Nothing was written, and the issue stays `pr-open`.
    Waiting { reason: String },
    /// The round was decided: the decision is recorded, the label is `building` and the
    /// earlier workspace's archive was asked for if one shows.
    Decided { round: u32, pr: u64, items: usize },
    /// A round under way waits for something.
    Preparing(Wait),
    /// The round's session runs.
    Started { round: u32, session: SessionId },
    /// The first message was asked for again by count.
    Resent { session: SessionId },
    /// The run went stuck; `reason` is as the ledger records it.
    Stuck { reason: String },
    /// The pull request was merged or closed: the run ended.
    Ended,
    /// A person closed the issue: the run ended.
    Abandoned,
}

/// What a round under way waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// The earlier workspace still shows.
    Archive,
    /// The create was answered and the fleet does not show the record yet, or a machine was
    /// given up on and another recorded.
    Workspace,
    /// The setup terminal runs, or its exit is not known.
    Setup,
}

/// What a pass of [`forward`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardStep {
    /// How many comments went into the session this pass.
    pub comments: usize,
    /// Whether the edited issue body went into the session this pass.
    pub body_edit: bool,
    /// Why the run was left alone, with nothing read, sent or written.
    pub skipped: Option<Skip>,
    /// What there was to send could not be typed now: the session is working or its newest turn
    /// is not read yet. Nothing was written for it.
    pub held_back: bool,
}

/// Why [`forward`] left a run alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// The run is of another repo.
    OtherRepo,
    Finished,
    /// A relaunch, an interruption or a rate limit is decided: another step carries it out.
    Relaunching,
    /// The run has no events, or no session recorded.
    NoSession,
    /// The run is not building, or a stop, a stuck path, questions or a gate pass are recorded.
    NotBuilding,
    /// A gate failure was being sent into the session.
    GateSendPending,
    /// A person closed the issue: the run is ended.
    Abandoned,
    /// The run's own merged pull request closed the issue.
    PrMerged,
    /// The fleet shows no such session.
    SessionGone,
}

/// The line of a forwarded instructor comment: the text is in the file it names.
pub const FORWARD_COMMENT_LINE: &str = "The factory forwards an instructor's comment. Read {body}";
/// The line of a forwarded edit of the issue body: the text is in the file it names.
pub const FORWARD_BODY_LINE: &str = "The factory forwards an edit of the issue body. Read {body}";

/// The instructors' new comments and body edit sent to a building run's session. See the
/// module's words.
pub fn forward(ctx: &RunContext<'_>, run: &RunId) -> Result<ForwardStep, RunError> {
    let skip = |why| {
        Ok(ForwardStep {
            comments: 0,
            body_edit: false,
            skipped: Some(why),
            held_back: false,
        })
    };
    if !run.repo().eq_ignore_ascii_case(ctx.repo()) {
        return skip(Skip::OtherRepo);
    }
    let events = ctx.ledger.read(run)?;
    let seen = read_progress(&events);
    if seen.finished {
        return skip(Skip::Finished);
    }
    let decided = seen.interruption.as_ref().is_some_and(|interruption| {
        interruption.relaunch || interruption.rate_limited || !interruption.sent
    }) || pending_relaunch(&seen).is_some();
    if decided {
        return skip(Skip::Relaunching);
    }
    let (Some(thread), Some(first)) = (&seen.thread_id, events.first()) else {
        return skip(Skip::NoSession);
    };
    // `state_changed` is the commit marker, so a stuck decision or posted questions without it
    // are a transition that was cut short. A recorded gate pass is not interrupted.
    if seen.state != Some(State::Building)
        || seen.pass.is_some()
        || seen.stuck.is_some()
        || seen.stuck_stop.is_some()
        || seen.stop_requested
        || seen.questions_posted
    {
        return skip(Skip::NotBuilding);
    }
    // The gate infers that a send-back reached the session from what the session did after it,
    // so a message waits until the gate has sent the failure and recorded it.
    if seen
        .sending
        .is_some_and(|sending| !seen.sent_back_attempts.contains(&sending.attempt))
    {
        return skip(Skip::GateSendPending);
    }
    // Only a run that would read the issue's activity asks whether the issue is closed. A run
    // whose own merge closed the issue gets nothing more sent.
    match closed_issue(ctx, run, &seen)?.0 {
        Closing::Open => {}
        Closing::PersonsMove => {
            abandon(ctx, run, &seen, CauseStep::Forward)?;
            return skip(Skip::Abandoned);
        }
        Closing::OwnMerge(_) => return skip(Skip::PrMerged),
    }
    let issue = run.issue();
    let since = format!("{}Z", &crate::factory::ledger::format_ts(first.ms)[..19]);
    let activity =
        ctx.tracker
            .activity_since(ctx.repo(), issue, &since, &ctx.config.instructors)?;
    if let Some(edit) = &activity.body_edit
        && !is_utc_second(&edit.edited_at)
    {
        let shown: String = edit.edited_at.chars().take(40).collect();
        return Err(RunError::Tracker(TrackerError::Other {
            kind: OtherKind::Parse,
            message: format!(
                "`gh api graphql` returned unexpected output: expected the body edit time as YYYY-MM-DDTHH:MM:SSZ, got {shown:?}"
            ),
        }));
    }
    let (comments, body_edit) = unsent(&seen, &activity);
    let drops = unreported(&seen, &activity);
    let notices = |refused: Vec<Notice>| -> Vec<Notice> {
        refused
            .into_iter()
            .chain(drops.iter().map(|dropped| drop_notice(ctx, issue, dropped)))
            .collect()
    };
    let done = |comments, body_edit, held_back| ForwardStep {
        comments,
        body_edit,
        skipped: None,
        held_back,
    };
    if comments.is_empty() && body_edit.is_none() {
        tell_issue(ctx, run, &notices(Vec::new()))?;
        return Ok(done(0, false, false));
    }
    // Something is to be sent: to a session the fleet shows, and only when it can take a
    // message now.
    let session = session_id(thread)?;
    if ctx.view.session_state(session).is_none() {
        return skip(Skip::SessionGone);
    }
    if cannot_take_a_message(ctx, &seen, session) {
        tell_issue(ctx, run, &notices(Vec::new()))?;
        return Ok(done(0, false, true));
    }
    let mut at = progress_from(run, CauseStep::Forward, &seen, State::Building);
    let mut refused: Vec<Notice> = Vec::new();
    let (mut sent, mut edit_sent, mut held_back) = (0, false, false);
    // The comments in order, then the edit; but a message whose send may have been typed and is
    // not known typed goes first, since no other goes before it is known.
    let mut queue: Vec<Outgoing<'_>> = comments.iter().map(Outgoing::Comment).collect();
    queue.extend(body_edit.as_ref().map(Outgoing::Edit));
    if let Some(pending) = pending_send(&seen, "")
        && let Some(at) = queue.iter().position(|item| item.key() == pending)
    {
        let item = queue.remove(at);
        queue.insert(0, item);
    }
    // One message goes in per pass: the count of prompts the next send is guarded by is read
    // from the fleet, which shows it a pass later.
    for item in queue {
        let (actions, payload, cause) = match item {
            Outgoing::Comment(comment) => (
                transition(
                    State::Building,
                    &LifecycleEvent::InstructorCommented {
                        comment_id: comment.id.clone(),
                    },
                )?
                .1,
                ActionPayload {
                    forward: Some(ForwardPayload {
                        message: Message::new(
                            FORWARD_COMMENT_LINE.into(),
                            messages::comment_text(
                                ctx.repo(),
                                issue,
                                &comment.author,
                                &comment.body,
                            ),
                        ),
                        step: None,
                    }),
                    ..ActionPayload::default()
                },
                Cause::new(CauseStep::Forward, "comment-forwarded"),
            ),
            Outgoing::Edit(edit) => (
                transition(State::Building, &LifecycleEvent::BodyEdited)?.1,
                ActionPayload {
                    forward: Some(ForwardPayload {
                        message: Message::new(
                            FORWARD_BODY_LINE.into(),
                            messages::body_edit_text(ctx.repo(), issue, &edit.editor, &edit.body),
                        ),
                        step: Some(format!("{BODY_EDIT_FORWARDED} {}", edit.edited_at)),
                    }),
                    ..ActionPayload::default()
                },
                Cause::new(CauseStep::Forward, "body-edit-forwarded"),
            ),
        };
        match send_outcome(apply_actions(
            ctx, issue, &mut at, &actions, &cause, &payload,
        ))? {
            Sent::Yes => {
                match item {
                    Outgoing::Comment(_) => sent = 1,
                    Outgoing::Edit(_) => edit_sent = true,
                }
                break;
            }
            Sent::Refused => refused.push(match item {
                Outgoing::Comment(comment) => drop_notice(
                    ctx,
                    issue,
                    &Dropped {
                        id: comment.id.clone(),
                        reason: None,
                    },
                ),
                Outgoing::Edit(edit) => Notice {
                    body: messages::REFUSED_BODY_EDIT.into(),
                    step: format!("{BODY_EDIT_DROPPED} {}", edit.edited_at),
                    reason: "body-edit-dropped",
                },
            }),
            Sent::Later => {
                held_back = true;
                break;
            }
        }
    }
    // Last: telling the issue must not hold back what the session needs.
    tell_issue(ctx, run, &notices(refused))?;
    Ok(done(sent, edit_sent, held_back))
}

/// A message that is to go into the session.
#[derive(Clone, Copy)]
enum Outgoing<'a> {
    Comment(&'a Comment),
    Edit(&'a BodyEdit),
}

impl Outgoing<'_> {
    /// The key `send_once` knows the message by.
    fn key(&self) -> String {
        match self {
            Outgoing::Comment(comment) => format!("forward-comment {}", comment.id),
            Outgoing::Edit(edit) => {
                format!("forward-body-edit {BODY_EDIT_FORWARDED} {}", edit.edited_at)
            }
        }
    }
}

/// What came of one try to send.
enum Sent {
    Yes,
    /// Refused for good: the same message is refused again.
    Refused,
    /// Not typed now, and nothing says it never will be: tried again next pass.
    Later,
}

/// A send's answer. A refusal that typed nothing and will be refused again is final; one that
/// says the session is not ready, or that a message sent before may have been typed, waits for
/// the next pass. Anything else, a send that may have been typed included, is an error.
fn send_outcome(result: Result<(), RunError>) -> Result<Sent, RunError> {
    match result {
        Ok(()) => Ok(Sent::Yes),
        Err(RunError::Send(SendRefused::BodyWithoutPath | SendRefused::SecretsUnreadable)) => {
            Ok(Sent::Refused)
        }
        Err(
            RunError::Send(
                SendRefused::NotIdle | SendRefused::CountRose { .. } | SendRefused::NoSuchSession,
            )
            | RunError::SendPending { .. }
            | RunError::NoPromptCount { .. },
        ) => Ok(Sent::Later),
        Err(error) => Err(error),
    }
}

/// Whether the session cannot be typed into now: it is not idle, it shows no turn to count
/// prompts by, a question waits for a person, or its newest turn is not read yet (a message
/// typed now would start a turn that hides it, and `building` acts on a turn once).
fn cannot_take_a_message(ctx: &RunContext<'_>, seen: &Seen, session: SessionId) -> bool {
    if ctx.view.session_state(session) != Some(SessionState::Idle) {
        return true;
    }
    match ctx.view.session_turn(session) {
        None => true,
        Some(turn) => {
            turn.question || (turn.turn > 0 && seen.turn_read.is_none_or(|read| turn.turn > read))
        }
    }
}

/// Whether `text` is a time as GitHub writes it: `YYYY-MM-DDTHH:MM:SSZ`.
fn is_utc_second(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 20
        && bytes.iter().enumerate().all(|(at, byte)| match at {
            4 | 7 => *byte == b'-',
            10 => *byte == b'T',
            13 | 16 => *byte == b':',
            19 => *byte == b'Z',
            _ => byte.is_ascii_digit(),
        })
}

/// What `forward` still has to send from one read of the instructors' activity: comments with
/// no `comment_forwarded` in the ledger, oldest first and each id once, and the body edit whose
/// time has no `body-edit-forwarded` step. A comment already dropped or sent as feedback is
/// never sent later.
fn unsent(seen: &Seen, activity: &Activity) -> (Vec<Comment>, Option<BodyEdit>) {
    let mut told: std::collections::BTreeSet<&str> = seen
        .forwarded
        .iter()
        .chain(&seen.dropped)
        .map(String::as_str)
        .collect();
    let comments = activity
        .comments
        .iter()
        .filter(|comment| {
            // Nor one that went into the session as instructor feedback.
            if told.contains(comment.id.as_str())
                || seen
                    .feedback_sent
                    .contains(&issue_comment_feedback_id(&comment.id))
            {
                return false;
            }
            told.insert(comment.id.as_str());
            true
        })
        .cloned()
        .collect();
    let body_edit = activity
        .body_edit
        .clone()
        .filter(|edit| !seen.body_edits.contains(&edit.edited_at));
    (comments, body_edit)
}

/// The dropped comments the issue was not told about yet, in the order read, each id once. A
/// comment already forwarded, or sent as instructor feedback, is left out: the session has the
/// instructor's own words, and a later edit by someone else changes nothing it holds.
fn unreported(seen: &Seen, activity: &Activity) -> Vec<Dropped> {
    let mut told: std::collections::BTreeSet<&str> =
        seen.dropped.iter().map(String::as_str).collect();
    activity
        .dropped
        .iter()
        .filter(|dropped| {
            if seen.forwarded.contains(&dropped.id)
                || seen
                    .feedback_sent
                    .contains(&issue_comment_feedback_id(&dropped.id))
                || told.contains(dropped.id.as_str())
            {
                return false;
            }
            told.insert(dropped.id.as_str());
            true
        })
        .map(|dropped| Dropped {
            id: dropped.id.clone(),
            reason: Some(dropped.reason),
        })
        .collect()
}

/// A comment of the factory's for the issue, and the step that records it was posted: the
/// comment first, the step after, so a crash between them posts it again (at-least-once).
struct Notice {
    body: String,
    step: String,
    reason: &'static str,
}

/// A comment that was not forwarded, and why: `None` when the worker refused it.
struct Dropped {
    id: String,
    reason: Option<DropReason>,
}

fn drop_notice(ctx: &RunContext<'_>, issue: u64, dropped: &Dropped) -> Notice {
    let reason = match dropped.reason {
        Some(DropReason::EditedByOther) => CommentDropReason::EditedByOther,
        Some(DropReason::Unverifiable) => CommentDropReason::Unverifiable,
        None => CommentDropReason::Refused,
    };
    Notice {
        body: messages::dropped_comment(ctx.repo(), issue, &dropped.id, reason),
        step: format!("{COMMENT_DROPPED} {}", dropped.id),
        reason: "comment-dropped",
    }
}

/// Posts each notice and records it. A failure stops the call and the rest is said on the next
/// one.
fn tell_issue(ctx: &RunContext<'_>, run: &RunId, notices: &[Notice]) -> Result<(), RunError> {
    for notice in notices {
        ctx.post_comment(run.issue(), &notice.body)?;
        note(ctx, run, &notice.step, notice.reason)?;
    }
    Ok(())
}

fn canonical(text: &str) -> Option<u64> {
    let plain = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    if plain { text.parse().ok() } else { None }
}

/// The sleep-counting clock's reading, in milliseconds.
fn clock_ms(ctx: &RunContext<'_>) -> u64 {
    u64::try_from(ctx.clock.now().as_millis()).unwrap_or(u64::MAX)
}

/// One second before `ms`, as GitHub is asked for it: `YYYY-MM-DDTHH:MM:SSZ`.
fn second_before(ms: u64) -> String {
    let text = crate::factory::ledger::format_ts(ms.saturating_sub(1_000));
    format!("{}Z", &text[..19])
}

/// The text of a `step_changed` event.
fn step_text(event: &RunEvent) -> Option<&str> {
    if event.event != names::STEP_CHANGED {
        return None;
    }
    event.data.get("step").and_then(Value::as_str)
}

/// What follows `prefix` and the count `n` in `step`, or `None` for a step of another kind or
/// another round.
fn numbered<'a>(step: &'a str, prefix: &str, n: u32) -> Option<&'a str> {
    let rest = step.strip_prefix(prefix)?;
    let (count, tail) = rest.split_once(' ').unwrap_or((rest, ""));
    (count == n.to_string()).then_some(tail)
}

fn parse_machine(text: &str) -> Option<MachineId> {
    uuid::Uuid::parse_str(text).ok().map(MachineId)
}

/// The name of round `n`'s workspace: `fac/<N>-<attempt>-f<n>`.
fn round_name(run: &RunId, n: u32) -> String {
    format!("fac/{}-{}-f{n}", run.issue(), run.attempt())
}

/// The id feedback on the issue carries, so that it cannot collide with a review's or a pull
/// request comment's.
fn issue_comment_feedback_id(id: &str) -> String {
    format!("issue-comment:{id}")
}

// ----- what the ledger says of the reads -----

/// The reads of the feedback a ledger records.
#[derive(Debug, Default)]
struct ReadLog {
    /// The lease generation and clock reading of the newest read.
    last: Option<(u64, u64)>,
    /// The wall time (the event's) of the newest clean read.
    clean: Option<u64>,
}

impl ReadLog {
    fn read(events: &[RunEvent]) -> Self {
        let mut log = ReadLog::default();
        for event in events {
            let Some(rest) = step_text(event).and_then(|step| step.strip_prefix(READ)) else {
                continue;
            };
            let parts: Vec<&str> = rest.split(' ').collect();
            let (generation, at, clean) = match parts.as_slice() {
                [generation, at] => (generation, at, false),
                [generation, at, "clean"] => (generation, at, true),
                _ => continue,
            };
            let (Some(generation), Some(at)) = (canonical(generation), canonical(at)) else {
                continue;
            };
            log.last = Some((generation, at));
            if clean {
                log.clean = Some(event.ms);
            }
        }
        log
    }

    /// Whether the feedback is to be read now. A read of another generation (a takeover), or of
    /// a clock reading above this one (a clock that was reset), leaves it due, never later.
    fn due(&self, generation: u64, now: u64, seconds: u64) -> bool {
        match self.last {
            Some((from, at)) if from == generation && now >= at => {
                now - at >= seconds.saturating_mul(1000)
            }
            _ => true,
        }
    }
}

// ----- what the ledger says of a round -----

/// What the ledger records of the newest round.
#[derive(Debug, Default)]
struct Round {
    n: u32,
    branch: String,
    items: Vec<String>,
    /// The body of the first message.
    message: String,
    machine: Option<MachineId>,
    gave_up: Vec<MachineId>,
    /// The workspace the archive is of, as written: an id.
    archiving: Option<String>,
    archive_asked: bool,
    requested: bool,
    workspace: Option<WorkspaceId>,
    sending: bool,
    /// The sessions launched since the decision.
    launches: usize,
}

impl Round {
    fn launched(&self) -> bool {
        self.launches > 0
    }

    fn read(events: &[RunEvent]) -> Option<Round> {
        let number = |event: &RunEvent| {
            (event.event == "feedback_relaunching")
                .then(|| event.data.get("n").and_then(Value::as_u64))
                .flatten()
                .and_then(|n| u32::try_from(n).ok())
        };
        let n = events.iter().filter_map(number).max()?;
        let at = events.iter().position(|event| number(event) == Some(n))?;
        let data = &events[at].data;
        let mut round = Round {
            n,
            branch: data.get("branch").and_then(Value::as_str)?.to_owned(),
            items: data
                .get("items")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            message: data.get("message").and_then(Value::as_str)?.to_owned(),
            ..Round::default()
        };
        for event in &events[at + 1..] {
            if event.event == names::SESSION_LAUNCHED {
                round.launches += 1;
                continue;
            }
            let Some(step) = step_text(event) else {
                continue;
            };
            if let Some(rest) = numbered(step, ARCHIVING, n) {
                round.archiving = Some(rest.to_owned());
            } else if numbered(step, ARCHIVE_ASKED, n).is_some() {
                round.archive_asked = true;
            } else if let Some(rest) = numbered(step, MACHINE_GIVEN_UP, n) {
                round.gave_up.extend(parse_machine(rest));
                round.machine = None;
                round.requested = false;
                round.workspace = None;
                round.sending = false;
            } else if let Some(rest) = numbered(step, MACHINE, n) {
                round.machine = parse_machine(rest);
                round.requested = false;
                round.workspace = None;
                round.sending = false;
            } else if numbered(step, REQUESTED, n).is_some() {
                round.requested = true;
            } else if let Some(rest) = numbered(step, WORKSPACE, n) {
                round.workspace = uuid::Uuid::parse_str(rest).ok().map(WorkspaceId);
            } else if numbered(step, SENDING, n).is_some()
                || numbered(step, ENDING_SESSIONS, n).is_some()
            {
                round.sending = true;
            }
        }
        Some(round)
    }
}

/// Every workspace of the project the fleet shows that is not archived and has one of the run's
/// build, resume or feedback names.
fn live_workspace<'a>(
    ctx: &'a RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    id: WorkspaceId,
) -> Option<&'a Record<Workspace>> {
    let highest = |name: &str, field: &str| -> u32 {
        events
            .iter()
            .filter(|event| event.event == name)
            .filter_map(|event| event.data.get(field).and_then(Value::as_u64))
            .max()
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0)
    };
    let base = format!("fac/{}-{}", run.issue(), run.attempt());
    let mut names = vec![base.clone()];
    names.extend(
        (1..=highest("session_interrupted", "resumes") + 1).map(|k| format!("{base}-r{k}")),
    );
    names.extend((1..=highest("feedback_relaunching", "n") + 1).map(|k| format!("{base}-f{k}")));
    names
        .iter()
        .flat_map(|name| ctx.view.workspaces_named(ctx.project.id, name))
        .find(|record| record.body.id == id)
}

fn unreachable_long(ctx: &RunContext<'_>, machine: MachineId) -> bool {
    ctx.view.unreachable_for(machine) >= UNREACHABLE_FOR
}

/// A step recorded, with the lease checked first so that a lost lease writes nothing.
fn note(ctx: &RunContext<'_>, run: &RunId, step: &str, reason: &str) -> Result<(), RunError> {
    ctx.held()?;
    ctx.record_step(run, step, &Cause::new(CauseStep::Forward, reason))
}

/// `seen` for a stop of the round: what is ended is the round's workspace once it has one,
/// and nothing before. The earlier build session ended at the gate.
fn round_seen(seen: &Seen, round: Option<&Round>) -> Seen {
    let mut stopping = seen.clone();
    stopping.workspace = None;
    match round.and_then(|round| round.workspace) {
        Some(id) => stopping.worktree = Some(id.to_string()),
        None => {
            stopping.worktree = None;
            stopping.thread_id = None;
            stopping.launch_sent = false;
        }
    }
    stopping
}

/// One pass of the feedback step over `run`. See the module's words.
pub fn advance(
    ctx: &RunContext<'_>,
    run: &RunId,
    shared: &SharedSettings,
    slots: &mut Slots,
    reserved: &mut Reserved,
) -> Result<FeedbackStep, RunError> {
    if run.repo() != ctx.repo() {
        return Ok(FeedbackStep::None);
    }
    let events = ctx.ledger.read(run)?;
    let seen = read_progress(&events);
    if seen.finished {
        return Ok(FeedbackStep::None);
    }
    let (Some(_), Some(last)) = (&seen.thread_id, &seen.last_pr) else {
        return Ok(FeedbackStep::None);
    };
    let last = last.clone();
    let round = Round::read(&events);
    if let Some(step) = finish_stuck(ctx, run, &events, &seen, round.as_ref())? {
        return Ok(step);
    }
    if seen.stuck.is_some()
        || seen.stuck_stop.is_some()
        || seen.stop_requested
        || seen.state == Some(State::Stuck)
    {
        return Ok(FeedbackStep::None);
    }
    if let Some(round) = &round {
        if !round.launched() {
            return carry_on(
                ctx, run, shared, slots, reserved, &events, &seen, round, &last,
            );
        }
        if round.launches == 1 && seen.state == Some(State::Building) {
            return resend(ctx, run, &seen, round, &last);
        }
    }
    if seen.state != Some(State::PrOpen) {
        return Ok(FeedbackStep::None);
    }
    // A re-gate is the gate's, and the feedback waits for it to settle.
    let regate = seen
        .relaunch
        .as_ref()
        .is_some_and(|relaunch| matches!(relaunch.fixes, Fixes::Regate { .. }));
    if seen.regating || regate {
        return Ok(FeedbackStep::None);
    }
    let Some(since_ms) = seen.pr_open_since else {
        return Ok(FeedbackStep::None);
    };
    read_and_decide(
        ctx, run, shared, slots, reserved, &events, &seen, &last, since_ms,
    )
}

/// The read of the feedback, when it is due, and the round it starts.
#[allow(clippy::too_many_arguments)]
fn read_and_decide(
    ctx: &RunContext<'_>,
    run: &RunId,
    shared: &SharedSettings,
    slots: &mut Slots,
    reserved: &mut Reserved,
    events: &[RunEvent],
    seen: &Seen,
    last: &super::progress::PullRequest,
    since_ms: u64,
) -> Result<FeedbackStep, RunError> {
    let generation = ctx.held()?;
    let now = clock_ms(ctx);
    let log = ReadLog::read(events);
    if !log.due(generation, now, ctx.settings.fleet.feedback_seconds) {
        return Ok(FeedbackStep::None);
    }
    let since = second_before(since_ms);
    let clean_floor = log.clean.unwrap_or(0).saturating_sub(CLEAN_READ_MARGIN_MS);
    let floor = second_before(since_ms.max(clean_floor));
    let items = match new_feedback(ctx, run, seen, last.pr, &since, &floor) {
        Ok(items) => items,
        // A read that fails waits for the next one too.
        Err(error) => {
            let _ = record_read(ctx, run, generation, now, false);
            return Err(error);
        }
    };
    if items.is_empty() {
        record_read(ctx, run, generation, now, true)?;
        return Ok(FeedbackStep::None);
    }
    // Nothing is written for a round that cannot start: the items are read again next pass.
    if slots.runs_free == 0 {
        return Ok(FeedbackStep::Waiting {
            reason: NO_SLOT.into(),
        });
    }
    if let Err(reasons) =
        placing::place_run(ctx.view, shared, &ctx.project.login, &mut reserved.clone())
    {
        return Ok(FeedbackStep::Waiting {
            reason: waiting_reason(&reasons),
        });
    }
    record_read(ctx, run, generation, now, false)?;

    let issue = run.issue();
    let pull = ctx.tracker.pull_request(ctx.repo(), last.pr)?;
    let mut at = progress_from(run, CauseStep::Forward, seen, State::PrOpen);
    // The build session ended at the gate and its workspace was archived.
    at.thread_id = None;
    at.worktree = None;
    let Some(pull) = pull.as_ref().filter(|pull| pull.open) else {
        end_run(ctx, &at, &last.url, pull.as_ref(), CauseStep::Forward)?;
        return Ok(FeedbackStep::Ended);
    };
    let detail = ctx.tracker.issue(ctx.repo(), issue)?;
    if detail.closed {
        abandon(ctx, run, &round_seen(seen, None), CauseStep::Forward)?;
        return Ok(FeedbackStep::Abandoned);
    }
    if let Some((code, stop)) = text_stop(ctx, issue, seen)? {
        return stuck(ctx, run, &mut at, code, stop);
    }
    if !is_safe_branch_name(&pull.head_ref) {
        let stop = Stop {
            reason: LAUNCH_FAILED.into(),
            detail: Some(BRANCH_REFUSED.into()),
            ..Stop::default()
        };
        return stuck(ctx, run, &mut at, STUCK_BRANCH, stop);
    }
    let Ok(machine) = placing::place_run(ctx.view, shared, &ctx.project.login, reserved) else {
        return Ok(FeedbackStep::Waiting {
            reason: NO_MACHINE.into(),
        });
    };
    slots.runs_free = slots.runs_free.saturating_sub(1);
    let n = seen.feedback_relaunches + 1;
    let converted: Vec<messages::Feedback> = items.iter().map(to_message).collect();
    let first = messages::feedback(
        ctx.repo(),
        issue,
        &ctx.project.login,
        &pull.head_ref,
        &last.url,
        &converted,
        BODY_PATH,
    );
    let round = Round {
        n,
        branch: pull.head_ref.clone(),
        items: items.iter().map(|item| item.id.clone()).collect(),
        message: first.body.unwrap_or_default(),
        ..Round::default()
    };
    ctx.held()?;
    record_decision(ctx, run, &round)?;
    note(
        ctx,
        run,
        &format!("{MACHINE}{n} {machine}"),
        "feedback-machine",
    )?;
    let round = Round {
        machine: Some(machine),
        ..round
    };
    decision_phase(ctx, run, events, seen, &round, last, &mut at)?;
    Ok(FeedbackStep::Decided {
        round: n,
        pr: last.pr,
        items: round.items.len(),
    })
}

fn waiting_reason(reasons: &[String]) -> String {
    if reasons.is_empty() {
        NO_MACHINE.into()
    } else {
        reasons.join("; ")
    }
}

/// The step of a read, written once the read is made (or failed): the window of the next one.
fn record_read(
    ctx: &RunContext<'_>,
    run: &RunId,
    generation: u64,
    now: u64,
    clean: bool,
) -> Result<(), RunError> {
    let tail = if clean { " clean" } else { "" };
    note(
        ctx,
        run,
        &format!("{READ}{generation} {now}{tail}"),
        "feedback-read",
    )
}

/// The instructors' feedback new since `since` (when the run reached `pr-open`), less what the
/// ledger already names as relaunched, forwarded or dropped. Oldest first.
fn new_feedback(
    ctx: &RunContext<'_>,
    run: &RunId,
    seen: &Seen,
    pr: u64,
    since: &str,
    floor: &str,
) -> Result<Vec<Item>, RunError> {
    let instructors = &ctx.config.instructors;
    let reviewed = ctx
        .tracker
        .pr_feedback(ctx.repo(), pr, since, floor, instructors)?;
    let activity = ctx
        .tracker
        .activity_since(ctx.repo(), run.issue(), since, instructors)?;
    let from_issue = activity
        .comments
        .into_iter()
        .filter(|comment| {
            !seen.forwarded.contains(&comment.id) && !seen.dropped.contains(&comment.id)
        })
        .map(|comment| Item {
            id: issue_comment_feedback_id(&comment.id),
            author: comment.author,
            kind: ItemKind::IssueComment,
            created_at: comment.created_at,
            body: comment.body,
            inline: vec![],
            inline_unread: 0,
        });
    let mut items: Vec<Item> = Vec::new();
    for item in reviewed.into_iter().chain(from_issue) {
        if !seen.feedback_sent.contains(&item.id) && !items.iter().any(|kept| kept.id == item.id) {
            items.push(item);
        }
    }
    // Stable, so items of the same second keep the order they were read in.
    items.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(items)
}

fn to_message(item: &Item) -> messages::Feedback {
    messages::Feedback {
        author: item.author.clone(),
        kind: match &item.kind {
            ItemKind::Review { state } => messages::FeedbackKind::Review {
                state: state.clone(),
            },
            ItemKind::PrComment => messages::FeedbackKind::PrComment,
            ItemKind::IssueComment => messages::FeedbackKind::IssueComment,
        },
        body: item.body.clone(),
        inline: item
            .inline
            .iter()
            .map(|ItemInline { path, line, body }| messages::InlineComment {
                path: path.clone(),
                line: *line,
                body: body.clone(),
            })
            .collect(),
        inline_unread: usize::try_from(item.inline_unread).unwrap_or(usize::MAX),
    }
}

/// The decision of a round, as a `feedback_relaunching` line.
fn record_decision(ctx: &RunContext<'_>, run: &RunId, round: &Round) -> Result<(), RunError> {
    ctx.record(
        run,
        Event::FeedbackRelaunching {
            n: round.n,
            branch: round.branch.clone(),
            items: round.items.clone(),
            message: round.message.clone(),
        },
        &Cause::new(CauseStep::Forward, "instructor-feedback"),
    )
}

/// Whether the issue text was changed since the approval by someone who is not an instructor,
/// or cannot be judged: the code and the stop of the run, or `None` when it stands.
fn text_stop(
    ctx: &RunContext<'_>,
    issue: u64,
    seen: &Seen,
) -> Result<Option<(&'static str, Stop)>, RunError> {
    let Some(approved_at) = &seen.approved_at else {
        return Ok(Some((STUCK_UNCONFIRMED, Stop::new(CANNOT_CONFIRM))));
    };
    Ok(
        match ctx
            .tracker
            .text_check(ctx.repo(), issue, approved_at, &ctx.config.instructors)?
        {
            TextCheck::Unchanged => None,
            TextCheck::Changed => Some((STUCK_TEXT, Stop::new(CHANGED_DURING_BUILD))),
            TextCheck::Unjudged => Some((STUCK_UNVERIFIED, Stop::new(CANNOT_VERIFY_DURING_BUILD))),
        },
    )
}

/// The run goes stuck under `code`, with `stop`.
fn stuck(
    ctx: &RunContext<'_>,
    run: &RunId,
    at: &mut Progress,
    code: &str,
    stop: Stop,
) -> Result<FeedbackStep, RunError> {
    let reason = recorded_reason(&stop);
    go_stuck(ctx, run.issue(), at, code, stop, None)?;
    Ok(FeedbackStep::Stuck { reason })
}

/// The newest decision's detail: the `stuck-detail` step right before its `stuck-stop`.
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

/// A stuck path cut short while a round was being set up, or begun here for a setup or a
/// launch, finished from the ledger with the words it recorded.
fn finish_stuck(
    ctx: &RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    seen: &Seen,
    round: Option<&Round>,
) -> Result<Option<FeedbackStep>, RunError> {
    let Some(raw) = &seen.stuck_stop else {
        return Ok(None);
    };
    if seen.state == Some(State::Stuck) {
        return Ok(None);
    }
    let decision: StuckStop = serde_json::from_str(raw)
        .map_err(|error| RunError::Invalid(format!("the stuck decision of {run}: {error}")))?;
    let owned = round.is_some_and(|round| !round.launched())
        || decision.code == STUCK_SETUP
        || decision.code == STUCK_LAUNCH;
    if !owned {
        return Ok(None);
    }
    let mut stop = decision.stop;
    stop.detail = decision_detail(events, &decision.code);
    let mut at = progress_from(
        run,
        CauseStep::Forward,
        &round_seen(seen, round),
        State::Building,
    );
    if seen.state == Some(State::PrOpen) {
        at.state = State::PrOpen;
    }
    let reason = recorded_reason(&stop);
    go_stuck(ctx, run.issue(), &mut at, &decision.code, stop, None)?;
    Ok(Some(FeedbackStep::Stuck { reason }))
}

// ----- a round under way -----

/// The label and the earlier workspace of a round decided: the machine, the move to
/// `building`, the decision recorded again after it, and the archive of the earlier workspace.
/// Each is recorded before it is done and skipped when recorded. Gives back the step that asked
/// something, or `None` when there is nothing more to do before the workspace.
#[allow(clippy::too_many_arguments)]
fn decision_phase(
    ctx: &RunContext<'_>,
    run: &RunId,
    events: &[RunEvent],
    seen: &Seen,
    round: &Round,
    last: &super::progress::PullRequest,
    at: &mut Progress,
) -> Result<bool, RunError> {
    let issue = run.issue();
    let mut did = false;
    let Some(machine) = round.machine else {
        return Ok(false);
    };
    if seen.state == Some(State::PrOpen) {
        let (_, actions) = transition(
            State::PrOpen,
            &LifecycleEvent::InstructorFeedback { pr: last.pr },
        )?;
        at.state = State::PrOpen;
        at.machine = Some(machine.to_string());
        at.thread_id = None;
        apply_actions(
            ctx,
            issue,
            at,
            &actions,
            &Cause::new(CauseStep::Forward, "instructor-feedback"),
            &ActionPayload::default(),
        )?;
        did = true;
    }
    // `state_changed` ends the decision a `Seen` holds, so it is written again after the move:
    // the round is decided until its session is recorded, which is what `building` and `resume`
    // skip a run for, and the run holds its slot.
    let after = read_progress(&ctx.ledger.read(run)?);
    if pending_relaunch(&after).is_none() {
        ctx.held()?;
        record_decision(ctx, run, round)?;
        did = true;
    }
    // The earlier workspace, when the fleet still shows it, is archived in a pass of its own.
    let old = seen
        .worktree
        .as_deref()
        .map(workspace_id)
        .transpose()?
        .and_then(|id| live_workspace(ctx, run, events, id).map(|record| record.body.id));
    let asked = match (&round.archiving, round.archive_asked, old) {
        (None, _, Some(old)) => Some((old, Records::Both)),
        (Some(named), false, _) => Some((workspace_id(named)?, Records::Asked)),
        _ => None,
    };
    if let Some((old, records)) = asked {
        archive_old(ctx, run, round.n, old, records)?;
        did = true;
    }
    Ok(did)
}

/// What [`archive_old`] records of the ask.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Records {
    Both,
    Asked,
    Nothing,
}

/// The earlier workspace's archive: recorded (as `records` says), then asked for. A machine
/// that cannot be reached is no error: the archive is asked again once it is heard from, or the
/// round goes on when it has been unreachable for 15 minutes.
fn archive_old(
    ctx: &RunContext<'_>,
    run: &RunId,
    n: u32,
    old: WorkspaceId,
    records: Records,
) -> Result<(), RunError> {
    if records == Records::Both {
        note(
            ctx,
            run,
            &format!("{ARCHIVING}{n} {old}"),
            "feedback-archiving",
        )?;
    }
    match ctx.archive(old) {
        Ok(()) | Err(RunError::Worker(WorkerError::Unreachable(_))) => {}
        Err(error) => return Err(error),
    }
    if records != Records::Nothing {
        note(
            ctx,
            run,
            &format!("{ARCHIVE_ASKED}{n}"),
            "feedback-archive-asked",
        )?;
    }
    Ok(())
}

/// The run's base: the one its launch recorded, else the project's.
fn run_base(ctx: &RunContext<'_>, seen: &Seen) -> String {
    seen.base
        .clone()
        .or_else(|| seen.launch_base.clone())
        .unwrap_or_else(|| ctx.config.base.clone())
}

/// The model a round's session starts on: the one the run's launch decided, else the fleet's.
fn model_of(ctx: &RunContext<'_>, seen: &Seen) -> String {
    seen.launch_model
        .clone()
        .unwrap_or_else(|| ctx.settings.fleet.model.clone())
}

/// The first message of round `round`: the launch's line and the body the decision recorded.
fn first_message(
    ctx: &RunContext<'_>,
    run: &RunId,
    round: &Round,
    last: &super::progress::PullRequest,
) -> Message {
    let line = messages::feedback(
        ctx.repo(),
        run.issue(),
        &ctx.project.login,
        &round.branch,
        &last.url,
        &[],
        BODY_PATH,
    )
    .line;
    Message {
        line,
        body: Some(round.message.clone()),
    }
}

/// A round decided and not yet launched: the machine, the label, the earlier workspace's
/// archive, the workspace, its setup and the session, each from where the ledger shows it
/// stopped.
#[allow(clippy::too_many_arguments)]
fn carry_on(
    ctx: &RunContext<'_>,
    run: &RunId,
    shared: &SharedSettings,
    slots: &mut Slots,
    reserved: &mut Reserved,
    events: &[RunEvent],
    seen: &Seen,
    round: &Round,
    last: &super::progress::PullRequest,
) -> Result<FeedbackStep, RunError> {
    let issue = run.issue();
    let n = round.n;
    // A round decided with its label not moved yet holds its slot from now on.
    if seen.state == Some(State::PrOpen) {
        slots.runs_free = slots.runs_free.saturating_sub(1);
    }
    let stopping = round_seen(seen, Some(round));
    let mut at = progress_from(run, CauseStep::Forward, &stopping, State::Building);
    at.state = seen.state.unwrap_or(State::PrOpen);
    // A closed issue is a person's move, or the run's own merge.
    let mut title = None;
    if seen.state == Some(State::Building) {
        let detail = ctx.tracker.issue(ctx.repo(), issue)?;
        match moved_on_by_closing(ctx, seen, detail.closed)? {
            Closing::Open => {}
            Closing::PersonsMove => {
                abandon(ctx, run, &stopping, CauseStep::Forward)?;
                return Ok(FeedbackStep::Abandoned);
            }
            Closing::OwnMerge(pull) => {
                end_own_merge(ctx, run, &stopping, CauseStep::Forward, &pull)?;
                return Ok(FeedbackStep::Ended);
            }
        }
        title = Some(detail.title);
    }
    // The machine: recorded with the decision, or placed again when the pass was cut before it.
    let machine = match round.machine {
        Some(machine) => machine,
        None => {
            let machine = match placing::place_run(ctx.view, shared, &ctx.project.login, reserved) {
                Ok(machine) => machine,
                Err(reasons) => {
                    return Ok(FeedbackStep::Waiting {
                        reason: waiting_reason(&reasons),
                    });
                }
            };
            note(
                ctx,
                run,
                &format!("{MACHINE}{n} {machine}"),
                "feedback-machine",
            )?;
            machine
        }
    };
    let placed = Round {
        machine: Some(machine),
        ..clone_round(round)
    };
    at.machine = Some(machine.to_string());
    if decision_phase(ctx, run, events, seen, &placed, last, &mut at)? {
        return Ok(FeedbackStep::Decided {
            round: n,
            pr: last.pr,
            items: round.items.len(),
        });
    }
    // The earlier workspace is waited for, until it shows archived or its machine is gone.
    if let Some(named) = &round.archiving {
        let old = workspace_id(named)?;
        if let Some(record) = live_workspace(ctx, run, events, old)
            && !unreachable_long(ctx, record.owner)
        {
            if ctx.view.online(record.owner) {
                archive_old(ctx, run, n, old, Records::Nothing)?;
            }
            return Ok(FeedbackStep::Preparing(Wait::Archive));
        }
    }
    // A machine that stays away for 15 minutes before the session started is given up.
    if unreachable_long(ctx, machine) {
        note(
            ctx,
            run,
            &format!("{MACHINE_GIVEN_UP}{n} {machine}"),
            "feedback-machine-given-up",
        )?;
        let next = placing::place_run(ctx.view, shared, &ctx.project.login, reserved);
        return match next {
            Ok(machine) => {
                note(
                    ctx,
                    run,
                    &format!("{MACHINE}{n} {machine}"),
                    "feedback-machine",
                )?;
                Ok(FeedbackStep::Preparing(Wait::Workspace))
            }
            Err(reasons) => Ok(FeedbackStep::Waiting {
                reason: waiting_reason(&reasons),
            }),
        };
    }
    let name = round_name(run, n);
    let named = ctx.view.workspaces_named(ctx.project.id, &name);
    let record = match round.workspace {
        Some(id) => match named.iter().find(|record| record.body.id == id) {
            Some(record) => (*record).clone(),
            // The worker answered, and the fleet does not show the record yet: wait.
            None => return Ok(FeedbackStep::Preparing(Wait::Workspace)),
        },
        None => {
            if !round.requested {
                note(
                    ctx,
                    run,
                    &format!("{REQUESTED}{n} {name}"),
                    "feedback-requested",
                )?;
            }
            // A record the fleet shows with its setup started is the first create's answer.
            let id = match named
                .iter()
                .find(|record| record.owner == machine && record.body.setup.is_some())
            {
                Some(record) => record.body.id,
                None => ctx.create(
                    machine,
                    CreateWorkspace {
                        project: ctx.project.id,
                        name: name.clone(),
                        checkout: Checkout::Branch {
                            name: round.branch.clone(),
                        },
                        setup: Setup::Own,
                        tag: FactoryTag {
                            project: ctx.project.id,
                            run: run.to_string(),
                        },
                    },
                )?,
            };
            note(
                ctx,
                run,
                &format!("{WORKSPACE}{n} {id}"),
                "feedback-workspace",
            )?;
            return Ok(FeedbackStep::Preparing(Wait::Workspace));
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
                return stuck(ctx, run, &mut at, STUCK_SETUP, stop);
            }
            _ => return Ok(FeedbackStep::Preparing(Wait::Setup)),
        }
    }
    let launch = Launch {
        run,
        seen,
        round,
        workspace,
        machine,
        title,
    };
    // A factory session the fleet already shows in the workspace (one with a turn record) for a
    // start that may have gone out: adopted, never started twice.
    if let Some(found) = ctx.view.session_of(workspace)
        && ctx.view.session_turn(found.body.id).is_some()
    {
        return launch.record(ctx, &mut at, found.body.id);
    }
    // Before every start, the text since the approval: a session that would act on text no
    // instructor approved is not started.
    if let Some((code, stop)) = text_stop(ctx, issue, seen)? {
        return stuck(ctx, run, &mut at, code, stop);
    }
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
            &format!("{ENDING_SESSIONS}{n}"),
            "feedback-ending-sessions",
        )?;
        ctx.end_sessions(workspace)?;
    } else {
        note(ctx, run, &format!("{SENDING}{n}"), "feedback-sending")?;
    }
    match ctx.start_session(workspace, start, first_message(ctx, run, round, last)) {
        Ok(session) => launch.record(ctx, &mut at, session),
        // The worker refused: stuck, with the detail in the ledger only. A machine that cannot
        // be reached is no refusal: the next pass starts again.
        Err(RunError::Worker(WorkerError::Failed(what))) => {
            let detail = if round.sending {
                messages::session_may_exist_note(&what, &format!("foundry:{run}:feedback-{n}"))
            } else {
                what
            };
            let stop = Stop {
                reason: LAUNCH_FAILED.into(),
                detail: Some(detail),
                ..Stop::default()
            };
            stuck(ctx, run, &mut at, STUCK_LAUNCH, stop)
        }
        Err(error) => Err(error),
    }
}

fn clone_round(round: &Round) -> Round {
    Round {
        n: round.n,
        branch: round.branch.clone(),
        items: round.items.clone(),
        message: round.message.clone(),
        machine: round.machine,
        gave_up: round.gave_up.clone(),
        archiving: round.archiving.clone(),
        archive_asked: round.archive_asked,
        requested: round.requested,
        workspace: round.workspace,
        sending: round.sending,
        launches: round.launches,
    }
}

/// What a `session_launched` line of a round is written from.
struct Launch<'a> {
    run: &'a RunId,
    seen: &'a Seen,
    round: &'a Round,
    workspace: WorkspaceId,
    machine: MachineId,
    title: Option<String>,
}

impl Launch<'_> {
    /// The `session_launched` line, then the status comment that names the machine and the
    /// session.
    fn record(
        &self,
        ctx: &RunContext<'_>,
        at: &mut Progress,
        session: SessionId,
    ) -> Result<FeedbackStep, RunError> {
        ctx.held()?;
        ctx.record(
            self.run,
            Event::SessionLaunched {
                thread_id: session.to_string(),
                worktree: Some(self.workspace.to_string()),
                branch: Some(self.round.branch.clone()),
                model_instance: None,
                model: Some(model_of(ctx, self.seen)),
                model_options: Some(vec![ModelOption {
                    id: "effort".into(),
                    value: OptionValue::Text(ctx.settings.fleet.effort.clone()),
                }]),
                resumed_from: Some(ResumedFrom {
                    run: self.run.to_string(),
                    branch: self.round.branch.clone(),
                }),
                base: Some(run_base(ctx, self.seen)),
                title: self.title.clone(),
            },
            &Cause::new(CauseStep::Forward, "resumed-from-branch"),
        )?;
        at.thread_id = Some(session.to_string());
        ctx.upsert_status(
            self.run.issue(),
            &messages::status(
                State::Building,
                &self.machine.to_string(),
                Some(&session.to_string()),
                at.pr,
            ),
        )?;
        Ok(FeedbackStep::Started {
            round: self.round.n,
            session,
        })
    }
}

/// A session whose newest launch is a round's, idle with no prompt a pass later: the first
/// message is asked for again by count, with the words the first had.
fn resend(
    ctx: &RunContext<'_>,
    run: &RunId,
    seen: &Seen,
    round: &Round,
    last: &super::progress::PullRequest,
) -> Result<FeedbackStep, RunError> {
    let Some(thread) = &seen.thread_id else {
        return Ok(FeedbackStep::None);
    };
    let session = session_id(thread)?;
    // The launch is the session's first prompt, so its count before the send is none. A session
    // that is idle with no turn ended, or whose turn record shows no prompt, has not taken it.
    let idle = ctx.view.session_state(session) == Some(SessionState::Idle);
    let untouched = ctx
        .view
        .session_turn(session)
        .is_none_or(|turn| turn.turn == 0 && turn.prompts == FIRST_PROMPT_COUNT);
    if !(idle && untouched) {
        return Ok(FeedbackStep::None);
    }
    note(
        ctx,
        run,
        &format!("{RESENDING}{} {FIRST_PROMPT_COUNT}", round.n),
        "feedback-resend",
    )?;
    match ctx.send(
        session,
        first_message(ctx, run, round, last),
        Some(FIRST_PROMPT_COUNT),
    ) {
        Ok(_) => Ok(FeedbackStep::Resent { session }),
        // The worker decides: its count rose, so the message was typed; or the session is not
        // idle yet, and the next pass looks again.
        Err(RunError::Send(SendRefused::CountRose { .. } | SendRefused::NotIdle)) => {
            Ok(FeedbackStep::None)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use foundry_proto::factory::{MachineHeld, RunEvent};
    use foundry_proto::ids::{MachineId, TerminalId, WorkspaceId};
    use foundry_proto::record::{MachineSample, Record, Terminal, Workspace};
    use foundry_proto::settings::{Limits, Machine};
    use foundry_proto::state::{SessionState, TerminalState};
    use serde_json::{Value, json};

    use super::super::progress::read_progress;
    use super::super::testing::*;
    use super::*;
    use crate::factory::github::read::{
        Activity, Comment, Feedback as Item, FeedbackKind as ItemKind, InlineComment as ItemInline,
    };
    use crate::factory::github::{Commenting, OtherKind, Tracker, TrackerError};
    use crate::factory::ledger::events::{Cause, CauseStep, Event, ResumedFrom};
    use crate::factory::lifecycle::State;
    use crate::factory::messages::{self, LAUNCH_FAILED};
    use crate::factory::runs::resume::BRANCH_REFUSED;
    use crate::factory::worker::{Call, CreateWorkspace, FleetView, FleetViewBuilder, WorkerError};
    use crate::sessions::factory::BODY_PATH;
    use crate::workspaces::{Checkout, FactoryTag, Setup};

    const SESSION: &str = "00000000-0000-0000-0000-0000000005e5";
    const OLD: &str = "00000000-0000-0000-0000-000000000a11";
    const BRANCH: &str = "feat/pr-12";
    const URL12: &str = "https://github.com/rawphp/softfac/pull/12";
    const HEAD: &str = "4f1c2d3e4a5b60718293a4b5c6d7e8f901234567";
    const TEN_MINUTES: Duration = Duration::from_secs(600);

    /// The machine the build ran on and the round is placed on.
    fn m1() -> MachineId {
        MachineId(uuid::Uuid::from_u128(0x2))
    }

    fn old() -> WorkspaceId {
        WorkspaceId(uuid::Uuid::from_u128(0xa11))
    }

    /// The round's workspace, as the worker answers the create.
    fn fresh() -> WorkspaceId {
        WorkspaceId(uuid::Uuid::from_u128(0xb22))
    }

    /// The session the round starts.
    fn round_session() -> SessionId {
        SessionId(uuid::Uuid::from_u128(0x5e9))
    }

    fn setup_terminal() -> TerminalId {
        TerminalId(uuid::Uuid::from_u128(0x7e1))
    }

    // ----- GitHub: the simulator, with the reads of a feedback round -----

    /// What the hub answers for the reads the simulator has no query for, and every call it
    /// counts so that the one at an index can be made to fail.
    #[derive(Default)]
    struct HubState {
        feedback: Vec<Item>,
        activity: Option<Activity>,
        reads: Vec<String>,
        counted: usize,
        fail_at: Option<(usize, bool)>,
    }

    /// GitHub as a feedback round reads it: the instructors' reviews and comments that a test
    /// sets, over the simulator for everything else. Every call, the two reads included,
    /// counts, so the `n`-th can fail with its effect kept or lost.
    struct Hub {
        sim: Arc<Sim>,
        state: Mutex<HubState>,
    }

    impl Hub {
        fn new(sim: Arc<Sim>) -> Self {
            Self {
                sim,
                state: Mutex::new(HubState::default()),
            }
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
            self.state.lock().unwrap_or_else(PoisonError::into_inner)
        }

        fn set_feedback(&self, items: Vec<Item>) {
            self.lock().feedback = items;
        }

        fn set_activity(&self, activity: Activity) {
            self.lock().activity = Some(activity);
        }

        /// The two reads of a pass, as one line each.
        fn reads(&self) -> Vec<String> {
            self.lock().reads.clone()
        }

        fn forget_reads(&self) {
            self.lock().reads.clear();
        }

        /// The calls counted so far.
        fn counted(&self) -> usize {
            self.lock().counted
        }

        /// The call at `index` (counted from the start of the hub) fails; with `lose` it takes
        /// effect first, as a timeout after GitHub applied the change does.
        fn fail_at(&self, index: usize, lose: bool) {
            self.lock().fail_at = Some((index, lose));
        }

        fn count<T>(
            &self,
            run: impl FnOnce() -> Result<T, TrackerError>,
        ) -> Result<T, TrackerError> {
            let hit = {
                let mut state = self.lock();
                let at = state.counted;
                state.counted += 1;
                state.fail_at.filter(|(index, _)| *index == at)
            };
            match hit {
                None => run(),
                Some((_, lose)) => {
                    if lose {
                        let _ = run();
                    }
                    Err(TrackerError::Other {
                        kind: OtherKind::Failed,
                        message: "`gh` failed: injected failure".into(),
                    })
                }
            }
        }
    }

    impl Tracker for Hub {
        fn gh(&self, args: &[String]) -> Result<String, TrackerError> {
            self.count(|| self.sim.gh(args))
        }

        fn pr_feedback(
            &self,
            _repo: &str,
            pr: u64,
            since: &str,
            floor: &str,
            instructors: &[String],
        ) -> Result<Vec<Item>, TrackerError> {
            self.lock().reads.push(format!(
                "feedback {pr} {since} {floor} {}",
                instructors.join(",")
            ));
            self.count(|| Ok(self.lock().feedback.clone()))
        }

        fn activity_since(
            &self,
            _repo: &str,
            issue: u64,
            since: &str,
            instructors: &[String],
        ) -> Result<Activity, TrackerError> {
            self.lock().reads.push(format!(
                "activity {issue} {since} {}",
                instructors.join(",")
            ));
            self.count(|| {
                Ok(self.lock().activity.clone().unwrap_or(Activity {
                    comments: vec![],
                    body_edit: None,
                    dropped: vec![],
                }))
            })
        }
    }

    impl Commenting for Hub {
        fn hide(&self, text: &str) -> String {
            text.to_owned()
        }
    }

    // ----- the fleet -----

    fn sample(owner: MachineId, sessions: u32) -> Record<MachineSample> {
        Record {
            owner,
            version: 3,
            deleted: false,
            body: MachineSample {
                load_per_core: 0.1,
                free_memory: 8_000_000_000,
                free_disk: 100_000_000_000,
                sessions,
                update_refused: None,
            },
        }
    }

    /// The machine online, with a sample, a token for the project's login and the skill.
    fn fleet_with(sessions: u32) -> FleetViewBuilder {
        FleetView::builder()
            .online(m1())
            .sample(sample(m1(), sessions))
            .held(
                m1(),
                MachineHeld {
                    logins: vec![LOGIN.into()],
                    skill: true,
                },
            )
    }

    fn fleet() -> FleetViewBuilder {
        fleet_with(0)
    }

    /// The shared settings: the machine named `mac`, with room for `max_sessions` sessions.
    fn shared_with(max_sessions: u32) -> SharedSettings {
        SharedSettings {
            machines: vec![Machine {
                id: m1(),
                name: "mac".into(),
                address: String::new(),
                key: String::new(),
            }],
            limits: [(
                m1(),
                Limits {
                    max_sessions,
                    max_load_per_core: 1.0,
                    min_free_disk: 10_000_000_000,
                },
            )]
            .into(),
            ..SharedSettings::default()
        }
    }

    fn shared() -> SharedSettings {
        shared_with(4)
    }

    fn workspace_record(
        id: WorkspaceId,
        name: &str,
        setup: Option<TerminalId>,
    ) -> Record<Workspace> {
        Record {
            owner: m1(),
            version: 1,
            deleted: false,
            body: Workspace {
                id,
                project: project_id(),
                name: name.into(),
                branch: BRANCH.into(),
                base: "main".into(),
                setup,
            },
        }
    }

    fn terminal_record(state: TerminalState, exit_code: Option<i32>) -> Record<Terminal> {
        Record {
            owner: m1(),
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

    /// The fleet once the round's workspace shows, its setup done.
    fn round_view() -> FleetView {
        fleet()
            .workspace(workspace_record(
                fresh(),
                "fac/8-1-f1",
                Some(setup_terminal()),
            ))
            .terminal(terminal_record(TerminalState::Exited, Some(0)))
            .build()
    }

    // ----- a run in pr-open -----

    /// A run as the gate leaves it: built, passed, the build session ended and its workspace
    /// archived, the issue `pr-open`. `early` are steps written before it reached `pr-open`.
    fn pr_open_after(rig: &Rig, run: &RunId, early: &[&str]) {
        pr_open_on(rig, run, early, "main");
    }

    /// [`pr_open_after`] for a run whose launch recorded the base `base`.
    fn pr_open_on(rig: &Rig, run: &RunId, early: &[&str], base: &str) {
        started(rig, run, base);
        for step in early {
            rig.step(run, step);
        }
        rig.write(
            run,
            Event::PrOpened {
                pr: 12,
                url: URL12.into(),
            },
        );
        passed_gate(rig, run);
        rig.moved(run, State::Building, State::PrOpen);
        rig.sim.set_labels(&["factory:pr-open"]);
        rig.sim.set_assignees(&[LOGIN]);
        rig.sim.set_pull(
            12,
            SimPull {
                state: "OPEN",
                draft: false,
                head: BRANCH.into(),
                base: base.into(),
                cross: false,
            },
        );
    }

    /// A run as `start` leaves it: approved, claimed, its session launched on the build
    /// workspace, `building`.
    fn started(rig: &Rig, run: &RunId, base: &str) {
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
        rig.step(run, "workspace-requested fac/8-1");
        rig.step(run, &format!("workspace-created {OLD}"));
        rig.step(run, "approval-rechecked");
        rig.write(
            run,
            Event::SessionLaunched {
                thread_id: SESSION.into(),
                worktree: Some(OLD.into()),
                branch: Some("fac/8-1".into()),
                model_instance: None,
                model: Some("opus".into()),
                model_options: None,
                resumed_from: None::<ResumedFrom>,
                base: Some(base.into()),
                title: Some("Make it faster".into()),
            },
        );
        rig.moved(run, State::Triage, State::Building);
    }

    /// The gate passed on the pull request's head, and the build session and workspace were
    /// ended and archived.
    fn passed_gate(rig: &Rig, run: &RunId) {
        rig.write(
            run,
            Event::GateCommand {
                attempt: 1,
                command: "cargo test".into(),
                exit_code: 0,
                duration_ms: 1000,
            },
        );
        rig.write(
            run,
            Event::StepChanged {
                step: format!("gate-passed {HEAD} 1"),
            },
        );
        rig.step(run, "build-session-ended");
        rig.step(run, "build-archiving");
        rig.step(run, "build-archived");
        rig.step(run, "gate-results-posted");
    }

    /// Everything a test of a round holds.
    struct World {
        rig: Rig,
        hub: Hub,
        run: RunId,
    }

    fn slots(runs_free: u32) -> Slots {
        Slots {
            runs_free,
            gates_free: 2,
        }
    }

    impl World {
        fn new() -> Self {
            Self::after(&[])
        }

        fn after(early: &[&str]) -> Self {
            let mut rig = Rig::new();
            rig.view = fleet().build();
            let hub = Hub::new(rig.sim.clone());
            let run = run_id(1);
            pr_open_after(&rig, &run, early);
            Self { rig, hub, run }
        }

        /// A world whose pull request has the review of [`review`] on it.
        fn reviewed() -> Self {
            let world = Self::new();
            world
                .hub
                .set_feedback(vec![review(11, "2026-10-04T11:00:00Z")]);
            world
        }

        fn ctx(&self) -> RunContext<'_> {
            RunContext {
                tracker: &self.hub,
                worker: &self.rig.worker,
                view: &self.rig.view,
                ledger: &self.rig.ledger,
                guard: self.rig.guard.as_ref(),
                project: &self.rig.project,
                config: &self.rig.config,
                settings: &self.rig.settings,
                clock: &self.rig.clock,
            }
        }

        fn try_pass(
            &self,
            slots: &mut Slots,
            reserved: &mut Reserved,
        ) -> Result<FeedbackStep, RunError> {
            advance(&self.ctx(), &self.run, &shared(), slots, reserved)
        }

        fn try_go(&self) -> Result<FeedbackStep, RunError> {
            self.try_pass(&mut slots(1), &mut Reserved::default())
        }

        fn go(&self) -> FeedbackStep {
            self.try_go().unwrap()
        }

        fn events(&self) -> Vec<RunEvent> {
            self.rig.events(&self.run)
        }

        fn count(&self) -> usize {
            self.events().len()
        }

        fn seen(&self) -> super::super::progress::Seen {
            read_progress(&self.events())
        }

        fn steps(&self) -> Vec<String> {
            self.rig.steps(&self.run)
        }

        fn steps_of(&self, prefix: &str) -> Vec<String> {
            self.steps()
                .into_iter()
                .filter(|step| step.starts_with(prefix))
                .collect()
        }

        /// The `feedback_relaunching` lines, as their data.
        fn decisions(&self) -> Vec<Value> {
            self.events()
                .into_iter()
                .filter(|event| event.event == "feedback_relaunching")
                .map(|event| event.data)
                .collect()
        }

        fn launches(&self) -> Vec<Value> {
            self.events()
                .into_iter()
                .filter(|event| event.event == "session_launched")
                .map(|event| event.data)
                .collect()
        }

        fn calls(&self, wanted: impl Fn(&Call) -> bool) -> Vec<Call> {
            self.rig
                .worker
                .calls()
                .into_iter()
                .filter(|call| wanted(call))
                .collect()
        }

        fn creates(&self) -> Vec<Call> {
            self.calls(|call| matches!(call, Call::Create { .. }))
        }

        fn starts(&self) -> Vec<Call> {
            self.calls(|call| matches!(call, Call::StartSession { .. }))
        }

        fn archives(&self) -> Vec<Call> {
            self.calls(|call| matches!(call, Call::Archive { .. }))
        }

        fn sends(&self) -> Vec<Call> {
            self.calls(|call| matches!(call, Call::Send { .. }))
        }

        /// Time passes on the sleep-counting clock.
        fn wait(&self, by: Duration) {
            self.rig.clock.advance(by);
        }

        /// Pads the ledger so that the wall clock has moved on by `n` seconds.
        fn pad(&self, n: usize) {
            for _ in 0..n {
                self.rig.step(&self.run, "padding");
            }
        }

        /// The run's own writes on GitHub, as one line each.
        fn writes(&self) -> Vec<String> {
            self.rig.sim.writes()
        }

        /// The round up to its session: decided, the workspace asked for, its record shown,
        /// the session started.
        fn round(&mut self) -> SessionId {
            assert!(matches!(self.go(), FeedbackStep::Decided { .. }));
            self.rig.worker.on_create(Ok(fresh()));
            assert_eq!(self.go(), FeedbackStep::Preparing(Wait::Workspace));
            self.rig.view = round_view();
            self.rig.worker.on_start_session(Ok(round_session()));
            assert_eq!(
                self.go(),
                FeedbackStep::Started {
                    round: 1,
                    session: round_session()
                }
            );
            round_session()
        }
    }

    fn item(id: &str, kind: ItemKind, created_at: &str, body: &str) -> Item {
        Item {
            id: id.into(),
            author: "tom".into(),
            kind,
            created_at: created_at.into(),
            body: body.into(),
            inline: vec![],
            inline_unread: 0,
        }
    }

    /// A review by an instructor that asks for changes, with an inline comment.
    fn review(id: u64, created_at: &str) -> Item {
        Item {
            inline: vec![ItemInline {
                path: "src/a.rs".into(),
                line: Some(7),
                body: "use a better name".into(),
            }],
            ..item(
                &format!("review:{id}"),
                ItemKind::Review {
                    state: "CHANGES_REQUESTED".into(),
                },
                created_at,
                "Please rename it.",
            )
        }
    }

    fn pr_comment(id: u64, created_at: &str, body: &str) -> Item {
        item(
            &format!("pr-comment:{id}"),
            ItemKind::PrComment,
            created_at,
            body,
        )
    }

    fn issue_comment(id: &str, created_at: &str, body: &str) -> Comment {
        Comment {
            id: id.into(),
            author: "tom".into(),
            created_at: created_at.into(),
            body: body.into(),
        }
    }

    fn activity(comments: Vec<Comment>) -> Activity {
        Activity {
            comments,
            body_edit: None,
            dropped: vec![],
        }
    }

    /// The feedback as the first message carries it: the messages crate's own type.
    fn block(items: &[Item]) -> messages::Message {
        let items: Vec<messages::Feedback> = items
            .iter()
            .map(|item| messages::Feedback {
                author: item.author.clone(),
                kind: match &item.kind {
                    ItemKind::Review { state } => messages::FeedbackKind::Review {
                        state: state.clone(),
                    },
                    ItemKind::PrComment => messages::FeedbackKind::PrComment,
                    ItemKind::IssueComment => messages::FeedbackKind::IssueComment,
                },
                body: item.body.clone(),
                inline: item
                    .inline
                    .iter()
                    .map(|inline| messages::InlineComment {
                        path: inline.path.clone(),
                        line: inline.line,
                        body: inline.body.clone(),
                    })
                    .collect(),
                inline_unread: 0,
            })
            .collect();
        messages::feedback(REPO, ISSUE, LOGIN, BRANCH, URL12, &items, BODY_PATH)
    }

    /// One second before `ms`, as GitHub is asked for it, spelled out without the code under
    /// test.
    fn second_before(ms: u64) -> String {
        let text = crate::factory::ledger::format_ts(ms - 1_000);
        format!("{}Z", &text[..19])
    }

    /// The two reads of a pass, with `floor` the moment the reviews are paged back to.
    fn reads(since: &str, floor: &str) -> Vec<String> {
        vec![
            format!("feedback 12 {since} {floor} tom"),
            format!("activity {ISSUE} {since} tom"),
        ]
    }

    /// The reads of a first pass: from the moment the run reached `pr-open`.
    fn first_reads(world: &World) -> Vec<String> {
        let since = second_before(world.seen().pr_open_since.expect("a pr-open run"));
        reads(&since, &since)
    }

    /// The wall time of the newest event whose step starts with `prefix` and ends with `suffix`.
    fn time_of(world: &World, prefix: &str, suffix: &str) -> u64 {
        world
            .events()
            .into_iter()
            .rev()
            .find(|event| {
                event.data["step"]
                    .as_str()
                    .is_some_and(|step| step.starts_with(prefix) && step.ends_with(suffix))
            })
            .expect("such a step")
            .ms
    }

    // ===== the window =====

    mod feedback_the_window {
        use super::*;

        /// The runner's `a failed regate does not move the start of the feedback window` has no
        /// counterpart (there is no re-gate); what it protects is that the window starts at the
        /// pass the run reached `pr-open` on, which this case pins.
        #[test]
        fn the_window_starts_at_the_pass_that_took_the_run_to_pr_open() {
            let world = World::new();
            let pass_at = world.seen().pass.expect("a pass").at_ms;
            assert_eq!(world.seen().pr_open_since, Some(pass_at));
            assert_eq!(world.go(), FeedbackStep::None);
            let since = second_before(pass_at);
            assert_eq!(world.hub.reads(), reads(&since, &since));
        }
    }

    mod feedback_the_step {
        use super::*;

        #[test]
        fn no_feedback_leaves_the_run_untouched() {
            let world = World::new();
            let before = world.count();
            let sim_before = world.rig.sim.calls().len();
            assert_eq!(world.go(), FeedbackStep::None);
            // Two reads, and a note that they were made: no write on GitHub, no call to a worker.
            assert_eq!(world.hub.reads(), first_reads(&world));
            assert_eq!(world.rig.sim.calls().len(), sim_before);
            assert!(world.writes().is_empty());
            assert!(world.rig.worker.calls().is_empty());
            assert_eq!(world.count(), before + 1);
            assert_eq!(world.steps_of("feedback-read").len(), 1);
            // And again, as every pass does: the window holds, so nothing is read or written.
            let after = world.count();
            assert_eq!(world.go(), FeedbackStep::None);
            assert_eq!(world.count(), after);
            assert_eq!(world.hub.reads().len(), 2);
        }

        #[test]
        fn feedback_is_read_at_most_once_every_ten_minutes_per_run() {
            let world = World::new();
            assert_eq!(world.go(), FeedbackStep::None);
            assert_eq!(world.hub.reads(), first_reads(&world));
            assert_eq!(world.steps_of("feedback-read"), ["feedback-read 1 0 clean"]);
            // A millisecond short of ten minutes later: no read, no write, nothing started.
            world.hub.forget_reads();
            let count = world.count();
            world.wait(TEN_MINUTES - Duration::from_millis(1));
            assert_eq!(world.go(), FeedbackStep::None);
            assert!(world.hub.reads().is_empty());
            assert_eq!(world.count(), count);
            assert!(world.rig.worker.calls().is_empty());
            // Ten minutes later it reads again, and records that read.
            world.wait(Duration::from_millis(1));
            assert_eq!(world.go(), FeedbackStep::None);
            assert_eq!(world.hub.reads().len(), 2);
            assert_eq!(
                world.steps_of("feedback-read"),
                ["feedback-read 1 0 clean", "feedback-read 1 600000 clean"]
            );
        }

        #[test]
        fn feedback_seconds_of_0_reads_on_every_pass() {
            let mut world = World::new();
            world.rig.settings.fleet.feedback_seconds = 0;
            assert_eq!(world.go(), FeedbackStep::None);
            world.hub.forget_reads();
            world.wait(Duration::from_secs(60));
            assert_eq!(world.go(), FeedbackStep::None);
            assert_eq!(world.hub.reads().len(), 2);
            assert_eq!(
                world.steps_of("feedback-read"),
                ["feedback-read 1 0 clean", "feedback-read 1 60000 clean"]
            );
        }

        #[test]
        fn feedback_seconds_of_120_waits_120_seconds_between_reads() {
            let mut world = World::new();
            world.rig.settings.fleet.feedback_seconds = 120;
            assert_eq!(world.go(), FeedbackStep::None);
            world.hub.forget_reads();
            world.wait(Duration::from_secs(119));
            assert_eq!(world.go(), FeedbackStep::None);
            assert!(world.hub.reads().is_empty());
            assert_eq!(world.steps_of("feedback-read"), ["feedback-read 1 0 clean"]);
            world.wait(Duration::from_secs(1));
            assert_eq!(world.go(), FeedbackStep::None);
            assert_eq!(world.hub.reads().len(), 2);
            assert_eq!(
                world.steps_of("feedback-read"),
                ["feedback-read 1 0 clean", "feedback-read 1 120000 clean"]
            );
        }

        #[test]
        fn feedback_that_arrives_between_two_reads_waits_for_the_next_one() {
            let world = World::reviewed();
            // The review is on the pull request, but the run was read a moment ago.
            world.rig.step(&world.run, "feedback-read 1 0 clean");
            world.wait(Duration::from_secs(60));
            assert_eq!(world.go(), FeedbackStep::None);
            assert!(world.hub.reads().is_empty());
            assert!(world.rig.worker.calls().is_empty());
            world.wait(TEN_MINUTES);
            assert!(matches!(world.go(), FeedbackStep::Decided { items: 1, .. }));
        }

        #[test]
        fn a_read_that_fails_also_waits_for_the_next_one() {
            let world = World::new();
            world.hub.fail_at(0, false);
            assert!(world.try_go().is_err());
            assert_eq!(world.steps_of("feedback-read"), ["feedback-read 1 0"]);
            world.hub.forget_reads();
            assert_eq!(world.go(), FeedbackStep::None);
            assert!(world.hub.reads().is_empty());
        }

        #[test]
        fn a_missing_or_unreadable_time_of_the_last_read_or_a_clock_set_back_means_the_read_is_due()
        {
            // The time of the last read is a step of the run, measured on the sleep-counting
            // clock under the lease generation it was read under: one of another generation,
            // one in the future (a clock that was reset) and one that cannot be read leave the
            // read due.
            for (name, step) in [
                ("missing", None),
                ("garbage", Some("feedback-read soon")),
                ("empty", Some("feedback-read ")),
                ("negative", Some("feedback-read 1 -5")),
                ("padded with zeros", Some("feedback-read 1 00001")),
                ("another generation", Some("feedback-read 2 59000")),
                ("later than now", Some("feedback-read 1 1000000000")),
            ] {
                let world = World::new();
                if let Some(step) = step {
                    world.rig.step(&world.run, step);
                }
                world.wait(Duration::from_secs(60));
                assert_eq!(world.go(), FeedbackStep::None, "{name}");
                assert_eq!(world.hub.reads(), first_reads(&world), "{name}");
                assert_eq!(
                    world.steps_of("feedback-read").last().map(String::as_str),
                    Some("feedback-read 1 60000 clean"),
                    "{name}"
                );
            }
        }

        #[test]
        fn a_read_that_finds_nothing_is_where_the_next_read_stops_paging() {
            let world = World::new();
            let since_ms = world.seen().pr_open_since.unwrap();
            // The first read starts well after the run reached `pr-open`, and is clean.
            world.pad(100);
            world.wait(TEN_MINUTES);
            assert_eq!(world.go(), FeedbackStep::None);
            let clean = time_of(&world, "feedback-read", " clean");
            assert!(clean > since_ms + 90_000);
            // The next read pages back only to a minute and a second before that read.
            world.hub.forget_reads();
            world.wait(TEN_MINUTES);
            assert_eq!(world.go(), FeedbackStep::None);
            assert_eq!(
                world.hub.reads(),
                reads(&second_before(since_ms), &second_before(clean - 60_000))
            );
        }

        #[test]
        fn a_read_that_fails_does_not_move_the_last_clean_read() {
            let world = World::new();
            let since_ms = world.seen().pr_open_since.unwrap();
            world.pad(100);
            world.wait(TEN_MINUTES);
            assert_eq!(world.go(), FeedbackStep::None);
            let clean = time_of(&world, "feedback-read", " clean");
            world.wait(TEN_MINUTES);
            world.hub.fail_at(world.hub.counted(), false);
            assert!(world.try_go().is_err());
            // The failed read has a step of its own, with no mark of a clean read.
            assert_eq!(
                world.steps_of("feedback-read").last().map(String::as_str),
                Some("feedback-read 1 1200000")
            );
            world.hub.forget_reads();
            world.wait(TEN_MINUTES);
            assert_eq!(world.go(), FeedbackStep::None);
            assert_eq!(
                world.hub.reads(),
                reads(&second_before(since_ms), &second_before(clean - 60_000))
            );
        }

        #[test]
        fn a_read_that_finds_feedback_is_not_a_clean_read() {
            let world = World::reviewed();
            assert!(matches!(world.go(), FeedbackStep::Decided { items: 1, .. }));
            assert_eq!(world.steps_of("feedback-read"), ["feedback-read 1 0"]);
        }

        #[test]
        fn a_clean_read_from_before_the_run_reached_pr_open_is_ignored() {
            // A clean read in an earlier period, written before the pull request's gate passed.
            let world = World::after(&["feedback-read 1 0 clean"]);
            world.wait(TEN_MINUTES);
            assert_eq!(world.go(), FeedbackStep::None);
            assert_eq!(world.hub.reads(), first_reads(&world));
        }

        #[test]
        fn a_garbled_clean_time_means_no_clean_read() {
            for (name, step) in [
                ("garbage", "feedback-read 1 0 soon"),
                ("empty", "feedback-read 1 0 "),
                ("none", "feedback-read 1 0"),
                ("more", "feedback-read 1 0 clean again"),
            ] {
                let world = World::new();
                world.pad(100);
                world.rig.step(&world.run, step);
                world.wait(TEN_MINUTES);
                assert_eq!(world.go(), FeedbackStep::None, "{name}");
                assert_eq!(world.hub.reads(), first_reads(&world), "{name}");
            }
        }

        #[test]
        fn the_time_of_a_new_read_keeps_the_clean_time_of_the_read_before() {
            // The runner's file kept the time of the last read and, on a line of its own, the
            // last clean one; a ledger step holds each read, so a new read's step leaves the
            // clean read before it where it is.
            let world = World::new();
            world.pad(100);
            world.wait(TEN_MINUTES);
            assert_eq!(world.go(), FeedbackStep::None);
            let clean = time_of(&world, "feedback-read", " clean");
            world.rig.step(&world.run, "feedback-read 1 1200000");
            world.hub.forget_reads();
            world.wait(TEN_MINUTES * 2);
            assert_eq!(world.go(), FeedbackStep::None);
            let since = world.seen().pr_open_since.unwrap();
            assert_eq!(
                world.hub.reads(),
                reads(&second_before(since), &second_before(clean - 60_000))
            );
        }

        #[test]
        fn a_decided_relaunch_is_finished_within_the_ten_minutes_without_a_read() {
            let world = World::reviewed();
            assert!(matches!(world.go(), FeedbackStep::Decided { .. }));
            let reads_made = world.hub.reads();
            assert_eq!(reads_made.len(), 2);
            world.rig.worker.on_create(Ok(fresh()));
            assert_eq!(world.go(), FeedbackStep::Preparing(Wait::Workspace));
            // The decision's own pass read the feedback a moment ago: nothing is read again.
            assert_eq!(world.hub.reads(), reads_made);
            assert_eq!(world.steps_of("feedback-read"), ["feedback-read 1 0"]);
            assert_eq!(world.rig.sim.labels(), ["factory:building"]);
        }

        #[test]
        fn after_a_feedback_relaunch_feedback_is_read_from_the_pass_the_run_came_back_to_pr_open_on()
         {
            let mut world = World::reviewed();
            world.round();
            // The session ran, and the gate passed again later: the run is back in `pr-open`.
            world.pad(100);
            passed_gate(&world.rig, &world.run);
            world.rig.moved(&world.run, State::Building, State::PrOpen);
            world.hub.forget_reads();
            world.hub.set_feedback(vec![]);
            world.wait(TEN_MINUTES);
            assert_eq!(world.go(), FeedbackStep::None);
            // The window starts over at that pass, so what the feedback session posted as the
            // factory's login while it built is never read as new feedback.
            let again = world.seen().pr_open_since.unwrap();
            assert_eq!(again, world.seen().pass.unwrap().at_ms);
            let since = second_before(again);
            assert_eq!(world.hub.reads(), reads(&since, &since));
        }

        #[test]
        fn the_feedback_is_asked_of_the_instructors_only() {
            let mut world = World::new();
            world.rig.config.instructors = vec!["rawphp".into(), "tom".into()];
            assert_eq!(world.go(), FeedbackStep::None);
            let since = second_before(world.seen().pr_open_since.unwrap());
            assert_eq!(
                world.hub.reads(),
                vec![
                    format!("feedback 12 {since} {since} rawphp,tom"),
                    format!("activity {ISSUE} {since} rawphp,tom"),
                ]
            );
        }
    }

    mod feedback_a_round {
        use super::*;

        #[test]
        fn an_issue_comment_forwarded_or_reported_as_dropped_while_building_is_not_read_as_feedback()
         {
            let mut world = World::new();
            world.rig.write(
                &world.run,
                Event::CommentForwarded {
                    comment_id: "42".into(),
                },
            );
            world.rig.step(&world.run, "comment-dropped 43");
            world.hub.set_activity(activity(vec![
                issue_comment("42", "2026-10-04T11:00:00Z", "forwarded"),
                issue_comment("43", "2026-10-04T11:01:00Z", "dropped"),
            ]));
            assert_eq!(world.go(), FeedbackStep::None);
            assert!(world.decisions().is_empty());
            assert!(world.rig.worker.calls().is_empty());
            // The filter is per id: a third comment in the same answer is relaunched alone.
            world.hub.set_activity(activity(vec![
                issue_comment("42", "2026-10-04T11:00:00Z", "forwarded"),
                issue_comment("43", "2026-10-04T11:01:00Z", "dropped"),
                issue_comment("44", "2026-10-04T11:02:00Z", "new"),
            ]));
            world.wait(TEN_MINUTES);
            assert!(matches!(world.go(), FeedbackStep::Decided { items: 1, .. }));
            assert_eq!(world.decisions()[0]["items"], json!(["issue-comment:44"]));
            world.rig.view = round_view();
        }

        #[test]
        fn feedback_forwarded_into_a_paused_session_is_not_sent_again_by_a_relaunch() {
            let world = World::reviewed();
            // While the run was `building` and the session was waiting, the review went into it.
            world.rig.write(
                &world.run,
                Event::FeedbackForwarding {
                    n: 1,
                    run_ordinal: Some(1),
                    items: vec!["review:11".into()],
                    message: "the block".into(),
                },
            );
            assert_eq!(world.go(), FeedbackStep::None);
            assert!(world.decisions().is_empty());
            assert!(world.rig.worker.calls().is_empty());
        }

        #[test]
        fn an_instructor_review_relaunches_a_session_on_the_pr_branch_with_its_inline_comments() {
            let mut world = World::reviewed();
            let first = block(&[review(11, "2026-10-04T11:00:00Z")]);
            // The pass that decides: the label, the pull request and the decision, in order.
            assert_eq!(
                world.go(),
                FeedbackStep::Decided {
                    round: 1,
                    pr: 12,
                    items: 1
                }
            );
            let body = first.body.clone().unwrap();
            assert!(body.contains("Inline comment: src/a.rs:7\n"), "{body}");
            assert!(body.contains("use a better name"), "{body}");
            assert!(body.contains("git push origin HEAD:feat/pr-12"), "{body}");
            let decisions = world.decisions();
            assert_eq!(decisions[0]["n"], 1);
            assert_eq!(decisions[0]["branch"], BRANCH);
            assert_eq!(decisions[0]["items"], json!(["review:11"]));
            assert_eq!(decisions[0]["message"], body.as_str());
            let names = world.rig.names(&world.run);
            assert_eq!(
                &names[names.len() - 4..],
                [
                    "feedback_relaunching",
                    "step_changed",
                    "state_changed",
                    "feedback_relaunching"
                ],
                "the decision, the machine, the move, and the decision again"
            );
            assert_eq!(
                world.steps_of("feedback-machine"),
                [format!("feedback-machine 1 {}", m1())]
            );
            // GitHub: the pull request a draft again, the issue ours and `building`.
            assert_eq!(world.rig.sim.labels(), ["factory:building"]);
            assert_eq!(world.rig.sim.assignees(), [LOGIN]);
            let writes = world.writes().join("\n");
            assert!(writes.contains("pr ready 12"), "{writes}");
            assert!(writes.contains("--undo"), "{writes}");
            assert!(writes.contains("Factory status:** building"), "{writes}");
            assert!(writes.contains(&m1().to_string()), "{writes}");
            let moved = world
                .events()
                .into_iter()
                .rev()
                .find(|event| event.event == "state_changed")
                .unwrap();
            assert_eq!(moved.data["from"], "pr-open");
            assert_eq!(moved.data["to"], "building");
            assert!(world.rig.worker.calls().is_empty(), "no workspace is live");
            // The pass that asks for the workspace: on the pull request's branch, with its own
            // setup, named for the round.
            world.rig.worker.on_create(Ok(fresh()));
            assert_eq!(world.go(), FeedbackStep::Preparing(Wait::Workspace));
            let Call::Create { machine, request } = world.creates().remove(0) else {
                unreachable!()
            };
            assert_eq!(machine, m1());
            assert_eq!(
                request,
                CreateWorkspace {
                    project: project_id(),
                    name: "fac/8-1-f1".into(),
                    checkout: Checkout::Branch {
                        name: BRANCH.into()
                    },
                    setup: Setup::Own,
                    tag: FactoryTag {
                        project: project_id(),
                        run: world.run.to_string(),
                    },
                }
            );
            assert_eq!(
                world.steps_of("feedback-"),
                [
                    "feedback-read 1 0".to_string(),
                    format!("feedback-machine 1 {}", m1()),
                    "feedback-requested 1 fac/8-1-f1".to_string(),
                    format!("feedback-workspace 1 {}", fresh()),
                ]
            );
            // The pass that starts the session, with the first message and the launch line.
            world.rig.view = round_view();
            world.rig.worker.on_start_session(Ok(round_session()));
            assert_eq!(
                world.go(),
                FeedbackStep::Started {
                    round: 1,
                    session: round_session()
                }
            );
            let Call::StartSession {
                workspace,
                project,
                login,
                model,
                effort,
                first: sent,
            } = world.starts().remove(0)
            else {
                unreachable!()
            };
            assert_eq!(
                (
                    workspace,
                    project,
                    login.as_str(),
                    model.as_str(),
                    effort.as_str()
                ),
                (fresh(), project_id(), LOGIN, "opus", "high")
            );
            assert_eq!(sent, first);
            assert!(world.steps().contains(&"feedback-sending 1".to_string()));
            let launch = world.launches().pop().unwrap();
            assert_eq!(launch["thread_id"], round_session().to_string());
            assert_eq!(launch["worktree"], fresh().to_string());
            assert_eq!(launch["branch"], BRANCH);
            assert_eq!(launch["base"], "main");
            assert_eq!(launch["title"], "Make it faster");
            assert_eq!(
                launch["resumed_from"],
                json!({"run": world.run.to_string(), "branch": BRANCH})
            );
            // The status comment is written again once the session is known, naming it (the
            // comment hides the id itself, as it hides any long word).
            let status = world.writes().pop().unwrap();
            assert!(status.starts_with("api -X PATCH"), "{status}");
            assert!(status.contains("Factory status:** building"), "{status}");
            assert!(!status.contains("Session: none"), "{status}");
            // The same run: no gate attempt, nothing run on the gate.
            let seen = world.seen();
            assert_eq!(seen.feedback_relaunches, 1);
            assert_eq!(seen.thread_id, Some(round_session().to_string()));
            assert_eq!(seen.state, Some(State::Building));
            assert_eq!(seen.resumed_from.as_deref(), Some(BRANCH));
            assert!(
                !world
                    .events()
                    .iter()
                    .any(|e| e.event == "gate_regate" || e.event == "gate_sent_back")
            );
        }

        #[test]
        fn a_review_a_conversation_comment_and_an_issue_comment_make_one_relaunch_in_time_order() {
            let world = World::new();
            let mut late = review(11, "2026-10-04T12:00:00Z");
            late.created_at = "2026-10-04T12:00:00Z".into();
            world.hub.set_feedback(vec![
                late.clone(),
                pr_comment(21, "2026-10-04T11:00:00Z", "pr first"),
            ]);
            world.hub.set_activity(activity(vec![issue_comment(
                "42",
                "2026-10-04T13:00:00Z",
                "issue last",
            )]));
            assert_eq!(
                world.go(),
                FeedbackStep::Decided {
                    round: 1,
                    pr: 12,
                    items: 3
                }
            );
            assert_eq!(
                world.decisions()[0]["items"],
                json!(["pr-comment:21", "review:11", "issue-comment:42"])
            );
            let issue = item(
                "issue-comment:42",
                ItemKind::IssueComment,
                "2026-10-04T13:00:00Z",
                "issue last",
            );
            let want = block(&[
                pr_comment(21, "2026-10-04T11:00:00Z", "pr first"),
                late,
                issue,
            ]);
            assert_eq!(world.decisions()[0]["message"], want.body.unwrap().as_str());
        }

        #[test]
        fn an_item_that_was_sent_is_not_sent_again_and_neither_is_an_edited_one() {
            let mut world = World::reviewed();
            world.hub.set_activity(activity(vec![issue_comment(
                "42",
                "2026-10-04T13:00:00Z",
                "issue",
            )]));
            world.round();
            // The session did its work and the gate passed again: the run is back in `pr-open`,
            // its workspace archived.
            world.rig.view = fleet().build();
            passed_gate(&world.rig, &world.run);
            world.rig.moved(&world.run, State::Building, State::PrOpen);
            // The same items are still on GitHub, one of them edited since.
            let mut edited = review(11, "2026-10-04T11:00:00Z");
            edited.body = "Edited: never mind".into();
            world.hub.set_feedback(vec![edited]);
            world.hub.set_activity(activity(vec![issue_comment(
                "42",
                "2026-10-04T13:00:00Z",
                "edited issue comment",
            )]));
            world.wait(TEN_MINUTES);
            let count = world.count();
            assert_eq!(world.go(), FeedbackStep::None);
            assert_eq!(world.starts().len(), 1);
            assert_eq!(world.count(), count + 1, "the read's own step only");
            // A new item is sent, alone, as the second round.
            world.hub.set_feedback(vec![
                review(11, "2026-10-04T11:00:00Z"),
                pr_comment(30, "2026-10-04T14:00:00Z", "one more"),
            ]);
            world.wait(TEN_MINUTES);
            assert_eq!(
                world.go(),
                FeedbackStep::Decided {
                    round: 2,
                    pr: 12,
                    items: 1
                }
            );
            let decided = world.decisions();
            assert_eq!(decided.len(), 4, "each round writes its decision twice");
            assert_eq!(decided[2]["n"], 2);
            assert_eq!(decided[2]["items"], json!(["pr-comment:30"]));
            assert_eq!(world.seen().feedback_relaunches, 2);
            world.rig.worker.on_create(Ok(fresh()));
            assert_eq!(world.go(), FeedbackStep::Preparing(Wait::Workspace));
            assert_eq!(
                world.steps_of("feedback-requested"),
                [
                    "feedback-requested 1 fac/8-1-f1",
                    "feedback-requested 2 fac/8-1-f2"
                ]
            );
        }

        #[test]
        fn a_run_that_reached_pr_open_with_its_build_workspace_still_showing_has_it_archived_before_the_relaunch()
         {
            let mut world = World::reviewed();
            world.rig.view = fleet()
                .workspace(workspace_record(old(), "fac/8-1", None))
                .build();
            assert!(matches!(world.go(), FeedbackStep::Decided { .. }));
            // The archive of the earlier workspace is recorded, then asked for; the new
            // workspace is not asked for in the same pass.
            assert_eq!(
                world.steps_of("feedback-archiv"),
                [
                    format!("feedback-archiving 1 {}", old()),
                    "feedback-archive-asked 1".to_string()
                ]
            );
            assert_eq!(
                world.rig.worker.calls(),
                vec![Call::Archive { workspace: old() }]
            );
            // While it still shows on a machine that is heard from, the archive is asked again
            // and nothing is created.
            assert_eq!(world.go(), FeedbackStep::Preparing(Wait::Archive));
            assert_eq!(world.archives().len(), 2);
            assert!(world.creates().is_empty());
            // Once it shows archived the workspace is made.
            world.rig.view = fleet().build();
            world.rig.worker.on_create(Ok(fresh()));
            assert_eq!(world.go(), FeedbackStep::Preparing(Wait::Workspace));
            assert_eq!(world.creates().len(), 1);
            assert_eq!(world.archives().len(), 2);
            world.rig.view = round_view();
        }

        #[test]
        fn a_workspace_on_a_machine_unreachable_for_15_minutes_is_not_waited_for() {
            let mut world = World::reviewed();
            let gone = MachineId(uuid::Uuid::from_u128(0x3));
            let mut record = workspace_record(old(), "fac/8-1", None);
            record.owner = gone;
            world.rig.view = fleet()
                .workspace(record.clone())
                .unreachable(gone, Duration::from_secs(14 * 60))
                .build();
            assert!(matches!(world.go(), FeedbackStep::Decided { .. }));
            assert_eq!(
                world.archives().len(),
                1,
                "asked once, in the decision's pass"
            );
            world.rig.view = fleet()
                .workspace(record)
                .unreachable(gone, Duration::from_secs(14 * 60))
                .build();
            assert_eq!(world.go(), FeedbackStep::Preparing(Wait::Archive));
            let mut record = workspace_record(old(), "fac/8-1", None);
            record.owner = gone;
            world.rig.view = fleet()
                .workspace(record)
                .unreachable(gone, Duration::from_secs(15 * 60))
                .build();
            world.rig.worker.on_create(Ok(fresh()));
            assert_eq!(world.go(), FeedbackStep::Preparing(Wait::Workspace));
            assert_eq!(world.creates().len(), 1);
        }

        #[test]
        fn a_round_with_every_run_slot_held_waits_in_pr_open_and_adds_nothing_to_worked_time() {
            let world = World::reviewed();
            world.pad(100);
            let before = world.count();
            let worked = world.seen().worked_ms;
            let mut none_free = slots(0);
            let mut reserved = Reserved::default();
            assert_eq!(
                world.try_pass(&mut none_free, &mut reserved).unwrap(),
                FeedbackStep::Waiting {
                    reason: NO_SLOT.into()
                }
            );
            // Nothing is written to the ledger or GitHub, and no worker is asked.
            assert_eq!(world.count(), before);
            assert!(world.writes().is_empty());
            assert!(world.rig.worker.calls().is_empty());
            assert!(world.decisions().is_empty());
            assert_eq!(world.rig.sim.labels(), ["factory:pr-open"]);
            // The issue stays `pr-open`, and the wait counts for nothing.
            world.wait(Duration::from_secs(3600));
            world.pad(100);
            assert_eq!(
                world.try_pass(&mut none_free, &mut reserved).unwrap(),
                FeedbackStep::Waiting {
                    reason: NO_SLOT.into()
                }
            );
            let seen = world.seen();
            assert_eq!(seen.state, Some(State::PrOpen));
            assert_eq!(seen.worked_ms, worked);
            assert_eq!(seen.working_since, None);
            // The feedback is read again when a slot is free, and the round starts then.
            assert!(matches!(world.go(), FeedbackStep::Decided { items: 1, .. }));
            assert_eq!(world.rig.sim.labels(), ["factory:building"]);
        }

        #[test]
        fn a_round_waits_when_placement_has_no_machine() {
            let mut world = World::reviewed();
            // The only machine is at its limit of sessions.
            world.rig.view = fleet_with(4).build();
            let before = world.count();
            let step = world.go();
            let FeedbackStep::Waiting { reason } = step else {
                panic!("expected a wait, got {step:?}");
            };
            assert!(reason.starts_with("mac:"), "{reason}");
            assert_eq!(world.count(), before);
            assert!(world.writes().is_empty());
            assert_eq!(world.rig.sim.labels(), ["factory:pr-open"]);
            // No machine at all.
            world.rig.view = FleetView::builder().build();
            assert!(matches!(world.go(), FeedbackStep::Waiting { .. }));
            assert_eq!(world.count(), before);
        }

        #[test]
        fn two_runs_in_one_pass_do_not_both_take_the_last_run_slot() {
            let world = World::reviewed();
            let second = RunId::new(REPO, 9, 1).unwrap();
            pr_open_after(&world.rig, &second, &[]);
            let mut last = slots(1);
            let mut reserved = Reserved::default();
            let first = advance(
                &world.ctx(),
                &world.run,
                &shared(),
                &mut last,
                &mut reserved,
            );
            assert!(matches!(first, Ok(FeedbackStep::Decided { .. })));
            assert_eq!(last.runs_free, 0);
            let other = advance(&world.ctx(), &second, &shared(), &mut last, &mut reserved);
            assert_eq!(
                other.unwrap(),
                FeedbackStep::Waiting {
                    reason: NO_SLOT.into()
                }
            );
            assert!(
                world
                    .rig
                    .events(&second)
                    .iter()
                    .all(|event| event.event != "feedback_relaunching")
            );
        }

        #[test]
        fn two_runs_in_one_pass_do_not_both_land_on_the_machine_with_one_session_of_room() {
            let world = World::reviewed();
            let second = RunId::new(REPO, 9, 1).unwrap();
            pr_open_after(&world.rig, &second, &[]);
            let mut room = slots(2);
            let mut reserved = Reserved::default();
            let one = shared_with(1);
            let first = advance(&world.ctx(), &world.run, &one, &mut room, &mut reserved);
            assert!(matches!(first, Ok(FeedbackStep::Decided { .. })));
            let other = advance(&world.ctx(), &second, &one, &mut room, &mut reserved).unwrap();
            let FeedbackStep::Waiting { reason } = other else {
                panic!("expected a wait, got {other:?}");
            };
            assert!(reason.starts_with("mac:"), "{reason}");
            assert_eq!(room.runs_free, 1, "a round that waits takes no slot");
        }

        #[test]
        fn an_issue_the_runner_left_in_pr_open_with_no_foundry_run_is_not_read_for_feedback() {
            let rig = Rig::new();
            let hub = Hub::new(rig.sim.clone());
            hub.set_feedback(vec![review(11, "2026-10-04T11:00:00Z")]);
            rig.sim.set_labels(&["factory:pr-open"]);
            let run = run_id(1);
            let world = World { rig, hub, run };
            assert_eq!(world.go(), FeedbackStep::None);
            // No call to GitHub, none to a worker, and nothing written to the ledger.
            assert_eq!(world.hub.counted(), 0);
            assert!(world.hub.reads().is_empty());
            assert!(world.rig.sim.calls().is_empty());
            assert!(world.rig.worker.calls().is_empty());
            assert!(world.events().is_empty());
            // A run that only began, with no session and no gate pass, is not read either.
            world
                .rig
                .step(&world.run, &format!("approved {LABELED_AT}"));
            world.rig.write(
                &world.run,
                Event::Claimed {
                    machine: m1().to_string(),
                    github_user: LOGIN.into(),
                },
            );
            let count = world.count();
            assert_eq!(world.go(), FeedbackStep::None);
            assert_eq!(world.hub.counted(), 0);
            assert_eq!(world.count(), count);
        }

        #[test]
        fn a_run_that_is_not_waiting_in_pr_open_is_left_alone() {
            // Building, with a session: the feedback step does nothing and asks GitHub nothing.
            let rig = Rig::new();
            let run = run_id(1);
            pr_open_after(&rig, &run, &[]);
            rig.moved(&run, State::PrOpen, State::Building);
            let hub = Hub::new(rig.sim.clone());
            hub.set_feedback(vec![review(11, "2026-10-04T11:00:00Z")]);
            let building = World { rig, hub, run };
            assert_eq!(building.go(), FeedbackStep::None);
            assert_eq!(building.hub.counted(), 0);
            // Finished.
            let finished = World::reviewed();
            finished
                .rig
                .ledger
                .finish(
                    &finished.run,
                    crate::factory::ledger::events::Outcome::PrOpen,
                    None,
                    None,
                    &Cause::new(CauseStep::Gate, "seed"),
                    None,
                )
                .unwrap();
            assert_eq!(finished.go(), FeedbackStep::None);
            assert_eq!(finished.hub.counted(), 0);
            // Stuck.
            let stuck = World::reviewed();
            stuck.rig.moved(&stuck.run, State::PrOpen, State::Stuck);
            assert_eq!(stuck.go(), FeedbackStep::None);
            assert_eq!(stuck.hub.counted(), 0);
            // A stop asked for is the stop step's.
            let stopping = World::reviewed();
            stopping.rig.write(
                &stopping.run,
                Event::StopRequested {
                    by: "operator".into(),
                },
            );
            assert_eq!(stopping.go(), FeedbackStep::None);
            assert_eq!(stopping.hub.counted(), 0);
        }

        #[test]
        fn a_run_of_another_repo_is_left_alone_and_nothing_is_touched() {
            let world = World::reviewed();
            let other = RunId::new("acme/widgets", 8, 1).unwrap();
            pr_open_after(&world.rig, &other, &[]);
            let count = world.rig.events(&other).len();
            let step = advance(
                &world.ctx(),
                &other,
                &shared(),
                &mut slots(1),
                &mut Reserved::default(),
            );
            assert_eq!(step.unwrap(), FeedbackStep::None);
            assert_eq!(world.hub.counted(), 0);
            assert_eq!(world.rig.events(&other).len(), count);
        }

        #[test]
        fn a_re_gate_in_progress_is_left_to_the_gate() {
            let world = World::reviewed();
            world.rig.write(
                &world.run,
                Event::GateRegate {
                    sha: HEAD.into(),
                    base_sha: "9f8e7d6c5b4a39281706f5e4d3c2b1a098765432".into(),
                    previous_base_sha: "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d".into(),
                    conflict: false,
                },
            );
            assert!(world.seen().regating);
            assert_eq!(world.go(), FeedbackStep::None);
            assert_eq!(world.hub.counted(), 0);
        }

        #[test]
        fn a_pending_re_gate_relaunch_holds_feedback_back_without_a_re_gate_on_record() {
            let world = World::reviewed();
            world.rig.write(
                &world.run,
                Event::RegateRelaunching {
                    attempt: 2,
                    branch: BRANCH.into(),
                    message: "boom".into(),
                },
            );
            assert!(!world.seen().regating);
            let count = world.count();
            assert_eq!(world.go(), FeedbackStep::None);
            assert_eq!(world.hub.counted(), 0);
            assert_eq!(world.count(), count, "nothing is written");
            assert!(world.rig.worker.calls().is_empty());
        }

        #[test]
        fn a_re_gate_relaunch_whose_session_is_recorded_is_left_to_the_gate() {
            let world = World::reviewed();
            world.rig.write(
                &world.run,
                Event::RegateRelaunching {
                    attempt: 2,
                    branch: BRANCH.into(),
                    message: "boom".into(),
                },
            );
            world.rig.write(
                &world.run,
                Event::SessionLaunched {
                    thread_id: round_session().to_string(),
                    worktree: Some(fresh().to_string()),
                    branch: Some(BRANCH.into()),
                    model_instance: None,
                    model: Some("opus".into()),
                    model_options: None,
                    resumed_from: None::<ResumedFrom>,
                    base: Some("main".into()),
                    title: None,
                },
            );
            let count = world.count();
            assert_eq!(world.go(), FeedbackStep::None);
            assert_eq!(world.hub.counted(), 0);
            assert_eq!(world.count(), count);
        }
    }

    mod feedback_stopping_a_round {
        use super::*;

        #[test]
        fn a_changed_issue_text_sends_the_run_stuck_and_nothing_is_launched() {
            let world = World::reviewed();
            world.rig.sim.set_approval(approval_answer(
                LABELED_AT,
                "tom",
                &[("2026-10-04T10:30:00Z", "eve")],
            ));
            assert_eq!(
                world.go(),
                FeedbackStep::Stuck {
                    reason: messages::CHANGED_DURING_BUILD.into()
                }
            );
            assert!(world.rig.worker.calls().is_empty());
            assert!(world.decisions().is_empty());
            assert!(world.seen().feedback_sent.is_empty());
            assert_eq!(world.rig.sim.labels(), ["factory:stuck"]);
            assert!(
                world.writes().join("\n").contains("--undo"),
                "a draft again"
            );
            assert!(world.rig.finished(&world.run));
        }

        #[test]
        fn an_edit_history_that_cannot_be_judged_sends_the_run_stuck_with_the_same_words() {
            let world = World::reviewed();
            // A history that shows no edit but for which the timeline is cut off.
            world.rig.sim.set_approval("{\"data\": null}".into());
            let step = world.try_go();
            assert!(
                step.is_err(),
                "an unreadable answer is an error, not a stop"
            );
            assert!(world.decisions().is_empty());
            assert!(world.rig.worker.calls().is_empty());
        }

        #[test]
        fn a_merged_pull_request_ends_the_run_and_the_feedback_is_not_sent() {
            let world = World::reviewed();
            world.rig.sim.set_pull(
                12,
                SimPull {
                    state: "MERGED",
                    draft: false,
                    head: BRANCH.into(),
                    base: "main".into(),
                    cross: false,
                },
            );
            assert_eq!(world.go(), FeedbackStep::Ended);
            assert!(world.rig.worker.calls().is_empty());
            assert!(world.decisions().is_empty());
            let end = world.events().pop().unwrap();
            assert_eq!(end.event, "finished");
            assert_eq!(end.data["outcome"], "pr-open");
            assert_eq!(end.data["pr_end"], "merged");
            // Nothing was written on GitHub.
            assert!(world.writes().is_empty());
            assert_eq!(world.rig.sim.labels(), ["factory:pr-open"]);
        }

        #[test]
        fn a_closed_pull_request_ends_the_run_too() {
            let world = World::reviewed();
            world.rig.sim.set_pull(
                12,
                SimPull {
                    state: "CLOSED",
                    draft: false,
                    head: BRANCH.into(),
                    base: "main".into(),
                    cross: false,
                },
            );
            assert_eq!(world.go(), FeedbackStep::Ended);
            assert_eq!(world.events().pop().unwrap().data["pr_end"], "closed");
            assert!(world.rig.worker.calls().is_empty());
        }

        #[test]
        fn a_closed_issue_is_a_persons_move_and_the_feedback_is_not_sent() {
            let world = World::reviewed();
            world.rig.sim.set_closed(true);
            assert_eq!(world.go(), FeedbackStep::Abandoned);
            assert!(world.rig.worker.calls().is_empty());
            assert!(world.decisions().is_empty());
            assert!(world.rig.finished(&world.run));
        }

        #[test]
        fn a_branch_that_is_not_a_plain_name_is_not_passed_to_a_session() {
            for head in ["x; rm -rf ~", "-x", "has space", "ünï", "a..b", ""] {
                let world = World::reviewed();
                world.rig.sim.set_pull(
                    12,
                    SimPull {
                        state: "OPEN",
                        draft: false,
                        head: head.into(),
                        base: "main".into(),
                        cross: false,
                    },
                );
                assert_eq!(
                    world.go(),
                    FeedbackStep::Stuck {
                        reason: BRANCH_REFUSED.into()
                    },
                    "{head:?}"
                );
                assert!(world.decisions().is_empty(), "{head:?}");
                assert_eq!(world.rig.sim.labels(), ["factory:stuck"], "{head:?}");
            }
        }

        #[test]
        fn a_pull_request_branch_named_a_dot_dot_b_is_refused_before_any_workspace_is_asked_for() {
            let world = World::reviewed();
            world.rig.sim.set_pull(
                12,
                SimPull {
                    state: "OPEN",
                    draft: false,
                    head: "a..b".into(),
                    base: "main".into(),
                    cross: false,
                },
            );
            assert_eq!(
                world.go(),
                FeedbackStep::Stuck {
                    reason: BRANCH_REFUSED.into()
                }
            );
            // No archive, no create, no session; and no decision for a later pass to carry on.
            assert!(world.rig.worker.calls().is_empty());
            assert!(world.decisions().is_empty());
            assert!(world.steps_of("feedback-machine").is_empty());
            assert!(world.steps_of("feedback-requested").is_empty());
            assert!(world.rig.finished(&world.run));
            // The comment says the factory could not launch, and holds no word of the branch.
            let comment = world.rig.sim.comments().join("\n");
            assert!(comment.contains(LAUNCH_FAILED), "{comment}");
            assert!(!comment.contains("a..b"), "{comment}");
        }
    }

    // ----- carrying a round on -----

    fn session_record(state: SessionState) -> Record<foundry_proto::record::Session> {
        Record {
            owner: m1(),
            version: 1,
            deleted: false,
            body: foundry_proto::record::Session {
                id: round_session(),
                workspace: fresh(),
                terminal: TerminalId(uuid::Uuid::from_u128(0x5e6)),
                model: "opus".into(),
                state,
            },
        }
    }

    /// The round's workspace and its session in `state`, with a turn record when `turn` gives
    /// the turn and the count of prompts.
    fn session_view(state: SessionState, turn: Option<(u64, u64)>) -> FleetView {
        let mut builder = fleet()
            .workspace(workspace_record(
                fresh(),
                "fac/8-1-f1",
                Some(setup_terminal()),
            ))
            .terminal(terminal_record(TerminalState::Exited, Some(0)))
            .session(session_record(state));
        if let Some((turn, prompts)) = turn {
            builder = builder.turn(foundry_proto::factory::SessionTurn {
                session: round_session(),
                workspace: fresh(),
                project: project_id(),
                turn,
                prompts,
                question: false,
            });
        }
        builder.build()
    }

    /// Passes until the round's session runs, a stop or the end of the passes: each pass ten
    /// minutes after the one before, the workspace showing once a create was asked for. Each
    /// answer the worker is scripted for is given as often as it is asked for.
    fn drive(world: &mut World) -> Vec<Result<FeedbackStep, String>> {
        let mut steps = Vec::new();
        for _ in 0..14 {
            let shown = !world
                .rig
                .view
                .workspaces_named(project_id(), "fac/8-1-f1")
                .is_empty();
            if !world.creates().is_empty() && !shown {
                world.rig.view = round_view();
            }
            let step = world.try_go().map_err(|error| error.to_string());
            steps.push(step);
            if world.launches().len() == 2 {
                break;
            }
            world.wait(TEN_MINUTES);
        }
        steps
    }

    /// A world whose worker answers every create and start the way a healthy fleet does.
    fn healthy() -> World {
        let world = World::reviewed();
        for _ in 0..8 {
            world.rig.worker.on_create(Ok(fresh()));
            world.rig.worker.on_start_session(Ok(round_session()));
        }
        world
    }

    mod feedback_carrying_a_round_on {
        use super::*;

        #[test]
        fn building_and_resume_leave_a_round_being_set_up_alone() {
            let mut world = World::reviewed();
            assert!(matches!(world.go(), FeedbackStep::Decided { .. }));
            // The label says `building` and the old session is not the run's: the steps that
            // follow a building session read the decision and leave the run to this step.
            assert_eq!(world.seen().state, Some(State::Building));
            assert!(crate::factory::runs::progress::pending_relaunch(&world.seen()).is_some());
            let before = world.count();
            let built = crate::factory::runs::building::advance(&world.ctx(), &world.run).unwrap();
            assert!(
                matches!(
                    built,
                    crate::factory::runs::building::BuildStep::Skipped { .. }
                ),
                "{built:?}"
            );
            let resumed = crate::factory::runs::resume::advance(&world.ctx(), &world.run).unwrap();
            assert!(
                matches!(
                    resumed,
                    crate::factory::runs::resume::ResumeStep::Skipped { .. }
                ),
                "{resumed:?}"
            );
            assert_eq!(world.count(), before);
            assert!(world.rig.worker.calls().is_empty());
            // The decision holds until the session is recorded, then the round is no decision.
            world.rig.worker.on_create(Ok(fresh()));
            assert_eq!(world.go(), FeedbackStep::Preparing(Wait::Workspace));
            assert!(crate::factory::runs::progress::pending_relaunch(&world.seen()).is_some());
            world.rig.view = round_view();
            world.rig.worker.on_start_session(Ok(round_session()));
            assert!(matches!(world.go(), FeedbackStep::Started { .. }));
            assert!(crate::factory::runs::progress::pending_relaunch(&world.seen()).is_none());
        }

        #[test]
        fn a_crash_after_the_decision_starts_the_session_once() {
            let mut world = healthy();
            world.rig.guard = Box::new(HeldFor::new(0));
            assert!(world.try_go().is_err(), "a lost lease writes nothing");
            assert!(world.decisions().is_empty());
            world.rig.guard = Box::new(HeldFor::new(3));
            // The lease is lost after the decision, the machine and the move are recorded.
            let first = world.try_go();
            let _ = first;
            world.rig.guard = Box::new(crate::factory::worker::AlwaysHeld(1));
            let before = world.decisions().len();
            let steps = drive(&mut world);
            assert!(
                matches!(steps.last(), Some(Ok(FeedbackStep::Started { .. }))),
                "{steps:?}"
            );
            assert_eq!(world.starts().len(), 1);
            assert_eq!(world.launches().len(), 2, "the build's and the round's");
            assert!(world.decisions().len() >= before);
            assert!(
                world
                    .decisions()
                    .iter()
                    .all(|decision| decision["items"] == json!(["review:11"]))
            );
        }

        #[test]
        fn a_launch_that_is_cut_short_after_the_session_is_recorded_is_finished_without_a_second_launch()
         {
            // The lease is lost after each number of checks in turn, up to the pass that starts
            // the session and writes its launch line: wherever it was cut, one session started.
            let mut cut_after_a_start = 0;
            for checks in 0..40 {
                let mut world = healthy();
                assert!(matches!(world.go(), FeedbackStep::Decided { .. }));
                assert_eq!(world.go(), FeedbackStep::Preparing(Wait::Workspace));
                world.rig.view = round_view();
                world.rig.guard = Box::new(HeldFor::new(checks));
                let cut = world.try_go().is_err();
                world.rig.guard = Box::new(crate::factory::worker::AlwaysHeld(1));
                if world.starts().len() == 1 {
                    // The session exists, whether or not its line was written: the fleet
                    // shows it with its turn.
                    world.rig.view = session_view(SessionState::Working, Some((1, 1)));
                    cut_after_a_start += usize::from(cut && world.launches().len() == 1);
                }
                let steps = drive(&mut world);
                assert_eq!(world.starts().len(), 1, "after {checks} checks: {steps:?}");
                assert_eq!(world.launches().len(), 2, "after {checks} checks");
                if !cut {
                    break;
                }
            }
            assert!(cut_after_a_start > 0, "a start with no launch line was cut");
        }

        #[test]
        fn a_rate_limit_between_the_ledger_and_the_tracker_write_is_finished_by_the_next_call() {
            let mut world = healthy();
            // The pull request's draft mark is the first write of the round: a rate limit there
            // leaves the decision recorded and the label where it was.
            world.rig.sim.fail("--undo", 1);
            assert!(world.try_go().is_err());
            assert_eq!(world.decisions().len(), 1);
            assert_eq!(world.rig.sim.labels(), ["factory:pr-open"]);
            assert_eq!(world.seen().state, Some(State::PrOpen));
            assert!(world.starts().is_empty());
            // The next pass finishes the move, with no read of the feedback.
            world.hub.forget_reads();
            let step = world.go();
            assert!(
                matches!(step, FeedbackStep::Decided { round: 1, .. }),
                "{step:?}"
            );
            assert!(world.hub.reads().is_empty());
            assert_eq!(world.rig.sim.labels(), ["factory:building"]);
            assert_eq!(world.seen().state, Some(State::Building));
            let steps = drive(&mut world);
            assert!(matches!(
                steps.last(),
                Some(Ok(FeedbackStep::Started { .. }))
            ));
            assert_eq!(world.starts().len(), 1);
        }

        #[test]
        fn a_feedback_relaunch_survives_a_failure_of_each_call() {
            // The calls to GitHub of a whole round, counted on a run of its own.
            let mut reference = healthy();
            let steps = drive(&mut reference);
            assert!(matches!(
                steps.last(),
                Some(Ok(FeedbackStep::Started { .. }))
            ));
            let calls = reference.hub.counted();
            assert!(calls >= 8, "{calls}");
            for call in 0..calls {
                for lose in [false, true] {
                    let label = format!("call {call}, effect kept: {lose}");
                    let mut world = healthy();
                    world.hub.fail_at(call, lose);
                    let _steps = drive(&mut world);
                    // However the round was cut, one session started and one round was made.
                    assert_eq!(world.starts().len(), 1, "{label}");
                    assert_eq!(world.launches().len(), 2, "{label}");
                    assert_eq!(world.seen().feedback_relaunches, 1, "{label}");
                    assert_eq!(world.seen().state, Some(State::Building), "{label}");
                    assert_eq!(world.rig.sim.labels(), ["factory:building"], "{label}");
                    assert_eq!(world.rig.sim.assignees(), [LOGIN], "{label}");
                    let names: Vec<_> = world
                        .creates()
                        .into_iter()
                        .map(|call| match call {
                            Call::Create { request, .. } => request.name,
                            _ => unreachable!(),
                        })
                        .collect();
                    assert!(names.iter().all(|name| name == "fac/8-1-f1"), "{label}");
                    assert!(
                        world
                            .decisions()
                            .iter()
                            .all(|decision| decision["items"] == json!(["review:11"])),
                        "{label}"
                    );
                }
            }
        }

        #[test]
        fn a_gh_failure_in_the_middle_of_a_round_leaves_it_to_be_carried_on_with_no_second_session()
        {
            let mut world = healthy();
            assert!(matches!(world.go(), FeedbackStep::Decided { .. }));
            assert_eq!(world.go(), FeedbackStep::Preparing(Wait::Workspace));
            world.rig.view = round_view();
            // The text is read again before the session starts, and that read fails.
            world.hub.fail_at(world.hub.counted(), false);
            assert!(world.try_go().is_err());
            assert!(world.starts().is_empty());
            // The next pass carries the round on: one session, started once.
            let step = world.go();
            assert!(
                matches!(step, FeedbackStep::Started { round: 1, .. }),
                "{step:?}"
            );
            assert_eq!(world.starts().len(), 1);
            assert_eq!(world.launches().len(), 2);
            // Nothing more is started by later passes.
            world.rig.view = session_view(SessionState::Working, Some((1, 1)));
            assert_eq!(world.go(), FeedbackStep::None);
            assert_eq!(world.starts().len(), 1);
        }

        #[test]
        fn a_worker_that_cannot_be_reached_is_asked_again_and_the_session_starts_once() {
            let mut world = World::reviewed();
            world
                .rig
                .worker
                .on_create(Err(WorkerError::Unreachable("down".into())));
            world.rig.worker.on_create(Ok(fresh()));
            world
                .rig
                .worker
                .on_start_session(Err(WorkerError::Unreachable("down".into())));
            world.rig.worker.on_start_session(Ok(round_session()));
            let steps = drive(&mut world);
            assert!(
                matches!(steps.last(), Some(Ok(FeedbackStep::Started { .. }))),
                "{steps:?}"
            );
            assert_eq!(
                world.starts().len(),
                2,
                "the second answers the first's lost one"
            );
            assert_eq!(world.launches().len(), 2);
            // The start that may have gone out is ended before the second, recorded first.
            assert!(
                world
                    .rig
                    .worker
                    .calls()
                    .contains(&Call::EndSessions { workspace: fresh() })
            );
            let steps = world.steps_of("feedback-");
            let sending = steps
                .iter()
                .position(|s| s == "feedback-sending 1")
                .unwrap();
            let ending = steps
                .iter()
                .position(|s| s == "feedback-ending-sessions 1")
                .unwrap();
            assert!(sending < ending);
        }

        #[test]
        fn a_refused_start_goes_stuck_without_saying_a_session_may_exist() {
            let mut world = World::reviewed();
            world.rig.worker.on_create(Ok(fresh()));
            world
                .rig
                .worker
                .on_start_session(Err(WorkerError::Failed("no such model".into())));
            assert!(matches!(world.go(), FeedbackStep::Decided { .. }));
            assert_eq!(world.go(), FeedbackStep::Preparing(Wait::Workspace));
            world.rig.view = round_view();
            let FeedbackStep::Stuck { reason } = world.go() else {
                panic!("expected stuck");
            };
            assert!(!reason.contains(messages::SESSION_MAY_EXIST), "{reason}");
            assert_eq!(world.rig.sim.labels(), ["factory:stuck"]);
            // The workspace is kept for a person to look at.
            assert!(world.archives().is_empty());
        }

        #[test]
        fn a_refused_second_start_says_a_session_may_exist_under_the_feedback_name() {
            let mut world = World::reviewed();
            world.rig.worker.on_create(Ok(fresh()));
            world
                .rig
                .worker
                .on_start_session(Err(WorkerError::Unreachable("down".into())));
            world
                .rig
                .worker
                .on_start_session(Err(WorkerError::Failed("no such model".into())));
            assert!(matches!(world.go(), FeedbackStep::Decided { .. }));
            assert_eq!(world.go(), FeedbackStep::Preparing(Wait::Workspace));
            world.rig.view = round_view();
            assert!(world.try_go().is_err(), "the machine could not be reached");
            let FeedbackStep::Stuck { reason } = world.go() else {
                panic!("expected stuck");
            };
            assert!(reason.contains(messages::SESSION_MAY_EXIST), "{reason}");
            assert!(
                reason.contains(&format!(
                    "(launch command id foundry:{}:feedback-1)",
                    world.run
                )),
                "{reason}"
            );
        }

        #[test]
        fn a_setup_that_runs_is_waited_for_and_one_that_failed_goes_stuck_with_its_tail() {
            let mut world = World::reviewed();
            world.rig.worker.on_create(Ok(fresh()));
            assert!(matches!(world.go(), FeedbackStep::Decided { .. }));
            assert_eq!(world.go(), FeedbackStep::Preparing(Wait::Workspace));
            let with_setup = |state, code| {
                fleet()
                    .workspace(workspace_record(
                        fresh(),
                        "fac/8-1-f1",
                        Some(setup_terminal()),
                    ))
                    .terminal(terminal_record(state, code))
                    .build()
            };
            world.rig.view = with_setup(TerminalState::Running, None);
            assert_eq!(world.go(), FeedbackStep::Preparing(Wait::Setup));
            assert!(world.starts().is_empty());
            world.rig.view = with_setup(TerminalState::Exited, Some(2));
            world.rig.worker.on_tail(Ok("npm ERR! boom".into()));
            let FeedbackStep::Stuck { .. } = world.go() else {
                panic!("expected stuck");
            };
            assert!(world.starts().is_empty());
            assert_eq!(world.rig.sim.labels(), ["factory:stuck"]);
            let comments = world.rig.sim.comments().join("\n");
            assert!(comments.contains("npm ERR! boom"), "{comments}");
            // Its workspace is kept for a person to look at.
            assert!(world.archives().is_empty());
        }

        #[test]
        fn a_text_changed_after_the_decision_stops_the_round_before_its_session_starts() {
            let mut world = World::reviewed();
            world.rig.worker.on_create(Ok(fresh()));
            assert!(matches!(world.go(), FeedbackStep::Decided { .. }));
            assert_eq!(world.go(), FeedbackStep::Preparing(Wait::Workspace));
            world.rig.view = round_view();
            world.rig.sim.set_approval(approval_answer(
                LABELED_AT,
                "tom",
                &[("2026-10-04T10:30:00Z", "eve")],
            ));
            assert_eq!(
                world.go(),
                FeedbackStep::Stuck {
                    reason: messages::CHANGED_DURING_BUILD.into()
                }
            );
            assert!(world.starts().is_empty());
            assert_eq!(world.rig.sim.labels(), ["factory:stuck"]);
        }

        #[test]
        fn a_closed_issue_ends_a_round_under_way_and_ends_the_sessions_of_its_workspace() {
            let world = World::reviewed();
            world.rig.worker.on_create(Ok(fresh()));
            assert!(matches!(world.go(), FeedbackStep::Decided { .. }));
            // Before any workspace is asked for there is nothing to end.
            world.rig.sim.set_closed(true);
            assert_eq!(world.go(), FeedbackStep::Abandoned);
            assert!(world.rig.worker.calls().is_empty());
            assert!(world.rig.finished(&world.run));
            // With the workspace made, its sessions are ended: a start may have gone out.
            let mut made = World::reviewed();
            made.rig.worker.on_create(Ok(fresh()));
            assert!(matches!(made.go(), FeedbackStep::Decided { .. }));
            assert_eq!(made.go(), FeedbackStep::Preparing(Wait::Workspace));
            made.rig.view = round_view();
            made.rig.sim.set_closed(true);
            assert_eq!(made.go(), FeedbackStep::Abandoned);
            assert!(
                made.rig
                    .worker
                    .calls()
                    .contains(&Call::EndSessions { workspace: fresh() })
            );
        }

        #[test]
        fn a_merge_that_closed_the_issue_ends_a_round_under_way() {
            let world = World::reviewed();
            assert!(matches!(world.go(), FeedbackStep::Decided { .. }));
            world.rig.sim.set_closed(true);
            world.rig.sim.set_pull(
                12,
                SimPull {
                    state: "MERGED",
                    draft: false,
                    head: BRANCH.into(),
                    base: "main".into(),
                    cross: false,
                },
            );
            assert_eq!(world.go(), FeedbackStep::Ended);
            let end = world.events().pop().unwrap();
            assert_eq!(end.event, "finished");
            assert_eq!(end.data["pr_end"], "merged");
        }

        #[test]
        fn a_stop_asked_for_leaves_a_round_under_way_to_the_stop_step() {
            let world = World::reviewed();
            assert!(matches!(world.go(), FeedbackStep::Decided { .. }));
            world.rig.write(
                &world.run,
                Event::StopRequested {
                    by: "operator".into(),
                },
            );
            let before = world.count();
            assert_eq!(world.go(), FeedbackStep::None);
            assert_eq!(world.count(), before);
            assert!(world.rig.worker.calls().is_empty());
        }

        #[test]
        fn a_session_idle_with_no_prompt_a_pass_later_is_sent_the_first_message_again_by_count() {
            let mut world = World::reviewed();
            world.round();
            world.rig.view = session_view(SessionState::Idle, None);
            world.rig.worker.on_send(Ok(0));
            assert_eq!(
                world.go(),
                FeedbackStep::Resent {
                    session: round_session()
                }
            );
            assert_eq!(
                world.sends(),
                vec![Call::Send {
                    session: round_session(),
                    message: block(&[review(11, "2026-10-04T11:00:00Z")]),
                    only_if_prompts: Some(0),
                }]
            );
            assert!(
                world
                    .steps()
                    .contains(&"feedback-resending 1 0".to_string())
            );
            // The worker decides: a count that rose says the message was typed.
            world
                .rig
                .worker
                .on_send(Err(crate::sessions::factory::SendRefused::CountRose {
                    now: 1,
                }));
            assert_eq!(world.go(), FeedbackStep::None);
            // A session that took its message, as a build session does for a whole gate, gets
            // nothing again.
            world.rig.view = session_view(SessionState::Idle, Some((0, 1)));
            let sends = world.sends().len();
            assert_eq!(world.go(), FeedbackStep::None);
            assert_eq!(world.sends().len(), sends);
            world.rig.view = session_view(SessionState::Idle, Some((1, 1)));
            assert_eq!(world.go(), FeedbackStep::None);
            assert_eq!(world.sends().len(), sends);
        }

        #[test]
        fn a_new_session_is_followed_by_building_like_any_other_and_its_pull_request_is_reported_again()
         {
            use crate::factory::runs::building::{BuildStep, advance as build};
            let mut world = World::reviewed();
            world.round();
            // The same pull request, a draft again, is what the session reports.
            world.rig.sim.set_pull(
                12,
                SimPull {
                    state: "OPEN",
                    draft: true,
                    head: BRANCH.into(),
                    base: "main".into(),
                    cross: false,
                },
            );
            world.rig.view = session_view(SessionState::Working, Some((0, 1)));
            let step = build(&world.ctx(), &world.run).unwrap();
            assert_eq!(step, BuildStep::Running);
            world.rig.view = session_view(SessionState::Idle, Some((1, 1)));
            world
                .rig
                .worker
                .on_turn(Ok(crate::sessions::turns::TurnText {
                    message: format!("Fixed it.\nFACTORY-DONE: {URL12}"),
                    question: None,
                }));
            let step = build(&world.ctx(), &world.run).unwrap();
            assert_eq!(
                step,
                BuildStep::ReadyForGate {
                    pr: 12,
                    url: URL12.into()
                }
            );
            // Reported again for the gate, and kept once; the round is no gate attempt.
            let seen = world.seen();
            assert_eq!(seen.opened, [12]);
            assert!(seen.pr_reported);
            assert!(seen.sending.is_none());
            assert_eq!(
                world
                    .events()
                    .iter()
                    .filter(|event| event.event == "pr_opened")
                    .count(),
                2
            );
        }

        #[test]
        fn forward_does_not_send_an_issue_comment_again_that_went_as_feedback() {
            let mut world = World::reviewed();
            world.hub.set_activity(activity(vec![issue_comment(
                "42",
                "2026-10-04T13:00:00Z",
                "update the docs",
            )]));
            world.round();
            // The run is `building` again, its session idle with its first turn read, and the
            // activity still lists the comment.
            world.rig.view = session_view(SessionState::Idle, Some((1, 1)));
            world.rig.step(&world.run, "turn-read 1");
            assert_eq!(forward_in(&world), outcome(0, false));
            assert!(world.sends().is_empty());
            // A comment that was not feedback is still sent.
            world.hub.set_activity(activity(vec![
                issue_comment("42", "2026-10-04T13:00:00Z", "update the docs"),
                issue_comment("43", "2026-10-04T14:00:00Z", "and the changelog"),
            ]));
            world.rig.worker.on_send(Ok(1));
            assert_eq!(forward_in(&world), outcome(1, false));
            let Call::Send { message, .. } = world.sends().remove(0) else {
                unreachable!()
            };
            assert!(message.body.unwrap().contains("and the changelog"));
        }

        #[test]
        fn relaunches_on_a_run_base_and_records_it() {
            // The runner's `a run whose launch recorded a base other than the project's`: the
            // round's launch line carries the run's base, not the project's.
            let mut rig = Rig::new();
            rig.view = fleet().build();
            let hub = Hub::new(rig.sim.clone());
            let run = run_id(1);
            pr_open_on(&rig, &run, &[], "feature/x");
            rig.sim.set_pull(
                12,
                SimPull {
                    state: "OPEN",
                    draft: false,
                    head: BRANCH.into(),
                    base: "feature/x".into(),
                    cross: false,
                },
            );
            hub.set_feedback(vec![review(11, "2026-10-04T11:00:00Z")]);
            let mut world = World { rig, hub, run };
            world.round();
            let bases: Vec<_> = world
                .launches()
                .iter()
                .map(|launch| launch["base"].clone())
                .collect();
            assert_eq!(bases, [json!("feature/x"), json!("feature/x")]);
            assert_eq!(world.seen().base.as_deref(), Some("feature/x"));
        }
    }

    mod feedback_the_budgets_start_again {
        use super::*;

        #[test]
        fn the_time_limit_counts_from_the_relaunch() {
            let mut world = World::reviewed();
            world.round();
            let moved = world
                .events()
                .into_iter()
                .rev()
                .find(|event| event.event == "state_changed")
                .unwrap()
                .ms;
            // The pull request waited for review for hours, and none of it counts: the time
            // `building::time_limit` measures is the worked time of the stints that ended plus
            // the one that runs, and the run's stint begins at the move to `building`.
            let seen = world.seen();
            assert!(seen.worked_ms < 60_000, "{}", seen.worked_ms);
            assert_eq!(seen.working_since, Some(moved));
            let limit_ms = 90 * 60_000;
            let worked = |now: u64| seen.worked_ms + now.saturating_sub(moved);
            assert!(worked(moved + 89 * 60_000) <= limit_ms);
            assert!(worked(moved + 91 * 60_000) > limit_ms);
            // And the wait before the round is not in it, however long it was.
            world.pad(100);
            assert_eq!(world.seen().worked_ms, seen.worked_ms);
        }

        #[test]
        fn a_round_is_a_return_to_building_after_pr_open_which_is_where_the_gate_starts_its_budget_again()
         {
            // `gating` reads a round as beginning at each `state_changed` to `building` that
            // follows `pr-open`, and `progress` zeroes the worked time at the same line: the
            // round writes exactly that line, once, last of its move.
            let mut world = World::reviewed();
            world.round();
            let moves: Vec<_> = world
                .events()
                .into_iter()
                .filter(|event| event.event == "state_changed")
                .map(|event| (event.data["from"].clone(), event.data["to"].clone()))
                .collect();
            assert_eq!(moves.last(), Some(&(json!("pr-open"), json!("building"))),);
            assert_eq!(
                moves
                    .iter()
                    .filter(|(from, to)| *from == json!("pr-open") && *to == json!("building"))
                    .count(),
                1
            );
        }
    }

    // ===== forward =====

    const EDITED: &str = "2026-10-04T10:15:00Z";

    fn the_session() -> SessionId {
        SessionId(uuid::Uuid::from_u128(0x5e5))
    }

    /// The build session idle in the build workspace, its newest turn (turn 1, one prompt) read.
    fn building_view(state: SessionState, turn: Option<(u64, u64)>) -> FleetView {
        let mut builder = fleet()
            .workspace(workspace_record(old(), "fac/8-1", None))
            .session(Record {
                owner: m1(),
                version: 1,
                deleted: false,
                body: foundry_proto::record::Session {
                    id: the_session(),
                    workspace: old(),
                    terminal: TerminalId(uuid::Uuid::from_u128(0x5e6)),
                    model: "opus".into(),
                    state,
                },
            });
        if let Some((turn, prompts)) = turn {
            builder = builder.turn(foundry_proto::factory::SessionTurn {
                session: the_session(),
                workspace: old(),
                project: project_id(),
                turn,
                prompts,
                question: false,
            });
        }
        builder.build()
    }

    /// A building run whose session is idle, its newest turn read; GitHub hands back a comment
    /// and an edit of the body, so a missing guard would send them.
    fn forwarding() -> World {
        let mut rig = Rig::new();
        rig.view = building_view(SessionState::Idle, Some((1, 1)));
        let hub = Hub::new(rig.sim.clone());
        let run = run_id(1);
        started(&rig, &run, "main");
        rig.step(&run, "turn-read 1");
        rig.sim.set_labels(&["factory:building"]);
        rig.sim.set_assignees(&[LOGIN]);
        World { rig, hub, run }
    }

    fn instructor_activity(world: &World) {
        script(
            world,
            vec![bare_comment("101", "tom", "one")],
            Some(edit(EDITED, "tom", "new")),
        );
    }

    fn bare_comment(id: &str, author: &str, body: &str) -> Comment {
        Comment {
            id: id.into(),
            author: author.into(),
            created_at: "2026-10-04T11:00:00Z".into(),
            body: body.into(),
        }
    }

    fn edit(at: &str, editor: &str, body: &str) -> crate::factory::github::BodyEdit {
        crate::factory::github::BodyEdit {
            edited_at: at.into(),
            editor: editor.into(),
            body: body.into(),
        }
    }

    /// What GitHub says the instructors did from now on.
    fn script(
        world: &World,
        comments: Vec<Comment>,
        body_edit: Option<crate::factory::github::BodyEdit>,
    ) {
        world.hub.set_activity(Activity {
            comments,
            body_edit,
            dropped: vec![],
        });
    }

    fn script_dropped(world: &World, dropped: &[(&str, crate::factory::github::DropReason)]) {
        world.hub.set_activity(Activity {
            comments: vec![],
            body_edit: None,
            dropped: dropped
                .iter()
                .map(|(id, reason)| crate::factory::github::Dropped {
                    id: (*id).into(),
                    reason: *reason,
                })
                .collect(),
        });
    }

    /// Every send the worker is asked for is taken, as a session that is idle takes it.
    fn allow(world: &World, sends: usize) {
        for _ in 0..sends {
            world.rig.worker.on_send(Ok(1));
        }
    }

    fn forward_in(world: &World) -> ForwardStep {
        forward(&world.ctx(), &world.run).unwrap()
    }

    fn outcome(comments: usize, body_edit: bool) -> ForwardStep {
        ForwardStep {
            comments,
            body_edit,
            skipped: None,
            held_back: false,
        }
    }

    fn skipped(why: Skip) -> ForwardStep {
        ForwardStep {
            comments: 0,
            body_edit: false,
            skipped: Some(why),
            held_back: false,
        }
    }

    fn comment_send(author: &str, body: &str) -> Call {
        Call::Send {
            session: the_session(),
            message: messages::Message::new(
                FORWARD_COMMENT_LINE.into(),
                messages::comment_text(REPO, ISSUE, author, body),
            ),
            only_if_prompts: Some(1),
        }
    }

    fn body_send(editor: &str, body: &str) -> Call {
        Call::Send {
            session: the_session(),
            message: messages::Message::new(
                FORWARD_BODY_LINE.into(),
                messages::body_edit_text(REPO, ISSUE, editor, body),
            ),
            only_if_prompts: Some(1),
        }
    }

    fn comment_forwarded_line(id: &str) -> Event {
        Event::CommentForwarded {
            comment_id: id.into(),
        }
    }

    /// The ledger's events from the first `n`, as (event, step text or data) pairs.
    fn after(world: &World, n: usize) -> Vec<(String, Value)> {
        world
            .events()
            .into_iter()
            .skip(n)
            .map(|event| {
                let shown = event
                    .data
                    .get("step")
                    .cloned()
                    .unwrap_or_else(|| event.data.clone());
                (event.event, shown)
            })
            .collect()
    }

    fn forwarded_ids(world: &World) -> Vec<String> {
        world
            .events()
            .into_iter()
            .filter(|event| event.event == "comment_forwarded")
            .map(|event| event.data["comment_id"].as_str().unwrap().to_owned())
            .collect()
    }

    /// The run's first line cut to whole seconds, as the activity is asked for.
    fn since_first(world: &World) -> String {
        let first = world.events()[0].ms;
        let text = crate::factory::ledger::format_ts(first);
        format!("{}Z", &text[..19])
    }

    fn activity_reads(world: &World) -> Vec<String> {
        world.hub.reads()
    }

    mod forward_comments {
        use super::*;

        #[test]
        fn sends_each_new_comment_once_in_order_and_records_it() {
            let world = forwarding();
            script(
                &world,
                vec![
                    bare_comment("101", "tom", "Use serde here."),
                    bare_comment("102", "tom", "And add a test."),
                ],
                None,
            );
            allow(&world, 2);
            let before = world.count();
            // One message at a time per session: the session's count of prompts says whether the
            // first was typed, and it is read afresh each pass.
            assert_eq!(forward_in(&world), outcome(1, false));
            assert_eq!(forward_in(&world), outcome(1, false));
            assert_eq!(
                world.sends(),
                vec![
                    comment_send("tom", "Use serde here."),
                    comment_send("tom", "And add a test."),
                ]
            );
            assert_eq!(forwarded_ids(&world), ["101", "102"]);
            assert_eq!(
                activity_reads(&world),
                vec![
                    format!("activity {ISSUE} {} tom", since_first(&world)),
                    format!("activity {ISSUE} {} tom", since_first(&world)),
                ]
            );
            // Nothing is written on GitHub: the forward has no write to make.
            assert!(world.writes().is_empty());
            assert!(world.count() > before);
        }

        #[test]
        fn writes_each_line_under_the_forward_step_with_the_reason_that_names_it() {
            let world = forwarding();
            script(
                &world,
                vec![bare_comment("101", "tom", "one")],
                Some(edit(EDITED, "tom", "new")),
            );
            allow(&world, 2);
            let before = world.count();
            forward_in(&world);
            forward_in(&world);
            let causes: Vec<String> = world
                .events()
                .into_iter()
                .skip(before)
                .map(|event| {
                    let cause = event.cause.unwrap();
                    format!(
                        "{} {}",
                        cause["step"].as_str().unwrap(),
                        cause["reason"].as_str().unwrap()
                    )
                })
                .collect();
            assert_eq!(
                causes,
                [
                    "forward send-intent",
                    "forward send-typed",
                    "forward comment-forwarded",
                    "forward send-intent",
                    "forward send-typed",
                    "forward body-edit-forwarded",
                ]
            );
        }

        #[test]
        fn a_second_forward_sends_nothing_and_writes_nothing() {
            let world = forwarding();
            script(
                &world,
                vec![
                    bare_comment("101", "tom", "one"),
                    bare_comment("102", "tom", "two"),
                ],
                None,
            );
            allow(&world, 2);
            forward_in(&world);
            forward_in(&world);
            let count = world.count();
            let sends = world.sends().len();
            assert_eq!(forward_in(&world), outcome(0, false));
            assert_eq!(world.sends().len(), sends);
            assert_eq!(world.count(), count);
            assert_eq!(activity_reads(&world).len(), 3);
        }

        #[test]
        fn a_comment_the_ledger_already_has_is_not_sent_again_but_a_new_one_after_it_is() {
            let world = forwarding();
            world.rig.write(&world.run, comment_forwarded_line("101"));
            script(
                &world,
                vec![
                    bare_comment("101", "tom", "one"),
                    bare_comment("102", "tom", "two"),
                ],
                None,
            );
            allow(&world, 1);
            assert_eq!(forward_in(&world), outcome(1, false));
            assert_eq!(world.sends(), vec![comment_send("tom", "two")]);
            assert_eq!(forwarded_ids(&world), ["101", "102"]);
        }

        #[test]
        fn a_comment_forwarded_out_of_order_is_still_skipped_by_its_id() {
            // The ledger knows 102 but not 101: 101 is sent, 102 is not.
            let world = forwarding();
            world.rig.write(&world.run, comment_forwarded_line("102"));
            script(
                &world,
                vec![
                    bare_comment("101", "tom", "one"),
                    bare_comment("102", "tom", "two"),
                ],
                None,
            );
            allow(&world, 2);
            assert_eq!(forward_in(&world), outcome(1, false));
            assert_eq!(forward_in(&world), outcome(0, false));
            assert_eq!(world.sends(), vec![comment_send("tom", "one")]);
        }

        #[test]
        fn sends_a_comment_id_that_github_lists_twice_only_once() {
            let world = forwarding();
            script(
                &world,
                vec![
                    bare_comment("101", "tom", "one"),
                    bare_comment("101", "tom", "one"),
                ],
                None,
            );
            allow(&world, 2);
            assert_eq!(forward_in(&world), outcome(1, false));
            assert_eq!(forward_in(&world), outcome(0, false));
            assert_eq!(world.sends(), vec![comment_send("tom", "one")]);
        }

        #[test]
        fn since_is_the_first_ledger_line_cut_to_whole_seconds_and_the_instructors_are_the_configs()
        {
            let mut world = forwarding();
            world.rig.config.instructors = vec!["tom".into(), "Amy".into()];
            assert_eq!(forward_in(&world), outcome(0, false));
            // The run's first line is at 10:13:16.123: cut, not rounded.
            assert_eq!(
                activity_reads(&world),
                vec![format!("activity {ISSUE} 2026-10-04T10:13:16Z tom,Amy")]
            );
            assert!(world.sends().is_empty());
        }

        #[test]
        fn since_stays_the_runs_start_after_comments_were_forwarded() {
            let world = forwarding();
            script(&world, vec![bare_comment("101", "tom", "one")], None);
            allow(&world, 1);
            forward_in(&world);
            forward_in(&world);
            let since = since_first(&world);
            assert_eq!(
                activity_reads(&world),
                vec![
                    format!("activity {ISSUE} {since} tom"),
                    format!("activity {ISSUE} {since} tom"),
                ]
            );
        }

        #[test]
        fn uses_the_configs_spelling_of_the_repo_in_the_message() {
            let mut world = forwarding();
            world.rig.project.repo = "RawPHP/SoftFac".into();
            script(&world, vec![bare_comment("101", "tom", "one")], None);
            allow(&world, 1);
            assert_eq!(forward_in(&world), outcome(1, false));
            let Call::Send { message, .. } = world.sends().remove(0) else {
                unreachable!()
            };
            let body = message.body.unwrap();
            assert!(
                body.contains("Forwarded instructor comment on RawPHP/SoftFac#8 from"),
                "{body}"
            );
        }
    }

    mod forward_a_body_edit {
        use super::*;

        #[test]
        fn is_sent_once_with_its_exact_message_and_step() {
            let world = forwarding();
            script(
                &world,
                vec![],
                Some(edit(EDITED, "tom", "## Outcome\nthe new body")),
            );
            allow(&world, 1);
            let before = world.count();
            assert_eq!(forward_in(&world), outcome(0, true));
            assert_eq!(
                world.sends(),
                vec![body_send("tom", "## Outcome\nthe new body")]
            );
            let written = after(&world, before);
            assert_eq!(
                written.last().unwrap(),
                &(
                    "step_changed".to_owned(),
                    json!(format!("body-edit-forwarded {EDITED}"))
                )
            );
        }

        #[test]
        fn the_same_edit_again_sends_nothing_and_a_later_edit_is_sent_again() {
            let world = forwarding();
            script(&world, vec![], Some(edit(EDITED, "tom", "v1")));
            allow(&world, 2);
            forward_in(&world);
            let count = world.count();
            assert_eq!(forward_in(&world), outcome(0, false));
            assert_eq!(world.sends().len(), 1);
            assert_eq!(world.count(), count);
            let later = "2026-10-04T10:20:00Z";
            script(&world, vec![], Some(edit(later, "tom", "v2")));
            assert_eq!(forward_in(&world), outcome(0, true));
            assert_eq!(
                world.sends(),
                vec![body_send("tom", "v1"), body_send("tom", "v2")]
            );
            assert_eq!(
                world.steps_of("body-edit-forwarded"),
                [
                    format!("body-edit-forwarded {EDITED}"),
                    format!("body-edit-forwarded {later}")
                ]
            );
        }

        #[test]
        fn goes_in_after_the_comments_of_its_call() {
            let world = forwarding();
            script(
                &world,
                vec![
                    bare_comment("101", "tom", "one"),
                    bare_comment("102", "tom", "two"),
                ],
                Some(edit(EDITED, "tom", "new")),
            );
            allow(&world, 3);
            assert_eq!(forward_in(&world), outcome(1, false));
            assert_eq!(forward_in(&world), outcome(1, false));
            assert_eq!(forward_in(&world), outcome(0, true));
            assert_eq!(
                world.sends(),
                vec![
                    comment_send("tom", "one"),
                    comment_send("tom", "two"),
                    body_send("tom", "new"),
                ]
            );
        }

        #[test]
        fn with_a_time_that_is_not_githubs_form_is_refused_before_anything_is_sent() {
            for bad in [
                "",
                "x",
                "2026-10-04 10:15:00",
                "2026-10-04T10:15:00.123Z",
                "2026-10-04T10:15:00+00:00",
                "2026-10-04T10:15:00Z\n",
                "2026-10-04T10:15:00Z:evil",
                "2026-10-04T10:15:0\u{0}Z",
            ] {
                let world = forwarding();
                script(
                    &world,
                    vec![bare_comment("101", "tom", "one")],
                    Some(edit(bad, "tom", "new")),
                );
                allow(&world, 2);
                let count = world.count();
                let error = forward(&world.ctx(), &world.run).unwrap_err();
                assert!(
                    error.to_string().contains("YYYY-MM-DDTHH:MM:SSZ"),
                    "{bad:?}: {error}"
                );
                assert!(world.sends().is_empty(), "{bad:?}");
                assert_eq!(world.count(), count, "{bad:?}");
            }
        }

        #[test]
        fn is_not_sent_twice_for_a_step_that_only_looks_like_a_forwarded_edit() {
            let world = forwarding();
            world
                .rig
                .step(&world.run, "body-edit-forwarded 2026-10-04T10:15:00");
            world
                .rig
                .step(&world.run, "body-edit-forwarded 2026-10-04T10:15:00Z ");
            world
                .rig
                .step(&world.run, &format!("body-edit-forwarded{EDITED}"));
            script(&world, vec![], Some(edit(EDITED, "tom", "new")));
            allow(&world, 1);
            assert_eq!(forward_in(&world), outcome(0, true));
        }
    }

    /// A building run whose ledger holds `extra` after the build started, with an instructor's
    /// comment and edit that a missing guard would send.
    fn guarded(extra: impl FnOnce(&World)) -> World {
        let world = forwarding();
        extra(&world);
        instructor_activity(&world);
        allow(&world, 4);
        world
    }

    /// `forward` skips the run for `why`: no read of the activity, no send and no write.
    fn assert_skips(world: &World, why: Skip) {
        let count = world.count();
        assert_eq!(forward_in(world), skipped(why), "{why:?}");
        assert!(activity_reads(world).is_empty(), "{why:?}");
        assert!(world.sends().is_empty(), "{why:?}");
        assert!(world.rig.worker.calls().is_empty(), "{why:?}");
        assert_eq!(world.count(), count, "{why:?}");
    }

    mod forward_guards_and_failures {
        use super::*;

        #[test]
        fn skips_a_finished_run() {
            let world = guarded(|world| {
                world
                    .rig
                    .ledger
                    .finish(
                        &world.run,
                        crate::factory::ledger::events::Outcome::Stuck,
                        None,
                        None,
                        &Cause::new(CauseStep::Gate, "seed"),
                        None,
                    )
                    .unwrap();
            });
            assert_skips(&world, Skip::Finished);
        }

        #[test]
        fn skips_a_run_with_no_session() {
            let rig = Rig::new();
            let run = run_id(1);
            rig.write(
                &run,
                Event::Claimed {
                    machine: m1().to_string(),
                    github_user: LOGIN.into(),
                },
            );
            rig.moved(&run, State::Ready, State::Triage);
            rig.moved(&run, State::Triage, State::Building);
            let hub = Hub::new(rig.sim.clone());
            let world = World { rig, hub, run };
            instructor_activity(&world);
            assert_skips(&world, Skip::NoSession);
        }

        #[test]
        fn skips_every_state_but_building() {
            // Triage, with a session.
            let rig = Rig::new();
            let run = run_id(1);
            started(&rig, &run, "main");
            let hub = Hub::new(rig.sim.clone());
            let world = World { rig, hub, run };
            world
                .rig
                .moved(&world.run, State::Building, State::NeedsInfo);
            instructor_activity(&world);
            assert_skips(&world, Skip::NotBuilding);
            // Stuck.
            let world = guarded(|world| {
                world.rig.write(
                    &world.run,
                    Event::Stuck {
                        reason: "x".into(),
                        branch: None,
                        commits_ahead: None,
                    },
                );
                world.rig.moved(&world.run, State::Building, State::Stuck);
            });
            assert_skips(&world, Skip::NotBuilding);
            // Waiting for review.
            let world = guarded(|world| {
                world.rig.write(
                    &world.run,
                    Event::PrOpened {
                        pr: 12,
                        url: URL12.into(),
                    },
                );
                passed_gate(&world.rig, &world.run);
                world.rig.moved(&world.run, State::Building, State::PrOpen);
            });
            assert_skips(&world, Skip::NotBuilding);
        }

        #[test]
        fn skips_a_run_whose_gate_pass_is_recorded() {
            let world = guarded(|world| {
                world.rig.write(
                    &world.run,
                    Event::PrOpened {
                        pr: 12,
                        url: URL12.into(),
                    },
                );
                passed_gate(&world.rig, &world.run);
            });
            assert_skips(&world, Skip::NotBuilding);
        }

        #[test]
        fn skips_a_run_an_operator_asked_to_stop() {
            let world = guarded(|world| {
                world.rig.write(
                    &world.run,
                    Event::StopRequested {
                        by: "operator".into(),
                    },
                );
            });
            assert_skips(&world, Skip::NotBuilding);
        }

        #[test]
        fn skips_a_transition_to_stuck_or_needs_info_that_was_cut_short() {
            // The decision is in the ledger but `state_changed`, the commit marker, is not.
            let world = guarded(|world| {
                world.rig.write(
                    &world.run,
                    Event::Stuck {
                        reason: "x".into(),
                        branch: None,
                        commits_ahead: None,
                    },
                );
            });
            assert_skips(&world, Skip::NotBuilding);
            let world = guarded(|world| {
                world.rig.write(
                    &world.run,
                    Event::QuestionsPosted {
                        questions: "1. why".into(),
                    },
                );
            });
            assert_skips(&world, Skip::NotBuilding);
        }

        #[test]
        fn skips_a_run_whose_gate_send_is_pending() {
            // A gate attempt was being sent into the session and the ledger does not say it
            // arrived: a message sent now could be mistaken for the proof that it did.
            let world = guarded(|world| {
                world.rig.step(&world.run, "gate-sending 1 1");
            });
            assert_skips(&world, Skip::GateSendPending);
            // Once `gate_sent_back` is written the run is forwarded to again.
            let world = guarded(|world| {
                world.rig.step(&world.run, "gate-sending 1 1");
                world.rig.write(
                    &world.run,
                    Event::GateSentBack {
                        attempt: 1,
                        run_ordinal: 1,
                    },
                );
            });
            assert_eq!(forward_in(&world), outcome(1, false));
        }

        #[test]
        fn skips_a_run_with_a_relaunch_or_an_interruption_decided() {
            // A round being set up, an owed resume: the steps that carry them out own the run.
            let world = guarded(|world| {
                world.rig.write(
                    &world.run,
                    Event::FeedbackRelaunching {
                        n: 1,
                        branch: BRANCH.into(),
                        items: vec!["review:11".into()],
                        message: "m".into(),
                    },
                );
            });
            assert_skips(&world, Skip::Relaunching);
            let world = guarded(|world| {
                world.rig.write(
                    &world.run,
                    Event::SessionInterrupted {
                        resumes: 1,
                        relaunch: false,
                        run_ordinal: None,
                        branch: None,
                        t3_error: None,
                        ended_in_pass: 0,
                    },
                );
            });
            assert_skips(&world, Skip::Relaunching);
        }

        #[test]
        fn refuses_a_run_of_another_repo_before_touching_anything() {
            let world = guarded(|_| {});
            let other = RunId::new("rawphp/other", 8, 1).unwrap();
            let step = forward(&world.ctx(), &other).unwrap();
            assert_eq!(step, skipped(Skip::OtherRepo));
            assert_eq!(world.hub.counted(), 0);
            assert!(world.rig.worker.calls().is_empty());
        }

        #[test]
        fn compares_the_repo_case_insensitively() {
            let mut world = guarded(|_| {});
            world.rig.project.repo = "RawPHP/SoftFac".into();
            assert_eq!(forward_in(&world), outcome(1, false));
        }

        #[test]
        fn on_a_run_with_no_events_is_skipped_as_having_no_session() {
            // The runner's `on a run with no file is a ledger error`: a run's events are a read
            // of the store, and a run with none is one Foundry never ran.
            let rig = Rig::new();
            let hub = Hub::new(rig.sim.clone());
            let world = World {
                rig,
                hub,
                run: run_id(1),
            };
            assert_eq!(forward_in(&world), skipped(Skip::NoSession));
            assert_eq!(world.hub.counted(), 0);
            assert!(world.rig.worker.calls().is_empty());
        }

        #[test]
        fn a_failing_activity_read_is_an_error_with_nothing_sent_or_written() {
            let world = guarded(|_| {});
            let count = world.count();
            world.hub.fail_at(world.hub.counted(), false);
            assert!(forward(&world.ctx(), &world.run).is_err());
            assert!(world.sends().is_empty());
            assert_eq!(world.count(), count);
            // The next call, with GitHub back, does the work.
            assert_eq!(forward_in(&world), outcome(1, false));
        }

        #[test]
        fn a_failed_send_transport_stops_the_call_and_the_rest_is_sent_in_order_under_the_same_count()
         {
            let world = forwarding();
            script(
                &world,
                vec![
                    bare_comment("101", "tom", "one"),
                    bare_comment("102", "tom", "two"),
                    bare_comment("103", "tom", "three"),
                ],
                None,
            );
            // The first send times out: it may have been typed, so nothing is recorded as sent
            // and no other message goes before it is known.
            world
                .rig
                .worker
                .on_send(Err(crate::sessions::factory::SendRefused::Failed {
                    reason: "timed out".into(),
                    prompts: None,
                }));
            assert!(forward(&world.ctx(), &world.run).is_err());
            assert!(forwarded_ids(&world).is_empty());
            assert_eq!(
                world.steps_of("send-intent"),
                ["send-intent forward-comment 101 1"]
            );
            allow(&world, 3);
            // The same message is sent again under the same count, then the rest in order.
            assert_eq!(forward_in(&world), outcome(1, false));
            assert_eq!(forward_in(&world), outcome(1, false));
            assert_eq!(forward_in(&world), outcome(1, false));
            assert_eq!(forwarded_ids(&world), ["101", "102", "103"]);
            let ids: Vec<_> = world
                .sends()
                .into_iter()
                .map(|call| match call {
                    Call::Send { message, .. } => message.body.unwrap(),
                    _ => unreachable!(),
                })
                .collect();
            assert!(ids[0].contains("one") && ids[1].contains("one"), "{ids:?}");
        }

        #[test]
        fn a_send_t3_refuses_is_dropped_with_a_notice_and_the_call_goes_on() {
            let world = forwarding();
            script(&world, vec![bare_comment("101", "tom", "one")], None);
            world
                .rig
                .worker
                .on_send(Err(crate::sessions::factory::SendRefused::BodyWithoutPath));
            assert_eq!(forward_in(&world), outcome(0, false));
            assert_eq!(world.sends(), vec![comment_send("tom", "one")]);
            let notice = world.rig.sim.comments().join("\n");
            assert!(notice.contains("Foundry refused it"), "{notice}");
            assert!(world.steps().contains(&"comment-dropped 101".to_string()));
            let cause = world.events().pop().unwrap().cause.unwrap();
            assert_eq!(cause["reason"], "comment-dropped");
            // Never sent again, said once.
            let count = world.count();
            let notices = world.rig.sim.comments().len();
            assert_eq!(forward_in(&world), outcome(0, false));
            assert_eq!(world.sends().len(), 1);
            assert_eq!(world.rig.sim.comments().len(), notices);
            assert_eq!(world.count(), count);
        }

        #[test]
        fn a_failed_edit_send_leaves_the_comments_recorded_and_the_edit_for_the_next_call() {
            let world = forwarding();
            instructor_activity(&world);
            allow(&world, 1);
            assert_eq!(forward_in(&world), outcome(1, false));
            world
                .rig
                .worker
                .on_send(Err(crate::sessions::factory::SendRefused::Failed {
                    reason: "timed out".into(),
                    prompts: None,
                }));
            assert!(forward(&world.ctx(), &world.run).is_err());
            assert_eq!(forwarded_ids(&world), ["101"]);
            assert!(world.steps_of("body-edit-forwarded").is_empty());
            allow(&world, 1);
            assert_eq!(forward_in(&world), outcome(0, true));
            assert_eq!(
                world.sends(),
                vec![
                    comment_send("tom", "one"),
                    body_send("tom", "new"),
                    body_send("tom", "new"),
                ]
            );
            assert_eq!(world.steps_of("body-edit-forwarded").len(), 1);
        }

        #[test]
        fn a_thread_that_no_longer_exists_skips_the_call_and_sends_nothing() {
            let mut world = forwarding();
            world.rig.view = fleet().build();
            instructor_activity(&world);
            assert_eq!(forward_in(&world), skipped(Skip::SessionGone));
            assert!(world.sends().is_empty());
            assert!(world.rig.sim.comments().is_empty());
        }

        #[test]
        fn a_session_at_work_holds_what_is_to_be_sent_back_and_writes_nothing() {
            let mut world = forwarding();
            world.rig.view = building_view(SessionState::Working, Some((1, 1)));
            instructor_activity(&world);
            let count = world.count();
            let step = forward_in(&world);
            assert_eq!(
                step,
                ForwardStep {
                    held_back: true,
                    ..outcome(0, false)
                }
            );
            assert!(world.sends().is_empty());
            assert_eq!(world.count(), count);
        }

        #[test]
        fn a_newest_turn_that_building_has_not_read_is_read_first() {
            // The session's turn 2 ended and was not read: a message typed now would start a
            // turn that hides it, so the forward waits for `building` to read it.
            let mut world = forwarding();
            world.rig.view = building_view(SessionState::Idle, Some((2, 2)));
            instructor_activity(&world);
            let step = forward_in(&world);
            assert!(step.held_back);
            assert!(world.sends().is_empty());
            world.rig.step(&world.run, "turn-read 2");
            allow(&world, 1);
            assert_eq!(forward_in(&world), outcome(1, false));
        }

        #[test]
        fn a_session_the_fleet_shows_no_turn_for_is_waited_for() {
            let mut world = forwarding();
            world.rig.view = building_view(SessionState::Idle, None);
            instructor_activity(&world);
            assert!(forward_in(&world).held_back);
            assert!(world.sends().is_empty());
        }
    }

    mod forward_a_closed_issue {
        use super::*;

        #[test]
        fn closed_by_a_person_abandons_the_run_and_sends_nothing() {
            let world = guarded(|_| {});
            world.rig.sim.set_closed(true);
            assert_eq!(forward_in(&world), skipped(Skip::Abandoned));
            assert!(activity_reads(&world).is_empty());
            assert!(world.sends().is_empty());
            // The session's workspace is ended, and the run finished by the issue's closing.
            assert_eq!(
                world.rig.worker.calls(),
                vec![Call::EndSessions { workspace: old() }]
            );
            let end = world.events().pop().unwrap();
            assert_eq!(end.event, "finished");
            assert_eq!(end.cause.unwrap()["reason"], "issue-closed");
        }

        #[test]
        fn closed_by_the_runs_own_merged_pull_request_sends_nothing_and_writes_nothing() {
            let world = guarded(|world| {
                world.rig.write(
                    &world.run,
                    Event::PrOpened {
                        pr: 12,
                        url: URL12.into(),
                    },
                );
            });
            world.rig.sim.set_closed(true);
            world.rig.sim.set_pull(
                12,
                SimPull {
                    state: "MERGED",
                    draft: false,
                    head: BRANCH.into(),
                    base: "main".into(),
                    cross: false,
                },
            );
            let count = world.count();
            assert_eq!(forward_in(&world), skipped(Skip::PrMerged));
            assert!(activity_reads(&world).is_empty());
            assert!(world.sends().is_empty());
            assert!(world.rig.worker.calls().is_empty());
            assert_eq!(world.count(), count);
        }
    }

    mod forward_dropped_comments {
        use super::*;
        use crate::factory::github::DropReason;

        fn dropped_text(id: &str, reason: messages::CommentDropReason) -> String {
            messages::dropped_comment(REPO, ISSUE, id, reason)
        }

        fn posted(world: &World) -> Vec<String> {
            world.rig.sim.comments()
        }

        #[test]
        fn a_dropped_comment_is_reported_once_with_the_factorys_fixed_words() {
            let world = forwarding();
            script_dropped(&world, &[("7", DropReason::EditedByOther)]);
            assert_eq!(forward_in(&world), outcome(0, false));
            // Nothing goes into the session.
            assert!(world.sends().is_empty());
            let said = posted(&world);
            assert_eq!(said.len(), 1);
            assert!(
                said[0].starts_with(&dropped_text(
                    "7",
                    messages::CommentDropReason::EditedByOther
                )),
                "{}",
                said[0]
            );
            assert!(world.steps().contains(&"comment-dropped 7".to_string()));
            assert_eq!(
                world.events().pop().unwrap().cause.unwrap()["reason"],
                "comment-dropped"
            );
            // The next pass reads the same drop and says nothing more.
            let count = world.count();
            assert_eq!(forward_in(&world), outcome(0, false));
            assert_eq!(posted(&world).len(), 1);
            assert_eq!(world.count(), count);
        }

        #[test]
        fn each_reason_has_its_own_fixed_words_and_each_drop_its_own_comment_in_order() {
            let world = forwarding();
            script_dropped(
                &world,
                &[
                    ("8", DropReason::Unverifiable),
                    ("7", DropReason::EditedByOther),
                ],
            );
            forward_in(&world);
            let said = posted(&world);
            assert_eq!(said.len(), 2);
            assert!(said[0].starts_with(&dropped_text(
                "8",
                messages::CommentDropReason::Unverifiable
            )));
            assert!(said[1].starts_with(&dropped_text(
                "7",
                messages::CommentDropReason::EditedByOther
            )));
            assert_eq!(
                world.steps_of("comment-dropped"),
                ["comment-dropped 8", "comment-dropped 7"]
            );
        }

        #[test]
        fn a_dropped_comment_comes_after_what_is_sent_and_changes_nothing_about_it() {
            let world = forwarding();
            world.hub.set_activity(Activity {
                comments: vec![bare_comment("101", "tom", "Use serde here.")],
                body_edit: Some(edit(EDITED, "tom", "new")),
                dropped: vec![crate::factory::github::Dropped {
                    id: "7".into(),
                    reason: DropReason::EditedByOther,
                }],
            });
            allow(&world, 2);
            assert_eq!(forward_in(&world), outcome(1, false));
            assert_eq!(forward_in(&world), outcome(0, true));
            assert_eq!(
                world.sends(),
                vec![
                    comment_send("tom", "Use serde here."),
                    body_send("tom", "new")
                ]
            );
            assert_eq!(posted(&world).len(), 1, "said once, with the first pass");
            let steps = world.steps();
            let sent = steps
                .iter()
                .position(|s| s == &format!("body-edit-forwarded {EDITED}"));
            let dropped = steps.iter().position(|s| s == "comment-dropped 7").unwrap();
            let first_send = steps
                .iter()
                .position(|s| s.starts_with("send-typed"))
                .unwrap();
            assert!(first_send < dropped);
            assert!(sent.is_some());
        }

        #[test]
        fn a_failed_send_leaves_the_drop_for_the_next_pass_and_it_is_still_said_once() {
            let world = forwarding();
            world.hub.set_activity(Activity {
                comments: vec![bare_comment("101", "tom", "one")],
                body_edit: None,
                dropped: vec![crate::factory::github::Dropped {
                    id: "7".into(),
                    reason: DropReason::EditedByOther,
                }],
            });
            world
                .rig
                .worker
                .on_send(Err(crate::sessions::factory::SendRefused::Failed {
                    reason: "timed out".into(),
                    prompts: None,
                }));
            assert!(forward(&world.ctx(), &world.run).is_err());
            assert!(posted(&world).is_empty());
            allow(&world, 1);
            assert_eq!(forward_in(&world), outcome(1, false));
            assert_eq!(posted(&world).len(), 1);
        }

        #[test]
        fn a_drop_the_ledger_already_records_is_not_said_again() {
            let world = forwarding();
            world.rig.step(&world.run, "comment-dropped 7");
            script_dropped(
                &world,
                &[
                    ("7", DropReason::EditedByOther),
                    ("8", DropReason::EditedByOther),
                ],
            );
            forward_in(&world);
            let said = posted(&world);
            assert_eq!(said.len(), 1);
            assert!(said[0].starts_with(&dropped_text(
                "8",
                messages::CommentDropReason::EditedByOther
            )));
        }

        #[test]
        fn a_comment_the_issue_was_told_about_as_dropped_is_never_sent_later() {
            // The instructor was asked to post it again; a later read that lists it is not
            // followed.
            let world = forwarding();
            world.rig.step(&world.run, "comment-dropped 7");
            script(&world, vec![bare_comment("7", "tom", "one")], None);
            allow(&world, 1);
            let count = world.count();
            assert_eq!(forward_in(&world), outcome(0, false));
            assert!(world.sends().is_empty());
            assert_eq!(world.count(), count);
        }

        #[test]
        fn a_comment_that_was_already_forwarded_and_was_edited_since_is_not_reported() {
            // The session has the instructor's own words. A later edit by someone else changes
            // nothing it holds.
            let world = forwarding();
            world.rig.write(&world.run, comment_forwarded_line("7"));
            script_dropped(&world, &[("7", DropReason::EditedByOther)]);
            forward_in(&world);
            assert!(posted(&world).is_empty());
        }

        #[test]
        fn an_issue_comment_sent_as_feedback_is_not_reported_as_dropped_when_someone_else_edits_it()
        {
            let world = forwarding();
            world.rig.write(
                &world.run,
                Event::FeedbackRelaunching {
                    n: 1,
                    branch: BRANCH.into(),
                    items: vec!["issue-comment:42".into()],
                    message: "Feedback 1 of 1".into(),
                },
            );
            // The round decided is the feedback step's; once it is over the run is building
            // again and the same rule holds.
            world.rig.write(
                &world.run,
                Event::SessionLaunched {
                    thread_id: SESSION.into(),
                    worktree: Some(OLD.into()),
                    branch: Some(BRANCH.into()),
                    model_instance: None,
                    model: Some("opus".into()),
                    model_options: None,
                    resumed_from: None::<ResumedFrom>,
                    base: Some("main".into()),
                    title: None,
                },
            );
            script_dropped(&world, &[("42", DropReason::EditedByOther)]);
            let count = world.count();
            forward_in(&world);
            assert!(posted(&world).is_empty());
            assert_eq!(world.count(), count);
        }

        #[test]
        fn a_comment_that_is_both_dropped_and_listed_twice_is_said_once() {
            let world = forwarding();
            script_dropped(
                &world,
                &[
                    ("7", DropReason::EditedByOther),
                    ("7", DropReason::EditedByOther),
                ],
            );
            forward_in(&world);
            assert_eq!(posted(&world).len(), 1);
            assert_eq!(world.steps_of("comment-dropped"), ["comment-dropped 7"]);
        }

        #[test]
        fn a_run_forward_skips_reports_no_drops() {
            for (why, setup) in [(Skip::NotBuilding, 0), (Skip::GateSendPending, 1)] {
                let world = forwarding();
                if setup == 0 {
                    world.rig.write(
                        &world.run,
                        Event::QuestionsPosted {
                            questions: "1. why".into(),
                        },
                    );
                } else {
                    world.rig.step(&world.run, "gate-sending 1 1");
                }
                script_dropped(&world, &[("7", DropReason::EditedByOther)]);
                let count = world.count();
                assert_eq!(forward_in(&world), skipped(why), "{why:?}");
                assert!(activity_reads(&world).is_empty(), "{why:?}");
                assert!(posted(&world).is_empty(), "{why:?}");
                assert_eq!(world.count(), count, "{why:?}");
            }
        }

        #[test]
        fn a_thread_that_no_longer_exists_skips_the_call_and_reports_no_drops() {
            let mut world = forwarding();
            world.rig.view = fleet().build();
            world.hub.set_activity(Activity {
                comments: vec![bare_comment("101", "tom", "one")],
                body_edit: None,
                dropped: vec![crate::factory::github::Dropped {
                    id: "7".into(),
                    reason: DropReason::EditedByOther,
                }],
            });
            assert_eq!(forward_in(&world), skipped(Skip::SessionGone));
            assert!(posted(&world).is_empty());
        }

        #[test]
        fn a_failing_comment_call_is_an_error_with_no_ledger_line_and_the_next_pass_says_it() {
            let world = forwarding();
            script_dropped(&world, &[("7", DropReason::Unverifiable)]);
            let count = world.count();
            // The activity read is call 0; the issue read before it in the sim counts too.
            world.rig.sim.fail("issue comment", 1);
            assert!(forward(&world.ctx(), &world.run).is_err());
            assert_eq!(world.count(), count);
            forward_in(&world);
            let said = posted(&world);
            assert_eq!(said.len(), 1);
            assert!(said[0].starts_with(&dropped_text(
                "7",
                messages::CommentDropReason::Unverifiable
            )));
            assert_eq!(world.steps_of("comment-dropped"), ["comment-dropped 7"]);
        }
    }

    mod forward_a_send_that_is_refused {
        use super::*;
        use crate::sessions::factory::SendRefused;

        #[test]
        fn a_refused_body_edit_is_dropped_with_a_notice_and_its_step_and_a_newer_edit_is_still_sent()
         {
            let world = forwarding();
            script(&world, vec![], Some(edit(EDITED, "tom", "new")));
            world
                .rig
                .worker
                .on_send(Err(SendRefused::SecretsUnreadable));
            assert_eq!(forward_in(&world), outcome(0, false));
            assert_eq!(world.sends(), vec![body_send("tom", "new")]);
            assert_eq!(world.rig.sim.comments().len(), 1);
            assert!(
                world.rig.sim.comments()[0].starts_with(messages::REFUSED_BODY_EDIT),
                "{}",
                world.rig.sim.comments()[0]
            );
            assert!(
                world
                    .steps()
                    .contains(&format!("body-edit-dropped {EDITED}"))
            );
            assert_eq!(
                world.events().pop().unwrap().cause.unwrap()["reason"],
                "body-edit-dropped"
            );
            // Never sent again, said once.
            let count = world.count();
            assert_eq!(forward_in(&world), outcome(0, false));
            assert_eq!(world.sends().len(), 1);
            assert_eq!(world.count(), count);
            // A newer edit is sent.
            let later = "2026-10-04T10:20:00Z";
            script(&world, vec![], Some(edit(later, "tom", "newer")));
            allow(&world, 1);
            assert_eq!(forward_in(&world), outcome(0, true));
            assert_eq!(world.sends().last(), Some(&body_send("tom", "newer")));
            assert!(
                world
                    .steps()
                    .contains(&format!("body-edit-forwarded {later}"))
            );
        }

        #[test]
        fn a_refusal_between_two_sendable_comments_stops_nothing() {
            let world = forwarding();
            script(
                &world,
                vec![
                    bare_comment("101", "tom", "one"),
                    bare_comment("102", "tom", "two"),
                    bare_comment("103", "tom", "three"),
                ],
                None,
            );
            allow(&world, 1);
            world.rig.worker.on_send(Err(SendRefused::BodyWithoutPath));
            allow(&world, 1);
            assert_eq!(forward_in(&world), outcome(1, false));
            // The second is refused and the third goes in the same pass.
            assert_eq!(forward_in(&world), outcome(1, false));
            assert_eq!(
                world.sends(),
                vec![
                    comment_send("tom", "one"),
                    comment_send("tom", "two"),
                    comment_send("tom", "three"),
                ]
            );
            assert_eq!(forwarded_ids(&world), ["101", "103"]);
            assert_eq!(world.steps_of("comment-dropped"), ["comment-dropped 102"]);
            let said = world.rig.sim.comments();
            assert_eq!(said.len(), 1);
            assert!(said[0].starts_with(&messages::dropped_comment(
                REPO,
                ISSUE,
                "102",
                messages::CommentDropReason::Refused
            )));
        }

        #[test]
        fn a_refused_comment_followed_by_a_sendable_edit_sends_the_edit() {
            let world = forwarding();
            instructor_activity(&world);
            world.rig.worker.on_send(Err(SendRefused::BodyWithoutPath));
            allow(&world, 1);
            assert_eq!(forward_in(&world), outcome(0, true));
            assert_eq!(
                world.sends(),
                vec![comment_send("tom", "one"), body_send("tom", "new")]
            );
            assert_eq!(world.rig.sim.comments().len(), 1);
            assert_eq!(world.steps_of("comment-dropped"), ["comment-dropped 101"]);
            assert!(
                world
                    .steps()
                    .contains(&format!("body-edit-forwarded {EDITED}"))
            );
        }

        #[test]
        fn a_call_whose_every_send_was_refused_sends_nothing_and_tells_the_issue_of_each() {
            // The runner's `a call whose every send was refused holds no gate and leaves no
            // wait`: there is no wait marker here to leave, and the notices come in order.
            let world = forwarding();
            instructor_activity(&world);
            world.rig.worker.on_send(Err(SendRefused::BodyWithoutPath));
            world.rig.worker.on_send(Err(SendRefused::BodyWithoutPath));
            assert_eq!(forward_in(&world), outcome(0, false));
            assert_eq!(world.rig.sim.comments().len(), 2);
            assert_eq!(
                world.steps_of("comment-dropped").len() + world.steps_of("body-edit-dropped").len(),
                2
            );
            // Nothing is left to send or to wait for.
            let count = world.count();
            assert_eq!(forward_in(&world), outcome(0, false));
            assert_eq!(world.count(), count);
        }

        #[test]
        fn a_crash_before_the_notice_says_it_on_the_next_call_and_the_step_comes_after_the_comment()
        {
            let world = forwarding();
            world.rig.worker.on_send(Err(SendRefused::BodyWithoutPath));
            script(&world, vec![bare_comment("101", "tom", "one")], None);
            // The notice's post fails: no step is written, so the next call says it.
            world.rig.sim.fail("issue comment", 1);
            assert!(forward(&world.ctx(), &world.run).is_err());
            assert!(world.steps_of("comment-dropped").is_empty());
            assert!(world.rig.sim.comments().is_empty());
            world.rig.worker.on_send(Err(SendRefused::BodyWithoutPath));
            assert_eq!(forward_in(&world), outcome(0, false));
            assert_eq!(world.rig.sim.comments().len(), 1);
            assert_eq!(world.steps_of("comment-dropped"), ["comment-dropped 101"]);
            // The step follows the comment: a crash between them says the notice twice, never
            // loses it.
            let world = forwarding();
            world.rig.worker.on_send(Err(SendRefused::BodyWithoutPath));
            world.rig.worker.on_send(Err(SendRefused::BodyWithoutPath));
            script(&world, vec![bare_comment("101", "tom", "one")], None);
            world.rig.sim.lose("issue comment", 1);
            assert!(forward(&world.ctx(), &world.run).is_err());
            assert_eq!(world.rig.sim.comments().len(), 1, "the comment landed");
            assert!(world.steps_of("comment-dropped").is_empty());
            assert_eq!(forward_in(&world), outcome(0, false));
            assert_eq!(world.steps_of("comment-dropped"), ["comment-dropped 101"]);
        }

        #[test]
        fn a_transport_failure_after_a_refusal_still_stops_the_call_and_the_refused_item_is_told_next_time()
         {
            let world = forwarding();
            script(
                &world,
                vec![
                    bare_comment("101", "tom", "one"),
                    bare_comment("102", "tom", "two"),
                ],
                None,
            );
            world.rig.worker.on_send(Err(SendRefused::BodyWithoutPath));
            world.rig.worker.on_send(Err(SendRefused::Failed {
                reason: "timed out".into(),
                prompts: None,
            }));
            assert!(forward(&world.ctx(), &world.run).is_err());
            assert!(world.rig.sim.comments().is_empty(), "nothing was told yet");
            assert!(world.steps_of("comment-dropped").is_empty());
            // The next call sends the second again first, under the same count, since it may
            // have been typed; the call after it refuses the first again and tells the issue.
            allow(&world, 1);
            assert_eq!(forward_in(&world), outcome(1, false));
            assert_eq!(forwarded_ids(&world), ["102"]);
            assert!(world.rig.sim.comments().is_empty());
            world.rig.worker.on_send(Err(SendRefused::BodyWithoutPath));
            assert_eq!(forward_in(&world), outcome(0, false));
            assert_eq!(world.steps_of("comment-dropped"), ["comment-dropped 101"]);
            assert_eq!(world.rig.sim.comments().len(), 1);
        }

        #[test]
        fn a_session_that_is_not_idle_when_the_worker_looks_is_tried_again_next_pass() {
            let world = forwarding();
            script(&world, vec![bare_comment("101", "tom", "one")], None);
            world.rig.worker.on_send(Err(SendRefused::NotIdle));
            let step = forward_in(&world);
            assert_eq!(
                step,
                ForwardStep {
                    held_back: true,
                    ..outcome(0, false)
                }
            );
            assert!(forwarded_ids(&world).is_empty());
            allow(&world, 1);
            assert_eq!(forward_in(&world), outcome(1, false));
        }
    }
}
